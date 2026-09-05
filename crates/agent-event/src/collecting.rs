use super::{AgentEvent, EventSink};
use agent_core::EventError;

/// An event sink that collects all emitted events in memory.
pub struct CollectingSink {
    events: Vec<AgentEvent>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn events(&self) -> &[AgentEvent] {
        &self.events
    }
}

impl Default for CollectingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for CollectingSink {
    fn emit(&mut self, event: AgentEvent) -> Result<(), EventError> {
        self.events.push(event);
        Ok(())
    }
}
