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

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
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
use crate::prompter::{PromptError, PromptOutcome, Prompter, resolve_prompt, resolve_session_pick};
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
    // panic hook 必须在 `setup_terminal` **之前**装好：`enable_raw_mode()` 已把终端切进
    // raw mode，此后到 `restore_terminal` 之间的任何 panic（含 `setup_terminal` 半途）
    // 都不会再经过那条正常恢复路径。
    install_panic_hook();
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
    // 鼠标捕获：**只为滚轮**（`events::handle_mouse`）。开着它终端就不再拿滚轮
    // 自己滚 scrollback / 发 `↑↓`，滚轮因此归聊天而不是输入历史（见 `events.rs`
    // 模块文档「滚轮」段）。代价：终端原生拖选失效，用户须 `⇧`+拖选。
    if let Err(e) = execute!(
        out,
        EnterAlternateScreen,
        DisableLineWrap,
        EnableBracketedPaste,
        EnableMouseCapture
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

fn event_loop<B: Backend<Error = std::io::Error> + Write + Send>(
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

        // 1b) 排空 Outbound **之后**立刻检查浮层意图 —— `apply_outbound` 可能刚设上
        //     `pending_prompt`（如 `/resume` 无参拿到 `SessionList` 非空）；不在这里检查，
        //     浮层要等用户再敲一个键才弹。
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

        // 2) 等输入，最多 POLL_INTERVAL
        if crossterm::event::poll(POLL_INTERVAL)? {
            let event = crossterm::event::read()?;
            events::handle_event(event, app, requests, boundaries, size.width, size.height);
            // `/login`、`/model` 无参、`/resume`（选会话）要开模态浮层 —— 画屏需要
            // Terminal，而 `events.rs` 没有，于是它只留下意图，这里跑循环（设计 §3）。
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

        // 滚动位置按当前尺寸重新夹紧（窗口缩放后 wrap 行数会变）——
        // `compute_max_scroll` 内部同步布局并把 `scroll_offset` 夹到有效范围。
        app.compute_max_scroll(size.width, size.height);

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

/// 跑一条 `Action::Prompt` 命令：模态问答 → 一条 [`Request`]（或回填输入框）。
///
/// **取消不改任何状态** —— `Err(Cancelled)` 直接吞掉，transcript / 输入框 /
/// 模型配置都不动（半截登录比什么都不做更糟）。
fn run_prompt<B: Backend<Error = std::io::Error> + Write + Send>(
    kind: PromptKind,
    terminal: &mut Terminal<B>,
    app: &mut App,
    outbound: &mut Receiver<Outbound<CodingView>>,
    outbound_open: &mut bool,
    requests: &Sender<Request>,
) {
    // 视图快照必须在把 `app` 借给 prompter **之前**取走（CodingView 是 Clone）
    let view = app.view.clone();
    // `/resume` 的候选与当前会话路径来自 `App`（与 `view` 不同源）—— 一并先取走。
    // `take()` 同时把 `pending_sessions` 清空：浮层结束时不再残留旧列表（与
    // `pending_prompt` 同 take 同清）。
    let sessions = app.pending_sessions.take().unwrap_or_default();
    let current_path = app.view.session_path.clone();

    let outcome = {
        let prompter = TuiPrompter::new(
            &mut *terminal,
            &mut *app,
            &mut *outbound,
            &mut *outbound_open,
        );
        match kind {
            // 会话候选来自 `App.pending_sessions`（与 `&CodingView` 不同源）⇒ 独立解析器。
            PromptKind::PickSession => {
                resolve_session_pick(&prompter, &sessions, current_path.as_deref())
            }
            _ => resolve_prompt(kind, &prompter, &view),
        }
    };

    apply_prompt_outcome(app, requests, outcome);
}

/// 把一条模态问答的**结局**落进 `App` / 请求信道。
///
/// 抽成自由函数：真模态循环需要真终端与真键盘，无法单测，但「取消不改任何状态」
/// 这条契约（`Err(Cancelled) => {}`）可以在这里单独钉住。
fn apply_prompt_outcome(
    app: &mut App,
    requests: &Sender<Request>,
    outcome: Result<PromptOutcome, PromptError>,
) {
    match outcome {
        Ok(PromptOutcome::Request(req)) => {
            app.note_request_sent(&req);
            if requests.blocking_send(req).is_err() {
                app.should_quit = true;
            }
        }
        // `/skills` 选中：**只回填输入框，不发送** —— 把光标留在末尾，用户补参数
        // 后自行回车（与 `Tab` 补全接受一项后的处置一致）。此时输入框必空
        // （`submit` 已 `take()`，光标归零），故 `insert_str` 即「从头写入」。
        Ok(PromptOutcome::Prefill(text)) => app.input.insert_str(&text),
        // 有意义的无操作（`/resume` 选中当前会话）：不发请求，但**不静默**。
        Ok(PromptOutcome::NoOp(hint)) => {
            app.transcript.push(TranscriptLine::System(hint));
            app.follow = true;
        }
        Err(PromptError::Cancelled) => {}
        // 浮层里的 Ctrl-C = **关程序**（与空闲时的 Ctrl-C 同义）。只关浮层不够：
        // 用户按 Ctrl-C 就是要退出，把人退回聊天界面是另一种「没生效」。
        Err(PromptError::Quit) => app.should_quit = true,
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
struct ModalHost<'a, B: Backend<Error = std::io::Error> + Write> {
    terminal: &'a mut Terminal<B>,
    app: &'a mut App,
    outbound: &'a mut Receiver<Outbound<CodingView>>,
    outbound_open: &'a mut bool,
}

/// [`Prompter`] 的 TUI 实现（`run.rs` 内，因为它握着 `Terminal`）。
pub(crate) struct TuiPrompter<'a, B: Backend<Error = std::io::Error> + Write>(
    Mutex<ModalHost<'a, B>>,
);

impl<'a, B: Backend<Error = std::io::Error> + Write> TuiPrompter<'a, B> {
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

impl<B: Backend<Error = std::io::Error> + Write + Send> Prompter for TuiPrompter<'_, B> {
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

impl<B: Backend<Error = std::io::Error> + Write> ModalHost<'_, B> {
    /// 模态打开期间继续排空 `Outbound`。
    ///
    /// 不排空的话，agent 在模态开着时吐事件会把有界信道灌满，把 app 线程堵在
    /// 发送端 —— 用户只是开了个选择器，不该让整条流水线停摆。
    /// 顺带：`Outbound::Quit` 在此时也照样生效（走 `should_quit`）。
    ///
    /// **不在这里开嵌套浮层**：模态期间到达的 `SessionList`（非空）会被
    /// `apply_outbound` 设上 `pending_prompt`，但这里只**存起来**、不消费它 ——
    /// 当前模态结束后，主循环的检查点（排空 Outbound 之后）自会把它打开。
    ///
    /// **模态期间到达 `SessionResumed` 会经 `apply_outbound` 走 `reset_for_replay` 清屏**，
    /// 把模态的底衬换掉。按 Q8 的顺序这**不应发生**：`SessionResumed` 只在回合边界抵达，
    /// 且通常在 `SessionList`（模态正是由它开启）之后。这里**不做防护** —— 依赖 Q8 成立；
    /// 若前提变化（设计 §7 的遗留项），需在此按事件类型分流。
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

    /// 选项选择器：**常驻筛选输入框** + ↑↓ 移动（回卷）、Enter 确认、Esc 取消。
    ///
    /// 一打开就能直接敲字筛选（大小写不敏感子串，见 [`filter_indices`]）—— 空格、`/`、
    /// 字母数字都进筛选框，故**不需要**模式切换键，也不会与 ↑↓/Enter 抢语义。
    /// 空查询时 `filter_indices` 返回全体 ⇒ 行为与「纯列表」逐字一致；四个模态命令
    /// （provider / model / skill / session）共用本函数，故筛选对它们一并生效。
    ///
    /// **Enter 在无匹配时不动作**（既不确认也不取消）：常驻输入框下，敲到一半误按
    /// Enter 不该把浮层关掉；**取消（Esc）与关程序（Ctrl-C）是两个不同的出口**。
    fn select_modal(
        &mut self,
        prompt: &str,
        options: &[String],
        page_size: usize,
    ) -> Result<String, PromptError> {
        if options.is_empty() {
            return Err(PromptError::Other(format!("{prompt}：没有可选项")));
        }
        let mut query = InputBuffer::new();
        let mut selected = 0usize;
        loop {
            self.drain_outbound();
            if !*self.outbound_open {
                // app 线程没了 —— 问谁都没意义
                return Err(PromptError::Cancelled);
            }
            // 每帧现算：查询变了、候选被 app 侧刷过，都自动跟上。
            // `selected` 必须跟着收敛 —— 它记的是**过滤后**的下标。
            let filtered = filter_indices(options, &query.text);
            selected = clamp_selection(selected, filtered.len());

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
                        filtered: &filtered,
                        selected,
                        query: &query,
                        page_size,
                    },
                )
            })?;

            if !crossterm::event::poll(POLL_INTERVAL)? {
                continue;
            }
            match crossterm::event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Up => selected = modal_move(selected, filtered.len(), true),
                    KeyCode::Down => selected = modal_move(selected, filtered.len(), false),
                    KeyCode::Enter if !filtered.is_empty() => {
                        return Ok(options[filtered[selected]].clone());
                    }
                    KeyCode::Enter => {}
                    KeyCode::Esc => return Err(PromptError::Cancelled),
                    // Ctrl-C 在浮层里也是「关程序」，不能被吞掉。
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(PromptError::Quit);
                    }
                    KeyCode::Backspace => query.backspace(),
                    KeyCode::Delete => query.delete_forward(),
                    KeyCode::Left => query.cursor_left(),
                    KeyCode::Right => query.cursor_right(),
                    KeyCode::Home => query.cursor_home(),
                    KeyCode::End => query.cursor_end(),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.insert_char(c)
                    }
                    _ => {}
                },
                // 粘贴进来的多行也只是一段筛选串（与 `text_modal` 同规，不与 Enter 混淆）
                Event::Paste(text) => query.insert_str(&text),
                _ => {}
            }
        }
    }

    /// 单行文本输入（`/login` 的 api_key）：Enter 确认、Esc 取消、Ctrl-C 关程序。
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
                    // Ctrl-C 在浮层里也是「关程序」（与 `select_modal` 同规）。
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(PromptError::Quit);
                    }
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
/// **不做绝对定位**：`LeaveAlternateScreen`（`CSI ?1049l`）已把光标恢复到
/// 进入 alt-screen 时的位置 —— 也就是本 app 自己那段启动输出（stderr 的降级提示、
/// 模型拉取提示）的**下一行**。回放从那里往下写即可。曾经在这里 `MoveTo(0, 0)`
/// 是错的：那是**视口绝对第 0 行**，会把屏幕顶部（往往是与本 app 无关的上一条
/// 命令输出）整片啃掉，并把启动提示的首行盖掉。
///
/// **best-effort**：恢复路径上的每一步都不得 `?` 提前返回 —— 否则
/// `disable_raw_mode()` 会被跳过，终端留在 raw mode。这里吞掉 IO 错误，
/// 保证「无论如何都退出 raw mode / 恢复主屏」。
fn restore_terminal<B: Backend<Error = std::io::Error> + Write>(
    terminal: &mut Terminal<B>,
    app: &App,
) {
    let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = (cols as usize).clamp(10, 400);
    let lines = draw::transcript_plain_lines(app, width);

    let mut out = std::io::stdout();
    let _ = write!(out, "{}", exit_payload(&lines));
    let _ = out.flush();

    let _ = disable_raw_mode();
    let _ = terminal.show_cursor();
}

/// 终端状态恢复的字节序列：开换行 / 退 alt-screen / 关括号粘贴 / 关鼠标捕获 / 弹键盘增强。
///
/// 抽成自由函数，让**正常退出**（[`exit_payload`]）与**异常退出**（panic hook）共用同一份
/// 序列 —— 两处若各写一遍，改一处必漏另一处。
///
/// **顺序**（与抽取前逐字节一致）：`EnableLineWrap` → `LeaveAlternateScreen` →
/// `DisableBracketedPaste` → `DisableMouseCapture` → `PopKeyboardEnhancementFlags`。五条命令各改
/// 一个**互不相干**的终端模式位，除「`LeaveAlternateScreen` 在回放之前」外彼此无先后依赖
/// （回放段的落点靠 `?1049l` 恢复的光标，见 [`exit_payload`]）；此顺序沿用既有实现，不另作调整。
///
/// 只含**字节序列**：`disable_raw_mode()` 是系统调用（改 termios），无法塞进这里，
/// 由两个调用方各自按需发出（见 [`restore_terminal`] 与 [`restore_terminal_on_panic`]）。
/// crossterm 的命令是 `impl Command`（写字节）而非 `Display`，故先 `queue!` 进缓冲区再转字符串。
fn restore_sequence() -> String {
    let mut restore: Vec<u8> = Vec::new();
    let _ = crossterm::queue!(
        restore,
        EnableLineWrap,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
        PopKeyboardEnhancementFlags
    );
    String::from_utf8_lossy(&restore).into_owned()
}

/// 退出时写给终端的**全部字节** —— 抽成纯函数，好让「不得绝对定位」这条能被单测钉住。
///
/// `restore_terminal` 除本函数的产物外**不再写任何东西**，所以单测直接盯着这里，
/// 就能覆盖「回放落点」这条不变量：任何人往里加 `MoveTo(0, 0)` 都会被测试拦下。
///
/// 前段是终端状态恢复（换行 / 退出 alt-screen / 关括号粘贴 / 弹键盘增强）；
/// 后段是回放 —— 逐行 `\r\x1b[2K{line}\x1b[0m`、行间 `\r\n`、末尾一个 `\r\n`，
/// **不含任何光标定位转义**：落点是进入 alt-screen 时保存、`?1049l` 恢复的那个位置。
fn exit_payload(lines: &[String]) -> String {
    // 前段：终端状态恢复 —— 与 panic hook 共用同一份序列（改一处即两处都改）。
    let mut out = restore_sequence();

    // 后段：回放。**不含任何光标定位转义** —— 落点由 `?1049l` 恢复。
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        out.push_str("\r\x1b[2K");
        out.push_str(line);
        out.push_str("\x1b[0m");
    }
    out.push_str("\r\n");
    out
}

// ---------------------------------------------------------------------------
// panic 边界
// ---------------------------------------------------------------------------

/// 装 panic hook，使**任何绘制路径的 panic** 都先恢复终端再让默认 hook 打印信息。
///
/// [`run`] 的正常恢复（[`restore_terminal`]）在 `event_loop` panic 时会被 unwind **跳过**，
/// 故本方案第一次把模型输出（不可信输入）喂给自述 PoC 的第三方解析器后，必须把恢复挂到
/// panic 路径上。用 hook 而非 `catch_unwind` 包某一处：hook 覆盖**全部**绘制路径。
///
/// **先 `take_hook` 取走上游默认 hook 再 `set_hook` 自己的**，并在 hook 体里最后调用它 ——
/// 这样保留默认的 panic 信息打印与 unwind 语义。hook 体**不** `process::exit`、
/// **不**用 `catch_unwind` 吞掉 panic：panic 后 `App` / 缓存状态不保证一致，**失败即退出**
/// 比带伤续跑安全。
fn install_panic_hook() {
    let delegate = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        panic_hook_body(restore_terminal_on_panic, &*delegate, info);
    }));
}

/// panic hook 的**体**：先跑注入的恢复动作，再把控制权交给上游 delegate。
///
/// 泛型入参 `T` 只为实现可单测 —— hook 内 `T = PanicHookInfo`，而单测用一个假 `T`
/// （如 `()`）就能断言「恢复先发生、delegate 后发生」，**无需触发真实 panic、不接触真终端**。
/// 恢复动作经 `impl FnOnce` 注入，正因如此 [`restore_terminal_on_panic`] 的真实现与
/// **不写任何字节**的探针可以在同一条码路上被演练。
fn panic_hook_body<T>(restore: impl FnOnce(), delegate: &(dyn Fn(&T) + Sync), info: &T) {
    restore();
    delegate(info);
}

/// panic 路径上的**真**恢复动作：best-effort，每步吞 IO 错误、**绝不二次 panic**。
///
/// 顺序固定为：恢复字节序列 → `disable_raw_mode()` → 显示光标。
/// `disable_raw_mode()` 是**系统调用**（改 termios），不在 [`restore_sequence`] 的字节内，
/// 必须在这里单独发出。任一步失败都只能忽略 —— 在 panic hook 里再 panic 会导致进程 abort。
fn restore_terminal_on_panic() {
    let mut out = std::io::stdout();
    let _ = write!(out, "{}", restore_sequence());
    let _ = out.flush();
    let _ = disable_raw_mode();
    // 绘制中途 panic 时光标可能仍处于隐藏状态，恢复回显。
    let _ = execute!(out, Show);
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

/// 模态选择器的**筛选**：返回匹配项在 `options` 里的下标（保序）。空 `query` ⇒ 全选。
///
/// 匹配对象是**整条候选串**（`provider/model`、`/command  — description`、会话标签…），
/// 大小写不敏感、**子串**匹配。故输 `deepseek` 会把 provider 前缀与模型名一起筛
/// （同名跨 provider 的几条都留下），再输 `deepseek/` 就精确到官方那家。
///
/// 抽成自由函数的理由与 [`modal_move`] 相同：模态循环要真终端，只能靠纯函数把语义
/// 钉进单测。`to_lowercase` / `contains` 都在 char 边界工作，CJK / emoji 不会 panic
/// （与 `InputBuffer` 的 char 纪律一致）。
///
/// 变异：去掉 `query.to_lowercase()` —— 大写查询（`CHAT`）与候选（`chat`）不再匹配
/// ⇒ `test_filter_indices_case_insensitive_substring` 变红。
fn filter_indices(options: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..options.len()).collect();
    }
    let needle = query.to_lowercase();
    options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// 筛选结果变化后把高亮收敛进新范围 —— **防越界**。
///
/// 查询每敲一个字符，候选就少一批；`selected` 是从**过滤前**继承下来的下标，
/// 不收敛的话 `filtered[selected]` 会在下一帧 panic（`len` 从 8 变 1 而 `selected`
/// 仍是 7）。收敛到**最后一项**而不是 0：删字符（放宽筛选）时高亮停在附近，
/// 不会跳回顶部。
///
/// 变异：直接返回 `selected` ⇒ `test_clamp_selection_converges` 变红。
fn clamp_selection(selected: usize, filtered_len: usize) -> usize {
    if filtered_len == 0 {
        0
    } else {
        selected.min(filtered_len - 1)
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

    fn opts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 筛选：**不区分大小写**的子串匹配，空查询 = 全选，无命中 = 空。
    ///
    /// 变异：`filter_indices` 里去掉 `to_lowercase()` ⇒ 大写查询匹配不上 ⇒ 变红。
    #[test]
    fn test_filter_indices_case_insensitive_substring() {
        let o = opts(&[
            "deepseek/deepseek-chat",
            "opencode-go/FLASH",
            "minimax/MiniMax-M3",
        ]);

        assert_eq!(
            filter_indices(&o, ""),
            vec![0, 1, 2],
            "空查询 = 全选（保序）"
        );
        assert_eq!(filter_indices(&o, "opencode"), vec![1]);
        assert_eq!(filter_indices(&o, "CHAT"), vec![0], "大小写不敏感");
        assert_eq!(filter_indices(&o, "flash"), vec![1], "候选里是大写也命中");
        assert_eq!(filter_indices(&o, "mini"), vec![2], "子串而非前缀");
        assert_eq!(filter_indices(&o, "zzz"), Vec::<usize>::new(), "无命中");
    }

    /// **匹配对象是整条候选串** —— 故 provider 前缀与模型名都能筛：
    /// 输 `deepseek` 留下两条（官方 + 网关转售的），再输 `deepseek/` 精确到官方那家。
    /// 这正是「筛出 provider」这条需求的落点。
    #[test]
    fn test_filter_indices_matches_provider_prefix() {
        let o = opts(&["deepseek/deepseek-chat", "opencode-go/deepseek-flash"]);

        assert_eq!(
            filter_indices(&o, "deepseek"),
            vec![0, 1],
            "provider 名与模型名一起筛"
        );
        assert_eq!(
            filter_indices(&o, "deepseek/"),
            vec![0],
            "加了斜杠即精确到官方"
        );
    }

    /// CJK / emoji 候选不 panic 且能命中（`to_lowercase`/`contains` 按 char 边界工作）。
    ///
    /// 变异：把匹配换成字节切片（`as_bytes().windows(..)`）⇒ trait 不符或命中集错 ⇒ 变红。
    #[test]
    fn test_filter_indices_cjk_and_symbols() {
        let o = opts(&["/commit  — 生成一条提交信息", "🔧 tools"]);

        assert_eq!(filter_indices(&o, "提交"), vec![0]);
        assert_eq!(filter_indices(&o, "🔧"), vec![1]);
        assert_eq!(filter_indices(&o, "生成"), vec![0]);
    }

    /// 高亮收敛：查询收窄后 `selected` 必须落进新范围（否则 `filtered[selected]` 越界）。
    ///
    /// 变异：`clamp_selection` 直接返回 `selected` ⇒ 上层 `filtered[selected]` panic。
    #[test]
    fn test_clamp_selection_converges() {
        assert_eq!(clamp_selection(5, 3), 2, "收窄到最后一顶");
        assert_eq!(clamp_selection(2, 5), 2, "范围内不动");
        assert_eq!(clamp_selection(0, 0), 0, "无命中不得 panic");
        assert_eq!(clamp_selection(9, 1), 0);
    }

    /// 退出回放：逐行 `\r\x1b[2K` 清行 + 行间 `\r\n` + 末尾一个 `\r\n`，
    /// 且**不含任何光标定位** —— 落点靠 `?1049l` 恢复的光标位置。
    ///
    /// 变异：在 `exit_payload` 里加回 `MoveTo(0, 0)`（`\x1b[1;1H`）→ 本用例变红
    /// （那会把回放挪到视口第 0 行，啃掉屏幕上与本 app 无关的内容）。
    #[test]
    fn exit_payload_replays_without_absolute_positioning() {
        let seq = exit_payload(&["first".to_string(), "second".to_string()]);

        // 前段是状态恢复，后段是回放（逐行 2K 清行、行间 \r\n、末尾一个 \r\n）。
        assert!(
            seq.ends_with("\r\x1b[2Kfirst\x1b[0m\r\n\r\x1b[2Ksecond\x1b[0m\r\n"),
            "回放段格式不对：{seq:?}"
        );
        // **不变量**：整段里不得出现任何以 `H` 结尾的 CSI（光标绝对定位），
        // 否则回放会落在视口顶部而非 app 自己输出的下一行。
        assert!(
            !seq.contains("\x1b[1;1H"),
            "回放不得绝对定位到视口顶部：{seq:?}"
        );
        assert!(
            !seq.contains("H"),
            "退出载荷里不应有任何光标定位 CSI（以 H 结尾）：{seq:?}"
        );
    }

    /// 空行列表也要吐一个 `\r\n`（不能把光标留在原行上）。
    #[test]
    fn exit_payload_with_no_lines_still_breaks_the_line() {
        assert!(exit_payload(&[]).ends_with("\r\n"));
    }

    /// 状态恢复命令确实在载荷前段（`?1049l` 退出 alt-screen 是回放贴合光标的前提）。
    #[test]
    fn exit_payload_starts_with_terminal_restore() {
        let seq = exit_payload(&["x".to_string()]);
        assert!(
            seq.contains("\x1b[?1049l"),
            "必须先退出 alt-screen 再回放：{seq:?}"
        );
        assert!(
            seq.find("\x1b[?1049l").unwrap() < seq.find("\r\x1b[2K").unwrap(),
            "退出 alt-screen 必须排在回放之前：{seq:?}"
        );
    }

    /// N9 ①：恢复序列必须含退 alt-screen / 开换行 / 关括号粘贴 / 关鼠标捕获 / 弹键盘增强，
    /// 缺任一条，panic 后终端都会停在 alt-screen、关闭换行、括号粘贴或鼠标捕获模式，或把
    /// 键盘增强标志泄漏进 shell（用户只能 `reset`）。
    ///
    /// 鼠标捕获这一条尤其不能漏：开着退出后，用户在主屏里的原生拖选会全部失效。
    ///
    /// 字节值取自 `crossterm` 0.29 实测（非凭记忆）；`disable_raw_mode()` 是**系统调用**
    /// （改 termios），不在本字节序列内，故无从在此断言 —— 它由
    /// [`restore_terminal_on_panic`] 在序列之后单独发出。
    #[test]
    fn restore_sequence_contains_terminal_state_restore() {
        let seq = restore_sequence();
        assert!(seq.contains("\x1b[?1049l"), "须退出 alt-screen：{seq:?}");
        assert!(seq.contains("\x1b[?7h"), "须开启换行：{seq:?}");
        assert!(seq.contains("\x1b[?2004l"), "须关闭括号粘贴：{seq:?}");
        assert!(
            seq.contains("\x1b[?1006l") && seq.contains("\x1b[?1000l"),
            "须关闭鼠标捕获（否则退出后拖选失效）：{seq:?}"
        );
        assert!(
            seq.contains("\x1b[<1u"),
            "须弹出键盘增强标志（否则泄漏进 shell）：{seq:?}"
        );
    }

    /// `apply_prompt_outcome(Err(Cancelled))` ⇒ **无副作用**：transcript / 输入框 /
    /// `pending_sessions` 全不变（Esc 取消的契约）。
    ///
    /// 变异：把 `Err(PromptError::Cancelled)` 臂改成推一条提示（或改 `NoOp` 那样写
    /// transcript）⇒ 本用例变红。
    #[test]
    fn test_apply_prompt_outcome_cancel_is_side_effect_free() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Request>(4);
        let mut app = App::new(CodingView::for_test());
        app.transcript.push(TranscriptLine::User("keep".into()));
        app.input.insert_str("/resume");
        app.pending_sessions = Some(Vec::new());
        let before = app.transcript.clone();

        apply_prompt_outcome(&mut app, &tx, Err(PromptError::Cancelled));

        assert_eq!(app.transcript, before, "取消不改 transcript");
        assert_eq!(app.input.text, "/resume", "取消不改输入框");
        assert!(app.pending_sessions.is_some(), "取消不改 pending_sessions");
    }

    /// 浮层里的 Ctrl-C ⇒ `PromptError::Quit` ⇒ **关程序**。
    ///
    /// 终端里的浮层循环无法单测，但「Quit 会落到 `should_quit`」这条可以：不落它，
    /// 浮层里的 Ctrl-C 就只是静默无效（用户按下去既不关浮层也不退程序）。
    ///
    /// 变异：删 `Err(PromptError::Quit) => app.should_quit = true` 整臂（或归入
    /// `Cancelled`）⇒ 本用例变红。
    #[test]
    fn test_apply_prompt_outcome_quit_closes_program() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Request>(4);
        let mut app = App::new(CodingView::for_test());
        apply_prompt_outcome(&mut app, &tx, Err(PromptError::Quit));
        assert!(app.should_quit, "浮层里的 Ctrl-C 必须关程序");
    }

    /// 浮层选完产出的**命令请求**也要进入在飞态 —— 否则 `/login` 的网络往返期间
    /// Esc 无人可投（路径：浮层 → `apply_prompt_outcome`，不经 `events::send_request`）。
    ///
    /// 变异：删 `app.note_request_sent(&req)` ⇒ 本用例变红。
    #[test]
    fn test_apply_prompt_outcome_marks_command_in_flight() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Request>(4);
        let mut app = App::new(CodingView::for_test());
        apply_prompt_outcome(&mut app, &tx, Ok(PromptOutcome::Request(Request::Logout)));
        assert!(app.command_in_flight, "浮层发出去的命令同样算在飞");
    }

    /// `NoOp` 结局 ⇒ 不发请求，但把提示写进 transcript 并贴底（不静默）。
    #[test]
    fn test_apply_prompt_outcome_noop_pushes_hint() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Request>(4);
        let mut app = App::new(CodingView::for_test());
        apply_prompt_outcome(
            &mut app,
            &tx,
            Ok(PromptOutcome::NoOp("已在当前会话".into())),
        );
        assert!(
            matches!(app.transcript.last(), Some(TranscriptLine::System(s)) if s == "已在当前会话"),
            "{:?}",
            app.transcript
        );
        assert!(app.follow);
    }

    /// N9 ②：hook 体**先**跑注入的恢复动作，**再**调用 delegate —— 后者保留默认的 panic
    /// 信息打印与 unwind 语义，绝不能被恢复路径吞掉。
    ///
    /// 用不接触真终端的探针（`AtomicBool`）与假 `T = ()` 断言，**不触发真实 panic**
    /// （真触发会污染测试输出、并与并行测试互相干扰）。
    #[test]
    fn panic_hook_body_restores_then_delegates() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let restored = AtomicBool::new(false);
        let delegated = AtomicBool::new(false);
        // delegate 里回看一眼恢复探针：钉住「delegate 时恢复已经发生」。
        let restored_before_delegate = AtomicBool::new(false);

        let restore = || restored.store(true, Ordering::SeqCst);
        let delegate = |_: &()| {
            restored_before_delegate.store(restored.load(Ordering::SeqCst), Ordering::SeqCst);
            delegated.store(true, Ordering::SeqCst);
        };
        panic_hook_body(restore, &delegate, &());

        assert!(restored.load(Ordering::SeqCst), "恢复动作必须被调用");
        assert!(delegated.load(Ordering::SeqCst), "delegate 必须仍被调用");
        assert!(
            restored_before_delegate.load(Ordering::SeqCst),
            "恢复必须发生在 delegate 之前"
        );
    }
}
