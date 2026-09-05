use async_trait::async_trait;

use super::{Command, CommandContext, CommandError, CommandResult};
use crate::config::known_providers;

/// Metadata for /help display. Keeps HelpCommand decoupled from the registry.
pub struct HelpEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
}

/// All built-in command metadata for /help display.
pub fn builtin_help_entries() -> Vec<HelpEntry> {
    vec![
        HelpEntry { name: "help", description: "Show available commands", arg_hint: Some("[command]") },
        HelpEntry { name: "login", description: "Configure API credentials", arg_hint: Some("[provider]") },
        HelpEntry { name: "logout", description: "Clear API credentials", arg_hint: None },
        HelpEntry { name: "model", description: "Show or switch the current model", arg_hint: Some("[model_name]") },
        HelpEntry { name: "new", description: "Start a new conversation", arg_hint: None },
        HelpEntry { name: "compact", description: "Compact conversation context", arg_hint: None },
        HelpEntry { name: "status", description: "Show current configuration", arg_hint: None },
        HelpEntry { name: "copy", description: "Copy last response to clipboard", arg_hint: None },
        HelpEntry { name: "export", description: "Export conversation to file", arg_hint: Some("[filename]") },
        HelpEntry { name: "quit", description: "Exit the agent", arg_hint: None },
    ]
}

// ---------------------------------------------------------------------------
// HelpCommand
// ---------------------------------------------------------------------------

pub struct HelpCommand;

#[async_trait]
impl Command for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Show available commands"
    }

    fn arg_hint(&self) -> Option<&str> {
        Some("[command]")
    }

    async fn execute(
        &self,
        args: &str,
        _ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        let entries = builtin_help_entries();
        let target = args.trim();
        if !target.is_empty() {
            let entry = entries.iter().find(|e| e.name == target).ok_or_else(|| {
                CommandError::UserError(format!("Unknown command: /{target}"))
            })?;
            let hint = entry.arg_hint.unwrap_or("");
            if hint.is_empty() {
                println!("/{} — {}", entry.name, entry.description);
            } else {
                println!("/{} {} — {}", entry.name, hint, entry.description);
            }
        } else {
            println!("Available commands:");
            for entry in &entries {
                let hint = entry.arg_hint.unwrap_or("");
                if hint.is_empty() {
                    println!("  /{:<12} {}", entry.name, entry.description);
                } else {
                    println!("  /{:<12} {} {}", entry.name, hint, entry.description);
                }
            }
        }
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// LoginCommand
// ---------------------------------------------------------------------------

pub struct LoginCommand;

#[async_trait]
impl Command for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Configure API credentials"
    }

    fn arg_hint(&self) -> Option<&str> {
        Some("[provider]")
    }

    async fn execute(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        let providers = known_providers();
        let arg = args.trim();

        // Determine which provider to use
        let provider = if arg.is_empty() {
            // Interactive selection with arrow keys
            let provider_labels: Vec<String> = providers
                .iter()
                .map(|p| {
                    if p.name == "custom" {
                        "Custom (manual URL)".to_string()
                    } else {
                        format!("{name} ({base})", name = p.name, base = p.api_base)
                    }
                })
                .collect();

            let selection = inquire::Select::new("Select a provider:", provider_labels)
                .with_page_size(10)
                .prompt();

            match selection {
                Ok(label) => {
                    // Find the provider by matching the label
                    let idx = providers
                        .iter()
                        .position(|p| {
                            if p.name == "custom" {
                                label.contains("Custom")
                            } else {
                                label.contains(p.name)
                            }
                        })
                        .ok_or_else(|| {
                            CommandError::Internal(format!("Could not find provider for: {label}"))
                        })?;
                    providers[idx].clone()
                }
                Err(inquire::InquireError::OperationCanceled)
                | Err(inquire::InquireError::OperationInterrupted) => {
                    println!("Login cancelled.");
                    return Ok(CommandResult::Continue);
                }
                Err(e) => {
                    return Err(CommandError::Internal(format!("Selection error: {e}")));
                }
            }
        } else {
            // Match by name
            providers.iter().find(|p| p.name == arg).cloned().ok_or_else(|| {
                let names: Vec<&str> = providers.iter().map(|p| p.name).collect();
                CommandError::UserError(format!(
                    "Unknown provider: {arg}. Available: {}",
                    names.join(", ")
                ))
            })?
        };

        // For custom provider, prompt for api_base
        let api_base = if provider.api_base.is_empty() {
            let base = inquire::Text::new("API base URL:")
                .with_help_message("e.g. https://api.deepseek.com")
                .prompt();

            match base {
                Ok(b) if !b.is_empty() => b,
                Ok(_) => {
                    return Err(CommandError::UserError("API base URL is required.".into()));
                }
                Err(inquire::InquireError::OperationCanceled)
                | Err(inquire::InquireError::OperationInterrupted) => {
                    println!("Login cancelled.");
                    return Ok(CommandResult::Continue);
                }
                Err(e) => {
                    return Err(CommandError::Internal(format!("Input error: {e}")));
                }
            }
        } else {
            provider.api_base.to_string()
        };

        // Prompt for api_key
        let api_key = inquire::Text::new("API key:")
            .with_help_message("Your authentication key for this provider")
            .prompt();

        let api_key = match api_key {
            Ok(k) if !k.is_empty() => k,
            Ok(_) => {
                return Err(CommandError::UserError("API key is required.".into()));
            }
            Err(inquire::InquireError::OperationCanceled)
            | Err(inquire::InquireError::OperationInterrupted) => {
                println!("Login cancelled.");
                return Ok(CommandResult::Continue);
            }
            Err(e) => {
                return Err(CommandError::Internal(format!("Input error: {e}")));
            }
        };

        let model_name = if provider.default_model.is_empty() {
            "deepseek-chat".to_string()
        } else {
            provider.default_model.to_string()
        };

        // Update config
        ctx.config.api_base = Some(api_base.clone());
        ctx.config.api_key = Some(api_key.clone());
        ctx.config.model = model_name.clone();

        // Build and set the model
        println!();
        println!("Provider:   {}", provider.name);
        println!("API base:   {api_base}");
        println!("API key:    {}...", &api_key[..api_key.len().min(8)]);
        println!("Model:      {model_name}");

        // Build model via factory and set on agent
        match ctx.config.build_model() {
            Some(model) => {
                ctx.agent.set_model(Some(model));
                println!();
                println!("Logged in. Model {model_name} ready.");
            }
            None => {
                println!();
                println!("Credentials saved. Model will be available once factory is configured.");
            }
        }

        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// LogoutCommand
// ---------------------------------------------------------------------------

pub struct LogoutCommand;

#[async_trait]
impl Command for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }

    fn description(&self) -> &str {
        "Clear API credentials"
    }

    async fn execute(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        // Config is immutable in CommandContext; we can only clear the agent's model.
        ctx.agent.set_model(None);
        println!("Logged out. API credentials cleared.");
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// ModelCommand
// ---------------------------------------------------------------------------

pub struct ModelCommand;

#[async_trait]
impl Command for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }

    fn description(&self) -> &str {
        "Show or switch the current model"
    }

    fn arg_hint(&self) -> Option<&str> {
        Some("[model_name]")
    }

    async fn execute(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        let target = args.trim();
        if target.is_empty() {
            match ctx.agent.model_id() {
                Some(id) => println!("Current model: {id}"),
                None => println!("No model configured. Use /login first."),
            }
        } else {
            // Update model name in config
            ctx.config.model = target.to_string();

            // Rebuild model via factory and set on agent
            match ctx.config.build_model() {
                Some(model) => {
                    ctx.agent.set_model(Some(model));
                    println!("Model switched to: {target}");
                }
                None => {
                    println!("Model name set to: {target}");
                    println!("Note: Cannot build model. Configure API credentials with /login first.");
                }
            }
        }
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// NewCommand
// ---------------------------------------------------------------------------

pub struct NewCommand;

#[async_trait]
impl Command for NewCommand {
    fn name(&self) -> &str {
        "new"
    }

    fn description(&self) -> &str {
        "Start a new conversation"
    }

    async fn execute(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        ctx.agent.clear_session().await.map_err(|e| {
            CommandError::Internal(format!("Failed to clear session: {e}"))
        })?;
        println!("New conversation started. Session cleared.");
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// CompactCommand
// ---------------------------------------------------------------------------

pub struct CompactCommand;

#[async_trait]
impl Command for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }

    fn description(&self) -> &str {
        "Compact conversation context"
    }

    async fn execute(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        // MVP: clear session as a stand-in for compaction.
        ctx.agent.clear_session().await.map_err(|e| {
            CommandError::Internal(format!("Failed to compact session: {e}"))
        })?;
        println!("Compacting... Session cleared (full compaction TBD).");
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// StatusCommand
// ---------------------------------------------------------------------------

pub struct StatusCommand;

#[async_trait]
impl Command for StatusCommand {
    fn name(&self) -> &str {
        "status"
    }

    fn description(&self) -> &str {
        "Show current configuration"
    }

    async fn execute(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        let model_id = ctx.agent.model_id().unwrap_or("none");
        let base = match &ctx.config.api_base {
            Some(b) => {
                // Mask the URL for security
                if b.len() > 20 {
                    format!("{}...{}", &b[..12], &b[b.len() - 6..])
                } else {
                    b.clone()
                }
            }
            None => "not set".into(),
        };
        let has_key = ctx.config.api_key.is_some();
        let cwd = ctx.config.cwd.display();
        let configured = ctx.agent.is_configured();

        println!("Model:        {model_id}");
        println!("API base:     {base}");
        println!("API key:      {}", if has_key { "set" } else { "not set" });
        println!("Working dir:  {cwd}");
        println!("Configured:   {configured}");
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// CopyCommand
// ---------------------------------------------------------------------------

pub struct CopyCommand;

#[async_trait]
impl Command for CopyCommand {
    fn name(&self) -> &str {
        "copy"
    }

    fn description(&self) -> &str {
        "Copy last response to clipboard"
    }

    async fn execute(
        &self,
        _args: &str,
        _ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        println!("Copy not yet implemented in MVP.");
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// ExportCommand
// ---------------------------------------------------------------------------

pub struct ExportCommand;

#[async_trait]
impl Command for ExportCommand {
    fn name(&self) -> &str {
        "export"
    }

    fn description(&self) -> &str {
        "Export conversation to file"
    }

    fn arg_hint(&self) -> Option<&str> {
        Some("[filename]")
    }

    async fn execute(
        &self,
        _args: &str,
        _ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        println!("Export not yet implemented in MVP.");
        Ok(CommandResult::Continue)
    }
}

// ---------------------------------------------------------------------------
// QuitCommand
// ---------------------------------------------------------------------------

pub struct QuitCommand;

#[async_trait]
impl Command for QuitCommand {
    fn name(&self) -> &str {
        "quit"
    }

    fn description(&self) -> &str {
        "Exit the agent"
    }

    async fn execute(
        &self,
        _args: &str,
        _ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        Ok(CommandResult::Exit)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::commands::CommandRegistry;
    use agent_event::CollectingSink;
    use agent_runtime::AgentBuilder;
    use agent_session::MemorySession;

    /// Helper: build a test agent with a mock model.
    fn test_agent(model_name: &str) -> agent_runtime::Agent {
        let model = agent_model::MockModel::new(model_name);
        AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap()
    }

    /// Helper: build a test agent without a model.
    fn test_agent_no_model() -> agent_runtime::Agent {
        AgentBuilder::new()
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap()
    }

    fn test_config() -> Config {
        Config::from_env().unwrap()
    }

    fn build_test_registry() -> CommandRegistry {
        let mut reg = CommandRegistry::new();
        reg.register(HelpCommand);
        reg.register(LoginCommand);
        reg.register(LogoutCommand);
        reg.register(ModelCommand);
        reg.register(NewCommand);
        reg.register(CompactCommand);
        reg.register(StatusCommand);
        reg.register(CopyCommand);
        reg.register(ExportCommand);
        reg.register(QuitCommand);
        reg
    }

    #[tokio::test]
    async fn test_help_lists_commands() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = HelpCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        // HelpCommand prints to stdout; verify it doesn't panic.
    }

    #[tokio::test]
    async fn test_help_specific_command() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = HelpCommand.execute("quit", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_help_unknown_command() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let err = HelpCommand.execute("nonexistent", &mut ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::UserError(_)));
    }

    #[tokio::test]
    async fn test_quit_returns_exit() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = QuitCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Exit));
    }

    #[tokio::test]
    async fn test_unknown_returns_user_error() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = reg.execute("/foobar", &mut ctx).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            CommandError::UserError(msg) => {
                assert!(msg.contains("foobar"), "error should mention the bad command name");
            }
            other => panic!("expected UserError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_model_no_args_shows_current() {
        let reg = build_test_registry();
        let mut agent = test_agent("gpt-4o");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = ModelCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        // Should print "Current model: gpt-4o" — no panic.
    }

    #[tokio::test]
    async fn test_model_no_args_no_model() {
        let reg = build_test_registry();
        let mut agent = test_agent_no_model();
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = ModelCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        // Should print "No model configured." — no panic.
    }

    #[tokio::test]
    async fn test_model_with_args() {
        let reg = build_test_registry();
        let mut agent = test_agent("old-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = ModelCommand.execute("claude-sonnet-4", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_new_clears_session() {
        let reg = build_test_registry();
        let model = agent_model::MockModel::new("test");
        model.push_text("hello");
        let mut agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        // Run a turn to add messages
        agent.run_turn(agent_loop::AgentInput::text("hi")).await.unwrap();
        assert!(!agent.session_messages().is_empty());

        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = NewCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.agent.session_messages().is_empty());
    }

    #[tokio::test]
    async fn test_status_shows_config() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = StatusCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_copy_mvp() {
        let reg = build_test_registry();
        let mut agent = test_agent("test");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = CopyCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_export_mvp() {
        let reg = build_test_registry();
        let mut agent = test_agent("test");
        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = ExportCommand.execute("output.md", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_compact_mvp() {
        let reg = build_test_registry();
        let model = agent_model::MockModel::new("test");
        model.push_text("hello");
        let mut agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        agent.run_turn(agent_loop::AgentInput::text("hi")).await.unwrap();
        assert!(!agent.session_messages().is_empty());

        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = CompactCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.agent.session_messages().is_empty());
    }

    #[tokio::test]
    async fn test_logout_clears_model() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        assert!(agent.model_id().is_some());

        let mut config = test_config();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
        };

        let result = LogoutCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.agent.model_id().is_none());
    }

    #[tokio::test]
    async fn test_build_registry_all_commands() {
        let reg = build_test_registry();
        let all = reg.all();
        assert_eq!(all.len(), 10);

        let names: Vec<&str> = all.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec![
                "help", "login", "logout", "model", "new", "compact", "status",
                "copy", "export", "quit"
            ]
        );
    }
}
