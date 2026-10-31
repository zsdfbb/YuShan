# 0064 — 插话不打断工具与子 agent

用户插话（`Boundary::Steer`）与 `/compact`（`Boundary::Compact`）**不缩短任何正在执行的
工作**：工具跑到自然结束，子 agent 跑到自然结束。唯一的打断信号是 `Abort`（整回合中止）。

## Status

accepted（2026-10-31）

## 背景

改动前有三个打断点，都靠 `BoundarySource::has_pending_steer()`（非破坏性探针，命中
`Steer` 或 `Compact`）：

| 位置 | 行为 |
|---|---|
| `adapters/tools-basic/src/exec.rs` | `break Stop::Steered` ⇒ 杀**整个进程组** |
| `adapters/tools-basic/src/bash.rs` | `return Interrupt::Steer` ⇒ 杀**整个进程组** |
| `apps/coding-agent/src/subagent.rs` | `run_or_steer` 的 `select!` 分支 ⇒ **drop 掉 runner future** |

第三条最重：`runner.run(..)` 的 future 被 drop ⇒ 子 agent 直接取消、partial work 丢失。
用户看到的是这条工具结果：

> `dispatch aborted early because the user sent a message while the sub-agent was working.
> The sub-agent was cancelled and did NOT finish`

按模式分：同步 `subagent` 会因此丢弃在跑的子 agent；`subagent_async` 本来就不阻塞父回合，
不受影响。

`ChildBoundary::has_pending_steer()` 从一开始就**恒 `false`** —— `apps/coding-agent/src/subagent.rs:134-139`
的注释已写明理由：“若让它透传，父的插话会以『孩子命令莫名提前中止』的形式表现出来”。
设计上早已认定「子的工作不该因父的插话被打断」。**父侧从未对齐到同一条判断。**

## 决策

**1. 工具只响应 `is_aborted()`。**

`exec.rs` 与 `bash.rs` 的轮询只剩中止一路。删 `Stop::Steered` / `Interrupt::Steer` /
`WaitOutcome::Steered` 三个变体，以及随之无调用者的 `steered_result` / `drain_after_kill` /
`PIPE_SETTLE`。

**2. 同步 `subagent` 直接 `.await` 派发结果。**

删 `run_or_steer` / `Dispatch` / `steered_dispatch_result` / `STEER_POLL_INTERVAL`。
`call()` 就是 `runner.run(..).await`。

**3. 插话的语义不变，只是延后。**

`Steer` 仍排在边界队列里，仍由 `BasicLoop` 在**轮边界** `take` 后 append 进会话并 emit
`UserMessage`。本决策只改「它多快生效」，不改「它生效时做什么」。

**4. `has_pending_steer` 作为公共 API 保留，但当前无生产消费者。**

删掉会破坏 `BoundarySource` 的对外形状。trait 文档如实写明「当前无消费者，保留供将来
的长工具选用」，`QueueBoundarySource` 的实现（含 `Compact` 在内）不变。

## 已接受的代价

**`/compact` 也会等。**

ADR-0052 决策 3 当初把 `Compact` 算进探针，理由正是「一个跑十几分钟的工具会让 `/compact`
同样等十几分钟 —— 那正是本变体要消除的延迟」。本决策**推翻该理由**：等待 `/compact`
的代价低于杀掉正在做的工作。ADR-0052 决策 3 已标注「已被取代」。

**用户要等工具跑完才能看到自己的话被模型读到。**

这本来就是这个设计的题中之义（插话一直是轮边界生效），改动只是把「等待时间」从
「一个轮询周期」拉长到「当前工具跑完」。TUI 的 `App::note_steered` 仍会立即画用户行 +
一条「已插话：将在当前工具调用结束后生效」的系统行 —— 文案与现在一致。

## 明确否决

- ❌ **只对 `Steer` 不打断、`Compact` 仍打断**（代号「甲」）：要在 trait 上加只查 `Steer` 的
  新探针，为的是一个用户感知不强的差异；且两个信号拆开后，同一个「打断」开关有两个名字。
  用户选「乙」：行为统一、最简。
- ❌ **给打断加超时**（「插话后最多等 N 秒，超时就杀」）：引入新的时间常量与非确定性 ——
  同样的操作有时打断有时不打断，比两者中任一都难解释。
- ❌ **保留 `run_or_steer` 但改成「不 drop，只记录」**：`select!` 一旦不再 drop，它就是纯
  轮询自己不需要的状态，是死代码。
- ❌ **把子 agent 改成可暂停/可恢复**：本仓 agent 无状态（ADR-0010），没有挂起面。

## 验证

- 三条旧契约测试改写为新契约（进程**不被杀**、`stop == Eof`、插话**仍在队列里**）：
  `test_run_bounded_steer_mid_run_does_not_stop_it`、
  `test_bash_steer_mid_command_does_not_stop_it`、
  `subagent_tool_steer_mid_dispatch_does_not_abandon_it`。
- 变异验证：把 `has_pending_steer()` 塞回 `exec.rs` 主循环 ⇒ 第一条变红
  （`left: Aborted, right: Eof`，耗时 204ms 暴露插话仍在杀进程）。

## 相关

- [ADR-0013](./0013-cancel-token-removal-boundary-source.md) — `Boundary` 的出处
- [ADR-0022](./0022-subagent-dispatch.md) — 同步 `subagent` 工具与子边界；
  `ChildBoundary::has_pending_steer` 恒 `false` 的理由在代码注释（`subagent.rs:134-139`）
- [ADR-0047](./0047-input-delivery-target-and-queue.md) — 输入投递去向
- [ADR-0052](./0052-compact-at-round-boundary.md) — 决策 3 已被本决策取代
