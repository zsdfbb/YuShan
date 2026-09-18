//! S4 + S5 —— `convert.rs`（HTML→Markdown 单点壳）与 `store.rs`（原子写 + 清理）。
//!
//! # 为什么本文件**不需要** `mod common;`
//!
//! 这里没有一个用例碰网络：`html_to_markdown` 收整块字符串、`write_fetched` / `sweep`
//! 只碰调用方给的目录。故不接那份假服务器（`tests/network.rs` 是它唯一的接线点）。
//!
//! # 副作用边界
//!
//! 只写 `std::env::temp_dir()/yushan_tools_web_convert_store_*` —— 目录名含
//! **进程内原子序号 + `process::id()`**（并行测试不得撞名），由 [`TempDir`] 的
//! `Drop` RAII 收摊（断言失败也收）。**绝不**碰真实的 `HOME` / `TMPDIR` 根、
//! 也绝不改进程级 env（故不需要 `env_lock`）。
//!
//! # 三个「零覆盖」的靶子（T7 review 点名）
//!
//! 1. `SKIP_TAGS` 移除**整个子树** → [`test_skip_tags_remove_whole_subtree_but_keep_header`]
//!    （嵌套 `nav`）+ [`test_every_skip_tag_swallows_its_own_subtree`]（逐标签）；
//! 2. 原子写的**唯一 tmp 名** → [`test_concurrent_writes_to_one_url_leave_one_complete_file`]；
//! 3. **手写 FNV-1a** 的已知值 → [`test_fetched_file_name_matches_independent_fnv_reference`]
//!    （期望值由**仓外** Python 参考实现独立算出，不是本 crate 输出的回声）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use ys_tools_web::{
    FetchedDir, SKIP_TAGS, SweepStats, fetched_file_name, html_to_markdown, sweep, write_fetched,
};

// ─────────────────────────── 本文件自带的小设施 ───────────────────────────

/// 临时目录的 RAII 守卫：断言失败（panic）时也收摊，不给 `/tmp` 留垃圾。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 目录名 = `yushan_tools_web_convert_store_<name>_<pid>_<序号>`。
    ///
    /// ⚠️ 序号必须是**进程内原子**：`SystemTime::as_nanos()` 在 macOS 只到微秒级，
    /// 并行测试会撞名 → 互删（本项目血的教训）。
    fn new(name: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yushan_tools_web_convert_store_{name}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("测试目录应能创建");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 目录下的条目名（排序后），用于「没有残留」「只有 1 个文件」这类断言。
fn entry_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("目录应可读")
        .map(|e| {
            e.expect("条目应可读")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    v.sort();
    v
}

/// 造一个「已经放了很久」（或刚写下）的文件 —— 用于 [`sweep`] 的 `mtime` 判据。
///
/// 用 `File::set_modified`（稳定 API），**不引 `filetime`**：测试依赖越少越好。
fn write_aged(dir: &Path, name: &str, content: &str, age: Duration) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("应能写文件");
    let file = std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("应能以写模式打开");
    file.set_modified(SystemTime::now() - age)
        .expect("应能设置 mtime");
    path
}

/// 「超龄」与「还新鲜」两个档（`sweep` 的阈值取 1 小时）。
const OLD: Duration = Duration::from_secs(10 * 24 * 3600);
const MAX_AGE: Duration = Duration::from_secs(3600);

// ═════════════════════════════ S4：convert.rs ═════════════════════════════

/// ⭐ **`skip_tags` 移除整个子树**，且**刻意不跳 `header`**。
///
/// 两条判据各自钉一个真值：
///
/// - `<nav><ul><li>deep</li></ul></nav>`：嵌套两层的文本**必须消失** —— 若 `htmd` 只
///   「去标签」而不丢弃子树，`deep-nav-text` 会留下来；
/// - `<header><h1>H</h1></header>`：**必须存活** —— 博客/新闻页常把正文标题放在
///   `<header>` 里（`SKIP_TAGS` 刻意不含 `header`，见 `convert.rs` 的常量文档）。
///
/// 变异①：`SKIP_TAGS` 删掉 `"nav"` → 第一条红。
/// 变异②：`SKIP_TAGS` 里加上 `"header"` → 第二条红。
#[test]
fn test_skip_tags_remove_whole_subtree_but_keep_header() {
    let html = "<nav><ul><li>deep-nav-text</li></ul></nav>\
                <header><h1>Post Title</h1></header>\
                <p>keep-me</p>";
    let md = html_to_markdown(html).expect("不 panic、不报错");

    assert!(
        !md.contains("deep-nav-text"),
        "嵌套子树里的文本必须消失（`skip_tags` 是移除子树，不是去标签）：{md:?}"
    );
    assert!(
        md.contains("Post Title"),
        "`header` 里的标题是正文，**刻意不跳** —— 丢了就是丢标题：{md:?}"
    );
    assert!(
        md.contains("keep-me"),
        "被跳标签**之后**的正文必须存活：{md:?}"
    );
}

/// 十个跳标签**逐个**钉：自己的子树消失，**紧随其后的正文存活**。
///
/// # 为什么这里把标签抄成**字面量**、而不是 `for tag in SKIP_TAGS`
///
/// 遍历 `SKIP_TAGS` 是**自指的**：从表里删掉一项，循环只是**少跑一圈**，断言一个不少
/// —— 测试**永远不会红**（实测：删掉 10 项里的一半，全绿）。那种「保护」比没有更糟：
/// 下一个误删条目、或升级 `htmd` 的人会以为这里有网。故标签**在此处独立抄写**。
///
/// 本用例**能**抓到：
/// - `SKIP_TAGS` 删掉任意一项 → 该项的 `boilerplate-` 活着 → 对应那一行红（报错点名标签）；
/// - `SKIP_TAGS` **新增**一项（末尾的集合相等断言要求同步本表，否则红）；
/// - `htmd` 升级后某标签不再「移除子树」（退化成只去标签）→ 对应那一行红；
/// - 跳标签多吃掉后续兄弟节点 → `after-` 那一行红。
///
/// 本用例**抓不到**：产物的**形态**（换行、围栏、表格）是否正确 —— 它只看「某段文本
/// 在不在」。那是 golden 用例（[`assert_golden`]）的活。也抓不到「某个**不在表里**的
/// 标签被误跳」（如 `header`）—— 那由 [`test_skip_tags_remove_whole_subtree_but_keep_header`]
/// 与 blog golden 钉。
#[test]
fn test_every_skip_tag_swallows_its_own_subtree() {
    // ⚠️ 与 `SKIP_TAGS` **刻意重复**：这份重复正是本用例的全部价值（见上）。
    const TAGS: [&str; 10] = [
        "script", "style", "noscript", "svg", "iframe", "form", "button", "nav", "footer", "aside",
    ];

    // 表变了（增/删）就红 —— 逼着把上面的字面量同步过来，否则覆盖率会**悄悄缩水**
    // （改顺序不算：无害，故按集合比而不是按序比）。这也让「新增标签没人测」不可能发生。
    let mut declared: Vec<&str> = TAGS.to_vec();
    let mut actual: Vec<&str> = SKIP_TAGS.to_vec();
    declared.sort_unstable();
    actual.sort_unstable();
    assert_eq!(
        actual, declared,
        "`SKIP_TAGS` 与本用例的字面量不一致 —— 请把上方 `TAGS` 同步为新表"
    );

    for tag in TAGS {
        let html = format!("<p>before-{tag}</p><{tag}>boilerplate-{tag}</{tag}><p>after-{tag}</p>");
        let md = html_to_markdown(&html).expect("不 panic、不报错");

        assert!(
            !md.contains(&format!("boilerplate-{tag}")),
            "`<{tag}>` 的内容必须消失（它被从 SKIP_TAGS 里删了吗？）：{md:?}"
        );
        assert!(
            md.contains(&format!("after-{tag}")),
            "`</{tag}>` 之后的正文必须存活（跳标签吃掉了多余的兄弟节点？）：{md:?}"
        );
    }
}

/// 标签之外的**普通文本节点**必须原样留下 —— 钉子「不是把所有文本都吃了」。
///
/// 上面两个用例都只断言「消失」，单靠它们无法排除「实现把整篇都清空了」这种荒谬的
/// 变异。这条把「正文存活」补成显式的对照。
#[test]
fn test_plain_text_survives_conversion() {
    let md = html_to_markdown("<p>alpha</p><p>beta</p>").expect("不 panic");
    assert!(md.contains("alpha") && md.contains("beta"), "{md:?}");
}

fn golden(name: &str) -> &'static str {
    match name {
        "tech-docs" => include_str!("golden/tech-docs.html"),
        "news" => include_str!("golden/news.html"),
        "blog" => include_str!("golden/blog.html"),
        "github-readme" => include_str!("golden/github-readme.html"),
        "chinese" => include_str!("golden/chinese.html"),
        other => panic!("未知夹具：{other}"),
    }
}

/// golden 的公共断言：`gone` 必须**在原始 HTML 里**（否则探针失效、测试是空转）
/// 且在产物里**消失**；`alive` 必须在产物里**存活**；产物**零 `U+FFFD`**。
fn assert_golden(name: &str, gone: &[&str], alive: &[&str], min_fences: usize) {
    let html = golden(name);
    let md = html_to_markdown(html).unwrap_or_else(|e| panic!("{name}：转换失败：{e}"));

    for probe in gone {
        assert!(
            html.contains(probe),
            "{name}：探针 {probe:?} 不在原始 HTML 里 —— 夹具换了，用例已空转"
        );
        assert!(
            !md.contains(probe),
            "{name}：被跳标签里的 {probe:?} 漏进了产物：{md:?}"
        );
    }
    for probe in alive {
        assert!(
            md.contains(probe),
            "{name}：正文里的 {probe:?} 没进产物（跳标签吃多了？）：{md:?}"
        );
    }

    assert!(
        !md.contains('\u{FFFD}'),
        "{name}：产物里有 U+FFFD —— 编码路径坏了（「绝不静默给乱码」）"
    );
    assert!(
        md.matches("```").count() / 2 >= min_fences,
        "{name}：代码块（围栏）少于 {min_fences} 个 —— `<pre>` 没转过来？"
    );
    // 产物不能是空的（否则上面所有 `!contains` 都白给）。
    assert!(md.len() > 1000, "{name}：产物只有 {} 字节", md.len());
}

/// `tech-docs.html`（MDN `<code>` 页）：`aside` / `footer` / `nav` / `style` / `script` 消失。
#[test]
fn test_golden_tech_docs() {
    assert_golden(
        "tech-docs",
        &[
            "In this article",                      // <aside> / <nav> 目录
            "Your blueprint for a better internet", // <footer>
            "Advertise with us",                    // <footer>
            "mdn-search-button{align-items",        // <style>（搜索框 CSS）
            "Unable to set theme",                  // <script>（主题初始化）
        ],
        &[
            "HTML inline code element",  // <h1> 正文标题
            "adds one or more elements", // 正文段落（<code>push()</code> 那个例子）
            "Try it",                    // 正文章节标题
        ],
        2,
    );
}

/// `news.html`（The Hacker News）：站点导航 / 分享栏 `aside` / `style` / `script` 消失。
#[test]
fn test_golden_news() {
    assert_golden(
        "news",
        &[
            "Advertise with us",         // <nav> / <footer>
            "Free eBooks",               // <nav>
            "Newsletter",                // <nav>
            "Share on Reddit",           // <aside> 分享栏
            "Reach out to get featured", // <aside>
            "articletext-color",         // <style>（站点 CSS 变量）
            "adsbygoogle",               // <script>（广告）
        ],
        &[
            "Can You Prove a New CVE",             // 标题
            "Stop prioritizing on severity alone", // 正文
        ],
        0, // 该页正文无 <pre>（见 tests/golden/README.md 的偏差记录 ②）
    );
}

/// `blog.html`（Smashing Magazine）：**标题住在 `<header>` 里** —— 「不跳 header」的
/// 真实理由；且该夹具**是截断的**（切在 `<script>` 的 JSON 中段，结构不闭合）。
///
/// ⚠️ 这条**只断言「畸形尾部不被 panic 掉、且截断前的内容照常转换」**，不断言
/// 「完整解析」：`htmd` 实测在 EOF-in-script 下正常返回（html5ever 容忍 EOF）。
#[test]
fn test_golden_blog_with_truncated_tail() {
    let html = golden("blog");
    assert!(
        !html.trim_end().ends_with("</html>"),
        "本夹具**故意是截断的**（见 tests/golden/README.md）；若它补全了，本用例的\
         「容忍畸形尾部」就没被验证 —— 请换一份截断夹具或删掉这条断言"
    );

    assert_golden(
        "blog",
        &[
            "Write for us",                        // <nav>
            "Advertise",                           // <nav>
            "Browse All Smashing Magazine Topics", // <footer> 主题列表
            "ElenaWebRegular",                     // <style>（@font-face）
            "fontsInServiceWorker",                // <script>
        ],
        &[
            // 标题就在 `<header class=article-header>` 里 —— 「不跳 header」的靶子。
            "Going Serverless With Cloudflare Workers",
            "Cloudflare Workers",
        ],
        4,
    );
}

/// `github-readme.html`（ripgrep 的 README）：GitHub 全局导航 / 页脚 / `style` / `script` 消失；
/// **README 的 Markdown 表格与代码块存活**（本夹具唯一的覆盖目标）。
#[test]
fn test_golden_github_readme() {
    assert_golden(
        "github-readme",
        &[
            "Write better code with AI",            // <nav>
            "Direct agents from issue to merge",    // <nav>
            "Manage cookies",                       // <footer>
            "Do not share my personal information", // <footer>
            "--tab-size-preference",                // <style>
            "actions_enable_background_steps",      // <script>（feature flags JSON）
        ],
        &[
            "Line count",                   // README 表格表头
            "ripgrep recursively searches", // README 正文
        ],
        4,
    );
}

/// `chinese.html`（阮一峰周刊）：**CJK 正文完整、零 mojibake** —— 「绝不静默给乱码」的靶子。
///
/// 该页**没有** `<nav>` / `<footer>` / `<aside>`（见 README 的偏差记录 ②），故这里只钉
/// `style` / `script` 两类 + CJK 存活。
#[test]
fn test_golden_chinese_cjk_survives_without_mojibake() {
    let md = html_to_markdown(golden("chinese")).expect("不 panic");
    let html = golden("chinese");

    for probe in [
        "background-color: #f5f5d5;", // <style>
        "loadjscssfile",              // <script>
    ] {
        assert!(html.contains(probe), "探针 {probe:?} 不在原始 HTML 里");
        assert!(
            !md.contains(probe),
            "`<style>/<script>` 漏进了产物：{probe:?}"
        );
    }

    assert!(!md.contains('\u{FFFD}'), "产物里有 U+FFFD：{md:?}");
    for probe in ["科技爱好者周刊", "这里记录每周值得分享的科技内容"] {
        assert!(md.contains(probe), "CJK 正文丢了：{probe:?}");
    }
    // 汉字数量级：该页约 8 900 个汉字，抽掉导航也不该低于 3 000。
    let cjk = md
        .chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
        .count();
    assert!(cjk > 3000, "产物里的汉字只有 {cjk} 个 —— 正文被吃掉了？");
}

// ═════════════════════════════ S5：store.rs ═════════════════════════════

/// ⭐ 落盘名 = `<slug>-<16hex>.md`，hash 是**手写 FNV-1a 64**。
///
/// 期望值由**仓外 Python 参考实现**独立算出（不是拿本 crate 的输出当期望，否则
/// 测试只是回声）：
///
/// ```text
/// h = 0xcbf29ce484222325
/// for b in url.encode(): h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
/// ```
///
/// 变异①：把 `fnv1a64` 换成 `DefaultHasher` → 本用例红（值不同；且 `DefaultHasher`
/// 跨 Rust 版本不保证稳定 —— 那正是手写 FNV 的理由）。
///
/// 变异②：`slug_of` 里把「滤空段」挪回 `next_back()` **之后**（即恢复「先取末段
/// 再滤空」）→ 两条**尾斜杠**用例（`/a/b/`、`/dir/`）红。其余各行不受影响 ——
/// 这正是该修复**只**影响尾斜杠 URL 的证据。
#[test]
fn test_fetched_file_name_matches_independent_fnv_reference() {
    // (URL, 名字, 这一行钉住的东西)
    let cases: &[(&str, &str, &str)] = &[
        (
            "https://example.com/",
            "example-com-0c8b41cfdcb3c914.md",
            "路径为空 → slug 退到 host，host 里的 `.` 折成 `-`",
        ),
        (
            "https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/code",
            "code-540da3c26ecdd878.md",
            "取路径**末段**（`code`），hash 打的是**整条** URL",
        ),
        (
            "https://example.com/a/b",
            "b-369647336abdf6a6.md",
            "取路径**末段**（`b`），hash 打的是**整条** URL",
        ),
        (
            // 尾斜杠是**空**末段，必须在 `next_back()` **之前**滤掉 —— 否则会误退回
            // host（`example-com`），「一眼认出是哪页」的设计目的落空。hash 只依赖
            // 整条 URL，故与上面「`/a/b`」那行只差 slug。
            "https://example.com/a/b/",
            "b-7f51855e60c9eacb.md",
            "尾斜杠 → 仍是末段 `b`（空段在取值前滤掉，不得退回 host）",
        ),
        (
            "https://example.com/dir/",
            "dir-03d36f33ec8ad6cc.md",
            "单层目录 + 尾斜杠 → `dir`（尾斜杠 URL 极常见，不能退化成 host）",
        ),
        (
            "not a url at all",
            "not-a-url-at-all-8f374d117f258fba.md",
            "URL 解析失败 → 清整串（**绝不 panic、绝不空 slug**）",
        ),
    ];

    for (url, expect, why) in cases {
        assert_eq!(fetched_file_name(url), *expect, "{why}（url={url}）");
    }

    // slug 截到 40 字符（`SLUG_MAX`），但 hash 仍打整条 URL。
    let long = format!("https://example.com/{}", "x".repeat(80));
    let name = fetched_file_name(&long);
    assert_eq!(
        name,
        format!("{}-8cf30ece4969ae14.md", "x".repeat(40)),
        "{name}"
    );

    // slug 洗不出字母数字时的兜底（保证与 `sweep` 的命名模式始终匹配 ——
    // 空 slug 的文件名会被 `is_fetched_name` 拒收，落盘文件就永远清不掉）。
    assert_eq!(
        fetched_file_name("https://example.com/---"),
        "page-2eff7a32447b38dd.md",
        "末段全是非字母数字 → 兜底 slug 必须是 `page`"
    );
}

/// ⭐ **唯一 tmp 名 + rename**：多路并发写**同一个 dir、同一个 URL、不同内容**时，
/// 最终只能有 **1 个**目标文件、内容是**某一整份**（非截断、非混合）、**无 `.tmp` 残留**。
///
/// 三条断言各自钉一个后果：
///
/// - **全部 `Ok`**：共用一个 tmp 名时，先完成的一方 `rename` 走它，后完成的一方
///   `rename` 同一个 tmp → `ENOENT`（写失败）—— 这是本用例最灵敏的那一枪；
/// - **内容恰好等于某一整份**：共用一个 tmp 名时会读到「正在被写一半」的文件
///   （`rename` 不等 `write`），于是落盘的是**截断/混合**的内容；
/// - **无 `.tmp` 残留**：失败路径只清自己那一个 tmp，共名时会把别人的 tmp 删掉 /
///   留下孤儿。
///
/// 变异：`unique_tmp_path` 退回固定名（如 `dir.join(format!("{name}.tmp"))`）→ 本用例红。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_writes_to_one_url_leave_one_complete_file() {
    const N: usize = 48;

    let dir = TempDir::new("concurrent_same_url");
    let url = "https://example.com/a/b/";
    let expected = dir.path().join(fetched_file_name(url));

    // 48 份**长度互不相同**的内容：截断/混合几乎必然与每一份都不相等。
    let bodies: Vec<String> = (0..N)
        .map(|i| {
            let filler = char::from(b'a' + (i % 26) as u8);
            let mut s = format!("body-{i:04}\n");
            s.extend(std::iter::repeat_n(filler, 4096 + i * 131));
            s.push('\n');
            s
        })
        .collect();

    let mut tasks = Vec::with_capacity(N);
    for body in &bodies {
        let dir = dir.path().to_path_buf();
        let body = body.clone();
        tasks.push(tokio::spawn(async move {
            write_fetched(&dir, "https://example.com/a/b/", &body).await
        }));
    }

    let mut paths = Vec::with_capacity(N);
    for (i, task) in tasks.into_iter().enumerate() {
        match task.await.expect("任务不该 panic") {
            Ok(p) => paths.push(p),
            Err(e) => panic!("第 {i} 路并发写失败：{e}（共用一个 tmp 名时这里会是 ENOENT）"),
        }
    }

    assert!(
        paths.iter().all(|p| *p == expected),
        "同一 URL 必须落到同一个目标名"
    );

    let names = entry_names(dir.path());
    assert_eq!(
        names,
        vec![fetched_file_name(url)],
        "同一 URL 并发写之后目录里只能有 1 个文件（且无 `.tmp` 残留）"
    );

    let written = std::fs::read_to_string(&expected).expect("应能读回落盘结果");
    assert!(
        bodies.contains(&written),
        "落盘内容必须是**完整的一份**（长度 {}，非截断/非混合）",
        written.len()
    );
}

/// 上面那条的**跨 URL 对照**：并发写**同一个 dir、不同 URL** 时，各自落到各自的名字上，
/// 一样不留 `.tmp`。
///
/// 本用例**能**抓到：tmp 名里**根本没有目标名**的粗粒度变异（如 `dir.join("x.tmp")`）
/// —— 此时不同 URL 抢同一个 tmp，`rename` 必有一方 `ENOENT`，或读到对方写一半的内容。
///
/// 本用例**抓不到**：更细的「tmp 名含 name 但没有 pid/序号」（如 `{name}.tmp`）——
/// 实测该变异下本条**仍绿**，因为不同 URL → 不同 name → 不同 tmp 名。那一档由上面
/// [`test_concurrent_writes_to_one_url_leave_one_complete_file`] 抓（48 路抢**同一**
/// URL ⇒ 同一 name ⇒ 同一 tmp，必炸）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_writes_to_distinct_urls_do_not_collide() {
    let dir = TempDir::new("concurrent_distinct_urls");
    let urls: Vec<String> = (0..8)
        .map(|i| format!("https://example.com/page-{i}"))
        .collect();

    let mut tasks = Vec::new();
    for url in &urls {
        let dir = dir.path().to_path_buf();
        let url = url.clone();
        let body = format!("content for {url}");
        tasks.push(tokio::spawn(async move {
            write_fetched(&dir, &url, &body).await
        }));
    }
    for task in tasks {
        task.await.expect("任务不该 panic").expect("并发写应成功");
    }

    let mut expected: Vec<String> = urls.iter().map(|u| fetched_file_name(u)).collect();
    expected.sort();
    assert_eq!(entry_names(dir.path()), expected, "每个 URL 一份，互不覆盖");
    assert!(
        !entry_names(dir.path()).iter().any(|n| n.ends_with(".tmp")),
        "不得有 `.tmp` 残留"
    );
}

/// `write_fetched` 的**幂等**：同一 URL 重复抓取落到同一个文件（覆盖写，目录不膨胀），
/// 且目录不存在时**自建**（`Temp` 档可能被 OS 清掉，工具不该因此永久失效）。
#[tokio::test]
async fn test_write_fetched_is_idempotent_and_creates_dir() {
    let base = TempDir::new("idempotent");
    let dir = base.path().join("deep").join("nested");

    let url = "https://example.com/";
    let first = write_fetched(&dir, url, "first body")
        .await
        .expect("应能落盘");
    let second = write_fetched(&dir, url, "second body")
        .await
        .expect("应能落盘");

    assert_eq!(first, second, "同一 URL → 同一个目标名");
    assert_eq!(first, dir.join(fetched_file_name(url)));
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "second body");
    assert_eq!(entry_names(&dir), vec![fetched_file_name(url)]);
}

/// `sweep` 的**判据矩阵**：三条判据（名字模式 / 普通文件 / 超龄）**同时**满足才删。
///
/// 一条用例里同时放：命名模式的各种边界、`mtime` 的两档、用户自己的文件、目录、
/// 子目录里的同名文件、符号链接 —— 于是任何一条判据被削掉都会在这里变红。
///
/// 变异：把 `if !is_fetched_name(name) { continue; }` 去掉（或让它恒真）→
/// 「不该删的」全部被删 → 本用例红。
#[test]
fn test_sweep_criteria_matrix() {
    let dir = TempDir::new("sweep_matrix");

    // (名字, 是否**恰好**匹配 `<slug>-<16hex>.md` 这一条判据)
    //
    // ⚠️ 这里**不放**只差大小写的变体（`page-…ABCDEF.md` / `Page-…abcdef.md`）：
    // macOS 默认文件系统**大小写不敏感**，它们会与下面的小写名指向**同一个 inode**
    // （于是「大写不删」这条判据根本测不出来，还会误删小写的那个）。那两条判据
    // 在 [`test_sweep_rejects_case_variant_names`] 里用**独占目录**单独钉。
    const NAME_MATRIX: &[(&str, bool)] = &[
        ("page-0123456789abcdef.md", true),
        ("a-0123456789abcdef.md", true),
        ("my-long-slug-0123456789abcdef.md", true), // slug 自带 `-`：从**右**切
        ("-0123456789abcdef.md", false),            // 空 slug
        ("page-0123456789abcde.md", false),         // 15 位 hex
        ("page-0123456789abcdef0.md", false),       // 17 位 hex
        ("page-0123456789abcdeg.md", false),        // 非 hex 字符
        ("page_0123456789abcdef.md", false),        // 没有 `-`
        ("page-0123456789abcdef.txt", false),       // 非 `.md`
        ("page-0123456789abcdef.md.bak", false),    // 后缀不是 `.md`
        ("page.md", false),                         // 无 hash
        ("README.md", false),                       // 用户自己的文件
    ];

    let mut expected_removed: Vec<String> = Vec::new();
    let mut expected_bytes = 0u64;

    for (name, should_match) in NAME_MATRIX {
        let content = format!("<{name}>"); // 每个文件长度不同：bytes_freed 才有意义
        write_aged(dir.path(), name, &content, OLD);
        if *should_match {
            expected_removed.push((*name).to_string());
            expected_bytes += content.len() as u64;
        }
    }

    // slug 超长（41 > SLUG_MAX=40）→ 不匹配。
    let too_long = format!("{}-0123456789abcdef.md", "x".repeat(41));
    write_aged(dir.path(), &too_long, "long slug", OLD);

    // 匹配但**还新鲜** → 不删（mtime 判据）。
    let fresh = "fresh-0123456789abcdef.md";
    write_aged(dir.path(), fresh, "fresh body", Duration::from_secs(30));

    // 目录（哪怕名字匹配）→ 不删（`file_type().is_file()` 判据）。
    let dir_named_like_a_file = dir.path().join("subdir-0123456789abcdef.md");
    std::fs::create_dir_all(&dir_named_like_a_file).unwrap();

    // **不递归**：子目录里的同名超龄文件不归本目录的清扫管。
    let nested_dir = dir.path().join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    let nested = write_aged(&nested_dir, "inner-0123456789abcdef.md", "inner", OLD);

    // **不 follow 符号链接**：链到的外部文件不得被删、内容不得变。
    let outside = TempDir::new("sweep_outside");
    let outside_file = write_aged(
        outside.path(),
        "target-0123456789abcdef.md",
        "external",
        OLD,
    );
    #[cfg(unix)]
    let symlink_name = {
        let link = dir.path().join("link-0123456789abcdef.md");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        "link-0123456789abcdef.md"
    };

    let stats = sweep(dir.path(), MAX_AGE);

    expected_removed.sort();
    for removed in &expected_removed {
        assert!(
            !dir.path().join(removed).exists(),
            "`sweep` 没删掉该删的：{removed}"
        );
    }

    assert_eq!(
        stats.removed,
        expected_removed.len(),
        "删除数不对（多删或少删）：{stats:?}"
    );
    assert_eq!(
        stats.bytes_freed, expected_bytes,
        "bytes_freed 应是删除文件长度之和"
    );
    // `scanned` = 「名字匹配且是普通文件」的个数（含**新鲜**的那个，mtime 判据在其后）。
    assert_eq!(
        stats.scanned,
        expected_removed.len() + 1,
        "scanned 应是「名字匹配的普通文件」数（含新鲜的那个）：{stats:?}"
    );

    for name in [
        "page-0123456789abcde.md",
        "page-0123456789abcdef0.md",
        "page-0123456789abcdeg.md",
        "page_0123456789abcdef.md",
        "page-0123456789abcdef.txt",
        "page-0123456789abcdef.md.bak",
        "page.md",
        "-0123456789abcdef.md",
        "README.md",
        too_long.as_str(),
        fresh,
        "subdir-0123456789abcdef.md",
        #[cfg(unix)]
        symlink_name,
    ] {
        assert!(
            dir.path().join(name).exists(),
            "不该被删的文件被删了：{name}"
        );
    }
    assert!(
        nested.exists(),
        "**不递归**：子目录里的文件不归本目录清扫管"
    );
    assert_eq!(
        std::fs::read_to_string(&outside_file).unwrap(),
        "external",
        "符号链接的目标（目录外）不得被动过"
    );
}

/// 「**大写不删**」这两条判据（大写 hex / 大写 slug）**必须独占目录**。
///
/// macOS 默认文件系统（APFS）**大小写不敏感**：只差大小写的两个名字指向**同一个
/// inode** —— 放进同一个目录里，小写那份被删掉时大写那份会跟着消失，判据根本测不出来。
/// 独占目录之后，无论文件系统敏不敏感，目录里都只有一个名字，判据被真正验证。
///
/// 变异：`is_fetched_name` 的 hex 判据放宽成 `is_ascii_hexdigit()`（接受大写）→ 本用例红。
#[test]
fn test_sweep_rejects_case_variant_names() {
    for name in ["page-0123456789ABCDEF.md", "Page-0123456789abcdef.md"] {
        let dir = TempDir::new("sweep_case_variant");
        let victim = write_aged(dir.path(), name, "body", OLD);

        let stats = sweep(dir.path(), MAX_AGE);

        assert_eq!(
            stats.removed, 0,
            "`{name}` 不是本工具产出的形状，不许删：{stats:?}"
        );
        assert_eq!(stats.scanned, 0, "`{name}` 不该被算作「本工具的文件」");
        assert!(
            victim.exists(),
            "`{name}` 被删了 —— 大写不算本工具的命名模式"
        );
    }
}

/// `sweep` 是 **best-effort**：目录不存在就**静默**返回空统计（绝不 panic、绝不报错）。
///
/// 清理是**附带动作**，它永远不该成为一次抓取失败的原因。
#[test]
fn test_sweep_missing_dir_is_silent() {
    let base = TempDir::new("sweep_missing");
    let missing = base.path().join("does-not-exist");

    let stats = sweep(&missing, MAX_AGE);
    assert_eq!(stats, SweepStats::default());
    assert_eq!(stats.scanned, 0);
    assert_eq!(stats.removed, 0);
    assert_eq!(stats.bytes_freed, 0);
}

/// `now - max_age` **下溢**（`max_age` 超出 `SystemTime` 的表示范围）→ **一个都不删**，
/// 而不是把阈值当成 UNIX 纪元（那会把所有文件都判为「超龄」并删光）。
#[test]
fn test_sweep_max_age_underflow_removes_nothing() {
    let dir = TempDir::new("sweep_underflow");
    let victim = write_aged(dir.path(), "page-0123456789abcdef.md", "body", OLD);

    let stats = sweep(dir.path(), Duration::MAX);

    assert_eq!(
        stats,
        SweepStats::default(),
        "下溢必须原样返回（早退），不得把阈值当成纪元"
    );
    assert!(victim.exists(), "下溢时一个文件都不许删");
}

/// 「清理只对 `Primary` 档生效」是**类型上的保证**（[`FetchedDir::is_cleanable`]）——
/// 用户显式给的目录（`Override`）**只写不删**。
///
/// 变异：`is_cleanable` 改成 `matches!(self, Self::Primary(_) | Self::Override(_))`
/// → 本用例红。
#[test]
fn test_only_primary_tier_is_cleanable() {
    let p = PathBuf::from("/abs/fetched");
    let tiers = [
        (
            FetchedDir::Override(p.clone()),
            false,
            "用户显式指定 → 只写不删",
        ),
        (
            FetchedDir::Primary(p.clone()),
            true,
            "主目录 → 唯一可清理的一档",
        ),
        (FetchedDir::Temp(p.clone()), false, "临时目录 → 只写不删"),
        (FetchedDir::None, false, "不落盘 → 无从清理"),
    ];
    for (tier, cleanable, why) in tiers {
        assert_eq!(tier.is_cleanable(), cleanable, "{tier:?}：{why}");
    }
}
