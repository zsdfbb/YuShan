use super::CompactPolicy;

/// 一次 run 的执行限制
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunLimits {
    /// 每个 turn 中 model call（rounds）的最大次数
    pub max_rounds: u32,
    /// Bash 命令超时毫秒数（None = 无默认超时）
    pub bash_timeout: Option<u64>,
    /// model context window 大小（以 token 计）
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
            context_window: 128_000,
            compaction: CompactPolicy::default(),
            // 0 是**承重**默认值：老调用方（不设该字段）的压缩行为逐字节不变。
            fixed_prompt_tokens: 0,
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
}
