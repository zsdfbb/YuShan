//! **发布契约测试套**（设计 `docs/arch/tools-web/design.md` §1.8②）。
//!
//! 这里是「**每次改动都必须过**」的那一组断言。它们钉的不是内部实现，而是
//! **对外承诺**：env 变量名、工具名、`description` 的关键语义、以及「本 crate
//! 绝不 print」这条硬约束。破坏其中任何一条 = **破坏兼容**，故值得一次显式评审。
//!
//! # 与既有测试的分工（**不重复**）
//!
//! 设计 §1.8② 的契约表里，多数条目**已有测试**，本文件只补**缺口**：
//!
//! | 契约 | 现有覆盖位置 |
//! |---|---|
//! | 工具数 9 + 装配顺序 `EXPECTED_ORDER` | `apps/coding-agent/src/toolset.rs`（3 条） |
//! | 落盘文件名形状 `<slug>-<16hex>.md` | `tests/convert_store.rs::test_fetched_file_name_matches_independent_fnv_reference` |
//! | `offset` 与文件行号 1:1 / `read` 穿透绝对路径 | `tests/tools.rs::test_fetch_head_lines_match_saved_file_and_read_continues_at_the_same_line` |
//! | 落盘目录解析必为绝对路径 / 不可用不写 | `src/lib.rs` 单测 + `tests/tools.rs` 降级用例 |
//! | 清理不碰 `Override` 档（档位语义） | `src/lib.rs::test_fetched_dir_accessors` + `tests/convert_store.rs::test_only_primary_tier_is_cleanable` |
//! | prompt snippet 的 web 两条 | `apps/coding-agent/src/prompt.rs::search_list_and_web_snippets_are_verbatim` |
//!
//! 本文件**新增**的只有：env 变量名快照 · 两个工具名/`description` 的 crate 本地钉死 ·
//! 源码无 print 扫描（`toolset.rs` 只扫了它自己，**没扫本 crate 的 8 个源文件**）。

use ys_tool::Tool;
use ys_tools_web::{
    ENV_BOCHA_KEY, ENV_FETCHED_DIR, ENV_FIRECRAWL_KEY, ENV_SEARXNG_URL, ENV_TAVILY_KEY,
    ENV_TAVILY_KEY_ALIAS, FetchedDir, Kind, WebConfig, WebFetchTool, WebSearchTool,
    resolve_backends,
};

/// 确定性配置：无 HTTP 栈、无后端、不落盘。
///
/// 本文件只读 `spec()`（纯数据），**绝不发请求**，故 `offline` 足够 ——
/// 且刻意**不读本机 env**（`from_env` 会让断言随机器漂移）。
fn offline_cfg() -> WebConfig {
    WebConfig::offline(FetchedDir::None)
}

/// **契约：env 变量名是「发布即契约」**（设计 §1.8④「命名一次定死」）。
///
/// 用户把 `YUSHAN_SEARCH_TAVILY_KEY` 写进 shell profile / CI secret / 文档里。
/// 一旦改名，用户的既有配置**静默失效**（工具照样装，只是链为空 → 搜索不可用），
/// 且没有任何编译错误提示我们 —— 只有这条快照式断言会在改名的**当次**变红。
///
/// 变异：把 `src/lib.rs` 顶部任一 `ENV_*` 常量的字面量改一个字符（如
/// `YUSHAN_SEARCH_TAVILY_KEY` → `YUSHAN_SEARCH_TAVILY_TOKEN`）→ 本用例红。
#[test]
fn env_var_names_are_a_frozen_release_contract() {
    assert_eq!(
        ENV_SEARXNG_URL, "YUSHAN_SEARCH_SEARXNG_URL",
        "SearXNG 实例地址的 env 名是发布契约，改名 = 破坏兼容"
    );
    assert_eq!(
        ENV_TAVILY_KEY, "YUSHAN_SEARCH_TAVILY_KEY",
        "Tavily key 的 env 名是发布契约，改名 = 破坏兼容"
    );
    assert_eq!(
        ENV_TAVILY_KEY_ALIAS, "TAVILY_API_KEY",
        "Tavily 通用别名同样是发布契约：改名 = 已按通用名配好的用户静默失效"
    );
    assert_eq!(
        ENV_BOCHA_KEY, "YUSHAN_SEARCH_BOCHA_KEY",
        "Bocha key 的 env 名是发布契约，改名 = 破坏兼容"
    );
    assert_eq!(
        ENV_FIRECRAWL_KEY, "YUSHAN_SEARCH_FIRECRAWL_KEY",
        "Firecrawl key 的 env 名是发布契约，改名 = 破坏兼容"
    );
    assert_eq!(
        ENV_FETCHED_DIR, "YUSHAN_FETCHED_DIR",
        "落盘目录覆盖的 env 名是发布契约，改名 = 破坏兼容（且测试隔离会静默失效）"
    );
}

/// **契约：Tavily 的通用名是回退，不是替代** —— 只配 `TAVILY_API_KEY` 也能进链；
/// 两个都配时 [`ENV_TAVILY_KEY`]（项目命名空间）优先；两个都不配仍被剔除。
///
/// 用 `resolve_backends` 的**注入式 env 闭包**确定性构造链，不改进程 env。
///
/// 变异：把 `TAVILY_KEY_ENVS` 的顺序反过来 → 「项目名优先」那条红；把包含
/// `ENV_TAVILY_KEY_ALIAS` 从表里去掉 → 第一条红；把 `keyless` 打开 → 第三条红。
#[test]
fn tavily_generic_env_is_a_fallback_and_project_name_wins() {
    let chain = |env: &[(&str, &str)]| {
        let map: std::collections::HashMap<&str, &str> = env.iter().copied().collect();
        resolve_backends(|k| map.get(k).map(|v| v.to_string()))
    };

    let only_generic = chain(&[(ENV_TAVILY_KEY_ALIAS, "tvly-generic")]);
    assert_eq!(only_generic.len(), 1, "{only_generic:?}");
    assert_eq!(only_generic[0].kind, Kind::Tavily);
    assert_eq!(only_generic[0].key.as_deref(), Some("tvly-generic"));

    let both = chain(&[
        (ENV_TAVILY_KEY, "tvly-project"),
        (ENV_TAVILY_KEY_ALIAS, "tvly-generic"),
    ]);
    assert_eq!(both.len(), 1, "{both:?}");
    assert_eq!(
        both[0].key.as_deref(),
        Some("tvly-project"),
        "项目命名空间必须压过通用名"
    );

    assert!(
        chain(&[]).is_empty(),
        "两个名字都没配 ⇒ Tavily 按无 key 剔除（别名不得变成「无 key 也进链」）"
    );
}

/// **契约：两个工具名逐字**（`websearch` / `webfetch`）。
///
/// 这两个字符串**同时**被 `apps/coding-agent/src/prompt.rs::tool_snippet` 的 match 臂
/// 依赖（那里逐字写着 `"websearch"` / `"webfetch"`）。改名 → prompt 的行内 snippet
/// 静默退化到兜底 `"Custom tool"`：**模型看到的工具描述变空**，编译与既有测试全绿。
///
/// > 既有覆盖：`toolset.rs` 的 `EXPECTED_ORDER` 断言**间接**钉住了这两个名字
/// > （装配顺序里含它们）。本条的价值是**在定义它们的 crate 里**单独钉一次 ——
/// > 只跑 `cargo test -p ys-tools-web` 也能挡住改名，不必依赖 app 层。
///
/// 变异：把 `search.rs` / `fetch.rs` 的 `ToolSpec::new("websearch", …)` 改成别的名字
/// → 本用例红（app 侧的 snippet 退化**不会**让它红，正是本条存在的理由）。
#[test]
fn web_tool_names_are_the_release_contract() {
    let search = WebSearchTool::new(offline_cfg());
    let fetch = WebFetchTool::new(offline_cfg());

    assert_eq!(
        search.spec().name,
        "websearch",
        "工具名逐字契约；`prompt.rs::tool_snippet` 靠它命中专属文案"
    );
    assert_eq!(
        fetch.spec().name,
        "webfetch",
        "工具名逐字契约；`prompt.rs::tool_snippet` 靠它命中专属文案"
    );
}

/// **契约：`description` 的关键语义**（设计 §1.8⑤「文案即行为」）。
///
/// `description` **直接进 `ModelRequest.tools`**，是模型行为的输入 —— 改文案等于改行为。
/// 这里只锁**关键短语**（不锁整段）：文案微调（换词、断行）不该红，但**语义漂移**
/// （链变随机、落盘被拿掉、摘要能力被静默删掉）必须红。
///
/// 变异：
/// ① `search.rs` 的 description 把 "fixed order" 换成 "a randomized order" → 本用例红；
/// ② `fetch.rs` 的 description 删掉「saved / path」那两句（暗指「不再落盘」）→ 本用例红；
/// ③ `fetch.rs` 的 description 删掉 `prompt` / `summary` 那段（可选摘要静默消失）→ 本用例红。
#[test]
fn descriptions_pin_chain_and_save_semantics() {
    let search_desc = WebSearchTool::new(offline_cfg()).spec().description;
    let fetch_desc = WebFetchTool::new(offline_cfg()).spec().description;

    // websearch：链是**确定的**（固定顺序）—— 与 §1.8①「顺序完全由常量表决定」一致。
    // 模型若读到「随机/竞速」，就可能对同一 query 反复重试碰运气。
    assert!(
        search_desc.contains("fixed order"),
        "websearch 的 description 须说明后端按**固定顺序**尝试（链确定即契约）：{search_desc}"
    );

    // webfetch：**落盘 + 返回指针**（ADR-0016 的核心决策）。
    // 这两条短语是「正文不随响应返回、而在本机文件里」的唯一线索；缺了模型会以为
    // 响应就是全文，进而漏读被截断的部分。
    assert!(
        fetch_desc.contains("saved"),
        "webfetch 的 description 须说明内容被**保存**到文件：{fetch_desc}"
    );
    assert!(
        fetch_desc.contains("path"),
        "webfetch 的 description 须给出**路径**（指针）以便续读：{fetch_desc}"
    );

    // webfetch：**可选 `prompt`（摘要）**—— 这是「多付一次模型调用」这个代价的**唯一**出口：
    // description 是模型判断「要不要为这次抓取多花一轮」的全部依据，缺了它模型不知道
    // 有这个能力（功能等于不存在），或者用了却不知道它更慢。
    assert!(
        fetch_desc.contains("prompt"),
        "webfetch 的 description 须说明**可选 prompt**（否则模型不知道有摘要能力）：{fetch_desc}"
    );
    assert!(
        fetch_desc.contains("summary"),
        "webfetch 的 description 须说明 prompt 会给出 **summary**（而不是别的效果）：{fetch_desc}"
    );
}

/// **契约：`prompt` 是可选参数**（设计 §2.4 / N11）—— schema 里必须**有**它，
/// 但 `required` 里**不许**有它。
///
/// 两半缺一不可：漏掉 schema 会让模型**根本传不出** `prompt`（能力静默消失）；
/// 误进 `required` 则把「可选」变成「必填」，于是**每一次**抓取都多花一轮模型调用
/// （设计 §7 风险 4：模型很会把「可选」用成「必选」）。
///
/// 变异：删掉 `properties.prompt` → 红；把 `"prompt"` 塞进 `required` → 红。
#[test]
fn webfetch_schema_exposes_an_optional_prompt() {
    let spec = WebFetchTool::new(offline_cfg()).spec();
    let params = &spec.parameters;

    let props = params["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("`properties` 必须是对象：{params}"));
    assert!(props.contains_key("url"), "{params}");
    assert!(
        props.contains_key("prompt"),
        "schema 必须**暴露**可选 prompt（否则模型传不出来）：{params}"
    );
    assert_eq!(
        props["prompt"]["type"], "string",
        "prompt 是字符串（摘要是自然语言的提问）：{params}"
    );

    let required = params["required"]
        .as_array()
        .unwrap_or_else(|| panic!("`required` 必须是数组：{params}"));
    assert_eq!(
        required.len(),
        1,
        "`prompt` 是**可选**的，不得进 required（否则每次抓取都要多花一轮模型调用）：{required:?}"
    );
    assert_eq!(required[0], "url", "{required:?}");
}

/// **契约：本 crate 的 8 个源文件都不得出现打印宏**（设计 §1.1 硬约束）。
///
/// 「本 crate 绝不 print」是输出三分的前提：库只**产出文案**，去处（stderr /
/// `Outbound` / 日志文件）由 app 层决定。一旦某处直接 `eprintln!`，TUI 起屏后
/// 它会冲掉整屏 —— 而这类改动**编译过、既有测试全绿**。
///
/// ⚠️ 写法照抄 `apps/coding-agent/src/toolset.rs:381` 的同名守卫：
/// - `include_str!` 相对**当前文件**解析 → 扫的是真实随包编译的源码，不是 CWD 下的同名文件；
/// - 针脚用 `concat!` 拼出 → 本文件源码里不出现 `println!` 字面量，否则**扫描会扫到自己**而恒红。
///
/// 变异：在 `src/fetch.rs` 临时插一句 `eprintln!("x")` → 本用例红。
#[test]
fn source_files_contain_no_print_macros() {
    let sources: [(&str, &str); 8] = [
        ("lib.rs", include_str!("../src/lib.rs")),
        ("net.rs", include_str!("../src/net.rs")),
        ("guard.rs", include_str!("../src/guard.rs")),
        ("convert.rs", include_str!("../src/convert.rs")),
        ("store.rs", include_str!("../src/store.rs")),
        ("backends.rs", include_str!("../src/backends.rs")),
        ("search.rs", include_str!("../src/search.rs")),
        ("fetch.rs", include_str!("../src/fetch.rs")),
    ];
    let needles = [
        concat!("print", "ln!"),
        concat!("eprint", "ln!"),
        concat!("dbg", "!"),
    ];

    for (file, src) in sources {
        for needle in needles {
            assert!(
                !src.contains(needle),
                "{file} 不得出现 {needle}：本 crate 只产出文案，打印归 app 层（输出三分）"
            );
        }
    }
}
