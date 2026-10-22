//! `-p` / `--json` 一次性模式的**上下文窗口**解析：厂商目录自报的窗口必须生效。
//!
//! 背景：TUI 模式起屏后在后台拉 `/v1/models`，目录到手后由 `app_loop` 每回合重算
//! 窗口。一次性模式**只跑一个回合** —— 后台任务来不及送达，窗口会整个停在兜底值。
//! 后果：模型报 1M 窗口，实际按兜底值压缩（多轮会话里尤其明显：1M 模型的 89 万
//! token 从未被使用）。
//!
//! 本文件用本地 mock 端点覆盖：
//!
//! 1. 目录里报了 1M ⇒ 一次性模式实际用 1M（而非兜底值）。
//! 2. 目录拉不到（端点 500）⇒ **静默降级**到兜底值，一次性模式**照常成功**
//!    （拉目录是为了把窗口估准，不该拦住一次正常运行）。
//!
//! 观测手段：debug 日志的 `run_start` 事件里带 `limits.context_window`（真实装配
//! 后的窗口值），比事后猜更直接。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 本地 mock HTTP 服务
// ---------------------------------------------------------------------------

/// mock 端点的行为。
#[derive(Clone, Copy)]
enum CatalogBehavior {
    /// `/v1/models` 返回一个自报 1M 窗口的模型。
    ReportsOneMeg,
    /// `/v1/models` 返回 500（拉不到目录）。
    Unreachable,
}

/// 回环地址上的极简 HTTP/1.1 mock（与 `stream_usage_fallback.rs` 同款，不引测试依赖）。
struct MockServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(behavior: CatalogBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_once(stream, behavior);
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

/// 处理一个连接：目录请求按 `behavior` 回，chat 请求回一段最小 SSE。
fn serve_once(mut stream: TcpStream, behavior: CatalogBehavior) -> std::io::Result<()> {
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
    let request_path = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");

    if request_path.ends_with("/models") {
        return match behavior {
            CatalogBehavior::ReportsOneMeg => {
                let payload = br#"{"data":[{"id":"test-model","context_window":1048576}]}"#;
                write_response(&mut stream, "200 OK", "application/json", payload)
            }
            CatalogBehavior::Unreachable => write_response(
                &mut stream,
                "500 Internal Server Error",
                "text/plain",
                b"boom",
            ),
        };
    }

    // chat 请求：最小 SSE（一个文本 delta + 终止 chunk + [DONE]）。
    // **终止 chunk 必给 `finish_reason`**：适配器据此收尾，缺了它就整轮无输出
    // （`StopReason::NoOutput`）。
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    write_response(&mut stream, "200 OK", "text/event-stream", sse.as_bytes())
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
// 子进程辅助
// ---------------------------------------------------------------------------

/// 隔离的 `HOME`（临时目录名含进程内序号 + `process::id()` —— 并行测试不得撞名）。
fn isolate_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "yushan_ctx_window_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// 跑一次 `--json`，返回输出与 debug 日志目录。
fn run_json(api_base: &str, home: &Path) -> (Output, std::path::PathBuf) {
    let log_dir = home.join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"))
        .args(["--json", "hi"])
        .env_remove("YUSHAN_LOG_DIR")
        .env("YUSHAN_DEBUG", "1")
        .env("YUSHAN_LOG_DIR", &log_dir)
        .env("YUSHAN_API_BASE", api_base)
        .env("YUSHAN_API_KEY", "test-key")
        .env("YUSHAN_MODEL", "test-model")
        .env("HOME", home)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .output()
        .expect("跑 --json 子进程");
    (out, log_dir)
}

/// 从 debug 日志里读出 `run_start` 事件记录的 `context_window`。
fn logged_context_window(log_dir: &Path) -> Option<usize> {
    let mut files: Vec<_> = std::fs::read_dir(log_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("debug-") && n.ends_with(".jsonl"))
        })
        .collect();
    files.sort();
    let path = files.last()?.clone();
    for line in std::fs::read_to_string(&path).ok()?.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("ev").and_then(|e| e.as_str()) == Some("run_start") {
            return v
                .get("limits")
                .and_then(|l| l.get("context_window"))
                .and_then(|w| w.as_u64())
                .map(|w| w as usize);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// **目录自报的 1M 窗口必须在一次性模式生效**。
///
/// 变异自检：把 `main.rs` 里「一次性模式同步拉目录」那段删掉 ⇒ 本用例变红
/// （窗口落回兜底值 1_000_000 ≠ 目录报的 1_048_576 —— 两者**刻意选成不同数**，
/// 免得「碰巧相等」把断言变成恒真）。
#[test]
fn one_shot_mode_uses_catalog_context_window() {
    let server = MockServer::start(CatalogBehavior::ReportsOneMeg);
    let home = isolate_home("catalog");

    let (out, log_dir) = run_json(&server.api_base(), &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "一次性模式应成功\nexit={:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let window =
        logged_context_window(&log_dir).expect("debug 日志里应有 run_start 事件（YUSHAN_DEBUG=1）");
    assert_eq!(
        window, 1_048_576,
        "一次性模式必须采用目录自报的窗口（1048576），而非兜底值（1000000）"
    );
}

/// **目录拉不到 ⇒ 静默降级，一次性模式照常成功**。
///
/// 拉目录是为了**把窗口估准**，不是为了选模型（模型名此时已定）—— 拉不到不该
/// 拦住一次正常运行。
#[test]
fn unreachable_catalog_degrades_to_default_window() {
    let server = MockServer::start(CatalogBehavior::Unreachable);
    let home = isolate_home("unreachable");

    let (out, log_dir) = run_json(&server.api_base(), &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "目录拉不到不该让一次性模式失败\nexit={:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("RunFinished"),
        "回合应正常结束\nstdout:\n{stdout}"
    );

    let window =
        logged_context_window(&log_dir).expect("debug 日志里应有 run_start 事件（YUSHAN_DEBUG=1）");
    assert_eq!(
        window, 1_000_000,
        "拉不到目录时落到兜底窗口（RunLimits::default() = 1000000）"
    );
}
