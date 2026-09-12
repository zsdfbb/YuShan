#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("session not provided")]
    MissingSession,
    #[error("events sink not provided")]
    MissingEvents,
    #[error("tool registry error: {0}")]
    ToolRegistry(String),
}
