use std::collections::HashMap;
use std::time::Duration;

use super::{AgentInput, AgentLoop, LoopError, RunResult};
use agent_component::RuntimeContext;
use agent_core::{ContentBlock, Message, Role, StopReason, ToolResult as CoreToolResult, Usage};
use agent_event::AgentEvent;
use agent_model::{ModelEvent, ModelEventSink, ModelRequest};
use agent_tool::{ToolContext, ToolError};
use async_trait::async_trait;

const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// Token budget reserved for summary when compressing context
const COMPACT_KEEP_TOKENS: usize = 20_000;

pub struct BasicLoop;

/// Forwarder bridges ModelEvent -> AgentEvent, forwarding to EventSink.
struct Forwarder<'a> {
    sink: &'a mut dyn agent_event::EventSink,
}

impl<'a> ModelEventSink for Forwarder<'a> {
    fn emit(&mut self, event: ModelEvent) -> Result<(), agent_core::EventError> {
        match event {
            ModelEvent::TextDelta { text } => self.sink.emit(AgentEvent::ModelTextDelta { text }),
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

        // Step 1: Entry boundary -- check cancel
        if ctx.cancel.is_cancelled() {
            ctx.events
                .emit(AgentEvent::RunFinished {
                    stop_reason: StopReason::Cancelled,
                    usage: Usage::default(),
                    rounds: 0,
                })
                .map_err(LoopError::Event)?;
            return Ok(RunResult {
                stop_reason: StopReason::Cancelled,
                usage: Usage::default(),
                rounds: 0,
                final_message: None,
            });
        }

        // Step 2: Append user message, emit UserMessage
        let user_message = input.message.clone();
        ctx.session
            .append(input.message)
            .await
            .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;
        ctx.events
            .emit(AgentEvent::UserMessage {
                message: user_message,
            })
            .map_err(LoopError::Event)?;

        // Main loop
        loop {
            // Step 3: Check cancel before model call
            if ctx.cancel.is_cancelled() {
                ctx.events
                    .emit(AgentEvent::RunFinished {
                        stop_reason: StopReason::Cancelled,
                        usage: total_usage.clone(),
                        rounds,
                    })
                    .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason: StopReason::Cancelled,
                    usage: total_usage,
                    rounds,
                    final_message: None,
                });
            }

            // Step 4: Check max rounds BEFORE model call
            if rounds >= ctx.limits.max_rounds {
                ctx.events
                    .emit(AgentEvent::RunFinished {
                        stop_reason: StopReason::MaxRounds,
                        usage: total_usage.clone(),
                        rounds,
                    })
                    .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason: StopReason::MaxRounds,
                    usage: total_usage,
                    rounds,
                    final_message: None,
                });
            }

            // Step 4b: Context compression check (T11e)
            {
                let messages = ctx.session.messages();
                let total_tokens = crate::token::estimate_session_tokens(messages);
                if total_tokens > ctx.limits.context_window.saturating_sub(16384) {
                    compact_session(ctx).await?;
                }
            }

            // Step 5: Assemble ModelRequest (ToolSpec already cached at build time)
            rounds += 1;
            let tools = ctx.registry.specs().to_vec();
            let request = ModelRequest {
                messages: ctx.session.messages().to_vec(),
                tools,
                system: ctx.system_prompt.clone(),
                ..Default::default()
            };

            // Step 6: Call model with Forwarder
            let mut forwarder = Forwarder { sink: ctx.events };
            let response = ctx
                .model
                .complete(request, &mut forwarder)
                .await
                .map_err(|e| {
                    // Emit RunFailed before returning error (terminal event invariant)
                    let _ = ctx.events.emit(AgentEvent::RunFailed {
                        error: e.to_string(),
                    });
                    LoopError::Model(e)
                })?;

            // Step 7: Append assistant message (cancel during call -> still append)
            let assistant_message = response.message.clone();
            total_usage = total_usage + response.usage;
            ctx.session
                .append(assistant_message.clone())
                .await
                .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;

            // Extract text content for final_message
            let mut text_content = String::new();
            for block in &assistant_message.content {
                if let ContentBlock::Text { text } = block {
                    text_content.push_str(text);
                }
            }

            // Step 8: Check for tool calls
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
                // No tool calls -> completed
                let final_msg = if text_content.is_empty() {
                    None
                } else {
                    Some(assistant_message)
                };
                ctx.events
                    .emit(AgentEvent::RunFinished {
                        stop_reason: StopReason::Completed,
                        usage: total_usage.clone(),
                        rounds,
                    })
                    .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason: StopReason::Completed,
                    usage: total_usage,
                    rounds,
                    final_message: final_msg,
                });
            }

            // Step 9: Execute tool calls serially (v0)
            let mut tool_results_for_message: Vec<ContentBlock> = Vec::new();

            for (tool_call_id, tool_name, tool_args) in &tool_calls {
                // T11a: Approval check
                if let Some(approval) = &ctx.approval {
                    if approval.needs_approval(tool_name, tool_args) {
                        match approval.request_approval(tool_name, tool_args).await {
                            agent_tool::ApprovalDecision::Approved => {}
                            agent_tool::ApprovalDecision::Denied { reason } => {
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

                // Emit ToolCall event
                ctx.events
                    .emit(AgentEvent::ToolCall {
                        call: agent_core::ToolCall {
                            id: tool_call_id.clone(),
                            name: tool_name.clone(),
                            arguments: tool_args.clone(),
                        },
                    })
                    .map_err(LoopError::Event)?;

                // Lookup in registry and execute with timeout (T11b) and error recovery (T11c)
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
                        // Registry miss -> synthesize is_error result (ADR-0001)
                        CoreToolResult {
                            content: format!("tool not found: {tool_name}"),
                            is_error: true,
                        }
                    }
                };

                // T11c: Consecutive error tracking
                if result.is_error {
                    let count = consecutive_errors.entry(tool_name.clone()).or_insert(0);
                    *count += 1;
                    if *count >= MAX_CONSECUTIVE_ERRORS {
                        // Continue loop but the error result will be fed to model
                        // The model should see the repeated errors and stop calling the tool
                    }
                } else {
                    consecutive_errors.remove(tool_name);
                }

                // Emit ToolResult event
                ctx.events
                    .emit(AgentEvent::ToolResult {
                        id: tool_call_id.clone(),
                        result: result.clone(),
                    })
                    .map_err(LoopError::Event)?;

                // Prepare content block for appending
                tool_results_for_message.push(ContentBlock::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    content: result.content,
                    is_error: result.is_error,
                });
            }

            // Append tool results as a single message and continue loop
            let tool_result_message = Message {
                role: Role::User,
                content: tool_results_for_message,
            };
            ctx.session
                .append(tool_result_message)
                .await
                .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;
        }
    }
}

/// Compact session by summarizing old messages when context window is near limit.
async fn compact_session(ctx: &mut RuntimeContext<'_>) -> Result<(), LoopError> {
    let messages: Vec<Message> = ctx.session.messages().to_vec();

    // Find the keep point: scan from newest, keep ~COMPACT_KEEP_TOKENS tokens
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

    // Nothing to compress if keep_from is 0 or 1
    if keep_from <= 1 {
        return Ok(());
    }

    let to_summarize = &messages[..keep_from];
    let to_keep = &messages[keep_from..];

    // Generate summary using the model
    let summary = generate_summary(ctx.model, to_summarize).await;

    // Rebuild session: summary message + kept messages
    ctx.session
        .clear()
        .await
        .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;

    // Write summary as system context
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
        .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;

    // Keep a synthetic assistant acknowledgment
    ctx.session
        .append(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "I have the context summary. Continuing.".into(),
            }],
        })
        .await
        .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;

    // Re-append the kept messages
    for msg in to_keep {
        ctx.session
            .append(msg.clone())
            .await
            .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;
    }

    Ok(())
}

/// Generate a summary of old messages using the model.
async fn generate_summary(
    model: &dyn agent_model::Model,
    messages: &[Message],
) -> Result<String, agent_model::ModelError> {
    struct NoopModelEventSink;
    impl ModelEventSink for NoopModelEventSink {
        fn emit(&mut self, _event: ModelEvent) -> Result<(), agent_core::EventError> {
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
    // Extract text from response
    for block in &response.message.content {
        if let ContentBlock::Text { text } = block {
            return Ok(text.clone());
        }
    }
    Ok(String::new())
}
