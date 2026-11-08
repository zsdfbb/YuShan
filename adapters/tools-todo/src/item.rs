//! `TodoItem` / `TodoStatus` —— 清单的词汇层。零 IO、零锁、零 tokio。
//!
//! 这两个类型同时是**落盘格式**（`<stem>.todo.json`）与**工具入参格式**（`todos` 数组的元素）：
//! 一处定义、两处消费，因此字段名与状态名的任何改动都会同时改掉文件格式与模型契约。

use serde::{Deserialize, Serialize};

/// 一项任务的状态。
///
/// `Cancelled` 是**终止态**，不是「失败」：它是模型能**诚实停下**的唯一出口 ——
/// 没有它，模型想让追问环停下就得把没做完的事标成 `completed`，那是伪造完成。
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    /// 终态 = `{Completed, Cancelled}`。
    ///
    /// 「全部终态 ⇒ 清空 + 删文件」与「不得丢弃非终态项」**都只问这一个问题**：
    /// 两处判据必须同源，否则会出现「写入侧认为已关闭、读取侧认为仍打开」的裂缝。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    /// 落盘与渲染共用的 snake_case 名（与 `serde` 的 `rename_all` 逐字一致）。
    ///
    /// 渲染不走 `serde`（它是纯文本拼装），故单列一份；两边若漂移，
    /// `render_block` 与文件内容会对不上，测试会先红。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 清单里的一项。
///
/// **没有 `id` 字段**：同一性由 `content` 承担（归一化后逐字比较）。工具入参里可以带 `id`
/// （serde 默认忽略未知字段），但它不落盘、不参与渲染 —— 清单的唯一性是内容。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

impl TodoItem {
    pub fn new(content: impl Into<String>, status: TodoStatus) -> Self {
        Self {
            content: content.into(),
            status,
        }
    }
}

/// 内容归一化：去掉首尾空白、把内部空白串折叠成单个空格。
///
/// **同一性由内容承担**（见 [`TodoItem`]），故比较前必须先过这里 —— 否则「换行改空格」
/// 会被当成「删掉一项又加一项」，`apply` 的防逃逸判据就会误报。
///
/// 渲染也走它：一项恒占一行，块的行结构才可依赖。
pub fn normalize_content(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 终态集合是 `{Completed, Cancelled}`；`Pending` / `InProgress` 都不是终态。
    ///
    /// 变异：把 `Cancelled` 从 `is_terminal` 里去掉 ⇒ 「放弃某项后计划永不关闭」，
    /// `apply` 的删除分支与 `has_open_items` 双双变红。
    #[test]
    fn is_terminal_covers_completed_and_cancelled_only() {
        assert!(TodoStatus::Completed.is_terminal());
        assert!(TodoStatus::Cancelled.is_terminal());
        assert!(!TodoStatus::Pending.is_terminal());
        assert!(!TodoStatus::InProgress.is_terminal());
    }

    /// `as_str` 与 serde 的 `rename_all = "snake_case"` 逐字一致。
    ///
    /// 变异：把 `InProgress` 的字符串写成 `"in-progress"` ⇒ 本用例变红（渲染与落盘漂移）。
    #[test]
    fn as_str_matches_serde_representation() {
        for status in [
            TodoStatus::Pending,
            TodoStatus::InProgress,
            TodoStatus::Completed,
            TodoStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).expect("状态可序列化");
            assert_eq!(
                json,
                format!("\"{}\"", status.as_str()),
                "渲染名必须与落盘名一致"
            );
        }
    }

    /// 归一化只折叠空白，不改动字符本身。
    #[test]
    fn normalize_content_collapses_whitespace_only() {
        assert_eq!(normalize_content("  a\n\tb   c "), "a b c");
        assert_eq!(normalize_content(""), "");
        assert_eq!(normalize_content("中文 内容"), "中文 内容");
    }

    /// 入参里的 `id` 被忽略，`content` / `status` 是必填。
    #[test]
    fn deserialization_ignores_unknown_id() {
        let item: TodoItem =
            serde_json::from_str(r#"{"content":"a","status":"in_progress","id":"7"}"#)
                .expect("id 是未知字段，应被忽略");
        assert_eq!(item, TodoItem::new("a", TodoStatus::InProgress));

        assert!(
            serde_json::from_str::<TodoItem>(r#"{"content":"a","status":"done"}"#).is_err(),
            "未知状态必须解析失败（由调用方转成模型可见的错误）"
        );
    }
}
