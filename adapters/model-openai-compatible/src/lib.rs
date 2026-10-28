pub mod compat;
pub mod request;
pub mod response;
pub mod stream;

mod inline_thinking;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use compat::ProviderCompat;
use inline_thinking::{Segment, SegmentKind};
use request::*;
use response::*;
use stream::{StreamEvent, parse_sse_stream};
use ys_core::{ContentBlock, EventError, Message, Role, ToolCallId, Usage};
use ys_model::{
    FinishReason, Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse,
};
use ys_protocol::BoundarySource;

/// 挂载了 `boundary` 时的中止轮询间隔 —— 模型调用期间中止的感知延迟上界。
const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 等到边界发出中止信号。
///
/// `None` = 没有边界控制 ⇒ **永不完成**（调用方的 `select!` 只会等另外两个分支）。
async fn wait_abort(boundary: Option<&dyn BoundarySource>) {
    match boundary {
        Some(boundary) => loop {
            if boundary.is_aborted() {
                return;
            }
            tokio::time::sleep(ABORT_POLL_INTERVAL).await;
        },
        None => std::future::pending::<()>().await,
    }
}

pub struct OpenAICompatibleConfig {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// 可选推理强度；`None` = 不发送该字段（默认，兼容所有端点）。
    pub reasoning_effort: Option<String>,
    /// 构造期是否**确定要求关闭思考**（来自配置层的 `off`）。
    ///
    /// 与 `reasoning_effort` 的分工：
    /// - `reasoning_effort` 是**强度**，且经 `ProviderCompat::effort_for` 折叠；
    ///   对未声明支持的 provider 它会是 `None` ⇒ **携带不了「用户要 off」这个意愿**。
    /// - 本字段是**开关**，与能力位 `supports_thinking_control` 配对。
    ///
    /// 两者独立：即使 `reasoning_effort` 被折叠或丢弃，本字段仍能生效。
    /// 这是「`/effort off` 对 deepseek 曾是空操作」这个缺陷的修复点。
    ///
    /// 默认 `false`（既有行为不变）；只有配置层显式给 `off` 时才置 `true`。
    pub disable_thinking: bool,
    pub compat: ProviderCompat,
}

/// 模型调用的有界重试策略（ADR-0028 决策 4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    /// **额外**重试次数（不含首次）。0 = 不重试。
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    /// 单次退避硬上限；同时是服务端 `Retry-After` 的硬上限。
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    }
}

/// 模型调用的两个超时旋钮（ADR-0028 决策 5）。
///
/// 连接建立与流空闲分开设：长思考模型（如 `deepseek-reasoner`）首字节可达数十秒，
/// 若用单一总超时会把正常的「慢首字节」误判为失败。「首字节延迟」与「中途停顿」
/// 语义不同，必须分开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutConfig {
    /// 连接建立超时（秒）。
    pub connect_secs: u64,
    /// 流空闲超时（秒）：相邻两次字节到达的最长间隔。
    pub idle_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_secs: 15,
            idle_secs: 120,
        }
    }
}

/// 诊断日志的去处由调用方注入 —— 适配器不认识 app 侧的 logging 文件（ADR-0028 决策 7）。
///
/// 重试痕迹只走这条回调：**对 TUI 不可见、对排障可见**，
/// 不往 `Envelope` / `AgentEvent` / `ModelEvent` 加任何变体。
pub type ModelLogger = Arc<dyn Fn(&str) + Send + Sync>;

/// 一次模型 HTTP 尝试的结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAttemptOutcome {
    Success,
    Retryable,
    Fatal,
}

impl ModelAttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Retryable => "retryable",
            Self::Fatal => "fatal",
        }
    }
}

/// 失败为何发生。主分类按 HTTP 状态 / 传输层来源给出；端点可通过
/// `x-should-retry` 覆盖是否重试，但不会改变底层失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAttemptReason {
    Transport,
    IdleTimeout,
    RateLimited,
    ServerError,
    Quota,
    Stream,
    Fatal,
}

impl ModelAttemptReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::IdleTimeout => "idle_timeout",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::Quota => "quota",
            Self::Stream => "stream",
            Self::Fatal => "fatal",
        }
    }
}

/// 本可重试却没有重试的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAttemptRetrySuppressed {
    StreamStarted,
    Exhausted,
    BackoffCap,
}

impl ModelAttemptRetrySuppressed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StreamStarted => "stream_started",
            Self::Exhausted => "exhausted",
            Self::BackoffCap => "backoff_cap",
        }
    }
}

/// 一次 HTTP 尝试的结构化读数。
///
/// `attempt` 从 1 起；`max_attempts` 是**总尝试次数**（配置里的额外重试次数 + 1）。
/// `ttfb_ms` 只在成功解析到非空响应体字节后给出；HTTP 错误与传输失败为 `None`，
/// 它们的等待时间由 `response_headers_ms` / `duration_ms` 解释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAttempt {
    pub attempt: u32,
    pub max_attempts: u32,
    pub outcome: ModelAttemptOutcome,
    pub reason_class: Option<ModelAttemptReason>,
    pub http_status: Option<u16>,
    pub ttfb_ms: Option<u64>,
    pub response_headers_ms: Option<u64>,
    pub duration_ms: u64,
    pub backoff_ms: Option<u64>,
    pub retried: bool,
    pub retry_suppressed: Option<ModelAttemptRetrySuppressed>,
    pub stream_started: bool,
    /// 端点拒绝 `stream_options` 后，本尝试内部又发了一次请求。
    pub fallback_retry: bool,
}

/// 每次 HTTP 尝试结束时同步回调一次。由 app 注入，适配器不直接写文件。
pub type ModelTrace = Arc<dyn Fn(&ModelAttempt) + Send + Sync>;

pub struct OpenAICompatibleModel {
    config: OpenAICompatibleConfig,
    client: reqwest::Client,
    retry: RetryConfig,
    timeouts: TimeoutConfig,
    log: Option<ModelLogger>,
    trace: Option<ModelTrace>,
    /// 随**每个请求**发送的额外头。空的默认值覆盖绝大多数端点；
    /// 少数网关有接入要求（如 OpenCode Go 的 `x-opencode-session`）。
    extra_headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
    /// client 是本模型自建（`new`）还是调用方注入（`with_client`）。
    /// 自建时 `with_timeouts` 按新超时重建 client；注入时其连接策略归调用方。
    owns_client: bool,
}

impl OpenAICompatibleModel {
    /// 用自建 client（遵循进程级/系统代理）构造；超时 / 重试取默认值，可用 builder 覆盖。
    pub fn new(config: OpenAICompatibleConfig) -> Self {
        let timeouts = TimeoutConfig::default();
        let client = build_client(&timeouts);
        Self {
            config,
            client,
            retry: RetryConfig::default(),
            timeouts,
            log: None,
            trace: None,
            extra_headers: Vec::new(),
            owns_client: true,
        }
    }

    /// 用调用方提供的 `reqwest::Client` 构造。
    ///
    /// 便于注入自定义超时、连接池或代理策略（如测试里 `.no_proxy()` 直连回环），
    /// 从而不必改进程级 env——避免同一测试二进制内并行执行时相互污染。
    /// 连接策略归调用方，故 `with_timeouts` 不会重建此 client。
    pub fn with_client(config: OpenAICompatibleConfig, client: reqwest::Client) -> Self {
        Self {
            config,
            client,
            retry: RetryConfig::default(),
            timeouts: TimeoutConfig::default(),
            log: None,
            trace: None,
            extra_headers: Vec::new(),
            owns_client: false,
        }
    }

    /// 设置有界重试策略。
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// 设置两个超时旋钮。仅当 client 由本模型自建时按新值重建；
    /// 调用方注入的 client（`with_client`）其连接策略归调用方，不改动。
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        if self.owns_client {
            self.client = build_client(&self.timeouts);
        }
        self
    }

    /// 注入诊断日志回调（重试痕迹的唯一出口）。
    pub fn with_logger(mut self, log: ModelLogger) -> Self {
        self.log = Some(log);
        self
    }

    /// 注入结构化尝试读数回调。未注入时静默；生产只在 debug 实际开启时注入。
    pub fn with_trace(mut self, trace: ModelTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// 追加一个随每个请求发送的额外头（可重复调用来追加多个）。
    ///
    /// 这是**传输层**契约，与 [`RetryConfig`] / [`TimeoutConfig`] / 日志回调同类；
    /// 不进 [`OpenAICompatibleConfig`] —— 后者描述的是**方言**（字段名与 payload
    /// 形状），而网关要求的是「每个请求都得带上这个头」。
    ///
    /// 头名 / 头值不合法（`HeaderName` / `HeaderValue` 校验失败）时**丢弃该条**：
    /// 一个拼错的头不该让整次请求失败，而校验结果在构建期就固定，不需要每请求重判。
    pub fn with_extra_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::try_from(name),
            reqwest::header::HeaderValue::try_from(value),
        ) {
            self.extra_headers.push((name, value));
        }
        self
    }

    /// 落一行诊断日志；未注入 logger 时静默。**不得**改为直接打印 ——
    /// 适配器的任何直接输出都会在 TUI 起屏后冲掉整屏。
    fn log(&self, message: &str) {
        if let Some(log) = &self.log {
            log(message);
        }
    }

    fn trace(&self, attempt: &ModelAttempt) {
        if let Some(trace) = &self.trace {
            trace(attempt);
        }
    }

    /// 发送一次 `/chat/completions` 请求。只负责传输层错误，不解读状态码——
    /// 状态码由 [`Model::complete`] 统一处理（便于实现一次性回退）。
    async fn send(
        &self,
        url: &str,
        chat_request: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut request = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json");
        for (name, value) in &self.extra_headers {
            request = request.header(name.clone(), value.clone());
        }
        request.json(chat_request).send().await
    }
}

/// 按超时配置建 client。
///
/// `build()` 失败（极罕见：TLS 后端初始化失败等）时回退到 `Client::new()` ——
/// 这是一个**保命**分支（宁可丢掉自定义超时也不让整个 agent 起不来），而非正常路径。
fn build_client(timeouts: &TimeoutConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(timeouts.connect_secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 一次尝试的失败分类。只在本模块内活 —— `ModelError` 是纯 `String` 且非
/// `#[non_exhaustive]`（ADR-0028 决策 1），不承载分类。
enum AttemptError {
    /// 值得重试。`server_delay` 是服务端要求的等待（`Retry-After` / `Retry-After-ms`）。
    Retryable {
        reason: String,
        reason_class: ModelAttemptReason,
        server_delay: Option<Duration>,
    },
    /// 确定性失败，直接上报。
    Fatal {
        error: ModelError,
        reason_class: ModelAttemptReason,
    },
    /// **被中止** —— 不是故障，是用户的意图。
    ///
    /// 绝**不**归入 `Retryable`：重试会重放请求，违背用户意图。
    /// `partial` 是中止瞬间已收到的可见文本（`None` = 尚无可见输出）。
    Cancelled { partial: Option<Message> },
}

/// 单次尝试的计时与 HTTP 元数据。错误路径同样携带，便于把“失败前等在哪”落盘。
#[derive(Debug, Clone, Copy, Default)]
struct AttemptMeta {
    http_status: Option<u16>,
    ttfb_ms: Option<u64>,
    response_headers_ms: Option<u64>,
    duration_ms: u64,
    stream_started: bool,
    fallback_retry: bool,
}

struct AttemptCall {
    result: Result<ModelResponse, AttemptError>,
    meta: AttemptMeta,
}

/// 额度 / 计费耗尽的错误体措辞（大小写不敏感的子串匹配）。
///
/// `429` 语义二义：瞬时限流（该重试）与账户额度耗尽（重试无用）状态码相同。
/// 该名单是**开放的常量** —— 为新增 provider 加措辞是加法，**不得**演变成规则引擎。
const QUOTA_MARKERS: &[&str] = &[
    "insufficient_quota",
    "quota exceeded",
    "out of budget",
    "billing",
    "usage limit",
    "available balance",
];

/// 记「本次尝试是否已经吐出过 delta」。
///
/// 一旦发出过 `TextDelta` / `ThinkingDelta`，其后的任何失败**直接上报、不重试**——
/// 重放会把已渲染的文本再渲染一遍。**受损的只有渲染，不是会话**：`BasicLoop` 的
/// `Forwarder` 只把 delta 转成 UI 事件，会话里的 assistant 内容来自适配器收敛出的
/// `ModelResponse`，delta 从不进会话。但渲染侧重复同样不可接受：TUI 会把它接进同一条
/// `Assistant` 行，而 `-p` 把增量直写 stdout、**无法撤回**。
///
/// **只看这两个事件**：工具参数增量**不冒泡**（在适配器内部组装），不构成「已显示」。
struct DeltaWatchingSink<'s> {
    inner: &'s mut dyn ModelEventSink,
    emitted: bool,
}

impl<'s> DeltaWatchingSink<'s> {
    fn new(inner: &'s mut dyn ModelEventSink) -> Self {
        Self {
            inner,
            emitted: false,
        }
    }
}

impl ModelEventSink for DeltaWatchingSink<'_> {
    fn emit(&mut self, event: ModelEvent) -> Result<(), EventError> {
        if matches!(
            event,
            ModelEvent::TextDelta { .. } | ModelEvent::ThinkingDelta { .. }
        ) {
            self.emitted = true;
        }
        self.inner.emit(event)
    }
}

#[async_trait::async_trait]
impl Model for OpenAICompatibleModel {
    fn model_id(&self) -> &str {
        &self.config.model
    }

    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        // 1. 翻译 v0 内部格式 → OpenAI API 格式（`stream: true`）。
        //    `supports_stream_usage` 决定是否带 `stream_options.include_usage`。
        let mut chat_request = translate_request(&request, &self.config)?;

        // 2. URL：base 已 trim 尾部 `/`，统一追加 `/chat/completions`。
        let url = format!(
            "{}/chat/completions",
            self.config.api_base.trim_end_matches('/')
        );

        // 3. 有界重试循环。`max_attempts` 是**额外**重试次数，故总尝试数为 max_attempts + 1。
        //    每次尝试用 DeltaWatchingSink 记「本次是否已吐出 delta」——吐过就不许重试。
        let max_attempts = self.retry.max_attempts.saturating_add(1);
        // 边界源：模型调用期间的中止通道（`None` = 不支持中止）。
        // 借自 `request` 的 `Arc` ⇒ 生命周期短于 `Arc` 持有期。
        let boundary = request.boundary.as_deref();
        for attempt in 0..max_attempts {
            let started = Instant::now();
            let (outcome, meta) = {
                let mut watcher = DeltaWatchingSink::new(&mut *sink);
                let outcome = self
                    .attempt(&url, &mut chat_request, &mut watcher, boundary)
                    .await;
                let mut meta = outcome.meta;
                meta.duration_ms = started.elapsed().as_millis() as u64;
                meta.stream_started = watcher.emitted;
                (outcome.result, meta)
            };
            let mut record = ModelAttempt {
                attempt: attempt + 1,
                max_attempts,
                outcome: ModelAttemptOutcome::Success,
                reason_class: None,
                http_status: meta.http_status,
                ttfb_ms: meta.ttfb_ms,
                response_headers_ms: meta.response_headers_ms,
                duration_ms: meta.duration_ms,
                backoff_ms: None,
                retried: false,
                retry_suppressed: None,
                stream_started: meta.stream_started,
                fallback_retry: meta.fallback_retry,
            };

            match outcome {
                Ok(response) => {
                    self.trace(&record);
                    return Ok(response);
                }
                Err(AttemptError::Cancelled { partial }) => {
                    // 中止不是失败 ⇒ 不 trace 为失败、不重试。
                    return Err(ModelError::Cancelled { partial });
                }
                Err(AttemptError::Fatal {
                    error,
                    reason_class,
                }) => {
                    record.outcome = ModelAttemptOutcome::Fatal;
                    record.reason_class = Some(reason_class);
                    self.trace(&record);
                    return Err(error);
                }
                Err(AttemptError::Retryable {
                    reason,
                    reason_class,
                    server_delay,
                }) => {
                    record.outcome = ModelAttemptOutcome::Retryable;
                    record.reason_class = Some(reason_class);
                    // 已吐出 delta ⇒ 一律不重试（重放会把已渲染的文本再渲染一遍）。
                    if meta.stream_started {
                        record.retry_suppressed = Some(ModelAttemptRetrySuppressed::StreamStarted);
                        self.trace(&record);
                        return Err(ModelError::Provider(format!(
                            "{reason} (streaming already began; not retried)"
                        )));
                    }
                    // 次数耗尽 ⇒ 上报最后一次的错误（消息里带尝试次数）。
                    if attempt + 1 == max_attempts {
                        record.retry_suppressed = Some(ModelAttemptRetrySuppressed::Exhausted);
                        self.trace(&record);
                        self.log(&format!(
                            "retry giving up after {} attempt(s): {reason}",
                            attempt + 1
                        ));
                        return Err(ModelError::Provider(format!(
                            "{reason} (after {} attempt(s))",
                            attempt + 1
                        )));
                    }
                    // 计算退避；服务端 `Retry-After` 超硬上限则放弃（绝不傻等）。
                    let delay = match self.backoff_delay(attempt, server_delay) {
                        Ok(delay) => delay,
                        Err(error) => {
                            record.retry_suppressed = Some(ModelAttemptRetrySuppressed::BackoffCap);
                            self.trace(&record);
                            self.log(&format!("retry giving up: {error}"));
                            return Err(error);
                        }
                    };
                    record.backoff_ms = Some(delay.as_millis() as u64);
                    record.retried = true;
                    self.trace(&record);
                    // 进入退避前落一行日志（决策 7）：对排障可见、对 TUI 不可见。
                    self.log(&format!(
                        "retry attempt {}/{} delay={}ms reason={reason}",
                        attempt + 1,
                        self.retry.max_attempts,
                        delay.as_millis()
                    ));
                    // 退避期间也要能被中止：否则一个长 `Retry-After` 会把取消请求
                    // 拖到退避结束才生效。
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = wait_abort(boundary) => {
                            return Err(ModelError::Cancelled { partial: None });
                        }
                    }
                }
            }
        }

        // 循环在最后一次尝试失败时已 return，不可达；仅为满足类型检查。
        unreachable!("retry loop always returns")
    }
}

impl OpenAICompatibleModel {
    /// 执行**一次**尝试：发请求 → 处理非成功状态 → 流式解析并收敛。
    /// 分类（可重试 / 确定性）只活在本方法内。
    async fn attempt(
        &self,
        url: &str,
        chat_request: &mut ChatCompletionRequest,
        sink: &mut dyn ModelEventSink,
        boundary: Option<&dyn BoundarySource>,
    ) -> AttemptCall {
        let started = Instant::now();
        let mut meta = AttemptMeta::default();
        // 发送阶段的中止：请求发出后、服务端应答前被中止 ⇒ 无部分输出。
        //
        // `select!` 的结果先落到一个中间枚举再 `match`：`select!` 本身已是
        // `match` 展开，直接嵌进 `match` 的臂里会破坏解析。
        enum SendOutcome {
            Sent(Result<reqwest::Response, reqwest::Error>),
            Cancelled,
        }
        let send_outcome = tokio::select! {
            // `biased` + **中止分支在前** ⇒ 中止**确定性地**胜出。
            //
            // `select!` 默认随机挑选就绪分支 ⇒ 当 abort 与发送**同时**就绪时
            // 胜负不定（实测会时而发请求、时而中止）。已 abort 的 boundary
            // 必须**确定性地**走到中止分支 —— 否则请求照发，取消请求形同虚设。
            biased;
            _ = wait_abort(boundary) => SendOutcome::Cancelled,
            result = self.send(url, chat_request) => SendOutcome::Sent(result),
        };
        let response = match send_outcome {
            SendOutcome::Sent(Ok(response)) => response,
            SendOutcome::Sent(Err(error)) => {
                return AttemptCall {
                    result: Err(retryable_transport_error(error)),
                    meta,
                };
            }
            SendOutcome::Cancelled => {
                return AttemptCall {
                    result: Err(AttemptError::Cancelled { partial: None }),
                    meta,
                };
            }
        };
        meta.response_headers_ms = Some(started.elapsed().as_millis() as u64);
        meta.http_status = Some(response.status().as_u16());

        // 一次性回退：端点明确表示不认识 `stream_options.include_usage` 时返回 400。
        // 只有响应体把该字段本身归为不支持时才剥掉重发；普通 400（例如历史里的
        // tool call 未配对）必须原样进入错误分类，不能被误判成兼容性问题。
        // 请求本来就没带（`supports_stream_usage = false`）→ 不触发。
        let response = if response.status() == reqwest::StatusCode::BAD_REQUEST
            && chat_request.stream_options.is_some()
        {
            let status = response.status();
            let should_retry = response
                .headers()
                .get("x-should-retry")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_ascii_lowercase());
            let server_delay = parse_retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            if !is_stream_options_unsupported(&body) {
                return AttemptCall {
                    result: Err(classify_error_parts(
                        status,
                        should_retry,
                        server_delay,
                        body,
                    )),
                    meta,
                };
            }

            meta.fallback_retry = true;
            self.log(&format!(
                "warning: endpoint rejected `stream_options.include_usage` \
                 ({status}: {body}); retrying once without it — \
                 token usage for this run will be reported as 0"
            ));
            chat_request.stream_options = None;
            match self.send(url, chat_request).await {
                Ok(response) => {
                    meta.response_headers_ms = Some(started.elapsed().as_millis() as u64);
                    meta.http_status = Some(response.status().as_u16());
                    response
                }
                Err(error) => {
                    return AttemptCall {
                        result: Err(retryable_transport_error(error)),
                        meta,
                    };
                }
            }
        } else {
            response
        };

        if !response.status().is_success() {
            return AttemptCall {
                result: Err(classify_error_response(response).await),
                meta,
            };
        }

        // 流式解析：SSE 每解析出一个 delta 就立刻推给 sink（真增量，不缓冲）。
        // 每次 `stream.next()` 都由流空闲超时包住 —— 黑洞端点「接受连接但不应答」
        // 会让 complete 永久挂起，超时是重试的前置。
        let idle = Duration::from_secs(self.timeouts.idle_secs);
        // `accumulator` 声明在 `select!` **之外**（栈上存活）⇒ 中止时可取到已收到的
        // 部分文本。`select!` 只 drop 借用它的闭包 future，不动它本身。
        let mut accumulator =
            StreamAccumulator::new(&mut *sink, self.config.compat.has_reasoning_content);
        let mut first_byte_at = None;
        //
        // `select!` 的结果先落到一个中间枚举再 `match`：
        // 1. `select!` 本身已是 `match` 展开，直接嵌进 `match` 的臂里会破坏解析；
        // 2. 两个臂分别要「可变借用 `accumulator`」与「**移走** `accumulator`」，
        //    放在同一个 `match` 里会同时存活 ⇒ 借用检查不通过。
        //    拆成两步后，`select!` 结束时借用的 future 已被 drop，移走才合法。
        enum StreamOutcome {
            Finished(Result<(), ModelError>),
            Cancelled,
        }
        let stream_outcome = tokio::select! {
            // `biased` + **中止分支在前** ⇒ 中止**确定性地**胜出。
            //
            // 不加 `biased` 时，若 abort 与「流已读完」同时就绪，胜负随机
            // ⇒ 同一个输入可能时而 `Cancelled`、时而 `Ok`，不可复现。
            biased;
            _ = wait_abort(boundary) => StreamOutcome::Cancelled,
            result = parse_sse_stream(
                response,
                idle,
                |event| accumulator.handle(event),
                || {
                    first_byte_at.get_or_insert_with(Instant::now);
                },
            ) => StreamOutcome::Finished(result),
        };
        let result = match stream_outcome {
            StreamOutcome::Finished(Ok(())) => {
                // `finish()` 仅在 `StreamEvent::Error` 置位后报错，而该事件只由传输层错误
                // 产生（见 `stream::parse_sse_stream` 的 `Ok(Some(Err(..)))` 分支）——
                // 故此处把它的 `Err` 无条件归为可重试是安全的。
                // sink 自身的 `Err` 不会落到这里：`StreamAccumulator::handle` 内以
                // `let _ = self.sink.emit(..)` 吞掉它，`handle` 恒返回 `Ok`，从不外抛。
                // 因此下面 `Err` 分支当前只由流空闲超时（`stream.rs`）到达，
                // `classify_stream_error` 的 `Fatal` 分支在当前路径下不可达。
                accumulator
                    .finish_with_logger(&|message| self.log(message))
                    .map_err(|error| AttemptError::Retryable {
                        reason: error.to_string(),
                        reason_class: ModelAttemptReason::Stream,
                        server_delay: None,
                    })
            }
            StreamOutcome::Finished(Err(error)) => Err(classify_stream_error(error)),
            StreamOutcome::Cancelled => {
                // 流式阶段被中止 ⇒ 已收到的文本**不丢弃**，作为 partial 交回。
                Err(AttemptError::Cancelled {
                    partial: accumulator.into_partial(),
                })
            }
        };
        meta.ttfb_ms = first_byte_at.map(|at| at.duration_since(started).as_millis() as u64);
        AttemptCall { result, meta }
    }

    /// 计算下一次退避时长。
    ///
    /// 服务端给了 `Retry-After` 时**尊重它**，但以 `max_delay_ms` 为**硬上限**：
    /// 超过上限 ⇒ 返回 `Err` 让调用方立即上报，**绝不取「与上界取 max」**
    /// （那等于允许服务端让本仓无界地等下去，与「必须有界」自相矛盾）。
    /// 无服务端要求时用指数退避 + 抖动，抖动位置取当前时钟，
    /// 由 [`backoff_delay_with_fraction`] 完成「先抖后夹」。
    fn backoff_delay(
        &self,
        attempt: u32,
        server_delay: Option<Duration>,
    ) -> Result<Duration, ModelError> {
        let max = Duration::from_millis(self.retry.max_delay_ms);
        if let Some(server) = server_delay {
            if server > max {
                return Err(ModelError::Provider(format!(
                    "server requested retry after {}s, exceeding max_delay_ms={}; giving up",
                    server.as_secs(),
                    self.retry.max_delay_ms
                )));
            }
            return Ok(server);
        }
        Ok(Duration::from_millis(backoff_delay_with_fraction(
            attempt,
            &self.retry,
            jitter_fraction(),
        )))
    }
}

/// 无服务端要求时的退避计算，抖动位置由 `fraction` 注入。
///
/// `fraction ∈ [0.0, 1.0]` 是抖动在区间内的位置：`0.0 ⇒ 0.75×exp`、`1.0 ⇒ 1.25×exp`。
/// 指数项为 `base_delay_ms × 2^min(attempt,32)`，用 `saturating_mul` 兜住乘法饱和。
/// **先抖后夹**：抖动施加于夹取之前，故 `max_delay_ms` 是绝对上界，抖动不得使其越界。
fn backoff_delay_with_fraction(attempt: u32, retry: &RetryConfig, fraction: f64) -> u64 {
    let exp_ms = retry.base_delay_ms.saturating_mul(1u64 << attempt.min(32));
    let jittered_ms = (exp_ms as f64 * (0.75 + 0.5 * fraction)) as u64;
    jittered_ms.min(retry.max_delay_ms)
}

/// 取当前时间纳秒做抖动位置（不引新依赖），返回 `[0.0, 1.0]`。
fn jitter_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1001) as f64 / 1000.0
}

/// 传输层错误（连接 / DNS / TLS / 超时）一律可重试。
fn retryable_transport_error(error: reqwest::Error) -> AttemptError {
    AttemptError::Retryable {
        reason: format!("HTTP request failed: {error}"),
        reason_class: ModelAttemptReason::Transport,
        server_delay: None,
    }
}

/// 流阶段错误分类：空闲超时可重试（属传输层）；其余（`on_event` / sink 错误）不可重试。
fn classify_stream_error(error: ModelError) -> AttemptError {
    if error.to_string().contains(stream::IDLE_TIMEOUT_MARKER) {
        AttemptError::Retryable {
            reason: error.to_string(),
            reason_class: ModelAttemptReason::IdleTimeout,
            server_delay: None,
        }
    } else {
        AttemptError::Fatal {
            error,
            reason_class: ModelAttemptReason::Stream,
        }
    }
}

/// 非成功响应的可重试性判定，三级从左到右短路：
/// ① 响应头 `x-should-retry` 定论（不看状态码）→ ② 额度名单否决 → ③ 状态码。
async fn classify_error_response(response: reqwest::Response) -> AttemptError {
    let status = response.status();
    let should_retry = response
        .headers()
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    let server_delay = parse_retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    classify_error_parts(status, should_retry, server_delay, body)
}

/// 已读出 body 后的错误分类；与 [`classify_error_response`] 共用同一条判定链。
fn classify_error_parts(
    status: reqwest::StatusCode,
    should_retry: Option<String>,
    server_delay: Option<Duration>,
    body: String,
) -> AttemptError {
    let reason = format!("API error {status}: {body}");

    // ① 响应头定论：端点自陈的可重试性比从状态码推断更权威。
    match should_retry.as_deref() {
        Some("true") => {
            return AttemptError::Retryable {
                reason,
                reason_class: http_reason_class(status),
                server_delay,
            };
        }
        Some("false") => {
            return AttemptError::Fatal {
                error: ModelError::Provider(reason),
                reason_class: http_reason_class(status),
            };
        }
        _ => {}
    }

    // ② 额度否决：即便状态码是 429，额度耗尽也不重试。
    if looks_like_quota_exhausted(&body) {
        return AttemptError::Fatal {
            error: ModelError::Provider(reason),
            reason_class: ModelAttemptReason::Quota,
        };
    }

    // ③ 状态码：429 与 5xx 可重试，其余 4xx 是确定性失败。
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        AttemptError::Retryable {
            reason,
            reason_class: http_reason_class(status),
            server_delay,
        }
    } else {
        AttemptError::Fatal {
            error: ModelError::Provider(reason),
            reason_class: http_reason_class(status),
        }
    }
}

fn http_reason_class(status: reqwest::StatusCode) -> ModelAttemptReason {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        ModelAttemptReason::RateLimited
    } else if status.is_server_error() {
        ModelAttemptReason::ServerError
    } else {
        ModelAttemptReason::Fatal
    }
}

fn looks_like_quota_exhausted(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    QUOTA_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// 400 响应体是否明确把 `stream_options` 归为不支持。
///
/// 仅凭状态码不足以触发兼容回退：协议错误、参数错误与鉴权错误同样可能返回 400。
/// 这里要求响应体同时出现字段名与「不支持 / 未知 / 未识别」语义。
fn is_stream_options_unsupported(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("stream_options")
        && [
            "unsupported",
            "unknown",
            "unrecognized",
            "not support",
            "not supported",
            "unexpected",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 解析服务端要求的等待：`Retry-After-ms`（毫秒）优先于 `Retry-After`（秒）。
///
/// 只识别整数形式（HTTP-date 形式的 `Retry-After` 忽略——国内兼容端点不用它）。
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(ms));
    }
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// 单个 tool call 的流式累积器：按 `index` 分组。
///
/// 流式协议把 `id` / `name` / `arguments` 切成任意片段分多片下发：
/// `id` / `name` 通常只在首片出现（`id` 用首个非空值），`arguments` 是
/// **JSON 字符串片段**，必须逐片拼接后才能反序列化。
#[derive(Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn apply(&mut self, call: ChunkToolCall) {
        if let Some(id) = call.id {
            // id 只在首片可靠出现，取首个非空值即可
            self.id.get_or_insert(id);
        }
        if let Some(function) = call.function {
            if let Some(name) = function.name {
                self.name.push_str(&name);
            }
            if let Some(arguments) = function.arguments {
                self.arguments.push_str(&arguments);
            }
        }
    }

    /// 收敛为一个 `ToolUse` block。参数为空串（无参工具）时退化为 `{}`。
    fn into_content_block(self, log: &dyn Fn(&str)) -> ContentBlock {
        // 降级目标是**空对象 `{}`**，不是 `Value::Null`。
        //
        // `ContentBlock::ToolUse.arguments` 会随会话历史回传成
        // `function.arguments` 字符串：`Value::Null` 序列化后是字面量 `"null"`，
        // 而多家端点（实测 minimax 返回 `400 (2013) invalid params`）拒绝这个
        // 形状——`arguments` 必须是 JSON **对象**。空对象在工具层语义相同
        // （无参数），线上却是合法形态。
        //
        // 保守兜底：不 panic、不回喂错误结果（回喂涉及工具层协议，超出适配器
        // 职责）；非空但解析失败时打一条警告以便观测。空串是「无参工具」的
        // 正常形态，不作为异常告警。
        let arguments = if self.arguments.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str(&self.arguments) {
                Ok(value) => value,
                Err(e) => {
                    let prefix: String = self.arguments.chars().take(120).collect();
                    log(&format!(
                        "warning: model tool_call arguments for `{}` are not valid JSON \
                         ({e}); falling back to {{}}. arguments prefix: {prefix:?}",
                        self.name
                    ));
                    serde_json::Value::Object(serde_json::Map::new())
                }
            }
        };
        ContentBlock::ToolUse {
            id: ToolCallId(self.id.unwrap_or_default()),
            name: self.name,
            arguments,
        }
    }
}

/// 流式响应的累积器：边收增量边发 [`ModelEvent`]，流结束后收敛为 `ModelResponse`。
///
/// 这是 [`Model::complete`] 的流式核心，与 `reqwest::Response` 解耦——
/// 测试可直接喂 [`StreamEvent`] 驱动它，无需网络。
struct StreamAccumulator<'s> {
    sink: &'s mut dyn ModelEventSink,
    /// provider 是否声明支持 `reasoning_content`；否则该字段不可信，忽略。
    has_reasoning_content: bool,
    text: String,
    reasoning: String,
    /// 剥离 `content` 内联思考标签；内联思考只经事件出口，不写 `reasoning`。
    filter: inline_thinking::InlineThinkingFilter,
    tool_calls: BTreeMap<u32, ToolCallAccumulator>,
    finish_reason: Option<String>,
    /// 流中出现的最后一个非空 usage（OpenAI 系在末包附带）。
    usage: Option<Usage>,
    error: Option<String>,
}

impl<'s> StreamAccumulator<'s> {
    fn new(sink: &'s mut dyn ModelEventSink, has_reasoning_content: bool) -> Self {
        Self {
            sink,
            has_reasoning_content,
            text: String::new(),
            reasoning: String::new(),
            filter: inline_thinking::InlineThinkingFilter::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            error: None,
        }
    }

    /// 处理一个流事件：累积增量并**立即**向 sink 推送产物型 delta。
    fn handle(&mut self, event: StreamEvent) -> Result<(), ModelError> {
        match event {
            StreamEvent::Delta {
                content,
                reasoning_content,
                tool_calls,
                finish_reason,
                usage,
            } => {
                if let Some(content) = content {
                    // 保留空 delta 守卫：否则快路径会发出空 `TextDelta`，
                    // TUI 会落一条空 Assistant 行。
                    if !content.is_empty() {
                        match self.filter.passthrough(&content) {
                            Some(SegmentKind::Text) => {
                                self.text.push_str(&content);
                                let _ = self.sink.emit(ModelEvent::TextDelta { text: content });
                            }
                            Some(SegmentKind::Thinking) => {
                                let _ = self.sink.emit(ModelEvent::ThinkingDelta { text: content });
                            }
                            None => {
                                for seg in self.filter.feed(&content) {
                                    self.emit_segment(seg);
                                }
                            }
                        }
                    }
                }

                if self.has_reasoning_content
                    && let Some(reasoning) = reasoning_content
                    && !reasoning.is_empty()
                {
                    self.reasoning.push_str(&reasoning);
                    let _ = self
                        .sink
                        .emit(ModelEvent::ThinkingDelta { text: reasoning });
                }

                // 工具参数增量**不冒泡**：在适配器内按 index 组装。
                if let Some(calls) = tool_calls {
                    for call in calls {
                        self.tool_calls.entry(call.index).or_default().apply(call);
                    }
                }

                // usage 尾包：累积最后一个非空值（末包覆盖前值即可）。
                if let Some(usage) = usage {
                    // 缓存字段**两种方言**都要认：DeepSeek 报 `prompt_cache_*`，
                    // OpenAI 兼容网关报 `prompt_tokens_details.cached_tokens`。
                    // 只认一家 ⇒ 另一家的读数恒为「无」，会把「缓存生效」误判成「测不到」。
                    let (cache_hit, cache_miss) = usage.cache_tokens();
                    self.usage = Some(Usage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        prompt_cache_hit_tokens: cache_hit,
                        prompt_cache_miss_tokens: cache_miss,
                        // `None`（明细块缺失）与 `Some(0)`（显式报 0）原样保留，
                        // 供上层决定是否写出该字段。
                        reasoning_tokens: usage.reasoning_tokens(),
                    });
                }

                if finish_reason.is_some() {
                    self.finish_reason = finish_reason;
                }
            }
            StreamEvent::Done => {}
            StreamEvent::Error(message) => self.error = Some(message),
        }
        Ok(())
    }

    /// 把过滤器产出的段落到「累积 `text` + emit 事件」两点。
    /// Text 段同时写 `self.text`（终态 `ContentBlock::Text` 的来源）；
    /// Thinking 段只 emit，不写 `self.reasoning`。
    fn emit_segment(&mut self, seg: Segment) {
        match seg {
            Segment::Text(t) => {
                self.text.push_str(&t);
                let _ = self.sink.emit(ModelEvent::TextDelta { text: t });
            }
            Segment::Thinking(t) => {
                let _ = self.sink.emit(ModelEvent::ThinkingDelta { text: t });
            }
        }
    }

    /// 流结束后收敛为 [`ModelResponse`]。
    #[cfg(test)]
    fn finish(self) -> Result<ModelResponse, ModelError> {
        self.finish_with_logger(&|_| {})
    }

    /// 取出中止瞬间已收到的**可见文本**，作为 partial 交回。
    ///
    /// **只取 `self.text`**（非空才产出）：reasoning 不入 partial
    /// （与「Thinking 段只 emit」的既有约定一致）。
    ///
    /// **不碰 `self.tool_calls`**：中止时未完成的工具调用参数可能残缺，
    /// 执行它等于拿半个 payload 去改状态 ⇒ partial 里绝不带工具调用。
    fn into_partial(self) -> Option<Message> {
        let block = text_block(self.text)?;
        Some(Message {
            role: Role::Assistant,
            content: vec![block],
        })
    }

    /// 与 [`Self::finish`] 相同，但把工具参数修复等诊断交给注入的 logger。
    fn finish_with_logger(mut self, log: &dyn Fn(&str)) -> Result<ModelResponse, ModelError> {
        if let Some(message) = self.error {
            // 流式解析失败不做非流式回退——直接以 Provider 错误上抛。
            return Err(ModelError::Provider(format!("SSE stream error: {message}")));
        }

        // 冲刷跨片扣留：思考态残留 → Thinking（未闭合思考不外泄）、
        // 正文态假前缀 → Text（不丢字）。须在错误检查之后、构造 content_blocks 之前。
        for seg in self.filter.flush() {
            self.emit_segment(seg);
        }

        let mut content_blocks = Vec::new();
        if let Some(block) = text_block(std::mem::take(&mut self.text)) {
            content_blocks.push(block);
        }
        for (_, accumulator) in self.tool_calls {
            // 参数完整（有 name）时才产出；一个调用一个 block。
            if !accumulator.name.is_empty() {
                content_blocks.push(accumulator.into_content_block(log));
            }
        }

        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
            },
            // 流式响应仅在 provider 声明支持 `stream_options.include_usage`
            // 时才会在末包附带 usage；此处取流中最后一个非空值。
            // 端点不支持该选项时始终收不到 → 退回 `Usage::default()`（token 统计为 0），
            // 这是保守兼容的已知代价，不 panic。
            usage: self.usage.unwrap_or_default(),
            finish_reason: normalize_finish_reason(self.finish_reason),
        })
    }
}

/// 空文本 → 无 block；非空 → 一个 [`ContentBlock::Text`]。
///
/// 终态构造（[`StreamAccumulator::finish_with_logger`]）与中止时的部分输出
/// （[`StreamAccumulator::into_partial`]）共用 ⇒ 两条路径的文本形态一致。
fn text_block(text: String) -> Option<ContentBlock> {
    if text.is_empty() {
        None
    } else {
        Some(ContentBlock::Text { text })
    }
}

/// provider 的 finish_reason 方言 → [`FinishReason`]。
///
/// 分工：**方言归适配器，词汇归 `ys-model`** —— 因此 `ys-loop` 永远不必认识
/// 任何 provider 的字符串。别名（`end_turn` / `max_tokens` / `tool_use`）是
/// 零成本的保险，覆盖 Anthropic 系词汇。
fn normalize_finish_reason(raw: Option<String>) -> FinishReason {
    match raw.as_deref() {
        Some("stop") | Some("end_turn") => FinishReason::Stop,
        Some("tool_calls") | Some("tool_use") => FinishReason::ToolCalls,
        Some("length") | Some("max_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        // 未识别的方言与「provider 压根没给」都落到 Other —— 语义是「没结论」，
        // 绝不猜成 Stop（那会让下游把未知当正常收尾）。
        Some(other) => FinishReason::Other(other.to_string()),
        None => FinishReason::Other(String::new()),
    }
}

pub fn translate_request(
    request: &ModelRequest,
    config: &OpenAICompatibleConfig,
) -> Result<ChatCompletionRequest, ModelError> {
    let mut messages = Vec::new();

    // 系统提示词
    if let Some(system) = &request.system {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(system.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    // Session 消息
    for msg in &request.messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };

        let mut text_content = String::new();
        let mut tool_calls = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => text_content.push_str(text),
                ContentBlock::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    // 出口兜底：`arguments` 必须是 JSON **对象**。
                    //
                    // 非对象值（尤其 `null`）序列化后会是字面量 `null` / `[]`，
                    // 多家端点直接 400（实测 minimax：`400 (2013) invalid params`）。
                    // 这里是**最后一道关口**：历史可能来自旧版本落盘的会话、手工编辑
                    // 的 JSONL，或任何在别处生成的坏块 —— 只要放过去，就会每回合重放
                    // 同一个错误，用户只能 `/new` 自救。
                    //
                    // 归一化不改语义（工具层看到的就是「无参数」），但保证线上合法。
                    let arguments = if arguments.is_object() {
                        arguments.to_string()
                    } else {
                        "{}".to_string()
                    };
                    tool_calls.push(ChatToolCall {
                        id: id.0.clone(),
                        call_type: "function".into(),
                        function: ChatFunctionCall {
                            name: name.clone(),
                            arguments,
                        },
                    });
                }
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    // OpenAI 格式：role="tool"、tool_call_id、content
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(if *is_error {
                            format!("[ERROR] {content}")
                        } else {
                            content.clone()
                        }),
                        tool_calls: None,
                        tool_call_id: Some(tool_call_id.0.clone()),
                        name: None,
                    });
                }
                _ => {} // 处理未来的 ContentBlock 变体
            }
        }

        // 追加主消息
        if !text_content.is_empty() || !tool_calls.is_empty() {
            messages.push(ChatMessage {
                role: role.into(),
                content: if text_content.is_empty() {
                    None
                } else {
                    Some(text_content)
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                name: None,
            });
        }
    }

    // Tools 列表
    let tools = if request.tools.is_empty() {
        None
    } else {
        Some(
            request
                .tools
                .iter()
                .map(|spec| ChatTool {
                    tool_type: "function".into(),
                    function: ChatFunction {
                        name: spec.name.clone(),
                        description: spec.description.clone(),
                        parameters: spec.parameters.clone(),
                    },
                })
                .collect(),
        )
    };

    Ok(ChatCompletionRequest {
        model: config.model.clone(),
        messages,
        tools,
        stream: Some(true),
        // 仅当 provider 声明支持时才请求流式 usage；否则整个字段不序列化。
        stream_options: config
            .compat
            .supports_stream_usage
            .then_some(StreamOptions {
                include_usage: true,
            }),
        max_tokens: config.max_tokens.or(request.max_tokens),
        temperature: config.temperature.or(request.temperature),
        // 强度：请求级覆盖**优先**，否则沿用实例冻结的档位。
        //
        // 两条路的门控**不在同一处**，不要合并：
        // - config 侧：构造实例时已由 app 通过 `reasoning_effort_for` →
        //   [`ProviderCompat::effort_for`] 门控与折叠过，此处**原样透传**
        //   （再调一次 `effort_for` 会对 `standard()` 之类重复门控，改变既有字节）。
        // - 请求侧：app 看不到每次调用的覆盖值，故**必须在这里**穿过同一套
        //   `effort_for`（能力位 + 模型级代际 + 方言折叠），否则会把非法值
        //   发给严格端点（GLM 对不认识的档位直接 400）。
        reasoning_effort: match request.thinking.as_ref().and_then(|p| p.effort.as_deref()) {
            Some(level) => config.compat.effort_for(&config.model, Some(level)),
            None => config.reasoning_effort.clone(),
        },
        // 思考开关：**两条来源取或**。
        //
        // - 构造期（配置层 `off`）⇒ `config.disable_thinking`
        // - 调用期（如摘要路径）⇒ `request.thinking` 要求关
        //
        // 取或而非「请求优先」：`/effort off` 是用户的**全局意图**，
        // 不能被任何单个调用面推翻；反之单个调用面要求关时也必须生效。
        //
        // 刻不发 `enabled`：那是 provider 默认值，发它只给不认识该字段的端点添风险。
        thinking: ((config.disable_thinking
            || request
                .thinking
                .as_ref()
                .is_some_and(|p| p.disables_thinking()))
            && config.compat.supports_thinking_control)
            .then_some(ThinkingControl::Disabled),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::ThinkingOverride;
    use ys_model::ModelRequest;

    // 简单测试：验证 translate_request 生成正确的 JSON
    #[test]
    fn test_translate_request_basic() {
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "deepseek-chat".into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            disable_thinking: false,
            compat: ProviderCompat::standard(),
        };

        let request = ModelRequest {
            messages: vec![ys_core::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            }],
            tools: vec![],
            system: Some("You are helpful.".into()),
            max_tokens: None,
            temperature: None,
            boundary: None,
            thinking: None,
        };

        let chat_req = translate_request(&request, &config).unwrap();
        assert_eq!(chat_req.model, "deepseek-chat");
        assert_eq!(chat_req.messages.len(), 2); // system + user 各一条
        assert_eq!(chat_req.messages[0].role, "system");
        assert_eq!(chat_req.messages[1].role, "user");
    }

    /// **出口兜底**：`translate_request` 必须把非法 `arguments` 归一化为 `{}`。
    ///
    /// 这是最后一圈护栏：会话历史可能来自旧版本落盘的 JSONL、手工编辑，或任何在
    /// 别处生成的坏块。只要 `null` 被发上线，minimax 就回 `400 (2013)`，且这个坏块
    /// 已在历史里 ⇒ **每回合重放同一个错误**。
    ///
    /// 变异：把 `arguments.is_object()` 分支删掉（改回直接 `arguments.to_string()`）
    /// ⇒ 本用例变红（拿到 `"null"` / `"[]"`）。
    #[test]
    fn translate_request_never_emits_non_object_tool_arguments() {
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            disable_thinking: false,
            compat: ProviderCompat::standard(),
        };

        let bad_values = [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!("null"),
            serde_json::json!(42),
        ];
        for (i, bad) in bad_values.into_iter().enumerate() {
            let request = ModelRequest {
                messages: vec![ys_core::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolCallId(format!("call_{i}")),
                        name: "edit".into(),
                        arguments: bad.clone(),
                    }],
                }],
                tools: vec![],
                system: None,
                max_tokens: None,
                temperature: None,
                boundary: None,
                thinking: None,
            };

            let chat_req = translate_request(&request, &config).unwrap();
            let serialized = &chat_req.messages[0].tool_calls.as_ref().unwrap()[0]
                .function
                .arguments;
            assert_eq!(
                serialized, "{}",
                "非法 arguments（输入 {bad:?}）必须归一化为空对象，\
                 不得线上发出 null / [] / 字面量"
            );
        }

        // 合法的对象必须**原样保留**（归一化不得损害正常路径）。
        let request = ModelRequest {
            messages: vec![ys_core::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: ToolCallId("call_ok".into()),
                    name: "edit".into(),
                    arguments: serde_json::json!({"path": "a.rs"}),
                }],
            }],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
            boundary: None,
            thinking: None,
        };
        let chat_req = translate_request(&request, &config).unwrap();
        let serialized = &chat_req.messages[0].tool_calls.as_ref().unwrap()[0]
            .function
            .arguments;
        let parsed: serde_json::Value = serde_json::from_str(serialized).unwrap();
        assert_eq!(parsed, serde_json::json!({"path": "a.rs"}));
    }

    #[test]
    fn test_translate_tool_result() {
        // 验证 ToolResult 翻译为 role:"tool"
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            disable_thinking: false,
            compat: ProviderCompat::standard(),
        };

        let request = ModelRequest {
            messages: vec![ys_core::Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: ToolCallId("call_1".into()),
                    content: "file contents".into(),
                    is_error: false,
                }],
            }],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
            boundary: None,
            thinking: None,
        };

        let chat_req = translate_request(&request, &config).unwrap();
        assert_eq!(chat_req.messages[0].role, "tool");
        assert_eq!(chat_req.messages[0].tool_call_id, Some("call_1".into()));
    }

    /// 记录所有 `ModelEvent` 的测试 sink。
    #[derive(Default)]
    struct RecordingModelSink {
        events: Vec<ModelEvent>,
    }

    impl ModelEventSink for RecordingModelSink {
        fn emit(&mut self, event: ModelEvent) -> Result<(), ys_core::EventError> {
            self.events.push(event);
            Ok(())
        }
    }

    /// 把一段原始 SSE 字节喂给累积器（与 `parse_sse_stream` 共用解析核心）。
    ///
    /// `buffer` 必须在多次调用间复用——否则跨 chunk 的半行会被丢弃。
    fn drive(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>, bytes: &[u8]) {
        stream::feed_sse_bytes(buffer, bytes, &mut |ev| acc.handle(ev)).unwrap();
    }

    /// 喂一个 `delta.content` 的 SSE 行。
    fn drive_content(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>, content: &str) {
        let line = format!(
            "data: {}\n",
            serde_json::json!({"choices":[{"index":0,"delta":{"content":content}}]})
        );
        drive(buffer, acc, line.as_bytes());
    }

    /// 喂一个 `delta.reasoning_content` 的 SSE 行。
    fn drive_reasoning(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>, reasoning: &str) {
        let line = format!(
            "data: {}\n",
            serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":reasoning}}]})
        );
        drive(buffer, acc, line.as_bytes());
    }

    /// 喂正常收尾（finish_reason=stop + [DONE]）。
    fn drive_stop(buffer: &mut Vec<u8>, acc: &mut StreamAccumulator<'_>) {
        drive(
            buffer,
            acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        drive(buffer, acc, b"data: [DONE]\n");
    }

    fn text_deltas(sink: &RecordingModelSink) -> String {
        sink.events
            .iter()
            .filter_map(|ev| match ev {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn thinking_deltas(sink: &RecordingModelSink) -> String {
        sink.events
            .iter()
            .filter_map(|ev| match ev {
                ModelEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn final_text(response: &ModelResponse) -> String {
        response
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 关键不变量（design §12「增量拼接与终态消息一致」）：
    /// 逐片 emit 的 `TextDelta` 拼接结果，与最终 `ModelResponse` 里
    /// `ContentBlock::Text` 的文本**逐字节相等**。含跨 chunk 的行边界。
    #[test]
    fn streamed_text_deltas_concatenate_to_final_text_byte_for_byte() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        // 一条 SSE 行被 TCP 切成两半，第二片才补齐换行
        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"content":"Hel"#,
        );
        drive(&mut buf, &mut acc, br#"lo, "}}]}"#);
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"world\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();

        let joined: String = sink
            .events
            .iter()
            .filter_map(|ev| match ev {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let final_text = match &response.message.content[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected Text block, got {other:?}"),
        };

        assert_eq!(joined, "Hello, world");
        assert_eq!(joined, final_text, "增量拼接必须与终态文本逐字节相等");
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    /// 空 `arguments` / 非法 JSON 的降级目标是**空对象 `{}`**，绝不是 `Value::Null`。
    ///
    /// 为什么必须钉住：`ToolUse.arguments` 会随会话历史回传为
    /// `function.arguments` **字符串**。`Value::Null` 序列化后是字面量 `"null"`，
    /// 实测 minimax 对此直接返回 `400 (2013) invalid params`（`arguments` 必须是
    /// JSON 对象）。更糟的是这个 400 会**永久污染会话**：坏块已落盘，之后每个回合
    /// 都在 round=1 失败（实测 turn 3 / turn 4 皆如此），用户只能 `/new` 自救。
    ///
    /// 变异：把任一降级分支改回 `serde_json::Value::Null`
    /// ⇒ 下面的 `is_object` 断言变红。
    #[test]
    fn empty_or_invalid_tool_arguments_degrade_to_empty_object_not_null() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        // ① 空串：无参工具的正常形态。
        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_empty","function":{"name":"noargs","arguments":""}}]}}]}"#,
        );
        drive(&mut buf, &mut acc, b"\n");
        // ② 非法 JSON：模型吐出被截断的参数。
        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_bad","function":{"name":"edit","arguments":"{\"newText\":\"pub enum Content"}}]}}]}"#,
        );
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        let uses: Vec<&ContentBlock> = response
            .message
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect();
        assert_eq!(uses.len(), 2, "两个工具调用都要收敛");

        for block in uses {
            let ContentBlock::ToolUse {
                name, arguments, ..
            } = block
            else {
                unreachable!()
            };
            assert!(
                arguments.is_object(),
                "`{name}` 的 arguments 必须是 JSON 对象（线上为 `{{}}`），\
                 不得是 null —— 那会让 minimax 返回 400 (2013) 并污染会话。实际：{arguments:?}"
            );
            assert_eq!(arguments, &serde_json::json!({}), "`{name}` 应退化为空对象");
            // 线上形态复核：序列化出的字符串必须是 "{}"，不是 "null"。
            assert_eq!(arguments.to_string(), "{}", "`{name}` 线上形态必须是空对象");
        }
    }

    /// 工具参数增量：同一 `index` 的 id/name/arguments 分多片下发，
    /// 在适配器内组装成**一个**完整 `ToolUse`；工具参数不冒泡为 `ModelEvent`。
    #[test]
    fn tool_call_deltas_assemble_into_one_tool_use() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_","arguments":"{\"pa"}}]}}]}"#,
        );
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"a.txt\"}"}}]}}]}"#,
        );
        drive(&mut buf, &mut acc, b"\n");
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.message.content.len(), 1);
        match &response.message.content[0] {
            ContentBlock::ToolUse {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id.0, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, &serde_json::json!({"path": "a.txt"}));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }
        // 工具参数不冒泡：没有任何增量事件被发出
        assert!(sink.events.is_empty());
    }

    /// `reasoning_content` 仅在 compat 声明支持时才发出 `ThinkingDelta`。
    #[test]
    fn thinking_delta_only_emitted_when_compat_enabled() {
        const LINE: &[u8] =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n";

        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, true);
        let mut buf: Vec<u8> = Vec::new();
        drive(&mut buf, &mut acc, LINE);
        let _ = acc.finish().unwrap();
        assert!(
            matches!(&sink.events[0], ModelEvent::ThinkingDelta { text } if text == "hmm"),
            "events = {:?}",
            sink.events
        );

        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();
        drive(&mut buf, &mut acc, LINE);
        let _ = acc.finish().unwrap();
        assert!(sink.events.is_empty(), "compat 关闭时 reasoning 必须被忽略");
    }

    /// 内联思考跨片流：`<think>…</think>` 被切成任意片段时，
    /// TextDelta 不含思考、`ContentBlock::Text` == TextDelta 拼接、
    /// ThinkingDelta 拼接 == 思考原文，且思考不进最终文本。
    #[test]
    fn inline_thinking_tags_split_across_content_chunks() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        for chunk in ["pre<thi", "nk>sec", "ret</thi", "nk>post"] {
            drive_content(&mut buf, &mut acc, chunk);
        }
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "prepost");
        assert_eq!(final_text(&response), "prepost");
        assert_eq!(thinking_deltas(&sink), "secret");
        for ev in &sink.events {
            if let ModelEvent::TextDelta { text } = ev {
                assert!(!text.contains("secret"), "TextDelta 泄漏思考：{text:?}");
            }
        }
    }

    /// 内联思考与 `reasoning_content` 并存时互不串扰：
    /// 两个来源都进 ThinkingDelta，正文各自独立。
    #[test]
    fn inline_thinking_coexists_with_reasoning_content() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, true);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "<think>T</think>V");
        drive_reasoning(&mut buf, &mut acc, "R");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "V");
        assert_eq!(final_text(&response), "V");
        assert_eq!(thinking_deltas(&sink), "TR");
    }

    /// 无标签流仍只发 TextDelta（守 `thinking_delta_only_emitted_when_compat_enabled`）。
    #[test]
    fn plain_content_stream_emits_only_text_deltas() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "hello ");
        drive_content(&mut buf, &mut acc, "world");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "hello world");
        assert_eq!(final_text(&response), "hello world");
        assert!(
            sink.events
                .iter()
                .all(|ev| matches!(ev, ModelEvent::TextDelta { .. })),
            "events = {:?}",
            sink.events
        );
    }

    /// 未闭合内联思考在 `feed` 阶段即已作为 Thinking 发出，`flush` 无产出。
    /// （本用例不覆盖 `finish` 的 flush 接线——那由下方两条半截标签用例守。）
    #[test]
    fn unclosed_inline_thinking_emitted_during_feed() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "answer<think>partial");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "answer");
        assert_eq!(final_text(&response), "answer");
        assert_eq!(thinking_deltas(&sink), "partial");
    }

    /// `finish` 的 flush：正文态流末半截开标签（`<thi`）须释放为 Text，不丢字。
    /// 删掉 `finish` 里的 flush 循环后本用例必须变红。
    #[test]
    fn finish_flush_releases_trailing_partial_open_tag() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "answer<thi");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "answer<thi");
        assert_eq!(final_text(&response), "answer<thi");
        assert_eq!(thinking_deltas(&sink), "");
    }

    /// `finish` 的 flush：思考态流末半截闭标签（`</thi`）须释放为 Thinking，
    /// 且不进入正文。删掉 `finish` 里的 flush 循环后本用例必须变红。
    #[test]
    fn finish_flush_releases_partial_close_tag_in_thinking() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "answer<think>abc</thi");
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "answer");
        assert_eq!(final_text(&response), "answer");
        assert_eq!(thinking_deltas(&sink), "abc</thi");
    }

    /// 空 `content` delta 不得发出空 `TextDelta`（既有守卫的出口验证）。
    #[test]
    fn empty_content_delta_emits_no_event() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(&mut buf, &mut acc, "");
        drive_stop(&mut buf, &mut acc);
        let _ = acc.finish().unwrap();

        assert!(sink.events.is_empty(), "events = {:?}", sink.events);
    }

    /// 三对标签经适配器出口均剥离：`<thinking>` / `<reasoning>` 同样生效。
    #[test]
    fn all_tag_pairs_are_stripped_from_content() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive_content(
            &mut buf,
            &mut acc,
            "<thinking>t</thinking>a<reasoning>r</reasoning>b",
        );
        drive_stop(&mut buf, &mut acc);
        let response = acc.finish().unwrap();

        assert_eq!(text_deltas(&sink), "ab");
        assert_eq!(thinking_deltas(&sink), "tr");
        assert_eq!(final_text(&response), "ab");
    }

    /// 流式请求必须携带 `stream: true`。
    #[test]
    fn translate_request_sets_stream_true() {
        let config = OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            disable_thinking: false,
            compat: ProviderCompat::standard(),
        };
        let request = ModelRequest::default();
        let chat_req = translate_request(&request, &config).unwrap();
        assert_eq!(chat_req.stream, Some(true));
    }

    /// 推理强度默认不出现；显式设置时按 provider 字段名发送。
    ///
    /// 变异：去掉 `skip_serializing_if` ⇒ 默认请求多出 `"reasoning_effort":null`；
    /// 把字段名改成 `reasoning` ⇒ 显式断言变红。
    #[test]
    fn reasoning_effort_is_optional_on_the_wire() {
        let mut config = config_with(ProviderCompat::standard());
        let request = ModelRequest::default();

        let absent = translate_request(&request, &config).unwrap();
        let json = serde_json::to_value(&absent).unwrap();
        assert!(json.get("reasoning_effort").is_none(), "{json}");

        config.reasoning_effort = Some("low".into());
        let present = translate_request(&request, &config).unwrap();
        let json = serde_json::to_value(&present).unwrap();
        assert_eq!(json["reasoning_effort"], "low", "{json}");
    }

    /// 「关闭思考」的 wire 形状与门控。
    ///
    /// 三件事一起钉：
    ///  1. `thinking: None`（既有调用）⇒ **整个 `thinking` 字段不出现** ——
    ///     这是「既有行为逐字节不变」的结构保证；
    ///  2. `ThinkingOverride::off()` + provider 支持 ⇒ 发 `{"type": "disabled"}`（不是 `off`，
    ///     `off` 是非法 wire 值，实测 422）；
    ///  3. 要求关闭 + provider **不支持** ⇒ 静默不发，不得因为调用方要求而误发。
    ///
    /// 变异：① 把 `None` 也当成关闭 ⇒ 第一条变红；② 去掉
    /// `supports_thinking_control` 门控 ⇒ 第三条变红；③ 把 `disabled` 写成 `off`
    /// ⇒ 第二条变红。
    #[test]
    fn thinking_control_is_gated_and_optional_on_the_wire() {
        // ① 既有调用（None）⇒ 字段不出现。
        let config = config_with(ProviderCompat::deepseek());
        let request = ModelRequest::default();
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert!(
            json.get("thinking").is_none(),
            "既有调用不得出现 thinking 字段；json = {json}"
        );

        // ② 显式关闭 + provider 支持 ⇒ disabled。
        let request = ModelRequest {
            thinking: Some(ThinkingOverride::off()),
            ..Default::default()
        };
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert_eq!(json["thinking"]["type"], "disabled", "json = {json}");

        // ③ 显式关闭但 provider 不支持 ⇒ 不发。
        let strict = config_with(ProviderCompat::standard());
        let json = serde_json::to_value(translate_request(&request, &strict).unwrap()).unwrap();
        assert!(
            json.get("thinking").is_none(),
            "未声明支持的 provider 不得收到 thinking；json = {json}"
        );

        // `Some(true)` 也不发（provider 默认即开启，发它只增风险）。
        let request = ModelRequest {
            thinking: Some(ThinkingOverride {
                thinking: Some(true),
                effort: None,
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert!(json.get("thinking").is_none(), "json = {json}");
    }

    /// 配置层 `off`（构造期 `disable_thinking`）也要能落到 wire。
    ///
    /// 这是「`/effort off` 对 deepseek 曾是空操作」的修复点：`off` 不是合法的
    /// `reasoning_effort` 值，只能走 `thinking` 开关。故它与强度字段**相互独立**。
    ///
    /// 变异：把 `config.disable_thinking` 从接线里去掉（只留 `request.disable_thinking`）
    /// ⇒ 本用例变红。
    #[test]
    fn config_level_off_maps_to_thinking_disabled() {
        let mut config = config_with(ProviderCompat::deepseek());
        // `reasoning_effort` 恒为 None（deepseek 未声明支持强度字段）。
        assert!(config.reasoning_effort.is_none());

        let request = ModelRequest::default();
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert!(
            json.get("thinking").is_none(),
            "默认不得关闭；json = {json}"
        );

        config.disable_thinking = true;
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert_eq!(
            json["thinking"]["type"], "disabled",
            "配置层 off 必须落成 thinking.disabled；json = {json}"
        );

        // 不支持该字段的端点：即使配置要求也**不得**发送。
        let mut strict = config_with(ProviderCompat::standard());
        strict.disable_thinking = true;
        let json = serde_json::to_value(translate_request(&request, &strict).unwrap()).unwrap();
        assert!(json.get("thinking").is_none(), "json = {json}");
    }

    /// 请求级强度覆盖：**穿过 `effort_for` 的门控与折叠**，config 侧仍原样透传。
    ///
    /// 两条路刻意不同：config 侧在实例构造时已由 app 门控过，适配器再门控一次会改变
    /// 既有字节；请求侧是 app 看不到的逐调用值，**必须**在此门控，否则 GLM 会把不认识的
    /// 档位当 400 拒。
    ///
    /// 变异：把请求侧改成 `Some(level) => Some(level.to_string())`（跳过门控）⇒
    /// `standard()` 那两条断言变红；把 config 侧也改成 `effort_for` ⇒ 最后一条变红。
    #[test]
    fn request_level_effort_override_is_gated_but_config_side_passes_through() {
        // 声明支持强度的方言：请求覆盖生效。
        let config = config_with(ProviderCompat::opencode_go());
        let request = ModelRequest {
            thinking: Some(ThinkingOverride {
                thinking: None,
                effort: Some("low".into()),
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert_eq!(json["reasoning_effort"], "low", "json = {json}");

        // 未声明支持的方言：请求覆盖**不得**绕门控。
        let strict = config_with(ProviderCompat::standard());
        let json = serde_json::to_value(translate_request(&request, &strict).unwrap()).unwrap();
        assert!(
            json.get("reasoning_effort").is_none(),
            "standard 不接受强度，请求覆盖也不得绕过；json = {json}"
        );

        // 无请求覆盖 ⇒ 完全回到既有路径（config 值原样透传，不再门控）。
        let mut config = config_with(ProviderCompat::standard());
        config.reasoning_effort = Some("low".into());
        let json =
            serde_json::to_value(translate_request(&ModelRequest::default(), &config).unwrap())
                .unwrap();
        assert_eq!(
            json["reasoning_effort"], "low",
            "config 侧必须原样透传（既有行为）；json = {json}"
        );
    }

    /// **能力位置位 ≠ 发送**：opencode-go 上默认请求体逐字节不变。
    ///
    /// 这是 P1 最关键的行为保证 —— 置位后若不配 `/effort off`，那些经网关的会话
    /// **不得**开始收到一个它们以前没见过的字段。两者取或再门控，但两源默认都是「不干预」。
    ///
    /// 变异：把 `thinking` 的 `.then_some(...)` 改成无条件构造 ⇒ 本用例第一条变红；
    /// 把 `config.disable_thinking || request.disable_thinking == Some(false)` 改成
    /// 只看 `request` ⇒ 第二条变红。
    #[test]
    fn enabling_opencode_go_does_not_change_the_default_request_body() {
        let config = config_with(ProviderCompat::opencode_go());
        assert!(
            config.compat.supports_thinking_control,
            "前置：能力位已开启"
        );
        assert!(!config.disable_thinking, "前置：默认不要求关思考");

        let request = ModelRequest::default();
        let json = serde_json::to_value(translate_request(&request, &config).unwrap()).unwrap();
        assert!(
            json.get("thinking").is_none(),
            "能力位开启但未要求关思考时，不得发出 thinking；json = {json}"
        );

        // 配了 `off` 之后才落地。
        let mut off = config_with(ProviderCompat::opencode_go());
        off.disable_thinking = true;
        let json = serde_json::to_value(translate_request(&request, &off).unwrap()).unwrap();
        assert_eq!(
            json["thinking"]["type"], "disabled",
            "`/effort off` 必须能在网关方言上落地；json = {json}"
        );
    }

    /// 构造一个仅 compat 不同的最小 config。
    fn config_with(compat: ProviderCompat) -> OpenAICompatibleConfig {
        OpenAICompatibleConfig {
            api_base: "http://localhost".into(),
            api_key: "test".into(),
            model: "test".into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            disable_thinking: false,
            compat,
        }
    }

    /// `stream_options: {"include_usage": true}`。
    #[test]
    fn stream_options_serialized_when_supported() {
        let request = ModelRequest::default();
        let on = config_with(ProviderCompat {
            supports_stream_usage: true,
            ..ProviderCompat::standard()
        });
        let json = serde_json::to_value(translate_request(&request, &on).unwrap()).unwrap();
        assert_eq!(
            json.get("stream_options"),
            Some(&serde_json::json!({ "include_usage": true })),
            "json = {json}"
        );
    }

    /// `supports_stream_usage = false` 时 `skip_serializing_if` 生效，
    /// 整个 `stream_options` 字段不出现。
    #[test]
    fn stream_options_omitted_when_unsupported() {
        let request = ModelRequest::default();
        let off = config_with(ProviderCompat {
            supports_stream_usage: false,
            ..ProviderCompat::standard()
        });
        let json = serde_json::to_value(translate_request(&request, &off).unwrap()).unwrap();
        assert!(
            json.get("stream_options").is_none(),
            "不支持时不得序列化 stream_options，json = {json}"
        );
    }

    /// 流末包带 usage（OpenAI 形状：`choices: []` + `usage`）时，
    /// 累积为 `ModelResponse.usage` 的实际值，而非默认 0。
    #[test]
    fn streamed_usage_is_captured_from_final_chunk() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        // usage 尾包：choices 为空，只带统计
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18,\"prompt_cache_hit_tokens\":9,\"prompt_cache_miss_tokens\":2,\"completion_tokens_details\":{\"reasoning_tokens\":4}}}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.usage.input_tokens, 11);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(
            response.usage.prompt_cache_hit_tokens, 9,
            "缓存命中读数必须从 provider 尾包解析出来"
        );
        assert_eq!(response.usage.prompt_cache_miss_tokens, 2);
        assert_eq!(
            response.usage.reasoning_tokens,
            Some(4),
            "推理 token 必须从尾包的 completion_tokens_details 解析出来"
        );
    }

    /// 未上报缓存字段的端点：`ChatUsage` 仍能反序列化（按 0），不失败。
    #[test]
    fn streamed_usage_without_cache_fields_defaults_to_zero() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");
        let response = acc.finish().unwrap();
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.prompt_cache_hit_tokens, 0);
        assert_eq!(response.usage.prompt_cache_miss_tokens, 0);
    }

    /// 流里从头到尾没有 usage（端点不支持 `include_usage`）时，
    /// 退回 `Usage::default()`，不 panic。
    #[test]
    fn streamed_without_usage_defaults_to_zero() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.usage, Usage::default());
        assert_eq!(response.usage.input_tokens, 0);
        assert_eq!(response.usage.output_tokens, 0);
    }

    /// 映射表：**全部已知方言**逐条覆盖，含 Anthropic 系别名与未知值的兜底。
    ///
    /// 用参数化表而非散装 `assert`，是为了让「新增一种方言」时只需加一行。
    #[test]
    fn normalize_finish_reason_maps_every_known_dialect() {
        let cases: Vec<(Option<&str>, FinishReason)> = vec![
            // OpenAI 系
            (Some("stop"), FinishReason::Stop),
            (Some("tool_calls"), FinishReason::ToolCalls),
            (Some("length"), FinishReason::Length),
            (Some("content_filter"), FinishReason::ContentFilter),
            // Anthropic 系别名（零成本保险）
            (Some("end_turn"), FinishReason::Stop),
            (Some("max_tokens"), FinishReason::Length),
            (Some("tool_use"), FinishReason::ToolCalls),
            // 未知方言 / provider 未给：一律 Other，**不得**猜成 Stop
            (
                Some("some_future_thing"),
                FinishReason::Other("some_future_thing".into()),
            ),
            (None, FinishReason::Other(String::new())),
        ];

        for (raw, expected) in cases {
            let got = normalize_finish_reason(raw.map(str::to_string));
            assert_eq!(got, expected, "raw = {raw:?}");
        }
    }

    /// **下游截断保护的判据来源**：只有 `length` / `max_tokens` 算截断；
    /// `stop` / `None` / 未知值都**不是** —— 未知绝不能伪装成正常收尾。
    #[test]
    fn normalize_finish_reason_truncation_flag() {
        assert!(normalize_finish_reason(Some("length".into())).is_truncated());
        assert!(normalize_finish_reason(Some("max_tokens".into())).is_truncated());

        assert!(!normalize_finish_reason(Some("stop".into())).is_truncated());
        assert!(!normalize_finish_reason(None).is_truncated());
        assert!(
            !normalize_finish_reason(Some("some_future_thing".into())).is_truncated(),
            "未知方言不得被判为截断"
        );
    }

    /// 端到端（合成 SSE）：provider 给 `finish_reason: "length"` 时，
    /// 收敛出的 `ModelResponse.finish_reason` 必须已归一化且 `is_truncated()`。
    /// 这条守着 `finish()` 的接线，而不仅是映射函数本身。
    #[test]
    fn streamed_length_finish_reason_is_normalized_to_truncated() {
        let mut sink = RecordingModelSink::default();
        let mut acc = StreamAccumulator::new(&mut sink, false);
        let mut buf: Vec<u8> = Vec::new();

        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"cut off\"}}]}\n",
        );
        drive(
            &mut buf,
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n",
        );
        drive(&mut buf, &mut acc, b"data: [DONE]\n");

        let response = acc.finish().unwrap();
        assert_eq!(response.finish_reason, FinishReason::Length);
        assert!(response.finish_reason.is_truncated());
    }

    /// 构造一个仅重试策略可调的最小 model（client 与网络无关，退避是纯计算）。
    fn model_with_retry(retry: RetryConfig) -> OpenAICompatibleModel {
        OpenAICompatibleModel::with_client(
            config_with(ProviderCompat::standard()),
            reqwest::Client::new(),
        )
        .with_retry(retry)
    }

    /// **`max_delay_ms` 是绝对上界**：先抖后夹，抖动不得越过它。
    ///
    /// 抖动位置由 `fraction` 确定注入（不经时钟、无 `sleep`），故断言对区间内
    /// 任一位置都确定成立。`base` 远大于上限 ⇒ 先抖后夹恒夹到**恰好上限**（此处仅断言
    /// `<= cap`）；旧式（先夹后抖）会对上限本身再乘抖动，采到 `fraction > 0.5`
    /// （如 0.75 / 1.0）即 `> max` ⇒ 必红，不依赖采样概率。
    #[test]
    fn backoff_delay_never_exceeds_hard_cap() {
        let retry = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 10_000_000,
            max_delay_ms: 10,
        };
        let cap = retry.max_delay_ms;
        for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for attempt in [0_u32, 1, 5, 32] {
                let delay = backoff_delay_with_fraction(attempt, &retry, fraction);
                assert!(
                    delay <= cap,
                    "attempt={attempt} fraction={fraction}：退避 {delay}ms 越过硬上限 {cap}ms"
                );
            }
        }

        // 生产路径（抖动取时钟）同样受硬上限约束。
        let model = model_with_retry(retry);
        let delay = model
            .backoff_delay(0, None)
            .expect("无服务端要求时必有退避值");
        assert!(
            delay <= Duration::from_millis(cap),
            "时钟路径退避 {delay:?} 越过硬上限"
        );
    }

    /// 饱和 + 硬上限：`base = 2^63` 时 `base × 2^32` 溢出 `u64` ⇒ `saturating_mul`
    /// 饱和到 `u64::MAX`，抖动后仍远大于上限，被夹到**恰好上限**。
    ///
    /// 把 `saturating_mul` 换成 `*`：release（wrapping）下 `2^63 × 2^32 ≡ 0 (mod 2^64)`
    /// ⇒ `exp = 0` ⇒ 结果为 `0 ≠ 上限` ⇒ 红；debug 下算术溢出 panic ⇒ 红。
    /// 故断言「结果恒等于 `max_delay_ms`」这一确定值，在 debug 与 release 下均有判别力
    /// （断言本身不依赖 panic）。极大 `attempt` 亦不 panic。
    #[test]
    fn backoff_delay_saturates_without_overflow() {
        let retry = RetryConfig {
            max_attempts: u32::MAX,
            base_delay_ms: 1u64 << 63,
            max_delay_ms: 30_000,
        };
        for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for attempt in [32_u32, 40, u32::MAX] {
                let delay = backoff_delay_with_fraction(attempt, &retry, fraction);
                assert_eq!(
                    delay, retry.max_delay_ms,
                    "attempt={attempt} fraction={fraction}：饱和后应夹到恰好上限"
                );
            }
        }
    }
}
