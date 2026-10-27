//! 模型调用**超时 + 有界重试**的端到端验收（ADR-0028）。
//!
//! 用**本地回环 TCP mock 服务器**驱动 [`OpenAICompatibleModel::complete`]：
//! 按预设序列逐个返回响应、记录请求数，覆盖空闲超时、`x-should-retry` 头、
//! 额度名单否决、状态码分类、`Retry-After` 硬上限、已吐 delta 不重试、退避日志。
//!
//! 回环地址需绕过代理：用 `reqwest::Client` 的 `.no_proxy()` 直连，**不改进程级 env**
//! （同 binary 内并行测试改 env 会数据竞争）；端口交给内核分配（`bind(:0)`）避免撞名。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ys_core::EventError;
use ys_model::{Model, ModelError, ModelEvent, ModelEventSink, ModelRequest};
use ys_model_openai_compat::{
    ModelAttempt, ModelAttemptOutcome, ModelAttemptReason, ModelAttemptRetrySuppressed,
    OpenAICompatibleConfig, OpenAICompatibleModel, RetryConfig, TimeoutConfig,
    compat::ProviderCompat,
};
use ys_protocol::{Boundary, QueueBoundarySource};

// ---------------------------------------------------------------------------
// 本地 mock HTTP 服务
// ---------------------------------------------------------------------------

/// 一条预设响应。
#[derive(Clone)]
enum Reply {
    /// 标准一次性 HTTP 响应（带 `Content-Length` + `Connection: close`）。
    Http {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
    /// 200 + 完整 SSE（`Content-Length` 与实际字节相等，干净收尾）。
    Sse(Vec<u8>),
    /// 200 + 一段 SSE 后**保持连接沉默**（不关、不再发字节）——用于流空闲超时。
    SseThenSilence(Vec<u8>),
    /// 200 + 一段 SSE 后**立即关闭**，且 `Content-Length` 声明得比实发字节大 ——
    /// 客户端读到 delta 后连接关闭，hyper 判定消息不完整 ⇒ 传输层错误。
    /// 用于「已吐 delta 后断连」。
    SseThenTruncate(Vec<u8>),
    /// **先睡一段**再回 200 + SSE —— 用于「请求已发出、服务端尚未应答」的窗口。
    ///
    /// 头**不**提前发（否则客户端会认为响应头已到，进入流阶段）。
    DelayThenSse { delay: Duration, sse: Vec<u8> },
}

/// 极简 HTTP/1.1 mock：第 N 个连接返回第 N 条预设响应（超出则复用最后一条）。
struct MockServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(replies: Vec<Reply>) -> Self {
        assert!(!replies.is_empty(), "至少一条预设响应");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests_thread = requests.clone();
        let stop_thread = stop.clone();
        let replies = Arc::new(replies);

        let handle = std::thread::spawn(move || {
            let mut index = 0usize;
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let reply = replies
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| replies.last().unwrap().clone());
                        index += 1;
                        let _ = handle_connection(stream, &reply, &requests_thread, &stop_thread);
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

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
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

fn handle_connection(
    mut stream: TcpStream,
    reply: &Reply,
    requests: &Arc<Mutex<Vec<String>>>,
    stop: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    // 监听 socket 非阻塞（accept 轮询 stop 需要）；macOS/BSD 上 accept 返回的连接
    // 会继承 O_NONBLOCK，导致后续 `set_read_timeout` 被忽略。显式复位为阻塞。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // 读头部直到 \r\n\r\n，再按 Content-Length 读完请求体。
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
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
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    requests
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&buf).to_string());

    match reply {
        Reply::Http {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (k, v) in headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes())?;
            stream.write_all(body)?;
            stream.flush()
        }
        Reply::Sse(sse) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                sse.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(sse)?;
            stream.flush()
        }
        Reply::SseThenSilence(sse) => {
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            stream.write_all(head.as_bytes())?;
            stream.write_all(sse)?;
            stream.flush()?;
            // 保持沉默，直到测试结束（stop 置位）或兜底上限到点。
            let deadline = Instant::now() + Duration::from_secs(5);
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        }
        Reply::SseThenTruncate(sse) => {
            let declared = sse.len() + 4096;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(sse)?;
            stream.flush()?;
            // 留出时间让客户端先读到 delta，再关闭（触发「消息不完整」）。
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        }
        Reply::DelayThenSse { delay, sse } => {
            // 头也延后发：客户端在整个 `delay` 内都停在「等待响应头」，
            // 即发送阶段的 `select!` 里。
            std::thread::sleep(*delay);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                sse.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(sse)?;
            stream.flush()
        }
    }
}

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

/// 只记录事件的空 sink。
struct NullSink;

impl ModelEventSink for NullSink {
    fn emit(&mut self, _event: ModelEvent) -> Result<(), EventError> {
        Ok(())
    }
}

/// 测试用小值重试：**不要用默认值**（默认最大等待 ~60s 会让测试很慢）。
fn fast_retry(max_attempts: u32, max_delay_ms: u64) -> RetryConfig {
    RetryConfig {
        max_attempts,
        base_delay_ms: 1,
        max_delay_ms,
    }
}

/// 关闭 `stream_options` 的配置：避免 400 触发既有的「剥参数回退」，
/// 让请求计数可预测（回退会在同一次尝试内多发一次请求）。
fn base_config(api_base: String) -> OpenAICompatibleConfig {
    OpenAICompatibleConfig {
        api_base,
        api_key: "test-key".into(),
        model: "test-model".into(),
        max_tokens: None,
        temperature: None,
        reasoning_effort: None,
        compat: ProviderCompat {
            supports_stream_usage: false,
            ..ProviderCompat::standard()
        },
    }
}

/// 回环直连、无代理的 client。
fn no_proxy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build no-proxy client")
}

/// 日志收集器：`Arc<Mutex<Vec<String>>>` 既是 logger 的载体，也是断言来源。
type Logs = Arc<Mutex<Vec<String>>>;

fn logs() -> Logs {
    Arc::new(Mutex::new(Vec::new()))
}

fn log_listener(logs: &Logs) -> Arc<dyn Fn(&str) + Send + Sync> {
    let logs = logs.clone();
    Arc::new(move |message: &str| logs.lock().unwrap().push(message.to_string()))
}

type Traces = Arc<Mutex<Vec<ModelAttempt>>>;

fn traces() -> Traces {
    Arc::new(Mutex::new(Vec::new()))
}

fn trace_listener(traces: &Traces) -> Arc<dyn Fn(&ModelAttempt) + Send + Sync> {
    let traces = traces.clone();
    Arc::new(move |attempt| traces.lock().unwrap().push(attempt.clone()))
}

fn full_sse() -> Vec<u8> {
    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n"
        .to_vec()
}

fn one_delta_sse() -> Vec<u8> {
    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n".to_vec()
}

/// 只吐一条**思考**增量（内联 `<think>` 标签，经 `inline_thinking` 过滤器
/// 转成 `ThinkingDelta`），不含任何正文 delta。
fn thinking_delta_sse() -> Vec<u8> {
    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>secret</think>\"}}]}\n\n"
        .to_vec()
}

fn json_reply(status: &'static str, headers: Vec<(&'static str, String)>, body: &str) -> Reply {
    Reply::Http {
        status,
        headers,
        body: body.as_bytes().to_vec(),
    }
}

/// 构造一个注入回环 client、小重试、指定空闲超时的 model。
fn model_for(
    server: &MockServer,
    retry: RetryConfig,
    idle_secs: u64,
    logs: Option<&Logs>,
) -> OpenAICompatibleModel {
    let model =
        OpenAICompatibleModel::with_client(base_config(server.api_base()), no_proxy_client())
            .with_retry(retry)
            .with_timeouts(TimeoutConfig {
                connect_secs: 5,
                idle_secs,
            });
    match logs {
        Some(logs) => model.with_logger(log_listener(logs)),
        None => model,
    }
}

/// 断言在**线路上**（而不只是 builder 内部）能看到额外头。
///
/// 网关接入要求走这条路（OpenCode Go 的 `x-opencode-session`）：头必须出现在
/// 真实请求里，光有 `with_extra_header` 的返回值不算数。
#[tokio::test]
async fn extra_headers_are_sent_on_the_wire() {
    let server = MockServer::start(vec![Reply::Sse(full_sse())]);
    let model = model_for(&server, fast_retry(0, 10), 5, None)
        .with_extra_header("x-opencode-session", "ys-test-session");

    model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect("call ok");

    let raw = server.requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        raw.contains("x-opencode-session: ys-test-session"),
        "请求里应带上会话头，实际：{raw}"
    );
}

/// 非法头名/头值**只丢自己**：不 panic、不影响后续头与请求本身。
#[tokio::test]
async fn invalid_extra_header_is_dropped_without_failing_the_call() {
    let server = MockServer::start(vec![Reply::Sse(full_sse())]);
    let model = model_for(&server, fast_retry(0, 10), 5, None)
        .with_extra_header("bad header name", "x")
        .with_extra_header("x-opencode-session", "ys-test-session");

    model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect("call ok");

    let raw = server.requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(raw.contains("x-opencode-session: ys-test-session"));
    assert!(!raw.contains("bad header name"));
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 1. **空闲超时**：server 200 + 一条 delta 后沉默 ⇒ `Err`，消息含 `idle timeout`。
#[tokio::test]
async fn idle_timeout_surfaces_error() {
    let server = MockServer::start(vec![Reply::SseThenSilence(one_delta_sse())]);
    let model = model_for(&server, fast_retry(2, 10), 1, None);

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    let err = result.expect_err("空闲超时必须是 Err");
    assert!(
        err.to_string().contains("idle timeout"),
        "错误消息应含 idle timeout：{err}"
    );
}

/// 2. **429 ⇒ 退避后成功**：第 1 个请求 429，第 2 个 200 + 完整 SSE ⇒ `Ok`；
///    server 收到 2 个请求；日志里有 ≥1 条 `retry attempt`。
#[tokio::test]
async fn retry_after_429_then_success() {
    let logs = logs();
    let server = MockServer::start(vec![
        json_reply(
            "429 Too Many Requests",
            vec![],
            "{\"error\":\"rate limited\"}",
        ),
        Reply::Sse(full_sse()),
    ]);
    let model = model_for(&server, fast_retry(2, 10), 30, Some(&logs));

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_ok(), "429 后一次重试应成功：{result:?}");
    assert_eq!(server.request_count(), 2, "应恰好重试一次");
    let lines = logs.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("retry attempt")),
        "日志应有 retry attempt：{lines:?}"
    );
}

/// 3. **`x-should-retry: false` 否决状态码**：返回 500 但带头拒绝 ⇒ `Err`，只 1 个请求。
#[tokio::test]
async fn x_should_retry_false_vetoes_status() {
    let server = MockServer::start(vec![json_reply(
        "500 Internal Server Error",
        vec![("x-should-retry", "false".into())],
        "{\"error\":\"boom\"}",
    )]);
    let model = model_for(&server, fast_retry(2, 10), 30, None);

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_err(), "x-should-retry:false 应否决 500 的重试");
    assert_eq!(server.request_count(), 1, "不应重试");
}

/// 4. **`x-should-retry: true` 覆盖状态码**：返回 400 但带头要求重试 ⇒ 重试（2 个请求）。
#[tokio::test]
async fn x_should_retry_true_overrides_status() {
    let server = MockServer::start(vec![
        json_reply(
            "400 Bad Request",
            vec![("x-should-retry", "true".into())],
            "{\"error\":\"nope\"}",
        ),
        Reply::Sse(full_sse()),
    ]);
    let model = model_for(&server, fast_retry(2, 10), 30, None);

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(
        result.is_ok(),
        "x-should-retry:true 后的第二次应成功：{result:?}"
    );
    assert_eq!(server.request_count(), 2, "应重试一次");
}

/// 5. **额度名单否决**：返回 429 + body 含 `insufficient_quota` ⇒ `Err`，只 1 个请求（零退避）。
#[tokio::test]
async fn quota_exhaustion_vetoes_429() {
    let logs = logs();
    let server = MockServer::start(vec![json_reply(
        "429 Too Many Requests",
        vec![],
        "{\"error\":{\"code\":\"insufficient_quota\",\"message\":\"You exceeded your current quota\"}}",
    )]);
    let model = model_for(&server, fast_retry(2, 10_000), 30, Some(&logs));

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    let err = result.expect_err("额度耗尽应立即失败");
    assert_eq!(server.request_count(), 1, "额度耗尽不得重试");
    assert!(
        err.to_string().contains("insufficient_quota"),
        "错误消息应保留额度原因：{err}"
    );
}

/// 6. **`Retry-After` 超硬上限 ⇒ 放弃**：`Retry-After: 999`、`max_delay_ms = 1000`
///    ⇒ `Err`，只 1 个请求，消息含服务端要求的秒数，且**立即返回**（不睡满）。
///
/// 外层 `timeout` 是给变异测试的针脚：若把硬上限改成「与上界取 max」，
/// `complete` 会睡满 999s，外层超时 ⇒ 断言失败（红）。
#[tokio::test]
async fn retry_after_exceeding_hard_cap_gives_up_immediately() {
    let server = MockServer::start(vec![json_reply(
        "429 Too Many Requests",
        vec![("retry-after", "999".into())],
        "{\"error\":\"slow down\"}",
    )]);
    let model = model_for(&server, fast_retry(2, 1_000), 30, None);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        model.complete(ModelRequest::default(), &mut NullSink),
    )
    .await
    .expect("硬上限应让 complete 立即返回，而不是睡满 999s");

    let err = result.expect_err("服务端要求超过硬上限 ⇒ 放弃重试");
    assert_eq!(server.request_count(), 1, "放弃时不应再发请求");
    assert!(
        err.to_string().contains("999"),
        "错误消息应含服务端要求的秒数 999：{err}"
    );
}

/// 7. **已吐 delta 不重试**：200 + 一条 delta 后断连 ⇒ `Err`，只 1 个请求。
///
/// 变异靶：去掉 `DeltaWatchingSink` 的 `emitted` 判断后本用例必须变红
/// （流中途传输错误可重试 ⇒ 会多发请求）。
#[tokio::test]
async fn no_retry_after_delta_emitted() {
    let server = MockServer::start(vec![Reply::SseThenTruncate(one_delta_sse())]);
    let model = model_for(&server, fast_retry(2, 10), 30, None);

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_err(), "断流的半截流必须是 Err");
    assert_eq!(server.request_count(), 1, "吐过 delta 后不得重试");
}

/// 8. **确定性 4xx 不重试**：各状态码一条，均 `Err` 且只 1 个请求。
///
/// （配置关闭了 `stream_options`，否则 400 会走既有的「剥参数回退」改变请求数。）
#[tokio::test]
async fn deterministic_4xx_never_retried() {
    let cases: [(&'static str, &'static str); 6] = [
        ("400 Bad Request", "{\"error\":\"bad\"}"),
        ("401 Unauthorized", "{\"error\":\"unauthorized\"}"),
        ("403 Forbidden", "{\"error\":\"forbidden\"}"),
        ("404 Not Found", "{\"error\":\"missing\"}"),
        ("413 Payload Too Large", "{\"error\":\"too big\"}"),
        ("422 Unprocessable Entity", "{\"error\":\"unprocessable\"}"),
    ];
    for (status, body) in cases {
        let server = MockServer::start(vec![json_reply(status, vec![], body)]);
        let model = model_for(&server, fast_retry(2, 10), 30, None);

        let result = model.complete(ModelRequest::default(), &mut NullSink).await;
        assert!(result.is_err(), "{status} 应是确定性失败");
        assert_eq!(server.request_count(), 1, "{status} 不应重试");
    }
}

/// 9. **重试日志可见**：连续 500 直到次数耗尽 —— 日志含 `retry attempt`（退避前）
///    与 `giving up`（最终放弃）两行；错误消息带尝试次数。
#[tokio::test]
async fn retry_logs_are_visible() {
    let logs = logs();
    let server = MockServer::start(vec![json_reply(
        "500 Internal Server Error",
        vec![],
        "{\"error\":\"boom\"}",
    )]);
    let model = model_for(&server, fast_retry(2, 10), 30, Some(&logs));

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_err(), "持续 500 最终应失败");
    assert_eq!(server.request_count(), 3, "2 次额外重试 ⇒ 共 3 次尝试");

    let lines = logs.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("retry attempt")),
        "应有退避前日志：{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("giving up")),
        "应有最终放弃日志：{lines:?}"
    );
}

/// 11. 结构化尝试读数覆盖 429 → 成功：第一次可重试且记录了退避，第二次成功；
///     成功尝试的 `ttfb_ms` 来自响应体首个非空 chunk，而不是连接建立时间。
#[tokio::test]
async fn model_attempt_trace_retry_then_success() {
    let traces = traces();
    let server = MockServer::start(vec![
        json_reply(
            "429 Too Many Requests",
            vec![("retry-after", "0".into())],
            "{\"error\":\"rate limited\"}",
        ),
        Reply::Sse(full_sse()),
    ]);
    let model =
        model_for(&server, fast_retry(2, 100), 30, None).with_trace(trace_listener(&traces));

    model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect("429 后应重试成功");

    let records = traces.lock().unwrap().clone();
    assert_eq!(records.len(), 2, "每个 HTTP 尝试恰好一条记录：{records:?}");
    let first = &records[0];
    assert_eq!(first.attempt, 1);
    assert_eq!(first.max_attempts, 3);
    assert_eq!(first.outcome, ModelAttemptOutcome::Retryable);
    assert_eq!(first.reason_class, Some(ModelAttemptReason::RateLimited));
    assert_eq!(first.http_status, Some(429));
    assert_eq!(first.ttfb_ms, None);
    assert!(first.response_headers_ms.is_some());
    assert!(first.retried);
    assert_eq!(first.retry_suppressed, None);
    assert!(!first.stream_started);
    assert!(!first.fallback_retry);
    assert!(
        records[0].backoff_ms.is_some(),
        "实际排程的退避必须落盘：{records:?}"
    );
    assert_eq!(records[1].attempt, 2);
    assert_eq!(records[1].outcome, ModelAttemptOutcome::Success);
    assert_eq!(records[1].http_status, Some(200));
    assert!(
        records[1].ttfb_ms.is_some(),
        "成功尝试必须记录首个响应体字节：{records:?}"
    );
    assert!(!records[1].retried);
}

/// 12. 次数耗尽只记“可重试但被抑制”，不得伪装成成功或普通 fatal。
#[tokio::test]
async fn model_attempt_trace_marks_exhausted_retry() {
    let traces = traces();
    let server = MockServer::start(vec![json_reply(
        "500 Internal Server Error",
        vec![],
        "{\"error\":\"boom\"}",
    )]);
    let model = model_for(&server, fast_retry(1, 10), 30, None).with_trace(trace_listener(&traces));

    model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect_err("持续 500 应失败");

    let records = traces.lock().unwrap().clone();
    assert_eq!(records.len(), 2, "1 次额外重试 ⇒ 2 个 HTTP 尝试");
    assert_eq!(records[0].outcome, ModelAttemptOutcome::Retryable);
    assert_eq!(
        records[0].reason_class,
        Some(ModelAttemptReason::ServerError)
    );
    assert!(records[0].retried);
    assert_eq!(records[1].attempt, 2);
    assert_eq!(records[1].outcome, ModelAttemptOutcome::Retryable);
    assert_eq!(
        records[1].retry_suppressed,
        Some(ModelAttemptRetrySuppressed::Exhausted)
    );
    assert!(!records[1].retried);
}

/// 13. 端点拒绝 `stream_options` 后的单次内部回退仍属同一 attempt，但必须由
///     `fallback_retry` 标明；否则 2 次 HTTP 请求会看起来像 1 次。
#[tokio::test]
async fn model_attempt_trace_marks_stream_options_fallback() {
    let traces = traces();
    let logs = logs();
    let server = MockServer::start(vec![
        json_reply(
            "400 Bad Request",
            vec![],
            "{\"error\":\"stream_options unsupported\"}",
        ),
        Reply::Sse(full_sse()),
    ]);
    let config = OpenAICompatibleConfig {
        api_base: server.api_base(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        max_tokens: None,
        temperature: None,
        reasoning_effort: None,
        compat: ProviderCompat::standard(),
    };
    let model = OpenAICompatibleModel::with_client(config, no_proxy_client())
        .with_retry(fast_retry(0, 10))
        .with_timeouts(TimeoutConfig {
            connect_secs: 5,
            idle_secs: 30,
        })
        .with_trace(trace_listener(&traces));
    let model = model.with_logger(log_listener(&logs));

    model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect("剥掉 stream_options 后应成功");

    let records = traces.lock().unwrap().clone();
    assert_eq!(records.len(), 1, "内部回退不增加 attempt 序号");
    assert!(records[0].fallback_retry);
    assert_eq!(records[0].attempt, 1);
    assert_eq!(records[0].http_status, Some(200));
    assert_eq!(records[0].outcome, ModelAttemptOutcome::Success);
    assert_eq!(server.request_count(), 2);
    let logs = logs.lock().unwrap().clone();
    assert!(
        logs.iter()
            .any(|line| line.contains("rejected `stream_options.include_usage`")),
        "fallback warning 必须进入 logger，不能写 stderr：{logs:?}"
    );
}

/// 普通 400 不是兼容性信号：tool call 配对错误必须原样失败，不能剥参数重发。
///
/// 变异：把 fallback 条件退回“任意 400 + stream_options 存在” ⇒ request_count 变 2。
#[tokio::test]
async fn unrelated_bad_request_does_not_trigger_stream_options_fallback() {
    let server = MockServer::start(vec![json_reply(
        "400 Bad Request",
        vec![],
        "{\"error\":{\"message\":\"An assistant message with tool_calls must be followed by tool messages\"}}",
    )]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let error = model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect_err("tool call 配对错误必须失败");

    assert_eq!(server.request_count(), 1, "普通 400 不得内部重发");
    assert!(
        error.to_string().contains("tool_calls must be followed"),
        "应保留 provider 原始错误正文：{error}"
    );
}

/// 非法工具参数 JSON 是运行期诊断：必须进入 logger，并按设计降级为**空对象**。
///
/// 降级目标不是 `Value::Null`：`arguments` 会随会话历史回传成
/// `function.arguments` 字符串，`Value::Null` 序列化为字面量 `"null"`，实测
/// minimax 对此返回 `400 (2013) invalid params`，且坏块已落盘 ⇒ 之后每个回合
/// 都在 round=1 失败（只能 `/new` 自救）。空对象在工具层语义相同且线上合法。
///
/// 变异：把降级改回 `Value::Null` ⇒ `is_object` 与 `"{}"` 断言变红；
/// 改回 `eprintln!` ⇒ logger 为空且 TUI 会被污染。
#[tokio::test]
async fn invalid_tool_arguments_warning_uses_logger() {
    let logs = logs();
    let server = MockServer::start(vec![Reply::Sse(
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{bad\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n"
            .to_vec(),
    )]);
    let model = model_for(&server, fast_retry(0, 10), 30, Some(&logs));

    let response = model
        .complete(ModelRequest::default(), &mut NullSink)
        .await
        .expect("非法参数应降级而不是失败");

    let ys_core::ContentBlock::ToolUse { arguments, .. } = &response.message.content[0] else {
        panic!("应产出 ToolUse");
    };
    assert!(
        arguments.is_object(),
        "降级必须产出 JSON 对象（线上 `\"{{}}\"`），不得是 null —— {arguments:?}"
    );
    assert_eq!(arguments, &serde_json::json!({}), "降级目标为空对象");
    let logs = logs.lock().unwrap().clone();
    assert!(
        logs.iter()
            .any(|line| line.contains("tool_call arguments for `read` are not valid JSON")),
        "工具参数修复 warning 必须进入 logger：{logs:?}"
    );
}

/// 适配器生产源不得绕过 logger 直写终端。
///
/// `concat!` 拼针脚，避免测试源码自身命中扫描。
#[test]
fn production_source_has_no_direct_terminal_prints() {
    let source = include_str!("../src/lib.rs");
    for needle in [concat!("eprint", "ln!"), concat!("print", "ln!")] {
        assert!(
            !source.contains(needle),
            "适配器不得直接打印 `{needle}`：TUI 起屏后会污染输入区"
        );
    }
}

/// 10. **默认值被钉住**：生产接线落地前，这两组默认值**就是实际生效值**，
///     且是 48h 自愈面的参数 —— 被人静默改大/改小必须被测试发现。
///     （真实环境校准记为 debt，届时连同此断言一并更新。）
#[test]
fn retry_config_default_is_pinned() {
    assert_eq!(
        RetryConfig::default(),
        RetryConfig {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    );
}

/// 11. **默认值被钉住**（同 10，超时旋钮）。
#[test]
fn timeout_config_default_is_pinned() {
    assert_eq!(
        TimeoutConfig::default(),
        TimeoutConfig {
            connect_secs: 15,
            idle_secs: 120,
        }
    );
}

/// 12. **`max_attempts = 0` 即不重试**：即便 429 本属可重试，也只发 1 个请求并立即 `Err`。
#[tokio::test]
async fn max_attempts_zero_disables_retry() {
    let server = MockServer::start(vec![json_reply(
        "429 Too Many Requests",
        vec![],
        "{\"error\":\"rate limited\"}",
    )]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_err(), "max_attempts=0 应直接失败：{result:?}");
    assert_eq!(server.request_count(), 1, "max_attempts=0 不得重试");
}

/// 12b. **极大 `max_attempts`（`u32::MAX`）不 panic / 不溢出**：上限极大时
///      循环仍应在成功时终止（`0..=u32::MAX` 不至于引发范围溢出）。
///      （退避在 `attempt` 极大时的算术由 lib.rs 的 `backoff_delay_saturates_without_overflow` 覆盖。）
#[tokio::test]
async fn huge_max_attempts_terminates_on_success() {
    let server = MockServer::start(vec![
        json_reply("429 Too Many Requests", vec![], "{\"error\":\"slow\"}"),
        Reply::Sse(full_sse()),
    ]);
    let model = model_for(
        &server,
        RetryConfig {
            max_attempts: u32::MAX,
            base_delay_ms: 1,
            max_delay_ms: 10,
        },
        30,
        None,
    );

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_ok(), "重试一次即应成功：{result:?}");
    assert_eq!(server.request_count(), 2, "应恰好重试一次后终止");
}

/// 13. **`ThinkingDelta` 同样触发 `emitted` 锁**：只吐思考增量后断流 ⇒ 不重试。
///
/// 变异靶：`DeltaWatchingSink::emit` 只对 `TextDelta` 置位（漏掉 `ThinkingDelta`）
/// 时本用例必须变红 —— 断流的传输错误可重试 ⇒ 会多发请求（max_attempts=2 ⇒ 3 次）。
#[tokio::test]
async fn no_retry_after_thinking_delta_emitted() {
    let server = MockServer::start(vec![Reply::SseThenTruncate(thinking_delta_sse())]);
    let model = model_for(&server, fast_retry(2, 10), 30, None);

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_err(), "断流的半截流必须是 Err");
    assert_eq!(server.request_count(), 1, "吐过思考增量后不得重试");
}

/// 14. **`with_client` 注入的 client 归调用方，`with_timeouts` 不得重建它**。
///
/// 判别手法（对进程级代理 env 免疫，故比「死代理 + 对照」更确定）：
/// 给注入的 client 挂一个指向「哨兵服务 B」的**显式代理**，`api_base` 指向另一台
/// 服务 A。
/// - client 未被重建 ⇒ 请求经代理 B；B 收到、A 收不到；
/// - client 被重建（`owns_client` 守卫失效）⇒ 请求直连 A（或经 env 代理），B 收不到。
/// 断言「B 恰好收到 1 个请求」即可区分两种实现，且与是否存在 env 代理无关。
#[tokio::test]
async fn with_client_injected_client_survives_with_timeouts() {
    let origin = MockServer::start(vec![Reply::Sse(full_sse())]);
    let sentinel = MockServer::start(vec![Reply::Sse(full_sse())]);

    let injected = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{}", sentinel.port)).unwrap())
        .build()
        .expect("build proxied client");

    let model = OpenAICompatibleModel::with_client(base_config(origin.api_base()), injected)
        .with_retry(fast_retry(2, 10))
        .with_timeouts(TimeoutConfig {
            connect_secs: 5,
            idle_secs: 30,
        });

    let result = model.complete(ModelRequest::default(), &mut NullSink).await;
    assert!(result.is_ok(), "经哨兵代理应成功：{result:?}");
    assert_eq!(
        sentinel.request_count(),
        1,
        "注入 client 的代理必须仍被使用（未被 with_timeouts 重建）"
    );
    assert_eq!(
        origin.request_count(),
        0,
        "请求不应绕过注入的代理直连 origin"
    );
}

// ---------------------------------------------------------------------------
// 中止（cancel）—— ADR-0046
// ---------------------------------------------------------------------------

/// 收集所有 `TextDelta` / `ThinkingDelta` 的 sink（中止用例需要看已吐内容）。
struct RecordingSink {
    texts: Vec<String>,
    thinking: Vec<String>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            texts: Vec::new(),
            thinking: Vec::new(),
        }
    }

    /// 已吐出的正文（拼接）。
    fn text(&self) -> String {
        self.texts.concat()
    }
}

impl ModelEventSink for RecordingSink {
    fn emit(&mut self, event: ModelEvent) -> Result<(), EventError> {
        match event {
            ModelEvent::TextDelta { text } => self.texts.push(text),
            ModelEvent::ThinkingDelta { text } => self.thinking.push(text),
            _ => {}
        }
        Ok(())
    }
}

/// 可控的边界源：**测试**能随时置位 abort。
///
/// 用 `QueueBoundarySource`（生产实现）而不是另写一个：这样测试覆盖的
/// 就是生产路径的语义（`push(Abort)` 即置位）。
type SharedBoundary = std::sync::Arc<QueueBoundarySource>;

fn boundary() -> SharedBoundary {
    std::sync::Arc::new(QueueBoundarySource::new())
}

/// 带 boundary 的请求。
fn request_with(boundary: &SharedBoundary) -> ModelRequest {
    ModelRequest {
        boundary: Some(boundary.clone()),
        ..ModelRequest::default()
    }
}

/// N1：**已 abort 的 boundary** ⇒ `Model::complete` 返回 `ModelError::Cancelled`。
///
/// 变异靶：去掉发送阶段的 `select!`（退化为直接 `await`）⇒ 本用例变红
/// （`complete` 会拿到 200 + SSE 并返回 `Ok`）。
#[tokio::test]
async fn already_aborted_boundary_returns_cancelled() {
    let server = MockServer::start(vec![Reply::Sse(full_sse())]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let boundary = boundary();
    // **先** abort，再发请求 ⇒ 请求发出前就已中止。
    boundary.push(Boundary::Abort);

    let result = model.complete(request_with(&boundary), &mut NullSink).await;

    let error = result.expect_err("已 abort 的 boundary 必须返回 Err");
    assert!(
        matches!(error, ModelError::Cancelled { .. }),
        "应是 ModelError::Cancelled，实际：{error:?}"
    );
}

/// N1b：**事先已 abort** ⇒ 请求**根本不上线**（`request_count() == 0`）。
///
/// 依据：发送阶段的 `select!` 里中止分支在前（`biased`）且**立即就绪**
/// （`wait_abort` 先探针后睡）⇒ 边界已 abort 时，请求还没发出就被中止。
///
/// 变异靶：去掉发送阶段的 `select!`（退化为裸 `await`）⇒ 本用例变红
/// （请求照发，`request_count()` 变成 1）。
///
/// 注意：断言的是「**一个都不发**」而非「至多一个」——
/// 「至多一个」无法区分「中止前没发」与「中止后重试了一次」。
#[tokio::test]
async fn already_aborted_boundary_sends_no_request() {
    let server = MockServer::start(vec![Reply::Sse(full_sse())]);
    let model = model_for(&server, fast_retry(3, 10), 30, None);

    let boundary = boundary();
    boundary.push(Boundary::Abort);

    let result = model.complete(request_with(&boundary), &mut NullSink).await;

    assert!(result.is_err(), "中止必须是 Err");
    assert_eq!(
        server.request_count(),
        0,
        "事先已 abort ⇒ 请求不得上线（更不得重试）"
    );
}

/// N2：**先吐 delta 再取消** ⇒ `partial` 含已吐文本。
///
/// 手法：mock server 先发一段带正文的 SSE，**然后保持连接沉默**（不关、不再发），
/// 测试在收到第一个 delta 后才 push `Abort`。
///
/// 变异靶：把 `into_partial` 改成恒返回 `None` ⇒ 本用例变红。
#[tokio::test]
async fn cancelled_after_delta_carries_partial_text() {
    let server = MockServer::start(vec![Reply::SseThenSilence(one_delta_sse())]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let boundary = boundary();
    let mut sink = RecordingSink::new();

    // 并发：一边跑模型（让它吐 delta），一边在 300ms 后取消。
    // `join!` 而非 `spawn` ⇒ sink 留在本任务里，事后可断言「确实吐过 delta」。
    let cancel = {
        let boundary = boundary.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            boundary.push(Boundary::Abort);
        }
    };
    let (result, ()) = tokio::join!(model.complete(request_with(&boundary), &mut sink), cancel);

    assert!(
        sink.text().contains("partial"),
        "前置条件：模型应已吐过 delta，实际：{:?}",
        sink.text()
    );

    let error = result.expect_err("取消后必须是 Err");
    let ModelError::Cancelled { partial } = error else {
        panic!("应是 ModelError::Cancelled，实际：{error:?}");
    };
    let partial = partial.expect("已吐 delta ⇒ partial 必须存在");
    let text = partial
        .content
        .iter()
        .filter_map(|block| match block {
            ys_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("partial"),
        "partial 应含已吐文本，实际：{text:?}"
    );
}

/// N3：**先吐 tool call 的部分参数再取消** ⇒ `partial` 的 content blocks
/// **全部**是 `Text`，无 `ToolUse`。
///
/// 理由：中止时未完成的工具调用参数可能残缺，执行它等于拿半个 payload 去改状态。
///
/// 手法：先吐一段**正文**，再吐一个**参数只写一半**的 tool call ——
/// 这样 `partial` 非空（正文在），断言「只有 Text、没有 ToolUse」才有意义。
///
/// 变异靶：把 `into_partial` 改成也吐 `ToolUse` ⇒ 本用例变红。
#[tokio::test]
async fn cancelled_with_partial_tool_call_carries_no_tool_use() {
    // 正文 + 参数只写一半的 tool call，随后沉默。
    let sse = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"before-tool\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n"
        .to_vec();
    let server = MockServer::start(vec![Reply::SseThenSilence(sse)]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let boundary = boundary();
    let mut sink = RecordingSink::new();

    // 并发：跑模型（吐正文 + 半个 tool call）与取消。
    let cancel = {
        let boundary = boundary.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            boundary.push(Boundary::Abort);
        }
    };
    let (result, ()) = tokio::join!(model.complete(request_with(&boundary), &mut sink), cancel);

    assert!(
        sink.text().contains("before-tool"),
        "前置条件：模型应已吐过正文，实际：{:?}",
        sink.text()
    );

    let error = result.expect_err("取消后必须是 Err");
    let ModelError::Cancelled { partial } = error else {
        panic!("应是 ModelError::Cancelled，实际：{error:?}");
    };
    let partial = partial.expect("已吐正文 ⇒ partial 必须存在");
    for block in &partial.content {
        assert!(
            matches!(block, ys_core::ContentBlock::Text { .. }),
            "partial 不得含 ToolUse 等非文本块：{block:?}"
        );
    }
    // 反向断言：正文确实在里面（否则「只有 Text」是空断言）。
    let text = partial
        .content
        .iter()
        .filter_map(|block| match block {
            ys_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("before-tool"),
        "partial 应含已吐正文，实际：{text:?}"
    );
}

/// N3b：**未吐任何内容就取消** ⇒ `partial` 为 `None`（不是空文本块）。
#[tokio::test]
async fn cancelled_before_any_output_has_no_partial() {
    let server = MockServer::start(vec![Reply::Sse(full_sse())]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let boundary = boundary();
    boundary.push(Boundary::Abort);

    let result = model.complete(request_with(&boundary), &mut NullSink).await;

    let error = result.expect_err("已 abort 的 boundary 必须返回 Err");
    let ModelError::Cancelled { partial } = error else {
        panic!("应是 ModelError::Cancelled，实际：{error:?}");
    };
    assert!(partial.is_none(), "尚无可见输出 ⇒ partial 应为 None");
}

/// N5：**请求已发出、服务端尚未应答**时取消 ⇒ 立即 `Cancelled`（不等应答）。
///
/// 场景：mock 收到请求后先睡 3s 才回 200 + SSE。测试在 300ms 时取消 ⇒
/// `complete` 应**立即**以 `Cancelled` 收场，而不是等 3s 后拿到响应。
///
/// 这是发送阶段 `select!` 的**唯一可观测**用例：只有在「请求已上线、
/// 响应头未到」这个窗口里取消，才能证明发送阶段真的可中止。
///
/// 变异靶：把发送阶段的 `select!` 退回裸 `await` ⇒ 本用例变红
/// （`complete` 会等满 5s，外层 2s 超时 ⇒ 断言失败）。
#[tokio::test]
async fn cancellation_while_awaiting_response_headers_returns_promptly() {
    let server = MockServer::start(vec![Reply::DelayThenSse {
        delay: Duration::from_secs(3),
        sse: full_sse(),
    }]);
    let model = model_for(&server, fast_retry(0, 10), 30, None);

    let boundary = boundary();
    let cancel = {
        let boundary = boundary.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            boundary.push(Boundary::Abort);
        }
    };
    let request = request_with(&boundary);
    let mut sink = NullSink;
    // 2s 上限：mock 要 3s 才应答 ⇒ 若发送阶段不可中止，这里会超时。
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async move {
        tokio::join!(model.complete(request, &mut sink), cancel)
    })
    .await
    .expect("发送阶段必须可中止：不应等服务端的 3s 应答");

    let error = result.expect_err("取消后必须是 Err");
    assert!(
        matches!(error, ModelError::Cancelled { .. }),
        "应是 ModelError::Cancelled，实际：{error:?}"
    );
    assert_eq!(
        server.request_count(),
        1,
        "请求已发出；取消不应再发第二个请求"
    );
}

/// N4：**退避期间取消** ⇒ 立即返回 `Cancelled`，不睡满退避。
///
/// 场景：server 回 429 + `Retry-After: 5`（在硬上限内）⇒ 适配器进入 5s 退避；
/// 测试在退避中 push `Abort` ⇒ `complete` 应**立即**以 `Cancelled` 收场。
///
/// 变异靶：把退避的 `select!` 退回裸 `sleep` ⇒ 本用例变红
/// （`complete` 会睡满 5s，外层 2s 超时 ⇒ 断言失败）。
#[tokio::test]
async fn cancellation_during_backoff_returns_promptly() {
    let server = MockServer::start(vec![json_reply(
        "429 Too Many Requests",
        vec![("retry-after", "5".into())],
        "{\"error\":\"rate limited\"}",
    )]);
    // 硬上限 10s > Retry-After 5s ⇒ 真的进入 5s 退避（而不是直接放弃）。
    let model = model_for(&server, fast_retry(3, 10_000), 30, None);

    let boundary = boundary();
    let call = {
        let boundary = boundary.clone();
        let request = request_with(&boundary);
        tokio::spawn(async move { model.complete(request, &mut NullSink).await })
    };

    // 等首个 429 落地、进入退避，再取消。
    tokio::time::sleep(Duration::from_millis(300)).await;
    boundary.push(Boundary::Abort);

    let result = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("退避期间取消应立即返回，而不是睡满 5s")
        .expect("调用 task 应结束");

    let error = result.expect_err("取消后必须是 Err");
    assert!(
        matches!(error, ModelError::Cancelled { .. }),
        "应是 ModelError::Cancelled，实际：{error:?}"
    );
    assert_eq!(
        server.request_count(),
        1,
        "退避期间取消 ⇒ 不得再发第二个请求"
    );
}
