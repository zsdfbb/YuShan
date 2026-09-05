use agent_loop::AgentInput;
use agent_runtime::Agent;
use std::io::{self, Write};

pub async fn run_interactive(agent: &mut Agent) -> Result<(), Box<dyn std::error::Error>> {
    println!("YuShan Coding Agent (type 'exit' to quit)");
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
