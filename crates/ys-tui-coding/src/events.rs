//! crossterm 事件 → [`App`] 状态迁移 + 对 app 线程的请求（`Request` / `Boundary`）。
//!
//! 键位集：**Enter 提交或填入补全（`Shift`/`Alt` + Enter 换行，`Ctrl-J` 亦可，见下）/
//! Ctrl-C 两段式（取消 → 退出）· Ctrl-D 退出 / Esc 取消或清空输入 / 上下翻页滚动 /
//! 左右与 Home·Delete 归输入光标（`Ctrl`·`⌥` + 左右 = 按词移动）/ Ctrl-A·Ctrl-E
//! 输入行首·行尾 / Tab 开补全浮层**。
//!
//! # 换行键：为什么除了 `Shift`/`Alt` + Enter 还要 `Ctrl-J`
//!
//! 输入区多行编辑要一个「换行但不提交」的键。三个候选的**可移植性**不同：
//!
//! | 键 | 终端发出的字节 | 依赖 |
//! |---|---|---|
//! | `Enter` | `\r` | —（提交） |
//! | `Shift` + `Enter` | 支持键盘增强协议时是 `CSI 13;2u`；**不支持时与 `Enter` 同为 `\r`** | 终端 |
//! | `Alt` + `Enter` | `\x1b\r`（Alt = ESC 前缀） | 几乎所有终端 |
//! | `Ctrl-J` | `\x0a`（终止控制符；raw 下 crossterm 解成 `Char('j')+CONTROL`） | 无 |
//!
//! `Shift` + `Enter` **不是**可依赖的键：它要求终端实现 Kitty 键盘增强协议；Termux 等
//! 终端**根本不实现**（`KeyHandler` 把 `Shift+Enter` 与 `Enter` 都映射成 `\r`），应用拿到的
//! 字节一模一样，无从区分。故 `Alt` + `Enter` 是终端无关的换行键，`Ctrl-J` 是普适兜底
//! （终止控制符，任何终端都能发）。三者都插入换行、都不提交。
//!
//! # 命令分发（T10）
//!
//! Enter 时把输入交给 [`crate::commands::parse_with_skills`]（内置命令 + skill 名），
//! 按 [`crate::commands::Action`] 分派：
//! 本地动作当场执行（写 transcript），`Request` 投给 app 线程。**唯一需要模态浮层的
//! 是 [`Action::Prompt`]** —— 它要画屏，而画屏得握 `Terminal`，故这里只把意图记进
//! [`App::pending_prompt`]，由 `run.rs` 的事件循环取走并跑模态循环（设计 §3
//!「TUI 自己的浮层选择器」）。本地动作默认**不发请求**；`/thinking` 是唯一例外 ——
//! 它本地立即生效后，再把 UI 运行期真值经 [`Request::SetThinking`] **单向**同步给
//! app 作持久化镜像（设计 §5.1 / §5.6）。
//!
//! # 方向键的取舍：上下归聊天、左右归输入
//!
//! 方向键只有 4 个，却要服务两个焦点（聊天滚动 + 输入光标）。本模块的划分：
//!
//! | 键 | 归属 |
//! |---|---|
//! | `Up` / `Down` / `PageUp` / `PageDown` | **聊天滚动**（`App::scroll_offset`） |
//! | `Left` / `Right` / `Delete` / `Home` | **输入光标**（`InputBuffer`） |
//! | `Ctrl`·`⌥` + `Left` / `Right`（或 `⌥b` / `⌥f`） | **输入按词移动** |
//! | `Ctrl-A` / `Ctrl-E` | **输入行首 / 行尾** |
//! | `End` | **回到最新消息**（`scroll_offset = 0`、`follow = true`） |
//!
//! 即：竖直方向 = 聊天，水平方向 = 输入。`Home`/`End` 这一对**不做对称绑定** ——
//! `End` 早在 T5 就钉住了「跳回聊天底部」的语义（有测试），`Home` 之前空着，
//! 于是给输入行首。理由：聊天滚动已有 4 个键（且退回底部是高频操作），而输入
//! 行首此前**无键可用**（`Home` 是唯一自然的候选）。输入的「行尾」因此**不占 `End`**：
//! 走 `Ctrl-E`，行首走 `Ctrl-A` —— 成对且是 readline 约定。
//!
//! **`⌘` 组合到不了应用**：终端自己消费 `⌘`，从不把它交给程序，所以 ⌘←/⌘→ 只能靠
//! 终端把它们映射成别的字节。Terminal.app 与 iTerm2 的出厂映射恰好是 `Ctrl-A` /
//! `Ctrl-E` —— 绑这两个即等价于「⌘ + 左右」可用。按词移动另给 `Ctrl`/`⌥` + 左右
//! 与 `⌥b`/`⌥f`（后者是 Terminal.app 映射 ⌥← 的形态）。
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
                    // 两段式：第一次请求取消，第二次退出。
                    //
                    // 只做「取消」是不够的 —— `Boundary::Abort` 是消息，`BasicLoop` 只在
                    // 轮边界（与工具轮询）里读它，**模型调用期间无人读**。于是一次挂住的
                    // 模型请求背后，Ctrl-C 按下去毫无反应：UI 不动、进程不退，用户手里
                    // 就没有杀死本进程的键了（默认流空闲超时 120s × 有界重试，窗口以分钟计）。
                    // 第二段让「按到不动就再按一次」成为可靠的退出路径，且不夺走第一段的取消语义。
                    if app.abort_requested {
                        app.should_quit = true;
                    } else {
                        app.abort_requested = true;
                        request_abort(boundaries);
                        app.transcript
                            .push(crate::transcript::TranscriptLine::System(
                                "已请求取消当前回合；若长时间无响应，再按一次 Ctrl-C 退出".into(),
                            ));
                        app.follow = true;
                    }
                } else {
                    app.should_quit = true;
                }
                return;
            }
            KeyCode::Char('d') => {
                app.should_quit = true;
                return;
            }
            // 行首 / 行尾（readline 约定）。macOS 上 ⌘←/⌘→ **默认就发这两个控制
            // 字符**（Terminal.app / iTerm2 的出厂键映射），而 ⌘ 本身从不交给应用
            // —— 绑这里等价于让「⌘ + 左右」可用。`End` 不动（它归「回到最新消息」，
            // 见模块文档的取舍表）。
            KeyCode::Char('a') => {
                app.input.cursor_home();
                return;
            }
            KeyCode::Char('e') => {
                app.input.cursor_end();
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => {
            // Esc = **全局取消**：回合中取消回合，命令在飞时取消命令（`/login` 的网络
            // 往返最长 5s；不取消就只能 Ctrl-C 把整个进程关掉）；两者都不是才后退到
            // 「清空输入」这条 idle 语义。在飞期间**不动输入框**：用户可能在等的时候
            // 已经在敲下一条消息，丢掉它比保留更意外。
            if app.is_turning || app.command_in_flight {
                request_abort(boundaries);
            } else {
                app.input.clear();
            }
        }
        // Shift/Alt+Enter 换行（Alt 是 macOS Terminal.app 的兜底，设计 §2.3）
        KeyCode::Enter if shift || alt => app.input.insert_char('\n'),
        KeyCode::Enter => submit(app, requests, boundaries),
        // Ctrl+J 换行。不支持键盘增强协议的终端（Termux 等）里 `Shift+Enter` 与 `Enter`
        // 发**同一个字节** `\r`，无法区分 —— 故补一个通用的「不提交换行」键。raw 模式下
        // crossterm 把终止控制符 0x0A 解成 `Char('j') + CONTROL`（实测，非 `Char('\n')`）。
        KeyCode::Char('j') if ctrl => app.input.insert_char('\n'),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete_forward(),
        // 左右归**输入光标**（上下归聊天滚动 —— 见模块文档的取舍表）。
        // 带 Ctrl / Alt 时**按词**移动：macOS 的 ⌥←/⌥→ 与多数终端的 Ctrl+←/Ctrl+→
        // 都落到这一支（终端各自发 `ESC[1;5D` 或 `ESC b` 这类序列，crossterm 解出
        // 的就是 Left/Right + 修饰键）。终端的键映射若把 ⌘← 发成别的，见 Ctrl-A/E。
        KeyCode::Left if ctrl || alt => app.input.cursor_word_left(),
        KeyCode::Right if ctrl || alt => app.input.cursor_word_right(),
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
        // Tab：输入 `/` 开头时按前缀开补全浮层（无命中 / 不以 `/` 开头 = no-op）。
        // 候选含 skill（与 `submit` 的解析同源，见模块文档）。
        KeyCode::Tab => {
            if let Some(state) = completion::open_with_skills(&app.input.text, &app.view.skills) {
                app.completion = Some(state);
            }
        }
        // ⌥b / ⌥f：readline 的词移动别名。macOS Terminal.app 若把 ⌥← 映射成
        // `ESC b`，到这里就是 `Char('b') + ALT` —— 与上面的 `Left + ALT` 同源但不同形。
        KeyCode::Char('b') if alt => app.input.cursor_word_left(),
        KeyCode::Char('f') if alt => app.input.cursor_word_right(),
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
                // 只替换**开头那个命令词**，保留其后正文（正文往往是 skill 的参数）。
                app.input.complete_leading_token(&text);
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

/// 提交输入：命令分发（[`commands::parse_with_skills`]，含 skill 名）或普通消息。
fn submit(app: &mut App, requests: &Sender<Request>, boundaries: &Sender<Boundary>) {
    let text = app.input.take();
    app.completion = None;
    if text.trim().is_empty() {
        return;
    }
    // 先把 `Action` 算出来（只读借 `app.view.skills`）再 dispatch（可变借 `app`）
    let action = commands::parse_with_skills(&text, &app.view.skills);
    dispatch(app, requests, boundaries, action);
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
        Action::Local(local) => {
            let is_thinking = matches!(local, commands::LocalAction::Thinking(_));
            commands::apply_local(app, local);
            // `/thinking` 是本地动作的唯一例外：先本地立即生效（`apply_local` 已改写
            // `app.thinking_visible`），再把改写后的 UI 真值单向同步给 app 持久化。
            // 其余本地动作不发任何请求。
            if is_thinking {
                send_request(
                    app,
                    requests,
                    Request::SetThinking {
                        visible: app.thinking_visible,
                    },
                );
            }
        }
        // 需要终端的模态浮层：交给 run.rs 的事件循环（它握着 Terminal）
        Action::Prompt(kind) => app.pending_prompt = Some(kind),
    }
}

/// 投一条请求给 app 线程（信道 ①）。
fn send_request(app: &mut App, requests: &Sender<Request>, req: Request) {
    app.note_request_sent(&req);
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
    use crate::view::{CodingView, SkillEntry};
    use tokio::sync::mpsc;
    use ys_core::{ContentBlock, EffortLevel, Role};
    use ys_protocol::Outbound;

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

    /// Ctrl+J（终止控制符 0x0A）：raw 模式下 crossterm 解成 `Char('j') + CONTROL`
    /// （实测，**不是** `Char('\n')`）。这是**不支持键盘增强协议的终端**（如 Termux）
    /// 里唯一可用的「不提交换行」键 —— 那些终端的 `Shift+Enter` 与 `Enter` 发同一个
    /// 字节 `\r`，无法区分（见模块文档的键位说明）。
    ///
    /// 变异：删掉处理里的 `KeyCode::Char('j') if ctrl` 臂 ⇒ Ctrl+J 变 no-op ⇒ 本用例红。
    #[test]
    fn test_ctrl_j_inserts_newline_not_submit() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("a");
        handle_key(
            with_mod(KeyCode::Char('j'), KeyModifiers::CONTROL),
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

    /// **回归（每次都必须能杀死进程）**：turn 中 **第二次** Ctrl-C 必须退出。
    ///
    /// 取消是**消息**（`Boundary::Abort` 只在轮边界 / 工具轮询里被读到），而一次
    /// 模型调用期间无人读它 —— 于是「turn 中 Ctrl-C 只取消」在真实终端里表现为
    /// **按了毫无反应**：UI 不动、进程不退，用户唯一的手动退出键失效。
    /// 本用例钉住第二段语义：第一次请求取消，第二次直接退出（Ctrl-C 永远能杀死本进程）。
    #[test]
    fn test_ctrl_c_second_press_quits_while_turning() {
        let ch = channels();
        let mut a = app();
        a.is_turning = true;
        let ctrl_c = || with_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);

        handle_key(ctrl_c(), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(!a.should_quit, "第一次仍是取消请求");
        assert_eq!(ch.next_boundary().unwrap(), Boundary::Abort);

        handle_key(ctrl_c(), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(
            a.should_quit,
            "第二次 Ctrl-C 必须退出 —— 否则没有杀死进程的手段"
        );
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

    /// **回归（命令在飞时 Esc 要能取消）**：`/login` 这类命令会做一次网络往返
    /// （`/v1/models`，最长 5s，TCP 挂起时更久），而命令不经过 `BasicLoop` —— 没有任何
    /// 人会去读 `Boundary`。若 Esc 只认 `is_turning`，这段时间里按 Esc 就毫无反应，
    /// 用户只能 Ctrl-C 关掉整个进程。本用例钉住：在飞态的 Esc 也要投 `Abort`。
    ///
    /// 变异：把 Esc 的判断改回只看 `is_turning` ⇒ 第一个断言变红。
    #[test]
    fn test_esc_sends_abort_while_command_in_flight() {
        let ch = channels();
        let mut a = app();

        a.input.insert_str("/model deepseek/chat");
        handle_key(press(KeyCode::Enter), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(!a.is_turning, "命令请求不开回合态");
        assert!(a.command_in_flight, "命令请求投出去即进入在飞态");
        assert!(matches!(ch.next_request(), Some(Request::SetModel { .. })));

        // 等的时候用户已经开始敲下一条：Esc 只取消命令，不该把它清掉。
        a.input.insert_str("第二条");
        handle_key(press(KeyCode::Esc), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(
            ch.next_boundary().unwrap(),
            Boundary::Abort,
            "在飞态的 Esc 必须投 Abort，否则命令无法取消"
        );
        assert_eq!(a.input.text, "第二条", "取消命令不该清掉用户新敲的字");
    }

    /// 在飞态由**任意一条出站消息**清除：`app_loop` 处理完每个请求都会刷一条
    /// `Outbound::View`，故命令收场后 Esc 必须退回「清空输入」的 idle 语义。
    ///
    /// 变异：漏掉 `apply_outbound` 里的清位 ⇒ 完成后 Esc 仍投 Abort（且不再清输入）。
    #[test]
    fn test_command_in_flight_cleared_by_any_outbound() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/logout");
        handle_key(press(KeyCode::Enter), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(a.command_in_flight);

        a.apply_outbound(Outbound::View(CodingView::for_test()));
        assert!(!a.command_in_flight, "出站消息即视为命令收场");

        a.input.insert_str("/mo");
        handle_key(press(KeyCode::Esc), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(ch.next_boundary().is_none(), "收场后的 Esc 不再投 Abort");
        assert!(a.input.text.is_empty(), "收场后的 Esc 回到清空输入");
    }

    /// `Request::Prompt` **不**进入在飞态：回合有自己的取消路径（`is_turning`）。
    #[test]
    fn test_prompt_request_does_not_enter_command_in_flight() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("你好");
        handle_key(press(KeyCode::Enter), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert!(a.is_turning);
        assert!(!a.command_in_flight, "回合请求归 is_turning 管");
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
        assert_eq!(input_height(&a, 120), 3, "空输入：1 行内容 + 2 边框");
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
        assert_eq!(input_height(&a, 120), 5, "粘贴 3 行 → 输入区撑高到 5");
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

    /// Ctrl/⌥ + 左右 = **按词**移动；Ctrl-A / Ctrl-E = 输入行首 / 行尾。
    ///
    /// **变异**：把 `Left`/`Right` 的 `if ctrl || alt` 守卫删掉（回到只看键码）
    /// ⇒ 下面的按词断言全变红（光标只在原地动一个 char）。
    #[test]
    fn test_word_and_line_motion_keys() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("foo bar");
        assert_eq!(a.input.cursor, 7, "insert_str 后光标在末尾");

        // 按词左移的三个来源：⌥← / Ctrl+← / ⌥b（Terminal.app 把 ⌥← 发成 `ESC b`）。
        // 每个来源都从同一位置起步 —— 「按词移动」不幂等（已在词首时会走到上一个词）。
        for key in [
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
        ] {
            a.input.cursor = 7;
            handle_key(key, &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
            assert_eq!(a.input.cursor, 4, "按词左移：越过 bar，停在空格之后");
        }
        // 按词右移：⌥→ / Ctrl+→ / ⌥f
        for key in [
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
        ] {
            a.input.cursor = 4;
            handle_key(key, &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
            assert_eq!(a.input.cursor, 7, "按词右移：越过 bar 到行尾");
        }

        // Ctrl-E / Ctrl-A：行尾 / 行首 —— macOS 的 ⌘→ / ⌘← 出厂就发这两个控制字符
        a.input.cursor = 2;
        handle_key(
            with_mod(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 7, "Ctrl-E 到行尾");
        handle_key(
            with_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 0, "Ctrl-A 到行首");
        assert_eq!(a.input.text, "foo bar", "移动不修改文本");

        // `End` 的语义**不变**（回到最新消息）—— 行尾走 Ctrl-E，不抢 `End`
        a.follow = false;
        a.scroll_offset = 5;
        handle_key(press(KeyCode::End), &mut a, &ch.req_tx, &ch.bnd_tx, 120, 40);
        assert_eq!(a.scroll_offset, 0, "End 仍回聊天底部");
        assert!(a.follow);
        assert_eq!(a.input.cursor, 0, "End 不动输入光标");
    }

    /// 多行输入下 Ctrl-A / Ctrl-E 只到**本行**首尾（与 `cursor_home`/`cursor_end` 同语义，
    /// 不是缓冲首尾）。
    #[test]
    fn test_ctrl_a_e_are_line_scoped_in_multiline_input() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("aa\nbbbb\ncc");
        a.input.cursor = 9; // 第三行「cc」的第二个字符之后

        handle_key(
            with_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 8, "Ctrl-A 到本行行首，不是缓冲开头");

        handle_key(
            with_mod(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &mut a,
            &ch.req_tx,
            &ch.bnd_tx,
            120,
            40,
        );
        assert_eq!(a.input.cursor, 10, "Ctrl-E 到本行行尾（末行即缓冲末尾）");
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

    /// 回归：先把正文敲好、再跳到行首补命令前缀，**接受补全不得吃掉正文**。
    ///
    /// 用户的正文既是 skill 的参数（`/commit 看看代码`），也可能只是命令后的任意
    /// 文本。补全只该替换**开头那个命令词**，其余原样保留。
    ///
    /// 变异：把接受分支改回整段覆写（`app.input.text = replacement`）⇒ 正文丢失，
    /// 本测试变红。
    #[test]
    fn test_completion_accept_keeps_text_after_command() {
        let ch = channels();
        let mut a = app();
        // 先敲正文
        a.input.insert_str("看看代码");
        // 跳到行首补 skill 前缀 + 空格（光标停在「前缀 + 空格」之后、正文之前）
        a.input.cursor = 0;
        a.input.insert_str("/comm ");
        assert_eq!(a.input.text, "/comm 看看代码");
        assert_eq!(a.input.cursor, 6);

        key(&mut a, &ch, KeyCode::Tab);
        assert!(a.completion.is_some(), "skill 前缀命中即开浮层");
        key(&mut a, &ch, KeyCode::Enter);

        assert_eq!(a.input.text, "/commit 看看代码", "正文必须原样保留");
        assert_eq!(a.input.cursor, 8, "光标停在命令词与正文之间");
        assert!(a.completion.is_none(), "接受后关浮层");
        assert!(!a.is_turning, "接受补全不是提交");
        assert!(ch.next_request().is_none());
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
                model: "gpt-4o".into(),
                provider: None,
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

    /// `/skills` 无参 → **记下意图交给 run.rs**（开选择器），此刻不发任何请求。
    /// 选中后是**回填输入框**而非发 `Request` —— 故这条路径上永远不该有请求。
    #[test]
    fn test_skills_defers_to_modal_prompt_without_request() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/skills");
        key(&mut a, &ch, KeyCode::Enter);

        assert_eq!(
            a.pending_prompt,
            Some(crate::commands::PromptKind::PickSkill)
        );
        assert!(ch.next_request().is_none(), "浮层结果出来前不发请求");
        assert!(a.input.text.is_empty());
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

    /// `/thinking on` 当场改状态（本地动作），并把改写后的真值经
    /// `SetThinking` **单向**同步给 app —— 只发这一条，不发别的请求。
    #[test]
    fn test_thinking_command_toggles_local_state() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/thinking on");
        key(&mut a, &ch, KeyCode::Enter);
        assert!(a.thinking_visible);
        assert_eq!(
            ch.next_request().unwrap(),
            Request::SetThinking { visible: true },
            "本地立即生效后回传一份给 app"
        );
        assert!(ch.next_request().is_none(), "只回传一条 SetThinking");

        a.input.insert_str("/thinking");
        key(&mut a, &ch, KeyCode::Enter);
        assert!(!a.thinking_visible, "无参 = toggle");
        assert_eq!(
            ch.next_request().unwrap(),
            Request::SetThinking { visible: false }
        );
        assert!(ch.next_request().is_none());
    }

    /// `/effort <level>` → 直发 `SetEffort`；`default` → `effort: None`。
    /// **不进 transcript**（档位真值由 app 侧快照推回来，命令本身没有本地回显）。
    ///
    /// 变异：把 `/effort` 接进 `/thinking` 那条「本地写一行 + 镜像请求」的路径
    /// ⇒ 下面的 `transcript.is_empty` 与「只发一条」断言变红。
    #[test]
    fn test_effort_with_argument_sends_set_effort_only() {
        for (input, expected) in [
            (
                "/effort high",
                Request::SetEffort {
                    effort: Some(EffortLevel::High),
                },
            ),
            ("/effort default", Request::SetEffort { effort: None }),
        ] {
            let ch = channels();
            let mut a = app();
            a.input.insert_str(input);
            key(&mut a, &ch, KeyCode::Enter);

            assert_eq!(ch.next_request().unwrap(), expected, "输入 `{input}`");
            assert!(ch.next_request().is_none(), "只发一条 SetEffort：`{input}`");
            assert!(a.transcript.is_empty(), "命令不改 transcript：`{input}`");
            assert!(!a.is_turning, "命令不是回合：`{input}`");
            assert!(a.pending_prompt.is_none(), "`{input}` 不走浮层");
        }
    }

    /// 无参 `/effort` 是**纯本地**动作：恰好一条 System 行（当前档位 + 用法），
    /// 一个请求都不发。
    ///
    /// 变异：让 `Action::Local` 下也替 `Effort` 镜像一条 Request ⇒ 本测试变红。
    #[test]
    fn test_bare_effort_writes_one_local_line_and_sends_nothing() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/effort");
        key(&mut a, &ch, KeyCode::Enter);

        assert_eq!(a.transcript.len(), 1, "只写一条：{:?}", a.transcript);
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.starts_with("effort: default")),
            "{:?}",
            a.transcript
        );
        assert!(ch.next_request().is_none(), "无参 /effort 不发任何请求");
        assert!(!a.is_turning);
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

    // -----------------------------------------------------------------------
    // skill 通路（`/skill-name` → 原文直发 app，展开归 app 侧）
    // -----------------------------------------------------------------------

    /// 命中 skill ⇒ **原文（含参数）**作为 `Request::Prompt` 发出，回显的也是原文。
    ///
    /// 变异：`submit` 改回 `commands::parse(&text)`（不传 skills）⇒ 本测试变红
    /// （会退化成「未知命令」本地提示，一个请求都不发）。
    #[test]
    fn test_skill_name_submits_original_text_as_prompt() {
        let ch = channels();
        let mut a = app(); // `for_test` 视图带 commit / review 两条 skill
        a.input.insert_str("/commit 帮我写一条");
        key(&mut a, &ch, KeyCode::Enter);

        match ch.next_request().expect("skill 命中应发 Prompt") {
            Request::Prompt(msg) => assert!(
                matches!(&msg.content[0], ContentBlock::Text { text } if text == "/commit 帮我写一条"),
                "{msg:?}"
            ),
            other => panic!("expected Prompt, got {other:?}"),
        }
        assert!(a.is_turning, "skill 调用是一个回合");
        assert!(
            matches!(&a.transcript[0], TranscriptLine::User(s) if s == "/commit 帮我写一条"),
            "transcript 回显原文（展开在 app 侧）：{:?}",
            a.transcript
        );
    }

    /// **内置命令 > skill 穿过整个事件层**：视图里塞一条同名 skill，`/status` 仍走本地。
    ///
    /// 变异：把 skill 判定提到内置表之前 ⇒ 本测试变红（`/status` 会被当 skill 发出去）。
    #[test]
    fn test_builtin_still_wins_when_view_has_same_named_skill() {
        let ch = channels();
        let mut a = app();
        a.view.skills.push(SkillEntry {
            command: "status".into(),
            label: "Fake Status".into(),
            description: "想遮蔽 /status".into(),
        });
        a.input.insert_str("/status");
        key(&mut a, &ch, KeyCode::Enter);

        assert!(ch.next_request().is_none(), "内置命令不得被 skill 顶掉");
        assert!(!a.is_turning);
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("provider")),
            "{:?}",
            a.transcript
        );
    }

    /// Tab 补全的候选含 skill（生产接线是 `open_with_skills`）。
    ///
    /// 变异：把 Tab 分支改回 `completion::open(..)` ⇒ 浮层开不出来 → 本测试变红。
    #[test]
    fn test_tab_completion_includes_skills() {
        let ch = channels();
        let mut a = app();
        a.input.insert_str("/comm"); // 内置表里没有 /comm* —— 只有 skill `commit` 命中
        key(&mut a, &ch, KeyCode::Tab);

        let state = a.completion.as_ref().expect("skill 命中即开浮层");
        assert_eq!(state.items.len(), 1, "{:?}", state.items);
        assert_eq!(state.items[0].replacement, "/commit ");
        assert!(ch.next_request().is_none(), "开浮层不发请求");
    }
}
