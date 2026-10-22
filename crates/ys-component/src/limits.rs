use super::CompactPolicy;

/// 同一批并发工具调用的默认上限（ADR-0022 决策 14 的**单一真值**）。
///
/// 放在 `ys-component`（`RunLimits` 所在 crate）：`ys-loop` 依赖 `ys-component`，
/// 反向不成立 —— 常量若放 `ys-loop`，`RunLimits::default()` 就引用不到它，
/// 「单一真值」失效。`prompt.rs` 的 guideline 也应读同一个常量。
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;

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
}
