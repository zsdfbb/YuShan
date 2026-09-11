#![cfg(feature = "tui-ratatui")]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{App, CompletionItem, CompletionState};

/// 单个 slash command 的元数据，用于 Tab 补全。
///
/// **c phase**：原 `crate::tui_completer::CmdEntry` 复制到 `ui/events.rs` 内部；
/// ratatui 模式不依赖 rustyline（rustyline 已在 c 阶段删除）。
#[derive(Clone)]
pub struct CmdEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
}

/// 处理单个 Event。App 状态变化由事件驱动。
pub fn handle_event(event: Event, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    if let Event::Key(key) = event {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        handle_key(key, app)?;
    } else if let Event::Resize(w, h) = event {
        // ratatui 自动响应 Resize 事件
        let _ = (w, h);
    }
    Ok(())
}

fn handle_key(key: KeyEvent, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 补全 popup 打开时优先处理
    if app.completion.is_some() {
        return handle_completion_key(key, app);
    }

    // 2. Ctrl-C — 调 token.cancel()（不需 &mut Agent）
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(token) = app.cancel_token.as_ref() {
            token.cancel();
        }
        app.cancel_requested = true; // 意图标记（debug 观测）
        return Ok(());
    }

    // 3. Ctrl-D 退出
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        app.should_quit = true;
        return Ok(());
    }

    // 4. Esc — turn 进行中触发取消；idle 清空 input
    if key.code == KeyCode::Esc {
        if app.is_turning {
            // turn 进行中：触发取消（与 Ctrl-C 等效）
            if let Some(token) = app.cancel_token.as_ref() {
                token.cancel();
            }
            app.cancel_requested = true;
        } else {
            // idle：保留旧行为 —— 清空 input
            app.input.clear();
            app.input_cursor = 0;
        }
        return Ok(());
    }

    // 5. 滚动（带 clamp）
    let max_scroll = compute_max_scroll(app);
    match key.code {
        KeyCode::Up => {
            app.scroll_offset = (app.scroll_offset + 1).min(max_scroll);
            app.follow = false;
        }
        KeyCode::Down => {
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
            if app.scroll_offset == 0 {
                app.follow = true;
            }
        }
        KeyCode::PageUp => {
            app.scroll_offset = (app.scroll_offset + 10).min(max_scroll);
            app.follow = false;
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
            if app.scroll_offset == 0 {
                app.follow = true;
            }
        }
        KeyCode::End => {
            app.scroll_offset = 0;
            app.follow = true;
        }
        _ => {}
    }

    // 6. Tab 触发补全
    if key.code == KeyCode::Tab {
        if app.input.starts_with('/') {
            let entries = build_cmd_entries();
            let (_start, pairs) = complete_inline(&entries, &app.input, app.input_cursor);
            if !pairs.is_empty() {
                app.completion = Some(CompletionState {
                    items: pairs
                        .into_iter()
                        .map(|p| CompletionItem {
                            display: p.display,
                            replacement: p.replacement,
                        })
                        .collect(),
                    selected: 0,
                });
            }
        }
        return Ok(());
    }

    // 7. Enter 提交
    if key.code == KeyCode::Enter {
        let line = std::mem::take(&mut app.input);
        app.input_cursor = 0;
        app.follow = true;
        app.pending_submit = Some(line);
        return Ok(());
    }

    // 8. Backspace / 字符输入
    if key.code == KeyCode::Backspace && app.input_cursor > 0 {
        app.input.remove(app.input_cursor - 1);
        app.input_cursor -= 1;
    } else if let KeyCode::Char(c) = key.code {
        app.input.insert(app.input_cursor, c);
        app.input_cursor += c.len_utf8();
    }

    Ok(())
}

fn handle_completion_key(key: KeyEvent, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    use KeyCode::*;

    if let Some(state) = app.completion.as_mut() {
        match key.code {
            Esc => {
                app.completion = None;
            }
            Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            Down => {
                if state.selected + 1 < state.items.len() {
                    state.selected += 1;
                }
            }
            Tab | Enter => {
                if let Some(item) = state.items.get(state.selected).cloned() {
                    app.input = item.replacement.clone();
                    app.input_cursor = app.input.len();
                }
                app.completion = None;
            }
            _ => {
                app.completion = None;
            }
        }
    }
    Ok(())
}

fn build_cmd_entries() -> Vec<CmdEntry> {
    use crate::commands::builtin::builtin_help_entries;
    builtin_help_entries()
        .into_iter()
        .map(|h| CmdEntry {
            name: h.name,
            description: h.description,
            arg_hint: h.arg_hint,
        })
        .collect()
}

// inline 实现（避免 rustyline 类型耦合）
fn complete_inline(entries: &[CmdEntry], line: &str, pos: usize) -> (usize, Vec<CmdPair>) {
    if !line.starts_with('/') {
        return (0, vec![]);
    }
    let prefix = &line[1..pos.min(line.len())];
    let pairs: Vec<CmdPair> = entries
        .iter()
        .filter(|e| e.name.starts_with(prefix))
        .map(|e| {
            let display = match e.arg_hint {
                Some(h) => format!("/{} {}  — {}", e.name, h, e.description),
                None => format!("/{}  — {}", e.name, e.description),
            };
            CmdPair {
                display,
                replacement: format!("/{} ", e.name),
            }
        })
        .collect();
    (1, pairs)
}

struct CmdPair {
    display: String,
    replacement: String,
}

/// 计算允许的最大 `scroll_offset`。
///
/// **v0 简化**：每个 `TranscriptLine` 算 1 行 scroll 单位（与 `compute_visible`
/// 一致）。max scroll = transcript 行数 - 1（保证至少 1 行可见）。
fn compute_max_scroll(app: &App) -> usize {
    app.transcript.len().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use agent_core::CancelToken;

    use super::handle_key;
    use crate::view::AppView;
    use crate::ui::app::App;

    /// 构造一个测试 App：view 是 placeholder，cancel_token 已挂。
    fn make_test_app() -> App {
        let view = AppView {
            cwd: PathBuf::from("/tmp"),
            provider: None,
            model: None,
            config_path: PathBuf::from("/tmp/auth.json"),
            logged_in_providers: vec![],
            total_known_providers: 0,
            version: "test",
            total_input_tokens: 0,
            total_output_tokens: 0,
            turn_count: 0,
            session_started: Instant::now(),
            message_count: 0,
            tools: vec![],
            context_window: None,
            is_first_run: false,
            commands: vec![],
        };
        let mut app = App::new(view);
        app.cancel_token = Some(CancelToken::new());
        app
    }

    /// Esc 在 turn 进行中应触发 cancel。
    #[test]
    fn test_esc_cancels_running_turn() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = make_test_app();
        app.is_turning = true;
        let token = app.cancel_token.as_ref().unwrap().clone();

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(key, &mut app).unwrap();

        assert!(token.is_cancelled(), "Esc during turn should trigger cancel");
    }

    /// Esc 在 idle 时应清空 input（保留旧行为），不触发 cancel。
    #[test]
    fn test_esc_idle_clears_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = make_test_app();
        app.is_turning = false;
        let token = app.cancel_token.as_ref().unwrap().clone();
        app.input = "/mo".to_string();

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(key, &mut app).unwrap();

        assert!(!token.is_cancelled(), "Esc when idle should NOT cancel");
        assert_eq!(app.input, "", "Esc when idle should clear input");
    }

    /// Ctrl-C 总是触发 cancel（不论 is_turning）；idle 时不修改 input。
    #[test]
    fn test_ctrl_c_always_cancels() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = make_test_app();
        app.is_turning = false; // 即使 idle
        let token = app.cancel_token.as_ref().unwrap().clone();

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(key, &mut app).unwrap();

        assert!(token.is_cancelled(), "Ctrl-C should always cancel");
        assert_eq!(app.input, "", "Ctrl-C should NOT clear input (idle)");
    }
}
