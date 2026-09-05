//! v0 end-to-end integration tests
//!
//! Uses AgentBuilder to assemble complete Agent, verifying all key scenarios
//! through both the Agent API and the lower-level BasicLoop API.

use agent_core::{ContentBlock, Message, Role};
use agent_event::CollectingSink;
use agent_loop::{AgentLoop, BasicLoop};
use agent_model::MockModel;
use agent_runtime::prelude::*;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Test Tools
// ---------------------------------------------------------------------------

struct EchoTool;

#[async_trait::async_trait]
impl agent_tool::Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("echo", "echoes input", serde_json::json!({}))
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: input.to_string(),
            is_error: false,
        })
    }
}

struct FailingTool;

#[async_trait::async_trait]
impl agent_tool::Tool for FailingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("fail", "always fails", serde_json::json!({}))
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution("deliberate failure".into()))
    }
}

/// Helper to create a RuntimeContext with default cwd/workspace
fn make_ctx<'a>(
    model: &'a dyn agent_model::Model,
    registry: &'a agent_tool::ToolRegistry,
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

// ---------------------------------------------------------------------------
// TC2: Pure text reply
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc2_pure_text_reply() {
    let model = MockModel::new("test");
    model.push_text("Hello, World!");

    let agent = AgentBuilder::new()
        .model(model)
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let mut agent = agent;
    let result = agent.run_turn(AgentInput::text("hi")).await.unwrap();

    assert_eq!(result.stop_reason, StopReason::Completed);
    assert!(result.final_message.is_some());
    assert_eq!(result.rounds, 1);
}

// ---------------------------------------------------------------------------
// TC3: Single tool call loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc3_single_tool_call() {
    let model = MockModel::new("test");
    model.push_tool_call("echo", serde_json::json!({"message": "test"}));
    model.push_text("done");

    let agent = AgentBuilder::new()
        .model(model)
        .tool(EchoTool)
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let mut agent = agent;
    let result = agent.run_turn(AgentInput::text("go")).await.unwrap();

    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.rounds, 2); // 1 tool round + 1 text round
    assert!(result.final_message.is_some());
}

// ---------------------------------------------------------------------------
// TC4: Multi-round tool loop (2 consecutive tool calls, then text)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc4_multi_round_tool_loop() {
    let model = MockModel::new("test");
    model.push_tool_call("echo", serde_json::json!({"message": "first"}));
    model.push_tool_call("echo", serde_json::json!({"message": "second"}));
    model.push_text("all done");

    let agent = AgentBuilder::new()
        .model(model)
        .tool(EchoTool)
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let mut agent = agent;
    let result = agent.run_turn(AgentInput::text("go")).await.unwrap();

    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.rounds, 3); // 2 tool rounds + 1 text round
}

// ---------------------------------------------------------------------------
// TC5: Tool error fed back to model (T11c: error recovery, not termination)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc5_tool_error_fed_back_to_model() {
    let model = MockModel::new("test");
    model.push_tool_call("fail", serde_json::json!({}));
    // After tool error, model responds with text (error was fed back as result)
    model.push_text("I see the tool failed, continuing anyway");

    let agent = AgentBuilder::new()
        .model(model)
        .tool(FailingTool)
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let mut agent = agent;
    let result = agent.run_turn(AgentInput::text("go")).await.unwrap();

    // Tool errors are now fed back as results, loop continues
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert!(result.final_message.is_some());
}

// ---------------------------------------------------------------------------
// TC8: Incremental text concatenation == final message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc8_incremental_concat_matches_final() {
    let model = MockModel::new("test");
    model.push_tool_call("echo", serde_json::json!({}));
    model.push_text("Hello, World!");

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
        BasicLoop
            .run_turn(AgentInput::text("go"), &mut ctx)
            .await
            .unwrap()
    };

    // Collect all ModelTextDelta texts
    let delta_text: String = events
        .events()
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ModelTextDelta { text } = e {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();

    // Final message text
    let final_text = result
        .final_message
        .as_ref()
        .and_then(|msg| {
            msg.content.iter().find_map(|block| {
                if let ContentBlock::Text { text } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            })
        })
        .unwrap_or("");

    assert!(
        !delta_text.is_empty(),
        "should have at least one text delta"
    );
    assert_eq!(
        delta_text, final_text,
        "concatenated deltas must equal final message text"
    );
}

// ---------------------------------------------------------------------------
// TC9: Terminal event invariant - exactly one RunFinished or RunFailed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc9_terminal_event_invariant() {
    let model = MockModel::new("test");
    model.push_text("done");

    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();
    let cancel = CancelToken::new();
    let registry = ToolRegistry::build(vec![]).unwrap();
    let limits = RunLimits::new(5);

    {
        let mut ctx = make_ctx(
            &model,
            &registry,
            &mut session,
            &mut events,
            &cancel,
            limits,
        );
        let _ = BasicLoop.run_turn(AgentInput::text("go"), &mut ctx).await;
    }

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

// ---------------------------------------------------------------------------
// TC14: Continuation with pre-filled history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc14_continuation_with_history() {
    let model = MockModel::new("test");
    model.push_text("new answer");

    let mut session = MemorySession::new();
    session
        .append(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "previous question".into(),
            }],
        })
        .await
        .unwrap();
    session
        .append(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "previous answer".into(),
            }],
        })
        .await
        .unwrap();

    let agent = AgentBuilder::new()
        .model(model)
        .session(session)
        .events(CollectingSink::new())
        .build()
        .unwrap();

    let mut agent = agent;
    let result = agent.run_turn(AgentInput::text("follow up")).await.unwrap();

    assert_eq!(result.stop_reason, StopReason::Completed);
    assert!(result.final_message.is_some());
}

// ---------------------------------------------------------------------------
// TC10: Duplicate tool name causes build error
// ---------------------------------------------------------------------------

#[test]
fn tc10_duplicate_tool_name_build_error() {
    let result = AgentBuilder::new()
        .model(MockModel::new("test"))
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .tool(EchoTool)
        .tool(EchoTool) // duplicate name "echo"
        .build();

    assert!(result.is_err());
    match result {
        Err(BuildError::ToolRegistry(msg)) => {
            assert!(
                msg.contains("duplicate"),
                "error should mention duplicate: {msg}"
            );
        }
        Err(other) => panic!("expected ToolRegistry error, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

// ---------------------------------------------------------------------------
// Cancel: mid-loop cancel via tool execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_mid_loop() {
    struct CancellingTool {
        cancel: CancelToken,
    }

    #[async_trait::async_trait]
    impl agent_tool::Tool for CancellingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("cancel", "cancels the run", serde_json::json!({}))
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            self.cancel.cancel();
            Ok(ToolResult {
                content: "cancelled".into(),
                is_error: false,
            })
        }
    }

    let cancel_token = CancelToken::new();

    let model = MockModel::new("test");
    model.push_tool_call("cancel", serde_json::json!({}));

    let agent = AgentBuilder::new()
        .model(model)
        .tool(CancellingTool {
            cancel: cancel_token.clone(),
        })
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .cancel_token(cancel_token)
        .build()
        .unwrap();

    let mut agent = agent;
    let result = agent.run_turn(AgentInput::text("go")).await.unwrap();

    // Cancel was set during tool execution, next loop boundary detects it
    assert_eq!(result.stop_reason, StopReason::Cancelled);
}
