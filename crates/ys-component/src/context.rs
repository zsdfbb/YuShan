use std::path::PathBuf;

use super::RunLimits;
use ys_channel::Inbox;
use ys_core::CancelToken;
use ys_event::EventSink;
use ys_model::Model;
use ys_session::Session;
use ys_tool::{ApprovalHandler, ToolRegistry};

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
    /// 轮边界可查的掌舵队列（None = 行为与今日逐字节一致）。
    pub inbox: Option<&'a Inbox>,
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
            inbox: None,
        }
    }

    /// 链式挂载掌舵队列（轮边界 steering 来源）。`None` 时行为不变。
    pub fn with_inbox(mut self, inbox: &'a Inbox) -> Self {
        self.inbox = Some(inbox);
        self
    }
}
