use super::{AgentEvent, EventSink};
use agent_core::EventError;

/// 静默丢弃全部事件的 event sink。
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&mut self, _event: AgentEvent) -> Result<(), EventError> {
        Ok(())
    }
}
