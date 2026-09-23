# Subagent 派发 — 架构设计

> 产出：`/arch-design`（2026-09-23）。输入：[`context.md`](./context.md)（arch-explore）、[`research.md`](./research.md)（12 个同类软件的补充调研）。
> 证据均带 `文件:行号`，对应本工作区 `master`（`02a3b55`）。**本文所有 token 数字均为量级估算**（估算方法见 §4.3），不是实测。
> 决策记录：[ADR-0022](../../adr/0022-subagent-dispatch.md)。
>
> ⚠️ **成本数字待重算**：本文的固定开销实测值（子 system ~18,100、每轮 ~24,630）是**在 `CLAUDE.md` 仍被加载时**量的。
> 现 `apps/coding-agent/src/prompt.rs::load_project_context` **不再读 `CLAUDE.md`** ⇒ 本仓实测已降为
> **系统提示 4,539 / 每轮固定 ~6,168**。§4.3/§4.4 的派发成本按旧基线推算，**待重算，勿按旧数引用**。

---

## 1. 结论先行

**推荐方案甲（窄能力注入），并吸收方案丙的两处修正 + 补充调研带来的 8 处改进。** 一句话：**给 `ToolContext` 注入一个「把一段任务文本交给一个隔离的子 agent，拿回摘要 + 失败诊断 + 产物路径」的窄能力** —— 形状与既有的 `Summarizer` 逐字同构，是同一个缝的第三个实例。

### 1.1 形态决策

| # | 决策 | 一句话 |
|---|---|---|
| **D1** | **派发能力走窄 trait 注入**，照 `Summarizer` | `ys-tool` 加 `SubagentRunner`（`&str` 进 / `SubagentOutcome` 出）；三跳 `AgentPorts → RuntimeContext → ToolContext`。**不是**「app 侧工具自带 factory」—— 那条会让模型快照在 `/model` 后失效（§5.6 决策 a） |
| **D2** | **子 agent 事件不进父事件流**（`NoopEventSink`） | ⇒ `Source` **不改**、三个消费者**不改**、回合号语义**整个消失**。**本设计在此是少数派**（12 家调研里多数项目都展示子 agent 活动），理由只有答② 的「内容不展示」（§5.6 决策 c） |
| **D3** | **子 tool 表 = 父 tool 表去掉派发工具**，且派发工具**追加在最后** | 结构上不可能递归（零深度参数）；且子 tools JSON 是父的**字节前缀** ⇒ 命中 prompt cache（§4.3）。**调研确认：Gemini（硬禁止、「图深恒为 1」）、Kimi（role 检查）、Goose（两道闸门）三家独立走了同一条路** |
| **D4** | **结果 = 硬截断的摘要 + 失败诊断 + 落盘转录的绝对路径** | 照本仓既有的 **`webfetch` 惯用法**（ADR-0016）。**调研确认这是差异化优势**：9 家里只有 2 家做到「截断 + 指针」的组合 |
| **D5** | **取消：`ChildBoundary` 透传 `is_aborted`、`take()` 恒 `None`** | 只透传探针，**不让子 agent 偷走父的 Steer 消息**。栈上值、每次派发新建 |
| **D6** | ⚠️ **前置阻塞项：工具超时 300s** | `basic.rs:470-475` 用 `bash_timeout.unwrap_or(300)` 包住**每一个** `tool.call`，且超时会 **drop future**。**必须解**（§5.6 决策 g） |
| **D7** | **P0 prefix 指纹测试先行**（用户已定） | 加工具会改前缀字节。指纹须**在加工具之前**量出锚点 |
| **D8** | **`prompt.rs:107` 改条件式派生** | 不再是常量字符串，改为「工具表里有没有派发工具」。父不说、**子说**（对子 agent 而言那句是真话） |
| **D14** | ✅ **子表可写 —— 能力由「创建时给它的工具表」决定** | 不设只读限制。子 agent 能写，是因为它的工具表里有 `write`/`edit`。⇒ **「给什么工具表」是创建期的显式决策**，不是硬编码的裁剪规则 |
| **D17** | ✅ **支持并行派发**：`BasicLoop` Step 9 从「串行（v0）」升级为**条件并行** | 默认**逐字节不变**（新增的 `Tool::is_concurrency_safe()` 默认 `false` ⇒ 既有工具全走串行路径）。派发工具声明 `true` ⇒ 同一批 N 个 `subagent` 调用并发跑。**详见 §5.7** |
| **D18** | ✅ 派发工具名 = **`subagent`** | 12 家里 `subagent`(deepseek-harness) / `Agent`(Claude Code/Kimi) / `invoke_agent`(Gemini) / `task`(opencode) 都有；`subagent` 与「主/子」区分语义最直接，且不与本仓 `task`/`turn` 术语冲突 |
| **D20** | ✅ **「缓存相干」是一等设计要求**：子的固定前缀只由 **(仓库, skills 集合, 模型)** 决定，**启动期算一次并冻结**，不得含任何时间/会话相关量 | DeepSeek 的缓存 TTL 是**数小时~数天**（V4.1 Flash 报 72 小时）⇒ 子前缀是**一次性**成本；破坏它 = 每 72 小时重付 18.1k。**P0 指纹测试（父一份 + 子一份）正是这条的守卫**（§4.4） |
| **D21** | ✅ **捕获 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`**，让成本从**估算**变成**读数** | ⚠️ DeepSeek 的 `usage` 里就有这两个字段，**本仓适配器把它们丢掉了**（`adapters/model-openai-compatible/src/response.rs:5-6`）。本需求的成本主张至今全是估算、且已错三次（1.1k→2,084；520→18,100；28k→51k）—— **真数一直在手边**（§4.4 (d)①） |
| **D19** | ✅ **子 agent 继承完整系统提示骨架**（工具清单 + guidelines + 项目上下文 + skill 段） | 照 deepseek-harness 的 *"the deployment's system prompt stays uniform across parents and children"*。维护面小、子 agent 知道全部仓库约定。⚠️ **代价：每次派发 ~18,100 est token 的子 system，且**必然 miss 缓存**（前缀在 tools 层就断了）—— §4.3 (B) 因此从 ~28k 修正为 ~51k。缓解：`notes.md` 待办 10 修掉软链重复加载 ⇒ 砍半到 9.3k** |

### 1.2 补充调研带来的 8 处改进

> 详见 [`research.md`](./research.md) §4 的采纳清单。**这一轮的调研不是复述，是改了设计。**

| # | 改进 | 依据（同类软件的实证） |
|---|---|---|
| **D9** | **结果契约加 `stop_reason` + `diagnostic`**，且 `stop_reason` **复用 `ys-core::StopReason` 既有词汇** | deepseek-harness 的 `SubagentResult{output, structured?, diagnostic?, stopReason}`；Codex 出错时截到 900 token；opencode #37852「aborted stream 被记成正常结束 ⇒ 子返回空且无错误」 |
| **D10** | ⚠️ **子 agent 的 usage 上卷进 `TurnStats` —— 从「建议」升为「必做」** | **六个项目对同一件事的同一批抱怨**：opencode #45417/#11027、Roo #5376、Goose #7877、Codex #9748、Kimi #2024、Amp 的 HN 吐槽。**这是本功能被骂得最多的一点** |
| **D11** | 派发工具加 **`description`（3-5 词，必填）** 参数，专供工具行显示 | Claude Code / Amp / deepseek-harness 三家都是必填且明写 "for display"。**这是让答②「TUI 简单展示在工作」真正好看的关键** |
| **D12** | 工具描述 + guideline 写死 **规模规则**：简单任务 1 个、复杂才多个；任务描述四要素 | Anthropic 官方博客的量化经验（含「简单查询 spawn 了 50 个 subagent」的反例、三个 subagent 做**完全相同**的搜索的反例） |
| **D13** | 子 `max_rounds` 默认从 8 调到 **16** | peer 默认值：Goose 25 / Gemini 30 / Cline 超时 5 分钟；反例 Goose #11999（太小 ⇒ 子停滞 ⇒ 父误判死循环） |
| **D14** | **只读子 agent 作为决策点提出**（本轮待定） | peer 里 **6/9 有内建只读角色**：Gemini `codebase-investigator`、Kimi `explore`、Amp `oracle`、OpenHands `code-explorer`、opencode `explore`。**本仓无沙箱**，只读是唯一的内建安全边界 |
| **D15** | **「one visibility」+ fail-closed**：工具限制必须同时是「提示词里看不见」与「执行时拒绝」；`model` 解析失败立即 `Err`、未知工具名大声拒绝 | deepseek-harness 的 `toolFilter`（注释原话 *"vanish from the child's prompt AND refuse to execute"*）；反例 Goose #11803（名字打错只 `warn!` ⇒ **静默造出无工具的子 agent**）、Kimi #2408（schema 说的与代码做的不一致） |
| **D16** | **参数名用 `prompt`**（任务正文），不用 `task` | 12 家收敛：**7/12 用 `prompt`**；且 `task` 与本仓「一次 `run_turn`」的术语易混。工具**名**见 D18 |

**一句话**：本轮**不加新概念类别**，只给既有的「窄能力注入」缝加第三个实例；**不给 `AgentEvent` / `Source` / `Envelope` 加任何东西**。补充调研的价值集中在**结果契约、成本上卷、防失控**三处 —— 那正是 12 个项目共同的伤。

---

## 2. 需求与约束（摘自 context.md，此处只列设计相关的）

**需求原话**：「我需要 subagent 的能力，至少能够区分主 agent 和 subagent，主 agent 可以派发 subagent 完成一些任务。」

**用户已澄清的四点**（详见 context.md §用户已澄清）：

| # | 答复 | 设计后果 |
|---|---|---|
| ① | **不能自嵌套**（工具表裁剪） | 零运行时检查、零深度参数 |
| ② | **TUI 简单展示「subagent 在工作」即可，内容不展示**；父 agent 需处理**返回和产物** | 身份只需「工具行」一级；结果要「摘要 + 产物」两段（D4） |
| ③ | **模型可指定**（工具参数） | `model` 可选参数；默认继承当前模型（缓存最暖） |
| ④ | **先补 P0 指纹，再加工具** | P0 是本需求的第 0 刀 |

**硬约束**（违反即设计偏离）：

| 约束 | 来源 |
|---|---|
| ⚠️ **`ys-tool` 不得依赖 `ys-model`**（`ModelRequest.tools: Vec<ToolSpec>` 使 `ys-model → ys-tool`，反向成环） | `crates/ys-model/src/request.rs:3` |
| `ys-core` / `ys-event` / `ys-protocol` 零 tokio | `CLAUDE.md` |
| `ys-tui-coding` 不依赖 `ys-runtime`（编译器强制） | `CLAUDE.md` |
| 不跨动态库边界传 trait object ⇒ pi 的子进程路线出局 | `CLAUDE.md` |
| `ToolResult` 只有 `{content, is_error}`，**无 `details`** | `crates/ys-core/src/tool.rs:14-17`；ADR-0021 决策 2 |
| `BoundarySource` **每回合必须新建**（`Abort` 永久置位） | `CLAUDE.md`；`apps/coding-agent/src/app_loop.rs:122-123` |
| `Agent::run_turn(&mut self)` ⇒ 同实例不可被工具重入，子 agent 必须**另建** `Agent` | `crates/ys-runtime/src/agent.rs:92` |
| 「不设计假想未来需求」 | ADR-0014 |
| 判断链：模型决定何时做 ⇒ **Tool**，不是 Hook | `docs/extension-points.md:16-26` |

---

## 3. 同类软件调研（Step 2）

> **完整版见 [`research.md`](./research.md)**（12 个项目 · 全景对比表 · 逐项目事实 · 六类共同失败模式 · 采纳清单）。本节只留对本设计**直接承重**的部分。

### 3.1 三份最贴近的参照

| 来源 | 关键事实 | 对本设计的意义 |
|---|---|---|
| **Claude Code**（最贴近需求形态） | 工具名 `Agent`（v2.1.63 起由 `Task` 改名），参数 `subagent_type` + `prompt` + **`description`（"for display"）**；子 agent **独立 context window**；**只返回最终一条消息**；UI 一行 `⏺ Task agent finished · 15 tool uses · 52.3k tokens` | 形状直接来自它；⚠️ 但它**丢了细节**（只回最终消息）⇒ 我们用 D4 的「摘要 + 落盘指针」补上 |
| **`tmp/deepseek-harness`**（本地源码，**最有价值**） | `SubagentResult{output, structured?, diagnostic?, stopReason}`，`stopReason = completed\|aborted\|error\|max-tokens\|refusal`；`diagnostic` **硬上限 4096 UTF-8 字节**；`toolFilter` 是 **"one visibility"**（提示词消失 + 执行拒绝）；子专属指令走 **runtime-context 贡献而非 system-prompt 节**（*"so the deployment's system prompt stays uniform across parents and children"*） | **D9 / D15 / D16 的直接来源**；其终局词汇与 `ys-core::StopReason` **几乎一一对应** ⇒ 直接复用不新造 |
| ⚠️ **Claude Code issue #23463** | 7 个并行 subagent 各返回 15–37K 字符 ⇒ **父上下文被撑爆 ⇒ 会话永久卡死、不可恢复**；官方建议的修复是「摘要 + **指向完整输出文件的指针**」 | **D4 的直接依据**。回传必须**硬截断**（在代码里，不在提示词里） |

### 3.2 共识（12 家收敛，给形状提供强证据）

- **不递归**：Gemini **硬禁止**（`if (tool.kind === Kind.Agent) return`，注释 *"We do not allow agents to call other agents. 不需要深度计数器，因为图深恒为 1"*）、Kimi Python **硬 depth-1**（`if role != "root": return ToolError(...)`）、Goose **两道闸门**（工具表里没有 + 入口拒绝）。⇒ **D3 不是「最便宜的解法」，是三家独立走出来的正解。**
- **schema 形状**：`{description: 3-5 词，给用户看, prompt: 完整自包含任务, [model]}` —— **12 家高度一致**；`prompt` 的语义都强调「自包含」（deepseek-harness 原文：*"include everything it needs. It does not share this conversation's context"*）。
- **默认继承模型、可选覆盖**：Codex 工具描述原文 *"inherit your current model by default. **Do not set the `model` field unless the user explicitly asks**"* —— 与本设计答③ 的语义一致。
- **隔离（fresh 而非 fork）**：多数项目走 fresh。Codex V2 默认 `fork_turns:"all"` ⇒ issue **#24704「Forked subagents lose prompt-cache lineage」**（fork 还打掉缓存）。

### 3.3 反例：它们都在同几件事上翻车

> 九个独立项目的 bug 列表**高度重合** ⇒ 这些不是实现瑕疵，是这类功能的**固有陷阱**。完整版见 [`research.md`](./research.md) §3。

| 陷阱 | 代表事故 | 本设计的应对 |
|---|---|---|
| **成本不聚合** | opencode #45417/#11027、Roo #5376、Goose #7877、Codex #9748（*"instantly drains entire Pro plan"*）、Kimi #2024、Amp HN（*"$1000/m EACH"*） | **D10（升为必做）** |
| **失控的孤儿 / 无界递归** | Kimi #2647（孤儿 subagent **烧 14 小时**）、#2615（`TaskStop` 后仍调 LLM **26 分钟**）、opencode #18100（20 层 / 47 会话）、Anthropic「简单查询 50 个 subagent」 | **D3（结构不可能）+ D13（`max_rounds`）+ D6（超时）** |
| **静默失败** | Goose #11803（名字打错 ⇒ 静默造出无工具的子 agent）、Kimi #2408（schema 说无超时、代码 120s）、opencode #37852（aborted 被记成正常 ⇒ 返回空且无错误） | **D15（fail-closed）+ D9（`diagnostic`）**，与本仓 ADR-0001 / ADR-0017 的「不许静默说谎」同构 |
| **结果无界** | Gemini / Roo / Kimi / OpenHands 都**无上限**；Roo #11364（子返回被截断 ⇒ **父不得不自己重读**） | **D4（硬截断 + 指针）** —— 9 家里只有 2 家做到这个组合 |
| **并行写冲突** | Google 自己的博客警告 *"agents overwriting one another"*；OpenHands 的 `tool_concurrency_limit` **默认 1** 且官方标注 *"use at your own risk"* | **不做并行**（与最保守的四家一致） |
| **UI 不可观测** | 多数项目**都**展示子 agent 活动（Codex `/subagents`、Gemini 可折叠组、Kimi TASK BROWSER、opencode 检查器） | ⚠️ **本设计在此是少数派**，唯一理由是答② —— 见 §5.6 决策 c 的偏离记录 |

---

## 4. 三个候选方案（Step 3/4）

### 4.1 三个候选

| | **甲：窄能力注入**（3 subagent 中的 1 与 3 收敛到这条） | **乙：app 侧工具自带 factory** | **丙：循环内一等概念** |
|---|---|---|---|
| 形状 | `ys-tool` 加 `SubagentRunner` 窄 trait；经 `AgentPorts → RuntimeContext → ToolContext` 三跳注入；app 侧实现 | `apps/coding-agent` 建一个持有「造子环境」句柄的 `TaskTool`，直接注册进表；**`ys-*` 零改动** | `BasicLoop` 识别某个工具名 / ContentBlock，自己跑嵌套回合 |
| 新增概念数 | **1**（`SubagentRunner`，与 `Summarizer` 同形同层） | **1**（`TaskTool` 自带的状态束：子 `Agent` + `ModelResolver` + 产物目录） | **1**（但落在 `BasicLoop` 里 —— 最高爆炸半径） |
| `ys-*` 改动 | `ys-tool` +1 trait +1 字段；`ys-component` +1 字段；`ys-runtime` +1 字段；`ys-loop` +1 行 | **0** | `ys-loop` 内改流程 |
| 工具可否住 plugin crate | **可以**（只依赖 `ys-tool`） | 不可以（需要 `Agent`/`Config`） | 不适用 |
| 模型跟随 `/model` | ✅ 每回合从 `ports.model` 借，**永远当前** | ❌ **工具在启动期快照 `Box<dyn Model>`；`/model` 只调 `wiring.set_model`（`wiring.rs:129-131`）、不重建 `Agent`** ⇒ 子 agent 用旧模型 | ✅（循环持 ctx.model） |
| 可测性 | ✅ 假 `SubagentRunner` 即可单测工具本体（照 `ys-loop` 现有的 `ProbeTool` 先例） | ❌ 工具与真 `Agent`/`Config` 绑死 | ❌ 要驱动真循环 |
| 与 `ToolPlugin`（待办②）的关系 | ✅ 天然契合（工具只依赖 `ys-tool` ⇒ 将来一行登记） | ❌ 工具是 app 内部物，与插件缝无关 | ❌ |
| 主要风险 | 3 个 crate 各加 1 个可选字段 | **`/model` 后静默用错模型**（正确性 bug，非性能问题） | 把策略塞进 `BasicLoop`，违反判断链 |
| 判定 | **✅ 推荐** | ❌ 否决（模型失效 + 不可测 + 与插件缝背道而驰） | ❌ 否决（违反 `docs/extension-points.md` 判断链） |

> **关于乙的一个诚实说明**：乙的「零核心改动」表面上很吸引人。但它的成立依赖「工具能拿到**当前**模型」，而这是假的：`capabilities::set_model` 只换 `Wiring` 里的模型，**不重建 `Agent`**（`apps/coding-agent/src/app_loop.rs:161-163` → `capabilities::set_model`）。工具若在启动期快照模型，用户 `/model` 之后派发的子 agent 会**静默用旧模型**。要修就得在每次换模型时重建整个 `Agent` + 工具 —— 那比甲的三跳注入重得多。**「零核心改动」不是免费的，它的代价是把一个正确性问题藏起来。**

### 4.2 对比矩阵（Step 4）

| 维度 | 甲（窄能力注入） | 乙（app 侧 factory 工具） | 丙（循环内概念） |
|---|---|---|---|
| 实现复杂度 | 中（3 处 1 行 + 1 trait） | 中（1 个大模块，无核心改动） | 高（动 `BasicLoop`） |
| 概念数量 | 1 | 1 | 1（但位置最差） |
| 破坏性变更 | 0（全为 `#[non_exhaustive]` 结构的加字段 + trait 默认方法） | 0 | 0 |
| 每轮全局 token 税 | **+165**（同三案，见 §4.3） | +165 | +165 |
| 每次派发 token | **≈28k**（同三案） | ≈28k | ≈28k |
| 前两级 prompt cache | **子 tools 命中父前缀**（省 ~1.4k/次） | 取决于是否同样追加在末位 | 同甲 |
| 模型时效性 | **永远当前** | ❌ 快照失效 | 当前 |
| 可测性 | **高** | 低 | 低 |
| 演进灵活性 | 高（工具可搬进 plugin crate） | 低 | 最低 |
| 风险 | 低 | **中高（静默用错模型）** | 中高（循环结构耦合） |

**唯一真正的分水岭是「模型时效性」与「可测性」**，两案在 token 成本上完全同价。

### 4.3 成本模型（back-of-envelope，量级估算）

> ⚠️ **本节 (B) 的「子首轮 system 必然 miss」只对冷缓存成立 —— 已被 §4.4 修订。** 缓存 TTL 是**数小时~数天**（DeepSeek），而子的前缀是确定性的 ⇒ 热缓存下那 18,114 按 0.1× 计。**主战场从「子 system」变成「子读进来多少 + 子输出了多少」。读 §4.4。**

> ⚠️ **估算方法**：以 `docs/arch/hooks-and-plugins/design.md` §4.2 的**实测基线**为锚 —— 今日 9 工具的每轮固定开销 = 工具表 JSON **6,517 B / 1,629 est token** + system prompt **1,823 B / 455 est token** = **2,084 est token**。下列数字按该基线与常见单价比例（cache 读 0.1×、cache 写 1.25×、输出 ≈4×）推算，**是量级估计不是实测**。

**(A) 全局税（加派发工具，对所有用户每轮生效，与是否派发无关）**

| 项 | 估算 |
|---|---:|
| `subagent` 工具的 spec（按 ≤400 B 预算，照插件设计 P2 的约束） | ~100 tok |
| snippet + guideline（两张手抄表各一行） | ~65 tok |
| `prompt.rs:107` 那句改为条件式派生（净增） | ~30 tok |
| **合计** | **≈ +165 tok/轮（+8%）** |

与插件设计实测的「加一个克制写法的工具 ≈ +173 token/轮」同量级，交叉印证。

**(B) 一次派发（取子回合数 R = 8）**

> ⚠️ **本节已按「子 agent 继承完整系统提示骨架」的裁决（D19）修正过一次。** 初版把子 system 估成 ~520 是照**空 cwd** 量的 —— 实际是 ~18,100（见下），**低估 35 倍**，导致初版「合计 ≈28k」与「固定前缀只占 8%」两个结论都错了。

| 块 | 计费 | token 当量 |
|---|---|---:|
| 子首轮 tools（1,629；子表是父的**字节前缀** ⇒ 大部分命中） | 命中 | ~160 |
| **子首轮 system（≈18,100 —— 见下方拆解）** | **新（必然 miss）** | **~18,100** |
| 子第 2..R 轮（子**自己**的前缀已稳定 ⇒ 命中 + 新消息 ~1.5k） | 命中 + 新 | ~18,200 |
| 子输出（8 × ~400 tok，≈4× 单价） | 输出 | ~12,800 |
| 回传父（摘要封顶 8 KB ≈ 2k + 指针） | 普通 | ~2,000 |
| **合计** | | **≈ 51k** |

**子 system 的 18,100 是怎么来的**（本仓 `cwd` = 仓库根，`build_system_prompt` 会追加 `load_project_context(cwd)`）：

| 组成 | est token | 依据 |
|---|---:|---|
| 项目上下文 · 单份 | 8,830 | 用本仓估算器量 `AGENTS.md`（27,804 B，CJK 3,554 字） |
| × **2（软链重复加载）** | **17,659** | `load_project_context` 按**路径**去重，而 `CLAUDE.md -> AGENTS.md` 是软链 ⇒ 同一份读两次（`prompt.rs:222-241`） |
| + tool 清单 / guidelines / cwd | ~455 | 与实测基线一致 |
| **合计** | **≈ 18,100** | |

**⇒ 三条被这个数字改写的结论**：

1. **子 system 占一次派发的 ~36%，是最大单项** —— 初版说「固定前缀只占 8%」是错的。**省 token 的主战场是子 agent 的 system prompt，不是子回合数**（虽然 `max_rounds` 仍是成本闸门）。
2. ⚠️ **它必然吃不到缓存**：prompt cache 是**字节前缀**匹配，而子表比父表少 `subagent` ⇒ **前缀在 tools 层就断了** ⇒ 其后的 system 一定 miss。（OpenAI 兼容端点是**自动前缀缓存**，没有显式断点可用，故不能靠"在 system 后打断点"绕过。）
3. **`fixed_prompt_tokens` 必须填子表的真值（≈18.1k）**，否则子会话系统性晚触发压缩。以 128k 窗口、默认 0.85 比例算，子 agent 的可用对话预算 ≈ **90k**。
4. ✅ **但同批的兄弟子 agent 之间会互相命中** —— N 个子 agent 的 system prompt 与工具表**逐字节相同**（`build_system_prompt(cwd, 同一份子表, 同一份 catalogue)` + 同样的 9 个 spec），故第 2..N 个走第 1 个写下的缓存。**一次 4 并发的 system 成本 ≈ `18.1k + 3×1.8k ≈ 23.5k`，而非 `4×18.1k = 72k`。**
   ⇒ 这正是 deepseek-harness「deployment prompt 在父子间保持统一」的**意外红利**（它给的理由是维护面，买到的是**兄弟间缓存共享**）。**唯一 caveat**：真并发启动时第 2 个可能在缓存写完前就发出，存在竞态；串行批次的后续派发必然命中。
5. **本需求因此把 `notes.md` 待办 10（软链重复加载）变成前置项** —— 修了它 18.1k 立刻变 **9.3k**（≈ −49%），且**收益大于任何插件缝优化**。**建议与 P0 指纹测试并列为本需求的第 0 刀。**

**（A）全局税（加派发工具，对所有用户每轮生效，与是否派发无关）**

---

### 4.4 缓存经济学（第三版 —— **TTL 决定了结论**）

> §4.3 前后改过两次（520 → 18,100；28k → 51k），两次都是**估算**。这一版引入了此前没有的一个决定性变量：**缓存的存活时间**。

#### (a) 三个 provider 的缓存机制**完全不同**

| Provider | 机制 | 粒度 | **TTL** | 命中折扣 |
|---|---|---|---|---|
| **DeepSeek**（本仓主用） | **磁盘自动前缀缓存**，零配置 | **64 token 存储单元**（<64 不缓存） | **数小时 ~ 数天**（V4.1 Flash 报 **72 小时**） | 10× → 50× → 120×（逐代加大） |
| OpenAI | 自动前缀缓存（内存） | ≥1,024 token 起 | **~5–10 分钟** | ~2× |
| Anthropic | **显式 `cache_control` 断点** | 2,048 / 4,096 | 5 分钟（付费可到 1 小时） | 读 0.1×，**写 1.25× 溢价** |

- DeepSeek 官方原话（**这是本设计字节前缀假设的直接依据**）：*"only requests with identical prefixes (starting from the 0th token) will be considered duplicates. **Partial matches in the middle of the input will not trigger a cache hit.**"* ⇒ D3（派发工具**追加在末位**以最大化共享前缀）方向正确。
- ⚠️ **本仓的适配器只发 OpenAI 兼容请求** ⇒ **发不出 Anthropic 的 `cache_control`**。故在 Anthropic 后端上只能吃到它那层自动缓存（若有）；在 DeepSeek 后端上拿到的是最好的那一档。
- ⚠️ DeepSeek 官方还有一句要记住：*"The cache system **does not guarantee 100% cache hits**."*
- DeepSeek 官方建议 *"Batch similar queries... send them in temporal proximity"* ⇒ **并行派发恰好符合**。

#### (b) 结论：子 agent 的 18.1k **不是每次都付**

子 agent 的前缀 `[tools(9)] [system_child]` 在 **(仓库, skills 集合, 模型)** 固定时是**确定性**的。配上「数小时~数天」的 TTL：

> **每台机器、每个仓库，那 18,114 最多每 72 小时付一次全价。** 兄弟之间、同回合内多次派发、跨回合、跨会话、甚至隔天 —— **全部命中**。

⇒ **§4.3 (B) 的主战场又要换**：固定前缀在热缓存下只剩 ~1,970 当量（0.1×），真正的大头变成：

| 项 | 当量 | 说明 |
|---|---:|---|
| 子 agent **读进来的内容**（10 个文件 × ~2k，**miss 价**） | **~20,000** | 子 agent 的活就是读这些；父刻意没读，所以**不共享任何缓存** |
| 子 agent 的**输出**（8 轮 × ~400，≈4× 价） | **~12,800** | 输出永远不缓存 |
| 子固定前缀（**热缓存** 0.1×） | ~1,970 | 冷缓存时是 ~19,700 |
| 回传父（≤8 KB 摘要） | ~2,000 | |

#### (c) 最诚实的一句话：**subagent 不省 token，它省的是父的上下文占用**

上面的账算下来，一次派发的 token ≈ **父自己干同一件事的 token**（读一样的文件、产出一样的结论）。**多出来的是**：子 agent 的固定前缀（一次性）+ 回传摘要 + 子 agent 可能多走几轮。

⇒ 那它凭什么存在？**因为父的上下文不被那 20k 的 read 结果永久占住。** 这正是本设计 §3.1 说的「subagent 存在的唯一理由：上下文隔离」—— 现在它有了数字：**一次代码勘探型派发，替父省下 ~20k 的**永久**上下文占用**（那 20k 本来会一直留在父的会话里，直到被压缩掉）。

#### (d) 因此杠杆也换了一批

| 杠杆 | 收益 | 备注 |
|---|---|---|
| **① 捕获 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`** | **把「估算」变成「读数」** | ⚠️ **DeepSeek 的 `usage` 里就有这两个字段，而本仓的适配器把它们丢掉了**（只映射 `prompt_tokens`/`completion_tokens`，`adapters/model-openai-compatible/src/response.rs:5-6` → `lib.rs:250-251`）。**本节所有数字都是估算，而真数一直在手边。** |
| **② 待办 10（软链去重）** | 每轮固定前缀 −8,829（miss 价）/ −883（热缓存 0.1×） | ⚠️ **性质要说清**：父的前缀在同一会话内本就是**命中的**，所以热缓存下省的**不是** 8,829 而是 **~883 当量/轮**。但它仍是唯一一条**同时降父与子、且对每轮生效、且不依赖任何缓存假设**的改动 |
| **③ 子 agent 的 `max_rounds`** | 直接砍「输出」那 12,800 | 主要**上界**，不是优化点 |
| **④ 子 agent 的读量** | 砍那 20,000 | 只能靠任务描述约束，**不应加机制** |
| **⑤ 工具 spec 精简**（≤400 B） | 每轮 −（数十~数百） | 它在**前缀**里，对父对子都生效 |

#### (e) 一条反直觉的推论：D19（子 agent 继承完整 system）**几乎不花钱**

按上面的 TTL 结论：那 18,114 是**一次性**的（且按 0.1× 计），而「去掉 skill 段」省的也是**同一个 0.1×**。⇒ **裁决 4 的两个选项，真实差别比我原先说的小一个数量级。** 「全部保留」因此不只是保守 —— 在缓存友好这个前提下它**几乎是免费的**，还换来子 agent 知情。

#### (f) ⚠️ 一个必须警惕的陷阱：`model: Some(小窗口模型)`

**缓存降低的是价格，不是窗口占用。** 子的固定前缀仍然**实实在在占着 19.7k 的上下文**。⇒ 给子 agent 指定一个 32k 窗口的模型，触发点 `0.85 × 32k = 27.2k`，**19.7k 被固定开销吃掉**，子 agent 只剩 ~7.5k 干活的预算 —— **几乎必然立刻触发压缩、然后什么也做不成**。
⇒ **设计后果**：`model` 参数应引导**同等或更大窗口**的模型；且 `fixed_prompt_tokens` 填错（尤其填 0）会让这个陷阱**静默**发生。

---

### 4.5 三条缓存纪律的对照检查

> 输入：用户提供的三条缓存纪律（DeepSeek 系）。逐条对照本仓**现状**与本**设计**，**标 ❌ 的是真的有冲突**。

#### 纪律 1 —— system prompt 与工具定义**绝对稳定**（“工具列表抖动会让命中率 71% → 33%”）

✅ **本仓已经懂这条**，而且写在代码里 —— `crates/ys-tool/src/registry.rs:21-22` 的原文：

> *「`specs` 严格按**输入 `tools` 的顺序**收集 —— 不能由 `HashMap` 的迭代顺序决定，否则顺序逐进程抖动（哈希随机种子），由它派生的提示词清单会让 **prompt cache 前缀失效**。」*

配套的既有护栏：`toolset.rs` 的 `vec!` 字面量、`EXPECTED_ORDER`、`ToolPlugin.order` 必须是**编译期常量**（ADR-0021，明确「不得来自 env / 登录态 / 探测结果」）、skill 目录**按 command 升序**、`工具面恒定` 不变量。

⇒ **P0 指纹测试（`sha256(tools_json)` + `sha256(system_prompt)`）正是这条纪律的守卫。** 用户给的 71%→33% 给了「P0 不可谈判」一个**量化**依据。

⚠️ **但本需求引入了一个新的抖动面**：子 agent 的 `Agent` 是**每次派发新建**的（§5.6 决策 b，为并行所必需），而 `ToolRegistry::build` 会对每个工具**重新调用 `spec()`**（`registry.rs:27-38`）。
- 若任何 `Tool::spec()` 不是**纯函数**，**子 agent 自己的缓存会跨派发抖动**（就变成它自己的 71%→33%）；
- 而且它的 system prompt（启动期冻结）会与它的 `request.tools`（每次重算）**不一致** ⇒ 提示词里列了 A、请求里发给模型的是 B。

⇒ **两条新要求**：① **`Tool::spec()` 必须是纯的**（同一次构建内幂等）；② **子 agent 的 `request.tools` 必须与启动期冻结的 spec 同源** —— 因为 `Box<dyn Tool>` 不可 `Clone`，实际做法是**在启动期把子表建成一个「重复构造器」**，每派发由它产出新实例，并保证 `spec()` 的输出逐字节相同。
⇒ **一条新测试**（P0 之外）：**同一份子工具表构造两次，序列化后逐字节相同**。P0 的 `sha256` 锚的是「跨版本不变」，**锁不住「同版本内运行期抖动」** —— 那是两条不同的测试。

#### 纪律 2 —— 历史**只追加、不修改**

❌ **本仓违背这条，而且是故意的**：压缩走 `ctx.session.replace_all(outcome.messages)`（`crates/ys-loop/src/basic.rs:168-173`；`Session::replace_all` 在 `crates/ys-session/src/lib.rs:37`），把 `[m₁…mₙ]` **改写成** `[摘要, 尾部…]` ⇒ **从第 0 条消息起就完全变样**。

⚠️ **它的代价此前没有被记账**：

| | 代价 |
|---|---|
| **不压缩**（理想） | 每轮只付**增量** —— 前缀 `[tools][system][m₁…mₙ₋₁]` 全部命中，第 N 轮重付的只有 `mₙ`（~2k）。**这就是纪律 2 收益的形状。** |
| **压缩那一轮** | 前缀 `[tools][system]` 仍命中，但其后的 `[摘要 + 保留尾部]` 与旧缓存**从消息层第 0 条起就分叉** ⇒ **整段按 miss 价重付**。⚠️ **实参是 `trigger_ratio = 0.85` / `keep_ratio = 0.10` / `keep_max_ratio = 0.10`**（`crates/ys-component/src/compact.rs:66-77`）⇒ 128k 窗口下保留段只有 **~12.8k** ⇒ **那一轮的 input 从 ~2k 尖峰到 ~12.8k**（额外 ~10.8k）。压缩 k 次 = k 次尖峰，**不是灾难级**。<br>（本行初稿按 `keep_ratio = 0.5` 算成 54k —— **是错的**，`0.10` 的保守保留让压缩的缓存代价小得多。）<br>⚠️ 反过来说：**`keep_ratio` 直接就是「缓存尖峰的高度」与「保留多少上下文」之间的旋钮**。 |

⇒ **这不是缺陷，是 ADR-0017 用「缓存」换「不撞窗」的显式代价**（不压缩就直接撞窗）。但**它没有被记进 ADR-0017 的调参依据** —— 而 `keep_ratio` 直接决定尖峰的**高度**（保留越少，重付越少）。

⇒ ✅ **D21（捕获 `prompt_cache_hit_tokens`/`miss`）能让这个尖峰第一次可见** —— 又一个做 D21 的理由，且它把「压缩该保留多少」从猜测变成读数。

**对本需求的推论**：
- **子的会话应尽量不压缩**（子是一次性、有界的；压缩对它是「一次摘要调用 + 整段重付 + 细节丢失」三合一，而它**没有下一次会话**去享受压缩的收益）⇒ 靠 **`max_rounds` 与任务范围**控制，而不是靠压缩。
- ✅ **父子前缀不同 ⇒ 父压缩不影响子的缓存**（反之亦然）。两者各自独立。

#### 纪律 3 —— 把变化的部分**推后**

✅ **本仓已按这条组织**，而且写在注释里（`apps/coding-agent/src/prompt.rs:17-21`）：

> *「段落在**整条提示的最末尾**（`Current working directory: …` 之后），故**空 `catalogue` 的输出是任何非空 `catalogue` 输出的逐字节前缀** —— 这是「同一台机器上先后两次启动的提示前缀稳定」的可测形式（**严格前缀不变量**）。」*

实际顺序：`角色段 → Available tools → Guidelines → <project_context> → Current working directory → <available_skills>`。**最不稳的 skill 目录在最末** ✓。

⚠️ **一处可议但不应改**：**工具清单在 system 的中部**，所以它一变，其后的 **~18k（guidelines + 项目上下文）会一起失效**。按纪律 3 似乎该把它挪到最后 —— **但不要挪**：工具清单只在**代码发布**时变（`工具面恒定` + `ToolPlugin.order` 编译期常量），而代码发布本来就意味着重启、**缓存全冷**，挪与不挪收益为零。**把这条推理记下来，避免后人「优化」它。**

✅ **本需求不向前缀引入任何时间/会话相关量**（D20 已写成硬要求）。

#### 小结算 —— 直接回答「怎么减 token / 提命中」

| # | 动作 | 性质 |
|---|---|---|
| **1** | **D21：捕获 `prompt_cache_hit_tokens` / `miss`** | **地基** —— 三条纪律的收益全部可被它直接读出；本设计已错三次的根因就是没有真数 |
| **2** | **P0 指纹（父一份 + 子一份）+ 新增「构造两次逐字节相同」** | 纪律 1 的守卫；把「缓存相干」从自觉变成测试 |
| **3** | **待办 10（软链去重）** | 唯一同时降父与子、且对每轮生效、且不依赖任何缓存假设的改动 |
| **4** | **少压缩**：`fixed_prompt_tokens` 填对（填错 ⇒ 压缩**系统性早触发** ⇒ 多付尖峰），长会话选大窗口 | 纪律 2 在本仓的可行形式（它故意不遵守「不修改」） |
| **5** | 派发工具 spec ≤400 B、schema 三字段 | 纪律 1 的顺带（它在前缀里） |

---

### 4.6 主 agent（父）的缓存与 token —— 它才是**每轮都在付**的那个

> §4.4 算的是**一次派发**；本节算的是**每一轮**。父的请求是 `[system 18.1k][tools 1.7k][messages]`，**每一轮重发**。

#### (a) 先说一个被沿用的错误假设：**线上顺序不是 `tools → system`**

`hooks-and-plugins/design.md` §4.3 写的「前缀按 `tools → system → messages` 分级」是 **Anthropic 的文档顺序**。本仓**不是**：适配器把 system prompt 拼成 **`messages[0]`**（`adapters/model-openai-compatible/src/lib.rs:323-326`），而请求体的字段声明序是 `model, messages, tools, stream, …`（`request.rs:6-17`，serde 按声明序输出）。

⇒ **本仓的线上顺序是 `system → tools → messages`。** 这有两个后果：

1. **「加一个工具会不会废掉 18.1k 的 system 缓存」这个问题，答案取决于是哪种顺序** —— 本仓**无法从代码确定**（provider 内部如何 tokenize/排序不在我们手里）。
2. ✅ **无论哪种顺序，本设计都已经把分歧压到最小**（这是一个**稳健性**结论，不是运气）：
   - 子的 tools = 父的 tools **去掉末位** ⇒ 共享前缀最大化（对 tools 敏感的顺序友好）；
   - 子的 system **只差工具清单那一行**（见下）⇒ 分歧点**早**，但只有一行；
   - 子的任务文本在 messages 末尾 ✓。
3. ⚠️ **D21（捕获 cache hit/miss）正好能让这个不确定性变成实测** —— 加工具前后各读一次命中率即可。**这是 D21 的第三个理由。**

#### (b) 每轮只付「增量」—— 稳态下主 agent 的缓存其实很好

追加式历史下，第 N 轮请求 `[…m_{N-1}]` 相对第 N-1 轮只多最后几条 ⇒ **每轮新付的只有 ~2k**。破坏这个稳态的只有四件事：

| # | 事件 | 代价 | 备注 |
|---|---|---|---|
| **1** | **压缩** | 那一轮 input 从 ~2k 尖峰到 ~12.8k（**+10.8k**） | `keep_ratio = 0.10` ⇒ 不灾难；但压缩 k 次 = k 次尖峰 |
| **2** | ⚠️ **`/model` 切换** | **下一次请求全冷**：18.1k + **整段历史**都按 miss 价 | 缓存**按模型分池** ⇒ 换模型 = 换池。`/model` 的这条代价**文档里没写**（`capabilities::set_model` 只报了「已切换」）。**建议在 `/model` 的输出里提示一次。** |
| **3** | **加/改工具** | 至少废掉 tools 那一层（~1.7k）；**是否连带废掉 system 取决于 (a) 的顺序** | 只在**代码发布**时发生 ⇒ 本来就要重启、缓存全冷 ⇒ 实际代价 ≈ 0 |
| **4** | **冷启动**（新进程 / TTL 过期） | 18.1k + 1.7k 全价，一次 | DeepSeek 的缓存是**磁盘级、跨进程**的 ⇒ `-p` 连续跑、重启恢复会话**都是热的** ✓ |

#### (c) ⚠️ 最大的主 agent token 杠杆：**工具输出上限 50 KB，且不落盘**

`crates/ys-tool/src/truncate.rs:22-24`：`MAX_LINES = 2000`、**`MAX_BYTES = 50 * 1024`**。

⇒ **一次 `read` / `grep` / `bash` 的结果最多可以塞 50 KB（≈12.5k token）进父的上下文，而且它是永久的** —— 进了 `messages` 就一直在，直到被压缩掉。

- 十次这样的工具调用 ⇒ **~125k token 的常驻上下文** ⇒ 直接顶到压缩触发点。
- 而 **`webfetch` 已经有了更好的模式**（ADR-0016）：**落盘 + 只回头部 + 给绝对路径**，模型按需 `read offset=`。**同一个仓库里两种截然不同的处理。**
- `truncate` 是「截断的**唯一实现**」⇒ **一处改动即可全局生效**（这是本仓少见的杠杆点）。

**性质说清**：这是**上下文占用**杠杆，**不是**每轮 token 杠杆 —— 那些输出在首次发出后就命中了，不会每轮重付。它真正的代价是**把窗口吃掉 ⇒ 更早压缩 ⇒ 更多尖峰 ⇒ 留给干活的预算更少**。

#### (d) 与 subagent 的关系：**subagent 正是这个杠杆的产品化形式**

(c) 说的是「大块内容不该常驻父上下文」。**subagent 干的就是这件事** —— 把「读 20 个文件」的噪声留在子的上下文里，只把结论带回父。§4.4 (c) 已经算过：一次勘探型派发替父省下 **~20k 的永久上下文**。

⇒ 所以这两个话题是**同一件事的两端**：
- **子 agent** = 把**主动的、任务型**的大块内容挡在外面；
- **(c) 的工具输出上限** = 把**被动的、探索型**的大块内容挡在外面（或至少落盘 + 指针）。

⇒ **建议**：把 (c) 作为**独立的一刀**（它不动 subagent 的任何代码，却对每一轮生效）。若不做，至少要意识到 **subagent 的收益会被 50 KB 的工具输出吃掉**。

#### (e) 主 agent 侧的动作清单（按收益排）

| # | 动作 | 性质 |
|---|---|---|
| **1** | **D21 捕获 cache hit/miss** | 让 (a) 的顺序问题、(b) 的尖峰、(c) 的占用**全部变成读数** |
| **2** | **待办 10（软链去重）** | 每轮固定前缀 −8,829（miss 价）；**不依赖任何缓存假设** |
| **3** | **(c) 工具输出：降上限或推广 `webfetch` 的落盘+指针** | 唯一能压「常驻上下文」的杠杆；`truncate` 是单点 |
| **4** | **`/model` 提示「缓存将全冷」** | 一行文案；避免用户以为切模型是免费的 |
| **5** | **`fixed_prompt_tokens` 保持填真值** | 否则压缩**系统性早触发** ⇒ 多付尖峰（现已由 `main.rs` 从真实 prompt 派生 ✓） |

---

## 5. 推荐方案（甲 + 丙的两处修正）

### 5.1 模块划分与职责

| 模块 | 归属 | 职责 | 改动 |
|---|---|---|---|
| `SubagentRunner` / `SubagentOutcome` | **`ys-tool`**（新模块 `subagent.rs`） | 窄能力 trait：`&str` 进 / 摘要 + 产物路径出。**不得出现 `Model`/`Session`/`Usage`/`AgentEvent`** | 新增 ~50 行 |
| `ToolContext.subagent` | `ys-tool` | 挂载点（第 3 个，前两个是 `boundary` / `summarizer`） | +1 字段 +1 链式方法 |
| `RuntimeContext.subagent` | `ys-component` | 穿透用（照 `boundary` 先例） | +1 字段 |
| `AgentPorts.subagent` | `ys-runtime` | 穿透用（app 只能经此注入） | +1 字段 |
| `BasicLoop` 接线 | `ys-loop` | 在**既有的单点接线处**（`basic.rs:459-474`）多挂一个 | ~1 行 |
| `Tool::timeout()` | `ys-tool` | **默认方法**，per-tool 超时（D6 的解）。**不进 `ToolSpec` ⇒ 不改 prompt 前缀** | +5 行 |
| `app_subagent.rs`（`AppSubagentRunner` + `ChildBoundary` + `SubagentTool`） | **`apps/coding-agent`** | 唯一真活：造子执行环境、跑、落盘、截断 | 新增 ~200 行 |
| `toolset` / `prompt` | `apps/coding-agent` | 父表加第 10 项（**末位**）；子表 = 前 9 项启动期预建；两张手抄表加 `subagent`；`:107` 改条件式 | 改 |
| `AgentEvent` / `Source` / `Envelope` / `ChannelSink` / `ys-tui-coding` / 三个消费者 | — | **零改动** | **0** |

> **`SubagentTool` 先放 `apps/coding-agent/src/subagent.rs`，不建新 crate**：它的唯一消费者是自己；等 `ToolPlugin`（待办②）落地、装配缝成形之后，它天然只需依赖 `ys-tool`，届时搬迁是机械动作。**今天建 crate 是为一个不存在的消费者设计。**

**依赖方向自证（不构成环）**：

```text
ys-core ──► ys-tool（+ subagent.rs 纯 trait）──► ys-component ──► ys-runtime ──► apps/coding-agent
                                                        ▲                              │
                                                        └──── AppSubagentRunner 实现 ──┘
                                                    （注意：ys-tool 侧**没有** Model/Session/Usage）
ys-event ──►（NoopEventSink 复用，零新增）
```

### 5.2 核心接口

```rust
// crates/ys-tool/src/subagent.rs —— 形状照 summarizer.rs：ys-tool 层不得出现 Model/Session
// （`StopReason` / `Usage` 来自 `ys-core`，ys-tool **已经**依赖它 ⇒ 可用）
use ys_core::{StopReason, Usage};

/// 一次派发的结果。
///
/// **摘要已由实现方按预算截断** —— 截断责任在代码，不在提示词。
#[non_exhaustive]
pub struct SubagentOutcome {
    /// 给模型看的摘要（已截断到实现方选定的上限）。
    pub summary: String,
    /// 完整转录的**绝对路径**（落盘）；失败时若已写出部分内容也尽量给。
    pub artifact: Option<String>,
    /// 子 agent 的终止原因。**复用 `ys-core::StopReason`，不新造词汇**（D9）。
    ///
    /// 映射：`Completed` ↔ 正常收尾；`MaxRounds` ↔ 撞轮数上限（**部分结果**）；
    /// `Cancelled` ↔ 被取消；`Truncated` ↔ 输出被预算截断。
    /// 非 `Completed` ⇒ 父侧 `ToolResult{is_error: true}`（ADR-0001 错误结果通道）。
    pub stop_reason: StopReason,
    /// 非 `Completed` 时的**安全失败详情**（provider 报错原文等），**实现方须硬上限截断**。
    pub diagnostic: Option<String>,
    /// 子 agent 累计的 token 用量 —— 供父**上卷进 `TurnStats`**（D10）。
    pub usage: Usage,
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// `model = None` ⇒ 用**当前**模型（缓存最暖、零额外配置）。
    /// `boundary` = 父回合的边界源；实现方**只透传 `is_aborted`，绝不 `take`**。
    ///
    /// `Err` = **产不出结果**（如 `model` 名解析失败）。**绝不静默回退到父模型**（D15）——
    /// 用户以为换了模型其实没换，是本仓明确反对的「静默的谎」。
    /// 子 agent「跑完了但没跑好」（撞 `max_rounds` / 被取消）走 `Ok` + 非 `Completed` 的
    /// `stop_reason`，**不是 `Err`**。
    async fn run(
        &self,
        prompt: &str,
        model: Option<&str>,
        boundary: Option<&dyn BoundarySource>,
    ) -> Result<SubagentOutcome, String>;
}
```

**派发工具的 schema**（D11 / D16；与 12 家的收敛形状一致）：

```jsonc
{
  "description": "string, 必填 — 任务的 3-5 词概括，**给用户看**（TUI 工具行显示这一行）",
  "prompt":      "string, 必填 — 完整自包含的任务正文（子 agent 看不到本对话的上下文）",
  "model":       "string, 可选 — 省略则继承当前模型"
}
```
参数名用 `prompt`（12 家里 7 家如此）；不用 `task` —— 与本仓「一次 `run_turn`」的术语易混。

```rust
// crates/ys-tool/src/context.rs —— 第 3 个挂载点，照 with_summarizer
#[non_exhaustive]
pub struct ToolContext<'a> {
    pub boundary: Option<&'a dyn BoundarySource>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub summarizer: Option<&'a dyn Summarizer>,
    pub subagent: Option<&'a dyn SubagentRunner>,   // ← 新增
}
```

```rust
// crates/ys-tool/src/lib.rs —— D6 的解：per-tool 超时，**不进 ToolSpec ⇒ 不改前缀**
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// 本工具单次调用的超时上限。`None` = 用 `RunLimits` 的默认值。
    ///
    /// 存在理由：`BasicLoop` 今天用 `bash_timeout.unwrap_or(300)` 包住**每一个** `tool.call`
    /// （`basic.rs:470-475`），且超时会 **drop future** —— 一次 subagent 派发若超过 5 分钟
    /// 会被静默杀掉，产物可能根本没落盘。默认实现保证既有实现的**行为逐字节不变**。
    fn timeout(&self) -> Option<Duration> { None }
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}
```

```rust
// apps/coding-agent/src/subagent.rs —— 只透传探针，不偷父的 Steer（方案丙的修正 ①）
/// 子 agent 的边界源：`is_aborted` 透传父的，`take()` **恒 `None`**。
///
/// 为什么不能直接把父的 `boundary` 传下去：`take()` 会**消费**父队列里的消息 ——
/// 用户在子 agent 跑动期间的一次 `Steer` 会被子 agent 吃掉，永远到不了父的轮边界。
/// 子的 `Steer` 需求是 0（没有给子 agent 的插话入口），故这里恒 `None` 才是正确的。
struct ChildBoundary<'a> { parent: Option<&'a dyn BoundarySource> }

impl BoundarySource for ChildBoundary<'_> {
    fn take(&self) -> Option<Boundary> { None }
    fn is_aborted(&self) -> bool { self.parent.is_some_and(|p| p.is_aborted()) }
}
```

### 5.3 关键数据流

```text
【启动期 · 一次 · 之后冻结】                                        ← D3 + 缓存前缀
  建 9 元工具表 ──► child_registry（预建，之后只读借用）
  建 10 元工具表（前 9 逐字不变 + 末尾追加 task）──► parent registry
  build_system_prompt(9) ──► child prompt（含「no subagents」，对子是真话）
  build_system_prompt(10) ──► parent prompt（不含该句）

【每回合 · app_loop】                                              ← D1 注入
  let parent_model = ports.model;                 // 拷出（Copy），拿到的永远是当前模型
  let runner = AppSubagentRunner { config: &config, parent_model, child: &child_agent, dir };
  ports.subagent = Some(&runner);
       └─► Agent::run_turn ──► RuntimeContext.subagent ──► BasicLoop Step 9
                                                        └─► ToolContext::with_subagent(..)

【一次派发】
  模型 ──ToolUse{name:"task", args:{prompt, model?}}──► BasicLoop
       ├─ emit ToolCall{name:"task"}                    ← **TUI 零改动就显示「在工作」**（答②）
       ├─ 超时闸：Tool::timeout()（task 返回一个长的，如 1800s）
       ├─ SubagentTool::call → ctx.subagent.run(prompt, model, ctx.boundary)
       │     ├─ ChildBoundary{parent: ctx.boundary}     ← 只透传 is_aborted
       │     ├─ MemorySession::new()                     ← 零容量分配
       │     ├─ NoopEventSink                            ← 子事件**不入**父信道（D2）
       │     ├─ model = resolve(model?) ?? parent_model  ← 解析失败**立即 Err**，不静默回退
       │     └─ child_agent.run_turn(AgentPorts::new(model, &mut child_session,
       │                                             &mut NoopEventSink, Some(&child_boundary)))
       │            └─ 跑 R 轮（子表 9 个，**无 task** ⇒ 结构上不可能递归）
       └─ 收尾：子会话 → <~/.yushan/subagents/…>.md（**增量落盘**，防超时丢产物）
                摘要硬截断 ≤ 8 KB，尾部附 "[full transcript: <abs path>]"
                → ToolResult{ content, is_error }
       └─ emit ToolResult{name:"task"}                  ← 工具行闭合，「在工作」结束
  下一轮模型**自然看见**这条结果（这就是「父处理返回」的全部形态）
  产物（子写进工作区的文件）本就在文件系统里，父用既有 read/grep 取
```

### 5.4 六个关键决策的取舍

#### a. 派发能力怎么到工具手里 —— **窄 trait 注入**（照 `Summarizer`）

`Summarizer` 存在的理由是「循环够不着 Model」；`SubagentRunner` 存在的理由是**「工具够不着当前 Model」+「`ys-tool` 不得依赖 `ys-model`」**。两条合起来把形状压死：**只能是一个与模型/会话无关的窄接口**（`&str` 进 / 摘要 + 路径出）。

**为什么不是乙（app 侧工具自带 factory）**：见 §4.1 —— 模型的**时效性**。工具快照的模型在 `/model` 之后会失效，而修它比三跳注入更重。**为什么不是丙**：违反判断链（模型决定何时做 ⇒ Tool），且把策略塞进 `BasicLoop`。

**代价如实记录**：3 个 crate 各加 1 个可选字段。`ToolContext` 是 `#[non_exhaustive]`，加字段是**加法式非破坏变更**，且这是第 3 个同形挂载点（先例：`boundary`、`summarizer`），不是新概念类别。

#### b. 子 `Session` / `EventSink` / `Model` 从哪来 —— **全部就地造，零新类型**

| 件 | 取法 | 理由 |
|---|---|---|
| `Session` | `MemorySession::new()` | `Vec::new()`，零容量分配；子会话是派发临时态，不落盘（「产物」那步显式写一次） |
| `EventSink` | `ys_event::NoopEventSink`（**已存在**，`crates/ys-event/src/noop.rs:8`） | 零新类型（D2） |
| `Model` | `model` 参数命中 ⇒ 按名构造；否则 ⇒ `ports.model`（**当前**模型） | 见决策 a；**解析失败立即 `Err`，不静默回退**（D15）—— 否则用户以为换了模型其实没换 |
| `Model` 的连带（✅ 全做） | ① `Config::build_model_for(name)`（`provider_for_model` → 该 provider 的 auth → 建模型）；② ⚠️ **`Config::context_window_for(name)`** —— `context_window()` 读的是 `self.model`（`config.rs:119-127`），**子 agent 换模型却继承父窗口 = 静默错**（32k 模型按 1M 判 ⇒ 不压缩 ⇒ 撞窗）；③ **按模型名缓存实例** —— `OpenAICompatibleModel::new` 内部 `reqwest::Client::new()`（`adapters/model-openai-compatible/src/lib.rs:33-35`），不缓存则每次派发新建连接池 + TLS 握手；④ 未登录 provider 的模型名 ⇒ 立即 `Err` | ⚠️ **代价（不缓解，只记录）**：`model: Some(不同模型)` **使 prompt cache 全失效**，包括兄弟间共享（缓存按模型分池）—— 这是用户显式选择的必然代价，**默认继承时自动消失** |
| `Agent` | **每次派发现建一个**（`AppSubagentRunner` 持有一份「造子 Agent」的能力） | **这是 D17 并行的前提**：`Agent::run_turn` 取 `&mut self`，一个 `Agent` 实例无法被两个并发派发共用。原设计用 `Mutex<Agent>` 会在并行时**退化成串行**。⚠️ 但「每 child 重建 runtime」正是 CodeWhale 自认的坑 —— **本仓不适用**：`ToolRegistry::build` 只是 9 个小结构体的 `HashMap`（`crates/ys-tool/src/registry.rs:25-41`），**微秒级**；CodeWhale 的问题是克隆重 runtime + 每 child 渲染一张卡。子 system prompt 也**启动期预算一次**（`String` clone） |
| `Limits` | 复用父的 `context_window`/`compaction`；**`max_rounds` 单独设（建议 16）**；**`fixed_prompt_tokens` 必须填子表的真值（本仓 ≈18.1k，修待办 10 后 ≈9.3k）** | `max_rounds` 是成本闸门之一。**16 的来源**：peer 默认值 Goose 25 / Gemini 30 / Cline 5 分钟超时；Anthropic 的经验是「简单任务 3–10 次工具调用」⇒ 8 偏紧、30 偏松。⚠️ 反例 Goose #11999：轮数上限**太小**会让子 agent 停滞、父误判成死循环。⚠️ `fixed_prompt_tokens` 填错（尤其填成 0 或父表的值）会让子会话**系统性晚触发压缩** —— 按 §4.3，这个数占了子 agent 可用预算的 ~18% |

> 实现需给 `Config` 加一个 `build_model_for(&self, name: &str)`（现有 `build_model` 是「按 `config.model`」的单例形态，`apps/coding-agent/src/config.rs:237-238`）。

#### c. `Source` 怎么表达身份 —— **不表达；子事件根本不进父事件流**

**这是对 context.md 的一处自我推翻，必须说明白。** context.md 的结论 2 与「后续建议」把「身份」列为**第 1 刀**，理由是「三个消费者都不读 `source` ⇒ 子 agent 的 `RunFinished` 会误关 `is_turning`、让 `--json` 提前收尾」。**那个 bug 是真的**（`crates/ys-tui-coding/src/app.rs:133-144`、`apps/coding-agent/src/main.rs:217-223`）。但它的**触发前提是「子 agent 的事件进了父事件流」** —— 而那个前提本身不成立：

- 需求 ② 明说「**具体内容不用展示**」。⇒ 子事件没有要展示的东西。
- **「不发射」是比「发射后过滤」更强的保证**：今天三个消费者都不读 `source`，若把子 `ModelTextDelta` 灌进同一 channel，`-p` 的 stdout 会**直接吐出子 agent 正文** —— 那是正确性 bug，不是成本问题。
- 按 ADR-0014：「零消费者 ⇒ 不建」。`Source::subagent()` 今天没有消费者。

⇒ **`Source` / `Envelope` / `AgentEvent` / `ChannelSink` / `ys-tui-coding` 全部零改动**；回合号语义（D4）、`is_turning` 误关、`--json` 提前收尾这三件事**整体消失**（不是被修好，是不会发生）。

**「至少能够区分」靠什么满足**：靠**工具名与工具行**。模型发出的是 `ToolUse{name:"task"}`，TUI 渲染成一条普通的 Tool 行（`app.rs:103-119` 既有机制），人一眼看到「一个 task 在跑」；结果行闭合时看到摘要。这与 Claude Code 的 `⏺ Task agent finished · 15 tool uses · 52.3k tokens` 是同一个层级的东西 —— 而答② 正是要这个层级。

**诚实记录的代价**：子 agent 的内部活动**在任何输出通道里都不可见**（`--json` 亦然）。若将来要可见，缝仍在：加 `Source::subagent(id)` + 在**单点**（`app_loop` 转发处）与 `-p`/`--json` 的消费循环各加一次 source 判断。**触发条件写下来**：出现「要在 UI 上看到子 agent 的逐条工具调用」的真实需求时再开。

> ⚠️ **补充调研后的偏离记录**：**本设计在这里是少数派。** 12 个项目里**多数都把子 agent 活动显示出来** —— Codex 有 `/subagents` + Alt+←/→ 切换、Gemini 有 `SubagentProgressDisplay` 与可折叠组、Kimi 有全屏 TASK BROWSER、opencode 有能看到逐条工具调用的检查器浮层、Cline 有 `SubagentStatusRow`。
> 我们偏离的**唯一依据是用户答②**（「具体内容不用展示」）。**这条偏离的代价必须记住**：UI 不可观测性正是「成本不聚合」与「排障困难」的共同根源 —— 在那六个项目的抱怨列表里是同一批用户的同一批抱怨。⇒ 若答② 松动，D2 是**第一个该翻的决策**（缝已留好）。

#### d. 结果怎么回传 —— **硬截断的摘要 + 落盘转录的绝对路径**

三道闸叠加（缺一不可）：

1. **回传封顶**：摘要 ≤ 8 KB（~2k tok），**在代码里硬截断**，不依赖模型自觉。
2. **落盘 + 指针**：完整子转录写 `~/.yushan/subagents/`，回传尾部附绝对路径；父按需 `read offset=` / `grep`（**照 `webfetch` 的 ADR-0016 惯用法**，snippet 已写在 `apps/coding-agent/src/prompt.rs:133-135`）。
   - **保留策略（✅ 裁决）**：照 `fetched` 的先例 —— **启动期 sweep 超过 7 天的文件**（只清 Primary 档，`Override` 档只写不删）。理由是它是**派生物**（源子会话已终止），且**悬空指针是优雅降级**（`read` 失败，模型自会 adapt），不是正确性 bug。
   - ⚠️ **不照 `sessions` 的「永不清理」** —— 那是**既存缺口**（`sessions/` 确实无界），不是本需求该复制到新目录的先例。**本轮不顺手改它**（独立的一刀）。
   - **测试覆盖口**：`YUSHAN_SUBAGENTS_DIR`（照 `YUSHAN_SESSIONS_DIR` / `YUSHAN_FETCHED_DIR` 的惯例）—— 否则测试会写进用户真实的 `~/.yushan/subagents/`，本仓对此有明文警惕。
   - **文件名唯一性**：`ToolContext` 不带 `tool_call_id`/turn ⇒ 并发下自己造 —— 用本仓已有的惯用法：**进程内原子序号 + `process::id()`**（`toolset.rs:180-188` 同一条规矩）。
   - （同类软件在这里用更短的策略：CodeWhale 1 小时 TTL + 256 条上限、Goose 600 秒 TTL —— 但它们管的是「已完成任务的**元数据**」而非转录全文，不可直接比。）
3. **子上限**：`max_rounds` ⇒ 产物本身有界。

**为什么不直接照 Claude Code（只回最终消息）**：那会**永久丢掉细节**。加一个指针的成本是一行字符串，换来的是「父可以按需取回全部」。这正是 #23463 官方建议的形态。

**为什么落盘在 `~/.yushan/` 而不是工作区**：会话与抓取内容都已在 `~/.yushan/`（`sessions/`、`fetched/`）。写进用户仓库会污染其 git 工作区 —— 本仓对此有明文警惕（见 `apps/coding-agent/src/main.rs:437-439` 关于「会话文件落进用户 git 工作区比看得见的污染更危险」）。

**「产物」的尾巴由 runner 保证（✅ 裁决）**：提示词要求子 agent 按 `## Files` 节汇报；**runner 在结果尾部保证一段固定形状的产物节**，子 agent 不报就写 `(none)`。
⇒ 把「这一段在不在」从**子 agent 的自觉**变成 **runner 的保证**：父能可靠区分「没产物」与「忘了报」。**它只保证格式，不保证内容完整** —— 不包装成「内容保证」。
❌ **被否：用 `git status --porcelain` 前后对比做真结构** —— ① 依赖 cwd 是 git 仓库；② **并行时兄弟的改动分不清**（共享工作区）。⇒ **「并行」这个要求顺手关掉了「用工作区差异做结构化产物」这条路**，记为实现代价。

**「产物」的另一半**：子 agent 写进**工作区**的文件，父本来就能用既有 `read`/`grep` 看见（父子 `cwd`/`workspace_root` 同源）。⇒ 要设计的是**父怎么知道去哪找**：在子 agent 的**输入消息**里要求它按 `## Files` 节汇报改了哪些文件。**该格式不需要发明** —— 本仓的压缩摘要已用这一节且承重（`crates/ys-component/src/compact.rs:87,109,154`：「避免重复劳动与误覆盖文件的唯一直接入口」）。**注意放在 messages 而非 system** —— 放 system 就是每轮前缀变动（§4.3 的 52× 代价）。这与 deepseek-harness 的 *"runtime-context contribution rather than a system-prompt section"* 是同一条纪律。

**失败路径（D9 修正）**：子 agent 撞 `max_rounds` / 被取消 ⇒ `Ok(SubagentOutcome{ stop_reason: MaxRounds | Cancelled, summary: 已有部分, artifact: 已落盘路径, diagnostic })` ⇒ 父侧合成 `ToolResult{is_error: true}`（ADR-0001 **错误结果**通道）⇒ 回喂父模型，**父回合不因此失败**（父可自行重试或换路）。
只有「**跑都跑不起来**」（如 `model` 名解析失败）才走 `Err`。
⚠️ **绝不允许**「返回空且无错误」—— opencode #37852 正是该形态（aborted stream 被记成正常结束），Kimi #11364 是它的下游后果（父只好自己重读）。**非 `Completed` 必须带 `diagnostic`。**

#### d-2. 子 agent 的 usage 必须上卷进 `TurnStats`（D10 —— 本轮从「建议」升为「必做」）

**这是本功能在所有同类软件里被骂得最多的一点**：opencode #45417/#11027、Roo #5376、Goose #7877、Codex #9748（*"instantly drains entire Pro plan"*）、Kimi #2024、Amp 的 HN 吐槽（*"$1000/m EACH"*）—— **六个独立项目，同一件事**。

做法（约 10 行）：`SubagentOutcome.usage` 由 app 侧 runner 累加进一个共享计数器，`app_loop` 每回合 drain 进 `TurnStats`。

**理由与本仓既有立场一致**：状态行的 token 计数是**给用户看的成本**，少算就是**静默说谎** —— 与本仓引入 `StopReason::Truncated`（拒绝把截断伪装成 `Completed`）、ADR-0017（压缩失败不再静默失忆）是同一条纪律。**这条不是锦上添花，是拒绝重蹈覆辙。**

#### d-3. 工具描述与 guideline 的内容（D12 —— 直接写死调研到的规模规则）

派发工具的 `description` 与 guideline 必须包含**防失控**的硬规则（来源：Anthropic 官方多 agent 博客的量化经验，见 [`research.md`](./research.md) §2.9）：

- **规模规则**：简单事实查询 **1 个 subagent 就够**（3–10 次工具调用）；直接对比 2–4 个；**只有真正复杂、可分解的任务才多派**。
- **任务描述四要素**（防重复劳动的唯一手段）：**目标 / 输出格式 / 可用工具与来源 / 明确边界**。反例：Anthropic 早期三个 subagent **做完全相同的搜索**，因为只给了 *"research the semiconductor shortage"* 这种短指令。
- **成本提醒**（Amp 官方原话）：*"the subagent has some overhead. It's another inference call. It's another system prompt that needs to be cached and stored."* ⇒ 子 agent **不是免费的并行**。
- **禁止瞎猜**：子 agent 看不到本对话 ⇒ `prompt` 必须自包含。

#### e. 工具表裁成什么 —— **父表去掉派发工具，且派发工具追加在末位**

- **子表 = 父表 − 派发工具**，按名字排除一行（不抄一份列表 ⇒ 随父表自动同步）。保留其余 9 个 —— 需求只要求不能自嵌套，多裁只会让子 agent 更没用。
- 递归是**结构上不可能**：`registry.get("task")` 返回 `None`，走既有的「tool not found」分支（`basic.rs:504-510`）。**零运行时检查、零 depth 参数。**
- **父表把派发工具追加在末位** ⇒ 子 tools JSON 是父的**字节前缀** ⇒ 首轮命中 prompt cache（§4.3 省 ~1.4k 当量/次）。这也与既有约定一致（「内置 9 逐字不变 + 扩展追加」）。
- **不按任务类型动态裁剪工具表**：动态表 = 每次派发重建 registry（**正是 CodeWhale 反例**）+ 破前缀。

> **调研佐证（D3 从「最便宜」升为「正解」）**：Gemini 用**硬编码的同类判断**（`if (tool.kind === Kind.Agent) return`，注释直言 *"不需要深度计数器，因为图深恒为 1"*）、Kimi Python 用 **role 检查**（`if role != "root": return ToolError("Subagents cannot launch other subagents.")`）、Goose 用**两道闸门**（工具表里没有 + 入口拒绝）—— **三家独立走了同一条路**。
> ⚠️ **反例**：Kimi 的 TS 侧改成「靠配置 allowlist 裁剪」后**立刻漏成无界树**（issue #3128：*"no depth guard, custom profiles get Agent tool by default"*）⇒ **「靠配置裁剪」不如「靠结构保证」可靠**。本设计走的是结构保证。

✅ **已裁（D14）：子表保持「父 − 派发工具」，即可写。** 用户原话：「**subagent 可以写，基于我们创建 subagent 时给它的工具**」。
⇒ **原则写死**：**子 agent 的能力 = 创建时给它的工具表** —— 不额外设只读开关，能力完全由那张表决定。子 agent 能写，是因为表里有 `write`/`edit`。
⇒ 附带好处：将来若要只读子 agent（12 家里 6 家有），**不需要新机制** —— 只是换一张表。这也把「加第二个内置角色」变成纯配置。
⚠️ **必须写的护栏**（子表可写 + D17 并行 ⇒ 三个并发子 agent 可以互相覆盖文件）：
- 工具 guideline 里写死 **「并行派发的子 agent 之间，写目标必须不相交」**（Gemini 的 prompt 原文：*"You should NEVER run multiple subagents in a single turn if their abilities mutate the same files or resources"*；Amp 的原文：*"multiple tasks in parallel **iff** their write targets are disjoint"*）。
- **本仓无 branch/worktree、不做沙箱** ⇒ 这条只能靠提示词约束，**记录为已知缺口**（§7.1）。

#### f. 取消怎么传播 —— **`ChildBoundary` 只透传探针**

`take()` 恒 `None`（否则子 agent 会吃掉父的 `Steer` 消息）；`is_aborted()` 透传父的。父 abort ⇒ 子在**下一个轮边界**停（成本 = 每轮一次原子读）；子内部的 `BashTool` 靠同一个 `is_aborted()` 做 100ms 轮询 ⇒ 长命令也能被杀。

`ChildBoundary` 是**栈上值、每次派发新建** ⇒ 满足「`BoundarySource` 每回合必须新建」；它**不向父 `push`**（否则子的 `Abort` 会被误当父的）。子被取消 ⇒ `Err("cancelled")` ⇒ `is_error` 结果（父此刻也在取消，无害）。

> **不违反 ADR-0013 的「每回合新建」**：那条针对的是**跨回合复用**（`Abort` 的 latch 会污染下一个回合）。此处是**同一回合内**父子共享一个只读探针，语义正确。

#### g. ⚠️ 工具超时 —— 本需求的**前置阻塞项**

`BasicLoop` 用 `ctx.limits.bash_timeout.unwrap_or(300)` 包住**每一个** `tool.call`（`crates/ys-loop/src/basic.rs:470-475`），且 `tokio::time::timeout` 超时会 **drop future**。⇒ **一次超过 5 分钟的派发会被静默杀掉，且产物可能根本没落盘。**

这是本设计里最容易被漏掉的一条 —— 它在**循环**里，不在工具里，三个候选方案都各自独立发现或默认了它。

**推荐解（两条叠加）**：

1. **`Tool::timeout()` 默认方法**（§5.2）—— per-tool 超时，`subagent` 返回一个长的（如 1800s）。**默认实现保证既有工具的 `bash_timeout` 行为逐字节不变**；`RunLimits.bash_timeout` 的语义收窄为「未声明超时的工具的上限」。**关键：它不进 `ToolSpec` ⇒ 不改 prompt 前缀。**
2. **增量落盘** —— 子转录边跑边写，超时/崩溃也留下部分产物，回传里给出路径。

**被否的替代**：
- **调大 `RunLimits.bash_timeout`** —— 顺带把 bash 的超时也抬到 30 分钟，一个挂死的命令会挂半小时。钝器，否决。
- **只靠子 `max_rounds` 保证跑得完** —— 无法保证（R 轮 × 每轮耗时不可控），且一旦超时产物全丢。可作为**附加**手段，不能作为**唯一**手段。

### 5.7 并行派发（D17 —— 本轮新增的实质设计）

> **这是本轮唯一需要改 `BasicLoop` 的决策**，且它推翻了初版「不做并行」的结论。按 `../hooks-and-plugins/notes.md` ③ 的准入规则，**此处显式记账**。

#### 为什么不能只在工具里做

两条路：

| | **(A) 循环层并行**（推荐） | **(B) 一个工具调用带任务数组** |
|---|---|---|
| 模型怎么发 | 在**同一条消息**里发 N 个 `subagent` 调用（业界标准形态） | 发 1 个调用，`tasks: [...]` |
| schema | `{description, prompt, model}` —— **与 12 家的收敛形状一致** | 多一层数组，**偏离收敛形状** |
| UI | **N 条工具行**，每条显示自己的 `description` —— 正是答② 要的「看到 subagent 在工作」 | 1 条工具行 |
| 组合性 | 可与 `grep` 等其他工具同批 | 封闭在工具内 |
| 改动面 | `ys-loop` Step 9 | 只改 app 侧工具 |

**(B) 的致命缺陷是它为了「并行」而改掉了那个已经被 12 家验证过的 schema。** 且 (A) 顺带修掉一个既有的已知简化 —— `BasicLoop` Step 9 的注释自称「**串行执行工具调用（v0）**」（`basic.rs:423`）。

#### 机制：两个默认零行为的开关

```rust
// crates/ys-tool/src/lib.rs —— 与 Tool::timeout() 同一种「加默认方法」的加法式改动
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn timeout(&self) -> Option<Duration> { None }
    /// 本工具是否可与其他「同样声明为真」的工具**并发执行**。
    ///
    /// **默认 `false`** ⇒ 既有工具全部保持今天的串行语义，**逐字节不变**。
    /// ⚠️ 这是 `BasicLoop` 并行的**唯一闸门**，不是「建议」而是「许可」：
    /// 声明 `true` 即承诺「并发调用本工具不会与同批其他工具互相破坏」。
    fn is_concurrency_safe(&self) -> bool { false }
    // ✅ 裁决（用户 2026-09-23）：**本轮只有 `subagent` 覆写为 true**；
    //    只读工具（read/grep/find/ls/…）**本轮不动** —— 它们改变的是既有的
    //    批次行为，而闸门已建好，日后翻默认值只是一行 + 一次行为变更评审。
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}
```

```rust
// crates/ys-component/src/limits.rs —— 并发上限
pub max_parallel_tools: usize,   // 建议默认 4（见下）
```

**执行规则（Step 9 的新逻辑）**：

```text
批次 = 本轮 assistant 消息里的全部 ToolUse
若 批次.len() > 1  且  批次中**每一个**工具的 is_concurrency_safe() 都为 true
    ⇒ 并行路径（带信号量限流 ≤ max_parallel_tools）
否则
    ⇒ 今天的串行路径（一字不改）
```

⚠️ **`批次中每一个` 这个条件是承重的**：它保证**既有的一切批次行为逐字节不变**（今天没有任何工具声明 `true`）。唯一会走并行路径的新情况是「同一条消息里 ≥2 个 `subagent`」—— 那是新场景，不存在回归。混合批次（`subagent` + `bash`）自动退回串行。

#### 三个必须处理的技术点

1. **`ctx.events` 是 `&mut dyn EventSink`，不能跨并发任务共享。** ⇒ 事件顺序改为：
   **先串行 emit 全部 `ToolCall`**（这正是「N 条 subagent 行一起出现」想要的观感）→ 并发执行 → **再按原始顺序串行 emit 全部 `ToolResult`**。
   `ContentBlock::ToolResult` 的落盘顺序同样按原始 `tool_call_id` 顺序（ADR-0001 配对完整性）。TUI 侧靠 `tool_index`（`app.rs:103-132`）按 id 回填，**无需改动**。
2. **审批检查必须提前到并发之前**（今天它在循环体内逐条做）—— 串行跑完审批，再并发执行。
3. **每次派发建一个全新子 `Agent`**（见 §5.6 决策 b）—— `Agent::run_turn` 取 `&mut self`，**一个实例无法被两个并发派发共用**。原设计的 `Mutex<Agent>` 会在并行下退化成串行，已删除。**代价已核实为可忽略**：`ToolRegistry::build` 只是 9 个小结构体的 `HashMap`（`crates/ys-tool/src/registry.rs:25-41`），子 system prompt 启动期预算一次（`String` clone）。

#### 上限：默认 **4**，且**排队而非报错**

**放置方式（✅ 裁决）**：**常量 + 字段双轨** —— `ys-loop` 里一个 `pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4` 是**唯一真值**；`RunLimits.max_parallel_tools` 默认取它；`prompt.rs` **读同一个常量**写 guideline。
⇒ 前缀稳定由**单一真值**保证（而不是靠「禁止用字段」），同时字段仍在（测试与高级用法可覆盖，与 `fixed_prompt_tokens` 由 app 填值的既有形制一致）。
⚠️ **诚实的残余不一致**：运行期真改了那个字段（只有测试会这么干），prompt 里的数字会与真实上限不符 —— **可接受且可测**，因为默认路径永不发生。
（注：`is_concurrency_safe()` 只能是 **`Tool` trait 的方法**，不能是 `ToolSpec` 的字段 —— 后者会被序列化进 prompt ⇒ 改它等于改前缀。）

- **数量依据**：Codex `DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION = 4`；Goose `GOOSE_MAX_BACKGROUND_TASKS = 5`；Anthropic 的经验是「lead 一次并行起 **3–5 个**」。
- **行为依据**：Codex 与 Goose 超限都**直接报错**（`AgentLimitReached` / `"Maximum N background tasks already running"`），对模型很不友好。**本仓用信号量排队** —— 超出的调用等前面跑完再开始，模型看到的是「慢一点」而不是「失败」。
- **必须告知模型**：Codex 把并发槽位数写进 prompt（*"There are N available concurrency slots, meaning that up to N agents can be active at once, including you."*）⇒ 本仓的 guideline 也写明槽位数，否则模型会一次发 20 个然后全部排队。
- ⚠️ **槽位数进 prompt ⇒ 它是前缀的一部分**（改它 = 改前缀）。故它是**编译期常量/固定默认值**，不做成运行时可调的用户配置（与 `ToolPlugin.order` 同一条理由）。

#### 并行下的取消、超时、用量

| 关注点 | 行为 |
|---|---|
| **取消** | `Abort` 到达 ⇒ **每个**子 agent 各自通过 `ChildBoundary.is_aborted()` 在**自己的下一个轮边界**停下。批次等全部收摊。父回合以 `Cancelled` 收场 |
| **超时** | 每任务各带 `Tool::timeout()`；批次墙钟 ≈ **max**（不是 sum）—— 这正是并行的收益 |
| **用量** | 每个 `SubagentOutcome.usage` 由 app 侧累加进**共享原子计数器**（并发安全），`app_loop` 每回合 drain（D10） |
| **子事件** | 仍然**不进父事件流**（D2 不变）—— 并行不改变这一点 |

#### 每回合**派发总量**上限（✅ 裁决 —— 补上一条我漏掉的路径）

原设计只有两个上限：子 agent 的 `max_rounds = 16`（**每个子 agent**）与 `max_parallel_tools = 4`（**同一批并发**）。**没有任何东西限制「一个回合总共能派发多少次」**：

```text
父 max_rounds（默认 10）× 每批最多 4 个并行 subagent = 一个回合最多 40 次派发
  × 每次 ~51k token（§4.3）≈ 200 万 token
```

⚠️ **这正是同类软件被骂得最惨的失败模式**：Codex #9748（*"instantly drains entire Pro plan"*）、Anthropic 官方承认早期**简单查询 spawn 了 50 个 subagent**、Amp 的 HN 吐槽（*"$1000/m EACH"*）—— 它们**都有并发上限，都没有总量上限**。

**做法**：**每回合派发次数上限（建议 16）**，超限返回**响亮的 `Err`** ⇒ `is_error` 结果回喂父模型（「本回合派发已达上限」）—— 符合本仓「不许静默」的立场。

**它零机制的原因**：runner 是**每回合构造**的（`app_loop` 里 `let runner = AppSubagentRunner {…}`）⇒ 把计数器做成 runner 的字段（`AtomicU32`，因为 `run(&self)`）就**天然是 per-turn 的**，不需要任何 reset。上限检查放在 **runner 内部**（工具不用知道），值同样走「常量 + 字段双轨」。

⚠️ **16 这个数是拍的，不是算的** —— §4.3 全是量级估计。取值依据是 Anthropic 的经验（简单任务 1 个 / 复杂 >10 个），目标是「正常用法撞不到、狂派撞得到」。

#### 并行的**代价**（必须如实说明）

- **写冲突是真实的**：子表可写（D14）+ 并行 ⇒ N 个子 agent 可能改同一文件。**本仓无 branch/worktree、无沙箱** ⇒ 只能靠提示词约束（§5.6 决策 e 的护栏）。同类软件的并行事故全部集中于此：Google 自己的博客警告 *"agents overwriting one another"*、OpenHands 的并发默认值至今是 **1** 并标注 *"use at your own risk"*。

---

## 6. 分阶段落地

### P0（前置，必做）：prefix 指纹测试

**这是任何 prompt 重构的安全带，且本需求会改前缀**（加工具 + 改 `prompt.rs:107`）。

```rust
/// 工具面的**字节级**回归锚点。变异：改动任一 snippet / 工具顺序 / spec 文案 ⇒ 立刻变红。
#[test]
fn tool_prefix_bytes_are_frozen() {
    let ts = build_toolset(&tmp, offline_web());          // 9 元（P0 时）
    let specs: Vec<_> = ts.tools.iter().map(|t| t.spec()).collect();
    assert_eq!(sha256(&serde_json::to_string(&specs).unwrap()), PREFIX_TOOLS_SHA256);
    assert_eq!(sha256(&build_system_prompt(&tmp, &specs, &empty_catalogue())), PREFIX_PROMPT_SHA256);
}
```

**本需求建议一次锚四值**：父 `sha256(tools_json)` + `sha256(system_prompt)`，**子**同样两项（子提示词现在也是承重前缀）。

**先量 9 工具的锚点 ⇒ 冻结 ⇒ 再加第 10 个工具 ⇒ 重新量 10 工具的锚点**（这是一次**预期的、显式的**前缀漂移）。

### P1：身份以外的「不会出事」保证 —— **本轮不存在**

context.md 原本把「身份」列为第 1 刀。**按 §5.6 决策 c，这一刀被消掉了**：子事件不进父事件流 ⇒ 没有需要区分的事件 ⇒ 无 bug 可修。**这是本设计与 context.md 的最大分歧，请以此处为准。**

### P2：派发（核心）

> 工具名已定为 **`subagent`**（D18）；下文 `subagent` 即指派发工具。

1. `ys-tool`：`subagent.rs`（`SubagentRunner` / `SubagentOutcome`）+ `ToolContext.subagent` + `Tool::timeout()` + **`Tool::is_concurrency_safe()`**（均默认零行为）。
2. `ys-component` / `ys-runtime`：`RuntimeContext.subagent` / `AgentPorts.subagent` / `RunLimits.max_parallel_tools`。
3. `ys-loop`：既有单点接线处多挂一个；Step 9 超时改用 `tool.timeout()`；**Step 9 加条件并行路径**（§5.7 —— 本轮唯一动 `BasicLoop` 的地方，混批/非安全工具一律退回原串行路径）。
4. `apps/coding-agent/src/subagent.rs`：`AppSubagentRunner`（**每次派发建全新子 `Agent`**）+ `ChildBoundary` + `SubagentTool` + 增量落盘。
5. `toolset.rs`：`EXPECTED_ORDER` 加第 10 项（**末位**）；启动期建 child registry；三处 `assert_eq!(len, 9)` 改 10。
6. `prompt.rs`：两张手抄表加 `subagent`（含 **D12 的规模规则 + §5.7 的并发槽位数 + 写目标不相交护栏**）；`:107` 改条件式派生；`:627` 与 `e2e_skills.rs:90` 同步。
7. `Config::build_model_for(name)`（`model` 参数的数据通路）。
8. **usage 上卷**（D10）：`SubagentOutcome.usage` → app 侧**原子累加**（并发安全）→ `app_loop` drain 进 `TurnStats`。
9. **`Tool::spec()` 纯性 + 子表构造两次逐字节相同**（§4.5 纪律 1）：子 `Agent` 每次派发新建 ⇒ `ToolRegistry::build` 会重算 `spec()` —— 这是本需求引入的**新抖动面**。测试与 P0 的 sha256 **是两条不同的测试**（后者只锁跨版本）。
10. **缓存读数（§4.4 (d)① / §4.5）**：`Usage` 加 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`（`#[serde(default)]` ⇒ 加法式、向后兼容），适配器从 `usage` 里解析（`response.rs:5-6` 旁边），进 `TurnStats`/状态行。
   ⇒ **本需求的所有成本主张至今全是估算，且已经错了三次**（1.1k→2,084；520→18,100；28k→51k）。**而真数一直在服务端返回里，本仓把它丢掉了。** 这一项让成本讨论从「论证」变成「读数」，也让 §4.3/§4.4 的每个数字可被证伪。⚠️ 它不改变 JSONL 会话契约（session 存的是 `Message`，不存 `Usage`），只给 `--json` 事件流加两个可选字段。

**验收**：`cargo test --workspace` 全绿；**既有工具的行为逐字节不变**（`is_concurrency_safe()` 默认 `false` ⇒ 一切既有批次仍走原串行路径 —— 这是可测的：同一批 `bash`+`edit` 的事件顺序与今天一致）；子 registry 里派发工具不存在（结构性不可递归的**可测**形式）；派发一轮的端到端测试（MockModel）能拿到摘要 + `stop_reason` + 落盘路径；**子 usage 出现在 `TurnStats` 里**（D10 的回归锚点）；**并发派发 N 个时墙钟 ≈ max 而非 sum**（D17 的回归锚点，用可阻塞的假模型测）。

### P3（条件触发，本轮不做）

| 项 | 触发条件 |
|---|---|
| `Source::subagent(id)` + 三个消费者读 source | 出现「要在 UI 上看到子 agent 逐条工具调用」的真实需求（§5.6 决策 c 的偏离记录） |
| **只读工具也开并发**（`read`/`grep`/`find`/`ls`/`websearch`/`webfetch`） | 闸门已建好，只差一次**显式的行为变更评审**。**准入规则：可证明对共享状态无副作用**（⇒ 只读工具可以；`bash`/`write`/`edit` **永远不行**）。⚠️ 它改变**既有批次**的事件顺序（`C1,R1,C2,R2` → `C1,C2,R1,R2`，TUI 观感 + `--json` 输出顺序），故需先补「单工具批次顺序不变」的测试 |
| fork（子会话继承父历史） | 出现「子 agent 必须先理解本对话才干活」的真实需求。⚠️ Codex #24704：fork **打掉 prompt cache lineage**；且与「隔离是 subagent 存在的唯一理由」相悖 |
| 多角色 agent 定义（frontmatter / 配置，含只读角色） | 出现**第二个**内置角色的真实需求（照 pi 的 `.pi/agents/*.md`、OpenHands 的 `AgentDefinition`）。届时只读角色会自然成为第一个候选 |
| 按任务类型动态裁剪子工具表 | ⚠️ **实测子回合数 R > 5 才回本**（每子轮省 ~312 tok，代价是丢失 ~1.4k 前缀复用）。但**动态表 = 每次派发重建 registry**（CodeWhale 反例）+ 破前缀 ⇒ 门槛应设得很高 |
| 子 agent 的 USD 预算上限 | 本仓无定价表 ⇒ 需先有价格库。OpenHands SDK 的 `max_budget_per_run` 是九家里唯一的先例 |

### 明确不做

- **子 agent 自嵌套 / 深度参数**（答① 已排除；结构上不可能）。
- **子 agent 事件进父事件流**（答②；D2）。
- **roster / focus / composer target / mailbox / 预算 scope**（CodeWhale 量级；答② 明确不要）。
- **子进程路线**（`CLAUDE.md` 禁止跨边界传 trait object）。
- **从子会话历史重放重建状态**（本仓无 branch/fork，语义不成立）。
- **后台 / 游离子 agent（detached）、chain / workflow 模式**（后续形态，本轮做同步、前台、单个）。

---

## 7. 代价与风险

### 7.1 功能退化（必须如实说明）

- **子 agent 内部完全不可观测**：事件流里只有一个派发工具行，看不到它调了几次工具、读了多少文件。答② 接受了这一点（「具体内容不用展示」），代价是**排障困难** —— 只有落盘的转录能看。**缓解**：转录落盘 + 结果里给路径。⚠️ **这是本设计与多数同类软件的偏离**（§5.6 决策 c 的偏离记录）。
- **并行会引入写冲突的真实风险**（D17 已按用户要求做并行）。子表可写（D14）+ 并行 ⇒ N 个子 agent 可能改同一文件。**本仓无 branch/worktree、无沙箱** ⇒ 只能靠提示词约束（§5.6 决策 e 的护栏）。这不是可以靠机制消除的风险 —— 同类软件全部卡在这里（Google 官方博客警告 *"agents overwriting one another"*；OpenHands 的并发默认值至今是 **1** 且标注 *"use at your own risk"*）。**本仓的缓解是「默认并行、但显式告知模型写目标必须不相交」，并记录为已知缺口。**
- **子 usage 上卷是必做项（D10）** —— 不做就是状态行少算，即**静默说谎**。见 §5.6 决策 d-2。
- **子 agent 的提示词是第二份固定开销**，且它自己也要参与压缩 ⇒ `fixed_prompt_tokens` 必须填**子表的值**，否则子会话系统性晚触发压缩、撞窗口。

### 7.2 真正的风险

| 风险 | 说明 | 缓解 | 同类软件的实证 |
|---|---|---|---|
| **R1 工具超时杀派发** | `basic.rs:470-475` 的 300s + drop future | **D6**：`Tool::timeout()` + 增量落盘。**P2 前必须定** | Kimi #2408（schema 说无超时、代码 120s） |
| **R2 父上下文被撑爆** | 子结果过大 | **D4**：代码硬截断（≤8 KB）+ 指针 + `diagnostic` 封顶。**截断不靠提示词** | Claude Code #23463；Roo #11364（截断而无指针 ⇒ 父自己重读） |
| **R3 前缀一次性漂移** | 加工具 + 改 `prompt.rs:107` ⇒ 全部用户缓存失效一轮（≈9k token） | **P0** 让这次漂移**显式**（锚点从 9 工具换到 10 工具） | Codex #24704（fork 打掉 cache lineage） |
| **R4 六处连锁点漂移** | `EXPECTED_ORDER` / 三处 `len` 断言 / 两张手抄表 / `:627` 测试 / `e2e_skills.rs:90` | 手工同步；待办② `ToolPlugin` 落地后此类漂移结构性消失 | — |
| **R5 无沙箱** | 子 agent 的 `edit`/`write` 直落用户工作区 | `CLAUDE.md` 已声明不做安全，**记录为已知**；**D14 的只读选项正是为此** | OpenHands `code-explorer` 的 prompt 硬约束 *"You never create, modify, or delete files."* |
| **R6 abort 只在轮边界生效** | 子 agent 内一次超长 bash 期间按 Ctrl+C 有延迟 | 既有语义（`BashTool` 100ms 轮询缓解），非本需求引入 | — |
| **R7 摘要质量 = 回传质量** | 子被截断时父只拿到 2k，可能重复劳动 | 子的输入里带模板化汇报要求（做了什么 / 结论 / 未完成 / `## Files`），**放 messages 不放 system** | Kimi 的 `SUMMARY_MIN_LENGTH = 200` 重提示（我们不用机制，用提示词） |
| **R10 小窗口模型的性价比陷阱**（§4.4 (f)） | `model: Some(32k 窗口模型)` ⇒ 子的固定前缀 19.7k **实占窗口**，触发点 27.2k 被吃掉，子只剩 ~7.5k ⇒ **必然立刻压缩、什么也做不成** | 工具描述引导「同等或更大窗口」；**`fixed_prompt_tokens` 填错会让它静默发生**。注意：**缓存降的是价格，不是窗口占用** | — |
| **R8 模型欠派发** | 模型可能根本不调这个工具（skills 正文里的「用 subagent」指令曾被忽略） | 工具描述写清适用场景（D12）；不承诺「一定会派」 | Codex #23496（*"Skill instructions to use subagents are ignored unless repeated in the prompt"*） |
| **R9 孤儿 / 失控** | 子 agent 跑超预期 | **D3（结构不可递归）+ D13（`max_rounds`=16）+ D6（超时）** 三道 | Kimi #2647（孤儿**烧 14 小时**）、#2615（stop 后仍跑 **26 分钟**）、opencode #18100（20 层嵌套）、Anthropic「简单查询 50 个」 |

---

## 8. 裁决记录

> 全部由 `/grill-with-docs` 访谈逐条定下（2026-09-23）。**标 ⚠️ 的是访谈中发现并修正的设计错误。**

| # | 问题 | 裁决 | 落在哪 |
|---|---|---|---|
| **1** | 子工具表是否只读？ | **可写。能力 = 创建时给它的工具表** | D14 · §5.6 决策 e |
| **2** | 是否支持并行？ | **支持**（本轮做） | D17 · §5.7 |
| **3** | 派发工具叫什么？ | **`subagent`** | D18 |
| **4** | 子 agent 拿什么 system prompt？ | **全部保留**（工具清单 + guidelines + 项目上下文 + skill 段） | D19 · §4.3 |
| **5** | 并行的闸门开给谁？ | **只开 `subagent`**；只读工具进 P3（附准入规则「可证明无共享副作用」） | §5.7 · §6 P3 |
| **6** | 「模型可指定」做到哪一步？ | **全做**：`build_model_for` + `context_window_for` + 实例缓存 + fail-closed | §5.6 决策 b |
| **7** | 「产物」怎么表达？ | **提示词 + runner 保证尾巴存在**（子不报写 `(none)`） | §5.6 决策 d |
| **8** | 落盘转录的保留策略？ | **7 天 sweep**（照 `fetched`）+ `YUSHAN_SUBAGENTS_DIR` 测试覆盖口 | §5.6 决策 d |
| **9** | 并发上限放哪？ | **常量 + 字段双轨**（`DEFAULT_MAX_PARALLEL_TOOLS` 是唯一真值） | §5.7 |
| **10** | 要不要限制每回合派发总量？ | **要，建议 16** —— runner 持有计数器（**天然 per-turn**） | §5.7 |
| **13** | 主 agent 的缓存与 token | **线上顺序实为 `system → tools → messages`（与被沿用的文档相反）；最大杠杆是 `truncate` 的 50 KB 工具输出上限** | §4.6 |
| **12** | 三条缓存纪律的对照检查 | **纪律 1 已满足（P0 守卫）+ 两条新要求；纪律 2 本仓故意违背（压缩=整段重付尖峰）；纪律 3 已按序组织** | §4.5 |
| **11** | 怎么减 token / 提缓存命中？ | **① 捕获 `prompt_cache_hit_tokens`（把估算变读数）；② 待办 10 软链去重；③「缓存相干」立为一等要求（子前缀启动期冻结）** | D20/D21 · §4.4 |

### ⚠️ 访谈中发现并修正的三处设计错误

1. **成本模型低估 35 倍**（§4.3）：初版把子 agent 的 system prompt 估成 ~520 est token —— 那是照**空 cwd** 量的。本仓 `cwd` 是仓库根，`load_project_context` 会追加项目上下文，而 `CLAUDE.md -> AGENTS.md` 软链被**重复加载** ⇒ 实测 **~18,100**。一次派发因此从 ~28k 修正为 **~51k**；「固定前缀只占 8%」的结论反转成「**子 system 占 36%，是最大单项**」。
   ⇒ **连带把 `notes.md` 待办 10（软链重复加载）从前置项之外拉进前置项**（修了 18.1k → 9.3k，≈ −49%）。
2. **自相矛盾**（§5.7）：同时写了「槽位数进 prompt ⇒ 必须是固定默认值、不做成用户配置」与「`RunLimits.max_parallel_tools` 默认 4」—— 后者就是运行时可配的。已改为**常量 + 字段双轨**（裁决 9）。
3. **漏掉一条成本爆炸路径**（§5.7）：原设计只有「每个子 agent 的 `max_rounds`」与「同批并发上限」，**没有回合内的派发总量上限** ⇒ 上界是 40 次/回合 ≈ 200 万 token。已补每回合派发计数（裁决 10）。

### 访谈中发现但**由实现方自行决定**的小项（不占用决策）

- **`-p` / `--json` 也要构造 runner** —— 它们走 `main.rs::run_single_turn`、不经过 `app_loop`。**两处都要构造**，否则 `-p` 下派发工具在表里却不可用。
- **子 agent 的第二道递归闸门** —— 除「子注册表无 `subagent`」外，子的 `AgentPorts.subagent` 显式设 `None`（Goose 的「看不见」+「调不动」两道闸门）。
- **子 agent 的输入消息要包装** —— 「你是被派来的 / 看不到父对话 / 按 `## Files` 汇报」这几条走**输入消息**而不是 system（深挖的判据见 §5.6 决策 d）。
- **混批退回串行时要在 guideline 里告知模型** —— 「派发多个 subagent 时让它们**单独占一条消息**」，否则模型会把它和 `grep` 混在一起、静默失去并行。
- **`fixed_prompt_tokens` 的双份账** —— 父用父的、子用子的（≈18.1k），二者都必须在 P0 指纹测试里各锚一份。

**仍然开放**（不再是本轮阻塞项）：

| 项 | 触发条件 |
|---|---|
| `Source::subagent(id)` + 三消费者读 source | 「要在 UI 上看到子 agent 逐条工具调用」（§5.6 决策 c 的偏离记录） |
| 只读子 agent | 有需要时**换一张表即可**（D14 的原则使其零成本） |
| 只读工具也开并发 | 见 §6 P3（准入规则已在） |
| fork / 后台游离 / 多角色定义 / USD 预算 | 见 §6 P3 |

---

## 9. 参考

- **同类软件调研（12 项目完整版）**：[`research.md`](./research.md) —— 全景对比表 · 逐项目事实 · **六类共同失败模式** · 采纳清单
- 上下文：[`context.md`](./context.md)
- 决策：[`../../adr/0022-subagent-dispatch.md`](../../adr/0022-subagent-dispatch.md)
- 直接交汇（P0 与待办②）：[`../hooks-and-plugins/design.md`](../hooks-and-plugins/design.md) §4.2（实测基线）、§6 P0（指纹测试写法）；[`../hooks-and-plugins/notes.md`](../hooks-and-plugins/notes.md) ⑤ 待办 ①②
- 判据：[`../../extension-points.md`](../../extension-points.md)、[ADR-0014](../../adr/0014-follow-up-without-hook-system.md)
- 形状先例：`crates/ys-tool/src/summarizer.rs`（窄能力）、`crates/ys-tool/src/approval.rs`（同形）
- 落盘惯用法：[ADR-0016](../../adr/0016-web-tools-design.md)（`webfetch` 落盘 + 指针）、`apps/coding-agent/src/prompt.rs:133-135`
- 产物清单格式先例：`crates/ys-component/src/compact.rs:87,109,154`（`## Files` 节）
- 失败双通道：[ADR-0001](../../adr/0001-tool-failure-dual-channel.md)
- **本地参照实现（未入版本库）**：`tmp/deepseek-harness/packages/subagent/`（**最有价值**）、`tmp/pi`、`tmp/Codewhale`
- **外部参照**：Anthropic `anthropic.com/engineering/multi-agent-research-system`（量化经验与规模规则）；Aider `aider.chat/2024/09/26/architect.html`（architect/editor + benchmark）；GitHub issue `anthropics/claude-code#23463`（子结果撑爆父上下文）
