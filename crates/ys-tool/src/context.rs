use std::path::PathBuf;
use ys_core::CancelToken;

#[non_exhaustive]
pub struct ToolContext<'a> {
    pub cancel: &'a CancelToken,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
}

impl<'a> ToolContext<'a> {
    pub fn new(cancel: &'a CancelToken, cwd: PathBuf, workspace_root: PathBuf) -> Self {
        Self {
            cancel,
            cwd,
            workspace_root,
        }
    }
}
