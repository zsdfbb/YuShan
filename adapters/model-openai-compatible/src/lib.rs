pub mod compat;
pub mod request;
pub mod response;
pub mod stream;

use agent_core::{ContentBlock, Message, Role, ToolCallId, Usage};
use agent_model::{Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse};
use compat::ProviderCompat;
use request::*;
use response::*;

pub struct OpenAICompatibleConfig {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub compat: ProviderCompat,
}

pub struct OpenAICompatibleModel {
    config: OpenAICompatibleConfig,
    client: reqwest::Client,
}

impl OpenAICompatibleModel {
    pub fn new(config: OpenAICompatibleConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Model for OpenAICompatibleModel {
    fn model_id(&self) -> &str {
        &self.config.model
    }

    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        // 1. 翻译 v0 内部格式 → OpenAI API 格式
        let chat_request = translate_request(&request, &self.config)?;

        // 2. 发送请求（先用非流式 stream: false）
        let url = format!(
            "{}/chat/completions",
            self.config.api_base.trim_end_matches('/')
        );
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&chat_request)
            .send()
            .await
            .map_err(|e| ModelError::Provider(format!("HTTP request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ModelError::Provider(format!("API error {status}: {body}")));
        }

        // 3. 解析响应
        let chat_response: ChatCompletionResponse = response
            .json()
            .await
            .map_err(|e| ModelError::Provider(format!("Failed to parse response: {e}")))?;

        // 4. 翻译响应 → v0 格式
        translate_response(chat_response, sink)
    }
}

pub fn translate_request(
    request: &ModelRequest,
    config: &OpenAICompatibleConfig,
) -> Result<ChatCompletionRequest, ModelError> {
    let mut messages = Vec::new();

    // System prompt
    if let Some(system) = &request.system {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(system.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    // Session messages
    for msg in &request.messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };

        let mut text_content = String::new();
        let mut tool_calls = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => text_content.push_str(text),
                ContentBlock::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls.push(ChatToolCall {
                        id: id.0.clone(),
                        call_type: "function".into(),
                        function: ChatFunctionCall {
                            name: name.clone(),
                            arguments: arguments.to_string(),
                        },
                    });
                }
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    // OpenAI format: role="tool", tool_call_id, content
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(if *is_error {
                            format!("[ERROR] {content}")
                        } else {
                            content.clone()
                        }),
                        tool_calls: None,
                        tool_call_id: Some(tool_call_id.0.clone()),
                        name: None,
                    });
                }
                _ => {} // Handle future ContentBlock variants
            }
        }

        // Add the main message
        if !text_content.is_empty() || !tool_calls.is_empty() {
            messages.push(ChatMessage {
                role: role.into(),
                content: if text_content.is_empty() {
                    None
                } else {
                    Some(text_content)
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                name: None,
            });
        }
    }

    // Tools
    let tools = if request.tools.is_empty() {
        None
    } else {
        Some(
            request
                .tools
                .iter()
                .map(|spec| ChatTool {
                    tool_type: "function".into(),
                    function: ChatFunction {
                        name: spec.name.clone(),
                        description: spec.description.clone(),
                        parameters: spec.parameters.clone(),
                    },
                })
                .collect(),
        )
    };

    Ok(ChatCompletionRequest {
        model: config.model.clone(),
        messages,
        tools,
        stream: Some(false),
        max_tokens: config.max_tokens.or(request.max_tokens),
        temperature: config.temperature.or(request.temperature),
    })
}

fn translate_response(
    response: ChatCompletionResponse,
    sink: &mut dyn ModelEventSink,
) -> Result<ModelResponse, ModelError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::Provider("No choices in response".into()))?;

    let mut content_blocks = Vec::new();

    // Text content
    if let Some(text) = choice.message.content {
        if !text.is_empty() {
            // Emit text delta event
            let _ = sink.emit(ModelEvent::TextDelta { text: text.clone() });
            content_blocks.push(ContentBlock::Text { text });
        }
    }

    // Tool calls
    if let Some(tool_calls) = choice.message.tool_calls {
        for tc in tool_calls {
            let arguments: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
            content_blocks.push(ContentBlock::ToolUse {
                id: ToolCallId(tc.id),
                name: tc.function.name,
                arguments,
            });
        }
    }

    let message = Message {
        role: Role::Assistant,
        content: content_blocks,
    };

    let usage = response
        .usage
        .map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        })
        .unwrap_or_default();

    Ok(ModelResponse {
        message,
        usage,
        stop_reason: choice.finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::ModelRequest;

    // 简单测试：验证 translate_request 生成正确的 JSON
    #[test]
    fn test_translate_request_basic() {
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "deepseek-chat".into(),
            max_tokens: None,
            temperature: None,
            compat: ProviderCompat::standard(),
        };

        let request = ModelRequest {
            messages: vec![agent_core::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            }],
            tools: vec![],
            system: Some("You are helpful.".into()),
            max_tokens: None,
            temperature: None,
        };

        let chat_req = translate_request(&request, &config).unwrap();
        assert_eq!(chat_req.model, "deepseek-chat");
        assert_eq!(chat_req.messages.len(), 2); // system + user
        assert_eq!(chat_req.messages[0].role, "system");
        assert_eq!(chat_req.messages[1].role, "user");
    }

    #[test]
    fn test_translate_tool_result() {
        // 验证 ToolResult 翻译为 role:"tool"
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            compat: ProviderCompat::standard(),
        };

        let request = ModelRequest {
            messages: vec![agent_core::Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: ToolCallId("call_1".into()),
                    content: "file contents".into(),
                    is_error: false,
                }],
            }],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
        };

        let chat_req = translate_request(&request, &config).unwrap();
        assert_eq!(chat_req.messages[0].role, "tool");
        assert_eq!(chat_req.messages[0].tool_call_id, Some("call_1".into()));
    }
}
