# 0052 — 手动 `/compact` 走轮边界，在回合内立即执行

让 `/compact` 在**下一个轮边界**执行，而不是排队等整个回合跑完；提交时当场给一行反馈。

## Status

accepted（2026-10-24）

## 背景

### as-built（改动前）

`/compact` 是 `Request`（信道 ①，`ys-protocol/src/request.rs` 的 `Request::Compact`）。
而 `request_rx` **只在回合之间**被消费（`apps/coding-agent/src/app_loop.rs` 的外层循环
`request_rx.recv()`）；回合内的 `select!` 只监听 `sink_rx` 与 `boundary_rx`。

⇒ **回合中敲的 `/compact` 会一直躺在信道里，直到整个回合结束。**

实测后果：一次跑 38 轮工具调用、历时 7 分钟的回合里敲 `/compact`，屏幕上**一个字都不变**。
两处叠加造成「完全没反应」的观感：

1. 命令排在回合之后（上面那条）；
2. `dispatch` 对非 `Prompt` 的 `Request` 只调 `send_request` → `note_request_sent`，
   置上 `command_in_flight` 标志 —— 而**该标志从不被渲染**
   （`crates/ys-tui-coding/src/draw.rs` 对它零引用），且输入框已被清空、命令原文不进
   transcript。用户无法区分「已接受在排队」「被吞了」「打错了」。

### 为什么不是「加一行提示」就够

只补提示能治「看不出敲进去了」，治不了「等 7 分钟」。而压缩本来就是**轮边界动作** ——
自动压缩就在 `BasicLoop` 的 Step 4b（轮边界、模型调用之前）。手动触发走同一个点，
是让两条路径共用同一份实现，而不是让它们分居两个生命周期。

## 决策

新增 `Boundary::Compact`（信道 ②，与 `Steer` / `Deferred` / `Abort` 同层）：

| 场景 | 投递 | 生效点 |
|---|---|---|
| 空闲时 `/compact` | `Request::Compact`（不变） | 立即（外层循环） |
| **回合中 `/compact`** | **`Boundary::Compact`** | **下一个轮边界** |
| 普通消息（空闲 / 回合中） | `Request::Prompt` / `Boundary::Steer` | 回合末 / 下一轮 |

### 三条承重的实现约束

**1. `Compact` 不设 `consumed`。**

`drain_boundary` 返回的 `Consumed` 在三处调用点的语义都是「模型有新东西要看 ⇒ 继续循环」：

| 调用点 | `Empty` | `Consumed` |
|---|---|---|
| 主循环（模型调用前） | 开新一轮 | 同左 |
| 截断续写后 | 收场 | `continue` |
| 最终答复后（收场前） | 收场 | `continue` |

压缩只缩短会话、**不带来新的模型输入**。若把它算 `Consumed`，后两处会 `continue` 出
一轮**没有任何新输入的模型调用** —— 白烧一次请求，且用户会看到模型自问自答。
（变异验证：设 `consumed = true` ⇒ `rounds` 从 1 变 2，测试变红。）

**2. 手动路径不判触发阈值。**

Step 4b 的 `fixed < trigger && estimated + fixed > trigger` 与 R5 早退
（`fixed >= trigger` 直接跳过）**只属于自动路径**。用户点了就压 —— 与既有
`capabilities::compact` 的语义一致（那里的注释已写明「手动 `/compact` 不等自动触发阈值」）。

**3. `has_pending_steer` 包含 `Compact`。**

该探针供**长工具**在运行期间轮询，命中即提前收口。若不把 `Compact` 算进去，
一个跑十几分钟的工具会让 `/compact` 同样等十几分钟 —— 那正是本变体要消除的延迟。

### 压本体抽成 `run_compaction(ctx, rounds)`

自动路径（Step 4b 判阈值后）与手动路径（`Boundary::Compact`）调用同一份实现 ⇒
事件（`RoundUsage { purpose: Compaction }` / `ContextCompacted` / `ContextCompactionFailed`）、
成本口径、失败语义（B-3：失败时 session 一个字节不动）**不可能漂移**。

## 明确否决

- ❌ **在 `run_turn_once` 的 `select!` 里加 `request_rx` 分支**：`/compact` 会
  `replace_all` 改写 session，而 `BasicLoop` 正持有消息快照跑本轮 ⇒ 竞态；
  且它绕过轮边界，与 `Steer` 的语义不一致。
- ❌ **让 `/compact` 单独占一个回合**：压缩本身只调摘要模型，占一个完整回合是
  无谓的生命周期开销；且「回合」在 `CONTEXT.md` 里是「一次输入进入 Runtime 到产出结果」，
  压缩没有用户输入。
- ❌ **只加提示文案、不改投递路径**：见「为什么不是加一行提示就够」。

## 后果

**正面**

- `/compact` 的生效延迟从「整个回合」降到「下一个轮边界」（工具轮询命中时更快）。
- 手动与自动压缩共用一份实现，两条路径的事件与失败语义结构性一致。
- 提交即有一行反馈，用户不再面对「毫无反应」。

**负面（诚实记录）**

- `Boundary` 多一个变体 ⇒ 所有穷尽 `match` 需补臂（编译器强制，改动集中在
  `drain_boundary` 与 `cancel_on_abort`）。
- **命令期间**（`cancel_on_abort` 等命令结果）到达的 `Compact` 被**丢弃**：
  命令不是回合，没有轮边界可消费它。用户重敲一次即可 —— 与 `Steer` 在该路径上的
  既有处理一致。
- `Compact` 与 `Abort` 同队时按 FIFO：先压再取消（白烧一次摘要调用）。
  未做优化 —— 用户同时点两者是罕见路径，而提前扫队会引入新的顺序语义。
