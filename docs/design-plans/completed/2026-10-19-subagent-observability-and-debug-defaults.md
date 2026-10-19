# Subagent 可观测性与默认 Debug 设计计划

## 背景

- 会话 `1792258864_767825000` 中，subagent 以 `Cancelled` 结束后只留下不醒目的 `✗`；父回合当时已被取消，没有机会继续推理。
- 子转录只保存文本，工具调用与工具结果全部丢失，失败现场无法复核。
- 并行 subagent 必须等整批结束才发出全部 `ToolResult`，先完成的任务也不会及时反馈。
- debug 需要在没有额外 flag 的正常运行中留下 JSONL 现场，否则关键故障缺少可聚合证据。
- 现有 subagent 调度提示只约束写入范围，没有表达任务依赖、验证依赖和失败后的处理。

## 设计

### 1. Subagent 失败与取消诊断

- 在 `apps/coding-agent/src/subagent.rs` 新增 crate 内部 `CancelCause`：
  - `ParentTurnCancelled`：父边界 `is_aborted()`。
  - `ChildBudget`：父边界未取消，但子边界源已由 watchdog 置位。
  - `Unknown`：`stop_reason == Cancelled`，但两个来源都不可判定。
- 分类优先级固定为父取消 > 子预算 > 未知。父取消是父 loop 的实际停止原因，即使子预算也同时到点，也先报告父取消。
- 从 `AppSubagentRunner::run` 入口开始计时；诊断使用整数 `elapsed_ms`，避免测试依赖格式化浮点数。
- 诊断由 crate 内部同一清洗函数生成，固定以 `[diagnostic:` 开头，并受
  `const MAX_DIAGNOSTIC_BYTES: usize = 1024` 约束：
  - `MAX_DIAGNOSTIC_BYTES` 约束最终整条 diagnostic 文本；超限时按 UTF-8 字符边界截断，
    预留并追加 ASCII `...`，保证结果仍是合法 UTF-8 且长度不超过上限。
  - CR、LF 及其他控制字符在截断前统一替换为一个 ASCII 空格，从而保证物理单行；
    替换后连续空格无需额外折叠。
  - `cause` 只在 `stop_reason == Cancelled` 时出现。父取消、子预算、未知三值及优先级仍按上文。
  - 只有子回合已经启动、`child.run_turn` 返回 `Err` 的路径才生成 `SubagentOutcome`，
    其错误文本进入 diagnostic 前走同一清洗与截断；该诊断固定写
    `stop_reason=cancelled cause=unknown`，由 error 文本说明真实失败，不伪装成父取消或子预算。
    无可用模型、派发次数超限、`AgentBuilder` 组装失败等前置错误直接从 runner 返回 `Err`，
    `SubagentTool` 映射为 `ToolError::Execution`，不生成 diagnostic 前缀。

  ```text
  [diagnostic: stop_reason=cancelled cause=parent-turn-cancelled elapsed_ms=1234]
  ```

- 其他非完成原因写 `stop_reason` 与耗时，不写 `cause`。
- `SubagentTool::call` 重排结果正文：有诊断时诊断第一行，随后是摘要和转录路径；无诊断时保持摘要在前。
- `SubagentOutcome` 与 `SubagentRunner` 的公共字段/签名不变。
- 父侧语义保持不变并加测试锁定：子结果是非完成时为 `is_error`，但父回合只有在父边界已取消时才停止；未取消时把错误结果回喂模型并继续下一轮。不自动重试。

### 2. 完整子转录

- 子 runner 的 event sink 使用 `ys_event::CollectingSink` 捕获本次回合事件。
  不得在回合结束后只从 `MemorySession` 反推完整工具轨迹：turn budget 可能已把较早的
  `ToolResult` 替换为降级文本，事后读会话无法恢复原始结果。
- 从收集到的 `AgentEvent::ToolResult { id, result }` 建立按 `tool_call_id` 索引的原始结果表；
  同名 id 后到值覆盖先到值，与真正回喂模型的结果一致。
- 构建转录时仍按会话内容块顺序遍历：
  - `Text` 保留原文。
  - `ToolUse` 保留 `id`、`name`、参数 JSON。
  - `ToolResult` 保留会话中的 `tool_call_id` 与 `is_error`，但结果正文优先取原始结果表中
    同 id 的值；仅当事件表缺失该 id 时才回退到会话块正文。
- 这样 `Text` / `ToolUse` 的顺序与边界仍由会话决定，被预算降级的工具结果不会污染诊断转录。
- 末尾追加终局元数据：`stop_reason`、取消来源、`elapsed_ms`、诊断。
- `write_transcript` 失败时仍返回可继续使用的 `SubagentOutcome`；错误文本并入诊断，不 panic、不丢弃已有摘要。
- 摘要选取与 `## Files` 保证逻辑不变；摘要仍只取文本。`MAX_SUMMARY_BYTES = 8 KiB`
  约束摘要文本主体，截断标记与随后追加的 `## Files` 节可使最终 summary 略超 8 KiB。

### 3. 并行工具结果即时可见

- `crates/ys-loop/src/basic.rs` 的并行路径继续先按原序发出全部 `ToolCall`。
- 执行阶段改为 `enumerate + buffer_unordered(max_parallel_tools.max(1))`：每个 future
  携带原始 index，完成后立即发对应 `ToolResult`，同时写入 `outcomes[original_index]`。
  必须保留 `.max(1)` 防护，不能让配置值 0 产生永不推进的并行流。
- 父模型回喂的单条 `Role::User` 消息仍在 Step 9c 按原始 tool-call 顺序组装；事件到达顺序与模型消息顺序明确分离。
- 保留全部既有闸门：仅全部工具 `is_concurrency_safe()` 才并行、混合批退回串行、`max_parallel_tools` 限流、TUI 按 `ToolCallId` 回填。当前安全集合是 `subagent` 与只读工具 `read` / `find` / `ls` / `grep`。
- 测试用可控完成顺序（`oneshot` / `Notify`，或至少明显分离的延迟）制造“快者先完成”；
  不依赖两个相近 `sleep` 的调度巧合。旧测试中硬编码 `R1,R2,...` 的断言改为分别验证：
  全部 `ToolCall` 先于任何 `ToolResult`、快者结果先发、会话结果仍按原始调用顺序。

### 4. TUI 回合摘要

- `Summary` 行增加可读 stop reason，例如：

  ```text
  ✗ cancelled · 2 rounds · 1.3s
  ```

- 映射为 `completed` / `cancelled` / `max rounds` / `truncated` / `no output` / `unknown`；保留现有符号，不改变 `TranscriptLine` 数据形状。
- 现有 `test_summary_symbols_per_stop_reason` 的逐字旧格式断言必须同步改为包含 label；
  窄宽度测试使用 `width >= MIN_WIDTH`，否则 `ui()` 会直接返回空帧而使断言失效。
- `StopReason` 是 `#[non_exhaustive]`，下游 crate 无法构造未来变体；`unknown` 由映射的
  通配分支保证，可用编译期兜底说明覆盖边界，但不要求伪造不可构造的变体。

### 5. 通用调度规则

- 在 `apps/coding-agent/src/prompt.rs` 把“并行写目标不相交”升级为两条通用 guideline，不硬编码 `review` / `test`：
  - 派发多个 subagent 前检查依赖与写入范围；消费另一任务输出或验证其修改的任务必须等前置完成；只有输出独立且写入范围不相交才并行；范围不明则串行。
  - subagent 返回错误或取消后，先读诊断，再缩窄重试或向用户报告。
- 不引入运行时依赖图、不强制全局串行，也不改变并发槽位与成本提醒。
- 替换后必须明确删除旧窄写入规则，同时保留并发槽位数与成本提醒；测试不能只断言新规则存在。
- 这些 guideline 属于默认内置提示。命中自定义 `.yushan/SYSTEM.md` 时仍整体短路，
  维持既有自定义 prompt 契约。

### 6. Debug 默认开启与保留

- `YUSHAN_DEBUG` 未设置或为空串时默认开启；非空值仍按 `1/true/yes/on` 开启，其余非空值关闭，并优先于 `--debug`。
- `--debug` 参数继续被识别以兼容脚本，但不再是开启日志的必要条件。
- TUI、`-p`、`--json`、`worktree attach` 的正常启动路径共享同一 `open_enabled` 决策，因此都默认建 sink；`help` 与进程级 worktree 子命令在 sink 创建前早退。
- `logging` 一次解析日志目录为 `path + source`，来源只有 `Default` / `Override`；
  写入与 sweep 共用这一结果，禁止在两个调用点分别重读环境变量产生分歧。
- `YUSHAN_LOG_DIR` 非空为 `Override`：照常写入，但不执行清理；空串等同于未设置，
  回落默认目录并标为 `Default`。
- 默认日志目录 `~/.yushan/logs` 的正常启动路径清理超过 7 天的 `debug-*.jsonl`。
  清理独立于 `open_enabled`：即便 `YUSHAN_DEBUG=0` 关闭写入，正常启动仍 sweep `Default`。
- sweep 只删除文件名匹配 `debug-*.jsonl` 且 `DirEntry::file_type().is_file()` 为真的项；
  不跟随符号链接，保留同名目录、符号链接、近期文件、非匹配文件与 `yushan.log`。
- 清理是一次启动期 best-effort 动作，不进入 `open_enabled` 或每回合路径；单文件 64 MiB、默认 `head` 摘要、脱敏规则全部不变。

### 7. `--help` 换行

- `HELP_TEXT` 去掉末尾自带换行，保留 `println!` 打印，使 stdout 恰以一个 `\n` 结尾。
- help 文案同步说明 debug 默认开启及 `YUSHAN_DEBUG=0` 关闭方式。

## 接口与行为变更

- 公共 Rust 类型无新增字段、无新增公共 trait、无新依赖。
- `AgentEvent::ToolResult` 在并行批中的时序从“整批结束后按原序”改为“按完成顺序即时”；事件变体不变。
- 子转录是本地诊断产物，格式升级为包含工具轨迹与终局元数据。
- debug 开关默认值、7 天清理、TUI Summary 文本和 help stdout 字节形状是用户可见行为变化。

## 涉及文件

- 修改运行时代码：
  - `apps/coding-agent/src/subagent.rs`
  - `crates/ys-loop/src/basic.rs`
  - `crates/ys-tui-coding/src/draw.rs`
  - `apps/coding-agent/src/prompt.rs`
  - `apps/coding-agent/src/debug_log.rs`
  - `apps/coding-agent/src/logging.rs`
  - `apps/coding-agent/src/main.rs`
  - `apps/coding-agent/src/app_loop.rs`（测试）
  - `apps/coding-agent/src/help.rs`
- 修改测试：
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
- `apps/coding-agent/tests/e2e_bench.rs` 仅在它的启动 helper 需要纳入统一 env 隔离时修改
- 新增文档：`docs/adr/0044-subagent-observability-and-debug-defaults.md`
  （ADR-0043 已被 resume 修复占用，本计划不改 0043）
- 修改文档：
  - `docs/adr/0022-subagent-dispatch.md`
  - `docs/adr/0029-subagent-landing-decisions.md`
  - `docs/arch/agent-observability/adr-debug-log.md`
  - `docs/arch/agent-observability/design.md`
  - `docs/architecture.md`
  - `docs/design-final/coding-agent-tui.md`
  - `docs/TODO.md`
  - `AGENTS.md`

## 测试策略

### 测试合同（Phase 3 审查依据）

#### N1: Subagent 失败诊断可解释
- **验收方法**: `cargo test -p ys-coding-agent subagent` — 断言非完成结果首行以 `[diagnostic:` 开头，包含 `stop_reason`、`elapsed_ms`；`Cancelled` 包含 `cause`，父取消、子预算、未知三值均有覆盖，父取消优先级有断言；非 `Cancelled` 不写 `cause`；子回合启动后的 `run_turn Err` 为 `stop_reason=cancelled cause=unknown` 且 error 文本说明真实失败；无模型、派发超限、`AgentBuilder` 失败等前置 `Err` 直接映射为 `ToolError::Execution`，没有 diagnostic 前缀。
- **清洗合同**: CR/LF/其他控制字符变空格；含多字节字符的边界输入按 UTF-8 安全截断；最终文本不超过 `MAX_DIAGNOSTIC_BYTES=1024`，超限以 ASCII `...` 结尾，且前缀仍位于开头。
- **优先级**: 必测
- **验收人**: review subagent

#### N2: 子转录保留完整轨迹
- **验收方法**: `cargo test -p ys-coding-agent subagent` — 子 runner 用 `CollectingSink` 捕获事件；构造含 `Text` / `ToolUse` / `ToolResult` 的子会话，断言 id、工具名、参数、原始结果、`is_error`、终局元数据均落盘；不可写目录时诊断含 `transcript` 写入错误。
- **预算回归**: 构造超过 `max_turn_tool_result_tokens` 的较早结果，确认 `MemorySession` 已降级后，转录仍按 `tool_call_id` 写回事件中的原始 `ToolResult`。
- **优先级**: 必测
- **验收人**: review subagent

#### N3: 父回合继续或停止
- **验收方法**: `cargo test -p ys-loop` — 非取消回合收到 `is_error` 工具结果后发生第二次模型调用并正常完成；父边界预先取消时零模型调用并以 `Cancelled` 结束。
- **优先级**: 必测
- **验收人**: review subagent

#### N4: 完成即发结果、回喂仍保序
- **验收方法**: `cargo test -p ys-loop` — 用 `oneshot` / `Notify` 或明显分离的延迟控制完成顺序，断言全部 `ToolCall` 先于任何 `ToolResult`、`R(fast)` 在 `R(slow)` 前，而会话中 tool result id 仍按原 tool-call 顺序；既有串行、混合批、`max_parallel_tools.max(1)` 与并发上限测试全绿。
- **优先级**: 必测
- **验收人**: review subagent

#### N5: 通用调度提示
- **验收方法**: 新增测试名必须包含 `prompt`，用 `cargo test -p ys-coding-agent prompt` 执行；父提示包含依赖检查、完成后派发、写入范围不相交、范围不明串行、失败先读诊断；子提示不含这些派发 guideline；只对 subagent guideline 段断言不出现 `review` / `test` 特例，不扫描完整父提示；同时保留并发槽位与成本提醒，并断言旧窄写入规则已删除。
- **优先级**: 必测
- **验收人**: review subagent

#### N6: Debug 默认开关
- **验收方法**: `cargo test -p ys-coding-agent debug_enabled` 与 `cargo test -p ys-coding-agent --test debug_log` — 未设置/空串默认开；显式关闭覆盖 `--debug`；无 flag 的 `-p` / `--json` 子进程生成 `run_end` 完整的 debug 文件；`app_loop` 等内部 debug-off 用例显式 `YUSHAN_DEBUG=0`。
- **进程隔离**: 所有启动二进制的测试 helper 先 `env_remove("YUSHAN_LOG_DIR")`，再按用途显式设置/移除 `YUSHAN_DEBUG`；默认开用例必须显式提供临时日志目录，避免继承真实目录。
- **优先级**: 必测
- **验收人**: review subagent

#### N7: 清理与早退边界
- **验收方法**: `cargo test -p ys-coding-agent debug_log` / `logging`，以及 `cargo test -p ys-coding-agent --test help_cli --test worktree_cli` — 超龄 debug 文件被删；近期文件、非匹配文件、同名目录、符号链接、`yushan.log` 保留；Override 不清理；`YUSHAN_DEBUG=0` 的正常启动仍清理 Default；help 与进程级 worktree 不建 sink。
- **优先级**: 必测
- **验收人**: review subagent

#### N8: `--help` 单一尾换行
- **验收方法**: `cargo test -p ys-coding-agent --test help_cli` — stdout 以 `\n` 结尾且不以 `\n\n` 结尾；三种 help 触发形态仍逐字节一致。
- **优先级**: 必测
- **验收人**: review subagent

#### N9: 文档与契约同步
- **验收方法**: `rg -n "buffer_unordered|按完成顺序|默认开启|7 天|parent-turn-cancelled|ToolError::Execution|MAX_SUMMARY_BYTES|HELP_TEXT"` 指定的 ADR/as-built/AGENTS/TODO 文件 — 事件顺序、debug 默认值、清理规则、帮助单一尾换行、并发安全集合、runner 错误分界与摘要上限均有唯一、一致描述，且 `docs/TODO.md` 不再保留已完成的 debug sweep 待办。
- **优先级**: 必测
- **验收人**: review subagent

### 测试框架与命令

- `cargo test -p ys-coding-agent`
- `cargo test -p ys-loop`
- `cargo test -p ys-tui-coding`
- `cargo test -p ys-coding-agent --test budget_exit --test debug_log --test degraded_search
  --test help_cli --test resume_cli --test session_cli --test skills_cli
  --test stream_usage_fallback --test truncation_guard --test worktree_cli`
- `cargo test --workspace`
- `cargo clippy --all-targets`

### 变异验证

| 变异 | 应变红 |
|---|---|
| 把诊断放回摘要之后 | N1 首行断言 |
| 父取消与子预算同时命中时选子预算 | N1 优先级断言 |
| 让 CR/LF 原样穿过或截断切坏 UTF-8 | N1 单行与边界断言 |
| 把子回合启动后的 `run_turn Err` cause 写成父取消/子预算 | N1 的 `cause=unknown` 断言 |
| 给前置 runner `Err` 伪造 diagnostic | N1 的 `ToolError::Execution` 断言 |
| 转录忽略 `ToolUse` / `ToolResult` | N2 |
| 转录只读已被 turn budget 降级的 `MemorySession` | N2 超预算原始结果回归 |
| `buffer_unordered` 后仍等全部完成再 emit | N4 完成顺序 |
| 收结果时改按完成顺序组装模型消息 | N4 会话 id 顺序 |
| 把 `.max(1)` 去掉并传入 0 | N4 并发下限回归 |
| 未设置 env 时回落到 `cli_flag` | N6 默认开启 |
| 对所有 `YUSHAN_LOG_DIR` 都清理 | N7 Override 断言 |
| sweep 跟随符号链接或删同名目录 | N7 文件类型断言 |
| 恢复 `println!` + 文本自带尾换行 | N8 |

## 波及文档

- 新增 ADR-0044，记录失败诊断、转录、并行事件时序和 debug 默认保留策略
  （ADR-0043 已被 resume 修复占用）。
- 更新 ADR-0022 / 0029，修正“`buffered` 保序发结果”和“仅靠写目标不相交”的旧表述。
- 更新 observability ADR 与 `docs/arch/agent-observability/design.md`；
  同步 `docs/architecture.md`、`docs/design-final/coding-agent-tui.md`、`docs/TODO.md`
  与 `AGENTS.md` 的 as-built/环境变量说明。`docs/TODO.md` 删除 A1 中已完成的 P2 debug sweep 行。
- `docs/design-final/coding-agent-tui.md` 增加明确的 CLI help as-built 段落，记录 stdout
  `HELP_TEXT` 与 `println!` 合计只有一个尾换行。

## 风险与注意事项

- `--json` 消费者会看到并行 `ToolResult` 的新时序；模型请求内容仍确定，记录变更到 ADR/as-built。
- debug 默认开启会增加磁盘写入；用 64 MiB 上限、7 天保留、`YUSHAN_DEBUG=0` 反向开关控制。
- 清理只看 Default 目录中的普通 `debug-*.jsonl` 与 mtime，必须 best-effort，失败不得影响启动。
- 所有进程级 env 测试使用 `crate::test_env::env_lock()` + `EnvRestore`；临时目录名包含进程内原子序号与 `process::id()`。
- 现有假定“未设置即关闭”的测试必须显式设置 `YUSHAN_DEBUG=0`，包括
  `apps/coding-agent/src/app_loop.rs` 的 `debug_off_tui_creates_no_files` 等内部用例。
- 所有启动 coding-agent 二进制的集成测试 helper 必须显式 `env_remove("YUSHAN_LOG_DIR")`，
  防止默认开启后把 debug 文件写进开发者真实的 Override 目录；debug 用例再为临时目录显式设回。
- 涉及进程环境面的测试至少覆盖 `budget_exit.rs`、`debug_log.rs`、`degraded_search.rs`、
  `help_cli.rs`、`resume_cli.rs`、`session_cli.rs`、`skills_cli.rs`、
  `stream_usage_fallback.rs`、`truncation_guard.rs`、`worktree_cli.rs`；
  `e2e_bench.rs` 仅在其 helper 需要纳入统一隔离时处理。
- 不猜测具体按键：父取消、预算到点与未知来源只说事实，不把诊断写成未经验证的 Esc/Ctrl-C 归因。
