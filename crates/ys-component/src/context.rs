use std::path::PathBuf;
use std::sync::Arc;

use super::RunLimits;
use ys_event::EventSink;
use ys_model::Model;
use ys_protocol::BoundarySource;
use ys_session::Session;
use ys_tool::{ApprovalHandler, SubagentRunner, ToolRegistry};

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
    ///
    /// **共享所有权**（`Arc`）而非借用：模型调用侧的适配器需要持有**同一份**
    /// abort 状态跨越 `await`，且该状态必须 `'static`（`Arc<dyn BoundarySource>`
    /// 默认隐含 `+ 'static`）。
    pub boundary: Option<Arc<dyn BoundarySource>>,
    /// 一次性派发能力（可选）。由接线器经 [`RuntimeContext::with_subagent`] 挂载，
    /// 再经 `BasicLoop` 落到 `ToolContext`。`None` = 本回合不支持派发。
    pub subagent: Option<&'a dyn SubagentRunner>,
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
            subagent: None,
        }
    }

    /// 链式挂载轮边界控制源。`None` 时行为不变。
    ///
    /// 收 `Arc` 而非借用：挂载方（app 层）需要**同一份** abort 状态
    /// 同时被 loop 侧与模型适配器侧看到，而后者必须跨越 `await` 持有它。
    pub fn with_boundary(mut self, boundary: Arc<dyn BoundarySource>) -> Self {
        self.boundary = Some(boundary);
        self
    }

    /// 链式挂载派发能力。`None` 时行为不变。
    pub fn with_subagent(mut self, subagent: &'a dyn SubagentRunner) -> Self {
        self.subagent = Some(subagent);
        self
    }
}
