use agent_core::Message;
use agent_tool::ToolSpec;
use serde::{Deserialize, Serialize};

/// Request sent to a model for completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

impl Default for ModelRequest {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            tools: Vec::new(),
            system: None,
            max_tokens: None,
            temperature: None,
        }
    }
}

/// Response received from a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: agent_core::Usage,
    #[serde(default)]
    pub stop_reason: Option<String>,
}
