# Subagent 派发 — 同类软件调研（补充）

> 产出：`/arch-design` Step 2 的第二轮（2026-09-23），承接 [`design.md`](./design.md) 的初版调研。
> 目的：**用更广的同类软件样本检验并改进设计**，而不是重述已知。
> 方法：4 个并行调研 agent，覆盖 12 个项目；本地参照实现直读源码（`tmp/deepseek-harness`、`tmp/pi`、`tmp/Codewhale`），外部项目走 GitHub 源码 + 官方文档。
> **置信度标注**：标 `[源]` 的直接读过源码/原文；标 `[述]` 的来自二手转述（多为 issue 号），未逐条复核。

---

## 0. 一句话结论

**12 个项目里，11 个有 subagent。唯独本仓没有。** 而它们的**字段级收敛度极高** —— 工具参数几乎都是 `{description, prompt, …}` 三元组，结果几乎都是「最后一条消息 + 有界」，隔离几乎都是 fresh（非 fork）。这给本仓的 schema 形状提供了强证据。

但更重要的是**它们共同的失败模式**，那才是本轮真正吸收的东西（§3）。

---

## 1. 全景对比表

| 维度 | **Codex CLI** `[源]` | **Gemini CLI** `[源]` | **Kimi** `[源]` | **deepseek-harness** `[源]` | **opencode** `[源]` | **Roo/Cline** `[源]` | **OpenHands SDK** `[源]` | **Goose** `[源]` | **Amp** `[述]` |
|---|---|---|---|---|---|---|---|---|---|
| 派发工具 | `spawn_agent`(+5) | `invoke_agent` | `Agent` | `subagent`（名可配） | `task` | `new_task` / `spawn_agent` | `task` | `delegate` | `Task` |
| 主/子区分 | `SessionSource::SubAgent` | `kind:'local'` | `Runtime.role` | `origin:'subagent'` + `delegationDepth` | 配置 `mode` 三值 | 运行时 `parentTaskId` | metadata `is_delegate` | `is_subagent` + `SessionType` | `parentThreadID` |
| 上下文 | 独立 thread；V2 fork 默认 **all** | 完全隔离 | 完全隔离 | 独立 Session；spawn 零 / fork 整父前缀 | 完全 fresh | 激进隔离（父被销毁） | 完全隔离 | fresh | fresh |
| 返回 | 异步注入；完成不截断，**出错截 900 tok** | 一条 tool result，**无上限** | `[summary]`，**下限 200 字** | 终局文本 + `diagnostic`(4096B) | XML 包 `<task state=…>` | 伪装成普通 tool_result | `TaskObservation(status, text)` | `return_last_only` | 最终摘要 |
| 模型 | 默认继承；override 门控 | frontmatter `model`（默认 inherit） | param / default / 父 | 继承父 header；opt-in 可选 | agent `model`；**调用时不可选**(#6651) | per-mode profile | `model: "inherit"` | 优先级链 | 按角色钉 |
| 工具限制 | **全量继承**，只能按 role 禁用 | 默认全量；opt-in | allowlist（3 内建） | `toolFilter` = 可见+授权同一次 | permission 规则集 | `groups` + fileRegex | `tools:` allowlist + `permission_mode` | extensions allowlist | `tools` glob |
| 只读角色 | ❌ 无 | ✅ `codebase-investigator` | ✅ `explore` / `plan` | 可配 | ✅ `explore` / `plan` | ✅ `architect` | ✅ `code-explorer` | ❌（仅提示词） | ✅ `oracle` |
| 递归 | V1 限 1；**V2 不限** | **硬禁止** | Python **硬 depth-1** | `maxDepth` 默认 3 | `subagent_depth` 默认 1 | 无限制 | 无限制 | **硬限 1（两闸门）** | 未知 |
| 并行 | V2 硬上限 **4** | 无上限 | 后台 **4** | 无上限 | 无上限 | **不能**（单开不变量） | 默认 **1**（实验性） | 后台 **5** | 有宣传，无上限 |
| 取消 | `interrupt_agent`；**不自动传播** | AbortSignal + 墙钟 | asyncio / AbortController | 单一 signal；**admission 后所有权转移** | abort 传播 | 无（历史重灾区） | **无** | token 传播 | turn 级 |
| 提示词 | 继承 base + role 追加 | 主追加 / 子**替换** | 追加 `ROLE_ADDITIONAL` | **追加**（runtime-context） | 替换 provider 人格层 | 追加 | **追加** suffix | 替换 | 替换/追加 |

---

## 2. 逐项目关键事实（只列对设计有影响的）

### 2.1 `tmp/deepseek-harness` —— **本轮最有价值的参照**（本机源码，逐文件读过）

TS monorepo，`packages/subagent/` 下 10 个包。它的抽象**与本仓的架构语言几乎同构**：

| 它的概念 | 本仓对应物 |
|---|---|
| Service Definition（`ctx.subagents` 注册表） | 能力面（`ys-protocol` 的定位） |
| Provider（`spawn` / `fork` / `acp` / `codex` / `claude-code`） | 静态组合优先的 crate 边界 |
| Consumer（`dsh-tool-subagent`） | `ys-tool::Tool` 实现 |
| `SubagentCapabilities` **静态 descriptor，启动前校验，缺能力大声拒绝** | 「工具面恒定」+ fail-fast 的既有纪律 |

**四条可直接采纳的具体设计**：

1. **`SubagentResult { output, structured?, diagnostic?, stopReason }`**，其中
   `SubagentStopReason = completed | aborted | error | max-tokens | refusal`
   —— **这套终局词汇与本仓 `ys-core::StopReason`（`Completed / MaxRounds / Cancelled / Truncated`）几乎一一对应**，可直接复用而不是新造。
   `diagnostic` 是**非 completed 时的安全失败详情，硬上限 4096 UTF-8 字节**。
2. **`toolFilter` 是「one visibility」**：注释原话 *"the named tools vanish from the child's prompt AND refuse to execute"* —— 可见性与授权是**同一次操作**，外加**大声的未知工具名校验**。（其 `packages/AGENTS.md` 有硬规则：*"Schema omission, prompt filtering, facades, wrappers, and listener order are **not** enforcement"*。）
3. **子 agent 的 system prompt 与父保持一致，子专属指令走 runtime-context 贡献**：注释原话 *"A runtime-context contribution rather than a system-prompt section, **so the deployment's system prompt stays uniform across parents and children**"*。
4. **fork provider 明确拒绝模型覆盖，为保 KV cache 复用**：*"they inherit the parent's provider and model to keep the copied conversation prefix eligible for KV Cache reuse"*。—— 与本设计 §4.3 的「子 tools 是父的字节前缀 ⇒ 命中」是**同一个直觉的正反两面**。

**它自己写的 Known Limitations**（诚实得罕见，值得记录）：无跨进程 lease；`list_agents` 无分页无删除；accepted-but-unlogged 消息不重放；取消收敛期有唤醒缺口。

### 2.2 Codex CLI（Rust）—— 与本文技术栈最接近，但**教训多于可取**

- **两代实现并存**：`MultiAgentVersion::V1 | V2`，两套工具命名空间（`multi_agent_v1` / `collaboration`）。后果：`agent_max_depth` **有一半被忽略** —— 有显式测试 `multi_agent_v2_spawn_agent_ignores_configured_max_depth`。**「一个工具面，一次造对」的反面教材。**
- **子 agent 全量继承父工具，无内建只读角色**：`apply_role_to_config_inner` **只能禁用** feature，白名单仅 5 项。⇒ 纯研究型委派也带着 `ShellTool`。**与 Gemini/Kimi/OpenHands/Amp 的经营共识相反。**
- **V2 默认 `fork_turns: "all"`** ⇒ issue **#24704「Forked subagents lose prompt-cache lineage」**。**fork 默认值打掉了缓存。**
- 并发硬上限 4，**超限报错不排队**；V2 完全忽略深度配置。
- UI 值得看：`/subagents` + Alt+←/→ 切换 pane；transcript 实录形如
  `• Spawned Robie [explorer] (gpt-5 high)` / `• Finished waiting └ Robie [explorer]: Completed - 39916800`。
- **成本**：issue **#9748「Launching concurrent subagents instantly drains entire Pro plan usage quota」**。
- 模型：工具描述明确写 *"inherit your current model by default. **Do not set the `model` field unless the user explicitly asks**"* —— 与本设计的「默认继承、可选指定」一致。

### 2.3 Gemini CLI —— **「无深度计数器，因为图深恒为 1」**

`local-executor.ts:192-197`：
```ts
// We do not allow agents to call other agents.
if (tool.kind === Kind.Agent) { return; }
```
**即使 `tools: ['*']`，子 agent 也看不到 `invoke_agent`。** ⇒ **这正是本设计 D3「子表 = 父表去掉派发工具」的同一条路，佐证 D3 是最小且正确的递归解法。**

其它：`max_turns` 默认 **30**、`timeout_mins` 默认 **10**；失败也返回**结果字符串**而非 error（`"Subagent 'X' failed. Error: …"`）；`output.result` **无长度上限**。

**Google 自己的博客警告**：*"agents overwriting one another. Parallel subagents will also lead to usage limits being hit faster."* ⇒ **并行写冲突 + 配额**，本仓无 branch/worktree，这条尤其贴脸。

### 2.4 Kimi CLI —— 事故密度最高，**最该学习它的教训**

两个仓库（`kimi-cli` Python 已归档 → `kimi-code` TS 现役），**136 / 425 个 subagent issue**。

- **硬 depth-1**（Python）：`if self._runtime.role != "root": return ToolError("Subagents cannot launch other subagents.")` —— **一行 role 检查**。
- **但 TS 侧靠 allowlist，漏成了无界树**（issue **#3128**：*"no depth guard, custom profiles get Agent tool by default"*）⇒ **「靠配置裁剪」不如「靠代码硬判」可靠**，两条都要。
- ⚠️ **孤儿 agent 烧额度**：**#2647** 一个终止的 403 派生 detached subagent 重试循环**跑了 14 小时**烧光 quota；**#2615** 后台 subagent 在 `TaskStop`/timeout 标记终态后**仍继续调 LLM 约 26 分钟**。
- **#2408** 前台 `timeout` **实际默认 120s**，而 schema 宣称 "no default timeout" ⇒ **schema 说的与代码做的不一致**。
- **#1931** subagent **不继承父 cwd**（破坏 git worktree 流程）。
- **#2024** subagent 的 context 用量不在父统计里可见。
- **返回有下限**：`SUMMARY_MIN_LENGTH = 200` —— 短于 200 就用 continuation prompt **重提示一次**要求更详细。**这是唯一的「结果质量下限」机制。**

### 2.5 opencode —— 「`mode` 一个字段回答两个问题」

- **`mode: "primary" | "subagent" | "all"`** 一个枚举同时回答「能不能当主 agent」「能不能被派发」。**拆成两个 bool 会有 4 种组合、其中 2 种无意义。**
- 内建 subagent：`general`（全工具除 todo）、**`explore`（只读）**、`scout`（只读）。
- **`permission.task` 用 glob 控制能派哪些子 agent，被 deny 的从 Task 工具描述里整条删掉 —— 模型根本看不到。** 与本仓 `<available_skills>` 的渐进式披露**完全同构**。
- **子权限推导**：*"父的 deny 规则 + `external_directory` 规则往下传，**父的 allow 不传**"*。
- **`subagent_depth` 默认 1** —— 是 issue **#18100（20 层嵌套 / 47 个会话）** 修出来的。
- 返回包 XML：`<task id="<子会话ID>" state="…"><summary/><task_result/></task>`，**带子会话 id 可作 `task_id` 回访**。
- **所有**工具输出统一过 `truncate.output`（2000 行 / 50KB），超限落盘 + 回喂 *"Do NOT read the full file yourself - delegate to save context"*。
- ⚠️ **无并发上限**（issue **#27110**：本地模型 5 并发把 5 分钟变 45 分钟）；**成本不含子 agent**（**#45417 / #11027**）。

### 2.6 Roo Code / Cline —— **反面参照，事故最密集**

- Roo 为「任何时刻最多 1 个打开的 Task」，把**父整个 dispose 再从磁盘重建**（代码里全是 `"retrying…"` / `"CRITICAL: … not persisted to disk"` / `"Non-fatal: proceed anyway"` 这类补丁痕迹）。后果：**#2534** 无法恢复、**#4881** resume 重跑重复计费、**#1547** 子不回父、**#5329/#4896** 取消破坏流程。
  ⇒ **本仓 `Agent` 是无状态执行器、session 归 `Wiring` 持有，根本没有这个约束 —— 不要引入它。**
- **返回无长度上限**（代码里没有截断）⇒ **#11364「子 agent 读文档经常返回被截断的内容，逼父任务自己重读」**。
- Cline 现状是**四套并存**（`new_task` / `subagent_*` / `use_subagents` / 18 个 `team_*` 工具）；**审批策略显式不继承**（*"Do not inherit approval policy"*）。
- **#5376**：子任务成本不聚合、层级可视化差。

### 2.7 OpenHands / Goose / Amp

- **OpenHands**：V0 的 `AgentDelegateAction` 是**死代码**（工具 schema 从未注册，issue #8187/#10542 `closed as not planned`）—— **「handler 写了但 schema 忘了注册」的经典事故**。现役 SDK 版才有：`TaskAction { prompt, subagent_type, description, resume }`。
  SDK 的 `AgentDefinition` 有 **`max_budget_per_run`（USD）** —— **九个项目里唯一的第一类成本上限**；`permission_mode: None = 继承父`。
  预设子 agent：`code-explorer`（tools 只有 terminal，prompt 硬约束 *"You never create, modify, or delete files."*）。
  **两代都无递归深度限制**；**两代都无取消**。
- **Goose**：**硬限深度 1，且是两道闸门** ——
  ```rust
  if !is_subagent { tools.push(create_delegate_tool()); }        // 看不见
  if session.session_type == SubAgent { return Err("…"); }        // 调不动
  ```
  「一道在看得见，一道在调得动」，零配置零计数器。`DEFAULT_SUBAGENT_MAX_TURNS = 25`；后台并发默认 **5**。
  ⚠️ **子强制 `GooseMode::Auto`** 是为绕过「子的审批需求无法回传父」的妥协，源码注释直言 *"any mode that requires approval will hang"* ⇒ open bug **#11778 审批绕过**。**不要照抄。**
  ⚠️ **#11803**：扩展名打错只 `warn!` ⇒ **静默造出一个无工具的子 agent**（修复 PR #12430 改为 fail-closed）。**这是「静默失忆」类 bug。**
  ⚠️ **#7877**：用户看不到子的花费。
- **Amp**：**闭源**，成本抱怨最凶（HN：*"$1000/m EACH"*、*"they all went by so fast"*）。官方播客承认权衡：*"It shouldn't use a subagent for this because the subagent has some overhead. It's another inference call. It's another system prompt that needs to be cached and stored."*
  有 **`oracle`（只读第二意见角色）**，官方说明 *"We intentionally do not force the main agent to always use the oracle, due to higher costs and slower inference."*
  工具无 `subagent_type`、无 `model` 参数 —— **模型按角色钉，不 per-call**。`RunAgentOptions.timeoutMs` 默认 10 分钟，且 *"A timeout rejects the call but leaves the child running until its turn finishes."*

### 2.8 Aider 的 architect/editor（**不是 subagent，但直接支持决策 ③**）

| Architect | Editor | Pass Rate |
|---|---|---:|
| o1-preview | o1-mini | **85.0%** |
| o1-preview | **Baseline（solo）** | 79.7% |
| claude-3.5-sonnet | claude-3.5-sonnet | 80.5% |
| claude-3.5-sonnet | **Baseline（solo）** | 77.4% |

**两个发现**：
1. **拆分确实有效**（85.0% vs 79.7%）。
2. **但同模型跑两次也有效**（80.5% vs 77.4%）⇒ 收益**部分来自「多一次推理」而非模型异构**。

消息传递**零新协议**：architect 的纯文本输出**原样**作为 editor 的输入消息（`aider/coders/architect_coder.py:44`）。**与本仓 `expand_skill_invocation` 的「文本进、文本出」同形。**

### 2.9 Anthropic 官方多 agent 工程博客（**量化经验，最有含金量**）

来源：`anthropic.com/engineering/multi-agent-research-system`

| 事实 | 数字 |
|---|---|
| 性能提升（多 agent vs 单 Opus 4，内部 research eval） | **+90.2%** |
| 时间 | 复杂查询**减少约 90%** |
| **成本** | token ≈ 普通 chat 的 **15×** |
| 归因 | **token 使用单独解释 80% 的性能方差**（另两个因素：工具调用数、模型选择） |
| 上下文红线 | **>200,000 token 会被截断** |

**规模决策规则（可直接写成工具描述里的硬规则）**：
- 简单事实查询：**1 个 agent + 3–10 次工具调用**
- 直接对比：**2–4 个 subagent** × 10–15 次调用
- 复杂研究：**>10 个 subagent**
- 并行两层：lead 一次起 **3–5 个**；每个 sub 并行用 **3+ 个工具**
- ⚠️ **他们早期的坑：简单查询 spawn 了 50 个 subagent**

**任务描述四要素**（防重复劳动的唯一手段）：**objective / output format / tools&sources guidance / clear task boundaries**。
早期只给 *"research the semiconductor shortage"* ⇒ 三个 subagent **做完全相同的搜索**。修法：更好的「管理型」prompt + **显式的「别研究 X，那是另一个 subagent 的活」**。

**其它**：LLM-as-judge + rubric（factual accuracy / citation precision / completeness / source quality / **tool efficiency**），**约 20 条测试查询**即可起步；做了 **tool-testing agent** 重写工具描述，**省了约 40% 任务完成时间**。

---

## 3. 横切主题：**它们共同的失败模式**（本轮真正吸收的东西）

> 这一节比 §1 的对比表重要。九个独立项目的 bug 列表**高度重合**，说明这些不是实现瑕疵，而是这类功能的**固有陷阱**。

### 陷阱 1：**成本不聚合 —— 每个项目都有，每个都被用户骂**

| 项目 | issue |
|---|---|
| opencode | #45417 / #11027「会话成本不含子 agent」 |
| Roo | #5376「只看到当前任务的钱」 |
| Goose | #7877「Cost for subagent and main agent」 |
| Codex | #9748「instantly drains entire Pro plan quota」 |
| Kimi | #2024「subagent 的 context 用量不在父统计里可见」 |
| Amp | HN：*"$1000/m EACH"* |

**本设计的初版把「子 usage 不进 `TurnStats`」记为「建议 P2 就做」。⇒ 调研后改为必做**（§4）。

### 陷阱 2：**失控的孤儿 / 无界递归**

- Kimi **#2647**：孤儿 detached subagent **烧 14 小时**；**#2615**：`TaskStop` 后仍调 LLM **26 分钟**。
- opencode **#18100**：20 层嵌套 / 47 个会话（后加 `subagent_depth` 默认 1 才修好）。
- Kimi TS **#3128**：无深度守卫 ⇒ 无界树。
- Roo：**完全没有深度限制**。
- Anthropic：简单查询 **50 个 subagent**。
- Goose **#11999**：`max_turns=25` 导致子停滞 → 父误判为死循环 → 杀掉 worker。

⇒ **两条独立教训**：① **递归必须在结构上不可能**（D3 已做，并被 Gemini/Kimi/Goose 三家佐证）；② **必须有硬性的轮数/时间上限 + 可证明的终止**，不能只靠「发个 stop」。

### 陷阱 3：**静默失败**

- Goose **#11803**：工具名打错 → **静默造出无工具的子 agent**（修法是 fail-closed）。
- Kimi **#2408**：schema 说「no default timeout」，代码实际 120s。
- Kimi **#37852**（opencode）：aborted stream 被记成正常结束 → **子 agent 返回空且无错误**。
- OpenHands：*"handler 在、schema 不在"* ⇒ 功能静默不可用三个月。
- Gemini **#26146**：无限 model-invalidation 循环。

⇒ 与本仓已有的立场**完全同构**（ADR-0001 失败双通道、`StopReason::Truncated` 的「不许静默说谎」、ADR-0017「压缩失败不再静默失忆」）。**本设计必须显式拒绝这几条**：模型名解析失败 ⇒ `Err`；非正常终止 ⇒ `is_error` + 诊断；工具名未知 ⇒ fail-closed。

### 陷阱 4：**结果无界 / 有界但无指针**

- Gemini `output.result` **无上限**；Roo **无上限**；Kimi **只有下限、无上限**；OpenHands SDK **无上限**。
- 唯一有上限的：Codex（**出错时**截到 900 token）、opencode（**全局** 50KB + 落盘 + 指针）、deepseek-harness（`diagnostic` 4096 B）。
- Roo **#11364**：子返回被截断 ⇒ **父不得不自己重读**（截断而无指针 = 最坏形态）。
- Cline **#9637**：子 agent 因 **context-window 超限**报错。

⇒ 本设计 D4「**硬截断 + 落盘指针**」是**九家里只有 2 家做到的组合**。**这正是差异化点，不是过度设计。**

### 陷阱 5：**并行写冲突（本仓无 branch/worktree，尤其贴脸）**

Google 自己的博客警告 *"agents overwriting one another"*；Gemini 调度器无文件级隔离；Roo 用「单开不变量」直接放弃并行；OpenHands SDK 的 `tool_concurrency_limit` **默认 1**，官方标注 *"use at your own risk. Concurrent execution can lead to race conditions"*，并专门有压测 `test_parallel_subagents.py`，头顶写着 *"Bug class this catches: Event-attribution races"*。

⇒ **本设计不做并行，与最保守的四家一致**（OpenHands 默认 1、Roo 不可能、本仓 Step 9 本就串行）。

### 陷阱 6：**UI 不可观测性与 «分离» 的代价**

与本设计 D2「子事件不进父流」**相反**：多数项目**都**把子 agent 活动显示出来 —— Codex `/subagents` + Alt+←/→、Gemini `SubagentProgressDisplay` + 可折叠组、Kimi 全屏 TASK BROWSER、opencode 检查器浮层（能看到逐条工具调用）、Cline `SubagentStatusRow`。

⇒ **本设计在这里是少数派**，理由只有一条：**用户答② 明确说「具体内容不用展示」**。这个偏离必须**显式记录**，并写下触发重访的条件（见 design.md §5.6 决策 c）。同时，**UI 可观测性缺失正是「成本不聚合」与「排障困难」的共同根源** —— 两者在坑列表里是同一批用户的同一批抱怨。

---

## 4. 采纳清单：调研改变了哪些设计

> 每一条都标了**来源**与**对 design.md 的哪一处改动**。

| # | 改动 | 来源 | 性质 |
|---|---|---|---|
| **A** | **结果契约升级**：`SubagentOutcome` 加 `stop_reason`（**复用 `ys-core::StopReason` 既有词汇**）+ `diagnostic`（非成功时的失败详情，硬上限 4096 B） | deepseek-harness 的 `SubagentResult`；Codex 的「出错截 900」；Kimi #37852「返回空且无错误」 | **修改**（原设计只有 `{summary, artifact}`） |
| **B** | **子 usage 上卷从「建议」升为「必做」** | 陷阱 1（六个项目的同一抱怨） | **升级** |
| **C** | 派发工具加 **`description`（3-5 词，必填）** 参数，专供工具行显示 | Claude Code / Amp / deepseek-harness 三家都是必填且明写 "for display" | **新增** |
| **D** | 工具描述 + guideline 里写死 **Anthropic 的规模规则**（简单 1 个 / 复杂才多个；任务描述四要素） | Anthropic 官方博客（含「50 个 subagent」反例） | **新增** |
| **E** | `max_rounds` 默认从 8 调到 **~16**（peer 值：Goose 25 / Gemini 30 / Cline 5 分钟） | 三家默认值 + Goose #11999 的「太小会停滞」 | **修改** |
| **F** | **只读子 agent 作为决策点提出**（peer 里 6/9 有内建只读角色；本仓无沙箱） | Gemini `codebase-investigator` / Kimi `explore` / Amp `oracle` / OpenHands `code-explorer` / opencode `explore` | **新增决策点** |
| **G** | **「one visibility」原则**：工具表限制必须同时是「提示词里看不见」与「执行时拒绝」，并**大声校验未知工具名** | deepseek-harness 的 `toolFilter` + 其 AGENTS.md 硬规则；Goose #11803 反面 | **明确**（本设计已由 registry 天然满足，但要写出来） |
| **H** | **fail-closed 校验**：`model` 参数解析失败立即 `Err`；未知工具名大声拒绝 | Goose #11803；Kimi #2408 | **明确** |
| **I** | 递归解法**得到三家独立佐证**（不再只是「最便宜」） | Gemini（硬禁止 + 图深恒 1）、Kimi Python（role 检查）、Goose（两道闸门） | **确认** |
| **J** | 结果回传的「硬截断 + 落盘指针」**被确认为差异化优势**（九家中只有 2 家做到） | 陷阱 4 | **确认** |
| **K** | 不做并行**得到最保守四家的支持** | OpenHands 默认 1 / Roo 禁止 / Google 博客警告 | **确认** |
| **L** | **D2（子事件不进父流）显式标注为「少数派偏离」**，写下重访条件 | 陷阱 6 | **记录** |
| **M** | Amdahl 式提醒进工具描述：Amp 官方 *"the subagent has some overhead. It's another inference call. It's another system prompt that needs to be cached and stored"* | Amp 播客 | **新增（措辞）** |

### 明确**不采纳**的

| 不采纳 | 理由 |
|---|---|
| `mode: primary \| subagent \| all` 配置字段（opencode） | 本仓只有一个内置角色，且答① 已定为工具表裁剪。**没有第二个消费者**（ADR-0014）。 |
| `max_budget_per_run`（USD） | 本仓无定价表；硬造一个会引入需要维护的价格库。`max_rounds` + usage 上卷已够。 |
| fork（继承父历史） | 本设计选 fresh —— 隔离是 subagent 存在的**唯一理由**；Codex #24704 证明 fork 还打掉缓存。**记为 P3 条件项**。 |
| 后台 / detached subagent | 陷阱 2 的两个最惨事故（14 小时 / 26 分钟）都在这里。本仓做同步、前台。 |
| `resume` / 持久可恢复实例 | 本仓无 branch/fork，且会话 JSONL 已有自己的恢复语义。**不做。** |
| roster / focus / TASK BROWSER | 答② 明确不要。 |
| 多 provider 注册表（deepseek-harness 的 `spawn`/`fork`/`acp`/`codex`） | **本仓只有一个 in-process provider**；建注册表就是为假想消费者设计。**但 seam 的「形状」值得记住**（Provider trait 的字段集）。 |
| 子返回**下限**（Kimi 的 200 字重提示） | 那是「模型欠汇报」的补丁；本仓用**提示词里的回报模板**（§5.6 决策 d）解决，零机制。 |

---

## 5. 对 schema 形状的交叉验证（12 家收敛）

派发工具的参数，**12 家高度趋同**：

```text
{ description: 3-5 词，给用户看  ,  prompt: 完整自包含任务,  [subagent_type | mode | agent_type],  [model],  [background | resume | fork*] }
```

- **`prompt` 是必填且都强调「自包含」**：deepseek-harness 的原文 *"include everything it needs. It does not share this conversation's context"*；Gemini *"MUST be comprehensive and detailed"*；Claude Code *"The task for the agent to perform"*。
- **`description` 是必填且专供显示**（Claude Code / Amp / deepseek-harness 三家）。
- **`subagent_type` 只在多角色时才出现**；本仓只有一个角色 ⇒ **不需要这个参数**（答① 已定「不能自嵌套」+ 单角色）。
- `model` 有 4 家暴露（Codex / Kimi / deepseek-harness / 本设计），**且都以「默认继承、可选覆盖」为语义**。

⇒ **本设计的 schema（`{task, description, model?}`）与收敛结果一致**；唯一命名差异是 `prompt` vs `task` —— 建议**用 `prompt`**（7/12 家用它，且 `task` 与本仓「一次 `run_turn`」的术语容易混）。

---

## 6. 参考

- 本地（未入版本库）：`tmp/deepseek-harness/packages/subagent/`（**最有价值**）、`tmp/pi`、`tmp/Codewhale`
- 设计：[`design.md`](./design.md)；上下文：[`context.md`](./context.md)；决策：[ADR-0022](../../adr/0022-subagent-dispatch.md)
- Anthropic：`anthropic.com/engineering/multi-agent-research-system`（量化经验与规模规则）
- Aider：`aider.chat/2024/09/26/architect.html`（architect/editor 与 benchmark）
- 其余：Codex `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`（Rust）；Gemini `packages/core/src/agents/local-executor.ts`；Kimi `subagents/{models,runner,builder}.py`；opencode `packages/opencode/src/tool/task.ts`；Roo `src/core/tasks/Task.ts`；OpenHands `openhands-tools/openhands/tools/task/definition.py`；Goose `crates/goose/src/agents/platform_extensions/summon.rs`
