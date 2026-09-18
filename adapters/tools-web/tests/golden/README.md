# `tests/golden/` —— HTML→Markdown 的冻结夹具

**来源：全部是真实抓取**（`curl` 直连，HTTP GET + 浏览器 UA），抓取日期 **2026-09-18**。
**没有合成夹具** —— 每份都是站点当日返回的原始 HTML 字节。

用途（设计 §1.8 ③）：`htmd` 是 `0.5.x`（0.x **无 SemVer 兼容保证**），升级或换 crate
**必须过 golden**。断言方向是「导航/脚本消失、正文完整、无 mojibake」，不是逐字节快照
（转换器的小版本差异会改空白，逐字节断言会把升级变成不可能）。

> ⚠️ 这些是**测试资产**（Task 8 消费），不是测试代码。本目录**没有** `#[test]`。

## 五份夹具

| 文件 | 原型 | 来源 URL | 字节 | 截断 |
|---|---|---|---|---|
| `tech-docs.html` | 技术文档 | `https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/code` | 184 487 | 否 |
| `news.html` | 新闻 | `https://thehackernews.com/2026/09/can-you-prove-new-cve-is-exploitable.html` | 170 629 | 否 |
| `blog.html` | 博客 | `https://www.smashingmagazine.com/2019/04/cloudflare-workers-serverless/` | 204 800 | **是**（原 262 605） |
| `github-readme.html` | GitHub README | `https://github.com/BurntSushi/ripgrep` | 415 127 | 否（见下） |
| `chinese.html` | 中文站 | `https://www.ruanyifeng.com/blog/2025/01/weekly-issue-334.html` | 110 790 | 否 |

> **来源 URL 更正（2026-09-18）**：`tech-docs.html` 的抓取路径以该页
> `<meta property="og:url">` 的**最终值**为准 —— MDN 站点改版后是
> `…/docs/Web/HTML/**Reference/Elements**/code`（旧路径
> `…/docs/Web/HTML/Element/code` 会 302 过去）。夹具是重定向**之后**的字节，
> 故上表已按最终 URL 记录。

截断按**字节**做，且**退到 UTF-8 字符边界**（绝不把多字节字符劈开 —— 本项目对这条有血的教训）。
截断后仍须是合法 UTF-8（已验：五份都可 `from_utf8` 成功）。

### 每份的结构（元素出现次数，`<tag` + 空格或 `>`）

| 文件 | `nav` | `footer` | `script` | `style` | `h1` | `pre` | `code` | `aside` | `header` | `table` |
|---|---|---|---|---|---|---|---|---|---|---|
| tech-docs | 4 | 1 | 7 | 24 | 1 | 3 | 251 | 2 | 1 | 2 |
| news | 3 | 1 | 30 | 6 | 1 | — | — | 4 | 1 | — |
| blog | 4 | 1 | 6 | 2 | 1 | 15 | 16 | — | 3 | — |
| github-readme | 4 | 1 | 15 | 1 | — | 31 | 95 | — | 2 | 7 |
| chinese | — | — | 13 | 1 | 1 | 15 | — | — | — | — |

各份的**结构意图**与要断言的东西：

- **`tech-docs.html`** —— MDN 的 `<code>` 元素页。侧栏（`<aside>`「In this article…」）
  与页脚（「Your blueprint for a better internet」）应当消失；`<h1>`（`` `<code>` HTML
  inline code element ``）与 `<pre><code>` 示例（转成围栏 ``` 块）应当存活。
- **`news.html`** —— The Hacker News 的正文页。站点导航（Home / Newsletter / Webinars…）、
  页脚导航、以及分享栏 `<aside>`（「Share on Facebook / Twitter…」）应当消失；
  `<h1 class='story-title'>` 与正文（「Stop prioritizing on severity alone」所在段落）
  应当存活。
- **`blog.html`** —— Smashing Magazine 的文章页。**标题住在 `<header>` 里**
  （`<header><h1 id=main-heading class=article-header--title>…</h1></header>`）——
  这是「**刻意不跳 `header`**」的理由本身（不跳才保得住标题）；导航（「Write for us /
  Advertise / More Less Menu」）与页脚主题列表应当消失；15 个 `<pre>` 应当存活。
- **`github-readme.html`** —— GitHub 渲染的 `ripgrep` README。README 里的 **6 张
  Markdown 表格**（benchmark 对照表，表头 `Tool | Command | Line count | Time`）
  与代码块应当存活；GitHub 的全局导航（「Write better code with AI」）与页脚
  （「Manage cookies」）应当消失。
- **`chinese.html`** —— 阮一峰的周刊文章。**CJK 正文**（约 8 900 个汉字）应当完整、
  **零 `U+FFFD`**（这条是「绝不静默给乱码」的靶子）；`<h1>` 标题与代码块应当存活。

## 两处**偏离设计字面要求**的地方（诚实记录）

设计 §4 希望「5 类真实页面**各截到 ≤200KB**」且「每份都含
`nav`/`footer`/`script`/`style`/`h1`/`pre`+`code`」。真实页面做不到，逐条说明：

1. **`github-readme.html` 未截断（415 127 字节）。** GitHub 仓库页的 `<head>` 内联了
   大量 JS/JSON，**README 正文从 ~350KB 处才开始**（实测：该页第一张 README 表格在
   偏移 349 665，第一个 `<pre>` 在 369 491）。截到 200KB 会把 **README 连同它的表格与
   代码块一起切掉** —— 而那正是本夹具唯一的覆盖目标。实测最小的 GitHub 仓库页也有
   ~286KB（`mitsuhiko/console`），**没有**任何含 README 表格、又 ≤200KB 的仓库页。
   故这一份**保留完整**，多出的 ~215KB 是刻意的取舍。
2. **`news.html` 没有 `<pre>`/`<code>`；`chinese.html` 没有 `<nav>`/`<footer>`。**
   现代新闻站的正文页**不产出 `<pre>`**（试过 The Hacker News / InfoWorld /
   Wired / Slashdot / OSNews / Computerworld / SD Times / Hackaday 等，正文一律是
   `<p>` + 行内 `<code>`，`<pre>` 计数恒为 0）；阮一峰的模板则用 `<div class="footer">`
   风格的容器而**不产出 `<nav>`/`<footer>` 元素**。
   即：不是漏抓，是**这些真实页面本来就没有这些元素**。
   **跨五份合计仍然覆盖全部十种元素**：`nav`（4 份）、`footer`（4 份）、`script`（5 份）、
   `style`（5 份）、`h1`（4 份）、`pre`+`code`（4 份）、`aside`（2 份）、`header`（3 份）、
   `table`（2 份）。
   **没有为了让表格好看而手写或改写过任何一份 HTML** —— 宁可如实标注缺口。

## 原型门槛：`htmd::skip_tags` 是「移除子树」还是「只去标签」？

设计 §4 把这条列为「切片二的放行条件」。**结论：是移除整个子树。**
用一条**仓外探针**（`/tmp` 下的临时 cargo 项目，依赖本 crate，**跑完即删、不入库**）坐实：

```
in : <nav>x</nav><h1>H</h1><script>s</script>
out: "# H"                        # x 与 s 都不见了，H 活着
nested out: "keep-me"             # <nav><ul><li>deep-nav-text</li></ul></nav> 整棵嵌套子树消失
header out: "# Post Title\n\nbody"  # 刻意不跳的 <header> 里的标题存活
SKIP_TAGS = ["script","style","noscript","svg","iframe","form","button","nav","footer","aside"]
             ↑ 十个标签逐个注入 `<TAG>boilerplate-<tag></TAG><p>body-<tag></p>` 验证：
               内容全消失、其后正文全存活
```

因此**不需要**退到 `html_to_tree` + `tree_to_markdown` 两段式；`convert.rs` 的单调用壳成立。

## 一个**已知的产物质量观察**（不是 bug，留给后续判断）

`news.html` 的正文里有一张 `data:image/png;base64,…` 的内联图，`htmd` 会把它原样转成
`![](data:image/png;base64,…)` —— 即 Markdown 里出现一段 base64。设计 §4 的跳标签表
**不含 `img`**（普通 `![](https://…)` 对模型是有用信息），故这里**不动它**，只记录：
若将来发现这类内联图把产物撑得太长，可考虑对 `data:` 协议单独处理（那是新的决策，
需要改设计，不该在 `convert.rs` 里悄悄加）。
