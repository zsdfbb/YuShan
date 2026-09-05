use async_trait::async_trait;

use super::{ModelError, ModelEventSink, ModelRequest, ModelResponse};

/// Interface for LLM backends.
#[async_trait]
pub trait Model: Send + Sync {
    /// Returns the model identifier (e.g. "claude-sonnet-4-20250514").
    fn model_id(&self) -> &str;

    /// Send a completion request and stream events to `sink`.
    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError>;
}
