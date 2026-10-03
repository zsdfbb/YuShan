pub mod compat;
pub mod request;
pub mod response;
pub mod stream;

mod inline_thinking;

use std::collections::BTreeMap;

use compat::ProviderCompat;
use inline_thinking::{Segment, SegmentKind};
use request::*;
use response::*;
use stream::{StreamEvent, parse_sse_stream};
use ys_core::{ContentBlock, Message, Role, ToolCallId, Usage};
use ys_model::{
    FinishReason, Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse,
};

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
    /// 用默认 client（`reqwest::Client::new()`，遵循进程级/系统代理）构造。
    pub fn new(config: OpenAICompatibleConfig) -> Self {
        Self::with_client(config, reqwest::Client::new())
    }

    /// 用调用方提供的 `reqwest::Client` 构造。
    ///
    /// 便于注入自定义超时、连接池或代理策略（如测试里 `.no_proxy()` 直连回环），
    /// 从而不必改进程级 env——避免同一测试二进制内并行执行时相互污染。
    pub fn with_client(config: OpenAICompatibleConfig, client: reqwest::Client) -> Self {
        Self { config, client }
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
    /// 剥离 `content` 内联思考标签；内联思考只经事件出口，不写 `reasoning`。
    filter: inline_thinking::InlineThinkingFilter,
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
            filter: inline_thinking::InlineThinkingFilter::new(),
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
                    // 保留空 delta 守卫：否则快路径会发出空 `TextDelta`，
                    // TUI 会落一条空 Assistant 行。
                    if !content.is_empty() {
                        match self.filter.passthrough(&content) {
                            Some(SegmentKind::Text) => {
                                self.text.push_str(&content);
                                let _ = self.sink.emit(ModelEvent::TextDelta { text: content });
                            }
                            Some(SegmentKind::Thinking) => {
                                let _ = self.sink.emit(ModelEvent::ThinkingDelta { text: content });
                            }
                            None => {
                                for seg in self.filter.feed(&content) {
                                    self.emit_segment(seg);
                                }
                            }
                        }
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

    /// 把过滤器产出的段落到「累积 `text` + emit 事件」两点。
    /// Text 段同时写 `self.text`（终态 `ContentBlock::Text` 的来源）；
    /// Thinking 段只 emit，不写 `self.reasoning`。
    fn emit_segment(&mut self, seg: Segment) {
        match seg {
            Segment::Text(t) => {
                self.text.push_str(&t);
                let _ = self.sink.emit(ModelEvent::TextDelta { text: t });
            }
            Segment::Thinking(t) => {
                let _ = self.sink.emit(ModelEvent::ThinkingDelta { text: t });
            }
        }
    }

    /// 流结束后收敛为 [`ModelResponse`]。
    fn finish(mut self) -> Result<ModelResponse, ModelError> {
        if let Some(message) = self.error {
            // 流式解析失败不做非流式回退——直接以 Provider 错误上抛。
            return Err(ModelError::Provider(format!("SSE stream error: {message}")));
        }

        // 冲刷跨片扣留：思考态残留 → Thinking（未闭合思考不外泄）、
        // 正文态假前缀 → Text（不丢字）。须在错误检查之后、构造 content_blocks 之前。
        for seg in self.filter.flush() {
            self.emit_segment(seg);
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
            finish_reason: normalize_finish_reason(self.finish_reason),
        })
    }
}

/// provider 的 finish_reason 方言 → [`FinishReason`]。
///
/// 分工：**方言归适配器，词汇归 `ys-model`** —— 因此 `ys-loop` 永远不必认识
/// 任何 provider 的字符串。别名（`end_turn` / `max_tokens` / `tool_use`）是
/// 零成本的保险，覆盖 Anthropic 系词汇。
fn normalize_finish_reason(raw: Option<String>) -> FinishReason {
    match raw.as_deref() {
        Some("stop") | Some("end_turn") => FinishReason::Stop,
        Some("tool_calls") | Some("tool_use") => FinishReason::ToolCalls,
        Some("length") | Some("max_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        // 未识别的方言与「provider 压根没给」都落到 Other —— 语义是「没结论」，
        // 绝不猜成 Stop（那会让下游把未知当正常收尾）。
        Some(other) => FinishReason::Other(other.to_string()),
        None => FinishReason::Other(String::new()),
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

    /// 喂一个 `delta.content` 的 SSE 行。
    fn drive_content(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>, content: &str) {
        let line = format!(
            "data: {}\n",
            serde_json::json!({"choices":[{"index":0,"delta":{"content":content}}]})
        );
        drive(buffer, acc, line.as_bytes());
    }

    /// 喂一个 `delta.reasoning_content` 的 SSE 行。
    fn drive_reasoning(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>, reasoning: &str) {
        let line = format!(
            "data: {}\n",
            serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":reasoning}}]})
        );
        drive(buffer, acc, line.as_bytes());
    }

    /// 喂正常收尾（finish_reason=stop + [DONE]）。
    fn drive_stop(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>) {
        drive(
            buffer,
            acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        drive(buffer, acc, b"data: [DONE]\n");
    }

    fn text_deltas(sink: &RecordingModelSink) -> String {
        sink.events
            .iter()
            .filter_map(|ev| match ev {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn thinking_deltas(sink: &RecordingModelSink) -> String {
        sink.events
            .iter()
            .filter_map(|ev| match ev {
                ModelEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn final_text(response: &ModelResponse) -> String {
        response
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
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
        assert_eq!(response.finish_reason, FinishReason::Stop);
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

    /// 内联思考跨片流：`<think>…</think>` 被切成任意片段时，
    /// TextDelta 不含思考、`ContentBlock::Text` == TextDelta 拼接、
    /// ThinkingDelta 拼接 == 思考原文，且思考不进最终文本。
    #[test]
    fn inline_thinking_tags_split_across_content_chunks() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        for chunk in ["pre<thi", "nk>sec", "ret</thi", "nk>post"] {
            drive_content(&mut buf, &mut acc, chunk);
        }
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "prepost");
        assert_eq!(final_text(&response), "prepost");
        assert_eq!(thinking_deltas(&sink), "secret");
        for ev in &sink.events {
            if let ModelEvent::TextDelta { text } = ev {
                assert!(!text.contains("secret"), "TextDelta 泄漏思考：{text:?}");
            }
        }
    }

    /// 内联思考与 `reasoning_content` 并存时互不串扰：
    /// 两个来源都进 ThinkingDelta，正文各自独立。
    #[test]
    fn inline_thinking_coexists_with_reasoning_content() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, true);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "<think>T</think>V");
        drive_reasoning(&mut buf, &mut acc, "R");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "V");
        assert_eq!(final_text(&response), "V");
        assert_eq!(thinking_deltas(&sink), "TR");
    }

    /// 无标签流仍只发 TextDelta（守 `thinking_delta_only_emitted_when_compat_enabled`）。
    #[test]
    fn plain_content_stream_emits_only_text_deltas() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "hello ");
        drive_content(&mut buf, &mut acc, "world");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "hello world");
        assert_eq!(final_text(&response), "hello world");
        assert!(
            sink.events
                .iter()
                .all(|ev| matches!(ev, ModelEvent::TextDelta { .. })),
            "events = {:?}",
            sink.events
        );
    }

    /// 未闭合内联思考在 `feed` 阶段即已作为 Thinking 发出，`flush` 无产出。
    /// （本用例不覆盖 `finish` 的 flush 接线——那由下方两条半截标签用例守。）
    #[test]
    fn unclosed_inline_thinking_emitted_during_feed() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "answer<think>partial");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "answer");
        assert_eq!(final_text(&response), "answer");
        assert_eq!(thinking_deltas(&sink), "partial");
    }

    /// `finish` 的 flush：正文态流末半截开标签（`<thi`）须释放为 Text，不丢字。
    /// 删掉 `finish` 里的 flush 循环后本用例必须变红。
    #[test]
    fn finish_flush_releases_trailing_partial_open_tag() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "answer<thi");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "answer<thi");
        assert_eq!(final_text(&response), "answer<thi");
        assert_eq!(thinking_deltas(&sink), "");
    }

    /// `finish` 的 flush：思考态流末半截闭标签（`</thi`）须释放为 Thinking，
    /// 且不进入正文。删掉 `finish` 里的 flush 循环后本用例必须变红。
    #[test]
    fn finish_flush_releases_partial_close_tag_in_thinking() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "answer<think>abc</thi");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "answer");
        assert_eq!(final_text(&response), "answer");
        assert_eq!(thinking_deltas(&sink), "abc</thi");
    }

    /// 空 `content` delta 不得发出空 `TextDelta`（既有守卫的出口验证）。
    #[test]
    fn empty_content_delta_emits_no_event() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "");
        drive_stop(&mut buf, &mut acc);
        let _ = acc.finish().unwrap();

        assert!(sink.events.is_empty(), "events = {:?}", sink.events);
    }

    /// 三对标签经适配器出口均剥离：`<thinking>` / `<reasoning>` 同样生效。
    #[test]
    fn all_tag_pairs_are_stripped_from_content() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(
            &mut buf,
            &mut acc,
            "<thinking>t</thinking>a<reasoning>r</reasoning>b",
        );
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "ab");
        assert_eq!(thinking_deltas(&sink), "tr");
        assert_eq!(final_text(&response), "ab");
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

    /// 映射表：**全部已知方言**逐条覆盖，含 Anthropic 系别名与未知值的兜底。
    ///
    /// 用参数化表而非散装 `assert`，是为了让「新增一种方言」时只需加一行。
    #[test]
    fn normalize_finish_reason_maps_every_known_dialect() {
        let cases: Vec<(Option<&str>, FinishReason)> = vec![
            // OpenAI 系
            (Some("stop"), FinishReason::Stop),
            (Some("tool_calls"), FinishReason::ToolCalls),
            (Some("length"), FinishReason::Length),
            (Some("content_filter"), FinishReason::ContentFilter),
            // Anthropic 系别名（零成本保险）
            (Some("end_turn"), FinishReason::Stop),
            (Some("max_tokens"), FinishReason::Length),
            (Some("tool_use"), FinishReason::ToolCalls),
            // 未知方言 / provider 未给：一律 Other，**不得**猜成 Stop
            (
                Some("some_future_thing"),
                FinishReason::Other("some_future_thing".into()),
            ),
            (None, FinishReason::Other(String::new())),
        ];

        for (raw, expected) in cases {
            let got = normalize_finish_reason(raw.map(str::to_string));
            assert_eq!(got, expected, "raw = {raw:?}");
        }
    }

    /// **下游截断保护的判据来源**：只有 `length` / `max_tokens` 算截断；
    /// `stop` / `None` / 未知值都**不是** —— 未知绝不能伪装成正常收尾。
    #[test]
    fn normalize_finish_reason_truncation_flag() {
        assert!(normalize_finish_reason(Some("length".into())).is_truncated());
        assert!(normalize_finish_reason(Some("max_tokens".into())).is_truncated());

        assert!(!normalize_finish_reason(Some("stop".into())).is_truncated());
        assert!(!normalize_finish_reason(None).is_truncated());
        assert!(
            !normalize_finish_reason(Some("some_future_thing".into())).is_truncated(),
            "未知方言不得被判为截断"
        );
    }

    /// 端到端（合成 SSE）：provider 给 `finish_reason: "length"` 时，
    /// 收敛出的 `ModelResponse.finish_reason` 必须已归一化且 `is_truncated()`。
    /// 这条守着 `finish()` 的接线，而不仅是映射函数本身。
    #[test]
    fn streamed_length_finish_reason_is_normalized_to_truncated() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"cut off\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.finish_reason, FinishReason::Length);
        assert!(response.finish_reason.is_truncated());
    }
}
