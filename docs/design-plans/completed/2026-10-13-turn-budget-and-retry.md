# turn-budget-and-retry 设计计划

> 上游：`docs/arch/turn-output-and-feedback/`（第一批已落地，commit `807f7b6`）。其 `design.md` §4 与 ADR 决策 4 把下列两项列为「第二批」并声明不覆盖。
> 本轮目标（用户确认）：**提高 agent 工作成功率 + 减少 token 消耗**。
> 路径：从零设计 → 用户已签字放行（`full` 编排）。

## 背景

第一批把「单条工具结果规模」收进边界、让「空产出可见」。第二批收两个尾：

1. **回合级工具输出总量闸门**：第一批只限**单条**（grep 16 KiB / `MAX_BYTES`）；一个回合内**多条**结果仍可累积放大（真实会话 37 条 ≈ 48k token）。需要在 `ys-loop` 里加**回合级**判据，超预算时降级**更早**结果（头部 + 续读指针），把当轮回喂 token 压下来。
2. **空响应重试一次**：第一批把空产出判为 `NoOutput` 并可见；但模型「只有思考、没有正文」而空手收场时，直接 `NoOutput` 仍无产出。加一次催促重发，仍空才如实上报 —— 直接提升「有答复」的概率。

另补 `docs/arch/turn-output-and-feedback/context.md` 范围内第 8 条里被 `design.md` 漏掉的 **过程叙述 guideline**（多轮工具任务每批前一句话叙述正在做什么）。

## 设计

### A · 回合级工具输出总量闸门

**配置**（`crates/ys-component/src/limits.rs`）
- 新增字段 `pub max_turn_tool_result_tokens: usize`，`Default` = `0`（关闭）。
  - 沿用 `fixed_prompt_tokens` 的承重默认惯例：核心默认关闭 ⇒ 既有消费者 / 测试**逐字节不变**。
- app 侧（`apps/coding-agent/src/main.rs::resolve_limits`）显式设 `limits.max_turn_tool_result_tokens = config.context_window() / 4`（128k → 32k；不新增 config 字段）。

**算法**（新模块 `crates/ys-loop/src/turn_budget.rs`；纯函数、零 IO、无 async，照 `compact.rs` 的可变异测试风格）
- 常量：`ELIDE_HEAD_LINES = 40`、`ELIDE_HEAD_BYTES = 2 * 1024`。
- 幂等哨兵：降级后的内容含固定标记 `[earlier tool result elided`，已降级的不再降级。
- 纯函数：
  - `tool_result_tokens(messages: &[Message]) -> usize` —— 一段内所有 `ToolResult` 块的估算 token 之和（复用 `crate::token::estimate_tokens`）。
  - `needs_elision(content: &str) -> bool` —— 非空、未降级、且超过头部预算。
  - `elide_message(msg: &Message) -> Option<Message>` —— 把一条消息里所有 `ToolResult` 块降级为 `ys_tool::head_lines(content, ELIDE_HEAD_LINES, ELIDE_HEAD_BYTES)` + 一条指针 notice（复用 `ys_tool::append_notices`）；**不改变块类型与 `tool_call_id`**（ADR-0001 配对完整性）；无可降级块则返回 `None`。
  - `plan_elisions(messages: &[Message], turn_start: usize, budget: usize) -> Vec<usize>` —— 在 `messages[turn_start..]` 内从**最旧**到最新累计，返回应降级的**绝对下标**（升序），直到该区间工具结果估算 token ≤ `budget`；**最新一条永不降级**；`budget == 0` 或未超预算 ⇒ 返回空。
- 指针文案（固定，便于断言）：`[earlier tool result elided to keep this turn's output bounded (~{kib} KiB dropped). Re-run the tool with a narrower scope (pattern/path/context, or read a line range) if you need the full result]`。

**接线**（`crates/ys-loop/src/basic.rs::run_turn`）
- 回合开始时记 `turn_start = ctx.session.messages().len()`（在 Step 2 append 用户消息**之前**）。
- 每次工具结果批次 append 后（Step 10 之后）：`let plan = turn_budget::plan_elisions(ctx.session.messages(), turn_start, budget)`；非空则重建消息表（对计划内下标 `elide_message`）并 `ctx.session.replace_all(...)`。
- **与压缩的关系**：压缩在 Step 4b 做 `replace_all`。故每次用 `let turn_start = turn_start.min(session.messages().len())` 取值（保守：宁少降级，不误删更早历史）；降级只缩内容、不删消息 ⇒ 不与压缩的窗口触发冲突。
- **不新增 `AgentEvent` 变体**（用户已在 transcript 实时看到工具结果；降级只影响模型侧下一轮请求）。

### B · 空响应重试一次

**配置**：`RunLimits` 新增 `pub retry_empty_response: bool`，默认 `true`。

**接线**（`basic.rs` 的 `tool_calls.is_empty()` 空产出分支）
- 触发条件（全部满足）：`text_content.trim().is_empty()` 且 `response.finish_reason == FinishReason::Stop` 且 `ctx.limits.retry_empty_response` 且本回合尚未重试过（循环内 `empty_retried` 标志）。
- 动作：置 `empty_retried = true`；向会话 append 一条 `Role::User` 催促消息；`continue`（**消耗一个 round**，自动受 `max_rounds` 与轮边界 abort 约束）。
- 催促文案（固定）：`You produced no visible output (no text and no tool call). Respond now: either give your answer, or make a tool call — do not reply with only reasoning.`
- 仍空：按第一批语义收场（`NoOutput`）；`retry_empty_response == false` ⇒ 与第一批**逐字节一致**。

### C · 过程叙述 guideline

`apps/coding-agent/src/prompt.rs::tool_guidelines`：当工具表含「工作型」工具（`read` / `grep` / `find` / `ls` / `bash` / `edit` / `write` 任一）时追加一条：
`For multi-step tool work, briefly say what you are about to do before a batch of tool calls (one short sentence, not a paragraph).`
- 进 `fixed_prompt_tokens`（system prompt 缓存前缀），只加一句。

## 涉及文件

- 修改：
  - `crates/ys-component/src/limits.rs`（+2 字段 + 默认值）
  - `crates/ys-loop/src/lib.rs`（`mod turn_budget;`）
  - `crates/ys-loop/src/basic.rs`（闸门接线 + 重试分支 + 测试）
  - `apps/coding-agent/src/main.rs`（`resolve_limits` 设预算）
  - `apps/coding-agent/src/prompt.rs`（叙述 guideline）
- 新增：
  - `crates/ys-loop/src/turn_budget.rs`
- 删除：无

## 测试策略

### 测试合同（Phase 3 审查依据）

> 由用户签字放行（从零设计路径）。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。

#### N10: 超预算 ⇒ 降级最旧、保留最新、配对完整
- **验收方法**: `cargo test -p ys-loop`（新用例）—— 造本回合多条大工具结果、超预算 ⇒ 断：最旧的被降级（内容 ≤ `ELIDE_HEAD_BYTES` 且含指针标记）、最新的逐字原样、每个 `ToolResult` 的 `tool_call_id` 不变、会话消息条数不变。降级后该回合工具结果估算 token ≤ 预算或只剩最新未降级。
- **优先级**: 必测

#### N11: 预算内 / 预算=0 ⇒ 会话逐字节不变（反向护栏）
- **验收方法**: `cargo test -p ys-loop` —— 同一批工具结果，预算未超 或 `max_turn_tool_result_tokens = 0` ⇒ 会话与不启用闸门时逐字节相同。
- **优先级**: 必测

#### N12: 首轮空 + 次轮正常 ⇒ `Completed`，含催促消息，`rounds == 2`
- **验收方法**: `cargo test -p ys-loop`（`MockModel`：`push_text("")` 后 `push_text("done")`）—— 断 `stop_reason == Completed`、`rounds == 2`、会话里出现催促文案。
- **优先级**: 必测

#### N13: 首轮空 + 次轮仍空 ⇒ `NoOutput`，`rounds == 2`（只重试一次）
- **验收方法**: `cargo test -p ys-loop`（`push_text("")` 两次）—— 断 `stop_reason == NoOutput`、`rounds == 2`；终局事件 `RunFinished{NoOutput}`。
- **优先级**: 必测

#### N14: `retry_empty_response = false` ⇒ 首轮空即 `NoOutput`，`rounds == 1`
- **验收方法**: `cargo test -p ys-loop` —— 与第一批行为一致。
- **优先级**: 必测

#### N15: 幂等 / 不误伤（已降级不二次降级；非工具结果消息不受影响）
- **验收方法**: `cargo test -p ys-loop`（`turn_budget` 单测）。
- **优先级**: 可推迟

#### N16: app 接线 —— `resolve_limits` 设 `max_turn_tool_result_tokens == context_window/4`
- **验收方法**: `cargo test -p ys-coding-agent`（新用例）。
- **优先级**: 必测

#### N17: 默认值钉死 —— `RunLimits::default()` 的 `max_turn_tool_result_tokens == 0`、`retry_empty_response == true`
- **验收方法**: `cargo test -p ys-component`。
- **优先级**: 可推迟

#### N18: 叙述 guideline 就位 —— 含工作型工具时出现、空工具表不出现
- **验收方法**: `cargo test -p ys-coding-agent`（新用例，仿 `narrow_before_broad_guideline_appears_only_with_read_or_grep`）。
- **优先级**: 必测

#### N19: 无回归 —— `cargo test --workspace` 不降级；既有 ys-loop「空响应 ⇒ NoOutput」用例按重试语义更新
- **验收方法**: `cargo test --workspace`。
- **优先级**: 必测

### 测试框架与命令
- 单元测试: `cargo test -p ys-component` / `-p ys-loop` / `-p ys-coding-agent`
- 回归: `cargo test --workspace`
- 质量门: `cargo clippy --all-targets`、`cargo fmt --check`（仅本批改动文件需干净）
- **每个新用例的文档注释里写明变异点**（删 / 改哪行会使它变红）。

## 波及文档
- `docs/CONTEXT.md`（终止语义：重试后仍空仍是 `NoOutput`，不新增变体）
- `docs/design.md` §5（失败与终止语义）/ §12（测试矩阵）
- `docs/design-final/`（as-built：新增 turn-budget-and-retry 记录）
- `docs/arch/turn-output-and-feedback/context.md`（勾掉范围内第 4 / 7 / 8 项）

## 风险与注意事项
- 重试使空路径多一次 model 调用（罕见）；限定 `finish_reason == Stop`，避免对 content-filter 误重试。
- 催促消息持久化 ⇒ 会话可能出现连续两条 `User`（适配器翻译接受，与 resume 现状一致）。
- 闸门降级**丢部分内容**（有指针可重取）；预算（`context_window/4`）是初值，需真实会话校准。
- 叙述 guideline 增大 `fixed_prompt_tokens`，个别断言 prompt 大小的测试可能要更新。
