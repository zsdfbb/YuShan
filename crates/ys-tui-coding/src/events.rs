//! crossterm 事件 → [`App`] 状态迁移 + 对 app 线程的请求（`Request` / `Boundary`）。
//!
//! 键位集：**Enter 提交或填入补全 / Ctrl-C·Ctrl-D·Esc 退出或取消 / 上下翻页滚动 /
//! 左右与 Home·Delete 归输入光标 / Tab 开补全浮层**。
//!
//! # 命令分发（T10）
//!
//! Enter 时把输入交给 [`crate::commands::parse`]，按 [`crate::commands::Action`] 分派：
//! 本地动作当场执行（写 transcript），`Request` 投给 app 线程。**唯一例外是
//! [`Action::Prompt`]** —— 它要开一块模态浮层，而画屏得握 `Terminal`，故这里只把
//! 意图记进 [`App::pending_prompt`]，由 `run.rs` 的事件循环取走并跑模态循环
//! （设计 §3「TUI 自己的浮层选择器」）。
//!
//! # 方向键的取舍：上下归聊天、左右归输入
//!
//! 方向键只有 4 个，却要服务两个焦点（聊天滚动 + 输入光标）。本模块的划分：
//!
//! | 键 | 归属 |
//! |---|---|
//! | `Up` / `Down` / `PageUp` / `PageDown` | **聊天滚动**（`App::scroll_offset`） |
//! | `Left` / `Right` / `Delete` / `Home` | **输入光标**（`InputBuffer`） |
//! | `End` | **回到最新消息**（`scroll_offset = 0`、`follow = true`） |
//!
//! 即：竖直方向 = 聊天，水平方向 = 输入。`Home`/`End` 这一对**不做对称绑定** ——
//! `End` 早在 T5 就钉住了「跳回聊天底部」的语义（有测试），`Home` 之前空着，
//! 于是给输入行首。理由：聊天滚动已有 4 个键（且退回底部是高频操作），而输入
//! 行首此前**无键可用**（`Home` 是唯一自然的候选）。若日后要让输入也拥有
//! 「行尾/缓冲末尾」，优先候选是 `Ctrl-E`（不占用 `End`），而不是改 `End` 的语义。
//!
//! **补全浮层打开时上下归浮层**（设计 §2.4）—— 焦点在浮层上，聊天滚动让位。

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::Sender;
use ys_protocol::{Boundary, Request};

use crate::app::{App, message_text};
use crate::commands::{self, Action};
use crate::completion;

/// 一次 PageUp/PageDown 滚动的行数。
const PAGE_ROWS: isize = 10;

/// 处理一个 crossterm 事件。
pub fn handle_event(
    event: Event,
    app: &mut App,
    requests: &Sender<Request>,
    boundaries: &Sender<Boundary>,
    width: u16,
    height: u16,
) {
    match event {
        Event::Key(key) => handle_key(key, app, requests, boundaries, width, height),
        // bracketed paste：多行粘贴**不得**被当多次提交
        Event::Paste(text) => app.input.insert_str(&text),
        // 尺寸变化由 ratatui 在下次 draw 时自动响应（wrap 行数随之重算）
        Event::Resize(_, _) => {}
        _ => {}
    }
}

/// 处理一次按键（`KeyEventKind::Release` / `Repeat` 之外的都忽略）。
pub fn handle_key(
    key: KeyEvent,
    app: &mut App,
    requests: &Sender<Request>,
    boundaries: &Sender<Boundary>,
    width: u16,
    height: u16,
) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // 补全浮层优先：↑↓ 归浮层；Tab/Enter 填入；Esc 关闭。
    // **其他键只关闭浮层并把这一下交还给正常处理**（不吞键 —— 用户接着敲的字
    // 必须进输入框，否则「按错一个键就丢一次输入」）。
    if handle_completion_key(app, key.code) {
        return;
    }

    if ctrl {
        match key.code {
            KeyCode::Char('c') => {
                if app.is_turning {
                    request_abort(boundaries);
                } else {
                    app.should_quit = true;
                }
                return;
            }
            KeyCode::Char('d') => {
                app.should_quit = true;
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => {
            if app.is_turning {
                request_abort(boundaries);
            } else {
                app.input.clear();
            }
        }
        // Shift/Alt+Enter 换行（Alt 是 macOS Terminal.app 的兜底，设计 §2.3）
        KeyCode::Enter if shift || alt => app.input.insert_char('\n'),
        KeyCode::Enter => submit(app, requests, boundaries),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete_forward(),
        // 左右归**输入光标**（上下归聊天滚动 —— 见模块文档的取舍表）
        KeyCode::Left => app.input.cursor_left(),
        KeyCode::Right => app.input.cursor_right(),
        // Home 之前未绑定：给输入**行首**（End 保留给「回到最新消息」）
        KeyCode::Home => app.input.cursor_home(),
        KeyCode::Up => scroll(app, width, height, 1),
        KeyCode::PageUp => scroll(app, width, height, PAGE_ROWS),
        KeyCode::Down => scroll(app, width, height, -1),
        KeyCode::PageDown => scroll(app, width, height, -PAGE_ROWS),
        KeyCode::End => {
            app.scroll_offset = 0;
            app.follow = true;
        }
        // Tab：输入 `/` 开头时按前缀开补全浮层（无命中 / 不以 `/` 开头 = no-op）
        KeyCode::Tab => {
            if let Some(state) = completion::open(&app.input.text) {
                app.completion = Some(state);
            }
        }
        // 字符输入（Ctrl-* 已在上面 return）
        KeyCode::Char(c) if !ctrl => app.input.insert_char(c),
        _ => {}
    }
}

/// 补全浮层打开时的键位（设计 §2.4）。
///
/// 返回 `true` = 这一下已被浮层消费；`false` = 浮层在本次按键里被关掉，
/// 调用方应**继续按正常键处理**（含浮层本来就关着的情况）。
fn handle_completion_key(app: &mut App, code: KeyCode) -> bool {
    if app.completion.is_none() {
        return false;
    }
    match code {
        KeyCode::Up => {
            app.completion.as_mut().expect("已断言非 None").move_up();
            true
        }
        KeyCode::Down => {
            app.completion.as_mut().expect("已断言非 None").move_down();
            true
        }
        // Tab/Enter 都「接受当前项」—— Tab 是补全的直觉键，Enter 是选择器的直觉键
        KeyCode::Tab | KeyCode::Enter => {
            let replacement = app
                .completion
                .as_ref()
                .and_then(|s| s.current())
                .map(|item| item.replacement.clone());
            if let Some(text) = replacement {
                app.input.text = text;
                app.input.cursor = app.input.text.chars().count();
            }
            app.completion = None;
            true
        }
        KeyCode::Esc => {
            app.completion = None;
            true
        }
        _ => {
            app.completion = None;
            false
        }
    }
}

/// 滚动：`delta > 0` 向上（远离底部），`delta < 0` 向下（回到贴底）。
fn scroll(app: &mut App, width: u16, height: u16, delta: isize) {
    let max = app.compute_max_scroll(width, height);
    let next = if delta >= 0 {
        app.scroll_offset.saturating_add(delta as usize).min(max)
    } else {
        app.scroll_offset.saturating_sub(delta.unsigned_abs())
    };
    app.scroll_offset = next;
    // 回到底部即恢复跟随（与旧 TUI 的 Down/PageDown 语义一致）
    app.follow = next == 0;
}

/// 提交输入：命令分发（[`commands::parse`]）或普通消息。
fn submit(app: &mut App, requests: &Sender<Request>, boundaries: &Sender<Boundary>) {
    let text = app.input.take();
    app.completion = None;
    if text.trim().is_empty() {
        return;
    }
    dispatch(app, requests, boundaries, commands::parse(&text));
}

/// 分派一个**已解析**的 [`Action`]。
///
/// 从 [`submit`] 里抽出来，是为了让 `parse` 目前**不产出**的动作（如
/// [`Action::Abort`]）也能被直接构造并测到其处理路径 —— 否则那一支天然是
/// 不可达代码，既没有测试，也说不清它还对不对。
fn dispatch(
    app: &mut App,
    requests: &Sender<Request>,
    boundaries: &Sender<Boundary>,
    action: Action,
) {
    match action {
        Action::None => {}
        Action::Request(req) => {
            // 只有普通消息才本地回显 + 进 turn 态；`/new` 之类不动 transcript
            if let Request::Prompt(msg) = &req {
                app.begin_local_turn(message_text(msg));
            }
            send_request(app, requests, req);
        }
        Action::Quit => app.should_quit = true,
        Action::Abort => request_abort(boundaries),
        Action::Local(local) => commands::apply_local(app, local),
        // 需要终端的模态浮层：交给 run.rs 的事件循环（它握着 Terminal）
        Action::Prompt(kind) => app.pending_prompt = Some(kind),
    }
}

/// 投一条请求给 app 线程（信道 ①）。
fn send_request(app: &mut App, requests: &Sender<Request>, req: Request) {
    if requests.blocking_send(req).is_err() {
        // app 线程已消失 —— 没有对话对象了，收摊（恢复仍由 run 无条件执行）
        app.should_quit = true;
    }
}

/// 中途取消：投 `Boundary::Abort` 给 `BasicLoop` 的**轮边界**。
fn request_abort(boundaries: &Sender<Boundary>) {
    if let Err(err) = boundaries.try_send(Boundary::Abort) {
        // 队列满（消费者只在轮边界拉）：退化为阻塞投递，绝不静默丢掉取消。
        // Disconnected 时阻塞版立即返回 Err，不会挂住 UI 线程。
        let _ = boundaries.blocking_send(err.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::input_height;
    use crate::transcript::TranscriptLine;
    use crate::view::CodingView;
    use tokio::sync::mpsc;
    use ys_core::{ContentBlock, Role};

    /// 四条端点都留着 —— 两端都活着才是「信道通了」的语义，
    /// 只留发送端会让 `try_send` 直接 Disconnected，测试会测出假象。
    ///
    /// 接收端用 `RefCell` 包起来，好让 `&self` 就能取（否则每个测试都要
    /// `let mut ch`，而多数测试根本不取消息 → `unused_mut` 警告）。
    struct TestChannels {
        req_tx: Sender<Request>,
        req_rx: std::cell::RefCell<mpsc::Receiver<Request>>,
        bnd_tx: Sender<Boundary>,
        bnd_rx: std::cell::RefCell<mpsc::Receiver<Boundary>>,
    }

    impl TestChannels {
        fn next_request(&self) -> Option<Request> {
            self.req_rx.borrow_mut().try_recv().ok()
        }

        fn next_boundary(&self) -> Option<Boundary> {
            self.bnd_rx.borrow_mut().try_recv().ok()
        }
    }

    fn channels() -> TestChannels {
        let (req_tx, req_rx) = mpsc::channel(8);
        let (bnd_tx, bnd_rx) = mpsc::channel(8);
        TestChannels {
            req_tx,
            req_rx: std::cell::RefCell::new(req_rx),
            bnd_tx,
            bnd_rx: std::cell::RefCell::new(bnd_rx),
        }
    }

    fn app() -> App {
        App::new(CodingView::for_test())
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn test_enter_submits_prompt_with_local_echo() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("你好");

        handle_key(
            press(KeyCode::Enter),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );

        assert!(a.is_turning, "提交即进入 turn 态");
        assert_eq!(a.suppress_user_echo, 1, "本地已回显，事件那条要抑制");
        assert!(a.input.text.is_empty(), "提交后输入框清空");
        assert!(matches!(&a.transcript[0], TranscriptLine::User(s) if s == "你好"));

        let req = ch.next_request().expect("应发出 Request::Prompt");
        match req {
            Request::Prompt(msg) => {
                assert_eq!(msg.role, Role::User);
                assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "你好"));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn test_enter_on_blank_input_does_nothing() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("   ");
        handle_key(
            press(KeyCode::Enter),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert!(!a.is_turning);
        assert!(ch.next_request().is_none(), "空白输入不提交");
    }

    #[test]
    fn test_shift_enter_inserts_newline_not_submit() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("a");
        handle_key(
            with_mod(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "a\n");
        assert_eq!(a.input.lines(), 2);
        assert!(!a.is_turning);
        assert!(ch.next_request().is_none());
    }

    #[test]
    fn test_alt_enter_is_macos_fallback_for_newline() {
        let ch = channels();
        let mut a = app();
        handle_key(
            with_mod(KeyCode::Enter, KeyModifiers::ALT),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "\n");
    }

    /// 中文退格：走 `InputBuffer` 的 char 边界实现，绝不 panic。
    #[test]
    fn test_backspace_cjk_via_key_event() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("中文");
        handle_key(
            press(KeyCode::Backspace),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "中");
        handle_key(
            press(KeyCode::Backspace),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "");
        handle_key(
            press(KeyCode::Backspace),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "", "空缓冲退格是 no-op");
    }

    #[test]
    fn test_char_input() {
        let ch = channels();
        let mut a = app();
        for c in ['你', 'h', 'i'] {
            handle_key(
                press(KeyCode::Char(c)),
                &mut a,
                &ch.req_tx,
                &ch.bnd_tx,
                120,
                40,
            );
        }
        assert_eq!(a.input.text, "你hi");
        assert_eq!(a.input.cursor, 3);
    }

    #[test]
    fn test_ctrl_c_quits_when_idle() {
        let ch = channels();
        let mut a = app();
        handle_key(
            with_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert!(a.should_quit);
    }

    #[test]
    fn test_ctrl_c_aborts_when_turning() {
        let ch = channels();
        let mut a = app();
        a.is_turning = true;
        handle_key(
            with_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert!(!a.should_quit, "turn 中 Ctrl-C 是取消，不是退出");
        assert_eq!(ch.next_boundary().unwrap(), Boundary::Abort);
    }

    #[test]
    fn test_esc_aborts_when_turning_and_clears_input_when_idle() {
        let ch = channels();

        let mut a = app();
        a.is_turning = true;
        handle_key(press(KeyCode::Esc), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(ch.next_boundary().unwrap(), Boundary::Abort);

        let mut a = app();
        a.input.insert_str("/mo");
        handle_key(press(KeyCode::Esc), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(a.input.text.is_empty(), "idle Esc 清空输入");
    }

    #[test]
    fn test_ctrl_d_quits() {
        let ch = channels();
        let mut a = app();
        handle_key(
            with_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert!(a.should_quit);
    }

    #[test]
    fn test_scroll_keys_move_offset_and_toggle_follow() {
        let ch = channels();
        let mut a = app();
        for i in 0..80 {
            a.transcript.push(TranscriptLine::User(format!("msg-{i}")));
        }
        assert!(a.follow);

        handle_key(press(KeyCode::Up), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(a.scroll_offset, 1);
        assert!(!a.follow, "向上滚即脱离跟随");

        handle_key(
            press(KeyCode::PageUp),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.scroll_offset, 11);

        handle_key(
            press(KeyCode::PageDown),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.scroll_offset, 1);

        handle_key(press(KeyCode::End), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(a.scroll_offset, 0);
        assert!(a.follow);
    }

    #[test]
    fn test_scroll_clamps_at_max() {
        let ch = channels();
        let mut a = app();
        a.transcript.push(TranscriptLine::User("only one".into()));
        // 内容远少于一屏：max scroll = 0，滚动是 no-op
        handle_key(
            press(KeyCode::PageUp),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.scroll_offset, 0);
    }

    #[test]
    fn test_paste_inserts_multiline_at_once() {
        let ch = channels();
        let mut a = app();
        assert_eq!(input_height(&a), 3, "空输入：1 行内容 + 2 边框");
        handle_event(
            Event::Paste("line1\nline2\nline3".into()),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "line1\nline2\nline3");
        assert_eq!(a.input.lines(), 3);
        assert!(!a.is_turning, "粘贴不得被当提交");
        assert_eq!(input_height(&a), 5, "粘贴 3 行 → 输入区撑高到 5");
        assert_eq!(a.input.cursor, a.input.text.chars().count(), "光标在末尾");
    }

    // -----------------------------------------------------------------------
    // 输入光标键（T9）
    // -----------------------------------------------------------------------

    /// `Left`/`Right` 经 `handle_key` 落到输入缓冲（不是聊天滚动）。
    #[test]
    fn test_left_right_move_input_cursor_not_scroll() {
        let ch = channels();
        let mut a = app();
        a.transcript.push(TranscriptLine::User("x".into()));
        a.input.insert_str("中文");
        assert_eq!(a.input.cursor, 2);

        handle_key(
            press(KeyCode::Left),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 1, "左移一个 char");
        assert_eq!(a.scroll_offset, 0, "左右键不动聊天滚动");
        assert!(a.follow, "左右键不脱离跟随");

        handle_key(
            press(KeyCode::Right),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 2, "右移回来");

        // 边界：行首左移 / 行尾右移都是 no-op
        a.input.cursor = 0;
        handle_key(
            press(KeyCode::Left),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 0);
        a.input.cursor = a.input.text.chars().count();
        handle_key(
            press(KeyCode::Right),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, a.input.text.chars().count());
    }

    /// `Delete` 删光标**后**一个 char。
    #[test]
    fn test_delete_key_deletes_forward() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("中文");
        a.input.cursor = 0;

        handle_key(
            press(KeyCode::Delete),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "文", "删掉光标后的「中」");
        assert_eq!(a.input.cursor, 0);

        handle_key(
            press(KeyCode::Delete),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "", "再删一个");
        handle_key(
            press(KeyCode::Delete),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.text, "", "末尾 Delete 是 no-op");
    }

    /// `Home` 移到**输入行首**；`End` 的聊天滚动语义**不受影响**。
    #[test]
    fn test_home_moves_input_cursor_while_end_still_scrolls() {
        let ch = channels();
        let mut a = app();
        for i in 0..80 {
            a.transcript.push(TranscriptLine::User(format!("msg-{i}")));
        }
        a.input.insert_str("ab\ncd");
        assert_eq!(a.input.cursor, 5);

        // Home：输入行首（第 2 行行首 = 3），聊天滚动纹丝不动
        handle_key(
            press(KeyCode::Home),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 3, "Home = 输入当前行行首");
        assert_eq!(a.scroll_offset, 0);
        assert!(a.follow);

        // End：聊天跳回最新（既有语义），输入光标不动
        a.follow = false;
        a.scroll_offset = 7;
        handle_key(press(KeyCode::End), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(a.scroll_offset, 0);
        assert!(a.follow);
        assert_eq!(a.input.cursor, 3, "End 不动输入光标");
    }

    /// 多行输入：光标上下移动**不属于**输入（那是聊天滚动），
    /// 因此输入区只能靠 Home/Left 回到行首 —— 行为必须自洽。
    #[test]
    fn test_up_down_do_not_touch_input_cursor() {
        let ch = channels();
        let mut a = app();
        for i in 0..80 {
            a.transcript.push(TranscriptLine::User(format!("msg-{i}")));
        }
        a.input.insert_str("ab\ncd");
        let before = a.input.cursor;

        handle_key(press(KeyCode::Up), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        handle_key(
            press(KeyCode::Down),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, before, "上下键只滚聊天");
    }

    /// 既有测试的补充：`End` 之后 `Left` 仍作用于输入 —— 两个键不互相串味。
    #[test]
    fn test_end_then_left_keeps_input_cursor_semantics() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("xy");
        handle_key(press(KeyCode::End), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(a.input.cursor, 2, "End 不重置输入光标");
        handle_key(
            press(KeyCode::Left),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 1);
    }

    #[test]
    fn test_release_events_ignored() {
        let ch = channels();
        let mut a = app();
        let mut key = press(KeyCode::Enter);
        key.kind = KeyEventKind::Release;
        a.input.insert_str("x");
        handle_key(key, &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(!a.is_turning, "Release 不触发提交");
        assert!(ch.next_request().is_none());
    }

    // -----------------------------------------------------------------------
    // 补全浮层（T10 / T11）
    // -----------------------------------------------------------------------

    fn key(a: &mut App, ch: &TestChannels, code: KeyCode) {
        handle_key(press(code), a, &ch.req_tx, &ch.bnd_tx, 120, 40);
    }

    #[test]
    fn test_tab_opens_completion_for_slash_input() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/mo");
        key(&mut a, &ch, KeyCode::Tab);

        let state = a.completion.as_ref().expect("Tab 必须开浮层");
        assert_eq!(state.items.len(), 1, "只有 /model 命中");
        assert_eq!(state.items[0].replacement, "/model ");
        assert_eq!(state.selected, 0);
        assert!(ch.next_request().is_none(), "开浮层不发请求");
    }

    /// Tab 在**不该开**的两种情况下必须安静（非 `/` 开头、无命中）。
    #[test]
    fn test_tab_is_noop_without_slash_or_without_match() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("hello");
        key(&mut a, &ch, KeyCode::Tab);
        assert!(a.completion.is_none(), "普通输入不补全");

        a.input.clear();
        a.input.insert_str("/zzz");
        key(&mut a, &ch, KeyCode::Tab);
        assert!(a.completion.is_none(), "无命中不开空浮层");
        assert_eq!(a.input.text, "/zzz", "Tab 不改输入内容");
    }

    #[test]
    fn test_completion_arrows_move_highlight_and_wrap() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/");
        key(&mut a, &ch, KeyCode::Tab);
        let n = a.completion.as_ref().unwrap().items.len();

        key(&mut a, &ch, KeyCode::Down);
        assert_eq!(a.completion.as_ref().unwrap().selected, 1);
        key(&mut a, &ch, KeyCode::Up);
        assert_eq!(a.completion.as_ref().unwrap().selected, 0);
        key(&mut a, &ch, KeyCode::Up);
        assert_eq!(a.completion.as_ref().unwrap().selected, n - 1, "到头回卷");
    }

    /// 浮层里的 `Up`/`Down` **不得**穿透去滚聊天（设计 §2.4：焦点在浮层）。
    #[test]
    fn test_completion_arrows_do_not_scroll_chat() {
        let ch = channels();
        let mut a = app();
        for i in 0..80 {
            a.transcript.push(TranscriptLine::User(format!("msg-{i}")));
        }
        a.input.insert_str("/");
        key(&mut a, &ch, KeyCode::Tab);
        key(&mut a, &ch, KeyCode::Up);
        assert_eq!(a.scroll_offset, 0, "上下键被浮层吃掉，聊天不动");
        assert!(a.follow);
    }

    /// Tab/Enter 都「接受当前项」：填入 `/{name} ` 并关闭浮层，**绝不提交**。
    #[test]
    fn test_completion_enter_and_tab_fill_replacement_without_submitting() {
        for accept in [KeyCode::Tab, KeyCode::Enter] {
            let ch = channels();
            let mut a = app();
            a.input.insert_str("/mo");
            key(&mut a, &ch, KeyCode::Tab);
            key(&mut a, &ch, accept);

            assert_eq!(a.input.text, "/model ", "{accept:?} 填入 replacement");
            assert_eq!(a.input.cursor, 7, "光标到填入文本末尾");
            assert!(a.completion.is_none(), "{accept:?} 填完即关浮层");
            assert!(!a.is_turning, "填入不是提交");
            assert!(ch.next_request().is_none());
        }
    }

    /// 选中第二项后填入的必须是第二项（不是第 0 项）。
    #[test]
    fn test_completion_fills_the_highlighted_item() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/");
        key(&mut a, &ch, KeyCode::Tab);
        key(&mut a, &ch, KeyCode::Down); // → /status
        key(&mut a, &ch, KeyCode::Enter);
        assert_eq!(a.input.text, "/status ");
    }

    #[test]
    fn test_completion_esc_closes_and_keeps_input() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/mo");
        key(&mut a, &ch, KeyCode::Tab);
        key(&mut a, &ch, KeyCode::Esc);
        assert!(a.completion.is_none(), "Esc 关浮层");
        assert_eq!(a.input.text, "/mo", "输入内容原样保留（Esc 不吞键入）");
        assert!(ch.next_request().is_none());
    }

    /// **其他键只关浮层、不吞键**：这一下必须继续按正常键处理（进输入框）。
    ///
    /// 变异：让 `_` 分支也 `return true`（吞掉按键）→ 本测试变红。
    #[test]
    fn test_completion_other_key_closes_overlay_and_still_types() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/");
        key(&mut a, &ch, KeyCode::Tab);
        key(&mut a, &ch, KeyCode::Char('x'));

        assert!(a.completion.is_none(), "其他键关闭浮层");
        assert_eq!(a.input.text, "/x", "这一下仍然进了输入框（没被吞）");
    }

    // -----------------------------------------------------------------------
    // 命令分发（T10 / T11）
    // -----------------------------------------------------------------------

    /// `/help` 等本地动作：push 一条 System，**一个字节都不发给 app**。
    #[test]
    fn test_local_commands_write_transcript_and_send_nothing() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/help");
        key(&mut a, &ch, KeyCode::Enter);

        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("可用命令")),
            "{:?}",
            a.transcript
        );
        assert!(!a.is_turning, "本地命令不进 turn 态");
        assert_eq!(a.suppress_user_echo, 0, "本地命令没有消息要抑制回显");
        assert!(ch.next_request().is_none());
        assert!(a.input.text.is_empty(), "提交后清空输入框");
    }

    #[test]
    fn test_quit_command_sets_should_quit() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/quit");
        key(&mut a, &ch, KeyCode::Enter);
        assert!(a.should_quit);
        assert!(ch.next_request().is_none());
    }

    /// `/model gpt-4o` → 直发 `SetModel`（**不进浮层**）。
    #[test]
    fn test_model_with_argument_sends_set_model() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/model gpt-4o");
        key(&mut a, &ch, KeyCode::Enter);

        assert_eq!(
            ch.next_request().unwrap(),
            Request::SetModel {
                model: "gpt-4o".into()
            }
        );
        assert!(a.pending_prompt.is_none(), "有参不走浮层");
        assert!(!a.is_turning, "命令不是回合");
        assert!(a.transcript.is_empty(), "命令不改 transcript");
    }

    /// `/model` 无参 → **记下意图交给 run.rs**，此刻不发任何请求。
    #[test]
    fn test_model_without_argument_defers_to_modal_prompt() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/model");
        key(&mut a, &ch, KeyCode::Enter);

        assert_eq!(
            a.pending_prompt,
            Some(crate::commands::PromptKind::PickModel)
        );
        assert!(ch.next_request().is_none(), "浮层结果出来前不发请求");
        assert!(a.input.text.is_empty());
    }

    #[test]
    fn test_login_defers_to_modal_prompt() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/login");
        key(&mut a, &ch, KeyCode::Enter);
        assert_eq!(a.pending_prompt, Some(crate::commands::PromptKind::Login));
        assert!(ch.next_request().is_none());
    }

    /// `/logout` `/new` `/compact` `/export` 直发对应变体。
    /// `/export` 无参必须发 `path: None`（**默认落点由 app 侧决定**，
    /// TUI 不替 app 猜路径）。
    #[test]
    fn test_direct_request_commands_send_expected_variants() {
        for (input, expected) in [
            ("/logout", Request::Logout),
            ("/new", Request::NewSession),
            ("/compact", Request::Compact),
            ("/export", Request::Export { path: None }),
            (
                "/export a.jsonl",
                Request::Export {
                    path: Some(std::path::PathBuf::from("a.jsonl")),
                },
            ),
        ] {
            let ch = channels();
            let mut a = app();
            a.input.insert_str(input);
            key(&mut a, &ch, KeyCode::Enter);
            assert_eq!(ch.next_request().unwrap(), expected, "输入 `{input}`");
            assert!(a.pending_prompt.is_none());
        }
    }

    /// [`Action::Abort`] 是**保留**变体：`parse` 目前不产出它（Ctrl-C/Esc 在
    /// `handle_key` 里直接投 `Boundary::Abort`），但「命令层可请求中止」这条
    /// 路径必须仍然通畅。这里**直接构造**该动作，断言分派确实投出
    /// `Boundary::Abort` —— 让它即使没有生产者也不再是死代码。
    ///
    /// 变异：把 `Action::Abort` 分支改成 `{}`（吞掉）→ 本测试变红。
    #[test]
    fn test_abort_action_still_routes_to_boundary() {
        let ch = channels();
        let mut a = app();
        dispatch(&mut a, &ch.req_tx, &ch.bnd_tx, Action::Abort);

        assert_eq!(ch.next_boundary().unwrap(), Boundary::Abort);
        assert!(ch.next_request().is_none(), "Abort 不发 Request");
        assert!(!a.should_quit, "Abort 是取消，不是退出");
        assert!(a.transcript.is_empty(), "Abort 不动 transcript");
    }

    /// `/thinking on` 当场改状态（本地动作，不发请求）。
    #[test]
    fn test_thinking_command_toggles_local_state() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/thinking on");
        key(&mut a, &ch, KeyCode::Enter);
        assert!(a.thinking_visible);
        assert!(ch.next_request().is_none());

        a.input.insert_str("/thinking");
        key(&mut a, &ch, KeyCode::Enter);
        assert!(!a.thinking_visible, "无参 = toggle");
        assert!(ch.next_request().is_none());
    }

    /// 未知命令：本地提示 + **不发请求** + 不误当普通消息提交。
    #[test]
    fn test_unknown_command_is_hinted_not_submitted() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/nope");
        key(&mut a, &ch, KeyCode::Enter);

        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("未知命令：/nope")),
            "{:?}",
            a.transcript
        );
        assert!(!a.is_turning);
        assert!(ch.next_request().is_none(), "未知命令绝不发给模型");
    }
}
