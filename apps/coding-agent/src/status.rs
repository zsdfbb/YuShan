use std::time::Duration;
use ys_core::Usage;
use ys_loop::RunResult;

/// TUI 层累加器。数据来源：每个 turn 结束时的 `RunResult.usage`。
///
/// **不实现 `Copy`**：压缩桶是 `Usage`（含 `Option` 桶，只 `Clone`）。调用方按引用传。
#[derive(Default, Clone, Debug)]
pub struct TurnStats {
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
    /// 缓存命中的 prompt token 累计（**只累加有上报的轮**）。
    pub total_cache_hit_tokens: u32,
    /// 缓存未命中的 prompt token 累计（**只累加有上报的轮**）。
    pub total_cache_miss_tokens: u32,
    /// 是否**任何一轮**上报过缓存读数。`false` ⇒ 显示 `-`，而不是 `0%`。
    pub cache_reported: bool,
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
    /// 本 run 内压缩（自动 + 手动 `/compact`）消耗的累计。
    ///
    /// 与 `total_input_tokens` / `total_output_tokens` 是**并列**的：
    /// 后者只含主对话轮（`RunFinished.usage` 的语义），压缩在这里单独记。
    /// **失败路径不计**（已知缺口，ADR-0061 D5）。
    pub total_compaction_usage: Usage,
}

impl TurnStats {
    /// 记账一批 usage。
    ///
    /// 缓存桶按 `cache_split()` **逐次**累加：`miss = input − hit` 只在**单次**里是
    /// 恒等式，故绝不在聚合后的 `Usage` 上派生（见 `Usage::cache_split` 的聚合前提）。
    /// 「没上报」的轮不碰分母 —— 既不进 `hit` 也不进 `miss`。
    #[inline]
    /// 记账一个**单轮** usage（测试与单轮调用方用）。
    ///
    /// 生产路径走 [`Self::record_result`]：回合聚合值不能用来派生缓存读数。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn record(&mut self, usage: &Usage) {
        // 单轮输入：`input − hit` 在单轮里是恒等式，可直接派生。
        self.record_with_cache(usage, usage.cache_split())
    }

    /// 记账一个**回合结果**：token 取累计值，缓存读数取 loop 的**逐轮**累加值。
    ///
    /// 缓存桶**不得**从 `result.usage` 派生：`miss = input − hit` 只在单轮里成立，
    /// 而 `usage.input_tokens` 无条件相加、`hit` 缺席时不参与相加（不对称）。
    /// 详见 `ys_loop::RunResult::cache_reading`。
    pub fn record_result(&mut self, result: &RunResult) {
        self.record_with_cache(&result.usage, result.cache_reading);
        // 压缩与主对话轮**并列**记账：`result.usage` 不含压缩消耗。
        // `Add for Usage` 是 move 语义，故两侧都要 clone。
        self.total_compaction_usage =
            self.total_compaction_usage.clone() + result.compaction_usage.clone();
    }

    /// 单次记账：`cache` **只取调用方给的值**，不再从 `usage` 兜底派生。
    ///
    /// 兜底看似安全（无读数时两边都是 `None`），但它留下一条活路径：
    /// 一旦有人把聚合 `usage` 传到 [`Self::record`]，`input − hit` 会静默算错。
    /// 故派生只发生在**明确的单轮入口** `record()` 里。
    fn record_with_cache(&mut self, usage: &Usage, cache: Option<(u32, u32)>) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(usage.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(usage.output_tokens);
        if let Some((hit, miss)) = cache {
            self.cache_reported = true;
            self.total_cache_hit_tokens = self.total_cache_hit_tokens.saturating_add(hit);
            self.total_cache_miss_tokens = self.total_cache_miss_tokens.saturating_add(miss);
        }
        self.total_reasoning_tokens = match (self.total_reasoning_tokens, usage.reasoning_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, None) => a,
            (None, b) => b,
        };
        self.turn_count = self.turn_count.saturating_add(1);
    }

    /// 压缩消耗的累计（未压缩过时全零）。
    pub fn compaction_usage(&self) -> &Usage {
        &self.total_compaction_usage
    }

    /// 本 run 是否发生过压缩。
    ///
    /// 判据是全零 usage：`RunResult.compaction_usage` 全零 = 没压过或压失败，
    /// 两种情形在显示上同为「无压缩读数」，不区分。
    pub fn has_compaction(&self) -> bool {
        self.total_compaction_usage != Usage::default()
    }

    /// 累计本回合实际占用的墙钟（**执行口径**）。
    ///
    /// 调用点在**回合返回后** —— 与 [`Self::record`] 同一位置，
    /// 保证「token 统计」与「时长统计」覆盖同一批回合。
    pub fn record_elapsed(&mut self, elapsed: Duration) {
        self.executed = self.executed.saturating_add(elapsed);
    }

    /// 命中率整数百分比；`None` = 无读数（渲染成 `-`）。
    ///
    /// 分母恒为 `hit + miss`（只含上报过的轮），**不是** `input_tokens`。
    /// 只有 `hit == hit + miss` 才给 100，避免四舍五入把 99.x 说成满命中。
    ///
    /// 展示面的消费点：状态栏的 `缓存 {p}%` 子段与 `/status` 的 `cache` 行 ——
    /// 二者都靠它把「有读数但算不出百分比」与「没有读数」分开。
    pub fn cache_hit_pct(&self) -> Option<u8> {
        if !self.cache_reported {
            return None;
        }
        let total = self.total_cache_hit_tokens as u64 + self.total_cache_miss_tokens as u64;
        if total == 0 {
            return None;
        }
        let hit = self.total_cache_hit_tokens as u64;
        if hit >= total {
            return Some(100);
        }
        Some((hit * 100 / total) as u8)
    }

    /// 本 run 的累计 usage（`run_end` 用）。
    ///
    /// 缓存命中桶按「是否上报过」投影回 `Option`：一次都没上报 ⇒ `None`，
    /// 不得写成 `Some(0)`（那会把「测不到」与「真 0 命中」混同）。
    ///
    /// `prompt_cache_miss_tokens` **恒 `None`**：它是 `input − hit` 的推导值，
    /// 本仓不落地该字段（`None` 与 `hit` 都在 `skip_serializing_if` 下，故不产生新键）。
    pub fn cumulative_usage(&self) -> Usage {
        Usage {
            input_tokens: self.total_input_tokens,
            output_tokens: self.total_output_tokens,
            prompt_cache_hit_tokens: self.cache_reported.then_some(self.total_cache_hit_tokens),
            prompt_cache_miss_tokens: None,
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
        assert!(!stats.cache_reported);
        assert_eq!(*stats.compaction_usage(), Usage::default());
    }

    /// 压缩桶**跨回合累加**，且与主对话轮桶**互不污染**。
    ///
    /// 变异：把 `record_result` 里那行累加删掉（或改成赋值而非 `+`）⇒
    /// 累计只剩最后一个回合的量 ⇒ 本用例变红。
    #[test]
    fn compaction_usage_accumulates_across_turns() {
        let mut stats = TurnStats::default();
        let turn = |main: u32, compact_in: u32, compact_out: u32| RunResult {
            stop_reason: ys_core::StopReason::Completed,
            usage: Usage {
                input_tokens: main,
                output_tokens: main,
                ..Default::default()
            },
            cache_reading: None,
            compaction_usage: Usage {
                input_tokens: compact_in,
                output_tokens: compact_out,
                ..Default::default()
            },
            rounds: 1,
            final_message: None,
        };

        stats.record_result(&turn(100, 1_000, 200));
        stats.record_result(&turn(50, 2_000, 300));

        assert_eq!(
            *stats.compaction_usage(),
            Usage {
                input_tokens: 3_000,
                output_tokens: 500,
                ..Default::default()
            },
            "两次压缩的消耗必须相加"
        );
        assert!(stats.has_compaction());
        // 主对话轮桶**不含**压缩 —— 两条口径并列而非嵌套。
        assert_eq!(stats.total_input_tokens, 150, "主轮 input 不被压缩污染");
        assert_eq!(stats.total_output_tokens, 150, "主轮 output 不被压缩污染");
    }

    /// 一次都没压缩过 ⇒ 全零，`has_compaction()` 为假（显示层据此整行省略）。
    #[test]
    fn has_compaction_false_when_never_compacted() {
        let mut stats = TurnStats::default();
        assert!(!stats.has_compaction(), "默认状态没有压缩");

        stats.record_result(&RunResult {
            stop_reason: ys_core::StopReason::Completed,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Default::default()
            },
            cache_reading: None,
            compaction_usage: Usage::default(),
            rounds: 1,
            final_message: None,
        });
        assert!(
            !stats.has_compaction(),
            "压过但零消耗（失败路径）也按「无压缩」显示"
        );
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

    /// 一次都没上报缓存 ⇒ 命中率是 `None`（渲染成 `-`），**不是** `0%`。
    ///
    /// 变异：把 `cache_reported` 的判据去掉（按 `total != 0` 判）⇒ 本用例变红。
    #[test]
    fn cache_hit_pct_none_when_never_reported() {
        let mut stats = TurnStats::default();
        stats.record(&Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: None,
            ..Default::default()
        });
        assert_eq!(stats.cache_hit_pct(), None);
        assert!(!stats.cache_reported);
        assert_eq!(
            stats.cumulative_usage().prompt_cache_hit_tokens,
            None,
            "没上报过 ⇒ 累计 usage 里也是 None，不是 Some(0)"
        );
    }

    /// 显式报 0 命中 ⇒ `Some(0)`，与「没上报」可区分。
    ///
    /// 变异：把 `record` 的 `if let Some(..)` 改成「`hit > 0` 才算上报」
    /// ⇒ 本用例变成 `None`，断言变红。
    #[test]
    fn cache_hit_pct_zero_is_distinct_from_unreported() {
        let mut stats = TurnStats::default();
        stats.record(&Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: Some(0),
            ..Default::default()
        });
        assert_eq!(stats.cache_hit_pct(), Some(0));
        assert_eq!(
            stats.cumulative_usage().prompt_cache_hit_tokens,
            Some(0),
            "真 0 必须投影成 Some(0)"
        );
    }

    /// 分母是 `hit + miss`（`hit + (input − hit)`），**不是** `input_tokens`：
    /// 缺报轮次不得进入分母。
    ///
    /// 变异：把分母换成 `total_input_tokens` ⇒ 缺报轮那 1000 个 prompt 会被算成
    /// 未命中，命中率从 98.8% 跌到 49.4% ⇒ 本用例变红。
    #[test]
    fn cache_hit_pct_uses_hit_plus_miss() {
        let mut stats = TurnStats::default();
        stats.record(&Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: Some(98),
            ..Default::default()
        });
        assert_eq!(stats.cache_hit_pct(), Some(98));

        // 再记一轮**没报**的：分母不动，命中率不变。
        stats.record(&Usage {
            input_tokens: 1000,
            prompt_cache_hit_tokens: None,
            ..Default::default()
        });
        assert_eq!(
            stats.cache_hit_pct(),
            Some(98),
            "缺报轮不得进入分母，也不得把 input 算成未命中"
        );
        assert_eq!(stats.total_cache_miss_tokens, 2, "缺报轮不碰未命中桶");
    }

    /// **M7 变异测试**：跨轮混合上报时，命中率必须按「逐轮累加」算。
    ///
    /// 构造与 loop 一致的输入：一个回合里 1 轮报 `(1000, hit=900)`、另 1 轮完全不报。
    /// loop 的 `total_usage` 会把两轮的 input 都加进 `input_tokens`（2000），
    /// 而不报那轮的 `hit` 缺席、不参与相加 —— **两者不对称**。
    ///
    /// 因此若从聚合后的 `usage` 派生（`input − hit` = 2000 − 900 = 1100），
    /// 命中率会跌到 45%；正确值（只看有读数的轮）是 90%。
    ///
    /// 变异：把 `record_result` 改成 `record_with_cache(&result.usage, None)`
    /// （即退回从聚合值派生）⇒ 本用例从 90% 变 45% ⇒ 变红。
    #[test]
    fn cache_hit_pct_accumulates_per_round_not_per_turn() {
        let mut stats = TurnStats::default();
        stats.record_result(&RunResult {
            stop_reason: ys_core::StopReason::Completed,
            // loop 的回合累计：两轮 input 相加，而 hit 只算有读数的那轮。
            usage: Usage {
                input_tokens: 2000,
                prompt_cache_hit_tokens: Some(900),
                ..Default::default()
            },
            // loop 逐轮累加的结果：只有第 1 轮贡献 (900, 100)。
            cache_reading: Some((900, 100)),
            compaction_usage: Usage::default(),
            rounds: 2,
            final_message: None,
        });
        assert_eq!(
            stats.cache_hit_pct(),
            Some(90),
            "必须按逐轮累加算：900/(900+100)=90%，不是从聚合值派生的 900/2000=45%"
        );
    }

    /// 只有 `hit == hit + miss` 才给 100，`99.6%` 必须显示 99。
    ///
    /// 变异：改用四舍五入（`(hit * 100 + total / 2) / total`）⇒ 本用例变红。
    #[test]
    fn cache_hit_pct_exact_only_at_full() {
        let mut stats = TurnStats::default();
        stats.record(&Usage {
            input_tokens: 1000,
            prompt_cache_hit_tokens: Some(996),
            ..Default::default()
        });
        assert_eq!(
            stats.cache_hit_pct(),
            Some(99),
            "99.6% 不得四舍五入成满命中"
        );

        let mut full = TurnStats::default();
        full.record(&Usage {
            input_tokens: 1000,
            prompt_cache_hit_tokens: Some(1000),
            ..Default::default()
        });
        assert_eq!(full.cache_hit_pct(), Some(100), "全命中才给 100");
    }
}
