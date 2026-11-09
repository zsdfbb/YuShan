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

### 1.5 `crates/ys-protocol/src/goal.rs`（66 行，零 tokio）

`FollowUp{Push, Stop(StopCause)}`（`:16`）与 `StopCause{Aborted, NotContinuable, JudgeUnreliable, AllDone, NoPlan, CapReached, Stalled}`（`:27`）。
只有词汇，没有判定逻辑 —— 判定在 `goal.rs`、判官在 `goal_judge.rs`（`:5-8`）。

### 1.6 `apps/coding-agent/src/goal.rs`（554 行）

| 项 | 形态 | 行 |
|---|---|---|
| `MAX_FOLLOW_UPS` | `const u32 = 8` | `:40` |
| `STALL_LIMIT` | `const u32 = 3` | `:46` |
| `JUDGE_FAILURE_LIMIT` | `const u32 = 2` | `:49` |
| `stall_digest` | `fn(items: &[TodoItem]) -> u64` | `:62` |
| `StallTracker` | `new()` / `observe(digest) -> u32` / `unchanged_turns() -> u32` | `:89`、`:95`、`:102`、`:114` |
| `decide` | `fn(store, used: u32, last_stop: &StopReason, model_claims_done: bool, stall: &mut StallTracker, judge_failures: &mut u32) -> FollowUp` | `:130` |

八条判据**按顺序短路**（`goal.rs:7-21` 的模块头即规范）：①用户取消（`Aborted`）→ ②收场形态不可续
（`Truncated`/`Failed`/`MaxRounds`/`NoOutput` 及未知变体 ⇒ `NotContinuable`）→ ③判官连续不可靠
（`JudgeUnreliable`）→ ④无 open 项且 `revision > 0`（`AllDone`）→ ⑤清单为空（`NoPlan`）→ ⑥轮数用尽
（`CapReached`）→ ⑦集合指纹连续不变 **且** 模型自称收尾（`Stalled`）→ ⑧`Push`。逐条落点 `:142-198`。

`model_claims_done` 取 `last_stop == Completed`（**结构事实**，不扫正文：正文里「还没完成」随时会出现，
`goal.rs:122-125`）；它与 stall 取 **AND**，避免误杀「一轮里干了活但还没更新状态」。

### 1.7 `apps/coding-agent/src/goal_judge.rs`（611 行）

| 项 | 形态 | 行 |
|---|---|---|
| `MIN_QUOTE_CHARS` | `const usize = 12` | `:37` |
| `JudgeParse` | `Met{quote}` / `NotMet{reason}` / `Unparseable` | `:47` |
| `parse_judge_output` | `fn(raw: &str, evidence: &str, open_items: &[TodoItem]) -> JudgeParse` | `:67` |

`MET` 需三个条件齐备：整词 `VERDICT: MET`（`VERDICT_SCAN_LINES = 8` 行内找，`:40`）、每个 `OK` 项都带
≥12 字符逐字引用、引用归一化空白后能在证据窗口里子串命中且至少命中一条。任一不满足 ⇒ `Unparseable`
（判官不可靠，**不是**「未达成」，`:14`）。`BLOCKED` 归入 `NotMet` 并带 `blocked:` 前缀 —— 它是判官的合法
判断，计入可靠性会把「任务被阻塞」误记成「判官坏了」（`:63-66`）。

### 1.8 `apps/coding-agent/src/goal_judge_call.rs`（382 行）

| 项 | 形态 | 行 |
|---|---|---|
| `ARG_LIMIT` / `OUTPUT_LIMIT` | `200` / `1200` 字节 | `:32` / `:34` |
| `JUDGE_MAX_TOKENS` | `1024` | `:38` |
| `JUDGE_SYSTEM` | 判官系统提示词（输出契约三条硬要求） | `:44` |
| `evidence_window` | `fn(&[Message]) -> String` | `:103` |
| `evidence_text` | `fn(window: &str, harness_facts: &str) -> String` | `:144` |
| `todo_text` | `fn(open_items: &[TodoItem]) -> String` | `:152` |
| `judge_request` | `fn(evidence: &str, open_items: &[TodoItem]) -> ModelRequest`（`tools: []` + `thinking: off` + `max_tokens`） | `:172` |
| `call_judge` | `async fn(..)`，失败重试一次，返回**原始串** | `:200` |

窗口 = 最后一条 assistant 的 `ToolUse` + 其后全部 `ToolResult` + harness 事实，**不含**助手 `Text`
（`:11-13`）。按 `ContentBlock` 变体切，不按 `Role` —— 工具结果以 `Role::User` 进 session（`:15-16`）。
截断按 `char` 边界退让，避免切坏中文（`:69-80`）。

### 1.9 `apps/coding-agent/src/turn_advance.rs`（435 行）

| 项 | 形态 | 行 |
|---|---|---|
| `AdvanceState` | `{used: u32, stall: StallTracker, judge_failures: u32, git_follow_up_used: bool}` + `new()` | `:29`、`:43` |
| `advance` | `fn(store, state: &mut AdvanceState, outcome: &Result<RunResult, LoopError>, follow_up_on_commit: bool, head_before: Option<&str>, head_after: Option<String>) -> Advance` | `:59` |
| `harness_facts` | `fn(head_before, head_after, outcome, open_items: usize, elapsed: Option<Duration>, tool_calls: usize) -> String` | `:116` |
| `judge_failure_delta` | `async fn(..) -> u32`（`0` = 可用结论或无 open 项；`1` = 调用失败 / 解析不可靠 / 无模型） | `:163` |
| `Advance` | `Push` / `Stop(StopCause)` | `:211` |

`advance` 是**两条判据的唯一合并处**：`NoPlan` 时退回一期 `git HEAD` 判据（至多一次，靠
`git_follow_up_used`），有计划时一期不参与（`turn_advance.rs:3-16`）。

### 1.10 接线（两条入口）

| 入口 | 落点 |
|---|---|
| TUI | `app_loop.rs` 的 `Request::Prompt` 臂内循环：首个回合注入点 `:781-793`、`advance_state` `:841`、判官 + `advance` `:853-877`、追问前再注入 `:883-895`、`used += 1` `:919` |
| `-p` | `main.rs` 的 `run_print_mode`（`:2410`）外层 `loop`：`advance_state` `:2433`、`turn` 从 1 起逐轮递增 `:2437`、`advance_after_turn` `:2518`、`used += 1` 与 `turn += 1` `:2536-2537`、`follow_up_input` `:2541` |
| `--json` | `run_json_mode`（`:2558`）逐行同形：`advance_state` `:2581`、`advance_after_turn` `:2657`、`used += 1` 与 `turn += 1` `:2674-2675`、`follow_up_input` `:2677` |

`advance_after_turn`（`main.rs:2310`）与 `follow_up_input`（`main.rs:2356`）是两条入口共用的两份函数 ——
推进判据只允许有一份实现，否则「同一任务在 `-p` 与 `--json` 上跑出不同轮数」这种漂移无从发现（`main.rs:2550-2553`）。

`rx` 是按引用的 `&mut mpsc::Receiver<Envelope>`（`run_print_mode` 签名 `main.rs:2413`、`run_json_mode` 同形），
消费循环在 `tokio::join!` **之内** ⇒ 多回合不死锁。守它的用例是
`print_mode_two_turns_do_not_deadlock_at_small_capacity`（`main.rs:4649`，容量 2、10 秒超时、断言 `main_calls == 4`）。

### 1.11 工具表位次

父表 **15 元** = 基础 11 元（`read` `write` `edit` `bash` `grep` `find` `ls` `ocr` `view_image` `websearch` `webfetch`）
+ 追加段 `bash_bg` → `subagent_async` → `subagent_send` → `todo`；子表 **11 元**（general）/ **9 元**（explore，去掉 `write` / `edit`）。
`todo` 排在追加段**末位**：父 JSON 仍以子 JSON 去掉末尾 `]` 开头 ⇒ 子表命中父的前缀缓存。
锚点与位次断言在 `apps/coding-agent/src/toolset.rs:802-859`（`parent_specs.len() == 15` 在 `:838`，追加段顺序在 `:854-859`）。

### 1.12 变异测试

`scripts/goal-mutation.py` 共 **15** 条，覆盖判据顺序（1 / 2 / 3 / 6 / 7）、集合指纹（含终态项 / 大小写折叠 / 排序）、
判官（引用未命中 / 前缀匹配 / 覆盖面 / MISSING 归类）、注入（自愈分支 / revision 检查 / 空清单）。全部能被现有用例抓住。

## 2. 与设计的偏差

> 本节是本文档的主要价值：**实际是这样，理由是 X**。设计原文见
> `docs/arch/long-horizon-todo/design.md` 与 `docs/arch/cross-turn-advance/design.md` §5。

### 2.1 `decide` 接管 `stall.observe` 与 `judge_failures` 归零

**实际签名**（`goal.rs:130`）：`decide(store, used, &StopReason, model_claims_done, &mut StallTracker, &mut u32)`。
`stall.observe(stall_digest(&store.snapshot()))` 在函数**第一行**执行（`:140`），`judge_failures` 是 `&mut u32` 入参（判据 3 读它）。

理由：两条入口（TUI 与 `-p`/`--json`）各写一遍「观察指纹 + 维护失败计数」必然漂移，而漂移的表现是
「同一任务在两条入口上停机时机不同」—— 这种 bug 在单入口测试里不可见。放进 `decide` 后，
**每回合恰好观察一次**由函数自身保证；`observe` 放在最前面，是为了让计数与「本回合结束时的清单」
严格一一对应（闸门短路后不再观察，`:138-139`）。

归零仍在调用点（`app_loop.rs:835`、`main.rs:2331-2335`）：判官成功即 `judge_failures = 0`，
失败则 `saturating_add(delta)`（`main.rs:2331-2335`）—— 归零需要「判官结论」，而 `decide` 刻意不问判官（`goal.rs:4-5`）。

### 2.2 `GoalVerdict` 已删除

**实际形态**：不存在 `GoalVerdict` 类型（全仓零命中）。判官结论的唯一载体是
`goal_judge::JudgeParse{Met{quote}, NotMet{reason}, Unparseable}`（`goal_judge.rs:47`），
推进结论的唯一载体是 `ys_protocol::FollowUp`（`goal.rs:16`）。

理由：`GoalVerdict` 零消费者，且与 `JudgeParse` 表达同一件事 —— 两个类型并存时，转换函数会成为
第三处需要维护的语义映射。

### 2.3 `parse_judge_output` 多了 `open_items` 参数

**实际签名**（`goal_judge.rs:67`）：`parse_judge_output(raw, evidence, open_items: &[TodoItem])`。

第三参数落地 §5.7 的**覆盖面检查**：每个非终态项都要在 `ITEMS` 段标 `OK`，只靠 `EVIDENCE` 段的
一条总引用不能通关。守护用例 `met_must_cover_every_open_item`（`goal_judge.rs:555`）与
`overall_evidence_does_not_skip_coverage`（`:593`）；变异测试「漏核未完成项也放行」抓这一条
（`scripts/goal-mutation.py:235`）。

### 2.4 判据 4「无 open 项」与「全部 Completed」在 `apply` 硬规则下语义等价

**实际形态**：判据 4 写的是 `!store.has_open_items() && store.revision() > 0`（`goal.rs:175-177`）。

`TodoStore::apply` 的第二条硬规则是「非空且**全部终态** ⇒ 清空 + 删文件」（`store.rs:11-13`），
而 `Cancelled` 是终止态（`item.rs:10-15`）。于是「清单非空且无 open 项」这个状态在 store 里
**不可能存在** —— 快照要么有 open 项，要么为空。所以「无 open 项」与「全部 `Completed`」在
`decide` 看到的输入上无法区分，**这一条做不出变异测试**。

守护者因此不在 `decide`，而在 `store.rs`：`all_terminal_clears_and_deletes_file`（`:522`）、
`single_shot_all_terminal_clears`（`:554`）、`dropping_terminal_items_is_allowed`（`:484`）。
`revision > 0` 用来区分「刚干完」与「根本没计划」—— 后者归判据 5（`goal.rs:173-174`）。

### 2.5 `contains_marker` 同时扫 `ToolResult`

**实际形态**（`todo_inject.rs:93-99`）：`ContentBlock::Text` 与 `ContentBlock::ToolResult` 都查 `TODO_MARKER`；
其余变体返回 `false`。

理由：`todo` 工具的回喂块与 `injection_text` 里的渲染**同源**（同 `items`、同 `revision`），
它还在历史里就说明模型已经看得见这一版清单 —— 只扫 `Text` 会在「模型刚写完清单」的那个回合边界
多注一份完全相同的块。变异测试「去掉 revision 检查」（`scripts/goal-mutation.py:282`）与
「空清单也注入」（`:297`）守这条判据的两个方向。

### 2.6 `follow_up_text`：输入由推进理由决定，两个入口共用

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

### 2.7 `-p`/`--json` 的 `turn` 号逐轮递增

**实际形态**：`run_print_mode` 里 `let mut turn: u32 = 1;`（`main.rs:2437`），每轮 `turn = turn.saturating_add(1)`
（`:2537`）；`run_json_mode` 同形（`:2675`）。`Envelope.turn` 与 debug 的 `turn=` 都取它。

理由：多回合链里两个回合的事件必须可区分 —— `--json` 的每行都带 `turn` 号，消费方据此切分回合；
恒为 1 会让多回合输出看起来像一个超长回合。

### 2.8 一期判据在 `-p`/`--json` 上按开关生效，清单判据无开关

`advance` 的 `follow_up_on_commit` 入参只管一期 `git HEAD` 判据（`turn_advance.rs:59-65`）；
清单判据**没有开关** —— 开关就是「有没有计划」（`NoPlan` 即退回一期）。这条与设计一致，列在此处是因为
它决定了 `-p` 的默认行为：模型不写清单 + 开关关 ⇒ 跑完即退出。

## 3. 已知取舍与退化

| 项 | 现状 | 代价 | 依据 |
|---|---|---|---|
| 判官只读最后一轮 | `evidence_window` 取最后一条 assistant 的 `ToolUse` 及其后的 `ToolResult` | 判官输入 **O(1)**，与总轮数无关；**跨轮证据不可见**，只能核本轮可验的项 | `goal_judge_call.rs:18-21` |
| `elapsed` 未知 | `harness_facts` 写 `unavailable`，不写 `0ms` | 判官看到的是「不知道」，不是「零耗时」—— 后者会让他采信「一秒内完成五项」 | `turn_advance.rs:142-145` |
| 判官调用不落盘 | `call_judge` 只返回原始串，不写 session、不写 debug 事件 | 判官输出事后不可回溯；换取的是判官不污染会话、不占上下文 | ADR-0072 D8、`goal_judge_call.rs:4`、`:6-7` |
| 落盘尽力而为 | `apply` 写失败不回滚内存 | 重启后可能少一份清单；换取「一次写失败不丢模型刚写好的计划」 | `store.rs:18-22` |
| 降级提示不打印 | `take_degraded_hint` 只**产出**，去处由接线器决定 | crate 边界强制（`tools-todo` 绝不 `print`） | `lib.rs:29-31` |
| stall 只看清单集合 | `stall_digest` 哈希非终态项的 `(内容, 状态)`，排序 + 大小写折叠 | 反复跑同一条命令但清单不变 ⇒ 判为打转；换取「不受日志噪声影响」（`3 failed (2.1s)` 与 `(2.3s)` 会让哈希输出漏检） | `goal.rs:23-27` |
| `stall` 需 AND 模型自陈 | `unchanged_turns() >= STALL_LIMIT && model_claims_done` | 模型还在干活时不误杀；代价是「模型死不承认收尾」时靠判据 6 兜底 | `goal.rs:191-194` |

## 4. 指针索引

| 事实 | 指针 |
|---|---|
| 装配缝类型与排序 | `crates/ys-tool/src/plugin.rs:27-300` |
| 清单真值与两条硬规则 | `adapters/tools-todo/src/store.rs:114-291` |
| 注入判据与渲染 | `apps/coding-agent/src/todo_inject.rs:22-66`、`adapters/tools-todo/src/render.rs:35` |
| 推进八条判据 | `apps/coding-agent/src/goal.rs:130-198` |
| 判官解析与引用校验 | `apps/coding-agent/src/goal_judge.rs:37-67` |
| 证据窗口与判官调用 | `apps/coding-agent/src/goal_judge_call.rs:103-210` |
| 两条判据合并 | `apps/coding-agent/src/turn_advance.rs:59-113` |
| 两条入口接线 | `apps/coding-agent/src/app_loop.rs:781-919`、`apps/coding-agent/src/main.rs:2310-2677` |
| 工具表位次（15 / 11） | `apps/coding-agent/src/toolset.rs:802-859`、`apps/coding-agent/src/assembly.rs:224-239` |
| 多回合不死锁 | `apps/coding-agent/src/main.rs:4649` |
| 变异测试清单 | `scripts/goal-mutation.py:60-310` |
