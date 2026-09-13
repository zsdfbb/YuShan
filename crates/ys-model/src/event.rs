use serde::{Deserialize, Serialize};
use ys_core::EventError;

/// model 在 streaming 期间发出的事件。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    /// 思考内容增量（DeepSeek 等 provider 的 `reasoning_content`）。
    ///
    /// 仅当适配器声明支持该字段（`ProviderCompat::has_reasoning_content`）时才会发出。
    /// 工具参数增量**不在此枚举内**——它只在适配器内部组装，见 `ModelEvent` 的
    /// 「工具参数不冒泡」约定。
    ThinkingDelta {
        text: String,
    },
}

/// 面向 model 级事件的窄化 event sink。
pub trait ModelEventSink: Send {
    fn emit(&mut self, event: ModelEvent) -> Result<(), EventError>;
}
