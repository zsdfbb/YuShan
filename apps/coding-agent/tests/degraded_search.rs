//! **N11 —— CLI 降级路径**：app 启动期会把**两条**降级提示合并成一段打到 **stderr**
//! （`toolset::merge_hints`，详见设计 §1.1）：
//!
//! ① 搜索后端侧 —— `PATH` 上没有 `rg` / `fd` 时
//!    [`ys_tools_basic::exec::SearchBackends::degraded_hint`] 产出；
//! ② web 侧 —— `WebConfig::degraded_hint`（未配搜索后端 / HTTP 栈不可用 / 落盘目录降级）。
//!
//! 二者**绝不互相覆盖**，且**不得污染 `--json` 的 stdout 纯 JSON 流**（输出三分，见设计 §5）。
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
//! 测试的 env 读取产生数据竞争。被清的键分**模型侧**与 **web 侧**两组 ——
//! 后者决定上面 ② 出不出现，**不清就会随开发机 shell 漂移**（见 [`isolate`]）。
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
///
/// # 两类被清的变量（都决定「stderr 上会出现什么」，不清则测试随开发机漂移）
///
/// ① **模型侧**（`API_BASE` / `API_KEY` / `MODEL` / `SESSIONS_DIR` / `CHANNEL_CAPACITY`）——
///    父进程若配了凭证，子进程就不会走到「无模型配置」的早退（本文件的两个早退断言失效）。
/// ② **web 侧**（四个 `YUSHAN_SEARCH_*` + Tavily 通用名 `TAVILY_API_KEY` + `YUSHAN_FETCHED_DIR`）—— 这些后端键决定
///    `WebConfig::from_env` 解析出的链是否为空，进而决定
///    [`ys_tools_web::WebConfig::degraded_hint`] **有没有**「未配置任何搜索后端」那句；
///    `YUSHAN_FETCHED_DIR` 决定落盘目录的**档位**（`Override` vs `Primary`）与是否附带
///    一条降级说明。开发机 shell 里配过 Tavily key 时，web 侧提示会**整个消失** ——
///    任何关于它的断言都会时红时绿。故这些键（含通用别名）必须一并在 per-child 上清掉。
///
/// 键名逐字取自 `adapters/tools-web/src/lib.rs` 顶部的 `ENV_*` 常量（**不猜**）。
fn isolate(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home)
        .env_remove("USERPROFILE")
        // ① 模型侧：不清则不走「无模型配置」早退。
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        .env_remove("YUSHAN_SESSIONS_DIR")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        // ② web 侧：不清则「未配置任何搜索后端」那句随开发机 env 有无而漂移。
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        // Tavily 的生态通用名：产品会作为回退读取，不清则开发机 shell 里配过它就漂移。
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("YUSHAN_FETCHED_DIR");
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
/// 本用例是 **CLI 级**对「两条 hint 合并后都打到 stderr」的唯一覆盖：`toolset.rs`
/// 的单测覆盖 `merge_hints`，但 `main.rs` 的 `eprintln!("{hint}")` 这条**生产路径**
/// 只有这里会走。故既钉搜索后端侧，也钉 web 侧（两侧都缺 → 合并成一段）。
///
/// 断言三件事（缺一不可）：
/// 1. **stdout 为空** —— 提示**没有**污染 `--json` 的纯 JSON 流（先断言它，让
///    `eprintln!` → `println!` 变异直接撞在这条上，见下）。
/// 2. **stderr 含搜索后端侧降级提示** —— 含 `rg` / `fd` / `.gitignore` 关键片段
///    （内容断言，而非 `is_some()`：硬编码文案或漏掉后果句都会红）。
/// 3. **stderr 同时含 web 侧降级提示** —— `HOME` 指向空目录且四个 `YUSHAN_SEARCH_*`
///    已清 → 链必空 → 「未配置任何搜索后端」必出现。它与 ②**在同一段 stderr 里
///    并存**，这正是 `merge_hints` 的 `(Some, Some)` 分支不被「后者覆盖前者」的证明。
///
/// 变异靶子：
/// - `main.rs` 把降级提示的 `eprintln!` 改成 `println!` → 断言 1 变红。
/// - `merge_hints` 的 `(Some, Some)` 分支只保留一侧（覆盖另一侧）→ 断言 2 或 3 变红
///   （`toolset.rs` 的单测也盯着这条，此处是**生产路径**上的第二道哨兵）。
/// - `isolate()` 里去掉某个 `YUSHAN_SEARCH_*` 的 `env_remove` → 开发机配了对应 key 时
///   断言 3 变红（见文件顶部隔离说明）。
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

    // 2. stderr 有搜索后端侧降级提示（内容片段，逐字来自 `degraded_hint()`）。
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

    // 3. stderr 里**同时**有 web 侧那句（逐字来自 `WebConfig::degraded_hint`）。
    //    四个后端 env 已清 + client 正常 → 这是 web 侧**唯一**会出现的 issue。
    assert!(
        stderr.contains("未配置任何搜索后端"),
        "stderr 应含 web 侧的「未配置任何搜索后端」（两条 hint 必须合并且都到 stderr）；\
         stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("YUSHAN_SEARCH_SEARXNG_URL"),
        "web 侧提示要能回答「下一步配什么」（点名 env）；stderr:\n{stderr}"
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

/// **对照**：`PATH` 上放齐假 `rg` 与 `fd` → **搜索后端那一条**不出现；
/// 但 **web 侧那条仍然出现**（本用例没配任何搜索后端 → 链必空），故本用例
/// **不是**「完全无提示」—— 叫 `no_hint_when_backends_present` 是误导，已改名。
///
/// 这里把两侧都钉死，是为了让「唯一被改的变量」这件事**可证**：
/// 与主用例相比只动了 `PATH`（有没有 `rg`/`fd`），于是 stderr 上消失的**只能**
/// 是搜索后端那条；web 那条必须**原地不动**。
///
/// 变异靶子：
/// - 把本用例的 `PATH` 换成空目录（等价于主用例）→ 第一条断言变红。
/// - `toolset.rs` 的 `merge_hints` 让 web 侧被搜索后端侧覆盖（或反之）→ 第二或
///   第三条断言变红。
///
/// 用**假二进制**（而非继承父进程 `PATH`）是为了不依赖开发机装没装 ripgrep：
/// 与主用例唯一被改的变量就是「PATH 上有没有后端」，对照才是干净的。
#[cfg(unix)]
#[test]
fn cli_no_search_hint_when_backends_present_but_web_hint_stays() {
    let bin_dir = unique_dir("with_backends");
    let home = unique_dir("home");
    write_fake_backend(&bin_dir, "rg");
    write_fake_backend(&bin_dir, "fd");

    let out = run_guarded(&bin_dir, &home);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // ① 搜索后端侧：rg/fd 都在 PATH 上 → `SearchBackends::degraded_hint()` 为 `None`，
    //    那句「not found on PATH」**不得**出现。
    assert!(
        !stderr.contains("not found on PATH"),
        "搜索后端齐全时不得出现后端侧降级提示；stderr:\n{stderr}"
    );
    // ② web 侧：四个 `YUSHAN_SEARCH_*` 已被 `isolate()` 清掉 → 链必空 →
    //    「未配置任何搜索后端」**必须出现**。它证明「唯一被改的变量只有搜索后端」
    //    （web 侧的状态在主用例与本用例里完全相同，故它消失与否**不能**被归因于 PATH）。
    assert!(
        stderr.contains("未配置任何搜索后端"),
        "本用例未配任何搜索后端 → web 侧提示必须仍在（否则「对照」会退化成\
         「完全无提示」，断言就失去意义）；stderr:\n{stderr}"
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
