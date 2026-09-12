use ys_core::EventError;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("event sink error: {0}")]
    Sink(#[from] EventError),
    #[error("model provider error: {0}")]
    Provider(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
