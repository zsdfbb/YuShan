use std::path::PathBuf;

use ys_component::{RunLimits, RuntimeContext};
use ys_event::EventSink;
use ys_loop::{AgentInput, AgentLoop, LoopError, RunResult};
use ys_model::Model;
use ys_protocol::BoundarySource;
use ys_session::Session;
use ys_tool::{ApprovalHandler, ToolRegistry};

/// 一次 `run_turn` 所需的**外部端口**。
///
/// **ADR-0010**：会话与配置（当前模型）的所有权归**接线器**，Agent 不持有它们。
/// 故这些能力经此结构**借用**传入——Agent 只执行、只产出事件。
///
/// 选择「端口结构」(a) 而非「多参数」(b)：与会话的端口概念一致，且后续
/// 再加端口（如工具审批流、记忆）时**不破签名**。
pub struct AgentPorts<'a> {
    /// 当前模型。`None` = 未配置（`run_turn` 返回
    /// [`LoopError::ConfigError`]）。
    pub model: Option<&'a dyn Model>,
    /// 会话（消息历史）。归接线器所有；`/new` 换的就是它。
    pub session: &'a mut dyn Session,
    /// 事件出口。归接线器所有。
    pub events: &'a mut dyn EventSink,
    /// 轮边界控制源（steering 插话 + abort 中止）。`None` = 无边界控制。
    ///
    /// 取代了旧的 `CancelToken`：取消与插话共享同一条有序通道，由
    /// `BasicLoop` 在**轮**边界拉取。
    pub boundary: Option<&'a dyn BoundarySource>,
}

impl<'a> AgentPorts<'a> {
    /// 便捷构造：顺序 `model, session, events, boundary`。
    pub fn new(
        model: Option<&'a dyn Model>,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
        boundary: Option<&'a dyn BoundarySource>,
    ) -> Self {
        Self {
            model,
            session,
            events,
            boundary,
        }
    }
}

/// **无状态**执行器。
///
/// 持有的是*执行所需*的东西（工具、限制、系统提示、工作目录），
/// **不持有**会话态（历史、当前模型选择、事件出口、边界控制）——那些归接线器
/// （ADR-0010）。「当前是哪个会话」由调用方经 [`AgentPorts`] 每次传入。
pub struct Agent {
    loop_impl: Box<dyn AgentLoop>,
    registry: ToolRegistry,
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
        registry: ToolRegistry,
        limits: RunLimits,
        cwd: PathBuf,
        workspace_root: PathBuf,
        approval: Option<Box<dyn ApprovalHandler>>,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            loop_impl,
            registry,
            limits,
            cwd,
            workspace_root,
            approval,
            system_prompt,
        }
    }

    /// 运行单个 turn。取 `&mut self` 以保证同时只运行一次。
    ///
    /// 轮边界控制（steering 注入 / abort 中止）经 [`AgentPorts::boundary`]
    /// 借用传入 —— Agent 不持有它（与 session / events 同构）。`None` 时
    /// 行为与历史实现逐字节一致（无插话、无取消）。
    pub async fn run_turn(
        &mut self,
        input: AgentInput,
        ports: AgentPorts<'_>,
    ) -> Result<RunResult, LoopError> {
        let model = ports.model.ok_or_else(|| {
            LoopError::ConfigError(
                "No model configured. Use /login to configure an API provider.".into(),
            )
        })?;
        let mut ctx = RuntimeContext::new(
            model,
            &self.registry,
            ports.session,
            ports.events,
            self.limits.clone(),
            self.cwd.clone(),
            self.workspace_root.clone(),
            self.approval.as_deref(),
            self.system_prompt.clone(),
        );
        ctx.boundary = ports.boundary;
        self.loop_impl.run_turn(input, &mut ctx).await
    }

    /// 返回工具名称（自有 String 列表）。供 banner/footer 展示可用工具。
    ///
    /// ADR-0007 保留：只读查询，不依赖 `&mut Agent`，消费者仍需要。
    pub fn tool_names(&self) -> Vec<String> {
        self.registry
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// 返回以 token 计的 model context window 大小。
    ///
    /// ADR-0007 保留：只读查询，不依赖 `&mut Agent`。
    pub fn context_window(&self) -> usize {
        self.limits.context_window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentBuilder;
    use ys_core::StopReason;
    use ys_event::CollectingSink;
    use ys_model::MockModel;
    use ys_protocol::{Boundary, QueueBoundarySource};
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

    fn test_agent() -> Agent {
        AgentBuilder::new().build().unwrap()
    }

    #[test]
    fn test_tool_names_empty_by_default() {
        let agent = test_agent();
        // 默认 builder 不注册任何工具。
        assert_eq!(agent.tool_names().len(), 0);
    }

    #[test]
    fn test_tool_names_returns_registered_tools() {
        let agent = AgentBuilder::new()
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
        let agent = test_agent();
        // RunLimits::default() 的 context_window = 128_000
        assert_eq!(agent.context_window(), 128_000);
    }

    /// 入口即中止：预置 `Abort` → `run_turn` 返回 `Cancelled`。
    ///
    /// 取代旧的 `test_cancel_handle_triggers_cancellation`：语义不变
    /// （`BasicLoop` 在入口边界检查取消，命中后立即返回）。
    #[tokio::test]
    async fn test_abort_boundary_triggers_cancellation() {
        let model = MockModel::new("test");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let mut agent = test_agent();
        let boundary = QueueBoundarySource::new();
        boundary.push(Boundary::Abort); // 运行前已中止

        let result = agent
            .run_turn(
                ys_loop::AgentInput::text("go"),
                AgentPorts::new(Some(&model), &mut session, &mut events, Some(&boundary)),
            )
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }
}
