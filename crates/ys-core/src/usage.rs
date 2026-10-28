use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// provider 回报的 prompt **总量**（含缓存命中部分）。
    ///
    /// 与 provider 是否上报缓存明细**无关** —— 这是唯一能跨 provider 比较的量。
    /// 缓存命中率的分母**不得**用它，用 [`Self::cache_split`]。
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 命中缓存的 prompt token 数；`None` = **provider 未上报**（≠ 0 命中）。
    ///
    /// `#[serde(default)]` 让老会话 JSONL / 不返回该字段的 provider 一律按 `None`，
    /// 不使反序列化失败。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u32>,
    /// **恒 `None`**：`miss = input_tokens - hit` 可推导，不再落地。
    /// 保留字段仅为老 JSONL 能反序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u32>,
    /// 推理 token 数（provider 若上报）。
    ///
    /// `None` = provider 未上报；`Some(0)` = 上报了真 0。两者语义不同，不能合并。
    /// 未上报时**不序列化该键**：`RunFinished.usage` 的既有 JSON 形态逐字节不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

/// 所有 `Option` 桶共用的加法规则：两侧都上报才相加，否则继承已上报的那一侧。
///
/// **不得**把 `Some(0)`（真 0）折叠成 `None`（未上报）—— 若恒为 `None`，
/// 多轮累加后已上报的值会被吞掉；若把真 0 当成缺席，`Some(0) + None` 会退化成 `None`。
#[inline]
fn add_opt(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (x, None) => x,
        (None, y) => y,
    }
}

impl Usage {
    /// `(hit, miss)`；`None` = 本对象**不自带**缓存读数。
    ///
    /// `miss = input_tokens.saturating_sub(hit)` —— 单轮可推导，故不存储。
    ///
    /// # 聚合前提（承重，误用会让命中率静默偏低）
    ///
    /// 只有在「缓存读数缺席时 `input_tokens` 也为 0」时，`input − hit` 才能在
    /// **多轮累加**后仍等于真实 `miss`。若某个 provider 回了 prompt 却不回缓存明细，
    /// 那些轮的整个 `input` 会被算成未命中 ⇒ 命中率偏低。
    ///
    /// 因此：**永远不要在聚合后的 `Usage` 上调本函数**。
    /// 聚合必须**逐轮调用本函数**、把 `(hit, miss)` 累加进专用计数器。
    /// 原因：`input_tokens` 无条件相加，而 `hit` 缺席时不参与相加 —— 两者不对称。
    #[inline]
    pub fn cache_split(&self) -> Option<(u32, u32)> {
        self.prompt_cache_hit_tokens
            .map(|hit| (hit, self.input_tokens.saturating_sub(hit)))
    }

    /// 本对象是否带缓存读数（`Some(0)` 也算上报）。
    #[inline]
    pub fn is_cache_reported(&self) -> bool {
        self.prompt_cache_hit_tokens.is_some()
    }
}

impl std::ops::Add for Usage {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            prompt_cache_hit_tokens: add_opt(
                self.prompt_cache_hit_tokens,
                rhs.prompt_cache_hit_tokens,
            ),
            prompt_cache_miss_tokens: add_opt(
                self.prompt_cache_miss_tokens,
                rhs.prompt_cache_miss_tokens,
            ),
            reasoning_tokens: add_opt(self.reasoning_tokens, rhs.reasoning_tokens),
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

    /// 未上报缓存 ⇒ 没有读数（**不是** `(0, input)`）。
    ///
    /// 变异：把 `cache_split` 改成 `unwrap_or(0)` 后恒返回 `Some` ⇒ 本用例变红，
    /// 上层会把「测不到」渲染成「0% 命中」。
    #[test]
    fn cache_split_returns_none_without_reading() {
        let usage = Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: None,
            ..Default::default()
        };
        assert_eq!(usage.cache_split(), None);
        assert!(!usage.is_cache_reported());
    }

    /// `miss` 由 `input − hit` 推导。
    ///
    /// 变异：把 `miss` 硬写成 0 ⇒ 本用例变红（命中率会永远 100%）。
    #[test]
    fn cache_split_derives_miss() {
        let usage = Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: Some(98),
            ..Default::default()
        };
        assert_eq!(usage.cache_split(), Some((98, 2)));
        assert!(usage.is_cache_reported());
    }

    /// provider 异常（`hit > input`）饱和到 0，不得 panic。
    #[test]
    fn cache_split_saturates() {
        let usage = Usage {
            input_tokens: 10,
            prompt_cache_hit_tokens: Some(20),
            ..Default::default()
        };
        assert_eq!(usage.cache_split(), Some((20, 0)));
    }

    /// 正常 provider 读数下 `hit + miss == input_tokens`（`miss` 由减法定义）。
    ///
    /// 这是 B 口径的可检不变量：`input_tokens` 是 provider 报的 prompt 总量，
    /// `hit` 是它的一部分。**但它在 provider 异常时报废** ——
    /// 见 `cache_split_saturates`：`hit > input` 时 `miss` 被钳到 0，
    /// 于是 `hit + miss > input`。故本不变量用于**发现异常**，不是**保证成立**。
    #[test]
    fn cache_split_conserves_input_when_provider_is_sane() {
        let usage = Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: Some(98),
            ..Default::default()
        };
        let (hit, miss) = usage.cache_split().expect("有读数");
        assert_eq!(
            hit + miss,
            usage.input_tokens,
            "hit + miss 必须守恒到 input"
        );
    }
}
