use agent_core::{Message, StopReason, Usage};

/// Result of a single agent turn
pub struct RunResult {
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub rounds: u32,
    pub final_message: Option<Message>,
}
