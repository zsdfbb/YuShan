use std::path::PathBuf;

use ys_component::{RunLimits, RuntimeContext};
use ys_core::{CancelToken, Message};
use ys_event::EventSink;
use ys_loop::{AgentInput, AgentLoop, LoopError, RunResult};
use ys_model::Model;
use ys_session::Session;
use ys_tool::{ApprovalHandler, ToolRegistry};

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

    /// 该 agent 是否已配置 model。
    pub fn is_configured(&self) -> bool {
        self.model.is_some()
    }

    /// 运行单个 turn。取 &mut self 以保证同时只运行一次。
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        let model = self.model.as_deref().ok_or_else(|| {
            LoopError::ConfigError(
                "No model configured. Use /login to configure an API provider.".into(),
            )
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

    /// 获取 model 标识符（若已配置）。
    pub fn model_id(&self) -> Option<&str> {
        self.model.as_deref().map(|m| m.model_id())
    }

    /// 返回工具名称（自有 String 列表）。供 banner/footer 展示可用工具。
    pub fn tool_names(&self) -> Vec<String> {
        self.registry
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// 返回以 token 计的 model context window 大小。
    pub fn context_window(&self) -> usize {
        self.limits.context_window
    }

    /// 取消当前运行。agent loop 的下一次迭代将停止。
    pub fn cancel(&mut self) {
        self.cancel.cancel();
    }

    /// 内部 cancel token 的可克隆句柄。
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
    pub fn cancel_handle(&self) -> ys_core::CancelToken {
        self.cancel.clone()
    }

    /// 替换 model。传 None 移除（例如 /logout）。
    pub fn set_model(&mut self, model: Option<Box<dyn Model>>) {
        self.model = model;
    }

    /// 清空 session 中的所有消息。
    pub async fn clear_session(&mut self) -> Result<(), ys_session::SessionError> {
        self.session.clear().await
    }

    /// 读取所有 session 消息。
    pub fn session_messages(&self) -> &[Message] {
        self.session.messages()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentBuilder;
    use ys_event::CollectingSink;
    use ys_model::MockModel;
    use ys_session::MemorySession;
    use ys_tool::{Tool, ToolContext, ToolResult, ToolSpec};

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
        ) -> Result<ToolResult, ys_tool::ToolError> {
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
        // 默认 builder 不注册任何工具。
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
        // RunLimits::default() 的 context_window = 128_000
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
        use ys_core::StopReason;

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

        let result = agent
            .run_turn(ys_loop::AgentInput::text("go"))
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }
}
