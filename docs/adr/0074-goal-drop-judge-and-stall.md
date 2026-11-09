# ADR-0074：`/goal` 砍掉判官与 stall，上限改为任务级 200 turn

- **状态**：已接受
- **日期**：2026-11-09
- **推翻了**：[ADR-0072](0072-goal-todo-verification.md) 的部分决策
- **相关**：[ADR-0041](0041-remove-completion-review-turn.md)、[ADR-0073](0073-loop-time-driven-without-criterion.md)
- **设计**：[`docs/arch/goal-command/design.md`](../arch/goal-command/design.md)

## 背景

ADR-0072 为跨回合推进设计了两道“验收”机制：一个**独立判官**（另起一次模型调用，核对模型
是否真的完成了）和一套 **stall 检测**（连续几轮清单不变且模型自称收尾 ⇒ 判为原地打转）。
两者都已落地，但实际使用中暴露出结构性问题。

目标 `/goal` 的需求要求“一句目标驱动、跨回合自动续跑”。在把目标接进这套判据之前，
必须先确认现有判据是否成立 —— 结论是**两者都不成立**，且不成立的原因是结构性的，
不是参数没调好。

## 决策

### D1：删除判官整层

删除 `goal_judge.rs`（622 行）、`goal_judge_call.rs`（388 行）、`judge_failure_delta`、
`judge_failures`、`JUDGE_FAILURE_LIMIT`、`StopCause::JudgeUnreliable`。

四条依据：

1. **ADR-0041 的前置条件从未满足。** ADR-0041 撤掉复核回合时留下硬要求：先实测
   「模型自称完工但实际未完工」的比例，再决定是否重新引入机制。该比例至今**没有数据**。
   判官在没有基线的情况下被引入，等于凭空增加一道开销。
2. **`Verdict` 无人消费。** 除 `turn_advance.rs:205` 的 `Unparseable` 分支外，判官的结论
   不改变任何执行路径 —— 它算出来的“达成/未达成”没有任何读者。
3. **证据窗口的粒度错了。** `evidence_window`（`goal_judge_call.rs:102`）只扫**最后一个
   `ToolUse` 之后**的消息。这是个 **round** 粒度的窗口，而它要裁决的是 **turn** 级的完成度。
   测试名 `evidence_window_takes_only_last_round` 就是这条事实。在 200 turn 的任务里，
   绝大多数证据落在视野之外 —— 判官看到的是最后一轮的尾巴，却被问整件事做完没有。
4. **成本从未计算。** 每次判官调用都要重发一段证据窗口的 prefill。与它带来的决策价值
   （0，见第 2 条）相比，这是纯亏损。

### D2：删除 stall 整层

删除判据 7、`STALL_LIMIT`、`StallTracker`、`stall_digest`、`StopCause::Stalled`。

三处缺陷，任一处都足以致命：

1. **`model_claims_done` 恒真。** 判据 2 只放行 `StopReason::Completed`，而
   `model_claims_done` 就是 `matches!(last_stop, StopReason::Completed)`
   （`goal.rs:150-161`）。判据 7 的 `&& model_claims_done` 因此是个恒真条件。
2. **信号源是模型自陈。** “清单变没变”是模型自己决定的，与 ADR-0072 D4 自己立的
   “助手自述不可当证据”直接矛盾。
3. **有目标时必然误杀。** 无清单是合法状态（目标只需要一句话），此时空集合指纹每回合恒等
   ⇒ 第 3 个 turn 必然 `Stalled`。

**不保留形态、不再修。** 一条恒真条件 + 一个自陈信号 + 一个必然误杀的分支，
没有可修的骨架。

### D3：上限改为任务级 200 turn

`MAX_FOLLOW_UPS = 8` 删除。唯一上限是 `GOAL_TURN_CAP = 200`，**只在有目标时生效**；
无目标时不设任何 turn 数上限。

理由：**轮数阀的形式错了。** 原来的 8 次追问限制的是“追问次数”，而用户真正关心的是
“这个任务最多能自己跑多久”。这两者量纲不同。200 turn 取初值，待实测后调整。
常量、不读配置（不引入 `config.json` 的 `goal` 段）。

**任务 = 一个 200 turn 预算的生命周期。** `/loop` 每个 tick 是一个新任务，重算 200 ——
因为 200 是针对**一个任务**的上限，不是针对会话。

### D4：200 turn 耗尽时起收卷回合

先跑一个**收卷回合**向用户汇报进展 / 状态 / 下一步，然后停。

- 收卷回合**不计入 200** —— 否则预算耗尽时恰好发不出这句话。
- 收卷回合**可调工具**（D-N）—— 否则它只能空口汇报。
- 工具表**原样保留** —— 工具表在请求最前面，一变就整段前缀缓存失效。

实现为 app 侧**循环外**的一次普通 `run_turn_chain` 调用，而非复用
`RunLimits::final_report_prompt`。后者是 `ys-loop` 里的**回合内**预算收卷，
触发判据 `deadline_reached()` 基于每回合重算的 deadline；要改成任务级就得让
`ys-loop` 认识“任务”概念，与分层相悖。

## 后果

### 变弱的

**目标的“达成”判定退化为“清单清空”。** 这是本 ADR 的主要代价。模型写一份简陋清单
即可宣告完成。候选方案中的另两种（模型输出 `GOAL: DONE` 令牌、新增 `update_goal` 工具）
本质**同样是自陈**，只是换了通道 —— 既然都要付自陈的代价，选最便宜的那个。
该代价无法用设计消除，只能用 prompt 约束压缩（要求目标块内先拆成可验收清单项）。

### 变好的

- 净删约 400 行（判官 1010 行 + stall 层，扣除新增的 `goal_state.rs`）。
- 每次回合少一次判官模型调用。
- 判据从 8 条降到 4 条，语义可被完整读出。

### 遗留的

1. **48h 兜底不存在。** `deadline` 在 `run_turn_chain` 入口按 `max_turn_duration()`
   每回合重算（`app_loop.rs:1705-1706`），是**回合级**阀，不是任务级。
   全仓 `chain_deadline` / `request_deadline` 零命中。故“48h = 最长自动化执行时间”
   目前是设计意图而非实现事实 —— 本 ADR 不修它，记为已知缺口。
2. **「自称完工但未完工」的比例仍是空白。** 在拿到实测数据之前，不应重新引入判官。
   这条是 ADR-0041 的原始要求，本 ADR 只是没有违背它，并未满足它。

## 备选方案为何未采纳

- **`update_goal` 工具 + `GoalStatus` 四态**：能表达“放弃/受阻”，但引入 9 个新概念
  （任务 / `TaskId` / `TaskBudget` / `BudgetLimit` / `BudgetScope` / `AdvanceCriterion` /
  `Spend` / `GoalStatus` / 收卷 turn），且工具常驻父表 ⇒ 不用 `/goal` 的用户每轮也付这份成本。
  与“不要过度设计”冲突。
- **把该工具放进 `ys-loop`（附 `GoalTurn` 凭证 + `CompletionGate` 闸门）**：状态机与预算
  落在 loop 层的思路有价值，但 `ys-loop` 生产代码至今零个 `Tool impl`，全部工具在
  `adapters/` —— 连长程任务核心的清单工具也在 `adapters/tools-todo`。在 loop 层装下
  第一个真实工具会破「最小核心」，故只吸收其“收卷不扣预算由结构保证”的意图，不吸收其落点。
- **整行令牌 `GOAL: DONE`**：成本最低（每 turn 约 4 output tok），但引入软契约：
  模型不写标记就得默认停（早停）或默认继续（烧满）。而它换来的“达成是目标的属性”
  这一优势，在 D-O 的任务级预算下收益有限。
