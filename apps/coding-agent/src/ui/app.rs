#![cfg(feature = "tui-ratatui")]

use std::time::Instant;

use agent_core::{CancelToken, StopReason};

use crate::view::AppView;

/// UI 全部状态（与 AppView 解耦 — AppView 是 snapshot，App 是 chat UI state）
pub struct App {
    pub view: AppView,
    pub view_built_at: Instant,

    // transcript
    pub transcript: Vec<TranscriptLine>,
    pub scroll_offset: usize,
    pub follow: bool,

    // input
    pub input: String,
    pub input_cursor: usize,
    pub completion: Option<CompletionState>,

    // 控制
    pub is_turning: bool,
    pub cancel_requested: bool,
    pub should_quit: bool,
    pub pending_submit: Option<String>,
    /// Cancel token clone — event_loop 构造时 set。Esc/Ctrl-C 触发 cancel。
    /// 由 `Agent::cancel_handle()` 拿 Clone 写进来。
    pub cancel_token: Option<CancelToken>,
}

#[derive(Clone, Debug)]
pub enum TranscriptLine {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        args: String,
        result: String,
        success: bool,
    },
    Summary {
        rounds: u32,
        stop: StopReason,
        elapsed_secs: f32,
    },
    Error(String),
    System(String),
}

pub struct CompletionState {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
}

#[derive(Clone)]
pub struct CompletionItem {
    pub display: String,
    pub replacement: String,
}

impl App {
    pub fn new(initial_view: AppView) -> Self {
        Self {
            view: initial_view,
            view_built_at: Instant::now(),
            transcript: Vec::new(),
            scroll_offset: 0,
            follow: true,
            input: String::new(),
            input_cursor: 0,
            completion: None,
            is_turning: false,
            cancel_requested: false,
            should_quit: false,
            pending_submit: None,
            cancel_token: None, // NEW: event_loop 构造时 set
        }
    }

    pub fn take_submitted(&mut self) -> Option<String> {
        self.pending_submit.take()
    }
}
