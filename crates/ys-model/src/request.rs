use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ys_core::{Message, ThinkingOverride};
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
    /// **思考覆盖**（可选）—— 本次调用的思考开关 / 强度覆盖。
    ///
    /// `None` = 不干预，由 config 侧（`/effort`）与 provider 默认决定。
    /// `Some(ThinkingOverride::off())` = 确定关闭思考。
    ///
    /// **为什么整个字段是 `Option`**：`None`（不干预）与「`thinking: Some(true)`」
    /// 语义不同 —— 前者是「随既有决定」，后者是「确定要开」。若用非 `Option` 结构，
    /// `Default` 就会让**所有既有调用**主动表态，那是一次静默的行为变更。
    ///
    /// **为何不用 `reasoning_effort = "off"`**：`off` 不是 DeepSeek 的合法 wire 值
    /// （实测返回 422 `unknown variant 'off'`），要关思考只能发
    /// `thinking: {"type": "disabled"}`。
    ///
    /// **不参与序列化**：与 [`Self::boundary`] 同理，`ModelRequest` 是会话契约的一部分，
    /// 加值字段会改字节。（此字段曾是 `disable_thinking: Option<bool>`，泛化时保持了
    /// 同一条 `serde(skip)` 契约 ⇒ 序列化字节逐字节不变。）
    #[serde(skip, default)]
    pub thinking: Option<ThinkingOverride>,
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
