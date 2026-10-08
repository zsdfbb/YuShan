//! **`main()` 会话装配的子进程端到端**：picker 输出去向、空列表回落、模型恢复优先级、
//! `-p` / `--json` 成功回合的收尾（落盘 meta + `session: <id>`）。
//!
//! 钉住设计 `docs/arch/session-default-new/design.md` 的 C8/T4（picker 输出全走 stderr）、
//! §5.4（空列表回落默认新会话）、§5.6/§9（模型恢复优先级 env > resume meta > state.json
//! > 首个有凭证）。
//!
//! # 三条被测的生产路径
//!
//! `main()` 的顺序（`apps/coding-agent/src/main.rs`）：
//!
//! 1. `parse_args`（含 `--resume[=<id>]`）；
//! 2. `resolve_resume` —— `sessions::resolve` / `sessions::pick`，**在
//!    `ensure_configured` 之前**：picker 的列表 / 非 TTY 提示 / 空列表回落提示全部写
//!    **stderr**，`--json` 的逐行 `Envelope` 走 stdout，两者绝不能混（C8/T4）；
//! 3. 模型解析 —— 仅当 `config.is_configured()` 为假时按
//!    `resume_meta`（provider 须有 auth entry）> `state.json`（last-active）>
//!    首个有凭证的 provider 依次恢复；env 完整凭证短路整段（§5.6）。
//!
//! 这些分支的**单测**覆盖不了「子进程真的把 `"model"` 字段发到了网络上」——
//! 只有本文件走得到：组 3 用回环 mock listener 观察子进程**真实发出的 HTTP 请求体**。
//!
//! # 组 1 / 组 2：无凭证、可离线
//!
//! 无模型凭证时 `ensure_configured` 在 `eprintln!("Error: No model configured.")` 后
//! `exit(1)`。组 1 的 picker 报错发生在它**之前**，故「stdout 为空」直接证明 picker
//! 没有污染数据流；组 2 的「没有可恢复的会话」必须先于早退出现，才证明空列表走的是
//! **回落默认新会话**而非报错。
//!
//! # 组 3：回环 mock listener
//!
//! listener 只记录请求、回最简 400（不实现 SSE）—— 回合失败无妨，我们只断言
//! 「请求确实发出、`"model"` 正确、路径含 `/chat/completions`」。
//!
//! # 组 4：成功回合（回真实 SSE 的回环 mock）
//!
//! 组 3 的回合**失败**，`persist_single_turn` 跑不到。组 4 用 [`success_sse`]（照搬
//! `truncation_guard.rs` 已验证的帧形状）跑通成功回合，断言「stdout 是纯数据流」「stderr
//! 的 `session: <id>` 与落盘文件 stem 一致」「落盘首行 meta 有 title + model + turns」
//! （设计 §6·T1/C7、§9）。
//!
//! # 子进程 env 隔离
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` / `.current_dir()` 注入，
//! **绝不改父进程 env**（改写进程级 env 会与同 binary 内并行测试的 env 读取产生数据竞争）。
//! `HOME` 指向空临时目录（`auth.json` / `state.json` / `config.json` / 用户级 skill 根
//! 都落这里）；`PATH` 指向空目录（把 toolset 降级提示钉成常量）；`YUSHAN_SESSIONS_DIR`
//! 显式指向临时会话目录。
//!
//! # 挂起即失败
//!
//! 所有用例套 [`CHILD_TIMEOUT`]：挂起必须是 panic（带已捕获输出），而非测试挂死。

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
///
/// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删 —— 故用计数器。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_session_cli_{tag}_{}_{seq}",
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
    /// `YUSHAN_SESSIONS_DIR` 的落点（测试自行决定放文件还是留空）。
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

/// 所有用例共用的隔离（**全部 per-child**，不碰父进程 env）：
///
/// ① `HOME` → 空临时目录；② `current_dir` → 临时 workspace；③ `PATH` → 空目录；
/// ④ 模型侧凭证（`YUSHAN_*` / `OPENAI_*`）与信道 / web 侧 env 全清；
/// ⑤ 回环地址绕过代理（`NO_PROXY`）并清掉继承来的代理变量（组 3 才有网络）。
///
/// 模型侧必须清干净：否则组 1/2 不会走到「无模型配置」早退，组 3 的恢复优先级
/// 也会被开发机 shell 里的凭证短路。
fn isolate(cmd: &mut Command, roots: &Roots) {
    cmd.current_dir(&roots.workspace)
        .env("HOME", &roots.home)
        .env("PATH", &roots.path_dir)
        .env_remove("USERPROFILE")
        .env("YUSHAN_SESSIONS_DIR", &roots.sessions_dir)
        // 模型侧
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        // `Config::from_env` 的生态通用回退名
        .env_remove("OPENAI_API_BASE")
        .env_remove("OPENAI_API_KEY")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        // web 侧
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("YUSHAN_FETCHED_DIR")
        // 回环 mock：绕过可能存在的代理
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
}

/// 已套好隔离的 Command（供需要额外 env 的组 3 追加）。
fn isolated_command(roots: &Roots) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    isolate(&mut cmd, roots);
    cmd
}

/// 跑一次被测二进制（**套 timeout**）。挂起 → panic（带已捕获输出），而非测试挂死。
fn run_guarded(argv: &[&str], roots: &Roots) -> Output {
    let mut cmd = isolated_command(roots);
    cmd.args(argv);
    run_guarded_cmd(cmd, argv)
}

/// 同上，但允许 per-child 追加 env（组 3 用；**不碰父进程 env**）。
fn run_guarded_with_env(argv: &[&str], roots: &Roots, envs: &[(&str, &str)]) -> Output {
    let mut cmd = isolated_command(roots);
    cmd.args(argv);
    for (key, value) in envs {
        cmd.env(key, value);
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

/// 落一个会话文件：首行 meta（形状见 `crates/ys-session/src/meta.rs`），随后一行裸
/// `Message`。`model = None` ⇒ meta 的 `"model": null`。
fn write_session_file(
    dir: &Path,
    id: &str,
    title: &str,
    provider: &str,
    model: Option<&str>,
) -> PathBuf {
    let meta = serde_json::json!({
        "type": "meta",
        "v": 1,
        "title": title,
        "provider": provider,
        "model": model,
        "thinking_visible": false,
        "input_tokens": 0,
        "output_tokens": 0,
        "turns": 0,
        "created_at": 0,
    });
    let message = serde_json::json!({
        "role": "user",
        "content": [{ "type": "text", "text": "hi" }],
    });
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{meta}\n{message}\n")).unwrap();
    path
}

/// 落 `$HOME/.yushan/auth.json` 的**单条** entry（形状见 `provider.rs::AuthEntry`）。
fn write_auth_entry(roots: &Roots, provider: &str, api_base: &str, api_key: &str, model: &str) {
    let mut map = serde_json::Map::new();
    map.insert(
        provider.to_string(),
        serde_json::json!({
            "api_base": api_base,
            "api_key": api_key,
            "model": model,
        }),
    );
    let path = roots.home.join(".yushan").join("auth.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&map).unwrap()).unwrap();
}

/// 落 `$HOME/.yushan/state.json`（形状见 `state.rs::AppState`）。
fn write_state_json(roots: &Roots, provider: &str, model: &str) {
    let path = roots.home.join(".yushan").join("state.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let state = serde_json::json!({
        "last_active_provider": provider,
        "last_active_model": model,
    });
    std::fs::write(path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// 组 3：回环 mock listener（只记录请求，回最简 400）
// ---------------------------------------------------------------------------

/// 回环地址上的极简 HTTP/1.1 listener：每个连接读完整个请求后记录原始字节，回
/// `400 Bad Request` 并关闭。**不实现 SSE** —— 组 3 只关心请求是否发出、body 里
/// 的 `"model"` 是什么。
struct MockListener {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockListener {
    fn start() -> Self {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind 回环 mock listener（沙箱可能禁止 bind）");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests_thread = requests.clone();
        let stop_thread = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_once(stream, &requests_thread);
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

    /// api_base（code 会在其后追加 `/chat/completions`）。
    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 已捕获请求的原始字节（请求行 + 头 + body）。
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockListener {
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
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                return value.trim().parse::<usize>().unwrap_or(0);
            }
        }
    }
    0
}

/// 处理一个连接：读请求行 / 头 / body，记录原始字节，回 400 后关闭。
fn serve_once(mut stream: TcpStream, requests: &Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
    // 监听 socket 是非阻塞的（accept 轮询 stop 标志需要）。macOS/BSD 上 `accept()`
    // 返回的连接**会继承**监听 socket 的 O_NONBLOCK，导致下面的 `set_read_timeout`
    // 被忽略、`read` 在客户端字节到达前就返回 EAGAIN。显式复位为阻塞模式。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut raw = Vec::new();
    let mut tmp = [0u8; 2048];
    let mut header_end = None;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find(&raw, b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    break;
                }
            }
            // 读超时 / 中断：记录已收到的部分即可。
            Err(_) => break,
        }
    }

    if let Some(end) = header_end {
        let content_length = headers_content_length(&raw[..end]);
        while raw.len() < end + content_length {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
    }

    requests
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&raw).into_owned());

    let head = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
    Ok(())
}

/// 捕获到的请求里是否有 body 的**顶层 `model` 字段**等于 `model`。
///
/// 从 HTTP 报文的 `\r\n\r\n` 之后取 body 并**解析为 JSON**，不依赖序列化形状 ——
/// 硬编码 `"model":"x"` 会漏掉带空格的 `"model": "x"`（紧凑 / pretty 皆须命中）。
fn model_sent(requests: &[String], model: &str) -> bool {
    requests.iter().any(|request| {
        let Some((_, body)) = request.split_once("\r\n\r\n") else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("model")?.as_str().map(str::to_string))
            .as_deref()
            == Some(model)
    })
}

/// 捕获到的请求里是否有任意一个请求行路径含 `/chat/completions`
/// （证明模型请求真的打到了 OpenAI 兼容端点，而非静默失败）。
fn hits_chat_completions(requests: &[String]) -> bool {
    requests
        .iter()
        .any(|request| request.contains("/chat/completions"))
}

// ---------------------------------------------------------------------------
// 组 4 —— `-p` / `--json` **成功**回合的收尾契约（设计 §9 / §6·T1·C7）
// ---------------------------------------------------------------------------

/// 一段最简成功 SSE：一个文本增量 `hi`、`finish_reason: "stop"`、`[DONE]`。
///
/// 逐字照搬 `truncation_guard.rs::MockServer` 已验证的帧形状（含 `index`，终止
/// chunk 只带 `finish_reason`）。
fn success_sse() -> String {
    let mut sse = String::new();
    sse.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
    );
    sse.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// 回环地址上的极简 **成功** mock：每个请求都回 [`success_sse`]，并记录请求原始字节。
///
/// 与组 3 的 [`MockListener`] 的唯一区别是响应体（200 + `text/event-stream` 而非
/// 400）—— 组 3 只关心「请求发了、`"model"` 对不对」，组 4 要跑通成功回合，
/// `persist_single_turn` 才会执行。
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

    /// api_base（code 会在其后追加 `/chat/completions`）。
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

/// 在临时 workspace 的 `.yushan/skills/<name>/SKILL.md` 放一份 skill，使
/// `expand_task` 命中（`Config::default_skill_roots` 的 `roots[0]` = `<cwd>/.yushan/skills`）。
fn write_skill(roots: &Roots, name: &str) {
    let dir = roots.workspace.join(".yushan").join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: 测试用\n---\n正文第一行\n"),
    )
    .unwrap();
}

/// `YUSHAN_SESSIONS_DIR` 下**唯一**的 `.jsonl` 文件（成功回合应恰生成一个）。
fn sole_jsonl(dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("sessions 目录应存在（会话已落盘）")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        1,
        "成功回合应恰有一个会话文件，实得：{files:?}"
    );
    files.pop().unwrap()
}

/// 读会话文件**首行** meta（`SessionMeta` 的 JSON 形状）。
fn read_meta_line(path: &Path) -> serde_json::Value {
    let content = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(content.lines().next().expect("会话文件应非空"))
        .expect("首行应是 meta JSON")
}

/// 捕获到的请求里 body 顶层 `"model"`（第一个命中的值）—— 用来钉住「meta.model
/// == 请求体里发出的 model」。
fn body_model(requests: &[String]) -> Option<String> {
    requests.iter().find_map(|request| {
        let (_, body) = request.split_once("\r\n\r\n")?;
        serde_json::from_str::<serde_json::Value>(body)
            .ok()?
            .get("model")?
            .as_str()
            .map(str::to_string)
    })
}

/// **组 4·G1a —— `-p` 成功回合的收尾契约**（端到端，子进程）。
///
/// 组 3 的 mock 恒回 400 ⇒ 回合失败、`persist_single_turn` 从未在测试里跑过。这里
/// 用回真实 SSE 的 mock 跑通成功回合，钉住设计 §9「`-p`/`--json` 落盘 ⇒ 必须在
/// **stderr** 打 `session: <id>`」与 §6/T1·C7「落盘文件首行有 title + model + tokens」。
///
/// 任务文本形如 `/git-status …` 且临时 workspace 里放了对应 skill ⇒ `expand_task`
/// 展开后送给模型，而 **meta.title 取展开前原文**（C3），不含 `<skill` 信封。
///
/// 变异：
/// - M1 把 `persist_single_turn` 的 `eprintln!("session: {id}")` 改成 `println!`
///   ⇒ stdout 混入 `session: …` → 「stdout 恰为模型文本」断言变红；
/// - M2 把 title 素材换成展开后文本 ⇒ title 断言变红。
#[test]
fn print_success_persists_meta_and_reports_session_to_stderr() {
    let server = SseMockServer::start();
    let roots = Roots::new("g1a_print");
    write_skill(&roots, "git-status");

    let task = "/git-status 现在改了什么";
    let api_base = server.api_base();
    let out = run_guarded_with_env(
        &["-p", task],
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
        "hi",
        "-p 的 stdout **恰为**模型文本（不得混入提示 / `session:` 等）：stdout:\n{stdout:?}\nstderr:\n{stderr}"
    );

    let path = sole_jsonl(&roots.sessions_dir);
    let id = path.file_stem().unwrap().to_str().unwrap();
    assert!(
        stderr.contains(&format!("session: {id}")),
        "stderr 应以 `session: <id>` 提示落盘文件（设计 §9）；stderr:\n{stderr}"
    );

    let meta = read_meta_line(&path);
    assert_eq!(meta["type"], "meta", "落盘首行必须是 meta：{meta}");
    assert_eq!(
        meta["title"], task,
        "title 取 skill 展开前原文（C3），且不含 <skill 信封：{meta}"
    );
    assert!(
        !meta["title"]
            .as_str()
            .unwrap_or_default()
            .contains("<skill"),
        "title 不得含 `<skill` 信封：{meta}"
    );
    assert_eq!(
        meta["model"].as_str(),
        body_model(&server.requests()).as_deref(),
        "meta.model 必须等于请求体里发出的 model；请求:\n{:#?}",
        server.requests()
    );
    assert_eq!(
        meta["model"], "mock-model",
        "model 应为 env 的 mock-model：{meta}"
    );
    assert!(
        meta["turns"].as_u64().unwrap_or(0) >= 1,
        "turns >= 1：{meta}"
    );

    roots.cleanup();
}

/// **组 4·G1b —— `--json` 成功回合的收尾契约**（端到端，子进程）。
///
/// 同一成功 mock；`--json` 的 stdout 是逐行 `Envelope`，**每一行都必须可解析为
/// JSON**（`session: <id>` 之类人读文本只许进 stderr）；落盘 meta 同样有 title / model。
///
/// 变异：M1 把 `session: <id>` 改成写 stdout ⇒ 出现不可解析行 → 逐行断言变红。
#[test]
fn json_success_keeps_stdout_pure_json_and_persists_meta() {
    let server = SseMockServer::start();
    let roots = Roots::new("g1b_json");
    let task = "现在改了什么";

    let api_base = server.api_base();
    let out = run_guarded_with_env(
        &["--json", task],
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
    assert!(
        !stdout.is_empty(),
        "成功回合应产出 Envelope 流；stderr:\n{stderr}"
    );
    for (i, line) in stdout.lines().enumerate() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
            panic!(
                "--json 的 stdout 第 {i} 行不是合法 JSON（不得混入人读文本）：{line:?}（{e}）\n\
                 stdout:\n{stdout}\nstderr:\n{stderr}"
            )
        });
    }

    let path = sole_jsonl(&roots.sessions_dir);
    let id = path.file_stem().unwrap().to_str().unwrap();
    assert!(
        stderr.contains(&format!("session: {id}")),
        "stderr 应以 `session: <id>` 提示落盘文件（设计 §9）；stderr:\n{stderr}"
    );

    let meta = read_meta_line(&path);
    assert_eq!(meta["type"], "meta", "落盘首行必须是 meta：{meta}");
    assert_eq!(meta["title"], task, "title 取任务原文：{meta}");
    assert_eq!(
        meta["model"].as_str(),
        body_model(&server.requests()).as_deref(),
        "meta.model 必须等于请求体里发出的 model；请求:\n{:#?}",
        server.requests()
    );
    assert_eq!(
        meta["model"], "mock-model",
        "model 应为 env 的 mock-model：{meta}"
    );
    assert!(
        meta["turns"].as_u64().unwrap_or(0) >= 1,
        "turns >= 1：{meta}"
    );

    roots.cleanup();
}

// ---------------------------------------------------------------------------
// 组 1 —— T4/C8：picker 输出不得污染 stdout
// ---------------------------------------------------------------------------

/// **组 1**：会话目录里有 1 个会话，跑 `--json --resume "hi"`。
///
/// `--json` 的 stdout 是逐行 `Envelope`，最容易被污染 —— 而 picker 在
/// `resolve_resume`（`main.rs`）里、`ensure_configured` 之前跑；非 TTY ⇒
/// `sessions::pick` 写列表 + 非交互提示后返回 `Err` ⇒ `eprintln!` + `exit(1)`。
///
/// 断言：退出码 ≠ 0；**stdout 必须为空**；stderr 含表头（`可恢复的会话`）、该会话的
/// **标题**与 **id**、以及 `非交互环境`（非 TTY 提示）。
///
/// 变异靶子：把 `pick` 的 `writeln!(err, …)` 改成 stdout ⇒ stdout 非空 ⇒ 红；
/// 表头 / 标题 / id / 非交互提示任一被删 ⇒ 对应断言红。
#[test]
fn resume_picker_keeps_json_stdout_pure_and_reports_to_stderr() {
    let roots = Roots::new("pick");
    write_session_file(
        &roots.sessions_dir,
        "sess-abc",
        "标题甲",
        "deepseek",
        Some("meta-model"),
    );

    let out = run_guarded(&["--json", "--resume", "hi"], &roots);
    let (stdout, stderr) = streams(&out);

    assert!(
        !out.status.success(),
        "非 TTY 的 picker 应以 exit(1) 收场；stderr:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "--json 的 stdout 必须是纯 Envelope 流，picker 输出不得污染；stdout:\n{stdout:?}"
    );
    assert!(
        stderr.contains("可恢复的会话"),
        "stderr 应含 picker 列表表头；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("标题甲"),
        "stderr 应含会话标题（meta.title）；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("sess-abc"),
        "stderr 应含会话 id（文件 stem）；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("非交互环境"),
        "非 TTY 应提示改用 --resume=<id>（`sessions.rs::pick`）；stderr:\n{stderr}"
    );

    roots.cleanup();
}

// ---------------------------------------------------------------------------
// 组 2 —— 空列表回落默认新会话
// ---------------------------------------------------------------------------

/// **组 2**：会话目录为空，跑 `-p --resume "hi"`（无凭证）。
///
/// 空列表**不是报错**：`resolve_resume` 写 `没有可恢复的会话` 到 stderr 后回落默认新会话。
/// 随后无凭证在 `ensure_configured` 早退（exit 1）。
///
/// 断言：退出码 = 1；stdout 为空；stderr 含 `没有可恢复的会话`，含 `No model configured`，
/// 且**前者出现在前**（证明回落发生在早退之前）。
///
/// 变异靶子：把空列表改成 `exit(1)` 报错 ⇒ 「没有可恢复的会话」缺席或「No model
/// configured」不再出现 ⇒ 红。
#[test]
fn resume_picker_empty_dir_falls_back_to_new_session() {
    let roots = Roots::new("empty");

    let out = run_guarded(&["-p", "--resume", "hi"], &roots);
    let (stdout, stderr) = streams(&out);

    assert_eq!(
        out.status.code(),
        Some(1),
        "无凭证应 exit(1)；stderr:\n{stderr}"
    );
    assert!(stdout.is_empty(), "stdout 应为空；stdout:\n{stdout:?}");

    let empty_at = stderr.find("没有可恢复的会话");
    let unconfigured_at = stderr.find("No model configured");
    assert!(
        empty_at.is_some(),
        "空列表应提示「没有可恢复的会话」（回落，不是报错）；stderr:\n{stderr}"
    );
    assert!(
        unconfigured_at.is_some(),
        "无凭证应走到「无模型配置」早退；stderr:\n{stderr}"
    );
    assert!(
        empty_at < unconfigured_at,
        "回落提示必须出现在早退之前，才证明回落真的发生在前；stderr:\n{stderr}"
    );

    roots.cleanup();
}

// ---------------------------------------------------------------------------
// 组 3 —— 模型恢复优先级（env > resume meta > state.json > 首个有凭证）
// ---------------------------------------------------------------------------

/// **组 3·C1 —— env 完整凭证胜出（env > resume meta > auth）。**
///
/// `YUSHAN_API_BASE / KEY / MODEL` 齐备 ⇒ `config.is_configured()` 为真 ⇒ 模型解析
/// **整段短路**。命令带 `--resume=s1`，故 `resume_meta` **确实非空**
/// （`provider=deepseek, model=meta-model`，且该 provider 有 auth entry），
/// 「env > resume meta」才真正被钉住；auth.json 指向**另一个不可达端口** —— env 若
/// 没胜出，请求会打到 meta / auth 的端口，listener 什么都收不到。
///
/// 断言：listener 捕获到请求体 `model == env-model`，路径含 `/chat/completions`；
/// 且**不含** meta 的 `meta-model` 与 auth 的 `auth-model`。
#[test]
fn model_recovery_env_wins_over_resume_meta_and_auth() {
    let server = MockListener::start();
    let roots = Roots::new("c1_env");
    write_auth_entry(
        &roots,
        "deepseek",
        "http://127.0.0.1:1/v1",
        "sk-unreachable",
        "auth-model",
    );
    write_session_file(
        &roots.sessions_dir,
        "s1",
        "会话一",
        "deepseek",
        Some("meta-model"),
    );

    let api_base = server.api_base();
    let out = run_guarded_with_env(
        &["-p", "--resume=s1", "hi"],
        &roots,
        &[
            ("YUSHAN_API_BASE", api_base.as_str()),
            ("YUSHAN_API_KEY", "sk-test"),
            ("YUSHAN_MODEL", "env-model"),
        ],
    );
    let (_stdout, stderr) = streams(&out);
    let requests = server.requests();

    assert!(
        model_sent(&requests, "env-model"),
        "env 凭证应胜出：请求体 model 须为 env-model；stderr:\n{stderr}\n捕获到的请求:\n{requests:#?}"
    );
    assert!(
        hits_chat_completions(&requests),
        "请求路径应含 /chat/completions；捕获到的请求:\n{requests:#?}"
    );
    assert!(
        !model_sent(&requests, "meta-model") && !model_sent(&requests, "auth-model"),
        "env 胜出时不得使用 meta / auth 的 model；捕获到的请求:\n{requests:#?}"
    );

    roots.cleanup();
}

/// **组 3·C2 —— resume meta 胜出（provider 有 auth entry，且 state.json 非空）。**
///
/// 清掉 env 凭证后，`config.is_configured()` 为假 ⇒ 走恢复：`resume_meta.provider =
/// "deepseek"` 有 auth entry（api_base 指向 listener），meta 的 `model` 非空 ⇒
/// 取 `meta-model`（`entry.model` 只作兜底）。
///
/// **本用例同时写非空 `state.json`**（`last_active_provider=deepseek,
/// last_active_model=state-model`）：`state.json` 非空 + `--resume=<id>` 是**实际使用
/// 后最常见**的场景 —— D10「resume 恢复会话记录的 model」正该守住它。若 state 分支
/// 抢先于 meta 分支，`state-model` 会盖过 `meta-model` ⇒ 本用例必须变红。
///
/// 断言：`model == meta-model`，路径含 `/chat/completions`；**不含** `state-model`，
/// 也**不含** `entry-fallback`。
#[test]
fn model_recovery_resume_meta_wins_over_state_and_entry() {
    let server = MockListener::start();
    let roots = Roots::new("c2_meta");
    let api_base = server.api_base();
    write_auth_entry(
        &roots,
        "deepseek",
        api_base.as_str(),
        "sk-test",
        "entry-fallback",
    );
    write_session_file(
        &roots.sessions_dir,
        "s2",
        "会话二",
        "deepseek",
        Some("meta-model"),
    );
    write_state_json(&roots, "deepseek", "state-model");

    let out = run_guarded(&["-p", "--resume=s2", "hi"], &roots);
    let (_stdout, stderr) = streams(&out);
    let requests = server.requests();

    assert!(
        model_sent(&requests, "meta-model"),
        "resume meta 应胜出：请求体 model 须为 meta-model；stderr:\n{stderr}\n捕获到的请求:\n{requests:#?}"
    );
    assert!(
        hits_chat_completions(&requests),
        "请求路径应含 /chat/completions；捕获到的请求:\n{requests:#?}"
    );
    assert!(
        !model_sent(&requests, "state-model"),
        "state.json 非空时 resume meta 仍须胜出，不得出现 state-model；捕获到的请求:\n{requests:#?}"
    );
    assert!(
        !model_sent(&requests, "entry-fallback"),
        "meta.model 非空时不得回落到 entry.model；捕获到的请求:\n{requests:#?}"
    );

    roots.cleanup();
}

/// **组 3·C2b —— resume meta 的 `model` 为 `null` ⇒ 回落 entry 的 model。**
///
/// 同一布局，仅把 meta 的 `model` 置为 `null`：`model.unwrap_or_else(|| entry.model)`
/// ⇒ `entry-fallback`。
#[test]
fn model_recovery_resume_meta_null_model_falls_back_to_entry() {
    let server = MockListener::start();
    let roots = Roots::new("c2b_meta_null");
    let api_base = server.api_base();
    write_auth_entry(
        &roots,
        "deepseek",
        api_base.as_str(),
        "sk-test",
        "entry-fallback",
    );
    write_session_file(&roots.sessions_dir, "s3", "会话三", "deepseek", None);

    let out = run_guarded(&["-p", "--resume=s3", "hi"], &roots);
    let (_stdout, stderr) = streams(&out);
    let requests = server.requests();

    assert!(
        model_sent(&requests, "entry-fallback"),
        "meta.model 为 null 时应回落 entry.model；stderr:\n{stderr}\n捕获到的请求:\n{requests:#?}"
    );
    assert!(
        hits_chat_completions(&requests),
        "请求路径应含 /chat/completions；捕获到的请求:\n{requests:#?}"
    );
    assert!(
        !model_sent(&requests, "meta-model"),
        "meta.model 为 null 时不应凭空出现 meta-model；捕获到的请求:\n{requests:#?}"
    );

    roots.cleanup();
}

/// **组 3·C2c —— 第 2 档降级：resume meta 的 provider 无 auth entry。**
///
/// `--resume=s4` 的会话 meta 写 `provider=ghost`（auth.json 里**没有** ghost
/// entry）⇒ 第 2 档（resume meta）**降级而非失败**，落回第 3 档 `state.json`
/// （`last_active_provider=deepseek` 有 entry）⇒ 取 `state-model`。
///
/// 断言：请求**真的发出**且路径含 `/chat/completions`；`model == state-model`；
/// **不含** ghost 的 `ghost-model`（未配置的 provider 不得被启用）。
#[test]
fn model_recovery_falls_back_to_state_when_meta_provider_unconfigured() {
    let server = MockListener::start();
    let roots = Roots::new("c2c_meta_ghost");
    let api_base = server.api_base();
    write_auth_entry(
        &roots,
        "deepseek",
        api_base.as_str(),
        "sk-test",
        "entry-fallback",
    );
    write_session_file(
        &roots.sessions_dir,
        "s4",
        "会话四",
        "ghost",
        Some("ghost-model"),
    );
    write_state_json(&roots, "deepseek", "state-model");

    let out = run_guarded(&["-p", "--resume=s4", "hi"], &roots);
    let (_stdout, stderr) = streams(&out);
    let requests = server.requests();

    assert!(
        !requests.is_empty(),
        "meta 的 provider 无 auth entry 时应降级并照常发出请求；stderr:\n{stderr}"
    );
    assert!(
        hits_chat_completions(&requests),
        "请求路径应含 /chat/completions；捕获到的请求:\n{requests:#?}"
    );
    assert!(
        model_sent(&requests, "state-model"),
        "无 auth entry 的 meta provider 应降级到 state.json 的 model；stderr:\n{stderr}\n捕获到的请求:\n{requests:#?}"
    );
    assert!(
        !model_sent(&requests, "ghost-model"),
        "未配置凭证的 provider 的 model 不得被启用；捕获到的请求:\n{requests:#?}"
    );

    roots.cleanup();
}

/// **组 3·C3 —— state.json 胜出。**
///
/// 无 env 凭证、无 `--resume`（会话目录留空）；`state.json` 记
/// `last_active_provider=deepseek, last_active_model=state-model`，auth entry
/// `model=entry-fallback` ⇒ 取 `state-model`（state 的 model 优先于 entry.model）。
#[test]
fn model_recovery_state_json_wins_over_auth_entry_model() {
    let server = MockListener::start();
    let roots = Roots::new("c3_state");
    let api_base = server.api_base();
    write_auth_entry(
        &roots,
        "deepseek",
        api_base.as_str(),
        "sk-test",
        "entry-fallback",
    );
    write_state_json(&roots, "deepseek", "state-model");

    let out = run_guarded(&["-p", "hi"], &roots);
    let (_stdout, stderr) = streams(&out);
    let requests = server.requests();

    assert!(
        model_sent(&requests, "state-model"),
        "state.json 应胜出：请求体 model 须为 state-model；stderr:\n{stderr}\n捕获到的请求:\n{requests:#?}"
    );
    assert!(
        hits_chat_completions(&requests),
        "请求路径应含 /chat/completions；捕获到的请求:\n{requests:#?}"
    );
    assert!(
        !model_sent(&requests, "entry-fallback"),
        "state.json 的 model 优先于 entry.model；捕获到的请求:\n{requests:#?}"
    );

    roots.cleanup();
}
