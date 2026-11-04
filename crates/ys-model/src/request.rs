use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ys_core::{Message, ThinkingOverride};
use ys_protocol::BoundarySource;
use ys_tool::ToolSpec;

/// 发送给 model 以完成 completion 的请求。
///
/// **不派生 `Serialize` / `Deserialize`** —— 它是**进程内调用契约**，不是线格式：
/// 从 `ys-loop` 构造、交给 [`Model::complete`]，生命周期不超出一个回合。
/// 真正跨线的请求体是适配器内部的 `ChatCompletionRequest`（那里才需要 serde）。
///
/// 把 `boundary`（`Arc<dyn BoundarySource>`）留在本结构里也依赖这一点：
/// 它无 `Serialize`，一旦给本类型加回派生就会编译失败。
///
/// [`Model::complete`]: crate::Model::complete
#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    /// 消息历史快照。
    ///
    /// **`Arc` 是为了让 `clone()` 变 O(1)**：重试循环每轮都 `request.clone()` 一次，
    /// 而 `ModelRequest` 的 `Clone` 是派生实现 —— 若这里是 `Vec<Message>`，
    /// 每次 clone 都会深拷贝整个历史。
    ///
    /// 读侧不受影响：`Arc<Vec<T>>` 的 deref 链是 `Arc<Vec<T>> → Vec<T> → [T]`，
    /// `&request.messages` / `.iter()` / `.len()` / `[i]` 均照旧可用。
    pub messages: Arc<Vec<Message>>,
    /// 工具 schema 表。
    ///
    /// **同样是 `Arc`**：它是每轮请求的固定开销（实测 10 个工具 ≈ 9 KB、`to_vec` ≈ 42 µs），
    /// 而循环内从不修改它 —— 没有理由每轮深拷一份。
    pub tools: Arc<Vec<ToolSpec>>,
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
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
    pub thinking: Option<ThinkingOverride>,
    /// 轮边界控制源（可选）—— 模型调用期间的**中止**通道。
    ///
    /// `None` = 不支持中止（适配器不装探针，行为与不挂载时一致）。
    ///
    /// 共享引用语义：loop 侧与 model 侧看到**同一个** abort 状态
    /// （`Arc` 是 `Clone` ⇒ clone 后指向同一队列）。
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
