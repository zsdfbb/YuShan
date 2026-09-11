use async_trait::async_trait;

/// 审批决策
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// 允许执行
    Approved,
    /// 拒绝执行，附原因
    Denied { reason: String },
}

/// Tool 审批接口（async，支持 TUI 交互）
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    /// 判断某个 tool call 是否需要审批
    fn needs_approval(&self, tool_name: &str, input: &serde_json::Value) -> bool;
    /// 请求用户审批
    async fn request_approval(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ApprovalDecision;
}

/// 默认的 no-op 审批器 —— 放行全部操作
pub struct AutoApprove;

#[async_trait]
impl ApprovalHandler for AutoApprove {
    fn needs_approval(&self, _tool_name: &str, _input: &serde_json::Value) -> bool {
        false
    }
    async fn request_approval(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> ApprovalDecision {
        ApprovalDecision::Approved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_approve_needs_approval() {
        let approve = AutoApprove;
        assert!(!approve.needs_approval("bash", &serde_json::json!({})));
        assert!(!approve.needs_approval("any_tool", &serde_json::json!({"arg": 1})));
    }

    #[tokio::test]
    async fn test_auto_approve_always_approves() {
        let approve = AutoApprove;
        let result = approve
            .request_approval("bash", &serde_json::json!({}))
            .await;
        assert_eq!(result, ApprovalDecision::Approved);
    }
}
