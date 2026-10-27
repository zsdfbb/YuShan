//! ys-core: 共享词汇与原语

mod effort;
mod error;
mod id;
mod message;
mod stop;
mod tool;
mod usage;

pub use effort::*;
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
    fn test_usage_add() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 20,
            ..Default::default()
        };
        let b = Usage {
            input_tokens: 5,
            output_tokens: 15,
            ..Default::default()
        };
        let c = a + b;
        assert_eq!(
            c,
            Usage {
                input_tokens: 15,
                output_tokens: 35,
                ..Default::default()
            }
        );
    }

    /// 缓存读数必须随 `Add` 一起累加（子 agent usage 上卷靠这条）。
    #[test]
    fn test_usage_add_carries_cache_fields() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 20,
            prompt_cache_hit_tokens: 7,
            prompt_cache_miss_tokens: 3,
            reasoning_tokens: None,
        };
        let b = Usage {
            input_tokens: 1,
            output_tokens: 2,
            prompt_cache_hit_tokens: 5,
            prompt_cache_miss_tokens: 4,
            reasoning_tokens: None,
        };
        let c = a + b;
        assert_eq!(c.prompt_cache_hit_tokens, 12);
        assert_eq!(c.prompt_cache_miss_tokens, 7);
    }

    #[test]
    fn test_stop_reason_serde() {
        let reasons = vec![
            StopReason::Completed,
            StopReason::MaxRounds,
            StopReason::Cancelled,
            StopReason::Truncated,
            StopReason::NoOutput,
            StopReason::Failed,
        ];
        for r in reasons {
            let json = serde_json::to_string(&r).unwrap();
            let back: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }

    // `Truncated` 是加法式新增变体，但它会出现在 `RunFinished.stop_reason` 里，
    // 经 `--json` 模式逐行打印 —— 序列化出的字符串是**对外契约**，必须钉死。
    #[test]
    fn test_stop_reason_truncated_serde_roundtrip() {
        let json = serde_json::to_string(&StopReason::Truncated).unwrap();
        assert_eq!(json, "\"Truncated\"", "序列化字符串是对外契约，不得改名");
        let back: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, StopReason::Truncated);
    }

    // `NoOutput` 同 `Truncated`：加法式新增，会出现在 `RunFinished.stop_reason` 里并经 `--json`
    // 打印 —— 序列化字符串是**对外契约**，必须钉死。
    //
    // 变异：把变体改名为 `Empty` ⇒ 本用例变红（字面量变了）。
    #[test]
    fn test_stop_reason_no_output_serde_roundtrip() {
        let json = serde_json::to_string(&StopReason::NoOutput).unwrap();
        assert_eq!(json, "\"NoOutput\"", "序列化字符串是对外契约，不得改名");
        let back: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, StopReason::NoOutput);
    }

    // `Failed` 同 `Truncated` / `NoOutput`：加法式新增，会出现在 `RunFinished.stop_reason`
    // 里并经 `--json` 打印 —— 序列化字符串是**对外契约**，必须钉死。
    //
    // 变异：把变体改名为 `Error` / `ProviderError` ⇒ 本用例变红（字面量变了）。
    #[test]
    fn test_stop_reason_failed_serde_roundtrip() {
        let json = serde_json::to_string(&StopReason::Failed).unwrap();
        assert_eq!(json, "\"Failed\"", "序列化字符串是对外契约，不得改名");
        let back: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, StopReason::Failed);
    }
}
