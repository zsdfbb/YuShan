//! CJK 断点：让 ratatui 的断词器在中文之间找到可断位置。
//!
//! # 问题
//!
//! ratatui 的 `Paragraph` 只在**空白**处断行（`ratatui-widgets` 的 `WordWrapper`：
//! `word_found = non_whitespace_previous && is_whitespace`）。中文没有空格 ⇒ 一整串 CJK
//! 被当成**一个词**：它在当前行放不下时**整体**被推到下一行，当前行只剩前缀那几个字。
//!
//! 实测（宽 80，首行实际占用列数）：
//!
//! | 条目 | 首行 | 说明 |
//! |---|---|---|
//! | `User` | 1 | 只剩 `> ` |
//! | `Thinking` | 3 | 只剩 `⌁ ` |
//! | `Error` | 6 | 只剩 `Error: ` |
//! | `Tool` | 10 | 只剩 `⏺ bash` |
//!
//! 正文（`Assistant`）不受影响 —— 它经 `md::render`，那一步已由 `tui_markdown` 生成断点。
//! 本模块把这套「渲染期生成断点」的做法补给其余全部变体。
//!
//! # 机制
//!
//! 在相邻 CJK 之间插入 **U+200B（零宽空格）**。ratatui 显式把它当断点
//! （`ratatui-core` 的 `Grapheme::is_whitespace`：`symbol == ZWSP || …`），且它宽度为 0：
//!
//! - **不占列** ⇒ 折行位置之外的一切宽度计算不受影响；
//! - **不写入 `Buffer`** ⇒ 屏幕上看不见，复制选区也拿不到（`selection.rs` 无需改动）。
//!
//! # 边界
//!
//! 只处理「相邻两个 CJK 之间」。ASCII、CJK 与 ASCII 的边界**不动** ——
//! 前者本就有空格可断，后者的边界正是自然断点（`layout.rs` 的折行口径已能处理）。
//! 已有空白 / 零宽字符的位置不重复插入。

/// U+200B ZERO WIDTH SPACE。ratatui 的断词器认它作断点，宽度 0。
const ZWSP: char = '\u{200B}';

/// 该字符是否属于「需要断点」的宽字符族。
///
/// 覆盖 CJK 汉字、假名、谚文、全角标点与 CJK 扩展平面。这些字符的共同点是
/// **没有词间空格**且**宽度为 2 列** —— 正是断词器找不到断点的那一类。
///
/// 判据取 Unicode 区段而非「宽度 == 2」：emoji 等宽字符之间插断点会把一个
/// 表情序列（ZWJ 连接的组合）从中间切断，那是**破坏**而不是断行。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F     // 谚文字母
        | 0x2E80..=0x303E   // CJK 部首扩展、标点
        | 0x3041..=0x33FF   // 平假名、片假名、注音、CJK 兼容
        | 0x3400..=0x4DBF   // CJK 扩展 A
        | 0x4E00..=0x9FFF   // CJK 基本区
        | 0xA000..=0xA4CF   // 彝文
        | 0xAC00..=0xD7A3   // 谚文音节
        | 0xF900..=0xFAFF   // CJK 兼容表意文字
        | 0xFE30..=0xFE4F   // CJK 兼容形式
        | 0xFF00..=0xFF60   // 全角 ASCII、全角标点
        | 0xFFE0..=0xFFE6   // 全角符号
        | 0x20000..=0x3FFFD // CJK 扩展 B 及以后
    )
}

/// 在相邻 CJK 字符之间插入零宽断点，返回新串。
///
/// **幂等**：已存在 ZWSP 的位置不再插入（重复插入不改变折行，但会让字符串无谓变长）。
///
/// 无 CJK 相邻对时**原样返回**（不分配新串）—— 纯 ASCII 走这条路径，开销为零。
pub(crate) fn with_cjk_breakpoints(s: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;

    // 快路径：不存在「相邻 CJK」时直接借用。绝大多数工具摘要（路径 / 命令）走这里。
    if !has_adjacent_cjk(s) {
        return Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len() + s.len() / 2);
    let mut prev_cjk = false;
    for ch in s.chars() {
        if ch == ZWSP {
            // 断点已在此 ⇒ 它**断开**了相邻关系：`中\u{200B}文` 再来一遍时，
            // 不能因为在 ZWSP 前面刚看过 CJK 就再插一个。幂等性就靠这一步。
            out.push(ch);
            prev_cjk = false;
            continue;
        }
        let cjk = is_cjk(ch);
        if cjk && prev_cjk {
            out.push(ZWSP);
        }
        out.push(ch);
        prev_cjk = cjk;
    }
    Cow::Owned(out)
}

/// 是否存在相邻的 CJK 字符对（即 [`with_cjk_breakpoints`] 是否有活可干）。
///
/// 与 [`with_cjk_breakpoints`] 同口径：零宽字符**断开**相邻关系。
fn has_adjacent_cjk(s: &str) -> bool {
    let mut prev_cjk = false;
    for ch in s.chars() {
        if ch == ZWSP {
            prev_cjk = false;
            continue;
        }
        let cjk = is_cjk(ch);
        if cjk && prev_cjk {
            return true;
        }
        prev_cjk = cjk;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> String {
        with_cjk_breakpoints(s).into_owned()
    }

    /// 相邻 CJK 之间插入；非相邻处不动。
    #[test]
    fn inserts_between_adjacent_cjk_only() {
        assert_eq!(f("中文"), "中\u{200B}文");
        assert_eq!(f("中文内容"), "中\u{200B}文\u{200B}内\u{200B}容");
        // ASCII 之间不动（本就有空格可断）
        assert_eq!(f("hello world"), "hello world");
        // CJK 与 ASCII 的边界不动（那是自然断点）
        assert_eq!(f("中文abc"), "中\u{200B}文abc");
        assert_eq!(f("abc中文"), "abc中\u{200B}文");
        // 单字无相邻对
        assert_eq!(f("中"), "中");
    }

    /// 已有的空白不被当成 CJK 相邻对 —— 空格本身已是断点，再插是冗余。
    #[test]
    fn existing_whitespace_is_not_a_pair() {
        assert_eq!(f("中 文"), "中 文");
        assert_eq!(f("中\n文"), "中\n文");
    }

    /// **幂等**：重复调用不叠加零宽字符。
    ///
    /// ⚠️ 这条测试**必须绕开快路径**。`with_cjk_breakpoints` 先问 `has_adjacent_cjk`：
    /// 已插过断点的串不再有相邻 CJK ⇒ 直接借用原串返回，**根本不进主循环**。
    /// 于是「主循环里对 ZWSP 的处理错了」这类变异会被快路径掩盖掉（实测过：
    /// 把 `prev_cjk = false` 删掉，本用例仍然绿）。
    ///
    /// 故这里直接测**主循环**的行为：构造一个既有 ZWSP、又有真正相邻 CJK 的串。
    /// 它既走不到快路径（有相邻 CJK），又能在循环里暴露重复插入。
    ///
    /// 变异：把 `if ch == ZWSP { … prev_cjk = false; … }` 里的 `prev_cjk = false`
    /// 删掉 ⇒ `中\u{200B}文` 在 ZWSP 处仍记着「上一个也是 CJK」⇒ 插出连续两个
    /// 零宽字符 ⇒ 本用例红。
    #[test]
    fn is_idempotent() {
        // 已插过的部分：不得再插。
        let once = f("中文内容");
        let twice = f(&once);
        assert_eq!(once, twice, "重复插入不得叠加");
        assert!(!twice.contains("\u{200B}\u{200B}"), "不得出现连续零宽字符");

        // **走主循环**的形态：前段已有断点，后段是尚未处理的相邻 CJK。
        // 快路径在此不生效（`内` `容` 相邻），故主循环的 ZWSP 分支真的会被执行。
        let mixed = "中\u{200B}文内容";
        let out = f(mixed);
        assert_eq!(
            out, "中\u{200B}文\u{200B}内\u{200B}容",
            "已有断点处不得再插，未处理的相邻处必须插"
        );
        assert!(
            !out.contains("\u{200B}\u{200B}"),
            "主循环不得在已有断点后再插一个：{out:?}"
        );
    }

    /// 全角标点参与断点（中文正文里标点后必须能断，否则一句一个「词」）。
    #[test]
    fn full_width_punctuation_breaks() {
        assert_eq!(f("好。"), "好\u{200B}。");
        assert_eq!(f("，中文"), "，\u{200B}中\u{200B}文");
    }

    /// 假名 / 谚文同样处理 —— 它们也没有词间空格。
    #[test]
    fn kana_and_hangul_break() {
        assert_eq!(f("ひらがな"), "ひ\u{200B}ら\u{200B}が\u{200B}な");
        assert_eq!(f("한국어"), "한\u{200B}국\u{200B}어");
    }

    /// emoji **不**参与：在 ZWJ 连接的表情序列中间插断点会把它切断。
    #[test]
    fn emoji_is_left_alone() {
        assert_eq!(f("😀😀"), "😀😀", "emoji 不是 CJK 区段，不得插入");
    }

    /// 无 CJK 相邻对时**借用**原串（不分配）—— 纯 ASCII 路径零开销。
    #[test]
    fn borrows_when_nothing_to_do() {
        assert!(matches!(
            with_cjk_breakpoints("/usr/local/bin/some_tool"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            with_cjk_breakpoints("中"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            with_cjk_breakpoints("中文"),
            std::borrow::Cow::Owned(_)
        ));
    }
}
