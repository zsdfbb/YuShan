# 核心信道 + Actor 模型（第一刀）— As-built 设计

> 性质：**实际建成记录**（as-built），非计划复述。写「最终是什么样」及「实现中改掉了什么」。
> 范围：sequential-workflow「核心信道 + Actor 模型」第一刀 = 迁移步 0-4 + 前置 Task 0。
> 日期：2026-09-12
> 上游文档：
> - 计划 `docs/design-plans/2026-09-12-core-channel.md`
> - 执行计划 `docs/exec-plans/2026-09-12-core-channel.md`（Task 0 + 18 任务）
> - 设计 `docs/arch/gap-closure/design-core-channel.md`（含 R1-R9 勘误）
> - 质量分析 `docs/arch/gap-closure/review.md`（R1-R8）
>
> 落地 commit（7 个）：
> `d6284af` 前置修竞态 → `99d4126` 步 0 重命名 → `a76c482` 步 1 EventSink → `0ee849b` 步 2 ys-channel + ChannelSink → `0af256e` 步 2 续 main 分发 + --json + 修死锁 → `061806c` 步 3 inbox + steering（含 -p 增量） → `d399ffc` 步 4 Agent::run + 接线。
>
> 一句话结论：**「事件出口 + 消息模型自转」这条主链已通**——`--json` / `-p` 生产路径实测输出正确、`turn` 从 1 起、死锁已结构性消除；实现期在设计的承重处发现并修掉 2 个真 bug（overflow 丢 turn、自由函数短路冲刷致死锁）。

## 1. 交付概览（对照计划逐项）

状态口径：**完成** = 与计划一致；**偏离** = 做了但形状/落点与计划不同；**补做** = 计划未含、实施期发现必须补。

| 计划项 | 计划内容 | 实际结果 | 状态 |
|---|---|---|---|
| Task 0 / 0T | 修 `tools-basic` 三处 `test_dir()` 竞态 | 三处加 `AtomicU64` + `process::id()`；**额外**修 `prompt.rs`（`ENV_LOCK` + `EnvRestore`）与 `commands/builtin.rs`（计数器 + pid），共 5 文件 | 完成（范围扩 3→5） |
| 步 0 / Task 1-2 | 全仓 `agent-*` → `ys-*` 重命名 | 8 个 `crates/ys-*` + `ys-model-openai-compat`（缩短）+ `ys-tools-basic` + `ys-coding-agent`；白名单未误伤；`docs/design.md` §9 同步 | 完成 |
| 步 1.1 / Task 3-4 | `EventSink` 改 try/await 双路径 + `begin_turn` | trait 三方法 + 自由函数 `emit`；8 处调用点、`Forwarder` 1 处适配；**`ModelEventSink` 零改动**（验收点达成） | 完成 |
| 步 1.2 / Task 5-6 | 生产导出 `FailingSink` + `SendFailed` 首覆盖 | `failing.rs` 生产导出，含 `fail_slow` 开关；`SendFailed → LoopError::Event` 有覆盖 | 完成 |
| 步 2.1 / Task 7-8 | 新建 `ys-channel` 契约 crate | `Envelope`/`Source`/`Inbox`/`Intent`/`QueueMode`/`LifecyclePolicy`；零 tokio；单测纯同步 | 完成 |
| 步 2.2 / Task 9-10 | `ChannelSink` 接线器 | `apps/coding-agent/src/channel.rs`：有界 mpsc + overflow(Envelope) + `begin_turn` + `stats`；tokio 显式加 `"sync"` | 完成 |
| 步 2.3 / Task 11-12 | `main.rs` 模式分发 + `--json` | 「先定模式 → 选消费者 → 建 agent」；`parse_args`、`write_json_envelope`、并发消费 | **偏离**（见 §1.1） |
| 步 2.4 / Task 13-14 | `-p` 流式打印 | `print_delta_text` + `consume_print_events<W: Write>`；增量按序写出 | **偏离**（随步 3 commit 落地） |
| 步 3.1 / Task 15-16 | `RuntimeContext.inbox` + 轮边界 steering | `Option<&Inbox>` + `with_inbox()`（`new()` 10 参数未变）；轮边界 drain → append + emit | 完成 |
| 步 4.1 / Task 17-18 | `Agent::run(inbox)` 自转驱动 | `run` + `RunSummary`；`run_turn` 保留为兼容入口委托 `run_one_turn(input, None)` | 完成（含补做接线） |

### 1.1 与计划的形状性分歧

| 项 | 计划写法 | 实际写法 | 原因 |
|---|---|---|---|
| `parse_args` 返回 | `Args { task: Option<String>, print_only: bool, json: bool }` | `Args { mode: Mode }`，`Mode { Print(String), Json(String), Interactive }` | 单枚举互斥模式更诚实：`--json -p` 两 flag 同真在计划里语义未定，枚举化后不可能同真；并新增 `sink_choice(&Mode)` 纯函数锁定「Interactive 用 Noop、Print/Json 用 Channel」 |
| `write_json_envelope` 落点 | `apps/coding-agent/src/{main,format}.rs` | 落 `main.rs` | 与消费循环同处，避免为一个 3 行函数新增模块 |
| `json_output.rs` / `stream_output.rs` | 计划新建两个集成测试文件 | **未创建**；`--json`/`-p` 行为并入 `main.rs` 内联 `mod tests` | 可注入 writer（`Vec<u8>` / `CountingWriter`）内联即可覆盖，且能直接调私有 `consume_events`；`apps/coding-agent/tests/` 仍只有 `integration.rs` + `e2e_tools.rs` |
| `run_turn` 降级 | 「降级为内部 `run_one_turn`」 | `run_turn` **仍为 pub 兼容入口**，内部委托 `run_one_turn(input, None)` | 既有测试与 TUI 仍直调 `run_turn`；保留公开面避免大面积改测试 |
| capacity 下限 | 未提 | CLI 层 `clamp_capacity` 到 16，`YUSHAN_CHANNEL_CAPACITY` 可覆盖并告警 | 性能护栏（非正确性补丁）；真极小容量由 `ChannelSink::new(1/2, …)` 单测直接覆盖 |
| `ys-channel` dev-dep | 计划写 `tokio = full` | 实际只加 `serde_json` | 契约层测试全纯同步，不需要 tokio——这本身就是 ADR-0012 约束的验证 |
| 生产路径接线到 `run` | 步 4 计划「`-p`/`--json` 改调 `run`」 | 初版遗漏（仍是 `run_turn`），review 发现后**补做** | 否则 `begin_turn` 在生产路径从不触发 → `--json` 的 `turn` 恒 0（见 §2.8） |

## 2. 实现与设计的重大分歧（最有价值的部分）

设计期 R1-R8 已修；下面 8 条是**实施期**才发现或进一步改写的点，均已回补进 `design-core-channel.md` 的勘误。

### 2.1 overflow 丢 turn（设计伪代码同源 bug）

- **设计原文**：`ChannelSink.overflow: VecDeque<AgentEvent>`，撞满时只留裸事件，冲刷时用**当前** turn 重新包装。
- **问题**：跨回合存活的积压事件会被误标为后一回合的 turn，破坏 `--json` 分组——正是 R4（turn 显式化）要防的误分组，却在 overflow 实现里复现了。
- **实际**：`overflow: VecDeque<Envelope>`，**turn 随信封冻结**，冲刷时原样 `send`，不重新 `wrap`。另修：冲刷失败时 `push_front` 保留事件（与 `drain` 的 Closed 分支一致），不静默丢弃。
- **验证**：`channel.rs::overflow_preserves_turn_across_turns`；**反向验证**——回退修复后该测试如期失败（`left: 2 right: 1`）。

### 2.2 死锁（自由函数快路径短路了冲刷）→ 改为总走慢路径

- **设计 R9**：自由函数 `emit` 原为「先试 `try_emit`，`Err` 才回落慢路径」。与 `ChannelSink::try_emit`「满时缓冲并返 `Ok`」契约相互作用成死锁：
  1. 满 → 事件进 `overflow`、返 `Ok`；
  2. 自由函数见 `Ok` 即返回，**永不进慢路径** → overflow 永不冲刷；
  3. 终局事件恰是最后一个，其后无人 `emit().await`；
  4. `consume_events` 等终局事件 → 永不返回 → `tokio::join!` 挂起。且 overflow 只增不减，背压失效。
- **实际**：自由函数 `emit(sink, event)` **一律** `sink.emit(event).await`。洞察是「同步快路径只对同步调用者有意义」——唯一需要它是 `Forwarder`（同步 `ModelEventSink`，不能 await）；循环内调用点本就在 async 上下文，没有理由走快路径。
- **验证**：`small_channel_no_deadlock_capacity_{1,2}`、`run_turn_and_consume_do_not_deadlock_at_small_capacity`、`consume_events_stops_at_terminal_event_without_channel_close`、`slow_path_applies_backpressure_when_consumer_stalls`。**注入旧实现后这些测试超时失败（Elapsed）**，证明确为回归测试。

### 2.3 `try_emit` 的 `Err` 语义（消费者消失）

- **实际契约**（写进 `ys-event/src/sink.rs` 文档）：`Err(event)` **仅表示「消费者已消失」**，事件原样退回；**信道满时必须内部缓冲，不得返回 `Err`**。
- **理由**：`try_emit` 的**唯一**调用者是 `Forwarder`，其签名为 `let _ = self.sink.try_emit(..)`——若满时返回 `Err`，事件会被静默丢弃。故把「满」的处置收敛在 sink 内（overflow），`Err` 只承载消费者消失。
- **与 `FailingSink` 的关系**：替身的 `try_emit` 为通用失败注入器，语义比生产契约宽松（文档已注明），两者不作同一要求。

### 2.4 `Source` 手写 serde（避开 `Arc<str>` 的 rc feature 扩散）

- `Source(Arc<str>)` 由 R7 采纳。但 `Arc<str>: Deserialize` 需要 serde 的 `rc` feature，而 **feature 会在 Cargo 工作区内统一（unification）扩散**到全仓。
- **实际**：手写 `Serialize`（`serialize_str`）与 `Deserialize`（读 `String` 再 `Arc::from`），只影响本类型，且保证序列化形如裸字符串 `"agent"`（`--json` 可读契约）。

### 2.5 `Forwarder` → `try_emit` 不背压的固有取舍

- 精确表述（已写进自由函数文档）：**异步路径（自由函数 → `ChannelSink::emit`）背压生效**；**同步路径（`Forwarder::emit` → `try_emit`）不生效**——`Forwarder` 实现同步 `ModelEventSink`（ADR-0004 点 2 的最小 ABI 面），不能 `await`，满时只能缓冲。
- 其 `overflow` 上界 ≈ **单次模型响应的增量事件数**（非严格无界，也远大于信道容量）。这是「同步回调无法背压」的固有取舍，**非缺陷**。要强背压须改 `Forwarder` 为异步形态（牵动模型适配器 ABI），本设计明确不做。

### 2.6 `#[non_exhaustive]` 的 `LifecyclePolicy` 迫使下游写 wildcard

- `LifecyclePolicy` 带 `#[non_exhaustive]`。在 `ChannelSink::on_consumer_gone` 里 match 时**无法穷尽**，必须写 `_ =>` 兜底。
- **实际**：`ContinueWithoutConsumer => Ok(())`，`_ => Err(event)`（保守——未知策略不静默丢弃）。这是 `#[non_exhaustive]` 的预期代价，换来将来加变体不破坏下游。

### 2.7 「消费者消失」的返回类型（Err vs 原设计措辞）

- **设计措辞**：「消费者消失 → 干完当前轮收摊」「正常收场（非故障）」。
- **实际语义**：`StopWhenConsumerGone` 下 sink 返回 `Err` → 自由函数传播为 `LoopError::Event(EventError::SendFailed)` → `Agent::run` 以 `Err` 返回。**不引入新的 `StopReason`**，保持 ADR-0004「emit 失败即终止」的既有语义。
- **对齐说明**：这是「策略预期内的终止，但返回类型与用户取消的 `Ok(RunResult { stop_reason: Cancelled })` 不同」。「正常收场」是语义描述，落地返回类型是 `Err`——已在 `Agent::run` 文档注释中写明该取舍。

### 2.8 生产路径漏接 `begin_turn`（补做）

- 步 4 初版只做了 `Agent::run`，但 `-p`/`--json` 仍调 `run_turn` → **`begin_turn` 在生产路径从不触发** → `ChannelSink.turn` 恒为初值 0。
- review 判定为「没做完」而非「有意推迟」（计划只推迟 `/new` 命令层），遂补做接线：`-p`/`--json` 构造 `Inbox` + `push(FollowUp)` + `agent.run(&inbox)`，turn 从 1 起。
- **验证**：`main.rs::run_via_inbox_sets_turn_from_one`；反证——改回 `run_turn` 则退回 0。

### 2.9 流式本体（后续块 A）：未加 `ModelEvent::ToolCallDelta`

> 本节记录第一刀之后补做的「流式本体」（流式计划块 A），归入同一份 as-built。

- **设计原貌**：`design-core-channel.md`「流式粒度」曾设想 `ModelEvent` 增 `ToolCallDelta { index, id?, name?, arguments_delta }`，与 `ThinkingDelta` 并列。
- **实际**：只加 `ThinkingDelta`；**不加 `ToolCallDelta`**。工具参数增量在适配器内部按 `index` 分组累积（`ToolCallAccumulator`，`arguments` 为 JSON 字符串片段拼接），流结束后一次性产出完整 `ContentBlock::ToolUse`。
- **理由**：设计已定「**工具参数不冒泡**到 `AgentEvent`」——没有任何 `AgentEvent` 与之对应。若仍把 `ToolCallDelta` 做成 `ModelEvent` 变体，`Forwarder` 的 `_ => Ok(())` 兜底会静默吞掉它，它就是一个**无人消费的 dead variant**。与其留空壳，不如把组装完全关在适配器内（那里才拿得到 `index` 顺序与分片语义），职责更内聚。
- **同批落地**：请求 `stream: true`；`parse_sse_stream` 从「返回缓冲 `Vec`」改为**边解析边回调**（可测核心 `feed_sse_bytes` 支持跨 chunk 行边界；缓冲为**字节级** `Vec<u8>`，只在字节层面切出完整行后才做 UTF-8 解码，避免多字节字符被 TCP 分片切在中间时损坏）；`TextDelta` / `ThinkingDelta` 实时冒泡（thinking 仅在 `ProviderCompat::has_reasoning_content` 为真时）；`usage` 由 `ProviderCompat::supports_stream_usage` 门控——**默认开启**（`standard()` 与 `Default` 均如此，故 env-var 配置落到 `custom` 时 token 统计正确），请求携带 `stream_options.include_usage` 并在流末包取 `usage`；若严格校验的端点因该字段返回 **HTTP 400**，适配器剥掉 `stream_options` **只重试一次**（stderr 打明确警告、本次 `usage` 退回 `Usage::default()`），故「安全」与「统计完整」兼顾；仅显式关闭者（`minimax()`，其兼容层放行行为未经验证）不带该字段、直接退回 `Usage::default()`。
- **验证**：`ys-model-openai-compat::stream::tests::feed_sse_bytes_*`（分片/多行/`[DONE]`/非 JSON/`reasoning_content`/`data:` 无空格前缀/末包缺 `delta`）、`tests::feed_sse_bytes_preserves_multibyte_utf8_split_across_chunks`（多字节 UTF-8 被切在字符中间，断言无 U+FFFD）、`tests::streamed_text_deltas_concatenate_to_final_text_byte_for_byte`（增量拼接 == 终态，逐字节）、`tests::tool_call_deltas_assemble_into_one_tool_use`（多片 → 一个完整 `ToolUse`）、`ys-loop::basic::tests::forwarder_maps_thinking_delta_to_agent_event`。端到端本地 SSE mock：`--json` 输出 3 条 `ModelTextDelta`，`-p` 在 0.21/0.41/0.62s 分三次写出（真增量）。**usage 门控修复**：`ys-model-openai-compat::compat::tests::standard_enables_stream_usage`（`standard()`/`Default` 默认 true）、`ys-coding-agent::provider::tests::test_compat_mapping`（custom/未知 → true）；端到端 `tests/stream_usage_fallback.rs`——env-var 配置＋容忍该字段的 mock 拿回 `usage {input:11, output:7}`；对带 `stream_options` 的请求返 400 的 mock 触发一次性回退（exit 0、有警告、重试请求不含该字段、`usage` 为 0）；不带该字段却遇 400 则直接失败、只发 1 次请求。

## 3. 最终 API 清单（各 crate 公开面）

### `ys-event`（契约层，无 tokio 生产依赖）

| 项 | 形状 |
|---|---|
| `trait EventSink: Send` | `fn try_emit(&mut self, AgentEvent) -> Result<(), AgentEvent>`（同步快路径，`Err` = 消费者消失） |
| | `fn emit<'a>(&'a mut self, AgentEvent) -> Pin<Box<dyn Future<Output=Result<(), EventError>> + Send + 'a>>`（异步慢路径，背压点） |
| | `fn begin_turn(&mut self, _turn: u32) {}`（默认 no-op，由 `Agent::run` 调用） |
| 自由函数 | `#[inline] pub async fn emit(sink: &mut dyn EventSink, AgentEvent) -> Result<(), EventError>`——**总走慢路径** |
| `NoopEventSink` | 两方法实现（快路径 `Ok`，慢路径 `Box::pin(async { Ok(()) })`） |
| `CollectingSink` | 慢路径委托 `try_emit`，避免两处各写一份 push |
| `FailingSink` | **生产导出**；`new(succeed_first)` / `always_ok()` / `set_fail_slow(bool)` / `emitted()` / `call_count()` / `count()` / `try_calls()` / `slow_calls()` |

`ys-event/Cargo.toml` 生产依赖未新增（仍 `ys-core` + serde + serde_json + thiserror），`Pin`/`Future` 来自 std。

### `ys-channel`（新契约 crate，零 tokio）

| 类型 | 形状 |
|---|---|
| `Source(Arc<str>)` | `Source::agent()`、`Default`；手写 serde（§2.4） |
| `Envelope` | `{ source: Source, turn: u32, event: AgentEvent }`；`Envelope::new(source, turn, event)`；`Serialize/Deserialize/PartialEq` |
| `LifecyclePolicy` | `#[non_exhaustive] { StopWhenConsumerGone, ContinueWithoutConsumer }`，`Copy`，`Default = StopWhenConsumerGone` |
| `Intent` | `{ Steering, FollowUp }`，`Copy` |
| `QueueMode` | `{ All, OneAtATime }`，`Copy`，`Default = OneAtATime`；`All` = 取空全部并合并为**一条** `Message`（多块依序，role 固定 `User`） |
| `Inbox` | `Clone`；`new()` / `with_modes(steering, follow_up)` / `push(&self, Message, Intent)` / `close(&self)` / `take_steering(&self) -> Vec<Message>` / `take_followup(&self)` / `is_empty()` / `is_closed()` / `steering_mode()` / `follow_up_mode()`；内部 `Arc<Mutex<Inner>>`，锁中毒借 `into_inner` 恢复 |

依赖：`ys-core` + `ys-event` + serde；dev-dep 仅 `serde_json`（**无 tokio**，`cargo tree -p ys-channel --edges normal` 确认）。

### `ys-component`

```rust
pub struct RuntimeContext<'a> { /* 原 10 字段不变 */, pub inbox: Option<&'a Inbox> }
impl<'a> RuntimeContext<'a> { pub fn with_inbox(self, inbox: &'a Inbox) -> Self; }
```
`new()` 的 10 参数签名**未变**（影响面压到 1 个构造点）；新增 `ys-channel` 依赖（零 tokio，不破坏契约层约束）。

### `ys-loop`

| 变化 | 落点 |
|---|---|
| 8 处 `ctx.events.emit(..)` → `emit(ctx.events, ..).await`（自由函数） | `basic.rs` |
| `Forwarder::emit` → `let _ = self.sink.try_emit(..)`（同步，无背压） | `basic.rs` |
| 轮边界 steering drain：**Step 4b 压缩之后、`rounds += 1` 之前** → `session.append` + emit `UserMessage`；**不增 rounds、不改 turn**；`inbox = None` 时零额外调用 | `basic.rs:134-146` |

### `ys-runtime`

```rust
pub struct RunSummary { pub turns: u32, pub usage: Usage, pub last_stop: Option<StopReason> }
impl Agent {
    pub async fn run(&mut self, inbox: &Inbox) -> Result<RunSummary, LoopError>; // 回合边界拉 followUp；空即返回；每回合 begin_turn(n)（n 从 1 递增）；不收 policy
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError>; // 兼容入口 → run_one_turn(input, None)
    async fn run_one_turn(&mut self, input: AgentInput, inbox: Option<&Inbox>) -> Result<RunResult, LoopError>; // 私有
}
```
新增 `ys-channel` 依赖；`prelude` 导出 `RunSummary`。

### `ys-coding-agent`（产品层）

| 模块 | 公开/关键项 |
|---|---|
| `channel.rs`（新） | `ChannelSink::new(capacity, policy) -> (Self, mpsc::Receiver<Envelope>)`；`impl EventSink`；`begin_turn` 显式设 turn；`ChannelStats { backpressure_waits, buffered, consumer_gone }` + `stats()` |
| `main.rs` | `parse_args(&[String]) -> Result<Args, String>`；`Mode { Print(String), Json(String), Interactive }`；`SinkChoice` + `sink_choice(&Mode)`；`write_json_envelope<W: Write>`；`print_delta_text(&Envelope) -> Option<&str>`；`is_terminal`；`consume_events<W, F>`（**以终局事件为终止条件**，非等信道关闭）；`consume_print_events<W: Write>`；`run_print_mode` / `run_json_mode`；`channel_capacity` / `clamp_capacity` |
| `Cargo.toml` | tokio features 显式加 `"sync"`（此前 mpsc 能编译纯属 `reqwest→hyper` 传递启用）；实际为 `["rt-multi-thread","macros","io-util","fs","sync","time"]` |

## 4. 测试盘点

### 4.1 总量

| 指标 | 值 |
|---|---|
| 基线（重命名前） | 178 |
| 现状 | **242 passed / 0 failed / 1 ignored** |
| 净增 | **+64** |
| ignored | `ys-coding-agent/tests/e2e_tools.rs`（真实 API，需凭证） |

### 4.2 各 crate 分布（`cargo test --workspace` 实测）

| 目标 | 通过数 | 备注 |
|---|---|---|
| `ys-core` | 5 | |
| `ys-event` | 14 | 含 `FailingSink` 6 条 |
| `ys-channel` | 15 | 全纯同步 `#[test]`，无 `#[tokio::test]` |
| `ys-component` | 2 | |
| `ys-loop` | 18 | 含 wheel 边界 steering / `SendFailed` |
| `ys-model` | 3 | |
| `ys-model-openai-compat` | 2 | |
| `ys-session` | 8 | |
| `ys-tool` | 8 | |
| `ys-tools-basic` | 21 | 含 Task 0 的 `test_dir()` 唯一性 |
| `ys-runtime`（lib） | 18 | 含 `run` 语义单测 |
| `ys-runtime` / `actor_run`（集成） | 7 | 新增，跨 crate 自转 |
| `ys-runtime` / `v0_integration`（集成） | 9 | 存量 |
| `ys-coding-agent`（bin） | 110 | 全模块内联单测（含 `channel::tests`、`main::tests`） |
| `ys-coding-agent` / `integration` | 2 | 存量 |
| `ys-coding-agent` / `e2e_tools` | 0（1 ignored） | 需凭证 |
| **合计** | **242** | |

### 4.3 集成测试落点

- `crates/ys-runtime/tests/actor_run.rs`（7 条）：`followup` 自转、空 inbox 立即返回、`begin_turn` 编号从 1、消费者消失 `Err(SendFailed)`、steering × followUp 协同、`QueueMode::All` / `OneAtATime` 粒度。自带 `SharedSink` / `CountingModel` / `SteeringProbeModel` 替身，不反向依赖 `apps/coding-agent`。
- `apps/coding-agent` 内联：`channel.rs` 11 条（含 2 条死锁回归 + 1 条背压阻塞 + overflow turn 保真）；`main.rs` 12 条（`parse_args`、`write_json_envelope`、`-p` 增量顺序 + `CountingWriter` 断言分次写出、消费终止条件、并发不死锁、`run` 接线 turn 从 1）。
- **未创建** `apps/coding-agent/tests/{json_output,stream_output}.rs`（见 §1.1）。

### 4.4 变异测试的使用（本次 review 大量采用）

实施期由 review **独立执行**注入验证，作为「测试是否真的锁住行为」的证据：

| 阶段 | 注入项 | 结果 |
|---|---|---|
| ys-channel 契约 | 5 项 | 5/5 被捕获 |
| ChannelSink | 6 项 | 6/6 被捕获 |
| overflow turn 保真 | 回退「存 Envelope」为「存 AgentEvent」 | 测试失败（`left: 2 right: 1`） |
| 死锁回归 | 注入旧「自由函数先走快路径」 | 相关测试超时失败（`Elapsed`） |
| 轮边界 steering | 删除注入 / 挪位置 / 加 rounds / 输出 ToolCall / 注入挪到 request 组装之后 | 全部被对应测试捕获 |
| `-p` 增量 | 改为「缓冲后一次写出」 | `write_calls == 1` → 断言失败 |
| 步 4 自转 | begin_turn 顺序、空批次检查、inbox 传递、`take_followup`/`take_steering` 互换、Err 吞掉 | 6/6 被捕获 |

## 5. 验证方式

### 5.1 命令与结果（本次实测）

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | 成功（13 条存量 warning，非本次引入） |
| `cargo test --workspace` | **242 passed / 0 failed / 1 ignored** |
| `cargo clippy --all-targets` | **33 条 warning**（存量，见 §7.4） |
| `cargo fmt --check` | 通过（exit 0） |
| `cargo tree -p ys-channel --edges normal` | 无 tokio |

### 5.2 端到端手工验证（本地 mock，实测）

起一个返回固定响应的本地 OpenAI 兼容 mock（`POST /v1/chat/completions` → `"Hello world"`），设 `YUSHAN_API_BASE/KEY/MODEL`：

`./target/debug/ys-coding-agent --json "hi"`（exit 0）：
```json
{"source":"agent","turn":1,"event":{"UserMessage":{"message":{"role":"User","content":[{"Text":{"text":"hi"}}]}}}}
{"source":"agent","turn":1,"event":{"ModelTextDelta":{"text":"Hello world"}}}
{"source":"agent","turn":1,"event":{"RunFinished":{"stop_reason":"Completed","usage":{"input_tokens":1,"output_tokens":2},"rounds":1}}}
```
- 每行均为合法 JSON，`source` 为可读字符串，`turn == 1`（`begin_turn` 生效），**末行为终局事件**。

`./target/debug/ys-coding-agent -p "hi"`（exit 0）：输出 `Hello world` + 换行；无 JSON 混入。

`YUSHAN_CHANNEL_CAPACITY=1 ./target/debug/ys-coding-agent --json "hi"`（exit 0）：stderr 警告 clamp 到 16，stdout 三行事件完整、含 `RunFinished`——**不复现旧死锁**。

无凭证路径（隔离 `HOME`）：`--json` / `-p` 均打印可操作提示（`No model configured…`）并 exit 1；`--nope` → `未知参数`；`-p` 无任务 → 参数错误提示。均 exit 1。

### 5.3 MANUAL_ACK（不在自动验收内）

1. 真实 API 端到端：`cargo test -p ys-coding-agent e2e -- --ignored`（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）。
2. TUI 终端观感：`cargo run -p ys-coding-agent` 跑一轮，人工确认渲染/滚动/Ctrl-C。本轮 TUI 未改（仍 `NoopEventSink` + `run_turn`）。

## 6. 明确未做（留待后续）

| 未做项 | 状态与归属 |
|---|---|
| **TUI 增量渲染** | 未做；TUI 仍 `NoopEventSink` + `run_turn`（无 turn 语义、无事件消费） |
| **`/new` 命令层**（换 Session + 新空 Inbox，pending 丢弃） | 留迁移步 5；契约已在 `Agent::run` 文档注释标注，命令层未实现 |
| **ADR-0010 所有权收敛**（session/events 移出 `Agent`、`CommandContext` 不再持 `&mut Agent`） | 留迁移步 5，与信道解耦 |
| **正交优化**：`Arc<Vec<Message>>` 请求快照、`Bytes` delta、`Usage`/`StopReason: Copy`、`max_rounds`/`bash_timeout` 可配、事件落盘 | 留迁移步 6，与信道正交 |
| **`docs/adr/*`、`docs/arch/*` 文档重命名** | 有意保留为历史记录 |

## 7. 遗留缺口与已知问题（诚实清单）

### 7.1 `-p` / `--json` 无 steering 生产者

CLI 形态下 `run` 是「喂一条 followUp → 跑完退出」，运行期**没有任何协程 push steering**（无第二个输入源）。轮边界 steering 机制代码路径完整、有单测与跨 crate 集成覆盖，但**生产路径下无人使用**。等 TUI/交互接入事件与输入桥接后才会真正用上。

### 7.2 `ChannelStats` 未接线

`stats()` 与 `ChannelStats` 目前**仅测试读取**，生产消费循环未输出指标；靠 `#[allow(dead_code)]` 压制 warning。R6 的「背压可观测」只到「可读」层面，未到「可见」层面。

### 7.3 `ys-loop` 注入测试未覆盖 `QueueMode::All`

`BasicLoop` 层的 steering 注入测试只覆盖默认 `OneAtATime`；`QueueMode::All` 的合并语义在 `ys-channel` 单测与 `actor_run` 集成（followUp）里覆盖，但**未在 `ys-loop` 轮边界走一遍 `All`**。

### 7.4 clippy 存量 warning

`cargo clippy --all-targets` 实测 **33 条**，`cargo build` 13 条，**均非本次引入**（exec-plan 曾误写「无 warning」，实际从未清零）。本次改动本身未新增 warning。

### 7.5 其他

- `Envelope` 的**实际内存尺寸未实测**：设计 §8 的 ~112 B 系估算，实现未补 `size_of::<Envelope>()` 断言。
- `ys-session/src/jsonl.rs` 4 个硬编码 `test_jsonl_*.jsonl` 文件名未修（单进程内不互踩，仅并发 `cargo test` 进程互踩，低危，本轮仅记录）。
- 步 2.4 的 `-p` 增量实现随步 3 commit（`061806c`）落地，提交粒度与计划步号非一一对应。
