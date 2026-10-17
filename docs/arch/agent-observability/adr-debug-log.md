# ADR — `--debug` 结构化执行日志：事件流而非旁路回调，只记段不记增量

给 `ys-coding-agent` 加一个显式开关的结构化执行日志（`--debug` / `YUSHAN_DEBUG`）：
关闭时零字节零开销；打开时把**段边界**（轮、工具调用、模型尝试、状态变化）写成
`$YUSHAN_LOG_DIR/debug-<session>.jsonl`，字段名对齐 OTel GenAI semconv。

**两处关键取舍**：① **逐轮 usage 走事件流**（新增非终局 `AgentEvent::RoundUsage`），
不走适配器旁路回调；② **永不逐增量记录**，只记段与计数。

## Status

proposed（2026-10-17）

## 背景

- **需求**（原话）：「先提高 agent 执行过程的可观测性，每次执行在启动 debug 选项时要输出充分的
  日志信息，提供给问题定位、性能分析等。」范围与约束见 [`context.md`](./context.md)。
- **直接动因是三处实测盲区**：33 轮里 3 个离群轮吃掉 36% 墙钟；一轮里单个 `bash` 吃掉 73.7 秒
  （逐调用计时已定位）；同一提示 `gen` 46 s vs 143 s **无法归因**（思考与正文混在一起）。
- **已有的可观测面**：`yushan.log`（手写 logger，35 个调用点，无级别）、`--json` 事件流、
  `--stats` 背压读数、启动/退出期 stderr。缺的是「可开关、可机器聚合、带相关 id」。
- **同类做法**：codex 用 `tracing` + OpenTelemetry（`api_request.duration_ms` / `ttft` /
  `tbt` / `tool.call.duration_ms` / `turn.token_usage`）；OTel GenAI semconv 给出
  `gen_ai.usage.*`（含 `cache_read.input_tokens` 与 `reasoning.output_tokens`）与
  `gen_ai.tool.call.*`，且**内容采集默认关闭**（PII）。
- **本仓约束**：不引新依赖（`docs/architecture.md` §13）；`ys-core`/`ys-event`/`ys-protocol`
  零 tokio、零 IO；TUI alt-screen 后只能落文件；事件只观察。

## 决策

### 1. 开关与落点

`--debug` 命令行 + `YUSHAN_DEBUG` 环境变量（**环境变量优先**）；输出**结构化 JSONL** 到
**独立文件** `debug-<session>.jsonl`。不开时 `Option<DebugSink>` 为 `None` ——
不建线程、不建文件、不分配、不写一个字节。

### 2. **只记段，不记增量**

`ModelTextDelta` / `ModelThinkingDelta` **零记录**，只累计计数。记录粒度是：轮
（`round_start` / `round_end`）、工具（`tool_call` / `tool_result`）、模型尝试
（`model_attempt`）、usage（`round_usage`）、状态变化。

**依据（实测）**：一次运行的 2,300–8,721 个事件中，**思考增量占 95–97%**
（`d-t4`：8,465 思考 vs 10 正文）。逐条落盘 = 单次 ~700 KB + 数千次写盘；更严重的是消费侧变慢会
通过**有界事件信道背压**反压到 `ys-loop` 的 `emit().await`，**改变模型流的真实时序** ——
这已不是读数偏移，而是观测改变了被测对象。

### 3. 异步单写者 + 有界队列 + 丢弃计数

调用线程只做**序列化 + `try_send`**，writer 在独立 OS 线程；队列满**丢当前记录并计数**，
绝不阻塞；**一行一次 `write`**（进程被杀不留半行）；`run_end` 自报 `dropped` 与
`hot_path_ms`（调用线程累计耗时，自证扰动上界 ~10–30 µs/轮）。

### 4. **逐轮 usage 走事件流**，不走旁路回调

新增非终局 `AgentEvent::RoundUsage { purpose, round, usage }`，`ys-loop` 在拿到
`response.usage` 后发一次；`ys_core::Usage` 增 **`reasoning_tokens: Option<u32>`**
（`#[serde(default, skip_serializing_if = "Option::is_none")]`，纳入 `Add`）。
**必须是 `Option`**：`None` = 未上报、`Some(0)` = 真 0；且 `None` 时不序列化该键 ——
这是「过滤 `RoundUsage` 后 `--json` 逐字节不变」的前提。

**为什么不走 observer 回调**（三个候选的共同倾向）：

- **旁路会静默退化** —— trait 带默认空实现，新 `Model` 适配器忘了接 observer，debug 里模型侧
  就是空的，**无任何报错**。这与本仓「漏实现是编译错误」的既有纪律相反。
- **与 `Model` 实现解耦** —— usage 由**主循环**从 `response.usage` 直接发出，不依赖适配器
  是否实现某个 trait；换模型后端不会让 usage 记录静默消失。三种消费者经同一信道看到同一份数据。
- **代价可控** —— `--json` 是加法变更（与 `RoundStarted` 同形）；关闭 debug 时每轮多一条事件，
  相对每轮 1k–10k 个增量可忽略。

> ⚠️ **撤回一条曾用过的理由**：「子 agent 的 usage 自然可见」—— **不成立**。
> 子 agent 的事件出口是 `NoopEventSink`（`apps/coding-agent/src/subagent.rs:4,371`），
> 其 `RoundUsage` 会被丢弃；父侧只在回合末看到上卷总量。子 agent 的逐轮可见性是**另一个需求**。

**覆盖面（有意为之的边界）**：主循环每轮（`purpose=turn`）与**压缩摘要**（`purpose=compaction`，
需给 `CompactOutcome` 加 `usage`）覆盖；**工具侧摘要**与**子 agent 逐轮**不覆盖 ——
前者所在工具上下文没有事件出口（`summarize.rs:42`），后者事件被 `NoopEventSink` 丢弃。
`run_end` 记回合累计 usage 兜底，总量始终可核。

### 4b. 通道边界：事件 vs 旁路

「旁路会静默退化」否掉的是**把主循环已握有的事实交给适配器去报**。但**只在适配器里存在的事实**
（每次 HTTP 尝试、`ttfb`、重试分类、退避）事件流里没有（实测适配器里 `AgentEvent::` 出现 **0 次**），
只能旁路。**旁路的静默退化风险只落在 P1 的适配器记录上**，缓解是 P1 加启动期自检：
debug 已开、已发过模型请求，却一条 `model_attempt` 都没有 ⇒ `run_end` 写 `adapter_trace_missing`。

### 5. 工具参数：索引，不是副本

`debug` 里只记**摘要**（默认 `head`：字符串前 160 B + 顶层键 + 原始字节数），
`write`/`edit` 的正文**一个字节都不记**，`webfetch` 的 query 一律 `<redacted>`；
键名命中 `token/secret/password/authorization/...` 的值替换为 `[REDACTED]`（含 `full` 档）。
完整内容以 session JSONL 为权威来源。

### 6. 字段名对齐 OTel GenAI semconv

`gen_ai.usage.input_tokens` / `output_tokens` / `total_tokens` / `cache_read.input_tokens` /
`reasoning.output_tokens`、`gen_ai.tool.name` / `tool.call.id` / `tool.call.arguments`、
`gen_ai.response.finish_reasons`。**零额外成本**：JSON key 允许点号，将来接 Collector 1:1 映射。
`SCHEMA_VERSION`（`yushan.debug/1`）与 `SEMCONV_VERSION` 都钉死并写进 `run_start`。

**缺值约定**：不适用 ⇒ 省略；适用但拿不到 ⇒ `null`。**永不把「没上报」写成 0** ——
cache 与 reasoning 都带 `*_reported` 布尔。

## Consequences

- **正面**：debug 模式不再扰动它要测的时序（只记段 + 时间戳先取 + 异步写）；
  `gen` 可以拆成「思考 / 正文」两条（`thinking_span_ms` / `text_span_ms` +
  `reasoning_tokens` 交叉验证）；一次运行可复现（`run_start` 记全 limits/工具表/配置来源）。
- **负面**：
  - `Usage` 加字段牵动**既有 27 处** `Usage` 字面量（其中 **17 处带 `..Default::default()`** 自动兼容，
    **实际补字段 10 处**）—— 机械修补，一次性；
  - `--json` 多一类事件，消费者须忽略未知变体（`AgentEvent` 已 `#[non_exhaustive]`）；
  - debug 文件是**新的敏感产物**（命令头、路径、pattern 会落盘）—— 默认 `head` 已最小化，
    但**没有默认保留期清理**（留作后续）；
  - 异步写线程在单核高负载下仍与主路径争 CPU（无 `nice`/亲和性）。
- **明确不做**：完整请求/响应体（用户排除 + semconv 默认）；日志级别体系（本轮只做开/关）；
  远端上报/采样；TUI 内展示 debug 流。
- **可逆性**：改动是加法（新事件 + 新字段 + 新文件）；移除时删 `debug_log.rs` 与 `RoundUsage`
  即可，`--json` 消费者因 `#[non_exhaustive]` 不受影响。

## 参考

- [`context.md`](./context.md) —— 现状、约束、范围与未澄清问题
- [`design.md`](./design.md) —— 融合方案、三个候选的对比矩阵、Q1/Q4 的详细答案
- codex 的指标命名（`api_request.duration_ms` / `ttft` / `tbt` / `tool.call.duration_ms`）
- OTel GenAI semantic conventions（`gen_ai.*`；内容采集默认关闭）
