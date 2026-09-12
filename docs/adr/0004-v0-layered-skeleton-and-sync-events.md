# 0004 — v0 骨架：保留 8-crate 分层，事件走同步推式双通道

v0 多方案评审（最小复杂度 2-crate / 可扩展 8-crate / 资源优先 8-crate）后的定稿：**结构取可扩展方案，热路径取资源方案，协议修正取最小方案**。过程详见 `docs/arch/v0-最小闭环/design.md`。

## 决策点

1. **8 crate 分层保留**（拒绝 2-crate 合并）：crate 边界 = 编译期强制的依赖单向，是本项目第一硬约束的机械化保障；「组件化」即产品本身，后拆分会触及所有外部可见路径。
2. **双事件口保留**（拒绝 ModelEventSink 并入 EventSink）：模型适配器只见窄口 `ModelEventSink`，run 级 `AgentEvent` 枚举不焊进模型 ABI——这是 v3 动态插件的最小面要求。
3. **`EventSink::emit` 定为同步 `fn(&mut self, AgentEvent) -> Result<(), EventError>`**（修订设计文档原 async 草图）：热路径零 Future 装箱、慢 sink 天然背压、`EventSink: Send` 即可；异步 sink 后续由 channel 适配器补齐，不触 trait。
4. **RuntimeContext 定于 agent-component**，agent-loop 依赖之：AgentLoop trait 签名引用 RuntimeContext，若留在 agent-runtime 则 loop→runtime 成环。
5. **终局事件不变式**：每个 run 恰好一个终局事件——正常路径 `RunFinished{stop_reason}`，错误路径先 `RunFailed` 再返回 Err；终局事件发送失败只返回 Err。
6. **ToolError 终止前补写 `is_error` ToolResult**（对 ADR-0001 后果的修订）：助手消息已入历史，不补写会产生无配对的 tool_use，续跑会话将被真实模型 API 拒收。补写仅为协议配对，不回喂、不继续本轮。
7. **v0 工具串行、零 tokio 生产依赖、schemars 推迟 v1**：并行工具与按源顺序持久化留待 v2（ToolContext 届时演进），`runtime-tokio` feature 随 v1 真实适配器引入。

## 方案对比

| 方案 | 优点 | 代价 |
|------|------|------|
| A 2-crate 最小化 | 代码与概念最少，测试最简 | 无编译期依赖边界；后拆分触全局 |
| B 8-crate 可扩展 | 编译期边界；v1–v4 槽位就位 | v0 仪式感偏重 |
| C 8-crate 资源 | 热路径零分配 | 借用事件/请求的生命周期负担 > 收益 |
| **融合（采纳）** | B 结构 + C 同步 emit + A 协议修正 | 双 sink 需文档固化（即本 ADR） |

## 后果

正面：后续四个演进阶段均为「加东西」而非「改东西」；事件路径与组装路径在 v0 就达到资源优先方案的水准。
负面：8 个 crate 对 ~2k 行代码偏重；「为何双 sink」「为何同步 emit」需要本 ADR 作为长期解释。

## 修订

- **决策点 3（`EventSink::emit` 定为同步）已被 [ADR-0009](./0009-async-event-channel.md) 修订**：为接入有界异步信道，`emit` 改为 async。点 3 中「热路径零装箱」的理由因信道引入而不再成立；本 ADR 其余决策不变。
