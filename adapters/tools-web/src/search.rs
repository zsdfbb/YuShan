//! `websearch` —— **链驱动**：一条冻结的后端链 + 总预算 + 自适应切片。
//!
//! # 数据流（设计 §1.2 的 `websearch` 一段）
//!
//! ```text
//! query → Budget::new(budget)
//!       → for (i, backend) in chain（启动期冻结、顺序由 const TABLE 决定）:
//!             remaining < MIN_SLICE → 记 "budget exhausted"，**停**
//!             slice = min(remaining, max(MIN_SLICE, remaining / backends_left))
//!             plan(b) → fetch(.timeout(slice)) → parse
//!               错误 / 超时 / 非 2xx / 解析失败 → 记理由，**继续**
//!               空结果 → **停**（真没结果，不白跑后面的后端）
//!               有结果 → **停**，胜出
//!       → 全败 → is_error 结果，逐条列「谁、为什么」（与链同序）
//!       → dedupe_by_url（保序）→ 截 limit → snippet 过 truncate::line(300) → head
//!       → notice：非第一顺位胜出时 `[via x]` **且**附上此前失败者的理由（ADR-0016）
//! ```
//!
//! # 三条承重语义（违反哪条都会变成真 bug）
//!
//! 1. **降级只认错误/限额，空结果不降级** —— 这是链的**核心区分**。把 `parse` 的
//!    `Err`（形状不对 / 非 JSON / 业务错误码）当空结果，一家改个字段名整条链就再也不
//!    会去试别人；反过来把空结果当错误，一个真没结果的查询会在五家上各跑一遍白等。
//! 2. **所有网络族失败都是 `is_error` 结果，不是工具失败**（ADR-0001）：对端 429 /
//!    超时 / 404 是**世界的事实**，模型换一个查询词或后端就能解决，判成「工具坏了」
//!    只会让它放弃。故本模块里 **`?` 只用于参数缺失**，`FetchError` 一律**降级成理由**。
//! 3. **`Aborted` 立刻停链**（不是记理由继续）：用户已经取消，再去试后面几家既无意义
//!    又违背取消的语义（`fetch_capped` 的 abort 分支只在 `is_aborted()` 命中时返回）。
//!
//! # 为什么预算与切片是**公开纯函数**（`slice_for`）+ **可注入的 `run_chain`**
//!
//! 设计 §1.7 把「**任何被尝试的后端至少拿到 8s**」定为**可断言的性质**。若预算
//! 埋在 `call()` 里恒为 [`CHAIN_BUDGET`]，测试只能靠真等 8s 去逼近这个性质（一条
//! 用例 8 秒起），或者根本测不到「预算耗尽 ⇒ 一个都不试」。故：
//!
//! - [`slice_for`] 是**纯函数**：`remaining` / `backends_left` 进，`Option<Duration>` 出
//!   （`None` = 不试这一家）。切片规则的全部内容都在这里，可被直接断言。
//! - [`run_chain`] 把 `budget` 作为**参数**（`call()` 传 [`CHAIN_BUDGET`]，测试传
//!   400ms 即可断言「全跳过且总耗时 <1s」）—— 与 `exec::run_bounded` 的 `deadline`
//!   同思路：**预算归调用方，不埋在库层**。
//!
//! # 响应体上限（[`RESPONSE_CAP`]）
//!
//! `fetch_capped` 要求一个 `cap`。搜索响应是 JSON、正常几 KB～几百 KB；2 MiB 已经
//! 是异常值。**截断的 JSON 必然解析失败**，故 `capped` 单独成一个理由（`TooLarge`），
//! 而不是让它在下游变成一句误导性的「不是合法 JSON」。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{Value, json};
use ys_core::ToolResult;
use ys_protocol::BoundarySource;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec, append_notices};

use crate::backends::{kind_slug, parse, plan};
use crate::net::{FetchError, fetch_capped};
use crate::{
    Backend, ENV_BOCHA_KEY, ENV_DEEPSEEK_KEY, ENV_DEEPSEEK_KEY_ALIAS, ENV_FIRECRAWL_KEY,
    ENV_SEARXNG_URL, ENV_TAVILY_KEY, ENV_TAVILY_KEY_ALIAS, Kind, SearchHit, WebConfig,
};

/// 整条链的**总预算**（设计 §1.7）。5 家各 60s = 最坏 5 分钟，而 `-p` / `--json`
/// 下没有中止通道（`boundary` 恒为 `None`）→ 超时是唯一的刹车。
pub const CHAIN_BUDGET: Duration = Duration::from_secs(30);

/// 单个后端的**硬下界**（设计 §1.7）：`remaining < MIN_SLICE` 就**不试这一家**。
///
/// 「要么至少 8s，要么不试」比「给 2s 然后必然超时」**更可预测**：抬高下界之后，
/// 「被尝试的后端至少 8s」成了**确定下界**，不确定性被压到「是否被尝试」这一格。
pub const MIN_SLICE: Duration = Duration::from_secs(8);

/// 单条 snippet 的**字符**上限（设计 §1.2；按字符截，不切碎 UTF-8）。
pub const SNIPPET_MAX_CHARS: usize = 300;

/// 一次响应体的字节上限。见模块文档「响应体上限」。
const RESPONSE_CAP: usize = 2 * 1024 * 1024;

/// 未给 `limit` 时的条数（设计 §4）。下限/上限由 [`MIN_LIMIT`] / [`MAX_LIMIT`] 管。
///
/// [`MIN_LIMIT`]: crate::MIN_LIMIT
/// [`MAX_LIMIT`]: crate::MAX_LIMIT
const DEFAULT_LIMIT: usize = 10;

pub struct WebSearchTool {
    /// **注入值**（不是全局单例）：`offline()` / `with_client()` 的配置下工具照常注册，
    /// 降级发生在**调用期**（`client` 为 `None` 或链为空 → 明确的错误结果）。
    cfg: WebConfig,
}

impl WebSearchTool {
    pub fn new(cfg: WebConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "websearch",
            "Search the web. Tries the configured backends in a fixed order and stops at the \
             first one that answers: an empty result set stops the chain (the query really has no \
             results), while errors, timeouts and rate limits fall through to the next backend. \
             Returns at most `limit` results (default 10, max 20), each rendered as title / URL / \
             snippet, with snippets cut to 300 chars and the whole output capped at 2000 lines / \
             50KB. When the answer comes from a backend other than the first one configured, the \
             output says which one (e.g. `[via searxng]`) and why the earlier backends were \
             skipped. If every backend fails, the result is an error listing each backend and why \
             it failed. The whole chain shares a 30s \
             budget, so a slow backend is abandoned after about 8s.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "number", "description": "Maximum number of results to return (default: 10, max: 20)" }
                },
                "required": ["query"]
            }),
        )
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        // `?` 只用于**参数**缺失（模型输入问题，属 `ToolError`）；其余一切降级成结果。
        let query = input["query"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidInput("missing 'query'".into()))?;
        // `limit` 至少为 1（0 会让「还有更多」的 notice 恒真且毫无意义）。
        let limit = (input["limit"].as_u64().unwrap_or(DEFAULT_LIMIT as u64) as usize)
            .clamp(crate::MIN_LIMIT, crate::MAX_LIMIT);

        // ① HTTP 栈不可用：**调用期**的明确错误（工具恒注册，见 `WebConfig::offline`）。
        let Some(client) = self.cfg.client() else {
            return Ok(error_result(
                "Web search is unavailable: the HTTP client could not be initialised \
                 (no usable TLS backend or networking disabled). Tell the user instead of retrying."
                    .to_string(),
            ));
        };
        // ② 链为空：把「下一步配什么」直接写进结果（零配置用户的第一份反馈）。
        if self.cfg.backends().is_empty() {
            return Ok(error_result(format!(
                "Web search is unavailable: no search backend is configured. Set {ENV_SEARXNG_URL} \
                 to a SearXNG instance address, or one of {ENV_DEEPSEEK_KEY} (or the generic \
                 {ENV_DEEPSEEK_KEY_ALIAS}) / {ENV_TAVILY_KEY} (or the generic \
                 {ENV_TAVILY_KEY_ALIAS}) / {ENV_BOCHA_KEY} / {ENV_FIRECRAWL_KEY} to an API key."
            )));
        }

        let outcome = run_chain(
            client,
            self.cfg.backends(),
            query,
            limit,
            CHAIN_BUDGET,
            ctx.boundary,
        )
        .await;
        Ok(render_chain_outcome(outcome, query, limit))
    }
}

/// 切片规则（设计 §1.7）—— **纯函数**，切片语义的全部内容都在这里。
///
/// ```text
/// remaining < MIN_SLICE → None（不试这一家，记 "budget exhausted"）
/// 否则 slice = min(remaining, max(MIN_SLICE, remaining / backends_left))
/// ```
///
/// **性质（可断言）**：`Some(slice)` 时恒有 `slice >= MIN_SLICE` —— 「被尝试的后端
/// 至少拿到 8s」是硬下界，而不是「看前几家跑得多快」。`backends_left == 0` 也返回
/// `None`（除零在这里就被挡住，调用方不必先判）。
pub fn slice_for(remaining: Duration, backends_left: usize) -> Option<Duration> {
    if backends_left == 0 || remaining < MIN_SLICE {
        return None;
    }
    // 均分：前几家快速失败时，后面的自动多拿（保留弹性）。
    let share = remaining / backends_left as u32;
    Some(remaining.min(share.max(MIN_SLICE)))
}

/// 链的终局（设计 §1.2 的三个出口 + 一个中止出口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainOutcome {
    /// 有后端答了（`hits` 可能为空 —— 那就是「真没结果」，链**停**在这里）。
    Answered {
        backend: Kind,
        /// 该后端在链上的**下标**（0 = 第一顺位）。非 0 时结果里要说 `[via x]`。
        index: usize,
        hits: Vec<SearchHit>,
        /// **胜出之前**那些被试过、失败的后端的理由（与链同序）。空 = 第一顺位就答了。
        ///
        /// 非空时必须进 notice（ADR-0016「降级路径行为更宽」的缓解措施）：同一次查询在
        /// 不同机器/时刻可能落到不同后端，理由是模型与用户判断「结果为何不同 / 第一家是不是
        /// 常年不可用」的**唯一线索**。丢了它就只剩一个 `[via x]`，看不出降级发生过。
        reasons: Vec<String>,
    },
    /// 全败 / 预算耗尽：**逐条理由，与链同序**。
    Failed { reasons: Vec<String> },
    /// 用户中止（`Boundary::Abort`）—— 不是失败，且**立刻停链**。
    Aborted,
}

/// 驱动整条链。
///
/// `budget` 是**参数**而非常量（见模块文档）：`call()` 传 [`CHAIN_BUDGET`]，
/// 测试传 `400ms` 就能断言「一个后端都不试、总耗时 <1s」。
///
/// 本函数**只分类、不渲染** —— 文案全部由 [`render_chain_outcome`] 拼，两者可分别断言。
pub async fn run_chain(
    client: &Client,
    backends: &[Backend],
    query: &str,
    limit: usize,
    budget: Duration,
    boundary: Option<&dyn BoundarySource>,
) -> ChainOutcome {
    let deadline = Instant::now() + budget;
    let mut reasons: Vec<String> = Vec::new();

    for (index, backend) in backends.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(slice) = slice_for(remaining, backends.len() - index) else {
            // 预算不足以给这一家「至少 8s」→ 不试。**剩下的每一家都要留痕**（否则
            // 「逐条列理由」会缺项，模型看到的是一个不完整的链）。
            for skipped in &backends[index..] {
                reasons.push(format!(
                    "{}: not tried — budget exhausted ({}s left, minimum slice {}s)",
                    kind_slug(skipped.kind),
                    remaining.as_secs(),
                    MIN_SLICE.as_secs()
                ));
            }
            break;
        };

        match attempt(client, backend, query, limit, slice, boundary).await {
            // **有结果与空结果都停**：空结果 = 真没结果（设计 §1.2 的核心区分），
            // 在剩下几家上各跑一遍是白等；`hits` 为空这件事由渲染层如实说出来。
            Ok(hits) => {
                return ChainOutcome::Answered {
                    backend: backend.kind,
                    index,
                    hits,
                    // 胜出之前那些理由**跟着终局走**：渲染层要靠它说清「为什么不是第一家」。
                    reasons: std::mem::take(&mut reasons),
                };
            }
            // 取消 → **立刻停链**，且不是失败。
            Err(AttemptError::Net(FetchError::Aborted)) => return ChainOutcome::Aborted,
            // 其余（网络 / 超时 / 非 2xx / 解析失败 / 超限）→ 记理由，**继续**下一家。
            Err(why) => reasons.push(format!("{}: {why}", kind_slug(backend.kind))),
        }
    }

    ChainOutcome::Failed { reasons }
}

/// 一次后端尝试的失败分类。
///
/// 与 `net::FetchError` 分开是为了**归属清楚**：前者的文案是网络层的事实，后者两条
/// （`Plan` / `Parse` / `TooLarge`）是搜索层的事实。混合成一个类型会让「请求没构造出来」
/// 被渲染成「网络错误」（误导）。
#[derive(Debug)]
enum AttemptError {
    /// `plan` 拒绝构造请求（地址非法 / 缺 key）。
    Plan(String),
    /// 网络族失败，原样透传 `FetchError` 的分类与文案。
    Net(FetchError),
    /// 响应不是合法 JSON / 缺结果数组 / 业务错误码（`parse` 的 `Err`）。
    Parse(String),
    /// 响应体撞上 [`RESPONSE_CAP`]：截断的 JSON 必然解析失败，故单列一条理由。
    TooLarge { cap: usize },
}

impl std::fmt::Display for AttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(why) => write!(f, "request could not be built: {why}"),
            Self::Net(e) => write!(f, "{e}"),
            Self::Parse(why) => write!(f, "{why}"),
            Self::TooLarge { cap } => write!(
                f,
                "response exceeded the {}MiB cap (truncated JSON cannot be parsed)",
                cap / (1024 * 1024)
            ),
        }
    }
}

/// 一次尝试：`plan → fetch(.timeout(slice)) → parse`。
///
/// **`ctx.boundary` 透传给 `fetch_capped`**（TUI 里 Ctrl+C 能中止挂起的请求）。
async fn attempt(
    client: &Client,
    backend: &Backend,
    query: &str,
    limit: usize,
    slice: Duration,
    boundary: Option<&dyn BoundarySource>,
) -> Result<Vec<SearchHit>, AttemptError> {
    let req = plan(client, backend, query, limit).map_err(AttemptError::Plan)?;
    // `.timeout(slice)` 与 `fetch_capped` 的 `budget` 都给同一个值：前者兜住「建连之后
    // 卡在某个 await」，后者兜住整个 future（含 `send`）。两者都是 `TimedOut`，不冲突。
    let fetched = fetch_capped(req.timeout(slice), RESPONSE_CAP, slice, boundary)
        .await
        .map_err(AttemptError::Net)?;

    if fetched.capped {
        return Err(AttemptError::TooLarge { cap: RESPONSE_CAP });
    }

    // JSON 按规范是 UTF-8（RFC 8259）；坏字节不 panic（宽松解码），解析失败会变成理由。
    let body = String::from_utf8_lossy(&fetched.body);
    parse(backend.kind, fetched.content_type.as_deref(), &body).map_err(AttemptError::Parse)
}

/// 链终局 → `ToolResult`。
///
/// 渲染规则（`is_error` 的判据是 ADR-0001，不是「有没有拿到东西」）：
///
/// | 终局 | `is_error` | 为什么 |
/// |---|---|---|
/// | 有结果 / 空结果 | `false` | 工具正常工作；「没搜到」是**答案**，不是故障 |
/// | 全败 | `true` | **模型可见**（逐条理由可据以改变行为：换词、换思路、直接 fetch） |
/// | 中止 | `false` | 用户取消是正常收场（与 `grep` 被我们自己杀掉同款） |
///
/// 胜出路径的 notice 有两层可见性：非第一顺位 → `[via x]`；胜出前有失败者 →
/// `[fell back after: …]`（逐条、与链同序）。**后者不是噪音**：它是「这家是不是常年
/// 不可用」的唯一线索，缺了它 `[via x]` 只说明「换了家」，不说明「为什么换」。
pub fn render_chain_outcome(outcome: ChainOutcome, query: &str, limit: usize) -> ToolResult {
    match outcome {
        ChainOutcome::Aborted => ToolResult {
            content: "Search aborted by user.".into(),
            is_error: false,
        },
        ChainOutcome::Failed { reasons } => {
            let listed: Vec<String> = reasons.iter().map(|r| format!("- {r}")).collect();
            ToolResult {
                content: format!(
                    "All search backends failed for query `{query}`:\n{}",
                    listed.join("\n")
                ),
                is_error: true,
            }
        }
        ChainOutcome::Answered {
            backend,
            index,
            hits,
            reasons,
        } => {
            let mut notices: Vec<String> = Vec::new();
            // 可见性（设计 §1.8①）：**用了非第一顺位就明说** —— 结果质量差异的归因信号。
            if index > 0 {
                notices.push(format!("via {}", kind_slug(backend)));
            }
            // 降级理由**逐条**进 notice（ADR-0016 对「降级路径行为更宽」的缓解）。
            // 空白守卫是冗余的（`index > 0` ⇒ 必有一条理由），但把它写出来更稳：
            // 将来若允许「跳过而不记理由」，这里不会凭空产出一条空 notice。
            if !reasons.is_empty() {
                notices.push(format!("fell back after: {}", reasons.join("; ")));
            }

            if hits.is_empty() {
                let mut content = format!("No results found for query `{query}`.");
                append_notices(&mut content, &notices);
                return ToolResult {
                    content,
                    is_error: false,
                };
            }

            // 去重（保序）→ 截 limit。
            let deduped = dedupe_by_url(hits);
            let total = deduped.len();
            if total > limit {
                notices.push(format!(
                    "{limit} of {total} results shown (limit reached; raise `limit` for more)"
                ));
            }

            let (text, snippets_cut) = render_hits(&deduped[..total.min(limit)]);
            if snippets_cut {
                notices.push(format!(
                    "Some snippets truncated to {SNIPPET_MAX_CHARS} chars"
                ));
            }

            // 整体过 `head`（有界）：行数/字节上限与 read / grep / ls 同一份实现。
            let head = ys_tool::head(&text, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
            if head.hit.is_some() {
                notices.push(format!(
                    "Truncated {} lines, showing first {}",
                    head.total_lines - head.shown_lines,
                    head.shown_lines
                ));
            }

            let mut content = head.content;
            append_notices(&mut content, &notices);
            ToolResult {
                content,
                is_error: false,
            }
        }
    }
}

/// 按 URL 去重，**保序**：保留每条 URL 的**首次**出现（后面的同名条目丢弃）。
///
/// 按**原样字符串**比较（不做大小写归一 / 不剥锚点）：mapper 层已经把空 URL 剔掉了，
/// 而「两条 URL 是不是同一个页面」是个需要抓取才能回答的问题 —— 这里只做廉价的
/// 精确去重，宁可留两个近似条目，也不要因为归一规则出错而**丢掉唯一的那条**。
fn dedupe_by_url(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut seen: HashSet<String> = HashSet::with_capacity(hits.len());
    let mut out: Vec<SearchHit> = Vec::with_capacity(hits.len());
    for hit in hits {
        if seen.insert(hit.url.clone()) {
            out.push(hit);
        }
    }
    out
}

/// 渲染命中：每条 = `title / url / snippet` 三行（空字段整行省略），条目间空一行。
///
/// 返回 `(文本, 是否有 snippet 被截)`。snippet 先**折叠成单行**再按字符截断 ——
/// 否则一段带换行的 HTML 片段会把条目切成看起来像两条的东西，而 300 字符的预算
/// 也会先被空白吃掉一大截。
fn render_hits(hits: &[SearchHit]) -> (String, bool) {
    let mut blocks: Vec<String> = Vec::with_capacity(hits.len());
    let mut cut_any = false;

    for hit in hits {
        let mut lines: Vec<String> = Vec::new();
        let title = one_line(&hit.title);
        if !title.is_empty() {
            lines.push(title);
        }
        lines.push(one_line(&hit.url));
        // 先折叠成单行再截断（`one_line` 的返回必须落进 `let`：`line()` 借用它）。
        let snippet_line = one_line(&hit.snippet);
        let (snippet, cut) = ys_tool::line(&snippet_line, SNIPPET_MAX_CHARS);
        cut_any |= cut;
        if !snippet.is_empty() {
            lines.push(snippet.into_owned());
        }
        blocks.push(lines.join("\n"));
    }

    (blocks.join("\n\n"), cut_any)
}

/// 折叠成单行：连续空白（含换行 / 制表 / 不换行空格）压成一个空格，两端去空白。
///
/// 用 `split_whitespace` 而非 `replace(['\n','\r'], " ")`：HTML 抓来的片段里常有一串
/// 缩进与 `\u{a0}`，只换换行符会在标题中间留下「五个空格」这类噪音。
fn one_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 模型可见的错误结果（与 `tools-basic` 同款的小助手）。
fn error_result(content: String) -> ToolResult {
    ToolResult {
        content,
        is_error: true,
    }
}
