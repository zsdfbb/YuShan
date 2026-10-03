//! 采集工具：打**真实端点**，把响应按原样冻进 `tests/fixtures/`。
//!
//! # 这是人工采集工具，**不进 CI**
//!
//! 每个测试都带 `#[ignore]` —— 它们要真实 key、真实实例、真实网络，而且**会写仓库里的文件**。
//! `cargo test --workspace` 只会看到它们被跳过（`ignored` 计数 +4）。
//!
//! ```bash
//! export YUSHAN_SEARCH_TAVILY_KEY=tvly-…
//! export YUSHAN_SEARCH_BOCHA_KEY=sk-…
//! export YUSHAN_SEARCH_FIRECRAWL_KEY=fc-…                  # 必需（ADR-0016 修订：它不免 key）
//! export YUSHAN_SEARCH_SEARXNG_URL=http://127.0.0.1:8888   # 自建实例（JSON 需开）
//! cargo test -p ys-tools-web --test harvest -- --ignored --nocapture
//! # 只采一家：`… -- --ignored harvest_tavily --nocapture`
//! ```
//!
//! 缺哪个 env，对应那条测试就**红**并告诉你要 export 什么 —— 不静默跳过。
//! （Firecrawl 也一样要 key：官方 API 要求 `Authorization`，不带凭证只会换回一个 401，
//! 采不到任何有用的东西 —— 见 ADR-0016 的修订。）
//!
//! # ⚠️ 跑完请检查 fixture 里没有个人信息
//!
//! 脚本能机械保证的只有两条：**拒收 `set-cookie` 头**、**写盘前断言 key 不出现在响应里**
//! （密钥卫生做成断言，而不是指望人记得）。**做不到**的是响应正文本身 ——
//! 查询词、站点名、`request_id` / `log_id` 都是对端给的，提交前请自己扫一眼。
//!
//! # 为什么每个后端都要「用产品同款请求」
//!
//! `ok` / `empty` 两个用例走的是 [`ys_tools_web::plan`] —— 也就是**产品真正会发出去的那条
//! 请求**（方法 / 路径 / 鉴权头 / 请求体全在里面）。再用 [`ys_tools_web::parse`] 解析回来的
//! 响应并断言「至少认出 1 条」。于是这份脚本同时是**一次端到端自检**：
//! 字段名写错、鉴权头写错，在这里就红，而不是等到线上由模型替我们发现。
//!
//! `error` 用例是**手搓**的（坏 key / 故意不带 `format=json`）—— 那正是「产品不该发出的
//! 请求」，只能绕开 `plan` 构造。
//!
//! # 一份 fixture = 一个「响应事实」
//!
//! 信封形态是 `{status, headers, body}`（不是裸 body）：SearXNG 的静默失败是
//! **一个 header 事实**（`200 + text/html`），只存 body 测不出。

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap};
use reqwest::{Client, RequestBuilder};
use serde_json::{Map, Value};
use ys_tools_web::{
    Backend, ENV_BOCHA_KEY, ENV_FIRECRAWL_KEY, ENV_SEARXNG_URL, ENV_TAVILY_KEY,
    ENV_TAVILY_KEY_ALIAS, Kind, build_client, kind_slug, parse, plan, resolve_backends,
};

/// 单次请求的超时。比产品里的预算宽（这是人跑的，宁可等也不愿意少采一份）。
const TIMEOUT: Duration = Duration::from_secs(30);

/// 「空结果」用例的查询串 —— 挑一个几乎不可能命中的长随机串。
///
/// ⚠️ 别用「空字符串」：那在部分后端是**参数错误**（400），会把「空结果」采成「错误」。
const EMPTY_QUERY: &str = "yushan-zzq9-no-such-page-7f3a4c1e";

/// 取一个必需 env；缺失或为空即**红**（harvest 不做「静默跳过」）。
fn require_env(name: &str) -> String {
    require_env_any(&[name])
}

/// 同 [`require_env`]，但接受**多个候选名**（返回第一个非空的）——
/// Tavily 的项目命名空间名与生态通用名都认。
fn require_env_any(names: &[&str]) -> String {
    for name in names {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    panic!(
        "未设置 {}（或为空）。harvest 是人工采集工具，先 export 再跑；\
         见 tests/fixtures/README.md 第四节",
        names.join(" / ")
    )
}

/// 从**产品那份 `TABLE`** 取这一家的端点（`plan` 用的就是它）—— 这里**刻意不硬编码地址**。
///
/// 硬编码会让「改了 `TABLE` 的端点、忘了改脚本」两件事**静默分叉**：脚本继续打旧地址，
/// 而且**一路绿灯**（T9 review P3）。`key` 由调用方给 —— `error` 用例要用**故意的坏 key**，
/// 那正是产品不该发出的请求，只能绕开 env 解析自己造。
fn backend_of(kind: Kind, key: Option<String>) -> Backend {
    let mut backend = resolve_backends(|k| std::env::var(k).ok())
        .into_iter()
        .find(|b| b.kind == kind)
        .unwrap_or_else(|| {
            panic!(
                "`resolve_backends` 里没有 {} —— 该 export 的 env（key / 实例地址）没 export 吗？",
                kind_slug(kind)
            )
        });
    backend.key = key;
    backend
}

/// 与产品同款的 client（UA / CONNECT_TIMEOUT / 重定向策略都一致）。
fn client() -> Client {
    build_client().expect("build_client")
}

/// 发请求并原样取回（状态、响应头、正文）。
async fn fire(req: RequestBuilder, label: &str) -> (u16, HeaderMap, Vec<u8>) {
    let resp = req
        .timeout(TIMEOUT)
        .send()
        .await
        .unwrap_or_else(|e| panic!("[{label}] 请求失败：{e}"));
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp
        .bytes()
        .await
        .unwrap_or_else(|e| panic!("[{label}] 读响应体失败：{e}"))
        .to_vec();
    (status, headers, body)
}

/// `Content-Type` 原样（缺头即 `None`）。
fn ctype_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn fixture_path(kind: Kind, case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("{}_{case}.json", kind_slug(kind)))
}

/// 把一份响应**按原样**冻进 `tests/fixtures/<kind>_<case>.json`。
///
/// 三件必须发生的事（任何一条不满足就 panic，**绝不写一份可疑的 fixture**）：
///
/// 1. **密钥卫生做成断言**（`secrets` 里的任何串出现在正文或响应头里就拒绝写盘）——
///    fixture 是要进仓库的，而「人记得检查」在深夜跑脚本时必然失效；
/// 2. **正文必须是合法 UTF-8** —— 信封的 `body` 是字符串，非 UTF-8 无法忠实表示；
/// 3. **排除 `set-cookie`** —— 唯一会带会话信息的响应头。
fn freeze(kind: Kind, case: &str, status: u16, headers: &HeaderMap, body: &[u8], secrets: &[&str]) {
    let text = std::str::from_utf8(body).unwrap_or_else(|e| {
        panic!(
            "[{}/{case}] 响应体不是合法 UTF-8（{e}）—— 信封的 body 是字符串，无法忠实表示；\
             请手工处理这份响应",
            kind_slug(kind)
        )
    });

    for secret in secrets {
        assert!(
            !secret.is_empty(),
            "[{}/{case}] 空密钥不该被当成「要检查的密钥」传进来",
            kind_slug(kind)
        );
        assert!(
            !text.contains(secret),
            "[{}/{case}] **响应正文里出现了密钥** —— 拒绝写盘（fixture 要进仓库）",
            kind_slug(kind)
        );
        for (name, value) in headers {
            let seen = String::from_utf8_lossy(value.as_bytes());
            assert!(
                !seen.contains(secret),
                "[{}/{case}] **响应头 {name} 里出现了密钥** —— 拒绝写盘（重定向 URL / 回显都可能）",
                kind_slug(kind)
            );
        }
    }

    let mut out_headers = Map::new();
    for (name, value) in headers {
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        out_headers.insert(
            name.as_str().to_string(),
            Value::String(String::from_utf8_lossy(value.as_bytes()).into_owned()),
        );
    }

    let serialized = write_envelope(status, &out_headers, text);
    let path = fixture_path(kind, case);
    std::fs::write(&path, serialized).unwrap_or_else(|e| panic!("写 {} 失败：{e}", path.display()));
    println!(
        "[harvest] 已写入 {}（status={status}，正文 {} 字节）",
        path.display(),
        body.len()
    );
}

/// 信封 → **与人读的顺序一致**的 JSON 文本：`status` → `headers` → `body`。
///
/// 手工拼，而不是 `to_string_pretty(&json!{…})`：`serde_json::Map` 默认按键名**排序**，
/// 会把信封排成 `body, headers, status` —— 与本目录**手写件**的顺序不一致，
/// 于是 harvest 覆写一次就产生满屏「只是换了行序」的 diff，把真正的变化埋掉。
///
/// 转义仍交给 `serde_json`（不手搓字符串转义：正文里引号、换行、中文都有）。
fn write_envelope(status: u16, headers: &Map<String, Value>, body: &str) -> String {
    /// JSON 字符串字面量（含转义）。
    fn quoted(s: &str) -> String {
        serde_json::to_string(s).expect("字符串一定可序列化")
    }

    let mut rendered = String::new();
    for (i, (name, value)) in headers.iter().enumerate() {
        if i > 0 {
            rendered.push(',');
        }
        rendered.push_str(&format!("\n    {}: {value}", quoted(name)));
    }
    format!(
        "{{\n  \"status\": {status},\n  \"headers\": {{{rendered}\n  }},\n  \"body\": {}\n}}\n",
        quoted(body),
    )
}

/// `ok` 用例：用**产品同款请求**采一次正常查询，并自检 `parse` 认得出结果。
async fn harvest_ok_like(client: &Client, backend: &Backend, secrets: &[&str]) {
    let label = format!("{}/ok", kind_slug(backend.kind));
    let req = plan(client, backend, "rust error handling", 3)
        .unwrap_or_else(|e| panic!("[{label}] plan 失败：{e}"));
    let (status, headers, body) = fire(req, &label).await;
    let text = String::from_utf8_lossy(&body).into_owned();

    assert_eq!(status, 200, "[{label}] 期望 200，实得 {status}：{text}");
    let hits = parse(backend.kind, ctype_of(&headers).as_deref(), &text).unwrap_or_else(|e| {
        panic!("[{label}] **自家 parse 认不出真实响应**（字段名很可能已变，这正是 harvest 要发现的）：{e}\n{text}")
    });
    assert!(
        !hits.is_empty(),
        "[{label}] 解析出 0 条命中 —— 要么字段名变了，要么查询词真的没结果：{text}"
    );
    freeze(backend.kind, "ok", status, &headers, &body, secrets);
}

/// `empty` 用例：采「对端对空查询回什么」。
///
/// **不对状态码下断言**：真正的采集对象就是这个事实（常见是 `200 + results: []`，
/// 但也可能是 400 / 429）。冻下来 + 把异常状态打出来。
async fn harvest_empty(client: &Client, backend: &Backend, secrets: &[&str]) {
    let label = format!("{}/empty", kind_slug(backend.kind));
    let req = plan(client, backend, EMPTY_QUERY, 3)
        .unwrap_or_else(|e| panic!("[{label}] plan 失败：{e}"));
    let (status, headers, body) = fire(req, &label).await;
    if status != 200 {
        println!(
            "[harvest] ⚠ {label}：status={status}（不是 200 —— 空结果用例采到的是错误，记下来）"
        );
    }
    freeze(backend.kind, "empty", status, &headers, &body, secrets);
}

// ---------- 四家各一个采集测试 ----------

/// Tavily：`POST /search`，`Authorization: Bearer`。
#[tokio::test]
#[ignore = "人工采集：需真实 key + 真实网络，不进 CI"]
async fn harvest_tavily() {
    let key = require_env_any(&[ENV_TAVILY_KEY, ENV_TAVILY_KEY_ALIAS]);
    let backend = backend_of(Kind::Tavily, Some(key.clone()));
    let client = client();

    harvest_ok_like(&client, &backend, &[&key]).await;
    harvest_empty(&client, &backend, &[&key]).await;

    // error 用例：坏 key（真 key 加后缀）。采的是「这家怎么报鉴权失败」。
    // ⚠️ 卫生断言仍传**真 key** —— 坏 key 以它为前缀，回显里可能带出来。
    let bad = Backend {
        key: Some(format!("{key}-invalid")),
        ..backend.clone()
    };
    let req = plan(&client, &bad, "rust", 3).expect("plan");
    let (status, headers, body) = fire(req, "tavily/error").await;
    if status == 200 {
        println!("[harvest] ⚠ tavily/error：坏 key 竟然 200 —— 值得看一眼正文");
    }
    freeze(Kind::Tavily, "error", status, &headers, &body, &[&key]);
}

/// 博查：`POST /v1/web-search`，`Authorization: Bearer`。
#[tokio::test]
#[ignore = "人工采集：需真实 key + 真实网络，不进 CI"]
async fn harvest_bocha() {
    let key = require_env(ENV_BOCHA_KEY);
    let backend = backend_of(Kind::Bocha, Some(key.clone()));
    let client = client();

    harvest_ok_like(&client, &backend, &[&key]).await;
    harvest_empty(&client, &backend, &[&key]).await;

    let bad = Backend {
        key: Some(format!("{key}-invalid")),
        ..backend.clone()
    };
    let req = plan(&client, &bad, "rust", 3).expect("plan");
    let (status, headers, body) = fire(req, "bocha/error").await;
    if status == 200 {
        println!(
            "[harvest] ⚠ bocha/error：坏 key 竟然 200 —— 正文里的 `code` 才是判据（parse 会读它）"
        );
    }
    freeze(Kind::Bocha, "error", status, &headers, &body, &[&key]);
}

/// Firecrawl：`POST /v2/search`，`Authorization: Bearer`（**必需**）。
///
/// 曾经这里允许不配 key（把「keyless 到底行不行」当作要采集的未决项）。**已定案**：
/// 官方 API 文档要求 `Authorization`，不带凭证只会 401（ADR-0016 修订 / T9 review P1）
/// —— 那一轮「不带 key 的探测」除了一个 401 之外采不到任何东西，故改为 `require_env`。
#[tokio::test]
#[ignore = "人工采集：需真实 key + 真实网络，不进 CI"]
async fn harvest_firecrawl() {
    let key = require_env(ENV_FIRECRAWL_KEY);
    let backend = backend_of(Kind::Firecrawl, Some(key.clone()));
    let client = client();
    let secrets = [key.as_str()];

    harvest_ok_like(&client, &backend, &secrets).await;
    harvest_empty(&client, &backend, &secrets).await;

    // error 用例：坏 key（真 key 加后缀）。采的是「这家怎么报鉴权失败」。
    let bad = Backend {
        key: Some(format!("{key}-invalid")),
        ..backend.clone()
    };
    let req = plan(&client, &bad, "rust", 3).expect("plan");
    let (status, headers, body) = fire(req, "firecrawl/error").await;
    if status == 200 {
        println!("[harvest] ⚠ firecrawl/error：坏凭证竟然 200 —— 正文里的 success/error 才是判据");
    }
    freeze(Kind::Firecrawl, "error", status, &headers, &body, &secrets);
}

/// SearXNG：`GET /search?q=&format=json`，免 key，地址来自 `YUSHAN_SEARCH_SEARXNG_URL`。
///
/// 用**自建实例**（`docker run … searxng/searxng`，`settings.yml` 的
/// `search.formats` 要含 `json`）。公共实例多数没开 JSON。
#[tokio::test]
#[ignore = "人工采集：需真实实例（自建 docker 或公共实例）+ 真实网络，不进 CI"]
async fn harvest_searxng() {
    require_env(ENV_SEARXNG_URL);
    let backend = backend_of(Kind::Searxng, None);
    let client = client();

    harvest_ok_like(&client, &backend, &[]).await;
    harvest_empty(&client, &backend, &[]).await;

    // error 用例：**故意不带 `format=json`** —— 采的正是设计 §1.5 的**静默失败**
    // （`200 + text/html`：状态码是成功、正文是完整的搜索页 HTML）。
    // ⚠️ 这一份比「5xx / 限流」有价值得多：非 2xx 在 fetch_capped 里就被拦了、parse 根本
    // 看不到；而静默失败**只有 parse 的 ctype 检查**能拦。
    let req = client
        .get(format!("{}/search", backend.endpoint.trim_end_matches('/')))
        .query(&[("q", "rust error handling")]);
    let (status, headers, body) = fire(req, "searxng/error").await;
    let ctype = ctype_of(&headers);

    let essence = ctype.as_deref().map(|c| {
        c.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    });
    assert_ne!(
        essence.as_deref(),
        Some("application/json"),
        "searxng/error 要采的是「JSON 未开」的形态，但这个实例对不带 format 的请求也回了 JSON\
         （content-type={ctype:?}）—— 换一个没开 json 的实例，或先确认 settings.yml"
    );

    // 自检：这份响应**必须**被自家 parse 拦下（这条就是设计里那个变异靶子）。
    let text = String::from_utf8_lossy(&body).into_owned();
    let err = parse(Kind::Searxng, ctype.as_deref(), &text)
        .expect_err("静默失败（200 + 非 JSON）必须被 parse 拦下");
    assert!(
        err.contains("settings.yml"),
        "拦下的理由必须给出可执行的修复指引：{err}"
    );

    freeze(Kind::Searxng, "error", status, &headers, &body, &[]);
}
