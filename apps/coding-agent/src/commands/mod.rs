//! coding-agent TUI 的 slash command 系统。

pub mod builtin;

pub use builtin::*;

use std::collections::HashMap;

use async_trait::async_trait;

use crate::config::Config;
use crate::state::StateStore;
use agent_runtime::Agent;

/// 可注册进 CommandRegistry 的 slash command。
#[async_trait]
#[allow(dead_code)] // description / arg_hint 属于公共 Command
// contract，但 v0 渲染路径不消费（completer
// 用自有的 CmdEntry；/help 读 builtin_help_entries）。
pub trait Command: Send + Sync {
    /// 不带前导 `/` 的 command 名（如 "help"、"model"）。
    fn name(&self) -> &str;

    /// 显示在 /help 输出中的一行描述。
    fn description(&self) -> &str;

    /// /help 中显示的可选参数提示（如 "<model_name>"）。
    fn arg_hint(&self) -> Option<&str> {
        None
    }

    /// 执行 command。
    async fn execute(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandResult, CommandError>;
}

/// command 执行期间可接触的可变环境（mutable world）。
pub struct CommandContext<'a> {
    pub agent: &'a mut Agent,
    pub config: &'a mut Config,
    pub state: &'a mut StateStore,
    pub prompter: &'a dyn Prompter,
}

/// 命令交互输入面——从 inquire 抽象而来，支持测试 fake。
pub trait Prompter: Send + Sync {
    /// 展示选项列表，返回用户选中的 label。
    fn select(
        &self,
        prompt: &str,
        options: Vec<String>,
        page_size: usize,
    ) -> Result<String, PromptError>;

    /// 文本输入（含可选 help message）。
    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError>;
}

/// 交互取消/失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// 用户取消或中断（Esc / Ctrl-C）。
    /// 当前 inquire 的 OperationCanceled 和 OperationInterrupted 始终合并处理，
    /// 故统一为一个变体；若未来需区分，可拆为 Canceled / Interrupted。
    Cancelled,
    /// 其他错误。
    Other(String),
}

/// command 结束后 TUI 循环应采取的后续动作。
#[derive(Debug)]
pub enum CommandResult {
    /// 正常继续 REPL 循环。
    Continue,
    /// 退出 REPL。
    Exit,
}

/// command 执行的错误。
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// 用户可恢复的错误（如 "No API key configured"）。
    #[error("{0}")]
    UserError(String),
    /// 内部错误（如 IO 失败）。
    #[error("internal error: {0}")]
    Internal(String),
}

/// 可用 slash command 的 registry。
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

    /// 注册 command。重复名称时 panic（启动期捕获）。
    pub fn register(&mut self, cmd: impl Command + 'static) {
        let name = cmd.name().to_string();
        let idx = self.commands.len();
        self.commands.push(Box::new(cmd));
        if self.by_name.insert(name.clone(), idx).is_some() {
            panic!("duplicate command name: /{name}");
        }
    }

    /// 按名称查找 command（不带前导 `/`）。
    pub fn get(&self, name: &str) -> Option<&dyn Command> {
        self.by_name
            .get(name)
            .map(|&idx| self.commands[idx].as_ref())
    }

    /// 按注册顺序返回全部 command。
    pub fn all(&self) -> Vec<&dyn Command> {
        self.commands.iter().map(|c| c.as_ref()).collect()
    }

    /// 解析输入、查找 command、执行之。
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

// ---- Registry 构建器 ----

/// 构建注册了全部内置 command 的 CommandRegistry。
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

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;

    // 测试 registry 用的 stub command
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

    // 测试用的最小 Prompter 实现（不交互，永远取消）。
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
        let prompter = StubPrompter;
        let mut ctx = CommandContext {
            agent: &mut agent,
            config: &mut config,
            state: &mut state_store,
            prompter: &prompter,
        };
        let result = reg.execute("/nonexistent", &mut ctx).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            CommandError::UserError(msg) => assert!(msg.contains("unknown command")),
            _ => panic!("expected UserError"),
        }
    }
}
