#![cfg(feature = "tui-ratatui")]

use std::time::Instant;

use ys_core::{CancelToken, StopReason};

use crate::view::AppView;

/// UI 全部状态（与 AppView 解耦 — AppView 是 snapshot，App 是 chat UI state）
pub struct App {
    pub view: AppView,
    pub view_built_at: Instant,

    // 会话记录
    pub transcript: Vec<TranscriptLine>,
    pub scroll_offset: usize,
    pub follow: bool,

    // 输入
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

    // turn 活渲染（pi 模型：动画节拍 + 按需绘制）
    pub turn_started_at: Option<Instant>,
    pub working_dot: u8,

    // 面板挂载（默认仅对话窗口；status/footer 为可选面板）
    pub show_status: bool,
    pub show_footer: bool,
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
            cancel_token: None, // 新增：event_loop 构造时 set
            turn_started_at: None,
            working_dot: 0,
            show_status: false, // 默认仅对话窗口；status/footer 为可选面板
            show_footer: false,
        }
    }

    pub fn take_submitted(&mut self) -> Option<String> {
        self.pending_submit.take()
    }
}
