//! 后台命令的两个工具 —— `bash_bg`（启动）与 `bash_poll`（取增量）。
//!
//! **两个工具都不准并行**（[`Tool::is_concurrency_safe`] 皆 `false`）：
//! - `bash_bg` 启动会 spawn 进程 ⇒ 有副作用；
//! - `bash_poll` 虽对模型是只读，但它推进注册表里的**交付偏移**
//!   ⇒ 并发 poll 时模型可能重复看到同一段输出 ⇒ 对模型可观测 ⇒ 判不安全。
//!
//! **cwd 契约**：用**工具自带的 workspace**（与 `bash` 同一条），**不是** `ctx.cwd`
//! ⇒ 同批调用里的两条命令落在同一目录，模型不会误判位置。
//!
//! **输出是 stdout+stderr 的合并流** —— 既有 `bash` 是分开收再拼；后台形态下
//! 两者按写入顺序交错，模型无法区分来源 ⇒ 这是后台形态必须接受的差异。
//!
//! **交付语义三条**（缺一不可）：
//! ① 已送达的字节**不重复**送达；② 未送达的字节必须**明示**
//! （报跳过量 + 完整输出路径）；③ **不静默丢弃**。
//!
//! **末行（没有换行符的那一段）的处置** —— 分两种情形：
//! - **运行中** ⇒ 末段**不交付**，留在 `consumed` 之后等下一次 poll
//!   ⇒ 不切半行、不切多字节字符；
//! - **终态**（进程已结束**且已收尸** ⇒ 输出文件不会再增长）⇒ 末段**就是最后一行**
//!   ⇒ 整段交付、`consumed` 推到文件尾 —— 否则进程退出后再无新字节
//!   ⇒ 末段**永久丢失**（模型只看到「产出 N 字节、0 字节新」）。
//!
//! 两种情形下**未交付的字节都要报出数量** ⇒ 不静默丢弃。
//!
//! **措辞纪律**：「被 harness 杀掉」与「命令自身非零退码」必须用**不同措辞** ——
//! 否则模型会把被杀误报成「测试挂了」。
//!
//! **输出上限的归因**（模型可见契约）：输出文件超过上限 ⇒ 进程被 **harness** 杀掉
//! ⇒ poll 报「killed by the harness: output exceeded the … output cap」。
//! 内核侧的文件大小限额是**粗闸门**（进程级、被子孙继承）⇒ 正常写大文件的命令
//! 碰不到它 ⇒ 「被内核文件大小限额杀掉」是另一回事，措辞必须区分开。

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use crate::background::{
    BgError, BgHandle, BgSnapshot, BgState, BackgroundTasks, KERNEL_FSIZE_SIGNAL, SessionSlot,
};
use crate::bg_output::{assemble, format_bytes};

/// `bash_bg` 的工具级墙钟上限 —— 启动本身只是几个同步 syscall（~1ms）。
const BG_TIMEOUT: Duration = Duration::from_secs(30);

/// `bash_poll` 的工具级墙钟上限 —— `wait_seconds`（≤ 20s）+ 余量。
const POLL_TIMEOUT: Duration = Duration::from_secs(35);

/// `wait_seconds` 的上限 —— 小于一次模型往返的量级（实测均值 17.3s）。
const MAX_WAIT_SECONDS: u64 = 20;

/// **终态收尾读**的硬上限 —— 单次 poll 的临时缓冲不得无界增长。
///
/// 收尾读要**整段**读出末行（否则末行永远交付不了）⇒ 上限取输出上限（默认 2 MiB）；
/// 再加一道硬闸门 ⇒ 缓冲有界（超出部分留给下一次 poll，**报出**待交付字节数）。
const END_READ_HARD_LIMIT: u64 = 8 * 1024 * 1024;

/// 轮询等待时的检查间隔 —— **异步** sleep，不阻塞 runtime 线程。
const WAIT_TICK: Duration = Duration::from_millis(50);

/// `bash_bg` —— 启动一条后台命令并**立刻**返回句柄。
pub struct BashBgTool {
    backgrounds: Arc<BackgroundTasks>,
    /// 工作目录 —— **工具自带**，与 `bash` 同一条契约。
    workspace: PathBuf,
    /// 当前会话 id 的共享槽（`/new`、`/resume` 后由 app 层切换）。
    session: SessionSlot,
}

impl BashBgTool {
    /// 装配工具 —— `backgrounds` 与两个工具共享同一份注册表。
    pub fn new(
        backgrounds: Arc<BackgroundTasks>,
        workspace: PathBuf,
        session: SessionSlot,
    ) -> Self {
        Self {
            backgrounds,
            workspace,
            session,
        }
    }
}

/// `bash_poll` —— 取一条后台命令的状态与**自上次轮询以来**的增量输出。
pub struct BashPollTool {
    backgrounds: Arc<BackgroundTasks>,
    /// 当前会话 id 的共享槽。
    session: SessionSlot,
}

impl BashPollTool {
    /// 装配工具 —— `backgrounds` 与两个工具共享同一份注册表。
    pub fn new(backgrounds: Arc<BackgroundTasks>, session: SessionSlot) -> Self {
        Self { backgrounds, session }
    }

    /// 有界等待命令结束 —— **异步** sleep，不阻塞 runtime 线程。
    ///
    /// 等待只到 `wait` 为止（到点即返回当前状态）；命令没结束也不挂死。
    async fn wait_for_end(&self, handle: BgHandle, session: &str, wait: Duration) {
        let deadline = Instant::now() + wait;
        loop {
            let Some(snapshot) = self.backgrounds.snapshot(handle, session) else {
                return;
            };
            if snapshot.state != BgState::Running {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            tokio::time::sleep(WAIT_TICK.min(deadline - now)).await;
        }
    }

    /// 「未知句柄」的结果 —— 明确报错，并列出该会话仍在跑的句柄。
    fn unknown_handle(&self, handle_text: &str, session: &str) -> ToolResult {
        let running = self.backgrounds.running_handles(session);
        let list = if running.is_empty() {
            "no running handles".to_string()
        } else {
            let labels: Vec<String> = running.iter().map(|handle| handle.label()).collect();
            format!("running handles: {}", labels.join(", "))
        };
        ToolResult {
            content: format!("[bash_poll] unknown handle \"{handle_text}\" — {list}"),
            is_error: true,
        }
    }
}

#[async_trait::async_trait]
impl Tool for BashBgTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash_bg",
            "Start a shell command in the background and get a handle back immediately; \
             the command keeps running while you do other work. Collect it later with \
             bash_poll. Use this for commands that take more than a few seconds (test suites, \
             builds); use bash for quick commands.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Max lifetime in seconds (default 900, max 3600)" }
                },
                "required": ["command"]
            }),
        )
    }

    /// 启动会 spawn 进程 ⇒ 有副作用 ⇒ **永不准开并行**。
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn timeout(&self) -> Option<Duration> {
        Some(BG_TIMEOUT)
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'command'".into()))?;
        if command.trim().is_empty() {
            return Err(ToolError::InvalidInput("'command' is empty".into()));
        }
        let timeout = input["timeout"].as_u64().map(Duration::from_secs);
        let session = self.session.get();

        // 启动失败**同步报错** —— 绝不返回「永远就绪」的句柄，注册表里也不留条目。
        let handle = self
            .backgrounds
            .start(&session, command, &self.workspace, timeout)
            .map_err(|error| match error {
                // **文案只指向模型真能做的动作** —— 模型面**没有 kill 入口**：
                // `bash_bg` 只能 start、`bash_poll` 只能 poll，而 poll **不会**让
                // `Running` 条目离开 `Running` ⇒ 「poll or kill one」是个**做不到**的
                // 动作 ⇒ 模型起满长命令后会被锁死（既停不掉、也推不动）。
                // 模型能做的只有「等一条跑完」⇒ 额度一空就能再起一条。
                BgError::TooManyRunning { limit } => ToolError::Execution(format!(
                    "[bash_bg] too many background commands are running ({limit}) — \
                     wait for one to finish, then start another"
                )),
                // 表满 ⇒ 全部条目都在跑 ⇒ 同样只能等（腾出的条目由下次 start 淘汰）。
                BgError::TableFull { limit } => ToolError::Execution(format!(
                    "[bash_bg] background command table is full ({limit}) — \
                     wait for one to finish, then start another"
                )),
                BgError::Spawn { source, .. } => {
                    ToolError::Execution(format!("[bash_bg] failed to start command: {source}"))
                }
                other => {
                    ToolError::Execution(format!("[bash_bg] failed to start command: {other}"))
                }
            })?;

        let snapshot = self
            .backgrounds
            .snapshot(handle, &session)
            .ok_or_else(|| ToolError::Execution(format!(
                "[bash_bg] handle {} vanished right after start",
                handle.label()
            )))?;

        Ok(ToolResult {
            content: format!(
                "Started in background.\nhandle: {}\npid: {}\ncwd: {}\noutput: {}\nlifetime: {}s\n\
                 Collect it with bash_poll(handle=\"{}\"). The command is killed when this session \
                 ends, is aborted, or exceeds its timeout.",
                snapshot.handle.label(),
                snapshot.pid,
                snapshot.cwd.display(),
                snapshot.output_path.display(),
                snapshot.timeout.as_secs(),
                snapshot.handle.label(),
            ),
            is_error: false,
        })
    }
}

#[async_trait::async_trait]
impl Tool for BashPollTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash_poll",
            "Check a background command started with bash_bg: returns its state plus the output \
             produced since the last poll. The full output stays on disk — grep or read that \
             path for any part you missed instead of running the command again.",
            json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "Handle returned by bash_bg" },
                    "wait_seconds": { "type": "number", "description": "Wait up to N seconds for the command to finish (default 0, max 20)" }
                },
                "required": ["handle"]
            }),
        )
    }

    /// 虽对模型是只读，但它推进注册表里的**交付偏移**（模型可观测：会重复看到同一段）
    /// ⇒ 按 `crates/ys-tool/src/lib.rs` 的准入纪律判**不安全**。
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn timeout(&self) -> Option<Duration> {
        Some(POLL_TIMEOUT)
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let handle_text = input["handle"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'handle'".into()))?;
        let session = self.session.get();

        let Some(handle) = parse_handle(handle_text) else {
            return Ok(self.unknown_handle(handle_text, &session));
        };

        let requested = input["wait_seconds"].as_u64().unwrap_or(0);
        let wait = clamp_wait_seconds(requested);
        if wait > 0 {
            self.wait_for_end(handle, &session, Duration::from_secs(wait)).await;
        }

        let Some(snapshot) = self.backgrounds.snapshot(handle, &session) else {
            return Ok(self.unknown_handle(handle_text, &session));
        };

        // **终态**（进程已结束且已收尸）⇒ 输出文件不会再增长 ⇒ 末行（无换行）**就是完整行**
        // ⇒ 整段交付；**运行中** ⇒ 末段留在 `consumed` 之后（不切半行）。
        let at_end = snapshot.state != BgState::Running && snapshot.process_gone;
        let end_limit = self.backgrounds.limits().max_output_bytes;
        let incremental = read_incremental(
            &snapshot.output_path,
            snapshot.consumed,
            at_end,
            end_limit,
        )
        .await
        .map_err(|error| {
            ToolError::Execution(format!("[bash_poll] failed to read output: {error}"))
        })?;
        let text = incremental.text;

        // 交付偏移推进到本次**已交付**的末尾（只进不退）⇒ 已送达的字节不重复送达。
        self.backgrounds
            .advance_consumed(handle, &session, snapshot.consumed + incremental.delivered);

        let cap = self.backgrounds.limits().max_output_bytes;
        let label = snapshot.handle.label();
        // **未交付的字节必须明示** —— 头尾裁剪只覆盖「读了但没展示」的那部分；
        // 「还没读」（末行未换行 / 超出单次读上限）同样要报出数量。
        let pending = pending_note(
            &label,
            &snapshot.output_path,
            incremental.pending,
            incremental.pending_kind,
        );
        // 输出拼装只有**一处真源**（`bg_output::assemble`）—— 本模块不得另写一份。
        let assembled = assemble(&text, &label, &snapshot.output_path, pending.as_deref());
        let new_bytes = assembled.head.len() + assembled.tail.len();
        let backstop = self.backgrounds.kernel_write_backstop_bytes();
        let (state, is_error) = state_line(&snapshot, cap, backstop, new_bytes);

        let mut lines: Vec<String> = vec![state, assembled.content];
        if requested > MAX_WAIT_SECONDS {
            lines.push(format!(
                "[bash_poll] wait_seconds {requested} clamped to {MAX_WAIT_SECONDS}"
            ));
        }

        Ok(ToolResult {
            content: lines.join("\n"),
            is_error,
        })
    }
}

/// 解析模型给的句柄串（`bg7`）—— 形态不对也算「未知句柄」。
fn parse_handle(text: &str) -> Option<BgHandle> {
    let digits = text.strip_prefix("bg")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().map(BgHandle)
}

/// `wait_seconds` 的生效值 —— **上限 20s**。
fn clamp_wait_seconds(requested: u64) -> u64 {
    requested.min(MAX_WAIT_SECONDS)
}

/// 状态行 + 是否算错误 —— **措辞纪律**的落点。
///
/// 结局与措辞一一对应，互不串味：
/// - 正常结束 ⇒ `finished — exit code 0`；
/// - 命令自身非零退码 ⇒ `finished — exit code N`（退码原样带出）；
/// - 被 harness 杀掉 ⇒ `failed — killed by the harness: …`；
/// - 被内核文件大小限额杀掉 ⇒ `failed — killed by the kernel file-size limit (SIGXFSZ): …`；
/// - 被别的信号杀掉 ⇒ `failed — killed by signal N …`；
/// - 输出超上限 ⇒ 一律**归因于 harness 的输出上限**（不是内核限额）——
///   内核限额是**粗闸门**（进程级、被子孙继承），正常工作的命令碰不到它。
fn state_line(
    snapshot: &BgSnapshot,
    cap: u64,
    backstop: u64,
    new_bytes: usize,
) -> (String, bool) {
    let label = snapshot.handle.label();
    let produced = format_bytes(snapshot.output_bytes);
    let elapsed = format!("{:.1}s", snapshot.elapsed.as_secs_f64());
    let new = format_bytes(new_bytes as u64);
    let counts = format!("{produced} produced, {new} new");
    let cap_bytes = format_bytes(cap);
    let backstop_bytes = format_bytes(backstop);
    let over_cap = snapshot.output_bytes >= cap;
    let signal_text = snapshot
        .signal
        .map(|signal| signal.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    match snapshot.state {
        BgState::Running => (
            format!("[{label}] running — {elapsed} elapsed — {counts}"),
            false,
        ),
        BgState::Exited(Some(0)) if !over_cap => (
            format!("[{label}] finished — exit code 0 — {elapsed} — {counts}"),
            false,
        ),
        BgState::Exited(Some(code)) if !over_cap => (
            format!("[{label}] finished — exit code {code} — {elapsed} — {counts}"),
            true,
        ),
        BgState::Exited(None) if !over_cap && snapshot.signal == Some(KERNEL_FSIZE_SIGNAL) => (
            format!(
                "[{label}] failed — killed by the kernel file-size limit (SIGXFSZ): \
                 the command wrote more than {backstop_bytes} to a file — {elapsed} — {counts}"
            ),
            true,
        ),
        BgState::Exited(None) if !over_cap => (
            format!(
                "[{label}] failed — killed by signal {signal_text} \
                 (not a non-zero exit code) — {elapsed} — {counts}"
            ),
            true,
        ),
        BgState::Killed => {
            let reason = if over_cap {
                format!("output exceeded the {cap_bytes} output cap")
            } else if snapshot.elapsed >= snapshot.timeout {
                format!("lifetime exceeded ({}s)", snapshot.timeout.as_secs())
            } else {
                "requested".to_string()
            };
            let detail = format!("{reason} — {elapsed} — {counts}");
            (format!("[{label}] failed — killed by the harness: {detail}"), true)
        }
        // 超上限且**不是**被 harness 杀的 ⇒ 命令在 harness 拦下它之前就自己结束了。
        // 归因仍是 harness 的输出上限（内核限额是粗闸门，不会在这里触发）。
        BgState::Exited(Some(code)) => (
            format!(
                "[{label}] failed — output exceeded the {cap_bytes} output cap \
                 (the command had already exited with exit code {code} before the \
                 harness could stop it) — {elapsed} — {counts}"
            ),
            true,
        ),
        BgState::Exited(None) => (
            format!(
                "[{label}] failed — output exceeded the {cap_bytes} output cap \
                 (the command was killed by signal {signal_text} before the harness \
                 could stop it) — {elapsed} — {counts}"
            ),
            true,
        ),
    }
}

/// 待交付字节的**形态** —— 决定**报出的措辞**（不静默丢弃）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PendingKind {
    /// 没有待交付的字节。
    #[default]
    None,
    /// 待交付的是**半行**（末行未换行）⇒ 命令结束后**整行**交付。
    PartialLine,
    /// 待交付的是**完整行**（还有行没交付）⇒ 下一次 poll 继续交付。
    WholeLine,
    /// 终态且文件末尾的字节**不完整**（多字节字符被截在文件里）⇒ 永远交付不了。
    IncompleteTail,
}

/// 一次增量读的结果 —— **已交付**与**待交付**分开记账。
#[derive(Debug, Default)]
struct Incremental {
    /// 本次交付的文本（只含**完整行**；终态时含末段）。
    text: String,
    /// 本次交付的字节数 —— `consumed` 的推进量。
    delivered: u64,
    /// **尚未交付**的字节数（末段未换行 / 超出单次读上限的那部分）。
    pending: u64,
    /// 待交付那一段的形态 —— 决定**报出的措辞**。
    pending_kind: PendingKind,
}

/// 按 `consumed` 偏移增量读 —— **只交付完整行**；末段（末行未换行）的处置分两种情形：
///
/// - **运行中**（`at_end == false`）⇒ 末段**不交付**，留在 `consumed` 之后等下一次 poll
///   ⇒ 不切半行、不切多字节字符；
/// - **终态**（`at_end == true`，进程已收尸 ⇒ 输出文件不会再增长）⇒ 末段**就是最后一行**
///   ⇒ 整段交付、`consumed` 推到文件尾 —— 否则进程退出后再无新字节 ⇒ 末段**永久丢失**。
///
/// `end_limit` 是终态时的**单次读上限**（= 输出上限）—— 末段必须**整段**读出来，
/// `consumed` 才能推到文件尾；**展示**预算（2000 行 / 50 KiB）仍由 `head_tail` 兜住，
/// 不放宽。
async fn read_incremental(
    path: &Path,
    consumed: u64,
    at_end: bool,
    end_limit: u64,
) -> std::io::Result<Incremental> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(SeekFrom::Start(consumed)).await?;
    let size = file.metadata().await?.len();
    let remaining = size.saturating_sub(consumed);
    if remaining == 0 {
        return Ok(Incremental::default());
    }

    // 运行中只读到 `MAX_BYTES`；终态读到 `end_limit`（末段要整段读出来）。
    let limit = if at_end {
        end_limit
            .min(END_READ_HARD_LIMIT)
            .max(ys_tool::MAX_BYTES as u64)
    } else {
        ys_tool::MAX_BYTES as u64
    };
    let want = remaining.min(limit) as usize;
    // 终态时多读 1 字节当 **EOF 探针**：读满 `want + 1` ⇒ 文件还没到头。
    // 运行中**不多读** ⇒ 单次读严格 ≤ `MAX_BYTES`。
    let read_cap = if at_end { want + 1 } else { want };
    let mut buffer = vec![0u8; read_cap];
    let mut filled = 0usize;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    let reached_eof = at_end && filled <= want;
    let region = &buffer[..filled.min(want)];

    // 切点：默认只到**最后一个完整行**；终态且已到 EOF ⇒ 末段整段交付。
    let mut cut = region
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    if at_end && (reached_eof || cut == 0) {
        // 末段是最后一行（或一行长过单次读上限）⇒ 整段交付，
        // 但**不得切在多字节字符中间**。
        cut = floor_char_boundary(region);
    }
    let text = String::from_utf8_lossy(&region[..cut]).into_owned();
    let delivered = cut as u64;

    // 待交付字节数以**读完之后**的文件大小为准 —— 读的过程中文件可能还在长。
    let current = file.metadata().await?.len();
    let pending = current.saturating_sub(consumed + delivered);
    let known = (region.len() - cut) as u64;
    let pending_kind = if pending == 0 {
        PendingKind::None
    } else if at_end && reached_eof {
        // 终态且已读到文件尾 ⇒ 剩下的只有「文件末尾被截断的多字节字符」⇒ 永远交付不了。
        PendingKind::IncompleteTail
    } else if region[cut..].contains(&b'\n') || pending > known {
        // 待交付的那段里**含完整行**（或还有我们没读到的字节）。
        PendingKind::WholeLine
    } else {
        // 只有**半行** ⇒ 命令结束后整行交付。
        PendingKind::PartialLine
    };
    Ok(Incremental {
        text,
        delivered,
        pending,
        pending_kind,
    })
}

/// 把切点回退到**字符边界** —— 不得把多字节字符切在中间。
fn floor_char_boundary(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        // `valid_up_to` = 最后一个不完整字符的起点 ⇒ 那里就是边界。
        Err(error) => error.valid_up_to(),
    }
}

/// 待交付字节的**报出** —— 未送达的字节不静默丢弃。
///
/// 措辞按**形态**分：半行等命令结束后整行交付；整行下次 poll 继续；
/// 文件末尾的不完整字节永远交付不了（只能去完整输出文件里看）。
fn pending_note(
    label: &str,
    output_path: &Path,
    pending: u64,
    kind: PendingKind,
) -> Option<String> {
    let reason = match kind {
        PendingKind::None => return None,
        PendingKind::PartialLine => "末行未换行，命令结束后整行交付",
        PendingKind::WholeLine => "还有完整行未交付，下次 poll 继续",
        PendingKind::IncompleteTail => "文件末尾的字节不完整，完整输出见路径",
    };
    Some(format!(
        "[{label}] 还有 {} 待交付（{reason}）—— 完整输出见 {}",
        format_bytes(pending),
        output_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::sync::Arc;

    use serde_json::json;

    use crate::background::BgLimits;
    use crate::test_support::{ctx, test_dir};

    /// 造一套工具（注册表 + 两个工具）—— 测试用的最小装配。
    ///
    /// 返回 `(注册表, bash_bg, bash_poll, 落盘根目录)`。
    fn tools(tag: &str) -> (Arc<BackgroundTasks>, BashBgTool, BashPollTool, PathBuf) {
        let root = test_dir(tag);
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), BgLimits::default()));
        let workspace = std::env::temp_dir();
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), workspace, session.clone());
        let poll = BashPollTool::new(tasks.clone(), session);
        (tasks, bg, poll, root)
    }

    /// 收尾：杀掉该会话的全部后台进程组并收尸，删掉落盘目录 —— 不留孤儿进程。
    fn cleanup(tasks: &BackgroundTasks, root: &Path) {
        tasks.kill_all("session-a");
        tasks.sweep();
        let _ = fs::remove_dir_all(root);
    }

    /// 产出行数可数的输出（`awk` 是 POSIX 工具 ⇒ 不依赖开发机装了 GNU coreutils）。
    fn many_lines(count: u32) -> String {
        format!("awk 'BEGIN{{for(i=1;i<={count};i++) print \"line \" i}}'")
    }

    /// 产出一条**慢速**写 stdout 的命令 —— 速率约 `chunk_bytes` 每秒。
    ///
    /// 必须**慢**：输出闸门是**轮询式**的 ⇒ 写得快的命令可能在两次检查之间就
    /// 写完并退出 ⇒ 闸门来不及杀它。慢写才能确定「命令还在跑时被闸门杀掉」。
    fn slow_writer(chunk_bytes: u32, chunks: u32) -> String {
        format!(
            "i=0; while [ \"$i\" -lt {chunks} ]; do \
            dd if=/dev/zero bs={chunk_bytes} count=1 2>/dev/null | tr '\\000' 'x'; \
            sleep 1; i=$((i+1)); done"
        )
    }

    /// 造一份状态快照 —— 用于**直接判别**两种归因措辞。
    fn snapshot_of(state: BgState, signal: Option<i32>, output_bytes: u64) -> BgSnapshot {
        BgSnapshot {
            handle: BgHandle(1),
            pid: 4242,
            command: "true".to_string(),
            cwd: PathBuf::from("/tmp"),
            state,
            signal,
            elapsed: Duration::from_secs(1),
            timeout: Duration::from_secs(900),
            output_path: PathBuf::from("/tmp/bg1.log"),
            output_bytes,
            consumed: 0,
            // 造快照的用例都指向**已收尸**的终态条目。
            process_gone: true,
        }
    }

    // ---------- spec 逐字 ----------

    /// 两个工具的 `spec` **逐字**钉住 —— spec 是模型可见契约，改字即改契约。
    #[test]
    fn the_specs_match_the_model_visible_contract() {
        let (_tasks, bg, poll, _root) = tools("bg_spec");

        let bg_spec = bg.spec();
        assert_eq!(bg_spec.name, "bash_bg");
        assert_eq!(
            bg_spec.description,
            "Start a shell command in the background and get a handle back immediately; the \
             command keeps running while you do other work. Collect it later with bash_poll. \
             Use this for commands that take more than a few seconds (test suites, builds); \
             use bash for quick commands."
        );
        assert_eq!(
            bg_spec.parameters,
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Max lifetime in seconds (default 900, max 3600)" }
                },
                "required": ["command"]
            })
        );

        let poll_spec = poll.spec();
        assert_eq!(poll_spec.name, "bash_poll");
        assert_eq!(
            poll_spec.description,
            "Check a background command started with bash_bg: returns its state plus the \
             output produced since the last poll. The full output stays on disk — grep or \
             read that path for any part you missed instead of running the command again."
        );
        assert_eq!(
            poll_spec.parameters,
            json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "Handle returned by bash_bg" },
                    "wait_seconds": { "type": "number", "description": "Wait up to N seconds for the command to finish (default 0, max 20)" }
                },
                "required": ["handle"]
            })
        );
    }

    /// `timeout()` 与并发标志 —— 两个工具都**不准开并行**。
    #[test]
    fn timeouts_and_concurrency_flags_match_the_contract() {
        let (_tasks, bg, poll, _root) = tools("bg_flags");

        assert_eq!(bg.timeout(), Some(Duration::from_secs(30)));
        assert_eq!(poll.timeout(), Some(Duration::from_secs(35)));

        assert!(
            !bg.is_concurrency_safe(),
            "启动会 spawn 进程 ⇒ 有副作用 ⇒ 永不准开并行"
        );
        assert!(
            !poll.is_concurrency_safe(),
            "推进交付偏移 ⇒ 对模型可观测 ⇒ 按准入纪律判不安全"
        );
    }

    // ---------- 错误路径 ----------

    /// 启动失败 ⇒ **同步报错**，且注册表**无新条目**。
    #[tokio::test]
    async fn a_failed_start_reports_an_error_and_leaves_no_entry() {
        let root = test_dir("bg_start_fail");
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), BgLimits::default()));
        let session = SessionSlot::new("session-a");
        // 工作目录不存在 ⇒ spawn 失败
        let bg = BashBgTool::new(tasks.clone(), root.join("no-such-dir"), session);

        let result = bg.call(json!({ "command": "true" }), ctx()).await;
        let error = result.expect_err("工作目录不存在 ⇒ 启动必须同步报错");
        assert!(
            error.to_string().contains("failed to start command"),
            "got {error}"
        );
        assert_eq!(tasks.entry_count(), 0, "启动失败不得留下条目");

        let _ = fs::remove_dir_all(&root);
    }

    /// 未知句柄 ⇒ 明确的「未知句柄」错误，并列出仍在跑的句柄。
    #[tokio::test]
    async fn an_unknown_handle_is_reported_as_such() {
        let (tasks, bg, poll, root) = tools("bg_unknown");
        bg.call(json!({ "command": "sleep 5" }), ctx()).await.unwrap();

        let result = poll.call(json!({ "handle": "bg9" }), ctx()).await.unwrap();
        assert!(result.is_error, "未知句柄必须是错误结果");
        assert!(
            result.content.contains("unknown handle"),
            "got {}",
            result.content
        );
        assert!(
            result.content.contains("bg1"),
            "应列出仍在跑的句柄：{}",
            result.content
        );

        cleanup(&tasks, &root);
    }

    /// 别的会话的句柄 ⇒ 命中不到条目（对模型同样表现为「未知句柄」）。
    #[tokio::test]
    async fn a_handle_from_another_session_is_unknown() {
        let (tasks, bg, _poll, root) = tools("bg_session");
        bg.call(json!({ "command": "echo hi" }), ctx()).await.unwrap();

        let other = BashPollTool::new(tasks.clone(), SessionSlot::new("session-b"));
        let result = other.call(json!({ "handle": "bg1" }), ctx()).await.unwrap();
        assert!(result.is_error, "跨会话的句柄不得命中");
        assert!(
            result.content.contains("unknown handle"),
            "got {}",
            result.content
        );

        cleanup(&tasks, &root);
    }

    // ---------- 措辞纪律 ----------

    /// 非零退码 ⇒ `is_error == true` 且**退码原样带出**。
    #[tokio::test]
    async fn a_non_zero_exit_code_is_carried_verbatim() {
        let (tasks, bg, poll, root) = tools("bg_exit");
        bg.call(json!({ "command": "echo out; exit 3" }), ctx()).await.unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(result.is_error, "非零退码 ⇒ is_error");
        assert!(
            result.content.contains("exit code 3"),
            "退码必须原样带出：{}",
            result.content
        );

        cleanup(&tasks, &root);
    }

    /// 被 harness 杀掉 ⇒ 措辞与**非零退码**不同（模型不得把被杀误报成「测试挂了」）。
    #[tokio::test]
    async fn a_harness_kill_uses_a_different_wording_than_a_non_zero_exit() {
        let (tasks, bg, poll, root) = tools("bg_killed");
        bg.call(json!({ "command": "sleep 30" }), ctx()).await.unwrap();
        assert!(tasks.kill(BgHandle(1), "session-a"), "句柄应存在");

        let result = poll.call(json!({ "handle": "bg1" }), ctx()).await.unwrap();
        assert!(result.is_error, "被杀掉 ⇒ is_error");
        assert!(
            result.content.contains("killed by the harness"),
            "got {}",
            result.content
        );
        assert!(
            !result.content.contains("exit code"),
            "被 harness 杀掉不得写成非零退码：{}",
            result.content
        );

        cleanup(&tasks, &root);
    }

    // ---------- 增量语义 ----------

    /// 连续两次 poll ⇒ 第二次**不重复**交付已送达字节。
    #[tokio::test]
    async fn a_second_poll_does_not_repeat_delivered_bytes() {
        let (tasks, bg, poll, root) = tools("bg_incremental");
        bg.call(json!({ "command": "echo first-line" }), ctx()).await.unwrap();

        let first = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(
            first.content.contains("first-line"),
            "首次 poll 必须交付输出：{}",
            first.content
        );

        let second = poll.call(json!({ "handle": "bg1" }), ctx()).await.unwrap();
        assert!(
            !second.content.contains("first-line"),
            "已送达的字节不得重复送达：{}",
            second.content
        );
        assert!(
            second.content.contains("0 B new"),
            "第二次没有新字节：{}",
            second.content
        );

        cleanup(&tasks, &root);
    }

    /// 被裁剪 ⇒ **报出 `skipped_bytes`** 与完整输出路径（不静默丢弃）。
    #[tokio::test]
    async fn a_trimmed_poll_reports_the_skipped_bytes_and_the_full_path() {
        let (tasks, bg, poll, root) = tools("bg_trimmed");
        bg.call(json!({ "command": many_lines(3000) }), ctx()).await.unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(
            result.content.contains("未送达"),
            "被裁剪的字节必须明示：{}",
            result.content
        );
        assert!(
            result.content.contains("头尾裁剪"),
            "got {}",
            result.content
        );
        assert!(
            result.content.contains("KiB 未送达"),
            "跳过量必须是**非零**的实数（恒置 0 就是不报）：{}",
            result.content
        );
        let path = root.join("session-a").join("bg1.log");
        assert!(
            result.content.contains(path.to_str().unwrap()),
            "必须给出完整输出路径：{}",
            result.content
        );

        cleanup(&tasks, &root);
    }

    // ---------- 字节边界 ----------

    /// **运行中**的半行 ⇒ **不交付**（不切半行），且结果里**明示**还有多少字节待交付。
    #[tokio::test]
    async fn a_running_half_line_is_kept_and_reported() {
        let (tasks, bg, poll, root) = tools("bg_boundary");
        // 命令**不结束** ⇒ 末段永远是半行 ⇒ 必须留在 `consumed` 之后。
        bg.call(json!({ "command": "printf 'alpha\\nbeta'; sleep 30" }), ctx())
            .await
            .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 1 }), ctx())
            .await
            .unwrap();
        assert!(
            result.content.contains("alpha"),
            "完整行必须交付：{}",
            result.content
        );
        assert!(
            !result.content.contains("beta"),
            "运行中的半行不得交付：{}",
            result.content
        );
        assert!(
            result.content.contains("4 B 待交付"),
            "未交付的字节必须明示（还有多少）：{}",
            result.content
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert_eq!(
            snapshot.consumed, 6,
            "consumed 必须停在完整行边界（alpha\\n = 6 字节）"
        );

        cleanup(&tasks, &root);
    }

    /// **终态**（进程已结束）⇒ 末行没有换行**也是完整行** ⇒ 必须交付，且 `consumed`
    /// 推到文件尾 —— 否则进程退出后再无新字节 ⇒ 末行**永久丢失**。
    #[tokio::test]
    async fn the_last_line_without_a_newline_is_delivered_when_the_command_has_ended() {
        let (tasks, bg, poll, root) = tools("bg_final_line");
        bg.call(json!({ "command": "printf 'alpha\\nbeta'" }), ctx())
            .await
            .unwrap();

        let first = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(
            first.content.contains("alpha"),
            "完整行必须交付：{}",
            first.content
        );
        assert!(
            first.content.contains("beta"),
            "终态的末行（无换行）必须交付：{}",
            first.content
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert_eq!(
            snapshot.consumed, 10,
            "consumed 必须推到文件尾（alpha\\nbeta = 10 字节）"
        );
        assert_eq!(snapshot.output_bytes, 10, "输出共 10 字节");

        // 第二次 poll：**不重复**交付已送达的字节。
        let second = poll.call(json!({ "handle": "bg1" }), ctx()).await.unwrap();
        assert!(
            !second.content.contains("beta"),
            "已送达的字节不得重复送达：{}",
            second.content
        );
        assert!(
            second.content.contains("0 B new"),
            "第二次没有新字节：{}",
            second.content
        );

        cleanup(&tasks, &root);
    }

    /// **长单行**（没有换行，但**放得进**展示预算）⇒ 终态时必须**整段出现在结果里** ——
    /// 否则模型只看到「产出了字节、一个字都没有」。
    #[tokio::test]
    async fn a_long_single_line_is_delivered_when_the_command_has_ended() {
        let (tasks, bg, poll, root) = tools("bg_long_line");
        // 10 KiB 单行（无换行）—— 在 50 KiB 的展示预算之内。
        bg.call(
            json!({ "command": "head -c 10240 /dev/zero | tr '\\000' 'x'" }),
            ctx(),
        )
        .await
        .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(
            result.content.contains('x'),
            "长单行必须出现在结果里：{}",
            crate::test_support::brief(&result.content)
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert_eq!(
            snapshot.consumed, snapshot.output_bytes,
            "consumed 必须推到文件尾"
        );
        assert_eq!(snapshot.output_bytes, 10240, "输出共 10 KiB");

        cleanup(&tasks, &root);
    }

    /// **单行超长**（超过展示预算）⇒ 交付**不丢**（`consumed` 推到文件尾），
    /// 且**不静默**：裁掉的字节必须报出数量 + 完整输出路径。
    #[tokio::test]
    async fn an_oversized_single_line_is_consumed_and_reported() {
        let (tasks, bg, poll, root) = tools("bg_huge_line");
        // 60 KiB 单行（无换行）—— 超过 50 KiB 的展示预算。
        bg.call(
            json!({ "command": "head -c 61440 /dev/zero | tr '\\000' 'x'" }),
            ctx(),
        )
        .await
        .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert_eq!(
            snapshot.consumed, snapshot.output_bytes,
            "consumed 必须推到文件尾（末行不得被跳过）"
        );
        assert_eq!(snapshot.output_bytes, 61440, "输出共 60 KiB");
        assert!(
            result.content.contains("未送达"),
            "展示不下也要报出未送达的字节：{}",
            crate::test_support::brief(&result.content)
        );
        // **正文不得为空**：单行超长时给出该行的**一个前缀** ——
        // 否则模型只能靠「未送达 N 字节 + 完整路径」去 `read` 那个文件。
        assert!(
            result.content.contains(&"x".repeat(1_000)),
            "单行超长也必须给出前缀（正文不得为空）：{}",
            crate::test_support::brief(&result.content)
        );
        let path = root.join("session-a").join("bg1.log");
        assert!(
            result.content.contains(path.to_str().unwrap()),
            "必须给出完整输出路径：{}",
            crate::test_support::brief(&result.content)
        );

        cleanup(&tasks, &root);
    }

    /// **运行中**的多字节半行 ⇒ 不得交付、也不得切在字符中间（模型看不到乱码）。
    #[tokio::test]
    async fn a_running_multibyte_half_line_is_not_cut_in_the_middle() {
        let (tasks, bg, poll, root) = tools("bg_utf8_half");
        bg.call(json!({ "command": "printf '中文字'; sleep 30" }), ctx())
            .await
            .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 1 }), ctx())
            .await
            .unwrap();
        assert!(
            !result.content.contains("中文字"),
            "运行中的半行不得交付：{}",
            result.content
        );
        assert!(
            !result.content.contains('\u{FFFD}'),
            "不得出现替换字符（说明切在字符中间）：{}",
            result.content
        );
        assert!(
            result.content.contains("9 B 待交付"),
            "未交付的字节必须明示：{}",
            result.content
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert_eq!(snapshot.consumed, 0, "半行不得推进 consumed");

        cleanup(&tasks, &root);
    }

    /// 多字节字符不得被切在中间（切一半 ⇒ 模型看到乱码）。
    #[tokio::test]
    async fn a_multibyte_line_is_never_cut_in_the_middle() {
        let (tasks, bg, poll, root) = tools("bg_utf8");
        bg.call(json!({ "command": "printf '中文字\\n'" }), ctx())
            .await
            .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(
            result.content.contains("中文字"),
            "多字节字符必须完整交付：{}",
            result.content
        );
        assert!(
            !result.content.contains('\u{FFFD}'),
            "不得出现替换字符（说明切在字符中间）：{}",
            result.content
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert_eq!(snapshot.consumed, 10, "3 个中文字（9 字节）+ 换行 = 10 字节");

        cleanup(&tasks, &root);
    }

    // ---------- 等待与上限 ----------

    /// `wait_seconds` 的**上限是 20**（不是 60）。
    #[test]
    fn the_wait_seconds_is_capped_at_twenty() {
        assert_eq!(clamp_wait_seconds(0), 0);
        assert_eq!(clamp_wait_seconds(5), 5);
        assert_eq!(clamp_wait_seconds(20), 20);
        assert_eq!(clamp_wait_seconds(60), 20, "上限是 20s，不是 60s");
        assert_eq!(MAX_WAIT_SECONDS, 20);
    }

    /// 等待是**有界**的：命令没结束也到点即返回（不挂死）。
    #[tokio::test]
    async fn a_bounded_wait_returns_while_the_command_is_still_running() {
        let (tasks, bg, poll, root) = tools("bg_wait");
        bg.call(json!({ "command": "sleep 30" }), ctx()).await.unwrap();

        let started = Instant::now();
        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 1 }), ctx())
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            result.content.contains("running"),
            "命令没结束 ⇒ 状态是 running：{}",
            result.content
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "等待必须有明确上界，实际等了 {elapsed:?}"
        );

        cleanup(&tasks, &root);
    }

    /// 输出上限闸门（**精确闸门**）：写爆输出的命令被**杀掉**，输出不会无限膨胀。
    #[tokio::test]
    async fn the_output_cap_stops_a_runaway_command() {
        let root = test_dir("bg_cap");
        let limits = BgLimits {
            max_output_bytes: 4096,
            ..BgLimits::default()
        };
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), limits));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), std::env::temp_dir(), session.clone());
        let poll = BashPollTool::new(tasks.clone(), session);

        bg.call(json!({ "command": slow_writer(64 * 1024, 64) }), ctx())
            .await
            .unwrap();
        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();

        assert!(result.is_error, "超限 ⇒ 错误结果：{}", result.content);
        assert!(
            result.content.contains("failed — killed by the harness"),
            "超限必须报成被杀：{}",
            result.content
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        assert!(
            snapshot.output_bytes >= 4096 && snapshot.output_bytes <= 4096 + 256 * 1024,
            "输出闸门必须兜住，实际写了 {} 字节",
            snapshot.output_bytes
        );

        tasks.kill_all("session-a");
        tasks.sweep();
        let _ = fs::remove_dir_all(&root);
    }

    // ---------- 内核限额与输出上限的分工 ----------

    /// **A**：正常写大文件的命令**不被内核的文件大小限额杀掉** ——
    /// 后台命令的主要用途就是这类活（构建、打包、落盘）。
    ///
    /// 内核限额是**进程级**的 ⇒ 它一旦被设成输出上限，正常写 4 MiB 文件的命令
    /// 就会吃 `SIGXFSZ` 被杀掉 ⇒ 功能被打断。故内核限额必须**高于**输出上限。
    #[tokio::test]
    async fn a_command_writing_a_large_file_is_not_killed_by_the_kernel_limit() {
        let root = test_dir("bg_big_file");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let tasks = Arc::new(BackgroundTasks::new(root.join("bg"), BgLimits::default()));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), workspace.clone(), session.clone());
        let poll = BashPollTool::new(tasks.clone(), session);

        bg.call(
            json!({ "command": "dd if=/dev/zero of=big.bin bs=1048576 count=4 2>/dev/null" }),
            ctx(),
        )
        .await
        .unwrap();
        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 10 }), ctx())
            .await
            .unwrap();

        assert!(
            !result.is_error,
            "写大文件的命令必须正常跑完（内核限额不得打断它）：{}",
            result.content
        );
        let big = workspace.join("big.bin");
        let size = fs::metadata(&big).map(|meta| meta.len()).unwrap_or(0);
        assert_eq!(
            size,
            4 * 1024 * 1024,
            "文件必须完整写完（内核限额不得打断正常写盘）"
        );

        tasks.kill_all("session-a");
        tasks.sweep();
        let _ = fs::remove_dir_all(&root);
    }

    /// **B**：输出上限**仍然生效** —— 往 stdout 灌超限输出的命令被**杀掉**
    /// （不是静默截断），poll 结果**明示**超限 + 完整输出路径。
    #[tokio::test]
    async fn the_output_cap_still_kills_a_command_that_floods_stdout() {
        let (tasks, bg, poll, root) = tools("bg_cap_flood");
        bg.call(json!({ "command": slow_writer(1024 * 1024, 64) }), ctx())
            .await
            .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 15 }), ctx())
            .await
            .unwrap();

        assert!(result.is_error, "超限 ⇒ 错误结果：{}", result.content);
        assert!(
            result.content.contains("killed by the harness"),
            "超限的命令必须被 harness 杀掉：{}",
            result.content
        );
        assert!(
            result.content.contains("output exceeded"),
            "必须明示输出超限：{}",
            result.content
        );
        let snapshot = tasks.snapshot(BgHandle(1), "session-a").unwrap();
        let cap = tasks.limits().max_output_bytes;
        assert!(
            snapshot.output_bytes >= cap,
            "输出必须真的到上限：{}",
            snapshot.output_bytes
        );
        assert!(
            snapshot.output_bytes < cap + 1024 * 1024,
            "输出不得无限膨胀：{}",
            snapshot.output_bytes
        );
        let path = root.join("session-a").join("bg1.log");
        assert!(
            result.content.contains(path.to_str().unwrap()),
            "必须给出完整输出路径：{}",
            result.content
        );

        cleanup(&tasks, &root);
    }

    /// **C**：归因正确 —— 超限的归因是**harness 的输出上限**，
    /// 不是「内核文件大小限额」（内核限额是粗闸门，正常工作碰不到它）。
    #[tokio::test]
    async fn the_over_cap_attribution_blames_the_harness_not_the_kernel_limit() {
        let (tasks, bg, poll, root) = tools("bg_cap_attribution");
        bg.call(json!({ "command": slow_writer(1024 * 1024, 64) }), ctx())
            .await
            .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 15 }), ctx())
            .await
            .unwrap();

        assert!(
            result.content.contains("output exceeded the 2.0 MiB output cap"),
            "必须归因到 harness 的输出上限：{}",
            result.content
        );
        assert!(
            !result.content.contains("kernel file-size limit"),
            "sweeper 杀的命令不得说成内核限额杀的：{}",
            result.content
        );
        assert!(
            !result.content.contains("SIGXFSZ"),
            "sweeper 杀的命令不得说成内核限额杀的：{}",
            result.content
        );

        // 两种归因必须**互不相同** —— 直接对同一段措辞逻辑喂两种快照。
        let cap = tasks.limits().max_output_bytes;
        let backstop = tasks.kernel_write_backstop_bytes();
        let harness_killed = state_line(
            &snapshot_of(BgState::Killed, None, cap),
            cap,
            backstop,
            0,
        )
        .0;
        let kernel_killed = state_line(
            &snapshot_of(BgState::Exited(None), Some(KERNEL_FSIZE_SIGNAL), 0),
            cap,
            backstop,
            0,
        )
        .0;
        assert!(
            harness_killed.contains("killed by the harness")
                && harness_killed.contains("output exceeded")
                && !harness_killed.contains("kernel file-size limit"),
            "被 harness 杀的措辞：{harness_killed}"
        );
        assert!(
            kernel_killed.contains("kernel file-size limit (SIGXFSZ)")
                && !kernel_killed.contains("output exceeded"),
            "被内核限额杀的措辞：{kernel_killed}"
        );

        cleanup(&tasks, &root);
    }

    // ---------- 额度与文案 ----------

    /// 超 `max_running` 的报错**只指向模型真能做的动作**。
    ///
    /// 模型面**没有 kill 入口**（`bash_bg` 只能 start、`bash_poll` 只能 poll），
    /// 而 poll **不会**让 `Running` 条目离开 `Running` ⇒ 文案里出现「kill」就是
    /// 指一个**做不到**的动作 ⇒ 模型会被自己的报错锁死。
    #[tokio::test]
    async fn the_too_many_running_message_points_only_at_actions_the_model_can_take() {
        let root = test_dir("bg_max_running");
        let limits = BgLimits {
            max_running: 1,
            ..BgLimits::default()
        };
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), limits));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), std::env::temp_dir(), session.clone());

        bg.call(json!({ "command": "sleep 30" }), ctx()).await.unwrap();
        let error = bg
            .call(json!({ "command": "true" }), ctx())
            .await
            .expect_err("超过 max_running 必须同步报错");
        let message = error.to_string();
        assert!(
            message.contains("too many background commands are running (1)"),
            "got {message}"
        );
        assert!(
            message.contains("wait for one to finish, then start another"),
            "文案必须指向模型真能做的动作（等一条跑完）：got {message}"
        );
        assert!(
            !message.contains("kill"),
            "模型面没有 kill 入口 ⇒ 文案不得指向 kill：got {message}"
        );
        assert!(
            !message.contains("poll or kill"),
            "「poll or kill」是做不到的动作：got {message}"
        );

        cleanup(&tasks, &root);
    }

    /// 两条限额是**分开的**：`max_running` 数的是**在跑**的，已结束的条目**不占**名额；
    /// 已结束的条目只占**条目表**（`max_entries`）。
    #[tokio::test]
    async fn a_finished_entry_occupies_a_table_slot_but_not_a_running_slot() {
        let root = test_dir("bg_limits");
        let limits = BgLimits {
            max_running: 1,
            max_entries: 8,
            ..BgLimits::default()
        };
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), limits));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), std::env::temp_dir(), session.clone());
        let poll = BashPollTool::new(tasks.clone(), session);

        // 第一条跑完 ⇒ 终态 ⇒ 不占 `max_running` 名额（但仍占条目表）。
        bg.call(json!({ "command": "true" }), ctx()).await.unwrap();
        let finished = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        assert!(
            finished.content.contains("finished"),
            "第一条必须已结束：{}",
            finished.content
        );
        assert_eq!(tasks.entry_count(), 1, "终态条目仍占条目表");
        assert!(
            tasks.running_handles("session-a").is_empty(),
            "终态条目不在跑 ⇒ 不占 max_running 名额"
        );

        // 唯一的名额空着 ⇒ 还能再起一条（在跑的）。
        bg.call(json!({ "command": "sleep 30" }), ctx()).await.unwrap();
        assert_eq!(
            tasks.running_handles("session-a"),
            vec![BgHandle(2)],
            "在跑的条目占着唯一的名额"
        );

        // 名额用满 ⇒ 报错（已结束的那条**不**算在内）。
        let error = bg
            .call(json!({ "command": "true" }), ctx())
            .await
            .expect_err("在跑的条数到上限必须同步报错");
        assert!(
            error.to_string().contains("too many background commands are running (1)"),
            "got {error}"
        );

        cleanup(&tasks, &root);
    }

    // ---------- 组装的**单一真源** ----------

    /// poll 的正文必须与 `bg_output::assemble` 的产物**逐字节相同** ——
    /// 输出拼装只有一处实现（两份实现必然漂移）。
    #[tokio::test]
    async fn the_poll_body_is_byte_identical_to_the_shared_assembly() {
        let (tasks, bg, poll, root) = tools("bg_assembly");
        bg.call(json!({ "command": "printf 'alpha\\nbeta'" }), ctx())
            .await
            .unwrap();

        let result = poll
            .call(json!({ "handle": "bg1", "wait_seconds": 5 }), ctx())
            .await
            .unwrap();
        let path = root.join("session-a").join("bg1.log");
        let expected = crate::bg_output::assemble("alpha\nbeta", "bg1", &path, None);

        // 第一行是状态行，其余部分必须与共享拼装**逐字节**相同。
        let (_, body) = result.content.split_once('\n').expect("结果至少两行");
        assert_eq!(
            body, expected.content,
            "拼装必须只有一处真源：\n实际={body}\n期望={}",
            expected.content
        );

        cleanup(&tasks, &root);
    }

    /// **单一真源**的守卫：本模块（非测试部分）不得出现输出拼装的字样 ——
    /// 那些字样只允许出现在 `bg_output` 里。
    ///
    /// 钉住的变异：在本模块内联一份同样的拼装（哪怕逐字相同）⇒ 用例变红。
    #[test]
    fn the_output_assembly_lives_only_in_the_shared_module() {
        let source = include_str!("bg_tools.rs");
        let body = source
            .split_once("#[cfg(test)]")
            .expect("本模块必须有测试段")
            .0;
        for marker in ["lines omitted", "未送达（头尾裁剪）", "full output:"] {
            assert!(
                !body.contains(marker),
                "输出拼装只能有一处真源（`bg_output::assemble`）—— 本模块出现了 {marker:?}"
            );
        }
    }
}
