# TUI turn 活渲染 — 阻塞 dispatch + 内层三路 select（事件/节拍/turn_future）+ 退出打印完整对话

修复 coding-agent ratatui TUI 两个显示 bug（turn 期间零刷新、退出残影）时，采用**阻塞式 dispatch_input + 内层自渲染三路 select** 形态：turn 仍由 `dispatch_input` 同步 await 执行，但 turn 期间的渲染与取消搬进 `run_turn_with_ticks` 的一个 `select!(turn_fut | events | Working 动画节拍)` 循环；取消统一走 crossterm 事件路径（删掉内层 `tokio::signal::ctrl_c`）；退出 alt-screen 时逐行清主屏并打印完整对话文档；`should_quit` 检查移到 dispatch 之后。

## 语境

- `event_loop` 原先在 events 分支内联 `dispatch_input(...).await`（ui/mod.rs:131），外层 select 在 turn 期间整体挂起，而 `run_turn_with_ticks` 内层 tick 为空 → turn 期间零 `terminal.draw()`。
- 源码级验证：crossterm raw mode 用 `cfmakeraw`（清 ISIG）→ ^C 是 crossterm KeyEvent 而非 SIGINT → 内层 `tokio::signal::ctrl_c()`（mod.rs:274）永不触发；而能识别 Esc/Ctrl-C 的 `handle_event` 只在被阻塞的外层 poll。**"Ctrl-C/Esc 中断 turn" 在真实终端假死**（commit 645e339 意图未达成，其单测只测了 `handle_key` 而非循环）。
- turn 期间 `turn_fut` 独占 `&mut agent` → `AppView::from_sources`（参数 `&Agent`）不可调用；`TurnStats::record` 只在 turn 结束调用，tokens/turn_count 冻结，唯一实时变化是 session 秒表（draw 时派生）。
- 退出残影：`?1049l` 只切回主屏不清 alt 缓冲区，终端把最后帧并入 scrollback。
- **参考实现**：本项目 `tmp/pi`（成熟 TypeScript coding-agent TUI）确立两个与本问题直接对应的模式——① 渲染按需驱动（"no independent layout loop"；`requestRender`=dirty 标志+节流合并）+ 动画用组件内定时器仅激活时运行（tui.ts:944-1005、loader.ts）；② 退出时打印完整逻辑文档而非清屏（"do not use only the last visible frame as the exit document"，tui-alt-screen.ts:381-407）。

## 决策（前两项已与用户确认）

1. **形态**：保持 blocking `dispatch_input`；`run_turn_with_ticks` 增 `&mut Terminal<B>`、`&mut EventStream`、`&mut App` 参数，内层 `select!(biased; turn_fut | events | 300ms 动画节拍)`，入场先 `terminal.draw` 一帧。**不引入**外层 Idle/Turning 状态机与输入队列。
2. **渲染模型（pi 对齐）**：渲染跟随状态变化（入场 / 事件 / turn 完成），无全局定时器；turn 期间仅一个 "Working…" 动画节拍（300ms，`working_dot` 步进）提供视觉心跳，turn 结束即停。不做逐 token 流式（用户选定最小刷新粒度）。
3. **取消**：删除内层 `ctrl_c` arm；Esc/Ctrl-C/Ctrl-D 经 `handle_event`（events.rs 已路由到 `cancel_token.cancel()`）触发，BasicLoop 在 round 边界返回 `Ok(stop_reason: Cancelled)`，不 drop future。
4. **退出（用户选定 pi 风格）**：`restore_terminal(terminal, app)` 在 `LeaveAlternateScreen` 后 `MoveTo(0,0)`，按 transcript 逐行 `\r\x1b[2K` 清行 + 重打完整对话进主屏 scrollback（复用 `line_to_text`，去样式/光标标记、按 width 硬 wrap、无 padding 行）。**不做** `terminal.clear()` 清空方案。
5. **单键退出**：`should_quit` 检查移到 `dispatch_input` 之后。
6. **状态**：`App` 增 `turn_started_at: Option<Instant>`（draw 时派生 `Turn: Ns`）与 `working_dot: u8`；保留 `is_turning: bool`，不引入 `TurnState` 枚举。
7. **面板组件化，默认仅对话窗口**（用户选定）：`App` 增 `show_status`/`show_footer`（默认 `false`），`draw::ui` 按开关挂载 status/footer 面板，默认只画对话窗口（transcript + input）。不做通用 `Panel` trait、不做动态插件（v0 红线：不支持运行时热插拔、不跨动态库边界传 ratatui/tokio 类型）。

## Considered Options

- **X1 内层只画（最小阻塞）**：内层 tick 直 `terminal.draw`，保留内层 `ctrl_c`。代价：turn 中事件仍不处理，Esc/Ctrl-C 取消维持假死。否决。
- **X3 两相状态机 + 输入队列（可扩展优先）**：外层 Idle/Turning + `classify_submit` + `pending_queue`，borrowck 静态禁止 turn 中碰 agent。净增 ~90 行 + 状态机 + 队列；turn 中排队输入**超出"最小"范围**。否决（留作未来流式/多输入接缝）。
- **spawn 后台 task + mpsc**：`&mut agent` 移出即失去 `from_sources(&agent)` 读权限（状态面板冻结）、需处理 task 生命周期与结果/事件 channel 乱序 drain，不抵。否决。
- **退出：清屏 + 空 prompt**（`terminal.clear()` 后 `?1049l`）：最简但刚进行的对话从终端消失（alt buffer 丢弃）。用户选定 pi 风格替代它，在咨询选项中明确对比后选"打印完整对话"。

推荐方案 D = X2（X1 上加 turn 内事件 arm）融合 pi 的"渲染按需 + 动画节拍 + 退出打印文档"，以 ~55 行/4 处签名换取取消真生效、对话留在 scrollback、心跳成本受控。

## Consequences

- 正面：turn 期间界面持续刷新且取消功能在真实终端生效（修复既有假死）；对话在退出后留在 scrollback 可回滚、无残影无 padding（pi 语义）；Esc/Ctrl-C/Ctrl-D 单一路径；删死代码 `ctrl_c` arm 与外侧 tick 的 `from_sources` 分支；`should_quit` 后置修复单键退出；渲染按需 + 动画节拍，无全局定时器。
- 代价：`dispatch_input`/`run_turn_with_ticks` 泛型化 `<B: Backend>` 并增加两个贯穿参数；`restore_terminal` 增加 `&App` 参数与退出打印逻辑（~30 行新码）；内层事件 arm 在 turn 中消费 crossterm 事件（键入会改 `app.input` 缓冲、turn 后可见——可接受的最小语义）；退出打印长对话会占用主屏 scrollback 若干屏幕（用户预期）。
- 演进：未来逐 token/逐 tool 流式 = 在 turn 内 select 加 `event_rx.recv()` 分支（`AgentEvent` 已备 `ModelTextDelta`/`ToolCall`）+ `app.apply_event`，无需重构本形态；turn 中排队输入 = 在 `App` 加 `pending_queue`，`dispatch_input` 后 drain，同样不重构；长会话绘制成本正解为 TranscriptLine 级渲染缓存（pi 叶子缓存思路，列为可选项）。
- 测试：draw.rs TestBackend 断言 turn 渲染（Working 动画 + User 行 + Turn 行）与 `exit_document_lines` 纯函数；`should_quit` 后置与事件 arm 的轮询节奏不做单测，留 `e2e_tools`（`#[ignore]`）。
- docs/design.md §12 测试矩阵 "Coding Agent" 行补：TUI turn 心跳渲染 + 事件路径取消 + 退出打印对话 + 单键退出。