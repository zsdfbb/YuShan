# 流中断恢复：根因可见 + 主轮自动重试

> 日期：2026-11-03 ｜ 编排：quick（设计内联，不单独走 arch-*）

## 1. 需求

1. 遇到 provider 中途断流时**自动重试几次**，不要整回合失败。
2. 断流时的错误信息要能**快速看出根因**（当前只剩 `error decoding response body` 一句，看不出是对端 RST、body 截断还是别的原因）。
3. `-p` 的输出语义改为**拿到完整结果才输出**（用户明确要求）。

## 2. 现状（已核实）

| 事实 | 证据 |
|---|---|
| 适配器「已吐 delta 就不重试」 | `adapters/model-openai-compatible/src/lib.rs:501-516` |
| 循环层已有「整次尝试作废 + 同请求重发」 | `crates/ys-loop/src/basic.rs:827-845`（`stream_interrupt_retries`） |
| 作废信号 `ModelStreamDiscarded` 已定义、已发出 | `crates/ys-event/src/event.rs:95-100`、`basic.rs:838` |
| **但三个消费端都没接这个信号** | TUI `app.rs` 走 `_ => {}`；`-p` 的 `print_delta_text` 对它返回 `None`；`--json` 只序列化 |
| 主轮默认关闭 | `crates/ys-component/src/limits.rs:218`（默认 `0`）；子 agent 由 `subagent.rs:939` 强制设 1 |
| 根因被丢弃 | `stream.rs:144` 只取 `e.to_string()`（reqwest `Display`），`source()` 链未带出 |
| 双重前缀 | `lib.rs:711` 用 `error.to_string()`（已含 `model provider error: `）当 reason，`:511` 再包一层 `Provider` |

## 3. 决策

**D1 —— 重试留在循环层，适配器不放开抑制。**
适配器仍拒绝重试（它不知道「屏幕能否擦」）。循环层的 `stream_interrupt_retries` 才是正确落点：
它丢弃整次尝试、用同一请求重发，且失败分支不 `append`（会话幂等）。
`ModelEvent` **不加新变体** —— 不需要新信号，作废信号已存在。

**D2 —— 作废信号带原因。**
`AgentEvent::ModelStreamDiscarded` 加 `reason: String`，内容为触发重发的适配器错误文本。
理由：事后要能区分「provider 断流」与「空闲超时」，否则重试在日志里不可归因。

**D3 —— 各消费端对作废信号的处理**

| 消费端 | 处理 | 理由 |
|---|---|---|
| TUI | 丢弃本轮已渲染的 Assistant / Thinking 行 | 有 `Vec<TranscriptLine>`，可弹；重放后不会出现两遍 |
| `-p` | **整轮缓冲**，`RunFinished` 时一次写出；`RunFailed` 时不写 | 已写 stdout 的字节不可撤回；用户明确要「拿到结果才输出」 |
| `--json` | 原样旁路，额外落一条 debug 记录 | 机器消费端自己决定；事件序列本身是契约 |

`-p` 的代价（本机 1826 轮实测）：`gen` 段 p50 +2.0s、p90 +11.9s。

**D4 —— 主轮默认重试 2 次，可配置。**
`resolve_limits` 从 `config.json` 取 `stream_interrupt_retries`（默认 2）；库面默认保持 `0`（老调用方行为不变）。
子 agent 保持 1 次（既有值，不动）。

**D5 —— 根因走 `source()` 链。**
`stream.rs` 新增 `describe_source_chain`，错误串形如
`error decoding response body ← unexpected EOF`；同时去掉 `lib.rs` 的重复前缀。

## 4. 不做什么

- 不改 `ModelError`（非 `#[non_exhaustive]`，加变体是 breaking）。
- 不改 `Model` trait 签名。
- 不做「非流式回退」（既有注释明确不做）。
- 不动适配器内部退避（`max_attempts=5`）—— 它管传输层，与流中断重试分层叠加。

## 5. 验收（测试合同）

| # | 场景 | 方式 | 优先级 |
|---|---|---|---|
| T1 | `source()` 链拼接出底层原因 | 单测（`stream.rs`） | 必测 |
| T2 | 流错误串**不含**重复的 `model provider error:` | 单测（`lib.rs`） | 必测 |
| T3 | 主轮 `stream_interrupt_retries` 默认 2，且配置可覆盖 | 单测（`main.rs`） | 必测 |
| T4 | TUI 收到作废信号后本轮 Assistant / Thinking 行消失 | 单测（`app.rs`） | 必测 |
| T5 | `-p` 在失败回合**零输出**、成功回合输出完整文本 | 单测（`main.rs`） | 必测 |
| T6 | 断流一次后重试成功 ⇒ 回合 `Completed`（主轮路径） | 单测（`basic.rs` 或 app_loop） | 必测 |
| T7 | 作废信号进 debug 记录 | 单测（`debug_log.rs`） | 可推迟 |
| T8 | 真实 provider 断流恢复 | 手工 | MANUAL_ACK_REQUIRED |

## 6. 变异测试要求

- 把 `resolve_limits` 里的重试数改回 `0` ⇒ T3 变红。
- 去掉 TUI 的行回退 ⇒ T4 变红。
- `-p` 改回边收边写 ⇒ T5 变红。
- `describe_source_chain` 改成只取 `Display` ⇒ T1 变红。
