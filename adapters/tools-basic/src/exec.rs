//! 搜索后端的**探测**与**有界子进程原语**（设计 `docs/arch/tools-align-pi/design.md` §1.3）。
//!
//! 两件事，彼此独立：
//!
//! 1. [`SearchBackends`] —— 启动期在 `PATH` 上找 `rg` / `fd`（**纯路径扫描**：不 spawn
//!    进程、不探 `--version`）。是**值**而非 `OnceLock`，故降级路径可在同进程内注入复现。
//! 2. [`run_bounded`] —— spawn 一个外部二进制，**有界**地收 stdout/stderr，并在中止/超时/
//!    超限时把**它的整个进程组**杀掉。`grep` / `find` 的两种后端（`rg`/`grep`、`fd`/`find`）共用它。
//!
//! # 为什么「有界」要单独成一个原语
//!
//! 外部二进制（尤其 `rg -r`）可能刷出 GB 级输出、也可能因为管道写满而**永远阻塞**。
//! 本模块把「spawn → 持续排空双管道 → 达上限/中止/超时即杀 → 收尸」这套易写错的
//! 控制流收拢在一处，调用方只关心 argv 与输出解析。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use ys_protocol::BoundarySource;
use ys_tool::ToolError;

#[cfg(unix)]
use crate::bash::kill_process_group;

/// 中止 / 上限轮询间隔 —— 与 `bash.rs::ABORT_POLL_INTERVAL` 一致：中止的感知延迟上界。
const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 每次 `read` 的缓冲区大小（同时是 stdout 累积的粒度）。
const STREAM_CHUNK: usize = 8 * 1024;

/// stderr 保留上限：**只保留前 4 KiB**，其余读到即丢（但仍继续读到 EOF，见 [`run_bounded`]）。
///
/// 调用方（`grep` / `find`）出错时展示的就是这前 4 KiB。
pub const STDERR_KEEP: usize = 4 * 1024;

/// `rg` 的候选可执行名。
const RG_NAMES: &[&str] = &["rg"];

/// `fd` 的候选可执行名 —— Debian/Ubuntu 打包成 `fdfind`（与 `fd` 冲突）。
const FD_NAMES: &[&str] = &["fd", "fdfind"];

/// 探测到的搜索后端（**值**，可注入）。
///
/// `None` 表示该后端不可用，调用方需走 POSIX 降级路径（`grep` / `find`）。
/// 探测是**纯路径扫描**：启动期只此一次，不 fork、不跑 `--version`（见 ADR-0015）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchBackends {
    pub rg: Option<PathBuf>,
    pub fd: Option<PathBuf>,
}

impl SearchBackends {
    /// 按**进程 `PATH`** 探测（生产路径；`toolset` 启动期调用一次）。
    pub fn detect() -> Self {
        Self::detect_from(&std::env::var("PATH").unwrap_or_default())
    }

    /// 按给定 `PATH` 字符串探测（**注入式**：降级路径可在测试内确定性复现，不读进程 env）。
    ///
    /// 按 `:` 切分目录（`std::env::split_paths`），**目录顺序即优先级**；每个目录内依次
    /// 试候选名，命中「是文件 + 带可执行位」即返回。
    pub fn detect_from(path: &str) -> Self {
        let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
        Self {
            rg: find_in_path(&dirs, RG_NAMES),
            fd: find_in_path(&dirs, FD_NAMES),
        }
    }

    /// 两后端皆 `None`（测试 / 强制降级）。
    pub fn posix_only() -> Self {
        Self { rg: None, fd: None }
    }

    /// 缺任一后端时返回**一行**启动期提示（含缺什么 + 「不读 `.gitignore`」这一后果）；
    /// 两者皆在时返回 `None`。
    ///
    /// 只产出文案，**不 print** —— 打印由 app 层在启动期做（输出三分，见 `main`）。
    pub fn degraded_hint(&self) -> Option<String> {
        let mut missing: Vec<&str> = Vec::new();
        if self.rg.is_none() {
            missing.push("rg (ripgrep)");
        }
        if self.fd.is_none() {
            missing.push("fd (fd-find)");
        }
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "[yushan] not found on PATH: {} — grep/find fall back to POSIX tools and will NOT \
             honor .gitignore; install ripgrep and fd-find for better results.",
            missing.join(", ")
        ))
    }
}

/// 在目录序列里找第一个可执行候选（目录顺序优先，其次候选名顺序）。
fn find_in_path(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    for dir in dirs {
        for name in names {
            let cand = dir.join(name);
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// 「是文件且带可执行位」。unix 上看 `mode & 0o111`；非 unix 退化为 `is_file()`。
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 子进程以何种方式结束 —— **`stop != Eof` 恒表示「是我们杀的」**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// 正常跑到 EOF（进程自然退出）；此时 `code` 才有意义。
    Eof,
    /// stdout 累积达到 `byte_cap` → 我们杀掉了它（防刷爆内存 / CPU）。
    Cap,
    /// `boundary.is_aborted()` → 我们杀掉了它。
    Aborted,
    /// 超过 `deadline` → 我们杀掉了它。
    TimedOut,
}

/// 一次有界子进程运行的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounded {
    /// stdout（`stop == Cap` 时已被截到 `byte_cap`）。
    pub stdout: Vec<u8>,
    /// stderr 的**前 4 KiB**（[`STDERR_KEEP`]），其余已丢弃。
    pub stderr: Vec<u8>,
    /// 退出码。被信号杀死时为 `None`。
    ///
    /// **`stop != Eof` 时调用方应忽略本字段** —— 那是我们杀的，`code` 多半是 `None`
    /// 或一个与「命令本身失败」无关的值；只有 `stop == Eof` 时退出码才承载语义。
    pub code: Option<i32>,
    /// 结束方式（见 [`Stop`]）。
    pub stop: Stop,
}

/// spawn 外部二进制，**有界**收集输出；达上限 / 被中止 / 超时则杀掉**它的整个进程组**。
///
/// - `byte_cap`：stdout 累积上限（字节）。达到即杀子进程、`stop = Cap`（`grep`/`find` 传 1 MiB）。
/// - `deadline`：从本函数开始跑的**最长时间**（调用方自带兜底 timeout —— 一次性模式
///   `-p`/`--json` 的 `boundary` 恒为 `None`，没有别的中止通道）。
/// - `boundary`：`Some` 时每 [`ABORT_POLL_INTERVAL`] 探一次 `is_aborted()`。
///
/// # 死锁防护（本函数最易错处之一）
///
/// **stdout 与 stderr 两个管道都必须被持续排空**，否则子进程会因管道缓冲区写满而**永远
/// 阻塞在 `write()` 上**——我们等它退出、它等我们读，双向死锁。故：
///
/// - 两个管道各起一个任务读到 **EOF**；stderr 任务**读满 4 KiB 后仍要继续读到 EOF**，
///   只是不再累积（若读满就停手，管道照样写满、子进程照样卡死）。
/// - stdout 任务达 `byte_cap` 后同样**继续读到 EOF（丢弃）**，只置 `capped` 标志；
///   主循环轮询到标志再杀 —— 被杀前的那段时间里管道仍有人读。
///
/// # 杀与收尸
///
/// 杀的是**整个进程组**（spawn 时 `process_group(0)`，故 pgid == 子进程 pid）：
/// 只杀直接子进程时，`sh -c '... &'` 这类后端的孙进程会漏网 —— 它们被 launchd 收养成
/// `ppid=1` 的孤儿，还会一直攥着管道写端，让两个排空任务永远等不到 EOF。
/// 顺序固定为 **杀组 → `start_kill()` 兜底 → `wait()` 收尸**（收尸后再按 pid 杀组不安全：
/// pid 可能已被复用）。`kill_on_drop(true)` 另外保证本 future 被外层 `timeout` drop 时
/// 子进程随之被杀（不泄漏 —— 但 drop 路径只及直接子进程）。组内进程全部退出后管道写端
/// 关闭，两个排空任务自然收到 EOF。
///
/// # 错误
///
/// 仅 spawn 失败 / `try_wait` 失败返回 [`ToolError::Execution`]。
/// **非零退出码不是错误** —— 原样返回 `Ok(Bounded { code: Some(2), .. })`，由调用方按
/// 退出码语义（`1` = 无命中、`2` = 出错）判定。本函数**不 print 任何东西**。
///
/// # 前提
///
/// 适用于 `rg` / `fd` / `grep` / `find` 这类单个搜索二进制。唯一未覆盖的形态：直接子进程
/// **自然退出**（`stop == Eof`）而孙进程仍攥着管道写端 —— 那条路径没有「我们杀的」这一步，
/// 无法安全定位进程组（leader 已收尸），排空任务会等不到 EOF。要覆盖它须由调用方保证不 fork。
pub async fn run_bounded(
    prog: &Path,
    args: &[String],
    cwd: &Path,
    byte_cap: usize,
    deadline: Duration,
    boundary: Option<&dyn BoundarySource>,
) -> Result<Bounded, ToolError> {
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 外层超时 drop future 时连带杀子进程（与 `bash.rs` 同款，防泄漏）。
        .kill_on_drop(true);

    // 独立进程组（pgid == 子进程 pid），与 [`kill_process_group`] 配对：**只有进了自己的组，
    // `kill -9 -<pid>` 才覆盖得到孙进程**。否则「被杀」只及直接子进程 —— `sh -c '... &'`
    // 这类后端的孙进程会漏网成 `ppid=1` 的孤儿，并继续攥着管道写端。
    #[cfg(unix)]
    {
        #[allow(unused_imports)]
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("Failed to spawn {}: {e}", prog.display())))?;

    let mut stdout = child.stdout.take().ok_or_else(|| {
        ToolError::Execution(format!("Failed to capture stdout of {}", prog.display()))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        ToolError::Execution(format!("Failed to capture stderr of {}", prog.display()))
    })?;

    // ── 排空 stdout：累积到 byte_cap，之后**继续读并丢弃**，只置标志 ──
    let capped = Arc::new(AtomicBool::new(false));
    let capped_out = Arc::clone(&capped);
    let out_task = tokio::spawn(async move {
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0u8; STREAM_CHUNK];
        loop {
            match stdout.read(&mut buf).await {
                // EOF / 读错误：管道已无内容，任务收束（错误也当作 EOF，保底不挂起）。
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept.len() < byte_cap {
                        let take = (byte_cap - kept.len()).min(n);
                        kept.extend_from_slice(&buf[..take]);
                    }
                    if kept.len() >= byte_cap {
                        capped_out.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        kept
    });

    // ── 排空 stderr：**只保留前 4 KiB，但必须读到 EOF**（读满就停手 = 子进程卡死） ──
    let err_task = tokio::spawn(async move {
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0u8; STREAM_CHUNK];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept.len() < STDERR_KEEP {
                        let take = (STDERR_KEEP - kept.len()).min(n);
                        kept.extend_from_slice(&buf[..take]);
                    }
                }
            }
        }
        kept
    });

    // ── 轮询：自然退出 / 达上限 / 被中止 / 超时 ──
    let started = Instant::now();
    let mut code: Option<i32> = None;

    let stop = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // `try_wait` 已收尸，此处拿到的 code 是权威的。
                code = status.code();
                break Stop::Eof;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ToolError::Execution(format!(
                    "Failed to wait for {}: {e}",
                    prog.display()
                )));
            }
        }

        if capped.load(Ordering::Relaxed) {
            break Stop::Cap;
        }
        if boundary.is_some_and(|b| b.is_aborted()) {
            break Stop::Aborted;
        }
        if started.elapsed() >= deadline {
            break Stop::TimedOut;
        }

        // 按剩余时间收敛，避免超时被轮询间隔拉长
        let nap = ABORT_POLL_INTERVAL.min(deadline.saturating_sub(started.elapsed()));
        tokio::time::sleep(nap).await;
    };

    if stop != Stop::Eof {
        // 是我们杀的：先杀**整个进程组**（含孙进程），再 `start_kill()` 兜底、`wait()` 收尸
        // （防僵尸）。组内进程全部退出后管道写端才关得干净 —— 只杀直接子进程时，孙进程会
        // 攥着写端让下面两个排空任务永远等不到 EOF。**必须赶在 `wait()` 之前**（见该函数文档）。
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            kill_process_group(pid).await;
        }
        let _ = child.start_kill();
        if let Ok(status) = child.wait().await {
            code = status.code();
        }
    }

    // 子进程已终止（或被 kill），管道已关闭 → 排空任务必然收束。
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();

    Ok(Bounded {
        stdout,
        stderr,
        code,
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_protocol::{Boundary, QueueBoundarySource};

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全的既有样板，见 `bash.rs`；
    /// `as_nanos()` 在 macOS 只到微秒级，单靠它会撞名）。`tag` 再按用例分桶。
    fn test_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agent_tools_basic_exec_test_{tag}_{}_{seq}",
            std::process::id()
        ))
    }

    /// 调用 [`run_bounded`]，并把**整次等待**套进 `tokio::time::timeout`。
    ///
    /// 本模块的核心风险是「双管道写满 → 双向死锁」，那种失败形态是**一直不返回**。
    /// 若不外套 timeout，挂起只会表现为测试永不结束（CI 上等同超时挂死、且无定位信息）；
    /// 外套之后，挂起 = panic = 确定性变红。返回 `(结果, 实际耗时)`。
    async fn run_guarded(
        prog: &str,
        args: &[&str],
        cwd: &Path,
        byte_cap: usize,
        deadline: Duration,
        boundary: Option<&dyn BoundarySource>,
        guard: Duration,
    ) -> (Result<Bounded, ToolError>, Duration) {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let started = Instant::now();
        let result = tokio::time::timeout(
            guard,
            run_bounded(Path::new(prog), &args, cwd, byte_cap, deadline, boundary),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("run_bounded 在 {guard:?} 内未返回 = 挂起（双管道死锁必须表现为失败）")
        });
        (result, started.elapsed())
    }

    // ── ① N4 四态 ────────────────────────────────────────────────────────

    /// `Eof`：自然退出 → 收集输出、退出码为 `Some(0)`。
    #[tokio::test]
    async fn test_run_bounded_eof_collects_output() {
        let dir = test_dir("eof");
        std::fs::create_dir_all(&dir).unwrap();

        let (result, _) = run_guarded(
            "sh",
            &["-c", "printf hello"],
            &dir,
            1024 * 1024,
            Duration::from_secs(5),
            None,
            Duration::from_secs(10),
        )
        .await;

        let b = result.expect("正常退出必须是 Ok");
        assert_eq!(b.stop, Stop::Eof);
        assert_eq!(b.stdout, b"hello");
        assert_eq!(b.code, Some(0), "自然退出时退出码才有语义");
        assert!(b.stderr.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Eof`（非零退码）：**退出码 3 不是 `Err`** —— 原样 `Ok`，由调用方按退出码语义判定
    /// （`grep` 的 `1` = 无命中、`2` = 出错，只有调用方知道怎么解释）。
    ///
    /// 变异：把 `try_wait` 分支里 `Ok(Some(status))` 改成对非零码返回 `Err` → 本用例红。
    #[tokio::test]
    async fn test_run_bounded_nonzero_exit_is_ok_not_err() {
        let dir = test_dir("nonzero");
        std::fs::create_dir_all(&dir).unwrap();

        let (result, _) = run_guarded(
            "sh",
            &["-c", "exit 3"],
            &dir,
            1024 * 1024,
            Duration::from_secs(5),
            None,
            Duration::from_secs(10),
        )
        .await;

        let b = result.expect("非零退出码不得是 Err");
        assert_eq!(b.stop, Stop::Eof);
        assert_eq!(b.code, Some(3));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Cap`：产出远超 `byte_cap` 的写者 → 达 `byte_cap` 即被杀，`stdout.len() == byte_cap`。
    ///
    /// 构造：`head -c 500000 /dev/zero` 先把管道写出 5 倍上限（保证 `capped` 置位），
    /// 再 `exec sleep 30` 让这个 pid **在检测到上限时仍然存活** —— 两条合起来才既触发
    /// `Stop::Cap` 又不依赖任何竞速。**刻意不用 `yes`**：无限写者一旦因回归漏杀就是
    /// 100% CPU 的空转孤儿；这里最坏情况只是一个 0% CPU 的 `sleep`。
    ///
    /// 变异：删掉主循环里的 `capped.load(..) → break Stop::Cap` → 走到 deadline 才返回
    /// `TimedOut` → `stop == Cap` 断言红（且 stdout 仍是截断后的长度，故只能靠 `stop` 抓）。
    #[tokio::test]
    async fn test_run_bounded_cap_kills_writer_at_byte_cap() {
        let dir = test_dir("cap");
        std::fs::create_dir_all(&dir).unwrap();
        let byte_cap = 100_000;

        let (result, elapsed) = run_guarded(
            "sh",
            &["-c", "head -c 500000 /dev/zero; exec sleep 30"],
            &dir,
            byte_cap,
            Duration::from_secs(10),
            None,
            Duration::from_secs(15),
        )
        .await;

        let b = result.expect("达上限不是错误（是我们杀的）");
        assert_eq!(b.stop, Stop::Cap);
        assert_eq!(b.stdout.len(), byte_cap, "stdout 应恰好截到 byte_cap");
        #[cfg(unix)]
        assert_eq!(b.code, None, "被 SIGKILL 杀死时退出码为 None");
        assert!(
            elapsed < Duration::from_secs(5),
            "确认即杀，实际 {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `TimedOut`：**小 deadline** 下断言返回耗时贴近 deadline。
    ///
    /// 这里刻意用 120ms（而非 review 里判别力不足的 500ms）：主循环的收敛式
    /// `nap = min(100ms, 剩余)` 与「固定 100ms 睡」在 120ms 上分得很开 ——
    /// 收敛时在 ~120ms 返回，退化为固定睡眠时会在 t≈100ms 睡满后于 t≈200ms 才察觉。
    ///
    /// 变异：把 `let nap = ABORT_POLL_INTERVAL.min(deadline.saturating_sub(..))`
    /// 改成恒 `ABORT_POLL_INTERVAL` → 耗时 ~200ms > 170ms → 本用例红。
    #[tokio::test]
    async fn test_run_bounded_timeout_converges_to_small_deadline() {
        let dir = test_dir("timeout");
        std::fs::create_dir_all(&dir).unwrap();
        let deadline = Duration::from_millis(120);

        let (result, elapsed) = run_guarded(
            // 直接跑 `sleep`（经 PATH 解析），不经 `sh -c`：孙进程会让 deadline 失去硬上界语义。
            "sleep",
            &["30"],
            &dir,
            1024 * 1024,
            deadline,
            None,
            Duration::from_secs(10),
        )
        .await;

        let b = result.expect("超时不是错误（是我们杀的）");
        assert_eq!(b.stop, Stop::TimedOut);
        #[cfg(unix)]
        assert_eq!(b.code, None, "被杀死时退出码为 None");
        assert!(
            elapsed >= deadline,
            "不得早于 deadline 收场，实际 {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(170),
            "超时必须按剩余时间收敛到 deadline（{deadline:?}）；实际 {elapsed:?} —— \
             若 ~200ms 说明收敛被轮询间隔拉长"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Aborted`：运行**期间**投递 `Boundary::Abort` → 被杀，不等 30 秒。
    ///
    /// 变异：删掉主循环里的 `boundary.is_some_and(|b| b.is_aborted()) → break Stop::Aborted`
    /// → 本用例会跑到 deadline 才返回 `TimedOut` → 红。
    #[tokio::test]
    async fn test_run_bounded_abort_mid_run_returns_aborted() {
        let dir = test_dir("abort");
        std::fs::create_dir_all(&dir).unwrap();

        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(Boundary::Abort);
        });
        let src: &dyn BoundarySource = &*boundary;

        let (result, elapsed) = run_guarded(
            "sleep",
            &["30"],
            &dir,
            1024 * 1024,
            Duration::from_secs(10),
            Some(src),
            Duration::from_secs(15),
        )
        .await;

        let b = result.expect("被中止不是错误（是我们杀的）");
        assert_eq!(b.stop, Stop::Aborted);
        #[cfg(unix)]
        assert_eq!(b.code, None, "被杀死时退出码为 None");
        assert!(
            elapsed < Duration::from_secs(5),
            "中止应在 200ms 后的下一次轮询内收场，实际 {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ⑤ 杀的是整个进程组（孙进程不得漏网） ─────────────────────────────

    /// 越限被杀时，**整个进程组**都必须死 —— 只杀直接子进程会让孙进程漏网。
    ///
    /// 形态：`sh -c 'sleep 300 & wait'`。`sh` 是直接子进程，`sleep 300` 是它的**孙进程**，
    /// 与它同组、且继承了 stdout/stderr 的写端。若只杀直接子进程，孙进程会活着攥住管道
    /// 写端 ⇒ 两个排空任务永远等不到 EOF ⇒ 本函数**不返回**（挂到 guard 才 panic）。
    /// 故「本用例能返回」本身就证明孙进程已死，无需再去 `ps` 里找它。
    ///
    /// 线上形态同此：漏网的孙进程会变成 `ppid=1` 的孤儿继续跑 —— 若它的 stdout 指向
    /// 一个仍有人在读的管道（或文件），它会以 ~100% CPU 永久空转。
    ///
    /// 变异：删掉 spawn 处的 `process_group(0)` → 本用例挂起 → 撞满 guard → 红。
    #[tokio::test]
    async fn test_run_bounded_kill_reaps_descendants() {
        let dir = test_dir("descendants");
        std::fs::create_dir_all(&dir).unwrap();

        let (result, elapsed) = run_guarded(
            "sh",
            &["-c", "sleep 300 & wait"],
            &dir,
            1024 * 1024,
            Duration::from_millis(150),
            None,
            Duration::from_secs(10),
        )
        .await;

        let b = result.expect("超时不是错误（是我们杀的）");
        assert_eq!(b.stop, Stop::TimedOut);
        assert!(
            elapsed < Duration::from_secs(5),
            "孙进程存活则排空任务等不到 EOF，本函数只能挂到 guard 才返回；实际 {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ② stderr 狂写不挂起（死锁防护的正身） ─────────────────────────────

    /// 子进程往 stderr 狂写 ~4.7MB：**必须在超时内正常返回**，且只留前 [`STDERR_KEEP`]。
    ///
    /// 构造：`while` 循环在 `sh` **自身进程内**跑（`echo` 是 shell 内建）—— 不 fork 孙进程，
    /// 故杀/收尸语义干净；若用 `sh -c 'yes >&2'` 那类写法，孙进程会攥住管道写端。
    ///
    /// 变异（两种形状，都必须变红）：
    ///
    /// 1. **删掉 stderr 排空任务**（读端 `ChildStderr` 仍是主函数的局部变量、不 drop）
    ///    → 读端不关、管道写满 → `sh` 永久阻塞在 `write()` 上 → 撞满 10s deadline，
    ///    `stop` 变 `TimedOut` → `stop == Eof` 断言红。这才是「不排空 = 双向死锁」的正身。
    /// 2. **让排空任务读满 `STDERR_KEEP` 就 `break`** → 任务结束、读端随之关闭 → 子进程
    ///    并非挂起，而是被 **SIGPIPE 打死**（实测 `stop` 仍是 `Eof`、`stderr.len()` 仍是
    ///    4096，只有 `code` 变 `None`）→ 靠 `code == Some(0)` 断言红。
    ///    **只断言 `stderr.len()` 是抓不住它的** —— 故本用例额外钉住「子进程**自然退出**、
    ///    不是被信号打死」。
    #[tokio::test]
    async fn test_run_bounded_stderr_flood_does_not_hang() {
        let dir = test_dir("stderr_flood");
        std::fs::create_dir_all(&dir).unwrap();
        let script = r#"i=0; while [ $i -lt 100000 ]; do \
echo "stderr padding padding padding padding padding" 1>&2; i=$((i+1)); done"#;

        let (result, elapsed) = run_guarded(
            "sh",
            &["-c", script],
            &dir,
            1024 * 1024,
            Duration::from_secs(10),
            None,
            Duration::from_secs(15),
        )
        .await;

        let b = result.expect("正常退出必须是 Ok");
        assert_eq!(
            b.stop,
            Stop::Eof,
            "stderr 不排空则读端不关、管道写满、子进程卡死在 write() 上 → 变 TimedOut"
        );
        assert_eq!(b.stdout.len(), 0, "本命令不写 stdout");
        assert_eq!(b.stderr.len(), STDERR_KEEP, "只保留前 4 KiB，其余丢弃");
        assert_eq!(
            b.code,
            Some(0),
            "子进程必须自然退出；若为 None 说明它被 SIGPIPE 打死（排空任务提前收手关了读端）"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "狂写 stderr 不应拖到 deadline，实际 {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ④ 护栏：越限后「同一 100ms 窗口内自然退出」→ stop 报 Eof ──────────

    /// 钉住一个**事实**：子进程越限后、在我们下一次轮询（100ms 窗口）之前自然退出时，
    /// `stop` 报 `Eof`，而 `stdout` 已按上限截到 `byte_cap`。
    ///
    /// 复现形状：`head -c 300000 /dev/zero`（30 万字节瞬间写完）+ `byte_cap = 100000`；
    /// 主循环先查 `try_wait` 再看 `capped` 标志，故「已退出」优先于「已越限」。
    ///
    /// **调用方（T7）因此必须用 `stdout.len() >= byte_cap` 判定是否报上限 notice，
    /// 不能依赖 `stop == Cap`** —— 这条路径上 `stop` 是 `Eof` 且 `code == Some(0)`，
    /// 若按 `stop == Cap` 判定就会把「已截断」误报成「完整输出」。
    ///
    /// 变异：把 `if kept.len() >= byte_cap { capped.store(true) }` 删掉 → `stdout.len()` 仍
    /// 是 `byte_cap`（截断在累积处），但 §Cap 用例会红；把截断去掉（`kept.extend_from_slice(&buf[..n])`）
    /// → 本用例 `stdout.len() == byte_cap` 红。
    ///
    /// 注：此处 `sh -c` 产生孙进程 `head`，但**本用例不依赖 deadline 是硬上界** ——
    /// `head` 自己会很快写完退出、管道随之关闭，无悬挂写端。
    #[tokio::test]
    async fn test_run_bounded_cap_then_immediate_exit_reports_eof_with_capped_stdout() {
        let dir = test_dir("cap_race");
        std::fs::create_dir_all(&dir).unwrap();
        let byte_cap = 100_000;

        let (result, _) = run_guarded(
            "sh",
            &["-c", "head -c 300000 /dev/zero"],
            &dir,
            byte_cap,
            Duration::from_secs(10),
            None,
            Duration::from_secs(15),
        )
        .await;

        let b = result.expect("自然退出必须是 Ok");
        assert_eq!(
            b.stop,
            Stop::Eof,
            "越限后在同一轮询窗口内自然退出 → 先观察到的是 try_wait 的 Eof"
        );
        assert_eq!(b.code, Some(0));
        assert_eq!(
            b.stdout.len(),
            byte_cap,
            "输出仍已被截到 byte_cap —— 所以 T7 必须用 stdout.len() >= byte_cap 判定上限，\
             不能依赖 stop == Cap"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ③ N10 探测（纯路径扫描，不 spawn） ────────────────────────────────

    /// 空目录：两后端皆 `None`，且 `degraded_hint()` 有值（含缺什么）。
    ///
    /// 变异：`detect_from` 改成读进程 `PATH`（而非入参）→ 在装有 rg 的机器上变红。
    #[cfg(unix)]
    #[test]
    fn test_detect_from_empty_dir_reports_both_missing_with_hint() {
        let dir = test_dir("detect_empty");
        std::fs::create_dir_all(&dir).unwrap();

        let backends = SearchBackends::detect_from(&dir.to_string_lossy());

        assert_eq!(backends.rg, None);
        assert_eq!(backends.fd, None);
        let hint = backends
            .degraded_hint()
            .expect("两后端皆缺 → 必须有降级提示");
        assert!(hint.contains("rg") && hint.contains("fd"), "提示：{hint}");
        assert!(hint.contains("gitignore"), "提示须点明后果：{hint}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_posix_only_has_no_backends_and_hint() {
        let backends = SearchBackends::posix_only();
        assert_eq!(backends.rg, None);
        assert_eq!(backends.fd, None);
        assert!(backends.degraded_hint().is_some());
    }

    /// 含**可执行** `rg` → `rg.is_some()`；且探测**不 spawn**（假 `rg` 会 touch 哨兵文件）。
    ///
    /// 变异：把 `is_executable_file` 的 spawn 化探测（跑 `rg --version`）→ 哨兵出现 → 红
    /// （ADR-0015：探测是纯路径扫描）。
    #[cfg(unix)]
    #[test]
    fn test_detect_from_finds_executable_rg_without_spawning_it() {
        let dir = test_dir("detect_rg");
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = dir.join("spawned.txt");
        let fake_rg = dir.join("rg");
        std::fs::write(
            &fake_rg,
            format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
        )
        .unwrap();
        set_mode(&fake_rg, 0o755);

        let backends = SearchBackends::detect_from(&dir.to_string_lossy());

        assert_eq!(backends.rg.as_deref(), Some(fake_rg.as_path()));
        assert_eq!(backends.fd, None, "目录里没有 fd/fdfind");
        assert!(
            !sentinel.exists(),
            "探测不得 spawn 目标二进制（哨兵 {} 被创建了）",
            sentinel.display()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 含**非可执行** `fdfind` → 不认（`None`）。
    ///
    /// 变异：把 `is_executable_file` 的执行位判断简化为 `path.exists()` → 本用例红。
    #[cfg(unix)]
    #[test]
    fn test_detect_from_ignores_non_executable_fdfind() {
        let dir = test_dir("detect_noexec");
        std::fs::create_dir_all(&dir).unwrap();
        let fdfind = dir.join("fdfind");
        std::fs::write(&fdfind, "not executable\n").unwrap();
        set_mode(&fdfind, 0o644);

        let backends = SearchBackends::detect_from(&dir.to_string_lossy());

        assert_eq!(backends.fd, None, "没有可执行位 → 不算找到 fd");
        assert_eq!(backends.rg, None);
        assert!(backends.degraded_hint().is_some(), "两者皆缺 → 有提示");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两后端都命中 → `degraded_hint()` 为 `None`（无提示即无噪声）。
    #[cfg(unix)]
    #[test]
    fn test_detect_from_both_backends_present_yields_no_hint() {
        let dir = test_dir("detect_both");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["rg", "fd"] {
            let p = dir.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            set_mode(&p, 0o755);
        }

        let backends = SearchBackends::detect_from(&dir.to_string_lossy());

        assert!(backends.rg.is_some());
        assert!(backends.fd.is_some());
        assert_eq!(backends.degraded_hint(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 设文件权限位（`std::os::unix::fs::PermissionsExt`，不引依赖）。
    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).unwrap();
    }
}
