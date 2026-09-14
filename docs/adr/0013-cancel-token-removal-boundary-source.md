# 0013 — 取消即消息：移除 `CancelToken`，`BoundarySource` 取代 `Inbox`

把「取消」从一个**跨线程原子**（`CancelToken`）改成**队列里的一条有序消息**（`Boundary::Abort`），
并把「轮边界的控制输入」从 `ys-channel::Inbox`（two-vec + `Intent` + `QueueMode`）收敛成一个
零 tokio 的 `BoundarySource` trait。二者是同一件事的两面：**取消与插话共享同一条通道**。

本 ADR 记录**决策与后果**；新拓扑的完整形态见 `docs/design-final/coding-agent-tui.md`，
推导过程见 `docs/arch/coding-agent-tui/design.md`（路线 B）+ `review.md`（R1/R5）。

## Status

accepted（2026-09-14）

**修订** ADR-0008（`cancel_handle`）与 ADR-0011（`Inbox` 产消模型）中与取消/入站队列相关的部分。
本 ADR 与 ADR-0010（Agent 无状态）方向一致，是其后续（见 0010 的附注）。

## 背景

### 路线 B 定下了三信道拓扑

coding agent TUI 重构（「拆 crate + 分线程 + 信道 + `ys-protocol`」）要求：

```
① Request   ys_protocol::Request   UI → app        回合边界 recv().await
② Boundary  ys_protocol::Boundary  UI → BasicLoop  轮边界拉取（Steer + Abort）
③ Outbound  ys_protocol::Outbound<V> app → UI      UI 唯一输入流
```

**② 必须独立于 ①**：回合跑动时 app 线程阻塞在 `agent.run_turn()` 里，不可能 `recv` ①；
而 `Steer` / `Abort` 要**中途**被看到 → 只能由 `BasicLoop` 在轮边界拉。

### 旧件在新拓扑下全部多余或错位

| 旧件 | 问题 |
|---|---|
| `CancelToken`（`ys-core`，跨线程原子 + `Agent::cancel()`/`cancel_handle()`） | 取消与插话是**两条互不知情的通道**：谁先到不确定，`Agent` 还因此必须被外部持有（与 ADR-0010「Agent 无状态」冲突） |
| `Inbox`（`ys-channel`，`Arc<Mutex<Inner{steering, follow_up}>>`） | `Message` 级路由（新输入 vs 插话）已由 `Request::Prompt` / `Boundary::Steer` 承担；two-vec + `Intent` + `QueueMode` 是**同一语义的第二套实现** |
| `RuntimeContext.inbox: Option<&Inbox>` | 轮边界只需「取一条 / 看一眼」，不需要「队列 = 会话 = 历史」那整套 |

> `BaseLoop` 的 `[Context Summary]` 压缩机制**独立于 inbox，保留**。

## 决策

**1. 新增 `crates/ys-protocol`（零 tokio）承载能力平面**：`Request` / `Boundary` / `Outbound<V>` /
`BoundarySource` / `QueueBoundarySource` / `Envelope` / `Source` / `LifecyclePolicy`。
`Envelope` / `Source` / `LifecyclePolicy` 自 `ys-channel` 迁入（纯数据，类型零改动）。

**2. `CancelToken` 删除，`Boundary::Abort` 取代之**：

```rust
pub enum Boundary { Steer(Message), Abort }
```

取消不再是跨线程原子，而是队列里的一条消息 —— 因此「取消」与「插话」共享同一条**有序**通道，
谁先到由入队顺序决定（旧拓扑下不确定）。`Agent::cancel()` / `Agent::cancel_handle()` 随 `CancelToken` 一并删除。

**3. `BoundarySource` 取代 `Inbox`（`ys-component` 的 `RuntimeContext` / `ToolContext`）**：

```rust
// ys-protocol，零 tokio
pub trait BoundarySource: Send + Sync {
    fn take(&self) -> Option<Boundary>;   // 轮边界：非阻塞、保序
    fn is_aborted(&self) -> bool;          // 非破坏性探针（模型调用前 / 工具轮询）
}
```

两方法都是 `&self`（内可变）—— 消费者（`BasicLoop`）经 `&dyn` 访问，生产者（app 侧）可并发 `push`。
`Abort` 入队即置位 `is_aborted`（探针立即可见），但**照常入队**（保序，`take` 仍会交还）。
`QueueBoundarySource` 是唯一具体实现（`std::sync::Mutex`，非 tokio 锁）。

**4. 工具级取消载体随之改写**：`ToolContext.cancel: &CancelToken` → `ToolContext.boundary: Option<&dyn BoundarySource>`。
`BashTool` 在挂载 `boundary` 时以 100ms 间隔轮询 `is_aborted()`，命中则杀进程组
（`sh -c "kill -9 -{pid}"`，**不引 libc**）并返回中止错误；未挂载时行为与改动前完全一致。

**5. `Envelope.turn` / `Envelope.source` 保留**：前者让消费者不必自己数轮次，后者为多 agent 协作预留。

## Considered Options

- **保留 `Inbox` / `CancelToken`，加一层薄适配**（把两者包成 `BoundarySource` 的实现）。
  **否决** —— 适配器会把 `Inbox` 的 two-vec + `QueueMode` 语义整个带进新拓扑：同一语义两套实现并存
  （`Request::Prompt` / `Boundary::Steer` 一套，`Intent::FollowUp` / `Intent::Steering` 又一套），
  而 `Inbox` 的「队列 = 会话 = 历史 + pending 转移」模型在 app 侧循环里**没有任何消费者**。
  删掉它是**简化而非损失**（review R1）。
- **保留 `CancelToken` 做「取消」、`Boundary` 只管插话**。**否决** —— 两条通道的到达顺序不确定，
  「先取消再插话」与「先插话再取消」会跑出不同结果；而且 `CancelToken` 必须被外部持有，
  与 ADR-0010 的「外部拿不到 `&mut Agent`」冲突。
- **`Boundary` 直接叫 `Control` / `Signal`**。**否决** —— 「边界」（turn/round boundary）是本项目
  已定的术语（`docs/CONTEXT.md`），`Boundary` 与「在轮边界拉取」这一消费时机同名，指代最紧。

## Consequences

- **波及面广（~110 处调用点）**：`CancelToken` 的移除牵动 `ys-core` / `ys-component` / `ys-loop` /
  `ys-runtime` / `adapters/tools-basic`；`Inbox` 的移除动 `ys-component` / `ys-loop` / `ys-runtime` /
  `apps/coding-agent`。这不是「加一个类型」，是**一次拓扑替换**。
- **`ys-runtime` 的 actor 测试重写**：`tests/actor_run.rs`（7 条，测 `Agent::run(inbox)` 自转）删除；
  多回合 / followUp 语义上移到 `apps/coding-agent/src/app_loop.rs`，等价覆盖落在该模块的测试
  与 `crates/ys-runtime/tests/run_turn_boundary.rs`（`run_turn` + `BoundarySource`）。
- **`Agent` 公开面收窄**：只剩 `run_turn` + 只读查询（`tool_names` / `context_window`）。
  `run` / `RunSummary` / `cancel` / `cancel_handle` 全删 —— `Agent` 更纯粹地是「无状态执行器」（ADR-0010 结论不变）。
- **`-p` / `--json` 的 turn 语义改为显式**：不再有 `Agent::run` 的逐回合 `begin_turn(n)`，
  一次性模式显式 `begin_turn(1)` + 一次 `run_turn`，恒为 turn 1。
- **每回合必须新建 `BoundarySource`**：`Abort` 的标记**永久**置位，复用同一个源会让「上一回合的取消」
  把之后每个回合立刻取消。app 循环还须在空闲期丢弃残留边界消息（UI 的 `is_turning` 复位有滞后）。
  这两点都有变异测试钉住。
- **测试隔离不变**：`ys-protocol` 零 tokio（生产与 dev 依赖皆无），契约层测试全为纯同步 `#[test]`。
- **ADR-0008 被修订**：`cancel_handle`（非 `&mut` 上下文触发取消）的**动机**仍成立，但**载体**换了 ——
  从「`Arc<AtomicBool>` 句柄」变成「往 ② 信道投一条 `Boundary::Abort`」。
