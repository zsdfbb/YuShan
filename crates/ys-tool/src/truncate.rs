//! 截断的唯一实现 —— 纯函数、零依赖、无 IO。
//!
//! **分工（重要）**：本模块只计算「取哪些行 / 撞上了哪个上限」，返回 [`Truncation`]；
//! **通知文案一律由调用方拼**。bash 的 `[Truncated N lines.]`（前插）、read 的
//! `[Showing lines a-b of n. Use offset=N to continue.]`（后追加）、grep 的 match limit
//! 提示，都留在各自工具里。这样 bash / read 改用本模块后输出**逐字节不变**，
//! 既有文案与测试一字不改。
//!
//! **两条字节计数规则**（`head` 与 `tail` 刻意不同，勿「统一」）：
//!
//! | 函数 | 方向 | 单行成本 | 最少保留 |
//! |---|---|---|---|
//! | [`head`] / [`head_lines`] | 取头 | `line.len()`，**不含**换行符 | **1 行**（首行无条件保留） |
//! | [`tail`] | 取尾 | `line.len() + 1`，**含**换行符 | 0 行 |
//!
//! 前者对齐既有 `read.rs` 的循环（先格式化再按格式化后字节数计，且 `&& !output_lines.is_empty()`），
//! 后者对齐既有 `bash.rs` 的 `tail_truncate`。

use std::borrow::Cow;

/// 输出保留的最大行数（与 read / bash 一致）。
pub const MAX_LINES: usize = 2000;
/// 输出保留的最大字节数（与 read / bash 一致）。
pub const MAX_BYTES: usize = 50 * 1024;
/// grep 单行渲染的最大字符数。
pub const GREP_MAX_LINE_CHARS: usize = 500;

/// 撞上的上限种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// 行数上限先命中。
    Lines,
    /// 字节上限先命中。
    Bytes,
}

/// 一次截断的结果。`content` **不含任何通知文案** —— 由调用方自行拼接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    /// 保留下来的内容（已按 `\n` 连接，不带尾随换行）。
    pub content: String,
    /// 撞上的上限；`None` 表示**未被截断**（全部保留）。
    pub hit: Option<Limit>,
    /// 输入的总行数。
    pub total_lines: usize,
    /// 实际保留的行数。`total_lines - shown_lines` 即被丢弃的行数。
    pub shown_lines: usize,
}

impl Truncation {
    /// 未被截断时构造结果。
    fn intact(content: String, total_lines: usize) -> Self {
        Self {
            content,
            hit: None,
            total_lines,
            shown_lines: total_lines,
        }
    }
}

/// 头截断，作用于**已渲染**的行切片。
///
/// `lines` 里每一行都应是最终要输出的样子（调用方自己决定是否加 `{line:>6}\t` 行号前缀、
/// `path:line:` 前缀等）。字节预算按 `line.as_ref().len()` 累加，**不含**分隔符 —— 与既有
/// `read.rs` 循环一致；因此 `read` 必须先格式化再传进来，才能逐字节不变。
///
/// **至少保留一行**：首行无论多大都进结果（后续行才可能因超字节被丢弃），
/// 这也是既有 `read.rs` 的 `&& !output_lines.is_empty()` 语义。
pub fn head_lines<S: AsRef<str>>(lines: &[S], max_lines: usize, max_bytes: usize) -> Truncation {
    let total_lines = lines.len();
    let mut shown_lines = 0usize;
    let mut bytes = 0usize;
    let mut stop = None;

    for line in lines {
        let rendered = line.as_ref();
        // 首行无条件保留；之后任一行放不下就停（严格大于，与 read 一致）。
        if shown_lines > 0 && bytes + rendered.len() > max_bytes {
            stop = Some(Limit::Bytes);
            break;
        }
        bytes += rendered.len();
        shown_lines += 1;
        if shown_lines >= max_lines {
            break;
        }
    }

    // 只有真的丢了行才算「截断」。行数恰好等于 max_lines 时循环也在此收尾，此时 hit 应为 None。
    let hit = if shown_lines < total_lines {
        Some(stop.unwrap_or(Limit::Lines))
    } else {
        None
    };

    let mut content = String::with_capacity(bytes);
    for (i, line) in lines[..shown_lines].iter().enumerate() {
        if i > 0 {
            content.push('\n');
        }
        content.push_str(line.as_ref());
    }

    Truncation {
        content,
        hit,
        total_lines,
        shown_lines,
    }
}

/// 头截断的 `&str` 便捷包装：按 `lines()` 切分后交给 [`head_lines`]。
///
/// 适用于「整段文本」的调用方（如 webfetch 转出的 Markdown）；字节规则同 [`head_lines`]。
pub fn head(text: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines: Vec<&str> = text.lines().collect();
    head_lines(&lines, max_lines, max_bytes)
}

/// 尾截断：保留**末尾**的 `max_lines` 行 / `max_bytes` 字节。
///
/// 单行成本按 `line.len() + 1` 计（**含**换行符），与既有 `bash.rs::tail_truncate`
/// 逐字节一致 —— 包括「全部放得下时原样返回、带尾随换行」的快路径。
/// 与 [`head`] 不同，这里**可以为 0 行**（单行本身就超字节时结果为空）。
pub fn tail(text: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // 快路径：整体都放得下 → 原样返回（保留尾随换行）。
    if total_lines <= max_lines && text.len() <= max_bytes {
        return Truncation::intact(text.to_string(), total_lines);
    }

    // 从尾往前数，看多少行能放进字节预算。
    let mut byte_count = 0usize;
    let mut byte_start = total_lines;
    for i in (0..total_lines).rev() {
        let line_len = lines[i].len() + 1; // +1 计入换行符
        if byte_count + line_len > max_bytes {
            break;
        }
        byte_count += line_len;
        byte_start = i;
    }

    // 再叠加行数上限，两者取更靠后的起点。
    let line_start = total_lines.saturating_sub(max_lines);
    // 字节起点更靠后 ⇒ 字节是约束；否则行数是约束（两者同界时报 Bytes，纯信息性）。
    let hit = if byte_start >= line_start {
        Limit::Bytes
    } else {
        Limit::Lines
    };
    let start = byte_start.max(line_start);

    Truncation {
        content: lines[start..].join("\n"),
        hit: Some(hit),
        total_lines,
        shown_lines: total_lines - start,
    }
}

/// 单行截断：按**字符**（非字节）保留前 `max_chars` 个，返回值与「是否被截断」。
///
/// 按字符切分保证不会切碎 UTF-8 多字节字符。**不追加省略号** —— 截断标记由调用方决定。
pub fn line(text: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => (Cow::Owned(text[..idx].to_string()), true),
        None => (Cow::Borrowed(text), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    /// 生成 `n` 行、每行恰好 `width` 个 ASCII 字符，且逐行内容可辨识（便于断言起点）。
    fn lines_of(n: usize, width: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:0width$}")).collect()
    }

    // ---------- head_lines / head：单行成本 = `line.len()`（不含换行），至少保留 1 行 ----------

    #[test]
    fn head_lines_byte_limit_is_the_constraint() {
        // 5 行 × 10 字节；行数闸（10）远未触及 ⇒ 只有字节闸能截断。
        let lines = lines_of(5, 10);
        let t = head_lines(&lines, 10, 30);

        // 30 = 10 + 10 + 10，第 4 行放不下（40 > 30）。
        assert_eq!(t.total_lines, 5);
        assert_eq!(t.shown_lines, 3);
        assert_eq!(t.hit, Some(Limit::Bytes));
        assert_eq!(t.content, lines[..3].join("\n"));

        // 变异：单行成本改成 `len() + 1`（tail 的规则）→ 第 3 行 33 > 30，shown_lines 变 2 → 红。
        // 变异：字节判断 `>` 改成 `>=` → 30 >= 30，shown_lines 变 2 → 红。
    }

    #[test]
    fn head_lines_keeps_first_line_even_when_over_byte_budget() {
        // 首行 100 字节 > max_bytes(10)，仍必须出 1 行（对齐 read.rs 的 `!output_lines.is_empty()`）。
        let big = "x".repeat(100);
        let lines = vec![big.clone(), "b".to_string()];
        let t = head_lines(&lines, 10, 10);

        assert_eq!(t.shown_lines, 1);
        assert_eq!(t.content, big);
        assert_eq!(t.hit, Some(Limit::Bytes));

        // 变异：把 `shown_lines > 0 &&` 去掉 → 首行被预算挡掉，shown_lines 变 0 → 红。
    }

    #[test]
    fn head_lines_zero_byte_budget_still_keeps_one_line() {
        // 边界：字节预算为 0 时，「至少一行」规则仍然成立。
        let lines = lines_of(2, 1);
        let t = head_lines(&lines, 10, 0);

        assert_eq!(t.shown_lines, 1);
        assert_eq!(t.total_lines, 2);
        assert_eq!(t.hit, Some(Limit::Bytes));
    }

    #[test]
    fn head_lines_byte_limit_uses_real_constants() {
        // 4 行（< MAX_LINES）× 30000 字节（总量 > MAX_BYTES）⇒ 字节闸生效，只留首行。
        let lines = vec!["a".repeat(30_000); 4];
        let t = head_lines(&lines, MAX_LINES, MAX_BYTES);

        assert_eq!(t.shown_lines, 1);
        assert_eq!(t.hit, Some(Limit::Bytes));
        assert!(t.content.len() <= MAX_BYTES, "保留内容不得超过字节预算");
    }

    #[test]
    fn head_lines_exact_line_limit_is_not_truncation() {
        // 行数恰好等于 max_lines → 全部保留，hit 必须是 None（不能误报 Lines）。
        let lines = lines_of(3, 4);
        let t = head_lines(&lines, 3, 1_000);

        assert_eq!(t.total_lines, 3);
        assert_eq!(t.shown_lines, 3);
        assert_eq!(t.hit, None);
        assert_eq!(t.content, lines.join("\n"));

        // 变异：`if shown_lines < total_lines` 的守卫去掉（无条件 `Some(...)`）→ 此处 hit 变 Some → 红。
    }

    #[test]
    fn head_lines_line_limit_is_the_constraint() {
        // 字节预算极大（实际只用了 20 字节）⇒ 只有行数闸生效。
        let lines = lines_of(5, 10);
        let t = head_lines(&lines, 2, 1_000);

        assert_eq!(t.total_lines, 5);
        assert_eq!(t.shown_lines, 2);
        assert_eq!(
            t.hit,
            Some(Limit::Lines),
            "行数闸命中时 stop 为 None，回退 Lines"
        );
        assert_eq!(t.content, lines[..2].join("\n"));

        // 变异：`stop.unwrap_or(Limit::Lines)` 改成 `unwrap_or(Limit::Bytes)` → 红。
    }

    #[test]
    fn head_lines_empty_slice() {
        let lines: Vec<String> = Vec::new();
        let t = head_lines(&lines, 10, 10);

        assert_eq!(t.total_lines, 0);
        assert_eq!(t.shown_lines, 0);
        assert_eq!(t.hit, None, "空输入不是截断");
        assert_eq!(t.content, "");
    }

    #[test]
    fn head_splits_text_on_newlines() {
        // `head` = `head_lines(text.lines())`：尾随换行不产生空行，输出也不带尾随换行。
        let t = head("a\nb\nc\n", 2, 1_000);

        assert_eq!(t.total_lines, 3);
        assert_eq!(t.shown_lines, 2);
        assert_eq!(t.hit, Some(Limit::Lines));
        assert_eq!(t.content, "a\nb");
    }

    // ---------- tail：单行成本 = `line.len() + 1`（含换行），可以为 0 行 ----------

    #[test]
    fn tail_byte_limit_is_the_constraint() {
        // 关键用例：真正卡住的是**字节**而非行数。
        // 40 字节/行 ⇒ 每行成本 41 ⇒ 51200 / 41 = 1248 行，比行数闸（2000）更紧。
        // （旧测试用 3000 行 × ≤8 字节，字节闸根本没被触及，检不出 `+1` 的变异。）
        let lines = lines_of(3_000, 40);
        let text = lines.join("\n");
        let t = tail(&text, MAX_LINES, MAX_BYTES);

        assert_eq!(t.total_lines, 3_000);
        assert_eq!(
            t.shown_lines, 1_248,
            "51200 / 41 = 1248（41 = 40 字节 + 换行）"
        );
        assert_eq!(
            t.hit,
            Some(Limit::Bytes),
            "字节起点比行数起点更靠后 ⇒ 字节是约束"
        );
        assert_eq!(t.content.lines().count(), 1_248);
        assert_eq!(
            t.content.lines().next(),
            Some(lines[1_752].as_str()),
            "起点应为 3000-1248"
        );
        assert_eq!(t.content.lines().last(), Some(lines[2_999].as_str()));
        assert!(!t.content.ends_with('\n'), "慢路径 join 不带尾随换行");

        // 变异：`lines[i].len() + 1` 改成 `lines[i].len()` → 51200 / 40 = 1280 行 → 本测试变红。
    }

    #[test]
    fn tail_line_limit_is_the_constraint() {
        // 10 行共 20 字节，字节预算 1024 ⇒ 只有行数闸生效。
        let t = tail("a\nb\nc\nd\ne\nf\ng\nh\ni\nj", 3, 1_024);

        assert_eq!(t.total_lines, 10);
        assert_eq!(t.shown_lines, 3);
        assert_eq!(t.hit, Some(Limit::Lines));
        assert_eq!(t.content, "h\ni\nj");

        // 变异：`start = byte_start.max(line_start)` 改成 `byte_start` → 起点变 0 → 红。
    }

    #[test]
    fn tail_intact_fast_path_keeps_trailing_newline() {
        // 快路径：整体放得下 ⇒ 原样返回，**保留尾随换行**（与慢路径的 join 不同）。
        let t = tail("abc\ndef\n", 5, 1_024);

        assert_eq!(t.content, "abc\ndef\n");
        assert_eq!(t.hit, None);
        assert_eq!(t.total_lines, 2);
        assert_eq!(t.shown_lines, 2);

        // 变异：快路径改成 `lines.join("\n")` → 尾随换行丢失 → 红。
        // 变异：快路径前置条件 `text.len() <= max_bytes` 去掉 → 大文本走快路径 → 别的用例红。
    }

    #[test]
    fn tail_empty_input() {
        let t = tail("", 10, 1_024);

        assert_eq!(t.content, "");
        assert_eq!(t.hit, None);
        assert_eq!(t.total_lines, 0);
        assert_eq!(t.shown_lines, 0);
    }

    #[test]
    fn tail_line_over_byte_budget_gives_zero_lines() {
        // 尾行 50 字节、预算正好 50：单行成本 51（含换行）⇒ 一行都放不下 ⇒ 0 行。
        // 快路径不走（text.len() = 52 > 50），否则会掩盖这条规则。
        let text = format!("a\n{}", "x".repeat(50));
        let t = tail(&text, 10, 50);

        assert_eq!(t.total_lines, 2);
        assert_eq!(
            t.shown_lines, 0,
            "tail 可以为 0 行（head 的「至少 1 行」不适用于此）"
        );
        assert_eq!(t.hit, Some(Limit::Bytes));
        assert_eq!(t.content, "");

        // 变异：`len() + 1` 改成 `len()` → 50 <= 50 放得下 → shown_lines 变 1 → 红。
    }

    // ---------- line：按**字符**截断，不切碎 UTF-8 ----------

    #[test]
    fn line_short_is_borrowed_and_not_truncated() {
        let (out, cut) = line("hello", 10);

        assert!(!cut);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "未截断时必须借用原串（零分配）"
        );
        assert_eq!(out, "hello");

        // 变异：`None => (Cow::Owned(text.to_string()), false)` → matches! 变红。
    }

    #[test]
    fn line_exact_char_count_is_not_truncated() {
        let (out, cut) = line("abc", 3);

        assert!(!cut);
        assert_eq!(out, "abc");

        // 变异：`nth(max_chars)` 改成 `nth(max_chars - 1)` → 恰好等长也被判截断 → 红。
    }

    #[test]
    fn line_truncates_on_char_boundary_not_byte() {
        // 3 个中文字 = 9 字节。按字符取 2 个 → "中文"（6 字节）。
        // 若按字节切 `text[..2]` 会直接 panic，按字节取 2 字符也只可能得 1 个字。
        let (out, cut) = line("中文字", 2);

        assert!(cut);
        assert_eq!(out, "中文");
        assert_eq!(out.chars().count(), 2);
        assert_eq!(out.len(), 6);

        // 变异：`char_indices().nth(max_chars)` 改成 `text.split_at(max_chars)` → panic → 红。
    }

    #[test]
    fn line_zero_chars_truncates_to_empty() {
        let (out, cut) = line("abc", 0);

        assert!(cut);
        assert_eq!(out, "");
    }

    #[test]
    fn line_empty_input() {
        let (out, cut) = line("", 5);

        assert!(!cut);
        assert_eq!(out, "");
    }

    #[test]
    fn line_grep_max_chars_counts_chars_not_bytes() {
        // GREP_MAX_LINE_CHARS 是**字符**数：600 个字（1800 字节）截到 500 字符 / 1500 字节。
        let text = "中".repeat(600);
        let (out, cut) = line(&text, GREP_MAX_LINE_CHARS);

        assert!(cut);
        assert_eq!(out.chars().count(), GREP_MAX_LINE_CHARS);
        assert_eq!(out.len(), GREP_MAX_LINE_CHARS * 3);

        // 变异：把字符闸改成字节闸（`text.len() > max_chars`）→ 只剩 166 个字 → 红。
    }
}
