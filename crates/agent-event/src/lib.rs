//! agent-event: 事件枚举与推式出口

mod collecting;
mod event;
mod noop;
mod sink;

pub use collecting::*;
pub use event::*;
pub use noop::*;
pub use sink::*;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::*;

    #[test]
    fn test_noop_sink() {
        let mut sink = NoopEventSink;
        let event = AgentEvent::ModelTextDelta {
            text: "hello".into(),
        };
        assert!(sink.emit(event).is_ok());
    }

    #[test]
    fn test_collecting_sink() {
        let mut sink = CollectingSink::new();
        sink.emit(AgentEvent::UserMessage {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            },
        })
        .unwrap();
        sink.emit(AgentEvent::ModelTextDelta {
            text: "world".into(),
        })
        .unwrap();
        assert_eq!(sink.events().len(), 2);
        assert!(matches!(&sink.events()[0], AgentEvent::UserMessage { .. }));
        assert!(
            matches!(&sink.events()[1], AgentEvent::ModelTextDelta { text } if text == "world")
        );
    }

    #[test]
    fn test_agent_event_serde_roundtrip() {
        let event = AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
            },
            rounds: 3,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }
}
