//! assistant markdown 的语义元素 → `Style` 映射（设计 §2.7）。
//!
//! 全仓**唯一**给 md 元素定色的地方：渲染出的每个 `Span` 样式都源于此，
//! `transcript.rs` 保持无样式（不变量 1）。
//!
//! **颜色唯一来源是 [`ThemeKind::palette`] 的 10 个槽位**：本结构持有一份 `Palette`，
//! 每个方法只从它取色，不出现任何具名色。槽位语义（哪个元素落哪个槽位）见 `theme.rs`
//! 模块头的表 —— 那张表是「语义 → 颜色」的唯一映射来源，改动配色只改色板，不改本文件。
//!
//! **必须逐项覆写全部方法**：`tui_markdown::StyleSheet` 的默认实现由 `DefaultStyleSheet`
//! 给出，自带一套主观配色（链接蓝、引用绿、表格头 cyan、`code()` 白字黑底…），漏写一项即
//! 会把默认色漏进渲染（风险 R3）。所有产出色都必须落在槽位集合内，由单测 C4 / C5 守卫。
//!
//! `tui_markdown::` 路径只准出现在 `md.rs` / `styles.rs`（R6 缓解）。

use ratatui::style::{Modifier, Style};
use tui_markdown::{AlertKind, StyleSheet};

use crate::theme::Palette;

/// assistant markdown 的样式表，携带一份主题色板。
///
/// `Clone + Copy`：库的 trait 要求 `Clone + Send + Sync + 'static`，本仓 `md::render` /
/// `cache::RenderCache` 又按值传递它；`Palette` 是 `Copy`，故这组 bound 自动满足。
///
/// **不实现 `Default`**：无主题的色板没有合理默认值，构造必须显式给出 [`ThemeKind`]，
/// 否则「漏传主题」会被一个静默的默认色板掩盖。
#[derive(Clone, Copy)]
pub(crate) struct AssistantStyles {
    palette: Palette,
}

impl AssistantStyles {
    /// 收**色板值**而不是主题枚举：渲染层只需要 10 个颜色槽位，不需要知道它来自哪套
    /// 内置主题。这样布局层直接按 `Palette` 比较失效、渲染层直接吃同一个值，中间不出现
    /// 「色板 → 主题 → 色板」的反查。
    pub(crate) fn new(palette: Palette) -> Self {
        Self { palette }
    }

    /// 本样式表使用的色板。`md::render` 用它给**库未上色**的正文 span 补前景。
    pub(crate) fn palette(&self) -> Palette {
        self.palette
    }
}

impl StyleSheet for AssistantStyles {
    /// 标题：`accent` 前景 + `BOLD`；h1 另加 `UNDERLINED` 以示最高层级。
    fn heading(&self, level: u8) -> Style {
        let base = Style::default()
            .fg(self.palette.accent)
            .add_modifier(Modifier::BOLD);
        if level == 1 {
            base.add_modifier(Modifier::UNDERLINED)
        } else {
            base
        }
    }

    /// 行内码与围栏代码块：**只有前景 `code`，无底色**。
    ///
    /// `code()` 同时作用于行内码与围栏代码块，**不分语言** ——
    /// 目标是「与正文区分开」，不是编辑器级语法高亮。
    /// 不设底色：围栏块是整行级样式，底色会给长代码块糊一大片，代码的缩进 / 行尾空白
    /// 也会成片显色。
    fn code(&self) -> Style {
        Style::new().fg(self.palette.code)
    }

    /// 链接：下划线 + `info` 槽位。
    ///
    /// **必须显式给前景色**：`md::render` 会给「库未上色」的 span 补 `fg` 槽位（正文
    /// 跟随主题），链接若留空就会与正文同色、只剩一条下划线可辨。用 `info` 而不另设
    /// 槽位 —— 它与 alert `Note` / `Tip` 同属「提示性、非错误」的语义族。
    fn link(&self) -> Style {
        Style::default()
            .fg(self.palette.info)
            .add_modifier(Modifier::UNDERLINED)
    }

    /// 引用块（`>` 前缀与正文）：`muted`。
    fn blockquote(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 标题属性元数据（如 `{#id}`）：`muted`。
    fn heading_meta(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 前沿元数据块（YAML front matter）：`muted`。
    fn metadata_block(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 标题前缀标记：置空 ⇒ 去掉 `#` 及其后的分隔空格（设计 §2.7）。
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    /// 代码块上下围栏行：置空 ⇒ 去掉两行围栏（拷贝得到纯代码，设计 §2.8）。
    fn code_block_fence(&self) -> &str {
        ""
    }

    /// 原始 HTML / 内联标签：`muted`。
    fn html(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 行内数学：`accent` + 斜体。
    fn math_inline(&self) -> Style {
        Style::default()
            .fg(self.palette.accent)
            .add_modifier(Modifier::ITALIC)
    }

    /// 独立数学块：`accent`。
    fn math_display(&self) -> Style {
        Style::default().fg(self.palette.accent)
    }

    /// 脚注引用（`[label]`）：`muted`。
    fn footnote_ref(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 脚注定义（含 `[label]: ` 前缀）：`muted`。
    fn footnote_def(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 定义列表的术语：只加粗，不额外着色。
    fn definition_term(&self) -> Style {
        Style::default()
            .fg(self.palette.fg)
            .add_modifier(Modifier::BOLD)
    }

    /// 定义列表的描述（含 `: ` 前缀）：不额外着色。
    fn definition_description(&self) -> Style {
        Style::default().fg(self.palette.fg)
    }

    /// GFM alert 的标题与正文基色：Note / Tip → `info`，Important / Warning → `warn`，
    /// Caution → `fail`（icon / label 另加粗）。
    fn alert(&self, kind: AlertKind) -> Style {
        let fg = match kind {
            AlertKind::Note | AlertKind::Tip => self.palette.info,
            AlertKind::Important | AlertKind::Warning => self.palette.warn,
            AlertKind::Caution => self.palette.fail,
        };
        Style::default().fg(fg)
    }

    /// GFM alert 图标：置空 —— 默认是 emoji（多列宽字形），会破坏「块级装饰宽度 = 1」的不变量 2。
    fn alert_icon(&self, _kind: AlertKind) -> &str {
        ""
    }

    /// GFM alert 标签：沿用库的规范英文标签。
    fn alert_label(&self, kind: AlertKind) -> &str {
        kind.label()
    }

    /// 表格表头单元格：只加粗、不上色。
    fn table_header(&self) -> Style {
        Style::default()
            .fg(self.palette.fg)
            .add_modifier(Modifier::BOLD)
    }

    /// 普通表格单元格：`fg`（与正文同色）。
    ///
    /// 与表头**同色**是有意的：层次靠 `BOLD` 表达，不靠第二套颜色 —— 否则一张表里
    /// 出现两种前景，读起来比「表头略重」更乱。
    fn table_cell(&self) -> Style {
        Style::default().fg(self.palette.fg)
    }

    /// 表格框线：`muted`（表格保留方框线是有意接受的拷贝例外，设计 §2.8）。
    fn table_border(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 有序列表标记（`1.` `2.` …）：`muted`。
    fn list_marker(&self) -> Style {
        Style::default().fg(self.palette.muted)
    }

    /// 图片占位文本（`[img] …`）：`muted` + 斜体。
    fn image_alt(&self) -> Style {
        Style::default()
            .fg(self.palette.muted)
            .add_modifier(Modifier::ITALIC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeKind;
    use ratatui::style::Color;

    /// 若 `style` 带了前景色，它必须恰为 `slot`。
    fn assert_fg(style: Style, slot: Color, what: &str) {
        assert_eq!(style.fg, Some(slot), "{what} 的前景应取自槽位");
    }

    /// C4：5 个主题 × 关键方法的「槽位 → 颜色」映射。
    ///
    /// 逐主题跑：同一方法在不同主题下必须取到**各自色板**的值，否则「主题没接上」会被
    /// 单一主题的断言掩盖（例如把槽位写死成默认主题的色）。
    #[test]
    fn test_each_theme_maps_key_methods_to_slots() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            let s = AssistantStyles::new(p);
            let ctx = kind.name();

            // heading：accent + BOLD；h1 另加 UNDERLINED，h2 不加。
            assert_fg(s.heading(1), p.accent, ctx);
            assert!(s.heading(1).add_modifier.contains(Modifier::BOLD));
            assert!(s.heading(1).add_modifier.contains(Modifier::UNDERLINED));
            assert_fg(s.heading(2), p.accent, ctx);
            assert!(s.heading(2).add_modifier.contains(Modifier::BOLD));
            assert!(!s.heading(2).add_modifier.contains(Modifier::UNDERLINED));

            // code：只前景 code，无底色（底色会给整行代码糊一大片）。
            assert_fg(s.code(), p.code, ctx);
            assert_eq!(s.code().bg, None, "code 不得带底色");

            // 数学：accent；行内另加 ITALIC。
            assert_fg(s.math_inline(), p.accent, ctx);
            assert!(s.math_inline().add_modifier.contains(Modifier::ITALIC));
            assert_fg(s.math_display(), p.accent, ctx);

            // 数学块不带 ITALIC（行内数学才带）。
            assert!(!s.math_display().add_modifier.contains(Modifier::ITALIC));

            // muted 一族：引用 / 标题属性 / 前沿元数据 / HTML / 脚注 / 列表标记 / 表格框线。
            for (style, what) in [
                (s.blockquote(), "blockquote"),
                (s.heading_meta(), "heading_meta"),
                (s.metadata_block(), "metadata_block"),
                (s.html(), "html"),
                (s.footnote_ref(), "footnote_ref"),
                (s.footnote_def(), "footnote_def"),
                (s.list_marker(), "list_marker"),
                (s.table_border(), "table_border"),
            ] {
                assert_fg(style, p.muted, &format!("{ctx}/{what}"));
                assert_eq!(style.bg, None, "{ctx}/{what} 不得带底色");
            }

            // 图片占位：muted + ITALIC。
            assert_fg(s.image_alt(), p.muted, ctx);
            assert!(s.image_alt().add_modifier.contains(Modifier::ITALIC));

            // alert 四值 → warn / fail / info。
            assert_fg(s.alert(AlertKind::Note), p.info, ctx);
            assert_fg(s.alert(AlertKind::Tip), p.info, ctx);
            assert_fg(s.alert(AlertKind::Important), p.warn, ctx);
            assert_fg(s.alert(AlertKind::Warning), p.warn, ctx);
            assert_fg(s.alert(AlertKind::Caution), p.fail, ctx);

            // 链接：下划线 + `info`（必须显式上色，否则被正文 fg 吞掉）。
            assert_fg(s.link(), p.info, ctx);
            assert!(s.link().add_modifier.contains(Modifier::UNDERLINED));

            // 表格：表头与单元格同 `fg`，层次只靠 BOLD（不引第二套颜色）。
            assert_fg(s.table_header(), p.fg, ctx);
            assert!(s.table_header().add_modifier.contains(Modifier::BOLD));
            assert_fg(s.table_cell(), p.fg, ctx);
            assert!(!s.table_cell().add_modifier.contains(Modifier::BOLD));

            // 定义列表：术语 BOLD、描述与正文同色。
            assert_fg(s.definition_term(), p.fg, ctx);
            assert!(s.definition_term().add_modifier.contains(Modifier::BOLD));
            assert_fg(s.definition_description(), p.fg, ctx);

            // 形态类：三个装饰串置空（`#` / ``` 围栏 / emoji 图标）。
            assert_eq!(s.heading_marker(1), "");
            assert_eq!(s.heading_marker(3), "");
            assert_eq!(s.code_block_fence(), "");
            for alert_kind in [
                AlertKind::Note,
                AlertKind::Tip,
                AlertKind::Important,
                AlertKind::Warning,
                AlertKind::Caution,
            ] {
                assert_eq!(s.alert_icon(alert_kind), "", "alert 图标应置空（多列宽）");
                assert_eq!(
                    s.alert_label(alert_kind),
                    alert_kind.label(),
                    "alert 标签沿库默认"
                );
            }
        }
    }

    /// 逐项覆写的基本保证：关键方法**不得**等于 `DefaultStyleSheet` 的主观配色。
    #[test]
    fn overrides_do_not_match_default_palette() {
        let s = AssistantStyles::new(ThemeKind::default().palette());
        assert_ne!(
            s.link(),
            Style::default().blue().underlined(),
            "链接不得回落默认蓝"
        );
        assert_ne!(
            s.blockquote(),
            Style::default().green(),
            "引用不得回落默认绿"
        );
        assert_ne!(
            s.table_header(),
            Style::default().bold().cyan(),
            "表头不得回落默认 cyan"
        );
        assert_ne!(
            s.code(),
            Style::default().white().on_black(),
            "code 不得回落白字黑底"
        );
    }
}
