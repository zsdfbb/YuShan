use agent_loop::AgentInput;
use agent_runtime::Agent;
use std::io::{self, Write};

use crate::commands::{CommandContext, CommandRegistry, CommandResult};
use crate::config::Config;

pub async fn run_interactive(
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("YuShan Coding Agent (type /help for commands, 'exit' to quit)");
    println!();

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
                    commands,
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
            }
            Err(e) => {
                eprintln!("Error: {e}");
                println!();
            }
        }
    }

    Ok(())
}
