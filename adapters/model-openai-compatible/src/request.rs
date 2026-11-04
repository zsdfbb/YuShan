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
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `messages[].content` 的两种 wire 形态。
///
/// OpenAI 协议里 `content` 既可以是**字符串**，也可以是**内容块数组**。
/// 纯文本消息必须走 [`Content::Text`] ⇒ 线上是 `"content":"…"`，
/// **不是** `[{"type":"text","text":"…"}]`：后者与既有请求逐字节不同，
/// 多家属地端点对这条差异的处理并不一致。`Content::Text` 变体就是为此存在，
/// 不是冗余。
///
/// `untagged` 是这里的**正确语义**：两种形态靠字段形状自然区分，
/// 没有 tag 字段可依靠。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// 多模态内容块。
///
/// `#[serde(tag = "type")]` 是 **OpenAI wire 协议**的形状
/// （`{"type":"image_url","image_url":{"url":"…"}}`），与本地会话的落盘形态无关。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

/// `image_url` 块的体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// `https://…` 或 `data:image/png;base64,…`（本仓出站时由
    /// `media::resolve_image` 生成）。
    pub url: String,
    /// 细节档（`low` / `high` / `auto`）。`None` 时该键不出现 ——
    /// 由 provider 默认决定，与既有请求体形态一致。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn message(content: Option<Content>) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// **golden**：纯文本消息的线上字节是 `"content":"…"`（字符串），
    /// 不是 `[{"type":"text","text":"…"}]`。
    ///
    /// 这条线一旦破掉，所有 provider 的请求体形态都会变（R4），而多数差异不会报错，
    /// 只会静静改变计费与行为 —— 所以逐字节钉死。
    ///
    /// 变异：给 `Content` 去掉 `#[serde(untagged)]` ⇒ 序列化成 `{"Text":"hi"}`，本条变红；
    ///       把 `ChatMessage.content` 改回 `Vec<ContentPart>` ⇒ 同样变红。
    #[test]
    fn pure_text_content_serializes_as_a_json_string() {
        let json = serde_json::to_string(&message(Some(Content::Text("hi".into())))).unwrap();

        assert_eq!(
            json, r#"{"role":"user","content":"hi"}"#,
            "纯文本必须是字符串形态"
        );
    }

    /// 整条请求体（含 system / tool 消息）同样不得出现块数组。
    #[test]
    fn text_only_messages_never_emit_content_arrays() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: Some(Content::Text("You are helpful.".into())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            message(Some(Content::Text("hello".into()))),
            ChatMessage {
                role: "tool".into(),
                content: Some(Content::Text("file contents".into())),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: None,
            },
        ];

        let json = serde_json::to_string(&messages).unwrap();

        assert!(!json.contains("image_url"));
        assert_eq!(
            json,
            r#"[{"role":"system","content":"You are helpful."},{"role":"user","content":"hello"},{"role":"tool","content":"file contents","tool_call_id":"call_1"}]"#
        );
    }

    /// 图片块的 wire 形态：`{"type":"image_url","image_url":{"url":"…"}}`。
    ///
    /// `detail` 为 `None` 时该键**不出现**（与 `ImageUrl.detail` 的
    /// `skip_serializing_if` 一致）。
    #[test]
    fn image_part_serializes_in_openai_wire_shape() {
        let content = Content::Parts(vec![
            ContentPart::Text {
                text: "what is this?".into(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,iVBORw0KGgo=".into(),
                    detail: Some("high".into()),
                },
            },
        ]);

        let json = serde_json::to_string(&message(Some(content))).unwrap();

        assert_eq!(
            json,
            r#"{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}}]}"#
        );
    }

    /// `detail: None` ⇒ 键不出现，不得写成 `"detail":null`。
    #[test]
    fn image_part_omits_absent_detail() {
        let part = ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "data:image/jpeg;base64,AAAA".into(),
                detail: None,
            },
        };

        assert_eq!(
            serde_json::to_string(&part).unwrap(),
            r#"{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,AAAA"}}"#
        );
    }

    /// `untagged` 同时要能**读回**两种形态（响应侧的对称性；不靠 `untagged` 猜 tag）。
    #[test]
    fn both_content_shapes_roundtrip() {
        for content in [
            Content::Text("plain".into()),
            Content::Parts(vec![ContentPart::Text {
                text: "blocks".into(),
            }]),
        ] {
            let json = serde_json::to_string(&content).unwrap();
            let back: Content = serde_json::from_str(&json).unwrap();
            assert_eq!(back, content, "json = {json}");
        }
    }
}
