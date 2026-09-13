//! 流式 `usage` 门控与 400 一次性回退的端到端验收。
//!
//! 背景：流式改造后 `usage` 依赖 `ProviderCompat::supports_stream_usage`。
//! env-var 配置（`YUSHAN_API_BASE/KEY`）下 provider 落为 `custom` → `standard()`。
//! 本文件覆盖两件事：
//!
//! 1. `standard()` 默认开启 `stream_options.include_usage` → env-var 用户也能拿到
//!    token 统计（回归的另一半）。
//! 2. 严格校验的端点若因该字段返回 400，则剥掉该字段**只重试一次**，请求成功但
//!    本次 usage 为 0；若请求本来就没带该字段，则 400 直接失败、不重试。
//!
//! 本地 mock 只需回环地址，运行前需绕过代理：子进程测试经 `Command` 的
//! per-child `.env()` 注入（不碰父进程 env）；进程内测试用 `reqwest::Client`
//! 的 `.no_proxy()` 直连，均不改写进程级环境变量——避免同一测试二进制内
//! 并行执行时互相污染。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ys_model::{Model, ModelEvent, ModelEventSink, ModelRequest};
use ys_model_openai_compat::{
    OpenAICompatibleConfig, OpenAICompatibleModel, compat::ProviderCompat,
};

// ---------------------------------------------------------------------------
// 本地 mock HTTP 服务
// ---------------------------------------------------------------------------

/// 一次 mock 运行的配置。
struct MockConfig {
    /// true：带 `stream_options` 的请求返回 400；false：一律放行。
    reject_stream_options: bool,
    /// true：任何请求都返回 400（用于验证「不带该字段遇 400 不重试」）。
    reject_all: bool,
    /// 成功响应的 SSE 末包是否附带 usage。
    emit_usage: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            reject_stream_options: false,
            reject_all: false,
            emit_usage: true,
        }
    }
}

/// 回环地址上的极简 HTTP/1.1 mock：每个连接处理一个请求后关闭。
///
/// 用 std 手写而非引入测试依赖；`Connection: close` 保证 reqwest 每次请求新开连接，
/// 从而两个请求可被分别记录、计数。
struct MockServer {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(cfg: MockConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let bodies_thread = bodies.clone();
        let stop_thread = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_once(
                            stream,
                            cfg.reject_stream_options,
                            cfg.reject_all,
                            cfg.emit_usage,
                            &bodies_thread,
                        );
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
            bodies,
            stop,
            handle: Some(handle),
        }
    }

    /// api_base（code 会在其后追加 `/chat/completions`）。
    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn request_bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
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

/// 处理一个连接：读请求行/头/体，记录 body，按配置回 400 或 SSE。
fn serve_once(
    mut stream: TcpStream,
    reject_stream_options: bool,
    reject_all: bool,
    emit_usage: bool,
    bodies: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    // 监听 socket 是非阻塞的（accept 轮询 stop 标志需要）。在 macOS/BSD 上
    // `accept()` 返回的连接**会继承**监听 socket 的 O_NONBLOCK，导致下面的
    // `set_read_timeout` 被忽略、`read` 在客户端字节到达前就返回 EAGAIN——
    // mock 随即断开连接，客户端报 "connection closed before message completed"。
    // 显式复位为阻塞模式，让 `set_read_timeout` 真正生效。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // 读头部直到 \r\n\r\n
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
    body.truncate(content_length);
    let body = String::from_utf8_lossy(&body).to_string();
    bodies.lock().unwrap().push(body.clone());

    let has_stream_options = body.contains("stream_options");
    if reject_all || (reject_stream_options && has_stream_options) {
        let payload = br#"{"error":{"message":"unknown field: stream_options"}}"#;
        write_response(&mut stream, "400 Bad Request", "application/json", payload)
    } else {
        let mut sse = String::new();
        sse.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n");
        sse.push_str(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        if emit_usage {
            sse.push_str(
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n",
            );
        }
        sse.push_str("data: [DONE]\n\n");
        write_response(&mut stream, "200 OK", "text/event-stream", sse.as_bytes())
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
// `--json` 输出解析
// ---------------------------------------------------------------------------

/// 取 `--json` 输出中某事件（如 `RunFinished`）的 payload。
fn event_payload(stdout: &str, name: &str) -> Option<serde_json::Value> {
    stdout.lines().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        v.get("event")?.get(name).cloned()
    })
}

// ---------------------------------------------------------------------------
// 子进程辅助
// ---------------------------------------------------------------------------

fn isolate_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("yushan_usage_fallback_{tag}"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// 以 env-var 配置（→ custom → standard）启动二进制，跑 `--json <task>`。
fn run_json(api_base: &str, task: &str, home: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"))
        .args(["--json", task])
        .env("YUSHAN_API_BASE", api_base)
        .env("YUSHAN_API_KEY", "test-key")
        .env("YUSHAN_MODEL", "test-model")
        .env("HOME", home)
        // 回环地址需绕过代理；同时清掉可能存在的代理变量。
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .output()
        .expect("spawn ys-coding-agent")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 1. env-var 配置（custom → standard）＋容忍 `stream_options` 的 mock：
///    token 统计必须正确（不再是 0），且请求带了 `stream_options`。
#[test]
fn env_var_config_captures_stream_usage() {
    let server = MockServer::start(MockConfig::default());
    let home = isolate_home("tolerant");

    let out = run_json(&server.api_base(), "hi", &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "exit != 0\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        event_payload(&stdout, "ModelTextDelta").is_some(),
        "应收到 ModelTextDelta\nstdout:\n{stdout}"
    );

    let finished = event_payload(&stdout, "RunFinished")
        .unwrap_or_else(|| panic!("应收到 RunFinished\nstdout:\n{stdout}"));
    let usage = &finished["usage"];
    assert_eq!(
        usage["input_tokens"], 11,
        "env-var 用户 usage 不应为 0。RunFinished = {finished}"
    );
    assert_eq!(usage["output_tokens"], 7);

    // 请求确实带了 stream_options（standard 默认开启）。
    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 1, "预期一次请求，实际 {:?}", bodies.len());
    assert!(
        bodies[0].contains("stream_options"),
        "standard() 应默认携带 stream_options，body = {}",
        bodies[0]
    );
    // 容忍该字段的端点不该触发回退警告。
    assert!(
        !stderr.contains("rejected"),
        "不应出现回退警告\nstderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// 2. env-var 配置＋对 `stream_options` 返回 400 的 mock：
///    剥掉该字段重试一次后成功；有回退警告；第二次请求不含 `stream_options`；
///    本次 `RunFinished.usage` 为 0。
#[test]
fn endpoint_rejecting_stream_options_falls_back_once() {
    let server = MockServer::start(MockConfig {
        reject_stream_options: true,
        reject_all: false,
        emit_usage: false,
    });
    let home = isolate_home("fallback");

    let out = run_json(&server.api_base(), "hi", &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "回退后应成功（exit 0）\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        event_payload(&stdout, "ModelTextDelta").is_some(),
        "应收到 ModelTextDelta\nstdout:\n{stdout}"
    );
    let finished = event_payload(&stdout, "RunFinished")
        .unwrap_or_else(|| panic!("应收到 RunFinished\nstdout:\n{stdout}"));
    assert_eq!(
        finished["usage"]["input_tokens"], 0,
        "回退后拿不到 usage，应为 0。RunFinished = {finished}"
    );
    assert_eq!(finished["usage"]["output_tokens"], 0);

    // stderr 有明确的回退警告（含 stream_options 与 400）。
    assert!(
        stderr.contains("stream_options") && stderr.contains("400"),
        "stderr 应含回退警告\nstderr:\n{stderr}"
    );

    // 恰好两次请求：首次带 stream_options（400），重试不带。
    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 2, "应恰好重试一次，bodies = {bodies:?}");
    assert!(
        bodies[0].contains("stream_options"),
        "首次请求应带 stream_options，body = {}",
        bodies[0]
    );
    assert!(
        !bodies[1].contains("stream_options"),
        "重试请求不应含 stream_options，body = {}",
        bodies[1]
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// 只记录事件的空 sink。
struct NullSink;

impl ModelEventSink for NullSink {
    fn emit(&mut self, _event: ModelEvent) -> Result<(), ys_core::EventError> {
        Ok(())
    }
}

/// 3. 请求本来就没带 `stream_options`（`supports_stream_usage = false`）
///    遇到 400 → 直接失败、**不重试**（只发 1 次请求）。
#[tokio::test]
async fn no_fallback_when_request_lacks_stream_options() {
    // 回环无代理：用 per-client `.no_proxy()` 绕过代理，**不改进程级 env**。
    // 改写进程 env（set_var/remove_var）会与同 binary 内并行测试的 env 读取
    // （含 `Command` 继承 env 时的 `environ` 读取）产生数据竞争，属必须消除的隐患。
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build no-proxy client");

    // 端点一律 400；本次请求不含 stream_options，故不触发回退。
    let server = MockServer::start(MockConfig {
        reject_stream_options: false,
        reject_all: true,
        emit_usage: false,
    });
    // minimax 兼容层：supports_stream_usage = false。
    let model = OpenAICompatibleModel::with_client(
        OpenAICompatibleConfig {
            api_base: server.api_base(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: None,
            temperature: None,
            compat: ProviderCompat::minimax(),
        },
        client,
    );

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_err(), "400 应直接失败，而不是回退成功");

    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 1, "不应重试，bodies = {bodies:?}");
    assert!(
        !bodies[0].contains("stream_options"),
        "前提不成立：请求竟带了 stream_options，body = {}",
        bodies[0]
    );
}
