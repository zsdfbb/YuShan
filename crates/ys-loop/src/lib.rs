//! Agent 执行循环与编排逻辑。

mod basic;
mod error;
mod input;
mod result;
pub mod token;

pub use basic::*;
pub use error::*;
pub use input::*;
pub use result::*;

use ys_component::RuntimeContext;

/// Agent loop trait —— 可替换的执行策略
#[async_trait::async_trait]
pub trait AgentLoop: Send + Sync {
    async fn run_turn(
        &self,
        input: AgentInput,
        ctx: &mut RuntimeContext<'_>,
    ) -> Result<RunResult, LoopError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use ys_component::{RunLimits, RuntimeContext};
    use ys_core::*;
    use ys_event::{AgentEvent, CollectingSink, FailingSink};
    use ys_model::MockModel;
    use ys_session::MemorySession;
    use ys_tool::{Tool, ToolContext, ToolRegistry};

    /// 用默认 cwd/workspace 创建 RuntimeContext 的辅助函数
    fn make_ctx<'a>(
        model: &'a dyn ys_model::Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn ys_session::Session,
        events: &'a mut dyn ys_event::EventSink,
        cancel: &'a CancelToken,
        limits: RunLimits,
    ) -> RuntimeContext<'a> {
        RuntimeContext::new(
            model,
            registry,
            session,
            events,
            cancel,
            limits,
            PathBuf::from("."),
            PathBuf::from("."),
            None,
            None,
        )
    }

    // 辅助工具：echo 工具
    struct EchoTool;

    // 辅助工具：failing 工具
    struct FailingTool;

    #[async_trait::async_trait]
    impl Tool for FailingTool {
        fn spec(&self) -> ys_tool::ToolSpec {
            ys_tool::ToolSpec::new("fail", "always fails", serde_json::json!({}))
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ys_tool::ToolResult, ys_tool::ToolError> {
            Err(ys_tool::ToolError::Execution("deliberate failure".into()))
        }
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ys_tool::ToolSpec {
            ys_tool::ToolSpec::new("echo", "echoes input", serde_json::json!({}))
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ys_tool::ToolResult, ys_tool::ToolError> {
            Ok(ys_tool::ToolResult {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    // TC2：纯文本回复
    #[tokio::test]
    async fn test_basic_text_reply() {
        let model = MockModel::new("m");
        model.push_text("hello world");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                &cancel,
                limits,
            );
            let input = AgentInput::text("hi");
            let loop_impl = BasicLoop;
            loop_impl.run_turn(input, &mut ctx).await.unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert!(result.final_message.is_some());
        assert_eq!(result.rounds, 1);

        // 检查事件（纯文本路径）
        let evts = events.events();
        assert!(matches!(&evts[0], AgentEvent::UserMessage { .. }));
        assert!(matches!(&evts[1], AgentEvent::ModelTextDelta { text } if text == "hello world"));
        assert!(matches!(
            &evts[2],
            AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                ..
            }
        ));
    }

    // TC3：工具调用循环
    #[tokio::test]
    async fn test_tool_call_loop() {
        let model = MockModel::new("m");
        model.push_tool_call("echo", serde_json::json!({"msg": "test"}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                &cancel,
                limits,
            );
            let input = AgentInput::text("go");
            let loop_impl = BasicLoop;
            loop_impl.run_turn(input, &mut ctx).await.unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert_eq!(result.rounds, 2);

        // 检查事件包含 ToolCall、ToolResult、ModelTextDelta
        let evts = events.events();
        let has_tool_call = evts
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }));
        let has_tool_result = evts
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { .. }));
        assert!(has_tool_call);
        assert!(has_tool_result);
    }

    // TC6：取消
    #[tokio::test]
    async fn test_cancellation() {
        let model = MockModel::new("m");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        cancel.cancel(); // 运行前取消
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx = make_ctx(
            &model,
            &registry,
            &mut session,
            &mut events,
            &cancel,
            limits,
        );
        let input = AgentInput::text("go");
        let loop_impl = BasicLoop;
        let result = loop_impl.run_turn(input, &mut ctx).await.unwrap();

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(result.rounds, 0);
    }

    // TC7：最大 round 数
    #[tokio::test]
    async fn test_max_rounds() {
        let model = MockModel::new("m");
        // 一直返回 tool call 以耗尽 round
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_tool_call("echo", serde_json::json!({}));
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(2); // 最多 2 个 round

        let mut ctx = make_ctx(
            &model,
            &registry,
            &mut session,
            &mut events,
            &cancel,
            limits,
        );
        let input = AgentInput::text("go");
        let loop_impl = BasicLoop;
        let result = loop_impl.run_turn(input, &mut ctx).await.unwrap();

        assert_eq!(result.stop_reason, StopReason::MaxRounds);
        assert_eq!(result.rounds, 2);
    }

    // TC5/TC13：ToolError 路径 —— 错误回喂给模型，而非终止
    #[tokio::test]
    async fn test_tool_error_path() {
        let model = MockModel::new("m");
        model.push_tool_call("fail", serde_json::json!({}));
        // tool 错误回喂后，模型以文本回复
        model.push_text("tool failed, but I'll continue");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![Box::new(FailingTool)]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                &cancel,
                limits,
            );
            let input = AgentInput::text("go");
            let loop_impl = BasicLoop;
            loop_impl.run_turn(input, &mut ctx).await
        };

        // 应成功 —— tool 错误作为结果回喂，不终止
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.stop_reason, StopReason::Completed);

        // 检查事件：UserMessage、ToolCall、ToolResult、ModelTextDelta、RunFinished
        let evts = events.events();
        let has_tool_call = evts
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }));
        let has_tool_result = evts
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { .. }));
        assert!(has_tool_call, "should emit ToolCall");
        assert!(has_tool_result, "should emit ToolResult");

        // 恰好一个终止事件
        let terminal_count = evts
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
                )
            })
            .count();
        assert_eq!(terminal_count, 1, "exactly one terminal event");
    }

    // 缺失测试：循环迭代中途取消（mid-loop）
    #[tokio::test]
    async fn test_cancel_during_loop() {
        let model = MockModel::new("m");
        model.push_tool_call("echo", serde_json::json!({}));
        // 第一轮 tool call 之后、下一次 model 调用之前设置 cancel
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let cancel_clone = cancel.clone();
        let limits = RunLimits::new(5);

        // 需要一个在执行中途取消的工具
        struct CancelingTool {
            cancel: CancelToken,
        }
        #[async_trait::async_trait]
        impl ys_tool::Tool for CancelingTool {
            fn spec(&self) -> ys_tool::ToolSpec {
                ys_tool::ToolSpec::new("echo", "cancels then echoes", serde_json::json!({}))
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: ys_tool::ToolContext<'_>,
            ) -> Result<ys_core::ToolResult, ys_tool::ToolError> {
                self.cancel.cancel();
                Ok(ys_core::ToolResult {
                    content: "ok".into(),
                    is_error: false,
                })
            }
        }

        let registry = ToolRegistry::build(vec![Box::new(CancelingTool {
            cancel: cancel_clone,
        })])
        .unwrap();
        let mut ctx = make_ctx(
            &model,
            &registry,
            &mut session,
            &mut events,
            &cancel,
            limits,
        );
        let input = AgentInput::text("go");
        let loop_impl = BasicLoop;
        let result = loop_impl.run_turn(input, &mut ctx).await.unwrap();

        // 应在下一个 round 边界以 Cancelled 完成
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }

    // TC9：终止事件不变量 —— 恰好一个终止事件
    #[tokio::test]
    async fn test_terminal_event_invariant() {
        let model = MockModel::new("m");
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let _ = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                &cancel,
                limits,
            );
            let input = AgentInput::text("go");
            let loop_impl = BasicLoop;
            loop_impl.run_turn(input, &mut ctx).await
        };

        let terminal_count = events
            .events()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
                )
            })
            .count();
        assert_eq!(terminal_count, 1, "exactly one terminal event required");
    }

    // Task 6：EventError::SendFailed → LoopError::Event 的首个覆盖
    #[tokio::test]
    async fn test_send_failed_propagates_as_loop_error() {
        let model = MockModel::new("m");
        model.push_text("hello");
        let mut session = MemorySession::new();
        // 首次投递即失败：自由函数总走慢路径，慢路径也失败
        let mut events = FailingSink::new(0);
        events.set_fail_slow(true);
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx = make_ctx(
            &model,
            &registry,
            &mut session,
            &mut events,
            &cancel,
            limits,
        );
        let input = AgentInput::text("hi");
        let result = BasicLoop.run_turn(input, &mut ctx).await;

        // 终局不变式：emit 失败即终止当前 turn，不静默续跑
        let err = result.err().expect("emit 失败应终止 turn");
        assert!(
            matches!(err, LoopError::Event(ys_core::EventError::SendFailed)),
            "expected LoopError::Event(SendFailed), got {err:?}"
        );
        assert_eq!(events.try_calls(), 0, "自由函数总走慢路径，不触碰快路径");
        assert_eq!(events.slow_calls(), 1, "慢路径失败一次即终止");
        assert!(events.emitted().is_empty(), "不应有事件被静默投递");
    }
}
