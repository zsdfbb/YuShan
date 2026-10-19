# Subagent 可观测性与默认 Debug 执行计划

> 每个任务按模块划分；impl/test 通过“关联任务”配对。
> **依赖声明是 Phase 3 并发调度的依据**。`Task N` 与设计计划的 `N1`–`N9` 验收项编号不通用。

## 上下文引用

参考设计计划：`docs/design-plans/2026-10-19-subagent-observability-and-debug-defaults.md`

## 基线

- **基线 ref**: `d5df3b76c12f20b86356d89e4f8522221b2ff3c7`
- **备份分支**: `sw-backup/2026-10-19-subagent-observability-and-debug-defaults`
- **Phase 1 核实偏差已吸收**: 新 ADR 改用 0044；诊断、原始工具结果、并行测试、提示测试、
  debug 默认翻转与文档波及面已按核实结论补全，Phase 3 基线仍待填写。
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含未跟踪新文件）。

## 任务清单

### Task 1: apps/coding-agent — subagent 诊断完整  [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/subagent.rs`
- **描述**:
  - 新增内部取消来源与耗时；`Cause` 只在 `Cancelled` 时填写，其他非完成 stop reason 不编造 cause。
  - 新增 `MAX_DIAGNOSTIC_BYTES = 1024` 的同一清洗函数：CR/LF/其他控制字符替换为空格，
    按 UTF-8 边界截断，超限以 ASCII `...` 结尾，保持 `[diagnostic:` 前缀。
  - 只有子回合已经启动、`child.run_turn` 返回 `Err` 时才构造 `SubagentOutcome` 并写
    `stop_reason=cancelled cause=unknown`，error 文本走同一清洗；无可用模型、派发超限、
    `AgentBuilder` 失败等前置错误直接返回 `Err`，由 `SubagentTool` 映射为
    `ToolError::Execution`，不生成 diagnostic 前缀。
  - 子 runner 使用 `CollectingSink` 捕获事件，从 `ToolResult` 事件按 `tool_call_id`
    建立原始结果表；转录仍按会话块顺序，Text/ToolUse 取会话，ToolResult 正文优先取事件原文，
    缺失时才回退会话，避免 turn budget 降级文本污染转录。
  - 按诊断首行组装 `ToolResult`；转录含终局元数据，写入错误并入诊断。
  - 公共 `SubagentOutcome` / `SubagentRunner` 不变。
- **关联测试任务**: Task 2
- **依赖**: 无

### Task 2: apps/coding-agent — subagent 诊断与转录测试  [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/subagent.rs`（`#[cfg(test)]`）
- **描述**:
  - 分类测试覆盖 parent / child budget / unknown，并断言 parent 优先；测试 helper 以纯函数接收两个 bool。
  - 断言 diagnostic 首行格式、`MAX_DIAGNOSTIC_BYTES=1024` 上限、CR/LF/控制字符变空格、
    多字节 UTF-8 安全截断及 ASCII `...` 尾；非 `Cancelled` 不写 cause。
  - 断言子回合启动后的 `run_turn Err` 为 `stop_reason=cancelled cause=unknown`，error 文本经同一清洗并说明真实失败；前置 runner `Err` 映射为 `ToolError::Execution` 且没有 diagnostic 前缀。
  - 用 `MemorySession` + `CollectingSink` 构造 Text + ToolUse + ToolResult，断言转录包含
    id/name/args/原始 result/is_error/stop_reason/cause/elapsed。
  - 构造超过 `max_turn_tool_result_tokens` 的较早结果，确认会话已被预算降级后，
    转录仍按 `tool_call_id` 保留事件里的原始 ToolResult。
  - 用“父目录是普通文件”的 `SubagentsDir` 制造写失败，断言诊断含 transcript 写入错误。
  - 更新 fake outcome → ToolResult 测试，断言诊断在摘要和路径之前。
- **验证方法**:
  - `cargo test -p ys-coding-agent subagent` — 全部通过
  - 变异：把诊断移到末尾、删掉块遍历、父取消优先级反转，对应断言变红。
- **关联 impl**: Task 1
- **依赖**: Task 1

### Task 3: crates/ys-loop — 并行结果完成即发  [type=impl]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`
- **描述**: 并行工具批用带原始 index 的 future +
  `buffer_unordered(max_parallel_tools.max(1))` 执行；每次完成立即 emit 对应 `ToolResult`
  并填 `outcomes[index]`。保留 `.max(1)` 防护、先发全部 `ToolCall`、串行回退与并发上限；
  Step 9c 仍按原 tool-call 顺序组装父模型消息。当前安全集合是 `subagent` 与
  `read` / `find` / `ls` / `grep`。
- **关联测试任务**: Task 4
- **依赖**: 无

### Task 4: crates/ys-loop — 时序与父回合继续/停止测试  [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`（`#[cfg(test)]`）
- **描述**:
  - 用 `oneshot` / `Notify`（或明显分离的延迟）控制一快一慢完成，断言全部 `ToolCall`
    先于任何 `ToolResult`、快者结果先出现；捕获会话并断言 tool result id 仍按原始调用顺序。
  - 改写 `batch_is_parallel_when_every_tool_is_safe` 与
    `parallel_respects_max_parallel_tools` 中硬编码 `R1,R2,...` 的断言，不再把批次收齐后的
    事件顺序当作契约。
  - 保留并复核默认串行、混合批回退、`max_parallel_tools=0` 的 `.max(1)` 下限与并发峰值。
  - 错误工具结果未取消父回合时发生第二次模型调用；父边界预先取消时零模型调用且 `RunFinished{Cancelled}`。
- **验证方法**:
  - `cargo test -p ys-loop` — 全部通过
  - 变异：改回 `buffered` 后整批 emit、按完成顺序写会话、错误结果后直接 return，对应断言变红。
- **关联 impl**: Task 3
- **依赖**: Task 3

### Task 5: crates/ys-tui-coding — Summary 可读原因  [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`
- **描述**: 将 `StopReason` 映射为短标签，Summary 行显示 `符号 + label + rounds + elapsed`；未知非穷尽变体显示 `unknown`。
- **关联测试任务**: Task 6
- **依赖**: 无

### Task 6: crates/ys-tui-coding — Summary 渲染测试  [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`（`#[cfg(test)]`）
- **描述**:
  - 逐项断言 Completed/Cancelled/MaxRounds/Truncated/NoOutput 的可读文本与符号。
  - 同步改写现有 `test_summary_symbols_per_stop_reason` 的旧格式断言，使其包含 label。
  - 窄宽度测试使用 `width >= MIN_WIDTH`；断言不重叠、不 panic，并接受 Paragraph 自动换行。
  - `StopReason` 为 `#[non_exhaustive]`，下游不能构造 unknown 变体；unknown 由通配分支
    保证，不伪造不可构造输入。
- **验证方法**:
  - `cargo test -p ys-tui-coding` — 全部通过
  - 变异：删掉 label、只改符号，标签断言变红。
- **关联 impl**: Task 5
- **依赖**: Task 5

### Task 7: apps/coding-agent — 通用 subagent 调度提示  [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`
- **描述**: 用两条通用 guideline 替换旧窄写入冲突规则：依赖/验证任务完成后派发，
  独立且写入不交叠才并行，范围不明串行；失败或取消先读 diagnostic 再缩窄重试或报告。
  必须删除旧规则而不是追加保留；并发槽位与成本提醒保持不变。不得出现 `review` / `test` 特例。
- **关联测试任务**: Task 8
- **依赖**: 无

### Task 8: apps/coding-agent — 调度提示测试  [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`（`#[cfg(test)]`）
- **描述**:
  - 新增测试名必须包含 `prompt`，避免被 `cargo test -p ys-coding-agent prompt` 静默过滤。
  - 断言父提示含依赖检查、完成后派发、互不相交、未知串行、失败读诊断；
    子提示不含派发 guideline。
  - `review` / `test` 负向断言只检查 subagent guideline 段，不扫描完整父提示，避免误伤
    `AGENTS.md` 项目上下文。
  - 断言旧窄写入规则已删除，并保留并发槽位与成本提醒。
- **验证方法**:
  - `cargo test -p ys-coding-agent prompt` — 全部通过
  - 变异：删除“完成后派发”或“范围不明串行”，对应断言变红。
- **关联 impl**: Task 7
- **依赖**: Task 7

### Task 9: apps/coding-agent — debug 默认开启与 7 天清理  [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/debug_log.rs`, `apps/coding-agent/src/logging.rs`, `apps/coding-agent/src/main.rs`
- **描述**:
  - `debug_enabled` 在 env 未设置/空串时返回 true；非空值规则与 env 优先级不变；保留参数与 `--debug` 解析兼容。
  - `logging` 一次解析 `{ path, source: Default | Override }`；非空 `YUSHAN_LOG_DIR`
    为 Override，空串回落 Default。写入与 sweep 共用这一解析结果。
  - `debug_log` 新增 7 天常量、可注入 now 的清理函数与正常启动 wrapper；
    只删 Default 目录中文件名匹配且为普通文件的 `debug-*.jsonl`，不跟随符号链接；
    Override 跳过。sweep 独立于 `open_enabled`，`YUSHAN_DEBUG=0` 的正常启动仍执行。
  - `main` 在 help/进程级 worktree 早退之后、正常启动 sink 之前调用一次清理；
    TUI/-p/--json/attach 仍共用 `open_enabled`。
  - 关闭 debug 只让 `open_enabled` 返回 `None`；正常启动无论开关值都解析一次 `LogDir`
    并执行 `Default` sweep，不能写成关闭后全启动路径“零路径解析”。
- **关联测试任务**: Task 10
- **依赖**: 无

### Task 10: apps/coding-agent — debug 开关、清理与集成测试  [type=test]
- **涉及目录**: `apps/coding-agent/src/`, `apps/coding-agent/tests/`
- **涉及文件**:
  - `apps/coding-agent/src/debug_log.rs`（`#[cfg(test)]`）
  - `apps/coding-agent/src/logging.rs`（`#[cfg(test)]`）
  - `apps/coding-agent/src/main.rs`（`#[cfg(test)]`）
  - `apps/coding-agent/src/app_loop.rs`（`#[cfg(test)]`）
  - `apps/coding-agent/tests/budget_exit.rs`
  - `apps/coding-agent/tests/debug_log.rs`
  - `apps/coding-agent/tests/degraded_search.rs`
  - `apps/coding-agent/tests/help_cli.rs`
  - `apps/coding-agent/tests/resume_cli.rs`
  - `apps/coding-agent/tests/session_cli.rs`
  - `apps/coding-agent/tests/skills_cli.rs`
  - `apps/coding-agent/tests/stream_usage_fallback.rs`
  - `apps/coding-agent/tests/truncation_guard.rs`
  - `apps/coding-agent/tests/worktree_cli.rs`
  - `apps/coding-agent/tests/e2e_bench.rs`（仅当 helper 需纳入统一 env 隔离）
- **描述**:
  - 更新开关矩阵：未设置/空串为 on；on 值 on；off 值即使 `--debug` 也 off。
  - 可注入 now 的清理测试：超龄 debug 删除；近期文件、非匹配文件、同名目录、符号链接、
    `yushan.log` 保留；Override 跳过；`YUSHAN_DEBUG=0` 仍清理 Default；目录不可读不 panic。
  - 进程测试：无 flag 的 `-p` / `--json` 默认生成一个 debug 文件并以 `run_end` 收尾；`YUSHAN_DEBUG=0` 时零文件。
  - `app_loop.rs` 的 `debug_off_tui_creates_no_files` 等假定关闭 debug 的既有用例显式设
    `YUSHAN_DEBUG=0`。
  - 所有启动二进制的集成测试 helper 先 `env_remove("YUSHAN_LOG_DIR")`，再按用途显式
    set/remove `YUSHAN_DEBUG`；默认开用例显式指向临时日志目录，避免继承真实 Override。
  - 至少核对 `budget_exit.rs`、`debug_log.rs`、`degraded_search.rs`、`help_cli.rs`、
    `resume_cli.rs`、`session_cli.rs`、`skills_cli.rs`、`stream_usage_fallback.rs`、
    `truncation_guard.rs`、`worktree_cli.rs`；`e2e_bench.rs` 仅在需要时处理。
  - env 测试使用 `env_lock` + `EnvRestore`，临时目录含原子序号 + PID。
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 覆盖所有受默认开启与 env helper 影响的单元/集成测试
  - `cargo test -p ys-coding-agent debug_enabled`
  - `cargo test -p ys-coding-agent debug_log`
  - `cargo test -p ys-coding-agent --test debug_log`
  - `cargo clippy --all-targets`
- **关联 impl**: Task 9
- **依赖**: Task 9

### Task 11: apps/coding-agent — help 文案与尾换行  [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/help.rs`
- **描述**: 更新 `YUSHAN_DEBUG` / `--debug` 说明为“默认开启，显式设置 0/false/no/off 关闭”；删除 `HELP_TEXT` 自身末尾换行，让现有 `println!` 只产生一个尾换行。
- **关联测试任务**: Task 12
- **依赖**: Task 9

### Task 12: apps/coding-agent — help 与进程级早退测试  [type=test]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/help_cli.rs`, `apps/coding-agent/tests/worktree_cli.rs`
- **描述**: help stdout 以一个 `\n` 结尾且不以 `\n\n` 结尾，三种形态逐字节一致；env 未设置时 help 和 worktree list 等进程级命令不创建 debug 文件。
- **验证方法**:
  - `cargo test -p ys-coding-agent --test help_cli --test worktree_cli` — 全部通过
  - 变异：恢复双换行或把 sink 创建提前到早退前，对应用例变红。
- **关联 impl**: Task 9, Task 11
- **依赖**: Task 9, Task 11

### Task 13: docs — 新增 ADR 并修正既有决策  [type=impl]
- **涉及目录**: `docs/adr/`, `docs/arch/agent-observability/`
- **涉及文件**: `docs/adr/0044-subagent-observability-and-debug-defaults.md`（新建）, `docs/adr/0022-subagent-dispatch.md`, `docs/adr/0029-subagent-landing-decisions.md`, `docs/arch/agent-observability/adr-debug-log.md`
- **描述**: ADR-0044 记录四项裁定：内部取消来源 + 有界单行诊断、基于事件原始结果的完整子转录、
  完成即发事件但模型消息保序、debug 默认开启 + 7 天清理 + Override 不清理。
  ADR-0043 已被 resume 修复占用，本任务不改该文件。修正 0022/0029 的 `buffered`
  时序和窄调度规则；observability ADR 更新默认值与 sweep 语义。
- **关联测试任务**: Task 14
- **依赖**: Task 1, Task 3, Task 7, Task 9, Task 11

### Task 14: docs — ADR 一致性核对  [type=test]
- **涉及目录**: `docs/adr/`, `docs/arch/agent-observability/`
- **涉及文件**: 同 Task 13
- **描述**: 用 `rg` 逐项核对 `buffer_unordered`、按完成顺序、模型消息原序、默认开启、
  7 天、Override 不删、parent-turn-cancelled、并发安全集合、runner 前置 `Err` 分界、
  摘要主体上限、诊断清洗与通用调度规则，确保无旧表述残留。
- **验证方法**:
  - `rg -n "buffered|按原始顺序 emit 全部 ToolResult|YUSHAN_DEBUG|默认开启|7 天|只写不删|显式开关|opt-in|不开时|没有默认保留期清理|今天没有工具|只有 .*subagent|零路径解析" docs/adr/0022-subagent-dispatch.md docs/adr/0029-subagent-landing-decisions.md docs/adr/0044-subagent-observability-and-debug-defaults.md docs/arch/agent-observability/adr-debug-log.md`
  - 期望：相关新语义存在；`buffered` 若保留仅作历史 rejected option 说明，不作为当前契约。
- **关联 impl**: Task 13
- **依赖**: Task 13

### Task 15: docs — as-built、架构与 AGENTS 同步  [type=impl]
- **涉及目录**: `docs/`, 仓库根
- **涉及文件**: `docs/architecture.md`, `docs/design-final/coding-agent-tui.md`,
  `docs/TODO.md`, `docs/arch/agent-observability/design.md`, `AGENTS.md`
- **描述**:
  - 更新并行事件时序、subagent 失败诊断/转录、TUI Summary、debug 默认开启与清理。
  - `AGENTS.md` 环境变量说明改为默认开启并给出关闭值。
  - `docs/TODO.md` 删除 A1 中已完成的 P2 debug sweep 行；若
    `docs/arch/agent-observability/design.md` 仍是当前设计状态台账，同步其 P2 状态。
  - 在 `docs/design-final/coding-agent-tui.md` 增加明确的 CLI help as-built 段落：
    `HELP_TEXT` 不自带尾换行，`println!` 合计只产生一个 `\n`。
- **关联测试任务**: Task 16
- **依赖**: Task 13

### Task 16: docs — as-built 一致性核对  [type=test]
- **涉及目录**: `docs/`, 仓库根
- **涉及文件**: 同 Task 15
- **描述**: 核对 as-built 不再声称 `ToolResult` 整批保序，debug 不再声称 opt-in 或全启动路径
  “零路径解析”，并发安全集合为 `subagent` + `read/find/ls/grep`，runner 前置错误不伪造
  diagnostic，摘要主体上限不被写成最终 summary 硬上限；AGENTS 不再写“默认关闭”；
  TODO 不再保留 debug sweep 待办；help 单一尾换行有明确落点。
- **验证方法**:
  - `rg -n "ToolResult|buffer_unordered|默认开启|YUSHAN_DEBUG|7 天|cancelled|ToolError::Execution|MAX_SUMMARY_BYTES|只有 .*subagent|零路径解析|HELP_TEXT|尾换行|debug sweep" docs/architecture.md docs/design-final/coding-agent-tui.md docs/TODO.md docs/arch/agent-observability/design.md AGENTS.md`
  - 期望：事件顺序、开关默认值、保留策略与 help 字节形状描述一致；
    `docs/TODO.md` 无已完成的 debug sweep 条目。
- **关联 impl**: Task 15
- **依赖**: Task 15

## 波次计划（Phase 3 计算后回填）

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | Task 1, 2 | `apps/coding-agent/src/subagent.rs` | 无 | W0 | deep |
| U2 | Task 3, 4 | `crates/ys-loop/src/basic.rs` | 无 | W0 | deep |
| U3 | Task 5, 6 | `crates/ys-tui-coding/src/draw.rs` | 无 | W0 | shallow |
| U4 | Task 7, 8 | `apps/coding-agent/src/prompt.rs` | 无 | W0 | shallow |
| U5a | Task 9, 11 + Task 10 内部测试 | `debug_log.rs`, `logging.rs`, `main.rs`, `app_loop.rs`, `help.rs` | 无 | W0 | deep |
| U5b1 | Task 10 集成测试第 1 组 + Task 12 | `budget_exit.rs`, `debug_log.rs`, `degraded_search.rs`, `help_cli.rs`, `worktree_cli.rs` | U5a | W1 | deep |
| U5b2 | Task 10 集成测试第 2 组 | `resume_cli.rs`, `session_cli.rs`, `skills_cli.rs`, `stream_usage_fallback.rs`, `truncation_guard.rs` | U5a | W1 | shallow |
| U7 | Task 13, 14 | ADR 与 observability 文档 | U1, U2, U4, U5a | W1 | shallow |
| U8 | Task 15, 16 | as-built、架构、TODO、AGENTS 文档 | U3, U7 | W2 | shallow |

## 验证清单

- [x] Task 1–16 的 impl/test 配对全部由 review 放行。
- [x] `cargo test -p ys-coding-agent`
- [x] `cargo test -p ys-loop`
- [x] `cargo test -p ys-tui-coding`
- [x] `cargo test -p ys-coding-agent --test budget_exit --test debug_log --test degraded_search
      --test help_cli --test resume_cli --test session_cli --test skills_cli
      --test stream_usage_fallback --test truncation_guard --test worktree_cli`
- [x] `cargo test --workspace`
- [x] `cargo clippy --all-targets`
- [x] N1–N9 测试合同逐条留证；关键变异均确认先红后绿。
- [x] 无计划外文件改动；测试隔离与 env 恢复纪律已核对。
