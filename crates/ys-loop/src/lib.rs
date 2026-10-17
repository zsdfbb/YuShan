//! Agent 执行循环与编排逻辑。

mod basic;
mod compact;
mod error;
mod input;
mod result;
mod summarize;
pub mod token;
mod turn_budget;

pub use basic::*;
pub use compact::*;
pub use error::*;
pub use input::*;
pub use result::*;
pub use summarize::*;
pub use turn_budget::*;

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
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use ys_component::{RunLimits, RuntimeContext};
    use ys_core::*;
    use ys_event::{AgentEvent, CollectingSink, FailingSink};
    use ys_model::{
        FinishReason, MockModel, Model, ModelError, ModelEventSink, ModelRequest, ModelResponse,
    };
    use ys_protocol::{Boundary, BoundarySource, QueueBoundarySource};
    use ys_session::{MemorySession, Session};
    use ys_tool::{Tool, ToolContext, ToolRegistry};

    /// 用默认 cwd/workspace 创建 RuntimeContext 的辅助函数（不挂边界源）。
    fn make_ctx<'a>(
        model: &'a dyn ys_model::Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn ys_session::Session,
        events: &'a mut dyn ys_event::EventSink,
        limits: RunLimits,
    ) -> RuntimeContext<'a> {
        RuntimeContext::new(
            model,
            registry,
            session,
            events,
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
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            let input = AgentInput::text("hi");
            let loop_impl = BasicLoop;
            loop_impl.run_turn(input, &mut ctx).await.unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert!(result.final_message.is_some());
        assert_eq!(result.rounds, 1);

        // 检查事件（纯文本路径）。Step 7 新增一条逐轮 `RoundUsage`（在模型返回后、
        // 终局事件前），故序列是：UserMessage → RoundStarted → ModelTextDelta →
        // RoundUsage → RunFinished。
        let evts = events.events();
        assert!(matches!(&evts[0], AgentEvent::UserMessage { .. }));
        assert!(
            matches!(&evts[1], AgentEvent::RoundStarted { round: 1 }),
            "轮开始事件必须先于该轮的模型输出，且首轮为 1：{evts:?}"
        );
        assert!(matches!(&evts[2], AgentEvent::ModelTextDelta { text } if text == "hello world"));
        assert!(
            matches!(
                &evts[3],
                AgentEvent::RoundUsage {
                    purpose: ys_event::RoundPurpose::Turn,
                    round: 1,
                    ..
                }
            ),
            "每轮追加一条 RoundUsage：{evts:?}"
        );
        assert!(matches!(
            &evts[4],
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
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
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

    // U3-P1：**唯一生产接线点**的探针工具。
    //
    // 为什么需要它：`tools-web` 的 W4 用例是**直接注入假 `Summarizer`** 构造
    // `ToolContext` 的，结构性绕过了 `basic.rs` 里 `.with_summarizer(&summarizer)`
    // 那一行 —— 把整段接线删掉，全仓测试依然全绿。本探针把「接线本身」变成被测对象：
    // 它由真实的 `BasicLoop::run_turn` 驱动，只记录拿到的 `ToolContext` 长什么样。
    //
    // ⚠️ 这里配 `MockModel` 是**允许**的：设计 R8 的禁令只针对「让 `ModelSummarizer`
    // 接 `MockModel`」（那会吃掉主循环的响应队列）。探针工具**不调 `complete`**，
    // 不消费模型队列。
    struct ProbeTool {
        /// 记录工具被调用时 `ToolContext.summarizer` 是否为 `Some`。
        saw_summarizer: Arc<AtomicBool>,
        /// 记录工具被调用时 `ToolContext.boundary` 是否为 `Some`。
        saw_boundary: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Tool for ProbeTool {
        fn spec(&self) -> ys_tool::ToolSpec {
            ys_tool::ToolSpec::new(
                "probe",
                "records what the loop wired into ToolContext",
                serde_json::json!({}),
            )
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolContext<'_>,
        ) -> Result<ys_tool::ToolResult, ys_tool::ToolError> {
            self.saw_summarizer
                .store(ctx.summarizer.is_some(), Ordering::SeqCst);
            self.saw_boundary
                .store(ctx.boundary.is_some(), Ordering::SeqCst);
            Ok(ys_tool::ToolResult {
                content: "probed".into(),
                is_error: false,
            })
        }
    }

    // U3-P1：`BasicLoop::run_turn` 必须给工具挂上 `ModelSummarizer`
    //
    // 通路是真实的两轮循环（tool_call → 文本），与 `test_tool_call_loop` 同构，
    // 唯一差别是工具换成探针。变异自检：删掉 `basic.rs` 的 `.with_summarizer(...)`
    // ⇒ 本用例必须变红。
    #[tokio::test]
    async fn test_loop_wires_summarizer_into_tool_context() {
        let model = MockModel::new("m");
        model.push_tool_call("probe", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let saw_summarizer = Arc::new(AtomicBool::new(false));
        let saw_boundary = Arc::new(AtomicBool::new(false));
        let registry = ToolRegistry::build(vec![Box::new(ProbeTool {
            saw_summarizer: saw_summarizer.clone(),
            saw_boundary: saw_boundary.clone(),
        })])
        .unwrap();
        let limits = RunLimits::new(5);
        let boundary = Arc::new(QueueBoundarySource::new());

        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits)
                .with_boundary(boundary.as_ref());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert!(
            saw_summarizer.load(Ordering::SeqCst),
            "BasicLoop 必须经 `with_summarizer` 把 ModelSummarizer 挂进 ToolContext；\
             为 false 说明唯一的摘要接线点断了（或整段被删）"
        );
        assert!(
            saw_boundary.load(Ordering::SeqCst),
            "ToolContext 应带上本回合的 boundary（工具靠它实现取消轮询）"
        );
    }

    // TC6：入口即中止（Abort 取代 CancelToken）
    #[tokio::test]
    async fn test_abort_at_entry() {
        let model = MockModel::new("m");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let boundary = QueueBoundarySource::new();
        boundary.push(Boundary::Abort); // 运行前已中止
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx =
            make_ctx(&model, &registry, &mut session, &mut events, limits).with_boundary(&boundary);
        let input = AgentInput::text("go");
        let loop_impl = BasicLoop;
        let result = loop_impl.run_turn(input, &mut ctx).await.unwrap();

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(result.rounds, 0);
        assert!(
            session.messages().is_empty(),
            "入口中止不得追加任何消息（用户输入也不落盘）"
        );
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
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(2); // 最多 2 个 round

        let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
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
        let registry = ToolRegistry::build(vec![Box::new(FailingTool)]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
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

    // 循环迭代中途中止（mid-loop）：工具执行期间推入 Abort
    #[tokio::test]
    async fn test_abort_during_loop() {
        let model = MockModel::new("m");
        model.push_tool_call("echo", serde_json::json!({}));
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let boundary = Arc::new(QueueBoundarySource::new());
        let limits = RunLimits::new(5);

        // 模拟「工具跑动期间用户按 Esc」：工具向边界源推入 Abort
        struct AbortingTool {
            boundary: Arc<QueueBoundarySource>,
        }
        #[async_trait::async_trait]
        impl ys_tool::Tool for AbortingTool {
            fn spec(&self) -> ys_tool::ToolSpec {
                ys_tool::ToolSpec::new("echo", "aborts then echoes", serde_json::json!({}))
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: ys_tool::ToolContext<'_>,
            ) -> Result<ys_core::ToolResult, ys_tool::ToolError> {
                self.boundary.push(Boundary::Abort);
                Ok(ys_core::ToolResult {
                    content: "ok".into(),
                    is_error: false,
                })
            }
        }

        let registry = ToolRegistry::build(vec![Box::new(AbortingTool {
            boundary: boundary.clone(),
        })])
        .unwrap();
        let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits)
            .with_boundary(boundary.as_ref());
        let input = AgentInput::text("go");
        let loop_impl = BasicLoop;
        let result = loop_impl.run_turn(input, &mut ctx).await.unwrap();

        // 应在下一个轮边界以 Cancelled 完成
        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(result.rounds, 1, "第 1 轮已跑完，第 2 轮入口前中止");
    }

    // TC9：终止事件不变量 —— 恰好一个终止事件
    #[tokio::test]
    async fn test_terminal_event_invariant() {
        let model = MockModel::new("m");
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let _ = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
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
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
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

    // ================= Task 16：轮边界 steering 注入 =================

    /// 可窥探每轮 `ModelRequest` 的 model：
    /// 第 1 轮返回 tool_call（迫使进入第 2 轮），第 2 轮返回文本；
    /// `inject = true` 时在第 1 轮调用中向边界源注入一条 steering。
    struct SteeringProbeModel {
        boundary: Arc<QueueBoundarySource>,
        inject: bool,
        calls: Mutex<usize>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl SteeringProbeModel {
        fn new(boundary: Arc<QueueBoundarySource>, inject: bool) -> Self {
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
                // 第 1 轮：模拟运行期间用户中途插话
                if self.inject {
                    self.boundary.push(Boundary::Steer(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "steer!".into(),
                        }],
                    }));
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
                    finish_reason: FinishReason::Stop,
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
                    finish_reason: FinishReason::Stop,
                })
            }
        }
    }

    /// 跑一遍「tool_call → 文本」两轮场景，返回结果、各轮 request、会话与事件。
    async fn run_steering_scenario(
        inject: bool,
    ) -> (RunResult, Vec<ModelRequest>, MemorySession, CollectingSink) {
        let boundary = Arc::new(QueueBoundarySource::new());
        let model = SteeringProbeModel::new(boundary.clone(), inject);
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits)
                .with_boundary(boundary.as_ref());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };
        (result, model.requests(), session, events)
    }

    fn content_has_text(msg: &Message, text: &str) -> bool {
        msg.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text: t } if t == text))
    }

    /// 含压缩摘要标记（`[Context Summary]`）的消息（子串匹配 —— 摘要是拼接出来的长文本）。
    fn summary_messages(session: &MemorySession) -> Vec<&Message> {
        session
            .messages()
            .iter()
            .filter(|m| {
                m.content.iter().any(|b| {
                    matches!(b, ContentBlock::Text { text } if text.contains("[Context Summary]"))
                })
            })
            .collect()
    }

    // Task 16-1：轮边界注入的 steering 出现在**下一轮**的 ModelRequest 里
    #[tokio::test]
    async fn test_steering_injected_into_next_round_request() {
        let (result, requests, _session, _events) = run_steering_scenario(true).await;

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert_eq!(requests.len(), 2, "两轮：tool_call → 文本");
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|m| content_has_text(m, "steer!")),
            "第 1 轮 request 不应含注入消息（注入发生于第 1 轮之后）"
        );
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| content_has_text(m, "steer!")),
            "第 2 轮 request 应含注入的 steering user 消息；实际 = {:?}",
            requests[1].messages
        );
        // 注入消息以 user 角色进入，且位于队尾（工具结果之后）
        let last = requests[1].messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(content_has_text(last, "steer!"));
    }

    // Task 16-2：注入不增 rounds（与不注入时相同）
    #[tokio::test]
    async fn test_steering_injection_does_not_add_round() {
        let (with, with_reqs, _, _) = run_steering_scenario(true).await;
        let (without, without_reqs, _, _) = run_steering_scenario(false).await;

        // 注入确实发生了（否则「相等」无意义）
        assert!(
            with_reqs[1]
                .messages
                .iter()
                .any(|m| content_has_text(m, "steer!")),
            "前置条件：inject=true 时消息应已注入"
        );
        assert!(
            !without_reqs[1]
                .messages
                .iter()
                .any(|m| content_has_text(m, "steer!")),
            "前置条件：inject=false 时不应有注入消息"
        );

        assert_eq!(with.rounds, without.rounds, "注入不得增加 rounds");
        assert_eq!(with.rounds, 2);
        assert_eq!(with.stop_reason, without.stop_reason);
    }

    // Task 16-3：boundary = None 时行为不变（复用既有文本路径断言）
    #[tokio::test]
    async fn test_no_boundary_behavior_unchanged() {
        let model = MockModel::new("m");
        model.push_text("hello world");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("hi"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert_eq!(result.rounds, 1);
        assert!(result.final_message.is_some());

        // 事件序列恰好是这五条（不得多也不得少）：
        // UserMessage → RoundStarted → ModelTextDelta → RoundUsage → RunFinished
        // Step 7 新增的 `RoundUsage` 每轮一条，位置在模型返回后、终局事件之前。
        let evts = events.events();
        assert_eq!(evts.len(), 5, "无 boundary 时不得多出这五条以外的事件");
        assert!(matches!(&evts[0], AgentEvent::UserMessage { .. }));
        assert!(matches!(&evts[1], AgentEvent::RoundStarted { round: 1 }));
        assert!(matches!(&evts[2], AgentEvent::ModelTextDelta { text } if text == "hello world"));
        assert!(matches!(
            &evts[3],
            AgentEvent::RoundUsage {
                purpose: ys_event::RoundPurpose::Turn,
                round: 1,
                ..
            }
        ));
        assert!(matches!(
            &evts[4],
            AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                ..
            }
        ));
    }

    // Task 16-4：注入的消息进入 session，并发出 UserMessage 事件
    #[tokio::test]
    async fn test_steering_injected_message_enters_session() {
        let (_result, _requests, session, events) = run_steering_scenario(true).await;

        assert!(
            session
                .messages()
                .iter()
                .any(|m| content_has_text(m, "steer!")),
            "注入消息应被 append 进 session"
        );

        let injected_events: Vec<_> = events
            .events()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::UserMessage { message } if content_has_text(message, "steer!")
                )
            })
            .collect();
        assert_eq!(
            injected_events.len(),
            1,
            "注入应恰好发出一条 UserMessage 事件"
        );
    }

    // ================= Task 3：轮边界的 `Boundary::Abort` 分支 =================

    /// 测试替身：把 `take()` 与 `is_aborted()` **解耦**。
    ///
    /// [`QueueBoundarySource::push`] 对 `Abort` 会**同时**置 `aborted` 标记，
    /// 于是 Step 3 的 `is_aborted()` 探针总是先行命中，`basic.rs` 轮边界里的
    /// `Boundary::Abort` 分支在真实实现下几乎不可达。本替身让 `is_aborted()`
    /// 恒为 false 而 `take()` 照常交出 `Abort`，从而直接驱动那个分支 ——
    /// 精确对应「Step 3 探针已过、Step 4b 压缩 `await` 期间 Abort 才到」的时序。
    struct DecoupledBoundarySource {
        queue: Mutex<VecDeque<Boundary>>,
        aborted: AtomicBool,
    }

    impl DecoupledBoundarySource {
        fn new() -> Self {
            Self {
                queue: Mutex::new(VecDeque::new()),
                aborted: AtomicBool::new(false),
            }
        }

        /// 只入队，**不**置 `aborted` 标记（与 `QueueBoundarySource` 的关键差异）。
        fn push(&self, boundary: Boundary) {
            self.queue.lock().unwrap().push_back(boundary);
        }
    }

    impl BoundarySource for DecoupledBoundarySource {
        fn take(&self) -> Option<Boundary> {
            self.queue.lock().unwrap().pop_front()
        }
        fn is_aborted(&self) -> bool {
            self.aborted.load(Ordering::SeqCst)
        }
    }

    /// 模型的每次 `complete` 都往边界源注入 `Abort`，并返回一个 tool_call
    /// （迫使循环回到轮边界）。
    struct AbortInjectingModel {
        boundary: Arc<DecoupledBoundarySource>,
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Model for AbortInjectingModel {
        fn model_id(&self) -> &str {
            "abort-injecting"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            *self.calls.lock().unwrap() += 1;
            self.boundary.push(Boundary::Abort);
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
                finish_reason: FinishReason::Stop,
            })
        }
    }

    /// 每次都计数并返回纯文本的模型；用于断言「模型根本没被调用」。
    /// （比 `MockModel` 的 `remove(0)` panic 给出更直白的失败信号。）
    struct CountingModel {
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Model for CountingModel {
        fn model_id(&self) -> &str {
            "counting"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            *self.calls.lock().unwrap() += 1;
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    // Task 3-1：轮边界 Abort —— 一轮未跑即中止（直击 `while let` 的 Abort 分支）
    #[tokio::test]
    async fn test_abort_at_round_boundary_branch() {
        let model = CountingModel {
            calls: Mutex::new(0),
        };
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let boundary = DecoupledBoundarySource::new();
        boundary.push(Boundary::Abort);
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx =
            make_ctx(&model, &registry, &mut session, &mut events, limits).with_boundary(&boundary);
        let result = BasicLoop
            .run_turn(AgentInput::text("go"), &mut ctx)
            .await
            .unwrap();

        assert_eq!(
            result.stop_reason,
            StopReason::Cancelled,
            "轮边界的 Abort 必须终止 turn"
        );
        assert_eq!(result.rounds, 0, "一轮都没跑完就中止");
        assert!(result.final_message.is_none(), "中止不得带 final_message");
        assert_eq!(
            *model.calls.lock().unwrap(),
            0,
            "轮边界中止必须先于本轮的 model 调用"
        );

        // 终局事件不变量：恰好一个，且是 RunFinished{Cancelled, rounds: 0}
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
        assert!(
            matches!(
                terminal[0],
                AgentEvent::RunFinished {
                    stop_reason: StopReason::Cancelled,
                    rounds: 0,
                    ..
                }
            ),
            "终局事件应为 RunFinished{{Cancelled, rounds: 0}}，实际 = {:?}",
            terminal[0]
        );
        // 且必须是事件流的**最后一条**（中止后不得再冒泡任何事件）
        assert!(
            matches!(
                events.events().last(),
                Some(AgentEvent::RunFinished {
                    stop_reason: StopReason::Cancelled,
                    ..
                })
            ),
            "事件流末尾应为 RunFinished{{Cancelled}}，实际 = {:?}",
            events.events().last()
        );

        // 轮边界的中止发生在 Step 2 之后：用户输入**已**入会话（与入口中止相区分）
        assert_eq!(
            session.messages().len(),
            1,
            "轮边界中止时用户消息已追加；入口中止才是空会话"
        );
    }

    // Task 3-2：轮边界 Abort —— 第 1 轮跑完后、第 2 轮 model 调用前中止
    //
    // 与既有 `test_abort_during_loop` 的差别：那里 `push(Abort)` 会置位标记，
    // 由 Step 3 探针命中；这里标记保持 false，走的是 `while let` 内的 Abort 分支。
    #[tokio::test]
    async fn test_abort_at_round_boundary_after_completed_round() {
        let boundary = Arc::new(DecoupledBoundarySource::new());
        let model = AbortInjectingModel {
            boundary: boundary.clone(),
            calls: Mutex::new(0),
        };
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let registry = ToolRegistry::build(vec![Box::new(EchoTool)]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits)
            .with_boundary(boundary.as_ref());
        let result = BasicLoop
            .run_turn(AgentInput::text("go"), &mut ctx)
            .await
            .unwrap();

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(result.rounds, 1, "第 1 轮已跑完，第 2 轮 model 调用前中止");
        assert_eq!(
            *model.calls.lock().unwrap(),
            1,
            "只应调用一次 model（第 2 轮被轮边界拦下）"
        );
        // 用户消息 + assistant tool_use + 工具结果消息
        assert_eq!(session.messages().len(), 3);

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
        assert_eq!(terminal_count, 1, "exactly one terminal event");
    }

    // Task 3-3：Abort 抵达前已排队的 Steer 仍先被消费（保序），随后才中止
    #[tokio::test]
    async fn test_steer_before_abort_is_consumed_in_order() {
        let model = MockModel::new("m");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let boundary = DecoupledBoundarySource::new();
        boundary.push(Boundary::Steer(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "steer!".into(),
            }],
        }));
        boundary.push(Boundary::Abort);
        let registry = ToolRegistry::build(vec![]).unwrap();
        let limits = RunLimits::new(5);

        let mut ctx =
            make_ctx(&model, &registry, &mut session, &mut events, limits).with_boundary(&boundary);
        let result = BasicLoop
            .run_turn(AgentInput::text("go"), &mut ctx)
            .await
            .unwrap();

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        // 用户消息 + 先被消费的 steering
        assert_eq!(session.messages().len(), 2, "Steer 先于 Abort 被消费");
        assert!(
            session
                .messages()
                .iter()
                .any(|m| content_has_text(m, "steer!")),
            "排队在 Abort 之前的 Steer 不得被丢弃"
        );
    }

    // ============ ADR-0017：Step 4b 自动压缩的接线（片 1） ============

    /// 预置 6 条各 ~100 token 的消息 + 本轮用户消息 = 601 token；
    /// window = 1000 ⇒ `trigger_at = 101`、`keep_budget = 100` ⇒ 必然触发压缩。
    async fn seed_compaction_session(session: &mut MemorySession) {
        for i in 0..6 {
            session
                .append(Message {
                    role: if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    content: vec![ContentBlock::Text {
                        text: "x".repeat(400),
                    }],
                })
                .await
                .unwrap();
        }
    }

    fn small_window_limits() -> RunLimits {
        let mut limits = RunLimits::new(5);
        limits.context_window = 1000;
        limits
    }

    /// 自动压缩成功：`ContextCompacted` **排在 `RunFinished` 之前**，会话含摘要而非被清空。
    ///
    /// mock 的响应顺序即调用顺序：第 1 条被**摘要调用**消费，第 2 条是主对话。
    #[tokio::test]
    async fn test_auto_compaction_emits_context_compacted_before_run_finished() {
        let model = MockModel::new("m");
        model.push_text("摘要正文"); // 第 1 次 complete：摘要调用
        model.push_text("done"); // 第 2 次 complete：主对话
        let mut session = MemorySession::new();
        seed_compaction_session(&mut session).await;
        let mut events = CollectingSink::new();
        let registry = ToolRegistry::build(vec![]).unwrap();

        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                small_window_limits(),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };
        assert_eq!(result.stop_reason, StopReason::Completed);

        let evts = events.events();
        let compacted_at = evts
            .iter()
            .position(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
            .expect("小窗口下必须发出 ContextCompacted");
        let finished_at = evts
            .iter()
            .position(|e| matches!(e, AgentEvent::RunFinished { .. }))
            .expect("必须有终局事件");
        assert!(
            compacted_at < finished_at,
            "ContextCompacted 必须排在 RunFinished 之前：{evts:?}"
        );

        // 事件里的数字必须自洽：before = 6 条预置 + 本轮用户消息（"go" ≈ 0 token）
        match &evts[compacted_at] {
            AgentEvent::ContextCompacted {
                estimated_tokens_before,
                estimated_tokens_after,
                summarized,
                kept,
            } => {
                assert_eq!(*estimated_tokens_before, 600, "前置：6 × 100 token");
                assert!(
                    estimated_tokens_after < estimated_tokens_before,
                    "压缩后必须变小：{estimated_tokens_before} → {estimated_tokens_after}"
                );
                // 尾扫留得下最后两条（"go" 0 token + 一条 100 token 的），其余 5 条进摘要段
                assert_eq!(*summarized, 5);
                assert_eq!(*kept, 2);
            }
            other => panic!("unreachable: {other:?}"),
        }

        // 会话**含摘要**而非被清空（旧实现失败时会退化成一行占位符）
        assert_eq!(
            summary_messages(&session).len(),
            1,
            "压缩后必须恰好留下 1 条摘要：{:?}",
            session.messages()
        );
        assert_eq!(
            session.messages().len(),
            4,
            "摘要 1 条 + 保留段 2 条 + 本轮的 assistant 回复（run_turn 随后 append）"
        );
    }

    /// 自动压缩失败（B-3）：发 `ContextCompactionFailed`、**session 逐字节不动**、本轮照常继续。
    ///
    /// mock 顺序：第 1 条错误响应被摘要调用吃掉，第 2 条供主对话使用 ——
    /// 若失败路径吞掉了 session 或改写了它，下面的断言立刻变红。
    #[tokio::test]
    async fn test_auto_compaction_failure_leaves_session_untouched() {
        let model = MockModel::new("m");
        model.push_error("provider 挂了"); // 第 1 次 complete：摘要调用失败
        model.push_text("done"); // 第 2 次 complete：主对话照常
        let mut session = MemorySession::new();
        seed_compaction_session(&mut session).await;
        let mut events = CollectingSink::new();
        let registry = ToolRegistry::build(vec![]).unwrap();

        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                small_window_limits(),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };
        assert_eq!(
            result.stop_reason,
            StopReason::Completed,
            "压缩失败不中止本轮"
        );

        let evts = events.events();
        assert!(
            evts.iter().any(|e| matches!(
                e,
                AgentEvent::ContextCompactionFailed { reason } if reason.contains("provider 挂了")
            )),
            "失败必须可见：{evts:?}"
        );
        assert!(
            !evts
                .iter()
                .any(|e| matches!(e, AgentEvent::ContextCompacted { .. })),
            "失败不得发成功事件"
        );

        // session 逐字节未变：6 条预置 + 本轮用户消息 + 本轮的 assistant 回复，
        // 且**没有**任何摘要 / 占位符。
        assert_eq!(
            session.messages().len(),
            8,
            "失败时 session 一个字节都不动（6 预置 + 用户消息 + 本轮回复）"
        );
        assert!(
            summary_messages(&session).is_empty(),
            "失败时不得写入占位符或空摘要：{:?}",
            session.messages()
        );
    }
}
