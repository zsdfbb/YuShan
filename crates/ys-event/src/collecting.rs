use std::future::Future;
use std::pin::Pin;

use super::{AgentEvent, EventSink};
use ys_core::EventError;

/// 在内存中收集所有已发出事件的 event sink。
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
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.events.push(event);
        Ok(())
    }

    fn emit<'a>(
        &'a mut self,
        event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>> {
        Box::pin(async move {
            // 无背压：`try_emit` 恒 `Ok`，直接委托以免两条路径各写一份 push
            let _ = self.try_emit(event);
            Ok(())
        })
    }
}
