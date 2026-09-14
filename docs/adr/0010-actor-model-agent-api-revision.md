# 0010 — Actor 模型：会话与配置归接线器，Agent 不再是"被伸手"的对象

Agent 从"被调用的库"变为"自己转的循环"。其直接后果是：**外部再也拿不到 `&mut Agent`**，因此"从外部伸手进 agent 改状态"这一整类接口失去存在基础。

本 ADR 记录**这一后果及其必然推导**，并**修订 ADR-0007 / 0008 中依赖"外部可伸手"的部分**。具体的 API 形状（新接口如何设计）留待设计阶段，不在本 ADR 决定。

## Status

proposed（2026-09-12）——待审

## 背景

`docs/arch/gap-closure/context.md` 本轮定下：

- agent **自己转**（取消息 → 跑 → 再取），不是被 `run_turn()` 一次性驱动
- **队列 = 会话 = 历史**（一条消息日志 + 一个"处理到哪了"的游标）
- **`/new` 与 agent 无关**——换队列，agent 全程不知情
- **配置由 agent 自取**，不进消息
- **生命周期是策略**（`StopWhenConsumerGone` / `ContinueWithoutConsumer`）

对照现状（`crates/agent-runtime/src/agent.rs`），今天有 8 个公开方法依赖"外部持有 `&mut Agent`"：

```
run_turn / set_model / clear_session / session_messages
tool_names / context_window / cancel / cancel_handle
```

其中 **ADR-0007**（`tool_names`/`context_window`/`cancel`）与 **ADR-0008**（`cancel_handle`）明确是"为外部访问 agent"而设。agent 一旦自己转，这批接口的调用点**在结构上无法成立**。

## 决策（仅记录已定部分）

**1. 会话与配置的所有权移出 agent，归接线器。** agent 不再持有"当前是哪个会话"或"当前用哪个模型"——它只执行、只产出事件。

**2. `Agent` 变为无状态执行能力封装。** 它持有的是*执行所需*的东西（工具、取消句柄、限制），不持有*会话态*（历史、当前模型选择）。

**3. 由此必然推导**（非新增决策，是 1+2 的结果）：

| 现状 | 必然结果 | 依据 |
|---|---|---|
| `/new` 调 `clear_session()` | 该调用点消失——`/new` 改为换队列 | 会话归接线器 |
| `/model` 调 `set_model()` | 该调用点消失——改为换 agent 或 agent 自取 | 配置归接线器 |
| 状态面板问 `session_messages()` | 该调用点失效——改为从事件流取 | 会话归接线器 |
| `run_turn()` 一把梭 | 与"自己转"矛盾——需新的驱动形态 | 自转循环 |

**4. ADR-0007 / 0008 的处置**：

| ADR | 条目 | 处置 |
|---|---|---|
| 0007 | `tool_names()` / `context_window()` | **保留**——只读查询，不依赖 `&mut`，且消费者（UI/事件）仍需要 |
| 0007 | `cancel(&mut self)` | **保留**——被动式强制打断，仍有调用场景 |
| 0008 | `cancel_handle()` | **保留**——外部从非 `&mut` 上下文触发取消，**agent 自转后更需要** |

**结论：ADR-0007 / 0008 本体不废弃**——它们描述的接口在新模型下依然有效且更有必要（外部唯一能与 agent 交互的通道就剩取消和只读查询了）。被架空的是**其余"伸手改状态"的方法**。

## 本 ADR 不决定的事

以下均**未定**，属设计阶段：

- 自转循环的新驱动接口叫什么、长什么样（`turn_stream`？事件流？）
- 模型自取的具体机制（trait？闭包？接线器直接持有？）
- 被架空的方法（`set_model` / `clear_session` / `session_messages`）是删除还是改造
- `AgentBuilder` 的字段如何随之调整

**这些应由 `arch-design` 出方案后另立 ADR**，本 ADR 只锁定"所有权归属"这一层。

## Considered Options

- **不修订，保留全部 API**：agent 自转后外部拿不到 `&mut Agent`，`set_model`/`clear_session` 等调用点**编译不过**。不可行。
- **保留 API，内部改为写"控制消息"**：给 agent 增加一套控制指令协议。**否决**——与已定的「入站队列只装自然语言」直接矛盾。
- **所有权外置（采纳）**：会话/配置归接线器，agent 只做无状态执行。符合 actor 语义，且与"队列 = 会话 = 历史"自洽。

## Consequences

- **接线器责任变重**：会话管理、模型配置、事件消费均上移。这是 actor 模型的固有代价。
- **两个产品形态共用**：交互式（`StopWhenConsumerGone`）与后台长任务（`ContinueWithoutConsumer`）共用同一套所有权结构，只是生命周期策略不同。
- **`AgentBuilder` 仍存在**（启动时静态组合），但其字段需随之调整——**具体形态待设计**。
- **Q13 部分关闭**：本 ADR 回答"所有权归谁"；"具体 API 形状"仍开放。

## 附注（2026-09-14）：actor 循环上移到 app 侧

**本 ADR 的结论不变**——`Agent` 仍是「只持执行能力、不持会话」的无状态执行器。**换的只是 actor
循环的承载者**：

- `Agent::run(inbox)` 与 `RunSummary` **已删除**。多回合 / followUp 的驱动语义上移到
  `apps/coding-agent/src/app_loop.rs`（`app_loop::run` 持 `Wiring` + `Agent`，逐条 `recv` `Request`、
  调 `run_turn`、转发事件）。
- 原因：路线 B（拆 crate + 分线程 + 三信道，见 `docs/design-final/coding-agent-tui.md`）让
  **回合边界在 app 侧**（`Request::Prompt` 到达即跑一回合），`Inbox` 连同其 two-vec / `Intent` /
  `QueueMode` 一并删除（见 ADR-0013）。
- `-p` / `--json` 不再经 `Agent::run`：直接 `run_turn` + `begin_turn(1)`，一次运行恒为 turn 1。
- 因此「Agent 自己转」这一措辞在本 ADR 的语境下收窄为「**app 侧循环驱动 `Agent` 逐回合执行**」；
  `Agent` 自身的无状态性（不持 session / events / model，经 `AgentPorts` 传入）**一字未改**。
