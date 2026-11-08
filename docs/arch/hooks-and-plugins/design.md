# Hook 支持 / Plugin 缝 — 架构设计

> 产出：`/arch-design`（2026-09-23）。输入：[`context.md`](./context.md)（arch-explore）。
> 证据均带 `文件:行号`，对应本工作区 `master`（`02a3b55`）。**本文所有 token 数字均为本机实测**（测量方法见 §4.2）。
> 决策记录：[ADR-0021](../../adr/0021-plugin-seam-without-hook-system.md)。
>
> ⚠️ **2026-09-23 修订**（`/arch-validate` → [`review.md`](./review.md)）：§5.1 / §5.2 / §6 P0·P1 / §7.2·§7.3 已按质量分析重写。
> 最重要的一处：**guideline 序 ≠ 工具序**（guideline 从 `edit` 开始，`prompt.rs:308`；
> 工具表从 `read` 开始，`toolset.rs:42`），且 **`grep` 的 guideline 块嵌在 read 组的 `if` 体内**
> （`prompt.rs:376`）⇒ `read` 的三块被插开。原 §6 P1 步骤 3 按字面实现会**静默重排**
> ⇒ 见 §5.2 的 `GuidelineBlock::slot`。
>
> ⚠️ **2026-11-06 修订**：**本文只负责装配缝（P1）。todo 的设计已全部移交给
> [`long-horizon-todo/design.md`](../long-horizon-todo/design.md)（ADR-0024）与
> [`cross-turn-advance/design.md`](../cross-turn-advance/design.md) §5（ADR-0072）。**
> 原先本文 §5.1 / §6 里的「`plugins/todo` 无状态 `TodoTool`」与「明确不做落盘持久化 todo」
> **均已作废** —— 落盘已由 ADR-0024 翻案，todo 真值在 `<stem>.todo.json`。
> 阶段编号见 [`exec-plans/2026-11-06-toolplugin-todo-verification.md`](../../exec-plans/2026-11-06-toolplugin-todo-verification.md)（W0–W9）。

---

## 1. 结论先行

**推荐方案丙（成本/缓存优先）**，并以方案甲（最小复杂度）的克制作为护栏：

| # | 决策 | 一句话 |
|---|---|---|
| **D1** | **不建 hook 系统** | 维持 ADR-0014。todo 是第 7 个用例，**第 7 个仍然落 Tool 桶** —— 门槛（第二个真实消费者）依旧未满足 |
| **D2** | **不引入 `ToolResult.details`** | 判据是「UI 需要但模型不需要」。todo **不满足**（模型必须看见列表才能推进），却要付 JSONL 契约的破坏性变更 |
| **D3** | **plugin 缝开在「装配层」** | 新增 `ToolPlugin { order, tools, prompts }`，把「加一个工具」的 **6 处连锁点收敛为 1 处登记**；插件自带 snippet/guidelines |
| **D4** | **显示走 `CodingView` 产品字段，不做通用渲染注册表** | `CodingView` 本就是「各产品视图字段不同」的容器（已有 `skills` / `providers` 先例）；通用注册表只有 1 个消费者 |
| **D5** | ~~**todo 状态 = 最后一次调用的 `arguments`**~~ | **已作废**（ADR-0024 supersede）：todo 真值已搬出会话、落 `<stem>.todo.json`。本文不再涉 todo 设计 |
| **D6** | **前置条件：prefix 指纹测试** | 不先钉住，搬家会静默废掉所有用户的 prompt cache。✅ **已实现**（`toolset.rs:635`，见 §6 P0） |
| **D7** | **顺手订正两处既有失真**（附带发现） | `fixed_prompt_tokens` 注释低估 1.9×；`CLAUDE.md` 软链导致项目上下文重复加载 |

**一句话**：本轮**不给 Hook**，而是给**装配缝**——把「第三方 crate 提供工具」从「改 6 处、散落 3 个文件」变成「1 个 crate + 1 行登记」。

> **todo 不是本文的产出。** 它是装配缝的**第一个真实消费者**，但其设计（落盘、注入、推进判据）
> 全在 ADR-0024。本文只负责让「加一个工具」变成一行登记。

---

## 2. 需求与约束（摘自 context.md，此处只列设计相关的）

**需求**：「考虑 hook 的支持，方便实现 todo 清单之类 plugin 功能」。经澄清：① 本轮目标是**澄清 plugin 的含义与缝**；② plugin 语义 = **静态优先**（第三方 Rust crate 编译期装配）；③ todo 只是试金石。

**硬约束**（违反即设计偏离）：

| 约束 | 来源 |
|---|---|
| `ys-core` / `ys-event` / `ys-protocol` 零 tokio；`ys-tui-coding` 不依赖 `ys-runtime`（编译器强制） | `CLAUDE.md` |
| **不跨动态库边界传 Rust trait object** ⇒ 运行期动态加载出局 | `CLAUDE.md` |
| **工具面必须恒定**（不按登录态/配置增减），降级只发生在调用期 | `apps/coding-agent/src/toolset.rs:66-72` |
| `ContentBlock::ToolResult` 是 **JSONL 落盘契约**，且变体上**无** `#[non_exhaustive]` | `crates/ys-core/src/message.rs:19-33` |
| 判断链：模型决定何时做 ⇒ **Tool**；不加工「别人的」工具结果就不需要 Hook | `docs/extension-points.md:16-43` |
| 「不设计假想未来需求」 | `docs/adr/0014` |

---

## 3. 同类软件调研（Step 2）

| 来源 | 关键事实 | 对本仓的意义 |
|---|---|---|
| **pi**（`tmp/pi`，本仓认定的参照实现） | `ExtensionAPI` 运行期注册；工具对象**自带 `renderCall`/`renderResult`**；状态藏在 `AgentToolResult.details`，靠 `session_start`/`session_tree` 从 session 重放重建；扩展靠目录扫描 + 运行期 import 发现 | **形状可借（工具 + 提示贡献 + 渲染三件套），实现不可借**——Rust crate 边界 + 无 branch 使「自带渲染」与「重放」都不成立 |
| **MCP** | 协议边界（client/server）；三原语 Tools（模型控制）/ Resources（应用控制）/ Prompts（用户控制）；`tools/list` 动态发现 + `list_changed` | 「提供工具」与「harness」可分离这件事本身可借；**动态工具表**与本仓「工具面恒定」正面冲突 |
| **Claude Code hooks** | 30 个具名事件、三种节奏（per-session / per-turn / per-tool-call），只有 `PreToolUse` / `UserPromptSubmit` / `Stop` 能阻塞。官方立场：**「必须每次发生的事，放进 hook 而不是 prompt」** | 这条立场正是 `docs/extension-points.md` 判据链的英语版；本仓判据更严（只有「加工别人的结果」才准入） |
| **Claude Code `TodoWrite`** | 状态**活在对话上下文里**；harness 往**工具结果**注入 `<system-reminder>`；**每次调用返回完整列表**（无部分更新逻辑） | 「完整列表」语义 ⇒ **最后一次调用的 `arguments` 即当前状态**（D5 的直接来源） |
| **Claude Code `Tasks`（2026-01）** | 改为落盘 `~/.claude/tasks` + 4 个工具 + `blockedBy` 依赖图；**理由正是 TodoWrite 不持久**（`/clear` 即失、压缩会摘要掉） | 「状态在对话上下文」与「状态持久」是**两种不同产品语义**。⚠️ **本行原结论「本轮明确选前者」已作废** —— ADR-0024 选了**后者**（落盘到 `<stem>.todo.json`） |
| **prompt caching**（Anthropic / OpenAI 文档与工程博客） | 前缀按 `tools → system → messages` 分级，**任一级变化 ⇒ 该级及其后全部失效**；MCP 客户端**必须**对工具表排序；时间戳进 system 开头是经典缓存杀手 | 「工具面恒定 + 前缀字节冻结」的**定量依据**（§4.3） |
| **`inventory` / `linkme`（Rust 生态）** | 链接期分布式注册，可在 `main` 之前自动收集 | **注册顺序由链接器决定、不确定** ⇒ 直接撞缓存前缀稳定。**拒绝** |

---

## 4. 三个候选方案（Step 3）

### 4.1 对照矩阵（Step 4）

| 维度 | **甲：最小复杂度** | **乙：可扩展优先** | **丙：成本/缓存优先** |
|---|---|---|---|
| 形状 | 无状态 `TodoTool` 独立 crate；显示塌成一行 | `ys-ext::Extension` + `ys-protocol::ToolDisplay` 双缝 | `ToolPlugin` 声明式装配 + `CodingView` 产品字段 |
| ~~todo 状态模型~~ | ~~无状态（会话 `arguments`）~~ | — | ~~同左~~ → **两者均被 ADR-0024 取代**：有状态 `TodoStore` + 落盘 |
| 新增代码 | ~200 行（1 crate）+ **16 行**装配 | ~270 行 + 2 个新模块 | ~200 行（1 crate）+ 装配重构（净减） |
| 新概念数 | **1**（`Tool`，已有） | **5**（`Extension`/`ToolDisplay`/`Tone`/`RenderLine`/`DisplayRegistry`） | **2**（`ToolPlugin`/`TodoItem`） |
| 破坏性变更 | 0 | 0 | 0 |
| 每轮 token 增量 | +173 | +173 | +173 |
| 装配连锁点 | 仍 **6 处** | **1 处** | **1 处** |
| 演进灵活性 | 低（下一个插件再手改 6 处） | **高**（注册表 + 渲染缝） | 中（装配缝已开，显示缝未开） |
| 主要风险 | 用户**根本看不见**清单 | 过度设计：**1 个消费者**撑 5 个新概念 | 重构静默改写缓存前缀 |
| 证据强度 | 中 | 中 | **高**（实测量化） |

### 4.2 成本模型（方案丙的量化，已独立复现）

**测量方法**：在 `apps/coding-agent` 内走真实装配路径（`build_tools` → `specs()` → `build_system_prompt`），cwd 取空临时目录（无 `AGENTS.md`）、catalogue 为空 ⇒ 只量工具面。

**实测结果（今日，9 工具）**：

| 组成 | 字节 | est token | 占比 |
|---|---:|---:|---:|
| 工具表 JSON（`request.tools`） | **6,517** | **1,629** | 78.2% |
| system prompt（snippet 行 + guidelines + 骨架） | **1,823** | **455** | 21.8% |
| **合计 = `fixed_prompt_tokens`** | **8,340** | **2,084** | 100% |

分工具实测（`bytes` / `est tok` / `description` 字节）：

| read | write | edit | bash | grep | find | ls | websearch | webfetch |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 367/91/40 | 317/79/68 | 662/165/69 | 274/68/41 | 1,329/332/542 | 705/176/306 | 454/113/197 | 1,019/254/759 | 1,380/345/955 |

**两个发现**：

1. **单工具成本几乎完全由 `description` 散文长度决定**，不由 schema 结构决定。`webfetch`+`websearch` 两个工具（599 est tok）占 9 个工具总成本的 **37%**。schema 骨架稳定在 ~250–350 B。
2. ⚠️ **既有注释失真**：`crates/ys-loop/src/basic.rs:618` 与 `crates/ys-component/src/limits.rs:20` 都写「9 工具时缺口约 **1.1k**（可忽略）」，实测是 **2,084** —— **低估约 1.9 倍**。该数字只估了 name+description、漏了 `parameters` 的 JSON。这不改变任何行为（`fixed_prompt_tokens` 走完整 `serde_json`），但**压缩触发点的真实位置比注释所述更靠前**，调参的人会基于错误基线判断「离 R5 早退还有多远」。

### 4.3 缓存经济学的决定性算式

前缀按 `tools → system → messages` 分级失效。设前缀规模 `P`、缓存命中单价按 0.1× 计：

```
静态（前缀不变）：每轮成本        C_static  = P × 0.1（命中）
动态（前缀每轮变）：每轮成本      C_dynamic = P × 1.0（永不命中）
差额                             Δ = 0.9 P
```

| 机制 | 每轮代价 | 相对静态加一个工具 |
|---|---:|---:|
| 静态加一个克制写法的工具 | **+173 tok** | 1× |
| 静态加一个 `webfetch` 写法的工具 | +345 tok | 2× |
| **工具面每轮变动**（取装 56 skill 的 `P ≈ 10k`） | **+9,000 tok** | **52×** |

**这就是「工具面必须恒定」的定量表述**：任何动态/可插拔的工具表，**每轮**代价是静态加工具的五十倍以上。据此把动态工具面列为**不可谈判的排除项**，而不是「后续再考虑」。

---

## 5. 推荐方案（丙）

### 5.1 模块划分与职责

| 模块 | 归属 | 职责 | 改动 |
|---|---|---|---|
| `ToolPlugin` / `ToolPrompt` | **`ys-tool`**（新模块 `plugin.rs`） | 纯数据：`order: u16` + `tools: Vec<Box<dyn Tool>>` + `prompts: Vec<ToolPrompt>`（guideline 的渲染序由每块的 `slot` 给，**不是**插件序 —— 见 §5.2） | 新增 ~50 行 |
| `toolset` | `apps/coding-agent` | `build_toolset(workspace, web, plugins)`；内置 **11 元**逐字不变，扩展块按 `order` 追加；`EXPECTED_ORDER` / `assert_eq!(len,11)` 降为**派生量** | 改（净减） |
| `prompt` | `apps/coding-agent` | `tool_snippet` / `tool_guidelines` **两张手抄表删除**，改查 `ToolPlugin` 声明 | 改（净减） |
| `plugins/todo` | **新 crate** | ~~`TodoTool`（无状态）~~ → **已移交**：`adapters/tools-todo`（有状态 `TodoStore` + 落盘），见 ADR-0024 §7.1。本文只管装配，不管 todo 的形状 | 见 ADR-0024 |
| `CodingView` | `ys-tui-coding` | 新增 `todos: Vec<TodoItem>`（先例：`skills` 字段） | 见 ADR-0024 |
| `view.rs` | `apps/coding-agent` | ~~`latest_todos()` 反向扫 `arguments`~~ → **已作废**：真值在 `TodoStore`，不在会话里 | 见 ADR-0024 |
| `draw.rs` | `ys-tui-coding` | 固定高度 todo 面板（≤5 行 + `… N more` 折叠，照 pi） | 改 |
| `ys-core` / `ys-event` / `ys-protocol` / `ys-runtime` | 核心 | **零改动** | **0** |
| `ys-component`（`RuntimeContext`）/ `ys-loop`（`basic.rs`） | 核心 | **P1.5**（§6）：加一个**可选**端口 + `None` 时就地回落。不挂载时逐字节不变 | +~10 行 |

> **`ToolPlugin` 放 `ys-tool` 而非新 crate**：第三方本就必须依赖 `ys-tool` 才能实现 `Tool`；放这里**零新 crate**，且 `ToolSpec.description` 已是 prompt 面向的文本，prompt 数据进 `ys-tool` 不新增语义类别。

### 5.2 核心接口

```rust
// crates/ys-tool/src/plugin.rs —— 纯数据，无 tokio
use crate::Tool;

/// 一个工具对**每轮固定前缀**的全部贡献。
///
/// 存在的理由：把「加一个工具」的 6 处连锁点（EXPECTED_ORDER / vec! 字面量 /
/// 3 处 assert_eq!(len,N) / tool_snippet / tool_guidelines）收敛成**一处声明**。
pub struct ToolPlugin {
    /// **工具位次**（升序折表），决定 `tools()` 与 tools JSON 的顺序。
    /// **必须是编译期常量** —— 顺序一变，prompt 前缀字节就变，所有用户的缓存全废（§4.3）。
    /// 不得来自 env / 登录态 / 探测结果。
    /// **同一批插件内必须唯一**：重复则顺序退化为「声明先后」这一隐式规则，
    /// 与「序必须是显式常量」直接冲突。由 `sort_plugins` 校验（返回 `Result`，不 panic）。
    pub order: u16,
    pub tools: Vec<Box<dyn Tool>>,
    pub prompts: Vec<ToolPrompt>,
}

/// 工具对 system prompt 的文本贡献。
pub struct ToolPrompt {
    pub tool: String,
    /// `- {tool}: {snippet}` 的 snippet 部分。
    /// **必须独立于 `spec().description`** —— 今天两者文本不同（`prompt.rs:117-138`），
    /// 复用会改前缀字节、废掉既有缓存基线。
    pub snippet: String,
    /// 本工具贡献的 guideline 块。空 = 该工具无 guideline。
    ///
    /// ⚠️ **块序是全局量，不是每插件内序**：`read` 的三块被 `grep` 的块插在中间
    /// （`prompt.rs:376` 嵌在 `:357` 的 `if` 体内）。⇒ 每块必须自带 [`GuidelineBlock::slot`]。
    pub guidelines: Vec<GuidelineBlock>,
}

/// 一块 guideline（渲染时每行加 `- ` 前缀）。
///
/// **不建模成 `Vec<String>`**：`render_guidelines`（`prompt.rs:305-599`）实际有 13 个门控块，
/// 其中 **3 个不是单工具门控**，`narrate` 块**夹在 read 组与 websearch 之间**，
/// 且 **`grep` 块嵌在 read 组的 `if` 体内**。简化成「每工具一个字符串列表」会静默改变字节顺序。
pub struct GuidelineBlock {
    /// **全局渲染位次**（升序）。与插件 `order` **无关** —— 见上「块序是全局量」。
    /// 现有 13 块的实际位次见本节末的清单表；新插件取「当前最大值 + 1」
    /// （`todo` 块插在 `subagent_send` 之后、尾部无条件三条之前）。
    pub slot: u16,
    /// **门控**。默认是「本工具在场」，但实际有跨工具门控。
    pub gate: Gate,
    /// **生效范围** —— `render_guidelines(tools, narrate)` 的第二个维度。
    pub scope: Scope,
    pub lines: Vec<String>,
}

pub enum Gate {
    /// 本工具在场（大多数）。
    ThisTool,
    /// 列出的工具**任一**在场。例：`edit\|\|write` 那条、read 组的 4 工具、
    /// narrate 块的 7 工具（`read/grep/find/ls/bash/edit/write`）。
    AnyOf(&'static [&'static str]),
}

pub enum Scope {
    /// 两个调用点都出。
    Always,
    /// 仅 `narrate = true`（主 agent）出。今天只有 narrate 那一块。
    MainOnly,
}
```

> ⚠️ **尾部三条无条件 guideline 不进 `ToolPlugin`**：`"Do exactly what the user asked…"`（`prompt.rs:588`）、
> `"Be concise in your responses"` / `"Show file paths clearly when working with files"`（`prompt.rs:598-599`）
> **不属于任何工具**，仍由 `build_system_prompt` 在拼完所有插件的 guideline 之后**逐字追加**。

> ⚠️ **guideline 渲染序 ≠ 工具序 ≠ 插件声明序**。完整清单（P1 的字节基准，共 **13 个门控块 + 3 条无条件**）：
>
> | # | 位置 | 门控 | 归属插件 |
> |---|---|---|---|
> | 1 | `:308` | `edit`（3 条一组） | `edit` |
> | 2 | `:319` | `write` | `write` |
> | 3 | `:328` | `edit \|\| write` | `edit`（`AnyOf`） |
> | 4 | `:357` | `read/grep/find/ls` 任一 | `read`（`AnyOf`） |
> | 5 | `:376` | `grep`（**嵌套在第 4 块内**） | `grep` |
> | 6 | `:383` | 同第 4 块（仍在它内部） | `read`（`AnyOf`） |
> | 7 | `:394` | `read \|\| grep` | `read`（`AnyOf`） |
> | 8 | `:426` | `narrate` **且** 7 工作型工具任一 | `read`（`AnyOf` + `MainOnly`） |
> | 9 | `:445` | `websearch` | `websearch` |
> | 10 | `:451` | `webfetch` | `webfetch` |
> | 11 | `:473` | `bash_bg`（2 条） | `bash_bg` |
> | 12 | `:488` | `subagent_async`（9 条） | `subagent_async` |
> | 13 | `:572` | `subagent_send` | `subagent_send` |
> | — | `:588/598/599` | 无条件 | **不属任何插件**，尾部追加 |
>
> **由此直接得出的三条约束**：
> ① **`read` 插件的三块不连续** —— 第 4、6、7 块被第 5 块（`grep`）**插在中间**（`:376` 嵌在
>    `:357` 的 `if` 体内）。⇒ **不能**用「按 `ToolPrompt::order` 遍历插件、每插件吐出自己的
>    全部 guideline」来渲染 —— 那样会得到 4,6,7 相邻、5 在后的顺序，字节就变了。
>    **必须**把「块序」本身表达出来（例如每块带一个 `slot: u16` 全局位次）。
> ② 第 8 块（narrate）夹在 `read` 组与 `websearch` 之间，故必须归到 `read` 插件、
>    不能当独立尾部块。
> ③ 尾部三条无条件 guideline **恒定最后**，不参与插件排序。
>
> 这三条同时成立才字节不变 —— 由 `fp_prompt`（P0）守住。
>
> **注意**：这张表本身就是 W1 必须逐块对照的清单 —— 搬完家后 `render_guidelines`
> 的输出必须与今天**逐字节相同**（除了预期的 `todo` 新块）。

> ⚠️ **三个 P1 陷阱（照字面实现会静默漂移前缀）**：
>
> 1. **guideline 序 ≠ 工具序** —— 见上表：guideline 从 `edit` 开始，工具表从 `read` 开始。
>    ⇒ `ToolPrompt::order` 必须独立于 `ToolPlugin::order`。
> 2. **`narrate` 维度插件表达不了** —— 两个调用点：`prompt.rs:298` 的 `tool_guidelines`（`true`，
>    主 agent）、`prompt.rs:154`（`false`，探索子 agent）。后者**只删** narrate 那一块。
>    ⇒ `Scope::MainOnly`。
> 3. **`EXPECTED_ORDER` 有 5 处消费者**（`assembly.rs:326`、`prompt.rs:687`、`prompt.rs:822-823`、
>    `prompt.rs:869`、`prompt.rs:2117`），不是原设计以为的 1 处。降为派生量时五处都要改。
>    （`prompt.rs:869` 的注释写「10 元」，实际已是 11 —— W1 一并订正。）

```rust
// apps/coding-agent/src/toolset.rs —— 唯一装配接缝
pub struct Toolset {
    pub tools: Vec<Box<dyn Tool>>,  // 按 order 升序
    pub prompts: Vec<ToolPrompt>,
    pub hint: Option<String>,        // 既有降级提示，不变
    pub order: Vec<String>,          // 顺序真值（取代 EXPECTED_ORDER 的手抄）
}

/// 内置 11 元顺序**逐字不变**；扩展块按 order 追加在后。
pub fn build_toolset(workspace: &Path, web: WebConfig, plugins: Vec<ToolPlugin>) -> Toolset;

/// 第三方登记点：**加一个插件只改这里一行** + Cargo.toml 一行依赖。
pub fn plugins() -> Vec<ToolPlugin> {
    let mut p = builtin_plugins();          // 内置 11 个，各自一条声明
    p.push(ys_tools_todo::plugin());
    p
}
```

```rust
// crates/ys-tui-coding/src/view.rs —— 产品字段（先例：skills / providers / available_models）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem { pub content: String, pub status: TodoStatus }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus { Pending, InProgress, Completed }

pub struct CodingView { /* …既有字段… */ pub todos: Vec<TodoItem> }
```

> **`CodingView.todos` 仍是正确的落点**（产品字段容器，先例 `skills`），
> 但**数据来源变了**：不再从 `arguments` 反推，而是从 `TodoStore::snapshot()` 取。
> 原先的 `latest_todos(messages)` 反向扫（旧 D5）**已作废** —— 见 ADR-0024 决策 1。

**依赖方向自证（不构成环）**：

```text
ys-core ──► ys-tool（+plugin.rs，只加纯数据）──► apps/coding-agent
                  ▲
                  └── adapters/tools-todo（只依赖 ys-tool + ys-core，零 tokio）

ys-tui-coding ──► ys-core / ys-event / ys-protocol   ← 依赖集合**一个都没变**
```

### 5.3 关键数据流

```text
【启动期 · 一次 · 之后冻结】
  plugins() ──[内置 11 + 第三方 N]──► build_toolset ──► Toolset{tools, prompts, order}
       │                                   │
       │                                   ├─► ToolRegistry ──► Agent
       │                                   ├─► build_system_prompt   → fixed_prompt_tokens
       │                                   └─► 顺序真值（不再手抄两份）
       │
【每回合】
  ModelRequest { tools: 6,517B, system: 1,823B, messages: session }
       │            └─ [缓存前缀：逐字节不变 ⇒ 命中]
       └─► 回合结束 → build_view → TodoStore::snapshot() → CodingView.todos
                                                         └─► Outbound::View → TUI 面板（≤5 行）
```

> **todo 在回合内的完整数据流**（工具写入 → `TodoStore::apply` → 落盘 → 回合边界注入）
> 见 [`long-horizon-todo/design.md`](../long-horizon-todo/design.md) §7.3。本文不重复。

### 5.4 对 5 条缝的取舍

| 缝 | 结论 | 判据 |
|---|---|---|
| **① 加工具** | **用**（本轮唯一必做） | 每轮 **+173 est tok（+8.3%）**，一次性写死在编译期；把 6 处连锁点收敛为 1 处声明 |
| **② 自带显示** | **有条件用**：走 `CodingView.todos` | 每 Request 一次快照，**不进请求 ⇒ 0 token**。`CodingView` 本就是产品字段容器（`view.rs:4` 注释：「各产品的视图字段不同」；已有 `skills: Vec<SkillEntry>` 先例）。**拒绝**「工具自带 render」（crate 边界不可能）与「通用渲染注册表」（1 个消费者撑 5 个新概念） |
| **③ `details`** | **不用** | 判据 =「UI 需要但模型不需要」。todo 不满足（模型必须看见列表）。**翻案条件**：出现「UI 要的字段量远大于模型要的」（如 50 项 todo 的进度统计），且实测 content 冗余 > 500 tok/轮 |
| **④ 生命周期事件** | **不用** | 状态从 `session.messages()` 派生即可（先例：`view.rs:91` 的 `message_count`）。本仓**无 branch/fork** ⇒ pi 的「重放重建」论据不成立 |
| **⑤ 每回合注入** | **不用**（被绕过，不是被实现） | 工具结果**本身就是会话消息** ⇒ 下一轮模型必然看见。若强行注入：进 **system** ⇒ 每轮前缀变动 ⇒ **+9,000 tok/轮**（§4.3）；进 **messages 尾部** ⇒ 缓存友好但仍烧 token，且会被压缩摘要掉，换来收益 **0**（模型本来就看得到） |

### 5.5 hook 系统的去留

**结论：不建**（维持 ADR-0014，`docs/design.md:291-293` 的状态注继续有效）。

逐条走判断链（`docs/extension-points.md:16-26`）：

| 用例 | 落桶 | 为什么不是 hook |
|---|---|---|
| todo 每回合提醒 | **Tool** | 工具结果就是会话消息，下一轮**必然**进 `messages` |
| 提交后同步 `AGENTS.md` | **Follow-up** | 回合**之间**；ADR-0014 已决 |
| 自动压缩 | **Compaction**（已有） | 不加工「别人的」结果 |
| LSP 改完查诊断 | **工具内部** | `EditTool` 是我们自己的 |
| 第三方脱敏/审计**任意**工具输出 | **Hook**（唯一合格形状） | **今天 0 消费者** |
| 每回合注入时间/记忆召回 | **Hook(Transform)** | **今天 0 消费者** |

**七用例（含 todo 试金石），仍零个需要 Hook。**

**若将来要建**，本轮把三条约束补全（供后人照做，本轮不写代码）：

1. **落点** `ys-component`（`RuntimeContext`），在 `BasicLoop` Step 5（`basic.rs:246-253`）前后各挂一次；
2. **签名只用 `ys-core` / `ys-model` 类型** —— `ModelRequest` 住在 `ys-model`，故 `before_model_request(ModelRequest) -> ModelRequest` **不构成环**；
3. 🆕 **注入必须走 `messages` 尾部，绝不改 `system`** —— `system` 是每轮整块重发的前缀，动它等于每轮废缓存（§4.3）。这条是本次新增的硬约束。
4. **触发门槛**：出现「一个**不是我们写的**工具，且它的结果需要被加工」的真实需求。本轮开了装配缝之后，这个消费者**第一次成为可能** —— 所以本方案同时把 hook 的触发条件从「不可能」推进到「随时可能」，**但不提前建**。

---

## 6. 分阶段落地

### P0（前置，必做）：prefix 指纹测试 —— **已实现**

> ✅ **2026-11-06 核实：本节已完成。** 实际实现位于 `apps/coding-agent/src/toolset.rs:635`
> 的 `prefix_fingerprint_is_anchored_and_stable`。**实际形态与本节原设计有四处不同，以代码为准**：
>
> | 本节原写 | 代码实际 | 为什么代码更好 |
> |---|---|---|
> | `sha256(&json)` | `fnv1a64(json.as_bytes())`（`:646`） | 省 9 个依赖包（`sha2` → `cfg-if`/`cpufeatures`/`libc`/`digest`/…），与 ADR-0072 D10 同一取舍 |
> | 一个 `PREFIX_TOOLS_SHA256` | **两个锚点**：`fp_tools`（子表 11 元，`:726`）+ `fp_parent`（父表 14 元，`:736`） | 追加段（`bash_bg` / `subagent_async` / `subagent_send`）的 schema 漂移在子表锚点里是**静默的** |
> | 锚两个 system prompt | 只断言 `fp_prompt` **两次构建一致**，**不入锚** | 注释理由（`:715-717`）：`fp_prompt` 含 `Current working directory: <临时目录>`，逐次运行都不同 |
> | — | 额外断言父表 JSON 是子表 JSON 的**字节前缀**（`:684`） | 这是子 agent 命中父前缀缓存的机械保证 |
>
> **本节不再需要执行**，直接作为 P1 的验收信号使用。

**它的作用**（原文保留）：把 snippet/guidelines 表搬进 `ToolPlugin` 时，**一处空格、一次 `trim`、一次顺序重排都会改变所有用户的前缀 ⇒ 缓存全废 ⇒ 每轮多付 ~9,000 token，而 832 条测试依然全绿。**

**它同时是 P1 的唯一验收信号**：P1 是纯搬家；若搬完**指纹仍绿**，就机械地证明了搬家忠实。一旦 P1 主动改了字节（例如把 guideline 序并进工具序），这个信号就没了 —— **这是 §5.2 坚持两条独立序的理由。**

**锚点常量的生成时点**（流程，同样承重）：危险在「重新量」—— 把失败测试的实际值抄进常量，**正是真实前缀漂移被洗白的唯一窗口**。
⇒ 锚点从**重构前的干净 checkout** 生成（独立 worktree / `git stash`），**不是**从重构后的树里「跑红 → 抄值」。

⚠️ **注意**：P1（纯搬家）**不得**改变这两个锚点。下一次**预期内**的漂移是新增 `todo` 工具（W5）。

### P1：装配缝（零行为变化，可独立发布）

1. `ys-tool` 新增 `plugin.rs`（`ToolPlugin` / `ToolPrompt` / `GuidelineBlock`，见 §5.2 ——
   **guideline 块序是全局的**，不由工具序或插件序派生）。
2. `toolset::build_toolset` 取代 `build_tools`；内置 **11 元** `vec!` **一字不动**，改为 `builtin_plugins()` 的 11 条声明。
3. `prompt.rs` 删除两张手抄表：
   - `tool_snippet` → 查 `ToolPlugin.prompts[].snippet`（按 `tool` 名匹配）；
   - `tool_guidelines` → 收集全部 `GuidelineBlock`，**按 `slot` 全局升序**展开（**不是**按工具 order、也不是按插件 order）；`narrate=false` ⇒ 过滤 `Scope::MainOnly`；
   - **全局尾部三条**（`prompt.rs:588` / `:598` / `:599`）**留在 `build_system_prompt`**，逐字追加在所有插件 guideline 之后。

   ⚠️ **这一步是 P1 唯一会出错的地方**（三个陷阱）：① 按**工具 order** 展开 guideline ⇒ 顺序变成 `read, write, edit, …`，而今天是 `edit` 组在最前；② 按**插件**分组展开 ⇒ `read` 的三块被 `grep` 插开的事实丢失；③ 漏掉 `narrate` ⇒ 探索子 agent 多拿一条。三者都会让 `fp_prompt` 变红，但要在动手之前就知道。
4. `EXPECTED_ORDER` 降为派生量；**测试侧保留一份独立硬编码的名字序列**（不引用 `plugins()`）。
   ⚠️ 它有**五处**消费者：`assembly.rs:326`、`prompt.rs:687`、`:822-823`、`:869`、`:2117`。降为派生量时**独立字面量必须落在测试侧、且这五处都改用它**，否则「装配端 vs 测试端两份真值」的交叉校验名存实亡。
5. 测试：打乱传入顺序 ⇒ 输出序不变（变异：不排序 ⇒ 红）；扩展在场时前 9 个名字仍是原序（变异：扩展插到前面 ⇒ 红）；跨插件重名工具 fail-fast —— **错误信息须点名来源**（形如 `tool "todo" declared by plugin #3 and plugin #7`），否则第三方拿到 `duplicate tool name: todo`（`registry.rs:30`）不知道是谁撞了谁。

**验收**：`cargo test --workspace` 全绿；**无任何 `ys-*` 核心 crate 被改动**；**P0 的指纹测试保持绿** —— 这是「搬家忠实」的机械证明，也是「零行为变化」承诺的可测形式。

### P1.5：补可达性（`summarizer` 上 `RuntimeContext`）

**与 P1 正交、可并行**；来自 `notes.md` ④ 修法 1，但**形状要改一处**。

```rust
// crates/ys-component/src/context.rs —— 加一个可选字段（该类型是 #[non_exhaustive]，加法式）
pub struct RuntimeContext<'a> { /* …既有… */ pub summarizer: Option<&'a dyn Summarizer> }
```

**不改 `AgentPorts` 签名、不 `pub ModelSummarizer`**：`BasicLoop` 在 `None` 时**就地回落**到今天的构造：

```rust
let fallback;
let summarizer: &dyn Summarizer = match ctx.summarizer {
    Some(s) => s,
    None => { fallback = ModelSummarizer { model: ctx.model }; &fallback }
};
```

**为什么不能照抄 `approval` 的位置**（`notes.md:111` 原文说「机械替换，零语义变化」—— **低估了这道缝**）：`approval` 在 **build 期**由 `AgentBuilder` 持有（`builder.rs:62`），`RuntimeContext.approval` 只是它的借用（`agent.rs:109`）；而 `ModelSummarizer { model: ctx.model }`（`basic.rs:463`）需要 **per-turn** 的 `&dyn Model`（`AgentPorts.model`，`agent.rs:21`）。「照抄」会把 `ModelSummarizer` 逼上 `pub`（现为 `pub(crate)`，`summarize.rs:53`）**并改 `AgentPorts`**。可选覆盖的形状同时满足两件事：缝**可达**，且不挂载时**逐字节不变**（与 `boundary` 同手法，`context.rs:23`）。

**验收**：不挂载时行为逐字节不变（既有 833 条测试全绿即证）；挂载假 `Summarizer` 后，`ToolContext.summarizer` 拿到的是它而非 `ModelSummarizer`。

**边界（写下来，本轮不做）**：compaction 的摘要**不走这条缝** —— `compact.rs:295` 的 `generate_summary` 用自己的 `policy.summary_system_prompt`，与 `ys_tool::Summarizer` 是两回事（已核 `compact.rs:33`）。⇒ 同进程将有**两套摘要**，本轮**有意分离**（提示词与用途不同）；是否统一归 ADR-0017 决策 10 的「片 2」。

### P2：todo 作为第一个真实消费者 —— **已移交**

**本节原内容（无状态 `plugins/todo` + `latest_todos()` 反向扫 `arguments`）已作废**：
ADR-0024 把 todo 真值搬出会话并落盘（`<stem>.todo.json`），**正面 supersede** 了无状态方案。

todo 的完整设计见：

| 内容 | 文档 |
|---|---|
| 状态 / 注入 / 推进判据 / 五道护栏 | [`long-horizon-todo/design.md`](../long-horizon-todo/design.md)、ADR-0024 |
| 清单 + 证据验收（三期） | [`cross-turn-advance/design.md`](../cross-turn-advance/design.md) §5、ADR-0072 |
| 实现波次 | [`exec-plans/2026-11-06-toolplugin-todo-verification.md`](../../exec-plans/2026-11-06-toolplugin-todo-verification.md) |

**仍然有效的那一条**（与 todo 无关，属装配缝本身）：

单工具 spec 预算约束：`ToolSpec.to_string().len() <= 400`（≈100 est tok），超限即红 ——
防止下一个工具照抄 `webfetch` 的 955 B description。**留一条显式豁免通道**
（`webfetch`/`websearch` 是「讲清代价」的正当理由撑大的），否则约束会被整体删除而不是被遵守。

### P3（条件触发，本轮不做）

| 项 | 触发条件 |
|---|---|
| `ToolResult.details` | 出现「渲染器要读、但模型不需要（甚至有害）」的状态，且实测 content 冗余 > 500 tok/轮 |
| 通用渲染注册表 | 出现**第二个**需要自带渲染的工具 |
| Hook 系统 | §5.5 第 4 条 |

### 明确不做

- **运行期动态加载**（`.so` / ABI / Manifest）—— `CLAUDE.md` 禁止跨动态库边界传 trait object；且与缓存经济学正面冲突（52×）。
- **`inventory` / `linkme` 式链接期注册** —— 顺序不确定，撞缓存前缀稳定。
- **branch / fork / 从 `details` 重放状态** —— 本仓无 branch，语义不成立。
- ~~**落盘持久化 todo**（`~/.yushan/tasks/`）~~ —— **已翻案**：ADR-0024 决定落盘，
  但落点是 `<sessions_dir>/<stem>.todo.json`（会话同级），**不是** `~/.yushan/tasks/`。
  翻案理由：长程任务必须活过压缩与重启；原否决是「为 todo 而 todo」，而长程需求让它成立。

---

## 7. 代价与风险

### 7.1 功能退化（必须如实说明）

**第三方仍不能在不改 `apps/coding-agent` 的前提下挂插件。** 这不是方案没做够，是两条硬约束的交集：①「工具面恒定」要求全集**编译期可知**；②`CLAUDE.md` 禁止跨动态库边界传 trait object ⇒ 运行期注册不可行。

因此**插件缝的上限 = 「不改 `ys-*` 核心 crate，但改 `apps/coding-agent` 的 `Cargo.toml` + 一行登记」**。本方案的贡献是把「6 处人工连锁点、散落 3 个文件」压成「1 个 crate + 1 行」——**不是消除登记**。若真要做到零改动装配，代价是接受运行期工具面 ⇒ 每轮 9,000 tok；那是个**产品决策**，不该由插件机制偷偷替用户做。

**显示缝同理未开**：第三方工具若要不塌成一行，仍需有人给 `CodingView` 加字段 + 写 `draw.rs`。这是「数据/显示分离」原则的直接后果 —— 在本仓，**显示是一等产品决策，不是插件的权利**。这与 pi 的设计哲学**恰好相反**，是有意的。

### 7.2 真正的风险：重构会**静默**改写缓存前缀

见 §6 P0。**缓解手段只有 prefix 指纹测试** —— 且它有两个已知盲区，均已覆盖：默认锚点是空 catalogue，覆盖不到 skill 段（P0 补第二个锚点）；测试若不隔离 `HOME`，锚点随机器而变、失去意义（P0 显式隔离）。

**最具体的风险面已定位（`/arch-validate`）**：P1 步骤 3 若按**工具 order** 展开 guideline，会静默重排成 `read, write, edit, …`（今天是 `edit` 组在最前，`prompt.rs:308`），且会把 `grep` 块从 read 组内部拽出来。这不是假想 —— 它是「照字面实现 §6 P1」的**默认结果**。⇒ §5.2 用**全局 `slot`** 表达既成事实。

### 7.3 其他

- **`EXPECTED_ORDER` 降为派生量会削弱一层交叉校验** —— 补偿：**独立字面量落在测试侧**，且 `prompt.rs` 的测试也改用它（它今天引用的是 `toolset::EXPECTED_ORDER`，`prompt.rs:279`，两侧本就是同源）。见 §6 P1 第 4 条。
- **单工具 spec 的 400 B 预算会被绕过** —— 必须留显式豁免通道，否则约束要么被整体删除、要么挡住正当的长 description。
- **`Fixed_prompt_tokens` 的账本自动正确**：插件只要走「工具 + snippet + guideline」这条既有通路，`main.rs` 的 `fixed_prompt_tokens` 计算自动把它算进去 —— 这是本设计最省事的一点。
- ~~**todo 状态会被压缩摘要掉**~~ —— **已由 ADR-0024 关闭**：真值在外部文件，`should_inject` 的
  「标记丢失就补」判据让压缩后自动补回，**零核心改动**。本行原缓解（「最后一条调用仍在
  `arguments` 里，模型可重读」）已作废 —— 长任务越长那条调用越早变成旧消息。

---

## 8. 附带发现（与本设计正交，建议单独修）

1. **`fixed_prompt_tokens` 注释低估 1.9×**：`basic.rs:618` / `limits.rs:20` 写「9 工具时缺口约 1.1k」，实测 **2,084**（§4.2）。建议订正为实测值 + 附测量方法。
2. **项目上下文重复加载（真 bug）**：`prompt.rs` 的 `load_project_context` 按**路径**去重（`seen.contains(path)`），而本仓 `CLAUDE.md -> ./AGENTS.md` 是**软链** ⇒ 同一份 27,804 B 的内容被加载**两次**（≈55.6 KB / ~18k est token，是工具面的 **9 倍**）。这使「第 20 个工具」相对项目上下文成为舍入误差，也说明**fix 它的收益远大于任何插件缝优化**。建议提独立 issue。

---

## 9. 参考

- 上下文：[`context.md`](./context.md)
- **延伸讨论（设计定稿后）**：[`notes.md`](./notes.md) —— todo 完成度保障 / pi 为何有 hook / 控制流是否该统一 / 第三方如何插既有缝，末尾附 12 项待办
- 决策：[`../../adr/0021-plugin-seam-without-hook-system.md`](../../adr/0021-plugin-seam-without-hook-system.md)
- 判据：[`../../extension-points.md`](../../extension-points.md)、[ADR-0014](../../adr/0014-follow-up-without-hook-system.md)
- 相邻（已设计未实现）：[`../../design-plans/2026-09-15-follow-up.md`](../../design-plans/2026-09-15-follow-up.md)
- as-built 先例：[`../../design-final/skills.md`](../../design-final/skills.md)（另一条「零新工具 / 顺带修压缩盲区」的路线）
- 参照实现（未入版本库）：`tmp/pi` 的 `packages/coding-agent/src/core/extensions/types.ts`、`examples/extensions/todo.ts`
