//! agent-core: 共享词汇与原语

mod cancel;
mod error;
mod id;
mod message;
mod stop;
mod tool;
mod usage;

pub use cancel::*;
pub use error::*;
pub use id::*;
pub use message::*;
pub use stop::*;
pub use tool::*;
pub use usage::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serde_roundtrip() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn test_tool_call_serde_roundtrip() {
        let call = ToolCall {
            id: ToolCallId("tc-1".into()),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call, back);
    }

    #[test]
    fn test_cancel_token() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_usage_add() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 20,
        };
        let b = Usage {
            input_tokens: 5,
            output_tokens: 15,
        };
        let c = a + b;
        assert_eq!(
            c,
            Usage {
                input_tokens: 15,
                output_tokens: 35,
            }
        );
    }

    #[test]
    fn test_stop_reason_serde() {
        let reasons = vec![
            StopReason::Completed,
            StopReason::MaxRounds,
            StopReason::Cancelled,
        ];
        for r in reasons {
            let json = serde_json::to_string(&r).unwrap();
            let back: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }
}
