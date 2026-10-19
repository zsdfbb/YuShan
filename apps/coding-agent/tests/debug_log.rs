//! `--debug` 结构化执行日志的端到端验收（exec-plan Task 17）。
//!
//! 被测对象是**真实二进制**（`CARGO_BIN_EXE_ys-coding-agent`），对接一个
//! 回环地址上的极简 OpenAI 兼容 SSE 服务（`std::net::TcpListener`，不引依赖）。
//!
//! 覆盖的验收项：
//!
//! - **T1**：关闭 debug ⇒ 目标目录零 `debug-*.jsonl`；既有 `yushan.log` 的诊断内容
//!   与打开 debug 时**逐行相同**（时间数字归一化后比较 —— 逐轮耗时本身不确定，
//!   逐字节比较只在无逐轮行的单元路径成立，见 `main.rs`
//!   `debug_off_writes_no_file_and_leaves_existing_log_byte_identical`）。
//! - **T2**：`--json` 下 `RoundUsage` 是**加法**事件；过滤掉这些行后与基线二进制
//!   生成的 golden 逐字节相同。基线 golden 由 `write_baseline_golden` 生成。
//! - **T5（P0 部分）**：`flush` 后 debug 文件以完整 JSON 行结尾，最后一行为 `run_end`。
//! - **T7**：强制压缩的运行里 `purpose=turn` 与 `purpose=compaction` 同时出现。
//! - **T3/T4** 的结构性断言（`seq` 连号、`t_ms - dt_ms` 恒定、零增量记录）在
//!   同一次 debug 文件上顺带复核；队列满与 10k 增量的注入式验收在单元层
//!   （`debug_log.rs` / `round_clock.rs` 的测试）。
//!
//! 基线 golden 的再生成（需要 7987b4b 构建出的二进制）：
//!
//! ```text
//! YUSHAN_BASELINE_BIN=/path/to/baseline/ys-coding-agent \
//!   cargo test -p ys-coding-agent --test debug_log -- --ignored write_baseline_golden
//! ```

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const GOLDEN: &str = "tests/golden/json_no_debug_baseline.jsonl";

// ---------------------------------------------------------------------------
// 本地 stub：OpenAI 兼容 SSE
// ---------------------------------------------------------------------------

/// 一次脚本化响应的 usage。
#[derive(Clone, Copy)]
struct UsageSpec {
    prompt: u32,
    completion: u32,
    total: u32,
    /// `None` = 不带 `completion_tokens_details`（provider 未上报）；
    /// `Some(n)` = 显式上报（`n == 0` 也视为已上报）。
    reasoning: Option<u32>,
}

impl UsageSpec {
    const NONE: Self = Self {
        prompt: 11,
        completion: 7,
        total: 18,
        reasoning: None,
    };

    fn with_reasoning(reasoning: u32) -> Self {
        Self {
            reasoning: Some(reasoning),
            ..Self::NONE
        }
    }

    fn json(self) -> Value {
        let mut usage = json!({
            "prompt_tokens": self.prompt,
            "completion_tokens": self.completion,
            "total_tokens": self.total,
        });
        if let Some(reasoning) = self.reasoning {
            usage["completion_tokens_details"] = json!({"reasoning_tokens": reasoning});
        }
        usage
    }
}

/// 脚本里的一步响应。按请求顺序消费。
#[derive(Clone)]
enum Step {
    Text {
        text: String,
        usage: UsageSpec,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        usage: UsageSpec,
    },
}

impl Step {
    fn text(text: impl Into<String>, usage: UsageSpec) -> Self {
        Self::Text {
            text: text.into(),
            usage,
        }
    }

    fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
        usage: UsageSpec,
    ) -> Self {
        Self::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            usage,
        }
    }

    /// 构造该步的 SSE 响应体。
    fn sse(&self) -> String {
        let mut body = String::new();
        match self {
            Self::Text { text, usage } => {
                let chunk = json!({
                    "choices": [{"index": 0, "delta": {"content": text}}],
                });
                body.push_str(&format!("data: {chunk}\n\n"));
                let finish = json!({
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                });
                body.push_str(&format!("data: {finish}\n\n"));
                let usage = json!({"choices": [], "usage": usage.json()});
                body.push_str(&format!("data: {usage}\n\n"));
            }
            Self::ToolCall {
                id,
                name,
                arguments,
                usage,
            } => {
                let chunk = json!({
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments},
                            }],
                        },
                    }],
                });
                body.push_str(&format!("data: {chunk}\n\n"));
                let finish = json!({
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                });
                body.push_str(&format!("data: {finish}\n\n"));
                let usage = json!({"choices": [], "usage": usage.json()});
                body.push_str(&format!("data: {usage}\n\n"));
            }
        }
        body.push_str("data: [DONE]\n\n");
        body
    }
}

/// 回环地址上的极简 HTTP/1.1 服务：每个连接处理一个请求后关闭。
struct MockServer {
    port: u16,
    script: Arc<Mutex<VecDeque<(Step, Duration)>>>,
    chat_bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(steps: Vec<Step>) -> Self {
        Self::start_with_delays(steps.into_iter().map(|step| (step, Duration::ZERO)).collect())
    }

    /// 带逐请求延迟的版本：用于让 wall-clock 预算在模型调用**进行中**到点。
    fn start_with_delays(steps: Vec<(Step, Duration)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock（沙箱可能禁止 bind）");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let script = Arc::new(Mutex::new(VecDeque::from(steps)));
        let chat_bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let script_thread = Arc::clone(&script);
        let bodies_thread = Arc::clone(&chat_bodies);
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_once(stream, &script_thread, &bodies_thread);
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
            script,
            chat_bodies,
            stop,
            handle: Some(handle),
        }
    }

    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 已收到的 `/chat/completions` 请求体（按顺序）。
    fn chat_bodies(&self) -> Vec<String> {
        self.chat_bodies.lock().unwrap().clone()
    }

    fn remaining_steps(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl Drop for MockServer {
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

fn serve_once(
    mut stream: TcpStream,
    script: &Arc<Mutex<VecDeque<(Step, Duration)>>>,
    chat_bodies: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    // macOS/BSD 上 accept 的连接会继承监听 socket 的 O_NONBLOCK；显式复位，
    // 让 `set_read_timeout` 生效。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

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
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
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
    body.truncate(content_length);
    let body = String::from_utf8_lossy(&body).to_string();
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();

    if path.ends_with("/chat/completions") {
        chat_bodies.lock().unwrap().push(body);
        let step = script.lock().unwrap().pop_front();
        match step {
            Some((step, delay)) => {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                write_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    step.sse().as_bytes(),
                )
            }
            None => write_response(
                &mut stream,
                "500 Internal Server Error",
                "application/json",
                br#"{"error":{"message":"stub script exhausted"}}"#,
            ),
        }
    } else if path.ends_with("/models") {
        let body = br#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#;
        write_response(&mut stream, "200 OK", "application/json", body)
    } else {
        write_response(
            &mut stream,
            "404 Not Found",
            "application/json",
            br#"{"error":{"message":"not found"}}"#,
        )
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// 测试环境与子进程
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 每个用例独占的 HOME / sessions / logs / workdir。
///
/// 目录名含**进程内原子序号 + `process::id()`**（并行测试不得撞名）。
struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
    logs: PathBuf,
    workdir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yushan_debug_e2e_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let sessions = root.join("sessions");
        let logs = root.join("logs");
        let workdir = root.join("work");
        for dir in [&home, &sessions, &logs, &workdir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self {
            root,
            home,
            sessions,
            logs,
            workdir,
        }
    }

    /// 写 `$HOME/.yushan/config.json`。
    fn write_config(&self, value: &Value) {
        let dir = self.home.join(".yushan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_string_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn log_path(&self) -> PathBuf {
        self.logs.join("yushan.log")
    }

    fn debug_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.logs)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("debug-") && name.ends_with(".jsonl"))
            })
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 子进程无退出时长的硬上限：debug sink/writer 的回归最可能表现为挂死，
/// 没有看门狗时测试会永久阻塞（而不是变红）。
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// 子进程的 debug 开关来源。集成测试不继承开发机环境：每个用例显式选择
/// 「未设置（默认开启）」或「显式关闭」。
#[derive(Clone, Copy)]
enum DebugEnv {
    Unset,
    Off,
}

fn run_bin(bin: &Path, api_base: &str, sandbox: &Sandbox, args: &[&str]) -> Output {
    run_bin_with_debug_env(bin, api_base, sandbox, args, DebugEnv::Unset)
}

fn run_bin_with_debug_env(
    bin: &Path,
    api_base: &str,
    sandbox: &Sandbox,
    args: &[&str],
    debug_env: DebugEnv,
) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(&sandbox.workdir)
        .env("YUSHAN_API_BASE", api_base)
        .env("YUSHAN_API_KEY", "test-key")
        .env("YUSHAN_MODEL", "test-model")
        .env_remove("YUSHAN_LOG_DIR")
        .env("YUSHAN_LOG_DIR", &sandbox.logs)
        .env("YUSHAN_SESSIONS_DIR", &sandbox.sessions)
        .env("HOME", &sandbox.home)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        // 清掉父进程可能带的配置面变量，避免并行/本机环境影响信道与工具。
        .env_remove("YUSHAN_DEBUG")
        .env_remove("YUSHAN_DEBUG_ARGS")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        .env_remove("YUSHAN_SUBAGENTS_DIR")
        .env_remove("YUSHAN_WORKTREES_DIR")
        .env_remove("YUSHAN_FETCHED_DIR")
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_DEEPSEEK_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match debug_env {
        DebugEnv::Unset => {
            cmd.env_remove("YUSHAN_DEBUG");
        }
        DebugEnv::Off => {
            cmd.env("YUSHAN_DEBUG", "0");
        }
    }

    let mut child = cmd.spawn().expect("spawn ys-coding-agent");
    // 必须**边跑边读**管道：输出（压缩用例的 80 KB 用户消息）超过管道缓冲时，
    // 子进程会阻塞在 write 上而不是退出。reader 线程负责持续排空。
    let mut child_stdout = child.stdout.take().expect("stdout piped");
    let mut child_stderr = child.stderr.take().expect("stderr piped");
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let stdout_thread = {
        let buf = Arc::clone(&out_buf);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = child_stdout.read_to_end(&mut bytes);
            *buf.lock().unwrap() = bytes;
        })
    };
    let stderr_thread = {
        let buf = Arc::clone(&err_buf);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = child_stderr.read_to_end(&mut bytes);
            *buf.lock().unwrap() = bytes;
        })
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if started.elapsed() >= CHILD_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                panic!(
                    "子进程超过 {CHILD_TIMEOUT:?} 未退出（疑似死锁）\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&out_buf.lock().unwrap()),
                    String::from_utf8_lossy(&err_buf.lock().unwrap())
                );
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    Output {
        status,
        stdout: out_buf.lock().unwrap().clone(),
        stderr: err_buf.lock().unwrap().clone(),
    }
}

fn current_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ys-coding-agent"))
}

// ---------------------------------------------------------------------------
// 解析辅助
// ---------------------------------------------------------------------------

/// 逐行解析 `--json` 输出。
fn envelopes(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("--json 每行必须是合法 JSON"))
        .collect()
}

/// 取某个事件变体的 payload（如 `"RoundUsage"`）。
fn event<'a>(envelope: &'a Value, name: &str) -> Option<&'a Value> {
    envelope.get("event")?.get(name)
}

/// 过滤掉 `RoundUsage` 行，保留原始字节行序列（T2 的加法契约）。
fn without_round_usage(stdout: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let is_round_usage = serde_json::from_slice::<Value>(line)
            .ok()
            .and_then(|value| value.get("event").cloned())
            .is_some_and(|event| event.get("RoundUsage").is_some());
        if is_round_usage {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// 归一化 `yushan.log` 中的不确定数字：unix 秒前缀与逐轮耗时。
///
/// 比较的是**诊断内容的结构**（行种类、顺序、除时间外的字段），而非计时值 ——
/// 逐轮耗时由真实调度决定，不可能逐字节复现。
fn normalize_log(text: &str) -> String {
    text.lines()
        .map(|line| {
            // `[unix秒] ...` 前缀
            let line = match line.strip_prefix('[') {
                Some(rest) => match rest.split_once("] ") {
                    Some((_, tail)) => format!("[t] {tail}"),
                    None => line.to_string(),
                },
                None => line.to_string(),
            };
            if line.starts_with("[t] round=") || line.starts_with("[t] tools round=") {
                line.chars()
                    .map(|ch| if ch.is_ascii_digit() { '#' } else { ch })
                    .collect()
            } else if line.starts_with("[t] turn=") {
                // 两次独立进程的整数秒读数可能跨秒（`elapsed=0s` vs `1s`），
                // 只归一化这两个**时间**字段；rounds / cumulative / cache 仍精确比较。
                line.split(' ')
                    .map(|token| match token.split_once('=') {
                        Some((key, _)) if key == "elapsed" || key == "budget_remaining" => {
                            format!("{key}=#")
                        }
                        _ => token.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析 debug JSONL：每行一个对象。
fn debug_records(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("debug 文件应可读");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("debug 每行必须是合法 JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// T1：关闭 debug 的负向验收
// ---------------------------------------------------------------------------

#[test]
fn t1_debug_off_creates_no_debug_file_and_keeps_existing_diagnostics() {
    let marker = b"pre-existing log line\n";

    // 同一条确定性脚本各跑一次：debug 关 / debug 开。
    let run = |debug: bool| {
        let server = MockServer::start(vec![Step::text("hi", UsageSpec::NONE)]);
        let sandbox = Sandbox::new(if debug { "t1_on" } else { "t1_off" });
        std::fs::write(sandbox.log_path(), marker).unwrap();

        let mut args = vec!["-p"];
        if debug {
            args.push("--debug");
        }
        args.push("hi");
        let out = if debug {
            run_bin(&current_bin(), &server.api_base(), &sandbox, &args)
        } else {
            run_bin_with_debug_env(
                &current_bin(),
                &server.api_base(),
                &sandbox,
                &args,
                DebugEnv::Off,
            )
        };
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            out.status.success(),
            "运行失败（debug={debug}）\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert_eq!(server.remaining_steps(), 0, "stub 脚本必须被完整消费");
        (sandbox, stdout, stderr)
    };

    let (off, off_stdout, off_stderr) = run(false);
    let (on, on_stdout, on_stderr) = run(true);

    // `-p` 的 stdout 只有模型文本；debug 内容只进文件。
    assert_eq!(on_stdout, "hi\n", "debug 开时 `-p` 的 stdout 必须保持干净");
    assert_eq!(off_stdout, "hi\n");
    for (label, stderr) in [("off", &off_stderr), ("on", &on_stderr)] {
        assert!(
            !stderr.contains("yushan.debug/1") && !stderr.contains("\"ev\":"),
            "{label}: debug 内容不得写 stderr：{stderr}"
        );
    }

    // T1 的核心断言：关闭时零 debug 文件。
    assert!(
        off.debug_files().is_empty(),
        "debug 关时不得出现 debug 文件：{:?}",
        off.debug_files()
    );
    assert_eq!(
        on.debug_files().len(),
        1,
        "debug 开时应恰好一个文件：{:?}",
        on.debug_files()
    );

    // 既有诊断输出：预置内容原样保留为前缀。
    let off_log = std::fs::read(off.log_path()).unwrap();
    assert!(
        off_log.starts_with(marker),
        "yushan.log 不得被覆盖：{:?}",
        String::from_utf8_lossy(&off_log)
    );

    // 打开 debug 不得改变既有 `yushan.log` 内容（逐轮耗时归一化后逐行相同）。
    let off_text = std::fs::read_to_string(off.log_path()).unwrap();
    let on_text = std::fs::read_to_string(on.log_path()).unwrap();
    assert_eq!(
        normalize_log(&off_text),
        normalize_log(&on_text),
        "debug 开关不得改变既有诊断输出\n--- off ---\n{off_text}\n--- on ---\n{on_text}"
    );
}

/// 未设置 `YUSHAN_DEBUG` 且不带 flag 时，两种单回合模式都应默认生成一个
/// 独立的 debug 文件，并以完整 `run_end` 收尾。
#[test]
fn default_debug_env_writes_complete_run_end_for_print_and_json() {
    for (tag, args) in [
        ("default_print", ["-p", "hi"]),
        ("default_json", ["--json", "hi"]),
    ] {
        let server = MockServer::start(vec![Step::text("hi", UsageSpec::NONE)]);
        let sandbox = Sandbox::new(tag);

        let out = run_bin(&current_bin(), &server.api_base(), &sandbox, &args);
        assert!(
            out.status.success(),
            "{tag}: 默认 debug 不得改变运行结果\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            server.remaining_steps(),
            0,
            "{tag}: mock 脚本必须被完整消费"
        );

        let files = sandbox.debug_files();
        assert_eq!(
            files.len(),
            1,
            "{tag}: 默认开启时应恰好一个 debug 文件：{files:?}"
        );
        let raw = std::fs::read(&files[0]).unwrap();
        assert!(
            raw.ends_with(b"\n"),
            "{tag}: debug 文件必须以完整 JSON 行收尾"
        );
        let records = debug_records(&files[0]);
        let run_end = records
            .last()
            .unwrap_or_else(|| panic!("{tag}: debug 文件不得为空"));
        assert_eq!(
            run_end["ev"],
            json!("run_end"),
            "{tag}: 最后一行应为 run_end"
        );
        assert_eq!(
            run_end["stop_reason"],
            json!("RunFinished"),
            "{tag}: run_end 必须有终局原因"
        );
        assert_eq!(
            run_end["exit_code"],
            json!(0),
            "{tag}: run_end 必须记录成功退出码"
        );
        assert!(
            !sandbox.home.join(".yushan").join("logs").exists(),
            "{tag}: 显式 YUSHAN_LOG_DIR 不得把日志写回 HOME"
        );
    }
}

// ---------------------------------------------------------------------------
// T2：`--json` 加法契约（对基线 golden）
// ---------------------------------------------------------------------------

/// 用确定性脚本跑一次 `--json`（不带 `--debug`）。
fn t2_run(bin: &Path) -> (Output, Sandbox) {
    let server = MockServer::start(vec![Step::text("hi", UsageSpec::NONE)]);
    let sandbox = Sandbox::new("t2");
    let out = run_bin(bin, &server.api_base(), &sandbox, &["--json", "hi"]);
    assert!(
        out.status.success(),
        "运行失败\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(server.remaining_steps(), 0, "脚本必须被完整消费");
    (out, sandbox)
}

#[test]
fn t2_json_is_additive_and_filtered_output_matches_baseline_golden() {
    let (out, _sandbox) = t2_run(&current_bin());
    let stdout = String::from_utf8(out.stdout).expect("stdout 应是 UTF-8");

    // 每个模型轮恰好一条 `purpose=turn` 的 `RoundUsage`。
    let events = envelopes(&stdout);
    let round_started = events
        .iter()
        .filter(|envelope| event(envelope, "RoundStarted").is_some())
        .count();
    let all_usage: Vec<&Value> = events
        .iter()
        .filter_map(|envelope| event(envelope, "RoundUsage"))
        .collect();
    let turn_usage = all_usage
        .iter()
        .filter(|payload| payload["purpose"] == json!("turn"))
        .count();
    assert_eq!(round_started, 1, "脚本只有一轮");
    assert_eq!(
        all_usage.len(),
        round_started,
        "普通回合不得出现额外的 RoundUsage（含 compaction）：{all_usage:?}"
    );
    assert_eq!(turn_usage, round_started, "每轮恰好一条 Turn RoundUsage");

    // `reasoning_tokens: None` 时不得写出该键（`skip_serializing_if` 承重契约）。
    assert!(
        !stdout.contains("reasoning_tokens"),
        "provider 未上报时不得出现 reasoning_tokens 键\nstdout:\n{stdout}"
    );

    // 过滤 `RoundUsage` 行后与基线二进制生成的 golden 逐字节相同。
    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    let golden = std::fs::read(&golden_path).unwrap_or_else(|e| {
        panic!(
            "缺少基线 golden（{e}）：用 7987b4b 构建的二进制运行\n\
             YUSHAN_BASELINE_BIN=... cargo test -p ys-coding-agent --test debug_log -- \
             --ignored write_baseline_golden"
        )
    });
    let filtered = without_round_usage(stdout.as_bytes());
    assert_eq!(
        String::from_utf8_lossy(&filtered),
        String::from_utf8_lossy(&golden),
        "过滤 RoundUsage 后必须与基线逐字节相同"
    );
}

/// 生成 T2 的基线 golden（手动、`#[ignore]`）。
///
/// 必须用**改动前**（基线 `7987b4b`）构建的二进制运行；用本工作树二进制会
/// 把被测实现自己的输出固化成「基线」，恒等比较就失去意义。
#[test]
#[ignore = "手动生成基线 golden：需要 7987b4b 构建出的二进制"]
fn write_baseline_golden() {
    let bin = std::env::var_os("YUSHAN_BASELINE_BIN")
        .map(PathBuf::from)
        .expect("必须设置 YUSHAN_BASELINE_BIN 指向基线二进制");
    let (out, _sandbox) = t2_run(&bin);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &out.stdout).unwrap();
    eprintln!("wrote {} ({} bytes)", path.display(), out.stdout.len());
}

// ---------------------------------------------------------------------------
// T7：压缩覆盖
// ---------------------------------------------------------------------------

#[test]
fn t7_compaction_and_turn_usage_appear_together() {
    // 小窗口 + 超长用户消息：第一轮工具调用后必然越过触发点。
    let sandbox = Sandbox::new("t7");
    sandbox.write_config(&json!({"context_window": {"test-model": 20_000}}));

    let summary_usage = UsageSpec {
        prompt: 111,
        completion: 22,
        total: 133,
        reasoning: Some(5),
    };
    let server = MockServer::start(vec![
        Step::tool_call(
            "call_1",
            "bash",
            r#"{"command":"echo ok"}"#,
            UsageSpec::NONE,
        ),
        Step::text("summary", summary_usage),
        Step::text("done", UsageSpec::NONE),
    ]);

    let task = "x".repeat(80_000);
    let out = run_bin(
        &current_bin(),
        &server.api_base(),
        &sandbox,
        &["--json", task.as_str()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "运行失败\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = envelopes(&stdout);
    let usages: Vec<(String, u32, Value)> = events
        .iter()
        .filter_map(|envelope| event(envelope, "RoundUsage"))
        .map(|payload| {
            (
                payload["purpose"].as_str().unwrap_or("").to_string(),
                payload["round"].as_u64().unwrap_or(0) as u32,
                payload["usage"].clone(),
            )
        })
        .collect();

    let turns: Vec<u32> = usages
        .iter()
        .filter(|(purpose, _, _)| purpose == "turn")
        .map(|(_, round, _)| *round)
        .collect();
    let compacted: Vec<&(String, u32, Value)> = usages
        .iter()
        .filter(|(purpose, _, _)| purpose == "compaction")
        .collect();

    assert_eq!(turns, vec![1, 2], "两个模型轮各一条 Turn usage：{usages:?}");
    assert_eq!(compacted.len(), 1, "压缩摘要调用恰好一条：{usages:?}");
    let (_, compaction_round, usage) = compacted[0];
    assert_eq!(
        *compaction_round, 2,
        "压缩记「正在准备的下一轮」：{usages:?}"
    );
    assert_eq!(
        usage,
        // `Usage` 的序列化形态只有这些字段：`total_tokens` 由 provider 上报，
        // 但落进 `Usage` 时已折算为 input + output，不再单独保留键。
        &json!({
            "input_tokens": 111,
            "output_tokens": 22,
            "reasoning_tokens": 5,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 0,
        }),
        "压缩 usage 必须等于摘要调用的 usage：{usage}"
    );

    // 压缩事件存在，且之后才出现第 2 轮 `RoundStarted`。
    let compacted_at = events
        .iter()
        .position(|envelope| event(envelope, "ContextCompacted").is_some())
        .expect("应出现 ContextCompacted");
    let round_two_at = events
        .iter()
        .position(|envelope| {
            event(envelope, "RoundStarted").and_then(|r| r["round"].as_u64()) == Some(2)
        })
        .expect("应出现第 2 轮 RoundStarted");
    assert!(compacted_at < round_two_at, "压缩必须发生在第 2 轮开始之前");

    assert_eq!(server.chat_bodies().len(), 3, "工具轮 + 摘要 + 收尾轮");
}

// ---------------------------------------------------------------------------
// T5（P0 部分）+ T3/T4 的结构性复核：debug 文件
// ---------------------------------------------------------------------------

#[test]
fn t5_debug_file_is_complete_and_structured() {
    let sandbox = Sandbox::new("t5");
    let server = MockServer::start(vec![Step::text("hello", UsageSpec::with_reasoning(4))]);

    let out = run_bin(
        &current_bin(),
        &server.api_base(),
        &sandbox,
        &["--json", "--debug", "hi"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "运行失败\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(server.remaining_steps(), 0, "stub 脚本必须被完整消费");

    // Task 13：debug 只进文件 —— stdout 仍是逐行 Envelope，stderr 无 debug 内容。
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("`--json` stdout 必须是合法 JSON：{e}: {line}"));
        assert!(value.get("event").is_some(), "stdout 只应有 Envelope：{line}");
        assert!(
            !line.contains("yushan.debug/1"),
            "debug 记录不得混入 stdout：{line}"
        );
    }
    assert!(
        !stderr.contains("yushan.debug/1") && !stderr.contains("\"ev\":"),
        "debug 内容不得写 stderr：{stderr}"
    );

    let files = sandbox.debug_files();
    assert_eq!(files.len(), 1, "debug 开时应恰好一个文件：{files:?}");
    let path = &files[0];

    // flush 后以完整 JSON 行结尾（T5 的 P0 部分）。
    let raw = std::fs::read(path).unwrap();
    assert!(raw.ends_with(b"\n"), "debug 文件必须以换行结尾（最后一行完整）");

    let records = debug_records(path);
    assert!(!records.is_empty(), "debug 文件不得为空");
    assert_eq!(
        records.last().unwrap()["ev"],
        json!("run_end"),
        "最后一行必须是 run_end"
    );
    let run_end = records.last().unwrap();
    assert_eq!(run_end["rounds"], json!(1));
    assert_eq!(run_end["exit_code"], json!(0));
    assert_eq!(run_end["stop_reason"], json!("RunFinished"));
    assert!(run_end["duration_ms"].is_u64(), "run_end 必须有 duration_ms");
    assert_eq!(run_end["usage"]["input_tokens"], json!(11));
    assert_eq!(run_end["usage"]["reasoning_tokens"], json!(4));
    assert_eq!(run_end["debug"]["dropped"], json!(0));
    assert_eq!(run_end["debug"]["serde_failures"], json!(0));

    // 每行合法 JSON、schema 一致、seq 连号（T3 的结构性部分）。
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["schema"], json!("yushan.debug/1"));
        assert_eq!(record["seq"], json!(index as u64), "seq 必须从 0 连号");
        assert!(record["t_ms"].is_u64() && record["dt_ms"].is_u64());
        assert!(
            record["t_ms"].as_u64().unwrap() >= record["dt_ms"].as_u64().unwrap(),
            "t_ms 由 run_start_unix_ms + dt_ms 派生"
        );
    }
    // `t_ms - dt_ms` 在整个文件里恒定（run_start 的原点）。
    let bases: Vec<u64> = records
        .iter()
        .map(|record| record["t_ms"].as_u64().unwrap() - record["dt_ms"].as_u64().unwrap())
        .collect();
    assert!(
        bases.iter().all(|base| *base == bases[0]),
        "t_ms - dt_ms 必须恒定：{bases:?}"
    );

    // 段记录齐全，且**零增量记录**（T3：只记段，不记增量）。
    let names: Vec<&str> = records
        .iter()
        .filter_map(|record| record["ev"].as_str())
        .collect();
    for expected in [
        "run_start",
        "turn_start",
        "round_start",
        "model_attempt",
        "round_usage",
        "round_end",
        "turn_end",
    ] {
        assert!(
            names.contains(&expected),
            "缺少 {expected} 记录：{names:?}"
        );
    }
    // 允许名单而非「名字不含 delta」：后者会放过任何新造的增量类记录名。
    const ALLOWED: [&str; 8] = [
        "run_start",
        "turn_start",
        "round_start",
        "model_attempt",
        "round_usage",
        "round_end",
        "turn_end",
        "run_end",
    ];
    assert!(
        names.iter().all(|name| ALLOWED.contains(name)),
        "只允许段记录；增量事件不得产生记录：{names:?}"
    );

    // `round_usage` 携带 reasoning_tokens 的三种形态（此处为 Some(4)）。
    let usage = records
        .iter()
        .find(|record| record["ev"] == json!("round_usage"))
        .expect("应有 round_usage 记录");
    assert_eq!(usage["gen_ai.usage.reasoning.output_tokens"], json!(4));
    assert_eq!(usage["reasoning_tokens_reported"], json!(true));
    assert_eq!(usage["purpose"], json!("turn"));

    // 适配器尝试读数必须经 app 路由器落盘；成功尝试有真实首字节延迟。
    let attempt = records
        .iter()
        .find(|record| record["ev"] == json!("model_attempt"))
        .expect("应有 model_attempt 记录");
    assert_eq!(attempt["attempt"], json!(1));
    assert_eq!(attempt["max_attempts"], json!(6));
    assert_eq!(attempt["outcome"], json!("success"));
    assert_eq!(attempt["http_status"], json!(200));
    assert!(attempt["ttfb_ms"].is_u64(), "成功尝试应有 ttfb_ms：{attempt}");
    assert_eq!(attempt["retried"], json!(false));
}

// ---------------------------------------------------------------------------
// 失败退出 / 预算边界 / 跨进程 resume 的 run 号
// ---------------------------------------------------------------------------

/// 模型错误走 `?` 退出时也必须 flush：`run_end` 与队列尾部不能丢。
///
/// 变异：删掉 main 里错误分支的 `exit_with_run_end`（恢复裸 `?`）⇒ 最后一行
/// 不再是 `run_end`。
#[test]
fn failed_run_still_writes_run_end() {
    let sandbox = Sandbox::new("failed_run");
    // 不重试，避免连接拒绝时 5 次退避拖慢用例。
    sandbox.write_config(&json!({"model_retry": {"max_attempts": 0}}));

    let out = run_bin(
        &current_bin(),
        "http://127.0.0.1:1/v1",
        &sandbox,
        &["--json", "--debug", "hi"],
    );
    assert!(!out.status.success(), "失败运行必须非零退出");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Error:"), "错误仍要可见：{stderr}");

    let files = sandbox.debug_files();
    assert_eq!(files.len(), 1, "debug 开时应恰好一个文件：{files:?}");
    let records = debug_records(&files[0]);
    let run_end = records.last().expect("debug 文件不得为空");
    assert_eq!(run_end["ev"], json!("run_end"), "失败退出也必须写 run_end");
    assert_eq!(run_end["stop_reason"], json!("RunFailed"));
    assert_eq!(run_end["exit_code"], json!(1));
    assert!(
        run_end["rounds"].as_u64().unwrap() >= 1,
        "失败前已开始过一轮：{run_end}"
    );

    // P2-2：`turn_end.rounds` 失败时取事件流累计，而不是恒 0。
    let turn_end = records
        .iter()
        .find(|record| record["ev"] == json!("turn_end"))
        .expect("失败回合也应有 turn_end");
    assert!(
        turn_end["rounds"].as_u64().unwrap() >= 1,
        "失败回合的 turn_end.rounds 必须来自事件流：{turn_end}"
    );
}

/// 预算 Abort：`enqueued` 必须与 `push` 同刻写（而不是回合收尾后补），
/// 且回合以 `Cancelled` 收束时补 `observed`。
#[test]
fn budget_abort_records_enqueued_and_observed() {
    let sandbox = Sandbox::new("budget");
    sandbox.write_config(&json!({"max_turn_duration_secs": 1}));

    // 第一轮模型调用延迟 2.5 s：预算在 await 模型期间到点。
    // 响应是工具调用 ⇒ 工具执行后，下一轮边界看到 Abort ⇒ Cancelled。
    let server = MockServer::start_with_delays(vec![
        (
            Step::tool_call("call_1", "bash", r#"{"command":"echo ok"}"#, UsageSpec::NONE),
            Duration::from_millis(2500),
        ),
        (Step::text("done", UsageSpec::NONE), Duration::ZERO),
    ]);

    let out = run_bin(
        &current_bin(),
        &server.api_base(),
        &sandbox,
        &["--json", "--debug", "hi"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "预算到点 ⇒ 退出码 1\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let files = sandbox.debug_files();
    assert_eq!(files.len(), 1, "debug 开时应恰好一个文件：{files:?}");
    let records = debug_records(&files[0]);
    let boundaries: Vec<&Value> = records
        .iter()
        .filter(|record| record["ev"] == json!("boundary"))
        .collect();
    let enqueued = boundaries
        .iter()
        .find(|record| record["phase"] == json!("enqueued") && record["origin"] == json!("budget"))
        .expect("预算 Abort 必须有 enqueued 记录");
    let observed = boundaries
        .iter()
        .find(|record| record["phase"] == json!("observed") && record["kind"] == json!("abort"))
        .expect("Cancelled 回合必须补 observed 记录");
    assert!(
        enqueued["t_ms"].as_u64() <= observed["t_ms"].as_u64(),
        "enqueued 不得晚于 observed：{enqueued} / {observed}"
    );

    // `enqueued` 必须在回合收尾之前（与 push 同刻）。
    let turn_end_at = records
        .iter()
        .position(|record| record["ev"] == json!("turn_end"))
        .expect("应有 turn_end");
    let enqueued_at = records
        .iter()
        .position(|record| {
            record["ev"] == json!("boundary")
                && record["phase"] == json!("enqueued")
                && record["origin"] == json!("budget")
        })
        .expect("应有 boundary");
    assert!(enqueued_at < turn_end_at, "enqueued 早于回合收尾");

    let run_end = records.last().unwrap();
    assert_eq!(run_end["ev"], json!("run_end"));
    assert_eq!(run_end["stop_reason"], json!("RunFinished"));
    assert_eq!(run_end["exit_code"], json!(1));
}

/// 跨进程 `--resume` 追加同一 debug 文件时，`run` 必须递增（不能两条 `run=0`）。
#[test]
fn resume_across_processes_increments_run() {
    let sandbox = Sandbox::new("resume_run");

    // 第一次运行建立会话。
    let server = MockServer::start(vec![Step::text("one", UsageSpec::NONE)]);
    let first = run_bin(
        &current_bin(),
        &server.api_base(),
        &sandbox,
        &["-p", "--debug", "hi"],
    );
    assert!(first.status.success(), "第一次运行失败");

    let session = std::fs::read_dir(&sandbox.sessions)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("第一次运行应落盘会话");
    let session_id = session.file_stem().unwrap().to_str().unwrap().to_string();

    // 第二次运行恢复到同一会话。
    let server = MockServer::start(vec![Step::text("two", UsageSpec::NONE)]);
    let second = run_bin(
        &current_bin(),
        &server.api_base(),
        &sandbox,
        &["-p", "--debug", "--resume", &session_id, "hi"],
    );
    assert!(
        second.status.success(),
        "resume 运行失败：{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let files = sandbox.debug_files();
    assert_eq!(files.len(), 1, "同一会话应只有一个 debug 文件：{files:?}");
    let records = debug_records(&files[0]);
    let runs: Vec<u64> = records
        .iter()
        .filter(|record| record["ev"] == json!("run_start"))
        .filter_map(|record| record["run"].as_u64())
        .collect();
    assert_eq!(runs, vec![0, 1], "跨进程 resume 的 run 必须递增：{runs:?}");
}
