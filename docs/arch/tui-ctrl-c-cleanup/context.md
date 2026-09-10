# ratatui 后续三件套 — 架构上下文

## 概述

ratatui 替换（c9a4177）后，三件事需要跟进：

1. **a**：真实验证 ratatui 模式在真实终端（不是 TestBackend）下的体验
2. **b**：修复 Ctrl-C + Esc 中途中断 turn 的 bug
3. **c**：彻底删除 `tui.rs` / `tui_completer.rs`（保留 `format::print_*`，但移除 rustline REPL 路径）

## 现有架构

### ratatui mode 当前状态

**main.rs:13** — `#[cfg(feature = "tui-ratatui")] mod ui;`
**main.rs:15** — `#[cfg(feature = "tui-stdout")] mod tui; mod tui_completer;`

**ui/mod.rs:243-257** — `run_turn_with_ticks` 当前实现：

```rust
/// R3 阶段 — agent.run_turn 直通。
///
/// 100ms tick 由外层 event_loop 处理（`is_turning` 时刷新 view）；
/// Ctrl-C 由 events.rs 捕获并设置 `app.cancel_requested`，agent 的
/// BasicLoop 在 round 间检查 cancel token，当前迭代自然完成后返回
/// Cancelled。R3 阶段不实现 turn 中途立即中断。
async fn run_turn_with_ticks(
    agent: &mut Agent,
    input: agent_loop::AgentInput,
) -> Result<agent_loop::RunResult, Box<dyn std::error::Error>> {
    agent
        .run_turn(input)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
```

**问题**：`run_turn_with_ticks` 完全**直通 agent.run_turn**，没有任何 tokio::select! 听 Ctrl-C / tick。注释承诺"Ctrl-C 由 events.rs 捕获"——但 events.rs 只设 `app.cancel_requested = true`，dispatch_input 完成后才读这个 flag。**dispatch_input 在 turn 完成前不会返回**——所以 Esc / Ctrl-C 无法打断正在进行的 turn。

**events.rs:42** — Esc 当前是"清空 input"（设计决策：未配置的 Esc 行为），不是"中断 turn"：

```rust
// 4. Esc 清空 input
if key.code == KeyCode::Esc {
    app.input.clear();
    app.input_cursor = 0;
    return Ok(());
}
```

**Agent::cancel**（crates/agent-runtime/src/agent.rs）已有公开方法：

```rust
pub fn cancel(&mut self) {
    self.cancel.cancel();
}
```

调用 `CancelToken::cancel()`（`crates/agent-core/src/cancel.rs`）—— `CancelToken: Clone` + `Arc<AtomicBool>`，可被多处持有。

### tui.rs / tui_completer.rs 当前状态

仍然在 `apps/coding-agent/src/` 顶层。feature-gate 由 main.rs 决定：

- `tui-stdout` 模式：编译，main.rs 调用 `tui::run_interactive`
- `tui-ratatui` 模式：**完全不编译**（mod gate 不引入）

**问题**：即使 ratatui 模式完全不调用 `tui.rs`，文件仍在源码树中——`cargo build --no-default-features --features tui-stdout` 仍会编译它。

**为什么保留**：
1. **stdout mode fallback** —— `--no-default-features --features tui-stdout` 是独立 binary，依赖 tui.rs 的 rustyline REPL
2. **应急路径** —— 如果 ratatui 模式崩溃，可临时用 stdout mode 调试

**为什么可删除**：
1. **Cargo.lock 已锁 ratatui 0.29 + crossterm 0.28** —— 依赖图稳定
2. **stdout mode 当前无明确用户场景** —— debug 时 `cargo run` 默认 ratatui 也行
3. **代码量增加维护负担** —— 200 行 tui.rs + 211 行 tui_completer.rs 是 ratatui 替换的迁移目标

## 约束

- **技术**：ratatui 0.29 + crossterm 0.28 event loop 已实现 `tokio::select!` 三路（event / tick / signal）—— 只需把 Ctrl-C 分支加入 `run_turn_with_ticks`
- **API**：`Agent::cancel(&mut self)` 已存在但有 borrow checker 限制——`run_turn` future 持有 `&mut Agent` 整个生命周期，无法在 select! 内部同时调 `agent.cancel(&mut self)`
- **现状**：`Agent` 内部 `cancel: CancelToken` 字段私有，无法 clone 出来在 select! 外触发

## 需求范围

### 范围内

1. **a — 真实终端手动验证**：
   - 启动 ratatui mode → 看到三栏布局
   - 输入 `/help` → 看到 HelpCommand 输出
   - 输入普通文本 → 触发 agent turn → 看到 transcript
   - Tab 补全
   - Ctrl-C / Ctrl-D 行为
   - resize

2. **b — Ctrl-C + Esc 中途中断 turn**：
   - 修改 `run_turn_with_ticks` 加 `tokio::select!` 三路：
     - turn future 完成
     - Ctrl-C signal → 调 `agent.cancel()`
     - 100ms tick（更新 status panel tokens）
   - Esc 行为扩展：turn 进行中按 Esc **也**触发 cancel（与 Ctrl-C 等效）
   - 现有 Esc 清空 input 行为**保留**（turn 不进行时）

3. **c — 删除 tui.rs / tui_completer.rs**：
   - 删除 `apps/coding-agent/src/tui.rs`
   - 删除 `apps/coding-agent/src/tui_completer.rs`
   - main.rs:15 移除 `mod tui; mod tui_completer;`
   - Cargo.toml `rustyline` 依赖改为 `optional = true` + `tui-stdout = ["dep:rustyline"]`
   - stdout mode 必须保留（feature flag 设计要求）
   - **注意**：ratatui mode `ui/events.rs` 用 `crate::tui_completer::CmdEntry`（R2 借用）—— 删除前需确保 ratatui mode 不依赖

### 范围外

- 不实现 turn 进行中的实时 token 刷新（status panel 显示 100ms tick 时的 token 数）
- 不实现 streaming output（agent token 流）
- 不实现 mouse 交互
- 不重构 `BasicLoop`（crate 层）

## 关键场景

**场景 1 — Ctrl-C 中断 turn**：
```
> 编译并修复错误
⏳ Working...
  → bash: cargo build
  ← result (failed: exit 1)
[用户按 Ctrl-C]
✗ Cancelled · 1 rounds · ↑1.2k ↓340 tokens
┌─ ... ─┐
> _
```

**场景 2 — Esc 中断 turn**：
```
> 运行 cargo test
⏳ Working...
  → bash: cargo test --no-run
[用户按 Esc]
✗ Cancelled · 1 rounds · ↑340 ↓120 tokens
┌─ ... ─┐
> _
```

**场景 3 — Esc 在 idle 时清空 input**（已有行为保留）：
```
> /mo█
[用户按 Esc]
> █
```

**场景 4 — 删除 tui.rs 后**：
```bash
cargo build --release                              # ratatui mode，OK
cargo build --release --features tui-stdout       # ❌ 编译失败：rustyline 找不到
```

## 关键文件清单

| 文件 | 改动 | 范围 |
|------|------|------|
| `apps/coding-agent/src/ui/mod.rs` | `run_turn_with_ticks` 加 `tokio::select!` 三路 | b |
| `apps/coding-agent/src/ui/events.rs` | Esc 处理分支：turn 进行中也触发 cancel | b |
| `apps/coding-agent/src/ui/mod.rs` | `dispatch_input` 等待 `cancel_requested` 标志 | b |
| `apps/coding-agent/src/ui/events.rs` | `CmdEntry` 复制到 `ui::` 内部（避免依赖 `tui_completer`） | c |
| `apps/coding-agent/src/tui.rs` | **DELETE** | c |
| `apps/coding-agent/src/tui_completer.rs` | **DELETE** | c |
| `apps/coding-agent/src/main.rs` | 移除 `mod tui; mod tui_completer;` + 简化 dispatch（无 --stdout 路径） | c |
| `apps/coding-agent/Cargo.toml` | `rustyline = "14"` 改 optional + `tui-stdout = ["dep:rustyline"]` | c |

## 复用与约束

- `Agent::cancel` 已存在，**但** `run_turn` future 持有 `&mut Agent` 生命周期，select! 内部无法同时调 `cancel(&mut self)`
- **解决**：在 select 之前 `let cancel_token = agent.cancel_token_clone()` —— 但 `CancelToken` 未公开——需要：
  - 选项 A：扩展 `Agent` 公开 `pub fn cancel_handle(&self) -> CancelToken`（Clone 的 `CancelToken`）
  - 选项 B：保持 `cancel(&mut self)`，在 select! 内部用 `&mut Agent::cancel` —— borrow checker 不允许
  - 选项 C：用 `tokio::sync::Notify` 中间桥接（独立通道）

**推荐 A**——与 ADR-0007 的"3 getter + 1 action"扩展一致，再加 1 个 getter `cancel_handle()`。代价小、可测、与现有 cancel() 并存。

## 验证清单

- [ ] `cargo build -p yushan-coding-agent`（ratatui mode）通过
- [ ] `cargo test -p yushan-coding-agent` 全绿
- [ ] 手动：ratatui mode 启动 → 输入 `/help` → 看到 HelpCommand 输出
- [ ] 手动：ratatui mode 输入长 turn → Ctrl-C 中断 → 看到 `✗ Cancelled`
- [ ] 手动：ratatui mode 输入长 turn → Esc 中断 → 看到 `✗ Cancelled`
- [ ] 手动：ratatui mode 输入 `/mo` → Tab → `/model `
- [ ] 手动：ratatui mode Ctrl-D → 退出
- [ ] 删除 tui.rs 后 `cargo build --features tui-stdout` **应该编译失败**（验证 tui-stdout 真删除）

## 关联文档

- `docs/arch/ratatui-replace/context.md` — 整体 ratatui 设计
- `docs/arch/ratatui-replace/adr-ratatui-replace.md` — decision records
- `docs/adr/0007-agent-public-api.md` — 已有 3 getter + 1 action，本次扩展加 1 个 getter
- commit `c9a4177` — ratatui 替换（baseline）
