use agent_loop::AgentInput;
use agent_runtime::Agent;
use std::io::{self, Write};

use crate::commands::{CommandContext, CommandRegistry, CommandResult};
use crate::config::Config;
use crate::format;
use crate::status::TurnStats;

pub async fn run_interactive(
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    format::print_banner(&mut stdout, config, agent.model_id())?;
    drop(stdout);

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        // Intercept slash commands
        if input.starts_with('/') {
            let result = {
                let mut ctx = CommandContext {
                    agent,
                    config,
                };
                match commands.execute(input, &mut ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        CommandResult::Continue
                    }
                }
            };
            match result {
                CommandResult::Continue => continue,
                CommandResult::Exit => break,
            }
        }

        // Normal agent turn
        let agent_input = AgentInput::text(input);
        match agent.run_turn(agent_input).await {
            Ok(result) => {
                if let Some(msg) = &result.final_message {
                    for block in &msg.content {
                        if let agent_core::ContentBlock::Text { text } = block {
                            println!("{text}");
                        }
                    }
                }
                println!();
                let mut stdout = io::stdout().lock();
                stats.record(&result.usage);
                format::print_turn_summary(&mut stdout, stats, result.rounds, &result.stop_reason)?;
                drop(stdout);
                println!();
            }
            Err(e) => {
                eprintln!("Error: {e}");
                println!();
            }
        }
    }

    Ok(())
}
