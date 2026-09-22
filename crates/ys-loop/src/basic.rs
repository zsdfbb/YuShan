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

/// 被截断的那批工具调用的统一回喂话术。必须**模型可读、能自愈**：
/// 点明「没执行、别以为成功了」，并给出可行的下一步。
const TRUNCATED_TOOL_NOTICE: &str = "tool call was NOT executed: the model response was truncated \
     (output token limit reached). Re-issue this call with a smaller payload, or split it into \
     several calls.";

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

            // Step 8b：截断保护（ADR-0020）—— 被截断的那一批工具**一个都不执行**。
            //
            // 模型被输出预算掐断时，工具块的参数很可能是残缺的（适配器只看 name 非空就出块），
            // 执行它等于拿半个 payload 去写文件。故整批作废、合成 is_error 结果回喂，
            // 让模型下一轮自己改小步。
            if response.finish_reason.is_truncated() {
                emit(
                    ctx.events,
                    AgentEvent::ResponseTruncated {
                        dropped_tool_calls: tool_calls.len(),
                    },
                )
                .await
                .map_err(LoopError::Event)?;

                if tool_calls.is_empty() {
                    // 纯文本被截断：诚实收场，**不得**伪装成 Completed（那是静默的谎）。
                    let final_msg = if text_content.is_empty() {
                        None
                    } else {
                        Some(assistant_message)
                    };
                    emit(
                        ctx.events,
                        AgentEvent::RunFinished {
                            stop_reason: StopReason::Truncated,
                            usage: total_usage.clone(),
                            rounds,
                        },
                    )
                    .await
                    .map_err(LoopError::Event)?;
                    return Ok(RunResult {
                        stop_reason: StopReason::Truncated,
                        usage: total_usage,
                        rounds,
                        final_message: final_msg,
                    });
                }

                // 有工具调用：整批作废。每个 tool_use 都必须拿到一条结果
                // （ADR-0001 配对完整性 —— 否则下一轮请求会被真实 provider 直接拒收）。
                //
                // 注意：此处**不做审批检查** —— 不执行就不该弹审批。
                let mut dropped: Vec<ContentBlock> = Vec::with_capacity(tool_calls.len());
                for (tool_call_id, tool_name, tool_args) in &tool_calls {
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

                    let content = TRUNCATED_TOOL_NOTICE.to_string();
                    emit(
                        ctx.events,
                        AgentEvent::ToolResult {
                            id: tool_call_id.clone(),
                            result: CoreToolResult {
                                content: content.clone(),
                                is_error: true,
                            },
                        },
                    )
                    .await
                    .map_err(LoopError::Event)?;

                    dropped.push(ContentBlock::ToolResult {
                        tool_call_id: tool_call_id.clone(),
                        content,
                        is_error: true,
                    });
                }
                ctx.session
                    .append(Message {
                        role: Role::User,
                        content: dropped,
                    })
                    .await
                    .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                continue;
            }

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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use ys_component::RunLimits;
    use ys_event::{AgentEvent, CollectingSink, EventSink};
    use ys_model::{FinishReason, MockModel, Model, ModelError, ModelResponse};
    use ys_session::{MemorySession, Session};
    use ys_tool::{Tool, ToolContext, ToolRegistry, ToolSpec};

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
                finish_reason: FinishReason::Stop,
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

    // ==================== N1-N4：截断保护（ADR-0020） ====================
    //
    // 背景：provider 输出预算耗尽时给 `finish_reason: "length"`，工具块的参数很可能是
    // 残缺的（适配器只看 name 非空就出块）。旧循环把这批工具**照常执行** ⇒ 半个 payload
    // 去写文件且返回 success；纯文本截断则直接判 `Completed` + ✓（静默的谎）。
    // 新分支（Step 8b）整批作废、合成 is_error 回喂，让模型下一轮改小步。

    /// 间谍工具：只数被调用了几次，`call` 里 `fetch_add(1)` 后返回成功结果。
    ///
    /// 「计数」是最直白的证据 —— 比「结果内容长什么样」更难被巧合糊弄过去。
    ///
    /// **名字可配**（每个名字各自一份计数器）：只有让被作废的那批工具名**真的注册进
    /// registry**，计数才说明问题。若用例里 model 返回的名字没注册，guard 失效后循环会走
    /// Step 9 的「registry 未命中」路径 —— 照样合成 id 一一对应的 `is_error` 结果、装进同
    /// 一条 `Role::User` 消息 —— 于是断言全被那条错误路径满足，计数更是恒为 0 的**空断言**。
    struct SpyTool {
        name: String,
        calls: Arc<AtomicUsize>,
    }

    impl SpyTool {
        /// 返回 (名为 `spy` 的工具, 计数器)。测试持有计数器即可断言调用次数。
        fn new() -> (Self, Arc<AtomicUsize>) {
            Self::new_named("spy")
        }

        /// 返回 (指定名字的工具, 计数器) —— 名字不同则计数器**互不相干**。
        fn new_named(name: &str) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    name: name.to_string(),
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    #[async_trait::async_trait]
    impl Tool for SpyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name.as_str(), "counts invocations", serde_json::json!({}))
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CoreToolResult {
                content: "spy ok".into(),
                is_error: false,
            })
        }
    }

    /// 会话里所有 `ToolResult` block（按出现顺序）。
    fn tool_results_in(session: &MemorySession) -> Vec<&ContentBlock> {
        session
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .collect()
    }

    /// 消息里是否含**恰好等于** `text` 的文本块（子串会把 "done" 误命中别处）。
    fn content_has_text(msg: &Message, text: &str) -> bool {
        msg.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text: t } if t == text))
    }

    /// N1：**被截断的那批工具一个都不执行**，整批作废并回喂 is_error。
    ///
    /// 场景：`push_truncated_tool_call("spy")`（`finish_reason = Length`）→ `push_text("done")`。
    /// 断言链：
    ///  - SpyTool 计数 **== 0**（没被执行 —— 本改动的全部意义）；
    ///  - 会话里有一条 `is_error` 且内容含 "truncated" 的 `ToolResult`（模型看得见「没执行」）；
    ///  - 事件流里有 `ResponseTruncated { dropped_tool_calls: 1 }`；
    ///  - 模型**被调了第二次**：证据是 `push_text("done")` 那条响应的文本出现在会话里
    ///    （第二轮的产出）。若少了 `continue`，队列里那条响应永远不会被消费。
    ///
    /// 变异自检（已实测）：删掉 `response.finish_reason.is_truncated()` 这个 `if`
    /// ⇒ spy 被真执行、计数变 1 ⇒ 本用例变红。
    #[tokio::test]
    async fn truncated_tool_calls_are_not_executed() {
        let (spy, calls) = SpyTool::new();
        let registry = ToolRegistry::build(vec![Box::new(spy)]).unwrap();
        let model = MockModel::new("m");
        model.push_truncated_tool_call("spy", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let result = {
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
                .unwrap()
        };

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "被截断的工具**一个都不得执行**（否则拿半个 payload 就去写文件）"
        );

        // 事件：必须有一条 ResponseTruncated，且 dropped_tool_calls = 1
        assert!(
            events
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::ResponseTruncated { dropped_tool_calls: 1 })),
            "必须发出 ResponseTruncated{{dropped_tool_calls: 1}}：{:?}",
            events.events()
        );

        // 会话：每个被作废的 tool_use 都拿到一条 is_error 结果，话术含 "truncated"
        let results = tool_results_in(&session);
        assert_eq!(results.len(), 1, "配对完整性：1 个 tool_use ⇒ 1 条结果");
        match results[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(*is_error, "作废的结果必须是 is_error（别让模型以为成功了）");
                assert!(
                    content.contains("truncated"),
                    "回喂话术必须点明「被截断」，实际 = {content:?}"
                );
            }
            other => panic!("unreachable: {other:?}"),
        }

        // 模型被调了第二次：第二轮那条 "done" 出现在会话里（在结果消息之后）
        assert_eq!(result.stop_reason, StopReason::Completed, "第二轮照常收场");
        assert_eq!(result.rounds, 2, "第 1 轮作废 → 第 2 轮拿回控制权");
        assert!(
            session
                .messages()
                .iter()
                .any(|m| m.role == Role::Assistant && content_has_text(m, "done")),
            "第二轮的模型输出必须落地（证明 continue 让循环再调了一次 model）：{:?}",
            session.messages()
        );
    }

    /// N2：纯文本被截断 ⇒ 回合以 `StopReason::Truncated` **诚实**收场（不是 `Completed`）。
    ///
    /// 变异自检（已实测）：把分支里两处 `StopReason::Truncated` 改回 `Completed`
    /// ⇒ `result.stop_reason` 与 `RunFinished.stop_reason` 两条断言同时变红；
    /// 把 `ResponseTruncated` 的 emit 挪到 `RunFinished` **之后** ⇒ 顺序断言变红。
    #[tokio::test]
    async fn truncated_text_ends_turn_as_truncated() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = MockModel::new("m");
        model.push_truncated_text("half an answer");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let result = {
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
                .unwrap()
        };

        assert_eq!(
            result.stop_reason,
            StopReason::Truncated,
            "被截断的纯文本**不得**伪装成 Completed"
        );
        assert_eq!(result.rounds, 1);
        // 截断的文本仍作为 final_message 交出去（半句答案总比没有好，但不能标成完成）
        assert!(
            result
                .final_message
                .as_ref()
                .is_some_and(|m| content_has_text(m, "half an answer")),
            "被截断的文本应仍带出，实际 = {:?}",
            result.final_message
        );

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
        assert_eq!(terminal.len(), 1, "终局事件不变量：恰好一个");
        assert!(
            matches!(
                terminal[0],
                AgentEvent::RunFinished {
                    stop_reason: StopReason::Truncated,
                    ..
                }
            ),
            "终局事件必须是 RunFinished{{Truncated}}，实际 = {:?}",
            terminal[0]
        );
        assert!(
            events
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::ResponseTruncated { dropped_tool_calls: 0 })),
            "纯文本截断也要发 ResponseTruncated（dropped = 0）：{:?}",
            events.events()
        );

        // 顺序不变量：截断通知是**非终局**事件，必须先于**终局**事件 RunFinished。
        // `terminal.len() == 1` 已钉住 RunFinished 只有一条，故 `position` 唯一。
        let truncated_at = events
            .events()
            .iter()
            .position(|e| matches!(e, AgentEvent::ResponseTruncated { .. }))
            .expect("截断时必须发出 ResponseTruncated");
        let finished_at = events
            .events()
            .iter()
            .position(|e| matches!(e, AgentEvent::RunFinished { .. }))
            .expect("回合终局必须发出 RunFinished");
        assert!(
            truncated_at < finished_at,
            "顺序不变量：ResponseTruncated（下标 {truncated_at}）必须先于 RunFinished\
             （下标 {finished_at}）—— 先告知「被截断了」，再收场：{:?}",
            events.events()
        );
    }

    /// N3（回归护栏）：**未**截断的 tool call 照常执行 —— 保护既有行为不被新分支误伤。
    ///
    /// 变异自检（已实测）：让 guard 恒真（把 `is_truncated()` 的判别写死成 `true`）
    /// ⇒ spy 计数变 0、`stop_reason` 不再是 `Completed`、且凭空多出 `ResponseTruncated`
    /// ⇒ 本用例三处断言全部变红。
    #[tokio::test]
    async fn non_truncated_tool_calls_still_execute() {
        let (spy, calls) = SpyTool::new();
        let registry = ToolRegistry::build(vec![Box::new(spy)]).unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("spy", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let result = {
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
                .unwrap()
        };

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "正常收尾的 tool call 必须照常执行（计数应为 1）"
        );
        assert_eq!(result.stop_reason, StopReason::Completed);
        assert!(
            !events
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::ResponseTruncated { .. })),
            "未截断 ⇒ 不得发出 ResponseTruncated：{:?}",
            events.events()
        );
    }

    /// 单条响应里含**两个** tool_use 的截断模型。
    ///
    /// `MockModel::push_truncated_tool_call` 一次只出一个 tool_use（每次 `push` 是**一个
    /// 独立响应** ⇒ 分两轮），覆盖不到「**同一条**被截断的响应里挂着多个工具块」——
    /// 而那正是真实 provider 的形状，也是配对完整性最容易写错的地方（只给
    /// `tool_calls[0]` 合成结果时，MockModel 版本会**漏检**，用例变假绿）。
    ///
    /// 两个工具名（`spy-a` / `spy-b`）**必须是 registry 里真注册过的** —— 见 `SpyTool` 注释。
    struct TwoTruncatedCallsModel {
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Model for TwoTruncatedCallsModel {
        fn model_id(&self) -> &str {
            "two-truncated-calls"
        }
        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let idx = {
                let mut calls = self.calls.lock().unwrap();
                let idx = *calls;
                *calls += 1;
                idx
            };
            if idx == 0 {
                // 第 1 轮：**一条**被截断的响应，里面挂着两个工具块
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![
                            ContentBlock::ToolUse {
                                id: ys_core::ToolCallId("tc-a".into()),
                                name: "spy-a".into(),
                                arguments: serde_json::json!({}),
                            },
                            ContentBlock::ToolUse {
                                id: ys_core::ToolCallId("tc-b".into()),
                                name: "spy-b".into(),
                                arguments: serde_json::json!({}),
                            },
                        ],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Length,
                })
            } else {
                // 第 2 轮：作废回喂后模型改小步，正常收尾
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

    /// N4：**配对完整性**（ADR-0001）—— 整批作废时每个 `ToolUse` 都必须拿到一条
    /// `ToolResult`，否则下一轮请求会被真实 provider 直接拒收（400）。
    ///
    /// 场景：**同一条**被截断的响应里两个不同名的 tool_use（`spy-a` / `spy-b`，**两个都真
    /// 注册进 registry**）→ 第 2 轮 `"done"`。
    /// 断言：
    ///  - 两个 spy 的计数**都 == 0**（真被作废，而非「名字没注册所以找不到」）；
    ///  - 回喂的那条 `Role::User` 消息里 `ToolResult` 数量 == 2，且 `tool_call_id` 集合与
    ///    assistant 消息里两个 `ToolUse` 的 `id` 集合**完全一致**；
    ///  - 两条结果全 `is_error`；
    ///  - 事件流里有**恰好一条** `ResponseTruncated { dropped_tool_calls: 2 }` ——
    ///    值必须是 2，钉住「**整批**作废」而非「只丢坏的」。
    ///
    /// 变异自检（已实测）：把 Step 8b 的 guard 恒假（`is_truncated()` 判别写死成 `false`）
    /// ⇒ 两个 spy 被**真执行**、计数变 1 ⇒ 计数断言变红，且 `ResponseTruncated` 事件断言同时变红。
    /// （旧版本用未注册的 `a`/`b` 名：guard 失效后走「registry 未命中」路径照样合成
    /// is_error 结果，计数恒 0 ⇒ 用例假绿、对该特性零判别力。见 `SpyTool` 注释。）
    #[tokio::test]
    async fn truncated_tool_calls_satisfy_pairing_invariant() {
        // 两个工具名都真注册：否则本用例对截断保护零判别力（见函数注释）。
        let (spy_a, calls_a) = SpyTool::new_named("spy-a");
        let (spy_b, calls_b) = SpyTool::new_named("spy-b");
        let registry = ToolRegistry::build(vec![Box::new(spy_a), Box::new(spy_b)]).unwrap();
        let model = TwoTruncatedCallsModel {
            calls: Mutex::new(0),
        };
        let mut session = MemorySession::new();
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

        assert_eq!(
            calls_a.load(Ordering::SeqCst),
            0,
            "整批作废 ⇒ spy-a 不得被执行（计数 1 说明 guard 失效、真去执行了半个 payload）"
        );
        assert_eq!(
            calls_b.load(Ordering::SeqCst),
            0,
            "整批作废 ⇒ spy-b 不得被执行（计数 1 说明 guard 失效、真去执行了半个 payload）"
        );

        // 事件：恰好一条 ResponseTruncated，且 dropped_tool_calls **恰好是 2**（整批作废）
        let truncated_events: Vec<_> = events
            .events()
            .iter()
            .filter(|e| matches!(e, AgentEvent::ResponseTruncated { .. }))
            .collect();
        assert_eq!(
            truncated_events.len(),
            1,
            "截断保护必须发出**恰好一条** ResponseTruncated：{:?}",
            events.events()
        );
        assert!(
            matches!(
                truncated_events[0],
                AgentEvent::ResponseTruncated {
                    dropped_tool_calls: 2
                }
            ),
            "dropped_tool_calls 必须恰好是 2（整批作废，不是只丢坏的）：{:?}",
            truncated_events[0]
        );

        // assistant 那批被作废的 tool_use 的 id 集合
        let use_ids: std::collections::BTreeSet<String> = session
            .messages()
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.0.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(use_ids.len(), 2, "前置：一条响应里两个 tool_use");

        // 回喂的那条 user 消息里的结果 id 集合
        let paired = session
            .messages()
            .iter()
            .find(|m| {
                m.role == Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .expect("截断批次必须把工具结果回喂进会话");
        let result_blocks: Vec<&ContentBlock> = paired
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .collect();
        assert_eq!(
            result_blocks.len(),
            2,
            "配对完整性：2 个 tool_use ⇒ 必须合成 2 条结果，实际 = {result_blocks:?}"
        );

        let result_ids: std::collections::BTreeSet<String> = result_blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.0.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            result_ids, use_ids,
            "结果的 tool_call_id 集合必须与 tool_use 的 id 集合**完全一致**（ADR-0001）"
        );
        assert!(
            result_blocks
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. })),
            "作废的结果必须全是 is_error：{result_blocks:?}"
        );
    }
}
