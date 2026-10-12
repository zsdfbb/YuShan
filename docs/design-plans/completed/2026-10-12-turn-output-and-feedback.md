# turn-output-and-feedback 设计计划

## 背景

- 真实会话 `~/.yushan/sessions/1791776859_054505669.jsonl` 暴露两个问题：① 单条 `grep{pattern:"M4", context:4}`
  结果 **45,255 字符（≈11k token），无任何 notice**；② 空 / 纯思考响应被判成 `StopReason::Completed` +
  `final_message = None`，**静默收场**（用户连问两次「汇报状态」都拿不到答复）。
- 目标：把**单回合工具输出**收进边界、让**回合产出与可见性**可见；两项一起做，且**可量化、可测试**。
- 上游：`docs/arch/turn-output-and-feedback/{context.md, design.md, review.md, adr-turn-output-and-feedback.md}`（用户已签字确认）。

## 设计

**A · 工具输出收窄（主：真收窄；辅：提示）**

1. `grep` 专用结果预算 `GREP_MAX_RESULT_BYTES = 16 KiB`（**收紧**，与「不放宽上限」不冲突）—— 渲染输出按它经
   `ys_tool::head_lines` 截断；**截断 notice 必须带收窄指引**（「收窄 pattern/path/context 或加 `glob`」）。
2. 软阈值 `SOFT_RESULT_BYTES = 8 KiB`：渲染 ≥ 软阈值且未被截断 ⇒ 附 notice（大小 + 命中数 + 收窄建议）；
   `read` 同（`read` 已有 `offset/limit`，**不设**专用预算）。
3. `grep` 的 `context` 钳到 `MAX_CONTEXT = 5` + notice（**非主手段**；不覆盖观察用例 `context=4`）。
4. `apps/coding-agent/src/prompt.rs` 的 `tool_guidelines` 加「先窄后宽」一条。

**B · 空响应可见**

5. 新增 `StopReason::NoOutput`（加法式，`#[non_exhaustive]`）；`BasicLoop` 空分身改判它，判定用
   `text.trim().is_empty()`（**空白响应也算**）。
6. TUI：`RunFinished{NoOutput}` ⇒ 一行**可见** `System` 提示 + `stop_symbol` 分支。
7. `-p/--json`：`single_turn_exit_code` 增 `NoOutput ⇒ 1` + stderr 一行（**不可静默装作成功**）。

**架构决策**：加法式新增变体（照 `Truncated` 先例 ADR-0020）；**不放宽**任何输出上限（收紧合规）；不引依赖；
**不引** `ResultBudget` 配置层与 `AgentEvent` 新变体（本需求内无真实需求）；`context` 先钳制再渲染。

**边界**：传输层空 → `llm-api-unavailable`（应为 `Err`）；合法空 / 纯思考 → 本需求 `NoOutput`；顺序依赖见 design.md §边界。

## 涉及文件

- 新增: 无（全部为修改 + inline 测试）
- 修改:
  - `crates/ys-core/src/stop.rs`, `crates/ys-core/src/lib.rs`
  - `crates/ys-tool/src/truncate.rs`, `crates/ys-tool/src/notice.rs`
  - `adapters/tools-basic/src/grep.rs`, `adapters/tools-basic/src/read.rs`
  - `crates/ys-loop/src/basic.rs`
  - `crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/app.rs`
  - `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/prompt.rs`
  - `docs/CONTEXT.md`, `docs/design.md`（§5 / §12）
- 删除: 无

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条需求一条验收条款。本轮由用户签字确认（阈值取建议初值：16 KiB / 8 KiB / 5）。

#### N1: grep 单条结果 ≤ 16 KiB（真收窄），且截断 notice 带收窄指引
- **验收方法**: `cargo test -p ys-tools-basic` — 新增用例（造 ≥16 KiB 命中集）断言渲染输出 ≤ `GREP_MAX_RESULT_BYTES` 且截断 notice 含收窄指引
- **优先级**: 必测

#### N2: grep 结果 ≥ 软阈值且未截断 ⇒ 必带 notice
- **验收方法**: `cargo test -p ys-tools-basic` — 新用例：≥8 KiB 命中集 ⇒ notice 出现；小集合 ⇒ 不出现
- **优先级**: 必测

#### N3: grep `context` 超限被钳到 `MAX_CONTEXT = 5` + notice
- **验收方法**: `cargo test -p ys-tools-basic` — 传 `context: 999` ⇒ 渲染行数 ≤ `2·5+1` 且 notice 出现
- **优先级**: 必测

#### N4: read 大结果带软阈值 notice
- **验收方法**: `cargo test -p ys-tools-basic`
- **优先级**: 可推迟

#### N5: prompt「先窄后宽」guideline 就位
- **验收方法**: `cargo test -p ys-coding-agent` — 断言 system prompt 含该 guideline 子串；工具表顺序 / 漂移断言不破
- **优先级**: 必测

#### N6: 空 / 空白响应 ⇒ `StopReason::NoOutput`
- **验收方法**: `cargo test -p ys-loop` — `MockModel` 返回 `content=[]` 与 `" "` ⇒ 断 `RunResult.stop_reason == NoOutput`；`cargo test -p ys-core` — serde roundtrip 钉 `"NoOutput"`
- **优先级**: 必测

#### N7: TUI 对 `NoOutput` 推可见 System 行
- **验收方法**: `cargo test -p ys-tui-coding` — 喂该 `RunFinished` ⇒ transcript 出现非空 System 行；`stop_symbol(NoOutput)` 有分支
- **优先级**: 必测

#### N8: `-p/--json` 空响应 ⇒ 退出码 1 + stderr 一行
- **验收方法**: `cargo test -p ys-coding-agent` — `single_turn_exit_code(NoOutput) == 1`（e2e 如可跑则断非零退出 + stderr 非空）
- **优先级**: 必测

#### N9: 反向 —— 非空正常收尾不受影响（无回归）
- **验收方法**: `cargo test --workspace` — 无新增失败（基线全绿）
- **优先级**: 必测

### 测试框架与命令

- 单元测试: `cargo test -p ys-core` / `-p ys-tool` / `-p ys-loop` / `-p ys-tools-basic` / `-p ys-tui-coding` / `-p ys-coding-agent`
- 全量: `cargo test --workspace`
- Lint: `cargo clippy --all-targets`
- 格式: `cargo fmt --check`

## 波及文档

- `docs/CONTEXT.md` — 终止原因（+「无可见产出」）
- `docs/design.md` — §5 失败与终止语义（+ 一行）、§12 测试矩阵
- `docs/arch/llm-api-unavailable/context.md` — 交叉引用（本设计**收窄其场景 4**）
- `docs/design-final/truncation-guard.md`、`docs/design-final/search-tools.md` — Phase 4 核对是否需同期说明

## 风险与注意事项

- `grep` 专用预算会**丢部分有用结果**（靠截断指引缓解）；阈值（16 / 8 KiB、`MAX_CONTEXT = 5`）属初值，需后续按会话复算校准。
- `NoOutput ⇒ 退出码 1` 的口径需与 `llm-api-unavailable` 的「终止粒度」未决项对齐（本批先按 1 实现）。
- 空响应判定**不看 `finish_reason`**：传输层空在 `llm-api-unavailable` 落地前也会显示为 `NoOutput`
  （好于 `Completed`，但**不等同**错误）。
