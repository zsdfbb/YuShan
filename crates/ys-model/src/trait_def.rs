use async_trait::async_trait;

use super::{ModelError, ModelEventSink, ModelRequest, ModelResponse};

/// LLM 后端的接口。
#[async_trait]
pub trait Model: Send + Sync {
    /// 返回 model 标识符（例如 "claude-sonnet-4-20250514"）。
    fn model_id(&self) -> &str;

    /// 发送 completion 请求，并把事件流式推送到 `sink`。
    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError>;
}
