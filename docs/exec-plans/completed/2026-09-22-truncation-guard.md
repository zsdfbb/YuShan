# 截断保护（片 1）执行计划

> 日期：2026-09-22
> 设计计划：[`../design-plans/2026-09-22-truncation-guard.md`](../design-plans/2026-09-22-truncation-guard.md)
> 决策：[ADR-0020](../adr/0020-truncation-guard.md) · as-built：[`../design-final/truncation-guard.md`](../design-final/truncation-guard.md) · 报告：[`../reports/2026-09-22-truncation-guard-report.md`](../reports/2026-09-22-truncation-guard-report.md)
> 波次划分见 §3（同波文件集互不相交）。
> 性质：**事后补录**（实现与验收均已完成，本文为记录归档，非实现前计划）。补录日期：2026-09-22。
> 本切片走 sequential-workflow **quick** 方案、**跳过了写 exec-plan 的阶段** —— 下表是**实际执行的 7 个单元**的如实回填，不是事前的任务拆分。

## 基线

- **基线 ref**: `aeabbaf8d83e3abb59f056f922f147cb3c992e0b`（`master`@`aeabbaf`，工作区干净）
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含 `??` 新文件）
- 结果口径：`cargo test --workspace` 基线 **813 passed / 7 ignored** → 完成后 **832 passed / 0 failed / 7 ignored**（**+19**）

## 1. 任务表

| 单元 | type | 依赖 | 涉及文件 | 描述 |
|---|---|---|---|---|
| **U1** | impl | — | `crates/ys-core/src/{stop.rs,lib.rs}`、`crates/ys-model/src/{request.rs,mock.rs,lib.rs}`、`crates/ys-event/src/{event.rs,lib.rs}` | **类型与事件基座**：`StopReason` 新增第四变体 `Truncated`（serde roundtrip 钉住 `"Truncated"` 字面量）；`ModelResponse.stop_reason: Option<String>` **改名并改类型**为 `finish_reason: FinishReason`（新 `#[non_exhaustive]` 枚举 + `is_truncated()` + `Default = Other(String::new())`）；`ys-event` 新增**非终局**事件 `ResponseTruncated { dropped_tool_calls: usize }`（roundtrip + 非终局解构）；`MockModel` 两个 truncated pusher（`push_truncated_tool_call` / `push_truncated_text`）且既有 pusher 的 `finish_reason` 明确化为 `Stop` / `ToolCalls`。**测试写在同一任务内**（各文件 `#[cfg(test)]`）。 |
| **U2** | impl | — | `docs/adr/0020-truncation-guard.md`（**新建**）、`docs/CONTEXT.md`、`docs/design.md` | **ADR + 文档同步**：ADR-0020（6 条决策 + 方案对比 A/B/C + Consequences，骨架照 `docs/adr/0017`）；`docs/CONTEXT.md`「终止原因」定义由「完成、达到最大轮数、被取消」扩入「**被截断**」；`docs/design.md` §5 失败与终止语义表加一行、§12 测试矩阵 `BasicLoop` 行加「截断保护」。 |
| **U3** | impl | U1 | `crates/ys-loop/src/{basic.rs,compact.rs,summarize.rs,lib.rs}` | **核心：Step 8b 截断保护分支**（位置是承重的：Step 8 提取 `tool_calls` **之后**、`if tool_calls.is_empty()` 判定**之前**）。`TRUNCATED_TOOL_NOTICE` 常量；`is_truncated()` 为真时：发 `ResponseTruncated` → 纯文本则 `RunFinished{Truncated}` 诚实收场 → 有工具批则**整批作废**（每个 `tool_use` 合成 `is_error` `ToolResult`、单条 `Role::User` append、**跳过审批**、`continue`）。**本 crate 改名收尾**（`compact.rs` / `summarize.rs` / `lib.rs` 的 `stop_reason` 构造点）。**测试写在同一任务内**：4 条 — N1 `truncated_tool_calls_are_not_executed`（真 spy）/ N2 `truncated_text_ends_turn_as_truncated`（含事件顺序）/ N3 `non_truncated_tool_calls_still_execute`（回归护栏）/ N4 `truncated_tool_calls_satisfy_pairing_invariant`（配对完整性 + 整批作废，用自建 `TwoTruncatedCallsModel`）。 |
| **U4** | impl | U1 | `adapters/model-openai-compatible/src/lib.rs` | `normalize_finish_reason(raw: Option<String>) -> FinishReason`：方言映射表（`stop`/`end_turn` → `Stop`；`tool_calls`/`tool_use` → `ToolCalls`；`length`/`max_tokens` → `Length`；`content_filter` → `ContentFilter`；**其余 / 缺失 → `Other(原值)`，绝不猜成 `Stop`**）。归一位于流式收敛点 `StreamAccumulator::finish()`。包名是 **`ys-model-openai-compat`**（≠ 目录名）。**测试写在同一任务内**：映射表（含别名 / 未知 / 缺失）+ `is_truncated` 判据 + 合成 SSE 端到端。 |
| **U5** | impl | U1 | `crates/ys-tui-coding/src/{draw.rs,app.rs}` | `draw.rs::stop_symbol` 新增 `Truncated => "✂"`（保留 `_ => "?"` 兜底）；`app.rs` 事件分发加 `ResponseTruncated` 一臂 → 推一行 `TranscriptLine::System` 警告，**不调 `finish_turn()`**（非终局）。**测试写在同一任务内**：符号表 + 「推 System 行且不 `finish_turn`」。 |
| **U6** | impl | U3, U4, U5 | `apps/coding-agent/src/app_loop.rs`、`apps/coding-agent/tests/{integration.rs,truncation_guard.rs（新建）}`、`crates/ys-runtime/tests/run_turn_boundary.rs` | **改名收尾 + 端到端**：`app_loop.rs` 与 `tests/integration.rs` 的 `stop_reason` 构造点同步；`crates/ys-runtime/tests/run_turn_boundary.rs` 的 2 处改名（**偏差 5**：首轮盘点漏了该文件，按 `cargo check` 实际报错补上）。**新建端到端** `tests/truncation_guard.rs::truncated_tool_batch_is_never_executed`（N11，回环 SSE 吐 `finish_reason:"length"` + 一个完整 `write` 当哨兵 + 一个被掐断的 `write`；断言预置文件未被覆盖、残缺文件未创建、`dropped_tool_calls:2`、终局仍 `RunFinished` 且 exit 0、回喂全 `is_error`、`request_count == 2`）。**顺带守 R1**：`every_request_variant_is_handled` 必须仍绿。 |
| **U7** | test | U3 | `crates/ys-loop/src/basic.rs`（**仅 `#[cfg(test)]`**） | **修 deep review 抓到的 P1 弱测试**：N4 首版工具名未注册，删掉整个截断保护分支它**依然全绿**（`calls == 0` 是空断言）⇒ 对特性**零判别力**。修法：两个工具名注册成真 spy（各自计数器）+ 加 `ResponseTruncated { dropped_tool_calls: 2 }` 断言；并补 N2 的**事件顺序断言**（`ResponseTruncated` 先于终局事件，原 **P2-2** 无守护）。**不改任何生产代码**。 |

## 2. 覆盖自查

- **每个 impl 都有配对的 test 任务或同任务测试**：U1 / U3 / U4 / U5 / U6 均在**同任务内**写明测试；
  U3 另配 **U7**（review 后修补弱测试）；U2 是文档任务，无测试。
- **`test` 类型任务**：仅 **U7**（纯 `#[cfg(test)]`，不碰生产代码）。
- **文件真实性**：所有既有路径均已核对存在；**新建**标注 2 处 —— `docs/adr/0020-truncation-guard.md`（U2）、
  `apps/coding-agent/tests/truncation_guard.rs`（U6）。
- **不变量守卫**：`toolset.rs` 不出现在任何单元里（工具仍 **9 个**）；新 `Request` 变体 **0**（仍 **7 个**）；
  新工具 **0**；新 crate **0**；新终局事件 **0**。
- **依赖图无环**：U1 是唯一根（U3 / U4 / U5 依赖它），U6 汇合，U7 只依赖 U3。

## 3. 波次计划

同波内**文件集互不相交**（事后按实际执行的划分回填）。

| 波次 | 单元 | 并集文件 | 说明 |
|---|---|---|---|
| **W0** | U1、U2 | `crates/ys-core/src/{stop.rs,lib.rs}` · `crates/ys-model/src/{request.rs,mock.rs,lib.rs}` · `crates/ys-event/src/{event.rs,lib.rs}` · `docs/adr/0020-truncation-guard.md` · `docs/CONTEXT.md` · `docs/design.md` | 两个**无依赖的根**：类型/事件基座 + ADR/文档。文档单元与代码单元文件集不相交。 |
| **W1** | U3、U4、U5 | `crates/ys-loop/src/{basic.rs,compact.rs,summarize.rs,lib.rs}` · `adapters/model-openai-compatible/src/lib.rs` · `crates/ys-tui-coding/src/{draw.rs,app.rs}` | 三个互不相交的落点，依赖全部落在 W0（U1）。 |
| **W2** | U6 | `apps/coding-agent/src/app_loop.rs` · `apps/coding-agent/tests/{integration.rs,truncation_guard.rs}` · `crates/ys-runtime/tests/run_turn_boundary.rs` | 汇合收口：改名 + 端到端。依赖 U3 / U4 / U5 全部就绪。 |
| **W3** | U7 | `crates/ys-loop/src/basic.rs`（`#[cfg(test)]`） | review 后修补（P1 / P2-2）。跨波复用 U3 的文件，**只动测试模块**。 |

**关键路径**：U1 → U3 → U6 → U7（4 跳）。**最大并行度**：W1 的 3 个单元。

## 4. 验证清单

- [x] **所有 (impl, test) 配对均已 completed**
- [x] `cargo test --workspace` — **832 passed / 0 failed / 7 ignored**（基线 813，**+19**）
- [x] `cargo clippy --all-targets` — **零 warning**
- [x] `cargo check --workspace --all-targets` — 通过
- [x] `cargo test -p ys-coding-agent every_request_variant_is_handled` — 通过（**R1**）
- [x] `cargo test -p ys-coding-agent --test truncation_guard` — **1 passed**（端到端，N11）
- [x] **变异清单 8 条全部被对应用例抓到**，且**全部还原**（`basic.rs` sha256 前后一致）
- [x] `git diff <基线 aeabbaf8>` — **19 改 + 2 新，无计划外改动**
- [ ] `cargo fmt --check` — **不通过，但属仓库既有分歧**（`apps/coding-agent/src/*.rs` 的 CJK 文本与
      本机 rustfmt，与本次改动无关）；**不要跑裸 `cargo fmt`**，它会重排无关文件污染 diff
- [ ] **MANUAL_ACK_REQUIRED**（需真实凭证）— 真实 provider 报 `length` 时目标文件**没被改坏**。**未做**
