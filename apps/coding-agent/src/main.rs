mod ansi;
mod commands;
mod config;
mod format;
mod prompt;
mod provider;
mod state;
mod status;
mod view;

#[cfg(feature = "tui-ratatui")]
mod ui;

use agent_event::NoopEventSink;
use agent_loop::AgentInput;
use agent_model_openai_compatible::{OpenAICompatibleConfig, OpenAICompatibleModel};
use agent_runtime::AgentBuilder;
use agent_session::MemorySession;
use agent_tools_basic::{BashTool, EditTool, ReadTool, WriteTool};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mut config = config::Config::from_env()?;

    // 从 auth.json 加载已保存的凭证
    config.registry.load_auth();

    // 从 state.json 加载已保存的会话状态
    let mut state_store = state::StateStore::new();
    let saved_state = state_store.load();

    // 启动恢复：若环境变量未提供完整凭证，
    // 先尝试从 state.json（last-active provider+model）恢复，
    // 再回退到第一个存有 auth 的 provider。
    if !config.is_configured() {
        // 尝试 state.json 恢复：last_active_provider + 匹配的 auth entry
        let recovered = saved_state
            .last_active_provider
            .as_deref()
            .and_then(|name| config.registry.find_provider(name))
            .zip(
                saved_state
                    .last_active_provider
                    .as_deref()
                    .and_then(|name| config.registry.auth_for(name)),
            );

        if let Some((provider, entry)) = recovered {
            config.api_base = Some(entry.api_base.clone());
            config.api_key = Some(entry.api_key.clone());
            config.model = saved_state
                .last_active_model
                .clone()
                .unwrap_or_else(|| entry.model.clone());
            config.provider = Some(provider.name.clone());
        } else {
            // 回退：第一个存有凭证的 provider
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
    }

    // 注册 model factory（适配器特有的构建逻辑）
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

    // 构建 command registry
    let command_registry = commands::build_registry();

    // 解析简单参数
    let mut task: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                if i + 1 < args.len() {
                    task = Some(args[i + 1..].join(" "));
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }

    // 构建 system prompt
    let cwd = config.cwd.clone();
    let system_prompt = prompt::build_system_prompt(&cwd);

    // 构建 model（可选——agent 可在无 API 凭证时启动）
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

    // 为工具构建 workspace
    let workspace = config.cwd.clone();

    // 构建 agent
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

    let mut stats = status::TurnStats::default();

    if let Some(task) = task {
        // 打印模式——需要 model
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
        // 交互模式——仅 ratatui（tui-stdout 路径已在 c 阶段删除）。
        // `state_store` 与恢复逻辑已在入口早期完成（line 33-73）。
        #[cfg(feature = "tui-ratatui")]
        {
            ui::run(
                &mut agent,
                &mut config,
                &command_registry,
                &mut stats,
                &mut state_store,
            )
            .await?;
        }
        #[cfg(not(feature = "tui-ratatui"))]
        {
            return Err(
                "ratatui mode required for interactive TUI; build with --features tui-ratatui"
                    .into(),
            );
        }
    }

    Ok(())
}
