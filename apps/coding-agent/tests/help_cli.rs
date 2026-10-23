//! **`--help` 的子进程端到端**：三种触发形态的进程级契约（exit 0 / stdout 非空 / stderr 空、
//! 三者逐字节一致、无凭证仍可跑）、以及「不该打印帮助」的两条回归护栏。
//!
//! 实现侧（`src/help.rs` 的 `help_text()` 与 `src/main.rs` 里的早退点）的**单测**覆盖了
//! `parse_args` 的模式判定与文案内容；单测跑在 `parse_args` 这一层，**到不了进程边界**：
//! 「三个形态输出逐字节相同」「stderr 一个字节都不打」「无凭证环境仍 exit 0」这三条
//! 只有真起一个子进程才钉得住 —— 任何一处把 `println!` 换成 `eprintln!`、或把早退点
//! 挪到 `Config::from_env()` 之后，进程边界上的差异立刻变红，而单测仍然全绿。
//!
//! # 早退点在 `parse_args` 之后、`Config::from_env()` 之前
//!
//! help 的早退（`src/main.rs::main`）紧跟 `parse_args`，因此本文件全程在**清空全部凭证**
//! 的环境里跑通 —— 这既是 T5 要守的契约，也是本文件能离线跑的原因。
//!
//! # 「帮助特征串」的选法（一条实测结论）
//!
//! T6 / T7 要断言「**没有**打印帮助」。直觉会选 `YUSHAN_API_BASE`，但它**不能**当标记：
//! 无凭证早退路径打的是
//! `Error: No model configured.` + `Set environment variables: YUSHAN_API_BASE — …`，
//! 那条报错**本身就会命中**这个串，断言会假红。
//! 故本文件统一用**只在 help 文案里出现**的串作标记（[`HELP_MARKERS`]），首推
//! `"斜杠命令"`：它出自 TUI 斜杠命令段落的引导句，任何正常启动路径都不会打它。
//!
//! # 子进程 env 隔离
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` 注入，
//! **绝不改父进程 env**（`crate::test_env` 只在 bin crate 的 `#[cfg(test)]` 里编译，
//! 集成测试用不到；且改写进程级 env 会与同 binary 内并行测试产生数据竞争）。
//! `HOME` 指向空临时目录（`auth.json` / `state.json` / 用户级 skill 根都落这里），
//! `YUSHAN_SESSIONS_DIR` 显式指向临时会话目录。
//!
//! # 挂起即失败
//!
//! 所有用例套 [`CHILD_TIMEOUT`]：挂起必须是 panic（带已捕获输出），而非测试挂死。

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 子进程退出上限。help 是**秒级纯打印**；T6 / T7 走正常启动路径（会扫工具 / 提示词 /
/// 校验 worktree 名），故取 60s 上限而非 help 自身的耗时。
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// 子进程的 debug 环境。所有命令都先移除 `YUSHAN_LOG_DIR`，再由用例显式选择。
#[derive(Clone, Copy)]
enum DebugEnv {
    Off,
    Unset,
}

/// 三种触发形态 → 子进程 argv（`src/main.rs::parse_args` 的**首位置**判定）。
const TRIGGERS: [(&str, &[&str]); 3] = [
    ("--help", &["--help"]),
    ("-h", &["-h"]),
    ("help", &["help"]),
];

/// 「确实打印了帮助文档」的特征串 —— **每一条都只出现在 help 文案里**。
///
/// 见文件头「帮助特征串的选法」：`YUSHAN_API_BASE` 在无凭证报错里也会出现，
/// 故不能作标记。每条都逐个 `contains` 检查（不是 `any`），任一条命中即判定为打印了帮助。
const HELP_MARKERS: [&str; 4] = [
    "斜杠命令",
    "轻量、组件化的 Agent 运行时",
    "/effort",
    "TUI 斜杠命令",
];

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全，项目硬约定）。
///
/// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删 —— 故用计数器。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_help_cli_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 每个用例一套隔离根：临时 workspace / HOME / 会话目录，`Drop` 时清理。
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
/// 模型侧凭证必须清干净：否则开发机 shell 里的凭证会让 T6 / T7 走到真实网络请求，
/// 既慢又不确定 —— 本文件全部用例都要求「无凭证」这一前提。
fn isolated_command(roots: &Roots) -> Command {
    isolated_command_with_debug(roots, DebugEnv::Off)
}

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

/// 跑一次被测二进制（**套 timeout**）。挂起 → panic（带已捕获输出），而非测试挂死。
fn run_guarded(argv: &[&str], roots: &Roots) -> String {
    let out = run_guarded_output(argv, roots);
    let name = argv.join(" ");
    let (stdout, stderr) = streams(&out);
    assert!(
        out.status.success(),
        "ys-coding-agent {name:?} 应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    stdout
}

/// 跑一次被测二进制（**套 timeout**），不做退出码断言 —— 供 T6 / T7 用
/// （那两条走正常启动路径，退出码本就非 0）。
fn run_guarded_output(argv: &[&str], roots: &Roots) -> Output {
    let mut cmd = isolated_command(roots);
    cmd.args(argv);
    run_guarded_cmd(cmd, argv)
}

fn run_guarded_output_with_debug(argv: &[&str], roots: &Roots, debug_env: DebugEnv) -> Output {
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

/// 命中的帮助特征串列表（空 = 没有打印帮助）。
fn hits_help_markers(text: &str) -> Vec<&'static str> {
    HELP_MARKERS
        .iter()
        .filter(|m| text.contains(**m))
        .copied()
        .collect()
}

fn debug_jsonl_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("debug-") && name.ends_with(".jsonl"))
        })
        .collect()
}

/// T3：三种触发形态都 **exit 0 + stdout 非空 + stderr 空**。
///
/// 这是 help 的基础进程级契约 —— 输出走 stdout（数据流），stderr 一个字节都不打。
#[test]
fn three_trigger_shapes_exit_zero_with_stderr_empty() {
    for (name, argv) in TRIGGERS {
        let roots = Roots::new(&format!("t3_{name}"));
        let out = run_guarded_output(argv, &roots);
        let (stdout, stderr) = streams(&out);
        assert!(
            out.status.success(),
            "{name:?} 应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        assert!(
            !stdout.trim().is_empty(),
            "{name:?} 的 stdout 应非空\n--- stdout ---\n{stdout}"
        );
        assert!(
            stderr.is_empty(),
            "{name:?} 的 stderr 应为空（帮助走 stdout），实际：\n{stderr}"
        );
    }
}

/// T4：三种形态的 stdout **逐字节一致**。
///
/// 直接比字节（不做解码再比）：`println!` 加一个 `\n`，任何形态的换行差异都会在这里现形。
#[test]
fn three_trigger_shapes_emit_byte_identical_stdout() {
    let mut captured: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for (name, argv) in TRIGGERS {
        let roots = Roots::new(&format!("t4_{name}"));
        let out = run_guarded_output(argv, &roots);
        assert!(
            out.status.success(),
            "{name:?} 应 exit 0\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        captured.push((name, out.stdout.clone()));
    }
    let ((first, first_bytes), rest) = captured.split_first().expect("TRIGGERS 非空");
    assert!(
        first_bytes.ends_with(b"\n"),
        "{first:?} 的 stdout 应以一个换行结尾"
    );
    assert!(
        !first_bytes.ends_with(b"\r\n"),
        "{first:?} 的 stdout 不得以 CRLF 结尾"
    );
    assert!(
        !first_bytes.ends_with(b"\n\n"),
        "{first:?} 的 stdout 不得以双换行结尾"
    );
    for (name, bytes) in rest {
        assert!(
            bytes.ends_with(b"\n") && !bytes.ends_with(b"\n\n"),
            "{name:?} 的 stdout 应恰以一个换行结尾"
        );
        assert!(
            !bytes.ends_with(b"\r\n"),
            "{name:?} 的 stdout 不得以 CRLF 结尾"
        );
        assert_eq!(
            first_bytes, bytes,
            "{first:?} 与 {name:?} 的 stdout 应逐字节一致（帮助是纯文本常量，无形态差异）"
        );
    }
}

/// help 在 sink 建立前早退：即使 `YUSHAN_DEBUG` 未设置（默认开启）且日志目录
/// 已显式隔离，也不得创建任何 debug 文件。
#[test]
fn help_early_exit_creates_no_debug_files_with_default_debug_on() {
    let roots = Roots::new("help_default_debug");
    let out = run_guarded_output_with_debug(&["--help"], &roots, DebugEnv::Unset);
    let (stdout, stderr) = streams(&out);
    assert!(
        out.status.success(),
        "`--help` 应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.is_empty(),
        "`--help` 的 stderr 应为空，实际：\n{stderr}"
    );

    let debug_files = debug_jsonl_files(&roots.logs_dir);
    assert!(
        debug_files.is_empty(),
        "help 早退不得建立 debug 文件：{debug_files:?}"
    );
    let home_debug_files = debug_jsonl_files(&roots.home.join(".yushan").join("logs"));
    assert!(
        home_debug_files.is_empty(),
        "help 早退不得绕过 Override 写进 HOME：{home_debug_files:?}"
    );
}

/// T5：无凭证环境下仍能跑通 —— **help 纯打印契约本身**。
///
/// 早退点在 `Config::from_env()` 之前，故 `YUSHAN_API_BASE` / `YUSHAN_API_KEY` 全清
/// 也必须 exit 0 且打出完整帮助；若早退点后移，这里会变成
/// `Error: No model configured.` + 非零退出。
#[test]
fn runs_without_any_credentials() {
    let roots = Roots::new("t5");
    let out = run_guarded_output(&["--help"], &roots);
    let (stdout, stderr) = streams(&out);
    assert!(
        out.status.success(),
        "无凭证时 `--help` 仍应 exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "无凭证时 stdout 应非空\n--- stdout ---\n{stdout}"
    );
    assert!(
        stderr.is_empty(),
        "无凭证时 stderr 应为空（早退在配置读取之前），实际：\n{stderr}"
    );
    assert!(
        hits_help_markers(&stdout).len() == HELP_MARKERS.len(),
        "无凭证时 stdout 应仍是完整帮助（未被凭证缺失的报错顶替），\
         实际只命中 {:?}",
        hits_help_markers(&stdout)
    );
    // 前提自查：确认这批隔离项**确实**对子进程生效（`env_remove` 不传值）——
    // 用 set 出来的命令会带着空值项，判别方式是有值即为携带。
    let probe = isolated_command(&roots);
    assert!(
        !probe
            .get_envs()
            .any(|(k, v)| { v.is_some() && (k == "YUSHAN_API_BASE" || k == "YUSHAN_API_KEY") }),
        "前提自查：注入的命令不得**带值**携带 YUSHAN_API_BASE / YUSHAN_API_KEY"
    );
}

/// T6：回归护栏 —— `-p --help me` **不得**打印帮助。
///
/// `--help` 只在**首个参数位置**判定模式 flag；跟在 `-p` 之后时它就是任务文本的一部分。
/// 本用例**不要求 exit 0**：无凭证时它会以 `Error: No model configured.` 失败，那是预期的；
/// 要守的只有「**没有**打印帮助」。若哪天 `parse_args` 改成全局搜 `--help`，
/// 这里会打印出整篇帮助并立刻变红。
#[test]
fn print_mode_task_text_named_help_does_not_print_help() {
    let roots = Roots::new("t6");
    let out = run_guarded_output(&["-p", "--help", "me"], &roots);
    let (stdout, stderr) = streams(&out);

    let hits_out = hits_help_markers(&stdout);
    let hits_err = hits_help_markers(&stderr);
    assert!(
        hits_out.is_empty() && hits_err.is_empty(),
        "`-p --help me` 把 `--help` 当任务文本，不得打印帮助文档。\n\
         stdout 命中特征串：{hits_out:?}\n\
         stderr 命中特征串：{hits_err:?}\n\
         --- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    );
    // 顺带把「非 0 退出」这条既有语义写进断言：不是断言具体失败原因（那属实现细节），
    // 而是确保本用例真的走的是正常启动路径，而不是悄悄 help 早退了。
    assert!(
        !out.status.success(),
        "无凭证下 `-p --help me` 应以非 0 退出（模型未配置）；\
         若它 exit 0，说明 `--help` 被误判成模式 flag 提前早退了。\n\
         --- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    );
}

/// T7：`worktree --help` **不是** help —— 子命令级帮助不存在。
///
/// `--help` 在 worktree 语境下是以 `-` 开头的名参 ⇒ 既有校验报「未知的 worktree 动词」
/// 并非零退出。本用例只守「没打印帮助」，不对报错文案与退出码细节作断言。
#[test]
fn worktree_subcommand_help_is_not_help() {
    let roots = Roots::new("t7");
    let out = run_guarded_output(&["worktree", "--help"], &roots);
    let (stdout, stderr) = streams(&out);

    let hits_out = hits_help_markers(&stdout);
    let hits_err = hits_help_markers(&stderr);
    assert!(
        hits_out.is_empty() && hits_err.is_empty(),
        "`worktree --help` 不是子命令级帮助，不得打印帮助文档。\n\
         stdout 命中特征串：{hits_out:?}\n\
         stderr 命中特征串：{hits_err:?}\n\
         --- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    );
    assert!(
        stdout.trim().is_empty(),
        "`worktree --help` 打在 stderr 上的报错不该污染 stdout（数据流纯净），实际 stdout：\n{stdout}"
    );
    assert!(
        !out.status.success(),
        "`worktree --help` 应以非 0 退出（名字不能以 `-` 开头）\n\
         --- stderr ---\n{stderr}"
    );
}

/// T8：输出内容的关键锚点 —— 用法 / flag / worktree 动词 / 环境变量 / 斜杠命令。
///
/// 文案是**手写常量**，与真实 CLI 语义可能漂移；本用例把「文档里该有的东西」钉住，
/// 删改任何一段（用法 / 公共 flag / 环境变量 / 斜杠命令）都会变红。
#[test]
fn help_output_contains_the_documented_anchors() {
    let roots = Roots::new("t8");
    let stdout = run_guarded(&["--help"], &roots);

    // 三种触发形态的字样 + 用法段
    for needle in [
        "--help",
        "-h",
        "help",
        "用法：",
        "公共 flag：",
        "环境变量：",
        "TUI 斜杠命令：",
    ] {
        assert!(
            stdout.contains(needle),
            "帮助应包含 {needle:?}\n--- stdout ---\n{stdout}"
        );
    }
    // worktree 的四个动词
    for verb in ["create", "attach", "list", "remove"] {
        assert!(
            stdout.contains(verb),
            "帮助应列出 worktree 动词 {verb:?}\n--- stdout ---\n{stdout}"
        );
    }
    // 公共 flag
    for flag in ["--debug", "--stats", "--resume"] {
        assert!(
            stdout.contains(flag),
            "帮助应记录公共 flag {flag:?}\n--- stdout ---\n{stdout}"
        );
    }
    // 环境变量
    for var in ["YUSHAN_API_BASE", "YUSHAN_API_KEY", "YUSHAN_MODEL"] {
        assert!(
            stdout.contains(var),
            "帮助应记录环境变量 {var:?}\n--- stdout ---\n{stdout}"
        );
    }
    // 斜杠命令（取 `src/help.rs` 里实有的那几条）
    for cmd in ["/help", "/status", "/model", "/login"] {
        assert!(
            stdout.contains(cmd),
            "帮助应列出斜杠命令 {cmd:?}\n--- stdout ---\n{stdout}"
        );
    }
}
