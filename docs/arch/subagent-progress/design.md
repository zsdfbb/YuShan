# Subagent 运行状态：进程级状态板 + 推送式进度

> 产出：`/arch-design`（2026-10-25）。需求原话：
> 「优化 subagent 的状态检测，我希望主 agent 需要时可以更好地查到 subagent 做了什么、现在的状态。
> 以及后续 TUI 能够展示 subagent 是否还在工作。subagent 完成后，TUI 也展示主 agent 收到了通知、
> 主 agent 也输出一些当前的工作状态以及后续要做的事情。」
>
> 证据均带 `文件:行号`。**本文只做设计，不含业务代码。** 决策待用户确认后才写 ADR。

---

## 0. 一句话

**给每次派发一份「进程级、按句柄索引、跑着就在写」的状态记录**，由子 agent 自己的事件流喂它；
主 agent 经一条**非终局 `AgentEvent`** 看到状态迁移（不新增 `Outbound` 变体），TUI 把它压成
**状态行一段 + 两行转换提示**；完成消息补上**结构化读数**，提示词补一条**「收到结果后先报一句
现状与下一步」**。

---

## 1. 现状与四个缺口

| # | 缺口 | 现状证据 | 后果 |
|---|---|---|---|
| **G1** | **完成了才知道**：子 agent 跑着的时候没有任何可读状态 | `ChildTask::execute` 用本地 `CollectingSink`（`apps/coding-agent/src/subagent.rs:801`），只在**跑完**后读它生成摘要；`CollectingSink` 不暴露任何增量快照（`crates/ys-event/src/collecting.rs:17`） | `subagent_async` 的 `ToolResult` **立刻**返回句柄（`subagent.rs:1293`）⇒ 工具行当场闭合 ⇒ TUI 之后完全看不出它还在跑。用户原话「TUI 能展示是否还在工作」正是这一条 |
| **G2** | **产物跑完才落盘**，与既有设计**直接矛盾** | 三处 `write_transcript` 全在收尾分支（`subagent.rs:769` / `:836` / `:865`），路径也是那时才分配（`subagent.rs:356-360` 的 `static COUNTER`） | `docs/arch/subagent-rebaseline/design.md:184`（「产物路径由工具**先分配**、再交给 runner」）与 `:186-187`（「**按轮 append**，不是收尾时一次写」）**是写成要求的，代码没做**。⇒ 外闸 `drop` future 时产物全丢；也意味着**跑着的子 agent 没有文件可 tail** |
| **G3** | **句柄不唯一** | `handle = format!("sa{}", self.dispatched.load(...))`（`subagent.rs:595`，注释自认「不需要全局唯一」），而 runner **每回合新建**（`app_loop.rs:1245`）⇒ 计数器从 0 重来 | 跨回合两个 `sa1` 并存：TUI 两行 `sa1 finished` 无法区分（`crates/ys-tui-coding/src/app.rs:442`）、模型拿到两个同句柄回执（`subagent.rs:1294-1299`）。**任何按句柄索引的状态结构都会被它压垮** ⇒ 必须先修 |
| **G4** | **要求了一个不存在的检查** | 提示词原文「never claim a dispatched sub-agent is still running **without checking**」（`apps/coding-agent/src/prompt.rs:321-332`） | 模型没有检查手段：没有状态工具、也读不到子状态 ⇒ 它只能二选一：撒谎或闭嘴。这条 guideline 当前**无法遵守** |
| — | 附带：usage 无法按句柄查 | `UsageTotals` 是全局累加器，`drain()` 一次清零全部（`subagent.rs:121-168`），上卷点 `app_loop.rs:1336-1341` | 「这个 subagent 烧了多少」只有回合总量。**本轮不改**（见 §6） |

---

## 2. 三个候选

### 2.1 候选 A —— 最小：不建状态，用既有事件反推

`BasicLoop` 已经为每次派发发 `ToolCall{name:"subagent"}`（`crates/ys-loop/src/basic.rs:880` / `:970`），
TUI 已有工具行机制。可否**只靠既有事件**判断「还在跑」？

**不可行，原因是机制性的**：
- `subagent_async` 的 `ToolResult` 在**启动后立刻**返回（`subagent.rs:1293`），工具行随即闭合（
  `crates/ys-tui-coding/src/app.rs:370-382` 的回填分支）⇒ 之后没有任何事件指向它。
- 子 agent 的事件**不进父流**（ADR-0022 决策 2）⇒ 父侧事件流里**根本不存在**子 agent 的进展信号。
- 唯一还能用的信号是「本回合 `ToolCall` 里出现过 `subagent_async`」—— 只知道「派过」，不知道
  「还在跑」还是「已经跑完在等投递」。

⇒ A 只能回答「派过没有」，回答不了「还在不在跑」。**否决**。

### 2.2 候选 B —— 进程级状态板 + 推送（推荐）

镜像仓库里**已经验证过的同形结构**：`BackgroundTasks`（`adapters/tools-basic/src/background.rs:444`
起）—— 进程级注册表 `Arc<Mutex<…>>`、每回合绑落点、`sweep` / `take_completions` / `snapshot` 三段式。

- **写者**：子 agent 侧一个 `EventSink` 包装（`ProgressTap`），顺手更新状态板；
  另有 `spawn` / 收尾两处写状态迁移。
- **读者**：app_loop 经 `Notify` 唤醒后 `take_progress()` 排空，逐条翻成
  `AgentEvent::SubagentProgress` 推给出站；TUI 与 `--json` 都免费拿到。
- **不引入新概念**：与 `PendingResults`（`subagent.rs:192`）并列的第二张进程级表，同样以 `Arc`
  跨 `/resume` 重建共享（`assembly.rs:66-70` 的先例）。

### 2.3 候选 C —— 子事件进父流 + `Source::subagent(id)`

ADR-0022 决策 2 明确否决过，理由仍然成立且**今天更硬**：三个消费者**一个都不读 `source`**
（`crates/ys-tui-coding/src/app.rs:189` 的穷尽 match、`main.rs:547` 的 `is_terminal`、
`app_loop.rs:1281` 的直接转发）⇒ 子 `ModelTextDelta` 灌进来会让 `-p` 的 stdout **直接吐出子
agent 正文**、`--json` 提前收尾、TUI 的 `is_turning` 被子的 `RunFinished` 误关。

> 该 ADR 同时写了**翻案条件**：「出现『要在 UI 上看到子 agent 逐条工具调用』的真实需求时再开」
> （`docs/adr/0022-subagent-dispatch.md` 决策 2）。**本轮需求够不到这个条件**：用户要的是
> 「还在不在工作 + 它做了什么」，不是「逐条工具调用」。故 C 不翻案，改用 B。

---

## 3. 推荐方案（B 的落实形态）

### 3.1 状态机 —— 四态，**完成 ≠ 送达**

```
       spawn/call 成功
             │
             ▼
        ┌─────────┐   run_turn 返回   ┌───────────┐   轮边界被 drain   ┌───────────┐
        │ Running │ ────────────────► │ Completed │ ─────────────────► │ Delivered │
        └─────────┘                   └───────────┘                    └───────────┘
             │                              │                                │
             └──── 组装失败 / Err ──────────┴────── 已投递过 ────────────────┘
                        │
                        ▼
                   ┌────────┐
                   │ Failed │
                   └────────┘
```

**为什么必须区分 `Completed` 与 `Delivered`**：进度是**演示级**的（用户要看），
但「主 agent 收到了通知」是**事实级**的 —— 结果先进 `PendingResults`（`subagent.rs:234-256`：
已绑定落点就直接 `push(Boundary::Deferred)`，否则留表），真正进会话是在 `BasicLoop` 的
`drain_boundary`（`crates/ys-loop/src/basic.rs:102-111`）。**两者可以差一整回合**（回合结束后
才跑完的子 agent 就是这种情况，`subagent.rs:569-577` 已记录）。用户要的第三件事正是这个差值。

`Delivered` 的判定点唯一：app_loop 转发到 `AgentEvent::DeferredResult{handle}`，且该 handle
在板上 ⇒ 置 `Delivered`（`app_loop.rs:1281` 的转发分支）。**不另造事件。**

### 3.2 `SubagentBoard` —— 进程级状态板

新模块 `apps/coding-agent/src/subagent_board.rs`（与 `subagent.rs` 并列，后者已 2628 行）。

```rust
/// 进程级：一次派发一条，**跑着就在写**。
pub struct SubagentBoard {
    inner: Mutex<BoardInner>,
    /// 状态变化的一次性唤醒 —— 与 `BackgroundTasks` 的 sweeper 不同，
    /// 这里**不轮询**：有变化才推，避免「空闲也每 100ms 醒一次」。
    wake: Arc<Notify>,
}

struct BoardInner {
    entries: Vec<BoardEntry>,   // 按 handle 索引；有界（见下）
    /// 待 app_loop 取走的进度迁移，**入表顺序**（与 `PendingResults` 同形）。
    pending: VecDeque<Progress>,
}

pub struct BoardEntry {
    pub handle: String,          // 全局唯一：`sa{turn}.{n}`（见 §3.3）
    pub label: String,           // 工具入参 `description`（3-5 词，供显示）
    pub model: Option<String>,
    pub started_at: Instant,
    pub state: SubagentState,
    /// 跑着时的**最新一格**读数 —— 覆盖写，不累积（板不存历史，历史在转录文件里）。
    pub live: LiveProgress,
}

pub struct LiveProgress {
    pub round: u32,              // 最近一次 `RoundStarted`
    pub last_tool: Option<String>,       // 最近一次 `ToolCall` 的名字
    pub tool_calls: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub text_head: String,       // 最近一段可见文本首行（**≤ 120 字节**，硬截断）
}

pub enum SubagentState {
    Running,
    Completed { stop: StopReason, elapsed_ms: u64, rounds: u32 },
    Failed { reason: String },   // ≤ 200 字节，清洗控制字符
    Delivered { .. },            // 承载 Completed 的读数
}
```

方法（**全部照 `BackgroundTasks` 的形状**）：

| 方法 | 语义 |
|---|---|
| `register(handle, label, model)` | `spawn` / `run` 成功后登记，入 `Running` |
| `note_round(handle, round)` / `note_tool(handle, name)` / `note_text(handle, head)` / `note_usage(..)` | `ProgressTap` 调用；只改 `live`，**不入表**（见 §3.4 节流） |
| `flush(handle)` | 把 `live` + 当前 `state` 合成一条 `Progress` 入 `pending`；同 handle 已有未取走的条目则**原地合并** |
| `finish(handle, stop, elapsed_ms, rounds)` | 收尾写 `Completed` / `Failed` + `flush` |
| `mark_delivered(handle)` | app_loop 见到 `DeferredResult` 时调用 |
| `take_progress() -> Vec<Progress>` | app_loop 排空（`take_completions` 同形） |
| `snapshot_all() -> Vec<SubagentRow>` | 供 `CodingView` 与（可选）状态工具 |
| `notified() -> impl Future` | `Notify` 的一次性等待 |
| `prune(now)` | 清掉 `Delivered` 且超龄的条目 |

**`Notify` 的丢失风险已排除**：tokio 的 `notify_one` 在无 waiter 时会**存一个 permit**，
其后第一个 `notified()` 立即完成 ⇒ 「通知发生在 `notified()` 创建之前」不丢；多次通知合并为一次
**无害** —— drain 是**全量取走** `pending`。故 `select!` 每轮新建 `notified()` 是安全的。

**有界**：`MAX_BOARD_ENTRIES = 32`（与 `MAX_PENDING_RESULTS = 32` 同量级，`subagent.rs:48`）。
超限时淘汰**最旧的已终态**条目；全是 `Running` 时**不淘汰**（在跑的东西不能被显示层忘掉），
只记一行日志 —— 上界因此由 `DEFAULT_MAX_DISPATCHES_PER_TURN = 16`（`subagent.rs:33`）自然保证。

**不按会话分片**：与 `PendingResults` 同一理由（`subagent.rs:258-262` 已写明「本表是进程级的、
会话无关」）。`/new` / `/resume` 不清理板（在跑的子 agent 不随重建）。

**持有者与传递路径**（与 `results` 逐字同形）：`SubagentSetup` 加 `board: Arc<SubagentBoard>` 字段
（`subagent.rs:444-460` 的 `results` 旁边），`AgentFactory` 同样以 `Arc` 跨重建共享
（`assembly.rs:66-70` / `:152`），`make_runner` 拷指针。**app_loop 侧**经 `ctx.subagent` 拿
（`app_loop.rs:1227` / `:1245` 已有两条现成取值路径）。

### 3.3 前置修正：全局唯一句柄

```rust
// 现状（subagent.rs:595）
let handle = format!("sa{}", self.dispatched.load(Ordering::Relaxed));
// 改为：回合号 + 回合内序号 —— 两个计数器都已现成（`dispatched` + 本回合号）
let handle = format!("sa{turn}.{n}");
```

- 回合号来源：`make_runner` 的调用方（`app_loop.rs:1245` / `main.rs:1769`）已有 `turns`；
  加一个 `turn: u32` 入参（`SubagentSetup::make_runner`）。
- **为什么不用进程级原子计数器**：`sa17` 这种编号跨会话不可读；`sa{turn}.{n}` 让用户/模型
  一眼对上「第几回合派的第几个」，而**跨会话同号**是良性的（会话不同，板上的条目也不会同时存在
  —— 同会话内 `turns` 单调）。
- **副作用面**：`SubagentAsyncTool` 的回执文案（`subagent.rs:1294-1299`）、`DeferredResult`
  的 TUI 行（`app.rs:442`）、`PendingResults` 的淘汰日志（`subagent.rs:244`）—— 都只是把
  `sa1` 换成 `sa3.1`，无逻辑变化。现有测试里断言 `"sa1"` 的地方需同步（`app.rs:773-804` 用的是
  字面量 `"sa1"`，与生产无关，不必改）。

### 3.4 采集：`ProgressTap`（`EventSink` 包装）

**不改 `ys-event`、不引新依赖**：`EventSink` 的契约允许任意 wrapper（`crates/ys-event/src/sink.rs:11`，
只有 `&mut self` 方法、无 `Clone`/`&self` 要求），`CollectingSink` 本身就被 `&mut` 反复传
（`crates/ys-loop/src/basic.rs:507` 的 `Forwarder`、`:937`）。

```rust
// apps/coding-agent/src/subagent.rs
struct ProgressTap {
    inner: CollectingSink,               // 保留：`raw_tool_results` 仍从这里取（:816）
    board: Arc<SubagentBoard>,
    handle: String,
    /// 本 tap 生命周期内的最新读数；`Drop` 时落一次（漏一条也不让板停在旧值）。
    live: LiveProgress,
}

impl EventSink for ProgressTap {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.observe(&event);            // 同步快路径也**必须**观察 —— `ModelTextDelta` 走这里
        self.inner.try_emit(event)
    }
    fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<...> {
        Box::pin(async move { self.observe(&event); self.inner.emit(event).await })
    }
}
```

`observe` 看四个变体（**这四个确实会到达子 sink**，逐点证据见下表）：

| 事件 | 到达路径 | 更新 |
|---|---|---|
| `RoundStarted{round}` | `basic.rs:494` `emit().await` | `live.round` |
| `ToolCall{call}` | `basic.rs:880` / `:970` `emit().await` | `live.last_tool` / `tool_calls += 1` |
| `RoundUsage{usage}` | `basic.rs:562` `emit().await` | `live.input_tokens` / `output_tokens` 累加 |
| `ModelTextDelta{text}` | `basic.rs:309` **同步 `try_emit`**（`Forwarder`） | `live.text_head`（取首行、剥空白、截 120B） |

> **`ToolResult` 不带工具名**（`crates/ys-loop/src/basic.rs:941` 只发 `id` + `content`）⇒
> 「当前工具」**只能**从 `ToolCall` 取。想显示「工具结果」就得自己按 id 配对 —— **不做**：
> 板只回答「在干什么」，不回答「干出了什么」（后者去 `read` 转录文件）。

**节流（承重）**：一次 `ToolCall` 一条进度会洪泛 —— 一个跑 50 次工具的子 agent 会往 TUI 与
`--json` 推 50 条。故 `observe` **只更新 `live`，不直接入 `pending`**；入表由
**`flush()`** 完成，且同一 handle 在 `pending` 里已有未取走的条目时**原地合并**（保留旧的
`from_state`，覆盖 `live` 与 `state`）。`flush` 的两个调用点：

1. `RoundStarted` —— 每轮恰好一次，天然是节拍（一个子 agent 的轮数是个位数到几十）；
2. `finish` / `Drop` —— 收尾必落一次。

⇒ **每个子 agent 每轮至多一条进度事件**，量级与父 agent 自己的 `RoundStarted` 同阶。

### 3.5 通道：为什么是 `AgentEvent` 而不是 `Outbound` 变体

三条硬证据：

1. **回合进行中只有 `Outbound::Event`**：`Outbound::View` 只在两处推 —— 启动期模型目录到达
   （`app_loop.rs:385-392`）与**每个 Request 处理完之后**（`app_loop.rs:799-806`）。回合内 TUI
   的新信息**只能**经 `app_loop.rs:1281` 的事件转发。→ 想显示「还在跑」，必须走事件。
2. **`AgentEvent` 加变体是零成本**：`#[non_exhaustive]`（`crates/ys-event/src/event.rs:19`），
   三个消费者全有兜底 —— `-p` 的 `print_delta_text` 有 `_ => None`（`main.rs:542`）、
   `RoundClock` 末臂 `_ =>`（`round_clock.rs:300`）、debug 的 `special_event_record` 有
   `_ => return None`（`main.rs:751`）、TUI 有 `_ => {}`（`app.rs:460`）。`--json` 自动多出该行。
3. **`Outbound` 加变体会打红既有不变量**：`app_loop.rs:2216-2219` 断言「回合期间只应有事件
   （唯一的 View 在末尾）」。新 `Outbound` 变体在回合内出现 ⇒ 该断言红 ⇒ 等于修改一条
   **有意写成的不变量**。而新 `AgentEvent` 变体天然满足它。

```rust
// crates/ys-event/src/event.rs
/// 一次派发的**进度迁移**（非终局）。
///
/// 与 `DeferredResult` 的分工：那是**结果已注入会话**；本事件只是「它现在是什么样」。
/// 携带的 `live` 读数**硬截断**（`text_head` ≤ 120B），**不含子 agent 正文** ——
/// ADR-0022 决策 2 的「子事件不进父流」不变：这是 app 侧 runner 对**一次派发**的
/// 观测，不是子 agent 的事件。
SubagentProgress {
    handle: String,
    state: SubagentState,      // 复用 §3.2 的枚举（`ys-event` 已依赖 `ys-core`，
                               //  `StopReason` 可用；**不可**出现 app 侧类型）
    live: LiveProgress,        // 同上，需落在 `ys-event`（或 `ys-core`）
},
```

> **`SubagentState` / `LiveProgress` 落哪个 crate**：`ys-event` 已依赖 `ys-core`
> （且 `ys-protocol` 依赖 `ys-event`）。放 `ys-event` 与本事件同处，零新依赖；
> 放 `ys-core` 则 `ys-protocol` 也能用（但本轮不需要）。**选 `ys-event` 同文件**。

**app_loop 首次成为事件生产者（需显式记账）**：现状 app_loop 只**转发** `sink_rx` 的事件
（`app_loop.rs:1281`），全仓 `Envelope::new` 只在测试里出现（`main.rs:3215`）。本方案要它在
drain 到板上的进度时**自造** `Envelope::new(Source::agent(), turn, AgentEvent::SubagentProgress{..})`
推给 `out_tx`。

- **为什么不让子任务自己发**：`ChildTask` 是 `'static`、拿不到父的 `&mut dyn EventSink`；
  要让它发就得把 `ChannelSink` 的 `mpsc::Sender` 克隆进 `ChildTask` —— 那就把
  「出站信道」的知识渗进 `subagent.rs`，且 `--json` 会看到**乱序**事件（子任务异步发、父回合
  同步发）。**app_loop 单点发现并转发 ⇒ 顺序天然正确（与 drain 同点）**。
- **代价**：`Source::agent()` 是「代理」，语义上是「父侧观测」。**不新增
  `Source::subagent()`** —— 那正是 ADR-0022 决策 2 关掉的门，本轮不开。

**drain 的两个调用点**：

| 场景 | 位置 | 机制 |
|---|---|---|
| **回合内** | `app_loop.rs:1267` 的 `select!` 加一臂 | `_ = board.notified() => drain_progress(..)` |
| **回合外**（空闲态） | `app_loop.rs:357-363` 的 `tokio::select!` 加一臂 | 同上，然后 `build_view` + `Outbound::View`（照 `Wake::Models` 的现成形态 `:372-392`） |

> **回合外这一臂不可省**：回合结束后异步子 agent 仍在跑（`subagent.rs:569-577` 的实测记录：
> 转录在回合结束后 2~6 分钟才落盘）。没有它，TUI 会在「子 agent 还在烧 token」时显示**完全空闲**
> —— 正是用户要看的那个反例。这也是 `CodingView` 必须带 `subagents` 的原因（§3.6）。

### 3.6 TUI 三处改动

#### (1) 状态行一段 —— 唯一「还在工作」的常显位

`crates/ys-tui-coding/src/draw.rs::status_text`（`:599-638`）的段序，在中段插入（**排在
「↓ 有新内容」之后、「第 N 轮」之前**）：

```
① model / ⏳Working   ② ↓有新内容   ③ [新] subagent 段   ④ 第 N 轮   ⑤ 秒   ⑥ tokens   ⑦ 轮   ⑧ 时长
```

剥除顺序是「从尾部剥」（`:631-636`）⇒ ③ 排在 ④ 之前 = **比轮号更不容易被剥掉**。
理由：这是本轮需求的头号信息，窄终端下也必须留着。

文案（三态三形，由 `App` 的本地字段决定）：

| 情形 | 文案 |
|---|---|
| 0 条 | **整段不 push**（逐字节不变 —— 与既有「空就不画」同形：`draw.rs:314-316`、`:425-427`） |
| 1 条 `Running` | `◦ sa3.2 · read · 第3轮 · 2m10s`（工具名与轮号可缺省） |
| ≥2 条 `Running` | `◦ 3 subagents` |
| 有 `Completed` 未 `Delivered` | `↓ sa3.2 待送达` |
| 有 `Completed` 且 `Delivered` | 从段里消失（不需要提示，transcript 已有行） |

`CodingView` 新字段：`subagents: Vec<SubagentRow>`（`crates/ys-tui-coding/src/view.rs:83-131`）。

- **为何进 `CodingView` 而非 `App` 本地**：回合外也要显示（子 agent 活过回合，§3.5），而
  回合外 TUI 得不到事件 ⇒ 只能靠快照。`App` 从 `view.subagents` 读，`Outbound::View`
  **整表替换**（与 `view.tools` / `view.skills` 同形）。
- **不进 `ys-protocol`**：视图类型归产品 TUI crate（`view.rs:1-7` 的既有纪律）。
- `SubagentRow` = 板上 `snapshot_all()` 的精简投影：
  `{ handle, label, state, round, last_tool, tool_calls, elapsed, input_tokens, output_tokens }`。
  **无 `text_head`** —— 状态行不显示正文（那是转录的活）。

#### (2) 转换行 —— 只在 `Started` / `Finished` 落 transcript

`app.rs::apply_event` 加一条分支（键在 `AgentEvent::SubagentProgress`）：

| 迁移 | 落一行 | 内容 |
|---|---|---|
| `→ Running`（仅异步派发） | `System` | `● sa3.2 started: 探索构建脚本` |
| `→ Completed` | `System` | `✓ sa3.2 完成 · 7 轮 · 12 次工具 · 96s · 12.3k↑ 2.1k↓` |
| `→ Failed` | `Error` | `✗ sa3.2 失败: <reason>` |
| `Round` / `Tool` 的中间进度 | **不落行** | 只更新 `subagents` 表（否则 50 次工具 = 50 行洪泛） |

同步派发（`subagent`）**不落 `started` 行**：它本来就有一条 `Tool` 行在跑（`app.rs:370-382`），
再来一条是重复。判据：板上 `SubagentEntry.async_dispatch: bool`。

**不新增 `TranscriptLine` 变体**。`System` / `Error` 现成够用，而新变体会打红三处编译
（`draw.rs:759-768` 的 `LineClass::of`、`draw.rs:864-1002` 的 `push_non_assistant`、
`layout.rs:556-586` 的 `entry_sig`）并被四处 `_ =>` **静默**漏掉（`draw.rs:790-796` 的
`is_visible`、`draw.rs:773-779` 的 `indent`、`commands.rs:543-548` 的 `copy_text`、
`transcript.rs:142` 的 `replay_lines`）。**代价可接受**：这些转换行与 `Summary` / 命令输出
一样**不持久化**，`/resume` 回放里不出现（与既有语义一致，不是缺陷）。

#### (3) 送达行 —— 把「收到通知」说清

现状（`app.rs:427-445`）：`↓ {handle} finished: {head}`（`:442`）。**问题**：`finished` 既可能指
「跑完」也可能指「送达」，而用户要的正是区分。

改为：

```
↓ sa3.2 结果已注入会话（第 6 轮）· completed · 7 轮 · 96s
   首行摘要…
```

- 保留「只显示首行」这条纪律（`app.rs:434-441` 的注释与 `:773-804` 的测试钉住了它：
  摘要有 8 KiB 上限，整段会刷屏）。
- 轮号取自 `Envelope.turn`（drain 点在 `basic.rs:102` 的轮边界，事件已封壳）。
- **这条行此刻才落**，同时板上置 `Delivered` ⇒ 状态段里那条 `待送达` 消失。用户看到的正是
  「主 agent 收到了通知」这一瞬时。

### 3.7 模型侧：让它「查到做了什么」并「报一句现状与下一步」

#### (a) 完成消息补结构化读数

现状 `render_outcome_text`（`subagent.rs:624-642`）= 诊断 → 摘要 → `[full transcript: path]`；
**而且同步工具里另有一份等价拼装**（`subagent.rs:1209-1224`，注释自称「两条路径不漂移」——
实际是两份实现）。

改：**合成一份** `render_dispatch_result(handle, &SubagentOutcome) -> String`，两条路径都调它；
首行加一条**结构化头**：

```
[sa3.2] completed · 7 rounds · 12 tool calls · 96s · 12.3k↑ 2.1k↓
<摘要正文…>
[full transcript: /Users/…/.yushan/subagents/65935_0.md]
```

- 数据来源：`SubagentOutcome` 增三个字段 `rounds: u32` / `tool_calls: u32` / `elapsed_ms: u64`。
  `SubagentOutcome` **不是** `#[non_exhaustive]`（`crates/ys-tool/src/subagent.rs:15-30`），
  加字段是 breaking ⇒ 同步改两处 fake（`crates/ys-tool/src/subagent.rs:99`、
  `crates/ys-tool/src/context.rs:101`）与三处构造（`subagent.rs:774` / `:844` / `:873`）。
- **头在截断之后拼**，故不破 `MAX_SUMMARY_BYTES = 8 KiB`（`subagent.rs:31`、`:1004`）的语义
  —— 摘要正文仍 ≤ 8 KiB，头是定长附加（≤ ~120 字节）。**当前没有任何断言约束总长**
  （全仓该常量只有 `:31` / `:1004` 两处引用）⇒ 加头不会打红既有测试，但要在注释里写明
  「总量 = 8 KiB + 定长头」。

#### (b) 两条例 guideline（改 `prompt.rs:321-332`）

**落在 `subagent_async` 段内、且插在 `subagent` 段之前** —— 因为
`subagent_guideline_block`（`prompt.rs:1169-1179`）用「成本提醒」那句话当**段尾锚**，
`subagent_async` 段在它之前 ⇒ 段尾锚不变，`subagent_dispatch_guidelines_in_parent_prompt`
（`prompt.rs:1182`）不会红。

1. **替换 G4 那条无法遵守的句子**（原文「never claim a dispatched sub-agent is still running
   without checking」）：

   > If you need to know what a running sub-agent is doing, call `subagent_status` — it reports
   > the current state of every dispatch in this process. Do NOT call it in a loop and do NOT
   > use it to wait: the result arrives on its own at the next round boundary.

   （若用户裁决**不做**状态工具，本条改写为：`Never claim a dispatched sub-agent is still
   running: you cannot see inside it. Say what you know — it is still running — or say nothing
   about it.`）

2. **新增：收到结果后先报一句现状与下一步**（用户第三件事的直接落地）：

   > When a sub-agent's result arrives, say in one line what it found and what you are doing
   > next, then continue.

   成本 ≈ 110 字节，**进 system prompt 的固定前缀**（每轮命中缓存，非每轮重付）。

#### (c) 为什么不让注入消息自己带指令

先例 `CHILD_TASK_SUFFIX`（`subagent.rs:978-979`）证明「指令拼在**消息**里而非 system」可行，
但对**注入给父**的结果消息不合适：尾部追加指令会让「结果正文」与「系统指令」混在同一条
`User` 消息里，压缩时会被一起摘要掉（`basic.rs` 的压缩按消息切块），而 system 里的
guideline 永远在。**故放 system。**

### 3.8 增量落盘（既有设计的欠账，与「查到做了什么」直接相关）

`docs/arch/subagent-rebaseline/design.md:184` / `:186-187` 已写成要求，代码没做。三处改动：

```rust
// SubagentsDir（subagent.rs:299）
pub fn allocate(&self) -> io::Result<Artifact>     // 建目录 + 定路径（先分配！）
// Artifact { path: PathBuf, file: File }
impl Artifact { pub fn append_round(&mut self,..) } // 按轮 append
```

- **路径在 `spawn` / `run` 成功之后、`tokio::spawn` / `execute` 之前分配**，句柄与路径一起进板
  ⇒ 跑着的子 agent **有文件可 tail**（`read offset=` / `tail -f`），用户与模型都能看。
- `append` 时机：`RoundStarted` 时把**上一轮**的 assistant 文本与工具轨迹写一段。
  与 `flush()`（§3.4）同一个节拍 ⇒ **一次写入点**。
- 收尾仍写完整转录（现有 `transcript_body` 的形状不变），只是从「一次 `write`」变成
  「`append` 若干段 + 最后一段」—— **`write_transcript` 的三个调用点合并成一个收尾调用**。
- **顺带收益**：外闸真响（`drop` future）时盘上已有完整前若干轮 ⇒ ADR-0029 §3.4 的第 3 层
  兜底第一次真正成立。

### 3.9 可选（P1）：`subagent_status` 工具

```
subagent_status(handle?: string) -> 每个派发一行
  sa3.2 [running] 第4轮 · read src/agent.rs · 12 次工具 · 2m10s · 3.1k↑ 0.8k↓
  sa3.1 [delivered] completed · 7 轮 · 96s
```

- **只读 + `is_concurrency_safe() == true`**（与 `read` / `grep` 同类，`basic.rs` Step 9 的并行准入）。
- **没有 `wait` 参数**：它**不能**变成 `bash_poll`。`bash_poll` 被删的根因是
  「`MAX_WAIT_SECONDS = 20` 小于任务时长 ⇒ 逼出轮询，79% 的返回零信息」
  （`docs/arch/bg-notify/design.md` §1）。本工具**不承担完成通知**（那是推送的活），
  只在模型「确实要判断现在能不能换任务」时回答一次。
- **代价（精确记账）**：
  - 父表 12 → 13 元；**子表前缀漂移 0 字节**（追加末位，`assembly.rs:184-185` 的既有契约）；
  - 父 tools JSON 约 +500 字节（`SubagentTool::spec` 实测 504 字节，同量级）—— **进固定前缀，
    每轮命中缓存**；
  - 装配断言要改：`assembly.rs:353`（`len == 12` → 13）、`:356-358`、`:379-390`（追加段名单）、
    `:416-419`（末两位 → 末三位）、`:421-424`（末位是谁）；
  - 提示词两处：`tool_snippet`（`prompt.rs:161-192`）+ `tool_guidelines`；
  - `fixed_prompt_tokens` **自动重算**（`main.rs:1074-1077` 现算，`assembly.rs:203`），
    **不需要改常量**。
- **风险**：模型可能用它轮询。缓解 = guideline 明写禁令（§3.7(b) 第 1 条）+ 无 `wait` 参数 +
  返回紧凑（十几行以内，没有「催它再问」的悬念）。

---

## 4. 分刀与验收

| 刀 | 内容 | 依赖 | 验收（每条都要能变红） |
|---|---|---|---|
| **0** | **全局唯一句柄** `sa{turn}.{n}`（§3.3） | — | 两个回合各派一次 ⇒ 两个句柄不相等；`PendingResults` 补投两条同回合结果均送达 |
| **1** | **增量落盘 + `Artifact` 先分配**（§3.8） | 0 | 转录文件在**跑之前**就存在；跑 3 轮后文件已有前 2 轮内容；外闸 `drop` 后文件非空且路径可读；`write_transcript` 调用点从 3 处收敛到 1 处 |
| **2** | **`SubagentBoard` + `ProgressTap`**（§3.2 / §3.4） | 0 | 单测：包装 sink 喂 `RoundStarted` / `ToolCall` / `RunFinished` ⇒ `snapshot_all` 的 `round` / `last_tool` / `state` 正确；**同轮多次 `ToolCall` 只产生 1 条 `pending`**（节流）；`text_head` 超 120B 被截且不破 UTF-8；`MAX_BOARD_ENTRIES` 淘汰最旧**已终态**、不淘汰 `Running` |
| **3** | **`AgentEvent::SubagentProgress` + app_loop 两条 drain**（§3.5） | 2 | 回合内：派一个慢子 agent ⇒ 事件流里出现 ≥1 条 `SubagentProgress`，且**所有其它出站仍是 Event**（不破 `app_loop.rs:2216-2219`）；回合外：回合结束后子 agent 再推进 ⇒ 收到额外 `Outbound::View`；`mark_delivered` 在 `DeferredResult` 转发后恰好一次 |
| **4** | **TUI**：状态段 + 转换行 + 送达行（§3.6） | 3 | 0 条 ⇒ `status_text` **逐字节不变**（既有用例全绿）；1 条 `Running` ⇒ 段含句柄与轮号；≥2 ⇒ `N subagents`；`Completed` 未送达 ⇒ `待送达`；`Round`/`Tool` 进度**不落 transcript 行**（行数断言）；送达行含 `结果已注入会话` + 轮号 + 只首行（既有 `app.rs:773-804` 扩写） |
| **5** | **完成消息结构化头 + prompt 两条例 guideline**（§3.7） | 1、2 | `render_dispatch_result` 一份实现被两条路径调用（同步与异步产出的**前缀逐字节相同**）；头含 rounds / tool_calls / elapsed；摘要正文仍 ≤ 8 KiB；父提示含新两条例句、**子提示不含**（`prompt.rs:1376` 同形负断言）；段尾锚 `use it to save context, not to look busy` 仍在末尾（`prompt.rs:1169-1179` 不改） |
| **6**（P1） | **`subagent_status` 工具**（§3.9） | 2、5 | 空板 ⇒ 明确说「本进程还没有派发」；1 条 running + 1 条 delivered ⇒ 两行、状态标注正确；`handle` 过滤生效；**子表不含它**（`assembly.rs` 的子表 9 元断言仍绿） |

**变异测试的重点**（AGENTS.md 要求关键逻辑必做）：

1. 把 `flush()` 的合并改成「每次 `ToolCall` 入一条」⇒ 刀 2 的节流用例必须红；
2. 把 `mark_delivered` 挂到 `finish` 上（而不是 `DeferredResult` 转发处）⇒ 刀 3 的「回合外送达」
   用例必须红（`Completed` 与 `Delivered` 被折叠）；
3. 把状态段的 push 改成无条件 ⇒ 刀 4 的「0 条逐字节不变」必须红；
4. 把 `text_head` 的截断改成 `&s[..120]` ⇒ UTF-8 边界用例必须红。

---

## 5. 成本记账

| 项 | 量级 | 性质 |
|---|---|---|
| 新增 crate 依赖 | **0** | `ys-event` 加两类型（已有依赖不变）；`apps` 加一个模块 |
| `ys-tool` 公共 API | `SubagentOutcome` +3 字段 | **breaking**（两处 fake + 三处构造同步） |
| `AgentEvent` +1 变体 | 零消费者改动 | `#[non_exhaustive]` + 四处兜底 |
| `Outbound` 变体 | **0**（明确不加） | 避免打红 `app_loop.rs:2216-2219` |
| 每轮固定前缀 | +110 字节（guideline）；+~500 字节（**仅**若做 P1 工具） | 命中缓存，非每轮重付 |
| 新增 tokio feature | **无**（`sync` 已在 `apps/coding-agent/Cargo.toml:27`） | — |
| 每子 agent 每轮事件 | ≤ 1 条 | 与父 agent 自己的 `RoundStarted` 同阶 |
| 板上内存 | ≈ 32 × (handle + label + live ≈ 300B) ≈ 10 KB | 可忽略 |
| TUI 重绘 | 无新节拍（`POLL_INTERVAL = 50ms` 已有帧循环，`run.rs:60` / `:165`） | 事件驱动 |
| 空闲态唤醒 | `Notify`，**有变化才醒** | 不用 100ms 轮询 |

**back-of-envelope：派发 3 个并发子 agent、各跑 8 轮** ⇒ 板上 3 条 × 8 次迁移 ≈ 24 条
`SubagentProgress`，分摊在父回合的 8 轮里 ≈ 每轮 3 条。与父 agent 自己的
`RoundStarted + ModelTextDelta × N + ToolCall/Result × M` 相比**低一个数量级**。

---

## 6. 明确不做（否掉的替代）

| 否掉的 | 理由 |
|---|---|
| **子事件进父流 + `Source::subagent(id)`**（候选 C） | ADR-0022 决策 2；三个消费者都不读 `source` ⇒ 会让 `-p` 吐子正文、`--json` 提前收尾、TUI `is_turning` 误关。翻案条件（「要看子 agent 逐条工具调用」）本轮**够不到** |
| **新 `Outbound` 变体** | 会在回合内出现 ⇒ 打红 `app_loop.rs:2216-2219` 这条有意写成的不变量；而 `AgentEvent` 变体零成本 |
| **`subagent_poll` / 带 `wait` 参数的工具** | `bash_poll` 的死因（`docs/arch/bg-notify/design.md` §1：68 次调用 / 79% 零信息 / 单句柄 24 次）会一字不差地复现。推送已完成「通知」，工具只回答「现在什么样」 |
| **TUI 右侧栏（roster）** | 要改**两处**纵向切分（`draw.rs:153-158` 与 `draw.rs:188-195` 的 `chat_area`），二者不同步会让滚动与画面错位（`app.rs:547` / `events.rs:368` 消费后者）。收益（更大的 roster）本轮不需要 —— 状态行一段 + 转换行已够；**且 TODO 的 todo 侧栏也在等同一处改动**，宜合并做 |
| **新 `TranscriptLine` 变体** | 三处编译点 + 四处 `_ =>` 静默漏掉；`System` / `Error` 够用，且与既有「这些行不持久化」的语义一致 |
| **`UsageTotals` 改成按句柄** | `drain()` 的一次性清零是**故意的**（回合末上卷，`app_loop.rs:1336-1341`）。按句柄查的需求由板上的 `live.input_tokens/output_tokens` 覆盖（每轮覆盖写，不是精确账单）—— **真正的精确账单要等「成本聚合」独立成项**（`docs/TODO.md` §A3 仍是欠账） |
| **`SubagentOutcome` 加 `#[non_exhaustive]`** | ADR-0022 文本提过，代码没加；加它会让外部构造变麻烦，而本轮已在加字段 —— 一笔账，**不在本轮合并** |

---

## 7. 未澄清问题（待用户裁决）

- [ ] **Q1 —— 要不要 `subagent_status` 工具（刀 6 / P1）？**
      「主 agent 需要时可以查到现在的状态」有两条路：① 只做推送（TUI 看得见、模型等到下一轮
      自然收到结果）；② 再加一个只读状态工具（模型可主动查，但多一个工具槽、父前缀
      +~500 字节、5 处断言、并承担「模型拿它轮询」的风险）。
      **我的倾向：做**，但必须是「无 `wait` 参数 + 明写禁令」的形态 —— 因为现状 guideline
      要求「检查后才可声称还在跑」，而**没有检查手段**（G4）。
- [ ] **Q2 —— 状态展示形态**：状态行一段（推荐，零布局风险）／右键侧栏（要动两处纵向切分，
      与 TODO 的 todo 侧栏宜合并）／transcript 常驻 roster 行（要新 `TranscriptLine` 变体）。
- [ ] **Q3 —— 回合外是否也刷新 `Outbound::View`**（新增 `Wake::Subagents` 分支）？
      不做的后果：回合结束后子 agent 继续跑时，TUI 显示**完全空闲**但 token 在烧。
      **我的倾向：做** —— 这正是用户「展示是否还在工作」的最强场景。
- [ ] **Q4 —— 增量落盘（刀 1）是否本轮做**？它是 `subagent-rebaseline/design.md:184-187`
      的**既有欠账**，与「查到做了什么」直接相关（跑着就有文件可 tail），但会动
      `write_transcript` 的三处调用点。**我的倾向：做，且排在第 1 刀** —— 它是其它所有
      「看进度」方案里唯一**零新概念**的一条。
- [ ] **Q5 —— 「主 agent 输出当前工作状态与后续要做的事情」**：靠 guideline（推荐，静态成本、
      永远在）还是要更硬的机制（如完成消息尾部追加指令，代价是压缩时会被一起摘要掉）？

---

## 8. 与既有决策的关系

| 既有决策 | 本方案 |
|---|---|
| ADR-0022 决策 2（子事件不进父流） | **遵守**。进父流的是 app 侧对「一次派发」的观测，不是子 agent 的事件 |
| ADR-0022 决策 4（结果 = 摘要 + 产物路径 + `StopReason`） | **扩张**：加结构化头（rounds / tool_calls / elapsed）。摘要上限不变 |
| ADR-0029 §3.4（产物路径先分配 + 增量落盘） | **落实**（刀 1）—— 当前代码与该要求矛盾 |
| ADR-0044 决策 1/2（诊断单行有界、基于事件原文的转录） | **不变**。诊断形状、转录内容口径都不动 |
| ADR-0014（零消费者 ⇒ 不建） | 板上状态有**四个消费者**：TUI 状态段、TUI 转换行、`--json`、可选状态工具 |
| ADR-0027（wall-clock 预算） | **不变**。板不参与计时 |
| `docs/arch/bg-notify/design.md`（推送而非轮询） | **沿用同一形态**（进程级表 + 每回合绑落点 + `take_*` 排空） |
| ADR-0040（分段工具调度） | 状态工具声明 `is_concurrency_safe() == true`，走并行准入 |
