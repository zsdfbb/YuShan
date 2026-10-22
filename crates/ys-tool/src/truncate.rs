//! 截断的唯一实现 —— 纯函数、零依赖、无 IO。
//!
//! **分工（重要）**：本模块只计算「取哪些行 / 撞上了哪个上限」，返回 [`Truncation`]；
//! 截断结果有两型：[`Truncation`]（单段：头或尾）与 [`HeadTail`]（头尾混合）。
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
//! | [`head_tail`] | 头尾混合 | 头部同 [`head`]、尾部同 [`tail`] | 0 行（行数预算为 0 时）；否则头部至少给出**首行的前缀** |
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
/// `read` 的**默认**单次结果字节预算（C6/C7）。
///
/// **为什么 `read` 需要比 [`MAX_BYTES`] 更窄**：实测（真实会话 `1792579023_125340094`）
/// `read` 占全部工具结果字节的 **40.8%**（118 次 / 823 KB），是最大的单一贡献者；
/// 且它的截断**完全可恢复** —— 结果里自带
/// `[Showing lines X-Y of Z. Use offset=N to continue.]`，模型据此续读即可。
///
/// 反向：`bash` **不**收窄（它同样走 [`MAX_BYTES`]）—— 实测 `bash` 结果中位仅 1,221 B，
/// 50 KB 上限**从未被撞到**（0/560），收窄它省不到 3%；而构建日志的**尾部**恰恰
/// 是错误所在（`bash` 用 `tail()` 保尾），截断风险高于收益。
///
/// **必须大于 [`SOFT_RESULT_BYTES`]**：软阈值提示只在**未**硬截断时出现
/// （两者互斥），预算若等于软阈值，「结果偏大、建议收窄」这条提示对 `read` 就成死代码。
///
/// 实测影响面（16 KiB）：`read` 里约 **5%** 的调用会被截断，可省约 **13%** 的 read 字节。
/// ⚠️ **收益未经实跑 A/B 验证** —— 截断会让模型多读几次（每次一轮模型往返），
/// 净收益取决于「省下的输入 token」能否抵消「多出的轮数」。
/// 可用 [`RESULT_BUDGET_ENV`] 在不重编译的前提下做 A/B。
pub const READ_RESULT_BUDGET_BYTES: usize = 16 * 1024;

/// 覆盖 [`READ_RESULT_BUDGET_BYTES`] 的环境变量名（A/B 用）。
///
/// 值被**钳制**在 `[512, MAX_BYTES]`：设成 0 会让每次 `read` 只回一行，
/// 设成天文数字等于取消预算 —— 两者都不该由环境变量引入。
pub const RESULT_BUDGET_ENV: &str = "YUSHAN_RESULT_BUDGET_BYTES";

/// 取当前的 `read` 结果字节预算：环境变量覆盖（钳制后）→ 否则默认值。
pub fn read_result_budget() -> usize {
    std::env::var(RESULT_BUDGET_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.clamp(512, MAX_BYTES))
        .unwrap_or(READ_RESULT_BUDGET_BYTES)
}

/// **软阈值**：单条工具结果达到此字节数即值得提醒模型「结果偏大、建议收窄」。
///
/// 与 [`MAX_BYTES`]（硬闸门，超出即截断）刻意区分 —— 软阈值**不改变内容**，只影响是否附一条提示。
/// 供 `grep` / `read` 复用（文案由 [`crate::notice::size_notice`] 统一）。
pub const SOFT_RESULT_BYTES: usize = 8 * 1024;

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

/// 头尾混合截断的结果：**不含任何通知文案**（文案由调用方拼 —— 与本模块既有分工一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadTail {
    /// 头部内容：源文本的前若干行（行间以 `\n` 连接，不带尾随换行）。
    pub head: String,
    /// 尾部内容：源文本的后若干行（行间以 `\n` 连接，不带尾随换行）。
    pub tail: String,
    /// 被跳过的行数（既不在头部、也不在尾部的行）。
    pub omitted_lines: usize,
    /// 源文本中**未出现在展示内容里**的字节数。
    ///
    /// 展示内容 = [`HeadTail::head`] + [`HeadTail::tail`]（两段互不重叠）⇒ 恒等式
    /// `omitted_bytes + head.len() + tail.len() == 源文本字节数`。
    ///
    /// 头尾之间的那个换行符、以及源文本的**尾随换行**都**不在**展示内容里
    /// ⇒ 计入本值（跳过量按**源文本**记，不按展示内容记）。
    pub omitted_bytes: usize,
    /// 源文本的总行数。
    pub total_lines: usize,
    /// 实际展示的行数（头部 + 尾部）。
    pub shown_lines: usize,
}

/// 头尾混合截断：保留**头部**与**尾部**，中间段跳过并**报出跳过量**。
///
/// 用途：长输出的**两端**通常比中段更有信息量（开头是上下文、结尾是报错与汇总），
/// 但只留尾部会丢掉开头、只留开头会丢掉结尾 —— 故取两头、跳过中间。
///
/// **预算不放宽**：
/// - 字节：头部内容 ≤ `head_bytes`（且 `head_bytes` 越过 `max_bytes` 时按 `max_bytes` 算），
///   尾部内容 ≤ `max_bytes - head_bytes`，两者之和 ≤ `max_bytes`；
/// - 行数：头部与尾部的行数之和 ≤ `max_lines`。
///
/// **份额等比**：行数预算按字节份额拆分（`head_bytes / max_bytes`），
/// 使头尾在字节与行数两个维度上的份额一致。
///
/// **头尾不重叠**：尾部先定（[`tail`]），头部只能在尾部起点之前取行（[`head_lines`]）。
///
/// 字节口径沿用本模块既有约定：头部按**行内容字节**（不含行间换行符，同 [`head_lines`]），
/// 尾部按**行内容 + 换行符**（同 [`tail`]）；本函数另按**拼装后的实际字节**回收头部行，
/// 保证头部内容不超过 `head_bytes`。
///
/// **单行超预算**：首行**放不进**头部预算且行数预算不为 0 ⇒ 头部给出该行的**一个前缀**
/// （长度 ≤ 头部预算、**不切在多字节字符中间**），其余字节计入 `omitted_bytes`
/// ⇒ 模型至少看到一行内容的开头，而不是一个空结果。行数预算为 0 时**不给**
/// （预算不放宽 ⇒ 一行都不展示）。
///
/// **跳过量口径**：`omitted_bytes` = 源文本中**未出现在展示内容里**的字节数 ⇒ 恒等式
/// `omitted_bytes + head.len() + tail.len() == 源文本字节数`。
pub fn head_tail(text: &str, max_lines: usize, max_bytes: usize, head_bytes: usize) -> HeadTail {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // 整体放得下 ⇒ 原样返回：头部即全文、尾部为空，跳过量恒为 0。
    if total_lines <= max_lines && text.len() <= max_bytes {
        return HeadTail {
            head: text.to_string(),
            tail: String::new(),
            omitted_lines: 0,
            omitted_bytes: 0,
            total_lines,
            shown_lines: total_lines,
        };
    }

    // 头部预算不得越过总预算 —— 越界即等于放宽预算。
    let head_budget = head_bytes.min(max_bytes);
    let tail_budget = max_bytes - head_budget;

    // 行数预算按字节份额等比分摊：头尾各得与自身字节份额相称的行数。
    // `max_bytes == 0` ⇒ 无字节可分 ⇒ 头部行数预算为 0。
    let head_line_budget = max_lines
        .saturating_mul(head_budget)
        .checked_div(max_bytes)
        .unwrap_or(0);
    let tail_line_budget = max_lines - head_line_budget;

    // 尾部先定：尾部的起点即头部的可用区间上界 ⇒ 头尾绝不重叠。
    let tail_trunc = tail(text, tail_line_budget, tail_budget);
    let tail_start = total_lines - tail_trunc.shown_lines;

    // 头部只能在尾部起点之前取行。
    let head_line_cap = head_line_budget.min(tail_start);
    let head_trunc = head_lines(&lines[..tail_start], head_line_cap, head_budget);
    // `head_lines` 的「首行无条件保留」在行数预算为 0 时会让头部多出一行 ⇒ 这里再收一道。
    let head_shown = trim_head_to_budget(
        &lines,
        head_trunc.shown_lines.min(head_line_cap),
        head_budget,
    );

    // 首行**放不进**头部预算 ⇒ 头部给出该行的**一个前缀**（不切碎多字节字符）。
    // 行数预算为 0 时**不给**（预算不放宽 ⇒ 一行都不展示）。
    let head_is_prefix = head_shown == 0 && head_line_cap >= 1;
    let head = if head_is_prefix {
        prefix_within_budget(lines[0], head_budget).to_string()
    } else {
        lines[..head_shown].join("\n")
    };
    // 前缀只覆盖**首行的一部分** ⇒ 该行仍算「已展示」（跳过量按**字节**记）。
    let head_lines_shown = if head_is_prefix { 1 } else { head_shown };

    // 跳过量 = 源文本字节数 − 展示内容字节数（展示内容 = 头 + 尾，两段互不重叠）。
    let omitted_bytes = text
        .len()
        .saturating_sub(head.len() + tail_trunc.content.len());

    HeadTail {
        head,
        tail: tail_trunc.content,
        omitted_lines: tail_start - head_lines_shown,
        omitted_bytes,
        total_lines,
        shown_lines: head_lines_shown + tail_trunc.shown_lines,
    }
}

/// 取文本的**前缀**：长度不超过 `budget` 字节，且**不切在多字节字符中间**。
///
/// 切点回退到**字符边界** ⇒ 展示内容里不会出现半个多字节字符。
fn prefix_within_budget(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 把头部行数收到**拼装后**的字节预算之内，返回实际保留的行数。
///
/// [`head_lines`] 的字节口径**不含**行间换行符（与既有 `read.rs` 一致），
/// 故拼装结果可能超出预算；这里按「行内容 + 行间分隔符」的实际字节回收尾部行，
/// 保证头部内容不超预算（预算不放宽）。
fn trim_head_to_budget(lines: &[&str], shown_lines: usize, budget: usize) -> usize {
    let mut content_bytes = 0usize;
    for (index, line) in lines.iter().take(shown_lines).enumerate() {
        content_bytes += line.len();
        // 拼装前 `index + 1` 行的字节 = 各行内容之和 + 行间分隔符（`index` 个）。
        if content_bytes + index > budget {
            return index;
        }
    }
    shown_lines
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

    /// **不变式**：`read` 的预算必须**大于**软阈值。
    ///
    /// 软阈值提示与硬截断**互斥**（截断时出续读提示、否则出软提示）⇒
    /// 预算若 ≤ 软阈值，「结果偏大、建议收窄」这条对 `read` 就成**死代码**。
    ///
    /// 变异：把 `READ_RESULT_BUDGET_BYTES` 调到 8 KiB（= 软阈值）⇒ 变红。
    #[test]
    fn read_budget_exceeds_soft_threshold() {
        // 三个操作数都是编译期常量 ⇒ 用 `const { assert!(..) }`（与上一条软阈值不变式同形），
        // 既让约束在编译期判定，也避开 `clippy::assertions_on_constants`。
        const {
            assert!(
                READ_RESULT_BUDGET_BYTES > SOFT_RESULT_BYTES,
                "read 预算必须大于软阈值，否则软提示分支不可达"
            );
            assert!(
                READ_RESULT_BUDGET_BYTES <= MAX_BYTES,
                "read 预算不得宽于硬闸门"
            );
        }
    }

    /// `YUSHAN_RESULT_BUDGET_BYTES` 覆盖**必被钳制**在 `[512, MAX_BYTES]`。
    ///
    /// 理由：设成 `0` 会让每次 `read` 只回一行；设成天文数字等于取消预算 ——
    /// 两者都不该由环境变量引入。
    ///
    /// 变异：去掉 `.clamp(512, MAX_BYTES)` ⇒ 本用例的「0」与「超大值」两段变红。
    ///
    /// ⚠️ 本用例改**进程级环境变量**，故整段持 [`crate::test_env`] 的锁（见 `AGENTS.md`
    /// 「测试隔离」）；`ys-tool` 无该模块时为单测加本地互斥。
    #[test]
    fn read_result_budget_env_is_clamped() {
        // 本 crate 无 `test_env` 模块 ⇒ 用一把本地静音锁把同一进程内的读环境用例串起来。
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::var_os(RESULT_BUDGET_ENV);
        // SAFETY: 持有上面那把锁，且本 crate 内只有本用例读写该变量。
        unsafe {
            std::env::set_var(RESULT_BUDGET_ENV, "0");
        }
        assert_eq!(read_result_budget(), 512, "0 必须钳到下限 512");

        // SAFETY: 同上。
        unsafe {
            std::env::set_var(RESULT_BUDGET_ENV, "999999999");
        }
        assert_eq!(read_result_budget(), MAX_BYTES, "超大值必须钳到硬闸门");

        // 非法值 ⇒ 回落默认。
        // SAFETY: 同上。
        unsafe {
            std::env::set_var(RESULT_BUDGET_ENV, "abc");
        }
        assert_eq!(read_result_budget(), READ_RESULT_BUDGET_BYTES);

        // SAFETY: 同上。
        unsafe {
            match prev {
                Some(v) => std::env::set_var(RESULT_BUDGET_ENV, v),
                None => std::env::remove_var(RESULT_BUDGET_ENV),
            }
        }
    }

    /// 不变式：软阈值必须**小于**硬闸门 —— 否则「先提示、后截断」的两段语义会塌成一段。
    ///
    /// 钉住的变异：把 `SOFT_RESULT_BYTES` 调到 ≥ `MAX_BYTES` ⇒ 变红。
    ///
    /// 两个操作数都是编译期常量 ⇒ 断言在编译期即可判定（这正是我们要的：约束是常量级不变式），
    /// 故显式关掉 `clippy::assertions_on_constants`。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn soft_result_bytes_is_below_hard_cap() {
        assert!(
            SOFT_RESULT_BYTES < MAX_BYTES,
            "软阈值({SOFT_RESULT_BYTES}) 必须小于硬闸门({MAX_BYTES})"
        );
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

    // ---------- head_tail：头尾混合，头尾不重叠，跳过段必须报出 ----------

    #[test]
    fn head_tail_short_text_is_intact() {
        let text = "第一行\n第二行\n第三行";
        let t = head_tail(text, MAX_LINES, MAX_BYTES, MAX_BYTES / 4);

        assert_eq!(t.omitted_lines, 0, "短文本不应有跳过行");
        assert_eq!(t.omitted_bytes, 0, "短文本不应有跳过字节");
        assert_eq!(t.total_lines, 3);
        assert_eq!(t.shown_lines, 3);

        let joined = t.head.clone() + &t.tail;
        assert_eq!(joined, text, "头尾拼起来必须等于原文");

        // 变异：短文本也走切分路径（早退分支去掉）⇒ omitted_bytes 不再为 0 ⇒ 红。
    }

    #[test]
    fn head_tail_empty_input() {
        let t = head_tail("", MAX_LINES, MAX_BYTES, MAX_BYTES / 4);

        assert_eq!(t.head, "");
        assert_eq!(t.tail, "");
        assert_eq!(t.omitted_lines, 0);
        assert_eq!(t.omitted_bytes, 0);
        assert_eq!(t.total_lines, 0);
        assert_eq!(t.shown_lines, 0);
    }

    #[test]
    fn head_tail_is_reachable_from_crate_root() {
        // `lib.rs` 以 `pub use truncate::*;` 导出 ⇒ crate 根路径必须可用。
        let t = crate::head_tail("a\nb", 10, 100, 50);

        assert_eq!(t.total_lines, 2);
        assert_eq!(t.head, "a\nb");
        assert_eq!(t.tail, "");
    }

    #[test]
    fn head_tail_long_text_splits_budget_and_reports_omission() {
        // 3000 行 × 40 字节 ⇒ 源文本 122999 字节，远超 50 KiB 预算。
        let lines = lines_of(3_000, 40);
        let text = lines.join("\n");
        let head_budget = MAX_BYTES / 4;
        let t = head_tail(&text, MAX_LINES, MAX_BYTES, head_budget);

        // 预算不放宽：头尾内容之和不得超过总字节预算。
        assert!(
            t.head.len() + t.tail.len() <= MAX_BYTES,
            "头尾合计 {} 字节，超出预算 {}",
            t.head.len() + t.tail.len(),
            MAX_BYTES
        );
        assert!(
            t.head.len() <= head_budget,
            "头部 {} 字节，超出分给它的预算 {}",
            t.head.len(),
            head_budget
        );
        assert!(
            t.tail.len() <= MAX_BYTES - head_budget,
            "尾部 {} 字节，超出剩余预算 {}",
            t.tail.len(),
            MAX_BYTES - head_budget
        );

        // 预算切分生效：尾部吃掉 38400 / 41 = 936 行，头部吃掉 (12800 - 312) / 41 ≈ 312 行。
        assert_eq!(t.tail.lines().count(), 936, "尾部的行数由尾部字节预算决定");
        assert_eq!(t.head.lines().count(), 312, "头部的行数由头部字节预算决定");

        // 头尾不得重叠。
        let head_last = t.head.lines().last().unwrap();
        let tail_first = t.tail.lines().next().unwrap();
        let head_last_index = lines.iter().position(|l| l.as_str() == head_last).unwrap();
        let tail_first_index = lines.iter().position(|l| l.as_str() == tail_first).unwrap();
        assert!(
            head_last_index < tail_first_index,
            "头部末行必须在尾部首行之前"
        );

        // 跳过量必须**等于源文本里没出现在展示内容中的字节**（按源文本独立重算）。
        // 被跳过的每一行都带自己的行尾换行；此外**头部末行之后**还有一个换行符
        // 没有出现在展示内容里（头与尾是分开拼的）⇒ 也要计入。
        let skipped: usize = lines[head_last_index + 1..tail_first_index]
            .iter()
            .map(|line| line.len() + 1)
            .sum();
        assert_eq!(t.omitted_lines, tail_first_index - head_last_index - 1);
        assert_eq!(
            t.omitted_bytes,
            skipped + 1,
            "跳过的字节数必须等于源文本里没出现在展示内容中的字节"
        );

        // 恒等式：源文本字节 = 展示内容字节 + 跳过量。
        assert_eq!(
            text.len(),
            t.head.len() + t.omitted_bytes + t.tail.len(),
            "源文本字节必须被展示内容与跳过量完整瓜分"
        );

        // 变异：把 `omitted_bytes` 恒置 0 ⇒ 上面两条断言变红。
        // 变异：头尾取行区间重叠（头部不再限制在尾部起点之前）⇒ 头尾不重叠断言变红。
    }

    #[test]
    fn head_tail_never_exceeds_the_byte_budget() {
        // 单行 100000 字节：整行都放不进任何一份预算 ⇒ 头部给出该行的**一个前缀**，
        // 尾部让位（同一行不得同时出现在头与尾），跳过量如实报出。
        let text = "x".repeat(100_000);
        let head_budget = MAX_BYTES / 4;
        let t = head_tail(&text, MAX_LINES, MAX_BYTES, head_budget);

        assert!(
            t.head.len() <= head_budget,
            "头部 {} 字节，超出分给它的预算 {}",
            t.head.len(),
            head_budget
        );
        assert_eq!(t.head, "x".repeat(head_budget), "前缀必须落在首行之内");
        assert!(t.tail.is_empty(), "同一行不得同时出现在头与尾");
        assert_eq!(t.total_lines, 1);
        assert_eq!(t.shown_lines, 1, "首行（部分）已出现在展示内容里");
        assert_eq!(t.omitted_lines, 0, "该行已（部分）出现 ⇒ 不算整行被跳过");
        assert_eq!(
            t.omitted_bytes,
            text.len() - head_budget,
            "跳过的字节数必须等于没出现在展示内容里的字节"
        );

        // 恒等式：源文本字节 = 展示内容字节 + 跳过量。
        assert_eq!(text.len(), t.head.len() + t.omitted_bytes + t.tail.len());

        // 变异：单行超预算时退回「不返回任何内容」⇒ 前缀与跳过量断言变红。
        // 变异：头部保留超预算的整行（`head_lines` 的「首行无条件保留」被照搬进来）⇒ 红。
    }

    /// 单行超预算时的前缀必须**落在字符边界上** —— 半个多字节字符会让模型看到乱码。
    #[test]
    fn head_tail_gives_a_character_safe_prefix_of_an_oversized_single_line() {
        // 20000 个中文字 = 60000 字节（单行，无换行）⇒ 整行放不进头部预算。
        let text = "中".repeat(20_000);
        let head_budget = MAX_BYTES / 4;
        let t = head_tail(&text, MAX_LINES, MAX_BYTES, head_budget);

        assert!(!t.head.is_empty(), "单行超预算也必须给出前缀");
        assert!(t.head.len() <= head_budget, "前缀不得超过头部预算");
        assert!(!t.head.contains('\u{FFFD}'), "前缀不得切在多字节字符中间");
        assert!(text.starts_with(&t.head), "前缀必须落在首行之内");
        assert_eq!(t.omitted_bytes + t.head.len() + t.tail.len(), text.len());
    }

    /// **尾随换行**没有出现在展示内容里 ⇒ 计入跳过量（源文本按字节记账）。
    #[test]
    fn head_tail_counts_the_trailing_newline_as_omitted() {
        // 5 行 × 8 字节 + 行间换行 + **尾随换行** = 45 字节。
        let lines = lines_of(5, 8);
        let mut text = lines.join("\n");
        text.push('\n');
        let t = head_tail(&text, 2, 24, 12);

        // 跳过量 = 3 个被跳过的行（各 9 字节）+ 头部末行之后的换行符 + 尾随换行。
        assert_eq!(t.omitted_lines, 3);
        assert_eq!(
            t.omitted_bytes, 29,
            "尾随换行必须计入跳过量：3×9 + 1（头后换行）+ 1（尾随换行）"
        );

        // 去掉尾随换行 ⇒ 跳过量恰好少 1 字节（差 1 字节的账目洞）。
        let without = lines.join("\n");
        let t2 = head_tail(&without, 2, 24, 12);
        assert_eq!(t2.omitted_bytes, 28);

        // 恒等式：源文本字节 = 展示内容字节 + 跳过量。
        assert_eq!(text.len(), t.head.len() + t.omitted_bytes + t.tail.len());
        assert_eq!(
            without.len(),
            t2.head.len() + t2.omitted_bytes + t2.tail.len()
        );
    }

    #[test]
    fn head_tail_line_budget_is_not_loosened() {
        // 3000 行 × 4 字节 ⇒ 源文本 14999 字节（< 50 KiB）⇒ 只有行数闸生效。
        let lines = lines_of(3_000, 4);
        let text = lines.join("\n");
        let t = head_tail(&text, MAX_LINES, MAX_BYTES, MAX_BYTES / 4);

        assert_eq!(t.total_lines, 3_000);
        assert_eq!(t.shown_lines, MAX_LINES, "头尾行数之和不得超过行数预算");
        assert_eq!(t.omitted_lines, 1_000, "跳过的行必须被报出");
        assert!(t.omitted_bytes > 0, "跳过的字节必须被报出");

        // 行数预算为 0 ⇒ 一行都不展示（预算不放宽）。
        let t0 = head_tail(&text, 0, MAX_BYTES, MAX_BYTES / 4);
        assert_eq!(t0.shown_lines, 0, "行数预算为 0 ⇒ 不得展示任何行");
        assert_eq!(t0.omitted_lines, 3_000);

        // 变异：行数预算按字节份额拆分这一条去掉（头尾各拿满 max_lines）⇒ 红。
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        // 中间段被跳过，但开头与结尾都留着。
        let lines: Vec<String> = (0..100).map(|i| format!("line{i}")).collect();
        let text = lines.join("\n");
        let t = head_tail(&text, 10, 100, 40);

        assert!(t.head.starts_with("line0"), "开头必须在");
        assert!(t.tail.ends_with("line99"), "结尾必须在");
        assert!(t.omitted_lines > 0, "中间段必须被跳过并报出");

        // 变异：只取尾部（头部恒空）⇒ 开头断言变红。
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
