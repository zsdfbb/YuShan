//! 三 pane 渲染（设计 §1 / §2）。
//!
//! ```text
//! ┌ Chat ──────────────────────────┐   Constraint::Min(3)      ← 唯一可滚动
//! ├────────────────────────────────┤
//! │ Input（多行，高自适应）        │   Constraint::Length(1..=N)
//! ├────────────────────────────────┤
//! │ model · ↑1.2k ↓345 · 2 轮 · 1m │   Constraint::Length(1)   ← 最底常驻
//! └────────────────────────────────┘
//! ```
//!
//! # 滚动模型（修正旧实现最大的 bug）
//!
//! 旧 `ui/draw.rs:176-179` 对 assistant **不做 wrap**，每 `TranscriptLine` 算
//! **1 行** scroll 单位；而外层 `Paragraph` 又会真 wrap —— 二者不一致，滚动位置
//! 与实际看到的完全错位。
//!
//! 本实现：**先建完整 `Text`，再用 `Paragraph::line_count(width)` 数 wrap 后的
//! 视觉行数**（ratatui 内部 `WordWrapper` 的同一实现，故与渲染严格一致），
//! 滚动以**视觉行**为单位，用 `Paragraph::scroll` 落到画面上。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use ys_core::StopReason;

use crate::app::App;
use crate::completion::CompletionState;
use crate::input::InputBuffer;
use crate::transcript::TranscriptLine;

/// 输入区最多显示的内容行数（不含边框）。
const INPUT_MAX_LINES: usize = 5;
/// 窗口小于此尺寸直接不渲染（留给「终端太小」）。
const MIN_WIDTH: u16 = 10;
const MIN_HEIGHT: u16 = 5;
/// 补全浮层的最大宽度（再宽也只是空荡荡的一行）。
const COMPLETION_MAX_WIDTH: u16 = 64;
/// 模态浮层的宽度夹取区间。
const MODAL_MIN_WIDTH: u16 = 24;
const MODAL_MAX_WIDTH: u16 = 72;

/// 渲染整屏。
pub fn ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Min(3),                    // Chat
        Constraint::Length(input_height(app)), // Input（1..=N 自适应）
        Constraint::Length(1),                 // Status（最底，常驻）
    ])
    .split(area);

    draw_transcript(frame, chunks[0], app);
    draw_input(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);

    // 补全浮层**最后画**：Clear 后盖在 Chat 区域上（设计 §1「独立于三个 pane」）。
    // 它不参与上面的布局计算 —— 开合时三个 pane 的位置一个像素都不动。
    if let Some(state) = &app.completion {
        draw_completion(frame, chunks[0], state);
    }
}

/// 输入区 pane 高度（**含**上下两条边框）。
pub fn input_height(app: &App) -> u16 {
    (app.input.lines().clamp(1, INPUT_MAX_LINES) + 2) as u16
}

/// Chat 区（含边框）—— 与 [`ui`] 的布局同源，供滚动算术复用。
fn chat_area(app: &App, area: Rect) -> Rect {
    Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_height(app)),
        Constraint::Length(1),
    ])
    .split(area)[0]
}

/// 允许的最大 `scroll_offset`（**视觉行**，0 = 贴底）。
///
/// 需要终端尺寸 —— 因为 wrap 行数依赖宽度、可见行数依赖高度。
/// 尺寸太小（[`ui`] 不渲染）时不滚动。
pub fn max_scroll_offset(app: &App, width: u16, height: u16) -> usize {
    let area = Rect::new(0, 0, width, height);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return 0;
    }
    let chat = chat_area(app, area);
    let inner_w = chat.width.saturating_sub(2).max(1);
    let inner_h = chat.height.saturating_sub(2) as usize;
    total_rows(app, inner_w).saturating_sub(inner_h)
}

/// transcript 在 `width` 下 wrap 后的总视觉行数。
fn total_rows(app: &App, width: u16) -> usize {
    Paragraph::new(build_text(app, width as usize))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &App) {
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2) as usize;
    let text = build_text(app, inner_w as usize);

    let total = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner_w);
    let bottom = total.saturating_sub(inner_h);
    // follow：贴底；否则从底部向上数 scroll_offset 行
    let offset = if app.follow {
        bottom
    } else {
        bottom.saturating_sub(app.scroll_offset)
    };

    let para = Paragraph::new(text)
        .block(Block::bordered().title(" Chat "))
        .wrap(Wrap { trim: false })
        .scroll((offset.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(para, area);
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Input ");
    let inner = block.inner(area);

    let (text, style) = if app.is_turning {
        (app.working_text(), Style::default().fg(Color::Yellow))
    } else {
        (app.input.text.clone(), Style::default())
    };
    frame.render_widget(Paragraph::new(text).block(block).style(style), area);

    if !app.is_turning {
        // 光标：**行号**取 char 语义的 `cursor_row_col`（`\n` 计数），
        // **列**按**显示宽度**算（不能按字节、也不能按 char 数 —— 中文占 2 列）。
        let (logical_row, _col_chars) = app.input.cursor_row_col();
        let last_row = app
            .input
            .text_before_cursor()
            .rsplit('\n')
            .next()
            .unwrap_or("");
        let inner_w = inner.width.max(1) as usize;
        let disp_col = display_width(last_row);
        // 软换行：本行显示宽度超过 inner_w 时，光标落到被折出的下一视觉行
        let row = logical_row + disp_col / inner_w;
        let col = disp_col % inner_w;
        if row < inner.height as usize && col < inner_w {
            frame.set_cursor_position(ratatui::layout::Position::new(
                inner.x + col as u16,
                inner.y + row as u16,
            ));
        }
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let text = status_text(app, area.width as usize);
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

// ---------------------------------------------------------------------------
// 浮层（补全 / 模态选择器）—— 都是 `Clear` 后盖上去，**不参与布局高度**
// ---------------------------------------------------------------------------

/// 补全浮层：贴 Chat 区底部、盖在对话内容上（设计 §1 mockup）。
///
/// 只画 `visible` 条（超出的一半被窗口裁掉），保证 `selected` 一定可见。
fn draw_completion(frame: &mut Frame, chat: Rect, state: &CompletionState) {
    if state.items.is_empty() || chat.height < 3 || chat.width < 8 {
        return;
    }
    let visible = state.items.len().min((chat.height - 2) as usize);
    let start = window_start(state.selected, state.items.len(), visible);
    let w = chat.width.min(COMPLETION_MAX_WIDTH);
    let h = visible as u16 + 2;
    let rect = Rect::new(chat.x, chat.bottom() - h, w, h);

    frame.render_widget(Clear, rect);
    let inner_w = (w - 2) as usize;
    let lines: Vec<Line<'static>> = state.items[start..start + visible]
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if start + i == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                truncate_to_width(&item.display, inner_w),
                style,
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(" Commands ")),
        rect,
    );
}

/// 一帧里要额外盖上去的模态浮层。
pub(crate) enum Modal<'a> {
    /// 选项选择器（`/login` 选 provider、`/model` 无参选模型）。
    Select {
        prompt: &'a str,
        options: &'a [String],
        selected: usize,
        page_size: usize,
    },
    /// 单行文本输入（`/login` 问 api_key）。
    Text {
        prompt: &'a str,
        help: Option<&'a str>,
        input: &'a InputBuffer,
    },
}

/// 画「**完整界面 + 盖在上面的模态浮层**」。
///
/// # 为什么浮层必须和 [`ui`] 画在同一帧
///
/// ratatui 的 `Terminal::draw` 每次从**空缓冲**起画（`swap_buffers` 会
/// `reset` 回来的那块），谁没画谁就是空白。所以「只画浮层」得到的是一屏
/// 空白中间一个小方框 —— 那不是「盖在 Chat 上」，是把界面擦掉了。
/// 浮层自己的 `Clear` 负责把**它占的那块**擦干净，其余交给 [`ui`]。
pub(crate) fn ui_with_modal(frame: &mut Frame, app: &mut App, modal: Modal<'_>) {
    ui(frame, app);
    match modal {
        Modal::Select {
            prompt,
            options,
            selected,
            page_size,
        } => prompt_select(frame, prompt, options, selected, page_size),
        Modal::Text {
            prompt,
            help,
            input,
        } => prompt_text(frame, prompt, help, input),
    }
}

/// 模态选择器：`Clear` 后居中盖在整屏上 —— **同一套 alt-screen，不切屏、不让出终端**；
/// 外部交互库与「切屏/恢复终端」那套机制按设计 §3 已作废。
///
/// 只画浮层本身；底下的界面由 [`ui_with_modal`] 先画好。
fn prompt_select(
    frame: &mut Frame,
    prompt: &str,
    options: &[String],
    selected: usize,
    page_size: usize,
) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT || options.is_empty() {
        return;
    }
    let rows = (area.height - 2) as usize;
    let visible = options.len().min(page_size.max(1)).min(rows).max(1);
    let start = window_start(selected, options.len(), visible);

    let hint = "↑↓ 选择 · Enter 确认 · Esc 取消";
    let title = format!(" {prompt} ({hint}) ");
    // 宽度按「标题 / 最长选项」里更宽的那个算 —— 否则标题会被边框裁掉，
    // 用户就看不到「Esc 取消」这类唯一能学会操作的提示。
    let content = options
        .iter()
        .map(|o| display_width(o))
        .max()
        .unwrap_or(0)
        .max(display_width(&title));
    let w = modal_width(area, content);
    let h = (visible + 2) as u16;
    let rect = centered_rect(area, w, h);

    frame.render_widget(Clear, rect);
    let inner_w = (w.saturating_sub(2)) as usize;
    let mut lines: Vec<Line<'static>> = options[start..start + visible]
        .iter()
        .enumerate()
        .map(|(i, option)| {
            let style = if start + i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(truncate_to_width(option, inner_w), style))
        })
        .collect();
    if options.len() > visible {
        lines.push(Line::from(Span::styled(
            format!("… {}/{}", selected + 1, options.len()),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
        rect,
    );
}

/// 模态单行输入（`/login` 问 api_key）。
///
/// 光标**由本函数直接落好**（按显示宽度，CJK 占 2 列），调用方不必再管。
/// 只画浮层本身；底下的界面由 [`ui_with_modal`] 先画好。
fn prompt_text(frame: &mut Frame, prompt: &str, help: Option<&str>, input: &InputBuffer) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return;
    }
    let title = match help {
        Some(help) => format!(" {prompt} — {help} "),
        None => format!(" {prompt} "),
    };
    let w = modal_width(area, display_width(&title).max(24));
    let rect = centered_rect(area, w, 3);

    frame.render_widget(Clear, rect);
    let block = Block::bordered().title(title);
    let inner = block.inner(rect);
    frame.render_widget(Paragraph::new(input.text.clone()).block(block), rect);

    // 光标按**显示宽度**定位（CJK 占 2 列），且取的是当前行的前缀
    let row = input.text_before_cursor().rsplit('\n').next().unwrap_or("");
    let col = display_width(row);
    if col < inner.width as usize {
        frame.set_cursor_position(ratatui::layout::Position::new(
            inner.x + col as u16,
            inner.y,
        ));
    }
}

/// 滚动窗口起点：让 `selected` 一定落在 `[start, start + visible)` 内。
///
/// 优先把选中项摆在**窗口底部**（视线自然落在刚移动过去的那一项上）。
fn window_start(selected: usize, total: usize, visible: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    selected
        .saturating_sub(visible - 1)
        .min(total.saturating_sub(visible))
}

/// 按内容宽度求模态框宽度（夹取区间 + 不超屏）。
fn modal_width(area: Rect, content_width: usize) -> u16 {
    (content_width as u16 + 4)
        .clamp(MODAL_MIN_WIDTH, MODAL_MAX_WIDTH)
        .min(area.width)
}

/// 在 `area` 内居中的矩形。
fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// 状态行文案：**从尾部剥**（时长 → 轮数 → tokens → model）。
///
/// `model` 最不可剥，故任何宽度下都至少显示它。
fn status_text(app: &App, width: usize) -> String {
    let model = if app.is_turning {
        format!("⏳ {}", app.working_text())
    } else {
        app.view.model_label()
    };

    let mut segs = vec![model];
    if app.view.total_input_tokens > 0 || app.view.total_output_tokens > 0 {
        segs.push(format!(
            "↑{} ↓{}",
            crate::format::format_tokens(app.view.total_input_tokens),
            crate::format::format_tokens(app.view.total_output_tokens),
        ));
    }
    if app.view.turn_count > 0 {
        segs.push(format!("{} 轮", app.view.turn_count));
    }
    segs.push(app.view.session_duration_str());

    // 取「能放下的最长前缀」= 逐段从尾部剥
    for k in (1..=segs.len()).rev() {
        let candidate = segs[..k].join(" · ");
        if display_width(&candidate) <= width {
            return candidate;
        }
    }
    segs[0].clone() // 连 model 都放不下：原样交给终端裁
}

/// transcript → 完整的、带样式的文本（**不做 wrap** —— 交给 `Paragraph`）。
///
/// `width` 只用于需要「截断到宽度」的地方（工具失败结果首行）。
fn build_text(app: &App, width: usize) -> Text<'static> {
    build_text_with(app, width, app.thinking_visible)
}

/// `build_text` 的完整形态：显式传入 thinking 可见性
/// （退出打印时**总是**打印思考内容）。
fn build_text_with(app: &App, width: usize, thinking_visible: bool) -> Text<'static> {
    if app.transcript.is_empty() {
        return Text::from(vec![Line::from(Span::styled(
            "(no messages yet)",
            Style::default().fg(Color::DarkGray),
        ))]);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for item in &app.transcript {
        push_transcript_line(&mut lines, item, width, thinking_visible);
    }
    Text::from(lines)
}

fn push_transcript_line(
    out: &mut Vec<Line<'static>>,
    item: &TranscriptLine,
    width: usize,
    thinking_visible: bool,
) {
    match item {
        TranscriptLine::User(s) => {
            for (i, part) in s.split('\n').enumerate() {
                let prefix = if i == 0 { "> " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green)),
                    Span::raw(part.to_string()),
                ]));
            }
        }
        TranscriptLine::Assistant(s) => {
            // 真 wrap 交给 Paragraph；这里只把显式换行拆成独立的 Line
            for part in s.split('\n') {
                out.push(Line::from(part.to_string()));
            }
        }
        TranscriptLine::Thinking(s) => {
            if !thinking_visible {
                return;
            }
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            for (i, part) in s.split('\n').enumerate() {
                let prefix = if i == 0 { "⌁ " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(part.to_string(), style),
                ]));
            }
        }
        TranscriptLine::Tool {
            name,
            summary,
            result,
            success,
            ..
        } => {
            let (mark, mark_style) = if *success {
                ("✓", Style::default().fg(Color::Green))
            } else {
                ("✗", Style::default().fg(Color::Red))
            };
            out.push(Line::from(vec![
                Span::raw(format!("  ⏺ {name} ")),
                Span::raw(summary.clone()),
                Span::raw("   "),
                Span::styled(mark, mark_style),
            ]));
            // 成功只给 ✓（结果默认不展开）；**失败**才显示结果首行 —— 「看不到任何结果」是真损失
            if !*success && let Some(body) = result {
                let first = body.lines().next().unwrap_or("");
                let budget = width.saturating_sub(6).max(1);
                out.push(Line::from(Span::styled(
                    format!("    └ {}", truncate_to_width(first, budget)),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        TranscriptLine::Summary {
            rounds,
            stop,
            elapsed_secs,
        } => {
            out.push(Line::from(format!(
                "{} {rounds} rounds · {elapsed_secs:.1}s",
                stop_symbol(stop)
            )));
        }
        TranscriptLine::Error(s) => {
            for (i, part) in s.split('\n').enumerate() {
                let prefix = if i == 0 { "Error: " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Red)),
                    Span::raw(part.to_string()),
                ]));
            }
        }
        TranscriptLine::System(s) => {
            for part in s.split('\n') {
                out.push(Line::from(Span::styled(
                    part.to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
}

/// `StopReason` → 摘要符号。
///
/// `StopReason` 是 `#[non_exhaustive]`，故必须有 `_` 兜底。
fn stop_symbol(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::Completed => "✓",
        StopReason::MaxRounds => "⚠",
        StopReason::Cancelled => "✗",
        _ => "?",
    }
}

/// 退出时写回主屏 scrollback 的**纯文本**完整对话。
///
/// 与 TUI 同源：复用 [`build_text_with`]，再按显示宽度硬 wrap。
/// thinking **总是**打印 —— 退出后终端里没有 `/thinking` 开关可用了。
pub(crate) fn transcript_plain_lines(app: &App, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text = build_text_with(app, width, true);
    let mut out: Vec<String> = Vec::new();
    for line in &text.lines {
        out.extend(wrap_plain(&line.to_string(), width));
    }
    if out.is_empty() {
        out.push("(no messages yet)".to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// 显示宽度工具
// ---------------------------------------------------------------------------

/// 一个字符串的**显示宽度**（CJK = 2 列）——直接借 ratatui 的 unicode-width 口径，
/// 保证与渲染一致。
fn display_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// 按显示宽度截断到 `max` 列（超长补 `…`，仍按 char 边界）。
fn truncate_to_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = display_width(&ch.to_string());
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// 按显示宽度贪心硬 wrap（退出打印用；不做断词）。
fn wrap_plain(s: &str, width: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let cw = display_width(&ch.to_string());
        if cur_w + cw > width && !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    rows.push(cur);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::CodingView;
    use ratatui::Terminal;
    use ratatui::backend::Backend;
    use ratatui::backend::TestBackend;
    use ys_core::ToolCallId;

    fn app_with(view: CodingView) -> App {
        App::new(view)
    }

    /// 渲染成「按行」的字符画（行尾裁剪便于断言）。
    fn rows_of(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area().width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    fn flat(app: &mut App, w: u16, h: u16) -> String {
        rows_of(app, w, h).concat()
    }

    #[test]
    fn test_three_panes_present_with_status_at_bottom() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        app.transcript
            .push(TranscriptLine::Assistant("hi there".into()));

        let rows = rows_of(&mut app, 120, 40);
        assert_eq!(rows.len(), 40);
        assert!(rows[0].contains(" Chat "), "Chat 标题应在最顶：{}", rows[0]);
        // 1 行输入 → 输入区高 3，位于 y=36..39，状态行占 y=39
        assert!(
            rows[36].contains(" Input "),
            "Input 标题应在 y=36：{}",
            rows[36]
        );
        assert!(
            rows[39].contains("deepseek-chat"),
            "Status 必须在最底一行：{}",
            rows[39]
        );
    }

    #[test]
    fn test_status_stays_at_bottom_when_transcript_scrolls() {
        let mut app = app_with(CodingView::for_test());
        for i in 0..50 {
            app.transcript
                .push(TranscriptLine::User(format!("msg-{i}")));
        }
        app.follow = false;
        app.scroll_offset = 3;
        let rows = rows_of(&mut app, 120, 40);
        assert!(
            rows[39].contains("deepseek-chat"),
            "Status 不随 Chat 滚动移动：{}",
            rows[39]
        );
    }

    /// 核心回归：assistant 长文本**真 wrap**，尾部内容不被截断。
    #[test]
    fn test_long_assistant_text_wraps_and_tail_is_visible() {
        let mut app = app_with(CodingView::for_test());
        let long = format!("{}TAILMARK", "lorem ipsum ".repeat(40));
        app.transcript.push(TranscriptLine::Assistant(long));

        let text = flat(&mut app, 120, 40);
        assert!(
            text.contains("TAILMARK"),
            "长文本 wrap 后尾部必须可见（follow 贴底）"
        );
    }

    /// wrap 行数必须参与滚动，且滚动单位是**视觉行**（不是 transcript 条数）。
    ///
    /// 精确钉死数值：反例（旧 bug / 变异）「按 transcript 条数算滚动」时，
    /// 单条记录会算出 `0`，与下面三个期望值全部不符。
    #[test]
    fn test_scroll_uses_wrapped_rows_not_transcript_entries() {
        // 固定 40×40 渲染。推导（不依赖实现内部常量，只依赖布局定义）：
        //   空输入 → 输入区 3 行（1 行内容 + 上下边框）、状态行 1 行（常驻）
        //   Chat 外框高 = 40 - 3 - 1 = 36  →  Chat 内高 = 36 - 2 = 34
        //   Chat 内宽   = 40 - 2       = 38
        //   "x".repeat(N) 无空格 → 每 38 列硬折一行，视觉行数 = ceil(N / 38)
        const W: u16 = 40;
        const H: u16 = 40;
        const INNER_W: usize = W as usize - 2; // 38
        const INNER_H: usize = H as usize - 3 - 1 - 2; // 34

        let probe = app_with(CodingView::for_test());
        assert_eq!(input_height(&probe), 3, "推导前提：空输入占 3 行");

        // 3 个数据点：恰好铺满（不可滚）、多 1 行、多 6 行
        for (n, expected) in [
            (INNER_W * INNER_H, 0usize),
            (INNER_W * INNER_H + 1, 1),
            (INNER_W * (INNER_H + 6), 6),
        ] {
            let mut a = app_with(CodingView::for_test());
            a.transcript.push(TranscriptLine::Assistant("x".repeat(n)));
            let got = max_scroll_offset(&a, W, H);
            assert_eq!(
                got,
                n.div_ceil(INNER_W) - INNER_H,
                "N={n}：{} 视觉行 - {INNER_H} 可见行",
                n.div_ceil(INNER_W)
            );
            assert_eq!(got, expected, "N={n}");
        }

        // 滚动真的改变可见内容，且 Status 行不随 Chat 滚动移动
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Assistant(
            "x".repeat(INNER_W * (INNER_H + 6)),
        ));
        let max = max_scroll_offset(&app, W, H);
        assert_eq!(max, 6);
        app.follow = false;
        app.scroll_offset = max;
        let top = rows_of(&mut app, W, H);
        assert!(
            top[H as usize - 1].contains("deepseek-chat"),
            "Status 行仍在（Chat 滚动不影响布局）"
        );
    }

    #[test]
    fn test_tool_call_renders_one_line_and_hides_success_result() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            summary: "src/x.rs".into(),
            result: Some("SECRET_BODY".into()),
            success: true,
        });
        let rows = rows_of(&mut app, 120, 40);
        let joined = rows.join("\n");
        assert!(joined.contains("⏺ read src/x.rs"), "{joined}");
        assert!(joined.contains("✓"), "成功给 ✓");
        assert!(
            !joined.contains("SECRET_BODY"),
            "成功时结果默认不展开：{joined}"
        );
    }

    #[test]
    fn test_failed_tool_shows_first_result_line() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "bash".into(),
            summary: "ls".into(),
            result: Some("line1\nline2-should-not-show".into()),
            success: false,
        });
        let joined = rows_of(&mut app, 120, 40).join("\n");
        assert!(joined.contains("✗"), "失败给 ✗：{joined}");
        assert!(joined.contains("└ line1"), "失败显示结果首行：{joined}");
        assert!(
            !joined.contains("line2-should-not-show"),
            "只显示首行：{joined}"
        );
    }

    #[test]
    fn test_summary_symbols_per_stop_reason() {
        for (reason, sym) in [
            (StopReason::Completed, "✓"),
            (StopReason::MaxRounds, "⚠"),
            (StopReason::Cancelled, "✗"),
        ] {
            let mut app = app_with(CodingView::for_test());
            app.transcript.push(TranscriptLine::Summary {
                rounds: 2,
                stop: reason.clone(),
                elapsed_secs: 1.3,
            });
            let joined = rows_of(&mut app, 120, 40).join("\n");
            assert!(
                joined.contains(&format!("{sym} 2 rounds · 1.3s")),
                "{reason:?} 应渲染 {sym}：{joined}"
            );
        }
    }

    #[test]
    fn test_error_line_and_system_line_render() {
        let mut app = app_with(CodingView::for_test());
        app.transcript
            .push(TranscriptLine::Error("401 unauthorized".into()));
        app.transcript
            .push(TranscriptLine::System("✓ Logged in".into()));
        let joined = rows_of(&mut app, 120, 40).join("\n");
        assert!(joined.contains("Error: 401 unauthorized"), "{joined}");
        assert!(joined.contains("✓ Logged in"), "{joined}");
    }

    #[test]
    fn test_empty_transcript_placeholder() {
        let mut app = app_with(CodingView::for_test());
        let joined = rows_of(&mut app, 120, 40).join("\n");
        assert!(joined.contains("(no messages yet)"), "{joined}");
    }

    #[test]
    fn test_thinking_hidden_by_default_then_shown() {
        let mut app = app_with(CodingView::for_test());
        app.transcript
            .push(TranscriptLine::Thinking("inner monologue".into()));

        let hidden = rows_of(&mut app, 120, 40).join("\n");
        assert!(!hidden.contains("inner monologue"), "默认隐藏 thinking");

        // /thinking on（T10 接线；这里直接改状态）
        let mut app = app_with(CodingView::for_test());
        app.thinking_visible = true;
        app.transcript
            .push(TranscriptLine::Thinking("inner monologue".into()));
        let shown = rows_of(&mut app, 120, 40).join("\n");
        assert!(shown.contains("inner monologue"), "开启后可见：{shown}");
        assert!(shown.contains("⌁"), "带 ⌁ 前缀：{shown}");
    }

    #[test]
    fn test_turn_renders_working_in_input_and_status() {
        let mut app = app_with(CodingView::for_test());
        app.is_turning = true;
        app.working_dot = 1; // Working..
        let rows = rows_of(&mut app, 120, 40);
        let joined = rows.join("\n");
        assert!(joined.contains("Working.."), "{joined}");
        // 状态行（最底）也把 model 段换成 Working（⏳ 是宽字符，占两格）
        assert!(
            rows[39].contains("Working.."),
            "状态行也应显示 Working：{}",
            rows[39]
        );
        assert!(rows[39].contains('⏳'), "带 ⏳ 前缀：{}", rows[39]);
        assert!(
            !rows[39].contains("deepseek-chat"),
            "turn 中 model 段被替换：{}",
            rows[39]
        );
    }

    #[test]
    fn test_idle_does_not_render_working() {
        let mut app = app_with(CodingView::for_test());
        app.is_turning = false;
        let joined = rows_of(&mut app, 120, 40).join("\n");
        assert!(!joined.contains("Working"), "{joined}");
    }

    /// 组合边界：**turn 中 + 极窄**。状态行只剩 `⏳ Working.` 一段，
    /// 且**不得**泄漏 model 名（turn 中 model 段已被 Working 段替换）。
    ///
    /// 与 [`test_idle_does_not_render_working`] 形成对照：同一份 view、同一宽度下，
    /// idle 走的是 model 段，不会出现 Working。
    #[test]
    fn test_turning_status_at_extreme_narrow_keeps_working_marker() {
        let view = || CodingView {
            total_input_tokens: 1_200,
            total_output_tokens: 345,
            turn_count: 2,
            ..CodingView::for_test()
        };

        let mut app = app_with(view());
        app.is_turning = true;
        app.working_dot = 0; // Working.

        // 宽屏：Working 段在最前，model 名不出现
        let full = status_text(&app, 120);
        assert!(full.starts_with("⏳ Working."), "{full}");
        assert!(
            !full.contains("deepseek-chat"),
            "turn 中替换 model 段：{full}"
        );

        // 极窄：宽度恰好只够 `⏳ Working.`（之后的 tokens/轮数/时长全被剥掉）
        let narrow_w = display_width("⏳ Working.");
        let narrow = status_text(&app, narrow_w);
        assert_eq!(narrow, "⏳ Working.", "极窄只剩 Working 段");
        assert!(!narrow.contains("deepseek-chat"), "不泄漏 model：{narrow}");
        assert!(!narrow.contains("1.2k"), "tokens 段已剥掉：{narrow}");
        assert!(!narrow.contains("轮"), "轮数段已剥掉：{narrow}");

        // 对照：idle 在同一宽度下不含 Working
        let idle = app_with(view());
        let idle_txt = status_text(&idle, narrow_w);
        assert!(
            !idle_txt.contains("Working"),
            "idle 不显示 Working：{idle_txt}"
        );
        assert!(
            idle_txt.contains("deepseek-chat"),
            "idle 显示 model：{idle_txt}"
        );
    }

    #[test]
    fn test_status_narrowing_drops_from_tail() {
        let view = CodingView {
            total_input_tokens: 1_200,
            total_output_tokens: 345,
            turn_count: 2,
            ..CodingView::for_test()
        };
        let app = app_with(view);
        let full = status_text(&app, 120);
        assert!(full.contains("deepseek-chat"), "{full}");
        assert!(full.contains("↑1.2k ↓345"), "{full}");
        assert!(full.contains("2 轮"), "{full}");
        assert!(full.contains('s'), "含时长：{full}");

        let mid = status_text(&app, display_width("deepseek-chat · ↑1.2k ↓345 · 2 轮"));
        assert_eq!(mid, "deepseek-chat · ↑1.2k ↓345 · 2 轮", "中屏去时长");

        let narrow = status_text(&app, display_width("deepseek-chat · ↑1.2k ↓345"));
        assert_eq!(narrow, "deepseek-chat · ↑1.2k ↓345");

        let tiny = status_text(&app, 1);
        assert_eq!(tiny, "deepseek-chat", "极窄只剩 model");
    }

    #[test]
    fn test_no_model_label_shown() {
        let app = app_with(CodingView {
            model: None,
            ..CodingView::for_test()
        });
        assert_eq!(status_text(&app, 1), "(no model)");
    }

    #[test]
    fn test_too_small_terminal_renders_nothing() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        let joined = flat(&mut app, 8, 4);
        assert!(
            !joined.contains("hello") && !joined.contains(" Chat "),
            "窗口过小不渲染：{joined}"
        );
    }

    #[test]
    fn test_input_height_grows_with_lines_and_caps() {
        let mut app = app_with(CodingView::for_test());
        assert_eq!(input_height(&app), 3, "1 行内容 + 2 边框");

        app.input.insert_str("a\nb\nc");
        assert_eq!(input_height(&app), 5, "3 行内容 + 2 边框");

        app.input.insert_str(&"\n".repeat(20));
        assert_eq!(
            input_height(&app),
            (INPUT_MAX_LINES + 2) as u16,
            "上限 5 行内容"
        );
    }

    /// Shift+Enter 走的 `insert_char('\n')`（逐步插入）同样撑高输入区。
    ///
    /// 与上面的 `insert_str` 路径互为对照：一条整段插入、一条逐 char 插入，
    /// 都必须被 `lines()` 数到。
    #[test]
    fn test_input_height_grows_with_shift_enter_newlines() {
        let mut app = app_with(CodingView::for_test());
        assert_eq!(input_height(&app), 3);

        for (n, expect) in [(1usize, 4u16), (2, 5), (3, 6), (10, 7)] {
            app.input = crate::input::InputBuffer::new();
            app.input.insert_str("x");
            for _ in 0..n {
                app.input.insert_char('\n');
            }
            assert_eq!(
                input_height(&app),
                expect,
                "{n} 个换行 → {} 行内容 + 2",
                n + 1
            );
        }
    }

    // -----------------------------------------------------------------------
    // 输入区光标定位（T9）
    // -----------------------------------------------------------------------

    /// 渲染一帧并取回后端记录的光标位置。
    fn cursor_pos(app: &mut App, w: u16, h: u16) -> ratatui::layout::Position {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        terminal.backend_mut().get_cursor_position().unwrap()
    }

    /// 光标**列**按显示宽度算：CJK 占 2 列。
    ///
    /// 120×40 下空/单行输入 → 输入区 y=36..39，`Block` 内区 x=1、y=37。
    /// `"中文"` 光标在末尾：显示宽 4 列 → x = 1 + 4 = 5。
    /// 反例（按 char 数算列 → x = 3，按字节算 → x = 7）都会被本断言抓住。
    #[test]
    fn test_input_cursor_column_uses_display_width_for_cjk() {
        let mut app = app_with(CodingView::for_test());
        app.input.insert_str("中文");
        assert_eq!(app.input.cursor, 2);

        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(5, 37),
            "CJK 光标列 = 1（边框）+ 4（显示宽），不是 char 数 2 也不是字节 6"
        );
    }

    /// 光标**行**按 `cursor_row_col` 的 char 语义（`\n` 计数）走。
    ///
    /// `"ab\ncd"` → 2 行内容 → 输入区高 4（y=35..39），内区 y=36、高 2。
    #[test]
    fn test_input_cursor_row_follows_logical_lines() {
        let mut app = app_with(CodingView::for_test());
        app.input.insert_str("ab\ncd");
        assert_eq!(input_height(&app), 4, "2 行内容 + 2 边框");

        // 第 1 行中间：cursor = 1（"a|b"）
        app.input.cursor = 1;
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(2, 36)
        );

        // 第 2 行行首：cursor = 3（"ab\n|cd"）
        app.input.cursor = 3;
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(1, 37)
        );

        // 第 2 行行尾：cursor = 5
        app.input.cursor = 5;
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(3, 37)
        );

        // `cursor_home` 与渲染一致：回到第 2 行行首
        app.input.cursor_home();
        assert_eq!(app.input.cursor, 3);
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(1, 37),
            "home 后渲染出的光标必须在行首"
        );
    }

    /// 超宽单行：输入区**不做软换行**（高度只按 `\n` 数、`Paragraph` 也没开 wrap），
    /// 光标若落在 pane 之外就**不显示** —— 但绝不画到别的 pane 上。
    ///
    /// 这条钉住的是「不越界」这个不变量（`row < inner.height` 守卫），
    /// 不是「超宽行有光标」。
    #[test]
    fn test_long_single_line_never_draws_cursor_outside_input_pane() {
        let mut app = app_with(CodingView::for_test());
        app.input.insert_str(&"a".repeat(300));
        assert_eq!(input_height(&app), 3, "无 \\n → 仍是 1 内容行（不软换行）");

        // 内区高 1、光标折到第 2 视觉行 → 守卫拦下，光标不显示（停在原点）
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(0, 0),
            "越界的光标不显示，但也不落到 Status 行上"
        );
    }

    /// turn 中不显示光标（输入区显示 Working）。
    #[test]
    fn test_no_input_cursor_while_turning() {
        let mut app = app_with(CodingView::for_test());
        app.input.insert_str("中文");
        app.is_turning = true;
        // 未调用 set_cursor_position → 后端光标停在原点
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(0, 0)
        );
    }

    #[test]
    fn test_plain_lines_mirror_transcript_and_wrap() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        app.transcript
            .push(TranscriptLine::Assistant("x".repeat(25)));
        let lines = transcript_plain_lines(&app, 10);
        assert!(lines.contains(&"> hello".to_string()), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.chars().count() == 10),
            "长行按宽度 wrap：{lines:?}"
        );
    }

    #[test]
    fn test_plain_lines_cjk_wrap_by_display_width() {
        let mut app = app_with(CodingView::for_test());
        app.transcript
            .push(TranscriptLine::Assistant("中".repeat(12)));
        // 每字 2 列 → 10 列的宽度只能放 5 个汉字
        let lines = transcript_plain_lines(&app, 10);
        for l in &lines {
            assert!(
                display_width(l) <= 10,
                "CJK 按显示宽度 wrap（每行 <= 10 列）：{l:?} = {}",
                display_width(l)
            );
        }
        assert!(lines.iter().any(|l| l.chars().count() == 5));
    }

    #[test]
    fn test_wrap_plain_empty_line_preserved() {
        assert_eq!(wrap_plain("", 10), vec![String::new()]);
    }

    // -----------------------------------------------------------------------
    // 补全浮层（T10 / T11）
    // -----------------------------------------------------------------------

    /// 渲染成「行 + 该行是否含反显单元格」—— 反显 = 浮层的高亮行。
    fn rows_with_reverse(app: &mut App, w: u16, h: u16) -> Vec<(String, bool)> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area().width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| {
                let text: String = row.iter().map(|c| c.symbol()).collect();
                let reversed = row.iter().any(|c| c.modifier.contains(Modifier::REVERSED));
                (text, reversed)
            })
            .collect()
    }

    /// 铺满一屏聊天内容，便于观察「浮层盖在上面」。
    fn full_chat_app() -> App {
        let mut app = app_with(CodingView::for_test());
        for i in 0..40 {
            app.transcript
                .push(TranscriptLine::User(format!("CHATMARK-{i}")));
        }
        app
    }

    /// 浮层可见、**盖在 Chat 区域上**（改动只发生在 Chat 内），且不改变布局高度。
    ///
    /// 变异：删掉 `ui()` 里对 `draw_completion` 的调用 → 前后帧逐行相同 → 变红。
    #[test]
    fn test_completion_overlay_covers_chat_without_changing_layout() {
        const W: u16 = 120;
        const H: u16 = 40;
        // 空输入 → 输入区 3 行 + 状态行 1 行 → Chat 外框占 y = 0..36
        const CHAT_BOTTOM: usize = (H - 3 - 1) as usize;

        let mut app = full_chat_app();
        let before = rows_of(&mut app, W, H);

        // 只开浮层，**不动输入框** —— 否则输入区那一行也会变，
        // 就分不清「画面变了」是浮层还是输入造成的。
        app.completion = Some(crate::completion::open("/").unwrap());
        let after = rows_with_reverse(&mut app, W, H);

        let changed: Vec<usize> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter(|(_, (b, a))| *b != &a.0)
            .map(|(i, _)| i)
            .collect();
        assert!(!changed.is_empty(), "浮层必须真的画出来了");
        assert!(
            changed.iter().all(|&y| y < CHAT_BOTTOM),
            "浮层只允许盖在 Chat 区（y < {CHAT_BOTTOM}），实际改动行 {changed:?}"
        );

        // 三个 pane 的相对位置不变：输入区标题行与状态行**逐字符相同**
        assert_eq!(before[CHAT_BOTTOM], after[CHAT_BOTTOM].0, "输入区未被挤动");
        assert_eq!(
            before[H as usize - 1],
            after[H as usize - 1].0,
            "状态行未被挤动"
        );

        // 浮层内容确实可见，且**盖住了**原本的聊天内容
        let joined: String = after.iter().map(|(t, _)| t.as_str()).collect();
        assert!(joined.contains("/help"), "浮层应列出命令：{joined}");
        assert!(joined.contains("/model"), "浮层应列出 /model");
        let covered = &after[CHAT_BOTTOM - 1].0;
        assert!(
            !covered.contains("CHATMARK"),
            "浮层压在最底部一行之上：{covered}"
        );
    }

    /// 高亮行跟着 `selected` 走（↑↓ 有可见反馈）。
    #[test]
    fn test_completion_highlight_follows_selection() {
        let mut app = full_chat_app();
        app.input.insert_str("/");
        let mut state = crate::completion::open("/").unwrap();
        app.completion = Some(state.clone());

        let first = rows_with_reverse(&mut app, 120, 40);
        let (row0, _) = first.iter().find(|(_, rev)| *rev).expect("应有一行高亮");
        assert!(row0.contains("/help"), "初始高亮第 0 项：{row0}");

        state.move_down();
        app.completion = Some(state.clone());
        let second = rows_with_reverse(&mut app, 120, 40);
        let (row1, _) = second.iter().find(|(_, rev)| *rev).expect("应有一行高亮");
        assert!(row1.contains("/status"), "下移后高亮第 1 项：{row1}");
        assert_ne!(row0, row1, "高亮行必须换了一行");

        // 只应有一行高亮
        assert_eq!(
            second.iter().filter(|(_, rev)| *rev).count(),
            1,
            "同时只能高亮一项"
        );
    }

    /// Esc 关浮层后画面回到原样（被盖住的聊天内容重新露出来）。
    #[test]
    fn test_completion_overlay_disappears_on_close() {
        let mut app = full_chat_app();
        let before = rows_of(&mut app, 120, 40);

        app.completion = Some(crate::completion::open("/").unwrap());
        let open = rows_of(&mut app, 120, 40);
        assert_ne!(before, open, "开着浮层时画面不同");

        app.completion = None;
        let closed = rows_of(&mut app, 120, 40);
        assert_eq!(before, closed, "关了浮层画面完全复原");
    }

    /// 条目比 Chat 区还多时也不能画出界（仍留在 Chat 内、行数不变）。
    #[test]
    fn test_completion_overlay_clamps_to_chat_height() {
        const W: u16 = 120;
        const H: u16 = 8;
        // 8 行终端：Chat 占 0..4（4 行）、Input 占 4..7、Status 占 7
        const CHAT_BOTTOM: usize = (H as usize) - 3 - 1;

        let mut app = full_chat_app();
        let before = rows_of(&mut app, W, H);
        app.completion = Some(crate::completion::open("/").unwrap());
        let after = rows_of(&mut app, W, H);

        assert_eq!(after.len(), H as usize, "行数由终端定，浮层不改变布局");
        assert_eq!(after[CHAT_BOTTOM], before[CHAT_BOTTOM], "输入区未被挤动");
        assert_eq!(
            after[H as usize - 1],
            before[H as usize - 1],
            "状态行未被挤动"
        );
        assert!(
            after[..CHAT_BOTTOM].join("\n").contains("/help"),
            "放不下的条目被窗口裁掉，但仍画在 Chat 内：{after:?}"
        );
    }

    // -----------------------------------------------------------------------
    // 模态浮层（Prompter 的 TUI 形态）——渲染层单测（循环在 run.rs，不可单测）
    // -----------------------------------------------------------------------

    fn render<F: FnOnce(&mut Frame)>(w: u16, h: u16, f: F) -> (Vec<String>, Vec<bool>) {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(f).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area().width as usize;
        let rows: Vec<(String, bool)> = buffer
            .content()
            .chunks(width)
            .map(|row| {
                let text: String = row.iter().map(|c| c.symbol()).collect();
                let reversed = row.iter().any(|c| c.modifier.contains(Modifier::REVERSED));
                (text, reversed)
            })
            .collect();
        (
            rows.iter().map(|(t, _)| t.clone()).collect(),
            rows.iter().map(|(_, r)| *r).collect(),
        )
    }

    #[test]
    fn test_prompt_select_renders_options_and_highlight() {
        let options = vec!["deepseek".to_string(), "openai".to_string()];
        let (rows, reversed) = render(60, 20, |f| {
            prompt_select(f, "Select provider", &options, 1, 8)
        });
        let joined = rows.join("\n");
        assert!(joined.contains("Select provider"), "{joined}");
        assert!(joined.contains("deepseek"), "{joined}");
        assert!(joined.contains("openai"), "{joined}");
        assert!(joined.contains("Esc"), "应有取消提示：{joined}");

        let highlighted: Vec<&String> = rows
            .iter()
            .zip(&reversed)
            .filter(|(_, rev)| **rev)
            .map(|(t, _)| t)
            .collect();
        assert_eq!(highlighted.len(), 1, "只高亮一项");
        assert!(
            highlighted[0].contains("openai"),
            "selected=1 → {highlighted:?}"
        );
    }

    #[test]
    fn test_prompt_text_renders_buffer_and_cursor() {
        let mut buf = crate::input::InputBuffer::new();
        buf.insert_str("sk-中文");
        let (rows, pos) = {
            let backend = TestBackend::new(60, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| prompt_text(f, "API key", Some("deepseek"), &buf))
                .unwrap();
            let rows: Vec<String> = {
                let buffer = terminal.backend().buffer().clone();
                let width = buffer.area().width as usize;
                buffer
                    .content()
                    .chunks(width)
                    .map(|row| row.iter().map(|c| c.symbol()).collect())
                    .collect()
            };
            let pos = terminal.backend_mut().get_cursor_position().unwrap();
            (rows, pos)
        };
        let joined = rows.join("\n");
        assert!(joined.contains("API key"), "{joined}");
        assert!(joined.contains("deepseek"), "help 出现在标题里：{joined}");
        // CJK 每个字占两格，第二格是空白衬垫，故不能整串搜 "sk-中文"
        assert!(joined.contains("sk-") && joined.contains('中') && joined.contains('文'));

        // 光标落在输入那一行、列按**显示宽度**算：3 (`sk-`) + 4（两个汉字）= 7。
        // 变异：按 char 数算列（3 + 2 = 5）→ 本断言变红。
        // 注意：CJK 在 `TestBackend` 里占**两个单元格**（第二个是空白衬垫），
        // 所以列号必须用显示宽度量，不能数 char。
        let row_idx = rows
            .iter()
            .position(|r| r.contains("sk-"))
            .expect("应能定位输入行");
        let byte = rows[row_idx].find("sk-").unwrap();
        let text_col = display_width(&rows[row_idx][..byte]);
        assert_eq!(pos.y as usize, row_idx, "光标应在输入行上：{pos:?}");
        assert_eq!(
            pos.x as usize,
            text_col + 7,
            "CJK 按显示宽度占两列（不是 char 数）：{pos:?}"
        );
    }

    /// 模态浮层必须**盖在完整界面之上**：其余 pane 一个都不能消失。
    ///
    /// ratatui 的 `Terminal::draw` 每帧从空缓冲起画，所以「只画浮层」会得到一屏
    /// 空白 + 中间一个小方框。变异：让 [`ui_with_modal`] 不先调 [`ui`] → 变红。
    #[test]
    fn test_ui_with_modal_keeps_panes_and_overlays_on_top() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        let options = vec!["deepseek".to_string(), "openai".to_string()];

        let rows = {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| {
                    ui_with_modal(
                        f,
                        &mut app,
                        Modal::Select {
                            prompt: "Select provider",
                            options: &options,
                            selected: 0,
                            page_size: 8,
                        },
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let width = buffer.area().width as usize;
            buffer
                .content()
                .chunks(width)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let joined = rows.join("\n");
        assert!(joined.contains(" Chat "), "Chat 仍在：{joined}");
        assert!(joined.contains(" Input "), "Input 仍在：{joined}");
        assert!(joined.contains("deepseek-chat"), "状态行仍在：{joined}");
        assert!(joined.contains("Select provider"), "浮层在：{joined}");
        assert!(joined.contains("openai"), "选项在：{joined}");
    }

    /// 文本模态同样不许擦掉底下的界面。
    #[test]
    fn test_ui_with_text_modal_keeps_panes() {
        let mut app = app_with(CodingView::for_test());
        let buf = crate::input::InputBuffer::new();
        let rows = {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| {
                    ui_with_modal(
                        f,
                        &mut app,
                        Modal::Text {
                            prompt: "API key",
                            help: None,
                            input: &buf,
                        },
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let width = buffer.area().width as usize;
            buffer
                .content()
                .chunks(width)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let joined = rows.join("\n");
        assert!(joined.contains(" Chat "), "{joined}");
        assert!(joined.contains("deepseek-chat"), "{joined}");
        assert!(joined.contains("API key"), "{joined}");
    }

    #[test]
    fn test_window_start_keeps_selection_visible() {
        assert_eq!(window_start(0, 3, 5), 0, "全放得下 → 从头开始");
        assert_eq!(window_start(1, 10, 4), 0, "选中项还在窗口内");
        assert_eq!(window_start(4, 10, 4), 1, "选中项贴窗口底");
        assert_eq!(window_start(9, 10, 4), 6, "到头夹住，不越尾");
        assert_eq!(window_start(0, 0, 0), 0, "空列表不 panic");
    }
}
