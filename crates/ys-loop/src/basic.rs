use super::{AgentInput, AgentLoop, LoopError, ModelSummarizer, RunResult};
use crate::compact;
use crate::turn_budget;
use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use ys_component::RuntimeContext;
use ys_core::{
    ContentBlock, Message, Role, StopReason, ThinkingOverride, ToolResult as CoreToolResult, Usage,
};
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

/// 「闷头干活」提醒：本回合连续工具调用已达阀值而模型未输出正文时，主循环把它作为
/// 一条 `User` 消息补进**下一轮的请求**（不落 session）。
///
/// 措辞四要点：
/// 1. **点名事实**（你已经连续调了 N 次工具）—— 模型看不到这个计数，必须告诉它；
/// 2. **点名代价**（每次调用都要一次完整模型往返）—— 否则模型会以为“少说话 = 更快”，
///    而实测工具执行只占墙钟 2.8%，静默省不掉往返（那段 38 条静默实际让用户等了 9.4 分钟）；
/// 3. **限定形态**（**一句话**、不超 120 字符、只说在做什么）—— 与系统提示里那条同源。
///    「一句」比纯字符数更好执行：模型按句组织语言，按字符数反而要先数。
/// 4. **禁止扩写**（不解释理由、不重述已找到的东西）—— 否则提醒会适得其反，
///    变成又一段要反复重读的长文。
const NARRATION_NUDGE_PROMPT: &str = "You have run several tool calls in a row without saying \
     anything to the user. Each call costs a full model round-trip, so silence does not make this \
     faster — it only leaves the user watching nothing happen. Before your next tool call, write \
     one short sentence (at most 120 characters) telling the user what you are doing right now. \
     One sentence only — do not explain your reasoning and do not restate what you already found.";

/// A4 批处理提醒：回合内累计工具调用到阈值、且模型当前仍在「一轮一调用」时，主循环把它
/// 作为一条 `User` 消息补进**下一轮的请求**（不落 session）。
///
/// 纯常量、不做数字插值（ADR-0024「同内容恒同字节」）。只陈述代价、**不规定形状**：
/// 模型每条 `bash` 已平均含 4 个以上 shell 步骤，硬性要求「一条消息发多个调用」会诱导
/// 它把一条管道拆成多次 `bash`。末尾的 `Keep dependent steps separate.` 是必需句，
/// 否则会诱导它对有依赖的步骤做无效批处理。
const BATCH_NUDGE_PROMPT: &str = "Harness hint: this turn has made many tool calls, and most \
     messages carried only one. Every message is a full model round-trip. If the next lookups \
     are independent of each other, cover them in one message or one call — `read` accepts \
     `paths=[...]` and `grep` accepts `paths=[...]`, and independent read-only calls sent \
     together run in parallel. Keep dependent steps separate.";

/// A6 轮数软提醒：回合内轮数到阈值时补进**下一轮请求**（不落 session、不中断回合）。
///
/// 只要求「汇报进展或收敛」，不规定具体动作 —— 到阀的回合可能仍在正常探索，
/// 提醒的作用是让用户看到当前位置，而不是强行终止。
const SOFT_ROUND_ADVICE: &str = "Harness hint: this turn has run for many rounds without \
     converging. If you are still exploring, stop and report: what is done, what is blocked, and \
     what you need from the user. Do not keep probing without telling the user where things stand.";

/// A4 批处理提醒计数推进。
///
/// - 累计本轮工具调用数（**单调，不因某一轮多调用而重置**）。
/// - 到阈值 **且** 当前 `tpr < 1.5` ⇒ 返回 `true` 并把阈值翻倍。
/// - `tpr >= 1.5`（模型已经在批处理）⇒ **不触发、也不推进阈值**；下一轮再判。
///   这正是旧判据的缺陷所在：旧判据按「连续单调用轮」计数，中间夹一轮多调用就清零，
///   实测只触发 4/10 次且多落在回合末尾。
fn advance_batch_nudge(
    calls: usize,
    rounds: u32,
    cumulative: &mut usize,
    threshold: &mut usize,
) -> bool {
    *cumulative = cumulative.saturating_add(calls);
    if *threshold == 0 || *cumulative < *threshold {
        return false;
    }
    // tpr < 1.5  ⇔  cumulative / rounds < 3/2  ⇔  cumulative * 2 < rounds * 3
    // 用 u64 相乘避免溢出；`rounds == 0` 时视为 tpr 0（会触发），但调用点保证 rounds >= 1。
    if (*cumulative as u64) * 2 < (rounds as u64) * 3 {
        *threshold = threshold.saturating_mul(2);
        true
    } else {
        false
    }
}

/// 一次工具调用的超时预算：**工具声明值优先** → 否则 `limits.bash_timeout` → 否则 300s。
///
/// 「优先」而非取 min：`bash_timeout` 管的是「shell 命令多久算卡死」，不该成为长任务
/// 工具（如 subagent 派发）的上限。
fn tool_budget(tool: &dyn Tool, limits: &ys_component::RunLimits) -> Duration {
    tool.timeout()
        .unwrap_or_else(|| Duration::from_secs(limits.bash_timeout.unwrap_or(300)))
}

/// 收卷轮的触发原因。
///
/// 两者都是「到点」：轮数约束 token 成本，墙钟约束时间风险（工具卡住、单轮特别慢）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalReportCause {
    /// 撞 `max_rounds`。
    MaxRounds,
    /// 撞 `wall_clock`。
    WallClock,
}

/// 判定本回合是否**到点**（轮数 / 墙钟）。
///
/// 返回 `None` = 正常跑下一轮；`Some(cause)` = 已到点。
///
/// ⚠️ **本函数与 `final_report_prompt` 无关**：到点与否是硬事实，而「到点后
/// 是收卷还是直接切断」是调用点的策略。两者合并写会把「未配提示词 ⇒ 不判上限」
/// 当成副作用 —— 那会让 `max_rounds` 对未配置的调用方完全失效（回合永不结束）。
fn deadline_reached(
    limits: &ys_component::RunLimits,
    rounds: u32,
    elapsed: Duration,
) -> Option<FinalReportCause> {
    // 轮数先判：两者同时到点时，`MaxRounds` 是更贴切的归因（它才是硬上限）。
    if rounds >= limits.max_rounds {
        return Some(FinalReportCause::MaxRounds);
    }
    match limits.wall_clock {
        // `final_report_grace` 是**给收卷轮自己的**宽限：进入收卷轮后墙钟判定推迟
        // 这么多，否则一个在预算到点前一瞬进入的收卷轮会立刻被判超时、白跑一趟。
        Some(budget) if elapsed >= budget + limits.final_report_grace => {
            Some(FinalReportCause::WallClock)
        }
        _ => None,
    }
}

/// 轮边界拉取的**结果**。
enum BoundaryDrain {
    /// 没有待处理的边界消息。
    Empty,
    /// 至少取到一条 `Steer` / `Deferred` —— 已 append 进会话并发出对应事件。
    /// `Abort` 在这里被消化成 `Cancelled` 收场（见 [`drain_boundary`]）。
    Consumed,
    /// 命中 `Abort`：调用方须以 `Cancelled` 收场。
    Aborted,
}

/// 收场前等待子 agent 的结局（ADR-0055）。
enum SubagentWait {
    /// 有结果/插话到达 ⇒ 调用方应 `continue` 跑下一轮。
    Arrived,
    /// 全部子 agent 都已收尾（可能全部失败 / 被取消）⇒ 可收场。
    Settled,
    /// 等满 [`RunLimits::subagent_wait`] 仍有人在跑 ⇒ 收场（结果稍后经投递表在
    /// 用户下次输入时补投，**不静默** —— app 侧会告知）。
    TimedOut,
    /// 命中 `Abort` ⇒ 调用方以 `Cancelled` 收场。
    Aborted,
}

/// 收场前等还在跑的子 agent 回来（ADR-0055）。
///
/// # 为什么在循环内等，而不是让 app 层再起一个回合
///
/// 只有循环内能把结果变成**同一回合的下一轮**的输入 ⇒ 上下文连续，模型知道刚才
/// 在做哪一步。出了 `run_turn` 就只能新起回合，而那会换掉 `RuntimeContext`
/// （模型看不到刚才派发的句柄与转录路径），且只在 app 那一条入口生效。
///
/// # 为什么不需要单独的 abort 分支
///
/// `Abort` 入队即置 `is_aborted`（`QueueBoundarySource::push`）。本函数返回后调用方
/// 必然 `continue` ⇒ 下一轮开头就是 **Step 3 探针** ⇒ `Cancelled` 收场。
/// 故取消延迟的上界 = 一个 [`RunLimits::subagent_poll`]，而不是 crates 那个上限。
///
/// # 两个唤醒源
///
/// - `settled()`：计数变化时**即时**醒（主路径）；
/// - `subagent_poll` 定时兜底：因为「结果已 push 进边界源」这件事**没有唤醒源**
///   （`QueueBoundarySource::push` 是同步的，不通知任何人），而 abort 也需要一个
///   返回的时机。
async fn wait_for_subagents(
    ctx: &mut RuntimeContext<'_>,
    rounds: u32,
    compaction: &mut Usage,
) -> Result<SubagentWait, LoopError> {
    let started = std::time::Instant::now();
    loop {
        // ① 结果/插话可能已经到了 —— 复用**同一条** drain，不另写注入逻辑。
        match drain_boundary(ctx, rounds, compaction).await? {
            BoundaryDrain::Consumed => return Ok(SubagentWait::Arrived),
            BoundaryDrain::Aborted => return Ok(SubagentWait::Aborted),
            BoundaryDrain::Empty => {}
        }
        // ② 都收尾了 ⇒ 收场（可能全部失败 / 被取消）。
        if !has_running_subagents(ctx) {
            return Ok(SubagentWait::Settled);
        }
        // ③ 到上限。
        if started.elapsed() >= ctx.limits.subagent_wait {
            return Ok(SubagentWait::TimedOut);
        }
        // ④ 等一拍。
        //
        // 剩余时间夹到 poll 粒度：否则 `subagent_wait` 比 `subagent_poll` 还小时
        // 会先多睡一个 poll 才复核上限（多睡的量与配置无关，是可观测的偏差）。
        let poll = ctx
            .limits
            .subagent_poll
            .min(ctx.limits.subagent_wait.saturating_sub(started.elapsed()));
        let tick = tokio::time::sleep(poll.max(Duration::from_millis(1)));
        match ctx.subagent {
            Some(runner) => {
                tokio::select! {
                    _ = runner.settled() => {}
                    _ = tick => {}
                }
            }
            None => tick.await,
        }
    }
}

/// 是否还有在跑的子 agent（无派发能力 ⇒ 恒 `false`）。
fn has_running_subagents(ctx: &RuntimeContext<'_>) -> bool {
    ctx.subagent.is_some_and(|runner| runner.running() > 0)
}

/// 轮边界：拉取中途插话（steering）、后台结果（deferred）、手动压缩请求与中止
/// （abort），保持入队顺序。
///
/// 不增 rounds、不改 turn —— `Steer` 是轮内的额外输入，不是新一轮。
///
/// **收场前也要调用**：模型给出**最终答复**（无工具调用）那条路不经过下一轮，
/// 若不在收场前拉一次，这段窗口内到达的 `Steer` / `Deferred` / `Compact` 会随边界源
/// 一起销毁 —— 用户在模型正要收尾时插的话会**静默消失**。故收场前补一次拉取，
/// 取到 `Steer` / `Deferred` 就**继续本轮循环**（模型下一轮就能看到它），让「轮边界
/// 总是存在」成为真的不变量，而不是「下一轮才有边界」。
///
/// `Abort` 在这里被消化成 `Cancelled` 收场（调用方据 [`BoundaryDrain::Aborted`] 收尾）。
///
/// `rounds` 只用于 `RoundUsage { purpose: Compaction }` 的轮号（与自动压缩同口径）。
async fn drain_boundary(
    ctx: &mut RuntimeContext<'_>,
    rounds: u32,
    compaction: &mut Usage,
) -> Result<BoundaryDrain, LoopError> {
    // **先克隆 `Arc` 再循环**：`Compact` 要 `&mut ctx`（压缩本体写 session / 发事件），
    // 若仍借用 `ctx.boundary` 会与它冲突。克隆只加一次引用计数。
    let Some(boundary) = ctx.boundary.clone() else {
        return Ok(BoundaryDrain::Empty);
    };
    let mut consumed = false;
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
                consumed = true;
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
                consumed = true;
            }
            // 手动 `/compact`：就地压一次。**不设 `consumed`** —— 压缩只缩短会话、
            // 不带来新的模型输入，而 `Consumed` 在三处调用点的语义都是「模型有新东西
            // 要看 ⇒ 继续循环」。设了它会让收场路径凭空多跑一轮空模型调用。
            Boundary::Compact => {
                run_compaction(ctx, rounds, compaction).await?;
            }
            Boundary::Abort => return Ok(BoundaryDrain::Aborted),
        }
    }
    Ok(if consumed {
        BoundaryDrain::Consumed
    } else {
        BoundaryDrain::Empty
    })
}

/// 执行一次压缩：`plan` → `execute` → 写回 → 发事件。**不判触发阈值**（调用方决定）。
///
/// 两个调用方：自动压缩（Step 4b，自己判阈值）与手动 `/compact`
/// （`Boundary::Compact`，用户点了就压）。两者共用这一份实现 ⇒ 事件 / 成本口径 /
/// 失败语义不可能漂移。
///
/// 返回是否真的压了：`false` = `plan()` 返回 `None`（消息太少 / 无合法切点）。
///
/// 失败语义（B-3）：**session 一个字节都不动**，只发 `ContextCompactionFailed`。
async fn run_compaction(
    ctx: &mut RuntimeContext<'_>,
    rounds: u32,
    compaction: &mut Usage,
) -> Result<bool, LoopError> {
    let policy = &ctx.limits.compaction;
    let window = ctx.limits.context_window;
    let fixed = ctx.limits.fixed_prompt_tokens;
    let messages = ctx.session.messages();
    let Some(plan) = compact::plan(messages, policy, window) else {
        return Ok(false);
    };
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
            //
            // 同一笔消耗也进回合账本：`RunFinished.usage` 不含压缩（逐字节承诺），
            // 故必须另立字段，否则 `/status` 与 meta 都会偏小。
            *compaction = compaction.clone() + outcome.usage.clone();
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
            Ok(true)
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
            Ok(false)
        }
    }
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
        // 缓存读数**逐轮**累加：`miss = input − hit` 只在单轮里成立，
        // 而 `total_usage` 的 input 无条件相加、hit 缺席时不参与相加（不对称）。
        // 故绝不能从聚合后的 `total_usage` 派生（见 `RunResult::cache_reading`）。
        let mut cache_reading: Option<(u32, u32)> = None;
        // 压缩消耗**与主对话分开记**：`RunFinished.usage` 不含它（逐字节承诺），
        // 故另立一个累计对象，随 `RunResult` 出去。
        let mut compaction_usage = Usage::default();
        let mut consecutive_errors: HashMap<String, u32> = HashMap::new();
        // 空响应**只重试一次**；本回合内一旦重试过，再空就如实收场。
        let mut empty_retried = false;
        // 纯文本截断后**只续写一次**（Step 8b）：截断没有工具那样的自愈回喂，一个总触顶的
        // 模型无限续写只会空烧 token，故以一次性标志封顶；仍被截断就如实收场。
        let mut truncated_continued = false;

        // 收卷轮状态：`None` = 尚未进入；`Some(reason)` = 本回合已经/正在跑收卷轮。
        //
        // **只进一次**：收卷轮保留原工具表（不破前缀缓存），模型仍可能发起工具调用
        // ⇒ 若无此一次性闸，上限就形同虚设。收卷轮之后无论有无工具调用都强制收场。
        let mut final_report: Option<FinalReportCause> = None;
        // 墙钟起点：回合开始处取一次，之后只读。`wall_clock: None` 时永不使用。
        let turn_started = Instant::now();

        // 「闷头干活」计数：本回合**连续**多少次工具调用而模型未输出正文。
        //
        // 必须由 harness 数：模型看不到自己的累计调用次数，把它写进系统提示词无效
        // （它无法判定何时到阀）。详见 `RunLimits::narration_silence_tool_calls`。
        // 阀值 `0` ⇒ 关闭，计数不增（下面只在 `silent_tool_calls >= 阀值` 时注入）。
        let silence_threshold = ctx.limits.narration_silence_tool_calls;
        let mut silent_tool_calls: usize = 0;
        // 本轮**是否为提醒轮**：`Some` = 上一轮的工具调用已到阀，本轮的请求里要多带
        // 一条「提醒模型说话」的消息。
        //
        // **只影响本轮请求，不落 session** —— 提醒是纠正当前行为的指令，不是对话内容。
        // 若追加进会话，它会像之前的叙述一样被之后**每一轮**重读，反而把想省的成本加回去。
        let mut narration_nudge = false;

        // A4 批处理提醒：本回合**累计**工具调用数（单调，不因某一轮多调用而重置）。
        //
        // 到阈值 **且** 当前 `tpr < 1.5` ⇒ 置 `batch_nudge`，并把阈值翻倍；
        // `tpr >= 1.5` 说明模型已经在批处理，不打扰、也不推进阈值。
        // 阈值 `0` ⇒ 关闭。
        let mut batch_nudge_at = ctx.limits.batch_nudge_at_tool_calls;
        let mut cumulative_tool_calls: usize = 0;
        let mut batch_nudge = false;

        // A6 轮数软提醒：回合内轮数到阈值后，在**下一轮请求**末尾补一条「汇报或收敛」。
        // 每回合重置（本变量就是回合局部状态），触发后阈值 ×2（60 → 120 → 240）。
        // 不产生事件、不落 session、不中断回合；阈值 `0` ⇒ 关闭。
        let mut soft_round_advice_at = ctx.limits.soft_round_advice_at;
        let mut soft_round_advice = false;

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
                cache_reading: None,
                compaction_usage: Usage::default(),
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
                    cache_reading,
                    compaction_usage: compaction_usage.clone(),
                    rounds,
                    final_message: None,
                });
            }

            // Step 4：到点检查（轮数 / 墙钟）。
            //
            // 三条路径，按顺序：
            // 1. **已在收卷轮里** ⇒ 无条件收场。收卷轮保留原工具表以不破前缀缓存，
            //    模型仍可能发起工具调用；若无这道一次性闸，上限就形同虚设。
            // 2. **未到点** ⇒ 正常跑下一轮。
            // 3. **首次到点**：配了 `final_report_prompt` ⇒ 本轮转为收卷轮；
            //    未配（库默认）⇒ **原样切断**，行为与改动前逐字节一致。
            let reached = final_report
                .or_else(|| deadline_reached(&ctx.limits, rounds, turn_started.elapsed()));
            if let Some(cause) = reached {
                // 1 或 3-未配：直接收场。只有「首次到点 **且** 配了提示词」才放行。
                let enter_final_report =
                    final_report.is_none() && ctx.limits.final_report_prompt.is_some();
                if !enter_final_report {
                    let stop_reason = match cause {
                        FinalReportCause::MaxRounds => StopReason::MaxRounds,
                        // 墙钟到点不是「故障」，但也不是用户取消 —— 沿用 `MaxRounds`
                        // 会把归因说错。本仓无专门的预算变体，故取 `Cancelled`（与子 agent
                        // 的 watchdog 同一口径：预算到点 = 取消）。
                        FinalReportCause::WallClock => StopReason::Cancelled,
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
                        cache_reading,
                        compaction_usage: compaction_usage.clone(),
                        rounds,
                        final_message: None,
                    });
                }
                // 首次到点 ⇒ 本轮转为收卷轮，往下走正常的请求组装。
                final_report = Some(cause);
            }

            // Step 4b：上下文压缩检查（T11e）—— 算法本体在 `crate::compact`（ADR-0017 决策 1）。
            //
            // ⚠️ **承重耦合（ADR-0017 决策 1）**：`plan()` 是**纯同步**函数、不碰 model，
            // 且下面的早退必须留在 Step 6 的 `ctx.model.complete` **之前**（Step 4b 本就在
            // 其前，改动时别把它挪到 model 调用之后）。`apps/coding-agent/src/app_loop.rs`
            // 的既有测试 `every_request_variant_is_handled` 会走到这里，它安全**只因为**
            // 会话太小 ⇒ 不满足触发阈值 / `plan()` 返回 `None` ⇒ `MockModel` 的响应队列
            // 不被消费（空队列 `remove(0)` 会 panic）。
            //
            // 这里只判**是否自动触发**；压缩本体与手动 `/compact`（`Boundary::Compact`）
            // 共用 [`run_compaction`] ⇒ 事件 / 成本口径 / 失败语义不可能漂移。
            {
                let policy = &ctx.limits.compaction;
                let window = ctx.limits.context_window;
                // 每回合固定开销（system prompt + 工具表）：每次请求都要重发，会话里
                // 数不到，但真实占着窗口。判定必须把它算进去（U1），否则压缩系统性晚触发。
                let fixed = ctx.limits.fixed_prompt_tokens;
                let estimated = crate::token::estimate_session_tokens(ctx.session.messages());
                let trigger = policy.trigger_at(window);

                // ⚠️ R5 边界：`fixed >= trigger` ⇒ **直接跳过压缩**。
                //
                // 固定开销永远压不掉（压缩只缩会话消息），故此时硬压会同时踩两个坑：
                //   ① 每轮空转 —— 消息已被 `plan()` 的 B-1 压到底（保留段非空），
                //      但 `estimated + fixed` 恒在阈值之上 ⇒ 每轮都压、每轮都不解渴；
                //   ② 小窗口下更糟 —— 压到最简后请求照样溢出，白烧一次摘要调用。
                //
                // **本早退只属于自动路径**：手动 `/compact` 是用户显式要求 ⇒ 不判阈值。
                //
                // 位置约束（承重，ADR-0017 决策 1）：本早退与下面的压缩都必须留在
                // Step 6 的 `ctx.model.complete` **之前**。`apps/coding-agent/src/app_loop.rs`
                // 的 `every_request_variant_is_handled` 会走到这里，它安全**只因为**
                // 不触发 model 调用（`MockModel` 空队列 `remove(0)` 会 panic）。
                if fixed < trigger && estimated + fixed > trigger {
                    run_compaction(ctx, rounds, &mut compaction_usage).await?;
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
            match drain_boundary(ctx, rounds, &mut compaction_usage).await? {
                BoundaryDrain::Empty | BoundaryDrain::Consumed => {}
                BoundaryDrain::Aborted => {
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
                        cache_reading,
                        compaction_usage: compaction_usage.clone(),
                        rounds,
                        final_message: None,
                    });
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
            // A6 轮数软提醒判定：轮数递增之后、请求组装之前。
            // 触发即把阈值 ×2，下一次要到更晚的轮次；`0` = 关闭。
            if soft_round_advice_at > 0 && rounds as usize >= soft_round_advice_at {
                soft_round_advice = true;
                soft_round_advice_at = soft_round_advice_at.saturating_mul(2);
            }
            let tools = ctx.registry.specs().to_vec();
            // 提醒注入：**只影响本轮请求**，不落 session（见 `narration_nudge` 声明处）。
            //
            // 时机：上一轮的工具调用把对应计数推到阀值 ⇒ 本轮开头把一条 `User` 消息
            // 补在消息末尾（照 `TRUNCATED_CONTINUE_PROMPT` 的 `User` 先例）。
            //
            // 优先级固定 `narration > A4 > A6`，**每轮至多一条**；未选中的标志保留到
            // 下一轮（不清零）—— 同时发三条会浪费 token 且互相稀释。
            let mut request_messages = ctx.session.messages().to_vec();
            // 收卷轮指令**优先级最高**：它要模型停止取数、把已有结果写成最终答复。
            // 与三条提醒同形（保留 tools、末尾追加一条 `User`）—— 前缀逐字不变 ⇒ 命中缓存。
            // 同时把三条提醒全部作废：收卷轮里它们只会与「现在交卷」互相稀释。
            if let Some(cause) = final_report {
                let hint = match cause {
                    FinalReportCause::MaxRounds => "the round budget for this task is exhausted",
                    FinalReportCause::WallClock => "the time budget for this task is exhausted",
                };
                request_messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "{}\n\nHarness hint: {hint}. Stop investigating now — do not call any \
                             more tools. Report what you already found: answer the question first, \
                             then list the concrete findings you have (file:line and the fact), and \
                             name anything you could not confirm. This is your final answer.",
                            ctx.limits
                                .final_report_prompt
                                .as_deref()
                                .unwrap_or_default()
                        ),
                    }],
                });
                narration_nudge = false;
                batch_nudge = false;
                soft_round_advice = false;
            } else if narration_nudge {
                request_messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: NARRATION_NUDGE_PROMPT.to_string(),
                    }],
                });
                narration_nudge = false;
            } else if batch_nudge {
                request_messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: BATCH_NUDGE_PROMPT.to_string(),
                    }],
                });
                batch_nudge = false;
            } else if soft_round_advice {
                request_messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: SOFT_ROUND_ADVICE.to_string(),
                    }],
                });
                soft_round_advice = false;
            }
            let request = ModelRequest {
                messages: request_messages,
                tools,
                system: ctx.system_prompt.clone(),
                boundary: ctx.boundary.clone(),
                // 主轮的思考开关。`None`（默认）= 不干预 ⇒ 与改动前逐字节一致。
                //
                // 只影响**开关**，不改强度：强度仍冻在 model 实例里
                // （`OpenAICompatibleConfig.reasoning_effort`）。
                thinking: ctx
                    .limits
                    .main_disable_thinking
                    .filter(|want_off| *want_off)
                    .map(|_| ThinkingOverride::off()),
                ..Default::default()
            };

            // Step 6：通过 Forwarder 调用 model
            //
            // **流中断重试**（`stream_interrupt_retries`，默认 0 = 关闭）：
            // provider 可以在 200 + 正常 ttfb 之后把流中途截断；适配器因
            // 「已吐 delta」拒绝重试（重放会把已渲染文本再渲染一遍）。
            // 那个理由只对「流边渲染给用户看」成立 —— 子 agent 的输出不进屏幕，
            // 故可丢弃部分输出、用**同一请求**重发。会话未受污染（失败分支不
            // `append`），因此重试幂等。
            let mut stream_retries_left = ctx.limits.stream_interrupt_retries;
            let response = loop {
                let mut forwarder = Forwarder { sink: ctx.events };
                match ctx.model.complete(request.clone(), &mut forwarder).await {
                    // 只有 `Provider` 可重试：`Cancelled` 是用户意图，
                    // `Sink` / `Serde` 是本地缺陷 —— 三者重发都无意义。
                    Err(ys_model::ModelError::Provider(_)) if stream_retries_left > 0 => {
                        stream_retries_left -= 1;
                        // 告知消费端：本轮已发出的流式增量作废（重发从头开始）。
                        // 子 agent 只取 `ToolResult`，多余 delta 对它无副作用；
                        // TUI / `-p` 下本开关恒为 0，不会走到这里。
                        emit(ctx.events, AgentEvent::ModelStreamDiscarded)
                            .await
                            .map_err(LoopError::Event)?;
                        continue;
                    }
                    other => break other,
                }
            };
            let response = match response {
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
                        cache_reading,
                        compaction_usage: compaction_usage.clone(),
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
            if let Some((hit, miss)) = round_usage.cache_split() {
                cache_reading = Some(match cache_reading {
                    Some((h, m)) => (h.saturating_add(hit), m.saturating_add(miss)),
                    None => (hit, miss),
                });
            }
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
            // 「闷头干活」计数：模型本轮**是否有正文**决定连续计数是否归零。
            //
            // 放在 Step 8 之前取：本轮的 `tool_calls` 个数随后在 Step 9 执行完才累加。
            // 判定「有正文」用 `!text_content.trim().is_empty()` 而不只看非空 ——
            // 只有空白也算没说话（用户屏幕上没任何东西）。
            let spoke = !text_content.trim().is_empty();

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
                    //
                    // **收场前也要拉一次轮边界** —— 与下面的正常收场路径同理由：
                    // 这段窗口内到达的插话不能随边界源一起销毁。
                    match drain_boundary(ctx, rounds, &mut compaction_usage).await? {
                        BoundaryDrain::Empty => {}
                        // 取到插话 ⇒ 先把它交给模型看（模型还没说完，这正是它该看到的东西）。
                        BoundaryDrain::Consumed => continue,
                        BoundaryDrain::Aborted => {
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
                                cache_reading,
                                compaction_usage: compaction_usage.clone(),
                                rounds,
                                final_message: None,
                            });
                        }
                    }
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
                        cache_reading,
                        compaction_usage: compaction_usage.clone(),
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

                // **收场前再拉一次轮边界** —— 这是「轮边界总是存在」这条不变量成立的地方。
                //
                // 没有这一步，模型在**给出最终答复**这段窗口里到达的 `Steer` / `Deferred`
                // 会随本回合的边界源一起销毁 ⇒ 用户在模型正要收尾时插的话**静默消失**，
                // 且下一个回合也不会带上（那层兼底只排 UI→app 信道，盖不到已 pump 进
                // 边界源的消息）。
                //
                // 取到插话就**继续本轮循环**：模型下一轮就能看到它，用户不必等整个回合结束。
                match drain_boundary(ctx, rounds, &mut compaction_usage).await? {
                    BoundaryDrain::Empty => {
                        // 【ADR-0055】还有子 agent 在跑 ⇒ **不许收场**。
                        //
                        // 不回这个分支，模型会“安恬地做完”并结束回合，而子 agent 的
                        // 结果只能进跨回合投递表、等用户下次敲字 —— 提示词里那句
                        // “结果会在下一回合送到” 在这条路径上是假的。
                        if has_running_subagents(ctx) {
                            match wait_for_subagents(ctx, rounds, &mut compaction_usage).await? {
                                // 有结果/插话到了 ⇒ 把它作为同一回合的下一轮输入。
                                SubagentWait::Arrived => continue,
                                SubagentWait::Aborted => {
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
                                        cache_reading,
                                        compaction_usage: compaction_usage.clone(),
                                        rounds,
                                        final_message: None,
                                    });
                                }
                                // 都收尾了 / 到上限 ⇒ 落到下面的正常收场。
                                SubagentWait::Settled | SubagentWait::TimedOut => {}
                            }
                        }
                    }
                    // 有插话/后台结果 ⇒ 继续本轮，让模型接着看得到它。
                    BoundaryDrain::Consumed => continue,
                    BoundaryDrain::Aborted => {
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
                            cache_reading,
                            compaction_usage: compaction_usage.clone(),
                            rounds,
                            final_message: None,
                        });
                    }
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
                    cache_reading,
                    compaction_usage: compaction_usage.clone(),
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

            // 「闷头干活」计数推进（见 `silent_tool_calls` 声明处）。
            //
            // **只看本轮的 `tool_calls.len()`**，不能用 session 长度差 —— 会话已被压缩 /
            // 降级重写过（Step 4b / Step 11），差值不可靠。
            //
            // 模型本轮说了话 ⇒ 连续计数归零（用户刚听到声音，不欠提醒）。
            // 反之累加；到阀即置 `narration_nudge`，下一轮 Step 5 注入一条（**不落 session**）。
            if silence_threshold > 0 {
                if spoke {
                    silent_tool_calls = 0;
                } else {
                    silent_tool_calls = silent_tool_calls.saturating_add(tool_calls.len());
                    if silent_tool_calls >= silence_threshold {
                        narration_nudge = true;
                        // 阀值**翻倍**：否则模型若又沉默同样多次，每一轮都会挨一条，
                        // 恰好变成我们想消除的唠叨。
                        silent_tool_calls = 0;
                    }
                }
            }

            // A4 批处理提醒计数推进（与 narration 同一处账务，判据独立）。
            //
            // 累计本回合工具调用数（单调、不因多调用轮清零）；到阈值且当前 `tpr < 1.5`
            // 才置标志并把阈值翻倍。下一轮 Step 5 按优先级注入，**不落 session**。
            if batch_nudge_at > 0 {
                batch_nudge |= advance_batch_nudge(
                    tool_calls.len(),
                    rounds,
                    &mut cumulative_tool_calls,
                    &mut batch_nudge_at,
                );
            }

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

    /// 预置 `count` 条各 `chars` 字符的交替消息（每 4 个 ASCII 字节 ≈ 1 token）。
    async fn seed_session_of(session: &mut MemorySession, count: usize, chars: usize) {
        for i in 0..count {
            session
                .append(Message {
                    role: if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    content: vec![ContentBlock::Text {
                        text: "x".repeat(chars),
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
    /// **M9 变异测试**：`cache_reading` 必须**逐轮**累加，不得从聚合 `usage` 派生。
    ///
    /// 构造（两轮）：第 1 轮 `(input=1000, hit=Some(900))`、第 2 轮 `(input=1000, hit=None)`。
    /// 正确（逐轮累加）：`(900, 100)` ⇒ 命中率 90%。
    /// 错误（聚合派生 `input − hit`）：`input=2000, hit=900` ⇒ `(900, 1100)` ⇒ 45%。
    ///
    /// 变异：删掉 `run_turn` 里 `round_usage.cache_split()` 那段累加 ⇒
    /// `cache_reading` 变 `None`（或从聚合派生后变 `(900,1100)`）⇒ 本用例变红。
    #[tokio::test]
    async fn cache_reading_accumulates_per_round_not_from_total() {
        let (spy, calls) = SpyTool::new();
        let registry = ToolRegistry::build(vec![Box::new(spy)]).unwrap();
        let model = UsageModel::new();
        model.push_tool_call(
            "spy",
            serde_json::json!({}),
            Usage {
                input_tokens: 1000,
                output_tokens: 10,
                prompt_cache_hit_tokens: Some(900),
                ..Default::default()
            },
        );
        model.push_text(
            "done",
            Usage {
                input_tokens: 1000,
                output_tokens: 20,
                prompt_cache_hit_tokens: None, // 第 2 轮不报
                ..Default::default()
            },
        );

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
        assert_eq!(calls.load(Ordering::SeqCst), 1, "前置：跑满两轮");

        // 回合累计：两轮 input 相加，hit 只算上报的那轮 —— 这就是「不对称」的来源。
        assert_eq!(
            result.usage.prompt_cache_hit_tokens,
            Some(900),
            "聚合后的 hit 只含上报轮"
        );
        assert_eq!(result.usage.input_tokens, 2000, "聚合后的 input 含两轮");

        assert_eq!(
            result.cache_reading,
            Some((900, 100)),
            "必须逐轮累加 (900, 100)；从聚合 input 派生会错成 (900, 1100)"
        );
    }

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

        // `compaction_usage` 必须**独立**于 `usage` 记账：前者是压缩，后者只含主对话轮。
        // 变异：把 `run_compaction` 里的 `*compaction = ...` 删掉 ⇒ 本条变红（零值）。
        assert_eq!(
            result.compaction_usage, summary_usage,
            "本回合压缩消耗必须记进 RunResult.compaction_usage"
        );
        assert_eq!(
            result.usage, turn_usage,
            "RunResult.usage 不得吞掉压缩消耗（只含主对话轮）"
        );
        assert_ne!(
            result.compaction_usage, result.usage,
            "两者是兄弟字段，压缩时必然不同"
        );

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
                _mode: ys_tool::SubagentMode,
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
            .filter(|&(kind, _)| *kind == "R")
            .map(|(_, id)| id.clone())
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

    // ==================== 收场前的轮边界拉取 ====================

    /// 在**模型调用期间**往边界源投一条 `Steer` 的替身 model。
    ///
    /// 第一次 `complete` 调用时把 `Steer` 推进边界源（模拟「用户在模型正要收尾时插话」），
    /// 之后回**纯文本**（无工具调用）⇒ 走 `tool_calls.is_empty()` 那条**收场**路径。
    struct SteerDuringFinalModel {
        boundary: Arc<ys_protocol::QueueBoundarySource>,
        steer_text: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Model for SteerDuringFinalModel {
        fn model_id(&self) -> &str {
            "steer-during-final"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // 模型**正在生成最终答复**时，用户插话。
                self.boundary.push(Boundary::Steer(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: self.steer_text.clone(),
                    }],
                }));
            }
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "final answer".into(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    /// **收场前必须拉一次轮边界** —— 否则「模型正要收尾时到达的插话」会**静默消失**。
    ///
    /// 故障形态：模型直接给最终答复（无工具调用）那条路**不经过下一轮**，
    /// 若不在收场前拉一次边界源，这段窗口内到达的 `Steer` 会随边界源一起销毁 ——
    /// 用户插的话既不生效、下一回合也不会带上（那层兜底只排 UI→app 信道，
    /// 盖不到已 pump 进边界源的消息）。
    ///
    /// 本用例钉住三件事：
    /// ① 插话**进了会话**（没被销毁）；
    /// ② 回合**继续跑了一轮**（`rounds == 2`）⇒ 模型下一轮就能看到它；
    /// ③ 插话出现在**第二轮**的模型请求里。
    ///
    /// 变异：把收场前的 `drain_boundary(ctx, rounds, &mut compaction_usage).await?` 整段删掉 ⇒
    /// ①「会话里有插话」与 ③「第二轮请求含插话」两条断言同时变红。
    #[tokio::test]
    async fn steer_arriving_during_final_answer_is_not_lost() {
        const MARKER: &str = "STEER-DURING-FINAL";
        let registry = ToolRegistry::build(vec![]).unwrap();
        let boundary = Arc::new(ys_protocol::QueueBoundarySource::new());
        let model = SteerDuringFinalModel {
            boundary: boundary.clone(),
            steer_text: MARKER.to_string(),
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
                RunLimits::new(5),
            )
            .with_boundary(boundary.clone());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert!(
            session
                .messages()
                .iter()
                .any(|m| content_has_text(m, MARKER)),
            "插话必须进了会话（不能随边界源一起销毁）：{:?}",
            session.messages()
        );
        assert_eq!(
            result.rounds, 2,
            "取到插话就继续本轮 ⇒ 模型下一轮就能看到它（不能一轮就收场）"
        );
        assert!(
            events.events().iter().any(|e| matches!(
                e,
                AgentEvent::UserMessage { message }
                    if content_has_text(message, MARKER)
            )),
            "插话必须发出 UserMessage 事件（UI 靠它把本地行标为已生效）：{:?}",
            events.events()
        );
    }

    /// 收场前的边界拉取遇到 `Abort` ⇒ 回合以 `Cancelled` 收场（不是 `Completed`）。
    ///
    /// 变异：把收场前分支里的 `BoundaryDrain::Aborted` 当成 `Empty` 处理 ⇒
    /// `stop_reason` 断言变红。
    #[tokio::test]
    async fn abort_arriving_during_final_answer_cancels_the_turn() {
        struct AbortDuringFinalModel {
            boundary: Arc<ys_protocol::QueueBoundarySource>,
            calls: AtomicUsize,
        }

        #[async_trait]
        impl Model for AbortDuringFinalModel {
            fn model_id(&self) -> &str {
                "abort-during-final"
            }

            async fn complete(
                &self,
                _request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    self.boundary.push(Boundary::Abort);
                }
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "final answer".into(),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                })
            }
        }

        let registry = ToolRegistry::build(vec![]).unwrap();
        let boundary = Arc::new(ys_protocol::QueueBoundarySource::new());
        let model = AbortDuringFinalModel {
            boundary: boundary.clone(),
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
                RunLimits::new(5),
            )
            .with_boundary(boundary.clone());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(
            result.stop_reason,
            StopReason::Cancelled,
            "收场前命中 Abort ⇒ 必须以 Cancelled 收场（不能伪装成 Completed）"
        );
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

    // ── 手动压缩（`Boundary::Compact`）────────────────────────────

    /// 轮边界收到 `Compact` ⇒ **就地压一次**，且本轮照常继续（模型被调用一次）。
    ///
    /// 会话预置成「**大于保留预算、远小于触发阈值**」⇒ 一次同时证明两件事：
    /// ① 压缩真的变小了；② 手动压缩**不判阈值**（自动路径在这个会话上不会触发）。
    ///
    /// 变异：
    /// - 删掉 `drain_boundary` 的 `Boundary::Compact` 臂 ⇒ 不压缩 ⇒ 本用例红；
    /// - 给手动路径套上 `fixed < trigger && estimated + fixed > trigger` ⇒ 本用例红
    ///   （会话未达阈值 ⇒ 不压缩）。
    #[tokio::test]
    async fn boundary_compact_compacts_without_waiting_for_threshold() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = UsageModel::new();
        model.push_text("SUMMARY", Usage::default());
        model.push_text("done", usage_with_reasoning(7));

        // 20 条 × 4000 字符 ≈ 20 × 1000 token = 20000 token。
        // 窗口 100000 ⇒ 保留预算 10000、触发点 85000 ⇒ **不达阈值**但可压。
        let mut session = MemorySession::new();
        seed_session_of(&mut session, 20, 4000).await;

        let boundary = Arc::new(ys_protocol::QueueBoundarySource::new());
        boundary.push(ys_protocol::Boundary::Compact);

        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                limits_with_window(100_000, 0),
            )
            .with_boundary(boundary.clone());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert!(
            has_compacted(&events),
            "轮边界收到 Compact 必须压缩（不看阈值）：{:?}",
            events.events()
        );
        let (before_tokens, after_tokens) = events
            .events()
            .iter()
            .find_map(|e| match e {
                AgentEvent::ContextCompacted {
                    estimated_tokens_before,
                    estimated_tokens_after,
                    ..
                } => Some((*estimated_tokens_before, *estimated_tokens_after)),
                _ => None,
            })
            .expect("必须发 ContextCompacted");
        assert!(
            after_tokens < before_tokens,
            "压缩后估算 token 应变小：~{before_tokens} → ~{after_tokens}"
        );
        assert_eq!(
            compaction_usages(&events).len(),
            1,
            "摘要调用的成本必须按 purpose=Compaction 上报：{:?}",
            events.events()
        );
        assert_eq!(result.rounds, 1, "压缩不带来新输入 ⇒ 只跑本轮这一轮");
    }

    /// **收场前**收到 `Compact` ⇒ 压了，但**不得**多跑一轮。
    ///
    /// 这是「`Compact` 不算 `Consumed`」这条承重决定的钉子：`Consumed` 在三处调用点
    /// 的语义都是「模型有新东西要看 ⇒ 继续循环」，而压缩只缩短会话、不带来新输入。
    /// 若把 `Compact` 算成 `Consumed`，收场路径会 `continue` 出一轮**没有任何新输入**
    /// 的模型调用。
    ///
    /// 构造方式：用自定义模型在**自己的 `complete()` 里**投 `Compact` —— 于是它必定
    /// 落在本次模型调用**之后**、收场前那次 drain 之前，时机确定。
    ///
    /// 变异：把 `drain_boundary` 的 `Compact` 臂改成 `consumed = true` ⇒ 本用例红
    /// （模型被调用两次）。
    #[tokio::test]
    async fn compact_arriving_during_final_answer_does_not_add_a_round() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let boundary = Arc::new(ys_protocol::QueueBoundarySource::new());
        let model = CompactOnCompleteModel {
            boundary: boundary.clone(),
            calls: Mutex::new(0),
        };

        let mut session = MemorySession::new();
        seed_session_of(&mut session, 20, 4000).await;

        let mut events = CollectingSink::new();
        let result = {
            let mut ctx = make_ctx(
                &model,
                &registry,
                &mut session,
                &mut events,
                limits_with_window(100_000, 0),
            )
            .with_boundary(boundary.clone());
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert!(
            has_compacted(&events),
            "收场前收到 Compact 仍必须压缩：{:?}",
            events.events()
        );
        // ⚠️ 不能数模型调用次数：摘要本身也是一次模型调用（`compact::execute`）。
        // 承重的是 `rounds` —— 它是「模型轮」的计数，压缩不占轮。
        assert_eq!(
            result.rounds, 1,
            "收场前收到 Compact 不得凭空多跑一轮：rounds = {}",
            result.rounds
        );
    }

    /// 在自己的 `complete()` 里投 `Boundary::Compact` 的模型：每次调用都回一句终稿。
    ///
    /// 用它可以**确定性地**把 `Compact` 放在「本轮模型调用之后、收场前排空之前」。
    struct CompactOnCompleteModel {
        boundary: Arc<ys_protocol::QueueBoundarySource>,
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Model for CompactOnCompleteModel {
        fn model_id(&self) -> &str {
            "compact-on-complete"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            // **只在第一次调用时投一次**：每次都投会让「收场前压缩」不断产生新的
            // 待压缩请求，若消费端误把 Compact 当 Consumed 就会无限续轮。
            let first = {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                *n == 1
            };
            if first {
                self.boundary.push(ys_protocol::Boundary::Compact);
            }
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

    // ── ADR-0055：收场前等待子 agent ─────────────────────────────────────────

    /// 可控的假 runner：`running` 由测试手推，`settled` 在收场时唤醒等待方。
    #[derive(Default)]
    struct FakeSubagentRunner {
        running: AtomicUsize,
        settled: tokio::sync::Notify,
    }

    impl FakeSubagentRunner {
        fn finish(&self) {
            self.running.fetch_sub(1, Ordering::SeqCst);
            self.settled.notify_waiters();
        }
    }

    #[async_trait]
    impl ys_tool::SubagentRunner for FakeSubagentRunner {
        async fn run(
            &self,
            _prompt: &str,
            _model: Option<&str>,
            _mode: ys_tool::SubagentMode,
            _boundary: Option<&dyn ys_protocol::BoundarySource>,
        ) -> Result<ys_tool::SubagentOutcome, String> {
            Err("not used".into())
        }

        fn running(&self) -> usize {
            self.running.load(Ordering::SeqCst)
        }

        fn settled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(self.settled.notified())
        }
    }

    /// 只答一次、立刻给出最终答复的模型（`calls` 计模型调用次数）。
    struct FinalAnswerModel {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Model for FinalAnswerModel {
        fn model_id(&self) -> &str {
            "final-answer"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "answer".into(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    /// **ADR-0055 承重用例**：子 agent 还在跑 ⇒ 父回合**不许收场**；它回来后
    /// 结果进会话、模型多跑一轮。
    ///
    /// 故障形态（修复前）：模型给出最终答复即收场，子 agent 的结果只能进跨回合
    /// 投递表、等用户下次敲字 ⇒ 提示词里「结果会在下一回合送到」是假话。
    ///
    /// 变异：把 Step 9c 的 `if has_running_subagents(ctx)` 整段删掉 ⇒ 模型只被调
    /// 一次、`rounds == 1`、会话里没有结果 ⇒ 三条断言同时变红。
    #[tokio::test]
    async fn final_answer_waits_for_running_subagents() {
        const RESULT: &str = "SUBAGENT-RESULT-MARKER";
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = FinalAnswerModel {
            calls: AtomicUsize::new(0),
        };
        let runner = Arc::new(FakeSubagentRunner::default());
        runner.running.store(1, Ordering::SeqCst);

        // 子 agent 在「父回合进入等待」之后的某个时刻交付结果。
        let boundary = Arc::new(ys_protocol::QueueBoundarySource::new());
        {
            let runner = runner.clone();
            let boundary = boundary.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                boundary.push(Boundary::Deferred {
                    handle: "sa1".into(),
                    message: Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: RESULT.to_string(),
                        }],
                    },
                });
                // 先交付、再收尾：两件事都可能唤醒等待方。
                runner.finish();
            });
        }

        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let result = {
            let mut limits = RunLimits::new(5);
            limits.subagent_wait = Duration::from_secs(5);
            limits.subagent_poll = Duration::from_millis(10);
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            ctx.boundary = Some(boundary.clone());
            ctx.subagent = Some(runner.as_ref() as &dyn ys_tool::SubagentRunner);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            2,
            "父回合必须等到结果并多跑一轮模型（不能答完就收场）"
        );
        assert_eq!(result.rounds, 2, "等待后继续的那一轮必须计入 rounds");
        assert!(
            session
                .messages()
                .iter()
                .any(|m| content_has_text(m, RESULT)),
            "子 agent 的结果必须进了会话（这就是「主 agent 收到了通知」）：{:?}",
            session.messages()
        );
        assert!(
            events.events().iter().any(|e| matches!(
                e,
                AgentEvent::DeferredResult { handle, .. } if handle == "sa1"
            )),
            "结果注入必须发 DeferredResult 事件（UI 靠它落一行）"
        );
    }

    /// 没有子 agent 在跑 ⇒ **立刻收场**，不白等一个 poll 周期。
    ///
    /// 变异：把 `has_running_subagents` 改成恒 `true` ⇒ 本用例墙钟断言变红
    /// （会睡满 `subagent_wait`）。
    #[tokio::test]
    async fn final_answer_returns_immediately_without_running_subagents() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = FinalAnswerModel {
            calls: AtomicUsize::new(0),
        };
        let runner = Arc::new(FakeSubagentRunner::default()); // running == 0
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let started = std::time::Instant::now();
        let result = {
            let mut limits = RunLimits::new(5);
            limits.subagent_wait = Duration::from_secs(30);
            limits.subagent_poll = Duration::from_millis(200);
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            ctx.subagent = Some(runner.as_ref() as &dyn ys_tool::SubagentRunner);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        assert_eq!(result.rounds, 1, "没有子在跑 ⇒ 一轮就收场");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "不得白等一个 poll 周期（实际 {:?}）",
            started.elapsed()
        );
    }

    /// 等满上限仍有人在跑 ⇒ 如实收场（不是死循环，也不是 panic）。
    ///
    /// 变异：把 `started.elapsed() >= subagent_wait` 的判断挪到循环**外** ⇒
    /// 本用例会一直等到 `subagent_wait` 之后才返回（断言墙钟上界变红），
    /// 或在 `running` 永不为 0 时永久挂起。
    #[tokio::test]
    async fn wait_times_out_and_closes_the_turn() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        let model = FinalAnswerModel {
            calls: AtomicUsize::new(0),
        };
        let runner = Arc::new(FakeSubagentRunner::default());
        runner.running.store(1, Ordering::SeqCst); // 永不 finish
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let started = std::time::Instant::now();
        let result = {
            let mut limits = RunLimits::new(5);
            limits.subagent_wait = Duration::from_millis(80);
            limits.subagent_poll = Duration::from_millis(10);
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            ctx.subagent = Some(runner.as_ref() as &dyn ys_tool::SubagentRunner);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap()
        };

        let elapsed = started.elapsed();
        assert_eq!(result.rounds, 1, "等不到就如实收场，不额外跑模型");
        assert_eq!(
            result.stop_reason,
            StopReason::Completed,
            "模型确实答完了 —— 终止原因不得被等待污染"
        );
        assert!(
            elapsed >= Duration::from_millis(80),
            "必须真的等到了上限（实际 {elapsed:?}）"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "上限必须生效，不能无限等（实际 {elapsed:?}）"
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

    // ==================== 主轮的思考开关（`RunLimits::main_disable_thinking`） ====================

    /// 记录每次 `complete()` 收到的 `ModelRequest.thinking`。
    struct ThinkingProbeModel {
        seen: Mutex<Vec<Option<ThinkingOverride>>>,
    }

    #[async_trait::async_trait]
    impl Model for ThinkingProbeModel {
        fn model_id(&self) -> &str {
            "thinking-probe"
        }
        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            self.seen.lock().unwrap().push(request.thinking.clone());
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

    /// 主轮把 `main_disable_thinking` 落成请求级 `thinking` 覆盖；默认 `None` 不改行为。
    ///
    /// 两半都是承重的：
    ///  - `None`（默认）⇒ 请求里**没有**覆盖 —— 既有消费者的行为逐字节不变；
    ///  - `Some(true)` ⇒ `ThinkingOverride::off()` —— 适配层据此发 `thinking.disabled`。
    ///
    /// 变异：把接入改成无条件 `Some(ThinkingOverride::off())` ⇒ 第一条变红；
    /// 把 `.filter(|want_off| *want_off)` 删掉（`Some(false)` 也关）⇒ 第三条变红。
    #[tokio::test]
    async fn main_round_thinking_override_follows_limits() {
        let registry = ToolRegistry::build(vec![]).unwrap();

        for (setting, expect_off) in [(None, false), (Some(true), true), (Some(false), false)] {
            let model = ThinkingProbeModel {
                seen: Mutex::new(Vec::new()),
            };
            let mut session = MemorySession::new();
            let mut events = CollectingSink::new();
            let mut limits = limits_with_window(100_000, 0);
            limits.main_disable_thinking = setting;

            let result = {
                let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
                BasicLoop
                    .run_turn(AgentInput::text("go"), &mut ctx)
                    .await
                    .unwrap()
            };
            assert_eq!(result.stop_reason, StopReason::Completed);

            let seen = model.seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "本用例只有一轮主对话");
            if expect_off {
                assert_eq!(
                    seen[0],
                    Some(ThinkingOverride::off()),
                    "setting = {setting:?} ⇒ 主轮必须要求关闭思考"
                );
            } else {
                assert_eq!(
                    seen[0], None,
                    "setting = {setting:?} ⇒ 不得下发任何思考覆盖（默认逐字节不变）"
                );
            }
        }
    }

    // ==================== 「闷头干活」提醒（narration nudge） ====================

    /// 记录**每次收到的 `ModelRequest` 里是否带着提醒消息**的模型。
    ///
    /// 前 `silent_rounds` 轮都回一批工具调用（不输出任何正文 ⇒ 静默计数会累加），
    /// 之后回终稿。断言看的是 `sampled` —— 每次 `complete()` 收到的请求消息里
    /// 是否包含 `NARRATION_NUDGE_PROMPT`。
    struct NudgeProbeModel {
        /// 前多少轮只发工具调用（不发正文）。
        silent_rounds: usize,
        /// 每轮发几个工具调用。
        calls_per_round: usize,
        round: AtomicUsize,
        /// 每次请求：`messages` 里是否含提醒。
        sampled: Mutex<Vec<bool>>,
    }

    impl NudgeProbeModel {
        fn new(silent_rounds: usize, calls_per_round: usize) -> Self {
            Self {
                silent_rounds,
                calls_per_round,
                round: AtomicUsize::new(0),
                sampled: Mutex::new(Vec::new()),
            }
        }

        fn nudges(&self) -> Vec<bool> {
            self.sampled.lock().unwrap().clone()
        }

        fn nudge_count(&self) -> usize {
            self.nudges().iter().filter(|b| **b).count()
        }
    }

    #[async_trait::async_trait]
    impl Model for NudgeProbeModel {
        fn model_id(&self) -> &str {
            "nudge-probe"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            let saw = request.messages.iter().any(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text == NARRATION_NUDGE_PROMPT),
                )
            });
            self.sampled.lock().unwrap().push(saw);

            let round = self.round.fetch_add(1, Ordering::SeqCst);
            if round < self.silent_rounds {
                let content = (0..self.calls_per_round)
                    .map(|i| ContentBlock::ToolUse {
                        id: ys_core::ToolCallId(format!("n{round}-{i}")),
                        name: "noop".into(),
                        arguments: serde_json::json!({}),
                    })
                    .collect();
                return Ok(ys_model::ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content,
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::ToolCalls,
                });
            }
            Ok(ys_model::ModelResponse {
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

    /// 一个什么也不做的工具 —— 只为让工具调用合法可执行。
    struct NoopTool;

    #[async_trait::async_trait]
    impl Tool for NoopTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                "noop",
                "does nothing",
                serde_json::json!({"type": "object"}),
            )
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ys_core::ToolResult, ys_tool::ToolError> {
            Ok(ys_core::ToolResult {
                content: "ok".into(),
                is_error: false,
            })
        }
    }

    async fn run_nudge(
        model: &NudgeProbeModel,
        threshold: usize,
        max_rounds: u32,
    ) -> MemorySession {
        let registry = ToolRegistry::build(vec![Box::new(NoopTool) as Box<dyn Tool>]).unwrap();
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(max_rounds);
        limits.narration_silence_tool_calls = threshold;
        {
            let mut ctx = make_ctx(model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        session
    }

    /// 连续静默到阀 ⇒ 下一轮请求里多一条提醒；提醒**不落 session**。
    ///
    /// 变异：① 删掉 Step 5 的注入 ⇒ 全 false；
    ///       ② 改成 `>= 0` 无条件注入 ⇒ 第 1 轮就是 true；
    ///       ③ 把提醒 `append` 进 session（而不是只拼进请求）⇒ 最后一条断言变红
    ///          （会话里会出现一条 `NARRATION_NUDGE_PROMPT`，而它会进压缩 / 预算计算）。
    #[tokio::test]
    async fn narration_nudge_fires_at_threshold_without_persisting() {
        // 每轮 1 个调用、阀值 3 ⇒ 第 3 轮结束后到阀，第 4 轮请求里应有提醒。
        let model = NudgeProbeModel::new(6, 1);
        let session = run_nudge(&model, 3, 20).await;
        let nudges = model.nudges();

        assert!(
            nudges.len() > 3,
            "用例要求跑到至少 4 轮才好断言；实际 {nudges:?}"
        );
        assert!(!nudges[0], "第 1 轮不可能带提醒（还没静默过）");
        assert_eq!(
            &nudges[1..3],
            &[false, false],
            "未到阀值前不得提前提醒；实际 {nudges:?}"
        );
        assert!(
            nudges[3],
            "第 3 轮后累计 3 次调用 ⇒ 第 4 轮必须带提醒；实际 {nudges:?}"
        );

        // 提醒**不得**落进会话（否则会被之后每一轮重读，也与压缩 / 预算口径混淆）。
        let persisted = session.messages().iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == NARRATION_NUDGE_PROMPT))
        });
        assert!(!persisted, "提醒只应存在于当轮请求，不得写进会话");
    }

    /// 模型一说话，连续计数就归零 ⇒ 阀值永远到不了，一次都不提醒。
    ///
    /// 形状（阀值 3）：交错发「纯文字轮」与「1 个工具调用轮」——
    /// `T, C, T, C, T, C, T, C, …`。若无归零，连续计数会累加到 3 并触发；
    /// 有归零则每次都被 T 打断、永远不到阀。
    ///
    /// **为何不能直接用「每轮都有正文」的模型测**：那种模型一个工具都不发，
    /// 计数恒为 0 ⇒ 删掉归零逻辑也照样绿（本条用例的第一版就死在这里，
    /// 变异测试才暴露）。所以这里必须让“有正文”与“发工具”两件事**同存**。
    ///
    /// 变异：把「`spoke` ⇒ 归零」删掉（改成只累加）⇒ 本用例变红。
    #[tokio::test]
    async fn narration_nudge_resets_when_model_speaks() {
        let registry = ToolRegistry::build(vec![Box::new(NoopTool) as Box<dyn Tool>]).unwrap();
        let model = SpeakThenCallModel::new(6);
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut limits = RunLimits::new(20);
        limits.narration_silence_tool_calls = 3;
        {
            let mut ctx = make_ctx(&model, &registry, &mut session, &mut events, limits);
            BasicLoop
                .run_turn(AgentInput::text("go"), &mut ctx)
                .await
                .unwrap();
        }
        assert_eq!(
            model.nudge_count(),
            0,
            "每发一次工具前都说过话 ⇒ 连续计数永远被归零，不该出现提醒；实际 {:?}",
            model.nudges()
        );
    }

    /// 「先说一句、再发一个工具调用」，重复 `rounds` 轮，最后一轮收尾。
    struct SpeakThenCallModel {
        rounds: usize,
        round: AtomicUsize,
        sampled: Mutex<Vec<bool>>,
    }

    impl SpeakThenCallModel {
        fn new(rounds: usize) -> Self {
            Self {
                rounds,
                round: AtomicUsize::new(0),
                sampled: Mutex::new(Vec::new()),
            }
        }
        fn nudges(&self) -> Vec<bool> {
            self.sampled.lock().unwrap().clone()
        }
        fn nudge_count(&self) -> usize {
            self.nudges().iter().filter(|b| **b).count()
        }
    }

    #[async_trait::async_trait]
    impl Model for SpeakThenCallModel {
        fn model_id(&self) -> &str {
            "speak-then-call"
        }
        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            let saw = request.messages.iter().any(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text == NARRATION_NUDGE_PROMPT),
                )
            });
            self.sampled.lock().unwrap().push(saw);

            let round = self.round.fetch_add(1, Ordering::SeqCst);
            // 正文与工具调用**同一轮**输出 —— 这是真实模型常见的形态，
            // 也是能区分「归零」与「不归零」的唯一形状。
            let content = if round < self.rounds {
                vec![
                    ContentBlock::Text {
                        text: "checking".into(),
                    },
                    ContentBlock::ToolUse {
                        id: ys_core::ToolCallId(format!("s{round}")),
                        name: "noop".into(),
                        arguments: serde_json::json!({}),
                    },
                ]
            } else {
                vec![ContentBlock::Text {
                    text: "done".into(),
                }]
            };
            let has_tool = round < self.rounds;
            Ok(ys_model::ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content,
                },
                usage: Usage::default(),
                finish_reason: if has_tool {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
            })
        }
    }

    /// 阀值 `0` ⇒ **关闭**：无论静默多少轮都不提醒（既有消费者逐字节不变）。
    ///
    /// 变异：把 `if silence_threshold > 0` 去掉 ⇒ 阀值 0 会让第 1 轮就 `>= 0` 到阀
    ///       （或永远到不了，取决于写法）⇒ 本用例变红。
    #[tokio::test]
    async fn narration_nudge_is_off_at_threshold_zero() {
        let model = NudgeProbeModel::new(8, 2);
        let _ = run_nudge(&model, 0, 20).await;
        assert_eq!(
            model.nudge_count(),
            0,
            "阀值 0 必须完全关闭提醒；实际 {:?}",
            model.nudges()
        );
    }

    // ==================== A4 单调用 / A6 轮数软提醒 ====================

    /// 一次请求里三条提醒各自是否出现。正常每轮至多一条。
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct ReminderPresence {
        narration: bool,
        batch_nudge: bool,
        soft_round: bool,
    }

    impl ReminderPresence {
        fn total(&self) -> usize {
            self.narration as usize + self.batch_nudge as usize + self.soft_round as usize
        }
    }

    fn contains_prompt(message: &Message, prompt: &str) -> bool {
        message
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == prompt))
    }

    /// 记录**每次请求带了哪条提醒**的模型；前 `silent_rounds` 轮各发
    /// `calls_per_round` 个工具调用（不发正文），之后回终稿。
    struct ReminderProbeModel {
        silent_rounds: usize,
        calls_per_round: usize,
        /// 从第几轮（0 起）开始**同时输出一句正文**。默认 `usize::MAX` = 始终不说话。
        /// 用于让 narration 只触发一次（说话会把它归零），而 A4 不受影响。
        speak_from_round: usize,
        round: AtomicUsize,
        sampled: Mutex<Vec<ReminderPresence>>,
    }

    impl ReminderProbeModel {
        fn new(silent_rounds: usize, calls_per_round: usize) -> Self {
            Self {
                silent_rounds,
                calls_per_round,
                speak_from_round: usize::MAX,
                round: AtomicUsize::new(0),
                sampled: Mutex::new(Vec::new()),
            }
        }

        fn speak_from(mut self, round: usize) -> Self {
            self.speak_from_round = round;
            self
        }

        fn sampled(&self) -> Vec<ReminderPresence> {
            self.sampled.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Model for ReminderProbeModel {
        fn model_id(&self) -> &str {
            "reminder-probe"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            self.sampled.lock().unwrap().push(ReminderPresence {
                narration: request
                    .messages
                    .iter()
                    .any(|m| contains_prompt(m, NARRATION_NUDGE_PROMPT)),
                batch_nudge: request
                    .messages
                    .iter()
                    .any(|m| contains_prompt(m, BATCH_NUDGE_PROMPT)),
                soft_round: request
                    .messages
                    .iter()
                    .any(|m| contains_prompt(m, SOFT_ROUND_ADVICE)),
            });

            let round = self.round.fetch_add(1, Ordering::SeqCst);
            if round < self.silent_rounds {
                let mut content: Vec<ContentBlock> = Vec::new();
                if round >= self.speak_from_round {
                    content.push(ContentBlock::Text {
                        text: "checking".into(),
                    });
                }
                content.extend((0..self.calls_per_round).map(|i| ContentBlock::ToolUse {
                    id: ys_core::ToolCallId(format!("r{round}-{i}")),
                    name: "noop".into(),
                    arguments: serde_json::json!({}),
                }));
                return Ok(ys_model::ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content,
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::ToolCalls,
                });
            }
            Ok(ys_model::ModelResponse {
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

    async fn run_reminder_turn(
        model: &ReminderProbeModel,
        session: &mut MemorySession,
        limits: RunLimits,
    ) {
        let registry = ToolRegistry::build(vec![Box::new(NoopTool) as Box<dyn Tool>]).unwrap();
        let mut events = CollectingSink::new();
        let mut ctx = make_ctx(model, &registry, session, &mut events, limits);
        BasicLoop
            .run_turn(AgentInput::text("go"), &mut ctx)
            .await
            .unwrap();
    }

    /// A4 文案契约：只陈述代价、不规定形状，且末尾保留 `Keep dependent steps separate.`。
    ///
    /// 变异：删掉末尾那句 ⇒ 本用例变红（缺了它，提醒会诱导对有依赖的步骤做无效批处理）。
    #[test]
    fn batch_nudge_prompt_contract() {
        assert!(
            BATCH_NUDGE_PROMPT.ends_with("Keep dependent steps separate."),
            "A4 提醒必须以 dependent-steps 约束收尾"
        );
        assert!(BATCH_NUDGE_PROMPT.contains("paths=[...]"));
        assert!(BATCH_NUDGE_PROMPT.starts_with("Harness hint:"));
        assert!(SOFT_ROUND_ADVICE.starts_with("Harness hint:"));
    }

    /// A4 计数规则（纯函数）。
    ///
    /// 覆盖：未到阈值不触发；到阈值且 `tpr < 1.5` 触发并翻倍；到阈值但
    /// `tpr >= 1.5` 不触发且阈值不变；累计单调（多调用轮不清零）；阈值 0 永不触发。
    ///
    /// 变异：① 把累计改成「本轮调用数」⇒ 单调性断言变红；
    ///       ② 删掉 `tpr < 1.5` 判据 ⇒ `tpr >= 1.5` 用例变红；
    ///       ③ 去掉阈值翻倍 ⇒ 翻倍断言变红；
    ///       ④ 删掉 `*threshold == 0` 早退 ⇒ 阈值 0 用例变红。
    #[test]
    fn advance_batch_nudge_rules() {
        // 未到阈值：累计但不触发，阈值不变。
        let mut cumulative = 0;
        let mut threshold = 12;
        assert!(!advance_batch_nudge(5, 3, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 5);
        assert_eq!(threshold, 12, "未到阈值不得推进阈值");

        // 到阈值且 tpr < 1.5：触发并翻倍。cumulative = 12, rounds = 12 ⇒ tpr = 1.0。
        assert!(advance_batch_nudge(7, 12, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 12);
        assert_eq!(threshold, 24, "12 → 24");

        // 到阈值但 tpr >= 1.5：不触发、阈值不变。
        // cumulative = 24, rounds = 16 ⇒ tpr = 1.5（边界，`<` 不成立）。
        let mut cumulative = 22;
        let mut threshold = 24;
        assert!(!advance_batch_nudge(2, 16, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 24);
        assert_eq!(threshold, 24, "已经在批处理时不得推进阈值");

        // 单调性：中间一轮多调用**不清零**累计（旧判据的缺陷回归钉）。
        let mut cumulative = 0;
        let mut threshold = 5;
        assert!(!advance_batch_nudge(3, 1, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 3, "多调用轮也要累计，不得归零");
        assert!(!advance_batch_nudge(1, 2, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 4);
        // rounds 3：tpr = 5/3 ≥ 1.5 ⇒ 累计已到阈值也不触发。
        assert!(!advance_batch_nudge(1, 3, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 5);
        // rounds 4：tpr = 6/4 = 1.5 ⇒ 仍不触发。
        assert!(!advance_batch_nudge(1, 4, &mut cumulative, &mut threshold));
        // rounds 5：tpr = 7/5 = 1.4 < 1.5 ⇒ 触发。
        assert!(advance_batch_nudge(1, 5, &mut cumulative, &mut threshold));
        assert_eq!(cumulative, 7, "累计从未被清零");
        assert_eq!(threshold, 10);

        // 阈值 0 ⇒ 永不触发。
        let mut cumulative = 0;
        let mut threshold = 0;
        assert!(!advance_batch_nudge(
            100,
            1,
            &mut cumulative,
            &mut threshold
        ));
        assert_eq!(threshold, 0);
    }

    /// A4：累计调用到阈值 ⇒ 下一轮请求末尾多一条 `User` 提醒；提醒不落 session；
    /// 阈值翻倍 ⇒ 第二次触发需要更多调用。
    ///
    /// 每轮 1 个调用、阈值 2：第 2 轮结束累计 2 且 tpr = 1 < 1.5 ⇒ 第 3 轮请求带提醒；
    /// 阈值翻倍到 4 ⇒ 第 4 轮结束累计 4 才再次触发（第 5 轮请求）。
    ///
    /// 变异：① 删除 Step 5 的 A4 注入 ⇒ 永不出现；
    ///       ② 去掉阈值翻倍 ⇒ 第 4 轮请求也会带提醒，`!sampled[3].batch_nudge` 变红；
    ///       ③ 把提醒 `append` 进 session ⇒ 长度断言与「会话无提醒」断言同时变红。
    #[tokio::test]
    async fn batch_nudge_fires_at_threshold_and_doubles() {
        let model = ReminderProbeModel::new(10, 1);
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(20);
        limits.batch_nudge_at_tool_calls = 2;
        run_reminder_turn(&model, &mut session, limits).await;

        let sampled = model.sampled();
        assert!(sampled.len() >= 5, "用例需要至少 5 轮；实际 {sampled:?}");
        assert!(!sampled[0].batch_nudge && !sampled[1].batch_nudge);
        assert!(
            sampled[2].batch_nudge,
            "第 2 轮后累计 2 次调用 ⇒ 第 3 轮请求必须带提醒；实际 {sampled:?}"
        );
        assert!(!sampled[3].batch_nudge);
        assert!(
            sampled[4].batch_nudge,
            "阈值已翻倍到 4 ⇒ 第 5 轮再次提醒；实际 {sampled:?}"
        );
        assert!(sampled.iter().all(|p| p.total() <= 1));

        // 22 = 1 条用户消息 + 11 轮 assistant + 10 条工具结果；提醒不落 session。
        assert_eq!(
            session.messages().len(),
            22,
            "提醒只应存在于当轮请求，不得写进会话"
        );
        assert!(
            !session
                .messages()
                .iter()
                .any(|m| contains_prompt(m, BATCH_NUDGE_PROMPT)),
            "A4 提醒不得写进会话"
        );
    }

    /// A4：模型本来就在批处理（每轮 2 个调用，tpr = 2 ≥ 1.5）⇒ 即使累计远超阈值也
    /// **不打扰** —— 此时提醒是错的建议。
    ///
    /// 变异：删掉 `tpr < 1.5` 判据 ⇒ 第 2 轮后就会注入提醒，本用例变红。
    #[tokio::test]
    async fn batch_nudge_skips_when_model_already_batches() {
        let model = ReminderProbeModel::new(10, 2);
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(20);
        limits.batch_nudge_at_tool_calls = 2;
        run_reminder_turn(&model, &mut session, limits).await;

        assert!(
            model.sampled().iter().all(|p| !p.batch_nudge),
            "tpr = 2 ≥ 1.5 ⇒ 模型已在批处理，不得提醒；实际 {:?}",
            model.sampled()
        );
    }

    /// A6：阈值 60 → 120 → 240 各提醒一次；提醒不落 session。
    ///
    /// 变异：去掉阈值 ×2 ⇒ 第 60 轮之后每轮都带提醒，第 61 轮断言变红。
    #[tokio::test]
    async fn soft_round_advice_doubles_at_60_120_240() {
        let model = ReminderProbeModel::new(250, 1);
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(300);
        limits.soft_round_advice_at = 60;
        run_reminder_turn(&model, &mut session, limits).await;

        let sampled = model.sampled();
        assert!(
            sampled.len() >= 240,
            "用例需要跑到第 240 轮；实际 {}",
            sampled.len()
        );
        assert!(!sampled[58].soft_round);
        assert!(sampled[59].soft_round, "第 60 轮必须提醒");
        assert!(!sampled[118].soft_round);
        assert!(sampled[119].soft_round, "第 120 轮必须再次提醒");
        assert!(!sampled[238].soft_round);
        assert!(sampled[239].soft_round, "第 240 轮必须再次提醒");
        assert!(sampled.iter().all(|p| p.total() <= 1));

        assert!(
            !session
                .messages()
                .iter()
                .any(|m| contains_prompt(m, SOFT_ROUND_ADVICE)),
            "A6 提醒只应存在于当轮请求，不得写进会话"
        );
    }

    /// A6：阈值每回合重置 —— 上一回合触发过并翻倍，新回合仍从基础阈值起算。
    ///
    /// 变异：把阈值改成跨回合持久（例如塞进 `RuntimeContext` / session）⇒ 第二回合
    ///       第 2 轮不会提醒，本用例变红。
    #[tokio::test]
    async fn soft_round_advice_resets_each_turn() {
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(20);
        limits.soft_round_advice_at = 2;

        let first = ReminderProbeModel::new(3, 1);
        run_reminder_turn(&first, &mut session, limits.clone()).await;
        assert!(
            first.sampled()[1].soft_round,
            "第一回合第 2 轮必须提醒；实际 {:?}",
            first.sampled()
        );

        let second = ReminderProbeModel::new(2, 1);
        run_reminder_turn(&second, &mut session, limits).await;
        assert!(
            second.sampled()[1].soft_round,
            "新回合阈值必须回到 2（不能沿用上一回合翻倍后的值）；实际 {:?}",
            second.sampled()
        );
    }

    /// R5 注入优先级：三条同时置位 ⇒ 本次请求**只有一条**且是 narration；
    /// A4 / A6 标志顺延到下一轮，顺序稳定为 `narration > A4 > A6`。
    ///
    /// A6 阈值取 2：narration / A4 在第 1 轮结束时置位，A6 在第 2 轮开头置位，
    /// 这样第 2 轮请求才会「三条同时置位」。
    ///
    /// 变异：把 `else if` 链改成三个独立 `if` ⇒ 每轮 `total()` 会 > 1，本用例变红。
    #[tokio::test]
    async fn nudge_priority_prefers_narration_then_batch_then_soft_round() {
        // 第 1 轮沉默（narration 触发一次）；之后每轮说一句（narration 归零，
        // 不再抢优先级），但 A4 只看累计调用数、照常累加。
        let model = ReminderProbeModel::new(10, 1).speak_from(1);
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(20);
        limits.narration_silence_tool_calls = 1;
        limits.batch_nudge_at_tool_calls = 1;
        limits.soft_round_advice_at = 2;
        run_reminder_turn(&model, &mut session, limits).await;

        let sampled = model.sampled();
        assert!(sampled.len() >= 4, "用例需要至少 4 轮；实际 {sampled:?}");
        assert_eq!(sampled[0], ReminderPresence::default());
        assert_eq!(
            sampled[1],
            ReminderPresence {
                narration: true,
                batch_nudge: false,
                soft_round: false,
            },
            "三条同时置位时本轮只能发 narration；实际 {sampled:?}"
        );
        assert_eq!(
            sampled[2],
            ReminderPresence {
                narration: false,
                batch_nudge: true,
                soft_round: false,
            },
            "A4 标志必须保留到下一轮；实际 {sampled:?}"
        );
        assert_eq!(
            sampled[3],
            ReminderPresence {
                narration: false,
                batch_nudge: false,
                soft_round: true,
            },
            "A6 标志必须再顺延一轮；实际 {sampled:?}"
        );
        assert!(
            sampled.iter().all(|p| p.total() <= 1),
            "每轮至多注入一条；实际 {sampled:?}"
        );
    }

    /// 阈值 `0` ⇒ A4 永不触发。
    #[tokio::test]
    async fn batch_nudge_is_off_at_zero() {
        let model = ReminderProbeModel::new(10, 1);
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(20);
        limits.batch_nudge_at_tool_calls = 0;
        run_reminder_turn(&model, &mut session, limits).await;
        assert!(
            model.sampled().iter().all(|p| !p.batch_nudge),
            "阈值 0 必须完全关闭 A4；实际 {:?}",
            model.sampled()
        );
    }

    /// 阈值 `0` ⇒ A6 永不触发。
    #[tokio::test]
    async fn soft_round_advice_is_off_at_zero() {
        let model = ReminderProbeModel::new(10, 1);
        let mut session = MemorySession::new();
        let mut limits = RunLimits::new(20);
        limits.soft_round_advice_at = 0;
        run_reminder_turn(&model, &mut session, limits).await;
        assert!(
            model.sampled().iter().all(|p| !p.soft_round),
            "阈值 0 必须完全关闭 A6；实际 {:?}",
            model.sampled()
        );
    }

    // ── 收卷轮（`final_report_prompt`） ─────────────────────────────

    /// 探针：记录每轮请求里是否带收卷指令、是否仍带着工具表。
    struct FinalReportProbeModel {
        /// 前多少轮只发工具调用（不发正文）。
        tool_rounds: usize,
        round: AtomicUsize,
        /// 每次请求：`(带收卷指令, 工具表非空)`。
        sampled: Mutex<Vec<(bool, bool)>>,
    }

    /// 请求次数硬上限：越过即 panic。
    ///
    /// 存在的理由：本探针恒发工具调用，若上限判定失效（例如删掉收卷轮的一次性闸），
    /// 回合就是**忙循环** —— 外部 `tokio::time::timeout` 抓不住它（future 从不 `Pending`），
    /// 表现为整个测试进程挂住而不是变红。故计数必须在这里做。
    const FINAL_REPORT_PROBE_MAX_CALLS: usize = 64;

    impl FinalReportProbeModel {
        fn new(tool_rounds: usize) -> Self {
            Self {
                tool_rounds,
                round: AtomicUsize::new(0),
                sampled: Mutex::new(Vec::new()),
            }
        }

        fn sampled(&self) -> Vec<(bool, bool)> {
            self.sampled.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Model for FinalReportProbeModel {
        fn model_id(&self) -> &str {
            "final-report-probe"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            // 收卷指令是**拼接文本**（提示词 + 固定尾句），故用 `contains` 而非
            // `contains_prompt` 的精确相等匹配。
            self.sampled.lock().unwrap().push((
                request.messages.iter().any(|m| {
                    m.content.iter().any(|b| {
                        matches!(b, ContentBlock::Text { text } if text.contains(FINAL_REPORT_SENTINEL))
                    })
                }),
                !request.tools.is_empty(),
            ));

            let round = self.round.fetch_add(1, Ordering::SeqCst);
            assert!(
                round < FINAL_REPORT_PROBE_MAX_CALLS,
                "回合跑了 {} 轮仍未收场 —— 到点判定失效（忙循环，超时抓不住）",
                FINAL_REPORT_PROBE_MAX_CALLS
            );
            if round < self.tool_rounds {
                return Ok(ys_model::ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: ys_core::ToolCallId(format!("f{round}")),
                            name: "noop".into(),
                            arguments: serde_json::json!({}),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::ToolCalls,
                });
            }
            Ok(ys_model::ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "report".into(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    /// 收卷指令的**可识别前缀**（与 `final_report_prompt` 的取值一致）。
    const FINAL_REPORT_SENTINEL: &str = "WRAP_UP_SENTINEL";

    /// 跑一个回合。**挂起保护在探针里**（见 [`FinalReportProbeModel::complete`]）。
    ///
    /// 不能用 `tokio::time::timeout` 兜底：本探针的 `complete` 立即返回 `Ready`，
    /// 回合一旦永不收场就是个**忙循环** —— future 从不 `Pending`，定时器永远得不到
    /// 一次 poll，超时不会触发。故上限必须由探针自己数。
    async fn run_final_report(
        model: &FinalReportProbeModel,
        limits: RunLimits,
    ) -> (MemorySession, CollectingSink) {
        let registry = ToolRegistry::build(vec![Box::new(NoopTool) as Box<dyn Tool>]).unwrap();
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        {
            let mut ctx = make_ctx(model, &registry, &mut session, &mut events, limits);
            let _ = BasicLoop.run_turn(AgentInput::text("go"), &mut ctx).await;
        }
        (session, events)
    }

    /// 未配置 `final_report_prompt` ⇒ **原样切断**：不额外跑一轮，收场 `MaxRounds`。
    ///
    /// 这是**回归守卫**：收卷轮特性不得改变未启用它的调用方的行为。
    /// 变异：把 Step 4 的 `enter_final_report` 判定删掉（无条件进入收卷轮）
    /// ⇒ 请求数变成 3 且会注入空提示词。
    #[tokio::test]
    async fn final_report_is_off_by_default() {
        let model = FinalReportProbeModel::new(usize::MAX);
        let (_, events) = run_final_report(&model, RunLimits::new(2)).await;

        assert_eq!(model.sampled().len(), 2, "max_rounds=2 ⇒ 恰好 2 次请求");
        assert!(
            model.sampled().iter().all(|(hint, _)| !*hint),
            "默认不得注入收卷指令；实际 {:?}",
            model.sampled()
        );
        let finished = events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished, vec![StopReason::MaxRounds]);
    }

    /// 配置了收卷提示 ⇒ 到点**多跑一轮**并注入指令，且该轮**保留工具表**（不破前缀缓存）。
    ///
    /// 变异：① 删掉 Step 5 的收卷注入 ⇒ `hint` 全 false；
    ///       ② 把收卷轮改成空工具表 ⇒ 第二列变 false（前缀缓存会被打掉）；
    ///       ③ 去掉「只给一次」的一次性闸 ⇒ 请求数变成 4（收卷轮又调了工具）。
    #[tokio::test]
    async fn final_report_runs_once_and_keeps_tools() {
        let model = FinalReportProbeModel::new(usize::MAX);
        let mut limits = RunLimits::new(2);
        limits.final_report_prompt = Some(FINAL_REPORT_SENTINEL.to_string());
        let (_, events) = run_final_report(&model, limits).await;

        let sampled = model.sampled();
        assert_eq!(sampled.len(), 3, "2 轮 + 1 轮收卷；实际 {sampled:?}");
        assert_eq!(
            &sampled[0..2],
            &[(false, true), (false, true)],
            "前两轮不带收卷指令；实际 {sampled:?}"
        );
        assert_eq!(
            sampled[2],
            (true, true),
            "第 3 轮必须带收卷指令**且保留工具表**（空表会破前缀缓存）；实际 {sampled:?}"
        );

        // 收卷轮之后必须真的收场，不能因为模型又调了工具而无限续。
        let finished = events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished, vec![StopReason::MaxRounds]);
    }

    /// 收卷轮**只给一次**：模型在收卷轮里又发起工具调用，也必须强制收场。
    ///
    /// 变异：把 Step 4 的 `if final_report.is_some()` 分支删掉 ⇒ 本用例超时 / 请求数暴涨。
    #[tokio::test]
    async fn final_report_gives_up_after_one_round_even_if_model_calls_tools() {
        // 永远发工具调用：若不设一次性闸，回合永远不会结束。
        let model = FinalReportProbeModel::new(usize::MAX);
        let mut limits = RunLimits::new(1);
        limits.final_report_prompt = Some(FINAL_REPORT_SENTINEL.to_string());
        let (_, events) = run_final_report(&model, limits).await;

        assert_eq!(
            model.sampled().len(),
            2,
            "1 轮 + 1 轮收卷后必须收场；实际 {:?}",
            model.sampled()
        );
        assert!(model.sampled()[1].0, "第 2 轮是收卷轮");
        let finished = events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished, vec![StopReason::MaxRounds]);
    }

    /// 墙钟到点同样触发收卷轮，且收场归因为 `Cancelled`（预算到点 = 取消，不是故障）。
    #[tokio::test]
    async fn wall_clock_triggers_final_report() {
        let model = FinalReportProbeModel::new(usize::MAX);
        let mut limits = RunLimits::new(u32::MAX);
        limits.final_report_prompt = Some(FINAL_REPORT_SENTINEL.to_string());
        // 零预算 ⇒ 第 1 轮就到点（不必真的等）。
        limits.wall_clock = Some(Duration::ZERO);
        let (_, events) = run_final_report(&model, limits).await;

        let sampled = model.sampled();
        assert_eq!(sampled.len(), 1, "首轮即为收卷轮；实际 {sampled:?}");
        assert_eq!(sampled[0], (true, true), "收卷轮带指令且保留工具表");
        let finished = events
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished, vec![StopReason::Cancelled]);
    }

    /// `deadline_reached` 的纯逻辑：**与是否配提示词无关**；轮数先于墙钟归因。
    ///
    /// ⚠️ 这条「与提示词无关」是**承重**的：曾把「未配提示词」写成该函数的早退，
    /// 后果是 `max_rounds` 对未配置的调用方**完全失效**（回合永不结束）。
    /// 「到点后是收卷还是切断」属于调用点策略，不属本函数。
    ///
    /// 变异：把 `if limits.final_report_prompt.is_none() { return None; }` 加回来
    /// ⇒ 第一个断言变红（配不配提示词都该到点）。
    #[test]
    fn deadline_reached_prefers_max_rounds() {
        let mut limits = RunLimits::new(5);
        assert_eq!(
            deadline_reached(&limits, 99, Duration::from_secs(99)),
            Some(FinalReportCause::MaxRounds),
            "未配置收卷提示时，轮数上限照样必须到点（否则 max_rounds 失效）"
        );
        assert_eq!(
            deadline_reached(&limits, 0, Duration::from_secs(99)),
            None,
            "未配墙钟 ⇒ 时间不参与判定"
        );

        limits.final_report_prompt = Some("x".into());
        limits.wall_clock = Some(Duration::from_secs(10));
        assert_eq!(deadline_reached(&limits, 4, Duration::from_secs(9)), None);
        assert_eq!(
            deadline_reached(&limits, 5, Duration::from_secs(9)),
            Some(FinalReportCause::MaxRounds)
        );
        assert_eq!(
            deadline_reached(&limits, 1, Duration::from_secs(10)),
            Some(FinalReportCause::WallClock)
        );
        // 两者同时到点 ⇒ 归因为轮数（硬上限更贴切）。
        assert_eq!(
            deadline_reached(&limits, 5, Duration::from_secs(10)),
            Some(FinalReportCause::MaxRounds)
        );
        // 宽限把墙钟判定整体推迟。
        limits.final_report_grace = Duration::from_secs(5);
        assert_eq!(deadline_reached(&limits, 1, Duration::from_secs(14)), None);
        assert_eq!(
            deadline_reached(&limits, 1, Duration::from_secs(15)),
            Some(FinalReportCause::WallClock)
        );
    }
}
