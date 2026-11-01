# 0065 — 删同步派发：子 agent 结果自动起回合，插话走 `subagent_send`

父 agent 只保留**后台派发**一种形态：`subagent_async` 拿句柄即返回，父回合**不等**它收场；
结果回来后 harness **自动起一个新回合**把它交给模型（与 `bash_bg` 同一条通路）；父模型需要
干预在跑的子 agent 时用新工具 `subagent_send` 发一句话。句柄改为**全局唯一**的
`sa{turn}.{n}`，否则按句柄寻址会打到上一回合的同序号任务。

**本设计取代 [ADR-0055](0055-subagent-continuation.md) 的「父回合等待子 agent」。**

## Status

accepted（2026-11-01；设计见
[`subagent-async-convergence`](../design-plans/2026-11-01-subagent-async-convergence.md)）

## 背景

### 1. 同步派发的病是可观察性，不是速度

同步 `subagent` 在父回合内 `await` 子 agent 的结果：这段时间父模型**什么都做不了**，
用户也看不到子 agent 在做什么（子事件不进父流，ADR-0022）。父模型拿不到句柄，因此也无法
在子 agent 跑偏时干预。所谓「并行」只是一句承诺，实际是一段谁也无法观察的黑箱。

### 2. 「父回合等得起」把同一段黑箱搬到了收场前

ADR-0055 的出发点是「子 agent 的回复必须推进父侧工作」，做法是**父回合在有子 agent 在跑时
不许收场**，等它回来、把结果作为同一回合的下一轮的输入。它解决了「结果躺着没人取」，
但代价正是第 1 条那个黑箱：父回合被钉在等待里，观察者依旧看不出子 agent 在干什么，
子 agent 也不会因为父回合在等它就变得可干预。

### 3. 观察载体已经定了，不需要新的推送通道

子 agent 自己写会话文件（`SubagentsDir` 的转录落盘），主 agent 读文件即读状态。
⇒ ADR-0054 的 `SubagentBoard` / `ProgressTap` / `AgentEvent::SubagentProgress` 不是本设计的
前置条件，状态板整体推迟（见「明确不做」）。

### 4. 句柄曾是回合内局部量

`sa{n}` 的 `n` 是 per-runner 的派发序号，每个回合从 0 重来 ⇒ 每个回合的第一个子 agent 都叫
`sa1`。只要句柄只用于**展示**，这没有害处；但一旦有工具按句柄**寻址投递**，跨回合同名就会
把消息发给上一回合已经收尾的任务。

## 决策

### 1. 删除同步 `subagent` 工具，父表只留 `subagent_async`

同步派发是可观察性最差的一种形态，且它占用的是父回合最贵的那段时间。工具表里只保留
`subagent_async`：拿句柄立刻返回，父模型**接着干自己的活**，需要看子 agent 进展就去读它的
转录文件。

`SubagentRunner` trait 上的同步方法（`run`）**作为带默认实现的签名保留**（默认返回「不支持」），
但**无生产调用者** —— 唯一实现方是 `AppSubagentRunner`，其 `run` 只在测试里作为「直接跑完一个
子 agent」的入口。跟着工具一起删的是工具本身、它的文本与测试；没有跟着删的是一个仍可被实现方
覆写的 trait 方法。

### 2. 父回合不再等待在跑的子 agent

删掉「有子在跑就不收场」那条等待链：收场判据回到「模型给出最终答复 + 边界队列已空」。
`LiveSubagents`（进程级在跑计数 + RAII 守卫）**保留**——它保证「进程内还有几个子 agent 在跑」
这个数准确，与是否等待无关，将来读这个数的消费者（TUI 计数、状态板）不必自己重造。
随等待链一并消失的是它内部的 `Notify`（唯一用途是唤醒等待者）。

**代价：子结果与父回合的上下文连续性变弱。** 结果不再续写同一个回合，而是**新起一个回合**——
父模型的「我刚派发完就拿到了结果」这条心流被打断，它看到的是「上一回合的某件事已经完成」。
这是本决策换来的东西的对价：父回合能在子 agent 还在跑时先收场，用户能早一步看到结论、
早一步插话，子 agent 的进展开盘即可查看。

### 3. 子结果自动起回合，走 `bash_bg` 的既有通路

`Wake::SubagentReturned` 与 `Wake::BackgroundReturned` **同构**（`apps/coding-agent/src/app_loop.rs:413`
的臂、`:1429` 的 `subagent_placeholder`）：

1. 复核 `pending_results().has_pending()` —— 唤醒只说「可能有」（`Notify` 存 permit，跨回合
   残留会假醒），空则 `continue`；
2. 起回合，占位输入是 `Role::Harness` 的一行「结果已送入上下文」——真正的内容随后经
   `Boundary::Deferred` 在回合开头的补投注入；
3. **本臂只读不取**：`has_pending` 不消费，取走仍归 `run_turn_once` 的补投 ⇒ 不会重复送达。

**为什么这不是「替用户花钱」。** 它不是新机制，是 `bash_bg` 已经在用的同一条 `run_turn_chain`
通路：后台命令跑完 → 起回合。子 agent 是同一类东西（父模型发起、在后台跑、结果是一种输入），
放在两条不同的唤醒语义下才是反常的那个。既有规则也没变：一次回合仍是一次模型调用序列，
开销可见、可中断、可拒绝。

**代价：每次子 agent 结果多一次模型调用。** 派发 N 个子 agent，最坏情况是 N 次额外的、可能
很短的回合（结果一句话就处理完）。对照 ADR-0055：那时这些处理挤在同一个回合里，总调用数
相近，但全部压在父回合收场之前 —— 延迟由用户承担，而不是由 token 承担。

**`-p` / `--json` 不自动起回合**：单回合模式没有「下一回合」，结果只落表。这是模式的既有
边界，不是本决策的例外。

### 4. `subagent_send`：一条普通消息，不限制语义

工具把 `message` 包成 `Message { role: Role::User, .. }` 推成 `Boundary::Steer`，投进
**该子 agent 自己那份**边界源（`SubagentHandles`，`apps/coding-agent/src/subagent.rs:420`），
子在下一个轮边界 `take` 到、append 进自己的会话并继续。trait 侧是
`SubagentRunner::send`（`crates/ys-tool/src/subagent.rs`），在 `ToolContext` 上**不开新字段**
—— 它本来就与 `spawn` 同属「派发能力」。

**为何不限制语义**：工具不检查「子 agent 是否该收敛」、不注入模板文本、不设「只能发一条」、
不把发完的消息升级成 `Abort`。约束写在提示词里（`prompt.rs:560` 的 guideline：
何时用、地址来自 `subagent_async` 打出的 handle、送达点是下一个轮边界、不打断正在跑的工具、
措辞随意但**不能替代重新派发**）。用机制约束，会逼出「模型想说的话被 schema 拒绝」这一类
无用失败；而且「什么算收敛」不可判定，能判定的只有「是否入队成功」。

**为何用 `Role::User` 而不是 `Role::Harness`**：子转录会打印角色名
（`transcript_body`，`apps/coding-agent/src/subagent.rs:1814`）。写 `Harness` 会让子转录里出现
「父 harness 说过话」的误导 —— 那是**父侧**的角色语义，在子会话里没有对应物。对子 agent 而言，
这条消息确实是一次外部输入，`User` 更准确。

**fail-closed**：handle 不存在、或对应子 agent 已收尾（注册表用 `Weak`，`upgrade()` 失败即
「已收尾」）⇒ `Err`，错误正文列出当前有效的 handle 列表。绝不静默 `Ok` —— 静默会让父模型
以为话已送到，而子 agent 永远看不到它。

### 5. 句柄全局唯一 `sa{turn}.{n}`（落实 ADR-0054 决策 ⑤）

`subagent_handle(turn, n)`（`apps/coding-agent/src/subagent.rs:1258`）= 回合号 + 回合内派发序号，
`n` 仍是 per-runner 计数（计数逻辑不变，只加前缀）。回合段由 `run_turn_once` 写入共享原子，
`sa1.1` / `sa1.2` / `sa2.1` 既唯一也可读（一眼对上第几回合）。

**不用进程级计数器**：`sa17` 跨会话不可读，`/resume` 之后序号不再有意义。回合号是会话内
可解释的唯一来源。

`subagent_send` 是本决策的直接动因：不带回合段的句柄在**跨回合**上不唯一，投递会打到上一个
回合的同序号任务 —— 那条任务大概率已收尾（于是报「没有在跑的子 agent」），或者更糟：
同序号任务还活着，消息发给了错误的对象。

### 6. `ChildBoundary::has_pending_steer()` 保持恒 `false`

它只控制一件事：「**父的**插话是否打断**子**正在跑的工具」。答案是不打断 —— 与 ADR-0064
对父侧工具的同口径。插话本来就在轮边界被消费，从不缩短正在跑的工具；若让父的插话透传到子，
它会以「孩子的命令莫名其妙提前中止」的形式出现，语义自相矛盾。

它**不阻断 `take()`**：`subagent_send` 的投递走 `take()`，与探针无关。因此本条不需要为新工具改动。

## 已接受的代价

- **上下文连续性变弱**（决策 2）：结果以新回合的形式到达，而不是同一回合的续写。
- **每个子结果多一次模型调用**（决策 3）：收益是父回合不必等，成本以 token 计。
- **插话的最大延迟 = 当前正在跑的工具跑完**：`Steer` 在轮边界生效，所以 `subagent_send` 不是
  中断，不能用来「立刻停手」。要硬停只有 `Abort`（整回合中止，且会丢掉子 agent 的残余工作）。
- **语义重复的风险由提示词承担**（决策 4）：子模型可能把「收敛吧」理解为新任务而继续展开。
  这是提示词问题，不是代码缺陷 —— 可判定的是「能正常收尾」，不是「按期望收敛」。
- **`subagent_send` 只能逐个 handle 发**：没有广播。

## 明确不做

防后来者以为已做：

- **ADR-0054 的状态板 / 增量落盘 / `subagent_status` 工具**（决策 ⑤ 以外的部分）——
  一条也未实现。全仓无 `SubagentBoard` / `ProgressTap` / `AgentEvent::SubagentProgress`。
  观察载体是子 agent 自己的会话文件。
- **`subagent_send` 的广播语义**（发给全部在跑的子 agent）—— 当前只能按 handle 逐个发；
  多条由模型多次调用实现。注册表的 `live_handles()` 是现成基座。
- **`subagent_send` 进入子 agent 的工具表** —— 子表 9 元，子 agent 没有派发工具，因此也不需要
  向谁发消息的能力。将来若支持嵌套派发，子表构造点加一行即可。
- **会话归属**：`PendingResults` 是进程级、会话无关的；`/resume` 到另一个会话时，同进程内
  别的会话的结果仍会投给新会话。自动起回合会**放大**这个既有边界（无人发言也会起回合），
  本轮记录为已知缺口，不改表结构。

## 已知边界

子 agent 收到 `subagent_send` 消息的最大延迟 = **它当前正在跑的那个工具跑完**。它不是立即生效，
也不打断任何工作。父模型在工具返回文案与提示词两处都被告知这一点，避免它以为发完就改了方向。

## 验证

- 投递通路与 fail-closed：`subagent_send_queues_steer_into_the_child_queue`（注册后按 handle
  投递 ⇒ `own.take()` 得到 `Boundary::Steer`，`role == User`）、
  `subagent_send_fails_closed_when_handle_is_unknown_or_finished`。
- 句柄形状与唯一性：`subagent_handle_carries_turn_and_sequence`（`sa1.1` / `sa1.2` / `sa2.1`）。
- 去等待：`final_answer_returns_immediately_without_running_subagents`（crates 层，子 agent 仍在跑
  时父回合正常收场）、`async_subagent_result_survives_turn_boundary`（app 层，父回合在子 agent
  还在跑时收场）。
- 被插话后仍能收尾：`steer_mid_run_still_finishes_completed`。
- 工具表：父表 12 元 = 9 基础 + `bash_bg` + `subagent_async` + `subagent_send`，名字集合**不含**
  `subagent`（`apps/coding-agent/src/toolset.rs:668` 的锚点）。

## 相关

- [ADR-0055](0055-subagent-continuation.md) — 被本决策取代（「父回合等得起」），正文保留记 why
- [ADR-0054](0054-subagent-progress-board.md) — 决策 ⑤（句柄唯一）由本 ADR 落地，其余仍 `proposed`
- [ADR-0064](0064-steer-does-not-interrupt.md) — 插话不打断工具与子 agent；本设计复用同一条口径
- [ADR-0029](0029-subagent-landing-decisions.md) — 子边界 `take()` 取自己那份（本设计依赖它）
- [ADR-0022](0022-subagent-dispatch.md) — 子事件不进父流
- [ADR-0013](0013-cancel-token-removal-boundary-source.md) — `Boundary` / `Abort` 的出处
- `apps/coding-agent/src/subagent.rs`（`SubagentHandles` / `SubagentSendTool` / `subagent_handle`）、
  `apps/coding-agent/src/app_loop.rs`（`Wake::SubagentReturned` / `subagent_placeholder`）、
  `crates/ys-tool/src/subagent.rs`（`SubagentRunner::send`）
