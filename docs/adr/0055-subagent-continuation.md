# 0055 — Subagent 回复后继续推进：父回合「等得起」，不新起回合

让子 agent 的回复**必然**推进父侧工作 —— 无论它什么时候回来。做法只有一条：
**父回合在「还有子 agent 在跑」时不许收场**；等它回来、把结果作为**同一回合的下一轮**的输入，
汇报并继续；等到上限才如实收场并明确告知。

**不新起回合、不新增协议格、不改会话格式。**

## Status

accepted（2026-10-25；设计见 [`subagent-continuation/design.md`](../arch/subagent-continuation/design.md)）

## 背景

### 1. 回合内已经是「新的一轮」了

子 agent 在回合内完成 ⇒ `PendingResults::deliver` 直接 `push(Boundary::Deferred)`
（`apps/coding-agent/src/subagent.rs:234-256`）⇒ `BasicLoop::drain_boundary` append 进会话并发
`DeferredResult`（`crates/ys-loop/src/basic.rs:102-111`）⇒ 返回 `Consumed` ⇒ **收场前那次 drain
也会 `continue`**（`basic.rs:769-782`）。⇒ 模型下一轮立刻看到结果。

**这条通路不需要改。** 用户要求的「把它当成新的一轮」在回合内**已经是事实**。

### 2. 病在回合**之后**

父回合收场后才跑完的子 agent：
- 落点已解绑（`app_loop.rs:1243` 的守卫随回合块 drop）⇒ 结果**静默入表**
  （`subagent.rs:241-252`，只有超限淘汰才写日志）；
- 表**没有回合外的读者**：`take_pending_results` 的唯一生产调用点在 `run_turn_once` 内部
  （`app_loop.rs:1229`），而它唯一的入口是 `Request::Prompt`（`:448`）；
- ⇒ 结果一直躺着，直到**用户随手敲一句话**才被「顺带」补投。

### 3. 提示词此刻在说谎

`apps/coding-agent/src/prompt.rs:321-332` 告诉模型：

> If you end the turn before it finishes, the sub-agent keeps running and its result is
> delivered **at the start of your next turn**

那个 "next turn" 需要用户开口。⇒ 模型遵守契约放心收工，结果永不自动回来。这是一条**假契约**，
也是「模型声称还在等 / 反复说还在跑」这类观感的根因。

## 决策

### 1. 收场条件收紧：有子在跑就不收场

在 `BasicLoop` 的 Step 9c（模型给出最终答复那条路，`basic.rs:769-782`）的
`BoundaryDrain::Empty` 分支里插入：若 `ctx.subagent.has_running()`，则进入等待。

等待循环（新函数 `wait_for_subagents`）每一轮：
1. 先 `drain_boundary` —— 结果可能已经进来（**复用同一条路径**，不另写）；
2. 再查 `running()` —— 没了就收场（可能全部失败/被取消）；
3. 到 `RunLimits::subagent_wait` 上限 ⇒ 收场；
4. 否则等一拍：`settled()` 唤醒 或 `SUBAGENT_POLL` 超时，两者取先到。

**不需要 abort 分支**：`Abort` 入队即置 `is_aborted`（`boundary.rs:117-125`），
本次等待最多再等一个 `SUBAGENT_POLL`（200ms）就回到 Step 3 探针（`basic.rs:379`）
⇒ `Cancelled` 收场。200ms 是取消延迟的上界，不是状态轮询。

**为什么在循环内、不在 `app_loop`**：
- 只有循环内能把结果变成**同一回合的下一轮** ⇒ 上下文连续、模型知道刚才在做哪一步；
- 出了 `run_turn` 就只能新起回合，而新回合有**四条硬代价**（见 Considered Options）；
- 循环内改动天然让 `-p` / `--json` / TUI **三条入口**同时生效 —— 这正是 ADR-0041 记下的那个
  前置条件（「若重新引入自动续跑，不该只在一条入口上生效」）。

### 2. 判据是**进程级**的（不是 runner 的字段）

新增 `LiveSubagents { running: AtomicUsize, settled: Notify }`，以 `Arc` 经 `SubagentSetup`
跨 `/resume` 重建共享（与 `PendingResults` 逐字同形）。

**为什么必须进程级**：`AppSubagentRunner` **每回合新建**（`app_loop.rs:1244`）⇒ 上一回合派发、
本回合仍在跑的子 agent 不在它的视野里。若判据读 runner 自己的计数器，它会恒为 `false`
⇒ 父回合立刻收场，等待从不发生。

**为什么用 RAII 守卫而不是手工 `fetch_add` / `fetch_sub`**：`ChildTask::execute` 有四条退出路径
（组装失败 / `Ok` / `Err` / future 被 drop，`subagent.rs:774` / `:844` / `:873`），手工减必漏一条。
守卫进 `ChildTask`，`Drop` 结构上保证回归。

**`notify_waiters` 而非 `notify_one`**：等待循环是「唤醒 + tick」双源 ⇒ 漏一次唤醒的成本只是
一个 `SUBAGENT_POLL`，而 `notify_waiters` 不存 permit ⇒ 不会出现「迟到 permit 让下一回合
立即醒来」的怪象。

### 3. 四道闸

| 闸 | 默认 | 理由 |
|---|---|---|
| `RunLimits::subagent_wait` | **30 min** | 实测最长派发 965 s（`subagent.rs:1234`）⇒ 留约 2 倍余量。装配点再按 `min(本值, max_turn_duration)` 收紧，避免与父 budget 双计时器打架 |
| `SUBAGENT_POLL` | 200 ms | 不是状态轮询 —— 只为 abort 延迟与「结果已入边界源但无唤醒源」兜底（`QueueBoundarySource::push` 是同步的、不通知任何人） |
| `settled()` 唤醒 | — | 计数变化时即时醒（主路径） |
| `running()` | — | **唯一收场判据** |

**上限不按「最后一次派发」算**：按「进入等待至今」算。否则模型可以在等待期间不断派新子 agent
无限续期。

### 3. 承重不变量：计数必须有出口

父回合的收场判据是 `running() == 0` ⇒ **任何让计数停在 >0 的路径都会把父回合钉到上限**。

⇒ `LiveGuard` 必须进 `ChildTask`，靠 `Drop` 回归 —— `execute` 有四条退出路径
（组装失败 / `Ok` / `Err` / future 被 drop，`subagent.rs:774` / `:844` / `:873`），手工配对必漏一条。
这不是防御性编程，是**正确性要求**，要写成测试。

### 4. 超时后：**告知，不代劳**

等满上限仍没回来 ⇒ 收场（**不自动续跑**）。结果稍后到达时进 `PendingResults` 表，
等用户下次输入时补投 —— 语义与今天一样。

但今天的问题是**它静默**。修法极小且**不引入任何回合**：`PendingResults` 加 `Notify`，
app_loop 空闲 `select` 加一臂 ⇒ 推一行 `Outbound::Output` + 一次 `View` ⇒
TUI 显示「sa3.1 已返回，将在你下次发言时进入上下文」。

**为何不自动起一轮**：那等于在用户没开口时替他花一次模型调用的钱，且要回答
「合成消息会落盘、`/resume` 回放会把它画成用户行」这个坑。**告知 ≠ 代劳。**

`StopReason` **不变**（仍是 `Completed` —— 模型确实答完了，不污染终止原因词汇）。

### 6. 提示词：把假契约改成真的

`prompt.rs:321-332`（`subagent_async` 段）三处：
1. 删掉 `at the start of your next turn` 的承诺，改为「你收工后它会自己把你叫醒」；
2. 替换无法遵守的 `never claim … without checking` → 改为
   `do not claim a sub-agent is still running unless you are waiting for it`
   （等待期间模型本来就知道在等谁）。
3. 新增汇报纪律：**结果到达后，先用一句说它找到了什么、你接下来做什么，再继续。**

落点必须插在 `subagent` 段**之前** —— `subagent_guideline_block`（`prompt.rs:1169-1179`）用
「成本提醒」那句当段尾锚。

## Considered Options

- **系统再起一个回合（follow-up / `DeliveryTarget::NextTurn`）**（否决）。四条硬代价：
  1. **后半段失忆**：新回合新建 `RuntimeContext`（`crates/ys-runtime/src/agent.rs:118-131`），
     `ToolContext.subagent` 是每回合现造的 runner（`app_loop.rs:1244-1251`）⇒ 新回合看不到
     上一回合那几次派发的句柄与转录路径，模型见到结果却对不上是谁；
  2. **重复注入**：回合开头补投（`app_loop.rs:1227-1231`）与 `drain_boundary` 的 append 是两条路
     ⇒ 同一条结果可能落两次；
  3. **入口不一致**：只在 `app_loop` 生效，`-p` / `--json` 的 `run_single_turn`
     （`main.rs:1741`）另一条路 ⇒ 重演 ADR-0041 记下的缺陷；
  4. **成本翻倍**：新回合重新组请求、可能重新触发压缩判断、新预算。
- **保持现状，等用户敲字**（否决）：结果滞留（表上限 32 条，`subagent.rs:48`），
  且提示词继续撒谎。
- **把结果塞回原 `ToolCall` 的 `ToolResult`**（否决）：协议上不可能 —— `tool_call_id` 已被 ACK
  用掉，而 `ContentBlock::ToolResult` 是 JSONL 落盘契约、一问一答。
- **父回合无限等待**（否决）：子预算 48h ⇒ 必须封顶。
- **`subagent_status` 带 wait 参数**（否决）：见 [ADR-0054](0054-subagent-progress-board.md) 决策 7
  与 `bg-notify/design.md` §1。

## Consequences

- **父回合可能比模型收工更晚结束** —— 用户在「模型已答完但子 agent 还在跑」时会看到
  `Working` 继续，状态行显示「等待 sa3.x」。这是**有意**的：它替代了现状的「假装空闲」。
- 回合的 wall-clock 因此变长，但**受 `subagent_wait` 与父 deadline 双重约束**。
- 子 agent 结果**在上限内回来时**不再滞留；超时后仍会留在表里，但**不再静默**
  （空闲态会告知「sa3.x 已返回，将在你下次发言时进入上下文」）。
- 提示词去掉一条假承诺、加一条汇报纪律 ⇒ 父前缀略变（固定前缀，命中缓存）。
- **本轮不改** `SubagentOutcome`（结构化头降 P1）⇒ 无 breaking 变更。
- 「一个请求恰好一个回合」（ADR-0041）**保持不变** —— 本决策只改收场条件，不增回合数。
- **ADR-0054（状态板）降为 P1**：本决策的判据用 `LiveSubagents`（进程级计数），
  `running()` 的签名与状态板兼容，届时只换实现。
- **`subagent_status` 工具与增量落盘降为 P1/P2**：等待设计生效后，模型不需要「问能不能换任务」。

## 参考

- [ADR-0041](0041-remove-completion-review-turn.md) — 一个请求恰好一个回合（本决策不违反它）
- [ADR-0047](0047-input-delivery-target-and-queue.md) — `NextTurn` 协议格（本决策不用它）
- [ADR-0054](0054-subagent-progress-board.md) — 状态板（**降为 P1**；本决策用进程级计数，签名兼容）
- [ADR-0014](0014-follow-up-without-hook-system.md) — 零消费者 ⇒ 不建
- [`subagent-continuation/design.md`](../arch/subagent-continuation/design.md) — 分刀与验收
- `crates/ys-loop/src/basic.rs`（Step 9c）/ `apps/coding-agent/src/subagent.rs`（投递表）
