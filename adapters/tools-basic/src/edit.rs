//! `edit` 工具：在同一份原文上做一次或多次定点替换。
//!
//! **匹配口径**（每条 edit 各自选空间）：先按原始字节精确查找；精确命中就在**精确空间**里
//! 计数唯一性，精确找不到才在[归一化视图](NormalizedView)里查找并按**归一化空间**计数与
//! 报 notice。计数与命中永远同一口径 —— 精确唯一的上文不会因同批别处走了归一化而变成
//! 「歧义」，归一化命中也不会漏报。归一化**逐行只去行尾空白**、不做整体 `trim`：行首缩进
//! 是匹配语义的一部分。
//!
//! **落回原文**：视图带「归一化区间 ↔ 原文区间」映射，归一化空间的命中端点一次性映射回
//! 原文，替换在**原文空间**里做。命中区间之外的字节（同行的弯引号、零宽字符、行尾空白）
//! 逐字节保留 —— 归一化只影响匹配，从不改写文件。
//!
//! **兼容字符不切半用**：NFKC 展开成多个字符的簇（`ﬁ` → `fi`、`½` → `1⁄2`）是 atomic 的。
//! 命中端点落在簇**内部**时报错并提示把整个字符写进 `oldText`，而不是整簇替换 —— 后者会
//! 吞掉同簇其余的归一化字符，破坏「区间之外逐字节保留」。
//!
//! **无改动即错误**：落盘字节与读入字节相同、或 `oldText` 与 `newText` 本身相同（替换请求
//! 没有提出任何改动），都报错且不写文件。判据只在**原文/请求**层面，不在归一化空间里
//! ——`"foo\nbar"` → `"foo \nbar"` 是真实的空白改写，必须成功。
//!
//! **被并入的行尾空白要揭示**：归一化命中的区间可能连带命中行尾空白（Markdown 的行尾两空格
//! 即 `<br>`），它们随 `newText` 一起消失，因此结果里必须点名，模型才能察觉。
//!
//! **换行**：读入先把 `\r\n` / `\r` 归一为 LF 再匹配；写回按**第一个换行符**判定的风格
//! （`\r\n` / `\r` / `\n`）统一还原。先在 LF 空间里完成全部编辑，是「绝不产生 `\r\r\n`」的前提。

use serde_json::{Value, json};
use std::borrow::Cow;
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{canonical_combining_class, compose};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use crate::workspace_path::{absolute_notice, attach_notice};

pub struct EditTool {
    workspace: PathBuf,
}

impl EditTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

// ---------------------------------------------------------------------------
// 文本规范化
// ---------------------------------------------------------------------------

/// 单字符映射：NFKC 之后的智能引号 / 破折号 / 特殊空格 → ASCII 等价物。
///
/// ASCII 输入恒等（NFKC 对 ASCII 稳定，且 ASCII 不在任何映射表里）。
fn map_char(ch: char) -> char {
    match ch {
        // 单引号 → '
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        // 双引号 → "
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // 各种连字符 / 破折号 / 减号 → -
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // 各种特殊空格 → 普通空格
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        _ => ch,
    }
}

/// `next` 是否应并入以 `cluster` 结尾的这一簇 —— 即 NFKC 会把两者合成为一个字符。
///
/// 组合字符（`canonical_combining_class != 0`）必然并入；此外只有合成序列（例如谚文字母
/// L + V + T、字母 + 重音的组合形式）会并入。ASCII 不可能是任何归一化等价物的后半部分。
fn joins_cluster(cluster: &str, next: char) -> bool {
    if next.is_ascii() {
        return false;
    }
    if canonical_combining_class(next) != 0 {
        return true;
    }
    let last = cluster.nfkc().map(map_char).last();
    last.is_some_and(|last| compose(last, next).is_some())
}

/// 一行正文（不含 `\n`）的归一化单元，逐个交给 `emit(ch, 原文区间)`。
///
/// 只服务 [`normalize_for_fuzzy_match`]（短串路径）；文件内容走 [`NormalizedView::new`]，
/// 那里对纯 ASCII 行有整行快路径。两处的单元语义相同：每个单元的原文区间要么是单个字符，
/// 要么是整簇（起始字符 + 其后的组合字符），都起止于字符边界；**行尾空白不产出单元** ——
/// 它不在归一化内容里，因此永远不属于任何命中区间。
///
/// 整串 NFKC 与逐簇 NFKC 等价（合成只发生在簇内），因此这样产出的归一化内容与
/// [`normalize_for_fuzzy_match`] 逐字节相同。
fn emit_line_units(body: &str, base: usize, emit: &mut impl FnMut(char, usize, usize)) {
    if body.is_ascii() {
        // ASCII 恒等：直接逐字节产出，无需 NFKC 与簇判定
        let trimmed = body.trim_end();
        for (index, byte) in trimmed.bytes().enumerate() {
            emit(byte as char, base + index, base + index + 1);
        }
        return;
    }

    let mut units: Vec<Unit> = Vec::new();
    collect_units(body, base, &mut units);
    for &(ch, start, end, _) in &units {
        emit(ch, start, end);
    }
}

/// 一个归一化单元：归一化后的字符 + 它在原文里的区间 + 所属簇是否 atomic。
///
/// 同一 NFKC 簇展开出的多个单元共享同一个原文区间。
///
/// `atomic` 表示该簇展开后**不止一个**字符（兼容字符：`ﬁ` → `fi`、`½` → `1⁄2`、`℃` → `°C`）。
/// 这类簇的原文区间无法只覆盖其中一个展开字符，因此命中端点落在簇内部时不能替换。
type Unit = (char, usize, usize, bool);

/// 收集一行正文（不含 `\n`）的归一化单元，**已去掉行尾空白**。
///
/// 单元顺序即输出顺序；每个单元的原文区间落在字符边界上。
fn collect_units(body: &str, base: usize, out: &mut Vec<Unit>) {
    let mut cluster_start = 0;
    let mut cluster_end = 0;
    let push_cluster = |out: &mut Vec<Unit>, start: usize, end: usize| {
        let first = out.len();
        for ch in body[start..end].nfkc() {
            out.push((map_char(ch), base + start, base + end, false));
        }
        if out.len() - first > 1 {
            for unit in &mut out[first..] {
                unit.3 = true;
            }
        }
    };
    for (index, ch) in body.char_indices() {
        if cluster_end > cluster_start && joins_cluster(&body[cluster_start..cluster_end], ch) {
            cluster_end = index + ch.len_utf8();
            continue;
        }
        if cluster_end > cluster_start {
            push_cluster(out, cluster_start, cluster_end);
        }
        cluster_start = index;
        cluster_end = index + ch.len_utf8();
    }
    if cluster_end > cluster_start {
        push_cluster(out, cluster_start, cluster_end);
    }

    // 逐行只去行尾空白（在**归一化字符**上判定：NBSP 之类的特殊空格先变成普通空格）
    while out.last().is_some_and(|(ch, _, _, _)| ch.is_whitespace()) {
        out.pop();
    }
}

/// 遍历整份内容的归一化单元：`(归一化字符, 原文起点, 原文终点)`，行序与换行不变。
///
/// 入参必须是 LF 归一化后的内容。这是**唯一**的归一化实现 —— 归一化内容、命中定位、原文
/// 映射都由它派生，三者不可能漂移。
fn for_each_unit(original: &str, mut emit: impl FnMut(char, usize, usize)) {
    let mut offset = 0;
    for raw_line in original.split_inclusive('\n') {
        let (body, has_newline) = match raw_line.strip_suffix('\n') {
            Some(body) => (body, true),
            None => (raw_line, false),
        };
        emit_line_units(body, offset, &mut emit);
        if has_newline {
            emit('\n', offset + body.len(), offset + body.len() + 1);
        }
        offset += raw_line.len();
    }
}

/// 归一化空间的内容：单字符规范化 + **逐行**去行尾空白。
///
/// 只去行尾是刻意的：行首缩进参与匹配，整体 `trim` 会让「带缩进的 `oldText`」与
/// 「不带缩进的 `oldText`」混为一谈。只用于短串（`oldText`）；文件内容走 [`NormalizedView`]。
fn normalize_for_fuzzy_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for_each_unit(s, |ch, _, _| out.push(ch));
    out
}

/// 把 `\r\n` 与孤立的 `\r` 统一折叠为 `\n`；此后内容中不再含 `\r`。
///
/// 无 `\r` 时不分配（LF 文件占绝对多数，正文动辄 MiB 级）。
fn normalize_to_lf(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// 非重叠出现次数（歧义报错要给出真实次数，模型据此才能判断加了上下文是否够）。
///
/// `needle` 为空时返回 0：空 needle 的「出现次数」没有定义，调用方必须先行拒绝它。
fn count_occurrences(content: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut rest = content;
    while let Some(offset) = rest.find(needle) {
        count += 1;
        rest = &rest[offset + needle.len()..];
    }
    count
}

// 视图构造次数计数器（**仅测试构建**）。
//
// 性能契约的护栅：精确命中不走归一化，一批模糊 edit 只建一次视图。按线程计数（每个测试
// 各占一线程，且 `#[tokio::test]` 默认为单线程运行时），故并行测试间互不干扰。
// 变异 `always_build_view` / `rebuild_view_per_edit` 会把它打红。
#[cfg(test)]
thread_local! {
    static VIEW_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_view_build() {
    VIEW_BUILDS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn view_builds() -> usize {
    VIEW_BUILDS.with(std::cell::Cell::get)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
    /// 孤立 `\r`（经典 Mac 风格）。单独一份，否则 `\r`-only 文件落盘时整文件换行风格被改。
    Cr,
}

/// 行尾风格：由**第一个**换行符决定。
///
/// `\r\n` 与 `\n` 按首字节位置比较。**孤立的 `\r` 只在全文没有任何 `\n` 时才算风格候选** ——
/// 否则 CSV 字段内嵌回车、Windows 工具输出、源码字符串字面量里的 `\r` 都会把整文件的
/// 换行风格判成 CR，落盘时把全文换行改写。
fn detect_line_ending(content: &str) -> LineEnding {
    let mut first: Option<(usize, LineEnding)> = None;
    let mut consider = |at: usize, ending: LineEnding| {
        if first.is_none_or(|(best, _)| at < best) {
            first = Some((at, ending));
        }
    };
    if let Some(at) = content.find("\r\n") {
        consider(at, LineEnding::Crlf);
    }
    if let Some(at) = content.find('\n') {
        consider(at, LineEnding::Lf);
    }
    if !content.contains('\n')
        && let Some(at) = content.find('\r')
    {
        consider(at, LineEnding::Cr);
    }
    first.map_or(LineEnding::Lf, |(_, ending)| ending)
}

/// 按检测到的风格还原换行。LF 档不复制。
///
/// 入参必须是 [`normalize_to_lf`] 的产物（不含 `\r`），因此这里的 `\n` → `\r\n` / `\n` → `\r`
/// 不会与既有 `\r` 叠加。
fn restore_line_endings(text: &str, ending: LineEnding) -> Cow<'_, str> {
    match ending {
        LineEnding::Crlf => Cow::Owned(text.replace('\n', "\r\n")),
        LineEnding::Cr => Cow::Owned(text.replace('\n', "\r")),
        LineEnding::Lf => Cow::Borrowed(text),
    }
}

/// 剥离（并报告）文件开头的 BOM。模型不会在 `oldText` 里带不可见的 BOM。
fn split_bom(content: &str) -> (bool, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => (true, rest),
        None => (false, content),
    }
}

// ---------------------------------------------------------------------------
// 归一化视图
// ---------------------------------------------------------------------------

/// 一段归一化内容里连续的一段「归一化区间 ↔ 原文区间」。
///
/// `direct` 为 true 表示这段里原文与归一化**逐字节相同**，因此 `orig_start + (pos -
/// norm_start)` 是精确的偏移映射；为 false 时这段只含一个归一化单元（单个字符或一个 NFKC
/// 簇的一个展开字符），映射一律取整段边界，保证端点绝不落进字符内部。
///
/// `atomic` 表示这个单元属于一个**展开成多个字符**的 NFKC 簇（`ﬁ` → `fi`）。同簇的多个单元
/// 共享同一个原文区间；该区间无法只覆盖其中一个展开字符，因此命中只能覆盖整簇或完全不碰。
///
/// 相邻的直通单元合并成长运行，因此运行表规模与**行数**同阶（纯 ASCII 正文整行一个运行），
/// 而不是与字节数同阶。
#[derive(Debug, Clone, Copy)]
struct Run {
    norm_start: usize,
    norm_end: usize,
    orig_start: usize,
    orig_end: usize,
    direct: bool,
    atomic: bool,
}

/// `find` 的三种结果。
///
/// `PartialCluster` 是**拒绝**而不是降级：命中只覆盖了兼容字符簇的一部分（`ﬁ` 里的 `f`），
/// 整簇替换会吞掉同簇其余字节，而按半簇切片又会把字符切碎 —— 唯一正确的动作是报错让模型
/// 把整个字符写进 `oldText`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuzzyFind {
    /// 命中区间（原文空间）。
    Hit {
        start: usize,
        end: usize,
    },
    /// 命中端点落在兼容字符簇内部；给出该簇的原文区间。
    PartialCluster {
        start: usize,
        end: usize,
    },
    Miss,
}

/// 一份内容的归一化视图：归一化内容 + 把归一化区间映射回原文的运行表。
///
/// 构造是**一次** O(n) 顺扫，同一次调用里的多条 edit 复用它。精确命中的 edit 完全不触发
/// 构造 —— 精确路径没有任何全量归一化开销。
struct NormalizedView {
    normalized: String,
    runs: Vec<Run>,
    /// 被逐行去尾去掉的空白在原文里的区间，升序。
    ///
    /// 这些字节不在归一化内容里，因此永远不属于任何 `oldText` 的命中区间，但它们可能**落在**
    /// 一个跨行命中的原文区间之内（例如 `oldText` 以 `\n` 收尾时）。命中时据此揭示被命中的它们。
    trimmed_whitespace: Vec<(usize, usize)>,
}

/// 登记刚追加的那段归一化字节 ↔ 原文区间 `orig` 的映射关系。
///
/// `norm_len` 是**刚追加**的字节数（`normalized` 已经包含它）。与上一段同为直通且首尾相接时
/// 延长上一段，而不是新增一项 —— 纯 ASCII 正文因此整行只占一个 `Run`。`atomic` 段不参与合并
/// （簇的每个展开字符必须各自留一个运行，才能识别出「命中落在簇内部」）。
fn register_run(
    runs: &mut Vec<Run>,
    normalized: &str,
    original: &str,
    orig: (usize, usize),
    norm_len: usize,
    atomic: bool,
) {
    let norm_end = normalized.len();
    let norm_start = norm_end - norm_len;
    let direct =
        orig.1 - orig.0 == norm_len && original[orig.0..orig.1] == normalized[norm_start..];
    if direct
        && let Some(last) = runs.last_mut()
        && last.direct
        && last.orig_end == orig.0
        && last.norm_end == norm_start
    {
        last.orig_end = orig.1;
        last.norm_end = norm_end;
        return;
    }
    runs.push(Run {
        norm_start,
        norm_end,
        orig_start: orig.0,
        orig_end: orig.1,
        direct,
        atomic,
    });
}

impl NormalizedView {
    /// 建视图。`original` 必须是 LF 归一化后的内容（视图不改换行语义）。
    ///
    /// 逐行构建：纯 ASCII 行整行一次追加、登记成一段直通运行；只有含非 ASCII 的行才逐单元
    /// 展开。因此运行表规模与**行数**同阶，而不是与字节数同阶。
    fn new(original: &str) -> Self {
        #[cfg(test)]
        note_view_build();
        let mut normalized = String::with_capacity(original.len());
        let mut runs: Vec<Run> = Vec::new();
        let mut units: Vec<Unit> = Vec::new();
        let mut trimmed_whitespace: Vec<(usize, usize)> = Vec::new();

        let mut offset = 0;
        for raw_line in original.split_inclusive('\n') {
            let (body, has_newline) = match raw_line.strip_suffix('\n') {
                Some(body) => (body, true),
                None => (raw_line, false),
            };

            if body.is_ascii() {
                // ASCII 恒等：整行一次追加（行尾空白不进视图）
                let trimmed = body.trim_end();
                if trimmed.len() < body.len() {
                    trimmed_whitespace.push((offset + trimmed.len(), offset + body.len()));
                }
                if !trimmed.is_empty() {
                    normalized.push_str(trimmed);
                    register_run(
                        &mut runs,
                        &normalized,
                        original,
                        (offset, offset + trimmed.len()),
                        trimmed.len(),
                        false,
                    );
                }
            } else {
                units.clear();
                collect_units(body, offset, &mut units);
                for &(ch, start, end, atomic) in &units {
                    normalized.push(ch);
                    register_run(
                        &mut runs,
                        &normalized,
                        original,
                        (start, end),
                        ch.len_utf8(),
                        atomic,
                    );
                }
                // 非 ASCII 行的尾尾空白：最后一个单元之后到行尾（无单元时即整行）
                let kept_end = units.last().map_or(0, |&(_, _, end, _)| end - offset);
                if kept_end < body.len() {
                    trimmed_whitespace.push((offset + kept_end, offset + body.len()));
                }
            }

            if has_newline {
                normalized.push('\n');
                let at = offset + body.len();
                register_run(&mut runs, &normalized, original, (at, at + 1), 1, false);
            }
            offset += raw_line.len();
        }

        Self {
            normalized,
            runs,
            trimmed_whitespace,
        }
    }

    /// 归一化空间里 `needle`（已归一化）的出现次数，与 [`Self::find`] 同一空间。
    fn count(&self, needle: &str) -> usize {
        count_occurrences(&self.normalized, needle)
    }

    /// 归一化空间里 `needle` 的第一处命中。
    ///
    /// 空 needle 返回 [`FuzzyFind::Miss`]（调用方必须先行拒绝纯空白 needle）。命中端点在
    /// 归一化空间里贴合，因此映射回原文后区间绝不会落进字符内部。
    ///
    /// 区间是**最小覆盖**：两端各扩到所属单元的边界。因此命中跨行（needle 以 `\n` 收尾）时，
    /// 行尾被去掉的空白会落进区间 —— 它是「命中那段原文」的一部分，随 `newText` 一起被替换，
    /// 而不是留给下游变成孤立残留。被并入的空白由 [`Self::removed_trailing_whitespace`] 报告。
    ///
    /// 命中只盖住兼容字符簇的一部分时返回 [`FuzzyFind::PartialCluster`]：该类簇的原文区间无法
    /// 只覆盖其中一个展开字符（`ﬁ` 的 `f` 与 `i` 共享同一段原文），整簇替换会吞掉同簇其余字节。
    fn find(&self, needle: &str) -> FuzzyFind {
        if needle.is_empty() {
            return FuzzyFind::Miss;
        }
        let Some(norm_start) = self.normalized.find(needle) else {
            return FuzzyFind::Miss;
        };
        let norm_end = norm_start + needle.len();
        let (Some(start_run), Some(end_run)) = (
            self.run_index_at(norm_start),
            self.run_index_at(norm_end - 1),
        ) else {
            return FuzzyFind::Miss;
        };

        for index in [start_run, end_run] {
            let run = self.runs[index];
            if !run.atomic {
                continue;
            }
            // 端点落在簇内部 ⇒ 按半簇替换会吞掉（或切碎）同簇其余字节。端点贴合簇边界则
            // 命中要么盖住整簇、要么完全不碰，安全。
            let (cluster_start, cluster_end) = self.cluster_bounds(index);
            if cluster_start < norm_start || cluster_end > norm_end {
                return FuzzyFind::PartialCluster {
                    start: run.orig_start,
                    end: run.orig_end,
                };
            }
        }

        let Some(from) = self.orig_start_at(norm_start) else {
            return FuzzyFind::Miss;
        };
        let to = self.orig_end_at(norm_end).unwrap_or(from).max(from);
        FuzzyFind::Hit {
            start: from,
            end: to,
        }
    }

    /// 命中区间 `[start, end)` 内被归一化去掉的**行尾空白**（原文区间，按出现顺序）。
    ///
    /// 这些字节会随 `newText` 一起消失，但它们不在 `oldText` 的重现里 —— 不揭示就是静默改文件。
    fn removed_trailing_whitespace(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        self.trimmed_whitespace
            .iter()
            .copied()
            .filter(|(from, to)| *from >= start && *to <= end)
            .collect()
    }

    /// 归一化位置 `pos` 所在运行的索引（`run.norm_start <= pos < run.norm_end`）。
    fn run_index_at(&self, pos: usize) -> Option<usize> {
        let index = self.runs.partition_point(|run| run.norm_end <= pos);
        let run = self.runs.get(index)?;
        (run.norm_start <= pos).then_some(index)
    }

    /// 含第 `index` 个运行的 **NFKC 展开簇**在归一化空间的区间（非 atomic 单元即自身）。
    ///
    /// 同簇的所有运行共享同一段原文区间且首尾相接。命中必须要么盖整簇、要么完全不碰。
    fn cluster_bounds(&self, index: usize) -> (usize, usize) {
        let mut first = index;
        let mut last = index;
        let anchor = self.runs[index];
        if anchor.atomic {
            while first > 0 {
                let prev = self.runs[first - 1];
                if !prev.atomic
                    || prev.orig_start != anchor.orig_start
                    || prev.orig_end != anchor.orig_end
                    || prev.norm_end != self.runs[first].norm_start
                {
                    break;
                }
                first -= 1;
            }
            while last + 1 < self.runs.len() {
                let next = self.runs[last + 1];
                if !next.atomic
                    || next.orig_start != anchor.orig_start
                    || next.orig_end != anchor.orig_end
                    || next.norm_start != self.runs[last].norm_end
                {
                    break;
                }
                last += 1;
            }
        }
        (self.runs[first].norm_start, self.runs[last].norm_end)
    }

    /// 归一化位置 `pos` 所在单元的**原文起点**。
    ///
    /// 直通运行按偏移内插；非直通（单字符或 NFKC 簇）取整段起点。
    fn orig_start_at(&self, pos: usize) -> Option<usize> {
        let index = self.runs.partition_point(|run| run.norm_end <= pos);
        let run = self.runs.get(index)?;
        if run.norm_start > pos {
            return None;
        }
        Some(if run.direct {
            run.orig_start + (pos - run.norm_start)
        } else {
            run.orig_start
        })
    }

    /// 归一化位置 `pos` **之前**那个单元（`norm_start < pos <= norm_end`）的原文终点。
    ///
    /// 右端点用它映射：`pos` 恰为单元终点时不吞掉其后被去掉的行尾空白。
    fn orig_end_at(&self, pos: usize) -> Option<usize> {
        if pos == 0 {
            return None;
        }
        let index = self.runs.partition_point(|run| run.norm_start < pos);
        let run = self.runs.get(index.checked_sub(1)?)?;
        Some(if run.direct {
            run.orig_start + (pos - run.norm_start)
        } else {
            run.orig_end
        })
    }
}

// ---------------------------------------------------------------------------
// 匹配
// ---------------------------------------------------------------------------

/// 一条 edit 的命中结果（`edit_index` 是它在入参 `edits[]` 里的序号，报错要用）。
///
/// 区间一律落在**原文空间**。`fuzzy` 记录这条 edit 最终走的是哪个空间：为 true 时结果里必须
/// 告知模型，否则它会以为自己精确命中，继续基于错误假设改文件。
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_len: usize,
    new_text: String,
    fuzzy: bool,
    /// 命中区间内被归一化去掉的行尾空白（原文片段）。非 fuzzy 命中的此项恒为空。
    removed_whitespace: Vec<String>,
}

impl MatchedEdit {
    /// 命中区间在原文上的切片（用于 notice 的取样）。
    fn matched_slice<'a>(&self, content: &'a str) -> &'a str {
        self.match_index
            .checked_add(self.match_len)
            .and_then(|end| content.get(self.match_index..end))
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// 应用
// ---------------------------------------------------------------------------

/// 应用结果：新内容 + 条条命中需要告知模型的细节。
struct AppliedEdits {
    new_content: String,
    /// 需要报 notice 的 edit，按 `edits[]` 顺序。
    notices: Vec<EditNotice>,
}

/// 一条 edit 需要告知模型的事实：走了归一化命中，以及因此被吞掉的行尾空白。
///
/// 两件事都必须说：归一化命中不说，模型会以为自己精确命中；被吞的空白不说（Markdown 的行尾
/// 两空格就是 `<br>`），下游会看到一份与预期不同的文本却没有人提过。
struct EditNotice {
    edit_index: usize,
    matched: String,
    /// 命中区间内被归一化去掉的行尾空白（原文片段，按出现顺序）。
    removed_whitespace: Vec<String>,
}

fn not_found_error(path: &str, index: usize, total: usize, old_text: &str) -> ToolError {
    let guidance = "oldText must match exactly including all whitespace and newlines";
    ToolError::Execution(if total == 1 {
        format!("oldText not found in {path}: {old_text:?} ({guidance})")
    } else {
        format!("edits[{index}] not found in {path}: {old_text:?} ({guidance})")
    })
}

fn duplicate_error(
    path: &str,
    index: usize,
    total: usize,
    occurrences: usize,
    old: &str,
) -> ToolError {
    ToolError::Execution(if total == 1 {
        format!(
            "oldText matches {occurrences} times in {path}, must match exactly once: {old:?}. \
             Add more surrounding context to make it unique."
        )
    } else {
        format!(
            "edits[{index}] matches {occurrences} times in {path}, must match exactly once: {old:?}. \
             Add more surrounding context to make it unique."
        )
    })
}

fn empty_old_text_error(path: &str, index: usize, total: usize) -> ToolError {
    ToolError::Execution(if total == 1 {
        format!("oldText must not be empty in {path}")
    } else {
        format!("edits[{index}].oldText must not be empty in {path}")
    })
}

/// 归一化后为空的 needle 只由空白构成，无法定位：拒绝并给出可自修的提示，而不是挑第一处改。
fn blank_old_text_error(path: &str, index: usize, total: usize, old: &str) -> ToolError {
    let guidance = format!(
        "contains only whitespace ({old:?}), so it cannot identify a unique location. Include \
         the surrounding non-blank text and retry"
    );
    ToolError::Execution(if total == 1 {
        format!("oldText in {path} {guidance}")
    } else {
        format!("edits[{index}].oldText in {path} {guidance}")
    })
}

fn no_change_error(path: &str, total: usize) -> ToolError {
    ToolError::Execution(if total == 1 {
        format!(
            "No changes made to {path}: newText leaves the matched text identical. \
             Check for special characters or a newText that repeats oldText."
        )
    } else {
        format!("No changes made to {path}: the replacements produced identical content.")
    })
}

/// 命中只盖住兼容字符簇的一部分（`ﬁ` 里的 `f`）时的错误。
///
/// 整簇替换会吞掉同簇的其余字节，半簇切片又会把字符切碎 —— 只能让模型把整个字符写进
/// `oldText`，否则它无从知道命中的不是它以为的那个字符。
fn partial_cluster_error(
    path: &str,
    index: usize,
    total: usize,
    old: &str,
    cluster: &str,
) -> ToolError {
    let codepoints = cluster
        .chars()
        .map(|ch| format!("U+{:04X}", ch as u32))
        .collect::<Vec<_>>()
        .join(" ");
    let subject = if total == 1 {
        format!("oldText {old:?} in {path}")
    } else {
        format!("edits[{index}].oldText {old:?} in {path}")
    };
    ToolError::Execution(format!(
        "{subject} matched only part of compatibility character {cluster:?} ({codepoints}) \
         after NFKC expansion; include the whole character in oldText"
    ))
}

fn invalid_range_error(path: &str, edit_index: usize, start: usize, len: usize) -> ToolError {
    ToolError::Execution(format!(
        "failed to apply edits in {path}: edits[{edit_index}] replacement range {start}..+{len} \
         is invalid"
    ))
}

/// 在**原文空间**里应用全部命中：只替换命中区间，区间前后的字节逐字节保留。
///
/// 调用方保证 `matched` 按 `match_index` 升序、区间互不重叠。切片端点来自 `str::find` 与原文
/// 映射，正常路径必然落在字符边界上；不满足时返回错误而不是 panic。
fn apply_matched(original: &str, matched: &[MatchedEdit], path: &str) -> Result<String, ToolError> {
    let mut result = String::with_capacity(original.len());
    let mut cursor = 0;
    for edit in matched {
        let start = edit.match_index;
        let end = start
            .checked_add(edit.match_len)
            .ok_or_else(|| invalid_range_error(path, edit.edit_index, start, edit.match_len))?;
        if start < cursor
            || end > original.len()
            || !original.is_char_boundary(start)
            || !original.is_char_boundary(end)
        {
            return Err(invalid_range_error(
                path,
                edit.edit_index,
                start,
                edit.match_len,
            ));
        }
        result.push_str(&original[cursor..start]);
        result.push_str(&edit.new_text);
        cursor = end;
    }
    result.push_str(&original[cursor..]);
    Ok(result)
}

/// 在一份 **LF 归一化**的内容上应用全部 `edits`。
///
/// 顺序是强制的：先校验全部 `oldText` 非空 → 逐条在自己的空间里定位（先精确、精确 miss 才
/// 归一化）、并按**同一空间**计数唯一性 → 排序查重叠 → 统一在原文空间里替换。任何一步失败
/// 都返回错误，文件不会被写。
///
/// 纯空白 `oldText` 的拒绝在**精确 miss 之后**：「原文恰好只有一个 tab」这种请求是精确唯一且
/// 完全可判定的，一律拒绝会把「缩进从 tab 改成空格」这类正常编辑变成无法表达。
fn apply_edits_to_normalized_content(
    content: &str,
    edits: &[(String, String)],
    path: &str,
) -> Result<AppliedEdits, ToolError> {
    let total = edits.len();
    // LF 归一化后的 edits + oldText 的归一化形态。
    //
    // `oldText` 是模型给的短串，归一化它没有成本；**内容**的归一化才是贵的，而它只由
    // [`NormalizedView`] 承担、且只在某条 edit 精确找不到时才构造。
    let prepared: Vec<(Cow<'_, str>, Cow<'_, str>, String)> = edits
        .iter()
        .map(|(old, new)| {
            let old = normalize_to_lf(old);
            let new = normalize_to_lf(new);
            let old_needle = normalize_for_fuzzy_match(&old);
            (old, new, old_needle)
        })
        .collect();

    for (index, (old, _, _)) in prepared.iter().enumerate() {
        if old.is_empty() {
            return Err(empty_old_text_error(path, index, total));
        }
    }

    // 归一化视图最多构造一次，只服务真正需要它的 edit
    let mut view: Option<NormalizedView> = None;
    let mut matched: Vec<MatchedEdit> = Vec::with_capacity(total);

    for (index, (old, new, old_needle)) in prepared.iter().enumerate() {
        // 1. 精确命中：唯一性按**精确口径**计数，且完全不触发归一化。
        //    这里不拒绝纯空白 —— 精确唯一的空白就是可判定的。
        if let Some(position) = content.find(old.as_ref()) {
            let occurrences = count_occurrences(content, old);
            if occurrences > 1 {
                return Err(duplicate_error(path, index, total, occurrences, old));
            }
            matched.push(MatchedEdit {
                edit_index: index,
                match_index: position,
                match_len: old.len(),
                new_text: new.to_string(),
                fuzzy: false,
                removed_whitespace: Vec::new(),
            });
            continue;
        }

        // 2. 精确 miss 且归一化后为空 ⇒ 纯空白 needle，无法定位。
        if old_needle.is_empty() {
            return Err(blank_old_text_error(path, index, total, old));
        }

        // 3. 请求本身没有提出任何改动（oldText 与 newText 字节相同）⇒ 不是改写请求。
        //    判据必须是**字节**相等：归一化后再比会把 `"foo\nbar"` → `"foo \nbar"` 这类
        //    真实的空白改写误判为 no-op。
        if old == new {
            return Err(no_change_error(path, total));
        }

        // 4. 归一化空间：计数与命中同一口径。
        let view = view.get_or_insert_with(|| NormalizedView::new(content));
        let occurrences = view.count(old_needle);
        if occurrences > 1 {
            return Err(duplicate_error(path, index, total, occurrences, old));
        }
        let (start, end) = match view.find(old_needle) {
            FuzzyFind::Hit { start, end } => (start, end),
            FuzzyFind::Miss => return Err(not_found_error(path, index, total, old)),
            FuzzyFind::PartialCluster { start, end } => {
                let cluster = content.get(start..end).unwrap_or_default();
                return Err(partial_cluster_error(path, index, total, old, cluster));
            }
        };
        matched.push(MatchedEdit {
            edit_index: index,
            match_index: start,
            match_len: end.saturating_sub(start),
            new_text: new.to_string(),
            fuzzy: true,
            removed_whitespace: view
                .removed_trailing_whitespace(start, end)
                .into_iter()
                .filter_map(|(from, to)| content.get(from..to))
                .map(String::from)
                .collect(),
        });
    }
    matched.sort_by_key(|edit| edit.match_index);

    // 命中区间已按起点升序 —— 只要与「前一个」比较即可覆盖全部重叠
    for pair in matched.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.match_index.saturating_add(previous.match_len) > current.match_index {
            return Err(ToolError::Execution(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or \
                 target disjoint regions.",
                previous.edit_index, current.edit_index
            )));
        }
    }

    let new_content = apply_matched(content, &matched, path)?;

    // 无改动判定只在**结果**层面：拼回去和原文一模一样就不算改动。不做「oldText 与 newText
    // 归一化后相同」这层——那会把 `"foo\nbar"` → `"foo \nbar"` 这种真实的空白改写误判为 no-op。
    if new_content == content {
        return Err(no_change_error(path, total));
    }

    let mut notices: Vec<EditNotice> = matched
        .iter()
        .filter(|edit| edit.fuzzy)
        .map(|edit| EditNotice {
            edit_index: edit.edit_index,
            matched: snippet(edit.matched_slice(content), 120),
            removed_whitespace: edit.removed_whitespace.clone(),
        })
        .collect();
    notices.sort_by_key(|notice| notice.edit_index);

    Ok(AppliedEdits {
        new_content,
        notices,
    })
}

/// 把过长的命中片段截到 `max_bytes` 以内，端点必须落在字符边界上。
fn snippet(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// ---------------------------------------------------------------------------
// 输入解析
// ---------------------------------------------------------------------------

fn resolve_path(input: &Value, path_field: &str) -> Result<String, ToolError> {
    input[path_field]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{path_field}'")))
        .map(String::from)
}

/// 缺 `newText` 的错误。
///
/// 删除必须显式写成 `""`：模型漏字段而无提示地当成空串，就会静默删掉文件里的一段内容。
/// 数组形态与 legacy 顶层形态共用这一个口子，两者行为不会漂移。
fn missing_new_text_error(subject: &str) -> ToolError {
    ToolError::InvalidInput(format!(
        "{subject}: missing 'newText' (send \"\" to delete the matched text)"
    ))
}

/// 解析单个 `{oldText, newText}` 对象。
///
/// 缺 `oldText` 或 `newText` 都是错误：删除必须显式写成 `""`，否则模型漏字段就会静默删掉
/// 文件里的一段内容。
fn parse_edit_object(value: &Value, index: usize) -> Result<(String, String), ToolError> {
    let old = value
        .get("oldText")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput(format!("edits[{index}]: missing 'oldText'")))?;
    let new = value
        .get("newText")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_new_text_error(&format!("edits[{index}]")))?;
    Ok((old.to_string(), new.to_string()))
}

/// 解析 `edits`，兼容模型实际会发的几种形态：
///
/// 1. `edits: [{oldText, newText}, ...]`（标准形态）；
/// 2. `edits: "{...}"` —— JSON **字符串**，解析后是数组则用、是单对象则包成一条；
/// 3. `edits: {oldText, newText}` —— 单个对象；
/// 4. 顶层 `oldText` / `newText`（legacy）—— 追加为最后一条。
///
/// 空数组、形态不认识、字符串不是合法 JSON、数组元素不是对象 ⇒ `InvalidInput`。空 `oldText`
/// 不在此处拒，它属于内容层错误（见 [`apply_edits_to_normalized_content`]），一并报给模型。
fn parse_edits(input: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let mut edits: Vec<(String, String)> = Vec::new();

    match input.get("edits") {
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                edits.push(parse_edit_object(item, index)?);
            }
        }
        Some(Value::Object(_)) => edits.push(parse_edit_object(&input["edits"], 0)?),
        Some(Value::String(text)) => {
            let parsed: Value = serde_json::from_str(text).map_err(|e| {
                ToolError::InvalidInput(format!(
                    "'edits' is a string but not valid JSON: {e}. Send an array of \
                     {{oldText, newText}} objects."
                ))
            })?;
            match parsed {
                Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        edits.push(parse_edit_object(item, index)?);
                    }
                }
                Value::Object(_) => edits.push(parse_edit_object(&parsed, 0)?),
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "'edits' JSON must be an array of {{oldText, newText}} objects, got {other}"
                    )));
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(other) => {
            return Err(ToolError::InvalidInput(format!(
                "'edits' must be an array of {{oldText, newText}} objects, got {other}"
            )));
        }
    }

    if let Some(old) = input.get("oldText").and_then(Value::as_str) {
        // 与 `edits[]` 数组形态同口径：缺 `newText` 不得当成删除。
        let new = input
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| missing_new_text_error("top-level 'oldText'"))?;
        edits.push((old.to_string(), new.to_string()));
    }

    if edits.is_empty() {
        return Err(ToolError::InvalidInput(
            "'edits' must contain at least one {oldText, newText} replacement".into(),
        ));
    }

    Ok(edits)
}

// ---------------------------------------------------------------------------
// 工具实现
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit",
            "Edit files with precise text matching. Supports batch edits.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit. Relative paths resolve against the workspace root; use an absolute path only for files outside it." },
                    "edits": {
                        "description": "One or more targeted replacements. Every edits[].oldText is matched against the original file (not incrementally) and must match a unique, non-overlapping region. Merge nearby changes into one edit instead of emitting overlapping or nested ones.",
                        "oneOf": [
                            {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "oldText": { "type": "string", "description": "Exact text of exactly one targeted replacement; must be unique in the original file." },
                                        "newText": { "type": "string", "description": "Replacement text for this targeted edit (\"\" deletes the matched text)." }
                                    },
                                    "required": ["oldText", "newText"]
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "oldText": { "type": "string" },
                                    "newText": { "type": "string" }
                                },
                                "required": ["oldText", "newText"]
                            }
                        ]
                    },
                    "oldText": { "type": "string", "description": "Legacy: text to find" },
                    "newText": { "type": "string", "description": "Legacy: replacement text" }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let path = resolve_path(&input, "path")?;
        let path_notice = absolute_notice("edit", &path, &self.workspace);
        let edits = parse_edits(&input)?;

        let abs_path = self.workspace.join(&path);
        let raw_content = tokio::fs::read_to_string(&abs_path)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read {path}: {e}")))?;

        let (had_bom, content) = split_bom(&raw_content);
        let ending = detect_line_ending(content);
        let normalized = normalize_to_lf(content);
        let applied = apply_edits_to_normalized_content(&normalized, &edits, &path)?;

        let restored = restore_line_endings(&applied.new_content, ending);
        let final_content = if had_bom {
            Cow::Owned(format!("\u{FEFF}{restored}"))
        } else {
            restored
        };

        // 最后一层判定落在**落盘字节**上：与磁盘完全相同就既不写也不报成功
        if final_content == raw_content {
            return Err(no_change_error(&path, edits.len()));
        }

        tokio::fs::write(&abs_path, final_content.as_bytes())
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write {path}: {e}")))?;

        // 归一化命中与被吞的行尾空白必须在结果里说明：模型否则会以为自己精确命中，继续基于
        // 错误假设改文件；行尾空白（Markdown 的 `<br>`）消失而不报，下游只会看到一份与预期不同
        // 的文本却无人提过。
        let total = edits.len();
        let mut notices: Vec<String> = Vec::new();
        for notice in &applied.notices {
            let label = if total == 1 {
                "edit".to_string()
            } else {
                format!("edit: edits[{}]", notice.edit_index)
            };
            notices.push(format!(
                "{label}: matched {:?} by normalizing whitespace and quotes (not an exact match)",
                notice.matched
            ));
            // 每个被吞的行尾空白各报一条：合并成一条会把两行的空白拼成一串，误报成同一个位置
            for removed in &notice.removed_whitespace {
                notices.push(format!(
                    "{label}: also removed trailing whitespace {removed:?} at the end of a \
                     matched line"
                ));
            }
        }

        let mut tool_result = ToolResult {
            content: format!("Successfully replaced {} block(s) in {path}.", edits.len()),
            is_error: false,
        };
        ys_tool::append_notices(&mut tool_result.content, &notices);
        Ok(attach_notice(tool_result, path_notice.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
    }

    fn test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agent_tools_basic_edit_test_{id}_{}_{seq}",
            std::process::id()
        ))
    }

    /// 建临时 workspace 并写入 `content`，返回 `(dir, 文件)`。
    fn workspace_with(name: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    /// 跑一次 `edit`，断言报错信息含 `needle` 且文件字节**逐字节不变**。
    async fn expect_rejected(content: &str, input: Value, needle: &str) -> String {
        let (dir, file) = workspace_with("rejected.txt", content);
        let tool = EditTool::new(dir.clone());
        let err = tool
            .call(input, make_ctx())
            .await
            .expect_err("edit must be rejected");
        let message = err.to_string();
        assert!(
            message.contains(needle),
            "err = {message}, expected to contain {needle:?}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            content.as_bytes(),
            "rejected edit must not touch the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
        message
    }

    #[test]
    fn test_test_dir_is_unique() {
        let a = test_dir();
        let b = test_dir();
        assert_ne!(a, b, "consecutive test_dir() calls must not collide");
    }

    #[tokio::test]
    async fn test_edit_exact_match() {
        let (dir, file) = workspace_with("exact.txt", "hello world");

        let tool = EditTool::new(dir.clone());
        let input = json!({
            "path": "exact.txt",
            "edits": [{ "oldText": "world", "newText": "rust" }]
        });
        let result = tool.call(input, make_ctx()).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("1 block"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello rust");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// workspace 内绝对路径成功替换后追加相对路径事实。
    ///
    /// 变异：删掉 `absolute_notice` 或 `attach_notice` ⇒ 断言变红。
    #[tokio::test]
    async fn absolute_workspace_path_adds_relative_hint() {
        let (dir, file) = workspace_with("absolute.txt", "hello world");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": file.to_string_lossy(),
                    "edits": [{ "oldText": "world", "newText": "rust" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(
            result
                .content
                .contains("edit: this path is inside the workspace root"),
            "content = {}",
            result.content
        );
        assert!(result.content.contains("`absolute.txt`"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello rust");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_batch() {
        let (dir, file) = workspace_with("batch.txt", "aaa bbb ccc");

        let tool = EditTool::new(dir.clone());
        let input = json!({
            "path": "batch.txt",
            "edits": [
                { "oldText": "aaa", "newText": "111" },
                { "oldText": "ccc", "newText": "333" }
            ]
        });
        tool.call(input, make_ctx()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "111 bbb 333");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_legacy_format() {
        let (dir, file) = workspace_with("legacy.txt", "foo bar");

        let tool = EditTool::new(dir.clone());
        let input = json!({
            "path": "legacy.txt",
            "oldText": "foo",
            "newText": "baz"
        });
        tool.call(input, make_ctx()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "baz bar");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let (dir, _file) = workspace_with("notfound.txt", "hello");

        let tool = EditTool::new(dir.clone());
        let input = json!({
            "path": "notfound.txt",
            "edits": [{ "oldText": "xyz", "newText": "abc" }]
        });
        assert!(tool.call(input, make_ctx()).await.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_multiple_matches_error() {
        let (dir, _file) = workspace_with("multi.txt", "aaa aaa aaa");

        let tool = EditTool::new(dir.clone());
        let input = json!({
            "path": "multi.txt",
            "edits": [{ "oldText": "aaa", "newText": "bbb" }]
        });
        assert!(tool.call(input, make_ctx()).await.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_preserves_crlf() {
        let (dir, file) = workspace_with("crlf.txt", "hello\r\nworld\r\n");

        let tool = EditTool::new(dir.clone());
        let input = json!({
            "path": "crlf.txt",
            "edits": [{ "oldText": "hello", "newText": "hi" }]
        });
        let result = tool.call(input, make_ctx()).await.unwrap();
        assert!(!result.is_error);

        let written = String::from_utf8(std::fs::read(&file).unwrap()).unwrap();
        assert!(written.contains("\r\n"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归一化命中时的替换范围必须按**实际匹配到的文本**计算。
    ///
    /// 变异：把切片长度换回原始 `old_text.len()`（`"hello world  "` 13 字节 vs 实际 11）⇒
    /// `apply_matched` 的越界校验报 `replacement range ... is invalid`（不 panic）⇒ 断言变红。
    #[tokio::test]
    async fn fuzzy_match_uses_actual_matched_length() {
        let (dir, file) = workspace_with("fuzzy.txt", "hello world");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "fuzzy.txt",
                    "edits": [{ "oldText": "hello world  ", "newText": "bye" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bye");
        // 归一化命中必须在结果里说明，模型才知道自己没精确命中
        assert!(
            result.content.contains("normaliz"),
            "content = {}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `oldText` 里含多字节字符、归一化后被压缩时，命中长度按原文映射计。
    ///
    /// 变异：把切片长度换回原始 `old_text.len()`（17 字节 vs 实际 12）⇒ `apply_matched` 的
    /// 越界/字符边界校验报 `replacement range ... is invalid`（不 panic）⇒ 断言变红。
    #[tokio::test]
    async fn fuzzy_match_multibyte_old_text_uses_normalized_length() {
        let (dir, file) = workspace_with("smart_quote.txt", "say \"hi\" now\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "smart_quote.txt",
                    // 智能双引号：每个 3 字节，归一化后 1 字节
                    "edits": [{ "oldText": "say \u{201C}hi\u{201D} now", "newText": "bye" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bye\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 命中范围跨多字节字符（NBSP 占 2 字节）时，切片端点仍须落在字符边界上。
    ///
    /// 变异：用 `old_text.len()` 之外的错误长度 ⇒ 端点落进多字节字符内部 ⇒ 由 `apply_matched`
    /// 里的 `is_char_boundary` 检查转成 `invalid_range_error`（不 panic）⇒ 断言变红。
    #[tokio::test]
    async fn fuzzy_match_range_keeps_char_boundaries() {
        let (dir, file) = workspace_with("nbsp.txt", "x a\u{00A0}b y");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "nbsp.txt",
                    "edits": [{ "oldText": "a b", "newText": "Z" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x Z y");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归一化命中只替换命中区间：`oldText` 多带的行尾空白不属于命中范围，
    /// 其余行原样保留。
    ///
    /// 变异：整行从归一化 base 取 ⇒ 第 2 行内容或被吞、或行尾空白丢失。
    #[tokio::test]
    async fn fuzzy_match_preserves_original_boundary_whitespace() {
        let (dir, file) = workspace_with("boundary.txt", "hello world\nsecond line\n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "boundary.txt",
                "edits": [{ "oldText": "hello world  \n", "newText": "bye\n" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "bye\nsecond line\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归一化匹配命中多处时同样必须拒绝，不能猜改哪一处。
    #[tokio::test]
    async fn fuzzy_match_multiple_locations_error() {
        let message = expect_rejected(
            "a\u{00A0}b and a\u{00A0}b",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "a b", "newText": "Z" }]
            }),
            "2 times",
        )
        .await;
        assert!(message.contains("must match exactly once"), "{message}");
    }

    /// T1a：CRLF 文件编辑后不得出现 `\r\r`（归一化后再还原，不能对已有 `\r\n` 二次展开）。
    ///
    /// 变异：去掉 `normalize_to_lf`、直接对含 `\r\n` 的内容做 `\n` → `\r\n` ⇒ 断言变红。
    #[tokio::test]
    async fn crlf_file_edit_produces_no_double_cr() {
        let (dir, file) = workspace_with("crlf_roundtrip.txt", "alpha\r\nbeta\r\ngamma\r\n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "crlf_roundtrip.txt",
                "edits": [{ "oldText": "beta", "newText": "BETA" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        let text = String::from_utf8(std::fs::read(&file).unwrap()).unwrap();
        assert!(!text.contains("\r\r"), "text = {text:?}");
        assert_eq!(text, "alpha\r\nBETA\r\ngamma\r\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T1b：LF 文件编辑后不得混入 `\r`。
    #[tokio::test]
    async fn lf_file_edit_stays_lf() {
        let (dir, file) = workspace_with("lf_roundtrip.txt", "alpha\nbeta\ngamma\n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "lf_roundtrip.txt",
                "edits": [{ "oldText": "beta", "newText": "BETA" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
        assert!(!std::fs::read(&file).unwrap().contains(&b'\r'));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2：`oldText` 带行首缩进、命中从行首之后开始时，缩进原样保留；未触及行不变。
    ///
    /// 变异：把归一化实现改成整体 `trim` ⇒ 文件首行的行首缩进被吞 ⇒ 断言变红。
    #[tokio::test]
    async fn fuzzy_match_keeps_indent_of_untouched_lines() {
        let (dir, file) = workspace_with(
            "indent.txt",
            "    let a = \u{201C}x\u{201D};\n    let b = 2;\n",
        );

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "indent.txt",
                // 命中区间不含行首缩进，也跨过了行尾
                "edits": [{ "oldText": "let a = \"x\";\n    let b", "newText": "let c = 1;\n    let d" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "    let c = 1;\n    let d = 2;\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T3：空 `oldText` 显式拒绝，且文件不变。
    #[tokio::test]
    async fn empty_old_text_is_rejected() {
        expect_rejected(
            "keep me",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "", "newText": "X" }]
            }),
            "oldText must not be empty",
        )
        .await;
    }

    /// T4：重叠的 edits 报错，错误信息含两个 edit 序号，且文件不变。
    #[tokio::test]
    async fn overlapping_edits_are_rejected() {
        let message = expect_rejected(
            "abcdef",
            json!({
                "path": "rejected.txt",
                "edits": [
                    { "oldText": "abc", "newText": "X" },
                    { "oldText": "cde", "newText": "Y" }
                ]
            }),
            "overlap",
        )
        .await;
        assert!(message.contains("edits[0]"), "err = {message}");
        assert!(message.contains("edits[1]"), "err = {message}");
    }

    /// T5：替换后内容与原文相同 ⇒ 报 `No changes made`，不得谎报成功。
    #[tokio::test]
    async fn identical_replacement_is_rejected() {
        expect_rejected(
            "hello\n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "hello", "newText": "hello" }]
            }),
            "No changes made",
        )
        .await;
    }

    /// P0：oldText 与 newText 字节相同 ⇒ 没有任何改写请求，必须拒绝且文件字节不变。
    ///
    /// 原文是 `foo<Nbsp>bar` + 两个行尾空格；模型把两边都写作 `foo bar`。这就是一次纯 no-op，
    /// 落到原文上只会抹平 NBSP 与行尾空白。
    ///
    /// 变异：删掉 `new_content == content` 判定 ⇒ 返回成功且文件被改写 ⇒ 断言变红。
    #[tokio::test]
    async fn normalizing_no_op_is_rejected() {
        expect_rejected(
            "foo\u{00A0}bar  \n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "foo bar", "newText": "foo bar" }]
            }),
            "No changes made",
        )
        .await;
    }

    /// P1-2：归一化空间看起来「old == new」，但落在原文上是**真实改写** —— 必须成功。
    ///
    /// 原文行尾两个空格，`newText` 只留一个：这是一次普通而明确的空白改写。若把「oldText 与
    /// newText 归一化后相同」当作 no-op 判据，它会被误报为「没改动」。
    ///
    /// 变异：加回「归一化 old == new」那一层 ⇒ 本例报 `No changes made` ⇒ 断言变红。
    #[tokio::test]
    async fn normalized_equality_is_not_a_no_op() {
        for (new_text, expected) in [("foo \nbar", "foo \nbar\n"), ("foo\t\nbar", "foo\t\nbar\n")] {
            let (dir, file) = workspace_with("real_change.txt", "foo  \nbar\n");
            let tool = EditTool::new(dir.clone());
            let result = tool
                .call(
                    json!({
                        "path": "real_change.txt",
                        "edits": [{ "oldText": "foo\nbar", "newText": new_text }]
                    }),
                    make_ctx(),
                )
                .await
                .unwrap_or_else(|e| panic!("newText {new_text:?} must be applied: {e}"));

            assert!(!result.is_error, "{}", result.content);
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                expected,
                "newText {new_text:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// P1-1：纯空白 `oldText` 精确且**唯一**时必须正常应用 —— 「把 tab 缩进改成空格」是
    /// 完全可判定的常规编辑，一律拒绝会让模型无法表达。
    ///
    /// 四个族：tab → 空格、NBSP → 空格、全角空格 → 半角、四空格 → 两空格。
    ///
    /// 变异：把「归一化后为空」的拒绝改回无条件前置 ⇒ 四例全报 `contains only whitespace`。
    #[tokio::test]
    async fn unique_blank_old_text_is_applied() {
        let cases = [
            ("x\ty\n", "\t", "    ", "x    y\n"),
            ("a\u{00A0}b\n", "\u{00A0}", " ", "a b\n"),
            ("p\u{3000}q\n", "\u{3000}", " ", "p q\n"),
            ("    z\n", "    ", "  ", "  z\n"),
        ];
        for (original, old, new, expected) in cases {
            let (dir, file) = workspace_with("blank.txt", original);
            let tool = EditTool::new(dir.clone());
            let result = tool
                .call(
                    json!({
                        "path": "blank.txt",
                        "edits": [{ "oldText": old, "newText": new }]
                    }),
                    make_ctx(),
                )
                .await
                .unwrap_or_else(|e| panic!("unique blank {old:?} must be applied: {e}"));

            assert!(!result.is_error, "{}", result.content);
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                expected,
                "oldText {old:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// P1-1：精确**多处**命中时是歧义（不是「只有空白」）—— 模型需要的建议是「加上下文」，
    /// 而不是「补上非空白字符」。
    ///
    /// 变异：把空白拒绝移到精确查找之前 ⇒ 报 `contains only whitespace` ⇒ 断言变红。
    #[tokio::test]
    async fn whitespace_only_old_text_is_rejected() {
        let message = expect_rejected(
            "a   b   c   d\n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "   ", "newText": "_" }]
            }),
            "3 times",
        )
        .await;
        assert!(message.contains("must match exactly once"), "{message}");
        assert!(!message.contains("contains only whitespace"), "{message}");
    }

    /// P1-1：精确 miss 且归一化后为空 ⇒ 无法定位，报「只有空白」并给出可自修提示。
    ///
    /// 原文没有裸的 tab（只有 NBSP 开头的行），所以 `"\t"` 精确 miss；归一化后它是空串，
    /// 不可能指任何位置。
    #[tokio::test]
    async fn blank_old_text_that_never_matches_is_reported_as_blank() {
        expect_rejected(
            "a\u{00A0}b\n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "\t", "newText": "    " }]
            }),
            "contains only whitespace",
        )
        .await;
    }

    /// P1：零宽字符不再是「可删除的空白」等价物，参与正常计数 ⇒ 多处命中即歧义。
    #[tokio::test]
    async fn zero_width_only_old_text_is_counted() {
        expect_rejected(
            "a\u{200B}b\u{200B}c\u{200B}d\n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "\u{200B}", "newText": "-" }]
            }),
            "3 times",
        )
        .await;
    }

    /// P1：同批混入一条归一化命中，不得让另一条精确唯一的 oldText 变成「歧义」。
    ///
    /// `"p  \n"` 在原文恰好出现一次；只有 `"q r"`（原文是 `q<Nbsp>r`）需要归一化。
    ///
    /// 变异：唯一性一律在归一化 base 上计数 ⇒ `edits[0] matches 2 times` ⇒ 断言变红。
    #[tokio::test]
    async fn exact_uniqueness_survives_a_fuzzy_sibling_edit() {
        let (dir, file) = workspace_with("mixed.txt", "p  \np\nq\u{00A0}r\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "mixed.txt",
                    "edits": [
                        { "oldText": "p  \n", "newText": "P\n" },
                        { "oldText": "q r", "newText": "Z" }
                    ]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error, "content = {}", result.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "P\np\nZ\n");
        // 精确命中的那条不应被报成归一化命中
        assert_eq!(
            result.content.matches("normaliz").count(),
            1,
            "{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1：notice 覆盖**所有**实际走归一化命中的 edit。
    ///
    /// 两条都只因空白差异而走归一化（第一条多一个行尾空格，第二条是 NBSP）。
    ///
    /// 变异：notice 取首轮判定 / 只报第一条 ⇒ 条数不足 ⇒ 断言变红。
    #[tokio::test]
    async fn every_fuzzy_hit_gets_a_notice() {
        let (dir, file) = workspace_with("notices.txt", "foo  \nbar\nbase\u{00A0}x\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "notices.txt",
                    "edits": [
                        // 原文是 "foo  \nbar"：oldText 少一个空格 ⇒ 只能归一化命中
                        { "oldText": "foo \nbar", "newText": "FOO\nbar" },
                        { "oldText": "base x", "newText": "BASE" }
                    ]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(
            result.content.matches("normaliz").count(),
            2,
            "{}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "FOO\nbar\nBASE\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// notice 只说明**实际**走归一化命中的 edit：同批里精确命中那条不应带 notice。
    #[tokio::test]
    async fn exact_hit_in_a_mixed_batch_gets_no_notice() {
        let (dir, file) = workspace_with("notices_mixed.txt", "foo  \nbar\nbase\u{00A0}x\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "notices_mixed.txt",
                    "edits": [
                        // 原文逐字节包含 "foo  \nbar" ⇒ 精确命中，行尾两个空格属于命中区间
                        { "oldText": "foo  \nbar", "newText": "FOO\nbar" },
                        { "oldText": "base x", "newText": "BASE" }
                    ]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(
            result.content.matches("normaliz").count(),
            1,
            "{}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "FOO\nbar\nBASE\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1：被触及行内、命中区间之外的字符必须逐字节保留（零宽字符与弯引号）。
    ///
    /// 变异①：把零宽字符加回删除集 ⇒ 归一化把它们吞掉 ⇒ 断言变红。
    /// 变异②：整行从归一化 base 取 ⇒ 弯引号与 NBSP 被抹平、ZWSP / ZWJ 被删 ⇒ 断言变红。
    #[tokio::test]
    async fn bytes_outside_the_match_are_preserved() {
        let original = "pre\u{200B}post \u{201C}q\u{201D} foo\u{00A0}bar \u{200D}\u{FEFF}\n";
        let (dir, file) = workspace_with("outside.txt", original);

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "outside.txt",
                "edits": [{ "oldText": "foo bar", "newText": "X" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "pre\u{200B}post \u{201C}q\u{201D} X \u{200D}\u{FEFF}\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 命中区间落在行尾时，行尾空白不得被吞（它就是「区间之外的字节」）。
    ///
    /// 变异：把命中区间扩到整行（整行从归一化 base 取回）⇒ 行尾两个空格丢失 ⇒ 断言变红。
    #[tokio::test]
    async fn trailing_whitespace_after_the_match_is_preserved() {
        let (dir, file) = workspace_with("trailing_ws.txt", "foo\u{00A0}bar  \n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "trailing_ws.txt",
                "edits": [{ "oldText": "foo bar", "newText": "X" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "X  \n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T6：全部 edits 都按**原始内容**匹配 —— 第一条的替换会移动第二条的原始位置。
    ///
    /// 变异：改成在已改过的内容上增量匹配 ⇒ 第二条命中第一条插入的 `bbb` ⇒ 结果变红。
    #[tokio::test]
    async fn all_edits_match_against_original_content() {
        let (dir, file) = workspace_with("non_incremental.txt", "aaa bbb");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "non_incremental.txt",
                "edits": [
                    { "oldText": "aaa", "newText": "xxx bbb" },
                    { "oldText": "bbb", "newText": "yyy" }
                ]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        // 若增量匹配，第二条会改到第一条插入的 `bbb`，得到 "xxx yyy"
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "xxx bbb yyy");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 精确口径优先：两处仅行尾空白不同时，精确命中的那一处唯一，直接改它。
    ///
    /// 归一化口径只在精确找不到时才启用；否则「加多少上下文都无解」的假歧义会让模型无法
    /// 表达这种编辑。
    #[tokio::test]
    async fn exact_occurrence_wins_over_trailing_whitespace_sibling() {
        let (dir, file) = workspace_with("trailing.txt", "foo\nfoo  \n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "trailing.txt",
                "edits": [{ "oldText": "foo\n", "newText": "bar\n" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        // 只改精确命中那一处，另一处的行尾空白不受影响
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bar\nfoo  \n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T8：归一化命中后，未触及的行与原文件**字节级相同**（含尾随空白、缩进、多字节字符）。
    ///
    /// 变异：把整个归一化内容写回 ⇒ 未触及行的尾随空白丢失 ⇒ 断言变红。
    #[tokio::test]
    async fn untouched_lines_keep_original_bytes() {
        let original = "keep  \r\nfoo\u{00A0}bar\r\n  尾行  \r\n";
        let (dir, file) = workspace_with("preserve_bytes.txt", original);

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "preserve_bytes.txt",
                // 文件里是 NBSP，模型打的是直空格 ⇒ 只能走归一化
                "edits": [{ "oldText": "foo bar", "newText": "REPLACED" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "keep  \r\nREPLACED\r\n  尾行  \r\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归一化不整个 `trim`：`oldText` 多带行首空白不算命中。
    ///
    /// 变异：给归一化加整体 `trim` ⇒ 本例变成成功 ⇒ 断言变红。
    #[tokio::test]
    async fn leading_indent_in_old_text_is_not_stripped() {
        expect_rejected(
            "value\n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "    value", "newText": "other" }]
            }),
            "not found",
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // 输入形态与错误分支
    // -----------------------------------------------------------------------

    /// T10：`edits` 传成 JSON 字符串时按数组解析（不是「删除这段文本」）。
    ///
    /// 变异：回退成「字符串 = 单个 oldText + 空 newText」⇒ 文件被清空 ⇒ 断言变红。
    #[tokio::test]
    async fn edits_as_json_string_are_parsed() {
        let (dir, file) = workspace_with("json_string.txt", "aaa bbb ccc");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "json_string.txt",
                "edits": "[{\"oldText\":\"aaa\",\"newText\":\"111\"},{\"oldText\":\"ccc\",\"newText\":\"333\"}]"
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "111 bbb 333");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `edits` 是单个对象时按一条 edit 处理。
    #[tokio::test]
    async fn edits_as_single_object_are_parsed() {
        let (dir, file) = workspace_with("single_object.txt", "foo bar");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "single_object.txt",
                "edits": { "oldText": "bar", "newText": "baz" }
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "foo baz");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `edits` 字符串里是**单个对象**时也按一条 edit 处理。
    #[tokio::test]
    async fn edits_as_json_string_with_single_object_is_parsed() {
        let (dir, file) = workspace_with("json_object_string.txt", "foo bar");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "json_object_string.txt",
                "edits": "{\"oldText\":\"bar\",\"newText\":\"qux\"}"
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "foo qux");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `edits` 字符串无法解析成 JSON 时报错，不得当成删除。
    #[tokio::test]
    async fn unparsable_edits_string_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({ "path": "rejected.txt", "edits": "not json at all" }),
            "not valid JSON",
        )
        .await;
    }

    /// `edits: []` ⇒ 报错。
    #[tokio::test]
    async fn empty_edits_array_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({ "path": "rejected.txt", "edits": [] }),
            "at least one",
        )
        .await;
    }

    /// `edits: null` ⇒ 报错（没有可执行的替换）。
    #[tokio::test]
    async fn null_edits_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({ "path": "rejected.txt", "edits": null }),
            "at least one",
        )
        .await;
    }

    /// `edits` 既不是数组也不是对象（数字）⇒ 报错。
    #[tokio::test]
    async fn numeric_edits_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({ "path": "rejected.txt", "edits": 42 }),
            "must be an array",
        )
        .await;
    }

    /// 数组元素不是对象（字符串）⇒ 报错。
    #[tokio::test]
    async fn non_object_edit_element_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({ "path": "rejected.txt", "edits": ["b"] }),
            "missing 'oldText'",
        )
        .await;
    }

    /// 缺 `newText` ⇒ 报错（删除必须显式写成 `""`，不得静默删除）。
    #[tokio::test]
    async fn missing_new_text_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "keep" }]
            }),
            "missing 'newText'",
        )
        .await;
    }

    /// legacy 顶层 `oldText` 缺 `newText` 时同样报错，口径与 `edits[]` 数组形态一致。
    ///
    /// 变异：退回 `unwrap_or("")` ⇒ 静默把文件里的 `keep` 删掉 ⇒ 断言变红。
    #[tokio::test]
    async fn legacy_missing_new_text_is_rejected() {
        expect_rejected(
            "keep me\n",
            json!({
                "path": "rejected.txt",
                "oldText": "keep"
            }),
            "missing 'newText'",
        )
        .await;
    }

    /// 显式空 `newText` 是删除，允许。
    #[tokio::test]
    async fn explicit_empty_new_text_deletes() {
        let (dir, file) = workspace_with("delete.txt", "keep drop\n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "delete.txt",
                "edits": [{ "oldText": " drop", "newText": "" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 换行
    // -----------------------------------------------------------------------

    /// 全是孤立 `\r` 的文件：换行风格属于 `LineEnding::Cr` 一档，编辑后仍保持 `\r`（不能把整个
    /// 文件的换行风格改成 LF），也不得出现 `\r\r`。
    ///
    /// 变异：`detect_line_ending` 不认 `\r`-only（归为 LF）⇒ 落盘变成 LF ⇒ 断言变红。
    /// 全文只要有 `\n`，孤立的 `\r` 就只是内容里的一个字符 —— 不得把整文件换行风格判成 CR。
    ///
    /// 变异：把孤立 `\r` 无条件当作风格候选 ⇒ 落盘把全文换成 `\r` ⇒ 断言变红。
    #[tokio::test]
    async fn isolated_cr_does_not_change_the_line_ending() {
        let (dir, file) = workspace_with("csv_cr.txt", "a\rb,c\r\nid,name\r\n1,x\r\n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "csv_cr.txt",
                "edits": [{ "oldText": "1,x", "newText": "1,y" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        // 关键不变量：CRLF 骨架不得被换成 CR-only（否则整个 CSV 被重写）。
        // 字段内嵌的孤立 `\r` 属于内容，会被归一到文件行尾风格。
        assert_eq!(text, "a\r\nb,c\r\nid,name\r\n1,y\r\n");
        assert!(!text.contains("\r\r"), "text = {text:?}");
        assert!(
            text.matches("\r\n").count() >= 3,
            "file must keep its CRLF skeleton: {text:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lone_cr_file_keeps_cr_endings() {
        let (dir, file) = workspace_with("lone_cr.txt", "alpha\rbeta\rgamma\r");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "lone_cr.txt",
                "edits": [{ "oldText": "beta", "newText": "BETA" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, "alpha\rBETA\rgamma\r");
        assert!(!text.contains('\n'), "text = {text:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 兼容字符簇（NFKC 展开）
    // -----------------------------------------------------------------------

    /// P1-3：命中只盖住一个展开成多字符的兼容字符的**一部分**时，必须报错而不是整簇替换。
    ///
    /// 整簇替换会吞掉同簇其余字节（`ﬁ` → 只剩 `F`，`°` / `1⁄` 同理），这是「区间之外逐字节
    /// 保留」在字符内部的破口；按半簇切片则会把字符切碎。
    ///
    /// 变异：`find` 遇到 atomic 簇仍取整簇区间（不算 PartialCluster）⇒ 四例都返回成功且
    /// 文件字节被改（丢了同簇其余字节）⇒ 断言变红。
    #[tokio::test]
    async fn partial_compatibility_cluster_is_rejected() {
        let cases = [
            // ﬁ (U+FB01) 展开为 "fi"
            ("deﬁne x\n", "f", "F", "U+FB01"),
            ("deﬁne x\n", "i", "I", "U+FB01"),
            // ℃ (U+2103) 展开为 "°C"
            ("温度 25℃ 今天\n", "C", "F", "U+2103"),
            // ½ (U+00BD) 展开为 "1⁄2"
            ("用量 1½ 杯\n", "2", "3", "U+00BD"),
        ];
        for (content, old, new, code) in cases {
            let message = expect_rejected(
                content,
                json!({
                    "path": "rejected.txt",
                    "edits": [{ "oldText": old, "newText": new }]
                }),
                "matched only part of compatibility character",
            )
            .await;
            assert!(message.contains(code), "{message}");
            assert!(
                message.contains("include the whole character in oldText"),
                "{message}"
            );
        }
    }

    /// 把整个兼容字符写进 `oldText` 时可以正常编辑：拒绝的是「半簇」，不是「簇」。
    ///
    /// 原文带 NBSP，所以 `oldText` 只能走归一化路径（精确 miss）—— 这正是 atomic 簇的路径。
    ///
    /// 变异：atomic 簇一律拒绝（不区分是否盖住整簇）⇒ 本例报错 ⇒ 断言变红。
    #[tokio::test]
    async fn whole_compatibility_cluster_can_be_edited() {
        let (dir, file) = workspace_with("ligature.txt", "deﬁne\u{00A0}x\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "ligature.txt",
                    "edits": [{ "oldText": "ﬁne x", "newText": "fine x" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "define x\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 被并入命中区间的行尾空白
    // -----------------------------------------------------------------------

    /// P1-4：归一化命中把行尾空白一起吞掉时必须**点名**它（Markdown 的行尾两空格就是 `<br>`）。
    ///
    /// 原文是 `intro` + 两个行尾空格；`oldText` 以 `\n` 收尾，命中区间必然包含这两个空格。
    /// 「能删就不静默」：不说，模型会以为自己只改了 `intro`。
    ///
    /// 变异：不生成行尾空白 notice ⇒ 断言变红。
    #[tokio::test]
    async fn swallowed_trailing_whitespace_is_reported() {
        let (dir, file) = workspace_with("hardbreak.txt", "intro  \nbody\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "hardbreak.txt",
                    "edits": [{ "oldText": "intro\n", "newText": "INTRO\n" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "INTRO\nbody\n");
        assert!(
            result
                .content
                .contains("also removed trailing whitespace \"  \""),
            "content = {}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 命中区间没碰到行尾空白时**不得**建这个 notice（否则提示本身会变成噪音）：
    /// 原文 `foo<Nbsp>bar` + 两个行尾空格，`oldText` 停在 `bar` 前。
    #[tokio::test]
    async fn untouched_trailing_whitespace_is_not_reported() {
        let (dir, file) = workspace_with("no_hardbreak.txt", "foo\u{00A0}bar  \nbaz\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "no_hardbreak.txt",
                    "edits": [{ "oldText": "foo bar", "newText": "X" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "X  \nbaz\n");
        assert!(result.content.contains("normaliz"), "{}", result.content);
        assert!(
            !result.content.contains("also removed trailing whitespace"),
            "content = {}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `oldText` 自带行尾空白时按**精确路径**命中：那两空格属于命中区间，内容不丢也无 notice。
    #[tokio::test]
    async fn exact_match_with_trailing_whitespace_keeps_it_in_range() {
        let (dir, file) = workspace_with("exact_hardbreak.txt", "intro  \nbody\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "exact_hardbreak.txt",
                    "edits": [{ "oldText": "intro  \n", "newText": "intro  \nBODY\n" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "intro  \nBODY\nbody\n"
        );
        assert!(
            !result.content.contains("also removed trailing whitespace"),
            "content = {}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 性能契约护栏（视图构造次数）
    // -----------------------------------------------------------------------

    /// 纯精确批次**不构造**归一化视图：精确路径没有任何全量归一化开销。
    ///
    /// 变异：`always_build_view`（在循环前无条件建视图）⇒ 计数变 1 ⇒ 断言变红。
    #[tokio::test]
    async fn exact_only_batch_builds_no_view() {
        let (dir, file) = workspace_with("perf_exact.txt", "aaa bbb ccc\n");

        let before = view_builds();
        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "perf_exact.txt",
                "edits": [
                    { "oldText": "aaa", "newText": "111" },
                    { "oldText": "bbb", "newText": "222" },
                    { "oldText": "ccc", "newText": "333" }
                ]
            }),
            make_ctx(),
        )
        .await
        .unwrap();
        let built = view_builds() - before;

        assert_eq!(built, 0, "exact matches must not build a normalized view");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "111 222 333\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一批 N 条归一化 edit 只构造**一次**视图（N 与文件大小相乘才是不可接受的代价）。
    ///
    /// 变异：`rebuild_view_per_edit`（每条 edit 各自建视图）⇒ 计数变 3 ⇒ 断言变红。
    #[tokio::test]
    async fn fuzzy_batch_builds_the_view_once() {
        let (dir, file) = workspace_with("perf_fuzzy.txt", "a\u{00A0}b\nc\u{00A0}d\ne\u{00A0}f\n");

        let before = view_builds();
        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "perf_fuzzy.txt",
                "edits": [
                    { "oldText": "a b", "newText": "A" },
                    { "oldText": "c d", "newText": "C" },
                    { "oldText": "e f", "newText": "E" }
                ]
            }),
            make_ctx(),
        )
        .await
        .unwrap();
        let built = view_builds() - before;

        assert_eq!(built, 1, "a fuzzy batch must reuse one normalized view");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "A\nC\nE\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mixed_line_endings_follow_the_first_newline() {
        let (dir, crlf_file) = workspace_with("mixed_crlf.txt", "a\r\nb\nc\r\n");
        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "mixed_crlf.txt",
                "edits": [{ "oldText": "c", "newText": "C" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&crlf_file).unwrap(),
            "a\r\nb\r\nC\r\n"
        );

        let lf_file = dir.join("mixed_lf.txt");
        std::fs::write(&lf_file, "a\nb\r\nc\n").unwrap();
        tool.call(
            json!({
                "path": "mixed_lf.txt",
                "edits": [{ "oldText": "c", "newText": "C" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&lf_file).unwrap(), "a\nb\nC\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BOM 必须原样保留，且不参与匹配。
    #[tokio::test]
    async fn bom_is_preserved() {
        let (dir, file) = workspace_with("bom.txt", "\u{FEFF}hello");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "bom.txt",
                "edits": [{ "oldText": "hello", "newText": "bye" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\u{FEFF}bye");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 归一化与映射的单元测试
    // -----------------------------------------------------------------------
    /// 零宽字符 needle 直接命中（不再被归一化吞掉），且只改命中那一处。
    ///
    /// 变异：把 U+200B-200D 加回删除集 ⇒ needle 归一化后为空 ⇒ 报「只有空白」而不是成功。
    #[tokio::test]
    async fn zero_width_needle_matches_exactly() {
        let (dir, file) = workspace_with("zw.txt", "a\u{200B}b\n");

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "zw.txt",
                    "edits": [{ "oldText": "a\u{200B}b", "newText": "ab" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ab\n");
        // 精确命中 ⇒ 不该带归一化 notice
        assert!(!result.content.contains("normaliz"), "{}", result.content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// emoji 的 ZWJ 序列：编辑同行的其他内容时，ZWJ 必须逐字节保留（拆开会变成多个独立字形）。
    ///
    /// 变异：把 ZWJ 加回删除集 ⇒ 家庭 emoji 被拆散 ⇒ 断言变红。
    #[tokio::test]
    async fn emoji_zwj_sequence_survives_a_sibling_fuzzy_edit() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let original = format!("{family} foo\u{00A0}bar\n");
        let (dir, file) = workspace_with("emoji.txt", &original);

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "emoji.txt",
                "edits": [{ "oldText": "foo bar", "newText": "X" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{family} X\n")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 组合字符（NFKC 会合成）作为命中边界时端点不落进字符内部。
    ///
    /// 变异：按字节而非按簇推进映射 ⇒ 端点停在中文字符 / 组合序列中间 ⇒ 报错或 panic。
    #[tokio::test]
    async fn combining_and_wide_chars_keep_boundaries_on_fuzzy_edit() {
        let (dir, file) = workspace_with("combining.txt", "cafe\u{301} \u{4E2D}\u{6587}\n");

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "combining.txt",
                // 文件里是 decomposed 的 é，模型打的是 composed 的 é
                "edits": [{ "oldText": "caf\u{E9}", "newText": "tea" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "tea \u{4E2D}\u{6587}\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空文件：任何 oldText 都找不到，报 not found 而不是 panic。
    #[tokio::test]
    async fn empty_file_reports_not_found() {
        expect_rejected(
            "",
            json!({
                "path": "rejected.txt",
                "edits": [{ "oldText": "x", "newText": "y" }]
            }),
            "not found",
        )
        .await;
    }

    /// 幂等归一化：视图内容与 `normalize_for_fuzzy_match` 一致，且重复归一化不变。
    #[test]
    fn normalization_is_idempotent() {
        let once = normalize_for_fuzzy_match("a\u{00A0}b  \nc\u{201C}d\u{201D}\n");
        assert_eq!(normalize_for_fuzzy_match(&once), once);
    }

    /// 精确命中路径不得构造归一化视图（否则「精确也要全量归一化」的退化会回来）。
    ///
    /// 两层断言：①构造计数为 0（直接可判定）；②其它位置的弯引号 / 全角空格 / 行尾空白
    /// 逐字节保留。变异 `always_build_view` 只靠②看不出来（精确命中本来就不会拿归一化内容
    /// 写回），必须靠①。
    #[tokio::test]
    async fn exact_path_leaves_surrounding_bytes_untouched() {
        let original = "\u{201C}a\u{201D}  \nkey\n\u{3000}b\u{3000}\n";
        let (dir, file) = workspace_with("exact_bytes.txt", original);

        let before = view_builds();
        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "exact_bytes.txt",
                    "edits": [{ "oldText": "key", "newText": "KEY" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert_eq!(
            view_builds() - before,
            0,
            "exact path must not build a view"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "\u{201C}a\u{201D}  \nKEY\n\u{3000}b\u{3000}\n"
        );
        assert!(!result.content.contains("normaliz"), "{}", result.content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fuzzy_normalize_maps_quotes_dashes_and_spaces() {
        assert_eq!(
            normalize_for_fuzzy_match("\u{201C}hello\u{201D}"),
            "\"hello\""
        );
        assert_eq!(normalize_for_fuzzy_match("a\u{2013}b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{00A0}b"), "a b");
        // 覆盖扩展字符集：破折号 / 空格 / 单引号
        assert_eq!(normalize_for_fuzzy_match("a\u{2010}b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{2011}b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{2012}b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{2212}b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{2003}b"), "a b");
        assert_eq!(normalize_for_fuzzy_match("a\u{3000}b"), "a b");
        assert_eq!(normalize_for_fuzzy_match("\u{2018}x\u{2019}"), "'x'");
        // 逐行只去行尾空白：行首缩进保留
        assert_eq!(normalize_for_fuzzy_match("  a  \n b\t\n"), "  a\n b\n");
    }

    /// 零宽字符**不是**空白的等价物：它们是断词 / 字形连接记号，不能删。
    #[test]
    fn fuzzy_normalize_keeps_zero_width_characters() {
        for ch in ['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'] {
            let text = format!("hello{ch}world");
            assert_eq!(normalize_for_fuzzy_match(&text), text);
        }
        // 组合字符仍是 NFKC 的一部分：合成为单字符
        assert_eq!(
            normalize_for_fuzzy_match("cafe\u{301}"),
            normalize_for_fuzzy_match("caf\u{E9}")
        );
    }

    /// 整串 NFKC 参照实现（与逐簇实现独立，用于校验等价性）。
    fn normalize_reference(s: &str) -> String {
        s.nfkc()
            .map(map_char)
            .collect::<String>()
            .split('\n')
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 逐簇归一化必须与整串 NFKC 等价，且所有映射端点落在字符边界上。
    ///
    /// 变异：簇判定漏掉组合字符（只认 ASCII）⇒ 组合字符样例的归一化比对变红；映射按字节推进
    /// ⇒ 边界断言变红。
    #[test]
    fn unit_normalization_matches_whole_string_normalization() {
        let samples = [
            "plain ascii\nsecond line  \n",
            "smart \u{201C}quotes\u{201D} and dash \u{2014} here\n",
            "nbsp\u{00A0}and ideographic\u{3000}space\n",
            "zero\u{200B}width\u{200D}joiners\u{FEFF}\n",
            "combining: cafe\u{301} vs caf\u{E9}\n",
            "hangul: \u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}\n",
            "ligature \u{FB01}ne\n",
            "emoji \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\n",
            "  尾行  \n",
            "",
            "no trailing newline  ",
            "\u{00A0}\u{3000}\n",
            "cjk \u{4E2D}\u{6587} mixed with \u{201C}quotes\u{201D}\n",
        ];

        for sample in samples {
            assert_eq!(
                normalize_for_fuzzy_match(sample),
                normalize_reference(sample),
                "unit-wise normalization diverged for {sample:?}"
            );

            for_each_unit(sample, |ch, start, end| {
                assert!(start < end && end <= sample.len(), "{sample:?}");
                assert!(sample.is_char_boundary(start), "{sample:?} start {start}");
                assert!(sample.is_char_boundary(end), "{sample:?} end {end}");
                // 单元必须非空且确实映射到原文
                assert!(ch.len_utf8() > 0);
            });

            // 视图的归一化内容必须与独立参照实现完全一致
            let view = NormalizedView::new(sample);
            assert_eq!(
                view.normalized,
                normalize_reference(sample),
                "view diverged for {sample:?}"
            );
            // 运行表规模由**行数**（纯 ASCII 行整行一个运行）与非 ASCII 簇数界定，与字节数
            // 无关 —— 这是模糊路径在 MiB 级文件上仍然 O(n) 的前提。
            let lines = sample.split_inclusive('\n').count();
            if sample.is_ascii() {
                assert!(
                    view.runs.len() <= lines,
                    "ascii run table must be line-scale for {sample:?}: {} runs",
                    view.runs.len()
                );
            }
            let bound = lines + sample.chars().count();
            assert!(
                view.runs.len() <= bound,
                "run table must stay char-scale for {sample:?}: {} runs > {bound}",
                view.runs.len()
            );

            // 视图的映射端点必须落在字符边界上
            for needle_len in 1..=view.normalized.len().min(6) {
                for start in 0..view.normalized.len() {
                    let end = start + needle_len;
                    if end > view.normalized.len()
                        || !view.normalized.is_char_boundary(start)
                        || !view.normalized.is_char_boundary(end)
                    {
                        continue;
                    }
                    let needle = &view.normalized[start..end];
                    match view.find(needle) {
                        FuzzyFind::Hit {
                            start: from,
                            end: to,
                        } => {
                            assert!(to <= sample.len(), "{sample:?} {needle:?}");
                            assert!(
                                sample.is_char_boundary(from),
                                "{sample:?} from {from} not a boundary"
                            );
                            assert!(
                                sample.is_char_boundary(to),
                                "{sample:?} to {to} not a boundary"
                            );
                            assert!(from <= to, "{sample:?} {from} > {to}");
                        }
                        FuzzyFind::PartialCluster {
                            start: from,
                            end: to,
                        } => {
                            // 报错用的簇区间同样必须落在字符边界上，且非空
                            assert!(from < to && to <= sample.len(), "{sample:?} {needle:?}");
                            assert!(sample.is_char_boundary(from), "{sample:?} {from}");
                            assert!(sample.is_char_boundary(to), "{sample:?} {to}");
                        }
                        FuzzyFind::Miss => {}
                    }
                }
            }
        }
    }

    /// 归一化命中映射回原文时，区间必须精确覆盖命中的原文片段（不含被去掉的行尾空白）。
    #[test]
    fn mapped_range_covers_exactly_the_matched_bytes() {
        let original = "foo\u{00A0}bar  \nbaz\n";
        let view = NormalizedView::new(original);
        let expect_hit = |needle: &str| match view.find(needle) {
            FuzzyFind::Hit { start, end } => (start, end),
            other => panic!("expected a hit for {needle:?}, got {other:?}"),
        };
        let needle = normalize_for_fuzzy_match("foo bar");
        let (from, to) = expect_hit(&needle);
        assert_eq!(&original[from..to], "foo\u{00A0}bar");

        // 命中落在行尾时不吞掉被去掉的行尾空白
        let needle = normalize_for_fuzzy_match("baz");
        let (from, to) = expect_hit(&needle);
        assert_eq!(&original[from..to], "baz");

        // 命中恰好是行尾空白之前的部分：右端点停在空白起点
        let (from, to) = expect_hit("bar");
        assert_eq!(&original[from..to], "bar");
        assert!(
            view.removed_trailing_whitespace(from, to).is_empty(),
            "a hit that stops before the line-ending blanks must not report them"
        );
    }

    /// 行尾风格由第一个换行符决定，而不是按数量投票。
    ///
    /// `\r`-only（经典 Mac）是独立一档：否则落盘时整个文件的换行风格被改成 LF。
    #[test]
    fn line_ending_comes_from_first_newline() {
        assert_eq!(detect_line_ending("a\r\nb\nc\nd\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\nb\r\nc\r\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("no newline"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\rb\rc\r"), LineEnding::Cr);
        // 混合：`\r\n` 与 `\n` 谁先出现就听谁
        assert_eq!(detect_line_ending("a\nb\rc\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb\rc\n"), LineEnding::Crlf);
        // 孤立的 `\r` 只算内容里一个字符，不参与风格投票：只要全文有 `\n`，
        // 前面的裸 `\r`（CSV 字段内嵌回车、Windows 工具输出、字符串字面量）不得把整文件判成 CR。
        assert_eq!(detect_line_ending("a\rb\nc\r"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\rb\r\nc\n"), LineEnding::Crlf);
    }

    /// 兼容字符簇的映射层级行为：半簇命中报 `PartialCluster`（原告文本区间），整簇命中报 `Hit`，
    /// 簇外的普通字符不受影响。
    ///
    /// 变异：不给展开簇打 atomic 标记 ⇒ 半簇命中变成 `Hit` ⇒ 断言变红。
    #[test]
    fn atomic_cluster_is_only_matched_whole() {
        let original = "deﬁne ½ x\n";
        let view = NormalizedView::new(original);
        // 归一化空间："define 1⁄2 x\n"
        assert_eq!(view.normalized, "define 1⁄2 x\n");

        // 半簇："f" / "i" 都只盖住 ﬁ 展开结果的一部分
        for needle in ["f", "i"] {
            match view.find(needle) {
                FuzzyFind::PartialCluster { start, end } => {
                    assert_eq!(&original[start..end], "ﬁ", "needle {needle:?}");
                }
                other => panic!("partial cluster {needle:?} must be rejected, got {other:?}"),
            }
        }
        // ½ 展开为 "1⁄2"：只打 "2" 也是半簇
        match view.find("2") {
            FuzzyFind::PartialCluster { start, end } => assert_eq!(&original[start..end], "½"),
            other => panic!("partial cluster \"2\" must be rejected, got {other:?}"),
        }

        // 整簇（带上前后普通字符）⇒ 正常命中，区间只盖住命中的原文
        let needle = normalize_for_fuzzy_match("ﬁne");
        match view.find(&needle) {
            FuzzyFind::Hit { start, end } => assert_eq!(&original[start..end], "ﬁne"),
            other => panic!("whole cluster must hit, got {other:?}"),
        }
        // 簇外字符照常命中
        match view.find("x") {
            FuzzyFind::Hit { start, end } => assert_eq!(&original[start..end], "x"),
            other => panic!("ordinary char must hit, got {other:?}"),
        }
    }

    /// 跨行命中吞掉多行的行尾空白时，每一处都单独报出来（不得拼成一串误报同一个位置）。
    #[test]
    fn multi_line_hit_reports_every_trailing_whitespace_run() {
        let original = "a  \nb\u{00A0}\u{00A0}\nc\n";
        let view = NormalizedView::new(original);
        let needle = normalize_for_fuzzy_match("a\nb\nc");
        let FuzzyFind::Hit { start, end } = view.find(&needle) else {
            panic!("expected a hit");
        };
        let removed: Vec<&str> = view
            .removed_trailing_whitespace(start, end)
            .into_iter()
            .map(|(from, to)| &original[from..to])
            .collect();
        assert_eq!(removed, ["  ", "\u{00A0}\u{00A0}"]);
    }

    /// 非重叠计数给出真实次数（歧义报错要用它），空 needle 一律计 0（调用方先行拒绝）。
    #[test]
    fn count_occurrences_is_non_overlapping() {
        assert_eq!(count_occurrences("aaaa", "aa"), 2);
        assert_eq!(count_occurrences("aaa", "aa"), 1);
        assert_eq!(count_occurrences("abc", "z"), 0);
        assert_eq!(count_occurrences("abc", ""), 0);
        assert_eq!(count_occurrences("x y x y x", "x"), 3);
    }
}
