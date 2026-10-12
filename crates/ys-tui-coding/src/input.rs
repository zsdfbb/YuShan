//! 输入缓冲 —— 编辑原语层（T5 最小可用 → T8 补全光标移动 / Delete / 行首行尾）。
//!
//! # 核心不变量：`cursor` 是 **char 索引**，不是字节索引
//!
//! 旧实现（`apps/coding-agent/src/ui/events.rs:131`）用
//! `text.remove(cursor - 1)` —— 对 `"中文"` 退格一次，`cursor - 1` 落在**多字节
//! 字符中间**，`String::remove` 直接 panic。本实现一律先按 char 边界求出字节区间，
//! 再 `replace_range`。
//!
//! **本模块的每一个编辑原语都遵守这条**：`cursor_left/right` 是 `±1` 个 char，
//! `cursor_home/end` 以 `'\n'` 为行界（char 层），`cursor_word_left/right` 按
//! [`is_word_char`] 划词、**按词计数**（故跨 CJK 与多字节字符都落在 char 边界上），
//! `delete_forward` 删的是**一个 char** 而非一个字节。字节索引只在一个地方出现：
//! [`InputBuffer::byte_index`]，且它只会返回合法边界。

/// 单行/多行输入缓冲。
#[derive(Clone, Debug, Default)]
pub struct InputBuffer {
    /// 原始文本（可含 `\n`）。
    pub text: String,
    /// **char 索引**（0 = 行首，`text.chars().count()` = 行尾）。
    pub cursor: usize,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 光标处插入一个字符（`cursor` 前进 1 个 **char**）。
    pub fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    /// 光标处插入一段文本（供 T8 的 bracketed paste 用）。
    ///
    /// 多行粘贴会原样进来 —— 「粘贴的多行不被当多次提交」由调用方保证
    /// （`Event::Paste` 与本方法一一对应，不经过 `Enter`）。
    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_index(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// 删除光标**前一个字符**（按 char；`cursor == 0` 时是 no-op，绝不 panic）。
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// 用补全候选项的 `replacement`（形如 `/{name} `，尾随空格）替换缓冲的
    /// **首个词**，其后的内容（正文 / 参数）**原样保留**。
    ///
    /// 补全浮层只对**开头的命令词**生效（`completion::open_with_skills` 只看首个词），
    /// 故这里也只动首个词 —— 用户先把正文敲好、再跳到行首补命令前缀时，正文是
    /// skill 的参数，绝不能连它一起被覆写（那正是「前面输入的内容被覆盖」这个 bug）。
    ///
    /// 光标落在**命令词与保留正文之间**（正文前若有空白一并越过），而不是整段末尾 ——
    /// 用户接着敲的应当是参数 / 正文，不是被推到最右边。
    ///
    /// 前提：`text` 以首个词开头（补全只在 `/` 开头时打开，见
    /// [`crate::completion::open_with_skills`]）。首个词之后的剩余串若存在，必以空白
    /// 开头（`split_whitespace` 给出的是**极大**词）。
    pub fn complete_leading_token(&mut self, replacement: &str) {
        let token_len = self
            .text
            .split_whitespace()
            .next()
            .map(str::len)
            .unwrap_or(0);
        // 先取成自有串：随后要可变借用 `self.text` 写回。
        let rest = self.text[token_len..].to_string();
        if rest.is_empty() {
            // 没有正文：整段写入 replacement（含尾随空格），光标到末尾。
            self.text = replacement.to_string();
            self.cursor = self.text.chars().count();
        } else {
            // 有正文：去掉 replacement 的尾随空格，复用正文自带的空白作分隔，
            // 避免出现两个并排空格。
            let head = replacement.trim_end();
            let lead_ws = rest.chars().take_while(|c| c.is_whitespace()).count();
            self.text = format!("{head}{rest}");
            self.cursor = head.chars().count() + lead_ws;
        }
    }

    /// 取走内容（提交用）；光标归零，缓冲清空。
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    // -----------------------------------------------------------------------
    // 光标移动（全部按 char 索引；CJK / emoji / ZWJ 序列一律不 panic）
    // -----------------------------------------------------------------------

    /// 光标左移一个 **char**（已在行首则 no-op）。
    pub fn cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// 光标右移一个 **char**（已在行尾则 no-op）。
    pub fn cursor_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    /// 光标左移一个**词**（emacs `backward-word`）：先回退吃掉非词字符，
    /// 再回退吃掉词字符。已在缓冲开头则 no-op。
    ///
    /// 不跨词地停：越过前一个词后**不吞**它左边那个分隔符。
    /// `'\n'` 是非词字符 ⇒ 会退回上一行行尾（见 [`is_word_char`]）。
    pub fn cursor_word_left(&mut self) {
        let at = self.byte_index(self.cursor);
        let mut consumed = 0;
        let mut in_word = false;
        for c in self.text[..at].chars().rev() {
            if is_word_char(c) {
                in_word = true;
                consumed += 1;
            } else if in_word {
                break;
            } else {
                consumed += 1;
            }
        }
        self.cursor -= consumed;
    }

    /// 光标右移一个**词**（emacs `forward-word`）：先前进吃掉非词字符，
    /// 再前进吃掉词字符，停在词后的第一个非词字符**之前**。
    pub fn cursor_word_right(&mut self) {
        let at = self.byte_index(self.cursor);
        let mut advance = 0;
        let mut in_word = false;
        for c in self.text[at..].chars() {
            if is_word_char(c) {
                in_word = true;
                advance += 1;
            } else if in_word {
                break;
            } else {
                advance += 1;
            }
        }
        self.cursor += advance;
    }

    /// 移到**当前行**行首（行界是 `'\n'`，不是缓冲开头）。
    ///
    /// 单行输入时等价于移到 0；多行时只回到本行开头。
    pub fn cursor_home(&mut self) {
        let before = self.text_before_cursor();
        self.cursor = match before.rfind('\n') {
            // `\n` 的下一个 char 即本行行首
            Some(byte) => before[..byte].chars().count() + 1,
            None => 0,
        };
    }

    /// 移到**当前行**行尾 —— 停在 `'\n'` **之前**（不越过换行）。
    ///
    /// 该行是最后一行时即 `char_count()`（缓冲末尾）。
    pub fn cursor_end(&mut self) {
        let rest = &self.text[self.byte_index(self.cursor)..];
        let advance = match rest.find('\n') {
            Some(byte) => rest[..byte].chars().count(),
            None => rest.chars().count(),
        };
        self.cursor += advance;
    }

    /// `Delete`：删除光标**后一个 char**（`cursor == char_count` 时是 no-op）。
    ///
    /// 注意与 [`InputBuffer::backspace`] 的方向相反：本方法删的是「光标右边」。
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    /// 光标位置 → `(行号, 行内 char 列)`，均从 0 起。
    ///
    /// 行号 = 光标前 `'\n'` 的个数；列 = 本行行首到光标之间的 **char** 数。
    /// 供光标渲染（配合显示宽度求真实列）与测试使用。
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let before = self.text_before_cursor();
        let row = before.matches('\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").chars().count();
        (row, col)
    }

    /// char 总数。
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// 清空（Esc 在 idle 时的行为）。
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// 内容行数（供输入区高度自适应）。空串算 1 行。
    pub fn lines(&self) -> usize {
        self.text.split('\n').count()
    }

    /// 光标前的子串（光标定位用）。
    pub(crate) fn text_before_cursor(&self) -> &str {
        &self.text[..self.byte_index(self.cursor)]
    }

    /// char 索引 → 字节索引。
    ///
    /// 越界（`char_idx >= 字符数`）返回 `text.len()` —— 即行尾，且**不会**落在
    /// 字符中间（`len()` 永远是合法边界）。
    pub(crate) fn byte_index(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }
}

/// 词字符 = 字母 / 数字 / `_`；其余（空白、标点、`'\n'`）都是分隔符。
///
/// 注意 `char::is_alphanumeric()` 对 CJK 汉字为真 ⇒ **一整段中文算一个词**，
/// 按词移动会整段跳过而不是逐字 —— 对中文语料这是想要的行为。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_take() {
        let mut b = InputBuffer::new();
        for c in "hi".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.text, "hi");
        assert_eq!(b.cursor, 2);
        assert_eq!(b.take(), "hi");
        assert_eq!(b.text, "");
        assert_eq!(b.cursor, 0);
    }

    /// 回归：中文退格**不得 panic**，且删掉的是**一个完整字符**。
    #[test]
    fn test_backspace_cjk_does_not_panic() {
        let mut b = InputBuffer::new();
        b.insert_str("中文");
        assert_eq!(b.text, "中文");
        assert_eq!(b.cursor, 2, "光标是 char 索引：2 个字符");

        b.backspace();
        assert_eq!(b.text, "中");
        assert_eq!(b.cursor, 1);

        b.backspace();
        assert_eq!(b.text, "");
        assert_eq!(b.cursor, 0);

        // 再退一次：光标已在行首，必须是 no-op（不是 panic、不是越界）
        b.backspace();
        assert_eq!(b.text, "");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn test_backspace_empty_buffer_is_noop() {
        let mut b = InputBuffer::new();
        b.backspace();
        assert_eq!(b.text, "");
        assert_eq!(b.cursor, 0);
    }

    /// 混排（ASCII + CJK + emoji）退格删一个 char。
    #[test]
    fn test_backspace_mixed_width() {
        let mut b = InputBuffer::new();
        b.insert_str("a中🦀");
        assert_eq!(b.cursor, 3);
        b.backspace();
        assert_eq!(b.text, "a中");
        b.backspace();
        assert_eq!(b.text, "a");
        b.backspace();
        assert_eq!(b.text, "");
    }

    /// 光标在中间时插入/退格只影响光标处。
    #[test]
    fn test_insert_mid_string() {
        let mut b = InputBuffer::new();
        b.insert_str("中文");
        b.cursor = 1; // 光标在「中」之后
        b.insert_char('x');
        assert_eq!(b.text, "中x文");
        assert_eq!(b.cursor, 2);
        b.backspace();
        assert_eq!(b.text, "中文");
        assert_eq!(b.cursor, 1);
    }

    /// 补全只替换**首个词**，其后正文原样保留；光标停在命令词与正文之间。
    ///
    /// 变异：改回整段覆写 ⇒ 正文丢失，本测试变红。
    #[test]
    fn test_complete_leading_token_keeps_trailing_body() {
        let mut b = InputBuffer::new();
        b.insert_str("看看代码");
        b.cursor = 0;
        b.insert_str("/comm ");
        assert_eq!(b.text, "/comm 看看代码");
        assert_eq!(b.cursor, 6, "光标停在「/comm 」之后");

        b.complete_leading_token("/commit ");
        assert_eq!(b.text, "/commit 看看代码", "正文不被吃掉");
        assert_eq!(b.cursor, 8, "光标停在「/commit 」之后、正文之前");
    }

    /// 没有正文时照旧：写入整段 `replacement`（含尾随空格），光标到末尾。
    #[test]
    fn test_complete_leading_token_without_body_keeps_trailing_space() {
        let mut b = InputBuffer::new();
        b.insert_str("/mo");
        b.complete_leading_token("/model ");
        assert_eq!(b.text, "/model ");
        assert_eq!(b.cursor, 7);
    }

    /// 正文以换行开头：换行与正文都保留，光标越过换行停在正文行首。
    #[test]
    fn test_complete_leading_token_preserves_newline_separator() {
        let mut b = InputBuffer::new();
        b.insert_str("/comm\n看看代码");
        b.complete_leading_token("/commit ");
        assert_eq!(b.text, "/commit\n看看代码");
        assert_eq!(b.cursor, 8, "越过换行，停在「看」之前");
    }

    /// 首个词后的剩余串全是空白也算「有正文」：不追加第二个尾随空格。
    #[test]
    fn test_complete_leading_token_whitespace_only_body() {
        let mut b = InputBuffer::new();
        b.insert_str("/mo  ");
        b.complete_leading_token("/model ");
        assert_eq!(b.text, "/model  ", "只保留原有空白，不叠一个");
        assert_eq!(b.cursor, 8);
    }

    #[test]
    fn test_insert_str_multiline_and_cursor() {
        let mut b = InputBuffer::new();
        b.insert_str("a\nb");
        assert_eq!(b.text, "a\nb");
        assert_eq!(b.cursor, 3, "char 索引把 \\n 也算一个");
        assert_eq!(b.lines(), 2);
    }

    #[test]
    fn test_lines_counts() {
        let mut b = InputBuffer::new();
        assert_eq!(b.lines(), 1, "空串算 1 行");
        b.insert_str("a\nb\nc");
        assert_eq!(b.lines(), 3);
        b.insert_str("\n");
        assert_eq!(b.lines(), 4, "尾随换行也算一行（光标停在那一行）");
    }

    #[test]
    fn test_clear() {
        let mut b = InputBuffer::new();
        b.insert_str("x中");
        b.clear();
        assert_eq!(b.text, "");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn test_text_before_cursor_is_char_safe() {
        let mut b = InputBuffer::new();
        b.insert_str("中文abc");
        b.cursor = 2;
        assert_eq!(b.text_before_cursor(), "中文");
        b.cursor = 0;
        assert_eq!(b.text_before_cursor(), "");
    }

    // -----------------------------------------------------------------------
    // 光标移动原语（T9）
    // -----------------------------------------------------------------------

    /// `cursor_left/right` 在 CJK 上按 **char** 移动，不按字节。
    ///
    /// 「中文」字节长 6 —— 若按字节左移，第一次就落到 `中` 的字节中间（cursor=5）
    /// 且 `byte_index` 会算出非法边界。char 语义下必须是 2 → 1 → 2。
    #[test]
    fn test_cursor_left_right_are_char_based_on_cjk() {
        let mut b = InputBuffer::new();
        b.insert_str("中文");
        assert_eq!(b.cursor, 2);

        b.cursor_left();
        assert_eq!(b.cursor, 1, "左移一个 char");

        b.cursor_right();
        assert_eq!(b.cursor, 2, "右移回来");

        // 左右移动后插入，位置必须正确（证明 cursor 始终落在 char 边界）
        b.cursor_left();
        b.insert_char('x');
        assert_eq!(b.text, "中x文");
    }

    #[test]
    fn test_cursor_left_at_zero_is_noop_and_does_not_underflow() {
        let mut b = InputBuffer::new();
        b.insert_str("ab");
        b.cursor_left();
        b.cursor_left();
        assert_eq!(b.cursor, 0);
        b.cursor_left();
        assert_eq!(b.cursor, 0, "行首左移是 no-op（不是下溢）");

        let mut empty = InputBuffer::new();
        empty.cursor_left();
        assert_eq!(empty.cursor, 0, "空缓冲左移不 panic");
    }

    #[test]
    fn test_cursor_right_at_end_is_noop_and_does_not_overflow() {
        let mut b = InputBuffer::new();
        b.insert_str("中");
        assert_eq!(b.cursor, 1);
        b.cursor_right();
        assert_eq!(b.cursor, 1, "行尾右移是 no-op（不是越界）");

        let mut empty = InputBuffer::new();
        empty.cursor_right();
        assert_eq!(empty.cursor, 0, "空缓冲右移不 panic");
    }

    /// `home`/`end` 以 `'\n'` 为**行界**，不是缓冲界。
    #[test]
    fn test_cursor_home_and_end_are_line_scoped() {
        let mut b = InputBuffer::new();
        b.insert_str("ab\ncd");
        assert_eq!(b.cursor, 5, "末尾：char 索引 5");

        b.cursor_home();
        assert_eq!(b.cursor, 3, "第 2 行行首（`\\n` 之后）");

        b.cursor_end();
        assert_eq!(b.cursor, 5, "第 2 行行尾 = 缓冲末尾");

        // 第 1 行：行尾停在 `\n` 之前（索引 2），不越过换行
        b.cursor = 0;
        b.cursor_end();
        assert_eq!(b.cursor, 2, "停在 `\\n` 之前，不越过换行");

        b.cursor_home();
        assert_eq!(b.cursor, 0, "第 1 行行首 = 0");
    }

    #[test]
    fn test_cursor_home_end_cjk_multiline() {
        let mut b = InputBuffer::new();
        b.insert_str("中文\nabcd");
        assert_eq!(b.text.chars().count(), 7);
        b.cursor_home();
        assert_eq!(b.cursor, 3, "第 2 行行首（3 个 char 的换行前缀）");
        b.cursor_end();
        assert_eq!(b.cursor, 7);

        b.cursor = 0;
        b.cursor_end();
        assert_eq!(b.cursor, 2, "第 1 行行尾按 char 计（中文 = 2 个 char）");
    }

    /// `delete_forward` 删光标**后**一个 char（与 backspace 方向相反）。
    #[test]
    fn test_delete_forward_removes_char_after_cursor() {
        let mut b = InputBuffer::new();
        b.insert_str("abc");
        b.cursor = 1;
        b.delete_forward();
        assert_eq!(b.text, "ac", "删掉光标后的 'b'");
        assert_eq!(b.cursor, 1, "光标不动");
        b.delete_forward();
        assert_eq!(b.text, "a");
        b.delete_forward();
        assert_eq!(b.text, "a", "光标已在末尾 → no-op");
        assert_eq!(b.cursor, 1);
    }

    #[test]
    fn test_delete_forward_at_end_is_noop() {
        let mut b = InputBuffer::new();
        b.insert_str("中");
        assert_eq!(b.cursor, 1);
        b.delete_forward();
        assert_eq!(b.text, "中", "行尾 Delete 是 no-op");

        let mut empty = InputBuffer::new();
        empty.delete_forward();
        assert_eq!(empty.text, "", "空缓冲 Delete 不 panic");
    }

    /// Delete 与 Backspace 是**不同**的操作 —— 同一初始状态下结果必须不同。
    ///
    /// 若把 `delete_forward` 变异成删前一个字符，本测试与
    /// [`test_delete_forward_removes_char_after_cursor`] 都会变红。
    #[test]
    fn test_delete_forward_differs_from_backspace() {
        let mut fwd = InputBuffer::new();
        fwd.insert_str("中文");
        fwd.cursor = 1;
        fwd.delete_forward();
        assert_eq!(fwd.text, "中", "Delete 删光标后的「文」");

        let mut back = InputBuffer::new();
        back.insert_str("中文");
        back.cursor = 1;
        back.backspace();
        assert_eq!(back.text, "文", "Backspace 删光标前的「中」");
    }

    #[test]
    fn test_delete_forward_cjk_and_zwj_emoji_do_not_panic() {
        let mut b = InputBuffer::new();
        b.insert_str("中文");
        b.cursor = 0;
        b.delete_forward();
        assert_eq!(b.text, "文", "CJK 删一个完整 char");

        // ZWJ 序列：「👨👩👧」由多个 char 组成，删一个 char 只改内容不 panic
        let mut z = InputBuffer::new();
        z.insert_str("👨‍👩‍👧");
        let n = z.text.chars().count();
        assert!(n > 1, "ZWJ 序列本身是多 char：{n}");
        z.cursor = 0;
        for _ in 0..n {
            z.delete_forward(); // 每次删一个 char，绝不 panic
        }
        assert_eq!(z.text, "", "逐 char 删空");
    }

    /// `cursor_row_col` 返回 `(行号, 行内 char 列)`，均 0 起。
    #[test]
    fn test_cursor_row_col() {
        let mut b = InputBuffer::new();
        b.insert_str("ab\n中文");

        b.cursor = 0;
        assert_eq!(b.cursor_row_col(), (0, 0));

        b.cursor = 2;
        assert_eq!(b.cursor_row_col(), (0, 2), "第 1 行行尾");

        b.cursor = 3;
        assert_eq!(b.cursor_row_col(), (1, 0), "第 2 行行首");

        b.cursor = 5;
        assert_eq!(b.cursor_row_col(), (1, 2), "第 2 行行尾（2 个 char）");

        b.cursor = 4;
        assert_eq!(
            b.cursor_row_col(),
            (1, 1),
            "列按 char 计，不是字节（中 = 1 char）"
        );
    }

    /// `home` 与 `cursor_row_col` 协同：home 后列必为 0、行号不变。
    #[test]
    fn test_home_is_consistent_with_row_col() {
        let mut b = InputBuffer::new();
        b.insert_str("aa\nbbbb\ncc");
        for start in [3usize, 5, 6, 9, 10] {
            b.cursor = start;
            let (row, _) = b.cursor_row_col();
            b.cursor_home();
            assert_eq!(b.cursor_row_col(), (row, 0), "start={start}");
        }
    }

    /// 按词右移：停在**词后第一个分隔符之前**，分隔符不前进。
    #[test]
    fn test_word_right_stops_before_separator() {
        let mut b = InputBuffer::new();
        b.insert_str("foo bar_baz  qux");
        b.cursor = 0; // insert_str 会把光标留在末尾

        b.cursor_word_right();
        assert_eq!(b.cursor, 3, "越过 foo，停在空格前");
        b.cursor_word_right();
        assert_eq!(b.cursor, 11, "`bar_baz` 里 `_` 是词字符，算一个词");
        b.cursor_word_right();
        assert_eq!(b.cursor, 16, "连续空格一次吃掉，停在 qux 之后（缓冲末尾）");
    }

    /// 按词左移：先吃掉分隔符，再退回整个词；越过词后**不吞**它左边的分隔符。
    #[test]
    fn test_word_left_skips_separators_then_word() {
        let mut b = InputBuffer::new();
        b.insert_str("foo bar_baz  qux");
        b.cursor = b.text.chars().count();

        b.cursor_word_left();
        assert_eq!(b.cursor, 13, "越过 qux 停在词首");
        b.cursor_word_left();
        assert_eq!(b.cursor, 4, "吃掉两个空格 + 整个 bar_baz，停在空格后");
        b.cursor_word_left();
        assert_eq!(b.cursor, 0, "吃掉空格 + foo，到缓冲开头");
        b.cursor_word_left();
        assert_eq!(b.cursor, 0, "已在开头：no-op");
    }

    /// 边界：空缓冲、只有分隔符、光标在词中间。
    #[test]
    fn test_word_motion_boundaries() {
        let mut b = InputBuffer::new();
        b.cursor_word_left();
        b.cursor_word_right();
        assert_eq!(b.cursor, 0, "空缓冲两侧都是 no-op");

        b.insert_str("   ");
        b.cursor_word_right();
        assert_eq!(b.cursor, 3, "只有空白 ⇒ 一路吃到末尾");
        b.cursor_word_left();
        assert_eq!(b.cursor, 0, "反向亦同");

        let mut b = InputBuffer::new();
        b.insert_str("alpha beta");
        b.cursor = 2;
        b.cursor_word_right();
        assert_eq!(b.cursor, 5, "从词中间：先吃完本词，不属于下一个词");
        b.cursor_word_left();
        assert_eq!(b.cursor, 0, "从词中间左移：回到本词词首");
    }

    /// 跨行：`'\n'` 是分隔符 ⇒ 按词移动会跨行，且**不吞**它。
    #[test]
    fn test_word_motion_crosses_newline() {
        let mut b = InputBuffer::new();
        b.insert_str("aa\nbb");

        b.cursor = 2; // 第一行行尾（'\n' 之前）
        b.cursor_word_right();
        assert_eq!(b.cursor, 5, "吃掉换行与 bb，到缓冲末尾");

        b.cursor_word_left();
        assert_eq!(b.cursor, 3, "退回 bb 的词首（'\n' 之下）");
        b.cursor_word_left();
        assert_eq!(b.cursor, 0, "再左移：吃掉换行 + aa");
    }

    /// CJK：`is_alphanumeric` 对汉字为真 ⇒ **一整段中文算一个词**，整段跳过。
    /// 且不得 panic、不得落在多字节字符中间。
    #[test]
    fn test_word_motion_cjk_is_one_word_and_never_panics() {
        let mut b = InputBuffer::new();
        b.insert_str("中文 段落 word");
        b.cursor = 0; // insert_str 会把光标留在末尾

        b.cursor_word_right();
        assert_eq!(b.cursor, 2, "「中文」两字算一个词");
        b.cursor_word_right();
        assert_eq!(b.cursor, 5, "「段落」同上");
        b.cursor_word_right();
        assert_eq!(b.cursor, 10, "再吃掉空格与 word");

        b.cursor_word_left();
        assert_eq!(b.cursor, 6, "退回 word 词首");
        // 反复移动不得 panic，且始终停在 char 边界上
        for _ in 0..20 {
            b.cursor_word_left();
            assert!(b.text.is_char_boundary(b.byte_index(b.cursor)));
        }
        assert_eq!(b.cursor, 0);
    }
}
