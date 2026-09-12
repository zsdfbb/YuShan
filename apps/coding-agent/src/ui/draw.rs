#![cfg(feature = "tui-ratatui")]

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use ys_core::StopReason;

use super::app::{App, TranscriptLine};

pub fn ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 10 || area.height < 5 {
        // 太窄：仅显示 prompt
        return;
    }

    // 面板组件化：默认仅对话窗口；status 面板可选挂载（挂载时右栏 30%，否则单列全宽）
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if app.show_status {
            [Constraint::Percentage(70), Constraint::Percentage(30)]
        } else {
            [Constraint::Percentage(100), Constraint::Length(0)]
        })
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(if app.show_footer { 1 } else { 0 }),
        ])
        .split(cols[0]);

    draw_transcript(frame, left[0], app);
    draw_input(frame, left[1], app);
    if app.show_footer {
        draw_footer(frame, left[2], app);
    }
    if app.show_status {
        draw_status_panel(frame, cols[1], app);
    }
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

    let visible = compute_visible(app, area.width as usize);
    let para = Paragraph::new(visible)
        .block(Block::default().borders(Borders::ALL).title(" Chat "))
        .wrap(Wrap { trim: false })
        .style(Style::default());
    frame.render_widget(para, area);
}

/// 按 `scroll_offset` / `follow` 计算 transcript 可见切片。
///
/// **模型（v0 简化）**：
/// - 每个 `TranscriptLine` 算 1 行 scroll 单位（不做精确 wrapped line 计算）
/// - visible_height 由 transcript 自身行数估算（取全部可见，避免 clamp 异常）
/// - follow = true：取末尾所有行
/// - follow = false：按 `scroll_offset` 从末尾反向切片
fn compute_visible(app: &App, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line;

    // 把所有 transcript 行展开成 line 列表（带原始索引）
    let all_lines: Vec<Line<'static>> = app
        .transcript
        .iter()
        .flat_map(|line| line_to_text(line, width))
        .collect();

    let total = all_lines.len();
    if total == 0 {
        return vec![Line::from("(no messages yet)")];
    }

    if app.follow {
        return all_lines;
    }

    // 手动滚动：scroll_offset 表示"向上滚动 N 行"
    // scroll_offset = 0 = 看末尾；scroll_offset = N = 向上 N 行
    // **v0 简化**：clamp 到 transcript 行数级别（transcript.len() - scroll_offset），
    // 留出底部至少 1 行可见
    let max_scroll = total.saturating_sub(1);
    let effective_offset = app.scroll_offset.min(max_scroll);
    let end = total.saturating_sub(effective_offset);
    let visible_height = end; // 简化：可见高度 = 当前 end 位置（向下展开）
    let start = end.saturating_sub(visible_height);

    all_lines
        .into_iter()
        .skip(start)
        .take(end - start)
        .collect()
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Color, Style};
    use ratatui::widgets::{Block, Borders, Paragraph};

    let (text, style) = if app.is_turning {
        let dots = ".".repeat(1 + app.working_dot as usize);
        (format!("Working{dots}"), Style::default().fg(Color::Yellow))
    } else {
        (app.input.clone(), Style::default())
    };
    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Input "))
        .style(style);
    frame.render_widget(para, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, _app: &App) {
    use ratatui::style::{Color, Style};
    use ratatui::widgets::{Block, Borders, Paragraph};

    let hint = "Ready · Ctrl-D to quit · Tab to autocomplete";
    let para = Paragraph::new(hint)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::TOP));
    frame.render_widget(para, area);
}

fn draw_status_panel(frame: &mut Frame, area: Rect, app: &App) {
    use crate::format::format_tokens;
    use ratatui::widgets::{Block, Borders, Paragraph};

    let status_text = if app.view.logged_in_providers.is_empty() {
        "First run".to_string()
    } else {
        format!(
            "Logged in: {}/{}",
            app.view.logged_in_providers.len(),
            app.view.total_known_providers
        )
    };
    // turn 已用时长 draw 时从 Instant 实时派生（0 mutate；与 session_duration_str 同构）
    let turn_str = app
        .turn_started_at
        .map(|t| format!("{:.1}s", t.elapsed().as_secs_f32()))
        .unwrap_or_else(|| "-".to_string());
    let text = format!(
        " Provider: {}\n Model:    {}\n\n Tokens:   ↑{} ↓{}\n Turns:    {}\n Turn:     {}\n Session:  {}\n\n Cwd:      {}\n Tools:    {}\n\n {}",
        app.view.provider.as_deref().unwrap_or("(none)"),
        app.view.model.as_deref().unwrap_or("(none)"),
        format_tokens(app.view.total_input_tokens),
        format_tokens(app.view.total_output_tokens),
        app.view.turn_count,
        turn_str,
        app.view.session_duration_str(),
        app.view.cwd.display(),
        app.view.tools.join(", "),
        status_text,
    );
    let para = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Status "));
    frame.render_widget(para, area);
}

pub(crate) fn line_to_text(
    line: &TranscriptLine,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    let width = width.saturating_sub(2).max(1); // 减 border
    match line {
        TranscriptLine::User(s) => vec![Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Green)),
            Span::raw(s.clone()),
        ])],
        TranscriptLine::Assistant(s) => {
            // 简化：不做 wrap，直接单行
            vec![Line::from(Span::raw(s.clone()))]
        }
        TranscriptLine::Tool {
            name,
            args,
            result,
            success,
        } => vec![
            Line::from(format!("→ {} {}", name, args)),
            Line::from(if *success {
                format!("  ✓ {}", truncate(result, width))
            } else {
                format!("  ✗ {}", truncate(result, width))
            }),
        ],
        TranscriptLine::Summary {
            rounds,
            stop,
            elapsed_secs,
        } => {
            let sym = match stop {
                StopReason::Completed => "✓",
                StopReason::MaxRounds => "⚠ MaxRounds",
                StopReason::Cancelled => "✗ Cancelled",
                _ => "?",
            };
            vec![Line::from(format!(
                "{} {} rounds · {:.1}s",
                sym, rounds, elapsed_secs
            ))]
        }
        TranscriptLine::Error(s) => vec![Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::Red)),
            Span::raw(s.clone()),
        ])],
        TranscriptLine::System(s) => vec![Line::from(vec![Span::styled(
            s.clone(),
            Style::default().fg(Color::DarkGray),
        )])],
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

/// 退出时打印的纯文本完整对话（pi 语义）：strip 样式/光标标记，按 `width` 硬 wrap。
///
/// 供 `mod.rs::restore_terminal` 在 `LeaveAlternateScreen` 后逐行写进主屏 scrollback。
pub(crate) fn transcript_to_lines(app: &App, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in &app.transcript {
        for raw in plain_line_rows(line) {
            for chunk in wrap_plain(&raw, width) {
                out.push(chunk);
            }
        }
    }
    if out.is_empty() {
        out.push("(no messages yet)".to_string());
    }
    out
}

/// 单个 TranscriptLine 的纯文本行（与 `line_to_text` 同源符号，但无样式）。
fn plain_line_rows(line: &TranscriptLine) -> Vec<String> {
    match line {
        TranscriptLine::User(s) => vec![format!("> {s}")],
        TranscriptLine::Assistant(s) => vec![s.clone()],
        TranscriptLine::Tool {
            name,
            args,
            result,
            success,
        } => vec![
            format!("→ {name} {args}"),
            format!("  {} {}", if *success { "✓" } else { "✗" }, result),
        ],
        TranscriptLine::Summary {
            rounds,
            stop,
            elapsed_secs,
        } => {
            let sym = match stop {
                StopReason::Completed => "✓",
                StopReason::MaxRounds => "⚠ MaxRounds",
                StopReason::Cancelled => "✗ Cancelled",
                _ => "?",
            };
            vec![format!("{sym} {rounds} rounds · {elapsed_secs:.1}s")]
        }
        TranscriptLine::Error(s) => vec![format!("Error: {s}")],
        TranscriptLine::System(s) => vec![s.clone()],
    }
}

/// 按字符宽度硬 wrap（退出文档用；不做 CJK 精确列宽）。
fn wrap_plain(s: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return vec![s.to_string()];
    }
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::{App, TranscriptLine};
    use crate::view::AppView;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::time::Instant;
    use ys_core::StopReason;

    fn make_app() -> App {
        let view = AppView {
            cwd: PathBuf::from("/tmp"),
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            config_path: PathBuf::from("/tmp/auth.json"),
            logged_in_providers: vec!["deepseek".into()],
            total_known_providers: 3,
            version: "test",
            total_input_tokens: 320,
            total_output_tokens: 1247,
            turn_count: 1,
            session_started: Instant::now(),
            message_count: 0,
            tools: vec!["read".into(), "write".into()],
            context_window: None,
            is_first_run: false,
            commands: vec![],
        };
        let mut app = App::new(view);
        app.show_status = true; // 既有 status 面板测试依赖该面板可见
        app.transcript.push(TranscriptLine::User("hello".into()));
        app.transcript
            .push(TranscriptLine::Assistant("hi there".into()));
        app.transcript.push(TranscriptLine::Summary {
            rounds: 1,
            stop: StopReason::Completed,
            elapsed_secs: 2.3,
        });
        app
    }

    fn render_to_text(app: &mut App) -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn test_ui_renders_three_columns() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        // 断言：buffer 不空 + 宽度正确
        assert_eq!(buffer.area().width, 120);
        assert_eq!(buffer.area().height, 40);
    }

    #[test]
    fn test_status_panel_shows_provider() {
        let text = render_to_text(&mut make_app());
        assert!(
            text.contains("deepseek"),
            "status panel should show provider name"
        );
        assert!(
            text.contains("deepseek-chat"),
            "status panel should show model name"
        );
    }

    #[test]
    fn test_summary_line_renders_completed_symbol() {
        let text = render_to_text(&mut make_app());
        assert!(text.contains("✓"), "summary should show ✓ for Completed");
        assert!(text.contains("1 rounds"), "summary should show round count");
    }

    #[test]
    fn test_summary_line_renders_max_rounds_symbol() {
        let mut app = make_app();
        app.transcript.pop();
        app.transcript.push(TranscriptLine::Summary {
            rounds: 10,
            stop: StopReason::MaxRounds,
            elapsed_secs: 30.0,
        });
        let text = render_to_text(&mut app);
        assert!(
            text.contains("⚠ MaxRounds"),
            "summary should show ⚠ MaxRounds"
        );
    }

    #[test]
    fn test_summary_line_renders_cancelled_symbol() {
        let mut app = make_app();
        app.transcript.pop();
        app.transcript.push(TranscriptLine::Summary {
            rounds: 2,
            stop: StopReason::Cancelled,
            elapsed_secs: 1.5,
        });
        let text = render_to_text(&mut app);
        assert!(
            text.contains("✗ Cancelled"),
            "summary should show ✗ Cancelled"
        );
    }

    #[test]
    fn test_user_line_renders_with_prompt() {
        let text = render_to_text(&mut make_app());
        assert!(text.contains("hello"), "user line should be in transcript");
        assert!(
            text.contains("hi there"),
            "assistant line should be in transcript"
        );
    }

    #[test]
    fn test_error_line_renders() {
        let mut app = make_app();
        app.transcript
            .push(TranscriptLine::Error("API key rejected (401)".into()));
        let text = render_to_text(&mut app);
        assert!(
            text.contains("API key rejected"),
            "error should appear in transcript"
        );
    }

    #[test]
    fn test_tools_in_status_panel() {
        let text = render_to_text(&mut make_app());
        assert!(text.contains("read"), "tools list should contain read");
        assert!(text.contains("write"), "tools list should contain write");
    }

    #[test]
    fn test_default_chat_only_hides_status_and_footer() {
        let mut app = make_app();
        app.show_status = false;
        app.show_footer = false;
        let text = render_to_text(&mut app);
        assert!(
            !text.contains("Provider:"),
            "status panel hidden by default"
        );
        assert!(!text.contains("Ready"), "footer hidden by default");
        assert!(
            text.contains("hello"),
            "chat window must still render transcript"
        );
    }

    #[test]
    fn test_turn_renders_working_animation() {
        let mut app = make_app();
        app.is_turning = true;
        app.working_dot = 1; // "Working.."
        let text = render_to_text(&mut app);
        assert!(text.contains("Working.."), "should render animated dots");
        assert!(text.contains("hello"), "user line visible during turn");
    }

    #[test]
    fn test_idle_does_not_render_working() {
        let mut app = make_app();
        app.is_turning = false;
        app.working_dot = 2;
        let text = render_to_text(&mut app);
        assert!(!text.contains("Working"), "idle must not show Working");
    }

    #[test]
    fn test_status_shows_turn_elapsed() {
        let mut app = make_app();
        app.turn_started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
        let text = render_to_text(&mut app);
        assert!(text.contains("Turn:"), "status panel should show Turn line");
    }

    #[test]
    fn test_transcript_to_lines_plain_and_wrapped() {
        let mut app = make_app();
        app.transcript.push(TranscriptLine::Assistant(
            "xxxxxxxxxxxxxxxxxxxx".to_string(),
        )); // 20 个字符
        let lines = transcript_to_lines(&app, 10);
        assert!(lines.contains(&"> hello".to_string()), "user row prefix");
        assert!(lines.contains(&"hi there".to_string()), "assistant row");
        assert!(
            lines.iter().any(|l| l.contains("1 rounds")),
            "summary row present"
        );
        assert!(
            lines.iter().any(|l| l.len() == 10),
            "long lines wrap to width"
        );
    }

    #[test]
    fn test_multi_turn_conversation() {
        let mut app = make_app();
        // 清空 make_app 默认内容，构造全新多轮对话
        app.transcript.clear();
        app.transcript
            .push(TranscriptLine::User("first question".into()));
        app.transcript
            .push(TranscriptLine::Assistant("first answer".into()));
        app.transcript
            .push(TranscriptLine::User("second question".into()));
        app.transcript
            .push(TranscriptLine::Assistant("second answer".into()));
        app.transcript.push(TranscriptLine::Summary {
            rounds: 2,
            stop: StopReason::Completed,
            elapsed_secs: 5.0,
        });
        let text = render_to_text(&mut app);
        assert!(
            text.contains("first question"),
            "first user message should be visible"
        );
        assert!(
            text.contains("second answer"),
            "last assistant message should be visible"
        );
    }

    #[test]
    fn test_empty_transcript_shows_placeholder() {
        let mut app = make_app();
        app.transcript.clear();
        let text = render_to_text(&mut app);
        assert!(
            text.contains("(no messages yet)"),
            "empty transcript should show placeholder"
        );
    }

    #[test]
    fn test_scroll_offset_changes_visible() {
        // follow=true 时所有消息可见
        let mut app = make_app();
        app.follow = true;
        let text_follow = render_to_text(&mut app);
        assert!(
            text_follow.contains("hello"),
            "follow=true should show first user message"
        );
        assert!(
            text_follow.contains("hi there"),
            "follow=true should show assistant message"
        );

        // follow=false + 大 scroll_offset：compute_visible 的 v0 简化逻辑中
        // start 恒为 0（visible_height = end），scroll_offset 只截断尾部行数。
        // effective_offset 被 clamp 到 total-1 后 end=1，即只保留第 1 个 buffer 行。
        // 先添加足够多的行使 total 足够大
        for i in 0..20 {
            app.transcript
                .push(TranscriptLine::User(format!("msg-{i}")));
            app.transcript
                .push(TranscriptLine::Assistant(format!("reply-{i}")));
        }
        app.follow = false;
        // 极大 scroll_offset 使 effective_offset clamp 到 total-1，end=1
        app.scroll_offset = usize::MAX;
        let text_scrolled = render_to_text(&mut app);
        // 尾部消息应被截断（end=1，只保留第 1 行）
        assert!(
            !text_scrolled.contains("msg-19"),
            "max scroll_offset should truncate tail message msg-19"
        );
        assert!(
            !text_scrolled.contains("reply-19"),
            "max scroll_offset should truncate tail reply-19"
        );
        // 首行仍然可见（start 恒为 0，v0 简化未实现真正的窗口滚动）
        assert!(
            text_scrolled.contains("hello"),
            "first line stays visible since start is fixed at 0 in v0 scroll"
        );
    }

    #[test]
    fn test_working_animation_three_states() {
        // 测试 Working. / Working.. / Working... 三个状态
        for (dot, expected) in [(0u8, "Working."), (1u8, "Working.."), (2u8, "Working...")] {
            let mut app = make_app();
            app.is_turning = true;
            app.working_dot = dot;
            let text = render_to_text(&mut app);
            assert!(
                text.contains(expected),
                "working_dot={dot} should render \"{expected}\""
            );
        }
    }
}
