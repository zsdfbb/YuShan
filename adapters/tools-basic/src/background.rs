//! 后台命令注册表 —— **进程级注册表 + 会话级内容**。
//!
//! 注册表对象随进程存活（工具表是进程级的 ⇒ 注册表也放在进程级），
//! 内容（条目）随会话失效：每条条目带 `session` 字段，查询时必须核对。
//!
//! **两道防线**：句柄号进程内单调递增、永不复用（第一道）；
//! 条目带 `session`、查询时核对（第二道）—— 即便某个清理点漏了，
//! 上个会话的句柄也命中不到本会话的条目（对模型表现为「未知句柄」）。
//!
//! 全部**同步**实现（`std::process` + `std::fs`）：注册表是进程级共享状态，
//! 句柄分配、条目淘汰、子进程回收都在同一把锁内完成。
//!
//! # 两个正交的状态机
//!
//! - [`BgState`]：进程**在跑 / 已退出 / 被杀**；
//! - `Delivery`：结果**还没投出 / 投递中 / 已投出**。
//!
//! 两者互不派生：一条命令可以先退出、再被 abort 有意丢弃（`Delivered` 但不投递）；
//! 也可以仍在跑却已被取消。
//!
//! # 清扫分三段（锁内只标记，重活在锁外）
//!
//! [`BackgroundTasks::sweep`] 分三段：段 1 持锁推进状态并认领候选（`Pending → InFlight`）、
//! 收集待杀 pid；段 2 **无锁**执行 `kill_group`（同步 fork/exec/join）与正文渲染
//! （读输出文件）；段 3 持锁落 `Delivered` / 回 `Pending`。
//!
//! **不变量：清扫路径的锁内绝不 fork / exec / 读输出文件。**
//! 反例代价：`snapshot` / `start` / `take_completions` 都走同一把锁 ⇒
//! 锁内 fork 会把「一条命令到期」的代价摊到**所有**工具调用上（低端设备尤其明显）。
//! 例外只有两个非阻塞操作：`try_wait`（子进程状态）与 `stat`（取输出字节数）。

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant};

use ys_core::Message;
use ys_protocol::{Boundary, QueueBoundarySource};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// 落盘根目录的环境变量名 —— 覆盖默认的 `<temp_dir>/yushan-bg`。
pub const BG_DIR_ENV: &str = "YUSHAN_BG_DIR";

/// 启动包装脚本：**先设写盘上限（内核侧粗闸门），再 `exec` 用户命令**。
///
/// `exec` 让用户命令**接管同一个 pid** ⇒ 进程组 leader 仍是记录在册的那个 pid，
/// 「杀进程组」的语义不变（`kill -9 -<pid>` 仍然命中整个组）。
///
/// 用户命令经**位置参数**传入（`$2` / `$3`）⇒ 不拼进脚本体 ⇒ 命令里的引号
/// 不会被二次解析。
const FILE_LIMIT_SCRIPT: &str = "ulimit -f \"$1\" 2>/dev/null || true; shift; exec sh \"$@\"";

/// `ulimit -f` 的块大小（字节）—— **运行时探测**。
///
/// POSIX 规定 512 字节，但各 shell 实现不同（macOS 的 bash 用 1024）⇒ 猜错会让
/// 限额差一倍（要么提前砍掉输出，要么放行两倍）⇒ 探测一次：把上限设成 1 块，
/// 写一个远超一块的文件，看它被截到多少字节。
///
/// 探测结果**按进程缓存** ⇒ 只付一次子进程开销。
fn ulimit_block_bytes() -> u64 {
    static BLOCK: OnceLock<u64> = OnceLock::new();
    *BLOCK.get_or_init(probe_ulimit_block)
}

/// 探测 `ulimit -f` 的块大小（字节）—— 探测失败时退回 POSIX 的 512。
fn probe_ulimit_block() -> u64 {
    /// 探测用的写入块大小 —— 必须大于任何真实的块大小，
    /// 否则「限额没生效」与「块本身很大」分不开。
    const PROBE_BLOCK: u64 = 64 * 1024;
    /// POSIX 规定的块大小（探测失败时的兜底）。
    const POSIX_BLOCK: u64 = 512;

    let path = std::env::temp_dir().join(format!("yushan_ulimit_probe_{}", std::process::id()));
    let script = concat!(
        "ulimit -f 1 2>/dev/null; ",
        "dd if=/dev/zero of=\"$1\" bs=65536 count=1 2>/dev/null; ",
        "wc -c < \"$1\" 2>/dev/null"
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let _ = fs::remove_file(&path);
    let Ok(output) = output else {
        return POSIX_BLOCK;
    };
    match String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
    {
        // 探到的大小必须**小于**探测用的写入块 ⇒ 限额确实生效了。
        Ok(size) if size > 0 && size < PROBE_BLOCK => size,
        _ => POSIX_BLOCK,
    }
}

/// 内核侧**写盘兜底**的最低字节数 —— `RLIMIT_FSIZE` 粗闸门的下限。
///
/// `RLIMIT_FSIZE` 是**进程级**上限（**被子孙继承**）⇒ 它拦的是「这个进程往
/// **任何**文件写超过这么多字节」，**不是**「输出文件超过 2 MiB」。
/// 把它设成输出上限 ⇒ 正常写大文件的命令（构建产物、打包、落盘日志）会被内核
/// 以 `SIGXFSZ` 杀掉 ⇒ 后台命令的主要用途被内核限额打断。
///
/// 故内核侧的职责收窄为「**兜住失控写盘**」：限额取 `max(输出上限, 64 MiB)`，
/// 对齐到 `ulimit -f` 的块大小 ⇒ 正常工作的命令不受影响，疯狂往磁盘灌数据的
/// 进程仍会被内核杀掉。
pub const KERNEL_WRITE_BACKSTOP_BYTES: u64 = 64 * 1024 * 1024;

/// 内核侧写盘兜底（粗闸门）的**生效**字节数 —— 取 `max(输出上限, 64 MiB)`。
pub fn kernel_write_backstop_bytes(max_output_bytes: u64) -> u64 {
    max_output_bytes.max(KERNEL_WRITE_BACKSTOP_BYTES)
}

/// 内核侧写盘兜底（粗闸门）换算成 `ulimit -f` 的块数。
///
/// 至少 1 块 —— `ulimit -f 0` 的语义是「不许写任何文件」，与「不许写输出」不同义。
fn kernel_write_backstop_blocks(max_output_bytes: u64) -> u64 {
    kernel_write_backstop_bytes(max_output_bytes)
        .div_ceil(ulimit_block_bytes())
        .max(1)
}

/// `SIGXFSZ` —— 内核的文件大小限额触发时送给进程的信号。
///
/// Linux / macOS / BSD 的信号编号一致（25）⇒ 硬编码即可，不引 `libc` 依赖。
pub const KERNEL_FSIZE_SIGNAL: i32 = 25;

/// 模型可见的后台命令句柄 —— **进程内单调递增、永不复用**。
///
/// 句柄号在**启动成功时**才落账 ⇒ 模型看到的句柄号严格连续且单调递增。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BgHandle(pub u32);

impl BgHandle {
    /// 模型可见的句柄名（`bg1` / `bg2` …）。
    pub fn label(&self) -> String {
        format!("bg{}", self.0)
    }
}

/// 后台命令的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgState {
    /// 仍在跑。
    Running,
    /// 已退出；`None` = 被信号杀死（不是 harness 主动杀的）。
    Exited(Option<i32>),
    /// 被 harness 主动杀掉（会话结束 / abort / 超时 / 输出超限）。
    Killed,
}

/// 结果的**投递状态** —— 与 [`BgState`] 正交（进程状态 vs 通知状态）。
///
/// 用三态而不是「一位两用」：从「取候选（锁内）→ 渲染（锁外）→ 投递（锁外）→
/// 落状态（锁内）」这条链上，两态没有正确的写入时刻 —— 先标记则中途换回合会
/// **静默丢失**，后标记则中途崩溃会**重复投递**。三态把窗口压成
/// 「可能重复、绝不丢失」，这是有意的方向选择（重复对模型可见，丢失不可见）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    /// 还没投出去 —— 每个 tick / 每个回合开头都会重试。
    Pending,
    /// 已取得候选、正在锁外渲染与投递。**只有清扫路径会设它**，
    /// 且必定离开（成功 ⇒ `Delivered`，失败或被中断 ⇒ 回 `Pending`）。
    InFlight,
    /// 已投出，或被**有意丢弃**（abort / `/new`）。此后不再尝试。
    Delivered,
}

/// 一条**已终态**的后台命令 —— 投递的输入，**不含正文**。
///
/// 正文渲染在锁外做（要读输出文件，最多 50 KiB）⇒ 这里只带定位与描述信息。
#[derive(Clone, Debug)]
pub struct BgCompletion {
    pub handle: BgHandle,
    pub state: BgState,
    pub elapsed: Duration,
    pub command: String,
    pub cwd: PathBuf,
    /// 这条命令的输出文件 —— 可能已被 `/new` 或淘汰删掉 ⇒ 渲染方须给「不可读」形态。
    pub output_path: PathBuf,
}

/// 完成正文的渲染回调 —— 由 `bg_output::render_completion` 提供。
///
/// 注册表**不渲染正文**：渲染要读输出文件 ⇒ 必须在锁外做；且正文口径只有一处
/// （`bg_output`），两份实现必然漂移。未装载回调时清扫**不投递**（回 `Pending`），
/// 绝不投空正文 —— 空正文会让模型以为「命令没有输出」。
pub type CompletionRenderer = fn(&BgCompletion) -> Message;

/// 一条待杀条目 —— 锁内「标记 + 交出 `Child`」，锁外再真正杀（[`Doomed::kill`]）。
///
/// 把 `Child` 一并带出锁，是为了让锁外那段时间**没有任何线程**能收尸：
/// 「先杀后收」成立的前提是「杀之前 pid 没被复用」。
struct Doomed {
    handle: BgHandle,
    pid: u32,
    child: Option<Child>,
}

impl Doomed {
    /// 真正动手：杀进程组，再收尸（顺序不可反）。
    ///
    /// 返回 `Some(child)` = 它还没退 ⇒ 必须交回注册表，下一轮再收：
    /// 直接丢掉句柄会把它留成**僵尸**（僵尸仍占 pid，`kill -0` 对它照样成功）。
    fn kill(self) -> Option<(BgHandle, Child)> {
        kill_group(self.pid);
        let mut child = self.child?;
        match child.try_wait() {
            Ok(Some(_)) => None,
            _ => Some((self.handle, child)),
        }
    }
}

/// 后台命令条目 —— **会话级内容**（注册表对象是进程级，条目随会话失效）。
struct BgEntry {
    /// 启动时的会话标识 —— **查询时必须核对**。
    ///
    /// 句柄不复用是第一道防线；这条是第二道：即便某个清理点漏了，
    /// 上个会话的句柄也命中不到本会话的条目（只会得到「未知句柄」）。
    session: String,
    handle: BgHandle,
    pid: u32,
    command: String,
    cwd: PathBuf,
    state: BgState,
    /// 进程被**信号**杀死时的信号号（`None` = 正常退出或尚未收尸）。
    ///
    /// 用来区分「被内核文件大小限额杀掉」（`SIGXFSZ`）与「被别的信号杀掉」。
    signal: Option<i32>,
    /// 结果的投递状态（与 `state` 正交）。
    delivery: Delivery,
    /// 进入 `InFlight` 的时刻 —— 超过 2 个 tick 仍未落状态 ⇒ 那次投递被中断。
    inflight_since: Option<Instant>,
    output_path: PathBuf,
    started_at: Instant,
    /// 本条命令的寿命上限 —— 到点由清扫路径杀进程组。
    timeout: Duration,
    /// 子进程句柄；`None` = 已收尸（不再需要持有）。
    child: Option<Child>,
}

/// 状态快照 —— **不含输出正文**。
#[derive(Clone, Debug)]
pub struct BgSnapshot {
    pub handle: BgHandle,
    pub pid: u32,
    pub command: String,
    pub cwd: PathBuf,
    pub state: BgState,
    /// 进程被**信号**杀死时的信号号（`None` = 正常退出 / 被 harness 杀掉 / 尚未收尸）。
    pub signal: Option<i32>,
    pub elapsed: Duration,
    /// 本条命令的寿命上限 —— 到点由清扫路径杀进程组。
    pub timeout: Duration,
    pub output_path: PathBuf,
    pub output_bytes: u64,
    /// 进程是否**已经收尸**（子进程句柄已丢弃）—— 输出文件**不会再增长**。
    ///
    /// `Exited` 恒为 `true`（退出码就是收尸的结果）；`Killed` 只有在进程真死了
    /// 之后才为 `true`（被信号打断到收尸之间，进程还可能再写几个字节）。
    ///
    /// 交付侧用它判定「末行是否**就是完整行**」：只有进程确实没了，
    /// 末段才可以整段交付（否则运行中的半行会被切掉）。
    pub process_gone: bool,
}

/// 后台命令额度（进程级，构造时冻结）。
#[derive(Clone, Debug)]
pub struct BgLimits {
    /// 并发在跑的后台命令上限 —— 超限**同步报错**（不排队、不静默）。
    pub max_running: usize,
    /// **未投出的完成项**上限 —— 到上限淘汰最旧的一条（连同它的输出文件）。
    ///
    /// 与在跑项**分开计数**：两者共用名额会让「启动构建 → 回合结束 → 下回合再启动」
    /// 这种正常用法把上一回合的结果挤掉（结果被静默丢弃）。
    pub max_entries: usize,
    /// 单命令输出的磁盘上限。
    pub max_output_bytes: u64,
    /// 模型不传寿命时的默认值。
    pub default_timeout: Duration,
    /// 寿命上限（模型传的值超过它时按它算）。
    pub max_timeout: Duration,
}

impl Default for BgLimits {
    fn default() -> Self {
        Self {
            max_running: 4,
            max_entries: 8,
            max_output_bytes: 2 * 1024 * 1024,
            default_timeout: Duration::from_secs(900),
            max_timeout: Duration::from_secs(3600),
        }
    }
}

/// 后台命令的失败原因。
#[derive(Debug)]
pub enum BgError {
    /// 落盘根目录不是绝对路径 —— 相对路径会把产物落进用户的工作区。
    RelativeRoot(PathBuf),
    /// 会话 id 不是单个路径片段 —— 含分隔符会让落盘目录逃出 `<root>/<session>/`。
    InvalidSession(String),
    /// 落盘目录创建 / 输出文件打开失败。
    Io { path: PathBuf, source: io::Error },
    /// 并发在跑的后台命令已达上限。
    TooManyRunning { limit: usize },
    /// 未投出的完成项已达上限，且没有可淘汰的条目（其余都在投递中）。
    TableFull { limit: usize },
    /// 子进程启动失败。
    Spawn { command: String, source: io::Error },
}

impl fmt::Display for BgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BgError::RelativeRoot(path) => write!(
                f,
                "background output root must be an absolute path, got {}",
                path.display()
            ),
            BgError::InvalidSession(session) => {
                write!(f, "invalid session id for background command: {session}")
            }
            BgError::Io { path, source } => {
                write!(f, "background io error at {}: {source}", path.display())
            }
            // 措辞只指向**调用方真能做的动作** —— 模型面没有 kill 入口
            // （`bash_bg` 只能 start、`bash_poll` 只能 poll）⇒ 只能「等一条跑完」。
            BgError::TooManyRunning { limit } => write!(
                f,
                "too many background commands are running ({limit}) — \
                 wait for one to finish, then start another"
            ),
            BgError::TableFull { limit } => write!(
                f,
                "{limit} finished background results are still waiting to be delivered — \
                 wait for them to be delivered, then start another command"
            ),
            BgError::Spawn { command, source } => {
                write!(f, "failed to start command {command}: {source}")
            }
        }
    }
}

impl std::error::Error for BgError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BgError::Io { source, .. } | BgError::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 清扫节奏（sweeper 的 tick）—— **`InFlight` 的超时基准**：
///
/// 投递是同步短操作（读 ≤ 50 KiB + 一次 `push`）⇒ 200ms 走完绰绰有余；
/// 超过就说明那次投递被中断（回合切换 / panic）⇒ 回 `Pending` 重投。
const SWEEP_TICK: Duration = Duration::from_millis(100);

/// `InFlight` 超过这么多个 tick 仍未被落状态 ⇒ 视为那次投递被中断。
const INFLIGHT_TICKS: u32 = 2;

/// 注册表内部状态 —— 全部内容都在一把锁内。
struct Inner {
    /// 下一个句柄号（**只增不减** ⇒ 句柄永不复用）。
    next_handle: u32,
    /// 条目表 —— 按启动顺序排列 ⇒ 「最旧的已完成」= 第一个非 `Running` 的条目。
    entries: Vec<BgEntry>,
}

/// 当前会话 id 的**共享槽**。
///
/// 工具表是**建表期产物**（进程级冻结）⇒ 会话 id 不能冻在工具里：
/// `/new`、`/resume` 之后当前会话会变 ⇒ 只能运行时读这个槽。
/// 槽是**进程级**的（`Arc` 共享）⇒ 一次切换对所有持有者同时可见。
#[derive(Clone, Debug)]
pub struct SessionSlot {
    inner: Arc<RwLock<String>>,
}

impl SessionSlot {
    /// 构造槽 —— 初值即当前会话 id。
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(session.into())),
        }
    }

    /// 当前会话 id。
    pub fn get(&self) -> String {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 切换当前会话 —— `/new`、`/resume` 之后由 app 层调用。
    pub fn set(&self, session: impl Into<String>) {
        *self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = session.into();
    }
}

/// 把「本回合的投递落点」绑定到注册表。
///
/// 工具表是**进程级**的，而真实边界源**每回合新建** ⇒ 启动那刻记住边界源毫无意义
/// （回合 5 起的命令可能回合 7 才结束，那时的边界源早已销毁）。所以落点存在注册表上，
/// 由 app 层每回合绑一次，**回合结束即解绑**。
///
/// 解绑走 [`Drop`]（RAII）：app 层有多处 `?` 提前返回，手动 unbind 必漏。
/// 解绑只断开「后续投递的落点」，**不改变**已投出条目的状态。
#[must_use = "守卫 drop 即解绑 ⇒ 立即丢弃等于什么都没绑"]
pub struct DeferredBinding {
    tasks: Arc<BackgroundTasks>,
    /// 绑定时那格的代数 —— `unbind` 只在代数仍相同时清空，
    /// 避免晚 drop 的旧守卫把新回合刚绑的落点抹掉。
    generation: u64,
}

impl Drop for DeferredBinding {
    fn drop(&mut self) {
        self.tasks.unbind(self.generation);
    }
}

/// 后台命令注册表 —— **进程级**；内容是**会话级**。
pub struct BackgroundTasks {
    /// 落盘根目录 —— **必须是绝对路径**。
    root: PathBuf,
    limits: BgLimits,
    inner: Mutex<Inner>,
    /// 本回合的投递落点（`None` = 未绑定 ⇒ 完成项留表等下回合补投）。
    deferred: Mutex<SinkSlot>,
    /// 完成正文的渲染回调（未装载 ⇒ 清扫不投递，回 `Pending`）。
    renderer: Mutex<Option<CompletionRenderer>>,
}

/// 投递落点格。
struct SinkSlot {
    sink: Option<Arc<QueueBoundarySource>>,
    /// 绑定代数 —— 区分「同一回合重复绑定」与「跨回合换绑」。
    generation: u64,
}

impl BackgroundTasks {
    /// 构造注册表 —— 额度在此**冻结**。
    pub fn new(root: PathBuf, limits: BgLimits) -> Self {
        Self {
            root,
            limits,
            inner: Mutex::new(Inner {
                next_handle: 1,
                entries: Vec::new(),
            }),
            deferred: Mutex::new(SinkSlot {
                sink: None,
                generation: 0,
            }),
            renderer: Mutex::new(None),
        }
    }

    /// 装载完成正文的渲染回调 —— 只需装一次（进程级）。
    ///
    /// 未装载时清扫**不投递**：宁可把结果留在表里等下回合，也不投空正文
    /// （空正文会让模型以为「命令没有输出」，是静默的谎）。
    pub fn set_completion_renderer(&self, renderer: CompletionRenderer) {
        *self
            .renderer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(renderer);
    }

    /// 绑定本回合的投递完成正文的落点 —— 守卫 drop 时解绑。
    ///
    /// 后一次绑定替换前一次（同一回合重复绑定会换到新队列，最后一个生效）。
    pub fn bind_deferred(self: &Arc<Self>, sink: Arc<QueueBoundarySource>) -> DeferredBinding {
        let generation = {
            let mut slot = self
                .deferred
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.generation = slot.generation.wrapping_add(1);
            slot.sink = Some(sink);
            slot.generation
        };
        DeferredBinding {
            tasks: Arc::clone(self),
            generation,
        }
    }

    /// 解绑 —— 只在代数仍匹配时生效（晚 drop 的旧守卫不得抹掉新回合的落点）。
    fn unbind(&self, generation: u64) {
        let mut slot = self
            .deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.generation == generation {
            slot.sink = None;
        }
    }

    /// 当前落点（`None` = 未绑定）。
    fn sink(&self) -> Option<Arc<QueueBoundarySource>> {
        self.deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sink
            .clone()
    }

    /// 按约定解析落盘根目录：`$YUSHAN_BG_DIR`（须绝对路径）或 `<temp_dir>/yushan-bg`。
    ///
    /// 不用相对路径 —— coding agent 的工作目录就是用户的仓库，
    /// 相对路径会把产物落进用户的 git 工作区。
    pub fn default_root() -> PathBuf {
        match std::env::var(BG_DIR_ENV) {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
            _ => std::env::temp_dir().join("yushan-bg"),
        }
    }

    /// 落盘根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 额度（构造时冻结）—— 调用方据此判断「输出是否已到上限」等。
    pub fn limits(&self) -> &BgLimits {
        &self.limits
    }

    /// 内核侧写盘兜底（粗闸门）的**生效**字节数 —— 供归因措辞报出具体数值。
    pub fn kernel_write_backstop_bytes(&self) -> u64 {
        kernel_write_backstop_bytes(self.limits.max_output_bytes)
    }

    /// 条目数（在跑 + 已完成未取走）。
    pub fn entry_count(&self) -> usize {
        self.lock().entries.len()
    }

    /// 某会话里**仍在跑**的句柄（按启动顺序）—— 供「未知句柄」错误列出候选。
    pub fn running_handles(&self, session: &str) -> Vec<BgHandle> {
        let inner = self.lock();
        inner
            .entries
            .iter()
            .filter(|entry| entry.session == session && entry.state == BgState::Running)
            .map(|entry| entry.handle)
            .collect()
    }

    /// 启动一条后台命令并**立刻**返回句柄；失败**同步返回 `Err`**。
    ///
    /// 失败**不产生条目** —— 注册表里不会出现「永远就绪」的句柄。
    /// 句柄号在**启动成功时**才落账 ⇒ 模型看到的句柄号严格连续且单调递增。
    pub fn start(
        &self,
        session: &str,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<BgHandle, BgError> {
        if !self.root.is_absolute() {
            return Err(BgError::RelativeRoot(self.root.clone()));
        }
        if !is_safe_session(session) {
            return Err(BgError::InvalidSession(session.to_string()));
        }
        let timeout = timeout
            .unwrap_or(self.limits.default_timeout)
            .min(self.limits.max_timeout);

        // 整段在锁内完成 ⇒ 句柄分配、在跑计数、条目淘汰不会与并发 `start` 竞态。
        // 但**杀进程**不在这段里：上一轮的到期 / 超限条目先在锁外处理完。
        self.mark_and_kill_expired();
        let mut inner = self.lock();

        let running = inner
            .entries
            .iter()
            .filter(|entry| entry.state == BgState::Running)
            .count();
        if running >= self.limits.max_running {
            return Err(BgError::TooManyRunning {
                limit: self.limits.max_running,
            });
        }

        let session_dir = self.root.join(session);
        fs::create_dir_all(&session_dir).map_err(|source| BgError::Io {
            path: session_dir.clone(),
            source,
        })?;

        // 未投出的完成项到上限 ⇒ 淘汰**最旧的一条**（删文件）。
        // 与在跑项**分开计数**：在跑项只受 `max_running` 约束。
        while Self::pending_completions(&inner) >= self.limits.max_entries {
            if !Self::evict_oldest_completed(&mut inner) {
                return Err(BgError::TableFull {
                    limit: self.limits.max_entries,
                });
            }
        }

        let handle = BgHandle(inner.next_handle);
        let output_path = session_dir.join(format!("bg{}.log", handle.0));

        let file = File::create(&output_path).map_err(|source| BgError::Io {
            path: output_path.clone(),
            source,
        })?;
        let stderr_file = file.try_clone().map_err(|source| BgError::Io {
            path: output_path.clone(),
            source,
        })?;

        // 启动命令时把**写盘兜底**交给内核（`ulimit -f`）——
        // 它是**粗闸门**：只兜住「进程往任何文件无限灌数据」这种失控写盘，
        // 不表达输出上限（输出上限由 `enforce_output_cap` 的 stat 闸门精确执行）。
        // 限额与用户命令都经**位置参数**传入 ⇒ 不拼进脚本体 ⇒ 引号不被二次解析。
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(FILE_LIMIT_SCRIPT)
            .arg("sh")
            .arg(kernel_write_backstop_blocks(self.limits.max_output_bytes).to_string())
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(stderr_file));
        // 进程组 id == 子进程 pid ⇒ 超时/中止时能连孙进程一起杀掉。
        #[cfg(unix)]
        cmd.process_group(0);

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(source) => {
                let _ = fs::remove_file(&output_path);
                return Err(BgError::Spawn {
                    command: command.to_string(),
                    source,
                });
            }
        };
        let pid = child.id();

        // 句柄号在启动成功后才落账 ⇒ 失败的启动不消耗句柄号。
        inner.next_handle = inner.next_handle.saturating_add(1);
        inner.entries.push(BgEntry {
            session: session.to_string(),
            handle,
            pid,
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            state: BgState::Running,
            signal: None,
            delivery: Delivery::Pending,
            inflight_since: None,
            output_path,
            started_at: Instant::now(),
            timeout,
            child: Some(child),
        });
        Ok(handle)
    }

    /// 状态快照（顺带执行输出上限闸门 + 回收已退出的子进程）。
    ///
    /// 输出上限闸门在这里执行（不只在 `sweep` 里）⇒ 每次查状态都是一次闸门检查
    /// ⇒ 「查状态」这条路径上的超限命令会被立刻杀掉，不靠后台清扫任务的节奏。
    ///
    /// `session` 不匹配 ⇒ 返回 `None`（对模型表现为「未知句柄」）。
    pub fn snapshot(&self, handle: BgHandle, session: &str) -> Option<BgSnapshot> {
        let (snapshot, to_kill) = {
            let mut inner = self.lock();
            // 闸门**先标记后收**：顺序不可颠倒（收尸 ⇒ pid 可被复用 ⇒ `kill -9 -<pid>`
            // 会打到无关的进程组）。
            let mut to_kill = self.enforce_output_cap(&mut inner);
            to_kill.extend(self.reap(&mut inner));
            let snapshot = inner
                .entries
                .iter()
                .find(|entry| entry.handle == handle && entry.session == session)
                .map(|entry| BgSnapshot {
                    handle: entry.handle,
                    pid: entry.pid,
                    command: entry.command.clone(),
                    cwd: entry.cwd.clone(),
                    state: entry.state,
                    signal: entry.signal,
                    elapsed: entry.started_at.elapsed(),
                    timeout: entry.timeout,
                    output_path: entry.output_path.clone(),
                    output_bytes: output_bytes(&entry.output_path),
                    // 子进程句柄已丢弃 = 已收尸 = 进程真没了 ⇒ 输出文件不会再增长。
                    process_gone: entry.child.is_none(),
                });
            (snapshot, to_kill)
        };
        // 待杀条目已在锁内标记 `Killed` 并交出 `Child` ⇒ 锁外杀死它。
        self.kill_outside(to_kill);
        snapshot
    }

    /// 清扫：回收子进程 / 标记超限项 / 认领并投递完成项 / 淘汰超额完成项。
    ///
    /// **三段式（P0-1）**：
    ///
    /// | 段 | 锁 | 做什么 |
    /// |---|---|---|
    /// | 1 | 持锁 | tick 修复、收尸（`try_wait`）、标记输出超限、认领候选（`Pending → InFlight`）、收集待杀 pid |
    /// | 2 | **无锁** | `kill_group`（fork/exec/join）、渲染正文（读输出文件）、`push` 入边界源 |
    /// | 3 | 持锁 | 落 `Delivered` / 回 `Pending`，并淘汰超额完成项 |
    ///
    /// **不变量：段 1 / 段 3 绝不 fork / exec / 读输出文件（`try_wait` 与 `stat` 除外）。**
    /// 违反它的代价不是「清扫变慢」，而是**所有工具调用一起卡** —— 同一把锁是
    /// `start` / `snapshot` / `take_completions` 的必经之路。
    pub fn sweep(&self) {
        // ── 段 1（持锁）：状态推进 + 认领，**不做 IO、不 fork** ──
        let (to_kill, claimed) = {
            let mut inner = self.lock();
            Self::repair_inflight(&mut inner);
            // 输出上限闸门**先标记后收尸**，顺序不可颠倒
            // （收尸 ⇒ pid 可被复用 ⇒ `kill -9 -<pid>` 会打到无关的进程组）。
            let mut to_kill = self.enforce_output_cap(&mut inner);
            to_kill.extend(self.reap(&mut inner));
            // 认领候选 —— **任何会话**的（清扫是进程级的）：
            // 未绑定落点 / 不属当前会话的结果会停回 `Pending` 等下一回合补投。
            let claimed = Self::claim_completions(&mut inner, None);
            (to_kill, claimed)
        };

        // ── 段 2（无锁）：重活 ──
        self.kill_outside(to_kill);
        self.deliver(claimed);

        // ── 段 3（持锁）：淘汰超额的完成项（只碰 `Pending` / `Delivered`）──
        let mut inner = self.lock();
        while Self::pending_completions(&inner) > self.limits.max_entries {
            if !Self::evict_oldest_completed(&mut inner) {
                break;
            }
        }
    }

    /// 取「已终态且 `Pending`」的完成项（**当前回合**）—— 内部置 `InFlight`。
    ///
    /// 回合开头的补投入口：调用方渲染 + `push` 后调 `finish_delivery`；
    /// 投不出去则调 [`Self::requeue`] 退回 `Pending`（下回合重试）。
    ///
    /// 条目取出后仍留在表里（`InFlight`）⇒ 同一 tick 内不会被重复取出。
    pub fn take_completions(&self, session: &str) -> Vec<BgCompletion> {
        let mut inner = self.lock();
        Self::claim_completions(&mut inner, Some(session))
    }

    /// 落投递结果：成功 ⇒ `Delivered`（此后不再尝试）；失败 ⇒ 回 `Pending`。
    ///
    /// 只对仍处于 `InFlight` 的条目生效 —— 期间被 abort / `/new` 改成 `Delivered`
    /// 或已被淘汰的条目，不得因迟到的一次调用又回到可投递状态。
    pub fn finish_delivery(&self, handle: BgHandle, delivered: bool) {
        let mut inner = self.lock();
        let Some(entry) = inner
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle)
        else {
            return;
        };
        if entry.delivery != Delivery::InFlight {
            return;
        }
        entry.delivery = if delivered {
            Delivery::Delivered
        } else {
            Delivery::Pending
        };
        entry.inflight_since = None;
    }

    /// 投递失败时把 `InFlight` 退回 `Pending`（下回合重试）。
    pub fn requeue(&self, handle: BgHandle) {
        self.finish_delivery(handle, false);
    }

    /// 超时过继：登记一只**已经启动**的子进程，由同一 sweeper 检测退出并投递结果。
    ///
    /// `child` 必须来自 `tokio::process::Command`（`kill_on_drop(true)` ⇒ 被丢弃时
    /// 内核仍会杀掉它）。这里只拿它当 `Child` 用：`try_wait` 能检测退出并收尸，
    /// 不依赖 tokio 的 reaper。
    /// 登记成功后它与 `start` 启动的条目**完全同构** —— 同样受 `max_running`
    /// 名额约束、同样受寿命上限与输出上限约束、同样在完成时投递结果。
    ///
    /// **登记必定成功**（不因名额 / 路径问题拒绝）：过继的进程**已经在跑**，
    /// 拒绝登记只会让它成为没人管的孤儿 —— 连调用方的清理点都碰不到它。
    /// 代价是「在跑数量」可能短暂超过 `max_running`（最多被后一条启动的报错拒绝）。
    ///
    /// 输出文件在**第一次补录时**才建 —— 过继的进程写的是它自己的管道（由 `bash` 排空），
    /// 调用方用 [`Self::append_output`] 把管道里已读到的那段补进来。
    pub fn adopt(&self, child: Child, session: &str, command: String, cwd: PathBuf) -> BgHandle {
        // 登记前先把上一轮的到期 / 超限条目处理掉 —— 这一步要 fork，故在锁外。
        self.mark_and_kill_expired();
        // 建会话目录也在锁外（mkdir 是文件系统调用）。
        let session_dir = self.root.join(session);
        let _ = fs::create_dir_all(&session_dir);

        let mut inner = self.lock();
        let handle = BgHandle(inner.next_handle);
        let pid = child.id();
        let output_path = session_dir.join(format!("bg{}.log", handle.0));

        inner.next_handle = inner.next_handle.saturating_add(1);
        inner.entries.push(BgEntry {
            session: session.to_string(),
            handle,
            pid,
            command,
            cwd,
            state: BgState::Running,
            signal: None,
            delivery: Delivery::Pending,
            inflight_since: None,
            output_path,
            started_at: Instant::now(),
            timeout: self.limits.default_timeout,
            child: Some(child),
        });
        handle
    }

    /// 过继一个**已在跑**的进程 —— 只登记 `pid`，不持有 `Child`。
    ///
    /// `bash` 超时转后台走这条：tokio 的 `Child` 无法转成 `std::process::Child`
    /// （`tokio 1.53` 只提供 `Command::into_std`），故收尸由调用方的 watcher 负责，
    /// 完成后调 [`Self::mark_finished`] 推终态。
    ///
    /// **登记必定成功**：进程已经在跑，拒绝登记只会让它成为没人管的孤儿。
    pub fn adopt_pid(&self, pid: u32, session: &str, command: String, cwd: PathBuf) -> BgHandle {
        self.mark_and_kill_expired();
        let session_dir = self.root.join(session);
        let _ = fs::create_dir_all(&session_dir);

        let mut inner = self.lock();
        let handle = BgHandle(inner.next_handle);
        let output_path = session_dir.join(format!("bg{}.log", handle.0));

        inner.next_handle = inner.next_handle.saturating_add(1);
        inner.entries.push(BgEntry {
            session: session.to_string(),
            handle,
            pid,
            command,
            cwd,
            state: BgState::Running,
            signal: None,
            delivery: Delivery::Pending,
            inflight_since: None,
            output_path,
            started_at: Instant::now(),
            timeout: self.limits.default_timeout,
            // 没有 `Child` ⇒ `sweep` 观察不到退出，靠 watcher 的 `mark_finished`。
            child: None,
        });
        handle
    }

    /// 把一条仍在跑的条目推入**终态** —— 供过继侧的 watcher 调。
    ///
    /// 与 `sweep` 的 `reap` 互补：那条路径靠 `try_wait` 观察，这条靠 watcher 主动告知。
    /// 已终态的条目不重复改写（可能已被 `Killed`）。
    pub fn mark_finished(&self, handle: BgHandle, exit_code: Option<i32>) {
        let mut inner = self.lock();
        if let Some(entry) = inner.entries.iter_mut().find(|e| e.handle == handle)
            && entry.state == BgState::Running
        {
            entry.state = BgState::Exited(exit_code);
        }
    }

    /// 本条目的输出文件路径（`None` = 句柄不存在）。
    pub fn output_path_of(&self, handle: BgHandle) -> Option<PathBuf> {
        let inner = self.lock();
        inner
            .entries
            .iter()
            .find(|e| e.handle == handle)
            .map(|e| e.output_path.clone())
    }

    /// 输出正文落盘：把一段文本**追加**进 `handle` 的输出文件。
    ///
    /// [`Self::adopt`] 过继的进程写的是它自己的管道，注册表里没有那条管道 ——
    /// 调用方用这个口子把「已经读到的那段」补进输出文件，投递时才能读到完整行。
    ///
    /// 返回 `false` = 句柄不存在（已被淘汰 / 从未登记）。
    /// **不在锁内写文件**：写入可能慢（磁盘），走同一把锁会拖慢所有工具调用。
    pub fn append_output(&self, handle: BgHandle, text: &str) -> bool {
        use std::io::Write;

        let path = {
            let inner = self.lock();
            match inner.entries.iter().find(|entry| entry.handle == handle) {
                Some(entry) => entry.output_path.clone(),
                None => return false,
            }
        };
        // 文件打不开（已被淘汰删掉）也就不必追加 —— 但不报错：追加是尽力而为。
        // `create` 是必要的：过继的条目在登记时还没有输出文件（它的输出走自己的管道）。
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(text.as_bytes());
        }
        true
    }

    /// 杀掉一个后台命令的**整个进程组**（**按会话过滤**）。
    ///
    /// 返回 `false` = 句柄不存在或不属于该会话（对模型表现为「未知句柄」）。
    /// **先杀后收**：先杀进程组，再收尸（告诫见 [`crate::proc_group::kill_group`]）。
    pub fn kill(&self, handle: BgHandle, session: &str) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle && entry.session == session)
        else {
            return false;
        };
        Self::kill_entry(entry);
        true
    }

    /// 杀掉**某会话**的全部（abort / `/new` / `/resume` / 退出）。
    ///
    /// **按会话过滤**（不是全局清空）⇒ 漏掉的清理点不会波及别的会话。
    pub fn kill_all(&self, session: &str) {
        let mut inner = self.lock();
        for entry in inner
            .entries
            .iter_mut()
            .filter(|entry| entry.session == session)
        {
            Self::kill_entry(entry);
        }
    }

    /// 删除某会话的落盘目录（`<root>/<session>/`）—— 整体删除。
    ///
    /// `/new` 时旧会话整体作废 ⇒ 它的输出目录一并删掉（否则磁盘上留着一整个会话的
    /// 产物，而那些进程已经被杀掉，文件永远不会再被写）。
    ///
    /// # 路径穿越防护
    ///
    /// 只允许删 `<root>` 的**直接子目录**：
    ///
    /// - 会话 id 为空、`.`、`..` 或含 `/`、`\` ⇒ **一律拒绝**（`InvalidInput`）；
    /// - 拼出的路径的父目录必须**就是** `<root>`（二次核对，不依赖上一步的判断）。
    ///
    /// 目录不存在时返回 `Ok(())`（幂等）—— 清理点不该因「已经删过了」而报错。
    pub fn remove_session_dir(&self, session: &str) -> std::io::Result<()> {
        if !is_safe_session(session) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("不安全的会话 id：{session}"),
            ));
        }
        let dir = self.root.join(session);
        // 二次防护：只允许删 `<root>` 的直接子目录。
        if dir.parent() != Some(self.root.as_path()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("会话目录不在落盘根之下：{session}"),
            ));
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 执行一批已在锁内标记的待杀条目（**必须在锁外调**）。
    ///
    /// 先杀进程组，再收尸 —— 顺序不可反：收尸 ⇒ pid 可被复用 ⇒ `kill -9 -<pid>`
    /// 会打到无关的进程组。锁内已把 `Child` 从条目里取走 ⇒ 这段时间没有别的线程
    /// 能收这个尸，这条不变量才站得住。
    ///
    /// 没退出的子进程交回注册表（**这一处重取锁是收尸，不是 fork**）。
    fn kill_outside(&self, doomed: Vec<Doomed>) {
        let leftovers: Vec<(BgHandle, Child)> =
            doomed.into_iter().filter_map(Doomed::kill).collect();
        if leftovers.is_empty() {
            return;
        }
        let mut inner = self.lock();
        for (handle, child) in leftovers {
            if let Some(entry) = inner
                .entries
                .iter_mut()
                .find(|entry| entry.handle == handle)
                && entry.child.is_none()
            {
                entry.child = Some(child);
            }
        }
    }

    /// 「到期 / 输出超限 ⇒ 标记 + 锁外杀」的独立一遍 —— 供 `start` / `adopt` 在取锁前调。
    ///
    /// 独立成一遍是为了让登记路径的锁内**只做登记**：锁内 fork 会把一条过期命令的
    /// 代价摊到所有走同一把锁的调用上。
    fn mark_and_kill_expired(&self) {
        let doomed = {
            let mut inner = self.lock();
            let mut doomed = self.enforce_output_cap(&mut inner);
            doomed.extend(self.reap(&mut inner));
            doomed
        };
        self.kill_outside(doomed);
    }
    /// 取候选并认领：已终态 + `Pending` ⇒ 置 `InFlight`。
    ///
    /// `session` 为 `None` 取所有会话（清扫是进程级的），`Some` 只取该会话（补投）。
    /// **不取 `InFlight`**：重复认领会把一个结果投两次。
    fn claim_completions(inner: &mut Inner, session: Option<&str>) -> Vec<BgCompletion> {
        let now = Instant::now();
        let mut claimed = Vec::new();
        for entry in inner.entries.iter_mut() {
            if entry.state == BgState::Running || entry.delivery != Delivery::Pending {
                continue;
            }
            if session.is_some_and(|session| entry.session != session) {
                continue;
            }
            entry.delivery = Delivery::InFlight;
            entry.inflight_since = Some(now);
            claimed.push(BgCompletion {
                handle: entry.handle,
                state: entry.state,
                elapsed: entry.started_at.elapsed(),
                command: entry.command.clone(),
                cwd: entry.cwd.clone(),
                output_path: entry.output_path.clone(),
            });
        }
        claimed
    }

    /// tick 修复：`InFlight` 超时 ⇒ 回 `Pending` 重投。
    ///
    /// 投递是同步短操作（读 ≤ 50 KiB + 一次 `push`）⇒ 超过 2 个 tick 仍未落状态，
    /// 只能是一次被中断的投递（回合切换 / panic）。这里选**宁可重复，绝不丢失**：
    /// 丢失对模型不可见（它以为命令还没结束），重复至少看得见。
    fn repair_inflight(inner: &mut Inner) {
        let deadline = SWEEP_TICK * INFLIGHT_TICKS;
        let now = Instant::now();
        for entry in inner.entries.iter_mut() {
            if entry.delivery != Delivery::InFlight {
                continue;
            }
            let expired = entry
                .inflight_since
                .is_none_or(|since| now.duration_since(since) > deadline);
            if expired {
                entry.delivery = Delivery::Pending;
                entry.inflight_since = None;
            }
        }
    }

    /// 渲染 + 投递一批已认领的完成项，并**逐条**落状态。
    ///
    /// 无渲染回调 / 无落点 ⇒ 回 `Pending`（下回合补投）：绝不投空正文，也绝不
    /// 把条目留在 `InFlight` 卡死。
    fn deliver(&self, claimed: Vec<BgCompletion>) {
        for candidate in claimed {
            let delivered = match (self.renderer(), self.sink()) {
                (Some(render), Some(sink)) => {
                    sink.push(Boundary::Deferred {
                        handle: candidate.handle.label(),
                        message: render(&candidate),
                    });
                    true
                }
                _ => false,
            };
            self.finish_delivery(candidate.handle, delivered);
        }
    }
    /// 完成正文的渲染回调（`None` = 未装载）。
    fn renderer(&self) -> Option<CompletionRenderer> {
        *self
            .renderer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 杀掉一条条目的进程组并收尸（**先杀后收**）。
    fn kill_entry(entry: &mut BgEntry) {
        // 已在终态的条目不动 —— 正常退出的命令不该被改写成「被 harness 杀掉」。
        if entry.state == BgState::Running {
            kill_group(entry.pid);
            entry.state = BgState::Killed;
        }
        // **有意丢弃**：被 harness 杀掉的命令不投递（模型已从 abort 得知结果）。
        entry.delivery = Delivery::Delivered;
        entry.inflight_since = None;
        // 收尸：被杀的进程可能还没退出 ⇒ 这轮拿不到就留着句柄，下轮再收。
        if let Some(mut child) = entry.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => entry.child = Some(child),
            }
        }
    }

    /// 输出上限闸门（**精确闸门**）：输出文件到上限 ⇒ 标记 `Killed` 并**交出 `Child`**，
    /// 由调用方在**锁外**杀进程组。
    ///
    /// 精确表达「输出文件不得超过 `max_output_bytes`」的**只有这一处** ——
    /// 内核侧的 `ulimit -f` 是**粗闸门**（进程级、被子孙继承）⇒ 拦不住
    /// 「输出文件超限但命令没在写别的大文件」这一形态。
    /// 已经退出的子进程不再杀（免得把正常结束误报成「被 harness 杀掉」）。
    fn enforce_output_cap(&self, inner: &mut Inner) -> Vec<Doomed> {
        if self.limits.max_output_bytes == 0 {
            return Vec::new();
        }
        let mut doomed = Vec::new();
        for entry in inner.entries.iter_mut() {
            if entry.state != BgState::Running {
                continue;
            }
            if output_bytes(&entry.output_path) < self.limits.max_output_bytes {
                continue;
            }
            // leader 已经退出 ⇒ 它的 pid 可能已被复用 ⇒ 此时杀进程组会打到无关的组。
            if let Some(child) = entry.child.as_mut()
                && matches!(child.try_wait(), Ok(Some(_)))
            {
                continue;
            }
            entry.state = BgState::Killed;
            doomed.push(Doomed {
                handle: entry.handle,
                pid: entry.pid,
                child: entry.child.take(),
            });
        }
        doomed
    }

    /// 把子进程状态推进到终态：到寿命上限 ⇒ 标记 `Killed` 并**交出 `Child`**（锁外杀）；
    /// 已退出的 ⇒ 收尸并落 `Exited`。
    ///
    /// **先杀后收**的顺序不可反：反了可能打到已被复用的 pid。
    fn reap(&self, inner: &mut Inner) -> Vec<Doomed> {
        let mut doomed = Vec::new();
        for entry in inner.entries.iter_mut() {
            if entry.state == BgState::Running && entry.started_at.elapsed() >= entry.timeout {
                entry.state = BgState::Killed;
                doomed.push(Doomed {
                    handle: entry.handle,
                    pid: entry.pid,
                    child: entry.child.take(),
                });
                continue;
            }
            if !matches!(entry.state, BgState::Running | BgState::Killed) {
                continue;
            }
            let Some(mut child) = entry.child.take() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    if entry.state == BgState::Running {
                        // 信号号只在「自己跑完 / 自己被信号打死」时才有意义 ——
                        // 被 harness 杀的条目归因是 harness，不看信号。
                        entry.signal = exit_signal(&status);
                        entry.state = match status.code() {
                            Some(code) => BgState::Exited(Some(code)),
                            None => BgState::Exited(None),
                        };
                    }
                    // 已收尸 ⇒ 子进程句柄丢弃（`child` 保持 `None`）。
                }
                Ok(None) => entry.child = Some(child),
                Err(_) => entry.child = Some(child),
            }
        }
        doomed
    }

    /// 未投出完成项的数量 —— `Pending` 与 `InFlight` 都算（它们都占着结果）。
    ///
    /// `Delivered` 不算：投出去的条目只等淘汰，不该拿来跟「等着结果的新命令」抢名额。
    fn pending_completions(inner: &Inner) -> usize {
        inner
            .entries
            .iter()
            .filter(|entry| {
                entry.state != BgState::Running && entry.delivery != Delivery::Delivered
            })
            .count()
    }

    /// 淘汰**最旧的已完成**条目（按启动顺序）并删除其落盘文件。
    ///
    /// 只淘汰 `Pending` / `Delivered` 的完成项 —— `InFlight` 正在锁外投递，
    /// 此时把它删掉会让那次投递把正文写进一个已被清空的名字。
    ///
    /// 返回 `false` = 没有可淘汰的条目（都在跑，或都在投递中）。
    fn evict_oldest_completed(inner: &mut Inner) -> bool {
        let Some(index) = inner.entries.iter().position(|entry| {
            entry.state != BgState::Running && entry.delivery != Delivery::InFlight
        }) else {
            return false;
        };
        let entry = inner.entries.remove(index);
        // 淘汰前再收一次尸 —— 避免留下僵尸（被杀的进程可能还没退）。
        if let Some(mut child) = entry.child {
            let _ = child.try_wait();
        }
        let _ = fs::remove_file(&entry.output_path);
        true
    }

    /// 取锁 —— 锁中毒时取回内部值继续：中毒只说明某次调用 panic 过，
    /// 不改变条目数据本身的结构。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// panic / unwind 兜底：杀掉**全部**后台进程组。
///
/// 只能用**同步** kill（`Drop` 里不能 await）⇒ 复用
/// [`crate::proc_group::kill_group`] 的同步内核。
///
/// 进程退出路径（`std::process::exit`）**不跑析构** ⇒ 那条路径由 app 层的
/// 显式清理点负责（见设计 §4.5 的六个清理点）。
impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        let mut inner = self.lock();
        for entry in inner.entries.iter_mut() {
            Self::kill_entry(entry);
        }
    }
}

/// 会话 id 必须是**单个路径片段** —— 含分隔符会让落盘目录逃出 `<root>/<session>/`。
fn is_safe_session(session: &str) -> bool {
    !session.is_empty()
        && session != "."
        && session != ".."
        && !session.contains('/')
        && !session.contains('\\')
}

/// 落盘文件当前的字节数 —— 文件不存在时按 0 算。
fn output_bytes(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// 进程被**信号**杀死时的信号号 —— 非 unix 平台恒为 `None`（没有信号编号这回事）。
#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// 杀整个进程组 —— 与 `bash` / `exec` 共用同一份内核（本 crate 内只有这一份实现）。
#[cfg(unix)]
fn kill_group(pid: u32) {
    crate::proc_group::kill_group(pid);
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    use ys_core::{ContentBlock, Role};
    use ys_protocol::BoundarySource;

    use crate::test_support::test_dir;

    /// 条目表上限可调的额度 —— 其余字段取默认值。
    fn limits(max_entries: usize) -> BgLimits {
        BgLimits {
            max_entries,
            ..BgLimits::default()
        }
    }

    /// 等一条目离开 `Running`（最多 2s）—— 子进程退出有竞态，不能假设立即可见。
    fn wait_done(tasks: &BackgroundTasks, handle: BgHandle, session: &str) -> BgState {
        for _ in 0..200 {
            tasks.sweep();
            if let Some(snapshot) = tasks.snapshot(handle, session)
                && snapshot.state != BgState::Running
            {
                return snapshot.state;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("等待条目 {:?} 结束超时", handle);
    }

    /// 句柄号**单调递增、永不复用**：连开两条 ⇒ `bg1` / `bg2`；淘汰后下一条是 `bg3`。
    ///
    /// 变异：把句柄号落账改成固定值 ⇒ 第三条断言变红。
    #[test]
    fn handles_are_monotonic_and_never_reused() {
        let dir = test_dir("bg_handles");
        let tasks = BackgroundTasks::new(dir.clone(), limits(1));
        let cwd = std::env::temp_dir();

        let first = tasks.start("s", "true", &cwd, None).unwrap();
        wait_done(&tasks, first, "s");
        let second = tasks.start("s", "true", &cwd, None).unwrap();
        assert_eq!(first, BgHandle(1));
        assert_eq!(second, BgHandle(2));
        assert_eq!(first.label(), "bg1");
        assert_eq!(second.label(), "bg2");

        // 未投出的完成项到上限（1 条）⇒ 下一条必须淘汰**最旧的已完成**条目。
        wait_done(&tasks, second, "s");
        let third = tasks.start("s", "true", &cwd, None).unwrap();
        assert_eq!(third, BgHandle(3), "句柄号不得因淘汰而回退");
        assert!(
            tasks.snapshot(first, "s").is_none(),
            "被淘汰的条目应从注册表消失"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **会话核对**：用会话 A 的句柄去问会话 B ⇒ 未知句柄。
    ///
    /// 变异：查询时去掉会话条件 ⇒ 第二条断言变红。
    #[test]
    fn snapshot_rejects_a_mismatched_session() {
        let dir = test_dir("bg_session");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        let handle = tasks.start("session-a", "true", &cwd, None).unwrap();
        assert!(tasks.snapshot(handle, "session-a").is_some());
        assert!(
            tasks.snapshot(handle, "session-b").is_none(),
            "会话不匹配 ⇒ 命中不到条目（对模型表现为未知句柄）"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **启动失败不产生条目**：命令的工作目录不存在 ⇒ `start` 返回 `Err`，条目数不变。
    ///
    /// 变异：失败分支留下条目 ⇒ 第三条断言变红。
    #[test]
    fn a_failed_start_leaves_no_entry() {
        let dir = test_dir("bg_failed_start");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        tasks.start("s", "true", &cwd, None).unwrap();
        let before = tasks.entry_count();

        let missing = dir.join("no-such-directory");
        let result = tasks.start("s", "true", &missing, None);
        assert_eq!(tasks.entry_count(), before, "启动失败不得留下条目");
        let error = result.expect_err("工作目录不存在 ⇒ 启动失败");
        assert!(matches!(error, BgError::Spawn { .. }), "got {error:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 输出文件落在 `<bg_root>/<session>/bg<N>.log`。
    #[test]
    fn output_file_lands_in_the_session_directory() {
        let dir = test_dir("bg_output_path");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        let handle = tasks.start("sess-1", "echo hello-bg", &cwd, None).unwrap();
        let state = wait_done(&tasks, handle, "sess-1");
        assert_eq!(state, BgState::Exited(Some(0)));

        let expected = dir.join("sess-1").join("bg1.log");
        assert!(expected.exists(), "输出文件应落在 {expected:?}");
        let snapshot = tasks.snapshot(handle, "sess-1").unwrap();
        assert_eq!(snapshot.output_path, expected);
        assert!(snapshot.output_bytes > 0, "输出字节数应大于 0");
        let content = fs::read_to_string(&expected).unwrap();
        assert!(
            content.contains("hello-bg"),
            "stdout 与 stderr 合并进同一个文件，实际内容：{content:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 条目数到 `max_entries`（**未投出的完成项**）时淘汰**最旧的一条**；在跑的条目不参与计数。
    ///
    /// 变异：把在跑项也计入名额（回到「共用一个 max_entries」）⇒ 第一条断言变红。
    #[test]
    fn a_full_table_evicts_the_oldest_completed_entry() {
        let dir = test_dir("bg_evict");
        let tasks = BackgroundTasks::new(dir.clone(), limits(1));
        let cwd = std::env::temp_dir();

        let finished = tasks.start("s", "true", &cwd, None).unwrap();
        let running = tasks.start("s", "sleep 30", &cwd, None).unwrap();
        wait_done(&tasks, finished, "s");

        // 未投出的完成项：1 条（`finished`）= 上限 ⇒ 下一条启动必须先淘汰它。
        let third = tasks.start("s", "true", &cwd, None).unwrap();
        assert_eq!(third, BgHandle(3));
        assert!(
            tasks.snapshot(finished, "s").is_none(),
            "最旧的已完成条目应被淘汰"
        );
        assert!(
            !dir.join("s").join("bg1.log").exists(),
            "被淘汰条目的落盘文件应一并删除"
        );
        assert!(
            tasks.snapshot(running, "s").is_some(),
            "在跑的条目不得被淘汰"
        );

        // 清理：杀掉还在跑的进程组并收尸，不留孤儿。
        let snapshot = tasks.snapshot(running, "s").unwrap();
        kill_group(snapshot.pid);
        tasks.sweep();

        let _ = fs::remove_dir_all(&dir);
    }

    /// 完成时**恰好一条** `Deferred`；连续多次 `sweep` 不重发。
    ///
    /// 变异：去掉 `Delivery` 判断（每 tick 重新取候选）⇒ 「只投一条」断言变红。
    #[test]
    fn a_completion_is_delivered_exactly_once() {
        let dir = test_dir("bg_deliver_once");
        let tasks = Arc::new(BackgroundTasks::new(dir.clone(), BgLimits::default()));
        tasks.set_completion_renderer(|candidate| Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("[{}] done", candidate.handle.label()),
            }],
        });
        let sink = Arc::new(QueueBoundarySource::new());
        let _binding = tasks.bind_deferred(Arc::clone(&sink));

        let handle = tasks
            .start("s", "true", &std::env::temp_dir(), None)
            .unwrap();
        wait_done(&tasks, handle, "s");

        // 再跑几轮：落 `Delivered` 后不得重发。
        for _ in 0..5 {
            tasks.sweep();
        }
        let mut delivered = 0;
        while let Some(boundary) = sink.take() {
            match boundary {
                Boundary::Deferred { handle, .. } => {
                    assert_eq!(handle, "bg1");
                    delivered += 1;
                }
                other => panic!("只应有 Deferred，得到 {other:?}"),
            }
        }
        assert_eq!(delivered, 1, "同一条结果只能投一次");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 未绑落点 ⇒ 结果留表；绑定后补投**只补一次**。
    ///
    /// 变异：不写 `Delivery::Delivered`（补投后仍是 `Pending`）⇒ 第二次补投又多一条。
    #[test]
    fn an_unbound_completion_waits_for_the_next_turn() {
        let dir = test_dir("bg_unbound");
        let tasks = Arc::new(BackgroundTasks::new(dir.clone(), BgLimits::default()));
        tasks.set_completion_renderer(|candidate| Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: candidate.handle.label(),
            }],
        });

        let handle = tasks
            .start("s", "true", &std::env::temp_dir(), None)
            .unwrap();
        wait_done(&tasks, handle, "s");
        // 未绑定 ⇒ 清扫只把条目留在表里（`Pending`），不投。
        tasks.sweep();
        assert!(
            tasks.snapshot(handle, "s").is_some(),
            "未投出的完成项必须留在表里"
        );

        let sink = Arc::new(QueueBoundarySource::new());
        let _binding = tasks.bind_deferred(Arc::clone(&sink));
        tasks.sweep();
        assert!(sink.take().is_some(), "绑定后的第一轮清扫应补投");
        tasks.sweep();
        assert!(sink.take().is_none(), "已投出的结果不得重复补投");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `take_completions` 只取本会话、只取 `Pending`，且取出后同一 tick 不再取。
    ///
    /// 变异：`claim_completions` 不看 `delivery` ⇒ 第二次取又拿到同一条。
    #[test]
    fn take_completions_claims_once_and_filters_by_session() {
        let dir = test_dir("bg_take_completions");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        let mine = tasks.start("mine", "true", &cwd, None).unwrap();
        let other = tasks.start("other", "true", &cwd, None).unwrap();
        wait_done(&tasks, mine, "mine");
        wait_done(&tasks, other, "other");

        let claimed = tasks.take_completions("mine");
        assert_eq!(claimed.len(), 1, "只取本会话的完成项");
        assert_eq!(claimed[0].handle, mine);
        assert!(
            tasks.take_completions("mine").is_empty(),
            "已认领（InFlight）的条目不得被重复取出"
        );

        // 投不出去 ⇒ 退回 `Pending`，下轮可取。
        tasks.requeue(mine);
        assert_eq!(
            tasks.take_completions("mine").len(),
            1,
            "requeue 后应可重投"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **渲染在锁外完成** —— `sweep` 三段式的段 2（设计 §2.4 不变量 1）。
    ///
    /// 判据不是计时，而是**可重入性**：`std::sync::Mutex` 不可重入 ⇒ 若清扫在**持锁**
    /// 期间调渲染回调，回调里再取注册表（`entry_count`）就会死锁。故这里让探针
    /// 渲染器**故意取一次注册表锁**，并把 `sweep` 放进独立线程限时 join ⇒
    /// 那个变异表现为**确定性超时**（而不是整个测试挂死、无定位信息）。
    ///
    /// 变异：把 `deliver` 挪进段 1（持锁渲染）⇒ 探针取锁时互相等 ⇒ 超时变红。
    ///
    /// **已知缺口**：「锁内不读**输出文件**」这条无法在本模块黑盒验证 ——
    /// 读取只在渲染回调里发生（`bg_output`），而回调是 `fn` 指针、不可测；
    /// 设计 §7 要求的 `#[cfg(test)]` 计数器未实现（不属本次测试改动范围）。
    #[test]
    fn the_renderer_runs_outside_the_registry_lock() {
        let dir = test_dir("bg_lock_render");
        let tasks = Arc::new(BackgroundTasks::new(dir.clone(), BgLimits::default()));
        *RENDERER_PROBE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&tasks));
        tasks.set_completion_renderer(probe_renderer);
        let sink = Arc::new(QueueBoundarySource::new());
        let _binding = tasks.bind_deferred(Arc::clone(&sink));

        let handle = tasks
            .start("s", "true", &std::env::temp_dir(), None)
            .unwrap();
        // 用 `snapshot` 收尸（**不认领**完成项）⇒ 条目落到「Exited + Pending」，
        // 等即将进行的那一次清扫去投递。不能走 `wait_done`（它自己会 sweep 并已投完）。
        for _ in 0..200 {
            if tasks.snapshot(handle, "s").unwrap().state != BgState::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            tasks.snapshot(handle, "s").unwrap().state,
            BgState::Running,
            "命令应先结束（否则下面量的是「没东西可投」）"
        );

        let sweep_tasks = Arc::clone(&tasks);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sweep_tasks.sweep();
            let _ = sender.send(());
        });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("清扫在 10s 内没返回 —— 渲染回调被要求在锁内执行（自死锁）");

        let observed = RENDER_RECORD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(observed.len(), 1, "这一次清扫应恰好渲染一条完成项");
        assert!(
            observed[0] >= 1,
            "渲染期间应能读到注册表（读到的是 {}）",
            observed[0]
        );

        *RENDERER_PROBE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let _ = fs::remove_dir_all(&dir);
    }

    /// 探针渲染器能拿到的注册表 —— 只在本用例里被设上。
    ///
    /// `CompletionRenderer` 是 `fn` 指针（不能捕获环境）⇒ 只能经静态变量取注册表。
    static RENDERER_PROBE: Mutex<Option<Arc<BackgroundTasks>>> = Mutex::new(None);

    /// 探针渲染器每次被调用时，在注册表里读到的条目数。
    static RENDER_RECORD: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    /// 探针渲染器 —— 回调体内**再取一次注册表锁**，证明它不在锁内被调。
    fn probe_renderer(candidate: &BgCompletion) -> Message {
        let tasks = RENDERER_PROBE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .expect("探针未安装");
        let observed = tasks.entry_count();
        RENDER_RECORD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(observed);
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("[{}] probe", candidate.handle.label()),
            }],
        }
    }

    /// `InFlight` 超 2 tick 未落状态 ⇒ 回归 `Pending` 重投（**宁可重复，绝不丢失**）。
    ///
    /// 变异：去掉 tick 修复 ⇒ 条目永久卡在 `InFlight`，第二次清扫取不到它。
    #[test]
    fn an_interrupted_delivery_is_repaired_by_the_next_ticks() {
        let dir = test_dir("bg_inflight_repair");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        let handle = tasks.start("s", "true", &cwd, None).unwrap();
        wait_done(&tasks, handle, "s");

        // 模拟「认领后那次投递被中断」：只取不落状态。
        assert_eq!(tasks.take_completions("s").len(), 1);
        assert!(
            tasks.take_completions("s").is_empty(),
            "InFlight 期间不可重取"
        );

        // 未超时前不得抢回（否则同一 tick 内会重投多次）。
        tasks.sweep();
        assert!(tasks.take_completions("s").is_empty(), "未超时就不该重取");

        // 超过 2 tick ⇒ 回 `Pending`，可重投。
        std::thread::sleep(SWEEP_TICK * INFLIGHT_TICKS + Duration::from_millis(50));
        tasks.sweep();
        assert_eq!(
            tasks.take_completions("s").len(),
            1,
            "超时未落的 InFlight 必须回归 Pending"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 在跑项（上限 4）与未投出完成项（上限 8）**分开计数**，互不挤占。
    ///
    /// 变异：两者共用一个名额计数 ⇒ 「4 在跑 + 2 完成」时就报 `TooManyRunning` 或开始淘汰。
    #[test]
    fn running_and_pending_are_counted_separately() {
        let dir = test_dir("bg_separate_quota");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        // 先把 2 条完成项堆起来（不投递）。
        let done: Vec<BgHandle> = (0..2)
            .map(|_| tasks.start("s", "true", &cwd, None).unwrap())
            .collect();
        for handle in &done {
            wait_done(&tasks, *handle, "s");
        }

        // 再开满在跑名额（4 条）—— 完成项不占名额 ⇒ 不得报错。
        let running: Vec<BgHandle> = (0..4)
            .map(|_| tasks.start("s", "sleep 30", &cwd, None).unwrap())
            .collect();
        for handle in &done {
            assert!(
                tasks.snapshot(*handle, "s").is_some(),
                "在跑项不得把未投出的完成项挤掉"
            );
        }

        // 清理：杀掉还在跑的进程组并收尸，不留孤儿。
        for handle in running {
            let snapshot = tasks.snapshot(handle, "s").unwrap();
            kill_group(snapshot.pid);
        }
        tasks.sweep();

        let _ = fs::remove_dir_all(&dir);
    }

    /// 投递中的条目（`InFlight`）不得被淘汰，也不得被投出去：abort 掉的条目不投。
    ///
    /// 变异①：`evict_oldest_completed` 不看 `InFlight` ⇒ 第一条断言变红。
    /// 变异②：`kill_entry` 不置 `Delivered` ⇒ 第二条断言变红。
    #[test]
    fn inflight_entries_are_neither_evicted_nor_killed_after_abort() {
        let dir = test_dir("bg_inflight_evict");
        let tasks = BackgroundTasks::new(dir.clone(), limits(1));
        let cwd = std::env::temp_dir();

        // ── ① `InFlight` 不参与淘汰（占了名额，但删不得）──
        let finished = tasks.start("s", "true", &cwd, None).unwrap();
        wait_done(&tasks, finished, "s");
        assert_eq!(tasks.take_completions("s").len(), 1);
        let error = tasks
            .start("s", "true", &cwd, None)
            .expect_err("投递中的条目不可淘汰 ⇒ 新启动必须报错，而不是删掉它");
        assert!(matches!(error, BgError::TableFull { .. }), "got {error:?}");
        // 投出去后它不再占名额 ⇒ 新启动才行。
        tasks.finish_delivery(finished, true);
        let third = tasks.start("s", "true", &cwd, None).unwrap();
        assert_eq!(third, BgHandle(2), "失败的启动不消耗句柄号");

        // ── ② abort 杀掉的条目不投递 ──
        let doomed = tasks.start("s", "sleep 30", &cwd, None).unwrap();
        assert!(tasks.kill(doomed, "s"));
        tasks.sweep();
        assert!(
            !tasks
                .take_completions("s")
                .iter()
                .any(|claimed| claimed.handle == doomed),
            "被 harness 杀掉的命令不得投递"
        );
        // 清理：收掉可能还没退的进程。
        tasks.sweep();

        let _ = fs::remove_dir_all(&dir);
    }

    /// `adopt` 的输出正文由调用方补录 —— 不建文件也不会丢（`append_output` 负责创建）。
    ///
    /// 变异：`append_output` 在锁内写文件（把 `path` 解析与写写在同一临界区）⇒
    /// 本用例仍绿，但锁内 IO 不变量破 —— 那条靠 code review 守。
    #[test]
    fn an_adopted_child_accepts_appended_output() {
        let dir = test_dir("bg_adopt_append");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());

        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let handle = tasks.adopt(child, "s", "sleep 30".to_string(), std::env::temp_dir());

        assert!(tasks.append_output(handle, "alpha\n"));
        let snapshot = tasks.snapshot(handle, "s").unwrap();
        assert_eq!(snapshot.output_bytes, 6, "补录的正文应落进输出文件");
        assert!(
            !tasks.append_output(BgHandle(9999), "beta\n"),
            "未知句柄不得凭空建出输出文件"
        );

        // 清理：杀掉收养的进程组并收尸，不留孤儿。
        kill_group(snapshot.pid);
        tasks.sweep();

        let _ = fs::remove_dir_all(&dir);
    }

    /// 过期的条目必须被**真的杀掉** —— 「锁外杀」不是「不杀」。
    ///
    /// 变异：`kill_outside` 只丢 `Child` 不调 `kill_group` ⇒ 进程仍然活着，断言变红。
    #[test]
    fn an_expired_entry_is_actually_killed_outside_the_lock() {
        let dir = test_dir("bg_timeout_kill");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        // 寿命 0 ⇒ 第一次清扫就到期。
        let handle = tasks
            .start("s", "sleep 30", &cwd, Some(Duration::ZERO))
            .unwrap();
        tasks.sweep();
        let snapshot = tasks.snapshot(handle, "s").unwrap();
        assert_eq!(snapshot.state, BgState::Killed, "到期条目应标 Killed");

        // 杀进程组在锁外完成 ⇒ 稍等它真的死掉（`kill` 是异步的，且死后要先收尸
        // —— 僵尸仍占 pid，`kill -0` 对它照样成功）。
        let mut gone = false;
        for _ in 0..200 {
            tasks.sweep();
            if process_is_gone(snapshot.pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone, "到期条目必须被杀掉（不是只改状态）");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 进程是否真的不在（`kill -0` 报告没有这个 pid）。
    fn process_is_gone(pid: u32) -> bool {
        !Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// `adopt` 登记的子进程与 `start` 启动的条目同构：被清扫收尸 ⇒ 可投递。
    ///
    /// 变异：`adopt` 不写 `delivery: Pending`（比如复用某个已 `Delivered` 的默认）
    /// ⇒ 完成项取不出来。
    #[test]
    fn an_adopted_child_is_reaped_and_deliverable() {
        let dir = test_dir("bg_adopt");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());

        let child = Command::new("sh")
            .arg("-c")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let handle = tasks.adopt(child, "s", "true".to_string(), std::env::temp_dir());

        wait_done(&tasks, handle, "s");
        let claimed = tasks.take_completions("s");
        assert_eq!(claimed.len(), 1, "过继的条目与自启的条目一样要投递");
        assert_eq!(claimed[0].handle, handle);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 落盘根目录必须是绝对路径 —— 相对路径会把产物落进用户的工作区。
    #[test]
    fn a_relative_root_is_rejected() {
        let tasks = BackgroundTasks::new(PathBuf::from("relative/bg-root"), BgLimits::default());
        let error = tasks
            .start("s", "true", &std::env::temp_dir(), None)
            .expect_err("相对路径必须被拒绝");
        assert!(matches!(error, BgError::RelativeRoot(_)), "got {error:?}");
        assert_eq!(tasks.entry_count(), 0);
    }

    /// `remove_session_dir` 删掉整个会话目录，且**拒绝路径穿越**。
    ///
    /// 变异：把 `is_safe_session` 的检查去掉 ⇒ 第二条断言变红。
    #[test]
    fn remove_session_dir_deletes_the_directory_and_rejects_traversal() {
        let dir = test_dir("bg_remove_dir");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        tasks.start("sess-1", "true", &cwd, None).unwrap();
        let session_dir = dir.join("sess-1");
        assert!(session_dir.is_dir(), "会话目录应已建好");

        tasks.remove_session_dir("sess-1").expect("删除会话目录");
        assert!(
            !session_dir.exists(),
            "会话目录应被整体删除（其下的输出文件随之消失）"
        );
        // 幂等：目录已经不在了 ⇒ 不报错。
        tasks
            .remove_session_dir("sess-1")
            .expect("删除不存在的目录应幂等成功");

        // 路径穿越：会话 id 含分隔符 / 是 `..` ⇒ 一律拒绝，且**不删任何东西**。
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        for bad in ["../outside", "a/b", "..", "."] {
            let error = tasks
                .remove_session_dir(bad)
                .expect_err("路径穿越的会话 id 必须被拒绝");
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::InvalidInput,
                "会话 id {bad:?} 应被拒绝"
            );
        }
        assert!(
            outside.join("keep.txt").exists(),
            "拒绝的会话 id 不得删掉落盘根之外的文件"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **`/new` 之后新会话收不到旧会话的完成项**（设计 §7）。
    ///
    /// `/new` 的语义是「旧会话整体作废」：旧会话已结束的条目不投给新会话，
    /// 也没必要重复杀已死的进程。老条目只等淘汰 —— 它不挤占新会话的任何东西。
    ///
    /// 变异：取候选时去掉会话过滤 ⇒ 第二条断言变红（新会话收到别的会话的结果）。
    #[test]
    fn after_new_session_the_old_completions_are_not_delivered() {
        let dir = test_dir("bg_new_session");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let cwd = std::env::temp_dir();

        let stale = tasks.start("old", "true", &cwd, None).unwrap();
        wait_done(&tasks, stale, "old");

        assert!(
            tasks.take_completions("new").is_empty(),
            "新会话不得收到旧会话的完成项"
        );

        // 旧会话已全部终态 ⇒ `kill_all` 无事可做，且不报错。
        tasks.kill_all("old");
        assert!(tasks.running_handles("old").is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 会话 id 含路径分隔符时必须被拒绝 —— 落盘目录不得逃出 `<root>/<session>/`。
    #[test]
    fn a_session_with_a_separator_is_rejected() {
        let dir = test_dir("bg_bad_session");
        let tasks = BackgroundTasks::new(dir.clone(), BgLimits::default());
        let error = tasks
            .start("../escape", "true", &std::env::temp_dir(), None)
            .expect_err("含分隔符的会话 id 必须被拒绝");
        assert!(matches!(error, BgError::InvalidSession(_)), "got {error:?}");
        assert_eq!(tasks.entry_count(), 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
