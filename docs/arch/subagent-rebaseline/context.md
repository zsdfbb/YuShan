# Subagent 支持（当前基线重估） — 架构上下文

> 产出：`/arch-explore`（2026-10-09）。**需求原话**：「考虑 subagent 的支持」。
>
> ⚠️ **本目录是新一轮独立上下文，不覆盖 [`../subagent/`](../subagent/)。** 既有那份（`context.md` / `design.md` / `research.md` + [ADR-0022](../../adr/0022-subagent-dispatch.md)，accepted 2026-09-23）是**设计期**的完整记录，**仍然有效、仍是权威设计**；本文**只补它之后发生的事**。分歧以 ADR-0022 与 `../subagent/design.md` 为准。
>
> 证据均带 `文件:行号`，对应本工作区 `master`（`911fd72`）。

---

## 概述

一句话：**subagent 的「设计」已完成且被接受，但「代码」一行都没有** —— 本轮的真正问题是**一个 16 天前的设计，遇到一个已大幅演进的代码库，哪些前提还成立、哪些前提已经变了**。

结论先行（四条）：

1. **零落地**：`SubagentRunner` / `SubagentOutcome` 在源码里**不存在**；`subagent` 只作为**字符串**出现在两处 —— 系统提示的能力边界声明（`apps/coding-agent/src/prompt.rs:107`）与它的测试拷贝（`apps/coding-agent/src/prompt.rs:634`、`apps/coding-agent/tests/e2e_skills.rs:90`）。`ToolContext`（`crates/ys-tool/src/context.rs:7-18`）与 `RuntimeContext`（`crates/ys-component/src/context.rs:12-27`）都**没有**任何派发相关字段。

2. **设计的「前置阻塞项」仍未解**：ADR-0022 决策 6 要求给 `Tool` 加 `fn timeout()` 默认方法，把 `BasicLoop` 的超时闸从「固定 `bash_timeout` → 否则 300s」改为「工具声明 → 否则 `bash_timeout` → 否则 300s」。**当前一字未改**：`crates/ys-loop/src/basic.rs:470` 仍是 `ctx.limits.bash_timeout.unwrap_or(300)`，且该 `timeout` 会 **drop future** ⇒ 超过 5 分钟的派发/长任务被静默杀掉。这是本需求**最大的单点风险**，且不因「先做身份」而消解。

3. **设计引用的「未实现」到今天一半已实现、一半仍缺**：
   - ✅ **follow-up 缝**（ADR-0022 决策 2 引用的 `docs/design-plans/2026-09-15-follow-up.md`）**已由 [ADR-0027](../../adr/0027-completion-check-and-round-budget.md) 落地** —— `apps/coding-agent/src/follow_up.rs` 存在，`app_loop` 已重构为 `while let Some((input, kind)) = pending.take()` + `run_turn_once`（`apps/coding-agent/src/app_loop.rs:300`）。⇒ subagent 的「回合计入哪条预算」**有了新的对齐对象**。
   - ✅ **skills 已上线**（ADR-0019）⇒ 常驻 skill 目录 ~9.6k token 已进 `RunLimits::fixed_prompt_tokens`（`crates/ys-component/src/limits.rs:15-24`）。ADR-0022 自己标注的「成本数字待重算」（子 system ~18,100 / 每轮 ~24,630 是**旧基线**）**到本轮仍是待办**。
   - ❌ **并行批**（决策 11）：`basic.rs:423` 的注释仍是「**串行执行工具调用（v0）**」；`Tool::is_concurrency_safe()` 与 `max_parallel_tools` **均不存在**。
   - ❌ **`Usage` 缓存读数**（决策 18）：`crates/ys-core/src/usage.rs` 仍只有 `input_tokens` / `output_tokens`，**没有** `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`。⇒ 「成本从论证变读数」这条**没做**。

4. **新增了两条 ADR-0022 写设计时还不存在的交互面**，必须在落地前显式对齐：
   - **wall-clock 回合预算**（ADR-0027 决策 6）：`app_loop` 现在用 `config.max_turn_duration()`（默认 **48h**）计时，到点向边界源 `push(Boundary::Abort)`（`apps/coding-agent/src/app_loop.rs:133, 351`）。⇒ 「一次 30 分钟的子 agent 派发」与「48h 父回合预算」是**同一根刹车**的两端。
   - **单回合轮数上限已可配**（ADR-0027 决策 1）：`Config::max_rounds()` 默认 `u32::MAX`（= 不设，`apps/coding-agent/src/config.rs:230`），`resolve_limits` 单点注入（`apps/coding-agent/src/main.rs:369`）。⇒ 父子各自的 `max_rounds` 从哪来、默认多少，需重定。

### 用户已澄清（本轮，2026-10-09）

| # | 问题 | 答复 | 对设计/范围的后果 |
|---|---|---|---|
| **1** | 本轮目标是「按 ADR-0022 落地」还是「先复核设计」？ | **先复核** | 本轮产出是**设计复核/修订**（下一步 `arch-design`），不是代码。本文作为复核的输入基线。 |
| **2** | 并行派发是否在范围内？ | **需要支持并行** | 决策 11（Step 9 条件并行 + `is_concurrency_safe()` + `max_parallel_tools`）**必做**，不再是可选项。⇒ 复核须给出「Step 9 改造」的完整形态。 |
| **3** | 决策 6（per-tool 超时）是否先于 `subagent` 工具落地？ | **加（先落地）** | per-tool `timeout()` 默认方法 **排在加 `subagent` 工具之前**，作为独立可交付刀。 |
| **4** | 子 agent 与 48h wall-clock 预算的关系？ | **subagent 独立预算** | 子**不计入**父 `max_turn_duration`；子有自己的预算（wall-clock 与/或 `max_rounds`）。⇒ 复核须定「子预算的具体形状与默认值」。 |
| **5** | 成本读数（决策 18）是否本轮做？ | **这轮做** | `Usage` 加 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` + 适配器解析 + `Config` per-name 窗口 —— **纳入本轮**（新增一刀）。 |
| **6** | 子预算的具体值？ | **与主 agent 保持一致** | 子用**独立预算实例**（不与父共享计数、不计入父的已用额度），**取值与主 agent 相同** —— 同一份 `Config`（`max_rounds` / `max_turn_duration` / `context_window`）。 |

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

**对 subagent 是硬约束的两条**（均未变）：

- `ys-tool` **不能**依赖 `ys-runtime`（依赖方向单向）⇒「工具内部跑一个子 `Agent`」写不出来。
- **`ys-model` → `ys-tool`**（`ModelRequest.tools: Vec<ToolSpec>`）⇒ **`ys-tool` 不得依赖 `ys-model`（成环）**。⇒ 落在 `ToolContext` 的任何派发能力 trait **不得出现 `Model` / `ModelRequest` / `Session` / `AgentEvent`**，只能像 `Summarizer` 那样 `&str` 进 / `String` 出（先例：`crates/ys-tool/src/summarizer.rs:1-5` 的开篇注释逐字写着这条理由）。

### 核心抽象（当前形态）

| 抽象 | 位置 | 形状 | 对 subagent 的意义 |
|---|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs` | `spec()` + `call(Value, ToolContext)`，**无 `timeout()` / 无 `is_concurrency_safe()`** | 派发**必须**走这里（判据链：模型决定何时做 ⇒ Tool） |
| `ToolContext` | `crates/ys-tool/src/context.rs:7-18` | `{ boundary, cwd, workspace_root, summarizer }` | **无 Model / 无派发能力** —— 硬堵点（D1） |
| `Summarizer` | `crates/ys-tool/src/summarizer.rs` | 窄能力 trait，`&str` 进 / `String` 出，`#[async_trait]` | **唯一的「给工具注入执行能力」先例**，是 `SubagentRunner` 的模板 |
| `RuntimeContext` | `crates/ys-component/src/context.rs:12-27` | `{ model, registry, session, events, limits, cwd, workspace_root, approval, system_prompt, boundary }` | loop 侧全家桶；**无派发字段** |
| `AgentPorts` | `crates/ys-runtime/src/agent.rs:18-31` | `{ model, session, events, boundary }` | 一次 `run_turn` 的全部端口；**无派发字段** |
| `Agent`（无状态） | `crates/ys-runtime/src/agent.rs` | 不持 session/model/events | ⇒ 子 agent 必须**另建**一个 `Agent` |
| `Source` | `crates/ys-protocol/src/envelope.rs:14-33` | `Source(Arc<str>)`，**只有 `Source::agent()` 一个构造**；`Default = agent` | 身份缝；**尚无 `Source::subagent(id)`** |
| `Envelope` | `crates/ys-protocol/src/envelope.rs:47-52` | `{ source, turn, event }` | 身份只在信封层；事件本身不动 |
| `Usage` | `crates/ys-core/src/usage.rs` | `{ input_tokens, output_tokens }` + `Add` | **无缓存字段** ⇒ 成本读数缺（决策 18） |
| `BoundarySource` | `crates/ys-protocol/src/boundary.rs` | `take()` / `is_aborted()`，**每回合必须新建** | 取消传播载体（D5） |
| `StopReason` | `crates/ys-core/src/stop.rs` | `Completed / MaxRounds / Cancelled / Truncated` | 可**直接复用**作子终止原因（决策 4） |
| `RunLimits` | `crates/ys-component/src/limits.rs` | `max_rounds / bash_timeout / context_window / compaction / fixed_prompt_tokens` | 子 agent 的 limits 从哪来 |
| `Config::build_model` | `apps/coding-agent/src/config.rs:346` | 委托给**单例** `model_factory`（`set_model_factory`，`:338`） | 「按给定 model 名构造」需改造（决策 17） |
| `Config::context_window` | `apps/coding-agent/src/config.rs:202` | 只读 `self.model`（当前模型） | **无 per-name 窗口查询** ⇒ 子换模型会继承父窗口（决策 17 的静默错） |

### 关键数据流：今天一次「派发」需要什么（未变）

```text
模型 ──ToolUse{name:"subagent"}──► BasicLoop::run_turn
                                       │
                           registry.get("subagent") ──► 派发工具::call(args, ToolContext)
                                       │                        │
                                       │                        └─ 想跑子 agent，需要：
                                       │                           · 一个 Model（或按名造 Model）
                                       │                           · 一个独立的 Session
                                       │                           · 一个 EventSink（NoopEventSink）
                                       │                           · 一个 Agent（子工具表 + 子 system prompt + 子 limits）
                                       │                           · 一个自己的 BoundarySource
                                       │                           · usage 归属 / 并发上限 / 超时
                                       │
                                       └─ ToolContext 只给 boundary/cwd/workspace_root/summarizer
                                          ⇒ 以上六样一样都拿不到（与 ADR-0022 时相同）
```

接线单点仍是 `crates/ys-loop/src/basic.rs:461`（注释自称「唯一的摘要适配器接线点」）。

### 外部依赖

无新增依赖需求。子 agent 只用既有的 `Model` / `Session` / `EventSink` / `Agent` 原语。

---

## 约束

- **技术**
  - **依赖方向单向**：`ys-tool` 不得依赖 `ys-runtime`；**`ys-model` → `ys-tool`** ⇒ `ys-tool` 不得依赖 `ys-model`（成环）。落 `ToolContext` 的新 trait 只能窄到 `&str` 进 / `String` 出（`summarizer.rs:1-5`）。
  - **`ys-core` / `ys-event` / `ys-protocol` 零 tokio**；**`ys-tui-coding` 不依赖 `ys-runtime`**（编译器强制）。
  - **Agent 无状态**（ADR-0010）⇒ 子 agent 必须另建 `Agent` 实例。
  - **`BoundarySource` 每回合必须新建**（`Abort` 永久置位）；父 abort 需**显式向下传播**，不能直接把父源交给子（`take()` 会消费父队列）。
  - **`ContentBlock::ToolResult` 是 JSONL 落盘契约，变体无 `#[non_exhaustive]`** ⇒ 结果回传只能是 `ToolResult.content: String`（ADR-0021 决策 2 已否决 `details`）。
  - **不跨动态库边界传 trait object** ⇒ 子进程路线出局（与 ADR-0022 同）。
- **性能**
  - **每轮固定开销**进 `RunLimits::fixed_prompt_tokens`（`limits.rs:15-24`），与压缩阈值耦合（ADR-0017）；**新工具 + 新 guideline 直接改 prompt 前缀字节** ⇒ 影响所有用户的 prompt cache。
  - **子 agent 的固定前缀是第二份开销**；ADR-0022 的「成本数字待重算」**仍未重算**（skills 上线改变了基线）。⚠️ 且 `Usage` 无缓存读数，「成本从论证变读数」这条**没做**。
  - **prompt cache 是一等要求**：子前缀只由 (仓库, skills 集合, 模型) 决定，启动期冻结。
- **演进**
  - **本仓无 branch / fork / worktree、无沙箱** ⇒ 「从历史重放重建子状态」「并行写同一文件」在本仓**无机制兜底**（只能靠提示词约束，ADR-0022 已记为已知缺口）。
  - **follow-up 缝已落地**（ADR-0027）⇒ subagent 与 follow-up **不再各自造「额外驱动模型」的机制**，须复用 `app_loop` 的 `pending.take()` / `run_turn_once` 形状。
  - **wall-clock 预算已存在**（ADR-0027，默认 48h）⇒ 子 agent 的长跑与它是同一根刹车，需定关系。
  - **per-turn-model-switch 议题**（`docs/arch/per-turn-model-switch/`）与 subagent 的 `model` 参数**在同一层**（都改 `Wiring` / `Config` 的模型持有形态）⇒ 边界需对齐。
- **组织**
  - 单人维护；「不设计假想未来需求」是明文原则（ADR-0014 / `docs/extension-points.md`）。
  - **判据链不变**：模型决定何时派发 ⇒ **Tool** 桶，**不是 Hook** ⇒ 本轮仍**不建 hook 系统**。

---

## 与 ADR-0022 的偏离清单（本轮核心增量）

> 逐条对「ADR-0022 的假设」与「当前 `911fd72` 的代码」。

| # | ADR-0022 的决策/假设 | 当前代码 | 状态 | 对本轮的影响 |
|---|---|---|---|---|
| 1 | 决策 6：`Tool::timeout()` 默认方法 + `BasicLoop` 超时闸改造 | `basic.rs:470` 固定 `bash_timeout.unwrap_or(300)`，超时会 drop future | ❌ **未做** | **仍是最大阻塞项**：>5min 派发被静默杀 |
| 2 | 决策 11：Step 9 条件并行 + `is_concurrency_safe()` + `max_parallel_tools` | `basic.rs:423` 仍「串行（v0）」 | ❌ **未做** | 并行派发零基础 |
| 3 | 决策 8：子 usage 上卷 `TurnStats` | `TurnStats` 结构存在，但无子来源 | ❌ **未做** | 依赖 runner 存在 |
| 4 | 决策 18：`Usage` 加缓存字段 | `usage.rs` 只有 input/output | ❌ **未做** | 成本读数缺；**成本数字仍待重算** |
| 5 | 决策 7：`prompt.rs:107` 改条件式派生 | 仍是硬编码 `no subagents and no plan mode`（`:107`，测试 `:634`，`e2e_skills.rs:90`） | ❌ **未做** | 加了工具却不说，会自相矛盾 |
| 6 | 决策 17：模型可指定的完整连带（按名 build + per-name 窗口 + 缓存实例） | `build_model` 是单例闭包；`context_window` 只读当前模型 | ❌ **未做** | 子换模型会继承父窗口（静默错） |
| 7 | 决策 2 引用「follow-up 已设计未实现」 | **已由 ADR-0027 实现**（`follow_up.rs`、`run_turn_once`） | ✅ **已变** | 对齐新对象，而非从零造 |
| 8 | 成本基线（子 system ~18,100 / 每轮 ~24,630） | skills 上线 + `fixed_prompt_tokens` 引入 | ⚠️ **前提已变** | P0 指纹 + 成本读数**应先做** |
| 9 | 「先锚 9 工具」 | 当前工具表**恰好 9**（`toolset.rs:40` + `EXPECTED_ORDER`） | ✅ **一致** | P0 锚点时机正好 |
| 10 | 派发工具排在**末位**以对齐缓存前缀 | `EXPECTED_ORDER` 固定 9 元 | ✅ **一致** | 加第 10 个工具 = 一次预期前缀漂移 |

---

## 需求范围

### 范围内（本轮 arch-design 应回答的）

> ✅ 已由 ADR-0022 定死、**本轮不重开**：子 agent **不自嵌套**（工具表裁剪）、模型**可指定**、UI **只显示「在工作」**、工具名 = `subagent`、结果 = 硬截断摘要 + 产物路径 + `StopReason`、取消 = `ChildBoundary` 只透传 `is_aborted`。

- [x] **落地顺序**（答 1，先复核）：ADR-0022 的「第 0 刀（P0 指纹）→ 第 1 刀（身份）→ …」在「决策 2 已把身份判为不发射」之后，第 1 刀**已不存在**。⇒ 复核须给出**重排后的刀序**（见 `## 后续建议`）。
- [ ] **决策 6 的落地形态**（答 3，**先落地**）：per-tool `timeout()` 默认方法的具体签名、`subagent` 声明多长（ADR 提议 1800s）、与 `RunLimits.bash_timeout` 的优先级。**顺序已定：先于 `subagent` 工具。**
- [ ] **并行的完整形态**（答 2，**必做**）：Step 9 从「串行（v0）」升级为条件并行 —— `Tool::is_concurrency_safe()` 默认 `false`、`max_parallel_tools`（ADR 建议 4）、信号量排队、事件按原始顺序 emit、「每次派发建新子 `Agent`」。**验收须证明既有批次（无工具声明 safe）逐字节不变。**
- [x] **子预算的形状**（答 4 + 答 6）：子用**独立预算实例**（不与父共享计数），**取值与主 agent 一致** —— `max_rounds` / `max_turn_duration` / `context_window` 都取同一份 `Config`。⚠️ **「子必有界」被用户否决**（与「保持一致」冲突 ⇒ 父不设子也不设）。仍需定：`fixed_prompt_tokens` 必须填**子表**的值（否则子会话系统性晚触发压缩）；子 `context_window` 按**子模型**（决策 17 的 per-name 窗口）。
- [x] **子 usage 上卷**（答 6，决策 8）：子的 usage 累加进父 `TurnStats` —— **与「预算独立」不矛盾**：预算是刹车（独立），记账是成本（上卷）。
- [ ] **`Config` 单例模型 → 按名构造**的改造范围（决策 17 的三项连带）。
- [x] **成本读数**（答 5，**本轮做**）：`Usage` 缓存字段 + 适配器解析 + `Config` per-name 窗口 —— 纳入本轮。
- [ ] **`prompt.rs:107` 改写 + 两份测试拷贝**的处置。

### 范围外（明确不做的）

- **再写一份独立设计**：ADR-0022 + `../subagent/design.md` 已是 accepted 设计，本轮的产出是**「差异与落地裁决」**，不是重画。
- **CodeWhale 量级的完整子系统**（11 action / mailbox / roster / focus / 预算 scope / checkpoint）：量级不是「加一个工具」。
- **子进程 / 跨进程路线**：跨动态库边界传 trait object 被禁。
- **fork / 从历史重放重建子状态**：本仓无 branch/fork，语义不成立。
- **后台 / 游离子 agent**：本仓做同步、前台。
- **per-role 模型覆盖表**：ADR-0022 只给「单次调用可指定」；role 表与 per-turn-model-switch 重叠，另议。

### 关键场景

- **场景 1（成功）**：「把这个仓库里所有调用 X 的地方找出来并总结」→ 父调 `subagent` → 子独立上下文里 grep/read 若干轮 → 回传摘要 + 产物路径 → 父上下文**没被子 agent 的 20 次 read 撑爆**。**这是 subagent 存在的唯一理由：上下文隔离。**
- **场景 2（身份）**：子 agent 干活时，UI / `--json` 能区分「这是子 agent 的输出」—— 否则 `is_turning` 误关、`--json` 提前收尾。按 ADR-0022 决策 2，本设计**让子事件根本不进父流**（用 `NoopEventSink`），此场景以「不发射」而非「过滤」满足。
- **场景 3（取消）**：子跑一半，用户 Esc → Abort 传到子（含子内 bash 轮询）→ 父回合以 `Cancelled` 收场，无悬挂子任务。
- **场景 4（失败）**：子撞 `max_rounds` / 模型报错 → **父回合不失败**：作为 `is_error` 结果回喂父模型（ADR-0001 双通道），父自行决定重试或换路。
- **场景 5（长跑）**：子派发跑 20 分钟 → **不被 300s 工具超时静默杀掉**（依赖决策 6 落地）。
- **场景 6（成本）**：派发后状态行 token 计数**要么反映真实总消耗，要么明确标注不含子 agent** —— 不静默少算（决策 8）。
- **场景 7（独立预算 + 上卷记账）**：父回合里派发一个跑 20 分钟的子 agent → **子走子自己那份预算实例**（答 4/6，取值同主 agent），不与父共享计数；但子的 **usage 仍须上卷进父 `TurnStats`**（答 6，决策 8）——「预算独立」与「记账上卷」是两件事，不能混。
- **场景 8（并行）**：同一条消息里模型发了 ≥2 个 `subagent` → 并行执行（答 2）；事件**按原始 id 顺序** emit，TUI 靠 `tool_index` 按 id 回填；并行子写目标不相交（靠提示词约束，本仓无沙箱）。

---

## 未澄清问题

> ADR-0022 已关闭的（工具表裁剪 / 模型可指定 / UI 只显示「在工作」/ P0 先行）**不重开**。以下为本轮新增或状态变化带来的。

- [x] **Q1（定调）**：本轮目标 = **② 先复核设计、再落地**（用户 2026-10-09 定）。⇒ 本轮产出是设计复核/修订。
- [x] **Q2（并行）**：**需要支持并行** ⇒ 决策 11 **必做**，非可选。
- [x] **Q3（超时闸优先级）**：**加**（per-tool `timeout()` 先于 `subagent` 工具落地）。
- [x] **Q4（父预算关系）**：**subagent 独立预算** ⇒ 子**不计入**父 48h `max_turn_duration`，子有自己的预算。
- [x] **Q5（子预算的具体值）**：**与主 agent 保持一致**（用户 2026-10-09 定）⇒ 子 = **独立实例 + 同取值**。⚠️ 推论：父默认 `max_rounds = u32::MAX`（不设）⇒ 子**也不设**；「子必有界」这条**由用户否决**，本轮的兜底是 `max_turn_duration`（子同为主 agent 的那条，默认 48h）。复核须显式记录这一点。
- [x] **Q6（成本读数）**：**这轮做**（用户 2026-10-09 定）⇒ 决策 18 **纳入本轮**：`Usage` 加两个缓存字段（加法式、`#[serde(default)]`）+ 适配器解析 + `Config` per-name context window。这是**独立于 subagent** 的一刀，可与第 1 刀（超时闸）并列先做。
- [ ] **Q7**：**`prompt.rs:107` 的能力边界声明**改成什么 —— 删掉「no subagents」、还是条件式派生？连带 `:634` 与 `e2e_skills.rs:90` 两份拷贝。
- [ ] **Q8**：**与 per-turn-model-switch 的边界** —— subagent 的 `model` 参数与「运行中换模型」是否共用同一套「按名构造 + 缓存实例」改造？
- [ ] **Q9**：**与 follow-up / todo 的记账** —— subagent 派发、ADR-0027 复核回合、未来的 todo 推进都会额外驱动模型调用，是否共用同一套上限与 usage 记账？
- [ ] **Q10**：**`Source` 的预留缝如何处置** —— ADR-0022 决策 2 判定「子事件不发射」⇒ `Source` 缝暂无消费者。是否顺手删掉/保留？触发翻案的条件（要在 UI 看到子逐条活动）是否继续写死为「已知缺口」？

---

## 后续建议

**建议的落地刀序（相对 ADR-0022 的「四刀」重排 —— 第 1 刀「身份」因决策 2 已判不发射而消失）**：

| 刀 | 内容 | 依赖 |
|---|---|---|
| **第 0 刀** | **P0 prefix 指纹测试**（`sha256(tools_json)` + `sha256(system_prompt)` 双锚点）—— 当前恰是 9 工具，锚点时机正好（偏离清单 #9） | 无 |
| **第 1 刀** | **per-tool 超时闸**（决策 6）—— 独立于 subagent，且是它的前置阻塞项 | 无 |
| **第 1.5 刀** | **成本读数**（决策 18：`Usage` 缓存字段 + 适配器解析 + per-name 窗口）—— 用户答 5 **本轮做**，独立于 subagent | 无 |
| **第 2 刀** | **窄能力 trait `SubagentRunner`**（照 `Summarizer`）+ 三跳注入（`AgentPorts → RuntimeContext → ToolContext`） | 第 1 刀 |
| **第 3 刀** | **runner 实现**（app 侧：造子 `Agent` / `MemorySession` / `NoopEventSink` / `ChildBoundary` / 子工具表裁剪 / 落盘转录）+ `subagent` 工具 | 第 2 刀 |
| **第 4 刀** | **提示词与文档改写**：`prompt.rs:107` 条件式 + guideline 防失控规则 + 两份测试拷贝 | 第 3 刀 |
| **第 5 刀** | **并行派发**（决策 11 的 Step 9 条件并行）—— 用户答 2 **必做** | 第 3 刀 |

**建议用 `arch-design` 做方案对比**（用户答 1 = **先复核**，故下一步就是设计复核/修订），把三个决策点作为首要：

1. **并行派发（决策 11）的完整形态** —— Step 9 是本 ADR 唯一动既有执行语义处；须给出「默认串行 + 显式声明 + 事件保序 + 每次建新子 Agent」的可验证形态，并证明既有批次逐字节不变。
2. **决策 6 的落地形态**（per-tool `timeout()` 的签名与优先级）—— 独立可交付项，且不改它就落地不了长派发。
3. **「子独立预算」的形状**（subagent wall-clock 兜底 + 子 `max_rounds` 默认值 + 子 `fixed_prompt_tokens` / `context_window`）—— ADR-0027 之后新增的交互面，ADR-0022 写的时候还不存在。

**三条纪律（沿用 ADR-0022）**：

- **不动 `BasicLoop` 的既有语义，除非显式记账**（判据链：模型决定 ⇒ Tool；Step 9 是唯一例外，且被 `is_concurrency_safe()` 默认 `false` 闸住）。
- **子 agent 的失败 = 错误结果，不是基础设施失败**（ADR-0001 双通道）。
- **「产物」显式设计，别留给提示词**（父子共用工作区，父本就能 `read`；要定的是父怎么知道去哪找）。

---

## 参考

- **权威设计**：[`../subagent/design.md`](../subagent/design.md)、[ADR-0022](../../adr/0022-subagent-dispatch.md)
- **既有探索（供对照，未覆盖）**：[`../subagent/context.md`](../subagent/context.md)、[`../subagent/research.md`](../subagent/research.md)（12 项目调研）
- **本轮新增交互面**：[ADR-0027](../../adr/0027-completion-check-and-round-budget.md)（预算可配 + 完工复核）、[ADR-0028](../../adr/0028-model-call-retry-and-timeout.md)（模型调用重试/超时）、`docs/design-final/coding-agent-tui.md`（as-built）
- **判据**：[`docs/extension-points.md`](../../extension-points.md)（四者边界 + 判断链）
- **相邻议题**：[`../per-turn-model-switch/`](../per-turn-model-switch/)（模型持有形态）、[`../skills/`](../skills/)（`fixed_prompt_tokens` 来源）
- **术语**：[`docs/CONTEXT.md`](../../CONTEXT.md)
