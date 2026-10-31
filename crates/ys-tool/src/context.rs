use std::path::PathBuf;
use std::time::Duration;
use ys_protocol::BoundarySource;

use crate::subagent::SubagentRunner;
use crate::summarizer::Summarizer;

#[non_exhaustive]
pub struct ToolContext<'a> {
    /// 轮边界控制源（工具可轮询 `is_aborted()` 感知中止）。
    ///
    /// `boundary.has_pending_steer()` 是**可选**能力，当前生产代码无消费者 ——
    /// 工具都跑到自然结束，用户插话在 `BasicLoop` 的轮边界被消费。
    ///
    /// `None` = 无边界控制；工具应视作「未中止」。
    pub boundary: Option<&'a dyn BoundarySource>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    /// 一次性摘要能力（可选）。
    ///
    /// `None` = 未挂载；工具应视作「本回合不支持摘要」。由上层经 [`ToolContext::with_summarizer`] 挂载。
    pub summarizer: Option<&'a dyn Summarizer>,
    /// 一次性派发能力（可选）。
    ///
    /// `None` = 未挂载；派发工具应视作「本回合不支持派发」。由上层经
    /// [`ToolContext::with_subagent`] 挂载。**这是第 3 个同形挂载点**
    /// （先例：`boundary`、`summarizer`）。
    pub subagent: Option<&'a dyn SubagentRunner>,
    /// 本次调用的**外层工具预算**（`ys-loop` 的 `tool_budget(tool, limits)` 的结果）。
    ///
    /// `None` = 上层未声明预算 ⇒ 工具按自己的语义兜底。
    /// **只读**（`Option<Duration>` 是 `Copy`）⇒ 与 `boundary` / `summarizer` / `subagent`
    /// 三个既有可选挂载点同形；**不进 `ToolSpec`** ⇒ 工具表指纹与父表前缀都不受影响。
    /// **这是第 4 个可选挂载点**（编号按「可选挂载点」计）。
    pub tool_budget: Option<Duration>,
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
            subagent: None,
            tool_budget: None,
        }
    }

    /// 链式挂载摘要能力（照 `RuntimeContext::with_boundary` 的先例）。不挂载时行为逐字节不变。
    pub fn with_summarizer(mut self, summarizer: &'a dyn Summarizer) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// 链式挂载派发能力（照 `with_summarizer` 的先例）。不挂载时行为逐字节不变。
    pub fn with_subagent(mut self, subagent: &'a dyn SubagentRunner) -> Self {
        self.subagent = Some(subagent);
        self
    }

    /// 链式挂载本次调用的**外层工具预算**（照 `with_summarizer` / `with_subagent` 的先例）。
    /// 不挂载时行为逐字节不变。
    ///
    /// 预算**每回合刷新**（上层每回合重算 limits）⇒ 由上层在每次构造 `ToolContext` 时挂载，
    /// 不在工具里缓存。
    pub fn with_tool_budget(mut self, budget: Duration) -> Self {
        self.tool_budget = Some(budget);
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

    /// 极简假派发器 —— 只为证明挂载点可用，不做真实派发。
    struct FakeRunner;

    #[async_trait]
    impl SubagentRunner for FakeRunner {
        async fn run(
            &self,
            _prompt: &str,
            _model: Option<&str>,
            _mode: crate::subagent::SubagentMode,
            _boundary: Option<&dyn ys_protocol::BoundarySource>,
        ) -> Result<crate::subagent::SubagentOutcome, String> {
            unimplemented!()
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

    /// 派发能力是第 3 个同形挂载点：默认 `None`，`with_subagent` 后为 `Some`。
    #[test]
    fn test_with_subagent_mounts() {
        let plain = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."));
        assert!(plain.subagent.is_none(), "未挂载时 subagent 应为 None");

        let mounted = plain.with_subagent(&FakeRunner);
        assert!(mounted.subagent.is_some(), "with_subagent 后应为 Some");
    }

    /// 外层工具预算是第 4 个可选挂载点：默认 `None`，`with_tool_budget` 后为 `Some`。
    #[test]
    fn test_new_has_no_tool_budget_by_default() {
        let ctx = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."));
        assert!(ctx.tool_budget.is_none(), "未挂载时 tool_budget 应为 None");
    }

    #[test]
    fn test_with_tool_budget_mounts_and_leaves_other_fields_intact() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let summarizer = FakeSummarizer;
        let runner = FakeRunner;

        let ctx = ToolContext::new(
            Some(&boundary),
            PathBuf::from("有个 cwd"),
            PathBuf::from("有个 workspace_root"),
        )
        .with_summarizer(&summarizer)
        .with_subagent(&runner)
        .with_tool_budget(Duration::from_secs(295));

        // 挂载生效
        assert_eq!(
            ctx.tool_budget,
            Some(Duration::from_secs(295)),
            "with_tool_budget 后应为 Some(295s)"
        );

        // 其余挂载点与路径字段均不受影响
        let mounted_boundary = ctx.boundary.expect("boundary 应保持原值 Some");
        assert!(!mounted_boundary.is_aborted(), "boundary 应仍是原来那个源");
        assert_eq!(ctx.cwd, Path::new("有个 cwd"));
        assert_eq!(ctx.workspace_root, Path::new("有个 workspace_root"));
        let mounted_summarizer = ctx.summarizer.expect("summarizer 应保持原值 Some");
        assert!(
            std::ptr::eq(
                mounted_summarizer as *const dyn Summarizer,
                &summarizer as *const dyn Summarizer
            ),
            "summarizer 应仍是原来那个实例"
        );
        let mounted_runner = ctx.subagent.expect("subagent 应保持原值 Some");
        assert!(
            std::ptr::eq(
                mounted_runner as *const dyn SubagentRunner,
                &runner as *const dyn SubagentRunner
            ),
            "subagent 应仍是原来那个实例"
        );
    }
}
