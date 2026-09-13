//! ys-runtime: 组装 facade，锁定外部导入路径

mod agent;
mod builder;
mod error;
pub mod prelude;

pub use agent::*;
pub use builder::*;
pub use error::*;

#[cfg(test)]
mod tests {
    use super::prelude::*;
    use ys_channel::{Inbox, Intent};
    use ys_event::CollectingSink;
    use ys_model::MockModel;
    use ys_session::{MemorySession, Session};

    struct EchoTool;

    #[async_trait::async_trait]
    impl ys_tool::Tool for EchoTool {
        fn spec(&self) -> ys_tool::ToolSpec {
            ys_tool::ToolSpec::new("echo", "echoes input", serde_json::json!({}))
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: ys_tool::ToolContext<'_>,
        ) -> Result<ys_tool::ToolResult, ys_tool::ToolError> {
            Ok(ys_tool::ToolResult {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    #[tokio::test]
    async fn test_agent_builder_and_run() {
        let model = MockModel::new("test");
        model.push_text("hello");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let mut agent = AgentBuilder::new().build().unwrap();
        let result = agent
            .run_turn(
                AgentInput::text("hi"),
                AgentPorts::new(Some(&model), &mut session, &mut events),
            )
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Completed);
    }

    /// 空 builder（无工具、无 model/session/events 端口）能成功构建。
    ///
    /// ADR-0010 后 `build` 不再要求 session/events/model —— 它们在 `run` 时经
    /// [`AgentPorts`] 传入。本测试锁住「空 builder 构建」这一路径本身。
    #[test]
    fn test_empty_builder_builds_successfully() {
        let agent = AgentBuilder::new().build().expect("空 builder 应能构建");
        assert!(agent.tool_names().is_empty(), "默认不注册工具");
    }

    #[tokio::test]
    async fn test_run_without_model_fails() {
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();
        let mut agent = AgentBuilder::new().build().unwrap();

        let result = agent
            .run_turn(
                AgentInput::text("hi"),
                AgentPorts::new(None, &mut session, &mut events),
            )
            .await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("No model configured"),
            "error should mention no model: {msg}"
        );
    }

    #[tokio::test]
    async fn test_agent_run_with_limits() {
        let model = MockModel::new("test");
        // 配置将被执行的 tool call（已注册 echo 工具）
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_tool_call("echo", serde_json::json!({}));
        model.push_text("done");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let mut agent = AgentBuilder::new()
            .tool(EchoTool)
            .limits(RunLimits::new(2))
            .build()
            .unwrap();
        let result = agent
            .run_turn(
                AgentInput::text("go"),
                AgentPorts::new(Some(&model), &mut session, &mut events),
            )
            .await
            .unwrap();
        // 两轮后应命中最大 round 数
        assert_eq!(result.stop_reason, StopReason::MaxRounds);
        assert_eq!(result.rounds, 2);
    }

    #[tokio::test]
    async fn test_agent_cancel_token() {
        let model = MockModel::new("test");
        let cancel = CancelToken::new();
        cancel.cancel(); // 运行前取消
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let mut agent = AgentBuilder::new()
            .cancel_token(cancel.clone())
            .build()
            .unwrap();
        let result = agent
            .run_turn(
                AgentInput::text("go"),
                AgentPorts::new(Some(&model), &mut session, &mut events),
            )
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Cancelled);
    }

    /// **agent 无状态**：同一 `Agent` 实例配不同 [`AgentPorts`] 能分别跑，
    /// 证明它不持有会话（也不持有模型）——「当前是哪个会话」由端口决定。
    #[tokio::test]
    async fn same_agent_instance_runs_with_different_ports() {
        let m1 = MockModel::new("m1");
        m1.push_text("one");
        let m2 = MockModel::new("m2");
        m2.push_text("two");

        let mut s1 = MemorySession::new();
        let mut e1 = CollectingSink::new();
        let mut s2 = MemorySession::new();
        let mut e2 = CollectingSink::new();

        let mut agent = AgentBuilder::new().build().unwrap();

        agent
            .run_turn(
                AgentInput::text("a"),
                AgentPorts::new(Some(&m1), &mut s1, &mut e1),
            )
            .await
            .unwrap();
        agent
            .run_turn(
                AgentInput::text("b"),
                AgentPorts::new(Some(&m2), &mut s2, &mut e2),
            )
            .await
            .unwrap();

        // 每个会话各自拿到 user + assistant 两条，互不串。
        assert_eq!(s1.messages().len(), 2, "会话 1 应独立记录");
        assert_eq!(s2.messages().len(), 2, "会话 2 应独立记录");
    }

    /// `RunSummary` 语义不变：`turns` 计回合、`last_stop` 取末回合、`usage` 累计。
    #[tokio::test]
    async fn run_summary_semantics_unchanged() {
        let model = MockModel::new("test");
        model.push_text("r1");
        model.push_text("r2");
        let mut session = MemorySession::new();
        let mut events = CollectingSink::new();

        let mut agent = AgentBuilder::new().build().unwrap();
        let inbox = Inbox::new();
        inbox.push(AgentInput::text("a").message, Intent::FollowUp);
        inbox.push(AgentInput::text("b").message, Intent::FollowUp);

        let summary = agent
            .run(
                AgentPorts::new(Some(&model), &mut session, &mut events),
                &inbox,
            )
            .await
            .unwrap();

        assert_eq!(summary.turns, 2, "两条 followUp → 两个回合");
        assert_eq!(summary.last_stop, Some(StopReason::Completed));
        assert_eq!(summary.last_rounds, 1);
        assert!(summary.last_message.is_some());
    }
}
