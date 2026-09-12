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
        }
    }
}
