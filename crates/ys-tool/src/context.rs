use std::path::PathBuf;
use ys_protocol::BoundarySource;

use crate::summarizer::Summarizer;

#[non_exhaustive]
pub struct ToolContext<'a> {
    /// 轮边界控制源（工具可轮询 `is_aborted()` 以感知中止）。
    ///
    /// `None` = 无边界控制；工具应视作「未中止」。
    pub boundary: Option<&'a dyn BoundarySource>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    /// 一次性摘要能力（可选）。
    ///
    /// `None` = 未挂载；工具应视作「本回合不支持摘要」。由上层经 [`ToolContext::with_summarizer`] 挂载。
    pub summarizer: Option<&'a dyn Summarizer>,
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
            summarizer: None,
        }
    }

    /// 链式挂载摘要能力（照 `RuntimeContext::with_boundary` 的先例）。不挂载时行为逐字节不变。
    pub fn with_summarizer(mut self, summarizer: &'a dyn Summarizer) -> Self {
        self.summarizer = Some(summarizer);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::Path;

    /// 最简假摘要器 —— 只为证明挂载点可用，不做真实摘要。
    struct FakeSummarizer;

    #[async_trait]
    impl Summarizer for FakeSummarizer {
        async fn summarize(&self, _content: &str, _prompt: &str) -> Result<String, String> {
            Ok("摘要".to_string())
        }
    }

    #[test]
    fn test_new_has_no_summarizer_by_default() {
        let ctx = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."));
        assert!(ctx.summarizer.is_none(), "未挂载时 summarizer 应为 None");
    }

    #[test]
    fn test_with_summarizer_mounts_and_leaves_other_fields_intact() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let ctx = ToolContext::new(
            Some(&boundary),
            PathBuf::from("有个 cwd"),
            PathBuf::from("有个 workspace_root"),
        )
        .with_summarizer(&FakeSummarizer);

        // 挂载生效
        assert!(ctx.summarizer.is_some(), "with_summarizer 后应为 Some");

        // 其余三个字段不受影响
        let mounted = ctx.boundary.expect("boundary 应保持原值 Some");
        assert!(!mounted.is_aborted(), "boundary 应仍是原来那个源");
        assert_eq!(ctx.cwd, Path::new("有个 cwd"));
        assert_eq!(ctx.workspace_root, Path::new("有个 workspace_root"));
    }
}
