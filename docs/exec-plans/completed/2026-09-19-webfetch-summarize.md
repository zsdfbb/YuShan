# webfetch 总结能力 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据** — Phase 3 据此合并执行单元并划分波次。

## 上下文引用

参考设计计划：`docs/design-plans/2026-09-19-webfetch-summarize.md`
设计来源：`docs/arch/webfetch-summarize/design.md` + `docs/adr/0018-webfetch-summarize.md`

## 基线

- **基线 ref**: `09a50a69206dd1da63f9c78b3d7ad1d407e8d0b0`（备份分支 `sw-backup/webfetch-summarize` 存原 HEAD `c2aa299`）
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含 `??` 新文件）

## 任务清单

### Task 1: crates/ys-tool — `Summarizer` trait + `ToolContext` 注入点 [type=impl]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/summarizer.rs`（新建）, `crates/ys-tool/src/context.rs`, `crates/ys-tool/src/lib.rs`
- **描述**: 新建 `Summarizer` 窄能力 trait（`async fn summarize(&self, content: &str, prompt: &str) -> Result<String, String>`）；`ToolContext` 加 `pub summarizer: Option<&'a dyn Summarizer>` 字段与 `with_summarizer` builder；`lib.rs` 加 `mod summarizer; pub use summarizer::*;`。**`ToolContext::new()` 签名一字不改**（15 处调用点零改动）。
- **关联测试任务**: T2
- **依赖**: 无

### Task 2: crates/ys-tool — trait 与 builder 的单测 [type=test]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/context.rs`（`#[cfg(test)]`）, `crates/ys-tool/src/summarizer.rs`（`#[cfg(test)]`）
- **描述**: 覆盖 `with_summarizer` 挂载后 `Some`、`new()` 默认 `None`；`Summ*` fake 实现可编译（证明 object-safe）。
- **验证方法**:
  - `cargo test -p ys-tool` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: T1
- **依赖**: T1

### Task 3: crates/ys-loop — `ModelSummarizer` 适配器 + 唯一生产接线 [type=impl]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/summarize.rs`（新建）, `crates/ys-loop/src/lib.rs`, `crates/ys-loop/src/basic.rs`
- **描述**: 新建 `SUMMARIZE_SYSTEM` 常量 + `pub(crate) struct ModelSummarizer<'a> { model: &'a dyn Model }` 实现 `ys_tool::Summarizer`（`tools: vec![]`、`system: SUMMARIZE_SYSTEM`、`max_tokens: 1024`、`temperature: None`；空/全空白响应 ⇒ `Err`）；把 `basic.rs` 里 `generate_summary` 函数内的 `NoopModelEventSink` 提取为 `pub(crate)` 复用；在 `basic.rs:308` 就地构造适配器并 `.with_summarizer(&summarizer)`。
- **关联测试任务**: T4
- **依赖**: T1

### Task 4: crates/ys-loop — 摘要请求形状测试 [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/summarize.rs`（`#[cfg(test)]`）
- **描述**: **新建「记录型 `Model`」测试基建**（记录最后一次 `ModelRequest`）：断言 `tools.is_empty()`、`system` 含 untrusted 框架措辞、请求 `max_tokens == 1024`（注释写明会被 app 的 4096 覆盖，不得当成线上保证）、模型返空文本 ⇒ `Err`、模型返 `Err` ⇒ `Err`。
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: T3
- **依赖**: T3

### Task 5: adapters/tools-web/src — `webfetch` 摘要管线 [type=impl]
- **涉及目录**: `adapters/tools-web/src/`
- **涉及文件**: `adapters/tools-web/src/fetch.rs`, `adapters/tools-web/src/net.rs`
- **描述**: `spec()` 加可选 `prompt` 并改写 description（**保留** `saved` / `path` 短语）；入参解析（缺省/空/全空白 ⇒ `None`，类型不对 ⇒ `ToolError::InvalidInput`）；`SUMMARIZE_BUDGET = 60s`；三分 `enum Outcome { Ok, Skipped, Failed }`；`head_is_header_only`（结构性退化判据）；`summarize_bounded`（`select! { biased; wait_aborted, timeout }`）；`summary_pointer`（**字节**覆盖率）；`net.rs` 的 `wait_aborted` 改 `pub(crate)`。**无 prompt 路径逐字节不变**。
- **关联测试任务**: T6
- **依赖**: T1

### Task 6: adapters/tools-web/tests — 摘要行为与契约 [type=test]
- **涉及目录**: `adapters/tools-web/tests/`
- **涉及文件**: `adapters/tools-web/tests/tools.rs`, `adapters/tools-web/tests/contracts.rs`
- **描述**: 覆盖 N1–N7、N11。⚠️ **测试纪律**：一律注入**独立的假 `Summarizer`**（带 `AtomicUsize` 计数），**绝不**让 `ModelSummarizer` 接 `MockModel`（`mock.rs:65-68` 忽略请求 + FIFO 消费 ⇒ 吃掉主循环响应 / `remove(0)` panic）。
- **验证方法**:
  - `cargo test -p ys-tools-web` — 期望全部 passed，且既有 23 个 fetch 用例零改动通过
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: T5
- **依赖**: T5

### Task 7: apps/coding-agent — 系统提示词同步 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`
- **描述**: 更新 `tool_snippet("webfetch")` 与 `tool_guidelines` 的 webfetch 措辞，提及可选 `prompt` 及其代价（`costs one extra model call — slower`）。**不改**工具数量与顺序。
- **关联测试任务**: T8
- **依赖**: 无

### Task 8: apps/coding-agent — 提示词断言同步 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`（`#[cfg(test)]`）
- **描述**: 更新 snippet / guideline 的逐字断言；**保留** `do not refetch the same URL` 子串断言与「有无 webfetch 才出现」的反向断言。
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: T7
- **依赖**: T7

### Task 9: workspace 全量验证 [type=test]
- **涉及目录**: 全仓
- **涉及文件**: 无（只跑命令）
- **描述**: 全量回归确认无破坏。
- **验证方法**:
  - `cargo test --workspace` — 期望 **passed ≥ 636**、`0 failed`（基线 636）
  - `cargo clippy --all-targets` — 期望零 warning
  - `cargo fmt --check` — 期望通过
- **关联 impl**: T1, T3, T5, T7
- **依赖**: T2, T4, T6, T8

## 波次计划（Phase 3 计算后回填）

> Phase 3 开始时按 3.2 合并单元并分层，回填于此。注意 **T5 与 T6 文件集不相交**（src/ vs tests/），**T7 与 T8 同文件**（`prompt.rs`）⇒ 按 R1 必合并。

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `crates/ys-tool/src/summarizer.rs`, `crates/ys-tool/src/context.rs`, `crates/ys-tool/src/lib.rs` | 无 | W0 | deep |
| U2 | T7, T8 | `apps/coding-agent/src/prompt.rs` | 无 | W0 | shallow |
| U3 | T3, T4 | `crates/ys-loop/src/summarize.rs`, `crates/ys-loop/src/lib.rs`, `crates/ys-loop/src/basic.rs` | U1 | W1 | deep |
| U4 | T5, T6 | `adapters/tools-web/src/fetch.rs`, `adapters/tools-web/src/net.rs`, `adapters/tools-web/tests/tools.rs`, `adapters/tools-web/tests/contracts.rs` | U1 | W1 | deep |
| U5 | T9 | 无（只跑命令） | U1–U4 | W2 | shallow |

> **合并依据**：R4（impl 与其配对 test 同单元）⇒ T1+T2 / T3+T4 / T5+T6 / T7+T8 各成一体。
> **同波文件集互不相交**已逐对核对（W0 的 ys-tool 三文件 vs `prompt.rs`；W1 的 ys-loop 三文件 vs tools-web 四文件）。
> **review 档依据**：U1/U3/U4 触及对外契约（新公开 trait / 生产接线点 / 工具 schema 与 description）= 模型行为输入，故 deep；U2 仅文案、U5 仅跑命令，故 shallow。

## 验证清单
- [ ] **所有 (impl, test) 配对均已 completed**（Phase 3 review 放行条件）
- [ ] `cargo test --workspace` — passed ≥ 636，0 failed
- [ ] `cargo clippy --all-targets` — 零 warning
- [ ] `cargo fmt --check` — 通过
