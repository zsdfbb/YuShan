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
//! 句柄分配、条目淘汰、子进程回收都在同一把锁内完成，不需要额外状态机。

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant};

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
    match String::from_utf8_lossy(&output.stdout).trim().parse::<u64>() {
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
    /// 已交付给模型的字节数（读偏移）。
    consumed: u64,
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
    /// 已交付给模型的字节数（读偏移）。
    pub consumed: u64,
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
    /// 句柄表条目上限（在跑 + 已完成未取走）—— 满了淘汰**最旧的已完成**条目。
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
    /// 条目表已满且没有可淘汰的已完成条目。
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
                "background command table is full ({limit}) and no finished entry can be evicted"
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

/// 后台命令注册表 —— **进程级**；内容是**会话级**。
pub struct BackgroundTasks {
    /// 落盘根目录 —— **必须是绝对路径**。
    root: PathBuf,
    limits: BgLimits,
    inner: Mutex<Inner>,
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
        }
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
        let mut inner = self.lock();
        self.reap(&mut inner);

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

        // 条目表到上限 ⇒ 淘汰**最旧的已完成**条目（删文件）；全在跑 ⇒ 报错。
        while inner.entries.len() >= self.limits.max_entries {
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
            consumed: 0,
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
        let mut inner = self.lock();
        // 闸门**先杀后收**：顺序不可颠倒（收尸 ⇒ pid 可被复用 ⇒ `kill -9 -<pid>`
        // 会打到无关的进程组）。
        self.enforce_output_cap(&mut inner);
        self.reap(&mut inner);
        let entry = inner
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle && entry.session == session)?;
        Some(BgSnapshot {
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
            consumed: entry.consumed,
            // 子进程句柄已丢弃 = 已收尸 = 进程真没了 ⇒ 输出文件不会再增长。
            process_gone: entry.child.is_none(),
        })
    }

    /// 清扫：回收已退出的子进程 + 淘汰超出条目上限的**最旧已完成**条目。
    pub fn sweep(&self) {
        let mut inner = self.lock();
        // 输出上限闸门**先杀进程组**，再由 `reap` 收尸 —— 顺序不可颠倒
        // （收尸 ⇒ pid 可被复用 ⇒ `kill -9 -<pid>` 会打到无关的进程组）。
        self.enforce_output_cap(&mut inner);
        self.reap(&mut inner);
        while inner.entries.len() > self.limits.max_entries {
            if !Self::evict_oldest_completed(&mut inner) {
                break;
            }
        }
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

    /// 推进交付偏移（`consumed`）—— **只进不退**。
    ///
    /// 返回 `false` = 句柄不存在或不属于该会话。
    pub fn advance_consumed(&self, handle: BgHandle, session: &str, offset: u64) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle && entry.session == session)
        else {
            return false;
        };
        if offset > entry.consumed {
            entry.consumed = offset;
        }
        true
    }

    /// 杀掉一条条目的进程组并收尸（**先杀后收**）。
    fn kill_entry(entry: &mut BgEntry) {
        // 已在终态的条目不动 —— 正常退出的命令不该被改写成「被 harness 杀掉」。
        if entry.state == BgState::Running {
            kill_group(entry.pid);
            entry.state = BgState::Killed;
        }
        // 收尸：被杀的进程可能还没退出 ⇒ 这轮拿不到就留着句柄，下轮再收。
        if let Some(mut child) = entry.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => entry.child = Some(child),
            }
        }
    }

    /// 输出上限闸门（**精确闸门**）：输出文件到上限 ⇒ **先杀进程组**（交由 `reap` 收尸）。
    ///
    /// 精确表达「输出文件不得超过 `max_output_bytes`」的**只有这一处** ——
    /// 内核侧的 `ulimit -f` 是**粗闸门**（进程级、被子孙继承）⇒ 拦不住
    /// 「输出文件超限但命令没在写别的大文件」这一形态。
    /// 已经退出的子进程不再杀（免得把正常结束误报成「被 harness 杀掉」）。
    fn enforce_output_cap(&self, inner: &mut Inner) {
        if self.limits.max_output_bytes == 0 {
            return;
        }
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
            kill_group(entry.pid);
            entry.state = BgState::Killed;
        }
    }

    /// 把子进程状态推进到终态：到寿命上限 ⇒ **先杀进程组再收尸**，否则只收尸。
    ///
    /// **先杀后收**的顺序不可反：反了可能打到已被复用的 pid。
    fn reap(&self, inner: &mut Inner) {
        for entry in inner.entries.iter_mut() {
            if entry.state == BgState::Running && entry.started_at.elapsed() >= entry.timeout {
                kill_group(entry.pid);
                entry.state = BgState::Killed;
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
    }

    /// 淘汰**最旧的已完成**条目（按启动顺序）并删除其落盘文件。
    ///
    /// 返回 `false` = 没有可淘汰的条目（全部在跑）。
    fn evict_oldest_completed(inner: &mut Inner) -> bool {
        let Some(index) = inner
            .entries
            .iter()
            .position(|entry| entry.state != BgState::Running)
        else {
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
            if entry.state == BgState::Running {
                kill_group(entry.pid);
                entry.state = BgState::Killed;
            }
            // 收尸：被杀的进程可能已经退出 ⇒ 顺手收掉，不留僵尸。
            if let Some(mut child) = entry.child.take() {
                match child.try_wait() {
                    Ok(Some(_)) => {}
                    _ => entry.child = Some(child),
                }
            }
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
        let tasks = BackgroundTasks::new(dir.clone(), limits(2));
        let cwd = std::env::temp_dir();

        let first = tasks.start("s", "true", &cwd, None).unwrap();
        let second = tasks.start("s", "true", &cwd, None).unwrap();
        assert_eq!(first, BgHandle(1));
        assert_eq!(second, BgHandle(2));
        assert_eq!(first.label(), "bg1");
        assert_eq!(second.label(), "bg2");

        // 条目表到上限（2 条）⇒ 下一条必须淘汰**最旧的已完成**条目。
        wait_done(&tasks, first, "s");
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
        assert!(
            matches!(error, BgError::Spawn { .. }),
            "got {error:?}"
        );

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

    /// 条目数到 `max_entries` 时淘汰**最旧的已完成**条目；在跑的条目不动。
    #[test]
    fn a_full_table_evicts_the_oldest_completed_entry() {
        let dir = test_dir("bg_evict");
        let tasks = BackgroundTasks::new(dir.clone(), limits(2));
        let cwd = std::env::temp_dir();

        let finished = tasks.start("s", "true", &cwd, None).unwrap();
        let running = tasks.start("s", "sleep 30", &cwd, None).unwrap();
        wait_done(&tasks, finished, "s");

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
