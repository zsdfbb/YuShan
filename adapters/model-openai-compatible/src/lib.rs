pub mod compat;
pub mod request;
pub mod response;
pub mod stream;

use std::collections::BTreeMap;

use compat::ProviderCompat;
use request::*;
use response::*;
use stream::{StreamEvent, parse_sse_stream};
use ys_core::{ContentBlock, Message, Role, ToolCallId, Usage};
use ys_model::{Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse};

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

    /// 发送一次 `/chat/completions` 请求。只负责传输层错误，不解读状态码——
    /// 状态码由 [`Model::complete`] 统一处理（便于实现一次性回退）。
    async fn send(
        &self,
        url: &str,
        chat_request: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, ModelError> {
        self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(chat_request)
            .send()
            .await
            .map_err(|e| ModelError::Provider(format!("HTTP request failed: {e}")))
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
        // 1. 翻译 v0 内部格式 → OpenAI API 格式（`stream: true`）。
        //    `supports_stream_usage` 决定是否带 `stream_options.include_usage`。
        let mut chat_request = translate_request(&request, &self.config)?;

        // 2. 发送请求。URL 里 base 已 trim 尾部 `/`，统一追加 `/chat/completions`。
        let url = format!(
            "{}/chat/completions",
            self.config.api_base.trim_end_matches('/')
        );
        let response = self.send(&url, &chat_request).await?;

        // 2b. 一次性回退：端点不认识 `stream_options.include_usage` 时返回 400。
        //     若本次请求带了该字段，则剥掉后**只重试一次**——避免让严格校验的
        //     网关直接失败，同时让容忍该字段的端点拿到 token 统计。
        //     请求本来就没带（`supports_stream_usage = false`）→ 不触发，直接报错。
        let response = if response.status() == reqwest::StatusCode::BAD_REQUEST
            && chat_request.stream_options.is_some()
        {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eprintln!(
                "warning: endpoint rejected `stream_options.include_usage` \
                 ({status}: {body}); retrying once without it — \
                 token usage for this run will be reported as 0"
            );
            chat_request.stream_options = None;
            self.send(&url, &chat_request).await?
        } else {
            response
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ModelError::Provider(format!("API error {status}: {body}")));
        }

        // 3. 流式解析：SSE 每解析出一个 delta 就立刻推给 sink（真增量，不缓冲）
        let mut accumulator =
            StreamAccumulator::new(sink, self.config.compat.has_reasoning_content);
        parse_sse_stream(response, |event| accumulator.handle(event)).await?;

        // 4. 收敛为完整 ModelResponse（工具参数已在累积器内组装）
        accumulator.finish()
    }
}

/// 单个 tool call 的流式累积器：按 `index` 分组。
///
/// 流式协议把 `id` / `name` / `arguments` 切成任意片段分多片下发：
/// `id` / `name` 通常只在首片出现（`id` 用首个非空值），`arguments` 是
/// **JSON 字符串片段**，必须逐片拼接后才能反序列化。
#[derive(Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn apply(&mut self, call: ChunkToolCall) {
        if let Some(id) = call.id {
            // id 只在首片可靠出现，取首个非空值即可
            self.id.get_or_insert(id);
        }
        if let Some(function) = call.function {
            if let Some(name) = function.name {
                self.name.push_str(&name);
            }
            if let Some(arguments) = function.arguments {
                self.arguments.push_str(&arguments);
            }
        }
    }

    /// 收敛为一个 `ToolUse` block。参数为空串（无参工具）时退化为 `null`。
    fn into_content_block(self) -> ContentBlock {
        // 已知降级：参数 JSON 非法时退化为 `Value::Null`（工具层看到空参数）。
        // 这是保守兜底——不 panic，也不回喂错误结果（回喂涉及工具层协议，
        // 超出适配器职责）；为使其可观测，非空但解析失败时打一条警告。
        // 空串是「无参工具」的正常形态，不作为异常告警。
        let arguments = if self.arguments.trim().is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_str(&self.arguments) {
                Ok(value) => value,
                Err(e) => {
                    let prefix: String = self.arguments.chars().take(120).collect();
                    eprintln!(
                        "warning: model tool_call arguments for `{}` are not valid JSON \
                         ({e}); falling back to null. arguments prefix: {prefix:?}",
                        self.name
                    );
                    serde_json::Value::Null
                }
            }
        };
        ContentBlock::ToolUse {
            id: ToolCallId(self.id.unwrap_or_default()),
            name: self.name,
            arguments,
        }
    }
}

/// 流式响应的累积器：边收增量边发 [`ModelEvent`]，流结束后收敛为 `ModelResponse`。
///
/// 这是 [`Model::complete`] 的流式核心，与 `reqwest::Response` 解耦——
/// 测试可直接喂 [`StreamEvent`] 驱动它，无需网络。
struct StreamAccumulator<'s> {
    sink: &'s mut dyn ModelEventSink,
    /// provider 是否声明支持 `reasoning_content`；否则该字段不可信，忽略。
    has_reasoning_content: bool,
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u32, ToolCallAccumulator>,
    finish_reason: Option<String>,
    /// 流中出现的最后一个非空 usage（OpenAI 系在末包附带）。
    usage: Option<Usage>,
    error: Option<String>,
}

impl<'s> StreamAccumulator<'s> {
    fn new(sink: &'s mut dyn ModelEventSink, has_reasoning_content: bool) -> Self {
        Self {
            sink,
            has_reasoning_content,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            error: None,
        }
    }

    /// 处理一个流事件：累积增量并**立即**向 sink 推送产物型 delta。
    fn handle(&mut self, event: StreamEvent) -> Result<(), ModelError> {
        match event {
            StreamEvent::Delta {
                content,
                reasoning_content,
                tool_calls,
                finish_reason,
                usage,
            } => {
                if let Some(content) = content {
                    if !content.is_empty() {
                        // 关键不变量：此处累积的 text 将原样成为最终
                        // `ContentBlock::Text`，与逐片 emit 的增量逐字节相等。
                        self.text.push_str(&content);
                        let _ = self.sink.emit(ModelEvent::TextDelta { text: content });
                    }
                }

                if self.has_reasoning_content {
                    if let Some(reasoning) = reasoning_content {
                        if !reasoning.is_empty() {
                            self.reasoning.push_str(&reasoning);
                            let _ = self
                                .sink
                                .emit(ModelEvent::ThinkingDelta { text: reasoning });
                        }
                    }
                }

                // 工具参数增量**不冒泡**：在适配器内按 index 组装。
                if let Some(calls) = tool_calls {
                    for call in calls {
                        self.tool_calls.entry(call.index).or_default().apply(call);
                    }
                }

                // usage 尾包：累积最后一个非空值（末包覆盖前值即可）。
                if let Some(usage) = usage {
                    self.usage = Some(Usage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                    });
                }

                if finish_reason.is_some() {
                    self.finish_reason = finish_reason;
                }
            }
            StreamEvent::Done => {}
            StreamEvent::Error(message) => self.error = Some(message),
        }
        Ok(())
    }

    /// 流结束后收敛为 [`ModelResponse`]。
    fn finish(self) -> Result<ModelResponse, ModelError> {
        if let Some(message) = self.error {
            // 流式解析失败不做非流式回退——直接以 Provider 错误上抛。
            return Err(ModelError::Provider(format!("SSE stream error: {message}")));
        }

        let mut content_blocks = Vec::new();
        if !self.text.is_empty() {
            content_blocks.push(ContentBlock::Text { text: self.text });
        }
        for (_, accumulator) in self.tool_calls {
            // 参数完整（有 name）时才产出；一个调用一个 block。
            if !accumulator.name.is_empty() {
                content_blocks.push(accumulator.into_content_block());
            }
        }

        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
            },
            // 流式响应仅在 provider 声明支持 `stream_options.include_usage`
            // 时才会在末包附带 usage；此处取流中最后一个非空值。
            // 端点不支持该选项时始终收不到 → 退回 `Usage::default()`（token 统计为 0），
            // 这是保守兼容的已知代价，不 panic。
            usage: self.usage.unwrap_or_default(),
            stop_reason: self.finish_reason,
        })
    }
}

pub fn translate_request(
    request: &ModelRequest,
    config: &OpenAICompatibleConfig,
) -> Result<ChatCompletionRequest, ModelError> {
    let mut messages = Vec::new();

    // 系统提示词
    if let Some(system) = &request.system {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(system.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    // Session 消息
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
                    // OpenAI 格式：role="tool"、tool_call_id、content
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
                _ => {} // 处理未来的 ContentBlock 变体
            }
        }

        // 追加主消息
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

    // Tools 列表
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
        stream: Some(true),
        // 仅当 provider 声明支持时才请求流式 usage；否则整个字段不序列化。
        stream_options: config
            .compat
            .supports_stream_usage
            .then_some(StreamOptions {
                include_usage: true,
            }),
        max_tokens: config.max_tokens.or(request.max_tokens),
        temperature: config.temperature.or(request.temperature),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_model::ModelRequest;

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
            messages: vec![ys_core::Message {
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
        assert_eq!(chat_req.messages.len(), 2); // system + user 各一条
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
            messages: vec![ys_core::Message {
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

    /// 记录所有 `ModelEvent` 的测试 sink。
    #[derive(Default)]
    struct RecordingModelSink {
        events: Vec<ModelEvent>,
    }

    impl ModelEventSink for RecordingModelSink {
        fn emit(&mut self, event: ModelEvent) -> Result<(), ys_core::EventError> {
            self.events.push(event);
            Ok(())
        }
    }

    /// 把一段原始 SSE 字节喂给累积器（与 `parse_sse_stream` 共用解析核心）。
    ///
    /// `buffer` 必须在多次调用间复用——否则跨 chunk 的半行会被丢弃。
    fn drive(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>, bytes: &[u8]) {
        stream::feed_sse_bytes(buffer, bytes, &mut |ev| acc.handle(ev)).unwrap();
    }

    /// 关键不变量（design §12「增量拼接与终态消息一致」）：
    /// 逐片 emit 的 `TextDelta` 拼接结果，与最终 `ModelResponse` 里
    /// `ContentBlock::Text` 的文本**逐字节相等**。含跨 chunk 的行边界。
    #[test]
    fn streamed_text_deltas_concatenate_to_final_text_byte_for_byte() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        // 一条 SSE 行被 TCP 切成两半，第二片才补齐换行
        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"content":"Hel"#,
        );
        drive(&mut buf, &mut acc, br#"lo, "}}]}"#);
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"world\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();

        let joined: String = sink
            .events
            .iter()
            .filter_map(|ev| match ev {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let final_text = match &response.message.content[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected Text block, got {other:?}"),
        };

        assert_eq!(joined, "Hello, world");
        assert_eq!(joined, final_text, "增量拼接必须与终态文本逐字节相等");
        assert_eq!(response.stop_reason.as_deref(), Some("stop"));
    }

    /// 工具参数增量：同一 `index` 的 id/name/arguments 分多片下发，
    /// 在适配器内组装成**一个**完整 `ToolUse`；工具参数不冒泡为 `ModelEvent`。
    #[test]
    fn tool_call_deltas_assemble_into_one_tool_use() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_","arguments":"{\"pa"}}]}}]}"#,
        );
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"a.txt\"}"}}]}}]}"#,
        );
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.message.content.len(), 1);
        match &response.message.content[0] {
            ContentBlock::ToolUse {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id.0, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, &serde_json::json!({"path": "a.txt"}));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }
        // 工具参数不冒泡：没有任何增量事件被发出
        assert!(sink.events.is_empty());
    }

    /// `reasoning_content` 仅在 compat 声明支持时才发出 `ThinkingDelta`。
    #[test]
    fn thinking_delta_only_emitted_when_compat_enabled() {
        const LINE: &[u8] =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n";

        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, true);
        let mut buf: Vec<u8> = Vec::new();
        drive(&mut buf, &mut acc, LINE);
        let _ = acc.finish().unwrap();
        assert!(
            matches!(&sink.events[0], ModelEvent::ThinkingDelta { text } if text == "hmm"),
            "events = {:?}",
            sink.events
        );

        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();
        drive(&mut buf, &mut acc, LINE);
        let _ = acc.finish().unwrap();
        assert!(sink.events.is_empty(), "compat 关闭时 reasoning 必须被忽略");
    }

    /// 流式请求必须携带 `stream: true`。
    #[test]
    fn translate_request_sets_stream_true() {
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            compat: ProviderCompat::standard(),
        };
        let request = ModelRequest::default();
        let chat_req = translate_request(&request, &config).unwrap();
        assert_eq!(chat_req.stream, Some(true));
    }

    /// 构造一个仅 compat 不同的最小 config。
    fn config_with(compat: ProviderCompat) -> OpenAICompatibleConfig {
        OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            compat,
        }
    }

    /// `supports_stream_usage = true` 时，请求 JSON 含
    /// `stream_options: {"include_usage": true}`。
    #[test]
    fn stream_options_serialized_when_supported() {
        let request = ModelRequest::default();
        let on = config_with(ProviderCompat {
            supports_stream_usage: true,
            ..ProviderCompat::standard()
        });
        let json = serde_json::to_value(translate_request(&request, &on).unwrap()).unwrap();
        assert_eq!(
            json.get("stream_options"),
            Some(&serde_json::json!({ "include_usage": true })),
            "json = {json}"
        );
    }

    /// `supports_stream_usage = false` 时 `skip_serializing_if` 生效，
    /// 整个 `stream_options` 字段不出现。
    #[test]
    fn stream_options_omitted_when_unsupported() {
        let request = ModelRequest::default();
        let off = config_with(ProviderCompat {
            supports_stream_usage: false,
            ..ProviderCompat::standard()
        });
        let json = serde_json::to_value(translate_request(&request, &off).unwrap()).unwrap();
        assert!(
            json.get("stream_options").is_none(),
            "不支持时不得序列化 stream_options，json = {json}"
        );
    }

    /// 流末包带 usage（OpenAI 形状：`choices: []` + `usage`）时，
    /// 累积为 `ModelResponse.usage` 的实际值，而非默认 0。
    #[test]
    fn streamed_usage_is_captured_from_final_chunk() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        // usage 尾包：choices 为空，只带统计
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.usage.input_tokens, 11);
        assert_eq!(response.usage.output_tokens, 7);
    }

    /// 流里从头到尾没有 usage（端点不支持 `include_usage`）时，
    /// 退回 `Usage::default()`，不 panic。
    #[test]
    fn streamed_without_usage_defaults_to_zero() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.usage, Usage::default());
        assert_eq!(response.usage.input_tokens, 0);
        assert_eq!(response.usage.output_tokens, 0);
    }
}
