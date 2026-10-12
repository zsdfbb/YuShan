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

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use ys_core::StopReason;

use crate::app::App;
use crate::completion::CompletionState;
use crate::input::InputBuffer;
use crate::transcript::TranscriptLine;

/// 输入区最多显示的内容行数（不含边框）。
const INPUT_MAX_LINES: usize = 5;
/// Chat 内容区底部与 Input 顶边框之间保留的**空白行数**。
///
/// 用空行而不是给 Chat 加一条底边框来当分隔：Input 区块自带带标题的边框
/// （见 `draw_input`），再画一条就成了相邻的两条横线；空行不添视觉噪音，
/// 拷贝时也不留痕 —— 与 Chat 去掉左右竖线是同一取向。
///
/// 它计入 `chat_block` 的 padding，故 `chat_block().inner(area)` 自动把它扣掉：
/// 内区几何的取用方（`max_scroll_offset` / `draw_transcript`）不必各自记得减它。
const INPUT_GAP_ROWS: u16 = 1;
/// 每条**非首条可见** `User` 条目之前保留的**空白行数** —— transcript **内部**的问答边界。
///
/// 上一回合的产出（assistant 文本 / 工具行 / 回合摘要）与本轮提问之间留白，多轮对话里
/// 一眼看得到「这一轮从哪开始」。与 [`INPUT_GAP_ROWS`] 正交：那个在 transcript **外部**
/// （Chat 内区底 → Input 顶边框），只保证末行不贴输入框，管不到问答边界。
///
/// 只有 `User` 之前留白 —— assistant / tool / summary 之间保持紧密，一屏能看更多内容。
/// 空行同样不添视觉噪音、拷贝不留痕（同 [`INPUT_GAP_ROWS`] 的取向）。
const USER_GAP_ROWS: usize = 2;
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

    // 帧时钟：帧内**只有 `draw_transcript` 一处**取产物，但它内部要取两次 ——
    // `build_text` 拿到产物、随后的 `line_count` 数它的视觉行数。二者必须基于**同一份**
    // 产物，否则画面按一份、滚动数学按另一份，二者错位（设计 §2.5 边界 ①）。
    // 故这里只取一次 `now`，一路传下去。
    let now = Instant::now();

    let chunks = Layout::vertical([
        Constraint::Min(3),                                // Chat
        Constraint::Length(input_height(app, area.width)), // Input（1..=N 自适应）
        Constraint::Length(1),                             // Status（最底，常驻）
    ])
    .split(area);

    draw_transcript(frame, chunks[0], app, now);
    draw_input(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);

    // 补全浮层**最后画**：Clear 后盖在 Chat 区域上（设计 §1「独立于三个 pane」）。
    // 它不参与上面的布局计算 —— 开合时三个 pane 的位置一个像素都不动。
    if let Some(state) = &app.completion {
        draw_completion(frame, chunks[0], state);
    }
}

/// 输入区 pane 高度（**含**上下两条边框）。
///
/// 行数按**软换行后的视觉行**算（[`wrap_input`]，与渲染、光标同源），不再只数 `\n`
/// —— 否则一行长文本折成多行后会被 pane 裁掉尾部。夹到 [`INPUT_MAX_LINES`]，超过则
/// 由 [`draw_input`] 滚动露出光标所在行。
pub fn input_height(app: &App, total_width: u16) -> u16 {
    let inner_w = input_inner_width(total_width);
    let rows = wrap_input(&app.input.text, app.input.cursor, inner_w).0;
    (rows.len().clamp(1, INPUT_MAX_LINES) + 2) as u16
}

/// 输入区内区宽度（总宽减左右两条边框）；不为 0。
fn input_inner_width(total_width: u16) -> usize {
    total_width.saturating_sub(2).max(1) as usize
}

/// Chat 区（含边框）—— 与 [`ui`] 的布局同源，供滚动算术复用。
fn chat_area(app: &App, area: Rect) -> Rect {
    Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_height(app, area.width)),
        Constraint::Length(1),
    ])
    .split(area)[0]
}

/// Chat 窗格的外框：**只留顶部标题线**（去掉左右竖线，拷贝友好，设计 §2.8），
/// 底部另留 [`INPUT_GAP_ROWS`] 行空白 —— 上轮输出不贴输入框。
///
/// 内区几何的唯一来源 —— `max_scroll_offset` 与 `draw_transcript` 都经它取
/// `inner(area)`（不变量 7）。`Borders::TOP` 令内区从「宽 `w-2` / 高 `h-2`」变为
/// 「宽 `w` / 高 `h-1`」（无左右竖线 ⇒ `inner.width` 仍 = `w`），再扣掉底部 padding。
/// **宽度与高度的逐项扣减都只发生在这一个调用里**，故绝不逐处手写减法。
fn chat_block() -> Block<'static> {
    Block::new()
        .borders(Borders::TOP)
        .padding(Padding::bottom(INPUT_GAP_ROWS))
        .title(" Chat ")
}

/// 允许的最大 `scroll_offset`（**视觉行**，0 = 贴底）。
///
/// 需要终端尺寸 —— 因为 wrap 行数依赖宽度、可见行数依赖高度。
/// 尺寸太小（[`ui`] 不渲染）时不滚动。
///
/// **这是事件路径的入口**（↑↓ / resize 时夹取 `scroll_offset`，见 `events.rs` / `run.rs`）；
/// 该路径没有帧时钟，故这里自取当前时刻。
///
/// 它与帧路径（[`ui`] → [`draw_transcript`]）的一致性**不靠共用时钟**：节流状态存在
/// `RenderCache` 内，同一帧内两处读到的是**同一份**「上次产物」，故即便各自取时刻，
/// 也不会「滚动按新、画面按旧」（设计 §2.5 边界 ①）。
pub fn max_scroll_offset(app: &App, width: u16, height: u16) -> usize {
    max_scroll_offset_at(app, width, height, Instant::now())
}

/// [`max_scroll_offset`] 的内部形态：显式传入 `now` —— 令单测能确定性注入时刻、
/// 不依赖真实时钟（生产的事件路径经 [`max_scroll_offset`] 自取）。
fn max_scroll_offset_at(app: &App, width: u16, height: u16, now: Instant) -> usize {
    let area = Rect::new(0, 0, width, height);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return 0;
    }
    let chat = chat_area(app, area);
    let inner = chat_block().inner(chat);
    let inner_w = inner.width.max(1);
    let inner_h = inner.height as usize;
    total_rows(app, inner_w, now).saturating_sub(inner_h)
}

/// transcript 在 `width` 下 wrap 后的总视觉行数。
fn total_rows(app: &App, width: u16, now: Instant) -> usize {
    Paragraph::new(build_text(app, width as usize, now))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &App, now: Instant) {
    let block = chat_block();
    let inner = block.inner(area);
    let inner_w = inner.width.max(1);
    let inner_h = inner.height as usize;
    let text = build_text(app, inner_w as usize, now);

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
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((offset.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(para, area);
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Input ");
    let inner = block.inner(area);
    let inner_w = input_inner_width(area.width);
    let inner_h = inner.height as usize;

    // 软换行 + 光标定位**同源**（[`wrap_input`]）：先折成视觉行，再画、再摆光标。
    // 于是「一行长文本」会折到后续视觉行显示，而不是被 pane 裁掉尾部。
    let (rows, cursor_row, cursor_col) = wrap_input(&app.input.text, app.input.cursor, inner_w);

    // turn 进行中也照常渲染输入缓冲与光标：`Working` 心跳只由状态行承担。
    // 视觉行超过 pane 时滚动，让光标所在行可见（贴住窗口底部）。
    let offset = cursor_row.saturating_sub(inner_h.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(rows.join("\n"))
            .block(block)
            .scroll((offset.min(u16::MAX as usize) as u16, 0)),
        area,
    );

    let row = cursor_row - offset;
    if row < inner_h && cursor_col < inner_w {
        frame.set_cursor_position(ratatui::layout::Position::new(
            inner.x + cursor_col as u16,
            inner.y + row as u16,
        ));
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
fn build_text(app: &App, width: usize, now: Instant) -> Text<'static> {
    build_text_with(app, width, app.thinking_visible, now)
}

/// `build_text` 的完整形态：显式传入 thinking 可见性
/// （TUI 与退出回放共用同一个开关值）。
fn build_text_with(app: &App, width: usize, thinking_visible: bool, now: Instant) -> Text<'static> {
    if app.transcript.is_empty() {
        return Text::from(vec![Line::from(Span::styled(
            "(no messages yet)",
            Style::default().fg(Color::DarkGray),
        ))]);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (idx, item) in app.transcript.iter().enumerate() {
        // 先算再借：`user_gap_rows` 要读 `lines` 的当前长度，而紧接着要可变借它。
        let gap = user_gap_rows(item, !lines.is_empty());
        lines.extend((0..gap).map(|_| Line::from("")));
        push_transcript_line(&mut lines, item, width, thinking_visible, app, idx, now);
    }
    Text::from(lines)
}

/// `User` 条目之前应插入的空白行数 —— 见 [`USER_GAP_ROWS`]。
///
/// **除了**「该条目之前尚无任何可见行」时取 `0`：会话首条提问不留白，免得开局白占两行。
/// 以「已有可见行」而非「下标 > 0」为判据 —— 首条若是 `/thinking off` 下不产出行
/// 的 `Thinking`，按下标会误判成非首条，顶部凭空多两行。
///
/// 只返回**计数**、不代插入：两个渲染循环的目标容器不同（TUI 是 `Vec<Line>`、
/// 退出回放是 `Vec<String>`），共享的是规则而不是插入动作。**不得**把本判断下沉进
/// [`push_non_assistant`]：退出回放（[`transcript_plain_lines`]）每条都新开一个局部
/// `Vec<Line>` 再 extend，在那个 vec 上判空恒为真 ⇒ 会给**每条**用户行都加空行。
fn user_gap_rows(item: &TranscriptLine, has_visible_before: bool) -> usize {
    if has_visible_before && matches!(item, TranscriptLine::User(_)) {
        USER_GAP_ROWS
    } else {
        0
    }
}

fn push_transcript_line(
    out: &mut Vec<Line<'static>>,
    item: &TranscriptLine,
    width: usize,
    thinking_visible: bool,
    app: &App,
    idx: usize,
    now: Instant,
) {
    if let TranscriptLine::Assistant(s) = item {
        // assistant markdown 走缓存 + `md::render`（**借用纪律**：`lines_for` 返回的切片
        // 生命周期绑在守卫上，必须在守卫作用域内 clone 进目标，不得带出；不重入 `borrow_mut`）。
        {
            let mut cache = app.md_cache.borrow_mut();
            cache.sync_len(app.transcript.len());
            let styles = crate::styles::AssistantStyles;
            out.extend(cache.lines_for(idx, s, width, styles, now).iter().cloned());
        }
        return;
    }
    push_non_assistant(out, item, width, thinking_visible);
}

/// 非 `Assistant` 变体 → 行（TUI 与退出回放共用，行为一致）。
fn push_non_assistant(
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
        // Assistant 由 [`push_transcript_line`] 提前分流（走缓存），正常不会到此处。
        // 但本函数在绘制路径上 —— **不得 panic**（否则绘制 panic 会把终端留在 raw mode，
        // 设计 §2.11）。故命中时降级为 raw：`\n` 原样切行、无样式（与
        // [`transcript_plain_lines`] 的 Assistant 分支同形）；测试期用 `debug_assert!` 大声报出。
        TranscriptLine::Assistant(s) => {
            debug_assert!(
                false,
                "Assistant 应由 push_transcript_line 提前分流（走缓存），不应到达 push_non_assistant"
            );
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
        StopReason::Truncated => "✂",
        // 「空产出」不是错误也不是完成：用 ∅ 表示「回合结束了，但模型什么也没给出」。
        StopReason::NoOutput => "∅",
        _ => "?",
    }
}

/// 退出时写回主屏 scrollback 的**纯文本**完整对话。
///
/// `Assistant` 走 **raw 路径**（设计 §2.10）：按 `\n` 原样切行、整行单色、**不预折行、
/// 不经缓存** —— 主屏留**原始 markdown 源**（可复制可搜），折行交给终端。
/// 其它变体行为不变（仍按显示宽度 wrap）。
/// thinking 遵守 `app.thinking_visible`（默认隐藏，`/thinking on` 后可见）。
pub(crate) fn transcript_plain_lines(app: &App, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for item in &app.transcript {
        let has_visible_before = !out.is_empty();
        if let TranscriptLine::Assistant(s) = item {
            out.extend(s.split('\n').map(str::to_string));
            continue;
        }
        // 问答边界与 TUI 内一致：空白行必须在 `wrap_plain` **之前**落到 `out` 上
        // （空行经 wrap 仍是 1 行空串，但顺序必须是「先空行、后内容」）。
        out.extend(std::iter::repeat_n(
            String::new(),
            user_gap_rows(item, has_visible_before),
        ));
        let mut lines: Vec<Line<'static>> = Vec::new();
        push_non_assistant(&mut lines, item, width, app.thinking_visible);
        for line in &lines {
            out.extend(wrap_plain(&line.to_string(), width));
        }
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

/// 输入区软换行：把输入文本按**显示宽度**折成视觉行（`\n` 强制换行），
/// 并给出光标落在哪一视觉行的哪一显示列。
///
/// 渲染（[`draw_input`]）、高度（[`input_height`]）与光标定位**共用**本函数 ——
/// 三者必须基于同一份折行结果，否则会「画在一行、光标在另一行」。折行规则与退出
/// 回放 [`wrap_plain`] 同源：按列贪心、不断词。
///
/// 光标恰好停在「填满一行的行尾」时，会多折出一个空行把光标放到它的行首，免得
/// 光标列 `== width` 越界。故在那一处，行数依赖光标位置 —— 这是有意的：行数要
/// 够放得下光标。
fn wrap_input(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut cursor_pos: Option<(usize, usize)> = None;

    for (i, ch) in text.chars().enumerate() {
        if i == cursor {
            cursor_pos = Some((rows.len(), cur_w));
        }
        if ch == '\n' {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
            continue;
        }
        let cw = display_width(&ch.to_string());
        if cur_w + cw > width && !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
            if i == cursor {
                cursor_pos = Some((rows.len(), 0));
            }
        }
        cur.push(ch);
        cur_w += cw;
    }

    // 光标在缓冲末尾时循环没机会记 —— 补记到当前行尾（下一行即待 push 的 `cur`）。
    let (mut row, mut col) = cursor_pos.unwrap_or((rows.len(), cur_w));
    rows.push(cur);

    // 光标落在「填满一行的行尾」：折出一个空行放它，避免列越界。
    if col >= width {
        row += 1;
        col = 0;
        if row >= rows.len() {
            rows.push(String::new());
        }
    }
    (rows, row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::CodingView;
    use ratatui::Terminal;
    use ratatui::backend::Backend;
    use ratatui::backend::TestBackend;
    use std::time::Duration;
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
    ///
    /// **几何从 `chat_block().inner(chat_area(...))` 推导**（设计 §「内区几何的唯一来源」）：
    /// 不写 `H - 3 - 1 - 1 - 1` 之类的硬编码魔法数字 —— 顶边框策略或
    /// [`INPUT_GAP_ROWS`] 一变，这里自动跟随。
    #[test]
    fn test_scroll_uses_wrapped_rows_not_transcript_entries() {
        const W: u16 = 40;
        const H: u16 = 40;

        let probe = app_with(CodingView::for_test());
        assert_eq!(input_height(&probe, W), 3, "推导前提：空输入占 3 行");

        // 从布局推：避免硬编码「h-3-1-1」这类随 spacing / 边框策略变化的魔法数字。
        let chat = chat_area(&probe, Rect::new(0, 0, W, H));
        let inner = chat_block().inner(chat);
        let inner_w = inner.width as usize;
        let inner_h = inner.height as usize;
        assert_eq!(inner_w, 40, "推导前提：无左右竖线 ⇒ w=40");
        // 内区高 = Chat 区高 − 顶边框 − 输入间距。**不是**终端总高减常数：
        // Chat 区高还要先扣掉输入区与状态行。
        assert_eq!(
            inner_h,
            chat.height as usize - 1 - INPUT_GAP_ROWS as usize,
            "内区高 = Chat 区高({}) − 顶边框 − 间距({INPUT_GAP_ROWS})",
            chat.height
        );
        assert_eq!(
            inner_h, 34,
            "H=40 时 = 40 − 3(输入) − 1(状态) − 1(顶边框) − 1(间距)"
        );

        // 3 个数据点：恰好铺满（不可滚）、多 1 行、多 6 行
        for (n, expected) in [
            (inner_w * inner_h, 0usize),
            (inner_w * inner_h + 1, 1),
            (inner_w * (inner_h + 6), 6),
        ] {
            let mut a = app_with(CodingView::for_test());
            a.transcript.push(TranscriptLine::Assistant("x".repeat(n)));
            let got = max_scroll_offset(&a, W, H);
            assert_eq!(
                got,
                n.div_ceil(inner_w) - inner_h,
                "N={n}：{} 视觉行 - {inner_h} 可见行",
                n.div_ceil(inner_w)
            );
            assert_eq!(got, expected, "N={n}");
        }

        // 滚动真的改变可见内容，且 Status 行不随 Chat 滚动移动
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Assistant(
            "x".repeat(inner_w * (inner_h + 6)),
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

    /// **Chat 末行与 Input 顶边框之间必须有 [`INPUT_GAP_ROWS`] 行空白** ——
    /// 上轮输出与输入框压在一起看不清边界。用的是**空行**而非横线：
    /// Input 自带带标题的边框，再画一条会成相邻双线，且横线会被拷进复制内容。
    ///
    /// **变异**：去掉 `chat_block` 的 `padding(bottom: …)` ⇒ 间距被内区吃掉、
    /// Chat 末行贴到 Input 顶框 ⇒ 本用例红。
    #[test]
    fn test_chat_has_one_row_gap_before_input() {
        const W: u16 = 40;
        const H: u16 = 40;
        // 铺满一屏以上：`follow` 靠滚动实现，内容不足一屏时首行在顶部、
        // 末行反而是空的 —— 本用例要的正是「最后一行内容」贴着间距。
        let mut app = full_chat_app();

        let chat = chat_area(&app, Rect::new(0, 0, W, H));
        let inner = chat_block().inner(chat);
        // Chat 内区末行 = inner.y + inner.height - 1；其下 INPUT_GAP_ROWS 行是间距
        let chat_inner_bottom = inner.y as usize + inner.height as usize - 1;
        let gap_first = inner.y as usize + inner.height as usize;
        let input_top = gap_first + INPUT_GAP_ROWS as usize;

        let rows = rows_of(&mut app, W, H);
        // 末行内容必须可见 —— 间距不得把 Chat 的最后一行吃成空行
        assert!(
            rows[chat_inner_bottom].contains("CHATMARK-39"),
            "Chat 末行应显示最后一条记录（贴底）：{:?}",
            rows[chat_inner_bottom]
        );
        // 间距行必须是空白：既不是内容，也不是横线
        for (i, row) in rows[gap_first..input_top].iter().enumerate() {
            assert!(
                row.trim().is_empty(),
                "间距行 y={} 必须为空：{row:?}",
                gap_first + i
            );
        }
        // 间距之下才是 Input 顶边框（带 `Input` 标题）
        assert!(
            rows[input_top].contains("Input"),
            "Input 顶边框应在间距之后：{:?}",
            rows[input_top]
        );
    }

    /// 问答边界：每条**非首条可见** `User` 之前恒 [`USER_GAP_ROWS`] 行空白，首条不留白。
    ///
    /// 相邻两条 `User`（回合中插话 / 排队回声）之间**同样留白** —— 规则统一为
    /// 「非首条可见即留」，不按「上一条是不是 User」特判。
    ///
    /// **变异**：`user_gap_rows` 令 `has_visible_before` 恒假（本次改动回退）⇒ 空白行断言红；
    /// 去掉该参数的判断（对首条也留白）⇒ `lines[0]` 断言红。
    #[test]
    fn test_user_gap_rows_before_each_user_but_not_the_first() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("q1".into()));
        app.transcript.push(TranscriptLine::Assistant("a1".into()));
        app.transcript.push(TranscriptLine::User("q2".into()));
        app.transcript.push(TranscriptLine::User("q3".into()));

        let lines = transcript_plain_lines(&app, 80);
        assert_eq!(
            lines,
            vec!["> q1", "a1", "", "", "> q2", "", "", "> q3"],
            "q1 首条不留白；q2 / q3 之前各 2 行空白"
        );
    }

    /// 空白行必须计入**滚动数学** —— `max_scroll_offset` 用同一份 `build_text` 数视觉行。
    ///
    /// 精确钉死数值：20 条用户行 + 19 处 × [`USER_GAP_ROWS`] = 58 视觉行。
    /// **变异**：只往渲染路径加空行、`build_text` 里不加（两条路径分叉）⇒ 算出 0（20 − 34）⇒ 红。
    #[test]
    fn test_user_gap_is_counted_in_scroll_math() {
        const W: u16 = 40;
        const H: u16 = 40;
        let mut app = app_with(CodingView::for_test());
        for i in 0..20 {
            app.transcript
                .push(TranscriptLine::User(format!("m{i:02}")));
        }

        let chat = chat_area(&app, Rect::new(0, 0, W, H));
        let inner = chat_block().inner(chat);
        assert_eq!(inner.height as usize, 34, "推导前提：内区高");

        // 20 条用户行 + 19 处 × 2 行空白 = 58 视觉行。**写死 58**（不写成含
        // `USER_GAP_ROWS` 的算式）：算式跟着常量走，常量被改成 0 时本用例会一起
        // 变成「0 = 0」而照样绿 —— 那就钉不住「两条空白行」这个契约了。
        assert_eq!(
            max_scroll_offset(&app, W, H),
            58 - inner.height as usize,
            "滚动上限必须含空白行（不含则为 0：20 − 34）"
        );
    }

    /// 屏上形状（TUI 渲染路径，不只是 `build_text` 的数据）：第 2 条用户行上方真的空两行。
    #[test]
    fn test_rendered_screen_shows_blank_rows_above_second_user_line() {
        const W: u16 = 40;
        const H: u16 = 12;
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("q1".into()));
        app.transcript.push(TranscriptLine::Assistant("a1".into()));
        app.transcript.push(TranscriptLine::User("q2".into()));

        let chat = chat_area(&app, Rect::new(0, 0, W, H));
        let inner = chat_block().inner(chat);
        let rows = rows_of(&mut app, W, H);

        assert!(
            rows[inner.y as usize].contains("> q1"),
            "首条贴顶不留白：{:?}",
            rows[inner.y as usize]
        );
        // 从 `a1` 所在行往下数：紧邻两行必须空白，第三行才是新的用户行。
        let a1 = rows
            .iter()
            .position(|r| r.contains("a1"))
            .expect("assistant 行应可见");
        for (i, row) in rows[a1 + 1..a1 + 1 + USER_GAP_ROWS].iter().enumerate() {
            assert!(row.trim().is_empty(), "第 {} 行空白被占：{row:?}", i + 1);
        }
        assert!(
            rows[a1 + 1 + USER_GAP_ROWS].contains("> q2"),
            "第 2 条用户行应在 2 行空白之后：{:?}",
            rows[a1 + 1 + USER_GAP_ROWS]
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
            (StopReason::Truncated, "✂"),
            (StopReason::NoOutput, "∅"),
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

    /// turn 中：输入区照常显示输入缓冲与光标，`Working` 心跳只在状态行出现一次。
    #[test]
    fn test_turn_shows_input_text_and_working_only_in_status() {
        let mut app = app_with(CodingView::for_test());
        app.input.insert_str("queued message");
        app.is_turning = true;
        app.working_dot = 1; // Working..
        let rows = rows_of(&mut app, 120, 40);
        // 内容仍是 1 行 → 布局与既有推导一致：输入区 36..39，状态行 39
        let input_pane = rows[36..39].join("\n");
        assert!(input_pane.contains("queued message"), "{input_pane}");
        assert!(
            !input_pane.contains("Working"),
            "输入区不再重复 Working：{input_pane}"
        );
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
        assert_eq!(
            rows.join("\n").matches("Working..").count(),
            1,
            "Working 全屏只出现一次（状态行）"
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
        assert_eq!(input_height(&app, 120), 3, "1 行内容 + 2 边框");

        app.input.insert_str("a\nb\nc");
        assert_eq!(input_height(&app, 120), 5, "3 行内容 + 2 边框");

        app.input.insert_str(&"\n".repeat(20));
        assert_eq!(
            input_height(&app, 120),
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
        assert_eq!(input_height(&app, 120), 3);

        for (n, expect) in [(1usize, 4u16), (2, 5), (3, 6), (10, 7)] {
            app.input = crate::input::InputBuffer::new();
            app.input.insert_str("x");
            for _ in 0..n {
                app.input.insert_char('\n');
            }
            assert_eq!(
                input_height(&app, 120),
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
        assert_eq!(input_height(&app, 120), 4, "2 行内容 + 2 边框");

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

    /// 超宽单行**软换行**：折到后续视觉行，尾部内容可见、输入区随之撑高。
    ///
    /// 变异：把 `input_height` 改回只数 `\n`（或 `draw_input` 关掉折行）⇒ 高度仍为
    /// 3、超出首行的内容被裁掉，本测试变红。
    #[test]
    fn test_long_single_line_soft_wraps_and_tail_is_visible() {
        let mut app = app_with(CodingView::for_test());
        // 120 列下内区宽 118：写 3 行多一点，第三行留在末尾。
        app.input.insert_str(&"a".repeat(118 * 2 + 10));
        assert_eq!(input_height(&app, 120), 5, "3 视觉行（118×2 + 10）+ 2 边框");

        let rows = rows_of(&mut app, 120, 40);
        // 输入区 = 3 内容行 + 上下边框，紧邻状态行之上：内容在 y=35/36/37。
        for y in [35usize, 36, 37] {
            assert!(rows[y].contains('a'), "第 {y} 行应有折行内容：{}", rows[y]);
        }
        assert!(!rows[38].contains('a'), "y=38 是输入区底边框：{}", rows[38]);
    }

    /// 光标跟随软换行：长单行的光标落在**被折出的视觉行**上，而不是只停在首行。
    ///
    /// 变异：光标行仍旧用「逻辑行号 + `display_width / inner_w`」之外的其他算法、
    /// 或与渲染折行不同步 ⇒ 光标不在末尾列上，本测试变红。
    #[test]
    fn test_cursor_follows_soft_wrap_on_long_line() {
        let mut app = app_with(CodingView::for_test());
        // 118 列内区：写满一整行 + 5 个字符 ⇒ 光标应在第 2 视觉行、第 5 列。
        app.input.insert_str(&"a".repeat(118 + 5));
        // 内区 x=1、y=36；第 2 视觉行 y=37、列 5 → x=6。
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(6, 37),
            "光标落在折出的第 2 视觉行行内第 5 列"
        );
    }

    /// 视觉行超过上限时**滚动**，光标所在行始终可见（贴住窗口底部）。
    #[test]
    fn test_input_scrolls_to_keep_cursor_visible_past_cap() {
        let mut app = app_with(CodingView::for_test());
        // 10 个逻辑行、每行都短 ⇒ 10 视觉行 > 上限 5。
        let text: Vec<String> = (0..10).map(|i| format!("L{i}")).collect();
        app.input.insert_str(&text.join("\n"));
        assert_eq!(input_height(&app, 120), (INPUT_MAX_LINES + 2) as u16);

        let rows = rows_of(&mut app, 120, 40);
        let joined = rows.join("\n");
        assert!(joined.contains("L9"), "光标所在末行必须可见：{joined}");
        assert!(!joined.contains("L0"), "顶部行应滚出窗口：{joined}");

        // 光标落在窗口底行（内区 y=33..37 的第 5 行 = y=37），行内列 2 → x=3。
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(3, 37)
        );
    }

    /// turn 中光标照常显示：输入区定位与 idle 同一套（CJK 按显示宽度算列）。
    #[test]
    fn test_input_cursor_visible_while_turning() {
        let mut app = app_with(CodingView::for_test());
        app.input.insert_str("中文");
        app.is_turning = true;
        assert_eq!(
            cursor_pos(&mut app, 120, 40),
            ratatui::layout::Position::new(5, 37)
        );
    }

    /// 回放与 transcript 同源：`User` 仍按宽度 wrap，`Assistant` 走 **raw**（原样、
    /// 不预折行，设计 §2.10）。
    #[test]
    fn test_plain_lines_mirror_transcript_and_assistant_raw() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        app.transcript
            .push(TranscriptLine::Assistant("x".repeat(25)));
        let lines = transcript_plain_lines(&app, 10);
        assert!(lines.contains(&"> hello".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"x".repeat(25)),
            "Assistant 回放不预折行（原样一条）：{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.chars().count() == 10),
            "Assistant 行不得被折成 10 列：{lines:?}"
        );
    }

    #[test]
    fn test_plain_lines_thinking_follows_visibility_toggle() {
        let mut app = app_with(CodingView::for_test());
        app.transcript
            .push(TranscriptLine::Thinking("inner monologue".into()));

        app.thinking_visible = false;
        let hidden = transcript_plain_lines(&app, 40);
        assert!(
            !hidden.iter().any(|l| l.contains("inner monologue")),
            "thinking_visible=false 时退出回放不应含思考：{hidden:?}"
        );

        app.thinking_visible = true;
        let shown = transcript_plain_lines(&app, 40);
        assert!(
            shown.iter().any(|l| l.contains("inner monologue")),
            "thinking_visible=true 时退出回放应含思考：{shown:?}"
        );
    }

    /// 非 `Assistant` 变体的回放仍按显示宽度 wrap（`Assistant` 改走 raw，见上）。
    #[test]
    fn test_plain_lines_cjk_wrap_by_display_width() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("中".repeat(12)));
        // 每字 2 列；`> ` 前缀 2 列 → 10 列的宽度只能再放 4 个汉字（首行）
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

    /// `wrap_input`：空串 → 一行空、光标 (0,0)。
    #[test]
    fn test_wrap_input_empty() {
        assert_eq!(wrap_input("", 0, 10), (vec![String::new()], 0, 0));
    }

    /// `\n` 强制换行；光标按逻辑行定位（这里是 `c|d` 之间）。
    #[test]
    fn test_wrap_input_newlines_split_rows() {
        assert_eq!(
            wrap_input("ab\ncd", 4, 10),
            (vec!["ab".to_string(), "cd".to_string()], 1, 1)
        );
    }

    /// 超宽按**显示宽度**折行（CJK 每字 2 列），光标跟随折行。
    ///
    /// 变异：按 char 数而非显示宽度折 ⇒ 每行放 4 个汉字、行数变 2，本测试变红。
    #[test]
    fn test_wrap_input_wraps_cjk_by_display_width() {
        // 宽 4 ⇒ 每行 2 个汉字（每个 2 列）。
        let (rows, row, col) = wrap_input("中文中文中", 5, 4);
        assert_eq!(
            rows,
            vec!["中文".to_string(), "中文".to_string(), "中".to_string()]
        );
        assert_eq!((row, col), (2, 2), "光标在末行「中」之后 = 第 2 列");
    }

    /// 光标恰好停在「填满一行的行尾」：折出一个空行放它，列不越界。
    #[test]
    fn test_wrap_input_cursor_at_full_row_end_gets_virtual_row() {
        assert_eq!(
            wrap_input("abcd", 4, 4),
            (vec!["abcd".to_string(), String::new()], 1, 0),
            "光标落到下一（虚拟）空行行首"
        );
    }

    // -----------------------------------------------------------------------
    // md 渲染接线（U2）：几何改道 / 回放 raw / line_count 一致 / 节流
    // -----------------------------------------------------------------------

    /// N8：退出回放保留**原始 markdown 源**（不套样式、不去标记）。
    #[test]
    fn n8_plain_lines_contain_raw_markdown() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Assistant(
            "# 标题\n\n**粗体** 与 `code`".into(),
        ));
        let lines = transcript_plain_lines(&app, 80);
        let joined = lines.join("\n");
        assert!(joined.contains('#'), "回放应保留原始 `#`：{lines:?}");
        assert!(joined.contains("**"), "回放应保留原始 `**`：{lines:?}");
        assert!(joined.contains("`code`"), "回放应保留反引号码：{lines:?}");
        assert!(
            lines.iter().any(|l| l == "# 标题"),
            "按 \\n 原样成行：{lines:?}"
        );
    }

    /// N4：任意宽度下，**整段** `Paragraph::line_count(w)` 应等于**逐行累计**折行 ——
    /// 这是滚动数学（用 `line_count` 数视觉行）赖以成立的一致性前提。
    #[test]
    fn n4_line_count_equals_per_line_accumulation() {
        let sources = [
            // 含 CJK 表格
            "| 名称 | 值 |\n| -- | -- |\n| 中文 | 数字 |\n",
            // 不含表格（标题 / 粗体 / 行内码 / 中文正文 / 列表）
            "# 标题\n\n这是 **粗体** 与 `code` 的一段中文正文\n\n- 一\n- 二\n",
        ];
        for src in sources {
            for w in 1..=80u16 {
                let mut app = app_with(CodingView::for_test());
                app.transcript
                    .push(TranscriptLine::Assistant(src.to_string()));
                let text = build_text_with(&app, w as usize, false, Instant::now());
                let whole = Paragraph::new(text.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(w);
                let per_line: usize = text
                    .lines
                    .iter()
                    .map(|l| {
                        Paragraph::new(Text::from(vec![l.clone()]))
                            .wrap(Wrap { trim: false })
                            .line_count(w)
                    })
                    .sum();
                assert_eq!(
                    whole, per_line,
                    "w={w}：整段 line_count 应等于逐行累计（src={src:?}）"
                );
            }
        }
    }

    /// N6 集成版：渲染**非表格** md 后，Chat 内区各行不含 `│`（表格框线是有意例外）。
    #[test]
    fn n6_chat_inner_has_no_vertical_border_for_non_table_md() {
        let mut app = app_with(CodingView::for_test());
        let src = "# 标题\n\n段落 **粗体**\n\n> 引用\n\n- 项\n\n```\ncode\n```\n";
        app.transcript.push(TranscriptLine::Assistant(src.into()));

        let rows = rows_of(&mut app, 120, 40);
        let chat = chat_area(&app, Rect::new(0, 0, 120, 40));
        let inner = chat_block().inner(chat);
        for y in inner.y..inner.bottom() {
            let row = &rows[y as usize];
            assert!(
                !row.contains('│'),
                "Chat 内区（row {y}）不应含 `│`：{row:?}"
            );
        }
    }

    /// `Borders::TOP` + 底部 [`INPUT_GAP_ROWS`] padding ⇒ 内区宽 `w`、
    /// 内区高 `h − 1（顶边框） − INPUT_GAP_ROWS`。
    #[test]
    fn geometry_inner_keeps_w_and_drops_border_and_gap() {
        let area = Rect::new(0, 0, 40, 36);
        let inner = chat_block().inner(area);
        assert_eq!(inner.width, 40, "无左右竖线 ⇒ 内区宽 = w");
        assert_eq!(
            inner.height,
            36 - 1 - INPUT_GAP_ROWS,
            "内区高 = h − 顶边框 − 间距"
        );
        assert_eq!(inner.height, 34, "h=36 时");
    }

    /// 矮终端的边界：内区还要再扣掉 [`INPUT_GAP_ROWS`]，Chat 区可能被压到只剩边框。
    /// 此高度下 `inner.height` 最多夹到 0 —— **不得**回绕成巨大的 `usize`（rustc 的
    /// 无符号下溢在 debug 会 panic），渲染与滚动算术也不得 panic。
    #[test]
    fn geometry_survives_tiny_heights() {
        for h in MIN_HEIGHT..=8 {
            let mut app = full_chat_app();
            let chat = chat_area(&app, Rect::new(0, 0, 40, h));
            let inner = chat_block().inner(chat);
            assert!(
                inner.height <= chat.height,
                "h={h}：内区不得高于 Chat 区（chat={}，inner={}）",
                chat.height,
                inner.height
            );
            let _ = rows_of(&mut app, 40, h);
            let _ = max_scroll_offset(&app, 40, h);
        }
    }

    /// 几何改道的核心：贴底时最后一行必须可见，且 `max_scroll_offset` 与 `line_count` 一致。
    ///
    /// **变异 M4**：把内区改回 `saturating_sub(2)`（或令 `inner_h` 回到 `h-2`）⇒
    /// 滚动多算一行、Chat 内区末行留空 ⇒ 本用例变红。
    #[test]
    fn geometry_follow_keeps_last_line_and_scroll_matches_line_count() {
        const W: u16 = 40;
        const H: u16 = 40;
        let mut app = app_with(CodingView::for_test());
        // 50 条各占一行的 User 行：总视觉行 = 50，远超可见行
        for i in 0..50 {
            app.transcript
                .push(TranscriptLine::User(format!("m{i:02}")));
        }
        assert!(app.follow, "默认贴底");

        let chat = chat_area(&app, Rect::new(0, 0, W, H));
        let inner = chat_block().inner(chat);
        let inner_w = inner.width;
        let inner_h = inner.height as usize;
        // 40 − 3（输入区）− 1（状态行）− 1（顶边框）− 1（间距）= 34
        // 与 `chat_block().inner(chat_area(...))` 同一份几何 —— 不要逐处手算。
        assert_eq!(inner_h, 34, "内区高 = Chat 区高 − 顶边框 − INPUT_GAP_ROWS");

        // 滚动上限应与同一 now 下的 line_count 一致
        let now = Instant::now();
        let text = build_text(&app, inner_w as usize, now);
        let total = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(inner_w);
        assert_eq!(
            max_scroll_offset_at(&app, W, H, Instant::now()),
            total.saturating_sub(inner_h),
            "滚动上限 = 总视觉行 - 可见行（35）"
        );

        // 贴底：内容最后一行（m49）必须落在 Chat 内区最后一行上
        let rows = rows_of(&mut app, W, H);
        let last_inner_row = (inner.y + inner.height - 1) as usize;
        assert!(
            rows[last_inner_row].contains("m49"),
            "贴底时最后一行必须可见：row {last_inner_row} = {:?}",
            rows[last_inner_row]
        );
        // 内区首行也应是内容（少算 inner_h 会在底部留空）
        let first_inner_row = inner.y as usize;
        assert!(
            rows[first_inner_row].contains('m'),
            "内区首行应是内容而非空白：row {first_inner_row} = {:?}",
            rows[first_inner_row]
        );
    }

    /// 节流一致性（R11）：同一帧内该条已增长但节流窗口未到时，画面与滚动上限必须基于
    /// **同一份**上次产物（都按旧内容）—— 不得「滚动按新、画面按旧」。
    ///
    /// 变异：去掉节流（逐次重算）⇒ 首条断言与渲染计数断言变红。
    #[test]
    fn throttle_consistency_scroll_and_draw_share_product() {
        crate::md::reset_render_calls();
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Assistant("OLD".into()));

        let t0 = Instant::now();
        // 首次取产物，填充缓存（streaming 槽位 = 0）
        let _ = build_text_with(&app, 40, false, t0);
        assert_eq!(crate::md::render_calls(), 1, "首次必算一次");

        // 同帧内该条继续增长（模拟 delta），但仍在节流窗口内
        if let Some(TranscriptLine::Assistant(s)) = app.transcript.last_mut() {
            s.push_str(" NEWTAIL");
        }
        let t1 = t0 + Duration::from_millis(50); // < RENDER_THROTTLE(100ms)

        // 画面：build_text_with；滚动：max_scroll_offset_at —— 共用同一个 now
        let text = build_text_with(&app, 40, false, t1);
        let shown: String = text.lines.iter().map(|l| l.to_string()).collect();
        assert!(
            !shown.contains("NEWTAIL"),
            "节流窗口内画面应沿用旧产物：{shown:?}"
        );
        let inner_h = chat_block()
            .inner(chat_area(&app, Rect::new(0, 0, 40, 40)))
            .height as usize;
        let expected = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(40)
            .saturating_sub(inner_h);
        assert_eq!(
            max_scroll_offset_at(&app, 40, 40, t1),
            expected,
            "滚动上限必须按同一份（旧）产物算"
        );
        assert_eq!(
            crate::md::render_calls(),
            1,
            "窗口内两处取产物共享缓存、均不重算"
        );
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
