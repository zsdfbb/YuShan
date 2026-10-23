# 后台命令完成通知（bg-notify）— 架构上下文

> 产出：`/arch-explore`。用途：为「**取消 `bash_poll` 轮询，改成后台命令完成时自动通知**」
> 界定现状、约束与范围。证据均带 `文件:行号`，对应工作区 `master` @ `86f131c`。

## 概述

把后台命令的结果回收方式从**模型显式轮询**（`bash_poll`）改成**完成时自动注入对话**
（`Boundary::Deferred`）。目标：消灭「模型反复问『好了吗』」造成的往返浪费。

**触发本需求的实测数据**：会话 `1792727582_325572909`（本机 `~/.yushan/sessions/`）：

| 指标 | 数值 |
|---|---|
| `bash_bg` 启动 | 16 次 |
| `bash_poll` 调用 | **68 次** |
| 其中返回 `running`（零新信息） | **54 次（79%）** |
| `wait_seconds = 20` 占 | 60 / 68 |
| 模型声明的等待累计 | **1243 s ≈ 20.7 min** |
| 单句柄 `bg1` 被 poll | **24 次** |

⇒ 54 个回合花在「问一句好了吗，答『还没』」上。

## 现有架构

### 模块边界

```
crates/ys-protocol         Boundary / BoundarySource / QueueBoundarySource（零 tokio）
crates/ys-tool             Tool / ToolContext（含 boundary、subagent 窄 trait）
crates/ys-loop             BasicLoop::drain_boundary —— 轮边界消费 Steer/Deferred/Abort
adapters/tools-basic       bash.rs（键）、background.rs（注册表）、bg_tools.rs（bash_bg/bash_poll）
apps/coding-agent          assembly.rs（装配工具表）、app_loop.rs（每回合造 runner）、subagent.rs
```

### 核心抽象

| 抽象 | 位置 | 现状 |
|---|---|---|
| `Boundary::Deferred { handle, message }` | `crates/ys-protocol/src/boundary.rs:23` | **已存在**：后台结果的注入载体 |
| `drain_boundary` | `crates/ys-loop/src/basic.rs:74-112` | **已存在**：`Deferred` → append 会话 + emit `DeferredResult` |
| `AgentEvent::DeferredResult` | `crates/ys-event/src/event.rs:35` | **已存在** |
| UI 渲染分支 | `crates/ys-tui-coding/src/app.rs:380` | **已存在**：`↓ {handle} finished: {首行}` |
| `SubagentRunner::spawn` | `crates/ys-tool/src/subagent.rs:70` | **先例**：注释原话「**不需要 poll**」 |
| `result_sink: Option<Arc<QueueBoundarySource>>` | `apps/coding-agent/src/subagent.rs:256` | **先例**：每回合由 `app_loop.rs:1150` 注入同一份边界源 |
| `BackgroundTasks` | `adapters/tools-basic/src/background.rs:321` | 进程注册表：`spawn` / `sweep` / `snapshot` |
| `BashBgTool` / `BashPollTool` | `adapters/tools-basic/src/bg_tools.rs:69,93` | 装配期构造一次（`assembly.rs:156-165`） |
| `ToolContext.boundary` | `crates/ys-tool/src/context.rs:13` | **每回合**的真实边界源（借用） |

### 关键数据流

**现状（轮询）**
```
模型 → bash_bg → 句柄 bg7
模型 → bash_poll(bg7) → running（无新信息）… ×N
模型 → bash_poll(bg7) → finished + 增量输出
```

**目标（通知）**
```
模型 → bash_bg → 句柄 bg7
       后台 task 在进程退出时 push Boundary::Deferred { handle: "bg7", message }
模型 → （继续干别的）
       BasicLoop 在下一个轮边界 drain → append 会话 + emit DeferredResult
```

### 外部依赖

无新增。注入通道全在 `ys-protocol`，零 tokio。

## 约束

- **技术**：
  - `ys-protocol` / `ys-core` / `ys-event` **零 tokio** —— 注入只用 `QueueBoundarySource::push`。
  - **工具表进程级、`BoundarySource` 每回合新建** ⇒ 后台 task 不能持有构造期的边界源
    （那是**上一回合**的，结果会注入已死的回合）。必须学 `SubagentRunner`：**每回合造一份 runner**。
  - `BackgroundTasks` 在 `adapters/tools-basic`，**不得**依赖 `apps/coding-agent`（依赖方向）。
  - `ys-tool` 可依赖 `ys-protocol`（`ToolContext` 已用）⇒ 窄 trait 放 `ys-tool` 可行。
- **性能**：注入本身零成本（一次 `push`）。共享 sweeper 100ms tick 已存在（`app_loop.rs:113`），
  检测进程退出**复用**它，不新增后台任务。
- **演进**：`bash_poll` 是我们自己加的工具、无外部依赖 ⇒ **可直接删**。
  删后 `is_concurrency_safe` 与 guideline 的既有矛盾（见下）自动消失。
- **组织**：用户已明确要求「不要 poll，直接通知注入」，并选择本仓既有的 `Deferred` 形态。

### 既有矛盾（本次顺带消除）

`bg_tools.rs:253` 把 `bash_poll` 判为 `is_concurrency_safe() == false`（poll 推进交付偏移），
而 `prompt.rs` 的 guideline 却要求「collect it with `bash_poll` **in the same batch**」——
一批里只要有 `bash_poll` 整批退化串行。**删掉 poll 即消除。**

## 需求范围

### 范围内

- 后台命令**完成时**把结果作为 `Boundary::Deferred` 注入会话，模型无需轮询。
- 删除 `bash_poll`；`bash_bg` 保留（显式请求后台）。
- `bash` 超时**不再杀进程**，改为转后台 + 完成时通知（M3）。
- 注入消息含：句柄、命令摘要、退码、耗时、输出摘要 + **完整输出路径**。
- 更新 `prompt.rs` guideline、`design.md` §4.6/§4.7、as-built 文档。

### 范围外

- **不做**「回合结束后继续等结果」（subagent 先例的既定语义：回合末不等）。
  文案必须诚实说明「若本回合先结束，结果不保证送达」。
- 不改 `ys-loop` 的轮次模型（严格交替保持不变）。
- 不动 `ys-core` / `ys-event` / `ys-tui-coding`（`DeferredResult` 渲染已存在）。
- 不做通知的持久化 / 重放。

### 关键场景

1. 模型启动 `cargo build`（90s）→ 继续读代码 → 构建完成 → 下一轮看到结果。
2. 模型调 `bash(cargo test)`，超时 → 自动转后台 → 继续 → 完成时通知。
3. 后台命令失败（非零退码）→ 通知里标明失败，模型据此行动。

### 异常/边界场景

- 回合在命令完成前结束 ⇒ 结果无人消费（**有意**，文案须写明）。
- abort / `/new` / 进程退出 ⇒ 在跑的后台进程按既有清扫逻辑杀掉（`background.rs:630-700`）。
- 多条命令同时完成 ⇒ 一次 drain 取多条，保序。
- 输出超限 ⇒ 摘要 + 路径，不静默丢弃。
- 句柄表满 ⇒ `bash_bg` 同步报错（既有行为）。

## 未澄清问题

> 用户已就 Q1/Q2/Q5 表态（「按你的建议来」）；以下按建议采纳并记录。

- [x] **Q1 注入时机** ⇒ **(a) 完成即 push，下一个轮边界消费**。回合结束则不送达（诚实文案）。
- [x] **Q2 是否保留主动查询** ⇒ **删除 `bash_poll`**；后台化由 `bash` 超时自动移交补齐。
- [x] **Q3 消息内容** ⇒ 摘要（尾部 50 KiB 策略）+ 完整输出路径。
- [x] **Q4 阈值/守卫** ⇒ 沿用既有（并发 4、句柄 8、输出 2 MiB）。
- [x] **Q5 `bash_poll` 去留** ⇒ **一次删干净**。

## 后续建议

- 直接用 `arch-design` 落 `design.md` + ADR，然后实现（用户已选择快速路线）。
