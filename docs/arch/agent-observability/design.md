# agent-observability — 设计：显式开关的结构化执行日志

> 日期：2026-10-17
> 性质：`arch-design` 产出。需求与约束见 [`context.md`](./context.md)，决策记录见
> [`adr-debug-log.md`](./adr-debug-log.md)。
> 方法：3 个并行 subagent 分别按「最小复杂度 / 可扩展优先 / 性能优先」出候选（见 §2），
> 本文是融合后的推荐方案。

---

## 1. 推荐方案（一句话）

**一个 `--debug` 开关 + 一个异步单写者的 JSONL sink；只在"段"边界记录，永不逐增量；
字段名对齐 OTel `gen_ai.*`；逐轮 usage 走事件流而非旁路回调。**

```text
ys-loop ──AgentEvent──► ChannelSink ──► 消费侧（TUI app_loop / -p main::consume_events）
                                          │
                                          ├─ RoundClock（既有计时，文本行逐字节不变）
                                          └─ DebugTap ──► DebugSink ──(有界队列)──► writer 线程
                                                                                     │
                                                                                     ▼
                                                              $YUSHAN_LOG_DIR/debug-<session>.jsonl

适配器（OpenAI 兼容）──► ① 解析 reasoning_tokens ② attempt/首字节/重试 也走 DebugTap
```

**三条源自实测的硬约束**（决定方案形状）：

1. **绝不逐增量记录** —— 实测一次运行 2,300–8,721 个事件，其中**思考增量占 95–97%**
   （`d-t4`：8,465 思考增量 vs 10 正文增量）。逐条落盘 = 单次运行 ~700 KB + 数千次写盘，
   且会通过**事件信道背压**反向拉长模型流 —— 不只是读数偏移，是改变被测对象。
2. **不在被测区间里做格式化** —— 时间戳在观测入口取，摘要/序列化/入队全部在其后。
3. **关闭时零字节、零线程、零分配** —— `Option<DebugSink>` 为 `None`，热路径只剩一个可预测空判断。

---

## 2. 三个候选与对比

| 维度 | **A 最小复杂度** | **B 可扩展优先** | **C 性能优先** |
|---|---|---|---|
| 模块 | 1 个新文件 `debug_log.rs` | `debug/` 6 文件 | `debug/` 5 文件 |
| 写盘 | 异步 SyncSender + writer | 异步 + writer | 异步 + **定长记录 + 有界队列** |
| 关闭时 | 无 sink | `Option<DebugRecorder>` | `Option<Debug>`（**真零**） |
| 逐轮 usage 来源 | 适配器 tracer 回调 | `ys-model::ModelObserver` + `ModelRequest.trace` | `ys-core::RunObserver` **穿透 5 个 crate** |
| `reasoning_tokens` | 只在 trace 载荷 | 进 `UsageReport`（不动 `Usage`） | 加进 `ys_core::Usage` |
| 事件契约 | 不动 | 新增 `BoundaryObserved` | 不动 |
| Q1 | 只记段 + 时间戳先取 + 异步 | 同 + 无 BufWriter/fsync | 同 + 定长记录 |
| Q4 默认 | bash 首行 256B；`write`/`edit` 正文零字节 | **三档可配** none/head/full，默认 head 160 | 224B 预算；bash **head 160 + tail 64**；哈希 |
| 代码量 | **~750–850 行 / 8 文件** | ~2,000–2,400 行 / 16–20 文件 | ~1,300 行 / 16–18 文件 |
| 自评弱点 | 模型层成为事件流之外的**第二通道**，`--json` 看不到 | **抽象税**；新适配器忘了接就静默为空 | 异步是保险不是必需；**侵入 core** |

**三者收敛的共识**（直接采纳）：只记段不记增量；时间戳先取；异步写 + 有界队列 + 丢弃计数；
工具参数只记摘要、正文零字节。

**分歧点**只有一处：**逐轮 usage 从哪来**。

---

## 3. 分歧的裁决：逐轮 usage 走**事件**，不走旁路回调

三个候选给了三条路：适配器回调（A）／`ys-model` observer（B）／`ys-core` observer 穿透（C）。
**B 自己的自评里给出了第四条**，也是本文的选择：

> 「可以退回到纯事件流方案：新增 `AgentEvent::RoundUsage`，给 `Usage` 加字段……那条路线更短、更少抽象。」

**选它的三条理由：**

1. **旁路回调会静默退化。** trait 带默认空实现 ⇒ 新增一个 `Model` 适配器忘了接 observer，
   debug 文件里模型侧就是空的，**没有任何报错**。这与本仓「漏实现是编译错误、不会静默漂移」的
   既有纪律（`Request` 的穷尽 `match`）正好相反。
2. **单一真相。** 事件流经同一条信道、同一套顺序，TUI / `-p` / `--json` 三种消费者都能看到；
   **子 agent 的 usage 也自然可见**（B 的自评指出旁路方案要靠 `agent_id` 手工透传）。
3. **代价可控。** 新增一个非终局事件 + `Usage` 加一个 `#[serde(default)]` 字段。
   `--json` 是**加法**变更（与已有的 `RoundStarted` 同形）；关闭 debug 时每轮只多一条事件，
   相对每轮 1k–10k 个增量可以忽略。

> `Usage` 新增字段会让 34 处结构体字面量需要机械修补 —— 接受，一次性的。

---

## 4. 详细设计

### 4.1 模块划分

| 文件 | 职责 |
|---|---|
| `apps/coding-agent/src/debug_log.rs`（新增） | 开关解析、记录信封、**异步单写者**、有界队列与丢弃计数、工具参数摘要与脱敏 |
| `apps/coding-agent/src/round_clock.rs`（改） | 既有文本行**逐字节不变**；新增把事件翻成 debug 记录（thinking/text 分段、计数、工具元数据） |
| `apps/coding-agent/src/main.rs`（改） | `--debug` 解析、sink 创建、`-p`/`--json` 接线、退出前 flush |
| `apps/coding-agent/src/app_loop.rs`（改） | TUI 接线、turn 边界、`/new` `/resume` 切文件、Abort 记录 |
| `apps/coding-agent/src/logging.rs`（改） | 抽出 `log_dir()` 供 debug 复用（`YUSHAN_LOG_DIR` / `HOME` 回退），**行为不变** |
| `adapters/model-openai-compatible/src/response.rs`（改） | `ChatUsage` 增 `completion_tokens_details.reasoning_tokens` |
| `adapters/model-openai-compatible/src/lib.rs`（改） | attempt / 首字节 / 重试 trace（与既有 `with_logger` 并列） |
| `crates/ys-core/src/usage.rs`（改） | `Usage` 增 `reasoning_tokens: u32`（`#[serde(default)]`），纳入 `Add` |
| `crates/ys-event/src/event.rs`（改） | 新增非终局 `RoundUsage { round, usage }` |
| `crates/ys-loop/src/basic.rs`（改） | Step 7 拿到 `response.usage` 后 emit `RoundUsage` |

**不动**：`--json` 的输出格式（只增一类事件）、`capabilities.rs`、`ys-tui-coding`、`ys-protocol`。

### 4.2 记录 schema

公共信封（每条）：`schema`（`"yushan.debug/1"`）、`ev`、`t_ms`（unix 毫秒）、`dt_ms`（相对
运行起点的**单调**毫秒）、`seq`（写线程分配的单调序号）、`run`、按适用性带
`session_id` / `turn` / `round` / `tool_call_id`。

**字段名对齐 OTel GenAI semconv**（B 的贡献，零额外成本，将来接 Collector 时 1:1 映射）：

| 记录 | 关键字段 |
|---|---|
| `run_start`（一次） | `version`、`pid`、`os`/`arch`、`cwd`、`session_file`、`gen_ai.system`、`gen_ai.request.model`、`api_base`（**只 scheme+host**）、`limits{...}`、`tools[{name,spec_bytes}]`、`skills{count,budget}`、`env_names[]`（**只记名字不记值**）、`semconv` |
| `turn_start` / `turn_end` | `turn`、`input_bytes`、`budget_ms`；结束时 `rounds`、`elapsed_ms`、`budget_remaining_ms`、`cache_hit/miss` |
| `round_start` / `round_end` | 轮号；结束时 `ttft_ms`、`gen_ms`、`tools_ms`、`total_ms`、**`first_thinking_ms` / `first_text_ms`**、`thinking_span_ms` / `text_span_ms`、`thinking_deltas` / `text_deltas`（**只计数**）、`tool_calls` |
| **`round_usage`** | `gen_ai.usage.input_tokens` / `output_tokens` / `total_tokens`、**`gen_ai.usage.cache_read.input_tokens`**、**`gen_ai.usage.reasoning.output_tokens`**、`gen_ai.response.finish_reasons`、`usage_reported`、`reasoning_tokens_reported`（**区分"真 0"与"没上报"**） |
| `tool_call` | `gen_ai.tool.name`、`gen_ai.tool.call.id`、`gen_ai.tool.call.arguments`（**摘要**）、`args_original_bytes`、`args_truncated`、`args_keys`、`args_redacted_keys` |
| `tool_result` | `duration_ms`、`is_error`、`result_bytes`、`result_lines`、`result_lines_exact`（**不记内容**） |
| `model_attempt` | `attempt`/`max_attempts`、`outcome`、`reason_class`（`transport`/`idle_timeout`/`rate_limited`/`server_error`/`quota`/`fatal`）、`http_status`、`backoff_ms`、**`ttfb_ms`（真·首字节）** |
| `context_compacted` | `estimated_tokens_before` / `after` / `delta`、`summarized` / `kept` |
| `boundary` | `phase`（`enqueued` / `observed`）、`kind`（`abort`/`steer`）、`origin`（`ui`/`budget`） |
| `state_change` | `name`（`model`/`provider`/`session`/…）、`from`/`to` |
| `run_end` | `stop_reason`、`rounds`、`duration_ms`、`exit_code`、累计 usage、`debug{records_written,dropped,write_errors,bytes_written}` |

**缺值约定**：字段适用但拿不到 ⇒ `null`；不适用 ⇒ 省略。**永不把"没上报"写成 0**（沿用
`turn_line` 的 `-` 精神）。

### 4.3 Q1：写盘如何不扰动 `ttft` / `gen`

四层，缺一不可：

1. **只记段，不记增量** —— `ModelTextDelta` / `ModelThinkingDelta` **零记录**，只累计计数。
   这是最大的一刀：事件量从 1k–10k/轮降到 ~10/轮。
2. **时间戳先取、格式化后做** —— `RoundClock::observe` 入口的 `now` 是唯一时间源；
   摘要、序列化、入队全部在其后。即使序列化耗时数微秒，也**不进** `ttft_ms`/`gen_ms`。
3. **调用线程只序列化 + `try_send`，绝不碰文件 IO** —— writer 在独立 **OS 线程**。
   队列满 ⇒ **丢当前记录并计数**，绝不阻塞（阻塞会通过有界事件信道反压到生产者）。
4. **自证扰动上界** —— 把调用线程上的序列化+入队累计耗时写进 `run_end.hot_path_ms`。
   估算：每轮 ~10 条 × 1–3 µs ≈ **10–30 µs/轮**，相对 0.5–143 s 的轮耗时可忽略。

**单行一次 `write`**（预先拼好 `\n`）：进程被杀最多丢整行，**不会产生半行 JSON**。

### 4.4 Q4：工具参数摘要

**原则：debug 是索引，不是内容副本** —— 完整内容在 session JSONL 里，debug 只记"足以定位与聚合"的摘要。

三档可配（`YUSHAN_DEBUG_ARGS`）：

| 档 | 语义 | 默认 |
|---|---|---|
| `none` | 只留 `args_keys` 与原始字节数 | |
| **`head`** | 字符串保留前 **160 B**（UTF-8 边界）+ `args_keys`；总预算 512 B | ✅ |
| `full` | 完整值但有 8 KiB 总预算 | |

**逐工具规则（`head` 档）**：

| 工具 | 记 | 不记 |
|---|---|---|
| `bash` | 命令 **head 160 + tail 64**（尾部常是管道/重定向，是"面操作"的关键）+ `command_bytes` | heredoc 正文 |
| `read` | `path` 全文（≤512B）、`offset`、`limit` | 文件内容 |
| `write` | `path` 全文、`content_bytes`、`content_lines` | **`content` 一个字节都不记** |
| `edit` | `path` 全文、`edits` 条数、`old/new` 字节数 | **`oldText` / `newText` 一个字节都不记** |
| `grep`/`find` | `pattern`、`path`、`glob`、`limit` | 匹配结果 |
| `websearch`/`webfetch` | `query`；`url` 的 scheme+host+path，**query 一律 `<redacted>`** | 完整 URL query（可能含 token） |
| `subagent` | `prompt` 首行 256B + 字节数 | prompt 全文 |
| 未知工具 | `arg_keys` + `args_bytes` + 有界摘要 | 全量序列化 |

**脱敏**（三档都生效，含 `full`）：键名小写命中
`token/api_key/access_key/secret/password/authorization/cookie/credential/private_key`
⇒ 值替换为 `[REDACTED]`，键名进 `args_redacted_keys`。**不提供关闭脱敏的开关** ——
完整内容与秘密泄露是两件事，后者没有排障收益。

### 4.5 写线程与资源上界

| 项 | 值 |
|---|---|
| 队列容量 | 1024（可配） |
| 内存上界 | < 1 MiB（记录本身有界） |
| 文件上限 | 64 MiB / session（可配），触顶停写并记一条 |
| 队列满 | 丢当前记录 + `dropped++`；`run_end` 报总数 |
| IO 错误 | 计数并继续；连续失败 8 次停写线程 |
| 退出 | `flush`：发 Stop → 排空 → 写收尾行；最多等 2 s，超时放弃（best-effort） |

### 4.6 `/resume` 与轮转

- **`/resume` 追加同一 session 的 debug 文件**，新 run 以一条 `run_start` 开头（`run` 字段区分）。
- **`/new` 切新文件**（`debug-<新 session>.jsonl`），旧文件自然结束。

---

## 5. 分阶段落地

| 阶段 | 内容 | 估计 |
|---|---|---|
| **P0** | 开关 + 独立文件 + 异步 sink + 段的富计时（thinking/text 拆分）+ 工具元数据 + `run_start`/`run_end` + `reasoning_tokens` + `RoundUsage` | ~700 行 |
| **P1** | `model_attempt`（含 `ttfb_ms`、重试分类）+ 压缩/截断/boundary 记录 | ~300 行 |
| **P2** | `/new` 轮转、文件上限、`state_change`、jq 配方文档 | ~200 行 |

**P0 就能回答本轮悬着的两个问题**：`gen` 46 s vs 143 s 差在哪（思考 vs 正文），以及逐轮 usage/缓存走向。

---

## 6. 被否方案与理由

| 被否 | 理由 |
|---|---|
| **完整请求/响应体**（用户已排除） | 单条数万 token；代码内容落盘涉及敏感性。OTel 的 GenAI semconv 也把内容采集**默认关闭** |
| **逐增量记录** | 实测占 95–97% 事件量；体积 + 信道背压双重灾难 |
| **旁路 observer 回调**（A/B/C 的共同倾向） | **静默退化**：新适配器忘了接就为空且无报错；且子 agent usage 要靠手工透传 |
| **同步写盘** | 消费侧是事件信道的唯一排水口；磁盘卡顿会反压到生产者，**改变模型流时序** |
| **`tracing` / OTel SDK** | 违反「不引新依赖」 |
| **日志级别体系（error/info/debug/trace）** | 本轮只做开/关（用户裁定）；级别需要给 35 个既有调用点逐一标级 |

---

## 7. 风险

1. **`Usage` 加字段**会牵动 34 处结构体字面量（机械修补，一次性）。
2. **`--json` 新增一类事件** —— 消费者必须忽略未知变体（`AgentEvent` 已 `#[non_exhaustive]`）。
3. **`reasoning_tokens` 依赖 provider 上报** —— 不报时为 `null` 而非 0，并由
   `reasoning_tokens_reported` 显式区分。
4. **异步写线程在单核高负载下仍与主路径争 CPU** —— 无 `nice`/亲和性（标准库不提供），接受。
5. **schema 与 semconv 版本需要维护** —— 同 major 只许加法；升级 semconv 必须显式提交。

---

## 8. 验收

1. **关闭 debug**：目标目录零新文件；`yushan.log` 与 `--json` 输出与今天**逐字节相同**。
2. **打开 debug + 一次 10k 增量的运行**：debug 文件 < 200 KiB；**零** `ModelTextDelta` 记录；
   每行是合法 JSON、`seq` 严格 +1、`t_ms - dt_ms` 恒定。
3. **队列满**（注入慢 writer）：`dropped > 0`，已写行完整。
4. **不调 flush 直接退出**：文件末尾仍是完整行。
5. **设计直击**：用一次真实运行回答「`gen` 里思考占多少」——`round_end.thinking_span_ms` 与
   `round_usage.reasoning_output_tokens` 交叉验证。
