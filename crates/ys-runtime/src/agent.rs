use std::path::PathBuf;
use std::sync::Arc;

use ys_component::{RunLimits, RuntimeContext};
use ys_event::EventSink;
use ys_loop::{AgentInput, AgentLoop, LoopError, RunResult};
use ys_model::Model;
use ys_protocol::BoundarySource;
use ys_session::Session;
use ys_tool::{ApprovalHandler, SubagentRunner, ToolRegistry};

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
    ///
    /// **共享所有权**（`Arc`）而非借用：模型适配器侧需要在 `await` 期间
    /// 持有**同一份** abort 状态，而它必须 `'static`。
    pub boundary: Option<Arc<dyn BoundarySource>>,
    /// 一次性派发能力（可选）。`None` = 本回合不支持 `subagent` 派发工具。
    ///
    /// 由接线器每回合构造并注入（照 `boundary` 的先例）—— 「造子执行环境」的能力
    /// 归 app 层，`ys-*` 只经此窄接口穿透。
    pub subagent: Option<&'a dyn SubagentRunner>,
}

impl<'a> AgentPorts<'a> {
    /// 便捷构造：顺序 `model, session, events, boundary`。
    pub fn new(
        model: Option<&'a dyn Model>,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
        boundary: Option<Arc<dyn BoundarySource>>,
    ) -> Self {
        Self {
            model,
            session,
            events,
            boundary,
            subagent: None,
        }
    }

    /// 链式挂载派发能力（`AgentPorts::new` 签名不变，保持既有调用点不动）。
    pub fn with_subagent(mut self, subagent: &'a dyn SubagentRunner) -> Self {
        self.subagent = Some(subagent);
        self
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
        ctx.subagent = ports.subagent;
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

    /// 返回**每轮固定开销**的 token 估算（system prompt + 工具表 spec）。
    ///
    /// 这部分**每次请求都要重发**，却既不在会话消息里、也不参与压缩切分
    /// ⇒ 压缩后的「真实占用」= 会话消息估算 **+ 本值**。
    ///
    /// **为什么需要暴露**：`/compact`（app 侧第二个调用方）要按**同一口径**
    /// 报「压完还剩多少」⇒ 需要与自动压缩**同一个**数。
    ///
    /// ADR-0007 保留：只读查询，不依赖 `&mut Agent`。
    pub fn fixed_prompt_tokens(&self) -> usize {
        self.limits.fixed_prompt_tokens
    }

    /// 返回**活对话的 system prompt**（`None` = 未设置）。
    ///
    /// **为什么需要暴露**：压缩的摘要调用要**复用活对话前缀**才能命中 KV cache
    /// （B4 / ADR-0017 决策 10）⇒ 摘要请求的 `system` 必须与主调用**逐字相同**。
    ///
    /// ADR-0007 保留：只读查询，不依赖 `&mut Agent`。
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// 设置 model 的 context window 大小（运行期刷新）。
    ///
    /// **为什么需要**：`/login`、`/model` 会在运行中换 provider/model，窗口差可达 16 倍。
    /// 只在 `build_agent` 解析的话，用户 `/login` 到 1M 模型后仍按旧窗口压缩 ——
    /// 这正是「1M 模型在 11 万 token 就被压缩」这个 bug 的另一半。
    ///
    /// ADR-0007：本 API 由 ADR-0017 覆盖。
    pub fn set_context_window(&mut self, window: usize) {
        self.limits.context_window = window;
    }

    /// 收紧「收场前等待子 agent」的上限（ADR-0055）。
    ///
    /// 与 [`set_context_window`](Self::set_context_window) 同一条纪律：由应用层在
    /// **回合开头**按当前配置派生一次，库不自行读配置。调用方须传入
    /// `min(配置值, 剩余预算)` —— 否则父 budget 到点会推 `Abort` 并报
    /// 「budget exhausted」，而回合其实只是在正常等待。
    pub fn set_subagent_wait(&mut self, wait: std::time::Duration) {
        // 0 会让等待静默失效（等价于「立刻收场」）⇒ 钳到最小 1ms，
        // 让「配错了」表现为「等极短」而不是「功能不存在」。
        self.limits.subagent_wait = wait.max(std::time::Duration::from_millis(1));
    }

    /// 当前生效的等待上限 —— 供调用方与自己的配置取 `min`（见 `app_loop` 的每回合刷新）。
    pub fn subagent_wait(&self) -> std::time::Duration {
        self.limits.subagent_wait
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
        // RunLimits::default() 的 context_window = 1_000_000
        assert_eq!(agent.context_window(), 1_000_000);
    }

    #[test]
    fn test_set_context_window_overrides_default() {
        let mut agent = test_agent();
        agent.set_context_window(1_000_000);
        assert_eq!(agent.context_window(), 1_000_000);
        // 再改一次也应生效（运行期可反复刷新）。
        agent.set_context_window(32_000);
        assert_eq!(agent.context_window(), 32_000);
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
                AgentPorts::new(
                    Some(&model),
                    &mut session,
                    &mut events,
                    Some(Arc::new(boundary)),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }
}
