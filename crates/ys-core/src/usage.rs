use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 命中 prompt cache 的输入 token 数（provider 若上报）。
    ///
    /// 加法式字段（`#[serde(default)]`）：老会话 JSONL / 不返回该字段的 provider 一律按 0，
    /// 不使反序列化失败。用途：把「缓存有没有生效、省了多少」从估算变成**服务端读数**。
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    /// 未命中 prompt cache 的输入 token 数（provider 若上报）。
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    /// 推理 token 数（provider 若上报）。
    ///
    /// `None` = provider 未上报；`Some(0)` = 上报了真 0。两者语义不同，不能合并。
    /// 未上报时**不序列化该键**：`RunFinished.usage` 的既有 JSON 形态逐字节不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

impl std::ops::Add for Usage {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            prompt_cache_hit_tokens: self.prompt_cache_hit_tokens + rhs.prompt_cache_hit_tokens,
            prompt_cache_miss_tokens: self.prompt_cache_miss_tokens + rhs.prompt_cache_miss_tokens,
            // 只有两侧都上报才可累加；任一侧未上报，结果继承已上报的那一侧。
            // 若恒为 `None`，多轮累加后已上报的推理 token 会被吞掉。
            reasoning_tokens: match (self.reasoning_tokens, rhs.reasoning_tokens) {
                (Some(a), Some(b)) => Some(a + b),
                (a, None) => a,
                (None, b) => b,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Usage;

    /// 默认 `None`：provider 未上报与「真 0」必须可区分。
    #[test]
    fn reasoning_tokens_default_is_none() {
        assert_eq!(Usage::default().reasoning_tokens, None);
    }

    /// `Add` 的各组合，含 `Some(0)`（真 0）与 `None`（未上报）的边界。
    ///
    /// 变异说明：
    /// - 把 `reasoning_tokens` 的组合改成恒 `None` ⇒ 第二至五条断言变红。
    /// - 把 `(a, None) => a` 改成「`a == Some(0)` 时退回 `None`」⇒ 第四条断言变红
    ///   （`Some(0) + None` 不得被折叠成 `None`）。
    /// - 把 `(None, b) => b` 同法改 ⇒ 第五条断言变红（`None + Some(0)` 同理）。
    #[test]
    fn reasoning_tokens_add() {
        let with = |reasoning_tokens| Usage {
            reasoning_tokens,
            ..Default::default()
        };

        assert_eq!(
            (with(None) + with(None)).reasoning_tokens,
            None,
            "两侧都未上报 ⇒ None"
        );
        assert_eq!(
            (with(None) + with(Some(4))).reasoning_tokens,
            Some(4),
            "左侧未上报 ⇒ 继承右侧"
        );
        assert_eq!(
            (with(Some(4)) + with(None)).reasoning_tokens,
            Some(4),
            "右侧未上报 ⇒ 继承左侧"
        );
        assert_eq!(
            (with(Some(0)) + with(None)).reasoning_tokens,
            Some(0),
            "真 0 + 未上报 ⇒ 仍是真 0（不能折叠成 None）"
        );
        assert_eq!(
            (with(None) + with(Some(0))).reasoning_tokens,
            Some(0),
            "未上报 + 真 0 ⇒ 仍是真 0（不能折叠成 None）"
        );
        assert_eq!(
            (with(Some(4)) + with(Some(3))).reasoning_tokens,
            Some(7),
            "两侧都上报 ⇒ 相加"
        );
    }

    /// 旧 JSON（无该键）反序列化为 `None`。
    #[test]
    fn reasoning_tokens_deserialize_missing_key() {
        let usage: Usage = serde_json::from_str(r#"{"input_tokens":1,"output_tokens":2}"#).unwrap();
        assert_eq!(usage.reasoning_tokens, None);
    }

    /// `None` 序列化后不含该键；`Some(0)` 含该键且 roundtrip 仍为 `Some(0)`。
    ///
    /// 变异说明：去掉 `skip_serializing_if = "Option::is_none"` ⇒ 第一条断言变红；
    /// 把字段改成裸 `u32` ⇒ 第一条断言同样变红（任何 usage 都会多出键）。
    #[test]
    fn reasoning_tokens_serialization_distinguishes_none_from_zero() {
        let none: serde_json::Value =
            serde_json::to_value(Usage::default()).expect("Usage 可序列化");
        assert!(
            none.get("reasoning_tokens").is_none(),
            "未上报时不得写出 reasoning_tokens 键，实际: {none}"
        );

        let zero = Usage {
            reasoning_tokens: Some(0),
            ..Default::default()
        };
        let json = serde_json::to_value(&zero).expect("Usage 可序列化");
        assert_eq!(
            json.get("reasoning_tokens"),
            Some(&serde_json::json!(0)),
            "真 0 必须写出该键"
        );
        let back: Usage = serde_json::from_value(json).expect("Usage 可反序列化");
        assert_eq!(back.reasoning_tokens, Some(0), "roundtrip 保持真 0");
    }
}
