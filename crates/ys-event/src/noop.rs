use super::{AgentEvent, EventSink};
use ys_core::EventError;

/// 静默丢弃全部事件的 event sink。
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&mut self, _event: AgentEvent) -> Result<(), EventError> {
        Ok(())
    }
}
