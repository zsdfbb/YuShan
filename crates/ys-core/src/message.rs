use serde::{Deserialize, Serialize};

use crate::ToolCallId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    /// harness 产生、**用户没有说过**的输入：后台命令完成的结果、子 agent 的结果。
    ///
    /// 与 [`Role::User`] 的区别**只在本地**：落盘、回放、统计都据它区分来源，
    /// 模型看到的仍旧是 user 角色（适配器在出站时映射）。
    Harness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        content: String,
        is_error: bool,
    },
}
