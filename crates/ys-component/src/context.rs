use std::path::PathBuf;

use super::RunLimits;
use ys_event::EventSink;
use ys_model::Model;
use ys_protocol::BoundarySource;
use ys_session::Session;
use ys_tool::{ApprovalHandler, ToolRegistry};

#[non_exhaustive]
pub struct RuntimeContext<'a> {
    pub model: &'a dyn Model,
    pub registry: &'a ToolRegistry,
    pub session: &'a mut dyn Session,
    pub events: &'a mut dyn EventSink,
    pub limits: RunLimits,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub approval: Option<&'a dyn ApprovalHandler>,
    pub system_prompt: Option<String>,
    /// 轮边界控制源（steering + abort 的唯一入口）。
    ///
    /// `None` = 无边界控制，行为与不挂载时逐字节一致（既无插话也无取消）。
    pub boundary: Option<&'a dyn BoundarySource>,
}

impl<'a> RuntimeContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &'a dyn Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
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
            limits,
            cwd,
            workspace_root,
            approval,
            system_prompt,
            boundary: None,
        }
    }

    /// 链式挂载轮边界控制源。`None` 时行为不变。
    pub fn with_boundary(mut self, boundary: &'a dyn BoundarySource) -> Self {
        self.boundary = Some(boundary);
        self
    }
}
