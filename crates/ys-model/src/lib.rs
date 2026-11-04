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
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
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
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
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
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
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
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
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
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
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
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
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

    /// N10：`boundary` 是进程内取消通道，不是数据字段。
    ///
    /// 守护方式是**类型层**而非运行期断言：`ModelRequest` 不派生 `Serialize`
    /// （线格式在适配器的 `ChatCompletionRequest`），故「把 boundary 序列化出去」
    /// 不可表达。若有人给 `ModelRequest` 加回 `Serialize`，则 `boundary` 字段
    /// （`Arc<dyn BoundarySource>` 无 `Serialize`）会让编译失败 —— 那正是需要
    /// 重新审视「请求体是否已变成线格式契约」的时刻。
    /// N11 仍钉住 clone 的共享语义（见下一条）。

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

    /// 重试循环每轮都 `request.clone()`：`messages` 必须是共享 `Arc`，不能每次深拷贝。
    ///
    /// 变异：把 `messages` 改回 `Vec<Message>` ⇒ 本用例变红。
    #[test]
    fn model_request_clone_shares_messages_arc() {
        let messages = Arc::new(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }]);
        let request = ModelRequest {
            messages: messages.clone(),
            ..Default::default()
        };
        let cloned = request.clone();
        assert!(
            Arc::ptr_eq(&cloned.messages, &request.messages),
            "clone 后必须共享同一份 messages，否则每次模型调用都会深拷贝整个历史"
        );
    }
}
