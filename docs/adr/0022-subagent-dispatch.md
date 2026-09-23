# 0022 — Subagent 派发：窄能力注入（照 `Summarizer`），子事件不进父事件流

给「主 agent 可派发 subagent 完成一些任务」定下形态 —— ① **派发能力走窄 trait 注入**（`ys-tool::SubagentRunner`，`&str` 进 / `SubagentOutcome` 出），经 `AgentPorts → RuntimeContext → ToolContext` 三跳送达工具，**形状与既有的 `Summarizer` 逐字同构**；② **子 agent 的事件不进父事件流**（复用既有的 `NoopEventSink`）⇒ `Source` / `Envelope` / `AgentEvent` / 三个事件消费者**全部零改动**；③ **子工具表 = 父表去掉派发工具**，且派发工具**追加在末位**（结构上不可能递归 + 命中 prompt cache）；④ **结果 = 代码硬截断的摘要 + 失败诊断 + 落盘转录的绝对路径**（照 `webfetch` 惯用法），`stop_reason` **复用 `ys-core::StopReason`**；⑤ **取消用 `ChildBoundary` 只透传 `is_aborted`**；⑥ **子 usage 必须上卷进 `TurnStats`**；⑦ 顺带解掉一个**前置阻塞项**：`BasicLoop` 用 300s 包住每个 `tool.call` 且超时 drop future。决策依据含 **12 个同类软件的补充调研**（见 `docs/arch/subagent/research.md`）。

>
> ⚠️ **成本数字待重算**：本文的固定开销实测值（子 system ~18,100、每轮 ~24,630）是**在 `CLAUDE.md` 仍被加载时**量的。
> 现 `apps/coding-agent/src/prompt.rs::load_project_context` **不再读 `CLAUDE.md`** ⇒ 本仓实测已降为
> **系统提示 4,539 / 每轮固定 ~6,168**。§4.3/§4.4 的派发成本按旧基线推算，**待重算，勿按旧数引用**。

## Status

accepted（2026-09-23）

## 背景

- **需求原话**：「我需要 subagent 的能力，至少能够区分主 agent 和 subagent，主 agent 可以派发 subagent 完成一些任务。」经澄清四点：① 子 agent **不能自嵌套**（工具表裁剪）；② TUI **只显示「在工作」即可、内容不展示**，但父 agent 需处理**返回和产物**；③ 模型**可指定**（工具参数）；④ **先补 P0 prefix 指纹测试再加工具**。
- **今天结构上不可能**：`Tool::call` 拿到的 `ToolContext`（`crates/ys-tool/src/context.rs:6-18`）只有 `boundary / cwd / workspace_root / summarizer`，**没有 Model**；且 `Agent::run_turn(&mut self)`（`crates/ys-runtime/src/agent.rs:92`）在外层回合期间已把 `&mut Agent` 借走 ⇒ 同一个 `Agent` 实例在借用层面也不可能被工具重入。子 agent 必须**另建** `Agent`。
- **形状被依赖图压死**：**`ys-model` 依赖 `ys-tool`**（为 `ModelRequest.tools: Vec<ToolSpec>`，`crates/ys-model/src/request.rs:3`）⇒ 反向成环被编译器禁止。故落在 `ys-tool` 的任何新能力 trait **不得出现 `Model` / `ModelRequest` / `Session` / `AgentEvent`**（`ys-core` 的 `StopReason` / `Usage` 可用 —— `ys-tool` 已依赖 `ys-core`）。**这不是设计偏好，是依赖图的硬结论。**
- **身份缝早就留好了，但今天没有消费者**：`Envelope { source, turn, event }` 的 `Source` 文档原话是「单 agent 阶段的唯一来源」（`crates/ys-protocol/src/envelope.rs:19`）。三个消费者（TUI `crates/ys-tui-coding/src/app.rs:86-186`、`-p`/`--json` 的 `is_terminal` `apps/coding-agent/src/main.rs:217-223`、`app_loop` 转发 `apps/coding-agent/src/app_loop.rs:129-152`）**一个都不读 `source`** —— **若**把子 agent 事件灌进同一信道，TUI 的 `is_turning` 会被子 agent 的 `RunFinished` 误关、`--json` 会提前收尾、`-p` 的 stdout 会直接吐出子 agent 正文。
- **同类软件：12 个项目里 11 个有 subagent**（Codex / Gemini / Kimi / deepseek-harness / opencode / Roo / Cline / OpenHands / Goose / Amp / pi-示例扩展），**唯独本仓没有**。工具参数**高度收敛**为 `{description（给用户看）, prompt（自包含任务）, [model]}`；结果几乎都是「最后一条消息」；隔离几乎都是 fresh。
- ⚠️ **但它们共同的失败模式才是本轮真正吸收的东西**（详见 `research.md` §3）：**成本不聚合**（opencode #45417、Roo #5376、Goose #7877、Codex #9748、Kimi #2024、Amp 的 HN 吐槽 —— **六个独立项目同一件事**）；**失控的孤儿**（Kimi #2647 孤儿 subagent **烧 14 小时**、#2615 `TaskStop` 后仍跑 **26 分钟**）；**静默失败**（Goose #11803 名字打错 ⇒ **静默造出无工具的子 agent**）；**结果无界**（Gemini/Roo/Kimi/OpenHands 都无上限，Roo #11364 截断而无指针 ⇒ **父自己重读**）。
- **一个前置阻塞项（三个候选方案各自独立发现）**：`BasicLoop` 用 `ctx.limits.bash_timeout.unwrap_or(300)` 包住**每一个** `tool.call`（`crates/ys-loop/src/basic.rs:470-475`），且 `tokio::time::timeout` 超时会 **drop future** ⇒ 超过 5 分钟的派发被静默杀掉、产物可能没落盘。

## 决策

### 1. 派发能力走窄 trait 注入（`ys-tool::SubagentRunner`）

```rust
// crates/ys-tool/src/subagent.rs —— 照 summarizer.rs 的形状
// （StopReason / Usage 来自 ys-core；ys-tool **已依赖** ys-core ⇒ 可用。
//  不得出现的是 ys-**model** 的 Model / ModelRequest，因 ys-model → ys-tool 会成环。）
use ys_core::{StopReason, Usage};

#[non_exhaustive]
pub struct SubagentOutcome {
    pub summary: String,             // 已由实现方按预算硬截断
    pub artifact: Option<String>,    // 完整转录的绝对路径
    pub stop_reason: StopReason,     // **复用 ys-core 词汇**，非 Completed ⇒ 父侧 is_error
    pub diagnostic: Option<String>,  // 非 Completed 时的失败详情，实现方须硬上限截断
    pub usage: Usage,                // 供父上卷进 TurnStats（决定 8）
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(&self, prompt: &str, model: Option<&str>,
                 boundary: Option<&dyn BoundarySource>) -> Result<SubagentOutcome, String>;
}
```

注入路径 `AgentPorts.subagent → RuntimeContext.subagent → ToolContext.subagent`，在 `BasicLoop` 既有的单点接线处（`basic.rs:459-474`，注释自称「唯一的摘要适配器接线点」）多挂一个。三处都是 `#[non_exhaustive]` 结构体的**加法式**新字段。

**为什么不是「app 侧工具自带 factory」（零核心改动）**：工具若在启动期快照 `Box<dyn Model>`，**`/model` 之后会静默用旧模型** —— `capabilities::set_model` 只换 `Wiring` 里的模型（`apps/coding-agent/src/wiring.rs:129-131`），**不重建 `Agent`**。修它得在每次换模型时重建整个 `Agent` + 工具，比三跳注入重。**「零核心改动」不是免费的，代价是把一个正确性问题藏起来。**

**为什么不是「循环内一等概念」**：违反 `docs/extension-points.md:16-26` 的判断链（模型决定何时做 ⇒ **Tool**，不是 Hook），且把策略塞进 `BasicLoop`（最高爆炸半径）。

**派发工具的 schema**（与同类软件收敛形状一致）：

```jsonc
{ "description": "必填 — 任务的 3-5 词概括，**给用户看**（TUI 工具行显示这一行）",
  "prompt":      "必填 — 完整自包含的任务正文（子 agent 看不到本对话的上下文）",
  "model":       "可选 — 省略则继承当前模型" }
```

- 参数名用 `prompt`（12 家里 7 家如此），不用 `task`（与本仓「一次 `run_turn`」的术语易混）。工具名定为 **`subagent`**（D18）。
- **`description` 是需求② 的落地关键**：没有它，TUI 工具行只能从 `prompt` 里截 40 字符（`transcript.rs` 的 `SUMMARY_KEYS` 没有能描述「派发」的 key）；有了它，「subagent 在工作」那行才可读。Claude Code / Amp / deepseek-harness **三家都把 `description` 设为必填并明写 "for display"**。

### 2. 子 agent 的事件**不进**父事件流

子 agent 用 `MemorySession::new()` + **既有的** `ys_event::NoopEventSink`（`crates/ys-event/src/noop.rs:8`）+ 新建的 `Agent`。

⇒ **`Source` / `Envelope` / `AgentEvent` / `ChannelSink` / `ys-tui-coding` / 三个消费者全部零改动。** 回合号语义、`is_turning` 误关、`--json` 提前收尾这三件事**整体消失**（不是被修好，是不会发生）。

**「至少能够区分」靠工具行满足**：模型发的是 `ToolUse{name:"task"}`，TUI 渲染成一条普通 Tool 行（`app.rs:103-119` 既有机制），结果行闭合时显示摘要 —— 与 Claude Code 的 `⏺ Task agent finished · …` 同一层级，正是需求②要的层级。

**如实记录代价**：子 agent 的内部活动**在任何输出通道都不可见**（`--json` 亦然），排障只能看落盘转录。**若将来要可见**，缝仍在：加 `Source::subagent(id)` + 在 `app_loop` 转发处与 `-p`/`--json` 的消费循环各加一次 source 判断。**触发条件**：出现「要在 UI 上看到子 agent 逐条工具调用」的真实需求。

> **本条推翻了 `context.md` 的「身份是第 1 刀」建议。** 那里判定的 bug 是真的，但它的**触发前提**（子 agent 事件进父流）在本设计下不成立。按 ADR-0014「零消费者 ⇒ 不建」，不发射比发射后过滤**更强**且更省。

### 3. 子工具表 = 父表去掉派发工具；派发工具**追加在末位**

- **结构上不可能递归**：子 registry 里没有 `subagent`，`registry.get("subagent")` 返回 `None`，走既有的「tool not found」分支（`basic.rs:504-510`）。⇒ **零运行时检查、零深度参数**（答①）。
- 按名字排除，不抄第二份列表 ⇒ 随父表自动同步。
- **末位** ⇒ 子 tools JSON 是父的**字节前缀** ⇒ 首轮命中 prompt cache。按实测基线（工具表 1,629 est token）估算省 ~1.4k 当量/次。
- **不做**按任务类型动态裁剪：动态表 = 每次派发重建 registry（**正是 CodeWhale 被自认的坑**）+ 破前缀。

### 4. 结果 = 硬截断的摘要 + 失败诊断 + 落盘转录的绝对路径

`SubagentOutcome { summary, artifact, stop_reason, diagnostic, usage }`。三道闸叠加：① 摘要 ≤ 8 KB，**在代码里硬截断**（不靠提示词）；② 完整子转录写 `~/.yushan/subagents/`，结果尾部附绝对路径，父按需 `read offset=` / `grep`（**照 `webfetch` 的 ADR-0016 惯用法**）；③ 子 `max_rounds` 保证产物有界。

- **`stop_reason` 复用 `ys-core::StopReason`，不新造词汇**：`Completed` ↔ 正常收尾；`MaxRounds` ↔ 撞轮数上限（部分结果）；`Cancelled` ↔ 被取消；`Truncated` ↔ 输出被截断。非 `Completed` ⇒ 父侧 `ToolResult{is_error: true}`（ADR-0001 错误结果通道）。（对照：deepseek-harness 需要自造 `SubagentStopReason`，因为它没有共享的 `StopReason` 类型 —— **本仓有，直接复用。**）
- **`diagnostic`**：非 `Completed` 时的安全失败详情（provider 报错原文等），**实现方须硬上限截断**（对照 deepseek-harness 的 4096 UTF-8 字节）。**绝不允许「返回空且无错误」** —— opencode #37852 正是该形态（aborted stream 被记成正常结束 ⇒ 子返回空且无错误），Kimi #11364 是它的下游后果（父只好自己重读）。
- **落 `~/.yushan/subagents/`，保留策略照 `fetched` 的先例：启动期 sweep 超过 7 天的文件**（只清 Primary 档，`Override` 档只写不删）。理由是它是**派生物**（源子会话已终止），且**悬空指针是优雅降级**（`read` 失败，模型自会 adapt），不是正确性 bug。⚠️ **不照 `sessions` 的「永不清理」** —— 那是既存缺口，不是该复制到新目录的先例；**本轮不顺手改它**。配套 `YUSHAN_SUBAGENTS_DIR` 测试覆盖口（否则测试会写进用户真实的 `~/.yushan/subagents/`）。
- **落 `~/.yushan/` 而非工作区**：会话与抓取内容都已在 `~/.yushan/`；写进用户仓库会污染其 git 工作区，本仓对此有明文警惕（`apps/coding-agent/src/main.rs:437-439`）。
- **不照抄 Claude Code 的「只回最终消息」**：那会永久丢掉细节；加一个指针的成本是一行字符串。**调研确认这是差异化优势** —— 9 家里只有 2 家做到「截断 + 指针」的组合。
- **「产物」的另一半**（子写进工作区的文件）父本就能 `read` 到；要设计的是**父怎么知道去哪找** ⇒ 在子的**输入消息**里要求按 `## Files` 节汇报。**格式不发明** —— 本仓压缩摘要已用且承重该节（`crates/ys-component/src/compact.rs:87,109,154`）。**放 messages 不放 system**（放 system 就是每轮前缀变动）。这与 deepseek-harness 的 *"runtime-context contribution rather than a system-prompt section"* 是同一条纪律。
- **失败**：子撞 `max_rounds` / 模型报错 ⇒ `Ok` + 非 `Completed` 的 `stop_reason` ⇒ `ToolResult{is_error: true}` ⇒ 照 ADR-0001 回喂父模型，**父回合不因此失败**。只有「**跑都跑不起来**」（如 `model` 名解析失败）才走 `Err`。

### 5. 取消：`ChildBoundary` 只透传 `is_aborted`，`take()` 恒 `None`

不能直接把父的 `BoundarySource` 传给子 —— `take()` 会**消费**父队列里的消息，用户在子 agent 跑动期间的一次 `Steer` 会被子吃掉、永远到不了父的轮边界。子的 `Steer` 需求为 0，故恒 `None` 是正确的。父 abort ⇒ 子在下一个轮边界停；子内部 `BashTool` 靠同一 `is_aborted()` 做 100ms 轮询。

`ChildBoundary` 是栈上值、**每次派发新建**，满足「`BoundarySource` 每回合必须新建」；**不向父 `push`**（否则子的 `Abort` 会被误当父的）。

### 6. 前置阻塞项：per-tool 超时

`Tool` trait 加**默认方法** `fn timeout(&self) -> Option<Duration> { None }`；`BasicLoop` 的超时闸改为「工具声明值 → 否则 `limits.bash_timeout` → 否则 300s」。`subagent` 声明一个长的（如 1800s）。**默认实现保证既有工具行为逐字节不变；且它不进 `ToolSpec` ⇒ 不改 prompt 前缀。** 叠加**增量落盘**，使超时/崩溃也留下部分产物。

被否的替代：调大 `RunLimits.bash_timeout`（顺带把 bash 提到半小时，钝器）；只靠子 `max_rounds` 保证跑得完（无法保证，且超时产物全丢）。

### 7. `prompt.rs:107` 改条件式派生

那句「This runtime has no subagents and no plan mode」不再是常量字符串，改为**按工具表派生**：父表有派发工具 ⇒ 不说；**子表没有 ⇒ 逐字保留**（对子 agent 而言那句是真话）。连带 `apps/coding-agent/src/prompt.rs:627` 的测试与 `apps/coding-agent/tests/e2e_skills.rs:90` 的拷贝同步。

### 8. 子 agent 的 usage **必须**上卷进 `TurnStats`

`SubagentOutcome.usage` → app 侧 runner 累加进共享计数器 → `app_loop` 每回合 drain 进 `TurnStats`（约 10 行）。

**理由**：状态行的 token 计数是**给用户看的成本**，少算就是**静默说谎** —— 与本仓引入 `StopReason::Truncated`（拒绝把截断伪装成 `Completed`）、ADR-0017（压缩失败不再静默失忆）是同一条纪律。
**调研证据**：这是同类软件里**被抱怨最多的一点**，且是**六个独立项目的同一件事** —— opencode #45417/#11027、Roo #5376、Goose #7877、Codex #9748（*"instantly drains entire Pro plan"*）、Kimi #2024、Amp 的 HN 吐槽（*"$1000/m EACH"*）。**不做就是主动重蹈一个已知覆辙。**

### 9. 工具描述与 guideline 写死「防失控」规则

内容来源：Anthropic 官方多 agent 博客的量化经验（`research.md` §2.9）。

- **规模规则**：简单事实查询 **1 个 subagent**（3–10 次工具调用）；直接对比 2–4 个；只有真正复杂、可分解的任务才多派。（反例：Anthropic 早期**简单查询 spawn 了 50 个 subagent**。）
- **任务描述四要素**（防重复劳动的唯一手段）：**目标 / 输出格式 / 可用工具与来源 / 明确边界**。（反例：只给 *"research the semiconductor shortage"* ⇒ 三个 subagent **做完全相同的搜索**。）
- **成本提醒**（Amp 官方原话）：*"the subagent has some overhead. It's another inference call. It's another system prompt that needs to be cached and stored."*
- **禁止瞎猜**：子 agent 看不到本对话 ⇒ `prompt` 必须自包含。

### 10. 子表**可写** —— 能力 = 创建时给它的工具表

不设只读限制：**子 agent 的能力完全由「创建时给它的那张表」决定**。子 agent 能写，是因为表里有 `write`/`edit`。
⇒ 附带好处：将来若要只读子 agent（12 家里 6 家有内建只读角色），**不需要新机制** —— 只是换一张表。

⚠️ **配套护栏（子表可写 + 决策 11 的并行 ⇒ 并发子 agent 可以互相覆盖文件）**：工具 guideline 写死 **「并行派发的子 agent 之间，写目标必须不相交」**（Gemini 的 prompt 原文 *"You should NEVER run multiple subagents in a single turn if their abilities mutate the same files or resources"*；Amp 的 *"multiple tasks in parallel **iff** their write targets are disjoint"*）。**本仓无 branch/worktree、不做沙箱** ⇒ 这条只能靠提示词约束，**记录为已知缺口**。

### 11. **支持并行派发** —— `BasicLoop` Step 9 从「串行（v0）」升级为**条件并行**

> **这是本 ADR 唯一需要改 `BasicLoop` 的决策**，且它推翻了初版「不做并行」的结论。按 `hooks-and-plugins/notes.md` ③ 的准入规则**显式记账**。

**为什么不能只在工具里做**：那要求把 schema 改成 `{tasks: [...]}` 数组 —— **为了并行而改掉一个已被 12 家验证过的收敛形状**，且 UI 只剩一条工具行（答② 要看到「subagent 在工作」，N 条行更好）。而循环层并行顺带修掉一个既有简化：`basic.rs:423` 的注释自称「**串行执行工具调用（v0）**」。

**机制：两个默认零行为的开关**

```rust
// ys-tool：与 Tool::timeout() 同一种加法式改动
fn is_concurrency_safe(&self) -> bool { false }   // 默认 false ⇒ 既有工具逐字节不变
// ys-component
pub max_parallel_tools: usize,                     // 建议默认 4
```

**执行规则**：仅当「批次 > 1 **且** 批次中**每一个**工具都声明 `is_concurrency_safe()`」时才走并行路径（信号量限流 ≤ `max_parallel_tools`），否则**原串行路径一字不改**。
⚠️ 「每一个」这个条件是承重的：它保证**既有的一切批次行为逐字节不变**（今天没有工具声明 `true`）；唯一走并行的新情况是「同一条消息里 ≥2 个 `subagent`」，那是新场景、不存在回归。混合批次（`subagent` + `bash`）自动退回串行。

**三个必须处理的技术点**：① `ctx.events` 是 `&mut dyn EventSink` **不能跨并发任务共享** ⇒ 先串行 emit 全部 `ToolCall`，并发执行，再**按原始顺序**串行 emit 全部 `ToolResult`（`ContentBlock` 落盘同样按原始 id 顺序，ADR-0001 配对完整性；TUI 靠 `tool_index` 按 id 回填，**无需改动**）；② 审批检查提前到并发之前；③ **每次派发建一个全新子 `Agent`** —— `Agent::run_turn(&mut self)` 使一个实例无法被两个并发派发共用，原设计的 `Mutex<Agent>` 会在并行下退化成串行，已删除。代价已核实可忽略（`ToolRegistry::build` 只是 9 个小结构体的 `HashMap`，`crates/ys-tool/src/registry.rs:25-41`）。

**上限 4，且排队而非报错**：数量依据 Codex（并发默认 4）/ Goose（后台 5）/ Anthropic 经验（一次并行 3–5 个）；行为上 Codex 与 Goose 超限都**直接报错**，对模型不友好 —— 本仓用信号量**排队**。槽位数写进 guideline（照 Codex 的 *"There are N available concurrency slots…"*），⚠️ **故它是前缀的一部分 ⇒ 必须是固定默认值，不做成运行时可调的用户配置**（与 `ToolPlugin.order` 同一条理由）。

**并行下的语义**：取消 ⇒ 每个子 agent 各自经 `ChildBoundary.is_aborted()` 在自己的下一个轮边界停下（批次等全部收摊）；超时 ⇒ 每任务各带 `Tool::timeout()`，批次墙钟 ≈ **max** 而非 sum（这正是并行收益）；用量 ⇒ 共享原子累加（决策 8）；子事件 ⇒ 仍不进父流（决策 2 不变）。

### 12. 子 agent **继承完整系统提示骨架**

工具清单 + guidelines + **项目上下文** + **skill 段**，与父同构（只有工具清单少一项、多一句「no subagents」）。照 deepseek-harness 的 *"the deployment's system prompt stays uniform across parents and children"*。

⚠️ **代价必须记住：每次派发 ~18,100 est token 的子 system**（本仓实测；详见下文「成本模型修正」），而且它**必然 miss 缓存**（前缀在 tools 层就断了）。
✅ **但有一条反向红利**：同批的 N 个子 agent 的 system 与工具表**逐字节相同** ⇒ 第 2..N 个**互相命中**（4 并发的 system 成本 ≈ 23.5k 而非 72k）。「统一」因此不只是保守，是**缓存相干**的。

### 13. 并行的闸门**只开给 `subagent`**

`Tool::is_concurrency_safe()` 默认 `false` ⇒ 既有工具**逐字节不变**；本轮只有 `subagent` 覆写为 `true`。**只读工具（read/grep/find/ls/…）本轮不动** —— 它们改的是**既有批次**的行为（事件顺序 `C1,R1,C2,R2` → `C1,C2,R1,R2`，TUI 观感 + `--json` 输出顺序），须走一次显式的行为变更评审。**准入规则先写下**：可证明对共享状态无副作用才准开（⇒ 只读工具可以；`bash`/`write`/`edit` 永远不行）。

### 14. 并发上限 `max_parallel_tools`：**常量 + 字段双轨**

`ys-loop` 的 `pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4` 是**唯一真值**；`RunLimits.max_parallel_tools` 默认取它；`prompt.rs` 读**同一个常量**写 guideline。⇒ 前缀稳定由**单一真值**保证，而不是靠「禁止使用字段」。（`is_concurrency_safe()` 只能是 **`Tool` trait 的方法**，不能是 `ToolSpec` 字段 —— 后者会被序列化进 prompt ⇒ 改它等于改前缀。）

### 15. 每回合**派发总量**上限（补一条漏掉的成本爆炸路径）

原设计只有「每个子 agent 的 `max_rounds`」与「同批并发上限」，**没有回合内的派发总量上限** ⇒ 上界 = 父 10 轮 × 每轮 4 个 = **40 次/回合 ≈ 200 万 token**。⚠️ 这正是同类软件被骂得最惨的失败模式（Codex #9748 *"instantly drains entire Pro plan"*、Anthropic 承认早期**简单查询 spawn 了 50 个**、Amp HN *"$1000/m EACH"*）—— **它们都有并发上限，都没有总量上限**。

**做法**：每回合派发次数上限（建议 16），超限返回**响亮的 `Err`** ⇒ `is_error` 结果回喂父模型。
**它零机制**：runner 本就**每回合构造**（`app_loop` 里现建）⇒ 计数器做成 runner 的字段（`AtomicU32`）就**天然 per-turn**，不需要 reset；检查放在 runner 内部，工具不用知道。

### 16. 「产物」的尾巴由 runner 保证存在

提示词要求子 agent 按 `## Files` 节汇报；**runner 在结果尾部保证一段固定形状的产物节**（子不报就写 `(none)`）。⇒ 把「这一段在不在」从**子 agent 的自觉**变成 **runner 的保证**；父能可靠区分「没产物」与「忘了报」。**它只保证格式，不保证内容完整** —— 不包装成内容保证。
❌ **被否：用 `git status --porcelain` 前后对比做真结构** —— 依赖 cwd 是 git 仓库，且**并行时兄弟的改动分不清**。⇒ **「并行」这个要求顺手关掉了「用工作区差异做结构化产物」这条路**。

### 17. 「模型可指定」的完整连带范围

除 `build_model_for(name)` 外，**必须同时**做：① ⚠️ **`Config::context_window_for(name)`** —— `context_window()` 读的是 `self.model`（`apps/coding-agent/src/config.rs:119-127`），**子 agent 换模型却继承父窗口是静默错**（32k 模型按 1M 判 ⇒ 不压缩 ⇒ 撞窗）；② **按模型名缓存实例** —— `OpenAICompatibleModel::new` 内部 `reqwest::Client::new()`（`adapters/model-openai-compatible/src/lib.rs:33-35`），不缓存则每次派发新建连接池 + TLS 握手；③ 未登录 provider 的模型名 ⇒ 立即 `Err`（fail-closed）。
⚠️ **不缓解、只记录的代价**：`model: Some(不同模型)` **使 prompt cache 全失效**，**包括决策 12 的兄弟间共享**（缓存按模型分池）。这是用户显式选择的必然代价，**默认继承时自动消失**。

### 18. 「**缓存相干**」是一等设计要求 + **捕获缓存读数**

**背景（第二轮的 token/缓存专项分析，见 `design.md` §4.4）**：三个 provider 的缓存机制完全不同，而**本仓主用的 DeepSeek 是最好的那一档** —— 磁盘自动前缀缓存、**64 token 粒度**、**TTL 数小时~数天**（V4.1 Flash 报 72 小时）、命中便宜 10×~120×。DeepSeek 官方原话确认了本设计的字节前缀假设：*"only requests with identical prefixes (**starting from the 0th token**) will be considered duplicates. Partial matches in the middle will not trigger a cache hit."*
（⚠️ 本仓适配器只发 OpenAI 兼容请求 ⇒ **发不出 Anthropic 的 `cache_control`**，在 Anthropic 后端吃不到显式断点那一层。）

**两个决定**：

1. **「缓存相干」立为一等要求**：子的固定前缀只由 **(仓库, skills 集合, 模型)** 决定，**启动期算一次并冻结**，**不得含任何时间/会话相关量**。
   ⇒ 因为 TTL 是数小时~数天，那 ~18,114 的固定前缀是**一次性**成本（兄弟间、跨回合、跨会话全命中）；破坏它 = 每 72 小时重付。**P0 指纹测试（父一份 + 子一份）正是这条的守卫。**
2. **捕获 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`**：`Usage` 加两个 `#[serde(default)]` 字段（加法式、向后兼容）+ 适配器解析（`adapters/model-openai-compatible/src/response.rs`）。
   ⇒ **本 ADR 的成本数字至今全是估算，且已经错了三次**（1.1k→2,084；520→18,100；28k→51k）。**而真数一直在服务端返回里，本仓把它丢掉了**（只映射 `prompt_tokens`/`completion_tokens`）。这一项让成本讨论从「论证」变成「读数」。
   ⚠️ 不改变 JSONL 会话契约（session 存 `Message`，不存 `Usage`），只给 `--json` 事件流加两个可选字段。

**两条必须记住的推论**：
- ✅ **决策 12（子继承完整 system）几乎不花钱** —— 那 18,114 按 0.1× 且一次性计。原先把「去掉 skill 段」当省 token 手段的论证，在缓存友好的前提下**弱了一个数量级**。
- ⚠️ **缓存降的是价格，不是窗口占用**。子的固定前缀仍实占 19.7k 上下文 ⇒ `model: Some(32k 窗口模型)` 会让子 agent 只剩 ~7.5k 干活预算（触发点被固定开销吃掉）⇒ **必然立刻压缩、什么也做不成**。工具描述须引导「同等或更大窗口」。

**最诚实的一句话**：**subagent 不省 token**（一次派发 ≈ 父自己干同一件事的 token，多出的是子固定前缀 + 回传摘要 + 可能多走的轮），**它省的是父的上下文占用** —— 一次代码勘探型派发替父省下 ~20k 的**永久**上下文。这正是「subagent 存在的唯一理由：上下文隔离」，现在有了数字。

### 19. 三条缓存纪律的对照结果（含两处真实冲突）

对照用户给的三条纪律（详见 `design.md` §4.5）：

**纪律 1（提示词与工具定义绝对稳定）—— 已满足，但本 ADR 引入了一个新抖动面。**
本仓早懂这条：`crates/ys-tool/src/registry.rs:21-22` 的注释原文就是「不能由 `HashMap` 的迭代顺序决定，否则顺序逐进程抖动（哈希随机种子），由它派生的提示词清单会让 **prompt cache 前缀失效**」。配套有 `vec!` 字面量、`EXPECTED_ORDER`、`ToolPlugin.order` 编译期常量、skill 按 command 升序。
⚠️ **新抖动面**：子 agent 的 `Agent` 是**每次派发新建**的（决策 11 为并行所必需），而 `ToolRegistry::build` 会对每个工具**重新调用 `spec()`**。若任何 `Tool::spec()` 不是**纯函数**，子 agent 自己的缓存会**跨派发抖动**，而且它的 prompt（启动期冻结）会与它的 `request.tools`（每次重算）**不一致**。
⇒ **两条新要求**：① **`Tool::spec()` 必须是纯的**；② 子 `request.tools` 必须与启动期冻结的 spec **同源**（`Box<dyn Tool>` 不可 `Clone` ⇒ 启动期建一个**重复构造器**）。
⇒ **一条新测试**：**同一份子工具表构造两次，序列化后逐字节相同**。P0 的 `sha256` 锚的是「跨版本不变」，**锁不住「同版本内运行期抖动」**。

**纪律 2（历史只追加不修改）—— ❌ 本仓故意违背，且代价此前没被记账。**
压缩走 `Session::replace_all`，把 `[m₁…mₙ]` 改写成 `[摘要, 尾部…]` ⇒ 从第 0 条消息起完全变样。
- **不压缩**：每轮只付**增量**（前缀全命中，~2k）。
- **压缩那一轮**：消息层整段分叉 ⇒ **整段按 miss 价重付**。⚠️ 实参是 `trigger_ratio = 0.85` / **`keep_ratio = 0.10`**（`crates/ys-component/src/compact.rs:66-77`）⇒ 128k 窗口下保留段只有 **~12.8k** ⇒ **那一轮 input 从 ~2k 尖峰到 ~12.8k（+10.8k）**，压缩 k 次 = k 次尖峰，**不是灾难级**。<br>（初稿按 `keep_ratio = 0.5` 算成 54k —— **是错的**。）<br>⚠️ 反过来说：**`keep_ratio` 就是「缓存尖峰高度」与「保留多少上下文」之间的旋钮**。
⇒ 这是 ADR-0017 用「缓存」换「不撞窗」的**显式代价**，但**没被记进它的调参依据**（`keep_ratio` 直接决定尖峰高度）。
⇒ **对本 ADR 的推论**：**子的会话应尽量不压缩**（子是一次性、有界的；压缩对它是「一次摘要调用 + 整段重付 + 细节丢失」三合一，而它**没有下一次会话**去享受压缩的收益）⇒ 靠 `max_rounds` 与任务范围控制。✅ 父子前缀不同 ⇒ 两者缓存互不影响。

**纪律 3（变化的部分推后）—— 已按序组织，含一处「可议但不该改」。**
顺序：`角色段 → Available tools → Guidelines → <project_context> → cwd → <available_skills>`，最不稳的 skill 目录在最末，且 `prompt.rs:17-21` 写明「严格前缀不变量」。
⚠️ 工具清单在 system **中部**，它一变其后的 ~18k 一起失效 —— 按纪律 3 似乎该挪到最后，**但不要挪**：工具清单只在**代码发布**时变，而发布本来就意味着重启、**缓存全冷**，收益为零。**记下推理，避免后人「优化」它。**

### 20. 主 agent（父）侧的缓存与 token（它才是**每轮都在付**的那个）

`design.md` §4.6。三条要点：

**① 被沿用的一个假设是错的**：`hooks-and-plugins/design.md` 写的「前缀按 `tools → system → messages` 分级」是 **Anthropic 的顺序**。本仓**不是** —— 适配器把 system 拼成 **`messages[0]`**（`adapters/model-openai-compatible/src/lib.rs:323-326`），请求体字段序是 `model, messages, tools, …`（`request.rs:6-17`）⇒ **本仓线上顺序是 `system → tools → messages`**。
⇒ 于是「加一个工具会不会废掉 18.1k 的 system 缓存」**取决于是哪种顺序，而本仓无法从代码确定**（provider 内部排序不在我们手里）。
⇒ ✅ **但设计对两种顺序都稳健**：子的 tools = 父的去掉**末位**（共享前缀最大化）；子的 system 只差工具清单**一行**；任务文本在 messages 末尾。
⇒ ⚠️ 这是 **D21 的第三个理由** —— 加工具前后各读一次命中率即可实测。

**② 四个破坏父缓存的事件**（按代价排）：压缩（那一轮 +10.8k，见决策 19）、⚠️ **`/model` 切换（下一次请求全冷：18.1k + 整段历史；文档里没写这条代价，建议在 `/model` 输出里提示）**、加/改工具（只在代码发布时发生 ⇒ 实≈0）、冷启动（DeepSeek 磁盘缓存跨进程 ⇒ `-p` 连跑与重启恢复**都是热的**）。

**③ 最大的父侧 token 杠杆不在本功能里**：`crates/ys-tool/src/truncate.rs:22-24` 的 **`MAX_BYTES = 50 KB`**（`MAX_LINES = 2000`）—— **一次 `read`/`grep`/`bash` 最多塞 ~12.5k token 进父上下文，且永久常驻**。而 **`webfetch` 早有更好的模式**（落盘 + 只回头部 + 给路径，ADR-0016）。`truncate` 是「截断的**唯一实现**」⇒ **一处改动全局生效**。
⚠️ 性质是**上下文占用**杠杆（不是每轮 token —— 那些输出首轮之后即命中）。**但若不处理，subagent 的收益会被 50 KB 的工具输出吃掉** —— 两者是同一件事的两端：subagent 挡「主动的、任务型的」大块，(c) 挡「被动的、探索型的」大块。

### ⚠️ 成本模型修正（访谈中发现低估 35 倍）

初版把子 agent 的 system prompt 估成 ~520 est token —— 那是照**空 cwd** 量的。本仓 `cwd` 是仓库根 ⇒ `build_system_prompt` 追加 `load_project_context(cwd)`，而**`CLAUDE.md -> AGENTS.md` 是软链、`load_project_context` 按路径去重** ⇒ 同一份 27,804 B 被读两次 ⇒ 用本仓估算器量得 **17,659**（单份 8,830）。

| | 初版 | 修正后 |
|---|---:|---:|
| 子首轮 system | ~520 | **~18,100** |
| 一次派发合计（R=8） | ~28k | **~51k** |
| 子 system 占比 | 8% | **36%（最大单项）** |

⇒ **连带效果**：`notes.md` 待办 10（软链重复加载）从「可选」变成**本需求的前置项** —— 修了它 18.1k → **9.3k**（≈ −49%），收益大于任何插件缝优化。

## Considered Options

- **方案甲：窄能力注入**（推荐）。代价：3 个 crate 各加 1 个可选字段。收益：模型**永远当前**（每回合从 `ports.model` 借）、工具可测试（假 `SubagentRunner` 即可单测，照 `ys-loop` 现有 `ProbeTool` 先例）、工具将来可搬进 plugin crate（只依赖 `ys-tool`）。
- **方案乙：app 侧工具自带 factory**（否决）。**零核心改动**表面诱人，但工具快照的模型在 `/model` 后**静默失效**，且工具与真 `Agent`/`Config` 绑死不可测、与 `ToolPlugin` 插件缝背道而驰。
- **方案丙：循环内一等概念**（否决）。违反判断链（模型决定何时做 ⇒ Tool），把策略塞进最高爆炸半径的 `BasicLoop`。**但其两处修正被采纳**：`ChildBoundary`（决策 5）、子 tools 字节前缀的缓存分析（决策 3）。
- **子进程路线**（照 pi 示例扩展，否决）。`CLAUDE.md` 禁止跨动态库边界传 trait object；且进程边界会让「身份」变成跨进程序列化问题 —— 而本仓已有 `Envelope.source` 这个进程内更优解。
- **子 agent 事件进父事件流 + 三消费者读 `source`**（否决，见决策 2）。今天零消费者；且「不发射」比「发射后过滤」更强 —— 后者漏一处就会把子 agent 正文泄漏进 `-p` 的 stdout。
- **落盘持久化 / 后台游离子 agent / chain 工作流**（否决）。CodeWhale 量级的产品特性，不是「加一个工具」；答② 明确只要「简单展示在工作」。
- **`ToolResult.details` 承载结构化结果**（否决）。见 ADR-0021 决策 2（JSONL 落盘契约 + 变体无 `#[non_exhaustive]`）。摘要 + 指针是**一个字符串**。

**补充调研后新增的否决项**（每条都有同类软件的实证代价）：

- **`mode: primary \| subagent \| all` 配置字段**（opencode）—— 本仓只有一个内置角色，**没有第二个消费者**（ADR-0014）。
- **fork（子会话继承父历史）** —— 与「隔离是 subagent 存在的唯一理由」相悖；Codex V2 把它设为默认（`fork_turns:"all"`）后立刻踩到 issue **#24704「Forked subagents lose prompt-cache lineage」**。**记为 P3 条件项。**
- **后台 / detached subagent** —— 同类软件**最惨的两个事故都在这里**：Kimi #2647（孤儿 subagent **烧 14 小时**）、#2615（`TaskStop` 后仍调 LLM **26 分钟**）。本仓做同步、前台。
- **`max_budget_per_run`（USD）** —— 本仓无定价表，硬造会引入需维护的价格库。OpenHands SDK 是九家里唯一的先例。`max_rounds` + usage 上卷已够。
- **`resume` / 持久可恢复的子实例**（Kimi / codex）—— 本仓无 branch/fork，且会话 JSONL 已有自己的恢复语义。
- **多 provider 注册表**（deepseek-harness 的 `spawn`/`fork`/`acp`/`codex`）—— 本仓只有一个 in-process provider；建注册表就是为假想消费者设计（ADR-0014）。
- **把并行做进工具（`{tasks: [...]}` 数组）而非循环** —— 为了并行而改掉一个已被 12 家验证过的收敛 schema，且 UI 只剩一条工具行。**已采纳循环层方案**（决策 11）。
- **并行默认开启（无 `is_concurrency_safe` 闸门）** —— 会让同批 `bash`+`edit` 也并发，直接引入既有工具的行为变更与写冲突。**必须是「默认串行 + 显式声明」**。
- **并发超限直接报错**（Codex 的 `AgentLimitReached` / Goose 的 *"Maximum N background tasks already running"*）—— 对模型不友好。**已改为信号量排队**。
- **子返回「下限」机制**（Kimi 的 `SUMMARY_MIN_LENGTH = 200` 重提示）—— 那是「模型欠汇报」的补丁；本仓用提示词里的回报模板解决，零机制。

## Consequences

- **`ys-*` 核心 crate 的改动**：`ys-tool` +1 trait +1 字段 +2 trait 默认方法；`ys-component` +1 字段 +1 limit；`ys-runtime` +1 字段；`ys-loop` 接线 1 行 + 超时改用 `tool.timeout()` + **Step 9 加条件并行路径**。**零破坏性变更、零 tokio 越界、零依赖方向变化**；除 Step 9 外全部为加法式。⚠️ **Step 9 是本 ADR 唯一动既有执行语义的地方**，其行为变更被 `Tool::is_concurrency_safe()` 默认 `false` **完全闸住** —— 既有的一切批次仍走原串行路径（决策 11 的「每一个」条件）。
- **`ys-tui-coding` / `ys-protocol` / `ys-event` / `ys-core` 零改动。**
- **`Source` 仍是预留缝**，其触发条件（要看见子 agent 逐条活动）已写下 —— 避免后人误以为「已经支持多 agent 身份了」。
- ⚠️ **本 ADR 在「子事件不进父流」上是少数派**：12 个同类软件里**多数都把子 agent 活动显示出来**（Codex `/subagents`、Gemini 可折叠组、Kimi TASK BROWSER、opencode 检查器）。偏离的唯一依据是用户答②。**代价必须记住**：UI 不可观测性正是「成本不聚合」与「排障困难」的共同根源 —— 若答② 松动，决策 2 是**第一个该翻的**（缝已留好）。
- **`context.md` 的「身份第 1 刀」被推翻**，理由与替代满足方式记录在决策 2；`context.md` 保持原样（它是探索期的诚实记录），分歧以本 ADR 与 `design.md` 为准。
- **加工具的连锁点从 9 元变 10 元**，6 处手工同步的债照旧 —— 但本需求**正是 `ToolPlugin`（`notes.md` 待办②）的第二个真实消费者**（第一个是 todo），落地后这类漂移结构性消失。
- **P0 prefix 指纹测试是本需求的第 0 刀**（用户已定④）：先锚 9 工具，冻结；加第 10 个工具后重新锚 —— 这一步把一次**预期的**前缀漂移（≈9k token 的一次性缓存失效）变成**显式**的。
- **已知缺口（明确接受）**：
  1. 子 agent 内部不可观测（只有落盘转录可查）—— **本设计的少数派偏离**，见上；
  2. **并行子 agent 之间的写冲突无人管**（子表可写 + 决策 11 的并行）—— 本仓无 branch/worktree、无沙箱，只能靠提示词约束「写目标必须不相交」。同类软件的并行事故全部集中于此（Google 官方博客警告 *"agents overwriting one another"*；OpenHands 的并发默认值至今是 **1**）。**这是决策 11 明确接受的代价。**
  3. 子 agent 的提示词是第二份固定开销，其 `fixed_prompt_tokens` 必须填**子表的值**（否则子会话系统性晚触发压缩）；
  4. 父子同时改同一文件无人管（本仓无 branch/worktree）—— 同类软件的并行事故最集中于此，与本仓「不做并行」的保守选择一致；
  5. **子 agent 的 `edit`/`write` 直落用户工作区**（本仓不做沙箱）—— 决策 10 的只读选项正是为此留的口子。
- **三项裁决已定**（用户 2026-09-23）：① 子表**可写**（能力 = 创建时给它的工具表）；② **支持并行**；③ 工具名 = **`subagent`**。见 `design.md` §8 裁决记录。
- 设计与成本模型（含 back-of-envelope）：`docs/arch/subagent/design.md`；**12 项目补充调研**：同目录 `research.md`；上下文与四点澄清：同目录 `context.md`。
