//! agent-tool: 工具接口与注册表

mod approval;
mod context;
mod error;
mod registry;
mod spec;

pub use approval::{ApprovalDecision, ApprovalHandler, AutoApprove};
pub use context::*;
pub use error::*;
pub use registry::*;
pub use spec::*;

// 为方便起见从 agent-core 再导出
pub use agent_core::{ToolCallId, ToolResult};

use async_trait::async_trait;
use serde_json::Value;

/// Tool 接口 — 自描述 + 执行
#[async_trait]
pub trait Tool: Send + Sync {
    /// 返回 tool 的 spec（构建时调用一次，缓存）
    fn spec(&self) -> ToolSpec;

    /// 用给定输入执行 tool
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::CancelToken;
    use std::path::PathBuf;

    struct DummyTool {
        spec: ToolSpec,
    }

    impl DummyTool {
        fn new(name: &str) -> Self {
            Self {
                spec: ToolSpec {
                    name: name.into(),
                    description: format!("dummy tool {name}"),
                    parameters: serde_json::json!({}),
                },
            }
        }
    }

    #[async_trait]
    impl Tool for DummyTool {
        fn spec(&self) -> ToolSpec {
            self.spec.clone()
        }

        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                content: "ok".into(),
                is_error: false,
            })
        }
    }

    #[test]
    fn test_registry_build_and_lookup() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(DummyTool::new("bash")),
            Box::new(DummyTool::new("read")),
        ];
        let registry = ToolRegistry::build(tools).unwrap();
        assert!(registry.get("bash").is_some());
        assert!(registry.get("read").is_some());
        assert!(registry.get("missing").is_none());
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn test_registry_duplicate_name() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(DummyTool::new("bash")),
            Box::new(DummyTool::new("bash")),
        ];
        let err = ToolRegistry::build(tools).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn test_tool_context_cancel() {
        let token = CancelToken::new();
        let ctx = ToolContext::new(&token, PathBuf::from("."), PathBuf::from("."));
        assert!(!ctx.cancel.is_cancelled());
        token.cancel();
        assert!(ctx.cancel.is_cancelled());
    }

    #[test]
    fn test_registry_specs() {
        let tools: Vec<Box<dyn Tool>> =
            vec![Box::new(DummyTool::new("a")), Box::new(DummyTool::new("b"))];
        let registry = ToolRegistry::build(tools).unwrap();
        let specs = registry.specs();
        assert_eq!(specs.len(), 2);
    }
}
