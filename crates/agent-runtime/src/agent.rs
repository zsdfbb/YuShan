use std::path::PathBuf;

use agent_component::{RunLimits, RuntimeContext};
use agent_core::{CancelToken, Message};
use agent_event::EventSink;
use agent_loop::{AgentInput, AgentLoop, LoopError, RunResult};
use agent_model::Model;
use agent_session::Session;
use agent_tool::{ApprovalHandler, ToolRegistry};

pub struct Agent {
    loop_impl: Box<dyn AgentLoop>,
    model: Option<Box<dyn Model>>,
    registry: ToolRegistry,
    session: Box<dyn Session>,
    events: Box<dyn EventSink>,
    cancel: CancelToken,
    limits: RunLimits,
    cwd: PathBuf,
    workspace_root: PathBuf,
    approval: Option<Box<dyn ApprovalHandler>>,
    system_prompt: Option<String>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        loop_impl: Box<dyn AgentLoop>,
        model: Option<Box<dyn Model>>,
        registry: ToolRegistry,
        session: Box<dyn Session>,
        events: Box<dyn EventSink>,
        cancel: CancelToken,
        limits: RunLimits,
        cwd: PathBuf,
        workspace_root: PathBuf,
        approval: Option<Box<dyn ApprovalHandler>>,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            loop_impl,
            model,
            registry,
            session,
            events,
            cancel,
            limits,
            cwd,
            workspace_root,
            approval,
            system_prompt,
        }
    }

    /// Whether a model has been configured for this agent.
    pub fn is_configured(&self) -> bool {
        self.model.is_some()
    }

    /// Run a single turn. Takes &mut self to ensure single concurrent run.
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        let model = self.model.as_deref().ok_or_else(|| {
            LoopError::ConfigError("No model configured. Use /login to configure an API provider.".into())
        })?;
        let mut ctx = RuntimeContext::new(
            model,
            &self.registry,
            self.session.as_mut(),
            self.events.as_mut(),
            &self.cancel,
            self.limits.clone(),
            self.cwd.clone(),
            self.workspace_root.clone(),
            self.approval.as_deref(),
            self.system_prompt.clone(),
        );
        self.loop_impl.run_turn(input, &mut ctx).await
    }

    /// Get the model identifier, if configured.
    pub fn model_id(&self) -> Option<&str> {
        self.model.as_deref().map(|m| m.model_id())
    }

    /// Return tool names (owned String list). Used by banner/footer to display available tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.registry.names().iter().map(|s| s.to_string()).collect()
    }

    /// Return the model context window size in tokens.
    pub fn context_window(&self) -> usize {
        self.limits.context_window
    }

    /// Cancel the current run. The next iteration of the agent loop will stop.
    pub fn cancel(&mut self) {
        self.cancel.cancel();
    }

    /// Clone-able handle to the internal cancel token.
    ///
    /// 返回的 `CancelToken` 与 `self.cancel` 共享底层 `Arc<AtomicBool>`，
    /// 调用方可在 `tokio::select!` 内持 `cancel_token.cancel()` 而无需借用 `&mut Agent`。
    ///
    /// Why `&self` (not `&mut self`): `run_turn(input).await` 借用 `&mut Agent` 整生命周期；
    /// select! 内调 `agent.cancel(&mut self)` 与 `&mut turn_fut` borrow 冲突（E0499）。
    /// 返回 Clone handle 让调用方跨借用边界触发取消。
    ///
    /// 与 `cancel(&mut self)` 共存；后者保留向后兼容（未来若 BasicLoop 加 abort 回调需要 &mut self，
    /// 仍可走 `&mut self` 路径）。
    pub fn cancel_handle(&self) -> agent_core::CancelToken {
        self.cancel.clone()
    }

    /// Replace the model. Pass None to remove (e.g., /logout).
    pub fn set_model(&mut self, model: Option<Box<dyn Model>>) {
        self.model = model;
    }

    /// Clear all messages from the session.
    pub async fn clear_session(&mut self) -> Result<(), agent_session::SessionError> {
        self.session.clear().await
    }

    /// Read all session messages.
    pub fn session_messages(&self) -> &[Message] {
        self.session.messages()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentBuilder;
    use agent_event::CollectingSink;
    use agent_model::MockModel;
    use agent_session::MemorySession;
    use agent_tool::{Tool, ToolContext, ToolResult, ToolSpec};

    struct MockTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.0, "desc", serde_json::json!({}))
        }

        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolResult, agent_tool::ToolError> {
            unimplemented!()
        }
    }

    #[test]
    fn test_tool_names_empty_by_default() {
        let agent = AgentBuilder::new()
            .model(MockModel::new("test"))
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();
        let names = agent.tool_names();
        // Default builder registers no tools.
        assert_eq!(names.len(), 0);
    }

    #[test]
    fn test_tool_names_returns_registered_tools() {
        let agent = AgentBuilder::new()
            .model(MockModel::new("test"))
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .tool(MockTool("read"))
            .tool(MockTool("write"))
            .build()
            .unwrap();
        let names = agent.tool_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"write".to_string()));
    }

    #[test]
    fn test_context_window_default() {
        let agent = AgentBuilder::new()
            .model(MockModel::new("test"))
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();
        // RunLimits::default().context_window = 128_000
        assert_eq!(agent.context_window(), 128_000);
    }

    #[test]
    fn test_cancel_signals_token() {
        let cancel = CancelToken::new();
        let token_clone = cancel.clone();
        let agent = AgentBuilder::new()
            .model(MockModel::new("test"))
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .cancel_token(cancel)
            .build()
            .unwrap();
        assert!(!token_clone.is_cancelled());
        let mut agent = agent;
        agent.cancel();
        assert!(token_clone.is_cancelled());
    }

    #[test]
    fn test_cancel_handle_is_clone_and_signals() {
        let agent = AgentBuilder::new()
            .model(MockModel::new("test"))
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        let handle = agent.cancel_handle();
        let handle2 = handle.clone();
        assert!(!handle.is_cancelled());

        // 通过 handle 触发取消 —— 无需 &mut agent
        handle.cancel();
        assert!(handle2.is_cancelled()); // 共享 Arc — 立即可见
    }

    #[tokio::test]
    async fn test_cancel_handle_triggers_cancellation() {
        use agent_core::StopReason;

        let agent = AgentBuilder::new()
            .model(MockModel::new("test"))
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        let mut agent = agent;
        let handle = agent.cancel_handle();

        // 通过 handle 触发取消 —— 无需 &mut agent
        // (BasicLoop 在入口边界检查 cancel — 命中后立即返回 Cancelled)
        handle.cancel();

        let result = agent.run_turn(agent_loop::AgentInput::text("go")).await.unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }
}
