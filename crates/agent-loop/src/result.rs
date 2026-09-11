use agent_core::{Message, StopReason, Usage};

/// 单次 agent turn 的结果
pub struct RunResult {
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub rounds: u32,
    pub final_message: Option<Message>,
}
