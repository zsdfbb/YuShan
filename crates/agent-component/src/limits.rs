/// Execution limits for a run
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunLimits {
    /// Maximum number of model calls (rounds) per turn
    pub max_rounds: u32,
    /// Bash command timeout in milliseconds (None = no default timeout)
    pub bash_timeout: Option<u64>,
    /// Model context window size in tokens
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
