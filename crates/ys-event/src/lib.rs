//! ys-event: 事件枚举与推式出口

mod collecting;
mod event;
mod failing;
mod noop;
mod sink;

pub use collecting::*;
pub use event::*;
pub use failing::*;
pub use noop::*;
pub use sink::*;

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::*;

    #[tokio::test]
    async fn test_noop_sink() {
        let mut sink = NoopEventSink;
        let event = AgentEvent::ModelTextDelta {
            text: "hello".into(),
        };
        assert!(emit(&mut sink, event).await.is_ok());
    }

    #[tokio::test]
    async fn test_collecting_sink() {
        let mut sink = CollectingSink::new();
        emit(
            &mut sink,
            AgentEvent::UserMessage {
                message: Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: "hi".into() }],
                },
            },
        )
        .await
        .unwrap();
        emit(
            &mut sink,
            AgentEvent::ModelTextDelta {
                text: "world".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(sink.events().len(), 2);
        assert!(matches!(&sink.events()[0], AgentEvent::UserMessage { .. }));
        assert!(
            matches!(&sink.events()[1], AgentEvent::ModelTextDelta { text } if text == "world")
        );
    }

    // Task 4 合同 (a)：自由函数稳态只走快路径
    #[tokio::test]
    async fn test_free_emit_fast_path_only() {
        let mut sink = FailingSink::always_ok();
        emit(&mut sink, AgentEvent::ModelTextDelta { text: "a".into() })
            .await
            .unwrap();
        assert_eq!(sink.slow_calls(), 0, "稳态不应走慢路径");
        assert_eq!(sink.try_calls(), 1);
        assert_eq!(sink.emitted().len(), 1);
    }

    // Task 4 合同 (b)：快路径失败 → 回落慢路径
    #[tokio::test]
    async fn test_free_emit_falls_back_to_slow_path() {
        let mut sink = FailingSink::new(0); // 首次就失败
        emit(&mut sink, AgentEvent::ModelTextDelta { text: "a".into() })
            .await
            .unwrap();
        assert_eq!(sink.try_calls(), 1, "快路径应被尝试一次");
        assert_eq!(sink.slow_calls(), 1, "快路径失败应回落慢路径");
        assert_eq!(sink.emitted().len(), 1, "慢路径应成功投递该事件");
        assert!(
            matches!(&sink.emitted()[0], AgentEvent::ModelTextDelta { text } if text == "a"),
            "慢路径收到的必须是同一事件"
        );
    }

    // Task 4 合同 (c)：try_emit 失败时事件载荷原样退回
    #[test]
    fn test_try_emit_returns_event_on_failure() {
        let mut sink = FailingSink::new(0);
        let ev = AgentEvent::ModelTextDelta {
            text: "payload".into(),
        };
        match sink.try_emit(ev.clone()) {
            Err(returned) => assert_eq!(returned, ev, "退回的事件必须与传入完全相同"),
            Ok(()) => panic!("expected failure"),
        }
    }

    // Task 4 合同 (d)：begin_turn 默认 no-op
    #[test]
    fn test_begin_turn_default_is_noop() {
        let mut sink = NoopEventSink;
        sink.begin_turn(7); // 不应 panic、不应有副作用
        assert!(
            sink.try_emit(AgentEvent::ModelTextDelta { text: "x".into() })
                .is_ok()
        );
    }

    // Task 4 合同 (e)：慢路径失败经自由函数传播为 EventError::SendFailed
    #[tokio::test]
    async fn test_slow_path_failure_propagates() {
        let mut sink = FailingSink::new(0);
        sink.set_fail_slow(true); // 让慢路径也失败
        let r = emit(&mut sink, AgentEvent::ModelTextDelta { text: "a".into() }).await;
        assert!(matches!(r, Err(EventError::SendFailed)));
        assert_eq!(sink.try_calls(), 1);
        assert_eq!(sink.slow_calls(), 1);
        assert!(sink.emitted().is_empty(), "两条路径都失败，不应有投递");
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
