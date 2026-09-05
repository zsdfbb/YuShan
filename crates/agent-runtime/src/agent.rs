use std::path::PathBuf;

use agent_component::{RunLimits, RuntimeContext};
use agent_core::CancelToken;
use agent_event::EventSink;
use agent_loop::{AgentInput, AgentLoop, LoopError, RunResult};
use agent_model::Model;
use agent_session::Session;
use agent_tool::{ApprovalHandler, ToolRegistry};

pub struct Agent {
    loop_impl: Box<dyn AgentLoop>,
    model: Box<dyn Model>,
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
        model: Box<dyn Model>,
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

    /// Run a single turn. Takes &mut self to ensure single concurrent run.
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        let mut ctx = RuntimeContext::new(
            self.model.as_ref(),
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
}
