#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("tool registry error: {0}")]
    ToolRegistry(String),
}
