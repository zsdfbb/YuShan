# turn-reliability-and-lean-context — 架构设计

> 日期：2026-10-13 ｜ 产出：`/arch-design`（基线 `5729401`，分支 `fix/turn-reliability-and-lean-context`）
> 上游：[`context.md`](./context.md)（现状证据 + 四条根因 + 约束）
> 决策：[`adr-turn-reliability-and-lean-context.md`](./adr-turn-reliability-and-lean-context.md)
> 同类对照：[`research.md`](./research.md)（**跨产品事实只在那里**）

## 概述

一句话：**让回合永远走到「有可见产出」或「明确失败」，并给工具输出加一道回合级闸门。**

拆成三个可独立回滚的切片：**A1** 适配器把「一个字节都没到」判成失败（借既有有界重试）；
**A2** 把自动追问的门从「仅自称完工」扩到「空产出 / 被截断」；**B1** 回合级工具输出预算
（循环侧定价、工具侧收窄）。A1、A2 先落地，B1 随后。

## 需求确认（Step 1）

| 项 | 内容 |
|---|---|
| 关键场景 | S1 模型沉默 ⇒ 自动追问一次，用户看到「继续」而不是停摆；S2 网关抖丢答复 ⇒ 有界重试，耗尽才是明确失败；S3 探索不再把 150 KB 级工具输出堆进会话；S4 截断的回合有界接力 |
| 质量目标 | 可变异测的**布尔断言**进 CI（无产出不再终结回合、超预算必被收窄）；「省了多少 token」用**离线对照**（`/tmp/ysprobe` 重放 + 一个小 `--json` 任务） |
| 约束 | 零新依赖；不加 Hook；`ys-core`/`ys-event`/`ys-protocol` 不含 tokio；终局事件不变式（先 `RunFailed` 再 `Err`）；不放宽任何输出上限；工具描述/参数面是对模型可见的契约（改需同步 `prompt.rs` + `EXPECTED_ORDER`） |
| 非目标 | 不改 `max_tokens=4096`；不改工具参数面；不落盘 spill；不新增跨 crate 事件；不动压缩策略；不修 `MAX_CONSECUTIVE_ERRORS` 空守卫 |
| 已定口径（用户接受） | ① A + B、A 先行；② B 压「每轮体积」优先；③ 收窄只作用于**之后产生**的结果，不改写已有历史消息（改写会废掉该点之后的缓存前缀）；④ 先取证据再动输出预算；⑤ 验收 = CI 布尔断言 + 离线对照 |

## 设计方案（推荐）

### 模块划分

| 层 | 文件 | 职责 | 依赖方向 |
|---|---|---|---|
| 适配器 | `adapters/model-openai-compatible/src/lib.rs` | `StreamAccumulator::finish()`：**零产出 + 零终止信号 ⇒ `Err`**；空产出/截断落一行现场日志 | 不动 |
| 循环 | `crates/ys-loop/src/basic.rs` | 回合级**工具输出账本**：批前算预算、批后记账；收窄由工具执行（预算经 `ToolContext` 下达） | 不动（已依赖 `ys-tool`） |
| 容器 | `crates/ys-component/src/limits.rs` | `RunLimits.tool_result_turn_bytes`（**默认 0 = 关**，加法式，既有调用方行为逐字节不变） | 不动 |
| 工具契约 | `crates/ys-tool/src/{context.rs,truncate.rs}` | `ToolContext.result_budget: Option<ResultBudget>`（默认 `None`）；`FLOOR_BYTES` 常量 | 不动 |
| 工具实现 | `adapters/tools-basic/src/{read,grep,find,ls,bash}.rs` | `cap = min(自身上限, 预算)`，各自保留自己的截断方向与通知 | 不动 |
| App | `apps/coding-agent/src/{follow_up.rs,main.rs}` | 追问判据扩门 + 派生预算值 | 不动 |

**不新增 crate、不新增 trait、不新增事件变体、不新增配置文件项。**

### 关键数据流

**三格交界（本设计的核心判据）**

```text
                       ┌─ 一个字节都没到（零正文/零思考/零工具，且 finish_reason 与 [DONE] 都没出现）
adapter.finish() ──────┤      ⇒ Err(Provider) ⇒ ADR-0028 有界重试（5 次退避）⇒ 耗尽才 RunFailed
                       ├─ 到了数据，但没有可见产出（finish_reason=stop 或 [DONE]）⇒ Ok ⇒ StopReason::NoOutput
                       └─ 到了数据，但被输出预算截断（finish_reason=length）⇒ Ok ⇒ StopReason::Truncated
                                                                                  │
                                    app 回合边界 ── follow_up_for(Initial, …) ──────┘
                                       NoOutput  ⇒ 追补文案（不看轮数）
                                       Truncated ⇒ 续写文案（不看轮数）
                                       Completed 且够长 ⇒ 复核文案（ADR-0027 不变）
                                       └─ 追问回合（TurnKind::FollowUp）恒不再追问 ⇒ 每用户输入至多 +1 回合
```

**回合级工具预算（B1）**

```text
每批工具调用前： allowance = max(FLOOR_BYTES, (turn_bytes − spent) / 批内条数)
                ⇒ ToolContext.result_budget = Some(ResultBudget{ visible_bytes: allowance })
每个工具      ： cap = min(自身硬上限, visible_bytes)
                read  ⇒ 保头 + 既有「Use offset=N to continue」通知（天然出现）
                bash  ⇒ 保尾（错误在尾）+「已按回合预算收窄」通知
                grep  ⇒ 保头 + 收窄指引（更窄的 pattern/path/context）
                find/ls ⇒ 保头 + 既有截断通知
9c            ： ledger.record(实际回喂字节) + 落一行日志（spent / narrowed 条数）
```

- **记账口径 = 实际回喂字节**（收窄后），因为模型的成本按回喂算。
- **不改写历史**：收窄只发生在 `emit(ToolResult)` / `session.append` **之前**。
- **不新增事件**：收窄事实由工具自带的 notice 可见（用户与模型同源），另落一行日志；新增事件留作 v2。

### 接口概要

```rust
// ① crates/ys-loop/src/basic.rs —— 账本（纯数据、零 IO，可变异测）
/// 回合级工具输出预算（`turn_bytes == 0` ⇒ 关闭，走现状）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnBudget { pub turn_bytes: usize, pub floor_bytes: usize }

#[derive(Debug, Default)]
pub(crate) struct ToolLedger { spent: usize, narrowed: u32 }

impl ToolLedger {
    /// 批前定价：剩余预算均摊到本批（无批内条数时为 1），下钳 `floor_bytes`。
    /// 均摊而非「先到先得」：同批是并行执行、返回顺序不定，预算是**顺序无关**的。
    pub(crate) fn allowance(&self, b: TurnBudget, batch: usize) -> usize;
    /// 批后记账：以**实际回喂**字节累加，并记收窄条数（诊断用）。
    pub(crate) fn record(&mut self, visible_bytes: usize, narrowed: bool);
}

// ② crates/ys-tool/src/context.rs —— 下达（默认 None ⇒ 既有工具测试逐字节不变）
pub struct ToolContext<'a> { /* …既有… */ pub result_budget: Option<ResultBudget> }
pub struct ResultBudget { pub visible_bytes: usize }
impl<'a> ToolContext<'a> { pub fn with_result_budget(self, b: ResultBudget) -> Self; }

// ③ crates/ys-tool/src/truncate.rs
/// 收窄下限：预算见底后每条结果仍保留的头部字节数（`read` 仍可用 offset 续读）。
pub const FLOOR_BYTES: usize = 1024;

// ④ apps/coding-agent/src/follow_up.rs —— 追问判据扩门（结构不变）
pub(crate) const RECOVER_PROMPT: &str = "…你上一条回复没有可见产出，请继续：要么用工具推进，要么给出答复…";
pub(crate) const TRUNCATED_PROMPT: &str = "…上一条回复被输出预算截断，请接着写；必要时拆成更短的步骤…";

pub(crate) fn follow_up_for(kind: TurnKind, result: &RunResult) -> Option<AgentInput>;
// Initial + NoOutput                     ⇒ RECOVER_PROMPT（不看 rounds）
// Initial + Truncated                    ⇒ TRUNCATED_PROMPT（不看 rounds）
// Initial + Completed ∧ rounds ≥ 5       ⇒ REVIEW_PROMPT（不变）
// FollowUp / Cancelled / MaxRounds / 短回合 Completed ⇒ None
```

### 落点清单

| # | 落点 | 改法 |
|---|---|---|
| A1-1 | `adapters/model-openai-compatible/src/lib.rs`（`StreamAccumulator`） | 加字段 `saw_done: bool`；`StreamEvent::Done => self.saw_done = true`（原 `{}`） |
| A1-2 | 同上 `finish()`（error 检查与 `filter.flush()` 之后） | `text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() && finish_reason.is_none() && !saw_done` ⇒ `Err(ModelError::Provider("stream ended with no data and no terminator …"))` |
| A1-3 | 同上 `complete()` 的 `Ok(response)` 臂 | 无可见产出或 `is_truncated()` 时 `self.log("empty/truncated response: finish_reason=… usage=…")`（**只写日志**） |
| A2-1 | `apps/coding-agent/src/follow_up.rs` | 加两条常量；`follow_up_for` 扩两个门（`NoOutput` / `Truncated`），`TurnKind` 二态不动 |
| A2-2 | `apps/coding-agent/src/app_loop.rs:343` | **不改**（判据全在 `follow_up_for`） |
| B1-1 | `crates/ys-component/src/limits.rs` | `RunLimits` 加 `tool_result_turn_bytes: usize`，`Default` 填 **0**（承重：库侧行为不变） |
| B1-2 | `crates/ys-tool/src/truncate.rs` | 加 `FLOOR_BYTES = 1024` |
| B1-3 | `crates/ys-tool/src/context.rs` | 加 `result_budget: Option<ResultBudget>` + `with_result_budget`（默认 `None`） |
| B1-4 | `crates/ys-loop/src/basic.rs`（Step 9 前 / 并行 / 串行 / 9c） | 建 `ToolLedger`；批前 `allowance`、批后 `record`；两处 `ToolContext::new(…)` 挂 `.with_result_budget(…)` |
| B1-5 | `adapters/tools-basic/src/{read,grep,find,ls}.rs` | `cap = min(自身上限, budget.visible_bytes)`；`None` ⇒ 现状（逐字节不变） |
| B1-6 | `adapters/tools-basic/src/bash.rs` | 同上，但保**尾** |
| B1-7 | `apps/coding-agent/src/main.rs`（`resolve_limits`） | `limits.tool_result_turn_bytes = config.context_window() / 8 * 4`（128k ⇒ 64 KiB；**唯一接线点**，三种模式共用） |

### 测试与变异点

| # | 用例 | 变异靶（改哪行变红） |
|---|---|---|
| A1-T1 | 零 delta + 无 `finish_reason` + 无 `[DONE]` ⇒ `finish()` 返回 `Err` | 删判据里任一合取项 ⇒ 红 |
| A1-T2 | 有 `finish_reason=stop`（或有 `[DONE]`）+ 零内容 ⇒ 仍 `Ok`（`NoOutput` 不误伤） | 去掉 `finish_reason.is_none()` 守卫 ⇒ 红 |
| A1-T3 | 回环服务器回 `200 + 非 SSE 体` ⇒ 重试一次后成功；请求数 = 2 | 让 `finish()` 恒 `Ok` ⇒ 请求数 = 1 ⇒ 红 |
| A1-T4 | 重试耗尽 ⇒ 先 `RunFailed` 再 `Err`（终局不变式） | 交换顺序 ⇒ 红 |
| A2-T1 | `follow_up_for(Initial, NoOutput{rounds:1}) = Some(RECOVER_PROMPT)`，文案逐字节相等 | 恢复 `rounds` 门槛 / 删 `NoOutput` 臂 ⇒ 红 |
| A2-T2 | `follow_up_for(Initial, Truncated{rounds:1}) = Some(TRUNCATED_PROMPT)` | 同上 ⇒ 红 |
| A2-T3 | `FollowUp` 回合恒 `None`（含它自己又空产出） | 删 `kind != Initial` 早退 ⇒ 红 |
| A2-T4 | `app_loop`：空产出回合出**恰好 2 条** `RunFinished`（主 + 追补） | `pending = None` ⇒ 1 条 ⇒ 红 |
| B1-T1 | `ToolLedger::allowance` 表驱动（含 `batch` 均摊、`floor` 下钳、`turn_bytes == 0`） | 删下钳 / 改除为乘 ⇒ 红 |
| B1-T2 | `RunLimits::default().tool_result_turn_bytes == 0`（承重默认） | 默认填非零 ⇒ 红（同 `fixed_prompt_tokens` 的先例） |
| B1-T3 | 注入 `ResultBudget` 后 `read`/`grep` 正文 ≤ cap 且带通知；注入 `None` 时**逐字节等于现状** | 让工具读「默认预算」⇒ 既有字节断言红 |
| B1-T4 | loop 集成：预算关闭时既有批次事件序列（`C1,R1,C2,R2`）逐字节不变 | 无条件收窄 ⇒ 形状测试红 |
| B1-T5 | loop 集成：超预算后**之后**的结果被收窄、**之前**的不动 | 改写已 append 的消息 ⇒ 「不改历史」断言红 |
| 离线 | `/tmp/ysprobe` 重放同一会话：工具可见字节 ≤ 基线 × 0.6，且轮数不增 | — |

### 成本核算（以失败会话为样本）

样本：turn 1 = 13 轮 / 21 条工具结果 / 156,073 B ≈ 51.4k est token（会话 ≈ 55.8k，工具占 92%）；
`turn_bytes = 64 KiB`、`floor = 1 KiB`。

| 项 | 基线 | 改造后 | 差 |
|---|---:|---:|---:|
| 工具可见字节 | 156.1 KB | ≈ 81 KB（前 3 条全宽 + 后 18 条被收窄） | −75 KB |
| 工具 est token | 51.4k | ≈ 26.6k | −24.7k |
| 每轮固定开销（system + 工具表） | 不变 | **不变**（无新工具、无新 guideline、`fixed_prompt_tokens` 不动） | 0 |
| 追问文案 | — | 每个异常回合一条 `Role::User`（≈ 40–60 token，1.0×，仅空产出/截断时出现） | 低频 |
| 追问的真实代价 | — | 触发一次 = **一次完整模型调用**（本来会是「静默停摆 + 用户手动重问」，后者更贵） | 净收益 |
| 轮数 | 13 | 13（B1 不承诺减轮；减轮属未纳入项） | 0 |

## 候选方案对比（Step 4）

| 维度 | A 最小复杂度 | B 可扩展优先 | C 性能/资源优先 | **推荐（融合）** |
|---|---|---|---|---|
| 核心形状 | 三处判据 + 一个常量阈值（`TurnToolBudget`，循环侧 binary latch + 8 KiB 收窄） | `TurnHealth`（`ys-core`）+ `relay_plan`/`narrow::plan` 纯函数 + `RunLimits` 两字段 | 回合账本 + **工具侧**应用预算 + **落盘指针**（推广 `webfetch` 形态）+ 新事件 | **A 的判据与接力 + C 的工具侧收窄（去落盘）+ B 的「预算默认关」纪律** |
| 实现复杂度 | 最低（~6 处小改） | 最高（新类型 7~8 个、`RunResult` 13 个构造点、退出码/图标收敛） | 中高（新模块 2 个、`ToolContext`/`RunLimits` 加字段、4 个工具 + 落盘 + 事件） | 中（加字段 2 个 + 4 个工具各 1~2 行） |
| 性能/成本 | 零常驻成本；减量有限（binary latch 后仍可 8 KB×n） | 零常驻成本；减量取决于 `narrow::plan` | 减量最大（连续递减 + 落盘）；指针文本有小额开销 | 减量接近 C（连续递减），无落盘面 |
| 演进灵活性 | 低（第五种原因/第二维度要重踩读点） | 高（加一个 `match` 臂 / 一个字段） | 中（预算维度加字段即可） | 中（**保留升级判据**：出现第二个「非 `stop_reason` 驱动」的接力原因时再提升为 `relay_plan`） |
| 排障负担 | 低（日志一行） | 中（多一层类型要读） | 中（多一条事件与落盘目录） | 低 |
| 主要风险 | bash 被通用 head 截掉尾部错误 | 过度抽象（`TurnHealth` 长成 god struct） | 掐死正常探索；指针文案成为对模型可见的契约；落盘失败路径 | 预算值需校准；`-p`/`--json` 不接力（见未澄清） |

### 被否理由

- **B 的 `TurnHealth` + `relay_plan` 抽象**：第二个消费者的论证**成立**（今天有 7 处读 `StopReason`，ADR-0024 还要加一处），但本轮 A 只收敛**一个**站点；若一并把退出码、TUI 图标、子 agent 诊断改读「处置」，爆炸半径会从「三个文件」扩到「四个 crate」。**升级判据**：ADR-0024 落地（出现第二个非 `stop_reason` 驱动的接力原因）时，把 `follow_up_for` 提升为 `relay_plan(RelayInputs)` —— 那时抽象有真实第二消费者，不是预建。
- **C 的落盘 + 指针**：需要 `ToolContext` 注入 + spill 目录解析 + 原子写 + 指针文案（**对模型可见的新契约**）+ 新事件。本仓 `webfetch` 已证明这条路可行，但把它推广到 `grep`/`bash` 之前，应先用基线证明「收窄 + 明确指引」不够。**列为 v2 第一候选**。
- **循环侧通用 `head` 收窄（A 的简版）**：`bash` 的关键信息在**尾部**，通用 head 会截掉错误；而「保头/保尾」本来就分属各工具。故采纳 C 的分工（循环定价、工具收窄）。
- **`read` 默认窄读 / 多目标参数面 / 布局快照**：都动「对模型可见的契约」（需同步 `prompt.rs` + `EXPECTED_ORDER` 漂移测试），收益是**减轮次**，而本仓目前没有可对照的基线 ⇒ 留待 B2。

## 未纳入项（记录，非本轮）

- 用 provider 返回的**真实 `usage`** 决定压缩触发（比现在的文本估算更准）——属 ADR-0017 的修订面。
- 修 `MAX_CONSECUTIVE_ERRORS` 空守卫（「原地打转」目前只有 `max_rounds` / 48 h wall-clock 兜底）。
- 抬 / 按模型给 `max_tokens`：先由 A1-3 的日志取证据。
- 把收窄做成**可见事件**（TUI 一行）：协议面成本换可见性，等 B1 稳定后再评。

## 未澄清问题

- [ ] **Q1 · 预算初值**：`context_window / 8` 换算成字节（128k ⇒ 64 KiB）是否合适？还是先只上 A1/A2、B1 等基线量完再合？
      待用户确认
- [ ] **Q2 · 一次性模式**：`-p` / `--json` 走 `run_single_turn`（`main.rs:1001/1116/1155`），**不经** `app_loop` 的追问循环，且见首个终局事件即停 ⇒ 本轮**不接力**（保留 `NoOutput ⇒ exit 1` + stderr）。是否接受这一不对称？ 待用户确认
- [ ] **Q3 · 可见性**：收窄只在工具 notice 与日志里可见，不进事件流；TUI 是否需要在回合末给一行「本轮有 N 条结果被收窄」？ 待用户确认

## 相关文档

- 现状与证据：[`context.md`](./context.md)
- 决策记录：[`adr-turn-reliability-and-lean-context.md`](./adr-turn-reliability-and-lean-context.md)
- 同类对照：[`research.md`](./research.md)
- 既有相邻设计：[`../llm-api-unavailable/context.md`](../llm-api-unavailable/context.md)、[`../tool-call-efficiency/context.md`](../tool-call-efficiency/context.md)、[`../turn-output-and-feedback/design.md`](../turn-output-and-feedback/design.md)
- ADR：`docs/adr/0020`（截断保护）、`0027`（完工复核 + 轮数预算）、`0028`（重试 + 超时）
