#!/usr/bin/env python3
"""`/goal` 推进判据的**变异测试**：注入 bug ⇒ 期望用例变红 ⇒ 还原。

为什么要有它：本层的不变量全是「看代码一眼觉得对」的那一类 ——
判据 4 的反转（有目标 + 空清单 ⇒ `Push`）、收卷回合不计入 200、上限只对有目标生效、
注入块字节恒定。删掉任何一处都不会让别的用例失败。AGENTS.md 约定「关键逻辑要做变异测试」，
这把尺子就是那把尺子。设计条目见 `docs/arch/goal-command/design.md` §8。

形态对齐 `scripts/cancellation-mutation.py`（同一套机制，便于维护）：

  - **一次跑完全部 mutant**，跑完给一张汇总表（红 / 逃逸 / 锚点未命中 / 编译失败）。
  - 每个 mutant 跑**包级**测试（`cargo test -p <pkg>`）而非单个具名用例 ——
    覆盖更强，且能报出「哪些用例抓到了它」。`expect_red` 是**必须命中**的子串：
    变红了但不是它 ⇒ 记「抓错人」，仍算逃逸。
  - 注入用**精确文本替换**（不是行号/正则），`old` 必须恰好命中一次；
    找不到（**锚点未命中**）或命中多次 ⇒ 直接报错退出 —— 这是脚本随代码漂移的唯一信号。
  - 每个 mutant 前后**快照/还原**；`finally` 兜底，异常也不留脏工作区。

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

GOAL = "apps/coding-agent/src/goal.rs"
GOAL_STATE = "apps/coding-agent/src/goal_state.rs"
ADVANCE = "apps/coding-agent/src/turn_advance.rs"
APP_LOOP = "apps/coding-agent/src/app_loop.rs"
INJECT = "apps/coding-agent/src/todo_inject.rs"

# 全部待注入文件（跑前快照、跑完统一还原）。
#
# 只在**本层自己的实现文件**里注入。`app_loop.rs` 由同波另一个人在并行编辑：
# 本脚本会临时改写它，故跑之前先看它的 mtime / md5 是否稳定（见 `_preflight`）。
TARGETS = [GOAL, GOAL_STATE, ADVANCE, APP_LOOP, INJECT]


@dataclass
class Mutant:
    """一处注入。

    `edits` 是 `(文件, 原文, 替换)` 三元组；`old` 必须是**精确**文本，在目标文件里
    必须恰好出现一次，否则注入直接失败（不算「测试通过」）。

    `expect_red` 是期望变红的用例名子串（如 `goal::tests::foo`）。
    """

    name: str
    package: str
    desc: str
    expect_red: str
    edits: list[tuple[str, str, str]] = field(default_factory=list)


MUTANTS: list[Mutant] = [
    # ── ① 判据 4：没有清单 ≠ 达成（D-A 的核心修补） ────────────────────
    Mutant(
        name="① 判据 4 反转失效：有目标 + 从未写过清单也报 NoPlan",
        package="ys-coding-agent",
        desc="用户设了目标、模型还没拆清单 ⇒ 运行时当场 Stop(NoPlan) 收场，"
        "目标被静默忽略（退回成立即放弃）。这条判据是本设计唯一的『目标不会白设』保证。",
        expect_red="goal::tests::never_written_plan_pushes_only_with_goal",
        edits=[
            (
                GOAL,
                """        return match goal {
            Some(_) => FollowUp::Push,
            None => FollowUp::Stop(StopCause::NoPlan),
        };""",
                """        // MUTATION: 有目标也报 NoPlan（判据 4 反转失效）
        return FollowUp::Stop(StopCause::NoPlan);""",
            )
        ],
    ),
    # ── ② 收卷回合不计入 200（D-M / D-N） ──────────────────────────────
    Mutant(
        name="② 收卷回合计入 200：收卷之前先 consume()",
        package="ys-coding-agent",
        desc="收卷回合记账 ⇒ 收尾报账变成「已用 201 回合」，任务级 200 上限被自己突破一次；"
        "此后任何按 `turns()` 算的读数都偏移一格。",
        expect_red="app_loop::tests::wrap_up_turn_runs_and_does_not_consume",
        edits=[
            (
                APP_LOOP,
                """                if let Some(g) = goal.as_ref().filter(|_| need_wrap_up) {
                    let wrap = Message {""",
                """                if let Some(g) = goal.as_mut().filter(|_| need_wrap_up) {
                    // MUTATION: 收卷回合也走一次 consume（设计：它不计入 200）
                    g.consume();
                    let wrap = Message {""",
            )
        ],
    ),
    # ── ③ 上限只对有目标生效（D-K） ───────────────────────────────────
    Mutant(
        name="③ 无目标也设 200：`advance` 里 `used` 到了上限也判耗尽",
        package="ys-coding-agent",
        desc="无目标追问（一期的 HEAD 追问）被加上 200 回合阀 ⇒ 与 D-K 直接冲突；"
        "而它本来靠判据 2/3/4 收场，多这一条只会让人反复遮蔽其中一条。",
        expect_red="turn_advance::tests::no_goal_never_final_reports",
        edits=[
            (
                ADVANCE,
                """        FollowUp::Push => match goal {
            Some(g) if g.exhausted() => Advance::FinalReport,
            _ => Advance::Push,
        },""",
                """        FollowUp::Push => match goal {
            Some(g) if g.exhausted() => Advance::FinalReport,
            // MUTATION: 无目标也按回合数判上限（D-K 只对有目标生效）
            None if state.used >= crate::goal_state::GOAL_TURN_CAP => Advance::FinalReport,
            _ => Advance::Push,
        },""",
            )
        ],
    ),
    # ── ④ 注入块字节恒定（前缀缓存前提） ──────────────────────────────
    Mutant(
        name="④ 目标块含回合序号：`marker_text()` 里带上回合数",
        package="ys-coding-agent",
        desc="注入块每回合都变 ⇒ 每回合都是一笔全价尾字节（前缀缓存全破），"
        "而模型知道回合序号也不能改变行为（结局由 harness 判定）。",
        expect_red="goal_state::tests::marker_text_is_byte_identical_across_calls",
        edits=[
            (
                GOAL_STATE,
                """             </goal_brief>",
            text = self.text""",
                """             </goal_brief>
             （第 {turns} 回合）",
            text = self.text,
            turns = self.turns""",
            )
        ],
    ),
    # ── ⑤ 判据 3：两种收场必须可区分（R1 的可观测面） ─────────────────
    Mutant(
        name="⑤ `GoalReached` 错报 `AllDone`",
        package="ys-coding-agent",
        desc="有目标的达成被报成普通回合收尾 ⇒ UI 无法区分「目标达成」与「清单清空」，"
        "`/goal` 的结果对用户不可见。",
        expect_red="goal::tests::no_open_items_reports_goal_reached_or_all_done",
        edits=[
            (
                GOAL,
                """        return FollowUp::Stop(match goal {
            Some(_) => StopCause::GoalReached,
            None => StopCause::AllDone,
        });""",
                """        // MUTATION: 有目标也错报 AllDone
        return FollowUp::Stop(StopCause::AllDone);""",
            )
        ],
    ),
    # ── ⑥ stall 残留（等价注入，不是逐字复现） ────────────────────────
    Mutant(
        name="⑥ stall 残留：把 `stall_digest` / `StallTracker` 判据加回 `decide`",
        package="ys-coding-agent",
        desc="【等价注入】stall 检测（集合指纹 + 连续 N 回合不变 ⇒ 停机）已随 T21 整体删除，"
        "当前没有可逐字复原的注入点。设计 §8 要验的是『有人把它加回来时测试会不会红』，"
        "故此处注入它**最危险的形态**：`used >= 4` 且清单无 open 项就直接 Stop(NotContinuable)，"
        "即用回合数误杀正常收场（stall 判据当年正是这样绕过判据 3/4 的）。",
        expect_red="goal::tests::used_never_changes_the_verdict",
        edits=[
            (
                GOAL,
                """    // ── 判据 3：无 open 项且曾经写过 ⇒ 正常收场 ───────────────────────""",
                """    // MUTATION: stall 残留 —— 用回合数误杀正常收场（等价注入，非逐字复现）
    if used >= 4 && !store.has_open_items() {
        return FollowUp::Stop(StopCause::NotContinuable);
    }

    // ── 判据 3：无 open 项且曾经写过 ⇒ 正常收场 ───────────────────────""",
            )
        ],
    ),
    # ── ⑦ 无清单提醒封顶 ─────────────────────────────────────────────
    Mutant(
        name="⑦ 提醒不封顶：`plan_remind()` 去掉上限判断",
        package="ys-coding-agent",
        desc="同一句「还没写清单」无限重复进上下文 ⇒ 每回合白烧 token 且稀释真正的追问文案；"
        "它不改变判据，只会污染长任务的前缀。",
        expect_red="goal_state::tests::plan_remind_caps_at_limit",
        edits=[
            (
                GOAL_STATE,
                """        if self.plan_reminders >= NO_PLAN_REMIND_LIMIT {
            return None;
        }""",
                """        // MUTATION: 提醒不封顶""",
            )
        ],
    ),
    # ── ⑧ 收卷回合丢目标原文（等价替换，不是「削工具」） ──────────────
    Mutant(
        name="⑧ 收卷回合丢目标原文（替换设计 §8 的『收卷回合被削工具』）",
        package="ys-coding-agent",
        desc="【等价替换】设计 §8 第 8 条要的是「收卷回合工具被削」。当前结构下**无法注入**："
        "工具表由 `agent` 内部持有，`run_turn_chain` 的参数里没有工具表，追问与收卷两个"
        "调用点传的是同一个 `agent` —— 从调用点改不动工具面（D-N 因此由结构保证，"
        "`request.tools` 直取 registry）。故换成同一 D-N 目的下可执行的变异："
        "收卷输入丢掉目标原文 ⇒ 收卷回合看不到原话，只能凭记忆汇报，正是 D-N 要防的。",
        expect_red="goal_state::tests::wrap_up_text_quotes_goal",
        edits=[
            (
                GOAL_STATE,
                """             清单里仍未完成的项要逐项点到，不要含糊带过。",
            text = self.text""",
                """             清单里仍未完成的项要逐项点到，不要含糊带过。",
            text = "",""",
            ),
        ],
    ),
    # ── 注入判据（todo 块） —— 跟进 `follow_up_text` 的第 4 形参 ───────
    Mutant(
        name="注入：去掉「历史里找不到标记」的自愈分支",
        package="ys-coding-agent",
        desc="压缩（含手敲 /compact）吃掉注入块后永不补回 ⇒ 长任务中途失明",
        expect_red="todo_inject::tests::lost_marker_reinjects",
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
        expect_red="todo_inject::tests::revision_change_injects_even_if_marker_present",
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
        expect_red="todo_inject::tests::empty_plan_never_injects",
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
    """注入本身失败（锚点找不到 / 不唯一）—— 绝不能当成「测试通过」。"""


def apply_edits(edits: list[tuple[str, str, str]]) -> None:
    """按精确文本替换注入。`old` 必须**恰好命中一次**。"""
    for rel, old, new in edits:
        path = ROOT / rel
        src = path.read_text(encoding="utf-8")
        count = src.count(old)
        if count == 0:
            raise InjectionError(
                f"{rel}: 锚点未命中（找不到待替换文本，源码已变？）\n---\n{old[:200]}"
            )
        if count > 1:
            raise InjectionError(f"{rel}: 锚点命中 {count} 次（须唯一）\n---\n{old[:200]}")
        path.write_text(src.replace(old, new), encoding="utf-8")


def snapshot() -> dict[str, str]:
    return {rel: (ROOT / rel).read_text(encoding="utf-8") for rel in TARGETS}


def restore(snap: dict[str, str]) -> None:
    for rel, content in snap.items():
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


def _preflight() -> bool:
    """跑之前的并发写入检查：被别处改着的文件不该由本脚本临时改写（会互相覆盖）。

    取两次 mtime + md5 采样（间隔 3 s），有变化即视为「正在被写」。
    """
    import hashlib
    import time

    def stamp() -> dict[str, tuple[int, str]]:
        out = {}
        for rel in TARGETS:
            path = ROOT / rel
            if not path.exists():
                out[rel] = (-1, "")
                continue
            st = path.stat()
            digest = hashlib.md5(path.read_bytes()).hexdigest()
            out[rel] = (st.st_mtime_ns, digest)
        return out

    first = stamp()
    time.sleep(3)
    second = stamp()
    busy = [rel for rel in TARGETS if first[rel] != second[rel]]
    if busy:
        print(
            "！以下文件正在被写入，先等它稳定再跑（本脚本会临时改写它们）：\n  "
            + "\n  ".join(busy),
            file=sys.stderr,
        )
        return False
    return True


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("ids", nargs="*", help="只跑指定编号（1 起）")
    ap.add_argument("--list", action="store_true", help="只列清单")
    ap.add_argument("--force", action="store_true", help="跳过并发写入预检")
    args = ap.parse_args()

    if args.list:
        for i, m in enumerate(MUTANTS, 1):
            print(f"{i:2}. [{m.package}] {m.name}")
            print(f"    期望变红：{m.expect_red or '（不要求具名用例，只要求整包不绿）'}")
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

    if not args.force and not _preflight():
        return 2

    snap = snapshot()
    results: list[tuple[Mutant, str, list[str]]] = []

    try:
        for i, m in enumerate(MUTANTS, 1):
            if m not in selected:
                continue
            print(f"\n=== [{i}] {m.name} ===", flush=True)
            try:
                apply_edits(m.edits)
            except InjectionError as err:
                restore(snap)
                print(f"  ✗ 锚点未命中：{err}", file=sys.stderr)
                results.append((m, "anchor-missed", []))
                continue
            status, failed = run_package_tests(m.package)
            restore(snap)
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
        restore(snap)

    print("\n" + "=" * 72)
    print("变异测试汇总")
    print("=" * 72)
    caught = 0
    for m, status, failed in results:
        if status == "red":
            caught += 1
        mark = {"red": "✓ 红", "green": "✗ 逃逸"}.get(status, "✗ " + status)
        print(f"{mark}  {m.name}")
        if failed:
            print(f"        抓住它的用例：{', '.join(failed[:3])}")
        elif status == "anchor-missed":
            print("        锚点未命中（源码已变，需更新本脚本）")
        elif status == "build-failed":
            print("        编译失败（注入写法有问题，不算被抓住）")
        elif status == "wrong-catcher":
            print("        红了但不是期望的用例（抓错人）")
        elif status == "errored":
            print("        测试非零退出但无 FAILED 行（崩溃/挂住）—— 需人工看")
        elif status == "green":
            print("        无用例变红 —— 这条逻辑没有被测试罩住")
    print("=" * 72)
    print(f"{caught}/{len(results)} 变红")
    return 0 if caught == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
