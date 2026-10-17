//! `--debug` 结构化执行日志（`yushan.debug/1` JSONL）。
//!
//! 设计：`docs/arch/agent-observability/design.md` §4；决策见
//! `docs/arch/agent-observability/adr-debug-log.md`。
//!
//! # 职责
//!
//! - 开关解析（`--debug` / `YUSHAN_DEBUG`，环境变量优先）；
//! - 单行一条记录的 JSONL 信封（`schema`/`ev`/`t_ms`/`dt_ms`/`seq`/`run`/相关 id）；
//! - **独立 OS 写线程 + 有界记录队列**：调用线程只序列化并 `try_send`，绝不碰
//!   文件 IO；控制命令（`Switch` / `BeginRun` / `Stop`）走独立无界通道；
//! - 工具参数摘要与脱敏（debug 是索引，不是内容副本）。
//!
//! # 硬约束
//!
//! 1. **时间戳先取**：`dt_ms` / `t_ms` / `seq` 在任何格式化之前取得；序列化与入队
//!    的耗时只进 `hot_path_ns`，不污染 `dt_ms`。
//! 2. **绝不阻塞生产者**：记录队列满即丢当前记录并 `dropped += 1`；控制命令
//!    不经过有界队列（无界通道，绝不丢失）；只有 `flush` 在 2 秒预算内等
//!    writer 收尾确认。
//! 3. **每行一次 `write_all`**（`\n` 预先拼好）：进程被杀最多丢整行，不留半行 JSON。
//! 4. **best-effort**：目录创建、打开、写入、flush 的任何 IO 错误都静默吞掉，
//!    不 panic、不传播，也**不改变任何执行路径**。
//! 5. 关闭 debug 时调用方持有 `None`，本模块不建线程、不建文件、不分配。

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use ys_core::Usage;

use crate::logging;

/// 开关环境变量：优先于命令行 `--debug`。
pub(crate) const DEBUG_ENV: &str = "YUSHAN_DEBUG";
/// 工具参数摘要档位环境变量（`none` / `head` / `full`，默认 `head`）。
pub(crate) const ARGS_ENV: &str = "YUSHAN_DEBUG_ARGS";
/// 记录 schema 版本。同一 major 内只许加字段。
pub(crate) const SCHEMA_VERSION: &str = "yushan.debug/1";
/// 有界队列默认容量。
pub(crate) const DEFAULT_QUEUE_CAPACITY: usize = 1024;
/// 单个 debug 文件默认上限（64 MiB）：触顶写一条 `log_truncated` 后停写。
pub(crate) const DEFAULT_FILE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// `flush` 的总预算：等 writer 排空记录、写 `run_end` 并收尾确认。
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// writer 空闲时轮询控制通道的间隔：控制命令不携带记录负载，最长延迟即该
/// 值；远小于 [`FLUSH_TIMEOUT`]。
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 连续 IO 错误达到该次数后停写（线程仍继续排空队列，保证 `flush` 能返回）。
const MAX_CONSECUTIVE_WRITE_ERRORS: u32 = 8;
/// `head` 档工具参数总预算。
const HEAD_BUDGET_BYTES: usize = 512;
/// `full` 档工具参数总预算。
const FULL_BUDGET_BYTES: usize = 8 * 1024;

/// 解析开关：`YUSHAN_DEBUG` 非空时优先；`1/true/yes/on` 为开，其余非空值为关；
/// 未设置或空字符串时跟随命令行 `cli_flag`。
pub(crate) fn debug_enabled(cli_flag: bool) -> bool {
    match std::env::var_os(DEBUG_ENV) {
        Some(value) if !value.is_empty() => {
            let value = value.to_string_lossy().trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        }
        _ => cli_flag,
    }
}

/// debug 文件路径：`log_dir()/debug-<session stem>.jsonl`。
///
/// 无 session 文件（会话尚未落盘）时用 `debug-<pid>-<unix_ms>.jsonl`；
/// 日志目录不可得时返回 `None`，调用方据此不创建 sink。
pub(crate) fn debug_path_for(session_path: Option<&Path>) -> Option<PathBuf> {
    let dir = logging::log_dir()?;
    let name = session_path
        .and_then(Path::file_stem)
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(|stem| format!("debug-{stem}.jsonl"))
        .unwrap_or_else(|| format!("debug-{}-{}.jsonl", std::process::id(), now_unix_ms()));
    Some(dir.join(name))
}

/// 接线入口：关闭 debug 时**在解析路径与起线程之前**早退。
///
/// `None` 同时表示「开关关闭」与「路径不可得」—— 两种情况调用方都只需持有
/// `None`，不解析路径、不建目录、不起线程。
pub(crate) fn open_enabled(cli_flag: bool, session_path: Option<&Path>) -> Option<DebugSink> {
    if !debug_enabled(cli_flag) {
        return None;
    }
    let path = debug_path_for(session_path)?;
    let mut sink = DebugSink::open(path.clone(), DEFAULT_QUEUE_CAPACITY)?;
    // 跨进程 `--resume` 会追加同一个 session 的 debug 文件：`run` 必须接着已有
    // 记录递增，否则文件里会出现两条 `run=0` 的 `run_start`，失去区分 run 的能力。
    let run = next_run(&path);
    if run > 0 {
        sink.begin_run(run, now_unix_ms());
    }
    Some(sink)
}

/// 已有 debug 文件里最大的 `run` + 1；文件不存在 / 无有效记录 ⇒ `0`。
fn next_run(path: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|record| record.get("run")?.as_u64())
        .max()
        .map_or(0, |run| run.saturating_add(1))
}

/// 工具参数摘要档位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgsMode {
    /// 只留 `args_keys` 与原始字节数。
    None,
    /// 字符串有界截断 + 总预算 512 B（默认）。
    Head,
    /// 完整值，总预算 8 KiB；脱敏仍然生效。
    Full,
}

impl ArgsMode {
    /// 从 `YUSHAN_DEBUG_ARGS` 解析；未设置 / 空 / 无法识别均为 `Head`。
    pub(crate) fn from_env() -> Self {
        match std::env::var_os(ARGS_ENV) {
            Some(value) if !value.is_empty() => {
                let value = value.to_string_lossy().trim().to_ascii_lowercase();
                match value.as_str() {
                    "none" => Self::None,
                    "full" => Self::Full,
                    _ => Self::Head,
                }
            }
            _ => Self::Head,
        }
    }

    fn budget(self) -> usize {
        match self {
            Self::None => 0,
            Self::Head => HEAD_BUDGET_BYTES,
            Self::Full => FULL_BUDGET_BYTES,
        }
    }
}

/// 一条待写记录的**内容部分**。信封字段（时间、序号、run、id）由
/// [`DebugSink::record`] 统一补齐；`round` / `tool_call_id` 供调用方构造
/// [`DebugIds`] 时复用。
#[derive(Debug)]
pub(crate) struct DebugRecord {
    pub ev: &'static str,
    pub round: Option<u32>,
    pub tool_call_id: Option<String>,
    pub fields: Map<String, Value>,
}

/// 记录信封里的相关 id。不适用时省略（不写 `null`）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct DebugIds<'a> {
    pub session_id: &'a str,
    pub turn: Option<u32>,
    pub round: Option<u32>,
    pub tool_call_id: Option<&'a str>,
}

/// 计数器。`seq` 由调用线程在取时间时分配，其余由 writer 线程或调用线程按
/// 各自语义累计。
#[derive(Default)]
pub(crate) struct DebugStats {
    pub(crate) seq: AtomicU64,
    pub(crate) records_written: AtomicU64,
    pub(crate) dropped: AtomicU64,
    pub(crate) write_errors: AtomicU64,
    pub(crate) serde_failures: AtomicU64,
    pub(crate) bytes_written: AtomicU64,
    pub(crate) hot_path_ns: AtomicU64,
}

/// 记录队列上的一条消息。`order` 是 sink 内单调序号：`order` 更小的控制命令
/// 必须先于该行生效，`order` 更大的则不得影响该行。
struct LineMessage {
    order: u64,
    line: String,
}

/// 控制通道上的命令（无界，绝不丢弃）。
enum Control {
    BeginRun {
        run: u64,
        start: Instant,
        run_start_unix_ms: u64,
    },
    Switch(PathBuf),
    Stop {
        stop_reason: String,
        rounds: u32,
        exit_code: i32,
        usage: Option<Value>,
        run: u64,
        t_ms: u64,
        dt_ms: u64,
        ack: SyncSender<()>,
    },
}

/// 带因果序的控制命令。
struct ControlMessage {
    order: u64,
    command: Control,
}

/// 记录路径的共享上下文：`DebugSink` 与 [`DebugHandle`] 都必须能写记录。
///
/// `record` 与回合执行必须**并发**（有界信道若无并发消费者会死锁），但二者
/// 不能同时持有 `&mut DebugSink`。故把「写一条记录」所需的字段抽成可克隆的
/// 上下文：sink 持有它的原件，回合开始前克隆一个 [`DebugHandle`] 交给执行侧。
///
/// 因果序号 `next_order` 是 `Arc<AtomicU64>`：两个写者必须共用同一个计数器，
/// 否则 writer 的 `order` 比较会错乱。
#[derive(Clone)]
struct RecordContext {
    tx: SyncSender<LineMessage>,
    stats: Arc<DebugStats>,
    /// 下一个消息的因果序号。`record` 与控制命令共用同一计数器。
    next_order: Arc<AtomicU64>,
    /// 当前 run 的单调原点（`begin_run` 重置）。
    start: Instant,
    run_start_unix_ms: u64,
    run: u64,
    args_mode: ArgsMode,
}

/// 可克隆的记录句柄：只暴露 `record`，不持有 writer / 控制通道。
///
/// 生命周期约束：在 `begin_run` / `switch_file` **之后**创建；这两个操作
/// 只在回合之间发生（CLI 在启动期，TUI 在请求处理中）。
#[derive(Clone)]
pub(crate) struct DebugHandle {
    ctx: RecordContext,
}

/// 退出时写入 `run_end` 的元数据（design §4.2）。
///
/// `stop_reason` 是**终局事件名**（`RunFinished` / `RunFailed`）；预算取消、
/// 无输出等仍属 `RunFinished`，由 `exit_code` 区分。
pub(crate) struct RunEndMeta<'a> {
    pub stop_reason: &'a str,
    pub rounds: u32,
    pub exit_code: i32,
    /// 本 run 的累计 usage（子 agent 已上卷）；无可用读数时为 `None`。
    pub usage: Option<&'a Usage>,
}

/// 异步单写者 sink。`None` 表示 debug 关闭（零线程、零文件、零分配）。
pub(crate) struct DebugSink {
    ctx: RecordContext,
    control: Sender<ControlMessage>,
    writer: Option<JoinHandle<()>>,
}

impl DebugSink {
    /// 打开 `path` 并起写线程。目录 / 文件不可写时返回 `None`（静默降级）。
    pub(crate) fn open(path: PathBuf, capacity: usize) -> Option<Self> {
        Self::open_file(
            path,
            capacity,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::from_env(),
        )
    }

    fn open_file(
        path: PathBuf,
        capacity: usize,
        file_limit_bytes: u64,
        args_mode: ArgsMode,
    ) -> Option<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).ok()?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let initial_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Self::spawn(
            Box::new(file),
            initial_bytes,
            capacity,
            file_limit_bytes,
            args_mode,
        )
    }

    fn spawn(
        writer: Box<dyn Write + Send>,
        initial_bytes: u64,
        capacity: usize,
        file_limit_bytes: u64,
        args_mode: ArgsMode,
    ) -> Option<Self> {
        // 记录队列容量 0 会让 `sync_channel` panic；下限取 1。
        let (tx, data_rx) = sync_channel(capacity.max(1));
        // 控制命令罕见，无界通道不会失控，也不会阻塞生产者。
        let (control_tx, control_rx) = channel();
        let stats = Arc::new(DebugStats::default());
        stats.bytes_written.store(initial_bytes, Ordering::Relaxed);

        let start = Instant::now();
        let run_start_unix_ms = now_unix_ms();
        let init = WriterInit {
            run: 0,
            start,
            run_start_unix_ms,
            bytes: initial_bytes,
        };
        let writer_stats = Arc::clone(&stats);
        let handle = std::thread::Builder::new()
            .name("yushan-debug".to_string())
            .spawn(move || {
                writer_loop(
                    writer,
                    data_rx,
                    control_rx,
                    writer_stats,
                    file_limit_bytes,
                    init,
                );
            })
            .ok()?;

        Some(Self {
            ctx: RecordContext {
                tx,
                stats,
                next_order: Arc::new(AtomicU64::new(0)),
                start,
                run_start_unix_ms,
                run: 0,
                args_mode,
            },
            control: control_tx,
            writer: Some(handle),
        })
    }

    /// 取下一个因果序号。
    fn take_order(&self) -> u64 {
        self.ctx.next_order.fetch_add(1, Ordering::Relaxed)
    }

    /// 当前参数摘要档位（供 `RoundClock` 构造时使用）。
    pub(crate) fn args_mode(&self) -> ArgsMode {
        self.ctx.args_mode
    }

    /// 克隆一个可并发写记录的句柄（回合执行侧使用）。
    pub(crate) fn handle(&self) -> DebugHandle {
        DebugHandle {
            ctx: self.ctx.clone(),
        }
    }

    /// 记录一条事件。
    ///
    /// 契约顺序：**先取时间与序号，再拼 JSON**。`hot_path_ns` 只累计序列化 +
    /// 入队，用于自证扰动上界。
    pub(crate) fn record(
        &mut self,
        ev: &'static str,
        ids: DebugIds<'_>,
        fields: Map<String, Value>,
    ) {
        record_with(&self.ctx, ev, ids, fields);
    }
}

impl DebugHandle {
    /// 记录一条事件（与 [`DebugSink::record`] 同一实现）。
    pub(crate) fn record(
        &self,
        ev: &'static str,
        ids: DebugIds<'_>,
        fields: Map<String, Value>,
    ) {
        record_with(&self.ctx, ev, ids, fields);
    }
}

/// 记录路径的唯一实现：两个入口（sink / handle）只在调用者身份上不同。
fn record_with(ctx: &RecordContext, ev: &'static str, ids: DebugIds<'_>, fields: Map<String, Value>) {
        let dt_ms = ctx.start.elapsed().as_millis() as u64;
        let t_ms = ctx.run_start_unix_ms.saturating_add(dt_ms);
        let seq = ctx.stats.seq.fetch_add(1, Ordering::Relaxed);
        let order = ctx.take_order();

        let hot_start = Instant::now();
        #[cfg(test)]
        {
            let delay_ms = SLOW_SERIALIZE_MS.with(|cell| cell.get());
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
        }

        let mut record = fields;
        record.insert("schema".to_string(), json!(SCHEMA_VERSION));
        record.insert("ev".to_string(), json!(ev));
        record.insert("t_ms".to_string(), json!(t_ms));
        record.insert("dt_ms".to_string(), json!(dt_ms));
        record.insert("seq".to_string(), json!(seq));
        record.insert("run".to_string(), json!(ctx.run));
        record.insert("session_id".to_string(), json!(ids.session_id));
        if let Some(turn) = ids.turn {
            record.insert("turn".to_string(), json!(turn));
        }
        if let Some(round) = ids.round {
            record.insert("round".to_string(), json!(round));
        }
        if let Some(tool_call_id) = ids.tool_call_id {
            record.insert("tool_call_id".to_string(), json!(tool_call_id));
        }

        match serde_json::to_string(&record) {
            Ok(mut line) => {
                line.push('\n');
                match ctx.tx.try_send(LineMessage { order, line }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        ctx.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            }
            Err(_) => {
                // 序列化失败同样是「记录缺失」，必须可解释。
                ctx.stats.serde_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        ctx.stats
            .hot_path_ns
            .fetch_add(hot_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

impl RecordContext {
    fn take_order(&self) -> u64 {
        self.next_order.fetch_add(1, Ordering::Relaxed)
    }
}

impl DebugSink {
    /// `/new` 换会话时切到新文件。同一 session 的 `/resume` **不调用**（继续追加）。
    pub(crate) fn switch_file(&mut self, path: &Path) {
        // 控制通道无界：即使记录队列已满，switch 也不会丢失。
        let order = self.take_order();
        let _ = self.control.send(ControlMessage {
            order,
            command: Control::Switch(path.to_path_buf()),
        });
    }

    /// `/resume` 后重置单调原点与 `run`：同一文件里的新 run 以新的
    /// `run_start_unix_ms` 为基准。
    pub(crate) fn begin_run(&mut self, run: u64, run_start_unix_ms: u64) {
        self.ctx.run = run;
        self.ctx.run_start_unix_ms = run_start_unix_ms;
        self.ctx.start = Instant::now();
        let order = self.take_order();
        let _ = self.control.send(ControlMessage {
            order,
            command: Control::BeginRun {
                run,
                start: self.ctx.start,
                run_start_unix_ms,
            },
        });
    }

    /// 排空队列并写 `run_end`，最多等 [`FLUSH_TIMEOUT`]；超时放弃（best-effort）。
    ///
    /// **终止性、一次性**：本函数只用于退出路径。writer 写完 `run_end` 后线程
    /// 退出、记录信道断开，此后本 sink 不再接受记录（`record` 命中 `Disconnected`
    /// 时既不写也不计 `dropped`），重复调用也不再有意义。
    ///
    /// 若将来需要「运行中途落盘」，那是另一个接口，**不要复用本函数**。
    ///
    /// 测试与内部调用方用这个简化形态；生产退出路径用 [`Self::flush_run`]
    /// 提供 `run_end` 的完整字段（rounds / exit_code / 累计 usage）。
    pub(crate) fn flush(&mut self, stop_reason: &str) {
        self.flush_run(RunEndMeta {
            stop_reason,
            rounds: 0,
            exit_code: 0,
            usage: None,
        });
    }

    /// 排空队列并写 `run_end`（完整元数据版）。
    pub(crate) fn flush_run(&mut self, meta: RunEndMeta<'_>) {
        let dt_ms = self.ctx.start.elapsed().as_millis() as u64;
        let t_ms = self.ctx.run_start_unix_ms.saturating_add(dt_ms);
        let (ack_tx, ack_rx) = sync_channel(1);

        let order = self.take_order();
        let stop = ControlMessage {
            order,
            command: Control::Stop {
                stop_reason: meta.stop_reason.to_string(),
                rounds: meta.rounds,
                exit_code: meta.exit_code,
                usage: meta.usage.and_then(|usage| serde_json::to_value(usage).ok()),
                run: self.ctx.run,
                t_ms,
                dt_ms,
                ack: ack_tx,
            },
        };
        // 无界通道不会满；`Stop` 的 `order` 保证 writer 先写完此前入队的记录。
        if self.control.send(stop).is_err() {
            return;
        }

        let acknowledged = ack_rx.recv_timeout(FLUSH_TIMEOUT).is_ok();
        if acknowledged && let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }

    /// 测试缝：注入自定义 writer（慢 writer / 小文件上限）。
    ///
    /// 生产路径不暴露；`Box<dyn Write>` 只在 writer 线程内使用。
    #[cfg(test)]
    fn open_with_writer<W: Write + Send + 'static>(
        writer: W,
        capacity: usize,
        file_limit_bytes: u64,
        args_mode: ArgsMode,
    ) -> Option<Self> {
        Self::spawn(Box::new(writer), 0, capacity, file_limit_bytes, args_mode)
    }
}

/// writer 线程的初始状态。
struct WriterInit {
    run: u64,
    start: Instant,
    run_start_unix_ms: u64,
    bytes: u64,
}

/// writer 线程的可变状态。
struct WriterState {
    writer: Box<dyn Write + Send>,
    stats: Arc<DebugStats>,
    file_limit_bytes: u64,
    run: u64,
    start: Instant,
    run_start_unix_ms: u64,
    /// **当前文件**的字节数（用上限判定）；`stats.bytes_written` 跨文件累计。
    written: u64,
    consecutive_errors: u32,
    halted: bool,
    truncated: bool,
    /// 已从控制通道取出、但 `order` 还不小于当前记录，尚不能生效的命令。
    pending_control: VecDeque<ControlMessage>,
}

/// `Stop` 取出后需要 writer 收尾的字段。
struct StopCommand {
    stop_reason: String,
    rounds: u32,
    exit_code: i32,
    usage: Option<Value>,
    run: u64,
    t_ms: u64,
    dt_ms: u64,
    ack: SyncSender<()>,
}

/// 单写者循环：唯一的文件 IO 发生在这里。
///
/// 记录与控制走两条通道，靠每条消息的 `order` 恢复因果顺序：
/// **写某条记录之前**先应用所有 `order` 更小的控制命令；`order` 更大的控制
/// 命令留在 `pending_control`，等后续记录越过它再生效。
fn writer_loop(
    writer: Box<dyn Write + Send>,
    data_rx: Receiver<LineMessage>,
    control_rx: Receiver<ControlMessage>,
    stats: Arc<DebugStats>,
    file_limit_bytes: u64,
    init: WriterInit,
) {
    let mut state = WriterState {
        writer,
        stats,
        file_limit_bytes,
        run: init.run,
        start: init.start,
        run_start_unix_ms: init.run_start_unix_ms,
        written: init.bytes,
        consecutive_errors: 0,
        halted: false,
        truncated: false,
        pending_control: VecDeque::new(),
    };

    loop {
        match data_rx.recv_timeout(CONTROL_POLL_INTERVAL) {
            Ok(message) => {
                state.pull_control(&control_rx);
                if let Some(stop) = state.apply_control_before(Some(message.order)) {
                    state.finish(stop);
                    return;
                }
                state.write_line(&message.line);
            }
            Err(RecvTimeoutError::Timeout) => {
                state.pull_control(&control_rx);
                if state.has_stop_pending() {
                    // `flush` 已发出：不再有新记录。先把 `Stop` 之前入队的
                    // 记录按各自 `order` 写完，再收尾。
                    while let Ok(message) = data_rx.try_recv() {
                        state.pull_control(&control_rx);
                        if let Some(stop) = state.apply_control_before(Some(message.order)) {
                            state.finish(stop);
                            return;
                        }
                        state.write_line(&message.line);
                    }
                    if let Some(stop) = state.apply_control_before(None) {
                        state.finish(stop);
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                state.pull_control(&control_rx);
                if let Some(stop) = state.apply_control_before(None) {
                    state.finish(stop);
                }
                return;
            }
        }
    }
}

impl WriterState {
    /// 把控制通道里已到达的命令移入 `pending_control`（不阻塞）。
    fn pull_control(&mut self, control_rx: &Receiver<ControlMessage>) {
        while let Ok(message) = control_rx.try_recv() {
            self.pending_control.push_back(message);
        }
    }

    fn has_stop_pending(&self) -> bool {
        self.pending_control
            .iter()
            .any(|message| matches!(message.command, Control::Stop { .. }))
    }

    /// 应用 `pending_control` 队首中 `order < limit` 的命令；`limit = None`
    /// 表示全部应用。返回遇到的 `Stop`（若应用到了它）。
    fn apply_control_before(&mut self, limit: Option<u64>) -> Option<StopCommand> {
        loop {
            let order = self.pending_control.front()?.order;
            let applicable = match limit {
                None => true,
                Some(limit) => order < limit,
            };
            if !applicable {
                return None;
            }
            let message = self.pending_control.pop_front().unwrap();
            match message.command {
                Control::BeginRun {
                    run,
                    start,
                    run_start_unix_ms,
                } => {
                    self.run = run;
                    self.start = start;
                    self.run_start_unix_ms = run_start_unix_ms;
                }
                Control::Switch(path) => {
                    if self.halted {
                        continue;
                    }
                    if let Some(file) = open_append(&path) {
                        self.written = file.1;
                        self.writer = Box::new(file.0);
                        // 新文件有自己的 64 MiB 预算。
                        self.truncated = false;
                        self.consecutive_errors = 0;
                    }
                }
                Control::Stop {
                    stop_reason,
                    rounds,
                    exit_code,
                    usage,
                    run,
                    t_ms,
                    dt_ms,
                    ack,
                } => {
                    return Some(StopCommand {
                        stop_reason,
                        rounds,
                        exit_code,
                        usage,
                        run,
                        t_ms,
                        dt_ms,
                        ack,
                    });
                }
            }
        }
    }

    fn write_line(&mut self, line: &str) {
        if self.halted || self.truncated {
            return;
        }
        match self.writer.write_all(line.as_bytes()) {
            Ok(()) => {
                self.written = self.written.saturating_add(line.len() as u64);
                self.stats
                    .bytes_written
                    .fetch_add(line.len() as u64, Ordering::Relaxed);
                self.stats.records_written.fetch_add(1, Ordering::Relaxed);
                self.consecutive_errors = 0;

                if self.written >= self.file_limit_bytes {
                    let marker = truncation_line(
                        &self.stats,
                        self.run,
                        self.start,
                        self.run_start_unix_ms,
                        self.file_limit_bytes,
                    );
                    if self.writer.write_all(marker.as_bytes()).is_ok() {
                        self.written = self.written.saturating_add(marker.len() as u64);
                        self.stats
                            .bytes_written
                            .fetch_add(marker.len() as u64, Ordering::Relaxed);
                        self.stats.records_written.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    self.truncated = true;
                }
            }
            Err(_) => {
                self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                self.consecutive_errors += 1;
                if self.consecutive_errors >= MAX_CONSECUTIVE_WRITE_ERRORS {
                    self.halted = true;
                }
            }
        }
    }

    fn finish(&mut self, stop: StopCommand) {
        if !self.halted && !self.truncated {
            let line = run_end_line(&self.stats, &stop);
            if self.writer.write_all(line.as_bytes()).is_ok() {
                self.stats
                    .bytes_written
                    .fetch_add(line.len() as u64, Ordering::Relaxed);
            } else {
                self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        let _ = self.writer.flush();
        let _ = stop.ack.send(());
    }
}

/// 以追加方式打开路径；返回 `(文件, 现有字节数)`。
fn open_append(path: &Path) -> Option<(std::fs::File, u64)> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some((file, len))
}

/// 触顶标记：写一条后停写（当前文件不再接受任何记录，包括 `run_end`）。
fn truncation_line(
    stats: &DebugStats,
    run: u64,
    start: Instant,
    run_start_unix_ms: u64,
    limit_bytes: u64,
) -> String {
    let dt_ms = start.elapsed().as_millis() as u64;
    let t_ms = run_start_unix_ms.saturating_add(dt_ms);
    let seq = stats.seq.fetch_add(1, Ordering::Relaxed);
    let record = json!({
        "schema": SCHEMA_VERSION,
        "ev": "log_truncated",
        "t_ms": t_ms,
        "dt_ms": dt_ms,
        "seq": seq,
        "run": run,
        "limit_bytes": limit_bytes,
    });
    let mut line = serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

/// 收尾记录：`debug{}` 自报丢弃与扰动，便于判断“记录缺失”的可信度。
fn run_end_line(stats: &DebugStats, stop: &StopCommand) -> String {
    let seq = stats.seq.fetch_add(1, Ordering::Relaxed);
    let mut record = json!({
        "schema": SCHEMA_VERSION,
        "ev": "run_end",
        "t_ms": stop.t_ms,
        "dt_ms": stop.dt_ms,
        "seq": seq,
        "run": stop.run,
        "stop_reason": stop.stop_reason,
        "rounds": stop.rounds,
        "duration_ms": stop.dt_ms,
        "exit_code": stop.exit_code,
        "debug": {
            "records_written": stats.records_written.load(Ordering::Relaxed),
            "dropped": stats.dropped.load(Ordering::Relaxed),
            "serde_failures": stats.serde_failures.load(Ordering::Relaxed),
            "write_errors": stats.write_errors.load(Ordering::Relaxed),
            "bytes_written": stats.bytes_written.load(Ordering::Relaxed),
            "hot_path_ms": stats.hot_path_ns.load(Ordering::Relaxed) / 1_000_000,
        },
    });
    if let Some(usage) = &stop.usage {
        record["usage"] = usage.clone();
    }
    let mut line = serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

// ---------------------------------------------------------------------------
// 工具参数摘要与脱敏
// ---------------------------------------------------------------------------

/// 工具参数摘要。`arguments` 是**安全投影**；完整内容以 session JSONL 为准。
#[derive(Debug)]
pub(crate) struct ToolArgsSummary {
    pub arguments: Value,
    pub original_bytes: usize,
    pub truncated: bool,
    pub keys: Vec<String>,
    pub redacted_keys: Vec<String>,
}

/// 按工具规则投影参数，再统一脱敏与总预算约束。
pub(crate) fn summarize_tool_args(tool: &str, args: &Value, mode: ArgsMode) -> ToolArgsSummary {
    let original_bytes = serde_json::to_vec(args).map(|v| v.len()).unwrap_or(0);
    let keys = args
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();

    let mut redacted = args.clone();
    let mut redacted_keys = Vec::new();
    redact_sensitive(&mut redacted, &mut redacted_keys);
    redacted_keys.sort();
    redacted_keys.dedup();

    let mut out = Map::new();
    let mut truncated = mode == ArgsMode::None;

    if mode != ArgsMode::None {
        match tool.to_ascii_lowercase().as_str() {
            "bash" => summarize_bash(&redacted, mode, &mut out, &mut truncated),
            "read" => summarize_read(&redacted, &mut out, &mut truncated),
            "write" => summarize_write(&redacted, &mut out, &mut truncated),
            "edit" => summarize_edit(&redacted, &mut out, &mut truncated),
            "grep" | "find" | "ls" => summarize_search(&redacted, &mut out, &mut truncated),
            "websearch" => summarize_websearch(&redacted, mode, &mut out, &mut truncated),
            "webfetch" => summarize_webfetch(&redacted, &mut out, &mut truncated),
            "subagent" => summarize_subagent(&redacted, &mut out, &mut truncated),
            _ => {
                if mode == ArgsMode::Full {
                    // 未知工具只在 full 档保留完整投影（受 8 KiB 预算约束）。
                    out = redacted.as_object().cloned().unwrap_or_default();
                } else {
                    // head / none：未知工具只留 keys + bytes。
                    truncated = true;
                }
            }
        }

        // 敏感键无论工具规则是否保留其值，都以占位符留下，便于核对脱敏生效。
        if let Some(map) = args.as_object() {
            for key in map.keys() {
                if is_sensitive_key(key) {
                    out.insert(key.clone(), Value::String("[REDACTED]".to_string()));
                }
            }
        }
    }

    let arguments = if mode == ArgsMode::None {
        Value::Object(Map::new())
    } else {
        let mut value = Value::Object(out);
        if enforce_budget(&mut value, mode.budget()) {
            truncated = true;
        }
        value
    };

    ToolArgsSummary {
        arguments,
        original_bytes,
        truncated,
        keys,
        redacted_keys,
    }
}

fn summarize_bash(
    args: &Value,
    mode: ArgsMode,
    out: &mut Map<String, Value>,
    truncated: &mut bool,
) {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return;
    };
    out.insert("command_bytes".to_string(), json!(command.len()));
    let value = if mode == ArgsMode::Full {
        command.to_string()
    } else {
        head_tail(command, 160, 64, truncated)
    };
    out.insert("command".to_string(), Value::String(value));
}

fn summarize_read(args: &Value, out: &mut Map<String, Value>, truncated: &mut bool) {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let (value, cut) = head_str(path, 512);
        *truncated |= cut;
        out.insert("path".to_string(), Value::String(value.to_string()));
    }
    copy_numbers(args, out, &["offset", "limit"]);
}

fn summarize_write(args: &Value, out: &mut Map<String, Value>, truncated: &mut bool) {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let (value, cut) = head_str(path, 512);
        *truncated |= cut;
        out.insert("path".to_string(), Value::String(value.to_string()));
    }
    if let Some(content) = args.get("content").and_then(Value::as_str) {
        // 正文一个字节都不记；只留尺寸供定位。空正文没有省略任何信息。
        out.insert("content_bytes".to_string(), json!(content.len()));
        out.insert("content_lines".to_string(), json!(content.lines().count()));
        *truncated |= !content.is_empty();
    }
}

fn summarize_edit(args: &Value, out: &mut Map<String, Value>, truncated: &mut bool) {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let (value, cut) = head_str(path, 512);
        *truncated |= cut;
        out.insert("path".to_string(), Value::String(value.to_string()));
    }

    let (count, old_bytes, new_bytes) = match args.get("edits").and_then(Value::as_array) {
        Some(edits) => {
            let mut old_bytes = 0usize;
            let mut new_bytes = 0usize;
            for edit in edits {
                old_bytes += edit
                    .get("oldText")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
                new_bytes += edit
                    .get("newText")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
            }
            (edits.len(), old_bytes, new_bytes)
        }
        None if args.get("oldText").is_some() || args.get("newText").is_some() => {
            let old_bytes = args
                .get("oldText")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0);
            let new_bytes = args
                .get("newText")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0);
            (1, old_bytes, new_bytes)
        }
        None => (0, 0, 0),
    };

    out.insert("edits".to_string(), json!(count));
    out.insert("old_bytes".to_string(), json!(old_bytes));
    out.insert("new_bytes".to_string(), json!(new_bytes));
    // oldText / newText 正文一个字节都不记；只有确实省略了非空正文才算截断。
    *truncated |= old_bytes > 0 || new_bytes > 0;
}

fn summarize_search(args: &Value, out: &mut Map<String, Value>, truncated: &mut bool) {
    for key in ["pattern", "path", "glob"] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            let (value, cut) = head_str(value, 512);
            *truncated |= cut;
            out.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
    copy_numbers(args, out, &["limit", "offset", "context"]);
}

fn summarize_websearch(
    args: &Value,
    mode: ArgsMode,
    out: &mut Map<String, Value>,
    truncated: &mut bool,
) {
    if let Some(query) = args.get("query").and_then(Value::as_str) {
        let value = if mode == ArgsMode::Full {
            query.to_string()
        } else {
            let (value, cut) = head_str(query, HEAD_BUDGET_BYTES);
            *truncated |= cut;
            value.to_string()
        };
        out.insert("query".to_string(), Value::String(value));
    }
    copy_numbers(args, out, &["limit"]);
}

fn summarize_webfetch(args: &Value, out: &mut Map<String, Value>, truncated: &mut bool) {
    if let Some(url) = args.get("url").and_then(Value::as_str) {
        // URL query 可能含 token：一律替换。
        let (value, cut) = redact_url(url);
        *truncated |= cut;
        out.insert("url".to_string(), Value::String(value));
    }
    if let Some(prompt) = args.get("prompt").and_then(Value::as_str) {
        // 只记尺寸，不记提示词正文；空提示词没有省略任何信息。
        out.insert("prompt_bytes".to_string(), json!(prompt.len()));
        *truncated |= !prompt.is_empty();
    }
}

fn summarize_subagent(args: &Value, out: &mut Map<String, Value>, truncated: &mut bool) {
    if let Some(prompt) = args.get("prompt").and_then(Value::as_str) {
        let first_line = prompt.lines().next().unwrap_or("");
        let (value, cut) = head_str(first_line, 256);
        *truncated |= cut || first_line.len() < prompt.len();
        out.insert("prompt".to_string(), Value::String(value.to_string()));
        out.insert("prompt_bytes".to_string(), json!(prompt.len()));
    }
}

fn copy_numbers(args: &Value, out: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = args.get(*key)
            && value.is_number()
        {
            out.insert((*key).to_string(), value.clone());
        }
    }
}

/// 键名小写包含任一敏感词 ⇒ 值替换为 `[REDACTED]`。所有档位都生效。
fn is_sensitive_key(key: &str) -> bool {
    const NEEDLES: [&str; 9] = [
        "token",
        "api_key",
        "access_key",
        "secret",
        "password",
        "authorization",
        "cookie",
        "credential",
        "private_key",
    ];
    let key = key.to_ascii_lowercase();
    NEEDLES.iter().any(|needle| key.contains(needle))
}

/// 递归脱敏，收集命中的**原始键名**。
fn redact_sensitive(value: &mut Value, redacted_keys: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *value = Value::String("[REDACTED]".to_string());
                    redacted_keys.push(key.clone());
                } else {
                    redact_sensitive(value, redacted_keys);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_sensitive(item, redacted_keys);
            }
        }
        _ => {}
    }
}

/// 把字符串压到 `max` 字节内的 UTF-8 边界（**不切断多字节字符**）。
fn head_str(value: &str, max: usize) -> (&str, bool) {
    let end = floor_boundary(value, max);
    (&value[..end], end < value.len())
}

/// 取 UTF-8 安全的前 `head` 字节 + 后 `tail` 字节，中间以 `…` 标明省略。
fn head_tail(value: &str, head: usize, tail: usize, truncated: &mut bool) -> String {
    if value.len() <= head + tail {
        return value.to_string();
    }
    *truncated = true;
    let head_end = floor_boundary(value, head);
    let mut tail_start = value.len().saturating_sub(tail);
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}…{}", &value[..head_end], &value[tail_start..])
}

fn floor_boundary(value: &str, max: usize) -> usize {
    if max >= value.len() {
        return value.len();
    }
    let mut index = max;
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 只留 `scheme://host/path`：去掉 userinfo 与 query/fragment 的内容。
///
/// 返回 `(脱敏后的 URL, 是否丢弃了任何内容)`。
fn redact_url(url: &str) -> (String, bool) {
    let without_fragment = url.split_once('#').map(|(base, _)| base).unwrap_or(url);
    let had_fragment = without_fragment.len() < url.len();
    let (base, had_query) = match without_fragment.split_once('?') {
        Some((base, _)) => (base, true),
        None => (without_fragment, false),
    };
    let (base, had_userinfo) = strip_userinfo(base);
    let value = if had_query {
        format!("{base}?<redacted>")
    } else {
        base
    };
    (value, had_query || had_fragment || had_userinfo)
}

fn strip_userinfo(url: &str) -> (String, bool) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), false);
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        // 用户信息（可能含密码）不落盘。
        Some(at) => (format!("{scheme}://{}", &rest[at + 1..]), true),
        None => (url.to_string(), false),
    }
}

/// 总预算约束：超预算时逐轮把最长字符串截半（UTF-8 安全），直到收敛。
fn enforce_budget(value: &mut Value, budget: usize) -> bool {
    let mut truncated = false;
    for _ in 0..64 {
        let length = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
        if length <= budget {
            break;
        }
        let longest = longest_string_len(value);
        if longest == 0 {
            break;
        }
        halve_strings_of_len(value, longest);
        truncated = true;
    }
    truncated
}

fn longest_string_len(value: &Value) -> usize {
    match value {
        Value::String(string) => string.len(),
        Value::Object(map) => map.values().map(longest_string_len).max().unwrap_or(0),
        Value::Array(items) => items.iter().map(longest_string_len).max().unwrap_or(0),
        _ => 0,
    }
}

fn halve_strings_of_len(value: &mut Value, length: usize) {
    match value {
        Value::String(string) if string.len() == length => {
            if length <= 4 {
                string.clear();
            } else {
                let keep = floor_boundary(string, length / 2);
                string.truncate(keep);
                // `…` 本身占 3 字节：长度 5/6 时 `keep + 3` 不小于原长，
                // 补省略号不会让字符串变短 ⇒ 预算永不收敛（64 轮后放弃）。
                // 这类退化情形直接清空。
                if keep + '…'.len_utf8() < length {
                    string.push('…');
                } else {
                    string.clear();
                }
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                halve_strings_of_len(item, length);
            }
        }
        Value::Array(items) => {
            for item in items {
                halve_strings_of_len(item, length);
            }
        }
        _ => {}
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 测试缝
// ---------------------------------------------------------------------------

// 测试用：模拟“序列化很慢”。记录时间戳在此之前取得，`dt_ms` 不应包含它。
#[cfg(test)]
thread_local! {
    static SLOW_SERIALIZE_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    /// 测试目录名含**进程内原子序号 + `process::id()`**：并行测试不得撞名。
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_debug_{name}_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ids(session: &'static str) -> DebugIds<'static> {
        DebugIds {
            session_id: session,
            turn: Some(1),
            round: Some(0),
            tool_call_id: None,
        }
    }

    // ---- 测试 writer ----

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }

        fn lines(&self) -> Vec<String> {
            let bytes = self.bytes();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 每行睡 `delay` 的 writer：用来触发队列满。
    struct SlowBuf {
        inner: SharedBuf,
        delay: Duration,
    }

    impl Write for SlowBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.delay);
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 每次写入都失败的 writer：`write_all` 一次调用即返回 `Err`。
    struct FailingBuf {
        attempts: Arc<AtomicU64>,
    }

    impl Write for FailingBuf {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::other("injected write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 写入阻塞很久的 writer：用来验证 `flush` 的 2 秒上界。
    struct BlockingBuf {
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Write for BlockingBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.started.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(5));
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn record_fields(index: u64) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("index".to_string(), json!(index));
        fields
    }

    // ---- T1 / 开关矩阵 ----

    /// 关闭 debug：`open_enabled` 在解析路径、建目录、起线程之前早退。
    ///
    /// 变异：删掉 `open_enabled` 里的 `if !debug_enabled { return None }` 早退
    /// ⇒ 会走到 `DebugSink::open` 并创建 `debug-*.jsonl`，文件数断言变红。
    #[test]
    fn test_disabled_debug_writes_nothing() {
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&[DEBUG_ENV, crate::logging::LOG_DIR_ENV]);
        let dir = temp_dir("disabled");
        // SAFETY: 持有 env_lock，独占这两个变量的修改与恢复。
        unsafe {
            std::env::set_var(DEBUG_ENV, "0");
            std::env::set_var(crate::logging::LOG_DIR_ENV, &dir);
        }
        let session = dir.join("session-1.jsonl");

        // CLI 为真但 env 明确为关：必须返回 None，且本模块不得创建任何文件。
        let sink = open_enabled(true, Some(&session));
        assert!(
            sink.is_none(),
            "YUSHAN_DEBUG=0 时 open_enabled 必须返回 None"
        );

        // 只检查本模块能产出的 `debug-*`：`YUSHAN_LOG_DIR` 是进程级覆盖，
        // 并发执行的其他用例经 `logging::log` 也可能写 `yushan.log`。
        let debug_files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("debug-"))
            .collect();
        assert!(
            debug_files.is_empty(),
            "关闭 debug 时不得产生 debug-*.jsonl：{debug_files:?}"
        );
        assert!(
            !session.with_file_name("debug-session-1.jsonl").exists(),
            "open_enabled 必须在解析路径之前早退"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 打开 debug 的正向路径：`YUSHAN_DEBUG=1` + `YUSHAN_LOG_DIR` 时确实创建
    /// `debug-<session stem>.jsonl`，且记录能落盘成合法 JSON 行。
    ///
    /// 变异：让 `open_enabled` 永远返回 `None`（debug 永远打不开）⇒ 本用例
    /// `expect` 直接 panic；把路径规则改成 `debug-<pid>-<ms>.jsonl` ⇒
    /// 文件名断言变红。
    #[test]
    fn test_open_enabled_creates_debug_file_for_session() {
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&[
            DEBUG_ENV,
            crate::logging::LOG_DIR_ENV,
            ARGS_ENV,
        ]);
        let dir = temp_dir("enabled");
        // SAFETY: 持有 env_lock，独占这些变量的修改与恢复。
        unsafe {
            std::env::set_var(DEBUG_ENV, "1");
            std::env::set_var(crate::logging::LOG_DIR_ENV, &dir);
            std::env::remove_var(ARGS_ENV);
        }
        let session = dir.join("session-1.jsonl");
        let expected = dir.join("debug-session-1.jsonl");

        let mut sink =
            open_enabled(false, Some(&session)).expect("YUSHAN_DEBUG=1 时必须打开 debug");
        assert!(
            expected.exists(),
            "open_enabled 必须创建 {expected:?}，实际目录：{:?}",
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>()
        );

        sink.record(
            "round_start",
            DebugIds {
                session_id: "session-1",
                turn: Some(1),
                round: Some(0),
                tool_call_id: None,
            },
            record_fields(0),
        );
        sink.flush("RunFinished");

        let content = std::fs::read_to_string(&expected).expect("debug 文件应可读");
        let lines: Vec<&str> = content.lines().collect();
        assert!(!lines.is_empty(), "至少应写出一行：{content:?}");
        let mut evs = Vec::new();
        for line in &lines {
            let value: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("debug 行必须是合法 JSON: {e}: {line}"));
            evs.push(value["ev"].as_str().unwrap_or_default().to_string());
        }
        assert!(evs.iter().any(|ev| ev == "round_start"), "{evs:?}");
        assert_eq!(evs.last().map(String::as_str), Some("run_end"), "{evs:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 开关优先级：env 优先；`1/true/yes/on` 为开、其余非空为关；空串跟随 flag。
    ///
    /// 变异：去掉 env 分支 ⇒ `YUSHAN_DEBUG=0` + flag 的断言变红；
    /// 把空串当 off ⇒ 空串 + flag 的断言变红。
    #[test]
    fn test_debug_enabled_matrix() {
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&[DEBUG_ENV]);

        // SAFETY: 持有 env_lock。
        unsafe { std::env::remove_var(DEBUG_ENV) };
        assert!(debug_enabled(true), "未设置时跟随 cli=true");
        assert!(!debug_enabled(false), "未设置时跟随 cli=false");

        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            // SAFETY: 持有 env_lock。
            unsafe { std::env::set_var(DEBUG_ENV, on) };
            assert!(debug_enabled(false), "{on:?} 应视为开");
        }
        for off in ["0", "false", "no", "off", "anything"] {
            // SAFETY: 持有 env_lock。
            unsafe { std::env::set_var(DEBUG_ENV, off) };
            assert!(!debug_enabled(true), "{off:?} 应视为关（env 优先）");
        }

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var(DEBUG_ENV, "") };
        assert!(debug_enabled(true), "空串视为未设置 ⇒ 跟随 cli=true");
        assert!(!debug_enabled(false), "空串视为未设置 ⇒ 跟随 cli=false");
    }

    #[test]
    fn test_args_mode_from_env() {
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&[ARGS_ENV]);

        // SAFETY: 持有 env_lock。
        unsafe { std::env::remove_var(ARGS_ENV) };
        assert_eq!(ArgsMode::from_env(), ArgsMode::Head);

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var(ARGS_ENV, "") };
        assert_eq!(ArgsMode::from_env(), ArgsMode::Head);

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var(ARGS_ENV, "none") };
        assert_eq!(ArgsMode::from_env(), ArgsMode::None);

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var(ARGS_ENV, "FULL") };
        assert_eq!(ArgsMode::from_env(), ArgsMode::Full);

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var(ARGS_ENV, "bogus") };
        assert_eq!(ArgsMode::from_env(), ArgsMode::Head);
    }

    #[test]
    fn test_debug_path_for_session_and_fallback() {
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&[crate::logging::LOG_DIR_ENV]);
        let dir = temp_dir("path");
        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var(crate::logging::LOG_DIR_ENV, &dir) };

        let path = debug_path_for(Some(Path::new("/sessions/session-42.json"))).unwrap();
        assert_eq!(path, dir.join("debug-session-42.jsonl"));

        let fallback = debug_path_for(None).unwrap();
        let name = fallback.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with(&format!("debug-{}-", std::process::id())),
            "{name}"
        );
        assert!(name.ends_with(".jsonl"), "{name}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 记录信封 / 序号 / 时间基准 ----

    /// 10k 条记录：每行合法 JSON、`seq` 严格 +1、`t_ms - dt_ms` 恒定。
    ///
    /// 变异：把 `t_ms` 改成 `now_unix_ms()`（而非 `run_start + dt`）⇒ 恒定断言变红。
    #[test]
    fn test_records_have_valid_envelope_and_monotonic_seq() {
        let dir = temp_dir("envelope");
        let path = dir.join("debug-envelope.jsonl");
        let mut sink = DebugSink::open_file(
            path.clone(),
            16_384,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();
        let origin = 1_700_000_000_000u64;
        sink.begin_run(7, origin);

        const COUNT: u64 = 10_000;
        for index in 0..COUNT {
            sink.record(
                "round_end",
                DebugIds {
                    session_id: "session-1",
                    turn: Some(2),
                    round: Some(index as u32),
                    tool_call_id: None,
                },
                record_fields(index),
            );
        }
        sink.flush("RunFinished");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with('\n'), "文件必须以完整行结尾");
        let lines: Vec<&str> = content.lines().collect();
        let data: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| !line.contains("\"ev\":\"run_end\""))
            .collect();
        assert_eq!(data.len(), COUNT as usize, "不得丢记录（队列 16384）");

        let mut previous_seq: Option<u64> = None;
        for (index, line) in data.iter().enumerate() {
            let value: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
            assert_eq!(value["schema"], SCHEMA_VERSION);
            assert_eq!(value["ev"], "round_end");
            assert_eq!(value["run"], 7);
            assert_eq!(value["session_id"], "session-1");
            assert_eq!(value["turn"], 2);
            assert_eq!(value["round"], index as u64);
            assert_eq!(value["index"], index as u64);
            let t_ms = value["t_ms"].as_u64().unwrap();
            let dt_ms = value["dt_ms"].as_u64().unwrap();
            assert_eq!(t_ms - dt_ms, origin, "t_ms - dt_ms 必须恒定");
            let seq = value["seq"].as_u64().unwrap();
            if let Some(previous) = previous_seq {
                assert_eq!(seq, previous + 1, "seq 必须严格 +1");
            }
            previous_seq = Some(seq);
        }

        let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["ev"], "run_end");
        assert_eq!(last["stop_reason"], "RunFinished");
        assert_eq!(last["debug"]["records_written"], COUNT);
        assert_eq!(last["debug"]["dropped"], 0);
        assert_eq!(last["debug"]["serde_failures"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `run_end.debug` 必须自报序列化失败数，否则「记录缺失」无法解释。
    ///
    /// 变异：从 `run_end_line` 删掉 `serde_failures` 字段 ⇒ 断言变红。
    #[test]
    fn test_run_end_reports_serde_failures() {
        let buffer = SharedBuf::default();
        let mut sink = DebugSink::open_with_writer(
            buffer.clone(),
            16,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();
        sink.ctx.stats.serde_failures.fetch_add(3, Ordering::Relaxed);
        sink.flush("RunFinished");

        let lines = buffer.lines();
        let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["ev"], "run_end");
        assert_eq!(last["debug"]["serde_failures"], 3);
    }

    /// 时间戳先取：模拟 30 ms 的慢序列化，`dt_ms` 不含它，`hot_path_ns` 含它。
    ///
    /// 变异：把 `dt_ms` 的取得挪到序列化之后 ⇒ `dt_ms < 20` 断言变红。
    #[test]
    fn test_slow_serialize_is_excluded_from_dt_ms() {
        let buffer = SharedBuf::default();
        let mut sink = DebugSink::open_with_writer(
            buffer.clone(),
            64,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();

        SLOW_SERIALIZE_MS.with(|cell| cell.set(30));
        sink.record("round_start", ids("s"), Map::new());
        SLOW_SERIALIZE_MS.with(|cell| cell.set(0));
        sink.flush("RunFinished");

        let lines = buffer.lines();
        let first: Value = serde_json::from_str(&lines[0]).unwrap();
        let dt_ms = first["dt_ms"].as_u64().unwrap();
        assert!(dt_ms < 20, "dt_ms 不得包含序列化耗时：{dt_ms}");
        // `t_ms` 同样必须在序列化之前派生：两者的差恒为 run 原点。
        assert_eq!(
            first["t_ms"].as_u64().unwrap() - dt_ms,
            sink.ctx.run_start_unix_ms,
            "t_ms - dt_ms 必须等于 run 原点（t_ms 不得在慢序列化之后取）"
        );
        let hot_path_ms = sink.ctx.stats.hot_path_ns.load(Ordering::Relaxed) / 1_000_000;
        assert!(
            hot_path_ms >= 25,
            "hot_path_ms 应包含慢序列化：{hot_path_ms}"
        );
    }

    // ---- T4：队列满 ----

    /// 慢 writer + 入队 > 1024：`dropped > 0`，已写出的每行仍是完整 JSON。
    ///
    /// 变异：`try_send` 改成阻塞 `send` ⇒ 本用例在生产者循环里挂住（超时红）。
    #[test]
    fn test_queue_full_drops_but_written_lines_are_complete() {
        let buffer = SharedBuf::default();
        let mut sink = DebugSink::open_with_writer(
            SlowBuf {
                inner: buffer.clone(),
                delay: Duration::from_millis(50),
            },
            DEFAULT_QUEUE_CAPACITY,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();

        for index in 0..(DEFAULT_QUEUE_CAPACITY as u64 * 3) {
            sink.record("round_start", ids("s"), record_fields(index));
        }
        assert!(
            sink.ctx.stats.dropped.load(Ordering::Relaxed) > 0,
            "慢 writer 下队列应溢出"
        );

        // flush 最多等 2 s；writer 仍在排空时直接 detach。
        sink.flush("RunFinished");

        let lines = buffer.lines();
        assert!(!lines.is_empty(), "至少应写出一行");
        for line in &lines {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|e| panic!("半行 JSON: {e}: {line}"));
        }
        assert!(buffer.bytes().ends_with(b"\n"), "快照必须以完整行结尾");
    }

    /// 连续 [`MAX_CONSECUTIVE_WRITE_ERRORS`] 次 IO 错误后必须停写：
    /// 第 9 条记录不得再调用 writer。
    ///
    /// 变异：把 `halted = true` 的阈值改大，或删掉 `write_line` 入口的
    /// `if self.halted { return }` ⇒ 尝试次数 > 8，断言变红。
    #[test]
    fn test_writer_stops_after_consecutive_io_errors() {
        // 设计规格是 8；改常量必须同步改契约（本断言防止常量与用例一起漂移）。
        assert_eq!(MAX_CONSECUTIVE_WRITE_ERRORS, 8);
        const RECORDS: u64 = 32;
        let attempts = Arc::new(AtomicU64::new(0));
        let mut sink = DebugSink::open_with_writer(
            FailingBuf {
                attempts: Arc::clone(&attempts),
            },
            64,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();

        for index in 0..RECORDS {
            sink.record("round_start", ids("s"), record_fields(index));
        }
        sink.flush("RunFinished");

        assert_eq!(
            attempts.load(Ordering::Relaxed),
            MAX_CONSECUTIVE_WRITE_ERRORS as u64,
            "连续失败达到上限后不得继续调用 writer"
        );
        assert_eq!(
            sink.ctx.stats.write_errors.load(Ordering::Relaxed),
            MAX_CONSECUTIVE_WRITE_ERRORS as u64
        );
        assert!(
            sink.ctx.stats.records_written.load(Ordering::Relaxed) == 0,
            "一行都写不成功"
        );
    }

    /// writer 被卡住时，`flush` 必须在 2 秒预算附近返回，而不是无限等 ack。
    ///
    /// 变异：把 `FLUSH_TIMEOUT` 改成 60 秒 ⇒ `flush` 跑 60 秒，3 秒断言变红。
    #[test]
    fn test_flush_returns_within_budget_when_writer_is_stuck() {
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut sink = DebugSink::open_with_writer(
            BlockingBuf {
                started: Arc::clone(&started),
            },
            16,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();
        sink.record("round_start", ids("s"), Map::new());

        // 确认 writer 已进入阻塞写，避免把「快路径直接成功」误当成通过。
        let wait_deadline = Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::SeqCst) {
            assert!(Instant::now() < wait_deadline, "writer 未开始写");
            std::thread::sleep(Duration::from_millis(1));
        }

        let begin = Instant::now();
        sink.flush("RunFinished");
        let elapsed = begin.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "flush 必须在 3 秒内返回（预算 2 秒）：{elapsed:?}"
        );
    }

    /// 文件上限：触顶后写一条 `log_truncated`，之后彻底停写（不再写 `run_end`）。
    ///
    /// 变异：去掉上限判断 ⇒ 出现 `run_end` 且行数远超预期。
    #[test]
    fn test_file_limit_writes_truncation_marker_and_stops() {
        let buffer = SharedBuf::default();
        let mut sink =
            DebugSink::open_with_writer(buffer.clone(), 256, 1024, ArgsMode::Head).unwrap();

        for index in 0..100 {
            sink.record("round_start", ids("s"), record_fields(index));
        }
        sink.flush("RunFinished");

        let lines = buffer.lines();
        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let marker = parsed
            .iter()
            .position(|value| value["ev"] == "log_truncated")
            .expect("应出现 log_truncated");
        assert_eq!(marker, parsed.len() - 1, "log_truncated 必须是最后一行");
        assert!(marker < 100, "触顶后不得继续写记录");
        assert!(
            !parsed.iter().any(|value| value["ev"] == "run_end"),
            "停写后不得再写 run_end"
        );
    }

    // ---- 切文件 / begin_run ----

    #[test]
    fn test_switch_file_and_begin_run_reset_origin() {
        // `DebugSink::open` 会读 `YUSHAN_DEBUG_ARGS`：读取方也必须持锁。
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&[ARGS_ENV]);
        let dir = temp_dir("switch");
        let first = dir.join("debug-first.jsonl");
        let second = dir.join("debug-second.jsonl");
        let mut sink = DebugSink::open(first.clone(), 64).unwrap();

        sink.record("round_start", ids("first"), Map::new());
        sink.switch_file(&second);
        let origin = 1_800_000_000_000u64;
        sink.begin_run(9, origin);
        sink.record("round_start", ids("second"), Map::new());
        sink.flush("RunFinished");

        let first_content = std::fs::read_to_string(&first).unwrap();
        assert!(first_content.contains("\"session_id\":\"first\""));
        assert!(!first_content.contains("\"session_id\":\"second\""));

        let second_content = std::fs::read_to_string(&second).unwrap();
        let lines: Vec<&str> = second_content.lines().collect();
        let record: Value =
            serde_json::from_str(lines.iter().find(|l| l.contains("round_start")).unwrap())
                .unwrap();
        assert_eq!(record["session_id"], "second");
        assert_eq!(record["run"], 9);
        assert_eq!(
            record["t_ms"].as_u64().unwrap() - record["dt_ms"].as_u64().unwrap(),
            origin
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 记录队列满时 `switch` 也必须送达：控制命令走独立无界通道。
    ///
    /// 变异：把 `switch_file` 改回记录队列上的 `try_send` ⇒ 队列满时 switch
    /// 被丢弃，第二个文件根本不会创建，`read_to_string` 直接 panic。
    #[test]
    fn test_switch_is_not_lost_when_record_queue_is_full() {
        let dir = temp_dir("switch_full");
        let second = dir.join("debug-second.jsonl");
        // 容量 1 + 慢 writer：记录队列必然溢出。
        let buffer = SharedBuf::default();
        let mut sink = DebugSink::open_with_writer(
            SlowBuf {
                inner: buffer.clone(),
                delay: Duration::from_millis(50),
            },
            1,
            DEFAULT_FILE_LIMIT_BYTES,
            ArgsMode::Head,
        )
        .unwrap();

        // 灌满队列，确保后续数据 `try_send` 会失败。
        for index in 0..10 {
            sink.record("round_start", ids("old"), record_fields(index));
        }
        sink.switch_file(&second);
        // 等 writer 排空容量为 1 的队列并应用 switch，避免新记录本身被丢弃
        // （本用例验证的是 switch 不丢，不是 record 不丢）。
        std::thread::sleep(Duration::from_millis(400));
        sink.record("round_start", ids("new"), record_fields(999));
        sink.flush("RunFinished");

        let content = std::fs::read_to_string(&second).expect("switch 不得丢失：新文件应存在");
        assert!(
            content.contains("\"session_id\":\"new\""),
            "switch 之后的记录必须写进新文件：{content}"
        );
        assert!(
            !content.contains("\"session_id\":\"old\""),
            "switch 之前的记录不得混入新文件：{content}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 摘要 ----

    #[test]
    fn test_summarize_bash_head_tail() {
        let long = format!("{}{}{}", "H".repeat(160), "M".repeat(76), "T".repeat(64));
        let summary = summarize_tool_args("bash", &json!({ "command": long }), ArgsMode::Head);
        assert_eq!(
            summary.arguments["command"],
            format!("{}…{}", "H".repeat(160), "T".repeat(64))
        );
        assert_eq!(summary.arguments["command_bytes"], 300);
        assert!(summary.truncated);
        assert_eq!(
            summary.original_bytes,
            serde_json::to_vec(&json!({ "command": long }))
                .unwrap()
                .len()
        );

        let short = summarize_tool_args("bash", &json!({ "command": "ls -la" }), ArgsMode::Head);
        assert_eq!(short.arguments["command"], "ls -la");
        assert!(!short.truncated);
    }

    /// 多字节 UTF-8：截断必须落在字符边界，不得出现半个字符。
    #[test]
    fn test_summarize_utf8_boundaries() {
        let command = "你".repeat(100); // 300 B
        let summary = summarize_tool_args("bash", &json!({ "command": command }), ArgsMode::Head);
        let value = summary.arguments["command"].as_str().unwrap();
        assert!(!value.contains('\u{FFFD}'), "不得产生替换字符：{value:?}");
        // head ≤160B + `…`(3B) + tail ≤64B。
        assert!(value.len() <= 160 + 3 + 64, "{}", value.len());
        assert!(value.starts_with(&"你".repeat(53)), "head 应取 159 B");
        assert!(value.ends_with('你'));

        let path = "你".repeat(300); // 900 B
        let read = summarize_tool_args("read", &json!({ "path": path }), ArgsMode::Head);
        let path = read.arguments["path"].as_str().unwrap();
        assert!(!path.contains('\u{FFFD}'));
        // 单字段先按 512 B 截断；若整个 JSON 超过 512 B 总预算，再收口到更短。
        assert!(path.len() <= 512, "{}", path.len());
        assert!(path.len() >= 250, "不得过度截断：{}", path.len());
        assert!(
            path.chars().all(|ch| ch == '你' || ch == '…'),
            "只应有完整字符与省略号：{path:?}"
        );
        assert!(read.truncated);
    }

    /// `write` / `edit` 的正文字节零记录，只留尺寸 —— `head` 与 `full` 两档都
    /// 必须如此（正文省略与摘要档位无关）。
    ///
    /// 变异：把 `content` / `oldText` / `newText` 放进投影（任意一档）⇒ 断言变红。
    #[test]
    fn test_summarize_write_and_edit_record_no_bodies() {
        for mode in [ArgsMode::Head, ArgsMode::Full] {
            let write = summarize_tool_args(
                "write",
                &json!({ "path": "src/a.rs", "content": "SECRET_BODY\nline2" }),
                mode,
            );
            assert!(write.arguments.get("content").is_none(), "{mode:?}");
            assert_eq!(write.arguments["content_bytes"], 17, "{mode:?}");
            assert_eq!(write.arguments["content_lines"], 2, "{mode:?}");
            assert!(
                !write.arguments.to_string().contains("SECRET_BODY"),
                "{mode:?}"
            );
            assert!(write.truncated, "{mode:?}");

            let edit = summarize_tool_args(
                "edit",
                &json!({
                    "path": "src/a.rs",
                    "edits": [
                        { "oldText": "OLD_TEXT_A", "newText": "NEW_TEXT_A" },
                        { "oldText": "OLD_TEXT_BB", "newText": "NEW_TEXT_BB" },
                    ],
                }),
                mode,
            );
            assert_eq!(edit.arguments["edits"], 2, "{mode:?}");
            assert_eq!(edit.arguments["old_bytes"], 21, "{mode:?}");
            assert_eq!(edit.arguments["new_bytes"], 21, "{mode:?}");
            let serialized = edit.arguments.to_string();
            for forbidden in ["OLD_TEXT", "NEW_TEXT"] {
                assert!(!serialized.contains(forbidden), "{mode:?}: {serialized}");
            }
            assert!(edit.truncated, "{mode:?}");
        }
    }

    /// `truncated` 只在**确实省略或截断了内容**时为真：空正文 / 空 edits /
    /// 空 prompt 都不算。
    ///
    /// 变异：把 `summarize_write` / `summarize_edit` / `summarize_webfetch`
    /// 里改回无条件的 `*truncated = true` ⇒ 对应断言变红。
    #[test]
    fn test_truncated_only_when_content_was_omitted() {
        for mode in [ArgsMode::Head, ArgsMode::Full] {
            let write =
                summarize_tool_args("write", &json!({ "path": "src/a.rs", "content": "" }), mode);
            assert!(!write.truncated, "{mode:?}: 空正文没有省略信息");
            assert_eq!(write.arguments["content_bytes"], 0, "{mode:?}");

            let no_edits =
                summarize_tool_args("edit", &json!({ "path": "src/a.rs", "edits": [] }), mode);
            assert!(!no_edits.truncated, "{mode:?}: 空 edits 没有省略信息");

            let blank_edit = summarize_tool_args(
                "edit",
                &json!({
                    "path": "src/a.rs",
                    "edits": [{ "oldText": "", "newText": "" }],
                }),
                mode,
            );
            assert!(
                !blank_edit.truncated,
                "{mode:?}: 空白 oldText/newText 没有省略信息"
            );

            let fetch = summarize_tool_args(
                "webfetch",
                &json!({ "url": "https://example.com/page", "prompt": "" }),
                mode,
            );
            assert!(!fetch.truncated, "{mode:?}: 空 prompt 没有省略信息");
        }
    }

    /// 未知工具在 `head` 档只留 keys + bytes；`full` 档保留投影。
    #[test]
    fn test_summarize_unknown_tool_only_keys_in_head() {
        let args = json!({ "alpha": 1, "beta": "x", "api_key": "sk-live" });
        let summary = summarize_tool_args("mystery", &args, ArgsMode::Head);
        assert_eq!(summary.keys, vec!["alpha", "api_key", "beta"]);
        assert_eq!(summary.arguments["api_key"], "[REDACTED]");
        assert!(summary.arguments.get("alpha").is_none());
        assert!(summary.arguments.get("beta").is_none());
        assert!(summary.truncated);
        assert_eq!(
            summary.original_bytes,
            serde_json::to_vec(&args).unwrap().len()
        );

        let full = summarize_tool_args("mystery", &args, ArgsMode::Full);
        assert_eq!(full.arguments["alpha"], 1);
        assert_eq!(full.arguments["beta"], "x");
        assert_eq!(full.arguments["api_key"], "[REDACTED]");
    }

    /// 脱敏在 `none` / `head` / `full` 三档都生效；原文一个字节都不落进摘要。
    ///
    /// 变异：删掉 `redact_sensitive` 调用 ⇒ 本用例变红。
    #[test]
    fn test_redaction_applies_in_all_modes() {
        let args = json!({
            "api_key": "sk-live-123",
            "Authorization": "Bearer abc",
            "password": "p@ss",
            "token": "t0k3n",
            "other": "visible",
        });
        for mode in [ArgsMode::None, ArgsMode::Head, ArgsMode::Full] {
            let summary = summarize_tool_args("mystery", &args, mode);
            // `none` 档不留任何参数值，但 `redacted_keys` 仍必须完整。
            if mode == ArgsMode::None {
                assert_eq!(summary.arguments, json!({}));
                assert_eq!(
                    summary.redacted_keys,
                    vec!["Authorization", "api_key", "password", "token"]
                );
                continue;
            }
            assert_eq!(summary.arguments["api_key"], "[REDACTED]");
            assert_eq!(summary.arguments["Authorization"], "[REDACTED]");
            assert_eq!(summary.arguments["password"], "[REDACTED]");
            assert_eq!(summary.arguments["token"], "[REDACTED]");
            assert_eq!(
                summary.redacted_keys,
                vec!["Authorization", "api_key", "password", "token"]
            );
            let serialized = summary.arguments.to_string();
            for secret in ["sk-live-123", "Bearer abc", "p@ss", "t0k3n"] {
                assert!(!serialized.contains(secret), "{mode:?}: {serialized}");
            }
        }
    }

    /// T3 的文件大小证据：10,000 条增量只累计计数，debug 文件远小于 200 KiB。
    ///
    /// 变异：给 `ModelTextDelta` / `ModelThinkingDelta` 任一条加记录 ⇒ 本用例红。
    #[test]
    fn test_ten_thousand_deltas_keep_debug_file_under_200_kib() {
        use ys_event::AgentEvent;
        use ys_protocol::{Envelope, Source};

        let dir = temp_dir("deltas_size");
        let path = dir.join("debug.jsonl");
        let mut sink = DebugSink::open(path.clone(), DEFAULT_QUEUE_CAPACITY).unwrap();
        let mut clock = crate::round_clock::RoundClock::new(ArgsMode::Head);
        let mut emit = |env: &Envelope| {
            let observation = clock.observe_debug(env, Instant::now());
            for record in observation.records {
                sink.record(record.ev, ids("s"), record.fields);
            }
        };

        emit(&Envelope::new(
            Source::agent(),
            1,
            AgentEvent::RoundStarted { round: 1 },
        ));
        for index in 0..10_000 {
            let event = if index % 2 == 0 {
                AgentEvent::ModelThinkingDelta {
                    text: "思考".to_string(),
                }
            } else {
                AgentEvent::ModelTextDelta {
                    text: "x".to_string(),
                }
            };
            emit(&Envelope::new(Source::agent(), 1, event));
        }
        emit(&Envelope::new(
            Source::agent(),
            1,
            AgentEvent::RunFinished {
                stop_reason: ys_core::StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            },
        ));
        sink.flush("RunFinished");

        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(
            bytes < 200 * 1024,
            "10k 增量不得写入逐条记录（文件 {bytes} B ≥ 200 KiB）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `webfetch` 的 URL query 一律 `<redacted>`，userinfo 不落盘。
    #[test]
    fn test_webfetch_url_is_sanitized() {
        let summary = summarize_tool_args(
            "webfetch",
            &json!({ "url": "https://user:secret@example.com/page?token=abc#frag" }),
            ArgsMode::Full,
        );
        assert_eq!(
            summary.arguments["url"],
            "https://example.com/page?<redacted>"
        );
        let serialized = summary.arguments.to_string();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("token=abc"));
        assert!(summary.truncated, "丢弃 query/userinfo 应标记 truncated");

        let plain = summarize_tool_args(
            "webfetch",
            &json!({ "url": "https://example.com/page" }),
            ArgsMode::Full,
        );
        assert_eq!(plain.arguments["url"], "https://example.com/page");
        assert!(!plain.truncated);
    }

    /// `subagent` 只留 prompt 首行 256 B + 字节数。
    #[test]
    fn test_subagent_prompt_first_line_only() {
        let prompt = format!("FIRST_LINE\n{}", "body ".repeat(1000));
        let summary = summarize_tool_args("subagent", &json!({ "prompt": prompt }), ArgsMode::Full);
        assert_eq!(summary.arguments["prompt"], "FIRST_LINE");
        assert_eq!(summary.arguments["prompt_bytes"], prompt.len());
        assert!(!summary.arguments.to_string().contains("body body"));
        assert!(summary.truncated);
    }
}
