# 0054 — Subagent 运行状态：进程级状态板 + 推送式进度，不新开 `Outbound` 变体

给「主 agent / TUI 看得到 subagent 现在在做什么」定下形态：① **每次派发一条进程级状态记录**
（`SubagentBoard`，与 `PendingResults` 并列、同形共享）；② 采集点唯一 —— 子 agent 的
`EventSink` 换成 `ProgressTap` 包装（**不改 `ys-event` 的 trait**）；③ 进度以 **`AgentEvent`
新变体**推送（**不加 `Outbound` 变体**）；④ 状态机**四态**，把「已完成」与「已送达」分开；
⑤ 句柄改为**全局唯一** `sa{turn}.{n}`；⑥ 落实 ADR-0029 §3.4 的欠账（产物路径先分配 + 按轮
append）；⑦ 可选只读工具 `subagent_status`，**无 wait 语义**。

## Status

**proposed（设计已定，代码未落）**（2026-10-25；设计见
[`subagent-progress/design.md`](../arch/subagent-progress/design.md)，
现状基线见 [`context.md`](../arch/subagent-progress/context.md)）

> ⚠️ **本 ADR 描述的是目标形态，不是 as-built。** 本轮的落地范围只包含
> [ADR-0055](0055-subagent-continuation.md)（父回合等得起）所必需的那一部分：
> `LiveSubagents`（进程级在跑计数 + RAII 守卫）。全文其余部分 —— 状态板
> `SubagentBoard` / `ProgressTap` / `AgentEvent::SubagentProgress` /
> `subagent_status` 工具 / 增量落盘 / 全局句柄 —— **代码零行**。
>
> 全仓 grep `SubagentBoard` / `ProgressTap` / `SubagentProgress` **均无命中**。
> 施工前先看 [`subagent-continuation/review.md`](../arch/subagent-continuation/review.md)
> 与 [`design.md`](../arch/subagent-continuation/design.md) §7（它们解释了为什么降级到
> P1/P2）。

## 背景

四件事同时成立，缺一条本 ADR 的形状就会变：

1. **`subagent_async` 的工具行在启动后立刻闭合** —— `runner.spawn()` 立刻返回句柄，`ToolResult`
   当场返回（`apps/coding-agent/src/subagent.rs:1293`）。此后父侧事件流里**再没有任何东西**
   指向这次派发。
2. **子 agent 的事件不进父流**（[ADR-0022](0022-subagent-dispatch.md) 决策 2），而唯一的采集点
   `CollectingSink`（`subagent.rs:801`）**跑完才被读**（`:816`）⇒ 跑着的时候零可观测。
3. **产物跑完才创建** —— 三处 `write_transcript` 全在收尾分支（`:769` / `:836` / `:865`），
   路径也在那时才分配（`:356-360`）。这与 [ADR-0029](0029-subagent-landing-decisions.md) §3.4
   写明的「路径先分配 + 按轮 append」**直接矛盾** ⇒ 没有旁路可 tail。
4. **句柄跨回合重复** —— `format!("sa{}", dispatched)`（`:595`，注释自认「不需要全局唯一」），
   runner 每回合新建（`app_loop.rs:1245`）⇒ 计数器从 0 重来。任何按句柄索引的状态结构都会串号。

另有一条**当前无法遵守的提示词**：`prompt.rs:321-332` 要求「never claim a dispatched sub-agent
is still running **without checking**」，而模型没有任何检查手段。

## 决策

### 1. 进程级状态板，与 `PendingResults` 并置

`SubagentBoard` 住在 `apps/coding-agent/src/subagent_board.rs`，以 `Arc` 经 `SubagentSetup`
跨 `/resume` 重建共享（与 `results` 逐字同形，`subagent.rs:444-460` / `assembly.rs:66-70`）。

- **进程级、不按会话分片**：与 `PendingResults` 同一理由（`subagent.rs:258-262`）—— 在跑的子
  agent 不随会话重建。`/new` / `/resume` **不清理**板。
- **有界**：`MAX_BOARD_ENTRIES = 32`（与 `MAX_PENDING_RESULTS` 同量级）。超限淘汰**最旧的已终态**
  条目；**全是 `Running` 时不淘汰**（在跑的东西不能被显示层忘掉），只记一行日志。上界由
  `DEFAULT_MAX_DISPATCHES_PER_TURN = 16` 保证。
- **唤醒用 `Notify` 而非轮询**：`BackgroundTasks` 的 100ms sweeper 是为收子进程（`app_loop.rs:79`），
  本需求没有进程要收。tokio 的 `notify_one` 在无 waiter 时保存一个 permit ⇒ 「通知先于等待」
  不丢；多次通知合并无害（drain 是全量取走）。

### 2. 四态：完成 ≠ 送达

```rust
enum SubagentState { Running, Completed{..}, Failed{..}, Delivered{..} }
```

「已完成」与「已送达」**必须分开**：结果先进 `PendingResults`（`subagent.rs:234-256`），真正进
会话是在 `BasicLoop::drain_boundary`（`crates/ys-loop/src/basic.rs:102-111`），两者**可以差一整
回合**（回合结束后才跑完的子 agent 就是这种，`subagent.rs:569-577` 有实测记录）。
用户要的「TUI 展示主 agent 收到了通知」**正是这个差值** —— 折叠成两态就等于抹掉它。

`Delivered` 的判定点唯一：app_loop 转发 `AgentEvent::DeferredResult{handle}` 时置位
（`app_loop.rs:1281` 的转发分支），**不另造事件**。

### 3. 采集点唯一：`ProgressTap` 包装 `EventSink`

`EventSink` 的契约允许任意 wrapper（`crates/ys-event/src/sink.rs:11`：只有 `&mut self` 方法、
无 `Clone`/`&self` 要求），而 `CollectingSink` 本来就被 `&mut` 反复传（`basic.rs:507` / `:937`）。

- **`try_emit` 也必须观察**：`ModelTextDelta` 走同步快路径（`basic.rs:309`，`Forwarder` 直调），
  只在 `emit().await` 里观察会漏掉全部正文增量。
- **`inner: CollectingSink` 保留**：`raw_tool_results` 仍从它取（`subagent.rs:816`）—— 不在
  `ProgressTap` 里另存一份事件，避免两份真值。
- **只观察四个变体**：`RoundStarted`（`:494`）、`ToolCall`（`:880` / `:970`）、
  `RoundUsage`（`:562`）、`ModelTextDelta`（`:309`）。`ToolResult`（`:941`）**不带工具名** ⇒
  「当前工具」只能从 `ToolCall` 取；板回答「在干什么」，不回答「干出了什么」（后者读转录）。
- **按轮聚合**：`observe` 只更新 `live`，**不入表**；入表由 `flush()` 做，且同一 handle 在
  `pending` 里已有未取走条目时**原地合并**。`flush` 只在 `RoundStarted` 与收尾/Drop 触发 ⇒
  **每个子 agent 每轮至多一条事件**（与父 agent 自己的 `RoundStarted` 同阶）。不聚合的话，一个
  跑 50 次工具的子 agent 会往 TUI 与 `--json` 推 50 条。

### 4. 通道用 `AgentEvent`，**不加** `Outbound` 变体

三条证据：

1. **回合进行中只有 `Outbound::Event`**：`Outbound::View` 只在启动期模型目录到达
   （`app_loop.rs:385-392`）与**每个 Request 处理完之后**（`:799-806`）推。想显示「还在跑」必须走事件。
2. **`AgentEvent` 加变体零成本**：`#[non_exhaustive]`（`crates/ys-event/src/event.rs:19`），四处
   兜底现成 —— `-p` 的 `_ => None`（`main.rs:542`）、`RoundClock` 末臂（`round_clock.rs:300`）、
   debug 的 `_ => return None`（`main.rs:751`）、TUI 的 `_ => {}`（`app.rs:460`）。`--json` 自动多出行。
3. **`Outbound` 加变体会打红一条有意写成的不变量**：`app_loop.rs:2216-2219` 断言「回合期间只应有
   事件（唯一的 View 在末尾）」。新 `Outbound` 变体在回合内出现 ⇒ 该断言红。

**app_loop 首次成为事件生产者**（现状只转发，`Envelope::new` 全仓只在测试里出现）：drain 到板上
进度时自造 `Envelope::new(Source::agent(), turn, SubagentProgress{..})` 推给 `out_tx`。
- **不让子任务自己发**：`ChildTask` 是 `'static`、拿不到父的 `&mut dyn EventSink`；要发就得把
  `mpsc::Sender` 克隆进去（出站信道知识渗进 `subagent.rs`），且 `--json` 会看到乱序（子异步发、
  父同步发）。app_loop 单点发现 ⇒ **顺序天然与 drain 同点**。
- **不新增 `Source::subagent()`** —— 那正是 ADR-0022 决策 2 关掉的门。语义是「父侧对一次派发的观测」，
  不是子 agent 的事件。

drain 有**两个**调用点（缺一不可）：
- 回合内：`app_loop.rs:1267` 的 `select!` 加一臂；
- 回合外：`app_loop.rs:357-363` 加一臂 + `build_view` + `Outbound::View`（照 `Wake::Models`
  的现成形态 `:372-392`）。缺它则「回合已结束、子 agent 仍在烧 token」时 TUI 显示**完全空闲** ——
  恰是本需求要消除的反例。

### 5. 句柄全局唯一：`sa{turn}.{n}`

`AppSubagentRunner` 增 `turn: u32`（`make_runner` 入参）。现状的两个计数器都现成：`dispatched`
（`subagent.rs:389`）+ 回合号（`app_loop.rs` 的 `turns`）。

- **不用进程级原子计数器**：`sa17` 跨会话不可读；`sa{turn}.{n}` 让用户/模型一眼对上「第几回合派的
  第几个」。跨会话同号是良性的（`turns` 在会话内单调，两条不会同时存在于板上）。
- 副作用面仅文案：`SubagentAsyncTool` 回执（`:1294-1299`）、`DeferredResult` 的 TUI 行
  （`app.rs:442`）、淘汰日志（`:244`）。

### 6. 落实 ADR-0029 §3.4：路径先分配 + 按轮 append

`SubagentsDir::allocate() -> io::Result<Artifact>`；`Artifact { path, file }` 提供 `append_round`。

- **路径在 `tokio::spawn` / `execute` 之前分配**，与句柄一起进板 ⇒ 跑着的子 agent**有文件可 tail**。
- **append 时机 = `flush()`**（与 §3 同一个节拍）⇒ 只有一个写入点。
- 收尾仍写完整转录（`transcript_body` 形状不变），`write_transcript` 的三个调用点**收敛成一个**。
- **顺带收益**：外闸真响（`drop` future）时盘上已有前若干轮 —— ADR-0029 §3.4 的第 3 层兜底
  第一次真正成立。

### 7. 只读工具 `subagent_status`（**无 wait 语义**）

`subagent_status(handle?: string)` 返回每个派发一行；`is_concurrency_safe() == true`
（与 `read` / `grep` 同类，走 Step 9 并行准入）。

**为什么不带 `wait`**：`bash_poll` 的死因写在 `docs/arch/bg-notify/design.md` §1 ——
16 次启动 / 68 次 poll / 54 次（79%）返回零新信息 / 单句柄 24 次。带 wait 的查询工具必然退化
成轮询。本工具**不承担完成通知**（那是推送的活），只在模型「确实要判断现在能不能换任务」时回答一次。

**代价（精确记账）**：
- 父表 12 → 13 元；**子表前缀漂移 0 字节**（追加末位，`assembly.rs:184-185` 的既有契约）；
- 父 tools JSON 约 +500 字节（`SubagentTool::spec` 同量级）—— 进固定前缀，**命中缓存**；
- 装配断言改五处：`assembly.rs:353`（12 → 13）、`:356-358`、`:379-390`、`:416-419`、`:421-424`；
- 提示词两处：`tool_snippet`（`prompt.rs:161-192`）+ `tool_guidelines`；
- `fixed_prompt_tokens` **自动重算**（`main.rs:1074-1077` 现算），不改常量。

### 8. TUI：状态行一段 + 三种转换行，不新增 `TranscriptLine` 变体

- **状态行**（`draw.rs::status_text:599-638`）：段序插入在「↓ 有新内容」之后、「第 N 轮」之前 ⇒
  比轮号**更不易被剥掉**（剥除是从尾部，`:631-636`）。0 条时**整段不 push** ⇒ 逐字节不变
  （与 `draw.rs:314-316` / `:425-427` 的「空就不画」同形）。
- **转换行**只落 `started`（仅异步派发）/ `finished` / `failed`；`Round` / `Tool` 中间进度
  **只更新状态表、不落行**（否则 50 次工具 = 50 行洪泛）。同步派发不落 `started` 行 ——
  它本来就有 `Tool` 行在跑（`app.rs:370-382`）。
- **不新增 `TranscriptLine` 变体**：会打红三处编译（`draw.rs:759-768` 的 `LineClass::of`、
  `draw.rs:864-1002` 的 `push_non_assistant`、`layout.rs:556-586` 的 `entry_sig`）并被四处
  `_ =>` **静默**漏掉（`draw.rs:790-796` / `:773-779`、`commands.rs:543-548`、
  `transcript.rs:142` 的 `replay_lines`）。`System` / `Error` 够用，且这些行与 `Summary` /
  命令输出一样**不持久化**（`/resume` 回放里不出现）—— 与既有语义一致，不是缺陷。
- **送达行改写**：`↓ {handle} finished: {head}`（`app.rs:442`）→
  `↓ {handle} 结果已注入会话（第 N 轮）· completed · 7 轮 · 96s` + 摘要首行。保留「只显示首行」
  （摘要有 8 KiB 上限，整段会刷屏 —— `app.rs:773-804` 钉住了它）。
- **`CodingView` 加 `subagents: Vec<SubagentRow>`**：回合外也要显示，而回合外只有快照可用。
  视图类型归产品 TUI crate（`view.rs:1-7`），不进 `ys-protocol`。

### 9. 模型侧：结构化头 + 两条例 guideline

- **完成消息加定长头**：`[sa3.2] completed · 7 rounds · 12 tool calls · 96s · 12.3k↑ 2.1k↓`。
  为此 `SubagentOutcome` 增 `rounds` / `tool_calls` / `elapsed_ms`（非 `#[non_exhaustive]` ⇒
  **breaking**，同步两处 fake 与三处构造）。头在**截断之后**拼 ⇒ `MAX_SUMMARY_BYTES = 8 KiB`
  的摘要正文上限不变（总量 = 8 KiB + 定长头，注释写明）。
- **合成一份渲染函数**：现状 `render_outcome_text`（`subagent.rs:624-642`）与
  `SubagentTool::call` 里的拼装（`:1209-1224`）是**两份实现**（注释自称「不漂移」）。合并为
  `render_dispatch_result`，两条路径都调它。
- **提示词**（`prompt.rs:321-332` 的 `subagent_async` 段内，插在 `subagent` 段之前 ——
  因为 `subagent_guideline_block`（`prompt.rs:1169-1179`）用「成本提醒」当**段尾锚**）：
  1. 替换 G4 那句无法遵守的：改为「要查就跑 `subagent_status`，但**不要循环调用、不要用它等**」；
  2. 新增「收到结果后先说一句它发现了什么、你接下来做什么，再继续」。

## Considered Options

- **不建状态，用既有事件反推**（否决）：`subagent_async` 的 `ToolResult` 立刻返回 ⇒ 工具行当场
  闭合；子事件不进父流 ⇒ 父侧事件流里根本没有子 agent 的进展信号。只能回答「派过没有」，
  回答不了「还在不在跑」。
- **子事件进父流 + `Source::subagent(id)`**（否决）：ADR-0022 决策 2。三个消费者都不读 `source`
  ⇒ 会让 `-p` 的 stdout 吐子正文、`--json` 提前收尾、TUI 的 `is_turning` 被子的 `RunFinished`
  误关。ADR 里写的翻案条件是「出现要在 UI 上看到子 agent 逐条工具调用的真实需求」—— 本轮够不到。
- **新 `Outbound` 变体**（否决）：见决策 4 第 3 条。
- **带 `wait` 的查询工具**（否决）：见决策 7。
- **TUI 右侧栏 roster**（本轮否决、留下次）：要改**两处**纵向切分（`draw.rs:153-158` 与
  `draw.rs:188-195` 的 `chat_area`），二者不同步会让滚动与画面错位（`app.rs:547` /
  `events.rs:368` 消费后者）。收益本轮不需要，且 `long-horizon-todo` 的侧栏在等同一处改动，
  宜合并做。
- **新 `TranscriptLine` 变体**（否决）：见决策 8。
- **`UsageTotals` 改成按句柄精确账单**（否决）：`drain()` 的一次性清零是**故意的**（回合末上卷，
  `app_loop.rs:1336-1341`）。板上的 `live.input_tokens` 是覆盖写的近似值，够回答「它烧了多少」；
  精确账单单属「成本聚合」独立项（`docs/TODO.md` §A3 仍是欠账）。

## Consequences

- `SubagentOutcome` 加三个字段（breaking）；`ys-event` 加一个事件变体与两个数据类型。
- `AgentEvent::SubagentProgress` 在 TUI / `--json` / debug 里都会出现；它是**非终局**事件，
  不影响任何消费循环的退出条件。
- app_loop 成为事件生产者（`Envelope::new` 首次出现在生产路径）。
- 子 agent 跑着的时候盘上就有转录文件增长（新行为，也是 ADR-0029 §3.4 的原意）。
- 句柄格式变化（`sa1` → `sa3.1`）—— 只影响文案与 debug 可读性。
- 父表 12 → 13 元（`subagent_status`），父 tools JSON 约 +500 字节；子表不变。
- 空闲态 TUI 会因 `SubagentProgress` 刷新 `Outbound::View`（此前空闲态只有请求驱动的刷新）。

## 参考

- [ADR-0022](0022-subagent-dispatch.md) — subagent 派发形态（子事件不进父流）
- [ADR-0029](0029-subagent-landing-decisions.md) — 落地裁定（§3.4 增量落盘）
- [ADR-0044](0044-subagent-observability-and-debug-defaults.md) — 失败诊断与转录口径
- [`subagent-progress/design.md`](../arch/subagent-progress/design.md) — 分刀与验收
- [`bg-notify/design.md`](../arch/bg-notify/design.md) §1 — 为什么查询工具不能带 wait
- `apps/coding-agent/src/subagent.rs` / `subagent_board.rs` / `app_loop.rs`
- `crates/ys-tui-coding/src/{draw,app,view}.rs`
