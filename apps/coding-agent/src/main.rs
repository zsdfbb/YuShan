mod config;
mod prompt;
mod tui;

use agent_event::NoopEventSink;
use agent_loop::AgentInput;
use agent_model_openai_compatible::{
    OpenAICompatibleConfig, OpenAICompatibleModel, compat::ProviderCompat,
};
use agent_runtime::AgentBuilder;
use agent_session::MemorySession;
use agent_tools_basic::{BashTool, EditTool, ReadTool, WriteTool};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let config = config::Config::from_env()?;

    // Parse simple args
    let task = if args.len() > 2 && args[1] == "-p" {
        // Print mode: yushan-coding-agent -p "task"
        Some(args[2..].join(" "))
    } else {
        // Interactive mode
        None
    };

    // Build system prompt
    let cwd = config.cwd.clone();
    let system_prompt = prompt::build_system_prompt(&cwd);

    // Build model
    let model = OpenAICompatibleModel::new(OpenAICompatibleConfig {
        api_base: config.api_base,
        api_key: config.api_key,
        model: config.model,
        max_tokens: Some(4096),
        temperature: Some(0.7),
        compat: ProviderCompat::standard(),
    });

    // Build workspace for tools
    let workspace = config.cwd.clone();

    // Build agent
    let mut agent = AgentBuilder::new()
        .model(model)
        .tool(ReadTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(EditTool::new(workspace.clone()))
        .tool(BashTool::new(workspace.clone()))
        .system_prompt(system_prompt)
        .working_dir(workspace.clone(), workspace.clone())
        .approval(agent_tool::AutoApprove)
        .session(MemorySession::new())
        .events(NoopEventSink)
        .build()?;

    if let Some(task) = task {
        // Print mode
        let input = AgentInput::text(&task);
        let result = agent.run_turn(input).await?;
        if let Some(msg) = &result.final_message {
            for block in &msg.content {
                if let agent_core::ContentBlock::Text { text } = block {
                    println!("{text}");
                }
            }
        }
    } else {
        // Interactive mode
        tui::run_interactive(&mut agent).await?;
    }

    Ok(())
}
