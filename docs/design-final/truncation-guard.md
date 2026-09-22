# 截断保护（片 1）— As-built

> 性质：**实际建成记录**。决策见 `docs/adr/0020-truncation-guard.md`，报告见 `docs/reports/2026-09-22-truncation-guard-report.md`。
> 日期：2026-09-22 ｜ 基线 `aeabbaf8`。
> ⚠️ 本切片走 sequential-workflow **quick** 方案，**无 design-plan / exec-plan**。

## 一句话

模型被输出预算截断（`finish_reason = length`）时，那一批工具调用**一个都不执行**、全部合成错误结果回喂；
纯文本被截断时回合以新终止原因 `StopReason::Truncated` **诚实收场** —— 从「把被砍断当答完了」变成
「截断是一等公民事实」。原先那条路径会让一个写到一半的 `write` 覆盖用户的文件，并在 TUI 上显示 ✓。

## 落点

```
crates/ys-core/src/stop.rs                     StopReason 新增第四变体 Truncated
crates/ys-core/src/lib.rs                      serde roundtrip 钉住 "Truncated" 字面量
crates/ys-model/src/request.rs                 FinishReason 枚举；ModelResponse 字段改名并改类型
crates/ys-model/src/mock.rs                    push_truncated_tool_call / push_truncated_text
crates/ys-model/src/lib.rs                     FinishReason 与 pusher 的测试
crates/ys-event/src/event.rs                   非终局事件 ResponseTruncated
crates/ys-event/src/lib.rs                     roundtrip + 非终局解构测试
adapters/model-openai-compatible/src/lib.rs    normalize_finish_reason（方言 → FinishReason）
crates/ys-loop/src/basic.rs                    ★ TRUNCATED_TOOL_NOTICE + Step 8b 截断保护分支
crates/ys-tui-coding/src/draw.rs               stop_symbol 新增 ✂
crates/ys-tui-coding/src/app.rs                ResponseTruncated → 一行 System 警告
apps/coding-agent/src/app_loop.rs              改名收尾
apps/coding-agent/tests/integration.rs         改名收尾
apps/coding-agent/tests/truncation_guard.rs    ★ 端到端：文件不得被截断的调用覆盖
crates/ys-runtime/tests/run_turn_boundary.rs   改名收尾
docs/CONTEXT.md                                终止原因定义加「被截断」
docs/design.md                                 §5 失败与终止语义 + §12 测试矩阵
docs/adr/0020-truncation-guard.md              决策记录
```

## 关键行为（实现期定死，勿凭直觉改）

**① 分支位置是承重的。** Step 8b 必须在「Step 8 提取 `tool_calls` **之后**、`if tool_calls.is_empty()`
判定**之前**」。若挪到 `is_empty` 之后，纯文本截断会掉进旧的 `Completed` 分支 —— 那正是被否决的方案
（ADR-0020 决策 4）。N2 用例守着这一点。

**② 是「整批作废」，不是「只丢坏的」。** `dropped_tool_calls = tool_calls.len()` —— 一个完整、
一个被掐断，两个都不执行。理由：截断意味着「这一轮模型没说完」，无法判断哪个调用是它想发的。
N4 用「恰好 2」钉住这条。

**③ 审批必须跳过。** 分支内**不得**出现 `ctx.approval` —— 不执行就不该弹审批，否则用户被问了一堆白问的。

**④ 配对完整性（ADR-0001）。** 每个 `tool_use` 都要拿到一条 `ToolResult`，作为**单条** `Role::User`
消息 append。否则下一轮请求会被真实 provider 直接拒收。

**⑤ 事件顺序：`ResponseTruncated` 先于终局事件。** 顺序被 N2 的断言守护（曾无守护，review 的 P2-2 补上）。

**⑥ 被作废的批**仍会发 `ToolCall` + `ToolResult` 事件**（有意的）**：TUI 据此把每个调用渲染成 ✗ 行，
用户能看见「哪几个没跑」。代价是事件层面「尝试过并失败」与「根本没执行」不可区分 —— 由前面那行
`ResponseTruncated` 警告承担区分责任。评审曾提为 P2，**判定为有意**，非缺陷。

**⑦ 归一化在适配器，词汇在 `ys-model`。** `ys-loop` 永远不认识任何 provider 的字符串。
`None` 与未知方言落到 `Other(_)`，**绝不猜成 `Stop`**（那会让下游把未知当正常收尾）。
适配器内部的 `StreamAccumulator.finish_reason: Option<String>`（SSE 抓到的原值）**保持是字符串**。

**⑧ 字段名 `finish_reason` 与 `StopReason` 刻意区分。** `ModelResponse.stop_reason` 旧名与
`ys_core::StopReason` 撞车 —— 这正是它此前被所有人忽略、从来没被读过的原因。改名是修复的一部分，**不得**改回。

**⑨ 截断分支不发模型调用。** `apps/coding-agent/src/app_loop.rs` 的 `every_request_variant_is_handled`
依赖「走到 `run_turn` 时不消费 `MockModel` 的空队列」（空队列 `remove(0)` 会 panic）。分支只 `continue`。

## 与设计的偏差

1. **无 design-plan / exec-plan** —— quick 方案的既定取舍（Phase 1 预告、用户确认）。这是本仓库
   首个没有这两份存档的切片；设计记录由 ADR-0020 + 报告 + 本文承担。
2. **N4 未用 `MockModel`，改用自建的 `TwoTruncatedCallsModel`** —— `MockModel` 每次 `push` 是一个
   **独立响应**，表达不了「同一条响应里两个 tool call」。不用它，N4 无法断言「同一条 `Role::User`
   消息里有 2 条结果」，更关键的是变异「只给第一个合成结果」会**杀不中**。
3. **N4 第一版是弱测试，评审后重修** —— 首版工具名未注册，删掉整个 guard 依然全绿（`calls == 0` 是空断言）。
   已改为真 spy + 事件断言。**读本用例时注意它守着两件事**：配对完整性 *与* 截断保护本身。
4. **端到端测试需要一个「参数完整的 write 当哨兵」** —— 适配器把非法 JSON 参数降级为 `Value::Null`，
   所以「整批只有一个残缺调用」时**删掉 guard 也不会创建文件**（`write` 只会报 `Invalid input`），
   抓不到回归。故 e2e 的批里放一个完整 `write` + 一个被掐断的 `write`，这才让变异变红，
   同时钉住「整批作废」语义。
5. **单元划分漏了 `crates/ys-runtime/tests/run_turn_boundary.rs`** —— 首轮文件盘点只覆盖了
   `crates/ys-loop` 与 `apps/`；该文件的 2 处改名由 U6 按 `cargo check` 的实际报错补上。
6. **适配器包名是 `ys-model-openai-compat`**，不是目录名 `model-openai-compatible` —— `cargo -p` 要用前者。

## 已知遗留 / 后续

- **[未做] 模型调用重试（片 2）** —— 429 / 5xx / 连接失败目前**零重试**，一撞限流整个回合 `RunFailed`，
  前面推进的上下文全白费。前置改造：`ModelError::Provider(String)` 需要结构化可重试标志
  （现在 HTTP 状态码只活在字符串里，判定只能靠 grep）。
- **[未做] provider 截断却不报 `finish_reason`** —— 如流被 TCP 断掉（现在走 SSE error ⇒ `ModelError`）。
  ADR-0020 明确未覆盖，属片 2。
- **[未做] 适配器的 `eprintln!`** —— `ToolCallAccumulator::into_content_block` 在参数非法时打 warning，
  违反「库内部不 print / 输出三分」约定。与本片相邻但独立。
- **[未做] `MockModel` 不支持「一次响应多个 tool call」** —— 本片因此自建了 model。
- **[未验证] MANUAL_ACK_REQUIRED** —— 「真实 provider 确实会报 `length`」需要凭证，未做：
  调小 `max_tokens` 让它重写一个大文件，`git diff` 确认目标文件**没被改坏**。
- **[仓库既有，非本片引入] `cargo fmt --all -- --check` 在 `apps/coding-agent/src/*.rs` 报差异** ——
  本机 rustfmt 与该仓库 CJK 文本的既有分歧，与本次改动无关。**给后续 agent 的警告：不要跑裸
  `cargo fmt`**，它会重排一批无关文件污染 diff。

## 测试资产

**单元（19 条新增）**
- `ys-core`：`StopReason::Truncated` 的 serde roundtrip（钉住 `"Truncated"` 字面量）
- `ys-model`：`FinishReason` 的 `Default` / `is_truncated()`；两个 pusher 产出 `Length`；`finish_reason` 字段名 roundtrip
- `ys-event`：`ResponseTruncated` 的 serde roundtrip（钉住 `dropped_tool_calls`）+ 非终局解构
- `ys-loop`（4 条，**本片的核心资产**）：
  - `truncated_tool_calls_are_not_executed`（N1，真 spy）
  - `truncated_text_ends_turn_as_truncated`（N2，含事件顺序断言）
  - `non_truncated_tool_calls_still_execute`（N3，回归护栏）
  - `truncated_tool_calls_satisfy_pairing_invariant`（N4，配对完整性 + 整批作废）
- `ys-model-openai-compat`：方言映射表 + `is_truncated` 判据 + 合成 SSE 端到端
- `ys-tui-coding`：`✂` 符号表 + `ResponseTruncated` 推行且**不**调 `finish_turn`

**端到端（1 条）**
- `apps/coding-agent/tests/truncation_guard.rs::truncated_tool_batch_is_never_executed`
  —— 回环 SSE 吐 `finish_reason:"length"` + 一个完整 `write`/一个残缺 `write`；
  断言**预置文件未被覆盖**、残缺文件未创建、`ResponseTruncated{dropped_tool_calls:2}`、
  终局仍是 `RunFinished` 且进程 exit 0、回喂结果全 `is_error` 且含 `"truncated"`、
  `request_count == 2`（证明循环真的继续到第二轮）。

**变异证据**：8 条注入全部被对应用例抓到并已还原（详见报告）。
