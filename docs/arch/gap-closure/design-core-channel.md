# 核心信道 + Actor 模型 — 设计方案

> 输入：`docs/arch/gap-closure/context.md`（权威上下文）+ `docs/adr/0009-async-event-channel.md` + `docs/adr/0010-actor-model-agent-api-revision.md`
> 范围：A（`ys-channel` 契约）+ B（自转循环接口）+ C（事件信道），三者一体设计
> 日期：2026-09-12

## 0. 摘要与推荐

三个候选方案（最小复杂度 / 可扩展优先 / 性能优先）在**核心决策上完全一致**：

- 新建 `ys-channel` 契约 crate（纯数据 + 纯枚举，无 tokio 依赖）
- `Envelope { source, turn, event }`
- 入站两队列（steering / followUp）+ `QueueMode`
- 生命周期策略（消费者消失后收摊或续跑）
- steering 在**轮**边界拉、followUp 在**回合**边界拉

分歧只在**结构化程度**。**推荐：以「最小复杂度」为骨架，移植「性能优先」的两处设计。**

| 移植项 | 理由 |
|---|---|
| `EventSink` 的 try/await 双路径 | 不只是省装箱——它**保住了 `ModelEventSink` 的同步性**，即 ADR-0004 点 2「模型适配器最小 ABI 面」；async trait 跨动态库边界困难 |
| `Inbox` 的产消模型 | 骨架方案 `Inbox` 用 `push(&mut self)`，与 `run(&Inbox)` 签名矛盾；改 `&mut` 则**接线器无法在 run 期间投递 → steering 失效**。改用 **内可变 + pending 转移进会话**（§3 移植 2） |

**不采纳**：可扩展方案的 `Projection`/`SharedLog`/`InboundHub`/`TurnDriver` trait——它们服务于**已被明确推迟**的群聊/多 agent（context.md「本轮不设计」）。记录为扩展点，不在本轮建。

## 1. 三个候选方案

### 方案 A：最小复杂度

**要旨**：复用现有 `Agent`，不新增骨架类型；契约用具体类型不用 trait；`Envelope` 由信道适配器封装（`EventSink::emit` 仍收 `AgentEvent`，仅改 async）。

| 决策 | 内容 |
|---|---|
| `ys-channel` | 装 `Envelope`/`Source`（`String`）/`Inbox`（`Arc<Mutex<VecDeque>>` ×2）/`QueueMode`/`LifecyclePolicy`（enum） |
| `EventSink` | 留在 `ys-event`，入参仍 `AgentEvent`，仅改 `async`；`Envelope` 在适配器里包 |
| `turn` 来源 | 适配器从 `UserMessage` 事件推导（每回合恰好一个，`basic.rs:68`） |
| 自转接口 | `Agent::run(&mut self, inbox, policy)`，内部循环调现有 `run_turn` |
| steering | `RuntimeContext` 加 `inbox: Option<&Inbox>`；`BasicLoop` 轮边界 drain |
| 生命周期 | 落在适配器 `emit` 返回值：send 失败 → `Err`（收摊）或 `Ok`（续跑），复用「emit 失败即终止」既有语义 |
| agent 休眠 | **不做**。`run()` 在 inbox 空时返回，接线器按需重驱动 |
| 迁移 | 6 步，`run_turn` 标 deprecated |

**主动放弃**：显式「日志+游标」（用两队列近似）、agent 常驻+唤醒、ADR-0010 所有权立即外置（推到步 5）。

### 方案 B：可扩展优先

**要旨**：凡 context.md 出现「N 种形态可配」之处，一律做成 trait 而非 enum。6 个 trait + 14 个扩展点。

```rust
pub trait EventBus { async fn publish(&self, env: Envelope) -> Result<(), ChannelClosed>;
                     fn subscribe(&self) -> Box<dyn EventSubscription>;
                     fn subscribers(&self) -> usize; }
pub trait EventSubscription { async fn recv(&mut self) -> Option<Envelope>; }
pub trait InboundQueue   { async fn drain(&mut self, mode: QueueMode) -> Vec<InboundMessage>; }
pub trait TurnDriver     { async fn next_turn(&mut self, ctx: &TurnContext) -> Option<AgentInput>;
                           async fn lifecycle(&mut self, view: &LifecycleView) -> LifecycleDecision; }
pub trait LifecyclePolicy{ async fn after_turn(&mut self, view: &LifecycleView) -> LifecycleDecision; }
pub trait Projection     { fn project(&self, log: &[Envelope], viewer: &SourceId) -> Vec<Message>; }
```

- `Envelope` 带 `version`/`seq`（前向兼容投资）
- `Source { id: SourceId, kind: SourceKind, display_name }`
- `InboundHub` = `HashMap<InboundKind, Box<dyn InboundQueue>>`（加队列不改 driver）
- `LifecycleDecision { Continue, Stop, Park }`——`Park` 支持「休眠等唤醒」
- `AgentActor::run()` 骨架 + `SharedLog`/`ProjectedInboundQueue`（群聊地基）
- **契约与实现分 crate**：`ys-channel`（契约）+ `adapters/channel-tokio`（mpsc 实现）。**理由**：契约不该指定传输。**注意**：B 给出的理由是「避免 `ys-loop` 间接拖进 tokio」——**该理由不成立**（`ys-loop`/`ys-session` 自 v1 起已直接依赖 tokio，见 ADR-0012）；分 crate 的正确理由是**分层**，不是 tokio 隔离。
- 多消费者 `MpscEventBus` + `SubscribePolicy { Lossless, Lossy }`

**主动放弃**：简单性。今日无消费者时 `Projection`/`SharedLog` 会触发 `dead_code`。

### 方案 C：性能/资源优先

**要旨**：尊重「事件是一等接口」，但把热路径的分配与拷贝压到零。

| 优化 | 现状 → 目标 |
|---|---|
| `EventSink` | `async fn`（每事件 `Box::pin`）→ **`try_emit`（同步）+ `emit_blocking`（装箱）+ 自由函数 `emit()`** |
| `SourceId` | `String`（24B + 分配）→ **`u32` 驻留句柄**（`Envelope` 96B、无堆分配） |
| `Inbox` | → **日志 + 游标 + `VecDeque<Intent>`（1B/条）** |
| `ModelRequest.messages` | `session.messages().to_vec()`（每轮 O(上下文)）→ **`Arc<Vec<Message>>`**（每轮 1 次原子加） |
| delta 载荷 | `String` → **`bytes::Bytes`**（SSE 零拷贝） |
| `Usage`/`StopReason` | `clone()` → **derive `Copy`** |
| `RuntimeContext` | 多字段 `clone` → **全借用** |

**关键洞察**（本方案独有）：`try_emit` 是同步的，因此 `Forwarder`（实现**同步**的 `ModelEventSink`）可以调用它——**`ModelEventSink` 无需改 async**，模型适配器层零改动。

**Back-of-envelope**：`Envelope` 96B；流式 20–200 events/s（峰值 ~1000）；单事件约 30–80ns；**信道仅占 0.02% CPU**。真正的浪费是**每轮 `messages().to_vec()`**——单回合 5 轮 ≈ 2000 次分配 + 2MB memcpy + 1ms，比信道开销高 4 个数量级。

**主动放弃**：`SourceId` 自描述性（不可读）、`EventSink` 单方法美感、delta 载荷类型纯粹性（引入 `bytes` 依赖）。

> **注意**：上表**只是方案 C 的提案**。实际采纳的只有 `EventSink` 双路径与 `Inbox` 产消模型两处；`SourceId(u32)`/`Bytes`/`Arc<Vec<Message>>`/`Copy` **均未采纳**——故上文"`Envelope` 96B"是 C 在 `SourceId(u32)` 下的数字，**与本设计实际的 ~112 B（`Arc<str>`）不同**（见 §8）。

## 2. 对比矩阵

| 维度 | A 最小复杂度 | B 可扩展优先 | C 性能优先 |
|---|---|---|---|
| **新增概念** | 1 crate + 4 类型 | 1 crate + 1 adapter crate + 6 trait + 骨架 | 1 crate + 4 类型（含双路径 sink） |
| **新增 LOC（估）** | ~350 | ~1200 | ~450 |
| **热路径开销** | 每事件 `Box::pin` | 每事件 `Box::pin`（async trait） | **每事件 0 分配**（`try_send`） |
| **`ModelEventSink` 是否需改 async** | **是**（连带改模型适配器） | **是** | **否**（`try_emit` 同步） |
| **演进灵活性** | 中（enum 加变体即可） | **高**（14 扩展点） | 中 |
| **群聊支持** | 需另设计 | **已预留**（`Projection`/`SharedLog`） | 需另设计 |
| **多消费者** | 否（单消费者） | **是**（`EventBus` fan-out） | 否（单消费者） |
| **今日无用代码** | 无 | 有（`Projection` 等） | 无（但如果砍掉优化则部分无用） |
| **单人维护负担** | **低** | 高 | 中 |
| **与本项目哲学契合** | **高**（最小核心 + 静态组合） | 低（提前建未需要的抽象） | 中（优化正确但部分超范围） |
| **风险** | `Inbox` 两队列与「日志+游标」语义有偏差 | 过度工程；`Projection` 今天无消费者 | 部分优化与信道设计正交，混入会放大本次改动 |

## 3. 推荐：A 骨架 + C 的两处移植

### 为什么是 A 做骨架

1. **范围与决策匹配**：context.md 明确「群聊本轮不设计」——B 的 `Projection`/`SharedLog` 正在建设被推迟的东西，会带 `dead_code`。
2. **单人维护 + 静态组合**：CLAUDE.md 的「最小核心」「静态组合优先」直接指向「用 enum 不用 trait」。
3. **今日需求**：`-p`/`--json`/TUI 是**互斥模式**（一次跑一个），**单消费者足够**——B 的 `EventBus` fan-out 无当下消费者。
4. **可增量**：A 的每一步都可单独合入验证；B 的 6 个 trait 必须同时成型才有意义。

### 移植 1：`EventSink` 的 try/await 双路径（来自 C）

```rust
// ys-event/src/sink.rs
pub trait EventSink: Send {
    /// 快速路径：同步尝试投递。**不阻塞、不丢事件**（满则 sink 内部缓冲；关闭则按策略处置）。
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>;

    /// 慢速路径：异步投递。先冲掉 sink 内部积压（背压点），再发本次。
    fn emit<'a>(
        &'a mut self,
        event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>>;

    /// 回合边界告知（默认 no-op）。由 `Agent::run` 在每个回合开始时调用——见下「turn 的显式化」。
    fn begin_turn(&mut self, _turn: u32) {}
}

/// 循环内唯一出口：自由函数组合两条路径，稳态不产生任何 Future。
#[inline(always)]
pub async fn emit(sink: &mut dyn EventSink, event: AgentEvent) -> Result<(), EventError> {
    match sink.try_emit(event) {
        Ok(()) => Ok(()),
        Err(event) => sink.emit(event).await,
    }
}
```

**为什么值得破例移植**（超出「简单」的收益）：

- **保住 `ModelEventSink` 的同步性**。`Forwarder` 实现的是**同步** `ModelEventSink`，要转发到 `EventSink`。若 `EventSink::emit` 只有 async 形态，`ModelEventSink` **也被迫 async** → 改动所有模型适配器，且 ADR-0004 点 2 的「模型适配器最小 ABI 面」被破坏（**async trait 跨动态库边界困难**，而 v3 动态插件需要它）。
- 附带：稳态零装箱（性能）。

**代价**：`EventSink` 从 1 个方法变 2 个 + 1 个自由函数；调用点由 `ctx.events.emit(e)` 改为 `emit(ctx.events, e)`。

**（修订 R2）overflow 放在 sink 内，不放 `Forwarder`：**

```rust
// 接线器：ChannelSink 自带积压缓冲——长生命周期，不会被中途 drop
pub struct ChannelSink {
    tx: mpsc::Sender<Envelope>,
    overflow: VecDeque<AgentEvent>,   // 仅 try_send 撞满时使用
    source: Source,
    turn: u32,
    policy: LifecyclePolicy,          // 生命周期策略**只在这里**（见「R8」）
}
impl EventSink for ChannelSink {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.drain_overflow_best_effort();                   // 顺手冲积压（同步 try_send）
        match self.tx.try_send(self.wrap(event)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(env)) => {                // 撞满 → 缓冲，**不算失败**
                self.overflow.push_back(env.event);
                Ok(())
            }
            Err(TrySendError::Closed(_)) => match self.policy {   // 消费者消失
                StopWhenConsumerGone    => Err(event),            //  交回循环 → 终止
                ContinueWithoutConsumer => Ok(()),                //  丢弃，继续跑
            },
        }
    }
    fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<Box<dyn Future<…> + Send + 'a>> {
        Box::pin(async move {
            while let Some(ev) = self.overflow.pop_front() {  // 先冲积压（背压点在此）
                self.tx.send(self.wrap(ev)).await.map_err(|_| EventError::SendFailed)?;
            }
            self.tx.send(self.wrap(event)).await.map_err(|_| EventError::SendFailed)
        })
    }
    fn begin_turn(&mut self, turn: u32) { self.turn = turn; }
}
```

**为什么这样改**：原方案把 overflow 放在 `Forwarder` 里，而 `Forwarder` 的生命周期是**单次 `model.complete()` 调用**（核实 `basic.rs:129-140`：构造 → 调用 → 作用域结束 drop）。缓冲会随之一并丢弃，且原设计所说的"回合边界由 loop 冲掉"在 `basic.rs` 中**没有落点**——缓冲内事件（可能含终局事件）永久丢失，违背 ADR-0004 点 5。

放进 sink 后：① sink 跨越整个会话，不会中途 drop；② 积压由**每次 `emit().await` 自动冲掉**（顺序不变：先积压后本次），无需循环"记得"调用 flush；③ `try_emit` 撞满时**不再失败**，快路径语义简化为「只有消费者消失才失败」。

**`Forwarder` 因此回归极简**（同步回调里只做一次 `try_send`）：

```rust
impl ModelEventSink for Forwarder<'_> {
    fn emit(&mut self, ev: ModelEvent) -> Result<(), EventError> {
        if let Some(agent_event) = map_to_agent_event(ev) {
            let _ = self.sink.try_emit(agent_event);   // 满/关闭都由 sink 内部处置
        }
        Ok(())
    }
}
```

**turn 的显式化（修订 R4）**：原设计让 sink 从 `AgentEvent::UserMessage` **推导** turn。但本设计的轮边界 steering 注入**也**会发 `UserMessage`（语义上它确实是用户消息），推导会**误增 turn**，破坏 `--json` 的分组。改为显式——`Agent::run` 每回合开始调 `sink.begin_turn(n)`（它知道回合号）。

**`try_emit` 的 `Err` 语义（已定）**：`Err` 仅表示「消费者已消失」。
信道满时 `ChannelSink` 必须内部缓冲（`overflow`），不得返回 Err。
据此：自由函数 `emit()` 收到 Err 意味着「该走慢路径 / 消费者已走」，
而 `Forwarder`（同步回调）丢弃 Err 是正确的——消费者没了，丢弃合理。

### 移植 2：`Inbox` 的产消模型（C 的动机 + 落法经修订）

```rust
// ys-channel/src/inbox.rs
pub enum Intent { Steering, FollowUp }
pub enum QueueMode { All, OneAtATime }

/// 入站队列：**只装「尚未处理」的消息**。
///
/// 与「队列 = 日志 + 游标」的关系：pending（本结构）+ 会话历史（`Session`）
/// **合起来**才是那条日志；「消费」= 把 pending 移入会话（即游标前移）。
/// 故不存在消息重复持有——是**转移**，不是拷贝。
///
/// 内可变（`Arc<Mutex<Inner>>`）：接线器需在 agent 运行期间投递（steering），
/// 故 `push` 取 `&self`。（修订 R1）
#[derive(Clone)]
pub struct Inbox { inner: Arc<Mutex<Inner>> }

struct Inner {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    closed: bool,
}

impl Inbox {
    // —— 生产者侧（接线器；可在 agent 运行中调用）——
    pub fn push(&self, message: Message, kind: Intent);
    pub fn close(&self);

    // —— 消费者侧（agent）——
    /// 轮边界：按 `steering_mode` 取一批。**返回 owned**（取走即移出队列）。
    pub fn take_steering(&self) -> Vec<Message>;
    /// 回合边界：按 `follow_up_mode` 取一批。
    pub fn take_followup(&self) -> Vec<Message>;

    pub fn is_empty(&self) -> bool;
    pub fn is_closed(&self) -> bool;
}
```

**为什么改为内可变（修订 R1）**：原设计把 `Inbox` 定为普通结构体、`push(&mut self)`，而 `Agent::run(&mut self, inbox: &Inbox)` 只取不可变引用——**签名自相矛盾**；若改成 `&mut Inbox`，接线器就**无法在 agent 运行期间投递**，steering 直接失效（本设计的核心需求）。

`Arc<Mutex<Inner>>` 同时解决两件事：`push(&self)` 供接线器并发投递、`take_*` 取 `&self` 内可变取走。`std::sync::Mutex` 属 std，`ys-channel` **保持运行时无关**。争用只在用户输入到达时（低频），不进模型热路径；`take_*` 内部**无 await**，不持锁跨 await。

**`QueueMode` 语义（修订 R5）**：

| 模式 | 语义 |
|---|---|
| `OneAtATime`（默认） | 一次取一条 → **每条一回合** |
| `All` | 取走该队列全部 → **合并为一条 `Message`**（多个 `ContentBlock::Text` 依序），**单个回合**处理 |

原设计未定义 `All` 与"每消息一回合"的关系，导致 `All` 的"合并处理"与 `run()` 的 `for m in batch { run_turn(m) }` 互相矛盾。

**不移植**：C 的「单一 `log` + `cursor` 缓冲」实现——它与「pending 缓冲 + 历史缓冲 + 消费即转移」**物理等价**，但要求 `Session` 暴露"已处理前缀"，会改动 ADR-0003 的 `messages()` 契约，收益仅为记法统一。

**`/new` 的语义（补充定义）**：

```
/new  =  新的 Session（空历史） + 新的空 Inbox
         旧 Session 按既有策略保存（JSONL 落盘）
         旧 Inbox 里的 **pending 消息丢弃**
```

**pending 为何丢弃**：用户说「开新对话」时，尚未被处理的输入本就失去了语境——带入新会话会污染它。这与最早定的「`/new` 与 agent 无关」一致：**agent 全程不知情**，它只是下次被喂一条新的 `Inbox`。

**接线器动作**（`/new` 命令的全部职责）：换掉它持有的 `Session` + `Inbox` 两个句柄。**Agent / BasicLoop / ys-channel 均不参与**。

### 不移植：C 的 `Arc`/`Bytes`/`Copy` 优化

它们是**与信道设计正交**的独立优化（`ModelRequest.messages` 的 O(上下文) 深拷贝是真问题，但它不是「信道 + Actor 模型」的一部分）。混入会放大本次改动、稀释可评审性。

**记录为独立后续项**（见 §6）。

## 4. 采纳方案的关键接口

```rust
// ============ crates/ys-channel/（契约层：纯数据 + 枚举，依赖 ys-core + ys-event）============

// envelope.rs
/// 事件来源。`Arc<str>` 使 `clone` 只做一次原子加（避免每事件一次堆分配），
/// 同时序列化为可读字符串（`--json` 需要）。（修订 R7）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source(Arc<str>);              // v0: 单值 "agent"
impl Source { pub fn agent() -> Self { Self("agent".into()) } }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub source: Source,
    pub turn: u32,
    pub event: AgentEvent,
}

// lifecycle.rs
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecyclePolicy {
    /// 消费者消失 → 干完当前轮收摊（交互式默认）
    StopWhenConsumerGone,
    /// 消费者消失 → 继续跑（事件落盘兜底；后台长任务）
    ContinueWithoutConsumer,
}

// inbox.rs —— 见上「移植 2」
```

```rust
// ============ ys-component：RuntimeContext 增字段 ============
pub struct RuntimeContext<'a> {
    // ... 现有字段不变 ...
    /// 轮边界可查的掌舵队列（None = 行为与今日逐字节一致）
    pub inbox: Option<&'a Inbox>,
}
impl<'a> RuntimeContext<'a> {
    pub fn with_inbox(mut self, inbox: &'a Inbox) -> Self { /* ... */ }
}
```

```rust
// ============ ys-runtime：自转驱动（ADR-0010 留空的那块）============
impl Agent {
    /// 自转：从 inbox 取消息 → 跑回合 → 投事件，直到 inbox 空闲。
    /// `run_turn` 降级为内部「跑一个回合」的原语。
    ///
    /// **不收 policy**（修订 R8）——生命周期策略只由 sink 持有（它在 `try_emit`
    /// 时才知道消费者是否消失）。`run()` 遇到 `emit` 报错即终止，沿用既有语义。
    pub async fn run(&mut self, inbox: &Inbox) -> Result<RunSummary, LoopError>;

    /// 内部：跑一个回合。原 `run_turn`，保持可用（测试直接调用）。
    async fn run_one_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError>;
}

pub struct RunSummary { pub turns: u32, pub usage: Usage, pub last_stop: Option<StopReason> }
```

```rust
// ============ 接线器（apps/coding-agent/src/channel.rs，唯一持有 tokio::mpsc）============
// 完整实现见 §3「移植 1」。要点：
//  - policy 在此持有（唯一一处）
//  - overflow 在此持有（长生命周期，不随 Forwarder drop）
//  - turn 由 Agent::run 通过 begin_turn() 显式设置，**不从事件推导**
pub struct ChannelSink { tx, overflow, source, turn, policy }
impl EventSink for ChannelSink {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>;   // §3
    fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<Box<…>>;           // §3
    fn begin_turn(&mut self, turn: u32) { self.turn = turn; }
}

/// 背压可观测（修订 R6）：计数器在 sink 内，wiring 可读。
pub struct ChannelStats {
    pub backpressure_waits: u64,   // try_send 撞满次数
    pub buffered: usize,           // 当前 overflow 深度
    pub consumer_gone: bool,
}
impl ChannelSink { pub fn stats(&self) -> ChannelStats; }
```

## 5. 迁移路径

每步可独立编译 + `cargo test` 通过。

| 步 | 内容 | 触碰 | 验证 |
|---|---|---|---|
| 0 | `agent-*` → `ys-*` 改名（context Q9 方案 A） | 全仓 | `cargo test` 基线绿 |
| 1 | **`EventSink` 改 try/await 双路径** + `begin_turn`；`BasicLoop`/`Forwarder` 适配；`Noop`/`Collecting` 跟进 | `ys-event` `ys-loop` | 事件顺序不变；**`ModelEventSink` 未被触碰** |
| 2 | **建 `ys-channel` 契约**（`Envelope`/`Source`/`Inbox`/`QueueMode`/`LifecyclePolicy`）+ `ChannelSink`（含 overflow 与 stats）+ 置 `main.rs` 「先定模式→选消费者→建 agent」；实现 `--json`、`-p` 流式 | `ys-channel`(新) 接线器 | `--json` 逐事件输出；「增量拼接 == 终态」不变式；**背压时事件不丢** |
| 3 | **`RuntimeContext.inbox` + `BasicLoop` 轮边界 steering** | `ys-component` `ys-loop` | 新单测：轮边界注入后模型看到新 user 消息；**注入不改变 `turn`** |
| 4 | **`Agent::run(inbox)`**（不收 policy）；TUI/`-p` 改调 `run`；`run_turn` → 内部 `run_one_turn` | `ys-runtime` 接线器 | 「followUp 排队→结束后自动下一趟」；「消费者消失→交互式收摊」 |
| 5 | **ADR-0010 所有权收敛**（可选、独立）：session/events 移出 `Agent`；`/new` = 新 `Session` + 新空 `Inbox`（**pending 丢弃**）；`CommandContext` 不再持 `&mut Agent` | `ys-runtime` 接线器 `commands` | ~20 处测试改构造；`/new` 后旧会话文件保留 |
| 6 | 独立后续项（**不在本设计**）：`Arc<Vec<Message>>` 请求快照、`Bytes` delta、`Usage`/`StopReason: Copy`、`max_rounds`/`bash_timeout` 显式配置、事件落盘 | — | 计数分配器断言；长任务解锁 |

**顺序理由**：步 1（信道契约就绪）→ 步 2（事件出口，后台 agent 的标准出口，风险最低）→ 步 3/4（消息模型）。步 5 与信道解耦，可最后做或不做。

## 6. 扩展点（记录，不实现）

未来需要时按此回访——它们**不改本设计的结构**：

| 触发场景 | 加什么 | 落点 |
|---|---|---|
| 多消费者（如 `--json` + TUI 同时） | `EventBus` fan-out（方案 B） | 接线器；`ChannelSink` → `MpscEventBus` |
| 群聊 / 多 agent | `SharedLog` + `Projection` + `ProjectedInboundQueue` | `ys-channel` + 接线器路由器 |
| 第三、四种队列（system / interrupt） | `InboundKind` 变体 + hub map（方案 B） | `ys-channel` |
| 第三种生命周期策略 | `LifecyclePolicy` 加变体（enum + `#[non_exhaustive]` 已备） | `ys-channel` |
| agent 休眠唤醒 | `Park` 决策 + `Notify` | `ys-runtime` + 接线器 |
| 驱动策略可替换 | `TurnDriver` trait（方案 B） | `ys-loop` |

## 7. 与既有 ADR 的关系

- **ADR-0009（accepted）**：语义完全遵守。**细化一条**：`emit` 的 async 落地为 try/await 双路径，避免 `#[async_trait]` 的每事件装箱，并保住 `ModelEventSink` 同步。建议记入后续 ADR 的实现约束。
- **ADR-0010（proposed）**：本设计给出其「待设计」的 API 形状（`Agent::run`），**但把所有权收敛排到步 5**——即先做信道，后动所有权，避免一次改动过大。
- **ADR-0004 点 3**：已由 ADR-0009 修订；本设计进一步说明「热路径零装箱」的原始关切**未消失**，靠 try/await 拆分救回。
- **ADR-0004 点 2**（双事件口）：本设计**强化**它——`ModelEventSink` 保持同步正是为动态插件的最小 ABI 面。

**本设计经 [`review.md`](./review.md) 质量分析并修订**（见 §9）。review 的总体判断是「骨架选对了，但移植方案 C 的两处动了承重结构、未回检承重」——现已回检修复。

## 8. Back-of-envelope（采纳 C 的估算）

| 项 | 值 |
|---|---|
| `Envelope` 带内大小 | **~112 B**（`Source(Arc<str>)` 16B + `turn` 4B + pad 4B + `AgentEvent` ~88B）。<br>**注**：早先写作 96 B，那是 `SourceId(u32)` 下的数字；采纳 `Arc<str>` 后按 ~112 B 计。**未实测**，实现时以 `size_of::<Envelope>()` 为准。 |
| 流式事件频率 | 20–200 events/s（峰值 ~1000/s） |
| 单事件开销（稳态） | ~30–80 ns（`try_send` + 一次 move；`Source` clone 为一次原子加） |
| **信道 CPU 占比** | **~0.02%**（峰值 0.1%） |
| 容量 1024 的内存 | **~112 KiB**（容量 4096 → ~450 KiB，用于后台长任务） |
| 每轮 `messages().to_vec()` | ~2000 次分配 + 2 MB memcpy / 单回合（5 轮）——**真正的热点**，另立后续项 |

**结论**：信道**不可能是瓶颈**。任何「为省事件而合并」的生产者侧复杂度都不值得，且会破坏「增量拼接 == 终态」不变量。

## 9. 修订记录（据 `review.md`）

本设计经 [`review.md`](./review.md) 质量分析，修掉 3 个 🔴 / 5 个 🟡。对应关系：

| 编号 | 原问题 | 处置 |
|---|---|---|
| **R1** 🔴 | `Inbox` 产消并发模型自相矛盾（`push(&mut self)` vs `run(&Inbox)`），改 `&mut` 则 **steering 失效** | `Inbox` 改 `Arc<Mutex<Inner>>` 内可变；`push(&self)` / `take_*` 取 `&self`（§3 移植 2） |
| **R2** 🔴 | `Forwarder` 的 overflow 随其 drop 而丢失（核实 `basic.rs:129-140`），**违 ADR-0004 点 5** | overflow **移入 sink**（长生命周期），由每次 `emit().await` 自动冲掉（§3 移植 1） |
| **R3** 🔴 | 声称"队列 = 日志 + 游标"，实为两容器；且 `/new` 时 pending 如何处置未定义 | 澄清：pending + 会话历史**合起来**才是那条日志，消费是**转移**非拷贝；撤回"单一缓冲"读法。补定义 `/new` = 新 Session + 新空 Inbox、**pending 丢弃**（§3 移植 2） |
| **R4** 🟡 | `turn` 由 `UserMessage` 推导，会被 steering 注入**误增** | 改显式：`EventSink::begin_turn(n)`，由 `Agent::run` 调用（§3 移植 1） |
| **R5** 🟡 | `QueueMode::All` 语义与"每消息一回合"矛盾 | 定义：`All` = 合并为一条 `Message`、单回合；`OneAtATime` = 每条一回合（§3 移植 2） |
| **R6** 🟡 | 背压不可观测 | 新增 `ChannelStats { backpressure_waits, buffered, consumer_gone }`（§4） |
| **R7** 🟡 | `Envelope` 尺寸 96 B 是按 `SourceId(u32)` 算的，与实际所选 `Source(String)` 不符 | `Source` 改 `Arc<str>`（clone 仅一次原子加）；尺寸重算为 **~112 B**（§8） |
| **R8** 🟡 | `LifecyclePolicy` 指定在两处（sink + `run()`），优先级不明 | **只留在 sink**；`Agent::run(inbox)` 不收 policy（§4） |

**未在本轮处理**（review 的架构级建议 9）：完整的 metrics/tracing 体系。R6 只提供最小读数，不引入埋点框架——待有真实运维需求时再做。

**遗留待决**：R1 的三个备选（内可变 / 拆分句柄 / 入站也走 mpsc）中，本设计取了**内可变**（`Arc<Mutex<Inner>>`）。若将来入站量级上升、`Mutex` 争用成为问题，可回退到「拆分句柄」——**不影响对外接口**。
