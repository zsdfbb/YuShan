# todo 清单 + 目标验收层 — As-built

> 性质：**实际建成记录**（as-built）。只写当前代码的样子。
> 决策依据：[ADR-0024](../../adr/0024-long-horizon-todo.md)（清单层）、
> [ADR-0072](../../adr/0072-goal-todo-verification.md)（验收层）、
> [ADR-0021](../../adr/0021-tool-plugin-assembly-seam.md)（装配缝）。
> 设计输入：[`docs/arch/long-horizon-todo/design.md`](../arch/long-horizon-todo/design.md)、
> [`docs/arch/cross-turn-advance/design.md`](../arch/cross-turn-advance/design.md) §5。
> 执行计划：`docs/exec-plans/2026-11-06-toolplugin-todo-verification.md`（W0–W9）。
> 日期：2026-11-06

## 一句话

计划清单的真值**不在会话里**：`<stem>.todo.json` 是唯一真值，`todo` 工具是模型唯一的写入面；
回合边界按「状态变了 ∨ 历史里找不到标记」把清单渲染成一条 `Role::Harness` 消息追加到 `messages` 尾部；
回合结束时**一份**判据（`turn_advance::advance`）决定「要不要再来一轮」，TUI 与 `-p`/`--json` 两条入口共用它。

## 1. 模块与公开 API

### 1.1 `crates/ys-tool/src/plugin.rs`（装配缝，495 行）

纯数据 + 纯函数：无 IO、无 tokio、无运行时回调语义（不建 hook 系统，ADR-0014）。

| 项 | 形态 | 行 |
|---|---|---|
| `ToolPlugin` | `{ order: u16, tools: Vec<Box<dyn Tool>>, prompts: Vec<ToolPrompt> }` | `:27` |
| `ToolPlugin::new` | `fn new(order: u16, tools: Vec<Box<dyn Tool>>) -> Self` | `:41` |
| `ToolPlugin::with_prompts` | `fn with_prompts(self, prompts: Vec<ToolPrompt>) -> Self` | `:50` |
| `ToolPlugin::guideline_entries` | `fn guideline_entries(&self) -> impl Iterator<Item = (&str, &GuidelineBlock)> + '_` | `:58` |
| `ToolPrompt` | `{ tool: String, snippet: String, .. }` + `new` / `with_guidelines` / `guideline_entries` | `:78`、`:94`、`:109` |
| `GuidelineBlock` | `{ slot: u16, gate: Gate, scope: Scope, lines: Vec<String> }` + `new` | `:121`、`:137` |
| `GuidelineBlock::is_active` | `fn is_active(&self, owner: &str, tools: &[&str], narrate: bool) -> bool` | `:155` |
| `Gate` | `ThisTool` / `AnyOf(&'static [&'static str])`；`matches(owner, tools)` | `:162`、`:172` |
| `Scope` | `Always` / `MainOnly`；`allows(narrate)` | `:182`、`:194` |
| `sort_plugins` | `fn(Vec<ToolPlugin>) -> Result<Vec<ToolPlugin>, ToolError>` | `:206` |
| `expand_guidelines` | `fn(blocks, tools: &[&str], narrate: bool) -> Result<Vec<String>, ToolError>` | `:252` |

**两条彼此独立的序**（缓存前缀稳定性的全部要点，`plugin.rs:10-17`）：工具序取 `ToolPlugin::order`
升序，guideline 序取每块自带的全局 `slot`；两者不能合并 —— guideline 从 `edit` 开始而工具表从
`read` 开始，且 `read` 的三块被 `grep` 的块插在中间。

**声明期不变量**由 `sort_plugins` / `expand_guidelines` 校验（违反即 `Err`）：工具名跨插件唯一、
插件 `order` 唯一、guideline `slot` 全局唯一。`expand_guidelines` 的 `slot` 校验在门控过滤**之前**
做 —— 被门控掉的块今天不渲染，门控条件一变就会暴露（`plugin.rs:249-251`）。

### 1.2 `adapters/tools-todo/`（清单层，6 文件）

`lib.rs` 的模块分工表在 `:5-10`，重导出在 `:38-42`；装配入口 `todo_tool(store: Arc<TodoStore>) -> Box<dyn Tool>` 在 `:50`。

| 模块 | 内容 | 关键行 |
|---|---|---|
| `item.rs` | `TodoStatus{Pending, InProgress, Completed, Cancelled}`（`#[non_exhaustive]`，`Cancelled` 是**终止态**）；`TodoItem{content, status}`；`normalize_content` | `:15`、`:50`、`:70` |
| `store.rs` | `TodoStore` + `TODO_FILE_SUFFIX = ".todo.json"` | `:114`、`:37` |
| `render.rs` | `render_block(items, revision) -> String`；`MAX_BLOCK_TOKENS = 600`、`MAX_BLOCK_BYTES = 2400`、`MAX_CONTENT_CHARS = 240` | `:35`、`:19`、`:22`、`:27` |
| `tool.rs` | `TodoTool::new(store: Arc<TodoStore>)`，工具名 `todo` | `:30`、`:39` |

`TodoStore` 的公开面（`store.rs`）：

| 方法 | 签名 | 行 |
|---|---|---|
| `open` | `fn open(session_path: Option<&Path>) -> Arc<Self>` | `:137` |
| `snapshot` | `fn snapshot(&self) -> Vec<TodoItem>` | `:160` |
| `revision` | `fn revision(&self) -> u64` | `:165` |
| `has_open_items` | `fn has_open_items(&self) -> bool` | `:170` |
| `take_degraded_hint` | `fn take_degraded_hint(&self) -> Option<String>` | `:180` |
| `apply` | `fn apply(&self, incoming: Vec<TodoItem>) -> Result<ApplyOutcome, TodoError>` | `:192` |
| `retarget` | `fn retarget(&self, session_path: Option<&Path>)` | `:264` |
| `path` | `fn path(&self) -> Option<PathBuf>` | `:281` |

- `ApplyOutcome{Updated, ClearedAllDone}`（`:51`）、`TodoError{WouldDropOpenItems, EmptyContent}`（`:61`）。
- **两条硬规则都在 `apply` 里**：拒绝丢弃既有非终态项；非空且全部终态 ⇒ 清空 + 删文件。
  判据是「无 open 项」而非「全部 `Completed`」—— 被 `Cancelled` 的项同样终结，否则一个放弃的项会永久卡住收场（`store.rs:11-13`）。
- 落盘**尽力而为**：写失败不回滚内存（内存是本进程真值，磁盘只是下次启动的起点，`store.rs:18-22`）。
- `TodoTool` **永不声明** `is_concurrency_safe`：清单是共享状态，打开它等于允许两个 `apply` 交错（`tool.rs:9-12`）。

### 1.3 `apps/coding-agent/src/tool_prompts.rs`（390 行）

14 个 `ToolPrompt` 声明（`builtin_prompts()`，`:122`），覆盖 `builtin_plugins` 的 11 条 + 父表追加段 3 个
（`bash_bg` / `subagent_async` / `subagent_send`）。`slot` 命名常量 25 个，最后一个 `TODO_MAINTAIN = 24`（`:84`），
`todo` 的块在 `:396-402`。

### 1.4 `apps/coding-agent/src/todo_inject.rs`（205 行）

| 项 | 形态 | 行 |
|---|---|---|
| `TODO_MARKER` | `const &str = "<todo_list"` | `:22` |
| `should_inject` | `fn(store: &TodoStore, last_injected_rev: Option<u64>, session: &[Message]) -> bool` | `:29` |
| `injection_text` | `fn(store: &TodoStore) -> String` | `:47` |
| `contains_marker` | `fn(&Message) -> bool`（私有） | `:60` |

判据严格是 `非空 && (revision 变 ∨ 会话里找不到 TODO_MARKER)`；`last_injected_rev` 只活在本进程，
重启后靠「找不到标记就补」兜底（`:24-28`）。自愈覆盖压缩：压缩吃掉块后标记随之消失，下个回合边界自动补，
因此**不监听** `ContextCompacted`（手敲 `/compact` 不发那条事件，`:5-9`）。

### 1.5 `crates/ys-protocol/src/goal.rs`（107 行，零 tokio）

`FollowUp{Push, Stop(StopCause)}`（`:20`）与
`StopCause{NoPlan, AllDone, GoalReached, Aborted, NotContinuable}`（`:28`）。
`StopCause::text()`（`:48`）给出面向 UI 的中文文案；它是穷尽 `match`、**不留 `_` 兜底臂**，
新增变体即编译失败。只有词汇，没有判定逻辑 —— 判定在 `goal.rs`。

`Request::SetGoal { text: Option<String> }`（`crates/ys-protocol/src/request.rs:134`）是 harness 的
**配置动作**：只改 app 侧的目标状态，**不起回合**（与 `Prompt` 的根本区别，`:129`）。

### 1.6 `apps/coding-agent/src/goal.rs`（321 行）

| 项 | 形态 | 行 |
|---|---|---|
| `decide` | `fn(store: &TodoStore, used: u32, last_stop: &StopReason, goal: Option<&GoalState>) -> FollowUp` | `:49` |

五条判据**按顺序短路**（模块头 `:7-35` 即规范）：
①用户取消（`Aborted`，`:58`）→ ②收场形态不可续（`Truncated`/`Failed`/`MaxRounds`/`NoOutput`
及未知变体 ⇒ `NotContinuable`，`:64`）→ ③无 open 项且 `revision > 0`：有目标报 `GoalReached`、
无目标报 `AllDone`（`:79`）→ ④清单从未写过：有目标 ⇒ `Push`、无目标 ⇒ `NoPlan`（`:88`）→ ⑤其余 `Push`。

`used` 是**诊断计数，不参与任何闸门**（`:47-54` 的 `let _ = used;`）：结论与它无关。
守护这一点的反向用例是 `used_never_changes_the_verdict`（`:297`）。

判据 3 的「无 open 项」不是「全部 `Completed`」：被放弃（`cancelled`）的项同样终结。
`revision > 0` 用来区分「刚干完」与「根本没计划」—— `TodoStore::apply` 在无 open 项时清空列表，
两种情形在快照上长得一样，只能靠「写过没有」分辨。

判据 4 是本设计唯一的「目标不会白设」保证：无清单时把它报成 `NoPlan` 会让目标被**静默忽略**，
因此有目标时走 `Push`，代价是模型若坚持不拆清单会一路跑到回合预算耗尽。

### 1.7 `apps/coding-agent/src/goal_state.rs`（215 行）

| 项 | 形态 | 行 |
|---|---|---|
| `GOAL_TURN_CAP` | `const u32 = 200` —— 唯一上限（D-J/D-K），常量、不读配置（D-D） | `:22` |
| `NO_PLAN_REMIND_LIMIT` | `const u32 = 5` —— 连续无清单提醒的上限 | `:25` |
| `GoalState` | `{ text: String, turns: u32, plan_reminders: u32 }` | `:29` |
| `new` / `turns` / `consume` / `exhausted` | 生命周期与预算 | `:35` / `:43` / `:48` / `:53` |
| `marker_text` | 注入块的 `Role::Harness` 文本 | `:65` |
| `wrap_up_text` | 收卷回合的输入文本 | `:85` |
| `plan_remind` | 无清单提醒，超过上限返 `None` | `:106` |

三条契约（模块头 `:1-16` 即规范）：

- **生命周期 = 一个任务**（D-O）：「`/goal` 设下、`/goal clear` 作废，**不落盘** —— `/resume` 之后
  目标是新任务的空白起点。
- **回合计数不含收卷回合**（D-N）：`consume` 只由追问循环推进；收卷回合在循环之外跑，跑完不再 `consume`。
- **注入块字节只由目标原文决定**：`marker_text()` 不含回合序号、也不含剩余预算。每回合都变的话
  每回合都是一笔全价尾字节（破前缀缓存），而模型知道这些也**不能**改变行为 —— 结局由 harness 判定。

达成判定靠「清单清空」⇒ 目标块必须把「先拆成可验收的清单项、每完成一项就更新清单」写进去：
这是 R1（“达成”退化为“清单清空”）唯一的非技术缓解。

### 1.8 `apps/coding-agent/src/turn_advance.rs`（478 行）

| 项 | 形态 | 行 |
|---|---|---|
| `AdvanceState` | `{used: u32, git_follow_up_used: bool}` + `new()` | `:28`、`:41` |
| `advance` | `fn(store, state, outcome, follow_up_on_commit, head_before, head_after, goal: Option<&GoalState>) -> Advance` | `:58` |
| `Advance` | `Push` / `Stop(StopCause)` / `FinalReport` | `:111` |

`Advance::FinalReport` 与 `Advance::Stop` **互斥**，调用方必须当不同动作处理：`Stop` = 直接收场；
`FinalReport` = 再跑一个**不计入预算**的回合后再收场。它只在 `FollowUp::Push` 分支内部判定 ——
放在 `match decision` 之前会把 `Esc` 与清单清空也改写成 `FinalReport`。

`advance` 是**两条判据的唯一合并处**：`NoPlan` 时退回一期 `git HEAD` 判据（至多一次，靠
`git_follow_up_used`），有计划时一期不参与（`turn_advance.rs:7-16`）。

`used` 与 `AdvanceState.used` 都是**诊断计数**：上限判据在 `goal.exhausted()`，不在这里。

### 1.9 接线（两条入口）

| 入口 | 落点 |
|---|---|
| TUI | `app_loop.rs` 的 `Request::Prompt` 臂内循环：目标块注入 `:783-790`、`advance` `:862-870`、`GoalReached` 快照 `:876-878`、`consume()` 单点 `:888-890`、无清单提醒 `:894-902`；收卷回合在 `loop` **之外** `:961` |
| `Request::SetGoal` | `app_loop.rs:1004`：只改 `goal` 变量、**不起回合**（确认行走 `Outbound::Output`） |
| `-p` | `main.rs` 的 `run_print_mode` 外层 `loop`：`advance` 调用 `:2320`，`goal` 传 `None` |
| `--json` | `run_json_mode` 逐行同形，同样传 `None` |

`advance_after_turn` 与 `follow_up_input` 是两条入口共用的两份函数 —— 推进判据只允许有
一份实现，否则「同一任务在 `-p` 与 `--json` 上跑出不同轮数」这种漂移无从发现。

**`/goal` 只接 TUI**：`-p` / `--json` 把 `goal` 位置传 `None`，行为逐字不变（没有 200 上限、不会收卷）。

`rx` 是按引用的 `&mut mpsc::Receiver<Envelope>`，消费循环在 `tokio::join!` **之内** ⇒ 多回合不死锁。守它的用例是
`print_mode_two_turns_do_not_deadlock_at_small_capacity`（`main.rs:4649`，容量 2、10 秒超时、断言 `main_calls == 4`）。

### 1.10 工具表位次

父表 **15 元** = 基础 11 元（`read` `write` `edit` `bash` `grep` `find` `ls` `ocr` `view_image` `websearch` `webfetch`）
+ 追加段 `bash_bg` → `subagent_async` → `subagent_send` → `todo`；子表 **11 元**（general）/ **9 元**（explore，去掉 `write` / `edit`）。
`todo` 排在追加段**末位**：父 JSON 仍以子 JSON 去掉末尾 `]` 开头 ⇒ 子表命中父的前缀缓存。
锚点与位次断言在 `apps/coding-agent/src/toolset.rs:802-859`（`parent_specs.len() == 15` 在 `:838`，追加段顺序在 `:854-859`）。

### 1.11 变异测试

`scripts/goal-mutation.py` 共 **11** 条：判据（空清单反转 / `GoalReached` 错报 `AllDone`）、
预算（收卷前先 `consume` / 无目标也设上限 / 目标块含回合序号）、提醒（去掉上限）、
注入（自愈分支 / revision / 空清单）。**11/11 全红**。

其中两条与设计 §8 的原文**不是一一对应**，归档时如实标注：

| 设计 §8 条目 | 实际注入 | 为何不同 |
|---|---|---|
| ⑥ stall 残留 | `used >= 4` 且清单无 open 项 ⇒ `Stop(NotContinuable)` | stall 检测（集合指纹 + 连续 N 回合不变）已整层删除，**无逐字可复原的注入点**。注入了它最危险的形态：用回合数误杀正常收场 |
| ⑧ 收卷削工具 | 收卷输入丢掉目标原文（`text = ""`） | 工具表由 `agent` 内部持有，`run_turn_chain` 参数里没有工具表，追问与收卷传的是同一个 `agent` —— **从调用点改不动工具面**。故换成同一目的下可执行的变异 |

另外三条（自愈分支 / revision / 空清单）守的是注入判据本身，不在设计 §8 的 8 条里。

## 2. 与设计的偏差

> 本节是本文档的主要价值：**实际是这样，理由是 X**。设计原文见
> `docs/arch/long-horizon-todo/design.md` 与 `docs/arch/cross-turn-advance/design.md` §5。

### 2.1 判据 3「无 open 项」与「全部 Completed」在 `apply` 硬规则下语义等价

**实际形态**：判据 3 写的是 `!store.has_open_items() && store.revision() > 0`（`goal.rs:81`）。

`TodoStore::apply` 的第二条硬规则是「非空且**全部终态** ⇒ 清空 + 删文件」（`store.rs:12`），
而 `Cancelled` 是终止态。于是「清单非空且无 open 项」这个状态在 store 里
**不可能存在** —— 快照要么有 open 项，要么为空。所以「无 open 项」与「全部 `Completed`」在
`decide` 看到的输入上无法区分，**这一条做不出变异测试**。

`revision > 0` 用来区分「刚干完」与「根本没计划」—— 后者归判据 4（`goal.rs:84`）。

### 2.2 判官与 stall 整层不存在

**实际形态**：全仓没有 `goal_judge` / `goal_judge_call` / `JudgeParse` / `JudgeUnreliable` /
`stall_digest` / `StallTracker` / `Stalled` / `MAX_FOLLOW_UPS` / `CapReached`（零命中）。

推进结论的唯一载体是 `ys_protocol::FollowUp`（`goal.rs:20`）；停机分类的唯一载体是
`StopCause` 的五个变体（`crates/ys-protocol/src/goal.rs:28`）。

理由与取舍见 [ADR-0074](../../adr/0074-goal-drop-judge-and-stall.md)：判官的比例从未测过，
`Verdict` 除 `Unparseable` 外无人消费；stall 的 `model_claims_done` 恒真、空清单指纹恒等
⇒ 第 3 轮必然误杀。

### 2.3 回合数上限从 `decide` 移出

**实际形态**：`decide` 没有 `CapReached` 判据；唯一上限是 `advance` 里的
`goal.exhausted()`（`turn_advance.rs:58-110`），且**只在 `FollowUp::Push` 分支内部判**。

理由：`FollowUp` 只有两态，而预算耗尽需要表达第三种动作（「停下，但先收一个卷」）。
把上限放进 `decide` 会让 `Esc` 与清单清空也被改写成收卷 —— 所以 `Advance` 是
三态（`turn_advance.rs:111`）。守护用例 `stop_wins_over_exhausted`（`:412`）。

### 2.4 `contains_marker` 同时扫 `ToolResult`

**实际形态**（`todo_inject.rs:93-99`）：`ContentBlock::Text` 与 `ContentBlock::ToolResult` 都查 `TODO_MARKER`；
其余变体返回 `false`。

理由：`todo` 工具的回喂块与 `injection_text` 里的渲染**同源**（同 `items`、同 `revision`），
它还在历史里就说明模型已经看得见这一版清单 —— 只扫 `Text` 会在「模型刚写完清单」的那个回合边界
多注一份完全相同的块。变异测试「去掉 revision 检查」（`scripts/goal-mutation.py:282`）与
「空清单也注入」（`:297`）守这条判据的两个方向。

### 2.5 `follow_up_text`：输入由推进理由决定，两个入口共用

**实际形态**（`todo_inject.rs:72-86`），三条分支：

1. 清单为空 ⇒ 回落一期的 `post_turn::harness_input_text()` 文案（它说的是「git 历史变了」，**不能**用于
   清单驱动的情形，否则模型会去查一个不存在的提交）；
2. 清单非空且 `should_inject` 为真 ⇒ 注入块**本身就是**输入（它已把「未完成项优先」摆在末尾，
   再叠一句「请继续」只是重复计费），并更新 `last_injected_rev`；
3. 清单非空但版本未变 ⇒ 只发一句短推进句「继续推进清单里的未完成项；全部完成后收尾。」

理由：避免把**同一份**清单在尾部堆成 N 份副本。三条分支都是 `Role::Harness` —— 用户没说过这些话，
记成用户说的会污染「我是谁、我说过什么」。

**判据只此一份，两个入口都调它**：`-p`/`--json` 经 `main.rs:2348` 的 `follow_up_input()`（薄包装），
TUI 在 `app_loop.rs:861-870` 直接构造 `Message`。追问回合**不再**「先 append 一块清单、再另传一份
占位输入」—— 那会让模型同时看到清单块与「git 历史变了」两句互相矛盾的话，而后一句在清单驱动的
情形下是**假前提**（追问由清单推动，`HEAD` 根本未变）。守护用例
`follow_up_text_never_mentions_git_when_plan_exists`（`todo_inject.rs:246`）与
`follow_up_text_uses_git_wording_only_without_plan`（`todo_inject.rs:270`）；变异「第一支无条件返回 git 文案」
必须让它变红。

### 2.6 `-p`/`--json` 的 `turn` 号逐轮递增

**实际形态**：`run_print_mode` 里 `let mut turn: u32 = 1;`（`main.rs:2437`），每轮 `turn = turn.saturating_add(1)`
（`:2537`）；`run_json_mode` 同形（`:2675`）。`Envelope.turn` 与 debug 的 `turn=` 都取它。

理由：多回合链里两个回合的事件必须可区分 —— `--json` 的每行都带 `turn` 号，消费方据此切分回合；
恒为 1 会让多回合输出看起来像一个超长回合。

### 2.7 一期判据在 `-p`/`--json` 上按开关生效，清单判据无开关

`advance` 的 `follow_up_on_commit` 入参只管一期 `git HEAD` 判据；
清单判据**没有开关** —— 开关就是「有没有计划」（`NoPlan` 即退回一期）。这条决定了 `-p` 的默认行为：
模型不写清单 + 开关关 ⇒ 跑完即退出。

## 3. 已知取舍与退化

| 项 | 现状 | 代价 | 依据 |
|---|---|---|---|
| 达成靠模型自陈 | 判据是「清单清空」，模型只要写三行简陋清单就能宣告完成 | 无技术手段区分「真干完」与「把清单划掉」；缓解只有目标块里那句「写到指着它就能说做完了」+ 达成时的清单快照 | `goal.rs:23-28`、`app_loop.rs:1561-1581` |
| 无清单时一路跑到底 | 有目标 + 模型坚持不拆清单 ⇒ `Push` 直到 200 回合耗尽 | 换取「目标不会因为模型没调 `todo` 就被静默忽略」；代价是可能白烧一整份预算 | `goal.rs:23-28` |
| 无清单提醒只有 5 次 | `plan_remind` 超上限返 `None` | 第 6 次起不再提，模型可能一直不拆清单；换取「同一句话不重复进上下文」 | `goal_state.rs:25`、`:106` |
| 目标不落盘 | `/resume` 后目标是空白起点 | 重启后要重设；换取「生命周期 = 一个任务」的简单契约 | `goal_state.rs:1-16` |
| `/goal` 只接 TUI | `-p` / `--json` 传 `None` | 批量模式没有目标与 200 上限；换取「两条入口行为逐字不变」 | `main.rs:2320` |
| 落盘尽力而为 | `apply` 写失败不回滚内存 | 重启后可能少一份清单；换取「一次写失败不丢模型刚写好的计划」 | `store.rs:18-22` |
| 降级提示不打印 | `take_degraded_hint` 只**产出**，去处由接线器决定 | crate 边界强制（`tools-todo` 绝不 `print`） | `lib.rs:29-31` |

## 4. 指针索引

| 事实 | 指针 |
|---|---|
| 装配缝类型与排序 | `crates/ys-tool/src/plugin.rs:27-300` |
| 清单真值与两条硬规则 | `adapters/tools-todo/src/store.rs:114-291` |
| 注入判据与渲染 | `apps/coding-agent/src/todo_inject.rs:22-66`、`adapters/tools-todo/src/render.rs:35` |
| 推进五条判据 | `apps/coding-agent/src/goal.rs:49-105` |
| 目标状态与三段文案 | `apps/coding-agent/src/goal_state.rs:22-115` |
| 停机分类与文案 | `crates/ys-protocol/src/goal.rs:28-57` |
| 三态推进 | `apps/coding-agent/src/turn_advance.rs:58-125` |
| `/goal` 命令解析 | `crates/ys-tui-coding/src/commands.rs` |
| 两条入口接线 | `apps/coding-agent/src/app_loop.rs:783-1004`、`apps/coding-agent/src/main.rs:2320` |
| 工具表位次（15 / 11） | `apps/coding-agent/src/toolset.rs:802-859`、`apps/coding-agent/src/assembly.rs:224-239` |
| 多回合不死锁 | `apps/coding-agent/src/main.rs:4649` |
| 变异测试清单 | `scripts/goal-mutation.py` |
