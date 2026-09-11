use agent_core::Message;

/// 单次 agent turn 的输入
pub struct AgentInput {
    pub message: Message,
}

impl AgentInput {
    pub fn new(message: Message) -> Self {
        Self { message }
    }

    /// 便捷方法：从文本字符串创建
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            message: Message {
                role: agent_core::Role::User,
                content: vec![agent_core::ContentBlock::Text { text: text.into() }],
            },
        }
    }
}
