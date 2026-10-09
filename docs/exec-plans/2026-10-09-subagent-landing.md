# Subagent 派发落地 执行计划

> 每个 `type=impl` 配一个 `type=test`。字段：类型 / 依赖 / 涉及文件 / 描述 / 验证命令。
> 波次划分属 Phase 3（见 `docs/arch/subagent-rebaseline/design.md` §8）。

## T0 [impl] P0 前缀指纹测试（刀 0）
- 依赖: 无
- 涉及文件: `apps/coding-agent/tests/prefix_fingerprint.rs`（新增）
- 描述: 手写 FNV-1a 64（不引 `sha2`）对 `tools_json` 与 `system_prompt` 取指纹；锚父 9 工具与子表；加「同一子表构造两次序列化逐字节相同」断言。
- 验证: `cargo test -p ys-coding-agent --test prefix_fingerprint`

## T0t [test] 指纹变异自检
- 依赖: T0
- 涉及文件: `apps/coding-agent/tests/prefix_fingerprint.rs`
- 描述: 注入「交换工具顺序」使用例变红，证明指纹真在测漂移。
- 验证: `cargo test -p ys-coding-agent --test prefix_fingerprint`

## T1 [impl] per-tool 超时闸（刀 1）
- 依赖: 无
- 涉及文件: `crates/ys-tool/src/lib.rs`、`crates/ys-loop/src/basic.rs`
- 描述: `Tool::timeout() -> Option<Duration>` 默认 `None`；`basic.rs` 超时闸改优先级链「工具声明 → `bash_timeout` → 300s」；超时文案带工具名与秒数（保留 `timed out` 子串）。
- 验证: `cargo test -p ys-loop -p ys-tool`

## T1t [test] 超时优先级
- 依赖: T1
- 涉及文件: `crates/ys-loop/src/basic.rs`
- 描述: spy 工具声明 600s ⇒ 不被 300s 杀；默认 `None` 的工具 ⇒ 既有超时用例行为逐字节不变。
- 验证: `cargo test -p ys-loop`

## T2 [impl] 成本读数（刀 2）
- 依赖: 无
- 涉及文件: `crates/ys-core/src/usage.rs`、`adapters/model-openai-compatible/src/response.rs`、`adapters/model-openai-compatible/src/lib.rs`、`apps/coding-agent/src/config.rs`
- 描述: `Usage` 加 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`（`#[serde(default)]`，`Add` 同步相加）；`ChatUsage` 加同名字段并在流式/非流式映射；`Config::context_window_for(name)`（抽出三级查表）、`Config::build_model_for(name)`。
- 验证: `cargo test -p ys-core -p ys-model-openai-compatible -p ys-coding-agent`

## T2t [test] 读数与模型构造
- 依赖: T2
- 涉及文件: 同上 + 对应测试
- 描述: 适配器解析命中/未命中；`--json` 的 `RunFinished.usage` 多两字段且既有键不变；`context_window_for`/`build_model_for` 查表正确。
- 验证: `cargo test -p ys-core -p ys-model-openai-compatible -p ys-coding-agent`

## T3 [impl] 窄 trait 与三跳注入（刀 3）
- 依赖: T1
- 涉及文件: `crates/ys-tool/src/subagent.rs`（新增）、`crates/ys-tool/src/lib.rs`、`crates/ys-tool/src/context.rs`、`crates/ys-component/src/context.rs`、`crates/ys-runtime/src/agent.rs`、`crates/ys-loop/src/basic.rs`
- 描述: 定义 `SubagentOutcome` + `SubagentRunner`（`&str`/`Option<&str>` 进、`SubagentOutcome` 出；用 `ys_core::{StopReason,Usage}`）；`ToolContext::with_subagent`；`RuntimeContext.subagent`；`AgentPorts.subagent` + `with_subagent`（`new` 签名不变）；`basic.rs` 摘要接线点多挂一处。
- 验证: `cargo build --workspace && cargo test -p ys-tool -p ys-runtime -p ys-loop`

## T3t [test] 注入可达
- 依赖: T3
- 涉及文件: `crates/ys-tool/src/context.rs`、`crates/ys-loop/src/basic.rs`
- 描述: 照 `FakeSummarizer` 先例，假 runner 注入后 `ToolContext.subagent` 可读；`AgentPorts::new` 既有调用点全部编译通过。
- 验证: `cargo test -p ys-tool -p ys-runtime -p ys-loop`

## T4 [impl] runner 与 `subagent` 工具（刀 4）
- 依赖: T2, T3
- 涉及文件: `apps/coding-agent/src/subagent.rs`（新增）、`apps/coding-agent/src/toolset.rs`、`apps/coding-agent/src/main.rs`、`apps/coding-agent/src/status.rs`、`apps/coding-agent/src/app_loop.rs`
- 描述: `AppSubagentRunner`（造子 `Agent`/`MemorySession`/`NoopEventSink`/`ChildBoundary`/watchdog `AbortOnDropHandle`/子 `RunLimits`(同 `Config`，`fixed_prompt_tokens` = 子表)/增量落盘到 `~/.yushan/subagents/`(支持 `YUSHAN_SUBAGENTS_DIR`)/sweep 7 天/每回合派发上限 `AtomicU32`/usage 上卷、`TurnStats` 加不增 `turn_count` 的记账入口）；`SubagentTool`；父表追加末位 `subagent`；子表 = 父表去掉 `subagent`；`build_tools` 签名承载超时参数。
- 验证: `cargo test -p ys-coding-agent`

## T4t [test] 派发行为
- 依赖: T4
- 涉及文件: 同上 + 对应测试
- 描述: 子撞 `max_rounds` ⇒ 父不失败 + `is_error`；usage 上卷；超派发上限 ⇒ `Err`；sweep 只清超龄；`model` 名解析失败 ⇒ `Err` 不静默回退。
- 验证: `cargo test -p ys-coding-agent`

## T5 [impl] 提示词（刀 5）
- 依赖: T4
- 涉及文件: `apps/coding-agent/src/prompt.rs`、`apps/coding-agent/tests/e2e_skills.rs`
- 描述: `prompt.rs:107` 改条件式派生（父表有派发工具 ⇒ 不说；子表没有 ⇒ 保留）；加 guideline（规模规则/四要素/并行写目标不相交/并发槽位数）；同步两份测试拷贝。
- 验证: `cargo test -p ys-coding-agent`

## T5t [test] 提示词内容
- 依赖: T5
- 涉及文件: `apps/coding-agent/src/prompt.rs`
- 描述: 父提示含 guideline 且不含「no subagents」；子提示含该句。
- 验证: `cargo test -p ys-coding-agent`

## T6 [impl] 并行派发（刀 6）
- 依赖: T4
- 涉及文件: `crates/ys-tool/src/lib.rs`、`crates/ys-component/src/limits.rs`、`crates/ys-loop/src/basic.rs`、`crates/ys-loop/Cargo.toml`
- 描述: `Tool::is_concurrency_safe()` 默认 `false`；`DEFAULT_MAX_PARALLEL_TOOLS` 常量 + `RunLimits.max_parallel_tools`；Step 9 三段（9a 串行审批 + emit `ToolCall`、9b all-safe ⇒ `futures_util::stream::iter(..).buffered(n)`、9c 保序 emit `ToolResult` + 组装消息）；`ys-loop` 加 `futures-util`。
- 验证: `cargo test -p ys-loop`

## T6t [test] 并行六条
- 依赖: T6
- 涉及文件: `crates/ys-loop/src/basic.rs`
- 描述: `design.md` §4.4 六条（默认串行、开并行更快、混合退回、保序、上限、变异自检）。
- 验证: `cargo test -p ys-loop`

## T7 [impl] 成本回填（刀 7，MANUAL）
- 依赖: T2, T4
- 涉及文件: `docs/arch/subagent-rebaseline/design.md`
- 描述: 装完读数后跑一次真实派发，把真实读数写回 §6.4。
- 验证: 需 `YUSHAN_API_BASE`/`YUSHAN_API_KEY`（当前缺失 ⇒ 人工）
