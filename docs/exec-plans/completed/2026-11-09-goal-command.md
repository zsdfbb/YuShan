# 执行计划：`/goal` 命令（TUI 一期）

> 对应设计计划：[`docs/design-plans/2026-11-09-goal-command.md`](../design-plans/2026-11-09-goal-command.md)
> 权威设计：[`docs/arch/goal-command/design.md`](../arch/goal-command/design.md) · 决策：[ADR-0074](../adr/0074-goal-drop-judge-and-stall.md)
> 日期：2026-11-09 · 基线 `efa0b7c`
> 构建/测试入口：`./scripts/cargo-build.sh` / `./scripts/par-test.sh`（**不用**裸 `cargo`）。

---

## 0. 范围红线（每个 subagent 开工前必读）

> **前置**：`apps/coding-agent/src/goal_state.rs` 是**新建文件**（T5 创建、T6 在同文件加测试）。
> T5 同时要在 `apps/coding-agent/src/main.rs` 的模块声明区（`:49 mod goal;` 旁）加 `mod goal_state;` ——
> 这是全表唯一一处「新建文件 + 同时改已有文件」的任务；漏加 `mod` 则 T6 跑不起来。

1. **`/goal` 只接 TUI 入口。** `-p` / `--json` 两个模式**只做机械适配** —— `advance(.., goal: None)`、
   `follow_up_text(.., None)`，行为与一期**逐字不变**。
   ⛔ 不要给 `-p` 加 `--goal` 参数，不要给 `-p` 加退出码 `2`，不要让这两条入口持有 `GoalState`。
2. **`MAX_FOLLOW_UPS` 彻底删除**，不留「仅无目标生效」的中间态。`StopCause::CapReached` 一起删
   （唯一来源就是它，删后无生产者）。`AdvanceState.used` **保留但只作诊断计数**，不参与任何闸门。
3. **`decide()` 只剩 5 条判据**：① `Cancelled` ② 不可续收场 ③ 清单无 open 项且 `revision>0` →
   `GoalReached`/`AllDone` ④ 清单从未写过 → 有目标 `Push` / 无目标 `Stop(NoPlan)` ⑤ 其余 `Push`。
   200 回合上限（第 6 条）在 `advance()` 里，返回 `Advance::FinalReport`。
4. **不改** `ys-loop` / `ys-component` / `ys-runtime` / `ys-core`；不新建 crate；不新增 trait。
5. **`/loop` 与 git 追问不受目标影响**（D-E）：不写任何「有目标时抑制 X」。

## 1. 波次总览

同波内**文件集互不相交**，可并行派发。`impl`/`test` 是**串行对**：改同一文件，必须先后两个 subagent，
`test` 任务要独立 review「断言是否真会变红」。**W3–W6 之间工作区会短暂不编译**（签名变更的必然结果），
第一个全绿点在 **W6 之后**。

| 波次 | 任务（串行对） | 主题 |
|---|---|---|
| **W1** | T1→T2, T3→T4, T5→T6, T7→T8 | 协议两处 + 新模块 `goal_state` + TUI 视图类型 |
| **W2** | T9→T10, T11→T12, T13→T14, T15→T16, T17→T18, T19→T20 | `decide` 重写、状态行、`/goal` 解析、`note_request_sent`、view 形参、`follow_up_text` 形参 |
| **W3** | T21→T22, T23→T24 | `advance` 加 `FinalReport`；`/goal clear` 双信道 |
| **W4** | T25→T26 | 整体删除判官 |
| **W5** | T27→T28 | `app_loop` 持有目标 + 目标块注入 |
| **W6** | T29→T30 | `app_loop` 追问记账 + 收卷回合 |
| **W7** | T31→T32 | `main.rs` 机械适配（`-p` / `--json` 行为不变） |
| **W8** | T33→T34 | 变异脚本重写为 8 条 |

## 2. 任务表

| ID | 类型 | 任务 | 涉及文件 | 依赖 |
|----|------|------|----------|------|
| **W1** | | | | |
| T1 | impl | `StopCause` 删 `Stalled` / `JudgeUnreliable` / `CapReached` 三个变体，加 `GoalReached`；新建 `impl StopCause { pub fn text(self) -> &'static str }`（5 个变体各一句非空中文）；更新模块头 `//!`（去掉 `goal_judge` 指针） | crates/ys-protocol/src/goal.rs | - |
| T2 | test | 更新 `follow_up_carries_stop_cause`（原 :53-65 引用了已删的 `CapReached`，换成 `GoalReached`）；新增 `every_stop_cause_has_text`（穷尽 5 变体断言非空且互不相同）；变异：把 `text()` 某个臂改成 `""` ⇒ 变红 | crates/ys-protocol/src/goal.rs | T1 |
| T3 | impl | `Request` 末尾追加 `SetGoal { text: Option<String> }`，带 doc 注释写清「**不起回合**」；不改其余 16 个变体 | crates/ys-protocol/src/request.rs | - |
| T4 | test | 新增用例：`SetGoal{Some}` / `{None}` 经 `serde_json` 往返相等、且与 `SetGoal{None}` 不等；`matches!(req, Request::SetGoal{..})` 两臂 | crates/ys-protocol/src/request.rs | T3 |
| T5 | impl | 新建 `goal_state.rs` 并在 `main.rs` 加 `mod goal_state;`。模块内容：`GOAL_TURN_CAP=200`、`NO_PLAN_REMIND_LIMIT=5`、`GoalState { text, turns, plan_reminders }` 与 `new/text/turns/consume/exhausted/marker_text/wrap_up_text/plan_remind`。`marker_text()` 首行含 `<goal_brief`，**字节只由 `text` 决定**；`wrap_up_text()` 含目标原文 | apps/coding-agent/src/goal_state.rs, apps/coding-agent/src/main.rs | - |
| T6 | test | 用例（同文件 `#[cfg(test)] mod tests`）：`marker_text()` 两次渲染**逐字节相同**且不含 turn 序号/剩余预算；`consume()` 后 `exhausted()` 在 199→false / 200→true；`plan_remind()` 前 5 次 `Some` 第 6 次 `None`；`wrap_up_text()` 含原文。变异：把 `turns` 写进 `marker_text` ⇒ 幂等用例变红 | apps/coding-agent/src/goal_state.rs | T5 |
| T7 | impl | 新建 `GoalView { text, turns, cap }`；`CodingView` 加 `pub goal: Option<GoalView>`；`Default` 与 `for_test()` 各补 `goal: None`（22 处 `..Default` / `..for_test()` 字面量靠它们自动兼容） | crates/ys-tui-coding/src/view.rs | - |
| T8 | test | 用例：`CodingView::default().goal.is_none()`；`GoalView{..}` 与 `goal: None` 的 `CodingView` 不相等（证明字段真参与比较）；变异：`Default` 里写成 `goal: Some(..)` ⇒ 变红 | crates/ys-tui-coding/src/view.rs | T7 |
| **W2** | | | | |
| T9 | impl | `decide()` 改成 `decide(store, used, last_stop, goal: Option<&GoalState>)` 并重写为 5 条短路判据；**删除** `MAX_FOLLOW_UPS` / `STALL_LIMIT` / `JUDGE_FAILURE_LIMIT` / `stall_digest` / `StallTracker` / 判据 5（cap）/ 判据 7（stall）/ 判据 3（judge）；判据 3 按 `goal.is_some()` 分岔 `GoalReached` / `AllDone`；判据 4 有目标时返回 `Push`；模块头 `//!` 重写为 5 条 | apps/coding-agent/src/goal.rs | T1 |
| T10 | test | 删除 stall 三个用例（终态项 / 大小写折叠 / 声明顺序）、`used_up_to_cap_stops`（:370）、judge 用例；重写 `Env` 测试助手（去掉 `stall` / `judge_failures`，`decide` 收 `Option<&GoalState>`）；新增：5 条判据 × 有/无目标矩阵、**`used=999` 与 `used=0` 结论相同**（证明闸门没了）、有目标+空清单 ⇒ `Push`。变异：判据 4 的 `goal.is_some()` 反转 ⇒ 变红 | apps/coding-agent/src/goal.rs | T9 |
| T11 | impl | 状态行加 `[goal]` 段（照 `[loop]` 同规，`draw.rs:979-984` 旁）：`Some(g)` ⇒ `[goal] {used}/{cap} {text截断}`，`None` ⇒ 不出现该段 | crates/ys-tui-coding/src/draw.rs | T7 |
| T12 | test | 用例：`goal: None` 时状态行**不含** `"[goal]"`；`goal: Some(..)` 时含 `"[goal] 3/200"`；变异：把 `if let Some(goal)` 改成无条件 `push` ⇒ 第一条变红 | crates/ys-tui-coding/src/draw.rs | T11 |
| T13 | impl | `COMMANDS` 长度 18 → **19** 并追加 `goal` 条目（`arg_hint: Some("[目标] / clear")`）；`parse_with_skills` 加 `"goal"` 分支 ⇒ `parse_goal(args)`；`LocalAction::GoalStatus`；`goal_status_text(view)` 纯函数；`parse_goal("")` ⇒ `Local(GoalStatus)`、`parse_goal("clear")` ⇒ `Request(SetGoal{None})`、其余 ⇒ `Request(SetGoal{Some(原文)})` | crates/ys-tui-coding/src/commands.rs | T3 |
| T14 | test | 用例：`all_commands().len() == 19`（原 18 + goal）；`parse("/goal 修好 flaky test")` ⇒ `SetGoal{Some("修好 flaky test")}`；`parse("/goal clear")` ⇒ `SetGoal{None}`；`parse("/goal")` ⇒ `Local(GoalStatus)`；`goal_status_text` 有/无目标两态。`completion.rs:186` 的既有守护用例会自动覆盖表长一致性 | crates/ys-tui-coding/src/commands.rs | T13 |
| T15 | impl | `note_request_sent`（`app.rs:385`）的守卫从「排除 `Request::Prompt`」改为「排除 `Prompt` 与 `SetGoal`」—— `SetGoal` 不起回合，不该进在飞态（否则 Esc 取消语义错位） | crates/ys-tui-coding/src/app.rs | T3 |
| T16 | test | 用例：`note_request_sent(&Request::SetGoal{text: Some(..)})` 后 `command_in_flight == false`；`Request::Compact` 后为 `true`；变异：守卫退回只排除 `Prompt` ⇒ 第一条变红 | crates/ys-tui-coding/src/app.rs | T15 |
| T17 | impl | `build_view_with_loop` 末位追加 `goal: Option<ys_tui_coding::GoalView>` 并写入 `view.goal`；**6 个调用点**（`app_loop.rs:540/599/636/676/701/1358`）机械补 `None` | apps/coding-agent/src/view.rs, apps/coding-agent/src/app_loop.rs | T7 |
| T18 | test | 用例：`build_view_with_loop(.., Some(LoopView..), Some(GoalView{..}))` ⇒ 返回值的 `loop_state` 与 `goal` **都是 `Some`**（证明两个形参不串位）；变异：把 `view.goal = goal` 写成 `view.goal = None` ⇒ 变红 | apps/coding-agent/src/view.rs | T17 |
| T19 | impl | `follow_up_text` 追加第 4 形参 `goal_prefix: Option<&str>`：`Some(p)` 时把 `p` 放在注入块**最前面**（在「未完成项优先」之前），`None` 时行为逐字不变 | apps/coding-agent/src/todo_inject.rs | T5 |
| T20 | test | 用例：`goal_prefix: Some("请先把目标拆成清单")` ⇒ 返回文本以该句开头；`None` ⇒ 与改动前逐字相同（用现有的三分支用例做基线）；变异：把 prefix 拼到末尾 ⇒ 第一条变红 | apps/coding-agent/src/todo_inject.rs | T19 |
| **W3** | | | | |
| T21 | impl | `AdvanceState` 瘦成 `{ used, git_follow_up_used }`（删 `stall` / `judge_failures`）；`Advance` 加 `FinalReport`；`advance` 末位追加 `goal: Option<&GoalState>`，**在 `Push` 之后**判 `goal.exhausted() ⇒ FinalReport`（`Stop` 与 `FinalReport` 互斥）；删 `judge_failure_delta` / `harness_facts` / `short_sha`；同步更新 3 个调用点形状（`app_loop.rs:837`、`main.rs:2337` 的 `advance_after_turn` 内） | apps/coding-agent/src/turn_advance.rs, apps/coding-agent/src/app_loop.rs, apps/coding-agent/src/main.rs | T9 |
| T22 | test | 删除 `harness_facts_*` 两个用例；新增：`turns=199 ⇒ Push` / `turns=200 ⇒ FinalReport` / **无目标时 `used=999` 仍走清单逻辑、绝不 `FinalReport`** / `goal=Some` 但 `Stop(Aborted)` ⇒ 仍是 `Stop`。变异：把上限判定挪到 `decide` 之前（`Stop` 也能变 `FinalReport`）⇒ 最后一条变红 | apps/coding-agent/src/turn_advance.rs | T21 |
| T23 | impl | `dispatch()` 里对 `Request::SetGoal { text: None }` **先**投 `Boundary::Abort`、**再**投 `Request`（顺序固定）；`Some` 只走 `Request`。照 `/compact`（`events.rs:928-939`）同形状，当场补一行 `TranscriptLine::System` 反馈 | crates/ys-tui-coding/src/events.rs | T13 |
| T24 | test | 用例：`/goal clear` 后 `next_boundary()` 是 `Abort`、`next_request()` 是 `SetGoal{None}`，两条**各一条**且边界先；`/goal x` 只投 `Request`、不投 `Abort`；变异：把两条投递顺序互换 ⇒ 第一条变红 | crates/ys-tui-coding/src/events.rs | T23 |
| **W4** | | | | |
| T25 | impl | **整体删除判官**：删 `goal_judge.rs` / `goal_judge_call.rs`；删 `main.rs` 的 `mod goal_judge;` / `mod goal_judge_call;`（:50-51）；删 `advance_after_turn` 里的 `judge_failure_delta` 段（:2320-2335）与 `state.judge_failures` 引用；删 `app_loop.rs:818-835` 的判官调用块 | apps/coding-agent/src/main.rs, apps/coding-agent/src/app_loop.rs, apps/coding-agent/src/goal_judge.rs, apps/coding-agent/src/goal_judge_call.rs | T21 |
| T26 | test | 更新 `main.rs` 中依赖判官的多回合用例（`:3860` 附近的 judge 分岔注释、`:3893` 的 `judge_failures` 叙述、`:4707` 的 `AllDone` 断言）：去掉 judge 分岔、断言改为「清单收场 ⇒ 2 回合 2 轮」；新增 `rg` 级守护不用，改为编译即守护（`mod` 已删） | apps/coding-agent/src/main.rs | T25 |
| **W5** | | | | |
| T27 | impl | `app_loop::run` 局部加 `let mut goal: Option<GoalState> = None;`（`todo_store` 旁，:465）；主 `match req` 加 `Request::SetGoal { text }` 臂：`Some(t)` ⇒ `goal = Some(GoalState::new(t))` + `Outbound::Output("目标已设：{t}（上限 200 回合；Esc 中断，/goal clear 作废）")`，`None` ⇒ `goal = None` + `Outbound::Output("目标已作废")`；**不起回合**；首回合注入点（todo 注入同处，:757-764 附近）用 `marker_text()` 作 `Role::Harness` 块，`contains_marker` 同契约自愈 | apps/coding-agent/src/app_loop.rs | T17, T25, T3 |
| T28 | test | 集成用例：发 `SetGoal{Some}` 后**不产生任何回合**（`is_turning == false`，无 `begin_turn` 事件）；随后的 `Prompt` 回合的消息序列里含 `<goal_brief` 且 `role == Role::Harness`；`SetGoal{None}` 后再起回合**不含**目标块。变异：把 `SetGoal` 臂写成起回合 ⇒ 第一条变红 | apps/coding-agent/src/app_loop.rs | T27 |
| **W6** | | | | |
| T29 | impl | 追问循环（:817-895）：每轮 `goal.as_mut().map(GoalState::consume)`；`advance(.., goal.as_ref())`；`match` 替掉 `let Advance::Push = step else { break }`，`FinalReport ⇒ need_wrap_up = true; break`；清单从未写过且有目标时用 `goal.plan_remind()` 取前缀传给 `follow_up_text(.., prefix)`；循环**之外**（`loop {}` 之后）起收卷回合：`run_turn_chain(goal.wrap_up_text())`，工具表原样，**不 `consume()`**；`Stop(GoalReached)` 时把最终清单快照写进 transcript；每回合一条 `Outbound::Output` 报进展（D-B） | apps/coding-agent/src/app_loop.rs | T27, T21, T19 |
| T30 | test | 集成用例：`goal.turns` 达 200 后**收卷回合真的跑了**（消息序列以 `wrap_up_text()` 的内容结尾）且 `goal.turns` 仍是 200（不额外 `consume`）；`Stop(GoalReached)` 时 transcript 出现最终清单快照；`Esc` 能停住多回合任务且 `goal` 仍 `Some`；`/goal clear` 后不再续跑。变异：收卷回合前调 `consume()` ⇒ 第一条变红 | apps/coding-agent/src/app_loop.rs | T29 |
| **W7** | | | | |
| T31 | impl | `main.rs` **机械适配**：两处 `advance_after_turn`（:2310-2337）删 judge 段、`advance(.., None)`；两处 `let Advance::Push = step else { return .. }`（:2511、:2654）改 `match`，`FinalReport` 与 `Stop` **同处理**并加注释「`-p` / `--json` 不接收卷回合，与 TUI 有意不等价」；`follow_up_input`（:2348）传 `goal_prefix: None`；**保留** `advance_state.used` 两处推进（:2518、:2660），字段注释改成「诊断计数，不参与闸门」 | apps/coding-agent/src/main.rs | T25, T21, T29 |
| T32 | test | 回归：`-p` / `--json` 现有全部多回合用例逐字保持通过（2 回合 2 轮、`AllDone` 收场）；新增用例：`used` 被人为设成 999 时 `-p` 仍按清单逻辑收场（证明 `-p` 侧没有残留闸门）；变异：`main.rs` 的 `FinalReport` 分支改成 `continue` ⇒ 收卷/停止用例变红 | apps/coding-agent/src/main.rs | T31 |
| **W8** | | | | |
| T33 | impl | 重写 mutant 集为**设计 §8 的 8 条**：①判据 4 反转失效 ②收卷计入 200 ③无目标也设 200 ④目标块含回合序号 ⑤`GoalReached` 错报 `AllDone` ⑥stall 残留（`stall_digest` 被重新引入）⑦提醒不封顶 ⑧收卷回合被削工具；删除 4 个判官 mutant、3 个 stall mutant、1 个 cap mutant；`TARGETS` 去掉已删的 `goal_judge.rs`，加入 `goal_state.rs` 与 `app_loop.rs` | scripts/goal-mutation.py | T9, T21, T29 |
| T34 | test | 跑 `python3 scripts/goal-mutation.py`：8 条**全部变红**、跑完工作区**复原**（`git status --porcelain` 为空）、汇总表打印 8/8；变异：把某个 mutant 的 `expect_red` 改成一个不存在的用例名 ⇒ 脚本报「未命中」而非静默通过 | scripts/goal-mutation.py | T33 |

## 3. 关键路径

```
T1 → T9 → T21 → T25 → T27 → T29 → T31 → T33 → T34
```

**最短 9 步串行**。其余任务（T3/T5/T7/T13/T15/T19/T23/T11）可从 W1 起并行铺开，只在与关键路径
交汇处等待。

## 4. 验收

| 检查 | 命令 |
|---|---|
| 编译 | `./scripts/cargo-build.sh check --workspace` |
| 全量测试 | `./scripts/par-test.sh` |
| 无判官残留 | `rg -n 'goal_judge\|MAX_FOLLOW_UPS\|CapReached\|StallTracker\|stall_digest\|JUDGE_FAILURE_LIMIT\|JudgeUnreliable\|Stalled' apps crates` ⇒ 零命中 |
| `-p` 行为不变 | `cargo run -p ys-coding-agent -- -p "列出当前目录"` 正常；`rg 'SetGoal' apps/coding-agent/src/main.rs` ⇒ 零命中 |
| 变异 | `python3 scripts/goal-mutation.py` ⇒ 8/8 变红，工作区复原 |
| 净代码量 | `wc -l apps/coding-agent/src/goal.rs apps/coding-agent/src/turn_advance.rs` 对比基线（553 / 427）应分别降到约 270 / 290 |
