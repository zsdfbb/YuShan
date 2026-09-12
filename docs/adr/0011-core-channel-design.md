# 0011 — 核心信道契约：三方案评审后取「最小骨架 + 两处性能移植」

为「核心信道 + Actor 模型」（`ys-channel` 契约 + 自转循环 + 事件信道）做三方案并行设计（最小复杂度 / 可扩展优先 / 性能优先，详见 [`docs/arch/gap-closure/design-core-channel.md`](../arch/gap-closure/design-core-channel.md)）。

**决策**：取**最小复杂度方案为骨架**，移植**性能方案的两处**——`EventSink` 的 try/await 双路径、`Inbox` 的产消模型（落法经质量分析修订为「内可变 + pending 转移进会话」）。**不采纳**可扩展方案的 `EventBus`/`Projection`/`SharedLog`/`InboundHub`/`TurnDriver`（它们服务于已被推迟的群聊/多消费者）。

理由：三个方案在核心决策上完全一致（新建 `ys-channel` 契约 crate、`Envelope{source,turn,event}`、两队列 + `QueueMode`、生命周期策略、steering 在轮边界）；分歧只在结构化程度。本项目「最小核心 + 静态组合 + 单人维护」指向最少概念，而可扩展方案的抽象在今日**无消费者**（context.md 明确「群聊本轮不设计」）。

## Status

accepted（2026-09-12）

## Considered Options

- **可扩展优先**（6 trait + 14 扩展点 + `Projection`/`SharedLog`）：否决。为被推迟的群聊提前建设，引入 `dead_code`，且今日 `-p`/`--json`/TUI 是**互斥模式**（单消费者足够），`EventBus` fan-out 无消费者。其扩展点已记录，将来按需回访。
- **纯最小复杂度**（不移植任何东西）：部分否决。其 `EventSink` 仅改 async 会让 `ModelEventSink` 被迫 async，破坏 ADR-0004 点 2 的「模型适配器最小 ABI 面」；其 `Inbox` 用两个独立 `VecDeque`，与 context.md 已定的「队列 = 日志 + 游标」相悖。
- **纯性能优先**：部分否决。`Arc<Vec<Message>>`/`Bytes`/`Copy` 是**与信道正交**的优化，混入会放大改动、稀释可评审性。记录为独立后续项。

## 关键决策点

1. **新建 `crates/ys-channel/`**：契约层，依赖 `ys-core` + `ys-event`。装 `Envelope`/`Source`/`Inbox`/`QueueMode`/`LifecyclePolicy`。**`tokio::mpsc` 实现在接线器**——理由是契约不该指定传输（焊死 mpsc = 该信道只能是 tokio 的，换传输要改契约），而非"避免拖进 tokio"（那个理由不成立：`ys-loop`/`ys-session` 自 v1 起已直接依赖 tokio，见 ADR-0012）。
2. **`EventSink` 改 try/await 双路径**，置于 `ys-event`，入参仍 `AgentEvent`（`Envelope` 由接线器的 `ChannelSink` 封装）。**这保住了 `ModelEventSink` 的同步性**——async trait 跨动态库边界困难，而 ADR-0004 点 2 为 v3 动态插件保留该最小 ABI 面。
3. **`Inbox = Arc<Mutex<Inner>>`（内可变），只装「尚未处理」的消息**。`push(&self)` 使接线器可在 agent 运行期间投递（steering 的前提）；消费 = 把 pending **转移**进 `Session`——pending + 会话历史合起来才是「队列 = 日志 + 游标」里的那条日志，故无重复持有。`/new` = 换 `Session` + `Inbox`（接线器动作，agent 不知情）。
4. **自转接口 = `Agent::run(&mut self, inbox)`**，复用现有 `Agent`；`run_turn` 降级为内部 `run_one_turn`（ADR-0010 留空的正是这块）。**不收 policy**——策略只由 sink 持有（见点 5）。
5. **`LifecyclePolicy` 用 enum + `#[non_exhaustive]`**，非 trait——两种策略已定，第三种加变体即可。**只由 `ChannelSink` 持有**（它是唯一知道"消费者是否消失"的地方），避免同一策略指定在两处而优先级不明。
6. **agent 不休眠**：`run()` 在 inbox 空时返回，接线器按需重驱动。省掉 `Notify`/常驻任务/唤醒协议。
7. **ADR-0010 的所有权收敛（session/events 移出 `Agent`）排到迁移步 5**，与信道解耦，可最后做或不做。
8. **`turn` 显式化**：`EventSink::begin_turn(n)` 由 `Agent::run` 调用；**不从 `UserMessage` 推导**——轮边界 steering 注入也发 `UserMessage`，推导会误增 turn 破坏 `--json` 分组。
9. **overflow 归 sink、不归 `Forwarder`**：`Forwarder` 生命周期是单次 `model.complete()`（`basic.rs:129-140`），缓冲随其 drop 会丢事件、违 ADR-0004 点 5。放进长生命周期的 sink，由每次 `emit().await` 自动冲掉。

> **说明**：点 3/4/5/8/9 是经 [`review.md`](../arch/gap-closure/review.md) 质量分析后修订的（原设计在 `Inbox` 产消模型、overflow 归属、`turn` 推导、policy 位置上不自洽）。修订前该设计**不具备进入实现的条件**。

## Consequences

- **信道不是瓶颈**：`Envelope` ~112 B、流式 20–200 events/s、单事件 30–80 ns、**~0.02% CPU**。故不做生产者侧批处理（那会破坏「增量拼接 == 终态」不变量）。
- **真正的热点另立**：`ModelRequest.messages` 每轮 `to_vec()` 是 O(上下文) 深拷贝（单回合 5 轮 ≈ 2000 次分配 + 2 MB memcpy），记录为独立后续项，不在本设计内。
- **`ModelEventSink` 保持同步**——模型适配器层零改动；v3 动态插件的最小 ABI 面不受损。
- **`EventSink` 调用点微调**：`ctx.events.emit(e)` → `emit(ctx.events, e)`（自由函数组合双路径）。
- **`try_emit` 撞满不再算失败**：满 → sink 内部缓冲；只有「消费者消失」才失败。快路径语义因此简化为「只报消费者离去」。
- **背压可观测**：`ChannelSink::stats() -> ChannelStats`（最小读数，不引入埋点框架）。
- **扩展点已记录、不改结构**：多消费者 `EventBus`、群聊 `Projection`/`SharedLog`、第三四种队列、第三种生命周期、agent 休眠、可替换 `TurnDriver`——见 design.md §6。
- **ADR-0010 仍为 proposed**：本设计给出其 API 形状（`Agent::run`），但所有权收敛被编排为后续步骤。
