# 0021 — Plugin 缝开在装配层（`ToolPlugin`），不建 hook 系统、不引入 `details`

给「第三方 crate 怎样加一个功能（如 todo 清单）」这条需求定下**缝的位置** —— ① **不建 hook 系统**（维持 ADR-0014；含 todo 在内七个用例**仍零个**需要 Hook）；② **不引入 `ToolResult.details`**（判据是「UI 需要但模型不需要」，todo 不满足，却要付 JSONL 契约的破坏性变更）；③ 新增 `ys-tool::ToolPlugin { order, tools, prompts }`，把「加一个工具」的 **6 处连锁点收敛为 1 处登记**，插件自带 snippet/guidelines；④ 显示走 **`CodingView` 产品字段**（先例：`skills`），**不做**通用渲染注册表；⑤ todo 状态 = **最后一次调用的 `arguments`**（照 Claude Code TodoWrite 的「每次传完整列表」语义）。前置条件是补一条 **prefix 指纹测试**（今天没有任何测试锁工具面前缀字节）。

## Status

accepted（2026-09-23）

## 背景

- **需求原话**：「考虑 hook 的支持，方便实现 todo 清单之类 plugin 功能」。经澄清：本轮真正目标是**澄清 plugin 的含义与缝**；plugin 语义 = **静态优先**（第三方 Rust crate 编译期装配）；todo 只是试金石。
- **todo 在本仓既有判据下落 Tool 桶，不是 Hook**：`docs/extension-points.md:16-26` 的判断链第 2 条 ——「模型决定何时做 ⇒ Tool」。它是该文档列的**第 7 个用例**，**第 7 个仍然不需要 Hook**。ADR-0014 的触发门槛原话是「等**第二个真实消费者**出现再抽」，**依旧未满足**。
- **本仓今天没有「装配方 = 第三方」的缝**：工具表是 `apps/coding-agent/src/toolset.rs:94` 里一个 `vec![Box::new(..), ..]` 字面量。加一个工具要手改 **6 处**：`EXPECTED_ORDER`（`:40-53`）、`vec!` 字面量、三处 `assert_eq!(tools.len(), 9)`（`:226,:316,:493`）、`prompt.rs:117` 的 `tool_snippet` 分支、`prompt.rs:140` 的 `tool_guidelines` 分支。核心 crate（`ys-*`）**确实**不用改 —— 但也**只到这一层为止**。
- **量化（本机实测，见 `docs/arch/hooks-and-plugins/design.md` §4.2）**：今日 9 工具的每轮固定开销 = 工具表 JSON **6,517 B / 1,629 est token** + system prompt **1,823 B / 455 est token** = **2,084 est token**。加一个克制写法的 todo 工具 ≈ **+173 token/轮（+8.3%）**。
- **缓存经济学是总开关**：prompt cache 前缀按 `tools → system → messages` 分级，**任一级变化即该级及其后全部失效**。⇒ 工具面**每轮变动**的代价（取 `P ≈ 10k`）是 **+9,000 token/轮**，静态加一个工具的 **52 倍**。「工具面必须恒定」因此不是风格偏好，是定量约束。
- **参照实现 pi**（`tmp/pi`，本仓认定的范本）：`ExtensionAPI` 运行期注册 + 工具**自带 `renderCall`/`renderResult`** + 状态藏在 `AgentToolResult.details` 靠 `session_start`/`session_tree` 重放。**这条路线在本仓结构上不可行**：`ys-tool` 是核心 crate，`ys-tui-coding` 不依赖 `ys-runtime`（编译器强制）⇒ 工具拿不到 `Theme`/`Component`；且本仓**没有 branch/fork** ⇒ 重放的价值锚不存在。
- **Claude Code 的演进作为教训**：`TodoWrite` 的状态**活在对话上下文里**、**每次调用返回完整列表**（无部分更新逻辑）；2026-01 新增 `Tasks` 改为落盘，**理由正是 TodoWrite 不持久**（`/clear` 即失、压缩会摘要掉）。⇒「状态在对话上下文」与「状态持久」是**两种不同产品语义**，必须显式选一个。

## 决策

### 1. 不建 hook 系统（维持 ADR-0014）

七个用例（含 todo）逐条走判断链后**仍零个**需要 Hook（完整表见 design.md §5.5）。唯一合格形状是「加工**不是我们写的**工具的结果」—— **今天 0 消费者**。

**新增一条硬约束（供将来建 hook 时不再踩）**：若将来做「每回合注入」，**注入必须走 `messages` 尾部，绝不改 `system`**。`system` 是每轮整块重发的前缀，动它等于每轮废掉缓存（52× 代价）。

### 2. 不引入 `ToolResult.details`

判据：**该字段「UI 需要但模型不需要」**。todo **不满足** —— 模型必须看见列表才能推进工作。而代价是破坏性的：`ContentBlock::ToolResult`（`crates/ys-core/src/message.rs:29-33`）是 JSONL 落盘契约，且**变体上没有 `#[non_exhaustive]`**，加字段即 API 破坏 + 动对外序列化契约。

**翻案条件**（写下来，不到不动）：出现「渲染器要读、但模型不需要（甚至有害）」的状态，且实测 content 冗余 > **500 token/轮**。

### 3. plugin 缝开在装配层：`ys-tool::ToolPlugin`

```rust
pub struct ToolPlugin {
    pub order: u16,                      // 装配位次；**必须编译期常量**
    pub tools: Vec<Box<dyn Tool>>,
    pub prompts: Vec<ToolPrompt>,        // snippet + guidelines（工具自己声明怎么进 prompt）
}
```

- **收敛 6 处连锁点为 1 处**：第三方 crate 导出 `pub fn plugin() -> ToolPlugin`；装配方在 `plugins()` 里加一行 + `Cargo.toml` 加一依赖。
- **排序用显式常量 `order`**，**不用**注册顺序或名字序 —— 前者随依赖图/Cargo feature 变化，后者是隐式规则；两者都会让前缀字节漂移。
- **拒绝 `inventory` / `linkme` 式链接期注册**：顺序由链接器决定、不确定 ⇒ 直接撞缓存前缀稳定（`ys-tool/src/registry.rs:21-22` 已为同一理由拒绝 HashMap 迭代序）。
- 放 `ys-tool` 而非新 crate：第三方本就必须依赖 `ys-tool` 才能实现 `Tool`；**零新 crate**。

### 4. 显示走 `CodingView` 产品字段，不做通用渲染注册表

`CodingView`（`crates/ys-tui-coding/src/view.rs:43-69`）本就是**产品字段容器** —— 其模块注释原话：「各产品的视图字段不同（coding 看 provider/tokens/tools；投资会看净值/持仓）」，且已有 `skills: Vec<SkillEntry>` 这一**完全同型**的先例。

因此 todo 加一个 `todos: Vec<TodoItem>` 是**本仓的既有模式**，不是特例。相比之下一套通用渲染注册表（`ToolDisplay` / `Tone` / `RenderLine` / `DisplayRegistry`，4 个新概念）只有 **1 个消费者** —— 正是 ADR-0014 拒绝的那类设计。

**代价如实记录**：第三方工具若要不塌成 transcript 一行，仍需有人给它加 `CodingView` 字段 + 写 `draw.rs`。在本仓，**显示是一等产品决策，不是插件的权利** —— 这与 pi 的哲学**恰好相反**，是有意的。

### 5. todo 状态 = 最后一次调用的 `arguments`

照 Claude Code `TodoWrite` 的「**每次调用传完整列表**」语义 ⇒ **最后一次调用的 `ToolUse.arguments.todos` 就是当前状态**。UI 侧 `latest_todos()` 反向扫 `session_messages()` 即得。

零 `details`、零重放、零新事件、零核心改动。副产品：`/new` 换 `Wiring` ⇒ 列表自然归零；重启恢复会话 ⇒ 列表自然恢复。

### 6. 前置条件：prefix 指纹测试

**今天 `prompt.rs` 没有任何测试锁「9 工具产出的前缀总字节」**（现有守卫只锁形状与部分文案）。把 snippet/guidelines 表搬进 `ToolPlugin` 时，一处空格/一次 `trim` 都会改变所有用户的前缀 ⇒ 缓存全废 ⇒ 每轮多付 ~9,000 token，**而 832 条测试依然全绿**。

故 **P0 先钉 `sha256(tools_json)` + `sha256(system_prompt)` 双锚点，再做搬家**。这不是可选项。

## Considered Options

- **方案甲：最小复杂度**（无状态 `TodoTool` 独立 crate + 16 行装配，显示塌成一行）。**部分采纳，但不完整** —— 它不动装配缝，下一个插件仍要手改 6 处；且**用户根本看不见清单**（`draw.rs:490-499` 只在失败时显示结果首行）。它的克制（能不加概念就不加）作为本方案的护栏保留。
- **方案乙：可扩展优先**（`ys-ext::Extension` + `ys-protocol::ToolDisplay` 双缝，270 行 / 5 个新概念）。**否决「通用渲染注册表」部分** —— 1 个消费者撑不起 4 个新概念，与 ADR-0014 的立场冲突。它的**分阶段纪律**（P1 零行为变化、可独立发布、可回滚）被采纳。
- **立即建 hook 系统**（照 `design.md` §6 落地 9 个点）。**否决** —— 消费者数仍为 0；且会把「每回合改请求」的诱惑制度化，而那正是缓存经济学的头号杀手。
- **用 MCP 式运行期工具发现**（`tools/list` + `list_changed`）。**否决** —— 与「工具面恒定」正面冲突（52× 代价）；且 `CLAUDE.md` 禁止跨动态库边界传 trait object。
- **落盘持久化 todo**（照 Claude Code `Tasks`，`~/.yushan/tasks/` + 依赖图）。**否决** —— 那是另一个量级的产品特性，不是「加一个工具」；且引入第二个真值源（文件 vs 会话），使 `/new` 语义立刻含糊。
- **`ToolResult.details`**：见决策 2。

## Consequences

- **ADR-0014 继续有效**，其「等第二个真实消费者」的门槛**仍未满足**。本 ADR 补充了它缺的一条：**若将来做 hook，注入必须走 `messages` 尾部而非 `system`**。
- **`design.md` §6 保持推迟状态**（`:291-293` 的状态注继续有效）。
- **插件缝的上限被明确写死**：**「不改 `ys-*` 核心 crate，但改 `apps/coding-agent` 的 `Cargo.toml` + 一行登记」**。零改动装配需要运行期工具面 ⇒ 每轮 9,000 token —— 那是**产品决策**，不该由插件机制替用户做。这个边界写进文档，避免后人误以为「已经支持插件了」。
- **加一个工具的连锁点从 6 处降到 1 处**，且 snippet/guidelines 不再散落在 `prompt.rs` 的两张手抄表里（`EXPECTED_ORDER` 与 `vec!` 的「改一处必须改两处」随之消失）。测试侧保留一份**独立硬编码**的名字序列，维持装配端/测试端两份真值的交叉校验。
- **补记（2026-09-23，`/arch-validate` → `design.md` §5.2）**：决策 3 的 `ToolPlugin` / `ToolPrompt` 需要**两条独立的序** —— `ToolPlugin.order` 定**工具**序（`read, write, edit, …`），`ToolPrompt.order` 定 **guideline** 序（`edit, write, read, …`）。两者今天确实不同（`toolset.rs:40-50` vs `prompt.rs:143-189`）。并成一条 = 一次全局前缀漂移，会毁掉「P1 搬完家指纹测试仍绿」这个唯一机械证明。
- **`fixed_prompt_tokens` 账本自动正确** —— 插件只要走「工具 + snippet + guideline」这条既有通路，`main.rs` 的计算自动把它算进去。
- **已知缺口（明确接受）**：todo 会被压缩摘要掉（与 `docs/design-final/skills.md` 的 R6 同型）；本轮不做「todo 免疫压缩」（那需要给 session 加保留标记，是新的核心契约）。
- **本轮顺带订正两处既有失真**（与本决策正交，建议单独修）：
  1. `basic.rs:618` / `limits.rs:20` 写「9 工具时缺口约 1.1k」，**实测 2,084**（低估 1.9×）；
  2. `prompt.rs::load_project_context` 按**路径**去重，而本仓 `CLAUDE.md -> ./AGENTS.md` 是软链 ⇒ 同一份 27,804 B 内容被加载**两次**（≈18k est token，是工具面的 9 倍）。**修它的收益远大于任何插件缝优化。**
- 设计全文（含成本模型、候选对比、分阶段落地）：`docs/arch/hooks-and-plugins/design.md`；上下文：同目录 `context.md`。
