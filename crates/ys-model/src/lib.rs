//! LLM 后端的 model 抽象层。

mod error;
mod event;
mod mock;
mod request;
mod trait_def;

pub use error::*;
pub use event::*;
pub use mock::*;
pub use request::*;
pub use trait_def::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use ys_core::*;

    struct MockEventSink;

    impl ModelEventSink for MockEventSink {
        fn emit(&mut self, _event: ModelEvent) -> Result<(), EventError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_mock_model_text() {
        let model = MockModel::new("test");
        model.push_text("hello world");
        let mut sink = MockEventSink;
        let req = ModelRequest {
            messages: vec![],
            tools: vec![],
            ..Default::default()
        };
        let resp = model.complete(req, &mut sink).await.unwrap();
        assert_eq!(resp.message.content.len(), 1);
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "hello world")
        );
    }

    #[tokio::test]
    async fn test_mock_model_tool_call() {
        let model = MockModel::new("test");
        model.push_tool_call("bash", serde_json::json!({"command": "ls"}));
        let mut sink = MockEventSink;
        let req = ModelRequest {
            messages: vec![],
            tools: vec![],
            ..Default::default()
        };
        let resp = model.complete(req, &mut sink).await.unwrap();
        assert!(
            matches!(&resp.message.content[0], ContentBlock::ToolUse { name, .. } if name == "bash")
        );
    }

    #[tokio::test]
    async fn test_mock_model_error() {
        let model = MockModel::new("test");
        model.push_error("connection failed");
        let mut sink = MockEventSink;
        let req = ModelRequest {
            messages: vec![],
            tools: vec![],
            ..Default::default()
        };
        let err = model.complete(req, &mut sink).await.unwrap_err();
        assert!(matches!(err, ModelError::Provider(_)));
    }

    // `FinishReason` 的 `Default` 正是为了保住 `#[serde(default)]` 的语义：
    // 缺字段 = 未识别，**不得**被当成 `Stop`（那不是「正常收尾」的证据）。
    #[test]
    fn test_finish_reason_default_is_other() {
        assert_eq!(FinishReason::default(), FinishReason::Other(String::new()));
        assert!(!FinishReason::default().is_truncated());
    }

    #[test]
    fn test_finish_reason_is_truncated() {
        assert!(FinishReason::Length.is_truncated());
        assert!(!FinishReason::Stop.is_truncated());
        assert!(!FinishReason::ToolCalls.is_truncated());
        assert!(!FinishReason::ContentFilter.is_truncated());
        assert!(!FinishReason::Other("weird".into()).is_truncated());
    }

    #[tokio::test]
    async fn test_mock_model_text_and_tool_call_finish_reasons() {
        let model = MockModel::new("test");
        model.push_text("hi");
        model.push_tool_call("bash", serde_json::json!({"command": "ls"}));
        let mut sink = MockEventSink;
        let req = ModelRequest {
            messages: vec![],
            tools: vec![],
            ..Default::default()
        };
        let text_resp = model
            .complete(req.clone(), &mut sink)
            .await
            .expect("text response");
        assert_eq!(text_resp.finish_reason, FinishReason::Stop);
        let call_resp = model
            .complete(req, &mut sink)
            .await
            .expect("tool call response");
        assert_eq!(call_resp.finish_reason, FinishReason::ToolCalls);
    }

    #[tokio::test]
    async fn test_mock_model_truncated_tool_call() {
        let model = MockModel::new("test");
        model.push_truncated_tool_call("bash", serde_json::json!({"command": "ls -l"}));
        let mut sink = MockEventSink;
        let req = ModelRequest {
            messages: vec![],
            tools: vec![],
            ..Default::default()
        };
        let resp = model.complete(req, &mut sink).await.unwrap();
        assert!(resp.finish_reason.is_truncated());
        assert_eq!(resp.finish_reason, FinishReason::Length);
        assert!(
            matches!(&resp.message.content[0], ContentBlock::ToolUse { name, .. } if name == "bash")
        );
    }

    #[tokio::test]
    async fn test_mock_model_truncated_text() {
        let model = MockModel::new("test");
        model.push_truncated_text("a long and cut off senten");
        let mut sink = MockEventSink;
        let req = ModelRequest {
            messages: vec![],
            tools: vec![],
            ..Default::default()
        };
        let resp = model.complete(req, &mut sink).await.unwrap();
        assert_eq!(resp.finish_reason, FinishReason::Length);
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "a long and cut off senten")
        );
    }

    // `ModelResponse` 的 serde 契约：`finish_reason` 字段名 + 缺省语义。
    #[test]
    fn test_model_response_finish_reason_serde() {
        let resp = ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "x".into() }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Length,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            json.contains("\"finish_reason\""),
            "字段名是对外契约: {json}"
        );
        assert!(json.contains("\"Length\""));
        let back: ModelResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.finish_reason, FinishReason::Length);

        // 缺字段 → Default → Other("")，**不是** Stop。
        let legacy = serde_json::json!({
            "message": {"role": "Assistant", "content": []},
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let back: ModelResponse = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.finish_reason, FinishReason::default());
    }

    /// N10：`boundary` **不参与序列化** ⇒ 既有序列化字节逐字节不变。
    ///
    /// 该字段带 `serde(skip)`；若误改成普通字段，JSON 里会多出 `boundary`
    /// 键（且 `Arc<dyn BoundarySource>` 无 `Serialize` ⇒ 直接编译失败）。
    /// 这里钉住的是「挂载边界源**不改变**请求的线上字节」。
    #[test]
    fn model_request_serialization_ignores_boundary() {
        let request = ModelRequest {
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            ..Default::default()
        };
        let without_boundary = serde_json::to_string(&request).unwrap();

        let with_boundary = ModelRequest {
            boundary: Some(Arc::new(ys_protocol::QueueBoundarySource::new())),
            ..request.clone()
        };
        let with_boundary = serde_json::to_string(&with_boundary).unwrap();

        assert_eq!(
            without_boundary, with_boundary,
            "挂载边界源不得改变序列化字节"
        );
        assert!(
            !with_boundary.contains("boundary"),
            "序列化结果不得含 boundary 键：{with_boundary}"
        );
    }

    /// N11：`clone` 后 `boundary` 仍是**同一份** `Arc`（`Arc::ptr_eq`）。
    ///
    /// 共享语义是取消能生效的前提：loop 侧与 model 侧必须看到**同一个**
    /// abort 状态；clone 若产生独立副本，模型侧就永远看不到 abort。
    #[test]
    fn model_request_clone_keeps_same_boundary_arc() {
        let boundary: Arc<dyn ys_protocol::BoundarySource> =
            Arc::new(ys_protocol::QueueBoundarySource::new());
        let request = ModelRequest {
            boundary: Some(boundary.clone()),
            ..Default::default()
        };
        let cloned = request.clone();
        assert!(
            cloned
                .boundary
                .as_ref()
                .is_some_and(|b| Arc::ptr_eq(b, &boundary)),
            "clone 后必须仍是同一份 Arc"
        );
    }
}
