# `/goal` 命令设计

> 需求与决策前提见同目录 [`context.md`](context.md)（D-A ~ D-P、F1 / F2、作用域表）。
> 本文回答的是**怎么实现**，并记录四个候选架构的对比与取舍。
> 术语按 `docs/CONTEXT.md`「回合、轮、追问与任务」：**轮**（round，一次模型调用）⊂
> **回合**（turn，一次 `run_turn_chain`）；**追问**是系统追加的下一个回合；**任务**是一个
> 200 回合预算的生命周期。

---

## 0. 一句话

`/goal` 只做三件事：**登记一句目标**、**让它驱动既有的追问循环**、**在 200 回合耗尽时起一个收卷回合**。
不新增验收器、不改 `ys-loop`、不改工具表 —— 达成判定由**清单清空**表达，代价明确写在 §7。

---

## 1. 候选方案对比

四个候选按不同的设计哲学独立产出（最小复杂度 / 可扩展优先 / 性能资源优先，外加一个
可扩展变体）。它们在 **Q1「目标怎么算达成」** 上分别选了三个不同答案，这是本设计唯一真正的分岔。

### 1.1 四个候选的形状

| | **A：最小复杂度** | **B：可扩展优先** | **C：性能/资源优先** | **D：可扩展变体** |
|---|---|---|---|---|
| 目标状态载体 | `Option<GoalState>`，`app_loop` 局部变量 | `Arc<GoalStore>`，adapter 层 + sidecar 落盘 | `Option<Goal>`，`app_loop` 局部变量 | `ys-loop` 内的状态机（`GoalState`） |
| 达成判定（Q1） | **①清单清空** = 达成 | **③`update_goal` 工具** + `GoalStatus` 四态 | **②整行自陈令牌** `GOAL: DONE` | **③`goal_update` 工具** + 闸门否决 |
| 判据结构 | `decide()` 6 条短路 | `AdvanceCriterion` trait 链 + `TaskPolicy` | `decide()` 6 条短路 | 状态机 + 预算凭证 |
| 任务/预算抽象 | 无（`u32 turns`） | `Task` / `TaskId` / `TaskBudget` / `BudgetLimit` / `BudgetScope` 一等概念 | `TaskState { turns_used, wrap_up_fired }` | `GoalTurn` 凭证类型 |
| 新 crate | 无 | `adapters/tools-goal` | 无 | 无 |
| 新工具 | 无 | `update_goal`（adapter，常驻父表） | 无 | `goal_update`（**在 `ys-loop` 里**，违反分层） |
| 净代码量 | **约 −400 行** | 约 +700 行 | 约 −450 行 | 约 +780 行 |
| 每 turn 额外 token | 目标块，一次性 | 目标块 + 工具表常驻开销（未实测） | 目标块一次性 + 约 4 output tok/turn | 目标块**每回合重写**（含 turn 序号 ⇒ 每回合全价尾字节） |

### 1.2 关键分歧：Q1 四选一

四个候选里，D 是 B 的变体（同选 ③），差别只在**工具放哪一层**。

**候选 A 选 ①（清单清空 = 达成）**

理由：清单清空是**全仓唯一结构化的“我干完了”信号**，`decide` 判据 4 已在用，
`TodoStore::revision > 0` 已能区分“干完了”与“从没干过”；而模型声明是**不可验证的断言**，
清单清空是**可观察的状态**。

代价：模型写一份简陋清单即可“达成”。D-P 删判官留下的洞，A 选择不掩盖它。

**候选 B 选 ③（`update_goal` 工具 + `GoalStatus`）**

理由：达成是目标的属性，不是清单的属性；`GoalStatus` 四态（`Pursuing` / `Achieved` /
`Abandoned` / `Blocked`）让“放弃”与“受阻”也成为可表达的结果，而不是只能靠 200 回合耗尽。
工具放 adapter 层（与 `tools-todo` 同位置），核心不认识它 —— 删掉整个 crate 仍可编译运行。

代价：**9 个新概念**（自陈：目标 / `GoalStatus` / 任务 / `TaskId` / `TaskBudget` /
`BudgetLimit` / `BudgetScope` / `AdvanceCriterion` / `Spend`），且 `update_goal` 常驻父表 ⇒
不用 `/goal` 的用户每一轮（round）也付这份固定开销。

**候选 C 选 ②（整行令牌 `GOAL: DONE`）**

理由：成本最低（每回合约 4 output tok，200 回合累计约 900 tok 有效约 430），
且达成是目标的属性。判据 6 默认方向是**停**（无标记即停），避免不守协议的模型烧满 200 回合。

代价：软契约，模型不写标记就早停；`RunResult.final_message` 已存在（`result.rs:28`），
无需改 `ys-loop`，但需要扫最后一条 assistant 正文。

**候选 D 也选 ③（`goal_update` 工具），但把工具实现放进 `ys-loop`**

与 B 的关键差别：目标状态机、预算扣减、工具实现全进 `ys-loop`
（`goal.rs` 220 行 + `goal_tool.rs` 130 行），并用 `GoalTurn` 凭证类型让
“收卷回合不扣预算”成为结构保证。清单闸门通过 `CompletionGate` 窄接口从 app 层回插，
避免 `ys-loop` 反向依赖 app。

**这条分层选择不成立。** 核查：`ys-loop` 的生产代码里**零个 `Tool impl`**
（`basic.rs` 前 2300 行 `impl Tool for` 计数为 0，全部出现在 `#[cfg(test)]` 里）；
全仓所有工具实现在 `adapters/tools-basic` / `adapters/tools-todo` / `adapters/tools-web`。
清单自己的工具也在 adapter（`adapters/tools-todo/src/tool.rs`），尽管清单是长程任务的核心机制。
把 `goal_update` 放进 `ys-loop` 会开创一个没有先例的形态：**loop 层开始装工具**，
与“最小核心 / 工具是可选的”直接冲突。

它有两个有价值的想法值得保留：**`GoalStatus` 五态**
（`Active` / `Done` / `Blocked` / `Exhausted` / `Cleared`）让“受阻”成为可表达的结果；
**`GoalTurn` 凭证**用类型系统保证“收卷回合不扣预算”而非靠算术。二者都应落在 adapter
或 app 层，不是 `ys-loop`。

代价：概念数最多（在 B 的 9 个之上再加 `GoalTurn` / `CompletionGate` / `GoalSnapshot`）；
且 `Blocked` 态的“需要用户决策”在 TUI 里没有对应入口 —— 用户看到 `Blocked` 之后
能做的只有 `/goal clear`（= 打断 + 清目标），它比直接 `Esc` 多一步清空目标。

### 1.3 事后核对：候选里的论据有两条不成立

候选 B 主张目标状态**必须**放 `Arc<GoalStore>`，否则「`app_loop` 局部变量会在 `:1482`
的 `*agent = built.agent` 重建时丢掉」。

**这条不成立。** `:1482` 在 `switch_workspace` 里，只重建 `agent` 与 `subagent` 两个字段，
`todo_store` 本身也是 `app_loop` 的局部变量（`app_loop.rs:465` `let todo_store = factory.todo()`），
两者都不受影响。`/resume` 走的是另一条完全不同的路径（重建整个 `run` 上下文，不是这个函数）。
故“局部变量会丢”不是选择 `GoalStore` 的理由；`GoalStore` 若要有理由，只能是**多目标/持久化**
这类未来需求，而 D-O 已经把任务定为进程内生命周期。

（保留这条记录是为了让后续评审知道：B 的抽象门槛比它自己声明的低。）

### 1.4 选择：候选 A 为基线，吸收 C 与 D 的局部做法

**选 A（最小复杂度）作为基线**，因为：

1. **D-P 已经拍板接受“达成判定变弱”这个代价**。B、C、D 都想把达成判定做得更可靠，
   但没有一个能真正消除它 —— B 和 D 的 `update_goal` 是模型自陈，只是换了通道；
   C 的令牌同样是自陈。既然三者本质都是自陈，选**最便宜**的那个。
2. B 的 9 个新概念与 D-D「不要过度设计」直接冲突，且其中 `AdvanceCriterion` trait 链、
   `BudgetScope`、`Spend` 在当前只有两条判据链（有目标 / 无目标）时是纯开销。
3. C 的结构与 A 几乎同形（都是局部变量 + 短路判据 + `TaskState`），但多一个自陈令牌协议 ——
   而 A 不需要新增协议面。

**从 C 和 D 各吸收一点**（这几点 A 没有但成本极低）：

- **`GoalMarker` 式的自愈注入**（来自 C）：目标块用标记（`<goal_brief`）判存在性，被压缩吃掉
  就在下个回合边界补一份。这与 `todo_inject::contains_marker`（`todo_inject.rs:60`）同一契约，
  约 10 行。
- **「不把 turn 序号 / 200 / 剩余预算写进 prompt」**（来自 C）：它每回合都变 ⇒ 每回合一笔全价
  尾字节，而模型知道也不能改变行为（结局由 harness 决定）。进展只走 `Outbound::Output`
  （transcript，0 token）。D 的注入块里带了 `turn 37/200`，正是这里要避开的形式。
- **收卷不计入 200 必须由结构保证**（来自 D 的 `GoalTurn` 凭证，化简）：D 用新类型强制，
  A 用「循环外独立调用点」达成同一件事 —— 收卷回合根本不经过 `consume()`，故不需要新类型，
  但保留的是**同一个意图**：不能靠算术“算的时候记得跳过它”。

**不采用 B/D 的 `update_goal` 工具**，理由记录在 ADR-0074：它是“为了表达力多付一个常驻工具槽 +
一个 adapter crate”，而 D-O 下任务不落盘、目标单值，用不上那四态。D 的 `Blocked` 态额外要求
一条从未存在过的 TUI 交互路径（用户如何“解锁”），落在范围之外。

> **Q1 的最终答案：①清单清空 = 达成。** 有目标时，清单清空返回 `Stop(GoalReached)`；
> 无清单不再 `Stop(NoPlan)`（D-A 的核心修补，见 §5.2 判据 4）。

---

## 2. 模块划分

| 文件 | 职责 | 改 / 新建 | 量级 |
|---|---|---|---|
| `apps/coding-agent/src/goal.rs` | 只留 `decide()` 5 条判据；删 `MAX_FOLLOW_UPS` / `STALL_LIMIT` / 判官常量 | 改（净删） | −200 行 |
| `apps/coding-agent/src/goal_judge.rs` | 判官解析 | **删** | −622 行 |
| `apps/coding-agent/src/goal_judge_call.rs` | 判官调用 | **删** | −388 行 |
| `apps/coding-agent/src/turn_advance.rs` | `AdvanceState` 瘦成 `used` + `git_follow_up_used`；删 `judge_failure_delta` / `harness_facts` | 改（净删） | −200 行 |
| `apps/coding-agent/src/goal_state.rs` | `GoalState` + `GOAL_TURN_CAP` + 目标块渲染 + 收卷文案 | **新建** | ~110 行 |
| `apps/coding-agent/src/app_loop.rs` | 持有 `Option<GoalState>`；追问循环加 turn 记账；`Request::SetGoal` 臂；收卷回合 | 改 | +120 / −40 |
| `apps/coding-agent/src/main.rs` | 删判官模块声明；`advance` 调用点适配签名 | 改 | +15 / −30 |
| `crates/ys-protocol/src/request.rs` | 新增 `SetGoal { text: Option<String> }` | 改 | +25 |
| `crates/ys-protocol/src/goal.rs` | `StopCause` 删 3（`Stalled` / `JudgeUnreliable` / `CapReached`），增 1（`GoalReached`） | 改 | +2 / −9 |
| `crates/ys-tui-coding/src/commands.rs` | `COMMANDS` 加一行；`parse_goal` | 改 | +40 |
| `crates/ys-tui-coding/src/view.rs` | `CodingView.goal: Option<GoalView>` | 改 | +18 |
| `crates/ys-tui-coding/src/draw.rs` | 状态行 `[goal]` 段（照 `[loop]` 同规） | 改 | +10 |
| `apps/coding-agent/src/view.rs` | `build_view_with_loop` 加 `goal` 形参 | 改 | +8 |

**没有新 crate、没有新 trait、没有改 `ys-loop` / `ys-component` / `ys-runtime`。**

净效果：**约 −400 行**。这是删掉判官（1010 行）与 stall 层（判据 7 + `StallTracker` + `stall_digest`）
之后的余量，而不是“新增功能”的正开销。

### 2.1 上表没算进去的两个改动面

两个都是**加法变编译错误**的地方，容易在估工时漏掉：

| 面 | 数量 | 后果 |
|---|---|---|
| `Request` 的穷尽 `match` 臂 | **19 处**（含 `app_loop` 主循环与 `ys-tui-coding` 的测试 fixture） | 新增 `SetGoal` ⇒ 全部编译失败。多数臂只需加 `=> {}`，但每个都得想清楚“这个位置收到 `SetGoal` 该干什么” |
| `CodingView { .. }` 字面量 / `build_view_with_loop` 调用点 | **30 处 / 10 处** | 加 `goal` 字段 ⇒ 全部编译失败，逐个补 `goal: None` |

耗时可控（编译器会把它们全列出来），但**不是 §2 表面看起来的“加个变体与字段”**。

---

## 3. 关键数据流

```
TUI: "/goal 修好 flaky test"
  └─ commands::parse_goal → Action::Request(Request::SetGoal{ text: Some(..) })
       └─ app_loop 主循环（回合之间）收到
            ├─ Output("目标已设：修好 flaky test（上限 200 回合；Esc 中断，/goal clear 作废）")
            └─ goal = Some(GoalState::new(text))        ← **不起回合**

用户接着说 "开始" （或 /goal go）
  └─ Request::Prompt(msg)
       └─ 回合前：goal_marker_text() 作为 Role::Harness 注入（与 todo 同一注入点）
            └─ 回合 #1 = run_turn_chain(...)
                 │
                 ▼
        ┌── 追问循环（`app_loop.rs:812-895`，只加记账与上限判定）─────────┐
        │  goal.consume()                       // turns += 1           │
        │  match advance(store, &mut state, &last_outcome, enabled,      │
        │                head_before, head_after, goal)                 │
        │    Push          ⇒ 注入清单 + 目标块（若被压缩吃掉）→ 再跑     │
        │    FinalReport   ⇒ break，标记需收卷（`advance` 的第 5 条判据）│
        │    Stop(cause)   ⇒ break                                     │
        └──────────────────────────────────────────────────────────────┘
                 │
        turn #2 … #200
                 │
                 ▼
        第 200 回合后 advance 返回 Advance::FinalReport
                 │
                 ▼
        ★ 收卷回合 = 循环**外**的第 201 次 run_turn_chain
           输入 = goal_wrap_up_text()（Role::Harness）
           工具表 = 原样（D-N：可调工具）
           **不计入 goal.turns** —— 结构性成立，不靠减法
                 │
                 ▼
        goal = None + Output("目标未达成：已用 200 回合 …")
```

**回合边界为什么清晰**：`run_turn_chain` 的每次调用 = 1 个回合。收卷回合是**循环外**的独立调用点，
所以“不计入 200”由代码结构保证，而不是“计数时记得跳过它”。

**中断与作废是两件事**：追问循环体在 `Request::Prompt` 臂内跑到底，循环期间新的 `Request`
不会被求值。但 `Esc` 与 `/goal clear` 都不靠把 `Request` 送进循环：

| 用户动作 | 发的消息 | 信道 | 何时被消费 | 效果 |
|---|---|---|---|---|
| `Esc` | `Boundary::Abort` | ② 边界 | **回合内** `select!`（`app_loop.rs:2057`） | 当前回合 `Cancelled` 收场 → 判据 1 → 断循环；**目标保留** |
| `/goal clear` | `Boundary::Abort` + `Request::SetGoal{None}` | ② + ① | Abort 同上一行；`SetGoal` 等循环退出后回到回合之间的 `recv()` | 断循环**并且**作废目标 |

两者都**不**需要“把 `Request` 送进循环”—— 打断走信道 ②，清理走循环退出后的信道 ①。
这与既有的 `Request::Compact && app.is_turning ⇒ 改投 Boundary::Compact`
（`events.rs:928-939`）是同一个形状：一条命令拆成两个信道上的两个动作。

**提示文案写 `Esc` 而不是 `Ctrl+C`**：两者都发 `Boundary::Abort`，但 `Ctrl+C` 是两段式
（第二次直接退出进程，`events.rs:594-603`）、且在有选区时先当复制（`:580-586`）。
一个跑了几小时的任务，用户看到卡住本能按两下 ⇒ 整个进程没了。`Esc` 一段式，空闲时
只清输入框，是更安全的止损键。

---

## 4. 核心接口

```rust
// ── crates/ys-protocol/src/request.rs ──────────────────────────────
pub enum Request {
    // …
    /// 设 / 清一个跨回合目标。`None` = `/goal clear`。
    ///
    /// **不起回合**：设目标只是登记一个验收对象，什么时候开始干活由用户的下一条
    /// 输入决定。D-A 把目标定为独立验收对象而非首回合提示词 —— 若设置即起回合，
    /// 用户无法在设完之后先看一眼再决定开始。
    SetGoal { text: Option<String> },
}

// ── crates/ys-protocol/src/goal.rs ────────────────────────────────
pub enum StopCause {
    NoPlan,          // 无目标且从未写清单 ⇒ 退回一期 git 追问
    AllDone,         // 无目标时的清单清空（唯一正常收场）
    GoalReached,     // 新：有目标时清单清空 ⇒ 目标达成
    Aborted,
    NotContinuable,
    // 删除：Stalled（D-F）/ JudgeUnreliable（D-P）/ CapReached（MAX_FOLLOW_UPS 已废弃）
    // **不新增 TurnCapReached**：200 上限是「停下但先收卷」，用 `Advance::FinalReport`
    // 表达（它是动作，不是停机原因）。`Stop(..)` 的语义恒为「到此为止，不再跑」。
}

// ── apps/coding-agent/src/goal_state.rs（新）───────────────────────
/// 一个任务的**活动目标**。生命周期 = 一个任务（D-O）。
/// **不落盘**：`/resume` 后目标是新任务的空白起点（D14）。
pub(crate) struct GoalState {
    /// 用户原话，逐字保留。注入块与收卷块都引它。
    text: String,
    /// 已消耗的 turn 数（**不含**收卷回合）。
    turns: u32,
}

/// 唯一上限（D-J / D-K）。常量、不读配置（D-D）。
pub(crate) const GOAL_TURN_CAP: u32 = 200;

impl GoalState {
    pub(crate) fn new(text: String) -> Self;
    pub(crate) fn consume(&mut self);
    pub(crate) fn exhausted(&self) -> bool;
    /// 每个回合注入的目标块。**字节只由 `text` 决定** —— 不含 turn 序号 / 剩余预算，
    /// 否则每回合都是一笔全价尾字节，而模型知道这些也不能改变行为。
    pub(crate) fn marker_text(&self) -> String;
    /// 收卷回合的输入。
    pub(crate) fn wrap_up_text(&self) -> String;
}

// ── apps/coding-agent/src/goal.rs（改后）──────────────────────────
/// 5 条判据，**只管清单与回合形态**，返回两态 [`FollowUp`]。
///
/// `goal` 只影响两个走向：无清单时是 `Push` 还是 `Stop(NoPlan)`（判据 4），
/// 以及无 open 项时报 `GoalReached` 还是 `AllDone`（判据 3）。
///
/// **唯一的回合数上限是 200（[`GOAL_TURN_CAP`]），在 [`turn_advance::advance`] 里判。**
/// `used` 形参只作诊断计数，不参与任何闸门。
///
/// **200 上限不在这里** —— `FollowUp` 只有 `Push` / `Stop` 两态，表达不了「停下但先收卷」
/// 这个第三种动作。把上限抬进 `decide` 就得改 `ys-protocol` 的 `FollowUp`，而那是三模式
/// 共用的词汇层。上限判定放 [`turn_advance::advance`]（它本来返回三态的 `Advance`）。
pub(crate) fn decide(
    store: &TodoStore,
    used: u32,
    last_stop: &StopReason,
    goal: Option<&GoalState>,
) -> FollowUp;

// ── apps/coding-agent/src/turn_advance.rs（改后）──────────────────
pub(crate) struct AdvanceState {
    pub(crate) used: u32,                 // 追问回合数（诊断）
    pub(crate) git_follow_up_used: bool,  // 一期至多一次
}

pub(crate) enum Advance {
    Push,
    /// 200 回合耗尽 ⇒ 调用方在**循环外**再跑一个收卷回合（D-M / D-N）。
    ///
    /// 它是**动作**而不是停机原因：收卷回合还会再跑一次，所以不能用
    /// `Stop(..)` 表达 —— 否则 `Stop(TurnCapReached)` 读起来是「停了但要再跑一回合」。
pub(crate) fn advance(
    store: &TodoStore,
    state: &mut AdvanceState,
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    follow_up_on_commit: bool,
    head_before: Option<&str>,
    head_after: Option<String>,
    goal: Option<&GoalState>,
) -> Advance;   // 内部：先问 decide，有目标且 `goal.exhausted()` 时改返 FinalReport
```

---

## 5. 判据设计

### 5.1 判据表（有目标 vs 无目标）

| # | 条件 | 有目标 | 无目标 | 在哪 |
|---|---|---|---|---|
| 1 | `stop_reason == Cancelled` | `Stop(Aborted)` | `Stop(Aborted)` | `decide` |
| 2 | `stop_reason ∈ {Truncated, Failed, MaxRounds, NoOutput}` | `Stop(NotContinuable)` | `Stop(NotContinuable)` | `decide` |
| 3 | 清单无 open 项 **且** `revision > 0` | `Stop(GoalReached)` | `Stop(AllDone)` | `decide` |
| 4 | 清单从未写过（空且 `revision == 0`） | **`Push`**（D-A 修补：无清单 ≠ 完成） | `Stop(NoPlan)` → git 兜底至多一次 | `decide` |
| 5 | `goal.exhausted()`（`turns >= GOAL_TURN_CAP`） | `Advance::FinalReport` | 不适用（D-K：不设上限） | **`advance`** |
| 6 | 其余 | `Push` | `Push` | `decide` |

**判据总数 6**：`decide` 里 5 条（1-4、6），`advance` 里 1 条（5）。

**`MAX_FOLLOW_UPS` 与 `StopCause::CapReached` 一并删除**（原判据 5）。它是一期无目标追问的
硬闸门（8 次），与「有目标」直接冲突：两条同时生效则任务在第 8 个回合就停，永远到不了 200。
保两条只会让人反复遮蔽其中一条，不如只留一条：**200 回合是唯一上限**。

代价：无目标追问失去那个 8 次闸门。可接受 —— 无目标追问的收场本来就不靠计数兜底：
判据 2（收场形态不可续）/ 判据 3（清单清空）/ 判据 4（git 兜底至多一次）三条都在它之前。

**判据 5 为什么在 `advance` 而不在 `decide`**：`decide` 返回 `FollowUp`（两态），
而这一条要的是第三种动作「停下但先收卷」。把它抬进 `decide` 就得扩 `ys-protocol` 的
`FollowUp`，而那是三模式共用的词汇层；`advance` 已返回三态的 `Advance`，
`FinalReport` 天然属于它。

### 5.2 两条为了 D-A 必须改的判据

**判据 3 分裂**：今天 eq `AllDone` 一个变体。有目标时改报 `GoalReached`，让 UI 能区分
“目标达成”与“普通回合收尾”。

**判据 4 在有目标时反转为 `Push`**：这是 D-A 的核心修补。今天的 `Stop(NoPlan)` 会让目标
在模型没写清单时被**静默忽略** —— 正是 D-A 明确否掉的形态。

反转的代价是：模型若坚持不调 `todo` 工具，会一路跑到 200 回合（见 §7 R2）。

**采用「连续 N 回合无清单提醒」作为不对抗式缓解**（`NO_PLAN_REMIND_LIMIT = 5`，常量、不读配置）：

```
有目标 && 清单仍为空 && 已连续提醒 < NO_PLAN_REMIND_LIMIT
  ⇒ Push，且在追问输入里把「请先把目标拆成清单」放在前面
  ⇒ 提醒次数 += 1
```

三个必须写清的性质：

1. **它不放行也不叫停**，只改**输入文案**。判据 4 的 `Push` 结论不变 —— 否则 D-A 又被推翻。
2. **不占 200 回合额度**（额度只由 `GoalState::consume()` 推进，提醒不发回合）。
3. **有上限**（5 次）。之后静默推进 —— 用户仍可 `Esc`。

不进 §4 接口签名：它只影响 `follow_up_text()` 的一个分支，是**纯文本选择**，不是判据。

### 5.3 被删掉的判据

**判据 7（stall，D-F）**：`model_claims_done` 恒真（F2 缺陷 1：它取 `last_stop == Completed`，
而判据 2 已只放行 `Completed`），且信号源是模型自陈、空清单指纹每回合恒等 ⇒ 有目标时必然误杀。
整层删除：`STALL_LIMIT`、`StallTracker`、`stall_digest`、`StopCause::Stalled`。

**判据 3（判官，D-P）**：`evidence_window` 只扫最后一个 `ToolUse` 之后（round 粒度，非 turn 粒度），
200 回合任务里绝大多数证据在视野外；`Verdict` 除 `Unparseable` 外无人消费；成本从未算过。

---

## 6. 收卷回合（D-M / D-N）

实现复用**既有**收卷机制的形状，但不复用 `RunLimits::final_report_prompt`：

| | `ys-loop` 的收卷（`basic.rs:695-740`） | 本设计 |
|---|---|---|
| 触发 | 回合内 `max_rounds` / `wall_clock` | 任务级 200 回合 |
| 作用域 | 一个回合内 | 跨回合 |
| 工具表 | 刻意保留原样（保前缀缓存） | 同 |
| 次数 | 只给一次 | 同 |
| app 侧是否设置 | **只有 `explore` 子 agent 设**（`subagent.rs:1342`）；主回合路径从未设置（全仓其余设置点都在 `basic.rs` 测试里） | — |

**为什么不复用 `final_report_prompt`**：它在 `ys-loop` 里是**回合内**预算的收卷，
触发判据是 `deadline_reached()`（`basic.rs:142`）—— 那个 deadline 在
`run_turn_chain` 入口按 `max_turn_duration()`（默认 48h）**每回合重算**，是回合级阀，
不是任务级阀（F1）。要把它改成任务级，得让 `ys-loop` 认识“任务”概念，与分层相悖。

故收卷回合是 **app 侧循环外的一次普通 `run_turn_chain` 调用**，输入换成收卷提示词。
代价：多一次模型往返（约全任务墙钟的 1/200）。收益：`ys-loop` 零改动。

**收卷回合必须可调工具（D-N）**：否则预算耗尽时它发不出“我这 200 回合干了什么”，
只能空口汇报。工具表原样 ⇒ 前缀缓存仍然命中。

---

## 7. 代价与风险

### 7.1 成本模型

四个量各自有独立来源，**不同假设不可相乘得出结论**：

| 量 | 值 | 来源 |
|---|---|---|
| 输入单价 | 200 回合 × 39k ≈ **7.8M input** | 实测会话规模（`docs/design-final/skills.md` 同量级） |
| 缓存命中率 | 大部分命中（系统提示 + 工具表前缀不变） | 未实测，**只能给区间** |
| **每回合的轮数（round）** | **无上界** | `config.max_rounds()` 生产默认 **`u32::MAX`**（`config.rs:435`，`resolve_limits` 于 `main.rs:1273`） |
| 回合级墙钟 | 48 h（每回合重算） | `max_turn_duration()`，`config.rs:441-443` |

⚠️ **不能把「每回合约数十轮」当作已知量**：生产 `max_rounds` 是 `u32::MAX`，回合内唯一的硬限是
**墙钟**（48 h/回合），不是轮数。故一个回合可以跑任意多轮直到 48 h 或模型收尾。

**有效的总成本上界：不存在。** 能给出的只是典型观察：一问答回合通常 5–20 轮。
要拿到真上界必须**额外加一条回合级轮数阀**（`RunLimits::max_rounds` 已支持），
**本设计不做**：它只能把「不可估」变成「可估」，并不让最坏情况变小，而 D-D 已定「不配置化闸门」。
后果已归入 §9 缺口 2。

### 7.2 风险表

| # | 风险 | 量级 | 缓解 |
|---|---|---|---|
| **R1** | **“达成”退化为“清单清空”** —— 模型写三行简陋清单即可宣告完成，用户那句「修好 flaky test」可能只被修了一个用例 | 语义级 | **无法用设计消除**（D-P 的代价）。两条实际缓解：① 目标块里约束「先拆成可验收清单项」；② **达成时把最终清单快照写进 transcript**（让用户至少看得见“它认为干完的是哪三件”） |
| **R2** | 有目标但模型不写清单 ⇒ 一路跑到 200 回合 | 最坏 200 回合 × 回合内无轮数上界（见 7.1） | D-A 修补的必然另一面。缓解：§5.2 的「连续 N 回合无清单提醒」（不占额度、有上限）；出口：`Esc` |
| **R3** | 无 stall 层（D-F）⇒ 打转的模型烧满 200 回合 | 同 7.1：**上界不可估** | 无技术缓解（D-F 有意如此）。**必须把 `Esc` 写进 `/goal` 提示文案** |
| **R4** | 墙钟可预测性差：没有任务级墙钟阀（F1：48 h 是**每回合**重算，200 回合 ⇒ 上界 200×48h） | 理论上是**天级**，不是小时级 | 已知缺口。D-H 的“48h = 最长自动化执行时间”在实现上**不存在**，见 §9 |
| **R5** | 目标块被压缩吃掉 | 一次性补字节 | 标记自愈（`<goal_brief` + `contains_marker` 同契约） |
| **R6** | 换目标靠 append 不靠改写 ⇒ 旧块留在历史 | ~160 tok 遗留 | 新块首行声明“目标已更新，以此为准” |
| **R7** | **`/goal clear` 与循环之间存在竞态窗口**：它在 TUI 侧被拆成 `Boundary::Abort` + `Request::SetGoal{None}`（见 §3），而 `run_turn_once` 开头会**排空并丢弃**残留 `Abort`（`app_loop.rs:1899-1905`，有 `stale_abort_queued_before_first_turn_is_discarded` 守着） | 窗口极小（循环体是纯同步判定），但非零 | 再按一次即命中。**目标仍会被清掉**（`SetGoal` 在回合之间必达）⇒ 最坏结果是「目标已作废、当前回合多跑一会儿」，不会“以为清了实际没清” |

**R2/R3 的出口为什么写 `Esc` 而不写 `Ctrl+C`**：两者都发同一个 `Boundary::Abort`（`events.rs:1020`），
取消能力无差别 —— 但 `Ctrl+C` 是两段式（第二次退出进程，`events.rs:594-603`），且在有选区时
先当复制（`:580-586`）。故提示文案选一段式的 `Esc`。`Ctrl+C` 仍可用，只是不写在说明书上。

**中断与作废不等价**：`Esc` 只管“这一回合停下”，目标仍在，用户想继续就再发一句话；
`/goal clear` 才作废目标。两者分属两个信道（见 §3），不要混为一谈。

---

## 8. 测试矩阵

| 层 | 用例 | 对应 |
|---|---|---|
| 纯函数 | `decide` 5 条判据 × 有/无目标 = 10 例 | §5.1 |
| 纯函数 | 有目标 + 空清单 ⇒ `Push`（不是 `NoPlan`） | §5.2 D-A 核心 |
| 纯函数 | `advance`：`turns == 200` ⇒ `FinalReport`；`199` ⇒ `Push` | §5.1 第 5 条（在 `advance`） |
| 纯函数 | `used` 不参与闸门：`used = 999` 时仍按清单逻辑走，`CapReached` 已不存在 | §5.1（`MAX_FOLLOW_UPS` 已废） |
| 纯函数 | 无目标时 `turns` 再大也 **不会** `FinalReport`（D-K） | §5.1 / D-K |
| 纯函数 | `marker_text()` 两次渲染逐字节相同；**不含回合序号 / 剩余预算** | §4 |
| 纯函数 | 无清单提醒：前 5 次输入含「拆成清单」，第 6 次起不再含；`Push` 结论不变 | §5.2 |
| 纯函数 | `StopCause -> &str` 每个变体都有非空文案（穷尽 match 的守护用例） | §4 / D-B |
| 协议 | `Request::SetGoal` 穷尽匹配两臂 | `request.rs` |
| 集成 | `/goal` 后首回合注入目标块，`Role::Harness` | §3 |
| 集成 | 200 回合后收卷回合**跑且不计入** `goal.turns` | §6 D-N |
| 集成 | 达成时 transcript 里出现最终清单快照（R1 可观测） | §7.2 R1 |
| 集成 | `/goal clear` 在回合之间生效：目标被清且不再追问 | §7.2 R7 / §9 缺口 4 |
| 单元 | TUI 运行中敲 `/goal clear` ⇒ 同时投出 `Boundary::Abort` 与 `Request::SetGoal{None}`（两信道各一条，顺序固定） | §3 / §7.2 R7 |
| 集成 | `Esc` 能停住一个正在追问的任务（多回合），且**目标仍保留** | §7.2 R2/R3 |
| 集成 | `/goal clear` 运行中敲：目标被作废，且循环不再续跑 | §7.2 R7 |
| 变异 | 注入 8 处 bug（判据 4 反转失效 / 收卷计入 200 / 无目标也设 200 / 目标块含回合序号 / `GoalReached` 错报 `AllDone` / stall 残留 / 提醒不封顶 / 收卷回合被削工具） | 全仓既有纪律 |

**变异测试必须真的改红**：本设计的三条不变量（判据 4 反转 / 收卷不计数 / 上限只对有目标生效）
全都是“看代码一眼觉得对”的那类，靠人工审查抓不住。参照 `scripts/goal-mutation.py` 的形态，
把上述 8 处做成可一键跑的脚本。

---

## 9. 已知缺口（不在本次范围）

1. **F1 未修**：48h 仍是**每回合**重算（`app_loop.rs:1705-1706`），不存在任务级墙钟阀。
   D-H 的“48h = 最长自动化执行时间”目前是**设计意图而非实现事实**。
   ⚠️ 量级后果：200 回合 × 48h ⇒ 理论墙钟上界是**天级**，与“最长自动化执行时间 48h”相差两个数量级。
   要么另立需求修，要么承认 D-H 只剩 200 回合一把阀。
2. **回合内轮数无上界**：`max_rounds` 生产默认 `u32::MAX`（`config.rs:435`）。
   本设计**不**为 `/goal` 额外加 round 阀（D-D：不配置化闸门；且加了也只是把“不可估”变“可估”）。
   后果已写入 §7.1：**总成本上界不可估计**。
3. **R1 的比例从未实测**：ADR-0041 要求的「自称完工但未完工」比例仍是空白。
   在拿到该数据之前，不应重新引入判官。
4. **`/goal clear` 与追问循环之间的竞态窗口**（§7 R7）。`SetGoal{None}` 走信道 ①，
   保证在回合之间到达 ⇒ **目标必被清掉**；但 `Boundary::Abort` 可能与
   `run_turn_once` 开头的残留排空（`app_loop.rs:1899-1905`）相撞而被丢弃，
   该回合不会被取消。
   后果：最坏多跑一个回合，不会“以为清了实际没清”。
   要做到确定性需在追问循环体内轮询 `SetGoal` —— 那会打破“`Request` 只在回合之间消费”
   这个迄今成立的不变量，**本设计不做**。若后续确实需要，再单独立需求。
5. ~~**TUI 追问输入误用 `harness_input()`**~~ **已修**：追问输入现由
   [`todo_inject::follow_up_text`](../../../apps/coding-agent/src/todo_inject.rs:72) 统一给出，
   TUI（`app_loop.rs:861-870`）与 `-p`/`--json`（`main.rs:2348`）共用同一份三分支判据；
   清单非空时不再出现「git 历史变了」这句假前提。
