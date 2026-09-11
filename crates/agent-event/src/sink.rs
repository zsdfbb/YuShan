use super::AgentEvent;
use agent_core::EventError;

/// 基于 push 的同步 event sink。
pub trait EventSink: Send {
    fn emit(&mut self, event: AgentEvent) -> Result<(), EventError>;
}
