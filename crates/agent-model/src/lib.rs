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
    use agent_core::*;

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
}
