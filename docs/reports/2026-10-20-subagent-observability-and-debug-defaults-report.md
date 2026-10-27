# Subagent 可观测性与默认 Debug 验收报告

- 执行计划：`docs/exec-plans/completed/2026-10-19-subagent-observability-and-debug-defaults.md`
- 设计计划：`docs/design-plans/completed/2026-10-19-subagent-observability-and-debug-defaults.md`
- 决策记录：`docs/adr/0044-subagent-observability-and-debug-defaults.md`
- 基线：`d5df3b76c12f20b86356d89e4f8522221b2ff3c7`
- 备份分支：`sw-backup/2026-10-19-subagent-observability-and-debug-defaults`
- 原始 HEAD：`dca5a8a`
- 验收日期：2026-10-20

## 结论

N1–N9 全部通过，执行计划 Task 1–16 的 impl/test 配对均已由 review 放行。
`cargo test --workspace` 全绿，`cargo clippy --all-targets` exit 0，仅保留仓库既有 warnings。
本轮无阻塞项。

## 变更摘要

最终提交相对基线的统计为 **31 files changed, +2620/-245**，包含新建的
`docs/adr/0044-subagent-observability-and-debug-defaults.md`、本报告，以及两份计划归档后的路径变化。

核心能力：

- Subagent 非完成结果生成有界、单行、可解释的诊断，区分父回合取消、子预算取消与未知原因。
- 子转录保留 `Text`、`ToolUse`、`ToolResult` 的完整字段；工具结果优先取事件流中的原始内容，
  不受 turn budget 对会话正文降级的影响。
- 并行工具批按完成顺序立即发送 `ToolResult`，父模型消息仍按原始 tool-call 顺序组装。
- TUI Summary 显示 stop reason 短标签，未知非穷尽变体显示 `unknown`。
- Subagent 调度提示改为依赖检查、完成后派发、写入范围不相交才并行、范围不明串行，
  失败或取消时先读诊断。
- Debug 日志未设置或空串时默认开启；默认日志目录清理超过 7 天的 `debug-*.jsonl`，
  `YUSHAN_LOG_DIR` Override 目录只写不清理。
- 正常启动只解析一次 `LogDir`，sweep、初始 sink 和会话切换共用同一结果。
- `--help` 三种触发形态逐字节一致，stdout 恰以一个 LF 结尾。

## 波次与 Review

| 单元 | 任务 | review 档 | 轮次 | 结果 |
|---|---|---|---|---|
| U1 | Task 1, 2 | deep | 2 | 实现后补齐真实 runner turn-budget P1，复审通过 |
| U2 | Task 3, 4 | deep | 2 | 完成 P2 测试加固后复审通过 |
| U3 + U4 | Task 5–8 | shallow | 1 | 首轮通过 |
| U5a | Task 9, 11 + Task 10 内部测试 | deep | 2 | 修复日志目录二次解析 P1 后通过，另补 P2 测试 |
| U5b1 | Task 10 第一组集成测试 + Task 12 | deep | 2 | 补 P2 字节与 HOME 断言后通过 |
| U5b2 | Task 10 第二组集成测试 | shallow | 1 | 首轮通过 |
| U7 | Task 13, 14 | shallow | 2 | 修复日志目录单次解析的文档/实现 P1 后通过 |
| U8 | Task 15, 16 | shallow | 2 | 修复 4 个文档与实现一致性 P1 后通过 |

## 测试证据

| 命令或范围 | 结果 |
|---|---|
| `cargo test --workspace` | 全绿 |
| `cargo test -p ys-coding-agent --bin ys-coding-agent` | 395 passed |
| `cargo test -p ys-loop` | 87 passed |
| `cargo test -p ys-tui-coding` | 323 passed, 1 ignored |
| `cargo test -p ys-coding-agent --test debug_log` | 8 passed, 1 ignored |
| `cargo test -p ys-coding-agent --test help_cli` | 7 passed |
| `cargo test -p ys-coding-agent --test worktree_cli` | 17 passed |
| `cargo test -p ys-coding-agent --test budget_exit` | 4 passed |
| `cargo test -p ys-coding-agent --test degraded_search` | 2 passed |
| `cargo test -p ys-coding-agent --test resume_cli` | 1 passed |
| `cargo test -p ys-coding-agent --test session_cli` | 9 passed |
| `cargo test -p ys-coding-agent --test skills_cli` | 3 passed |
| `cargo test -p ys-coding-agent --test stream_usage_fallback` | 4 passed |
| `cargo test -p ys-coding-agent --test truncation_guard` | 2 passed |
| `cargo clippy --all-targets` | exit 0，仅既有 warnings |

## N1–N9 验收

| 项 | 结果 | 证据 |
|---|---|---|
| N1 Subagent 失败诊断可解释 | 通过 | `cargo test -p ys-coding-agent subagent` 覆盖诊断首行、`stop_reason`、`elapsed_ms`、三种 `cause`、父取消优先、非取消不写 cause、UTF-8 安全清洗与 1024 字节上限 |
| N2 子转录保留完整轨迹 | 通过 | 同一测试集覆盖 `Text` / `ToolUse` / `ToolResult`、参数、原始结果、`is_error`、终局元数据和写失败诊断 |
| N3 父回合继续或停止 | 通过 | `error_tool_result_continues_parent_turn` 与 `parent_boundary_pre_cancel_makes_zero_model_calls` 均通过 |
| N4 完成即发结果、回喂仍保序 | 通过 | `parallel_results_follow_completion_while_session_keeps_call_order`、并发错误路径与 `.max(1)` 用例通过 |
| N5 通用调度提示 | 通过 | `cargo test -p ys-coding-agent prompt` 27 passed；父/子提示边界、旧规则删除、并发槽位与成本提醒均有断言 |
| N6 Debug 默认开关 | 通过 | 未设置/空串默认开启、显式关闭覆盖 `--debug`、无 flag 的 `-p` / `--json` 生成完整 debug 文件；集成测试 helper 统一隔离环境变量 |
| N7 清理与早退边界 | 通过 | 超龄文件删除，近期文件、非匹配文件、目录、符号链接与 `yushan.log` 保留；Override 不清理；`YUSHAN_DEBUG=0` 仍 sweep；help 与 worktree 早退不建 sink |
| N8 `--help` 单一尾换行 | 通过 | `help_cli` 断言三种形态逐字节一致，结尾必须为单个 `\n`，拒绝 CRLF 和双 LF |
| N9 文档与契约同步 | 通过 | ADR-0044、ADR-0022、ADR-0029、observability 文档、architecture、as-built、TODO 与 AGENTS 的现行描述一致 |

关键变异验证：临时让 `CollectingSink` 的原始 ToolResult map 返回空后，
`app_runner_turn_budget_elides_session_but_transcript_keeps_raw_result` 会因转录出现降级标记且失去原始结果而变红；
恢复实现后该用例通过。这说明预算降级与转录原始结果之间的回归合同确实被测试锁定。

## 关键决策与不变式

- 诊断固定以 `[diagnostic:` 开头，控制字符替换为空格，最终整条诊断不超过 1024 字节；
  截断按 UTF-8 边界进行，并以 ASCII `...` 收尾。
- 子 runner 使用本地 `CollectingSink` 捕获事件。转录中的 ToolResult 正文优先取事件原文，
  会话正文只作为缺失时的回退。
- 并行工具使用 `buffer_unordered(max_parallel_tools.max(1))`，每个 future 携带原始 index。
  `ToolResult` 完成即发，写回数组使用原始 index，父模型消息仍按原始 tool-call 顺序组装。
- 默认日志目录只清理超过 7 天、名称匹配 `debug-*.jsonl` 的普通文件，不跟随符号链接；
  `YUSHAN_LOG_DIR` Override 目录不清理。
- 关闭 debug 只关闭 sink，不创建文件、writer 或线程；正常启动仍在 help/worktree 早退之后
  解析一次 `LogDir` 并执行 Default sweep。
- `MAX_SUMMARY_BYTES = 8 KiB` 只约束摘要文本主体；截断标记与随后追加的 `## Files`
  可使最终 summary 略超 8 KiB。

## Review 后修复

- 将新 ADR 编号从已被占用的 0043 改为 0044，并同步计划与文档引用。
- U1 补真实 `AppSubagentRunner::run` 与极小 turn budget 回归，验证第三次模型请求已被降级，
  落盘转录仍保留原始 ToolResult；另补 Text 原文、真实 watchdog `child-budget`、
  父取消与子预算同时到点、写失败摘要和重复 tool-call id 覆盖。
- U2 删除受机器负载影响的耗时断言，补 ToolResult id/content 配对断言，
  并新增 `buffer_unordered` 路径下错误结果继续父回合的用例。
- U5a 修复 sweep、初始 sink 与后续会话切换分别解析日志目录的问题；新增修改环境变量后
  仍使用首次解析结果的测试。另补超龄同名目录与不可读权限测试。
- U5b1 补 help 的单 LF、禁 CRLF 断言，以及 help/worktree 早退时显式日志目录和
  `$HOME/.yushan/logs` 都不产生 debug 文件的断言。
- U7 修复文档声称“一次解析 path + source”但实现仍二次读取环境变量的问题，
  使 sweep、写入和会话切换共用同一 `LogDir`。
- U8 修正 4 个文档与实现不一致点：并发安全集合为 `subagent` + `read/find/ls/grep`；
  debug 关闭不等于全启动路径零解析；前置 runner `Err` 不生成 diagnostic；
  8 KiB 只约束摘要文本主体。

## 剩余风险

- 默认 debug 会增加磁盘写入，由单文件 64 MiB 上限、默认目录 7 天清理和
  `YUSHAN_DEBUG=0` 显式关闭控制。
- 仓库既有 clippy warnings 不在本轮处理范围内。
