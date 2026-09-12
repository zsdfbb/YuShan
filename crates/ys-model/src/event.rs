use serde::{Deserialize, Serialize};
use ys_core::EventError;

/// model 在 streaming 期间发出的事件。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelEvent {
    TextDelta { text: String },
}

/// 面向 model 级事件的窄化 event sink。
pub trait ModelEventSink: Send {
    fn emit(&mut self, event: ModelEvent) -> Result<(), EventError>;
}
