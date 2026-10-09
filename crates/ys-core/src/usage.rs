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
}

impl std::ops::Add for Usage {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            prompt_cache_hit_tokens: self.prompt_cache_hit_tokens + rhs.prompt_cache_hit_tokens,
            prompt_cache_miss_tokens: self.prompt_cache_miss_tokens + rhs.prompt_cache_miss_tokens,
        }
    }
}
