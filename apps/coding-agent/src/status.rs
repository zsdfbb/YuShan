use ys_core::Usage;

/// TUI 层累加器。数据来源：每个 turn 结束时的 `RunResult.usage`。
#[derive(Default, Clone, Copy, Debug)]
pub struct TurnStats {
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
}

impl TurnStats {
    #[inline]
    pub fn record(&mut self, usage: &Usage) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(usage.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(usage.output_tokens);
        self.turn_count = self.turn_count.saturating_add(1);
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
        });
        stats.record(&Usage {
            input_tokens: 50,
            output_tokens: 75,
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
            });
        }
        assert_eq!(stats.turn_count, 3);
        assert_eq!(stats.total_input_tokens, 3);
        assert_eq!(stats.total_output_tokens, 3);
    }
}
