# 0009 — 事件信道改为有界异步投递，`EventSink::emit` 由同步改为 async

事件从「同步推送到 sink」改为「投递到有界信道，满时异步等待」：`EventSink::emit` 因此从同步 `fn` 改为 `async fn`，**修订 ADR-0004 决策点 3**。

理由：后台 agent 需要一个统一的、生产者/消费者式的信道，供 `-p` / `--json` / TUI / 回放等消费者共享，并为将来的多 agent 协作复用同一信道。而**同步 `emit` 与有界信道在技术上无法共存**——`BasicLoop::run_turn` 跑在 `#[tokio::main]` 的 async 上下文中，同步函数要等待有界信道腾出空位，只有 `blocking_send`（在 runtime 内 panic）或 `try_send`（满了丢事件）两条路，二者都不可接受。改成 async 后，满时等待即 ADR-0004 原本想要的背压语义，只是实现路径不同。

## Status

accepted（2026-09-12）

## Considered Options

- **维持同步 emit + 无界信道**：实现最简，但放弃背压，且信道无上限等于把内存暴露给 DoS。已拒绝。
- **维持同步 emit + 有界信道 + `try_send` 丢弃**：不 panic，但会丢事件——违反 ADR-0004 决策点 5「每个 run 恰好一个终局事件」的不变量。已拒绝。
- **`blocking_send`**：在 tokio runtime 内会 panic，不可用。
- **改 async emit（采纳）**：满时等待，不丢弃、不崩溃，恢复 ADR-0004 原本的背压意图。

## Consequences

- ADR-0004 决策点 3 中「同步 emit 换取热路径零装箱」的理由**不再成立**——信道本就在路径中间，这层开销跑不掉。该 ADR 其余决策（8-crate 分层、双事件口、终局事件不变式、协作取消）**不受影响**。
- `EventSink` 的所有实现（`NoopEventSink`、`CollectingSink`、新增的 channel sink）与所有调用点（`BasicLoop` 中的 `ctx.events.emit(...)`）需改为 `.await`。
- 需要新增一条错误语义：**等待中发现接收端已关闭**（消费者消失）时，agent 结束本轮并以 `Err(LoopError::Event(SendFailed))` 返回。这属于**策略预期内的终止**（非内部故障），由接线器按正常关机处理；但注意其**返回类型与「用户取消」不同**——取消路径是 `Ok(RunResult { stop_reason: Cancelled })`，本路径是 `Err`。Agent 不具备交互能力，无人接收时本就应当停止运行。
- 信道传递的是 `Envelope { source, turn, event }` 而非裸 `AgentEvent`——`source` 为多 agent 协作预留占位，使 `--json` 的输出格式现在即可定死，将来扩展多 agent 不构成破坏性变更。
- 协作取消在步骤边界检查的语义不变；取消延迟仍为「单次调用耗时」量级。
