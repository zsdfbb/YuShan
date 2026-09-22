# 截断保护（片 1）设计计划

> 日期：2026-09-22
> 决策记录：ADR-0020（`finish_reason` 归一化、被截断工具批作废、新增终止原因 `Truncated`）
> as-built：[`../design-final/truncation-guard.md`](../design-final/truncation-guard.md)；报告：[`../reports/2026-09-22-truncation-guard-report.md`](../reports/2026-09-22-truncation-guard-report.md)
> 基线：`aeabbaf8`
> 性质：**事后补录**（实现与验收均已完成，本文为记录归档，非实现前计划）。补录日期：2026-09-22。
> 本切片走 sequential-workflow **quick** 方案、**跳过了写 design-plan / exec-plan 的阶段**，本文与配套 exec-plan 是事后补齐的记录。

## 1. 背景与动机

**问题**：provider 在输出预算耗尽时会掐断响应，并给出 `finish_reason: "length"`。适配器**确实**抓到了这个值
（`adapters/model-openai-compatible/src/lib.rs` 的 `StreamAccumulator.finish_reason`，原样写进 `ModelResponse`），
但 `ys-loop` **从头到尾没有读过这个字段一次**（全 crate grep 零命中）。于是循环把「被砍断」当成「答完了」，
产生三种后果：

1. **拿残缺参数执行工具** —— 工具块的产出条件只是「`name` 非空」，**不看参数是否完整**。一个写到一半的
   `write` 调用照样执行 ⇒ **覆盖用户的文件**，且工具返回 success、TUI 显示 ✓。
2. **参数烂到解析不出** ⇒ 适配器降级为 `Value::Null` ⇒ 工具收到 `null`，报谁也看不懂的错。
3. **连工具块都没产出** ⇒ 循环判「无 tool call」⇒ **`Completed` + ✓** —— 用户以为答完了。

**目标**：模型被截断时，绝不执行该批工具调用；把「被截断」这一事实一路带到用户与 `-p` / `--json` 消费方眼前。

**为什么值得做**：这不是「少一个特性」，而是**已有撒谎路径**。把 ✓ 打在一个被砍断的回答上，与压缩那里的
「静默失忆」是同一类的谎 —— 项目对此已有明确立场（ADR-0017 决策 7 的 B-3：**「那是静默失忆，不是降级」**）。
参照实现 Pi 的 `failToolCallsFromTruncatedMessage` 走的是同一条路：截断消息里的工具调用**全部失败化**。

**为什么归到「片 1」**：与它并列的另一条基础缺口是**模型调用重试**（429 / 5xx / 连接失败目前零重试）。
两条都属「不修就是 bug」，但**重试不在本片范围**（见 §5）。

## 2. 概念边界（必须先分清）

本片最关键的认知修正：**仓库里有两个都叫「stop reason」的东西，它们是不同的概念。**

| 概念 | 定义 | 归谁 | 本计划中的角色 |
|---|---|---|---|
| **`StopReason`**（`ys-core`） | **回合**是怎么结束的：`Completed` / `MaxRounds` / `Cancelled` / 新增 `Truncated` | `ys-core` | 回合的**终止原因** —— 对外契约 |
| **`FinishReason`**（`ys-model`） | **provider** 这一条响应是怎么收的尾：`Stop` / `ToolCalls` / `Length` / `ContentFilter` / `Other` | `ys-model`（词汇）+ 适配器（方言） | 循环判断「这条响应能不能用」的**输入** |
| **「被截断」** | `FinishReason::Length` | — | 本片要处理的信号；`is_truncated()` 是**唯一**判据 |

**旧字段名 `ModelResponse.stop_reason: Option<String>` 与 `ys_core::StopReason` 撞车，正是这个 bug 隐身的根因。**
读代码的人与写代码的人都以为那是同一个东西，于是没人去读 —— 「存在却从未被读过」不是巧合。
所以本片把两个概念在**类型上**分开（改名 `finish_reason` + 换成枚举），而不是靠读者自觉。

另有两个相邻概念要分清：

- **截断 ≠ 基础设施失败**：它是模型侧的**输出预算限制**，是**正常终止路径**，与 `Completed` 并列。
  故走 `RunResult`（`StopReason::Truncated`），**不走** `Err(LoopError)`。
- **归一化在适配器 ≠ 适配器决定语义**：适配器只负责**方言 → 词汇**的映射；哪些方言算截断，
  由 `ys-model` 的 `FinishReason` 定义。`ys-loop` 从此**不认识任何 provider 的字符串**。

## 3. 设计

### 3.1 形状：三条「零」

| 零 | 说明 |
|---|---|
| **零新工具** | 本片不碰工具面 —— `toolset.rs` 的 `EXPECTED_ORDER` / `vec!` / `assert_eq!(tools.len(), 9)` 一律不动；工具仍是 **9 个**。 |
| **零新 `Request` 变体** | 协议（`ys-protocol`）不动。新增的 `ResponseTruncated` 是 `AgentEvent` 的一个**非终局**臂，不是 `Request` 变体；`Request` 仍是 **7 个变体**。 |
| **零新依赖** | 无新 crate、无新 crate 依赖；`FinishReason` / `ResponseTruncated` 都是既有 crate 内的加法式类型。 |

### 3.2 数据流

```
【适配器 · 流式收敛点】
  SSE delta.finish_reason: Option<String>            ← provider 方言原值，保持是字符串
      → StreamAccumulator::finish()
      → normalize_finish_reason(raw) → FinishReason  ← 归一化的**唯一**发生地
      → ModelResponse { message, usage, finish_reason }

【ys-loop · Step 8b】（Step 7 追加 assistant 消息之后、Step 9 执行工具之前）
  if response.finish_reason.is_truncated() {
      emit ResponseTruncated { dropped_tool_calls: tool_calls.len() }   ← await；失败 → LoopError::Event

      if tool_calls.is_empty() {
          emit RunFinished { stop_reason: Truncated, usage, rounds }
          return Ok(RunResult { stop_reason: Truncated, .. })           ← 纯文本截断：诚实收场
      }

      for (id, name, _args) in &tool_calls {                            ← 整批作废：一个都不执行
          emit ToolCall   { .. }                                        ← 与 ToolResult 配对（TUI 渲染 ✗ 行）
          emit ToolResult { id, is_error: true, content: TRUNCATED_TOOL_NOTICE }
      }
      append(单条 Role::User 的工具结果消息)                              ← ADR-0001 配对完整性
      continue                                                          ← 回到轮首，模型自行改小步
  }

【ys-tui-coding】
  ResponseTruncated → 推一行 TranscriptLine::System 警告（**非终局**，不调 finish_turn）
  StopReason::Truncated → stop_symbol = "✂"
```

**两条路给消费方的东西不同**：`-p` / `--json` 从**终止原因**（`"Truncated"`，契约）区分「答完了」与「被砍了」；
TUI 从**警告行 + ✂**看见。这正是决策 4 否决「仍记 `Completed` + 只发警告」的理由。

### 3.3 接口概要

```rust
// crates/ys-core/src/stop.rs
#[non_exhaustive]
pub enum StopReason { Completed, MaxRounds, Cancelled, Truncated }   // 新增第四变体

// crates/ys-model/src/request.rs
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason { Stop, ToolCalls, Length, ContentFilter, Other(String) }
impl FinishReason {
    pub fn is_truncated(&self) -> bool { matches!(self, Self::Length) }   // 唯一的截断判据
}
impl Default for FinishReason { /* Other(String::new()) —— 缺字段 = 未识别，不得当成 Stop */ }

pub struct ModelResponse {
    pub message: Message,
    pub usage: ys_core::Usage,
    /// provider 的原始 finish_reason **归一化后**的形态（字段名与 StopReason 区分开是有意的）。
    #[serde(default)]
    pub finish_reason: FinishReason,       // 原 stop_reason: Option<String>
}

// crates/ys-model/src/mock.rs（测试替身）
pub fn push_truncated_tool_call(&self, name: impl Into<String>, arguments: serde_json::Value)
pub fn push_truncated_text(&self, text: impl Into<String>)

// crates/ys-event/src/event.rs
ResponseTruncated {
    /// 被作废、**未执行**的工具调用数（0 = 纯文本被截断）。
    dropped_tool_calls: usize,
}

// adapters/model-openai-compatible/src/lib.rs
fn normalize_finish_reason(raw: Option<String>) -> FinishReason   // 私有；流式收敛点唯一调用

// crates/ys-loop/src/basic.rs
const TRUNCATED_TOOL_NOTICE: &str = "tool call was NOT executed: the model response was truncated \
(output token limit reached). Re-issue this call with a smaller payload, or split it into several calls.";
```

`ResponseTruncated` 只带**平铺字段**（`usize`）—— `ys-event` 不能反向依赖别的 crate（同 ADR-0017 决策 6 的约束）。

### 3.4 决策 1 — `ModelResponse.stop_reason` → `finish_reason: FinishReason`

- **为什么改名**：旧名与 `ys_core::StopReason` 撞车，这正是它「存在却从未被读过」的直接原因。
- **为什么改类型**：把 provider 词汇（`"length"`）关回适配器，核心类型只认语义。
- **`Option` 消失**：「provider 没给」现在由 `Other(String::new())` 表达，不再是需要 unwrap 的状态。
- **缺省语义**：`Default = Other(String::new())` + `#[serde(default)]` —— **缺字段 = 未识别，不得当成 `Stop`**。
- **破坏面**：该字段是 `ys-model` 公开 API。仓库内 **10 个文件、22 处构造点**一并同步（含 `mock.rs` 两大测试替身
  与各夹具），**全在仓库内，无下游消费者**。

### 3.5 决策 2 — 归一化发生在适配器：方言映射表

```
"stop" | "end_turn"        → Stop
"tool_calls" | "tool_use"  → ToolCalls
"length" | "max_tokens"    → Length
"content_filter"           → ContentFilter
其余 / 缺失                 → Other(原值)
```

- 分工是「**vocabulary 归 `ys-model`、方言归适配器**」—— `ys-loop` 从此不必认识任何 provider 的词汇。
- 别名（`end_turn` / `tool_use` / `max_tokens`）是**零成本的保险**，让上面这条分工真的成立。
- `None` 与未知方言落到 `Other(_)`，**绝不猜成 `Stop`** —— 那会让下游把未知当正常收尾。
- 归一是**一处**（流式收敛点），不是散在各 provider 分支里。适配器内部 `StreamAccumulator.finish_reason`
  （SSE 抓到的原值）**保持是字符串**。

### 3.6 决策 3 — 被截断的那批工具调用一个都不执行

- **整批作废，不是「只丢坏的」**：`dropped_tool_calls = tool_calls.len()` —— 一个完整、一个被掐断，**两个都不执行**。
  理由：截断意味着「这一轮模型没说完」，**无法判断哪个调用是它想发的**。
- **全部合成 `is_error` 结果回喂模型**，话术（`TRUNCATED_TOOL_NOTICE`）点明「没有执行、别以为成功了」，
  **循环继续**（不终止回合）。
- **审批跳过** —— 不执行就不该弹审批：让用户为一次**不会发生**的执行按 y/n 是纯粹的骚扰。
- **配对完整性（ADR-0001）**：每个 `tool_use` 都必须拿到一条结果，作为**单条** `Role::User` 消息 append；
  否则下一轮请求会被真实 provider **直接拒收**。
- **被作废的批仍发 `ToolCall` + `ToolResult` 事件**（**有意**）：TUI 据此把每个调用渲染成 ✗ 行，用户能看见
  「哪几个没跑」。代价是事件层面「尝试过并失败」与「根本没执行」不可区分 —— 由 `ResponseTruncated` 警告行承担区分责任。
- **不发模型调用**：分支只多一次 `continue`。`app_loop.rs::every_request_variant_is_handled` 会走到这里，
  它安全**只因为**不触发 `complete()`（`MockModel` 空队列 `remove(0)` 会 panic）。
- `rounds` 在 `continue` 后自增、受 `max_rounds` 约束 ⇒ 反复截断以 `MaxRounds` 收场，**不会死循环**。

### 3.7 决策 4 — 纯文本被截断不伪装 `Completed`：新增 `StopReason::Truncated`

- 新增第四种终止原因，与「完成」**并列**，**不是错误**（见 §2 概念边界）。
- **替代方案被否决**：**「仍记 `Completed`、只发一个警告事件」**（ADR-0020 方案对比 C）—— 把 ✓ 打在一个被砍断的
  回答上，与压缩那里的「静默失忆」是同一类的谎；且 `-p` / `--json` 的消费者**无法从终止原因区分**「答完了」与
  「被砍了」—— 警告事件可被忽略，**终止原因是契约**。
- 另一被否决方案是 **B「只补读取，不改名」**：它保留了本片要修的**根因**（字段名撞车 ⇒ 从不被读），修一半没有意义。
- **分支位置是承重的**：Step 8b 必须在「Step 8 提取 `tool_calls` **之后**、`if tool_calls.is_empty()` 判定**之前**」。
  若挪到 `is_empty` 之后，纯文本截断会掉进旧的 `Completed` 分支 —— 即被否决的方案 C。
- `"Truncated"` 这个 **serde 字面量从此是对外契约**，不得再改名。

### 3.8 决策 5 — 非终局事件 `AgentEvent::ResponseTruncated { dropped_tool_calls }`

- 先例是 `ContextCompacted` / `ContextCompactionFailed`（同为加法式**非终局**事件）。
- 让截断**可见**。**终局事件仍只有** `RunFinished` / `RunFailed`（`main.rs` 的退出条件、`channel.rs` 的
  `is_terminal` 都硬编码这一条）⇒ `-p` / `--json` 的消费循环退出条件不变，**不得新增终局事件**。
- `dropped_tool_calls = 0` 即「纯文本被截断」，`> 0` 即「有一批工具被作废」。
- **事件顺序承重**：`ResponseTruncated` **先于**终局事件（由 N2 的顺序断言守护）。

### 3.9 决策 6 — 明确不做

| 项 | 为什么不做 |
|---|---|
| 模型调用重试 | 与本决策正交的另一件事，属**片 2**。429 / 5xx / 连接失败目前零重试，前置改造是 `ModelError::Provider(String)` 需要结构化可重试标志 |
| provider 截断却**不报** `finish_reason` | 例如流被 TCP 断掉 ⇒ 走 SSE error ⇒ `ModelError`。本片**显式记为已知限制**，属片 2 范畴 |
| 改写 `finish_reason` 的**产出**侧（`name` 非空即出块） | `name` 非空仍是产出条件 —— 截断批的作废发生在**循环侧**（决策 3），适配器职责不变 |

### 3.10 TUI 通路

- `crates/ys-tui-coding/src/app.rs` 的事件分发加一臂（`ContextCompacted` 的臂是模板）：推一条
  `TranscriptLine::System` 警告行、**不调 `finish_turn()`**（非终局）。
- `crates/ys-tui-coding/src/draw.rs` 的 `stop_symbol`：`Truncated => "✂"`，`_ => "?"` 兜底**保留**。

### 3.11 承重约束小结（实现期定死，勿凭直觉改）

| # | 约束 | 守护用例 |
|---|---|---|
| ① | 分支位置在 `tool_calls.is_empty()` **之前** | N2 |
| ② | 「整批作废」而非「只丢坏的」 | N4（用「恰好 2」钉住） |
| ③ | 分支内**不得**出现 `ctx.approval` | —（代码审查） |
| ④ | 每个 `tool_use` 都有结果，**单条** `Role::User` append | N4（ADR-0001） |
| ⑤ | `ResponseTruncated` 先于终局事件 | N2 顺序断言 |
| ⑥ | 截断分支不发模型调用 | `every_request_variant_is_handled` |

## 4. 与设计的偏差

本节记录**实现与初始设计之间**的 6 处偏差（as-built 口径，详见 `../design-final/truncation-guard.md`）。

| # | 设计写的 | 实际 | 原因 |
|---|---|---|---|
| 1 | 应有 design-plan / exec-plan | **无**（quick 方案跳过 Phase 2） | 本仓库首个无这两份存档的切片；记录由 ADR-0020 + 报告 + as-built 承担。**本文与配套 exec-plan 是事后补录** |
| 2 | N4 用 `MockModel` | 改用自建 `TwoTruncatedCallsModel` | `MockModel` 每次 `push` 是一个**独立响应**，表达不了「同一条响应里两个 tool call」；不用它，变异「只给第一个合成结果」会**杀不中** |
| 3 | N4 按设计即可 | 首版是**弱测试**，评审后重修 | 首版工具名未注册，删掉整个 guard 依然全绿（`calls == 0` 是空断言）。已改为真 spy + `ResponseTruncated { dropped_tool_calls: 2 }` 断言 |
| 4 | e2e 用「一个残缺的 `write`」 | 批里加了一个**参数完整的 `write` 当哨兵** | 适配器把非法 JSON 参数降级为 `Value::Null`，单个残缺调用只会报错、**不创建文件**，删掉 guard 也不会变红 ⇒ 抓不到回归 |
| 5 | 单元划分含 `crates/ys-loop` 与 `apps/` | 原计划**漏了** `crates/ys-runtime/tests/run_turn_boundary.rs` | 首轮文件盘点只覆盖前两者；U6 按 `cargo check` 的实际报错扩了范围（2 处改名） |
| 6 | `cargo test -p model-openai-compatible` | 真实包名 **`ys-model-openai-compat`** | 目录名 ≠ 包名；由 U4 纠正 |

## 5. 非目标

- ❌ **不做模型调用重试**（429 / 5xx / 连接失败）—— 与本次改动正交，属**片 2**。
- ❌ **不覆盖「provider 截断却不报 `finish_reason`」** —— 如流被 TCP 断掉（现在走 SSE error ⇒ `ModelError`），
  ADR-0020 明确未覆盖，属片 2。
- ❌ **不修**适配器的 `eprintln!` —— `ToolCallAccumulator::into_content_block` 在参数非法时打 warning，
  违反「库内部不 print / 输出三分」约定；与本片**相邻但独立**，记入 as-built 遗留节。
- ❌ **不改写** `finish_reason` 的**产出**侧（`name` 非空即出块仍成立，见 §3.9）。
- ❌ **不新增终局事件** —— 终局仍只有 `RunFinished` / `RunFailed`。
- ❌ **不加新工具、不改 `Request`** —— 工具 9 个、`Request` 7 个变体原样不动（§3.1）。
- ❌ **不给 `MockModel` 补「一次响应多个 tool call」的 pusher** —— 本片自建 model 绕开；
  若后续还有多工具测试，值得补（记入 as-built 遗留）。

## 6. 测试

### 6.1 单元（模块内 `#[cfg(test)]`，共 19 条新增）

- **`ys-core`**：`StopReason::Truncated` 的 serde roundtrip（**钉住 `"Truncated"` 字面量**）。
- **`ys-model`**：`FinishReason` 的 `Default` / `is_truncated()`；两个 pusher 产出 `Length`；
  `finish_reason` 字段名 roundtrip。
- **`ys-event`**：`ResponseTruncated` 的 serde roundtrip（**钉住 `dropped_tool_calls`**）+ 非终局解构。
- **`ys-loop`（4 条，本片核心资产）**：
  - `truncated_tool_calls_are_not_executed`（**N1**，真 spy：断言计数 0、结果为 `is_error` 且含截断话术、
    发了 `ResponseTruncated`、模型被调 **2** 次）
  - `truncated_text_ends_turn_as_truncated`（**N2**，含事件顺序断言）
  - `non_truncated_tool_calls_still_execute`（**N3**，回归护栏：计数 1、`Completed`）
  - `truncated_tool_calls_satisfy_pairing_invariant`（**N4**，配对完整性 + 整批作废）
- **`ys-model-openai-compat`**：方言映射表（含别名、未知、缺失）+ `is_truncated` 判据 + 合成 SSE 端到端。
- **`ys-tui-coding`**：`stop_symbol(Truncated) == "✂"` 且兜底 `_ => "?"` 仍在；
  `ResponseTruncated` 推 `System` 行且**不**调 `finish_turn`。

### 6.2 集成（`apps/coding-agent/tests/`）

- **端到端（1 条，**N11**，经用户确认升级为「必测」）**：
  `truncation_guard.rs::truncated_tool_batch_is_never_executed` —— 仿 `stream_usage_fallback.rs` 的回环
  HTTP server，吐 `finish_reason:"length"` 的 SSE（带一个**参数完整**的 `write` + 一个被掐断的 `write`）；
  断言**预置文件未被覆盖**、残缺文件未创建、`ResponseTruncated{dropped_tool_calls:2}`、
  终局仍是 `RunFinished` 且进程 exit 0、回喂结果全 `is_error` 且含 `"truncated"`、
  `request_count == 2`（证明循环真的继续到第二轮、**消费循环不死锁**）。

### 6.3 变异测试清单（本项目约定：关键逻辑必须注入 bug 验证测试能抓到）

共 **8 条**注入，全部独立复现过、且**均已还原**（`basic.rs` sha256 前后一致）。

| # | 注入 | 应该变红 | 实测 |
|---|---|---|---|
| 1 | `is_truncated()` guard → `if false` | N1 / N2 / **N4** | ✅ 三条全红 |
| 2 | 文本分支 `Truncated` → `Completed` | N2 | ✅ |
| 3 | guard 恒真 | N3 + 12 条既有用例 | ✅ |
| 4 | `for` 里首个结果后 `break` | N4 | ✅ |
| 5 | 删掉 `continue;` | N1（spy 被真执行） | ✅ |
| 6 | `ResponseTruncated` 的 emit 挪到终局之后 | N2 顺序断言 | ✅ |
| 7 | e2e：删掉 guard | `truncation_guard`（文件被创建） | ✅ |
| 8 | 适配器 `None` → `Stop` / `"length"` → `Stop` | 映射表 + 端到端 | ✅ |

**变异 1 与 4 是本清单的价值所在**：它们正是 review 抓出的 **P1**（N4 对特性零判别力）暴露与修复的抓手。
修复后变异 1 能让 N4 变红（`calls_a: left 1 / right 0`）。

## 7. 风险

- **R1（必须先跑）**：`every_request_variant_is_handled`（`apps/coding-agent/src/app_loop.rs`）仍在
  Step 6 之前早退、不被本分支影响 —— 本片**先跑这一条**，实测通过。
- **R2（对外契约，不可再改）**：`ResponseTruncated` 与 `StopReason::Truncated` 的**变体名字符串**（serde）
  从此是对外契约；`dropped_tool_calls` 字段名同。
- **R3（构造性上限，非实现缺陷）**：`FinishReason::Other` 的**原值是未知方言的兜底**，语义上不保证
  `is_truncated()` 判断正确 —— 一个把截断报成陌生字符串的 provider 仍会漏网。
  **`Length` 的覆盖面只能等于已知方言表。**
- **R4（未验证，需凭证）**：真实 provider 返回 `finish_reason: "length"` 时的端到端行为
  （`MANUAL_ACK_REQUIRED`）—— 调小 `max_tokens` 让它重写一个大文件，`git diff` 确认目标文件**没被改坏**。未做。
- **R5（仓库既有，非本片引入）**：`cargo fmt --all -- --check` 在 `apps/coding-agent/src/*.rs` 报差异 ——
  本机 rustfmt 与该仓库 CJK 文本的既有分歧。**后续 agent 注意：不要跑裸 `cargo fmt`**，它会重排一批无关文件污染 diff。

## 8. 交付判据

1. `cargo test --workspace` 全绿（基线 813 → **832 passed / 0 failed / 7 ignored**，即 **+19**）；
2. `cargo clippy --all-targets` **零 warning**；
3. `cargo check --workspace --all-targets` 通过；
4. 代码改动面 **19 改 + 2 新**，**无计划外改动**（`git diff <基线>` 逐文件核对）；
5. `cargo test -p ys-coding-agent every_request_variant_is_handled` 通过（R1）；
6. `cargo test -p ys-coding-agent --test truncation_guard` 通过（端到端，N11）；
7. 变异清单 8 条**全部被对应用例抓到**并已还原（§6.3）。
