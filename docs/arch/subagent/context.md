# Subagent 派发 — 架构上下文

> 产出：`/arch-explore`（2026-09-23）。用途：为「**我需要 subagent 的能力，至少能够区分主 agent 和 subagent，主 agent 可以派发 subagent 完成一些任务**」界定现状、约束与范围，供后续 `arch-design` 使用。
> 证据均带 `文件:行号`，对应本工作区 `master`（`02a3b55`）。参照实现 `tmp/pi`、`tmp/Codewhale` **未入版本库**（`tmp/` 已被 `.gitignore`）。
>
> ⚠️ **本文是探索期的诚实记录，已被 [`design.md`](./design.md) 部分修订。** 最主要的一处：本文「后续建议」把**身份**（改 `Source` + 三个消费者读它）列为第 1 刀；设计阶段判定**子 agent 事件不应进入父事件流**，故那一刀被消掉（bug 不会发生，而非被修好）。分歧以 [`design.md`](./design.md) 与 [ADR-0022](../../adr/0022-subagent-dispatch.md) 为准 —— **本文其余分析（缺口 D1–D10、约束、同类调研）仍然有效**。

---

## 概述

一句话：**用户给了两条最小要求 —— ① 能区分主 agent 与 subagent（身份）；② 主 agent 能派发 subagent 干活（派发）。今天两条都做不到，但第二条的地基（`Envelope.source`）从第一天就浇好了，而第一条恰好是今天最需要补的洞。**

结论先行（四条，按承重程度排）：

1. **今天结构上不可能派发**：`Tool::call` 拿到的 `ToolContext`（`crates/ys-tool/src/context.rs:6-18`）只有 `boundary / cwd / workspace_root / summarizer` —— **没有 Model、没有 Session、没有 EventSink**。工具**无法调模型**，因此无法跑子 agent。这不只是「缺一个字段」：`Agent::run_turn` 取 `&mut self`（`crates/ys-runtime/src/agent.rs:92`），外层回合期间已被借走，**同一个 `Agent` 实例在 Rust 借用层面也不可能被工具重入**。

2. **身份缝已经预留，但没人读**：`Envelope { source, turn, event }`（`crates/ys-protocol/src/envelope.rs:47-52`）的 `Source` 文档原话是「**单 agent 阶段的唯一来源**」（`:19`），即从一开始就是为多 agent 预留的。但**三个消费者目前一个都不读 `source`**：TUI `apply_event` 只看 `env.event`（`crates/ys-tui-coding/src/app.rs:86-186`）、`-p`/`--json` 的 `is_terminal` 只看事件种类（`apps/coding-agent/src/main.rs:217-223`）、`app_loop` 原样转发（`apps/coding-agent/src/app_loop.rs:137`）。⇒ **子 agent 的 `RunFinished` 会立刻关掉主 agent 的 `is_turning`、并让 `--json` 输出提前收尾**。这正是用户说的「至少能够区分」的落点，也是**最便宜的第一刀**。

3. **同类软件两极，且极端的那个自己认错**：`pi` **明确不做**（`README.md:501` 原话「**No sub-agents.**」），其示例扩展靠 **spawn 子进程**绕开；`CodeWhale` **做到了极致**（11 个 action 的 `agent` 工具 + 50 个上限常量 + focus/roster UI），但**它自己的设计文档承认这条路走错了** —— in-process fanout 把 TUI 拖慢，因为每个 child 都克隆重 runtime、重建工具注册表、每 child 渲染一张卡（`tmp/Codewhale/docs/AGENT_RUNTIME.md`）。**两极都不该照抄**：pi 的「不做」放过了真实需求（见下条），CodeWhale 的「全做」是三年工程量的债务。

4. **skills 侧在等它，而系统提示正在否认它**：JackCC 生态里 17 个 skill 依赖 subagent，本仓的应对是**声明能力边界** —— 常驻系统提示里逐字写着「**This runtime has no subagents and no plan mode — ignore instructions that ask for them.**」（`apps/coding-agent/src/prompt.rs:107`），且**有测试逐字钉住这句话**（`apps/coding-agent/src/prompt.rs:627`）。做 subagent ⇒ 这句必须改、那条测试必须改。**这是唯一一处「文档/提示主动否定某个能力」的地方**，改动它是本轮的可见副作用。

---

## 用户已澄清的四点（本轮问答）

> 这四条**直接约束设计空间**，下文凡涉及之处以此为准。

| # | 问题 | 答复 | 设计后果 |
|---|---|---|---|
| **①** | 子 agent 能否再派发子 agent？ | **不能自嵌套（工具表裁剪）** | 递归**结构上不可能** —— 子 agent 的工具表里根本没有派发工具。**零运行时检查、零深度上限参数**。这条同时把 D7 的「深度」一项整条消掉，也把「子 agent 的工具表 = 父的工具表吗」这个问号变成了明确的**否** |
| **②** | 子 agent 的进展要不要进 UI？ | **TUI 简单展示「subagent 在工作」即可，具体内容不展示；主 agent 需要处理 subagent 的返回和产物** | ① 身份**必须被 TUI 消费**（否则「在工作」无从显示）；但只到「状态指示」一级，**不做 roster / focus / 面板** ⇒ `CodingView` 至多加一个轻量字段（或零字段，见下）。② **`产物` 二字是新需求**：结果回传不能只有散文摘要，父 agent 要能**定位并使用子 agent 产生的东西**（D6 因此不是纯文本那么简单） |
| **③** | 子 agent 用哪个模型？ | **可指定（工具参数）** | 派发工具的 schema 带一个可选的 `model` 字段。**本仓今天没有 per-call 模型概念** ⇒ 需要一条新通道把「本次调用用哪个模型」传到「造子执行环境」那一层（`Wiring` 持单一 `model`，`apps/coding-agent/src/wiring.rs:129-131`） |
| **④** | 与 P0（prefix 指纹测试）的先后？ | **先补 P0 指纹，再加工具** | 本需求的**第一刀是 `hooks-and-plugins/notes.md` 待办①**，不是派发工具本身。指纹必须**在加任何工具之前**量出锚点，否则锚点本身就把新工具算进去了，等于没锁 |

**由 ② 衍生的一个待答细节**：「产物」＝子 agent 在**共享工作区**写下的文件（父子 `cwd`/`workspace_root` 相同，父用既有 `read`/`grep` 就能看），还是子 agent 需要**显式交出**一份东西？本仓父子共用工作区是默认形态，故「文件类产物」父本来就能看见 —— 真正要设计的是**父 agent 怎么知道去哪找**（子 agent 在结果里报路径？还是结构化一段清单？）。见 D6 与 Q5。

---

## 用户原话拆解（两条最小要求）

| # | 要求 | 本仓对应物 | 今天 |
|---|---|---|---|
| **①** | **区分主 agent 与 subagent** | `Envelope.source: Source` | 字段在，**消费者不读** ⇒ 事实上无法区分 |
| **②** | **主 agent 可派发 subagent 完成任务** | 一个 Tool（判据链见下） + 一套「造子执行环境」的能力 | **完全不可达**（见 §差距 D1/D2） |

「至少」二字很重要：用户把 **① 身份** 提在同一位，甚至更前。**这暗示最小可交付不是「完整 subagent 系统」，而是「identity + 一条能跑通的最简派发」**。display/roster/focus 可留待后续。

---

## 现有架构

### 模块边界

```text
ys-core / ys-event / ys-protocol        （零 tokio：纯数据 + 纯 trait）
   ↑
ys-model / ys-tool / ys-session / ys-component
   ↑
ys-loop                                  （BasicLoop；压缩在轮边界）
   ↑
ys-runtime                               （AgentBuilder + 无状态 Agent + AgentPorts）
   ↑
apps/coding-agent                        （接线器：三信道 + 两线程 + 启动组装）
   └── ys-tui-coding                     （独立 crate，**不依赖 ys-runtime**）
```

**关键后果（对 subagent 是硬约束）**：`ys-tool` 是核心 crate，**它拿不到 `ys-runtime` 的 `Agent`**（依赖方向单向）。故任何「工具内部跑一个子 Agent」的写法都会把 `ys-tool` 反向依赖到 `ys-runtime` —— 要么开新 crate，要么走**窄能力注入**（`Summarizer` 的先例）。

### 核心抽象

| 抽象 | 位置 | 形状 | 对 subagent 的意义 |
|---|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs:27-35` | `spec() -> ToolSpec` + `call(Value, ToolContext) -> Result<ToolResult, ToolError>`（仅此二法） | 派发**必须**走这里（判据链：模型决定何时做 ⇒ Tool） |
| `ToolContext` | `crates/ys-tool/src/context.rs:6-18` | `{ boundary, cwd, workspace_root, summarizer }` | **无 Model / 无 Session / 无 EventSink** —— 硬堵点 |
| `ToolResult` | `crates/ys-core/src/tool.rs:14-17` | `{ content: String, is_error: bool }` | 结果回传的**唯一**通道（无 `details`，ADR-0021 已决） |
| `Summarizer` | `crates/ys-tool/src/summarizer.rs` | 窄能力 trait；loop 构造、经 `ToolContext::with_summarizer` 注入 | **唯一的「给工具注入执行能力」先例**，是 D1 的直接模板 |
| `AgentPorts` | `crates/ys-runtime/src/agent.rs:18-31` | `{ model, session, events, boundary }` | 一次 `run_turn` 的全部外部端口 |
| `Agent`（无状态） | `crates/ys-runtime/src/agent.rs:55-63` | `{ loop_impl, registry, limits, cwd, workspace_root, approval, system_prompt }` | **不持 session/model/events** ⇒ 子 agent 需要**另建一个 `Agent`** |
| `RuntimeContext` | `crates/ys-component/src/context.rs:10-25` | `{ model, registry, session, events, limits, cwd, workspace_root, approval, system_prompt, boundary }` | loop 侧的全家桶；子 agent 若走 loop 路径，需要**同样一份的全新实例** |
| `Source` / `Envelope` | `crates/ys-protocol/src/envelope.rs:14-23, 47-52` | `Source(Arc<str>)`；`{ source, turn, event }` | **身份缝**（要求①） |
| `AgentEvent` | `crates/ys-event/src/event.rs:6-67` | 10 变体，`#[non_exhaustive]`，**扁平的、无 agent 身份** | 身份只能在**信封**层携带，事件本身不动 |
| `BoundarySource` | `crates/ys-protocol/src/boundary.rs` | `take()` / `is_aborted()`；**每回合必须新建** | 取消传播的载体（D5） |
| `StopReason` | `crates/ys-core/src/stop.rs:5-11` | `Completed / MaxRounds / Cancelled / Truncated` | 无「子 agent 失败」之类；子 agent 的终止原因是它**自己**的 `RunResult` |

### 今天：一次「派发」需要什么（缺口全景）

```text
模型 ──ToolUse{name:"task"}──► BasicLoop::run_turn
                                   │
                       registry.get("task")  ──► TaskTool::call(args, ToolContext)
                                   │                      │
                                   │                      └─ 想跑子 agent，需要：
                                   │                         · 一个 Model（或 model factory）
                                   │                         · 一个**独立的** Session
                                   │                         · 一个 EventSink（**带身份**）
                                   │                         · 一个 Agent / 一套 registry+prompt+limits
                                   │                         · 一个自己的 BoundarySource
                                   │                         · 递归深度 / 并发上限 / usage 归属
                                   │
                                   └─ ToolContext 只给了 boundary/cwd/workspace_root/summarizer
                                      ⇒ **以上六样一样都拿不到**
```

对应的既有单点接线（D1 的模板）：`crates/ys-loop/src/basic.rs:459-474` —— loop 在此 `registry.get` 并 `ToolContext::new(..).with_summarizer(&summarizer)`，注释自称「**唯一的摘要适配器接线点**」。

### 外部依赖

无新增依赖需求。子 agent 只用既有的 `Model` / `Session` / `EventSink` / `Agent` 原语。

---

## 差距：今天缺哪几块

> 拆成 D1–D10，**每一条都要在设计阶段给出取舍**。标 ★ 的是「不做决定就写不出代码」的硬项。

### ★ D1 工具怎么拿到「跑模型」的能力

`ToolContext` 无 Model。三条候选（供 design 阶段评估）：

| 候选 | 形状 | 先例 / 代价 |
|---|---|---|
| **a. 窄能力注入** | `ToolContext.delegator: Option<&dyn AgentDelegator>`，由 loop 构造并 `with_*` 挂载 | **照 `Summarizer` 原样**（`basic.rs:463-469`）。零新依赖方向，第三方工具不感知 |
| **b. app 侧自带 factory 的工具** | `TaskTool` 在 `apps/coding-agent` 构造，自带「造子环境」的闭包 | 不碰核心 crate，但**工具成了 core 之外的一等公民**，`ys-tool` 的「工具只依赖 ys-tool」契约破了 |
| **c. 循环内一等概念** | loop 识别某个特殊工具名 / ContentBlock，自己跑嵌套回合 | **与判据链冲突**：「模型决定何时做 ⇒ Tool」（`docs/extension-points.md:21-25`）。且把策略塞进 `BasicLoop` |

**约束（决定性，且比预想更紧）**：**`ys-model` 依赖 `ys-tool`**（为 `ModelRequest.tools: Vec<ToolSpec>`，`crates/ys-model/src/request.rs:3`；`crates/ys-model/Cargo.toml`）⇒ **`ys-tool` 反向依赖 `ys-model` 即构成环，被编译器禁止**。故落在 `ys-tool` 的 `ToolContext.delegator` **不能出现 `Model` / `ModelRequest` / `ModelError` / `Usage` / `Session` / `AgentEvent`** —— 它只能像 `Summarizer` 一样**窄到 `&str` 进 / `String` 出**（先例：`crates/ys-tool/src/summarizer.rs:1-20` 的开篇注释把这条理由写得很直白：「`ys-model` 依赖 `ys-tool` … 反向依赖成环 ⇒ 此处**不得**出现 `ModelRequest` / `ModelError` / `Usage`」）。

⇒ **推论（a 与 b 在此收敛）**：无论走哪条，**到达工具的「派发能力」必然是一个与模型/会话无关的窄接口**；真正的「造子执行环境」只能住在 `ys-tool` **之上**（`apps/coding-agent`，或一个夹在 `ys-runtime` 与 app 之间的新 crate）。这不是设计偏好，是依赖图的硬结论。

**约束**：`ys-tool` 不能依赖 `ys-runtime`（依赖方向单向，`CLAUDE.md`）⇒ 候选 a 的 trait 必须定义在 `ys-tool` 或更底，用 `ys-core`/`ys-model` 类型；候选 b 只有 app 层能写。

### ★ D2 子 agent 的执行环境从哪来

CodeWhale / Claude Code / pi **三家一致：子会话与父会话隔离**（子 agent 只回传一段摘要）。本仓要这一套，需要有人能造出：

- **`Session`**（新的，不能是父的 —— 否则父子消息互相污染）
- **`EventSink`**（**带 identity**，见 D3）
- **`Model`**（同一个？还是子 agent 用更便宜的？见 D7）
- **`Agent`**（工具表 / 系统提示 / limits —— 复用父的还是另建一套？）
- **`BoundarySource`**（每回合新建的硬约束，见 D5）

**结构观察**：这五样**恰好就是 `main.rs` 启动组装产出物**（`tools` → `build_agent` → `Wiring` → `ChannelSink`，`apps/coding-agent/src/main.rs:265-278, 596-615`）。⇒ **subagent 能力本质上是「把 app 的启动组装缩成一个可重复调用的 factory」**。这也解释了 CodeWhale 为什么需要一个 `SubAgentRuntime` 结构体：这是不可避免的，不是设计选择。

### ★ D3 事件的归属与路由（要求①的落点）

`Source` 已预留。最小改动是三处消费者开始读它：

| 消费者 | 位置 | 今天的风险 |
|---|---|---|
| TUI `apply_event` | `crates/ys-tui-coding/src/app.rs:86-186` | 子 agent 的 `RunFinished` → `finish_turn()`（`:133-144`）⇒ **主 agent 的 `is_turning` 被误关**，还会多印一条 Summary |
| `-p` / `--json` 消费循环 | `apps/coding-agent/src/main.rs:217-223, 240-249` | 子 agent 的终局事件 ⇒ **提前 `return`**，父回合剩余输出全丢 |
| `app_loop` 事件转发 | `apps/coding-agent/src/app_loop.rs:129-152` | 原样转发，不做归属（这条**不改也对**，前提是下游读 source） |

还需决定：**子 agent 的事件要不要进 UI？**（进 ⇒ 要 identity + 显示；不进 ⇒ 「区分」只剩 `--json` 的可观测性，UI 上看不到子 agent 在干活 —— 与用户原话的强调不符）。

### ★ D4 回合号语义

`Envelope.turn` 由 `ChannelSink::begin_turn(turn)` 盖章（`apps/coding-agent/src/channel.rs:277`），**全局单 sink、单一回合计数器**（`app_loop.rs:85, 121`）。子 agent 的回合号是：复用父的 `turn`？子自己的 1,2,3？还是需要一个二维 `(parent_turn, child_turn)`？**不决定就无法读 `--json` 输出。**

### ★ D5 取消传播

`BoundarySource` **每回合必须新建**（`Abort` 永久置位，复用会立刻取消之后每个回合 —— `CLAUDE.md` 硬约束 + `app_loop.rs:122-123`）。子 agent 需要自己的 boundary；`Abort` 要能**从父传到子**（CodeWhale 用 `cancel_token.child_token()`，`tmp/Codewhale/.../subagent/mod.rs:2931`）。同时 `BashTool` 的 100ms `is_aborted()` 轮询是子 agent 内部工具的唯一取消感知途径。

### D6 结果如何回传（含「产物」，答② 加权）

无 `details`（ADR-0021 决策 2 已否决），故只有 `ToolResult.content: String`。三种形态：

- **纯文本**（pi 的做法）：子 agent 最后一段 assistant 文本 → content。
- **哨兵包裹**（CodeWhale 的做法）：`<…:subagent.done>{json}</…>` + 一行摘要 —— 父 agent 能可靠拿到「成功/失败/产物位置」，但引入了**格式约定**（对模型是提示工程，对代码是解析负担）。
- **不引入**：直接把子会话 transcript 全量塞进 content —— **明确的坏选项**（token 爆炸 + 污染父上下文）。

⚠️ **答② 加了一层**：「主 agent 需要处理 subagent 的**返回和产物**」。父子共用工作区（`cwd`/`workspace_root` 同源），故**文件类产物父本来就能用既有 `read`/`grep` 看见** —— 真正要设计的是**父怎么知道去哪找**。三条路：① 子 agent 在结果文本里自己报路径（零机制，靠提示词约束）；② 约定一段「产物清单」格式（哨兵的轻量版）；③ 父自己扫工作区（不可靠，无法区分谁写的）。**倾向 ①②的组合**，但要显式记账。

### D7 模型选择、成本与上限（答①③ 已收窄）

- **模型**：答③ = **可指定（工具参数）**。⇒ 派发工具 schema 带可选 `model` 字段，省略则继承父。**新增一条数据通路**：本仓模型归 `Wiring` 单一持有（`apps/coding-agent/src/wiring.rs:129-131`），每次 `run_turn` 经 `AgentPorts.model: Option<&dyn Model>` 借用（`crates/ys-runtime/src/agent.rs:21`）—— 「造子执行环境」那一层必须能**按名字造出另一个 `Model`**。`Config::build_model` + `model_factory`（`apps/coding-agent/src/main.rs:774-788`）是现成的构造口，但它是**按 `Config` 的单例**，要变成「按给定 model 名构造」需一点改造。
- **usage 归属**：子 agent 的 token 进不进 `TurnStats`（`apps/coding-agent/src/status.rs`）与状态行的 `total_input_tokens`？**不进 ⇒ 用户看到的成本是假的。**
- **上限**：~~递归深度~~ **答① 已整条消掉**（子 agent 工具表裁剪 ⇒ 结构上不可能递归）。剩下**并发数**（本轮若只做单个，可暂不做）与**总 token 预算**（CodeWhale 有预算 scope 继承；本仓无）。**建议本轮只加一条最粗的**：派发工具的 `max_rounds` 走子 agent 自己的 `RunLimits`（已有机制，`crates/ys-component/src/limits.rs:8`），避免子 agent 无限跑。

### D8 显示（答② 定调）

- **答② = 「简单展示 subagent 在工作即可，具体内容不展示」** ⇒ 落在「最小形态」与「完整形态」**之间**：
  - ❌ **不做** roster / focus chip / composer target（CodeWhale 的完整形态，`docs/design/SUBAGENT_FOCUS.md`）—— 那要动 `CodingView` 字段 + `draw.rs` 面板。
  - ✅ **要做**一个轻量状态指示。**最省的实现可能零新数据**：派发工具的 `ToolCall` 到达即「开始」、其 `ToolResult` 到达即「结束」——`app.rs:103-132` 已有 `tool_index` 的配对机制。但那样「在工作」是靠**工具名**认出来的，与「用 `Source` 做身份」是两套；见 Q11。
- **最小形态（今天零 TUI 改动）**：塌成 transcript 一行 Tool 行（`crates/ys-tui-coding/src/transcript.rs:43` 的 `SUMMARY_KEYS` 抽摘要）。⚠️ `SUMMARY_KEYS` 只有 `path/file_path/command/pattern/query/url`，**没有能描述「派发」的 key** ⇒ 会落到「递归找第一个字符串」，摘要很可能是任务描述的前 40 字符 —— 可接受，但要知道是这个行为。

### D9 加工具的连锁点（既有债务，被本需求踩爆）

今天加一个工具要手改 **6 处**：`EXPECTED_ORDER`（`toolset.rs:40-50`）、`vec!` 字面量（`:94-106`）、三处 `assert_eq!(tools.len(), 9)`（`:226, :316, :493`）、`prompt.rs:117` 的 `tool_snippet` 分支、`prompt.rs:140` 的 `tool_guidelines` 分支。

⚠️ **且加工具会改变 prompt 前缀字节**，而**今天没有任何测试锁住它** —— 这正是 `docs/adr/0021` 决策 6 定的 P0 前置项（prefix 指纹测试），**尚未落地**。`docs/arch/hooks-and-plugins/notes.md` 的待办 ①/②（P0 指纹 + `ToolPlugin` 装配缝）与本需求**直接交汇**：subagent 工具会是那 6 处连锁点的下一个真实消费者。

### D10 系统提示必须改口

`apps/coding-agent/src/prompt.rs:107` 逐字写着「This runtime has no subagents…」，且 `:627` 有测试逐字断言它、`apps/coding-agent/tests/e2e_skills.rs:90` 也有一份拷贝。做 subagent ⇒ **三处同时改**。（这也说明「不做」是**有意的当前状态**，不是遗漏。）

---

## 同类软件调研（Step 2）

### pi —— 明确不做，做也靠子进程

| 维度 | 事实 | 出处 |
|---|---|---|
| 立场 | **「No sub-agents. There's many ways to do this. Spawn pi instances via tmux, or build your own with extensions」** | `tmp/pi/packages/coding-agent/README.md:501` |
| 内置工具 | **无** Task 工具。`ToolName` 只有 `read/batch/edit/write/grep/find/ls` | `.../src/core/tools/index.ts:95` |
| 唯一实现 | **示例扩展**（需用户手动装）：工具名 `subagent`，三模式 single/parallel/chain | `.../examples/extensions/subagent/index.ts:471-481` |
| 怎么跑 | **`spawn` 一个子 `pi` 进程**：`--mode json -p --no-session --model … --tools … --append-system-prompt …` | `index.ts:300-350` |
| 事件 | 子进程 JSON 在**扩展内部**消费，**不进父事件流**；无 `source`/`agentId` 字段 | `.../packages/agent/src/types.ts:431-446` |
| 身份/UI | **无 roster、无 focus**；只在 tool 卡片里渲染 | `index.ts:723-765` |
| 模型/提示/工具 | 由 agent 定义（Markdown frontmatter：`name/description/tools/model/systemPrompt`）；省略 `model` 则**继承父** | `subagent/agents.ts:11-19`；`index.ts:301-306` |
| 会话 | **完全隔离且不持久**（`--no-session`） | `index.ts:300` |
| 回传 | `content` = 子 agent 最后一段文本；结构化数据在 `details` | `index.ts:170-180, 163-168` |
| 上限 | 并发 8/4；**无递归深度限制**（子进程仍加载扩展 ⇒ 可无限嵌套） | `index.ts:33-34` |
| 取消 | 父工具调用的 `AbortSignal` → 子进程 `SIGTERM` → 5s 后 `SIGKILL` | `index.ts:410-424` |

**可借**：模式（single/parallel/chain）、agent 定义用 Markdown frontmatter、`model` 省略即继承。
**不可借**：子进程路线 —— 本仓 `CLAUDE.md` 禁止跨进程/动态库边界传 trait object，且进程边界会让「身份」变成跨进程序列化问题（本仓已有 `Envelope.source` 这个进程内的更优解）。

### CodeWhale —— 做到极致，并自认架构走错

| 维度 | 事实 | 出处 |
|---|---|---|
| 派发面 | **一个 `agent` 工具，11 个 action**：`start/roster/status/peek/message/followup/interrupt/wait/claim/release/cancel` | `tmp/Codewhale/crates/tui/src/tools/subagent/mod.rs:8440-8489, 8617-8707` |
| 怎么跑 | **in-process**：`SubAgentManager` + `tokio::spawn`；子 agent 有自己的 runtime / registry / `Vec<Message>` / transcript artifact | `mod.rs:6601-6619, 11586, 11666-11683` |
| 模型 | `model_strength: same\|faster`（2 值枚举，**非**分档评分）+ per-role 覆盖 + profile + 优先级链 | `mod.rs:1716-1742, 13573-13841` |
| 事件 | Engine → UI 显式事件：`AgentProgress/AgentComplete/AgentList/SubAgentMailbox`，**每条带 `owner_session_id` / `agent_id` / `spawn_depth`** | `crates/tui/src/core/events.rs:411-469` |
| Mailbox | 单调 seq 的信箱扇出（`Started/Progress/Completed/Failed/Interrupted/TokenUsage…`），`seal()` 作回合末屏障 | `tools/subagent/mailbox.rs:32-93` |
| 身份/UI | **完整**：roster（`AgentRosterRow`）+ focus（`AgentFocus`，一个 child 一个会话一个 composer target） | `tui/agent_roster.rs:107`；`tui/agent_focus.rs:42`；`docs/design/SUBAGENT_FOCUS.md` |
| 回传 | (a) 同步 ack（压缩掉 12KB 快照）；(b) **异步完成注入为父角色消息** + `<codewhale:subagent.done>{…}</codewhale:subagent.done>` 哨兵；(c) transcript handle | `mod.rs:9032-9051, 10628-10673, 10794` |
| 上限 | depth 默认 3 / 天花板 8；并发 64 / 128 / 1024；**子可再生子** | `crates/config/src/lib.rs:1951,1961`；`config/subagent_limits.rs:13-21` |
| 取消 | `child_token()` 递归传播；回合末 `park_and_wait()` 把子标为 **Interrupted 且可续**（非 Cancelled） | `mod.rs:2931, 2413-2448` |
| 成本 | per-child usage 进 mailbox；共享预算 scope（父继承/耗尽即拒新派生） | `mod.rs:5257-5288` |
| **自我批评** | 「**Architecturally, sub-agents should not be a second execution substrate**」；「in-process fanout 把 TUI 拖慢，因为每个 child 克隆重 runtime + 重建 registry + 每 child 一张卡；Claude Code / Codex / Kimi 的『保持 orchestrator 轻』来自 **isolation + 紧凑事件流**，**不是进程边界**」 | `docs/SUBAGENTS.md:14-28`；`docs/AGENT_RUNTIME.md:59-84, 86-101` |

**可借（三条，按价值排）**：
1. **「isolation + 紧凑事件流」才是本质**，不是进程边界 —— 直接支持本仓用 `Source` + 一个摘要回传的轻量路线。
2. **身份是逐事件携带的**（`owner_session_id` / `agent_id` / `spawn_depth`）—— 与本仓 `Envelope.source` 同构，且提示 `spawn_depth` 也许该进信封。
3. **结果回传是「异步注入父消息」而非同步阻塞** —— 父 agent 可以继续干活。本仓的同步形态（工具调用阻塞到子 agent 跑完）**是更简单的选项，但要显式记账**。

**明确不可借**：11 个 action、mailbox、roster/focus、预算 scope、checkpoint/parking —— **量级不是「加一个工具」，是三年工程**。CodeWhale 的文档自己给出了诊断。

### 三家对照

| 维度 | pi | CodeWhale | YuShan today |
|---|---|---|---|
| 有无 subagent | **无**（示例扩展 + 子进程） | **有**（in-process，极繁） | **无** |
| 身份载体 | 无（tool 卡片承担） | 逐事件 `owner_session_id`/`agent_id`/`spawn_depth` | `Envelope.source` 已预留，**消费者不读** |
| 子会话 | 独立子进程，`--no-session` | 独立 `Vec<Message>` + transcript artifact | 无 |
| 结果回传 | 最后一段文本 | 异步哨兵 + 摘要 + handle | `ToolResult.content` |
| 取消 | 子进程 SIGTERM/KILL | `child_token()` 递归 | 无 |
| 显示 | 一张 tool 卡 | roster + focus + composer target | 一张 tool 行（若塌成一行） |

---

## 约束

- **技术**
  - **依赖方向单向**（`CLAUDE.md`）：`ys-tool` **不能**依赖 `ys-runtime` ⇒ 「工具内部跑子 Agent」不能写成 `ys-tool → ys-runtime`；只能窄能力注入或 app 层组装。
  - ⚠️ **更紧的一条**：**`ys-model` → `ys-tool`**（`ModelRequest.tools: Vec<ToolSpec>`，`crates/ys-model/src/request.rs:3`）⇒ **`ys-tool` 不得依赖 `ys-model`（成环）**。故落在 `ys-tool`（`ToolContext` 所在层）的任何新能力 trait **不得出现 `Model` / `ModelRequest` / `Usage` / `Session` / `AgentEvent`** —— 只好像 `Summarizer` 那样 `&str` 进 / `String` 出。**「造子执行环境」只能住在 `ys-tool` 之上。**
  - **`ys-core` / `ys-event` / `ys-protocol` 零 tokio** ⇒ 任何新 trait/类型若落在这一层，不得带 tokio。
  - **`ys-tui-coding` 不依赖 `ys-runtime`**（编译器强制）⇒ 身份与显示只能经 `Outbound`/`CodingView` 传，UI 拿不到 `Agent`。
  - **不跨动态库边界传 trait object** ⇒ pi 的子进程路线出局。
  - **`Agent::run_turn(&mut self)`** ⇒ 同一个 `Agent` 实例不可能被工具重入（借用冲突）；子 agent 必须**另建** `Agent`。
  - **`ContentBlock::ToolResult` 是 JSONL 落盘契约，变体上无 `#[non_exhaustive]`** ⇒ 不要在结果里加结构化字段（ADR-0021 决策 2 已因此否决 `details`）。
  - **`BoundarySource` 每回合必须新建** ⇒ 子 agent 需要自己的、且父的 Abort 要显式向下传播。
- **性能**
  - **每轮固定开销**（system prompt + 工具表）= `RunLimits::fixed_prompt_tokens`（`crates/ys-component/src/limits.rs:15-24`），与压缩阈值耦合（ADR-0017）。**新工具 + 新 guideline 直接进这块固定成本**，且**改变 prompt 前缀字节** ⇒ 影响所有用户的 prompt cache（`docs/adr/0021`）。
  - **子 agent 的 token 是额外开销**：一次派发 = 一整套新的固定开销 + 子会话增长。**父子固定开销是否重复计费、子 usage 是否进 `TurnStats`，必须显式决定**（D7）。
  - `BashTool` 的取消靠 100ms 轮询（`is_aborted()`）；子 agent 若并发跑多个，取消延迟与资源占用需评估。
- **演进**
  - **本仓没有 branch / fork** ⇒ CodeWhale/pi 的「从历史重放重建子 agent 状态」在本仓**语义不成立**，不应照抄。
  - **follow-up 已设计未实现**（`docs/design-plans/2026-09-15-follow-up.md`，`apps/coding-agent/src/` 下无 `follow_up.rs`）—— subagent 与 follow-up 都会「增回合 / 跨回合驱动」，**边界需对齐**（前者是「模型派发」，后者是「系统追问」）。
  - **`docs/arch/hooks-and-plugins/notes.md` 的 12 项待办**里，①（P0 prefix 指纹）与 ②（`ToolPlugin`）**是本需求的前置或强相关项**。
  - **已知缺口**：todo 状态会被压缩摘要掉（同型问题会在子 agent 上重现 —— 子会话也会被压缩）。
- **组织**
  - 单人维护；「不设计假想未来需求」是明文原则（ADR-0014 / `docs/extension-points.md`）。
  - **判据链是本轮的第一筛选器**（`docs/extension-points.md:16-26`）：模型决定何时派发 ⇒ **Tool** 桶，**不是 Hook**。⇒ 本轮**不建 hook 系统**的结论不受影响。

---

## 需求范围

### 范围内（本轮 arch-design 应回答的）

> ✅ 已定：子 agent **不自嵌套**（工具表裁剪）、模型**可指定**、UI **只显示「在工作」**、**P0 指纹先行**。

- [ ] **D1 的取舍**：能力怎么进 `ToolContext` —— 受 `ys-model → ys-tool` 硬约束，**签名只能是 `&str` 进 / `String` 出**（见 D1 的「约束」段）。故真正要定的是：这个窄 trait 叫什么、放 `ys-tool` 还是别处、`ToolContext` 加字段还是照 `ApprovalHandler` 走 `AgentBuilder`
- [ ] **D2 的形态**：「造子执行环境」的 factory 落在哪一层、持有什么；**子 agent 的工具表具体裁剪成什么**（去掉派发工具是底线，是否连 `write`/`edit`/`bash` 一起裁？—— 裁剪越狠，子 agent 越安全但越没用）
- [ ] **身份的最小完整形态**（要求①）：`Source` 怎么表达 subagent（`Source::subagent(id)`？id 从哪来）？**三个消费者各自怎么读**（TUI / `-p`/`--json` / `app_loop`）？要不要 `spawn_depth`？
- [ ] **「子 agent 在工作」用什么信号驱动**（Q11）：复用工具行的 `ToolCall`/`ToolResult` 配对，还是走专用事件 / `Source`？
- [ ] **结果回传形态**（Q5）：纯文本 + 自报路径，还是约定产物清单格式？「返回**和产物**」各自怎么体现
- [ ] **per-call 模型的数据通路**（D7）：`Config::build_model` 的单例形态怎么改成「按给定 model 名构造」
- [ ] **回合号语义**（D4）
- [ ] **取消传播**（D5）与**子 agent 的 `RunLimits`**（复用 `max_rounds` 即可，但要定默认值）
- [ ] **usage 归属**：子 agent 的 token 进不进 `TurnStats` / 状态行（D7）
- [ ] **`prompt.rs:107` 那句「no subagents」改成什么**，以及 `:627` 与 `e2e_skills.rs:90` 两份拷贝怎么处理

### 范围外（明确不做的）

- **递归 / 自嵌套**：答① 已排除。⇒ **不需要深度上限参数**，也不需要「子 agent 的工具表 = 父的工具表」这类讨论。
- **`tmp/Codewhale` 量级的完整 subagent 子系统**：11 个 action、mailbox、roster/focus、预算 scope、checkpoint/parking —— 那是另一个量级的产品特性，不是「加一个工具」。答② 也明确只要「简单展示在工作」。
- **pi 的子进程路线**：跨进程/动态库边界传 trait object 被禁；且进程边界解决的是 pi 的问题（Node 生态天然可 spawn），不是本仓的。
- **从子会话历史「重放重建」状态**：本仓无 branch/fork，语义不成立。
- **后台/游离子 agent（detached）与工作流模式（chain/workflow）**：CodeWhale 的 `detached` 与 pi 的 chain 模式都是后续形态；本轮先做**同步、前台、单个**（并发是 Q1 的未定项）。
- **per-role 模型覆盖**：答③ 给的是「单次调用可指定」，不是「按角色配置的模型表」。后者与 `docs/arch/per-turn-model-switch/` 那个独立议题重叠，不在本轮。

### 关键场景

- **场景 1（成功路径）**：「把这个仓库里所有调用 X 的地方找出来并总结」→ 主 agent 调派发工具 → 子 agent 在**独立上下文**里 grep/read 若干轮 → 回传一段摘要 → 主 agent 的上下文**没被子 agent 的 20 次 read 撑爆** → 主 agent 继续推进。**这是 subagent 存在的唯一理由：上下文隔离。**
- **场景 2（身份）**：子 agent 干活时，UI **必须**能区分「这是子 agent 的输出」而不是主 agent 的 —— 否则 `is_turning` 误关、Summary 误印、用户看到交替打架的两段流（**今天的实际后果**）。
- **场景 3（取消）**：子 agent 跑到一半，用户按 Esc → Abort 必须传到子 agent（含它内部的 bash 轮询）→ 父回合以 `Cancelled` 收场，不留悬挂的子任务。
- **场景 4（失败）**：子 agent 撞 `max_rounds` / 模型报错 → **父回合不能因此失败**：应把「子 agent 失败」作为**错误结果**回喂父模型（ADR-0001 双通道），让父 agent 自行决定重试或换路。
- **场景 5（`--json`）**：子 agent 事件必须以可区分的形式出现在 stdout（否则「区分」在脚本消费侧不成立），且**不得让消费循环提前收尾**。
- **场景 6（成本）**：派发一次子 agent 后，状态行的 token 计数**要么反映真实总消耗，要么明确标注不含子 agent** —— 不能静默少算。

---

## 未澄清问题

> ✅ 已由本轮问答关闭：**Q2**（工具表 → 裁剪、不可自嵌套）、**Q3**（模型 → 可指定）、**Q4**（UI → 只显示「在工作」）、**Q7**（顺序 → P0 指纹先行）。

- [ ] **Q1（最关键）**：本轮的最小可交付是**「identity + 一条能跑通的最简派发」**，还是要一并做**并行 / 多子 agent**？用户说「至少」，但「至少」之后的边界未定。**已定的一半**：子 agent 不可自嵌套（答①）⇒ 即使将来要并行，也是**父层平铺多个**，不是树。
- [ ] **Q5（被答② 重新加权）**：结果回传要**纯文本**，还是**带一段可解析的产物清单**？答② 明说父 agent 要「处理 subagent 的返回**和产物**」⇒ 纯散文可能不够。但 `ToolResult` 无 `details`（ADR-0021 决策 2 已否决），结构化只能靠**约定格式的文本**（CodeWhale 用 `<…:subagent.done>{json}</…>` 哨兵，`tmp/Codewhale/.../subagent/mod.rs:10628-10673`）。
- [ ] **Q6**：**usage 归属** —— 子 agent 的 token 计入 `TurnStats`（`apps/coding-agent/src/status.rs`）吗？不计 ⇒ 状态行显示的成本是假的。
- [ ] **Q8**：`prompt.rs:107` 的能力边界声明改成什么？删掉「no subagents」、还是改成条件式？连带 `prompt.rs:627` 与 `apps/coding-agent/tests/e2e_skills.rs:90` 两份拷贝。
- [ ] **Q9**：**取消语义** —— 子 agent 被 Abort 时，父回合以 `Cancelled` 收场（照 `BasicLoop` 现状），还是把中断作为**错误结果**回喂父模型（照 CodeWhale 的「Interrupt 可续」）？
- [ ] **Q10**：与 **follow-up**（已设计未实现）的关系：两者都会额外驱动模型调用，是否共用上限与记账？
- [ ] **Q11（新，由答② 派生）**：TUI 的「subagent 在工作」用什么信号驱动？**零新事件的做法是现成的** —— 派发工具的 `ToolCall` 到达即「开始」、对应 `ToolResult` 到达即「结束」（`app.rs:103-132` 已有 `tool_index` 的配对机制）；也可加一个专用事件或用 `Source`。**但「区分」若只靠工具名，就与「用 Source 做身份」分家了** —— 需在设计里定一个说法。

---

## 后续建议

**四刀的顺序（前两刀独立可交付，且后两刀依赖它们）：**

| 刀 | 内容 | 为什么在这个位置 |
|---|---|---|
| **第 0 刀** | **P0 prefix 指纹测试**（`sha256(tools_json)` + `sha256(system_prompt)` 双锚点） | 答④ 已定。**必须在加任何工具之前**量出锚点 —— 否则锚点把新工具算进去了，等于没锁 |
| **第 1 刀** | **身份**（要求①）：`Source` 加 subagent 构造 + **三个消费者开始读 `source`** | 用户提在前的、且**独立可交付**。它修的是「今天就已经埋好的 bug」：子 agent 一上线，TUI 的 `is_turning` 误关、`--json` 提前收尾会立刻发生 |
| **第 2 刀** | **派发**（要求②）：窄能力 trait + 子执行环境 factory + 工具表裁剪 + 派发工具 | 依赖第 1 刀（子 agent 事件若无身份，第 1 刀的洞当场踩爆） |
| **第 3 刀** | **轻量状态指示**（答②）：TUI/输出层显示「subagent 在工作」 | 依赖第 2 刀 |

**建议用 `arch-design` 做方案对比**，把 **D1（能力怎么进 `ToolContext`）** 与 **D2（子执行环境从哪来）** 当作**第一、第二决策点**。注意 **`ys-model → ys-tool` 这条硬约束已把 D1 的签名压死**（只能 `&str` 进 / `String` 出），故三个候选的差别不在「接口长什么样」，而在**「谁持有造子环境的那份能力」**。

**建议对比的三个候选**：

- **甲：窄能力注入（照 `Summarizer` 原样）** —— `ys-tool` 加一个 `&str` 进 / `String` 出的 trait；`BasicLoop` 在**既有的单点接线处**（`crates/ys-loop/src/basic.rs:459-474`，注释自称「唯一的摘要适配器接线点」）构造并挂载。app 侧实现它。零新 crate、零新依赖方向。**代价**：`ys-loop` 要知道「子 agent」这个概念的存在（哪怕只作为一个不透明的窄 trait）。
- **乙：能力不进 `ToolContext`，派发工具自带 factory** —— `apps/coding-agent` 构造一个持有「造子环境」闭包的 `TaskTool`，直接注册进工具表。`ys-tool` / `ys-loop` **一个字不动**。**代价**：工具不再是「只依赖 `ys-tool` 的纯实现」，且它**仍然拿不到父的 `ToolContext`**（取消传播、cwd 得另想办法）。
- **丙：循环内一等概念** —— `BasicLoop` 识别某个工具名/ContentBlock，自己跑嵌套回合。**建议作为反例对比**：它与判据链「模型决定何时做 ⇒ Tool」（`docs/extension-points.md:21-25`）冲突，且把策略塞进 `BasicLoop`（`notes.md` ③ 称之为最高爆炸半径）。

**其余三条设计纪律**：

- **若要动 `BasicLoop`，先回到 `docs/extension-points.md` 的判据链**：模型决定何时派发 ⇒ **Tool**，不是 Hook；因此**本轮不该建 hook 系统**（维持 ADR-0014）。若设计过程中发现必须改 `BasicLoop`，那是「开了新路」，应照 `notes.md` ③ 的准入规则**显式记账**。
- **子 agent 的失败必须是「错误结果」而不是「基础设施失败」**（`docs/CONTEXT.md` 的失败双通道）：子 agent 撞 `max_rounds` / 模型报错时，**父回合不能因此失败** —— 应回喂一条 `is_error` 结果让父 agent 自己决定重试或换路（同 [ADR-0001](../../adr/0001-tool-failure-dual-channel.md)）。
- **「产物」要显式设计，别留给提示词**（答② 的新要求）：父子共用工作区，文件类产物父本就能 `read` 到 —— 要定的是**父怎么知道去哪找**（D6/Q5）。

---

## 参考

- **设计（本文的下一步）**：[`design.md`](./design.md)、[ADR-0022](../../adr/0022-subagent-dispatch.md)
- **同类软件调研（12 项目完整版）**：[`research.md`](./research.md)
- 判据：[`docs/extension-points.md`](../../extension-points.md)（四者边界 + 判断链 + 六个用例）
- 决策：[ADR-0010](../../adr/0010-actor-model-agent-api-revision.md)（Agent 无状态 / actor 上移）、[ADR-0013](../../adr/0013-cancel-token-removal-boundary-source.md)（取消即消息）、[ADR-0014](../../adr/0014-follow-up-without-hook-system.md)（不建 hook）、[ADR-0021](../../adr/0021-plugin-seam-without-hook-system.md)（装配缝 / 不引入 details）
- **直接交汇**：[`docs/arch/hooks-and-plugins/notes.md`](../hooks-and-plugins/notes.md)（待办 ①P0 指纹 / ②`ToolPlugin`；④ 五条缝的装配方式）
- 术语：[`docs/CONTEXT.md`](../../CONTEXT.md)（Turn / Round / StopReason / Follow-up / Compaction）
- 相邻已设计未实现：[`docs/design-plans/2026-09-15-follow-up.md`](../../design-plans/2026-09-15-follow-up.md)
- 上下文压缩（子会话同样受其约束）：[`docs/design-final/compaction.md`](../../design-final/compaction.md)
- 加工具的连锁点先例：[`docs/arch/skills/context.md`](../skills/context.md)（D1 推论 1）
- subagent 缺口的历史记录：[`docs/arch/skills-and-compaction/context.md`](../skills-and-compaction/context.md)（`:237-238, 265, 317`）、[`docs/arch/per-turn-model-switch/context.md`](../per-turn-model-switch/context.md)（`:193`）、[`docs/arch/webfetch-summarize/context.md`](../webfetch-summarize/context.md)（`:153`）
- **参照实现（未入版本库）**：`tmp/pi` 的 `packages/coding-agent/examples/extensions/subagent/{index.ts,agents.ts}`、`README.md:501`、`docs/usage.md:309`；`tmp/Codewhale` 的 `crates/tui/src/tools/subagent/mod.rs`、`crates/tui/src/tui/{agent_focus.rs,agent_roster.rs}`、`docs/{SUBAGENTS.md,AGENT_RUNTIME.md}`、`docs/design/SUBAGENT_FOCUS.md`
