# 跨回合推进 / Follow-up / Loop — 架构上下文

> 性质：探索与调查记录。只记**现状、约束、和已查明的结论**，不含方案选择。
> 日期：2026-11-06

## 概述

**系统能否在一个回合结束后、由自己（而非用户）发起下一个回合**——以及围绕这件事的三个
被混用过的词（跨回合推进 / Follow-up / Loop）各自到底指什么。

本文件先把三个词拆开，再对账仓库现状，最后给出已查明的结论与未澄清问题。

## 三个词的拆解

这三个词在本仓的讨论里被混用过，必须分开：

| 词 | 是什么 | 回答的问题 | 代码现状 |
|---|---|---|---|
| **跨回合推进** | **机制**：起下一个回合的通路 | 「消息放在哪、谁去取」 | 部分存在（见下） |
| **Follow-up（追问）** | **一种用途**：回合后系统自动做的事 | 「为什么还要再跑一次」 | `follow_up` grep = **0** |
| **Loop（定时循环）** | **一种用途**：按时间反复触发 | 「什么时候再跑」 | 无任何实现 |

三者是**机制与消费者**的关系：Follow-up 与 Loop 都是跨回合推进的消费者。

定义依据 `docs/CONTEXT.md:31`（追问定义）：「一个回合**结束后**、由**系统**（而非用户）
发起的**下一个回合**，携带一条自动生成的输入」。注意它描述的是**触发理由**，不含实现方式。

> **本轮结论先行**：`docs/TODO.md` §B 把「缺协议格」当作三者的共同根因，这个判断
> **只对 Follow-up 那一条成立**。Loop 与 G4 都有**不碰协议**的通路（见 §4）。

## 现有架构

### 模块边界

跨回合推进横跨三层，位置决定了改动成本：

| 层 | 位置 | 是否跨回合 |
|---|---|---|
| 协议 | `crates/ys-protocol/src/boundary.rs` | **否**——四格全是回合内 |
| 执行 | `crates/ys-loop/src/basic.rs` | 否——只跑单回合 |
| 接线 | `apps/coding-agent/src/app_loop.rs` | **是**——外层循环跑多回合 |

`app_loop` 拥有回合间的一切（ADR-0010 附注：actor 循环已上移）。⇒ **跨回合推进天然属于
`apps/coding-agent`，不属于 runtime 核心。**

### 核心抽象

**`Boundary`（`crates/ys-protocol/src/boundary.rs:12`）只有四格**：

| 变体 | 语义 | 生效时机 |
|---|---|---|
| `Steer(Message)` | 掌舵：运行期注入，追加进会话 | 下一轮模型调用前 |
| `Deferred { handle, message }` | 后台任务结果到达 | 下一轮模型调用前 |
| `Compact` | 手动压缩 | 下一个轮边界 |
| `Abort` | 中止 | 立即标记 |

**四格全是「回合内」的**——没有一格表达「排到**下一个回合**」。

**`PendingResults`（`apps/coding-agent/src/subagent.rs:268`）** 是**进程级投递表 + `Notify` 唤醒**：

- `deliver`：结果落地；未绑定时静默留表
- `notify_one`（存 permit，不用 `notify_waiters`）——理由见该字段注释：消费端是事件驱动的，
  `notify_waiters` 不存 permit 会让间隙里的结果唤醒落空
- `has_pending` / `take_pending`：只读 / 全量取走

### 关键数据流（as-built）

`app_loop` 的空闲等待（`apps/coding-agent/src/app_loop.rs:397`）已经是 **4 条 `select!` 分支**，
其中**两条已经在做「系统自动起回合」**：

```rust
tokio::select! {
    req = request_rx.recv()                                  => Wake::Request(req),
    Some(cache) = optional_wake(models_rx...)                => Wake::Models(cache),
    Some(()) = optional_wake(pending)                        => Wake::SubagentReturned,   // :402
    Some(()) = optional_wake(background)                     => Wake::BackgroundReturned, // :403
}
```

`optional_wake`（`:148`）把「可选的唤醒源」变成一条 `select!` 分支：源为 `None` 时该分支
被 tokio **禁用**，不忙转。

两条自动起回合的臂形状完全一致（`:413` / `:468`）：

```rust
Wake::SubagentReturned => {
    if !has_pending { continue; }                  // 复核（permit 可跨回合残留）
    let message = subagent_placeholder();          // Role::Harness 占位消息（:1429）
    run_turn_chain(..., message, None).await?;      // 起一整回合
}
```

**这就是「跨回合推进」的现成通路**——只是触发源只有两个：子 agent 结果、后台命令完成。

补投落点：`run_turn_chain` 开头（`:1575`）把表里残留的结果转成 `Boundary::Deferred` 推入
本回合边界源。

### 外部依赖

无。本主题不涉及外部服务。

## 约束

- **技术**：`Boundary` 属于 `ys-protocol`，**零 tokio**（AGENTS.md 硬性约束）⇒ 队列若进协议，
  实现必须留在 app 侧。
- **技术**：`ys-protocol` / `ys-core` / `ys-event` 不含 tokio，直接与间接皆无。
- **演进**：`-p` / `--json` 是「跑一个回合就退出」⇒ 任何依赖常驻进程的机制都只对 TUI 生效。
- **演进**：`toolset.rs:648-661` 有前缀指纹锚断言——**工具表的变化会被测试抓到**，新增工具
  不是零成本。
- **组织**：无外部截止期；本主题无人在等。

## 调查结论

### 1. `follow_up` 代码零命中（设计已于 2026-11-06 修订）

`grep -rn "follow_up\|followup\|FollowUp" --include=*.rs apps/ crates/` = **0**。
`DeliveryTarget` / `Delivery`（ADR-0047 的协议格）同样 **0**。

设计存在且已修订：`docs/design-plans/2026-09-15-follow-up.md`（详情见同目录 `follow-up.md`）。
ADR-0014（accepted 2026-09-15）。

⚠️ 该设计曾被**实现过又删掉**：`apps/coding-agent/src/follow_up.rs`（ADR-0027 决策 2）→
**ADR-0041（2026-10-17）整体删除**。现设计已按 as-built 重写，不再复用该文件名。

### 2. 那两条自动起回合**不等于**协议格

`docs/TODO.md` §B 的表述仍然成立：「`PendingResults` 是**进程级投递表 + `Notify` 唤醒**，
不是 `Boundary` 变体」。

> ⚠️ **但其中一句已过期**：`apps/coding-agent/src/subagent.rs:273` 的注释仍写
> 「`take_pending` 唯一的入口是 `Request::Prompt`」。自 ADR-0065 起，
> `Wake::SubagentReturned`（`app_loop.rs:413`）也经 `run_turn_chain` → `take_pending_results`
> （`app_loop.rs:1575`）取走结果 ⇒ **该注释需要修**，但它不影响「不是协议变体」这个结论。

### 3. 三条路线可以不走协议格

关键观察：`select!` 每次进入外层 `loop`（`:389`）都**重建**，所以「到点唤醒」可以写成
一条纯计算的 `optional_wake` 分支——与已有两条同构。

| 路线 | 触发源 | 是否需 `Boundary` 新格 | 成本 |
|---|---|---|---|
| **A. 协议格**（ADR-0047） | 任意 | **是** | 高：G1 / G3 / G5 / G6 四道闸门未做 |
| **A′. design plan 那条** | 回合结束 + HEAD 变化 | **否** | 低：签名改 `TurnOutcome` + 一个新模块 + 一个 config 字段（design plan §3.6 / §3.4）|
| **B. app 层唤醒源** | 进程内（时钟 / 定时） | **否** | 低：加一条 `optional_wake` + 一个 `Wake` 变体 |
| **C. 外部桥接**（G4） | 进程外 | **否**（桥接自己起进程） | 低（见 §4） |

A′ 与 B 的差别不在效果，在**触发时机**：A′ 只能「用户刚说完话之后」触发；
B 能「回合外任意时刻」触发。**Follow-up 用 A′，Loop 用 B。**

A（协议格）与它们的差别在**投递语义**：A 能表达「回合**中**提交、排到下一回合」，
A′ / B 都不能。**三条真实路线（Follow-up / Loop / G4）没有一条需要 A。**

### 4. G4（飞书）不走本仓，且不需要公网

`/Users/zs/Develop/lark-agent-bridge`（TypeScript，生产可用）已实现整条链路：

- 事件接入：`@larksuite/channel` 的 **WebSocket 长连接**（`src/bot/channel.ts:257` 的 `wsConfig`）
  ⇒ **不需要公网地址、不开端口、无 HTTP 回调**
- agent 接入方式：`spawn` CLI 子进程 + 解析 stream-json（`src/agent/claude/adapter.ts:73`）
- 已有 adapter：`'claude' | 'codex'`（`src/agent/capability.ts:5`）

**⇒ G4 的「入站无落点」判断已过期。** 桥接侧轮到自己起回合，YuShan 只是被 spawn 的子进程。

接入缺口（若要做）只有两项：
| 缺口 | 桥接侧期望 | YuShan 现状 |
|---|---|---|
| 注入额外系统提示 | `--append-system-prompt-file` | ❌ 无此参数 |
| 事件格式 | `AgentEvent`（`src/agent/types.ts:6`） | `--json` 出的是 `Envelope`（`main.rs:573`），格式不同 |

⚠️ `--append-system-prompt` 与 `prompt.rs:35` 的既有短路语义**冲突**（命中 `.yushan/SYSTEM.md`
时整体逐字返回）——这是**语义决策**，不是加个参数。

**用户已明确：本项目暂时不考虑处理 webhook。** 本项记为范围外。

### 5. Loop 的两个模式（外部参考）

Claude Code 的 `/loop`（从 `bin/claude.exe` 的字符串提取，**仅作形态参考**）有两个模式：

| 模式 | 触发 | 关键字段 |
|---|---|---|
| 固定间隔 | cron / `5m` `2h` `1d`，最小 1 分钟 | 落 `scheduled_tasks.json`（**默认仅内存**） |
| 动态节律 | 模型自己定下次延迟 | `ScheduleWakeup{delaySeconds, reason, stop}`，延迟钳制 `[1min, 1h]` |

两个值得注意的设计点：

- **模型可自定节奏**：每轮结束时选下次延迟并打印理由（构建快完则短、PR 安静则长）。
  终止靠 `stop: true`；若某轮既不定时也不停 ⇒ 兜底约 20 分钟后再跑一轮。
- **self-paced `/loop` 不被 `--resume` 恢复**（cron 驱动的一般可恢复）⇒ 恢复后需重新 `/loop`。

⚠️ 这两点说明：**调度结构不难，难的是「tick 跑在哪个 session、跨 tick 记忆放哪」。**

> 详细的四家对照（含 CodeWhale / Codex）见同目录 **`research.md`**。

## 需求范围

### 范围内

- 把「跨回合推进 / Follow-up / Loop」三个词的边界固定下来（本文件 §2）
- 记录 A / B / C 三条路线的成本对比（§4）
- 记录 Loop 的调度骨架可行形态（`select!` 第三条 `optional_wake` + 按时间排序的队列）
- 修正 `docs/goals.md` 与 `docs/TODO.md` 中已过期的判断

### 范围外（明确不做的）

- **webhook / 飞书入站**——用户已明确暂不考虑；归 `lark-agent-bridge`
- **`Boundary` 新格（ADR-0047 的 G1 / G3 / G5 / G6）**——Loop 不需要，暂不碰
- **MCP / `ToolPlugin` 接入缝**——与跨回合推进无关
- **进 runtime 核心**——跨回合推进属 `apps/coding-agent`，不动 `ys-loop` / `ys-protocol`
- **落盘持久化调度**——跨进程重启的语义没有好答案，第一版不做

### 关键场景

- **场景 1（Follow-up）**：git 提交后自动起一轮同步文档。触发=回合结束 + HEAD 变。
  现状：设计有（ADR-0014），代码零。
- **场景 2（Loop）**：TUI 挂着，每 5 分钟检查一次 GitHub 新 issue，有活就干。
  触发=时间到。现状：无。
- **场景 3（批量）**：夜间连续处理 3 个 issue。**不需要本机制**——shell `for` 循环开独立进程
  即可，上下文隔离反而更好。
- **场景 4（G4）**：飞书收到消息 → 起回合。**归桥接**，不需要本机制。

### 边界场景

- **执行循环占住导致定时漂移**：`select!` 只在**回合之间**转；回合进行中跑的是
  `run_turn_chain` 内部另一套 `select!`（`:1022`），**不监听调度源** ⇒ 到点必须等当前回合结束。
- **积压**：一次只能跑一个回合（一条历史）⇒ 三个任务同时到点也只能串行。
- **可见性**：定时行为若完全隐身在后台，用户不知道它在等什么、还有几条。

## 未澄清问题

- [ ] **Loop 的 tick 跑在哪个 session？**（待用户确认）
      主 session ⇒ 记得昨天干了什么，但历史里混进大量 tick 占位消息；独立 session ⇒ 干净但失忆。
      **这是本主题最难的一环，比调度结构重要。**
- [ ] **队列按时间排序用堆还是有序数组？**（待用户确认）
      规模十几到几十条，二者都够；这是实现偏好，不是设计问题。
- [ ] **多任务同时到点时串行是否可接受？**（待用户确认）
- [ ] **是否要做 `noop` 语义 / 动态延迟？**（待用户确认）
      先做固定间隔够用，还是直接照 `noop` + `[60,3600]` 做。
- [ ] **是否修正 `docs/goals.md:20/:23/:41` 与 `docs/TODO.md` §B 的过期判断？**（待用户确认）
      `:23` 那句「G3 / G4 同一个根因」与 §4 结论冲突；`:20` 的 G4 缺口语义已过期。

## 后续建议

- 若要推进 **Loop**：用 `arch-design` 做方案设计，重点是 session 归属取哪个、以及
  「可见性」怎么表达（用户得能看见还有几条、下一条几点）。
- **Follow-up**：设计已修订（`docs/design-plans/2026-09-15-follow-up.md`），
  状态与过期断言清单见同目录 **`follow-up.md`**。它走的是 A′（低），**不是** ADR-0047 那条重路径。
- 若要推进 **G4**：不在本仓，改 `lark-agent-bridge`；先决条件是
  `--append-system-prompt-file` 的语义决策（与 `prompt.rs:35` 短路冲突）。
- **不建议**先做 ADR-0047 的 G1/G3/G5/G6——三条真实路线（Loop / G4 / G3 批处理）
  没有一条依赖它。
