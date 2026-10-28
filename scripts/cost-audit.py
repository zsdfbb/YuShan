#!/usr/bin/env python3
"""成本审计：按单价把真实会话的 token 拆成**金额**，指出钱花在哪。

为什么要有它：token 数量与成本**不是一回事**。同一份数据按 token 排与按金额排
得到的结论相反 —— 例如 reasoning 占 output 的 70%（token 口径）却只占总成本 17.8%，
而「可见正文」占 content 的 12% 却只值成本的 1.7%。任何「省 X%」的说法都必须
先落成金额，否则只是把体量当成本。

数据源（全部是本地已有文件，**零 API 调用**）：
  1. `~/.yushan/logs/debug-*.jsonl` —— `round_usage` 事件带 provider 上报的
     `input_tokens` / `output_tokens` / `reasoning_tokens` / `cache_read_tokens`。
  2. `~/.yushan/sessions/*.jsonl` —— 消息内容，用于把「输出」再拆成
     reasoning / 工具调用参数 / 可见正文（日志里没有这个细分）。

口径与限制：
  - **只统计 provider 实际上报了 usage 的轮**。`minimax()` 的
    `supports_stream_usage = false`（`adapters/model-openai-compatible/src/compat.rs`）
    ⇒ 它的 session 在 `round_usage` 里没有读数，本脚本会把它显式列成「无读数」，
    绝不推成 0。
  - **内容细分是估算**：`tool_args` / 正文的比例来自 session JSONL 的字符数，
    按 `chars/3` 折算 token（全仓唯一的估算口径，`ys_loop::token`）。它用来**排序**
    杠杆，不能当作账单。
  - **默认单价是 DeepSeek 闲置时段**（<https://api-docs.deepseek.com/zh-cn/quick_start/pricing>）：
    缓存命中 0.02 / 未命中 1.00 / 输出 4.00 元每百万 token。高峰时段全部 ×2，
    比例不变。其他 provider 价目不同，须用 `--price-*` 覆盖。

用法：
    python3 scripts/cost-audit.py                     # 默认：全部会话，DeepSeek 闲置价
    python3 scripts/cost-audit.py --model deepseek    # 只统计某模型前缀
    python3 scripts/cost-audit.py --peak             # 高峰时段（×2）
    python3 scripts/cost-audit.py --price-out 8     # 覆盖单价
    python3 scripts/cost-audit.py --json            # 机器可读（给 A/B 比较用）
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import sys
from collections import Counter

# DeepSeek 闲置时段单价（元 / 百万 token）。权威口径见模块头注释里的 URL。
PRICE_HIT = 0.02
PRICE_MISS = 1.00
PRICE_OUT = 4.00

# 日志与会话的默认位置。`YUSHAN_LOG_DIR` / `YUSHAN_SESSIONS_DIR` 只影响新写入，
# 审计针对**已有**文件，故直接读默认目录；要审别的目录用参数覆盖。
DEFAULT_LOG_DIR = os.path.expanduser("~/.yushan/logs")
DEFAULT_SESSION_DIR = os.path.expanduser("~/.yushan/sessions")


def read_round_usage(log_dir: str) -> tuple[dict[str, Counter], dict[str, str], dict[str, str]]:
    """按 session_id 聚合 `round_usage` 的 token 读数。

    返回 `(per_session, models, providers)`。

    - 只统计 provider **确实上报**的字段：`None`（未上报）不参与累加，也不当 0 ——
      与 `Usage::reasoning_tokens` 的既有语义一致。
    - **provider 名单独记**（`gen_ai.system`）：单价是按 provider 定的，把 A 家的 token
      套到 B 家的价目上会得出无意义的金额（本脚本默认只用 DeepSeek 单价）。
    """
    per_session: dict[str, Counter] = {}
    models: dict[str, str] = {}
    providers: dict[str, str] = {}
    for path in sorted(glob.glob(os.path.join(log_dir, "debug-*.jsonl"))):
        session = os.path.basename(path)[len("debug-") : -len(".jsonl")]
        acc: Counter = Counter()
        with open(path, errors="ignore") as fh:
            for line in fh:
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                ev = rec.get("ev")
                # 模型名 / provider 只在 run_start 里；一个会话可能多次 run（`/new`、resume）。
                if ev == "run_start":
                    if name := rec.get("gen_ai.request.model"):
                        models.setdefault(session, name)
                    if system := rec.get("gen_ai.system"):
                        providers.setdefault(session, system)
                    continue
                if ev != "round_usage":
                    continue
                acc["rounds"] += 1
                for key, field in (
                    ("in", "gen_ai.usage.input_tokens"),
                    ("out", "gen_ai.usage.output_tokens"),
                    ("reason", "gen_ai.usage.reasoning.output_tokens"),
                    ("hit", "gen_ai.usage.cache_read.input_tokens"),
                ):
                    value = rec.get(field)
                    if isinstance(value, int):
                        acc[key] += value
        if acc["rounds"]:
            per_session[session] = acc
    return per_session, models, providers


def read_content_split(session_dir: str, sessions: set[str]) -> Counter:
    """按字符数统计消息内容构成（仅用于把「输出」再拆细分）。

    只数 `Text` / `ToolUse.arguments` / `ToolResult.content` 三类 —— 与
    `crates/ys-core/src/message.rs` 的 `ContentBlock` 一一对应。
    """
    acc = Counter()
    for path in sorted(glob.glob(os.path.join(session_dir, "*.jsonl"))):
        session = os.path.basename(path)[: -len(".jsonl")]
        if sessions and session not in sessions:
            continue
        with open(path, errors="ignore") as fh:
            for line in fh:
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                if rec.get("type") == "meta":
                    continue
                role = rec.get("role")
                for block in rec.get("content") or []:
                    if "Text" in block:
                        key = "assistant_text" if role == "Assistant" else "user_text"
                        acc[key] += len(block["Text"].get("text") or "")
                    elif "ToolUse" in block:
                        acc["tool_args"] += len(json.dumps(block["ToolUse"].get("arguments")))
                    elif "ToolResult" in block:
                        acc["tool_result"] += len(block["ToolResult"].get("content") or "")
    return acc


def main() -> int:
    ap = argparse.ArgumentParser(description="按单价拆解会话成本构成")
    ap.add_argument("--log-dir", default=DEFAULT_LOG_DIR)
    ap.add_argument("--session-dir", default=DEFAULT_SESSION_DIR)
    ap.add_argument("--model", default="", help="只统计模型名含该子串的会话")
    ap.add_argument(
        "--model-prefix",
        default="deepseek",
        help="只统计模型名以该前缀开头的会话（默认 deepseek —— 唯一优化标准）",
    )
    ap.add_argument(
        "--provider-all",
        action="store_true",
        help="不限模型（金额列不可信，仅看 token 分布）",
    )
    ap.add_argument("--peak", action="store_true", help="高峰时段（单价 ×2）")
    ap.add_argument("--price-hit", type=float, default=PRICE_HIT)
    ap.add_argument("--price-miss", type=float, default=PRICE_MISS)
    ap.add_argument("--price-out", type=float, default=PRICE_OUT)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    factor = 2.0 if args.peak else 1.0
    p_hit = args.price_hit * factor
    p_miss = args.price_miss * factor
    p_out = args.price_out * factor

    per_session, models, providers = read_round_usage(args.log_dir)
    if not per_session:
        print(f"没有找到带 usage 读数的日志（{args.log_dir}）", file=sys.stderr)
        return 1

    # **默认只统计 DeepSeek 模型**（模型名以 `deepseek` 开头）：单价按 DeepSeek 价目定。
    #
    # 注意筛的是**模型名而不是 provider 名**：本仓第一优选模型是 `deepseek-*`，但它可能
    # 经内置 `deepseek` 或 `opencode-go` 网关接入 —— 按 provider 筛会把主力会话全漏掉。
    # 用 `--provider-all` 看全量（金额列此时不可信，仅看 token 分布）。
    if not args.provider_all:
        per_session = {
            s: v
            for s, v in per_session.items()
            if (models.get(s) or "").startswith(args.model_prefix)
        }
    if args.model:
        keep = {s for s, m in models.items() if args.model in m}
        per_session = {s: v for s, v in per_session.items() if s in keep}
    if not per_session:
        print(
            f"没有模型名以 {args.model_prefix!r} 开头的会话读数"
            "（用 --provider-all 看全量）",
            file=sys.stderr,
        )
        return 1

    total = Counter()
    silent: list[str] = []  # 有轮但没有 usage 读数（provider 未上报）
    for session, acc in per_session.items():
        if acc["in"] == 0 and acc["out"] == 0 and acc["hit"] == 0:
            silent.append(f"{session} ({models.get(session, '?')})")
            continue
        for key in ("in", "out", "reason", "hit", "rounds"):
            total[key] += acc[key]

    hit = total["hit"]
    miss = max(0, total["in"] - hit)
    out = total["out"]
    reason = total["reason"]

    content = read_content_split(args.session_dir, set(per_session))
    # 把 content 的字符比例折算到 provider 报的 output 上：以「reasoning + tool_args + 正文」
    # 为分母，只用来给输出内部排序，不改变 output 总额。
    args_tok = content["tool_args"] // 3
    text_tok = content["assistant_text"] // 3
    denom = reason + args_tok + text_tok
    scale = out / denom if denom else 0.0
    args_est = args_tok * scale
    text_est = text_tok * scale

    cost_hit = hit * p_hit / 1e6
    cost_miss = miss * p_miss / 1e6
    cost_reason = reason * p_out / 1e6
    cost_args = args_est * p_out / 1e6
    cost_text = text_est * p_out / 1e6
    cost_out = out * p_out / 1e6
    grand = cost_hit + cost_miss + cost_out

    def pct(value: float) -> float:
        return 100.0 * value / grand if grand else 0.0

    rows = [
        ("输入 · 缓存命中", hit, p_hit, cost_hit),
        ("输入 · 未命中", miss, p_miss, cost_miss),
        ("输出 · reasoning", reason, p_out, cost_reason),
        ("输出 · 工具调用参数", args_est, p_out, cost_args),
        ("输出 · 可见正文", text_est, p_out, cost_text),
    ]

    if args.json:
        print(
            json.dumps(
                {
                    "sessions": len(per_session) - len(silent),
                    "rounds": total["rounds"],
                    "peak": args.peak,
                    "rows": [
                        {"name": n, "tokens": round(t), "price": p, "cost": round(c, 4), "pct": round(pct(c), 2)}
                        for n, t, p, c in rows
                    ],
                    "total_cost": round(grand, 4),
                    "silent_sessions": silent,
                },
                ensure_ascii=False,
                indent=2,
            )
        )
        return 0

    print(f"会话 {len(per_session) - len(silent)} 个 / {total['rounds']} 轮　"
          f"单价：缓存命中 {p_hit} / 未命中 {p_miss} / 输出 {p_out} 元每百万"
          f"{'（高峰 ×2）' if args.peak else '（闲置时段）'}")
    print()
    print(f"{'构成':22}{'token':>14}{'单价':>8}{'元':>9}{'占比':>8}")
    print("-" * 61)
    for name, tokens, price, cost in rows:
        print(f"{name:22}{tokens:14,.0f}{price:8.2f}{cost:9.2f}{pct(cost):7.1f}%")
    print("-" * 61)
    print(f"{'合计':22}{'':14}{'':8}{grand:9.2f}  100.0%")
    print()
    print(f"缓存命中率 {100 * hit / total['in'] if total['in'] else 0:.1f}%　"
          f"reasoning 占 output {100 * reason / out if out else 0:.1f}%")
    print(f"★ 全部关闭思考可省 {cost_reason:.2f} 元（{pct(cost_reason):.1f}%）")
    print(f"△ 少调工具（参数随之减少）最多省 {cost_args:.2f} 元（{pct(cost_args):.1f}%）")
    print(f"✗ 削减叙述与正文最多省 {cost_text:.2f} 元（{pct(cost_text):.1f}%）—— 不值得优先动")
    if silent:
        print()
        print(f"⚠️ {len(silent)} 个会话无 usage 读数（provider 未上报，不计入、不当 0）：")
        for item in silent[:8]:
            print(f"   - {item}")
        if len(silent) > 8:
            print(f"   …… 另 {len(silent) - 8} 个")
    return 0


if __name__ == "__main__":
    sys.exit(main())
