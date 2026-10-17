# `tests/fixtures/` —— 五家搜索后端的冻结响应

> **状态：按官方文档手写，未经真实响应校验。**
> 采集脚本 `tests/harvest.rs` 存在的唯一目的，就是把它们**替换成真的**
> （见本文末「怎么换成真的」）。在此之前，T10 的 mapper 测试验的是
> 「**我们对文档的理解**」，不是「对端的实际行为」。

## 一、信封形态：`{status, headers, body}`

```json
{
  "status": 200,
  "headers": { "content-type": "application/json; charset=utf-8" },
  "body": "<响应正文，按 UTF-8 解码后的原样文本>"
}
```

三个字段缺一不可，理由不是「完整」，而是：

- **`status` + `headers` 是解析分支的输入的一部分**。`parse()` 只看 `Content-Type`，
  但**哪一份 fixture 是错误态**只有 `status` 说得清；
- **SearXNG 的静默失败是一个 `header` 事实**：JSON 没开时它回 **`200 + text/html`** ——
  状态码是成功、正文是完整的搜索页 HTML（`searxng_error.json` 就是这一份）。
  只存 body 的 fixture **测不出这条**：它看起来与「一个正常的 HTML 页面」毫无区别；
- `body` 是**字符串**（不是嵌套的 JSON 对象），因为它代表**字节**（UTF-8 解码后）：
  HTML 与 JSON 一视同仁，`harvest` 写出来和手写的形状完全一致，读的一方也不必分支判断。

`headers` 只保留解析用得上的那些（手写件里只有 `content-type`）。
`harvest` 会写入**对端真实返回的响应头**，但**排除 `set-cookie`**（唯一会带会话信息的一项）。

## 二、17 份 fixture 与各自覆盖的解析分支

| 文件 | 模拟的响应 | 覆盖的解析分支 | `parse()` 期望 |
|---|---|---|---|
| `deepseek_ok.json` | `200 + application/json` | `results[]{title,url,snippet,summary,...}`；`summary` 优先、`snippet` 回退；空 url 条目跳过；`rerank_score`/`authority`/`site_name` 等忽略 | `Ok(2 条)` |
| `deepseek_empty.json` | `200`，`results: []` | 空结果 | `Ok(0 条)` |
| `deepseek_error.json` | **`400`** + OpenAI 兼容 `{"error":{"message",...}}` | 错误信封识别（`error.message`）；**状态码这一层 `parse` 看不到**（`fetch_capped` 已拦 4xx） | `Err`，文案含 message |
| `searxng_ok.json` | `200 + application/json` | 正常映射；中文 UTF-8；未知字段（`engine`/`positions`/`timings`）；`publishedDate: null`；`unresponsive_engines`（**故意忽略**，设计 R11） | `Ok(2 条)` |
| `searxng_empty.json` | `200 + application/json`，`results: []` | **空结果**（链在此**停**，不白跑后面的后端） | `Ok(0 条)` |
| `searxng_error.json` | **`200 + text/html`**（`format=json` 时 JSON 未开） | ★ **静默失败**：`Content-Type` 检查（设计 §1.5 的核心） | `Err`，且文案含 `settings.yml` |
| `tavily_ok.json` | `200 + application/json` | `results[]{title,url,content}`；`answer`/`images`/`raw_content` 忽略 | `Ok(2 条)` |
| `tavily_empty.json` | `200`，`results: []` | 空结果 | `Ok(0 条)` |
| `tavily_missing_results.json` | `200`，**没有 `results`**（只有一个网关式的 `error` 字段） | ★ **「形状不对」≠「空结果」**：`take_array` 取不到数组 → `Err`（链**继续**试下一家，而不是提前停） | `Err`，且文案含 `results` |
| `tavily_error.json` | **`401`** + `{"detail":{...}}` | 错误信封识别（`detail`）；**状态码这一层 `parse` 看不到**（`fetch_capped` 已拦 4xx） | `Err`，文案含 `Unauthorized` |
| `bocha_ok.json` | `200` + `data.webPages.value[]` | `name`→title / `url` / `snippet`→snippet（`summary` 兜底）；第二条**缺 `id`/`summary`/`datePublished`**（容错）；`siteIcon`/`displayUrl`/`isFamilyFriendly` 忽略 | `Ok(2 条)` |
| `bocha_flat_ok.json` | `200` + **不带 `data` 信封**的 `webPages.value[]` | ★ 同一家的**第二种公开形态**（开放平台首页样例）：候选路径 `["webPages","value"]` 命中；无 `code` 字段 → **不假设**它错 | `Ok(2 条)` |
| `bocha_empty.json` | `200`，`value: []` | 空结果 | `Ok(0 条)` |
| `bocha_error.json` | **`401`** + `{"code":401,"msg":…}` | **正文内的 `code != 200`**（这家把错误放进 200 级信封里） | `Err`，文案含 `401` 与 `msg` |
| `firecrawl_ok.json` | `200` + `data.web[]` | `description`→snippet；`position`/`creditsUsed` 忽略；v2 路径 `data.web` | `Ok(2 条)` |
| `firecrawl_empty.json` | `200`，`data.web: []` | 空结果 | `Ok(0 条)` |
| `firecrawl_error.json` | **`401`** + `{"success":false,"error":…}` | `success == false` → `Err` | `Err` |

### 刻意**没有**覆盖的形态（以及理由）

- **非 UTF-8 正文 / 二进制**：搜索 API 恒返 UTF-8 文本；抓取侧的编码问题归 `fetch.rs`。
- **`429` / `5xx`**：非 2xx 在 `fetch_capped` 里就成 `FetchError::Status` 了，**不读体**，
  `parse` 永远看不到 —— 给它们造 fixture 会让「`parse` 会处理限流」变成一个**假印象**。

> 「`results` 整个缺失 / 是 `null` / 不是数组」这三兄弟走的是**同一条** `take_array → None → Err` 路径
> （见 `tavily_missing_results.json`）—— 它**不是**空结果：`Err` 让链**继续**试下一家，
> 「空结果」会让链**停**（设计 §1.2）。

## 三、五家响应形状的置信度（诚实标注）

| backend | 字段路径 | 结论 | 依据 |
|---|---|---|---|
| DeepSeek | `results[]{title, url, snippet, summary}` | **响应字段已核、错误信封未核** | 响应字段来自官方 Search API 文档给出的完整响应样例（含 `rerank_score`/`content`/`site_name`/`date`/`authority` 等）；错误信封按官方 OpenAI 兼容格式 `{"error":{"message",...}}` 书写，**未经真实响应校验** |
| SearXNG | `results[]{title, url, content}` | **已核** | 官方 `docs.searxng.org/dev/search_api.html` 的字段表；`200 + text/html` 静默失败由 litellm / n8n 的接入文档与官方 `settings.yml`（`search.formats`）印证 |
| Bocha | `data.webPages.value[]{name, url, snippet, summary}` | **路径已核、错误体未核** | 路径逐字来自 Bocha 官方组织在 GitHub 的 `Bocha-Labs/bocha-skills`（`bocha-web-search/SKILL.md`：「搜索结果位于 `data.webPages.value[]`」）；开放平台首页与第三方实测给的是**不带 `data` 信封**的扁平形态（`bocha_flat_ok.json` 就是这一形），故 `parse` 两条路径都试。`code != 200` 的错误体形状**未核**（官方只列了 HTTP 错误码 400/401/403/429/500） |
| Tavily | `results[]{title, url, content}` | **已核** | 官方文档与 SDK 示例一致（`response["results"]` 的 `title`/`url`/`content`）。**错误体 `{"detail": {...}}` 未核**：文档没给错误 schema，依据是 SDK 与社区报错样例 |
| Firecrawl | `data.web[]{title, url, description}` | **已核** | 官方 Search 文档的样例逐字为 `{"success":true,"data":{"web":[{"url","title","description","position"}]}}`（`/v2/search`）。**错误体 `{"success":false,"error":…}` 未核** |

## 四、怎么换成真的（`tests/harvest.rs`）

```bash
# 一条命令跑全部（缺哪个 env 就红哪一条，红的文案告诉你要 export 什么）
export YUSHAN_SEARCH_DEEPSEEK_KEY=sk-…   # 亦可 export DEEPSEEK_API_KEY（通用名，作为回退）
export YUSHAN_SEARCH_TAVILY_KEY=tvly-…    # 亦可 export TAVILY_API_KEY（通用名，作为回退）
export YUSHAN_SEARCH_BOCHA_KEY=sk-…
export YUSHAN_SEARCH_FIRECRAWL_KEY=fc-…                 # 必需（ADR-0016 修订：它不免 key）
export YUSHAN_SEARCH_SEARXNG_URL=http://127.0.0.1:8888   # 自建实例；公共实例也行
cargo test -p ys-tools-web --test harvest -- --ignored --nocapture
```

- **脚本不硬编码端点**：每家的地址（含 SearXNG 的实例地址）都从**产品那份 `TABLE`**
  经 `resolve_backends` 取 —— 「改了表、忘了改脚本」不会再静默地继续打旧地址。
- **不进 CI**：每个采集测试都带 `#[ignore]`（要真实 key / 真实实例 / 真实网络）。
- **跑完请检查 fixture 里有没有个人信息**：脚本会拒收 `set-cookie` 头、并在写盘前
  **断言 key 不出现在响应里**，但**响应正文本身**（查询词、站点名、`request_id`/`log_id`）
  是我们控制不了的 —— 一眼扫过再提交。
- 覆写后请回头改本文件的第二节（真实响应可能有这次没料到的形态）。

## 五、消费者

- `tests/mapper.rs`（T10）：读这些 fixture → `parse()` → 断言三态与容错。
- 想手工看一眼 `body` 解码后长什么样：

  ```bash
  python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['body'])" tests/fixtures/bocha_ok.json
  ```
