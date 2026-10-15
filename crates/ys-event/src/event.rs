use serde::{Deserialize, Serialize};
use ys_core::{Message, StopReason, ToolCall, ToolCallId, ToolResult, Usage};

/// agent run 期间发出的 runtime 级事件。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    UserMessage {
        message: Message,
    },
    /// 一轮开始：每轮**模型调用之前**发出一次。
    ///
    /// **非终局**：终局事件仍只有 `RunFinished` / `RunFailed`，本事件不影响
    /// `-p` / `--json` 消费循环的退出条件。
    ///
    /// `round` 从 `1` 起，与该回合 `RunFinished.rounds` / `RunResult.rounds` **同口径**。
    /// 存在的理由是**回合进行中的可见性**：一个回合可能跑几十轮、每轮数秒，此前消费者
    /// 只能等终局事件才知道「跑到第几轮、还要多久」，UI 因此只能显示一个不动的进度点。
    RoundStarted {
        round: u32,
    },
    ModelTextDelta {
        text: String,
    },
    /// 模型思考内容增量（`reasoning_content`）。由 `Forwarder` 从
    /// `ModelEvent::ThinkingDelta` 映射而来。
    ModelThinkingDelta {
        text: String,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        id: ToolCallId,
        result: ToolResult,
    },
    RunFinished {
        stop_reason: StopReason,
        usage: Usage,
        rounds: u32,
    },
    RunFailed {
        error: String,
    },
    /// 一次自动压缩完成。
    ///
    /// **只发终局、不发「开始」** —— `Started` 会引入悬挂状态：压缩中途被
    /// `Abort` 时终止事件永不发出，transcript 会永远挂着「Compacting…」。
    ///
    /// 字段名里的 `estimated_` 前缀是**有意的**：这两个数来自启发式估算器
    /// （`ys-loop` 的 `token.rs`：ASCII 字节 / 4 + CJK 每字 1.5），
    /// 不是 provider 回报的真实 `usage`，命名不得简写成 `tokens_before/after`。
    ///
    /// 非终局事件：不影响 `-p` / `--json` 的消费循环退出条件。
    ContextCompacted {
        /// 压缩前的**估算** token 数（启发式，非 provider 真实 usage）
        estimated_tokens_before: usize,
        /// 压缩后的**估算** token 数
        estimated_tokens_after: usize,
        /// 被摘要掉的消息条数
        summarized: usize,
        /// 原样保留的消息条数
        kept: usize,
    },
    /// 一次压缩失败（session 未被修改）。
    ContextCompactionFailed {
        reason: String,
    },
    /// 模型输出被截断（provider finish_reason = length/max_tokens）。
    ///
    /// **非终局**：终局事件仍只有 `RunFinished` / `RunFailed`。
    /// 有工具调用时该批**全部不执行**，循环继续；无工具调用时回合以
    /// `StopReason::Truncated` 收场。
    ResponseTruncated {
        /// 被作废、**未执行**的工具调用数（0 = 纯文本被截断）。
        dropped_tool_calls: usize,
    },
}
