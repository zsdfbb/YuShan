//! 后端链的**唯一真值**：一张常量表 + 两个纯函数。
//!
//! # 为什么是表 + 两个 `match`，而不是 `SearchBackend` trait
//!
//! 四家的差异只有 6 个轴 —— 方法 / URL / 鉴权头 / 请求体 / 响应路径 / 字段名 ——
//! **全是数据**，不是算法。一张 `const TABLE` + 两个**穷尽** `match` 就够了，且
//! 将来加一家时漏改某个臂是**编译错误**，不会静默漂移（设计 §1.3 / §3 裁决一）。
//!
//! **触发条件**（再考虑 trait）：出现需要签名（HMAC / AK-SK）或 mapper 需持状态
//! （token 缓存）的后端 —— 那时差异从「数据的形状」变成「算法的形状」。
//!
//! # 一张表的三种读法
//!
//! | 读法 | 谁读 | 用来干什么 |
//! |---|---|---|
//! | [`TABLE`] 的**顺序** | [`resolve_backends`] | 链序：`已配 key 的 provider（tavily → bocha → firecrawl）→ searxng` |
//! | `Row.endpoint` / `endpoint_env` | [`resolve_backends`] + [`plan`] | 请求地址（SearXNG 的地址天然是用户配置） |
//! | `Row.key_env` / `keyless` | [`resolve_backends`] | 未配 key 的行在**构造期**就被剔除 |
//!
//! # `plan` 为什么多一个 `&Client` 参数（与设计 §1.3 的签名有出入）
//!
//! 设计写的是 `fn plan(b: &Backend, q: &str, limit: usize) -> Result<RequestBuilder, String>`。
//! `reqwest::RequestBuilder` **无法脱离 `Client` 构造**（`Client::request` 是它唯一的
//! 公开入口，没有 `new`）。三条出路里选第三条：
//!
//! 1. 在 `plan` 里 `Client::new()` —— 每次调用新建连接池，白白丢掉 keep-alive；
//! 2. 返回自定义的 `PlannedRequest` 结构 —— 但 [`crate::fetch_capped`] 收的是
//!    `RequestBuilder`，等于把「怎么构造请求」再拆成两处；
//! 3. **把 `&Client` 透传进来**（现在的选择）—— 调用点（`search.rs`）本来就从
//!    `WebConfig` 拿 client，多传一个参数不增加任何概念。
//!
//! `plan` 依旧是**纯函数**：只拼 URL / 头 / 体，**不 send**、不碰网络 ——
//! fixture 测试可以直接对它的产物（URL、请求体）做断言。

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Client, RequestBuilder, Url};
use serde_json::{Value, json};

use crate::{Backend, ENV_BOCHA_KEY, ENV_FIRECRAWL_KEY, ENV_SEARXNG_URL, ENV_TAVILY_KEY, Kind};

// ---------- 常量：端点与上限 ----------

/// Tavily 服务根（`plan` 追加 `/search`）。
const TAVILY_ROOT: &str = "https://api.tavily.com";
/// 博查服务根（`plan` 追加 `/v1/web-search`）。
const BOCHA_ROOT: &str = "https://api.bochaai.com";
/// Firecrawl 服务根（`plan` 追加 `/v2/search`）。
const FIRECRAWL_ROOT: &str = "https://api.firecrawl.dev";

/// `limit` 的**契约上限**（设计 §4「搜索 `limit` 默认 10（clamp 1..=20）」）。
///
/// `plan` 再做一次 clamp 而不是只靠工具参数层：各家对上限的容忍度不同
/// （Tavily 文档写 1–20），越界会被对端判成 400 —— 那是**我们可以避免的**一次失败。
pub const MAX_LIMIT: usize = 20;
/// `limit` 的下限。`0` 会让部分后端直接报错，而「要 0 条结果」也不是有意义的请求。
pub const MIN_LIMIT: usize = 1;

// ---------- 表 ----------

/// 后端链的一行 —— 一家的**全部静态差异**。
///
/// 字段刻意窄：任何「只有某一家需要」的旋钮都不进来（先例 `tools-basic::SearchBackends`
/// 的 `unavailable_reason`：有触发条件才加）。
struct Row {
    kind: Kind,
    /// 服务**根**地址常量；`plan` 追加各自路径（见 [`request_url`]）。
    ///
    /// SearXNG 这里是空串 —— 它**没有**可用的常量端点：把查询默认发给某个陌生人的
    /// 公共实例既不可靠也不礼貌（对方一改配置、一加验证码，我们就静默变了行为），
    /// 故它必须由 [`Row::endpoint_env`] 提供。
    endpoint: &'static str,
    /// 端点由哪个 env 提供 —— **只有 SearXNG 有**（自建实例地址天然是用户配置）。
    /// 该 env 缺席 ⇒ 这一行从链上剔除。
    endpoint_env: Option<&'static str>,
    /// 密钥所在的 env；`None` = 这家永不读密钥。
    key_env: Option<&'static str>,
    /// **没有密钥时是否仍留在链上**。
    ///
    /// - `false`（tavily / bocha / firecrawl）：没配 key = 用户没打算用这一家 → **构造期**剔除；
    /// - `true`（**只剩 searxng**）：这一家**本来就没有密钥这个概念** —— 它的「凭证」是
    ///   实例地址（由 [`Row::endpoint_env`] 保证，地址缺席时走另一条剔除路径）。
    ///
    /// ⚠️ 这个 `true` 是**给「本来就不必带密钥」的后端**的，**不是**给「我们猜它可能免 key」
    /// 的后端。踩过的坑：初稿据一篇第三方对比文把 Firecrawl 标成 `keyless: true`（大概率是把
    /// 「免费额度」误读成「免 key」），而官方 API 文档明写认证**必须**带 `Authorization`
    /// （错误码表：「401 | The API key was not provided」）。后果不是「多试一次」这么轻：**零配置
    /// 用户的链因此非空**（只剩 firecrawl），`degraded_hint` 里那句「未配置任何搜索后端」
    /// **在 `from_env` 路径上不可达** —— 用户一句提示都没有，首次 `websearch` 撞上一个**他从没
    /// 选过**的厂商的 401，且链上无后备。这**正面违反 ADR-0016**「Firecrawl keyless 绝不能是
    /// 唯一路径」。**修订（2026-09-18）**：改为需 key，零配置链恢复为空。
    keyless: bool,
}

/// 后端链的**唯一真值**。**顺序即链序**。
///
/// ```text
/// 已配 key 的 provider（tavily → bocha → firecrawl） → searxng
/// ```
///
/// # 为什么是这个序（设计 §1.8①，2026-09-18 修订）
///
/// 分界线是「**用户有没有显式配置**」，不是「免费 / 收费」：
///
/// - **前三家都是托管搜索 API**（用户配 key ⇒ 想要那一家的质量），且**返回的就是搜索结果
///   本身** —— 一家一问，没有中间层；
/// - **searxng 是元搜索**：它自己还要再转发给上游引擎（`unresponsive_engines` 是**部分引擎
///   挂掉**的常态，见自家 fixture），多一跳、结果质量取决于该实例开了哪些引擎与有没有被上游
///   限流，**我们无法评估** → 排最后。
///
/// ⚠️ **旧理由已失效**：初稿写的是「用户自建的 SearXNG 比第三方**免费额度**可靠，故 searxng
/// 排在 firecrawl 之前」。Firecrawl 转入「需 key 的托管服务」组后**不再是免费额度**，那条比较
/// 的另一个对象消失了 —— 留 searxng 在它前面就只是「自建比收费可靠」这一条**未经验证**的断言，
/// 且与「配了 key 说明用户想要那一家的质量」自相矛盾（同一理由会让 searxng 该排 tavily 前面）。
/// 故 searxng 退到**末位**。
///
/// **零配置用户的链与旧稿一致地「什么都不猜」**：无地址的 searxng 与无 key 的三家**全被剔除**
/// ⇒ 链为空 ⇒ [`crate::WebConfig::degraded_hint`] 明确报「未配置任何搜索后端」。
///
/// 顺序**完全由这张表决定** —— 不按延迟、不按成功率、不随机、不做并行竞速。
///
/// ⚠️ 诚实边界：表能定死**尝试顺序**，定不死「**谁成功**」（网络随机）。目标是让
/// 不确定性**留在外部**并**可见**（`search.rs` 用 `[via x]` 明示、全败时逐条列理由）。
const TABLE: &[Row] = &[
    Row {
        kind: Kind::Tavily,
        endpoint: TAVILY_ROOT,
        endpoint_env: None,
        key_env: Some(ENV_TAVILY_KEY),
        keyless: false,
    },
    Row {
        kind: Kind::Bocha,
        endpoint: BOCHA_ROOT,
        endpoint_env: None,
        key_env: Some(ENV_BOCHA_KEY),
        keyless: false,
    },
    Row {
        kind: Kind::Firecrawl,
        endpoint: FIRECRAWL_ROOT,
        endpoint_env: None,
        key_env: Some(ENV_FIRECRAWL_KEY),
        // 官方 API 要求 `Authorization: Bearer`（无 key 回 401 "The API key was not provided"）
        // ⇒ 与 tavily / bocha 同组：没配 key 就在构造期剔除（见 `Row::keyless` 那段血泪）。
        keyless: false,
    },
    Row {
        kind: Kind::Searxng,
        endpoint: "",
        endpoint_env: Some(ENV_SEARXNG_URL),
        key_env: None,
        // searxng 不需要密钥；它的「凭证」是实例地址，缺席时上面那条 endpoint 分支就剔了它。
        keyless: true,
    },
];

/// 把 env 取值规整成「配了 / 没配」。
///
/// **空串与纯空白都算没配**：`export KEY=`、`KEY=" "`、`YUSHAN_SEARCH_SEARXNG_URL=`
/// 在 shell 里都是常见事故，若算「配了」，链上就会出现一个**必然失败**的后端
/// （每次调用白等一个 8s 切片，且理由里写着「网络错误」而不是真正的原因）。
///
/// 同理，**首尾空白一律去掉**：密钥不该有空格，而粘贴时多带一个空格是常事。
fn non_empty(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 由 [`TABLE`] + env 解析出一条**冻结的链**。
///
/// 「冻结」= 未配 key 的行**在这一步**就被剔除，调用期不再有「这个后端能不能用」的
/// 分支（设计 §1.3：链确定的载体）。于是 `search.rs` 里读不到任何 env。
///
/// # 为什么收一个取值函数而不是直接读 `std::env`
///
/// 进程级 env 的读写与并行测试是**数据竞争**（本仓库已因此出过血，见 `lib.rs`
/// 测试模块的 `EnvRestore`）。收 `impl Fn(&str) -> Option<String>` 后，
/// **解析规则可以被确定性地测**（传一个 `HashMap` 查找即可），不必改进程 env、
/// 也不必赌初始化顺序 —— 与「`WebConfig` 是注入值而非单例」是同一个理由。
/// 生产路径传 `|k| std::env::var(k).ok()`（见 [`crate::WebConfig::from_env`]）。
///
/// # 一条**推论**：零配置 ⇒ 这条链**为空**
///
/// 四行**都可能被剔除**：tavily / bocha / firecrawl 要 key，searxng 要地址。于是
/// 「**一个 `YUSHAN_SEARCH_*` 都没配**」得到一个空链，[`crate::WebConfig::degraded_hint`]
/// 里那句「未配置任何搜索后端」因此**在 `from_env` 路径上可达** —— 零配置的用户
/// 一定能看到一句提示，而不是带着一条**自己没选过**的链去撞 401（见 `Row::keyless` 的修订记录）。
pub fn resolve_backends(env: impl Fn(&str) -> Option<String>) -> Vec<Backend> {
    let mut chain: Vec<Backend> = Vec::with_capacity(TABLE.len());

    for row in TABLE {
        // ① 端点：有的行端点必须来自 env，缺席即整行剔除。
        let endpoint = match row.endpoint_env {
            Some(key) => match non_empty(env(key)) {
                Some(value) => value,
                None => continue,
            },
            None => row.endpoint.to_string(),
        };

        // ② 密钥：`None`（key_env 缺席）= 这家不读密钥。
        let key = row.key_env.and_then(|k| non_empty(env(k)));
        if row.key_env.is_some() && key.is_none() && !row.keyless {
            continue;
        }

        chain.push(Backend {
            kind: row.kind,
            endpoint,
            key,
        });
    }

    chain
}

// ---------- 请求构造 ----------

/// 后端的**稳定小写标识**：`[via x]` 标签、fixture 文件名、错误理由三处共用一份真值。
///
/// 不用 `Debug` 的 `{:?}`：那是**给开发者看的**（拼写随重构而变），而这三处的读者是
/// 用户 / 模型 / 采集脚本，`Searxng` 与 `searxng` 混用会让 fixture 文件名两处对不上。
pub fn kind_slug(kind: Kind) -> &'static str {
    match kind {
        Kind::Searxng => "searxng",
        Kind::Tavily => "tavily",
        Kind::Bocha => "bocha",
        Kind::Firecrawl => "firecrawl",
    }
}

/// 拼 `根 + 路径`：解析成 [`Url`] 后**改路径、清查询串与片段**。
///
/// # 为什么要解析而不是纯字符串拼接（T9 review P2：一个真出过的静默错查询）
///
/// 容错的对象是**真实的粘贴事故**：`YUSHAN_SEARCH_SEARXNG_URL` 的语义是「实例地址」，
/// 而用户完全可能把浏览器里那条 `…/search?q=rust&format=html` 的地址整条贴进来。
///
/// 纯字符串拼接只认「裸 `/search` 后缀」，于是那条地址会拼成
/// `https://searx.be/search?q=rust/search&q=rust+error+handling&format=json`：
/// **路径仍然对**、`format=json` 也在，`q` 却出现两次 —— 而 Flask/Werkzeug 的
/// `args.get("q")` 取**第一个** ⇒ 搜索跑在**用户那次浏览的旧查询词**上，状态码 200、
/// 结果看着正常。这是「静默给错答案」，比一个 404 **危险得多**（404 至少会被看见）。
///
/// 故这里的规则是：**路径里已经带了要追加的那段就不重复追加**（去掉尾斜杠后再比一次）；
/// **查询串与片段一律清掉** —— 它们是那次浏览的残留、不是配置，`plan` 会自己拼出正确的
/// `q` 与 `format=json`。同理，实例挂在子路径下（`https://example.com/searxng/`）也照常工作，
/// 因为路径前缀**原样保留**，只做「是否已带后缀」的判断。
///
/// 解析失败（空串 / 相对地址）时退回朴素拼接：这里**不报错、不 panic** ——
/// 错误留给下游（`plan` 的 searxng 分支会报「实例地址非法」），那里知道 `kind`、文案更准。
fn join(root: &str, path: &str) -> String {
    let Ok(mut url) = Url::parse(root.trim()) else {
        return format!("{}{path}", root.trim_end_matches('/'));
    };

    let trimmed = url.path().trim_end_matches('/');
    let with_suffix = if trimmed.ends_with(path) {
        trimmed.to_string()
    } else {
        format!("{trimmed}{path}")
    };
    // 只在真的变了时才 `set_path`（它会对不合法字符做百分号编码，而没变时不该被动过）。
    if with_suffix != url.path() {
        url.set_path(&with_suffix);
    }

    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// 请求地址（`plan` 与采集脚本共用的一份真值）。
fn request_url(b: &Backend) -> String {
    match b.kind {
        // SearXNG 的 API 路径固定是 `/search`（自建实例常在 http://127.0.0.1:8888）。
        Kind::Searxng => join(&b.endpoint, "/search"),
        Kind::Tavily => join(&b.endpoint, "/search"),
        Kind::Bocha => join(&b.endpoint, "/v1/web-search"),
        Kind::Firecrawl => join(&b.endpoint, "/v2/search"),
    }
}

/// 需要密钥的那几家的统一检查。
///
/// [`resolve_backends`] 已把「没 key 的」剔掉了，所以这里走到 `None` 只可能是
/// **手工构造的 `Backend`**（测试、将来的其它装配路径）。给出可执行的文案而不是 panic。
fn require_key(b: &Backend) -> Result<&str, String> {
    b.key.as_deref().ok_or_else(|| {
        format!(
            "{} 需要 API key（{} 未配置或为空）",
            kind_slug(b.kind),
            key_env_of(b.kind).unwrap_or("<无>")
        )
    })
}

/// 这家读哪个 env 取密钥；`None` = 不读（**只有 SearXNG** —— 它的凭证是实例地址）。
///
/// 与 [`TABLE`] 同源：`plan` 的错误文案要能**告诉用户改哪个变量**，而那是表的事实。
fn key_env_of(kind: Kind) -> Option<&'static str> {
    TABLE
        .iter()
        .find(|row| row.kind == kind)
        .and_then(|row| row.key_env)
}

/// `limit` 的 clamp（见 [`MIN_LIMIT`] / [`MAX_LIMIT`]）。
fn clamp_limit(limit: usize) -> usize {
    limit.clamp(MIN_LIMIT, MAX_LIMIT)
}

/// 统一加 `Authorization: Bearer`。
///
/// 用 `HeaderValue::from_str` 显式校验，而不是 `RequestBuilder::bearer_auth`：后者把
/// 「key 里有非法字符」推迟到 `send()` 才报（reqwest 的 builder 会先存下错误），
/// 那时的错误是笼统的 `builder error`，**看不出是哪一家的 key 脏了**。
/// [`plan`] 的契约是「构造期返回 `Err`」，而只有这一层知道 `kind`，归属能写准。
fn bearer(req: RequestBuilder, key: &str) -> Result<RequestBuilder, String> {
    let value = HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| "API key 含非法 HTTP header 字符（换行 / 非 ASCII）".to_string())?;
    Ok(req.header(AUTHORIZATION, value))
}

/// 按 `kind` **穷尽分派**构造请求。**纯函数**：不 send、不碰网络。
///
/// 各家的形状（字段名以各自官方文档为准；「未核」标注见下表）：
///
/// | backend | 方法 | 路径 | 鉴权 | 请求体 |
/// |---|---|---|---|---|
/// | SearXNG | GET | `/search?q=&format=json` | 无 | — |
/// | Tavily | POST | `/search` | `Authorization: Bearer` | `{query, max_results, search_depth}` |
/// | Bocha | POST | `/v1/web-search` | `Authorization: Bearer` | `{query, count, summary}` |
/// | Firecrawl | POST | `/v2/search` | `Authorization: Bearer`（**必需**） | `{query, limit}` |
///
/// 三处**有意为之**的取舍：
///
/// - **SearXNG 没有 `limit` 参数**（API 只有 `q/categories/engines/language/pageno/
///   time_range/format/safesearch`），故这一家**忽略 `limit`** —— 裁剪由 `search.rs`
///   在客户端侧做（各家返回条数本就不可控，统一在那里收拾）。
/// - **SearXNG 不带 `language` / `safesearch`**：两者都有服务端默认值，而我们没有
///   任何理由去覆盖自建实例主人的选择。
/// - **Bocha 传 `summary: false`**：`true` 会额外让对端生成一段 AI 摘要（更慢、更贵），
///   而我们的映射只取短片段（`snippet`）—— 花了钱却丢掉，不如不要。
pub fn plan(
    client: &Client,
    b: &Backend,
    query: &str,
    limit: usize,
) -> Result<RequestBuilder, String> {
    let limit = clamp_limit(limit);
    let url = request_url(b);

    match b.kind {
        Kind::Searxng => {
            let mut url = Url::parse(&url).map_err(|e| {
                format!("searxng 实例地址非法：`{url}`（{e}）—— 检查 {ENV_SEARXNG_URL}")
            })?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("format", "json");
            Ok(client.get(url))
        }
        Kind::Tavily => bearer(
            client.post(url).json(&json!({
                "query": query,
                "max_results": limit,
                "search_depth": "basic",
            })),
            require_key(b)?,
        ),
        Kind::Bocha => bearer(
            client.post(url).json(&json!({
                "query": query,
                "count": limit,
                "summary": false,
            })),
            require_key(b)?,
        ),
        Kind::Firecrawl => bearer(
            client.post(url).json(&json!({
                "query": query,
                "limit": limit,
            })),
            // 官方 API 要求 `Authorization`：不带凭证只会换回 401（"The API key was not
            // provided"），而**这里**能给出「改哪个 env」的可执行文案。手工构造的 `Backend`
            // 若 key 为 `None`（测试 / 将来的其它装配路径）走的就是这条路 —— 不做「猜它可能
            // 免 key」的兜底：那个猜测正是 `Row::keyless` 记下的那次静默 401（T9 review P1）。
            require_key(b)?,
        ),
    }
}

// ---------- 响应映射 ----------

/// 一条搜索命中 —— 四家的响应**最终都收敛到这三个字段**。
///
/// 刻意只有这三个：`score` / `engine` / `datePublished` 之类各家不一（有的一家有一家
/// 没有），把它们塞进来就要解释「SearXNG 的 score 与 Tavily 的 score 能不能比」——
/// 而对模型来说，一条结果的可信度来自**它能不能被打开**，不来自一个跨家不可比的分数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 沿路径取值（`dig(&v, &["data","webPages","value"])`）。
///
/// `Value::get` 对非对象返回 `None`（不 panic），故整条链天然容错。
fn dig<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    Some(cur)
}

/// 取一个字符串字段：缺失 / `null` / 类型不对 → `None`（**绝不 panic、绝不猜**）。
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `code` 字段：接受数字，也接受数字**字符串**（部分网关会把 `200` 序列化成 `"200"`）。
fn code_of(v: &Value) -> Option<i64> {
    let raw = v.get("code")?;
    raw.as_i64()
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 在若干候选路径里找**第一个数组**。
///
/// 候选路径不止一条，是为了容纳同一家的多个 API 版本（Firecrawl v2 的 `data.web[]`
/// 与 v1 的 `data[]`；Bocha 带 `data` 信封与不带信封两种公开样例）。
///
/// ⚠️ **数组本身缺失 / 是 `null` / 不是数组 ⇒ `None`（→ 调用方 `Err`）**，**不是**
/// 「空结果」。这条区分是**链的语义**：`Err` 让链**继续**试下一家，而「空结果」会让链
/// **停**（设计 §1.2：「空结果 → 停（真没结果，不白跑后面的后端）」）。把形状不对
/// 当成空结果 = 因为一家改了个字段名，整条链就再也不会去试别人了。
fn take_array<'a>(v: &'a Value, paths: &[&[&str]]) -> Option<&'a [Value]> {
    paths
        .iter()
        .find_map(|path| dig(v, path).and_then(Value::as_array).map(Vec::as_slice))
}

/// 一组「结果对象」→ `Vec<SearchHit>`。
///
/// 容错规则（设计 R11 与 T10 的靶子）：
///
/// - **非对象元素**（`null` / 字符串 / 数字 / 数组）→ **跳过**；
/// - **`url` 为空**的条目 → **跳过**（没有可打开地址的东西对模型不是「结果」，
///   而 `search.rs` 的 `dedupe_by_url` 也依赖它非空）；
/// - `title` / `snippet` 缺失或类型不对 → **空串**（内容缺了但仍值得给模型一个链接）；
/// - **未知字段一律忽略**（不报错 —— 对端加字段不该让我们变红）。
fn map_hits(
    items: &[Value],
    title_key: &str,
    url_key: &str,
    snippet_keys: &[&str],
) -> Vec<SearchHit> {
    let mut hits = Vec::with_capacity(items.len());
    for item in items {
        if !item.is_object() {
            continue;
        }
        let Some(url) = str_field(item, url_key).filter(|u| !u.trim().is_empty()) else {
            continue;
        };
        let title = str_field(item, title_key).unwrap_or_default();
        // `snippet_keys` 是多选一：Bocha 的 `summary` 只在请求 `summary:true` 时才有，
        // 而我们传的是 `false` —— 留作兜底，真拿到就用。
        let snippet = snippet_keys
            .iter()
            .find_map(|key| str_field(item, key))
            .unwrap_or_default();
        hits.push(SearchHit {
            title,
            url,
            snippet,
        });
    }
    hits
}

/// 把错误信封压成一行可读的文案（尽量不把整个 JSON 塞进提示）。
fn brief(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(e) = str_field(v, "error") {
        return e;
    }
    if let Some(m) = str_field(v, "message") {
        return m;
    }
    v.to_string()
}

/// SearXNG 的 **JSON 前提检查**（设计 §1.5 的核心，也是本文件最重要的一条）。
///
/// SearXNG 的 JSON **默认关闭**：`search.formats` 里没有 `json` 时，它对着
/// `?format=json` 的请求回 **`200 + text/html`** —— 一次**静默失败**：状态码是成功，
/// 正文是完整的搜索页 HTML。若在这里放行，`serde_json` 会报「不是合法 JSON」，
/// 用户于是去查网络、查查询词、查实例存活 —— **全都不是原因**。
///
/// 故：**先查 `Content-Type`**，非 JSON 直接给出**可执行的修复指引**。
///
/// 接受 `application/json` 与 `+json` 后缀（`application/ld+json` 等）。对所有非
/// JSON 都回同一句「去开 `json`」是刻意的：**这句在绝大多数情况下就是真答案**，
/// 而按 MIME 分门别类地猜（`text/html` → 这个提示、`text/plain` → 那个提示）只会
/// 稀释那条最可能的修复路径。
fn ensure_json_ctype(ctype: Option<&str>) -> Result<(), String> {
    let essence = ctype
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence == "application/json" || essence.ends_with("+json") {
        return Ok(());
    }
    let seen = match ctype {
        Some(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => "<no Content-Type>".to_string(),
    };
    Err(format!(
        "searxng returned {seen} — enable `json` in search.formats (settings.yml)"
    ))
}

/// 响应映射。**纯函数**：只吃 `Kind` / `Content-Type` / **正文**，不碰网络、不碰文件。
///
/// 之所以是纯函数而不是 trait 方法：它是 fixture 测试的靶子（`tests/fixtures/*.json`
/// 直接喂进来），也是「加一家 = 加一行 + 两个 `match` 臂 + 三份 fixture」里那个
/// **唯一的算法落点**。
///
/// # 为什么 `parse` 不看 HTTP 状态码
///
/// 非 2xx 在 [`crate::fetch_capped`] 里就已经是 [`crate::FetchError::Status`] 了
/// （那一层**不读体**）。于是 `parse` 面对的永远是 2xx 正文 —— **但 2xx 也可能是失败**：
/// SearXNG 的静默失败（上面那条），以及各家把自己家的错误塞进 200 正文的形态
/// （Bocha 的 `code` / Firecrawl 的 `success` / Tavily 的 `detail`）。
/// 故这三家在下面各自**认领自己的错误字段** —— 它们是「正文里的事实」，
/// 不归状态码管。
///
/// # 错误文案是给**模型/用户**看的
///
/// 每一条 `Err` 都要能回答「下一步改什么」：少一个数组要说清是哪一条路径、
/// Bocha 的 `code` 要把 `msg` 带上、SearXNG 那条要直接说 `settings.yml`。
pub fn parse(kind: Kind, ctype: Option<&str>, body: &str) -> Result<Vec<SearchHit>, String> {
    // ⚠️ SearXNG 必须**先**查 ctype：正文是不是 JSON 都不能证明「JSON 是开着的」，
    // 反过来 HTML 一定能证明它没开。顺序写反就会先落到下面的 JSON 解析分支，
    // 于是静默失败变成一句误导性的「不是合法 JSON」。
    if kind == Kind::Searxng {
        ensure_json_ctype(ctype)?;
    }

    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("{} 返回的不是合法 JSON：{e}", kind_slug(kind)))?;

    match kind {
        Kind::Searxng => searxng_hits(&value),
        Kind::Tavily => tavily_hits(&value),
        Kind::Bocha => bocha_hits(&value),
        Kind::Firecrawl => firecrawl_hits(&value),
    }
}

/// SearXNG：`results[]{title, url, content}`。
///
/// **已核**（`docs.searxng.org/dev/search_api.html` 的响应字段表；正文路径由
/// 本项目自己的 docker 实例核对过）。
/// `unresponsive_engines` / `answers` / `infoboxes` 一律忽略：设计 R11 已指出
/// 「部分引擎挂」只有 SearXNG 有对应字段，把它抬成通用 notice 只会覆盖一家。
fn searxng_hits(v: &Value) -> Result<Vec<SearchHit>, String> {
    let items = take_array(v, &[&["results"]])
        .ok_or_else(|| "searxng 响应缺少 results 数组".to_string())?;
    Ok(map_hits(items, "title", "url", &["content"]))
}

/// Tavily：`results[]{title, url, content}`。
///
/// **已核**（官方文档与 SDK 示例一致：`response["results"]` 的 `title` / `url` /
/// `content`）。`answer` / `images` / `raw_content` 忽略。
///
/// 错误形态：Tavily 用 `detail` 报错（`{"detail": {"error": "..."}}`）。
/// **未经真实响应校验** —— 依据是 SDK 与社区报错的样例，官方文档没有给错误体的 schema。
/// 判据刻意宽松（只要没有 `results` 且出现 `detail` 就是错误），因为**多认出一个错误
/// 的代价（链继续试下一家）远小于漏认的代价（把错误当空结果，链提前停）**。
fn tavily_hits(v: &Value) -> Result<Vec<SearchHit>, String> {
    if let Some(items) = take_array(v, &[&["results"]]) {
        return Ok(map_hits(items, "title", "url", &["content"]));
    }
    if let Some(detail) = v.get("detail") {
        return Err(format!("tavily detail: {}", brief(detail)));
    }
    Err("tavily 响应缺少 results 数组".to_string())
}

/// Bocha：`data.webPages.value[]{name, url, snippet, summary}`。
///
/// 字段路径 **已核**（Bocha 官方组织在 GitHub 上的 `Bocha-Labs/bocha-skills`
/// 明写「搜索结果位于 `data.webPages.value[]`」，逐字段列出 `name` / `url` /
/// `snippet` / `summary` / `siteName` / `datePublished`）。开放平台首页与多篇第三方
/// 实测给出的则是**不带 `data` 信封**的 `{_type:"SearchResponse", webPages:{…}}`
/// —— 两者并存，故候选路径写两条（带信封优先）。
///
/// ⚠️ `code` 的**取值与错误体形状未经真实响应校验**（第三方实测文章只列了 HTTP
/// 错误码 400/401/403/429/500）。故这里的处理是「若 `code` 存在且不是 200 → 报错」，
/// 不存在则**不假设**：不带 `code` 的响应（上面那个扁平形态）照常解析。
fn bocha_hits(v: &Value) -> Result<Vec<SearchHit>, String> {
    if let Some(code) = code_of(v)
        && code != 200
    {
        let msg = str_field(v, "msg").unwrap_or_else(|| "(响应里没有 msg)".to_string());
        return Err(format!("bocha code={code}: {msg}"));
    }
    let items = take_array(v, &[&["data", "webPages", "value"], &["webPages", "value"]])
        .ok_or_else(|| "bocha 响应缺少 data.webPages.value 数组".to_string())?;
    // `snippet` 优先，`summary` 兜底（我们请求的是 `summary:false`，正常只有前者）。
    Ok(map_hits(items, "name", "url", &["snippet", "summary"]))
}

/// Firecrawl：`data.web[]{title, url, description}`。
///
/// **已核**（官方 Search 文档给出的样例逐字为
/// `{"success": true, "data": {"web": [{"url","title","description","position"}]}}`
/// —— `/v2/search` 端点）。候选路径同时收 v1 的 `data[]`（老版本是数组），
/// 两者都试是因为自建实例的版本不受我们控制。
///
/// 错误形态 `{"success": false, "error": "..."}` **未经真实响应校验**（文档只给出
/// 成功样例；依据是 SDK 与社区报错样例）。同理走「宽松认领」：`success == false`
/// 就报错，字段缺失不假设。
fn firecrawl_hits(v: &Value) -> Result<Vec<SearchHit>, String> {
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        let why = v
            .get("error")
            .map(brief)
            .unwrap_or_else(|| "(响应里没有 error)".to_string());
        return Err(format!("firecrawl success=false: {why}"));
    }
    let items = take_array(v, &[&["data", "web"], &["data"]])
        .ok_or_else(|| "firecrawl 响应缺少 data.web 数组".to_string())?;
    Ok(map_hits(items, "title", "url", &["description"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常量根（`TABLE` 里的三行）：追加路径。
    #[test]
    fn test_join_appends_path_to_bare_root() {
        assert_eq!(
            join("https://api.tavily.com", "/search"),
            "https://api.tavily.com/search"
        );
        assert_eq!(
            join("https://api.bochaai.com/", "/v1/web-search"),
            "https://api.bochaai.com/v1/web-search"
        );
    }

    /// 末尾已带该路径 → **不重复追加**（`…/search/search` 只会换回一个 404）。
    #[test]
    fn test_join_does_not_double_the_suffix() {
        assert_eq!(
            join("https://searx.be/search", "/search"),
            "https://searx.be/search"
        );
        assert_eq!(
            join("https://searx.be/search/", "/search"),
            "https://searx.be/search"
        );
    }

    /// **子路径原样保留** —— 实例常挂在 `https://example.com/searxng` 下，
    /// 不能因为要「规范化」就把前缀吃掉。
    #[test]
    fn test_join_keeps_subpath_prefix() {
        assert_eq!(
            join("https://example.com/searxng", "/search"),
            "https://example.com/searxng/search"
        );
    }

    /// **T9 review P2 的靶子**：用户把浏览器里那条地址整条贴进来。
    ///
    /// 残留的 `q` / 片段必须被丢掉 —— 否则 `plan` 追加的 `q` 排在其后，而
    /// Flask/Werkzeug 的 `args.get("q")` 取**第一个** ⇒ 搜索静默跑在旧查询词上。
    #[test]
    fn test_join_drops_pasted_query_and_fragment() {
        assert_eq!(
            join(
                "https://searx.be/search?q=old+browsing+query&format=html",
                "/search"
            ),
            "https://searx.be/search"
        );
        assert_eq!(
            join("https://searx.be/search#results", "/search"),
            "https://searx.be/search"
        );
        // 查询串存在时**仍然**要能认出「后缀已在」（不能拼成 `…?q=x/search`）。
        assert_eq!(
            join("https://searx.be/api/search?q=x", "/search"),
            "https://searx.be/api/search"
        );
    }

    /// 解析不了（空串 / 相对地址）→ 退回朴素拼接，**不 panic**（错误由下游给上下文）。
    #[test]
    fn test_join_falls_back_for_non_url_root() {
        assert_eq!(join("", "/search"), "/search");
        assert_eq!(join("searx.local/", "/search"), "searx.local/search");
    }

    /// 宿主粘贴事故的**端到端**形态：`plan` 产出的请求 URL 里 `q` **只出现一次**，
    /// 且是我们传进去的那个查询词。
    #[test]
    fn test_plan_searxng_from_pasted_browser_url_has_exactly_one_query() {
        let client = Client::new();
        let backend = Backend {
            kind: Kind::Searxng,
            endpoint: "https://searx.be/search?q=old+browsing+query&format=html".to_string(),
            key: None,
        };

        let url = plan(&client, &backend, "rust error handling", 3)
            .expect("plan")
            .build()
            .expect("build")
            .url()
            .to_string();

        assert_eq!(
            url, "https://searx.be/search?q=rust+error+handling&format=json",
            "陈旧的 `q` 必须被清掉（否则搜索跑在旧查询词上）"
        );
        assert_eq!(url.matches("q=").count(), 1, "`q` 只能出现一次：{url}");
    }

    /// Firecrawl 转入「需 key」组：手工构造的无 key `Backend` 拿到的是**可执行的文案**
    /// （点名 env），而不是一个静默不带凭证的请求。
    #[test]
    fn test_plan_firecrawl_without_key_names_the_env() {
        let client = Client::new();
        let backend = Backend {
            kind: Kind::Firecrawl,
            endpoint: FIRECRAWL_ROOT.to_string(),
            key: None,
        };

        let err = plan(&client, &backend, "rust", 3).expect_err("无 key 必须构造期报错");
        assert!(err.contains(ENV_FIRECRAWL_KEY), "{err}");
    }
}
