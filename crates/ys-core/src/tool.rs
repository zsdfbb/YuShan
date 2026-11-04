use serde::{Deserialize, Serialize};

use crate::media::MediaAttachment;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// 工具执行的结果：文本 + 可选媒体附件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// 随工具结果一起回喂给模型的媒体附件。
    ///
    /// 空表示「无附件」——此时序列化形态与加字段前**逐字节相同**（靠 `skip_serializing_if`），
    /// 这是既有会话 JSONL 与事件流形态不变的前提。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaAttachment>,
}

impl ToolResult {
    /// 无附件的便捷构造 —— 等价于「既有工具返回的东西」。
    pub fn text(content: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: content.into(),
            is_error,
            media: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ImageDetail;

    #[test]
    fn tool_result_without_media_serializes_byte_identically() {
        let r = ToolResult::text("x", false);
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"content":"x","is_error":false}"#
        );
        // 反向：有 media 时该键必须出现
        let with = ToolResult {
            media: vec![MediaAttachment::Image {
                path: "/tmp/a.png".to_string(),
                detail: Some(ImageDetail::High),
            }],
            ..ToolResult::text("x", false)
        };
        assert_eq!(
            serde_json::to_string(&with).unwrap(),
            r#"{"content":"x","is_error":false,"media":[{"Image":{"path":"/tmp/a.png","detail":"high"}}]}"#
        );
    }

    #[test]
    fn media_attachment_serde_roundtrip() {
        let cases = [
            MediaAttachment::Image {
                path: "/tmp/a.png".to_string(),
                detail: Some(ImageDetail::High),
            },
            MediaAttachment::Image {
                path: "/tmp/b.jpg".to_string(),
                detail: None,
            },
        ];
        for case in &cases {
            let json = serde_json::to_string(case).unwrap();
            let back: MediaAttachment = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, case, "round-trip 应相等：{json}");
        }
        // 外部标签 + 小写档位 + None 省略键 是对外契约
        assert_eq!(
            serde_json::to_string(&cases[0]).unwrap(),
            r#"{"Image":{"path":"/tmp/a.png","detail":"high"}}"#
        );
        assert_eq!(
            serde_json::to_string(&cases[1]).unwrap(),
            r#"{"Image":{"path":"/tmp/b.jpg"}}"#
        );
    }

    /// 空 `media` 是「无附件」的唯一表示：缺失该键与空数组反序列化后相等。
    #[test]
    fn absent_media_field_deserializes_to_empty() {
        let r: ToolResult = serde_json::from_str(r#"{"content":"x","is_error":true}"#).unwrap();
        assert_eq!(r, ToolResult::text("x", true));
        assert!(r.media.is_empty());
    }
}
