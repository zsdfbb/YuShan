# Plan 模式 — 落地设计

> 需求与已定决策见 [`context.md`](context.md)（532 行，**权威来源**）。本文只回答
> 「怎么落地」，不重述需求。两者冲突时以 `context.md` 为准，本文须修订。
>
> 切片 ①（seabox 接入）不在本文范围 —— 它是独立可交付项，见 `context.md` §切片与顺序。

## 概述

`Plan | Yolo` 两档模式，默认 `Yolo`。`Plan` 下 `write` / `edit` / `bash_bg` 在**调用期**
被门禁拒绝；模型可调 `EnterPlanMode` 自行收紧，调 `ExitPlanMode` **请求**用户确认放宽。

**本文解决的核心难题**：`ExitPlanMode` 要在回合内挂住等用户回答，而 `Request` 只在
回合边界被消费，`run_turn_once` 的 `select!` 也不读 `request_rx`。因此必须新开一条
UI → app 的信道 —— 这是本设计**唯一**的新通信路径。

**未决问题的回答**：`context.md` Q6（`ExitPlanMode` 是否设长度上限）**已结**：
不收 `summary` 参数，本工具输入 schema 为空对象 ⇒ 无长度上限可言。理由见
[§6.3](#63-q6-summary-长度上限)。

---

## 1. 候选方案与选择

按 `arch-design` skill 派出三个候选，设计哲学分别为「最小复杂度」「可扩展优先」
「不阻塞关键路径」。三者的分歧**只落在一处**：回合内问答由谁驱动。

| 维度 | B 可扩展优先 | C 不阻塞优先 | **A 最小复杂度** |
|---|---|---|---|
| 回合内问答驱动者 | `app_loop` 的 `run_turn_once` `select!` **加分支** | 工具自己 `select!` + 250ms 轮询 | 同 C（唯一可行解，见下） |
| 交互抽象 | `Interactor` trait + `PromptKind` / `PromptRequest` / `PromptAnswer` 三类型 | `PlanPrompt` trait + 一个 `Outbound` 变体 + `PlanAnswer` | **无 trait**，`Arc<PlanGate>` 具体类型 |
| 状态真值 | `&dyn PlanModeSwitch`（可写 trait） | `Arc<AtomicBool>` 走 trait | `Arc<AtomicBool>` **直接**挂 `ToolContext` |
| `Outbound` 变体 | `PromptRequest` 结构体整体 | `PlanPrompt { text, resolved }` | `Output` + 一条独立 `PlanPrompt` 分体 |
| 回合热路径增量 | **+1 分支**（多一个唤醒源） | **0** | **0** |
| 死锁风险面 | 新增分支后 `select!` 的公平性需重新论证 | 分支数不变 | 分支数不变 |
| 将来接 `ApprovalHandler` | **是**（这正是 B 的设计目标） | 需重构成 trait | 需重构成 trait |

### 选择：B 作为骨架，C 的驱动方式，A 的抽象最小化

**没有第三个候选真正胜出** —— 方案 A 的 subagent 因对话中断两次未完成（第 1 次跑了
23 分 52 秒被取消）。A 的「最小复杂度」目标在事实上被 C 覆盖了大半：C 已经证明
`PlanPrompt` trait 可以薄到只有**一个方法**，`Outbound` 只需一个变体。

**结论**：

1. **驱动方式取 C**：`app_loop` 的任何 `select!` 都不加分支。理由是唤醒源数量的代价
   是**结构性**的 —— `run_turn_once` 的 `select!` 现有三路（turn 本体 / `sink_rx` /
   预算计时器），每一路都有独立的 `if` 前置条件与公平性论证；加第四路要重新论证全部
   四条，而收益只是「多一层间接」。
2. **但取 B 的抽象形状**：`Interactor` 这个**能力名**比 `PlanPrompt` 诚实 ——
   `ExitPlanMode` 要的不是「plan 提示」，是「问用户一句」。将来 `ApprovalHandler`
   接进 TUI 时不该再造第二个 trait。
3. **取 A 的删减**：`PromptKind` / `PromptRequest` / `PromptAnswer` 三个协议类型
   **不要**。只有一个提问者、只有一个问题、只有一种答案 ⇒ 一个结构体足够
   （`PlanPromptSpec` 内联在 `Outbound::PlanPrompt` 里）。

**明确拒绝的**：

- `&dyn PlanModeSwitch`（B）：需要 `set_plan` 的可写 trait，而写者只有两处
  （`EnterPlanMode` 工具、`app_loop` 的命令分支），且两处都已持有 `Arc`。多一条
  trait 只换来「`ToolContext` 的挂载点同形性」—— 但它本来就已经破了（见 §3.1）。
- 复用 `Outbound::Output` + 约定前缀（A 的一个变体）：UI 需要**结构化的
  `resolved` 信号**才能清 `pending_plan_prompt`，字符串前缀要 UI 侧解析
  —— 把协议知识塞进文案是既有的反面教材（`SessionList` 的空列表语义就直接做了变体）。

---

## 2. 模块划分与职责

```
crates/ys-core/src/plan.rs           PlanMode 枚举（纯数据，零 tokio）
       ↓
crates/ys-tool/src/plan.rs           PlanModeState / PlanPrompt 两个 trait + deny_if_plan()
       ↓
crates/ys-tool/src/context.rs        ToolContext 第五、六挂载点
       ↓
crates/ys-loop/src/basic.rs          两处 ToolContext 构造串上挂载点（串并行同形）
       ↓
apps/coding-agent/src/plan.rs        PlanModeCell（Arc<AtomicBool> 的 PlanModeState 实现）
apps/coding-agent/src/plan_prompt.rs PlanPromptImpl（Outbound + 应答信道 + 模式真值）
apps/coding-agent/src/plan_tools.rs  EnterPlanMode / ExitPlanMode 两个 Tool
       ↓
crates/ys-protocol/src/request.rs    Request::SetPlanMode / Outbound::PlanPrompt
       ↓
crates/ys-tui-coding/                submit 前置分支、status_text、COMMANDS
```

**为什么 `PlanMode` 进 `ys-core` 而不是 `ys-protocol`**：`Request::SetPlanMode` 的载荷
需要它，而 `ys-protocol` 依赖 `ys-core`（既有：`EffortLevel` / `Message` 都在这条路上）。
放进 `ys-protocol` 会让 `ys-tool` 反向依赖协议层 —— 违反依赖方向。

**为什么两个工具放 `apps/` 而不是 `crates/ys-tool`**：它们需要 `EXIT_PLAN_WAIT` 这个
**产品策略常数**。`ys-tool` 是接口层，不该有「等 180 秒」这种取值。`tools-basic` 也不
合适 —— 它们**不碰文件系统**，只读 `ToolContext` 的挂载点。

**为什么 `PlanModeCell` 与 `PlanPromptImpl` 分成两个文件**：它们的消费者不同
（前者被门禁读、被命令写；后者只在 `ExitPlanMode` 里被 `await`），且后者的
`Mutex<Receiver>` 有锁纪律要单独写清。

---

## 3. 关键数据流

### 3.1 状态真值：一处写、多处读

```
                          ┌──────────────────────────────┐
                          │  PlanModeCell                │
                          │  Arc<AtomicBool> (Relaxed)   │
                          └──────────────────────────────┘
                             ▲          ▲           │
              写             │          │           │ 读（Relaxed load，~1ns）
        ┌────────────────────┘          │           ├──────────────┐
        │                               │           ▼              ▼
  app_loop:                       EnterPlanMode  deny_if_plan()  build_view()
  Request::SetPlanMode            Tool::call()    write/edit/      → CodingView.plan
  （/plan, /yolo）                                bash_bg / Exit   → status_text "PLAN"
        │                                                          → 系统提示门控
        └── 也写 ──→ PlanPromptImpl::ask_exit()（用户答 yes 时）
```

`Relaxed` 足够：这个原子**只携带一个 bit**，不与任何其他内存建立 happens-before 关系
—— 它保护的是「写文件这个动作要不要发生」，而 `tokio::fs` 的路径本身已由 channel 与
`await` 建立了完整顺序。用 `Acquire` / `SeqCst` 只增加 x86 上的无谓 `mfence`。

### 3.2 门禁：`call()` 的第一行

```
Tool::call(input, ctx)
  ├── deny_if_plan(ctx.plan_mode, "<tool>")?     ← 新增，第一行
  └── …既有逻辑（逐字节不变）
```

`deny_if_plan` 的语义：`None` ⇒ 放行（**挂载点未挂 = Yolo**，与既有四个挂载点的
`None` 约定同规）；`Some(s)` ⇒ `s.is_plan()` 为真时返回 `Err(PermissionDenied)`。

**工具表不变** —— 这是 `toolset.rs:68-73`「工具恒注册」那条禁令的直接后果：工具面一旦会变，模型会把
「工具消失」误读成「我记错了」。降级一律发生在**调用期**（与 `websearch` 未配 key
时同规）。

**门禁立即生效**：`ToolContext` 每次调用新建（`basic.rs:1337` / `:1409`），因此
`EnterPlanMode` 在本轮写入的状态，**同一个回合的后续每次调用**都能读到。状态栏则要等
回合末的 `build_view` —— 那条滞后是 `context.md` §状态栏滞后到回合末 已接受的。

### 3.3 第四通道：`ExitPlanMode` 的等待

```
模型                 ExitPlanMode::call           app_loop              TUI
 │                        │                          │                   │
 │ tool_call              │                          │                   │
 ├───────────────────────>│                          │                   │
 │                        │ out_tx.send(             │                   │
 │                        │   PlanPrompt{resolved:   │                   │
 │                        │     false})  ────────────┼──────────────────>│ 推一行
 │                        │                          │                   │ 置 pending
 │                        │ loop {                   │                   │
 │                        │   select! {              │                   │
 │                        │     ans = answer_rx      │                   │
 │                        │     _ = sleep(250ms)     │                   │
 │                        │   }                      │                   │
 │                        │   if boundary.is_aborted │                   │
 │                        │      → "未切"            │                   │
 │                        │ }                        │                   │
 │                        │                          │   submit("yes")   │
 │                        │<─── PlanAnswer{switched: true} ───────────────┤
 │                        │                          │                   │
 │                        │ 写状态（若 switched）     │                   │
 │                        │ out_tx.send(             │                   │
 │                        │   PlanPrompt{resolved:   │                   │
 │                        │     true}) ──────────────┼──────────────────>│ 清 pending
 │ tool_result            │                          │                   │
 │<───────────────────────┤                          │                   │
```

**只有 `ExitPlanMode` 会进这条通道**。它 `is_concurrency_safe() == false`（默认值）
⇒ 分段时被切成 `Segment::Serial`（`basic.rs:437` / `:442`）⇒ 一次只有一个，且**不与其他工具并发**
—— 等待不会扩散到并行批。这是「接受有限阻塞」这句话的编译期保证，不是约定。

**上层已经有超时**：`basic.rs:459` 的 `tokio::time::timeout(budget, tool.call(..))`，
`budget` 由 `tool_budget()`（`:113-116`）算：`tool.timeout() → limits.bash_timeout → 300s`。
故本设计**不新建超时机制**，只实现 `Tool::timeout()` 返回 `EXIT_PLAN_WAIT = 180s`。

**Esc 穿透**：`Boundary::Abort` 只置标记、不 drop future（ADR-0013），故工具内部必须
**主动轮询** `boundary.is_aborted()`。250ms 是取值：
- 太快（16ms）⇒ 与帧率同量级，无谓唤醒；
- 太慢（1s）⇒ 用户按 Esc 后要等接近 1 秒才见到反应。

4 次/秒的 `AtomicBool::load` + `tokio::time::sleep` 的 CPU 成本 < 20µs/5s，相对一次
模型调用（秒级）可忽略。

**为什么状态切换在工具侧而不是 UI 侧**：单一写者。若 UI 收到 `yes` 就自己改本地镜像、
再由 app 侧读镜像改真值，就有两条写路径，必然漂移（`/thinking` 那条正是先例：
`LocalAction::Thinking` + 补发 `Request::SetThinking`，但那条**不需要即时真值**）。

### 3.4 `-p` / `--json` 的降级

这两条路径**不经过 `ys_tui_coding`**（`main.rs:2135` / `:2230` 直接跑
`run_single_turn` + `consume_events`），`Outbound` 在那边根本不存在 —— 它们只消费
`Envelope` 流。

故 `ExitPlanMode` 在这两条路径上必须**立刻返回**而不是挂住等待：

- `PlanPrompt` 挂载点为 `None` ⇒ `call()` 立即返回
  `Err(ToolError::Execution("no user to ask: this runtime has no interactive UI"))`；
- 模型拿到明确错误，不会重试（错误文案里带「按当前模式继续」）。

**这比「用一个立即返回 `Cancelled` 的 fake prompt」更好**：后者的错误结果是
`Ok(ToolResult)`，模型会以为「用户拒绝了」；而事实是**没有人存在**。两者对模型的
后续动作指引不同。

`EnterPlanMode` 在 `-p` 下仍然工作（自己改状态、立即生效）—— 它不需要用户。

---

## 4. 核心接口

### 4.1 `crates/ys-core/src/plan.rs`（新建）

```rust
/// 会话级模式。两档，默认 [`PlanMode::Yolo`]（= 本设计之前的生产行为）。
///
/// **不是安全边界**：`Plan` 下派出的 `General` 子 agent 仍可写（见 §8）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanMode {
    /// 只读：`write` / `edit` / `bash_bg` 在调用期被拒。
    Plan,
    /// 生产行为的名字（`main.rs:1112` 注入 `AutoApprove`、`bash` 无边界）。
    #[default]
    Yolo,
}

impl PlanMode {
    pub fn is_plan(self) -> bool { matches!(self, Self::Plan) }
}
```

`ys-core/src/lib.rs` 加两行：`mod plan;` + `pub use plan::*;`（照 `mod effort;` 的形状，
`:3` / `:12`）。**零 tokio**。

### 4.2 `crates/ys-tool/src/plan.rs`（新建）

```rust
/// 模式真值的**只读**视图。写者不经本 trait。
///
/// `&self` + 内部原子 ⇒ 与既有四个只读挂载点同形；实现方负责同步。
pub trait PlanModeState: Send + Sync {
    fn is_plan(&self) -> bool;
}

/// 门禁：`None` 放行（未挂载 = Yolo），`Some` 且处于 plan 时拒绝。
///
/// 文案第三句是**契约**（`context.md` 决策 4）：不点明「不要重试」模型会反复调。
pub fn deny_if_plan(mode: Option<&dyn PlanModeState>, tool: &str) -> Result<(), ToolError> {
    let Some(mode) = mode else { return Ok(()) };
    if !mode.is_plan() { return Ok(()); }
    Err(ToolError::PermissionDenied(format!(
        "{tool} is unavailable in plan mode (read-only). \
         Do not retry: either work read-only, or call ExitPlanMode to ask the user \
         to switch to Yolo mode."
    )))
}

/// 用户对 [`PlanPrompt::ask_exit`] 的答复。
pub struct PlanAnswer {
    /// 用户是否明确同意退出。
    pub switched: bool,
}

/// 回合内问用户的能力 —— **唯一 async 的 `ToolContext` 挂载点**。
///
/// 实现方**必须**保证 `await` 在 [`Tool::timeout`] 内返回：UI 消失、超时、被取消
/// 都不得无限挂住工具调用。
#[async_trait]
pub trait PlanPrompt: Send + Sync {
    /// 推固定文案 + 等答复。**不改模式真值** —— 那归调用方（单一写者）。
    async fn ask_exit(&self) -> PlanAnswer;
}
```

**`ask_exit` 不改状态**：与 B 的 `Interactor` 不同，这里让工具自己写。理由是
「同一动作只有一个写者」比「封装得更彻底」重要 —— 若 trait 实现也写状态，`/yolo`
命令是第二个写者，两者会在 `PlanAnswer` 与 `Request::SetPlanMode` 竞态时打架。

### 4.3 `ToolContext` 第五、六挂载点

```rust
pub struct ToolContext<'a> {
    // …既有：boundary / cwd / workspace_root / summarizer / subagent / tool_budget
    /// 第五个：模式真值。`None` = Yolo（门禁放行，行为逐字节不变）。
    pub plan_mode: Option<&'a dyn PlanModeState>,
    /// 第六个：问用户。`None` = 无交互 UI（`-p` / `--json`）⇒ 工具立即报错。
    pub plan_prompt: Option<&'a dyn PlanPrompt>,
}
```

两个都是 `&'a` 借用 ⇒ **不破坏既有同形性**（有 `Arc` 的那一侧在 app 侧，每次调用
`as_deref()` 借出去）。`ToolContext::new` 里填 `None`（既有调用点零改动）+
`with_plan_mode` / `with_plan_prompt`（照 `with_summarizer` 的形状）。

### 4.4 两个工具（`apps/coding-agent/src/plan_tools.rs`，新建）

```rust
/// 等用户作答的上界。取值理由：读一份长计划 + 想 + 打 `yes` 的 p99 远小于 180s；
/// 300s（`limits.bash_timeout` 默认）太久 —— 用户离开时回合会白等 5 分钟；
/// 60s 太短 —— 读长计划的人常常超。
pub const EXIT_PLAN_WAIT: Duration = Duration::from_secs(180);

impl Tool for EnterPlanMode {
    fn spec(&self) -> ToolSpec { /* 空 properties 对象 */ }
    fn timeout(&self) -> Option<Duration> { None }   // 不等待
    async fn call(&self, _in, ctx) -> Result<ToolResult, ToolError> {
        let Some(mode) = ctx.plan_mode else { return Err(/* 明确错误 */) };
        // 进入是收紧 ⇒ 无需用户确认；幂等（已在 Plan 也返回成功）
        mode.enter();
        Ok(ok_result("Entered plan mode: write/edit/bash_bg are now denied. \
                      Read-only work continues; call ExitPlanMode to request leaving."))
    }
}

impl Tool for ExitPlanMode {
    fn timeout(&self) -> Option<Duration> { Some(EXIT_PLAN_WAIT) }
    // is_concurrency_safe() 保持默认 false ⇒ Segment::Serial ⇒ 等待不扩散
    async fn call(&self, _in, ctx) -> Result<ToolResult, ToolError> {
        let (Some(mode), Some(prompt)) = (ctx.plan_mode, ctx.plan_prompt) else { /* 明确错误 */ };
        let answer = prompt.ask_exit().await;
        if answer.switched { mode.exit(); }
        Ok(ok_result(render_exit_result(answer.switched, mode.is_plan())))
    }
}
```

`render_exit_result` 的三句话逐字是契约（`context.md:260-264`），用 `#[test]` 钉住：

```text
switched: yes | no
mode now: Yolo | Plan
-> 按当前模式工作（Yolo：可以写；Plan：继续只读、不要再调本工具）
```

`PlanModeState` 需要 `enter` / `exit` 才算完整 —— 但它们**只被 app 侧调用**，
故定义在 `apps/coding-agent/src/plan.rs` 的 `PlanModeCell` 上，不进 `ys-tool` 的 trait：

```rust
/// 模式真值。`Arc<AtomicBool>` ⇒ 一份挂 `ToolContext`、一份留 `app_loop`。
pub struct PlanModeCell(Arc<AtomicBool>);

impl PlanModeCell {
    pub fn new(plan: bool) -> Self { … }
    pub fn enter(&self) { self.0.store(true, Relaxed) }   // 幂等
    pub fn exit(&self)  { self.0.store(false, Relaxed) }
    pub fn is_plan(&self) -> bool { self.0.load(Relaxed) }
}
impl PlanModeState for PlanModeCell { fn is_plan(&self) -> bool { … } }
```

### 4.5 第四通道的两个端（`crates/ys-protocol` + `apps/coding-agent/src/plan_prompt.rs`）

```rust
// ys-protocol/src/request.rs
pub enum Outbound<V> {
    // …既有变体
    /// 展示固定文案并等待用户作答（[`Request::SetPlanMode`] 之外的第四通道）。
    ///
    /// `resolved: false` = 提问；`resolved: true` = **收回**（工具已离开等待态）。
    /// 用同一个变体的两个状态而非两个变体：UI 只需清一个 `pending_plan_prompt` 位，
    /// 两个变体会让「忘了处理其中一个」成为可能。
    PlanPrompt { text: String, resolved: bool },
}

pub enum Request {
    // …既有变体
    /// 设置模式。`/plan` / `/yolo` 都经这里（`SetThinking` 同形）。
    SetPlanMode { plan: PlanMode },
}
```

```rust
// apps/coding-agent/src/plan_prompt.rs
pub struct PlanPromptImpl {
    out_tx: mpsc::Sender<Outbound<CodingView>>,
    /// **锁纪律**：`tokio::Mutex`，只在 `ask_exit` 的整个等待期间持有。
    /// 因为 `ExitPlanMode` 落在 `Segment::Serial`（`is_concurrency_safe() == false`），
    /// 结构上不可能有两个 `ask_exit` 并发 ⇒ 锁永不争用。
    answers: tokio::sync::Mutex<mpsc::Receiver<PlanAnswer>>,
}

impl PlanPrompt for PlanPromptImpl {
    async fn ask_exit(&self) -> PlanAnswer {
        // 1. 推提问（`resolved: false`）。失败 = UI 没了 ⇒ 立刻返回「未切」。
        // 2. 持锁 + 循环：
        //      select! {
        //        Some(a) = rx.recv() => 收集答案,
        //        _ = sleep(250ms)   => 看 boundary.is_aborted(),
        //      }
        //    abort ⇒ 返回「未切」（Esc 穿透）。
        //    **不自己计时**：超时归外层 `basic.rs:459` 的 `timeout`。
        // 3. 推收回（`resolved: true`）—— 成功与超时/abort **都要发**。
    }
}
```

**第 3 步无条件发**是「UI 不会挂着」的唯一保证：工具超时后外层 `timeout` 让 future
被 drop，`ask_exit` 的收尾代码**可能不执行**（drop 不跑异步代码）⇒ 收回信号必须在
可能返回的每条 `Ok` 路径上发；超时那条由 `app_loop` 在回合末的 `build_view` 兜底
（`Outbound::View` 的 `plan: bool` 让 UI 发现状态没变而自清）。这是**已知的窄窗口**，
记在 §10。

---

## 5. 装配与契约

### 5.1 工具表：父表 12 → 14，子表不变

| 表 | 组成 | 变动 |
|---|---|---|
| 子表（`toolset::build_tools`） | 基础 9 元（`EXPECTED_ORDER`） | **不动** |
| 父表（`assembly.rs:141`） | 9 + `bash_bg` + `subagent` + `subagent_async`（+2） | **12 → 14** |

新工具**追加在 `subagent_async` 之后**（`assembly.rs:200-202` 之后）。顺序是契约：
子表 JSON 仍是父表 JSON 的字节前缀 ⇒ 子 agent 命中父的**前缀缓存**。

**必须同步改的三处测试真值**（漏一处就是静默漂移）：

| 位置 | 改什么 |
|---|---|
| `assembly.rs:313-317` | `chain([...])` 追加 `"EnterPlanMode"` / `"ExitPlanMode"` |
| `toolset.rs:646-660` | 父表 `12` → `14`，追加段断言 +两项 |
| `toolset.rs:714` | `fp_parent` 字面值（`0x76e0bbd866bc74b8`） |

**一次性成本**：父表 14 元 ⇒ 所有既存会话的父 agent 前缀缓存**全部 miss 一次**。
`fp_tools`（只覆盖 9 元）不变。这是不可避免的 —— 任何工具表变动都如此，与
本设计无关。注释里写清，不要让它下次被误当成 bug。

### 5.2 两个工具**都**进父表，子表都不进

`child_tools`（`subagent.rs:1082`）从 `build_tools` 构造，**不含**这两个工具 ⇒
子 agent 无法 `EnterPlanMode` / `ExitPlanMode`。这是 §8「Plan 不传染」的实现方式：
子 agent 没有模式工具，也就不会去改一个它读不到的状态。

### 5.3 系统提示的能力边界声明

`prompt.rs:224-228` 的 `capability` 那句现在写着 `no plan mode`，**会变成谎话**。
必须改。但它有两个分支（`has_subagent` 与否），且 `e2e_skills.rs:90` **逐字拷贝**了
不带 subagent 的那一支。

改法：把「无 plan mode」从句子里删掉（**不**加「有 plan mode」的新句子）：

```rust
let capability = if has_subagent {
    "This runtime CAN delegate tasks to subagents via the `subagent` tool."
} else {
    "This runtime has no subagents."
};
```

**为什么不宣称「有 plan mode」**：模式是**会话级**的，而系统提示在会话内不变
（前缀缓存）。若提示里写「你在 plan 模式」，用户 `/yolo` 后提示就是错的，且
改提示会击穿整个缓存。故模式**不进系统提示** —— `EnterPlanMode` / `ExitPlanMode`
的 `ToolSpec.description` 自带说明，那是随工具表走的、本来就要付出缓存代价的位置。

连带改的真值断言：

| 位置 | 改什么 |
|---|---|
| `prompt.rs:1310` | `contains("no plan mode")` 那半句删掉（保留 `"no subagents"`） |
| `e2e_skills.rs:90` | 逐字拷贝里同样删掉 `and no plan mode` |

### 5.4 命令表：17 → 19

`COMMANDS: [CommandSpec; 17]`（`commands.rs:53`）加两条。解析（`commands.rs:265` 的
`match name`）：

```rust
"plan" => Action::Request(Request::SetPlanMode { plan: PlanMode::Plan }),
"yolo" => Action::Request(Request::SetPlanMode { plan: PlanMode::Yolo }),
```

**不做 `LocalAction` 变体**：模式真值在 app 侧（`PlanModeCell`），TUI 侧只有镜像
（`CodingView.plan`）。走 `LocalAction` 会让 TUI 先改镜像再发 `Request` ⇒ 两条真值
必然漂移。`/plan` 没有「本地即时生效」的需求（状态栏可以等半个 RTT）。

`/plan` **幂等**（重复调用无副作用、不报错）；`/yolo` 是**设成**而非 toggle
（`context.md` Q9 已结）。

### 5.5 `Request::SetPlanMode` 的处理

`app_loop.rs` 的 `match req`（`:467` 起）加一臂，形状照 `Request::SetThinking`
（`:565`）：

```rust
Request::SetPlanMode { plan } => {
    if plan.is_plan() { plan_mode.enter() } else { plan_mode.exit() }
    lines.push(if plan.is_plan() {
        "已进入 plan 模式：只读（write/edit/bash_bg 被拒），/yolo 退出".to_string()
    } else {
        "已退出 plan 模式".to_string()
    });
}
```

**与 `SetThinking` 的关键差别：这里要 `lines.push`**。`SetThinking` 静默是因为
它是显示偏好；模式是**安全属性**，静默切换不可接受。

### 5.6 `/clear` 与 `/new` 不改模式，但 `/clear` 要提示

`PlanModeCell` 不被这两个请求触碰（进程级状态，照 `thinking_visible`
`app_loop.rs:341-345` 与 `theme` `:347-359` 的先例）。但 `/clear` 成功路径的
那一行（`:748` 附近）在 `is_plan()` 为真时追加半句：

```text
上下文已清空（旧会话保留在 …）；plan 模式仍生效，/yolo 退出
```

代价：一次 `Relaxed` load + 一次字符串拼接，只在 `/clear` 时发生。

### 5.7 状态栏：`PLAN` 紧跟 model

`draw.rs:749` 的 `status_text` 拼 `segs: Vec<String>`，尾部逐段剥（`:803-810`）。
插入位置：`segs.insert(1, "PLAN")` —— **先于** `↓ 有新内容`。

理由比「↓ 有新内容」更强：模式是**安全属性**。窄终端下丢「tokens 多少」无所谓，
丢「我在只读模式」很严重。

文案全大写 `PLAN`：状态栏宽度紧张，且与 `⏳ Working…` 的视觉层级区分开。

### 5.8 视图：`CodingView.plan`

`CodingView`（`crates/ys-tui-coding/src/view.rs:83`）加 `pub plan: bool`，由
`build_view` 填。

**但 `build_view` 的签名不能加参数** —— `view.rs:39-40` 的注释明确写着
「形参已达 8 个。**不再加参数**：若将来还有『每请求都要刷新的单个偏好』，应连同
`stats` / `session_started` / `theme` 一起收进一个 `ViewInputs` 结构体」。

故按那条注释的两条路走**后者**：`stats` / `session_started` / `theme` / `plan` 一起
收进 `ViewInputs`。这是**本设计唯一的结构性重构**，13 个调用点（`app_loop.rs` 3 处
+ `view.rs` 测试 12 处 + `main.rs:2562`）。

**替代方案（更小，但不推荐）**：`build_view` 收 `&PlanModeCell` 而不加标量参数 ——
形参从 8 变 9，仍违反那条注释。若评审认为 `ViewInputs` 超范围，退而用这条，
但要在注释里改口径（「8 个」→「9 个」），**不能就这么加上去**。

`CodingView` 的构造点共 3 处：`view.rs:98`（生产）、`prompter.rs:374` / `:393`（测试）。
`#[derive(Clone, Debug)]` 无 `Default` ⇒ 三处都要显式填。

### 5.9 `app_loop::run` 的参数

新增一条信道 ⇒ 两个参数（`plan_mode` 与 `plan_answers`）。该函数已有 17 个形参、
带 `#[allow(clippy::too_many_arguments)]`（`:294`）—— 加两个不触发新 lint，但**不再
容忍第三个**。若评审要求收敛，同 `ViewInputs` 处理。

```rust
// main.rs:2576 附近
let (out_tx, out_rx) = mpsc::channel::<Outbound<CodingView>>(OUTBOUND_CAPACITY);
let (plan_tx, plan_rx) = mpsc::channel::<PlanAnswer>(PLAN_ANSWER_CAPACITY);   // 新，容量 4
```

容量 4 的理由：用户在 TUI 上每次只能提交一行，`pending_plan_prompt` 为真时
`submit` 只发一条 `PlanAnswer` ⇒ 正常负载 0–1 条在飞。4 是「多按几次 Enter 也不阻塞」。

---

## 6. TUI 侧

### 6.1 `submit` 的前置分支

`events.rs:692` 的 `submit` 在 `parse_with_skills` **之前**加分派：

```rust
fn submit(app: &mut App, requests: &Sender<Request>, boundaries: &Sender<Boundary>,
          plan_answers: &Sender<PlanAnswer>) {
    let text = app.input.take();
    app.completion = None;
    if text.trim().is_empty() { /* 既有：清浏览态 */ return; }
    app.history.record(&text);

    // 第四通道：plan 提问待答时，这一行是**答案**而不是新 Prompt。
    if app.pending_plan_prompt {
        let trimmed = text.trim();
        let yes = trimmed.eq_ignore_ascii_case("yes") || trimmed.eq_ignore_ascii_case("y");
        // 先答后（可能）转发 —— 顺序固定：工具侧要拿到答复才可能返回。
        let _ = plan_answers.try_send(PlanAnswer { switched: yes });
        if yes { return; }                      // 只当答案，不再当 Prompt
        // 用户提修改意见 ⇒ 这句话**同时**是普通 Prompt 进对话。
        // `pending_plan_prompt` 由 `Outbound::PlanPrompt{resolved:true}` 清，
        // 但工具此刻已拿到 `switched: false` 并返回 ⇒ 理论上随后就到；
        // 为防「答复与收回之间用户又敲一句」，这里**主动清位**。
        app.pending_plan_prompt = false;
    }
    let action = commands::parse_with_skills(&text, &app.view.skills);
    dispatch(app, requests, boundaries, action);
}
```

**为什么 `try_send` 而不是 `blocking_send`**：`submit` 在 UI 线程，`PlanAnswer` 容量 4
⇒ 正常永不失败。失败（信道满 / 关闭）意味着 app 侧有问题，此时**不该阻塞 UI**；
后果是工具超时返回「未切」，用户看到的是「模式没变」，可重试。

`App` 加 `pub pending_plan_prompt: bool`（默认 `false`），在
`apply_outbound` 的 `Outbound::PlanPrompt { text, resolved }` 分支里置 / 清：

```rust
Outbound::PlanPrompt { text, resolved } => {
    self.pending_plan_prompt = !resolved;
    if !resolved {
        self.transcript.push(TranscriptLine::System(text));
        self.note_appended_below();
    }
    // `resolved: true` **不推行** —— 收回信号是内部状态，不该在对话里留痕。
}
```

### 6.2 `exit_plan_mode` 的固定文案

```text
计划已完成。回复 yes / Y 退出 plan 模式；否则继续提修改意见。
```

这一行由 `app_loop` 侧（`PlanPromptImpl`）生成，**UI 只显示不拼** —— 文案归 app，
与 `Outbound::Output` 同规。

### 6.3 Q6：`summary` 长度上限

**结论：不收 `summary` 参数，schema 为空对象 ⇒ 无长度上限问题。**

`context.md` 记的契约（决策 4 的「三句话」）只要求工具**返回**这三行，没有要求模型
**传入**计划正文。`ExitPlanMode` 的输入是 `{}`。

理由：
1. **计划正文从哪来**：模型在本回合的 assistant 消息里已经写完了计划，用户刚刚
   在屏幕上读过。让它再复制一遍进工具参数，是**重复 token**（一份长计划 + 一份
   参数），且引入「上限截断」这个新问题。
2. **上限截断会破坏协议**：若允许 `summary` 且设 8 KiB 上限，超限时的行为是
   截断（用户看到的确认文案被截断 = 骗人）还是拒绝（模型要重写）？两个都不好。
3. **`MAX_SUMMARY_BYTES` 不是先例而是反例**：`subagent.rs:32` 限的是**子 agent
   回传给父**的摘要，那是跨 agent 边界、父没看过原文。这里用户已经看过了。

若将来确实需要 `summary`（例如用户想「先看摘要再决定」），那是**新需求**：
届时应加长度上限并**拒绝**超限（而非截断），因为截断的确认文案比没有更糟。

### 6.4 命令补全与 `/help`

两条新命令进 `COMMANDS` 即自动出现在补全浮层与 `/help`（三者同源，
`commands.rs:42-45`）。`arg_hint: None`（都不带参数）。

`commands.rs:1446` 那条断言列出了命令名清单，要加 `"plan"` / `"yolo"`。

---

## 7. 与子 agent 的边界（明确的不保证）

`Plan` **不是**安全边界。已知行为：

| 场景 | 行为 |
|---|---|
| `Plan` 下派 `General` 子 agent | **允许**，且子 agent **可写**（它没有门禁） |
| `Plan` 下派 `Explore` 子 agent | 允许；本来就只读（`subagent.rs:1084` 过滤掉 `write`/`edit`） |
| `Plan` 下新起 `bash_bg` | **拒绝**（门禁在 `bg_tools.rs:84`） |
| 切 `Plan` **前**已在跑的后台进程 | **不动**（`context.md` 必需项 2 已定） |

**推断**：若将来要「Plan 是安全边界」，做法是 `child_tools` 也挂 `plan_mode`（它
已经是独立构造点，`subagent.rs:1082`）。本设计**故意不做** —— 那会让 `Plan` 下
的探索能力也受限，与「plan 模式用来规划」的用途相反。

这句话**不写进系统提示**（模型不需要知道自己的边界之外还有什么），但必须写进
**用户文档**与**代码注释**。

---

## 8. 改动清单

| 文件 | 改什么 |
|---|---|
| `crates/ys-core/src/lib.rs` | `mod plan;` + `pub use plan::*;` |
| `crates/ys-core/src/plan.rs` | **新建**：`PlanMode` |
| `crates/ys-tool/src/lib.rs` | `mod plan;` + `pub use plan::*;` |
| `crates/ys-tool/src/plan.rs` | **新建**：`PlanModeState` / `PlanPrompt` / `PlanAnswer` / `deny_if_plan` |
| `crates/ys-tool/src/context.rs` | 加 `plan_mode` / `plan_prompt` 两字段 + 两个 `with_*` |
| `crates/ys-component/src/context.rs` | 加 `plan_mode: Option<&dyn PlanModeState>` + `with_plan_mode` |
| `crates/ys-loop/src/basic.rs:1337` / `:1409` | 两处 `ToolContext` 构造串上挂载点（**必须同形**） |
| `crates/ys-protocol/src/request.rs` | `Outbound::PlanPrompt` / `Request::SetPlanMode` |
| `adapters/tools-basic/src/write.rs:35` | 首行 `deny_if_plan(ctx.plan_mode, "write")?`（`_ctx` → `ctx`） |
| `adapters/tools-basic/src/edit.rs:1072` | 同上，`"edit"` |
| `adapters/tools-basic/src/bg_tools.rs:84` | 同上，`"bash_bg"` |
| `apps/coding-agent/src/plan.rs` | **新建**：`PlanModeCell` |
| `apps/coding-agent/src/plan_prompt.rs` | **新建**：`PlanPromptImpl` |
| `apps/coding-agent/src/plan_tools.rs` | **新建**：`EnterPlanMode` / `ExitPlanMode` / `EXIT_PLAN_WAIT` |
| `apps/coding-agent/src/lib.rs` | 三个新 mod |
| `apps/coding-agent/src/assembly.rs:200` | 追加两个工具 |
| `apps/coding-agent/src/assembly.rs:313-317` | `chain` 追加两项 |
| `apps/coding-agent/src/toolset.rs:646-660` | 父表 `12` → `14` + 追加段断言 +两项 |
| `apps/coding-agent/src/toolset.rs:714` | `fp_parent` 字面值 |
| `apps/coding-agent/src/prompt.rs:224-228` | 删 `no plan mode` 从句 |
| `apps/coding-agent/src/prompt.rs:1310` | 断言同步删 |
| `apps/coding-agent/tests/e2e_skills.rs:90` | 逐字拷贝同步删 |
| `apps/coding-agent/src/view.rs:41` | 引入 `ViewInputs`（或按 §5.8 替代方案） |
| `apps/coding-agent/src/view.rs:98` | 填 `plan` |
| `crates/ys-tui-coding/src/view.rs:83` | `CodingView.plan` + `Default` |
| `crates/ys-tui-coding/src/prompter.rs:374` / `:393` | 测试构造补字段 |
| `crates/ys-tui-coding/src/commands.rs:53` | `17` → `19` + 两条 `CommandSpec` |
| `crates/ys-tui-coding/src/commands.rs:265` | `match name` 加两臂 |
| `crates/ys-tui-coding/src/commands.rs:1446` | 断言命令名清单 |
| `crates/ys-tui-coding/src/app.rs` | `pending_plan_prompt` + `apply_outbound` 分支 |
| `crates/ys-tui-coding/src/events.rs:692` | `submit` 前置分支 + 签名加 `plan_answers` |
| `crates/ys-tui-coding/src/events.rs:516` | Enter 键调用点传新参数 |
| `crates/ys-tui-coding/src/events.rs:180-193` | `run` / `event_loop` 签名 |
| `crates/ys-tui-coding/src/run.rs:183` | `App::new` + 传 `plan_answers` |
| `crates/ys-tui-coding/src/draw.rs:749` | `segs.insert(1, "PLAN")` |
| `apps/coding-agent/src/app_loop.rs:295` | `run` 加 `plan_mode` / `plan_answers` |
| `apps/coding-agent/src/app_loop.rs:467` | `SetPlanMode` 分支 |
| `apps/coding-agent/src/app_loop.rs:748` | `/clear` 追加半句 |
| `apps/coding-agent/src/main.rs:2576` | 建 `plan` 信道 |
| `apps/coding-agent/src/main.rs:2631` | `ys_tui_coding::run` 传参 |
| `apps/coding-agent/src/main.rs:2603` | `app_loop::run` 传参 |
| `skills/sequential-workflow/REFERENCE.md:23/30/352/359/373` | `ExitPlanMode` 措辞改为「请求退出，TUI 请用户在输入框确认」 |

**不动**：`toolset.rs:40` `EXPECTED_ORDER`（9 元）、`fp_tools`、`child_tools`、
`ApprovalHandler` / `AutoApprove`、`Boundary`、任何 `select!` 的分支数。

---

## 9. 测试矩阵

`docs/design.md` §12 的条目对应关系见表末列。

### A. 门禁（对应「工具调用期降级」）

| 用例 | 断言 | 变异 |
|---|---|---|
| `gate_passes_when_unmounted` | `deny_if_plan(None, "write")` 为 `Ok` | `None` 分支改成 `Err` ⇒ 红 |
| `gate_denies_in_plan_and_names_the_exit` | `Err(PermissionDenied)`，文案含 `ExitPlanMode` 与 `Do not retry` | 删掉 `ExitPlanMode` ⇒ 红 |
| `gate_passes_in_yolo` | 挂 `PlanModeCell::new(false)` ⇒ `Ok` | `is_plan()` 恒真 ⇒ 红 |
| `write_call_is_unchanged_when_unmounted` | 同输入同输出（既有 `write.rs` 单测形状） | 门禁改成无条件 `Err` ⇒ 红 |

### B. 阻塞有界（对应「工具超时」）

| 用例 | 断言 | 变异 |
|---|---|---|
| `exit_plan_mode_declares_its_budget` | `ExitPlanMode::timeout() == Some(Duration::from_secs(180))` | 返回 `None` ⇒ 红 |
| `exit_plan_mode_is_serial` | `!ExitPlanMode::is_concurrency_safe()` | 改成 `true` ⇒ 红 |
| `abort_pierces_the_wait` | fake prompt 永不答 + push `Abort` ⇒ < 400ms 返回「未切」 | 删 250ms 轮询分支 ⇒ 红 |
| `no_prompt_mount_fails_fast` | `plan_prompt` 为 `None` ⇒ `timeout(1ms, call)` 成功且 `is_error` | 改成等待 ⇒ 红 |

### C. 第四通道端到端

| 用例 | 断言 | 变异 |
|---|---|---|
| `plan_answer_switches_the_mode` | 注入 fake prompt 返回 `switched: true` ⇒ 回合后 `is_plan() == false` 且结果含 `now: Yolo` | 不写状态 ⇒ 红 |
| `plan_answer_declined_keeps_the_mode` | `switched: false` ⇒ 模式不变，结果含 `now: Plan` + `不要再调` | 删第三句 ⇒ 红 |
| `plan_prompt_resolved_is_sent_on_both_paths` | 成功与超时两条路径都收到 `resolved: true` | 只在成功路径发 ⇒ 红 |
| `submit_yes_sends_only_plan_answer` | `pending_plan_prompt` 为真时 `submit("yes")` ⇒ 发 `PlanAnswer` 且**不**发 `Prompt` | 删 `return` ⇒ 红 |
| `submit_edit_sends_answer_then_prompt` | `submit("改成 X")` ⇒ 先 `PlanAnswer{false}` 再 `Prompt` | 顺序反了 ⇒ 红 |

### D. 装配契约

| 用例 | 断言 | 变异 |
|---|---|---|
| `assembly.rs:313-317`（既有用例扩展） | 父表 == 9 + 3 + 2 | 插入位置挪到 `bash_bg` 前 ⇒ 红 |
| `toolset.rs:646-660`（既有） | 父表 14 元 + 追加段 5 项 | 漏一项 / 长度写 12 ⇒ 红 |
| `toolset.rs:714`（既有） | `fp_parent` 新字面值 | 改回旧值 ⇒ 红 |
| **新增** `child_table_is_a_prefix_of_parent` | `built.agent.tool_names()[..9] == EXPECTED_ORDER` | 新工具插在基础段 ⇒ 红 |

### E. 命令与状态栏

| 用例 | 断言 | 变异 |
|---|---|---|
| `commands.rs` 既有 `parse` 表驱动 | `("/plan", Request::SetPlanMode{Plan})` / `("/yolo", …Yolo)` | 两臂互换 ⇒ 红 |
| `commands.rs:1446` 既有 | 命令名清单含 `plan` / `yolo` | 漏 ⇒ 红 |
| `draw.rs` 新增 | `view.plan` 为真时 `status_text` 的 `segs[1] == "PLAN"` | 插到尾部 ⇒ 窄屏下先被剥 ⇒ 红 |

### F. 模式存续

| 用例 | 断言 | 变异 |
|---|---|---|
| `mode_survives_new_and_clear` | `SetPlanMode{Plan}` → `NewSession` → `ClearSession` ⇒ 仍 `is_plan()` | 任一分支重置 ⇒ 红 |
| `clear_output_mentions_mode_when_plan` | `/clear` 输出含 `plan 模式仍生效` | 恒定不加 ⇒ 红 |

### G. 变异测试脚本

建议 `python3 scripts/plan-mode-mutation.py`，注入 6 处（`Tool::timeout` 返 `None`、
删轮询分支、`None` 挂载点改成等、门禁 `None` 分支改成拒、新工具插到基础段、
`submit` 的 `return` 删除），期望各自用例变红。与 `cancellation-mutation.py` /
`wheel-mutation.py` 同规。

---

## 10. 已知代价与接受的理由

| 代价 | 量化 | 为什么接受 |
|---|---|---|
| 父表前缀缓存**一次性**全 miss | 每会话一次，量级 = 系统提示 + 14 个 spec | 任何工具表变动都如此，与设计无关 |
| `ExitPlanMode` 等待期间占住 `Segment::Serial` | 上界 180s；期间其他工具**仍可跑**（并行批不受影响，只有串行段阻塞） | 用户不在时会超时；模型看不到「卡住」 |
| 250ms 轮询 | 4 次/秒 × `AtomicBool::load` ≈ 5s 内 20 次唤醒、< 20µs CPU | 相对一次模型调用（秒级）可忽略 |
| 门禁多一次虚调用 | ~2–5ns，相对 `write::call`（20–200µs）< 0.03% | 不值得为 5ns 破坏 `ToolContext` 的 `&dyn` 同形性 |
| **超时窗口**：外层 `timeout` drop future ⇒ `ask_exit` 的收回信号可能不发 | UI 会短暂挂在 `pending_plan_prompt` | 由 `Outbound::View` 的 `plan: bool` 兜底 —— 用户看到状态没变，再敲一次 `yes` 即可；不新增机制 |
| `Plan` 不传染子 agent | `General` 子 agent 在 `Plan` 下可写 | 见 §7：`Plan` 是防误触门禁，不是安全边界 |
| `ViewInputs` 重构 | 13 个 `build_view` 调用点 | 由 `view.rs:39-40` 的既有注释要求（「不再加参数」），不是本设计引入 |

---

## 11. 与 `context.md` 的对账

逐条核对 `context.md` 的 6 条决策与 2 个必需项，确认无偏离：

| `context.md` | 本文落点 | 状态 |
|---|---|---|
| 决策 1：门禁在执行时，工具表恒定 | §3.2，§8「不动」 | ✓ |
| 决策 2：两档，默认 `Yolo` | §4.1 | ✓ |
| 决策 3：`/plan` 进、`/yolo` 出，幂等/设成 | §5.4，§5.5 | ✓ |
| 决策 4：两工具不对称，三句话契约 | §3.3，§4.4 | ✓ |
| 决策 5：seabox 先落地 | 本文不涉及（切片 ①） | n/a |
| 决策 6：无沙箱时仍进入模式 | `EnterPlanMode` 不检查沙箱（§4.4） | ✓ |
| 必需项 1：系统提示的能力边界 | §5.3 | ✓ |
| 必需项 2：后台任务只拦新进程 | §7 表格 | ✓ |
| Q6（`summary` 上限） | §6.3 | ✓ 已结 |
| Q10（改 skill） | §8 末行（5 处措辞） | ✓ |

**未在 `context.md` 中出现、本文新增的决策**（需 ADR 记录）：

1. 第四通道**只有一条** `mpsc::Sender<PlanAnswer>`，且 `app_loop` 的任何 `select!`
   **不加分支** —— 与 B 的分歧点。
2. `Outbound::PlanPrompt` 用 `resolved: bool` 双态而非两个变体。
3. `build_view` 引入 `ViewInputs`（由既有注释要求，非本设计偏好）。
4. `PlanAnswer` 容量 4、`EXIT_PLAN_WAIT = 180s` 两个取值。
5. `ExitPlanMode` **不收** `summary` 参数（Q6 的答案）。
