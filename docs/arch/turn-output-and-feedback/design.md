# turn-output-and-feedback — 架构设计

> 日期：2026-10-12 ｜ 产出：`/arch-design`（基线 `3c9aef5`）｜ **修订 v2**（按 [`review.md`](./review.md) 的 P1 发现修复）
> 上游：[`context.md`](./context.md)｜决策：[`adr-turn-output-and-feedback.md`](./adr-turn-output-and-feedback.md)｜质量分析：[`review.md`](./review.md)

## 概述

一句话：把「**单回合的工具输出规模**」收进边界、让「**回合的产出与进展可见**」，两项一起做，
每项都有**可量化、可测试**的验收，且指标**直接测危害（体积 / token）**而非只看「有没有提示」。

两个子问题：**A · 一次性读取太多内容**（工具面）；**B · 改进过程迟迟没有回复**（循环 / 渲染面）。

> v2 相对 v1 的关键修订：A 的**主手段从「只加提示」改为「真收窄」**（`grep` 专用结果预算）；
> 验收指标补「单条结果字节 / 当轮回喂 token」；明确 `NoOutput` 的**退出码**与**与 `llm-api-unavailable` 的边界**；
> 空响应判定改用 `trim()`；删去过程性注记。理由见 `review.md` 的 R1–R3。

## 需求确认（Step 1）

| 项 | 内容 |
|---|---|
| 关键场景 | S1 窄探索不产生 45KB 级结果；S2 大结果**被收窄**且有提示；S3 空响应有可见说明；S4 长循环有过程叙述 |
| 质量目标 | **可量化、可测试**：单条结果 max ≤ 预算；空 assistant 回合数 → 0；全量用例不降级 |
| 约束 | 不引新依赖；**不放宽任何输出上限**（**收紧合规**）；`ys-core/ys-event/ys-protocol` 不含 tokio；`ModelEventSink` 同步 `fn`；不动 Agent 无状态 / Boundary / 信道 / 取消四条不变式 |
| 非目标 | 传输层空响应 / 超时 / 半截流（归 `llm-api-unavailable`）；自动重试 / 故障转移；放宽上限；持久 shell；按每轮注入常量文本 |
| 与既有决策的边界 | `llm-api-unavailable` 把「合法空回复」判为正常（其场景 4）；本需求**有意收窄**这一档 —— 见 §边界 |

## 同类软件调研（Step 2）

> 来源：本仓既有竞品调研 `docs/arch/subagent/research.md`（聚合 9 个同类项目）+ `docs/arch/tools-align-pi/web-research.md`。
> 本轮外部检索工具（Tavily）密钥不可用，故以仓内调研为准（已覆盖所需口径）。

| 项目 | 工具输出规模 | 空 / 无产出响应 |
|---|---|---|
| **opencode** | 所有工具输出统一过 truncate（2000 行 / 50KB），超限**落盘 + 回喂指针**（*"Do NOT read the full file yourself"*） | Kimi/opencode **#37852**：aborted stream 被记成正常结束 ⇒ **返回空且无错误** |
| **Codex CLI** | 只在**出错时**把输出截到 900 token | — |
| **deepseek-harness** | `diagnostic` 截到 4096 B | — |
| **Roo Code** | 返回**无上限** ⇒ **#11364**「子返回被截断 ⇒ 父不得不自己重读」（**截断而无指针 = 最坏形态**） | — |
| **Cline** | — | **#9637**：因 context-window 超限报错 |
| **本仓既有立场** | 子 agent 设计 D4「**硬截断 + 落盘指针**」；`ys_tool` 上限 2000 行 / 50KB | ADR-0001 失败双通道；`StopReason::Truncated`「**不许静默说谎**」 |

**可借鉴 / 不适用**：

- **可借鉴（本版采纳）**：opencode 的「**硬上限 + 一句可执行的下一步**」组合 —— 本需求把它落到
  `grep` 的**专用结果预算**（真收窄）+ 截断 notice 的**收窄指引**；后者单独用（v1 的方案）治不了当轮体积。
- **不适用**：opencode 的**落盘 + 指针**依赖「有个文件可回读」，而 `grep` 的结果是**搜索结果**不是文件 ——
  落盘无意义，故本需求用「收窄 pattern/path/context」作指针。
- **反面对照**：Roo「有界但无指针」与 Kimi「空响应记成正常结束」正是本需求要治的两种病；
  `Truncated` 先例证明「**给终止一个诚实的名字**」是本仓既有立场，不是新发明。

## 设计方案（推荐）

### 模块划分

```text
① ys-core      StopReason +1 变体（NoOutput）              —— 词汇层，零 tokio
② ys-loop      basic.rs 空响应分支选停止原因（trim 判定）  —— 控制流层
③ ys-tool      软阈值常量 + notice 拼接助手                —— 形态统一（grep/read 共用）
④ tools-basic  grep：专用结果预算（真收窄）+ 软阈值；read：软阈值
⑤ apps/tui     prompt guideline；draw/app 渲染 NoOutput；main 退出码
```

| 模块 | 职责 | 核心抽象 | 复用 / 新增 |
|---|---|---|---|
| `ys-core` | 终止原因的词汇 | `StopReason::NoOutput` | **新增变体**（照 `Truncated` 先例，加法式） |
| `ys-loop` | 判「本轮有没有可见产出」并选停止原因 | 复用已有 `content` 扫描 + `trim()` | **复用** |
| `ys-tool` | 软阈值常量 + notice 文案形态 | `SOFT_RESULT_BYTES` + `size_notice(...)` | **新增常量/小助手**（grep、read 两个真实消费者） |
| `tools-basic` | 单条结果**收窄**与提示 | `GREP_MAX_RESULT_BYTES`、`grep::build_result`、`read` 软阈值 | 改动 |
| `apps/coding-agent` + `ys-tui-coding` | 提示词纪律 + 可见性 + 退出码 | `tool_guidelines`、`stop_symbol`、`RunFinished` 臂、`single_turn_exit_code` | 改动 |

### 关键数据流

**B · 空 / 纯思考响应（承重路径）**

```text
StreamAccumulator::finish()  → content = []（reasoning 只发事件、不进 content）
        │
basic.rs:448  if tool_calls.is_empty()
        ├─ text.trim() 非空 ──► RunFinished{ Completed } + final_message = Some(..)   （不变）
        └─ 空（含空白）     ──► RunFinished{ NoOutput }  + final_message = None        ← 本次改动
                                    │
   ys-tui-coding/app.rs  RunFinished 臂：NoOutput ⇒ Summary(NoOutput) + 一行 System（可见）
   apps/coding-agent/main.rs  single_turn_exit_code：NoOutput ⇒ 1 + stderr 一行        ← 不可静默装作成功
```

**A · 工具结果收窄**

```text
grep.rs build_result
  context   ← clamp(context, MAX_CONTEXT=5)                         ← 防更宽放大（非主手段）
  lines     ← render_entries(entries, limit, context)
  truncated ← head_lines(lines, MAX_LINES, GREP_MAX_RESULT_BYTES=16KiB)   ← ★ 真收窄（主手段）
  notices   ← [limit 命中?, 截断?（带「收窄」指引）, 超长行?, 渲染 ≥ 8KiB 软阈值?]
```

### 接口概要

```rust
// ① crates/ys-core/src/stop.rs —— 加法式新增（#[non_exhaustive]，下游已有 `_ =>` 兜底）
pub enum StopReason { Completed, MaxRounds, Cancelled, Truncated, NoOutput }

// ③ crates/ys-tool（truncate.rs / notice.rs）
pub const SOFT_RESULT_BYTES: usize = 8 * 1024;               // 软阈值（提示用）
pub fn size_notice(bytes: usize, matches: Option<usize>) -> String;  // 统一文案形态（`None` = 无命中数概念，如 read）

// ④ adapters/tools-basic/src/grep.rs
pub const GREP_MAX_RESULT_BYTES: usize = 16 * 1024;          // ★ 专用预算（收紧，真收窄）
const MAX_CONTEXT: usize = 5;                                // context 上限

// ⑤ apps/coding-agent/src/main.rs —— single_turn_exit_code 增加 NoOutput 臂（⇒ 1）
```

### 落点清单

| # | 落点 | 改法 |
|---|---|---|
| A0 | `adapters/tools-basic/src/grep.rs:191-256` | 渲染输出按 `GREP_MAX_RESULT_BYTES`（16 KiB）截断；截断 notice **带收窄指引** |
| A1 | `crates/ys-tool/src/truncate.rs` | 加 `SOFT_RESULT_BYTES` |
| A2 | `crates/ys-tool/src/notice.rs` | 加 `size_notice`（形态统一） |
| A3 | `adapters/tools-basic/src/grep.rs` | `context` 钳到 `MAX_CONTEXT` |
| A4 | `adapters/tools-basic/src/read.rs` | 软阈值 notice（`offset/limit` 提示已有，**不设**专用预算） |
| A5 | `apps/coding-agent/src/prompt.rs:154` | `tool_guidelines` 加「先窄后宽」一条 |
| B1 | `crates/ys-core/src/stop.rs` | 加 `NoOutput` 变体 + 文档注释 |
| B2 | `crates/ys-core/src/lib.rs:92-95,107` | 变体列表补一项 + 照 `Truncated` 加 roundtrip 测试 |
| B3 | `crates/ys-loop/src/basic.rs:448-469` | `text.trim().is_empty()` ⇒ `stop_reason = NoOutput` |
| B4 | `crates/ys-tui-coding/src/draw.rs:605-613` | `stop_symbol` 加 `NoOutput` 分支 + 表测补一行 |
| B5 | `crates/ys-tui-coding/src/app.rs:263-274` | `RunFinished` 臂：`NoOutput` ⇒ 推一行可见 `System` 提示 |
| B6 | `apps/coding-agent/src/main.rs:1166-1176` | `single_turn_exit_code`：`NoOutput ⇒ 1`；stderr 一行；更新 doc 注释（原因集合补 `NoOutput`） |
| 文档 | `docs/CONTEXT.md`、`docs/design.md` §5/§12 | 终止原因定义 + 测试矩阵 |

### 边界：与 `llm-api-unavailable`（谁负责什么）

| 情形 | 判定层 | 归属 | 结果 |
|---|---|---|---|
| 传输层空（200 + 非 SSE 体 / 零 delta，`finish_reason` 无结论） | 适配器 | **`llm-api-unavailable`** | 应为 `Err` → `RunFailed`（待其落地） |
| 合法空回复 / 纯思考（`finish_reason = stop`，无正文、无工具） | 循环 | **本需求** | `NoOutput` + 可见行 |

- **落地顺序依赖**：在 `llm-api-unavailable` 把「传输层空」转成 `Err` **之前**，该情形也会落到本需求的
  `NoOutput`（好于现状的 `Completed`，但**不等于**错误）。本设计**不吞掉**这个差异 —— 它是 `NoOutput`，
  不是 `Completed`；待上游落地后自动变为 `RunFailed`。
- **交叉引用落笔**（实现该批时应做）：在 `docs/arch/llm-api-unavailable/context.md` 顶部加一句指向本设计，
  声明「本设计收窄其场景 4」，两处不再各说各话。

### 量化验收对照（详见 `context.md`）

| 指标 | 基线 | 目标 | 测法 |
|---|---|---|---|
| **单条工具结果字节（max）** | **45,255** | `grep` ≤ **16 KiB**（≈4k token） | `grep` 单测 + 会话复算 |
| **当轮回喂 token** | ~48k est（37 条） | 同任务显著下降 | 会话复算对照 |
| 单条「偏大且无 notice」 | 2 | **0** | `grep` 单测（软阈值） |
| `context` 越界 | 无上限 | 钳到 5 + notice | 传 `context: 999` 断言 |
| 空 assistant 回合 | 2 | **0** | `MockModel` 喂 `content=[]` ⇒ 断 `NoOutput` + TUI System 行 |
| `-p/--json` 空响应 | exit 0、无输出 | **exit 1** + stderr 一行 | e2e |
| 全量用例 | `AGENTS.md` 记 975（疑过期） | **不降级** | `cargo test --workspace` |

## 候选方案对比（Step 3 / Step 4）

### 方案 A — 最小复杂度

- **B**：空分身复用 `RunFailed { error }`（不新增 `StopReason`）；**A**：软阈值 + `context` 钳制**只写在 `grep.rs` 内**。
- **代码量** ~15 行。**代价**：把「模型空回复」与「基础设施失败」混淆（退出码语义、无法用 `stop_reason` 表达）。

### 方案 B — 可扩展优先

- **A**：`ys-tool` 建可配置 `ResultBudget`；**B**：抽 `AssistantOutcome` + 新非终局 `AgentEvent`。
- **代码量** ~120 行。**代价**：本需求内无真实配置需求 ⇒ 过度设计；新增事件消费者面大。

### 方案 C — 性能 / 资源优先

- **A**：把「先渲染再截」改成带字节预算的**流式渲染**；**B**：`StopReason::NoOutput`，仅靠符号表达。
- **代码量** ~60 行。**代价**：45KB 本就是被闸门限住的小量，内存非瓶颈；**只给符号**信息量不足。

### 对比矩阵

| 维度 | A 最小 | B 可扩展 | C 性能 |
|---|---|---|---|
| 新增概念数 | 0 | 3 | 1 |
| 代码量估算 | ~15 行 | ~120 行 | ~60 行 |
| 当轮体积是否真下降 | 否（仅提示） | 否（仅提示） | 否（仅渲染层，输出量不变） |
| 空响应可否区分于基础设施失败 | 否 | 是 | 是 |
| `-p/--json` 退出码语义 | 改（且混淆） | 可定义 | 可定义 |
| 空响应可解释性（用户视角） | 高（Error 行） | 高 | 低（仅符号） |
| 可测性 | 高 | 高 | 高 |
| 演进成本 | 低 | 高 | 中 |
| 主要风险 | 语义混淆 | 过度设计 | 用户仍不解 |

> **v2 说明**：三个候选在 v1 都**没有真正降低当轮体积**（A/B 只提示、C 只优化本地渲染）。
> `review.md` 的 R1 指出这一共性缺口，故推荐方案在 v2 把「**真收窄**」（`grep` 专用预算）并入第一批 ——
> 这是候选之外、由分析补上的一步。

## 选择理由（Step 5）

**推荐 = 融合方案（v2）**：候选 A 的落点 + 候选 C 的诚实停止原因 + 候选 B 的最小复用点，
**并补上 review 指出的「真收窄」**（三个候选都缺）。

| 来源 | 采纳 | 否决理由 |
|---|---|---|
| A 最小 | 采纳「改动小、可独立测」的落点原则 | 否决「复用 `RunFailed`」—— 与基础设施失败混淆 |
| B 可扩展 | **仅**采纳「notice 拼接落 `ys-tool` 供 grep/read 复用」 | 否决 `ResultBudget` 配置层与 `AgentEvent` 新变体（无真实需求） |
| C 性能 | 采纳「`context` 先钳制」「`StopReason::NoOutput` 零额外分配检测」 | 否决「流式渲染」—— 非瓶颈（成本在回喂模型的 token） |
| **review R1（新增）** | **`grep` 专用结果预算 `GREP_MAX_RESULT_BYTES = 16 KiB`（真收窄）** | — |
| **review R2（新增）** | **`NoOutput ⇒ 退出码 1` + stderr 一行** | 保持 0 会「静默装作成功」 |
| — | TUI 在 `NoOutput` 时推**一行可见 System** | 只给符号信息量不足 |

**为什么这样是自洽的**：

- **B 用新变体而非 `RunFailed`**：`Truncated`（ADR-0020）已确立「给终止一个诚实的名字」，`NoOutput` 属同族。
- **A 既收窄又提示**：收窄治**当轮体积**（R1），提示治**下一轮行为**；两者互补，单用任一个都不够。
- **一次性模式退出码为 1**：`-p/--json` 是脚本路径，「无可用产出」不应报成功；与 `llm-api-unavailable`
  的「不可静默装作成功」一致（`Truncated` 仍为 0 —— 它有部分可用输出，区别是「有输出 / 无输出」）。

**代价（诚实表达）**：`grep` 专用预算会**丢部分有用结果**（故带收窄指引，让模型重查）；
新变体要同步 `CONTEXT.md` / `design.md` §5 与测试矩阵；阈值（16 KiB / 8 KiB / `MAX_CONTEXT=5`）需校准。

## 已知风险与待验证项

- [ ] **预算取值校准**：`GREP_MAX_RESULT_BYTES = 16 KiB` 是否过紧？用真实会话复算（见 `context.md` 复算脚本）。
- [ ] **软阈值噪声**：`SOFT_RESULT_BYTES = 8 KiB` 是否在正常结果上触发提示。
- [ ] **`NoOutput` 命名**：与 `docs/CONTEXT.md` 术语风格对齐（备选 `Empty` / `NoContent`）。
- [ ] **退出码 1 的口径**：需与 `llm-api-unavailable` 的「终止粒度」未决项一并确认（该文档「`-p/--json` 的错误措辞与退出码」一栏）。
- [ ] **与 `llm-api-unavailable` 交叉引用落笔**（见 §边界）。

## 修订记录

- **v2（2026-10-12）**：按 `review.md` R1/R2/R3 修复 —— A 主手段改为**真收窄**（`grep` 专用预算）；
  验收补**体积 / token** 指标；明确 `NoOutput` **退出码 1** 与**与 `llm-api-unavailable` 的边界**；
  空响应判定改 `trim()`；删去 v1 顶部的过程性注记。
- **v1（2026-10-12）**：初稿（候选 A/B/C + 对比矩阵 + 融合推荐）。

## 相关文档

- [`context.md`](./context.md) — 上下文与约束、量化验收、复算脚本
- [`adr-turn-output-and-feedback.md`](./adr-turn-output-and-feedback.md) — 决策记录
- [`review.md`](./review.md) — 质量分析（R1–R7）
- `docs/arch/tool-call-efficiency/context.md` — 工具输出「不放宽上限」约束的出处
- `docs/arch/llm-api-unavailable/context.md` — 传输层空响应 / 超时 / 场景 4（边界）
- `docs/arch/long-horizon-completion/design.md` — 「静默把空响应当 Completed = 静默丢工作」的同源账
- `docs/design-final/truncation-guard.md` — `StopReason` 加法式新增的先例
