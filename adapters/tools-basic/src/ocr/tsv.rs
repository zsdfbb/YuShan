//! OCR 结果的解析、过滤与渲染 —— **纯函数、零 IO、零 tokio**。
//!
//! **分工（重要）**：本模块只算「有哪些行、保留哪些行、每行长什么样」；**头行的字段值与
//! 一切 notice 文案由调用方拼**。空结果的三种成因（真没字 / 全被滤 / 缺语言包）由调用方
//! 依据 [`FilterStats`] 与引擎 stderr 判定，本模块只如实计数、不打标语。
//!
//! # 输入契约：TSV 列序（Swift ↔ Rust 的同一份契约，见设计 §4.1 / §9）
//!
//! 引擎输出 12 列、以 `\t` 分隔、一行一条，列序固定：
//!
//! ```text
//! level  page_num  block_num  par_num  line_num  word_num  left  top  width  height  conf  text
//!   0       1         2          3        4         5        6     7     8       9     10   11
//! ```
//!
//! - `level == 4` ⇒ **行**，`level == 5` ⇒ **词**；其余 level（页 / 块 / 段 / 空行）一律忽略。
//! - 除 `conf` 外，前 11 列必须能解析成**整数**（不接受浮点 / 空串之外的形态）；`conf` 见
//!   下条；`text` 是**第 12 列起的全部剩余内容**，其中不得含 `\t` —— 引擎侧负责 sanitize
//!   （tesseract 天然如此，Vision 脚本必须吐同样形态）。
//! - 列数不足 12 的行、以及对不上数字的行**跳过不报错**：引擎吐的脏行不该让整个工具失败。
//! - `conf` 列是 0–100 的置信度：**tesseract 5.x 输出浮点数**（`96.748116`），旧版本输出整数；
//!   页 / 块 / 行级记录用 `-1` 表示「该层级无置信度」（页 / 块级已被 level 过滤掉，行级的
//!   `-1` 必须留下）。解析后由 [`parse_conf`] 统一折算成 [`OcrLine::conf`] 的 `u8`：四舍五入
//!   进 `[0, 100]`，负数（含 `-1`）⇒ `0`，`> 100` 视为脏列整条跳过。
//!   无置信度的后端（Vision）**必须填 `0`**，0 是有效值（见下「绝不过滤」）。
//!
//! # 行记录是**行锚点**：`text` 允许为空
//!
//! `level == 4` 的 `text` 列**不保证有字**：tesseract 5.x 的行级记录恒为空（文字只出现在
//! `level == 5` 词记录里），Vision 的行级与词级都填。故行记录的职责是「给出该行的坐标框
//! 与行号」，它**无条件**成为锚点 —— 按空 text 丢弃会让后续词记录全部无处可挂（结果恒空）。
//! 锚点自带 text 为空时，行文本在解析末尾由该行的词拼接（见 [`join_words`]）：行级渲染与
//! [`filter`] 都只看行记录的 `text`，空串既能渲染成空行，也会被当成纯空白滤掉。
//!
//! # 过滤：只丢碎片，**绝不按 `conf` 过滤**
//!
//! [`filter`] 只丢两种行：纯空白（`text.trim()` 为空）与 `w < 6` px 的窄碎片（行号竖线、
//! 引号）。**`conf == 0` 不是过滤条件** —— 实测 `conf: 0` 的行是真文本，按置信度过滤会
//! 静默吞掉整段结果，而模型会把它读成「图里没字」。被滤行数经 [`FilterStats::filtered`]
//! 回给调用方，它是空结果判定的唯一判据。
//!
//! # 输出格式（行级 / 词级）
//!
//! 头行 + 每行一条，形态由 [`Granularity`] 决定；`conf_min:` / `conf:` 前缀**恒出现**，
//! 不因 0 或缺失而省略 —— 二者标签不同，模型据此区分「最小值」与「该词值」。

/// 一行 OCR 结果（行级与词级共用同一结构，靠 [`OcrLine::word_no`] 区分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrLine {
    /// 本结果序号，从 1 递增。**不是**引擎的 `line_num`（那会因跳过空行而跳号）。
    pub line_no: usize,
    /// `Some` ⇒ 词级（同一行内从 1 递增）；`None` ⇒ 行级。
    pub word_no: Option<usize>,
    /// 左上原点的原图像素坐标与尺寸。
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// `level == 5` 词的最小置信度（行级是聚合值）；无置信度的后端填 0。
    pub conf: u8,
    pub text: String,
}

/// `left` / `top` / `width` / `height` / `conf` 列下标。
const COL_LEFT: usize = 6;
const COL_TOP: usize = 7;
const COL_WIDTH: usize = 8;
const COL_HEIGHT: usize = 9;
const COL_CONF: usize = 10;
/// `text` 列下标；其后的列（若有）一并并入正文。
const COL_TEXT: usize = 11;
/// `level == 4` ⇒ 行框。
const LEVEL_LINE: u16 = 4;
/// `level == 5` ⇒ 词框。
const LEVEL_WORD: u16 = 5;
/// 窄于该宽度（px）的行视为碎片（行号竖线、引号）。
const MIN_WIDTH: u32 = 6;

/// 解析 TSV 成 [`OcrLine`] 列表。
///
/// 只取 `level == 4`（行）与 `level == 5`（词）两类记录，并按出现顺序重编行号与词号 ——
/// 行号从 1 起、同一行的词号从 1 起。
///
/// **行记录无条件保留**（`text` 为空是 tesseract 的正常形态，见模块头）：它提供该行的坐标框
/// 与行号，文字由紧随其后的词记录填充；词记录出现在任何行记录之前 ⇒ 整条丢弃（列序已乱，
/// 不编造行号）。**词记录的 `text` 为空则跳过** —— 空词框贡献不了可读内容，也不该参与
/// 行级 `conf` 的最小值计算。行记录自带 text 为空时，其 text 在返回前由该行的词拼接。
///
/// 词级行的 `conf` 取该词自身的置信度，行级行的 `conf` 取该行 [`LEVEL_WORD`] 记录的
/// **最小值**（无词记录 ⇒ 0）。文本为空的词不参与最小值的计算，否则整行会被一个空框
/// 拉到 `conf: 0`。行级 `conf` 在内部以一个不可能的哨兵值起步，函数返回前折算成 0 ——
/// 直接以 0 起步会把任何非零词置信度算成 0。
///
/// 畸形行（列数不足、数字字段解析失败）**跳过**。坐标 / 尺寸字段越界（负数、> `u32::MAX`）
/// 以及 `conf > 100` 的脏列同样跳过：宁可少一行，也不把错误坐标喂给模型。
pub fn parse_tsv(raw: &str) -> Vec<OcrLine> {
    let mut lines: Vec<OcrLine> = Vec::new();
    // 当前正在收集词的行（`level == 4` 最近一条）在 `lines` 里的下标。
    let mut current_line: Option<usize> = None;
    // 当前行的下一个词号；每遇到新的行记录重置为 1。
    let mut next_word_no: usize = 1;

    /// 行级 `conf` 的哨兵初值：真实置信度不可能到达（> 100），返回前折算成 0。
    const NO_WORD_CONF: u8 = u8::MAX;

    for raw_line in raw.lines() {
        let mut cols = raw_line.split('\t');
        // 前 11 列是数字，`text` 是余下全部列拼回去（引擎若在文本里漏了 `\t`，这里兜住）。
        let numbers: Option<Vec<&str>> = (0..COL_TEXT).map(|_| cols.next()).collect();
        let Some(numbers) = numbers else {
            continue;
        };
        let text = cols.collect::<Vec<_>>().join("\t");

        let Some(level) = parse_number::<u16>(numbers[0]) else {
            continue;
        };
        if level != LEVEL_LINE && level != LEVEL_WORD {
            continue;
        }
        let (Some(left), Some(top), Some(width), Some(height), Some(conf)) = (
            parse_number::<u32>(numbers[COL_LEFT]),
            parse_number::<u32>(numbers[COL_TOP]),
            parse_number::<u32>(numbers[COL_WIDTH]),
            parse_number::<u32>(numbers[COL_HEIGHT]),
            parse_conf(numbers[COL_CONF]),
        ) else {
            continue;
        };

        match level {
            LEVEL_WORD => {
                let Some(index) = current_line else {
                    // 词之前没有行记录：列序已乱，整条丢弃（不编造一个行号）。
                    continue;
                };
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                if conf < lines[index].conf {
                    lines[index].conf = conf;
                }
                let word_no = next_word_no;
                next_word_no = next_word_no.saturating_add(1);
                lines.push(OcrLine {
                    line_no: lines[index].line_no,
                    word_no: Some(word_no),
                    x: left,
                    y: top,
                    w: width,
                    h: height,
                    conf,
                    text,
                });
            }
            _ => {
                // 行级记录是锚点：**不判 text**（tesseract 的行级 text 恒为空，文字在词记录里）。
                let text = text.trim().to_string();
                let line_no = lines
                    .iter()
                    .filter(|line| line.word_no.is_none())
                    .count()
                    .saturating_add(1);
                lines.push(OcrLine {
                    line_no,
                    word_no: None,
                    x: left,
                    y: top,
                    w: width,
                    h: height,
                    conf: NO_WORD_CONF,
                    text,
                });
                current_line = Some(lines.len() - 1);
                next_word_no = 1;
            }
        }
    }

    for line in &mut lines {
        if line.conf == NO_WORD_CONF {
            // 无词记录：无置信度可聚合。行记录自身的 `conf`（tesseract 给 `-1`）不参与
            // 最小值计算 —— 它不是词置信度，混进来会把整行拉到 0。
            line.conf = 0;
        }
    }

    // 行锚点自带 text 为空时，用该行的词拼出行文本。词记录恒排在其行锚点之后，
    // 直到下一个锚点为止的连续 `Some(word_no)` 就是本行的全部词。
    let line_texts: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.word_no.is_none() && line.text.trim().is_empty())
        .map(|(index, _)| {
            let words: Vec<&OcrLine> = lines[index + 1..]
                .iter()
                .take_while(|word| word.word_no.is_some())
                .collect();
            (index, join_words(&words))
        })
        .collect();
    for (index, text) in line_texts {
        lines[index].text = text;
    }

    lines
}

/// 把同一行的词拼成行文本。
///
/// TSV 不记录「词间有没有空格」，只能看**字间距**：
///
/// - 相邻两词中只要有一个不是 CJK ⇒ 插一个空格（西文词之间必是词边界）；
/// - 两个词都是 CJK ⇒ **通常不插**（tesseract 把中文逐字切成词，插空格会得到
///   `第 三 章`），但字间距达到两词较大行高的 `2/3` 时视为原文里就有空格。
///
/// 于是真机中英混排两行分别拼出 `第三章 配置文件说明` 与 `Hello OCR test 2026`。
fn join_words(words: &[&OcrLine]) -> String {
    /// CJK 之间的空格判据：字间距达到行高的这个比例 ⇒ 原文里有空格。全角字的字面宽度约为
    /// 行高，正常字距远小于行高，取 2/3 落在实测量级（行高 37 px、字距 28 px vs 20 px）的两端。
    const CJK_SPACE_RATIO: f64 = 2.0 / 3.0;

    let mut out = String::new();
    for (index, word) in words.iter().enumerate() {
        out.push_str(&word.text);
        let Some(next) = words.get(index + 1) else {
            break;
        };
        let gap = next.x.saturating_sub(word.x.saturating_add(word.w));
        let cjk_pair = word.text.chars().next_back().is_some_and(is_cjk)
            && next.text.chars().next().is_some_and(is_cjk);
        let spaced = CJK_SPACE_RATIO * f64::from(word.h.max(next.h));
        if !cjk_pair || (gap as f64) >= spaced {
            out.push(' ');
        }
    }
    out
}

/// CJK 表意文字（基本区 / 扩展 A / 兼容区）。只用于 [`join_words`] 的分词间距判断。
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
    )
}

/// 解析单个数字列；空串、负数、非数字、越界一律 `None`。
fn parse_number<T: std::str::FromStr>(raw: &str) -> Option<T> {
    raw.trim().parse::<T>().ok()
}

/// 解析 `conf` 列。
///
/// tesseract 5.x 输出**浮点数**（`96.748116`），旧版本输出整数；行级 / 页级 / 块级记录用
/// `-1` 表示「该层级无置信度」。
///
/// - 非数字（含空串、`NaN`、`inf`）⇒ `None`，调用方跳过该整条记录；
/// - 负数（含 `-1`）⇒ `Some(0)` —— 无置信度折成 0，**不是**跳过该行（0 是有效值，见模块头
///   「绝不过滤」）；
/// - `> 100` ⇒ `None`：脏列跳过，且保证结果不会撞上行级内部哨兵（`u8::MAX`）；
/// - 其余四舍五入并夹到 `[0, 100]`。
fn parse_conf(raw: &str) -> Option<u8> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    if value < 0.0 {
        return Some(0);
    }
    if value > 100.0 {
        return None;
    }
    Some(value.round().clamp(0.0, 100.0) as u8)
}

/// 渲染粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// 每行一条，`conf_min:` 是该行词的最小置信度。
    Line,
    /// 每个词一条（`<line_no>.<word_no>`），`conf:` 是该词自身的值。
    Word,
}

/// 过滤统计。
///
/// `filtered` 是空结果三分的唯一判据：`lines == 0 && filtered == 0` ⇒ 图里真没字；
/// `lines == 0 && filtered > 0` ⇒ 全被滤掉（细笔画 / 竖排单字），文案应提示换 `word_boxes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterStats {
    /// 保留的行数（词级行按词计数，与渲染行数一致）。
    pub kept: usize,
    /// 被丢弃的行数（纯空白 + `w < 6`）。
    pub filtered: usize,
}

/// 丢弃纯空白行与 `w < 6` 的窄碎片。**不按 `conf` 过滤**（见模块头注释）。
///
/// `kept + filtered` 恒等于入参长度 —— 两种丢弃互斥且穷尽。
pub fn filter(lines: Vec<OcrLine>) -> (Vec<OcrLine>, FilterStats) {
    let total = lines.len();
    let mut kept = Vec::with_capacity(total);
    for line in lines {
        if line.text.trim().is_empty() || line.w < MIN_WIDTH {
            continue;
        }
        kept.push(line);
    }
    let stats = FilterStats {
        kept: kept.len(),
        filtered: total - kept.len(),
    };
    (kept, stats)
}

/// 渲染头行所需的字段。**头行的字节形态**由本模块给定，字段**值**由调用方算。
pub struct Header<'a> {
    /// 图片文件名（basename，不含目录）。
    pub name: &'a str,
    /// 原图像素尺寸。
    pub width: u32,
    pub height: u32,
    /// 实际成功的后端（`tesseract` / `vision`）。
    pub via: &'a str,
    /// 实际生效的语种；Vision 下必须是 `n/a`（不回显入参）。
    pub lang: &'a str,
    /// 保留行数，即 [`FilterStats::kept`]。
    pub lines: usize,
    /// 被滤行数，即 [`FilterStats::filtered`]。
    pub filtered: usize,
    /// 降级原因（`vision-failed` / `lang-missing` / `word-boxes-unsupported`）；
    /// `None` ⇒ 头行结尾不留 ` degraded=` 痕迹。
    pub degraded: Option<&'a str>,
}

/// 渲染头行 + 各行，行尾带 `\n`（便于直接拼 notice）。
///
/// 每行按自身 `word_no` 决定形态：`None` ⇒ `conf_min:`（行级），`Some` ⇒ `<line_no>.<word_no>`
/// 与 `conf:`（词级）。**不二次过滤** —— [`filter`] 已做过一轮，这里再判一次宽度会让
/// `lines=` 与实际渲染行数对不上。粒度挑选见 [`render_at`]。
pub fn render(header: &Header, lines: &[OcrLine]) -> String {
    let mut out = format!(
        "# {} {}x{} via={} lang={} lines={} filtered={}",
        header.name,
        header.width,
        header.height,
        header.via,
        header.lang,
        header.lines,
        header.filtered
    );
    if let Some(reason) = header.degraded {
        out.push_str(" degraded=");
        out.push_str(reason);
    }
    out.push('\n');

    for line in lines {
        match line.word_no {
            Some(word_no) => out.push_str(&format!(
                "{}.{} ({},{},{},{}) conf:{} {}\n",
                line.line_no, word_no, line.x, line.y, line.w, line.h, line.conf, line.text
            )),
            None => out.push_str(&format!(
                "{} ({},{},{},{}) conf_min:{} {}\n",
                line.line_no, line.x, line.y, line.w, line.h, line.conf, line.text
            )),
        }
    }

    out
}

/// 渲染**指定**粒度的行；行号按该粒度的可见行重编（跳过被隐藏的另一种粒度）。
///
/// 这是 [`render`] 的粒度化入口：调用方先 [`filter`]、再按 [`Granularity`] 挑行，
/// 本函数只负责「哪些行可见 + 序号连续」，不重复计数。
pub fn render_at(header: &Header, lines: &[OcrLine], granularity: Granularity) -> String {
    let selected: Vec<OcrLine> = lines
        .iter()
        .filter(|line| match granularity {
            Granularity::Line => line.word_no.is_none(),
            Granularity::Word => line.word_no.is_some(),
        })
        .cloned()
        .collect();
    render(header, &selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `level` 等前 11 列 + 文本，造一条 tesseract TSV 记录。
    fn tsv_row(
        level: u16,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        conf: u8,
        text: &str,
    ) -> String {
        format!("{level}\t1\t1\t1\t1\t1\t{left}\t{top}\t{width}\t{height}\t{conf}\t{text}")
    }

    /// `conf` 列按**原始字符串**写入：`tsv_row` 只能表达整数，而真机吐的是浮点 / `-1`。
    fn tsv_row_conf(
        level: u16,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        conf: &str,
        text: &str,
    ) -> String {
        format!("{level}\t1\t1\t1\t1\t1\t{left}\t{top}\t{width}\t{height}\t{conf}\t{text}")
    }

    fn line(text: &str, w: u32, conf: u8) -> OcrLine {
        OcrLine {
            line_no: 1,
            word_no: None,
            x: 0,
            y: 0,
            w,
            h: 20,
            conf,
            text: text.to_string(),
        }
    }

    /// design.md §1 的逐字节样例 —— 钉死输出格式的唯一真值。
    #[test]
    fn golden_fixture_row_level() {
        let header = Header {
            name: "shot.png",
            width: 1200,
            height: 900,
            via: "tesseract",
            lang: "chi_sim+eng",
            lines: 2,
            filtered: 0,
            degraded: None,
        };
        let lines = vec![
            OcrLine {
                line_no: 1,
                word_no: None,
                x: 62,
                y: 49,
                w: 890,
                h: 28,
                conf: 96,
                text: "第三章 配置文件说明".into(),
            },
            OcrLine {
                line_no: 2,
                word_no: None,
                x: 61,
                y: 116,
                w: 906,
                h: 26,
                conf: 91,
                text: "应用程序启动时会依次读取以下三个位置的配置文件，".into(),
            },
        ];

        assert_eq!(
            render(&header, &lines),
            "# shot.png 1200x900 via=tesseract lang=chi_sim+eng lines=2 filtered=0\n\
             1 (62,49,890,28) conf_min:96 第三章 配置文件说明\n\
             2 (61,116,906,26) conf_min:91 应用程序启动时会依次读取以下三个位置的配置文件，\n"
        );
    }

    /// design.md §1 的词级样例：`conf:0` 与 `degraded=` 同在。
    #[test]
    fn golden_fixture_word_level() {
        let header = Header {
            name: "shot.png",
            width: 1200,
            height: 900,
            via: "vision",
            lang: "n/a",
            lines: 1,
            filtered: 0,
            degraded: Some("word-boxes-unsupported"),
        };
        let lines = vec![OcrLine {
            line_no: 1,
            word_no: Some(1),
            x: 62,
            y: 49,
            w: 890,
            h: 28,
            conf: 0,
            text: "第三章".into(),
        }];

        assert_eq!(
            render(&header, &lines),
            "# shot.png 1200x900 via=vision lang=n/a lines=1 filtered=0 \
             degraded=word-boxes-unsupported\n\
             1.1 (62,49,890,28) conf:0 第三章\n"
        );
    }

    #[test]
    fn parse_tsv_keeps_lines_and_words() {
        let raw = [
            tsv_row(4, 62, 49, 890, 28, 96, "hello world"),
            tsv_row(5, 62, 49, 100, 28, 98, "hello"),
            tsv_row(5, 200, 49, 120, 28, 96, "world"),
        ]
        .join("\n");

        let parsed = parse_tsv(&raw);
        assert_eq!(parsed.len(), 3);
        // 行级 conf = 该行词的最小值。
        assert_eq!(parsed[0].line_no, 1);
        assert_eq!(parsed[0].word_no, None);
        assert_eq!(parsed[0].conf, 96);
        assert_eq!(parsed[1].word_no, Some(1));
        assert_eq!(parsed[1].x, 62);
        assert_eq!(parsed[2].word_no, Some(2));
        assert_eq!(parsed[2].text, "world");
    }

    // 变异测试点：`parse_conf` 退回 `parse::<u8>` ⇒ 本用例必须变红（tesseract 5.x 吐浮点）。
    #[test]
    fn parse_conf_rounds_real_float_output() {
        // 真机样本：`tesseract 5.5.4` 的 `conf` 列（`96.748116` ⇒ 四舍五入到 97）。
        assert_eq!(parse_conf("96.748116"), Some(97));
        assert_eq!(parse_conf("93.212006"), Some(93));
        // 旧版 tesseract 的整数字形态仍然接受。
        assert_eq!(parse_conf("96"), Some(96));
    }

    /// 行级 / 页级 / 块级记录的 `-1` 是「无置信度」，折成 0 而不是丢弃整条。
    #[test]
    fn parse_conf_folds_minus_one_to_zero() {
        assert_eq!(parse_conf("-1"), Some(0));
        assert_eq!(parse_conf("-1.0"), Some(0));

        let parsed = parse_tsv(&tsv_row_conf(4, 62, 49, 890, 28, "-1", "你好"));
        assert_eq!(parsed.len(), 1, "conf=-1 的行级记录必须留下");
        assert_eq!(parsed[0].conf, 0);
    }

    #[test]
    fn parse_conf_boundaries() {
        assert_eq!(parse_conf("100"), Some(100));
        assert_eq!(parse_conf("100.0"), Some(100));
        assert_eq!(parse_conf("0"), Some(0));
        assert_eq!(parse_conf("0.0"), Some(0));
        // 略小于 100 的浮点四舍五入后正好撞上上界。
        assert_eq!(parse_conf("99.5"), Some(100));
        // 非数字形态仍是脏列。
        assert_eq!(parse_conf(""), None);
        assert_eq!(parse_conf("n/a"), None);
    }

    /// `> 100` 的脏列保留既有语义：**整条跳过**，不放宽成截断。
    #[test]
    fn parse_conf_rejects_above_hundred() {
        assert_eq!(parse_conf("150.5"), None);
        assert_eq!(parse_conf("100.5"), None);
        assert!(parse_tsv(&tsv_row_conf(5, 0, 0, 50, 20, "150.5", "x")).is_empty());
    }

    /// 真机 TSV 片段端到端：行级 `conf=-1` + 两条浮点 `conf` 的词。
    #[test]
    fn parse_tsv_accepts_real_tesseract_5_output() {
        let raw = [
            tsv_row_conf(4, 57, 47, 890, 28, "-1", "第三章"),
            tsv_row_conf(5, 63, 53, 35, 37, "96.748116", "第"),
            tsv_row_conf(5, 116, 57, 21, 29, "93.212006", "三"),
        ]
        .join("\n");

        let parsed = parse_tsv(&raw);
        assert_eq!(parsed.len(), 3, "行 + 两个词都必须保留");
        assert_eq!(parsed[0].word_no, None);
        assert_eq!(parsed[0].conf, 93, "行级 conf 取两词最小值 93.212006 ⇒ 93");
        assert_eq!(parsed[1].conf, 97);
        assert_eq!(parsed[1].text, "第");
        assert_eq!(parsed[2].word_no, Some(2));
        assert_eq!(parsed[2].conf, 93);
    }

    /// 真机 TSV 全文（`tesseract 5.5.4` + `chi_sim+eng` + `--psm 6`，1200x400 PNG，中英混排两行）。
    ///
    /// **此 fixture 取自真机输出，不要改写**：两条 `level == 4` 行记录的 text 列**恒为空**
    /// （第 12 列之后什么都没有，行尾就是那个 `\t`），文字只出现在 `level == 5` 词记录里；
    /// 两行记录的 `line_num` 恰好是 1 / 2，但 `line_no` 按「第几条行记录」算，不读该列。
    const REAL_TESSERACT_TSV: &str = "\
4\t1\t1\t1\t1\t0\t63\t52\t363\t38\t-1\t\n\
5\t1\t1\t1\t1\t1\t63\t53\t35\t37\t96.748116\t第\n\
5\t1\t1\t1\t1\t2\t116\t57\t21\t29\t93.212006\t三\n\
5\t1\t1\t1\t1\t3\t157\t52\t20\t37\t93.111855\t章\n\
5\t1\t1\t1\t1\t4\t205\t53\t87\t36\t94.062805\t配置\n\
5\t1\t1\t1\t1\t5\t312\t53\t53\t36\t96.532242\t文件\n\
5\t1\t1\t1\t1\t6\t368\t53\t36\t36\t93.272507\t说\n\
5\t1\t1\t1\t1\t7\t404\t55\t22\t34\t89.289970\t明\n\
4\t1\t1\t1\t2\t0\t63\t152\t371\t32\t-1\t\n\
5\t1\t1\t1\t2\t1\t63\t153\t90\t30\t96.238342\tHello\n\
5\t1\t1\t1\t2\t2\t167\t153\t88\t31\t96.276039\tOCR\n\
5\t1\t1\t1\t2\t3\t268\t153\t68\t30\t95.370544\ttest\n\
5\t1\t1\t1\t2\t4\t348\t152\t86\t31\t95.370544\t2026";

    /// 真机形态端到端：`level == 4` 的 text 恒为空 ⇒ 行锤点仍必须成立，行 text 由词拼接。
    ///
    /// 变异测试靶心：把「行级 text 空 ⇒ `continue`」改回去 ⇒ 本用例产出 `0 条`，全红。
    #[test]
    fn parse_tsv_real_tesseract_rows_have_empty_text() {
        let parsed = parse_tsv(REAL_TESSERACT_TSV);
        assert_eq!(parsed.len(), 13, "2 条行记录 + 11 条词记录都必须保留");

        let lines: Vec<&OcrLine> = parsed.iter().filter(|l| l.word_no.is_none()).collect();
        assert_eq!(lines.len(), 2, "行记录不能被空 text 滤掉");
        assert_eq!(lines[0].line_no, 1);
        assert_eq!(
            (lines[0].x, lines[0].y, lines[0].w, lines[0].h),
            (63, 52, 363, 38)
        );
        assert_eq!(lines[0].text, "第三章 配置文件说明");
        assert_eq!(lines[0].conf, 89, "7 个词的最小值 89.289970 ⇒ 89");
        assert_eq!(lines[1].line_no, 2);
        assert_eq!(
            (lines[1].x, lines[1].y, lines[1].w, lines[1].h),
            (63, 152, 371, 32)
        );
        assert_eq!(lines[1].text, "Hello OCR test 2026");
        assert_eq!(lines[1].conf, 95, "4 个词的最小值 95.370544 ⇒ 95");

        let words: Vec<&OcrLine> = parsed.iter().filter(|l| l.word_no.is_some()).collect();
        assert_eq!(words.len(), 11);
        // 词只挂到自己那一行；行级 `conf` 不因 `-1` 被拉到 0。
        assert_eq!(words[6].line_no, 1);
        assert_eq!(words[6].word_no, Some(7));
        assert_eq!(words[6].text, "明");
        assert_eq!(words[7].line_no, 2);
        assert_eq!(words[7].word_no, Some(1));
        assert_eq!(words[7].text, "Hello");

        // 行级渲染与过滤都只看行记录的 `text`：拼接后不再被当成纯空白滤掉。
        let (kept, stats) = filter(parsed);
        assert_eq!(stats.kept, 13);
        assert_eq!(stats.filtered, 0);
        assert_eq!(
            render_at(
                &Header {
                    name: "shot.png",
                    width: 1200,
                    height: 400,
                    via: "tesseract",
                    lang: "chi_sim+eng",
                    lines: 2,
                    filtered: 0,
                    degraded: None,
                },
                &kept,
                Granularity::Line,
            ),
            "# shot.png 1200x400 via=tesseract lang=chi_sim+eng lines=2 filtered=0\n\
             1 (63,52,363,38) conf_min:89 第三章 配置文件说明\n\
             2 (63,152,371,32) conf_min:95 Hello OCR test 2026\n"
        );
    }

    /// 最小形态：行级 text 空 + 两个词 ⇒ 行锤点**必须**设置 `current_line`，词不被丢弃。
    #[test]
    fn line_anchor_without_text_keeps_following_words() {
        let raw = [
            tsv_row_conf(4, 10, 20, 300, 40, "-1", ""),
            tsv_row_conf(5, 10, 20, 60, 40, "90.0", "a"),
            tsv_row_conf(5, 90, 20, 60, 40, "80.0", "b"),
        ]
        .join("\n");

        let parsed = parse_tsv(&raw);
        assert_eq!(parsed.len(), 3, "空 text 的行记录 + 两个词都要留下");
        assert_eq!(parsed[0].word_no, None);
        assert_eq!(parsed[0].text, "a b", "行 text 由词拼接");
        assert_eq!(parsed[0].conf, 80);
        assert_eq!(parsed[1].line_no, 1);
        assert_eq!(parsed[1].word_no, Some(1));
        assert_eq!(parsed[2].word_no, Some(2));
    }

    /// 多行：行级 text 空，词各自的归属与行号都不串；无词的行 null 结果仍是空 text。
    #[test]
    fn parse_tsv_attributes_words_to_their_own_line() {
        let raw = [
            tsv_row_conf(4, 10, 20, 300, 40, "-1", ""),
            tsv_row_conf(5, 10, 20, 60, 40, "90.0", "a"),
            tsv_row_conf(5, 90, 20, 60, 40, "94.0", "b"),
            tsv_row_conf(4, 10, 80, 300, 40, "-1", ""),
            tsv_row_conf(5, 10, 80, 60, 40, "70.0", "c"),
            tsv_row_conf(5, 90, 80, 60, 40, "60.0", "d"),
        ]
        .join("\n");

        let parsed = parse_tsv(&raw);
        assert_eq!(parsed.len(), 6);
        assert_eq!(
            parsed
                .iter()
                .map(|l| (l.line_no, l.word_no, l.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, None, "a b"),
                (1, Some(1), "a"),
                (1, Some(2), "b"),
                (2, None, "c d"),
                (2, Some(1), "c"),
                (2, Some(2), "d"),
            ]
        );
        assert_eq!(parsed[0].conf, 90);
        assert_eq!(parsed[3].conf, 60);
    }

    /// Vision 形态：行级**自带** text ⇒ 直接用行文本，词不参与拼接；两边都绿。
    #[test]
    fn vision_shape_line_text_is_not_overwritten_by_words() {
        let raw = [
            tsv_row_conf(4, 62, 49, 890, 28, "0", "第三章 配置文件说明"),
            tsv_row_conf(5, 62, 49, 100, 28, "0", "第三章"),
            tsv_row_conf(5, 200, 49, 120, 28, "0", "配置文件说明"),
        ]
        .join("\n");

        let parsed = parse_tsv(&raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].text, "第三章 配置文件说明");
        assert_eq!(parsed[0].conf, 0, "Vision 的 conf 恒为 0，是有效值");
    }

    // 变异测试点：若 `filter` 加上 `conf == 0` 过滤，本用例必须变红（design §8 第 6 条）。
    #[test]
    fn filter_keeps_conf_zero_nonempty_lines() {
        let lines = vec![line("真文本", 100, 0)];
        let (kept, stats) = filter(lines);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].conf, 0);
        assert_eq!(
            stats,
            FilterStats {
                kept: 1,
                filtered: 0
            }
        );
    }

    #[test]
    fn filter_drops_blank_and_narrow() {
        let lines = vec![
            line("正文", 100, 90),
            line("   ", 100, 90),
            line("", 100, 90),
            line("1", 5, 90),    // 行号竖线：宽 5 < 6
            line("引号", 6, 90), // 恰好 6 ⇒ 保留
        ];
        let (kept, stats) = filter(lines);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].text, "正文");
        assert_eq!(kept[1].text, "引号");
        assert_eq!(
            stats,
            FilterStats {
                kept: 2,
                filtered: 3
            }
        );
    }

    /// §3.2 的空结果三分靠 `filtered` 计数：全被滤掉时 `kept==0 && filtered>0`。
    #[test]
    fn filter_counts_all_filtered_case() {
        let lines = vec![line("1", 3, 80), line("  ", 40, 80)];
        let (kept, stats) = filter(lines);
        assert!(kept.is_empty());
        assert_eq!(
            stats,
            FilterStats {
                kept: 0,
                filtered: 2
            }
        );
    }

    #[test]
    fn parse_tsv_tolerates_malformed_input() {
        assert!(parse_tsv("").is_empty());
        assert!(parse_tsv("\n\n\n").is_empty());
        // 列数不足。
        assert!(parse_tsv("4\t1\t1\t1").is_empty());
        // 非数字坐标。
        assert!(parse_tsv("4\t1\t1\t1\t1\t1\tx\t1\t1\t1\t90\thello").is_empty());
        // 未知 level（页 / 块 / 段 / 空行）。
        assert!(parse_tsv(&tsv_row(1, 0, 0, 1200, 900, 0, "")).is_empty());
        // 越界 `conf`（> 100）：脏列整条丢，行级 conf 才不会与内部哨兵撞值。
        assert!(parse_tsv(&tsv_row(5, 0, 0, 50, 20, 200, "x")).is_empty());
        // 好行夹在脏行中间仍然保留。
        let raw = [
            "garbage".to_string(),
            tsv_row(4, 10, 20, 100, 20, 90, "ok"),
            "4\t1\t1".to_string(),
        ]
        .join("\n");
        let parsed = parse_tsv(&raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "ok");
    }

    #[test]
    fn parse_tsv_keeps_chinese_text_intact() {
        let text = "第三章 配置文件说明 — 应用程序启动时会依次读取";
        let parsed = parse_tsv(&tsv_row(4, 62, 49, 890, 28, 96, text));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, text);
        assert_eq!(parsed[0].text.chars().count(), text.chars().count());
    }

    #[test]
    fn render_at_selects_one_granularity() {
        let raw = [
            tsv_row(4, 62, 49, 890, 28, 96, "ab"),
            tsv_row(5, 62, 49, 100, 28, 98, "a"),
            tsv_row(5, 200, 49, 120, 28, 95, "b"),
        ]
        .join("\n");
        let (lines, stats) = filter(parse_tsv(&raw));
        assert_eq!(stats.kept, 3);

        let header = Header {
            name: "x.png",
            width: 100,
            height: 50,
            via: "tesseract",
            lang: "eng",
            lines: stats.kept,
            filtered: stats.filtered,
            degraded: None,
        };
        let row = render_at(&header, &lines, Granularity::Line);
        assert!(row.contains("\n1 (62,49,890,28) conf_min:95 ab\n"));
        let word = render_at(&header, &lines, Granularity::Word);
        assert!(word.contains("\n1.1 (62,49,100,28) conf:98 a\n"));
        assert!(word.contains("\n1.2 (200,49,120,28) conf:95 b\n"));
    }
}
