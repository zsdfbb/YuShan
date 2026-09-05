use super::AgentEvent;
use agent_core::EventError;

/// Synchronous push-based event sink.
pub trait EventSink: Send {
    fn emit(&mut self, event: AgentEvent) -> Result<(), EventError>;
}
