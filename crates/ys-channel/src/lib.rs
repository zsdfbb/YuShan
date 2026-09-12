//! ys-channel: 核心信道契约
//!
//! 与 `ys-event` 平级的**契约层**：只装纯数据与纯枚举，**零 tokio**，运行时无关。
//! `tokio::sync::mpsc` 的实现属于接线器（`apps/coding-agent`），不进本 crate——
//! 契约不该指定传输方式。
//!
//! - [`Envelope`] / [`Source`]：事件在信道上的传输单位（谁发的、哪一轮、什么事）
//! - [`Inbox`] / [`Intent`] / [`QueueMode`]：入站消息的两队列与消费粒度
//! - [`LifecyclePolicy`]：消费者消失后的生命周期策略

mod envelope;
mod inbox;
mod lifecycle;

pub use envelope::*;
pub use inbox::*;
pub use lifecycle::*;
