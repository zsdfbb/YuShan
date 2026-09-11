# coding-agent TUI 显示修复 — 设计方案

## 背景与已核实根因

承接 `context.md`（tui-redraw-residual）。三个 bug：

| # | 症状 | 根因（已核实） |
|---|------|----------------|
| 1 | turn 期间界面零刷新 | `ui/mod.rs` event_loop 在 events 分支内联 `dispatch_input(...).await`（:131），外层 select 在 turn 期间整体挂起；`run_turn_with_ticks` 内层 tick 为空（:272），turn 期间零 `terminal.draw()` |
| 2 | 退出后残影残留 | `restore_terminal`（:91-103）无清理；crossterm `LeaveAlternateScreen`=`\x1b[?1049l` 只切回主屏不清 alt 缓冲区，终端把 alt 最后帧并入 scrollback |
| 3 | exit/quit 需二次按键 | `should_quit` 检查在 `dispatch_input` 之前（:127-129），而 `/quit`/`exit` 在 dispatch 内才置位 |

## 关键事实

1. **现有 turn 取消路径是死的（源码级验证）**：crossterm Unix raw mode 用 `cfmakeraw`（清 `ISIG`）→ ^C 是 crossterm KeyEvent 而非 SIGINT → 内层 `tokio::signal::ctrl_c()`（mod.rs:274）永不触发；而识别 Esc/Ctrl-C 的 `handle_event`（events.rs:39-67，已路由到 `cancel_token.cancel()`）只在被阻塞的外层 poll。commit 645e339 的意图在真实终端未生效。
2. **turn 期间不能也不需重建 `AppView`**：`turn_fut` 独占 `&mut agent` → `AppView::from_sources`（参数 `&Agent`）是 E0502；`TurnStats::record` 只在 turn 结束调用（tokens/turn_count 冻结）；唯一实时变化的是 session 秒表（draw 时派生）。turn 内不重建快照既是借用所迫也是数据所然。
3. **旧 `is_turning` tick 重建是死代码**：外层 select 在 turn 期间必然阻塞，`is_turning==true` 永不会被外层观察（mod.rs:134-145 化石）。

## 代码库 + 本地 crate 源码验证（网络不可用）

| 事实 | 出处 |
|------|------|
| `?1049l` 切回主屏不清 alt 缓冲区 | crossterm-0.28.1/src/terminal.rs:258-260 |
| `Terminal::clear()` = `ClearType::All`（全屏 viewport） | ratatui-0.29.0/src/terminal/terminal.rs:471-473 |
| raw mode 关 ISIG（^C 不是 SIGINT） | crossterm-0.28.1/src/terminal/sys/unix.rs:291 + POSIX `cfmakeraw` |
| `Agent::cancel_handle()` 返回 owned clone（`&self`），专为跨借用取消设计 | crates/agent-runtime/src/agent.rs:100-113 |
| `run_turn_with_ticks` 三路 select 已证明 `&mut turn_fut` 作 select 一路可行 | mod.rs:264-278 |
| draw.rs 已有 TestBackend + `make_app()`/`render_to_text` 可绕开 Agent/网络测渲染 | draw.rs:209-351 |

## 调研补充：tmp/pi 参考实现（同问题的成熟 TypeScript coding-agent TUI）

`tmp/pi`（`packages/tui` + `packages/coding-agent`）是仍在维护的同类 TUI，其渲染与退出模型与本问题直接对应，作为方案依据：

| pi 的做法 | 出处 | 对本次设计的启示 |
|---|---|---|
| **渲染按需驱动，独立渲染循环不存在**："Only rebuild a layout frame after `requestRender()` … no independent layout loop"；`requestRender()` = dirty 标志 + `process.nextTick` 节流（`MIN_RENDER_INTERVAL_MS=16`）合并，输入事件 `renderNow` 抢占 | tui-plan.md:385-388；tui.ts:944-1005 | 内层 turn 循环不要盲目 100ms/500ms 定时画；改为"状态变化即画 + 一个 spinner 节拍"。外层 idle 纯事件驱动（我们已删外层 tick 死代码，方向一致） |
| **动画 = 组件内定时器，仅在动画激活时运行**：`loader.ts` spinning（`setInterval` 步进帧 + `requestRender`，start/stop 随工作态）；armin/countdown-timer 同模式 | packages/tui/src/components/loader.ts:49,77-82 | turn 停顿时仍要视觉心跳 → 用"Working"三点动画定时器（300ms），turn 结束即停；不是全局节流器 |
| **叶子渲染缓存（by content+width），不引框架级缓存**："rely on existing leaf render caches … Do not introduce a second framework-level render cache initially" | tui-plan.md:349-376 | transcript 长会话绘制成本来自每帧 `line_to_text`+wrap 全量重算；按 TranscriptLine 缓存渲染行是正解（见性能节），而非简单降频 |
| **退出打印完整逻辑文档**："Leaving alt mode must print a complete logical final document · do not use only the last visible frame as the exit document"；`afterTerminalStop` 默认分支 = `EXIT_ALT_SCREEN` 后按行 `\r\x1b[2K`（逐行清屏）+ 重打全树 unbounded 文档进主屏 scrollback；`preserveScreen` 才只 `EXIT_ALT_SCREEN` | tui-plan.md:677-688；tui-alt-screen.ts:381-407 | **bug2 用户已选定此策略**：退出清行并重打完整对话，对话留在 scrollback、无残影、无 padding 行 |

否决的 pi 点（不适用）：布局树/裁剪/命中测试/VStack/HStack/ScrollView 体系（ratatui 无组件树概念，差异过大）；Kitty 图片/鼠标选择等边缘能力（范围外）。

## 需求确认（已与用户确认）

- **刷新粒度 = 最小**（用户选定）：turn 期间只需 ①提交行立即可见 ②input 框 Working… 动画 ③status 心跳（若有 status 面板）；assistant 输出 turn 结束一次性显示。不做流式、不做逐 tool call、不碰 agent-loop/agent-model。
- **退出策略 = 打印完整对话（pi 风格）**（用户选定）：退出 alt-screen 时逐行清主屏并把全部 transcript 重打进 scrollback，对话可回滚，无残影。
- **显示面板组件化，默认仅对话窗口**（用户选定，折进本设计）：默认 TUI = 对话窗口（transcript + input）两块核心；footer / status 面板为**可选挂载**（`App` 字段 `show_footer`/`show_status`，默认 `false`）。不做动态插件（v0 红线，见非目标）。

## 候选方案（3 个 subagent 并行产出）

### X1 最小阻塞：内层只画（Agent 1）
`dispatch_input`/`run_turn_with_ticks` 加 `&mut Terminal<B>` + `&mut App`，内层 tick 直接 `terminal.draw`；保留内层 `ctrl_c` arm。
- **优点**：diff 最小。
- **代价**：turn 中事件仍不处理 → Esc/Ctrl-C 取消依旧假死（其依赖的 `ctrl_c` 恰是死路径，X1 误以为生效）。

### X2 阻塞 + turn 内事件 arm（推荐形态）
同 X1 的阻塞 dispatch，但内层 select 换成 `turn_fut | events | spinner`，删死掉的内层 `ctrl_c`。事件路径统一处理 Esc/Ctrl-C/Ctrl-D（修复假死取消）；渲染按需 + spinner 节拍（pi 模型）。
- **获得**：取消真生效；turn 中事件可见；渲染按 state-change 驱动 + 动画节拍，成本受控。
- **代价**：比 X1 多一个 `&mut EventStream` 贯穿。

### X3 两相状态机 + 输入队列（Agent 2，可扩展优先）
外层拆 Idle/Turning，turn 循环提升为独立 `drive_turn`；`TurnState` 枚举、`classify_submit`、`pending_queue` 队列。
- **优点**：borrowck 静态禁止 turn 中碰 agent；为流式/逐 tool/并行输入预留三处接缝。
- **代价**：净增 ~90 行 + 状态机 + 队列；turn 中排队输入**超出"最小"范围**。否决（留作未来接缝）。

## 对比矩阵

| 维度 | X1 内层只画 | **X2（推荐）** | X3 状态机 |
|------|:---:|:---:|:---:|
| 实现复杂度 | ~40 行/3 签名 | ~55 行/4 签名 | ~90 行/状态机+队列 |
| turn 内 Esc/Ctrl-C 取消 | ❌ 仍假死 | ✅ 修好 | ✅ 修好 |
| turn 内事件处理 | ❌ 仅排队 | ✅ 可见处理 | ✅ 队列 FIFO |
| 渲染模型 | 定时全量重绘 | **事件驱动 + spinner**（pi） | 同上 |
| 代码库风格契合 | 高 | 高 | 中（外层重排） |
| 演进接缝（流式） | 需再重构 | 加 event channel 即可 | 已内建 |

## 推荐方案 D（X2 形态 + pi 渲染/退出模型）

### 形态
保持 blocking 的 `dispatch_input` 调用链，但 turn 期间由一个**自渲染的内层三路 select** 驱动：`agent.run_turn` future + crossterm 事件 + "Working" 动画节拍。取消统一走事件路径，内层 `tokio::signal::ctrl_c` 删除；渲染在状态变化时发生（事件/入场/turn 完成），不加全局定时器。退出时打印完整对话文档。

### 改动清单（全部在 `apps/coding-agent/src/ui/`，不碰 agent-loop/agent-model）

**① `mod.rs` event_loop（:121-146）—— 外层纯事件驱动，删死代码**
```rust
async fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    app: &mut App,
    session_started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;
    let mut events = EventStream::new();

    loop {
        terminal.draw(|f| draw::ui(f, app))?;
        match events.next().await {
            Some(Ok(event)) => {
                events::handle_event(event, app)?;
                if let Some(input) = app.take_submitted() {
                    dispatch_input(terminal, input, &mut events, app,
                                   agent, config, commands, stats, state_store).await?;
                }
                if app.should_quit { break; }    // ← bug3：检查移到 dispatch 之后
            }
            Some(Err(_)) | None => break,        // 事件流错误/关闭即退出
        }
    }
    Ok(())
}
```
- 外层 tick（:134-145 `if app.is_turning { from_sources }`，死代码）**整体删除**——外层无 select、无定时器，纯事件驱动（pi "no independent layout loop"）。
- `events` 传给 `dispatch_input`（turn 内复用同一事件流）；`event_loop` 改收 `&mut App`，退出后 `run()` 仍能读 `app.transcript` 打印。

**② `mod.rs` run_turn_with_ticks（:254-279）—— 内层三路 select + 真渲染 + 真取消**
```rust
async fn run_turn_with_ticks<B: Backend>(
    terminal: &mut Terminal<B>,
    events: &mut crossterm::event::EventStream,
    agent: &mut Agent,
    input: agent_loop::AgentInput,
    cancel_token: CancelToken,
    app: &mut App,
) -> Result<agent_loop::RunResult, Box<dyn std::error::Error>> {
    use futures::StreamExt;
    let mut ticker = tokio::time::interval(Duration::from_millis(300)); // Working 动画节拍
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut turn_fut = Box::pin(agent.run_turn(input)); // &mut agent 独占至此

    terminal.draw(|f| draw::ui(f, app))?;               // 入场即画：提交行 + Working… 立即可见

    loop {
        tokio::select! {
            biased;
            res = &mut turn_fut => {
                return res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>);
            }
            Some(Ok(ev)) = events.next() => {
                events::handle_event(ev, app)?;          // ← 事件路径：Esc/Ctrl-C→cancel；Ctrl-D→cancel+should_quit
                if app.should_quit {
                    cancel_token.cancel();               // Ctrl-D 顺带取消进行中的 turn
                }
            }
            _ = ticker.tick() => {
                app.working_dot = (app.working_dot + 1) % 3;  // "Working." / "Working.." / "Working..."
                terminal.draw(|f| draw::ui(f, app))?;
            }
        }
    }
}
```
- `biased` 保持 turn 完成优先（取消后 round 边界返回 `Ok(stop_reason: Cancelled)`，不 drop future，走 Ok 分支——语义与 mod.rs:249-253 一致）。
- **删除**内层 `tokio::signal::ctrl_c()` arm（死路径，见事实 1），取消全部经事件流。
- ticker **只在 turn 期间存在**（turn 结束返回即释放）——pi loader.ts 模式，动画激活才跑，非全局节流。

**③ `mod.rs` dispatch_input（:152-160, :212）**
- 签名加 `terminal: &mut Terminal<B>`、`events: &mut EventStream`，整个函数泛型化 `<B: Backend>`（与 event_loop 一致，TestBackend 可测）。
- turn 调用点 `run_turn_with_ticks(terminal, events, agent, turn_input, agent.cancel_handle(), app)`。
- turn 开始前已有 `transcript.push(User)`（:177）+ `is_turning = true`（:208），进入即被 ① 的首帧 draw 渲染。

**④ `mod.rs` restore_terminal（:91-103）+ 新函数 —— bug2（用户选定 pi 风格）**
`restore_terminal` 增 `app: &App` 参数；退出序列为 pi-style"清行 + 打印完整文档"：
```rust
fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &App,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::execute;
    let width = app.view.cwd.as_os_str().len().max(80); // 为紧凑起见选固定/查 term，见下
    execute!(terminal.backend_mut(), EnableLineWrap)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;   // ?1049l 回主屏
    execute!(terminal.backend_mut(), MoveTo(0, 0))?;           // 确定性起始
    let backend = terminal.backend_mut();
    for (row, line) in app.exit_document_lines(width).into_iter().enumerate() {
        if row > 0 { write!(backend, "\r\n")?; }
        write!(backend, "\r\x1b[2K{}\x1b[0m", line)?;         // 逐行 2K 清行 + 重打
    }
    write!(backend, "\r\n")?;
    disable_raw_mode()?;
    terminal.show_cursor()?;
    Ok(())
}
```
- `exit_document_lines(width)`：把 `app.transcript` 逐行转成终端行（复用 `draw::line_to_text`，按 width 硬 wrap，去样式/光标标记、收尾行），在 `LeaveAlternateScreen` **之后**写进主屏 → 对话留在 scrollback，无残影、无 padding 行（对齐 pi：不 after 帧为 exit 文档）。
- `width` 实取：`terminal` 的 `backend` 拿不到列数，改在退出前一次 `terminal.draw(|f| _ = f.area())` 拿 `area.width`，或按 `app.view` 缓存；实现时取 `area.width` 传入即可（占位示意）。
- `app.rs` 增加 `exit_document_lines`（或放 `view.rs`/`draw.rs`）：transcript → 终端行数组，纯函数，TestBackend 可测。

**⑤ `app.rs` —— 心跳/退出所需字段（最小）**
- `pub turn_started_at: Option<Instant>`（turn 起始写一次；draw.rs 状态面板 `Turn: Ns` 用之）。
- `pub working_dot: u8`（spinner 索引，ticker 自增；`is_turning` 时 `draw_input` 显示 `Working{'.'.repeat(1+dot)}`）。
- 不加 `TurnState` 枚举（超出最小范围），保留 `is_turning: bool`。

**⑥ `draw.rs` —— Working 动画 + 状态面板 Turn 行 + `line_to_text` 提升为可复用**
- `draw_input`（:92-105）：`is_turning` 时文本改为 `format!("Working{}", ".".repeat(1 + app.working_dot as usize))`。
- `draw_status_panel`（:118）追加 `Turn: {}s`，值由 `app.turn_started_at` 在 draw 时实时派生（与 `session_duration_str` 同构，0 mutate）。
- `line_to_text`（:147）改 `pub(crate)`，供 ④ 退出文档复用。

**⑦ 面板组件化（用户选定"默认仅对话窗口"）—— `draw.rs` `ui()` 条件挂载**
- `App` 增 `show_status: bool`、`show_footer: bool`（`App::new` 默认 `false`）——默认只挂对话窗口（transcript + input 为核心，必画）。
- `draw::ui`：`show_status` 时推进右栏（否则 `[100%, 0]` 单列）、`show_footer` 时保留底部 hint 行（否则 `Length(0)`），其余按需调用 `draw_footer`/`draw_status_panel`。
- **不做**通用 `Panel` trait、不做动态插件（v0 红线，见非目标）；复用时经 `App` 字段开关即可，未来要任意第三方面板再升 trait。

### 改动后关键数据流

```
Enter → events.rs:121 置 pending_submit → 外层下一事件触发 dispatch_input
     → :177 push User 行；:208 is_turning=true
     → run_turn_with_ticks 入场即 draw（User 行 + Working… 立即可见）
     → 内层 select 驱动：
         turn_fut 完成 → 返回（Ok / Ok(Cancelled)）
         Esc / Ctrl-C → handle_event → cancel_token.cancel() → 下一 round 边界返回 Cancelled
         Ctrl-D      → should_quit=true + cancel → Cancelled 收尾
         300ms 节拍  → working_dot 步进 + draw（"Working." 动画 + status 秒表）
     → dispatch_input :217-243 is_turning=false、push Assistant/Summary、from_sources 重建 snapshot
     → 外层下一轮 draw 显示完整结果；外层 after-await 检查 should_quit → break
退出  → restore_terminal(terminal, app)：
     LeaveAlternateScreen → MoveTo(0,0) → 逐行 2K 清行 + 打印完整 transcript → 显示光标 → 回 shell prompt
```

### 借用分析（成立性）
- `turn_fut` 独占 `&mut agent`（整个内层 loop）；内层事件/节拍 arm 只碰 `app`、`terminal`、`cancel_token`——三者均为与 `agent` 独立的变量，E0499/E0502 均不触发。
- `cancel_token` 是 `agent.cancel_handle()` 的 owned clone（`&self`），取消路径零 `&mut agent` 触碰。
- `events` 由外层经 `dispatch_input` reborrow 传入内层，返回后外层续用——作用域不交叠。
- turn 结束 `turn_fut` drop → `&mut agent` 释放 → `from_sources(&agent, ..)` 合法；退出时 `app.transcript` 已是终态，可读借用打印。

### 性能
- **渲染按需（pi 模型）**：turn 内仅在 事件/入场/turn 完成/300ms 节拍 画；turn 结束 ticker 即停。无全局定时器、无 idle 周期 draw（外层 tick 已删）。
- **cost**：300ms 全帧 draw 期间唯一真实开销是 `compute_visible` 每帧对全部 transcript 行重 wrap（成本随会话线性增长）。**正解是叶子渲染缓存（pi 启发的 L2）**：按 `TranscriptLine` 缓存渲染行（key = 内容 + width），仅追加行/宽变化时重建——**列为可选项，不在首个实现硬性要求**（pi 明确"不引框架级缓存"，此处按既有 leaf 自行缓存同样克制）。若不做缓存，节拍可降到 500ms 兜底。
- `AppView` turn 内只读（snapshot 冻结），零 `from_sources` 重复重建。

## 测试计划（TestBackend 可覆盖）

| 测试 | 位置 | 断言 |
|------|------|------|
| turn 中渲染 User 行 + Working | draw.rs `mod tests` | `is_turning=true` + push User + `working_dot=1` → buffer 含用户文本与 "Working.." |
| idle 不显示 Working | draw.rs | `is_turning=false` → buffer 无 "Working" |
| 状态面板 Turn 行 | draw.rs | 设 `turn_started_at` 后 draw → buffer 含 `Turn:` |
| **默认仅对话窗口** | draw.rs | `App::new` 默认 `show_status`/`show_footer=false` → buffer 无 "Provider:"/"Ready ·"；`show_status=true` 后 status 出现 |
| `exit_document_lines` 纯函数 | app.rs/draw.rs | 给定 transcript → 断言行数组内容、width wrap、无光标/样式标记 |
| restore 序列语义 | draw.rs（廉价依赖断言） | TestBackend 按行打印后 buffer 逐行含 transcript 内容 |
| 现有 events.rs 三个测试（Esc/清除/Ctrl-C） | events.rs（保留 `is_turning` 字段，零改动） | 不回归 |
| 现有 draw.rs 渲染回归（三栏/provider/符号） | draw.rs | 不回归 |

- 事件 arm / 内层 select 的轮询节奏、真实 crossterm 流不做单测，留 `e2e_tools`（`#[ignore]`）人工验证 Esc/Ctrl-C 中断与退出文档。
- `docs/design.md` §12 测试矩阵："Coding Agent" 产品层行补一条 `TUI turn 心跳渲染 + 事件路径取消 + 退出打印对话 + 单键退出`。

## 非目标（明确不做）

- 逐 token 流式 / 逐 tool call 追加（用户已选最小粒度；`AgentEvent` 已备 `ModelTextDelta`/`ToolCall`，将来在 turn 内 select 加 `event_rx.recv()` 分支即可）。
- turn 中排队输入 / 打断当前 turn（X3 的 `pending_queue`；最小实现保持键入进 `app.input` 缓冲、turn 后可见）。
- 把 turn 移出事件循环做后台任务（spawn+mpsc 需 `&mut agent` 移出、失去 from_sources 读权限、任务生命周期/结果乱序，不抵）。
- pi 的布局树/组件体系（VStack/HStack/ScrollView/命中测试/鼠标/图片），ratatui 差异过大且超范围。
- 输出若遇 `terminal.draw` IO 错误：沿用 `?` 语义（终端已坏即中止，与 event_loop:122 一致）。

## 决策

见 `docs/arch/tui-redraw-residual/adr-tui-turn-heartbeat.md`。