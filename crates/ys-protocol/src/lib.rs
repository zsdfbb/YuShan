//! ys-protocol: 能力平面（协议）
//!
//! 多产品共用的**协议层**：只装纯数据与纯 trait，**零 tokio**（生产与 dev
//! 依赖都不含），故可被任意运行时、任意产品 TUI crate 引用。
//!
//! 三层角色：
//!
//! ```text
//! ys-protocol          能力平面：Request / Boundary / Outbound<V>   ← 所有产品共用
//! ys-tui-coding        coding agent 的 TUI（自己定义 CodingView）
//! ys-tui-invest        投资 agent 的 TUI（将来）
//! apps/coding-agent    接线器：构造视图、实现 app 侧循环、驱动 agent
//! ```
//!
//! 本 crate 的公开面：
//!
//! - [`Request`] / [`Outbound`]：UI ↔ app 的请求与出站消息
//! - [`Boundary`] / [`BoundarySource`] / [`QueueBoundarySource`]：轮边界控制
//!   （`Steer` 插话 + `Abort` 取消），取代旧的 `CancelToken`
//! - [`Envelope`] / [`Source`]：事件在信道上的传输单位（谁发的、哪一轮、什么事）
//! - [`LifecyclePolicy`]：消费者消失后的生命周期策略
//!
//! **信道拓扑（设计 §8）**：① `Request`（回合边界）/ ② `Boundary`（轮边界）/
//! ③ `Outbound<V>`（app → UI）。② 必须独立于 ① —— 回合跑动时 app 线程阻塞在
//! `agent.run()` 里，不可能 `recv` ①，而 `Steer` / `Abort` 要**中途**被看到。

mod boundary;
mod envelope;
mod lifecycle;
mod request;

pub use boundary::*;
pub use envelope::*;
pub use lifecycle::*;
pub use request::*;
