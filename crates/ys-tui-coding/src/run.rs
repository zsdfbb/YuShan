//! [`run`] —— UI 线程的同步阻塞事件循环（设计 §7 / §8）。
//!
//! # 线程契约（**必须遵守**）
//!
//! [`run`] **阻塞**在自己的循环里，并且用 `Sender::blocking_send` 向 app 线程投递
//! 请求 —— 因此**必须在非 async 线程调用**。它不能跑在 tokio worker 上，
//! 也**不认识 `Agent`**：agent 跑在另一个线程，二者只经三条信道说话。
//!
//! ```text
//! UI 线程 ──run()──► tx_req / tx_boundary ──► app 线程（agent）
//!         ◄────────── rx_out（Outbound<CodingView>）
//! ```
//!
//! # 循环形状
//!
//! ```text
//! loop {
//!   draw
//!   排空 rx_out（try_recv，非阻塞）
//!   poll(50ms) → 按键 / 粘贴
//!   300ms 节拍 → Working 动画前进（仅 turn 中）
//!   should_quit / app 线程消失 → 退出
//! }
//! ```
//!
//! 退出**无条件**恢复终端：把完整对话按当前宽度 wrap 后逐行写回主屏 scrollback，
//! 再离开 alt-screen（pi 语义 —— 退出后对话仍在终端历史里可翻）。

use std::fmt;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Receiver, Sender};
use ys_protocol::{Boundary, Outbound, Request};

use crate::app::App;
use crate::commands::PromptKind;
use crate::input::InputBuffer;
use crate::prompter::{PromptError, Prompter, resolve_prompt};
use crate::transcript::TranscriptLine;
use crate::view::CodingView;
use crate::{draw, events};

/// 等输入的时长上限（空闲重绘节拍）。
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Working 动画节拍。
const TICK_INTERVAL: Duration = Duration::from_millis(300);

/// TUI 初始化 / 渲染过程中的 IO 失败。
#[derive(Debug)]
pub enum TuiError {
    Io(std::io::Error),
}

impl fmt::Display for TuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TuiError::Io(e) => write!(f, "terminal io error: {e}"),
        }
    }
}

impl std::error::Error for TuiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TuiError::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for TuiError {
    fn from(e: std::io::Error) -> Self {
        TuiError::Io(e)
    }
}

/// 跑 TUI 主循环，直到用户退出或 app 线程消失。
///
/// # Panics / 前置条件
///
/// **不得在 async 运行时线程上调用** —— 内部用 `Sender::blocking_send`，
/// tokio 会在 async 上下文里 panic（`Cannot block the current thread from within
/// a runtime`）。正确用法是专用线程（如 `std::thread::spawn`）。
///
/// `outbound` 是 app → UI 的唯一输入流；其断开（`Disconnected`）表示 app 线程
/// 已收摊，UI 随之退出。
pub fn run(
    view: CodingView,
    outbound: Receiver<Outbound<CodingView>>,
    requests: Sender<Request>,
    boundaries: Sender<Boundary>,
) -> Result<(), TuiError> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new(view);

    let result = event_loop(&mut terminal, &mut app, outbound, &requests, &boundaries);

    // 恢复**无条件**发生 —— 即使循环出错，也不能把终端留在 raw mode + alt-screen。
    // best-effort：恢复自身的 IO 失败不再向上传播，避免掩盖 `event_loop` 的原始错误。
    restore_terminal(&mut terminal, &app);
    result
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Term, TuiError> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    if let Err(e) = execute!(
        out,
        EnterAlternateScreen,
        DisableLineWrap,
        EnableBracketedPaste
    ) {
        // 半途失败别把终端留在 raw mode
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    // 键盘增强协议：Shift/Alt+Enter 能区分出来。失败就降级（老终端不支持），不 panic。
    let _ = execute!(
        out,
        crossterm::event::PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    let terminal = Terminal::new(CrosstermBackend::new(out))?;
    Ok(terminal)
}

fn event_loop<B: Backend + Write + Send>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut outbound: Receiver<Outbound<CodingView>>,
    requests: &Sender<Request>,
    boundaries: &Sender<Boundary>,
) -> Result<(), TuiError> {
    let mut outbound_open = true;
    let mut last_tick = Instant::now();

    loop {
        let size = terminal.size()?;
        terminal.draw(|frame| draw::ui(frame, app))?;

        // 1) 排空 Outbound（非阻塞）—— 单一出站写者，事件/视图/输出保序
        if outbound_open {
            loop {
                match outbound.try_recv() {
                    Ok(out) => app.apply_outbound(out),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        outbound_open = false;
                        break;
                    }
                }
            }
        }

        // 2) 等输入，最多 POLL_INTERVAL
        if crossterm::event::poll(POLL_INTERVAL)? {
            let event = crossterm::event::read()?;
            events::handle_event(event, app, requests, boundaries, size.width, size.height);
            // `/login`、`/model` 无参要开模态浮层 —— 画屏需要 Terminal，
            // 而 `events.rs` 没有，于是它只留下意图，这里跑循环（设计 §3）。
            if let Some(kind) = app.pending_prompt.take() {
                run_prompt(
                    kind,
                    terminal,
                    app,
                    &mut outbound,
                    &mut outbound_open,
                    requests,
                );
            }
        }

        // 3) Working 动画节拍（只在 turn 中有意义）
        if app.is_turning && last_tick.elapsed() >= TICK_INTERVAL {
            app.tick_working();
            last_tick = Instant::now();
        }

        // 滚动位置按当前尺寸重新夹紧（窗口缩放后 wrap 行数会变）
        app.scroll_offset = app
            .scroll_offset
            .min(app.compute_max_scroll(size.width, size.height));

        if app.should_quit {
            break;
        }
        // app 线程消失且没有在跑的 turn → 没有对话对象了
        if !outbound_open && !app.is_turning {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 模态浮层（`Prompter` 的 TUI 实现）
// ---------------------------------------------------------------------------

/// 跑一条 `Action::Prompt` 命令：模态问答 → 一条 [`Request`]。
///
/// **取消不改任何状态** —— `Err(Cancelled)` 直接吞掉，transcript / 输入框 /
/// 模型配置都不动（半截登录比什么都不做更糟）。
fn run_prompt<B: Backend + Write + Send>(
    kind: PromptKind,
    terminal: &mut Terminal<B>,
    app: &mut App,
    outbound: &mut Receiver<Outbound<CodingView>>,
    outbound_open: &mut bool,
    requests: &Sender<Request>,
) {
    // 视图快照必须在把 `app` 借给 prompter **之前**取走（CodingView 是 Clone）
    let view = app.view.clone();
    let outcome = {
        let prompter = TuiPrompter::new(
            &mut *terminal,
            &mut *app,
            &mut *outbound,
            &mut *outbound_open,
        );
        resolve_prompt(kind, &prompter, &view)
    };

    match outcome {
        Ok(req) => {
            if requests.blocking_send(req).is_err() {
                app.should_quit = true;
            }
        }
        Err(PromptError::Cancelled) => {}
        Err(PromptError::Other(msg)) => {
            app.transcript
                .push(TranscriptLine::System(format!("命令失败：{msg}")));
            app.follow = true;
        }
    }
}

/// 模态浮层的绘制后端：三份借用打包在一起。
///
/// **为什么要 `Mutex`**：[`Prompter`] 的签名是 `&self` 且要求 `Send + Sync`，
/// 而画屏必须 `&mut Terminal` —— 二者只能靠内可变调和。UI 线程是单线程的，
/// 锁永远无人争用；它在这里只是「用类型系统能接受的方式表达内可变」。
///
/// **禁止在 `Prompter` 实现内部重入 `select` / `text`** —— `Mutex` 不可重入，
/// 重入即自锁死锁（同一个 `TuiPrompter` 的两个方法都去 `lock()` 同一把锁）。
/// [`resolve_prompt`] 是本类型的唯一调用方，它保证 `select` 与 `text` **串行**
/// （前一个返回后才问下一个），不会有嵌套问答。
struct ModalHost<'a, B: Backend + Write> {
    terminal: &'a mut Terminal<B>,
    app: &'a mut App,
    outbound: &'a mut Receiver<Outbound<CodingView>>,
    outbound_open: &'a mut bool,
}

/// [`Prompter`] 的 TUI 实现（`run.rs` 内，因为它握着 `Terminal`）。
pub(crate) struct TuiPrompter<'a, B: Backend + Write>(Mutex<ModalHost<'a, B>>);

impl<'a, B: Backend + Write> TuiPrompter<'a, B> {
    fn new(
        terminal: &'a mut Terminal<B>,
        app: &'a mut App,
        outbound: &'a mut Receiver<Outbound<CodingView>>,
        outbound_open: &'a mut bool,
    ) -> Self {
        Self(Mutex::new(ModalHost {
            terminal,
            app,
            outbound,
            outbound_open,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, ModalHost<'a, B>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<B: Backend + Write + Send> Prompter for TuiPrompter<'_, B> {
    fn select(
        &self,
        prompt: &str,
        options: Vec<String>,
        page_size: usize,
    ) -> Result<String, PromptError> {
        self.lock().select_modal(prompt, &options, page_size)
    }

    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError> {
        self.lock().text_modal(prompt, help)
    }
}

impl<B: Backend + Write> ModalHost<'_, B> {
    /// 模态打开期间继续排空 `Outbound`。
    ///
    /// 不排空的话，agent 在模态开着时吐事件会把有界信道灌满，把 app 线程堵在
    /// 发送端 —— 用户只是开了个选择器，不该让整条流水线停摆。
    /// 顺带：`Outbound::Quit` 在此时也照样生效（走 `should_quit`）。
    fn drain_outbound(&mut self) {
        if !*self.outbound_open {
            return;
        }
        loop {
            match self.outbound.try_recv() {
                Ok(out) => self.app.apply_outbound(out),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    *self.outbound_open = false;
                    break;
                }
            }
        }
    }

    /// 选项选择器：↑↓ 移动（回卷）、Enter 确认、Esc 取消。
    fn select_modal(
        &mut self,
        prompt: &str,
        options: &[String],
        page_size: usize,
    ) -> Result<String, PromptError> {
        if options.is_empty() {
            return Err(PromptError::Other(format!("{prompt}：没有可选项")));
        }
        let mut selected = 0usize;
        loop {
            self.drain_outbound();
            if !*self.outbound_open {
                // app 线程没了 —— 问谁都没意义
                return Err(PromptError::Cancelled);
            }
            // 借位拆开：`terminal.draw(|f| …)` 的闭包要同时拿 app 与 terminal
            let app = &mut *self.app;
            let terminal = &mut *self.terminal;
            terminal.draw(|f| {
                draw::ui_with_modal(
                    f,
                    app,
                    draw::Modal::Select {
                        prompt,
                        options,
                        selected,
                        page_size,
                    },
                )
            })?;

            if !crossterm::event::poll(POLL_INTERVAL)? {
                continue;
            }
            let Event::Key(key) = crossterm::event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up => selected = modal_move(selected, options.len(), true),
                KeyCode::Down => selected = modal_move(selected, options.len(), false),
                KeyCode::Enter => return Ok(options[selected].clone()),
                KeyCode::Esc => return Err(PromptError::Cancelled),
                _ => {}
            }
        }
    }

    /// 单行文本输入（`/login` 的 api_key）：Enter 确认、Esc 取消。
    fn text_modal(&mut self, prompt: &str, help: Option<&str>) -> Result<String, PromptError> {
        let mut buf = InputBuffer::new();
        loop {
            self.drain_outbound();
            if !*self.outbound_open {
                return Err(PromptError::Cancelled);
            }
            let app = &mut *self.app;
            let terminal = &mut *self.terminal;
            terminal.draw(|f| {
                draw::ui_with_modal(
                    f,
                    app,
                    draw::Modal::Text {
                        prompt,
                        help,
                        input: &buf,
                    },
                )
            })?;

            if !crossterm::event::poll(POLL_INTERVAL)? {
                continue;
            }
            match crossterm::event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter => return Ok(buf.take().trim().to_string()),
                    KeyCode::Esc => return Err(PromptError::Cancelled),
                    KeyCode::Backspace => buf.backspace(),
                    KeyCode::Delete => buf.delete_forward(),
                    KeyCode::Left => buf.cursor_left(),
                    KeyCode::Right => buf.cursor_right(),
                    KeyCode::Home => buf.cursor_home(),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.insert_char(c)
                    }
                    _ => {}
                },
                // 粘贴进来的多行也只是一段文本（不与 Enter 混淆）
                Event::Paste(text) => buf.insert_str(&text),
                _ => {}
            }
        }
    }
}

/// 退出恢复：把完整对话写回主屏 scrollback，再离开 alt-screen。
///
/// 照搬旧 `ui/mod.rs:142-174` 的 pi 语义（逐行 2K 清行，无残影、无 padding 行）。
///
/// **best-effort**：恢复路径上的每一步都不得 `?` 提前返回 —— 否则
/// `disable_raw_mode()` 会被跳过，终端留在 raw mode。这里吞掉 IO 错误，
/// 保证「无论如何都退出 raw mode / 恢复主屏」。
fn restore_terminal<B: Backend + Write>(terminal: &mut Terminal<B>, app: &App) {
    use crossterm::cursor::MoveTo;

    let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = (cols as usize).clamp(10, 400);
    let lines = draw::transcript_plain_lines(app, width);

    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        EnableLineWrap,
        LeaveAlternateScreen,
        DisableBracketedPaste
    );
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, MoveTo(0, 0));

    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            let _ = write!(out, "\r\n");
        }
        let _ = write!(out, "\r\x1b[2K{line}\x1b[0m");
    }
    let _ = write!(out, "\r\n");
    let _ = out.flush();

    let _ = disable_raw_mode();
    let _ = terminal.show_cursor();
}

/// 模态选择器里 ↑↓ 的落点（回卷；空列表原地不动）。
///
/// 抽成自由函数是为了**可单测** —— 模态循环本身要真终端与真键盘，
/// 只能靠这两个字节的算术把「回卷」语义钉在测试里。
fn modal_move(selected: usize, len: usize, up: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if up {
        if selected == 0 { len - 1 } else { selected - 1 }
    } else {
        (selected + 1) % len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modal_move_wraps_both_ways() {
        assert_eq!(modal_move(0, 3, false), 1, "下移");
        assert_eq!(modal_move(2, 3, false), 0, "到尾回卷");
        assert_eq!(modal_move(0, 3, true), 2, "到头回卷");
        assert_eq!(modal_move(1, 3, true), 0);
    }

    #[test]
    fn test_modal_move_on_empty_list_is_noop() {
        assert_eq!(modal_move(0, 0, true), 0);
        assert_eq!(modal_move(0, 0, false), 0);
    }

    /// 单元素列表上下移动都必须留在原地（否则 `options[selected]` 越界）。
    #[test]
    fn test_modal_move_single_option_stays() {
        assert_eq!(modal_move(0, 1, true), 0);
        assert_eq!(modal_move(0, 1, false), 0);
    }
}
