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
