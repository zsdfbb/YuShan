use async_trait::async_trait;

use super::{Command, CommandContext, CommandError, CommandResult, PromptError, Prompter};
use crate::provider::{AuthEntry, ProviderRegistry};

/// 生产 Prompter：包装 inquire，在 suspend 后的真实终端交互。
pub struct InquirePrompter;

impl Prompter for InquirePrompter {
    fn select(
        &self,
        prompt: &str,
        options: Vec<String>,
        page_size: usize,
    ) -> Result<String, PromptError> {
        inquire::Select::new(prompt, options)
            .with_page_size(page_size)
            .prompt()
            .map_err(map_inquire_err)
    }

    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError> {
        let mut q = inquire::Text::new(prompt);
        if let Some(h) = help {
            q = q.with_help_message(h);
        }
        q.prompt().map_err(map_inquire_err)
    }
}

fn map_inquire_err(e: inquire::InquireError) -> PromptError {
    match e {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            PromptError::Cancelled
        }
        e => PromptError::Other(e.to_string()),
    }
}

/// /help 展示所需的元数据。让 HelpCommand 与 registry 解耦。
pub struct HelpEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
}

/// 所有内置 command 的元数据，用于 /help 展示。
pub fn builtin_help_entries() -> Vec<HelpEntry> {
    vec![
        HelpEntry {
            name: "help",
            description: "Show available commands (or /help <name> for details)",
            arg_hint: Some("[command]"),
        },
        HelpEntry {
            name: "login",
            description: "Configure API credentials (interactive picker if no arg)",
            arg_hint: Some("[provider]"),
        },
        HelpEntry {
            name: "logout",
            description: "Clear API credentials and reset to default",
            arg_hint: None,
        },
        HelpEntry {
            name: "model",
            description: "Show or switch the current model (interactive picker if no arg)",
            arg_hint: Some("[model_name]"),
        },
        HelpEntry {
            name: "new",
            description: "Start a new conversation",
            arg_hint: None,
        },
        HelpEntry {
            name: "compact",
            description: "Compact conversation context",
            arg_hint: None,
        },
        HelpEntry {
            name: "status",
            description: "Show current configuration",
            arg_hint: None,
        },
        HelpEntry {
            name: "copy",
            description: "Copy last response to clipboard",
            arg_hint: None,
        },
        HelpEntry {
            name: "export",
            description: "Export conversation to file",
            arg_hint: Some("[filename]"),
        },
        HelpEntry {
            name: "quit",
            description: "Exit the agent",
            arg_hint: None,
        },
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
        "Show available commands (or /help <name> for details)"
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
            let entry = entries
                .iter()
                .find(|e| e.name == target)
                .ok_or_else(|| CommandError::UserError(format!("Unknown command: /{target}")))?;
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
        "Configure API credentials (interactive picker if no arg)"
    }

    fn arg_hint(&self) -> Option<&str> {
        Some("[provider]")
    }

    async fn execute(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        let providers = ctx.config.registry.providers().to_vec();
        let arg = args.trim();

        // 决定使用哪个 provider
        let provider = if arg.is_empty() {
            // 用方向键做交互式选择
            // 已存储 auth 的 provider 打 ✓ 标记
            let provider_labels: Vec<String> = providers
                .iter()
                .map(|p| {
                    let status = if ctx.config.registry.auth_for(&p.name).is_some() {
                        " ✓"
                    } else {
                        ""
                    };
                    if p.name == "custom" {
                        format!("Custom (manual URL){status}")
                    } else {
                        format!("{name} ({base}){status}", name = p.name, base = p.api_base)
                    }
                })
                .collect();

            let selection = ctx
                .prompter
                .select("Select a provider:", provider_labels, 10);

            match selection {
                Ok(label) => {
                    // 通过匹配 label 找到对应 provider
                    let idx = providers
                        .iter()
                        .position(|p| {
                            if p.name == "custom" {
                                label.starts_with("Custom")
                            } else {
                                label.starts_with(&p.name)
                            }
                        })
                        .ok_or_else(|| {
                            CommandError::Internal(format!("Could not find provider for: {label}"))
                        })?;
                    providers[idx].clone()
                }
                Err(PromptError::Cancelled) => {
                    println!("Login cancelled.");
                    return Ok(CommandResult::Continue);
                }
                Err(e) => {
                    return Err(CommandError::Internal(format!("Selection error: {e:?}")));
                }
            }
        } else {
            // 按名称匹配
            providers
                .iter()
                .find(|p| p.name == arg)
                .cloned()
                .ok_or_else(|| {
                    let names: Vec<String> = providers.iter().map(|p| p.name.clone()).collect();
                    CommandError::UserError(format!(
                        "Unknown provider: {arg}. Available: {}",
                        names.join(", ")
                    ))
                })?
        };

        // 对 custom provider，提示输入 api_base
        let api_base = if provider.api_base.is_empty() {
            let base = ctx
                .prompter
                .text("API base URL:", Some("e.g. https://api.deepseek.com"));

            match base {
                Ok(b) if !b.is_empty() => b,
                Ok(_) => {
                    return Err(CommandError::UserError("API base URL is required.".into()));
                }
                Err(PromptError::Cancelled) => {
                    println!("Login cancelled.");
                    return Ok(CommandResult::Continue);
                }
                Err(e) => {
                    return Err(CommandError::Internal(format!("Input error: {e:?}")));
                }
            }
        } else {
            provider.api_base.to_string()
        };

        // 提示输入 api_key
        let api_key = ctx.prompter.text(
            "API key:",
            Some("Your authentication key for this provider"),
        );

        let api_key = match api_key {
            Ok(k) if !k.is_empty() => k,
            Ok(_) => {
                return Err(CommandError::UserError("API key is required.".into()));
            }
            Err(PromptError::Cancelled) => {
                println!("Login cancelled.");
                return Ok(CommandResult::Continue);
            }
            Err(e) => {
                return Err(CommandError::Internal(format!("Input error: {e:?}")));
            }
        };

        let model_name = if provider.default_model.is_empty() {
            "deepseek-chat".to_string()
        } else {
            provider.default_model.to_string()
        };

        // 保存凭证到 registry（持久化到 auth.json）
        if let Err(e) = ctx.config.registry.save_auth(
            &provider.name,
            &AuthEntry {
                api_base: api_base.clone(),
                api_key: api_key.clone(),
                model: model_name.clone(),
            },
        ) {
            eprintln!("Warning: Could not persist credentials: {e}");
        }

        // 更新 config 字段
        ctx.config.api_base = Some(api_base.clone());
        ctx.config.api_key = Some(api_key.clone());
        ctx.config.model = model_name.clone();
        ctx.config.provider = Some(provider.name.clone());

        // 通过 factory 构建 model 并设到 agent
        match ctx.config.build_model() {
            Some(model) => {
                ctx.agent.set_model(Some(model));
            }
            None => {
                eprintln!("Warning: Could not build model. Check API credentials.");
            }
        }

        // 尽力而为：拉取 models 填充 cache
        match ProviderRegistry::fetch_models(&api_base, &api_key).await {
            crate::provider::FetchModelsResult::Success(models) => {
                println!("Fetched {} model(s).", models.len());
            }
            crate::provider::FetchModelsResult::AuthError(e) => {
                println!("Warning: Could not fetch models (auth error): {e}");
            }
            crate::provider::FetchModelsResult::NetworkError(e) => {
                println!("Warning: Could not fetch models (network error): {e}");
            }
        }

        if let Err(e) = ctx.state.save(&crate::state::AppState {
            last_active_provider: Some(provider.name.clone()),
            last_active_model: Some(model_name.clone()),
        }) {
            eprintln!("Warning: Could not persist state: {e}");
        }

        println!();
        println!("✓ Logged in to {} ({}).", provider.name, model_name);

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
        "Clear API credentials and reset to default"
    }

    async fn execute(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        // 若已设置 provider，则从 registry 移除其 auth
        if let Some(ref provider_name) = ctx.config.provider.clone() {
            if let Err(e) = ctx.config.registry.remove_auth(provider_name) {
                eprintln!("Warning: Could not remove persisted credentials: {e}");
            }
        }

        // 清空所有 config 字段
        ctx.config.api_base = None;
        ctx.config.api_key = None;
        ctx.config.provider = None;

        // 清空 agent 的 model
        ctx.agent.set_model(None);

        // 持久化已清空的状态
        let _ = ctx.state.save(&crate::state::AppState::default());

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
        "Show or switch the current model (interactive picker if no arg)"
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
            // 交互式 model 选择
            let models = if ctx.config.is_configured() {
                let api_base = ctx.config.api_base.as_deref().unwrap_or("");
                let api_key = ctx.config.api_key.as_deref().unwrap_or("");
                ctx.config
                    .registry
                    .available_models(api_base, api_key)
                    .await
            } else {
                ProviderRegistry::known_models_static()
            };

            let current = &ctx.config.model;
            let labels: Vec<String> = models
                .iter()
                .map(|m| {
                    if m.id == *current {
                        format!("✓ {}", m.id)
                    } else {
                        m.id.clone()
                    }
                })
                .collect();

            let selection = ctx.prompter.select("Select a model:", labels, 8);

            match selection {
                Ok(label) => {
                    let id = if let Some(rest) = label.strip_prefix("✓ ") {
                        rest.to_string()
                    } else {
                        label
                    };
                    let model = models.iter().find(|m| m.id == id).ok_or_else(|| {
                        CommandError::Internal(format!("Could not find model for: {id}"))
                    })?;
                    // 更新 config 并重建
                    ctx.config.model = model.id.clone();
                    let _ = ctx.state.save(&crate::state::AppState {
                        last_active_provider: ctx.config.provider.clone(),
                        last_active_model: Some(model.id.clone()),
                    });
                    match ctx.config.build_model() {
                        Some(m) => {
                            ctx.agent.set_model(Some(m));
                            println!("Model switched to: {}", model.id);
                        }
                        None => {
                            println!("Model set to: {}", model.id);
                            println!(
                                "Note: Cannot build model. Check API credentials with /login."
                            );
                        }
                    }
                }
                Err(PromptError::Cancelled) => {
                    // 不做任何事，保持当前 model
                }
                Err(e) => {
                    return Err(CommandError::Internal(format!("Selection error: {e:?}")));
                }
            }
        } else {
            // 直接指定名称：/model deepseek-chat
            ctx.config.model = target.to_string();
            let _ = ctx.state.save(&crate::state::AppState {
                last_active_provider: ctx.config.provider.clone(),
                last_active_model: Some(target.to_string()),
            });
            match ctx.config.build_model() {
                Some(m) => {
                    ctx.agent.set_model(Some(m));
                    println!("Model switched to: {target}");
                }
                None => {
                    println!("Model set to: {target}");
                    println!("Note: Cannot build model. Check API credentials with /login.");
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
        ctx.agent
            .clear_session()
            .await
            .map_err(|e| CommandError::Internal(format!("Failed to clear session: {e}")))?;
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
        // MVP：以清空 session 代替 compaction。
        ctx.agent
            .clear_session()
            .await
            .map_err(|e| CommandError::Internal(format!("Failed to compact session: {e}")))?;
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
        // 在 `tui-ratatui` 模式下，status 面板由 `ui/draw::draw_status_panel` 直接渲染，
        // 故 /status 是 no-op，`ctx` 未使用。
        // **c phase**: `tui-stdout` 已删除；tui-stdout 路径下的 `render_status` 调用
        // 也随 tui-stdout feature 一起删除；/status 在 ratatui 模式仅作为 ui 面板
        // 入口（draw_status_panel 自动渲染）。
        _ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        // v0：不显示 token 总数，因为 TurnStats 归 main.rs 所有，未穿过
        // CommandContext。后续工作：给 CommandContext 增加
        // &mut TurnStats（或 Arc<Mutex<>>）。
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
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::CommandRegistry;
    use crate::config::Config;
    use agent_event::CollectingSink;
    use agent_runtime::AgentBuilder;
    use agent_session::MemorySession;

    /// 辅助：用 mock model 构建测试 agent。
    fn test_agent(model_name: &str) -> agent_runtime::Agent {
        let model = agent_model::MockModel::new(model_name);
        AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap()
    }

    fn test_config() -> Config {
        let mut config = Config::from_env().unwrap();
        // 指向临时 auth.json，避免测试（如 logout）误写真实的 ~/.yushan/auth.json。
        let dir = std::env::temp_dir().join(format!(
            "yushan_test_auth_{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        config.registry.set_auth_override(dir.join("auth.json"));
        config
    }

    /// 辅助：构建指向临时目录的一次性 StateStore，
    /// 使测试永不触碰真实的 ~/.yushan/state.json。
    fn test_state_store() -> crate::state::StateStore {
        let mut store = crate::state::StateStore::new();
        let dir = std::env::temp_dir().join(format!(
            "yushan_test_state_{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.set_override(dir.join("state.json"));
        store
    }

    /// 测试用 Prompter：返回预设答案队列，记录调用。
    struct FakePrompter {
        answers: std::sync::Mutex<std::collections::VecDeque<Result<String, PromptError>>>,
    }

    impl FakePrompter {
        fn with_answers(answers: Vec<Result<String, PromptError>>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers.into()),
            }
        }

        fn next(&self) -> Result<String, PromptError> {
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(PromptError::Cancelled))
        }
    }

    impl Prompter for FakePrompter {
        fn select(
            &self,
            _prompt: &str,
            _options: Vec<String>,
            _page_size: usize,
        ) -> Result<String, PromptError> {
            self.next()
        }
        fn text(&self, _prompt: &str, _help: Option<&str>) -> Result<String, PromptError> {
            self.next()
        }
    }

    /// 测试用 Prompter：无交互，永远返回取消。用于不触发交互路径的测试。
    struct StubPrompter;

    impl Prompter for StubPrompter {
        fn select(
            &self,
            _prompt: &str,
            _options: Vec<String>,
            _page_size: usize,
        ) -> Result<String, PromptError> {
            Err(PromptError::Cancelled)
        }
        fn text(&self, _prompt: &str, _help: Option<&str>) -> Result<String, PromptError> {
            Err(PromptError::Cancelled)
        }
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
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = HelpCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        // HelpCommand 打印到 stdout；验证它不 panic。
    }

    #[tokio::test]
    async fn test_help_specific_command() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = HelpCommand.execute("quit", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_help_unknown_command() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let err = HelpCommand
            .execute("nonexistent", &mut ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::UserError(_)));
    }

    #[tokio::test]
    async fn test_quit_returns_exit() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = QuitCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Exit));
    }

    #[tokio::test]
    async fn test_unknown_returns_user_error() {
        let reg = build_test_registry();
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = reg.execute("/foobar", &mut ctx).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            CommandError::UserError(msg) => {
                assert!(
                    msg.contains("foobar"),
                    "error should mention the bad command name"
                );
            }
            other => panic!("expected UserError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_model_with_args() {
        let mut agent = test_agent("old-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        // /model 带参数直接切换（不走交互式选择器）
        let result = ModelCommand
            .execute("deepseek-chat", &mut ctx)
            .await
            .unwrap();
        assert!(matches!(result, CommandResult::Continue));
        // Config.model 应被更新
        assert_eq!(ctx.config.model, "deepseek-chat");
    }

    #[tokio::test]
    async fn test_model_with_args_sets_config() {
        let mut agent = test_agent("old-model");
        let mut config = test_config();

        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = ModelCommand
            .execute("deepseek-reasoner", &mut ctx)
            .await
            .unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert_eq!(ctx.config.model, "deepseek-reasoner");
    }

    #[test]
    fn test_model_no_args_returns_continue() {
        // /model 不带参数会打开交互式选择器（inquire::Select），
        // 无法在测试中运行。仅验证 command 存在。
        assert_eq!(ModelCommand.name(), "model");
        assert!(ModelCommand.arg_hint().is_some());
    }

    #[tokio::test]
    async fn test_new_clears_session() {
        let model = agent_model::MockModel::new("test");
        model.push_text("hello");
        let mut agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        // 跑一个 turn 以添加消息
        agent
            .run_turn(agent_loop::AgentInput::text("hi"))
            .await
            .unwrap();
        assert!(!agent.session_messages().is_empty());

        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = NewCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.agent.session_messages().is_empty());
    }

    #[tokio::test]
    async fn test_status_shows_config() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = StatusCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_copy_mvp() {
        let mut agent = test_agent("test");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = CopyCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_export_mvp() {
        let mut agent = test_agent("test");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = ExportCommand.execute("output.md", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
    }

    #[tokio::test]
    async fn test_compact_mvp() {
        let model = agent_model::MockModel::new("test");
        model.push_text("hello");
        let mut agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        agent
            .run_turn(agent_loop::AgentInput::text("hi"))
            .await
            .unwrap();
        assert!(!agent.session_messages().is_empty());

        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = CompactCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.agent.session_messages().is_empty());
    }

    #[tokio::test]
    async fn test_logout_clears_model() {
        let mut agent = test_agent("test-model");
        assert!(agent.model_id().is_some());

        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = LogoutCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.agent.model_id().is_none());
    }

    #[tokio::test]
    async fn test_logout_clears_config_fields() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();

        // 设置 config 字段
        config.api_base = Some("https://api.example.com".into());
        config.api_key = Some("sk-test".into());
        config.provider = Some("deepseek".into());

        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = LogoutCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));

        // 所有 config 字段都应被清空
        assert!(ctx.config.api_base.is_none());
        assert!(ctx.config.api_key.is_none());
        assert!(ctx.config.provider.is_none());
        assert!(ctx.agent.model_id().is_none());
    }

    #[tokio::test]
    async fn test_logout_removes_from_auth_json() {
        let dir = std::env::temp_dir().join("yushan_test_logout_persist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let auth_path = dir.join("auth.json");

        let mut registry = ProviderRegistry::new();
        registry.set_auth_override(auth_path.clone());
        registry
            .save_auth(
                "deepseek",
                &AuthEntry {
                    api_base: "https://api.deepseek.com".into(),
                    api_key: "sk-test-logout".into(),
                    model: "deepseek-chat".into(),
                },
            )
            .unwrap();

        let mut config = test_config();
        config.registry = registry;
        config.provider = Some("deepseek".into());

        let mut agent = test_agent("test-model");
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = LogoutCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));

        // 创建新 registry，验证文件已更新到磁盘
        let mut registry2 = ProviderRegistry::new();
        registry2.set_auth_override(auth_path);
        registry2.load_auth();
        assert!(registry2.auth_for("deepseek").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_login_rejects_unknown_provider() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let err = LoginCommand
            .execute("nonexistent", &mut ctx)
            .await
            .unwrap_err();
        match err {
            CommandError::UserError(msg) => {
                assert!(msg.contains("Unknown provider"), "msg: {msg}");
                assert!(msg.contains("nonexistent"), "msg: {msg}");
            }
            other => panic!("expected UserError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_logout_clears_state() {
        let dir = std::env::temp_dir().join("yushan_test_logout_state");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("state.json");

        let mut config = test_config();
        config.api_base = Some("https://api.example.com".into());
        config.api_key = Some("sk-test".into());
        config.provider = Some("deepseek".into());
        config.model = "deepseek-chat".into();

        let mut state_store = test_state_store();
        state_store.set_override(state_path.clone());
        // 预先填充 state.json，模拟之前的一次活跃会话
        state_store
            .save(&crate::state::AppState {
                last_active_provider: Some("deepseek".into()),
                last_active_model: Some("deepseek-chat".into()),
            })
            .unwrap();

        let mut agent = test_agent("test-model");
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };
        LogoutCommand.execute("", &mut ctx).await.unwrap();

        let loaded = state_store.load();
        assert_eq!(loaded.last_active_provider, None);
        assert_eq!(loaded.last_active_model, None);

        let _ = std::fs::remove_dir_all(&dir);
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
                "help", "login", "logout", "model", "new", "compact", "status", "copy", "export",
                "quit"
            ]
        );
    }

    #[test]
    fn test_help_enhanced_descriptions() {
        let entries = builtin_help_entries();
        let login = entries.iter().find(|e| e.name == "login").unwrap();
        assert!(
            login.description.contains("interactive picker"),
            "login description should mention interactive picker: {}",
            login.description
        );
        let model = entries.iter().find(|e| e.name == "model").unwrap();
        assert!(
            model.description.contains("interactive picker"),
            "model description should mention interactive picker: {}",
            model.description
        );
        let logout = entries.iter().find(|e| e.name == "logout").unwrap();
        assert!(
            logout.description.contains("reset"),
            "logout description should mention reset: {}",
            logout.description
        );
        let help = entries.iter().find(|e| e.name == "help").unwrap();
        assert!(
            help.description.contains("/help <name>"),
            "help description should mention /help <name>: {}",
            help.description
        );
    }

    // ---- 交互决策逻辑测试（FakePrompter 注入） ----

    /// /model 无参：config 未配置时走 static models（不触发网络），
    /// select 返回 "deepseek-chat" → config.model 切换。
    #[tokio::test]
    async fn test_model_interactive_selects_model() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        // test_config 未设置 api_base/api_key → is_configured()==false → 走 static models
        let prompter = FakePrompter::with_answers(vec![Ok("deepseek-chat".into())]);
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = ModelCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert_eq!(ctx.config.model, "deepseek-chat");
    }

    /// /model 无参：select 取消 → 保持原 model 不变。
    #[tokio::test]
    async fn test_model_interactive_cancelled_keeps_model() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        config.model = "deepseek-chat".into();
        let mut state_store = test_state_store();
        let prompter = FakePrompter::with_answers(vec![Err(PromptError::Cancelled)]);
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = ModelCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert_eq!(ctx.config.model, "deepseek-chat", "cancel keeps model");
    }

    /// /login 无参：provider 选择时取消 → Continue，不持久化 auth。
    #[tokio::test]
    async fn test_login_cancelled_at_provider_select() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        let prompter = FakePrompter::with_answers(vec![Err(PromptError::Cancelled)]);
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = LoginCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.config.api_key.is_none(), "no auth persisted on cancel");
    }

    /// /login 无参：provider 选 deepseek 后，api_key 输入取消 → Continue，不持久化。
    #[tokio::test]
    async fn test_login_cancelled_at_api_key() {
        let mut agent = test_agent("test-model");
        let mut config = test_config();
        let mut state_store = test_state_store();
        // deepseek 非 custom → 跳过 api_base 输入；第二个答案是 api_key 输入
        let prompter = FakePrompter::with_answers(vec![
            Ok("deepseek (https://api.deepseek.com)".into()),
            Err(PromptError::Cancelled),
        ]);
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };

        let result = LoginCommand.execute("", &mut ctx).await.unwrap();
        assert!(matches!(result, CommandResult::Continue));
        assert!(ctx.config.api_key.is_none(), "no auth persisted on cancel");
    }
}
