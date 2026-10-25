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
use std::process::{Command, Stdio};
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
use crate::commands::{PROMPT_PAGE_SIZE, PromptKind};
use crate::input::InputBuffer;
use crate::prompter::{PromptError, PromptOutcome, Prompter, resolve_prompt, resolve_session_pick};
use crate::theme::ThemeKind;
use crate::transcript::TranscriptLine;
use crate::view::CodingView;
use crate::{draw, events, selection};

/// 等输入的时长上限（空闲重绘节拍）。
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 拖选贴边时的轮询周期 —— 自动滚动要跟手，故比空闲节拍短。
const DRAG_TICK: Duration = Duration::from_millis(16);
/// Working 动画节拍。
const TICK_INTERVAL: Duration = Duration::from_millis(300);

/// 本轮的等输入时长：拖到边缘时用 [`DRAG_TICK`]（16 ms），否则用 [`POLL_INTERVAL`]。
fn poll_interval(dragging_with_edge: bool) -> Duration {
    if dragging_with_edge {
        DRAG_TICK
    } else {
        POLL_INTERVAL
    }
}

/// 把一批连续到达的鼠标事件合并成**最后一个**，并把碰到的首个非鼠标事件**推迟**返回。
///
/// 触控板一次滑动会爆发 8–30 个事件，逐个走一遍 `draw`（每次约 150 µs）就是可见的卡顿
/// （设计 §2.5）。合并不丢事件：非鼠标事件一律原样返回，由调用方在本轮消费。
///
/// 返回 `(最后一个鼠标事件, 被推迟的非鼠标事件)`。
fn coalesce(
    first: Event,
    mut next: impl FnMut() -> Option<Event>,
) -> (Option<Event>, Option<Event>) {
    let mut mouse = match first {
        Event::Mouse(_) => Some(first),
        other => return (None, Some(other)),
    };
    while let Some(event) = next() {
        match event {
            Event::Mouse(_) => mouse = Some(event),
            // 遇到非鼠标事件就停：**不 read 丢掉**，原样交给调用方。
            other => return (mouse, Some(other)),
        }
    }
    (mouse, None)
}

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

/// 消费**一轮**里排好的事件，并在同一个轮次里收口剪贴板的两条路（写 / 读）。
///
/// 四件事的顺序是承重的：
///
/// 1. 合并后的**鼠标**事件（拖选松手 → `pending_clipboard`）；
/// 2. 被推迟的**非鼠标**事件（`Ctrl+C` 复制、`Ctrl+V` 置 `pending_paste`、`/copy`、粘贴）；
/// 3. 排空 `pending_paste` **读**剪贴板并插进输入框；
/// 4. 排空 `pending_clipboard` **写**剪贴板。
///
/// **2 必须在 3 / 4 之前**：键盘那条路既可能置位读请求也直接写 `pending_clipboard`，
/// 反过来排会让「按 Ctrl+C 复制」「按 Ctrl+V 粘贴」都晚一个事件才生效（用户看到的是
/// 「没反应」）。
///
/// **3 必须在 4 之前**（ADR-0059 决策 4）：两者在同一轮同时非空，说明上一轮刚复制过
/// （X11 下 `xsel` 接管 selection 需要一点时间），这一轮又按了粘贴。先写后读会把**刚
/// 复制的文本**读回来并插进输入框（「我明明粘的是别处的内容」）；先读后写则可能读到旧
/// 剪贴板 —— 两种都不完美，但「粘到旧内容」是可删的、可预期的，而「静默地把输入框
/// 改成刚复制的草稿」是破坏性的。读失败（`None`）只清标志、不动输入框。
///
/// 抽成自由函数是为了让这个顺序可测（`test_round_writes_clipboard_from_deferred_key`、
/// `test_round_pastes_into_input`、`test_round_reads_before_writing`）—— 塞在
/// `event_loop` 里就只能靠人读。参数多恰是它的形态：它要转交两个信道、尺寸与剪贴板
/// 的读写出口，收成一个结构体只会让「顺序」这件事更难看出来。
#[allow(clippy::too_many_arguments)]
fn handle_round(
    app: &mut App,
    mouse: Option<Event>,
    deferred: Option<Event>,
    requests: &Sender<Request>,
    boundaries: &Sender<Boundary>,
    width: u16,
    height: u16,
    clip: &mut impl FnMut(&str),
    read: &mut impl FnMut() -> Option<String>,
) {
    if let Some(m) = mouse {
        events::handle_event(m, app, requests, boundaries, width, height);
    }
    if let Some(other) = deferred {
        events::handle_event(other, app, requests, boundaries, width, height);
    }
    if std::mem::take(&mut app.pending_paste)
        && let Some(text) = read()
    {
        // `insert_str` 先消费选区（有选区时粘贴即替换，与 GUI 输入框同语义）。
        app.input.insert_str(&text);
    }
    if let Some(text) = app.pending_clipboard.take() {
        clip(&text);
    }
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

        // 2) 等输入，最多 poll 周期（拖到边缘时缩短到 `DRAG_TICK`）
        let poll = poll_interval(app.selection.is_dragging() && app.selection.edge() != 0);
        if crossterm::event::poll(poll)? {
            let first = crossterm::event::read()?;
            // 触控板会爆发一串事件：只应用最后一个鼠标事件，非鼠标事件推到后面。
            let (mouse, deferred) = coalesce(first, || {
                if !crossterm::event::poll(Duration::ZERO).ok()? {
                    return None;
                }
                crossterm::event::read().ok()
            });
            handle_round(
                app,
                mouse,
                deferred,
                requests,
                boundaries,
                size.width,
                size.height,
                &mut write_clipboard,
                &mut read_system_clipboard,
            );
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
        // 必须在 `compute_max_scroll` **之前**跑自动滚动：它会重算 `top_row`，
        // 而 `drag_tick` 的 `cursor_row` 依赖新的 `top_row`（设计 §5.5）。
        selection::drag_tick(app, size.width, size.height, Instant::now());
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
    // `/theme` 无参：**实时预览**浮层（每移动一格立即铺色）。它在
    // [`resolve_prompt`] **之前**分流 —— 后者的 `Prompter::select` 只回传最终
    // 选中项、不给中间过程留回调，结构上做不了逐键预览（见 `PromptKind::PickTheme`）。
    if matches!(kind, PromptKind::PickTheme) {
        let outcome = {
            let prompter = TuiPrompter::new(
                &mut *terminal,
                &mut *app,
                &mut *outbound,
                &mut *outbound_open,
            );
            prompter.lock().apply_theme_modal()
        };
        match outcome {
            Ok(close) => {
                // 确认：预览已把主题设成 `kind` 了 ⇒ 这里只补本地回显 + 单向同步。
                // 取消（Esc）：主题已还原、`request()` 为 `None` ⇒ 不发请求、不落盘。
                if let Some(kind) = close.confirmed() {
                    app.transcript
                        .push(TranscriptLine::System(format!("theme: {}", kind.name())));
                    app.follow = true;
                }
                if let Some(req) = close.request() {
                    apply_prompt_outcome(app, requests, Ok(PromptOutcome::Request(req)));
                }
            }
            Err(e) => apply_prompt_outcome(app, requests, Err(e)),
        }
        return;
    }

    // 视图快照必须在把 `app` 借给 prompter **之前**取走（CodingView 是 Clone）
    let view = app.view.clone();
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

    /// 主题浮层：**每移一格立即预览**（`app.set_theme`），Enter 确认、Esc 还原。
    ///
    /// # 为何复用 [`draw::Modal::Select`]
    ///
    /// 浮层框架（居中尺寸、`Clear` + 背景补铺、页脚键位、滚动与计数）只该有一份。
    /// 主题浮层的差异只有两处：**不筛**（无查询框，`query` 恒空）与**选中态可立即
    /// 生效**。前者用空 `InputBuffer` 即可表达（`filter_indices` 对空查询返回全体），
    /// 后者在逐键处理里做 —— 都不需要新的 `Modal` 变体。故候选用
    /// [`ThemeKind::label`]，选中项按同一数组的 `position` 反查回 `ThemeKind`。
    ///
    /// # 出口
    ///
    /// - `Enter` ⇒ [`ThemeModalClose::Confirmed`]，由调用方发 `Request::SetTheme`（落盘）。
    /// - `Esc` ⇒ [`ThemeModalClose::Cancelled`]，主题**已还原到 `opened_with`**，不落盘。
    /// - `Ctrl-C` ⇒ 与 Esc **同还原语义**，但返回 [`PromptError::Quit`]（与
    ///   [`Self::select_modal`] 一致：浮层里 Ctrl-C = 关程序）。先还原再退，是为了
    ///   让退出回放的 scrollback 用回用户原来那套配色。
    /// - 其余键 ⇒ `Ignore`（本浮层无查询框，不做过滤）。
    fn apply_theme_modal(&mut self) -> Result<ThemeModalClose, PromptError> {
        let labels: Vec<String> = ThemeKind::ALL
            .iter()
            .map(|k| k.label().to_string())
            .collect();
        let mut state = ThemeModalState {
            opened_with: self.app.theme,
            selected: self.app.theme,
        };
        // 无查询框：候选恒为全体（`theme_index` 给的高亮就是 `options` 的下标）。
        let filtered: Vec<usize> = (0..labels.len()).collect();
        let query = InputBuffer::new();
        loop {
            self.drain_outbound();
            if !*self.outbound_open {
                // app 线程没了 —— 问谁都没意义。**仍先还原主题**：预览已改过色板，
                // 不还原会让「退出前最后一眼」与「落盘值」撕裂。
                self.app.set_theme(state.opened_with);
                return Err(PromptError::Cancelled);
            }

            let selected = theme_index(state.selected);
            let app = &mut *self.app;
            let terminal = &mut *self.terminal;
            terminal.draw(|f| {
                draw::ui_with_modal(
                    f,
                    app,
                    draw::Modal::Select {
                        prompt: "主题（↑↓ 预览，Enter 确认，Esc 取消）",
                        options: &labels,
                        filtered: &filtered,
                        selected,
                        query: &query,
                        page_size: PROMPT_PAGE_SIZE,
                    },
                )
            })?;

            if !crossterm::event::poll(POLL_INTERVAL)? {
                continue;
            }
            let key = match crossterm::event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Ctrl-C 在浮层里也是「关程序」，不能被吞掉（同 `select_modal`）。
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        self.app.set_theme(state.opened_with);
                        return Err(PromptError::Quit);
                    }
                    match key.code {
                        KeyCode::Up => ThemeKey::Up,
                        KeyCode::Down => ThemeKey::Down,
                        KeyCode::Enter => ThemeKey::Enter,
                        KeyCode::Esc => ThemeKey::Esc,
                        _ => ThemeKey::Other,
                    }
                }
                // 粘贴 / 鼠标 / 松开事件：无查询框，一律忽略。
                _ => ThemeKey::Other,
            };

            // 借位拆开：`apply` 要 `&mut self.app`，而 `state` 是本地量。
            let app = &mut *self.app;
            let mut apply = |kind: ThemeKind| app.set_theme(kind);
            if let Some(close) = apply_theme_step(&mut state, key, &mut apply) {
                return Ok(close);
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

/// `/copy` 的载荷：OSC 52 剪贴板转义序列（终端无关，无新依赖）。
///
/// 形如 `ESC ] 52 ; c ; <base64> BEL`。
///
/// # 为什么保留 OSC 52（已有本机工具了）
///
/// 它是**唯一**能穿过 SSH / 容器 / tmux（配 `set-clipboard on`）的通道：那些环境里没有
/// 本机工具可用，剪贴板在终端那一侧。与 [`write_system_clipboard`] 是两条互补的路。
///
/// # 已知边界
///
/// 不少终端默认**禁用** OSC 52（防恶意程序写剪贴板）：VS Code 的 xterm.js 至今未实现
/// 它，有些只支持读不支持写。此时本序列会被静默忽略 —— 这正是本仓库不再只靠它的理由
/// （本机工具是那条路的兜底）。
///
/// # 为什么内联 base64 而不引 crate
///
/// `ys-tui-coding` 的依赖面已经收得很紧（见 `Cargo.toml` 里的逐条理由），
/// 而 base64 的编码规则三行就能写清：每 3 字节 → 4 字符，余 1/2 字节补 `=`。
fn clipboard_sequence(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4 + 8);
    out.push_str("\x1b]52;c;");

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        // 每组的第几字节存在 ⇒ 输出几个字符（余 1 字节出 2 字符，余 2 出 3，整 3 出 4）。
        let keep = chunk.len() + 1;
        for i in 0..4 {
            if i < keep {
                let idx = ((n >> (18 - 6 * i)) & 0x3f) as usize;
                out.push(ALPHABET[idx] as char);
            } else {
                out.push('=');
            }
        }
    }

    out.push('\x07');
    out
}

/// 读系统剪贴板；取不到（没有任何本机工具 / 工具退出非 0 / 空内容）⇒ `None`。
///
/// **只走本机工具，不发 OSC 52 读请求**：`OSC 52` 的**读**形态要求终端把内容回写到 stdin，
/// 而 stdin 正被 crossterm 的事件读取占着 —— 两路读同一字节流会互相吞事件（终端回的应答
/// 一旦被事件解析器吃掉，用户按的键也一并丢）。写是单向的，读不是，故读取只走工具。
/// 后果：SSH / 容器里没有工具时 `Ctrl+V` 无内容可粘 —— 那时用终端的粘贴键
/// （`Ctrl+Shift+V` / `⌘V`，它由终端自己把内容塞进 bracketed paste）。
///
/// **同步**：读必须在把内容插进缓冲**之前**完成（用户按 `Ctrl+V` 期待的是当场出现文本），
/// 而这是 UI 线程上的一次短命子进程（本地文件/进程通信，毫秒级）。写入路径可以异步
/// （不等结果），读取不能。
fn read_system_clipboard() -> Option<String> {
    for (program, args) in clipboard_readers() {
        if let Some(text) = run_reader(program, &args) {
            // 空白剪贴板等同「没内容」：粘一段纯空格比什么都不做更意外。
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// 各平台的剪贴板**读取**命令（顺序与 [`clipboard_writers`] 一致：Wayland 优先）。
fn clipboard_readers() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut v: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|s| !s.is_empty()) {
        v.push(("wl-paste", vec!["--no-newline"]));
    }
    v.push(("xsel", vec!["--clipboard", "--output"]));
    v.push(("xclip", vec!["-selection", "clipboard", "-o"]));
    v.push(("pbpaste", vec![]));
    v.push(("wl-paste", vec!["--no-newline"]));
    v
}

/// 跑一个读取工具并取它的 stdout；命令缺失 / 非 0 退出 / 输出不是 UTF-8 ⇒ `None`。
///
/// `--no-newline`（`wl-paste` 默认补一个换行）与 `xsel` 的输出会带尾随换行 —— 那是
/// 工具的行格式而不是用户复制的内容，**按终端的口径保留原样**：粘贴时多出的换行
/// 落在输入框里是可见的，用户能删；反过来猜「哪个换行是工具加的」必然猜错一半。
fn run_reader(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// 把文本写进**系统**剪贴板。
///
/// 两条路**都走**，不去探测终端支不支持 OSC 52：
///
/// 1. **终端原生工具**（`xsel` / `xclip` / `pbcopy` / `wl-copy` / `clip.exe`）—— 命中即
///    真的进了系统剪贴板，`⇧`+拖选之外的任何粘贴入口都能用（包括在别的 app 里粘贴）。
/// 2. **OSC 52** —— 兜住 SSH / 无 X 的环境，且终端若把它落到自己的剪贴板就同样可用。
///
/// 为什么不二选一：两者**互不排斥**，而失败都是静默的（终端可以忽略 OSC 52；没有工具的
/// 机器上命令直接 `ENOENT`）。分头探测得先知道「终端到底支不支持」，那个信息拿不到
/// （写入是单向的，无应答可等），所以只能两边都发。
///
/// **写第一条**的是工具：它成功时 OSC 52 只作冗余；工具不存在时序列已经发出去了。
fn write_clipboard(text: &str) {
    write_system_clipboard(text);
    let mut out = std::io::stdout();
    let _ = out.write_all(clipboard_sequence(text).as_bytes());
    let _ = out.flush();
}

/// 各平台的剪贴板写入命令（**只选本机存在的一个**，按序优先）。
///
/// `WAYLAND_DISPLAY` 有值时 Wayland 工具优先：Wayland 会话里 `xsel` 可能连得上（XWayland），
/// 但写进去的剪贴板只有 X 客户端看得见，浏览器 / 原生 Wayland 应用粘不到 ——
/// 那比「写不进去」更难排查。
fn clipboard_writers() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut v: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|s| !s.is_empty()) {
        v.push(("wl-copy", vec![]));
    }
    v.push(("xsel", vec!["--clipboard", "--input"]));
    v.push(("xclip", vec!["-selection", "clipboard"]));
    v.push(("pbcopy", vec![]));
    v.push(("wl-copy", vec![]));
    v.push(("clip.exe", vec![]));
    v
}

/// 本机命令行剪贴板工具（不依赖任何 crate）。
///
/// **全异步**：写入要 fork/exec 一个进程，而 UI 线程是个 `block_on` 驱动的大循环
/// （见 `lib.rs` 的线程模型）；同步等子进程退出会在每次复制上搭一笔进程启动成本。
/// 于是 `spawn` 之后立即返回，由子进程自己等 stdin 关、自己退。
///
/// **stdin 写完就 drop**：`xsel` / `xclip` 靠 stdin 的 EOF 判断「粘完了」，不关它们
/// 会永远挂着（不单是泄漏 —— `xsel` 会一直占着 clipboard selection）。
///
/// 进程组：子进程是 UI 线程的直系子进程，不额外 `setsid` —— 它是一个短命进程，
/// 且 stdin 已关，不会读到终端的输入。
fn write_system_clipboard(text: &str) {
    for (program, args) in clipboard_writers() {
        if spawn_writer(program, &args, text) {
            return;
        }
    }
}

/// 启动一个写入工具并把 `text` 输到它的 stdin；`true` = 成功发出。
///
/// 抽成函数是为了可测：「子进程真的收到了字节」与「stdin 已关」两件事都能用
/// `sh -c 'cat > 文件'` 当假工具验到（见单测 `test_spawn_writer_feeds_child_stdin`）。
///
/// 返回后**不等子进程**（见 [`write_system_clipboard`]）。
fn spawn_writer(program: &str, args: &[&str], text: &str) -> bool {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        // 工具不存在 / 无权限：试下一个。**不打印** —— 复制失败时界面多一行
        // 与操作无关的报错，比静默失败更吵。
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        // drop 即关闭 ⇒ 子进程读到 EOF，开始退出。
    }
    // `xsel` **写完后不会自己退**：它要一直持有 selection，直到别的程序接管它
    // （这是 X11 clipboard 的语义：selection 的拥有者必须是个活进程）。
    // 于是这里**不** wait、不 kill —— 让它后台挂着；下次复制时新的 `xsel` 会
    // 接管 selection，旧的自动退出。
    true
}

/// 终端状态恢复的字节序列：开换行 / 退 alt-screen / 关括号粘贴 / 关鼠标捕获 / 弹键盘增强。///
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

// ---------------------------------------------------------------------------
// 主题浮层（实时预览）
// ---------------------------------------------------------------------------

/// 主题浮层的**状态**。
///
/// `opened_with` 是打开那一刻的 `App.theme` —— Esc / Ctrl-C 拿它还原（预览把主题
/// 改过很多次了，「关掉浮层」必须回到用户进来时的样子）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThemeModalState {
    pub opened_with: ThemeKind,
    pub selected: ThemeKind,
}

/// 主题浮层关心的键。其余键（`Backspace` / `Left` / `Paste` …）一律 [`ThemeKey::Other`]
/// —— 本浮层**无查询框**，不做过滤。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeKey {
    Up,
    Down,
    Enter,
    Esc,
    Other,
}

/// 一次按键的**纯决策结果**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeModalStep {
    /// 高亮移到了新主题：调用方应**立即** `app.set_theme(kind)`（预览）。
    Preview(ThemeKind),
    /// 取消：调用方应 `app.set_theme(kind)` 还原并关掉浮层（**不落盘**）。
    Restore(ThemeKind),
    /// 确认：关掉浮层，`kind` 由调用方单向同步给 app 落盘。
    Confirm(ThemeKind),
    /// 无动作（本浮层不用的键）。
    Ignore,
}

/// 「按键 → 下一步」的**纯函数**，不碰 `App`、不画屏（C11 的落点）。
///
/// # 谁推进 `selected`
///
/// **本函数不改 `ThemeModalState`** —— 它只读 `state` 给出决策。推进责任在调用方：
/// 收到 [`ThemeModalStep::Preview`]`(k)` 时，把 `state.selected = k`（同时
/// `app.set_theme(k)`）。**不要**在函数里也推一次，否则预览与高亮会各走一条
/// 推进逻辑而分叉（`↓` 一帧跳两格、或高亮停在旧项而屏上已是新色）。
///
/// `Esc` / `Enter` 返回的 `kind` 不变 `selected`：前者的目标是 `opened_with`（回溯
/// 到桌子起点，不是移到另一项），后者的目标是当前 `selected`（已经对）。
pub(crate) fn theme_modal_step(state: ThemeModalState, key: ThemeKey) -> ThemeModalStep {
    match key {
        ThemeKey::Down => ThemeModalStep::Preview(state.selected.next()),
        ThemeKey::Up => ThemeModalStep::Preview(state.selected.prev()),
        // 恒用 `opened_with`（而不是 `selected`）—— 用户可能已移动了十几格，
        // 「取消」的语义是回到进来时的样子，不是后退一步。
        ThemeKey::Esc => ThemeModalStep::Restore(state.opened_with),
        ThemeKey::Enter => ThemeModalStep::Confirm(state.selected),
        ThemeKey::Other => ThemeModalStep::Ignore,
    }
}

/// 主题浮层的**结局** —— 决定要不要发 `Request::SetTheme`（= 要不要落盘）。
///
/// 两个出口的区别就是 D3 的核心：**预览既然已本地生效，取消就不能留下痕迹**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeModalClose {
    /// Enter：`kind` 是用户确认的主题。
    Confirmed(ThemeKind),
    /// Esc（以及 Ctrl-C 的**还原语义**）：主题已回到打开时的值，**不发请求、不落盘**。
    Cancelled,
}

impl ThemeModalClose {
    /// 本结局要发出的请求 —— [`Self::Cancelled`] 恒为 `None`（不落盘）。
    fn request(&self) -> Option<Request> {
        match self {
            ThemeModalClose::Confirmed(kind) => Some(Request::SetTheme {
                theme: kind.name().to_string(),
            }),
            ThemeModalClose::Cancelled => None,
        }
    }

    /// 确认时返回选中的主题（用于本地回显）；取消时为 `None`。
    fn confirmed(&self) -> Option<ThemeKind> {
        match self {
            ThemeModalClose::Confirmed(kind) => Some(*kind),
            ThemeModalClose::Cancelled => None,
        }
    }
}

/// 把一次按键**施加**到浮层状态与 app 上 —— 宿主层的纯逻辑，`apply` 是可测的
/// 「设主题」回调（生产路径上是 `App::set_theme`）。
///
/// 返回 `Some(close)` 表示浮层该关，`None` 表示继续。**`selected` 的推进在这里**
/// （见 [`theme_modal_step`] 的说明）：`Preview(k)` ⇒ 先写 `state.selected = k`
/// 再调 `apply(k)`，两件事同处一步，不会分叉。
fn apply_theme_step(
    state: &mut ThemeModalState,
    key: ThemeKey,
    apply: &mut dyn FnMut(ThemeKind),
) -> Option<ThemeModalClose> {
    match theme_modal_step(*state, key) {
        ThemeModalStep::Preview(kind) => {
            state.selected = kind;
            apply(kind);
            None
        }
        ThemeModalStep::Restore(kind) => {
            apply(kind);
            Some(ThemeModalClose::Cancelled)
        }
        ThemeModalStep::Confirm(kind) => Some(ThemeModalClose::Confirmed(kind)),
        ThemeModalStep::Ignore => None,
    }
}

/// 主题在 [`ThemeKind::ALL`] 里的下标 —— 浮层高亮用（`ALL` 顺序即展示顺序）。
fn theme_index(kind: ThemeKind) -> usize {
    ThemeKind::ALL
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    // -----------------------------------------------------------------------
    // `/copy` 的载荷：OSC 52 剪贴板序列（含内联 base64）
    // -----------------------------------------------------------------------

    /// OSC 52 的形状：`ESC ] 52 ; c ; <base64> BEL`。
    ///
    /// 变异：把 `c` 改成 `p`（主选择 vs 剪贴板）、或把 BER 终止符 `\x07` 换掉 ⇒ 红。
    #[test]
    fn test_clipboard_sequence_shape() {
        let seq = clipboard_sequence("hi");
        assert!(seq.starts_with("\x1b]52;c;"), "前缀不对：{seq:?}");
        assert!(seq.ends_with('\x07'), "须以 BEL 终止：{seq:?}");
        assert_eq!(&seq[7..seq.len() - 1], "aGk=", "payload 应为 base64");
    }

    /// **一轮里排空剪贴板的顺序**：被推迟的键盘事件必须先被处理，否则
    /// 「按 `Ctrl+C` 复制」要等下一个事件才生效（用户看到的是「没反应」）。
    ///
    /// 这是 `handle_round` 的存在理由 —— 变异：把 `deferred` 那段挪到 `clip` 之后
    /// ⇒ 本用例红（`clip` 收到零次调用）。
    #[test]
    fn test_round_writes_clipboard_from_deferred_key() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (req_tx, _req_rx) = mpsc::channel(8);
        let (bnd_tx, _bnd_rx) = mpsc::channel(8);
        let mut app = App::new(CodingView::for_test());
        app.input.insert_str("要复制的草稿");
        app.input.select_all();

        // 键盘事件走的是 `deferred`（鼠标合并时被推迟的那条路）。
        let key = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let mut written: Vec<String> = Vec::new();
        handle_round(
            &mut app,
            None,
            Some(key),
            &req_tx,
            &bnd_tx,
            120,
            40,
            &mut |t: &str| written.push(t.to_string()),
            &mut || None,
        );
        assert_eq!(
            written,
            vec!["要复制的草稿".to_string()],
            "同一轮里就要写出"
        );
        assert!(app.pending_clipboard.is_none(), "写完即清");
    }

    /// 没有可写内容时**一届都不调**（不得凭空写空串 —— 那会清空用户的剪贴板）。
    #[test]
    fn test_round_without_clipboard_never_calls_writer() {
        let (req_tx, _req_rx) = mpsc::channel(8);
        let (bnd_tx, _bnd_rx) = mpsc::channel(8);
        let mut app = App::new(CodingView::for_test());
        let mut calls = 0;
        handle_round(
            &mut app,
            None,
            None,
            &req_tx,
            &bnd_tx,
            120,
            40,
            &mut |_: &str| calls += 1,
            &mut || None,
        );
        assert_eq!(calls, 0);
    }

    /// `Ctrl+V` → 读剪贴板 → 插进输入框；读不到（`None`）⇒ **不动输入框**
    /// （不清空、不插空串 —— 「按了没反应」比「草稿被清空」好）。
    ///
    /// 变异：把 `insert_str` 换成 `clear` / 在读失败时仍清输入框 ⇒ 红。
    #[test]
    fn test_round_pastes_into_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (req_tx, _req_rx) = mpsc::channel(8);
        let (bnd_tx, _bnd_rx) = mpsc::channel(8);
        let mut app = App::new(CodingView::for_test());
        app.input.insert_str("草稿");

        let key = Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        let mut written: Vec<String> = Vec::new();
        handle_round(
            &mut app,
            None,
            Some(key),
            &req_tx,
            &bnd_tx,
            120,
            40,
            &mut |t: &str| written.push(t.to_string()),
            &mut || Some("粘进来的".to_string()),
        );
        assert_eq!(app.input.text, "草稿粘进来的", "插在光标处");
        assert!(!app.pending_paste, "读完即清标志");
        assert!(written.is_empty(), "读不是写：不得碰写剪贴板的出口");

        // 读失败：输入框保持原样（连标志一起清掉，否则下一轮会莫名其妙读一次）。
        app.pending_paste = true;
        handle_round(
            &mut app,
            None,
            None,
            &req_tx,
            &bnd_tx,
            120,
            40,
            &mut |_: &str| {},
            &mut || None,
        );
        assert_eq!(app.input.text, "草稿粘进来的", "读不到时不动输入框");
        assert!(!app.pending_paste, "失败也要清标志");
    }

    /// 同一轮里**先读后写**（ADR-0059 决策 4）：反过来会把自己刚复制的文本粘回输入框。
    ///
    /// 变异：把 `read` 那一段挪到 `clip` 之后 ⇒ `read` 观察到的是「已经写过」⇒ 红。
    #[test]
    fn test_round_reads_before_writing() {
        use std::cell::Cell;
        let (req_tx, _req_rx) = mpsc::channel(8);
        let (bnd_tx, _bnd_rx) = mpsc::channel(8);
        let mut app = App::new(CodingView::for_test());

        let wrote = Cell::new(false);
        let read_saw_write = Cell::new(false);
        handle_round(
            &mut app,
            None,
            None,
            &req_tx,
            &bnd_tx,
            120,
            40,
            &mut |_: &str| wrote.set(true),
            &mut || {
                read_saw_write.set(wrote.get());
                None
            },
        );
        // 这一轮两条都没内容（都空），单独构造：读在前、写在后。
        app.pending_paste = true;
        app.pending_clipboard = Some("刚复制的".to_string());
        handle_round(
            &mut app,
            None,
            None,
            &req_tx,
            &bnd_tx,
            120,
            40,
            &mut |_: &str| wrote.set(true),
            &mut || {
                read_saw_write.set(wrote.get());
                None
            },
        );
        assert!(
            !read_saw_write.get(),
            "读必须在写之前：否则会把自己刚复制的文本粘回输入框"
        );
    }

    /// 本机剪贴板工具链的**形态**：每个候选都带完整参数，且 `xsel` 明确指定
    /// `--clipboard`（不带它写的是 PRIMARY selection，而粘贴读的是 CLIPBOARD）。
    ///
    /// 变异：把 `xsel` 的参数删成空 ⇒ 本用例红（那是「复制了但粘不出来」的真实形态）。
    #[test]
    fn test_clipboard_writers_are_well_formed() {
        let writers = clipboard_writers();
        let xsel = writers
            .iter()
            .find(|(p, _)| *p == "xsel")
            .expect("xsel 必须在候选链里");
        assert_eq!(
            xsel.1,
            vec!["--clipboard", "--input"],
            "xsel 必须写 CLIPBOARD"
        );
        let xclip = writers
            .iter()
            .find(|(p, _)| *p == "xclip")
            .expect("xclip 必须在候选链里");
        assert_eq!(xclip.1, vec!["-selection", "clipboard"]);
        // macOS / Windows 兜底：两个都不带参数（它们本来就只写剪贴板）。
        assert!(writers.iter().any(|(p, a)| *p == "pbcopy" && a.is_empty()));
        assert!(writers.iter().any(|(p, _)| *p == "clip.exe"));
    }

    /// 子进程**真的收到了字节**，且 stdin 被关（否则工具永远等 EOF）。
    ///
    /// 用 `sh -c 'cat > 文件'` 当假工具：它读到 EOF 就退，文件内容即 stdin 的全部字节。
    ///
    /// 变异：把 `drop(stdin)` 删掉（守着 `stdin` 不放）⇒ `cat` 永不退 ⇒ 文件为空 ⇒ 红。
    #[test]
    fn test_spawn_writer_feeds_child_stdin_and_closes_it() {
        let dir = std::env::temp_dir().join(format!(
            "yushan-clip-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("got.txt");
        let script = format!("cat > {}", out.display());
        let ok = spawn_writer("sh", &["-c", &script], "剪贴板内容 abc");
        assert!(ok, "spawn 应成功");

        // 子进程是异步的：等文件写出内容（最多 2s），不 sleep 一个固定值。
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = String::new();
        while Instant::now() < deadline {
            got = std::fs::read_to_string(&out).unwrap_or_default();
            if got == "剪贴板内容 abc" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got, "剪贴板内容 abc", "子进程 stdin 内容应当是待复制文本");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 命令不存在时返回 `false`（让调用方试下一个），不 panic。
    #[test]
    fn test_spawn_writer_reports_missing_program() {
        assert!(!spawn_writer("yushan-no-such-clipboard-tool-xyz", &[], "x"));
    }

    /// Wayland 会话里 `wl-copy` 必须**排第一**：挂在 XWayland 上的 `xsel` / `xclip`
    /// 写得进去，但原生 Wayland 应用（含浏览器）粘不到。
    ///
    /// 变异：把 wl-copy 的优先级顺序调换 ⇒ 本用例红。
    #[test]
    fn test_clipboard_writer_prefers_wayland_when_present() {
        // 进程级 env 改写：本仓的 tui crate 里只有这一处需要，故就地锁一把，
        // 不去引 apps 侧的 `test_env`（方向不允许：tui 不依赖 apps）。
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("WAYLAND_DISPLAY");
        // SAFETY: 已持锁，同一时刻只有一个测试改这个变量。
        unsafe { std::env::set_var("WAYLAND_DISPLAY", "wayland-9") };
        let first = clipboard_writers()[0].0;
        match prev {
            // SAFETY: 同上。
            Some(v) => unsafe { std::env::set_var("WAYLAND_DISPLAY", v) },
            None => unsafe { std::env::remove_var("WAYLAND_DISPLAY") },
        }
        assert_eq!(first, "wl-copy", "Wayland 会话里 wl-copy 必须优先");
    }

    /// base64 三种余数（0/1/2 字节）都要对 —— 这是手写编码最容易错的地方。
    ///
    /// 参照：`""` → `""`、`"a"` → `"YQ=="`、`"ab"` → `"YWI="`、`"abc"` → `"YWJj"`。
    #[test]
    fn test_clipboard_base64_covers_all_remainders() {
        let payload = |s: &str| {
            let seq = clipboard_sequence(s);
            seq[7..seq.len() - 1].to_string()
        };
        assert_eq!(payload(""), "");
        assert_eq!(payload("a"), "YQ==");
        assert_eq!(payload("ab"), "YWI=");
        assert_eq!(payload("abc"), "YWJj");
        assert_eq!(payload("abcd"), "YWJjZA==");
    }

    /// UTF-8 多字节必须按**字节**编码，不能按字符 —— 中文是这档的高发区。
    #[test]
    fn test_clipboard_base64_encodes_utf8_bytes() {
        let payload = |s: &str| {
            let seq = clipboard_sequence(s);
            seq[7..seq.len() - 1].to_string()
        };
        // "中" = E4 B8 AD
        assert_eq!(payload("中"), "5Lit");
        // "中文" = E4 B8 AD E6 96 87
        assert_eq!(payload("中文"), "5Lit5paH");
    }

    /// 正文里的控制字符（含 ESC）不得截断序列 —— 它们是 payload 的一部分，
    /// 编码后只出现 base64 字母表内的字符。
    ///
    /// 安全含义：文本直接拼进转义序列时，正文里的 `\x07` 会提前终止 OSC，
    /// 之后的内容会被终端当普通输出执行。
    #[test]
    fn test_clipboard_payload_never_leaks_control_chars() {
        let seq = clipboard_sequence("evil\x07\x1b]52;c;AAAA\x07");
        assert_eq!(
            seq.matches('\x07').count(),
            1,
            "正文里的 BEL 不得进入序列：{seq:?}"
        );
        let payload = &seq[7..seq.len() - 1];
        assert!(
            payload
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "payload 只含 base64 字母表：{payload}"
        );
    }

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

    // -----------------------------------------------------------------------
    // C11：主题浮层的预览 / 还原 / 确认三态
    // -----------------------------------------------------------------------

    /// 纯函数的四个转移，对 `(state, key)` 穷举：5 个起始主题 × 5 个 `selected`。
    ///
    /// 变异：把 `Esc` 臂改成 `Confirm(state.selected)` ⇒ 本用例与
    /// `test_theme_modal_escape_restores_opened_with` 同时变红。
    #[test]
    fn test_theme_modal_step_transitions() {
        for opened in ThemeKind::ALL {
            for selected in ThemeKind::ALL {
                let state = ThemeModalState {
                    opened_with: opened,
                    selected,
                };
                assert_eq!(
                    theme_modal_step(state, ThemeKey::Down),
                    ThemeModalStep::Preview(selected.next()),
                    "Down ⇒ next"
                );
                assert_eq!(
                    theme_modal_step(state, ThemeKey::Up),
                    ThemeModalStep::Preview(selected.prev()),
                    "Up ⇒ prev"
                );
                assert_eq!(
                    theme_modal_step(state, ThemeKey::Enter),
                    ThemeModalStep::Confirm(selected),
                    "Enter ⇒ 当前选中项"
                );
                assert_eq!(
                    theme_modal_step(state, ThemeKey::Other),
                    ThemeModalStep::Ignore,
                    "Other ⇒ Ignore"
                );
            }
        }
    }

    /// `Esc` 还原的是 **`opened_with`**，不是 `selected`，也不是「后退一步」——
    /// 用户可能已经移动了十几格，取消的语义是「回到进来时的样子」。
    ///
    /// 变异：把 `Esc` 臂改成 `Restore(state.selected)` ⇒ 本用例变红。
    #[test]
    fn test_theme_modal_escape_restores_opened_with() {
        let opened = ThemeKind::AyuMirage;
        // 逐个试「已移开很多格」的选择：每一个都必须还原到 opened。
        for selected in ThemeKind::ALL {
            let state = ThemeModalState {
                opened_with: opened,
                selected,
            };
            assert_eq!(
                theme_modal_step(state, ThemeKey::Esc),
                ThemeModalStep::Restore(opened),
                "selected={} 时仍须还原到 opened",
                selected.name()
            );
        }
    }

    /// 环形闭合的两个具体端点（与 `theme.rs` 的互逆用例互补：这里钉的是
    /// 浮层首尾相接的两个方向，用户连按到底不会停住）。
    #[test]
    fn test_theme_modal_wraps_around() {
        assert_eq!(
            ThemeKind::SolarizedDark.next(),
            ThemeKind::AyuMirage,
            "最后一个的下一个是第一个"
        );
        assert_eq!(
            ThemeKind::AyuMirage.prev(),
            ThemeKind::SolarizedDark,
            "第一个的上一个是最后一个"
        );
    }

    /// 宿主层：「按键序列 → set_theme 调用序列」—— `Preview` 必须**每次都调**
    /// applier，`Esc` 后最终值是 `opened_with`，且 Esc 那条路**不发请求**（不落盘）。
    ///
    /// 变异：把 `apply_theme_step` 的 `Preview` 臂里的 `apply(kind)` 删掉（只推
    /// `selected`）⇒ 调用序列变短、变红；把 `Restore` 臂的 `apply(kind)` 删掉 ⇒
    /// 最终值断言变红。
    #[test]
    fn test_theme_modal_applies_every_preview_then_restores() {
        let opened = ThemeKind::Nord;
        let mut state = ThemeModalState {
            opened_with: opened,
            selected: opened,
        };
        let mut applied: Vec<ThemeKind> = Vec::new();

        // ↓↓↑：每格都预览一次（含回退到同值）。
        for key in [ThemeKey::Down, ThemeKey::Down, ThemeKey::Up] {
            assert!(
                apply_theme_step(&mut state, key, &mut |k| applied.push(k)).is_none(),
                "移动键不关浮层"
            );
        }
        assert_eq!(
            applied,
            vec![opened.next(), opened.next().next(), opened.next()],
            "每次 Preview 都要调 applier"
        );
        assert_eq!(state.selected, opened.next(), "selected 由调用方推进");

        // Esc：还原到 opened，且**不发请求**。
        let close = apply_theme_step(&mut state, ThemeKey::Esc, &mut |k| applied.push(k))
            .expect("Esc 必须关浮层");
        assert_eq!(close, ThemeModalClose::Cancelled);
        assert_eq!(
            applied.last().copied(),
            Some(opened),
            "Esc 后最终 applier 收到的是 opened_with"
        );
        assert!(
            close.request().is_none(),
            "Esc 不得发 Request::SetTheme（不落盘）"
        );
        assert!(close.confirmed().is_none(), "Esc 不写本地回显");
    }

    /// `Enter` ⇒ `Confirmed(selected)`，且它**是**唯一会产生 `Request::SetTheme`
    /// 的出口（名字用 kebab-case，协议只收这一种）。
    ///
    /// 变异：把 `request()` 改成返回 `None` ⇒ 本用例变红（确认后不落盘）。
    #[test]
    fn test_theme_modal_confirm_requests_set_theme() {
        let mut state = ThemeModalState {
            opened_with: ThemeKind::AyuMirage,
            selected: ThemeKind::AyuMirage,
        };
        let mut applied: Vec<ThemeKind> = Vec::new();

        // ↓ 到 Gruvbox，再 Enter —— 确认的是**预览到的那一个**。
        assert!(apply_theme_step(&mut state, ThemeKey::Down, &mut |k| applied.push(k)).is_none());
        let close = apply_theme_step(&mut state, ThemeKey::Enter, &mut |k| applied.push(k))
            .expect("Enter 必须关浮层");
        assert_eq!(close, ThemeModalClose::Confirmed(ThemeKind::GruvboxDark));
        assert_eq!(close.confirmed(), Some(ThemeKind::GruvboxDark));
        assert_eq!(
            close.request(),
            Some(Request::SetTheme {
                theme: "gruvbox-dark".into()
            }),
            "确认才落盘，且载荷是 kebab-case 名"
        );
        assert_eq!(
            applied,
            vec![ThemeKind::GruvboxDark],
            "Enter 本身不再调 applier（预览已设过）"
        );
    }

    /// 本浮层**不认的键**既不改状态也不关浮层（无查询框，不做过滤）。
    #[test]
    fn test_theme_modal_ignores_other_keys() {
        let mut state = ThemeModalState {
            opened_with: ThemeKind::Nord,
            selected: ThemeKind::EverforestDark,
        };
        let mut applied: Vec<ThemeKind> = Vec::new();
        assert!(apply_theme_step(&mut state, ThemeKey::Other, &mut |k| applied.push(k)).is_none());
        assert!(applied.is_empty(), "Ignore 不得碰 app");
        assert_eq!(state.selected, ThemeKind::EverforestDark, "state 原封不动");
    }

    /// 选题浮层高亮的下标与 `ThemeKind::ALL` 顺序一致（`Modal::Select` 的
    /// `selected` 是 `options` 下标，而 `options` 直接从 `ALL` 派生）。
    #[test]
    fn test_theme_index_mirrors_all_order() {
        for (i, kind) in ThemeKind::ALL.iter().enumerate() {
            assert_eq!(theme_index(*kind), i, "{}", kind.name());
        }
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
