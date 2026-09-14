//! ys-tui-coding: coding agent 的 TUI（设计 §1–§8）
//!
//! **产品专属的 UI crate** —— 与 `ys-protocol`（能力平面，所有产品共用）平级：
//!
//! ```text
//! ys-protocol          能力平面：Request / Boundary / Outbound<V>
//! ys-tui-coding        本 crate：CodingView + ratatui 渲染/输入/补全
//! apps/coding-agent    接线器：构造视图、实现 app 侧循环、驱动 agent
//! ```
//!
//! # 边界纪律（编译器强制）
//!
//! 本 crate **不认识 `Agent`**（不依赖 `ys-runtime`）。它与 app 线程之间只有三条
//! 信道 + `ys-protocol` 的纯数据类型：
//!
//! | 信道 | 类型 | 方向 |
//! |---|---|---|
//! | ① Request | [`ys_protocol::Request`] | UI → app（回合边界） |
//! | ② Boundary | [`ys_protocol::Boundary`] | UI → `BasicLoop`（轮边界） |
//! | ③ Outbound | [`ys_protocol::Outbound`]`<CodingView>` | app → UI |
//!
//! # 形态
//!
//! 三个 pane 自上而下：**Chat**（`Min(3)`，唯一可滚动）/ **Input**（`Length(1..=N)`，
//! 高度自适应）/ **Status**（`Length(1)`，**最底常驻**）。三 pane 都是固定分区，
//! 每次重绘整屏 —— 会「滚走」的只有 Chat 里的内容。
//!
//! # 线程模型
//!
//! [`run`] **同步阻塞**在自己的事件循环里，必须在**非 async 线程**调用
//! （它用 `Sender::blocking_send`）。agent 在另一个线程跑 —— 这正是拆 crate 的
//! 代价与收益：agent 不能跑在 `run()` 里，TUI 也不认识 `Agent`。

mod app;
mod commands;
mod completion;
mod draw;
mod events;
mod format;
mod input;
mod prompter;
mod run;
mod transcript;
mod view;

pub use app::App;
pub use commands::{Action, CommandSpec, LocalAction, PromptKind, all_commands, parse};
pub use completion::{CompletionItem, CompletionState, entries};
pub use input::InputBuffer;
pub use prompter::{FakePrompter, PromptError, Prompter};
pub use run::{TuiError, run};
pub use transcript::{TranscriptLine, summarize_tool_args};
pub use view::{CodingView, ProviderEntry};
