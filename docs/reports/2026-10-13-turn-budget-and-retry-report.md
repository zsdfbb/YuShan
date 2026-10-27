# 回合级工具输出闸门与空响应重试 — 修改报告

> 日期：2026-10-13 ｜ 基线 `fece752` ｜ 设计 `docs/design-plans/completed/2026-10-13-turn-budget-and-retry.md`
> 上游设计 `docs/arch/turn-output-and-feedback/design.md` ｜ as-built `docs/design-final/turn-budget-and-retry.md`

## 背景

第一批（`807f7b6`）把**单条**工具结果收进边界、让**空产出**可见。第二批收两个尾（目标：提高
agent 工作成功率 + 减少 token 消耗）：

1. **回合级工具输出总量闸门** —— 单条封顶后，一回合内**多条**结果仍会累积放大（真实会话 37 条 ≈ 48k token）。
2. **空响应重试一次** —— 「只有思考、没有正文」仍空手收场时，先催一句再收场，直接抬高「有答复」的概率。

外加一条**过程叙述** guideline（多轮工具任务每批前一句话）。本轮由**主 agent 逐步实现**（本环境 subagent 消息投递不稳定，经用户确认改为主 agent 直接开发）。

## 改动清单（5 个源文件 + 4 个文档）

| 模块 | 文件 | 内容 |
|---|---|---|
| 配置 | `crates/ys-component/src/limits.rs` | `RunLimits` 新增 `max_turn_tool_result_tokens`（默认 0）/ `retry_empty_response`（默认 true）+ N17 测试 |
| 循环 | `crates/ys-loop/src/turn_budget.rs` | **新增**纯算法：`tool_result_tokens` / `needs_elision` / `elide_message` / `plan_elisions`；N10 内核 + N15 单测 |
| 循环 | `crates/ys-loop/src/lib.rs` | `mod turn_budget; pub use turn_budget::*;` |
| 循环 | `crates/ys-loop/src/basic.rs` | `turn_start` 锚；Step 11 闸门接线（`replace_all`）；空产出分支一次性重试；N10–N14 接线测试 |
| 应用 | `apps/coding-agent/src/main.rs` | `resolve_limits`：`max_turn_tool_result_tokens = context_window / 4` + N16 测试 |
| 应用 | `apps/coding-agent/src/prompt.rs` | 过程叙述 guideline（工作型工具在场时）+ N18 测试 |
| 文档 | `docs/CONTEXT.md`、`docs/design.md`、`docs/design-final/`、`docs/arch/turn-output-and-feedback/context.md`、`AGENTS.md` | 终止语义 / §5 / §12 / as-built / 勾掉第二批 / 测试计数 |

**无新依赖、未放宽任何上限**（闸门是**降级**，重试受 `max_rounds` 约束）。

## 测试结果

| 命令 | 结果 |
|---|---|
| `cargo test -p ys-component` | 16 passed（+2） |
| `cargo test -p ys-loop` | 75 passed（+8） |
| `cargo test -p ys-coding-agent` | 294 passed（+2） |
| **`cargo test --workspace`** | **1147 passed / 0 failed / 9 ignored**（基线 1135/0/9） |
| `cargo clippy --all-targets` | 本批改动文件**无新增告警** |
| `cargo fmt --check`（本批文件） | 干净；`main.rs`/`app_loop.rs` 的剩余差异为基线既有 |

## 验收对照（测试合同 N10–N19）

- N10（降最旧 / 保留最新 / 配对完整）：`turn_budget` 单测 + `basic.rs` 接线 ✅
- N11（预算内 / 预算=0 ⇒ 逐字节不变）：`basic.rs` ✅
- N12（首轮空 + 次轮正常 ⇒ `Completed`、`rounds==2`）：✅
- N13（两轮空 ⇒ `NoOutput`、`rounds==2`）：更新既有 `empty_response_*` 用例 ✅
- N14（`retry_empty_response=false` ⇒ 首轮即 `NoOutput`、`rounds==1`）：✅
- N15（幂等 / 不误伤）：`turn_budget` 单测 ✅
- N16（`resolve_limits` 预算 == `context_window/4`）：✅
- N17（默认值钉死）：`ys-component` ✅
- N18（叙述 guideline 就位 / 缺席）：`ys-coding-agent` ✅
- N19（无回归）：`--workspace` 1147/0/9 ✅

## 变异测试（已实测）

- 删 `plan_elisions` 的「最新一条永不降级」保护 ⇒ `turn_budget` 与接线两用例变红。
- 删空产出分支的 `retry_empty_response` 闸门 ⇒ N14 变红。

## 遗留 / MANUAL_ACK_REQUIRED

- [ ] 预算初值（`context_window / 4`；`ELIDE_HEAD_LINES=40` / `ELIDE_HEAD_BYTES=2 KiB`）待下一次真实会话复算校准。
- [ ] 有 provider 凭证时跑一次真实「空响应 ⇒ 催促 ⇒ 有答复」路径做端到端确认（单测已覆盖逻辑）。
- [ ] 一次运行中的**一行 flake**：`app_loop::stale_abort_between_turns_does_not_cancel_review` 在并行全量跑时
      偶发假红（该用例文档自述为**调度性**依赖）；单跑 / 连跑 5 次均绿，baseline 亦绿 ⇒ 非本批引入。
