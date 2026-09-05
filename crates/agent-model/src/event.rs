use agent_core::EventError;
use serde::{Deserialize, Serialize};

/// Events emitted by a model during streaming.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelEvent {
    TextDelta { text: String },
}

/// Narrow event sink for model-level events.
pub trait ModelEventSink: Send {
    fn emit(&mut self, event: ModelEvent) -> Result<(), EventError>;
}
