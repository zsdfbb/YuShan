---
title: Agent 公开 API 扩展 — tool_names / context_window / cancel
date: 2026-09-08
status: accepted
---

## Context

`apps/coding-agent` TUI 改进（`docs/arch/tui-resident-status/`）需要在 banner/footer 展示：

- 工具列表（用户想知道 agent 有什么能力）
- Context window size（未来要展示 context % 占比）
- Ctrl-C 取消当前 turn（用户中途打断）

Agent 私有字段（`registry` / `cancel` / `limits`）无法直接访问，需扩展公开 API。

## Decision

在 `agent_runtime::Agent` 新增 3 个方法：

| API | 类型 | 用途 |
|-----|------|------|
| `tool_names(&self) -> Vec<String>` | 只读 getter | banner/footer 展示工具列表 |
| `context_window(&self) -> usize` | 只读 getter | context % 计算 |
| `cancel(&mut self)` | mut action | Ctrl-C 中断当前 turn |

在 `agent_tool::ToolRegistry` 新增 1 个方法：

| API | 类型 | 用途 |
|-----|------|------|
| `names(&self) -> Vec<&str>` | 只读 getter | Agent::tool_names 借用 |

## Why

### 必须扩展的原因

- **tool_names**: banner 加工具列表（P1-4）是「展示态」需求。返回 owned `Vec<String>` 而非 `&'static str`——`ToolSpec.name`（`crates/agent-tool/src/spec.rs`）类型是 `String`，无法借用 `'static`
- **context_window**: 已有 `RunLimits::context_window` 字段 pub，Agent 是其唯一持有者，公开 getter 让 TUI 零拷贝拿到
- **cancel**: Ctrl-C 中断依赖 `CancelToken::cancel()`。不暴露 `&mut CancelToken` 让 TUI 持有 token 长期借用——单一 action 更安全

### 不暴露内部类型的原因

- TUI 不需要知道 `ToolRegistry` / `CancelToken` / `RunLimits` 的内部结构
- 返回 owned `Vec<String>` 让 API 边界清晰（Agent 内部数据所有权不外泄）
- mut 字段**零暴露**——TUI 不能绕过 Builder 改 Agent 内部

### CancelToken vs cancel() 的选择

考虑过两个方案：

**方案 A（取消）**：暴露 `pub fn cancel_token_mut(&mut self) -> &mut CancelToken`

- 优点：调用方控制力强，可读 is_cancelled
- 缺点：TUI 长期持有 token 借用，绕过 Agent 的状态机；多组件共享 token 难追踪所有权

**方案 B（采纳）**：暴露 `pub fn cancel(&mut self)`

- 优点：单一入口，意图明确（"取消当前 turn"）；TUI 不持有 token
- 缺点：不能 read is_cancelled（**不必要**——TUI 不需要轮询状态）

选择 B。

## How to apply

### 新加 API 的纪律

未来扩展 `Agent` 公开 API 必须：

1. **明确读 vs 写**：只读 getter 返回 owned 数据（`String`/`Vec<String>`）；mut action 单一意图（`cancel()`）
2. **不暴露内部类型**：`ToolRegistry` / `CancelToken` / `RunLimits` 等不直接返回引用
3. **新加 API 必须有 ADR**：本 ADR 是模板，未来加 `pause()` / `resume()` / `register_skill()` 等需另开 ADR
4. **PR review 检查清单**：
   - 是否能用现有 API 组合？
   - 返回类型是否 owned？引用类型需要论证为何安全
   - mut 方法的语义是否单一？

### 演进收敛

本 ADR 之后，**Agent 公开 API 收敛**：

- `is_configured` / `model_id` / `set_model` / `clear_session` / `session_messages` — 已存在
- `run_turn` — 已存在
- `tool_names` / `context_window` / `cancel` — 本 ADR 新增

未来加 footer 字段（git branch / theme / active skill）**不需要** 扩展 Agent 公开 API——从 AppView::from_sources 内部组装即可。

## Consequences

### 正面

- render 层（format.rs / tui.rs）零 source 耦合，只通过 AppView 读 snapshot
- TUI 不能绕过 Builder 改 Agent 内部状态（零 mut 字段暴露）
- 公开 API 有边界，未来扩展受 ADR 约束

### 负面

- Agent 公开 API 表面扩大——但都是只读 snapshot + 单一 action，破坏性影响低
- 内部类型不外泄——但需要为 TUI 构造 owned 数据（如 `tool_names` 返回 `Vec<String>` 而非 `Vec<&str>`），有少量 clone 开销

### 风险

| 风险 | 缓解 |
|------|------|
| 未来 cancel() 语义被滥用（如在 builder 阶段调） | doc comment 明确「cancel current run」 |
| tool_names 返回 String 频繁 clone | 一次性 banner 调用，不在热路径；AppView::from_sources 是低频操作 |
| context_window 返回 usize 错误（某些 provider 上下文不固定） | 当前 RunLimits::default = 128_000 是合理默认；未来 provider 可在 builder 阶段重设 |

## Related

- `docs/arch/tui-resident-status/design.md` §解耦评估
- `docs/arch/tui-resident-status/adr-tui-resident-status.md` §决策 4
- `docs/arch/tui-resident-status/review.md` §R1/R2 修正（`Vec<String>` 而非 `Vec<&'static str>`）