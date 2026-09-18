//! 网络策略的**唯一出口**：`build_client` + `fetch_capped`。
//!
//! 搜索链、`webfetch`、以及将来任何要出网的东西都从这里拿 client 与抓取原语 ——
//! 于是「UA 是什么」「重定向怎么跟」「Content-Length 能不能信」只有一处答案。
//!
//! # 超时策略（设计 §1.7）
//!
//! | 层 | 值 | 为什么 |
//! |---|---|---|
//! | `CONNECT_TIMEOUT` | 5s | 唯一的 client 级常量：TCP 建连是**纯开销**，与预算无关 |
//! | **client 级总超时** | **不设** | 链的预算是**每次调用递减**的，固定值会与它互相打架 |
//! | `budget` 参数 | 调用方给 | 搜索切片 8s 起 / `webfetch` 30s；**预算归调用方** |
//!
//! # 重定向：手工策略而非默认
//!
//! client 的 redirect 策略**每一跳重跑 [`guard::vet_url`]** 并计数 ≤ [`MAX_REDIRECTS`]。
//! 否则 `302 → http://169.254.169.254/` 会**直接绕过** `webfetch` 入口那次校验
//! （入口只校验模型给的 URL，而重定向目标是服务端给的）。
//!
//! ⚠️ 注意适用面：redirect 策略是 **client 级**的，故它对**链上后端**的请求同样生效 ——
//! 这是刻意的（后端也不该把人往内网带）。而**后端的初始地址**永不经过 `vet_url`
//! （它来自配置），所以自建 `http://127.0.0.1:8888` 的 SearXNG 照常工作。

use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder};
use ys_protocol::BoundarySource;

use crate::guard;

/// TCP 建连超时。**唯一**归 client 级的超时常量（见模块文档）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 重定向最大跳数（不含初始请求）。每一跳都要重跑 SSRF 守卫。
pub const MAX_REDIRECTS: usize = 5;

/// 中止探针的轮询间隔 —— 与 `exec::run_bounded` 的 `ABORT_POLL_INTERVAL` 同值：
/// 中止的感知延迟上界。
const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `Content-Length` 能影响预分配的**上限**。
///
/// `Content-Length` 是**对端的声明**，不是事实：`Content-Length: 4GiB` 若直接
/// `Vec::with_capacity` 就是一次**低成本内存放大攻击**（一个几行的响应花掉我们 4 GiB）。
/// 真实响应体的增长由 `cap` 兜住，这里只保留「常见页面一次到位」的收益。
const PREALLOC_MAX: usize = 256 * 1024;

/// 出网 UA。不伪装浏览器，但也不留空白（部分站点直接拒无 UA 请求）。
const USER_AGENT: &str = concat!("yushan-coding-agent/", env!("CARGO_PKG_VERSION"));

/// 一次有界抓取的结果（设计 §1.3 ④）。
///
/// **只有 4 个字段是刻意的**：HTTP 状态码**不在这里** —— 非 2xx 在 [`fetch_capped`]
/// 里就已经是 [`FetchError::Status`] 了（搜索链要把 429/5xx 与「解析失败」分开记理由）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// **最终** URL（重定向之后）。落盘文件的 `Source:` 行用它，不是模型给的原始 URL。
    pub final_url: String,
    /// `Content-Type` 原样（含 `charset=…`）；缺失为 `None`。
    /// 分派（html→convert / text→原样 / 其它→明确错误）归 `fetch.rs`。
    pub content_type: Option<String>,
    /// 响应体（**已收完整块**；`capped` 时是前 `cap` 字节）。
    pub body: Vec<u8>,
    /// 是否因达到 `cap` 而**提前停止读取**（不是「读完了但正好等于 cap」）。
    pub capped: bool,
}

/// 抓取失败。
///
/// # 为什么是自定义类型而不是 `ToolError`（设计写的是后者）
///
/// 设计 §1.3 写的是 `Result<Fetched, ToolError>`。这里**刻意改成独立的 `FetchError`**：
///
/// 1. **`ToolError` 缺两个必须区分的态** —— 「被中止」（用户取消，正常收场）与
///    「超时」（对端太慢，值得重试/换后端）都是 `ToolError::Timeout`，而
///    `search.rs` 要按 `TimedOut` 决定「换下一个后端」、按 `Aborted` 决定「整条链停」。
///    用 `ToolError` 就得靠**字符串匹配**把它们分开。
/// 2. **本层不该决定「工具失败 vs 错误结果」**。按 ADR-0001，工具失败 = 基础设施问题
///    （模型改参数也没用），`is_error` 结果 = 模型可以据此改变行为的**数据**。
///    这一层只负责**如实分类**，转换归 `fetch.rs`。
///
/// ⚠️ **同一约束仍然成立**：网络族的失败（DNS / 连接 / 超时 / TLS / 非 2xx）最终都要由
/// `fetch.rs`（T11）渲染成 **`is_error` 结果**而不是工具失败 —— 对端 404 是世界的
/// 事实，模型换一个 URL 就能解决，不该判成「工具坏了」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 请求**构造**失败（URL 非法 / header 非法）—— 还没出网。
    Request(String),
    /// DNS / TCP / TLS / 读取中途断流。**对端的可达性问题**。
    Transport(String),
    /// 超预算：要么是 `budget`，要么是调用方在 `RequestBuilder` 上设的切片超时。
    TimedOut,
    /// `BoundarySource::is_aborted()` —— **用户取消**，不是失败。
    Aborted,
    /// 重定向失败：跳数超 [`MAX_REDIRECTS`]，或**某一跳没通过 [`guard::vet_url`]**。
    ///
    /// 文案里带得动守卫给的理由（靠 `err_text` 把 `source()` 链拼上 ——
    /// `reqwest::Error` 的 `Display` **只印 kind 与 url、不印 source**）。
    Redirect(String),
    /// **非 2xx**。带着状态码与最终 URL —— 搜索链的「逐条列理由」要靠它。
    Status { status: u16, url: String },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(why) => write!(f, "bad request: {why}"),
            Self::Transport(why) => write!(f, "network error: {why}"),
            Self::TimedOut => write!(f, "timed out"),
            Self::Aborted => write!(f, "aborted by user"),
            Self::Redirect(why) => write!(f, "redirect refused: {why}"),
            Self::Status { status, url } => {
                write!(f, "HTTP {status} from {url}")
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// 构造 HTTP client。
///
/// - `USER_AGENT`（本仓库**第一个** UA 策略：此前所有出网请求都是裸的）
/// - [`CONNECT_TIMEOUT`]
/// - **不设** client 级总超时（模块文档）
/// - redirect 用 [`Policy::custom`]：**每一跳重跑 `vet_url` + 计数 ≤ [`MAX_REDIRECTS`]**
///
/// 返回 `Result` 而不是直接 `Client`：`Client::build()` 会因 TLS 后端初始化失败而报错，
/// 而那是**真实的降级场景**（容器 / 异常动态库），该由 `WebConfig::from_env` 落成
/// `offline()`（工具照常注册、调用期明确报错），而不是让进程在启动期 panic。
pub fn build_client() -> Result<Client, reqwest::Error> {
    build_client_with(false)
}

/// [`build_client`] 的**可配置构造器**：`allow_loopback` 透传给重定向策略里的
/// [`guard::vet_url_with`]。
///
/// 存在的理由：重定向**跳数上限**只有让假服务器**重定向到自己**（回环）才能测 ——
/// 否则要么打真实网络，要么只能测到「第一跳就被拒」。与 [`guard::vet_url_with`] 同款约定：
/// **是构造参数、不是 env**，生产路径恒为 `false`（[`build_client`]）。
///
/// ⚠️ 测试若目的是**绕开进程代理**（本机有系统 HTTP 代理时，打 `127.0.0.1` 假服务器的
/// 请求会被代理截走），**不要**动这个构造器、**更不要**设 `NO_PROXY` 进程 env
/// （进程级 env 改写与并行测试的 env 读取是数据竞争）—— 改用 [`redirect_policy`]：
/// 把**生产那份策略**挂到 `Client::builder().no_proxy()` 上即可（策略与代理正交）。
pub fn build_client_with(allow_loopback: bool) -> Result<Client, reqwest::Error> {
    Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect_policy(allow_loopback))
        .build()
}

/// 重定向策略：**每一跳都过 SSRF 守卫**，且总跳数 ≤ [`MAX_REDIRECTS`]。
///
/// `previous()` 含**初始请求**（reqwest 自己的 `Policy::limited` 注释与实现都是这个口径），
/// 故判据写 `> MAX_REDIRECTS` 而不是 `>=`：`previous().len() == 6` 意味着已经跟了 5 跳。
///
/// # 为什么是 `pub`（策略与代理**正交**）
///
/// 生产 client（[`build_client`]）**必须**尊重进程代理 —— 企业环境正是靠它出网，
/// 故不能为了测试把它关掉。而本机有系统 HTTP 代理时，测试打 `127.0.0.1` 假服务器
/// （`MockServer`）的请求会被代理截走：重定向根本到不了被测的那段逻辑。
///
/// 把策略**单独**暴露出来，测试就能：
///
/// ```ignore
/// Client::builder().no_proxy().redirect(redirect_policy(false)).build()
/// ```
///
/// **被测的仍是生产这一份策略**（[`build_client`] 内部用的就是它），只是换了代理开关。
/// 这比「在测试里设 `NO_PROXY` 环境变量」正确：进程级 env 改写与并行测试的 env 读取
/// 是数据竞争（见 `tests/common/mod.rs` 的 `no_proxy_client`）。
pub fn redirect_policy(allow_loopback: bool) -> Policy {
    Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(format!("too many redirects (max {MAX_REDIRECTS})"));
        }
        match guard::vet_url_with(attempt.url(), allow_loopback) {
            Ok(()) => attempt.follow(),
            Err(why) => attempt.error(format!("redirect target refused by SSRF guard: {why}")),
        }
    })
}

/// 有界抓取：`cap` 是**字节上限**，`budget` 是**整次抓取**（含建连与读完）的时间上限。
///
/// - **越界即停、不排空**：达到 `cap` 就停止读取并返回（`capped = true`），**不把剩下的
///   响应体读完**。⚠️ 这与同族的 `exec::run_bounded`（`tools-basic`）**刻意相反** ——
///   那里的子进程会因管道写满而**阻塞在 `write()`**，不排空是死锁；而 HTTP 对端不是
///   我们的子进程，它**不会因为不被读而卡死**，多读 1 GiB 只为「礼貌」纯属浪费
///   （流量、时间、内存三样都浪费）。**请不要把这两处「统一」掉。**
/// - **`Content-Length` 不当分配指令**：只看它做**有上限**的预分配（见 `PREALLOC_MAX`）。
/// - **中止**：循环全程与 [`BoundarySource::is_aborted`] 赛跑，命中即 `Aborted`（见 `wait_aborted`）。
/// - **超时**：`budget` 包住**整个** future（`send` + 读体），超时返回 [`FetchError::TimedOut`]。
///
/// # UTF-8 边界（本项目已因此出过血）
///
/// 本函数返回的是**字节**（`body: Vec<u8>`），不做任何解码；`convert.rs` 也要求整块输入。
/// 故多字节字符被 TCP 分片切开**在这里不是问题**。将来若有人把它改成**流式解码**，
/// 必须改成「字节级缓冲、只在完整行/字符上解码」—— 现在不这么做只是因为下游
/// （`htmd`）要整块输入，不是因为这条约束消失了。
///
/// # 错误
///
/// 见 [`FetchError`]：**这一层只分类，不决定工具成败**（ADR-0001 的转换在 `fetch.rs`）。
///
/// # 入口 URL 的校验不属于本函数
///
/// 本函数**不校验** `req` 里的初始 URL —— 那是 `fetch.rs` 调 [`guard::vet_url`] 的事，
/// 而链上后端的配置地址**必须**绕开它（自建 SearXNG 就在回环上）。它只经 client 的
/// redirect 策略间接把关**重定向目标**（见 [`build_client`]）—— 故**注入自己的 client**
/// 时要注意：默认 reqwest client 会跟 10 跳且**不做 SSRF 校验**。
pub async fn fetch_capped(
    req: RequestBuilder,
    cap: usize,
    budget: Duration,
    b: Option<&dyn BoundarySource>,
) -> Result<Fetched, FetchError> {
    tokio::select! {
        biased;
        // 中止优先：用户已经取消时，报「超时」会让人以为还能再等一等。
        () = wait_aborted(b) => Err(FetchError::Aborted),
        r = tokio::time::timeout(budget, read_response(req, cap)) => {
            r.unwrap_or(Err(FetchError::TimedOut))
        }
    }
}

/// 发请求 + 校验状态 + 逐 chunk 读到 `cap`。
async fn read_response(req: RequestBuilder, cap: usize) -> Result<Fetched, FetchError> {
    // `mut`：`chunk()` 要 `&mut self`。
    let mut resp = req.send().await.map_err(map_reqwest)?;
    let final_url = resp.url().to_string();
    let status = resp.status();
    if !status.is_success() {
        // 非 2xx **不读体**：搜索链要的是「谁、为什么」，错误页正文没有价值。
        return Err(FetchError::Status {
            status: status.as_u16(),
            url: final_url,
        });
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let mut body: Vec<u8> = Vec::new();
    if let Some(len) = resp.content_length() {
        // ⚠️ `Content-Length` 是**声明**，不是分配指令。夹在 `cap` 与 `PREALLOC_MAX` 之下。
        let hint = usize::try_from(len)
            .unwrap_or(cap)
            .min(cap)
            .min(PREALLOC_MAX);
        body.reserve(hint);
    }

    let mut capped = false;
    while let Some(chunk) = resp.chunk().await.map_err(map_reqwest)? {
        let room = cap.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            capped = true;
            // 越界即停、**不排空**（见函数文档；与 `exec::run_bounded` 刻意相反）。
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Fetched {
        final_url,
        content_type,
        body,
        capped,
    })
}

/// 中止等待：`Some` 时每 [`ABORT_POLL_INTERVAL`] 探一次；`None`（`-p` / `--json` 模式
/// 恒为 `None`）时**永不就绪** —— 由 `select!` 的另一个分支（预算）兜底。
///
/// 独立成一个 future（而非在 chunk 循环里 `is_aborted()`）是为了**挂起也能中止**：
/// 对端迟迟不发第一个字节时，chunk 循环根本没机会轮询。
async fn wait_aborted(b: Option<&dyn BoundarySource>) {
    let Some(src) = b else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if src.is_aborted() {
            return;
        }
        tokio::time::sleep(ABORT_POLL_INTERVAL).await;
    }
}

/// `reqwest::Error` → [`FetchError`]。只做分类，不做文案加工。
fn map_reqwest(e: reqwest::Error) -> FetchError {
    if e.is_redirect() {
        // reqwest 报的重定向错误是 `Kind::Redirect`（策略里的 `attempt.error(..)` 经
        // `error::redirect()` 构造），故判别可靠；**但它的 `Display` 不印 source**，
        // 我们必须自己把策略给的理由捞出来，否则「为什么被拒」就丢了。
        FetchError::Redirect(err_text(&e))
    } else if e.is_timeout() {
        // 含调用方在 `RequestBuilder` 上设的切片超时（搜索链的 `slice`）。
        FetchError::TimedOut
    } else if e.is_builder() {
        FetchError::Request(err_text(&e))
    } else {
        // `is_connect` / `is_body` / `is_decode` / 其余一律归「对端可达性」。
        FetchError::Transport(err_text(&e))
    }
}

/// 把 `Display` 与整条 `source()` 链拼成一段可用文案。
///
/// `reqwest::Error` 的 `Display` **只印 kind 与 url**（见其 `error.rs`），策略给的理由
/// 在 `source()` 里 —— 直接 `e.to_string()` 会得到「error following redirect for url (…)」
/// 这种**看不出原因**的句子。
fn err_text(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        out.push_str(": ");
        out.push_str(&inner.to_string());
        src = inner.source();
    }
    out
}
