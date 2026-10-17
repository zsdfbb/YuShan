//! T10（设计 S6）：`backends.rs` 的 **mapper 三态 + 容错** —— 五家 × `ok`/`empty`/`error`，
//! 外加 `plan` 的请求构造（纯函数，不碰网络）。
//!
//! # 这份测试与 `backends.rs` 单元测试的分工
//!
//! `backends.rs` 的 `mod tests` 考的是 **`join` 的路径规则**（粘贴事故、子路径、后缀去重），
//! 本文件考的是**响应映射**：17 份 fixture 直接喂给 `parse`，断言 **`Ok`/`Err` 的语义**
//! 与**逐条的字段内容**。fixture 用 [`include_str!`] 编译期嵌入 —— 永久离线、不读盘。
//!
//! # 为什么必须断言「内容」而不只是「条数」
//!
//! 三条**用变异实测出来的**沉默失败，条数全对但内容不对或丢失：
//!
//! - **字段名写错**（如把 Firecrawl 的 `url` 看成 `urlx`）：整份响应**条数归零**
//!   （`map_hits` 跳过所有条目），而 `Ok(0)` 在链上的语义是「**真没结果，停**」——
//!   于是链**再也不会去试下一家**，一次静默的「没搜到」；
//! - **SearXNG 的 `content` 看成 `contentx`**：条数照旧是 2，**snippet 全空** ——
//!   只断言条数的测试**全绿**；
//! - **提前把「形状不对」当成「空结果」**：本该 `Err`（链**继续**试下一家）变成 `Ok(0)`
//!   （链**停**）—— 语义**反转**。
//!
//! 故本文件的 `_ok` 用例一律用 [`hits_of`] 做**整表相等**断言（`SearchHit` 的
//! `title`/`url`/`snippet` 三个字面量都从 fixture 里抄出来），而不是 `assert_eq!(n, 2)`。
//!
//! # fixture 的 `status` / `headers` 不是装饰
//!
//! - **非 2xx 在 `fetch_capped` 就被 `FetchError::Status` 拦下且不读体** ⇒ `parse`
//!   永远看不到状态码。故 401 那三份 fixture 的 `status` 只在**测试里**被断言
//!   （用来证明「这份 fixture 确实是错误态」），喂给 `parse` 的只有 `Content-Type` + 正文；
//! - **SearXNG 的静默失败本身就是一个 header 事实**（`200 + text/html`）：只存 body
//!   的话它与「一个正常的 HTML 页面」毫无区别，那一条根本测不出来。

use reqwest::Method;
use reqwest::header::AUTHORIZATION;
use serde_json::{Map, Value, json};
use ys_tools_web::{
    Backend, ENV_BOCHA_KEY, ENV_DEEPSEEK_KEY, ENV_FIRECRAWL_KEY, ENV_SEARXNG_URL, ENV_TAVILY_KEY,
    Kind, MAX_LIMIT, MIN_LIMIT, SearchHit, kind_slug, parse, plan,
};

/// 五家 —— 「每家都要过」的用例直接遍历它（漏掉一家是这类测试最常见的疏忽）。
const ALL_KINDS: [Kind; 5] = [
    Kind::Deepseek,
    Kind::Searxng,
    Kind::Tavily,
    Kind::Bocha,
    Kind::Firecrawl,
];

// ════════════════════ fixture 读取 ════════════════════

/// 一份冻结的响应信封：`{status, headers, body}`（见 `tests/fixtures/README.md` §一）。
///
/// `body` 在 fixture 里是**字符串**（代表 UTF-8 解码后的字节）—— HTML 与 JSON 一视同仁，
/// 故这里读出来就是原样的 `&str`，不必分支判断。
struct Fixture {
    #[allow(dead_code)] // 只在「这份 fixture 是哪种状态」的用例里被读。
    status: u16,
    ctype: Option<String>,
    body: String,
}

/// 解析一份 fixture 信封。
fn load(raw: &str) -> Fixture {
    let v: Value = serde_json::from_str(raw).expect("fixture 信封必须是合法 JSON");
    Fixture {
        status: u16::try_from(v["status"].as_u64().expect("信封要有 status"))
            .expect("status 应在 u16 范围内"),
        ctype: v["headers"]["content-type"].as_str().map(str::to_owned),
        body: v["body"]
            .as_str()
            .expect("body 是字符串（UTF-8 解码后的字节）")
            .to_string(),
    }
}

/// `(title, url, snippet)` 字面量 → `Vec<SearchHit>`：断言**内容**，不止条数。
///
/// 整表相等（而不是逐条 `contains`）是刻意选的：字段名写错时条目会被**整条跳过**，
/// 逐条断言在「少了哪条」上会给出更难读的失败；整表 diff 一眼能看出缺了谁、错在哪。
fn hits_of(rows: &[(&str, &str, &str)]) -> Vec<SearchHit> {
    rows.iter()
        .map(|(title, url, snippet)| SearchHit {
            title: (*title).to_string(),
            url: (*url).to_string(),
            snippet: (*snippet).to_string(),
        })
        .collect()
}

/// 读 fixture → `parse`，期望 `Ok`。
///
/// 失败信息里带上 `ctype`：SearXNG 的用例里「头是什么」本身就是被测事实，
/// 出错时报出来比只报一个 `kind` 有用得多。
fn parsed(kind: Kind, raw: &str) -> Vec<SearchHit> {
    let f = load(raw);
    parse(kind, f.ctype.as_deref(), &f.body).unwrap_or_else(|e| {
        panic!(
            "[{}] 期望 Ok，实得 Err：{e}（ctype={:?}）",
            kind_slug(kind),
            f.ctype
        )
    })
}

/// 读 fixture → `parse`，期望 `Err`（**不是** `Ok(0)` —— 见文件头那段语义说明）。
fn parse_err(kind: Kind, raw: &str) -> String {
    let f = load(raw);
    match parse(kind, f.ctype.as_deref(), &f.body) {
        Ok(hits) => panic!(
            "[{}] 期望 Err，实得 Ok({} 条)——「形状不对」被当成了「空结果」",
            kind_slug(kind),
            hits.len()
        ),
        Err(e) => e,
    }
}

/// 直接喂**正文**（`Content-Type: application/json`）—— 合成的容错用例走这条。
fn parse_json_body(kind: Kind, body: &str) -> Result<Vec<SearchHit>, String> {
    parse(kind, Some("application/json"), body)
}

/// 同 [`parse_json_body`]，期望 `Ok`。
fn parsed_json_body(kind: Kind, body: &str) -> Vec<SearchHit> {
    parse_json_body(kind, body)
        .unwrap_or_else(|e| panic!("[{}] 合成正文期望 Ok，实得 Err：{e}", kind_slug(kind)))
}

/// 同 [`parse_json_body`]，期望 `Err`。
fn parse_json_body_err(kind: Kind, body: &str) -> String {
    match parse_json_body(kind, body) {
        Ok(hits) => panic!(
            "[{}] 合成正文期望 Err，实得 Ok({} 条)",
            kind_slug(kind),
            hits.len()
        ),
        Err(e) => e,
    }
}

// ════════════════════ 合成正文的小工具 ════════════════════

/// 各家 `title` / `url` / `snippet` 对应的**字段名**（`backends.rs` 里那 5 处 `map_hits` 调用）。
///
/// ⚠️ 这里写死的是**我们对各家文档的理解**（`tests/fixtures/README.md` §三 的置信度表）：
/// SearXNG / Tavily / Firecrawl 的字段路径已核；Bocha 的**路径已核、错误体未核**。
/// 一旦 `harvest` 用真实响应覆写了 fixture，这张表要跟着改。
fn keys(kind: Kind) -> (&'static str, &'static str, &'static str) {
    match kind {
        Kind::Deepseek => ("title", "url", "summary"),
        Kind::Searxng | Kind::Tavily => ("title", "url", "content"),
        Kind::Bocha => ("name", "url", "snippet"),
        Kind::Firecrawl => ("title", "url", "description"),
    }
}

/// 按 `kind` 的字段名拼一条条目；`None` = **该字段整个缺席**（与 `Some(Value::Null)` 不同）。
///
/// 顺带塞一个**条目级未知字段**：对端加字段不该让我们变红。
fn item(kind: Kind, title: Option<&str>, url: Option<Value>, snippet: Option<&str>) -> Value {
    let (title_key, url_key, snippet_key) = keys(kind);
    let mut m = Map::new();
    if let Some(t) = title {
        m.insert(title_key.to_string(), json!(t));
    }
    if let Some(u) = url {
        m.insert(url_key.to_string(), u);
    }
    if let Some(s) = snippet {
        m.insert(snippet_key.to_string(), json!(s));
    }
    m.insert("unknownField".to_string(), json!({ "nested": [1, 2, 3] }));
    Value::Object(m)
}

/// 按 `kind` 把条目列表塞进这一家的**信封**里（对应 `take_array` 的候选路径）。
fn envelope(kind: Kind, items: Value) -> String {
    match kind {
        Kind::Deepseek | Kind::Searxng | Kind::Tavily => json!({ "results": items }).to_string(),
        Kind::Bocha => {
            json!({ "code": 200, "data": { "webPages": { "value": items } } }).to_string()
        }
        Kind::Firecrawl => json!({ "success": true, "data": { "web": items } }).to_string(),
    }
}

// ════════════════════ S6.1 五家 × 三态 ════════════════════

// ---------- DeepSeek ----------

/// `deepseek_ok`：`results[]{title, url, snippet, summary}` 逐字段落位。
///
/// 整表断言同时钉住四件事：
/// - **`summary` 优先**（第一条两者都有且文本不同）；
/// - **`snippet` 兜底**（第二条 `summary: null`）；
/// - **空 `url` 跳过**（第三条）；
/// - **未知字段不渗入**（`rerank_score` / `content` / `site_name` / `date` / `authority`）。
#[test]
fn test_deepseek_ok_maps_each_hit_verbatim() {
    assert_eq!(
        parsed(Kind::Deepseek, include_str!("fixtures/deepseek_ok.json")),
        hits_of(&[
            (
                "Rust 错误处理 - 官方文档",
                "https://doc.rust-lang.org/book/ch09-00-error-handling.html",
                "摘要优先：错误分为可恢复与不可恢复两类。",
            ),
            (
                "anyhow - Rust",
                "https://docs.rs/anyhow/latest/anyhow/",
                "来自 snippet 的兜底片段。",
            ),
        ]),
        "summary 优先、snippet 兜底、空 url 跳过、未知字段不渗入"
    );
}

/// `deepseek_empty`：`results: []` ⇒ **`Ok(0)`**（真空结果 —— 链在**这里停**）。
#[test]
fn test_deepseek_empty_is_ok_zero() {
    assert_eq!(
        parsed(Kind::Deepseek, include_str!("fixtures/deepseek_empty.json")),
        Vec::<SearchHit>::new(),
        "空数组是「真没结果」⇒ Ok(0)（链停），与「形状不对」的 Err（链继续）是两回事"
    );
}

/// `deepseek_error`：官方 OpenAI 兼容错误信封 `{"error":{"message",…}}`。
///
/// `status` 是 **400**（请求体不合法），但 `parse` 看不到它（非 2xx 在 `fetch_capped`
/// 就被拦下、不读体）；这里断言 `status` 只为标明「这份 fixture 是错误态」。
#[test]
fn test_deepseek_error_envelope_is_err_not_empty_result() {
    let f = load(include_str!("fixtures/deepseek_error.json"));
    assert_eq!(f.status, 400, "该 fixture 是错误态（parse 看不到状态码）");

    let err = parse(Kind::Deepseek, f.ctype.as_deref(), &f.body).expect_err("error 信封必须报错");
    assert!(err.contains("deepseek"), "要点名是哪一家：{err}");
    assert!(err.contains("count"), "要把 message 带出来：{err}");
}

/// ★ **「形状不对」≠「空结果」**：缺 `results` 数组必须 `Err`（链**继续**试下一家），
/// **绝不能**是 `Ok(0)`（链**停**）。
///
/// 变异：把 `deepseek_hits` 的 `ok_or_else(…)` 换成 `unwrap_or_default()` ⇒ 本用例红。
#[test]
fn test_deepseek_missing_results_is_err() {
    let err = parse_json_body_err(Kind::Deepseek, r#"{"foo":1}"#);
    assert!(err.contains("results"), "文案要说清缺的是什么：{err}");
    assert!(err.contains("deepseek"), "要点名是哪一家：{err}");
}

/// `summary` 优先、`snippet` 兜底 —— 合成正文把两条路径各钉一次。
///
/// fixture 的整表断言已覆盖同一事实；这条单独拎出来，是为了让「`summary` 缺失 /
/// 是 `null` 时回退」在失败信息里一眼可见（而不是混在整表 diff 里）。
#[test]
fn test_deepseek_summary_preferred_and_snippet_falls_back() {
    let body = json!({
        "results": [
            {
                "title": "两者都有",
                "url": "https://both.example/",
                "summary": "来自 summary",
                "snippet": "来自 snippet",
            },
            {
                "title": "summary 为 null",
                "url": "https://null.example/",
                "summary": null,
                "snippet": "回退到 snippet",
            },
        ]
    })
    .to_string();

    assert_eq!(
        parsed_json_body(Kind::Deepseek, &body),
        hits_of(&[
            ("两者都有", "https://both.example/", "来自 summary"),
            ("summary 为 null", "https://null.example/", "回退到 snippet"),
        ])
    );
}

// ---------- SearXNG ----------

/// `searxng_ok`：`results[]{title, url, content}` 逐字段落位。
///
/// **变异靶子（M2b）**：把 `searxng_hits` 里的 `"content"` 改成 `"contentx"` ⇒ 条数**仍为 2**、
/// snippet **全空** —— 只有内容断言能抓（这也正是本条用整表相等的原因）。
///
/// 同一条还顺带钉住：`engine` / `score` / `positions` / `publishedDate: null` /
/// `unresponsive_engines` 这些**未知字段一律不渗进 `SearchHit`**。
#[test]
fn test_searxng_ok_maps_each_hit_verbatim() {
    assert_eq!(
        parsed(Kind::Searxng, include_str!("fixtures/searxng_ok.json")),
        hits_of(&[
            (
                "Error Handling - The Rust Programming Language",
                "https://doc.rust-lang.org/book/ch09-00-error-handling.html",
                "Rust groups errors into two major categories: recoverable and unrecoverable errors.",
            ),
            (
                "Rust 错误处理实践",
                "https://blog.example.org/rust-errors-in-practice",
                "anyhow 与 thiserror 的取舍。",
            ),
        ]),
        "M2b 靶子：`content` 是 snippet 的来源，字段名写错 ⇒ 条数照旧、snippet 全空"
    );
}

/// `searxng_empty`：`results: []` ⇒ **`Ok(0)`**（真空结果 —— 链在**这里停**，不白跑后面的后端）。
#[test]
fn test_searxng_empty_is_ok_zero() {
    assert_eq!(
        parsed(Kind::Searxng, include_str!("fixtures/searxng_empty.json")),
        Vec::<SearchHit>::new(),
        "空数组是「真没结果」⇒ Ok(0)（链停），与「形状不对」的 Err（链继续）是两回事"
    );
}

/// ★ **SearXNG 的静默失败**：`200 + text/html`（JSON 没开时它就是这个形态）。
///
/// 状态码是**成功**、正文是完整的搜索页 HTML。若先落进 JSON 解析分支，用户看到的是
/// 「不是合法 JSON」，于是去查网络、查查询词、查实例存活 —— **全都不是原因**。
/// 必须**先查 `Content-Type`** 并直接给出可执行的修复指引。
///
/// **变异靶子（M1）**：去掉 `parse` 里那句 `ensure_json_ctype(ctype)?` ⇒ 本用例红
/// （文案退化成「不是合法 JSON」，`json` / `search.formats` / 「不是合法 JSON」三条断言同时红）。
#[test]
fn test_searxng_html_ctype_names_the_json_setting_not_a_json_parse_error() {
    let f = load(include_str!("fixtures/searxng_error.json"));
    assert_eq!(f.status, 200, "这份 fixture 的要点正是「状态码说成功」");

    // 前提：正文**确实**不是合法 JSON —— 否则下面的断言证明不了「先查 ctype」。
    // （顺序写反也能通过测试，那就等于什么都没钉住。）
    assert!(
        serde_json::from_str::<Value>(&f.body).is_err(),
        "searxng_error.json 的正文必须是 HTML（非合法 JSON）"
    );

    let err = parse(Kind::Searxng, f.ctype.as_deref(), &f.body).expect_err("HTML 正文必须报错");

    assert!(err.contains("searxng"), "文案要点名是哪一家：{err}");
    assert!(
        err.contains("json"),
        "文案必须含 `json`（改的就是它）：{err}"
    );
    assert!(
        err.contains("search.formats"),
        "文案必须点名改哪里（settings.yml 的 search.formats）：{err}"
    );
    assert!(
        !err.contains("不是合法 JSON"),
        "退化成 JSON 解析错误 = 把用户引向错误的排查方向：{err}"
    );
}

/// 缺 `Content-Type` 也**不得**放行 —— 看不到头不能证明 JSON 是开着的。
///
/// 文案要**如实**说出「看到的头是什么」（`<no Content-Type>`），而不是编一个。
#[test]
fn test_searxng_missing_ctype_still_points_at_the_json_setting() {
    let body = r#"{"results":[{"title":"t","url":"https://e.example/","content":"c"}]}"#;

    let err = parse(Kind::Searxng, None, body).expect_err("缺头不得放行");
    assert!(err.contains("search.formats"), "{err}");
    assert!(
        err.contains("<no Content-Type>"),
        "文案要如实说看到了什么头：{err}"
    );
}

/// 反面对照：`application/json`（含 `; charset=`）与 `+json` 后缀都要放行。
///
/// 这条与上一条**成对**：上一条钉住「非 JSON 一律拒」，这条钉住「别把 JSON 也拒了」。
/// 少了这条，一个「永远返回 `Err`」的实现能通过所有 SearXNG 错误用例。
#[test]
fn test_searxng_accepts_json_essence_and_plus_json_suffix() {
    let body = r#"{"results":[{"title":"T","url":"https://ok.example/","content":"C"}]}"#;
    let want = hits_of(&[("T", "https://ok.example/", "C")]);

    for ctype in [
        "application/json",
        "application/json; charset=utf-8",
        "application/ld+json",
        "APPLICATION/JSON",
    ] {
        assert_eq!(
            parse(Kind::Searxng, Some(ctype), body)
                .unwrap_or_else(|e| panic!("ctype={ctype} 应放行，实得 Err：{e}")),
            want,
            "ctype={ctype} 应被认作 JSON"
        );
    }
}

/// 对照：`Content-Type` 已经是 JSON、正文却不是 ⇒ 这时「不是合法 JSON」**才是**对的文案。
///
/// （与 [`test_searxng_html_ctype_names_the_json_setting_not_a_json_parse_error`] 一起，
/// 把「哪种失败给哪句话」钉死。）
#[test]
fn test_non_json_body_with_json_ctype_is_named_and_blames_the_backend() {
    for kind in ALL_KINDS {
        let err = parse_json_body_err(kind, "<html>这不是 JSON</html>");
        assert!(err.contains("不是合法 JSON"), "[{}] {err}", kind_slug(kind));
        assert!(
            err.contains(kind_slug(kind)),
            "[{}] 文案要点名是哪一家：{err}",
            kind_slug(kind)
        );
    }
}

// ---------- Tavily ----------

/// `tavily_ok`：`results[]{title, url, content}`；`answer` / `images` / `raw_content` 忽略。
#[test]
fn test_tavily_ok_maps_each_hit_verbatim() {
    assert_eq!(
        parsed(Kind::Tavily, include_str!("fixtures/tavily_ok.json")),
        hits_of(&[
            (
                "Error Handling - The Rust Programming Language",
                "https://doc.rust-lang.org/book/ch09-00-error-handling.html",
                "Rust groups errors into two major categories: recoverable and unrecoverable errors.",
            ),
            (
                "anyhow - Rust",
                "https://docs.rs/anyhow/latest/anyhow/",
                "Flexible concrete Error type built on std::error::Error.",
            ),
        ]),
        "`content` 是 snippet 的来源（M2b 的同类靶子）"
    );
}

/// `tavily_empty`：`results: []` ⇒ `Ok(0)`。
#[test]
fn test_tavily_empty_is_ok_zero() {
    assert_eq!(
        parsed(Kind::Tavily, include_str!("fixtures/tavily_empty.json")),
        Vec::<SearchHit>::new()
    );
}

/// `tavily_error`：正文里的 `detail` 信封。
///
/// ⚠️ 这份 fixture 的 `status` 是 **401** —— 但 `parse` **看不到它**：非 2xx 在 `fetch_capped`
/// 里就已经是 `FetchError::Status`（那一层不读体）。这里断言 `status` 只是为了说明
/// 「这份 fixture 是错误态」，真正喂给 `parse` 的只有 `Content-Type` + 正文。
///
/// `detail` 的形状**未经真实响应校验**（官方文档只列了成功样例），故判据刻意宽松：
/// 没有 `results` 且出现 `detail` 就算错误 —— 多认出一个错误的代价（链继续试下一家）
/// 远小于漏认（把错误当空结果，链提前停）。
#[test]
fn test_tavily_error_detail_names_the_reason() {
    let f = load(include_str!("fixtures/tavily_error.json"));
    assert_eq!(f.status, 401, "该 fixture 是错误态（parse 看不到状态码）");

    let err = parse(Kind::Tavily, f.ctype.as_deref(), &f.body).expect_err("detail 信封必须报错");
    assert!(err.contains("tavily"), "要点名是哪一家：{err}");
    assert!(err.contains("Unauthorized"), "要把原因带出来：{err}");
}

/// ★ **「形状不对」≠「空结果」**（M3 的靶子）：`tavily_missing_results.json` 是
/// `200` + 一个网关式的 `error` 字段、**整个没有 `results`**。
///
/// 这里必须 `Err`（链**继续**试下一家），**绝不能**是 `Ok(0)`（链**停**）。
/// 两种语义的差别是**链还会不会去试别人**。
///
/// **变异靶子（M3）**：把 `tavily_hits` 的 `ok_or_else(…)` 换成 `unwrap_or_default()`
/// ⇒ 本用例红（`Ok(0)` 而不是 `Err`）。
#[test]
fn test_tavily_missing_results_is_err_not_empty() {
    let err = parse_err(
        Kind::Tavily,
        include_str!("fixtures/tavily_missing_results.json"),
    );
    assert!(err.contains("results"), "文案要说清缺的是什么：{err}");
}

/// 「缺失 / 是 `null` / 不是数组」三兄弟走**同一条**路径 ⇒ 五家一律 `Err`。
///
/// 这条刻意遍历 **五家**：链的语义是全局的，只钉一家会漏掉另外四处的
/// `unwrap_or_default()` 式漂移。
#[test]
fn test_missing_or_non_array_results_is_err_not_empty_for_every_backend() {
    let cases = [
        ("整个缺失", r#"{}"#),
        ("是 null", r#"{"results":null,"data":null,"webPages":null}"#),
        ("不是数组", r#"{"results":{},"data":{},"webPages":{}}"#),
    ];

    for kind in ALL_KINDS {
        for (label, body) in cases {
            let err = parse_json_body_err(kind, body);
            assert!(
                err.contains(kind_slug(kind)),
                "[{}] {label} ⇒ 文案要点名是哪一家：{err}",
                kind_slug(kind)
            );
        }
    }
}

// ---------- Bocha ----------

/// `bocha_ok`：`data.webPages.value[]{name, url, snippet, summary}`。
///
/// 第一条**同时**有 `snippet` 与 `summary` —— 断言取到的是 `snippet`（`summary` 只是兜底，
/// 我们请求的是 `summary:false`）；第二条**缺 `id`/`summary`/`datePublished`**（容错）；
/// `siteIcon` / `displayUrl` / `isFamilyFriendly` 忽略。
#[test]
fn test_bocha_ok_maps_each_hit_verbatim() {
    assert_eq!(
        parsed(Kind::Bocha, include_str!("fixtures/bocha_ok.json")),
        hits_of(&[
            (
                "Error Handling - The Rust Programming Language",
                "https://doc.rust-lang.org/book/ch09-00-error-handling.html",
                "Rust groups errors into two major categories: recoverable and unrecoverable errors.",
            ),
            (
                "Rust 错误处理实践",
                "https://blog.example.org/rust-errors-in-practice",
                "anyhow 与 thiserror 的取舍。",
            ),
        ]),
        "第一条的 `summary` 不得盖过 `snippet`（我们请求的是 summary:false）"
    );
}

/// ★ **Bocha 的扁平形态**（无 `data` 信封）：开放平台首页样例就是这个形状
/// （`{_type, queryContext, webPages:{value:[…]}}`），故候选路径写两条。
///
/// 它同时钉住另一条：**没有 `code` 字段 ≠ 错误** —— 不许假设。
#[test]
fn test_bocha_flat_ok_without_data_envelope() {
    assert_eq!(
        parsed(Kind::Bocha, include_str!("fixtures/bocha_flat_ok.json")),
        hits_of(&[
            (
                "Error Handling - The Rust Programming Language",
                "https://doc.rust-lang.org/book/ch09-00-error-handling.html",
                "Rust groups errors into two major categories: recoverable and unrecoverable errors.",
            ),
            (
                "Rust 错误处理实践",
                "https://blog.example.org/rust-errors-in-practice",
                "anyhow 与 thiserror 的取舍。",
            ),
        ]),
        "不带 data 信封的形态必须照常解析（且不得因为「没有 code」就报错）"
    );
}

/// `bocha_empty`：`value: []` ⇒ `Ok(0)`（`code` 是 200，不是错误）。
#[test]
fn test_bocha_empty_is_ok_zero() {
    assert_eq!(
        parsed(Kind::Bocha, include_str!("fixtures/bocha_empty.json")),
        Vec::<SearchHit>::new()
    );
}

/// `bocha_error`：这家把错误塞进 **200 级信封**里（`{"code":401,"msg":…}`）。
///
/// 状态码这层 `parse` 看不到，故「正文里的 `code != 200`」必须被**这家自己**认领；
/// 文案要把 `msg` 带上（用户得知道是哪把 key 出了什么问题）。
#[test]
fn test_bocha_error_code_in_body_names_the_msg() {
    let f = load(include_str!("fixtures/bocha_error.json"));
    assert_eq!(f.status, 401, "该 fixture 是错误态（parse 看不到状态码）");

    let err = parse(Kind::Bocha, f.ctype.as_deref(), &f.body).expect_err("code != 200 必须报错");
    assert!(err.contains("bocha"), "要点名是哪一家：{err}");
    assert!(err.contains("401"), "要把 code 带出来：{err}");
    assert!(err.contains("API Key 无效"), "要把 msg 带出来：{err}");
}

/// `snippet` 优先、`summary` 兜底（多选一的 key 列表）。
///
/// 我们请求 `summary:false`，正常只有 `snippet`；但若对端真给了 `summary`，也不该丢掉。
#[test]
fn test_bocha_snippet_falls_back_to_summary() {
    let body = json!({
        "code": 200,
        "data": { "webPages": { "value": [
            {
                "name": "只有 summary",
                "url": "https://s.example/",
                "summary": "来自 summary 兜底",
            },
        ]}}
    })
    .to_string();

    assert_eq!(
        parsed_json_body(Kind::Bocha, &body),
        hits_of(&[("只有 summary", "https://s.example/", "来自 summary 兜底")])
    );
}

// ---------- Firecrawl ----------

/// `firecrawl_ok`：`data.web[]{title, url, description}`；`position` / `creditsUsed` 忽略。
///
/// **变异靶子（M2）**：把 `firecrawl_hits` 的 `"url"` 改成 `"urlx"` ⇒ `map_hits` 跳过**全部**
/// 条目 ⇒ `Ok(0)`。这是最危险的一种：`Ok(0)` 在链上是「真没结果，**停**」，
/// 于是因为一个字段名拼错，整条链**再也不会去试下一家**。
#[test]
fn test_firecrawl_ok_maps_each_hit_verbatim() {
    assert_eq!(
        parsed(Kind::Firecrawl, include_str!("fixtures/firecrawl_ok.json")),
        hits_of(&[
            (
                "Firecrawl - The Web Data API for AI",
                "https://www.firecrawl.dev/",
                "The web crawling, scraping, and search API for AI.",
            ),
            (
                "Search | Firecrawl",
                "https://docs.firecrawl.dev/features/search",
                "Search the web and get clean, structured content from every result in a single API call.",
            ),
        ]),
        "M2 靶子：`url` 字段名写错 ⇒ 条目全被跳过 ⇒ Ok(0)，而 Ok(0) 会让链**停**"
    );
}

/// `firecrawl_empty`：`data.web: []` ⇒ `Ok(0)`（`success` 是 `true`，不是错误）。
#[test]
fn test_firecrawl_empty_is_ok_zero() {
    assert_eq!(
        parsed(
            Kind::Firecrawl,
            include_str!("fixtures/firecrawl_empty.json")
        ),
        Vec::<SearchHit>::new()
    );
}

/// `firecrawl_error`：`success == false` + `error` 文案。
#[test]
fn test_firecrawl_error_success_false_names_the_reason() {
    let f = load(include_str!("fixtures/firecrawl_error.json"));
    assert_eq!(f.status, 401, "该 fixture 是错误态（parse 看不到状态码）");

    let err =
        parse(Kind::Firecrawl, f.ctype.as_deref(), &f.body).expect_err("success=false 必须报错");
    assert!(err.contains("firecrawl"), "要点名是哪一家：{err}");
    assert!(err.contains("success=false"), "要说清判据：{err}");
    assert!(err.contains("Unauthorized"), "要把原因带出来：{err}");
}

/// 候选路径的**第二条**（Firecrawl v1 的 `data[]`）：老版本是数组，自建实例的版本不受我们控制。
///
/// 只钉 v2（`data.web`）的话，把候选路径砍成一条依旧全绿 —— 而线上就有实例是 v1。
#[test]
fn test_firecrawl_v1_data_array_is_still_accepted() {
    let body = json!({
        "success": true,
        "data": [{"title": "v1 标题", "url": "https://v1.example/", "description": "v1 描述"}],
    })
    .to_string();

    assert_eq!(
        parsed_json_body(Kind::Firecrawl, &body),
        hits_of(&[("v1 标题", "https://v1.example/", "v1 描述")])
    );
}

// ════════════════════ S6.1b 容错：不 panic、跳过而非炸 ════════════════════

/// **列表里混进垃圾** ⇒ 跳过该条，绝不 panic。
///
/// 一次覆盖五种形态（每家都用自己家的字段名跑一遍）：
/// 非对象元素（`null` / 字符串 / 数字 / 数组）、缺 `url`、`url` 是空白、`url` 是 `null`、
/// `url` 类型不对。**只有最后两条**该留下 —— 而且 `title` / `snippet` 缺席时要落成**空串**
/// （内容缺了，但一个能打开的链接对模型仍有价值）。
#[test]
fn test_malformed_entries_are_skipped_never_panic() {
    for kind in ALL_KINDS {
        // 每家都跑**同一套形态**，只把键名换成自己家的（`item` 按 kind 拼）。
        let per_kind = json!([
            null,
            "a bare string",
            42,
            ["nested", "array"],
            { "unrelated": true },
            item(kind, Some("缺 url"), None, Some("s")),
            item(kind, Some("空 url"), Some(json!("   ")), Some("s")),
            item(kind, Some("null url"), Some(Value::Null), Some("s")),
            item(kind, Some("数字 url"), Some(json!(123)), Some("s")),
            item(
                kind,
                None,
                Some(json!("https://ok.example/kept")),
                Some("kept snippet"),
            ),
            item(kind, None, Some(json!("https://ok.example/bare")), None),
        ]);

        assert_eq!(
            parsed_json_body(kind, &envelope(kind, per_kind)),
            hits_of(&[
                ("", "https://ok.example/kept", "kept snippet"),
                ("", "https://ok.example/bare", ""),
            ]),
            "[{}] 只有带非空 url 的两条留下；`title`/`snippet` 缺席 → 空串（不是跳过）",
            kind_slug(kind)
        );
    }
}

/// **未知字段一律忽略**：顶层的（`answer` / `images` / `response_time`）与条目级的
/// 都不该让我们变红，也不该渗进 `SearchHit`（它刻意只有三个字段）。
#[test]
fn test_unknown_fields_are_ignored_at_every_level() {
    let body = json!({
        "results": [item(
            Kind::Tavily,
            Some("T"),
            Some(json!("https://ok.example/")),
            Some("S"),
        )],
        "answer": "一段我们不映射的 AI 摘要",
        "images": [],
        "response_time": 0.42,
        "brand_new_field": { "deeply": { "nested": true } },
    })
    .to_string();

    assert_eq!(
        parsed_json_body(Kind::Tavily, &body),
        hits_of(&[("T", "https://ok.example/", "S")])
    );
}

/// 顶层**不是对象**（`null` / 数组 / 字符串 / 数字）⇒ `Err`（链继续），且不 panic。
#[test]
fn test_non_object_top_level_is_err_not_empty() {
    for kind in ALL_KINDS {
        for body in ["null", "[]", "\"a string\"", "42", "true"] {
            let err = parse_json_body_err(kind, body);
            assert!(
                err.contains(kind_slug(kind)),
                "[{}] 顶层 {body} ⇒ 文案要点名是哪一家：{err}",
                kind_slug(kind)
            );
        }
    }
}

// ════════════════════ S6.2 `plan` 的请求构造（纯函数，不碰网络） ════════════════════

/// 手工构造一个后端（`Table` 的解析结果就是这个形状）。
fn backend(kind: Kind, endpoint: &str, key: Option<&str>) -> Backend {
    Backend {
        kind,
        endpoint: endpoint.to_string(),
        key: key.map(str::to_owned),
    }
}

/// `plan` → `build`（`RequestBuilder` 不 send、不碰网络）。
fn built(b: &Backend, query: &str, limit: usize) -> reqwest::Request {
    plan(&reqwest::Client::new(), b, query, limit)
        .unwrap_or_else(|e| panic!("[{}] plan 应成功，实得 Err：{e}", kind_slug(b.kind)))
        .build()
        .expect("build 应成功")
}

/// POST 请求的 JSON 体。
fn query_body(req: &reqwest::Request) -> Value {
    let bytes = req
        .body()
        .and_then(reqwest::Body::as_bytes)
        .expect("POST 请求应有可读的 JSON 体");
    serde_json::from_slice(bytes).expect("请求体应是 JSON")
}

/// ★ **SearXNG 的请求 URL 里 `q` 恰好出现一次**，且就是我们传进去的那个查询词。
///
/// T9 review P2 修的就是这条：`YUSHAN_SEARCH_SEARXNG_URL` 的语义是「实例地址」，
/// 而用户可能把浏览器里那条 `…?q=rust&format=html` 整条贴进来。纯字符串拼接会让
/// `q` 出现两次，而 Flask/Werkzeug 的 `args.get("q")` 取**第一个** ⇒ 搜索跑在
/// **用户那次浏览的旧查询词**上，状态码 200、结果看着正常 —— 比 404 **危险得多**。
#[test]
fn test_plan_searxng_url_carries_exactly_one_q() {
    let req = built(
        &backend(Kind::Searxng, "https://searx.be", None),
        "rust error handling",
        3,
    );

    assert_eq!(req.method(), Method::GET, "searxng 是 GET");
    assert!(req.body().is_none(), "GET 不带请求体");

    let url = req.url().to_string();
    assert_eq!(
        url,
        "https://searx.be/search?q=rust+error+handling&format=json"
    );
    assert_eq!(url.matches("q=").count(), 1, "`q` 只能出现一次：{url}");
}

/// 同上，但端点是**粘贴进来的浏览器地址**（带旧查询串与片段）。
#[test]
fn test_plan_searxng_from_pasted_browser_url_has_exactly_one_q() {
    let req = built(
        &backend(
            Kind::Searxng,
            "https://searx.be/search?q=OLD+BROWSING+QUERY&format=html#results",
            None,
        ),
        "rust error handling",
        3,
    );

    let url = req.url().to_string();
    assert_eq!(url.matches("q=").count(), 1, "陈旧的 `q` 必须被清掉：{url}");
    assert!(
        url.contains("q=rust+error+handling"),
        "查询词必须是我们传进去的那个：{url}"
    );
    assert!(
        !url.contains("OLD+BROWSING+QUERY"),
        "残留的旧查询词会让搜索**静默跑在旧词上**：{url}"
    );
    assert!(url.contains("format=json"), "{url}");
}

/// 实例地址解析不了（空串 / 相对地址）⇒ **构造期** `Err`，文案点名改哪个 env。
///
/// `join` 那层**故意不报错**（它不知道 `kind`，文案写不准）；报错留在这里。
#[test]
fn test_plan_searxng_rejects_illegal_instance_url_naming_the_env() {
    for endpoint in ["", "searx.local"] {
        let err = plan(
            &reqwest::Client::new(),
            &backend(Kind::Searxng, endpoint, None),
            "q",
            3,
        )
        .expect_err("非法实例地址必须构造期报错");
        assert!(err.contains("searxng"), "要点名是哪一家：{err}");
        assert!(err.contains(ENV_SEARXNG_URL), "要点名改哪个 env：{err}");
    }
}

/// DeepSeek：POST `/v1/tools/search`、`Bearer` 鉴权、体是
/// `{query, count, need_content:false}`；`limit` 同样 clamp 到契约边界。
#[test]
fn test_plan_deepseek_method_path_auth_and_body() {
    let b = backend(Kind::Deepseek, "https://api.deepseek.com", Some("dk-test"));
    let req = built(&b, "rust error handling", 3);

    assert_eq!(req.method(), Method::POST);
    assert_eq!(
        req.url().as_str(),
        "https://api.deepseek.com/v1/tools/search"
    );
    assert_eq!(
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer dk-test")
    );
    assert_eq!(
        query_body(&req),
        json!({
            "query": "rust error handling",
            "count": 3,
            "need_content": false,
        }),
        "正文交给 webfetch，搜索不请求网页正文"
    );

    for (asked, want) in [(0usize, MIN_LIMIT), (MAX_LIMIT + 80, MAX_LIMIT)] {
        assert_eq!(
            query_body(&built(&b, "q", asked))["count"],
            json!(want),
            "[deepseek] 要 {asked} 条 → 应 clamp 成 {want}"
        );
    }
}

/// Tavily：POST `/search`、`Bearer` 鉴权、体是 `{query, max_results, search_depth}`。
#[test]
fn test_plan_tavily_method_path_auth_and_body() {
    let req = built(
        &backend(Kind::Tavily, "https://api.tavily.com", Some("tvly-test")),
        "rust error handling",
        3,
    );

    assert_eq!(req.method(), Method::POST);
    assert_eq!(req.url().as_str(), "https://api.tavily.com/search");
    assert_eq!(
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer tvly-test")
    );
    assert_eq!(
        query_body(&req),
        json!({
            "query": "rust error handling",
            "max_results": 3,
            "search_depth": "basic",
        })
    );
}

/// Bocha：POST `/v1/web-search`、体是 `{query, count, summary}`，且 `summary` **必须为 false**
/// （`true` 会额外让对端生成一段 AI 摘要 —— 更慢更贵，而我们的映射只取短片段）。
#[test]
fn test_plan_bocha_method_path_auth_and_body() {
    let req = built(
        &backend(Kind::Bocha, "https://api.bochaai.com", Some("sk-test")),
        "rust error handling",
        3,
    );

    assert_eq!(req.method(), Method::POST);
    assert_eq!(req.url().as_str(), "https://api.bochaai.com/v1/web-search");
    assert_eq!(
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-test")
    );
    assert_eq!(
        query_body(&req),
        json!({
            "query": "rust error handling",
            "count": 3,
            "summary": false,
        }),
        "`summary:true` 是花钱买一段我们不看的东西"
    );
}

/// Firecrawl：POST `/v2/search`、`Bearer` 鉴权（**必需**）、体是 `{query, limit}`。
#[test]
fn test_plan_firecrawl_method_path_auth_and_body() {
    let req = built(
        &backend(
            Kind::Firecrawl,
            "https://api.firecrawl.dev",
            Some("fc-test"),
        ),
        "rust error handling",
        3,
    );

    assert_eq!(req.method(), Method::POST);
    assert_eq!(req.url().as_str(), "https://api.firecrawl.dev/v2/search");
    assert_eq!(
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer fc-test")
    );
    assert_eq!(
        query_body(&req),
        json!({ "query": "rust error handling", "limit": 3 })
    );
}

/// `limit` 的字段名（四家的键名不同，取值规则必须**同一套**）。
fn limit_field(kind: Kind) -> &'static str {
    match kind {
        Kind::Deepseek => "count",
        Kind::Tavily => "max_results",
        Kind::Bocha => "count",
        Kind::Firecrawl => "limit",
        Kind::Searxng => panic!("searxng 没有 limit 参数（裁剪在客户端侧做）"),
    }
}

/// `limit` 一律 clamp 到 `MIN_LIMIT..=MAX_LIMIT`：`0` 会让部分后端直接报错，
/// 越界会被对端判成 400 —— 那是**我们可以避免的**一次失败。
#[test]
fn test_plan_clamps_limit_into_contract_bounds() {
    for (kind, endpoint) in [
        (Kind::Tavily, "https://api.tavily.com"),
        (Kind::Bocha, "https://api.bochaai.com"),
        (Kind::Firecrawl, "https://api.firecrawl.dev"),
    ] {
        let b = backend(kind, endpoint, Some("k"));
        let field = limit_field(kind);

        for (asked, want) in [
            (0usize, MIN_LIMIT),
            (1, 1),
            (7, 7),
            (MAX_LIMIT, MAX_LIMIT),
            (MAX_LIMIT + 80, MAX_LIMIT),
        ] {
            let body = query_body(&built(&b, "q", asked));
            assert_eq!(
                body[field],
                json!(want),
                "[{}] 要 {asked} 条 → 应 clamp 成 {want}",
                kind_slug(kind)
            );
        }
    }
}

/// SearXNG **没有 `limit` 参数**（API 只有 `q/categories/engines/…`）——
/// 传任何 `limit` 都不该报错，也不该把这个参数塞进 URL（裁剪在客户端侧做）。
#[test]
fn test_plan_searxng_ignores_limit_without_error() {
    let b = backend(Kind::Searxng, "https://searx.be", None);

    for asked in [0usize, 1, MAX_LIMIT, 1000] {
        let req = built(&b, "q", asked);
        assert_eq!(req.method(), Method::GET);
        assert!(req.body().is_none());
        let query = req.url().query().unwrap_or_default().to_string();
        assert!(
            !query.contains("limit"),
            "searxng 不该收到 limit（limit={asked}）：{query}"
        );
    }
}

/// 需要 key 的四家：手工构造的无 key 后端必须在**构造期**报错，且点名改哪个 env。
///
/// 这不是「多试一次」那么轻：一次静默不带凭证的请求换回的是 401，
/// 而文案里只有「builder error」时用户不知道该改什么。
#[test]
fn test_plan_without_key_is_err_naming_the_env() {
    for (kind, endpoint, env) in [
        (Kind::Deepseek, "https://api.deepseek.com", ENV_DEEPSEEK_KEY),
        (Kind::Tavily, "https://api.tavily.com", ENV_TAVILY_KEY),
        (Kind::Bocha, "https://api.bochaai.com", ENV_BOCHA_KEY),
        (
            Kind::Firecrawl,
            "https://api.firecrawl.dev",
            ENV_FIRECRAWL_KEY,
        ),
    ] {
        let err = plan(
            &reqwest::Client::new(),
            &backend(kind, endpoint, None),
            "q",
            3,
        )
        .expect_err("无 key 必须构造期报错");

        assert!(
            err.contains(kind_slug(kind)),
            "[{}] 要点名是哪一家：{err}",
            kind_slug(kind)
        );
        assert!(
            err.contains(env),
            "[{}] 要点名改哪个 env：{err}",
            kind_slug(kind)
        );
    }
}

/// 端点里**已经带了要追加的路径**时不重复追加（`…/search/search` 只会换回一个 404），
/// 且子路径前缀原样保留（实例常挂在 `https://example.com/searxng` 下）。
///
/// 这两条 `join` 的单元测试已覆盖，这里从 `plan` 的**产物**再确认一次：
/// 单元测试过而 `plan` 用错根的时候，只有这一层能发现。
#[test]
fn test_plan_keeps_endpoint_path_shape() {
    let cases = [
        (
            Kind::Searxng,
            "https://searx.be/search",
            "https://searx.be/search",
        ),
        (
            Kind::Searxng,
            "https://example.com/searxng/",
            "https://example.com/searxng/search",
        ),
        (
            Kind::Deepseek,
            "https://api.deepseek.com",
            "https://api.deepseek.com/v1/tools/search",
        ),
        (
            Kind::Tavily,
            "https://api.tavily.com/",
            "https://api.tavily.com/search",
        ),
        (
            Kind::Bocha,
            "https://api.bochaai.com",
            "https://api.bochaai.com/v1/web-search",
        ),
        (
            Kind::Firecrawl,
            "https://api.firecrawl.dev",
            "https://api.firecrawl.dev/v2/search",
        ),
    ];

    for (kind, endpoint, want) in cases {
        let b = backend(kind, endpoint, Some("k"));
        assert_eq!(
            built(&b, "q", 3).url().as_str().split('?').next().unwrap(),
            want,
            "[{}] endpoint={endpoint}",
            kind_slug(kind)
        );
    }
}
