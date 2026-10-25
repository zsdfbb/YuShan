//! assistant markdown → owned `Line` —— 库胶水 + 表格窄屏守卫（设计 §2.2 / §2.6）。
//!
//! 本模块是 `tui_markdown` 的唯一「调用点」：把库返回的 `Text<'a>` 收成
//! `Vec<Line<'static>>`（缓存需要 owned），并在返回前执行**表格窄屏守卫** —— 库不保证
//! 表格输出行宽 ≤ `width`，越界的**表格行**会被消费方的 `Paragraph` 二次折行、损坏表格（设计 §2.6）。
//! 守卫**只针对表格行**：库不 wrap 正文，正文超宽是常态、由 `Paragraph` 折行。
//!
//! `tui_markdown::` 路径只准出现在 `md.rs` / `styles.rs`（R6 缓解）。

use ratatui::text::{Line, Span};
use tui_markdown::Options;

use crate::styles::AssistantStyles;

/// 表格框线字符（库画表格只用这些）。产出行含其中任一字符即视为**表格行**。
///
/// **不把 `─` 作判据** —— 分隔线 `---` 是三个普通连字符（U+002D），不是制表符（U+2500）。
/// 用框线字符判定即可覆盖嵌套在列表 / 引用里的表格（它们的行同样含框线字符）。
const TABLE_FRAME_CHARS: [char; 10] = ['│', '┌', '┬', '┐', '├', '┼', '┤', '└', '┴', '┘'];

/// 该行是否为表格行（含任一框线字符）。
fn is_table_line(line: &Line<'static>) -> bool {
    line.spans
        .iter()
        .any(|s| s.content.chars().any(|c| TABLE_FRAME_CHARS.contains(&c)))
}

/// 渲染一条 assistant markdown。
///
/// `width` 是**表格可用的总列数**（含边框、内边距、外层列表/引用前缀）—— 库只对表格用宽度，
/// 正文 wrap 仍交给消费方的 `Paragraph`（设计 §2.3）。返回 owned 行，便于缓存。
///
/// **表格守卫内置**：渲染完成后，只对**表格行**（含框线字符者）量 [`Line::width`]，
/// **任一行 > `width` 即整条退回 raw**（按 `\n` 切成无样式纯文本，不折行）。正文行超宽**不触发**
/// 回退 —— 库不 wrap 正文（`Options::table_width` 只作用于表格单元格），长于窗宽的段落本就是
/// 超宽行，若按「任一行超宽」判定会把每条稍有长度的回答整条退成纯文本、静默关掉 Markdown 渲染
/// （设计 §2.6 / 不变量 3）。故返回的**已是最终产物**，调用方与缓存无需再判断。
pub(crate) fn render(src: &str, width: usize, styles: AssistantStyles) -> Vec<Line<'static>> {
    #[cfg(test)]
    record_render_call();

    // `table_width` 是 `u16`；`width` 是 `usize`，做饱和转换。
    let table_width = u16::try_from(width).unwrap_or(u16::MAX);
    let text =
        tui_markdown::from_str_with_options(src, &Options::new(styles).table_width(table_width));

    // `Text` / `Line` / `Span` 都没有 `into_owned`；字段是 pub 的，手写 owned 转换。
    // **务必保留 `Line::style` 与 `Line::alignment`** —— 库会写这两者（对齐、行级样式）。
    let lines: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .map(|line| {
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| {
                    // 库对**正文** span 不给前景色（继承终端前景）。主题要能决定正文
                    // 颜色，就必须在这里补上 `fg` 槽位 —— 否则白底终端上正文与主题
                    // `bg` 可能撞成一片。**只补无前景的**：库明确上了色的 span
                    // （标题 / 代码 / 引用 / alert…）保持原样，故语法层的语义不受影响。
                    let mut style = span.style;
                    if style.fg.is_none() {
                        style = style.fg(styles.palette().fg);
                    }
                    Span::styled(span.content.into_owned(), style)
                })
                .collect();
            // `alignment` 字段是 `Option<Alignment>`，而 `Line::alignment()` setter 只收
            // `Alignment`（会丢掉"未设置"态）—— 直接写字段以原样保留。
            let mut owned = Line::from(spans).style(line.style);
            owned.alignment = line.alignment;
            owned
        })
        .collect();

    // 表格窄屏守卫（设计 §2.6）：库**不保证**表格输出行宽 ≤ width —— 当
    // `width < (3N+1) + Σ(每列最宽单字素宽)` 时它不截断、直接超宽。这里量一遍**表格行**的显示
    // 宽度，任一行 > width 即整条退回 raw。守卫**不依赖**该 `(3N+1)+Σ` 公式（它只解释"为何
    // 溢出"；硬编码成阈值常数会随列数 / CJK / 嵌套前缀失真）。**正文行不参与判定**（见函数文档）。
    if lines
        .iter()
        .any(|line| is_table_line(line) && line.width() > width)
    {
        return raw_plain_lines(src);
    }
    lines
}

/// 退化路径：按 `\n` 原样切行得**无样式、不折行**纯文本。
///
/// 不变量 3 只约束**表格行**；正文折行归消费方的 `Paragraph`（`wrap(trim:false)`），
/// 预折行会把单词 / URL 从中间截断，属无谓加码（设计 §2.10）。
///
/// 自洽性：raw 行是**源文本**（ASCII `|` 等），不含框线字符 ⇒ 不会被本模块的表格判定命中。
fn raw_plain_lines(src: &str) -> Vec<Line<'static>> {
    src.split('\n')
        .map(|logical| Line::from(logical.to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------
// 测试打点：只数「解析」实际发生次数（N7 断言 ≤ 11），不依赖任何计时。
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// 本线程内 `md::render` 的实际执行次数。
    static RENDER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_render_call() {
    RENDER_CALLS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
pub(crate) fn reset_render_calls() {
    RENDER_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn render_calls() -> usize {
    RENDER_CALLS.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier, Style};

    use crate::theme::{Palette, ThemeKind};

    /// N3 覆盖用的固定文档（标题 / 粗体 / 斜体 / 行内码 / 链接 / 引用 / 列表 / 围栏码 / 表格）。
    const DOC: &str = "\
# Heading

A **bold** and *italic* and `code` and [link](http://x).

> quote line

- item a
- item b

1. one
2. two

```rust
fn main() {}
```

| H1 | H2 |
| -- | -- |
| a | b |
";

    /// 触发**全部 23 个 `StyleSheet` 方法**的固定文档：front matter / h1–h6 / 标题属性 /
    /// 段落（粗斜删码链接图片 + 内联 HTML）/ 引用 / 无序 + 有序 + task 列表 / 围栏码 /
    /// 行内 + 独立数学 / 脚注引用 + 定义 / 定义列表 / 分隔线 / 表格（表头 + 数据 + 对齐）/
    /// 五种 GFM alert。
    const ALL_ELEMENTS: &str = "\
---
title: Meta
---

# H1
## H2
### H3
#### H4
##### H5
###### H6

# H1meta {#anchor}

para **bold** *em* ~~strike~~ `code` [link](http://x) <br> ![alt](pic.png)

> quote

- un a

1. ord one

- [x] task

```rust
fn main() {}
```

$$x$$

$y$

[^fn]: footnote def text

ref[^fn]

Term
: Definition desc

---

| Left | Center | Right |
| :--- | :----: | ----: |
| a | b | c |

> [!NOTE]
> note body

> [!TIP]
> tip body

> [!IMPORTANT]
> imp body

> [!WARNING]
> warn body

> [!CAUTION]
> cau body
";

    /// 用默认主题渲染（多数用例只需一个基准色板）。
    fn r(src: &str, width: usize) -> Vec<Line<'static>> {
        r_with(src, width, ThemeKind::default())
    }

    fn r_with(src: &str, width: usize, kind: ThemeKind) -> Vec<Line<'static>> {
        render(src, width, AssistantStyles::new(kind.palette()))
    }

    /// 基准色板（默认主题的 10 个槽位）。
    fn base_palette() -> Palette {
        ThemeKind::default().palette()
    }

    /// 样式的前景色。
    fn fg(style: Style) -> Option<Color> {
        style.fg
    }

    /// 断言该样式的**前景恰为该主题的某槽位**。`what` 进失败信息，便于定位到漏覆写的方法。
    fn assert_slot(style: Style, p: &Palette, slot: Color, what: &str) {
        assert_eq!(
            fg(style),
            Some(slot),
            "{what} 的前景应取自主题槽位，实际色板：{p:?}"
        );
    }

    /// 一行里所有 span 内容的拼接（用于内容级断言）。
    fn flat(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// 整个产物的纯文本（行间以 `\n` 连接）。
    fn flat_all(lines: &[Line<'static>]) -> String {
        lines.iter().map(flat).collect::<Vec<_>>().join("\n")
    }

    /// 收集所有产出样式（行级 + span 级）—— 泄漏检测要看两处都算。
    fn all_styles(lines: &[Line<'static>]) -> Vec<ratatui::style::Style> {
        let mut out = Vec::new();
        for l in lines {
            out.push(l.style);
            for s in &l.spans {
                out.push(s.style);
            }
        }
        out
    }

    /// 产出的 (样式, 文本) 对：行级用整行文本，span 级用 span 文本。
    fn styled_texts(lines: &[Line<'static>]) -> Vec<(Style, String)> {
        let mut out = Vec::new();
        for l in lines {
            out.push((l.style, flat(l)));
            for s in &l.spans {
                out.push((s.style, s.content.to_string()));
            }
        }
        out
    }

    fn has_span(lines: &[Line<'static>], content: &str, pred: impl Fn(&Span) -> bool) -> bool {
        lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.as_ref() == content && pred(s))
        })
    }

    #[test]
    fn n3_heading_levels_have_explicit_style_and_no_marker() {
        let lines = r(DOC, 120);
        // h1：accent 前景 + 粗体 + 下划线，**且不带 `#` 标记**（heading_marker 置空）
        let h1 = lines
            .iter()
            .find(|l| flat(l).contains("Heading"))
            .expect("应有一行含 Heading");
        assert_slot(h1.style, &base_palette(), base_palette().accent, "h1");
        assert!(h1.style.add_modifier.contains(Modifier::BOLD));
        assert!(h1.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!flat(h1).contains('#'), "标题标记 `#` 应被去掉");
    }

    #[test]
    fn n3_bold_italic_inline_code_link() {
        let lines = r(DOC, 120);
        assert!(
            has_span(&lines, "bold", |s| s
                .style
                .add_modifier
                .contains(Modifier::BOLD)),
            "粗体应为 BOLD"
        );
        assert!(
            has_span(&lines, "italic", |s| s
                .style
                .add_modifier
                .contains(Modifier::ITALIC)),
            "斜体应为 ITALIC"
        );
        // 行内码：单一代码色（`code` 槽位前景、无底色），与正文区分
        let p = base_palette();
        assert!(
            has_span(&lines, "code", |s| fg(s.style) == Some(p.code)
                && s.style.bg.is_none()),
            "行内码应为 `code` 槽位前景、无底色"
        );
        // 链接：下划线 + `info` 槽位（显式上色，避免被正文 fg 吞掉）
        assert!(
            has_span(&lines, "link", |s| s
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
                && fg(s.style) == Some(p.info)),
            "链接应为下划线且无前景色"
        );
    }

    #[test]
    fn n3_blockquote_list_fenced_code_table() {
        let lines = r(DOC, 120);
        let p = base_palette();
        // 引用：行级 `muted`
        let quote = lines
            .iter()
            .find(|l| flat(l).contains("quote line"))
            .expect("引用行");
        assert_slot(quote.style, &p, p.muted, "blockquote");
        // 有序列表标记 `muted`
        assert!(has_span(&lines, "1. ", |s| fg(s.style) == Some(p.muted)));
        // 无序列表标记（库默认 `- `，字形写死）
        assert!(has_span(&lines, "- ", |_| true));
        // 围栏代码：围栏行被去掉；内容**逐字保留**；行级 + span 级都是同一个代码色
        let code = lines
            .iter()
            .find(|l| flat(l).contains("fn main() {}"))
            .expect("代码行");
        assert_eq!(flat(code), "fn main() {}", "代码样式不得改动字符");
        assert_eq!(code.style.fg, Some(p.code), "围栏代码用行级 `code` 槽位色");
        assert!(code.style.bg.is_none(), "围栏代码不得带底色");
        assert!(!flat_all(&lines).contains("```"), "围栏行应被去掉");
        // 表格头加粗、框线 `muted`
        assert!(has_span(&lines, "H1", |s| s
            .style
            .add_modifier
            .contains(Modifier::BOLD)));
        let joined = flat_all(&lines);
        assert!(
            joined.contains('─') && joined.contains('│'),
            "应画出表格框线"
        );
        assert!(
            lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| s.content.starts_with('┌') && fg(s.style) == Some(p.muted))),
            "框线应取 `muted` 槽位"
        );
    }

    /// 回归：代码（行内 + 围栏块）**不产出任何背景色**。
    ///
    /// `code()` 同时作用于行内码与围栏代码块，而围栏块是整行级样式 —— 底色会让长代码块
    /// 糊成一大片、并给缩进 / 行尾空白染色。故此处按产物扫描：任一行级或 span 级样式
    /// 都不许带 `bg`。
    #[test]
    fn code_never_paints_a_background() {
        let lines = r(
            "前置 `inline` 后置\n\n```rust\nfn main() {}\n    let x = 1;\n```\n",
            120,
        );
        for style in all_styles(&lines) {
            assert_eq!(style.bg, None, "代码不得产出背景色：{style:?}");
        }
        // 非空性：行内与围栏两处代码都确实渲染到了（否则扫描是 vacuous）。
        assert!(has_span(&lines, "inline", |_| true), "行内 code 未渲染");
        assert!(
            lines.iter().any(|l| flat(l) == "fn main() {}"),
            "围栏 code 未渲染"
        );
    }

    /// 代码**不分语言**：rust / python / json 围栏块与行内码共用同一个代码色；
    /// 围栏行不进产物，字符逐字不变。
    #[test]
    fn every_code_block_shares_one_style() {
        let src = "\
```rust
fn main() { let s = \"hi\"; }
```

```python
def f(x):
    return x  # 注释
```

```json
{ \"a\": 1 }
```

正文里的行内码 `inline` 也同色。
";
        let lines = r(src, 120);
        assert!(!flat_all(&lines).contains("```"), "围栏行应被去掉");

        // 三个不同语言的围栏块：样式必须完全一致（`code` 槽位前景、无底色）。
        let want = Style::new().fg(base_palette().code);
        for needle in [
            "fn main() { let s = \"hi\"; }",
            "def f(x):",
            "    return x  # 注释",
            "{ \"a\": 1 }",
        ] {
            let line = lines
                .iter()
                .find(|l| flat(l) == needle)
                .unwrap_or_else(|| panic!("缺少代码行 {needle:?}"));
            assert_eq!(line.style, want, "代码行样式应统一：{needle:?}");
            assert_eq!(flat(line), needle, "代码样式不得改动字符");
        }
        assert!(
            has_span(&lines, "inline", |s| s.style == want),
            "行内码应与围栏块同色"
        );
    }

    /// N3 附加（R3）：**逐元素非空性** —— 用一份**触发全部 23 个 `StyleSheet` 方法**的文档渲染，
    /// 逐条断言每种元素都被本模块的样式表着色（而非 vacuous 地漏过）。
    ///
    /// 判据一律是「等于某主题的某槽位」：漏覆写某个方法 ⇒ 该元素取到库默认色 ⇒ 不等于任何槽位 ⇒ 红。
    /// 全产物范围的「产出色 ∈ 主题调色板」扫描由 [`test_no_default_color_leak_per_theme`] 承担。
    #[test]
    fn n3_every_element_is_colored() {
        let lines = r(ALL_ELEMENTS, 120);
        let p = base_palette();

        // ---- 非空性：逐条确认 23 个样式点确被本输入触发（否则泄漏断言是 vacuous）----
        // 1 heading（各级，行级 `accent`；标题标记已置空 ⇒ 行文本恰为标题文本）
        for h in ["H1", "H2", "H3", "H4", "H5", "H6"] {
            assert!(
                lines
                    .iter()
                    .any(|l| flat(l) == h && fg(l.style) == Some(p.accent)),
                "heading {h} 未触发或未着色"
            );
        }
        // 2 code（行内 + 围栏）
        assert!(
            has_span(&lines, "code", |s| fg(s.style) == Some(p.code)),
            "行内 code 未触发"
        );
        assert!(
            lines
                .iter()
                .any(|l| flat(l) == "fn main() {}" && fg(l.style) == Some(p.code)),
            "围栏 code 未触发"
        );
        // 3 link：下划线 + `info` 槽位（必须显式上色，否则会被正文 fg 吞掉）
        assert!(
            has_span(&lines, "link", |s| s
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
                && fg(s.style) == Some(p.info)),
            "link 未触发"
        );
        // 4 blockquote
        assert!(
            lines
                .iter()
                .any(|l| flat(l).contains("quote") && fg(l.style) == Some(p.muted)),
            "blockquote 未触发"
        );
        // 5 heading_meta
        assert!(
            lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| s.content.contains("{#anchor}") && fg(s.style) == Some(p.muted))),
            "heading_meta 未触发"
        );
        // 6 metadata_block
        assert!(
            lines
                .iter()
                .any(|l| flat(l) == "title: Meta" && fg(l.style) == Some(p.muted)),
            "metadata_block 未触发"
        );
        // 7 heading_marker 置空已由上面「heading 行恰为 H1..H6、无 `#` 前缀」钉住
        // 8 code_block_fence 置空
        assert!(!flat_all(&lines).contains("```"), "code_block_fence 未置空");
        // 9 html
        assert!(
            has_span(&lines, "<br>", |s| fg(s.style) == Some(p.muted)),
            "html 未触发"
        );
        // 10 math_inline
        assert!(
            has_span(&lines, "$y$", |s| fg(s.style) == Some(p.accent)
                && s.style.add_modifier.contains(Modifier::ITALIC)),
            "math_inline 未触发"
        );
        // 11 math_display
        assert!(
            has_span(&lines, "$$x$$", |s| fg(s.style) == Some(p.accent)),
            "math_display 未触发"
        );
        // 12 footnote_ref
        assert!(
            has_span(&lines, "[fn]", |s| fg(s.style) == Some(p.muted)),
            "footnote_ref 未触发"
        );
        // 13 footnote_def
        assert!(
            has_span(&lines, "[fn]: ", |s| fg(s.style) == Some(p.muted)),
            "footnote_def 未触发"
        );
        // 14 definition_term
        assert!(
            has_span(&lines, "Term", |s| s
                .style
                .add_modifier
                .contains(Modifier::BOLD)),
            "definition_term 未触发"
        );
        // 15 definition_description（`: ` 前缀 span 存在）
        assert!(
            has_span(&lines, ": ", |_| true),
            "definition_description 未触发"
        );
        // 16/17/18 alert（五种 kind）+ alert_icon（置空 ⇒ 行文本恰为 `> {label}`）+ alert_label
        for (color, label) in [
            (p.info, "Note"),
            (p.info, "Tip"),
            (p.warn, "Important"),
            (p.warn, "Warning"),
            (p.fail, "Caution"),
        ] {
            assert!(
                lines
                    .iter()
                    .any(|l| flat(l) == format!("> {label}") && fg(l.style) == Some(color)),
                "alert {label} 未触发/未着色，或 alert_icon 未置空"
            );
        }
        // 19 table_header：与单元格同 `fg`，层次只靠 BOLD。
        assert!(
            has_span(&lines, "Left", |s| s
                .style
                .add_modifier
                .contains(Modifier::BOLD)
                && fg(s.style) == Some(p.fg)),
            "table_header 未触发"
        );
        // 20 table_cell：`fg`、无 BOLD。
        assert!(
            has_span(&lines, "a", |s| fg(s.style) == Some(p.fg)
                && !s.style.add_modifier.contains(Modifier::BOLD)),
            "table_cell 未触发"
        );
        // 21 table_border
        assert!(
            lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| s.content.starts_with('┌') && fg(s.style) == Some(p.muted))),
            "table_border 未触发"
        );
        // 22 list_marker
        assert!(
            has_span(&lines, "1. ", |s| fg(s.style) == Some(p.muted)),
            "list_marker 未触发"
        );
        // 23 image_alt
        assert!(
            has_span(&lines, "[img] ", |s| fg(s.style) == Some(p.muted)
                && s.style.add_modifier.contains(Modifier::ITALIC)),
            "image_alt 未触发"
        );

        // 全产物范围的「产出色 ∈ 主题调色板」扫描见 `test_no_default_color_leak_per_theme`。
    }

    /// C5（R3）：**逐主题扫全部产物**，断言没有任何 `Span` / 行携带主题色板之外的颜色。
    ///
    /// 漏覆写砦任一 `StyleSheet` 方法 ⇒ 该元素取到 `DefaultStyleSheet` 的具名色（蓝 / 绿 /
    /// cyan / 白字黑底 / dim…）⇒ 落不进 10 个槽位 ⇒ 红。逐主题跑：写死成默认主题色的
    /// 实现同样会红。
    ///
    /// **前景与背景都查**：只看 `fg` 会漏掉「白字黑底」这类只改背景的默认配色（`code()` 的默认
    /// 形态正是其一）。产出色在 `md.rs` 里只会是 `None` 或色板值（`bg` 恒为 `None`）：
    /// 库给字体形态叠加的除外，那不影响颜色。
    /// 正文（库不给前景的 span）必须取当前主题的 `fg` —— 否则白底终端上正文与主题
    /// `bg` 可能撞成一片，「换主题」对最该换的那部分文本失效。
    ///
    /// 变异：删掉 `render` 里「`style.fg.is_none()` ⇒ 补 `fg`」那段 ⇒ 本用例红。
    #[test]
    fn body_text_takes_fg_slot_per_theme() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            let lines = r_with("plain body text\n", 120, kind);
            let span = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content.contains("plain body text"))
                .expect("应有正文 span");
            assert_eq!(
                fg(span.style),
                Some(p.fg),
                "{}：正文应取 fg 槽位",
                kind.name()
            );
        }
    }

    #[test]
    fn test_no_default_color_leak_per_theme() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            let slots = [
                p.bg, p.fg, p.accent, p.code, p.muted, p.user, p.ok, p.warn, p.fail, p.info,
            ];
            let lines = r_with(ALL_ELEMENTS, 120, kind);

            // 非空性：扫描前先确认这份文档确实产出了内容（否则整个扫描是 vacuous）。
            assert!(
                styled_texts(&lines).len() > 40,
                "{}：样例文档产出过少，泄漏扫描可能空闲",
                kind.name()
            );

            for (style, text) in styled_texts(&lines) {
                if let Some(color) = style.fg {
                    assert!(
                        slots.contains(&color),
                        "{}：前景色 {color:?} 不在该主题色板内（文本 {text:?}）",
                        kind.name()
                    );
                }
                if let Some(color) = style.bg {
                    assert_eq!(
                        color,
                        p.bg,
                        "{}：背景色 {color:?} 应为主题 bg（文本 {text:?}）",
                        kind.name()
                    );
                }
            }
        }
    }

    /// N5：表格窄屏守卫 —— 任意宽度下，凡**含框线字符**的行显示宽度都不得超过 `width`。
    ///
    /// 回退分支的 raw 行是源文本（ASCII `|`），不含框线字符，故该判定对它天然 vacuous
    /// （不需要为 CJK 放宽 `width.max(2)`）。
    #[test]
    fn n5_table_never_exceeds_width() {
        let ascii2 = "| a | b |\n| - | - |\n| c | d |\n";
        let ascii3 = "| one | two | three |\n| - | - | - |\n| 1 | 2 | 3 |\n";
        let cjk = "| 名称 | 值 |\n| -- | -- |\n| 中文 | 数字 |\n";

        for width in 1..=80usize {
            for src in [ascii2, ascii3, cjk] {
                for line in r(src, width) {
                    if is_table_line(&line) {
                        assert!(
                            line.width() <= width,
                            "表格行在 width={width} 出现超宽：{}",
                            flat(&line)
                        );
                    }
                }
            }
        }
    }

    /// N6：非表格 md 渲染后各行**不含 `│`**（表格框线是有意例外）。
    #[test]
    fn n6_non_table_has_no_vertical_border() {
        let src = "# T\n\npara **b**\n\n> q\n\n- a\n\n```\ncode\n```\n";
        for line in r(src, 120) {
            assert!(
                !flat(&line).contains('│'),
                "非表格行不应含 │：{}",
                flat(&line)
            );
        }
    }

    /// 边界：空串 / 半截围栏 / 深嵌套引用 / 超长正文 / 只有表头 / emoji 都不 panic；
    /// 且凡含框线字符的行 ≤ width（正文行不再受宽度约束）。
    #[test]
    fn edge_inputs_do_not_panic_and_table_stays_within_width() {
        let cases = [
            "",
            "```rust\nfn main() {",
            "> > > > > deep quote",
            &"x".repeat(5000),
            "| 只有表头 |\n",
            "\u{1F600}\u{1F600}\u{1F600}",
        ];
        for src in cases {
            let lines = r(src, 40);
            for line in &lines {
                if is_table_line(line) {
                    assert!(
                        line.width() <= 40,
                        "边界输入 {src:?} 的表格行超宽: {}",
                        flat(line)
                    );
                }
            }
        }
    }

    /// 正面用例（替换旧「超长单行退 raw 并折行」）：正文超宽**不触发**回退、也**不预折行** ——
    /// 折行归消费方的 `Paragraph`。故 `"z"×100` 应为**一条**未折行的行，而非 10 条。
    #[test]
    fn long_plain_line_is_not_prewrapped() {
        let lines = r(&"z".repeat(100), 10);
        assert_eq!(lines.len(), 1, "正文超宽不应预折行（由 Paragraph 折行）");
        assert_eq!(flat(&lines[0]), "z".repeat(100), "内容须原样保留");
    }

    /// N5 反面（R13 / 变异 ⑥）：长于 `width` 的正文**必须保住 Markdown 样式**，
    /// 不得因「有行超宽」而整条退回 raw。
    #[test]
    fn narrow_width_long_prose_keeps_markdown_styles() {
        let src = "# 标题\n\n这是一段非常长的正文，明显超过窗口宽度，并且含有**粗体**内容，\
                   用来验证守卫不会把整条回答静默退回纯文本。";
        let lines = r(src, 8);
        // 标题行：行级样式（`accent` + 粗体 + 下划线），且不带 `#` 标记
        let h = lines
            .iter()
            .find(|l| flat(l).contains("标题"))
            .expect("应有标题行");
        assert!(
            h.style.add_modifier.contains(Modifier::BOLD),
            "窄窗下标题行应保留行级粗体"
        );
        assert!(!flat(h).contains('#'), "标题标记应被去掉（说明未退 raw）");
        // 粗体 span 仍在（raw 回退会把它抹成字面 `**粗体**`）
        assert!(
            has_span(&lines, "粗体", |s| s
                .style
                .add_modifier
                .contains(Modifier::BOLD)),
            "长正文中的粗体应保留 BOLD（不得退成 raw）"
        );
        assert!(
            !flat_all(&lines).contains("**"),
            "不应残留字面 `**`（说明未退 raw）"
        );
    }

    /// 窄表退 raw 的正面用例（变异 ① 的对应）：极小宽度下表格必然溢出 ⇒ 该条退回无样式 raw。
    #[test]
    fn narrow_table_falls_back_to_raw() {
        let src = "| one | two | three |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n";
        let lines = r(src, 8);
        let joined = flat_all(&lines);
        assert!(!joined.contains('│'), "退 raw 后不应含框线字符：{joined}");
        assert_eq!(
            lines.len(),
            src.split('\n').count(),
            "raw 应严格按 `\\n` 原样切行（含结尾空行）"
        );
        assert!(
            lines
                .iter()
                .all(|l| l.style == ratatui::style::Style::default()),
            "raw 不应带行级样式"
        );
        assert!(
            lines.iter().all(|l| l
                .spans
                .iter()
                .all(|s| s.style == ratatui::style::Style::default())),
            "raw 不应带 span 样式"
        );
    }

    /// 空串输入不 panic。
    #[test]
    fn empty_input_yields_no_overwide_lines() {
        let lines = r("", 20);
        assert!(lines.iter().all(|l| l.width() <= 20));
    }
}
