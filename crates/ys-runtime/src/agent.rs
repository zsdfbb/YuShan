use std::path::PathBuf;

use ys_channel::Inbox;
use ys_component::{RunLimits, RuntimeContext};
use ys_core::{CancelToken, Message, StopReason, Usage};
use ys_event::EventSink;
use ys_loop::{AgentInput, AgentLoop, LoopError, RunResult};
use ys_model::Model;
use ys_session::Session;
use ys_tool::{ApprovalHandler, ToolRegistry};

/// 自转驱动的返回值。
///
/// 一次 `run()` 可能跑多个回合（followUp 排队时），故用汇总而非单个 `RunResult`。
#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    /// 本次自转实际执行的回合数。
    pub turns: u32,
    /// 所有回合的 usage 累计。
    pub usage: Usage,
    /// 最后一个回合的停止原因；一个回合都没跑时为 `None`。
    pub last_stop: Option<StopReason>,
    /// 最后一个回合的轮数；一个回合都没跑时为 0。
    ///
    /// 供 TUI 的 Summary 行复用（其 `rounds` 语义）；`-p`/`--json` 不读。
    pub last_rounds: u32,
    /// 最后一个回合的终态消息；一个回合都没跑时为 `None`。
    ///
    /// 供 TUI 在「模型未产文本增量」时兜底追加 assistant 文本（块 C）：
    /// 增量渲染优先，仅当本回合一个 `ModelTextDelta` 都没收到才回退到它。
    pub last_message: Option<Message>,
}

/// 一次 `run` / `run_turn` 所需的**外部端口**。
///
/// **ADR-0010**：会话与配置（当前模型）的所有权归**接线器**，Agent 不持有它们。
/// 故这些能力经此结构**借用**传入——Agent 只执行、只产出事件。
///
/// 选择「端口结构」(a) 而非「多参数」(b)：与会话的端口概念一致，且后续
/// 再加端口（如工具审批流、记忆）时**不破签名**。
pub struct AgentPorts<'a> {
    /// 当前模型。`None` = 未配置（`run`/`run_turn` 返回
    /// [`LoopError::ConfigError`]）。
    pub model: Option<&'a dyn Model>,
    /// 会话（消息历史）。归接线器所有；`/new` 换的就是它。
    pub session: &'a mut dyn Session,
    /// 事件出口。归接线器所有。
    pub events: &'a mut dyn EventSink,
}

impl<'a> AgentPorts<'a> {
    /// 便捷构造：顺序 `model, session, events`。
    pub fn new(
        model: Option<&'a dyn Model>,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
    ) -> Self {
        Self {
            model,
            session,
            events,
        }
    }
}

/// **无状态**执行器。
///
/// 持有的是*执行所需*的东西（工具、取消句柄、限制、系统提示、工作目录），
/// **不持有**会话态（历史、当前模型选择、事件出口）——那些归接线器
/// （ADR-0010）。「当前是哪个会话」由调用方经 [`AgentPorts`] 每次传入。
pub struct Agent {
    loop_impl: Box<dyn AgentLoop>,
    registry: ToolRegistry,
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
        registry: ToolRegistry,
        cancel: CancelToken,
        limits: RunLimits,
        cwd: PathBuf,
        workspace_root: PathBuf,
        approval: Option<Box<dyn ApprovalHandler>>,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            loop_impl,
            registry,
            cancel,
            limits,
            cwd,
            workspace_root,
            approval,
            system_prompt,
        }
    }

    /// 运行单个 turn。取 `&mut self` 以保证同时只运行一次。
    ///
    /// **兼容入口**：既有测试与调用点都用它。内部委托
    /// [`run_one_turn`](Self::run_one_turn)，语义与历史实现逐字节一致
    /// （不挂载 inbox，无轮边界 steering）。
    pub async fn run_turn(
        &mut self,
        input: AgentInput,
        ports: AgentPorts<'_>,
    ) -> Result<RunResult, LoopError> {
        self.run_one_turn(input, None, ports).await
    }

    /// 自转：从 inbox 取消息 → 跑回合 → 投事件，直到 inbox 空闲。
    ///
    /// - **不收 policy**——生命周期策略只由 sink 持有（它才知道消费者是否消失）。
    ///   遇 `emit` 报错即终止，沿用既有语义（见下「消费者消失」）。
    /// - 每个回合开始调 [`EventSink::begin_turn`]，使 `Envelope` 的 turn 正确
    ///   （不从 `UserMessage` 推导——轮边界 steering 注入也发 `UserMessage`，
    ///   推导会误增 turn）。
    /// - followUp 在**回合**边界拉（steering 在**轮**边界，已由 `BasicLoop` 处理）。
    /// - inbox 空即返回，agent 不休眠（ADR-0011 点 6）；接线器按需重驱动。
    ///
    /// **消费者消失的语义取舍**：设计文档曾写「消费者消失 → 正常收场（非故障）」，
    /// 但既有 ADR-0004 的语义是「emit 失败即终止当前 run（`Err`）」——`ChannelSink`
    /// 在 `StopWhenConsumerGone` 下让 `try_emit` / `emit` 返回 `Err`，经自由函数
    /// `emit` 传播为 `LoopError::Event(SendFailed)`，`run` 随之以 `Err` 返回。
    /// 本轮保持既有语义（Err），不引入新的 `StopReason`；「正常收场」的措辞
    /// 需与设计文档对齐。
    ///
    /// **`/new` 契约**（ADR-0010）：接线器换上新 `Session` + 新空 `Inbox`，
    /// 下次传进来的 [`AgentPorts`] 与 inbox 即指向新会话；**agent 全程不知情**。
    pub async fn run(
        &mut self,
        ports: AgentPorts<'_>,
        inbox: &Inbox,
    ) -> Result<RunSummary, LoopError> {
        let mut summary = RunSummary::default();
        loop {
            let batch = inbox.take_followup();
            if batch.is_empty() {
                return Ok(summary); // inbox 空 → 收摊
            }
            for message in batch {
                summary.turns += 1;
                ports.events.begin_turn(summary.turns);
                let result = self
                    .run_one_turn(
                        AgentInput::new(message),
                        Some(inbox),
                        AgentPorts {
                            model: ports.model,
                            session: &mut *ports.session,
                            events: &mut *ports.events,
                        },
                    )
                    .await?;
                let RunResult {
                    stop_reason,
                    usage,
                    rounds,
                    final_message,
                } = result;
                summary.usage = summary.usage + usage;
                summary.last_stop = Some(stop_reason);
                summary.last_rounds = rounds;
                summary.last_message = final_message;
            }
        }
    }

    /// 内部原语：跑一个回合（原 `run_turn` 的实现）。
    ///
    /// `inbox` 为 `Some` 时挂到 `RuntimeContext` 上，使 `BasicLoop` 在轮边界
    /// 能拉取 steering；为 `None` 时行为与历史实现逐字节一致。
    ///
    /// 取 `Option<&Inbox>` 参数（而非让 `Agent` 持有跨 await 的引用）——避免
    /// 在 `Agent` 上存借用的队列句柄。
    async fn run_one_turn(
        &mut self,
        input: AgentInput,
        inbox: Option<&Inbox>,
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
            &self.cancel,
            self.limits.clone(),
            self.cwd.clone(),
            self.workspace_root.clone(),
            self.approval.as_deref(),
            self.system_prompt.clone(),
        );
        if let Some(inbox) = inbox {
            ctx = ctx.with_inbox(inbox);
        }
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

    /// 取消当前运行。agent loop 的下一次迭代将停止。
    ///
    /// ADR-0007 保留：被动式强制打断，仍有调用场景。
    pub fn cancel(&mut self) {
        self.cancel.cancel();
    }

    /// 内部 cancel token 的可克隆句柄。
    ///
    /// 返回的 `CancelToken` 与 `self.cancel` 共享底层 `Arc<AtomicBool>`，
    /// 调用方可在 `tokio::select!` 内持 `cancel_token.cancel()` 而无需借用 `&mut Agent`。
    ///
    /// ADR-0008 保留：agent 自转后，外部唯一能与 agent 交互的通道就剩取消与只读查询。
    pub fn cancel_handle(&self) -> ys_core::CancelToken {
        self.cancel.clone()
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

    #[test]
    fn test_cancel_signals_token() {
        let cancel = CancelToken::new();
        let token_clone = cancel.clone();
        let mut agent = AgentBuilder::new().cancel_token(cancel).build().unwrap();
        assert!(!token_clone.is_cancelled());
        agent.cancel();
        assert!(token_clone.is_cancelled());
    }

    #[test]
    fn test_cancel_handle_is_clone_and_signals() {
        let agent = test_agent();

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

        let model = MockModel::new("test");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let mut agent = test_agent();
        let handle = agent.cancel_handle();

        // 通过 handle 触发取消 —— 无需 &mut agent
        // (BasicLoop 在入口边界检查 cancel — 命中后立即返回 Cancelled)
        handle.cancel();

        let result = agent
            .run_turn(
                ys_loop::AgentInput::text("go"),
                AgentPorts::new(Some(&model), &mut session, &mut events),
            )
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }
}
