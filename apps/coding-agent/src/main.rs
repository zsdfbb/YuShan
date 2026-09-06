mod commands;
mod config;
mod provider;
mod prompt;
mod tui;

use agent_event::NoopEventSink;
use agent_loop::AgentInput;
use agent_model_openai_compatible::{
    OpenAICompatibleConfig, OpenAICompatibleModel,
};
use agent_runtime::AgentBuilder;
use agent_session::MemorySession;
use agent_tools_basic::{BashTool, EditTool, ReadTool, WriteTool};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mut config = config::Config::from_env()?;

    // Load saved credentials from auth.json
    config.registry.load_auth();

    // Startup recovery: if env vars don't provide full credentials,
    // try to restore from auth.json by finding the first stored provider.
    if !config.is_configured() {
        for provider in config.registry.providers() {
            if let Some(entry) = config.registry.auth_for(&provider.name) {
                config.api_base = Some(entry.api_base.clone());
                config.api_key = Some(entry.api_key.clone());
                config.model = entry.model.clone();
                config.provider = Some(provider.name.clone());
                break;
            }
        }
    }

    // Register the model factory (adapter-specific construction logic)
    config.set_model_factory(|cfg| {
        let base = cfg.api_base.as_ref()?;
        let key = cfg.api_key.as_ref()?;
        let compat = cfg.current_compat();
        Some(Box::new(OpenAICompatibleModel::new(
            OpenAICompatibleConfig {
                api_base: base.clone(),
                api_key: key.clone(),
                model: cfg.model.clone(),
                max_tokens: Some(4096),
                temperature: Some(0.7),
                compat,
            },
        )))
    });

    // Build the command registry
    let command_registry = commands::build_registry();

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

    // Build model (optional — agent can start without API credentials)
    let model = if config.is_configured() {
        let compat = config.current_compat();
        Some(OpenAICompatibleModel::new(OpenAICompatibleConfig {
            api_base: config.api_base.clone().unwrap(),
            api_key: config.api_key.clone().unwrap(),
            model: config.model.clone(),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            compat,
        }))
    } else {
        None
    };

    // Build workspace for tools
    let workspace = config.cwd.clone();

    // Build agent
    let mut builder = AgentBuilder::new()
        .tool(ReadTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(EditTool::new(workspace.clone()))
        .tool(BashTool::new(workspace.clone()))
        .system_prompt(system_prompt)
        .working_dir(workspace.clone(), workspace.clone())
        .approval(agent_tool::AutoApprove)
        .session(MemorySession::new())
        .events(NoopEventSink);

    if let Some(model) = model {
        builder = builder.model(model);
    }

    let mut agent = builder.build()?;

    if let Some(task) = task {
        // Print mode — requires model
        if !agent.is_configured() {
            eprintln!("Error: No model configured.");
            eprintln!("Set environment variables:");
            eprintln!("  YUSHAN_API_BASE  — API endpoint URL");
            eprintln!("  YUSHAN_API_KEY   — API authentication key");
            eprintln!("  YUSHAN_MODEL     — Model name (optional, default: deepseek-chat)");
            eprintln!("Or run in interactive mode and use /login to configure.");
            std::process::exit(1);
        }
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
        if !agent.is_configured() {
            println!("No API credentials configured. Use /login to set up, or set environment variables:");
            println!("  YUSHAN_API_BASE  — API endpoint URL");
            println!("  YUSHAN_API_KEY   — API authentication key");
            println!();
        }
        tui::run_interactive(&mut agent, &mut config, &command_registry).await?;
    }

    Ok(())
}
