//! ys-tool: 工具接口与注册表

mod approval;
mod context;
mod error;
mod notice;
mod registry;
mod spec;
mod subagent;
mod summarizer;
mod truncate;

pub use approval::{ApprovalDecision, ApprovalHandler, AutoApprove};
pub use context::*;
pub use error::*;
pub use notice::*;
pub use registry::*;
pub use spec::*;
pub use subagent::*;
pub use summarizer::*;
pub use truncate::*;

// 为方便起见从 ys-core 再导出
pub use ys_core::{ToolCallId, ToolResult};

use async_trait::async_trait;
use serde_json::Value;

/// Tool 接口 — 自描述 + 执行
#[async_trait]
pub trait Tool: Send + Sync {
    /// 返回 tool 的 spec（构建时调用一次，缓存）
    fn spec(&self) -> ToolSpec;

    /// 本工具单次 `call` 的墙钟上限。`None` = 未声明。
    ///
    /// `None` 时由循环回落到 `RunLimits::bash_timeout`（再回落 300s）—— 默认实现
    /// 保证既有工具行为逐字节不变。**不进 `ToolSpec`**，故不改 prompt 前缀。
    ///
    /// 语义：工具**声明值优先于** `bash_timeout`（不取 min）。`bash_timeout` 的语义是
    /// 「shell 命令多久算卡死」，取 min 会让「把 bash 限到 60s」顺手砍掉长任务派发。
    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }

    /// 本工具是否可在**同一批**里与其他工具并发执行。默认 `false`。
    ///
    /// 循环只在「批次 > 1 **且每一个**工具都为 `true`」时才走并行 —— 默认 `false`
    /// 保证既有的一切批次仍走原串行路径（行为逐字节不变）。**不进 `ToolSpec`**。
    ///
    /// 准入规则：可证明对共享状态无副作用才准开（只读工具可以；`bash`/`write`/`edit`
    /// 永远不行）。
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// 用给定输入执行 tool
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use ys_protocol::{Boundary, QueueBoundarySource};

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
    fn test_tool_context_exposes_boundary_abort_probe() {
        let boundary = QueueBoundarySource::new();
        let ctx = ToolContext::new(Some(&boundary), PathBuf::from("."), PathBuf::from("."));
        assert!(!ctx.boundary.expect("boundary 已挂载").is_aborted());
        boundary.push(Boundary::Abort);
        assert!(
            ctx.boundary.expect("boundary 已挂载").is_aborted(),
            "Abort 入队后工具应能经 boundary 探针看到"
        );
    }

    #[test]
    fn test_tool_context_without_boundary() {
        let ctx = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."));
        assert!(ctx.boundary.is_none(), "无边界控制时 boundary 为 None");
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
