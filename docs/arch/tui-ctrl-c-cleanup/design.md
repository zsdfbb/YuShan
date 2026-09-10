# Ctrl-C / Esc 中断 turn — 架构设计

## 概述

修复 ratatui 模式下 Ctrl-C 与 Esc **无法中断正在进行 turn** 的 bug（commit `c9a4177` ratatui 替换遗留）。3 个并行 subagent 基于 [`context.md`](./context.md) 给出方案。本文综合后推荐 **方案 A 的简化版**——`Agent::cancel_handle(&self)` getter（最干净），但**不需要新 trait、不需要 Notify 通道**。

## 三方案对比

| 维度 | **A：cancel_handle getter（推荐）** | B：tokio::Notify 通道 | C：Cancellable trait |
|------|------|------|------|
| crates/ 公开 API 改动 | **+1 getter**（`cancel_handle(&self)`） | **0**（Notify 在 UI 层） | **+1 trait + 1 fn + 1 async fn** |
| 跨借用边界 | ✅ CancelToken: Clone（Arc） | ✅ Notify: Clone（Arc） | ✅ 同 A |
| 暴露内部类型 | 是（CancelToken） | 否（只暴露 Notify） | 是（Cancellable + CancelToken） |
| 新 ADR 需要 | 是（ADR-0008 论证扩展边界） | 否 | 是 |
| 代码增量 | ~5 行（1 个 getter + 1 个 select! 改造） | ~90 行（cancel.rs + mod.rs + events.rs） | ~120 行（trait + 1 fn + 1 async fn + select! 改造） |
| borrow checker 路径 | `&self` 借用一次 → Clone → Arc<AtomicBool> 触发 | Notify Arc 完全独立 | CancelToken 显式参数化 |
| select! 内调用 cancel | ✅ `cancel.cancel()`（Arc 写） | ✅ `notifier.signal()` | ✅ `token.cancel()` |
| 跨 turn 重置 | 需要（CancelToken 单调 cancelled） | 自动（每 turn 新建 Notify） | 同 A |
| 信号延迟 | round 边界（同现有） | 同 A | 同 A |
| 与现有 `cancel(&mut self)` 共存 | 是（保留向后兼容） | 是 | 是 |

## 关键事实核查

`crates/agent-loop/src/basic.rs:48, 54, 79, 85` — BasicLoop 在 cancel 时返回 **`Ok(RunResult { stop_reason: StopReason::Cancelled, .. })`**（**不是 Err**）。

**关键修正**：3 个 subagent 都假设 cancel 返回 `Err`，但实际是 `Ok`。`select!` 的 Ok 分支自然处理——**不需要 drop turn_fut**。

## 推荐方案 — A 简化版（cancel_handle getter）

### 核心改动

```rust
// crates/agent-runtime/src/agent.rs (在 pub fn cancel(&mut self) 后面)

/// Clone-able handle to the internal cancel token.
/// Returned value shares the underlying `Arc<AtomicBool>` with `self.cancel`,
/// so `cancel_handle().cancel()` is observable by `BasicLoop`'s round-boundary check.
///
/// Why `&self` (not `&mut self`): `run_turn(input).await` borrows `&mut Agent`
/// for the entire future lifetime; a separate `&mut self` borrow in `select!`
/// branches would be a borrow-checker conflict. Returning a Clone handle lets
/// the caller trigger cancellation without re-borrowing `Agent`.
pub fn cancel_handle(&self) -> agent_core::CancelToken {
    self.cancel.clone()
}
```

### apps/coding-agent/src/ui/mod.rs 改造

```rust
async fn dispatch_input(...) -> Result<...> {
    // ... slash command 不变 ...

    app.is_turning = true;
    app.cancel_requested = false;          // NEW: 重置每轮状态
    let t0 = Instant::now();
    let turn_input = AgentInput::text(&input);

    let cancel_token = agent.cancel_handle();    // NEW: &self 借用
    let turn_result = run_turn_with_ticks(agent, turn_input, cancel_token).await;
    let elapsed = t0.elapsed().as_secs_f32();
    app.is_turning = false;
    app.cancel_requested = false;          // NEW: turn 退出后清标记

    match turn_result {
        Ok(run) => {
            // ... 推 transcript + summary ...
            // run.stop_reason = Cancelled 走同一条路径（format::print_turn_summary
            // 用 status_symbol 显示 ✗ Cancelled）
        }
        Err(e) => app.transcript.push(TranscriptLine::Error(format!("{e}"))),
    }
    Ok(())
}

async fn run_turn_with_ticks(
    agent: &mut Agent,
    input: AgentInput,
    cancel_token: CancelToken,           // NEW: 接 cancel token
) -> Result<RunResult, Box<dyn std::error::Error>> {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut turn_fut = Box::pin(agent.run_turn(input));

    loop {
        tokio::select! {
            biased;
            res = &mut turn_fut => {
                return res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>);
            }
            _ = tick.tick() => {
                app.view_dirty = true;    // 这里 app 需要 &mut App，但当前签名没有
                                          // → 需要改 run_turn_with_ticks 签名加 app: &mut App
            }
            _ = tokio::signal::ctrl_c() => {
                cancel_token.cancel();      // ✓ 不需要 &mut Agent
                // 不 break / 不 return: 让 turn future 自然完成（BasicLoop
                // 在 round 边界检测 cancel，返回 Ok(RunResult{ stop_reason: Cancelled })
            }
        }
    }
}
```

**修正 tick 分支的 app 借用**：`run_turn_with_ticks` 需要 `app: &mut App` 参数来 mark view_dirty：

```rust
async fn run_turn_with_ticks(
    agent: &mut Agent,
    input: AgentInput,
    cancel_token: CancelToken,
    app: &mut App,                       // NEW: mark view_dirty
) -> Result<...> {
    // ...
    _ = tick.tick() => app.view_dirty = true,
}
```

### apps/coding-agent/src/ui/events.rs 改造

```rust
// 2. Ctrl-C — 调 token.cancel()（不需 &mut Agent）
if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
    if let Some(token) = app.cancel_token.as_ref() {
        token.cancel();
    }
    app.cancel_requested = true;            // 意图标记（debug 观测）
    return Ok(());
}

// 4. Esc — turn 进行中触发取消；idle 清空 input
if key.code == KeyCode::Esc {
    if app.is_turning {
        if let Some(token) = app.cancel_token.as_ref() {
            token.cancel();
        }
        app.cancel_requested = true;
    } else {
        app.input.clear();
        app.input_cursor = 0;
    }
    return Ok(());
}
```

### apps/coding-agent/src/ui/app.rs 扩展

```rust
pub struct App {
    // ... 既有字段 ...
    pub cancel_token: Option<CancelToken>,    // NEW: event_loop 启动时 set
}
```

### event_loop 改造（apps/coding-agent/src/ui/mod.rs）

```rust
let cancel_token = agent.cancel_handle();    // &self 借用
app.cancel_token = Some(cancel_token.clone()); // 写进 App（events.rs 用）
```

### 关键 borrow checker 论证

```rust
let cancel_token = agent.cancel_handle();  // (1) &self 借用 — 立即结束
let mut turn_fut = Box::pin(agent.run_turn(input));  // (2) &mut Agent 借用 — 进入 future
loop {
    tokio::select! {
        res = &mut turn_fut => ...,         // borrow (2) active
        _ = cancel_token.cancelled().await => ...,  // borrow (1) finished — 借用 CancelToken owned
        _ = tick.tick() => ...,             // local Interval — 无 Agent 借用
    }
}
```

**为什么合法**：
1. `cancel_handle(&self)` 是 `&self` 借用，**不与** `run_turn(&mut self)` 的 `&mut self` 冲突（一为共享、一为独占；不重叠）
2. 返回的 `CancelToken: Clone` 是 owned，与 agent 解耦
3. `cancel_token.cancelled()` 借用 local `cancel_token`（非 agent），与 `&mut turn_fut`（借 agent）正交
4. `tick.tick()` 借用 local `Interval`，无冲突
5. **不需要 drop turn_fut**——因为 `BasicLoop` 在 round 边界检测 cancel 后**自然返回 `Ok(RunResult)`**（不是 Err）

**对比方案 C（trait 抽象）**：完全等价的 borrow 模式，但多 100+ 行 trait/impl/fn 代码。**over-engineering**。

**对比方案 B（Notify 通道）**：同等 borrow 解法，但增加一个 UI 层文件（cancel.rs）+ signal 调用点修改。**比 A 复杂**。

## 关键文件清单

| 文件 | 改动 | 行数 |
|------|------|------|
| `crates/agent-runtime/src/agent.rs` | + `pub fn cancel_handle(&self) -> CancelToken` | +5 |
| `apps/coding-agent/src/ui/app.rs` | + `cancel_token: Option<CancelToken>` | +2 |
| `apps/coding-agent/src/ui/mod.rs` | event_loop set cancel_token；`run_turn_with_ticks` 加参数 + select! 三路；`dispatch_input` 调 `cancel_handle()` | +20 |
| `apps/coding-agent/src/ui/events.rs` | Ctrl-C + Esc 分流（turn 进行中 token.cancel()；idle 清空 input） | +10 |
| `docs/adr/0008-agent-cancel-handle.md` | NEW — 标注扩展边界 | ~40 |
| **总计** | **+1 crate getter + 3 ui 改 + 1 adr** | **~80 行** |

## 复用与约束

- ✅ `CancelToken: Clone` + `Arc<AtomicBool>` 内部共享 — 已存在
- ✅ `BasicLoop` round-boundary cancel 检查（basic.rs:48, 54, 79, 85）— 已存在
- ✅ `tokio::signal::ctrl_c()` — tokio 已有
- ✅ `cargo public-api` 不破坏（+1 getter，零破坏性变更）
- ✅ stdout mode 不受影响（cfg gate 隔离，tui-stdout 模式无 ui 模块）
- ✅ 与现有 `cancel(&mut self)` 共存（向后兼容）
- ✅ 与 ADR-0007 风格一致（再 +1 getter，共 4 getter + 1 action）

## 测试设计

### 单元测试（agent-runtime）

```rust
#[test]
fn test_cancel_handle_is_clone_and_signals() {
    let agent = AgentBuilder::new()
        .model(MockModel::new("test"))
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let handle = agent.cancel_handle();
    let handle2 = handle.clone();
    assert!(!handle.is_cancelled());

    handle.cancel();
    assert!(handle2.is_cancelled());      // shared Arc — 立即可见
}

#[tokio::test]
async fn test_cancel_handle_triggers_cancellation() {
    let agent = AgentBuilder::new()
        .model(MockModel::new("test"))
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let mut agent = agent;
    let handle = agent.cancel_handle();

    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle.cancel();
    });

    let result = agent.run_turn(AgentInput::text("go")).await.unwrap();
    cancel_task.await.unwrap();
    assert_eq!(result.stop_reason, StopReason::Cancelled);
}
```

### 单元测试（ui 模块）

```rust
#[tokio::test]
async fn esc_cancels_running_turn() {
    let mut app = make_test_app();
    let token = CancelToken::new();
    app.cancel_token = Some(token.clone());
    app.is_turning = true;
    
    handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app).unwrap();
    
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn esc_idle_clears_input() {
    let mut app = make_test_app();
    let token = CancelToken::new();
    app.cancel_token = Some(token.clone());
    app.is_turning = false;
    app.input = "/mo".into();
    
    handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app).unwrap();
    
    assert!(!token.is_cancelled());
    assert_eq!(app.input, "");
}

#[tokio::test]
async fn ctrl_c_always_signals() {
    let mut app = make_test_app();
    let token = CancelToken::new();
    app.cancel_token = Some(token.clone());
    
    handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut app).unwrap();
    
    assert!(token.is_cancelled());
}
```

### 手动验证

- [ ] ratatui mode 长 turn → Ctrl-C → 数秒内 `✗ Cancelled · N rounds`
- [ ] ratatui mode 长 turn → Esc → 行为与 Ctrl-C 等效
- [ ] ratatui mode idle 时 Esc 清空 input（保留旧行为）
- [ ] ratatui mode idle 时 Ctrl-C 不应崩（虽然 cancel 触发但 turn 已完成）

## 演进路径

### v0（本次）

- `Agent::cancel_handle(&self) -> CancelToken`
- run_turn_with_ticks select! 三路
- Esc/Ctrl-C 调 `token.cancel()`

### v1（短期，可选）

- 多 turn 持久化 token：每次 dispatch_input 新建 `cancel_handle()` clone（避免上一 turn cancel 影响本 turn）
- **当前实现已满足**：dispatch_input 入口 `let cancel_token = agent.cancel_handle()` 拿新 clone

### v2（中期，若 BasicLoop 改为长 round-cooperative 取消）

- BasicLoop 内部在 tool call 后增加 cancel 检查点
- BasicLoop 加 abort 请求字段（`RuntimeContext::request_abort`）
- **当前需求不要求** — round 边界取消已满足体感

### 不应做的事

- **不要**用 `Notify` 桥接（subagent 方案 B）—— 增加 UI 层文件 + 信号触发点修改，无收益
- **不要**引入 `Cancellable` trait（subagent 方案 C）—— 100+ 行抽象等价于 1 个 getter，over-engineering
- **不要**改 `Agent::cancel(&mut self)` 签名 —— 现有 stdout 模式（rustyline REPL）可能依赖
- **不要**在 `run_turn` 接受 `Notify` 参数 —— 污染 AgentLoop trait

## 关联文档

- [`context.md`](./context.md) — 现状分析 + 三个任务
- `docs/arch/ratatui-replace/adr-ratatui-replace.md` — ratatui 替换决策
- `docs/adr/0007-agent-public-api.md` — 已有 3 getter + 1 action，本次 +1 getter
- `crates/agent-loop/src/basic.rs` — BasicLoop cancel 检查路径（不动）
