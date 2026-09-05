# v0 最小闭环 — 实现计划

> 轻量、组件化 Rust Agent Runtime 的第一个可运行版本：8 crate 分层骨架 + MockModel 驱动端到端测试。

## 1. 概述

### 目标

YuShan v0 实现 Agent 的最小闭环：把用户输入交给模型；模型如果需要工具就执行工具并把结果交还给模型；模型给出最终回答后结束本轮。v0 验证的核心命题是——**8 crate 分层骨架能否在零外部服务依赖下跑通完整 tool-use 循环**。

### 范围边界

| v0 包含 | v0 不包含 |
|---------|----------|
| 8 crate 骨架与 trait 定义 | 真实模型适配器（OpenAI-compatible、DeepSeek） |
| BasicLoop 串行执行策略 | 并行工具调用（v2） |
| MockModel（可脚本化文本/tool-call 输出） | 运行时 Hook Dispatcher（v2） |
| MemorySession（内存会话） | 持久化 Session（JSONL/SQLite，v2） |
| NoopEventSink + CollectingSink（测试用） | 异步 channel 适配器（v2） |
| 14 项端到端测试（TC1-TC14） | Coding Agent 产品层 |
| 协作式取消（CancelToken） | 强中断 in-flight 调用（v1 tokio） |

### 技术栈约束

- Rust stable 1.88, edition 2024
- 虚拟 workspace（无根包），crate 名 `agent-*`，版本 0.0.1
- 依赖：serde + serde_json（仅 agent-core）、async-trait、thiserror
- tokio 仅 dev-dependency（供 `#[tokio::test]`），v0 无 tokio 生产依赖
- MockModel 通过 feature `test-util` 暴露

## 2. 模块设计

### 2.1 8 Crate 总览

```text
agent-core          基础数据类型 + serde 序列化
  ↑
agent-event         AgentEvent + EventSink trait
agent-model         Model trait + ModelEventSink + MockModel
agent-tool          Tool trait + ToolSpec + ToolRegistry
agent-session       Session trait + MemorySession
  ↑
agent-component     RuntimeContext + RunLimits
  ↑
agent-loop          AgentLoop trait + BasicLoop
  ↑
agent-runtime       Agent + AgentBuilder + prelude
```

依赖严格单向：agent-core ← 组件接口（event/model/tool/session）← component ← loop ← runtime。任何反向依赖均为设计错误，编译器会直接拒绝。

### 2.2 各 Crate 职责与关键类型

#### agent-core — 共享词汇与原语

所有组件共享的稳定数据类型。serde 全覆盖，是整个系统的序列化边界。

```rust
// 取消令牌：std 原语，零 tokio 依赖（ADR-0002）
pub struct CancelToken(Arc<AtomicBool>);

// 运行终止原因
pub enum StopReason { Completed, MaxRounds, Cancelled }

// 工具返回值：core 中唯一定义，ContentBlock::ToolResult 与 Tool::call 共用
pub struct ToolResult { pub content: String, pub is_error: bool }

// 事件发送错误：供 EventSink（agent-event）与 ModelEventSink（agent-model）共用
pub enum EventError { SendFailed }

// 消息模型
pub struct Message { pub role: Role, pub content: Vec<ContentBlock> }
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: ToolCallId, name: String, arguments: serde_json::Value },
    ToolResult { id: ToolCallId, content: String, is_error: bool },
}

// 工具调用
pub struct ToolCall { pub id: ToolCallId, pub name: String, pub arguments: serde_json::Value }

// 模型请求/响应
pub struct ModelRequest { pub messages: Vec<Message>, pub tools: Vec<ToolSpec> }
pub struct ModelResponse { pub message: Message, pub usage: Usage }
pub struct Usage { pub input_tokens: u32, pub output_tokens: u32 }
```

`EventError` 定于 agent-core 而非 agent-event，因为 `EventSink`（agent-event）和 `ModelEventSink`（agent-model）共用此类型——若放在 agent-event，agent-model 将产生对 agent-event 的反向依赖。

#### agent-event — 事件枚举与推式出口

```rust
pub trait EventSink: Send {
    fn emit(&mut self, event: AgentEvent) -> Result<(), EventError>;
}

#[non_exhaustive]
pub enum AgentEvent {
    UserMessage { message: Message },
    ModelTextDelta { text: String },
    ToolCall { call: ToolCall },
    ToolResult { id: ToolCallId, result: ToolResult },
    RunFinished { stop_reason: StopReason, usage: Usage, rounds: u32 },
    RunFailed { error: String },
}
```

设计要点：
- `emit` 是同步 `fn`，非 async——热路径零 Future 装箱，慢 sink 天然背压（ADR-0004）
- `EventSink: Send` 即可，不需要 `Sync`
- `#[non_exhaustive]` 允许 v1+ 增加事件变体而不破坏下游
- 默认实现 `NoopEventSink`（吞掉所有事件）；测试用 `CollectingSink`（收集到 Vec）

#### agent-model — 模型口与窄模型事件口

```rust
#[non_exhaustive]
pub enum ModelEvent { TextDelta { text: String } }

pub trait ModelEventSink: Send {
    fn emit(&mut self, event: ModelEvent) -> Result<(), EventError>;
}

#[async_trait]
pub trait Model: Send + Sync {
    fn model_id(&self) -> &str;
    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError>;
}

pub enum ModelError {
    Sink(EventError),    // Forwarder 写 sink 失败
    Mock(String),        // MockModel 脚本错误（v0 测试用）
}
```

设计要点：
- `ModelEventSink` 是窄口，只承载模型级事件（TextDelta），不承载 run 级事件（ToolCall/ToolResult/RunFinished）——这是 v3 动态插件的最小 ABI 面要求
- `Model` 是 `Send + Sync`（可跨线程共享），但 `complete` 接收 `&mut dyn ModelEventSink`（逐次调用独占 sink）
- MockModel 通过 feature `test-util` 暴露，支持脚本化：按顺序返回预设的文本或 tool call

#### agent-tool — 工具口与注册表

```rust
#[non_exhaustive]
pub struct ToolContext<'a> { pub cancel: &'a CancelToken }

impl<'a> ToolContext<'a> {
    pub fn new(cancel: &'a CancelToken) -> Self { ... }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(
        &self,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> Result<ToolResult, ToolError>;
}

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

pub enum ToolError {
    Execution(String),
}

pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> { ... }
    pub fn lookup(&self, name: &str) -> Result<&dyn Tool, ToolError> { ... }
}
```

设计要点：
- `Tool::spec()` 仅在 `AgentBuilder::build()` 期调用一次并缓存到 `ModelRequest`，每轮零重建
- `ToolContext` 携带 `&CancelToken`，长任务工具可自愿检查取消（ADR-0002）
- `ToolRegistry` 只负责按名称查找，不负责权限判断
- 重复工具名在 `register()` 时即报错（构建期查重），不在运行时才发现

#### agent-session — 会话口

```rust
#[async_trait]
pub trait Session: Send {
    fn messages(&self) -> &[Message];
    async fn append(&mut self, message: Message) -> Result<(), SessionError>;
}

pub struct MemorySession {
    messages: Vec<Message>,
}

pub enum SessionError {
    Full,
}
```

设计要点：
- `append` 是 `async + Result`（ADR-0003），为 v2 JSONL 持久化预留失败通道
- `messages()` 返回同步切片视图，启动时全量加载，持久化是实现细节
- v0 MemorySession 的 `append` 在内存中追加，`Full` 错误可在测试中触发

#### agent-component — 解析后的组件容器

```rust
#[non_exhaustive]
pub struct RuntimeContext<'a> {
    pub model: &'a dyn Model,
    pub registry: &'a ToolRegistry,
    pub session: &'a mut dyn Session,
    pub events: &'a mut dyn EventSink,
    pub cancel: &'a CancelToken,
    pub limits: RunLimits,
}

impl<'a> RuntimeContext<'a> {
    pub fn new(
        model: &'a dyn Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
        cancel: &'a CancelToken,
        limits: RunLimits,
    ) -> Self { ... }
}

pub struct RunLimits {
    pub max_rounds: u32,
}
```

设计要点：
- `RuntimeContext` 定于 agent-component 而非 agent-runtime——agent-loop 的 `AgentLoop::run_turn` 签名引用它，若留在 agent-runtime 则 loop → runtime 成环（ADR-0004）
- `#[non_exhaustive]` 需要 `new()` 伴生构造器，否则 agent-loop/runtime 跨 crate 字面构造会编译失败
- v2 的 Hook trait 也规划于此 crate

#### agent-loop — 可替换执行策略

```rust
#[async_trait]
pub trait AgentLoop: Send + Sync {
    async fn run_turn(
        &self,
        input: AgentInput,
        ctx: &mut RuntimeContext<'_>,
    ) -> Result<RunResult, LoopError>;
}

pub struct AgentInput {
    pub message: Message,
}

pub struct RunResult {
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub rounds: u32,
    pub final_message: Option<Message>,
}

pub enum LoopError {
    Model(ModelError),
    Tool(ToolError),
    Event(EventError),
}
```

设计要点：
- `AgentLoop` 是可替换的执行策略——BasicLoop 是 v0 唯一实现，后续可增加 StreamingLoop、WorkflowLoop 而不修改 Model/Tool/Session 接口
- `RunResult` 仅在正常终止路径返回；错误路径经 `Err(LoopError)` 返回，不产生 `RunResult`
- `final_message` 仅在 `Completed` 时为 `Some`，其余情况为 `None`

#### agent-runtime — 组装 facade

```rust
pub struct Agent {
    loop_impl: Box<dyn AgentLoop>,
    model: Box<dyn Model>,
    registry: ToolRegistry,
    session: Box<dyn Session>,
    events: Box<dyn EventSink>,
    cancel: CancelToken,
    limits: RunLimits,
}

impl Agent {
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        // 借用组件组装 RuntimeContext，委托给 Box<dyn AgentLoop>
    }
}

pub struct AgentBuilder { ... }

impl AgentBuilder {
    pub fn new() -> Self { ... }
    pub fn model(mut self, model: Box<dyn Model>) -> Self { ... }
    pub fn tool(mut self, tool: Box<dyn Tool>) -> Self { ... }
    pub fn session(mut self, session: Box<dyn Session>) -> Self { ... }
    pub fn events(mut self, events: Box<dyn EventSink>) -> Self { ... }
    pub fn limits(mut self, limits: RunLimits) -> Self { ... }
    pub fn build(self) -> Result<Agent, BuildError> { ... }
}

pub enum BuildError {
    DuplicateTool(String),
    MissingComponent(String),
}
```

设计要点：
- `Agent` 单实例同时只允许一个 run（`run_turn(&mut self)`，组件独占借用）
- `build()` 期查重：重复工具名 → `DuplicateTool`，缺 Model/Session → `MissingComponent`
- `prelude` 模块 re-export 常用类型，降低 8 crate 的使用门槛

## 3. 数据流设计：BasicLoop 8 步

BasicLoop 是 v0 的唯一执行策略。以下描述一次 `Agent::run_turn` 的完整数据流，步骤编号与设计文档对齐。`●` 标记事件发射点。

### 步骤 0 — 委托入口

`Agent::run_turn` 接收 `AgentInput`，借用内部组件组装 `RuntimeContext`，委托给 `Box<dyn AgentLoop>`（默认 BasicLoop）。Agent 不包含循环逻辑，只负责组件生命周期与借用组装。

```rust
// agent-runtime 中
impl Agent {
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        let ctx = RuntimeContext::new(
            &*self.model,
            &self.registry,
            &mut *self.session,
            &mut *self.events,
            &self.cancel,
            self.limits,
        );
        self.loop_impl.run_turn(input, ctx).await
    }
}
```

### 步骤 1 — 入口边界检查

检查 `CancelToken`。若已取消，直接 emit 终局事件并返回。

```
if cancel.is_cancelled() {
    ● RunFinished { stop_reason: Cancelled, usage: Usage::default(), rounds: 0 }
    return Ok(RunResult { stop_reason: Cancelled, rounds: 0, .. })
}
```

取消是正常终止路径，不是错误（ADR-0002）。

### 步骤 2 — 追加用户消息

将输入消息 append 到 session，成功后 emit 事件。

```
session.append(input.message.clone())?
● UserMessage { message: input.message }
```

事件 = 已发生之事，append 先于 emit。

### 步骤 3 — 模型调用与增量转发

每轮循环的起点：

```
loop {
    // 组装 ModelRequest（ToolSpec 在 build() 期已缓存，每轮零重建）
    let request = ModelRequest {
        messages: session.messages().to_vec(),
        tools: cached_tool_specs.clone(),
    };

    // 调用模型，通过 Forwarder 转发增量事件
    let mut forwarder = Forwarder { events: &mut *events };
    let response = model.complete(request, &mut forwarder).await?;
    // Forwarder 内部：ModelEvent::TextDelta → ● ModelTextDelta { text }
```

Forwarder 是一个轻量结构体，实现 `ModelEventSink`，将 `ModelEvent::TextDelta` 转发为 `AgentEvent::ModelTextDelta`。sink 失败时返回 `ModelError::Sink(EventError)`。

### 步骤 4 — 追加助手消息

```
    session.append(response.message.clone())?;
    usage = usage + response.usage;  // 按值累加
```

cancel 于调用期间置位时，响应仍照常 append——不丢数据。

### 步骤 5 — 无 ToolCall → 正常完成

```
    if !has_tool_calls(&response.message) {
        ● RunFinished { stop_reason: Completed, usage, rounds }
        return Ok(RunResult { stop_reason: Completed, usage, rounds, final_message: Some(response.message) })
    }
```

### 步骤 6 — 有 ToolCall → 串行执行

v0 逐个串行执行工具：

```
    let tool_calls = extract_tool_calls(&response.message);
    let mut tool_results = Vec::new();

    for call in &tool_calls {
        // 发射工具调用事件
        ● ToolCall { call: call.clone() }

        // registry 查找
        match registry.lookup(&call.name) {
            Ok(tool) => {
                // 命中：调用工具
                match tool.call(call.arguments.clone(), ToolContext::new(cancel)).await {
                    Ok(result) => {
                        tool_results.push(result.clone());
                        ● ToolResult { id: call.id.clone(), result }
                    }
                    Err(ToolError::Execution(msg)) => {
                        // 工具执行失败 → 合成 is_error 结果（ADR-0001）
                        let err_result = ToolResult { content: msg, is_error: true };
                        tool_results.push(err_result.clone());
                        ● ToolResult { id: call.id.clone(), result: err_result }
                    }
                }
            }
            Err(_) => {
                // 未注册工具 → 合成 is_error 结果（ADR-0001）
                let err_result = ToolResult {
                    content: format!("tool '{}' not found", call.name),
                    is_error: true,
                };
                tool_results.push(err_result.clone());
                ● ToolResult { id: call.id.clone(), result: err_result }
            }
        }
    }

    // 全部完成后，append 单条 tool-result 消息（按源顺序）
    session.append(Message {
        role: Role::Tool,
        content: tool_results.into_iter().map(|r| ContentBlock::ToolResult { ... }).collect(),
    })?;

    rounds += 1;
    // 回到步骤 3
}
```

### 步骤 7 — 错误处理

**ToolError（工具无法产出可回喂结果的运行时失败）**：

```
Err(ToolError) => {
    // 补写协议配对消息（ADR-0001 + ADR-0004 第 6 条）
    // 该助手消息中的每个 tool_use 都必须得到结果：
    // - 已执行的：用真实结果
    // - 失败与未执行的：合成 is_error 结果
    // 否则续跑会话会被真实模型 API 拒收
    session.append(pairing_message)?;

    ● RunFailed { error: tool_error.to_string() }
    return Err(LoopError::Tool(tool_error))
}
```

**ModelError**：

```
Err(ModelError) => {
    ● RunFailed { error: model_error.to_string() }
    return Err(LoopError::Model(model_error))
}
```

**EventError（emit 失败）**：

```
Err(EventError) => {
    // 不 emit RunFailed（因为 emit 本身就失败了）
    return Err(LoopError::Event(event_error))
}
```

### 步骤 8 — 触顶

下一轮循环开始前检查 `rounds >= limits.max_rounds`：

```
    if rounds >= limits.max_rounds {
        ● RunFinished { stop_reason: MaxRounds, usage, rounds }
        return Ok(RunResult { stop_reason: MaxRounds, usage, rounds, final_message: None })
    }
```

### 终局不变式

每个 run 恰好一个终局事件：
- 正常路径：`RunFinished`（Completed / MaxRounds / Cancelled）
- 错误路径：先 `RunFailed`，再返回 `Err`
- 若终局事件自身 emit 失败：只返回 Err（不重试，不双发）

## 4. 接口规格

### 4.1 完整类型/签名索引

| crate | 类型 | 签名摘要 |
|-------|------|----------|
| agent-core | `CancelToken` | `pub struct CancelToken(Arc<AtomicBool>)` |
| agent-core | `StopReason` | `enum { Completed, MaxRounds, Cancelled }` |
| agent-core | `ToolResult` | `struct { content: String, is_error: bool }` |
| agent-core | `EventError` | `enum { SendFailed }` |
| agent-core | `Message` | `struct { role: Role, content: Vec<ContentBlock> }` |
| agent-core | `ContentBlock` | `enum { Text, ToolUse, ToolResult }` |
| agent-core | `ToolCall` | `struct { id: ToolCallId, name: String, arguments: Value }` |
| agent-core | `ModelRequest` | `struct { messages: Vec<Message>, tools: Vec<ToolSpec> }` |
| agent-core | `ModelResponse` | `struct { message: Message, usage: Usage }` |
| agent-core | `Usage` | `struct { input_tokens: u32, output_tokens: u32 }` |
| agent-event | `EventSink` | `trait { fn emit(&mut self, AgentEvent) -> Result<(), EventError> }` |
| agent-event | `AgentEvent` | `enum { UserMessage, ModelTextDelta, ToolCall, ToolResult, RunFinished, RunFailed }` |
| agent-model | `ModelEvent` | `enum { TextDelta { text: String } }` |
| agent-model | `ModelEventSink` | `trait { fn emit(&mut self, ModelEvent) -> Result<(), EventError> }` |
| agent-model | `Model` | `trait { fn model_id() -> &str; async fn complete(...) -> Result<ModelResponse, ModelError> }` |
| agent-model | `ModelError` | `enum { Sink(EventError), Mock(String) }` |
| agent-tool | `ToolContext<'a>` | `struct { cancel: &'a CancelToken }` |
| agent-tool | `Tool` | `trait { fn spec() -> ToolSpec; async fn call(...) -> Result<ToolResult, ToolError> }` |
| agent-tool | `ToolSpec` | `struct { name, description, parameters: Value }` |
| agent-tool | `ToolError` | `enum { Execution(String) }` |
| agent-tool | `ToolRegistry` | `struct { tools: HashMap<String, Box<dyn Tool>> }` |
| agent-session | `Session` | `trait { fn messages() -> &[Message]; async fn append(Message) -> Result<(), SessionError> }` |
| agent-session | `MemorySession` | `struct { messages: Vec<Message> }` |
| agent-session | `SessionError` | `enum { Full }` |
| agent-component | `RuntimeContext<'a>` | `struct { model, registry, session, events, cancel, limits }` |
| agent-component | `RunLimits` | `struct { max_rounds: u32 }` |
| agent-loop | `AgentLoop` | `trait { async fn run_turn(AgentInput, &mut RuntimeContext) -> Result<RunResult, LoopError> }` |
| agent-loop | `AgentInput` | `struct { message: Message }` |
| agent-loop | `RunResult` | `struct { stop_reason, usage, rounds, final_message }` |
| agent-loop | `LoopError` | `enum { Model(ModelError), Tool(ToolError), Event(EventError) }` |
| agent-runtime | `Agent` | `struct { loop_impl, model, registry, session, events, cancel, limits }` |
| agent-runtime | `AgentBuilder` | `struct { ... } // 链式 builder` |
| agent-runtime | `BuildError` | `enum { DuplicateTool(String), MissingComponent(String) }` |

### 4.2 MockModel 规格

```rust
// feature = "test-util"
pub struct MockModel {
    id: String,
    responses: VecDeque<MockResponse>,
}

pub enum MockResponse {
    Text(String),
    ToolCall(Vec<ToolCall>),
}
```

脚本化行为：
- 每次 `complete()` 消费 `responses` 队首
- Text 变体：emit `ModelEvent::TextDelta` 逐字（或一次性），返回含 Text ContentBlock 的 response
- ToolCall 变体：返回含 ToolUse ContentBlock 的 response
- 队列耗尽：返回 `ModelError::Mock("no more scripted responses")`
- usage 固定返回 `Usage { input_tokens: 0, output_tokens: 0 }`

## 5. 实现约束

### 5.1 ADR 锁定约束

| ADR | 约束 | v0 影响 |
|-----|------|---------|
| ADR-0001 | 工具失败双通道：错误结果回喂模型，ToolError 才终止回合 | ToolRegistry miss 合成 `is_error` 结果；ToolError 终止前补写配对 |
| ADR-0002 | 协作式取消：CancelToken 在步骤边界检查 | BasicLoop 在步骤 1 和每轮循环入口检查；取消 = RunFinished，不是 Err |
| ADR-0003 | Session::append async + Result | MemorySession append 返回 Ok(())，但签名预留失败通道 |
| ADR-0004 | 8-crate 分层 + 同步推式事件 + 终局事件不变式 | crate 结构不缩减；emit 是同步 fn；每个 run 恰好一个终局事件 |

### 5.2 编译期约束

- crate 依赖严格单向，`cargo check` 全 workspace 通过
- `#[non_exhaustive]` 结构体（RuntimeContext、ToolContext）提供 `new()` 构造器
- 所有 pub 类型实现 `Debug`（便于测试输出）
- `EventError` 在 agent-core，供 agent-event 和 agent-model 共用

### 5.3 运行时约束

- **单 run 独占**：`Agent::run_turn(&mut self)` 组件独占借用，不支持并发 run
- **串行工具**：v0 工具逐个执行，按源顺序 append
- **同步 emit**：慢 sink 阻塞执行器线程——v0 仅 Noop/Collecting sink，不是问题
- **O(n) 历史克隆**：每轮 `session.messages().to_vec()` 构建 ModelRequest，千条消息 ≈ 2MB ≈ 1ms

### 5.4 Workspace 结构

```text
YuShan/
├── Cargo.toml                    # 虚拟 workspace
├── crates/
│   ├── agent-core/
│   │   ├── Cargo.toml            # serde, serde_json, thiserror
│   │   └── src/lib.rs
│   ├── agent-event/
│   │   ├── Cargo.toml            # agent-core, async-trait
│   │   └── src/lib.rs
│   ├── agent-model/
│   │   ├── Cargo.toml            # agent-core, async-trait
│   │   └── src/lib.rs            # + MockModel (feature test-util)
│   ├── agent-tool/
│   │   ├── Cargo.toml            # agent-core, async-trait, serde_json
│   │   └── src/lib.rs
│   ├── agent-session/
│   │   ├── Cargo.toml            # agent-core, async-trait
│   │   └── src/lib.rs
│   ├── agent-component/
│   │   ├── Cargo.toml            # agent-core, agent-model, agent-tool, agent-session, agent-event
│   │   └── src/lib.rs
│   ├── agent-loop/
│   │   ├── Cargo.toml            # agent-core, agent-component, agent-model, agent-tool, agent-event
│   │   └── src/lib.rs
│   └── agent-runtime/
│       ├── Cargo.toml            # agent-core, agent-component, agent-loop, agent-model, agent-tool, agent-session, agent-event
│       └── src/lib.rs
└── docs/
```

## 6. 测试策略

### 6.1 测试矩阵总览

14 项端到端测试（TC1-TC14），全部通过 `Agent + MockModel + CollectingSink` 驱动，验证 BasicLoop 的完整行为。

| 编号 | 名称 | 路径 | 验收标准 |
|------|------|------|----------|
| TC1 | 纯文本回复 | 文本→文本 | RunFinished{Completed}，最终消息含文本，无 ToolCall 事件 |
| TC2 | 单工具调用循环 | 文本→ToolCall→ToolResult→文本 | RunFinished{Completed}，事件序列含 ToolCall+ToolResult，usage 累加正确 |
| TC3 | 多轮工具调用 | 文本→TC→TR→TC→TR→文本 | rounds > 1，每轮 ToolCall/ToolResult 事件配对 |
| TC4 | 未注册工具回喂 | ToolCall(name=unknown) | ToolResult{is_error:true} 回喂，模型收到错误后给出文本回答 |
| TC5 | 工具执行错误回喂 | Tool.call 返回 Err(Execution) | ToolResult{is_error:true} 回喂，模型收到错误后给出文本回答 |
| TC6 | ToolError 终止 | Tool.call 返回 Err(ToolError) | RunFailed 事件 + Err(LoopError::Tool)，配对消息完整 |
| TC7 | 模型错误终止 | Model.complete 返回 Err | RunFailed 事件 + Err(LoopError::Model) |
| TC8 | 协作式取消（入口） | CancelToken.cancel() 在 run 前 | RunFinished{Cancelled}，rounds=0 |
| TC9 | 协作式取消（轮间） | CancelToken.cancel() 在 tool 执行后 | RunFinished{Cancelled}，rounds>0 |
| TC10 | 最大轮数 | max_rounds=2，MockModel 连续返回 ToolCall | RunFinished{MaxRounds}，rounds=2，final_message=None |
| TC11 | 终局事件不变式 | 各终止路径 | 每种路径恰好一个终局事件（RunFinished 或 RunFailed） |
| TC12 | 增量拼接一致性 | CollectingSink 收集 ModelTextDelta | 拼接所有 ModelTextDelta.text == 最终消息中的 Text ContentBlock |
| TC13 | Builder 查重 | 重复注册同名工具 | build() 返回 Err(BuildError::DuplicateTool) |
| TC14 | Builder 缺组件 | 不设置 Model | build() 返回 Err(BuildError::MissingComponent) |

### 6.2 测试基础设施

```rust
// 测试用 EventSink：收集所有事件
pub struct CollectingSink {
    pub events: Vec<AgentEvent>,
}

impl EventSink for CollectingSink {
    fn emit(&mut self, event: AgentEvent) -> Result<(), EventError> {
        self.events.push(event);
        Ok(())
    }
}

// 测试用 EventSink：第 N 次 emit 失败（测试 EventError 路径）
pub struct FailingSink {
    pub fail_after: u32,
    pub count: u32,
}

impl EventSink for FailingSink {
    fn emit(&mut self, event: AgentEvent) -> Result<(), EventError> {
        self.count += 1;
        if self.count > self.fail_after {
            Err(EventError::SendFailed)
        } else {
            Ok(())
        }
    }
}
```

### 6.3 测试模式

每项测试的标准模式：

```rust
#[tokio::test]
async fn tc1_pure_text_reply() {
    // 1. 构建 MockModel 脚本
    let model = MockModel::new("test", vec![
        MockResponse::Text("Hello!".into()),
    ]);

    // 2. 构建 Agent
    let mut agent = AgentBuilder::new()
        .model(Box::new(model))
        .session(Box::new(MemorySession::new()))
        .events(Box::new(CollectingSink::new()))
        .build()
        .unwrap();

    // 3. 执行
    let result = agent.run_turn(AgentInput {
        message: Message { role: Role::User, content: vec![ContentBlock::Text { text: "hi".into() }] },
    }).await.unwrap();

    // 4. 断言
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert!(result.final_message.is_some());
    // 断言事件序列...
}
```

### 6.4 测试分层

| 层级 | 测试位置 | 内容 |
|------|----------|------|
| 单元测试 | 各 crate 内 `#[cfg(test)]` | ToolRegistry 查找/查重、MemorySession append、CancelToken 状态 |
| 集成测试 | agent-runtime `tests/` | TC1-TC14 端到端测试 |
| 属性测试 | 后续 v1 | 用 proptest 验证 BasicLoop 的终局不变式 |

### 6.5 通过标准

- `cargo test --workspace` 全部通过
- `cargo clippy --workspace` 无 warning
- 14 项 TC 全部绿灯
- 终局事件不变式：每条测试的事件序列中 RunFinished + RunFailed 恰好出现一次

## 7. 实现顺序

建议按依赖深度从底向上实现：

1. **agent-core**：所有基础类型，无外部依赖，编译即验证
2. **agent-event**：EventSink + AgentEvent + NoopEventSink
3. **agent-model**：Model + ModelEventSink + MockModel（feature test-util）
4. **agent-tool**：Tool + ToolSpec + ToolContext + ToolRegistry
5. **agent-session**：Session + MemorySession
6. **agent-component**：RuntimeContext + RunLimits
7. **agent-loop**：AgentLoop + BasicLoop + Forwarder
8. **agent-runtime**：Agent + AgentBuilder + prelude

每完成一层即可 `cargo check --workspace` 验证编译，最终 TC1-TC14 在第 8 步全部跑通。

## 8. 相关文档

- [总体设计](../design.md) -- YuShan 整体架构与路线
- [v0 架构设计](../arch/v0-最小闭环/design.md) -- 候选方案对比与选择理由
- [v0 架构审查](../arch/v0-最小闭环/review.md) -- 对抗性质量分析
- [ADR-0001](../adr/0001-tool-failure-dual-channel.md) -- 工具失败双通道
- [ADR-0002](../adr/0002-cooperative-cancellation-in-core.md) -- 协作式取消
- [ADR-0003](../adr/0003-session-append-async-result.md) -- Session append async + Result
- [ADR-0004](../adr/0004-v0-layered-skeleton-and-sync-events.md) -- v0 骨架与同步事件
