# 单回合输出规模与可见性 — 修改报告

> 日期：2026-10-12 ｜ 基线 `43ad632c` ｜ 设计 `docs/arch/turn-output-and-feedback/design.md`（v2）
> 审查 `docs/reports/2026-10-12-turn-output-and-feedback-review.md` ｜ as-built `docs/design-final/turn-output-and-feedback.md`

## 背景

真实会话暴露：单条 `grep` 结果 45KB（≈11k token）**无提示**；空 / 纯思考响应被当 `Completed` **静默收场**。
本轮把「单回合工具输出规模」收进边界、让「回合产出与可见性」可见。

## 改动清单（11 个源文件 + 5 个文档）

| 模块 | 文件 | 内容 |
|---|---|---|
| 词汇 | `crates/ys-core/src/stop.rs`、`lib.rs` | `StopReason::NoOutput` 加法式新增；serde roundtrip 钉 `"NoOutput"` |
| 形态 | `crates/ys-tool/src/truncate.rs`、`notice.rs` | `SOFT_RESULT_BYTES`(8 KiB)；`size_notice(bytes, Option<matches>)` |
| 工具 | `adapters/tools-basic/src/grep.rs` | 专用预算 16 KiB 截断 + 收窄指引；`MAX_CONTEXT=5` 钳制；软阈值提示 |
| 工具 | `adapters/tools-basic/src/read.rs` | 大结果软阈值提示（与续读通知互斥） |
| 循环 | `crates/ys-loop/src/basic.rs` | 空分支：`trim().is_empty()` ⇒ `NoOutput` |
| 渲染 | `crates/ys-tui-coding/src/draw.rs`、`app.rs` | `stop_symbol(NoOutput)="∅"`；`RunFinished{NoOutput}` ⇒ 一行 System |
| 应用 | `apps/coding-agent/src/main.rs`、`prompt.rs` | `single_turn_exit_code`：`NoOutput ⇒ 1` + stderr 一行；「先窄后宽」guideline |
| 文档 | `docs/CONTEXT.md`、`docs/design.md`、`docs/design-final/`、`docs/arch/llm-api-unavailable/context.md` | 术语 / 测试矩阵 / as-built / 交叉引用 |

**无新增文件、无新依赖、未放宽任何输出上限**（`grep` 预算为**收紧**）。

## 测试结果

| 命令 | 结果 |
|---|---|
| `cargo test -p ys-core` | 7 passed |
| `cargo test -p ys-tool` | 43 passed |
| `cargo test -p ys-tools-basic` | 79 passed |
| `cargo test -p ys-loop` | 67 passed |
| `cargo test -p ys-tui-coding` | 279 passed |
| `cargo test -p ys-coding-agent` | 291 passed（lib） |
| **`cargo test --workspace`** | **1120 passed / 0 failed / 9 ignored** |
| `cargo fmt --check` | 我改的 4 个曾脏文件已回干净；`app.rs`/`main.rs` 剩余差异为基线既有 |
| `cargo clippy --all-targets` | 我改的文件**无新增告警** |

## 验收对照（测试合同 N1–N9）

- N1–N4（grep 预算 / 软阈值 / context / read 提示）：`ys-tools-basic` 新用例 ✅
- N5（guideline）：`ys-coding-agent` ✅
- N6（空响应 `NoOutput`）：`ys-loop` + `ys-core` ✅
- N7（TUI 可见行）：`ys-tui-coding` ✅
- N8（退出码 1）：`ys-coding-agent` ✅（进程级 e2e 待凭证）
- N9（无回归）：`--workspace` 1120/0/9 ✅

## 遗留 / MANUAL_ACK_REQUIRED

- [ ] **有 provider 凭证时**跑一次 `-p` 空响应路径，确认**进程退出码为 1 且 stderr 有一行说明**
      （单测已覆盖逻辑，此行仅为进程级确认）。
- [ ] 阈值（16 KiB / 8 KiB / `MAX_CONTEXT=5`）待下一次真实会话复算校准
      （`docs/arch/turn-output-and-feedback/context.md` 有复算脚本）。
- [ ] `llm-api-unavailable` 落地时，由其接管「传输层空 ⇒ `Err`」；届时 `NoOutput` 只剩合法空一档。
