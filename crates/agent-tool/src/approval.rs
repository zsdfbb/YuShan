use async_trait::async_trait;

/// Approval decision
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow execution
    Approved,
    /// Deny execution, with reason
    Denied { reason: String },
}

/// Tool approval interface (async, supports TUI interaction)
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    /// Determine if a tool call requires approval
    fn needs_approval(&self, tool_name: &str, input: &serde_json::Value) -> bool;
    /// Request user approval
    async fn request_approval(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ApprovalDecision;
}

/// Default no-op approver -- allows all operations
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
