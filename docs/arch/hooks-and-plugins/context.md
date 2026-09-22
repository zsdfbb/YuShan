# Hook 支持 / Plugin 缝 — 架构上下文

> 产出：`/arch-explore`（2026-09-23）。用途：为「**考虑 hook 的支持，方便实现 todo 清单之类 plugin 功能**」
> 界定现状、约束与范围，供后续 `arch-design` 使用。证据均带 `文件:行号`，对应本工作区 `master`（`02a3b55`）。
>
> **用户已澄清的三点**（本轮问答）：
> 1. **本轮真正目标 = 澄清「plugin 的含义与缝」**，不预设 hook 或 tool（todo 只是试金石）；
> 2. **plugin 语义 = 静态优先**（第三方 Rust crate 编译期装配），动态加载后续再考虑；
> 3. **todo 的界面形态与提醒注入方式「先不定，看设计结论」**，并反问「**pi 的工具是如何实现的？**」。

---

## 概述

一句话：**todo 清单在本仓的既有判据下落「Tool」桶，不是 Hook**（[`docs/extension-points.md`](../../extension-points.md) 判断链 +
[ADR-0014](../../adr/0014-follow-up-without-hook-system.md)）。用户真正想问的是**更上一层**的问题：
「**第三方/AI 怎样在不改核心代码的前提下，往这个 Agent 里加一个功能**」——todo 只是拿来做试金石的那块石头。

因此本文的重心是：**① 抄 pi 的作业（用户直接问的）；② 把本仓今天的「缝」逐条点出来并量化代价**。
结论先行：

- **pi 的答案是 `ExtensionAPI`**：工具是「**四件套 + 自带渲染**」的对象，状态藏在**工具结果的 `details`** 里，
  生命周期靠 `pi.on(...)` 的 30+ 个事件；扩展从**目录扫描 + 运行期 import**。
- **本仓今天没有「装配方 = 第三方」的缝**：工具表是 `toolset::build_tools` 里一个 `vec![...]` 字面量
  （`apps/coding-agent/src/toolset.rs:94`），加一个工具要动 ~6 处、且必须改 `apps/coding-agent`。
  核心 crate（`ys-*`）**确实**不用改 —— 但也**只到这一层为止**。
- **五条缝**按「todo 是否必须」排：① 加工具（必须）、② 工具自带显示（可选）、③ 结构化状态 `details`（可选）、
  ④ 会话生命周期事件（可选）、⑤ 每回合注入（可选，**这是唯一真正的 hook 形状**）。
- **最大的设计张力**：pi 让工具**自带渲染器**（`renderCall`/`renderResult`）；本仓因
  **crate 边界 + 数据/显示分离**（`ys-tool` 是核心 crate，`ys-tui-coding` 不依赖 `ys-runtime`），
  **结构上不可能**照抄。等价物只能是「**`ToolResult.details` + UI 侧渲染注册表**」。

---

## 用户直接问题的答案：pi 的工具与扩展模型

> 参照实现：`/Users/zs/Develop/YuShan/tmp/pi`（**未入版本库**，`tmp/` 已被 `.gitignore`；行号对应本次快照）。
> 这也是 [ADR-0014](../../adr/0014-follow-up-without-hook-system.md) 认定的「**真·控制流 hook 的范本**」。

### 1. 工具 = 一个「四件套 + 渲染器」对象（不是 trait 的两个方法）

```ts
// packages/coding-agent/src/core/extensions/types.ts:451  ToolDefinition
pi.registerTool({
  name, label, description,
  promptSnippet?, promptGuidelines?,   // ← 系统提示的接入点（工具自己声明怎么被写进 prompt）
  parameters,                          // TypeBox schema（运行期校验）
  execute(toolCallId, params, signal, onUpdate, ctx),  // ← 逻辑
  renderCall?(args, theme, context),   // ★ 调用行的 UI 渲染
  renderResult?(result, opts, theme, context),  // ★ 结果区的 UI 渲染
  executionMode?,                      // 串行 / 并行
  renderShell?: "default" | "self",    // 用框架外壳还是自绘
})
```

**关键点：显示不由核心 TUI 决定，由工具自带。** todo 的复选框列表、进度 `3/7`、
「… N more」折叠，全部写在扩展自己的 `renderResult` 里
（`packages/coding-agent/examples/extensions/todo.ts` 的 `renderResult`，约 200 行）。

### 2. 状态藏在**工具结果的 `details`** 里，靠事件重放重建

```ts
// todo.ts 顶部注释原文
// "State is stored in tool result details (not external files), which allows
//  proper branching - when you branch, the todo state is automatically correct
//  for that point in history."
```

- `AgentToolResult = { content, details?: JsonValue }` —— 证据：harness 的 `after_tool` hook
  事件把两者**并列**列出（`packages/agent/src/harness/agent-harness.ts:477-484`）。
- 扩展在内存里维护 `todos`，**启动期 / 切换分支时**从 session 重放：
  ```ts
  pi.on("session_start", async (_e, ctx) => reconstructState(ctx));
  pi.on("session_tree",  async (_e, ctx) => reconstructState(ctx));
  // reconstructState：遍历 ctx.sessionManager.getBranch()，只取 toolName === "todo"
  //                   的 toolResult，读它的 details，按序覆盖内存状态
  ```
  —— 这份「**从历史重放重建状态**」的模式，与 pi 的 **branch/fork** 强绑定（本仓**没有** branch/fork）。

### 3. 生命周期 = `pi.on(event, handler)`，30+ 个点

`packages/coding-agent/src/core/extensions/types.ts:1252` 的 `ExtensionAPI` 列出的订阅点分四族：

| 族 | 例子 | 带 result？（能否改流程） |
|---|---|---|
| session | `session_start` / `session_before_compact` / `session_before_switch` / `session_tree` | 部分带（可 cancel / 定制） |
| agent/turn | `before_agent_start` / `agent_end` / `turn_start` / `turn_end` | 部分带 |
| message | `message_start` / `message_update` / `message_end` | `message_end` 带 result |
| **provider/tool** | `before_provider_request` / `after_provider_response` / **`tool_call`** / **`tool_result`** / `input` / `context` | 带（transform / control） |

另有一套**更底层的 harness hook**（`packages/agent/src/harness/agent-harness.ts:430` 的 `HookMap`，11 个点）：
`before_run` / `before_drive` / `before_run_end`（→ `{ followUp?: string }`！）/ `transform_context` /
`before_request` / `before_payload` / `after_response` / `before_tool` / `after_tool` /
`before_compaction` / `before_navigation`。

> **注意**：`before_run_end` 的 result 是 `{ followUp?: string }` —— 这正是本仓
> [follow-up 设计](../../design-plans/2026-09-15-follow-up.md)（**尚未实现**）想做的事。
> pi 是把它做进了 harness hook，本仓设计成 `app_loop` 里的一个函数。

### 4. 装配：目录扫描 + 运行期 import

- 发现顺序（`packages/coding-agent/src/core/extensions/loader.ts:710-720, 754-777`）：
  ① `cwd/.pi/extensions/*.ts`（或 `index.ts` / 带 `pi` 字段的 `package.json`）→ ② 用户级目录。
- **Node/TS 生态天然支持运行期加载**，故 pi 的「动态」是零成本的；这正是本仓**没有**的东西。

### 5. 一句话对比

| 维度 | pi | YuShan today |
|---|---|---|
| 加工具 | `pi.registerTool({...})` 运行期一行 | `toolset::build_tools` 的 `vec![...]` 字面量（编译期） |
| 工具显示 | **工具自带** `renderCall`/`renderResult` | `transcript.rs` 的**通用摘要规则**（`SUMMARY_KEYS`），工具无权干预 |
| 结构化状态 | `AgentToolResult.details`（入 session、可重放） | `ToolResult { content, is_error }` —— **无 details** |
| 每回合注入 / 改请求 | `before_provider_request` / `transform_context` / `before_run_end` | **无**（`system_prompt` 启动期定死） |
| 生命周期 | `pi.on(...)` 30+ 点 | `AgentEvent` 10 个变体，**只观察** |
| 装配 | 目录扫描 + 运行期 import | crate + Cargo feature（静态），装配点在 `apps/coding-agent` |

---

## 现有架构（本仓）

### 模块边界

```
ys-core / ys-event / ys-protocol        （零 tokio，纯数据 + 纯 trait）
   ↑
ys-model / ys-tool / ys-session / ys-component
   ↑
ys-loop                                  （BasicLoop；压缩在轮边界）
   ↑
ys-runtime                               （AgentBuilder + 无状态 Agent + AgentPorts）
   ↑
apps/coding-agent                        （接线器：三信道 + 两线程 + TUI）
   └── ys-tui-coding                     （独立 crate，**不依赖 ys-runtime**）
```

**所有「加工具」的动作都发生在 `apps/coding-agent`**，核心 crate 一动不动 —— 这就是
「静态组合优先」的兑现方式。代价见下。

### 核心抽象

| 抽象 | 位置 | 形状 |
|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs:29-35` | `spec() -> ToolSpec` + `call(Value, ToolContext) -> Result<ToolResult, ToolError>`（**仅此二法**） |
| `ToolResult` | `crates/ys-core/src/tool.rs:14-17` | `{ content: String, is_error: bool }` |
| `ContentBlock::ToolResult` | `crates/ys-core/src/message.rs:29-33` | `{ tool_call_id, content, is_error }` —— **会话里真正落的形态** |
| `ToolContext` | `crates/ys-tool/src/context.rs:7-18` | `{ boundary, cwd, workspace_root, summarizer }` |
| `RuntimeContext` | `crates/ys-component/src/context.rs:11-25` | `{ model, registry, session, events, limits, cwd, workspace_root, approval, system_prompt, boundary }` |
| `Agent`（无状态） | `crates/ys-runtime/src/agent.rs:55-63` | `{ loop_impl, registry, limits, cwd, workspace_root, approval, system_prompt }` |
| `AgentEvent` | `crates/ys-event/src/event.rs:6-67` | 10 个变体，`#[non_exhaustive]`，**闭合枚举** |

### 关键数据流（加一个工具的现状）

```
toolset::build_tools()  ── vec![read, write, edit, bash, grep, find, ls, websearch, webfetch]
        │
        ├─→ ToolRegistry    → Agent.registry → run_turn → ctx.registry.specs()
        ├─→ build_system_prompt(tools)  → "- {name}: {snippet}" + guidelines
        └─→ agent.tool_names() → build_view().tools → TUI 状态行
        │
   工具执行 → ToolResult{content,is_error} → ContentBlock::ToolResult → session
                                          → AgentEvent::ToolResult → TUI transcript
```

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `serde` / `serde_json` | 全部核心类型的序列化 | `ContentBlock` 落 JSONL 会话 |
| `async-trait` | `Tool` / `Model` / `Session` / `AgentLoop` | |
| `tokio` | `ys-session` / `ys-loop` / `ys-runtime` | 核心三 crate 零 tokio |

---

## 差距：本仓今天的「缝」有哪几条

> 判据统一为：**「不改 `ys-*` 核心 crate」之外，还允许改谁？** 今天答案是「必须改 `apps/coding-agent`」。

### 缝 ①：加一个工具（todo **必须**）

改动面（照 skills context 的点算方法，`docs/arch/skills/context.md` D1 推论 1）：

| 落点 | 内容 |
|---|---|
| `apps/coding-agent/src/toolset.rs:40-53` | `EXPECTED_ORDER` 常量（**现为 9 元**） |
| `apps/coding-agent/src/toolset.rs:94` | `build_tools` 里 `vec![...]` 字面量（**与常量人工对齐**） |
| `apps/coding-agent/src/toolset.rs:226, 316, 493` | 三处 `assert_eq!(tools.len(), 9)` |
| `apps/coding-agent/src/prompt.rs:117` `tool_snippet` | 新工具的提示词片段（**测试 `every_real_toolset_tool_has_non_fallback_snippet` 强制补**） |
| `apps/coding-agent/src/prompt.rs:140` `tool_guidelines` | 新工具的 guideline 段 |
| 新 crate / 新模块 | `Tool` impl + `ToolSpec`（含 JSON Schema） |

**结论**：加工具**不需要 hook**，今天就能做；痛点是**连锁点分散**（6 处手改、两处「改一处必须改两处」），
且**装配权在 `apps/coding-agent` 手里，不在第三方手里**。

### 缝 ②：工具自带的显示（todo **可选**）

- 今天：`crates/ys-tui-coding/src/transcript.rs:56` 的 `summarize_tool_args` 按固定的
  `SUMMARY_KEYS = ["path","file_path","command","pattern","query","url"]` 抽一行摘要，**工具无权干预**。
- todo 若接受「**塌成 transcript 一行**」（如 `todo add "写测试"`），**今天就能跑通，零 TUI 改动**。
- 若要 **常驻面板 / 复选框 / 进度**，则触及 `ys-tui-coding::CodingView`（`crates/ys-tui-coding/src/view.rs:43-69`，
  **字段闭合**）+ `draw.rs`（1480 行）—— 即 **TUI 与 todo 功能编译期耦合**。
- **结构约束**：**不能**照抄 pi 的「工具自带 render」——
  `ys-tool` 是核心 crate，`ys-tui-coding` 不依赖 `ys-runtime`（编译期强制），
  工具**不可能**拿到 `Theme` / `Component`。等价物只能是「**`details` + UI 侧「工具名 → 渲染器」注册表**」。

### 缝 ③：结构化状态 `details`（todo **可选**）

- 今天 `ToolResult` 只有 `content: String` —— 结构化状态**只能**：塞 JSON 进 content 字符串（模型能读、UI 只能当文本）
  或写外部文件（丢「随会话回滚」语义）。
- pi 的 `details` 入 session、可重放。本仓要等价物，需**同时**改：
  `ys-core::ToolResult` + `ys-core::ContentBlock::ToolResult`（**落盘形态**）+ `AgentEvent::ToolResult`
  + loop 里的转换 + TUI 消费。**这是触及 `ys-core` 的改动，且 `ContentBlock` 是 JSONL 契约**
  （`crates/ys-core/src/lib.rs` 的 serde 契约测试是既有先例，`StopReason::Truncated` 那条注释写得很清楚）。
- **注意**：`ContentBlock::ToolResult` 变体上**没有** `#[non_exhaustive]`，加字段是**破坏性 API 变更**
  （只有枚举整体标了 `#[non_exhaustive]`，`crates/ys-core/src/message.rs:19-20`）。

### 缝 ④：会话生命周期事件（todo **可选**，仅当要「重放重建」）

- 今天 `AgentEvent` **没有** `SessionStarted` / `TurnStarted` / `SessionLoaded` —— 只有
  `UserMessage` / `ModelTextDelta` / `ToolCall` / `ToolResult` / `RunFinished` / `RunFailed` /
  `ContextCompacted` / `ContextCompactionFailed` / `ResponseTruncated`（10 个）。
- pi 的 `reconstructState` 挂在 `session_start` / `session_tree` 上。本仓若要等价物，
  得先有「会话开始」事件 —— 或者换个思路：**todo 状态直接从 `wiring.session_messages()` 里扫**
  （`apps/coding-agent/src/view.rs:91` 已有 `message_count` 的用法先例）。

### 缝 ⑤：每回合注入 / 改请求（唯一**真正的 hook 形状**）

- 今天 `system_prompt: Option<String>` 在 `AgentBuilder::system_prompt`（`crates/ys-runtime/src/builder.rs:67`）
  **启动期定死一次**，`RuntimeContext` 只是 clone 它（`crates/ys-runtime/src/agent.rs:111`）。
- 要在每轮请求里注入「你还有 3 项 todo 未完成」，今天**无落点** —— 这正是 `design.md` §6 的
  `before_model_request`（Transform）、也是 pi 的 `transform_context` / `before_provider_request`。
- **这是「hook 系统」在本轮唯一站得住的理由**。但注意：[`docs/extension-points.md`](../../extension-points.md)
  的判据是「**Hook 只在「不能改那个工具、却又要加工它的结果」时不可替代**」——
  todo 工具**是我们自己的**，它完全可以自己在 `call()` 的返回值里写清楚状态；
  真正需要的是「**跨轮**把它送回去」，而**工具返回的结果本身就是会话消息，下一轮模型自然看得到**。
  ⇒ **即使缝 ⑤，todo 也不构成 hook 的第二个消费者**。

### 小结算义表

| 缝 | todo 是否必须 | 触及核心 crate？ | 今天能否绕开 |
|---|---|---|---|
| ① 加工具 | **必须** | 否（只动 `apps/`） | 不能（6 处连锁点） |
| ② 自带显示 | 可选（常驻面板才要） | `ys-tui-coding` | 能（塌成一行） |
| ③ `details` | 可选 | **`ys-core`**（JSONL 契约） | 能（content 塞 JSON / 外部文件） |
| ④ 生命周期事件 | 可选 | `ys-event` | 能（扫 session messages） |
| ⑤ 每回合注入 | 可选 | `ys-component` / `ys-runtime` | **不能**（无落点） |

---

## 约束

- **技术**
  - **依赖方向单向**（`CLAUDE.md` 硬性约束）：`ys-core` ← 组件接口 ← loop/runtime ← 应用适配器。
    `ys-core` / `ys-event` / `ys-protocol` **零 tokio**；`ys-tui-coding` **不依赖 `ys-runtime`**（编译器强制）。
  - **跨动态库边界不传 Rust trait object / Tokio 类型**（`CLAUDE.md`）⇒ 真动态插件（路线图第 4 步）成本极高，
    与本轮「静态优先」一致地**排除**。
  - **工具面必须恒定**（`apps/coding-agent/src/toolset.rs` 注释 + skills context）：工具不得按登录态/配置增减，
    否则模型会把「工具消失」误读成「我记错了」，且会废掉 prompt cache 前缀。
  - **`ContentBlock` 是 JSONL 会话落盘的对外契约**（`crates/ys-core/src/lib.rs` 的 serde 契约测试）。
- **性能**
  - 每轮固定开销 = system prompt + 工具表，**每次请求都重发**；`RunLimits::fixed_prompt_tokens`
    （`crates/ys-loop/src/basic.rs:147`）就是它的入口，且**与压缩阈值耦合**（ADR-0017）。
    新工具的描述/guideline 直接计入这块固定成本。
  - skills 上线后 skill 目录常驻 ~9.6k token ⇒ 留给对话的预算已被压低（`docs/design-final/skills.md`）。
- **演进**
  - 本仓**没有** branch / fork（pi 有）⇒ 缝 ④ 的「分支正确性」论据在本仓**不成立**，
    不应照抄 `details` + 重放的复杂度。
  - follow-up（`docs/design-plans/2026-09-15-follow-up.md`）**已设计、未实现**（`apps/coding-agent/src/` 下
    **无** `follow_up.rs`）。ADR-0014 称它是「**可长成 hook 的 seam**」——本轮若动 hook，须先与之对齐。
  - ADR-0014 的触发条件原文：**「等第二个真实消费者出现再抽」**。todo 是第 7 个用例
    （extension-points.md 已列 6 个），且**第 7 个仍然落 Tool 桶**。
- **组织**
  - 单人维护（git user: Zhang Shuai）；「不设计假想未来需求」是明文原则（ADR-0014 / `docs/extension-points.md`）。

---

## 需求范围

### 「plugin」在本仓的三种可能含义（待选）

| 含义 | 本仓对应物 | 今天状态 |
|---|---|---|
| **A. 第三方 Rust crate 提供工具**（用户选定的语义） | crate + Cargo feature 静态组合；但**装配点**在 `apps/coding-agent` | 能加工具，但**装配权没开放**（缝 ①） |
| **B. 功能可插拔（口语）** | 就是「加一个能力」，判据走 `docs/extension-points.md` | 可用 Tool / Event / Follow-up 承载 |
| **C. 运行期动态加载** | `design.md` §8（Manifest / ABI / `.so`） | 路线图第 4 步，**本轮排除** |

### 范围内（本轮 arch-design 应回答的）

- [ ] **「缝」到底开到哪一层**：只到「不改 `ys-*` 核心 crate」，还是到「不改 `apps/coding-agent`」？
      （前者今天已成立；后者需要把工具装配从 `vec![...]` 字面量抽成**可扩展的注册入口**）
- [ ] **todo 作为试金石**：给出一个**最小可跑**的 todo 方案，并标注它踩了上面哪几条缝
- [ ] **缝 ② 的等价物设计**：`details` + UI 侧渲染注册表 vs 塌成一行 —— 二选一并给判据
- [ ] **是否引入 `ToolResult.details`**（含 `ContentBlock::ToolResult` 的 JSONL 兼容策略）
- [ ] **hook 系统的去留**：维持 ADR-0014 的推迟，还是因为缝 ⑤ 而重新评估（**需要给出消费者论证**）

### 范围外（明确不做的）

- **运行期动态加载**（用户已选「静态优先，动态后续再考虑」；`CLAUDE.md` 亦禁止跨动态库边界传 trait object）。
- **照抄 pi 的「工具自带 render」** —— 结构性不可能（crate 边界 + 数据/显示分离）。
- **照抄 pi 的 branch/fork 语义** —— 本仓无 branch。
- **Plan 模式 / TodoWrite** 曾被列为 skills 的**明确非目标**（`docs/arch/skills/context.md:189`），
  本轮若要翻案，需显式记录理由。

### 关键场景

- 场景 1：用户向 agent 说「把这件事拆成 todo 并开始做」→ 模型调 `todo` 工具 → 列表可见 → 逐项推进打勾。
- 场景 2：会话重启 / `/new` 后，todo 列表是否还在？（取决于状态承载 —— 本轮未定）
- 场景 3：第三方（或 AI 自己）**不改 `apps/coding-agent`** 就挂上一个新工具。（**今天做不到**）

---

## 未澄清问题

- [ ] **Q1（最关键）**：本轮「不改核心代码」的边界在哪？
      「只不改 `ys-*`」今天已成立，那「探索新缝」就只剩缝 ②③⑤；
      「连 `apps/coding-agent` 也不改」才是真正的**插件缝**，但那要先定义「谁来装配」。
- [ ] **Q2**：todo 的显示形态度量 —— 接受**塌成 transcript 一行**吗？（接受则缝 ② 可整体跳过，
      改动面缩到只剩缝 ①）
- [ ] **Q3**：todo 状态要不要**跨 `/new`、跨重启**保留？（要 → 缝 ③/④ 上升为必须；不要 → 内存态即可）
- [ ] **Q4**：是否引入 `ToolResult.details`（破坏性改 `ys-core` + JSONL 契约）？
- [ ] **Q5**：缝 ⑤（每回合注入）要不要做？做了就要定义「谁在什么时候注入什么」，
      这是 hook 系统的**唯一入口**，也是最容易滑向「建一套通用框架」的地方。
- [ ] **Q6**：本轮与**已完成设计但未实现**的 follow-up（`docs/design-plans/2026-09-15-follow-up.md`）
      是何关系？先实现 follow-up 再谈 hook，还是并行？

---

## 后续建议

- **建议用 `arch-design` 做方案对比**，且把「**装配缝 ① 的开放度**」当作**第一决策点**——
  它决定本轮是「加一个功能」还是「改一层架构」。
- 建议至少对比三个候选（`docs/extension-points.md` 的判据应作为筛选器，而非结论）：
  - **甲：最小落点** —— 只加 `todo` 工具（缝 ①），显示塌成一行，状态在会话消息里。
    零核心改动，**证明「静态组合够用」**。
  - **乙：把缝 ① 抽成注册入口** —— `build_tools` 从字面量改为「内置工具 + 外挂工具列表」，
    第三方可注入；顺带把 `EXPECTED_ORDER` / prompt snippet 的连锁点收敛。
  - **丙：引入 `details` + UI 渲染注册表**（缝 ②③）—— 真正的「插件缝」形状，但触及 `ys-core`。
- **若要动 hook，先回答「第二个消费者是谁」**（ADR-0014 的原文门槛）。todo **不是**。
- 验证假设可考虑 `prototype`（先在 `apps/coding-agent` 里跑通一个最土的 todo 工具，看真实痛点落在哪条缝）。

---

## 参考

- 判据：[`docs/extension-points.md`](../../extension-points.md)（四者边界 + 判断链 + 六个用例）
- 决策：[`docs/adr/0014-follow-up-without-hook-system.md`](../../adr/0014-follow-up-without-hook-system.md)（为何推迟 hook；两条已探明约束）
- 规范：[`docs/design.md`](../../design.md) §6（9 个 hook 点，**有意未实现**）、§8（动态插件）、§10（Coding Hooks）
- 术语：[`docs/CONTEXT.md`](../../CONTEXT.md)（Hook / Hookpoint / Handler / Hook Plugin 定义）
- 未实现的相邻设计：[`docs/design-plans/2026-09-15-follow-up.md`](../../design-plans/2026-09-15-follow-up.md)
- 加工具的连锁点先例：[`docs/arch/skills/context.md`](../skills/context.md)（D1 推论 1）
- as-built：[`docs/design-final/skills.md`](../../design-final/skills.md)、[`docs/design-final/coding-agent-tui.md`](../../design-final/coding-agent-tui.md)
- 参照实现（**未入版本库**）：`tmp/pi` 的
  `packages/coding-agent/src/core/extensions/{types.ts,loader.ts}` 与
  `packages/coding-agent/examples/extensions/todo.ts`、`packages/agent/src/harness/{hooks.ts,agent-harness.ts}`
