//! **N11 —— CLI 降级路径**：`PATH` 上没有 `rg` / `fd` 时，app 启动期必须把
//! [`ys_tools_basic::exec::SearchBackends::degraded_hint`] 的提示打到 **stderr**，
//! 且**不得污染 `--json` 的 stdout 纯 JSON 流**（输出三分，见设计 §5）。
//!
//! # 为什么断言「stdout 为空」
//!
//! `--json` 的 stdout 是**逐行 `Envelope`**：任何一行非 JSON 都会让下游解析器崩。
//! 只断言「stderr 有提示」抓不住「提示被写到了 stdout」这类变异（T11 review 点名的
//! 覆盖缺口）—— 故必须有第二条断言把两个流**分开**钉住。
//!
//! # 子进程 env 隔离
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` 注入，**不改父进程
//! env**（与 `stream_usage_fallback.rs` 同规）：改写进程级 env 会与同 binary 内并行
//! 测试的 env 读取产生数据竞争。
//!
//! # 挂起即失败
//!
//! 无模型配置时进程应**快速退出**（打到 `No model configured` 即 `exit(1)`）。命令
//! 一律套 [`CHILD_TIMEOUT`]：挂起必须表现为 panic，而不是测试永不结束。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 子进程退出上限。正常路径毫秒级返回；超时 = 挂起 = panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全）。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_degraded_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 跑一次 `ys-coding-agent --json x`（`PATH` 只含 `path_dir`），**套 timeout**。
///
/// 挂起 → panic，而非测试挂死。env 全部 per-child 注入，不碰父进程。
fn run_guarded(path_dir: &Path, home: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    cmd.args(["--json", "x"]);
    isolate(&mut cmd, home);
    cmd.env("PATH", path_dir);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output());
    });

    match rx.recv_timeout(CHILD_TIMEOUT) {
        Ok(out) => out.expect("spawn ys-coding-agent"),
        Err(_) => panic!(
            "ys-coding-agent 在 {CHILD_TIMEOUT:?} 内未退出 = 挂起（必须表现为失败，而非测试挂死）"
        ),
    }
}

/// 所有用例共用的隔离：`HOME` 指向空目录（无 `auth.json` / `state.json`），
/// 清掉 `YUSHAN_*`（否则父进程若有凭证，进程就不会走到「无模型配置」的早退）。
fn isolate(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home)
        .env_remove("USERPROFILE")
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        .env_remove("YUSHAN_SESSIONS_DIR")
        .env_remove("YUSHAN_CHANNEL_CAPACITY");
}

/// 在 `dir` 里放一个「能被探测认出」的假后端：文件 + 可执行位。
///
/// 探测是**纯路径扫描**（不 spawn，ADR-0015），故内容不必真能跑 —— 本用例因此
/// 不依赖机器上装了 ripgrep。
#[cfg(unix)]
fn write_fake_backend(dir: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// **降级主用例**：`PATH` 只剩一个**空**目录 → 两后端皆缺。
///
/// 断言两件事（缺一不可）：
/// 1. **stdout 为空** —— 提示**没有**污染 `--json` 的纯 JSON 流（先断言它，让
///    `eprintln!` → `println!` 变异直接撞在这条上，见下）。
/// 2. **stderr 含降级提示** —— 含 `rg` / `fd` / `.gitignore` 关键片段（内容断言，
///    而非 `is_some()`：硬编码文案或漏掉后果句都会红）。
///
/// 变异：`main.rs` 把降级提示的 `eprintln!` 改成 `println!` → 断言 1 变红。
#[test]
fn cli_degraded_path_hints_to_stderr_not_stdout() {
    let path_dir = unique_dir("empty_path");
    let home = unique_dir("home");

    let out = run_guarded(&path_dir, &home);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // 1. **stdout 必须为空** —— 提示不得混进 `--json` 的 JSON 流。
    //    先断言这条：它是本用例相对「只看 stderr」多出来的那半覆盖，
    //    也是 `eprintln!` → `println!` 变异**唯一**会撞上的断言。
    assert!(
        stdout.is_empty(),
        "降级提示/任何诊断都不得写 stdout（--json 流是逐行 Envelope）；stdout:\n{stdout:?}"
    );

    // 2. stderr 有降级提示（内容片段，逐字来自 `degraded_hint()`）。
    assert!(
        stderr.contains("rg"),
        "stderr 应点名缺 rg；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("fd"),
        "stderr 应点名缺 fd；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(".gitignore"),
        "stderr 应说明后果（POSIX 回退不读 .gitignore）；stderr:\n{stderr}"
    );

    // 前端（无模型配置）应已正常走到早退：证明提示确实来自**启动期**而非别处。
    assert!(!out.status.success(), "无模型配置应非零退出");
    assert!(
        stderr.contains("No model configured"),
        "进程应走到「无模型配置」早退；stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&path_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// **对照**：`PATH` 上放齐假 `rg` 与 `fd` → **stderr 不含**该提示（无降级即零噪声），
/// 且 stdout 同样为空。
///
/// 用**假二进制**（而非继承父进程 `PATH`）是为了不依赖开发机装没装 ripgrep：
/// 与主用例唯一被改的变量就是「PATH 上有没有后端」，对照才是干净的。
#[cfg(unix)]
#[test]
fn cli_no_hint_when_backends_present() {
    let bin_dir = unique_dir("with_backends");
    let home = unique_dir("home");
    write_fake_backend(&bin_dir, "rg");
    write_fake_backend(&bin_dir, "fd");

    let out = run_guarded(&bin_dir, &home);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("not found on PATH"),
        "后端齐全时不得出现降级提示；stderr:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "无模型配置的 --json 运行不得写 stdout；stdout:\n{stdout:?}"
    );
    assert!(
        stderr.contains("No model configured"),
        "对照用例同样应走到早退；stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&bin_dir);
    let _ = std::fs::remove_dir_all(&home);
}
