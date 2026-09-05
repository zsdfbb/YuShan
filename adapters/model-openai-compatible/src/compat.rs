#[derive(Debug, Clone, Default)]
pub struct ProviderCompat {
    /// DeepSeek: reasoning_content 字段
    pub has_reasoning_content: bool,
    /// MiniMax: tool_calls 可能作为 text 返回
    pub tool_calls_as_text: bool,
}

impl ProviderCompat {
    pub fn deepseek() -> Self {
        Self {
            has_reasoning_content: true,
            ..Default::default()
        }
    }

    pub fn minimax() -> Self {
        Self {
            tool_calls_as_text: true,
            ..Default::default()
        }
    }

    pub fn standard() -> Self {
        Self::default()
    }
}
