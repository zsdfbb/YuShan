//! `Agent::run(inbox)` 自转驱动语义的跨 crate 集成测试（执行计划 Task 18）。
//!
//! 放在 `tests/` 而非 `src/agent.rs` 内联，理由：
//! `run` 是 `ys-runtime` 的**公开 API**，测试需要同时组装 `ys-channel`（Inbox）、
//! `ys-event`（sink）、`ys-model`（MockModel）三个 crate 的真实类型。集成测试
//! 恰好只经公开面使用它们，与 `-p`/TUI 等真实消费者同形；且照 `v0_integration.rs`
//! 既有模板，不需要把生产代码的私有 helper 暴露给单测。
//!
//! 覆盖：followUp 自转、空 inbox 立即返回、`begin_turn` 编号、消费者消失
//! （`Err` 语义）、steering × followUp 协同，以及 `QueueMode` 两种粒度。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ys_channel::{Inbox, Intent, QueueMode};
use ys_core::{ContentBlock, EventError, Message, Role, StopReason, ToolCallId, Usage};
use ys_event::{AgentEvent, EventSink, FailingSink};
use ys_model::{MockModel, Model, ModelError, ModelEventSink, ModelRequest, ModelResponse};
use ys_runtime::prelude::*;

// ---------------------------------------------------------------------------
// 测试替身
// ---------------------------------------------------------------------------

/// 共享记录 sink：克隆句柄后仍可读回 `try_emit`/`emit` 收到的事件与 `begin_turn` 编号。
///
/// `CollectingSink` 不记录 `begin_turn`，故需此替身。用 `Arc<Mutex<..>>` 内可变，
/// 便于在 ports 借用结束后仍能读回断言（ADR-0010 后 sink 由调用方持有，不再移入 agent）。
#[derive(Clone, Default)]
struct SharedSink {
    events: Arc<Mutex<Vec<AgentEvent>>>,
    begin_turns: Arc<Mutex<Vec<u32>>>,
}

impl SharedSink {
    fn new() -> Self {
        Self::default()
    }

    fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    fn begin_turns(&self) -> Vec<u32> {
        self.begin_turns.lock().unwrap().clone()
    }

    fn user_message_texts(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::UserMessage { message } => Some(
                    message
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for SharedSink {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    fn emit<'a>(
        &'a mut self,
        event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(event);
            Ok(())
        })
    }

    fn begin_turn(&mut self, turn: u32) {
        self.begin_turns.lock().unwrap().push(turn);
    }
}

/// 计数 model：委托内层 `MockModel`，同时记录 `complete` 被调用次数。
struct CountingModel {
    inner: MockModel,
    calls: Arc<AtomicUsize>,
}

impl CountingModel {
    fn new(inner: MockModel, calls: Arc<AtomicUsize>) -> Self {
        Self { inner, calls }
    }
}

#[async_trait]
impl Model for CountingModel {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(request, sink).await
    }
}

/// 探针 model：逐次调用返回预设响应，并记录每次请求是否已含 steering 文本。
struct SteeringProbeModel {
    calls: Arc<AtomicUsize>,
    seen_steering: Arc<Mutex<Vec<bool>>>,
    inbox: Inbox,
}

impl SteeringProbeModel {
    fn new(inbox: Inbox) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            seen_steering: Arc::new(Mutex::new(Vec::new())),
            inbox,
        }
    }
}

fn text_response(text: &str) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        usage: Usage::default(),
        stop_reason: None,
    }
}

fn tool_call_response(name: &str) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: ToolCallId(format!("mock-{name}")),
                name: name.into(),
                arguments: serde_json::json!({}),
            }],
        },
        usage: Usage::default(),
        stop_reason: None,
    }
}

#[async_trait]
impl Model for SteeringProbeModel {
    fn model_id(&self) -> &str {
        "steering-probe"
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let has_steer = request.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("steer")))
        });
        self.seen_steering.lock().unwrap().push(has_steer);

        match n {
            // 首次 complete：中途投一条 steering（同回合后续轮可见），
            // 再投一条 followUp（本回合结束后由 run 自转拉起第 2 回合），
            // 并返回 tool call 迫使循环进入下一轮。
            0 => {
                self.inbox.push(text_message("steer-now"), Intent::Steering);
                self.inbox.push(text_message("later"), Intent::FollowUp);
                Ok(tool_call_response("echo"))
            }
            1 => Ok(text_response("done")),
            _ => Ok(text_response("second")),
        }
    }
}

fn text_message(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

struct EchoTool;

#[async_trait]
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

// ---------------------------------------------------------------------------
// 1. followUp 排队 → 自动跑下一趟
// ---------------------------------------------------------------------------

#[tokio::test]
async fn followup_batch_drives_one_turn_each() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = MockModel::new("test");
    mock.push_text("reply-1");
    mock.push_text("reply-2");
    let model = CountingModel::new(mock, calls.clone());
    let mut sink = SharedSink::new();
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().build().unwrap();

    let inbox = Inbox::new();
    inbox.push(text_message("a"), Intent::FollowUp);
    inbox.push(text_message("b"), Intent::FollowUp);

    let summary = agent
        .run(
            AgentPorts::new(Some(&model), &mut session, &mut sink),
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(summary.turns, 2, "两条 followUp → 两个回合");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "模型被调用 2 次");
    assert_eq!(summary.last_stop, Some(StopReason::Completed));
    assert_eq!(
        sink.user_message_texts(),
        vec!["a".to_string(), "b".to_string()],
        "事件里应有 2 个 UserMessage，顺序为 a、b"
    );
}

// ---------------------------------------------------------------------------
// 2. inbox 空 → 立即返回（零 model 调用）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_inbox_returns_immediately() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = MockModel::new("test");
    // 故意压入一条响应：若 run 错误地跑了一个回合，它会被消费（而非 panic），
    // 计数断言也会失败。
    mock.push_text("should-not-be-used");

    let model = CountingModel::new(mock, calls.clone());
    let mut sink = SharedSink::new();
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().build().unwrap();

    let inbox = Inbox::new();
    let summary = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run(
            AgentPorts::new(Some(&model), &mut session, &mut sink),
            &inbox,
        ),
    )
    .await
    .expect("空 inbox 的 run 必须立即返回（超时保护）")
    .unwrap();

    assert_eq!(summary.turns, 0);
    assert_eq!(summary.last_stop, None);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "零次 model.complete");
}

// ---------------------------------------------------------------------------
// 3. begin_turn 生效 → turn 编号正确
// ---------------------------------------------------------------------------

#[tokio::test]
async fn begin_turn_numbers_each_turn_from_one() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = MockModel::new("test");
    mock.push_text("one");
    mock.push_text("two");
    let model = CountingModel::new(mock, calls);
    let mut sink = SharedSink::new();
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().build().unwrap();

    let inbox = Inbox::new();
    inbox.push(text_message("a"), Intent::FollowUp);
    inbox.push(text_message("b"), Intent::FollowUp);

    agent
        .run(
            AgentPorts::new(Some(&model), &mut session, &mut sink),
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(
        sink.begin_turns(),
        vec![1, 2],
        "每回合开始调用一次 begin_turn，编号从 1 递增"
    );
}

// ---------------------------------------------------------------------------
// 4. 消费者消失 → run 返回 Err（既有 ADR-0004 语义）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consumer_gone_makes_run_return_err() {
    let model = MockModel::new("test");
    model.push_text("unused");
    let mut failing = FailingSink::new(0);
    // 慢路径默认成功；要模拟「消费者消失直达调用方」，须显式开启。
    failing.set_fail_slow(true);
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().build().unwrap();

    let inbox = Inbox::new();
    inbox.push(text_message("a"), Intent::FollowUp);

    let result = agent
        .run(
            AgentPorts::new(Some(&model), &mut session, &mut failing),
            &inbox,
        )
        .await;
    match result {
        Err(LoopError::Event(EventError::SendFailed)) => {}
        other => panic!("expected Err(LoopError::Event(SendFailed)), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. steering × followUp 协同
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steering_seen_in_same_turn_and_followup_starts_next() {
    let inbox = Inbox::new();
    let model = SteeringProbeModel::new(inbox.clone());
    let calls = model.calls.clone();
    let seen = model.seen_steering.clone();
    let mut sink = SharedSink::new();
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().tool(EchoTool).build().unwrap();

    // 起始一条 followUp 拉起第 1 回合；模型首次 complete 内再投 steering（同回合
    // 后续轮可见）与另一条 followUp（触发第 2 回合）。
    inbox.push(text_message("kick"), Intent::FollowUp);

    let summary = agent
        .run(
            AgentPorts::new(Some(&model), &mut session, &mut sink),
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(summary.turns, 2, "中途投递的 followUp 触发第 2 回合");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "turn1 两轮 + turn2 一轮");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(!seen[0], "第 1 轮不应看到尚未注入的 steering");
    assert!(seen[1], "同一回合的后续轮必须看到 steering（轮边界注入）");
}

// ---------------------------------------------------------------------------
// 6/7. QueueMode 粒度
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queue_mode_all_merges_followups_into_single_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = MockModel::new("test");
    mock.push_text("merged");
    let model = CountingModel::new(mock, calls.clone());
    let mut sink = SharedSink::new();
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().build().unwrap();

    let inbox = Inbox::with_modes(QueueMode::OneAtATime, QueueMode::All);
    for t in ["a", "b", "c"] {
        inbox.push(text_message(t), Intent::FollowUp);
    }

    let summary = agent
        .run(
            AgentPorts::new(Some(&model), &mut session, &mut sink),
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(summary.turns, 1, "All 合并为单回合");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "模型只被调一次");
}

#[tokio::test]
async fn queue_mode_one_at_a_time_runs_one_turn_per_message() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = MockModel::new("test");
    mock.push_text("a");
    mock.push_text("b");
    mock.push_text("c");

    let model = CountingModel::new(mock, calls.clone());
    let mut sink = SharedSink::new();
    let mut session = MemorySession::new();

    let mut agent = AgentBuilder::new().build().unwrap();

    let inbox = Inbox::new(); // 默认 OneAtATime
    for t in ["a", "b", "c"] {
        inbox.push(text_message(t), Intent::FollowUp);
    }

    let summary = agent
        .run(
            AgentPorts::new(Some(&model), &mut session, &mut sink),
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(summary.turns, 3, "每条一回合");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "模型被调 3 次");
}
