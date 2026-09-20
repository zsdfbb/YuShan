use std::collections::HashMap;
use std::time::Duration;

use super::{AgentInput, AgentLoop, LoopError, ModelSummarizer, RunResult};
use crate::compact;
use async_trait::async_trait;
use ys_component::RuntimeContext;
use ys_core::{ContentBlock, Message, Role, StopReason, ToolResult as CoreToolResult, Usage};
use ys_event::{AgentEvent, emit};
use ys_model::{ModelEvent, ModelEventSink, ModelRequest};
use ys_protocol::Boundary;
use ys_tool::{ToolContext, ToolError};

const MAX_CONSECUTIVE_ERRORS: u32 = 3;

pub struct BasicLoop;

/// Forwarder 桥接 ModelEvent → AgentEvent，转发到 EventSink。
struct Forwarder<'a> {
    sink: &'a mut dyn ys_event::EventSink,
}

impl<'a> ModelEventSink for Forwarder<'a> {
    fn emit(&mut self, event: ModelEvent) -> Result<(), ys_core::EventError> {
        match event {
            // 同步回调里只走快路径（满 / 关闭都由 sink 内部处置，此处不做背压）
            ModelEvent::TextDelta { text } => {
                let _ = self.sink.try_emit(AgentEvent::ModelTextDelta { text });
                Ok(())
            }
            ModelEvent::ThinkingDelta { text } => {
                let _ = self.sink.try_emit(AgentEvent::ModelThinkingDelta { text });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl AgentLoop for BasicLoop {
    async fn run_turn(
        &self,
        input: AgentInput,
        ctx: &mut RuntimeContext<'_>,
    ) -> Result<RunResult, LoopError> {
        let mut rounds: u32 = 0;
        let mut total_usage = Usage::default();
        let mut consecutive_errors: HashMap<String, u32> = HashMap::new();

        // Step 1：入口边界 —— 检查 abort（非破坏性探针）
        if ctx.boundary.is_some_and(|b| b.is_aborted()) {
            emit(
                ctx.events,
                AgentEvent::RunFinished {
                    stop_reason: StopReason::Cancelled,
                    usage: Usage::default(),
                    rounds: 0,
                },
            )
            .await
            .map_err(LoopError::Event)?;
            return Ok(RunResult {
                stop_reason: StopReason::Cancelled,
                usage: Usage::default(),
                rounds: 0,
                final_message: None,
            });
        }

        // Step 2：追加用户消息，发出 UserMessage
        let user_message = input.message.clone();
        ctx.session
            .append(input.message)
            .await
            .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
        emit(
            ctx.events,
            AgentEvent::UserMessage {
                message: user_message,
            },
        )
        .await
        .map_err(LoopError::Event)?;

        // 主循环
        loop {
            // Step 3：调用 model 前检查 abort
            if ctx.boundary.is_some_and(|b| b.is_aborted()) {
                emit(
                    ctx.events,
                    AgentEvent::RunFinished {
                        stop_reason: StopReason::Cancelled,
                        usage: total_usage.clone(),
                        rounds,
                    },
                )
                .await
                .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason: StopReason::Cancelled,
                    usage: total_usage,
                    rounds,
                    final_message: None,
                });
            }

            // Step 4：在调用 model 之前检查最大 round 数
            if rounds >= ctx.limits.max_rounds {
                emit(
                    ctx.events,
                    AgentEvent::RunFinished {
                        stop_reason: StopReason::MaxRounds,
                        usage: total_usage.clone(),
                        rounds,
                    },
                )
                .await
                .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason: StopReason::MaxRounds,
                    usage: total_usage,
                    rounds,
                    final_message: None,
                });
            }

            // Step 4b：上下文压缩检查（T11e）—— 算法本体在 `crate::compact`（ADR-0017 决策 1）。
            //
            // ⚠️ **承重耦合（ADR-0017 决策 1）**：`plan()` 是**纯同步**函数、不碰 model，
            // 且下面的早退必须留在 Step 6 的 `ctx.model.complete` **之前**（Step 4b 本就在
            // 其前，改动时别把它挪到 model 调用之后）。`apps/coding-agent/src/app_loop.rs`
            // 的既有测试 `every_request_variant_is_handled` 会走到这里，它安全**只因为**
            // 会话太小 ⇒ 不满足触发阈值 / `plan()` 返回 `None` ⇒ `MockModel` 的响应队列
            // 不被消费（空队列 `remove(0)` 会 panic）。
            {
                let policy = &ctx.limits.compaction;
                let window = ctx.limits.context_window;
                // 每回合固定开销（system prompt + 工具表）：每次请求都要重发，会话里
                // 数不到，但真实占着窗口。判定必须把它算进去（U1），否则压缩系统性晚触发。
                let fixed = ctx.limits.fixed_prompt_tokens;
                let messages = ctx.session.messages();
                let estimated = crate::token::estimate_session_tokens(messages);
                let trigger = policy.trigger_at(window);

                // ⚠️ R5 边界：`fixed >= trigger` ⇒ **直接跳过压缩**。
                //
                // 固定开销永远压不掉（压缩只缩会话消息），故此时硬压会同时踩两个坑：
                //   ① 每轮空转 —— 消息已被 `plan()` 的 B-1 压到底（保留段非空），
                //      但 `estimated + fixed` 恒在阈值之上 ⇒ 每轮都压、每轮都不解渴；
                //   ② 小窗口下更糟 —— 压到最简后请求照样溢出，白烧一次摘要调用。
                //
                // 位置约束（承重，ADR-0017 决策 1）：本早退与下面的 `plan()` 都必须留在
                // Step 6 的 `ctx.model.complete` **之前**。`apps/coding-agent/src/app_loop.rs`
                // 的 `every_request_variant_is_handled` 会走到这里，它安全**只因为**
                // 不触发 model 调用（`MockModel` 空队列 `remove(0)` 会 panic）。
                if fixed < trigger && estimated + fixed > trigger {
                    if let Some(plan) = compact::plan(messages, policy, window) {
                        match compact::execute(messages, &plan, ctx.model, policy).await {
                            Ok(outcome) => {
                                // 写回由**调用方**做（`execute` 不持有 session，评审 R3）。
                                ctx.session
                                    .replace_all(outcome.messages)
                                    .await
                                    .map_err(|_| {
                                        LoopError::Event(ys_core::EventError::SendFailed)
                                    })?;
                                emit(
                                    ctx.events,
                                    AgentEvent::ContextCompacted {
                                        estimated_tokens_before: outcome.estimated_tokens_before,
                                        estimated_tokens_after: outcome.estimated_tokens_after,
                                        summarized: outcome.summarized,
                                        kept: outcome.kept,
                                    },
                                )
                                .await
                                .map_err(LoopError::Event)?;
                            }
                            Err(e) => {
                                // B-3：失败 ⇒ **session 一个字节都不动**，本轮照常继续。
                                // 旧实现会 `clear()` + 写一行占位符 —— 那是静默失忆，不是降级。
                                emit(
                                    ctx.events,
                                    AgentEvent::ContextCompactionFailed {
                                        reason: e.to_string(),
                                    },
                                )
                                .await
                                .map_err(LoopError::Event)?;
                            }
                        }
                    }
                }
            }

            // 轮边界：拉取中途插话（steering）与中止（abort），保持入队顺序。
            // 不增 rounds、不改 turn —— Steer 是轮内的额外输入，不是新一轮。
            // Abort 命中即以 Cancelled 收场（先于本轮的 model 调用）。
            //
            // 注意：这里的 `Boundary::Abort` 是**压缩窗口的防御路径**。正常时序下
            // Abort 入队即置 `is_aborted` 标记，上面的 Step 3 探针（以及轮内的工具
            // 轮询）总是先行命中并提前返回，本分支几乎不可达；唯一窗口是 Step 4b
            // 上下文压缩 `await` 期间 Abort 才到达 —— 探针已过、消息刚到。保留它是
            // 为了在压缩耗时较长时不至于漏掉一次中止（宁可晚一轮，不可丢失）。
            if let Some(boundary) = ctx.boundary {
                while let Some(bnd) = boundary.take() {
                    match bnd {
                        Boundary::Steer(msg) => {
                            ctx.session
                                .append(msg.clone())
                                .await
                                .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                            emit(ctx.events, AgentEvent::UserMessage { message: msg })
                                .await
                                .map_err(LoopError::Event)?;
                        }
                        Boundary::Abort => {
                            emit(
                                ctx.events,
                                AgentEvent::RunFinished {
                                    stop_reason: StopReason::Cancelled,
                                    usage: total_usage.clone(),
                                    rounds,
                                },
                            )
                            .await
                            .map_err(LoopError::Event)?;
                            return Ok(RunResult {
                                stop_reason: StopReason::Cancelled,
                                usage: total_usage,
                                rounds,
                                final_message: None,
                            });
                        }
                    }
                }
            }

            // Step 5：组装 ModelRequest（ToolSpec 已在构建时缓存）
            rounds += 1;
            let tools = ctx.registry.specs().to_vec();
            let request = ModelRequest {
                messages: ctx.session.messages().to_vec(),
                tools,
                system: ctx.system_prompt.clone(),
                ..Default::default()
            };

            // Step 6：通过 Forwarder 调用 model
            let mut forwarder = Forwarder { sink: ctx.events };
            let model_result = ctx.model.complete(request, &mut forwarder).await;
            let response = match model_result {
                Ok(response) => response,
                Err(e) => {
                    // 返回错误前发出 RunFailed（终止事件不变量）
                    // （同步闭包内无法 await，故把 emit 提到闭包外）
                    let _ = emit(
                        ctx.events,
                        AgentEvent::RunFailed {
                            error: e.to_string(),
                        },
                    )
                    .await;
                    return Err(LoopError::Model(e));
                }
            };

            // Step 7：追加 assistant 消息（调用中取消 -> 仍要追加）
            let assistant_message = response.message.clone();
            total_usage = total_usage + response.usage;
            ctx.session
                .append(assistant_message.clone())
                .await
                .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;

            // 提取文本内容作为 final_message
            let mut text_content = String::new();
            for block in &assistant_message.content {
                if let ContentBlock::Text { text } = block {
                    text_content.push_str(text);
                }
            }

            // Step 8：检查 tool call
            let tool_calls: Vec<_> = assistant_message
                .content
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::ToolUse {
                        id,
                        name,
                        arguments,
                    } = block
                    {
                        Some((id.clone(), name.clone(), arguments.clone()))
                    } else {
                        None
                    }
                })
                .collect();

            if tool_calls.is_empty() {
                // 无 tool call -> 完成
                let final_msg = if text_content.is_empty() {
                    None
                } else {
                    Some(assistant_message)
                };
                emit(
                    ctx.events,
                    AgentEvent::RunFinished {
                        stop_reason: StopReason::Completed,
                        usage: total_usage.clone(),
                        rounds,
                    },
                )
                .await
                .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason: StopReason::Completed,
                    usage: total_usage,
                    rounds,
                    final_message: final_msg,
                });
            }

            // Step 9：串行执行工具调用（v0）
            let mut tool_results_for_message: Vec<ContentBlock> = Vec::new();

            for (tool_call_id, tool_name, tool_args) in &tool_calls {
                // T11a：审批检查
                if let Some(approval) = &ctx.approval {
                    if approval.needs_approval(tool_name, tool_args) {
                        match approval.request_approval(tool_name, tool_args).await {
                            ys_tool::ApprovalDecision::Approved => {}
                            ys_tool::ApprovalDecision::Denied { reason } => {
                                tool_results_for_message.push(ContentBlock::ToolResult {
                                    tool_call_id: tool_call_id.clone(),
                                    content: format!("operation denied: {reason}"),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }
                }

                // 发出 ToolCall 事件
                emit(
                    ctx.events,
                    AgentEvent::ToolCall {
                        call: ys_core::ToolCall {
                            id: tool_call_id.clone(),
                            name: tool_name.clone(),
                            arguments: tool_args.clone(),
                        },
                    },
                )
                .await
                .map_err(LoopError::Event)?;

                // 在 registry 中查找并带超时执行（T11b）与错误恢复（T11c）
                let result = match ctx.registry.get(tool_name) {
                    Some(tool) => {
                        // 唯一的摘要适配器接线点：`ctx.model` / `ctx.registry` / `ctx.boundary`
                        // 全是不可变借用，可共存；`summarizer` 必须先于 `tool_ctx` 声明。
                        let summarizer = ModelSummarizer { model: ctx.model };
                        let tool_ctx = ToolContext::new(
                            ctx.boundary,
                            ctx.cwd.clone(),
                            ctx.workspace_root.clone(),
                        )
                        .with_summarizer(&summarizer);
                        let timeout_secs = ctx.limits.bash_timeout.unwrap_or(300);
                        match tokio::time::timeout(
                            Duration::from_secs(timeout_secs),
                            tool.call(tool_args.clone(), tool_ctx),
                        )
                        .await
                        {
                            Ok(Ok(r)) => r,
                            Ok(Err(ToolError::Execution(msg))) => CoreToolResult {
                                content: msg,
                                is_error: true,
                            },
                            Ok(Err(ToolError::InvalidInput(msg))) => CoreToolResult {
                                content: format!("Invalid input: {msg}"),
                                is_error: true,
                            },
                            Ok(Err(ToolError::PermissionDenied(msg))) => CoreToolResult {
                                content: format!("Permission denied: {msg}"),
                                is_error: true,
                            },
                            Ok(Err(ToolError::Timeout(msg))) => CoreToolResult {
                                content: format!("Tool timed out: {msg}"),
                                is_error: true,
                            },
                            Ok(Err(e)) => CoreToolResult {
                                content: e.to_string(),
                                is_error: true,
                            },
                            Err(_elapsed) => CoreToolResult {
                                content: "Command timed out".into(),
                                is_error: true,
                            },
                        }
                    }
                    None => {
                        // registry 未命中 -> 合成 is_error 结果（ADR-0001）
                        CoreToolResult {
                            content: format!("tool not found: {tool_name}"),
                            is_error: true,
                        }
                    }
                };

                // T11c：连续错误跟踪
                if result.is_error {
                    let count = consecutive_errors.entry(tool_name.clone()).or_insert(0);
                    *count += 1;
                    if *count >= MAX_CONSECUTIVE_ERRORS {
                        // 继续循环，但错误结果会回喂给模型
                        // 模型应看到反复出现的错误并停止调用该工具
                    }
                } else {
                    consecutive_errors.remove(tool_name);
                }

                // 发出 ToolResult 事件
                emit(
                    ctx.events,
                    AgentEvent::ToolResult {
                        id: tool_call_id.clone(),
                        result: result.clone(),
                    },
                )
                .await
                .map_err(LoopError::Event)?;

                // 准备待追加的 content block
                tool_results_for_message.push(ContentBlock::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    content: result.content,
                    is_error: result.is_error,
                });
            }

            // 将工具结果作为单条消息追加并继续循环
            let tool_result_message = Message {
                role: Role::User,
                content: tool_results_for_message,
            };
            ctx.session
                .append(tool_result_message)
                .await
                .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use ys_component::RunLimits;
    use ys_event::{AgentEvent, CollectingSink, EventSink};
    use ys_model::{MockModel, Model, ModelError, ModelResponse};
    use ys_session::{MemorySession, Session};
    use ys_tool::ToolRegistry;

    /// A3：`ModelEvent::ThinkingDelta` 经 `Forwarder` 冒泡为
    /// `AgentEvent::ModelThinkingDelta`；`TextDelta` 路径不变。
    #[test]
    fn forwarder_maps_thinking_delta_to_agent_event() {
        let mut sink = CollectingSink::new();
        {
            let mut forwarder = Forwarder { sink: &mut sink };
            forwarder
                .emit(ModelEvent::ThinkingDelta { text: "hmm".into() })
                .unwrap();
            forwarder
                .emit(ModelEvent::TextDelta { text: "hi".into() })
                .unwrap();
        }
        assert_eq!(sink.events().len(), 2);
        assert!(
            matches!(&sink.events()[0], AgentEvent::ModelThinkingDelta { text } if text == "hmm")
        );
        assert!(matches!(&sink.events()[1], AgentEvent::ModelTextDelta { text } if text == "hi"));
    }

    /// 向后兼容：不产 `ThinkingDelta` 的 model（如 `MockModel`）行为不变，
    /// 不会凭空产生 `ModelThinkingDelta`。
    #[test]
    fn forwarder_backward_compatible_without_thinking_delta() {
        let mut sink = CollectingSink::new();
        {
            let mut forwarder = Forwarder { sink: &mut sink };
            forwarder
                .emit(ModelEvent::TextDelta { text: "a".into() })
                .unwrap();
            forwarder
                .emit(ModelEvent::TextDelta { text: "b".into() })
                .unwrap();
        }
        assert_eq!(sink.events().len(), 2);
        assert!(
            sink.events()
                .iter()
                .all(|e| matches!(e, AgentEvent::ModelTextDelta { .. })),
            "events = {:?}",
            sink.events()
        );
    }

    // ==================== U1：压缩触发必须计入固定开销 ====================
    //
    // 背景：Step 4b 的判定只数 session 消息（`estimate_session_tokens`），看不见每次
    // 请求都要重发的 system prompt 与工具表。9 工具时缺口约 1.1k（可忽略）；skills 上线
    // 后 skill 目录常驻 ~9.6k ⇒ **压缩系统性晚触发**。`RunLimits::fixed_prompt_tokens`
    // 就是这块固定开销的入口，默认 0（⇒ 老行为逐字节不变）。

    fn make_ctx<'a>(
        model: &'a dyn Model,
        registry: &'a ToolRegistry,
        session: &'a mut dyn Session,
        events: &'a mut dyn EventSink,
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

    /// `window = 1000` + 默认策略 ⇒ `trigger_at = 101`（`keep_budget = 100`）。
    fn limits_with_window(window: usize, fixed: usize) -> RunLimits {
        let mut limits = RunLimits::new(5);
        limits.context_window = window;
        limits.fixed_prompt_tokens = fixed;
        limits
    }

    /// 预置 6 条各 10 token 的交替消息（估算合计 60）。
    /// 本轮用户消息 "go" ≈ 0 token ⇒ 判定时的 `estimated` = 60，**不达**触发点 101。
    async fn seed_small_session(session: &mut MemorySession) {
        for i in 0..6 {
            session
                .append(Message {
                    role: if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    content: vec![ContentBlock::Text {
                        text: "x".repeat(40),
                    }],
                })
                .await
                .unwrap();
        }
    }

    /// 预置 6 条各 100 token 的消息（估算合计 600，远超触发点 101）。
    async fn seed_large_session(session: &mut MemorySession) {
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

    fn has_compacted(events: &CollectingSink) -> bool {
        events
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
    }

    /// 会话里是否留下压缩摘要（`[Context Summary]` 标记）。
    fn has_summary(session: &MemorySession) -> bool {
        session.messages().iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("[Context Summary]")),
            )
        })
    }

    /// U1-1：**触发点被固定开销推前** —— 同样的消息量，`fixed > 0` 触发、`fixed = 0` 不触发。
    ///
    /// 会话估算 60 token、`window = 1000 ⇒ trigger_at = 101`：
    /// - `fixed = 0`  ⇒ 60 ≤ 101 ⇒ **不压**：`MockModel` 只被消费 1 条（主对话）；
    /// - `fixed = 50` ⇒ 110 > 101 ⇒ **压**：第 1 条响应被摘要调用吃掉，第 2 条才是主对话。
    ///
    /// 走的是**真实 `BasicLoop::run_turn`**（Step 4b），不是重算一遍算术 ——
    /// `MockModel` 的响应队列按调用顺序被消费，压缩与否由此**直接可见**。
    ///
    /// 变异自检（已实测）：把 `basic.rs` 的 `estimated + fixed` 改回 `estimated`
    /// ⇒ 分支 B 不触发压缩、只消费 1 条响应、不发 `ContextCompacted` ⇒ 本用例变红。
    #[tokio::test]
    async fn fixed_prompt_tokens_shifts_compaction_trigger() {
        let registry = ToolRegistry::build(vec![]).unwrap();

        // 分支 A：fixed = 0 ⇒ 60 ≤ 101 ⇒ 不压缩。
        let model = MockModel::new("m");
        model.push_text("done"); // 唯一一次调用：主对话
        let mut session = MemorySession::new();
        seed_small_session(&mut session).await;
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                limits_with_window(1000, 0),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert!(
            !has_compacted(&events),
            "fixed = 0 时 60 ≤ 101 ⇒ 不得压缩：{:?}",
            events.events()
        );
        assert!(!has_summary(&session), "未压缩则不得写入摘要");

        // 分支 B：fixed = 50 ⇒ 60 + 50 = 110 > 101 ⇒ 触发压缩。
        let model = MockModel::new("m");
        model.push_text("SUMMARY"); // 第 1 次调用：摘要
        model.push_text("done"); // 第 2 次调用：主对话
        let mut session = MemorySession::new();
        seed_small_session(&mut session).await;
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                limits_with_window(1000, 50),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert!(
            has_compacted(&events),
            "fixed = 50 时 110 > 101 ⇒ 必须压缩：{:?}",
            events.events()
        );
        assert!(has_summary(&session), "压缩后会话里应留下摘要");
    }

    /// 每次都计数、恒返回纯文本的 model —— 用来断言「模型一共被调用了几次」。
    /// （比 `MockModel` 的空队列 `remove(0)` panic 给出更直白的失败信号。）
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
                stop_reason: None,
            })
        }
    }

    /// U1-2（R5 边界）：`fixed >= trigger_at(window)` ⇒ **不进入压缩**（不空转、不 panic）。
    ///
    /// 固定开销永远压不掉：硬压的话每轮空转（消息已被 `plan()` 的 B-1 压到底，但
    /// `estimated + fixed` 恒在阈值之上），小窗口下更会「压了还溢出」白烧一次摘要调用。
    ///
    /// 会话养到 600 token —— 只要走到 `plan()` 就一定压得动（`len ≥ 2`、两段非空、
    /// 保留段首条是 Assistant 文本），**唯一**拦下它的是 R5 早退。断言模型只被调用
    /// **1 次**：若压缩发生则是 2 次（摘要 + 主对话），且次数可证早退仍在 Step 6 的
    /// `model.complete` **之前**。
    ///
    /// 变异自检（已实测）：删掉 `fixed < trigger` 这个条件（只留 `estimated + fixed > trigger`）
    /// ⇒ 模型被调用 2 次、发 `ContextCompacted` ⇒ 本用例变红。
    #[tokio::test]
    async fn fixed_prompt_tokens_at_or_above_trigger_skips_compaction() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let window = 1000;
        let trigger = RunLimits::default().compaction.trigger_at(window);
        assert_eq!(
            trigger, 101,
            "前置：默认策略下 window = 1000 的触发点是 101"
        );

        // 边界两侧：`fixed == trigger`（下界）+ 远超触发点的两个点。
        for fixed in [trigger, trigger + 1, 10_000] {
            let model = CountingModel {
                calls: Mutex::new(0),
            };
            let mut session = MemorySession::new();
            seed_large_session(&mut session).await;
            let mut events = CollectingSink::new();
            let result = {
                let mut ctx = make_ctx(
                    &model,
                    &registry,
                    &mut session,
                    &mut events,
                    limits_with_window(window, fixed),
                );
                BasicLoop
                    .run_turn(AgentInput::text("go"), &mut ctx)
                    .await
                    .unwrap()
            };

            assert_eq!(
                result.stop_reason,
                StopReason::Completed,
                "fixed = {fixed}：本轮照常收场"
            );
            assert_eq!(
                *model.calls.lock().unwrap(),
                1,
                "fixed = {fixed} ≥ trigger = {trigger} ⇒ 必须跳过压缩（只该有主对话那一次调用）"
            );
            assert!(
                !has_compacted(&events),
                "fixed = {fixed} ⇒ 不得发 ContextCompacted：{:?}",
                events.events()
            );
            assert!(!has_summary(&session), "fixed = {fixed} ⇒ 不得写入摘要");
            // 会话逐字节未动：6 条预置 + 本轮用户消息 + 本轮 assistant 回复。
            assert_eq!(
                session.messages().len(),
                8,
                "fixed = {fixed}：压缩被跳过 ⇒ 会话形状不变"
            );
        }
    }
}
