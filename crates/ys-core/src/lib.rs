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
            prompt_cache_hit_tokens: Some(7),
            prompt_cache_miss_tokens: None,
            reasoning_tokens: None,
        };
        let b = Usage {
            input_tokens: 1,
            output_tokens: 2,
            prompt_cache_hit_tokens: Some(5),
            prompt_cache_miss_tokens: None,
            reasoning_tokens: None,
        };
        let c = a + b;
        assert_eq!(c.prompt_cache_hit_tokens, Some(12));
        assert_eq!(
            c.prompt_cache_miss_tokens, None,
            "`miss` 恒 `None`：由 `input − hit` 推导，不落地"
        );
    }

    /// 三个 `Option` 桶共用同一条 `Add` 规则：两侧都 `Some` 才相加，
    /// 否则继承已上报侧，**不得**把 `Some(0)` 折叠成 `None`。
    ///
    /// 变异：把任一侧的 `(x, None) => x` / `(None, y) => y` 改成「`Some(0)` 也退回 `None`」
    /// ⇒ 前两条断言变红；把相加分支删掉 ⇒ 第三条变红。
    #[test]
    fn test_usage_add_shares_one_rule_across_option_buckets() {
        let with = |hit, miss, reasoning| Usage {
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            reasoning_tokens: reasoning,
            ..Default::default()
        };
        let zero = with(Some(0), Some(0), Some(0));
        let absent = with(None, None, None);
        let three = with(Some(3), Some(3), Some(3));
        let four = with(Some(4), Some(4), Some(4));

        let sum = zero.clone() + absent.clone();
        assert_eq!(sum.prompt_cache_hit_tokens, Some(0), "Some(0) + None");
        assert_eq!(sum.prompt_cache_miss_tokens, Some(0), "Some(0) + None");
        assert_eq!(sum.reasoning_tokens, Some(0), "Some(0) + None");

        let sum = absent + zero;
        assert_eq!(sum.prompt_cache_hit_tokens, Some(0), "None + Some(0)");
        assert_eq!(sum.prompt_cache_miss_tokens, Some(0), "None + Some(0)");
        assert_eq!(sum.reasoning_tokens, Some(0), "None + Some(0)");

        let sum = three + four;
        assert_eq!(sum.prompt_cache_hit_tokens, Some(7), "Some(3) + Some(4)");
        assert_eq!(sum.prompt_cache_miss_tokens, Some(7), "Some(3) + Some(4)");
        assert_eq!(sum.reasoning_tokens, Some(7), "Some(3) + Some(4)");
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
