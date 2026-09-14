use std::path::PathBuf;
use ys_protocol::BoundarySource;

#[non_exhaustive]
pub struct ToolContext<'a> {
    /// 轮边界控制源（工具可轮询 `is_aborted()` 以感知中止）。
    ///
    /// `None` = 无边界控制；工具应视作「未中止」。
    pub boundary: Option<&'a dyn BoundarySource>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
}

impl<'a> ToolContext<'a> {
    pub fn new(
        boundary: Option<&'a dyn BoundarySource>,
        cwd: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            boundary,
            cwd,
            workspace_root,
        }
    }
}
