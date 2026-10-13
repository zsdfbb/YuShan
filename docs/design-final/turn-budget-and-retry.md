# 回合级工具输出闸门与空响应重试 — As-built

> 性质：**实际建成记录**。上游设计见 `docs/arch/turn-output-and-feedback/design.md`（§4 与 ADR 决策 4
> 列为「第二批」的两项）；计划见 `docs/design-plans/2026-10-13-turn-budget-and-retry.md` 与
> `docs/exec-plans/2026-10-13-turn-budget-and-retry.md`。
> 日期：2026-10-13 ｜ 基线 `fece752`（本轮改动在其上，尚未 commit）。

## 一句话

第一批把「**单条**工具结果规模」收进边界、让「空产出」可见；第二批收两个尾：**回合级工具输出总量闸门**（一个回合内**多条**结果不再累积放大——超预算时降级**更早**的结果为「头部 + 续读指针」，最新一条保留）与**空响应重试一次**（只有思考、没有正文/工具调用时先催一句，仍空才如实判 `NoOutput`），并补一条「过程叙述」guideline。

## 落点

```text
crates/ys-component/src/limits.rs   RunLimits +max_turn_tool_result_tokens（默认 0=关闭）
                                    +retry_empty_response（默认 true）
crates/ys-loop/src/turn_budget.rs   新增：回合级闸门纯算法（估算 / 计划 / 降级），零 IO、无 async
crates/ys-loop/src/lib.rs           mod turn_budget; pub use turn_budget::*;
crates/ys-loop/src/basic.rs         run_turn：turn_start 锚；Step 11 闸门接线；空产出分支一次性重试
apps/coding-agent/src/main.rs       resolve_limits：max_turn_tool_result_tokens = context_window / 4
apps/coding-agent/src/prompt.rs     guideline「多轮工具任务每批前一句话叙述」（工作型工具在场时）
```

## 关键行为（实现期定死，勿凭直觉改）

**① 两个新字段都是承重默认。** `max_turn_tool_result_tokens` 默认 `0` = **关闭** ⇒ 既有消费者 / 测试逐字节不变（沿用 `fixed_prompt_tokens` 惯例）；`retry_empty_response` 默认 `true` = 启用重试。app 侧 `resolve_limits` 显式设 `context_window / 4`（128k → 32k）——`0` 会关闭闸门，故必须显式设非零。

**② 闸门只降级更早结果；最新一条永不降级；不改块类型与 `tool_call_id`。** 降级 = `ys_tool::head_lines`（`ELIDE_HEAD_LINES = 40` 行 / `ELIDE_HEAD_BYTES = 2 KiB`）+ 固定指针 notice（含 `[earlier tool result elided` 哨兵 ⇒ 幂等）。`plan_elisions` 按**降级后残量**判定，能压进预算就压进；压不进则只剩最新未降级。只缩 `content`、**不增删消息** ⇒ 与 `compact.rs` 的窗口触发不冲突（下界取 `turn_start.min(len)` 保守钳制：宁少降级，不误删更早历史）。

**③ 空响应重试只对合法空回复生效。** 触发条件**全部**满足：`text.trim().is_empty()` + `finish_reason == Stop` + `RunLimits::retry_empty_response` + 本回合尚未重试。动作：append 一条固定催促消息 + `continue`（消耗一个轮 ⇒ 自动受 `max_rounds` 与轮边界 abort 约束）。仍空才判 `NoOutput`。`retry_empty_response = false` ⇒ 与第一批**逐字节一致**。注意：**`NoOutput` 的判定本身仍只看 `trim()`、不看 `finish_reason`** —— 第二批只给「是否重试」加闸，不改变第一批判定。

**④ 过程叙述 guideline 仅工作型工具在场时出现。** `read`/`grep`/`find`/`ls`/`bash`/`edit`/`write` 任一 ⇒ 加一句（一句话，不是一段）；空工具表 / 纯检索类不加。它进 `fixed_prompt_tokens`（system prompt 缓存前缀）。

## 测试与验证

- **ys-component**：N17 —— 两字段默认值（`0` / `true`）+ 可显式设置。
- **ys-loop**：`turn_budget` 单测覆盖 N10 算法内核 + N15（幂等 / 不误伤 / 边界）；接线测试覆盖 N10（降最旧、保留最新、条数不变）、N11（`budget = 0` 逐字节不变）、N12（首轮空 + 次轮正常 ⇒ `Completed`、`rounds == 2`）、N13（两轮空 ⇒ `NoOutput`、`rounds == 2`，并更新既有 `empty_response_*` 用例喂两次空）、N14（`retry_empty_response = false` ⇒ 首轮即 `NoOutput`、`rounds == 1`）。
- **ys-coding-agent**：N16（`resolve_limits` 预算 == `context_window / 4`）、N18（叙述 guideline 就位 / 缺席）。
- **N19 回归**：`cargo test --workspace` 全绿且不降级；`cargo clippy --all-targets` 与 `cargo fmt --check` 对本批改动文件干净（基线本有**无关** fmt 漂移，本轮未触碰）。
- **变异测试（已实测）**：去掉 `plan_elisions` 的「最新一条永不降级」保护 ⇒ `turn_budget` 与接线两用例变红；去掉 `retry_empty_response` 闸门 ⇒ N14 变红。

## 文件清单

| 文件 | 变更 |
|---|---|
| `crates/ys-component/src/limits.rs` | 新增两个 `RunLimits` 字段 + 默认值 + N17 测试 |
| `crates/ys-loop/src/turn_budget.rs` | **新增**：纯算法模块 + N10/N15 单测 |
| `crates/ys-loop/src/lib.rs` | 挂载 `turn_budget` 模块 |
| `crates/ys-loop/src/basic.rs` | `turn_start` 锚 + Step 11 闸门接线 + 空产出重试 + N10–N14 测试 |
| `apps/coding-agent/src/main.rs` | `resolve_limits` 设回合级预算 + N16 测试 |
| `apps/coding-agent/src/prompt.rs` | 过程叙述 guideline + N18 测试 |
