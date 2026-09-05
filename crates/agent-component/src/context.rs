use std::path::PathBuf;

use super::RunLimits;
use agent_core::CancelToken;
use agent_event::EventSink;
use agent_model::Model;
use agent_session::Session;
use agent_tool::{ApprovalHandler, ToolRegistry};

#[non_exhaustive]
pub struct RuntimeContext<'a> {
    pub model: &'a dyn Model,
    pub registry: &'a ToolRegistry,
    pub session: &'a mut dyn Session,
    pub events: &'a mut dyn EventSink,
    pub cancel: &'a CancelToken,
    pub limits: RunLimits,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub approval: Option<&'a dyn ApprovalHandler>,
    pub system_prompt: Option<String>,
}

impl<'a> RuntimeContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &'a dyn Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
        cancel: &'a CancelToken,
        limits: RunLimits,
        cwd: PathBuf,
        workspace_root: PathBuf,
        approval: Option<&'a dyn ApprovalHandler>,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
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
}
