use agent_core::{Message, StopReason, ToolCall, ToolCallId, ToolResult, Usage};
use serde::{Deserialize, Serialize};

/// Runtime-level events emitted during an agent run.
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
