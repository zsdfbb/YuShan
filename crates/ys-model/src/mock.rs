use std::sync::Mutex;

use async_trait::async_trait;
use ys_core::{ContentBlock, Message, Role, ToolCallId, Usage};

use super::{Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse};

/// 供 mock model 使用的预写响应。
pub enum MockResponse {
    Text(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    Error(String),
}

/// 返回预设响应（pre-programmed）的测试 model。
pub struct MockModel {
    id: String,
    responses: Mutex<Vec<MockResponse>>,
}

impl MockModel {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            responses: Mutex::new(Vec::new()),
        }
    }

    /// 压入一个文本响应。
    pub fn push_text(&self, text: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::Text(text.into()));
    }

    /// 压入一个 tool call 响应。
    pub fn push_tool_call(&self, name: impl Into<String>, arguments: serde_json::Value) {
        self.responses.lock().unwrap().push(MockResponse::ToolCall {
            name: name.into(),
            arguments,
        });
    }

    /// 压入一个错误响应。
    pub fn push_error(&self, msg: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::Error(msg.into()));
    }
}

#[async_trait]
impl Model for MockModel {
    fn model_id(&self) -> &str {
        &self.id
    }

    async fn complete(
        &self,
        _request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        let response = self.responses.lock().unwrap().remove(0);
        match response {
            MockResponse::Text(text) => {
                sink.emit(ModelEvent::TextDelta { text: text.clone() })?;
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text { text }],
                    },
                    usage: Usage::default(),
                    stop_reason: None,
                })
            }
            MockResponse::ToolCall { name, arguments } => {
                let call_id = ToolCallId(format!("mock-{name}"));
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: call_id,
                            name,
                            arguments,
                        }],
                    },
                    usage: Usage::default(),
                    stop_reason: None,
                })
            }
            MockResponse::Error(msg) => Err(ModelError::Provider(msg)),
        }
    }
}
