# todo 清单 + 目标验收层 设计计划

> 日期：2026-11-06
> 决策记录：ADR-0024（清单层）、ADR-0072（验收层）、ADR-0021 决策 3（`ToolPlugin` 装配缝）
> 性质：**新增能力的设计**。本文件记录「这一期为什么这样切、边界在哪、改动面多大、怎么验」。
> 执行波次见 [`../exec-plans/2026-11-06-toolplugin-todo-verification.md`](../exec-plans/2026-11-06-toolplugin-todo-verification.md)（W0–W9）。
> 设计全文见 `../arch/long-horizon-todo/design.md` §6–§8、`../arch/cross-turn-advance/design.md` §5、
> `../arch/hooks-and-plugins/design.md` §5.2 / §6。

## 1. 背景与动机

Runtime 已经有两条「该不该再起一个回合」的机制：

- 一期 follow-up：`git HEAD` 前后不等 ⇒ 再问一次（`apps/coding-agent/src/post_turn.rs:66` 的
  `should_follow_up`，`apps/coding-agent/src/app_loop.rs:801` 调用，受 `follow_up_on_commit` 开关控制）。
- 二期定时唤醒：`has_work` 零 token 判据（`../arch/cross-turn-advance/design.md` §4.2）。

两者都只回答「**该不该醒**」，都不回答「**还剩什么**」，更不回答「**声称做完的那条，真做完了吗**」。
长任务上这留下两个洞：

**洞一：模型忘掉计划。** 回合越走越长，压缩摘掉的就是旧消息，而「我们本来要干什么」恰好写在最早
的那条消息里。模型于是重复劳动、漏掉后段、或者干到一半开始即兴发挥。压缩摘要里的
`## Progress` / `## Next Steps` 是散文，机器读不了，也没法当判据。

**洞二：模型自称完成但没完成。** 执行回合的模型说「已完成」不构成证据 —— ADR-0041 正是为此撤掉了
「完工复核回合」（`docs/adr/0041-remove-completion-review-turn.md`）。它留下的硬要求是：日后若实测
这个比例不可忽略，重新引入的机制**不能只在一条入口上生效**。

于是这一期分两层补上，**顺序不可换**（先有清单，判官才有核对对象）：

| 层 | 回答 | 机制 | 成本 | 出处 |
|---|---|---|---|---|
| **清单层** | 还剩什么活没干 | `TodoStore::has_open_items()` + 回合边界注入 | 零 token | ADR-0024 |
| **验收层** | 声称干完的那条，证据在哪 | 判官 + 证据窗口 + 引用校验 | 每轮一次模型调用 | ADR-0072 |

清单层是**状态**：真值落盘，活过压缩、活过重启、活过回合。
验收层是**判据**：拿清单逐条去证据里找凭据，找不到就不放行。

**清单单独用不够**：勾选本身也是自陈，模型可以把全部项标成 `completed` 然后收工（ADR-0024 决策 4
自陈的天花板）。**验收单独用也不够**：判官没有核对对象，只能对着一句话猜「算不算完成」。
两层咬合才成立。

## 2. 概念边界

六级，前两级已有权威定义（`docs/CONTEXT.md`），后四级为本期新增。

| 层级 | 定义 | 权威出处 |
|---|---|---|
| **轮**（Round） | 回合内的一次「模型调用 +（可能的）工具执行」往复。一个回合包含一或多轮，受最大轮数约束 | `docs/CONTEXT.md:13-15` |
| **回合**（Turn） | 一次输入进入 Runtime 到产出结果的一次完整处理：模型与工具往复推进，直到给出回答、触顶或被取消 | `docs/CONTEXT.md:9-11` |
| **todo 项** | 清单里的一条待办：`{ 内容, 状态 }`，状态 ∈ `Pending` / `InProgress` / `Completed` / `Cancelled` | ADR-0024 决策 1、决策 4 |
| **goal** | 用户用一句话给出的**目标**（`/goal` / `--goal`）。只说「想达到什么」，不说「什么算完成」 | ADR-0072 D1 |
| **verdict** | 判官对「目标是否达成」的判定：`Met` / `NotMet` / `Blocked` | ADR-0072 D5 |
| **evidence** | 证据窗口：工具调用（名字 + 参数）、工具输出、harness 事实。**不含助手正文** | ADR-0072 D4 |

**回合 ≠ 轮**：轮在回合**之内**，回合由多轮组成。这一期的所有动作都发生在**回合边界**，不在轮边界 ——
注入（ADR-0024 决策 2）、判官求值（ADR-0072 D11）、继续条件求值都是回合级。唯一在轮内的相关机制
是压缩（`docs/CONTEXT.md:36-41`），而注入的设计恰恰是为了在压缩后自愈。

**goal ≠ todo 项**：goal 是「本回合想达到什么」，todo 项是「还剩什么活」。前者过期即失效，
后者重启后仍然有效 —— 所以 **goal 不落盘**（D14），**todo 落盘**（D2）。

**verdict ≠ todo 项的状态**：todo 项的状态是模型的**自陈**，只能当「待核对的问题」；
verdict 是判官对证据的**判定**。两者不可互换，这正是引用校验存在的理由（§3.7）。

**evidence ≠ 会话全文**：证据窗口只取**最后一轮**（D6），且**排除助手正文**（D4）。
排除是按 `ContentBlock` 变体做的，不是按 `Role` —— 工具结果以 `Role::User` 进 session
（`crates/ys-loop/src/basic.rs:195-200` 的 `Boundary::Steer` 路径），按 `Role` 区分会把它误排。

## 3. 关键设计决策

### 3.1 todo 真值落外部文件，不进会话

**决策**：真值在 `<sessions_dir>/<stem>.todo.json`，与会话同级。会话里的 `arguments` 降级为写入日志。
`-p` / `--json` 用纯内存 store，不落盘。（ADR-0024 决策 1、ADR-0072 D2）

**理由**：三条独立的必要性。① **活过压缩** —— 压缩摘的就是旧消息，真值在会话里必然被摘掉。
② **活过重启** —— 会话同级目录 ⇒ 复用既有的会话路径恢复（`apps/coding-agent/src/wiring.rs:302` 的
`session_path()`），零新目录、零新环境变量，`YUSHAN_SESSIONS_DIR` 的测试覆盖自动生效。
③ **活过回合** —— 长程任务必然撞轮数上限 ⇒ 多回合，真值不能只在回合内有效。
附带收益：`/new` 换 stem ⇒ 计划天然归零，语义清楚。

**代价**：多一个文件、多一条 IO 错误路径（文件损坏 ⇒ 空列表 + 降级提示，绝不 panic）。
`-p` 与 TUI 因此有**两种 store 形态**（内存 / 落盘），两处都必须测。

### 3.2 `todo` 排父表追加段末位，子表结构上拿不到

**决策**：父表顺序为 `… bash_bg, subagent_async, subagent_send, todo`。
`todo` 只进父表的**追加段**，不进 `build_tools()` 的 11 元基础表。
（ADR-0024 决策 5、`docs/arch/long-horizon-todo/design.md` §7.4 e）

**理由**：子 agent 的工具表就是那 11 元基础表（`apps/coding-agent/src/assembly.rs:149`），
父表在它之后追加（`apps/coding-agent/src/assembly.rs:198-219`）。把 `todo` 放进基础表会同时打破两条不变量：

1. **子表字节前缀**（`apps/coding-agent/src/toolset.rs:684` 断言父表 JSON 以子表 JSON 为前缀）——
   每次派发多付固定前缀，且其后 system 全 miss。
2. **单写者**——并行派发下多个子 agent 并发写同一份 JSON 就是 lost update，要引入锁/仲裁/合并。

而收益是零：子 agent 看不到父的对话（子 prompt 必须自包含），它也就不需要父的计划 ——
计划由主 agent 在派发时**编译进 prompt**。

**代价**：主 agent 把一项 todo 拆给 3 个子 agent 时，那 3 个之间无法通过共享列表协调，
只能靠 prompt 写清边界。`subagent_send` 让出末位（ADR-0022 相关措辞需同步）。

### 3.3 回合边界尾部注入 `Role::Harness` 消息，绝不改 `system`

**决策**：`app_loop` 在回合边界 `session.append()` 一条携带渲染块的 `Role::Harness` 消息。
判据是纯函数 `should_inject(store, last_injected_rev, session)` = **状态变了 ∨ 会话里找不到标记**。
**绝不改 `system`。**（ADR-0024 决策 2）

**为什么是 `Harness` 而不是 `User`**：用户没说过这些话，记成用户说的会污染「我是谁、我说过什么」
（同 `../arch/cross-turn-advance/design.md` §3.1a）。出站时适配器把它映射成 user 角色
（`adapters/model-openai-compatible/src/lib.rs:1424`）⇒ 模型看到的仍是 user 输入；
TUI 画成 `System` 行 ⇒ 用户一眼能看出不是自己说的。

**理由**：prompt cache 是**字节前缀**（`tools → system → messages`），命中约 0.1×。
`system` 每轮整块重发 ⇒ 动它等于每轮废掉其后全部缓存；`messages` **尾部**注入只让**新追加的那一段**
按 1.0× 付，此后进前缀按 0.1× 命中。这个差额是本期成本结构的全部来源
（`../arch/long-horizon-todo/design.md` §6）。

「找不到标记就补」是**自愈**：压缩（含手敲 `/compact`，那条路径不发 `ContextCompacted` 事件）
把块吃掉后自动补回，不需要监听任何事件。

**代价**：注入块在长回合里会变旧（回合内不刷新）。这是明确接受的退化 —— 回合边界刷新已经
把窗口压到「一个回合」的量级，而回合内重写会把成本从每回合一块抬到每轮一块。

**硬约束**：直接 `append` **不发事件** ⇒ UI 不显示（对比 `Boundary::Steer` 会 emit `UserMessage`，
`crates/ys-loop/src/basic.rs:200`）。这是有意的：注入块是 harness 的内部状态，不是用户说过的话 ——
`Role::Harness` 把这个语义写进了数据类型。

### 3.4 follow-up 两个判据并存，不是替换

**决策**：同一个循环里两个判据，各贡献一个继续条件：

| 判据 | 触发源 | 开关 |
|---|---|---|
| `should_follow_up`（HEAD 不等） | commit 后同步文档 | `follow_up_on_commit`（默认关） |
| `has_open_items()`（有未完成项） | 用户自己的计划 | **无开关**（计划的有无就是开关） |

（ADR-0024 决策 3）

**理由**：两者管的是**不同的触发源**，不是同一件事的两种做法。HEAD 判据为「commit 后同步文档」设，
与 todo 无关；用 `has_open_items()` 去替换它会让那个场景失效。反过来，HEAD 判据对 todo 无效 ——
todo 项的完成不一定伴随 commit。

「无开关」成立的原因是触发前提变了：老设计的触发与用户意图无关（用户没要也得发生），
本设计的触发前提是**用户自己的计划** —— 模型不写计划 ⇒ `has_open_items() == false` ⇒ 永不追问。
**开关从配置项变成了计划的有无。**

**代价**：两个判据共用一个追问计数上限（`MAX_FOLLOW_UPS = 8`）与同一个停滞护栏，
两者的预算互相挤占。用户输入抢占（追问链路每次迭代开头先 `try_recv`）对两者同等生效。

### 3.5 `apply` 的两条硬规则

**决策**：都在 `TodoStore::apply` 里，由代码执行，不靠模型自觉。

| 规则 | 防什么 |
|---|---|
| **拒绝丢弃既有非终态项** | 模型发一个 `[]` 就关掉整个闭环 |
| **无 open 项 ⇒ 清空 + 删文件** | 判据写「全部 Completed」时，被放弃的项永久卡住 |

（ADR-0024 决策 4、ADR-0072 D3）

**理由**：删除判据若写成「全部 `Completed`」，一个被**放弃**的项永久非 `Completed` ⇒ 永不满足 ⇒
永不删除。改成「**无 open 项**（无 `Pending` / `InProgress`）」后，这一条**结构上不可能发生**。
配套必须有终止态 `Cancelled` —— 它是模型能**诚实停下**的唯一出口；没有它，模型想让环停下
就得把没做完的事标成 `completed`，那是伪造完成。

**执行者是代码，收口在 `apply`**：任何写入者（模型经工具、将来的 `/todos` 命令、`/new`）
都不可能留下一个已关闭的计划。这是「不靠模型自觉」的结构保证。

**代价**：模型不能通过「重发一个空表」来重置计划 —— 想放弃必须逐项标 `Cancelled`，
多花几次工具调用。这是有意的摩擦。

### 3.6 判官 `MET` 的三条件缺一不可

**决策**：`MET` 的充分条件由 harness 强制，不靠判官自觉：

1. `VERDICT: MET`；
2. `ITEMS` 里**每个非终态项**都标了 `OK`，且**每项**的引用都能在证据窗口里逐字命中；
3. **至少一条**引用命中。

任一不满足 ⇒ 降级 `NOT_MET` + 计入可靠性计数。（ADR-0072 D5）

**理由**：判官是模型，它的输出是**文本**，可以被措辞绕过。把「是否达成」的最终裁决权留在 harness 手里，
判官只提供「逐条核对的结果 + 引用」，harness 校验引用是否真的存在。这样「模型自称完成」在
机制上无法通关 —— 除非它真的跑出了通过的证据。

`VERDICT` 的解析**必须整词等于** `MET|NOT_MET|BLOCKED`，不做前缀匹配：否则 `METRICALLY`
会被误判成 `MET`，正是最危险的假阳性方向。

**代价**：跨轮完成的项会被保守判为未完成（判官只读最后一轮，D6），靠收尾轮重跑验证缓解。
多一次模型调用 / 轮，且不计入任何预算。

### 3.7 引用校验是本设计唯一强于同类实现的地方

**决策**：harness 在**排除助手自述**的证据窗口里做**逐字子串匹配**（归一化空白后比对），
不命中即降级。（ADR-0072 D5、`../arch/cross-turn-advance/design.md:717`）

**理由**：这是把「判据外部可读」从口号变成机制的那一步。判官可以幻觉，但幻觉出来的引用
不会在证据窗口里逐字命中。两个必要条件同时成立才放行：**判官说 OK** ∧ **引用真的存在**。
证据窗口本身排除了助手正文，所以「我在回复里说我做完了」不构成证据。

**代价**：引用必须 ≥12 字符且逐字摘录 ⇒ 判官要在长输出里做精确摘抄，解析失败率高于
「一句话结论」式的判官。这由可靠性计数兜底（连续 2 次解析失败 / 引用未命中 ⇒ 硬闸门停止）。

### 3.8 stall 用 todo 集合指纹，不用哈希命令输出

**决策**：`stall_digest = DefaultHasher(Σ(归一化内容 ‖ 状态) for 非终态项)`，
归一化链 `折叠空白 → 小写 → 排序 → 去重`。连续 3 轮相同 **且** 模型自称收尾 ⇒ 判无进展。
（ADR-0072 D10、`../arch/cross-turn-advance/design.md:749-772`）

**理由**：哈希命令输出会因「同一测试跑两次耗时不同」而漏检；清单是**离散集合**，不受日志噪声影响。
而且它与推进判据**同源**（推进看 `has_open_items()`，停滞看这些项一字未变），不需要额外采集。
**只算非终态项**：已完成的项不参与，否则模型把已完成项重新描述一遍就重置了 stall。

与「模型自称收尾」取 **AND**：只靠集合不变会误杀「一轮里干了活但还没更新状态」的情形。

**代价**：模型若只重排措辞（内容实际未变）也会被算作「变了」⇒ stall 归零 ⇒ 防套娃失效一次。
归一化链（折叠空白/小写/排序/去重）压掉大部分噪声，但压不掉改写。

### 3.9 `-p` 的继续条件是单一表达式

**决策**：两个入口共用**一份**继续条件，不是「`-p` 被 `--goal` 单独门控」：

```text
continue = todo.has_open_items()          // 清单层（无开关）
         ∨ (goal.is_some() ∧ ¬goal.met)   // 验收层（需显式 --goal / /goal）
         ∨ should_follow_up(head)         // 一期 HEAD 判据（仅 TUI）
```

（ADR-0072 D11、`../arch/cross-turn-advance/design.md:818-836`）

**理由**：ADR-0041 的硬要求是「不该只在一条入口上生效」。一份表达式意味着两条入口的行为
在结构上不可能分叉 —— 而不是靠两处代码「写得一样」。`-p` 默认行为不变：不写清单、不传 `--goal`
⇒ 三项均假 ⇒ 跑完即退出。退出码：`0` = 达成，`2` = 未达成。

**代价**：`-p` 的消费必须与每个回合**并发**（有界信道无并发消费者 ⇒ 第一次撞满即死锁），
且消费循环以**每回合**的终局事件为终止条件 —— 因此消费器必须按 `&mut` 拿 receiver、
逐回合 `join!`，不能整个 run 只 `join!` 一次。

### 3.10 三期不做的事

| 不做 | 理由 | 出处 |
|---|---|---|
| **跨轮证据** | 判官只读最后一轮 ⇒ 输入 O(1)、成本不随轮数线性增长。代价是跨轮完成的项可能被保守判未完成 | D6 |
| **滚动摘要** | 会让判官输入随轮数增长，与 D6 直接冲突 | D6 |
| **目标状态落盘** | 目标是「本回合想达到什么」，过期即失效；落盘 + 恢复等于把「重置预算」开成跑飞口子 | D14 |
| **`--resume` 恢复目标** | 同上。`--resume` 后目标未激活，但 **todo 清单恢复** | D14 |
| **多目标** | 多个目标要处理冲突（两个循环同时到点怎么办）、可见性（状态行显示多条）、排队语义 | `../arch/cross-turn-advance/design.md` §4.7 |
| **通用 hook / 插件系统** | 仍只有零个真实消费者 | ADR-0014、ADR-0021 |
| **子 agent 有 todo** | 同时打破字节前缀与单写者不变量 | `long-horizon-todo/design.md` §7.4 e |
| **判官调工具 / 改状态 / 产生给模型的指令** | 注入面收窄到「判错」 | D4、D8 |

## 4. 改动面

| 文件 | 动作 | 内容 |
|---|---|---|
| `apps/coding-agent/src/toolset.rs` | **不改** | `EXPECTED_ORDER` 保持 11 元（`toolset.rs:42-54`）、`vec!` 不动 —— 它描述**子表**，子表不含 `todo` |
| `apps/coding-agent/src/assembly.rs` | 改 | 父表追加段**末位** push `todo`（在 `subagent_send` 之后，现为 `assembly.rs:219`）；两处既有断言随之更新：父表元数 14→15（`assembly.rs:387`）与末位断言 `real_parent_table_ends_with_the_subagent_tools`（`assembly.rs:436`，现钉 `["subagent_async", "subagent_send"]` 为末两位）；子表 11 元断言不动（`assembly.rs:388`） |
| `adapters/tools-todo/` | **新 crate** | `TodoItem` / `TodoStatus`（含 `Cancelled`）/ `TodoStore`（唯一真值 + 落盘）/ `render_block` / `todo_tool()` |
| `crates/ys-protocol/src/goal.rs` | **新增** | `GoalSpec` / `Verdict` / `ItemVerdict`（纯数据、**无 tokio**；`ys-protocol` 依赖只有 `ys-core` / `ys-event` / `serde`） |
| `crates/ys-protocol/src/request.rs` | 改 | `Request::Goal { objective }` / `Request::GoalOff`（现枚举见 `request.rs:14`） |
| `crates/ys-tui-coding/src/commands.rs` | 改 | `parse_goal`（紧邻 `parse_loop`，`commands.rs:400`） |
| `apps/coding-agent/src/goal.rs` | **新增** | `GoalState` / `Evidence` / `EvidenceBuilder` / `evaluate` / 闸门 / stall 指纹 |
| `apps/coding-agent/src/goal_judge.rs` | **新增** | 判官提示词、请求构造、`VERDICT`/`ITEMS` 解析、**引用校验**、usage 累计 |
| `apps/coding-agent/src/todo_inject.rs` | **新增** | `TODO_MARKER` / `should_inject` |
| `apps/coding-agent/src/wiring.rs` | 改 | 持 `Arc<TodoStore>`；新增只读访问器 `session_ref()`（现只有 `session()` 返回 `SessionSlot`，`wiring.rs:86`；`ports_with_boundary()` 借 `&mut`，`wiring.rs:261`） |
| `apps/coding-agent/src/app_loop.rs` | 改 | `Request::Goal` 处理；回合边界注入；回合结束求值；抑制一期/二期自驱 |
| `apps/coding-agent/src/main.rs` | 改 | `--goal` 旗标；`-p`/`--json` 的目标循环；退出码；消费器改按 `&mut` 拿 receiver（`main.rs:1145` / `main.rs:2194`） |
| `apps/coding-agent/src/prompt.rs` | 改（P1 内） | `tool_snippet` / `tool_guidelines` 改为读 `ToolPlugin.prompts`；guideline 按全局 `slot` 展开 |
| `crates/ys-tool/src/plugin.rs` | **新增**（P1） | `ToolPlugin` / `ToolPrompt` / `GuidelineBlock`（`../arch/hooks-and-plugins/design.md` §5.2） |

**核心 crate 零改动**：`ys-core` / `ys-event` / `ys-loop` / `ys-component` / `ys-runtime` / `ys-session`
均不改，`BasicLoop` 一行不动。注入走 `app_loop` 的 `session.append()`，判官走独立的模型调用，
两者都不需要新的 loop 钩子。

**依赖方向检查**：`adapters/tools-todo` 与 `tools-basic` / `tools-web` 同形，依赖 `ys-tool` + `ys-core` ✅；
`ys-protocol` 新增的是纯数据（无 tokio ✅）；判官代码放 **app 层** —— 它需要 `Wiring` 的 model + session
视图，且无第二个消费者（按 ADR-0014 不进 `ys-*`）。

## 5. 前缀漂移与指纹

工具表是 prompt 的**第一段**，它的字节决定其后全部内容能否命中缓存。本期唯一会动前缀的动作
是 W5 给父表追加 `todo`。

**W5 之后只有 `fp_parent` 变，`fp_tools` 不变。** 原因是两个锚点覆盖的范围不同：

| 锚点 | 覆盖 | 现值 | 位置 |
|---|---|---|---|
| `fp_tools` | **子表** 11 元（基础表） | `0xf017256b30c9d6f7u64` | `apps/coding-agent/src/toolset.rs:726` |
| `fp_parent` | 子表 11 元 + 追加段（`bash_bg` / `subagent_async` / `subagent_send`） | `0x318be4770f32c8e6u64` | `apps/coding-agent/src/toolset.rs:736` |

`todo` 只进追加段 ⇒ 子表字节不动 ⇒ `fp_tools` 必须**逐字节不变**。若 `fp_tools` 变了，
说明改动越界到了基础表（例如顺手改了 `EXPECTED_ORDER` 或某个基础工具的 description），
应回滚而不是抄新值。

**锚点更新纪律**：

1. 只更新 `fp_parent`，**从干净树取值**（`git stash` 后跑测试取实际值），**不是**从跑红的树里抄 ——
   「跑红 → 抄新值」是把真实前缀漂移洗白的唯一窗口。
2. commit message 写明「父表追加 `todo`，前缀 +N B」。
3. 先确认 `apps/coding-agent/src/toolset.rs:684` 那条「父表 JSON 是子表 JSON 字节前缀」的断言仍绿。

W1–W3（`ToolPlugin` 装配缝）是**纯搬家**：搬完两个锚点必须**逐字节不变**，这是 P1 唯一的机械验收信号。
W5 是**预期内漂移**：只有父锚变，且变化量可解释。

## 6. 测试合同

用例 **14 条**（清单层 5 + 验收层 9，其中 1 条是装配缝结构回归），另有**变异注入 10 条**。
用例编号 T1–T14 是**不变量级**的切分；`../arch/cross-turn-advance/design.md` §8 测试矩阵
（`:942-989`，编号 19–29）是**实现级**的完整清单，两者覆盖同一批不变量，此处按不变量归并。
「必测」= 与不变量直接对应，缺了就等于没实现；「可推迟」= 覆盖边缘路径，可在下一轮补。

### 6.1 清单层（5 条）

| # | 用例名 | 断言什么 | 优先级 |
|---|---|---|---|
| T1 | `apply_rejects_dropping_open_items` | `apply([])` 在存在非终态项时返回错误，原表不变（§3.5 规则一） | 必测 |
| T2 | `apply_clears_and_deletes_when_no_open_items` | 全部项终态（含 `Cancelled`）⇒ 表清空 + 文件删除；`Cancelled` 也满足删除条件 | 必测 |
| T3 | `render_block_is_byte_stable` | 同内容两次渲染逐字节相同；**不得含时间戳 / turn 号 / session id**（变异：加时间戳 ⇒ 红） | 必测 |
| T4 | `inject_self_heals_after_compact` | 状态未变但会话里找不到 `TODO_MARKER` ⇒ 仍注入（变异：去掉「找不到就补」⇒ 红） | 必测 |
| T5 | `system_prompt_does_not_depend_on_todo_state` | 有 todo / 无 todo 两种状态下 `system` 逐字节相同（§3.3 的机械保证） | 必测 |

### 6.2 验收层 + 装配缝（9 条）

| # | 用例名 | 断言什么 | 优先级 |
|---|---|---|---|
| T6 | `met_without_evidence_downgrades` | `VERDICT: MET` 但引用不存在于证据窗口 ⇒ 降级 `NOT_MET` + 可靠性计数 +1（§3.6 条件 2/3） | 必测 |
| T7 | `met_with_missing_item_downgrades` | `MET` 但某非终态项标 `MISSING` ⇒ 降级 `NOT_MET`（§3.6 条件 2） | 必测 |
| T8 | `completed_without_evidence_never_passes` | 模型把全部项标 `completed` 但无凭据 ⇒ 不得放行（ADR-0024 天花板） | 必测 |
| T9 | `assistant_text_is_not_evidence` | 助手正文里的「已完成」不出现在证据窗口 ⇒ 自称完成无法通关 | 必测 |
| T10 | `verdict_word_must_match_whole_token` | `VERDICT: METRICALLY` 不得被解析成 `MET`（§3.6 整词匹配） | 必测 |
| T11 | `stall_stops_after_three_identical_sets` | 非终态集合连续 3 轮相同 **且** 模型自称收尾 ⇒ 停；集合变了 ⇒ 归零 | 必测 |
| T12 | `judge_unreliable_gate_stops_after_two` | 连续 2 次解析失败 / 引用未命中 ⇒ 停；调用失败重试 1 次后停 | 必测 |
| T13 | `real_parent_table_keeps_the_child_table_as_its_byte_prefix`（既有，需改 14→15）+ `real_parent_table_ends_with_the_subagent_tools`（既有，需改末位断言） | 子表 11 元、不含 `todo`（`assembly.rs:388` 不动）；父表 15 元、`todo` 在末位、`subagent_send` 退一位；父表 JSON 仍以子表为字节前缀（§3.2） | 必测 |
| T14 | `two_entries_behave_identically` | TUI 与 `-p` 跑同一条用例（第 1 轮 `NOT_MET`、第 2 轮 `MET`）⇒ 回合数 = 2、退出码 0、**不超时**（ADR-0041 硬要求） | 必测 |

### 6.3 变异测试（10 条注入，落 `scripts/goal-mutation.py`）

形态对齐 `scripts/cancellation-mutation.py`：一次跑完全部注入并给汇总表。
**脚本必须带超时** —— 第 10 条忘了改消费器会**挂死**而不是失败。

| # | 注入的 bug | 期望变红的用例 |
|---|---|---|
| M1 | `apply` 允许丢弃非终态项 | `apply_rejects_dropping_open_items` |
| M2 | `render_block` 加时间戳 | `render_block_is_byte_stable` |
| M3 | 去掉「标记丢失就补」 | `inject_self_heals_after_compact` |
| M4 | guideline 按工具 order 展开 | P0 指纹（`fp_tools` / `fp_parent` 都应变） |
| M5 | `narrate=false` 不过滤 `MainOnly` | explore 提示词不含 narrate guideline |
| M6 | `MET` 不校验引用 | `met_without_evidence_downgrades` |
| M7 | `MET` 不校验逐项 | `met_with_missing_item_downgrades` |
| M8 | stall 不计数 | `stall_stops_after_three_identical_sets` |
| M9 | 闸门恒真 | 墙钟闸门用例（48 h 到点 ⇒ 未达成） |
| M10 | 子表含 `todo` | `real_parent_table_keeps_the_child_table_as_its_byte_prefix` |

**本节没有「可推迟」项**：14 条用例全部落在不变量上。真正可推迟的是本节未列的边缘路径
（文件损坏降级、`/new` retarget、`--resume` 后 todo 恢复），它们属于实现期补充，
不构成「这一期做完了没有」的判据。

## 7. 风险与非目标

### 7.1 判官本身不可靠

判官是一个模型，它会幻觉、会格式跑偏、会把「看起来像证据」的东西当成证据。
本设计不假设它可靠，而是**不把最终裁决权交给它**：三条件（§3.6）+ 引用校验（§3.7）让它的输出
必须与 harness 可验证的事实对上；对不上就降级 `NOT_MET`。

**兜底**：可靠性计数 —— 连续 2 次解析失败 / 引用未命中 ⇒ 硬闸门停止（`judge_unreliable_gate_stops_after_two`）。
代价是「判官一直不可靠 ⇒ 目标永远达不成」，但这比「判官一直放行 ⇒ 假阳性」安全：
假阳性（误判达成 ⇒ 过早停止）远比假阴性（误判未满足 ⇒ 多烧几轮钱）严重（ADR-0072 D9）。

### 7.2 前缀漂移归因

`fp_parent` 变化有多种可能来源：W5 的 `todo` 追加（预期内）、某个追加段工具的描述改动（越界）、
`ToolPlugin` 搬家的字节偏差（P1 的 bug）。**三者混在一起时，跑红的值无法自证来源。**

缓解：① 三个动作分在不同波次，每次只跑一次指纹；② W1–W3 搬家要求两个锚点**逐字节不变**，
W5 才允许父锚变；③ 更新锚点必须从干净树取值，并在 commit message 写明变化量。
**未缓解的部分**：如果 W1–W3 与 W5 在同一次构建里跑，指纹变红无法区分是搬家偏差还是预期漂移 ——
这正是波次必须串行、不能合并的原因。

### 7.3 `-p` 与 TUI 行为一致性

ADR-0041 的硬要求是「不该只在一条入口上生效」。两条入口的**形态差异**是结构性的：
TUI 有 `Request` 通道、有状态行、有 follow-up 链路；`-p` 只有一次 `run_single_turn`
（`apps/coding-agent/src/main.rs:2059`）加一个消费器。任何「在 TUI 里顺手加一句」的改动都会造成分叉。

缓解：继续条件是**一份表达式**（§3.9），两个入口共用；`two_entries_behave_identically`（T14）
是回归防线。**已知风险**：T14 只覆盖「`NOT_MET` → `MET`」这一条路径，
不覆盖「stall 停止」「判官不可靠停止」在两条入口上的一致性 —— 这两条只测了 TUI 侧。

### 7.4 非目标

- **通用 hook 系统**（ADR-0014、ADR-0021）：仍只有零个真实消费者。本期的一切机制都落在
  既有的四个位置（Tool / follow-up 判据 / 回合边界 / 独立模型调用），不新增抽象。
- **跨回合输入队列**（ADR-0047）：追问链路的用户输入抢占是 `try_recv`，不是队列。
- **落盘 automation 子系统**：定时唤醒（二期）已有，本期不把它做成可配置的任务编排。

## 8. 与执行计划的关系

**本文件是设计计划** —— 它记录「为什么这样切、边界在哪、改动面多大、怎么验」。

执行波次（W0–W9：前提修正 → `ToolPlugin` 装配缝 → `tools-todo` crate → 父表接入 → 注入 →
验收层 → 两条入口 → 归档）见
[`../exec-plans/2026-11-06-toolplugin-todo-verification.md`](../exec-plans/2026-11-06-toolplugin-todo-verification.md)，
本文不重复其内容。两份文档的分工：本文的 §4 改动面是**文件级**，执行计划的波次是**提交级**
（同波文件集互不相交、单单元 ≤5 文件）；本文的 §6 测试合同是**不变量级**，执行计划把它摊到各波的
T4–T8 里并绑定到具体实现。

阶段编号的历史冲突（三份文档的 P0–P3 互相指不同东西）已在执行计划 §0 统一为 W 编号，
本文一律用 W。
