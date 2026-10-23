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

    // 自由函数**总是**走慢路径（死锁修复）：不再先试同步快路径。
    // 旧实现「稳态走快路径」会在信道满时把终局事件缓冲进 sink 而无人冲刷 → 死锁。
    #[tokio::test]
    async fn test_free_emit_always_goes_slow_path() {
        let mut sink = FailingSink::always_ok();
        emit(&mut sink, AgentEvent::ModelTextDelta { text: "a".into() })
            .await
            .unwrap();
        assert_eq!(sink.slow_calls(), 1, "自由函数必须走慢路径");
        assert_eq!(sink.try_calls(), 0, "自由函数不再调用同步快路径");
        assert_eq!(sink.emitted().len(), 1);
    }

    // 自由函数经慢路径投递，且不依赖快路径是否成功
    // （FailingSink::new(0) 的快路径必失败，但自由函数根本不调用它）。
    #[tokio::test]
    async fn test_free_emit_delivers_via_slow_path() {
        let mut sink = FailingSink::new(0);
        emit(&mut sink, AgentEvent::ModelTextDelta { text: "a".into() })
            .await
            .unwrap();
        assert_eq!(sink.try_calls(), 0, "自由函数不应触碰快路径");
        assert_eq!(sink.slow_calls(), 1);
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
        assert_eq!(sink.try_calls(), 0, "自由函数不再触碰快路径");
        assert_eq!(sink.slow_calls(), 1);
        assert!(sink.emitted().is_empty(), "慢路径失败，不应有投递");
    }

    #[test]
    fn test_agent_event_serde_roundtrip() {
        let event = AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            rounds: 3,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    // 压缩事件：加法式新增，序列化 / 反序列化roundtrip 必须稳定
    // （`--json` 模式逐行打印 `Envelope`，字段名即对外契约）。
    #[test]
    fn test_agent_event_compaction_serde_roundtrip() {
        let compacted = AgentEvent::ContextCompacted {
            estimated_tokens_before: 12_000,
            estimated_tokens_after: 3_500,
            summarized: 18,
            kept: 4,
        };
        let json = serde_json::to_string(&compacted).unwrap();
        assert!(
            json.contains("estimated_tokens_before"),
            "字段名里的 estimated_ 前缀是有意的，不得简写成 tokens_before：{json}"
        );
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(compacted, back);

        let failed = AgentEvent::ContextCompactionFailed {
            reason: "summarizer returned empty".into(),
        };
        let json = serde_json::to_string(&failed).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(failed, back);
    }

    // 两个压缩变体都是**非终局**事件 —— 消费循环（`-p` / `--json`）的退出条件
    // 只认 `RunFinished` / `RunFailed`，故压缩事件必须能落到兜底分支、不产生输出。
    #[test]
    fn test_compaction_events_destructure_as_non_terminal() {
        let compacted = AgentEvent::ContextCompacted {
            estimated_tokens_before: 100,
            estimated_tokens_after: 40,
            summarized: 3,
            kept: 1,
        };
        match compacted {
            AgentEvent::ContextCompacted {
                estimated_tokens_before,
                estimated_tokens_after,
                summarized,
                kept,
            } => {
                assert_eq!(estimated_tokens_before, 100);
                assert_eq!(estimated_tokens_after, 40);
                assert_eq!(summarized, 3);
                assert_eq!(kept, 1);
            }
            other => panic!("期望 ContextCompacted，实得 {other:?}"),
        }

        let failed = AgentEvent::ContextCompactionFailed {
            reason: "boom".into(),
        };
        assert!(
            matches!(&failed, AgentEvent::ContextCompactionFailed { reason } if reason == "boom")
        );
    }

    // 截断事件：加法式新增，序列化 / 反序列化 roundtrip 必须稳定
    // （`--json` 模式逐行打印 `Envelope`，字段名即对外契约）。
    #[test]
    fn test_agent_event_truncated_serde_roundtrip() {
        let truncated = AgentEvent::ResponseTruncated {
            dropped_tool_calls: 2,
        };
        let json = serde_json::to_string(&truncated).unwrap();
        assert!(
            json.contains("dropped_tool_calls"),
            "字段名是对外契约，不得改名：{json}"
        );
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(truncated, back);

        // 纯文本被截断 = 0 个工具调用被作废
        let text_only = AgentEvent::ResponseTruncated {
            dropped_tool_calls: 0,
        };
        let json = serde_json::to_string(&text_only).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(text_only, back);
    }

    // `ResponseTruncated` 是**非终局**事件 —— 消费循环（`-p` / `--json`）的退出条件
    // 只认 `RunFinished` / `RunFailed`，故截断事件必须能落到兜底分支、不产生输出。
    #[test]
    fn test_truncated_event_destructures_as_non_terminal() {
        let truncated = AgentEvent::ResponseTruncated {
            dropped_tool_calls: 3,
        };
        match truncated {
            AgentEvent::ResponseTruncated { dropped_tool_calls } => {
                assert_eq!(dropped_tool_calls, 3);
            }
            other => panic!("期望 ResponseTruncated，实得 {other:?}"),
        }
    }

    // 轮开始事件：加法式新增，序列化 / 反序列化 roundtrip 必须稳定
    // （`--json` 模式逐行打印 `Envelope`，字段名即对外契约）。
    #[test]
    fn test_agent_event_round_started_serde_roundtrip() {
        let started = AgentEvent::RoundStarted { round: 7 };
        let json = serde_json::to_string(&started).unwrap();
        assert!(json.contains("round"), "字段名是对外契约，不得改名：{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(started, back);
    }

    // `RoundStarted` 是**非终局**事件 —— 消费循环（`-p` / `--json`）的退出条件只认
    // `RunFinished` / `RunFailed`，故本事件必须能落到兜底分支、不产生输出。
    #[test]
    fn test_round_started_event_destructures_as_non_terminal() {
        let started = AgentEvent::RoundStarted { round: 3 };
        match started {
            AgentEvent::RoundStarted { round } => assert_eq!(round, 3),
            other => panic!("期望 RoundStarted，实得 {other:?}"),
        }
    }
}
