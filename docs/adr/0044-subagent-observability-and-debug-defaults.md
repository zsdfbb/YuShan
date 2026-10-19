# 0044 — Subagent 可观测性、并行工具结果时序与 debug 默认保留

本 ADR 固定 subagent 失败现场、并行工具事件时序和 debug 日志保留的现行契约：
子 agent 的失败可归因并可事后复核；并行工具完成一个结果就发一个，同时父模型收到的工具结果
仍按调用顺序排列；debug 日志默认写入，并只对默认目录执行有限期清理。

## Status

accepted（2026-10-20）

## 背景

- subagent 以非 `Completed` 结束时，`StopReason` 只说明结果类型，不能说明是父回合取消、
  子预算到点还是其他失败；provider 或运行时错误还需要以单行、无控制字符且长度有界的形式
  回给父模型。
- `MemorySession` 不是子工具轨迹的权威来源：turn budget 会把较早的 `ToolResult` 改写为降级
  文本，事后读取会话无法恢复原始工具结果。
- 并行工具若等整批完成再发 `ToolResult`，先完成的任务不能及时被 TUI、`--json` 和 debug 观察；
  但父模型协议仍要求同一条消息里的工具结果与原始 `ToolCall` 顺序配对。
- debug 日志是排障现场，正常运行需要在不预设环境变量时留下证据；默认目录必须有保留期，
  否则会持续增长。`YUSHAN_LOG_DIR` 覆盖目录属于调用方，不能由运行时单方面清理。

## 决策

### 1. 取消来源与单行有界诊断

`AppSubagentRunner` 内部区分三种取消来源，按以下优先级判定：

1. `parent-turn-cancelled`：父边界已 abort；
2. `child-budget`：父边界未取消，但子 watchdog 已向子边界源写入 `Abort`；
3. `unknown`：`stop_reason == Cancelled`，但两个边界源都不能给出归因。

`cause` 只在 `stop_reason == Cancelled` 时出现。只有子回合已经启动、`child.run_turn`
返回 `Err` 的路径才构造 `SubagentOutcome` 并写 `stop_reason=cancelled cause=unknown`；
真实错误文本经清洗后写入同一诊断，不伪装成父取消或子预算。无可用模型、派发次数超限、
`AgentBuilder` 组装失败等子回合开始前的错误直接从 runner 返回 `Err`，由 `SubagentTool`
映射为 `ToolError::Execution`，没有 diagnostic 前缀。

诊断从 runner 入口开始计时，使用整数 `elapsed_ms`，固定以 `[diagnostic: ` 开头。
整条诊断最多 `1024` 字节，包含前缀和截断标记；CR、LF 及其他控制字符在截断前替换为
一个 ASCII 空格，超限时在 UTF-8 字符边界截断并追加 ASCII `...`。因此诊断保持物理单行、
合法 UTF-8 且长度有界。

`SubagentTool` 的结果正文把诊断放在第一行，随后是摘要和转录路径；没有诊断时摘要在前。
`SubagentOutcome` 与 `SubagentRunner` 的公共签名不变。

### 2. 基于事件原文的完整子转录

子 runner 使用本地 `CollectingSink` 捕获本次回合事件。事件只用于本地转录，不进入父事件流。
从 `AgentEvent::ToolResult { id, result }` 建立按 `tool_call_id` 索引的原始结果表；同一 id
后到的结果覆盖先到的结果。

转录仍按子会话的 `ContentBlock` 顺序遍历：

- `Text` 保留原文；
- `ToolUse` 保留 `id`、工具名和参数 JSON；
- `ToolResult` 保留 `tool_call_id` 与 `is_error`，正文优先取事件结果表中的原文，只有事件缺失
  时才回退到会话正文。

转录末尾写 `stop_reason`、取消来源、`elapsed_ms` 和诊断。`write_transcript` 失败不 panic，
也不丢弃已有摘要；错误文本并入诊断。摘要仍只取文本并保持 `## Files` 保证逻辑；
`MAX_SUMMARY_BYTES = 8 KiB` 约束摘要文本主体，截断标记与随后追加的 `## Files` 可使最终
`summary` 略超 8 KiB。

### 3. 并行结果完成即发，模型消息保持调用顺序

并行工具批先按原始顺序发出全部 `ToolCall`。执行阶段使用
`buffer_unordered(max_parallel_tools.max(1))`，每个 future 携带原始 index；任一工具完成后
立即发出对应 `ToolResult`，并写回 `outcomes[original_index]`。

父模型回喂的单条 `Role::User` 消息在 Step 9c 按原始 `tool_call` 顺序组装。事件顺序与模型消息
顺序是两个独立契约：消费者按完成顺序看到结果，模型按调用顺序收到结果。

`max_parallel_tools = 0` 钳为并发 1。仅当批次中每个工具都声明
`is_concurrency_safe() == true` 时走并行路径；混合批仍走原有串行路径。TUI 按 `ToolCallId`
回填，不依赖 `ToolResult` 事件顺序。当前安全集合是 `subagent` 与只读工具
`read` / `find` / `ls` / `grep`；`bash` / `write` / `edit` 等不入并行准入。

### 4. debug 默认开启与默认目录保留

`YUSHAN_DEBUG` 未设置或为空串时 debug 开启。非空值去除首尾空白并忽略大小写后，
`1` / `true` / `yes` / `on` 开启，其余非空值关闭。环境变量优先于 `--debug`；`--debug`
仍被命令行解析器接受以兼容脚本，但不是创建 sink 的必要条件。

TUI、`-p`、`--json` 和 `worktree attach` 的正常启动路径共享同一 `open_enabled` 决策。
`help` 与进程级 `worktree` 子命令在 sink 创建前早退。
关闭 debug 只使 `open_enabled` 返回 `None`，不创建 debug 文件、writer 线程或 sink；
正常启动无论开关值如何，都在早退之后解析一次日志目录并执行默认目录 sweep。

日志目录通过一次解析得到 `path + source`，来源只有 `Default` 与 `Override`：

- `YUSHAN_LOG_DIR` 非空为 `Override`：照常写入，不执行清理；
- 未设置或为空串为 `Default`：使用 `~/.yushan/logs`，并可在正常启动时清理。

默认目录中，修改时间超过 7 天的普通文件且文件名匹配 `debug-*.jsonl` 才会被删除。
判断使用 `DirEntry::file_type().is_file()`，不跟随符号链接；同名目录、符号链接、近期文件、
非匹配文件和 `yushan.log` 都保留。清理是一次启动期 best-effort 动作，任何读取或删除失败
都静默跳过。

清理独立于 `open_enabled`：`YUSHAN_DEBUG=0` 的正常启动仍清理 Default 目录。
单文件 64 MiB 上限、默认 `head` 参数摘要和脱敏规则不变。

## Considered Options

- **只按 `StopReason::Cancelled` 报告取消**（否决）：无法区分父取消、子预算和未知来源，
  父模型无法据此调整重试范围。
- **从 `MemorySession` 反推完整工具轨迹**（否决）：turn budget 会替换较早的结果正文，
  转录会把降级文本当作原始结果。
- **用 `buffered` 等齐整批后按原序发出 `ToolResult`**（否决）：事件顺序稳定，但先完成的结果
  被延迟，实时观察和首结果反馈消失。该路径保留为历史否决方案，不作为现行契约。
- **仅以 `--debug` 作为开启条件且默认目录不清理**（否决）：常规运行缺少现场，默认目录无界增长。
- **清理 `YUSHAN_LOG_DIR` 覆盖目录**（否决）：覆盖目录归调用方所有，运行时不能擅自删除。

## Consequences

- 公共 Rust 类型、`SubagentOutcome` 和 `SubagentRunner` 签名不变；子转录格式增加工具轨迹与
  终局元数据。
- `AgentEvent::ToolResult` 在并行批中按完成顺序出现，`--json` 消费者会观察到新的时序；
  父模型消息仍保持原始调用顺序。
- debug 默认产生磁盘写入；默认目录由 7 天起始清理、单文件 64 MiB 上限和显式关闭开关约束。
- 子回合成败产生的失败诊断总长有界且保持单行；runner 前置错误走普通 `ToolError::Execution`，
  不产生 diagnostic 前缀。
- 通用 subagent 调度 guideline 要求派发前检查依赖与写入范围、范围不明时串行，并在失败或取消后
  先读诊断再缩窄重试或报告；不引入运行时依赖图，也不改变并发槽位与成本提醒。

## 参考

- [ADR-0022](./0022-subagent-dispatch.md) — subagent 派发形态
- [ADR-0029](./0029-subagent-landing-decisions.md) — subagent 落地裁定
- [`adr-debug-log.md`](../arch/agent-observability/adr-debug-log.md) — debug 日志结构
- `apps/coding-agent/src/subagent.rs` — 诊断、原始事件结果与转录
- `crates/ys-loop/src/basic.rs` — 并行工具完成即发与消息保序
- `apps/coding-agent/src/debug_log.rs` — 默认开关与保留清理
- `apps/coding-agent/src/logging.rs` — 日志目录来源解析
