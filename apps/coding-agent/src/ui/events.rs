#![cfg(feature = "tui-ratatui")]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui_completer::CmdEntry;

use super::app::{App, CompletionItem, CompletionState};

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

    // 2. Ctrl-C
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.cancel_requested = true;
        return Ok(());
    }

    // 3. Ctrl-D 退出
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        app.should_quit = true;
        return Ok(());
    }

    // 4. Esc 清空 input
    if key.code == KeyCode::Esc {
        app.input.clear();
        app.input_cursor = 0;
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
