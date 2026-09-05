use agent_component::{RunLimits, RuntimeContext};
use agent_core::CancelToken;
use agent_core::{ContentBlock, Message, Role, ToolCallId};
use agent_event::CollectingSink;
use agent_loop::{AgentInput, AgentLoop, BasicLoop};
use agent_model::{Model, ModelError, ModelEventSink, ModelRequest, ModelResponse};
use agent_session::MemorySession;
use agent_tool::{Tool, ToolContext};
use agent_tools_basic::{EditTool, ReadTool, WriteTool};

// Mock model that echoes tool calls
struct MockCodingModel;

#[async_trait::async_trait]
impl Model for MockCodingModel {
    fn model_id(&self) -> &str {
        "mock-coding"
    }
    async fn complete(
        &self,
        request: ModelRequest,
        _sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        // Check if there are tool results - if so, finish
        let has_tool_results = request
            .messages
            .last()
            .map(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .unwrap_or(false);

        if has_tool_results {
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Task completed.".into(),
                    }],
                },
                usage: Default::default(),
                stop_reason: Some("stop".into()),
            })
        } else {
            // Return a read tool call
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolCallId("call_1".into()),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": "test.txt" }),
                    }],
                },
                usage: Default::default(),
                stop_reason: Some("tool_calls".into()),
            })
        }
    }
}

#[tokio::test]
async fn test_read_write_edit_roundtrip() {
    let tmp = std::env::temp_dir().join("yushan_test");
    std::fs::create_dir_all(&tmp).unwrap();

    let cancel = CancelToken::new();

    // Write a file
    let write_tool = WriteTool::new(tmp.clone());
    let ctx = ToolContext::new(&cancel, tmp.clone(), tmp.clone());
    let result = write_tool
        .call(
            serde_json::json!({ "path": "test.txt", "content": "hello world" }),
            ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);

    // Read it back
    let read_tool = ReadTool::new(tmp.clone());
    let ctx = ToolContext::new(&cancel, tmp.clone(), tmp.clone());
    let result = read_tool
        .call(serde_json::json!({ "path": "test.txt" }), ctx)
        .await
        .unwrap();
    assert!(result.content.contains("hello world"));

    // Edit it
    let edit_tool = EditTool::new(tmp.clone());
    let ctx = ToolContext::new(&cancel, tmp.clone(), tmp.clone());
    let result = edit_tool
        .call(
            serde_json::json!({
                "path": "test.txt",
                "edits": [{ "oldText": "hello", "newText": "goodbye" }]
            }),
            ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);

    // Read again to verify
    let ctx = ToolContext::new(&cancel, tmp.clone(), tmp.clone());
    let result = read_tool
        .call(serde_json::json!({ "path": "test.txt" }), ctx)
        .await
        .unwrap();
    assert!(result.content.contains("goodbye"));

    // Cleanup
    std::fs::remove_dir_all(&tmp).ok();
}

#[tokio::test]
async fn test_full_agent_turn_with_mock_model() {
    let tmp = std::env::temp_dir().join("yushan_agent_test");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("test.txt"), "initial content").unwrap();

    let model = MockCodingModel;
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();
    let cancel = CancelToken::new();
    let registry = agent_tool::ToolRegistry::build(vec![
        Box::new(ReadTool::new(tmp.clone())),
        Box::new(WriteTool::new(tmp.clone())),
        Box::new(EditTool::new(tmp.clone())),
    ])
    .unwrap();
    let limits = RunLimits::new(5);

    let result = {
        let mut ctx = RuntimeContext::new(
            &model,
            &registry,
            &mut session,
            &mut events,
            &cancel,
            limits,
            tmp.clone(),
            tmp.clone(),
            None,
            None,
        );
        let input = AgentInput::text("read test.txt");
        BasicLoop.run_turn(input, &mut ctx).await.unwrap()
    };

    assert_eq!(result.stop_reason, agent_core::StopReason::Completed);
    assert!(result.final_message.is_some());

    // Cleanup
    std::fs::remove_dir_all(&tmp).ok();
}
