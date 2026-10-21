use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ys_core::Message;
use ys_protocol::BoundarySource;
use ys_tool::ToolSpec;

/// 发送给 model 以完成 completion 的请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 轮边界控制源（可选）—— 模型调用期间的**中止**通道。
    ///
    /// `None` = 不支持中止（适配器不装探针，行为与不挂载时一致）。
    ///
    /// **不参与序列化**（`serde(skip)`）⇒ 既有序列化字节逐字节不变。
    ///
    /// 共享引用语义：loop 侧与 model 侧看到**同一个** abort 状态
    /// （`Arc` 是 `Clone` ⇒ clone 后指向同一队列）。
    #[serde(skip, default)]
    pub boundary: Option<Arc<dyn BoundarySource>>,
}

/// provider 的 finish_reason **归一化后**的形态。
///
/// 归一化在适配器完成（provider 方言 → 本类型），故 ys-loop 不必认识
/// 任何 provider 的词汇。字段名与 `ys_core::StopReason` 刻意区分开。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    /// 正常收尾
    Stop,
    /// 有工具调用
    ToolCalls,
    /// 输出预算耗尽 —— **截断**
    Length,
    ContentFilter,
    /// 未识别 / provider 未给
    Other(String),
}

impl FinishReason {
    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::Length)
    }
}

impl Default for FinishReason {
    fn default() -> Self {
        Self::Other(String::new())
    }
}

/// 从 model 收到的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: ys_core::Usage,
    #[serde(default)]
    pub finish_reason: FinishReason,
}
