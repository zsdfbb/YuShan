//! Slash command system for the coding-agent TUI.

pub mod builtin;

pub use builtin::*;

use std::collections::HashMap;

use async_trait::async_trait;

use crate::config::Config;
use crate::state::StateStore;
use agent_runtime::Agent;

/// A slash command that can be registered in the CommandRegistry.
#[async_trait]
#[allow(dead_code)] // description / arg_hint are part of the public Command
// contract but not consumed by v0 render paths (completer
// uses its own CmdEntry; /help reads builtin_help_entries).
pub trait Command: Send + Sync {
    /// Command name without the leading `/` (e.g., "help", "model").
    fn name(&self) -> &str;

    /// One-line description shown in /help output.
    fn description(&self) -> &str;

    /// Optional argument hint shown in /help (e.g., "<model_name>").
    fn arg_hint(&self) -> Option<&str> {
        None
    }

    /// Execute the command.
    async fn execute(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError>;
}

/// The mutable world a command can touch during execution.
pub struct CommandContext<'a> {
    pub agent: &'a mut Agent,
    pub config: &'a mut Config,
    pub state: &'a mut StateStore,
}

/// What the TUI loop should do after a command finishes.
#[derive(Debug)]
pub enum CommandResult {
    /// Continue the REPL loop normally.
    Continue,
    /// Exit the REPL.
    Exit,
}

/// Errors from command execution.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// User-recoverable error (e.g., "No API key configured").
    #[error("{0}")]
    UserError(String),
    /// Internal error (e.g., IO failure).
    #[error("internal error: {0}")]
    Internal(String),
}

/// Registry of available slash commands.
pub struct CommandRegistry {
    commands: Vec<Box<dyn Command>>,
    by_name: HashMap<String, usize>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Register a command. Panics on duplicate name (caught at startup).
    pub fn register(&mut self, cmd: impl Command + 'static) {
        let name = cmd.name().to_string();
        let idx = self.commands.len();
        self.commands.push(Box::new(cmd));
        if self.by_name.insert(name.clone(), idx).is_some() {
            panic!("duplicate command name: /{name}");
        }
    }

    /// Look up a command by name (without leading `/`).
    pub fn get(&self, name: &str) -> Option<&dyn Command> {
        self.by_name
            .get(name)
            .map(|&idx| self.commands[idx].as_ref())
    }

    /// All commands in registration order.
    pub fn all(&self) -> Vec<&dyn Command> {
        self.commands.iter().map(|c| c.as_ref()).collect()
    }

    /// Parse input, look up command, execute it.
    pub async fn execute(
        &self,
        input: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError> {
        let input = input.trim();
        let without_slash = input
            .strip_prefix('/')
            .ok_or_else(|| CommandError::UserError("not a command".into()))?;

        let (name, args) = match without_slash.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (without_slash, ""),
        };

        let cmd = self.get(name).ok_or_else(|| {
            CommandError::UserError(format!(
                "unknown command: /{name}.\nType /help for available commands."
            ))
        })?;

        cmd.execute(args, ctx).await
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Registry builder ----

/// Build a CommandRegistry with all built-in commands registered.
pub fn build_registry() -> CommandRegistry {
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

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;

    // Stub command for testing registry
    struct StubCmd {
        name: String,
        desc: String,
    }

    #[async_trait]
    impl Command for StubCmd {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.desc
        }
        async fn execute(
            &self,
            _args: &str,
            _ctx: &mut CommandContext<'_>,
        ) -> Result<CommandResult, CommandError> {
            Ok(CommandResult::Continue)
        }
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut reg = CommandRegistry::new();
        reg.register(StubCmd {
            name: "foo".into(),
            desc: "Foo command".into(),
        });
        assert!(reg.get("foo").is_some());
        assert!(reg.get("bar").is_none());
    }

    #[test]
    fn test_registry_all_order() {
        let mut reg = CommandRegistry::new();
        reg.register(StubCmd {
            name: "a".into(),
            desc: "A".into(),
        });
        reg.register(StubCmd {
            name: "b".into(),
            desc: "B".into(),
        });
        reg.register(StubCmd {
            name: "c".into(),
            desc: "C".into(),
        });
        let all = reg.all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name(), "a");
        assert_eq!(all[1].name(), "b");
        assert_eq!(all[2].name(), "c");
    }

    #[test]
    #[should_panic(expected = "duplicate command name")]
    fn test_registry_duplicate_panics() {
        let mut reg = CommandRegistry::new();
        reg.register(StubCmd {
            name: "dup".into(),
            desc: "first".into(),
        });
        reg.register(StubCmd {
            name: "dup".into(),
            desc: "second".into(),
        });
    }

    #[tokio::test]
    async fn test_execute_unknown_command() {
        let reg = CommandRegistry::new();
        let mut agent = agent_runtime::AgentBuilder::new()
            .session(agent_session::MemorySession::new())
            .events(agent_event::CollectingSink::new())
            .build()
            .unwrap();
        let mut config = Config::from_env().unwrap();
        let mut state_store = crate::state::StateStore::new();
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
        };
        let result = reg.execute("/nonexistent", &mut ctx).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            CommandError::UserError(msg) => assert!(msg.contains("unknown command")),
            _ => panic!("expected UserError"),
        }
    }
}
