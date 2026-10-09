//! `-p` / `--json` 一次性模式的 **wall-clock 预算兜底**的子进程级验收。
//!
//! 被测的是**真实二进制**（`env!("CARGO_BIN_EXE_ys-coding-agent")`）：单测能钉住
//! [`run_single_turn`] 的终局 `Cancelled` 与 `single_turn_exit_code` 的**判定**，
//! 但「进程**真的**以非零码退出」「说明行**真的**落在 stderr 而非 stdout」只能在本
//! 文件走得到。两者都是对外契约，不是内部实现细节。
//!
//! # 预算如何喂进子进程
//!
//! 预算时长取自 `Config::max_turn_duration()`，由 `~/.yushan/config.json` 的
//! `max_turn_duration_secs` 覆盖（`config.rs` 的 `default_user_config_path` 把路径钉在
//! `$HOME/.yushan/config.json`）。故每个用例给子进程一个**独立的临时 `HOME`**，在其下
//! 落一份写有 `max_turn_duration_secs = 1` 的 `config.json`。`HOME` 经 `Command` 的
//! per-child `.env()` 注入，**不改父进程 env**（改写进程级 env 会与同 binary 内并行测试
//! 的 env 读取产生数据竞争）。
//!
//! # 让回合「跑得过预算」的 mock
//!
//! 预算到点时 `run_single_turn` 只做两件事：写一行 **stderr** 说明行、向本回合的边界源
//! `push(Boundary::Abort)`。`Abort` 是**轮边界**信号 —— `BasicLoop` 在**每轮调用 model
//! 之前**（Step 3）检查它。故要观察到 `Cancelled`，回合必须**跨过一个轮边界**：
//!
//! - 纯「黑洞」mock（接受连接后永不应答）**不行**：回合会卡在 Step 6 的 model 调用里，
//!   直到流空闲超时（默认 120s）才以 `Model` 错误收场 —— 那是**错误路径**，不是
//!   `Cancelled`，`single_turn_exit_code` 根本不会被调用。
//! - 本文件用**延迟的 tool-call 响应**：mock 收到请求后停留
//!   [`SLOW_RESPONSE_DELAY`]（> 1s 预算），再回一个 tool call。预算先到点（Abort 置位），
//!   模型随后返回 tool call 让回合走到下一个轮边界 → Step 3 命中 Abort → `Cancelled`。
//!   mock 只回一个**不存在**的工具（`noop`），执行只产生一条 `tool not found` 结果，
//!   回合继续走到轮边界，不触碰真实文件系统。
//!
//! # 挂起即失败
//!
//! 所有用例套 [`CHILD_TIMEOUT`]：超时**先 `kill` 子进程再 panic**（带已捕获输出），
//! 而非测试挂死。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 子进程退出上限。正常路径 2s 级返回；超时 = 挂起 = kill + panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// 测试用 wall-clock 预算（写进子进程的 `config.json`）。下限 = 1s。
const BUDGET_SECS: u64 = 1;

/// 正常完成路径（用例 3、4）用的充裕预算。
///
/// **必须显著大于该回合的耗时**（mock 秒回，实测 ~20ms）：正常路径要论证的是「预算**没有**
/// 到点」，故计时器须在整个回合内都不可能触发 —— 让「无预算说明行」不再依赖 wall-clock
/// 时序。取值只需 ≫ 回合耗时，30s 是 ~20ms 的千级以上，任何合理负载都碰不到。
const NORMAL_BUDGET_SECS: u64 = 30;

/// 慢响应 mock 在写 SSE 之前的停留时长。
///
/// **必须显著大于 [`BUDGET_SECS`]**：本用例的时序前提是「预算计时器先于 model 返回到点」。
/// 取 2s（预算 1s）留 1s 余量，而非贴着 1s 走 —— 免得并行负载把两端推到一起。
const SLOW_RESPONSE_DELAY: Duration = Duration::from_millis(2000);

// ---------------------------------------------------------------------------
// 临时目录 / 子进程隔离
// ---------------------------------------------------------------------------

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（项目硬约定）。
///
/// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删 —— 故用计数器。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_budget_exit_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 每个用例一套隔离根：临时 workspace / HOME / PATH / 会话目录。
struct Roots {
    workspace: PathBuf,
    home: PathBuf,
    path_dir: PathBuf,
    sessions_dir: PathBuf,
}

impl Roots {
    fn new(tag: &str) -> Self {
        Self {
            workspace: unique_dir(&format!("{tag}_ws")),
            home: unique_dir(&format!("{tag}_home")),
            path_dir: unique_dir(&format!("{tag}_path")),
            sessions_dir: unique_dir(&format!("{tag}_sessions")),
        }
    }

    /// 落预算配置：`$HOME/.yushan/config.json` 写 `max_turn_duration_secs`。
    fn write_budget_config(&self, secs: u64) {
        let dir = self.home.join(".yushan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            format!(r#"{{"max_turn_duration_secs": {secs}}}"#),
        )
        .unwrap();
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.path_dir);
        let _ = std::fs::remove_dir_all(&self.sessions_dir);
    }
}

/// 给子进程套一层隔离（**全部 per-child**，不碰父进程 env），并指向 mock 的 api_base。
///
/// `HOME` → 临时目录（预算 `config.json` / `auth.json` / `state.json` / 用户级 skill 根
/// 都落这里）；`current_dir` → 临时 workspace；`PATH` → 空目录（把 toolset 降级提示钉成
/// 常量）；`YUSHAN_SESSIONS_DIR` 显式指向临时会话目录；模型凭证指向回环 mock 并清掉
/// 生态通用回退名；web / 信道侧 env 全清；回环地址绕过代理。
fn isolate(cmd: &mut Command, roots: &Roots, api_base: &str) {
    cmd.current_dir(&roots.workspace)
        .env("HOME", &roots.home)
        .env("PATH", &roots.path_dir)
        .env_remove("USERPROFILE")
        .env("YUSHAN_SESSIONS_DIR", &roots.sessions_dir)
        // 模型侧凭证：指向回环 mock。
        .env("YUSHAN_API_BASE", api_base)
        .env("YUSHAN_API_KEY", "test-key")
        .env("YUSHAN_MODEL", "test-model")
        // `Config::from_env` 的生态通用回退名（避免开发机 shell 里的凭证短路）。
        .env_remove("OPENAI_API_BASE")
        .env_remove("OPENAI_API_KEY")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        // web 侧（净化 toolset 的提示词，避免无关噪音）。
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("YUSHAN_FETCHED_DIR")
        // 回环 mock：绕过可能存在的代理。
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
}

/// 跑一次被测二进制，**自带超时并在超时后 `kill` 子进程**。
///
/// 手动 `spawn` + 轮询 `try_wait`（而非 `Command::output()`）：只有这样才能在超时分支
/// 拿到 `&mut Child` 去 `kill`。stdout / stderr 由两个读取线程排空，避免管道缓冲区写满
/// 造成子进程假死。
fn run_guarded(mut cmd: Command, argv: &[&str]) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn ys-coding-agent");

    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_pipe.read_to_end(&mut v);
        v
    });
    let err_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err_pipe.read_to_end(&mut v);
        v
    });

    let deadline = Instant::now() + CHILD_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "ys-coding-agent {argv:?} 在 {CHILD_TIMEOUT:?} 内未退出 = 挂起\
                 （已 kill，必须表现为失败，而非测试挂死）"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    Output {
        status,
        stdout: out_thread.join().unwrap(),
        stderr: err_thread.join().unwrap(),
    }
}

/// 已套好隔离的 Command，跑 `argv`。
fn run_child(argv: &[&str], roots: &Roots, api_base: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    isolate(&mut cmd, roots, api_base);
    cmd.args(argv);
    run_guarded(cmd, argv)
}

fn streams(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ---------------------------------------------------------------------------
// 回环 mock HTTP 服务
// ---------------------------------------------------------------------------

/// mock 的响应脚本。
#[derive(Clone, Copy)]
enum Script {
    /// 停留 [`SLOW_RESPONSE_DELAY`] 后回一个 `noop` tool call —— 让回合跨过轮边界，
    /// 使预算到点置位的 `Abort` 被 `BasicLoop` 的 Step 3 探针命中。
    SlowToolCall,
    /// 立即回一段纯文本 —— 回合在预算内以 `Completed` 收场（回归护栏的对照）。
    FastText,
}

/// 回环地址上的极简 HTTP/1.1 mock：每个连接处理一个请求后关闭（`Connection: close`）。
///
/// 用 std 手写而非引入测试依赖（与 `truncation_guard.rs` / `stream_usage_fallback.rs`
/// 同款）。
struct MockServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(script: Script) -> Self {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind 回环 mock（沙箱可能禁止 bind）");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_once(stream, script);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    /// api_base（code 会在其后追加 `/chat/completions`）。
    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 处理一个连接：读请求行 / 头 / body，按 [`Script`] 回一段 SSE。
fn serve_once(mut stream: TcpStream, script: Script) -> std::io::Result<()> {
    // 监听 socket 是非阻塞的（accept 轮询 stop 标志需要）。macOS/BSD 上 `accept()`
    // 返回的连接**会继承** O_NONBLOCK，导致下面的 `set_read_timeout` 被忽略 —— 显式复位。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    let sse = match script {
        Script::SlowToolCall => {
            // 停留 > 预算：让预算计时器先到点（`Abort` 置位）。
            std::thread::sleep(SLOW_RESPONSE_DELAY);
            let mut s = String::new();
            s.push_str(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
                 \"id\":\"call_1\",\"function\":{\"name\":\"noop\",\"arguments\":\"{}\"}}]}}]}\n\n",
            );
            s.push_str(
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            );
            s.push_str("data: [DONE]\n\n");
            s
        }
        Script::FastText => {
            let mut s = String::new();
            s.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n");
            s.push_str(
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            );
            s.push_str("data: [DONE]\n\n");
            s
        }
    };

    let body = sse.as_bytes();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// 断言辅助
// ---------------------------------------------------------------------------

/// stdout 的**每一条非空行**都必须是合法 JSON（钉住「说明行没污染 stdout」）。
/// 返回解析后的值，供进一步断言。
fn parse_all_json_lines(stdout: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    for (i, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout 第 {} 行不是合法 JSON：{e}\n行内容：{line}", i + 1));
        values.push(v);
    }
    values
}

/// 取 `--json` 输出中某事件的 payload。
fn event_payload(values: &[serde_json::Value], name: &str) -> Option<serde_json::Value> {
    values
        .iter()
        .find_map(|v| v.get("event")?.get(name).cloned())
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 1. `-p` + 极短预算 ⇒ **非零退出**且 **stderr** 含说明行。
///
/// 钉住设计判断：`-p` 没有 `Outbound` 信道，stdout 承载模型文本增量，预算说明行只能走
/// stderr。断言的是**用户可观察**的两件事：退出码、说明行的**去处**。
#[test]
fn print_budget_exhaustion_exits_nonzero_with_stderr_note() {
    let server = MockServer::start(Script::SlowToolCall);
    let roots = Roots::new("print_budget");
    roots.write_budget_config(BUDGET_SECS);

    let out = run_child(&["-p", "hi"], &roots, &server.api_base());
    let (_stdout, stderr) = streams(&out);

    assert!(
        !out.status.success(),
        "预算到点必须以非零码退出\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("budget exhausted"),
        "预算说明行必须在 stderr\nstderr:\n{stderr}"
    );

    roots.cleanup();
}

/// 2. `--json` + 极短预算 ⇒ **非零退出**，且 **stdout 每一条非空行都是合法 JSON**。
///
/// 本用例是「说明行没污染 stdout」这条设计判断的**唯一**进程级钉法：若说明行误走 stdout
/// （`eprintln!` → `println!`），那条 `budget exhausted: …` 文本无法被解析为 JSON，
/// [`parse_all_json_lines`] 立即 panic。
///
/// 同时断言终局事件是 `RunFinished { stop_reason: Cancelled }` —— 把「非零退出」与
/// 「回合以 `Cancelled` 收场」的因果关系显式钉住（退出码正是由它推出）。
#[test]
fn json_budget_exhaustion_keeps_stdout_json_clean_and_exits_nonzero() {
    let server = MockServer::start(Script::SlowToolCall);
    let roots = Roots::new("json_budget");
    roots.write_budget_config(BUDGET_SECS);

    let out = run_child(&["--json", "hi"], &roots, &server.api_base());
    let (stdout, stderr) = streams(&out);

    assert!(
        !out.status.success(),
        "预算到点必须以非零码退出\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // **关键断言**（先于 stderr 断言，好让它成为变异 `eprintln!→println!` 的**首个**
    // 命中点）：stdout 全行可解析 JSON —— 说明行不得混入。
    let values = parse_all_json_lines(&stdout);
    assert!(!values.is_empty(), "应至少收到一个 Envelope\nstdout:\n{stdout}");

    assert!(
        stderr.contains("budget exhausted"),
        "预算说明行必须在 stderr\nstderr:\n{stderr}"
    );

    let finished = event_payload(&values, "RunFinished")
        .unwrap_or_else(|| panic!("应收到 RunFinished\nstdout:\n{stdout}"));
    assert_eq!(
        finished["stop_reason"], "Cancelled",
        "预算到点应以 Cancelled 收场（非零退出正由它推出）\nstdout:\n{stdout}"
    );

    roots.cleanup();
}

/// 3. 正常完成路径（`-p`）退出码不变：mock 秒回文本 ⇒ **退出码 0**，stdout 含模型文本。
///
/// 这是「非零退出没有误伤正常路径」的回归护栏：预算取 [`NORMAL_BUDGET_SECS`]，远大于该
/// 回合耗时 ⇒ 计时器**不可能**到点。故「退出码 0」与「stderr 无预算说明行」合起来证明
/// **非零退出只由预算到点触发**（纯文本回合的 `BasicLoop` 直接 `Completed`、不检查 abort，
/// 单看退出码 0 并不足以排除预算已触发，须有 stderr 断言兜底）。
#[test]
fn print_normal_completion_exit_code_is_zero() {
    let server = MockServer::start(Script::FastText);
    let roots = Roots::new("print_normal");
    roots.write_budget_config(NORMAL_BUDGET_SECS);

    let out = run_child(&["-p", "hi"], &roots, &server.api_base());
    let (stdout, stderr) = streams(&out);

    assert!(
        out.status.success(),
        "正常完成应退出码 0\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("hi"),
        "`-p` 的 stdout 应含模型文本增量\nstdout:\n{stdout}"
    );
    assert!(
        !stderr.contains("budget exhausted"),
        "正常路径不得出现预算说明行\nstderr:\n{stderr}"
    );

    roots.cleanup();
}

/// 4. 正常完成路径（`--json`）退出码不变：mock 秒回文本 ⇒ **退出码 0**，stdout 是合法
///    JSON 流且终局为 `Completed`。与用例 2 共用 [`parse_all_json_lines`]，形成对照。
///
/// 预算取 [`NORMAL_BUDGET_SECS`]，远大于该回合耗时 ⇒ 计时器**不可能**到点，退出码 0 因此
/// 反映的是「正常完成」而非「预算恰未触发」。
#[test]
fn json_normal_completion_exit_code_is_zero() {
    let server = MockServer::start(Script::FastText);
    let roots = Roots::new("json_normal");
    roots.write_budget_config(NORMAL_BUDGET_SECS);

    let out = run_child(&["--json", "hi"], &roots, &server.api_base());
    let (stdout, stderr) = streams(&out);

    assert!(
        out.status.success(),
        "正常完成应退出码 0\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let values = parse_all_json_lines(&stdout);
    let finished = event_payload(&values, "RunFinished")
        .unwrap_or_else(|| panic!("应收到 RunFinished\nstdout:\n{stdout}"));
    assert_eq!(
        finished["stop_reason"], "Completed",
        "正常路径应以 Completed 收场\nstdout:\n{stdout}"
    );

    roots.cleanup();
}
