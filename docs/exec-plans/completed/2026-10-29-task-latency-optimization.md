# 任务延迟优化（第一刀 A1–A7）执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **测试任务（`type=test`）是一等公民**——它们是 Phase 3 必须创建、必须完成、必须被证据支持的独立任务。
> **依赖声明是并发调度的依据**——Phase 3 据此合并执行单元并划分波次。

## 上下文引用

参考设计计划：`docs/design-plans/2026-10-29-task-latency-optimization.md`
参考验证策略：设计计划「测试策略」节（测试合同 N1–N16）

## 基线

- **基线 ref**: `<Phase 3 开始时由 scripts/baseline.sh 填写>`
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含 `??` 新文件）

## 任务清单

### Task 1: apps/coding-agent/ — 度量核心 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/round_clock.rs`, `apps/coding-agent/src/debug_log.rs`
- **描述**: `RoundClock` 扩展 `TurnTotals`（`rounds` / `tool_calls` / 三段耗时 / `input_tokens` / `cache_hit` / `cache_miss` / `cache_reported`），新增 `summary_line(turn)`；`debug_log.rs` 支持 `ev="run_reading"` 记录。
- **关联测试任务**: T2
- **依赖**: 无

### Task 2: apps/coding-agent/ — 度量核心单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/round_clock.rs`（内嵌 `#[cfg(test)] mod tests`）
- **描述**: 覆盖多轮累加、`summary_line` 定形正则、缓存分母只算上报轮、全未上报时 `cache_pct=n/a`。
- **验证方法**:
  - `cargo test -p ys-coding-agent round_clock` — 期望 ≥6 passed
  - `cargo test -p ys-coding-agent summary_line` — 期望 ≥2 passed
- **关联 impl**: T1
- **依赖**: T1

### Task 3: apps/coding-agent/ — 配置面与有效窗口 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/config.rs`
- **描述**: 新增 `context_cap_tokens` / `single_call_nudge_rounds` / `soft_round_advice_at` 三个顶层标量（`RawUserConfig` → 校验 → `UserConfig` → accessor），并实现 `effective_context_window()` / `effective_context_window_for()`（`min(声明, cap)`，`cap=0` 不封顶）。
- **关联测试任务**: T4
- **依赖**: 无

### Task 4: apps/coding-agent/ — 配置面单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/config.rs`（内嵌测试）
- **描述**: 覆盖三个标量的解析与非法值丢弃、`min` 语义（1M+200k / cap=0 / 128k+200k）、`effective_context_window_for(子模型)`。
- **验证方法**:
  - `cargo test -p ys-coding-agent config` — 期望 ≥8 passed
  - `cargo test -p ys-coding-agent effective_context_window` — 期望 ≥4 passed
- **关联 impl**: T3
- **依赖**: T3

### Task 5: adapters/tools-basic/ — `read` schema 修正 [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/read.rs`
- **描述**: 从 `required` 移除 `path`，与既有 `paths` 对齐；两者都缺时仍由 `call` 的 `InvalidInput("missing 'path'")` 兜底。
- **关联测试任务**: T6
- **依赖**: 无

### Task 6: adapters/tools-basic/ — `read` schema 单测 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/read.rs`（内嵌测试）
- **描述**: 三条断言：`required` 不含 `path`；只给 `paths` 成功；`path` 与 `paths` 都不给报 `InvalidInput`。既有 `multi_file_reads_in_one_call_with_headers` 保持绿。
- **验证方法**:
  - `cargo test -p ys-tools-basic read` — 期望全部通过（含新增 3 条）
  - `cargo test -p ys-tools-basic multi_file_reads_in_one_call_with_headers` — 期望 1 passed
- **关联 impl**: T5
- **依赖**: T5

### Task 7: 提示面 — 批处理指引与任务边界 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`, `skills/sequential-workflow/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`, `skills/sequential-workflow/SKILL.md`
- **描述**: 重写批处理段（删 `one shell loop is fine` 逃生口，给出 `read paths=[...]` / `grep paths=[...]` 形状，保留 `Keep dependent steps separate`）；新增任务边界 guideline；SKILL.md 的 Phase 4/5 验证范围限定为「由该 workflow 驱动的实现任务」。
- **关联测试任务**: T8
- **依赖**: 无

### Task 8: 提示面 — 文案单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`（内嵌测试）
- **描述**: 断言批处理段不含 `one shell loop is fine`、含 `paths=[...]` 与 `Keep dependent steps separate`；任务边界 guideline 在 `bash` 在场时出现且含两句关键约束；同步更新既有守卫测试 `multi_target_guideline_appears_only_with_read_only_tools`。
- **验证方法**:
  - `cargo test -p ys-coding-agent prompt` — 期望 ≥10 passed
  - `cargo test -p ys-coding-agent multi_target_guideline_appears_only_with_read_only_tools` — 期望 1 passed
- **关联 impl**: T7
- **依赖**: T7

### Task 9: 提醒通道 core [type=impl]
- **涉及目录**: `crates/ys-component/src/`, `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-component/src/limits.rs`, `crates/ys-loop/src/basic.rs`
- **描述**: `RunLimits` 新增 `single_call_nudge_rounds` / `soft_round_advice_at`（默认 0）与两个默认阈值常量；`basic.rs` 新增 `SINGLE_CALL_NUDGE_PROMPT` / `SOFT_ROUND_ADVICE` 常量、两个独立计数（A4 不因模型说话清零；A6 每回合重置）、Step 5 按 `narration > A4 > A6` 每轮取一注入且未发标志顺延不清零。
- **关联测试任务**: T10
- **依赖**: 无

### Task 10: 提醒通道 core 单测 [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`（内嵌测试）
- **描述**: 覆盖 A4 计数规则（1 个调用 +1、≥2 归零、模型说话不清零、到阈值注入且不落 session、阈值翻倍）、A6 60→120→240 与每回合重置、三条同时置位时只发一条且为 narration、标志顺延。
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全部通过（含新增 ≥8 条）
  - `cargo test -p ys-loop single_call` — 期望 ≥4 passed
  - `cargo test -p ys-loop soft_round` — 期望 ≥3 passed
  - `cargo test -p ys-loop nudge_priority` — 期望 ≥1 passed
- **关联 impl**: T9
- **依赖**: T9

### Task 11: 应用接线 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/app_loop.rs`, `apps/coding-agent/src/subagent.rs`
- **描述**: `-p` / `--json` 终局后写 stderr、TUI 回合末走 `Outbound::Output`、`run_reading` 落 debug；窗口读取全部改 effective（含 `subagent.rs` 的 `child_limits_for`，D3）；`resolve_limits` 设 A4/A6 默认 4 / 60；启动日志并列 declared/effective。
- **关联测试任务**: T12
- **依赖**: T1, T3, T9

### Task 12: 应用接线测试 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/app_loop.rs`, `apps/coding-agent/src/subagent.rs`（内嵌测试）
- **描述**: 断言 `-p` stderr 含读数行而 stdout 干净；`child_limits_for` 回归钉（父 cap 200k + 子模型声明 1M ⇒ 子 `limits.context_window == 200_000`）；`/status` 快照字段与 effective 窗口一致。
- **验证方法**:
  - `cargo test -p ys-coding-agent subagent` — 期望 ≥5 passed（含 `child_limits_for` 回归钉）
  - `cargo test -p ys-coding-agent run_reading` — 期望 ≥1 passed
  - `cargo test -p ys-coding-agent` — 期望全部通过
- **关联 impl**: T11
- **依赖**: T11

### Task 13: 展示口径 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`, `apps/coding-agent/src/`
- **涉及文件**: `crates/ys-tui-coding/src/view.rs`, `crates/ys-tui-coding/src/commands.rs`, `apps/coding-agent/src/view.rs`, `apps/coding-agent/src/capabilities.rs`
- **描述**: `CodingView` 新增 `context_window_declared: Option<usize>`；`/status` 在 declared ≠ effective 时渲染 `200k / 1M (capped)`，相等时只显示一个值；`/model` 同口径。
- **关联测试任务**: T14
- **依赖**: T3

### Task 14: 展示口径测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/commands.rs`（内嵌测试）
- **描述**: declared ≠ effective 时输出含 `(capped)` 与两个值；相等时只输出一个值。
- **验证方法**:
  - `cargo test -p ys-tui-coding` — 期望全部通过（含新增 ≥2 条）
  - `cargo test -p ys-tui-coding status` — 期望 ≥2 passed
- **关联 impl**: T13
- **依赖**: T13

### Task 15: bench 落盘 [type=impl]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/e2e_bench.rs`
- **描述**: `Reading` 新增 `tpr_x100` / `input_med` / `single_call_rounds`；每条结果写 `bench-summary.json`；支持 `YUSHAN_BENCH_OUT` 覆盖路径；stdout 表格逐字节不变。
- **关联测试任务**: T16
- **依赖**: T1

### Task 16: bench 落盘测试 [type=test]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/e2e_bench.rs`（内嵌测试）
- **描述**: 断言 JSON 字段齐全、`YUSHAN_BENCH_OUT` 覆盖生效、未设置时写 run dir、stdout 表格输出逐字节不变。
- **验证方法**:
  - `cargo test -p ys-coding-agent --test e2e_bench` — 期望全部通过（非 `--ignored` 的内嵌测试）
  - `cargo test -p ys-coding-agent --test e2e_bench bench_summary` — 期望 ≥2 passed
- **关联 impl**: T15
- **依赖**: T15

### Task 17: 文档 [type=impl]
- **涉及目录**: `docs/`, `docs/adr/`
- **涉及文件**: `docs/design.md`, `docs/adr/0062-task-latency-levers.md`
- **描述**: `docs/design.md` §12 测试矩阵新增本特性条目；新增 ADR-0062，写明 A6 反转 ADR-0027「软警告本轮不做」的理由与边界，以及第一刀真实下界约 2 min。
- **关联测试任务**: T18
- **依赖**: 无

### Task 18: 文档验证 [type=test]
- **涉及目录**: `docs/`, `docs/adr/`
- **涉及文件**: `docs/design.md`, `docs/adr/0062-task-latency-levers.md`
- **描述**: grep 断言矩阵条目与 ADR 关键句存在（A6 反转理由、边界、真实下界）。
- **验证方法**:
  - `grep -q "task-latency" docs/design.md` — 期望退出码 0
  - `grep -q "ADR-0027" docs/adr/0062-task-latency-levers.md` — 期望退出码 0
  - `grep -q "soft_round_advice_at" docs/adr/0062-task-latency-levers.md` — 期望退出码 0
  - `grep -q "2 min" docs/adr/0062-task-latency-levers.md` — 期望退出码 0
- **关联 impl**: T17
- **依赖**: T17

### Task 19: 变异测试（注入 → 跑 → 回滚） [type=test]
- **涉及目录**: `apps/coding-agent/src/`, `crates/ys-loop/src/`
- **涉及文件**: `apps/coding-agent/src/round_clock.rs`, `apps/coding-agent/src/config.rs`, `crates/ys-loop/src/basic.rs`（**临时修改后必须逐项回滚**）
- **描述**: 逐项注入 5 个 bug 并确认对应用例变红：① A4 模型说话也清零；② A6 阈值不翻倍；③ A5 用 declared 而非 effective；④ `summary_line` 缓存分母算全部轮；⑤ 提醒优先级取最后一个 `Some`。每项注入后立即回滚，最终 `git diff` 相对基线**不得含注入残留**。
- **验证方法**:
  - 每项：注入 → `cargo test -p <crate> <对应用例>` — 期望 **FAILED**（红）→ `git checkout -- <file>` 回滚
  - 全部回滚后：`git diff <基线 ref> -- apps/coding-agent/src/round_clock.rs apps/coding-agent/src/config.rs crates/ys-loop/src/basic.rs` — 期望与注入前一致（无残留）
  - 最终 `./scripts/par-test.sh` — 期望退出码 0
- **关联 impl**: T1, T3, T9
- **依赖**: T2, T4, T10

## 波次计划（Phase 3 计算后回填）

> Phase 3 开始时按 [3.2](#phase-3) 合并单元并分层，回填于此。

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|

（同波内文件集必须互不相交；相交则顺延到下一波）

## 验证清单

- [ ] **所有 (impl, test) 配对均已 completed**：T1+T2、T3+T4、T5+T6、T7+T8、T9+T10、T11+T12、T13+T14、T15+T16、T17+T18、T19（变异，关联 T1/T3/T9）（Phase 3 review 放行条件）
- [ ] N1 `cargo test -p ys-coding-agent round_clock` — 通过
- [ ] N2 单测断言读数行格式化纯函数；grep 断言 `main.rs` 终局后调 stderr 出口、`app_loop.rs` 回合末走 `Outbound::Output`；`cargo test -p ys-coding-agent run_reading` — 通过（真实 `-p` 端到端为**可推迟**，并入 N16 人工验收）
- [ ] N3 `cargo test -p ys-coding-agent --test e2e_bench bench_summary` — 通过
- [ ] N4 `cargo test -p ys-tools-basic read` — 通过
- [ ] N5 `cargo test -p ys-coding-agent prompt` — 通过
- [ ] N6 `cargo test -p ys-loop single_call` — 通过
- [ ] N7 `cargo test -p ys-coding-agent effective_context_window` — 通过
- [ ] N8 `grep -rn "effective_context_window" apps/coding-agent/src/{main.rs,app_loop.rs,subagent.rs}` — 5 处命中；`cargo test -p ys-coding-agent subagent` — 通过
- [ ] N9 `cargo test -p ys-tui-coding status` — 通过
- [ ] N10 `cargo test -p ys-loop soft_round` — 通过
- [ ] N11 `cargo test -p ys-loop nudge_priority` — 通过
- [ ] N12 `cargo test -p ys-coding-agent prompt` — 通过
- [ ] N13 `cargo test -p ys-coding-agent toolset` — 通过（指纹锚已更新，尾断言不变）
- [ ] N14 变异测试：逐项注入 A4 说话清零 / A6 不翻倍 / A5 用 declared / 缓存分母算全部轮 / 优先级取最后 `Some`，对应用例变红
- [ ] N15 `./scripts/par-test.sh` 退出码 0；`cargo clippy --all-targets` 无新警告；`cargo fmt --check` 干净
- [ ] N16 **MANUAL_ACK_REQUIRED**（Phase 4 报告须原样保留，由用户勾选）:
  - [ ] 设置 `YUSHAN_API_BASE` / `YUSHAN_API_KEY`
  - [ ] 跑 3 次 bench：`YUSHAN_BENCH_REPEATS=3 cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture`
  - [ ] 对比 `bench-summary.json`：`rounds` 降幅 ≥15%
  - [ ] 确认 `cache_hit` 不低于 98.3%
  - [ ] 确认 `QUALITY_SPECS` 不回退
