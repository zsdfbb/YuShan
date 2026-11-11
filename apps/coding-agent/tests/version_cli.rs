//! **`--version` 的子进程端到端**：进程级契约（exit 0 / stdout 恰好一行 `<bin> <version>` /
//! stderr 逐字节为空）、以及「早退在 debug sink 与 runtime 建立之前」两条护栏。
//!
//! 实现侧（`src/main.rs` 的 `Mode::Version` 早退点）的**单测**只覆盖 `parse_args` 的模式判定；
//! 「stderr 一个字节都不打」「不建 debug sink 也不留任何日志文件」「无凭证仍 exit 0」这三条
//! **只有真起子进程**才钉得住 —— 把 `println!` 换成 `eprintln!`、或把早退点挪到 debug sink /
//! runtime 建立之后，单测仍然全绿，这里立刻变红。
//!
//! **守卫点不在 `Config::from_env()` 自身的失败路径上**：它不校验凭证（凭证缺失的报错出自
//! 后面的 `ensure_configured`），故「早退点移到 `Config::from_env()` 之后」在 stdout / exit code /
//! stderr 上都不可见。它的唯一可观测副作用是**日志文件**：后移后，非法的
//! `YUSHAN_REASONING_EFFORT` 会让 `parse_reasoning_effort` 经 `log_line` 落一行
//! `$YUSHAN_LOG_DIR/yushan.log`（早退分支下该目录零文件）—— T2 刻意注入该非法值并断言目录
//! **完全为空**，就是钉这条。
//!
//! 本文件的结构（隔离根 / 每子进程 env / timeout / 并行安全目录命名）与 `help_cli.rs` 同规，
//! 因为要守的是**同一类**契约：两个早退分支都不建 runtime、不读凭证、只往 stdout 写。
//!
//! # 子进程 env 隔离
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` 注入，**绝不改父进程 env**
//! （改写进程级 env 会与同 binary 内并行测试产生数据竞争）。`HOME` 指向空临时目录
//! （`auth.json` / `state.json` / 用户级 skill 根都落这里），`YUSHAN_SESSIONS_DIR` 显式指向
//! 临时会话目录。
//!
//! # 挂起即失败
//!
//! 所有用例套 [`CHILD_TIMEOUT`]：挂起必须是 panic（带已捕获输出），而非测试挂死。

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 子进程退出上限。`--version` 是**毫秒级纯打印**，60s 只是防挂起的兜底。
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// 子进程的 debug 环境。所有命令都先移除 `YUSHAN_LOG_DIR`，再由用例显式选择。
#[derive(Clone, Copy)]
enum DebugEnv {
    Off,
    Unset,
}

/// `--version` 输出的固定前缀：`<bin> <version>`，版本来自 `CARGO_PKG_VERSION`。
const VERSION_PREFIX: &str = "ys-coding-agent ";

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全，项目硬约定）。
///
/// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删 —— 故用计数器。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_version_cli_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 每个用例一套隔离根：临时 workspace / HOME / 会话目录 / 日志目录，`Drop` 时清理。
struct Roots {
    workspace: PathBuf,
    home: PathBuf,
    sessions_dir: PathBuf,
    logs_dir: PathBuf,
}

impl Roots {
    fn new(tag: &str) -> Self {
        let roots = Self {
            workspace: unique_dir(&format!("{tag}_ws")),
            home: unique_dir(&format!("{tag}_home")),
            sessions_dir: unique_dir(&format!("{tag}_sessions")),
            logs_dir: unique_dir(&format!("{tag}_logs")),
        };
        // 「无凭证」是本文件全部用例的前提：隔离 HOME 必须真的是空的，
        // 否则从上一用例漏进来的 auth.json 会让「无凭证也能跑」这条断言失去意义。
        assert!(
            !roots.home.join("auth.json").exists(),
            "隔离 HOME 下不应有 auth.json（会破坏「无凭证」前提）：{}",
            roots.home.display()
        );
        roots
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.sessions_dir);
        let _ = std::fs::remove_dir_all(&self.logs_dir);
    }
}

/// 已套好隔离的被测二进制命令（**per-child env，不碰父进程**）。
///
/// 模型侧与 web 侧凭证全部清除：`--version` 的早退必须在任何凭证读取之前，
/// 带着开发机 shell 的凭证跑就测不出这条契约。
fn isolated_command_with_debug(roots: &Roots, debug_env: DebugEnv) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    cmd.current_dir(&roots.workspace)
        .env("HOME", &roots.home)
        .env_remove("USERPROFILE")
        .env("YUSHAN_SESSIONS_DIR", &roots.sessions_dir)
        .env_remove("YUSHAN_LOG_DIR")
        // 模型侧
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        .env_remove("OPENAI_API_BASE")
        .env_remove("OPENAI_API_KEY")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        // web 侧
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_DEEPSEEK_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("YUSHAN_FETCHED_DIR");
    match debug_env {
        DebugEnv::Off => {
            cmd.env("YUSHAN_DEBUG", "0");
        }
        DebugEnv::Unset => {
            cmd.env_remove("YUSHAN_DEBUG");
        }
    }
    cmd
}

/// 跑一次被测二进制（**套 timeout**），不做退出码断言。
fn run_guarded_output(argv: &[&str], roots: &Roots, debug_env: DebugEnv) -> Output {
    let mut cmd = isolated_command_with_debug(roots, debug_env);
    cmd.args(argv);
    if matches!(debug_env, DebugEnv::Unset) {
        cmd.env("YUSHAN_LOG_DIR", &roots.logs_dir);
    }
    run_guarded_cmd(cmd, argv)
}

fn run_guarded_cmd(mut cmd: Command, argv: &[&str]) -> Output {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output());
    });
    match rx.recv_timeout(CHILD_TIMEOUT) {
        Ok(out) => out.expect("spawn ys-coding-agent"),
        Err(_) => panic!(
            "ys-coding-agent {argv:?} 在 {CHILD_TIMEOUT:?} 内未退出 = 挂起\
             （必须表现为失败，而非测试挂死）"
        ),
    }
}

/// stderr / stdout 的 lossy 解码（子进程输出可能含非 UTF-8，绝不 panic）。
fn streams(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 列出目录下的**全部**条目标名（含子目录名），目录不可读时返回空。
///
/// 查全部条目而非只查 `debug-*.jsonl`：早退后移时留下的是 `yushan.log`
/// （`parse_reasoning_effort` 的诊断），只匹配 debug 文件会放过它。
fn all_entries(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect()
}

/// T1：`--version` ⇒ **exit 0 + stdout 一行 `<bin> <version>` + stderr 逐字节为空**。
///
/// 直接比字节而不是解码后 trim：stderr 少一个空行、stdout 少一个换行，都在这里现形。
#[test]
fn version_exits_zero_with_exact_stdout_and_empty_stderr() {
    let roots = Roots::new("t1");
    let out = run_guarded_output(&["--version"], &roots, DebugEnv::Off);
    let (stdout, stderr) = streams(&out);

    assert!(
        out.status.success(),
        "`--version` 应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.is_empty(),
        "`--version` 的 stderr 应逐字节为空，实际：\n{stderr}"
    );
    assert!(!out.stdout.is_empty(), "`--version` 的 stdout 应非空");

    let text = stdout.strip_suffix('\n').unwrap_or_else(|| {
        panic!("`--version` 的 stdout 应以恰好一个换行结尾：{stdout:?}");
    });
    assert!(
        !text.ends_with('\n'),
        "`--version` 的 stdout 应恰好一行（不得有双换行）：{stdout:?}"
    );
    assert!(
        !text.ends_with('\r'),
        "`--version` 的 stdout 不得以 CRLF 结尾：{stdout:?}"
    );
    assert_eq!(
        text,
        format!("{VERSION_PREFIX}{}", env!("CARGO_PKG_VERSION")),
        "`--version` 应打印 `<bin> <version>`"
    );
}

/// T2：默认 debug 开启（`YUSHAN_DEBUG` 未设置）+ 已显式隔离 `YUSHAN_LOG_DIR` 时，
/// `--version` 早退**不得**在日志目录里留下**任何**条目。
///
/// 早退发生在 debug sink 建立之前 ⇒ 隔离的日志目录必须一个条目都没有；同时检查 HOME 下的
/// 常规日志位置，确认 Override 没有被绕过。
///
/// **为什么查「完全为空」而不是只查 `debug-*.jsonl`**：早退点后移（移到 `Config::from_env()`
/// 之后）不改变 stdout / exit code，但 `Config::from_env()` 里的 `parse_reasoning_effort`
/// 会经 `log_line` 落一行 `$YUSHAN_LOG_DIR/yushan.log` —— 只匹配 `debug-*.jsonl` 看不见它，
/// 漏掉这个文件就等于放过了「早退晚了」的唯一可观测信号。
#[test]
fn version_early_exit_creates_no_debug_files_with_default_debug_on() {
    let roots = Roots::new("t2");
    let mut cmd = isolated_command_with_debug(&roots, DebugEnv::Unset);
    cmd.env("YUSHAN_LOG_DIR", &roots.logs_dir)
        .env("YUSHAN_REASONING_EFFORT", "bogus")
        .arg("--version");
    let out = run_guarded_cmd(cmd, &["--version"]);
    let (stdout, stderr) = streams(&out);
    assert!(
        out.status.success(),
        "`--version` 应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.is_empty(),
        "`--version` 的 stderr 应为空，实际：\n{stderr}"
    );

    let entries = all_entries(&roots.logs_dir);
    assert!(
        entries.is_empty(),
        "`--version` 早退不得在 YUSHAN_LOG_DIR 下留下任何条目（含 yushan.log）：{entries:?}"
    );
    let home_entries = all_entries(&roots.home.join(".yushan").join("logs"));
    assert!(
        home_entries.is_empty(),
        "`--version` 早退不得绕过 Override 写进 HOME：{home_entries:?}"
    );
}

/// T1b：无凭证环境下仍 exit 0 且打出同一行。
///
/// `--version` 的早退发生在凭证校验（`ensure_configured`）之前；凭证缺失的报错来自后者，
/// 故凭证被清空也不影响本路径。
#[test]
fn version_runs_without_any_credentials() {
    let roots = Roots::new("t1b");
    // 前提自查：注入的命令不得**带值**携带模型凭证（`env_remove` 不传值）。
    let probe = isolated_command_with_debug(&roots, DebugEnv::Off);
    assert!(
        !probe
            .get_envs()
            .any(|(k, v)| { v.is_some() && (k == "YUSHAN_API_BASE" || k == "YUSHAN_API_KEY") }),
        "前提自查：注入的命令不得**带值**携带 YUSHAN_API_BASE / YUSHAN_API_KEY"
    );

    let out = run_guarded_output(&["--version"], &roots, DebugEnv::Off);
    let (stdout, stderr) = streams(&out);
    assert!(
        out.status.success(),
        "无凭证时 `--version` 仍应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.starts_with(VERSION_PREFIX),
        "无凭证时 stdout 应仍是版本行（未被凭证缺失的报错顶替），实际：\n{stdout}"
    );
    assert!(
        stderr.is_empty(),
        "无凭证时 stderr 应为空（早退在配置读取之前），实际：\n{stderr}"
    );
}
