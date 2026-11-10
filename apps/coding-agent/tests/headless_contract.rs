//! 程序化入口（headless worker）的三处契约：`--json` 首行会话记录、`SIGTERM` 收尾、
//! 退出码三档。设计见 `docs/arch/headless-worker-api/design.md` §5，
//! 决策见 `adr-001-headless-worker-contract.md`。
//!
//! 被测的是**真实二进制**（`env!("CARGO_BIN_EXE_ys-coding-agent")`）。这三处都是对外
//! 契约（调用方直接解析 stdout / 读退出码），单测只能钉住判定函数，钉不住进程行为。
//!
//! # 隔离
//!
//! 每个用例一套临时 `HOME` / workspace / 空 `PATH` / 会话目录，全部经 `Command` 的
//! per-child `.env()` 注入 —— **不改父进程 env**（改写进程级 env 会与同 binary 内并行
//! 测试的 env 读取产生数据竞争）。
//!
//! # 挂起即失败
//!
//! 所有用例套 [`CHILD_TIMEOUT`]：超时**先 `kill` 子进程再 panic**（带已捕获输出），
//! 而非测试挂死。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 子进程退出上限。正常路径秒级返回；超时 = 挂起 = kill + panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// SIGTERM 用例里 mock 写响应前的停留时长。
///
/// 必须**显著大于**本用例发信号的时刻，否则回合可能在信号到达前就收场，
/// 测的就不再是「信号打断在飞的回合」。取 2s（信号在回合确认开跑后立即发出）留足余量。
const SLOW_RESPONSE_DELAY: Duration = Duration::from_millis(2000);

// ---------------------------------------------------------------------------
// 临时目录 / 子进程隔离
// ---------------------------------------------------------------------------

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（项目硬约定）。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_headless_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Roots {
    workspace: PathBuf,
    home: PathBuf,
    path_dir: PathBuf,
    sessions_dir: PathBuf,
    logs_dir: PathBuf,
}

impl Roots {
    fn new(tag: &str) -> Self {
        Self {
            workspace: unique_dir(&format!("{tag}_ws")),
            home: unique_dir(&format!("{tag}_home")),
            path_dir: unique_dir(&format!("{tag}_path")),
            sessions_dir: unique_dir(&format!("{tag}_sessions")),
            logs_dir: unique_dir(&format!("{tag}_logs")),
        }
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.path_dir);
        let _ = std::fs::remove_dir_all(&self.sessions_dir);
        let _ = std::fs::remove_dir_all(&self.logs_dir);
    }
}

/// 给子进程套隔离（**全部 per-child**），并指向 mock 的 api_base。
fn isolate(cmd: &mut Command, roots: &Roots, api_base: &str) {
    cmd.current_dir(&roots.workspace)
        .env("HOME", &roots.home)
        .env("PATH", &roots.path_dir)
        .env_remove("USERPROFILE")
        .env_remove("YUSHAN_LOG_DIR")
        .env("YUSHAN_DEBUG", "0")
        .env("YUSHAN_SESSIONS_DIR", &roots.sessions_dir)
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
        .env_remove("YUSHAN_SEARCH_DEEPSEEK_KEY")
        .env_remove("DEEPSEEK_API_KEY")
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

/// 已套好隔离、管道已接好的子进程。
struct Spawned {
    child: Child,
    out_thread: std::thread::JoinHandle<Vec<u8>>,
    err_thread: std::thread::JoinHandle<Vec<u8>>,
}

impl Spawned {
    fn new(argv: &[&str], roots: &Roots, api_base: &str) -> Self {
        Self::new_with_env(argv, roots, api_base, &[])
    }

    /// [`Self::new`] 的额外 env 版本（`isolate` 之后再覆盖，用于 --debug 这类开关）。
    fn new_with_env(argv: &[&str], roots: &Roots, api_base: &str, extra: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
        isolate(&mut cmd, roots, api_base);
        for (key, value) in extra {
            cmd.env(key, value);
        }
        cmd.args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn ys-coding-agent");

        // 两个读取线程持续排空管道：输出超过管道缓冲时子进程会阻塞在 write 上而非退出。
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
        Self {
            child,
            out_thread,
            err_thread,
        }
    }

    /// 等退出（**带超时**）：超时先 `kill` 再 panic，绝不挂死。
    fn wait(self, argv: &[&str]) -> Output {
        let mut child = self.child;
        let deadline = Instant::now() + CHILD_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("ys-coding-agent {argv:?} 在 {CHILD_TIMEOUT:?} 内未退出 = 挂起");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Output {
            status,
            stdout: self.out_thread.join().unwrap(),
            stderr: self.err_thread.join().unwrap(),
        }
    }
}

fn run_child(argv: &[&str], roots: &Roots, api_base: &str) -> Output {
    Spawned::new(argv, roots, api_base).wait(argv)
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
    /// 立即回一段纯文本 —— 回合在超时内以 `Completed` 收场。
    FastText,
    /// 停留 [`SLOW_RESPONSE_DELAY`] 后回一个 `noop` tool call —— 让回合在信号到达后
    /// 仍能跨过轮边界，使 `Abort` 被 `BasicLoop` 的轮边界探针命中。
    SlowToolCall,
}

struct MockServer {
    port: u16,
    hits: Arc<AtomicU64>,
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
        let hits = Arc::new(AtomicU64::new(0));
        let hits_thread = Arc::clone(&hits);

        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // 只计**模型调用**：启动期的 `/v1/models` 目录探测不算「回合开跑」。
                        if serve_once(stream, script).unwrap_or(false) {
                            hits_thread.fetch_add(1, Ordering::SeqCst);
                        }
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
            hits,
            stop,
            handle: Some(handle),
        }
    }

    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 等到**至少一条**连接被接受（= 回合已真的向模型发请求）。
    ///
    /// SIGTERM 用例靠它把「信号送达时刻」钉在回合内：此刻信号监听器必然已建好
    /// （它在回合循环之前创建，而请求发生在回合里）。单纯按墙钟 sleep 会在慢机器上
    /// 落到启动期 —— 那时 SIGTERM 走默认处置，进程被信号杀死（退出码 `None`），
    /// 用例会以一个与信号处理无关的原因变红。
    fn wait_for_first_request(&self) {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        while self.hits.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "子进程 {CHILD_TIMEOUT:?} 内没有向 mock 发出请求"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
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
///
/// 返回值 = 本次是否**模型调用**（`POST …/chat/completions`）。`-p` / `--json` 在启动期
/// 还会同步拉一次 `GET /v1/models` 解析上下文窗口 —— 那一次必须**立即**应答，
/// 否则信号会在进程仍处于启动期时送达（那时监听器还没建，SIGTERM 走默认处置直接杀进程）。
fn serve_once(mut stream: TcpStream, script: Script) -> std::io::Result<bool> {
    // 监听 socket 是非阻塞的；BSD/macOS 上 `accept()` 的连接会继承 O_NONBLOCK。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(false);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let request_line = headers.lines().next().unwrap_or("").to_string();
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let is_chat = path.contains("chat/completions");
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

    if !is_chat {
        // 启动期目录探测：空目录 ⇒ 窗口走兜底值，与本用例无关。必须立即返回。
        let payload = b"{\"data\":[]}";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(payload)?;
        stream.flush()?;
        return Ok(false);
    }

    let sse = match script {
        Script::SlowToolCall => {
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

    let payload = sse.as_bytes();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 断言辅助
// ---------------------------------------------------------------------------

/// stdout 的非空行，全部解析为 JSON。
fn json_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stdout 每行必须是合法 JSON：{e}\n行内容：{line}"))
        })
        .collect()
}

/// 会话目录里唯一的 `*.jsonl` 文件名（不含扩展名）。
fn sole_session_id(dir: &Path) -> String {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("会话目录必须可读")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "会话目录里应恰有一个 jsonl，实际：{found:?}"
    );
    found.pop().unwrap()
}

/// 落一份可恢复的会话：`meta` 行 + 一条用户历史。
fn write_resumable_session(dir: &Path, id: &str) {
    let meta = serde_json::json!({
        "type": "meta",
        "v": 1,
        "title": "历史会话",
        "provider": null,
        "model": null,
        "thinking_visible": false,
        "input_tokens": 0,
        "output_tokens": 0,
        "turns": 0,
        "created_at": 0,
    });
    // `Role` / `ContentBlock` 是 serde **外部标签**。
    let message = serde_json::json!({
        "role": "User",
        "content": [{ "Text": { "text": "HISTORICAL-PING" } }],
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        format!("{meta}\n{message}\n"),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 1. `--json` 的**首行**是会话记录 `{"type":"session","id":…}`，id 非空且等于落盘的
///    会话文件名；其后每行都是 `Envelope`。调用方拿到这个 id 就能 `--resume`。
///
/// 变异：把首行写进共用的 `consume_events` ⇒「`-p` 不输出该行」用例变红；
/// 把它挪到 `ensure_configured` 之前 ⇒ 无配置早退也会报一个不会被落盘的 id。
#[test]
fn json_first_line_is_session_record() {
    let server = MockServer::start(Script::FastText);
    let roots = Roots::new("json_session_first_line");

    let out = run_child(&["--json", "hi"], &roots, &server.api_base());
    let (stdout, stderr) = streams(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "正常完成应 exit(0)；stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let values = json_lines(&stdout);
    let first = values
        .first()
        .unwrap_or_else(|| panic!("stdout 不得为空\nstderr:\n{stderr}"));
    assert_eq!(first["type"], "session", "首行必须是会话记录：{first}");
    let id = first["id"]
        .as_str()
        .unwrap_or_else(|| panic!("会话记录必须带字符串 id：{first}"));
    assert!(!id.is_empty(), "会话 id 不得为空：{first}");
    assert_eq!(
        id,
        sole_session_id(&roots.sessions_dir),
        "首行 id 必须等于落盘的会话文件名（调用方就靠它 --resume）"
    );

    // 首行之后只允许 `Envelope`。
    for value in &values[1..] {
        assert!(
            value.get("event").is_some(),
            "首行之后 stdout 只应有 Envelope：{value}"
        );
    }
    assert!(
        values.iter().skip(1).any(|value| value
            .get("event")
            .and_then(|e| e.get("RunFinished"))
            .is_some()),
        "应收到终局 RunFinished\nstdout:\n{stdout}"
    );

    roots.cleanup();
}

/// 2. `-p` 的 stdout **不含**会话记录：它只放正文，多一行就污染了逐字输出。
///
/// 变异：把首行输出挪到两条入口共用的 `consume_events` ⇒ 本用例变红。
#[test]
fn print_stdout_has_no_session_record() {
    let server = MockServer::start(Script::FastText);
    let roots = Roots::new("print_no_session_line");

    let out = run_child(&["-p", "hi"], &roots, &server.api_base());
    let (stdout, stderr) = streams(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "正常完成应 exit(0)；stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("\"type\":\"session\""),
        "`-p` 的 stdout 不得出现会话记录：{stdout:?}"
    );
    assert!(
        !stdout.contains("\"session\""),
        "`-p` 的 stdout 只放正文：{stdout:?}"
    );

    roots.cleanup();
}

/// 3. `--json --resume <id>` 的首行 id **等于**传入的 id —— 钉住调用方存下的 id 不漂。
///
/// 变异：首行改取「本次新建的会话 id」而不是当前会话路径 ⇒ 本用例变红
/// （恢复态下二者不同：`--resume` 续写目标文件，不新建）。
#[test]
fn json_resume_first_line_echoes_requested_id() {
    let server = MockServer::start(Script::FastText);
    let roots = Roots::new("json_resume_id");
    let id = "sess-headless";
    write_resumable_session(&roots.sessions_dir, id);

    let out = run_child(
        &["--json", "--resume", id, "hi"],
        &roots,
        &server.api_base(),
    );
    let (stdout, stderr) = streams(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--resume 续跑应 exit(0)；stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let values = json_lines(&stdout);
    let first = values
        .first()
        .unwrap_or_else(|| panic!("stdout 不得为空\nstderr:\n{stderr}"));
    assert_eq!(first["type"], "session", "首行必须是会话记录：{first}");
    assert_eq!(
        first["id"], id,
        "首行 id 必须回显 --resume 传入的 id（否则调用方存的 id 会漂）：{first}"
    );
    assert_eq!(
        sole_session_id(&roots.sessions_dir),
        id,
        "--resume 不得新建第二个会话文件"
    );

    roots.cleanup();
}

/// 4. `SIGTERM` ⇒ 只向回合边界源投 `Abort`，回合以 `Cancelled` 收场 ⇒ **退出码 75**
///    （可重试档）。进程**不被信号直接杀死**，故退出码是 `Some(75)` 而不是 `None`。
///
/// 时序不变量：信号在**回合已向模型发出请求之后**才发 ⇒ 监听器必然已建好，
/// 且回合正卡在模型调用里（不是启动期，也不是回合之间）。
///
/// 变异：把信号分支写成直接退进程 ⇒ 退出码不再是 75（且 `kill_backgrounds_on_exit`
/// 与 debug 尾部收尾都被绕过）；把信号监听器改成每回合新建 ⇒ 信号可能落进
/// 没人 `recv` 的窗口，本用例会以超时（挂起）变红。
#[cfg(unix)]
#[test]
fn sigterm_aborts_turn_and_exits_75() {
    let server = MockServer::start(Script::SlowToolCall);
    let roots = Roots::new("sigterm");
    let argv = ["--json", "hi"];

    let spawned = Spawned::new(&argv, &roots, &server.api_base());
    // 回合已真的开跑（模型请求在飞）⇒ 信号监听器已建好。
    server.wait_for_first_request();

    let pid = spawned.child.id().to_string();
    let sent = Command::new("kill")
        .arg("-TERM")
        .arg(&pid)
        .status()
        .expect("调用 kill -TERM（PATH 应含 kill）");
    assert!(sent.success(), "kill -TERM {pid} 必须成功送达");

    let out = spawned.wait(&argv);
    let (stdout, stderr) = streams(&out);
    assert_eq!(
        out.status.code(),
        Some(75),
        "SIGTERM 收尾必须以可重试档 75 退出（None = 被信号直接杀死，绕过了清理与 debug 尾部）\
         \nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("SIGTERM"),
        "应有一行 stderr 说明信号已被处理\nstderr:\n{stderr}"
    );

    // stdout 契约不因信号而变：首行会话记录，其后全是 Envelope。
    let values = json_lines(&stdout);
    if let Some(first) = values.first() {
        assert_eq!(first["type"], "session", "首行必须是会话记录：{first}");
    }
    assert!(
        values.iter().skip(1).any(|value| value
            .get("event")
            .and_then(|e| e.get("RunFinished"))
            .is_some()),
        "回合应以 RunFinished 收场（不是被半路丢弃）\nstdout:\n{stdout}"
    );

    roots.cleanup();
}

/// 5. **无配置早退 ⇒ 不输出会话行**：首行必须写在 `ensure_configured` **之后**，
///    否则一个马上要退出的进程会先报出一个连自己都没落盘的 id。
///
/// 变异：把 `emit_session_line` 挪到 `ensure_configured` 之前 ⇒ 本用例变红
/// （stdout 会出现一行 `{"type":"session",…}`）。
#[test]
fn no_config_early_exit_prints_no_session_record() {
    let server = MockServer::start(Script::FastText);
    let roots = Roots::new("no_config");
    // `YUSHAN_API_KEY` 被清掉 ⇒ 无任何可用凭证 ⇒ `ensure_configured` 早退。
    let argv = ["--json", "hi"];
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    isolate(&mut cmd, &roots, &server.api_base());
    cmd.env_remove("YUSHAN_API_KEY")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().expect("spawn ys-coding-agent");
    let (stdout, stderr) = streams(&out);

    assert_eq!(
        out.status.code(),
        Some(1),
        "无配置应早退 exit(1)；stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "无配置早退时 stdout 必须为空（不得先报一个不会被落盘的会话 id）：{stdout:?}"
    );
    assert!(
        stderr.contains("No model configured"),
        "应说明缺少模型配置：\n{stderr}"
    );

    roots.cleanup();
}

/// 6. `SIGTERM` 收尾后 **debug 尾部完好**：文件以完整 JSON 行结尾、最后一行为
///    `run_end` 且 `exit_code == 75`。硬停（信号走默认处置）会丢掉这段尾部。
///
/// 变异：信号分支改成直接 `process::exit` ⇒ 本用例变红（`run_end` 缺席或码不对）。
#[cfg(unix)]
#[test]
fn sigterm_still_writes_debug_run_end() {
    let server = MockServer::start(Script::SlowToolCall);
    let roots = Roots::new("sigterm_debug");
    let argv = ["--json", "hi"];
    let logs = roots.logs_dir.to_str().unwrap().to_string();

    let spawned = Spawned::new_with_env(
        &argv,
        &roots,
        &server.api_base(),
        &[("YUSHAN_DEBUG", "1"), ("YUSHAN_LOG_DIR", &logs)],
    );
    server.wait_for_first_request();

    let pid = spawned.child.id().to_string();
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(&pid)
            .status()
            .expect("调用 kill -TERM")
            .success(),
        "kill -TERM {pid} 必须成功送达"
    );

    let out = spawned.wait(&argv);
    let (stdout, stderr) = streams(&out);
    assert_eq!(
        out.status.code(),
        Some(75),
        "SIGTERM 收尾应以 75 退出\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let files: Vec<PathBuf> = std::fs::read_dir(&roots.logs_dir)
        .expect("日志目录必须可读")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("debug-") && name.ends_with(".jsonl"))
        })
        .collect();
    assert_eq!(files.len(), 1, "debug 开时应恰好一个文件：{files:?}");

    let text = std::fs::read_to_string(&files[0]).unwrap();
    assert!(
        text.ends_with('\n'),
        "debug 文件必须以完整行结尾（硬停会截断）：{text:?}"
    );
    let last: serde_json::Value =
        serde_json::from_str(text.lines().next_back().expect("debug 文件不得为空"))
            .expect("debug 每行必须是合法 JSON");
    assert_eq!(last["ev"], "run_end", "最后一行为 run_end：{last}");
    assert_eq!(last["stop_reason"], "RunFinished", "收场形态：{last}");
    assert_eq!(
        last["exit_code"], 75,
        "debug 里的 exit_code 与实际退出码同源：{last}"
    );

    roots.cleanup();
}
