use ys_core::{EventError, Message};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("event sink error: {0}")]
    Sink(#[from] EventError),
    #[error("model provider error: {0}")]
    Provider(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// 模型调用**被中止**（轮边界收到 `Abort`）。
    ///
    /// 与 [`ModelError::Provider`] 分开：中止是**用户的意图**，不是故障 ——
    /// 上层据此以 `StopReason::Cancelled` 收场，而非 `RunFailed`。
    ///
    /// `partial` 携带中止瞬间已收到的可见文本（`None` = 尚无可见输出）。
    /// 它**不含**工具调用：中止时未完成的工具调用参数可能残缺，
    /// 执行它等于拿半个 payload 去改状态。
    #[error("model call cancelled")]
    Cancelled { partial: Option<Message> },
}
