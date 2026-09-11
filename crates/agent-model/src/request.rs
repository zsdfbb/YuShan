use agent_core::Message;
use agent_tool::ToolSpec;
use serde::{Deserialize, Serialize};

/// 发送给 model 以完成 completion 的请求。
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

/// 从 model 收到的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: agent_core::Usage,
    #[serde(default)]
    pub stop_reason: Option<String>,
}
