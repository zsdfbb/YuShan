#![cfg(feature = "tui-ratatui")]
//! ratatui TUI 模式入口。
//!
//! R2 阶段：骨架 — alt-screen 切换 + 三栏布局渲染 + 事件循环占位。
//! R3 阶段：补全 `dispatch_input`（slash command 分发 + agent turn 桥接）。
//! R4 阶段：transcript 滚动 + follow mode 验证。
//! R5 阶段：Summary 符号渲染验证。
//! R6 阶段：TestBackend 单测 + main.rs 入口分发切换。
//!
//! Feature flag: `tui-ratatui`（默认开启）。`tui-stdout` 模式下整个 ui/ 模块不编译。

mod app;
mod completion;
mod draw;
mod events;

pub use app::App;
#[allow(unused_imports)]
pub use app::{CompletionItem, CompletionState, TranscriptLine};

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use tokio::sync::mpsc;

use crossterm::event::EventStream;
use std::io::Write;

use ys_channel::{Envelope, Intent};
use ys_core::{CancelToken, ContentBlock, Message, StopReason};
use ys_event::AgentEvent;
use ys_runtime::{Agent, RunSummary};

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::wiring::Wiring;

/// 进入 ratatui TUI 模式。
///
/// `rx` 是 [`ChannelSink`](crate::channel::ChannelSink) 的接收端；TUI 在 turn 期间
/// 通过 `select!` 增量消费事件做流式渲染（块 C）。
///
/// `wiring` 是接线器（ADR-0010）：持有会话 + 队列 + 模型 + 事件出口。
/// `/new` 后其 session/inbox 已换新，下一次 turn 自动用新会话。
pub async fn run(
    agent: &mut Agent,
    wiring: &mut Wiring,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    rx: mpsc::Receiver<Envelope>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut terminal = setup_terminal()?;

    let session_started = Instant::now();
    let mut app = App::new(crate::view::AppView::from_sources(
        config,
        agent,
        wiring,
        &config.registry,
        state_store,
        stats,
        session_started,
    ));

    // 启动时构造 cancel token clone，写进 App（events.rs / Esc-Ctrl-C 分流用）
    let cancel_token = agent.cancel_handle();
    app.cancel_token = Some(cancel_token);

    let result = event_loop(
        &mut terminal,
        agent,
        wiring,
        config,
        commands,
        stats,
        state_store,
        &mut app,
        session_started,
        rx,
    )
    .await;

    // 退出前打印完整对话进主屏 scrollback（pi 语义）——需要终态 app.transcript
    restore_terminal(&mut terminal, &app)?;
    result
}

fn setup_terminal()
-> Result<Terminal<CrosstermBackend<std::io::Stdout>>, Box<dyn std::error::Error>> {
    use crossterm::{
        execute,
        terminal::{DisableLineWrap, EnterAlternateScreen, enable_raw_mode},
    };
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, DisableLineWrap)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// 执行 slash command 前暂停 TUI：离开 alt-screen + 关闭 raw mode，
/// 让命令（含 `inquire` 交互）在真实终端运行，避免其 stdout/键盘操作污染 alt-screen。
/// 期间不 poll 事件流 → crossterm 后台线程停在 channel recv，不抢 stdin。
fn suspend_terminal<B: Backend + Write>(
    terminal: &mut Terminal<B>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::{
        cursor::Show,
        execute,
        terminal::{EnableLineWrap, LeaveAlternateScreen, disable_raw_mode},
    };
    execute!(terminal.backend_mut(), EnableLineWrap)?;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), Show)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

/// 命令执行完恢复 TUI：重进 alt-screen + raw mode + 清空 ratatui 缓冲（避免与旧帧 diff）。
fn resume_terminal<B: Backend + Write>(
    terminal: &mut Terminal<B>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::{
        execute,
        terminal::{DisableLineWrap, EnterAlternateScreen, enable_raw_mode},
    };
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        DisableLineWrap
    )?;
    terminal.clear()?;
    Ok(())
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &App,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::{
        cursor::MoveTo,
        execute,
        terminal::{EnableLineWrap, LeaveAlternateScreen, disable_raw_mode},
    };

    let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = (cols as usize).clamp(10, 400);
    let lines = draw::transcript_to_lines(app, width);

    execute!(terminal.backend_mut(), EnableLineWrap)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    execute!(terminal.backend_mut(), MoveTo(0, 0))?;

    // pi 语义：逐行 2K 清行 + 打印完整对话进主屏 scrollback（无残影、无 padding 行）
    let mut out = std::io::stdout().lock();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            write!(out, "\r\n")?;
        }
        write!(out, "\r\x1b[2K{}\x1b[0m", line)?;
    }
    write!(out, "\r\n")?;
    out.flush()?;

    disable_raw_mode()?;
    terminal.show_cursor()?;
    Ok(())
}

async fn event_loop<B: Backend + Write>(
    terminal: &mut Terminal<B>,
    agent: &mut Agent,
    wiring: &mut Wiring,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    app: &mut App,
    _session_started: Instant,
    rx: mpsc::Receiver<Envelope>,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;

    let mut events = EventStream::new();
    let mut rx = rx;

    loop {
        // 渲染按需：每处理一个事件重绘一次；无独立渲染循环（pi 模型）
        terminal.draw(|f| draw::ui(f, app))?;

        match events.next().await {
            Some(Ok(event)) => {
                events::handle_event(event, app)?;
                if let Some(input) = app.take_submitted() {
                    dispatch_input(
                        terminal,
                        input,
                        &mut events,
                        app,
                        agent,
                        wiring,
                        config,
                        commands,
                        stats,
                        state_store,
                        &mut rx,
                    )
                    .await?;
                }
                if app.should_quit {
                    break; // bug3：检查移到 dispatch 之后 → /quit、exit 单键退出
                }
            }
            Some(Err(_)) | None => break, // 事件流错误/关闭
        }
    }
    Ok(())
}

/// R3 阶段完整实现 — slash command 分发 + agent turn 桥接（带 spin 节拍 + 事件路径取消）。
#[allow(clippy::too_many_arguments)]
async fn dispatch_input<B: Backend + Write>(
    terminal: &mut Terminal<B>,
    input: String,
    events: &mut EventStream,
    app: &mut App,
    agent: &mut Agent,
    wiring: &mut Wiring,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    rx: &mut mpsc::Receiver<Envelope>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::commands::{CommandContext, CommandResult, InquirePrompter};
    use ys_loop::AgentInput;

    let input = input.trim().to_string();
    if input.is_empty() {
        return Ok(());
    }

    // exit / quit (非 slash command 形式)
    if matches!(input.as_str(), "exit" | "quit") {
        app.should_quit = true;
        return Ok(());
    }

    // User 输入记录到 transcript
    app.transcript.push(TranscriptLine::User(input.clone()));
    app.follow = true;

    // slash command —— TUI 让位：命令（含 inquire 交互）在真实终端运行，
    // 避免其 stdout/键盘操作污染 alt-screen（修复"交互错位"根因）
    if input.starts_with('/') {
        suspend_terminal(terminal)?;
        let result = {
            let prompter = InquirePrompter;
            let mut ctx = CommandContext {
                wiring,
                config,
                state: state_store,
                prompter: &prompter,
            };
            commands.execute(&input, &mut ctx).await
        };
        if let Err(e) = resume_terminal(terminal) {
            return Err(e.into());
        }
        match result {
            Ok(CommandResult::Continue) => {
                // 命令可能换了会话（/new）或模型（/model、/login），重建快照；
                // wiring 是同一句柄，无需重新取。
                app.view = crate::view::AppView::from_sources(
                    config,
                    agent,
                    wiring,
                    &config.registry,
                    state_store,
                    stats,
                    app.view.session_started,
                );
            }
            Ok(CommandResult::Exit) => app.should_quit = true,
            Err(e) => app.transcript.push(TranscriptLine::Error(format!("{e}"))),
        }
        return Ok(());
    }

    // agent turn
    app.is_turning = true;
    let t0 = Instant::now();
    app.turn_started_at = Some(t0);
    app.working_dot = 0;
    let turn_input = AgentInput::text(&input);

    let turn_result = run_turn_with_ticks(
        terminal,
        events,
        agent,
        wiring,
        turn_input,
        agent.cancel_handle(),
        app,
        rx,
    )
    .await;
    let elapsed = t0.elapsed().as_secs_f32();

    app.is_turning = false;

    match turn_result {
        Ok((summary, streamed)) => {
            stats.record(&summary.usage);
            // 增量渲染优先：已流式追加过则不再从 final_message 追加（避免两遍）；
            // 一个 delta 都没收到时兜底，防止模型不产 delta 导致文本丢失。
            finalize_assistant_text(app, summary.last_message.as_ref(), streamed);
            app.transcript.push(TranscriptLine::Summary {
                rounds: summary.last_rounds,
                stop: summary.last_stop.clone().unwrap_or(StopReason::Completed),
                elapsed_secs: elapsed,
            });
            app.view = crate::view::AppView::from_sources(
                config,
                agent,
                wiring,
                &config.registry,
                state_store,
                stats,
                app.view.session_started,
            );
            app.follow = true;
        }
        Err(e) => app.transcript.push(TranscriptLine::Error(format!("{e}"))),
    }

    Ok(())
}

/// 把一个 agent 事件**增量**应用到 `App.transcript`（纯函数，便于单测）。
///
/// - `ModelTextDelta`：本回合**首个**增量时新建 `TranscriptLine::Assistant` 行
///   （`*assistant_started = true`），此后追加到该行 `String`。
/// - `ModelThinkingDelta`：**忽略**（设计定 TUI 展示从简——思考过程不占对话区，
///   避免 reasoning 与正文混排）。
/// - `ToolCall` / `ToolResult`：**忽略**（理由见本模块 `run_turn_with_ticks` 注释：
///   单条 `TranscriptLine::Tool` 需同时持 args + result，二者分属两个事件且以
///   `ToolCallId` 配对，需额外缓冲状态；本轮以「assistant 文本增量」为核心目标，
///   维持 TUI 既有的「不渲染工具调用」行为，无回归）。
/// - `RunFinished` / `RunFailed`：**忽略**——它们是「本回合结束」的旁证，
///   但结束的**权威**是 `turn_fut` 完成；select! 不以它退出。
/// - 其余事件：忽略。
fn apply_agent_event(app: &mut App, env: &Envelope, assistant_started: &mut bool) {
    let AgentEvent::ModelTextDelta { text } = &env.event else {
        return;
    };
    if !*assistant_started {
        app.transcript
            .push(TranscriptLine::Assistant(String::new()));
        *assistant_started = true;
    }
    // 本回合不渲染 Tool/Summary 行，故末行恒为本回合的 assistant 行。
    if let Some(TranscriptLine::Assistant(buf)) = app.transcript.last_mut() {
        buf.push_str(text);
    }
}

/// 回合收尾：决定是否从 `final_message` **兜底**追加 assistant 文本。
///
/// `streamed == true`（本回合收到过 `ModelTextDelta`）时**不追加**——否则同一段
/// 文本会出现两遍。仅当本回合一个增量都没收到时才用 `final_message` 兜底，
/// 防止模型/适配器不产 delta 时文本丢失。
fn finalize_assistant_text(app: &mut App, final_message: Option<&Message>, streamed: bool) {
    if streamed {
        return;
    }
    let Some(msg) = final_message else {
        return;
    };
    for block in &msg.content {
        if let ContentBlock::Text { text } = block {
            app.transcript.push(TranscriptLine::Assistant(text.clone()));
        }
    }
}

/// **非阻塞**排空信道中已到达的事件，逐个 `apply_agent_event`。返回处理条数。
///
/// `turn_fut` 完成后调用：此刻 `Agent::run` 已返回，其 `RunFinished` 的
/// `emit().await` 已把 sink 的 overflow 冲刷干净，故 `try_recv` 能取到本回合
/// 全部剩余事件，保证 transcript 完整。
fn drain_pending_events(
    rx: &mut mpsc::Receiver<Envelope>,
    app: &mut App,
    assistant_started: &mut bool,
) -> usize {
    let mut drained = 0;
    while let Ok(env) = rx.try_recv() {
        apply_agent_event(app, &env, assistant_started);
        drained += 1;
    }
    drained
}

/// turn 自渲染循环：四路 select（turn_fut | 终端键鼠 | 信道事件 | Working 节拍）。
///
/// 事件路径收走 Esc/Ctrl-C → `handle_event` → `cancel_token.cancel()`；Ctrl-D → should_quit + cancel。
/// 取代原 `tokio::signal::ctrl_c` 分支——raw mode 下 ^C 是 crossterm KeyEvent 而非 SIGINT，
/// 那条路在真实终端是死的（commit 645e339 意图未达成）。
///
/// 信道路径（块 C）：从 [`ChannelSink`](crate::channel::ChannelSink) 接收端收
/// `Envelope`，`ModelTextDelta` 增量追加到 assistant 行。**必须**在 turn 期间持续
/// 收——有界信道无并发消费者时 agent 撞满即阻塞（死锁）。`biased` 使 turn_fut
/// 优先，事件洪峰不会饿死回合完成。
///
/// 300ms 节拍仅 turn 期间存在（return 即释放）；turn 结束 `turn_fut` drop → `&mut agent` 释放。
///
/// 返回 `(RunSummary, streamed)`：`streamed` 表示本回合是否收到过文本增量，
/// 供调用方决定是否走 `final_message` 兜底。
#[allow(clippy::too_many_arguments)]
async fn run_turn_with_ticks<B: Backend>(
    terminal: &mut Terminal<B>,
    events: &mut EventStream,
    agent: &mut Agent,
    wiring: &mut Wiring,
    input: ys_loop::AgentInput,
    cancel_token: CancelToken,
    app: &mut App,
    rx: &mut mpsc::Receiver<Envelope>,
) -> Result<(RunSummary, bool), Box<dyn std::error::Error>> {
    use futures::StreamExt;

    let mut ticker = tokio::time::interval(Duration::from_millis(300));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 用户输入作为 followUp 入队，走 `run(&inbox)`（而非 `run_turn`）：
    // 自转会调 `begin_turn(n)`，使信封携带正确 turn 号，TUI 也看到 turn 语义。
    // inbox 是接线器的克隆句柄——`/new` 换掉的正是它，故这里每次重新取。
    let inbox = wiring.inbox();
    inbox.push(input.message, Intent::FollowUp);
    let mut turn_fut = Box::pin(agent.run(wiring.ports(), &inbox));

    // 本回合是否已建 assistant 行（增量渲染标志）；亦作为「是否收到过 delta」的判据。
    let mut assistant_started = false;
    // 信道是否仍开着。关闭后禁用该 select 分支，避免 `recv()` 立即返回 None 造成忙等。
    let mut channel_open = true;

    terminal.draw(|f| draw::ui(f, app))?; // 入场即画：提交行 + "Working" 立即可见

    loop {
        tokio::select! {
            biased;
            // 1. turn 完成（自然 / Cancelled 都走这里 — BasicLoop 返回 Ok(.. stop_reason: Cancelled)）
            res = &mut turn_fut => {
                // 先排空剩余事件，再返回（保证 transcript 完整）
                drain_pending_events(rx, app, &mut assistant_started);
                let summary = res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
                return Ok((summary, assistant_started));
            }
            // 2. 事件路径取消（Esc/Ctrl-C → cancel；Ctrl-D → should_quit + cancel）
            Some(Ok(ev)) = events.next() => {
                events::handle_event(ev, app)?;
                if app.should_quit {
                    cancel_token.cancel();
                }
            }
            // 3. 信道事件：增量渲染（流式文本逐块追加）
            env = rx.recv(), if channel_open => {
                match env {
                    Some(env) => {
                        apply_agent_event(app, &env, &mut assistant_started);
                        terminal.draw(|f| draw::ui(f, app))?;
                    }
                    None => channel_open = false, // 信道关闭：不再可能有事件
                }
            }
            // 4. Working 动画节拍（pi loader.ts：动画激活才跑，turn 结束即停）
            _ = ticker.tick() => {
                app.working_dot = (app.working_dot + 1) % 3;
                terminal.draw(|f| draw::ui(f, app))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use ys_channel::Source;
    use ys_core::{Message, Role, Usage};

    use super::*;
    use crate::view::AppView;

    /// 最小测试 App：空 transcript、默认面板隐藏（只测对话窗口）。
    fn make_app() -> App {
        let view = AppView {
            cwd: PathBuf::from("/tmp"),
            provider: None,
            model: None,
            config_path: PathBuf::from("/tmp/auth.json"),
            logged_in_providers: vec![],
            total_known_providers: 0,
            version: "test",
            total_input_tokens: 0,
            total_output_tokens: 0,
            turn_count: 0,
            session_started: Instant::now(),
            message_count: 0,
            tools: vec![],
            context_window: None,
            is_first_run: false,
            commands: vec![],
        };
        App::new(view)
    }

    /// 复用 `draw.rs` 的 TestBackend 手法：渲染成一张字符画（按行拼接）。
    fn render_to_text(app: &mut App) -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw::ui(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    fn delta_env(text: &str) -> Envelope {
        Envelope::new(
            Source::agent(),
            1,
            AgentEvent::ModelTextDelta {
                text: text.to_string(),
            },
        )
    }

    fn finished_env() -> Envelope {
        Envelope::new(
            Source::agent(),
            1,
            AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            },
        )
    }

    fn text_message(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// transcript 里的全部 assistant 行文本（按序）。
    fn assistant_texts(app: &App) -> Vec<String> {
        app.transcript
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::Assistant(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// 1. **增量渲染**：多个 `ModelTextDelta` 拼进**同一** assistant 行，渲染可见。
    #[test]
    fn incremental_deltas_append_into_one_assistant_line() {
        let mut app = make_app();
        let mut started = false;

        apply_agent_event(&mut app, &delta_env("Hel"), &mut started);
        apply_agent_event(&mut app, &delta_env("lo "), &mut started);
        apply_agent_event(&mut app, &delta_env("world"), &mut started);

        assert!(started, "首个 delta 应建立 assistant 行");
        assert_eq!(
            assistant_texts(&app),
            vec!["Hello world".to_string()],
            "多个 delta 应拼进同一行"
        );

        let text = render_to_text(&mut app);
        assert!(
            text.contains("Hello world"),
            "渲染应包含增量文本；text = {text}"
        );
    }

    /// 2. **不重复**：已流式追加过（`streamed = true`）+ `final_message` 同文本
    ///    → 文本在渲染里**只出现一次**。
    #[test]
    fn finalize_does_not_duplicate_streamed_text() {
        let mut app = make_app();
        let mut started = false;
        apply_agent_event(&mut app, &delta_env("Hello world"), &mut started);

        let final_msg = text_message("Hello world");
        finalize_assistant_text(&mut app, Some(&final_msg), started);

        assert_eq!(
            assistant_texts(&app),
            vec!["Hello world".to_string()],
            "已流式追加后不得再从 final_message 追加"
        );
        let text = render_to_text(&mut app);
        assert_eq!(
            text.matches("Hello world").count(),
            1,
            "文本应只出现一次；text = {text}"
        );
    }

    /// 3. **兜底**：无任何 `ModelTextDelta`（`streamed = false`）+ `final_message` 有文本
    ///    → 仍从 `final_message` 追加，文本不丢。
    #[test]
    fn finalize_falls_back_when_no_delta_arrived() {
        let mut app = make_app();
        let started = false; // 本回合一个 delta 都没收到

        let final_msg = text_message("fallback text");
        finalize_assistant_text(&mut app, Some(&final_msg), started);

        assert_eq!(assistant_texts(&app), vec!["fallback text".to_string()]);
        let text = render_to_text(&mut app);
        assert!(text.contains("fallback text"), "text = {text}");
    }

    /// 4. **排空**：信道里预置若干 `Envelope`（含终局事件）→ `try_recv` 排空逻辑
    ///    全部处理；文本按序拼接，终局事件不产出文本。
    #[tokio::test]
    async fn drain_pending_events_applies_all_buffered_envelopes() {
        let (tx, mut rx) = mpsc::channel(16);
        tx.send(delta_env("a")).await.unwrap();
        tx.send(delta_env("b")).await.unwrap();
        tx.send(finished_env()).await.unwrap();
        tx.send(delta_env("c")).await.unwrap();
        drop(tx); // 关闭信道：排空到 Disconnected 即停

        let mut app = make_app();
        let mut started = false;
        let drained = drain_pending_events(&mut rx, &mut app, &mut started);

        assert_eq!(drained, 4, "应把 4 个已到达信封全部处理");
        assert_eq!(
            assistant_texts(&app),
            vec!["abc".to_string()],
            "文本增量应按序拼接；RunFinished 不产出文本"
        );
    }

    /// 4b. 空信道 → 排空 0 条（不 panic、不阻塞）。
    #[tokio::test]
    async fn drain_pending_events_on_empty_channel_is_zero() {
        let (tx, mut rx) = mpsc::channel(4);
        drop(tx);
        let mut app = make_app();
        let mut started = false;
        assert_eq!(drain_pending_events(&mut rx, &mut app, &mut started), 0);
        assert!(!started, "无事件不应建 assistant 行");
    }
}
