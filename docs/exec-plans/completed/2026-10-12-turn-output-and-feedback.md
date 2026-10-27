# turn-output-and-feedback 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据** — Phase 3 据此合并执行单元并划分波次。

## 上下文引用

参考设计计划：`docs/design-plans/completed/2026-10-12-turn-output-and-feedback.md`
上游设计：`docs/arch/turn-output-and-feedback/design.md`（v2）

## 基线

- **基线 ref**: `43ad632c0ffef366102358db1f15d2b64901d5ed`（备份分支 `sw-backup/2026-10-12-turn-output-and-feedback`；原 HEAD `3c9aef5`）
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含 `??` 新文件）

## 任务清单

### Task 1: crates/ys-core — StopReason::NoOutput 变体 [type=impl]
- **涉及目录**: `crates/ys-core/src/`
- **涉及文件**: `crates/ys-core/src/stop.rs`, `crates/ys-core/src/lib.rs`
- **描述**: 新增 `StopReason::NoOutput`（加法式 + doc 注释）；`lib.rs` 变体集合测试补该项，并照 `Truncated` 加 serde roundtrip 钉住 `"NoOutput"` 字面量
- **关联测试任务**: T2
- **依赖**: 无

### Task 2: crates/ys-core 测试 [type=test]
- **涉及目录**: `crates/ys-core/src/`
- **涉及文件**: `crates/ys-core/src/lib.rs`
- **描述**: 覆盖 N6 的 ys-core 部分：`NoOutput` serde roundtrip + 变体集合完整
- **验证方法**:
  - `cargo test -p ys-core` — 期望全绿
- **关联 impl**: T1
- **依赖**: 无

### Task 3: crates/ys-tool — 软阈值常量 + notice 助手 [type=impl]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/truncate.rs`, `crates/ys-tool/src/notice.rs`
- **描述**: 加 `SOFT_RESULT_BYTES = 8 * 1024`；加 `size_notice(bytes, matches) -> String`（统一文案形态）
- **关联测试任务**: T4
- **依赖**: 无

### Task 4: crates/ys-tool 测试 [type=test]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/truncate.rs`, `crates/ys-tool/src/notice.rs`
- **描述**: 覆盖 `SOFT_RESULT_BYTES` 常量值与 `size_notice` 文案形态
- **验证方法**:
  - `cargo test -p ys-tool` — 期望全绿
- **关联 impl**: T3
- **依赖**: 无

### Task 5: adapters/tools-basic — grep 收窄/提示 + read 软阈值 [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/grep.rs`, `adapters/tools-basic/src/read.rs`
- **描述**: grep：`GREP_MAX_RESULT_BYTES`(16 KiB) 截断 + **收窄指引 notice**；`MAX_CONTEXT = 5` 钳制 + notice；软阈值 notice。read：软阈值 notice
- **关联测试任务**: T6
- **依赖**: T3（需 `SOFT_RESULT_BYTES` / `size_notice`）

### Task 6: adapters/tools-basic 测试 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/grep.rs`, `adapters/tools-basic/src/read.rs`
- **描述**: 覆盖 N1 / N2 / N3 / N4
- **验证方法**:
  - `cargo test -p ys-tools-basic` — 期望全绿，含新增用例
- **关联 impl**: T5
- **依赖**: T3

### Task 7: crates/ys-loop — 空响应改判 NoOutput [type=impl]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`
- **描述**: `tool_calls.is_empty()` 分支：`text.trim().is_empty()` ⇒ `stop_reason = NoOutput`（`emit(RunFinished)` + `RunResult` 同值）
- **关联测试任务**: T8
- **依赖**: T1（需 `StopReason::NoOutput`）

### Task 8: crates/ys-loop 测试 [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`
- **描述**: 覆盖 N6：`MockModel` 喂 `content=[]` 与 `" "` ⇒ 断 `NoOutput`
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全绿
- **关联 impl**: T7
- **依赖**: T1

### Task 9: crates/ys-tui-coding — NoOutput 渲染 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/app.rs`
- **描述**: `stop_symbol` 加 `NoOutput` 分支；`RunFinished` 臂在 `NoOutput` 时推一行可见 `System` 提示
- **关联测试任务**: T10
- **依赖**: T1

### Task 10: crates/ys-tui-coding 测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/app.rs`
- **描述**: 覆盖 N7
- **验证方法**:
  - `cargo test -p ys-tui-coding` — 期望全绿
- **关联 impl**: T9
- **依赖**: T1

### Task 11: apps/coding-agent — 退出码 + guideline [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/prompt.rs`
- **描述**: `single_turn_exit_code`：`NoOutput ⇒ 1` + stderr 一行 + 更新 doc 注释（原因集合）；`tool_guidelines` 加「先窄后宽」
- **关联测试任务**: T12
- **依赖**: T1

### Task 12: apps/coding-agent 测试 + 全量回归 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/prompt.rs`
- **描述**: 覆盖 N5 / N8，并做全量回归 N9
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 期望全绿
  - `cargo test --workspace` — 期望无新增失败
  - `cargo clippy --all-targets`、`cargo fmt --check` — 期望干净
- **关联 impl**: T11
- **依赖**: T1

## 波次计划（Phase 3 计算后回填）

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `crates/ys-core/src/stop.rs`, `crates/ys-core/src/lib.rs` | 无 | W0 | deep |
| U2 | T3, T4 | `crates/ys-tool/src/truncate.rs`, `crates/ys-tool/src/notice.rs` | 无 | W0 | shallow |
| U3 | T5, T6 | `adapters/tools-basic/src/grep.rs`, `adapters/tools-basic/src/read.rs` | U2 | W1 | deep |
| U4 | T7, T8 | `crates/ys-loop/src/basic.rs` | U1 | W1 | deep |
| U5 | T9, T10 | `crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/app.rs` | U1 | W1 | shallow |
| U6 | T11, T12 | `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/prompt.rs` | U1 | W1 | deep |

## 验证清单

- [ ] **所有 (impl, test) 配对均 completed**（Phase 3 review 放行条件）
- [ ] `cargo test --workspace` 通过
- [ ] `cargo clippy --all-targets` / `cargo fmt --check` 干净
- [ ] 文档同步：`docs/CONTEXT.md`、`docs/design.md` §5/§12、`llm-api-unavailable` 交叉引用
