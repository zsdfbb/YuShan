use std::collections::HashMap;
use std::time::Duration;

use super::{AgentInput, AgentLoop, LoopError, RunResult};
use async_trait::async_trait;
use ys_component::RuntimeContext;
use ys_core::{ContentBlock, Message, Role, StopReason, ToolResult as CoreToolResult, Usage};
use ys_event::{AgentEvent, emit};
use ys_model::{ModelEvent, ModelEventSink, ModelRequest};
use ys_tool::{ToolContext, ToolError};

const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// 压缩上下文时为 summary 预留的 token 预算
const COMPACT_KEEP_TOKENS: usize = 20_000;

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

        // Step 1：入口边界 —— 检查 cancel
        if ctx.cancel.is_cancelled() {
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
            // Step 3：调用 model 前检查 cancel
            if ctx.cancel.is_cancelled() {
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

            // Step 4b：上下文压缩检查（T11e）
            {
                let messages = ctx.session.messages();
                let total_tokens = crate::token::estimate_session_tokens(messages);
                if total_tokens > ctx.limits.context_window.saturating_sub(16384) {
                    compact_session(ctx).await?;
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
                        let tool_ctx = ToolContext::new(
                            ctx.cancel,
                            ctx.cwd.clone(),
                            ctx.workspace_root.clone(),
                        );
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

/// 当 context window 接近上限时，通过总结旧消息来压缩 session。
async fn compact_session(ctx: &mut RuntimeContext<'_>) -> Result<(), LoopError> {
    let messages: Vec<Message> = ctx.session.messages().to_vec();

    // 计算保留点：从最新消息往回扫，保留约 COMPACT_KEEP_TOKENS 个 token
    let mut keep_from = messages.len();
    let mut kept_tokens = 0usize;
    for msg in messages.iter().rev() {
        let msg_tokens = crate::token::estimate_tokens(msg);
        if kept_tokens + msg_tokens > COMPACT_KEEP_TOKENS {
            break;
        }
        kept_tokens += msg_tokens;
        keep_from -= 1;
    }

    // keep_from 为 0 或 1 时无需压缩
    if keep_from <= 1 {
        return Ok(());
    }

    let to_summarize = &messages[..keep_from];
    let to_keep = &messages[keep_from..];

    // 使用模型生成 summary
    let summary = generate_summary(ctx.model, to_summarize).await;

    // 重建 session：summary 消息 + 保留的消息
    ctx.session
        .clear()
        .await
        .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;

    // 以 system context 形式写入 summary
    let summary_text = match summary {
        Ok(text) => format!("[Context Summary]\n{text}\n[/Context Summary]"),
        Err(_) => "[Context Summary]\n[Compression failed — proceeding with truncated history]\n[/Context Summary]".into(),
    };
    ctx.session
        .append(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: summary_text }],
        })
        .await
        .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;

    // 保留一条合成的 assistant 确认消息
    ctx.session
        .append(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "I have the context summary. Continuing.".into(),
            }],
        })
        .await
        .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;

    // 重新追加保留的消息
    for msg in to_keep {
        ctx.session
            .append(msg.clone())
            .await
            .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
    }

    Ok(())
}

/// 使用模型生成旧消息的 summary。
async fn generate_summary(
    model: &dyn ys_model::Model,
    messages: &[Message],
) -> Result<String, ys_model::ModelError> {
    struct NoopModelEventSink;
    impl ModelEventSink for NoopModelEventSink {
        fn emit(&mut self, _event: ModelEvent) -> Result<(), ys_core::EventError> {
            Ok(())
        }
    }

    let mut sink = NoopModelEventSink;
    let request = ModelRequest {
        messages: messages.to_vec(),
        tools: vec![],
        system: Some(
            "You are a conversation summarizer. Summarize the following conversation \
             concisely, preserving key decisions, facts, and context."
                .into(),
        ),
        max_tokens: Some(1024),
        ..Default::default()
    };
    let response = model.complete(request, &mut sink).await?;
    // 从响应中提取文本
    for block in &response.message.content {
        if let ContentBlock::Text { text } = block {
            return Ok(text.clone());
        }
    }
    Ok(String::new())
}
