# Subagent 执行可观测 — 设计与实现规格

## 要解决的两个具体问题

1. **子 agent 崩了就什么都没留下**：转录只在 `run_turn` 返回后一次性写
   （`apps/coding-agent/src/subagent.rs:1123/1190/1225`）。进程被 SIGKILL、OOM、panic，
   或卡在 48 h 的 `General` 派发里 ⇒ `~/.yushan/subagents/` 下一个字节都没有。
2. **harness 提醒完全不可见**：四条提醒（narration / batch / soft-round / final-report）
   注入的是 `request_messages` 这个**局部 Vec**（`crates/ys-loop/src/basic.rs:780`），
   不落 session、不发事件、不打日志 ⇒ 事后无法判断「提醒到底发过没有」。
   这是 `docs/arch/subagent-budget/context.md` 未澄清问题 1 无法回答的直接原因。

## 现状（已核实）

| 项 | 现状 | 指针 |
|---|---|---|
| 子 agent 的 session | `MemorySession`，纯内存 | `subagent.rs:1154` |
| 子 agent 的事件出口 | `CollectingSink`，固定实现 | `subagent.rs:1155` |
| 转录写入 | 一次性 `std::fs::write` | `subagent.rs:576-583` |
| 转录清理 | 7 天 sweep，**不按扩展名过滤** | `subagent.rs:553-573` |
| `AgentEvent` | `#[non_exhaustive]` | `event.rs:20` |
| 提醒注入点 | 4 处，都只写局部 `request_messages` | `basic.rs:781-830` |
| `ModelTextDelta` | 经 `Forwarder` 走 `try_emit`（同步快路径） | `basic.rs:501` |
| `ToolCall` / `ToolResult` / `UserMessage` | 走 `emit()`（异步慢路径） | `basic.rs:278`、`:1080-1100` |
| `ToolResult.content` | `String`，**可能很大**（工具输出） | `ys-core/src/tool.rs:15` |
| 优雅 Abort | `run_turn` 返回 **`Ok`** + `stop_reason=Cancelled` | `basic.rs` Abort 分支 |

**优雅 Abort 与崩溃是两条路径**：前者返回 `Ok`，走 `:1190` 分支，md 照写；
后者根本到不了任何写盘代码。这是本设计的关键分野。

外部消费者对 `AgentEvent` 的 match 绝大多数已有通配臂（`app_loop.rs:2166/2182/4143`、
`main.rs:540`、`round_clock.rs:334`、`tui/app.rs:489`），新增变体的破坏面是编译器指出来的少数几处。

## 决策

### 1. 提醒走事件，不走 session

新增 `AgentEvent::AdviceInjected { kind: AdviceKind, round: u32, threshold: Option<usize> }`。

```rust
pub enum AdviceKind {
    Narration,    // 「闷头干活」提醒
    Batch,        // A4 批处理提醒
    SoftRound,    // A6 轮数软提醒
    FinalReport,  // 收卷轮指令
}
```

**为什么不落 session**：`basic.rs:780` 的「只影响本轮请求」是承重语义 —— 会话要能被
`--resume` 读回，里面不能有「harness 让模型收敛」这类内部消息。事件是旁路，不碰 session。

**`threshold` 四条都有值，不是只有两条**（修正：narration 也有阈值）：

| kind | `threshold` 填什么 | 触发点 |
|---|---|---|
| `Narration` | `narration_silence_tool_calls` | `basic.rs:1547` |
| `Batch` | `batch_nudge_at` | `advance_batch_nudge`，`basic.rs:106` |
| `SoftRound` | 本次触发时的软阈值 | `basic.rs:766` |
| `FinalReport` | `None` | 收卷轮不是阈值触发，是「首次到点」 |

⚠️ **`SoftRound` 与 `Batch` 都有阈值翻倍陷阱**：两者都在触发时执行
`*= 2`（`basic.rs:768`、`:106`）。**必须先存旧值再 emit**，否则记的是翻倍后的值 ——
第一条会记成 `100` 而不是 `50`。这三条各自独立踩坑，实现时任一处漏掉都会让
「提醒是否在预期轮次触发」这个唯一用途失效。

⚠️ **`FinalReport` 的 emit 位置有陷阱**：收卷轮在 **Step 4**（`basic.rs:638`）判定，
而 `rounds += 1` 在 **Step 5**（`:755`）。若在 Step 4 直接 emit，`round` 会**少 1**。
处理方式：在 Step 4 记下 `final_report = Some(cause)` 的同时设置一个待发标志，
在 Step 5 的 `rounds += 1` 之后、四条提醒的 if-else 链**之前** emit。

### 2. 子转录改增量：旁路 journal + 收尾成文

`CollectingSink` 换成 app 侧的 `JournalingSink`：包一个 `CollectingSink`，同时把事件
逐行追加到 `<dir>/<pid>_<seq>.live.jsonl`。

**只在异步 `emit` 路径写盘。** 依据：`ModelTextDelta` / `ModelThinkingDelta` 是
`Forwarder` 通过 `try_emit` 投递的（`basic.rs:501/505`），每 token 一次；而
`UserMessage` / `RoundStarted` / `RoundUsage` / `ToolCall` / `ToolResult` /
`AdviceInjected` / `RunFinished` 全部走 `emit()`。两者恰好是「少量大事件」与
「大量小事件」的分界，所以这条规则不是巧合而是承重。

- `try_emit` 直通内存，**不写盘**。
- 正常结束：写 markdown 转录**成功后**删 `.live.jsonl`。
- md 写失败：**保留 journal**（见决策 5）。
- 崩溃 / 被杀：journal 残留，由既有 7 天 sweep 清掉。

**为什么是 journal 而不是「让子 agent 用 `JsonlSession`」**：后者要把 `tokio::fs` 与
恢复语义引进工具派发路径，并且要重定义「子会话能不能 `--resume`」—— 而子会话**不该**
被恢复（它是一次性派生物，ADR-0022 决策②）。journal 只追加事实，不承诺可恢复。

### 3. `ys-event` 保持零 IO、零 tokio

`JournalingSink` 放 `apps/coding-agent`，不进 `ys-event`。`ys-event` 只加一个纯数据
枚举变体与 `AdviceKind`。

### 4. journal 必须有界（修正：原设计漏了）

`ToolResult.content` 是 `String` 且装着完整工具输出。一次 `General` 派发实测最长
116 轮、单条工具输出可达数十 KB ⇒ journal 会**无界增长**，且与 md 转录**重复写同一批数据**。

三条限制同时生效：

1. **单文件上限 `JOURNAL_MAX_BYTES = 8 MiB`**（照 `debug_log` 的 `DEFAULT_FILE_LIMIT_BYTES`
   先例，取更小的值 —— journal 是临时现场，不是长期档案）。触顶后**停写**并追加一条
   `{"truncated":true}` 标记行，不轮转、不新建文件。
2. **`ToolResult` 正文截断到 `JOURNAL_MAX_TOOL_RESULT_BYTES = 4 KiB`**（照
   `bash.rs` 的 `STDERR_KEEP` 先例）。截断标记为 `…[truncated N bytes]`，原长度记入字段。
   ⇒ 崩溃现场的价值在于「跑到哪、调了什么」，不在于完整输出。
3. **不记录 `ModelTextDelta` / `ModelThinkingDelta`**（它们本来就不走 `emit`）。
   若将来有实现走 `emit` 投递增量，`JournalingSink` 也要**跳过**这两个变体 ——
   否则同样是每 token 一次写盘。

### 5. md 写失败不得销毁 journal（修正：原设计漏了）

`write_transcript` 会失败（`subagent.rs:1191-1195` 有 `Err` 分支，失败时把错误并入诊断）。
**此时若删 journal，等于销毁唯一记录。**

规则：`write_transcript` 返回 `Ok` 才删 journal；返回 `Err` 时保留，并在诊断里补一句
`journal kept at <path>`，让父 agent 知道去哪儿找。

### 6. `## Advice` 节

`transcript_body` 现在只遍历 `session.messages()`，看不见提醒。改为额外接收提醒记录，
在 `## Run` 节**之前**写：

```
## Advice
round=50 kind=soft_round threshold=50
round=100 kind=soft_round threshold=100
```

没有提醒时**整节不输出**（保持既有转录逐字节不变 —— 有测试钉住）。

## 改动清单

| 文件 | 改动 |
|---|---|
| `crates/ys-event/src/event.rs` | 加 `AdviceKind` + `AgentEvent::AdviceInjected` |
| `crates/ys-loop/src/basic.rs` | 4 个注入点各 emit；`SoftRound`/`Batch` 存旧阈值；`FinalReport` 延到 `rounds += 1` 之后发 |
| `apps/coding-agent/src/subagent.rs` | 新 `JournalingSink`（含上限与正文截断）；`transcript_body` 加 `## Advice`；md 成功后删 journal |
| `crates/ys-tui-coding/src/app.rs` 等 | 若编译器要求补臂 —— TUI 一律忽略该事件 |
| `apps/coding-agent/src/debug_log.rs` | 可选：记进 debug log（`advice_injected`） |

## 测试矩阵

| 用例 | 钉住什么 |
|---|---|
| `advice_injected_carries_triggering_threshold` | `SoftRound` 第一条记 `50` 不是 `100`；`Batch` 同理 |
| `narration_advice_reports_silence_threshold` | `Narration` 的 `threshold` 等于 `narration_silence_tool_calls`，不是 `None` |
| `final_report_advice_carries_current_round` | 收卷轮事件的 `round` 与 `RunFinished.rounds` 一致（防 Step 4/5 错位） |
| `advice_not_persisted_to_session` | 触发后 `session.messages()` 无提醒文本（承重不变量） |
| `transcript_renders_advice_section` | 有提醒 ⇒ 有 `## Advice`；无提醒 ⇒ **整节不出现** |
| `journaling_sink_writes_on_emit_only` | `try_emit` 不写盘、`emit` 才写盘 |
| `journal_truncates_large_tool_results` | 超大 `ToolResult` 被截到 4 KiB 且带标记；截断按 UTF-8 字符边界 |
| `journal_stops_at_size_limit` | 超 8 MiB 后停写并留 `truncated` 标记 |
| `journal_kept_when_transcript_write_fails` | md 写失败 ⇒ journal 仍在（不销毁唯一残留记录） |
| `journal_location_reported_only_when_bytes_exist` | 诊断只在 journal 真有字节时给路径，否则说 `journal unavailable` |
| `journal_never_appends_to_existing_file` | `create_new` + 换号：不追加到既有（可能是上次崩溃的）文件 |
| `journal_removed_after_transcript_written` | 正常结束 ⇒ `.live.jsonl` 不存在、`.md` 存在 |
| 变异 | `try_emit` 也写盘 ⇒ 流式用例红；阈值记成翻倍后 ⇒ 首条断言红；Step 4 直接 emit ⇒ round 错位用例红 |

**「崩溃后 journal 仍在」不需要单独测试**：它等价于「进程没跑到删文件那行」，
无法在进程内构造。用 `journal_kept_when_transcript_write_fails` 覆盖同一效果
（都是「没成功写完 md」这条路径）。

## 不做什么

- ❌ 不给子 agent 建可恢复的 session（派生物，不该 `--resume`）
- ❌ 不新增 `EventSink` 实现到 `ys-event`
- ❌ 不把提醒写进 session
- ❌ 不改 `Source` / `Envelope`（子事件仍不进父事件流，ADR-0022 决策②不变）
- ❌ 不做 journal 轮转 / 多文件（单文件上限 + 停写足够，现场不需要完整历史）

## 已知边界

- **`FinalReport` 对 `General` 子 agent 永不触发**：`FinalReport` 受
  `ctx.limits.final_report_prompt.is_some()` 门控（`basic.rs:656`），而只有 `Explore` 派发设置它
  （`subagent.rs:1117`）；`General` 继承父值而父 `resolve_limits` 不给（`main.rs:1134-1151`）⇒
  `AdviceKind::FinalReport` 是 `General` 子 agent 上的死代码。三条阈值提醒
  （narration / batch / soft_round）**对全部子 agent 生效**，取 app 默认值
  12 / 12 / 60（`limits.rs:234`、`config.rs:383`、`config.rs:394`）。
- **journal 的价值是「跑到哪」，不是「完整输出」**：正文截断到 4 KiB。
- 崩溃残留与正常转录共用 7 天保留期，不区分；且 `.live.jsonl` 也在这期间占空间
  （上限 8 MiB/次派发）。
- **优雅 Abort 不留 journal**：它走 `Ok` 分支、md 正常写出，journal 按规则删除。
  现场由 md 提供 —— 这本来就够。
- `AdviceInjected` 会出现在 `--json` 输出里，是新增的可观测面；对既有脚本是增量，
  不破坏已有解析。
- `JournalingSink` 在 `emit` 里做同步写盘，阻塞当前 async 任务。这与
  `debug_log` 的「独立 writer 线程」原则**不一致** —— 取舍理由：journal 事件频率是
  「每轮数个」而非「每 token 一次」，4 KiB 级 `write_all` 在 SSD 上约 10–50 µs，
  相对轮次间隔（秒级）可忽略。若将来实测成为瓶颈，再改为 channel + 独立线程。
