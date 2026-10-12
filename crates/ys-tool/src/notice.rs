//! 通知（notice）拼接的**唯一实现** —— 纯字符串函数、零依赖、无 IO。
//!
//! **分工（重要）**：本模块只拼**形态**（`[a. b]` 附在正文后），**不决定报什么** ——
//! 是否报由调用方判定。上限一律**显式报出**、不得静默（设计 §1.4），但「这一条该不该出现」
//! 是各工具自己的判断（已杀子进程时不说「无命中」、未截断时不报上限…）。
//!
//! 与 [`crate::truncate`] 同模式：`truncate` 只算「取哪些行」，文案由调用方拼；本模块只负责
//! 把调用方给出的若干条 notice 拼成统一形态，避免 grep / find / ls / websearch / webfetch
//! 各写一份而漂移。

/// 把 notice 拼成 `\n\n[… . …]` 附在正文后；正文为空时 notice 独占内容（不留下前导空行）。
///
/// `notices` 为空时**原样不动** —— 调用方无需先判空。
pub fn append_notices(content: &mut String, notices: &[String]) {
    if notices.is_empty() {
        return;
    }
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push('[');
    content.push_str(&notices.join(". "));
    content.push(']');
}

/// 「结果偏大」提示的**统一文案**（供 `grep` / `read` 复用）。
///
/// 只拼文案；是否附由调用方决定（与本模块 [`append_notices`] 的分工一致）。
/// `matches` 为 `None` 时表示「无命中数概念」的工具（如 `read`），走读区间措辞。
pub fn size_notice(bytes: usize, matches: Option<usize>) -> String {
    let kib = bytes.div_ceil(1024);
    match matches {
        Some(n) => format!(
            "result is large (~{kib} KiB, {n} matches). Narrow it: use a smaller context, \
             a tighter pattern/path, or read a specific line range"
        ),
        None => {
            format!("result is large (~{kib} KiB). Read a narrower range with `offset` / `limit`")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `size_notice` 两种形态：带命中数（grep）/ 不带（read）。
    ///
    /// 钉住的变异：把 `div_ceil(1024)` 改成 `/ 1024`（8 KiB+1 字节 ⇒ 报「8 KiB」而非「9 KiB」）→ 变红；
    /// 或让 `None` 分支也输出 "matches" → 变红。
    #[test]
    fn size_notice_reports_kib_and_narrowing() {
        let with = size_notice(16 * 1024, Some(100));
        assert!(
            with.contains("16 KiB") && with.contains("100 matches"),
            "{with}"
        );

        let without = size_notice(9 * 1024, None);
        assert!(
            without.contains("9 KiB") && !without.contains("matches"),
            "{without}"
        );

        // 恰好 8 KiB ⇒ 8；多 1 字节 ⇒ 进位到 9（`div_ceil` 语义）。
        assert!(size_notice(8 * 1024, None).contains("8 KiB"));
        assert!(size_notice(8 * 1024 + 1, None).contains("9 KiB"));
    }

    // `notice` 的既有测试全在工具层、且都是**单条**用例 —— `join` 不参与，`. ` 分隔符与
    // 「空正文守卫」**不可观测**（变异后全绿）。本模块把两处补齐：这里钉纯函数形态，
    // `tools-basic` 的多 notice 用例钉真实路径。

    /// `notices` 为空 → 内容**逐字节原样**（空串仍是空串、非空正文一个字节不加）。
    ///
    /// 钉住的变异：删掉 `if notices.is_empty() { return; }` 早返回
    /// → 尾部多出 `[]`（空串从空变 `[]`）→ 本用例变红。
    #[test]
    fn empty_notices_leave_content_untouched() {
        let mut empty = String::new();
        append_notices(&mut empty, &[]);
        assert_eq!(empty, "", "空 notices 不得改变空内容");

        let mut body = String::from("hello");
        append_notices(&mut body, &[]);
        assert_eq!(body, "hello", "空 notices 不得给正文追加任何字节");
    }

    /// 正文非空 + 一条 → `body\n\n[x]`（**空行在前**是既有形态，不得改成行内拼接）。
    ///
    /// 钉住的变异：删掉 `content.push_str("\n\n")` → `body[x]` → 本用例变红。
    #[test]
    fn non_empty_body_gets_blank_line_before_bracket() {
        let mut content = String::from("body");
        append_notices(&mut content, &["x".to_string()]);
        assert_eq!(content, "body\n\n[x]");
    }

    /// 正文非空 + 两条 → `body\n\n[a. b]`。
    ///
    /// 钉住的变异：`notices.join(". ")` → `join("; ")`（或 `join("")`）——
    /// 单条用例看不见分隔符，本用例是纯函数层唯一的 `. ` 锚点 → 变红。
    #[test]
    fn multiple_notices_are_joined_with_dot_space() {
        let mut content = String::from("body");
        append_notices(&mut content, &["a".to_string(), "b".to_string()]);
        assert_eq!(content, "body\n\n[a. b]");
    }

    /// 正文**为空** + 一条 → `[x]`：notice 独占内容，**不留前导空行**。
    ///
    /// 钉住的变异：去掉 `if !content.is_empty()` 守卫（恒插 `\n\n`）
    /// → 变成 `\n\n[x]` → 本用例变红。空正文只在「零命中 + 被中止/超时」等路径出现，
    /// 工具层夹具难构造，故这条守卫的锚点主要落在这里。
    #[test]
    fn empty_body_has_no_leading_blank_line() {
        let mut content = String::new();
        append_notices(&mut content, &["x".to_string()]);
        assert_eq!(content, "[x]");
    }

    /// 正文为空 + 两条 → `[a. b]`（空正文守卫与 `. ` 分隔符**同时**成立）。
    ///
    /// 钉住的变异：恒插 `\n\n` → `\n\n[a. b]`；`join(". ")` → `join("; ")` → `[a; b]`。
    #[test]
    fn empty_body_still_joins_multiple_notices() {
        let mut content = String::new();
        append_notices(&mut content, &["a".to_string(), "b".to_string()]);
        assert_eq!(content, "[a. b]");
    }
}
