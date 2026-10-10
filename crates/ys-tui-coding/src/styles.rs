//! assistant markdown 的语义元素 → `Style` 映射（设计 §2.7）。
//!
//! 全仓**唯一**给 md 元素定色的地方：渲染出的每个 `Span` 样式都源于此，
//! `transcript.rs` 保持无样式（不变量 1）。
//!
//! `tui_markdown::StyleSheet` 是**编译期 trait、不是主题系统** —— 无配置、无持久化、
//! 无 `Palette` 结构，与「颜色只在渲染层决定」「不引 Theme」两条约束相容。
//!
//! **必须逐项覆写全部方法**：`DefaultStyleSheet` 自带一套主观配色（链接蓝、引用绿、
//! 表格头 cyan、`code()` 白字黑底…），漏写一项即会把默认色漏进渲染（风险 R3）。
//! 本实现的配色**刻意不使用蓝 / 绿 / cyan，也不使用白字黑底**，使「默认色泄漏」可被单测
//! 以「产出样式不含这些颜色」直接钉住。
//!
//! `tui_markdown::` 路径只准出现在 `md.rs` / `styles.rs`（R6 缓解）。

use ratatui::style::{Color, Modifier, Style};
use tui_markdown::{AlertKind, StyleSheet};

/// assistant markdown 的配色。
///
/// `Clone + Copy + Default`：库的 trait 要求 `Clone + Send + Sync + 'static`，本仓
/// `md::render` / `cache::RenderCache` 又按值传递它。它是零大小类型。
#[derive(Clone, Copy, Default)]
pub(crate) struct AssistantStyles;

impl StyleSheet for AssistantStyles {
    /// 标题分级：h1 加下划线，各级均加粗并用品红前景（有意避开默认的 cyan 系列）。
    fn heading(&self, level: u8) -> Style {
        let base = Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD);
        if level == 1 {
            base.add_modifier(Modifier::UNDERLINED)
        } else {
            base
        }
    }

    /// 行内码与围栏代码块：**只加背景**（默认是 `white on black`，会把整块代码变白字黑底）。
    fn code(&self) -> Style {
        Style::default().bg(Color::DarkGray)
    }

    /// 链接：**只加下划线、不上色**（默认是蓝色下划线；URL 尾注由库附在公司默认形态里）。
    fn link(&self) -> Style {
        Style::default().add_modifier(Modifier::UNDERLINED)
    }

    /// 引用块（`>` 前缀与正文）：暗灰（有意避开默认的绿色）。
    fn blockquote(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 标题属性元数据（如 `{#id}`）：暗灰。
    fn heading_meta(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 前沿元数据块（YAML front matter）：暗灰。
    fn metadata_block(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 标题前缀标记：置空 ⇒ 去掉 `#` 及其后的分隔空格（设计 §2.7）。
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    /// 代码块上下围栏行：置空 ⇒ 去掉两行围栏（拷贝得到纯代码，设计 §2.8）。
    fn code_block_fence(&self) -> &str {
        ""
    }

    /// 原始 HTML / 内联标签：暗灰。
    fn html(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 行内数学：品红 + 斜体。
    fn math_inline(&self) -> Style {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::ITALIC)
    }

    /// 独立数学块：品红。
    fn math_display(&self) -> Style {
        Style::default().fg(Color::Magenta)
    }

    /// 脚注引用（`[label]`）：暗灰。
    fn footnote_ref(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 脚注定义（含 `[label]: ` 前缀）：暗灰。
    fn footnote_def(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 定义列表的术语：加粗。
    fn definition_term(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// 定义列表的描述（含 `: ` 前缀）：不额外着色。
    fn definition_description(&self) -> Style {
        Style::default()
    }

    /// GFM alert 的标题与正文基色（icon / label 另加粗）。
    fn alert(&self, kind: AlertKind) -> Style {
        let fg = match kind {
            AlertKind::Note | AlertKind::Tip => Color::Magenta,
            AlertKind::Important | AlertKind::Warning => Color::Yellow,
            AlertKind::Caution => Color::Red,
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

    /// 表格表头单元格：只加粗、不上色（有意避开默认的 cyan）。
    fn table_header(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// 普通表格单元格：不额外着色。
    fn table_cell(&self) -> Style {
        Style::default()
    }

    /// 表格框线：暗灰（表格保留方框线是有意接受的拷贝例外，设计 §2.8）。
    fn table_border(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 有序列表标记（`1.` `2.` …）：暗灰。
    fn list_marker(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    /// 图片占位文本（`[img] …`）：暗灰 + 斜体。
    fn image_alt(&self) -> Style {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐项覆写的基本保证：关键方法**不得**等于 `DefaultStyleSheet` 的主观配色。
    #[test]
    fn overrides_do_not_match_default_palette() {
        let s = AssistantStyles;
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

    /// 有意选择的形态：标题用品红、无 `#` 标记、围栏置空、code 只加背景。
    #[test]
    fn chosen_shapes_are_as_designed() {
        let s = AssistantStyles;
        assert_eq!(s.heading_marker(1), "");
        assert_eq!(s.heading_marker(3), "");
        assert_eq!(s.code_block_fence(), "");
        assert_eq!(s.code().fg, None, "code 只加背景，不设前景");
        assert_eq!(s.code().bg, Some(Color::DarkGray));
        assert_eq!(s.heading(1).fg, Some(Color::Magenta));
        assert!(s.heading(1).add_modifier.contains(Modifier::UNDERLINED));
        assert!(s.heading(2).add_modifier.contains(Modifier::BOLD));
        assert!(!s.heading(2).add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(
            s.alert_icon(AlertKind::Warning),
            "",
            "emoji 图标应去掉（多列宽）"
        );
    }
}
