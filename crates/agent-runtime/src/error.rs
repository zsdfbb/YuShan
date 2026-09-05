#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("model not provided")]
    MissingModel,
    #[error("session not provided")]
    MissingSession,
    #[error("events sink not provided")]
    MissingEvents,
    #[error("tool registry error: {0}")]
    ToolRegistry(String),
}
