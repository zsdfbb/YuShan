use ys_core::Usage;

/// TUI 层累加器。数据来源：每个 turn 结束时的 `RunResult.usage`。
#[derive(Default, Clone, Copy, Debug)]
pub struct TurnStats {
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
    /// 缓存命中的 prompt token 累计（debug `run_end.usage` 用）。
    pub total_cache_hit_tokens: u32,
    /// 缓存未命中的 prompt token 累计。
    pub total_cache_miss_tokens: u32,
    /// 推理 token 累计；`None` = 本 run 内没有任何一轮上报过。
    pub total_reasoning_tokens: Option<u32>,
}

impl TurnStats {
    #[inline]
    pub fn record(&mut self, usage: &Usage) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(usage.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(usage.output_tokens);
        self.total_cache_hit_tokens = self
            .total_cache_hit_tokens
            .saturating_add(usage.prompt_cache_hit_tokens);
        self.total_cache_miss_tokens = self
            .total_cache_miss_tokens
            .saturating_add(usage.prompt_cache_miss_tokens);
        self.total_reasoning_tokens = match (self.total_reasoning_tokens, usage.reasoning_tokens)
        {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, None) => a,
            (None, b) => b,
        };
        self.turn_count = self.turn_count.saturating_add(1);
    }

    /// 本 run 的累计 usage（`run_end` 用）。
    pub fn cumulative_usage(&self) -> Usage {
        Usage {
            input_tokens: self.total_input_tokens,
            output_tokens: self.total_output_tokens,
            prompt_cache_hit_tokens: self.total_cache_hit_tokens,
            prompt_cache_miss_tokens: self.total_cache_miss_tokens,
            reasoning_tokens: self.total_reasoning_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_all_zero() {
        let stats = TurnStats::default();
        assert_eq!(stats.total_input_tokens, 0);
        assert_eq!(stats.total_output_tokens, 0);
        assert_eq!(stats.turn_count, 0);
    }

    #[test]
    fn test_record_normal() {
        let mut stats = TurnStats::default();
        stats.record(&Usage {
            input_tokens: 100,
            output_tokens: 200,
            ..Default::default()
        });
        stats.record(&Usage {
            input_tokens: 50,
            output_tokens: 75,
            ..Default::default()
        });
        assert_eq!(stats.total_input_tokens, 150);
        assert_eq!(stats.total_output_tokens, 275);
        assert_eq!(stats.turn_count, 2);
    }

    #[test]
    fn test_record_saturating() {
        let mut stats = TurnStats {
            total_input_tokens: u32::MAX,
            ..Default::default()
        };
        stats.record(&Usage {
            input_tokens: 100,
            output_tokens: 200,
            ..Default::default()
        });
        assert_eq!(stats.total_input_tokens, u32::MAX); // 不应溢出
        assert_eq!(stats.total_output_tokens, 200);
    }

    #[test]
    fn test_record_count() {
        let mut stats = TurnStats::default();
        for _ in 0..3 {
            stats.record(&Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            });
        }
        assert_eq!(stats.turn_count, 3);
        assert_eq!(stats.total_input_tokens, 3);
        assert_eq!(stats.total_output_tokens, 3);
    }
}
