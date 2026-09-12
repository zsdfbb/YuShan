# ADR: TUI 交互与显示可测试性 — Prompter trait + Transcript 渲染双层测试

## 状态

Proposed

## 背景

coding-agent 的 slash 命令（`/login` `/model`）直接调用 `inquire::Select` / `inquire::Text`，进程内不可伪造。这些命令的交互行为（选 provider、输 api key、切 model）无法放进 `cargo test`。

当前 suspend/resume 机制（`dispatch_input` 中 `suspend_terminal → commands.execute → resume_terminal`）也是自由函数，suspend/resume 成对性无自动化断言。

## 决策

采用 **双层测试架构**（详见 `docs/arch/tui-interaction-test/design.md`）：

**第一层 — 决策逻辑（Prompter + TuiSurface trait）**：

1. `commands/mod.rs` 新增 `Prompter` trait（select / text / password），嵌入 `CommandContext` 字段
2. `ui/mod.rs` 新增 `TuiSurface` trait（suspend / resume），不进入 `CommandContext`
3. 生产实现：`InquirePrompter`（包装 inquire）；TuiSurface 保持自由函数内联
4. 测试实现：`FakePrompter`（预设答案队列 + 调用记录）、`MockSurface`（调用序列记录）

**第二层 — 屏幕渲染（Transcript → TestBackend）**：

5. 直接构造 `app.transcript`（`TranscriptLine::User/Assistant/Summary/Error`），通过 TestBackend 渲染并断言缓冲区内容
6. 零生产代码改动，纯测试代码

## 理由

| 因素 | 说明 |
|------|------|
| 最小侵入 | CommandContext 加字段，execute 签名不变 |
| 充分覆盖 | 4 个 inquire 调用点 + suspend/resume 成对性，100% 覆盖 |
| 延续范式 | 沿用 draw.rs 的 `#[cfg(test)]` 模块内测试 |
| 演进友好 | trait 已在，后续加 feature gate 或统一抽象只需调整注入方式 |
| 代码量 | ~350 行（~130 生产 + ~220 测试），1 人天 |

## 否决的替代方案

### B — 统一注入 InteractionContext

泛型传播到 Command trait / CommandContext，导致全仓库改动；dispatch_input 已持有 `&mut Terminal<B>`，再泛型化 surface 引入复杂生命周期。收益不抵代价。

### C — feature gate 隔离 inquire

只有 2 个命令（4 个调用点）用 inquire，feature 组合增加 CI 矩阵维度，隔离收益太小。

## 后果

- **正**：slash 命令控制逻辑可单测；suspend/resume 成对性有断言；屏幕渲染可断言；生产行为不变
- **负**：新增 ~350 行代码（~130 生产 + ~220 测试）；`CommandContext` 多一个字段（19 个构造点需改）；`dispatch_input` 多一个参数（10 个）
- **风险**：TuiSurface 的 suspend/resume 签名需适配 terminal 引用（实施时解决）；async 上下文中 `&dyn Prompter` 需确认 lifetime 兼容

## 验证方式

1. `cargo test -p yushan-coding-agent` 全通过（含新增的 6 个交互测试）
2. `cargo clippy --all-targets` 无 warning
3. 手动在真终端执行 `/login` 和 `/model`，行为与改造前完全一致
