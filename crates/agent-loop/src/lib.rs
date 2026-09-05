//! Agent execution loop and orchestration logic.

mod basic;
mod error;
mod input;
mod result;
pub mod token;

pub use basic::*;
pub use error::*;
pub use input::*;
pub use result::*;

use agent_component::RuntimeContext;

/// Agent loop trait - replaceable execution strategy
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
    use agent_component::{RunLimits, RuntimeContext};
    use agent_core::*;
    use agent_event::{AgentEvent, CollectingSink};
    use agent_model::MockModel;
    use agent_session::MemorySession;
    use agent_tool::{Tool, ToolContext, ToolRegistry};
    use std::path::PathBuf;

    /// Helper to create a RuntimeContext with default cwd/workspace
    fn make_ctx<'a>(
        model: &'a dyn agent_model::Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn agent_session::Session,
        events: &'a mut dyn agent_event::EventSink,
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

    // Helper: echo tool
    struct EchoTool;

    // Helper: failing tool
    struct FailingTool;

    #[async_trait::async_trait]
    impl Tool for FailingTool {
        fn spec(&self) -> agent_tool::ToolSpec {
            agent_tool::ToolSpec::new("fail", "always fails", serde_json::json!({}))
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<agent_tool::ToolResult, agent_tool::ToolError> {
            Err(agent_tool::ToolError::Execution(
                "deliberate failure".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> agent_tool::ToolSpec {
            agent_tool::ToolSpec::new("echo", "echoes input", serde_json::json!({}))
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<agent_tool::ToolResult, agent_tool::ToolError> {
            Ok(agent_tool::ToolResult {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    // TC2: Pure text reply
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

        // Check events
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

    // TC3: Tool call loop
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

        // Check events contain ToolCall, ToolResult, ModelTextDelta
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

    // TC6: Cancellation
    #[tokio::test]
    async fn test_cancellation() {
        let model = MockModel::new("m");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        cancel.cancel(); // Cancel before run
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

    // TC7: Max rounds
    #[tokio::test]
    async fn test_max_rounds() {
        let model = MockModel::new("m");
        // Always return tool calls to exhaust rounds
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_tool_call("echo", serde_json::json!({}));
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(2); // max 2 rounds

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

    // TC5/TC13: ToolError path - errors are fed back to model, not terminating
    #[tokio::test]
    async fn test_tool_error_path() {
        let model = MockModel::new("m");
        model.push_tool_call("fail", serde_json::json!({}));
        // After tool error is fed back, model responds with text
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

        // Should succeed - tool errors are fed back as results, not terminating
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.stop_reason, StopReason::Completed);

        // Check events: UserMessage, ToolCall, ToolResult, ModelTextDelta, RunFinished
        let evts = events.events();
        let has_tool_call = evts
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }));
        let has_tool_result = evts
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { .. }));
        assert!(has_tool_call, "should emit ToolCall");
        assert!(has_tool_result, "should emit ToolResult");

        // Exactly one terminal event
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

    // Missing test: Cancel during loop iteration (mid-loop)
    #[tokio::test]
    async fn test_cancel_during_loop() {
        let model = MockModel::new("m");
        model.push_tool_call("echo", serde_json::json!({}));
        // After first round's tool call, cancel is set before next model call
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let cancel = CancelToken::new();
        let cancel_clone = cancel.clone();
        let limits = RunLimits::new(5);

        // We need a tool that cancels mid-execution
        struct CancelingTool {
            cancel: CancelToken,
        }
        #[async_trait::async_trait]
        impl agent_tool::Tool for CancelingTool {
            fn spec(&self) -> agent_tool::ToolSpec {
                agent_tool::ToolSpec::new("echo", "cancels then echoes", serde_json::json!({}))
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: agent_tool::ToolContext<'_>,
            ) -> Result<agent_core::ToolResult, agent_tool::ToolError> {
                self.cancel.cancel();
                Ok(agent_core::ToolResult {
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

        // Should complete with Cancelled at next round boundary
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }

    // TC9: Terminal event invariant - exactly one terminal event
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
}
