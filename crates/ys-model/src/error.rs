use ys_core::{EventError, Message};


#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("event sink error: {0}")]
    Sink(#[from] EventError),
    #[error("model provider error: {0}")]
    Provider(String),
    /// 流**已开始后**中断（provider 掐断了响应体）。
    ///
    /// 与 [`Self::Provider`] 分开是上层重试判定的需要：本变体可以「丢弃已发出的增量
    /// 后整次重发」，而 `Provider` 还装着适配器**已退避耗尽**的传输失败 —— 对它再重试
    /// 会把适配器的重试预算再乘一遍。适配器只在自己不重试时（`meta.stream_started`）
    /// 产出本变体，两处重试机制因此是相加而非相乘。
    ///
    /// `Display` 前缀与 `Provider` **完全相同**：用户看到的错误文本不变，区分只对
    /// 程序可见。
    #[error("model provider error: {0}")]
    StreamInterrupted(String),
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
