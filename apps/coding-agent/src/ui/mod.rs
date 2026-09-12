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

use crossterm::event::EventStream;
use std::io::Write;

use agent_core::CancelToken;
use agent_runtime::Agent;

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::state::StateStore;
use crate::status::TurnStats;

/// 进入 ratatui TUI 模式。
///
/// R2 阶段：搭骨架（alt-screen 进入 + 三栏 layout + 事件循环占位）。
/// 完整 dispatch（slash command + agent turn）R3 阶段补全。
pub async fn run(
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut terminal = setup_terminal()?;

    let session_started = Instant::now();
    let mut app = App::new(crate::view::AppView::from_sources(
        config,
        agent,
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
        config,
        commands,
        stats,
        state_store,
        &mut app,
        session_started,
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
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    app: &mut App,
    _session_started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;

    let mut events = EventStream::new();

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
                        config,
                        commands,
                        stats,
                        state_store,
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
async fn dispatch_input<B: Backend + Write>(
    terminal: &mut Terminal<B>,
    input: String,
    events: &mut EventStream,
    app: &mut App,
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::commands::{CommandContext, CommandResult, InquirePrompter};
    use agent_core::ContentBlock;
    use agent_loop::AgentInput;

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
                agent,
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
                app.view = crate::view::AppView::from_sources(
                    config,
                    agent,
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
        turn_input,
        agent.cancel_handle(),
        app,
    )
    .await;
    let elapsed = t0.elapsed().as_secs_f32();

    app.is_turning = false;

    match turn_result {
        Ok(run) => {
            stats.record(&run.usage);
            if let Some(msg) = &run.final_message {
                for block in &msg.content {
                    if let ContentBlock::Text { text } = block {
                        app.transcript.push(TranscriptLine::Assistant(text.clone()));
                    }
                }
            }
            app.transcript.push(TranscriptLine::Summary {
                rounds: run.rounds,
                stop: run.stop_reason.clone(),
                elapsed_secs: elapsed,
            });
            app.view = crate::view::AppView::from_sources(
                config,
                agent,
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

/// turn 自渲染循环：三路 select（turn_fut | 事件 | Working 动画节拍）。
///
/// 事件路径收走 Esc/Ctrl-C → `handle_event` → `cancel_token.cancel()`；Ctrl-D → should_quit + cancel。
/// 取代原 `tokio::signal::ctrl_c` 分支——raw mode 下 ^C 是 crossterm KeyEvent 而非 SIGINT，
/// 那条路在真实终端是死的（commit 645e339 意图未达成）。
///
/// 300ms 节拍仅 turn 期间存在（return 即释放）；turn 结束 `turn_fut` drop → `&mut agent` 释放。
async fn run_turn_with_ticks<B: Backend>(
    terminal: &mut Terminal<B>,
    events: &mut EventStream,
    agent: &mut Agent,
    input: agent_loop::AgentInput,
    cancel_token: CancelToken,
    app: &mut App,
) -> Result<agent_loop::RunResult, Box<dyn std::error::Error>> {
    use futures::StreamExt;

    let mut ticker = tokio::time::interval(Duration::from_millis(300));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut turn_fut = Box::pin(agent.run_turn(input));

    terminal.draw(|f| draw::ui(f, app))?; // 入场即画：提交行 + "Working" 立即可见

    loop {
        tokio::select! {
            biased;
            // 1. turn 完成（自然 / Cancelled 都走这里 — BasicLoop 返回 Ok(.. stop_reason: Cancelled)）
            res = &mut turn_fut => {
                return res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>);
            }
            // 2. 事件路径取消（Esc/Ctrl-C → cancel；Ctrl-D → should_quit + cancel）
            Some(Ok(ev)) = events.next() => {
                events::handle_event(ev, app)?;
                if app.should_quit {
                    cancel_token.cancel();
                }
            }
            // 3. Working 动画节拍（pi loader.ts：动画激活才跑，turn 结束即停）
            _ = ticker.tick() => {
                app.working_dot = (app.working_dot + 1) % 3;
                terminal.draw(|f| draw::ui(f, app))?;
            }
        }
    }
}
