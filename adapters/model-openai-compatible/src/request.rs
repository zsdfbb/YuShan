use serde::{Deserialize, Serialize};
use serde_json::Value;

/// OpenAI chat completion 请求体
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// 流式请求的可选项。仅在 provider 声明支持时才置 `Some`——
    /// `None` 时整体不序列化，避免不认识该字段的端点 400。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 推理强度（如 `low` / `high` / `max`）。`None` 时整个字段不序列化，
    /// 保持既有端点行为不变；是否支持由具体 provider / model 决定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// 思考开关。`None` 时整个字段不序列化（由 provider 默认决定）。
    ///
    /// 与 `reasoning_effort` 是**两个独立字段**：本字段管开/关，那个管强度。
    /// 权威口径见 <https://api-docs.deepseek.com/zh-cn/guides/thinking_mode>。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingControl>,
}

/// 「思考开关」的 wire 形态。
///
/// 序列化为 `{"type": "enabled"|"disabled"}` —— `disabled` 是 DeepSeek 关闭思考的
/// **唯一**合法表达（`reasoning_effort: "off"` 会返回 422 `unknown variant 'off'`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingControl {
    Enabled,
    Disabled,
}

/// `stream_options`：OpenAI 兼容端点用于让流式末包携带 usage 的开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ChatFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ChatFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatFunctionCall {
    pub name: String,
    pub arguments: String,
}
