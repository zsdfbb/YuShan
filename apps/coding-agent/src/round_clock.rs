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

use serde_json::{Map, Value, json};
use ys_core::ToolCallId;
use ys_event::AgentEvent;
use ys_loop::{LoopError, RunResult};
use ys_protocol::Envelope;

use crate::debug_log::{ArgsMode, DebugRecord, summarize_tool_args};

/// 两个时刻之间的毫秒数；任一缺失记 0（该段没有对应事件）。
fn span_ms(from: Option<Instant>, to: Option<Instant>) -> u128 {
    match (from, to) {
        (Some(a), Some(b)) => b.saturating_duration_since(a).as_millis(),
        _ => 0,
    }
}

/// 一轮关闭时的产物：文本行（两条入口共用）与 debug 记录（仅 debug 开着时）。
#[derive(Default)]
pub(crate) struct RoundObservation {
    pub lines: Vec<String>,
    pub records: Vec<DebugRecord>,
}

/// 逐轮计时状态机：吃事件、吐可观测行。
pub(crate) struct RoundClock {
    round: Option<u32>,
    started: Option<Instant>,
    first_token: Option<Instant>,
    last_model: Option<Instant>,
    first_tool: Option<Instant>,
    last_tool: Option<Instant>,
    /// 本轮首个 / 末个思考增量（`ModelThinkingDelta`）。
    first_thinking: Option<Instant>,
    last_thinking: Option<Instant>,
    /// 本轮首个 / 末个正文增量（`ModelTextDelta`）。
    first_text: Option<Instant>,
    last_text: Option<Instant>,
    /// 逐增量**只计数**，不落记录：增量占事件量的 95% 以上。
    thinking_deltas: u32,
    text_deltas: u32,
    /// 本轮 `ToolCall` 事件数（不是 `ToolResult` 数：调用可能失败而不回结果）。
    tool_calls: u32,
    /// 工具参数摘要档位；仅 `observe_debug` 读取。
    args_mode: ArgsMode,
    /// 本轮**在飞**的调用：`id → (名字, 起点)`。
    ///
    /// 存在的理由：轮级的 `tools_ms` 是**首末之差**，它答不了"这一轮的 74 秒花在哪一个
    /// 调用上" —— 实测出现过一轮里单个 `bash` 吃掉 73.7 秒，而命令本身平平无奇。
    in_flight: HashMap<ToolCallId, (String, Instant)>,
    /// 本轮**已完成**调用的耗时，按完成顺序（与 `in_flight` 一起在轮末清空）。
    calls: Vec<(String, u128)>,
}

impl Default for RoundClock {
    /// 默认按 `head` 档摘要工具参数（关闭 debug 时不会走到摘要）。
    fn default() -> Self {
        Self::new(ArgsMode::Head)
    }
}

impl RoundClock {
    pub(crate) fn new(args_mode: ArgsMode) -> Self {
        Self {
            round: None,
            started: None,
            first_token: None,
            last_model: None,
            first_tool: None,
            last_tool: None,
            first_thinking: None,
            last_thinking: None,
            first_text: None,
            last_text: None,
            thinking_deltas: 0,
            text_deltas: 0,
            tool_calls: 0,
            args_mode,
            in_flight: HashMap::new(),
            calls: Vec::new(),
        }
    }

    /// 观察一条事件，返回**刚被它关闭的那一轮**的可观测行（没有则空）。
    ///
    /// 至多两条：轮摘要行；若该轮有工具调用，再跟一条**逐调用**耗时行
    /// （`tools round=N name:ms name:ms …`）。
    ///
    /// **签名与输出是既有契约**：文本行逐字节不变，也不产生任何 debug 记录。
    pub(crate) fn observe(&mut self, env: &Envelope, now: Instant) -> Vec<String> {
        self.observe_inner(env, now, false).lines
    }

    /// [`RoundClock::observe`] 的 debug 版本：文本行完全相同，另返回结构化记录。
    ///
    /// 每个事件只能走其中一条路径（共享同一套状态机）；关轮时文本行与
    /// `round_end` 记录在同一次状态迁移里产出，二者因此不可能漂移。
    ///
    /// 关闭 debug 的生产路径走 [`RoundClock::observe`]。
    pub(crate) fn observe_debug(&mut self, env: &Envelope, now: Instant) -> RoundObservation {
        self.observe_inner(env, now, true)
    }

    fn observe_inner(&mut self, env: &Envelope, now: Instant, debug: bool) -> RoundObservation {
        match &env.event {
            AgentEvent::RoundStarted { round } => {
                let mut observation = self.close_round(debug);
                self.round = Some(*round);
                self.started = Some(now);
                if debug {
                    observation.records.push(DebugRecord {
                        ev: "round_start",
                        round: Some(*round),
                        tool_call_id: None,
                        fields: Map::new(),
                    });
                }
                observation
            }
            AgentEvent::ModelThinkingDelta { .. } => {
                self.first_token.get_or_insert(now);
                self.last_model = Some(now);
                self.first_thinking.get_or_insert(now);
                self.last_thinking = Some(now);
                self.thinking_deltas += 1;
                RoundObservation::default()
            }
            AgentEvent::ModelTextDelta { .. } => {
                self.first_token.get_or_insert(now);
                self.last_model = Some(now);
                self.first_text.get_or_insert(now);
                self.last_text = Some(now);
                self.text_deltas += 1;
                RoundObservation::default()
            }
            AgentEvent::ToolCall { call } => {
                self.first_tool.get_or_insert(now);
                self.tool_calls += 1;
                self.in_flight
                    .insert(call.id.clone(), (call.name.clone(), now));
                let mut observation = RoundObservation::default();
                if debug {
                    let summary = summarize_tool_args(&call.name, &call.arguments, self.args_mode);
                    let mut fields = Map::new();
                    fields.insert("gen_ai.tool.name".to_string(), json!(call.name));
                    fields.insert("gen_ai.tool.call.id".to_string(), json!(call.id.0));
                    fields.insert("gen_ai.tool.call.arguments".to_string(), summary.arguments);
                    // 命令串指纹（非命令类工具为 `null`）：「重复命令数」的唯一落点；
                    // 只落指纹、不落正文。
                    fields.insert("command_fp".to_string(), json!(summary.command_fp));
                    fields.insert(
                        "args_original_bytes".to_string(),
                        json!(summary.original_bytes),
                    );
                    fields.insert("args_truncated".to_string(), json!(summary.truncated));
                    fields.insert("args_keys".to_string(), json!(summary.keys));
                    fields.insert(
                        "args_redacted_keys".to_string(),
                        json!(summary.redacted_keys),
                    );
                    observation.records.push(DebugRecord {
                        ev: "tool_call",
                        round: self.round,
                        tool_call_id: Some(call.id.0.clone()),
                        fields,
                    });
                }
                observation
            }
            AgentEvent::ToolResult { id, result } => {
                self.last_tool = Some(now);
                let duration_ms = self.in_flight.remove(id).map(|(name, start)| {
                    let ms = now.saturating_duration_since(start).as_millis();
                    self.calls.push((name, ms));
                    ms
                });
                let mut observation = RoundObservation::default();
                if debug {
                    // **不记结果内容**：debug 是索引，内容以 session 为准。
                    let mut fields = Map::new();
                    fields.insert(
                        "duration_ms".to_string(),
                        json!(duration_ms.map(|ms| ms as u64)),
                    );
                    fields.insert("is_error".to_string(), json!(result.is_error));
                    fields.insert("result_bytes".to_string(), json!(result.content.len()));
                    fields.insert(
                        "result_lines".to_string(),
                        json!(result.content.lines().count()),
                    );
                    fields.insert("result_lines_exact".to_string(), json!(true));
                    observation.records.push(DebugRecord {
                        ev: "tool_result",
                        round: self.round,
                        tool_call_id: Some(id.0.clone()),
                        fields,
                    });
                }
                observation
            }
            AgentEvent::RoundUsage {
                purpose,
                round,
                usage,
            } => {
                let mut observation = RoundObservation::default();
                if debug {
                    let mut fields = Map::new();
                    fields.insert(
                        "purpose".to_string(),
                        serde_json::to_value(purpose).unwrap_or(Value::Null),
                    );
                    fields.insert(
                        "gen_ai.usage.input_tokens".to_string(),
                        json!(usage.input_tokens),
                    );
                    fields.insert(
                        "gen_ai.usage.output_tokens".to_string(),
                        json!(usage.output_tokens),
                    );
                    fields.insert(
                        "gen_ai.usage.total_tokens".to_string(),
                        json!(usage.input_tokens.saturating_add(usage.output_tokens)),
                    );
                    fields.insert(
                        "gen_ai.usage.cache_read.input_tokens".to_string(),
                        json!(usage.prompt_cache_hit_tokens),
                    );
                    // 「没上报」写 `null`，绝不伪装成 0：真 0 与缺失语义不同。
                    fields.insert(
                        "gen_ai.usage.reasoning.output_tokens".to_string(),
                        match usage.reasoning_tokens {
                            Some(tokens) => json!(tokens),
                            None => Value::Null,
                        },
                    );
                    // **代理判据**：非 Option 的计数栏没有独立的「在不在」信号，只能按
                    // 「input/output 是否有读数」推断是否上报，故 `false` **不能**被读作
                    // 「确定没上报」。以下情况都会误判成 `false`：
                    // - 真实上报 `input = 0 && output = 0`（如空响应）；
                    // - provider 只填 `total_tokens`，或只填缓存计数
                    //   （`prompt_cache_hit/miss > 0` 而 input/output 为 0）。
                    // 要判定「确实没上报」，需与 provider 的原始响应对照。
                    fields.insert(
                        "usage_reported".to_string(),
                        json!(usage.input_tokens > 0 || usage.output_tokens > 0),
                    );
                    fields.insert(
                        "reasoning_tokens_reported".to_string(),
                        json!(usage.reasoning_tokens.is_some()),
                    );
                    observation.records.push(DebugRecord {
                        ev: "round_usage",
                        round: Some(*round),
                        tool_call_id: None,
                        fields,
                    });
                }
                observation
            }
            // 终局事件之后不会再有本轮的增量/工具事件 ⇒ 就地收尾这一轮。
            AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. } => {
                self.close_round(debug)
            }
            _ => RoundObservation::default(),
        }
    }

    /// 关闭当前轮并返回它的可观测行；无进行中的轮时为空（也不产记录）。
    pub(crate) fn take_lines(&mut self) -> Vec<String> {
        self.close_round(false).lines
    }

    /// 关闭当前轮：文本行与 `round_end` 记录来自同一次状态迁移。
    ///
    /// `with_records = false` 时跳过摘要与记录构造（关闭 debug 的路径）。
    fn close_round(&mut self, with_records: bool) -> RoundObservation {
        let Some(round) = self.round.take() else {
            return RoundObservation::default();
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
        let mut lines = vec![format!(
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
            lines.push(format!("tools round={round} {}", parts.join(" ")));
        }

        let mut records = Vec::new();
        if with_records {
            // 首增量相对轮起点；该轮没有对应增量 ⇒ `null`（不是 0 —— 0 会伪装成
            // 「第 0 毫秒就开始了」）。段长则是真实的观测值，无增量时确为 0。
            let first_ms = |at: Option<Instant>| {
                self.started.and_then(|started| {
                    at.map(|instant| instant.saturating_duration_since(started).as_millis() as u64)
                })
            };
            let mut fields = Map::new();
            fields.insert(
                "ttft_ms".to_string(),
                json!(span_ms(self.started, first_out) as u64),
            );
            fields.insert(
                "gen_ms".to_string(),
                json!(span_ms(first_out, model_end) as u64),
            );
            fields.insert(
                "tools_ms".to_string(),
                json!(span_ms(self.first_tool, self.last_tool) as u64),
            );
            fields.insert(
                "total_ms".to_string(),
                json!(span_ms(self.started, self.last_tool.or(model_end)) as u64),
            );
            fields.insert(
                "first_thinking_ms".to_string(),
                json!(first_ms(self.first_thinking)),
            );
            fields.insert(
                "first_text_ms".to_string(),
                json!(first_ms(self.first_text)),
            );
            fields.insert(
                "thinking_span_ms".to_string(),
                json!(span_ms(self.first_thinking, self.last_thinking) as u64),
            );
            fields.insert(
                "text_span_ms".to_string(),
                json!(span_ms(self.first_text, self.last_text) as u64),
            );
            fields.insert("thinking_deltas".to_string(), json!(self.thinking_deltas));
            fields.insert("text_deltas".to_string(), json!(self.text_deltas));
            fields.insert("tool_calls".to_string(), json!(self.tool_calls));
            records.push(DebugRecord {
                ev: "round_end",
                round: Some(round),
                tool_call_id: None,
                fields,
            });
        }

        self.reset_round();
        RoundObservation { lines, records }
    }

    fn reset_round(&mut self) {
        self.started = None;
        self.first_token = None;
        self.last_model = None;
        self.first_tool = None;
        self.last_tool = None;
        self.first_thinking = None;
        self.last_thinking = None;
        self.first_text = None;
        self.last_text = None;
        self.thinking_deltas = 0;
        self.text_deltas = 0;
        self.tool_calls = 0;
        self.in_flight.clear();
        self.calls.clear();
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
    use std::collections::HashSet;
    use std::time::Duration;

    use serde_json::json;
    use ys_core::{StopReason, ToolCall, ToolCallId, ToolResult, Usage};
    use ys_event::RoundPurpose;
    use ys_protocol::Source;

    const MS: Duration = Duration::from_millis(1);

    /// 造一个信封 —— [`RoundClock`] 只看事件**变体**，故载荷取最小可用值。
    fn env_of(event: AgentEvent) -> Envelope {
        Envelope::new(Source::default(), 1, event)
    }

    fn text_delta() -> AgentEvent {
        AgentEvent::ModelTextDelta { text: "x".into() }
    }

    fn thinking_delta() -> AgentEvent {
        AgentEvent::ModelThinkingDelta { text: "t".into() }
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

    /// 造一条**命令类**工具调用（`bash`），参数带 `command`。
    fn bash_call(id: &str, command: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId(id.into()),
                name: "bash".into(),
                arguments: json!({ "command": command }),
            },
        }
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
            reasoning_tokens: None,
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

        let mut lines = clock.observe(
            &env_of(AgentEvent::RoundStarted { round: 2 }),
            t0 + 400 * MS,
        );

        assert_eq!(
            lines.remove(0),
            "round=1 ttft_ms=100 gen_ms=220 tools_ms=50 total_ms=370",
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

        let mut lines = clock.observe(
            &env_of(AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            }),
            t0 + 320 * MS,
        );

        assert_eq!(
            lines.remove(0),
            "round=1 ttft_ms=100 gen_ms=200 tools_ms=0 total_ms=300"
        );
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

        let mut lines = clock.observe(
            &env_of(AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            }),
            t0 + 1790 * MS,
        );

        assert_eq!(
            lines.remove(0),
            "round=1 ttft_ms=1780 gen_ms=0 tools_ms=0 total_ms=1780"
        );
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
                reasoning_tokens: None,
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

    // ---- debug 富化：文本行契约不变 + 段/工具/usage 记录 ----

    /// `observe` 与 `observe_debug` 必须共享同一套状态机：同一事件序列下
    /// `lines` **逐字节一致**（既有 `yushan.log` 格式不变）。
    ///
    /// 变异：在任一路径多算/漏算一个时刻（例如只在 debug 分支里更新
    /// `first_text`）⇒ 末行断言变红。
    #[test]
    fn observe_and_observe_debug_emit_identical_lines() {
        let t0 = Instant::now();
        let events = vec![
            (AgentEvent::RoundStarted { round: 1 }, 0),
            (thinking_delta(), 100),
            (text_delta(), 150),
            (tool_call(), 220),
            (tool_result(), 280),
            (
                AgentEvent::RoundUsage {
                    purpose: RoundPurpose::Turn,
                    round: 1,
                    usage: usage(0, 0),
                },
                290,
            ),
            (AgentEvent::RoundStarted { round: 2 }, 400),
            (text_delta(), 450),
            (
                AgentEvent::RunFinished {
                    stop_reason: StopReason::Completed,
                    usage: Usage::default(),
                    rounds: 2,
                },
                500,
            ),
        ];

        let mut plain = RoundClock::default();
        let mut rich = RoundClock::default();
        let mut plain_lines = Vec::new();
        let mut rich_lines = Vec::new();
        for (event, offset) in events {
            let at = t0 + offset * MS;
            plain_lines.extend(plain.observe(&env_of(event.clone()), at));
            rich_lines.extend(rich.observe_debug(&env_of(event), at).lines);
        }
        plain_lines.extend(plain.take_lines());
        rich_lines.extend(rich.take_lines());

        assert_eq!(plain_lines, rich_lines, "两条路径的文本行必须逐字节一致");
        assert_eq!(
            plain_lines[0],
            "round=1 ttft_ms=100 gen_ms=120 tools_ms=60 total_ms=280"
        );
    }

    /// thinking / text 分段：首时刻相对轮起点，段长 = 同类末增量 − 首增量；
    /// 计数只出现在 `round_end`，**没有任何逐增量记录**。
    ///
    /// 变异：① 交换 `first_thinking` / `first_text` 的更新 ⇒ 首时刻断言变红；
    /// ② 把 `thinking_span_ms` 改成 `last_thinking - started` ⇒ 段长断言变红。
    #[test]
    fn round_end_splits_thinking_and_text_segments() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        clock.observe_debug(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        clock.observe_debug(&env_of(thinking_delta()), t0 + 100 * MS);
        clock.observe_debug(&env_of(text_delta()), t0 + 150 * MS);
        clock.observe_debug(&env_of(thinking_delta()), t0 + 200 * MS);
        clock.observe_debug(&env_of(text_delta()), t0 + 400 * MS);
        clock.observe_debug(&env_of(tool_call()), t0 + 420 * MS);
        clock.observe_debug(&env_of(tool_result()), t0 + 470 * MS);

        let observation = clock.observe_debug(
            &env_of(AgentEvent::RoundStarted { round: 2 }),
            t0 + 500 * MS,
        );
        let evs: Vec<&str> = observation.records.iter().map(|r| r.ev).collect();
        assert_eq!(evs, vec!["round_end", "round_start"], "{evs:?}");

        let end = &observation.records[0];
        assert_eq!(end.round, Some(1));
        let fields = &end.fields;
        assert_eq!(fields["ttft_ms"], json!(100), "首个可观测输出是思考增量");
        assert_eq!(fields["gen_ms"], json!(320), "首输出 → 末增量/首工具较晚者");
        assert_eq!(fields["tools_ms"], json!(50));
        assert_eq!(fields["total_ms"], json!(470));
        assert_eq!(fields["first_thinking_ms"], json!(100));
        assert_eq!(fields["first_text_ms"], json!(150));
        assert_eq!(fields["thinking_span_ms"], json!(100));
        assert_eq!(fields["text_span_ms"], json!(250));
        assert_eq!(fields["thinking_deltas"], json!(2));
        assert_eq!(fields["text_deltas"], json!(2));
        assert_eq!(fields["tool_calls"], json!(1));
    }

    /// 零增量记录：喂 10,000 条思考/正文增量，`records` 里只允许出现段记录，
    /// **不得有任何以 `thinking` / `text` / `delta` 命名的逐增量记录**。
    ///
    /// 变异：给 `ModelTextDelta` 分支加一条记录 ⇒ 本用例的 `evs` 断言与
    /// 逐事件 `records.is_empty()` 断言同时变红。
    #[test]
    fn model_deltas_only_count_and_produce_no_records() {
        const PAIRS: u32 = 5_000;
        let t0 = Instant::now();
        let mut clock = RoundClock::default();
        clock.observe_debug(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);

        for index in 0..PAIRS {
            let at = t0 + (index * 2) * MS;
            let thinking = clock.observe_debug(&env_of(thinking_delta()), at);
            assert!(thinking.lines.is_empty() && thinking.records.is_empty());
            let text = clock.observe_debug(&env_of(text_delta()), at + MS);
            assert!(text.lines.is_empty() && text.records.is_empty());
        }

        let observation = clock.observe_debug(
            &env_of(AgentEvent::RoundStarted { round: 2 }),
            t0 + PAIRS * 4 * MS,
        );
        let evs: Vec<&str> = observation.records.iter().map(|r| r.ev).collect();
        assert_eq!(evs, vec!["round_end", "round_start"], "{evs:?}");
        assert!(
            !evs.iter()
                .any(|ev| ev.contains("thinking") || ev.contains("text") || ev.contains("delta")),
            "增量不得产出记录：{evs:?}"
        );

        let end = &observation.records[0];
        assert_eq!(end.fields["thinking_deltas"], json!(PAIRS));
        assert_eq!(end.fields["text_deltas"], json!(PAIRS));
    }

    /// `round_usage`：`reasoning_tokens` 的「上报了 0」与「没上报」必须可区分 ——
    /// `Some(7)` ⇒ 值 7 + `reported=true`；`None` ⇒ JSON `null` + `reported=false`。
    ///
    /// 变异：把 `None` 分支写成 `json!(0)` ⇒ 第二组断言变红（0 会伪装成真 0）。
    #[test]
    fn round_usage_distinguishes_reasoning_tokens_none_from_zero() {
        let t0 = Instant::now();
        let mut reported = RoundClock::default();
        let with = reported.observe_debug(
            &env_of(AgentEvent::RoundUsage {
                purpose: RoundPurpose::Turn,
                round: 3,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    prompt_cache_hit_tokens: 80,
                    prompt_cache_miss_tokens: 20,
                    reasoning_tokens: Some(7),
                },
            }),
            t0,
        );
        let record = &with.records[0];
        assert_eq!(record.ev, "round_usage");
        assert_eq!(record.round, Some(3));
        assert_eq!(record.fields["purpose"], json!("turn"));
        assert_eq!(record.fields["gen_ai.usage.input_tokens"], json!(100));
        assert_eq!(record.fields["gen_ai.usage.output_tokens"], json!(20));
        assert_eq!(record.fields["gen_ai.usage.total_tokens"], json!(120));
        assert_eq!(
            record.fields["gen_ai.usage.cache_read.input_tokens"],
            json!(80)
        );
        assert_eq!(
            record.fields["gen_ai.usage.reasoning.output_tokens"],
            json!(7)
        );
        assert_eq!(record.fields["usage_reported"], json!(true));
        assert_eq!(record.fields["reasoning_tokens_reported"], json!(true));

        let mut unreported = RoundClock::default();
        let without = unreported.observe_debug(
            &env_of(AgentEvent::RoundUsage {
                purpose: RoundPurpose::Compaction,
                round: 4,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    reasoning_tokens: None,
                    ..Default::default()
                },
            }),
            t0,
        );
        let record = &without.records[0];
        assert_eq!(record.fields["purpose"], json!("compaction"));
        assert_eq!(
            record.fields["gen_ai.usage.reasoning.output_tokens"],
            Value::Null,
            "未上报必须是 null"
        );
        assert_ne!(
            record.fields["gen_ai.usage.reasoning.output_tokens"],
            json!(0),
            "未上报绝不能写成 0"
        );
        assert_eq!(record.fields["reasoning_tokens_reported"], json!(false));
    }

    /// `tool_call` 走 `summarize_tool_args`（`write` 正文零字节），`tool_result`
    /// 只记尺寸与时长、**不记内容**；两者都带 `round` / `tool_call_id` 外键。
    ///
    /// 变异：① 跳过摘要直接写 `call.arguments` ⇒ `SECRET_BODY` 断言变红；
    /// ② 删掉 `in_flight` 的逐调用耗时 ⇒ `duration_ms` 变 `null`；
    /// ③ 把结果内容写进字段 ⇒ 内容泄漏断言变红。
    #[test]
    fn tool_records_summarize_args_and_only_sizes_for_results() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();
        clock.observe_debug(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);

        let call = AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId("c1".into()),
                name: "write".into(),
                arguments: json!({
                    "path": "src/a.rs",
                    "content": "SECRET_BODY",
                    "api_key": "sk-live",
                }),
            },
        };
        let call_observation = clock.observe_debug(&env_of(call), t0 + 10 * MS);
        let call_record = &call_observation.records[0];
        assert_eq!(call_record.ev, "tool_call");
        assert_eq!(call_record.round, Some(1));
        assert_eq!(call_record.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(call_record.fields["gen_ai.tool.name"], json!("write"));
        assert_eq!(call_record.fields["gen_ai.tool.call.id"], json!("c1"));
        let arguments = &call_record.fields["gen_ai.tool.call.arguments"];
        assert_eq!(arguments["path"], json!("src/a.rs"));
        assert_eq!(arguments["content_bytes"], json!("SECRET_BODY".len()));
        assert_eq!(arguments["content_lines"], json!(1));
        assert_eq!(arguments["api_key"], json!("[REDACTED]"));
        assert!(
            !serde_json::to_string(&call_record.fields)
                .unwrap()
                .contains("SECRET_BODY"),
            "正文一个字节都不得落进记录：{call_record:?}"
        );
        assert_eq!(call_record.fields["args_truncated"], json!(true));
        assert!(
            call_record.fields["args_keys"]
                .as_array()
                .unwrap()
                .contains(&json!("content"))
        );
        assert_eq!(call_record.fields["args_redacted_keys"], json!(["api_key"]));
        assert!(call_record.fields["args_original_bytes"].as_u64().unwrap() > 0);

        let result = AgentEvent::ToolResult {
            id: ToolCallId("c1".into()),
            result: ToolResult {
                content: "line1\nline2\n".into(),
                is_error: true,
            },
        };
        let result_observation = clock.observe_debug(&env_of(result), t0 + 70 * MS);
        let result_record = &result_observation.records[0];
        assert_eq!(result_record.ev, "tool_result");
        assert_eq!(result_record.round, Some(1));
        assert_eq!(result_record.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(result_record.fields["duration_ms"], json!(60));
        assert_eq!(result_record.fields["is_error"], json!(true));
        assert_eq!(result_record.fields["result_bytes"], json!(12));
        assert_eq!(result_record.fields["result_lines"], json!(2));
        assert_eq!(result_record.fields["result_lines_exact"], json!(true));
        assert!(
            !serde_json::to_string(&result_record.fields)
                .unwrap()
                .contains("line1"),
            "结果内容不得落进记录：{result_record:?}"
        );
    }

    /// `usage_reported` 是**代理判据**：input/output 无读数时记 `false`，但这**不等于**
    /// provider 确定没上报 —— 空响应、只填 `total_tokens`、只填缓存计数都会落到这里。
    ///
    /// 变异：把 `usage_reported` 改成恒 `true` ⇒ 本例全 0 与「只有缓存读数」两条断言变红。
    #[test]
    fn round_usage_all_zero_is_marked_unreported_by_proxy() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        let zero = clock.observe_debug(
            &env_of(AgentEvent::RoundUsage {
                purpose: RoundPurpose::Turn,
                round: 5,
                usage: Usage::default(),
            }),
            t0,
        );
        assert_eq!(
            zero.records[0].fields["usage_reported"],
            json!(false),
            "全 0 读数按代理判据记 false"
        );

        // 只填缓存计数、input/output 为 0：同样是误判成 `false` 的已知情形。
        let cache_only = clock.observe_debug(
            &env_of(AgentEvent::RoundUsage {
                purpose: RoundPurpose::Compaction,
                round: 6,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    prompt_cache_hit_tokens: 80,
                    prompt_cache_miss_tokens: 20,
                    reasoning_tokens: None,
                },
            }),
            t0,
        );
        assert_eq!(
            cache_only.records[0].fields["usage_reported"],
            json!(false),
            "代理判据只看 input/output，缓存计数不会被认作上报"
        );
    }

    /// **无增量轮**：`first_thinking_ms` / `first_text_ms` 必须是 `null`，**不是 0**
    /// （0 会伪装成「第 0 毫秒就开始了」）。
    ///
    /// 变异：把 `first_ms` 的缺失分支改成 `unwrap_or(0)` ⇒ 本例 `null` / `!= 0` 断言变红。
    #[test]
    fn round_end_marks_missing_first_deltas_as_null() {
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        clock.observe_debug(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        // 没有任何 `Model*Delta`，直接收尾。
        let observation = clock.observe_debug(
            &env_of(AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            }),
            t0 + 500 * MS,
        );
        let end = &observation.records[0];
        assert_eq!(end.fields["first_thinking_ms"], Value::Null);
        assert_eq!(end.fields["first_text_ms"], Value::Null);
        assert_ne!(end.fields["first_thinking_ms"], json!(0));
        assert_ne!(end.fields["first_text_ms"], json!(0));
    }

    /// `RoundClock::new` 必须把 `args_mode` 透传下去：`None` 档把工具参数投影成空对象，
    /// 与 `Default` 的 `Head` 档（保留 `path` 等）可区分。
    ///
    /// 变异：`new` 忽略入参、恒用 `ArgsMode::Head` ⇒ `None` 档的 `arguments == {}` 断言变红。
    #[test]
    fn new_honors_args_mode() {
        fn read_call() -> AgentEvent {
            AgentEvent::ToolCall {
                call: ToolCall {
                    id: ToolCallId("c1".into()),
                    name: "read".into(),
                    arguments: json!({"path": "src/a.rs", "limit": 20}),
                },
            }
        }

        let t0 = Instant::now();

        let mut head = RoundClock::default();
        head.observe_debug(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        let head_observation = head.observe_debug(&env_of(read_call()), t0 + MS);
        assert_eq!(
            head_observation.records[0].fields["gen_ai.tool.call.arguments"]["path"],
            json!("src/a.rs"),
            "Default 走 Head 档：保留 path"
        );

        let mut none = RoundClock::new(ArgsMode::None);
        none.observe_debug(&env_of(AgentEvent::RoundStarted { round: 2 }), t0);
        let none_observation = none.observe_debug(&env_of(read_call()), t0 + MS);
        let fields = &none_observation.records[0].fields;
        assert_eq!(
            fields["gen_ai.tool.call.arguments"],
            json!({}),
            "None 档只留 keys / bytes，参数投影为空对象"
        );
        assert_eq!(fields["args_truncated"], json!(true));
    }

    /// `tool_call` 记录带 `command_fp`：命令类工具给数字，非命令类工具给 `null`；
    /// 记录里**只有指纹、没有命令正文**。
    ///
    /// 变异：把 `command_fp` 改成落**命令正文** ⇒ 「正文不得进记录」断言变红。
    #[test]
    fn tool_call_record_carries_fingerprint_not_command_body() {
        let long = format!("cargo test {}", "x".repeat(400));
        let t0 = Instant::now();
        let mut clock = RoundClock::default();

        clock.observe_debug(&env_of(AgentEvent::RoundStarted { round: 1 }), t0);
        let bash = clock.observe_debug(&env_of(bash_call("c1", &long)), t0 + MS);
        let read = clock.observe_debug(
            &env_of(AgentEvent::ToolCall {
                call: ToolCall {
                    id: ToolCallId("c2".into()),
                    name: "read".into(),
                    arguments: json!({ "path": "a.rs" }),
                },
            }),
            t0 + 2 * MS,
        );

        let fields = &bash.records[0].fields;
        assert!(fields["command_fp"].is_u64(), "命令类工具的指纹是数字");
        let serialized = serde_json::to_string(fields).unwrap();
        assert!(
            !serialized.contains(&long),
            "命令正文不得进记录：{serialized}"
        );
        assert_eq!(
            read.records[0].fields["command_fp"],
            Value::Null,
            "非命令类工具无指纹"
        );
    }

    /// 合成一轮：2 次**相同**命令 + 1 次**等长异串**命令 + 1 次慢调用（6 s）。
    ///
    /// 四项指标都要能从 debug 记录**离线**读出：重复命令数 / 真命令耗时分布 /
    /// 按工具名分桶的耗时 / 工具阻塞占比。
    ///
    /// 变异：把 `command_fp` 换成 `args_original_bytes` ⇒ 「三条不同命令 ⇒ 三个指纹」
    /// 断言变红（等长异串的参数字节数相同 ⇒ 只剩两个指纹，重复数也会跟着错）。
    #[test]
    fn debug_records_expose_repeat_count_and_duration_buckets() {
        const COMMAND_A: &str = "cargo test --workspace"; // 21 B
        const COMMAND_TWIN: &str = "cargo test --worktrees"; // 21 B，与 A 等长但异串
        const COMMAND_SLOW: &str = "cargo build --release --all-targets";

        let t0 = Instant::now();
        let mut clock = RoundClock::default();
        let mut records: Vec<DebugRecord> = Vec::new();
        for (event, offset_ms) in [
            (AgentEvent::RoundStarted { round: 1 }, 0u64),
            (bash_call("c1", COMMAND_A), 10),
            (bash_call("c2", COMMAND_A), 20),
            (bash_call("c3", COMMAND_TWIN), 30),
            (bash_call("c4", COMMAND_SLOW), 40),
            (tool_result_id("c1"), 110),
            (tool_result_id("c2"), 120),
            (tool_result_id("c3"), 130),
            (tool_result_id("c4"), 6040),
            (AgentEvent::RoundStarted { round: 2 }, 6100),
        ] {
            let at = t0 + Duration::from_millis(offset_ms);
            records.extend(clock.observe_debug(&env_of(event), at).records);
        }

        let calls: Vec<&DebugRecord> = records
            .iter()
            .filter(|record| record.ev == "tool_call")
            .collect();
        let results: Vec<&DebugRecord> = records
            .iter()
            .filter(|record| record.ev == "tool_result")
            .collect();
        assert_eq!(calls.len(), 4, "四次调用");
        assert_eq!(results.len(), 4, "四条结果");

        // 指标一 · 重复命令数：同一回合内同一条命令串出现 >1 次。
        let fingerprints: Vec<u64> = calls
            .iter()
            .filter_map(|record| record.fields["command_fp"].as_u64())
            .collect();
        let distinct: HashSet<u64> = fingerprints.iter().copied().collect();
        assert_eq!(distinct.len(), 3, "三条不同命令 ⇒ 三个不同指纹");
        assert_eq!(
            fingerprints.len() - distinct.len(),
            1,
            "重复命令数 = 1（只有命令 A 重复了一次）"
        );

        // 指标二 · 真命令耗时分布：单条调用耗时超过阈值（5 s）。
        let slow: Vec<u64> = results
            .iter()
            .filter_map(|record| record.fields["duration_ms"].as_u64())
            .filter(|ms| *ms > 5_000)
            .collect();
        assert_eq!(slow, vec![6000], "只有那条慢调用超过 5 s");

        // 指标三 · 按工具名分桶的耗时：`tool_call` 的名字 + `tool_call_id`
        // 与 `tool_result` 的 `tool_call_id` + `duration_ms` 离线关联。
        let mut buckets: HashMap<String, u64> = HashMap::new();
        for call in &calls {
            let id = call
                .tool_call_id
                .as_deref()
                .expect("tool_call 带 tool_call_id");
            let name = call.fields["gen_ai.tool.name"]
                .as_str()
                .expect("tool_call 带工具名")
                .to_string();
            let duration = results
                .iter()
                .find(|result| result.tool_call_id.as_deref() == Some(id))
                .and_then(|result| result.fields["duration_ms"].as_u64())
                .expect("每个调用都有结果");
            *buckets.entry(name).or_default() += duration;
        }
        assert_eq!(
            buckets.get("bash"),
            Some(&6300),
            "四条调用全经 bash ⇒ 单桶合计 6300 ms"
        );

        // 指标四 · 工具阻塞占比：`round_end.tools_ms` / 回合墙钟
        // （整回合的分母在日志里是 `turn_end.elapsed_ms`；此处以本轮墙钟为分母）。
        let end = records
            .iter()
            .find(|record| record.ev == "round_end")
            .expect("有 round_end 记录");
        let tools_ms = end.fields["tools_ms"].as_u64().expect("有 tools_ms");
        let total_ms = end.fields["total_ms"].as_u64().expect("有 total_ms");
        assert_eq!(tools_ms, 6030, "工具段 = 首个调用 → 末个结果");
        assert_eq!(total_ms, 6040, "本轮墙钟");
        assert!(
            tools_ms as f64 / total_ms as f64 > 0.99,
            "工具阻塞占比 = {tools_ms}/{total_ms}"
        );
    }
}
