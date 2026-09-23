# Hook / Plugin 缝 — 延伸讨论记录

> 日期：2026-09-23。承接 [`design.md`](./design.md) 与 [ADR-0021](../../adr/0021-plugin-seam-without-hook-system.md)。
> 记录设计定稿后的四轮追问（todo 完成度保障 / pi 为何有 hook / 控制流是否该统一 / 第三方如何插既有缝）。
>
> **状态：仅记录，未落地。** ①②③ 结论已稳定（可直接写成 ADR）；④⑤ 是待办（已排期，未做）。

---

## ① todo 不持久化，如何确保按计划完成

**核心校正：持久化 ≠ 确保完成，两者正交。** 用本仓词汇：持久化是 Component（状态），确保完成是 Hook/Loop（控制流）。**被持久化的停滞计划仍然是停滞的。**

| 层 | 机制 | 本仓 | 证据 |
|---|---|---|---|
| L1 可见性 | 模型每轮看得见清单 | ✅ 有 | 工具结果落会话（`basic.rs:549`），每轮整份重发（`basic.rs:250`）；`arguments` 也在 assistant 消息里 |
| L2 提醒（nudge） | harness 注入「还有 N 项未完成」 | ❌ 无落点 | 即缝⑤；`system_prompt` 启动期定死（`agent.rs:111`） |
| L3 强制 | 回合不完整不许结束 | ❌ 无 | **但已设计** —— 见下 |
| L4 持久性 | 跨压缩 / 跨会话 / 跨重启 | ⚠️ 部分 | 跨重启免费（会话 JSONL 恢复）；跨压缩看运气；跨 `/new` 明确归零 |

**结论：L3 是「确保」的唯一机械化落点，而它就是已设计未实现的 follow-up seam。**

把触发条件从「HEAD 变化」换成「**存在未完成项**」即可（`docs/design-plans/2026-09-15-follow-up.md` §3.5 的扩展轴①本来就预留了这条）。**不需要 hook 系统** —— 判断链走下来落 **Follow-up 桶**，且 follow-up 住 `app_loop`（回合之间），正是 ADR-0014 指定的位置。三样护栏（防套娃 / 开关 / 上限）可直接继承。

**天花板（必须诚实）**：todo 是模型的**自陈**，不是**验收**。能强制的只有「再问一次」，不能强制「真的做完」。真正的保证来自可验证的验收条件（测试绿、构建过），不是清单本身。

**压缩风险的精确形态**：保留段按 token 预算取尾部（`compact.rs:86-127`），且保留段首条不得含 `ToolResult`（否则 `return None` 不压）。状态取自**最后一次**调用 ⇒ 只要它落在保留段内就安全；风险只在「最后一次更新后又干了很多轮」时出现 —— 而那恰是最该提醒模型的时刻，反而支持 L2/L3。

**pi 的范本**：`plan-mode` 扩展用六个钩子点拼出来（`tool_call` 禁写工具 / `context` 注入 / `before_agent_start` 改 prompt / `turn_end` 跟踪进度 / `agent_end` 收尾 / `session_start` 重建状态）。本仓若不做 hook，**做不到它那种「回合内直接禁写」的强约束**，只能到 follow-up 级别。

---

## ② pi 为何提供 hook

实测 pi 仓库的 **79 个扩展示例**，统计各 hook 事件被多少扩展使用：

| 事件 | 数 | 类别 |
|---|---:|---|
| `session_start` | 28 | 观察 |
| `before_agent_start` | 8 | 改输入（注入 prompt） |
| `session_shutdown` | 7 | 观察 |
| **`tool_call`** | **5** | **拦截** |
| `turn_start` / `turn_end` / `agent_settled` | 各 4 | 观察 |
| `session_tree` / `session_before_switch` / `session_before_fork` | 各 3 | 观察 + 部分可取消 |
| 其余（`tool_result` / `context` / `before_provider_request` / …） | 各 1~2 | 分散 |

**发现一**：绝大多数扩展只是想「知道发生了什么」—— 本仓已有等价物（`EventSink`）。
**发现二**：真正非 hook 不可的只有 5 个用 `tool_call` 的（`permission-gate` / `protected-paths` / `plan-mode` 等）。

**四个理由**（按「非 hook 不可」的强度）：

1. **拦截不属于自己的东西** —— `permission-gate` 拦的是 pi **核心团队**写的 `bash`。这正是 `docs/extension-points.md:40-41` 那条硬边界的**另一端**：本仓所有工具都是自己的，所以这条边不存在。
2. **确定性** —— 「必须每次发生的事，不要放进 prompt，放进 hook」（Claude Code 文档）。**本仓 ADR-0014 的 follow-up 动机是同一句话**（「依赖模型自觉…触发必须是代码」）。所以本仓已经承认这个理由，只是落成一个专用函数而非一套框架。
3. **权威边界** —— 「拒绝这次工具调用」**原理上无法用工具实现**（模型不调就没有拦截）。**本仓已有这条缝：`ApprovalHandler`**（签名与 pi 的 `tool_call` 同构）。且 `CLAUDE.md` 明写「不做安全」，这块是**有意外包**。
4. **生态** —— 扩展作者是第三方。本仓定位相反（「不是自带全部功能的完整 Agent 产品」+ 单人维护）。

**两个反直觉发现**：

- pi 把 **follow-up 做进了 hook**（`HookMap.before_run_end` → `{ followUp?: string }`）。本仓一个设计文档 + 一个专用函数做的事，在 pi 是一个可选返回值。
- **但 pi 没有把 abort/cancel/steer 做进 hook** —— 它们另有消息队列（`deliverAs: "steer" | "followUp" | "nextTurn"`）与 operation 状态机（`OperationStatus = "running" | "open" | "aborting"`）。**一个拥有完整 hook 系统的仓库，仍把「传输」与「取消」留在 hook 之外。**

**结论：本仓不缺 hook 机制，缺的是「让第三方往既有缝里插」。**

---

## ③ 四条控制流是否该统一成一个机制

**结论：不该。**

**前提校正**：不是「四条同类路」。**Steer 与 Abort 已经统一了**（ADR-0013「取消即消息」，共享一条有序通道 + 一个 `Boundary` 枚举）。真正的问题是：**压缩、追问要不要并进 Steer/Abort 那套机制？**

**两个障碍**：

1. **语义不兼容**。把 Abort 做成 hook 会丢两样东西：**保序**（多条 hook 返回值之间无全序，而「先 Abort 后 Steer」≠「先 Steer 后 Abort」）与**探针**（`BashTool` 每 100ms 轮询 `is_aborted()`，hook 的「调一次拿返回值」语义表达不了）。反过来，把 hook 做成队列会丢掉它唯一的卖点：**空置即逐字节不变**。
2. **分层**。统一意味着一个机制横跨 `ys-loop`（压缩/steer/abort）与 `app_loop`（follow-up）。**ADR-0014 已证这会构成依赖环**（`RunResult` 住 `ys-loop`，hook 从 `ys-component` 碰它就是 `ys-component → ys-loop → ys-component`）。**分层正是它们分开的原因，不是疏忽。**

**决定性证据**：pi 自己也没统一（见 ②）。

**该做的三件事**（替代「统一」）：

1. **补一条「什么时候开新路」的准入规则**（目前真正缺的东西）：新控制流需求 → 先走判断链 → **落既有桶则复用**（例：todo 完成度 → follow-up 第二触发器）→ 只有「既不在既有桶、又改不了那个工具」时才开新路 → **开新路必须显式记账**（动 `BasicLoop` 是最高爆炸半径）。
2. **把 Step 们抽成具名函数** —— 抽函数 ≠ 抽机制。`basic.rs` 是 555 行一个 async fn，Step 4b 有 60 行内联 + 承重注释。这是纯重构、零新概念。
3. **统一「缝的装配方式」，而不是「缝的机制」** —— 见 ④。

**回头门槛**（可检验，不是「以后再说」）：同一点出现**第 2 个消费者**（现压缩 1、follow-up 0）；或出现「**不是我们写的**控制流逻辑」；或 `BasicLoop` 的路数增长到「加一条要动 3 处以上」。到那时统一的最小形态是**把「一个点」从函数升成列表**（ADR-0014 原文：「加法，非重写」），**不是一次建 11 个点的 HookMap**。

---

## ④ 如何让第三方往既有缝里插（**待办**）

### 五条缝的卡法各不相同

| 缝 | 基数 | 怎么装的 | 卡在哪 |
|---|---|---|---|
| 工具 `Tool` | **N** | `AgentBuilder::tools` + `toolset` 字面量 | 列表的**装配权**在 app 的 `vec![]` 里 |
| 拦截 `ApprovalHandler` | 1 | `AgentBuilder::approval`（`builder.rs:62`） | **基数 1，装第二个就覆盖第一个** |
| 观察 `EventSink` | 1 | `AgentPorts.events` 每回合借用 | 同上（可自己套扇出） |
| 摘要 `Summarizer` | **0** | **循环内部构造**（`basic.rs:463`） | **完全不可达** |
| 轮边界 `BoundarySource` | 1 | **app_loop 内部构造**（`app_loop.rs:123`） | **完全不可达** |
| 循环 `AgentLoop` | **0** | `AgentBuilder` **无 setter** | **完全不可达** |

**三个新发现**：

- **`AgentBuilder` 根本没有 `Loop` 的装配点** —— `loop_impl`（`builder.rs:15`）声明、`:27` 置 `None`、`:76` `unwrap_or_else(BasicLoop)`，**无 setter** ⇒ 永远是 `BasicLoop`。而 `docs/design.md` §2 明写「Loop 可以替换」。**这是未兑现的承诺 / 文档与实现的背离**，不只是缺插件入口。
- **`Summarizer` 由循环自己造**（`basic.rs:463`），第三方无从提供。
- **`BoundarySource` 由 `app_loop` 自己造**（`app_loop.rs:123`），写死。

### 三种修法

**修法 1：补可达性** —— 把「内部构造」变成「端口传入」。
- `Summarizer` 提到 `RuntimeContext` 上（**照抄 `approval` 的位置**，`context.rs:19`），循环改读 `ctx.summarizer`。机械替换，零语义变化。
- `AgentBuilder::agent_loop(...)` 三行，兑现设计文档的承诺。

**修法 2：补可组合性** —— 从「单数」变「可叠加」。**trait 签名不动**（会破所有实现者），只加组合器：

```rust
pub struct ApprovalChain { handlers: Vec<Box<dyn ApprovalHandler>> }
// needs_approval: any()    request_approval: 任一 Denied 即拒（一票否决，顺序即优先级）
```

**修法 3（正解）：统一装配单元，而非统一机制。** 给第三方**一个**概念而不是五个：

```rust
pub struct Plugin {
    pub order: u16,
    pub tools: Vec<Box<dyn Tool>>,                     // 能力
    pub prompts: Vec<ToolPrompt>,                      // 系统提示贡献
    pub approval: Option<Box<dyn ApprovalHandler>>,    // 拦截
    pub observer: Option<Box<dyn EventSink>>,          // 观察
    pub summarizer: Option<Box<dyn Summarizer>>,       // 窄能力
}
```

装配方折成 `Assembled`：tools 拼接、approval 串成链、observer 扇出、prompts 归并。第三方只学一个 `Plugin`，`BasicLoop` **一个字都不用改**（对比 hook 系统要动循环）。

**这是 ③ 的对偶**：机制保持独立（语义互斥），装配面统一成一张表。**不违反「不设计假想未来需求」** —— `Plugin` 是纯数据（~30 行）+ 合并逻辑（~50 行），**零运行期机制、零新 trait、不碰循环**；它只是把**已经存在**的缝接出来，建缝的工作早做完了。

### 一条不能这么接的边界

**`BoundarySource` 不该成为插件贡献**：它每回合**必须新建**（ADR-0013：`Abort` 永久置位，复用会让上一回合的取消立刻取消之后每个回合）；插件持有它 = 跨回合持有 = 破坏该不变量。真要扩展，形态应是**工厂**（`fn make_boundary(inner) -> Box<dyn BoundarySource>`），生命周期仍归 app_loop。今天无消费者 ⇒ **待定**。

同理 `Session` / `Model` 归接线器（ADR-0010）—— **插件提供能力，不接管状态**。

### 分期

| 期 | 做什么 | 消费者 | 判断 |
|---|---|---|---|
| **P1** | `ToolPlugin`（ADR-0021 已定） | **todo（真实）** | 立刻做 |
| **P1.5** | 补可达性：`summarizer` 上 `RuntimeContext`、`AgentBuilder::agent_loop` | 无 | **做** —— 极便宜，且后者**修文档背离** |
| **P2** | `Plugin` 泛化到全部缝 | 待出现 | **等** —— 第 2 个缝的消费者出现时做，纯加法 |

---

## ⑤ 待办清单（**全部未做**）

| # | 项 | 触发条件 | 依据 |
|---|---|---|---|
| 1 | **P0：prefix 指纹测试**（`sha256(tools_json)` + `sha256(system_prompt)` 双锚点） | 立即 —— 是任何 prompt 重构的前置条件 | design.md §6 P0 |
| 2 | **P1：`ToolPlugin`** 装配缝 | 立即 | ADR-0021 决策 3 |
| 3 | **P1.5：`summarizer` 上 `RuntimeContext`** | 立即（极便宜） | 本文 ④ 修法 1 |
| 4 | **P1.5：`AgentBuilder::agent_loop`** | 立即 —— 修文档背离 | 本文 ④ |
| 5 | **P2b：follow-up 第二触发器**（回合 `Completed` + 有未完成项 ⇒ 追问） | 待 follow-up 本身实现后 | 本文 ① |
| 6 | **`Plugin` 泛化**（统一装配面） | 第 2 个缝的消费者出现 | 本文 ④ 修法 3 |
| 7 | **ADR-0023：控制流分类**（传输 / 决策 / 分层，不统一） | 可立即写（结论已稳定） | 本文 ③ |
| 8 | **「开新路」准入规则**补进 `docs/extension-points.md` | 可立即写 | 本文 ③ |
| 9 | design.md 加 **P2b**；ADR-0021 Consequences 补一句「确保完成是 follow-up 的第二触发条件，**不是 hook 的第二消费者**」 | 可立即写 | 本文 ① |
| 10 | **修 `load_project_context` 软链重复加载**（`CLAUDE.md -> AGENTS.md`，27.8KB 加载两遍 ≈18k est token） | 独立 issue —— **收益远大于任何插件缝优化** | design.md §8 |
| 11 | 订正 `basic.rs:618` / `limits.rs:20` 的「缺口约 1.1k」（实测 **2,084**，低估 1.9×） | 可立即写 | design.md §8 |
| 12 | **把 `BasicLoop` 的 Step 抽成具名函数** | 纯重构，可随时 | 本文 ③ 第 2 件 |

---

## ⑥ 与本目录其他文件的关系

- [`context.md`](./context.md) —— arch-explore 的现状与约束（五条缝的原始量化）
- [`design.md`](./design.md) —— 三候选对比 + 推荐方案（含成本模型、分阶段落地）
- [ADR-0021](../../adr/0021-plugin-seam-without-hook-system.md) —— 决策记录
- **本文** —— 设计定稿后的四轮追问；①②③ 是结论，④⑤ 是待办
