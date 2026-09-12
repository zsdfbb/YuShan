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
}

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
    ///
    /// **兼容入口**：既有测试与 `-p`/TUI 调用点都用它。内部委托
    /// [`run_one_turn`](Self::run_one_turn)，语义与历史实现逐字节一致
    /// （不挂载 inbox，无轮边界 steering）。
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        self.run_one_turn(input, None).await
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
    /// **`/new` 契约**（本轮不做命令层，留迁移步 5）：接线器换上新 `Session` +
    /// 新空 `Inbox`，agent 全程不知情。
    pub async fn run(&mut self, inbox: &Inbox) -> Result<RunSummary, LoopError> {
        let mut summary = RunSummary::default();
        loop {
            let batch = inbox.take_followup();
            if batch.is_empty() {
                return Ok(summary); // inbox 空 → 收摊
            }
            for message in batch {
                summary.turns += 1;
                self.events.begin_turn(summary.turns);
                let result = self
                    .run_one_turn(AgentInput::new(message), Some(inbox))
                    .await?;
                summary.usage = summary.usage + result.usage;
                summary.last_stop = Some(result.stop_reason);
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
    ) -> Result<RunResult, LoopError> {
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
        if let Some(inbox) = inbox {
            ctx = ctx.with_inbox(inbox);
        }
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
