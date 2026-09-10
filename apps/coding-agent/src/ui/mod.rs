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
use tokio::time::interval;

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
        app,
        session_started,
    )
    .await;

    restore_terminal(&mut terminal)?;
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

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::{
        execute,
        terminal::{EnableLineWrap, LeaveAlternateScreen, disable_raw_mode},
    };
    execute!(terminal.backend_mut(), EnableLineWrap)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    terminal.show_cursor()?;
    Ok(())
}

async fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    mut app: App,
    session_started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::event::EventStream;
    use futures::StreamExt;

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| draw::ui(f, &mut app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                events::handle_event(event, &mut app)?;
                if app.should_quit {
                    break;
                }
                if let Some(input) = app.take_submitted() {
                    dispatch_input(input, &mut app, agent, config, commands, stats, state_store).await?;
                }
            }
            _ = tick.tick() => {
                if app.is_turning {
                    app.view = crate::view::AppView::from_sources(
                        config,
                        agent,
                        &config.registry,
                        state_store,
                        stats,
                        session_started,
                    );
                }
            }
        }
    }
    Ok(())
}

/// R3 阶段完整实现 — slash command 分发 + agent turn 桥接（带 tick + Ctrl-C 取消）。
async fn dispatch_input(
    input: String,
    app: &mut App,
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::commands::{CommandContext, CommandResult};
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

    // slash command
    if input.starts_with('/') {
        let result = {
            let mut ctx = CommandContext {
                agent,
                config,
                state: state_store,
            };
            commands.execute(&input, &mut ctx).await
        };
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
    let turn_input = AgentInput::text(&input);

    let turn_result = run_turn_with_ticks(agent, turn_input, agent.cancel_handle()).await;
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

/// R3 阶段 — agent.run_turn 直通 + Ctrl-C 中断（select! 三路）。
///
/// 100ms tick 由 select! 第二路处理（view refresh 由 event_loop 外层判断）；
/// Ctrl-C 由 select! 第三路捕获，触发 `cancel_token.cancel()`，BasicLoop
/// 在 round 边界检测 cancel 后返回 `Ok(RunResult { stop_reason: Cancelled, .. })`，
/// turn_fut 自然完成。不 drop future — 走 Ok 分支。
async fn run_turn_with_ticks(
    agent: &mut Agent,
    input: agent_loop::AgentInput,
    cancel_token: CancelToken,
) -> Result<agent_loop::RunResult, Box<dyn std::error::Error>> {
    use std::time::Duration;

    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut turn_fut = Box::pin(agent.run_turn(input));

    loop {
        tokio::select! {
            biased;
            // 1. turn 完成（自然 / Cancelled 都会走这里 — BasicLoop 返回 Ok(.. stop_reason: Cancelled)）
            res = &mut turn_fut => {
                return res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>);
            }
            // 2. 100ms tick — view_dirty 由 is_turning 在 event_loop 外层判断
            _ = tick.tick() => { /* view 由 event_loop refresh */ }
            // 3. Ctrl-C — 触发 cancel，turn_fut 不 drop，等 round 边界 BasicLoop 检测 cancel 返回 Cancelled
            _ = tokio::signal::ctrl_c() => {
                cancel_token.cancel();
            }
        }
    }
}
