# ADR-0008: Agent::cancel_handle — Ctrl-C / Esc 中断 turn

## 状态

提议（2026-09-10）

## 上下文

ratatui 替换（commit `c9a4177`）遗留 bug：ratatui mode 下 Ctrl-C 与 Esc **无法中断正在进行 turn**——turn 持续到 BasicLoop 自然完成 round 边界（最坏延迟：in-flight LLM HTTP 请求 timeout）。

详见 [`context.md`](../arch/tui-ctrl-c-cleanup/context.md) + 三方案对比 [`design.md`](../arch/tui-ctrl-c-cleanup/design.md)。

## 决策

在 `crates/agent-runtime/src/agent.rs` 新增 1 个 getter：

```rust
/// Clone-able handle to the internal cancel token.
/// 返回的 CancelToken 与 self.cancel 共享 Arc<AtomicBool>，
/// 调用方可在 select! 内 cancel.cancel() 而无需借用 &mut Agent。
pub fn cancel_handle(&self) -> agent_core::CancelToken {
    self.cancel.clone()
}
```

调用方（`apps/coding-agent/src/ui/mod.rs::run_turn_with_ticks`）改造：

```rust
let cancel_token = agent.cancel_handle();   // &self 借用一次
let mut turn_fut = Box::pin(agent.run_turn(input));

tokio::select! {
    biased;
    res = &mut turn_fut => res,
    _ = tick.tick() => { /* view 刷新 */ }
    _ = tokio::signal::ctrl_c() => {
        cancel_token.cancel();   // ✓ 不需 &mut Agent
        // turn_fut 不 drop — BasicLoop 在 round 边界检测 cancel
        // 后自然返回 Ok(RunResult{ stop_reason: Cancelled })
    }
}
```

Esc 处理扩展为：

```rust
// events.rs
if key.code == KeyCode::Esc {
    if app.is_turning {
        app.cancel_token.as_ref().unwrap().cancel();
        app.cancel_requested = true;
    } else {
        app.input.clear();
        app.input_cursor = 0;
    }
}
```

## Why

### borrow checker 关键问题

`run_turn(input).await` 返回的 future 在 `tokio::select!` 内**持有 `&mut Agent` 整生命周期**。在 select! 同一作用域调 `agent.cancel(&mut self)` 需要**第二个 `&mut Agent` 借用** —— rustc 报 E0499 拒绝。

### 解决方案：Clone handle

`Agent::cancel(&mut self)` 的瓶颈在 `&mut self`。`Agent::cancel_handle(&self)` 返回 `CancelToken: Clone`（内部 `Arc<AtomicBool>`），调用方可以：

1. **select! 之前**一次性 `&self` 借用 → 拿到 Clone handle → `&self` 借用立即结束
2. **select! 内**调 `cancel_token.cancel()` —— 仅借用 local `cancel_token`，**不碰 Agent**

`CancelToken: Clone` + `Arc<AtomicBool>` 让 handle 与 `self.cancel` 共享状态——写一边另一边立即可见（`Ordering::Release/Acquire` 保证）。

### 为什么不是 Notify（方案 B）

- **多 1 个文件**：`apps/coding-agent/src/ui/cancel.rs`
- **多 1 个字段**：`App.cancel_pending: bool` + signal 触发点修改
- **收益相同**：borrow checker 同样解，cancel 延迟同样（round 边界）
- **CancelToken 已是 token 抽象**——UI 层再加 Notify 是双重抽象

### 为什么不是 Cancellable trait（方案 C）

- **多 100+ 行**：trait 定义 + `Agent: Cancellable` impl + 1 个 getter + 1 个 async fn
- **收益相同**：功能等价于 1 个 getter
- **与现有 CancelToken API 重复**：trait 提供 `cancel_handle(&self) -> CancelToken`，本质上与直接加 getter 等价

### 与现有 cancel(&mut self) 共存

保留 `pub fn cancel(&mut self) { self.cancel.cancel() }`：
- 向后兼容（现有调用方可能依赖）
- 内部仍是同一 `self.cancel: CancelToken`
- **未来**若 BasicLoop 加 abort 回调需要 `&mut self`，仍可走 `cancel(&mut self)`

## 关键事实核查

`crates/agent-loop/src/basic.rs:48, 54, 79, 85` —— BasicLoop 在检测到 `ctx.cancel.is_cancelled()` 时返回 **`Ok(RunResult { stop_reason: StopReason::Cancelled, .. })`**（**不是 Err**）。

**关键修正**：3 个并行 subagent 都假设 cancel 返回 `Err`。但实际是 `Ok`。select! 的 Ok 分支自然处理——**不需要 drop turn_fut**。

## How to apply

### 调用方纪律

```rust
// ✓ 正确：在 select! 之前一次性 &self 借用
let cancel_token = agent.cancel_handle();
let mut turn_fut = Box::pin(agent.run_turn(input));

// ✗ 错误：在 select! 内同时 &self + &mut self
tokio::select! {
    res = turn_fut => res,
    _ = ... => agent.cancel(),  // ❌ E0499
}
```

### 跨 turn 重置

`CancelToken: Arc<AtomicBool>` 的 `cancel()` 单调（一旦 true 永远 true）。**如果旧 turn cancel 没被消费，会影响下一 turn**。

**当前解决**：dispatch_input 入口 `let cancel_token = agent.cancel_handle()` 拿**新 Clone**——每 turn 独立 token，旧 turn cancel 不影响新 turn。

```rust
async fn dispatch_input(...) -> Result<...> {
    // ...
    let cancel_token = agent.cancel_handle();  // ← 每 turn 新建
    let turn_result = run_turn_with_ticks(agent, turn_input, cancel_token).await;
    // ...
}
```

## 后果

### 正面

- ✅ 修复 Ctrl-C / Esc 中断 turn bug
- ✅ crates 公开 API 仅 +1 getter，与 ADR-0007「3 getter + 1 action」风格一致
- ✅ stdout mode 完全不受影响（cfg gate 隔离）
- ✅ 与 `cancel(&mut self)` 共存，向后兼容
- ✅ borrow checker 路径清晰（一行解释）
- ✅ 单元测试简单（直接构造 mock token）

### 负面

- ⚠️ 暴露 `CancelToken` 类型给 UI（违反 ADR-0007 §不暴露内部类型——但与 `cancel_handle()` 必然暴露配套）
- ⚠️ Round 边界取消（与现有 `cancel(&mut self)` 语义一致，**不**实现 hard interrupt）

### 风险

| 风险 | 缓解 |
|------|------|
| CancelToken 暴露给 UI 增加 API 表面积 | 已经在 ADR-0007 中讨论 cancel 机制；本 ADR 是补充 |
| 跨 turn cancel 残留 | dispatch_input 入口新建 token（每 turn 独立） |
| BasicLoop round 边界延迟 | 与现有 `cancel(&mut self)` 一致；可接受 |
| `cancel_handle(&self)` + `cancel(&mut self)` 双 API 冗余 | 保留向后兼容；标注「prefer cancel_handle」 |

## 演进收敛

本 ADR 后 Agent 公开 API（v0 终态）：
- 4 getter：`is_configured` / `model_id` / `session_messages` / `cancel_handle`
- 1 action：`set_model` / `cancel`

未来加 turn-level timeout 用同一 token 桥接：

```rust
let cancel_token = agent.cancel_handle();
tokio::select! {
    res = agent.run_turn_cancellable(input, cancel_token.clone()) => res,
    _ = tokio::time::sleep(Duration::from_secs(60)) => { cancel_token.cancel(); }
}
```

## 相关

- [`context.md`](../arch/tui-ctrl-c-cleanup/context.md)
- [`design.md`](../arch/tui-ctrl-c-cleanup/design.md)
- `docs/adr/0007-agent-public-api.md` — 前一轮扩展（3 getter + 1 action）
- `crates/agent-loop/src/basic.rs` — BasicLoop cancel 检查（不动）
- `crates/agent-core/src/cancel.rs` — CancelToken 定义
