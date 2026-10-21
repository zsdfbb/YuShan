#!/usr/bin/env python3
"""取消链路（ADR-0046）的**变异测试**：注入 bug ⇒ 期望测试变红 ⇒ 还原。

为什么要有它：取消链路横跨适配器 / `ys-loop` / TUI 三层，**全绿**的测试套件
仍可能对它零覆盖（顺序颠倒、探针缺失、边界副本化 —— 这些 bug 不会让任何
既有测试失败）。AGENTS.md 约定「关键逻辑要做变异测试」，这把尺子就是那把尺子。

设计要点：
  - **一次跑完全部 mutant**，不逐个手动驱动；跑完给一张汇总表。
  - 每个 mutant 跑**包级**测试（`cargo test -p <pkg>`）而非单个具名用例 ——
    覆盖更强，且能报出「哪些用例抓到了它」。
  - 注入用**精确文本替换**（不是行号/正则），改不动就**报错退出**，
    绝不允许「注入没生效 ⇒ 测试绿 ⇒ 假通过」。
  - 每个 mutant 前**快照**、跑完**还原**；`finally` 兜底，异常也不留脏工作区。
  - 变异体清单写在同一文件里，可复跑、可 review。

用法：
    python3 scripts/cancellation-mutation.py            # 跑全部
    python3 scripts/cancellation-mutation.py --list     # 只列清单
    python3 scripts/cancellation-mutation.py 1 4 7      # 只跑指定编号
"""
from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
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


# 全部待注入文件（跑完统一还原）。
TARGETS = [
    "crates/ys-loop/src/basic.rs",
    "crates/ys-model/src/lib.rs",
    "crates/ys-model/src/request.rs",
    "crates/ys-tui-coding/src/events.rs",
    "adapters/model-openai-compatible/src/lib.rs",
]


MUTANTS: list[Mutant] = [
    Mutant(
        name="适配器发送阶段的 select! 去掉（退化为直接 await）",
        package="ys-model-openai-compat",
        desc="已 abort 的 boundary 仍会把请求发上线 ⇒ 取消请求形同虚设",
        expect_red="already_aborted_boundary",
        edits=[
            (
                "adapters/model-openai-compatible/src/lib.rs",
                """            biased;
            _ = wait_abort(boundary) => SendOutcome::Cancelled,
            result = self.send(url, chat_request) => SendOutcome::Sent(result),""",
                """            biased;
            _ = std::future::pending::<()>() => SendOutcome::Cancelled,
            result = self.send(url, chat_request) => SendOutcome::Sent(result),""",
            )
        ],
    ),
    Mutant(
        name="适配器流式阶段的中止分支去掉",
        package="ys-model-openai-compat",
        desc="流式读取期间 abort 不再中断 ⇒ 已吐 delta 也拿不回 partial",
        expect_red="cancelled_after_delta_carries_partial_text",
        edits=[
            (
                "adapters/model-openai-compatible/src/lib.rs",
                """            biased;
            _ = wait_abort(boundary) => StreamOutcome::Cancelled,""",
                """            biased;
            _ = std::future::pending::<()>() => StreamOutcome::Cancelled,""",
            )
        ],
    ),
    Mutant(
        name="into_partial 恒返回 None",
        package="ys-model-openai-compat",
        desc="中止时已产出的文本被丢弃 ⇒ 用户付费拿到的内容消失",
        expect_red="cancelled_after_delta_carries_partial_text",
        edits=[
            (
                "adapters/model-openai-compatible/src/lib.rs",
                """    fn into_partial(self) -> Option<Message> {
        let block = text_block(self.text)?;
        Some(Message {
            role: Role::Assistant,
            content: vec![block],
        })
    }""",
                """    fn into_partial(self) -> Option<Message> {
        let _ = self;
        None
    }""",
            )
        ],
    ),
    Mutant(
        name="into_partial 多吐 ToolUse（带出未完成的工具调用）",
        package="ys-model-openai-compat",
        desc="中止在工具参数途中 ⇒ partial 里出现参数残缺的 ToolUse",
        expect_red="cancelled_with_partial_tool_call_carries_no_tool_use",
        edits=[
            (
                "adapters/model-openai-compatible/src/lib.rs",
                """    fn into_partial(self) -> Option<Message> {
        let block = text_block(self.text)?;
        Some(Message {
            role: Role::Assistant,
            content: vec![block],
        })
    }""",
                """    fn into_partial(self) -> Option<Message> {
        let mut content = Vec::new();
        if let Some(block) = text_block(self.text) {
            content.push(block);
        }
        for (_, accumulator) in self.tool_calls {
            if !accumulator.name.is_empty() {
                content.push(accumulator.into_content_block(&|_| {}));
            }
        }
        if content.is_empty() {
            return None;
        }
        Some(Message {
            role: Role::Assistant,
            content,
        })
    }""",
            )
        ],
    ),
    Mutant(
        name="退避退回裸 sleep（退避期间不可取消）",
        package="ys-model-openai-compat",
        desc="重试退避期间按 Esc 无效 ⇒ 用户要等满整个退避窗口",
        expect_red="cancellation_during_backoff_returns_promptly",
        edits=[
            (
                "adapters/model-openai-compatible/src/lib.rs",
                """                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = wait_abort(boundary) => {
                            return Err(ModelError::Cancelled { partial: None });
                        }
                    }""",
                """                    tokio::time::sleep(delay).await;""",
            )
        ],
    ),
    Mutant(
        name="ys-loop 的 Cancelled 分流去掉（落回通用 Err）",
        package="ys-loop",
        desc="用户主动取消被当成故障 ⇒ 发 RunFailed（红色失败）",
        expect_red="model_cancelled_finishes_with_cancelled_and_never_run_failed",
        edits=[
            (
                "crates/ys-loop/src/basic.rs",
                """                Err(ys_model::ModelError::Cancelled { partial }) => {""",
                """                Err(ys_model::ModelError::Cancelled { partial }) if false => {""",
            )
        ],
    ),
    Mutant(
        name="「先 append partial 再收场」的顺序颠倒",
        package="ys-loop",
        desc="先发 RunFinished、后 append ⇒ 消费端已判定回合结束，partial 无人接收",
        expect_red="model_cancelled_appends_partial_before_run_finished",
        edits=[
            (
                "crates/ys-loop/src/basic.rs",
                """                    let final_message = partial.clone();
                    if let Some(partial) = partial {
                        ctx.session
                            .append(partial)
                            .await
                            .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                    }
                    emit(
                        ctx.events,
                        AgentEvent::RunFinished {
                            stop_reason: StopReason::Cancelled,
                            usage: total_usage.clone(),
                            rounds,
                        },
                    )
                    .await
                    .map_err(LoopError::Event)?;""",
                """                    let final_message = partial.clone();
                    emit(
                        ctx.events,
                        AgentEvent::RunFinished {
                            stop_reason: StopReason::Cancelled,
                            usage: total_usage.clone(),
                            rounds,
                        },
                    )
                    .await
                    .map_err(LoopError::Event)?;
                    if let Some(partial) = partial {
                        ctx.session
                            .append(partial)
                            .await
                            .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                    }""",
            )
        ],
    ),
    Mutant(
        name="Esc 的系统行去掉",
        package="ys-tui-coding",
        desc="按 Esc 后界面纹丝不动 ⇒ 用户分不清「没生效」与「正在取消」",
        expect_red="test_esc_while_turning_appends_system_line",
        edits=[
            (
                "crates/ys-tui-coding/src/events.rs",
                """                app.transcript
                    .push(crate::transcript::TranscriptLine::System(
                        "已请求取消，正在停止…".into(),
                    ));""",
                """                // MUTATION: 系统行被去掉""",
            )
        ],
    ),
    Mutant(
        name="Esc 置 abort_requested（破坏「Esc 非两段式」）",
        package="ys-tui-coding",
        desc="按完 Esc 再按 Ctrl-C 会直接退出进程，跳过「先取消」这一步",
        expect_red="test_esc_does_not_set_abort_requested",
        edits=[
            (
                "crates/ys-tui-coding/src/events.rs",
                """            if app.is_turning || app.command_in_flight {
                request_abort(boundaries);""",
                """            if app.is_turning || app.command_in_flight {
                app.abort_requested = true;
                request_abort(boundaries);""",
            )
        ],
    ),
    Mutant(
        name="工具之间的 abort 探针去掉",
        package="ys-loop",
        desc="一批工具里第一个跑完后，后续工具仍会继续执行",
        expect_red="abort_between_tools_stops_the_next_tool",
        edits=[
            (
                "crates/ys-loop/src/basic.rs",
                """                    if ctx.boundary.as_ref().is_some_and(|b| b.is_aborted()) {
                        outcomes[i] = Some(CoreToolResult {
                            content: "tool not executed: turn cancelled".to_string(),
                            is_error: true,
                        });
                        continue;
                    }""",
                """                    // MUTATION: abort 探针被去掉""",
            )
        ],
    ),
    Mutant(
        name="boundary 参与序列化",
        package="ys-model",
        desc="挂载边界源改变了请求的线上字节 ⇒ 既有序列化契约被破坏",
        expect_red="model_request_serialization_ignores_boundary",
        edits=[
            (
                "crates/ys-model/src/request.rs",
                """    #[serde(skip, default)]
    pub boundary: Option<Arc<dyn BoundarySource>>,""",
                """    #[serde(
        default,
        skip_deserializing,
        serialize_with = "serialize_boundary_placeholder"
    )]
    pub boundary: Option<Arc<dyn BoundarySource>>,""",
            ),
            (
                "crates/ys-model/src/request.rs",
                """/// provider 的 finish_reason **归一化后**的形态。""",
                """fn serialize_boundary_placeholder<S>(
    value: &Option<Arc<dyn BoundarySource>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(_) => serializer.serialize_str("present"),
        None => serializer.serialize_none(),
    }
}

/// provider 的 finish_reason **归一化后**的形态。""",
            ),
        ],
    ),
    Mutant(
        name="ModelRequest::clone 产生独立的边界副本",
        package="ys-model",
        desc="clone 后 loop 侧与 model 侧看到不同的 abort 状态 ⇒ 模型侧永远看不到 abort",
        expect_red="model_request_clone_keeps_same_boundary_arc",
        edits=[
            (
                "crates/ys-model/src/request.rs",
                """#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRequest {""",
                """#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelRequest {""",
            ),
            (
                "crates/ys-model/src/request.rs",
                """/// provider 的 finish_reason **归一化后**的形态。""",
                """impl Clone for ModelRequest {
    fn clone(&self) -> Self {
        Self {
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            system: self.system.clone(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            boundary: self
                .boundary
                .as_ref()
                .map(|_| Arc::new(ys_protocol::QueueBoundarySource::new()) as Arc<dyn BoundarySource>),
        }
    }
}

/// provider 的 finish_reason **归一化后**的形态。""",
            ),
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
      - ``"red"``   —— 编译通过且**有**用例失败（真正的「被抓住」）。
      - ``"green"`` —— 编译通过且全绿（**覆盖缺口**）。
      - ``"build-failed"`` —— 编译不过（注入写法有问题，**不算**被抓住）。
      - ``"errored"`` —— 跑起来了但非零退出、且没有 FAILED 行（测试二进制崩溃/挂住）。
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
    # 编译失败：stderr 里会有 cargo/rustc 的编译错误标记。
    if "could not compile" in proc.stderr or re.search(r"^error\[E", proc.stderr, re.M):
        return "build-failed", []
    if "running " not in proc.stdout:
        return "build-failed", []
    if proc.returncode != 0:
        # 跑起来了但非零退出、且没抓到 FAILED 行（如某个测试二进制挂掉）。
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
    results: list[tuple[Mutant, str, list[str], str]] = []

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
                results.append((m, "injection-failed", [], "injection-failed"))
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
            results.append((m, status, failed, "ok"))
    finally:
        restore(snapshot)

    print("\n" + "=" * 72)
    print("变异测试汇总")
    print("=" * 72)
    caught = 0
    for m, status, failed, _ in results:
        mark = {"red": "✓", "wrong-catcher": "!"}.get(status, "✗")
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
