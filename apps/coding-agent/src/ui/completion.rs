#![cfg(feature = "tui-ratatui")]
//! 补全弹窗（completion popup）扩展点。
//!
//! v0 简化：completion popup 复用 events.rs 的 inline 实现（`complete_inline`）。
//! 此文件作为扩展点预留——未来 popup 用 ratatui List widget 渲染时可放这里。
//!
//! 当前保留原因：保持 ui/ 模块子结构完整（4 个子模块一致），便于后续在不
//! 改 mod 树的情况下加入 popup rendering / filtering / preview 等逻辑。
