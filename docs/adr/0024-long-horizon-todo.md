# 0024 — 长程闭环：todo 真值落外部文件、回合边界注入、follow-up 按「有未完成项」推进

给「长程任务的成功率与闭环」定下**状态、可见性、推进**三件事的落点：① **todo 真值搬出会话**，落在会话同级的 `<stem>.todo.json`（**supersede 0021 决策 5**，并**推翻** 0021 的「否决落盘持久化 todo」）；② 可见性靠 **`app_loop` 在回合边界追加一条 `Role::User` 消息到 `messages` 尾部**，判据是「**状态变了 ∨ 会话里找不到标记**」—— 绝不改 `system`；③ 推进靠 **follow-up 的纯函数判据「有未完成项」**（替换 HEAD 对比），配**五道护栏**。新增终止态 **`Cancelled`**，「全部完成后删除」的判据是「**无 open 项**」而非「全部 Completed」。`todo` 与 `subagent` **都排工具表末两位**（子表须仍是父表的字节前缀）。**零核心 crate 改动、零改 `BasicLoop`。**

## Status

accepted（2026-09-23）

## 背景

- **需求**（原话）：「重新考虑 todo 的设计，我们需要提供**长程任务的成功率**和**闭环能力**。它可以和 subagent 结合起来，比如主 agent 按照 todo 派发 subagent 等等、agent 按照 todo 逐个完成等等」。澄清见 [`../arch/long-horizon-todo/context.md`](../arch/long-horizon-todo/context.md) §7（7 条，含「回合之间」「持久化」「写计划=Tool 推动=Follow-up」「主 agent 执行、只有主 agent 有 todo」「侧栏展示」）。
- **这不是 0021 的增量，是换了问题类别**：0021 问「第三方怎样加一个功能」（**能力装配**），本 ADR 问「谁推进、状态活多久」（**控制流 + 状态**）。而 0021 给 todo 定的状态位置 —— **对话上下文**（决策 5）—— **恰好是长程任务里最先消失的地方**：压缩摘的就是旧消息，任务越长那条 `todo` 调用越早变成旧消息。
- **同类软件三家收敛**（Manus `todo.md` / Claude Code Tasks / Cline Focus Chain）：① 计划必须**持久**（外部存储）；② 计划必须**被反复推回上下文**（Manus 每次重写 / Cline 每 6 条消息，**都不是「注入一次」**）；③ 计划必须**活过压缩**。
- **成本结构是总开关**：prompt cache 是字节前缀（`tools → system → messages`），命中 ≈0.1×。`system` 每轮整块重发 ⇒ 动它 = 每轮废掉其后全部；`messages` **尾部**注入只让**新追加的那一段**按 1.0× 付，且此后进前缀。工具面每轮变动的代价（每轮 +9,000）是静态加一个工具的 **52 倍**。
- **判断链的结论**（[`../extension-points.md`](../extension-points.md)：模型决定何时做 ⇒ **Tool**；无论模型怎么想都必然发生 ⇒ 下一条；回合之间 ⇒ **Follow-up**）：**写计划 = Tool，推动 = Follow-up** —— 两件事天然是**两个机制**，用一个机制覆盖两者正是 0014/0021 反复否掉的错。

## 决策

### 1. todo 真值搬出会话：`<sessions_dir>/<stem>.todo.json`

- **Supersede 0021 决策 5**（真值 = 会话里最后一次 `todo` 调用的 `arguments`）。会话里的 `arguments` 降级为**写入日志**，也是删除后的审计余量。
- **推翻 0021 的「否决落盘持久化 todo」**：原否决的两条理由是「另一个量级的产品特性」与「引入第二真值源使 `/new` 语义含糊」。后一半由本决策正面回答（**文件是唯一真值**，`/new` 换 stem ⇒ 计划天然归零，与 0021 自己写的「`/new` 后 todo 归零是正确语义」一致）；前一半由长程需求承担。
- **落点选会话同级**而非新目录：复用 `Wiring::latest_session_path()`（`wiring.rs:64,155`）的重启恢复，**零新目录、零新环境变量**，且 `YUSHAN_SESSIONS_DIR` 的测试覆盖自动生效（既有测试不改一行就不会碰用户的真实 `~/.yushan`）。
- `-p` / `--json`（一次性会话）⇒ **纯内存 store，不落盘**。

### 2. 可见性：`app_loop` 回合边界追加一条 `Role::User` 到 `messages` 尾部

- **绝不改 `system`** —— 0021 决策 1 的硬约束，本 ADR 首次**真正使用**它，并加一条不变量测试（`system_prompt_does_not_depend_on_todo_state()`）把它变成机械保证。
- **判据 = `状态变了 ∨ 会话里找不到标记`**（纯函数 `should_inject`）。前半堵住「上下文里堆 N 份互相矛盾的副本」；后半是**自愈** —— 压缩（或手敲 `/compact`）把块吃掉后自动补回，**不需要**监听 `ContextCompacted` 事件，因此也覆盖得了那条不发事件的手敲路径。
- **注入点选 `app_loop` 而非 `BasicLoop`**：`BasicLoop` 是最高爆炸半径；且直接 `session.append` **不发事件** ⇒ todo 块不显示成用户消息（对比 `Boundary::Steer` 会 emit `AgentEvent::UserMessage`，`basic.rs:222`）。
- **渲染必须是纯函数**：同样内容恒产出同样字节（**不得含时间戳 / turn 号 / session id**）—— 这是「状态没变就不注入」判据成立的前提。

### 3. 推进：follow-up 判据换成「有未完成项」，五道护栏

- **判据**：`store.has_open_items()`。替换 [`design-plans/2026-09-15-follow-up.md`](../design-plans/2026-09-15-follow-up.md) §3.2 的 git HEAD 对比 —— 后者是为「commit 后同步文档」设的，与 todo 无关，且需要 fork。**seam 复用**：该文件的 `while let Some(msg) = pending.take()` 循环正是本设计的落点；将来 commit-doc 追问只是**同一循环里的第二个判据**（加法）。
- **五道护栏**，缺一不可：
  1. **计数上限**（`MAX_FOLLOW_UPS = 8`，替换原默认 1）—— 防「跑太久」；
  2. **停滞护栏**（本轮 `revision` 未变 **且** 模型自称收尾 ⇒ 停）—— 防「**原地打转**」。这是**真正的防套娃**：上限只防量，它防质；
  3. **`stop_reason` 门**：`Cancelled` / `Truncated` **一律不续**（用户 Ctrl+C 永远赢）；
  4. **用户输入抢占**：追问链路每次迭代开头先 `try_recv`，有待处理输入立即中断（否则 8 次追问会把用户输入排队到 8 个回合之后）；
  5. **防逃逸**：写入拒绝丢弃既有非终态项（见决策 4）。
- **不需要开关**：老设计默认关，是因为触发与用户意图无关；**本设计的触发前提是用户自己的计划** —— 模型不写计划 ⇒ `NoPlan` ⇒ 永不追问。**开关从「配置项」变成了「计划的有无」。**

### 4. 加终止态 `Cancelled`；删除判据 = 「无 open 项」；执行者是代码

- **删除判据不写成「全部 Completed」**：那样一个被**放弃**的项永久非 Completed ⇒ **永不满足 ⇒ 永不删除**。改成「**无 open 项**（无 `Pending` / `InProgress`）」后，这一条**结构上不可能发生**。
- **`Cancelled` 是让模型能诚实停下的唯一出口**：没有它，模型想让环停下就得把没做完的事标成 `completed` —— 那是**伪造完成**，与引入 `StopReason::Truncated`（拒绝把截断伪装成 Completed）是同一条纪律。
- **执行者是代码，收口在 `TodoStore::apply`** —— 任何写入者（模型经工具、将来的 `/todos` 命令、`/new`）都不可能留下一个已关闭的计划。这是「不靠模型自觉」的结构保证（`notes.md` ①：「确保完成是控制流那一半」）。
- **回喂显式**：工具结果里明说「计划已完成并清空」，否则模型会困惑于「我的列表去哪了」。
- **诚实的天花板**：todo 是模型的**自陈**，不是**验收**。本 ADR 只能强制「再问一次」，不能强制「真的做完」—— 模型可以把全部项标成 `completed` 然后收工，代码照约删除。**真正的保证来自可验证的验收**（测试绿、构建过），不来自清单。

### 5. `todo` 与 `subagent` 都排工具表末两位；子表须仍是父表的**字节前缀**

- **扩展 0022 D3**（原：子表 = 父表 − 派发工具）为 **− {`subagent`, `todo`}**，并加一条更强的硬规则：**被摘的两项必须都在后缀区**，否则子 `tools` JSON 不再是父的**字节前缀**，0022 D3 的缓存理由（每次派发省 ~1.6k + 其后 system 命中）失效。
- 顺序定为 `…, webfetch, todo, subagent`：`subagent` 保持末位可让 0022 D18/§6 的措辞继续成立，`todo` 插在它之前 ⇒ **改动面最小**。
- **「只有主 agent 有 todo」由结构保证，零运行时检查**：子 registry 里没有 `todo` ⇒ `registry.get("todo")` 返回 `None` ⇒ 走既有的「tool not found」分支。提示词层同步门控（`if tools.contains(&"todo")`）⇒ 子 agent 的 system prompt 里**既无清单行、也无纪律句**（0022 D15 的「one visibility」：看不见 + 执行拒绝）。
- **`todo` 永不声明 `is_concurrency_safe()`**（默认 `false`）⇒ 0022 D17 的条件并行路径永远不会并发两个 `todo` 调用。这是「共享状态 + 并行」的唯一防线。
- **将来若给子表加回 `todo`（共享列表）**：会**同时**打破字节前缀与单写者两个不变量 ⇒ 那时本节结论要**重算**，且需引入锁/仲裁/冲突合并。

### 6. 不引 hook / 不引依赖图 / 不引共享列表 / 不引 trait 列表

四条都是**零消费者抽象**：
- **hook / 注入系统**：消费者仍为 0（0021 决策 1 继续有效）。本设计的注入是**我们自己的循环**在做，不是「加工别人的工具结果」。
- **依赖图 `blockedBy`**：本仓单 agent **串行**执行（`basic.rs:423` 注释自陈「串行执行工具调用（v0）」），没有调度器去消费「解锁」事件 ⇒ 零消费者。
- **共享列表 + 广播**：与需求「只有主 agent 有 todo」正面冲突，理由是**结构的**（见决策 5 的「将来若…」）。收益是零：子 agent 看不到父的对话（0022 D19）⇒ 它也不需要父的计划；计划由主 agent 在派发时**编译进 `prompt`**。
  > **代价如实**：主 agent 把一项 todo 拆给 3 个子 agent 时，那 3 个之间无法通过共享列表协调（只能靠 `prompt` 写清边界）。
  > **翻案条件**：出现「子 agent 之间需要动态认领彼此的工作」的真实需求。
- **`TodoStore` 核心 trait / `FollowUpRule` trait / `InjectionStrategy` trait**：各只有 1 个实现 ⇒ 退化为**具体类型 + 纯函数**（本仓对「策略」的既有形状是纯数据/纯函数，如 `CompactPolicy` + `compact::plan`）。若将来真要「每轮重述」形态，**那是一个 bool 字段**，不是 trait。

## Considered Options

- **候选甲（最小复杂度）**：全部落 `apps/coding-agent`，`Arc<Mutex<TodoStore>>` 由工具直接持有，零核心改动。**部分采纳** —— 注入判据（状态变 ∨ 标记丢失）与 `Arc` 持有被采纳；**拒其护栏不足**（只有计数上限，无停滞护栏、无防逃逸）。
- **候选乙（可扩展优先）**：`ys-tool::TodoStore` **窄 trait** + 三跳注入（`ToolContext → RuntimeContext → AgentPorts`）+ **`basic.rs` 1 行** + `FollowUpRule` trait 列表。**部分采纳** —— **停滞护栏**被采纳；**拒其抽象与落点**：0022 用窄 trait 注入的**唯一理由**是「工具不能快照 `Model`（`/model` 之后静默用旧模型）」，**todo 没有这个面**（真值是一份文件，`/new` 由接线器**就地改指向**，不存在过期快照）；且把 app 生命周期绑定的 store 做成核心 trait 今天**没有第二个实现者**。另：它选的 `Steer` 补注入实测会 emit `UserMessage`（`basic.rs:222`），使 todo 块显示成用户消息。
- **候选丙（缓存/成本优先）**：新 adapter crate + `~/.yushan/todos/` 新目录 + **每回合无条件重放** + `StopCause` 六态枚举 + 防逃逸。**部分采纳** —— **adapter crate 落点**、**防逃逸**、**`StopCause` 显式枚举**被采纳；**拒其注入频率**：「每回合无条件重放」在状态未变时是**纯浪费**（`prompt-delivery/notes.md:37` 的结论：内容不变却注入 = 纯浪费），且它自陈的「轮级新鲜度缺失」用一个更贵的默认去补，方向相反。另：新目录 + 新环境变量相对「会话同级」无收益、多一处需处理「会话删了 todo 怎么办」。
- **让计划免疫压缩**（给 session 加保留标记）：需要新的核心契约 + 改 `compact.rs` 的切分算法。0021 已判过同一件事。**否决** —— 真值搬到压缩够不着的地方，是同一个问题的零成本解。
- **每轮注入**（Manus 形态）：成本 ×`R`（R=5 ⇒ +43.9%，R=10 ⇒ +100%），且内容不变时纯浪费。**默认否决**，但**保留逃生门**：轮级注入经 `Boundary::Steer`（`push(&self)`，`boundary.rs:74`）⇒ **零 `BasicLoop` 改动**即可拿到轮级粒度，代价 ×R。触发条件：实测出现「长回合里明显丢失计划」。
- **子 agent 共享同一份计划**（Claude Code Tasks 形态）：见决策 5/6 的否决理由。**Anthropic 做 Tasks 的核心动机恰恰是 subagent 协作**（官方定位原话：「Tasks = 跨 session、subagent、context window 协调工作」）—— 本仓**反向**选择，理由是成本结构（每次派发 ~18.1k 子 system）决定子 agent 只干**短、自包含**的任务。
- **`ToolResult.details` 携带结构化 todo 状态**：0021 决策 2 继续有效。

## Consequences

- **`apps/coding-agent` 的 `wiring` / `app_loop` 各增一份生命周期责任**：`Wiring` 持 `Arc<TodoStore>` 并在 `/new` 时 retarget（同一 `Arc`，身份不变）；`app_loop` 多一层 `while` 循环与一次回合边界注入。
- **`build_tools(workspace, web)` 签名多一个 `Arc<TodoStore>` 参数**（app 内部函数）。
- **`EXPECTED_ORDER` 从 9 项变 10 项（todo）→ 11 项（subagent）**，并新增一条注释把「末两位都必须可摘」写成不变量。
- **`prompt.rs` 的 `tool_snippet` / `tool_guidelines` 各加一支**（门控），且有既有测试 `every_real_toolset_tool_has_non_fallback_snippet` 强制这一点。
- **`-p` / `--json` 无闭环**（不走 `app_loop`）—— 明确接受的功能退化，也是本设计最大的缺口。
- **`/export` 不带走活计划**（导出的仍是对话），但**注入过的快照会跟着走**（它们本就是普通消息）。
- **摘要散文（`## Progress` / `## Next Steps`）与结构化 todo 仍可能不一致** —— 未解决，裁决是**让权威源唯一**：追问判据只读 todo（机器读），摘要只作模型的理解背景，**矛盾时行为由数组决定**。
- **落地顺序**（`context.md` §8.5 已定）：`P0（指纹测试）→ P1（ToolPlugin 缝）→ subagent（第 10 个工具）→ 本设计（第 11 个）`。P0 的字节指纹**盖不住本设计的前两类锚点**（回合形状 / 控制流），需另立。
- 设计全文（含三个候选的对比矩阵、成本模型、模块划分与接口）：[`../arch/long-horizon-todo/design.md`](../arch/long-horizon-todo/design.md)；上下文：[`../arch/long-horizon-todo/context.md`](../arch/long-horizon-todo/context.md)。
