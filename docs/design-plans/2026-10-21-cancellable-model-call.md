# 可取消的模型调用 设计计划

## 背景

当前 YuShan 的取消能力只覆盖**轮边界**：用户按 Esc 触发 `Boundary::Abort`，`ys-loop` 在轮与轮之间探测到 abort 后发出 `RunFinished{StopReason::Cancelled}` 收场。

但**模型调用期间**（实测占单轮墙钟约 80%）abort 完全不生效：

- `Model::complete(&self, request, sink) -> Result<ModelResponse, ModelError>` —— **签名里没有取消入参**。
- `crates/ys-loop/src/basic.rs:137` 与 `:174` 的 abort 探针**都在模型调用之前**；模型一旦进入 await，Esc 只能等到这次调用自然返回（超时或正常完成）才被感知。
- `crates/ys-loop/src/basic.rs:360-377`：Step 6 调 model 时，**任何 `Err(e)` 都发 `RunFailed` 并 return Err** —— 即使用户主动取消，也会被呈现为「运行失败」。

用户体感：按了 Esc，界面毫无反应，必须等模型跑完才停下。

本设计的来源是 `docs/adr/0046-cancellable-model-call-and-abort-feedback.md`（proposed）。ADR 的 Phase 1 as-built 核实已完成，下面引用的行号与事实**均已对照真实代码核实过**。

设计目标：

1. 模型调用在**进行中**可被取消，取消应尽快生效（目标：100ms 量级感知）。
2. 取消**不是失败**：不发 `RunFailed`，不发假的 `RoundUsage`，按 `StopReason::Cancelled` 收场。
3. 已产生的部分内容（文本）**不丢**：进会话历史，让屏幕可见内容与下一轮上下文一致。
4. `Model` trait **零改动**（不破坏 `ys-model` 的最小核心定位与既有适配器实现）。
5. 既有路径**逐字节不变**（序列化、事件序列、终局事件形态）。

## 设计

### 整体方案

采用「**取消信号随请求下行 + 适配器内部自检**」的方案：

```
┌──────────────┐   Esc
│  TUI 事件层  │──────► Boundary::Abort（投进 QueueBoundarySource）
└──────────────┘                │
                                ▼
                    ┌───────────────────────────┐
                    │ QueueBoundarySource(Arc)  │  ← 'static，可共享
                    └───────────────────────────┘
                                │ Arc<dyn BoundarySource>
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                 ▼
   RuntimeContext.boundary  ModelRequest.boundary  ToolContext.boundary
   (ys-component)            (ys-model)              (ys-tool)
              │                 │
              ▼                 ▼
     ys-loop 探针           Model::complete 实现
     (轮边界 / 工具之间)    （适配器内部 select! 包裹 await）
```

关键点：**取消信号与请求一起走**，而不是作为 `Model::complete` 的新参数。理由见「架构决策」决策 A。

取消在适配器内部通过**三处非嵌套 `tokio::select!`** 生效：把「模型调用的 await」与「等待 abort」放在同一个 `select!` 里，谁先完成就走谁。

### 架构决策（方案选择及理由）

#### 决策 A：取消信号走 `ModelRequest.boundary`，`Model` trait 零改动

**选择**：`ModelRequest` 增加字段

```rust
#[serde(skip, default)]
pub boundary: Option<Arc<dyn BoundarySource>>,
```

**理由**：

- **trait 零改动** ⇒ 不破坏 `Model` 的最小核心契约；所有既有适配器（含第三方实现）无需改签名即可编译。
- `Arc<dyn BoundarySource>` **不是** `Serialize` ⇒ 必须 `#[serde(skip, default)]` ⇒ **既有序列化字节逐字节不变**（满足 N10）。
- `Default` 派生要求字段实现 `Default`；`Option<Arc<dyn BoundarySource>>` 的 `Default` 是 `None` ⇒ 派生可继续，既有 `..Default::default()` 构造点**不需要改**。
- `Clone` 派生要求字段实现 `Clone`；`Arc` 是 `Clone` ⇒ 派生可继续，且 clone 后**指向同一队列**（取消语义正确，满足 N11）。

**代价**：`crates/ys-model/Cargo.toml` 需新增 `ys-protocol = { path = "../ys-protocol" }` 依赖。

**不成环证明**：`ys-tool` 已依赖 `ys-protocol`，且 `ys-model` 已依赖 `ys-tool` ⇒ `ys-model → ys-protocol` 不引入新环。`ys-protocol` 自身零 tokio（依赖 `ys-core` / `ys-event` / `serde`），符合硬约束。

**否决的替代方案**：

| 方案 | 否决理由 |
|---|---|
| `Model::complete(&self, request, sink, cancel: &CancelToken)` | 改 trait 签名 ⇒ 所有适配器与所有调用点都要改；且 `CancelToken` 需要 `Send + Sync + 'static`，在 `&self` 方法上传递生命周期困难 |
| `ModelEventSink` 增加 `is_cancelled()` 方法 | sink 是**推式出口**，语义是「发生了什么」；让它变成控制面违反「事件只观察」边界；且 sink 的实现者与调用者分离，cancel 源在 loop 侧不在 sink 侧 |
| `ModelRequest` 传 `Arc<dyn Fn() -> bool>` 闭包 | 丢失 `BoundarySource` 的既有语义（`take()` / `is_aborted()`），且闭包要捕获 `Arc` 形成自引用 |

#### 决策 B：边界源 Arc 化（改签名，共 3 处**签名**；但**调用点**共 25+ 处）

**选择**：

- `RuntimeContext.boundary`：`Option<&'a dyn BoundarySource>` → `Option<Arc<dyn BoundarySource>>`
- `AgentPorts.boundary`：同上
- `with_boundary` 相应改为接收 `Arc<dyn BoundarySource>`

> ⚠️ **注意**：改的是 **3 处签名**，但受影响的**调用点**是 **25+ 处**（含测试文件）。完整清单见「涉及文件」章节。**签名改了但调用点漏改 ⇒ Phase 3 编译失败**。

**理由**：

- `Arc<dyn BoundarySource>` 需要 `'static` ⇒ 不能借用栈上临时值 ⇒ app 层必须持有 `Arc`（`app_loop.rs:998` 处 `let boundary = Arc::new(QueueBoundarySource::new());`，`:1004` 传 `Some(boundary.clone())`；`main.rs:1715-1723` 同样处理）。
- 共享引用语义正确：loop 侧与 model 侧看到**同一个** abort 状态。
- `RuntimeContext` 已是 `#[non_exhaustive]` 且不 `Clone`；`boundary` 改类型不影响其余字段的生命周期 `'a`（`Arc` 是 `'static`，与 `'a` 无关）。

**连带影响**（`ctx.boundary` 的全部使用点）：

| 位置 | 原写法 | 新写法 |
|---|---|---|
| `crates/ys-loop/src/basic.rs:137` | `ctx.boundary.is_some_and(\|b\| b.is_aborted())` | 不变（`Option<Arc<..>>` 上 `is_some_and` 可用，`b` 自动 deref 到 `&dyn BoundarySource`） |
| `crates/ys-loop/src/basic.rs:174` | 同上 | 不变 |
| `crates/ys-loop/src/basic.rs:305` | `if let Some(boundary) = ctx.boundary` | `ctx.boundary.as_ref()` |
| `crates/ys-loop/src/basic.rs:670` / `:724` | `ToolContext::new(ctx.boundary, ...)` | `ToolContext::new(ctx.boundary.as_deref(), ...)`（`ToolContext.boundary` 是 `Option<&dyn BoundarySource>`） |

**已核实（grep）**：`crates/ys-loop/src/compact.rs` **无** `ctx.boundary` 使用点（`boundary` 仅出现在测试名与注释里）⇒ **无需适配**。

**否决的替代方案**：`Option<&'a dyn BoundarySource>` 保持不变、另外再加一个 `'static` 引用字段 ⇒ 双字段表达同一件事，必然出现两者不一致的状态，且 `RuntimeContext` 已有「不 Clone / non_exhaustive」的定位，再加字段是噪声。

##### 决策 B 的具体形态（签名怎么改）

「Arc 化」本身不足以落地 —— 必须定死**每个签名改成什么**。以下为**采纳形态**：

```rust
// crates/ys-component/src/context.rs
pub struct RuntimeContext<'a> {
    // …其余字段不动…
    /// 轮边界控制源（steering + abort 的唯一入口）。
    ///
    /// `Arc` 化有两个原因：(1) `ModelRequest.boundary` 需要 `'static` 的
    /// `Arc<dyn BoundarySource>`（借用无法满足）；(2) 同一份边界源要同时
    /// 交给 loop、工具层、模型层三处。
    ///
    /// `None` = 无边界控制，行为与不挂载时逐字节一致（既无插话也无取消）。
    pub boundary: Option<Arc<dyn BoundarySource>>,
    // …
}

impl<'a> RuntimeContext<'a> {
    pub fn with_boundary(mut self, boundary: Arc<dyn BoundarySource>) -> Self {
        self.boundary = Some(boundary);
        self
    }
}
```

对应的其余两处签名：

```rust
// crates/ys-runtime/src/agent.rs
impl<'a> AgentPorts<'a> {
    pub fn new(
        model: Option<&'a dyn Model>,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
        boundary: Option<Arc<dyn BoundarySource>>,
    ) -> Self { /* … */ }
}

// apps/coding-agent/src/wiring.rs
impl Wiring {
    pub fn ports_with_boundary<'a>(
        &'a mut self,
        boundary: Option<Arc<dyn BoundarySource>>,
    ) -> AgentPorts<'a> { /* … */ }
}
```

**`ys-tool` 层零改动**（工具层的公开类型保持不变）：

- `ys_tool::ToolContext.boundary` 仍是 `Option<&'a dyn BoundarySource>`；
- `ys_tool::SubagentRunner::run(&self, prompt, model, boundary: Option<&dyn BoundarySource>)` **签名不变**；
- 适配点只有一个：在把 `ctx.boundary`（`Option<Arc<dyn BoundarySource>>`）交给工具层时，用 **`ctx.boundary.as_deref()`** 转成 `Option<&dyn BoundarySource>`。

⇒ `crates/ys-tool/` **零改动**，`adapters/tools-basic/`、`adapters/tools-web/` 的**业务代码**也零改动（只有测试替身需要适配 `Debug` supertrait，见决策 B′）。

**为什么 `Arc<dyn BoundarySource>` 必须是 `'static`**：`Arc<dyn BoundarySource>` 默认即 `Arc<dyn BoundarySource + 'static>` ⇒ 边界源**不能**借用栈上临时值 ⇒ app 层必须自己持有 `Arc`（`Arc::new(QueueBoundarySource::new())`）。

**`'a` 的语义变化（是放宽，不是收紧）**：`RuntimeContext<'a>` 的 `'a` 仍约束 `model` / `session` / `events` / `subagent` 等借用字段；`boundary` 字段是 `'static` 的 ⇒ **不再受 `'a` 约束**。

**`ChildBoundary::new(parent)` 必须改为接收 `Option<Arc<dyn BoundarySource>>`**（原设计以为可保持 `Option<&'a dyn BoundarySource>` 不变 —— **这是错的**）：

- **原因**：`Arc<dyn BoundarySource>` 默认隐含 `+ 'static`；而 `ChildBoundary<'a> { parent: Option<&'a dyn BoundarySource> }` 持**父的借用** ⇒ `Arc<ChildBoundary<'_>>` **不是** `'static` ⇒ 无法 coerce 成 `Arc<dyn BoundarySource>` ⇒ `AgentPorts::new` 收不下。
- **证据**：已用 rustc 最小复现验证 —— `error[E0597]: parent does not live long enough ... coercion requires that parent is borrowed for 'static`。
- **修法**：`ChildBoundary` **去掉 `<'a>` 生命周期参数**，`parent` 字段改为 `Option<Arc<dyn BoundarySource>>` ⇒ 自身 `'static` ⇒ `Arc::new(ChildBoundary::new(parent_arc))` 可用。
- **连带**：`is_aborted` 里 `self.parent.is_some_and(...)` 改 `self.parent.as_ref().is_some_and(...)`（`Option<Arc<..>>` 非 `Copy` ⇒ 不加 `.as_ref()` 会 E0507）。
- **父边界源如何到达 `ChildBoundary`**：`SubagentRunner::run` 的 `boundary: Option<&dyn BoundarySource>`（`ys-tool` 层，**签名不变**）只有借用 ⇒ 无法从中造 `Arc` ⇒ `AppSubagentRunner` 改为**持** `Option<Arc<dyn BoundarySource>>`（`make_runner` 时传入）⇒ `run` 内用 `self.parent_boundary.clone()`。

#### 决策 B′：`BoundarySource` 加 `Debug` supertrait（`ModelRequest` 的 `Debug` 派生保留）

**选择**：

```rust
// crates/ys-protocol/src/boundary.rs
pub trait BoundarySource: Send + Sync + std::fmt::Debug {
    fn take(&self) -> Option<Boundary>;
    fn is_aborted(&self) -> bool;
}
```

**为什么必须加**（编译硬伤，非风格问题）：

- `crates/ys-model/src/request.rs:6`：`#[derive(Debug, Clone, Default, Serialize, Deserialize)] pub struct ModelRequest`；
- 决策 A 让 `ModelRequest` 新增 `pub boundary: Option<Arc<dyn BoundarySource>>`；
- `Arc<dyn BoundarySource>` 的 `Debug` 要求 `dyn BoundarySource: Debug` ⇒ **`BoundarySource` 必须有 `Debug` supertrait**，否则 `#[derive(Debug)]` **编译失败**：

```
error[E0277]: `(dyn BS + 'static)` doesn't implement `Debug`
  --> #[derive(Debug, Clone)] struct R { pub b: Option<Arc<dyn BS>> }
```

（该错误已用 rustc 最小复现验证。）

**理由**：

- **保住 `ModelRequest` 的 `Debug` 派生** ⇒ `ys-model` 的对外 API 不缩减（下游打印 `ModelRequest` 的代码不会编译失败）。
- `BoundarySource` 的实现者**本就只有 5 个**，且**全在仓内**（无外部实现者）⇒ 加 supertrait 的破坏面完全可控：
  1. `crates/ys-protocol/src/boundary.rs:99` — `QueueBoundarySource`（生产实现）；
  2. `crates/ys-loop/src/lib.rs:758` — `DecoupledBoundarySource`（测试替身）；
  3. `apps/coding-agent/src/subagent.rs:83` — `ChildBoundary<'_>`（生产实现）；
  4. `adapters/tools-web/tests/tools.rs:1872` — `StoredBoundary`（测试替身）；
  5. `adapters/tools-web/tests/tools.rs:1908` — `ArmBoundary`（测试替身）。
- **不泄露 steer 消息内容**：派生 `Debug` 会把队列里的 `Boundary::Steer(Message)` **原样打印**（消息正文进日志）⇒ **不采用派生**，改为**手写 `impl Debug`**，只输出「是否已 abort」与「队列长度」两项结构信息，不输出任何消息内容。

**手写 `Debug` 的形态约定**：

```rust
// crates/ys-protocol/src/boundary.rs
impl std::fmt::Debug for QueueBoundarySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只报结构，不报内容：队列里的 Steer 消息正文不得进日志。
        let inner = self.lock();
        f.debug_struct("QueueBoundarySource")
            .field("aborted", &inner.aborted)
            .field("queued", &inner.queue.len())
            .finish()
    }
}
```

**否决的替代方案**：

| 方案 | 否决理由 |
|---|---|
| 去掉 `ModelRequest` 的 `Debug` 派生 | `ModelRequest` 是 `ys-model` 的**公开类型** ⇒ 去掉 `Debug` 是对外 API 缩减，下游 `{:?}` 打印会编译失败（改动面比方案 B′ 大） |
| `ModelRequest` 手写 `impl Debug` 跳过 boundary 字段 | 绕过问题但丢掉 `Clone` / `Default` 派生的简洁性，且手写 `Debug` 冗长、易与字段漂移 |
| `BoundarySource` 加 supertrait 但**派生** `Debug` | 派生会把 steer 消息正文写进日志 ⇒ 泄露用户内容。**必须手写** |

#### 决策 C：取消错误用 `ModelError::Cancelled { partial: Option<Message> }`（不加 `ModelResponse` 字段）

**选择**：`crates/ys-model/src/error.rs` 增加变体

```rust
    /// **用户主动取消**：模型调用在完成前被中止。
    ///
    /// `partial` 携带**已产生的部分内容**（可能为 `None`）—— 上层**应当**把它追加进
    /// 会话历史，否则屏幕上看得见的文本会与下一轮请求的上下文不一致。
    #[error("model call cancelled")]
    Cancelled { partial: Option<ys_core::Message> },
```

**理由**：

- **改动面对比**：`ModelResponse` 加字段要改 **35 处**构造点（`grep -rn "ModelResponse {"` 命中 35，且 `ModelResponse` **没有** `Default` 派生，全是字面量）；`ModelError` 加变体只改 **1 处**定义 + 1 处 match。
- `ModelError` 现状只有 `Sink` / `Provider(String)` / `Serde` 三变体，全仓 **12 处**引用且**零穷尽匹配**（全是构造 + `matches!`）⇒ 加变体的连带修改极小。
- `thiserror` 要求字段类型实现 `Debug`；`ys_core::Message` 派生 `Debug` ✓。
- **不加 `#[non_exhaustive]`**：加了会让仓内所有 `match` 都必须加 `_` 臂，噪音大于收益；仓内无发布件。

**否决的替代方案**：`ModelResponse` 加 `cancelled: bool` + `partial_blocks: Vec<ContentBlock>` ⇒ 35 处构造点全部要改，且 `ModelResponse` 语义上应表示「完整响应」。

#### 决策 D：适配器三处**非嵌套** `select!`

**核心约束**：`tokio::select!` 会**丢弃未选中分支的 future**。若 accumulator 位于那个 future 内部，它会被 drop ⇒ partial 丢失。
⇒ **accumulator 必须在 `select!` 之外声明**（栈上存活），`select!` 只 drop 借用它的闭包 future。

三处落点（均在 `adapters/model-openai-compatible/src/lib.rs`）：

| # | 阶段 | 位置 | 取消时 partial |
|---|---|---|---|
| 1 | 发送阶段 | `attempt` 内，`:524` `self.send(...)` 之前 | `None`（还没开始吐） |
| 2 | 流阶段 | `attempt` 内，`:601` `parse_sse_stream` 之前 | **可取到**（`accumulator.into_partial()`） |
| 3 | 退避阶段 | `complete` 的重试循环内，退避 `sleep` 处 | `None` |

**为什么不做「统一外层 select!」**：统一外层要求 accumulator 活在 `select!` 之外，而 accumulator **借用 sink** 且**每次重试重建** ⇒ 借用关系跨 `await` 且跨循环迭代 ⇒ 与 `select!` 的 drop 语义交织，复杂易错。

**为什么不能嵌套**：外层 `select!` 丢弃内层 future ⇒ 内层 accumulator 被 drop ⇒ partial 丢失。

**`wait_abort` 的形态**：照抄 `adapters/tools-basic/src/bash.rs:101-107` —— 100ms 轮询 `is_aborted()`，无 boundary 时 `std::future::pending::<()>().await`。`bash.rs:14` 与 `exec.rs:29-30` 的 `ABORT_POLL_INTERVAL = 100ms` 是既有先例。

#### 决策 E：`AttemptError` 加第三支

**选择**：

```rust
    /// **用户主动取消**：不重试、不上报为失败。
    Cancelled { partial: Option<ys_core::Message> },
```

`complete` 的 `match outcome` 里，`Err(AttemptError::Cancelled { partial })` ⇒ **立即 `return Err(ModelError::Cancelled { partial })`**（不走重试、不 trace 为失败）。

**理由**：取消若归入 `Retryable` ⇒ 重试循环会重放请求 ⇒ **违背用户意图**（用户就是要停下）。`AttemptError` 现状只有 `Retryable` / `Fatal` 两支（`:312-324`），必须显式开第三支才能表达「第三种结局」。

#### 决策 F：`StreamAccumulator::into_partial(self) -> Option<Message>`

**选择**：

- **只取 `self.text`**（非空则产出 `ContentBlock::Text`），**不碰 `self.tool_calls`** ⇒ 半截 tool call 一律丢弃。
- 与 `finish_with_logger`（`:1012`）**共用**「从 `text` 造 `ContentBlock::Text`」这一步（`:1025-1027`）⇒ 抽成小函数 `fn text_block(text: String) -> Option<ContentBlock>`，两处共用，避免逻辑漂移。
- **既有 `finish` 路径的字节输出完全不变**。

**理由**：

- 半截 tool call（参数 JSON 不全）写进历史会污染下一轮请求上下文，且可能触发下游解析错误 ⇒ 丢弃是安全侧。
- reasoning **不入 partial**：`lib.rs:990-993` 注释明确「Thinking 段只 emit，不写 `self.reasoning`」，`finish_with_logger` 的 `content_blocks` 也不含 reasoning ⇒ partial 与 `finish` 保持同构。

#### 决策 G：`ys-loop` 的取消分支

在 `crates/ys-loop/src/basic.rs:363-377` 的 match 里**加一支** `Err(ModelError::Cancelled { partial })`：

1. 若 `partial` 有内容 ⇒ **先** `ctx.session.append(partial)`（与 `:398-401` 同一路径）⇒ **顺序不可颠倒**（先 append 再发终局事件；否则消费端可能已认为回合结束，导致部分输出丢失）。
2. **不发** `RunFailed`。
3. **不发** `RoundUsage`（本轮无完整 usage；发假的 0-token usage 会让计费统计多出一轮 0 记录）。
4. 发 `RunFinished { stop_reason: StopReason::Cancelled, usage: total_usage.clone(), rounds }` —— 形态与 `basic.rs:317-333` 的轮边界 abort 一致。
5. `return Ok(RunResult { stop_reason: Cancelled, usage: total_usage, rounds, final_message: <partial 的文本或 None> })`。

**关于 `final_message`**：取 partial 的文本（若有）。理由：那段文本**已经进了历史**，`final_message` 应当反映它，否则下游（`-p` 模式的最终输出、TUI 结束行）看不到用户已经看到的内容。轮边界 abort 用 `final_message: None` 是对的（那里确实没有输出）—— **两处形态不同不是不一致，而是事实不同**。

#### 决策 H：工具之间补 abort 探针

**现状**：`read` / `write` / `edit` / `find` / `grep` / `ls` 六个短时工具**没有** abort 探针 ⇒ 一旦模型连发多个工具调用，要跑到外层 budget（默认 300s）才停。

**选择**：**不加轮询**（对毫秒级 IO 是纯开销）⇒ 改在 `ys-loop` 侧：工具执行处（`basic.rs:670` 附近、`:724` 附近）在**每个工具调用前后**检查 `ctx.boundary.is_some_and(|b| b.is_aborted())` ⇒ 已中止则不再发起下一个工具。

**与工具侧自查的分工**：`bash` / `exec` / `webfetch` 已在工具侧自查（`bash.rs:104`、`exec.rs:305`、`fetch.rs:409`）⇒ 两层互补、不重复：工具侧负责「正在跑的长任务尽快停」，loop 侧负责「不再发起下一个工具」。

#### 决策 I：TUI Esc 立即给可见反馈

**选择**：`crates/ys-tui-coding/src/events.rs:169-177` 的 Esc 分支：回合中按 Esc 投 `Boundary::Abort` 后，**立即**在 transcript 追加一条系统行（如「已请求取消，正在停止…」）。

**理由**：消除「Esc 是否有反应」的困惑 —— 模型调用期间 abort 要等 100ms 轮询才被感知，缺少即时反馈会让人以为按键无效。

### 数据流 / 接口变更

#### 接口变更清单

| 位置 | 变更 |
|---|---|
| `crates/ys-model/src/error.rs` | 新增 `ModelError::Cancelled { partial: Option<ys_core::Message> }` |
| `crates/ys-model/src/request.rs` | `ModelRequest` 新增 `#[serde(skip, default)] pub boundary: Option<Arc<dyn BoundarySource>>` |
| `crates/ys-model/Cargo.toml` | 新增 `ys-protocol = { path = "../ys-protocol" }` |
| `crates/ys-protocol/src/boundary.rs` | `BoundarySource` 加 `Debug` supertrait；`QueueBoundarySource` 手写 `impl Debug`（只输出 aborted 与队列长度） |
| `crates/ys-component/src/context.rs` | `RuntimeContext.boundary` → `Option<Arc<dyn BoundarySource>>`；`with_boundary` 签名相应改 |
| `crates/ys-runtime/src/agent.rs` | `AgentPorts.boundary` → `Option<Arc<dyn BoundarySource>>`；`AgentPorts::new` 签名相应改 |
| `adapters/model-openai-compatible/Cargo.toml` | 新增 `ys-protocol = { path = "../../crates/ys-protocol" }`（适配器当前**不依赖** `ys-protocol`，而 `wait_abort` 需 `BoundarySource`） |
| `adapters/model-openai-compatible/src/lib.rs` | 新增 `wait_abort`、三处 `select!`、`AttemptError::Cancelled`、`StreamAccumulator::into_partial`、`text_block` |
| `crates/ys-loop/src/summarize.rs` | `:61` 的 `ModelRequest` 是**穷尽构造点** ⇒ 补 `boundary: None` |
| `apps/coding-agent/src/app_loop.rs` | 边界源 Arc 化接线（`:998` 建 `Arc`，`:1004` 传 `clone()`） |
| `apps/coding-agent/src/main.rs` | 边界源 Arc 化接线（`:1716` `ports_with_boundary`、`:1723` `AgentPorts::new`） |
| `apps/coding-agent/src/wiring.rs` | `ports_with_boundary` 签名改（`:254`）；调用点 `:243` 适配 |
| `apps/coding-agent/src/subagent.rs` | `:427` `AgentPorts::new` 适配；`ChildBoundary` **去生命周期参数**、`parent` 改 `Option<Arc<dyn BoundarySource>>`；`ChildBoundary` 加 `Debug` |
| `crates/ys-tui-coding/src/events.rs` | Esc 分支追加系统行 |

**`Model` trait 签名不变**、`ModelEventSink` 不变、`ModelResponse` 不变、`ContentBlock` 不变（不新增 Thinking 变体）。

#### 取消时序

```
T0  用户按 Esc
T0  app_loop: boundary.take() → Some(Boundary::Abort) → QueueBoundarySource 置位
T0  TUI: transcript 追加系统行「已请求取消，正在停止…」   ← 决策 I，即时反馈
T0+≤100ms  适配器 wait_abort 轮询发现 is_aborted() → true
T0+≤100ms  select! 选中 abort 分支 → 丢弃模型 future
T0+ε       阶段2：accumulator.into_partial() → Option<Message>
T0+ε       AttemptError::Cancelled{partial} → ModelError::Cancelled{partial}
T0+ε       ys-loop: session.append(partial)     ← 先追加
T0+ε       ys-loop: 发 RunFinished{Cancelled}    ← 后收场
```

#### 事件序列对比

| 场景 | 事件序列 |
|---|---|
| 既有模型错误 | `… → RunFailed{…}` |
| **新增**：模型调用中取消 | `… → RunFinished{stop_reason: Cancelled}`（**无** `RunFailed`、**无** `RoundUsage`） |
| 既有轮边界取消 | `… → RunFinished{stop_reason: Cancelled}` |

## 涉及文件

**新增**：无新 crate、无新文件（`Arc` 用 std）。

**修改**（**完整清单** —— 签名变更的调用点共 25+ 处，含大量测试文件）：

**核心实现**：

- `crates/ys-model/Cargo.toml` — 加 `ys-protocol` 依赖
- `crates/ys-model/src/error.rs` — 加 `Cancelled { partial }` 变体
- `crates/ys-model/src/request.rs` — `ModelRequest` 加 `boundary` 字段
- `crates/ys-protocol/src/boundary.rs` — `BoundarySource` 加 `Debug` supertrait；`QueueBoundarySource` 手写 `impl Debug`（只输出 aborted 状态与队列长度，**不输出 steer 消息内容**）
- `crates/ys-component/src/context.rs` — `RuntimeContext.boundary` 改 `Option<Arc<dyn BoundarySource>>`，`with_boundary` 签名改
- `crates/ys-runtime/src/agent.rs` — `AgentPorts.boundary` 改类型；`AgentPorts::new` 签名改
- `crates/ys-runtime/src/lib.rs` — **6 处** `AgentPorts::new` 调用点适配（`:49`、`:75`、`:104`、`:129`、`:155`、`:162`）
- `crates/ys-loop/src/basic.rs` — `ctx.boundary` 使用点适配（`:137`、`:174`、`:305`、`:670`、`:724`）；Step 6 加 `Cancelled` 分支；工具之间加 abort 探针；`ModelRequest` 组装挂 boundary
- `crates/ys-loop/src/lib.rs` — **8 处** `with_boundary` 调用点适配（`:257`、`:288`、`:408`、`:570`、`:847`、`:928`、`:975`）；`DecoupledBoundarySource` 加 `Debug`（测试替身）
- `crates/ys-loop/src/compact.rs` — **已核实无 `ctx.boundary` 使用点 ⇒ 无需适配**
- `adapters/model-openai-compatible/Cargo.toml` — 加 `ys-protocol` 依赖
- `adapters/model-openai-compatible/src/lib.rs` — `wait_abort`、三处 `select!`、`AttemptError::Cancelled`、`StreamAccumulator::into_partial`、`text_block` 抽取
- `crates/ys-loop/src/summarize.rs` — `:61` 的 `ModelRequest` 是**穷尽构造点**（无 `..Default::default()`）⇒ T1 加 `boundary` 字段后必须补 `boundary: None`
- `apps/coding-agent/src/app_loop.rs` — 边界源 Arc 化接线（`:998` 建 `Arc`、`:1004` 传 `clone()`）
- `apps/coding-agent/src/main.rs` — `:1716` `ports_with_boundary` + `:1723` `AgentPorts::new`；边界源 Arc 化
- `apps/coding-agent/src/wiring.rs` — `:254` `ports_with_boundary` 签名改；`:243` 调用点适配
- `apps/coding-agent/src/subagent.rs` — `:427` `AgentPorts::new` 适配；`ChildBoundary` **去生命周期参数**、`parent` 改 `Option<Arc<dyn BoundarySource>>`（否则 `Arc<ChildBoundary>` 非 `'static`，E0597）；`ChildBoundary` 加 `Debug`
- `crates/ys-loop/src/summarize.rs` — `:61` 的 `ModelRequest` 是**穷尽构造点**（无 `..Default::default()`）⇒ T1 加 `boundary` 字段后必须补 `boundary: None`
- `crates/ys-tui-coding/src/events.rs` — Esc 系统行反馈

**测试文件**（**不得漏** —— 它们同样是签名变更的调用点）：

- `crates/ys-runtime/src/agent.rs`（内联测试）— `AgentPorts::new` 调用点适配
- `crates/ys-runtime/tests/run_turn_boundary.rs` — **6 处** `AgentPorts::new` 调用点适配（`:147`、`:194`、`:219`、`:233`、`:267`、`:313`）
- `crates/ys-runtime/tests/v0_integration.rs` — **6 处** `AgentPorts::new` 调用点适配（`:92`、`:118`、`:145`、`:171`、`:308`、`:384`）
- `apps/coding-agent/tests/e2e_skills.rs` — `AgentPorts::new` 调用点适配（`:239`）
- `apps/coding-agent/tests/e2e_tools.rs` — `AgentPorts::new` 调用点适配（`:75`）
- `adapters/tools-web/tests/tools.rs` — `StoredBoundary`（`:1872`）与 `ArmBoundary`（`:1908`）加 `Debug`（仅测试替身适配）

**调用点分布核实结论**（`grep` 全仓实测）：

| 符号 | 调用点总数 | 分布 |
|---|---|---|
| `AgentPorts::new` | **17 处** | `apps/coding-agent/src/app_loop.rs:1004`、`apps/coding-agent/src/main.rs:1723`、`apps/coding-agent/src/wiring.rs:258`、`apps/coding-agent/src/subagent.rs:427`、`apps/coding-agent/tests/e2e_skills.rs:239`、`apps/coding-agent/tests/e2e_tools.rs:75`、`crates/ys-runtime/src/agent.rs:246`、`crates/ys-runtime/src/lib.rs:49/75/104/129/155/162`、`crates/ys-runtime/tests/run_turn_boundary.rs:147/194/219/233/267/313`、`crates/ys-runtime/tests/v0_integration.rs:92/118/145/171/308/384` |
| `RuntimeContext::with_boundary` | **8 处** | `crates/ys-loop/src/lib.rs:257/288/408/570/847/928/975`、`crates/ys-loop/src/basic.rs:2405` |
| `Wiring::ports_with_boundary` | **3 处** | `apps/coding-agent/src/main.rs:1716`、`apps/coding-agent/src/wiring.rs:243`（自调用）、`:254`（定义） |
| `impl BoundarySource` | **5 处** | `crates/ys-protocol/src/boundary.rs:99`、`crates/ys-loop/src/lib.rs:758`、`apps/coding-agent/src/subagent.rs:83`、`adapters/tools-web/tests/tools.rs:1872`、`adapters/tools-web/tests/tools.rs:1908` |

**零改动声明**：`crates/ys-tool/`（`ToolContext.boundary` 保持 `Option<&'a dyn BoundarySource>`、`SubagentRunner::run` 签名不变）、`adapters/tools-basic/` 的**业务代码**（仅其测试替身用 `QueueBoundarySource`，而 `QueueBoundarySource` 由 `ys-protocol` 提供 `Debug` ⇒ 自身无需改）。

## 测试策略

### 测试合同（Phase 3 审查依据）

| # | 需求 | 验收方法 | 优先级 |
|---|---|---|---|
| N1 | 适配器在已 abort 时返回 `ModelError::Cancelled` | `cargo test -p ys-model-openai-compat <取消相关测试名>` — 期望返回 `Cancelled` | 必测 |
| N2 | partial 携带已产生文本 | 同上 — 期望 `partial` 含取消前吐出的文本 | 必测 |
| N3 | partial 不含 `ToolUse` | 同上 — 期望 `partial` 只含 `ContentBlock::Text` | 必测 |
| N4 | `ys-loop` 取消 → `RunFinished{Cancelled}` 且**不发** `RunFailed` | `cargo test -p ys-loop <取消相关测试名>` | 必测 |
| N5 | partial 追加进会话历史（先 append 后收场） | `cargo test -p ys-loop <取消相关测试名>` — 期望会话历史含该段文本 | 必测 |
| N6 | 取消后不再发起下一次模型调用 | `cargo test -p ys-loop <取消相关测试名>` — mock model 调用计数 == 1 | 必测 |
| N7 | Esc 产生系统行反馈 | `cargo test -p ys-tui-coding <Esc 相关测试名>` — 期望 transcript 末行是系统提示 | 必测 |
| N8 | 工具之间 abort → 不再发起下一个工具 | `cargo test -p ys-loop <工具探针相关测试名>` — 工具调用计数 < 计划数 | 必测 |
| N9 | 既有路径逐字节不变 | `cargo test --workspace` — 期望全绿（基线：本次改动前全绿） | 必测 |
| N10 | `ModelRequest` 序列化不含 `boundary` | `cargo test -p ys-model <序列化测试名>` — 期望 serde 快照与既有逐字节一致 | 必测 |
| N11 | `ModelRequest::clone` 保留同一 `Arc` | `cargo test -p ys-model` — 期望 `Arc::ptr_eq` | 必测 |
| N12 | 变异测试（4 处） | 见下 | 必测 |
| N13 | `BoundarySource` 的 `Debug` 实现不泄露 steer 消息内容 | `cargo test -p ys-protocol` — 期望 `Debug` 输出不含 steer 文本 | 必测 |
| N14 | 签名变更后全仓编译通过 | `cargo build --workspace` — 期望无错误（**这是 25+ 处调用点是否全部适配的最终判据**） | 必测 |

#### N1: 适配器在已 abort 时返回 `ModelError::Cancelled`

**需求**：模型调用进行中收到 abort，适配器必须返回 `ModelError::Cancelled`，而不是 `Provider` / `Serde` / `Sink`，也不是静默成功。

**验收**：构造一个已置位 abort 的 `QueueBoundarySource`（或用 `Arc` 包裹的测试替身），在 `Model::complete` 中断言返回值匹配 `ModelError::Cancelled { .. }`。

#### N2: partial 携带已产生文本

**需求**：取消发生前模型已吐出的文本，必须出现在 `partial` 中。

**验收**：mock 一个先吐若干 delta 再挂住的流，中途置位 abort，断言 `Err(ModelError::Cancelled { partial: Some(m) })` 且 `m` 的 content 含已吐文本。

#### N3: partial 不含 `ToolUse`

**需求**：partial 只含 `ContentBlock::Text`，不含 `ToolUse`（半截 tool call 必须丢弃）。

**验收**：让模型先吐一个 tool call 的部分参数再取消，断言 `partial` 的 content blocks **全部**是 `Text`，没有任何 `ToolUse`。

#### N4: `ys-loop` 取消 → `RunFinished{Cancelled}`，不发 `RunFailed`

**需求**：mock model 返回 `Err(ModelError::Cancelled{..})`，断言事件流里出现 `RunFinished{stop_reason: Cancelled}`，且**不存在** `RunFailed`。

#### N5: partial 追加进会话历史（先 append 后收场）

**需求**：取消时 partial 被追加进 `ctx.session`；且**先 append 再发终局事件**。

**验收**：断言会话历史含该段文本；并断言事件序列中 `RunFinished` 之前已发生 append（可通过 mock session 记录调用顺序，或断言历史内容在收到 `RunFinished` 时已可见）。

#### N6: 取消后不再发起下一次模型调用

**需求**：取消即终止整个 turn，不进入下一轮。

**验收**：mock model 的调用计数 == 1。

#### N7: Esc 产生系统行反馈

**需求**：回合中按 Esc，transcript 立即出现一条系统提示行。

**验收**：驱动 TUI 事件层注入 Esc，断言 transcript 末行是系统提示（而非空 / 无变化）。

#### N8: 工具之间 abort → 不再发起下一个工具

**需求**：模型计划调用 N 个工具，其中第一个之后 abort，则只发起第 1 个。

**验收**：工具调用计数 < 计划数（精确断言为 1）。

#### N9: 既有路径逐字节不变

**验收**：`cargo test --workspace` 全绿（基线：改动前全绿）。同时确认 `ModelRequest` 序列化输出、`ModelResponse` 构造点、`finish` 路径的 content blocks 全部未变。

#### N10: `ModelRequest` 序列化不含 `boundary`

**需求**：`#[serde(skip, default)]` 生效 ⇒ 既有序列化字节逐字节一致。

**验收**：对含 `boundary: Some(..)` 的 request 做 serde 序列化，断言结果与不含该字段时逐字节相同。

#### N11: `ModelRequest::clone` 保留同一 `Arc`

**需求**：clone 后指向**同一个** `BoundarySource`（取消语义正确）。

**验收**：`Arc::ptr_eq(original.boundary.as_ref().unwrap(), cloned.boundary.as_ref().unwrap())` 为真。

#### N12: 变异测试（4 处）

按 AGENTS.md「关键逻辑要做变异测试」的约定，至少覆盖 4 个注入点：

| # | 注入的 bug | 期望变红 |
|---|---|---|
| ① | 去掉适配器的 `select!`（退化为直接 await） | N1 红 |
| ② | 去掉 `ys-loop` 的 `Cancelled` 分流（落回通用 `Err` ⇒ 发 `RunFailed`） | N4 红 |
| ③ | 去掉 Esc 的系统行 | N7 红 |
| ④ | 把「先 append partial 再收场」的顺序反了 | N5 红 |

#### N13：`BoundarySource` 的 `Debug` 实现不泄露 steer 消息内容

**需求**：`QueueBoundarySource` 的 `Debug` 输出**只能**包含「是否已 abort」与「队列长度」两项结构信息，**不得**包含队列里 `Boundary::Steer(Message)` 的正文。

**验收**：`cargo test -p ys-protocol` —— 往 `QueueBoundarySource` 推一条含**可识别文本**的 `Boundary::Steer(Message)`，断言 `format!("{:?}", source)` 的输出**不含**该文本；且断言输出含 `aborted` 与队列长度。`ChildBoundary` / `DecoupledBoundarySource` / `StoredBoundary` / `ArmBoundary` 的 `Debug` 同样不得输出父队列内容。

#### N14：签名变更后全仓编译通过

**需求**：`RuntimeContext.boundary` / `AgentPorts.boundary` / `with_boundary` / `ports_with_boundary` 改型后，**全仓**（含所有测试文件）编译通过。

**验收**：`cargo build --workspace` —— 期望无错误。这是「25+ 处调用点是否全部适配」的**最终判据**：漏改任何一处都会在这里暴露。注意 `cargo build --workspace` **不含**测试目标 ⇒ 须再跑 `cargo test --workspace --no-run` 或 `cargo clippy --all-targets` 才能覆盖 `tests/` 目录下的调用点。

### 测试框架与命令

- 单元测试内联在各 crate 的 `#[cfg(test)]` 模块（沿用既有风格）。
- 适配器测试需要 mock HTTP/流：沿用 `adapters/model-openai-compatible` 既有测试设施（现有测试从 `:1350` 起，构造流式响应替身）。
- 命令：
  - `cargo build --workspace`（N14：签名变更后全仓编译）
  - `cargo test --workspace --no-run`（编译**集成测试**目标 —— `cargo build --workspace` 不含 `tests/`）
  - `cargo test -p ys-protocol`（N13：`Debug` 不泄露 steer 内容）
  - `cargo test -p ys-model`
  - `cargo test -p ys-component`
  - `cargo test -p ys-runtime`
  - `cargo test -p ys-loop`
  - `cargo test -p ys-model-openai-compat`
  - `cargo test -p ys-tui-coding`
  - `cargo test --workspace`（N9 总闸）
  - `cargo clippy --all-targets` / `cargo fmt`
- 测试隔离：临时目录名必须含进程内原子序号 + `process::id()`；进程级 env 改写必须走 `crate::test_env::env_lock()` + `EnvRestore`。

## 波及文档

| 文档 | 动作 |
|---|---|
| `docs/adr/0046-cancellable-model-call-and-abort-feedback.md` | 实施完成后从 proposed 改为 accepted（或按评审结论调整）；**另需回填「仓内调用点实际数量」的更正**（§决策 4 声称「本仓内的调用点只有 `app_loop.rs:1004` 与 `main.rs:1718`」与实际不符，详见「风险与注意事项」第 15 条） |
| `docs/design-final/` | 补充「可取消模型调用」的 as-built 形态（取消信号通道、`ModelError::Cancelled`、三处 `select!`） |
| `docs/design.md` | 若测试矩阵需要新增条目，按「接口改动须能对应 §12 测试矩阵」补一行 |
| `docs/architecture.md` | 若事件形态有变（新增 `ModelError::Cancelled` 的事件表现），在对应节补一句 |
| `docs/TODO.md` | 完成后删除对应条目 |

## 风险与注意事项

1. **`Arc<dyn BoundarySource>` 需要 `'static`** ⇒ app 层必须持有 `Arc`，不能用栈上临时值。接线点**不止一处**：`apps/coding-agent/src/app_loop.rs:998-1004`（TUI 交互模式）与 `apps/coding-agent/src/main.rs:1715-1723`（`-p` / `--json` 一次性模式）**各自**新建边界源。
2. **`RuntimeContext` 带生命周期 `'a`** ⇒ 改字段类型时所有 `ctx.boundary` 使用点都要改（`basic.rs:137`、`:174`、`:305`、`:670`、`:724`），漏一个即编译错（这是好事：编译器会指出来）。**注意**：除 `ctx.boundary` 的使用点外，**所有构造 `RuntimeContext` 的地方**（`with_boundary` 调用点 8 处）也要改 —— 签名从 `&'a dyn` 变成 `Arc<dyn>`，参数类型不兼容即编译错。
3. **`ToolContext.boundary` 类型是 `Option<&dyn BoundarySource>`** ⇒ 传 `ctx.boundary.as_deref()`（`Arc<dyn BoundarySource>` → `&dyn BoundarySource`）。
4. **`ModelRequest` 加字段会影响 `..Default::default()` 的构造点** ⇒ `Default` 派生让 `boundary` 自动为 `None`，既有构造点**不需要改**。
5. **`ModelResponse` 加字段会改 35 处** ⇒ 本方案**不走这条路**（决策 C）。评审时若见到 `ModelResponse` 变更 ⇒ 偏离设计。
6. **`select!` 的 drop 语义**：accumulator 必须声明在 `select!` **之外**；嵌套 `select!` 会丢 partial（决策 D）。这是本设计最容易实现错的地方 ⇒ N2/N3 是它的护栏。
7. **取消不重试**：`AttemptError::Cancelled` 必须独立于 `Retryable`，否则重试循环重放请求，违背用户意图（决策 E）。
8. **`RoundUsage` 不发**：取消轮没有完整 usage，发 0-token usage 会让计费统计多出一轮 0 记录。
9. **`final_message` 语义差异**：模型调用中取消 ⇒ `final_message` 为 partial 文本（内容已入历史）；轮边界取消 ⇒ `None`（确实无输出）。两者**不是**不一致。
10. **既有路径逐字节不变**：新增字段必须带 `#[serde(skip, default)]`；`text_block` 抽取不得改变 `finish_with_logger` 的 content blocks 顺序（先 `Text` 后 `ToolUse`，`:1024-1033`）。
11. **`ModelError` 不加 `#[non_exhaustive]`**：加了会让仓内所有 `match` 必须加 `_` 臂。
12. **`ContentBlock` 不加 `Thinking` 变体**：reasoning 只 emit 不进 content_blocks（`:990-993`），partial 保持同构。
13. **`ModelRequest` 派生 `Debug` ⇒ `Arc<dyn BoundarySource>` 须 `Debug` ⇒ `BoundarySource` 必须加 `Debug` supertrait**（决策 B′）。否则 `#[derive(Debug)]` **编译失败**（已用 rustc 最小复现验证：`error[E0277]: (dyn BS + 'static) doesn't implement Debug`）。**爆炸半径**：`ys-protocol` 的 trait 定义 + **5 个** `impl BoundarySource`（`ys-protocol` 的 `QueueBoundarySource`、`ys-loop` 的 `DecoupledBoundarySource`、`apps/coding-agent` 的 `ChildBoundary`、`adapters/tools-web/tests/tools.rs` 的 `StoredBoundary` 与 `ArmBoundary`）。**已核实**：`ModelRequest` 的 `Debug` 在全仓**无任何实际使用**（`grep "ModelRequest" | grep ":?"` 无命中）⇒ 备选方案（去掉 `Debug` 派生）技术上可行，但会缩减 `ys-model` 的对外 API ⇒ 采纳加 supertrait 的方案。
14. **`RuntimeContext<'a>` 的 `'a` 与 `Arc<dyn BoundarySource>`（`'static`）共存 ⇒ `ctx.boundary.as_deref()` 传给 `ToolContext<'a>` 时生命周期可能不满足**。因为 `ctx.boundary` 是 `RuntimeContext` 的一个**字段**，`ctx.boundary.as_deref()` 得到的 `&dyn BoundarySource` 借用自 `ctx` ⇒ 在 `run_turn(ctx: &mut RuntimeContext<'_>)` 里，该引用**不能**比 `&mut ctx` 活得久 ⇒ 传给要求 `&'a dyn` 的 `ToolContext` 可能报生命周期不足。**⇒ 必须实际编译验证，不得想当然**：在 `basic.rs` 里先写
    ```rust
    let boundary_ref: Option<&dyn BoundarySource> = ctx.boundary.as_deref();
    ```
    再传给 `ToolContext::new(boundary_ref, ...)`，让编译器给出**精确**报错后调整。**Phase 3 必须先跑 `cargo build -p ys-loop` 验证这一处**（先编译，再跑测试）。
15. **ADR 0046 §决策 4 声称「本仓内的调用点只有 `app_loop.rs:1004` 与 `main.rs:1718`」与实际不符** ⇒ 实为 **25+ 处**（`AgentPorts::new` 17 处、`RuntimeContext::with_boundary` 8 处、`Wiring::ports_with_boundary` 3 处），且**大量在测试文件里**（`crates/ys-runtime/tests/run_turn_boundary.rs`、`crates/ys-runtime/tests/v0_integration.rs`、`apps/coding-agent/tests/e2e_skills.rs`、`apps/coding-agent/tests/e2e_tools.rs`）。⇒ **本 plan 以核实结果为准**；ADR 0046 的该处表述需由 ADR 负责人另行更正（**本 plan 不改 ADR**）。
16. **测试文件也是调用点**：`cargo build --workspace` **不编译** `tests/` 下的集成测试 ⇒ 集成测试的漏改会在 `cargo test --workspace --no-run` / `cargo clippy --all-targets` 才暴露 ⇒ **测试文件的适配不可当作“尾巴”省掉**。
