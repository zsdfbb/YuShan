use super::{AgentEvent, EventSink};
use agent_core::EventError;

/// An event sink that silently discards all events.
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&mut self, _event: AgentEvent) -> Result<(), EventError> {
        Ok(())
    }
}
