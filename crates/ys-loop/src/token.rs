use ys_core::{ContentBlock, Message};

/// 单张图片的估算 token 数。
///
/// 厂商按像素/瓦片计费，这里只做一个量级正确的固定值 —— 目的是让上下文预算感知到
/// 「图片不便宜」，不是精确计费。取值与 `turn_budget` 的口径注释一致。
pub const IMAGE_TOKEN_ESTIMATE: usize = 1024;

/// 估算一条消息中的 token 数（字符数/4 + CJK 补偿 + 图片固定值）
pub fn estimate_tokens(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::ToolUse { arguments, .. } => estimate_text_tokens(&arguments.to_string()),
            ContentBlock::ToolResult { content, .. } => estimate_text_tokens(content),
            // 只存路径，不存字节 ⇒ 尺寸不可知，按固定值记。
            ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
            // 正文的判据在 [`ContentBlock::as_text`]（定义 crate 内的穷尽 match）。
            // `ContentBlock` 是 `#[non_exhaustive]`，外部 crate 无法穷尽列举 —— 这里把
            // `Text` 与**任何将来新增的变体**一并交给它：算不算正文由定义 crate 决定。
            block => block.as_text().map_or(0, estimate_text_tokens),
        })
        .sum()
}

/// 估算文本字符串中的 token 数
pub fn estimate_text_tokens(text: &str) -> usize {
    let mut cjk_count = 0u64;
    let mut total_bytes = text.len() as u64;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk_count += 1;
            // UTF-8 CJK = 3 字节，已计入 len()，需按 /4 调整
            total_bytes -= 2;
        }
    }
    let ascii_tokens = (total_bytes as usize) / 4;
    let cjk_tokens = (cjk_count as f64 * 1.5) as usize;
    ascii_tokens + cjk_tokens
}

fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{3000}'..='\u{303F}'
            | '\u{FF00}'..='\u{FFEF}'
    )
}

/// 估算 session 中全部消息的总 token 数
pub fn estimate_session_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_tokens).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::Role;

    #[test]
    fn test_estimate_text_tokens_ascii() {
        // "hello" = 5 字节，5/4 = 1
        assert_eq!(estimate_text_tokens("hello"), 1);
        // "abcdefgh" = 8 字节，8/4 = 2
        assert_eq!(estimate_text_tokens("abcdefgh"), 2);
    }

    #[test]
    fn test_estimate_text_tokens_cjk() {
        // "你好" = 2 个 CJK 字符，共 6 字节，调整：6-2*2=2，2/4=0，cjk：2*1.5=3
        assert_eq!(estimate_text_tokens("你好"), 3);
    }

    #[test]
    fn test_estimate_text_tokens_mixed() {
        // "hi你好" = "hi"（2 字节）+ "你好"（6 字节，2 个 CJK 字符）
        // total_bytes = 8，调整：8-2*2=4，ascii=4/4=1，cjk=2*1.5=3，合计 4
        assert_eq!(estimate_text_tokens("hi你好"), 4);
    }

    #[test]
    fn test_estimate_text_tokens_empty() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_message() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello world".into(),
            }],
        };
        // "hello world" = 11 字节，11/4 = 2
        assert_eq!(estimate_tokens(&msg), 2);
    }

    /// T12t（K3）：同一段文本加一张图 ⇒ 估算**恰多** `IMAGE_TOKEN_ESTIMATE`。
    ///
    /// 断言差值而非绝对值：绝对值会随 `estimate_text_tokens` 的实现漂移（分母、CJK 系数），
    /// 「图片不便宜」的信号只由差值承载。
    ///
    /// 变异自检：把 `ContentBlock::Image` 臂回退成落入 `_ => 0` ⇒ 差值变 0，本用例红。
    #[test]
    fn test_estimate_tokens_counts_image_block() {
        let text = "x".repeat(100);
        let text_only = Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.clone() }],
        };
        let with_image = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text },
                ContentBlock::Image {
                    path: "/tmp/a.png".into(),
                    detail: Some(ys_core::ImageDetail::High),
                },
            ],
        };
        assert_eq!(
            estimate_tokens(&with_image) - estimate_tokens(&text_only),
            IMAGE_TOKEN_ESTIMATE,
            "一张图必须恰计 {IMAGE_TOKEN_ESTIMATE} token"
        );
        // 两张图 = 两倍：计费不因同一条消息而打邓。
        let two_images = Message {
            role: Role::User,
            content: vec![
                text_only.content[0].clone(),
                ContentBlock::Image {
                    path: "/tmp/a.png".into(),
                    detail: None,
                },
                ContentBlock::Image {
                    path: "/tmp/b.png".into(),
                    detail: None,
                },
            ],
        };
        assert_eq!(
            estimate_tokens(&two_images) - estimate_tokens(&text_only),
            2 * IMAGE_TOKEN_ESTIMATE
        );
    }

    #[test]
    fn test_estimate_session_tokens() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "world".into(),
                }],
            },
        ];
        // "hello" = 1 个 token，"world" = 1 个 token
        assert_eq!(estimate_session_tokens(&messages), 2);
    }
}
