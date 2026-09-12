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
