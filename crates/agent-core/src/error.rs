use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error("event error: {0}")]
    Event(#[from] EventError),
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventError {
    #[error("failed to send event")]
    SendFailed,
}
