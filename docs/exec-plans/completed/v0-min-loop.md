# v0 最小闭环 — 执行计划

> 本文档定义 YuShan v0 最小闭环的实现任务列表、详细说明与执行顺序。设计依据：`docs/arch/v0-最小闭环/design.md`、4 份 ADR、`docs/design.md`。

## 技术约束

- Rust stable 1.88，edition 2024
- 虚拟 workspace，crate 名 `agent-*`，版本 0.0.1
- 依赖策略：serde + serde_json（仅 agent-core）、async-trait、thiserror；tokio 仅 dev-dependency
- v0 无 tokio 生产依赖
- 依赖方向严格单向：agent-core <- 组件接口 <- loop/runtime <- 应用层

## 任务概览表

| # | 任务 | 描述 | 涉及 crate | 测试 |
|---|------|------|-----------|------|
| T1 | Workspace 脚手架 | Cargo.toml workspace + 8 crate 骨架（lib.rs）+ 依赖声明 | 根 + 全部 | 编译通过 |
| T2 | agent-core 类型 | Message, ContentBlock, ToolCall, ToolResult, Usage, StopReason, CancelToken, ID types, AgentError, EventError — 全部 serde | agent-core | TC1 |
| T3 | agent-event 事件 | AgentEvent (6 variants, non_exhaustive), EventSink trait, NoopEventSink, CollectingSink | agent-event | TC12 |
| T4 | agent-model 模型口 | Model trait, ModelEventSink, ModelEvent, ModelRequest/Response, ModelError, MockModel | agent-model | TC4 partial |
| T5 | agent-tool 工具口 | Tool trait, ToolSpec, ToolContext, ToolError, ToolRegistry | agent-tool | TC10 |
| T6 | agent-session 会话口 | Session trait, MemorySession, SessionError | agent-session | TC11 |
| T7 | agent-component 容器 | RuntimeContext (with new() constructor), RunLimits | agent-component | 编译通过 |
| T8 | agent-loop 基础循环 | AgentLoop trait, BasicLoop (完整 8 步数据流), AgentInput, RunResult, LoopError | agent-loop | TC2-TC9, TC13 |
| T9 | agent-runtime 组装 | Agent, AgentBuilder, BuildError, prelude | agent-runtime | TC14 |
| T10 | 集成测试 | 端到端测试覆盖 5 个关键场景 | tests/ | TC2-TC14 全量 |

## 依赖图

```text
T1 (脚手架)
 ├─> T2 (agent-core) ──────────────────────┐
 │   ├─> T3 (agent-event)                   │
 │   ├─> T4 (agent-model)                   │ 全部依赖 T2
 │   ├─> T5 (agent-tool)                    │
 │   ├─> T6 (agent-session)                 │
 │   └─> T7 (agent-component) ──────────────┘
 │       │
 │       └─> T8 (agent-loop) ──> T9 (agent-runtime) ──> T10 (集成测试)
```

## 实现顺序说明

**为什么是 T1->T2->...->T10：**

1. **T1 脚手架先行**：workspace 和 crate 骨架是所有工作的地基，必须第一个完成。
2. **T2 agent-core 优先**：所有组件接口 crate（event/model/tool/session）和容器 crate（component）都依赖 agent-core 的词汇类型。AgentError、EventError 等错误类型定义在 agent-core，是后续 crate 编译的前提。
3. **T3-T7 并行就绪**：5 个组件 crate 之间互不依赖（均只依赖 agent-core），理论上可并行实现。按字母/职责排序列出仅为可读性。实际执行时可任意顺序，只要 T2 已完成。
4. **T8 agent-loop 必须在 T3-T7 之后**：BasicLoop 引用 RuntimeContext（T7）、Model（T4）、ToolRegistry（T5）、Session（T6）、EventSink（T3）、CancelToken（T2），是依赖最重的 crate。
5. **T9 agent-runtime 在 T8 之后**：Agent/AgentBuilder 持有 Box<dyn AgentLoop>，组装全部组件，必须在 loop 之后。
6. **T10 集成测试最后**：端到端测试验证 T2-T9 全部协作正确，是最重的验收关卡。

## 详细任务说明

---

### T1 -- Workspace 脚手架

- **前置**: 无
- **目标文件**:
  - `Cargo.toml`（workspace 根）
  - `crates/agent-core/Cargo.toml`
  - `crates/agent-core/src/lib.rs`
  - `crates/agent-event/Cargo.toml`
  - `crates/agent-event/src/lib.rs`
  - `crates/agent-model/Cargo.toml`
  - `crates/agent-model/src/lib.rs`
  - `crates/agent-tool/Cargo.toml`
  - `crates/agent-tool/src/lib.rs`
  - `crates/agent-session/Cargo.toml`
  - `crates/agent-session/src/lib.rs`
  - `crates/agent-component/Cargo.toml`
  - `crates/agent-component/src/lib.rs`
  - `crates/agent-loop/Cargo.toml`
  - `crates/agent-loop/src/lib.rs`
  - `crates/agent-runtime/Cargo.toml`
  - `crates/agent-runtime/src/lib.rs`
- **实现要点**:
  - 根 `Cargo.toml` 声明 `[workspace]` + `members`（8 crate），设置 `resolver = "2"`、`edition = "2024"`
  - 每个 crate 的 `Cargo.toml` 声明 `version = "0.0.1"`、`edition = "2024"`
  - 依赖声明严格遵循层级：agent-core 无内部依赖；agent-event/model/tool/session 均依赖 agent-core；agent-component 依赖 agent-model + agent-tool + agent-session + agent-event + agent-core；agent-loop 依赖 agent-component + agent-core；agent-runtime 依赖 agent-loop + agent-component + agent-core
  - 每个 `lib.rs` 为空文件（或仅 `//! crate doc comment`），确保 `cargo build --workspace` 通过
  - v0 依赖精简：serde + serde_json 仅在 agent-core；async-trait 在 agent-model/tool/session/loop；thiserror 在 agent-core + 各错误 crate；tokio 仅在 dev-dependencies
- **验收标准**: `cargo build --workspace` 编译通过，`cargo clippy --all-targets` 无警告
- **风险**:
  - edition 2024 的语法变更（如 `gen` 成为保留字）可能影响命名 -- 应对：严格检查保留字列表
  - workspace 成员遗漏导致后续 crate 无法发现 -- 应对：T1 完成后立即 `cargo build --workspace` 验证

---

### T2 -- agent-core 类型

- **前置**: T1
- **目标文件**:
  - `crates/agent-core/src/lib.rs`（模块声明 + re-export）
  - `crates/agent-core/src/message.rs`（Message, Role, ContentBlock）
  - `crates/agent-core/src/tool.rs`（ToolCall, ToolCallId, ToolResult）
  - `crates/agent-core/src/usage.rs`（Usage）
  - `crates/agent-core/src/stop.rs`（StopReason）
  - `crates/agent-core/src/cancel.rs`（CancelToken）
  - `crates/agent-core/src/id.rs`（MessageId, SessionId, RunId）
  - `crates/agent-core/src/error.rs`（AgentError, EventError）
- **实现要点**:
  - 全部结构体/枚举 derive `Debug, Clone, PartialEq, Serialize, Deserialize`
  - `CancelToken` 用 `Arc<AtomicBool>` 实现（ADR-0002），提供 `new()`、`cancel()`、`is_cancelled()` 方法
  - `ToolResult` 定义在 agent-core 中（设计文档明确：ContentBlock::ToolResult 与 Tool::call 共用此类型），含 `content: String` 和 `is_error: bool`
  - `EventError` 定于 agent-core（设计文档明确：EventSink 与 ModelEventSink 共用，避免 model->event 依赖）
  - `AgentError` 包含 `Event(EventError)` 变体，用于 loop 层 sink 失败的错误传播
  - `StopReason` 三个变体：`Completed`、`MaxRounds`、`Cancelled`
  - ID 类型用 `String` 包装的 newtype，derive serde
  - `Message` 含 `role: Role` + `content: Vec<ContentBlock>`
  - `ContentBlock` 三个变体：`Text(String)`、`ToolCall(ToolCall)`、`ToolResult(ToolResult)`
- **验收标准**: TC1 — 所有类型可序列化/反序列化；CancelToken 可协作取消；StopReason 覆盖三种终止原因
- **依赖的前置任务**: T1
- **风险**:
  - serde 边界类型（如 `serde_json::Value` 在 ToolCall.arguments）的序列化行为需验证 -- 应对：编写 roundtrip 测试
  - edition 2024 下 async-trait 与 Rust 新语法的兼容性 -- 应对：T2 不涉及 async-trait，低风险

---

### T3 -- agent-event 事件

- **前置**: T2
- **目标文件**:
  - `crates/agent-event/src/lib.rs`
  - `crates/agent-event/src/event.rs`（AgentEvent 枚举）
  - `crates/agent-event/src/sink.rs`（EventSink trait）
  - `crates/agent-event/src/noop.rs`（NoopEventSink）
  - `crates/agent-event/src/collecting.rs`（CollectingSink，test-util feature）
- **实现要点**:
  - `AgentEvent` 标记 `#[non_exhaustive]`，6 个变体与设计文档一致：`UserMessage { message }`、`ModelTextDelta { text }`、`ToolCall { call }`、`ToolResult { id, result }`、`RunFinished { stop_reason, usage, rounds }`、`RunFailed { error }`
  - `EventSink` trait：`fn emit(&mut self, event: AgentEvent) -> Result<(), EventError>`（同步推式，ADR-0004 第 3 点）
  - `NoopEventSink`：`emit` 恒返回 `Ok(())`
  - `CollectingSink`：内部 `Vec<AgentEvent>`，提供 `events() -> &[AgentEvent]`，仅在 `#[cfg(test)]` 或 `feature = "test-util"` 下可用
  - `AgentEvent` 引用的 `Message`、`ToolCall`、`ToolResult`、`StopReason`、`Usage` 均来自 agent-core
- **验收标准**: TC12 — 事件顺序与内容正确；NoopEventSink 静默丢弃；CollectingSink 收集全部事件；emit 失败传播 EventError
- **依赖的前置任务**: T2
- **风险**:
  - `#[non_exhaustive]` 枚举在外部 crate match 时需 `_ => {}` 分支 -- 应对：文档标注这是设计意图，v1-v4 扩展变体时不破坏外部代码

---

### T4 -- agent-model 模型口

- **前置**: T2
- **目标文件**:
  - `crates/agent-model/src/lib.rs`
  - `crates/agent-model/src/trait_def.rs`（Model trait）
  - `crates/agent-model/src/event.rs`（ModelEvent, ModelEventSink）
  - `crates/agent-model/src/request.rs`（ModelRequest, ModelResponse）
  - `crates/agent-model/src/error.rs`（ModelError）
  - `crates/agent-model/src/mock.rs`（MockModel，feature `test-util`）
- **实现要点**:
  - `Model` trait：`#[async_trait]`，`fn model_id() -> &str` + `async fn complete(request, sink) -> Result<ModelResponse, ModelError>`
  - `ModelEvent` 标记 `#[non_exhaustive]`，仅 `TextDelta { text }`（设计文档：v3 动态插件的最小 ABI 面）
  - `ModelEventSink` trait：`fn emit(&mut self, event: ModelEvent) -> Result<(), EventError>`
  - `ModelRequest`：含 `messages: Vec<Message>`、`tools: Vec<ToolSpec>`（build() 期缓存的 ToolSpec 传入）
  - `ModelResponse`：含 `content: Vec<ContentBlock>`、`usage: Usage`、`stop_reason: StopReason`
  - `ModelError`：`Sink(EventError)`、`Provider(String)`、`Serde(serde_json::Error)`
  - `MockModel`：可配置返回预设 response 序列，支持文本回复和 tool call 场景；feature `test-util` 控制
- **验收标准**: TC4 partial -- MockModel 可返回预设响应；ModelEventSink 可接收 TextDelta；ModelError 传播 sink 错误
- **依赖的前置任务**: T2
- **风险**:
  - `ModelRequest` 携带 `Vec<ToolSpec>` 而非 `Vec<Tool>` -- 需确保 ToolSpec 在 build() 期生成并缓存（设计文档明确要求每轮零重建 ToolSpec）
  - MockModel 的序列化配置复杂度 -- 应对：v0 仅支持线性预设响应，不做通用 mock 框架

---

### T5 -- agent-tool 工具口

- **前置**: T2
- **目标文件**:
  - `crates/agent-tool/src/lib.rs`
  - `crates/agent-tool/src/trait_def.rs`（Tool trait）
  - `crates/agent-tool/src/spec.rs`（ToolSpec）
  - `crates/agent-tool/src/context.rs`（ToolContext）
  - `crates/agent-tool/src/error.rs`（ToolError）
  - `crates/agent-tool/src/registry.rs`（ToolRegistry）
- **实现要点**:
  - `Tool` trait：`#[async_trait]`，`fn spec() -> ToolSpec`（build() 期调用一次并缓存）+ `async fn call(input, ctx) -> Result<ToolResult, ToolError>`
  - `ToolSpec`：含 `name: String`、`description: String`、`input_schema: serde_json::Value`；derive Debug, Clone, Serialize, Deserialize
  - `ToolContext<'a>`：标记 `#[non_exhaustive]`，含 `cancel: &'a CancelToken`；提供 `new(cancel)` 构造器
  - `ToolError`：`InvalidInput(String)`、`Execution(String)`；derive thiserror
  - `ToolRegistry`：构建期注册 `Vec<Box<dyn Tool>>`，按名称查找 `fn get(&self, name: &str) -> Option<&dyn Tool>`；**构建时查重**（ADR-0004 第 7 点），重复名称返回错误
  - `ToolRegistry::build(tools)` 返回 `Result<ToolRegistry, ToolError>`（重复工具名报错）
- **验收标准**: TC10 -- ToolRegistry 按名称查找；重复名称报错；缺失工具返回 None；ToolContext 携带取消句柄
- **依赖的前置任务**: T2
- **风险**:
  - ToolSpec 的 `input_schema` 用 `serde_json::Value` 而非 schemars -- v0 可接受，v1 再引入 schemars 生成 schema
  - `ToolRegistry` 存储 `Box<dyn Tool>` 的生命周期 -- 需 `'static` bound，对 v0 足够

---

### T6 -- agent-session 会话口

- **前置**: T2
- **目标文件**:
  - `crates/agent-session/src/lib.rs`
  - `crates/agent-session/src/trait_def.rs`（Session trait）
  - `crates/agent-session/src/memory.rs`（MemorySession）
  - `crates/agent-session/src/error.rs`（SessionError）
- **实现要点**:
  - `Session` trait（ADR-0003）：`fn messages(&self) -> &[Message]`（同步切片视图）+ `async fn append(&mut self, message: Message) -> Result<(), SessionError>`
  - `MemorySession`：内部 `Vec<Message>`，append 只做 push（永不失败），messages 返回切片
  - `SessionError`：`Storage(String)`（预留持久化扩展）；v0 MemorySession 不产生错误
  - 提供 `MemorySession::new()` 构造器
- **验收标准**: TC11 -- Session 追加消息后可读取；空会话返回空切片；append 返回 Ok
- **依赖的前置任务**: T2
- **风险**:
  - `async fn append` 对 MemorySession 来说"杀鸡用牛刀" -- 这是 ADR-0003 的决策，代价仅为多一层 Result 包装，未来 JSONL/SQLite 适配器直接受益

---

### T7 -- agent-component 容器

- **前置**: T2（以及 T3-T6 的类型定义，但实际只需 agent-core + trait 引用）
- **目标文件**:
  - `crates/agent-component/src/lib.rs`
  - `crates/agent-component/src/context.rs`（RuntimeContext）
  - `crates/agent-component/src/limits.rs`（RunLimits）
- **实现要点**:
  - `RuntimeContext<'a>` 标记 `#[non_exhaustive]`，字段：`model: &'a dyn Model`、`registry: &'a ToolRegistry`、`session: &'a mut dyn Session`、`events: &'a mut dyn EventSink`、`cancel: &'a CancelToken`、`limits: RunLimits`
  - 提供 `RuntimeContext::new(...)` 构造器（设计文档明确：non_exhaustive 结构体不可跨 crate 字面构造，需伴生构造器）
  - `RunLimits`：`max_rounds: u32`（默认 10）；derive Debug, Clone
  - agent-component 依赖 agent-model（Model trait）、agent-tool（ToolRegistry）、agent-session（Session trait）、agent-event（EventSink trait）、agent-core（CancelToken）
  - RuntimeContext 与 ToolContext 均为 `#[non_exhaustive]`，分别在各自 crate 提供 `new()` -- 设计文档 §实现注记
- **验收标准**: `cargo build -p agent-component` 编译通过；`RuntimeContext::new()` 可构造
- **依赖的前置任务**: T2（需要 agent-core 的 CancelToken），T4/T5/T6 的 trait 定义
- **风险**:
  - agent-component 依赖 4 个组件 crate，是依赖扇入最大的 crate -- 应对：只引用 trait，不引入实现，编译时间可控
  - `&'a mut dyn Session` 和 `&'a mut dyn EventSink` 的双可变借用 -- 需确保调用侧不同时借用，BasicLoop 通过分步使用避免

---

### T8 -- agent-loop 基础循环

- **前置**: T3, T4, T5, T6, T7
- **目标文件**:
  - `crates/agent-loop/src/lib.rs`
  - `crates/agent-loop/src/trait_def.rs`（AgentLoop trait）
  - `crates/agent-loop/src/basic.rs`（BasicLoop）
  - `crates/agent-loop/src/input.rs`（AgentInput）
  - `crates/agent-loop/src/result.rs`（RunResult）
  - `crates/agent-loop/src/error.rs`（LoopError）
  - `crates/agent-loop/src/forwarder.rs`（Forwarder -- 模型事件转发器）
- **实现要点**:
  - `AgentLoop` trait：`#[async_trait]`，`async fn run_turn(input, ctx) -> Result<RunResult, LoopError>`
  - `AgentInput`：`text: String`（用户输入文本）
  - `RunResult`：`stop_reason: StopReason`、`usage: Usage`、`rounds: u32`、`final_message: Option<Message>`
  - `LoopError`：`Model(ModelError)`、`Tool(ToolError)`、`Event(EventError)`
  - `BasicLoop` 实现完整 8 步数据流（design.md 数据流 0-8）：
    1. 检查 cancel -> RunFinished{Cancelled}，rounds=0
    2. `session.append(input)` -> emit UserMessage
    3. 组装 ModelRequest（ToolSpec 从 registry 已缓存，每轮零重建）-> `model.complete(req, &mut Forwarder)`
    4. Forwarder 将 ModelEvent::TextDelta 转发为 AgentEvent::ModelTextDelta -> emit
    5. append 助手消息（cancel 于调用期间置位时仍照常 append）；usage 累加
    6. 无 ToolCall -> emit RunFinished{Completed}，Ok(RunResult)
    7. 有 ToolCall：串行执行 -> emit ToolCall -> registry 查找 -> miss 则合成 is_error 结果（ADR-0001）/ 命中则 `tool.call(args, ToolContext{cancel})` -> emit ToolResult -> append tool-result 消息 -> 轮数+1 -> 回 3
    8. ToolError -> 先补写协议配对结果（ADR-0004 第 6 条）-> emit RunFailed -> Err(LoopError::Tool)
    9. 触顶（下一轮边界检查）-> emit RunFinished{MaxRounds}，final_message: None
  - **终局事件不变式**（ADR-0004 第 5 点）：每个 run 恰好一个终局事件（RunFinished 或 RunFailed）；错误路径先 emit 终局事件再返回 Err；终局事件发送失败只返回 Err（不重试）
  - `Forwarder`：实现 `ModelEventSink`，将 `ModelEvent::TextDelta` 转发为 `AgentEvent::ModelTextDelta` 并写入 `EventSink`
- **验收标准**:
  - TC2: 文本回复 -- MockModel 返回纯文本，BasicLoop 产出 RunFinished{Completed}
  - TC3: 单次工具调用 -- MockModel 返回 ToolCall，BasicLoop 查找并执行工具，回喂后 MockModel 返回文本
  - TC4: 多轮工具调用 -- 连续 tool call 直到文本回复
  - TC5: 工具错误回喂 -- 工具返回 is_error 结果，模型可见并可自愈
  - TC6: 未注册工具 -- Registry miss 合成 is_error 结果回喂（ADR-0001）
  - TC7: 取消 -- CancelToken 置位后 RunFinished{Cancelled}
  - TC8: 最大轮数 -- 触顶 RunFinished{MaxRounds}
  - TC9: ToolError 终止 -- 补写协议配对后 RunFailed
  - TC13: 增量事件拼接 -- 所有 ModelTextDelta 拼接结果 == 最终消息文本
- **依赖的前置任务**: T3, T4, T5, T6, T7
- **风险**:
  - 8 步数据流的 cancel 检查点位置 -- 必须在步骤边界（模型调用之间、工具执行之间），不可在调用中途
  - ToolError 补写协议配对的正确性 -- 助手消息已入历史，漏补会导致真实模型 API 拒收
  - Forwarder 的 sink 失败处理 -- sink 失败应传播为 LoopError::Event，不可静默吞掉
  - BasicLoop 代码量最大（预估 ~300-400 行），需注意可读性 -- 拆分为步骤函数

---

### T9 -- agent-runtime 组装

- **前置**: T8
- **目标文件**:
  - `crates/agent-runtime/src/lib.rs`
  - `crates/agent-runtime/src/agent.rs`（Agent）
  - `crates/agent-runtime/src/builder.rs`（AgentBuilder）
  - `crates/agent-runtime/src/error.rs`（BuildError）
  - `crates/agent-runtime/src/prelude.rs`（re-export）
- **实现要点**:
  - `Agent`：持有组件所有权（model、registry、session、events、cancel、loop），提供 `async fn run_turn(&mut self, input) -> Result<RunResult, LoopError>` -- 内部组装 RuntimeContext 委托给 loop
  - `Agent` 单实例同时只允许一个 run（`&mut self` 独占借用，设计文档明确）
  - `AgentBuilder`：链式 API，收集 model/tool/session/events/loop，`build()` 返回 `Result<Agent, BuildError>`
  - `BuildError`：`DuplicateTool(String)`（重复工具名）、`MissingModel`、`MissingSession`、`MissingEvents` -- 组装期报错
  - `prelude` 模块 re-export 常用类型：`Agent`、`AgentBuilder`、`AgentInput`、`RunResult`、`StopReason`、`Message`、`ToolCall`、`ToolResult`、`AgentEvent`、`EventSink`、`NoopEventSink`、`MemorySession`、`CancelToken`、`ToolSpec`、`ToolContext`、`RuntimeContext`、`RunLimits`
- **验收标准**: TC14 -- AgentBuilder 可组装完整 Agent；缺失组件报 BuildError；重复工具名报 BuildError；run_turn 委托 BasicLoop 正确执行
- **依赖的前置任务**: T8
- **风险**:
  - Agent 持有 `Box<dyn AgentLoop>` 的 Send + Sync bound -- 需确保 AgentLoop: Send + Sync
  - 组件生命周期管理 -- 所有权在 Agent 中，run_turn 时临时组装 RuntimeContext 借用，需仔细处理 borrow checker

---

### T10 -- 集成测试

- **前置**: T9
- **目标文件**:
  - `tests/agent_loop_basic.rs`（基础 loop 行为）
  - `tests/agent_loop_tools.rs`（工具调用场景）
  - `tests/agent_loop_error.rs`（错误与取消场景）
  - `tests/agent_builder.rs`（组装与验证）
  - `tests/event_collecting.rs`（事件流验证）
- **实现要点**:
  - 端到端测试使用 MockModel + CollectingSink + MemorySession
  - 覆盖 5 个关键场景：
    1. **纯文本回复**：用户输入 -> MockModel 返回文本 -> RunFinished{Completed}，验证 final_message 和 usage
    2. **工具调用循环**：MockModel 先返回 ToolCall -> 工具执行 -> MockModel 返回文本，验证事件序列（ToolCall -> ToolResult -> ModelTextDelta -> RunFinished）
    3. **错误恢复**：工具返回 is_error 结果 -> MockModel 识别并换路 -> 最终文本回复
    4. **取消**：CancelToken 在工具执行前/中置位 -> RunFinished{Cancelled}，验证 rounds 和事件
    5. **最大轮数**：MockModel 始终返回 ToolCall -> 触顶 RunFinished{MaxRounds}
  - 额外场景：AgentBuilder 缺失组件报错、重复工具名报错、增量事件拼接一致性
  - 测试文件使用 `#[tokio::test]`（tokio 为 dev-dependency）
- **验收标准**: TC2-TC14 全量覆盖；所有测试通过；`cargo test --workspace` 零失败
- **依赖的前置任务**: T9
- **风险**:
  - MockModel 的预设响应配置复杂度 -- 线性预设足够 v0 场景
  - 集成测试的异步协调 -- tokio::test 提供确定性执行环境
  - 测试覆盖的边界 case -- 优先覆盖 happy path 和 ADR 明确要求的错误路径

---

## 验收检查清单

全部任务完成后，执行以下验证：

```bash
cargo build --workspace                    # 编译通过
cargo test --workspace                     # 全量测试通过
cargo clippy --all-targets                 # 无警告
cargo fmt --check                          # 格式化检查
```

设计符合性验证：
- 依赖方向单向（无 crate 反向依赖）
- v0 零 tokio 生产依赖（仅 dev-dependency）
- AgentEvent 和 ModelEvent 为 #[non_exhaustive]
- 终局事件不变式（每个 run 恰好一个终局事件）
- ToolError 终止前补写协议配对结果
- CancelToken 使用 std 原语（Arc + AtomicBool）
- Session::append 为 async + Result
- ToolRegistry 构建期查重
