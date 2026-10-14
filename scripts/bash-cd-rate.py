#!/usr/bin/env python3
"""统计真实会话里 `bash` 调用的 `cd` 前缀率 —— 量「冗余 `cd`」这件事的尺子。

用法：
    python3 scripts/bash-cd-rate.py                       # 默认 ~/.yushan/sessions
    python3 scripts/bash-cd-rate.py --dir <目录>
    python3 scripts/bash-cd-rate.py --workspace /Users/zs/Develop/YuShan
    python3 scripts/bash-cd-rate.py --json

为什么要它：此前判断「模型还加不加 `cd <工作目录> &&`」只能靠临时接缝跑变体实验 ——
每格 n=1、花 API 额度、还曾被 429 打断。而**真实会话本来就落盘**，里面有全部调用
原文。这个脚本把那把尺子换成零成本的长期度量。

度量边界（结论强度全靠它，别越过）：
  1. **跨模型不可比** —— 实测 MiniMax-M3 44 次调用 0 次前缀，deepseek-flash 则九成
     带前缀。这是两个对象，不是两张配置。
  2. **跨任务不可比** —— 单问句任务与开放式探索任务的调用数、钉位置倾向都不同。
  3. 只能**同模型、同任务型、改动前后**对照，且采集多个会话。
  4. 旧格式会话没有 meta 行（首行就是 `{"role":…}`）⇒ provider/model 记为 `?`，
     不得并入任何模型的分组。
  5. 会话里过长的工具结果可能被省略（`[[earlier tool result elided …]]`）⇒
     `after_note` 是**下界**。

输出列：
  calls      该会话的 bash 调用数（只数 Assistant 消息里的 ToolUse，不数结果回显）
  cd         以 `cd` 开头的调用数
  first_cd   第一个带 `cd` 的调用是第几次（1-based；0 = 全程没有）
  cd_root    其中 `cd <workspace> && …` 形态的次数（只在传了 --workspace 时有意义）
  after_note 首个含注记的结果之后，仍以 `cd` 开头的调用数
             （只在改动落地后的会话里可能非零；用于判定「给了事实之后行为变没变」）
"""

import argparse
import json
import os
import re
import sys
from collections import defaultdict

# 与 `adapters/tools-basic/src/bash.rs` 里的注记文案同源（改动文案时这里要跟着改）
NOTE_MARK = "[bash: this shell already started in"

# `cd` 之后的第一个词：引号形态，或到空白 / shell 元字符为止的裸词
CD_ARG = re.compile(r"""^\s*cd\s+("[^"]*"|'[^']*'|[^\s&;|<>]+)""")


def iter_records(path):
    """逐行读会话文件；坏行跳过（会话可能被 kill，末尾允许截断）。"""
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def scan_session(path, workspace):
    """扫一个会话文件，返回 (provider, model, 统计字典)。"""
    provider = model = "?"
    calls = cds = cd_root = first_cd = after_note = 0
    note_seen = False  # 是否已经出现过「冗余 cd 注记」的工具结果

    for rec in iter_records(path):
        if rec.get("type") == "meta":
            provider = rec.get("provider") or "?"
            model = rec.get("model") or "?"
            continue

        for block in rec.get("content") or []:
            if not isinstance(block, dict):
                continue

            # 调用：Assistant 消息里的 ToolUse（结果回显不在其中，不会重复计数）
            use = block.get("ToolUse")
            if use and use.get("name") == "bash":
                cmd = (use.get("arguments") or {}).get("command")
                if not isinstance(cmd, str):
                    continue
                calls += 1
                if re.match(r"^\s*cd(\s|$)", cmd):
                    cds += 1
                    if first_cd == 0:
                        first_cd = calls
                    if note_seen:
                        after_note += 1
                    if workspace:
                        m = CD_ARG.match(cmd)
                        if m:
                            arg = m.group(1).strip("\"'")
                            cand = arg if os.path.isabs(arg) else os.path.join(workspace, arg)
                            if os.path.realpath(cand) == os.path.realpath(workspace):
                                cd_root += 1
                continue

            # 结果：User 消息里的 ToolResult（注记在这里，不在调用那一侧）
            result = block.get("ToolResult")
            if result and NOTE_MARK in (result.get("content") or ""):
                note_seen = True

    return provider, model, {
        "calls": calls,
        "cd": cds,
        "first_cd": first_cd,
        "cd_root": cd_root,
        "after_note": after_note,
    }


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--dir",
        default=os.environ.get("YUSHAN_SESSIONS_DIR") or os.path.expanduser("~/.yushan/sessions"),
    )
    ap.add_argument(
        "--workspace",
        default=None,
        help="用于把前缀分成「cd 到工作目录本身」与其他（不传则 cd_root 恒 0）",
    )
    ap.add_argument("--json", action="store_true", dest="as_json")
    args = ap.parse_args()

    if not os.path.isdir(args.dir):
        print(f"会话目录不存在：{args.dir}", file=sys.stderr)
        return 2

    rows = []
    for name in sorted(os.listdir(args.dir)):
        if name.endswith(".jsonl"):
            provider, model, st = scan_session(os.path.join(args.dir, name), args.workspace)
            rows.append({"session": name, "provider": provider, "model": model, **st})

    if args.as_json:
        print(json.dumps(rows, ensure_ascii=False, indent=2))
        return 0

    print(f"{'session':30} {'provider/model':26} {'calls':>5} {'cd':>4} {'rate':>6} "
          f"{'cd_root':>7} {'first_cd':>8} {'after_note':>10}")
    for r in rows:
        rate = f"{100 * r['cd'] / r['calls']:.0f}%" if r["calls"] else "-"
        print(f"{r['session']:30} {r['provider'] + '/' + r['model']:26} {r['calls']:5} "
              f"{r['cd']:4} {rate:>6} {r['cd_root']:7} {r['first_cd']:8} {r['after_note']:10}")

    print("\n按模型分组（跨模型不可比，只能同模型看改动前后）：")
    groups = defaultdict(lambda: {"sessions": 0, "calls": 0, "cd": 0, "cd_root": 0, "first_cd": []})
    for r in rows:
        if r["calls"] == 0:
            continue
        g = groups[f"{r['provider']}/{r['model']}"]
        g["sessions"] += 1
        g["calls"] += r["calls"]
        g["cd"] += r["cd"]
        g["cd_root"] += r["cd_root"]
        g["first_cd"].append(r["first_cd"])
    for key, g in sorted(groups.items()):
        rate = f"{100 * g['cd'] / g['calls']:.0f}%" if g["calls"] else "-"
        print(f"  {key:30} sessions={g['sessions']} calls={g['calls']:4} cd={g['cd']:4} "
              f"({rate}) cd_root={g['cd_root']:4} first_cd={g['first_cd']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
