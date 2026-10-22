use std::time::Duration;
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
    /// **执行时长**累计（Σ 各回合实际占用的墙钟）。
    ///
    /// 与「存活时长」（进程起止，`CodingView.session_started`）是**两个口径**：
    /// 二者之差 = 用户不在时 TUI 空等输入的时长。
    ///
    /// **为什么要分开**：一次 15 小时的会话可能只实际执行了 3 小时 ——
    /// 只报存活时长会把「空等」误读成「干了 15 小时」。
    pub executed: Duration,
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
        self.total_reasoning_tokens = match (self.total_reasoning_tokens, usage.reasoning_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, None) => a,
            (None, b) => b,
        };
        self.turn_count = self.turn_count.saturating_add(1);
    }

    /// 累计本回合实际占用的墙钟（**执行口径**）。
    ///
    /// 调用点在**回合返回后** —— 与 [`Self::record`] 同一位置，
    /// 保证「token 统计」与「时长统计」覆盖同一批回合。
    pub fn record_elapsed(&mut self, elapsed: Duration) {
        self.executed = self.executed.saturating_add(elapsed);
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

    /// A1 的承重不变量：**执行时长**与**存活时长**是两个口径。
    ///
    /// 执行时长只能由 `record_elapsed` 累加；默认必须是零
    /// （否则「还没跑任何回合」会被误报成已执行）。
    ///
    /// 变异：删掉 `record_elapsed` 里的 `saturating_add` ⇒ 第二次累加后
    /// 仍停在第一次的值 ⇒ 变红。
    #[test]
    fn executed_duration_accumulates_independently_of_token_stats() {
        let mut stats = TurnStats::default();
        assert_eq!(stats.executed, Duration::ZERO, "默认执行时长必须是零");

        stats.record_elapsed(Duration::from_secs(30));
        stats.record_elapsed(Duration::from_secs(45));
        assert_eq!(stats.executed, Duration::from_secs(75));

        // 与 token 统计**互不影响** —— 两条口径各走各的累加器。
        stats.record(&Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..Default::default()
        });
        assert_eq!(
            stats.executed,
            Duration::from_secs(75),
            "记 usage 不应改时长"
        );
        assert_eq!(stats.total_input_tokens, 100);
    }
}
