//! **`--resume <id>` 子进程端到端**：恢复既有会话、续写同一文件、退出提示的 id。
//!
//! 钉住设计 `docs/arch/runtime-resume/design.md` 的 D3/§9（`--resume` 加载既有历史）
//! 与 §5 的 I3（会话路径快照必须指向目标会话）。本文件覆盖执行计划的 T13：
//!
//! - **N5**：退出时 stderr 的 `session: <id>` 与**目标**会话文件 stem 一致；
//! - **N12**：`--resume` 启动期把既有历史带进本轮对话（见下「关于 TUI 回放」）。
//!
//! # 关于 TUI 回放（N12 的取舍）
//!
//! 设计 D3 修的是「`--resume` 启动后屏上恒空」—— 正解是启动期发
//! `Outbound::SessionResumed` 让 UI 回放。但**捕获 TUI 屏幕需要 pty**，而本仓
//! **不引入新依赖**（无 `libc` / `nix`），crossterm 在无 tty 的子进程里起不了屏。
//! 故 N12 在子进程侧验证其**可离线观测的等价事实**：恢复的历史确实被加载进会话，
//! 并随本轮请求发给模型（`body_contains` 断言）。启动期出站**顺序**（`SessionResumed`
//! 排在 `startup_hint` 之前，I10）由 `src/main.rs` 的
//! `startup_outbound_sequence_puts_replay_before_hint` 进程内单测钉住。
//! 这样不写不稳定的 pty 测试，也不为凑覆盖率放松断言。
//!
//! # 子进程 env 隔离
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` / `.current_dir()` 注入，
//! **绝不改父进程 env**。`HOME` 指向空临时目录；`PATH` 指向空目录（钉住 toolset 降级
//! 提示为常量）；`YUSHAN_SESSIONS_DIR` 指向临时会话目录。模型侧凭证指向回环 SSE mock。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 子进程退出上限。正常路径秒级返回；超时 = 挂起 = panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全，项目硬约定）。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_resume_cli_{tag}_{}_{seq}",
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

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.path_dir);
        let _ = std::fs::remove_dir_all(&self.sessions_dir);
    }
}

/// 所有用例共用的隔离（**全部 per-child**，不碰父进程 env）。
fn isolate(cmd: &mut Command, roots: &Roots) {
    cmd.current_dir(&roots.workspace)
        .env_remove("YUSHAN_LOG_DIR")
        .env("YUSHAN_DEBUG", "0")
        .env("HOME", &roots.home)
        .env("PATH", &roots.path_dir)
        .env_remove("USERPROFILE")
        .env("YUSHAN_SESSIONS_DIR", &roots.sessions_dir)
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        .env_remove("OPENAI_API_BASE")
        .env_remove("OPENAI_API_KEY")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_DEEPSEEK_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("YUSHAN_FETCHED_DIR")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
}

/// 跑一次被测二进制（**套 timeout**；挂起 → panic，而非测试挂死）。
fn run_guarded_with_env(argv: &[&str], roots: &Roots, envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    isolate(&mut cmd, roots);
    cmd.args(argv);
    for (key, value) in envs {
        cmd.env(key, value);
    }

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

/// 落一个**可恢复的**会话文件：首行 meta + 一条历史 user 消息（形状见
/// `crates/ys-session/src/meta.rs` 与 `ys-core` 的 `Message`）。
fn write_resumable_session(dir: &Path, id: &str, title: &str, historical: &str) -> PathBuf {
    let meta = serde_json::json!({
        "type": "meta",
        "v": 1,
        "title": title,
        "provider": null,
        "model": null,
        "thinking_visible": false,
        "input_tokens": 0,
        "output_tokens": 0,
        "turns": 0,
        "created_at": 0,
    });
    // `Role` / `ContentBlock` 是 serde **外部标签**：role 是变体名 `"User"`，
    // 文本块是 `{"Text":{"text":…}}`（不是 `{"type":"text",…}`）。
    let message = serde_json::json!({
        "role": "User",
        "content": [{ "Text": { "text": historical } }],
    });
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{meta}\n{message}\n")).unwrap();
    path
}

/// `dir` 下 `*.jsonl` 的数量。
fn jsonl_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .count()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 回环 SSE mock：跑通成功回合（`persist_single_turn` 才会执行）
// ---------------------------------------------------------------------------

/// 一段最简成功 SSE（照搬 `session_cli.rs` / `truncation_guard.rs` 已验证的帧形状）。
fn success_sse() -> String {
    let mut sse = String::new();
    sse.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    );
    sse.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    sse.push_str("data: [DONE]\n\n");
    sse
}

struct SseMockServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SseMockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .expect("bind 回环 SSE mock listener（沙箱可能禁止 bind）");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests_thread = requests.clone();
        let stop_thread = stop.clone();
        let sse = success_sse();

        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_sse_once(stream, &requests_thread, &sse);
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
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for SseMockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn headers_content_length(header_bytes: &[u8]) -> usize {
    let text = String::from_utf8_lossy(header_bytes);
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse::<usize>().unwrap_or(0);
        }
    }
    0
}

/// 处理一个连接：读请求行 / 头 / body，记录原始字节，回 200 + SSE 后关闭。
fn serve_sse_once(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<String>>>,
    sse: &str,
) -> std::io::Result<()> {
    // macOS/BSD 上 `accept()` 的连接会继承监听 socket 的 O_NONBLOCK，显式复位。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut raw = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&raw, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let content_length = headers_content_length(&raw[..header_end]);
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&tmp[..n]);
    }

    requests
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&raw).into_owned());

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sse.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(sse.as_bytes())?;
    stream.flush()
}

/// 捕获到的请求里是否有任意一条的 body 含 `needle`（历史文本会被 JSON 原样内嵌，
/// 不做转义 —— `serde_json` 默认输出 UTF-8）。
fn body_contains(requests: &[String], needle: &str) -> bool {
    requests.iter().any(|request| request.contains(needle))
}

// ---------------------------------------------------------------------------
// T13 e2e —— N5 / N12
// ---------------------------------------------------------------------------

/// **T13 —— `--resume <id>` 续写目标会话 + 回放既有历史 + 退出提示 id**。
///
/// 变异的靶子：
/// - M1 把 `-p` 的 `session_exit_line` 改成写 stdout ⇒ stdout 混入人读文本（本用例
///   断言 stdout 恰为模型文本）；
/// - M2 让 `--resume` 落回**默认新会话**（忽略 `resume_path`）⇒ 新消息写进**新文件**、
///   原目标文件不含 `NEW-TASK`，且 `jsonl_count == 2` ⇒ 多条断言变红。
#[test]
fn resume_continues_target_session_and_replays_history() {
    let server = SseMockServer::start();
    let roots = Roots::new("n12");

    let id = "sess-hist";
    let hist = "HISTORICAL-PING-内容";
    let new_task = "NEW-TASK-x";
    write_resumable_session(&roots.sessions_dir, id, "历史会话", hist);

    let api_base = server.api_base();
    let out = run_guarded_with_env(
        &["-p", "--resume", id, new_task],
        &roots,
        &[
            ("YUSHAN_API_BASE", api_base.as_str()),
            ("YUSHAN_API_KEY", "sk-test"),
            ("YUSHAN_MODEL", "mock-model"),
        ],
    );
    let (stdout, stderr) = streams(&out);

    assert_eq!(
        out.status.code(),
        Some(0),
        "成功回合应 exit(0)；stderr:\n{stderr}"
    );
    assert_eq!(
        stdout.trim(),
        "ok",
        "-p 的 stdout 恰为模型文本（不得混入 `session:` 等）：{stdout:?}\nstderr:\n{stderr}"
    );

    // N5：退出提示的 id == **目标**会话文件 stem。
    assert!(
        stderr.contains(&format!("session: {id}")),
        "stderr 应以 `session: <目标 id>` 提示（I3）：\n{stderr}"
    );

    // N12（等价证据）：恢复的历史确实被加载并进入本轮发给模型的请求。
    let requests = server.requests();
    assert!(
        body_contains(&requests, hist),
        "恢复的历史应进入模型请求（--resume 未加载历史？）：\n{requests:#?}"
    );
    assert!(
        body_contains(&requests, new_task),
        "本轮新消息也应进入请求：\n{requests:#?}"
    );

    // 续写**同一文件**：新消息 append 进目标文件、历史不丢。
    let path = roots.sessions_dir.join(format!("{id}.jsonl"));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains(new_task),
        "新消息应写进目标会话文件：{content}"
    );
    assert!(content.contains(hist), "恢复的历史不得丢失：{content}");

    // 不新建会话文件 —— 仍是同一个目标文件。
    assert_eq!(
        jsonl_count(&roots.sessions_dir),
        1,
        "`--resume` 不得新建第二个会话文件"
    );

    roots.cleanup();
}
