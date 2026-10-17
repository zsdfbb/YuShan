//! **逐轮读数**：把事件流按「首 token 延迟 / 生成 / 工具」三段计时，落进 `yushan.log`。
//!
//! 存在的理由：一次运行的墙钟 ≈ 轮数 × 每轮模型耗时，而工具执行是毫秒项
//! （见 `docs/arch/explore-speed/cost-model.md`）。在此之前这两个数都不可见 ——
//! 唯一的回合级读数只有 `turn=` 行里的整数秒，工具那半完全没有。
//!
//! # 为什么在消费者侧、而不是在循环里
//!
//! 段的边界全部取自**既有事件**，故不需要在 `ys-loop` 里插桩、也不新增事件变体
//! （也不改 `--json` 的对外契约）：
//!
//! ```text
//! RoundStarted ──ttft──▶ 首个模型增量 ──gen──▶ 末个模型增量
//!              └────────────tools────────────▶ 首/末个 ToolResult
//! ```
//!
//! 代价是时间戳取**消费者收到事件的时刻**，含信道传输 —— 信道是同进程内的有界
//! 队列，相对秒级的模型往返可忽略。
//!
//! # 两条入口
//!
//! TUI 走 `app_loop` 的事件泵，`-p` / `--json` 走 `main::consume_events`；两者都在
//! 本模块的同一套口径下落行，故跨入口可比。
//!
//! # 口径
//!
//! - `ttft` = 收到 `RoundStarted` → 收到**首个可观测输出**（模型增量**或**首个 `ToolCall`，
//!   取更早者）。正常情况下是「排队 + prefill」；若该轮**完全不流式**（只给工具调用），
//!   它还含上生成时间 —— 见 `take_line` 的注释，这是为了让那类轮**不被记成 0**。
//! - `gen` = 首个可观测输出 → 生成终点（末个增量与首个 `ToolCall` 取更晚者）。
//! - `tools` = 首末 `ToolResult` 之间。无工具调用的轮记 0。
//! - 三者之外的余额（组装请求、落盘、事件转发）不计入任何一段 ⇒ **三段之和 ≤ total**。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ys_core::ToolCallId;
use ys_event::AgentEvent;
use ys_loop::{LoopError, RunResult};
use ys_protocol::Envelope;

/// 两个时刻之间的毫秒数；任一缺失记 0（该段没有对应事件）。
fn span_ms(from: Option<Instant>, to: Option<Instant>) -> u128 {
    match (from, to) {
        (Some(a), Some(b)) => b.saturating_duration_since(a).as_millis(),
        _ => 0,
    }
}

/// 逐轮计时状态机：吃事件、吐可观测行。
#[derive(Default)]
pub(crate) struct RoundClock {
    round: Option<u32>,
    started: Option<Instant>,
    first_token: Option<Instant>,
    last_model: Option<Instant>,
    first_tool: Option<Instant>,
    last_tool: Option<Instant>,
    /// 本轮**在飞**的调用：`id → (名字, 起点)`。
    ///
    /// 存在的理由：轮级的 `tools_ms` 是**首末之差**，它答不了"这一轮的 74 秒花在哪一个
    /// 调用上" —— 实测出现过一轮里单个 `bash` 吃掉 73.7 秒，而命令本身平平无奇。
    in_flight: HashMap<ToolCallId, (String, Instant)>,
    /// 本轮**已完成**调用的耗时，按完成顺序（与 `in_flight` 一起在轮末清空）。
    calls: Vec<(String, u128)>,
}

impl RoundClock {
    /// 观察一条事件，返回**刚被它关闭的那一轮**的可观测行（没有则空）。
    ///
    /// 至多两条：轮摘要行；若该轮有工具调用，再跟一条**逐调用**耗时行
    /// （`tools round=N name:ms name:ms …`）。
    pub(crate) fn observe(&mut self, env: &Envelope, now: Instant) -> Vec<String> {
        match &env.event {
            AgentEvent::RoundStarted { round } => {
                let closed = self.take_lines();
                self.round = Some(*round);
                self.started = Some(now);
                closed
            }
            AgentEvent::ModelTextDelta { .. } | AgentEvent::ModelThinkingDelta { .. } => {
                self.first_token.get_or_insert(now);
                self.last_model = Some(now);
                Vec::new()
            }
            AgentEvent::ToolCall { call } => {
                self.first_tool.get_or_insert(now);
                self.in_flight
                    .insert(call.id.clone(), (call.name.clone(), now));
                Vec::new()
            }
            AgentEvent::ToolResult { id, .. } => {
                self.last_tool = Some(now);
                if let Some((name, start)) = self.in_flight.remove(id) {
                    self.calls
                        .push((name, now.saturating_duration_since(start).as_millis()));
                }
                Vec::new()
            }
            // 终局事件之后不会再有本轮的增量/工具事件 ⇒ 就地收尾这一轮。
            AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. } => self.take_lines(),
            _ => Vec::new(),
        }
    }

    /// 关闭当前轮并返回它的可观测行；无进行中的轮时为空。
    pub(crate) fn take_lines(&mut self) -> Vec<String> {
        let Some(round) = self.round.take() else {
            return Vec::new();
        };
        // **首个可观测输出**：优先取模型增量，没有才取首个 `ToolCall`。
        //
        // 为什么用 `or` 而不是取更早者：**增量必然先于 `ToolCall` 到达** —— 增量在流式
        // 期间发，`ToolCall` 在响应收完之后才发。所以两者都在时增量一定更早，取更早者
        // 是个永远走不到的分支。
        //
        // 而这个 `or` 是承重的：模型可能**一个字符都不流式**、直接给工具调用（实测出现过），
        // 只看增量会把整轮的等待记成 `ttft=0 gen=0`（时间只在 `total` 里露一下），
        // 平均下来系统性拉低 `ttft` —— 并据此得出「缓存生效」这类错误结论。
        let first_out = self.first_token.or(self.first_tool);
        // 生成终点：末个模型增量与首个 `ToolCall` 取较晚者（正常是后者更晚）。
        let model_end = match (self.last_model, self.first_tool) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let mut out = vec![format!(
            "round={round} ttft_ms={} gen_ms={} tools_ms={} total_ms={}",
            span_ms(self.started, first_out),
            span_ms(first_out, model_end),
            span_ms(self.first_tool, self.last_tool),
            span_ms(self.started, self.last_tool.or(model_end)),
        )];
        if !self.calls.is_empty() {
            let parts: Vec<String> = self
                .calls
                .iter()
                .map(|(name, ms)| format!("{name}:{ms}"))
                .collect();
            out.push(format!("tools round={round} {}", parts.join(" ")));
        }
        self.started = None;
        self.first_token = None;
        self.last_model = None;
        self.first_tool = None;
        self.last_tool = None;
        self.in_flight.clear();
        self.calls.clear();
        out
    }
}

/// 本回合的 prompt 缓存读数 `(命中, 未命中)`；**读不到**时为 `None`。
///
/// 两种「没有」都要归为 `None`，否则会把排查引到错的方向：
/// - **失败回合**压根拿不到 `RunResult`。
/// - **provider 不上报**：用量存在（`input_tokens > 0`）但缓存两个字段时间为 0。
///   这时「0 命中」与「没上报」在数值上完全一样 —— 一律记「没有读数」，
///   而不是伪造成「缓存 0% 命中」。
pub(crate) fn turn_cache(outcome: &Result<RunResult, LoopError>) -> Option<(u32, u32)> {
    let usage = &outcome.as_ref().ok()?.usage;
    if usage.prompt_cache_hit_tokens == 0
        && usage.prompt_cache_miss_tokens == 0
        && usage.input_tokens > 0
    {
        return None;
    }
    Some((
        usage.prompt_cache_hit_tokens,
        usage.prompt_cache_miss_tokens,
    ))
}

/// 回合边界日志行 —— 两条入口**同格式**，便于跨入口 grep 比对。
///
/// 时间一律取**整数秒**（便于 grep）；`budget_remaining` 由调用方用
/// `saturating_duration_since` 算好，绝不出现负数。
pub(crate) fn turn_line(
    turn: u32,
    rounds: u32,
    elapsed: Duration,
    cumulative_rounds: u32,
    budget_remaining: Duration,
    cache: Option<(u32, u32)>,
) -> String {
    // 失败回合没有用量来源 ⇒ 用 `-` 与「服务端没报」的 0 区分开。
    let (hit, miss) = match cache {
        Some((hit, miss)) => (hit.to_string(), miss.to_string()),
        None => ("-".to_string(), "-".to_string()),
    };
    format!(
        "turn={turn} rounds={rounds} elapsed={}s cumulative_rounds={cumulative_rounds} \
         budget_remaining={}s cache_hit={hit} cache_miss={miss}",
        elapsed.as_secs(),
        budget_remaining.as_secs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use ys_core::{StopReason, ToolCall, ToolCallId, ToolResult, Usage};
    use ys_protocol::Source;

    const MS: Duration = Duration::from_millis(1);

    /// 造一个信封 —— [`RoundClock`] 只看事件**变体**，故载荷取最小可用值。
    fn env_of(event: AgentEvent) -> Envelope {
        Envelope::new(Source::default(), 1, event)
    }

    fn text_delta() -> AgentEvent {
        AgentEvent::ModelTextDelta { text: "x".into() }
    }

    fn tool_call() -> AgentEvent {
        tool_call_id("c1")
    }

    fn tool_call_id(id: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId(id.into()),
                name: "read".into(),
                arguments: serde_json::Value::Null,
            },
        }
    }

    fn tool_result() -> AgentEvent {
        tool_result_id("c1")
    }

    fn tool_result_id(id: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            id: ToolCallId(id.into()),
            result: ToolResult {
                content: "ok".into(),
                is_error: false,
            },
        }
    }

    fn usage(hit: u32, miss: u32) -> Usage {
        Usage {
            input_tokens: 0,
            output_tokens: 0,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
        }
    }

    fn result_with(usage: Usage) -> Result<RunResult, LoopError> {
        Ok(RunResult {
            stop_reason: StopReason::Completed,
            usage,
            rounds: 1,
            final_message: None,
        })
    }

    /// 三段口径：`ttft`（排队 + prefill）/ `gen`（解码）/ `tools`。
    ///
    /// 变异：① `ttft` 的起点从 `started` 改成 `first_token` ⇒ 恒为 0；
    /// ② `model_end` 的 `a.max(b)` 退化成 `a` ⇒ 有工具调用的轮 `gen` 短一截；
    /// ③ `tools` 的起点从 `first_tool` 改成 `last_tool` ⇒ 恒为 0。
    #[test]
    fn round_clock_splits_ttft_gen_and_tools() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        assert!(
            clock
                .observe(&env_of(AgentEvent::RoundStarted { round: 1 }), t0)
                .is_empty(),
            "开轮本身不落行"
        );
        clock.observe(&env_of(text_delta()), t0 + 100 * MS);
        clock.observe(&env_of(text_delta()), t0 + 300 * MS);
        // `ToolCall` **刻意晚于**末个增量 —— 两者取较晚者才是「解码结束」，
        // 同刻会让 `max` 的退化不可观测。
        clock.observe(&env_of(tool_call()), t0 + 320 * MS);
        clock.observe(&env_of(tool_result()), t0 + 370 * MS);

        let mut lines = clock
            .observe(
                &env_of(AgentEvent::RoundStarted { round: 2 }),
                t0 + 400 * MS,
            );

        assert_eq!(
            lines.remove(0), "round=1 ttft_ms=100 gen_ms=220 tools_ms=50 total_ms=370",
            "三段口径：排队+prefill / 解码 / 工具"
        );
    }

    /// 无工具调用的轮：`tools_ms` 记 0，`total` 以模型结束为准。
    ///
    /// 变异：把 `total` 的终点从 `last_tool.or(model_end)` 改成 `last_tool` ⇒ 本例变 0。
    #[test]
    fn round_clock_records_zero_tools_when_no_tool_calls() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        clock.observe(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        clock.observe(&env_of(text_delta()), t0 + 100 * MS);
        clock.observe(&env_of(text_delta()), t0 + 300 * MS);

        let mut lines = clock
            .observe(
                &env_of(AgentEvent::RunFinished {
                    stop_reason: StopReason::Completed,
                    usage: Usage::default(),
                    rounds: 1,
                }),
                t0 + 320 * MS,
            );

        assert_eq!(lines.remove(0), "round=1 ttft_ms=100 gen_ms=200 tools_ms=0 total_ms=300");
    }

    /// **完全不流式的轮**（模型一个字符都不吐、直接给工具调用）必须仍被计时。
    ///
    /// 实测出现过这种轮（`daily/1` 的 round=3）：若只看增量，它会记成 `ttft=0 gen=0`，
    /// 整轮 1.8 秒只在 `total` 里露一下 —— 平均下来会**系统性拉低 ttft**，并据此得出
    /// 「缓存生效」这类错误结论。
    ///
    /// 变异：把 `ttft` 的终点从 `first_out` 改回只用 `first_token` ⇒ 本条 `ttft_ms` 变 0
    /// （已实际注入验证）。
    #[test]
    fn round_clock_times_a_round_with_no_streamed_deltas() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        clock.observe(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        // 没有任何 `Model*Delta`，直接就是工具调用。
        clock.observe(&env_of(tool_call()), t0 + 1780 * MS);

        let mut lines = clock
            .observe(
                &env_of(AgentEvent::RunFinished {
                    stop_reason: StopReason::Completed,
                    usage: Usage::default(),
                    rounds: 1,
                }),
                t0 + 1790 * MS,
            );

        assert_eq!(lines.remove(0), "round=1 ttft_ms=1780 gen_ms=0 tools_ms=0 total_ms=1780");
    }

    /// **逐调用耗时**：轮级 `tools_ms` 是首末之差，答不了「这轮的 N 秒花在哪一个调用上」。
    ///
    /// 实测出现过一轮里**单个 `bash` 吃掉 73.7 秒**（而命令本身平平无奇），
    /// 只凭轮级数字无法定位 ⇒ 需要每个调用自己的耗时。
    ///
    /// 变异：把 `ToolResult` 分支里的 `self.calls.push(...)` 删掉 ⇒ 只落一条行，第一条断言变红
    /// （已实际注入验证）。
    #[test]
    fn round_clock_reports_per_call_durations() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        clock.observe(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        clock.observe(&env_of(tool_call_id("c1")), t0 + 10 * MS);
        clock.observe(&env_of(tool_result_id("c1")), t0 + 110 * MS); // 100 ms
        clock.observe(&env_of(tool_call_id("c2")), t0 + 120 * MS);
        clock.observe(&env_of(tool_result_id("c2")), t0 + 1320 * MS); // 1200 ms

        let mut lines = clock.observe(
            &env_of(AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            }),
            t0 + 1400 * MS,
        );
        assert_eq!(
            lines.len(),
            2,
            "有工具调用的轮应落两条：轮摘要 + 逐调用；{lines:?}"
        );
        assert_eq!(
            lines.remove(1),
            "tools round=1 read:100 read:1200",
            "按完成顺序列每个调用的耗时"
        );
    }

    /// 没有开轮时落不出行 —— 终局事件的 `take_line` 必须是 `None` 而不是空串。
    ///
    /// 变异：把 `take_line` 的 `self.round.take()?` 改成 `unwrap_or(0)` ⇒ 本例变红
    /// （会凭空写出 `round=0` 的一行）。
    #[test]
    fn round_clock_silent_without_an_open_round() {
        let mut clock = RoundClock::default();
        assert!(
            clock
                .observe(
                    &env_of(AgentEvent::RunFailed {
                        error: "boom".into()
                    }),
                    Instant::now()
                )
                .is_empty()
        );
    }

    /// 缓存读数：**有 input 却缓存两栏全 0** ⇒ `None`（provider 没上报），
    /// 不能被当成「缓存 0% 命中」—— 那会把排查引到错的方向。
    ///
    /// 变异：① 把 `input_tokens > 0` 那个条件去掉 ⇒ 第二条断言变红（真读数被伪装成 0% 命中）；
    /// ② 把 `as_ref().ok()?` 改成 `Some(...)` 兜底 ⇒ 第三条断言变红。
    #[test]
    fn turn_cache_separates_unreported_from_zero_hit() {
        assert_eq!(
            turn_cache(&result_with(usage(900, 100))),
            Some((900, 100)),
            "正常上报"
        );

        let unreported = RunResult {
            stop_reason: StopReason::Completed,
            usage: Usage {
                input_tokens: 29_993,
                output_tokens: 149,
                prompt_cache_hit_tokens: 0,
                prompt_cache_miss_tokens: 0,
            },
            rounds: 2,
            final_message: None,
        };
        assert_eq!(
            turn_cache(&Ok(unreported)),
            None,
            "有 input 却缓存两栏全 0 ⇒ 服务端没上报，不是 0% 命中"
        );

        assert_eq!(
            turn_cache(&Err(LoopError::Tool("boom".into()))),
            None,
            "失败回合没有用量来源"
        );
    }

    /// 回合行恒为定形：五个计数键 + 两个缓存键，缺用量时用 `-` 而不是省略。
    ///
    /// 变异：把 `None` 分支的 `"-"` 改成 `"0"` ⇒ 前两条断言变红，且「没有用量来源」
    /// 会伪装成「缓存全没命中」。
    #[test]
    fn turn_line_keeps_a_fixed_shape() {
        let line = turn_line(
            3,
            7,
            Duration::from_secs(42),
            11,
            Duration::from_secs(99),
            Some((900, 100)),
        );
        assert_eq!(
            line,
            "turn=3 rounds=7 elapsed=42s cumulative_rounds=11 budget_remaining=99s \
             cache_hit=900 cache_miss=100"
        );

        let unknown = turn_line(1, 1, Duration::ZERO, 1, Duration::ZERO, None);
        assert!(
            unknown.ends_with("cache_hit=- cache_miss=-"),
            "缺用量来源时必须是 `-`：{unknown}"
        );
    }
}
