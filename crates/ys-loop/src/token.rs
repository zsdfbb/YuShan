use ys_core::{ContentBlock, Message};

/// 估算一条消息中的 token 数（字符数/4 + CJK 补偿）
pub fn estimate_tokens(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => estimate_text_tokens(text),
            ContentBlock::ToolUse { arguments, .. } => estimate_text_tokens(&arguments.to_string()),
            ContentBlock::ToolResult { content, .. } => estimate_text_tokens(content),
            _ => 0,
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
    messages.iter().map(|m| estimate_tokens(m)).sum()
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
