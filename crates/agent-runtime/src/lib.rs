//! agent-runtime: 组装 facade，锁定外部导入路径

mod agent;
mod builder;
mod error;
pub mod prelude;

pub use agent::*;
pub use builder::*;
pub use error::*;

#[cfg(test)]
mod tests {
    use super::prelude::*;
    use agent_event::CollectingSink;
    use agent_model::MockModel;

    struct EchoTool;

    #[async_trait::async_trait]
    impl agent_tool::Tool for EchoTool {
        fn spec(&self) -> agent_tool::ToolSpec {
            agent_tool::ToolSpec::new("echo", "echoes input", serde_json::json!({}))
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: agent_tool::ToolContext<'_>,
        ) -> Result<agent_tool::ToolResult, agent_tool::ToolError> {
            Ok(agent_tool::ToolResult {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    #[tokio::test]
    async fn test_agent_builder_and_run() {
        let model = MockModel::new("test");
        model.push_text("hello");

        let agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        let mut agent = agent;
        let result = agent.run_turn(AgentInput::text("hi")).await.unwrap();
        assert_eq!(result.stop_reason, StopReason::Completed);
    }

    #[tokio::test]
    async fn test_build_without_model() {
        // Building without model is allowed — agent can start in command-only mode
        let agent = AgentBuilder::new()
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        assert!(!agent.is_configured());
    }

    #[tokio::test]
    async fn test_run_without_model_fails() {
        let mut agent = AgentBuilder::new()
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .build()
            .unwrap();

        let result = agent.run_turn(AgentInput::text("hi")).await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("No model configured"),
            "error should mention no model: {msg}"
        );
    }

    #[test]
    fn test_build_error_missing_session() {
        let model = MockModel::new("test");
        let result = AgentBuilder::new()
            .model(model)
            .events(CollectingSink::new())
            .build();
        assert!(matches!(result, Err(BuildError::MissingSession)));
    }

    #[test]
    fn test_build_error_missing_events() {
        let model = MockModel::new("test");
        let result = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .build();
        assert!(matches!(result, Err(BuildError::MissingEvents)));
    }

    #[tokio::test]
    async fn test_agent_run_with_limits() {
        let model = MockModel::new("test");
        // Set up tool calls that will be executed (echo tool registered)
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_text("done");

        let agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .tool(EchoTool)
            .limits(RunLimits::new(2))
            .build()
            .unwrap();

        let mut agent = agent;
        let result = agent.run_turn(AgentInput::text("go")).await.unwrap();
        // Should hit max rounds after 2 rounds
        assert_eq!(result.stop_reason, StopReason::MaxRounds);
        assert_eq!(result.rounds, 2);
    }

    #[tokio::test]
    async fn test_agent_cancel_token() {
        let model = MockModel::new("test");
        let cancel = CancelToken::new();
        cancel.cancel(); // Cancel before run

        let agent = AgentBuilder::new()
            .model(model)
            .session(MemorySession::new())
            .events(CollectingSink::new())
            .cancel_token(cancel.clone())
            .build()
            .unwrap();

        let mut agent = agent;
        let result = agent.run_turn(AgentInput::text("go")).await.unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }

    #[test]
    fn test_builder_default() {
        // Building without session/events should fail
        let result = AgentBuilder::new().build();
        assert!(result.is_err());
        assert!(
            matches!(result.err().unwrap(), BuildError::MissingSession),
            "expected MissingSession"
        );
    }
}
