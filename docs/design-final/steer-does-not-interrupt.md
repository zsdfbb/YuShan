# 插话不打断工具与子 agent — As-built

## 一句话

用户插话（`Boundary::Steer`）与 `/compact`（`Boundary::Compact`）**不缩短任何正在执行的工作**：
工具跑到自然结束、子 agent 跑到自然结束。唯一能打断的是 `Abort`（整回合中止）。

决策依据见 [ADR-0064](../adr/0064-steer-does-not-interrupt.md)。

## 落点

| 文件 | 现状 |
|---|---|
| `adapters/tools-basic/src/exec.rs` | 主循环只探 `is_aborted()`。`Stop` 只剩 `Eof` / `Cap` / `Aborted` / `TimedOut` |
| `adapters/tools-basic/src/bash.rs` | `wait_interrupt` 只探 `is_aborted()`。`Interrupt` 只剩 `Abort` 一个变体；`WaitOutcome` 只剩 `Aborted` / `TimedOut` / `Failed` |
| `apps/coding-agent/src/subagent.rs` | `SubagentTool::call` 直接 `runner.run(..).await`，无 `select!` |
| `adapters/tools-basic/src/{grep,find}.rs` | `Bounded.stop` 的匹配无 `Steered` 臂 |
| `crates/ys-protocol/src/boundary.rs` | `has_pending_steer` 保留，文档标注「当前无生产消费者」 |

## 承重的不变量

### 1. `Steer` 的生效点仍是轮边界

工具和子 agent 都不消费插话 —— 它们只是不再**因它而中止**。`Steer` 仍留在队列里，
由 `BasicLoop` 的 `take()` 在下一个轮边界 append 进会话并 emit `AgentEvent::UserMessage`
（`crates/ys-loop/src/basic.rs` 的 Step 4b）。

⇒ 「工具不偷吃用户的插话」这条旧不变量**原样保留**，三个改写后的测试都断言
`boundary.take() == Some(steer)`。

### 2. 只有 `Abort` 能缩短工作

`is_aborted()` 的语义没变：整回合中止 ⇒ 工具杀进程组、子 agent 在轮边界收场。
`ChildBoundary::is_aborted()` 仍是「自己 **或** 父」，所以父回合中止照样向下传播。

### 3. 探针不消费队列

`has_pending_steer()` 与 `take()` 互不影响，正文也不进日志（`boundary.rs` 的既有约定）。
现在它没有生产消费者，但这条性质仍由 6 条单测钉住。

## TUI 侧

`App::note_steered` 的行为不变：提交后立即画用户行 + 紧跟一条系统行
「已插话：将在当前工具调用结束后生效」。这条文案与实际行为一致 —— 它承诺的是
「当前工具调用结束后」，不是「立即」。

## 已知边界

- **`/compact` 会等长工具跑完。** ADR-0052 决策 3 原本把这种等待视为要消除的延迟，
  现被接受为代价（ADR-0064 已标注取代）。
- **插话的可见延迟从「一个轮询周期」变成「当前工具跑完」。** 若模型正在跑一个 10 分钟的
  `bash`，用户敲的话要 10 分钟后才进上下文。这是设计选择，不是缺陷。
- **`has_pending_steer` 是死代码（无生产消费者）。** 保留它是为了不破坏 `BoundarySource`
  的对外形状；若将来确认无用，可以删（属破坏性变更）。
- `subagent_async` 本来就通过 `tokio::spawn` 后台跑，不受插话影响 —— 本次未改动它。

## 测试

三条契约测试（都由旧测试改写，语义反转）：

| 测试 | 钉住什么 |
|---|---|
| `exec::tests::test_run_bounded_steer_mid_run_does_not_stop_it` | 插话后进程跑到自然结束（`stop == Eof`、`code == Some(0)`） |
| `bash::tests::test_bash_steer_mid_command_does_not_stop_it` | 同上，且孙进程自然退出、插话留在队列里 |
| `subagent::tests::subagent_tool_steer_mid_dispatch_does_not_abandon_it` | 派发跑满自身时长、结果是子 agent 的正常结果 |

变异验证：把 `has_pending_steer()` 塞回 `exec.rs` 主循环 ⇒ 第一条变红。

## 与设计的偏差

无。降级路径（`-p` / `--json` 无 steering 生产者）本来就不受影响 —— 那里 `boundary`
恒为 `None`，`wait_interrupt` 与旧的 `wait_steer` 都退化成 `pending()`。
