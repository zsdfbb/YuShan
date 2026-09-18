//! HTML → Markdown 的**单点隔离壳**（换 crate 只改这一个文件）。
//!
//! # 单函数、**不流式** —— 这不是简化，是被上游 API 逼出来的形状
//!
//! [`html_to_markdown`] 收**整块** `&str`、吐**整块** `String`：
//!
//! 1. `htmd` 要求整块输入 —— 它先 `parse_document` 建完整 DOM，再遍历成 Markdown
//!    （`HtmlToMarkdown::convert` = `tree_to_markdown(&html_to_tree(…))`）。
//!    没有任何「喂一块吐一块」的入口。
//! 2. 内存由**下载侧**的 5 MiB cap（`net::fetch_capped`）兜住：正文最多 5 MiB 常驻
//!    + 转换期间 ~46% 开销 ≈ 峰值 7.5 MiB（设计 §2 的内存口径）。
//!
//! ⚠️ **刻意没有 `CappedWriter`**。设计早期草稿里有过一个「流式适配器」，
//! **已删除**：`htmd` 的输入不是 `Write` 而是 `&str`，写一个只负责丢弃超量字节的
//! writer 既无消费者也无效果（真正的上界在 `cap` 上，不在这里）。若你在别处见到
//! 「convert 是不是漏了 CappedWriter」的疑问，**这段就是答案**。
//!
//! # `skip_tags` 会移除**整个子树**
//!
//! `HtmlToMarkdown::builder().skip_tags(vec![…])` 注册的 handler 返回 `None`，
//! `htmd` 的 walker 拿到 `None` 后**既不追加内容、也不再往下走子节点** ——
//! 效果等同于整棵子树不存在（官方 README 的 `<script>x</script>` → `""` 就是这个）。
//! 本模块的 [`SKIP_TAGS`] 完全依赖这条语义；T7 用一条**仓外探针**把它坐实过
//! （见 `tests/golden/README.md` 的「原型门槛」一节）。
//!
//! # 为什么返回 `Result` 而不是「降级后的纯文本」
//!
//! `htmd` 的 `convert` 返回 `io::Result`（解析层可能报错）。两条候选处置：
//!
//! - 在**这里**就地降级（比如失败时剥标签返回近似纯文本）；
//! - **把 `Result` 交给调用方**（现在的选择）。
//!
//! 选后者的理由：本模块**没有**「失败该对模型说什么」的信息 —— 那是
//! `fetch.rs`（T11）的职责，它才知道该渲染成 `is_error` 结果还是工具失败
//! （ADR-0001 的分界）。在没有上下文的地方替调用方决定「静默给一份也许更差的
//! 答案」正是设计反复拒绝的**不静默给错答案**。
//!
//! **唯一不允许的处置是 `unwrap_or(html)`** —— 那会把原始 HTML（含 `<script>`
//! 正文、含被跳掉的导航）当 Markdown 塞给模型，既没说失败了，又给了错的东西。

use std::sync::LazyLock;

use htmd::HtmlToMarkdown;

/// 转换时**连子树一起丢弃**的标签（设计 §4「跳标签常量」）。
///
/// | 标签 | 为什么跳 |
/// |---|---|
/// | `script` / `style` | 代码不是正文；`style` 的内联 CSS 会变成一大坨噪音 |
/// | `noscript` | 无 JS 时的替代内容，与正文高度重复 |
/// | `svg` | 内联图标 —— 解析出来是一堆无意义的路径数字 |
/// | `iframe` | 嵌入物，正文不在我们自己这份 HTML 里 |
/// | `form` / `button` | 交互控件（搜索框、订阅按钮），对阅读毫无价值 |
/// | `nav` / `footer` | 站点的**骨架**，每页重复 |
/// | `aside` | 侧栏 / 相关阅读 / 广告位 |
///
/// ⚠️ **刻意不跳 `header`** —— 博客与新闻页常把**正文标题**放在 `<header>` 里
/// （`<header><h1>标题</h1></header>` 是极常见的骨架）。跳掉它会把标题一起丢掉，
/// 那是**正文**，不是 boilerplate。宁可多留一点导航，也不冒丢标题的风险。
pub const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "svg", "iframe", "form", "button", "nav", "footer", "aside",
];

/// 共享的转换器实例。
///
/// `htmd` 内置 handler 全部是 `Fn + Send + Sync`（`ElementHandler: Send + Sync`），
/// 故 `HtmlToMarkdown` 可跨线程共享 —— 不必每次调用重建一遍 handler 表。
static CONVERTER: LazyLock<HtmlToMarkdown> = LazyLock::new(|| {
    HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .build()
});

/// 把一整块 HTML 转成 Markdown。**不流式**（理由见模块文档）。
///
/// 失败时返回**可读的原因**，由调用方决定怎么渲染（通常是一条 `is_error` 结果）。
/// 本函数绝不返回原始 HTML 冒充 Markdown，也绝不 panic。
pub fn html_to_markdown(html: &str) -> Result<String, String> {
    CONVERTER
        .convert(html)
        .map_err(|e| format!("HTML 解析失败：{e}"))
}
