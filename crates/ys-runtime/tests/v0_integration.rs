//! v0 端到端集成测试
//!
//! 使用 AgentBuilder 组装完整的 Agent，
//! 通过 Agent API 与低层 BasicLoop API 共同验证所有关键场景。

use std::path::PathBuf;
use ys_core::{ContentBlock, Message, Role};
use ys_event::CollectingSink;
use ys_loop::{AgentLoop, BasicLoop};
use ys_model::MockModel;
use ys_runtime::prelude::*;

// ---------------------------------------------------------------------------
// 测试工具
// ---------------------------------------------------------------------------

struct EchoTool;

#[async_trait::async_trait]
impl ys_tool::Tool for EchoTool {
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
impl ys_tool::Tool for FailingTool {
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

/// 用默认 cwd/workspace 创建 RuntimeContext 的辅助函数
fn make_ctx<'a>(
    model: &'a dyn ys_model::Model,
    registry: &'a ys_tool::ToolRegistry,
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

// ---------------------------------------------------------------------------
// TC2：纯文本回复
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
// TC3：单工具调用循环
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
    assert_eq!(result.rounds, 2); // 1 个工具 round + 1 个文本 round
    assert!(result.final_message.is_some());
}

// ---------------------------------------------------------------------------
// TC4：多轮工具循环（连续 2 次工具调用，然后文本）
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
    assert_eq!(result.rounds, 3); // 2 个工具 round + 1 个文本 round
}

// ---------------------------------------------------------------------------
// TC5：tool 错误回喂给模型（T11c：错误恢复而非终止）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tc5_tool_error_fed_back_to_model() {
    let model = MockModel::new("test");
    model.push_tool_call("fail", serde_json::json!({}));
    // tool 错误之后，模型以文本回复（错误已作为结果回喂）
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

    // tool 错误现在作为结果回喂，循环继续
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert!(result.final_message.is_some());
}

// ---------------------------------------------------------------------------
// TC8：增量文本拼接 == 最终消息
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

    // 收集所有 ModelTextDelta 文本
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

    // 最终消息文本
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
// TC9：终止事件不变量 —— 恰好一个 RunFinished 或 RunFailed
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
// TC14：带预填历史记录继续会话
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
// TC10：重复工具名称导致构建错误
// ---------------------------------------------------------------------------

#[test]
fn tc10_duplicate_tool_name_build_error() {
    let result = AgentBuilder::new()
        .model(MockModel::new("test"))
        .session(MemorySession::new())
        .events(CollectingSink::new())
        .tool(EchoTool)
        .tool(EchoTool) // 重复名称 "echo"
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
// 取消：经工具执行在循环中途取消
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_mid_loop() {
    struct CancellingTool {
        cancel: CancelToken,
    }

    #[async_trait::async_trait]
    impl ys_tool::Tool for CancellingTool {
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

    // 工具执行期间已设置取消，下一个 loop 边界检测到它
    assert_eq!(result.stop_reason, StopReason::Cancelled);
}
