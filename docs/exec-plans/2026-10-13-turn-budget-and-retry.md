# turn-budget-and-retry 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据** —— Phase 3 据此合并执行单元并划分波次。
> 参考设计计划：`docs/design-plans/2026-10-13-turn-budget-and-retry.md`
> 上游设计：`docs/arch/turn-output-and-feedback/design.md`（v2）

## 基线

- **基线 ref**: 由 Phase 3.0 `scripts/baseline.sh` 产出（工作区干净 ⇒ 以当时 HEAD 为基线）。

## 任务清单

### Task 1: crates/ys-component — RunLimits 新增两字段 [type=impl]
- **涉及目录**: `crates/ys-component/src/`
- **涉及文件**: `crates/ys-component/src/limits.rs`
- **描述**: 新增 `pub max_turn_tool_result_tokens: usize`（`Default` = 0）与 `pub retry_empty_response: bool`（`Default` = true）；doc 注释写明语义与「默认不扰动既有行为」的承重理由。
- **关联测试任务**: T2
- **依赖**: 无

### Task 2: crates/ys-component 测试 [type=test]
- **涉及目录**: `crates/ys-component/src/`
- **涉及文件**: `crates/ys-component/src/limits.rs`
- **描述**: 覆盖 N17：两个新字段默认值 + 可显式设置（仿既有 `fixed_prompt_tokens_*` 用例）；写明变异点。
- **验证方法**:
  - `cargo test -p ys-component` — 期望全绿
- **关联 impl**: T1
- **依赖**: T1

### Task 3: crates/ys-loop — turn_budget 模块（纯算法） [type=impl]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/turn_budget.rs`（新增）, `crates/ys-loop/src/lib.rs`
- **描述**: 新建 `turn_budget.rs`：常量 `ELIDE_HEAD_LINES`/`ELIDE_HEAD_BYTES`、哨兵标记、`tool_result_tokens` / `needs_elision` / `elide_message` / `plan_elisions`（纯函数，复用 `ys_tool::head_lines` / `ys_tool::append_notices` 与 `crate::token`）。`lib.rs` 加 `mod turn_budget; pub use turn_budget::*;`（按既有模块风格）。
- **关联测试任务**: T4
- **依赖**: 无

### Task 4: crates/ys-loop — turn_budget 单元测试 [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/turn_budget.rs`
- **描述**: 覆盖 N10 的算法内核与 N15：`plan_elisions` 超预算降最旧 / 保留最新 / 预算 0 空；`elide_message` 保留 `tool_call_id` 与块类型、幂等；`needs_elision` 边界。写明变异点。
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全绿
- **关联 impl**: T3
- **依赖**: T3

### Task 5: crates/ys-loop — 闸门接线 + 空响应重试 [type=impl]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`
- **描述**: (A) `run_turn` 记 `turn_start`，Step 10 后 `plan_elisions` + `replace_all` 降级（含压缩后的下界钳制）。(B) `tool_calls.is_empty()` 空产出分支加一次性重试（催促消息 + `continue`），受 `finish_reason == Stop` 与 `retry_empty_response` 闸门。
- **关联测试任务**: T6
- **依赖**: T1, T3

### Task 6: crates/ys-loop — 闸门与重试的接线测试 [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`
- **描述**: 覆盖 N10（接线层）/ N11 / N12 / N13 / N14；**更新既有**「空响应 ⇒ NoOutput」用例（`empty_response_ends_with_no_output_not_completed`）以反映重试语义（喂两次空响应）。写明变异点。
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全绿
- **关联 impl**: T5
- **依赖**: T5

### Task 7: apps/coding-agent — resolve_limits 设闸门预算 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`
- **描述**: `resolve_limits` 里 `limits.max_turn_tool_result_tokens = config.context_window() / 4`（不新增 config 字段）。
- **关联测试任务**: T8
- **依赖**: T1

### Task 8: apps/coding-agent — 预算接线测试 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`
- **描述**: 覆盖 N16：`resolve_limits` 产出的 `max_turn_tool_result_tokens == context_window/4`。写明变异点。
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 期望全绿
- **关联 impl**: T7
- **依赖**: T7

### Task 9: apps/coding-agent — 过程叙述 guideline [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`
- **描述**: `tool_guidelines` 加一条「多轮工具任务每批前一句话叙述」。当且仅当工具表含工作型工具（read/grep/find/ls/bash/edit/write 任一）时出现。
- **关联测试任务**: T10
- **依赖**: 无

### Task 10: apps/coding-agent — guideline 测试 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`
- **描述**: 覆盖 N18：含 read / 或含 bash ⇒ 出现；空工具表 ⇒ 不出现。写明变异点。
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 期望全绿
- **关联 impl**: T9
- **依赖**: T9

### Task 11: 全量回归 [type=test]
- **涉及目录**: （全仓回归，无新增文件）
- **涉及文件**: 无（跑命令）
- **描述**: N19 —— `cargo test --workspace` 全绿且不降级；`cargo clippy --all-targets` / `cargo fmt --check` 对本批改动文件干净。
- **验证方法**:
  - `cargo test --workspace`
- **关联 impl**: T5, T7, T9
- **依赖**: T6, T8, T10

## 波次计划（Phase 3 计算后回填）

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `crates/ys-component/src/limits.rs` | 无 | W0 | shallow |
| U2 | T3, T4 | `crates/ys-loop/src/turn_budget.rs`, `crates/ys-loop/src/lib.rs` | 无 | W0 | deep |
| U3 | T9, T10 | `apps/coding-agent/src/prompt.rs` | 无 | W0 | shallow |
| U4 | T5, T6 | `crates/ys-loop/src/basic.rs` | U1, U2 | W1 | deep |
| U5 | T7, T8 | `apps/coding-agent/src/main.rs` | U1 | W1 | shallow |

> T11（全量回归）由 orchestrator 在 Phase 3.6 / Phase 5 直接跑。

## 验证清单

- [ ] 所有 (impl, test) 配对均 completed
- [ ] `cargo test --workspace` 通过、不降级
- [ ] `cargo clippy --all-targets` / `cargo fmt --check`（本批改动文件）干净
- [ ] 文档同步：`docs/CONTEXT.md`、`docs/design.md` §5/§12、`docs/design-final/`、上游 `context.md`
