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
//! # 滚动模型
//!
//! 滚动以 **wrap 后的视觉行**为单位（不是 transcript 条目数）——
//! 旧实现每条 `TranscriptLine` 算 1 行、而外层 `Paragraph` 又会真 wrap，
//! 二者不一致，滚动位置与实际看到的完全错位。
//!
//! 折行口径用 ratatui 自己的 `Paragraph::line_count`（内部 `WordWrapper` 的同一实现，
//! 故与渲染严格一致；**不自写折行** —— CJK 列宽必须与渲染一致）。
//!
//! **视窗化**（ADR-0045）：折行结果按**条目**记住（[`crate::layout`]），
//! 渲染只取视窗覆盖的那些行 ⇒ 滚动与重绘的成本**与会话长度解耦**。
//! 视窗切片依赖「整段折行 == 逐行折行之和」（N4）。

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use ys_core::StopReason;

use crate::app::App;
use crate::completion::CompletionState;
use crate::input::InputBuffer;
use crate::theme::Palette;
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
/// 与 [`BLOCK_GAP_ROWS`] 正交：那条管「**块类型**之间」（行动块 / 正文块），
/// 本条管「**问答**之间」。两条规则叠加（同一处可能各留一次）。
const USER_GAP_ROWS: usize = 2;
/// 条目「类别」变化时保留的**空白行数** —— transcript **内部**的块边界。
///
/// 「类别」见 [`LineClass`]：用户提问 / 行动块（思考 + 工具）/ 正文块（assistant 文本）。
/// 类变了 ⇒ 插一行空白，让「过程」与「产物」在扫视时分得开。
///
/// 与 [`USER_GAP_ROWS`] 正交（那条按**条目类型**、这条按**块类别**）——
/// 「思考 → 工具」同类不留白（一串工具调用是同一个行动块，切碎它反而更难读），
/// 「工具 → 正文」类变了则留白。
const BLOCK_GAP_ROWS: usize = 1;
/// 行动块（思考 + 工具）整体的**缩进格数**。
///
/// 缩进把「过程」推到正文之下，配合降色，使「模型在做什么」与「模型说了什么」可分。
/// **不依赖任何特殊字形** —— 字体缺某个符号时缩进照样成立（对比 `⌁` U+2301 的低覆盖率）。
///
/// 用**空格**而非制表符：制表宽度随终端设置变化，同一份内容在不同终端下会错位。
const ACTION_INDENT: usize = 2;
/// 行动块**内部**再缩进的格数（工具行相对思考行）。
///
/// 层次：思考（[`ACTION_INDENT`]）⊂ 工具（[`ACTION_INDENT`] + [`ACTION_TOOL_INDENT`]）。
const ACTION_TOOL_INDENT: usize = 2;
/// 一条工具调用（含参数摘要）最多占的**视觉行数**。
///
/// 工具行是「过程」的刻度，不是正文 —— 它再长也不该把一串调用摊成半屏。
/// 失败时要另起一行显示结果首行，故那时正文只准占 1 行（两者合计仍 ≤ 2 行）。
const TOOL_MAX_ROWS: usize = 2;
/// 工具行正文与结尾 `✓` / `✗` 之间保留的空格数（含末行的整行右边距）。
const TOOL_MARK_GAP: usize = 2;
/// 窗口小于此尺寸直接不渲染（留给「终端太小」）。
const MIN_WIDTH: u16 = 10;
const MIN_HEIGHT: u16 = 5;

/// 筛选无命中时占位的那行（有候选但都不匹配）。中文全角括号，与既有提示同风格。
const NO_MATCH: &str = "（无匹配）";

/// 筛选框为空时的占位文案 —— **可筛选这件事在这里告诉用户**，不进标题。
///
/// 标题里塞「输入筛选」会把整框撑到 60+ 列（原有标题约 40 列），窄终端下反而被裁掉；
/// 而提示放在输入框里，位置更对：正是用户要敲字的地方。
const FILTER_PLACEHOLDER: &str = "输入以筛选…";
/// 补全浮层的最大宽度（再宽也只是空荡荡的一行）。
const COMPLETION_MAX_WIDTH: u16 = 64;
/// 模态浮层的宽度夹取区间。
const MODAL_MIN_WIDTH: u16 = 24;
const MODAL_MAX_WIDTH: u16 = 72;

/// 当前主题的色板 —— 本模块**唯一**取色入口。
///
/// 主题真值只有一处：`App.theme`（运行期真值，不随 `Outbound::View` 快照覆盖）。
/// chrome（状态行 / 选择器 / 用户前缀 / ✓✗ / 错误前缀 / 系统行）与 markdown 样式
/// （`push_transcript_line`）都从这里或从这里派生的色板取色，故换主题不会漏掉一条路径。
///
/// **按值**：`Palette` 是 `Copy`，且一帧只取一次、一路传下去 —— 同一帧内所有 pane
/// 必然读到同一份色板（否则「先铺底、后画浮层」可能用两套色板）。
fn chrome(app: &App) -> Palette {
    app.theme.palette()
}

/// 色板 → 主题（[`ThemeKind::palette`] 的逆）。
///
/// 渲染整屏。
///
/// 取色在 `&mut app` 借用**之前**完成：`ui_with` 内部要可变借用 `app`
/// （`sync_scroll_state`），那一刻再取色就借不到了。
pub fn ui(frame: &mut Frame, app: &mut App) {
    let palette = chrome(app);
    ui_with(frame, app, palette);
}

/// [`ui`] 的完整形态：显式传入色板。
///
/// 允许单测渲染**任意**主题而不依赖全局状态；生产路径由 [`ui`] / [`ui_with_modal`]
/// 从 `App.theme` 取色传入。
fn ui_with(frame: &mut Frame, app: &mut App, palette: Palette) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return;
    }

    // 整屏背景铺色（D2）：先铺满，再由各 pane 在上面画。
    // 必须在 early-return 之后 —— 终端过小时不画任何东西。
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        frame.area(),
    );

    // 帧时钟：帧内**只有一处**取时刻（`layout` 同步），一路传下去。
    let now = Instant::now();

    // 同步布局（逐条目的折行记忆）并维护滚动状态 ⇒ 得到视窗顶部的全局视觉行号。
    // 滚动数学与画面读**同一份**布局，故不会出现「滚动按新、画面按旧」。
    let scroll = app.sync_scroll_state(area.width, area.height, now);

    let chunks = Layout::vertical([
        Constraint::Min(3),                                // Chat
        Constraint::Length(input_height(app, area.width)), // Input（1..=N 自适应）
        Constraint::Length(1),                             // Status（最底，常驻）
    ])
    .split(area);

    draw_transcript(frame, chunks[0], app, scroll.top_row, palette);
    draw_input(frame, chunks[1], app);
    draw_status(frame, chunks[2], app, palette);

    // 补全浮层**最后画**：Clear 后盖在 Chat 区域上（设计 §1「独立于三个 pane」）。
    // 它不参与上面的布局计算 —— 开合时三个 pane 的位置一个像素都不动。
    if let Some(state) = &app.completion {
        draw_completion(frame, chunks[0], state, palette);
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

/// Chat 内区几何（**宽、高**）—— 滚动数学与渲染共用同一份。
///
/// 需要终端尺寸：wrap 行数依赖宽度、可见行数依赖高度。尺寸太小（[`ui`] 不渲染）时返回 `(0, 0)`。
pub(crate) fn chat_viewport(app: &App, width: u16, height: u16) -> (u16, u16) {
    let area = Rect::new(0, 0, width, height);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return (0, 0);
    }
    let chat = chat_area(app, area);
    let inner = chat_block().inner(chat);
    (inner.width.max(1), inner.height)
}

/// 允许的最大 `scroll_offset`（**视觉行**，0 = 贴底）。
///
/// 需要终端尺寸 —— 因为 wrap 行数依赖宽度、可见行数依赖高度。
///
/// **这是事件路径的入口**（↑↓ / resize 时夹取 `scroll_offset`，见 `events.rs` / `run.rs`）；
/// 该路径没有帧时钟，故这里自取当前时刻。同步布局顺带把滚动状态夹到有效范围
/// （见 [`App::sync_scroll_state`]）。
pub fn max_scroll_offset(app: &mut App, width: u16, height: u16) -> usize {
    max_scroll_offset_at(app, width, height, Instant::now())
}

/// [`max_scroll_offset`] 的内部形态：显式传入 `now` —— 令单测能确定性注入时刻、
/// 不依赖真实时钟（生产的事件路径经 [`max_scroll_offset`] 自取）。
pub fn max_scroll_offset_at(app: &mut App, width: u16, height: u16, now: Instant) -> usize {
    app.sync_scroll_state(width, height, now).max_scroll
}

/// 画对话区 —— **只画视窗覆盖的那些行**（[`crate::layout`]）。
///
/// `top_row` 是视窗顶部的**全局视觉行号**（由 [`App::sync_scroll_state`] 给出），
/// 与滚动数学读的是同一份布局 ⇒ 画面与滚动位置必然一致。
///
/// 视窗切片依赖「整段折行 == 逐行折行之和」（N4）—— 故切片后拼起来与整段折行等价。
fn draw_transcript(frame: &mut Frame, area: Rect, app: &App, top_row: usize, palette: Palette) {
    let block = chat_block();
    let inner = block.inner(area);
    let inner_h = inner.height as usize;

    let window = app.layout.borrow().window(top_row, inner_h);
    // 空 transcript ⇒ 占位一行（与旧实现同形）。
    let lines = if window.lines.is_empty() && app.transcript.is_empty() {
        vec![Line::from(Span::styled(
            "(no messages yet)",
            Style::default().fg(palette.muted),
        ))]
    } else {
        window.lines
    };
    let para = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((window.skip_rows.min(u16::MAX as usize) as u16, 0));
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

fn draw_status(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let text = status_text(app, area.width as usize);
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(palette.muted)),
        area,
    );
}

// ---------------------------------------------------------------------------
// 浮层（补全 / 模态选择器）—— 都是 `Clear` 后盖上去，**不参与布局高度**
// ---------------------------------------------------------------------------

/// 补全浮层：贴 Chat 区底部、盖在对话内容上（设计 §1 mockup）。
///
/// 只画 `visible` 条（超出的一半被窗口裁掉），保证 `selected` 一定可见。
fn draw_completion(frame: &mut Frame, chat: Rect, state: &CompletionState, palette: Palette) {
    if state.items.is_empty() || chat.height < 3 || chat.width < 8 {
        return;
    }
    let visible = state.items.len().min((chat.height - 2) as usize);
    let start = window_start(state.selected, state.items.len(), visible);
    let w = chat.width.min(COMPLETION_MAX_WIDTH);
    let h = visible as u16 + 2;
    let rect = Rect::new(chat.x, chat.bottom() - h, w, h);

    frame.render_widget(Clear, rect);
    // `Clear` 把区域重置为终端默认底色 ⇒ 立即用主题 `bg` 补铺，否则这块是视觉黑洞。
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        rect,
    );
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
    /// 选项选择器（`/login` 选 provider、`/model` 无参选模型、`/skills`、`/resume`）。
    Select {
        prompt: &'a str,
        /// 全量候选（`filtered` 里的下标指向它）。
        options: &'a [String],
        /// 经筛选后**保序**的下标。空查询时即 `0..options.len()`。
        filtered: &'a [usize],
        /// 在 **`filtered`** 内的下标（不是 `options` 的下标 —— 两处语义不同，
        /// 高亮/滚动/计数都用这一个）。
        selected: usize,
        /// 常驻筛选输入框（内容 + 光标）。
        query: &'a InputBuffer,
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
    let palette = chrome(app);
    ui_with(frame, app, palette);
    match modal {
        Modal::Select {
            prompt,
            options,
            filtered,
            selected,
            query,
            page_size,
        } => prompt_select(
            frame, prompt, options, filtered, selected, query, page_size, palette,
        ),
        Modal::Text {
            prompt,
            help,
            input,
        } => prompt_text(frame, prompt, help, input, palette),
    }
}

/// 模态选择器：`Clear` 后居中盖在整屏上 —— **同一套 alt-screen，不切屏、不让出终端**；
/// 外部交互库与「切屏/恢复终端」那套机制按设计 §3 已作废。
///
/// 首行是**常驻筛选输入框**（`> ` 前缀，光标由本函数落位），其下是过滤后的候选；
/// 候选为空（有候选但都不匹配）时画一行「无匹配」—— **不早退**，否则用户看不到
/// 自己敲的查询串，会以为浮层坏了。
///
/// 只画浮层本身；底下的界面由 [`ui_with_modal`] 先画好。
#[allow(clippy::too_many_arguments)]
fn prompt_select(
    frame: &mut Frame,
    prompt: &str,
    options: &[String],
    filtered: &[usize],
    selected: usize,
    query: &InputBuffer,
    page_size: usize,
    palette: Palette,
) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT || options.is_empty() {
        return;
    }
    // 比原来少留一行：筛选行也要占一格。
    let rows = area.height.saturating_sub(3) as usize;
    let visible = filtered.len().min(page_size.max(1)).min(rows).max(1);
    let start = window_start(selected, filtered.len(), visible);

    let hint = "↑↓ 选择 · Enter 确认 · Esc 取消";
    let title = format!(" {prompt} ({hint}) ");
    // 空查询时显示占位文案（光标仍落在 `> ` 之后）。
    let (query_row, query_style) = if query.text.is_empty() {
        (
            format!("> {FILTER_PLACEHOLDER}"),
            Style::default().fg(palette.muted),
        )
    } else {
        (
            format!("> {}", query.text),
            Style::default().fg(palette.code),
        )
    };
    // 宽度按「标题 / 筛选行 / 最长候选」里更宽的那个算 —— 否则标题会被边框裁掉，
    // 用户就看不到「Esc 取消」这类唯一能学会操作的提示。
    let content = filtered
        .iter()
        .map(|i| display_width(&options[*i]))
        .max()
        .unwrap_or(0)
        .max(display_width(&query_row))
        .max(display_width(&title))
        .max(display_width(NO_MATCH));
    let w = modal_width(area, content);
    let h = (visible + 3) as u16; // 边框 2 + 筛选行 1 + 候选
    let rect = centered_rect(area, w, h);

    frame.render_widget(Clear, rect);
    // `Clear` 后补铺主题 `bg`（同 [`draw_completion`]）。
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        rect,
    );
    let inner_w = (w.saturating_sub(2)) as usize;

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        truncate_to_width(&query_row, inner_w),
        query_style,
    ))];
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_to_width(NO_MATCH, inner_w),
            Style::default().fg(palette.muted),
        )));
    } else {
        lines.extend(
            filtered[start..start + visible]
                .iter()
                .enumerate()
                .map(|(i, idx)| {
                    let style = if start + i == selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    Line::from(Span::styled(
                        truncate_to_width(&options[*idx], inner_w),
                        style,
                    ))
                }),
        );
        if filtered.len() > visible {
            lines.push(Line::from(Span::styled(
                format!("… {}/{}", selected + 1, filtered.len()),
                Style::default().fg(palette.muted),
            )));
        }
    }

    let block = Block::bordered().title(title);
    let inner = block.inner(rect);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), rect);

    // 光标落在筛选行（按**显示宽度**定位，CJK 占 2 列 —— 同 `prompt_text`）；
    // 与 `>` 前缀两列相加。
    let col = 2 + display_width(query.text_before_cursor().rsplit('\n').next().unwrap_or(""));
    if col < inner.width as usize {
        frame.set_cursor_position(ratatui::layout::Position::new(
            inner.x + col as u16,
            inner.y,
        ));
    }
}

/// 模态单行输入（`/login` 问 api_key）。
///
/// 光标**由本函数直接落好**（按显示宽度，CJK 占 2 列），调用方不必再管。
/// 只画浮层本身；底下的界面由 [`ui_with_modal`] 先画好。
fn prompt_text(
    frame: &mut Frame,
    prompt: &str,
    help: Option<&str>,
    input: &InputBuffer,
    palette: Palette,
) {
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
    // `Clear` 后补铺主题 `bg`（同 [`draw_completion`]）。
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        rect,
    );
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
///
/// **回合进行中**首段换成 `⏳ Working…`，其后紧跟两个**进度段**（本回合轮号、已用秒数）
/// —— 它们在剥除顺序里排在 tokens/轮数/时长**之前**，故「它在推进」这条信息在窄终端下
/// 也保得住。回合结束后不再显示：那时轮号与耗时的权威形态是 transcript 里的 `Summary` 行。
fn status_text(app: &App, width: usize) -> String {
    let model = if app.is_turning {
        format!("⏳ {}", app.working_text())
    } else {
        app.view.model_label()
    };

    let mut segs = vec![model];
    // 「下方有新内容」提示：脱离底部时才有 —— 排在 model 之后、其余段之前，
    // 于是窄终端下先被剥掉的是 tokens / 轮数 / 时长，提示与 model 都留得住。
    if app.new_content_below {
        segs.push("↓ 有新内容".to_string());
    }
    if app.is_turning {
        if let Some(round) = app.current_round {
            segs.push(format!("第 {round} 轮"));
        }
        segs.push(format!("{:.0}s", app.turn_elapsed_secs()));
    }
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
/// **仅测试用**：它是「整段折行」这条参考路径 —— 视窗化渲染（[`crate::layout`]）
/// 的等价性用例拿它当对照。生产路径不再拼整段文本。
#[cfg(test)]
pub(crate) fn build_text(app: &App, width: usize, now: Instant) -> Text<'static> {
    build_text_with(app, width, app.thinking_visible, now)
}

/// `build_text` 的完整形态：显式传入 thinking 可见性
/// （TUI 与退出回放共用同一个开关值）。
#[cfg(test)]
fn build_text_with(app: &App, width: usize, thinking_visible: bool, now: Instant) -> Text<'static> {
    let palette = chrome(app);
    if app.transcript.is_empty() {
        return Text::from(vec![Line::from(Span::styled(
            "(no messages yet)",
            Style::default().fg(palette.muted),
        ))]);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    // 「上一条的类别」与「已有可见行」共同决定本条之前的留白。
    // 二者都由 `gap_rows` 统一算（两条渲染路径共用同一套规则）。
    let mut prev_class: Option<LineClass> = None;
    let mut has_visible = false;
    let mut cache = app.md_cache.borrow_mut();
    cache.sync_len(app.transcript.len());
    for (idx, item) in app.transcript.iter().enumerate() {
        let gap = gap_rows(item, prev_class, has_visible, thinking_visible);
        // 先算再借：`gap_rows` 要读 `lines` 的当前长度，而紧接着要可变借它。
        lines.extend((0..gap).map(|_| Line::from("")));
        push_transcript_line(
            &mut lines,
            item,
            width,
            thinking_visible,
            palette,
            &mut cache,
            idx,
            now,
        );
        // 不可见的条目（隐藏的思考、空 assistant）不推进状态 ——
        // 它们在屏上不存在，不该切块边界。
        if is_visible(item, thinking_visible) {
            has_visible = true;
            prev_class = Some(LineClass::of(item));
        }
    }
    Text::from(lines)
}

/// 本条之前的**留白行数**（问答边界 + 块边界之和）。
///
/// 两条留白规则**正交**、各自独立：
/// - [`user_gap_rows`]：按**条目类型**（`User` 之前留 [`USER_GAP_ROWS`] 行）
/// - [`block_gap_rows`]：按**块类别**（类变了留 [`BLOCK_GAP_ROWS`] 行）
///
/// 两者可叠加（「用户提问」紧接「正文」时 = 2 + 1 行）。这是有意的：一个是**问答**边界，
/// 一个是**块类型**边界，语义不同、视觉上叠加才完整。
///
/// **本条自己不可见时（隐藏的思考、空 assistant）取 0** —— 它不产出行，
/// 在它之前留白会留下**孤立空行**（无人消费的空白）。且它**不推进**「上一条类别」，
/// 故它后面的条目仍按**前一个可见条目**算边界。
///
/// 只返回**计数**、不代插入（两条渲染路径的目标容器不同：TUI 是 `Vec<Line>`、
/// 退出回放是 `Vec<String>`）。
pub(crate) fn gap_rows(
    item: &TranscriptLine,
    prev_class: Option<LineClass>,
    has_visible_before: bool,
    thinking_visible: bool,
) -> usize {
    if !is_visible(item, thinking_visible) {
        return 0;
    }
    user_gap_rows(item, has_visible_before) + block_gap_rows(item, prev_class, has_visible_before)
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

/// transcript 条目的**块类别** —— 决定缩进、降色与块边界留白。
///
/// 三类：
/// - [`LineClass::Prompt`]：用户提问
/// - [`LineClass::Action`]：**行动块** = 模型思考 + 工具调用（模型的「过程」）
/// - [`LineClass::Content`]：正文块 = assistant 文本（模型的「产物」）
///
/// 「思考」与「工具」同属 `Action` —— 它们是同一件事的两面（想 → 做），
/// 拆成两类会在中间插空白，把一串工具调用切碎。两者在块**内部**的层次差
/// 由 [`ACTION_INDENT`] / [`ACTION_TOOL_INDENT`] 表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineClass {
    Prompt,
    Action,
    Content,
}

impl LineClass {
    /// 条目 → 类别。
    ///
    /// `Summary` / `Error` / `System` 归 [`LineClass::Content`] —— 它们是模型的
    /// **产物或提示**，与 assistant 文本同属「内容」一侧，不该带行动块的缩进。
    pub(crate) fn of(item: &TranscriptLine) -> Self {
        match item {
            TranscriptLine::User(_) => Self::Prompt,
            TranscriptLine::Thinking(_) | TranscriptLine::Tool { .. } => Self::Action,
            TranscriptLine::Assistant(_)
            | TranscriptLine::Summary { .. }
            | TranscriptLine::Error(_)
            | TranscriptLine::System(_) => Self::Content,
        }
    }

    /// 行动块的**缩进格数**。
    ///
    /// 行动块内再分两级：思考 [`ACTION_INDENT`]、工具 [`ACTION_INDENT`] + [`ACTION_TOOL_INDENT`]。
    fn indent(self, item: &TranscriptLine) -> usize {
        match (self, item) {
            (Self::Action, TranscriptLine::Tool { .. }) => ACTION_INDENT + ACTION_TOOL_INDENT,
            (Self::Action, _) => ACTION_INDENT,
            _ => 0,
        }
    }
}

/// 本条目是否产出**可见行** —— 留白规则的可见性判据。
///
/// 两条渲染路径（TUI / 退出回放）**共用**本判据 ⇒ 留白在两边必然一致。
/// 不看「实际产出了几行」而看「内容是否可见」：回放路径的 assistant 走 raw
/// `split('\n')`，空串会产出 1 个空行；而 TUI 路径的 `md::render("")` 产出 0 行。
/// 若用「产出行数」当判据，两条路径会在**空 assistant** 之后给出不同的留白。
///
/// 不可见的条目**不推进**「上一条类别」—— 它在屏上不存在，不该切块边界。
pub(crate) fn is_visible(item: &TranscriptLine, thinking_visible: bool) -> bool {
    match item {
        TranscriptLine::Thinking(s) => thinking_visible && !s.is_empty(),
        TranscriptLine::Assistant(s) => !s.is_empty(),
        _ => true,
    }
}

/// 块类别变化时插入的空白行数 —— 见 [`BLOCK_GAP_ROWS`]。
///
/// **除了**「该条目之前尚无任何可见行」时取 `0`（顶部凭空多行）。
/// 判据与 [`user_gap_rows`] 同源（以「已有可见行」为准，而非下标 > 0）——
/// `/thinking off` 时首条 `Thinking` 不产出行，按下标会把紧接的首条误判成非首条。
///
/// 「上一条的类别」= **上一条可见条目**的类别：不可见的条目（隐藏的思考、空 assistant）
/// 不推进它 —— 它们在屏上不存在，切块边界没有意义。
fn block_gap_rows(
    item: &TranscriptLine,
    prev: Option<LineClass>,
    has_visible_before: bool,
) -> usize {
    if !has_visible_before {
        return 0;
    }
    let Some(prev) = prev else {
        return 0;
    };
    if LineClass::of(item) == prev {
        0
    } else {
        BLOCK_GAP_ROWS
    }
}

/// 一个 transcript 条目 → 它的渲染行（**不含**前置留白）。
///
/// `palette` 由调用方给出（**按值**，`Copy`）：本函数拿不到 `App`，而颜色又是已烘进
/// `Line` 的产物 ⇒ 主题必须由持有真值的一方显式传入，不得在这里重新取默认色板。
///
/// `cache` 由调用方持有（`RenderCache` 的 `RefCell` 守卫不得跨调用带出切片 ——
/// **借用纪律**：`lines_for` 返回的切片生命周期绑在守卫上，必须在守卫作用域内 clone
/// 进目标，不得带出；不重入 `borrow_mut`）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_transcript_line(
    out: &mut Vec<Line<'static>>,
    item: &TranscriptLine,
    width: usize,
    thinking_visible: bool,
    palette: Palette,
    cache: &mut crate::cache::RenderCache,
    idx: usize,
    now: Instant,
) {
    if let TranscriptLine::Assistant(s) = item {
        // assistant markdown 走缓存 + `md::render`。
        // 样式表从**入参色板**构造：换成默认主题会让「切了主题 markdown 还是旧配色」。
        let styles = crate::styles::AssistantStyles::new(palette);
        out.extend(cache.lines_for(idx, s, width, styles, now).iter().cloned());
        return;
    }
    push_non_assistant(out, item, width, thinking_visible, palette);
}

/// 非 `Assistant` 变体 → 行（TUI 与退出回放共用，行为一致）。
///
/// `palette` 按值传入：颜色已烘进 `Line`，故取色必须由持有主题真值的一方给出 ——
/// 本函数拿不到 `App`，若在这里重新取默认色板，切主题后这些行会保持旧配色。
pub(crate) fn push_non_assistant(
    out: &mut Vec<Line<'static>>,
    item: &TranscriptLine,
    width: usize,
    thinking_visible: bool,
    palette: Palette,
) {
    match item {
        TranscriptLine::User(s) => {
            for (i, part) in s.split('\n').enumerate() {
                let prefix = if i == 0 { "> " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(palette.user)),
                    Span::styled(part.to_string(), Style::default().fg(palette.fg)),
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
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC);
            // 缩进由 `LineClass` 决定（唯一真相源）—— 思考与工具同属行动块，
            // 但工具在块内**多缩进**一级（层次：想 ⊂ 做）。
            let indent = " ".repeat(LineClass::of(item).indent(item));
            for (i, part) in s.split('\n').enumerate() {
                let prefix = if i == 0 {
                    format!("{indent}⌁ ")
                } else {
                    format!("{indent}  ")
                };
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
                ("✓", Style::default().fg(palette.ok))
            } else {
                ("✗", Style::default().fg(palette.fail))
            };
            // 行动块整体降色（`muted`）：与顶格的 assistant 正文拉开视觉重量。
            // 标记 `✓` / `✗` 保留自己的 `ok` / `fail` —— 那是**语义**信号（成功与否），
            // 不属于「行动」的类别色。
            let action_style = Style::default().fg(palette.muted);
            let indent = " ".repeat(LineClass::of(item).indent(item));
            // 失败时要另起一行给结果首行 ⇒ 正文只准占 1 行（合计仍 ≤ [`TOOL_MAX_ROWS`]）。
            let show_result = !*success && result.is_some();
            let max_rows = if show_result { 1 } else { TOOL_MAX_ROWS };
            // 行首 `⏺ ` 与续行对齐位按**实际显示宽度**算 —— `⏺` 的宽度随 unicode-width
            // 版本可能是 1 或 2，硬编码列数会在续行错位。
            let head_cols = display_width("⏺ ");
            let budget = width
                .saturating_sub(
                    display_width(&indent) + head_cols + TOOL_MARK_GAP + display_width(mark),
                )
                .max(1);
            // 工具名恒在首行，摘要跟随 —— 二者之间一个空格即可（末尾不留）。
            let text = if summary.is_empty() {
                name.clone()
            } else {
                format!("{name} {summary}")
            };
            let rows = wrap_limited(&text, budget, max_rows);
            let last = rows.len().saturating_sub(1);
            for (i, row) in rows.into_iter().enumerate() {
                // 首行 `⏺ `，续行用等宽空白对齐 —— 缩进与续行对齐都不依赖制表符。
                let prefix = if i == 0 {
                    format!("{indent}⏺ ")
                } else {
                    format!("{indent}{}", " ".repeat(head_cols))
                };
                let mut spans = vec![
                    Span::styled(prefix, action_style),
                    Span::styled(row, action_style),
                ];
                // 标记落在**末行**行尾：两行时它也跟着走，不会停在半截内容后。
                if i == last {
                    spans.push(Span::raw(" ".repeat(TOOL_MARK_GAP)));
                    spans.push(Span::styled(mark, mark_style));
                }
                out.push(Line::from(spans));
            }
            // 成功只给 ✓（结果默认不展开）；**失败**才显示结果首行 —— 「看不到任何结果」是真损失
            if show_result && let Some(body) = result {
                let first = body.lines().next().unwrap_or("");
                let prefix = format!("{indent}  └ ");
                let budget = width.saturating_sub(display_width(&prefix)).max(1);
                out.push(Line::from(Span::styled(
                    format!("{prefix}{}", truncate_to_width(first, budget)),
                    Style::default().fg(palette.fail),
                )));
            }
        }
        TranscriptLine::Summary {
            rounds,
            stop,
            elapsed_secs,
        } => {
            out.push(Line::from(format!(
                "{} {} · {rounds} rounds · {elapsed_secs:.1}s",
                stop_symbol(stop),
                stop_label(stop)
            )));
        }
        TranscriptLine::Error(s) => {
            for (i, part) in s.split('\n').enumerate() {
                let prefix = if i == 0 { "Error: " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(palette.fail)),
                    Span::styled(part.to_string(), Style::default().fg(palette.fg)),
                ]));
            }
        }
        TranscriptLine::System(s) => {
            for part in s.split('\n') {
                out.push(Line::from(Span::styled(
                    part.to_string(),
                    Style::default().fg(palette.muted),
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

/// `StopReason` → 一个稳定的数值键 —— 供条目内容签名用（见 [`crate::layout`]）。
///
/// `StopReason` 没有 `Hash` 派生（且是 `#[non_exhaustive]`），故手工映射；
/// 未知变体一律落 `_` ⇒ 同一个键（它们渲染出的字符也相同）。
pub(crate) fn stop_key(stop: &StopReason) -> u64 {
    match stop {
        StopReason::Completed => 1,
        StopReason::MaxRounds => 2,
        StopReason::Cancelled => 3,
        StopReason::Truncated => 4,
        StopReason::NoOutput => 5,
        _ => 0,
    }
}

/// `StopReason` → 摘要中的可读短标签。
///
/// `StopReason` 是 `#[non_exhaustive]`，下游无法构造未来变体；
/// 通配分支让新增变体至少显示为 `unknown`，不会静默落成错误的原因。
fn stop_label(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::Completed => "completed",
        StopReason::Cancelled => "cancelled",
        StopReason::MaxRounds => "max rounds",
        StopReason::Truncated => "truncated",
        StopReason::NoOutput => "no output",
        _ => "unknown",
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
    // 与 TUI 路径（`build_text_with`）**同一套**留白规则：同一个 `gap_rows`。
    // `prev_class` / `has_visible` 的推进方式也一致（“本条是否产出行”）。
    let mut prev_class: Option<LineClass> = None;
    let mut has_visible = false;
    for item in &app.transcript {
        let gap = gap_rows(item, prev_class, has_visible, app.thinking_visible);
        if let TranscriptLine::Assistant(s) = item {
            // assistant 走 raw 路径（`\n` 原样切行、不经 md 渲染）——
            // 与 TUI 内不同形，这是**有意**的（退出后主屏留原始 markdown 源）。
            // 但留白与缩进仍与 TUI 一致，故**先**落留白行、再落内容。
            out.extend(std::iter::repeat_n(String::new(), gap));
            out.extend(s.split('\n').map(str::to_string));
        } else {
            // 问答边界与块边界均与 TUI 内一致：空白行必须在 `wrap_plain` **之前**落到 `out` 上
            // （空行经 wrap 仍是 1 行空串，但顺序必须是「先空行、后内容」）。
            out.extend(std::iter::repeat_n(String::new(), gap));
            let mut lines: Vec<Line<'static>> = Vec::new();
            push_non_assistant(&mut lines, item, width, app.thinking_visible, chrome(app));
            for line in &lines {
                out.extend(wrap_plain(&line.to_string(), width));
            }
        }
        // 与 TUI 路径**同一个**可见性判据 ⇒ 留白在两边必然一致。
        if is_visible(item, app.thinking_visible) {
            has_visible = true;
            prev_class = Some(LineClass::of(item));
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
    let mut out = take_width(s, max.saturating_sub(1));
    out.push('…');
    out
}

/// 取前缀，直取到再加一个字符就超 `budget` 列为止（不补省略号）。
fn take_width(s: &str, budget: usize) -> String {
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
    out
}

/// 按显示宽度贪心折行，**最多** `max_rows` 行；溢出的内容在末行以 `…` 收尾。
///
/// 工具行用它（[`TOOL_MAX_ROWS`]）：工具调用是「过程」的刻度，不是正文 ——
/// 一条长 `bash` 命令不该把一串调用摊满半屏。**多出的部分只截不续**是有意的：
/// 那一行的信息量远低于它占的垂直空间，全文在工具结果与日志里。
///
/// 优先在**空格**处断行（`bash` 命令、多词参数读起来不断词），
/// 单个词本身超宽（路径、URL、中文）才按列硬断 —— CJK 没有空格，只能硬断。
fn wrap_limited(s: &str, width: usize, max_rows: usize) -> Vec<String> {
    let width = width.max(1);
    let max_rows = max_rows.max(1);
    let mut rows = wrap_words(s, width);
    if rows.len() <= max_rows {
        return rows;
    }
    rows.truncate(max_rows);
    if let Some(last) = rows.last_mut() {
        let mut kept = take_width(last, width.saturating_sub(1));
        kept.push('…');
        *last = kept;
    }
    rows
}

/// 按显示宽度折行：贪心装词，词间在空格处断；词本身超宽则按列硬断。
fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for token in s.split_inclusive(' ') {
        let tw = display_width(token);
        if cur_w + tw <= width {
            cur.push_str(token);
            cur_w += tw;
            continue;
        }
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
            // 断行处的空格不带到下一行行首（行首空白既无信息又占列）。
            if token.trim_end().is_empty() {
                continue;
            }
        }
        if tw <= width {
            cur.push_str(token);
            cur_w = tw;
            continue;
        }
        // 词本身放不下一行：先把它硬断到当前行，剩下的继续累积。
        for ch in token.chars() {
            let cw = display_width(&ch.to_string());
            if cur_w + cw > width && !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
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
    use crate::theme::ThemeKind;
    use crate::view::CodingView;
    use ratatui::Terminal;
    use ratatui::backend::Backend;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
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

    /// **视窗化渲染 ≡ 整段渲染**（逐行相同）—— 视窗化的核心安全网。
    ///
    /// 视窗渲染只取视窗覆盖的那些行再折行；整段渲染把整份 transcript 折一遍再滚。
    /// 两者必须**逐行相同** —— 否则「滚动后画面错位」这类问题会悄悄回来。
    ///
    /// 依赖的不变量是「整段折行 == 逐行折行之和」（N4）。
    ///
    /// 覆盖：多个宽度（含 CJK 与表格）、多个滚动位置（含 0 / 中间 / 贴底）、
    /// thinking 可见与隐藏、空 transcript。
    #[test]
    fn test_windowed_render_matches_full_render() {
        for (w, h) in [(40u16, 20u16), (80, 30), (120, 40)] {
            for thinking_visible in [false, true] {
                let mut app = app_with(CodingView::for_test());
                app.thinking_visible = thinking_visible;
                app.transcript
                    .push(TranscriptLine::User("问题：这段代码怎么读？".into()));
                app.transcript.push(TranscriptLine::Assistant(
                    "# 标题\n\n这是 **粗体** 与 `code` 的一段中文正文。\n\n- 要点一\n- 要点二\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n".into(),
                ));
                app.transcript
                    .push(TranscriptLine::Thinking("先想想再答。".into()));
                app.transcript.push(TranscriptLine::User("继续".into()));
                app.transcript.push(TranscriptLine::Assistant(
                    "| 名称 | 值 |\n| -- | -- |\n| 中文 | 数字 |\n".into(),
                ));

                let chat = chat_area(&app, Rect::new(0, 0, w, h));
                let inner = chat_block().inner(chat);
                let inner_w = inner.width.max(1);
                let inner_h = inner.height as usize;
                let total = Paragraph::new(build_text(&app, inner_w as usize, Instant::now()))
                    .wrap(Wrap { trim: false })
                    .line_count(inner_w);
                let max_scroll = total.saturating_sub(inner_h);

                // 逐个滚动位置比对（贴底 + 中间 + 顶）。
                let offsets: Vec<usize> = (0..=max_scroll.min(6))
                    .chain([max_scroll / 2, max_scroll])
                    .filter(|&o| o <= max_scroll)
                    .collect();
                for offset in offsets {
                    app.follow = false;
                    app.scroll_offset = offset;
                    let windowed = chat_inner_rows(&rows_of(&mut app, w, h), &app, w, h);
                    let full = chat_rows_full(&app, w, h, total.saturating_sub(inner_h + offset));
                    assert_eq!(
                        windowed, full,
                        "w={w} h={h} thinking={thinking_visible} offset={offset}：视窗渲染与整段渲染必须逐行相同"
                    );
                }
            }
        }
    }

    /// 视窗**有界**：送进 `Paragraph` 的行数不超过视窗高度 + 1 —— 无论 transcript 多长。
    ///
    /// 这是「滚动不再随会话长度变慢」的**结构性**断言（不依赖计时）：
    /// 视窗化之后，渲染只碰视窗覆盖的那些行。
    #[test]
    fn test_window_is_bounded_by_viewport() {
        let mut app = app_with(CodingView::for_test());
        for i in 0..500 {
            app.transcript.push(TranscriptLine::Assistant(
                format!("第 {i} 条回答。").repeat(20),
            ));
        }
        let (w, h) = (100u16, 40u16);
        let now = Instant::now();
        let state = app.sync_scroll_state(w, h, now);
        assert!(state.max_scroll > 100, "前置：内容远长于一屏");

        // 取若干个滚动位置，每次视窗的行数都必须有界。
        for offset in [0usize, 1, 50, state.max_scroll / 2, state.max_scroll] {
            app.follow = false;
            app.scroll_offset = offset;
            let inner_h = chat_block()
                .inner(chat_area(&app, Rect::new(0, 0, w, h)))
                .height as usize;
            let window = app
                .layout
                .borrow()
                .window(state.max_scroll.saturating_sub(offset), inner_h);
            assert!(
                window.lines.len() <= inner_h + 1,
                "offset={offset}：视窗行数 {} 超过视窗高度 {inner_h} + 1",
                window.lines.len()
            );
        }
    }

    /// Chat 内区的行（从整屏字符画里裁出来）。
    fn chat_inner_rows(rows: &[String], app: &App, w: u16, h: u16) -> Vec<String> {
        let chat = chat_area(app, Rect::new(0, 0, w, h));
        let inner = chat_block().inner(chat);
        (inner.y..inner.bottom())
            .map(|y| rows[y as usize].clone())
            .collect()
    }

    /// **整段渲染**这条参考路径（旧实现）：整份 transcript 折一遍，再按 `top_row` 滚。
    ///
    /// 它只渲染 Chat **内区**（外框的标题与边框不在内区里，不影响逐行比对）。
    fn chat_rows_full(app: &App, w: u16, h: u16, top_row: usize) -> Vec<String> {
        let chat = chat_area(app, Rect::new(0, 0, w, h));
        let inner = chat_block().inner(chat);
        let inner_w = inner.width.max(1);
        let text = build_text(app, inner_w as usize, Instant::now());
        let backend = TestBackend::new(inner.width, inner.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let para = Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((top_row.min(u16::MAX as usize) as u16, 0));
                f.render_widget(para, f.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area().width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
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
            let got = max_scroll_offset(&mut a, W, H);
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
        let max = max_scroll_offset(&mut app, W, H);
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

    /// 留白契约：**问答边界**（`User` 之前）与**块边界**（类别变化时）**叠加**。
    ///
    /// - `q1`（首条可见）两条规则都不生效 ⇒ 0 行空白。
    /// - `a1` 之前：`Content` → `Content`，类未变 ⇒ 0 行（块边界不生效）。
    /// - `q2` 之前：`Content` → `Prompt`，类变了 ⇒ 块边界 1 行 + 问答边界 2 行 = **3 行**。
    /// - `q3` 之前：`Prompt` → `Prompt`，类未变 ⇒ 只问答边界 2 行 = **2 行**。
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
            vec!["> q1", "", "a1", "", "", "", "> q2", "", "", "> q3"],
            "q1 首条不留白；q2 前 3 行（块边界 1 + 问答 2）；q3 前 2 行（同类，仅问答边界）"
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
            max_scroll_offset(&mut app, W, H),
            58 - inner.height as usize,
            "滚动上限必须含空白行（不含则为 0：20 − 34）"
        );
    }

    /// 屏上形状（TUI 渲染路径，不只是 `build_text` 的数据）：第 2 条用户行上方真的空 3 行。
    ///
    /// 3 = 块边界 1（`Content` → `Prompt`，类别变了）+ 问答边界 2（`User` 之前）。
    /// 两条规则**叠加**，屏上必须看得见叠加后的总数。
    #[test]
    fn test_rendered_screen_shows_blank_rows_above_second_user_line() {
        const W: u16 = 40;
        // 内容高 7 行（`> q1` / 空 / `a1` / 空×3 / `> q2`）+ Chat 边框与底部留白
        // ⇒ 内区需 ≥ 7 行，否则 `follow` 贴底会把 `> q1` 滚出视野。
        const H: u16 = 16;
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
        // 从 `a1` 所在行往下数：紧邻 3 行必须空白（块边界 + 问答边界），第 4 行才是新的用户行。
        let a1 = rows
            .iter()
            .position(|r| r.contains("a1"))
            .expect("assistant 行应可见");
        let gap = USER_GAP_ROWS + BLOCK_GAP_ROWS;
        for (i, row) in rows[a1 + 1..a1 + 1 + gap].iter().enumerate() {
            assert!(row.trim().is_empty(), "第 {} 行空白被占：{row:?}", i + 1);
        }
        assert!(
            rows[a1 + 1 + gap].contains("> q2"),
            "第 2 条用户行应在 {gap} 行空白之后：{:?}",
            rows[a1 + 1 + gap]
        );
    }

    /// 短摘要：一行搞定，结果不展开（成功只给 ✓）。
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

    /// 工具行**占满可用宽度**：宽屏下看到远多于摘要护栏的内容，仍是一行。
    ///
    /// **变异**：把正文宽度改回写死的常量（如摘要先截到 40 字符）⇒ 本用例红。
    #[test]
    fn test_tool_call_uses_available_width() {
        let mut app = app_with(CodingView::for_test());
        let long = "src/very/long/path/to/some/deeply/nested/module/file.rs";
        assert!(long.chars().count() > 40, "前提：长于旧的硬上限");
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            summary: long.into(),
            result: None,
            success: true,
        });
        let rows = rows_of(&mut app, 120, 40);
        let tool: Vec<&String> = rows.iter().filter(|r| r.contains('⏺')).collect();
        assert_eq!(tool.len(), 1, "短于宽度时只占一行：{rows:?}");
        assert!(
            tool[0].contains(long),
            "宽屏下应完整显示摘要：{:?}",
            tool[0]
        );
        assert!(display_width(tool[0]) <= 120, "不得超出行宽：{:?}", tool[0]);
    }

    /// 超长摘要：**最多两行**，第二行以 `…` 收尾，`✓` 跟着落在末行。
    ///
    /// **变异**：`TOOL_MAX_ROWS` 改回不限（或 `wrap_limited` 忽略 `max_rows`）⇒ 行数红；
    /// 标记固定在首行 ⇒ 「末行带 ✓」红。
    #[test]
    fn test_tool_call_wraps_to_at_most_two_lines() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "bash".into(),
            summary: "cargo test --workspace --all-features --no-fail-fast -- --nocapture "
                .repeat(6),
            result: None,
            success: true,
        });
        let rows = rows_of(&mut app, 60, 40);
        let tool: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains('⏺') || r.contains('✓'))
            .collect();
        assert_eq!(tool.len(), TOOL_MAX_ROWS, "最多两行：{rows:?}");
        let last = tool.last().unwrap();
        assert!(last.contains('✓'), "标记落在末行：{last:?}");
        assert!(last.contains('…'), "被截断的末行补省略号：{last:?}");
        for row in &tool {
            assert!(display_width(row) <= 60, "折行后每行仍不超宽：{row:?}");
        }
    }

    /// 失败时：正文只占 1 行（给结果首行留位），合计仍 ≤ [`TOOL_MAX_ROWS`]。
    #[test]
    fn test_failed_tool_wraps_to_one_line_plus_result() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "bash".into(),
            summary: "cargo test --workspace --all-features --no-fail-fast ".repeat(4),
            result: Some("error: could not compile".into()),
            success: false,
        });
        let rows = rows_of(&mut app, 60, 40);
        // 只数**内容行**：`└` 也出现在 Input 边框上，故按行首缩进分辨。
        let tool: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains('⏺') || r.trim_start().starts_with("└ "))
            .collect();
        assert_eq!(tool.len(), TOOL_MAX_ROWS, "摘要 1 行 + 结果 1 行：{rows:?}");
        assert!(tool[0].contains('✗'), "标记在摘要行：{:?}", tool[0]);
        assert!(
            tool[1].contains("└ error: could not compile"),
            "结果首行：{:?}",
            tool[1]
        );
    }

    /// 摘要里的换行被抹平 —— 一条工具调用不能占掉多行。
    #[test]
    fn test_tool_summary_has_no_embedded_newlines() {
        let s = crate::transcript::summarize_tool_args(
            "bash",
            &serde_json::json!({ "command": "line1\nline2\tline3" }),
        );
        assert_eq!(s, "line1 line2 line3");
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

    /// 行动块（思考 + 工具）与正文块的**块边界**留白。
    ///
    /// 类别序列 `Prompt → Action → Action → Content` ⇒ 留白：
    /// - `Prompt` 之后（→ `Action`）：类变了 ⇒ 1 行块边界
    /// - `Action` → `Action`（思考 → 工具）：同类 ⇒ **0 行**（一串工具调用是同一行动块）
    /// - `Action` → `Content`（工具 → 正文）：类变了 ⇒ 1 行块边界
    ///
    /// **变异**：`block_gap_rows` 恒返回 0（去掉块边界）⇒ 本用例红；
    /// 把 `Thinking` 与 `Tool` 分成不同类（`Action` 拆两类）⇒ 思考与工具之间多出空行 ⇒ 本用例红。
    #[test]
    fn test_block_gap_only_between_different_classes() {
        let mut app = app_with(CodingView::for_test());
        // `for_test` 默认隐藏思考（ADR-0025）⇒ 本用例需显式开启，否则看不到思考行。
        app.thinking_visible = true;
        app.transcript.push(TranscriptLine::User("q1".into()));
        app.transcript.push(TranscriptLine::Thinking("思考".into()));
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            summary: "src/x.rs".into(),
            result: None,
            success: true,
        });
        app.transcript.push(TranscriptLine::Assistant("a1".into()));

        let lines = transcript_plain_lines(&app, 80);
        // 思考与工具**之间无空行**（同一行动块）。
        let think = lines
            .iter()
            .position(|l| l.contains("思考"))
            .expect("思考行应可见");
        let tool = lines
            .iter()
            .position(|l| l.contains("⏺ read"))
            .expect("工具行应可见");
        assert_eq!(tool, think + 1, "思考与工具同属行动块，不留白：{lines:?}");
        // 工具与正文**之间**留 1 行块边界。
        let body = lines
            .iter()
            .position(|l| l.contains("a1"))
            .expect("正文行应可见");
        assert_eq!(body, tool + 2, "工具 → 正文：1 行块边界：{lines:?}");
    }

    /// 行动块的**缩进**：思考缩进 2 格、工具缩进 4 格（层次：想 ⊂ 做）。
    ///
    /// 缩进用**空格**而非制表符 —— 制表宽度随终端设置变化。
    ///
    /// **变异**：`LineClass::indent` 恒返回 0（去掉缩进）⇒ 本用例红；
    /// 工具缩进改成与思考相同（层次塌陷）⇒ 本用例红。
    #[test]
    fn test_action_block_indent_levels() {
        let mut app = app_with(CodingView::for_test());
        app.thinking_visible = true;
        app.transcript.push(TranscriptLine::Thinking("思考".into()));
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            summary: "src/x.rs".into(),
            result: None,
            success: true,
        });

        let lines = transcript_plain_lines(&app, 80);
        let think = &lines[0];
        let tool = &lines[1];
        assert!(
            think.starts_with(&" ".repeat(ACTION_INDENT)),
            "思考缩进 {ACTION_INDENT} 格：{think:?}"
        );
        assert!(
            tool.starts_with(&" ".repeat(ACTION_INDENT + ACTION_TOOL_INDENT)),
            "工具缩进 {} 格：{tool:?}",
            ACTION_INDENT + ACTION_TOOL_INDENT
        );
        // 缩进用空格 —— 断言不含制表符。
        assert!(
            !think.contains('\t') && !tool.contains('\t'),
            "缩进不得用制表符"
        );
    }

    /// 两条渲染路径的**留白必须一致** —— 否则退出回放与 TUI 内形状不同。
    ///
    /// 退出回放（`transcript_plain_lines`）与 TUI（`build_text_with`）共用 `gap_rows`，
    /// 本用例钉住二者**行数**相同（不比较内容：assistant 在回放走 raw、TUI 走 md 渲染，
    /// 形状本就有意不同）。
    ///
    /// **变异**：只在一侧加块边界留白（两条路径分叉）⇒ 本用例红。
    #[test]
    fn test_both_render_paths_agree_on_gap_rows() {
        let mut app = app_with(CodingView::for_test());
        app.thinking_visible = true;
        app.transcript.push(TranscriptLine::User("q1".into()));
        app.transcript.push(TranscriptLine::Thinking("思考".into()));
        app.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            summary: "src/x.rs".into(),
            result: None,
            success: true,
        });
        app.transcript.push(TranscriptLine::Assistant("a1".into()));
        app.transcript.push(TranscriptLine::User("q2".into()));

        let plain = transcript_plain_lines(&app, 80);
        let tui = build_text_with(&app, 80, true, Instant::now());
        // `build_text_with` 的行**已是** wrap 后的视觉行（`md::render` 内部折行完毕），
        // 故直接取行数，**不得**再乘一次折行系数。
        assert_eq!(
            plain.len(),
            tui.lines.len(),
            "两条路径行数应一致：plain={} tui={}",
            plain.len(),
            tui.lines.len()
        );
    }

    /// 隐藏的思考**不推进**块边界、也**不留下孤立空行** —— 它在屏上不存在。
    ///
    /// `/thinking off` 时 `Thinking` 不产出行：
    /// - 若它仍推进「上一条类别」⇒ `User` →（隐藏思考）→ `Assistant` 被切成两块，多出空行；
    /// - 若它自己之前仍插空白 ⇒ 留下**无人消费的孤立空行**。
    ///
    /// 正确形状：`> q1` / 空（块边界 1）/ `a1` —— `a1` 在**第 3 行**（下标 2）。
    ///
    /// **变异**：`is_visible` 对 `Thinking` 恒返回 true（不看 `thinking_visible`）⇒ 本用例红；
    /// `gap_rows` 不对不可见条目返回 0（留白照插）⇒ 本用例红。
    #[test]
    fn test_hidden_thinking_does_not_shift_block_boundary() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("q1".into()));
        app.transcript.push(TranscriptLine::Thinking("思考".into()));
        app.transcript.push(TranscriptLine::Assistant("a1".into()));

        let hidden = transcript_plain_lines(&app, 80);
        assert!(
            !hidden.iter().any(|l| l.contains("思考")),
            "思考隐藏时不应产出：{hidden:?}"
        );
        // `User` →（隐藏）→ `Assistant`：assistant 与 user **不同类** ⇒ 恰好 1 行块边界。
        // 隐藏的思考**不额外**制造第二个块边界，也**不**留下自己的空行。
        assert_eq!(
            hidden,
            vec!["> q1", "", "a1"],
            "隐藏思考不应留下孤立空行、也不应多切一块：{hidden:?}"
        );
    }

    #[test]
    fn test_summary_symbols_per_stop_reason() {
        for (reason, sym, label) in [
            (StopReason::Completed, "✓", "completed"),
            (StopReason::MaxRounds, "⚠", "max rounds"),
            (StopReason::Cancelled, "✗", "cancelled"),
            (StopReason::Truncated, "✂", "truncated"),
            (StopReason::NoOutput, "∅", "no output"),
        ] {
            let mut app = app_with(CodingView::for_test());
            app.transcript.push(TranscriptLine::Summary {
                rounds: 2,
                stop: reason.clone(),
                elapsed_secs: 1.3,
            });
            let joined = rows_of(&mut app, 120, 40).join("\n");
            assert!(
                joined.contains(&format!("{sym} {label} · 2 rounds · 1.3s")),
                "{reason:?} 应渲染符号 {sym} 与标签 {label}：{joined}"
            );
        }
    }

    #[test]
    fn test_summary_render_matches_plain_transcript() {
        let expected = "✗ cancelled · 2 rounds · 1.3s";
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::Summary {
            rounds: 2,
            stop: StopReason::Cancelled,
            elapsed_secs: 1.3,
        });

        let tui = rows_of(&mut app, 120, 40).join("\n");
        let plain = transcript_plain_lines(&app, 120);
        assert!(tui.contains(expected), "TUI Summary 应含完整标签：{tui}");
        assert_eq!(plain, vec![expected], "退出回放应与 TUI 使用同一格式");
    }

    #[test]
    fn test_summary_wraps_without_overlap_at_min_width() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("q".into()));
        app.transcript.push(TranscriptLine::Summary {
            rounds: 2,
            stop: StopReason::Cancelled,
            elapsed_secs: 1.3,
        });

        let rows = rows_of(&mut app, MIN_WIDTH, 20);
        let input_row = rows
            .iter()
            .position(|row| row.contains("Input"))
            .expect("窄宽度下仍应渲染输入区");
        let chat = rows[..input_row].join("\n");
        assert!(chat.contains('✗'), "Summary 符号应可见：{rows:#?}");
        assert!(
            chat.contains("rounds"),
            "Summary 应在窄宽度下自动换行：{rows:#?}"
        );
        assert!(chat.contains("1.3s"), "Summary 耗时应可见：{rows:#?}");
        assert!(
            !rows[input_row..]
                .iter()
                .any(|row| row.contains('✗') || row.contains("rounds") || row.contains("1.3s")),
            "Summary 不得与本条之后的输入区或状态行重叠：{rows:#?}"
        );
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

    /// 回合进行中：`⏳ Working…` 之后补两个**进度段**（本回合轮号、已用秒数）。
    ///
    /// 这是回合内唯一的进度刻度 —— 在此之前，一个跑 26 轮、六分钟的回合在界面上
    /// 只有一个不动的动画。
    ///
    /// 变异：① 把进度段从 `segs` 里去掉 ⇒ 前两条断言变红；② 把进度段插到 `segs` **末尾**
    /// ⇒ 剥段断言变红（窄屏会把轮号连同时长一起剥掉，而这正是最该留下的信息）。
    #[test]
    fn test_status_shows_round_and_elapsed_during_turn() {
        let mut app = app_with(CodingView::for_test());
        app.begin_local_turn("q".into());
        app.current_round = Some(3);

        let full = status_text(&app, 200);
        assert!(full.contains("Working"), "回合中首段是 Working：{full}");
        assert!(full.contains("第 3 轮"), "回合中要报轮号：{full}");
        assert!(
            !full.contains("deepseek-chat"),
            "回合中不显示 model（被 Working 段替换）：{full}"
        );

        // 剥除顺序：秒数先于轮号被剥掉 ⇒ 宽度只够 [Working.. · 第 3 轮] 时，
        // 「跑到第几轮」这条信息保得住。
        let prefix = format!("⏳ {} · 第 3 轮", app.working_text());
        assert_eq!(
            status_text(&app, display_width(&prefix)),
            prefix,
            "秒数先被剥、轮号留住"
        );
    }

    /// 回合结束后状态行不残留轮号 / 秒数 —— 那些信息的权威形态是 transcript 的 Summary 行。
    ///
    /// 变异：把 `status_text` 里 `if app.is_turning` 的闸去掉 ⇒ 本用例变红。
    #[test]
    fn test_status_has_no_progress_segments_outside_turn() {
        let mut app = app_with(CodingView::for_test());
        app.current_round = Some(5);

        let txt = status_text(&app, 200);
        assert!(!txt.contains("第 5 轮"), "回合外不得显示轮号：{txt}");
        assert!(txt.contains("deepseek-chat"), "{txt}");
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
            let _ = max_scroll_offset(&mut app, 40, h);
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
            max_scroll_offset_at(&mut app, W, H, Instant::now()),
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
            max_scroll_offset_at(&mut app, 40, 40, t1),
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
        let filtered = vec![0usize, 1];
        let query = InputBuffer::new();
        let (rows, reversed) = render(60, 20, |f| {
            prompt_select(
                f,
                "Select provider",
                &options,
                &filtered,
                1,
                &query,
                8,
                ThemeKind::default().palette(),
            )
        });
        let joined = rows.join("\n");
        assert!(joined.contains("Select provider"), "{joined}");
        assert!(joined.contains("deepseek"), "{joined}");
        assert!(joined.contains("openai"), "{joined}");
        assert!(joined.contains("Esc"), "应有取消提示：{joined}");
        // CJK 是宽字符，占两格 ⇒ 行转字符串时第二格为空串，逐格拼出来是「输 入 以…」。
        // 断言中文前先把空格去掉（ASCII 断言不受影响，仍按原样）。
        assert!(
            joined.replace(' ', "").contains("输入以筛选"),
            "空查询时应显示占位文案（可筛选这件事在这里告诉用户）：{joined}"
        );

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

    /// 只有**过滤后**的候选被画出来，且高亮按 `filtered` 内的下标算 ——
    /// 这是「高亮在 A、Enter 选中 B」错位的唯一防线（两处下标语义不同）。
    ///
    /// 变异：把 `options[*idx]` 换回 `options[i]` ⇒ 画出的是原列表的前两项
    /// （`deepseek` / `opencode-go`）而不是被筛中的 ⇒ 变红。
    #[test]
    fn test_prompt_select_draws_only_filtered_rows() {
        let options = vec![
            "deepseek/deepseek-chat".to_string(),
            "opencode-go/deepseek-flash".to_string(),
            "minimax/MiniMax-M3".to_string(),
        ];
        let filtered = vec![2usize]; // 只筛中 minimax 那条
        let query = InputBuffer::new();
        let (rows, reversed) = render(60, 20, |f| {
            prompt_select(
                f,
                "Select model",
                &options,
                &filtered,
                0,
                &query,
                8,
                ThemeKind::default().palette(),
            )
        });
        let joined = rows.join("\n");
        assert!(joined.contains("minimax/MiniMax-M3"), "{joined}");
        assert!(
            !joined.contains("deepseek/deepseek-chat"),
            "未筛中的不该画：{joined}"
        );
        assert!(!joined.contains("opencode-go"), "未筛中的不该画：{joined}");

        let highlighted: Vec<&String> = rows
            .iter()
            .zip(&reversed)
            .filter(|(_, rev)| **rev)
            .map(|(t, _)| t)
            .collect();
        assert_eq!(highlighted.len(), 1, "只高亮一项");
        assert!(highlighted[0].contains("minimax"), "{highlighted:?}");
    }

    /// 筛选行画出查询串；**无匹配时画「无匹配」而不是空框** —— 否则用户看不到自己
    /// 敲了什么，会以为浮层坏了。
    ///
    /// 变异：`if filtered.is_empty()` 分支删掉（直接画空列表）⇒ 变红。
    #[test]
    fn test_prompt_select_shows_query_and_no_match_line() {
        let options = vec!["deepseek".to_string()];

        let mut query = InputBuffer::new();
        query.insert_str("mini");
        let (rows, _) = render(60, 20, |f| {
            prompt_select(
                f,
                "Select model",
                &options,
                &[],
                0,
                &query,
                8,
                ThemeKind::default().palette(),
            )
        });
        let joined = rows.join("\n");
        // CJK 宽字符的空格问题同上一个用例。
        let dense = joined.replace(' ', "");
        assert!(joined.contains("> mini"), "筛选行应显示查询串：{joined}");
        assert!(dense.contains("无匹配"), "{joined}");
        assert!(
            !dense.contains("输入以筛选"),
            "有查询串时不该再显示占位：{joined}"
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
                .draw(|f| {
                    prompt_text(
                        f,
                        "API key",
                        Some("deepseek"),
                        &buf,
                        ThemeKind::default().palette(),
                    )
                })
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
        let filtered = vec![0usize, 1];
        let query = InputBuffer::new();

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
                            filtered: &filtered,
                            selected: 0,
                            query: &query,
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

    // -----------------------------------------------------------------------
    // 主题：整屏背景铺色与 `Clear` 之后的补铺（T5b）
    // -----------------------------------------------------------------------

    /// 渲染一帧 `draw::ui`，返回**整块**缓冲（含每个格子的样式）。
    fn buffer_of(app: &mut App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// 该格子是否是**宽字符的尾随列**。
    ///
    /// ratatui 写宽字符时会把紧随其后的列 `reset()` 回 `Color::Reset`（`Buffer::set_stringn`），
    /// 因为那一列在终端上被前一个字符覆盖、底色看不见。这不是背景铺色的漏点，也无法补铺
    /// —— 故逐格断言时跳过它，其余格子一律必须等于主题 `bg`。
    fn is_wide_trailing(buf: &Buffer, i: usize) -> bool {
        let width = buf.area().width as usize;
        if width == 0 || i.is_multiple_of(width) {
            return false;
        }
        Span::raw(buf.content()[i - 1].symbol()).width() > 1
    }

    /// 缓冲的每个格子都必须铺上主题 `bg`（宽字符尾随列除外，见 [`is_wide_trailing`]）。
    ///
    /// 逐格比对（而不是只抽查几处）：`bg` 泄漏可以出现在任意一个没被 widget 覆盖的格子上，
    /// 抽查漏掉恰好就是那些边角。
    fn assert_screen_bg(buf: &Buffer, palette: Palette, ctx: &str) {
        for (i, cell) in buf.content().iter().enumerate() {
            if is_wide_trailing(buf, i) {
                continue;
            }
            let (x, y) = buf.pos_of(i);
            assert_eq!(
                cell.bg, palette.bg,
                "{ctx}：格子 ({x},{y}) 的 bg 不是主题 bg（{:?}）",
                palette.bg
            );
        }
    }

    /// **整屏背景铺色**（D2）：含顶边框行、输入框内区、状态行在内的**每个**格子都是 `bg`。
    ///
    /// 变异：删掉 `ui()` 里的整屏铺色 ⇒ 未被 pane 着色的格子回落 `Color::Reset` ⇒ 红。
    #[test]
    fn test_ui_paints_theme_bg_everywhere() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        app.transcript
            .push(TranscriptLine::Assistant("hi there **bold**".into()));
        let palette = ThemeKind::default().palette();

        // 多尺寸：小（刚过 MIN_*）、常规、宽 —— 边角与输入框内区都覆盖到。
        for (w, h) in [(MIN_WIDTH, MIN_HEIGHT), (80, 24), (120, 40)] {
            let buf = buffer_of(&mut app, w, h);
            assert_screen_bg(&buf, palette, &format!("ui {w}x{h}"));
        }
    }

    /// **`Clear` 之后必须补铺背景**：模态浮层区域内仍是主题 `bg`（不是 ``Color::Reset``）。
    ///
    /// `Clear` 把区域重置为终端默认底色，不补铺就会出现与主题背景不一致的矩形
    /// —— 屏上一个视觉黑洞。变异：删掉 `prompt_select` 里 `Clear` 之后的补铺 ⇒ 红。
    #[test]
    fn test_modal_overlay_uses_theme_bg_not_default() {
        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));
        let palette = ThemeKind::default().palette();
        let options: Vec<String> = (0..6).map(|i| format!("option-{i}")).collect();
        let filtered: Vec<usize> = (0..options.len()).collect();
        let query = crate::input::InputBuffer::new();

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                ui_with_modal(
                    f,
                    &mut app,
                    Modal::Select {
                        prompt: "模型",
                        options: &options,
                        filtered: &filtered,
                        selected: 0,
                        query: &query,
                        page_size: 5,
                    },
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // 整屏仍是主题 bg（浮层区域也包括在内 —— 这就是「不黑洞」的判据）。
        assert_screen_bg(&buf, palette, "ui_with_modal Select");
        // 前置：浮层真的画出来了（否则上面的断言对“没有浮层”也成立）。
        let joined: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(joined.contains("option-0"), "浮层应列出候选：{joined}");
    }

    /// **切主题整屏换色**：同一个 `App` 走 `set_theme` 后整屏 `bg` 跟着换。
    ///
    /// 走生产路径（`set_theme` + [`ui`]，而非直接传色给 [`ui_with`]）—— 钉的正是
    /// 「`ui` 从 `App.theme` 取色」：漏接 `chrome(app)` 就恒为默认主题，本用例必红。
    #[test]
    fn test_theme_switch_repaints_whole_screen_bg() {
        let palette_a = ThemeKind::default().palette();
        let palette_b = ThemeKind::Nord.palette();
        assert_ne!(palette_a.bg, palette_b.bg, "前置：两套主题的 bg 必须不同");

        let mut app = app_with(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("hello".into()));

        let buf_a = buffer_of(&mut app, 80, 24);
        assert_screen_bg(&buf_a, palette_a, "主题 A");

        app.set_theme(ThemeKind::Nord);
        let buf_b = buffer_of(&mut app, 80, 24);
        assert_screen_bg(&buf_b, palette_b, "主题 B");

        assert_ne!(
            buf_a.content()[0].bg,
            buf_b.content()[0].bg,
            "换主题必须整屏换色"
        );
    }

    /// **chrome 槽位正确**：各类 transcript 条目的 fg 取自对应槽位。
    ///
    /// 用 `push_non_assistant` 直接产出行再逐 span 取样式 —— 比在字符画里找格子
    /// 更稳（不受 wrap / 横滚影响）。覆盖 user / thinking / 工具成功 / 工具失败 /
    /// Error / System 六类。
    #[test]
    fn test_push_non_assistant_uses_chrome_slots() {
        let palette = ThemeKind::default().palette();
        let width = 80;
        let lines = |item: &TranscriptLine| {
            let mut out: Vec<Line<'static>> = Vec::new();
            push_non_assistant(&mut out, item, width, true, palette);
            out
        };
        // 首行首 span 的 fg。
        let first_fg = |lines: &[Line<'static>]| lines[0].spans[0].style.fg;

        // 用户消息前缀 `> ` → `user`
        let user = lines(&TranscriptLine::User("hello".into()));
        assert_eq!(user[0].spans[0].content, "> ");
        assert_eq!(first_fg(&user), Some(palette.user), "用户前缀应取 `user`");

        // 思考 → `muted` + ITALIC
        let thinking = lines(&TranscriptLine::Thinking("想一下".into()));
        assert_eq!(
            first_fg(&thinking),
            Some(palette.muted),
            "思考行应取 `muted`"
        );
        assert!(
            thinking[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC),
            "思考行必须保持斜体"
        );

        // 工具成功 `✓` → `ok`；行动块本身仍是 `muted`
        let ok_line = lines(&TranscriptLine::Tool {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            summary: "a.rs".into(),
            result: None,
            success: true,
        });
        let ok_marks: Vec<_> = ok_line[0]
            .spans
            .iter()
            .filter(|s| s.content == "✓")
            .collect();
        assert_eq!(ok_marks.len(), 1, "成功工具行应有一个 ✓");
        assert_eq!(ok_marks[0].style.fg, Some(palette.ok), "✓ 应取 `ok`");
        assert_eq!(
            ok_line[0].spans[0].style.fg,
            Some(palette.muted),
            "行动块前缀应取 `muted`"
        );

        // 工具失败 `✗` → `fail`；失败结果首行也是 `fail`
        let fail_line = lines(&TranscriptLine::Tool {
            id: ToolCallId("c2".into()),
            name: "bash".into(),
            summary: "false".into(),
            result: Some("boom".into()),
            success: false,
        });
        let fail_marks: Vec<_> = fail_line[0]
            .spans
            .iter()
            .filter(|s| s.content == "✗")
            .collect();
        assert_eq!(fail_marks.len(), 1, "失败工具行应有一个 ✗");
        assert_eq!(fail_marks[0].style.fg, Some(palette.fail), "✗ 应取 `fail`");
        assert_eq!(
            fail_line[1].spans[0].style.fg,
            Some(palette.fail),
            "失败结果首行应取 `fail`"
        );

        // `Error: ` 前缀 → `fail`
        let err = lines(&TranscriptLine::Error("坏了".into()));
        assert_eq!(err[0].spans[0].content, "Error: ");
        assert_eq!(
            first_fg(&err),
            Some(palette.fail),
            "`Error: ` 前缀应取 `fail`"
        );

        // System 行 → `muted`
        let sys = lines(&TranscriptLine::System("提示".into()));
        assert_eq!(first_fg(&sys), Some(palette.muted), "System 行应取 `muted`");
    }
}
