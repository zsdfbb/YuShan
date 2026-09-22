use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    Completed,
    MaxRounds,
    Cancelled,
    /// 模型输出被输出预算截断（provider 的 finish_reason 为 length/max_tokens）。
    Truncated,
}
