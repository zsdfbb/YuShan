//! 集成测试的**假服务器基建**（设计 §1.1 的 `tests/common/mod.rs`）。
//!
//! 刻意**不复用** `apps/coding-agent/tests/stream_usage_fallback.rs` 里的 `MockServer`：
//! 那份是文件私有（跨 crate 用不了）、且**没有超时护栏**（网络路径的失败形态是
//! 「不返回」，没有护栏只会看到测试挂死）。本文件是它的严格超集：
//!
//! | 能力 | 为什么必须有 |
//! |---|---|
//! | **脚本化应答** `Vec<MockReply>` 按序派发 | 「500 → 200+HTML → 200+JSON」这类**链降级**场景变成声明式 |
//! | **`delay`** | 造挂起：测超时 / 预算 / 中止（这三条都要求对端**先不发**） |
//! | **`guarded(label, fut)`** | 超时即 panic，把「挂死」变成「红」 |
//! | 记录收到的请求（method/path/body） | 断言「用了几次、带的什么参数」 |
//! | 每连接一请求 + `Connection: close` | 让每次请求**可分别计数**（keep-alive 会把它们合并） |
//!
//! # 两个踩出来的坑（照抄 `stream_usage_fallback.rs:134-138`）
//!
//! 1. `accept()` 返回的连接在 macOS/BSD 上**继承监听 socket 的 `O_NONBLOCK`** ——
//!    不复位的话 `set_read_timeout` 被忽略、`read` 在客户端字节到达前就返回 EAGAIN。
//! 2. 监听 socket **必须**非阻塞（否则 accept 线程没法轮询停止标志），
//!    而连接 socket **必须**阻塞。两者相反，正是上面那条的原因。
//!
//! # 停止语义
//!
//! `Drop` 只做两件事：置 `stop` + join **accept 线程**（它每 5ms 看一次标志，秒退）。
//! 每连接的处理器跑在**独立线程**里：`delay` 期间也在轮询 `stop`，所以挂起中的连接
//! 同样会及时收摊（不会留一个睡 300s 的线程吊住测试进程）。

#![allow(dead_code)] // 测试基建：不同测试文件各取所需，单看某一文件必有「没人用」的助手。

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// [`guarded`] 的默认上界。**测试失败必须是「红」，不能是「挂着」**。
pub const GUARD: Duration = Duration::from_secs(20);

/// 读一个请求的超时（真到了这里说明测试本身写错了）。
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// `stop` 的轮询粒度 —— 也是 `Drop` 与挂起连接的收摊延迟上界。
const STOP_POLL: Duration = Duration::from_millis(5);

/// 中止感知的轮询粒度（`delay` 期间）。
const DELAY_STEP: Duration = Duration::from_millis(20);

/// 超时护栏：`fut` 在 [`GUARD`] 内没返回就 **panic**。
///
/// 本项目网络路径的失败形态是「**不返回**」（等一个永远不来的响应），没有护栏的话
/// 表现是测试挂死 —— 那种「红」既不告诉你是哪条断言，也会吊住 CI。
pub async fn guarded<F: std::future::Future>(label: &str, fut: F) -> F::Output {
    match tokio::time::timeout(GUARD, fut).await {
        Ok(v) => v,
        Err(_) => panic!("[guarded] `{label}` 在 {GUARD:?} 内未返回（挂死而非失败）"),
    }
}

/// 直连回环、绕过代理的 client。
///
/// 用 **per-client** `.no_proxy()` 而不是改进程 env：改写进程 env（`set_var`/`remove_var`）
/// 会与同一测试二进制内并行测试的 env 读取产生**数据竞争**。
pub fn no_proxy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build no-proxy test client")
}

/// 直连回环、绕过代理，但**带上生产的重定向策略** [`redirect_policy`] 的 client。
///
/// 为什么需要它（真缺陷，不是洁癖）：测**重定向策略**的用例必须用生产策略
/// （策略挂在 client 上，换 client 就换了被测对象），而生产 client（`build_client`）
/// **必须**尊重进程代理 —— 企业环境靠它出网。于是在**有系统 HTTP 代理的机器上**，
/// 打 `127.0.0.1` 假服务器的请求被代理截走（拿到的是代理的 503），
/// 「302 → 回环应被守卫拒」这类用例**必红**，与策略本身对不对无关。
///
/// 修法是承认**策略与代理正交**：策略仍取自生产那份 [`redirect_policy`]，
/// 只把代理关掉。**不要**改成在测试里设 `NO_PROXY` 环境变量 —— 那是掩盖问题，
/// 且进程级 env 改写与并行测试的 env 读取是数据竞争（见 [`no_proxy_client`]）。
///
/// [`redirect_policy`]: ys_tools_web::redirect_policy
pub fn policy_client(allow_loopback: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(ys_tools_web::redirect_policy(allow_loopback))
        .build()
        .expect("build policy test client")
}

/// 一条脚本化应答。
#[derive(Clone, Debug)]
pub struct MockReply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// 从**收到请求**到写出响应之间的挂起（造超时 / 中止场景）。
    pub delay: Duration,
    /// 响应**写完**之后继续挂住连接不关（造「先发一部分、再停住」—— 排空实现
    /// 会在这里撞预算；正常实现越界即停、早就走了）。
    pub hold_after: Duration,
    /// 响应头里声明的 `Content-Length`；`None` = 用 `body.len()`（正常形态）。
    ///
    /// 用来造「声明一个大得离谱的长度、却只发一点点」：`fetch_capped` 若把
    /// `Content-Length` 当分配指令，就是一次低成本内存放大攻击。
    pub declared_length: Option<u64>,
    /// 附加响应头（如重定向的 `Location`）。
    pub extra_headers: Vec<(String, String)>,
    /// **完全不发** `Content-Type` 头（`fetch.rs` 的 `content_type: None` 那一格）。
    ///
    /// 与「发了但值为空」（`MockReply::new(…, "")`）是**两条不同的路径**：
    /// 前者 `headers().get(CONTENT_TYPE)` 得 `None`，后者得 `Some("")` ——
    /// `fetch::is_blank` 必须把两者都判成「没有信息」。
    pub omit_content_type: bool,
}

impl MockReply {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: content_type.to_string(),
            body: body.into(),
            delay: Duration::ZERO,
            hold_after: Duration::ZERO,
            declared_length: None,
            extra_headers: Vec::new(),
            omit_content_type: false,
        }
    }

    /// `200 + text/html` —— `webfetch` 的常规路径。
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    /// `200 + application/json` —— 搜索后端的常规路径。
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "application/json", body)
    }

    /// `200 + text/plain`。
    pub fn text(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/plain", body)
    }

    /// `status + Location:` —— 重定向。`status` 用 301/302/307/308 之一。
    pub fn redirect(status: u16, location: &str) -> Self {
        Self::new(status, "text/plain", "").with_header("Location", location)
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// 响应**写出之后**继续挂住连接（不关、不再发）—— 见字段说明。
    pub fn with_hold_after(mut self, hold: Duration) -> Self {
        self.hold_after = hold;
        self
    }

    /// 谎报 `Content-Length`（造内存放大场景）—— 见字段说明。
    pub fn with_declared_length(mut self, declared: u64) -> Self {
        self.declared_length = Some(declared);
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers
            .push((name.to_string(), value.to_string()));
        self
    }

    /// 头里**不发** `Content-Type`（见字段说明）。
    pub fn without_content_type(mut self) -> Self {
        self.omit_content_type = true;
        self
    }
}

/// 服务端记录到的一次请求（断言用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// 回环上的极简 HTTP/1.1 假服务器。
///
/// 应答按 **第几号请求**（从 0 起）从脚本里取；脚本用尽后**重复最后一条** ——
/// 「同一个应答服务 N 次请求」于是不必手写 N 份。
pub struct MockServer {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    pub fn start(replies: Vec<MockReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("mock server addr").port();
        // 监听 socket 非阻塞：accept 线程要靠轮询 `stop` 才能退出。
        listener.set_nonblocking(true).expect("set_nonblocking");

        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let accept = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        // 每连接一个线程：挂起中的连接不得挡住后续 accept。
                        Ok((stream, _)) => {
                            let replies = replies.clone();
                            let requests = Arc::clone(&requests);
                            let stop = Arc::clone(&stop);
                            std::thread::spawn(move || {
                                serve_once(stream, &replies, &requests, &stop);
                            });
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            std::thread::sleep(STOP_POLL);
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            port,
            requests,
            stop,
            accept: Some(accept),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 拼一个本服务器的 URL（`path` 无需带前导 `/`）。
    pub fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("http://127.0.0.1:{}{path}", self.port)
        } else {
            format!("http://127.0.0.1:{}/{path}", self.port)
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.path).collect()
    }

    pub fn bodies(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.body).collect()
    }

    /// 清空记录（跨阶段的断言用：「后半程有没有再发请求」）。
    pub fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

/// 处理一个连接：读一个请求、记录、按脚本应答、关闭。
fn serve_once(
    mut stream: TcpStream,
    replies: &[MockReply],
    requests: &Arc<Mutex<Vec<Recorded>>>,
    stop: &AtomicBool,
) {
    // ⚠️ macOS/BSD：`accept()` 的连接**继承监听 socket 的 `O_NONBLOCK`** → 下面的
    // `set_read_timeout` 会被忽略、`read` 在客户端字节到达前就返回 EAGAIN，客户端看到
    // "connection closed before message completed"。**必须显式复位为阻塞**。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let Ok(req) = read_request(&mut stream) else {
        return;
    };
    let idx = {
        let mut guard = requests.lock().unwrap();
        guard.push(req);
        guard.len() - 1
    };

    // 脚本用尽 → 重复最后一条（「同一应答服务 N 次」不必写 N 份）。
    let Some(reply) = replies.get(idx).or_else(|| replies.last()).cloned() else {
        return;
    };

    sleep_until_stopped(reply.delay, stop);
    if stop.load(Ordering::SeqCst) {
        return;
    }
    let _ = write_response(&mut stream, &reply);
    // 写出后继续挂住（`hold_after` 非零时）：客户端此时看到的是「响应头已到、
    // 体只来了一部分、连接还开着」—— 正是「排空 vs 不排空」唯一能分出来的形状。
    sleep_until_stopped(reply.hold_after, stop);
}

/// 读到 `\r\n\r\n`，再按 `Content-Length` 读体。
fn read_request(stream: &mut TcpStream) -> std::io::Result<Recorded> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "eof"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let first = head.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let content_length = head
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

    Ok(Recorded {
        method,
        path,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn write_response(stream: &mut TcpStream, reply: &MockReply) -> std::io::Result<()> {
    // 默认如实声明；`declared_length` 用来**谎报**（内存放大 / 排空陷阱两处要用）。
    let declared = reply.declared_length.unwrap_or(reply.body.len() as u64);
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reason(reply.status),
        declared
    );
    // `omit_content_type` → 整个头都不发（与「发一个空值」是两条不同的路径，见字段说明）。
    if !reply.omit_content_type {
        head.push_str(&format!("Content-Type: {}\r\n", reply.content_type));
    }
    for (name, value) in &reply.extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

/// 状态码 → 原因短语（手写一张小表，避免为此引依赖）。
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// 分片友好的子串查找（不引 `regex`，也不假设 `needle` 非空）。
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 「睡 `total`，但 `stop` 一置位就立刻醒」—— 否则一个 300s 的挂起会吊住测试进程。
fn sleep_until_stopped(total: Duration, stop: &AtomicBool) {
    let mut left = total;
    while left > Duration::ZERO {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let step = DELAY_STEP.min(left);
        std::thread::sleep(step);
        left = left.saturating_sub(step);
    }
}
