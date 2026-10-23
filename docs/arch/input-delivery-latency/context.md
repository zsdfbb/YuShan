# 回合中输入的投递通路 — 架构上下文

> 产出：`/arch-explore`。用途：为「**用户在 agent 工作过程中输入，却得不到及时回复**」界定现状、
> 根因与范围，供后续 `arch-design` 使用。证据均带 `文件:行号`，对应本工作区（`master`）。

## 概述

交互式 TUI 里，用户在回合（turn）进行中提交一条消息，**它不会在本回合内生效**——它被排进
请求信道，等到**整个回合结束**（所有轮 + 所有工具跑完）才被取出并另起一个回合。

用户原话：「ys-coding-agent 在工作过程中，我输入并没有得到及时回复。」

**核心结论**：缺的不是性能，是**一条消息投递通道的第三格**。`Boundary` 枚举只有 `Steer` / `Abort`
两格，其中 `Steer` 在生产侧**没有生产者**；「排到下一回合」这一路（pi 的 `nextTurn`）**连格子都没有**。
这不是当前实现的疏漏，而是一次**可追溯的架构决策**造成的——ADR-0013 在产品接线器写出来之前
删掉了旧 `Inbox` 的 `follow_up` 路。

## 现有架构

### 模块边界

投递相关的三条通道（UI → app）：

```
crossterm 事件
   │
   ▼
crates/ys-tui-coding/src/events.rs:307   submit() ──► dispatch()
   │                                          │
   │                            ① Request 信道 │ ② Boundary 信道
   │                                          │
   ▼                                          ▼
apps/coding-agent/src/app_loop.rs:329   外层循环：request_rx.recv().await
   │                                     │
   │                          Request::Prompt ──► run_turn_once()  (:947)
   │                                                          │
   │                          ③ 回合内 select! (:1021) ◄──────┘
   │                             只监听 sink_rx（事件）+ boundary_rx（Steer/Abort）
   ▼
crates/ys-loop/src/basic.rs   BasicLoop::run_turn ──► 每轮 boundary.take() (:305)
```

- **请求信道**：`Request`（`crates/ys-protocol/src/request.rs`）——`Prompt` / `SetModel` / `SetThinking` /
  `SetEffort` / `Login` / `Logout` / `NewSession` / `Compact` / `Export` / `ListSessions` / `ResumeSession`。
- **边界信道**：`Boundary`（`crates/ys-protocol/src/boundary.rs:12-16`）——**只有两格**：
  `Steer(Message)` / `Abort`。
- **事件信道**：`AgentEvent` → `Envelope` → `Outbound::Event`（单向，app → UI）。

### 核心抽象

| 抽象 | 位置 | 现状 |
|---|---|---|
| `Request::Prompt(Message)` | `crates/ys-protocol/src/request.rs` | **一个 Prompt = 一个回合**（`app_loop.rs:387`「恰好跑一个回合」） |
| `Boundary` | `crates/ys-protocol/src/boundary.rs:12-16` | 2 格：`Steer` / `Abort`；**无 `followUp` / `nextTurn`** |
| `BoundarySource` / `QueueBoundarySource` | `crates/ys-protocol/src/boundary.rs:118` | 单队列（`VecDeque`）；`take()` 取一条、`is_aborted()` 看标志位 |
| `QueueBoundarySource::push` | `crates/ys-protocol/src/boundary.rs` | `Abort` 入队即置 `is_aborted` 标志位，**永久生效**（ADR-0013） |
| `BasicLoop` 轮边界拉取 | `crates/ys-loop/src/basic.rs:305-316` | `while let Some(bnd) = boundary.take()` → `Steer` 追加进 session + emit `UserMessage` |
| `app_loop` 回合内 `select!` | `apps/coding-agent/src/app_loop.rs:1021-1086` | **只监听 `sink_rx` + `boundary_rx`，不监听 `request_rx`** |
| `App::begin_local_turn` | `crates/ys-tui-coding/src/app.rs:394-412` | 回合中提交 ⇒ 登记 `pending_echo_texts`（迟到回声认领）；**不**改变投递语义 |
| `App::pending_echo_texts` | `crates/ys-tui-coding/src/app.rs:117` | 专治「排队提交的回声被误标 System」 |

### 关键数据流

**当前（回合中输入被排队）**：

```
用户回合中敲 "B" + Enter
   │
   ▼
events.rs:307 submit() → dispatch() → Action::Request(Request::Prompt)
   │                                    （app.rs:395 注释：「排到后续回合」）
   ▼
request 信道（容量 64，main.rs:503）
   │
   │  ← 阻塞在此：app 线程正在 run_turn_once 的 select! 里，
   │     该 select 不监听 request_rx ⇒ 没人取
   ▼
… 整个回合跑完（所有轮 + 所有工具）…
   │
   ▼
app_loop.rs:342 取到 Request::Prompt("B") → 另起一个回合
```

**本该走的路（`Steer`，已存在但无生产者）**：

```
用户回合中敲 "B" + Enter
   │
   ▼
boundary 信道 → app_loop.rs:1039 → boundary.push(Steer)
   │
   ▼
basic.rs:305 每轮模型调用**前** take() → session.append + emit UserMessage
   ⇒ 下一轮模型调用即可见，不等整个回合结束
```

### 外部依赖

无（纯内部协议问题）。竞品对照取自本仓自己的调研笔记 `docs/arch/hooks-and-plugins/notes.md:60`。

## 约束

### 技术

- **`Boundary` 枚举的 `Abort` 标志位永久生效**（ADR-0013）：`Abort` 入队即置 `is_aborted`，**永久**。
  `Steer` 无此语义，但若复用 `QueueBoundarySource`，**不能**顺带置该标志位。
- **每回合必须新建 `BoundarySource`**（`app_loop.rs:998`）：`Abort` 的标志位随回合作废。
  ⇒ 待生效的 `Steer` 若在回合边界被丢弃、下一回合又新建边界源，steering 会丢。
- **`run_turn_once` 头部会排空残留边界消息**（`app_loop.rs:951-952`）：`while ctx.boundary_rx.try_recv().is_ok() {}`
  —— 接通 steering 时必须确认它**不会把待生效的 Steer 误排空**。
- **UI 线程必须无 runtime 上下文**（`AGENTS.md` 硬性约束）：TUI 用 `blocking_send`，
  在 async 上下文会 panic ⇒ steering 的发送端仍是同步路径。
- **`request_rx` 容量 64**（`main.rs:503`），`boundary_rx` 容量见装配期。

### 性能

- **流式已经是 `stream: true`**（`adapters/model-openai-compatible/src/lib.rs:1297`），
  文本 delta 经 `Forwarder` → `AgentEvent::ModelTextDelta` → TUI 实时上屏。
  ⇒ **首 token 之后的输出没有延迟问题**；问题在「用户输入的投递时机」与「首 token 之前的等待」。
- **中止感知上界 100ms**（`adapters/model-openai-compatible/src/lib.rs:24` `ABORT_POLL_INTERVAL`），
  bash 工具同量级（`adapters/tools-basic/src/bash.rs:13`）⇒ 取消是及时的，
  **唯独「投递新消息」这条路断了**。
- **默认 `reasoning_effort = high`**（`apps/coding-agent/src/config.rs:11`）：直接决定首 token 时间，
  是与投递问题**正交**的第二个杠杆（`YUSHAN_REASONING_EFFORT=low` 零改动可试）。

### 演进

- **ADR-0013（accepted）删掉了 `follow_up` 路**：见下节「历史沿革」。
- **ADR-0014（accepted，2026-09-15）** 定义了 Follow-up，**但代码零落地**（`docs/TODO.md` §B）。
- **`docs/arch/hooks-and-plugins/notes.md:81` 的准入规则**：「新控制流需求 → 判断链 →
  **落既有桶则复用** ⇒ follow-up 就是既有的桶，应复用而非新造通路。」

### 组织

单仓单人维护；`docs/design.md` 不变量 + ADR 是硬性约束（`AGENTS.md`）。

## 根因（历史沿革）

### ADR-0013 删掉了 `follow_up` 路

`docs/adr/0013-cancel-token-removal-boundary-source.md:4`：

> 并把「轮边界的控制输入」从 `ys-channel::Inbox`（**two-vec** + `Intent` + `QueueMode`）收敛成一个

`docs/adr/0013-cancel-token-removal-boundary-source.md:37`：

> `Inbox`（`ys-channel`，`Arc<Mutex<Inner{steering, follow_up}>>`）| `Message` 级路由（新输入 vs 插话）
> **已由 `Request::Prompt` / `Boundary::Steer` 承担**；two-vec + `Intent` + `QueueMode` 是
> **同一语义的第二套实现**

旧 `Inbox` 的 two-vec 是 **`steering` + `follow_up`**。ADR-0013 判定「新输入 vs 插话」已由
`Request::Prompt` / `Boundary::Steer` 承担，于是把 two-vec 压成单枚举 `Boundary`。

**但 `Boundary` 只有 `Steer` / `Abort` 两格**（`crates/ys-protocol/src/boundary.rs:12-16`）——
`follow_up` / `nextTurn` **没有对应变体**，被当作「重复语义」一并删掉。

同一 ADR `:82` 判定 `Inbox` 的「**队列 = 会话 = 历史 + pending 转移**」模型在 app 侧循环里
**没有任何消费者**——那个消费者**本来就不存在**：ADR-0011（2026-09-12）建 `Inbox` 时，
coding-agent 的 TUI 还没写。

### 时间线

| 日期 | 事件 | 后果 |
|---|---|---|
| 2026-09-12 | ADR-0011 建 `Inbox`（two-vec：`steering` + `follow_up`） | ✅ 有中途投递 |
| 2026-09-15 | ADR-0014 设计 Follow-up（「追问 ≠ 插话」） | 判定完成，**代码零落地** |
| ~2026-09 | ADR-0013 用单枚举取代 two-vec | ❌ `follow_up` 路被删 |
| 之后 | coding-agent TUI 落地 | 需要该语义时**没有协议可发** |

⇒ **「先有协议、后有产品」的代价**：核心库为了干净做的正确决策，落在产品接线器写出来之前；
等产品需要这条语义时，协议格已经被收敛掉了。

### 竞品对照

`docs/arch/hooks-and-plugins/notes.md:60` 记录 pi 的做法：

> 但 pi 没有把 abort/cancel/steer 做进 hook —— 它们另有消息队列
> （**`deliverAs: "steer" | "followUp" | "nextTurn"`**）与 operation 状态机
> （`OperationStatus = "running" | "open" | "aborting"`）

**三路齐全**：中途插话 / 回合后追问 / 排到下一回合。Claude Code 与 Codex 同类——
它们的「用户输入投递」是**独立于回合生命周期的投递层**，不是回合的一个变体。

YuShan 把「投递」塞进了回合语义内部（`Request::Prompt` = 一个回合），
⇒ **投递时机只能等回合边界**。

## 需求范围

### 范围内

- **让回合中的用户输入有明确、及时的生效路径**（至少「下一轮」级，而非「整个回合后」级）。
- **投递时机对用户可见**（区分「立即生效」「已排队，下一回合」「已排队，回合结束后」）。
- **明确三种投递语义的边界**：`Steer`（轮内插话）/ `nextTurn`（下一回合）/ `followUp`（系统发起）。

### 范围外（明确不做的）

- **不新造第四种投递语义**——准入规则要求「落既有桶则复用」
  （`docs/arch/hooks-and-plugins/notes.md:81`）；follow-up 的桶已存在（ADR-0014）。
- **不改 `Boundary` 的 `Abort` 语义**（永久置位、每回合新建源）——ADR-0013 硬约束。
- **不动 `ys-loop` 的轮边界拉取点**（`basic.rs:305`）——拉取点已就位，缺的是生产者与第三格。
- **不做 hook 系统**（ADR-0021 / ADR-0014 同）——本需求是投递，不是扩展点。

### 关键场景

| 场景 | 期望 | 现状 |
|---|---|---|
| S1 空闲时提交 | 立即起一个回合 | ✅ 已如此 |
| S2 回合中提交，等下一轮生效 | 下一轮模型调用即可见 | ❌ 等整个回合结束 |
| S3 回合中提交，排到下一回合 | 立即被接收并**可见**，回合结束后自动执行 | ❌ 无此语义 |
| S4 回合中提交，用户想知道状态 | 状态行显示投递去向 | ❌ 无（且 `docs/arch/tui-resident-status/` 在待办中） |
| S5 回合中取消 | 及时收场 | ✅ 已实现（100ms 轮询） |
| S6 回合中插话（中途改方向） | 下一轮生效 | ❌ 通路存在但无生产者 |

## 未澄清问题（**已解，见下**）

> **2026-10-22：Q1–Q5 已由 `docs/adr/0047-input-delivery-target-and-queue.md` 裁决。**
> 结论摘要：默认「下一轮生效」（`Steer`）；`Boundary` **不动**；
> 投递去向做成 `DeliveryTarget`（`ys-protocol` 新文件）；
> 队列住在 **app 侧**（`app_loop`），跨回合存活 ⇒ 补上 ADR-0014 follow-up 缺的协议格；
> UI 不传目标（app 侧是权威）；**不新增回执事件**。
> 仍**未裁决**的两项已登记进 `docs/arch/input-delivery-latency/design.md` §7：
> 「命令与消息谁先」与「回合中 `/login` 是否即时拒绝」。

- [x] **S2 与 S3 是同一件事还是两件事？** ⇒ **两件事**（决策 1）。
      默认「下一轮生效」；「下一回合」留给系统发起的 follow-up，
      **不给用户**显式选择（模型这轮调不调工具不可预知 ⇒ 会产生撒谎的承诺）。
- [x] **投递可见性归 UI 还是 app？** ⇒ **UI 本地维护**（决策 7）。
      `Delivered` 的唯一信号是**已存在**的 `AgentEvent::UserMessage` ⇒ 不新增回执协议。
- [x] **回合中提交时，是否仍应本地回显？** ⇒ **仍回显**（`app.rs:402` 不变）。
      本地行先显示「已提交」，收到 `UserMessage` 后标为「已生效」。
- [x] **`/命令` 在回合中提交怎么办？** ⇒ **排队**，回合结束后按序执行（决策 5 末段）。
      ⚠️ 「命令与消息谁先」仍未裁决（设计 §7）——
      本设计的取舍是「先命令后消息」（命令是控制面）。
- [x] **ADR-0014 的 Follow-up（系统发起）是否与本需求合并？** ⇒ **合并**（决策 8）。
      用户输入与系统 follow-up 共用同一条 app 侧队列，用 `origin` 区分。

## 后续建议

- **先解 Q1**（S2 vs S3）：它决定改动面是「接上已有的 `Steer` 生产者」（小）
  还是「给 `Boundary` 加第三格 + 新增 `nextTurn` 投递路径」（中）。
- **优先复用既有桶**：`docs/arch/hooks-and-plugins/notes.md:81` 的准入规则要求
  「落既有桶则复用」⇒ 评估 ADR-0014（`docs/design-plans/2026-09-15-follow-up.md`）的落地，
  而不是新造通路。
- **同步修 `docs/TODO.md` §B**：Follow-up 条目当前只记「代码零落地」，
  应补上「回合中输入无中途投递通路」这一根因指针。
- 建议用 `arch-design` 做方案设计；`docs/design.md` 与 `docs/architecture.md`
  的「回合 vs 轮 vs 追问」表需同步更新。
