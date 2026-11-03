# 流中断恢复与根因可见 — As-built

> 性质：**实际建成记录**。计划见 `docs/design-plans/completed/2026-11-03-stream-interrupt-recovery.md`。
> 日期：2026-11-03 ｜ 基线 `f27044e`。

## 一句话

provider 在 200 + 正常 ttfb 之后掐断响应体（实测 2772 次 attempt 中 13 次，0.47%）不再让整回合失败：
**适配器把「流已开始后中断」与其它 provider 失败分开上报**，循环层只对前者丢弃本轮增量并重发
（默认 2 次，`config.json` 可覆盖），错误文本带上 `source()` 链的根因，且两处重试机制**相加而非相乘**。

## 根因可见

改前后（实测同一条截断流）：

```text
before: model provider error: model provider error: SSE stream error: error decoding response body (streaming already began; not retried)
after:  model provider error: SSE stream error: error decoding response body ← error reading a body from connection ← end of file before message length reached (streaming already began; not retried)
```

两处改动合成这个结果：

- `stream.rs` 的 `describe_error_chain` 沿 `source()` 逐层拼接（` ← ` 连接，相邻同文去重，
  `ERROR_CHAIN_MAX_DEPTH = 8` 封顶）—— `reqwest` 的 `Display` 只给最外层一句。
- `lib.rs` 的 `bare_message` 剥掉 provider 类变体自带的 `model provider error: ` 前缀 ——
  该 reason 随后会被重新包进 provider 类错误，不剥就是双层前缀。

## 重试分成两段（关键，勿合并）

```text
适配器（adapters/model-openai-compatible）
  attempt 循环：只管传输层抖动（连接 / 超时 / 429 / 5xx），max_attempts = 5
  流已开始后中断（meta.stream_started）⇒ 停止重试，返回 ModelError::StreamInterrupted

循环层（crates/ys-loop/src/basic.rs Step 6）
  stream_interrupt_retries 次：收到 StreamInterrupted ⇒ 发 ModelStreamDiscarded ⇒ 用同一请求重发
  其它错误（含 ModelError::Provider）不重试
```

**`StreamInterrupted` 是新的 `ModelError` 变体**（`crates/ys-model/src/error.rs`）。它的 `Display`
前缀与 `Provider` **逐字节相同**，故用户看到的错误文本不变，区分只对程序可见。

为什么必须分开：`Provider` 同时装着「适配器已退避耗尽」的传输失败
（`adapters/model-openai-compatible/src/lib.rs` 的 `(after N attempt(s))`）。循环层若对 `Provider`
一律重试，会把适配器的重试预算再乘一遍 —— 2 次循环额度 × 6 次适配器尝试 = 12 次 HTTP。
现在两者的上界是**相加**：6 + 2。`provider_error_is_not_retried_by_the_loop`
（`basic.rs`）钉住这条，变异（把匹配改回 `Provider`）会让它变红。

`quota` 类错误不在此列：它经 `Fatal` 直上，不经重试。

## 作废信号与消费端

`AgentEvent::ModelStreamDiscarded { reason }` 在重发前发出，`reason` 是适配器给出的错误文本。

| 消费端 | 落点 | 行为 |
|---|---|---|
| TUI | `app.rs` `discard_stream_attempt` | 自尾部向前扫，弹 `Assistant` / `Thinking`，遇任何其它变体即停 |
| `-p` | `main.rs` `PrintBuffer` | 清空累计缓冲；只在 `RunFinished` 一次写出；`RunFailed` 不写 |
| `--json` | 原样旁路 | 事件序列本身是契约，机器消费端自行决定 |
| debug | `model_stream_discarded` 记录 | `reason` 截到 `DISCARD_REASON_MAX_CHARS = 200` |

**`Tool { .. }` 行不在弹除范围**：`AgentEvent::ToolCall` 只在模型响应完整收敛后由
`basic.rs` Step 7 发出，而 `ModelStreamDiscarded` 发生在响应之前 —— 此刻屏上的工具行必属
**已提交的轮次**，弹掉会让已执行的调用凭空消失。`tool_index` 同理**不清理**。

**布局记忆必须与 `md_cache` 一起作废**：`layout::entry_sig` 对正文 / 思考只哈希**长度**，
重放文本与被弹文本等长时 `sync` 会在旧 memo 上命中并 `continue`，屏上留下旧折行结果。
`test_stream_discard_resets_layout_memo`（`app.rs`）用等长异文砸这条路径，删掉
`layout.reset()` 即变红。

## `-p` 的整回合缓冲与截断处理

已写 stdout 的字节不可撤回，故 `-p` 只在 `RunFinished` 写出。代价（本机 1826 轮实测）：
`gen` 段 p50 +2.0s、p90 +11.9s。

这条缓冲带来一个新的失败面：**信道在终局事件之前关闭**时，正文再没有提交时机。
底线行为是「写出已累计的部分 + 报错」：

- `ConsumeOutcome.terminal` 记录是否真见过终局事件（`consume_events` 的信道关闭出口置 `false`）；
- `consume_print_events` 见 `terminal == false` ⇒ 先 `commit` 已累计正文、`flush`，再返回
  `io::Error`（首段 `event stream truncated`）；
- 异常经 `run_print_mode` 冒到调用方 ⇒ `exit_with_run_end(..., 1, ...)`，退出码非 0、stderr 有根因。

`print_mode_reports_truncated_stream_and_still_writes_text`（`main.rs`）钉住这条，
变异（跳过 `terminal` 判断）会让它变红 —— 那种情况下 stdout 空白、退出码 0，是静默丢结果。

## 默认值分层

| 层 | `stream_interrupt_retries` | 理由 |
|---|---|---|
| `RunLimits::default()` / `RunLimits::new()` | `0` | 老调用方行为逐字节不变 |
| app 侧 `resolve_limits`（TUI / `-p` / `--json`） | `2` | 主轮自动恢复 |
| `config.json` | 可覆盖（`0` = 关闭） | 用户可关 |
| 子 agent（`subagent.rs`） | `1` | 既有值，未改 |

## 测试

| 用例 | 落点 | 钉住 |
|---|---|---|
| `describe_error_chain_joins_source_chain` 等 3 条 | `stream.rs` | 链拼接、同文去重、深度封顶 |
| `stream_truncation_error_has_single_provider_prefix` | `lib.rs`（真回环 mock） | 单层前缀 + 链在真实截断流上生效 + 变体是 `StreamInterrupted` |
| `stream_interrupt_retry_emits_discarded_with_reason` | `basic.rs` | 重发一次且作废事件恰一条带 reason |
| `provider_error_is_not_retried_by_the_loop` | `basic.rs` | `Provider` 不触发重试（放大防线） |
| `test_stream_discarded_pops_current_attempt_lines` 等 3 条 | `app.rs` | 弹除范围、已提交工具行保留、布局 memo 同步 |
| `print_mode_writes_deltas_in_order` 等 3 条 | `main.rs` | 整轮缓冲语义、作废后只剩一份、截断时写出并报错 |
| `stream_interrupt_is_retried_and_task_survives` / `_is_bounded` | `subagent.rs` | 子 agent 救回 + 有界 |
| `resolve_limits_reads_stream_interrupt_retries_from_config` | `main.rs` | 默认 2 且配置可覆盖 |

## 与计划的偏差

计划 §4 写着「不改 `ModelError`（加变体是 breaking）」。实际**加了**
`ModelError::StreamInterrupted` —— 让适配器用字符串后缀表达「流中断」再让循环层判文本，
等于把协议塞进人类可读消息里；加变体虽有 `#[non_exhaustive]` 的代价，但它是唯一能把
「谁来重试」讲清楚的结构化载体。`Display` 前缀刻意与 `Provider` 相同，对外文案零变化。
