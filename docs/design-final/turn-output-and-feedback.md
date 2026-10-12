# 单回合输出规模与可见性 — As-built

> 性质：**实际建成记录**。决策见 `docs/arch/turn-output-and-feedback/adr-turn-output-and-feedback.md`，
> 设计见 `docs/arch/turn-output-and-feedback/design.md`，审查见 `docs/reports/2026-10-12-turn-output-and-feedback-review.md`。
> 日期：2026-10-12 ｜ 基线 `43ad632c`。

## 一句话

把单回合的**工具输出规模**收进边界、让回合的**产出与可见性**可见：`grep` 单条结果不再无提示地膨胀到
45KB（预算收到 16 KiB + 收窄指引 + 软阈值提示）；模型返回**空 / 纯思考响应**时回合不再静默当
`Completed`，而是以 `StopReason::NoOutput` 收场 —— TUI 给一行可见说明，`-p`/`--json` 返回非零退出码。

## 落点

```text
crates/ys-core/src/stop.rs                 StopReason 新增 NoOutput（第五变体）
crates/ys-core/src/lib.rs                  变体集合 + NoOutput serde roundtrip（钉 "NoOutput"）
crates/ys-tool/src/truncate.rs             SOFT_RESULT_BYTES = 8 KiB（软阈值，< MAX_BYTES 不变式）
crates/ys-tool/src/notice.rs               size_notice(bytes, Option<matches>) 统一文案
adapters/tools-basic/src/grep.rs           GREP_MAX_RESULT_BYTES=16KiB 截断 + 收窄指引；MAX_CONTEXT=5 钳制；软阈值提示
adapters/tools-basic/src/read.rs           大结果软阈值提示（与「Showing lines」互斥）
crates/ys-loop/src/basic.rs                空分支：text.trim().is_empty() ⇒ NoOutput
crates/ys-tui-coding/src/draw.rs           stop_symbol(NoOutput) = "∅"
crates/ys-tui-coding/src/app.rs            RunFinished{NoOutput} ⇒ 一行 System 提示
apps/coding-agent/src/main.rs              single_turn_exit_code：NoOutput ⇒ 1 + stderr 一行
apps/coding-agent/src/prompt.rs            guideline「先窄后宽」（read/grep 在场时）
```

## 关键行为（实现期定死，勿凭直觉改）

**① `NoOutput` 是加法式第五变体。** `StopReason` 为 `#[non_exhaustive]`，下游 `draw.rs` 已有 `_ => "?"`
兜底；`lib.rs` 的 serde roundtrip 钉住 `"NoOutput"` 字面量（`--json` 的对外契约）。序列化改名会红。

**② 空响应判定用 `trim()`，且**不看** `finish_reason。** `text_content.trim().is_empty()` 才算空 ——
空白字符（含 `" "`）同样落 `NoOutput`。传输层空（`llm-api-unavailable` 管辖）在上游落地前也会显示为
`NoOutput`；待其接管后变为 `RunFailed`。这是**记录在案**的顺序依赖，不是缺陷。

**③ `grep` 专用预算低于通用闸门。** `head_lines(lines, MAX_LINES, GREP_MAX_RESULT_BYTES=16 KiB)`，
且**截断 notice 必须带收窄指引**（「Truncated N lines, showing first M. Narrow the search …」）——
否则「截断而无指针」正是要治的最坏形态。软阈值（8 KiB）**仅未硬截断时**触发，二者不重复。

**④ `context` 钳制是防更宽放大，不是主手段。** `MAX_CONTEXT = 5`；观察用例 `context=4` **不触发**它 ——
主手段是 ③ 的结果预算。

**⑤ 空产出三处可见，缺一不可。** 单靠 `Summary` 行的 `∅` 符号用户看不懂（实测会继续追问），故：
TUI 一行 `System` 提示 + `-p`/`--json` 的非零退出码 + stderr 一行。退出码由 `single_turn_exit_code`
决定（`-p` 与 `--json` 两处都经 `exit_with_code` 落地）。

**⑥ 「先窄后宽」是产品内置 guideline，随 `read`/`grep` 在场而出现。** 它进 `fixed_prompt_tokens`
（随 system prompt 缓存前缀），故措辞只加一句、不加一段。

## 与设计的偏差

1. **`size_notice` 签名由 `matches: usize` 改为 `matches: Option<usize>`** —— 设计对 `read`（无命中数）
   不适用现签名；`None` 走「read a narrower range with `offset` / `limit`」措辞。功能等价、更诚实。
   `design.md` 接口概要已同步。
2. **无其它偏差。** 阈值（16 KiB / 8 KiB / 5）、退出码（1）、命名（`NoOutput`）均按设计执行。

## 量化验收（实测）

| 指标 | 目标 | 结果 |
|---|---|---|
| 单条工具结果字节（max） | `grep` ≤ 16 KiB | ✅ 单测断言正文 ≤ `GREP_MAX_RESULT_BYTES` |
| 空 assistant 回合 | 0（改 `NoOutput`） | ✅ `""` / `"   "` 均 `NoOutput` |
| `-p` / `--json` 空响应 | 退出码 1 + stderr | ✅ 单测 `NoOutput ⇒ 1`；进程级 e2e 待凭证（见审查 P2） |
| 全量测试 | 不降级 | ✅ **1120 passed / 0 failed / 9 ignored** |

## 测试（新增，各自带变异点）

- `ys-core`：`test_stop_reason_no_output_serde_roundtrip`
- `ys-tool`：`soft_result_bytes_is_below_hard_cap`、`size_notice_reports_kib_and_narrowing`
- `tools-basic`：`test_grep_truncates_to_result_budget_with_narrowing_hint`、
  `test_grep_soft_size_notice_only_when_large_and_not_truncated`、`test_grep_context_is_capped_with_notice`、
  `test_read_large_range_gets_soft_size_notice`
- `ys-loop`：`empty_response_ends_with_no_output_not_completed`
- `ys-tui-coding`：`test_no_output_turn_pushes_visible_system_note`（+ 反向 `completed_turn_does_not_push_no_output_note`）
- `coding-agent`：`single_turn_exit_code_is_nonzero_on_budget_cancellation`（扩写）、
  `narrow_before_broad_guideline_appears_only_with_read_or_grep`

## 相关文档

- `docs/arch/turn-output-and-feedback/`（context / design v2 / adr / review）
- `docs/design-plans/completed/2026-10-12-turn-output-and-feedback.md`
- `docs/reports/2026-10-12-turn-output-and-feedback-{review,report}.md`
- `docs/design-final/truncation-guard.md` — `StopReason` 加法式新增的先例
