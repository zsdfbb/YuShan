# 回合中输入投递通路 — 架构评审

> 评审对象：[`design.md`](./design.md) 与 [`ADR-0047`](../../adr/0047-input-delivery-target-and-queue.md)。
> 评审日期：2026-10-22。评审方式：**源码核对**（不采信文档的 as-built 叙述）。
> 结论：**Request changes —— 架构方向正确，但保证链与实现要点有阻断级缺口，暂不可实现。**

---

## 0. 评分

自定评分表（每项 0–2 分，满分 10）：可行性 / 正确性 / 协议设计 / 可验证性 / 问题定位。

| 维度 | 分 | 判定 | 依据 |
|---|---|---|---|
| 问题定位 | **2** | 🟢 | 根因（「协议格缺失 + 无生产者」）定位准确；三条「去处」表与源码一致；**「外层循环与回合内 `select!` 不争抢 `request_rx`」的核实是对的**（见 §2.1） |
| 可行性 | **1** | 🟡 | 架构骨架可实现（`ys-protocol` 新文件 + app 侧 `Vec`），但触发机制、残留回收的落地位置、状态机载体均未指定 |
| 正确性 | **0** | 🔴 | 两条核心保证（「绝不丢失」「下一轮生效」）在源码下**都不成立**（F1 / F2）；TUI 回声机制会自伤（F4） |
| 协议设计 | **1** | 🟡 | `Delivery` 三类型形状合理、零破坏、`Boundary` 不动是对的；但 `origin` 无处可去（F5）、状态机无载体（F6） |
| 可验证性 | **1** | 🟡 | 测试矩阵与变异要求写得扎实，但矩阵里**没有一条能抓到 F1/F2**（原因见 F9） |
| **合计** | **5 / 10** | | |

**裁决**：`Request changes`（不否决方向，但需修订后方可实现）。
**实施闸门**：F1 / F2 / F3 / F4 / F6 五项修订完成并回写 ADR-0047 后，方可进入实现。

---

## 1. 结论摘要

设计的**架构选择是对的**，且理由成立：

- 「投递去向做成协议一等公民」比「隐式取决于模型这轮还调不调工具」更可预期 —— 这条判断正确。
- `Boundary` 枚举**不动**（`Steer`/`Abort` 两格保持轮边界语义），`Delivery` 另起类型 —— 层次切分正确，与 ADR-0013 的收敛方向一致。
- 队列住在 **app 侧**、跨回合存活，而不是塞进 `ys-protocol` —— 依赖方向正确（`ys-protocol` 零 tokio 约束不破）。
- **UI 不传投递目标，app 侧是唯一权威** —— 正确，避开了 `is_turning` 滞后窗口的错判风险。

问题**不在架构脊柱**，在**保证链与实现要点**：文档承诺了两条「绝不丢 / 下一轮就生效」的保证，而源码里这两条都不成立；而承载保证的机制（回声抑制、投递状态机）按文档写法会互相打架。

---

## 2. 已核实为「设计正确」的部分（不作为问题）

### 2.1 `request_rx` 无双消费者争抢 ✅

ADR-0047 决策 5 与 design §3 方案 C 的「致命缺陷 1」互相矛盾，**ADR 是对的**：

- 外层循环 `apps/coding-agent/src/app_loop.rs:329` 起，`await` 在 `request_rx.recv()`（`:340`）；
- `Request::Prompt` 分支（`:375`）内 `await run_turn_once(...)`（`:419`）；
- `run_turn_once` 内的回合 `select!`（`:1021`）消费 `request_rx` 时，外层循环**正挂在 `run_turn_once` 上，不在 `recv()`**。

⇒ 两者**顺序互斥**，不存在 `tokio::mpsc::Receiver` 的双消费者抢消息。design §3 对方案 C 的否决理由**不成立**（但结论仍可接受 —— 方案 C 的另一条「两处真相」成立）。

> 建议：把这条从「方案 C 的致命缺陷」改成「方案 C 的**伪**缺陷，已核实排除」，避免后人重犯同一个误判。

### 2.2 其他已核实无误的点

| 设计断言 | 核实结果 |
|---|---|
| 回合内 `select!` 只监听 `sink_rx` / `boundary_rx` | ✅ `app_loop.rs:1021-1040` |
| `Boundary::Steer` 通路齐备、只缺生产者 | ✅ 拉取点 `basic.rs:305-337`、`session.append`（`:310`）、`emit(UserMessage)`（`:313`）全在；全仓无生产侧 |
| `Agent` 无状态、每回合新建 `QueueBoundarySource` | ✅ `app_loop.rs:998`；`Abort` 标记随之作废（`boundary.rs:82-88`） |
| 子 agent 不会偷走父的 steering | ✅ `ChildBoundary::take()` 只读 `own`（`subagent.rs:95-97`），`is_aborted` 才探父（`:98-99`） |
| `request_rx` 容量 64 | ✅ `main.rs:503`，信道 `:2250` |

---

## 3. 阻断级问题

### F1 🔴 降级到「下一回合」的投递**没有触发器** —— 可能永远不生效

**证据链**

- 外层循环唯一的唤醒源是 `request_rx.recv()`（`app_loop.rs:340`），**只有 `Request::Prompt` 会起回合**（`:375` → `:419`）。
- `DeliveryQueue` 里的 `NextTurn` 条目既不被任何 `select!` 轮询，也没有任何唤醒机制（设计 §5.5 的 `DeliveryQueue` 只有 `push` / `take_next_turn` / `requeue` 三个方法，全仓库不存在任何自动起回合的代码）。
- design §5.3 的兜底链条末端写的是「**下一回合开始时消费**」（`design.md:269`、`:371`）—— 但「下一回合」由**用户再提交一次**才开始。

**后果**

- design §5.7 **S7**（`:335`，回合末未消费的 `Steer` 降级）与 **S5**（`:333`，取消后已提交消息保留）落进队列后，若用户不再提交任何东西，**这些消息永远不会被执行**。
- ADR-0047 的「**绝不丢失**」因此退化为「**不丢，但可能永远不生效**」—— 从用户视角（输入框里的文字还在、状态显示「已排队」、模型却永远不回应）与**丢失**难以区分，且比丢失更坏：丢失至少有「没了」的确定感。
- **新增的活性缺口**（设计引入的，原代码没有）：回合内 `select!`（`app_loop.rs:1021`）在 `turn` future 与 `request_rx` **同时就绪**时，`tokio::select!` 默认**随机**选一个分支。⇒ 一条恰好在回合末尾到达的 `Request::Prompt` 会被回合内分支捕获、转成 `Steer`、因回合已结束而成为残留、降级到 `NextTurn`、然后**永远不动**。今天这条消息会由外层循环取走并正常起一个回合。

**修复要求**（必须选一种，并写进 ADR）

1. **回合后自起回合**：`run_turn_once` 返回后，若投递队列非空 ⇒ **不再回到 `request_rx.recv()`**，而是在外层循环内直接起下一回合（形状与 ADR-0014 follow-up 的「回合结束后起一轮」一致，`docs/design-plans/2026-09-15-follow-up.md:61`）。同时必须处理「自动回合结束后队列仍非空」的**连锁**，并设上限（防套娃，ADR-0014 已有 `follow_ups` 上限先例）。
2. **显式唤醒**：`DeliveryQueue` 带一个 `Notify`（或 `watch`），外层 `select!`（`app_loop.rs:336-340`，注意 `models_rx` 那支也要一起 race）同时等 `request_rx` 与该唤醒源。

> 附带实现点：`request_rx` 目前**不在 `TurnCtx` 里**（`app_loop.rs:835-863` 只有 `sink_rx` / `boundary_rx`）⇒ 回合内 `select!` 要消费它，得先把 `&mut mpsc::Receiver<Request>` 穿进 `TurnCtx`。§6 实现要点未提这一步。

---

### F2 ✅ 已修（2026-10-22，ADR-0049）—— 「下一轮生效」已成立

> **状态**：本文评审时的源码事实（见下）仍然准确；**已按「B 路」修复**，
> 落地见 [`docs/adr/0049-final-answer-boundary-drain.md`](../../adr/0049-final-answer-boundary-drain.md)。
> 关键变化：轮边界拉取被抽成 `drain_boundary(ctx)`，在**每轮开头**（原有）与
> **收场前**（新增）两处调用 ⇒ 「轮边界总是存在」从**假前提**变成**真不变量**。
> ADR-0047 的「不改 `BasicLoop` 拉取点」那条已相应修订。

ADR-0047 决策 1 明写：

> 「『当前这一轮』的下界由**轮边界**决定，与模型这轮还调不调工具无关（**轮边界总是存在**）。」

design §3 的 back-of-envelope 同样据此写「最好 = 下一个轮边界（毫秒级）／最坏 = 当前模型调用剩余时间」。

**源码事实**：`BasicLoop` 的轮边界拉取点**只有一个**（`basic.rs:305-337`，`take()` 在 `:306`），位于**每轮开头**、组装 `ModelRequest` 之前。而模型给出**最终答复**的那条路（`tool_calls.is_empty()`，`basic.rs:566`）**直接 `return`**（`:617-623`），**不会再走一次轮边界拉取**。

⇒ **模型正在输出最终答复时到达的 steering，永远不会被本回合消费** —— 它必然成为残留、降级到下一回合。

**后果**

- ADR-0047 决策 1 的核心论证（「用户真正要的是『别让我等到整个回合结束』⇒ 统一 Steer」）**在最常见的场景下失效**：模型正要收尾时用户插话，等的正是「整个回合结束」，而不是「下一轮」。这恰恰是用户最需要插话的时刻。
- 「轮边界总是存在」是**假前提**，且它同时支撑了 ADR 的**决策 1**、**不做什么**里的「不改 `BasicLoop` 的轮边界拉取点（`basic.rs:306` 保持不动）」，以及 design §3 的延迟估算。⇒ 这三处需要一并修订。
- 叠加 F1：这类 steer 会**既不生效、又永远不生效**。

**修复要求**（二选一，不可回避）

- **A. 承认现实并改口**：把承诺从「下一轮生效」改成「**下一轮或下一回合**（不可预知）」，并把这一不可预知性**如实显示给用户**（当前 design §5.6 写「提交即知『下一轮生效』」，是**错的**）。
- **B. 让轮边界真的总存在**：在 `basic.rs:566` 的最终答复路径 `return` **之前**补一次拉取；若取到 `Steer` ⇒ 追加进会话并 `continue` 本轮循环（而不是收场）。这让保证为真，但**直接违反 ADR-0047「不改 `BasicLoop` 的轮边界拉取点」这条「不做什么」** ⇒ 需要显式修订该条。

> 无论选 A 还是 B，都必须补一条测试：**模型返回最终文本（无工具调用）时到达的 steer，其生效回合是什么**。当前测试矩阵没有这一格。
>
> ✅ **已补**：`crates/ys-loop/src/basic.rs` 的 `steer_arriving_during_final_answer_is_not_lost`
> （断言插话进会话 + `rounds == 2` + 发出 `UserMessage`）与
> `apps/coding-agent/src/app_loop.rs` 的 `steer_during_final_answer_reaches_the_model`（端到端）。
> **变异已实测**：两处各自注入「跳过收场前的拉取」⇒ 对应用例**均变红**。

---

## 4. 重要问题

### F3 🟠 降级队列与新到达 prompt 的**组合方式与执行顺序未定义**

- design §5.5 的 `take_next_turn()` 返回 **`Vec<Delivery>`**，而 `run_turn_once` 只接受**一条** `AgentInput`（`crates/ys-loop/src/input.rs:4-6`：`pub struct AgentInput { pub message: Message }`）。
- ⇒ N 条降级投递**无法**直接喂进一个回合。到底是「合并成一条 `Message`（文本拼接）」还是「连起 N 个回合」，文档没说。
- 更关键的缺口：**降级队列里已有的条目 vs 用户新提交的一条 prompt，谁先跑？**
  - 场景：回合 1 内提交 A ⇒ 降级；用户再提交 B ⇒ 起回合 2。
  - 可能的语义：(i) A 先、B 后（两个回合）；(ii) A 合并进回合 2 的输入、B 是回合 2 的输入（A 先 B 后但同一回合）；(iii) B 先、A 后。
  - design §7 只裁决了「**命令与消息**谁先」（先命令后消息，`:424`，且标「待用户确认」），**没有裁决「降级消息与新消息」谁先**。
- ADR-0047 决策 5 的 ⚠️ 只说「回合内收到的非 `Prompt` 请求（命令）必须单独记账」，**只覆盖命令，不覆盖投递**。

**修复要求**：明确「降级队列 vs 新 prompt」的顺序与组合形态，并把 `take_next_turn() -> Vec<Delivery>` 与 `AgentInput` 的单条约束对齐（要么改接口为逐条消费 + 回合内串接，要么明确合并规则并写死文本拼接格式）。

---

### F4 🟠 TUI 回声机制**互相打架**：design §6 第 4 条的事实前提为假

design §6 第 4 条断言：

> 「`pending_echo_texts`（`app.rs:117`）的认领机制**必须保留** …… Steer 路径下回声**在本回合内**抵达（`is_turning == true` ⇒ 走 `app.rs:280` 的用户行分支），与 `pending_echo_texts` 的场景**不重叠**。」

**源码事实（三条，与之矛盾）**

1. `begin_local_turn`（`app.rs:394-412`）对**回合中提交**恰恰**就是**登记 `pending_echo_texts` 的那个分支：
   ```rust
   if self.is_turning {
       self.pending_echo_texts.push_back(text.clone());   // app.rs:399-401
   }
   self.transcript.push(TranscriptLine::User(text));
   self.suppress_user_echo += 1;                          // app.rs:404
   ```
   ⇒ 「回合中提交」与「登记 `pending_echo_texts`」**完全重叠**，不是「不重叠」。
2. Steer 回声**到不了** `app.rs:280`：`apply_event` 的第一道闸是 `suppress_user_echo > 0`（`app.rs:247-250`）—— 回合中提交刚把它加到 1（`:404`）⇒ 事件在此**递减并 `return`**，永远到不了 `:280` 的 `is_turning` 分支。
3. `pending_echo_texts` 的**唯一**认领点在 `app.rs:262`，其闸是 **`!is_turning`** —— Steer 路径下 `is_turning == true`，该闸**恒不成立**。

**后果**

- 每次回合中提交，都会给 `pending_echo_texts` 留下一条**永不被认领的残留**（`app.rs:224` 只在 `reset_for_replay` 清，回合结束不清）。
- 残留条目按**文本相等**匹配（`app.rs:262`：`position(|t| t == &text)`）⇒ 之后任何一条**文本相同**的 `UserMessage` 在 `!is_turning` 时抵达（例如回合收尾竞态下的迟到事件）会被**误认领**：该消息**不渲染**（`app.rs:265-269` 直接 `return`），且 `is_turning` 被**错误置真**（`:265`）⇒ UI 以为在跑回合（实际没跑）⇒ 用户后续提交会被误判为「空闲提交」而**另起一个回合**，而不是 steer。
- 更根本的是：**在 Steer 设计下，`pending_echo_texts` 的合法消费场景（回合中提交 → 排到后续回合 → 回声迟到）被设计本身消灭了**（回合中提交不再排到后续回合）⇒ 该机制在新设计下**只承担负债、不承担保护**。

**更深的协议矛盾（design 未察觉）**

TUI 必须在**提交时**决定要不要登记待回声，但**投递目标是 app 侧推导的**（ADR-0047 决策 4：UI 不传 target，app 侧按「是否真的在跑回合」判定）⇒ **TUI 在提交时无从知道**这条提交会走 Steer（回声在本回合内抵达、需立刻抑制）还是走 NextTurn（回声在下一回合抵达、需登记待认领）。

⇒ 现有两个机制**互斥且不兼容**：`suppress_user_echo`（计数、先到先得）与 `pending_echo_texts`（文本匹配、跨回合）—— 同一个事件先撞 `:247` 就永远到不了 `:262`。design §6 必须显式规定**二者的交互与清理规则**，否则实现者只能各写各的。

**修复要求**：明确 echo 认领的单一机制。候选：

- **简化路线**：既然 Steer 回声在本回合内抵达 ⇒ 提交时的计数抑制（`suppress_user_echo`）已足够；**回合中提交不再登记 `pending_echo_texts`**，并把该机制的适用面收窄为「确实排到后续回合的投递」（即仅 `NextTurn` 路径）。但因目标由 app 推导，**TUI 无法在提交时区分** ⇒ 需要 app 侧回传一个「这条提交的实际去向」信号（见 F5）。
- **关联路线**：给投递加 ID 并随事件回传（见 F5），TUI 凭 ID 精确认领。

---

### F5 🟠 `AgentEvent::UserMessage` 无关联 ID ⇒ 「逐条标为已生效」做不到；`origin` 也无处可去

- ADR-0047 决策 7：「`Delivered` 的**唯一**信号是 `AgentEvent::UserMessage`（**已存在**）⇒ UI 收到它即可把本地行从「已提交」标为「已生效」」；且「**不新增** `AgentEvent` 变体」。
- 源码事实：`AgentEvent::UserMessage { message: Message }`（`crates/ys-event/src/event.rs:22-24`）—— **没有投递 ID、没有 origin**。
- ⇒ **逐条认领不可能**：
  - **重复文本**：两条相同的提交 ⇒ 两条待认领本地行 ⇒ 一条 `UserMessage` 无法确定对应哪一条；
  - **系统 follow-up**（`Delivery{origin: System}`）经 `BasicLoop` Step 2（`basic.rs:156-168`）也以 `UserMessage` 形态出现 ⇒ UI 会把一条**系统消息**当成用户行的「已生效」回执，或错配到用户行上。
- ⇒ `DeliveryOrigin` 只存在于 **app 侧队列**里，**没有任何通道把它送到 UI** ⇒ design §2 声称 origin「区分意图与**可见性**」，但「可见性」对谁可见，文档未答；§5.4/§5.6 的「系统 follow-up → 用户可见『系统已排队』」在当前协议下**无实现路径**。

**修复要求**（与决策 7 直接冲突，必须修订 ADR）

- **简化**：放弃逐条状态（只显示**计数**：「已排队 N 条」），`origin` 不必外送；但 ADR「投递去向可见：强（显式 target + 状态回执）」（design §5 矩阵）与 §5.4「UI 把本地行标为已生效」必须相应降级措辞。
- **完整**：给 `AgentEvent::UserMessage` 加**可选**关联字段（`#[serde(default)]`，沿用 `Request::Login.api_base` 的加字段先例）——注意 `AgentEvent` 是 `#[non_exhaustive]` + serde，**加字段会改事件线上形态**，与 ADR「零协议破坏」的说法需对齐（该说法只覆盖 `Request`/`Boundary`，未覆盖事件）。

---

### F6 🟠 投递状态机**没有载体**，与 §5.5 的接口不自洽

- design §5.4 定义四态机（`Queued` / `Injected` / `Delivered` / `Deferred`），并称「app 侧是权威」（`:287`）、「单处真相」（`:289`）。
- 但 §5.5 的 `DeliveryQueue` 接口只有 `push` / `take_next_turn` / `requeue`（`:300-306`）—— **不携带任何逐条状态**；`DeliveryStatus` / `DeliveryAck` 已被决策 7 砍掉；`Delivery` 本身也只有 `message` / `target` / `origin`（`:297`）。
- ⇒ **四态机在 §5.5 的接口下不可实现**：没有地方存「这条投递处于哪个态」。
- 且 `Delivered` 的迁移触发被写成「`UserMessage` 事件」（`:278`）—— 但 app 侧从 `UserMessage` **无法定位到具体某条投递**（见 F5）⇒ 该迁移只能退化为**计数**，而计数无法支撑 §5.4 声称的逐条语义。
- `Deferred → Queued` 的表述「回合末取回残留」（`:279`）还隐含了一个 §5.5 未表达的动作：**把 `QueueBoundarySource` 的 `Arc` 提到回合之外**（当前是 `run_turn_once` 内的局部变量，`app_loop.rs:998`）——见 F7。

**修复要求**：要么给出承载状态的结构（如 `Delivery { id, state, .. }` + `DeliveryQueue` 维护索引），要么**删掉 §5.4 的四态机**、改写为与实际实现一致的**两阶段**描述（「app 已收 / 已交给 `BasicLoop` / 回合末仍在队列」）。当前形态是「文档里有、实现里无处安放」。

---

## 5. 中等问题

### F7 🟡 「四个静默丢弃点」混了两条不同通路；真正吃消息的那条在 §6 实现要点里缺席

ADR-0047「背景」的丢弃点表把下列位置并列为同质的「丢弃点」，但它们属于**两条不同的传输**：

| 被列为丢弃点 | 实际传输 | 本设计下是否吃消息 |
|---|---|---|
| 回合头部排空（`app_loop.rs:952`） | **UI→app** `boundary_rx` | **否** —— UI 侧只发 `Boundary::Abort`（`events.rs:371-377`），全仓 `Steer` 无生产者；本设计的 steering 走 app→`QueueBoundarySource` 直投，不经此信道 |
| `cancel_on_abort`（`app_loop.rs:745-760`，`Some(_) => continue`） | **UI→app** `boundary_rx` | **否** —— 该函数只在 `/login` 拉 provider 目录时调用（`app_loop.rs:500`），是**命令路径**，不是回合路径 |
| 回合末未被轮边界消费 | app→`QueueBoundarySource` 残留 | **是 —— 这才是本设计真正的硬损失边界** |
| 回合已结束但 TUI 仍认为在跑 | — | 不成立（app 侧是权威，ADR 决策 4） |

⇒ ADR 决策 6 的三行表里，前两行是**防御性的**（面向一个尚不存在的生产者），第三行才是承重的。而 design §6「实现要点」列的两条恰好是前两行（排空改为分类丢弃、`cancel_on_abort` 不丢 `Steer`），**真正的承重项（回合末从 source 取回残留）只在 §5.3 的叙述里出现，没进 §6 的实施清单**。

**修复要求**：把「承重路径」写进 §6 实现要点，并注明其**结构性前提** —— `QueueBoundarySource` 的 `Arc` 必须活过回合（`app_loop.rs:998` 当前是 `run_turn_once` 内的局部变量）⇒ 需提到调用方或改为由 app 侧长生命周期持有。

> 附：`cancel_on_abort_ignores_steer`（`app_loop.rs:1937`）在 ADR 决策 6 下须改写 —— 但如上，该路径在本设计下**根本收不到 `Steer`** ⇒ 该测试的语义变更可能是**空改**。建议改为断言「`cancel_on_abort` 期间到达的投递不丢」（若保留此路径）。

---

### F8 🟡 `command_in_flight` 契约与「排队执行的命令」不兼容

- `note_request_sent`（`app.rs:235-241`）：**任何非 `Prompt` 请求**在**投递时**置 `command_in_flight = true`。
- `apply_outbound`（`app.rs:153-155`）：**任何**出站消息都把它清掉。
- 本设计下，回合中提交的 `/login` 要**排队到回合结束后才执行** ⇒
  1. 它的「在飞」标志在**命令还没开始**时就被置真（UI 会显示「可 Esc 取消」，而命令尚未启动）；
  2. 回合期间**大量出站事件**（`Outbound::Event` 等）会**把它清掉** ⇒ 命令真正开始执行时，`command_in_flight == false` ⇒ **Esc 取消能力丢失**。
- ⇒ 排队的命令需要**独立状态**（如 `queued_commands: usize` + 在**执行时**才置真的在飞标志），不能复用「投出即在飞、任何出站即收场」的现有契约。

---

### F9 🟡 文档内部矛盾：声称「已全部裁决」，同一文档里仍有三项「待用户确认」

- design §8（`:436`）称 context.md 的「5 个未决问题（**已于 2026-10-22 全部裁决**）」；
- 而 design §7（`:414-432`）仍列着 **3 项「待用户确认」**：命令与消息谁先（`:425`）、回合中命令是否即时拒绝（`:428`）、投递状态显示落点（`:432`）；
- ADR-0047 风险表同样把「回合中提交的命令（`/login` 有 5s 网络往返）」标为「**待用户裁决（设计 §7）**」——而 ADR 的 Status 是 **accepted**。
- ⇒ **一个 accepted 的 ADR 留了一条顺序规则未裁决**（design §7 已擅自给出「先命令后消息」的取舍），这与 F3 的缺口叠加。

**修复要求**：要么在 ADR-0047 里补裁这三条，要么把 ADR 的 Status 改为 `accepted (partial)` 并把未裁项显式登记进 `docs/TODO.md` §C。

---

### F10 🟢 子 agent 路径未被设计覆盖（覆盖缺口，非缺陷）

design 通篇未提子 agent。源码事实：父回合的 `QueueBoundarySource` 会**同时**传给 subagent runner（`app_loop.rs:1007`：`Some(boundary.clone())`），但 `ChildBoundary::take()` 只读自己的队列（`subagent.rs:95-97`）⇒ **父的 steering 不会被子偷走**（✅ 无 bug）。但两点未覆盖：

1. 父回合被**长时子 agent 派发**占住时，steer 的生效延迟会被放大（轮边界拉取点在派发之后）—— 属延迟特性，应写进文档；
2. 子 agent 自己的输入是否走 `Delivery` 通道？未定义（当前子 agent 走独立 `Message` 构造）。

---

## 6. 测试矩阵的覆盖缺口

ADR-0047 的测试矩阵本身写得扎实（含 3 处变异测试要求，与 AGENTS.md 约定一致）。但：

| 现有矩阵条目 | 为什么抓不到问题 |
|---|---|
| 「回合中提交 ⇒ 回合末未消费则降级：下一回合**首个**模型调用上下文含它」 | 测试会**手工驱动**下一个回合 ⇒ 绕过 F1（无自动触发器） |
| 「回合中提交 ⇒ 下一轮生效：mock model 的**第二次**调用上下文里含该消息」 | mock 会持续产工具调用 ⇒ 绕过 F2（最终答复路径不拉轮边界） |

**建议补充的用例**

1. **活性**：回合结束且投递队列非空 ⇒ **不需要用户再提交**，下一回合自动开始（钉住 F1 的修复）。
2. **最终答复路径**：mock model 返回**无工具调用的最终文本**，在模型调用期间到达 steer ⇒ 断言其生效回合（钉住 F2）。
3. **select! 分支竞态**：回合**最后一刻**到达的 `Request::Prompt` ⇒ 断言最终**要么** steer 生效、**要么**起新回合，**绝不**停在队列里（F1 的新增活性缺口）。
4. **回声**：回合中提交 ⇒ 断言 `pending_echo_texts` **无残留**、且后续**同文本**的 `UserMessage` 仍能正常渲染（F4）。
5. **命令在飞**：回合中提交 `/login` ⇒ 回合结束后执行期间 `command_in_flight` 仍为真、Esc 可取消（F8）。
6. **顺序**：降级队列有 A、用户新提交 B ⇒ 断言 A、B 的执行顺序与回合数（F3）。

---

## 7. 实施闸门（Gate）

**进入实现前必须完成**（建议以 ADR-0047 修订或新增 ADR-0048 的形式落地）：

| # | 事项 | 对应 | 状态 |
|---|---|---|---|
| G1 | 定下**降级投递的触发机制**（回合后自起回合 / 显式唤醒），含连锁上限与 select! 分支竞态的归属 | F1 | ⬜ 未做 |
| G2 | 修订「轮边界总是存在」这一假前提：或补最终答复前的拉取点（并修订「不改 `BasicLoop` 拉取点」），或把承诺降级为「下一轮**或**下一回合」并如实显示 | F2 | ✅ **已做**（2026-10-22，[ADR-0049](../../adr/0049-final-answer-boundary-drain.md)） |
| G3 | 定下「降级队列 vs 新 prompt」的顺序与组合形态，并与 `AgentInput` 的单条约束对齐 | F3 | ⬜ 未做 |
| G4 | 定下**回声认领的单一机制**（`suppress_user_echo` 与 `pending_echo_texts` 的分工与清理），并解决「TUI 提交时不知道投递去向」这一跨层矛盾 | F4 | ✅ **已解**（随 `8a08bd6` 落地：回合中提交走 `note_steered`，**不**登记 `pending_echo_texts`；只有空闲提交走 `begin_local_turn`） |
| G5 | 决定 `origin` 是否外送；若外送，需给事件加**可选**关联字段（并说明事件线上形态的变化）；若不外送，删掉「逐条标为已生效」的措辞 | F5 | ⬜ 未做（依赖 G1/G3 的队列设计） |
| G6 | 给 §5.4 状态机一个载体，或删除它；把「回合末从 source 取回残留」及其 `Arc` 生命周期前提写进 §6 实现要点 | F6 / F7 | ⬜ 未做 |

**建议同期修订**：`command_in_flight` 的排队语义（F8）、ADR-0047 「已全部裁决」与实际未裁项的矛盾（F9）、子 agent 覆盖（F10）。

> **G2 的落地形态**（ADR-0049）：把轮边界拉取抽成 `drain_boundary(ctx)`，
> 在**每轮开头**（原有）与**收场前**（新增）两处调用 ⇒ 「轮边界总是存在」成为真不变量。
> 实测确认：修复前「模型正要收尾时到达的插话」在**本回合与后续回合都不出现**（静默丢失）。
> 变异测试：两处（`ys-loop` 与 app 层）各自注入「跳过收场前的拉取」⇒ 对应用例**均变红**。

---

## 8. 附：评审未采信 / 已排除的疑点

- ❌ **「外层循环与回合内 `select!` 双消费者争抢 `request_rx`」** —— **不成立**，见 §2.1。design §3 对方案 C 的这条否决理由应撤回。
- ❌ **「子 agent 会偷走父的 steering」** —— **不成立**（`subagent.rs:95-97`）。
- ❌ **「`Delivery` 进 `ys-protocol` 会破零 tokio 约束」** —— **不成立**：`Delivery` 只含 `Message`（`ys-core` 的纯数据），与既有 `boundary.rs` 同构。
- ❌ **「回合内 `select!` 新增 `request_rx` 分支会与外层循环产生死锁」** —— **不成立**：顺序消费，外层在 `run_turn_once` 上 await。
- ❌ **「`request_rx` 容量 64 会被填满导致丢消息」** —— **不成立**：回合内持续消费；填满的表现是 UI 线程阻塞（非丢失），且 design §7 已记录。
