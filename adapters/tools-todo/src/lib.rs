//! `ys-tools-todo` —— 长程任务的**计划清单**：唯一真值 + 落盘 + 一个工具。
//!
//! # 分工
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`item`] | `TodoItem` / `TodoStatus`（含终止态 `Cancelled`）—— 落盘格式与工具入参**共用**一套词汇 |
//! | [`store`] | [`TodoStore`]：内存真值 + `<stem>.todo.json` 落盘 + 两条硬规则 |
//! | [`render`] | [`render_block`]：纯函数、**字节稳定**的注入块 |
//! | [`tool`] | [`TodoTool`]：模型唯一能写清单的入口 |
//!
//! # 三件事各自归谁
//!
//! - **状态**：本 crate（`TodoStore` 是唯一真值，落盘在会话同级 —— 压缩够不着）
//! - **可见性**：不在这里。回合边界把 [`render_block`] 的结果作为一条 `Role::Harness`
//!   消息追加到 `messages` 尾部（出站时映射成 user，但本地记录不归用户），
//!   判据是「状态变了 ∨ 会话里找不到标记」
//! - **推进**：不在这里。回合结束后由 follow-up 纯函数问一次 [`TodoStore::has_open_items`]
//!
//! 本 crate 因此**不认识** `ys-loop` / `ys-runtime` / `ys-component` / `ys-protocol`：
//! 它只提供状态与工具，控制流留在 app 层。
//!
//! # 两条硬规则在 [`TodoStore::apply`] 里，不靠模型自觉
//!
//! 1. **拒绝丢弃既有非终态项** —— 否则模型发一个 `[]` 就关掉了整个闭环；
//! 2. **无 open 项 ⇒ 清空 + 删文件** —— 判据是「无 open 项」而非「全部 `Completed`」，
//!    后者会让被放弃的项永远卡住。
//!
//! # 本 crate 绝不 `print`
//!
//! 降级提示由 [`TodoStore::take_degraded_hint`] **产出**，去处由接线器决定。

pub mod item;
pub mod render;
pub mod store;
pub mod tool;

pub use item::{TodoItem, TodoStatus, normalize_content};
pub use render::{MAX_BLOCK_BYTES, MAX_BLOCK_TOKENS, MAX_CONTENT_CHARS, render_block};
pub use store::{ApplyOutcome, TODO_FILE_SUFFIX, TodoError, TodoStore};
pub use tool::TodoTool;

use std::sync::Arc;

use ys_tool::Tool;

/// 装配入口：把 store 交给工具，返回可登记的 `Box<dyn Tool>`。
///
/// 工具持的是**同一个 `Arc`** —— `/new` 时接线器 `retarget` 它，身份不变，工具不会用到过期快照。
pub fn todo_tool(store: Arc<TodoStore>) -> Box<dyn Tool> {
    Box::new(TodoTool::new(store))
}

/// 测试专用助手（`#[cfg(test)]`，不进生产二进制）。
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use ys_tool::ToolContext;

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全的既有样板）。
    ///
    /// `SystemTime::as_nanos()` 在 macOS 只到微秒级，单靠它会撞名 ⇒ 并行测试互删文件。
    pub fn test_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_tools_todo_test_{tag}_{}_{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建测试临时目录");
        dir
    }

    /// `todo` 工具不看 `cwd` / `workspace_root` / `boundary` ⇒ 全空 ctx 即可。
    pub fn ctx() -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
    }
}
