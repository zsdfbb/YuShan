//! TUI main loop.
//!
//!
//! - Reads input via `rustyline` (history, completion, Ctrl-C / Ctrl-D handling).
//! - Maintains an `AppView` rebuilt only when `view_dirty = true` — see
//!   `docs/arch/tui-resident-status/design.md` §Step 4 + §决策 5.
//! - Slash commands are dispatched via `CommandRegistry::execute`.
//! - Normal input becomes an `AgentInput::text` and is sent through
//!   `Agent::run_turn`. Wall-clock duration is recorded and rendered by
//!   `format::print_turn_summary` (grouping E will add context %).

use agent_loop::AgentInput;
use agent_runtime::Agent;
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use std::io::{self};
use std::time::Instant;

use crate::commands::{CommandContext, CommandRegistry, CommandResult};
use crate::config::Config;
use crate::format;
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::tui_completer::{CmdCompleter, CmdEntry};
use crate::view;

pub async fn run_interactive(
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_started = Instant::now();

    // Build the initial view + populate command list from the registry.
    let mut view = build_view(config, agent, state_store, stats, session_started);
    inject_command_meta(&mut view, commands);

    // One-shot first-run hint + banner. The first footer will be
    // emitted by the loop entry (`view_dirty = true`).
    if view.is_first_run {
        println!("No API credentials. Run /login to set up your provider.");
        println!();
    }
    {
        let mut stdout = io::stdout().lock();
        format::print_banner(&mut stdout, &view)?;
    }

    // Build rustyline + completer from the same command metadata.
    let helper = CmdCompleter::new(cmd_entries_from_registry(commands));
    let mut rl: Editor<CmdCompleter, DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(helper));

    // After any command execution or completed turn the view must be
    // refreshed so the next footer reflects the new state.
    let mut view_dirty = true;

    loop {
        if view_dirty {
            view = build_view(config, agent, state_store, stats, session_started);
            inject_command_meta(&mut view, commands);
            view_dirty = false;
        }

        // Print the resident footer (single line above the prompt).
        {
            let mut out = io::stdout().lock();
            format::print_footer(&mut out, &view)?;
        }

        // Read one line. rustyline surfaces Ctrl-C as `Interrupted` and
        // Ctrl-D / EOF as `Eof`; both are user-visible signals we honour.
        let line = match rl.readline("> ") {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("(press Ctrl-D or type 'exit' to quit)");
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(Box::new(e)),
        };

        rl.add_history_entry(line.as_str())?;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "exit" | "quit") {
            break;
        }

        // Slash command dispatch.
        if input.starts_with('/') {
            let result = {
                let mut ctx = CommandContext {
                    agent,
                    config,
                    state: state_store,
                };
                match commands.execute(input, &mut ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        CommandResult::Continue
                    }
                }
            };
            view_dirty = true;
            match result {
                CommandResult::Continue => continue,
                CommandResult::Exit => break,
            }
        }

        // Normal agent turn.
        let agent_input = AgentInput::text(input);
        let t0 = Instant::now();
        let result = agent.run_turn(agent_input).await;
        let elapsed_secs = t0.elapsed().as_secs_f32();

        match result {
            Ok(run) => {
                if let Some(msg) = &run.final_message {
                    for block in &msg.content {
                        if let agent_core::ContentBlock::Text { text } = block {
                            println!("{text}");
                        }
                    }
                }
                println!();
                stats.record(&run.usage);
                {
                    let mut out = io::stdout().lock();
                    format::print_turn_summary(
                        &mut out,
                        &view,
                        run.rounds,
                        &run.stop_reason,
                        elapsed_secs,
                    )?;
                }
                println!();
                view_dirty = true;
            }
            Err(e) => {
                let prefix = match &e {
                    agent_loop::LoopError::Model(_) => "[model]",
                    agent_loop::LoopError::ConfigError(_) => "[config]",
                    agent_loop::LoopError::Tool(_) => "[tool]",
                    agent_loop::LoopError::Event(_) => "[event]",
                };
                eprintln!("Error {}: {}", prefix, e);
                println!();
            }
        }
    }

    Ok(())
}

/// Re-build the snapshot from the live sources. Called whenever
/// `view_dirty = true` (set after any command execution or completed turn).
fn build_view(
    config: &Config,
    agent: &Agent,
    state_store: &StateStore,
    stats: &TurnStats,
    session_started: Instant,
) -> view::AppView {
    view::AppView::from_sources(
        config,
        agent,
        &config.registry,
        state_store,
        stats,
        session_started,
    )
}

/// Populate `view.commands` with the same metadata the /help command exposes.
fn inject_command_meta(view: &mut view::AppView, _commands: &CommandRegistry) {
    view.commands = crate::commands::builtin::builtin_help_entries()
        .into_iter()
        .map(|h| view::CommandMeta {
            name: h.name,
            description: h.description,
            arg_hint: h.arg_hint,
        })
        .collect();
}

/// Build `CmdEntry`s for the rustyline completer from the registered
/// commands' metadata.
fn cmd_entries_from_registry(_commands: &CommandRegistry) -> Vec<CmdEntry> {
    crate::commands::builtin::builtin_help_entries()
        .into_iter()
        .map(|h| CmdEntry {
            name: h.name,
            description: h.description,
            arg_hint: h.arg_hint,
        })
        .collect()
}
