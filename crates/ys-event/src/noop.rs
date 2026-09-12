use std::future::Future;
use std::pin::Pin;

use super::{AgentEvent, EventSink};
use ys_core::EventError;

/// 静默丢弃全部事件的 event sink。
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn try_emit(&mut self, _event: AgentEvent) -> Result<(), AgentEvent> {
        Ok(())
    }

    fn emit<'a>(
        &'a mut self,
        _event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}
