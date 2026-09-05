use agent_core::Message;

/// Input for a single agent turn
pub struct AgentInput {
    pub message: Message,
}

impl AgentInput {
    pub fn new(message: Message) -> Self {
        Self { message }
    }

    /// Convenience: create from text string
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            message: Message {
                role: agent_core::Role::User,
                content: vec![agent_core::ContentBlock::Text { text: text.into() }],
            },
        }
    }
}
