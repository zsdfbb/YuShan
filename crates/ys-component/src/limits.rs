use super::CompactPolicy;
use std::time::Duration;

/// 同一批并发工具调用的默认上限（ADR-0022 决策 14 的**单一真值**）。
///
/// 放在 `ys-component`（`RunLimits` 所在 crate）：`ys-loop` 依赖 `ys-component`，
/// 反向不成立 —— 常量若放 `ys-loop`，`RunLimits::default()` 就引用不到它，
/// 「单一真值」失效。`prompt.rs` 的 guideline 也应读同一个常量。
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;

/// 「闷头干活」提醒的默认阀值（连续多少次工具调用而模型未输出正文）。
///
/// 与 [`RunLimits::narration_silence_tool_calls`] 同源，放在本 crate 的理由与
/// [`DEFAULT_MAX_PARALLEL_TOOLS`] 一致：`RunLimits::default()` 要能引用到它。
///
/// 取 **12** 的依据（本仓实测会话逐条回测）：
/// - 上限 8  ⇒ 全程多插 4 条，最长等待 ≈ 2.0 分钟
/// - 上限 **12** ⇒ 全程多插 **3** 条，最长等待 ≈ **3.0 分钟**
/// - 上限 20 ⇒ 全程多插 1 条，最长等待 ≈ 5.0 分钟
///
/// 未加此阀时实测出现过**连续 38 条调用不吭声**的段 ≈ 565 s（9.4 分钟）。
pub const DEFAULT_NARRATION_SILENCE_TOOL_CALLS: usize = 12;

/// 回合内**累计工具调用数**阈值的**应用默认值**（A4）。
///
/// 库面默认是 `0`（关闭）⇒ 老调用方逐字节不变；app 侧显式设为本值。
/// 取 **12**：10–15 轮的任务仍能在中段触发；626 次调用的长会话会在
/// 12 / 24 / 48 / 96 / 192 / 384 各触发一次（约 6 次，每次一行）。
pub const DEFAULT_BATCH_NUDGE_AT_TOOL_CALLS: usize = 12;

/// 回合内轮数软提醒阈值的**应用默认值**（A6）。
///
/// 库面默认是 `0`（关闭）⇒ 老调用方逐字节不变；app 侧显式设为本值。
pub const DEFAULT_SOFT_ROUND_ADVICE_AT: usize = 60;

/// 一次 run 的执行限制
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunLimits {
    /// 每个 turn 中 model call（rounds）的最大次数
    pub max_rounds: u32,
    /// Bash 命令超时毫秒数（None = 无默认超时）
    pub bash_timeout: Option<u64>,
    /// model context window 大小（以 token 计）
    ///
    /// 默认 **1_000_000**。目录（provider 自报）是第一手来源；本值是**兜底**，
    /// 取 1M 而非更小的数，是因为压缩是**保守动作**（提前压 ⇒ 只是多花一次摘要
    /// 调用），而窗口**估小了**会直接顶到服务端上限 ⇒ 400 报错。两者不对称，
    /// 故兜底取**偏大**的一侧。
    pub context_window: usize,
    /// context 压缩策略（阈值与保留预算；算法在 `ys-loop`）
    pub compaction: CompactPolicy,
    /// **每回合固定开销**的估算 token 数：system prompt + 工具表（ToolSpec）等
    /// 「每次请求都要重发、却既不随会话增长、也不参与压缩」的部分。
    ///
    /// 压缩触发判定（`ys-loop` 的 Step 4b）只看得见 session 消息
    /// （`token::estimate_session_tokens`）。不把这块固定开销加进去，压缩就
    /// **系统性晚触发** —— 窗口里真实被占掉的部分比判定的多。9 工具时缺口约 1.1k
    /// 尚可忽略；skills 上线后 skill 目录常驻 ~9.6k，晚触发不可接受。
    ///
    /// 默认 `0` ⇒ 既有行为逐字节不变（老调用方无需改动）。
    pub fixed_prompt_tokens: usize,
    /// 同一批并发工具调用的上限（见 [`DEFAULT_MAX_PARALLEL_TOOLS`]）。
    ///
    /// 只有 `BasicLoop` Step 9 的并行分支读它；默认值取自常量 ⇒ 既有行为不变。
    pub max_parallel_tools: usize,
    /// **回合级**工具结果 token 总预算：一个回合内所有工具结果估算 token 之和的上限，
    /// 超预算时由 `ys-loop` 降级（头部截断 + 续读指针）**更早**的结果。
    ///
    /// 与第一批的**单条**上限（grep 专用预算、`MAX_BYTES`）互补：单条封顶后，
    /// 一个回合里的**多条**结果仍可累积放大。默认 `0` ⇒ 关闭，既有消费者 / 测试
    /// 逐字节不变（沿用 [`fixed_prompt_tokens`](Self::fixed_prompt_tokens) 的承重默认惯例）。
    pub max_turn_tool_result_tokens: usize,
    /// 空响应是否**重试一次**：模型只产出思考、无正文也无工具调用时，先催促重发；
    /// 仍空才收为 `NoOutput`。
    ///
    /// 默认 `true`（直接提升「有答复」的概率）；置 `false` ⇒ 与既有行为逐字节一致
    /// （首轮空响应当即收场）。重试消耗一个 round，受 `max_rounds` 与轮边界 abort 约束。
    pub retry_empty_response: bool,
    /// 收场前**等待在跑子 agent** 的上限（ADR-0055）。
    ///
    /// 模型给出最终答复时若仍有子 agent 在跑，父回合**不许收场** —— 结果回来要作为
    /// 同一回合的下一轮输入，模型才能汇报并继续。本值是该等待的硬上限。
    ///
    /// 默认 **30 min**：实测最长一次派发 965 s（`subagent.rs` 的注释记录），留约 2 倍余量。
    /// 装配点会再按 `min(本值, max_turn_duration)` 收紧一次 —— 否则父 budget 到点会
    /// 推 `Abort` 并打印「budget exhausted」，而回合其实只是在正常等待。
    pub subagent_wait: Duration,
    /// 等待期间的唤醒兜底间隔（ADR-0055）。
    ///
    /// **不是状态轮询**：主路径是 `SubagentRunner::settled()` 的即时唤醒。本值只为两件事
    /// 兜底 —— ① `Boundary::Abort` 的生效延迟（等待返回后必经 Step 3 探针）；
    /// ② 「结果已 push 进边界源但无唤醒源」这条缝隙（`QueueBoundarySource::push` 是同步的，
    /// 不通知任何人）。
    pub subagent_poll: Duration,
    /// 「闷头干活」的阀值：本回合累计**连续**这么多次工具调用而模型一个字都没说，
    /// 主循环就往下一轮的请求里补一条**仅本轮可见**的提醒。
    ///
    /// # 为何必须由 harness 计数
    ///
    /// 模型看不到自己的累计调用次数 —— 把它写进系统提示词（「连续 N 次调用后说一句」）
    /// 是无效的：它无法判定何时到阀。计数必须由能看到每次 `ToolUse` 的一方做。
    ///
    /// # 为何是「连续」而不仅是「累计」
    ///
    /// 阀值管的是「用户多久没听到声音」。模型一旦输出正文，计时就应归零 ——
    /// 否则一个每轮都说话的回合也会在累计到 N 时挨一条莫名其妙的提醒。
    ///
    /// 默认 **12**：按本仓实测会话回测，上限 12 ⇒ 全程只多 3 条提醒，
    /// 对应最长等待 ≈ 3 分钟（每轮 ≈ 14.9s，其中工具执行仅占 2.8%）。
    /// `0` ⇒ 关闭（既有消费者逐字节不变）。
    pub narration_silence_tool_calls: usize,
    /// 主轮是否**关闭思考**。`None` = 不干预（默认）；`Some(false)` = 关。
    ///
    /// # 为何默认是 `None`
    ///
    /// **承重默认**：既有消费者（不设该字段）的行为逐字节不变，沿用
    /// `fixed_prompt_tokens = 0` / `max_turn_tool_result_tokens = 0` 的惯例。
    /// 设成 `Some(false)` 等于对所有老调用方做一次静默的思考能力削减。
    ///
    /// # 为何只覆盖主轮
    ///
    /// 工具摘要已恒为关（`summarize.rs`，理由含「思考吃光 1024 预算 ⇒ 空正文」）；
    /// 压缩**保持开**（结果会进会话，错了要付后续每轮的代价，见 `docs/TODO.md`）。
    /// 子 agent 复用本字段？**不** —— 子 agent 由 `assembly.rs` 克隆父 `limits`，
    /// 故它继承父的选择；要分开就得给子 agent 单独的字段，等真实需求出现再说。
    ///
    /// 成本口径：DeepSeek 实测关思考可省 **18.3%** 总费用
    /// （`docs/arch/effort-per-call-type/cost-analysis.md`）。
    pub main_disable_thinking: Option<bool>,
    /// 流式中断后**重试整个模型轮**的次数上限（默认 `0` = 关闭）。
    ///
    /// 背景：provider 流可以返回 `200` + 正常 `ttfb`，然后在持续输出几十秒后
    /// 被**中途截断**。此类故障归为 `ModelError::Provider`，且因为**已吐出 delta**
    /// 而被适配器拒绝重试（重放会把已渲染文本再渲染一遍）。
    ///
    /// 但那个理由只对「流边渲染边给用户看」的路径成立。**子 agent 的输出不进
    /// 用户屏幕**（它只把最终 summary 交回父），所以对子 agent 可以安全地丢弃
    /// 已收到的部分输出、用**同一请求**从头重发 —— 会话未被污染（`Err` 分支不
    /// `append` 任何消息），故重试是幂等的。
    ///
    /// 账：用一次重复的 token 支出，换回前面几十上百次工具调用的上下文不丢。
    /// 只在子 agent 场景划算（重建上下文要几十轮），故默认 `0`，由 subagent
    /// 的 child_limits 置非零。
    ///
    /// **只对 `ModelError::Provider` 生效**：`Cancelled` 是用户意图，`Sink` /
    /// `Serde` 是本地缺陷 —— 三者重试都无意义。
    pub stream_interrupt_retries: u32,
    /// 回合内**累计工具调用数**阈值（A4）；到阈值且当前 `tpr < 1.5` 时在**下一轮请求**
    /// 末尾补一条批处理提醒，阈值触发后 ×2。
    ///
    /// # 为何是累计而不是连续
    ///
    /// 累计单调、永不重置：旧判据按「连续单调用轮」计数，中间夹一轮多调用就清零，
    /// 实测只触发 4/10 次且多落在回合末尾（那时提醒改变不了任何东西）。
    ///
    /// # 为何要 `tpr < 1.5` 过滤
    ///
    /// `tpr`（tools per round）已经 ≥ 1.5 的回合，模型**本来就在批处理**，此时提醒
    /// 是错的建议。故到阈值但 `tpr >= 1.5` 时**不触发、也不推进阈值**，下一轮再判。
    ///
    /// 默认 `0` = 关闭（**核心默认**）⇒ 老调用方逐字节不变。
    /// app 默认 [`DEFAULT_BATCH_NUDGE_AT_TOOL_CALLS`]。
    pub batch_nudge_at_tool_calls: usize,
    /// 回合内轮数软提醒阈值（A6）：到 60 / 120 / 240 … 各提醒一次。
    ///
    /// 每回合重置，触发后阈值 ×2；不产生事件、不落 session、不中断回合。
    /// 默认 `0` = 关闭（**核心默认**）。app 默认 [`DEFAULT_SOFT_ROUND_ADVICE_AT`]。
    pub soft_round_advice_at: usize,
}

impl RunLimits {
    pub fn new(max_rounds: u32) -> Self {
        Self {
            max_rounds,
            ..Default::default()
        }
    }
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_rounds: 10,
            bash_timeout: None,
            context_window: 1_000_000,
            compaction: CompactPolicy::default(),
            // 0 是**承重**默认值：老调用方（不设该字段）的压缩行为逐字节不变。
            fixed_prompt_tokens: 0,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            // 0 = 关闭回合级闸门 ⇒ 老调用方逐字节不变。
            max_turn_tool_result_tokens: 0,
            // true = 启用一次空响应重试；置 false 时行为与旧版一致。
            retry_empty_response: true,
            // 30 min：实测最长派发 965 s，留约 2 倍余量。
            subagent_wait: Duration::from_secs(30 * 60),
            // 200ms：只兜 abort 延迟与边界源缝隙，不是状态轮询。
            subagent_poll: Duration::from_millis(200),
            // 12：实测回测下全程只多 3 条提醒，最长静默 ≈ 3 分钟。
            narration_silence_tool_calls: DEFAULT_NARRATION_SILENCE_TOOL_CALLS,
            // None = 不干预 ⇒ 既有行为逐字节不变（主轮仍随 provider 默认思考）。
            main_disable_thinking: None,
            // 0 = 关闭 ⇒ 既有行为逐字节不变（客户端不重发已开始的流）。
            stream_interrupt_retries: 0,
            // 0 = 关闭（核心默认）⇒ 老调用方逐字节不变；app 侧显式设 12。
            batch_nudge_at_tool_calls: 0,
            // 0 = 关闭（核心默认）⇒ 老调用方逐字节不变；app 侧显式设 60。
            soft_round_advice_at: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新字段默认必须是 `0` —— 这是「既有压缩行为逐字节不变」的结构保证。
    ///
    /// 变异自检：把 `Default` 里的 `fixed_prompt_tokens: 0` 改成任何非零值 ⇒ 本用例
    /// 变红（且全仓既有压缩用例的阈值判定会整体偏移）。
    #[test]
    fn fixed_prompt_tokens_defaults_to_zero() {
        assert_eq!(RunLimits::default().fixed_prompt_tokens, 0);
        assert_eq!(RunLimits::new(5).fixed_prompt_tokens, 0);
        assert_eq!(RunLimits::new(0).fixed_prompt_tokens, 0);
    }

    /// 该字段是**普通可写字段**：调用方能显式设成非零，别被默认值绑死。
    ///
    /// （跨 crate 的实际写法是 `RunLimits::default()` + 字段赋值 —— `#[non_exhaustive]`
    /// 只禁止**外** crate 用结构体字面量；本用例在定义 crate 内，故字面量合法。）
    #[test]
    fn fixed_prompt_tokens_is_settable() {
        let limits = RunLimits {
            fixed_prompt_tokens: 9_600,
            ..Default::default()
        };
        assert_eq!(limits.fixed_prompt_tokens, 9_600);

        let mut assigned = RunLimits::new(5);
        assigned.fixed_prompt_tokens = 42;
        assert_eq!(assigned.fixed_prompt_tokens, 42);
    }

    /// N17：两个新字段的默认值必须钉死 —— `max_turn_tool_result_tokens == 0`（闸门关闭）、
    /// `retry_empty_response == true`（启用重试）。
    ///
    /// 变异自检：把 `Default` 里的 `max_turn_tool_result_tokens: 0` 改成任何非零值 ⇒
    /// 本用例变红，且既有「预算内逐字节不变」的用例会整体偏移；把 `retry_empty_response`
    /// 改成 `false` ⇒ 本用例变红，且首轮空响应会被提前收场。
    #[test]
    fn new_budget_fields_default_values() {
        assert_eq!(RunLimits::default().max_turn_tool_result_tokens, 0);
        assert_eq!(RunLimits::new(5).max_turn_tool_result_tokens, 0);
        assert!(RunLimits::default().retry_empty_response);
        assert!(RunLimits::new(5).retry_empty_response);
    }

    /// 两个新字段是**普通可写字段**：调用方能显式设成非默认值，别被默认值绑死。
    ///
    /// 变异自检：把 `Default` 里的字段名写错（编译不过）或让字段不可写 ⇒ 本用例变红。
    #[test]
    fn new_budget_fields_are_settable() {
        let limits = RunLimits {
            max_turn_tool_result_tokens: 32_000,
            retry_empty_response: false,
            ..Default::default()
        };
        assert_eq!(limits.max_turn_tool_result_tokens, 32_000);
        assert!(!limits.retry_empty_response);

        let mut assigned = RunLimits::new(5);
        assigned.max_turn_tool_result_tokens = 1;
        assigned.retry_empty_response = false;
        assert_eq!(assigned.max_turn_tool_result_tokens, 1);
        assert!(!assigned.retry_empty_response);
    }

    /// ADR-0055：`subagent_wait` / `subagent_poll` 的默认值。
    ///
    /// 两者必须**非零** —— `subagent_wait == 0` 会让「等子 agent」变成「立刻收场」
    /// （功能静默失效），`subagent_poll == 0` 会让等待循环忙转。
    ///
    /// 变异自检：把 `subagent_wait` 默认改 `Duration::ZERO` ⇒ 本用例变红；
    /// 把 30 min 改成 15 min ⇒ 第二段断言变红（实测最长派发 965 s，15 min 余量不足）。
    #[test]
    fn subagent_wait_fields_default_and_are_settable() {
        let d = RunLimits::default();
        assert_eq!(d.subagent_wait, Duration::from_secs(30 * 60));
        assert_eq!(d.subagent_poll, Duration::from_millis(200));
        assert!(!d.subagent_wait.is_zero(), "0 会让等待静默失效");
        assert!(!d.subagent_poll.is_zero(), "0 会让等待循环忙转");
        // 15 min 不够：实测最长派发 965 s ≈ 16 min。
        assert!(
            d.subagent_wait >= Duration::from_secs(30 * 60),
            "默认上限必须给 965 s 的实测最长派发留余量"
        );

        let limits = RunLimits {
            subagent_wait: Duration::from_millis(80),
            subagent_poll: Duration::from_millis(10),
            ..Default::default()
        };
        assert_eq!(limits.subagent_wait, Duration::from_millis(80));
        assert_eq!(limits.subagent_poll, Duration::from_millis(10));
    }

    /// A4 / A6 的库面默认必须是 `0`（关闭）—— 这是「老调用方逐字节不变」的承重默认；
    /// app 侧才引用非零的应用默认常量。
    ///
    /// 变异自检：把任一字段的默认改成非零 ⇒ 本用例变红，且未显式配置的老调用方
    /// 会凭空多出提醒。
    #[test]
    fn reminder_thresholds_default_to_zero() {
        assert_eq!(RunLimits::default().batch_nudge_at_tool_calls, 0);
        assert_eq!(RunLimits::default().soft_round_advice_at, 0);
        assert_eq!(RunLimits::new(5).batch_nudge_at_tool_calls, 0);
        assert_eq!(RunLimits::new(5).soft_round_advice_at, 0);
        // 应用默认常量供 app 侧引用，不得被核心默认吞掉。
        assert_eq!(DEFAULT_BATCH_NUDGE_AT_TOOL_CALLS, 12);
        assert_eq!(DEFAULT_SOFT_ROUND_ADVICE_AT, 60);
    }
}
