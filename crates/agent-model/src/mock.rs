use std::sync::Mutex;

use agent_core::{ContentBlock, Message, Role, ToolCallId, Usage};
use async_trait::async_trait;

use super::{Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse};

/// Scripted responses for the mock model.
pub enum MockResponse {
    Text(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    Error(String),
}

/// A test model that returns pre-programmed responses.
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

    /// Push a text response.
    pub fn push_text(&self, text: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::Text(text.into()));
    }

    /// Push a tool call response.
    pub fn push_tool_call(&self, name: impl Into<String>, arguments: serde_json::Value) {
        self.responses.lock().unwrap().push(MockResponse::ToolCall {
            name: name.into(),
            arguments,
        });
    }

    /// Push an error response.
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
