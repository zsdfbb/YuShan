use agent_core::EventError;
use agent_model::ModelError;

/// Error from agent loop execution
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("event error: {0}")]
    Event(#[from] EventError),
    #[error("configuration error: {0}")]
    ConfigError(String),
}
