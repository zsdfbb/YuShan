//! `Agent::run_turn` 的轮边界语义测试。
//!
//! ADR-0010 后 `Agent` 无状态：会话 / 事件 / 模型 / 边界源都作为
//! [`AgentPorts`] 借用传入。本文件覆盖**端口层面**的轮边界契约：
//!
//! - 预置 `Abort` → `Cancelled`，且用户消息**不落会话**；
//! - `boundary = None` → 与无边界源时行为一致（回归保护）；
//! - 轮中 `Steer` → 进入**下一轮** `ModelRequest`，且**不增 rounds**；
//! - `model = None` → `LoopError::ConfigError`。
//!
//! 多回合驱动 / `begin_turn` 编号属 app 层语义（T13），此处不测。

use std::sync::Mutex;

use ys_event::CollectingSink;
use ys_model::{MockModel, Model, ModelError, ModelEventSink, ModelRequest, ModelResponse};
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

fn text_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn content_has_text(msg: &Message, text: &str) -> bool {
    msg.content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text: t } if t == text))
}

/// 可窥探每轮 `ModelRequest` 的 model：第 1 轮返回 `tool_call`（逼出第 2 轮），
/// 第 2 轮返回文本；`inject` 时在第 1 轮调用中向边界源注入一条 steering。
struct SteeringProbeModel {
    boundary: std::sync::Arc<QueueBoundarySource>,
    inject: bool,
    calls: Mutex<usize>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl SteeringProbeModel {
    fn new(boundary: std::sync::Arc<QueueBoundarySource>, inject: bool) -> Self {
        Self {
            boundary,
            inject,
            calls: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Model for SteeringProbeModel {
    fn model_id(&self) -> &str {
        "steering-probe"
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        let call_index = {
            let mut calls = self.calls.lock().unwrap();
            let idx = *calls;
            *calls += 1;
            idx
        };
        if call_index == 0 {
            if self.inject {
                self.boundary.push(Boundary::Steer(text_msg("steer!")));
            }
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolCallId("tc-1".into()),
                        name: "echo".into(),
                        arguments: serde_json::json!({}),
                    }],
                },
                usage: Usage::default(),
                stop_reason: None,
            })
        } else {
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
                usage: Usage::default(),
                stop_reason: None,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// 预置 Abort：Cancelled，且用户消息不落会话
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preset_abort_cancels_before_appending_user_message() {
    let model = MockModel::new("test");
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();

    let boundary = QueueBoundarySource::new();
    boundary.push(Boundary::Abort); // 运行前已中止

    let mut agent = AgentBuilder::new().build().unwrap();
    let result = agent
        .run_turn(
            AgentInput::text("go"),
            AgentPorts::new(Some(&model), &mut session, &mut events, Some(&boundary)),
        )
        .await
        .unwrap();

    assert_eq!(result.stop_reason, StopReason::Cancelled);
    assert_eq!(result.rounds, 0);
    assert!(
        session.messages().is_empty(),
        "入口中止发生在 Step 2 之前：用户消息不得落会话"
    );
    // 终局事件恰好一条且为 RunFinished{Cancelled}
    let terminal: Vec<_> = events
        .events()
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
            )
        })
        .collect();
    assert_eq!(terminal.len(), 1, "exactly one terminal event");
    assert!(matches!(
        terminal[0],
        AgentEvent::RunFinished {
            stop_reason: StopReason::Cancelled,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// boundary = None：正常跑完（回归保护）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn none_boundary_runs_to_completion() {
    let model = MockModel::new("test");
    model.push_text("Hello");
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();

    let mut agent = AgentBuilder::new().build().unwrap();
    let result = agent
        .run_turn(
            AgentInput::text("hi"),
            AgentPorts::new(Some(&model), &mut session, &mut events, None),
        )
        .await
        .unwrap();

    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.rounds, 1);
    assert!(result.final_message.is_some());
}

// ---------------------------------------------------------------------------
// boundary = None 与「有边界源但不投递」行为一致（回归保护）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn none_boundary_matches_idle_boundary_source() {
    // 无边界源
    let model_a = MockModel::new("test");
    model_a.push_text("Hello");
    let mut session_a = MemorySession::new();
    let mut events_a = CollectingSink::new();
    let mut agent = AgentBuilder::new().build().unwrap();
    let result_a = agent
        .run_turn(
            AgentInput::text("hi"),
            AgentPorts::new(Some(&model_a), &mut session_a, &mut events_a, None),
        )
        .await
        .unwrap();

    // 有边界源但什么都不投递
    let model_b = MockModel::new("test");
    model_b.push_text("Hello");
    let mut session_b = MemorySession::new();
    let mut events_b = CollectingSink::new();
    let idle = QueueBoundarySource::new();
    let result_b = agent
        .run_turn(
            AgentInput::text("hi"),
            AgentPorts::new(Some(&model_b), &mut session_b, &mut events_b, Some(&idle)),
        )
        .await
        .unwrap();

    assert_eq!(result_a.stop_reason, result_b.stop_reason);
    assert_eq!(result_a.rounds, result_b.rounds);
    assert_eq!(
        session_a.messages().len(),
        session_b.messages().len(),
        "空闲边界源不得改变会话形状"
    );
    assert_eq!(
        events_a.events().len(),
        events_b.events().len(),
        "空闲边界源不得多出任何事件"
    );
}

// ---------------------------------------------------------------------------
// 轮中 Steer：进入下一轮 request，且不增 rounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steer_mid_round_enters_next_request_without_adding_round() {
    let boundary = std::sync::Arc::new(QueueBoundarySource::new());
    let model = SteeringProbeModel::new(boundary.clone(), true);
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();

    let mut agent = AgentBuilder::new().tool(EchoTool).build().unwrap();
    let result = agent
        .run_turn(
            AgentInput::text("go"),
            AgentPorts::new(
                Some(&model),
                &mut session,
                &mut events,
                Some(boundary.as_ref()),
            ),
        )
        .await
        .unwrap();

    let requests = model.requests();
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(requests.len(), 2, "两轮：tool_call → 文本");
    assert_eq!(result.rounds, 2, "steering 是轮内输入，不增 rounds");

    // 第 1 轮 request 不应含注入消息（注入发生于第 1 轮之后）
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|m| content_has_text(m, "steer!")),
        "第 1 轮 request 不应含注入消息"
    );
    // 第 2 轮 request 应含注入消息，且位于队尾（工具结果之后）
    let last = requests[1].messages.last().expect("第 2 轮 request 非空");
    assert_eq!(last.role, Role::User);
    assert!(
        content_has_text(last, "steer!"),
        "第 2 轮 request 末尾应为注入的 steering user 消息；实际 = {:?}",
        requests[1].messages
    );
}

// ---------------------------------------------------------------------------
// model = None：ConfigError
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_model_returns_config_error() {
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();

    let mut agent = AgentBuilder::new().build().unwrap();
    let err = agent
        .run_turn(
            AgentInput::text("go"),
            AgentPorts::new(None, &mut session, &mut events, None),
        )
        .await
        .err()
        .expect("未配置 model 应返回错误");

    assert!(
        matches!(err, LoopError::ConfigError(_)),
        "expected LoopError::ConfigError, got {err:?}"
    );
}
