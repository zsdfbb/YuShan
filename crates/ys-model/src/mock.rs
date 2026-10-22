use std::sync::Mutex;

use async_trait::async_trait;
use ys_core::{ContentBlock, Message, Role, ToolCallId, Usage};

use super::{
    FinishReason, Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse,
};

/// 供 mock model 使用的预写响应。
pub enum MockResponse {
    Text(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    Error(String),
    /// 被截断的纯文本响应（`finish_reason = Length`）。
    TruncatedText(String),
    /// 被截断的 tool call 响应（`finish_reason = Length`）。
    TruncatedToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    /// **只输出思考内容、无正文**的响应。
    ///
    /// 终态 message 的 `content` **无 `Text` 块** —— 与「模型只回 reasoning、
    /// 没回正文」的真实形态同构（适配器把 reasoning 走 `ThinkingDelta`、
    /// 不进 `Text` block）。
    ///
    /// 存在理由：摘要调用在**没有正文**时必须能降级到思考内容
    /// （否则报「the model returned no text」而白丢一次满载调用）。
    ThinkingOnly(String),
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

    /// 压入一个**只输出思考、无正文**的响应。
    ///
    /// 终态 `content` 无 `Text` 块；思考内容经 `ThinkingDelta` 发出
    /// ⇒ 与「模型只回 reasoning」的真实形态同构。
    pub fn push_thinking_only(&self, text: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::ThinkingOnly(text.into()));
    }

    /// 压入一个错误响应。
    pub fn push_error(&self, msg: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::Error(msg.into()));
    }

    /// 压入一个「被截断的 tool call」响应（finish_reason = Length）。
    pub fn push_truncated_tool_call(&self, name: impl Into<String>, arguments: serde_json::Value) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::TruncatedToolCall {
                name: name.into(),
                arguments,
            });
    }

    /// 压入一个「被截断的纯文本」响应（finish_reason = Length）。
    pub fn push_truncated_text(&self, text: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push(MockResponse::TruncatedText(text.into()));
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
                    finish_reason: FinishReason::Stop,
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
                    finish_reason: FinishReason::ToolCalls,
                })
            }
            MockResponse::ThinkingOnly(text) => {
                // 只发 `ThinkingDelta`、**不发** `TextDelta` —— 终态无 `Text` 块。
                sink.emit(ModelEvent::ThinkingDelta { text })?;
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                })
            }
            MockResponse::Error(msg) => Err(ModelError::Provider(msg)),
            // 截断分支**不** emit `TextDelta` —— 与 `push_tool_call` 一致，
            // 只有 `push_text` 才发增量。
            MockResponse::TruncatedText(text) => Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Length,
            }),
            MockResponse::TruncatedToolCall { name, arguments } => {
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
                    finish_reason: FinishReason::Length,
                })
            }
        }
    }
}
