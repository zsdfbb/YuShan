# `/goal` 命令 — 架构上下文

> 探索日期：2026-11-09；核实基线 `efa0b7c`（工作区干净）。
> 需求来源：ADR-0072 决策 D1「新增 `/goal <目标>` 与 `--goal <目标>`」。
> 本文只记**现状与约束**，不含方案选型。

## 概述

让用户用**一句话目标**（`/goal 修好登录超时`）驱动一个**任务**内的自动续跑：agent 边干边维护
todo 清单，回合结束后若清单还有 open 项就自动追加一个追问回合，直到清单清空、撞上 turn 上限、
或被取消。**不调独立判官核对证据**（D-P）。

**结论先行：判据已实现，命令入口未实现。** `/goal` 与 `--goal` 在全仓
**零命中**（`apps/` / `crates/` 的 CLI 参数表、TUI 命令表、`Request` 枚举里都没有），
但 `decide` 判据与八条停机闸门已经落地并带测试。

## 已澄清的决策（2026-11-09 用户答复）

| # | 决策 | 含义 |
|---|---|---|
| **D-A 独立验收对象** | **目标是独立的验收对象**，不是「起首个回合的提示词」 | 目标文本必须能进入推进决策，而不是只核对 todo 清单项。今天 `decide` 在「模型没写清单」时直接 `Stop(NoPlan)` ⇒ 目标会被静默忽略，这条路径必须改。⚠️ D-P 删除判官后，「怎么核对」的方法待重新设计（见 Q1） |
| **D-B 进展定期展示** | 允许显示状态，但**不进状态行**；改为**定期把工作进展写进 transcript** | 不做 `LoopView` 那种状态栏投影。每个回合结束后发一条 `Outbound::Output`，说清「第几 turn / 为什么继续或停下」 |
| **D-C 先做 TUI** | 本轮只做 `/goal`（TUI） | `--goal`、`-p` 退出码 `2` 不进本轮范围 |
| **D-D 不配置化闸门** | `MAX_FOLLOW_UPS` / `JUDGE_FAILURE_LIMIT` 保持常量（`STALL_LIMIT` 随 stall 一起删，见 D-F） | 不做 `config.json` 的 `goal` 段（Q4 取消） |
| **D-E 不做跨机制抑制** | 不实现 D12（目标期间压掉一期 git 追问 / `/loop` tick） | 各机制各走各的路，不新增耦合逻辑（Q5 取消） |
| **D-F 干掉 stall** | 删除判据 7、`STALL_LIMIT`、`StallTracker`、`stall_digest`，以及 `StopCause::Stalled` | 理由见 F2：恒真的冗余条件、信号源是模型自陈、在 D-A 下必然误杀。**不保留形态、不再修** |
| **D-G 上限重新设计** | 上限**不按轮数**收场 —— 轮数阀是错误的形式（见 F1），需改成**任务级预算** | 具体形式（任务级墙钟 / 任务级花费 / 两者）留给 `arch-design` 定；本文件只定「不再用轮数当唯一硬阀」 |
| **D-H 上限的两个维度** | **48 h** = 一个任务的最长自动化执行时间，**以用户是否介入为标准**；**200 轮** = 自动化任务可自动执行的轮次 | 两者是**不同量纲的两把阀**。⚠️ **具体取值已被 D-J 修正**：只保留 200 turn 一个上限，48 h 暂不作为任务级阀（见 Q6） |
| **D-I 任务边界 = 用户介入** | ⚠️ **已被 D-O 取代**。原意：`Request::Prompt` / `/loop` tick 都算新任务起点 | 保留此条只为记录演变；现行定义看 D-O |
| **D-J 唯一上限 = 200 turn** | `MAX_FOLLOW_UPS = 8` 删除；`JUDGE_FAILURE_LIMIT = 2` 保留 | 先取 200 作初值，实测执行结果后再调。**不再有 round 上限、不再有 follow-up 上限**。其他闸门一律不加 |
| **D-K 200 turn 只约束 `/goal`** | 200 turn 阀**仅在活动目标存在时生效**；没有目标的任务**不设任何轮数上限** | 理由：没目标时人在场，随时可以 Ctrl+C 接管。阀的职责只是保护「`/goal` 设完就走」的无人值守场景 |
| **D-L 删 `MAX_FOLLOW_UPS` 会改变无目标行为** | ⚠️ 删除后追问循环将**无轮数上限**（只有 `JUDGE_FAILURE_LIMIT` 与一期 git 一次性兜底能停它） | 这与 D-K 一致（无目标时不设阀），但要知道它是**行为变更**，不是无改动 |
| **D-M 耗尽后先收卷再停** | 200 turn 耗尽时**不直接切断**：先起一轮**总结汇报**（当前进展 / 状态 / 下一步计划），然后终止 | 不能让用户面对的只是一行「到上限了」——尤其 `/goal` 场景人可能不在屏前，回来后需要知道干了什么、卡在哪、接下来该干什么 |
| **D-N 收卷轮不计数、可用工具** | 收卷轮**不算进 200**；收卷轮里模型**可以调用工具**（不削工具表） | 不算数：否则预算耗尽那一刻就发不出这句话。可用工具：汇报前可能还需要看一眼文件才能说准 |
| **D-O 任务边界再界定** | **任务 = 一个 200 turn 预算的生命周期**，不是“用户一句话”。`/loop` **每 tick 重新计算 200**；`Request::Prompt` 新发言重新计算 | 原则：**200 turn 上限是针对一个任务的**（用户原话）。`/loop` 的一次定时工作 = 一个独立任务（placeholder 是 `[harness] 定时检查：{prompt}`，不带上一轮目标上下文）。⚠️ 本决策**修正 D-I** |
| **D-P 删掉判官** | **本次不做判官**。跨回合推进只由「清单驱动 + 模型自判」决定，不额外调模型核对。`goal_judge.rs` / `goal_judge_call.rs` 及 `judge_failure_delta` / `judge_failures` / `JUDGE_FAILURE_LIMIT` / `StopCause::JudgeUnreliable` 均在实现时删除。后续需要时再加 | 四条实证：① ADR-0041 要求「先实测自称完工但未完工的比例再重引机制」，该比例**从未被测量**（全仓零数据）；② `Verdict` 除一个 `Unparseable` 分支外**无人消费**，判官今天不阻止任何一次续跑；③ `evidence_window` 只读**最后一轮**（`goal_judge_call.rs:102`，测试名 `evidence_window_takes_only_last_round`），在 200 turn 的任务里绝大多数可验证据在视野外；④ 每个有 open 项的回合都多一次模型调用，成本未算过。**同类产品（codex / Pi）都是模型自判** |

> **术语纪律**：本文所有计数一律写明是哪一层 —— **round**（模型调用一次）/ **turn**（一次 `run_turn_chain`）/
> **追问**（系统追加的下一个 turn）/ **任务**（一个预算生命周期）。「跨轮」是禁用词。
> 权威定义见 [`docs/CONTEXT.md`](../../CONTEXT.md) 的「回合、轮与追问」节。

## 现有架构

### 三层轮次语义（D-H / D-I 的坐标）

「200 轮」必须落到底下一层，三层现在都叫得上「轮」：

| 层 | 现有计数 | 语义 | 今天的阀 |
|---|---|---|---|
| **round** | `BasicLoop` 的 `rounds`（`crates/ys-loop/src/basic.rs:673` 的 `rounds += 1`） | 一次模型调用（含工具调用回合） | `limits.max_rounds`（默认 `u32::MAX` = 不设，`config.rs:434`） |
| **turn** | `TurnCtx.cumulative_rounds`（`app_loop.rs:1516`），**每次 `run_turn_chain` 归零**（`:1717`） | 一次 `run_turn_chain` 调用 | `config.max_turn_duration()` = 48 h，**但每轮重置**（F1） |
| **follow-up** | `AdvanceState.used`（`goal.rs:186`） | 一次自动追问 | `MAX_FOLLOW_UPS = 8`（**待删**，D-J） |

⇒ **D-J / D-K 的结论**：只给 **turn** 这一层加阀（200），且**只在目标激活时**加；
round 与 follow-up 都不加。实现上需把 turn 计数移到追问循环外（今天
`TurnCtx.cumulative_rounds` 每次 `run_turn_chain` 归零，`app_loop.rs:1717`）。

**阀的作用域坐标**（按今天的实际分支）：

| 入口 | 代码位 | 有目标时 | 无目标时 |
|---|---|---|---|
| `Request::Prompt`（用户发言） | `app_loop.rs:781` + 追问循环 `:812-889` | 受 200 turn 阀 | **不设阀**（D-K） |
| `Wake::TimerFired`（`/loop`） | `app_loop.rs:615-711`，**不进追问循环** | **每 tick 重新计 200**（D-O） | 不设阀 |
| `Wake::BackgroundReturned` / `SubagentReturned` | 同上，单 turn | 不适用 | 不设阀 |

D-O 的推论：`/loop` 每 tick **只跑一个 turn**，那个 turn 自成一个任务，有自己的 200 预算；
它不进追问循环 ⇒ tick 之间**不共享预算**。所以 `/loop` 永远不会把 200 turn 耗尽（除了
那个 tick 内部的追问）。真正的 200 turn 链只存在于 `Request::Prompt` 的追问循环里。

### 两条闸门机制的实证核查（2026-11-09）

核查起因：「为什么要限最大轮次？stall 真的有意义吗？」以下是**代码事实**，不是设计意图。

#### F1：48 h 实际耗时兑底**不存在**，`MAX_FOLLOW_UPS` 是唯一绝对上限

ADR-0072 声称「token 预算不设 ⇒ 极端情况下花费无上界（由墙钟 48 h 兜底）」。
代码里这个兜底不成立：

- `run_turn_chain` **每次都重新取时钟**：`apps/coding-agent/src/app_loop.rs:1705-1706`
  ```rust
  let request_started = Instant::now();
  let deadline = request_started + config.max_turn_duration();   // 默认 48h
  ```
- 追问循环**每轮重新调用它**：首个回合 `:781`，追问回合 `:870`。
- 全仓**无任务级预算**（`grep chain_deadline` / `turn_chain_deadline` / `request_deadline` 全零命中）。

⇒ 48 h 是**每轮**预算。整个任务的实际上界 = `MAX_FOLLOW_UPS × 48h` ≈ **16 天**，
而 `MAX_FOLLOW_UPS = 8`（`apps/coding-agent/src/goal.rs:39`，全仓**仅一处引用**：`:186`）
就是那个唯一的绝对阀。

**上限不能删**，但它**形式错了**：轮数上限会误杀正当前的任务（需要 12 轮、每轮都在推进
的目标被 `CapReached` 截断），而 8 这个数字在 ADR-0024:47 只有「替换原默认 1」一句话，
无推导。正确的形式是**任务级的成本或耗时上限**，而不是轮数。

> ✅ **已决定（D-G）**：上限重新设计，不再以轮数作唯一硬阀。具体形式进 `arch-design`。
> 候选：① 任务级墙钟（把 `deadline` 从 `run_turn_chain` 提到追问循环外，每轮只看剩余时间）；
> ② 任务级花费上限（需给 `Usage` 累加阈值）；③ 两者都有。

#### F2：stall 实测等价于「清单 3 轮不变就停」，且以模型自陈为信号

三个问题：

1. **`model_claims_done` 是恒为真的冗余条件。** 判据 2（`goal.rs:150-161`）让
   只有 `StopReason::Completed` 能继续走到判据 7；而 `model_claims_done` 就是
   `matches!(last_stop, StopReason::Completed)`（`turn_advance.rs:75`）⇒ 走到判据 7 时
   **恒为 true**。注释里的「与模型自称收尾取 AND」实际不存在。
2. **判据信号是模型的自陈。** 清单变不变是模型自己决定的。模型完全可能连续三轮
   在改文件却没更新清单 —— stall 会判它「原地打转」。这与 ADR-0072 D4 自己的立论
   （「助手自述不可当证据」）直接矛盾。
3. **在 D-A（目标独立验收）下会直接误杀。** 无清单合法后，空集合指纹每轮恒等 ⇒
   第 3 轮必然 `Stalled`。模型什么都没做错，只是没写清单。
   （今天不会发：判据 5 用 `NoPlan` 先行短路径；但 D-A 要求改掉判据 5。）

> ADR-0072:214 的意图陈述是对的：「判官只答核对通过没有，不答你有没有在原地打转」。
> 但代码选的代理量（清单集合指纹）不属于**可观测事实**，属于模型自述。

> ✅ **已决定（D-F）**：stall 整层删除 —— 判据 7、`STALL_LIMIT`、`StallTracker`、
> `stall_digest`、`StopCause::Stalled` 全部移除，不保留形态、不用「判官结论」重写。
> 删后「无进展」不再有独立停机理由；成本由任务级预算（D-G）记。

### 已实现的部分（验收层，ADR-0072 的 D3–D11）

| 模块 | 关键项 | 位置 |
|---|---|---|
| `ys-protocol` 词汇层 | `FollowUp { Push, Stop(StopCause) }`；`StopCause` 七种（`NoPlan` / `AllDone` / `CapReached` / `Stalled`（**待删**，D-F）/ `Aborted` / `NotContinuable` / `JudgeUnreliable`） | `crates/ys-protocol/src/goal.rs:14`、`:24` |
| 判据（纯函数、同步） | `decide(store, used, last_stop, model_claims_done, stall, judge_failures) -> FollowUp`，八条判据按序短路 | `apps/coding-agent/src/goal.rs:129` |
| 闸门常量 | `MAX_FOLLOW_UPS = 8`（**待删**，D-J）、`STALL_LIMIT = 3`（**待删**，D-F）、`JUDGE_FAILURE_LIMIT = 2`（**待删**，D-P） | `apps/coding-agent/src/goal.rs:38`、`:45`、`:48` |
| ~~判官提示词 / 请求~~ | **D-P 删除**：`JUDGE_SYSTEM`；`judge_request(evidence, open_items)` | `apps/coding-agent/src/goal_judge_call.rs:45`、`:174` |
| ~~判官调用~~ | **D-P 删除**：`call_judge(model, evidence, open_items)` | `apps/coding-agent/src/goal_judge_call.rs:202` |
| ~~输出解析 + 引用校验~~ | **D-P 删除**：`parse_judge_output` / `Verdict` | `apps/coding-agent/src/goal_judge.rs:66`、`:87` |
| ~~证据窗口~~ | **D-P 删除**：`evidence_window` 只取最后一条 `ToolUse` 之后（只读**最后一轮**） | `apps/coding-agent/src/goal_judge_call.rs:102`、`:146` |
| stall 指纹（**待删**，D-F） | `stall_digest(&[TodoItem]) -> u64`（折叠空白→小写→排序→去重；状态进摘要；只算非终态项） | `apps/coding-agent/src/goal.rs:61` |
| 判官提示词 / 请求 | `JUDGE_SYSTEM`；`judge_request(evidence, open_items)` —— 不带工具表、关思考、`max_tokens` 封顶 | `apps/coding-agent/src/goal_judge_call.rs:45`、`:174` |
| 判官调用 | `call_judge(model, evidence, open_items)`：失败重试 1 次，**不落盘、不发事件** | `apps/coding-agent/src/goal_judge_call.rs:202` |
| 输出解析 + 引用校验 | `parse_judge_output(raw, evidence, open_items)`：`MET` 三条件（逐项 `OK` ∧ 每项引用逐字命中证据 ∧ 至少一条命中），任一不满足降级 `NOT_MET` | `apps/coding-agent/src/goal_judge.rs:66`、`Verdict` `:87` |
| 证据窗口 | `evidence_window(messages)`：只取最后一条 `ToolUse` 之后；`evidence_text` 拼 `<harness_facts>` + `<tool_trace>` | `apps/coding-agent/src/goal_judge_call.rs:102`、`:146` |
| 入口合并层 | `turn_advance::advance(...)`（判据 + 一期 git 兜底）、`judge_failure_delta(...)`（**待删**，D-P） | `apps/coding-agent/src/turn_advance.rs:64`、`:172` |

### 未实现的部分（缺口清单）

| 缺口 | 证据 | 影响 |
|---|---|---|
| **`/goal` 命令解析** | `crates/ys-tui-coding/src/commands.rs` 无 `goal` 分支（`COMMANDS` 长度 `18`，`:53`；`"loop" => parse_loop(&args)` 在 `:367`） | 用户敲不出来 |
| **`--goal <目标>` CLI 参数** | `apps/coding-agent/src/main.rs` 的 `parse_args` 只认 `--help` / `--json` / `-p` / `--resume` / `--debug` / `--stats` / `worktree` | Print / JSON 模式无法设目标 |
| **`Request` 协议格** | `crates/ys-protocol/src/request.rs:14` 的 `Request` 无 `Goal` / `GoalClear`；`Loop` / `LoopOff`（`:120`、`:133`）是唯一的跨回合驱动命令 | TUI → app 无通路 |
| **目标状态的持有者** | `app_loop.rs` 里只有 `LoopState`（`:330`）；全仓无 `GoalState`，`App`/`Wiring` 也没有目标字段 | 目标无处存 |
| **`GoalSpec` / `Verdict` / `ItemVerdict` 类型** | design 要求放 `crates/ys-protocol`（`design.md:887`）；实际只有 `FollowUp` / `StopCause` | D-P 删除判官后 **`Verdict` / `ItemVerdict` 不再需要**；`GoalSpec` 仍要（目标原文 + 已跑 turn 数） |
| ~~**目标文本进判官**~~ | 判官已删（D-P） | 改为：目标原文直接进 `decide` 的入参（见 Q1） |
| **目标未达成时的续跑条件** | `decide` 的「继续」只看 todo 的 open 项（判据 4/5）；无 `goal.is_some() ∧ ¬goal.met` 这一项（`design.md:872` 设计的继续条件） | 不入清单就无法被目标驱动 |
| ~~**`-p` 退出码 `2`**~~ | `single_turn_exit_code`（`main.rs:2693`）只映射 `stop_reason` → `0/1`；无「未达成」这一档 | **出本轮范围**（D-C 只做 TUI）；今后接 `-p` 时再补 |
| ~~**D12：目标期间抑制 `git` 追问与 `/loop` tick**~~ | `app_loop.rs:777` 无条件取 `config.follow_up_on_commit()`；`Wake::TimerFired`（`:615`）无条件起回合 | **已决定不做**（D-E）：各机制独立，不新增跨机制耦合 |
| ~~**`config.json` 的 `goal` 段**~~ | `config.rs` 无 `goal` 命中（`design.md:837-841` 设计的闸门配置） | **已决定不做**（D-D）：闸门保持常量 |
| **`/goal` 无参显示状态 / `clear`** | research 里对照产品的是「无参显示条件 / 耗时 / 评估轮数 / 最近理由」+ `/goal clear`（`cross-turn-advance/research.md:71`、`:73`） | 目标一旦设定无法查看 |

> ⚠️ **这条缺口没有被任何台账跟踪**：`docs/TODO.md` 与 `docs/goals.md` 都没有
> 「ADR-0072 缺入口」的条目 —— `grep 0072 docs/TODO.md` 零命中。
> 唯一的相关记录是 `docs/arch/long-horizon-todo/design.md:419` 的「验收层 ✅ 已实现」，
> 它读起来像**整体已落地**，实际只落了判据与判官（判官现已决定删除，D-P）。

### 关键数据流（今天的样子）

```
TUI: 用户输入 ──► commands.rs parse() ──► Action::Request(Request::Prompt)
                                          │
Print/JSON: argv ──► parse_args ──► Mode::Print/Json(任务)
                                          │
                                          ▼
                        run_turn_chain / run_single_turn
                                          │
                     ┌────────────────────┴────────────────────┐
                     │  回合跑完                                │
                     ▼                                          ▼
   app_loop.rs:812 AdvanceState::new()              main.rs:2433 AdvanceState::new()
   app_loop.rs:824 judge_failure_delta(...)         main.rs:2521 advance_after_turn(...)
   app_loop.rs:841 turn_advance::advance(...)       main.rs:2536 used += 1
                     │                                          │
                     └──────────────► Advance::Push ───────────┘
                                          │
                              todo_inject 注入清单 + 再跑一轮
```

**注意**：今天这套循环是**无条件**对每个回合求值的 —— 没有目标也照样跑，只是
`decide` 在第 5 条判据（从未写过清单）返回 `Stop(NoPlan)`，再退回一期的 git 追问。
也就是说「验收层已通电」，缺的只是「用户下达目标」这个**触发器**和它的状态载体。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `ys_tools_todo::TodoStore` | 清单唯一真值；`snapshot()` / `has_open_items()` / `revision()` | `adapters/tools-todo`；TUI 侧落盘 `<stem>.todo.json`，`-p` 侧纯内存 |
| `ys-core` | `Message` / `Role` / `ContentBlock` / `StopReason` / `Usage` | 推进决策的入参类型 |

## 约束

- **技术**
  - `ys-protocol` **零 tokio**（直接与间接）⇒ 目标类型只能是纯数据；推进判据留在 app 层。
  - `ys-tui-coding` **不依赖 `ys-runtime`**（编译器强制）⇒ `/goal` 的解析只能在
    `commands.rs` 产出 `Request` / `LocalAction`，不能直接碰 `TodoStore` 或 `Agent`。
  - `Request` 变体是**穷尽匹配** ⇒ 加一格会让 `app_loop` / `main` 两侧漏实现变成编译错误
    （这是既定纪律，见 `request.rs:11-13`）。
  - 目标达成**不靠独立判官核对**（D-P）：只看模型是否把清单做完、是否自我收尾。
- **性能**
  - **无任务级预算**（F1）：`max_turn_duration` 是**每 turn** 48 h，追问循环每 turn 重置；
    今天任务上唯一的绝对阀是 `MAX_FOLLOW_UPS`。
  - **D-J 的任务级上限**：一个任务最多 **200 turn**；round 与追问都不设阀。
    ⚠️ 实现上 turn 计数必须提到追问循环外 —— 今天 `TurnCtx.cumulative_rounds`
    每次 `run_turn_chain` 归零（`app_loop.rs:1717`）。单 turn 仍受既有 48 h 约束
    （D-J 不新增任务级墙钟，见 Q6）。
- **演进**
  - ADR-0072 D14：**目标不落盘**，`--resume` 不恢复目标；`/new` / `/clear` 同样清目标。
  - 清单**落盘**（`<stem>.todo.json`），与目标生命周期不同 —— 这是两条独立的真值。
  - `-p` 默认行为必须不变：不给 `--goal`、模型不写清单 ⇒ 跑完即退（`design.md:876`）。
- **组织**
  - 判据实现**只有一份**（`turn_advance::advance`）。
    本轮只接 TUI 入口，但**不得**在 `app_loop` 里另写一份判据 —— 今后接 `-p` 时需直接复用
    （否则「同一任务在不同入口跑出不同轮数」的漂移无从发现，`main.rs:2302-2306`）。

## 需求范围

### 范围内（本次要做的）

- `/goal <目标>`（TUI）—— **仅 TUI**，不做 `--goal` / 退出码 `2`（D-C）。
- 目标的运行期状态（一个会话至多一个、不落盘）。
- **目标作为独立验收对象**：目标原文是推进决策的一个入参，且目标未达成本身就是继续理由
  （不依赖模型是否写了清单）—— D-A。具体核对方式见 Q1（判官已删，D-P）。
- **每个回合结束后把进展写进 transcript**（不进状态行）：一条 `Outbound::Output`，
  说清「第几 turn / 为什么继续或停下」—— D-B。
- 目标结束（达成 / 停机）时的用户可见收场文案。

### 范围外（明确不做的）

- **不落盘、`--resume` 不恢复**（ADR-0072 D14 已决）。
- **不让用户写验收条件**（ADR-0072 D1 否决过该形态）。
- **不做多目标 / 多优先级 / 目标队列**（同一时刻只有一个活动目标，D13 的立论）。
- **不做 `--goal` / `-p` 退出码 `2`**（D-C：先做 TUI）。
- **不做 `config.json` 的 `goal` 闸门段**（D-D：闸门保持常量；唯一上限 200 turn 也是常量）。
- **不做 round 上限与 follow-up 上限**（D-J）。
- **不做任务级墙钟**（本轮；Q6 仍开放）。
- **不做 D12 的跨机制抑制**（D-E：不压 `git` 追问、不压 `/loop` tick）。
- **不做状态行 / `LoopView` 那种视图投影**（D-B：只走 transcript）。
- **不做独立判官 / 证据核对**（D-P：模型自判 + 清单驱动，后续需要时再加）。
- **不建 hook 系统**（ADR-0014）。

### 关键场景

- 场景 1（成功路径）：`/goal 修好登录超时` → agent 干活（写不写清单均可）→ 模型把清单做完并收尾
  ⇒ 清单无 open 项 ⇒ `Stop(AllDone)`，transcript 报「已达成」。
- 场景 2（多回合）：清单仍有 open 项 ⇒ 自动追加一个**追问回合**，直到清单清空或到 200 turn。
- 场景 3（无进展）：**没有专门的闸门**（D-F）。没进展就是没清空清单 ⇒ 继续跑，
  直到 200 turn 或用户介入。
- 场景 4：**没有判官**（D-P）⇒ 「判官不可用」这类停机理由一并消失。
- 场景 5（用户打断）：`Ctrl+C` ⇒ `Stop(Aborted)`，用户意图优先于任何判据。
- 场景 6（到 200 turn）：先起一个**收卷回合**汇报进展 / 状态 / 下一步计划（D-M），然后停下。
  收卷回合不计数、可调工具（D-N）。用户再说一句话或下一次 `/loop` tick 即新任务重新计 200（D-O）。
- 场景 7（无参 `/goal`）：打印当前目标与已跑 turn 数（见 Q2）。

### 异常 / 边界

- 目标未设且不写清单 ⇒ 行为与今天完全一致（`NoPlan` → 一期 git 追问）。
- **目标设了但模型没写清单** ⇒ 这是本次要打通的路径：推进决策必须能只拿目标工作。
  今天的 `decide` 在这里直接 `Stop(NoPlan)`。
- 目标达成的判定不依赖清单项全部 `completed` —— 清单是模型的自陈。
- `/goal` 与 `/resume` / `/new` / `/clear`：目标不继承（D14），换会话后目标归零。
- 会话内切模型是允许的；D-P 后推进不再依赖任何额外模型调用，切模型不影响推进。

## 未澄清问题

待用户确认（留给 `arch-design` 阶段定，不阻塞）：

- [ ] **Q1（D-P 后重开）：目标怎么参与「要不要继续」的决策。** 判官已删，但 D-A 要求目标是独立验收对象，
  所以必须回答：① 只靠清单（目标只当首回合的输入，不参与判据，最简）；
  ② 目标原文进 `decide`，但由**模型在收尾时自我声明**是否达成（如要求它在正文给一个可解析标记）；
  ③ 其他。① 最省，但它使 D-A 退化成「目标只是一句提示词」；② 保留了独立性但重引入自述风险。
- [ ] **Q2：无参 `/goal` 的显示内容。** 已定要显示状态、且只走 transcript；待定的是**触发时机**：
  无参 `/goal` 按需打印一次，还是每轮都打一条进展？
- [ ] **Q3：`/goal` 是否支持 clear。** 不设的话，用户只能靠 `/new` / `/clear` 摆脱一个卡住的目标。
- [ ] **Q4：目标与 todo 清单同时存在时的停机理由归属。** 目标达成但清单还有 open 项时，
  按 `StopCause::AllDone` 报告还是新增一个「目标达成」原因？
- [ ] **Q5（D-J / D-M）：收卷回合的三种实现路径。** 现有代码里已有一套**回合内**收卷机制：
  `RunLimits.final_report_prompt`（`crates/ys-component/src/limits.rs:170`）+ `deadline_reached`
  （`crates/ys-loop/src/basic.rs:142`）+ 收卷轮注入（`:695-740`）。但它是**回合内 round 数/墙钟**到点触发，
  且 **app 侧从未设置该字段**（全仓只有测试设）。三个选项：
  ① 任务级收卷从 app 层做：耗尽时额外调一次 `run_turn_chain`，输入一句「汇报进展/状态/下一步」的
     `Role::Harness` 消息；
  ② 给最后一个 turn 的 `limits` 设上 `final_report_prompt`，复用既有回合内收卷轮（它保留工具表、只放行一个 round）；
  ③ 新增一个任务级收卷机制。
  另需定：**收卷回合里模型还能不能调工具** → 已定 **可以**（D-N）。
- [ ] **Q6（D-H / D-K 残留）：48 h 还要不要。** 本轮只给 200 turn（且仅限 `/goal`），
  「一个任务最长 48 h」**没有任何实现承载**（旧的 `max_turn_duration` 是每 turn 重置，不等于任务级）。
  选项：① 本轮不实现，接受「200 turn 跑多久算多久」；② 一并把 `deadline` 提到 `/goal` 任务级。
- [ ] **Q7（D-L / D-P）：删 `MAX_FOLLOW_UPS` 以后的止停条件。** 先说清现状：**唯一的追问循环由清单（todo）驱动**
  —— 没有清单就 `Stop(NoPlan)`，最多再靠一期 git 兜底放行**一次**。所以删了 `MAX_FOLLOW_UPS`
  之后，只有「模型持续写清单且不清空」才会无限跑；D-P 删掉判官后，
  `JUDGE_FAILURE_LIMIT` 也不再存在 ⇒ **只剩一期 git 的一次性兜底**。

## 后续建议

- 建议用 `arch-design` 做方案设计 —— 剩余分歧集中在 Q1（目标怎么参与判据）、Q5（收卷回合怎么实现）、
  与 Q6（48 h 还要不要）。D-F / D-G / D-P 已经定了方向，设计只需给具体形状。
- **判官不要现在就重加**（D-P）。若今后要重建，先补 ADR-0041 要求的那项实测：
  模型自称完工但实际未完工的比例。没有这个数据，任何形式的判官都只是假设。
- 实现前把 Q1–Q7 收敛；D-A～D-P 已经定了方向，不得再回退。
- **建议另开 ADR** 记 D-F / D-G / D-P —— 它们改的是 ADR-0024 / ADR-0072 / ADR-0041 已经写下的硬约束，
  不只在本文里口头记一笔。
