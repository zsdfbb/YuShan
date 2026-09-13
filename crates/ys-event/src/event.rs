use serde::{Deserialize, Serialize};
use ys_core::{Message, StopReason, ToolCall, ToolCallId, ToolResult, Usage};

/// agent run 期间发出的 runtime 级事件。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    UserMessage {
        message: Message,
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
}
