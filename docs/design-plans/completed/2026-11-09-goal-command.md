# 设计计划：`/goal` 命令

> 权威设计：[`docs/arch/goal-command/design.md`](../arch/goal-command/design.md)（下称「设计」）。
> 决策前提：[`context.md`](../arch/goal-command/context.md)（D-A ~ D-P）、[ADR-0074](../adr/0074-goal-drop-judge-and-stall.md)。
> 评审：[`review.md`](../arch/goal-command/review.md)。
> 执行计划：[`docs/exec-plans/2026-11-09-goal-command.md`](../exec-plans/2026-11-09-goal-command.md)。
> 核实基线：`efa0b7c` 工作区；本文所有行号与量级均经 `rg`/`read` 核对，与设计不符处已在 §5 单列。

---

## 1. 一句话目标

`/goal <文本>` 登记一个**跨回合验收对象**，让它驱动既有的追问循环，在 200 回合耗尽时起一个**收卷回合**；
不新增验收器、不改 `ys-loop`、不改工具表 —— 达成判定由**清单清空**表达。

**本期范围（D-C）**：只做 TUI 入口。`/goal` 与 `GoalState` **不接** `-p` / `--json`；
那两条入口只做机械适配（`advance(.., goal: None)`），行为与一期逐字相同。`--goal` 参数与
`-p` 退出码 `2` 均不在本期。

---

## 2. 模块划分（含核实后的真实量级）

| 文件 | 职责 | 动作 | 核实基线 | 预计落点 |
|---|---|---|---|---|
| `crates/ys-protocol/src/goal.rs` | `StopCause` 删 `Stalled` / `JudgeUnreliable` / `CapReached`，加 `GoalReached`；**新增 `StopCause::text()`** | 改 | 66 行 / 7 变体 | 约 60 行 / 5 变体 + 文案函数 |
| `crates/ys-protocol/src/request.rs` | `Request::SetGoal { text: Option<String> }` | 改 | 535 行 | +25 |
| `apps/coding-agent/src/goal_state.rs` | `GoalState` / `GOAL_TURN_CAP` / `NO_PLAN_REMIND_LIMIT` / `marker_text` / `wrap_up_text` / 提醒 | **新建** | 不存在 | 约 130 行 |
| `apps/coding-agent/src/goal.rs` | 只留 5 条判据；删 `MAX_FOLLOW_UPS` / `STALL_LIMIT` / `JUDGE_FAILURE_LIMIT` / `stall_digest` / `StallTracker` / 判据 5 / 判据 7 | 改（净删） | **553 行**（测试约 430） | 约 270 行（−280） |
| `apps/coding-agent/src/turn_advance.rs` | `AdvanceState` 瘦成 `used` + `git_follow_up_used`；删 `judge_failure_delta` / `harness_facts`；`Advance::FinalReport`；`advance` 加 `goal` 形参 | 改（净删） | **427 行** | 约 290 行（−140） |
| `apps/coding-agent/src/goal_judge.rs` | 判官解析 | **删** | **622 行** | 0 |
| `apps/coding-agent/src/goal_judge_call.rs` | 判官调用 | **删** | **388 行** | 0 |
| `apps/coding-agent/src/app_loop.rs` | 持 `Option<GoalState>`、`SetGoal` 臂、回合前注入目标块、追问循环记账、收卷回合、达成清单快照 | 改 | 7951 行（改动面见 §4） | +120 / −40 |
| `apps/coding-agent/src/main.rs` | 删两条 `mod`；`advance_after_turn` 删 judge 段；两处 `used` 推进点保留为诊断 | 改 | 7478 行 | +20 / −35 |
| `crates/ys-tui-coding/src/commands.rs` | `COMMANDS` 18 → 19；`parse_goal`；`LocalAction::GoalStatus` | 改 | 2383 行 / 18 条 | +45 |
| `crates/ys-tui-coding/src/view.rs` | `GoalView`；`CodingView.goal`；`Default` / `for_test` | 改 | 433 行 | +20 |
| `crates/ys-tui-coding/src/draw.rs` | 状态行 `[goal]` 段（照 `[loop]` 同规） | 改 | 4720 行 | +12 |
| `crates/ys-tui-coding/src/events.rs` | `dispatch()` 里 `/goal clear` 双信道 | 改 | 4322 行 | +15 |
| `crates/ys-tui-coding/src/app.rs` | `note_request_sent` 不把 `SetGoal` 当在飞 | 改 | — | +3 |
| `apps/coding-agent/src/view.rs` | `build_view_with_loop` 加 `goal` 形参 | 改 | 1222 行 | +10 |
| `scripts/goal-mutation.py` | mutant 集改为 8 条 | 改 | **453 行 / 15 mutant** | 约 380 行 / 8 mutant |

**没有新 crate、没有新 trait、没有改 `ys-loop` / `ys-component` / `ys-runtime` / `ys-core`。**
净效果约 **−400 行**（删 1010 行判官 + 约 420 行 stall/cap 层，加约 250 行新代码）。

---

## 3. 核心接口（核实后的准确形式）

### 3.1 协议层

```rust
// crates/ys-protocol/src/request.rs —— 变体在枚举末尾追加（`Request` 不是 non_exhaustive）
pub enum Request {
    // …现有 15 个变体…
    /// 设 / 清一个跨回合目标。`None` = `/goal clear`。**不起回合**。
    SetGoal { text: Option<String> },
}

// crates/ys-protocol/src/goal.rs
pub enum FollowUp { Push, Stop(StopCause) }   // 两态不变

pub enum StopCause {          // 7 → 5
    NoPlan,
    AllDone,
    GoalReached,              // 新：有目标时清单清空
    Aborted,
    NotContinuable,
}

impl StopCause {
    /// 每个变体一句非空中文文案（UI 与统计共用）。
    /// 设计 §8 要求它存在，但**当前全仓没有这个函数** —— 本期新建。
    pub fn text(self) -> &'static str;
}
```

### 3.2 `goal_state.rs`（新建）

```rust
pub(crate) const GOAL_TURN_CAP: u32 = 200;
pub(crate) const NO_PLAN_REMIND_LIMIT: u32 = 5;

pub(crate) struct GoalState {
    text: String,
    /// 已消耗的 turn 数（**不含**收卷回合）。
    turns: u32,
    /// 已发出的「请先拆清单」提醒次数（设计 §5.2 三个性质之一：有上限）。
    plan_reminders: u32,
}

impl GoalState {
    pub(crate) fn new(text: String) -> Self;
    pub(crate) fn text(&self) -> &str;
    pub(crate) fn turns(&self) -> u32;
    pub(crate) fn consume(&mut self);              // turns += 1
    pub(crate) fn exhausted(&self) -> bool;        // turns >= GOAL_TURN_CAP
    /// 回合前注入的目标块。**字节只由 `text` 决定** —— 不含 turn 序号 / 剩余预算。
    pub(crate) fn marker_text(&self) -> String;
    /// 收卷回合的输入（`Role::Harness`）。
    pub(crate) fn wrap_up_text(&self) -> String;
    /// 无清单提醒的前缀：前 `NO_PLAN_REMIND_LIMIT` 次 `Some(..)`，之后 `None`。
    /// **不改判据结论**，只改追问输入文案。
    pub(crate) fn plan_remind(&mut self) -> Option<&'static str>;
}
```

### 3.3 `goal.rs`（改后）

```rust
/// 5 条判据；`goal` 只影响两个走向：
///   判据 3 报 `GoalReached` 还是 `AllDone`；
///   判据 4 是 `Push`（有目标）还是 `Stop(NoPlan)`（无目标）。
/// `used` **只作诊断计数，不参与任何闸门** —— `MAX_FOLLOW_UPS` 已整体删除。
pub fn decide(
    store: &TodoStore,
    used: u32,
    last_stop: &StopReason,
    goal: Option<&GoalState>,
) -> FollowUp;
```

判据顺序（**短路**）：① `Cancelled ⇒ Stop(Aborted)` ② `Truncated|Failed|MaxRounds|NoOutput
⇒ Stop(NotContinuable)`（`_` 兜底同此） ③ `!has_open_items() && revision() > 0` ⇒
有目标 `GoalReached` / 无目标 `AllDone` ④ `snapshot().is_empty()` ⇒ 有目标 `Push` / 无目标
`Stop(NoPlan)`（交 `advance` 退回一期 git 兜底） ⑤ 其余 `Push`。

### 3.4 `turn_advance.rs`（改后）

```rust
pub(crate) struct AdvanceState {
    pub(crate) used: u32,                 // 追问回合数（**诊断**，不参与闸门）
    pub(crate) git_follow_up_used: bool,  // 一期至多一次
}

pub(crate) enum Advance {
    Push,
    Stop(StopCause),
    /// 200 回合耗尽 ⇒ 调用方在**循环外**再跑一个收卷回合（D-M / D-N）。
    FinalReport,
}

pub(crate) fn advance(
    store: &TodoStore,
    state: &mut AdvanceState,
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    follow_up_on_commit: bool,
    head_before: Option<&str>,
    head_after: Option<String>,
    goal: Option<&GoalState>,
) -> Advance;
```

内部顺序：`decide(..)` 得 `FollowUp` → 若 `Stop(NoPlan)` 走一期 git 兜底 → 得到 `Push` 后
**最后**判 `goal.map_or(false, |g| g.exhausted()) ⇒ FinalReport`。
**上限判定在 `Push` 之后**：`Stop` 与 `FinalReport` 不能同时成立（否则「用户取消」会被改写成「去收卷」）。

### 3.5 TUI 层

```rust
// crates/ys-tui-coding/src/view.rs
pub struct GoalView { pub text: String, pub turns: u32, pub cap: u32 }

pub struct CodingView {
    // …
    pub goal: Option<GoalView>,
}

// crates/ys-tui-coding/src/commands.rs
pub enum LocalAction { /* … */ GoalStatus }        // `/goal` 无参：显示当前目标

// crates/ys-tui-coding/src/events.rs —— dispatch()
// SetGoal { text: None }  ⇒ 先 boundary Abort、后 Request::SetGoal{None}（顺序固定）
// SetGoal { text: Some }  ⇒ 只走 Request
```

`Action` 保持单值（不新增「双动作」变体）：拆信道的地点就在 `dispatch()`，与既有
`/compact`（`events.rs:928-939`）同一形状。

### 3.6 app 层接线

```rust
// apps/coding-agent/src/view.rs
pub fn build_view_with_loop(
    /* 现有 9 个形参 */
    loop_state: Option<ys_tui_coding::LoopView>,
    goal: Option<ys_tui_coding::GoalView>,   // 新，末位追加
) -> CodingView;
```

---

## 4. 实现顺序与依赖关系

依赖图（箭头 = 先于）：

```
T1 goal.rs(protocol) ──┐
T3 request.rs ─────────┤
T5 goal_state.rs ──────┼──► T7 goal.rs(decide) ──► T9 turn_advance.rs ──┬──► T11 删判官 ──► T12 回归
                       │                                              └──► T27 app_loop 追问/收卷
T13 tui view.rs ───────┴──► T15 draw.rs
                       ├──► T17 commands.rs ──► T19 events.rs
                       └──► T23 apps view.rs ──► T25 app_loop 设目标 ──► T27 ──► T29 main.rs
T3 ──► T21 app.rs
T7, T9, T25, T27 ──► T31 变异脚本
```

七波（同波内文件集不相交，可并行）：

| 波 | 任务 | 说明 |
|---|---|---|
| **W1** | T1/T3/T5/T13 | 四个独立地基：协议两处、新模块、TUI 视图类型。互不引用 |
| **W2** | T7, T15, T17, T21, T23 | `decide` 重写；`[goal]` 状态行；`/goal` 解析；`note_request_sent`；`build_view_with_loop` 形参 |
| **W3** | T9, T19 | `advance` 加 `FinalReport`；`/goal clear` 双信道 |
| **W4** | T11, T25 | 删判官两文件；`app_loop` 持 `GoalState` + 目标块注入 |
| **W5** | T12, T27 | stall 删除回归；追问循环记账 + 收卷回合 |
| **W6** | T29 | `main.rs` 机械适配（`-p` / `--json` **不接** `GoalState`） |
| **W7** | T31 | 变异脚本重写为 8 条 |

**每个 impl 与它的 test 是串行对**（改同一文件，不能并行），但必须分两次 review —— `test` 任务
要检查断言是否**真会变红**，不是「加个 assert 让它绿」。

**顺序上的三个硬约束**：

1. **协议先于一切**（T1/T3）。加变体/删变体都是编译错误面，晚做会让其余任务反复返工。
2. **`decide` 先于 `advance`**（T7 → T9）。`advance` 内部要调新签名的 `decide`。
3. **`app_loop` 的两次改动分开**（T25 设目标 → T27 追问/收卷）。两者都改 `app_loop.rs`，
   但 T25 只碰主 `match req` 与注入点，T27 只碰追问循环与循环外收卷 —— 合并成一行会让
   review 无法区分「登记目标」与「记账 + 收卷」两类错误。

---

## 5. 设计已定但实施会踩的坑

### 5.1 设计与代码事实不符（必须先知道）

| # | 设计说 | 核实结果 | 影响 |
|---|---|---|---|
| **K1** | §2.1：「`Request` 的穷尽 `match` 臂 **19 处**」 | 全仓只有 **2 个** `match req {`：`app_loop.rs:730`（主循环）、`events.rs:1113`（测试 fixture，**带 `other => panic!` 兜底**）。另有 1 个 `matches!` 守卫 `app.rs:385` | 「19 处」是把 `Request::` 的出现次数当成了 match 臂。真实改动面：`app_loop.rs:730` 加一个臂；`app.rs:385` 是**语义修正**（`SetGoal` 不起回合，不该进在飞态）；`events.rs` 的 fixture 无需改 |
| **K2** | §2.1：「`CodingView { .. }` 字面量 **30 处** / `build_view_with_loop` **10 处**」 | `CodingView {` 语法命中 **28 处**，其中 **22 处带 `..Default` / `..for_test()`** ⇒ 加字段只需改 `Default` 与 `for_test()` 两处；**真正要手写 `goal: None` 的完整字面量只有 3 处**（`apps/coding-agent/src/view.rs:98`、`app_loop.rs:2728`、`app_loop.rs:4284`）。`build_view_with_loop` 调用点 **6 处**（`app_loop.rs:540 / 599 / 636 / 676 / 701 / 1358`） | 工时远小于设计估计，但 6 个调用点分散在 6 个臂里，**漏一个就是编译错误**（不是静默漂移） |
| **K3** | §8 测试矩阵：「`StopCause -> &str` 每个变体都有非空文案（穷尽 match 的守护用例）」 | **该函数不存在**（`rg 'StopCause.*&str'` 零命中；`StopCause` 今天只被构造、从未被渲染） | 本期要**新建** `StopCause::text()`，否则文案断言无从落笔。这是设计 §2 表里没算的一小块 |
| **K4** | §5.2：「它只影响 `follow_up_text()` 的一个分支，是纯文本选择」 | `todo_inject::follow_up_text(store, last_injected_rev, session)` 只有 3 个形参，**拿不到 `GoalState`**；且它被 `app_loop.rs:868`（内联构造）与 `main.rs:2348 follow_up_input` **两处**各自调用 | 必须加第 4 形参 `goal_prefix: Option<&str>`；`-p` / `--json` 侧恒传 `None`。**提醒计数器不能放进 `todo_inject`**（那是纯函数模块、无状态）⇒ 留在 `GoalState::plan_remind()`，调用方取前缀再传进来 |
| **K5** | §4 说 `decide(store, used, last_stop, goal)` | 现签名是 `decide(store, used, last_stop, model_claims_done, stall, judge_failures)` —— **6 个形参 5 个要删** | `model_claims_done` 一并消失。它是 stall 判据的输入；判据 2 已只放行 `Completed`，故它**恒真**（F2 缺陷 1），删掉无损 |

### 5.2 实施时最容易写错的地方

| # | 坑 | 为什么危险 | 正确做法 |
|---|---|---|---|
| **K6** | `let Advance::Push = step else { … }` | 全仓 **3 处**：`app_loop.rs:845`（`else { break }`）、`main.rs:2511` 与 `main.rs:2654`（`else { return … }`）。加 `FinalReport` 后，**后两者会把「该收卷」当成「该停」静默丢弃**，且**编译通过**（let-else 只区分 Push 与「其余」） | 三处全改 `match`。`app_loop` 的 `FinalReport` ⇒ 置 `need_wrap_up`；`main.rs` 的两处 ⇒ 与 `Stop` 同处理（`-p` 不接收卷），并**加注释说明这是有意不等价** |
| **K7** | `AdvanceState.used` 的推进点在 `main.rs:2518` / `:2660`（`app_loop` 侧**没有**推进点） | 删 `MAX_FOLLOW_UPS` 后 `used` 只剩诊断语义。若有人「顺手」把它删掉，`AdvanceState` 就少一个字段，而 `app_loop` 从不写它 ⇒ 埋一个恒 0 的死字段 | **保留两处推进**，字段注释改成「诊断计数，不参与闸门」，并加一条断言 `used` 不影响结论的用例（`used = 999` 仍按清单逻辑收场） |
| **K8** | 追问问循环里 `advance` 的调用（`app_loop.rs:837-844`）在 **`Request::Prompt` 臂内**同步跑到底 | 循环期间新的 `Request` **不会被求值** ⇒ `/goal clear` 的 `SetGoal{None}` 要等循环退出才到达。`Esc` 靠 `Boundary::Abort` 在回合内被消费，能立刻断 | 不要试图「在循环里 `try_recv` Request」—— 那会破坏「回合间消费」的契约。按设计：`Esc` 管停下（目标保留），`SetGoal{None}` 管作废（循环退出后必达） |
| **K9** | R7 竞态：`run_turn_once` 开头排空并丢弃残留 `Abort`（`app_loop.rs:1899-1905`） | `/goal clear` 在同一瞬间投出 `Abort` + `SetGoal{None}`，`Abort` 可能被当「上一回合的残留」丢掉 | 已定不做额外同步（设计 R7）。但**顺序必须固定：先 Abort、后 SetGoal**，且用例要断言两条各一条 |
| **K10** | `/goal` 无参要显示状态，但 `Action` 是单值枚举 | 若为「显示」新增一个 `Request` 变体，`-p` / `--json` 也得实现它 —— 凭空多一条要机械适配的路径 | 用 `Action::Local(LocalAction::GoalStatus)`：状态显示**完全在 TUI 侧**（读 `CodingView.goal`），不产生任何 `Request` |
| **K11** | `COMMANDS` 是**定长数组** `[CommandSpec; 18]`（`commands.rs:53`） | 加一行不改长度 ⇒ **编译错误**（不是静默） | 同时改成 `[CommandSpec; 19]`。`completion.rs:186` 有 `items.len() == all_commands().len()` 的守护用例，会自动覆盖 |
| **K12** | 判官删除涉及 **1010 行 + 3 个 mutant + `main.rs` 两条 `mod`** | 只删文件不删 `mod` ⇒ 编译错误；只删 `mod` 不删 `judge_failure_delta` 的调用点 ⇒ 编译错误；两者都删但漏了 `judge_failures` 字段 ⇒ `goal.rs` 判据 3 悬空 | 用一次「删到底」的改动：两文件 + `main.rs` 两条 `mod` + `turn_advance.rs` 的 `judge_failure_delta` / `harness_facts` + `AdvanceState.judge_failures` + `goal.rs` 判据 3 与其测试。`cargo build` 会把漏网的一一点出来 |
| **K13** | `goal.rs` 553 行里约 **430 行是测试**，其中 stall 三个用例（终态项 / 大小写折叠 / 声明顺序）、cap 一个用例（`used_up_to_cap_stops`，:370）、judge 一个用例 | 删实现不删测试 ⇒ 编译错误；删测试不删实现 ⇒ dead_code warning 升级为 error（若开了 `-D warnings`） | 实现与测试一起删，且**新增**一条反向守护：`used = 999` 时结论与 `used = 0` 相同（证明闸门真的没了） |
| **K14** | `StopCause` 现有测试 `follow_up_carries_stop_cause`（`goal.rs:53-65`）**引用了 `CapReached`**（:58） | 删变体后该用例编译失败 | 把它换成 `GoalReached`，顺带覆盖「不同停机原因不得相等」 |
| **K15** | `scripts/goal-mutation.py` 现有 **15 个 mutant**，其中 4 个针对判官、3 个针对 stall、1 个针对 cap，`TARGETS` 里含已被删除的 `goal_judge.rs` | 只加新 mutant 不删旧 ⇒ 脚本注入失败（文件不存在）⇒ 整轮报错 | 重写 mutant 集为设计 §8 的 8 条：判据 4 反转失效 / 收卷计入 200 / 无目标也设 200 / 目标块含回合序号 / `GoalReached` 错报 `AllDone` / stall 残留 / 提醒不封顶 / 收卷回合被削工具 |
| **K16** | 收卷回合是**循环外**的一次 `run_turn_chain` | 若写在循环体内并「记得不 `consume()`」，未来任何一次重构都可能把它改成计入 | 结构保证：收卷调用点在 `loop { }` **之后**，与 `goal.consume()` 在物理上不同作用域。用例断言 `goal.turns == 200` 且收卷回合的输入含目标原文 |
| **K17** | 目标块被压缩吃掉（R5） | 只在首回合注入 ⇒ 长任务压缩后目标消失，模型「忘了要干什么」 | 追问回合的注入点与 todo 注入**同一处**（`app_loop.rs:757-764` 附近），且用 `contains_marker` 同契约自愈：历史里找不到 `<goal_brief` 就重注 |

### 5.3 一期行为的保护边界（防「顺手做多」）

- **`MAX_FOLLOW_UPS` 彻底删除，不留「仅无目标生效」的中间态。** 无目标追问的收场由判据 2 / 3 / 4
  三条兜住（收场形态不可续 / 清单清空 / git 兜底至多一次）。实现时**不要**写成
  `if goal.is_none() && used >= 8` —— 那种写法会让人以为上限还在。
- **`-p` / `--json` 不接 `GoalState`。** `main.rs` 的两处 `advance(..)` 调用恒定传 `None`；
  `FinalReport` 在这两条入口上按 `Stop` 处理。**不要**给 `-p` 加 `--goal` 参数，
  也不要给它加退出码 `2`（那是 D-C 明确划出去的范围）。
- **`/loop` 与 git 追问不受目标影响**（D-E）：不新增任何「有目标时抑制 X」的耦合。
