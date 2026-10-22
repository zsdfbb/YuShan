use std::collections::HashMap;
use std::time::Duration;

use super::{AgentInput, AgentLoop, LoopError, ModelSummarizer, RunResult};
use crate::compact;
use crate::turn_budget;
use async_trait::async_trait;
use futures_util::StreamExt;
use ys_component::RuntimeContext;
use ys_core::{ContentBlock, Message, Role, StopReason, ToolResult as CoreToolResult, Usage};
use ys_event::{AgentEvent, RoundPurpose, emit};
use ys_model::{FinishReason, ModelEvent, ModelEventSink, ModelRequest};
use ys_protocol::Boundary;
use ys_tool::{Tool, ToolContext, ToolError, ToolRegistry};

const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// 被截断的那批工具调用的统一回喂话术。必须**模型可读、能自愈**：
/// 点明「没执行、别以为成功了」，并给出可行的下一步。
const TRUNCATED_TOOL_NOTICE: &str = "tool call was NOT executed: the model response was truncated \
     (output token limit reached). Re-issue this call with a smaller payload, or split it into \
     several calls.";

/// 空响应的**一次性催促**话术：直接点名缺失的两样产出，并要求「给答案或发工具调用」。
///
/// 只在模型「只有思考、没有正文也没有工具调用」且 `finish_reason == Stop` 时发一次；
/// 仍空才收为 `NoOutput`。措辞刻意排除「只回思考」——那正是要治的形态。
const EMPTY_RETRY_PROMPT: &str = "You produced no visible output (no text and no tool call). \
     Respond now: either give your answer, or make a tool call — do not reply with only reasoning.";

/// 纯文本被截断后**续写一次**的催促话术。截断只代表「这一轮没说完」，不是回合结束，
/// 故回喂这条提示并 `continue`，让模型从断点接着把任务做完 —— 而不是把回合终结在半个
/// 回答上（那正是「一撞截断就停、任务没做完」的故障）。
///
/// 与 `TRUNCATED_TOOL_NOTICE` 同源但**不同形态**：工具批被作废时回喂的是一条 `ToolResult`
/// 错误；纯文本截断时没有工具可配对，回喂的是一条 `User` 催促消息。措辞点明「没说完」、
/// 要求接着做，并提示「正要发工具调用就用更小的 payload」。
const TRUNCATED_CONTINUE_PROMPT: &str = "Your previous response was cut off by the output token \
     limit before it finished — this was not the end of your turn. Continue from exactly where \
     you left off; do not repeat what you already said. If you were about to make a tool call, \
     make it now (use a smaller payload if it is large).";

/// 一次工具调用的超时预算：**工具声明值优先** → 否则 `limits.bash_timeout` → 否则 300s。
///
/// 「优先」而非取 min：`bash_timeout` 管的是「shell 命令多久算卡死」，不该成为长任务
/// 工具（如 subagent 派发）的上限。
fn tool_budget(tool: &dyn Tool, limits: &ys_component::RunLimits) -> Duration {
    tool.timeout()
        .unwrap_or_else(|| Duration::from_secs(limits.bash_timeout.unwrap_or(300)))
}

/// Step 9b 的调度单元。
///
/// - `Parallel` = 一段**连续**的并发安全工具（段内 `buffered(n)` 并发、**保序**）；
/// - `Serial` = 单个非安全工具，是**串行屏障** —— 它左右两段不得与它重叠。
///
/// **事件形状**（设计 §3.1）：`[read, read, bash]` ⇒ `C1,C2,R1,R2,C3,R3`。
/// 于是两条退化路径逐字节不变：全安全批 = 单个 `Parallel` 段（`C…C R…R`）；
/// 全不安全批 = N 个 `Serial` 段（`C1,R1,C2,R2,…`）。
#[derive(Debug, PartialEq)]
enum Segment {
    Parallel(Vec<usize>),
    Serial(usize),
}

/// 把待执行的工具按下标切成「连续只读段 + 串行屏障」。
///
/// **边界定义在 `pending` 上，不是 `tool_calls`**：被审批拒绝的工具不进 `pending`
/// （它一个字节都没执行）⇒ 不是屏障，也不得把两段只读切碎。
///
/// **registry 未命中归「非安全」**（沿用既有 `is_some_and` 语义）⇒ 自成 `Serial` 段，
/// 由该分支给「tool not found」兜底。**不可**把「未命中」当成「不存在所以跨过」——
/// 否则 `[read, 错名, read]` 会并发，而错名工具仍需一条错误结果。
fn split_segments(
    pending: &[usize],
    tool_calls: &[(ys_core::ToolCallId, String, serde_json::Value)],
    registry: &ToolRegistry,
) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut run: Vec<usize> = Vec::new();

    let flush = |run: &mut Vec<usize>, segments: &mut Vec<Segment>| {
        if !run.is_empty() {
            segments.push(Segment::Parallel(std::mem::take(run)));
        }
    };

    for &i in pending {
        let safe = registry
            .get(&tool_calls[i].1)
            .is_some_and(|t| t.is_concurrency_safe());
        if safe {
            run.push(i);
        } else {
            flush(&mut run, &mut segments);
            segments.push(Segment::Serial(i));
        }
    }
    flush(&mut run, &mut segments);
    segments
}

/// 执行一次工具调用，并把一切错误形态映射成 `ToolResult`（ADR-0001 错误结果通道）。
///
/// 串行与并行两条路径都走这里 —— 保证两路的错误语义逐字相同。
async fn call_one(
    tool: &dyn Tool,
    tool_name: String,
    args: serde_json::Value,
    tool_ctx: ToolContext<'_>,
    budget: Duration,
) -> CoreToolResult {
    match tokio::time::timeout(budget, tool.call(args, tool_ctx)).await {
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
            // 保留 `timed out` 子串（既有文案惯性），并补上工具名与生效预算。
            content: format!("Tool '{tool_name}' timed out after {budget:?}"),
            is_error: true,
        },
    }
}

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
        // 空响应**只重试一次**；本回合内一旦重试过，再空就如实收场。
        let mut empty_retried = false;
        // 纯文本截断后**只续写一次**（Step 8b）：截断没有工具那样的自愈回喂，一个总触顶的
        // 模型无限续写只会空烧 token，故以一次性标志封顶；仍被截断就如实收场。
        let mut truncated_continued = false;

        // 回合级工具输出闸门的**下界锚**：本回合新增消息的起点。必须在 Step 2 append
        // 用户消息**之前**取，否则用户消息会被当成可降级对象。
        let turn_start = ctx.session.messages().len();

        // Step 1：入口边界 —— 检查 abort（非破坏性探针）
        if ctx.boundary.as_ref().is_some_and(|b| b.is_aborted()) {
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
            if ctx.boundary.as_ref().is_some_and(|b| b.is_aborted()) {
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
                if fixed < trigger
                    && estimated + fixed > trigger
                    && let Some(plan) = compact::plan(messages, policy, window)
                {
                    match compact::execute(
                        messages,
                        &plan,
                        ctx.model,
                        policy,
                        fixed,
                        // **活对话的 system**（B4）：与主调用逐字相同 ⇒ 命中 KV cache。
                        ctx.system_prompt.as_deref(),
                    )
                    .await
                    {
                        Ok(outcome) => {
                            // 成本口径必须先于写回上报：摘要调用的 token 已经真实消耗，
                            // 若 `replace_all` 失败会提前 `?` 返回 —— 放在写回前，写回失败
                            // 也不丢成本口径；顺序上仍在 `ContextCompacted` 之前。
                            emit(
                                ctx.events,
                                AgentEvent::RoundUsage {
                                    purpose: RoundPurpose::Compaction,
                                    // 压缩发生在 `RoundStarted` **之前** ⇒ 记它正在准备的下一轮，
                                    // 与随后发出的 `RoundStarted.round` 同口径（不差一）。
                                    round: rounds.saturating_add(1),
                                    usage: outcome.usage,
                                },
                            )
                            .await
                            .map_err(LoopError::Event)?;
                            // 写回由**调用方**做（`execute` 不持有 session，评审 R3）。
                            ctx.session
                                .replace_all(outcome.messages)
                                .await
                                .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
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

            // 轮边界：拉取中途插话（steering）与中止（abort），保持入队顺序。
            // 不增 rounds、不改 turn —— Steer 是轮内的额外输入，不是新一轮。
            // Abort 命中即以 Cancelled 收场（先于本轮的 model 调用）。
            //
            // 注意：这里的 `Boundary::Abort` 是**压缩窗口的防御路径**。正常时序下
            // Abort 入队即置 `is_aborted` 标记，上面的 Step 3 探针（以及轮内的工具
            // 轮询）总是先行命中并提前返回，本分支几乎不可达；唯一窗口是 Step 4b
            // 上下文压缩 `await` 期间 Abort 才到达 —— 探针已过、消息刚到。保留它是
            // 为了在压缩耗时较长时不至于漏掉一次中止（宁可晚一轮，不可丢失）。
            if let Some(boundary) = ctx.boundary.as_ref() {
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
                        // 后台子 agent 的结果：与 `Steer` **同一条处理路径**（append + 进本轮
                        // `ModelRequest`），只换成 `DeferredResult` 事件 ⇒ UI 才不会把它
                        // 画成「用户说的话」。
                        Boundary::Deferred { handle, message } => {
                            ctx.session
                                .append(message.clone())
                                .await
                                .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                            emit(ctx.events, AgentEvent::DeferredResult { handle, message })
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
            // 轮开始的可见性事件：必须在模型调用**之前**发，消费者才能在本轮还在跑时
            // 就知道「跑到第几轮」。走自由函数 `emit()`（异步慢路径）—— 循环内自产事件
            // 不得走 `try_emit` 快路径（满了会被缓冲进 overflow，冲刷永不发生）。
            //
            // `round` 与终局 `RunFinished.rounds` 同口径（都在本行 `+= 1` 之后取值）。
            emit(ctx.events, AgentEvent::RoundStarted { round: rounds })
                .await
                .map_err(LoopError::Event)?;
            let tools = ctx.registry.specs().to_vec();
            let request = ModelRequest {
                messages: ctx.session.messages().to_vec(),
                tools,
                system: ctx.system_prompt.clone(),
                boundary: ctx.boundary.clone(),
                ..Default::default()
            };

            // Step 6：通过 Forwarder 调用 model
            let mut forwarder = Forwarder { sink: ctx.events };
            let model_result = ctx.model.complete(request, &mut forwarder).await;
            let response = match model_result {
                Ok(response) => response,
                // 中止**不是故障** ⇒ 不发 `RunFailed`，以 `Cancelled` 收场。
                Err(ys_model::ModelError::Cancelled { partial }) => {
                    // 先追加部分输出、再收场：顺序不可颠倒 ——
                    // 否则消费端可能已认为回合结束 ⇒ 部分输出丢失。
                    let final_message = partial.clone();
                    if let Some(partial) = partial {
                        ctx.session
                            .append(partial)
                            .await
                            .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                    }
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
                        final_message,
                    });
                }
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
            // 先复制本轮 usage 再累加：`RoundUsage` 上报的是**单轮**消耗，
            // 而 `total_usage` 是回合累计（会随终局事件一起发出）。
            let round_usage = response.usage.clone();
            total_usage = total_usage + response.usage;
            // 逐轮 usage 上报：位置固定在 `RoundStarted` 之后、`ToolCall` / `RunFinished`
            // 之前 —— 每条模型轮恰好一条。走自由函数 `emit()`（异步慢路径），不得走
            // `try_emit` 快路径（满了会被缓冲进 overflow，冲刷永不发生）。
            emit(
                ctx.events,
                AgentEvent::RoundUsage {
                    purpose: RoundPurpose::Turn,
                    round: rounds,
                    usage: round_usage,
                },
            )
            .await
            .map_err(LoopError::Event)?;
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
                    // 纯文本被截断 ⇒ 模型**这一轮没说完**。先回喂一条「续写」提示、
                    // `continue` 让它在下一轮接着把任务做完 —— 而不是把回合终结在半个回答上。
                    // （截断的文本已在本轮 Step 7 作为 assistant 消息落进会话，故模型看得到
                    // 自己说到一半的样子。）
                    //
                    // **只续一次**：截断没有工具批那样的自愈回喂，一个总触顶的模型无限续写
                    // 只会空烧 token（app 默认 `max_rounds = u32::MAX`，不是可靠的阀 ⇒ 必须
                    // 自带上限）。已经续过一次仍被截断 ⇒ 落下去如实收场。
                    if !truncated_continued {
                        truncated_continued = true;
                        ctx.session
                            .append(Message {
                                role: Role::User,
                                content: vec![ContentBlock::Text {
                                    text: TRUNCATED_CONTINUE_PROMPT.to_string(),
                                }],
                            })
                            .await
                            .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                        continue;
                    }

                    // 续写一次仍被截断：诚实收场，**不得**伪装成 Completed（那是静默的谎）。
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
                // 无 tool call -> 收场。
                //
                // **空 / 纯思考响应**（无正文、无工具调用）**不**当作 `Completed`：那会让「模型什么
                // 也没说」看起来像正常收尾（用户实测会以为卡住）。改判 `NoOutput` —— UI 据此给一行
                // 可见说明，一次性模式据此返回非零退出码。空白字符（如 `" "`）同样算空。
                let empty_output = text_content.trim().is_empty();

                // 空产出**先催一次**（直接提升「有答复」的概率）：仅当本回合尚未重试过、
                // `retry_empty_response` 开启、且是**合法空回复**（`finish_reason == Stop`；
                // 截断 / content-filter 不走此路，避免误催）时，追加一条催促消息并 `continue`。
                // 消耗一个 round ⇒ 自动受 `max_rounds` 与轮边界 abort 约束；仍空则按第一批
                // 语义如实收为 `NoOutput`。
                if empty_output
                    && !empty_retried
                    && ctx.limits.retry_empty_response
                    && response.finish_reason == FinishReason::Stop
                {
                    empty_retried = true;
                    ctx.session
                        .append(Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: EMPTY_RETRY_PROMPT.to_string(),
                            }],
                        })
                        .await
                        .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                    continue;
                }

                let stop_reason = if empty_output {
                    StopReason::NoOutput
                } else {
                    StopReason::Completed
                };
                let final_msg = if empty_output {
                    None
                } else {
                    Some(assistant_message)
                };
                emit(
                    ctx.events,
                    AgentEvent::RunFinished {
                        stop_reason: stop_reason.clone(),
                        usage: total_usage.clone(),
                        rounds,
                    },
                )
                .await
                .map_err(LoopError::Event)?;
                return Ok(RunResult {
                    stop_reason,
                    usage: total_usage,
                    rounds,
                    final_message: final_msg,
                });
            }

            // Step 9：执行工具调用。
            //
            // **按「连续只读段」调度**（C5 / `explore-speed` 设计 §4）：把待执行工具切成
            // `Segment::Parallel`（段内并发、保序）与 `Segment::Serial`（单工具，串行屏障）。
            // 旧的准入是**全批** all-or-nothing —— 批里只要有一个非安全工具就整批退化串行，
            // 而 `bash` 占全部工具调用的大头 ⇒ 绝大多数混合批次拿不到任何并行。
            //
            // 三条退化路径**逐字节不变**：全安全批 = 单个 `Parallel` 段（`C…C R…R`）；
            // 全不安全批 = N 个 `Serial` 段（`C1,R1,C2,R2,…`）；单元素同理。
            //
            // 三段式（`ctx.events: &mut dyn EventSink` 不能跨并发任务共享）：
            //   9a 串行：审批 + 收集 `pending`（被拒的不进 `pending`）
            //   9b 逐段执行：`Parallel` ⇒ emit 全部 `ToolCall` → 并发执行 → 完成即发
            //                 `Serial`  ⇒ emit `ToolCall` → 执行 → emit `ToolResult`
            //   9c 保序：连续错误计数 + 组装单条消息（ADR-0001 配对）
            let mut outcomes: Vec<Option<CoreToolResult>> = vec![None; tool_calls.len()];
            let summarizer = ModelSummarizer { model: ctx.model };

            // 9a：审批（串行）。**被拒的工具不进 `pending`** ⇒ 它既不发 `ToolCall`
            // 也不发 `ToolResult` 事件（既有形状），只在回喂消息里留一条 `is_error`。
            let mut pending: Vec<usize> = Vec::new();
            for (idx, (_, tool_name, tool_args)) in tool_calls.iter().enumerate() {
                if let Some(approval) = &ctx.approval
                    && approval.needs_approval(tool_name, tool_args)
                {
                    match approval.request_approval(tool_name, tool_args).await {
                        ys_tool::ApprovalDecision::Approved => {}
                        ys_tool::ApprovalDecision::Denied { reason } => {
                            outcomes[idx] = Some(CoreToolResult {
                                content: format!("operation denied: {reason}"),
                                is_error: true,
                            });
                            continue;
                        }
                    }
                }
                pending.push(idx);
            }

            // 9b：**逐段**执行。准入从「全批 all-safe」改为「连续只读段并发 + 非安全工具
            // 作串行屏障」（C5）。
            let segments = split_segments(&pending, &tool_calls, ctx.registry);
            for segment in segments {
                match segment {
                    Segment::Parallel(indices) => {
                        // 并行段：先**保序** emit 全部 `ToolCall`（否则执行期无法再借
                        // `ctx.events`）—— 形状 `C1..Cn`，与旧并行路径逐字节相同。
                        for &i in &indices {
                            let (tool_call_id, tool_name, tool_args) = &tool_calls[i];
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
                        }
                        // `buffer_unordered` 而非 `spawn`：`Tool::call` 的 future 借了
                        // `&dyn Tool` 与 `ToolContext<'_>`（非 `'static`）；该组合子在单任务内
                        // 并发，完成一个就交出一个，结果在这里按完成顺序立即发出。
                        //
                        // 边界源借自**本地副本**而非 `ctx`：`ctx.boundary.as_deref()` 会
                        // 不可变借用 `ctx`，而下方 `let events = &mut *ctx.events;` 要可变借用
                        // 同一结构体 ⇒ 两者直接共存会冲突。先 clone `Arc` 到本地，
                        // 借用只落在本地变量上，`ctx` 得以解放。
                        //
                        // 声明顺序吃紧：`futs` 持有 `tool_ctx`（其中借了 `boundary_ref`）
                        // ⇒ `boundary_arc` 必须**先于** `futs` 声明，否则它先于 `futs` 被 drop
                        // ⇒ 借用指向已析构的值。
                        let boundary_arc = ctx.boundary.clone();
                        let boundary_ref: Option<&dyn ys_protocol::BoundarySource> =
                            boundary_arc.as_deref();
                        let mut futs = Vec::with_capacity(indices.len());
                        for &i in &indices {
                            let (_, tool_name, tool_args) = &tool_calls[i];
                            let tool = ctx.registry.get(tool_name).expect("已确认命中");
                            let budget = tool_budget(tool, &ctx.limits);
                            // 外层预算挂进 `ToolContext` ⇒ 工具内**始终**有 deadline 可依。
                            // 预算**按工具**算（工具声明值优先 → `limits.bash_timeout` → 300s）
                            // ⇒ 并行批里每个工具拿到**各自**的预算。
                            let mut tool_ctx = ToolContext::new(
                                boundary_ref,
                                ctx.cwd.clone(),
                                ctx.workspace_root.clone(),
                            )
                            .with_summarizer(&summarizer)
                            .with_tool_budget(budget);
                            if let Some(runner) = ctx.subagent {
                                tool_ctx = tool_ctx.with_subagent(runner);
                            }
                            let name = tool_name.clone();
                            let args = tool_args.clone();
                            futs.push(async move {
                                (i, call_one(tool, name, args, tool_ctx, budget).await)
                            });
                        }
                        // **全批算一次**上限（与旧并行路径同一语义）；
                        // 段的有效并发 = `min(n, 段长)`。
                        let n = ctx.limits.max_parallel_tools.max(1);
                        let mut executed = futures_util::stream::iter(futs).buffer_unordered(n);
                        // future 只借 registry / boundary / subagent；events 是另一个字段，
                        // 因而可以在每个结果完成时立即 reborrow 并 await emit。
                        let events = &mut *ctx.events;
                        while let Some((i, result)) = executed.next().await {
                            let id = tool_calls[i].0.clone();
                            emit(
                                events,
                                AgentEvent::ToolResult {
                                    id,
                                    result: result.clone(),
                                },
                            )
                            .await
                            .map_err(LoopError::Event)?;
                            outcomes[i] = Some(result);
                        }
                    }
                    Segment::Serial(i) => {
                        // 串行屏障：**逐个** emit ToolCall → 执行 → emit ToolResult
                        // （保持既有交错形状 `C1,R1`，旧串行路径逐字节不变）。
                        let boundary_arc = ctx.boundary.clone();
                        let boundary_ref: Option<&dyn ys_protocol::BoundarySource> =
                            boundary_arc.as_deref();
                        let (tool_call_id, tool_name, tool_args) = &tool_calls[i];
                        // abort 探针（不轮询）：已中止 ⇒ 不再发起这个工具。
                        // 探针放在 emit `ToolCall` **之前** ⇒ 被中止的那次调用连事件都不发，
                        // 消费端不会看到一个「叫了但没执行」的工具调用。
                        if ctx.boundary.as_ref().is_some_and(|b| b.is_aborted()) {
                            outcomes[i] = Some(CoreToolResult {
                                content: "tool not executed: turn cancelled".to_string(),
                                is_error: true,
                            });
                            continue;
                        }
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

                        let result = match ctx.registry.get(tool_name) {
                            Some(tool) => {
                                let budget = tool_budget(tool, &ctx.limits);
                                // 外层预算挂进 `ToolContext` ⇒ 工具内**始终**有 deadline 可依
                                // （与并行路径同款：预算按工具算）。
                                let mut tool_ctx = ToolContext::new(
                                    boundary_ref,
                                    ctx.cwd.clone(),
                                    ctx.workspace_root.clone(),
                                )
                                .with_summarizer(&summarizer)
                                .with_tool_budget(budget);
                                if let Some(runner) = ctx.subagent {
                                    tool_ctx = tool_ctx.with_subagent(runner);
                                }
                                call_one(
                                    tool,
                                    tool_name.clone(),
                                    tool_args.clone(),
                                    tool_ctx,
                                    budget,
                                )
                                .await
                            }
                            None => CoreToolResult {
                                content: format!("tool not found: {tool_name}"),
                                is_error: true,
                            },
                        };

                        emit(
                            ctx.events,
                            AgentEvent::ToolResult {
                                id: tool_call_id.clone(),
                                result: result.clone(),
                            },
                        )
                        .await
                        .map_err(LoopError::Event)?;
                        outcomes[i] = Some(result);
                    }
                }
            }

            // 9c：按**原始顺序**记账 + 组装单条消息（ADR-0001 配对完整性）。
            let mut tool_results_for_message: Vec<ContentBlock> =
                Vec::with_capacity(tool_calls.len());
            for (idx, (tool_call_id, tool_name, _)) in tool_calls.iter().enumerate() {
                let result = outcomes[idx].take().unwrap_or_else(|| CoreToolResult {
                    content: format!("tool not executed: {tool_name}"),
                    is_error: true,
                });

                // T11c：连续错误跟踪（按原始顺序更新）。
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

            // Step 11：回合级工具输出总量闸门 —— 超预算时降级**更早**的结果（头部截断 +
            // 续读指针），把当轮回喂 token 压下来。算法在 `crate::turn_budget`。
            //
            // 与压缩的关系：Step 4b 的压缩已 `replace_all` 缩短会话，故下界取
            // `turn_start.min(len)`（保守：宁少降级，不误删更早历史）；降级只缩内容、
            // 不删消息 ⇒ 不与压缩的窗口触发冲突。`budget == 0`（默认）时整段跳过。
            //
            // **不新增 `AgentEvent` 变体**：降级只影响模型侧下一轮请求；工具结果的原始
            // 形态用户已在 transcript 实时看到。
            let budget = ctx.limits.max_turn_tool_result_tokens;
            if budget > 0 {
                let len = ctx.session.messages().len();
                let start = turn_start.min(len);
                let plan = turn_budget::plan_elisions(ctx.session.messages(), start, budget);
                if !plan.is_empty() {
                    let mut messages = ctx.session.messages().to_vec();
                    for &idx in &plan {
                        if let Some(elided) = turn_budget::elide_message(&messages[idx]) {
                            messages[idx] = elided;
                        }
                    }
                    ctx.session
                        .replace_all(messages)
                        .await
                        .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use ys_component::RunLimits;
    use ys_event::{AgentEvent, CollectingSink, EventSink};
    use ys_model::{FinishReason, MockModel, MockResponse, Model, ModelError, ModelResponse};
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

    // ==================== RoundUsage：逐轮 usage 上报 ====================

    fn usage_with_reasoning(reasoning_tokens: u32) -> Usage {
        Usage {
            input_tokens: 10,
            output_tokens: 2,
            reasoning_tokens: Some(reasoning_tokens),
            ..Default::default()
        }
    }

    /// 附带 `usage` 的预写响应 model。
    ///
    /// `MockModel` 的 `usage` 恒为 `Usage::default()`，无法表达「provider 上报了
    /// `reasoning_tokens`」这条路径；本替身让每条响应自带 usage，才能断言**每轮单独上报**
    /// 而非只断言「有事件」。
    struct UsageModel {
        responses: Mutex<Vec<(MockResponse, Usage)>>,
    }

    impl UsageModel {
        fn new() -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
            }
        }

        fn push_text(&self, text: &str, usage: Usage) {
            self.responses
                .lock()
                .unwrap()
                .push((MockResponse::Text(text.into()), usage));
        }

        fn push_tool_call(&self, name: &str, arguments: serde_json::Value, usage: Usage) {
            self.responses.lock().unwrap().push((
                MockResponse::ToolCall {
                    name: name.into(),
                    arguments,
                },
                usage,
            ));
        }

        fn push_error(&self, msg: &str) {
            self.responses
                .lock()
                .unwrap()
                .push((MockResponse::Error(msg.into()), Usage::default()));
        }
    }

    #[async_trait::async_trait]
    impl Model for UsageModel {
        fn model_id(&self) -> &str {
            "usage-model"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
            sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let (response, usage) = self.responses.lock().unwrap().remove(0);
            match response {
                MockResponse::Text(text) => {
                    sink.emit(ModelEvent::TextDelta { text: text.clone() })?;
                    Ok(ModelResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::Text { text }],
                        },
                        usage,
                        finish_reason: FinishReason::Stop,
                    })
                }
                MockResponse::ToolCall { name, arguments } => Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: ys_core::ToolCallId(format!("usage-{name}")),
                            name,
                            arguments,
                        }],
                    },
                    usage,
                    finish_reason: FinishReason::ToolCalls,
                }),
                MockResponse::Error(msg) => Err(ModelError::Provider(msg)),
                _ => panic!("UsageModel 只支持 Text / ToolCall / Error"),
            }
        }
    }

    fn turn_usages(events: &CollectingSink) -> Vec<(u32, Usage)> {
        events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RoundUsage {
                    purpose: RoundPurpose::Turn,
                    round,
                    usage,
                } => Some((*round, usage.clone())),
                _ => None,
            })
            .collect()
    }

    fn compaction_usages(events: &CollectingSink) -> Vec<(u32, Usage)> {
        events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RoundUsage {
                    purpose: RoundPurpose::Compaction,
                    round,
                    usage,
                } => Some((*round, usage.clone())),
                _ => None,
            })
            .collect()
    }

    /// Step 7：每个模型轮**恰好一条** `RoundUsage { purpose: Turn, round: N }`，且上报的是
    /// **该轮单轮** usage（不是回合累计），顺序在 `RoundStarted` 之后、`ToolCall` /
    /// `RunFinished` 之前。
    ///
    /// 变异说明：
    /// - 删掉 Step 7 的 `emit` ⇒ 本用例红（`turn_usages` 为空）。
    /// - 把 `round_usage` 换成 `total_usage.clone()` ⇒ 第 2 轮断言红（累计 `Some(14)` ≠
    ///   单轮 `Some(7)`）。
    /// - 把 emit 挪到 `RoundStarted` 之前 ⇒ 顺序断言红。
    #[tokio::test]
    async fn round_usage_emitted_once_per_turn_round() {
        let (spy, calls) = SpyTool::new();
        let registry = ToolRegistry::build(vec![Box::new(spy)]).unwrap();
        let model = UsageModel::new();
        let per_round = usage_with_reasoning(7);
        model.push_tool_call("spy", serde_json::json!({}), per_round.clone());
        model.push_text("done", per_round.clone());

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
        assert_eq!(calls.load(Ordering::SeqCst), 1, "前置：工具被真实执行");

        let usages = turn_usages(&events);
        assert_eq!(
            usages.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            vec![1, 2],
            "两轮 ⇒ 恰好两条，轮号从 1 起：{:?}",
            events.events()
        );
        for (round, usage) in &usages {
            assert_eq!(
                usage, &per_round,
                "第 {round} 轮的 RoundUsage 必须是该轮的单轮 usage"
            );
        }

        let evts = events.events();
        let position =
            |pred: &dyn Fn(&AgentEvent) -> bool| evts.iter().position(pred).expect("事件应存在");
        let started_1 = position(&|e| matches!(e, AgentEvent::RoundStarted { round: 1 }));
        let usage_1 = position(&|e| {
            matches!(
                e,
                AgentEvent::RoundUsage {
                    purpose: RoundPurpose::Turn,
                    round: 1,
                    ..
                }
            )
        });
        let first_tool_call = position(&|e| matches!(e, AgentEvent::ToolCall { .. }));
        let started_2 = position(&|e| matches!(e, AgentEvent::RoundStarted { round: 2 }));
        let usage_2 = position(&|e| {
            matches!(
                e,
                AgentEvent::RoundUsage {
                    purpose: RoundPurpose::Turn,
                    round: 2,
                    ..
                }
            )
        });
        let finished = position(&|e| matches!(e, AgentEvent::RunFinished { .. }));
        assert!(
            started_1 < usage_1 && usage_1 < first_tool_call,
            "第 1 轮 usage 必须在 RoundStarted 之后、ToolCall 之前：{evts:?}"
        );
        assert!(
            started_2 < usage_2 && usage_2 < finished,
            "第 2 轮 usage 必须在 RoundStarted 之后、RunFinished 之前：{evts:?}"
        );

        // 终局事件仍报**累计**值（14 = 7 + 7），与逐轮上报的单轮值互不混淆。
        match &evts[finished] {
            AgentEvent::RunFinished { usage, .. } => assert_eq!(
                usage.reasoning_tokens,
                Some(14),
                "RunFinished 报回合累计，RoundUsage 报单轮"
            ),
            other => panic!("期望 RunFinished，实得 {other:?}"),
        }
    }

    /// 重试路径同样服从「每个模型轮恰好一条 Turn usage」：首轮空响应先催一次，
    /// 次轮正常收场 ⇒ 恰好两条，轮号 `[1, 2]`，各自等于该轮 `response.usage`。
    ///
    /// 变异说明：把 Step 7 的 emit 下移到空响应判断之后 ⇒ 首轮走 `continue` 前不会
    /// 发 usage ⇒ 本用例红（只剩第 2 轮一条）。
    #[tokio::test]
    async fn round_usage_emitted_once_per_retried_empty_round() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = UsageModel::new();
        let first_usage = usage_with_reasoning(3);
        let second_usage = usage_with_reasoning(9);
        model.push_text("", first_usage.clone());
        model.push_text("done", second_usage.clone());

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

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert_eq!(result.rounds, 2, "空响应重试消耗一个 round");
        assert_eq!(
            turn_usages(&events),
            vec![(1, first_usage), (2, second_usage)],
            "重试路径每个模型轮恰好一条 Turn usage：{:?}",
            events.events()
        );
    }

    /// 自动压缩：事件流同时含 `purpose=Turn` 与 `purpose=Compaction`；后者 usage **等于
    /// 摘要响应的 usage**、`round` 记它正在准备的下一轮（1），且排在 `ContextCompacted` 之前。
    ///
    /// 变异说明：
    /// - 删掉压缩分支的 emit ⇒ 本用例红（`compaction_usages` 为空）。
    /// - 把 purpose 写成 `Turn` ⇒ 本用例红（`compaction_usages` 为空，且 Turn 多一条）。
    #[tokio::test]
    async fn compaction_usage_has_compaction_purpose_and_summary_usage() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = UsageModel::new();
        let summary_usage = Usage {
            input_tokens: 60,
            output_tokens: 11,
            reasoning_tokens: Some(7),
            ..Default::default()
        };
        let turn_usage = usage_with_reasoning(7);
        model.push_text("SUMMARY", summary_usage.clone());
        model.push_text("done", turn_usage.clone());

        let mut session = MemorySession::new();
        seed_large_session(&mut session).await;
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

        let compaction = compaction_usages(&events);
        assert_eq!(
            compaction,
            vec![(1, summary_usage)],
            "压缩 usage 必须等于摘要响应的 usage，轮号记下一轮：{:?}",
            events.events()
        );
        assert_eq!(
            turn_usages(&events),
            vec![(1, turn_usage)],
            "压缩后主对话仍只有一条 Turn usage"
        );

        let evts = events.events();
        let compaction_at = evts
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AgentEvent::RoundUsage {
                        purpose: RoundPurpose::Compaction,
                        ..
                    }
                )
            })
            .expect("压缩必须发 RoundUsage");
        let compacted_at = evts
            .iter()
            .position(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
            .expect("必须发 ContextCompacted");
        assert!(
            compaction_at < compacted_at,
            "RoundUsage(Compaction) 必须排在 ContextCompacted 之前：{evts:?}"
        );
    }

    /// 压缩失败：只发 `ContextCompactionFailed`，**不发** `purpose=Compaction` 的 usage。
    #[tokio::test]
    async fn failed_compaction_emits_no_compaction_usage() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = UsageModel::new();
        model.push_error("provider 挂了");
        model.push_text("done", usage_with_reasoning(7));

        let mut session = MemorySession::new();
        seed_large_session(&mut session).await;
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
            compaction_usages(&events).is_empty(),
            "失败的压缩不得上报 Compaction usage：{:?}",
            events.events()
        );
        assert_eq!(
            turn_usages(&events).len(),
            1,
            "主对话那一轮仍要上报 Turn usage：{:?}",
            events.events()
        );
        assert!(
            events
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::ContextCompactionFailed { .. })),
            "失败必须可见：{:?}",
            events.events()
        );
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
            ToolSpec::new(
                self.name.as_str(),
                "counts invocations",
                serde_json::json!({}),
            )
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

    /// 可配超时的 spy：`timeout()` 返回声明值，`call` 睡 `sleep`。
    struct TimedTool {
        name: String,
        timeout: Option<Duration>,
        sleep: Duration,
    }

    #[async_trait::async_trait]
    impl Tool for TimedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name.as_str(), "times out", serde_json::json!({}))
        }
        fn timeout(&self) -> Option<Duration> {
            self.timeout
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            tokio::time::sleep(self.sleep).await;
            Ok(CoreToolResult {
                content: "ok".into(),
                is_error: false,
            })
        }
    }

    /// U1-N2a：**工具声明的超时优先于 `bash_timeout` 的 300s 兜底**。
    ///
    /// 声明 50ms、`call` 睡 200ms 的工具，在 `bash_timeout = None`（⇒ 300s）下必须被
    /// **50ms** 杀掉并给出 `is_error`（含工具名）。若实现取 `min` 或忽略声明，本用例
    /// 要么挂 300s 后变红、要么 `call` 成功返回 ⇒ 两种都变红。
    #[tokio::test]
    async fn declared_tool_timeout_beats_bash_timeout_fallback() {
        let registry = ToolRegistry::build(vec![Box::new(TimedTool {
            name: "slow".into(),
            timeout: Some(Duration::from_millis(50)),
            sleep: Duration::from_millis(200),
        })])
        .unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("slow", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(5);
        limits.bash_timeout = None; // ⇒ 兜底 300s
        {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        let results = tool_results_in(&session);
        assert_eq!(results.len(), 1, "应有恰好一条工具结果");
        match results[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(is_error, "声明超时应产生 is_error：{content}");
                assert!(
                    content.contains("timed out"),
                    "文案应含 timed out：{content}"
                );
                assert!(content.contains("slow"), "文案应含工具名：{content}");
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    /// U1-N2b：**默认 `None` 的工具不受影响**（短调用正常成功，无回归）。
    #[tokio::test]
    async fn default_tool_timeout_none_is_unchanged() {
        let registry = ToolRegistry::build(vec![Box::new(TimedTool {
            name: "fast".into(),
            timeout: None,
            sleep: Duration::from_millis(1),
        })])
        .unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("fast", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        let results = tool_results_in(&session);
        assert_eq!(results.len(), 1);
        match results[0] {
            ContentBlock::ToolResult { is_error, .. } => {
                assert!(!is_error, "默认 None 的短调用应成功");
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    /// U3：`RuntimeContext.subagent` 经 `BasicLoop` 落到工具看到的 `ToolContext.subagent`。
    #[tokio::test]
    async fn subagent_runner_reaches_tool_context() {
        struct ProbeRunner;
        #[async_trait::async_trait]
        impl ys_tool::SubagentRunner for ProbeRunner {
            async fn run(
                &self,
                _prompt: &str,
                _model: Option<&str>,
                _boundary: Option<&dyn ys_protocol::BoundarySource>,
            ) -> Result<ys_tool::SubagentOutcome, String> {
                unimplemented!("probe 不应被调用")
            }
        }

        struct SeenTool(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl Tool for SeenTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec::new("seen", "records subagent mount", serde_json::json!({}))
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                ctx: ToolContext<'_>,
            ) -> Result<CoreToolResult, ys_tool::ToolError> {
                if ctx.subagent.is_some() {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
                Ok(CoreToolResult {
                    content: "ok".into(),
                    is_error: false,
                })
            }
        }

        let seen = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::build(vec![Box::new(SeenTool(seen.clone()))]).unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("seen", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let runner = ProbeRunner;
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            ctx.subagent = Some(&runner);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "工具应看到经 RuntimeContext 挂载的 subagent runner（三跳注入）"
        );
    }

    // ───────────────────────── 刀 6：并行工具批（§4.4 六条） ─────────────────────────

    /// 可声明并发安全的 spy：记录「同时在跑」的峰值。
    struct ConcTool {
        name: String,
        safe: bool,
        sleep: Duration,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Tool for ConcTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name.as_str(), "conc probe", serde_json::json!({}))
        }
        fn is_concurrency_safe(&self) -> bool {
            self.safe
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.sleep).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(CoreToolResult {
                content: format!("{} ok", self.name),
                is_error: false,
            })
        }
    }

    /// 一次响应发 N 个工具调用；第二次调用返回文本收尾。
    struct MultiToolModel {
        calls: Vec<(String, String)>,
        round: AtomicUsize,
    }

    impl MultiToolModel {
        fn new(calls: &[(&str, &str)]) -> Self {
            Self {
                calls: calls
                    .iter()
                    .map(|(id, n)| (id.to_string(), n.to_string()))
                    .collect(),
                round: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Model for MultiToolModel {
        fn model_id(&self) -> &str {
            "multi"
        }
        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            let round = self.round.fetch_add(1, Ordering::SeqCst);
            let message = if round == 0 {
                Message {
                    role: Role::Assistant,
                    content: self
                        .calls
                        .iter()
                        .map(|(id, name)| ContentBlock::ToolUse {
                            id: ys_core::ToolCallId(id.clone()),
                            name: name.clone(),
                            arguments: serde_json::json!({}),
                        })
                        .collect(),
                }
            } else {
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                }
            };
            Ok(ys_model::ModelResponse {
                message,
                usage: Usage::default(),
                finish_reason: if round == 0 {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
            })
        }
    }

    /// 事件流里 `ToolCall` / `ToolResult` 的 (kind, id) 序列。
    fn call_result_order(events: &CollectingSink) -> Vec<(&'static str, String)> {
        events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCall { call } => Some(("C", call.id.0.clone())),
                AgentEvent::ToolResult { id, .. } => Some(("R", id.0.clone())),
                _ => None,
            })
            .collect()
    }

    fn assert_calls_precede_results(
        order: &[(&'static str, String)],
        expected_calls: usize,
        expected_results: usize,
    ) {
        assert_eq!(order.len(), expected_calls + expected_results);
        assert!(
            order[..expected_calls].iter().all(|(kind, _)| *kind == "C"),
            "全部 ToolCall 必须先于任何 ToolResult：{order:?}"
        );
        assert!(
            order[expected_calls..].iter().all(|(kind, _)| *kind == "R"),
            "全部 ToolCall 发完后才允许出现 ToolResult：{order:?}"
        );
    }

    fn result_ids_in(order: &[(&'static str, String)]) -> Vec<String> {
        order
            .iter()
            .filter_map(|(kind, id)| (*kind == "R").then(|| id.clone()))
            .collect()
    }

    fn tool_result_pairs(events: &CollectingSink) -> Vec<(String, String)> {
        events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { id, result } => {
                    Some((id.0.clone(), result.content.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn tool_result_ids_in(session: &MemorySession) -> Vec<String> {
        session
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.0.clone()),
                _ => None,
            })
            .collect()
    }

    fn conc_tool_registry(
        specs: &[(&str, bool, u64)],
    ) -> (ToolRegistry, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tools: Vec<Box<dyn Tool>> = specs
            .iter()
            .map(|(name, safe, ms)| {
                Box::new(ConcTool {
                    name: name.to_string(),
                    safe: *safe,
                    sleep: Duration::from_millis(*ms),
                    active: active.clone(),
                    peak: peak.clone(),
                }) as Box<dyn Tool>
            })
            .collect();
        (ToolRegistry::build(tools).unwrap(), active, peak)
    }

    async fn run_batch(
        registry: &ToolRegistry,
        model: &MultiToolModel,
        max_parallel: usize,
    ) -> (Vec<(&'static str, String)>, std::time::Duration) {
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(5);
        limits.max_parallel_tools = max_parallel;
        let started = std::time::Instant::now();
        {
            let mut ctx = make_ctx(model, registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        (call_result_order(&events), started.elapsed())
    }

    /// §4.4-1：**默认串行**（两个 `false`）⇒ 事件序列 `C1,R1,C2,R2`，且无重叠。
    #[tokio::test]
    async fn batch_is_serial_by_default_and_keeps_order() {
        let (registry, _, peak) = conc_tool_registry(&[("a", false, 40), ("b", false, 40)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b")]);
        let (order, _) = run_batch(&registry, &model, 4).await;
        assert_eq!(
            order,
            vec![
                ("C", "1".into()),
                ("R", "1".into()),
                ("C", "2".into()),
                ("R", "2".into())
            ],
            "默认串行：C1,R1,C2,R2"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 1, "串行不得有重叠执行");
    }

    /// §4.4-2：**每一个都声明 safe** ⇒ 并行。
    ///
    /// `ToolResult` 的顺序由完成顺序决定，不是本用例的稳定契约；这里只钉住
    /// 「全部 ToolCall 先发」、两个结果恰好各出现一次，以及峰值并发为 2。
    #[tokio::test]
    async fn batch_is_parallel_when_every_tool_is_safe() {
        let (registry, _, peak) = conc_tool_registry(&[("a", true, 250), ("b", true, 250)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b")]);
        let (order, _) = run_batch(&registry, &model, 4).await;
        assert_calls_precede_results(&order, 2, 2);
        let mut result_ids = result_ids_in(&order);
        result_ids.sort();
        assert_eq!(result_ids, vec!["1", "2"]);
        assert_eq!(peak.load(Ordering::SeqCst), 2, "两个 safe 工具应真的并发");
    }

    /// **[safe, unsafe]**：非安全工具是**串行屏障**，两侧不重叠。
    ///
    /// C5 之后形状仍是 `C1,R1,C2,R2` —— 但成因变了：`a` 自成**单元素并行段**
    /// （`Parallel([0])` ⇒ `C,R` 相邻），`b` 是 `Serial(1)`。
    /// 这与旧「整批退化串行」的输出一致，故本用例保留为**回归钩子**。
    ///
    /// 变异自检：把 `Segment::Parallel` 的 emit 顺序改成「先全部 R 再 C」⇒ 变红。
    #[tokio::test]
    async fn mixed_batch_keeps_barrier_and_no_overlap() {
        let (registry, _, peak) = conc_tool_registry(&[("a", true, 60), ("b", false, 60)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b")]);
        let (order, _) = run_batch(&registry, &model, 4).await;
        assert_eq!(
            order,
            vec![
                ("C", "1".into()),
                ("R", "1".into()),
                ("C", "2".into()),
                ("R", "2".into())
            ],
            "屏障两侧不得重叠：单元素段 = C,R 相邻"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    /// **C5 的核心收益**：`[safe, safe, unsafe]` ⇒ 前两个**真的并发**。
    ///
    /// 旧准入是**全批** all-or-nothing ⇒ 批里有一个非安全工具就整批串行。
    /// 现在切成 `Parallel([0,1]) + Serial(2)` ⇒ 形状 `C1,C2,R1,R2,C3,R3`，峰值并发 2。
    ///
    /// 变异：把 `split_segments` 改成「整批全 safe 才给并行段」（旧逻辑）⇒
    /// 形状变成 `C1,R1,C2,R2,C3,R3` 且峰值 1 ⇒ 两条断言都变红。
    #[tokio::test]
    async fn leading_safe_run_is_parallel_before_the_barrier() {
        let (registry, _, peak) =
            conc_tool_registry(&[("a", true, 200), ("b", true, 200), ("c", false, 200)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b"), ("3", "c")]);
        let (order, _) = run_batch(&registry, &model, 4).await;

        assert_eq!(
            order,
            vec![
                ("C", "1".into()),
                ("C", "2".into()),
                ("R", "1".into()),
                ("R", "2".into()),
                ("C", "3".into()),
                ("R", "3".into())
            ],
            "前两个连着发、结果紧随其后；屏障单独一段：{order:?}"
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "两个 safe 工具必须真的并发（旧实现会整批串行 ⇒ 峰值 1）"
        );
    }

    /// **registry 未命中 ⇒ 归「非安全」**（设计 §4.1-2），自成 `Serial` 段。
    ///
    /// 两条理由：
    ///  1. `[read, 错名, read]` **不得**并发 —— 错名工具仍需一条 `tool not found` 结果；
    ///  2. 防 panic：并行段里对已确认命中的工具用 `.expect("已确认命中")`，
    ///     若把未命中当安全，它会被放进并行段 ⇒ **直接 panic**。
    ///
    /// 变异：把 `.unwrap_or_default()` 判成「未命中 ⇒ safe」⇒ 本用例 panic/变红。
    #[tokio::test]
    async fn unknown_tool_in_batch_is_a_barrier_not_parallel() {
        // registry 只有 `a` / `c`；模型却调了不存在的 `nope` 夹在中间。
        let (registry, _, peak) = conc_tool_registry(&[("a", true, 60), ("c", true, 60)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "nope"), ("3", "c")]);
        let (order, _) = run_batch(&registry, &model, 4).await;

        assert_eq!(
            order,
            vec![
                ("C", "1".into()),
                ("R", "1".into()),
                ("C", "2".into()),
                ("R", "2".into()),
                ("C", "3".into()),
                ("R", "3".into())
            ],
            "未命中工具必须是串行屏障：{order:?}"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 1, "屏障两侧不得并发");
    }

    /// **[safe, unsafe, safe]** ⇒ 两段各一个元素 ⇒ 完全不并行（屏障把只读切碎）。
    ///
    /// 这是设计的**已知代价**（§3.2：`[read, bash, read]` 新旧事件流完全相同）。
    /// 钉住它，防止有人为了「提高并行度」把屏障跨过去（那会让写操作与读操作重叠）。
    #[tokio::test]
    async fn safe_runs_separated_by_barrier_do_not_parallelize() {
        let (registry, _, peak) =
            conc_tool_registry(&[("a", true, 60), ("b", false, 60), ("c", true, 60)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b"), ("3", "c")]);
        let (order, _) = run_batch(&registry, &model, 4).await;

        assert_eq!(
            order,
            vec![
                ("C", "1".into()),
                ("R", "1".into()),
                ("C", "2".into()),
                ("R", "2".into()),
                ("C", "3".into()),
                ("R", "3".into())
            ],
            "屏障两侧的两段各只有一个元素 ⇒ 不并发"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    /// §4.4-5：并发上限 `max_parallel_tools` 被遵守（4 个 safe、上限 2 ⇒ 峰值 ≤ 2）。
    #[tokio::test]
    async fn parallel_respects_max_parallel_tools() {
        let (registry, _, peak) = conc_tool_registry(&[
            ("a", true, 60),
            ("b", true, 60),
            ("c", true, 60),
            ("d", true, 60),
        ]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b"), ("3", "c"), ("4", "d")]);
        let (order, _) = run_batch(&registry, &model, 2).await;
        assert_eq!(peak.load(Ordering::SeqCst), 2, "峰值并发不得超过上限");
        assert_calls_precede_results(&order, 4, 4);
        let mut result_ids = result_ids_in(&order);
        result_ids.sort();
        assert_eq!(result_ids, vec!["1", "2", "3", "4"]);
    }

    /// `max_parallel_tools = 0` 必须被 `.max(1)` 钳成 1。
    ///
    /// 变异：直接传 0 给 `buffer_unordered` ⇒ 流不推进，本用例被超时打红。
    #[tokio::test]
    async fn zero_max_parallel_tools_is_clamped_to_one() {
        let (registry, _, peak) = conc_tool_registry(&[("a", true, 20), ("b", true, 20)]);
        let model = MultiToolModel::new(&[("1", "a"), ("2", "b")]);
        let (order, _) =
            tokio::time::timeout(Duration::from_secs(2), run_batch(&registry, &model, 0))
                .await
                .expect("max_parallel_tools=0 必须被钳成 1，而不是让并行流停住");

        assert_calls_precede_results(&order, 2, 2);
        assert_eq!(peak.load(Ordering::SeqCst), 1, "钳成 1 后只允许单路推进");
    }

    struct NotifyOnToolResult {
        inner: CollectingSink,
        notify: Arc<tokio::sync::Notify>,
        tool_result_id: &'static str,
    }

    impl EventSink for NotifyOnToolResult {
        fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
            if matches!(
                &event,
                AgentEvent::ToolResult { id, .. } if id.0 == self.tool_result_id
            ) {
                self.notify.notify_one();
            }
            self.inner.try_emit(event)
        }

        fn emit<'a>(
            &'a mut self,
            event: AgentEvent,
        ) -> Pin<Box<dyn Future<Output = Result<(), ys_core::EventError>> + Send + 'a>> {
            Box::pin(async move {
                let _ = self.try_emit(event);
                Ok(())
            })
        }
    }

    /// 声明并发安全；带 `release` 的工具会等测试发信号后才完成。
    struct GatedTool {
        name: String,
        release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        is_error: bool,
    }

    #[async_trait::async_trait]
    impl Tool for GatedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                self.name.as_str(),
                "gated conc probe",
                serde_json::json!({}),
            )
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                release
                    .await
                    .map_err(|_| ys_tool::ToolError::Execution("gate dropped".into()))?;
            }
            Ok(CoreToolResult {
                content: if self.is_error {
                    format!("{} failed", self.name)
                } else {
                    format!("{} ok", self.name)
                },
                is_error: self.is_error,
            })
        }
    }

    /// 结果按完成顺序即时发出，但 Step 9c 回喂模型的单条消息仍按原 tool-call 顺序。
    ///
    /// slow 先占住原序第 1 位并等待；fast 完成后发事件，测试看到 fast 的 `ToolResult`
    /// 才释放 slow。若实现退回 `buffered`，fast 事件要等 slow 完成才出现，双方互等至超时。
    #[tokio::test]
    async fn parallel_results_follow_completion_while_session_keeps_call_order() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let registry = ToolRegistry::build(vec![
            Box::new(GatedTool {
                name: "slow".into(),
                release: Mutex::new(Some(release_rx)),
                is_error: false,
            }),
            Box::new(GatedTool {
                name: "fast".into(),
                release: Mutex::new(None),
                is_error: false,
            }),
        ])
        .unwrap();
        let model = MultiToolModel::new(&[("slow", "slow"), ("fast", "fast")]);
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut events = NotifyOnToolResult {
            inner: CollectingSink::new(),
            notify: notify.clone(),
            tool_result_id: "fast",
        };
        let mut session = MemorySession::new();

        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                let run = BasicLoop.run_turn(AgentInput::text("go"), &mut ctx);
                let release = async {
                    notify.notified().await;
                    release_tx.send(()).unwrap();
                };
                let (run_result, ()) = tokio::join!(run, release);
                run_result.unwrap()
            })
            .await
            .expect("fast 的 ToolResult 必须在 slow 完成前发出，否则双方互等");
        }

        let order = call_result_order(&events.inner);
        assert_eq!(
            order,
            vec![
                ("C", "slow".into()),
                ("C", "fast".into()),
                ("R", "fast".into()),
                ("R", "slow".into()),
            ],
            "事件按完成顺序发结果，但全部 ToolCall 仍先发"
        );
        assert_eq!(
            tool_result_pairs(&events.inner),
            vec![
                ("fast".to_string(), "fast ok".to_string()),
                ("slow".to_string(), "slow ok".to_string()),
            ],
            "ToolResult 正文必须与对应 id 匹配，不能按完成顺序错配"
        );
        assert_eq!(
            tool_result_ids_in(&session),
            vec!["slow".to_string(), "fast".to_string()],
            "回喂模型的单条 User 消息必须保持原始 tool-call 顺序"
        );
    }

    /// 并行批中快速成功、慢速失败：快速结果即时发出，错误结果回喂后父回合继续。
    #[tokio::test]
    async fn parallel_error_result_continues_parent_turn() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let registry = ToolRegistry::build(vec![
            Box::new(GatedTool {
                name: "slow-error".into(),
                release: Mutex::new(Some(release_rx)),
                is_error: true,
            }),
            Box::new(GatedTool {
                name: "fast-ok".into(),
                release: Mutex::new(None),
                is_error: false,
            }),
        ])
        .unwrap();
        let model = MultiToolModel::new(&[("slow-error", "slow-error"), ("fast-ok", "fast-ok")]);
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut events = NotifyOnToolResult {
            inner: CollectingSink::new(),
            notify: notify.clone(),
            tool_result_id: "fast-ok",
        };
        let mut session = MemorySession::new();

        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                let run = BasicLoop.run_turn(AgentInput::text("go"), &mut ctx);
                let release = async {
                    notify.notified().await;
                    release_tx.send(()).unwrap();
                };
                let (run_result, ()) = tokio::join!(run, release);
                run_result.unwrap()
            })
            .await
            .expect("并行批中 fast-ok 的结果必须立即发出，不能等待 slow-error 完成")
        };

        let order = call_result_order(&events.inner);
        assert_eq!(
            order,
            vec![
                ("C", "slow-error".into()),
                ("C", "fast-ok".into()),
                ("R", "fast-ok".into()),
                ("R", "slow-error".into()),
            ],
            "快速结果必须先发出，随后才是慢速错误结果"
        );
        assert_eq!(
            tool_result_pairs(&events.inner),
            vec![
                ("fast-ok".to_string(), "fast-ok ok".to_string()),
                ("slow-error".to_string(), "slow-error failed".to_string()),
            ]
        );
        assert_eq!(
            model.round.load(Ordering::SeqCst),
            2,
            "错误结果回喂后必须继续第二次模型调用"
        );
        assert_eq!(result.stop_reason, StopReason::Completed);

        let results = tool_results_in(&session);
        assert_eq!(results.len(), 2);
        assert!(matches!(
            results[0],
            ContentBlock::ToolResult {
                tool_call_id,
                is_error: true,
                ..
            } if tool_call_id.0 == "slow-error"
        ));
        assert!(matches!(
            results[1],
            ContentBlock::ToolResult {
                tool_call_id,
                is_error: false,
                ..
            } if tool_call_id.0 == "fast-ok"
        ));
    }

    struct ErrorThenSuccessModel {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Model for ErrorThenSuccessModel {
        fn model_id(&self) -> &str {
            "error-then-success"
        }
        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if call == 0 {
                ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: ys_core::ToolCallId("err-1".into()),
                            name: "unstable".into(),
                            arguments: serde_json::json!({}),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::ToolCalls,
                }
            } else {
                ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "recovered".into(),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                }
            })
        }
    }

    struct AlwaysFailTool;

    #[async_trait::async_trait]
    impl Tool for AlwaysFailTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("unstable", "always errors", serde_json::json!({}))
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            Err(ys_tool::ToolError::Execution("boom".into()))
        }
    }

    /// 工具错误结果回喂后父回合继续；取消只由父边界决定。
    #[tokio::test]
    async fn error_tool_result_continues_parent_turn() {
        let registry = ToolRegistry::build(vec![Box::new(AlwaysFailTool)]).unwrap();
        let model = ErrorThenSuccessModel {
            calls: AtomicUsize::new(0),
        };
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            2,
            "错误结果后必须再调模型"
        );
        assert_eq!(result.stop_reason, StopReason::Completed);
        let results = tool_results_in(&session);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
        assert!(
            result
                .final_message
                .as_ref()
                .is_some_and(|m| content_has_text(m, "recovered")),
            "第二次模型调用应正常收尾"
        );
    }

    /// N8：工具**之间** abort ⇒ 不再发起下一个工具。
    ///
    /// 场景：模型一轮给出两个工具调用；第一个工具执行时向边界投 `Abort` ⇒
    /// 第二个工具**不得**被执行（探针在 emit `ToolCall` 之前 ⇒ 连事件都不发）。
    ///
    /// 变异靶：把 `basic.rs` 串行路径里的 abort 探针删掉 ⇒ 本用例变红。
    #[tokio::test]
    async fn abort_between_tools_stops_the_next_tool() {
        struct AbortingTool {
            name: String,
            calls: Arc<AtomicUsize>,
            boundary: Arc<ys_protocol::QueueBoundarySource>,
        }

        #[async_trait::async_trait]
        impl Tool for AbortingTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec::new(self.name.as_str(), "aborts the turn", serde_json::json!({}))
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: ToolContext<'_>,
            ) -> Result<CoreToolResult, ys_tool::ToolError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.boundary.push(Boundary::Abort);
                Ok(CoreToolResult {
                    content: "aborted".into(),
                    is_error: false,
                })
            }
        }

        let boundary = Arc::new(ys_protocol::QueueBoundarySource::new());
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::build(vec![
            Box::new(AbortingTool {
                name: "aborting".into(),
                calls: first_calls.clone(),
                boundary: boundary.clone(),
            }),
            Box::new(CountingNamedTool {
                name: "second".into(),
                calls: second_calls.clone(),
            }),
        ])
        .unwrap();

        let model = MultiToolModel::new(&[("aborting", "aborting"), ("second", "second")]);
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            )
            .with_boundary(boundary.clone());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(
            first_calls.load(Ordering::SeqCst),
            1,
            "第一个工具应执行一次"
        );
        assert_eq!(
            second_calls.load(Ordering::SeqCst),
            0,
            "abort 之后不得再发起下一个工具"
        );
        assert_eq!(result.stop_reason, StopReason::Cancelled);
        // 探针在 emit `ToolCall` **之前** ⇒ 被中止的那次调用连事件都不发。
        let second_call_events = events
            .events()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::ToolCall { call } if call.name == "second"
                )
            })
            .count();
        assert_eq!(
            second_call_events,
            0,
            "被中止的工具不得发出 ToolCall 事件，实际事件：{:?}",
            events.events()
        );
    }

    /// 名字可配的计数工具（注册表里的实例与测试持有的计数器必须是**同一个**）。
    struct CountingNamedTool {
        name: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Tool for CountingNamedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                self.name.as_str(),
                "counts invocations",
                serde_json::json!({}),
            )
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CoreToolResult {
                content: "ok".into(),
                is_error: false,
            })
        }
    }

    /// 父边界在入口已取消：零模型调用，且只发一条 `RunFinished{Cancelled}`。
    #[tokio::test]
    async fn parent_boundary_pre_cancel_makes_zero_model_calls() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = CountingModel {
            calls: Mutex::new(0),
        };
        let boundary = ys_protocol::QueueBoundarySource::new();
        boundary.push(Boundary::Abort);
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            )
            .with_boundary(std::sync::Arc::new(boundary));
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(*model.calls.lock().unwrap(), 0, "预取消不得调用模型");
        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(result.rounds, 0);
        assert!(session.messages().is_empty());
        let terminals: Vec<_> = events
            .events()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
                )
            })
            .collect();
        assert_eq!(terminals.len(), 1);
        assert!(matches!(
            terminals[0],
            AgentEvent::RunFinished {
                stop_reason: StopReason::Cancelled,
                rounds: 0,
                ..
            }
        ));
    }

    /// 模型恒返回 `ModelError::Cancelled`（可带 partial）—— 用于钉住「中止不是故障」这条分流。
    ///
    /// `partial` 为 `None` 时模拟「中止瞬间尚无可见输出」；`Some(..)` 时模拟
    /// 「已收到部分文本后被中止」。
    struct CancellingModel {
        partial: Option<Message>,
        calls: AtomicUsize,
    }

    impl CancellingModel {
        fn with_partial(text: &str) -> Self {
            Self {
                partial: Some(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: text.into() }],
                }),
                calls: AtomicUsize::new(0),
            }
        }

        fn without_partial() -> Self {
            Self {
                partial: None,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Model for CancellingModel {
        fn model_id(&self) -> &str {
            "cancelling"
        }
        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ModelError::Cancelled {
                partial: self.partial.clone(),
            })
        }
    }

    /// N4：模型返回 `Cancelled` ⇒ 回合以 `RunFinished{Cancelled}` 收场，**不发** `RunFailed`。
    ///
    /// 「中止不是故障」是本设计的核心区分：若落回通用 `Err` 分支，消费端会看到
    /// `RunFailed`（红色失败），与「用户主动取消」的预期矛盾。
    ///
    /// 变异靶：把 `basic.rs` 里的 `Cancelled` 分流删掉（落回通用 `Err`）⇒ 本用例变红。
    #[tokio::test]
    async fn model_cancelled_finishes_with_cancelled_and_never_run_failed() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = CancellingModel::without_partial();
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert!(
            !events
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::RunFailed { .. })),
            "中止不是故障 ⇒ 不得发 RunFailed，实际事件：{:?}",
            events.events()
        );
        let terminals: Vec<_> = events
            .events()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
                )
            })
            .collect();
        assert_eq!(terminals.len(), 1, "终局事件恰好一条");
        assert!(matches!(
            terminals[0],
            AgentEvent::RunFinished {
                stop_reason: StopReason::Cancelled,
                ..
            }
        ));
    }

    /// **顺序可观测的 session / sink 替身**。
    ///
    /// `Session::append` 与 `EventSink::try_emit` 往**同一个**顺序日志里追加标记，
    /// 于是「partial 落库」与「`RunFinished` 发出」的先后可直接读出。
    struct OrderedLog {
        entries: Mutex<Vec<String>>,
    }

    impl OrderedLog {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entries: Mutex::new(Vec::new()),
            })
        }

        fn entries(&self) -> Vec<String> {
            self.entries.lock().expect("顺序日志锁").clone()
        }
    }

    /// 记录 append 时刻的会话内容是否已含指定文本。
    struct OrderedSession {
        inner: MemorySession,
        log: Arc<OrderedLog>,
        marker: String,
    }

    #[async_trait::async_trait]
    impl Session for OrderedSession {
        fn messages(&self) -> &[Message] {
            self.inner.messages()
        }

        async fn append(&mut self, message: Message) -> Result<(), ys_session::SessionError> {
            let has_marker = message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if *text == self.marker));
            self.log.entries.lock().expect("顺序日志锁").push(format!(
                "append:{}",
                if has_marker { "partial" } else { "other" }
            ));
            self.inner.append(message).await
        }
    }

    /// 记录终局事件发出时刻，日志里此刻是否已含 partial。
    struct OrderedSink {
        log: Arc<OrderedLog>,
        events: Vec<AgentEvent>,
    }

    impl EventSink for OrderedSink {
        fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
            if matches!(event, AgentEvent::RunFinished { .. }) {
                self.log
                    .entries
                    .lock()
                    .expect("顺序日志锁")
                    .push("RunFinished".to_string());
            }
            self.events.push(event);
            Ok(())
        }

        fn emit<'a>(
            &'a mut self,
            event: AgentEvent,
        ) -> Pin<Box<dyn Future<Output = Result<(), ys_core::EventError>> + Send + 'a>> {
            Box::pin(async move {
                let _ = self.try_emit(event);
                Ok(())
            })
        }
    }

    /// N5：模型返回 `Cancelled { partial: Some(msg) }` ⇒ 该段文本**落进会话历史**，
    /// 且**先落库、再收场**。
    ///
    /// 变异的反面：partial 是用户已经付费拿到的内容，丢掉它等于白烧 token。
    ///
    /// 顺序同样是契约：若先发 `RunFinished`、后 append，消费端可能已认为回合结束
    /// ⇒ 部分输出丢失（ADR-0046「先追加部分输出、再收场」）。
    ///
    /// 变异靶：把 `basic.rs` 里「先 append partial 再收场」的顺序颠倒
    /// （先 `RunFinished` 后 append）⇒ 本用例变红。
    #[tokio::test]
    async fn model_cancelled_partial_is_appended_to_session() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = CancellingModel::with_partial("半句话");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert!(
            session
                .messages()
                .iter()
                .any(|m| content_has_text(m, "半句话")),
            "partial 必须落进会话历史，实际会话：{:?}",
            session.messages()
        );
        // `final_message` 同样携带那段文本 —— 消费端据此显示「已产出什么」。
        assert!(
            result
                .final_message
                .as_ref()
                .is_some_and(|m| content_has_text(m, "半句话")),
            "final_message 应是 partial，实际：{:?}",
            result.final_message
        );
    }

    /// N5′：**先 append partial、再发 `RunFinished`** —— 顺序不可颠倒。
    ///
    /// 契约来源：ADR-0046「先追加部分输出、再收场」。若顺序反过来，消费端可能
    /// 已因 `RunFinished` 判定回合结束 ⇒ 随后的 partial 无人接收，白丢已付费的输出。
    ///
    /// 变异靶：把 `basic.rs` 里「先 append partial 再收场」的顺序颠倒
    /// （先 `RunFinished` 后 append）⇒ 本用例变红。
    #[tokio::test]
    async fn model_cancelled_appends_partial_before_run_finished() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = CancellingModel::with_partial("半句话");
        let log = OrderedLog::new();
        let mut session = OrderedSession {
            inner: MemorySession::new(),
            log: log.clone(),
            marker: "半句话".to_string(),
        };
        let mut events = OrderedSink {
            log: log.clone(),
            events: Vec::new(),
        };
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(3),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        let entries = log.entries();
        let partial_at = entries
            .iter()
            .position(|e| e == "append:partial")
            .unwrap_or_else(|| panic!("partial 未落库，顺序日志：{:?}", entries));
        let finished_at = entries
            .iter()
            .position(|e| e == "RunFinished")
            .unwrap_or_else(|| panic!("未发出 RunFinished，顺序日志：{:?}", entries));
        assert!(
            partial_at < finished_at,
            "partial 必须**先**落库、再发 RunFinished（顺序日志：{:?}）",
            entries
        );
    }

    /// N6：取消后**不再发起下一次模型调用**（模型调用计数 == 1）。
    ///
    /// 「取消」若只收场当前轮、循环继续，就会再调一次模型 —— 违背用户意图，
    /// 也白烧 token。
    ///
    /// 变异靶：把 `Cancelled` 分支的 `return` 去掉（落回继续循环）⇒ 本用例变红。
    #[tokio::test]
    async fn model_cancelled_stops_after_first_model_call() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = CancellingModel::without_partial();
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "取消后不得再发起模型调用"
        );
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
            events.events().iter().any(|e| matches!(
                e,
                AgentEvent::ResponseTruncated {
                    dropped_tool_calls: 1
                }
            )),
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

    /// N2：**续写一次仍被截断** ⇒ 回合以 `StopReason::Truncated` **诚实**收场（不是 `Completed`）。
    ///
    /// 第一次纯文本截断会触发 Step 8b 的续写（见 N2b）；本用例给两次截断，验证「只续一次」
    /// 的封顶：第二次截断落下去如实收场，绝不伪装成 `Completed`。
    ///
    /// 变异自检（已实测）：把分支里两处 `StopReason::Truncated` 改回 `Completed`
    /// ⇒ `result.stop_reason` 与 `RunFinished.stop_reason` 两条断言同时变红；
    /// 把 `ResponseTruncated` 的 emit 挪到 `RunFinished` **之后** ⇒ 顺序断言变红。
    /// 把「只续一次」的一次性标志去掉（无条件续写）⇒ 本用例因 `MockModel` 空队列 panic
    /// （第 3 次调用无响应）而变红。
    #[tokio::test]
    async fn truncated_text_twice_ends_turn_as_truncated() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = MockModel::new("m");
        model.push_truncated_text("half an answer");
        model.push_truncated_text("still too long");
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
            "续写一次仍被截断**不得**伪装成 Completed"
        );
        assert_eq!(
            result.rounds, 2,
            "第一次截断续写一轮、第二次截断收场 ⇒ 共 2 轮"
        );
        // 截断的文本仍作为 final_message 交出去（半句答案总比没有好，但不能标成完成）
        assert!(
            result
                .final_message
                .as_ref()
                .is_some_and(|m| content_has_text(m, "still too long")),
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
            events.events().iter().any(|e| matches!(
                e,
                AgentEvent::ResponseTruncated {
                    dropped_tool_calls: 0
                }
            )),
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

    /// N2b（核心回归）：纯文本被截断 ⇒ 回合**继续**（回喂续写提示），而不是就地终结。
    ///
    /// 场景：第 1 轮模型吐半句回答即触顶（`finish_reason = Length`）；循环回喂一条续写催促，
    /// 第 2 轮（`push_text("second half")`）被消费并把任务做完。
    ///
    /// 这是用户报告故障的锁：「一撞截断就停、没完成预期任务」。变异自检（已实测）：
    /// 把 Step 8b 的空 `tool_calls` 分支改回**无条件** `return StopReason::Truncated`
    /// ⇒ `rounds == 1`、`stop_reason == Truncated` ⇒ 本用例三条断言全部变红。
    #[tokio::test]
    async fn truncated_text_continues_once_then_completes() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = MockModel::new("m");
        model.push_truncated_text("first half");
        model.push_text("second half"); // 只有循环继续才会被消费
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

        let all_text: String = session
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");

        assert_eq!(
            result.rounds, 2,
            "截断后应继续调模型一轮：session={all_text:?}"
        );
        assert_eq!(
            result.stop_reason,
            StopReason::Completed,
            "续写完成后应判 Completed：session={all_text:?}"
        );
        assert!(
            all_text.contains("second half"),
            "续写必须落地：session={all_text:?}"
        );
        assert!(
            session.messages().iter().any(|m| {
                m.role == Role::User
                    && m.content.iter().any(|b| {
                        matches!(
                            b,
                            ContentBlock::Text { text } if text == TRUNCATED_CONTINUE_PROMPT
                        )
                    })
            }),
            "会话里必须有续写催促消息（模型据此知道要接着做）：{all_text:?}"
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

    // ==================== 空 / 纯思考响应 ⇒ NoOutput ====================

    /// **N6**：模型返回「无正文、无工具调用」⇒ 回合以 `StopReason::NoOutput` 收场（**不是**
    /// `Completed`），**终局事件同值**；空白字符响应同样算空。
    ///
    /// 第二批起增加「空响应重试一次」：故这里喂**两次**空响应 —— 第一次触发催促，
    /// 第二次仍空才收为 `NoOutput`（`rounds == 2`）。
    ///
    /// 变异：把分支改回 `StopReason::Completed` ⇒ 两个断言同时变红。
    /// 变异：把 `text_content.trim().is_empty()` 换回 `text_content.is_empty()` ⇒ 空白用例（payload `"   "`）变红。
    /// 变异：把空产出分支的催促重试整段删掉（首次空即收场）⇒ `rounds == 1` ⇒ 本用例变红。
    #[tokio::test]
    async fn empty_response_ends_with_no_output_not_completed() {
        for payload in ["", "   "] {
            let registry = ToolRegistry::build(vec![]).unwrap();
            let model = MockModel::new("m");
            // 喂两次空 ⇒ 第一次催促、第二次仍空。
            model.push_text(payload);
            model.push_text(payload);
            let mut session = MemorySession::new();
            let mut events = CollectingSink::new();
            let result = {
                let mut ctx = make_ctx(
                    &model,
                    &registry,
                    &mut session,
                    &mut events,
                    RunLimits::new(5),
                );
                BasicLoop
                    .run_turn(AgentInput::text("go"), &mut ctx)
                    .await
                    .unwrap()
            };

            assert_eq!(
                result.rounds, 2,
                "空响应重试一次 ⇒ rounds == 2（payload={payload:?}）"
            );
            assert_eq!(
                result.stop_reason,
                StopReason::NoOutput,
                "payload={payload:?} 必须收为 NoOutput"
            );
            assert!(
                result.final_message.is_none(),
                "空产出不得有 final_message（payload={payload:?}）"
            );

            let finished = events.events().iter().find_map(|e| match e {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            });
            assert_eq!(
                finished,
                Some(StopReason::NoOutput),
                "终局事件的 stop_reason 必须同值（payload={payload:?}）"
            );
        }
    }

    // ==================== 空响应重试（第二批） ====================

    /// N12：首轮空 + 次轮正常 ⇒ `Completed`，会话里出现催促文案，`rounds == 2`。
    ///
    /// 变异：把空产出分支的 `continue`（催促重试）删掉 ⇒ 首轮空即收场，`rounds == 1`、
    /// `stop_reason == NoOutput` ⇒ 本用例变红。
    #[tokio::test]
    async fn empty_then_normal_completes_after_one_retry() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = MockModel::new("m");
        model.push_text("");
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::Completed);
        assert_eq!(result.rounds, 2, "重试消耗一个 round");
        assert!(result.final_message.is_some(), "次轮有正文 ⇒ final_message");
        // 会话里出现催促消息（Role::User + 文案含 "no visible output"）。
        assert!(
            session.messages().iter().any(|m| {
                m.role == Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("no visible output")))
            }),
            "应留下催促消息；session = {:?}",
            session.messages()
        );
    }

    /// N14：`retry_empty_response = false` ⇒ 首轮空即 `NoOutput`，`rounds == 1`
    /// （与第一批行为逐字节一致）。
    ///
    /// 变异：忽略 `retry_empty_response` 开关、始终重试 ⇒ 本用例 `rounds == 2` ⇒ 变红。
    #[tokio::test]
    async fn empty_response_no_retry_when_disabled() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = MockModel::new("m");
        model.push_text("");
        // 故意再备一条：若实现错误地重试，会消费它并得到 Completed。
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(5);
        limits.retry_empty_response = false;
        let result = {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.stop_reason, StopReason::NoOutput);
        assert_eq!(result.rounds, 1, "关闭重试 ⇒ 一轮即收场");
    }

    // ==================== 回合级工具输出总量闸门（第二批） ====================

    /// 逐字大结果：内容确定，便于逐字节断言（200 行）。
    fn big_result_content() -> String {
        (0..200)
            .map(|i| format!("line {i:04} ................................................"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    struct BigResultTool;

    #[async_trait::async_trait]
    impl Tool for BigResultTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("big", "returns a large result", serde_json::json!({}))
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            Ok(CoreToolResult {
                content: big_result_content(),
                is_error: false,
            })
        }
    }

    /// 收集会话里所有 `ToolResult` 块的内容（按出现顺序）。
    fn tool_result_contents(session: &MemorySession) -> Vec<String> {
        session
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    /// N10（接线层）：本回合**多条**大工具结果超预算 ⇒ 降级更旧的、保留最新；
    /// 消息条数不变，最新一条逐字原样。
    ///
    /// 变异：删掉 Step 11 的 `replace_all` 整段 ⇒ 旧结果不含指针标记 ⇒ 变红；
    /// 把「最新一条永不降级」保护删掉 ⇒ 最新一条含标记 ⇒ 变红。
    #[tokio::test]
    async fn turn_budget_elides_older_results_in_wiring() {
        let registry = ToolRegistry::build(vec![Box::new(BigResultTool)]).unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("big", serde_json::json!({}));
        model.push_tool_call("big", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(5);
        limits.max_turn_tool_result_tokens = 1; // 极小 ⇒ 必触发降级
        let msg_count_before_turn = session.messages().len();
        {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }

        let contents = tool_result_contents(&session);
        assert_eq!(
            contents.len(),
            2,
            "本回合两条工具结果；contents = {contents:?}"
        );
        assert!(
            contents[0].contains(crate::turn_budget::ELIDED_MARKER),
            "更旧的一条应被降级（含指针标记）"
        );
        assert!(
            contents[0].len() < big_result_content().len(),
            "降级后应真的变小"
        );
        assert!(
            !contents[1].contains(crate::turn_budget::ELIDED_MARKER),
            "最新一条永不降级"
        );
        assert_eq!(contents[1], big_result_content(), "最新一条逐字原样");
        // 消息条数 = 本轮新增（user + 2*(assistant+toolresult) + 收尾 assistant = 6），
        // 不因降级增删。
        assert_eq!(
            session.messages().len(),
            msg_count_before_turn + 6,
            "降级只缩内容、不增删消息"
        );
    }

    /// N11：`max_turn_tool_result_tokens = 0`（默认，闸门关闭）⇒ 会话逐字节不变 ——
    /// 无指针标记、工具结果内容原样。
    ///
    /// 变异：让 Step 11 在 `budget == 0` 时也降级 ⇒ 内容含标记 / 变小 ⇒ 变红。
    #[tokio::test]
    async fn turn_budget_disabled_leaves_session_untouched() {
        let registry = ToolRegistry::build(vec![Box::new(BigResultTool)]).unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("big", serde_json::json!({}));
        model.push_tool_call("big", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        // RunLimits::new 保持默认 max_turn_tool_result_tokens = 0。
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }

        let contents = tool_result_contents(&session);
        assert_eq!(contents.len(), 2);
        assert!(
            contents
                .iter()
                .all(|c| !c.contains(crate::turn_budget::ELIDED_MARKER)),
            "关闭闸门不得留下指针标记；contents = {contents:?}"
        );
        assert!(
            contents.iter().all(|c| *c == big_result_content()),
            "关闭闸门 ⇒ 内容逐字节原样"
        );
    }
    // ==================== 外层工具预算：挂进 `ToolContext` ====================
    //
    // 契约：本次调用的**外层预算**（`tool_budget(tool, limits)`）必须挂进
    // `ToolContext` ⇒ 工具内**始终**有 deadline 可依。预算**按工具**算
    // （工具声明 `tool.timeout()` → `limits.bash_timeout` → 300s）。
    // 证据取「工具自己读到的值」—— 挂载点漏了就是 `None`，不会被旁路计算掩盖。

    /// 探针读到的预算记录：(工具名, 工具读到的外层预算)。
    type SeenBudgets = Arc<Mutex<Vec<(String, Option<Duration>)>>>;

    /// 记录「工具在 `ToolContext` 里读到的外层预算」的探针工具。
    struct BudgetProbeTool {
        name: String,
        timeout: Option<Duration>,
        safe: bool,
        seen: SeenBudgets,
    }

    impl BudgetProbeTool {
        fn new(name: &str, timeout: Option<Duration>, safe: bool, seen: SeenBudgets) -> Self {
            Self {
                name: name.to_string(),
                timeout,
                safe,
                seen,
            }
        }
    }

    #[async_trait::async_trait]
    impl Tool for BudgetProbeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                self.name.as_str(),
                "records the outer tool budget it sees",
                serde_json::json!({}),
            )
        }
        fn timeout(&self) -> Option<Duration> {
            self.timeout
        }
        fn is_concurrency_safe(&self) -> bool {
            self.safe
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolContext<'_>,
        ) -> Result<CoreToolResult, ys_tool::ToolError> {
            self.seen
                .lock()
                .unwrap()
                .push((self.name.clone(), ctx.tool_budget));
            Ok(CoreToolResult {
                content: "probe ok".into(),
                is_error: false,
            })
        }
    }

    /// 探针读到的预算（按工具名索引）；工具没被调用过 ⇒ 断言失败。
    fn seen_budget(seen: &SeenBudgets, name: &str) -> Option<Duration> {
        seen.lock()
            .unwrap()
            .iter()
            .find(|(tool_name, _)| tool_name == name)
            .map(|(_, budget)| *budget)
            .expect("该工具应被调用过")
    }

    /// 串行路径：工具在 `ToolContext` 里**读到**外层预算，值 = 该工具自己的
    /// `tool.timeout()`（42s）。
    ///
    /// 变异：删掉串行路径的 `with_tool_budget` ⇒ 探针读到 `None` ⇒ 变红。
    #[tokio::test]
    async fn serial_path_mounts_outer_budget_into_tool_context() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::build(vec![Box::new(BudgetProbeTool::new(
            "probe",
            Some(Duration::from_secs(42)),
            // 非并发安全 ⇒ 单个工具调用走串行路径。
            false,
            seen.clone(),
        ))])
        .unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("probe", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert_eq!(
            seen_budget(&seen, "probe"),
            Some(Duration::from_secs(42)),
            "串行路径的工具应读到外层预算（= 该工具的 timeout 声明值）"
        );
    }

    /// 并行路径：两个并发安全工具**各自**读到**自己**的外层预算（42s / 7s）——
    /// 证明并行批里预算仍**按工具**算，不是整批共用一个值。
    ///
    /// 变异：删掉并行路径的 `with_tool_budget` ⇒ 两个探针都读到 `None` ⇒ 变红；
    /// 把并行路径的预算换成「整批一个值」⇒ 第二个探针的断言变红。
    #[tokio::test]
    async fn parallel_path_mounts_outer_budget_into_tool_context() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::build(vec![
            Box::new(BudgetProbeTool::new(
                "probe_a",
                Some(Duration::from_secs(42)),
                true,
                seen.clone(),
            )),
            Box::new(BudgetProbeTool::new(
                "probe_b",
                Some(Duration::from_secs(7)),
                true,
                seen.clone(),
            )),
        ])
        .unwrap();
        let model = MultiToolModel::new(&[("id-a", "probe_a"), ("id-b", "probe_b")]);
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        // 并行路径的事件形状：全部 `ToolCall` 先于任何 `ToolResult`（串行路径是交错的）。
        let order = call_result_order(&events);
        assert_calls_precede_results(&order, 2, 2);
        assert_eq!(
            seen_budget(&seen, "probe_a"),
            Some(Duration::from_secs(42)),
            "并行路径的工具应读到自己的外层预算"
        );
        assert_eq!(
            seen_budget(&seen, "probe_b"),
            Some(Duration::from_secs(7)),
            "并行批里每个工具的预算按工具算，不是整批共用一个值"
        );
    }

    /// 工具**未声明** `timeout()` ⇒ 外层预算回落到 `limits.bash_timeout`；
    /// `bash_timeout` 也没设 ⇒ 回落到 300s。
    ///
    /// 变异：把 `tool_budget` 改成忽略 `bash_timeout` ⇒ 第一条断言变红。
    #[tokio::test]
    async fn outer_budget_falls_back_to_limits_then_default() {
        // ① 工具未声明 timeout，limits 给了 bash_timeout = 11s。
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::build(vec![Box::new(BudgetProbeTool::new(
            "probe",
            None,
            false,
            seen.clone(),
        ))])
        .unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("probe", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(5);
        limits.bash_timeout = Some(11);
        {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert_eq!(
            seen_budget(&seen, "probe"),
            Some(Duration::from_secs(11)),
            "工具未声明 timeout ⇒ 外层预算回落 limits.bash_timeout"
        );

        // ② 工具未声明 timeout，limits 也没给 bash_timeout ⇒ 300s 兜底。
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::build(vec![Box::new(BudgetProbeTool::new(
            "probe",
            None,
            false,
            seen.clone(),
        ))])
        .unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("probe", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                RunLimits::new(5),
            );
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert_eq!(
            seen_budget(&seen, "probe"),
            Some(Duration::from_secs(300)),
            "工具与 limits 都未声明 ⇒ 外层预算回落 300s"
        );
    }

    /// 源码级钉子：并行与串行**两处**构造点都要挂预算 ⇒ 源码里那条挂载调用的
    /// 出现次数 ≥ 2。
    ///
    /// 探针用的字面量**刻意拆开**（分成两半拼）⇒ 本文件自身不含那条完整调用，
    /// 计数只反映实现处（注释里提到的名字不带调用形态，不计入）。
    ///
    /// 变异：删掉任一处的挂载调用 ⇒ 计数变 1 ⇒ 变红。
    #[test]
    fn both_tool_context_sites_mount_budget() {
        let needle = format!(".with_tool_{}({})", "budget", "budget");
        let count = include_str!("basic.rs").matches(&needle).count();
        assert!(
            count >= 2,
            "并行与串行两处构造点都要挂外层预算；count = {count}"
        );
    }
}
