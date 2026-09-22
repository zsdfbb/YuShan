# 0020 — 截断保护：finish_reason 归一化、被截断工具批作废、新增终止原因 Truncated

给「provider 输出预算耗尽」这条路径补上保护 —— ① `ModelResponse.stop_reason: Option<String>` **改名并改类型**为 `finish_reason: FinishReason`（新枚举，**归一化在适配器**完成）；② 被截断的那批工具调用**一个都不执行**，全部合成 `is_error` 结果回喂模型（对齐 Pi 的 `failToolCallsFromTruncatedMessage`），且**跳过审批**；③ 纯文本被截断时**不伪装 `Completed`** —— 新增第四种终止原因 `StopReason::Truncated`；④ 新增**非终局**事件 `AgentEvent::ResponseTruncated { dropped_tool_calls }`，让截断在 transcript 里可见。**不做**模型调用重试，也不覆盖「provider 截断却不报 `finish_reason`」。

## Status

accepted（2026-09-22）

## 背景

- **适配器早就抓到了截断，只是没人读它。** `finish_reason` 在流式累积器里被完整接住（`adapters/model-openai-compatible/src/lib.rs:253-255`），并原样写进响应（同文件 `:291`）；但 `ys-loop` 从头到尾**没有读过这个字段一次**。
- **于是「被砍断」被当成「答完了」，产生三种后果**：
  1. **半截工具被真的执行**：产出工具块的条件只是「`name` 非空」（`adapters/model-openai-compatible/src/lib.rs:277-280`），**不看参数是否完整** ⇒ 一个写到一半的 `write` 调用照样执行，**覆盖用户文件**、返回 success、TUI 打 ✓。
  2. **参数烂到解析不出** ⇒ 适配器降级为 `Value::Null`（同文件 `:153-168`）⇒ 工具收到 `null` 参数。
  3. **连工具块都没产出** ⇒ 循环判「无 tool call」⇒ `Completed` + ✓，把被砍断的回答当成完整回答。
- **旧字段名是它被忽略的原因，不是巧合**：`stop_reason: Option<String>`（`crates/ys-model/src/request.rs`）与 `ys_core::StopReason`（`crates/ys-core/src/stop.rs`）**同名不同物** —— 读代码的人与写代码的人都以为那是同一个东西，于是没人去读；同时它把 OpenAI 的词汇（`"length"`）泄进了 `ys-model` 的类型。
- **终止路径缺一档**：`StopReason` 只有 `Completed` / `MaxRounds` / `Cancelled`，**「被截断」无处可去**，只能落进 `Completed`。
- **配对完整性是硬约束**：ADR-0001 要求「当轮助手消息中的每个 `tool_use` 都必须获得结果」，否则续跑会话会被真实 provider 直接拒收。故被作废的工具批**不能简单丢掉**，必须合成结果。
- **参照实现**：Pi 的 `failToolCallsFromTruncatedMessage` —— 截断消息里的工具调用**全部失败化**，一个都不执行。
- **项目一贯立场**：压缩那里的原话是「那是**静默失忆**，不是降级」（ADR-0017 决策 7 的 B-3）。把 ✓ 打在一个被砍断的回答上正是同类的谎。

## 决策

**1. `ModelResponse.stop_reason: Option<String>` → `finish_reason: FinishReason`**

```rust
#[non_exhaustive]
pub enum FinishReason { Stop, ToolCalls, Length, ContentFilter, Other(String) }
```

- **为什么改名**：旧名与 `ys_core::StopReason` 撞车，这正是它「存在却从未被读过」的直接原因。两个概念 —— provider 怎么收的尾 / 回合怎么结束的 —— 必须在**类型上**分开，而不是靠读者自觉。
- **为什么改类型**：把 provider 词汇（`"length"`）关回适配器，核心类型只认语义。
- **`Option` 消失**：「provider 没给」现在由 `Other(String::new())` 表达，不再是「没有值」这个需要被 unwrap 的状态。
- **缺省语义**：`Default = Other(String::new())`，配 `#[serde(default)]` —— **缺字段 = 未识别，不得当成 `Stop`**（缺省不是「正常收尾」的证据）。`is_truncated()` 是唯一的截断判据。
- 该字段是 `ys-model` 的**公开 API**，此为破坏性变更（见 Consequences 行为变更 ①）。

**2. 归一化发生在适配器（`StreamAccumulator::finish()`）**

```
"stop" | "end_turn"        → Stop
"tool_calls" | "tool_use"  → ToolCalls
"length" | "max_tokens"    → Length
"content_filter"           → ContentFilter
其余 / 缺失                 → Other(原值)
```

- 分工是「**vocabulary 归 `ys-model`、方言归适配器**」—— `ys-loop` 从此不必认识任何 provider 的词汇。
- 归一是**一处**（流式收敛点），不是散在各 provider 分支里。

**3. 被截断的那批工具调用一个都不执行**

- **全部合成 `is_error` 结果回喂模型**，话术点明「没有执行、别以为成功了」，**循环继续**（不终止回合）。
- **审批跳过** —— 不执行就不该弹审批：让用户为一次**不会发生**的执行按 y/n 是纯粹的骚扰。
- **这是 ADR-0001 配对完整性的要求**：每个 `tool_use` 都必须拿到一条结果，否则下一轮请求会被真实 provider 直接拒收。
- **对齐 Pi 的 `failToolCallsFromTruncatedMessage`。**

**4. 纯文本被截断时不伪装 `Completed`：新增 `StopReason::Truncated`**

- 新增第四种终止原因。**截断是正常终止路径，与「完成」并列，不是错误** —— 它是模型侧的**输出预算限制**，不是基础设施失败，故仍走 `RunResult`（而不是 `Err(LoopError)`）。
- **替代方案「仍记 `Completed`、只发一个警告事件」被否决**：把 ✓ 打在一个被砍断的回答上，与压缩那里的「静默失忆」是同一类的谎。（见方案对比 C）

**5. 新增非终局事件 `AgentEvent::ResponseTruncated { dropped_tool_calls: usize }`**

- 让截断**可见**。终局事件**仍只有** `RunFinished` / `RunFailed`；本事件非终局 ⇒ `-p` / `--json` 的消费循环退出条件不变。
- 只带平铺字段（`ys-event` 不能反向依赖别的 crate，同 ADR-0017 决策 6 的约束）。
- `dropped_tool_calls = 0` 即「纯文本被截断」，`> 0` 即「有一批工具被作废」。

**6. 明确不做**

| 项 | 为什么不做 |
|---|---|
| provider 截断却**不报** `finish_reason` | 例如流被 TCP 断掉 ⇒ 走 SSE error ⇒ `ModelError`。属「模型调用重试」那一片的范畴，本 ADR 不覆盖，**显式记为已知限制** |
| 模型调用重试本身 | 与本决策正交的另一件事 |
| 改写 `finish_reason` 的**产出**侧（`name` 非空即出块） | `name` 非空仍是产出条件 —— 截断批的作废发生在**循环侧**（决策 3），适配器职责不变 |

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 改名 + 读它（采纳）** | `finish_reason: FinishReason`；适配器归一化；截断批作废；新增 `Truncated` 终止原因 + 非终局事件 | 类型上把两个「stop reason」分开；方言被关在适配器；半截工具结构上不可能再跑；配对完整性保住 | `ys-model` 公开 API 破坏性变更；`StopReason` 加一个变体 |
| **B 只补读取，不改名** | 保留 `stop_reason: Option<String>`，在 `ys-loop` 里 `match` 字符串 | 改动最小；无 API 破坏 | **方言泄进核心循环**；更重要的是「改名要解决的根因」（撞名 ⇒ 没人读）原样保留 —— 下一个读代码的人仍会以为它是 `StopReason` |
| **C 仍记 `Completed` + 警告事件** | 不改 `StopReason`，截断时只发一条警告 | 零 API 变更 | 把 ✓ 打在一个被砍断的回答上；`-p` / `--json` 的消费者**无法从终止原因区分**「答完了」与「被砍了」—— 警告事件可被忽略，终止原因是契约 |

**采纳 = A**。理由：B 的问题**正是**本 ADR 要修的根因（字段名撞车导致它从不被读），修一半没有意义；C 与项目对「静默失忆」的既有判断（ADR-0017）直接冲突。

## Consequences

- **正面**：三种「把被砍断当成答完了」的后果全部堵死 —— 半截工具不再执行、null 参数不再来自截断、被砍断的纯文本不再打 ✓；截断从「隐形」变成有终止原因 + 有事件的**一等结局**；`ys-loop` 从此不认识任何 provider 词汇；ADR-0001 的配对完整性在截断路径上同样成立。
- **负面**：`ys-model` 公开 API 破坏性变更（改名 + 改类型）；`ys-core`、`ys-model`、`ys-loop`、适配器、`ys-event`、app 六处各有一处改动；用户会看到新增的 `Truncated` 终止原因与截断提示行（此前是静默的 ✓）。
- **行为变更（须写进 PR 描述）**：① **`ModelResponse` 字段改名 + 改类型**（`stop_reason: Option<String>` → `finish_reason: FinishReason`）—— `ys-model` 公开 API 的破坏性变更，仓库内 **10 个文件、22 处构造点**一并同步（含测试替身与夹具：`ys-model/src/mock.rs` 与 `ys-model/src/lib.rs`、`ys-loop` 的 `lib.rs` / `summarize.rs` / `compact.rs` / `basic.rs`、`adapters/model-openai-compatible/src/lib.rs`、`apps/coding-agent` 的 `app_loop.rs` 与 `tests/integration.rs`、`ys-runtime/tests/run_turn_boundary.rs`）；② `StopReason` **新增第四种值** `Truncated` —— `-p` / `--json` 契约的**加法式变更**，但 `"Truncated"` 这个字符串**从此是对外契约**，不得再改名；③ **截断时工具不再执行** —— **这是有意的行为变更**，此前半截工具会真的跑（可能覆盖用户文件）并返回 success、TUI 显示 ✓。
- **遗留风险**：provider 截断却不报 `finish_reason` 的情形未覆盖（走 `ModelError`，见决策 6）；`FinishReason::Other` 的**原值是未知方言的兜底**，语义上不保证 `is_truncated()` 判断正确 —— 一个把截断报成陌生字符串的 provider 仍会漏网。**这是构造性的上限，不是实现缺陷**：`Length` 的覆盖面只能等于已知方言表。
- **未验证**：真实 provider 返回 `finish_reason: "length"` 时的端到端行为（需真实截断，靠 `MockModel::push_truncated_*` 在测试层覆盖；真实验证需 `e2e_*` 那条 `#[ignore]` 路径）。

## 关联

- [ADR-0001](./0001-tool-failure-dual-channel.md) — **配对完整性**的出处：每个 `tool_use` 都必须获得一条结果，故被作废的工具批须合成 `is_error` 而非丢弃
- [ADR-0017](./0017-compaction-hardening.md) — 「静默失忆不是降级」的立场出处（本 ADR 决策 4 沿用同一判据）；非终局事件只带平铺字段的约束同其决策 6
- `docs/design-final/truncation-guard.md` — 本决策的 as-built（**实现后回写**）
- `docs/CONTEXT.md` — 「终止原因（StopReason）」条已扩入「被截断」
- `docs/design.md` §5 失败与终止语义、§12 测试矩阵
