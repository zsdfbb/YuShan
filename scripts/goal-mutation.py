#!/usr/bin/env python3
"""目标推进与验收层（ADR-0072）的**变异测试**：注入 bug ⇒ 期望测试变红 ⇒ 还原。

为什么要有它：验收层是「模型自称完成」这条路的唯一结构防线。它的每条判据
（取消优先 / 无 open 项收场 / 计数上限 / 停滞护栏 / 引用校验）都是**一行 if**，
删掉任何一行都不会让别的测试失败 —— 全绿的套件仍可能对它们零覆盖。
AGENTS.md 约定「关键逻辑要做变异测试」，这把尺子就是那把尺子。

形态对齐 `scripts/cancellation-mutation.py`（同一套机制，便于维护）：

  - **一次跑完全部 mutant**，跑完给一张汇总表。
  - 每个 mutant 跑**包级**测试（`cargo test -p <pkg>`）而非单个具名用例 ——
    覆盖更强，且能报出「哪些用例抓到了它」。
  - 注入用**精确文本替换**（不是行号/正则），改不动就**报错退出**，
    绝不允许「注入没生效 ⇒ 测试绿 ⇒ 假通过」。
  - 每个 mutant 前**快照**、跑完**还原**；`finally` 兜底，异常也不留脏工作区。

用法：
    python3 scripts/goal-mutation.py            # 跑全部
    python3 scripts/goal-mutation.py --list     # 只列清单
    python3 scripts/goal-mutation.py 1 4 7      # 只跑指定编号
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@dataclass
class Mutant:
    """一处注入。

    `old`/`new` 必须是**精确**文本；`old` 在目标文件里不唯一或不存在时，
    注入直接失败（不算「测试通过」）。
    """

    name: str
    package: str
    desc: str
    edits: list[tuple[str, str, str]] = field(default_factory=list)
    #: 期望至少有一个测试变红；为空表示「该 mutant 只要求整包不绿」。
    expect_red: str = ""


GOAL = "apps/coding-agent/src/goal.rs"
JUDGE = "apps/coding-agent/src/goal_judge.rs"
INJECT = "apps/coding-agent/src/todo_inject.rs"

# 全部待注入文件（跑完统一还原）。
TARGETS = [GOAL, JUDGE, INJECT]


MUTANTS: list[Mutant] = [
    # ── 判据顺序 ──────────────────────────────────────────────────────
    Mutant(
        name="判据 1：用户取消不再优先（挪到「无 open 项」之后）",
        package="ys-coding-agent",
        desc="清单恰好清空时，用户的 Ctrl+C 被覆盖成「干完了」⇒ 用户意图失效",
        expect_red="cancelled_wins_over_all_done",
        edits=[
            (
                GOAL,
                """    if matches!(last_stop, StopReason::Cancelled) {
        return FollowUp::Stop(StopCause::Aborted);
    }""",
                """    // MUTATION: 取消优先被移除""",
            ),
            (
                GOAL,
                """    if !store.has_open_items() && store.revision() > 0 {
        return FollowUp::Stop(StopCause::AllDone);
    }""",
                """    if !store.has_open_items() && store.revision() > 0 {
        return FollowUp::Stop(StopCause::AllDone);
    }
    if matches!(last_stop, StopReason::Cancelled) {
        return FollowUp::Stop(StopCause::Aborted);
    }""",
            ),
        ],
    ),
    Mutant(
        name="判据 2：不可续的收场形态被放行",
        package="ys-coding-agent",
        desc="Truncated / Failed / MaxRounds / NoOutput 仍会再起一轮 ⇒ 对失败回合继续烧钱",
        expect_red="non_continuable_stop_reasons_stop",
        edits=[
            (
                GOAL,
                """        StopReason::Truncated
        | StopReason::Failed
        | StopReason::MaxRounds
        | StopReason::NoOutput => {
            return FollowUp::Stop(StopCause::NotContinuable);
        }""",
                """        StopReason::Truncated
        | StopReason::Failed
        | StopReason::MaxRounds
        | StopReason::NoOutput => {}""",
            )
        ],
    ),
    Mutant(
        name="判据 3：判官不可靠不再停机",
        package="ys-coding-agent",
        desc="没有判据就没有停止条件 ⇒ 判官连续坏掉时仍无限推进（跑飞）",
        expect_red="judge_failures_reaching_limit_stop",
        edits=[
            (
                GOAL,
                """    if *judge_failures >= JUDGE_FAILURE_LIMIT {
        return FollowUp::Stop(StopCause::JudgeUnreliable);
    }""",
                """    // MUTATION: 判官可靠性闸门被移除""",
            )
        ],
    ),
    # 判据 4（收场 = 「无 open 项」而非「全部 Completed」）**没有变异条目**：
    #
    # 这两个判据在 `TodoStore` 的不变量下**语义等价**，无法用变异区分。
    # `apply` 的硬规则 2 保证「无 open 项 ⇒ 清空清单」，因此 `snapshot()` 里
    # **不可能**出现「有项但全部终态」——「全部 Completed」在空集上恒为真。
    # 用变异去抓它只会得到一个假缺口（实测确实如此）。
    #
    # 真正的守护是 `apply` 自己的两条硬规则（`adapters/tools-todo/src/store.rs`）
    # 与它们的 40 条测试，不在本层。
    Mutant(
        name="判据 6：追问计数上限去掉",
        package="ys-coding-agent",
        desc="used 无上限 ⇒ 一直推下去，直到墙钟兜底（48h）",
        expect_red="used_up_to_cap_stops",
        edits=[
            (
                GOAL,
                """    if used >= MAX_FOLLOW_UPS {
        return FollowUp::Stop(StopCause::CapReached);
    }""",
                """    // MUTATION: 计数上限被移除""",
            )
        ],
    ),
    Mutant(
        name="判据 7：停滞护栏去掉「模型自称收尾」的 AND",
        package="ys-coding-agent",
        desc="只靠集合不变判定 ⇒ 误杀「一轮里干了活但还没更新状态」的情形",
        expect_red="stalled_requires_unchanged_set_and_model_claims_done",
        edits=[
            (
                GOAL,
                """    if stall.unchanged_turns() >= STALL_LIMIT && model_claims_done {""",
                """    if stall.unchanged_turns() >= STALL_LIMIT {""",
            )
        ],
    ),
    # ── 集合指纹 ──────────────────────────────────────────────────────
    Mutant(
        name="集合指纹把已完成的项也算进去",
        package="ys-coding-agent",
        desc="模型把已完成项重述一遍就重置了 stall ⇒ 停滞永远检不出来",
        expect_red="digest_ignores_terminal_items",
        edits=[
            (
                GOAL,
                """        .filter(|item| !item.status.is_terminal())""",
                """        .filter(|_| true)""",
            )
        ],
    ),
    Mutant(
        name="集合指纹去掉大小写折叠",
        package="ys-coding-agent",
        desc="模型换个大小写就重置 stall ⇒ 停滞漏检",
        expect_red="digest_ignores_case",
        edits=[
            (
                GOAL,
                """                normalize_content(&item.content).to_lowercase(),""",
                """                normalize_content(&item.content),""",
            )
        ],
    ),
    Mutant(
        name="集合指纹不排序（依赖声明顺序）",
        package="ys-coding-agent",
        desc="模型调整项的顺序就重置 stall ⇒ 停滞漏检",
        expect_red="digest_is_order_insensitive",
        edits=[
            (
                GOAL,
                """    keys.sort_unstable();
    keys.dedup();""",
                """    keys.dedup();""",
            )
        ],
    ),
    # ── 判官引用校验 ──────────────────────────────────────────────────
    Mutant(
        name="判官：有 MET 就放行，不校验引用是否命中",
        package="ys-coding-agent",
        desc="判官编造引用也能通关 ⇒ 「模型自称完成」靠判官幻觉通过，核心防线失效",
        expect_red="met_with_fabricated_quote_is_unparseable",
        edits=[
            (
                JUDGE,
                """                if !quote_is_valid(&quote) || !haystack.contains(&normalize(&quote)) {
                    // 引用为空 / 过短 / 找不到 —— 判官不可靠，绝不采信「达成」。
                    return JudgeParse::Unparseable;
                }""",
                """                // MUTATION: 引用校验被移除""",
            )
        ],
    ),
    Mutant(
        name="判官：VERDICT 改成前缀匹配",
        package="ys-coding-agent",
        desc="`METRICALLY` 被读成 `MET` ⇒ 最危险的假阳性方向",
        expect_red="verdict_requires_whole_word",
        edits=[
            (
                JUDGE,
                """        return match token.to_ascii_uppercase().as_str() {
            "MET" => Some(Verdict::Met),""",
                """        return match token.to_ascii_uppercase().as_str() {
            v if v.starts_with("MET") => Some(Verdict::Met),""",
            )
        ],
    ),
    Mutant(
        name="判官：漏核未完成项也放行（覆盖面检查去掉）",
        package="ys-coding-agent",
        desc="判官只核一项就放行整份清单 ⇒ 「标 completed 但没凭据」的项蒙混过关",
        expect_red="met_must_cover_every_open_item",
        edits=[
            (
                JUDGE,
                """    if !missing.is_empty() {""",
                """    if false && !missing.is_empty() {""",
            )
        ],
    ),
    Mutant(
        name="判官：MISSING 被当成「不可靠」而非「未达成」",
        package="ys-coding-agent",
        desc="判官合法报告「某项没凭据」被记成判官坏了 ⇒ 连续两次即误停机",
        expect_red="met_with_missing_item_downgrades_to_not_met",
        edits=[
            (
                JUDGE,
                """            "MISSING" => {
                let label = parts[0];
                return JudgeParse::NotMet {
                    reason: format!("判官报告该项缺少凭据：{label}"),
                };
            }""",
                """            "MISSING" => {
                return JudgeParse::Unparseable;
            }""",
            )
        ],
    ),
    # ── 注入判据 ──────────────────────────────────────────────────────
    Mutant(
        name="注入：去掉「历史里找不到标记」的自愈分支",
        package="ys-coding-agent",
        desc="压缩（含手敲 /compact）吃掉注入块后永不补回 ⇒ 长任务中途失明",
        expect_red="lost_marker_reinjects",
        edits=[
            (
                INJECT,
                """    !session.iter().any(contains_marker)""",
                """    false""",
            )
        ],
    ),
    Mutant(
        name="注入：去掉 revision 检查",
        package="ys-coding-agent",
        desc="清单变了也不重注 ⇒ 模型一直看着旧版本，按已作废的计划干活",
        expect_red="revision_change_injects_even_if_marker_present",
        edits=[
            (
                INJECT,
                """    if last_injected_rev != Some(store.revision()) {
        return true;
    }""",
                """    // MUTATION: revision 检查被移除""",
            )
        ],
    ),
    Mutant(
        name="注入：空清单也注入",
        package="ys-coding-agent",
        desc="没有计划时白注一份空块 ⇒ 每回合多花 token 且暗示「有计划」",
        expect_red="empty_plan_never_injects",
        edits=[
            (
                INJECT,
                """    if store.snapshot().is_empty() {
        return false;
    }""",
                """    // MUTATION: 非空检查被移除""",
            )
        ],
    ),
]


class InjectionError(RuntimeError):
    """注入本身失败（文本不匹配 / 不唯一）—— 绝不能当成「测试通过」。"""


def apply_edits(edits: list[tuple[str, str, str]]) -> None:
    """按精确文本替换注入。`old` 必须**恰好命中一次**。"""
    for rel, old, new in edits:
        path = ROOT / rel
        src = path.read_text(encoding="utf-8")
        count = src.count(old)
        if count == 0:
            raise InjectionError(f"{rel}: 找不到待替换文本（源码已变？）\n---\n{old[:200]}")
        if count > 1:
            raise InjectionError(f"{rel}: 待替换文本命中 {count} 次（须唯一）")
        path.write_text(src.replace(old, new), encoding="utf-8")


def restore(snapshot: dict[str, str]) -> None:
    for rel, content in snapshot.items():
        (ROOT / rel).write_text(content, encoding="utf-8")


def run_package_tests(package: str) -> tuple[str, list[str]]:
    """跑整包测试。

    返回 (状态, 失败的用例名列表)：
      - ``"red"``          —— 编译通过且**有**用例失败（真正的「被抓住」）。
      - ``"green"``        —— 编译通过且全绿（**覆盖缺口**）。
      - ``"build-failed"`` —— 编译不过（注入写法有问题，不算被抓住）。
      - ``"errored"``      —— 跑起来了但非零退出、且没有 FAILED 行（崩溃/挂住）。
    """
    proc = subprocess.run(
        ["cargo", "test", "-p", package, "--", "--test-threads=1"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    failed: list[str] = []
    for line in proc.stdout.splitlines():
        m = re.match(r"^test\s+(\S+)\s+\.\.\. FAILED", line)
        if m:
            failed.append(m.group(1))
    if failed:
        return "red", failed
    if "could not compile" in proc.stderr or re.search(r"^error\[E", proc.stderr, re.M):
        return "build-failed", []
    if "running " not in proc.stdout:
        return "build-failed", []
    if proc.returncode != 0:
        return "errored", []
    return "green", []


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("ids", nargs="*", help="只跑指定编号（1 起）")
    ap.add_argument("--list", action="store_true", help="只列清单")
    args = ap.parse_args()

    if args.list:
        for i, m in enumerate(MUTANTS, 1):
            print(f"{i:2}. [{m.package}] {m.name}")
            print(f"    {m.desc}")
        return 0

    selected = MUTANTS
    if args.ids:
        try:
            wanted = {int(x) for x in args.ids}
        except ValueError:
            print("编号必须是整数", file=sys.stderr)
            return 2
        selected = [m for i, m in enumerate(MUTANTS, 1) if i in wanted]
        if not selected:
            print("没有匹配的编号", file=sys.stderr)
            return 2

    snapshot = {rel: (ROOT / rel).read_text(encoding="utf-8") for rel in TARGETS}
    results: list[tuple[Mutant, str, list[str]]] = []

    try:
        for i, m in enumerate(MUTANTS, 1):
            if m not in selected:
                continue
            print(f"\n=== [{i}] {m.name} ===", flush=True)
            try:
                apply_edits(m.edits)
            except InjectionError as err:
                restore(snapshot)
                print(f"  注入失败：{err}", file=sys.stderr)
                results.append((m, "injection-failed", []))
                continue
            status, failed = run_package_tests(m.package)
            restore(snapshot)
            if status == "red":
                hit = [f for f in failed if m.expect_red in f] if m.expect_red else failed
                if m.expect_red and not hit:
                    print(
                        f"  ! 变红了，但**不是**期望的用例（期望含 {m.expect_red!r}）："
                        f"{', '.join(failed[:5])}"
                    )
                    status = "wrong-catcher"
                else:
                    print(f"  ✓ 被抓住（{len(failed)} 个用例红）：{', '.join(failed[:5])}")
            elif status == "build-failed":
                print("  ! 编译失败 —— 注入写法有问题，不算被抓住")
            elif status == "errored":
                print("  ! 测试非零退出但无 FAILED 行（崩溃/挂住）—— 需人工看")
            else:
                print("  ✗ 未被抓住 —— 覆盖缺口！")
            results.append((m, status, failed))
    finally:
        restore(snapshot)

    print("\n" + "=" * 72)
    print("变异测试汇总")
    print("=" * 72)
    caught = 0
    for m, status, failed in results:
        mark = {"red": "✓"}.get(status, "✗")
        if status == "red":
            caught += 1
        print(f"{mark} [{m.package}] {m.name}")
        if failed:
            print(f"    抓住它的用例：{', '.join(failed[:3])}")
        elif status == "injection-failed":
            print("    注入失败（源码已变，需更新本脚本）")
        elif status == "build-failed":
            print("    编译失败（注入写法有问题，不算被抓住）")
        elif status == "errored":
            print("    测试非零退出但无 FAILED 行（崩溃/挂住）—— 需人工看")
        else:
            print("    无用例变红 —— 这条逻辑没有被测试罩住")
    print("=" * 72)
    print(f"{caught}/{len(results)} 处变异被测试抓住")
    return 0 if caught == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
