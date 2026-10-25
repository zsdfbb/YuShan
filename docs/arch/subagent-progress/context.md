# Subagent 运行状态 — 架构上下文

> 产出：`/arch-explore`（2026-10-25）。**需求原话**：
> 「考虑一下如何优化 subagent 的状态检测，我希望主 agent 需要时可以更好的查到 subagent 做了什么、
> 现在的状态。以及后续 tui 能够展示 subagent 是否还在工作。subagent 完成后，tui 也展示主 agent
> 收到了通知、主 agent 也输出一些当前的工作状态以及后续要做的事情。」
>
> 本文只记录**现状基线**与**约束**。方案见 [`design.md`](design.md)。
>
> 证据均带 `文件:行号`，基线为本工作区（`apps/coding-agent/src/subagent.rs` 2628 行版本）。

---

## 概述

一句话：**subagent 的派发、落盘、投递、记账都已落地，但「跑着的时候是什么样」这一整层是空的** ——
子 agent 的进展只存在于一个跑完才被读的 `CollectingSink` 里，产物文件跑完才创建，句柄还跨回合重复。
需求的三件事（查到状态 / TUI 显示在跑 / 显示已收到通知 + 主 agent 报现状）**全部落在这一层上**。

### 需求拆成三件事

| # | 用户原话 | 落到哪个机制 |
|---|---|---|
| R1 | 「主 agent **需要时可以查到** subagent 做了什么、现在的状态」 | 推送（`AgentEvent`）+ **可选**的只读状态工具 |
| R2 | 「TUI 能展示 subagent **是否还在工作**」 | 状态行一段（回合内 + 回合外）+ 转换行 |
| R3 | 「完成后 TUI 展示主 agent **收到了通知**；主 agent 也输出**当前工作状态与后续要做的事情**」 | `DeferredResult` 渲染改写 + 提示词 guideline + 完成消息的结构化头 |

---

## 现有架构

### 模块边界（与本需求相关的部分）

```text
ys-tool          ToolContext { boundary, cwd, workspace_root, summarizer, subagent, tool_budget }
                 SubagentRunner trait（&str 进 / SubagentOutcome 出）+ SubagentOutcome
   ↑
ys-loop          BasicLoop Step 9：ToolContext 挂 subagent（basic.rs:916-924 / :988-996）
                 Step 9 并行段：buffer_unordered（ADR-0044 决策 3）
   ↑
ys-runtime       AgentPorts { model, session, events, boundary, subagent }（agent.rs:18-31）
   ↑
apps/coding-agent
  ├── subagent.rs         AppSubagentRunner / ChildTask / SubagentTool / SubagentAsyncTool
  │                       PendingResults（跨回合投递表）/ SubagentsDir / UsageTotals
  ├── assembly.rs         AgentFactory：父表 12 元 = 9 + bash_bg + subagent + subagent_async
  └── app_loop.rs         run_turn_once：每回合 bind_results + make_runner + 事件转发
   ↑
ys-tui-coding            App（transcript + is_turning + current_round）/ draw::status_text
```

**两条硬边界**（决定方案形状）：

- `ys-tool` **不得**依赖 `ys-runtime`，且因 `ys-model → ys-tool`（为 `ToolSpec`）而**不得**依赖
  `ys-model` ⇒ 落在 `ToolContext` 的能力 trait 里**不得出现** `Model` / `ModelRequest` /
  `Session` / `AgentEvent`（`docs/adr/0022-subagent-dispatch.md:18`、
  `crates/ys-tool/src/subagent.rs:3-6`）。
  - 补充事实：`ys-tool` **可以**加 `ys-event`（`ys-event` 只依赖 `ys-core`，不成环）——
    但本轮用不到，见 design §3.4。
- `ys-tui-coding` **不依赖** `ys-runtime` ⇒ TUI 只能看 `Outbound`（事件 + `CodingView` 快照）。

### 核心抽象（现状）

| 抽象 | 位置 | 形状 | 对本需求的意义 |
|---|---|---|---|
| `SubagentRunner::run` / `spawn` | `crates/ys-tool/src/subagent.rs:45` / `:70` | `run` 返 `SubagentOutcome`；`spawn` 返 `handle: String` | **`spawn` 只给句柄，不给任何状态** ⇒ R1/R2 的结构缺口 |
| `SubagentOutcome` | `crates/ys-tool/src/subagent.rs:16-30` | `{ summary, artifact, stop_reason, diagnostic, usage }` | **没有 rounds / tool_calls / elapsed** ⇒ R3 的「报现状」缺读数。非 `#[non_exhaustive]` ⇒ 加字段是 breaking |
| `PendingResults` | `apps/coding-agent/src/subagent.rs:192-283` | 进程级 `Mutex`；`sink`（每回合绑）+ `generation` + `queue` | **本方案的状态板与它并列**，形状照抄 |
| `ChildTask::execute` | `apps/coding-agent/src/subagent.rs:749-882` | 建子 `Agent` + `MemorySession` + **本地 `CollectingSink`**（`:801`）+ watchdog | **唯一的采集点**：sink 是 `&mut` 传入（`:803-810`），可换包装 |
| `SubagentsDir::write_transcript` | `apps/coding-agent/src/subagent.rs:356-363` | `&str` → 路径；`{pid}_{seq}.md`；`static COUNTER` | **跑完才分配路径**；无 append 形态 ⇒ R1 的「看做了什么」没有可 tail 的文件 |
| `UsageTotals` | `apps/coding-agent/src/subagent.rs:121-168` | 4+2 个 `AtomicU64/Bool`，`drain()` 一次清零 | **全局累加，不可按句柄查** |
| `Boundary::Deferred { handle, message }` | `crates/ys-protocol/src/boundary.rs:23` | 子结果进会话的唯一载体 | 「送达」与「完成」是两个时刻 ⇒ 状态机必须区分 |
| `AgentEvent::DeferredResult` | `crates/ys-event/src/event.rs:35-40` | `{ handle, message }`，非终局 | **「主 agent 收到通知」的唯一可观测点** |
| `CodingView` | `crates/ys-tui-coding/src/view.rs:83-131` | 18 个字段（cwd / model / tokens / tools / skills / …） | **没有任何「正在跑什么」的字段** |
| `status_text` | `crates/ys-tui-coding/src/draw.rs:599-638` | 段数组 + 「从尾部剥」 | R2 的落点 |

### 关键数据流：一次异步派发现在发生什么

```text
模型 ──ToolUse{subagent_async}──► BasicLoop Step 9
                                     │ emit ToolCall（basic.rs:880 / :970）
                                     ▼
                          SubagentAsyncTool::call（subagent.rs:1281-1302）
                                     │ runner.spawn(..)                  ← 只解析模型 + 占额度
                                     │ 返回 handle = "sa{n}"（:595）      ← **每回合从 0 重来**
                                     │
                                     ├─► ToolResult 立刻返回（:1293）     ← **工具行当场闭合**
                                     │   「its result will be delivered … do NOT poll」
                                     │
                                     └─► tokio::spawn {                    ← 分离任务
                                           ChildTask::execute（:749）
                                             ├─ CollectingSink::new()（:801）   ← 唯一采集点
                                             ├─ child.run_turn(..)              ← 全程无外部可观测
                                             ├─ write_transcript（:836）        ← **跑完才建文件**
                                             └─ results.deliver(handle, msg)（:606-614）
                                                   ├─ 已绑定落点 ⇒ push(Boundary::Deferred)
                                                   └─ 未绑定 ⇒ 留表（下回合开头补投，app_loop.rs:1227-1231）
                                         }
                                     ▼
                    BasicLoop drain_boundary（basic.rs:102-111）
                                     │ emit AgentEvent::DeferredResult{handle, message}
                                     ▼
                    app_loop 转发（:1281）→ TUI app.rs:427-445「↓ sa1 finished: …」
                                     │ append 进 session → 下一轮 ModelRequest 可见
```

**五个可观测空窗**（本需求的全部靶子）：

1. `spawn` 返回到 `execute` 收尾之间 —— **零事件**；
2. 工具行在 `ToolResult` 处闭合 —— 之后 TUI 看不出它还在跑；
3. 产物文件不存在 —— 没有旁路可查；
4. 「完成」与「送达」是两个时刻，现状 UI 用同一个词 `finished`；
5. 句柄跨回合重复 ⇒ 任何按句柄索引的东西都会串。

### 外部依赖

无新增依赖需求。`Notify` / `Mutex` / `VecDeque` 全在既有的 tokio + std 里；
`ys-event` 加两个类型（无新 crate）。`futures-util` 已在 `Cargo.lock`（ADR-0029 落地时引入）。

---

## 约束

### 技术

- **依赖方向单向**；`ys-core` / `ys-event` / `ys-protocol` **零 tokio**；`ys-tui-coding`
  **不依赖** `ys-runtime`。
- **不跨动态库边界传 trait object** ⇒ 静态装配。
- **Agent 无状态**（ADR-0010）⇒ 状态不能挂在 `Agent` 上；本方案挂**进程级表**（照 `PendingResults`）。
- **`BoundarySource` 每回合新建** ⇒ 任何「每回合绑落点」的结构必须 RAII 守卫
  （`PendingResultsBinding`，`subagent.rs:286-295`）。
- **`ContentBlock` / `Boundary` 是落盘/线格式契约** ⇒ 本轮**不改**它们（新增的是
  `AgentEvent` 变体与 app 侧结构）。
- **`AgentEvent` 是 `#[non_exhaustive]`**，但 **`Outbound` 不是** ⇒ 加 `Outbound` 变体必须改 TUI 的
  穷尽 match（`app.rs:189-233`，无 `_` 兜底）。

### 性能

- **prompt cache 是一等要求**：父 tools JSON → system prompt → messages 是字节前缀。
  子表必须仍是父表 JSON 的字节前缀（`assembly.rs:184-185` 的「追加末位」契约，
  `toolset.rs:644-648` 钉住）。⇒ **新工具只能追加末位，且不碰现有工具的 spec**。
- **每轮固定开销**经 `fixed_prompt_tokens` 进压缩判定（`limits.rs:27-36`），由
  `main.rs:1074-1077` 现算 ⇒ 加工具自动重算，但**每次派发**都要重付这部分。
- **事件频率**：子 agent 一次派发可能几十次工具调用。任何「一个动作一条事件」的设计都会
  在 TUI 与 `--json` 上放大成洪泛 —— 必须**按轮**聚合。
- **空闲态不许轮询**：`BackgroundTasks` 用 100ms sweeper（`app_loop.rs:79-136`），那是因为
  要收尸子进程。本需求没有进程要收，故用 `Notify`（有变化才醒）。

### 演进

- **ADR-0014「零消费者 ⇒ 不建」**：本需求的每个新结构都必须点名它的消费者
  （板上状态有四个：TUI 状态段、TUI 转换行、`--json`、可选状态工具）。
- **ADR-0022 决策 2 的门是关着的**：子事件不进父流。翻案条件写在 ADR 里
  （「出现要在 UI 上看到子 agent 逐条工具调用的真实需求」）—— 本轮需求**够不到**。
- **ADR-0029 §3.4 是欠账**：产物路径先分配 + 增量落盘**写成要求但没做**
  （`docs/arch/subagent-rebaseline/design.md:184` / `:186-187`）。本轮 R1 与它同向。
- **`bash_poll` 的教训是硬约束**：`docs/arch/bg-notify/design.md` §1 的实测（16 次启动 /
  68 次 poll / 54 次零信息 / 单句柄 24 次）说明「带 wait 的查询工具」必然退化成轮询。
  ⇒ 任何状态查询工具**不得有 wait 语义**。
- **todo 侧栏也在等同一处 TUI 改动**（`docs/arch/long-horizon-todo/design.md:214` 的
  `CodingView.todos` + 侧栏）⇒ 若本轮做侧栏，宜合并；若本轮只做状态行，两者互不阻塞。

### 组织

- 单人维护 ⇒ 刀要小、每刀可独立验收；**不做**跨 crate 的框架式抽象。
- **判断链**（`docs/extension-points.md`）：
  - TUI 显示 → **Event 消费者**；
  - 模型主动查 → **Tool**；
  - 「收到结果后说一句」→ **提示词 guideline**（不是 Hook）。
  - ⇒ **零个需要 hook 系统。**

---

## 需求范围

### 范围内

- 进程级**状态板**（一次派发一条，跑着就在写）+ 推送式进度事件；
- **全局唯一句柄**（前置修正：现状会让任何按句柄索引的结构串号）；
- **增量落盘**（既有设计的欠账，顺带让「跑着也有文件可 tail」成立）；
- TUI：状态行一段 + `started` / `finished` / `delivered` 三种转换行；
- 完成消息加**结构化头**（rounds / tool_calls / elapsed）；
- 提示词：替换一条**无法遵守**的 guideline，新增「收到结果后报一句现状与下一步」。

### 范围外（明确不做）

- ❌ 子 agent 的逐条工具调用进 TUI（`Source::subagent`）—— ADR-0022 决策 2，条件未到；
- ❌ 新 `Outbound` 变体（会在回合内出现 ⇒ 打红 `app_loop.rs:2216-2219` 的不变量）；
- ❌ 带 `wait` 参数的查询工具（会复现 `bash_poll`）；
- ❌ 新的 `TranscriptLine` 变体（三处编译 + 四处 `_ =>` 静默漏掉）；
- ❌ `UsageTotals` 改成按句柄精确账单（**成本聚合**是独立欠账，`docs/TODO.md` §A3）；
- ❌ 子 agent 的逐轮**精确**模型/token 读数（板上是覆盖写的近似值）。

### 关键场景

1. **回合内 + 异步派发**：模型派 2 个后台子 agent → TUI 状态行出现 `◦ 2 subagents` →
   其中一个每轮更新 → 第一个完成 → 状态行变 `◦ 1 subagents · ↓ sa3.1 待送达` →
   轮边界送达 → 状态行回 `◦ 1 subagents`，transcript 出一行
   `↓ sa3.1 结果已注入会话（第 6 轮）· completed · 7 轮 · 96s`。
2. **回合结束后仍在跑**（实测会发生：转录在回合结束后 2~6 分钟才落盘）：
   主 agent 已收工，TUI 仍显示 `◦ sa3.2 · read · 第4轮 · 2m10s` ⇒ 用户知道 token 还在烧。
3. **主 agent 要查**：模型在处理别的任务时想确认「能不能换到另一件事」，调一次
   `subagent_status` 拿到两行状态（**不是**轮询，因为结果本来就会自己送到）。
4. **失败的子 agent**：状态行 → `✗ sa3.2 失败: …`（Error 行），板上 `Failed`，
   提示词要求「先读诊断再决定缩窄重试或报告」。
5. **外闸 `drop`**（连轮边界都到不了）：盘上已有前若干轮转录，状态行最终因条目超龄被 `prune`。

### 异常/边界场景

- 板满（32 条）且**全部在跑** ⇒ 不淘汰，只记日志（上界由每回合 16 次派发保证）；
- 子 agent 组装失败（`AgentBuilder::build` 返回 `Err`）⇒ `Failed`，**不经** `Running`；
- `/new` / `/resume` 期间有子在跑 ⇒ 板**不清理**（进程级，与 `PendingResults` 一致）；
- `-p` / `--json` 模式：`SubagentProgress` 属非终局事件，**不影响**消费循环退出条件；
  `--json` 会多出这些行（`Envelope` 全量序列化，`main.rs:531-534`）；
- 极窄终端：状态段排在「轮号」之前 ⇒ **比轮号更不容易被剥掉**；宽度只够 model 时整段消失。

---

## 未澄清问题（待用户确认）

- [ ] **Q1** 要不要 `subagent_status` 工具？（R1 的「主动查」只有它可以给；代价是父表 13 元、
      父前缀 +~500 字节、5 处装配断言、并承担「模型拿它轮询」的风险）
- [ ] **Q2** 展示形态：状态行一段 / 右侧栏（要动两处纵向切分，宜与 todo 侧栏合并）/ roster 行；
- [ ] **Q3** 回合外是否也刷新 `Outbound::View`（`Wake::Subagents` 分支）？不做则空闲态看不见
      「还在烧 token」；
- [ ] **Q4** 增量落盘（`subagent-rebaseline/design.md:184-187` 的欠账）是否本轮做？
- [ ] **Q5** 「主 agent 报现状与下一步」放 system guideline（推荐）还是注入消息尾部指令
      （会被压缩一起摘要掉）？

---

## 后续建议

- 方案与分刀见 [`design.md`](design.md)（六刀 + 每刀验收 + 变异测试清单）。
- 用户裁决 Q1–Q5 后再写 ADR；**Q1 = 否** 时 design §3.9 整节作废，刀 6 删除。
