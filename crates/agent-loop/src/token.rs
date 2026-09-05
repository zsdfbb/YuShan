use agent_core::{ContentBlock, Message};

/// Estimate the number of tokens in a message (chars/4 + CJK compensation)
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

/// Estimate the number of tokens in a text string
pub fn estimate_text_tokens(text: &str) -> usize {
    let mut cjk_count = 0u64;
    let mut total_bytes = text.len() as u64;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk_count += 1;
            // UTF-8 CJK = 3 bytes, counted in len(), adjust for /4
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

/// Estimate total tokens for a session's messages
pub fn estimate_session_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| estimate_tokens(m)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::Role;

    #[test]
    fn test_estimate_text_tokens_ascii() {
        // "hello" = 5 bytes, 5/4 = 1
        assert_eq!(estimate_text_tokens("hello"), 1);
        // "abcdefgh" = 8 bytes, 8/4 = 2
        assert_eq!(estimate_text_tokens("abcdefgh"), 2);
    }

    #[test]
    fn test_estimate_text_tokens_cjk() {
        // "你好" = 2 CJK chars, 6 bytes total, adjust: 6-2*2=2, 2/4=0, cjk: 2*1.5=3
        assert_eq!(estimate_text_tokens("你好"), 3);
    }

    #[test]
    fn test_estimate_text_tokens_mixed() {
        // "hi你好" = "hi" (2 bytes) + "你好" (6 bytes, 2 CJK)
        // total_bytes = 8, adjust: 8-2*2=4, ascii=4/4=1, cjk=2*1.5=3, total=4
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
        // "hello world" = 11 bytes, 11/4 = 2
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
        // "hello" = 1 token, "world" = 1 token
        assert_eq!(estimate_session_tokens(&messages), 2);
    }
}
