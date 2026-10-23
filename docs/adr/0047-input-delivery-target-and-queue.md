# 0047 — 回合中输入的投递去向：显式目标 + app 侧跨回合队列

让**回合进行中**提交的用户输入有一个**明确的生效时机**，而不是被动等到整个回合结束。

## Status

**accepted (partial)**（2026-10-22；依据 `docs/arch/input-delivery-latency/context.md` 的现状调查
与 `docs/arch/input-delivery-latency/design.md` 的三方案对比）

> **「partial」的含义**：**协议与队列的设计已定并 accepted，但代码尚未落地**。
> 评审（`docs/arch/input-delivery-latency/review.md`）判 **「Request changes —— 暂不可实现」**，
> 列出六项实施闸门 G1–G6；其中 **G2 已由 [ADR-0049](0049-final-answer-boundary-drain.md) 落地**、
> **G4 已随 `8a08bd6` 解决**；**G1 / G3 / G5 / G6 未做**。
> 设计 §7 仍有 **3 项「待用户确认」**。
>
> ⇒ **本 ADR 描述的是「尚未实现的目标形态」**，不是当前 as-built。
> 读它之前先看 `review.md` 的闸门表与 `docs/TODO.md` §C。

## 背景

### as-built（2026-10-22 上午核实；⚠️ 当日傍晚代码已变）

> ⚠️ **本节描述的是 2026-10-22 **上午**的源码状态**。当日傍晚 `8a08bd6` 与
> `6bb8552`（ADR-0049）先后落地 ⇒ 下表「生产者」列与「只缺生产者」的结论**已不成立**。
> **仍然成立的那一半**：**「下一回合」语义（`NextTurn`）仍缺协议格与队列** ——
> `Boundary` 仍无 `NextTurn` 一格，app 侧无跨回合队列。

用户输入在当前实现里只有**两个**去处：

| 去处 | 消费者 | 生效时机 | 生产者 |
|---|---|---|---|
| `Request::Prompt(Message)` | `app_loop` 外层循环（`apps/coding-agent/src/app_loop.rs`） | **整个回合结束后** | TUI `submit()`（`crates/ys-tui-coding/src/events.rs`） |
| `Boundary::Steer(Message)` | `BasicLoop` 轮边界（`crates/ys-loop/src/basic.rs`） | 下一轮模型调用前 | ✅ **已接上**（`8a08bd6`） |

回合内的 `select!`（`apps/coding-agent/src/app_loop.rs`）只监听 `sink_rx`
与 `boundary_rx`，**不监听 `request_rx`** ⇒ 回合中提交的 `Request::Prompt`
在信道里排队，直到回合结束才被取出。

`Boundary::Steer` 这条通路**完全实现了**（拉取点、`session.append`、`UserMessage` 事件
都齐备）。缺的不是性能，是**「下一回合」的协议格**与**投递去向的可见性**。

### 四个静默丢弃点

> ⚠️ **本节写于 2026-10-22 上午；其中两条已不成立**（`8a08bd6` / `6bb8552` 修复）。
> 下表保留是为了说清**当初为什么要做这套设计** —— **仍然成立的那两条**见备注。

若只把 `Steer` 接上生产端，消息仍会在下列位置**静默**丢失：

| 丢弃点 | 位置 | 后果 | 现状 |
|---|---|---|---|
| 回合末未被轮边界消费 | 模型直接给最终答复 ⇒ 不再有下一次 `take()` | 消息滞留，下一回合头部排空时被丢 | ✅ **已修**（`6bb8552` / ADR-0049：收场前也拉一次边界） |
| 回合头部排空 | `apps/coding-agent/src/app_loop.rs` `while ctx.boundary_rx.try_recv().is_ok() {}` | 同上，**无差别丢弃** | ✅ **已修**（`8a08bd6`：改为只丢 `Abort`、保留 `Steer`） |
| 命令期间 | `cancel_on_abort` 收到 `Steer` **直接 `continue`** | 同上（有既存测试 `cancel_on_abort_ignores_steer` 钉住「丢弃」语义） | ⚠️ **仍在**（但该路径只跑命令、不跑回合 ⇒ 收不到 `Steer`） |
| 回合已结束但 TUI 仍认为在跑 | 消息落在回合边界之外 | 同上 | ⚠️ 仍在（app 侧是权威 ⇒ 后果是多起一个回合而非丢失） |

⇒ **「绝不丢失」** 是硬需求，而只接生产端**不满足**。

### 与 ADR-0013 / ADR-0014 的关系

- **ADR-0013**（取消即消息）删掉了旧 `Inbox` 的 `follow_up` 路，
  而 `Boundary` 枚举只有 `Steer` / `Abort` 两格（`crates/ys-protocol/src/boundary.rs:12-16`）
  ⇒ 「排到下一回合」这一路**连格子都没有**。
- **ADR-0014**（follow-up）定义的「系统发起的追问」正是这条路，**代码零落地**
  （`docs/TODO.md` §B）—— 本 ADR 补上它的协议格与队列。

## 决策

### 决策 1：默认语义 = **「下一轮生效」**（`Steer`），不提供「下一回合」的用户显式选择

「下一轮」与「下一回合」是**两件事**，不是同一件事：

| 语义 | 时机 | 载体 | 会话连续性 | 模型可见性 |
|---|---|---|---|---|
| **Steer**（下一轮） | 下一轮模型调用**前** | `Boundary::Steer` | 插入**当前**会话历史 | 本回合的下一轮即可见 |
| **NextTurn**（下一回合） | 本回合**结束后** | app 侧队列 | **新**回合，接在同一会话上 | 本回合从头到尾**看不见** |

**为何默认「下一轮」**：用户要的是「别让我等到整个回合结束」。
「下一轮」把等待从「整个回合」缩短到「当前这一轮」。

**「轮边界总是存在」这条前提曾经是假的，已修正（2026-10-22）**：
初版本决策的依据是「『当前这一轮』的下界由轮边界决定，与模型这轮还调不调工具无关
（轮边界总是存在）」—— 但**模型给出最终答复那条路不经过下一轮**，
于是「模型正要收尾时到达的插话」会随本回合的边界源一起销毁（静默丢失）。
现在**收场前也拉一次轮边界**（`crates/ys-loop/src/basic.rs` 的 `drain_boundary`）
⇒ 该前提**成为真的不变量**，本决策的依据成立。
详见 `docs/adr/0049-final-answer-boundary-drain.md`。

**为何不给用户「下一回合」的显式选择**：模型这轮还调不调工具**不可预知**。
让用户选「下一回合」会产生一个**撒谎的承诺** ——
说了「下一回合」，模型这轮又调了 5 次工具，实际等了几分钟。
⇒ 用户真正要的是「别让我等到整个回合结束」，**统一 Steer**。

**「下一回合」这一格留给系统发起的 follow-up**（那里时机由代码决定，不由用户选）。

### 决策 2：投递去向做成**协议一等公民**（`DeliveryTarget`），但 **`Boundary` 枚举不动**

```rust
// crates/ys-protocol/src/delivery.rs（新增文件）

/// 一条待投递的用户/系统输入及其去向。
pub struct Delivery {
    pub message: Message,
    /// 去向：`Steer` 下一轮生效；`NextTurn` 下一回合生效。
    pub target: DeliveryTarget,
    /// 发起方：用户输入 vs 系统发起（ADR-0014 follow-up）。
    pub origin: DeliveryOrigin,
}

pub enum DeliveryTarget { Steer, NextTurn }
pub enum DeliveryOrigin { User, System }
```

**为什么 `Boundary` 不加第三格**：`Boundary` 是**轮边界**语义（`BasicLoop` 消费，
`basic.rs`），「下一回合」是**回合边界**语义（`app_loop` 消费）—— **两者错层**。
把 `NextTurn` 塞进 `Boundary` 会迫使 `BasicLoop` 理解「回合」这个它不该管的概念。

**为什么 `Delivery` 是新类型而不是 `Boundary` 的变体**：`Delivery` 是**跨回合存活**的队列条目，
`Boundary` 是**回合内的瞬时信箱**条目。两者生命周期不同 ⇒ 类型分开。

**兼容性**：`Delivery` 是**新类型**，不改动 `Request` / `Boundary` 任何既有变体的 serde 形态
⇒ **零协议破坏**。

### 决策 3：**app 侧**最小队列是唯一状态源（单处真相）

队列住在 `app_loop`（`apps/coding-agent`），**不进 `ys-protocol` 的组件层**。

**为什么不是独立的投递层组件**（候选方案 C）：它需要引入
「投递状态机 + 模式枚举」共 5 个新概念，而 MVP 阶段真实的状态机只有
「排队 → 已注入 → 已生效 / 已降级」三态 ⇒ **过度设计**。
且独立投递层与既有 `Boundary::Steer` 构成**两处真相**
（投递层记一份「是否已生效」、`BasicLoop` 记一份「是否已 take」）⇒ 排障要对账。

**队列必须跨回合存活** —— 这是「绝不丢失」的唯一保证。

### 决策 4：UI **不传**投递目标；**app 侧是唯一权威**

`Request::Prompt` 保持 `Request::Prompt(Message)` 不变。
投递目标由 **app 侧**根据「当前是否真的在跑回合」推导。

**理由**：UI 侧的 `is_turning` 是**本地推测**（靠事件推断，最长滞后一个 poll 周期，
见 `crates/ys-tui-coding/src/app.rs:394` 的注释）⇒ UI 猜「我现在该不该 steer」
会引入**错判窗口** —— 猜错就是消息插错回合。
app 侧知道回合是否真的在跑 ⇒ **app 侧是唯一权威**。

### 决策 5：回合内 `select!` 新增一支 `request_rx` 分支 —— **顺序消费，不引入独立信道**

候选方案 C 担心「外层循环与回合内 `select!` 同时消费同一个 `request_rx` 会争抢」。
核实：**不存在争抢** —— 外层循环在 `run_turn_once` 期间**根本不在 `recv()`**
（它 `await` 在 `run_turn_once` 上，`app_loop.rs` 的 `recv()` 只在外层循环跑）。
⇒ 回合内 `select!` 收走的只会是**回合期间**到达的请求；回合结束后外层循环接着收。
⇒ **不需要独立信道**（这也避免了「第二个消费者随机抢走消息」的隐患）。

⚠️ 但回合内收到的**非 `Prompt`** 请求（命令）必须**单独记账**，
不能在外层循环外执行 —— 它们要在回合结束后按序执行。

### 决策 6：三个丢弃点改为**分类处理**，`Steer` 一律不丢

| 位置 | 现状 | 改为 |
|---|---|---|
| 回合头部排空（`app_loop.rs`） | 无差别丢弃 | `Abort` 丢弃（其标志位随新 source 作废，丢弃安全）；**`Steer` 转 `Delivery{NextTurn}` 放回队列** |
| `cancel_on_abort`（`app_loop.rs`） | `Some(_) => continue`（丢弃） | 同上。⚠️ 既存测试 `cancel_on_abort_ignores_steer` 需随语义更新 |
| 回合末未消费的 `Steer` | 无兜底 | 从当回合的 `QueueBoundarySource` 取回残留 ⇒ 转 `Delivery{NextTurn}` 放回队列 |

⇒ **「绝不丢失」的保证由三处分类丢弃 + 队列跨回合存活共同构成。**

> ⚠️ **2026-10-22 更新**：前两行**已由 `8a08bd6` 落实**（排空改为只丢 `Abort`）；
> 第三行**已由 `6bb8552`（ADR-0049）从根上消除** —— 收场前也拉一次边界源，
> ⇒ 回合末**不再有未消费的残留**。本表描述的是**当初的设计依据**。

### 决策 7：投递状态**不新增回执事件**；TUI 本地维护

`Delivered` 的**唯一**信号是 `AgentEvent::UserMessage`（**已存在**）⇒
UI 收到它即可把本地行从「已提交」标为「已生效」。

⇒ **不新增 `AgentEvent` 变体**，不引入 `DeliveryStatus` / `DeliveryAck` 往返协议。
（候选方案 B/C 提出的这两者在最终形态里是**多余的一层**。）

### 决策 8：用户输入与系统 follow-up **共用同一条队列**，用 `origin` 区分

两者**时机相同**（「回合后另起一轮」），但**协议意图不同**。
共用队列 ⇒ 一套队列语义、一套消费点；`origin` 字段区分意图与可见性。

**这同时补上 ADR-0014 follow-up 缺的协议格** ⇒ `docs/TODO.md` §B 的 Follow-up 条目
从「代码零落地」升级为「协议格已备，待实现」。

## 不做什么

- **不改 `Boundary` 枚举**（决策 2）。
- **不改 `Request` 的既有变体形态**（决策 4）⇒ 零协议破坏。
- **不引入独立的投递层组件 / 新 crate**（决策 3）。
- **不新增 `AgentEvent` 变体或投递回执协议**（决策 7）。
- **不给用户「下一回合」的显式选择**（决策 1）。
- **不改 `BasicLoop` 的**轮**边界拉取点**（每轮开头那处保持不动）。
  ⚠️ **2026-10-22 修订**：初版本写的是「`basic.rs:306` 保持不动」，但**收场前**新增了一处拉取
  （见 ADR-0048）—— 那不是「轮边界拉取点」的改动，而是**新增一个拉取时机**，
  使「轮边界总是存在」从不变量缺口的修补。
- **不引入独立信道**（决策 5）。

## Considered Options

### 选项 A：只补 `Steer` 生产者（最小复杂度）

- **优点**：协议变体 **+0**；改动 4 个文件；概念最少。
- **缺点**：**四个静默丢弃点**（见「背景」）⇒ 不保证「绝不丢失」。
  用户看到输入框里的文字消失，没有任何提示。
  且「这轮生效还是下一轮生效」**隐式取决于模型这轮还调不调工具** ⇒ 不可预期。
- **不采纳**：静默丢消息比晚点生效严重得多。

### 选项 B：语义显式 + app 侧最小队列（**采纳**）

见「决策」全文。代价：协议面从 2 个投递概念扩到 4 个；多一个状态面需要排障。
**这笔预付划算** —— `Delivery.origin = System` 已有真实消费者（ADR-0014 follow-up）。

### 选项 C：独立投递层（`ys-protocol` 新组件）

- **优点**：概念上最接近 pi / Claude Code / Codex。
- **缺点**：① 需要「回合内 `select!` 另加一支 `request_rx`」⇒ 双消费者争抢
  （`tokio::mpsc::Receiver` 是单消费者语义）⇒ 必须引入独立信道 ⇒ **已经多了一个协议载体**；
  ② 与 `Boundary::Steer` 构成**两处真相**；③ 5 个新概念换 3 个状态。
  其产出 subagent 自己也承认：**MVP 阶段做独立组件是过度设计**，
  「投递层 = app_loop 里的一个 `Vec`」才是诚实结论。
- **不采纳**（其 MVP 形态并入选项 B）。

### 选项 D：给 `Boundary` 加第三格 `NextTurn(Message)`

- **优点**：协议变体 +1，比选项 B 少。
- **缺点**：`Boundary` 是**轮边界**语义（`BasicLoop` 消费），
  `NextTurn` 是**回合边界**语义（`app_loop` 消费）⇒ **错层**；
  且 `Boundary` 消息生命周期与回合绑定 ⇒ **跨回合存活问题原样保留**（仍需队列）。
  ⇒ **加了格子，却没解决「不丢」**。
- **不采纳**：格子加在了错的层。

## 风险与规避

| 风险 | 规避 |
|---|---|
| 回合内 `select!` 收 `request_rx` 与外层循环争抢 | **顺序消费**：外层循环在 `run_turn_once` 期间不在 `recv()`（决策 5） |
| 回合中提交的命令（`/login` 有 5s 网络往返）被排队到回合结束 | 待用户裁决（设计 §7）—— 是否即时拒绝 |
| skill 展开在回合内路径被漏掉（`/skill-name` 走成裸文本） | 回合内路径**必须也**调 `expand_skill_invocation`（`app_loop.rs:1195`）⇒ 测试覆盖 |
| TUI 的 `is_turning` 滞后 ⇒ 消息被当回合中提交 | 后果是**多起一个回合**而非丢失；app 侧是权威（决策 4） |
| `request_rx` 容量 64（`main.rs:503`）在回合内被填满 | 回合内 `select!` 持续消费 ⇒ 不会满；>64 条则 UI 线程阻塞（非丢失） |
| 既存测试 `cancel_on_abort_ignores_steer` 钉住「丢弃」语义 | 该测试**须随语义更新**（决策 6）—— 这是有意的语义变更，不是回归 |

## 测试矩阵

| 用例 | 位置 | 断言 |
|---|---|---|
| 回合中提交 ⇒ 下一轮生效 | `apps/coding-agent/src/app_loop.rs` | mock model 的**第二次**调用上下文里**含**该消息 |
| 回合中提交 ⇒ 回合末未消费则降级 | 同上 | 回合末后队列里**有**该消息；下一回合**首个**模型调用上下文含它 |
| 头部排空**保留** `Steer` | 同上 | 排空后队列非空 |
| `cancel_on_abort` **不丢** `Steer` | 同上 | 命令结束后队列里有该消息（更新既存用例） |
| 回合中提交 `/命令` ⇒ 回合后按序执行 | 同上 | 命令在回合结束后执行，顺序保持 |
| 回合中提交 `/skill-name` ⇒ 展开 | 同上 | 模型收到的正文是展开后的信封 |
| 空闲时提交 ⇒ 立即起回合 | 同上 | **既有行为不变** |
| `Delivery` 序列化形态 | `crates/ys-protocol/src/delivery.rs` | serde 快照稳定 |
| 既有路径逐字节不变 | 全仓既有测试 | `cargo test --workspace` 全绿 |

**变异测试要求**（AGENTS.md 约定）：至少 3 处
—— ① 把头部排空改回无差别丢弃 ⇒ 「排空保留 `Steer`」用例红；
② 把回合末残留兜底去掉 ⇒ 「降级到下一回合」用例红；
③ 把 `cancel_on_abort` 的 `Steer` 分支改回丢弃 ⇒ 对应用例红。

## 与其它文档的关系

- **ADR-0013**（取消即消息）—— `Boundary` 保持两格（决策 2）；
  `Abort` 永久置位的语义**不变**。
- **ADR-0014**（follow-up）—— 本 ADR **补上它的协议格**（`Delivery{origin: System}`）⇒
  `docs/TODO.md` §B 的 Follow-up 条目状态随之更新。
- **ADR-0046**（可取消的模型调用）—— 取消链路独立；本 ADR 的「回合中提交」与之正交。
- `docs/arch/hooks-and-plugins/notes.md:60` — 竞品调研（pi 的 `deliverAs` 三路）。
- `docs/arch/input-delivery-latency/context.md` — 现状调查与根因。
- `docs/arch/input-delivery-latency/design.md` — 完整设计（场景表、时序图、语义边界表）。
