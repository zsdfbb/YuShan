# turn-output-and-feedback — 实现审查报告（Phase 3）

> 日期：2026-10-12 ｜ 基线 `43ad632c0ffef366102358db1f15d2b64901d5ed`
> 设计：[`docs/arch/turn-output-and-feedback/design.md`]（v2）｜ 计划：`docs/design-plans/2026-10-12-turn-output-and-feedback.md`
>
> ⚠️ **审查者说明**：本轮 subagent 通道不可用（多次投递均未送达任务正文），按用户授权由**主 agent 自审**。
> **自审弱于独立审查** —— 已用「字节级 `git diff` 基线 + 测试合同逐条核对 + 每项写清变异点」补偿，
> 但仍不排除盲区。这一局限应被知悉。

## 审查范围

- 基线 → 工作区（`git diff 43ad632c -- <files>`）
- 单元：U1 `ys-core` / U2 `ys-tool` / U3 `tools-basic` / U4 `ys-loop` / U5 `ys-tui-coding` / U6 `coding-agent`

## 逐单元结论

| 单元 | 任务 | 档 | 结论 | 证据 |
|---|---|---|---|---|
| U1 | T1, T2 | deep | ✅ 通过 | `cargo test -p ys-core` → **7 passed**；`git diff` 仅 `stop.rs`(+5) / `lib.rs`(+13) |
| U2 | T3, T4 | shallow | ✅ 通过 | `cargo test -p ys-tool` → **43 passed**；新增 `SOFT_RESULT_BYTES` / `size_notice` |
| U3 | T5, T6 | deep | ✅ 通过 | `cargo test -p ys-tools-basic` → **79 passed**（含 N1/N2/N3/N4 新用例） |
| U4 | T7, T8 | deep | ✅ 通过 | `cargo test -p ys-loop` → **67 passed**（含 N6 新用例） |
| U5 | T9, T10 | shallow | ✅ 通过 | `cargo test -p ys-tui-coding` → **279 passed**（含 N7 + 反向用例） |
| U6 | T11, T12 | deep | ✅ 通过 | `cargo test -p ys-coding-agent` → lib **291 passed**（含 N5/N8） |

## 测试合同逐条核对

| # | 条款 | 命令 | 结果 |
|---|---|---|---|
| N1 | grep 单条 ≤ 16 KiB + 截断带收窄指引 | `cargo test -p ys-tools-basic`（`test_grep_truncates_to_result_budget_with_narrowing_hint`） | ✅ 正文 ≤ `GREP_MAX_RESULT_BYTES`、notice 含 `Narrow the search` |
| N2 | grep ≥ 软阈值且未截断 ⇒ notice | 同上（`..._soft_size_notice_only_when_large_and_not_truncated`） | ✅ 大带提示、小不带 |
| N3 | `context` 钳到 5 + notice | 同上（`test_grep_context_is_capped_with_notice`） | ✅ `context capped to 5`、后置 context 行 ≤ 5 |
| N4 | read 大结果软阈值提示 | 同上（`test_read_large_range_gets_soft_size_notice`） | ✅ 有 `result is large`、无 `Showing lines` |
| N5 | prompt「先窄后宽」guideline | `cargo test -p ys-coding-agent`（`narrow_before_broad_guideline_appears_only_with_read_or_grep`） | ✅ 在场出现 / 仅 bash 不出现 |
| N6 | 空 / 空白 ⇒ `NoOutput` | `cargo test -p ys-loop`（`empty_response_ends_with_no_output_not_completed`）+ `-p ys-core`（roundtrip） | ✅ `""` 与 `"   "` 均 `NoOutput`，终局事件同值 |
| N7 | TUI 对 `NoOutput` 推可见行 | `cargo test -p ys-tui-coding`（`test_no_output_turn_pushes_visible_system_note` + 反向） | ✅ 有 System 行、`Completed` 不带 |
| N8 | `-p/--json` 空响应 ⇒ 退出码 1 | `cargo test -p ys-coding-agent`（`single_turn_exit_code_is_nonzero_on_budget_cancellation` 扩写） | ✅ `NoOutput ⇒ 1`；调用点 `single_turn_exit_code` → `exit_with_code`（`main.rs:1823,1863`） |
| N9 | 无回归 | `cargo test --workspace` | ✅ **1120 passed / 0 failed / 9 ignored** |

**未跑的项**：N8 的**进程级 e2e**（需真实 provider 凭证）→ 归 `MANUAL_ACK_REQUIRED` 之外：
退出码逻辑已在单测覆盖，进程级验证留待有凭证时补（见 §遗留）。

## 字节级证据

`git diff 43ad632c --stat`（生产 + 测试）：

```text
 adapters/tools-basic/src/grep.rs | 185 ++++-
 adapters/tools-basic/src/read.rs |  41 ++
 apps/coding-agent/src/main.rs    |  38 +++-
 apps/coding-agent/src/prompt.rs  |  42 ++
 crates/ys-core/src/lib.rs        |  13 ++
 crates/ys-core/src/stop.rs       |   5 +
 crates/ys-loop/src/basic.rs      |  69 ++-
 crates/ys-tool/src/notice.rs     |  40 ++
 crates/ys-tool/src/truncate.rs   |  20 ++
 crates/ys-tui-coding/src/app.rs  |  64 ++-
 crates/ys-tui-coding/src/draw.rs |   3 +
```

## 质量门

- `cargo fmt`：**我改的 4 个 `notice.rs`/`grep.rs`/`read.rs`/`basic.rs` 已由基线干净 → 现在仍干净**；
  `app.rs` / `main.rs` 的其余 fmt 差异均为**基线既有**（`WorktreeCmd`、`thinking_visible` 断言等），未新增。
- `cargo clippy --all-targets`：**我改的文件无新增告警**（`truncate.rs` 的常量断言已 `#[allow(clippy::assertions_on_constants)]`）。

## 问题清单

### P0（阻断）
无。

### P1（应修）
无。

### P2（记录）

1. **`size_notice` 签名偏离设计**：设计写 `size_notice(bytes, matches: usize)`；实现对 `read`（无命中数概念）
   不适用，改为 `matches: Option<usize>`。**功能等价、更诚实**，但 `design.md` 接口概要需同步（Phase 4 已改）。
2. **空响应判定不看 `finish_reason`**：传输层空（`llm-api-unavailable` 管辖）在上游落地前也会显示为 `NoOutput`。
   设计与 ADR 已写明该「顺序依赖」，非缺陷，但需在 `llm-api-unavailable` 落地时接管。
3. **阈值未经真实会话校准**：`16 KiB` / `8 KiB` / `5` 是初值；建议按下一次真实会话复算校准
   （`docs/arch/turn-output-and-feedback/context.md` 有复算脚本）。

## 结论

**放行。** 六个单元全部通过，测试合同 N1–N9 逐条落实，全量回归 1120/0/9 无回归，无 P0/P1 问题。
三项 P2 均已记录并各有去处。

## 遗留（MANUAL_ACK_REQUIRED）

- [ ] 有 provider 凭证时，跑一次 `-p` 空响应路径，确认**进程退出码为 1 且 stderr 有一行说明**
      （单测已覆盖逻辑，此行仅为进程级确认）。
