//! 输入历史 —— **会话内**（UI 进程内）的「敲过什么」记忆，供 `↑` / `↓` 取回。
//!
//! # 范围
//!
//! 只活在**进程内**：不落盘、不跨会话、不跨 `--resume`。记录的是用户**提交过的原文**
//! （含 `/` 命令，多行原文原样保存）。`/new` / `/resume` 都**不清空**它 —— 它记的是
//! 「你敲过什么」，不是会话内容，清掉只会让用户重敲。
//!
//! # 浏览态与草稿
//!
//! 取历史是**有状态**的：第一次 `↑` 先把输入框里的现有内容存成草稿，再翻到最新一条；
//! 之后 `↑` 一路往旧走，`↓` 往新走，**越过最新一条就还原草稿并退出浏览态**（readline
//! 语义）。没有浏览态时 `↓` 不归这里 —— 那是聊天滚动。
//!
//! 提交（[`InputHistory::record`]）会退出浏览态并清掉草稿：那一份草稿已经被用户
//! 改写成提交内容了。

use std::collections::VecDeque;

/// 保留条目上限：超出丢**最旧**的（`VecDeque` 头部）。
///
/// 200 条足以覆盖一次会话里的翻找；再多的价值低于它占的内存与「翻不到头」的困惑。
const MAX_ENTRIES: usize = 200;

/// 会话内输入历史 + 浏览游标。
#[derive(Debug, Default)]
pub struct InputHistory {
    /// 旧的在前、新的在后（`back()` 是最近一次提交）。
    entries: VecDeque<String>,
    /// 正在浏览的条目下标；`None` = 没在浏览（输入框归用户自己）。
    cursor: Option<usize>,
    /// 进入浏览态时输入框里的原文 —— 退出浏览态时还回去。
    draft: String,
}

impl InputHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 条目数（测试与「是否值得翻」的判据）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 是否正在浏览（`↑` 翻进来之后、`↓` 翻出去之前）。
    pub fn is_browsing(&self) -> bool {
        self.cursor.is_some()
    }

    /// 记一条**已提交**的原文。
    ///
    /// 与上一条**逐字相同**的不重复入栈（连按两次 Enter 只留一条）；空白串由调用方
    /// 过滤（[`crate::events`] 的 `submit` 只在非空白时才走到这里）。
    pub fn record(&mut self, text: &str) {
        self.reset_nav();
        if self.entries.back().is_some_and(|last| last == text) {
            return;
        }
        self.entries.push_back(text.to_string());
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// `↑` 取**更旧**的一条。返回 `Some` 表示输入框应换成它；`None` = 不动输入框。
    ///
    /// - 不在浏览态 ⇒ 先存草稿（`current`），返回**最新**一条。
    /// - 已在**最旧**一条 ⇒ 停在原地（仍返回它，按键不生效但不报错）。
    pub fn prev(&mut self, current: &str) -> Option<String> {
        let last = self.entries.len().checked_sub(1)?;
        let index = match self.cursor {
            None => {
                self.draft = current.to_string();
                last
            }
            // 已是第一条：原地不动
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.cursor = Some(index);
        self.entries.get(index).cloned()
    }

    /// `↓` 取**更新**的一条；越过最新一条时还原草稿并退出浏览态。
    ///
    /// 不在浏览态 ⇒ `None`（`↓` 归聊天滚动，不由本类型解释）。
    pub fn next_entry(&mut self) -> Option<String> {
        let index = self.cursor?;
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            return self.entries.get(index + 1).cloned();
        }
        // 最新一条再往下 ⇒ 回到用户原本在写的东西。
        // **先取出草稿再复位** —— `reset_nav()` 会清掉 `draft`，顺序反过来就永远还空串。
        let draft = std::mem::take(&mut self.draft);
        self.reset_nav();
        Some(draft)
    }

    /// 退出浏览态并丢弃草稿。
    ///
    /// 提交与「回放替换会话」都会调用：前者说明草稿已被改写，后者说明输入框已换归属，
    /// 留着游标只会指向一份不再对得上的上下文。
    pub fn reset_nav(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_history_has_nothing_to_recall() {
        let mut h = InputHistory::new();
        assert_eq!(h.prev("draft"), None, "空历史：↑ 不动输入框");
        assert_eq!(h.next_entry(), None, "空历史：↓ 不归本类型");
        assert!(!h.is_browsing());
    }

    /// 第一次 `↑` 存草稿并给最新一条；连按 `↑` 往旧走，到头停住。
    #[test]
    fn test_prev_walks_backward_and_stops_at_oldest() {
        let mut h = InputHistory::new();
        h.record("one");
        h.record("two");
        h.record("three");

        assert_eq!(h.prev("draft").as_deref(), Some("three"));
        assert_eq!(h.prev("three").as_deref(), Some("two"));
        assert_eq!(h.prev("two").as_deref(), Some("one"));
        assert_eq!(h.prev("one").as_deref(), Some("one"), "最旧一条停住");
        assert!(h.is_browsing());
    }

    /// `↓` 往新走，越过最新一条 ⇒ 还原**进入浏览前**的草稿并退出浏览态。
    ///
    /// 草稿的来源也在这里钉住：输入框**非空**时首次 `prev` 就把它完整存下（含多行与
    /// CJK，不按行切），所以「边写边翻历史」不会丢已敲的内容。
    ///
    /// 变异：① `prev` 的 `None` 分支不存草稿 ⇒ `draft` 断言红；
    /// ② `next_entry` 直接返回空串而不还草稿 ⇒ 草稿断言红。
    #[test]
    fn test_next_restores_draft_and_leaves_browsing() {
        let mut h = InputHistory::new();
        h.record("one");
        h.record("two");

        let draft = "写到一半的话\n第二行";
        assert_eq!(h.prev(draft).as_deref(), Some("two"));
        assert_eq!(h.draft, draft, "首次 prev 即把输入框现有内容存成草稿");
        assert_eq!(h.prev("two").as_deref(), Some("one"));
        assert_eq!(h.next_entry().as_deref(), Some("two"));
        assert_eq!(
            h.next_entry().as_deref(),
            Some(draft),
            "越过最新返回草稿（原样多行）"
        );
        assert!(!h.is_browsing());
        assert!(h.draft.is_empty(), "退出浏览态即丢弃草稿");
        assert_eq!(h.next_entry(), None, "退出浏览态后 ↓ 归聊天滚动");
    }

    /// 历史只有一条时，`prev` 拿它、`next_entry` 直接还草稿 —— 不返回空串。
    ///
    /// 变异：`next_entry` 越过最新时返回空串 ⇒ 草稿断言红。
    #[test]
    fn test_single_entry_browse_roundtrip_keeps_draft() {
        let mut h = InputHistory::new();
        h.record("唯一一条");

        assert_eq!(h.prev("没写完").as_deref(), Some("唯一一条"));
        assert_eq!(h.prev("唯一一条").as_deref(), Some("唯一一条"), "到头停住");
        assert_eq!(h.next_entry().as_deref(), Some("没写完"));
        assert!(!h.is_browsing());
    }

    /// 相邻重复不入栈（连按 Enter 只留一条），中间夹别的仍会入栈。
    #[test]
    fn test_adjacent_duplicates_are_skipped() {
        let mut h = InputHistory::new();
        h.record("same");
        h.record("same");
        assert_eq!(h.len(), 1);
        h.record("other");
        h.record("same");
        assert_eq!(h.len(), 3, "非相邻的重复照常入栈");
    }

    /// 超出上限丢**最旧**的，最新的始终留在 `back()`。
    #[test]
    fn test_cap_drops_oldest() {
        let mut h = InputHistory::new();
        for i in 0..(MAX_ENTRIES + 5) {
            h.record(&format!("m{i}"));
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        // 最新一条仍可翻到
        assert_eq!(
            h.prev("").as_deref(),
            Some(format!("m{}", MAX_ENTRIES + 4).as_str())
        );
    }

    /// 提交会退出浏览态：不留下指向旧条目的游标，草稿也不再有用。
    #[test]
    fn test_record_leaves_browsing() {
        let mut h = InputHistory::new();
        h.record("one");
        h.prev("draft");
        assert!(h.is_browsing());

        h.record("two");
        assert!(!h.is_browsing(), "提交后退出浏览态");
        assert_eq!(h.prev("").as_deref(), Some("two"), "从最新一条重新开始");
    }

    /// 多行原文原样保存与取回（含 CJK）—— 不按行切、不改写。
    #[test]
    fn test_multiline_and_cjk_roundtrip() {
        let mut h = InputHistory::new();
        h.record("第一行\n第二行");
        assert_eq!(h.prev("").as_deref(), Some("第一行\n第二行"));
    }
}
