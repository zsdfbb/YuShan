# websearch / webfetch — 同类实现调研与决策

> 日期：2026-09-17 ｜ 状态：**切片二的设计输入**（切片二尚未启动）
> 上游：`docs/arch/tools-align-pi/design.md`（切片一已落地）、`docs/adr/0015-…`
> 参考实现（只读）：`tmp/pi`、`tmp/codewhale`、`tmp/Vibe-Trading`

## 1. 四家怎么做

| | **Claude Code** | **CodeWhale** | **Vibe-Trading** | **Pi** |
|---|---|---|---|---|
| 搜索 | Anthropic **服务端** `web_search_20250305` | **11 家 provider 链** | **ddgs 免 key 元搜索** + 阿里云 IQS | **无 web 工具** |
| 抓取 | **本地**（Axios）→ 转 MD → **Haiku 子对话总结** | **本地** → `htmd`+html5ever + 正文抽取 + PDF | **外包 Jina Reader**（`r.jina.ai`） | — |
| HTML→MD | 自转 | 自转（`extract.rs` 1.2k 行） | **零本地转换** | — |
| 免 key | 无（服务端） | Bing / DuckDuckGo **HTML 抓取** | **ddgs**（6 引擎聚合） | — |
| SSRF | 域名拒绝清单 | **747 行 guard** | **~35 行 `_url_allowed`** | — |
| 注入防护 | **无** | — | **正则扫描 + 标注（不改内容）** | — |
| 体量 | — | ~14k 行 | **2 个文件** | — |

### 值得单独记的三点

- **CodeWhale 的 provider 链**（`tools/web/backend.rs`）：`provider 原生搜索（Kimi/智谱）→ 配置的 provider（默认 Firecrawl，免 key 有限额）→ DuckDuckGo 兜底`，按序试、带 capability/degraded 模型。**与切片一的「rg → grep」是同一个降级哲学。**
- **Vibe-Trading 的免 key 搜索**用 `ddgs`（`duckduckgo_search` 的后继）：一个库覆盖 DDG/Google/Bing/Brave/Mojeek/Yahoo，**显式有序后端列表 + 限流顺延 + 退避重试**。另有可选的**阿里云 IQS**（国内直连 ~1s、结构化、支持财经类目）。
- **Vibe-Trading 的注入防护**（`security/scanner.py`）：正则规则（`ignore/disregard/forget/bypass/override …`）**只往 JSON 信封加警告元数据，从不重写或丢弃内容** —— 约 40 行，三条路线里最平衡。

## 2. 已定决策（2026-09-17）

- **`webfetch`：本地抓取**，不外包给第三方 reader 服务 —— 保持自主与稳定（不受 `r.jina.ai` 之类可用性/限流牵连）。
- **`websearch`：免 key 搜索 + provider API 服务**（如 Tavily）并存，即"先试免 key，再落到配置的 provider"。

## 3. 由此带出的仍未决

1. **HTML→Markdown（因"本地抓"而重新成立）**：候选 `htmd`（CodeWhale 在踩过 readability+html5ever **双栈编译冲突**之后最终选的）+ 跳 boilerplate 标签。若质量不够，再考虑正文抽取（readability 思路）—— 那是数倍工程。
2. **「免 key 搜索」在 Rust 没有 `ddgs` 等价物**。现实选项：
   - **自抓 HTML**（CodeWhale 的 Bing/DDG 免 key provider 正是这么做的，其 `scrape.rs` 约 528 行）—— 能力自主，但**页面结构一变就坏**，需按"脆弱件"对待（可降级、可关）。
   - 找一个 Rust 侧的 DDG 客户端 crate（生态里多为个人维护、更新滞后）。
   - **建议**：把免 key 抓取做成**一个 backend**、失败即顺延，而不是唯一路径；并在启动/调用时能明确告知"当前走的是免 key 抓取（可能不稳）"。
3. **provider 清单与默认值**：CodeWhale 已验证国内可用的有 **博查 Bocha / 秘塔 Metaso / 百度 / 火山 Volcengine**；Vibe-Trading 用**阿里云 IQS**。默认选哪家、是否 provider 原生搜索优先（CodeWhale 已证明 Kimi/智谱可行）——**待定**。
4. **SSRF**：设计文档此前写"不做安全"，**建议改** —— Vibe-Trading 用 ~35 行就做到「scheme 白名单 + 拒 URL 内凭据 + 拒 localhost/`.local` + IP 段判定（私有/回环/链路本地/保留/非全局）」。这个性价比没有理由不做。
5. **注入防护**：**建议采纳"只标注、不改内容"**（约 40 行正则），与 `docs/extension-points.md` 的「Event 只观察」精神一致 —— 我们**不替模型过滤**，只给它归因信号。

## 4. 对设计文档的待办

- `design.md` §4「未决」与 §3「代价」中关于 **SSRF「不做」** 的结论应据 §3.4 修正。
- `design.md` §1.2 的 webfetch 数据流需补 **本地抓取 + HTML→MD** 的确定路径（现在写的是"分派"，未定实现）。
- 切片二开工前，需先 prototype 验证 **`htmd` 的「跳 boilerplate + 写入字节计数适配器」** 两个前提（design.md §4 已列）。
