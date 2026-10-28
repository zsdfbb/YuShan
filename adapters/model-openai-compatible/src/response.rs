use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ChatUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// 部分兼容端点不返回该字段；缺失时按 0 计，不使整包反序列化失败。
    #[serde(default)]
    pub total_tokens: u32,
    /// DeepSeek 方言的 prompt cache 命中 token 数。
    ///
    /// `Option` 而非 `u32`：**字段缺席**（不报缓存）与**显式报 0**（真 0 命中）
    /// 是两件事，混同会让「测不到」被渲染成「0% 命中」。
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    /// 同上，未命中部分。
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
    /// **OpenAI 标准形状**的明细：`{"cached_tokens": N}`。
    ///
    /// 与上面两个字段是**两家不同方言**：DeepSeek 报前者，OpenAI 兼容网关
    /// （实测 `opencode-go`）只报这里。**只认一家会让另一家的缓存读数恒为「无」**。
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// OpenAI 标准形状的完成侧明细：`{"reasoning_tokens": N}`。
    ///
    /// `None` = 整块明细缺失（provider 未上报）；`Some(..)` = 报了这一块。
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// `prompt_tokens_details` 的已知子集（其余字段忽略）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

/// `completion_tokens_details` 的已知子集（其余字段忽略）。
///
/// 明细块存在但 `reasoning_tokens` 缺失时按 0 计，仍视为「已上报」。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

impl ChatUsage {
    /// 归一成 `(prompt_total, Option<hit>)`，兼容两种方言。
    ///
    /// - **DeepSeek 方言**：`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
    ///   **任一在场**即算有读数 —— `Some(0)`（显式报的真 0 命中）与字段缺席是两件事，
    ///   不得用 `> 0` 混同（那会让「测不到」变成「0% 命中」）。只报 miss 时反推 hit。
    /// - **OpenAI 形状**：认 `prompt_tokens_details.cached_tokens`。
    /// - **两样都不在场** ⇒ `None`，表示**没有读数**。**不得**推成「全部未命中」。
    ///
    /// `miss` 不返回：它等于 `prompt_total - hit`，由 `Usage::cache_split()` 推导。
    pub fn cache_reading(&self) -> (u32, Option<u32>) {
        if let Some(hit) = self.prompt_cache_hit_tokens {
            return (self.prompt_tokens, Some(hit));
        }
        if let Some(miss) = self.prompt_cache_miss_tokens {
            // 只报未命中：`hit = prompt − miss`（方言没单独给命中数）。
            return (
                self.prompt_tokens,
                Some(self.prompt_tokens.saturating_sub(miss)),
            );
        }
        match &self.prompt_tokens_details {
            Some(d) => (self.prompt_tokens, Some(d.cached_tokens)),
            None => (self.prompt_tokens, None),
        }
    }

    /// 推理 token 数：`None` = provider 未上报该明细块；`Some(0)` = 上报了真 0。
    ///
    /// 两者语义不同，**不得**把 `Some(0)` 折叠成 `None`。
    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
    }
}

/// SSE 流式 chunk
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChunk {
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    /// 部分端点的终止 chunk 只带 `finish_reason` 而不带 `delta`；
    /// 缺失时按空 `Delta` 处理，避免整行反序列化失败被静默跳过。
    #[serde(default)]
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek 等 provider 用该字段承载思考内容（流式增量）。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolCall {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkFunction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ChatUsage {
        serde_json::from_str(json).expect("usage 应能反序列化")
    }

    /// **两种方言都要认**：DeepSeek 报 `prompt_cache_*`，OpenAI 兼容网关报
    /// `prompt_tokens_details.cached_tokens`。只认一家 ⇒ 另一家恒为「无读数」，
    /// 会把「缓存明明命中」误判成「测不到缓存」。
    ///
    /// 变异：把 `cache_reading` 的回落分支删掉（直接返回 DeepSeek 那两个字段）
    /// ⇒ 第二条断言变红。
    #[test]
    fn cache_reading_handles_both_dialects() {
        // DeepSeek 方言：`miss` 由 `prompt - hit` 推导，不在本函数里算。
        let ds = parse(
            r#"{"prompt_tokens":9735,"completion_tokens":1,
                "prompt_cache_hit_tokens":9600,"prompt_cache_miss_tokens":135}"#,
        );
        assert_eq!(ds.cache_reading(), (9735, Some(9600)));

        // OpenAI 标准形状（实测 opencode-go 只报这个）
        let oa = parse(
            r#"{"prompt_tokens":9760,"completion_tokens":4,
                "prompt_tokens_details":{"cached_tokens":9728}}"#,
        );
        assert_eq!(oa.cache_reading(), (9760, Some(9728)));

        // 两样都没有 ⇒ `None`（这是**「没有读数」**，不是「没命中」）
        let none = parse(r#"{"prompt_tokens":11,"completion_tokens":7}"#);
        assert_eq!(none.cache_reading(), (11, None));

        // 首调用：OpenAI 形状**显式**报 0 命中 —— 真 0 不是未上报。
        let first = parse(
            r#"{"prompt_tokens":9760,"completion_tokens":4,
                "prompt_tokens_details":{"cached_tokens":0}}"#,
        );
        assert_eq!(
            first.cache_reading(),
            (9760, Some(0)),
            "显式报 0 必须落在 Some(0)，不得折叠成 None"
        );

        // DeepSeek 方言**显式**报 `hit=0, miss=1000`（真实形态：整段 prompt 都没命中）。
        let ds_miss = parse(
            r#"{"prompt_tokens":1000,"completion_tokens":7,
                "prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":1000}"#,
        );
        assert_eq!(
            ds_miss.cache_reading(),
            (1000, Some(0)),
            "DS 显式 hit=0 是真 0 命中，不得用 `> 0` 误判成未上报"
        );

        // 只报 miss：hit 由 `prompt − miss` 反推（方言没单独给命中数）。
        let miss_only = parse(
            r#"{"prompt_tokens":1000,"completion_tokens":7,
                "prompt_cache_miss_tokens":250}"#,
        );
        assert_eq!(miss_only.cache_reading(), (1000, Some(750)));
    }

    /// 推理 token：整块明细缺失 ⇒ `None`；显式 `0` ⇒ `Some(0)`。
    ///
    /// 两者语义不同（未上报 vs 上报了真 0），上层靠它决定是否把该字段写进 JSON。
    ///
    /// 变异说明：
    /// - 删掉 `ChatUsage::completion_tokens_details` 字段 ⇒ 第一条断言变红
    ///   （serde 忽略未知键，解析为 `None`）。
    /// - 把 `reasoning_tokens()` 改成 `filter(|n| *n > 0)` 或把字段改成裸 `u32`
    ///   ⇒ 第三条断言变红（`Some(0)` 被折叠成 `None`）。
    #[test]
    fn reasoning_tokens_distinguishes_missing_from_zero() {
        let reported = parse(
            r#"{"prompt_tokens":11,"completion_tokens":4,
                "completion_tokens_details":{"reasoning_tokens":4}}"#,
        );
        assert_eq!(
            reported.reasoning_tokens(),
            Some(4),
            "provider 上报了推理 token 数"
        );

        let missing = parse(r#"{"prompt_tokens":11,"completion_tokens":4}"#);
        assert_eq!(
            missing.reasoning_tokens(),
            None,
            "整块明细缺失 = 未上报，不能当成 0"
        );

        let zero = parse(
            r#"{"prompt_tokens":11,"completion_tokens":4,
                "completion_tokens_details":{"reasoning_tokens":0}}"#,
        );
        assert_eq!(
            zero.reasoning_tokens(),
            Some(0),
            "显式报 0 是真 0，不得折叠成 None"
        );
    }
}
