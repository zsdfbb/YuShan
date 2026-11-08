//! 工具装配的**唯一接缝**：启动期探测一次后端 → 折平插件声明成有序工具表 → 暴露降级提示。
//!
//! # 为什么收拢成一个模块
//!
//! 工具集的「有哪些工具 / 什么顺序 / 探测到什么后端」在三个地方被消费：
//! 注册进 `ToolRegistry`（`spec()` 顺序由此而定）、系统提示词工具清单、
//! 以及启动期给用户的后端降级提示。若三处各自装配，顺序与探测结果会漂移
//! （典型症状：提示词里列了 `grep`，注册表里没有；或两次探测结果不一致）。
//!
//! # 装配 = 插件声明
//!
//! 工具面由 [`builtin_plugins`] 的 **11 条声明**给出，每条一个工具，`order` 是编译期常量。
//! [`build_tools`] 只做两件事：按 `order` 折表（[`ys_tool::sort_plugins`]）+ 折平成
//! `Vec<Box<dyn Tool>>`。加一个工具的连锁点因此收敛到**一处声明**，顺序不再有第二份手抄。
//!
//! 顺序真值有**两份、故意不合并**：装配端（声明里的 `order`）与测试端
//! （`EXPECTED_ORDER_FOR_TESTS` 的字面量）。两份才能交叉校验 —— 测试若引用生产派生量，
//! 「顺序变了」将没有任何断言能发现。
//!
//! # 探测只做一次
//!
//! [`SearchBackends::detect`] 是**纯路径扫描**（不 fork、不跑 `--version`，见 ADR-0015），
//! 但在语义上仍是「启动期一次」的事：同一进程内两次探测可能给出不同答案（PATH 被改），
//! 且 `grep` / `find` 与降级提示必须看到**同一份**后端认知。故 [`build_tools`] 探测一次，
//! 把结果传给声明函数；[`builtin_plugins`] 单独调用时也只探测一次（`grep` 拿 clone、
//! `find` 拿走本体）。
//!
//! # 不 print
//!
//! 与 `capabilities.rs` 同规：本模块只**产出文案**，打印由 `main` 在启动期做
//! （输出三分，见 `docs/design-final/coding-agent-tui.md`）。

use std::path::Path;

use ys_tool::{Tool, ToolPlugin, sort_plugins};
use ys_tools_basic::exec::SearchBackends;
use ys_tools_basic::{
    BashTool, EditTool, FindTool, GrepTool, LsTool, OcrTool, ReadTool, ViewImageTool, WriteTool,
};
use ys_tools_web::{WebConfig, WebFetchTool, WebSearchTool};

/// 装配顺序的**派生量**：`builtin_plugins()` 按 `order` 升序折表后的工具名序列。
///
/// 这是生产路径的**唯一**顺序来源（`ToolRegistry::specs()` / 系统提示词工具清单 /
/// `agent.tool_names()` 三者同源）。它由声明派生，**不得**手工维护。
///
/// ⚠️ **测试不得引用本函数**：交叉校验靠「装配端一份真值 + 测试端一份独立字面量」——
/// 测试侧用 `EXPECTED_ORDER_FOR_TESTS`。若测试也读这里，「顺序变了」就没有任何
/// 断言能发现（两份真值退化成一份）。
///
/// 目前仅测试消费；生产路径由 [`build_tools`] 就地折平（它要拿同一份探测结果出提示）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn builtin_order(workspace: &Path, web: WebConfig) -> Vec<String> {
    let plugins = sort_plugins(builtin_plugins(workspace, web))
        .expect("内置 11 条声明必须通过 sort_plugins 的三条声明期校验");
    plugins
        .iter()
        .flat_map(|plugin| plugin.tools.iter().map(|tool| tool.spec().name))
        .collect()
}

/// 内置工具面的**唯一声明**：11 条，每条一个工具。
///
/// `order` 是**编译期常量**且互不相同 —— 它决定 `tools()` 与 tools JSON 的顺序，
/// 一变就让 prompt 缓存前缀失效（不得来自 env / 登录态 / 探测结果）。
///
/// 顺序（即 `order` 升序）**固定为**：
///
/// ```text
/// read, write, edit, bash, grep, find, ls, ocr, view_image, websearch, webfetch
/// ```
///
/// 即「读写改 → 执行 → 检索 → 列目录 → 读图 → 联网」：模型最常用的文件读写与 shell 排在最前，
/// **带网络的两个工具排在最后**（本机能力优先；联网是外挂能力，见 `tools-web` 文档）。
/// **不要**随意重排（会改动 prompt 文本与测试基线）。
///
/// # 工具恒注册
///
/// `web` 里的两个工具**恒进工具表**，绝不按登录态 / 是否配了搜索后端增减：
/// 工具面一旦会变，模型会把「工具消失」误读成「我记错了」。降级一律发生在
/// **调用期**（明确错误结果），而非从工具表里摘掉（设计 §1.8①）。
///
/// # 无副作用
///
/// 本函数（与 [`WebConfig::from_env`]）**绝不清理落盘目录** —— 清理由 `main`
/// 在装配序列里显式调，且只对 `FetchedDir::Primary` 生效。否则本函数的测试
/// 会删掉用户真实的 `~/.yushan/fetched`（设计 §1.1 硬约束）。
///
/// 目前仅测试消费声明本身；生产路径走 [`build_tools`]（它必须自己探测一次，
/// 以便同一份 [`SearchBackends`] 既建表又出降级提示）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn builtin_plugins(workspace: &Path, web: WebConfig) -> Vec<ToolPlugin> {
    builtin_plugins_from(workspace, web, SearchBackends::detect())
}

/// [`builtin_plugins`] 的本体：后端认知由调用方传入。
///
/// 抽出这一层是为了让「探测一次」在 [`build_tools`] 里可被**机械保证** —— 它既要建表
/// 又要出降级提示，两者必须看到同一份 [`SearchBackends`]。
fn builtin_plugins_from(
    workspace: &Path,
    web: WebConfig,
    backends: SearchBackends,
) -> Vec<ToolPlugin> {
    let ws = workspace.to_path_buf();

    vec![
        ToolPlugin::new(0, vec![Box::new(ReadTool::new(ws.clone()))]),
        ToolPlugin::new(1, vec![Box::new(WriteTool::new(ws.clone()))]),
        ToolPlugin::new(2, vec![Box::new(EditTool::new(ws.clone()))]),
        ToolPlugin::new(3, vec![Box::new(BashTool::new(ws.clone()))]),
        // `grep` 拿 clone、`find` 拿走本体：同一次探测的后端认知，两工具不得分歧。
        ToolPlugin::new(
            4,
            vec![Box::new(GrepTool::new(ws.clone(), backends.clone()))],
        ),
        ToolPlugin::new(5, vec![Box::new(FindTool::new(ws.clone(), backends))]),
        ToolPlugin::new(6, vec![Box::new(LsTool::new(ws.clone()))]),
        // `ocr` 是**只读本地输入**（不写 workspace，落盘只在 `<temp>/yushan-ocr/`）⇒
        // 与文件类工具同组，紧跟 `ls` 之后；带网络的两个工具仍留在最后（本机能力优先）。
        ToolPlugin::new(7, vec![Box::new(OcrTool::new().with_workspace(ws.clone()))]),
        // `view_image` 同样只读（只读文件头 + `metadata`，不写任何位置）⇒ 紧跟 `ocr`：
        // 两者是同一件事的两面（`ocr` 给文字与坐标，它给像素），相邻便于模型读清单。
        ToolPlugin::new(
            8,
            vec![Box::new(
                ViewImageTool::default().with_workspace(ws.clone()),
            )],
        ),
        // `WebConfig` 是 `Clone` 的值（`Client` 内含 `Arc`，克隆只复制指针）：
        // 两个工具各持一份，连接池与 UA 策略仍是同一套。
        ToolPlugin::new(9, vec![Box::new(WebSearchTool::new(web.clone()))]),
        ToolPlugin::new(10, vec![Box::new(WebFetchTool::new(web))]),
    ]
    .into_iter()
    .map(with_prompt_declaration)
    .collect()
}

/// 把 [`crate::tool_prompts`] 里该工具的 snippet 与 guideline 块挂到插件上。
///
/// 两条清单**故意分处两个模块**：插件声明管「有哪些工具、什么顺序」，`tool_prompts` 管
/// 「它们各自对固定前缀说什么」。本函数是两者唯一的接合点 —— 声明里没有对应条目时
/// 插件照旧（第三方工具不必声明 snippet，清单会退化为 `Custom tool`）。
fn with_prompt_declaration(plugin: ToolPlugin) -> ToolPlugin {
    let declared: Vec<ys_tool::ToolPrompt> = plugin
        .tools
        .iter()
        .filter_map(|tool| crate::tool_prompts::prompt_for(&tool.spec().name).cloned())
        .collect();
    plugin.with_prompts(declared)
}

/// 探测一次搜索后端，把插件声明折平成**有序**工具表；一并交还降级提示。
///
/// 顺序由 [`builtin_plugins`] 的 `order` 给出（经 `sort_plugins` 升序折表），
/// 该顺序 **就是**注册顺序，决定 `ToolRegistry::specs()` 的顺序，进而决定系统提示词里
/// 工具清单的顺序。顺序断言与指纹锚在测试侧用**独立的** `EXPECTED_ORDER_FOR_TESTS`。
///
/// # 降级提示
///
/// 第二个返回值 **合并**了两条来源（**绝不互相覆盖**）：
/// ① [`SearchBackends::degraded_hint`]（`rg` / `fd` 缺失）；
/// ② [`WebConfig::degraded_hint`]（HTTP 栈不可用 / 未配搜索后端 / 落盘目录降级）。
/// 两者都非空时按 `\n` 拼接，仅一侧非空时返回该条，都为空才 `None`。
/// 本模块**不**另写文案、**不**打印 —— 何时打印、打到哪由 `main` 决定
/// （启动期 → stderr）。
pub fn build_tools(workspace: &Path, web: WebConfig) -> (Vec<Box<dyn Tool>>, Option<String>) {
    // 探测一次：同一份 `backends` 既进 `grep` / `find`，又出降级提示。
    let backends = SearchBackends::detect();
    let hint = merge_hints(backends.degraded_hint(), web.degraded_hint());

    let plugins = sort_plugins(builtin_plugins_from(workspace, web, backends))
        .expect("内置 11 条声明必须通过 sort_plugins 的三条声明期校验");
    let tools: Vec<Box<dyn Tool>> = plugins
        .into_iter()
        .flat_map(|plugin| plugin.tools)
        .collect();

    (tools, hint)
}

/// 合并两条启动期降级提示（搜索后端 + web 配置）—— **绝不互相覆盖**。
///
/// 两条来源彼此独立（缺 `rg`/`fd` 与缺搜索后端是两回事），故必须**同时**可见：
/// 早先用「后取者覆盖先取者」会让用户只看到一条，另一条静默丢失。
///
/// 变异：把 `(Some, Some)` 分支改成只保留一条（覆盖另一条）→
/// `both_hints_are_merged_not_overwritten` 变红。
fn merge_hints(search: Option<String>, web: Option<String>) -> Option<String> {
    match (search, web) {
        (Some(search), Some(web)) => Some(format!("{search}\n{web}")),
        (Some(search), None) => Some(search),
        (None, Some(web)) => Some(web),
        (None, None) => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use ys_runtime::AgentBuilder;
    use ys_tool::AutoApprove;
    use ys_tools_basic::{BackgroundTasks, BgLimits, SessionSlot};
    use ys_tools_web::{Backend, FetchedDir, Kind};

    use crate::test_env::{EnvRestore, env_lock};

    /// **测试侧的独立顺序真值**（硬编码，**不引用** `builtin_plugins()` / `builtin_order()`）。
    ///
    /// 装配端与测试端保留两份真值，才能交叉校验：装配端的 `order` 被改乱、或被改成
    /// 从探测 / env 派生时，本字面量不动 ⇒ 顺序断言变红。若本处也改引生产派生量，
    /// 「顺序变了」这件事就没有任何断言能发现（两份真值退化成一份）。
    ///
    /// `assembly.rs` / `prompt.rs` 的测试也引用这一份 —— 它们不各自另抄一份。
    pub(crate) const EXPECTED_ORDER_FOR_TESTS: &[&str] = &[
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "find",
        "ls",
        "ocr",
        "view_image",
        "websearch",
        "webfetch",
    ];

    /// [`EXPECTED_ORDER_FOR_TESTS`] 的 owned 形式（`spec().name` / `tool_names()` 都是 `String`）。
    pub(crate) fn expected_order_for_tests() -> Vec<String> {
        EXPECTED_ORDER_FOR_TESTS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// **确定性** `WebConfig`：无搜索后端、无 HTTP 栈（`offline`），落盘目录走 `Temp` 档。
    ///
    /// 测试**绝不**用 `WebConfig::from_env()` —— 那会读本机 env（有没有配
    /// `YUSHAN_SEARCH_*` / `YUSHAN_FETCHED_DIR`），结果随机器漂移：CI 上绿、
    /// 开发机上红，且红得莫名其妙。`offline` 的提示文案是**固定**的。
    ///
    /// 用 `Temp` 档而非 `Primary`：`Primary` 指向真实 `~/.yushan/fetched`，
    /// 构造它（`resolve_fetched_dir`）会创建用户目录。这里只走显式注入的路径。
    fn offline_web() -> WebConfig {
        WebConfig::offline(FetchedDir::Temp(unique_dir("web")))
    }

    /// **确定性**「已配置」`WebConfig`：HTTP 栈可用 + 链非空 → `degraded_hint()` 恒 `None`。
    ///
    /// 用来把「无提示」的判据从「仅搜索后端齐全」升级到「搜索后端齐全 **且** web 侧无问题」——
    /// 合并后只要任一侧有问题就会有提示。
    ///
    /// `Client::new()` 只建配置不发请求（本用例也不调工具）；`Backend` 的 endpoint
    /// 与 key 均为占位值，**不被消费**（只用于让 `backends` 非空）。
    fn configured_web() -> WebConfig {
        WebConfig::with_client(
            reqwest::Client::new(),
            FetchedDir::Temp(unique_dir("web_client")),
        )
        .with_backends(vec![Backend {
            kind: Kind::Searxng,
            endpoint: "http://127.0.0.1:9/".to_string(),
            key: None,
        }])
    }

    /// 进程内唯一临时目录：**原子序号 + `process::id()`**。
    ///
    /// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删（血的教训）。
    fn unique_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_toolset_{tag}_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 把进程 `PATH` 指向 `dir`，返回 Drop 时恢复原值的 guard。
    ///
    /// 调用方必须先持有 [`env_lock`]（与 `config.rs::clear_api_env` 同规）。
    fn set_path(dir: &Path) -> EnvRestore {
        let env = EnvRestore::capture(&["PATH"]);
        // SAFETY: 调用方持有 env_lock，独占 PATH 的修改与恢复。
        unsafe {
            std::env::set_var("PATH", dir);
        }
        env
    }

    /// 写一个「能被探测认出」的假后端：带 shebang 的可执行文件。
    ///
    /// 探测是**纯路径扫描**（不 spawn），故内容不必真能跑 —— 但这正是要钉住的语义：
    /// 假脚本绝不能被真的执行（若哪天探测改成 fork `--version`，本测试会因脚本
    /// 无实际功能而暴露，故脚本保持最简）。
    #[cfg(unix)]
    fn write_fake_backend(dir: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 1. `build_tools` 返回恰好 **11** 个工具，且 name 序列**逐字**等于固定顺序。
    ///
    /// 变异 1：`toolset.rs` 里把 `grep` / `find` 互换 → 本用例变红。
    /// 变异 2：`builtin_plugins()` 少写一个工具（或与测试侧硬编码序不一致）→ 本用例变红。
    #[test]
    fn build_tools_returns_fixed_order_of_eleven() {
        let _guard = env_lock();
        let dir = unique_dir("order");

        let (tools, _hint) = build_tools(&dir, offline_web());
        assert_eq!(tools.len(), 11, "工具表应为 11 个");

        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(
            names,
            expected_order_for_tests(),
            "装配顺序即语义（决定 prompt 工具清单顺序）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1b. `builtin_plugins()` 产出 **11 条声明**（每条一个工具），折平后名字序
    ///     逐字等于**测试侧硬编码**的 [`EXPECTED_ORDER_FOR_TESTS`]。
    ///
    /// 与用例 1 的分工：1 走 [`build_tools`]（含折平），本用例直看**声明本身** ——
    /// 断言的是「一条声明一个工具」与 `order` 一一对应，不经 `sort_plugins` 的排序。
    ///
    /// 变异 1：把两个工具塞进同一条声明 ⇒ 条数断言变红。
    /// 变异 2：调换任意两条声明的 `order` ⇒ 名字序断言变红。
    /// 变异 3：删掉一条声明 ⇒ 条数断言变红（`len == 11`）。
    #[test]
    fn builtin_plugins_declares_eleven_in_test_side_order() {
        let dir = unique_dir("plugins_decl");

        let plugins = builtin_plugins(&dir, offline_web());
        assert_eq!(plugins.len(), 11, "内置工具面应为 11 条声明");
        assert!(
            plugins.iter().all(|plugin| plugin.tools.len() == 1),
            "一条声明一个工具（顺序真值才能与工具名一一对应）"
        );

        let names: Vec<String> = plugins
            .iter()
            .flat_map(|plugin| plugin.tools.iter().map(|tool| tool.spec().name))
            .collect();
        assert_eq!(
            names,
            expected_order_for_tests(),
            "声明的 order 升序必须逐字等于测试侧的独立硬编码序"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1c. **交叉校验：装配端派生量 == 测试端硬编码序**。
    ///
    /// 这是「两份真值」得以互证的那条缝：`builtin_order` 由声明的 `order` 派生，
    /// 本用例把它与独立字面量逐项对账。两者都指向同一顺序时才算对齐。
    ///
    /// 变异：把任意两条声明的 `order` 对调 ⇒ 派生量变、字面量不变 ⇒ 变红。
    #[test]
    fn derived_order_matches_test_side_literal() {
        let dir = unique_dir("plugins_derived");

        assert_eq!(
            builtin_order(&dir, offline_web()),
            expected_order_for_tests(),
            "装配端派生量必须逐字等于测试侧的独立硬编码序"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1d. **打乱传入顺序 ⇒ 折表结果不变**：`sort_plugins` 按 `order` 升序重排，
    ///     与声明先后无关。
    ///
    /// 反向传入比随机打乱更强：它把**每一条**声明都移到了与 `order` 相反的位置，
    /// 故任何「按传入顺序折表」的实现必红。
    ///
    /// 先断言反向序列**确实**不是期望序（否则下面的正向断言平凡通过）。
    ///
    /// 变异：`sort_plugins` 里去掉 `sort_by_key(|p| p.order)` ⇒ 名字序变成反向 ⇒ 变红。
    #[test]
    fn sort_plugins_folds_reversed_declarations_back_to_order() {
        let dir = unique_dir("plugins_sort");

        let mut plugins = builtin_plugins(&dir, offline_web());
        let mut reversed_names: Vec<String> = plugins
            .iter()
            .flat_map(|plugin| plugin.tools.iter().map(|tool| tool.spec().name))
            .collect();
        reversed_names.reverse();
        assert_ne!(
            reversed_names,
            expected_order_for_tests(),
            "反向序列必须与期望序不同，否则本用例无法发现「不排序」"
        );

        plugins.reverse();
        let sorted =
            sort_plugins(plugins).expect("11 条声明互不重名、order 唯一、无 guideline slot");
        let names: Vec<String> = sorted
            .iter()
            .flat_map(|plugin| plugin.tools.iter().map(|tool| tool.spec().name))
            .collect();
        assert_eq!(
            names,
            expected_order_for_tests(),
            "折表结果由 order 决定，与传入顺序无关"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2. **顺序穿透到 agent**：`build_tools` → `AgentBuilder::tools` →
    ///    `agent.tool_names()` 同为该 11 元序列。
    ///
    ///    钉住「顺序经 `ToolRegistry` 到达 agent」这条承重链路 —— 注册表若改成
    ///    `HashMap`/排序容器，`spec()` 顺序会静默丢失，而 1 只看 `build_tools`
    ///    的返回值，抓不到。
    ///
    ///    变异：`toolset.rs` 里把 `grep` / `find` 互换 → 本用例变红。
    #[test]
    fn tool_order_survives_registry_into_agent() {
        let _guard = env_lock();
        let dir = unique_dir("agent");

        let (tools, _hint) = build_tools(&dir, offline_web());
        let agent = AgentBuilder::new()
            .tools(tools)
            .system_prompt("")
            .working_dir(dir.clone(), dir.clone())
            .approval(AutoApprove)
            .build()
            .expect("装配 11 个基本工具应能建成 agent");

        assert_eq!(agent.tool_names(), expected_order_for_tests());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 3. **降级提示内容**（`PATH` 指向空目录 → 两后端皆缺；web 侧 offline → 无搜索后端）。
    ///
    ///    断言的不只是 `is_some()`，而是**内容片段**：`rg` / `fd` / `.gitignore`。
    ///    这是「提示逐字来自 `degraded_hint()`」的**唯一**能抓变异的形式 ——
    ///    若实现被改成硬编码文案（或同源比对式的自证），只有内容断言会红。
    ///
    ///    变异：`toolset.rs` 把 `backends.degraded_hint()` 换成
    ///    `Some("hardcoded".into())` → 本用例变红。
    #[test]
    fn degraded_hint_is_verbatim_from_detect_when_backends_missing() {
        let _guard = env_lock();
        let dir = unique_dir("degraded_path");
        // 空目录即「PATH 上什么都没有」——绝对不命中任何后端。
        let _env = set_path(&dir);

        let (tools, hint) = build_tools(&dir, offline_web());
        assert_eq!(
            tools.len(),
            11,
            "降级不影响工具数量（grep/find 退 POSIX 路径；web 工具恒注册）"
        );

        let hint = hint.expect("两后端皆缺时应给出启动期降级提示");
        assert!(hint.contains("rg"), "提示应点名缺 rg；hint = {hint}");
        assert!(hint.contains("fd"), "提示应点名缺 fd；hint = {hint}");
        assert!(
            hint.contains(".gitignore"),
            "提示应说明后果（POSIX 回退不读 .gitignore）；hint = {hint}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 4. **无降级**：`PATH` 上放齐假 `rg` 与 `fd`，且 web 侧「已配置」
    ///    （HTTP 栈可用 + 链非空 + 落盘无降级）→ `hint == None`（无提示即零噪声）。
    ///
    ///    假二进制只需**能被探测认出**（文件 + 可执行位），不必真能跑 ——
    ///    探测是纯路径扫描，本用例因此不依赖机器上装了 ripgrep。
    ///
    ///    ⚠️ web 侧必须**显式注入**（不能用 `from_env()`）：合并后 `None` 的判据
    ///    是**两侧都无问题**，若 web 侧没配搜索后端，提示依然会出现。
    #[cfg(unix)]
    #[test]
    fn no_hint_when_both_backends_present() {
        let _guard = env_lock();
        let dir = unique_dir("both_backends");
        write_fake_backend(&dir, "rg");
        write_fake_backend(&dir, "fd");
        let _env = set_path(&dir);

        let (tools, hint) = build_tools(&dir, configured_web());
        assert_eq!(tools.len(), 11);
        assert_eq!(hint, None, "两侧都无问题时不得产出任何提示");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 4b. **两条 hint 合并、绝不互相覆盖**（设计 §1.1 硬约束）。
    ///
    ///     `PATH` 指向空目录（`rg`/`fd` 皆缺）**且** web 侧 `offline`（无搜索后端）：
    ///     提示必须**同时**含搜索后端的文案（`rg` / `fd` / `.gitignore`）**与**
    ///     web 的「未配置任何搜索后端」文案。
    ///
    ///     ⚠️ web 侧刻意用 `offline` 而非 `configured_web`：让**两个来源同时非空**，
    ///     这才是「合并」与「覆盖」的分水岭 —— 若只测单侧，覆盖实现照样能过。
    ///
    ///     变异：`merge_hints` 的 `(Some, Some)` 分支改成只保留一侧（覆盖另一侧）
    ///     → 本用例变红（缺掉的那侧片段断言失败）。
    #[test]
    fn both_hints_are_merged_not_overwritten() {
        let _guard = env_lock();
        let dir = unique_dir("merged_hints");
        // 空目录即「PATH 上什么都没有」——搜索后端侧必然出提示。
        let _env = set_path(&dir);

        let (_tools, hint) = build_tools(&dir, offline_web());
        let hint = hint.expect("两侧都降级时应给出提示（合并后的那一条）");

        // 搜索后端侧（`SearchBackends::degraded_hint` 的逐字片段）。
        assert!(
            hint.contains("rg"),
            "合并提示应含搜索后端侧点名 rg；hint = {hint}"
        );
        assert!(
            hint.contains("fd"),
            "合并提示应含搜索后端侧点名 fd；hint = {hint}"
        );
        assert!(
            hint.contains(".gitignore"),
            "合并提示应含搜索后端侧的后果句；hint = {hint}"
        );
        // web 侧（`WebConfig::degraded_hint` 的逐字片段）。
        assert!(
            hint.contains("未配置任何搜索后端"),
            "合并提示应含 web 侧「未配置任何搜索后端」；hint = {hint}"
        );

        // 两条**都在**（不是一条覆盖另一条）：顺序为「搜索后端 → web」。
        let search_at = hint.find("rg").unwrap();
        let web_at = hint.find("未配置任何搜索后端").unwrap();
        assert!(
            search_at < web_at,
            "合并顺序应为 搜索后端 → web；hint = {hint}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 5. **`toolset.rs` 不 print**（与 `capabilities.rs` 同规：只产出文案）。
    ///
    ///    读的是**本文件自身**（`include_str!` 相对当前文件解析，不依赖运行目录），
    ///    故断言的是真实随包编译的源码，而非 CWD 下碰巧同名的文件。
    ///
    ///    注意：针脚用 `concat!` 拼出，源码里不出现这些字面量 —— 否则扫描会扫到
    ///    自己的断言字符串而恒红。
    #[test]
    fn toolset_source_contains_no_print_macros() {
        let src = include_str!("toolset.rs");
        let needles = [
            concat!("print", "ln!"),
            concat!("eprint", "ln!"),
            concat!("dbg", "!"),
        ];
        for needle in needles {
            assert!(
                !src.contains(needle),
                "toolset.rs 不得出现 {needle}：本模块只产出文案，打印归 main（输出三分）"
            );
        }
    }

    /// 6. **单侧 hint：搜索后端侧独存**（`merge_hints` 的 `(Some(search), None)`）。
    ///
    ///    `PATH` 指向空目录（`rg`/`fd` 皆缺）**且** web 侧 [`configured_web`]
    ///    （HTTP 栈可用 + 链非空 → web 侧 `degraded_hint()` 恒 `None`）：用户必须
    ///    **仍能看到**搜索后端的降级提示。
    ///
    ///    这是**最常见的生产形态** —— 用户配了搜索 key（web 侧无提示），但本机没装
    ///    ripgrep / fd。若 `(Some, None)` 分支被写成 `None`，这类用户一条降级提示
    ///    都看不到，且 grep/find 已静默退到 POSIX 路径（不读 `.gitignore`）——
    ///    正是最需要提示的场景反而没有。
    ///
    ///    变异：`merge_hints` 的 `(Some(search), None)` 分支改成 `None`
    ///    → 本用例变红（`hint.expect` panic）。
    #[test]
    fn search_hint_survives_when_web_side_is_clean() {
        let _guard = env_lock();
        let dir = unique_dir("search_only_hint");
        // 空目录即「PATH 上什么都没有」——搜索后端侧必然出提示。
        let _env = set_path(&dir);

        let (_tools, hint) = build_tools(&dir, configured_web());
        let hint =
            hint.expect("本机缺 rg/fd 时必须给出提示 —— web 侧无问题（configured_web）不抵消它");
        assert!(hint.contains("rg"), "提示应点名缺 rg；hint = {hint}");
        assert!(hint.contains("fd"), "提示应点名缺 fd；hint = {hint}");
        assert!(
            hint.contains(".gitignore"),
            "提示应说明后果（POSIX 回退不读 .gitignore）；hint = {hint}"
        );
        // web 侧本条无问题：不应混入 web 侧那句（否则说明合并把来源搞串了）。
        assert!(
            !hint.contains("未配置任何搜索后端"),
            "web 侧 configured_web 无问题，提示里不应出现 web 侧文案；hint = {hint}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 7. **单侧 hint：web 侧独存**（`merge_hints` 的 `(None, Some(web))`）。
    ///
    ///    `PATH` 放齐假 `rg`/`fd`（搜索后端侧恒无提示）**且** web 侧 [`offline_web`]
    ///    （HTTP 栈不可用 + 链空 → 两条 web 文案）：用户必须**仍能看到** web 侧的
    ///    降级文案。
    ///
    ///    断言用**逐字文案**（`未配置任何搜索后端`，摘自
    ///    `adapters/tools-web/src/lib.rs::WebConfig::degraded_hint`），不是
    ///    `is_some()` —— 否则「提示串被换成别的/来源串了」这类变异照样全绿。
    ///
    ///    变异：`merge_hints` 的 `(None, Some(web))` 分支改成 `None`
    ///    → 本用例变红（`hint.expect` panic）。
    #[cfg(unix)]
    #[test]
    fn web_hint_survives_when_search_backends_present() {
        let _guard = env_lock();
        let dir = unique_dir("web_only_hint");
        write_fake_backend(&dir, "rg");
        write_fake_backend(&dir, "fd");
        let _env = set_path(&dir);

        let (_tools, hint) = build_tools(&dir, offline_web());
        let hint = hint.expect("web 侧降级（offline + 空链）时必须给出提示 —— rg/fd 齐全不抵消它");
        assert!(
            hint.contains("未配置任何搜索后端"),
            "提示应逐字含 web 侧「未配置任何搜索后端」；hint = {hint}"
        );
        // 搜索后端侧本条无问题：不应混入搜索后端侧的后果句。
        assert!(
            !hint.contains(".gitignore"),
            "rg/fd 齐全时不应出现搜索后端侧的 `.gitignore` 后果句；hint = {hint}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 8. **`build_tools` 无副作用**（设计 §1.1 硬约束）。
    ///
    ///    临时目录扮演 [`FetchedDir::Primary`] —— **可清理**档，故这是最坏情形：
    ///    若装配路径里混进了任何清理调用，这一档必被删。目录里放一个**超龄且名字
    ///    符合落盘形状**（`<slug>-<16hex>.md`）的文件 → 调 `build_tools` 后文件必须
    ///    **仍在**：清理**只**由 `main` 在装配序列里显式调。
    ///
    ///    文件名直接调 `ys_tools_web::fetched_file_name`（落盘文件名的唯一定义），
    ///    mtime 用 `File::set_modified` 显式拨到 8 天前 —— 故「仍然存在」不可能是因为
    ///    「文件本来就不会被认出来 / 本来就没超龄」。
    ///
    ///    变异：在 `build_tools` 里加
    ///    `if let Some(d) = web.fetched().path() { let _ = ys_tools_web::sweep(d, Duration::ZERO); }`
    ///    → 本用例变红（文件被删）。
    #[test]
    fn build_tools_has_no_sweep_side_effect() {
        let workspace = unique_dir("no_sweep_ws");
        let fetched = unique_dir("no_sweep_fetched");
        let aged = write_aged_fetched(&fetched, "https://example.com/aged-page");

        let web = WebConfig::offline(FetchedDir::Primary(fetched.clone()));
        let (tools, _hint) = build_tools(&workspace, web);

        assert_eq!(tools.len(), 11, "装配本身不应受影响");
        assert!(
            aged.exists(),
            "build_tools 装配不得清理落盘目录（设计 §1.1）；被删 = {}",
            aged.display()
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&fetched);
    }

    /// 9. **并发准入表**：只有**只读**工具声明 `is_concurrency_safe`。
    ///
    ///    `BasicLoop` Step 9 的准入是「批内**每一个**都为 `true`」才走并行 ⇒ 少了这里的声明，
    ///    只读批次会**静默退回串行**（没有任何报错，只有变慢）；把写/执行类工具误标成 `true`
    ///    则会让同一批里的两个写操作并发跑。两个方向都必须被钉住，故本条断言的是**精确集合**，
    ///    不是「至少含有」。
    ///
    ///    变异 1：删掉 `read`/`grep`/`find`/`ls`/`ocr`/`view_image` 任一处的
    ///    `is_concurrency_safe` ⇒ 变红。
    ///    变异 2：给 `bash`/`write`/`edit` 加上 `is_concurrency_safe() -> true` ⇒ 变红。
    ///    （`subagent` 不在这张表里：它由 `main` 追加，且在自己的模块里声明。）
    #[test]
    fn only_read_only_tools_are_concurrency_safe() {
        let _guard = env_lock();
        let dir = unique_dir("concurrency");

        let (tools, _hint) = build_tools(&dir, offline_web());
        let safe: Vec<String> = tools
            .iter()
            .filter(|t| t.is_concurrency_safe())
            .map(|t| t.spec().name)
            .collect();
        assert_eq!(
            safe,
            vec!["read", "grep", "find", "ls", "ocr", "view_image"],
            "并发安全的应恰是六个只读工具（顺序随工具表）；写/执行/联网工具必须保持默认 false"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实 `bash_bg` 的 `spec()` —— 父表追加段的第一项。
    ///
    /// 用真工具而不手写 `ToolSpec`：锚点要钉的是**模型真看到的那份 schema**。
    /// 注册表落在临时目录里（绝不碰真实的 `YUSHAN_BG_DIR`），且本用例不启动任何进程。
    fn bash_bg_spec(workspace: &Path) -> ys_tool::ToolSpec {
        let tasks = Arc::new(BackgroundTasks::new(
            workspace.join("bg"),
            BgLimits::default(),
        ));
        ys_tools_basic::BashBgTool::new(tasks, workspace.to_path_buf(), SessionSlot::new("anchor"))
            .spec()
    }

    /// 在 `dir` 下写一个**超龄**（mtime = 8 天前）且名字**符合落盘命名形状**的文件，
    /// 返回其路径 —— 供「装配不清理」这类断言做**真会命中 sweep 判据**的靶子。
    fn write_aged_fetched(dir: &Path, final_url: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(ys_tools_web::fetched_file_name(final_url));
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60))
            .unwrap();
        path
    }

    /// FNV-1a 64（**不引 `sha2`** —— 锚点的目的是**发现漂移**，不是抗碰撞）。
    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// P0（刀 0）：**前缀指纹锚** —— 工具表 JSON 与系统提示的字节指纹。
    ///
    /// 守卫的是 prompt cache 相干性：任何让「工具清单或系统提示的字节」漂移的改动
    /// （重排、新增字段、动态表）都会让此处的指纹变化。
    ///
    /// 所有带 `path` 参数的文件工具都必须说明**相对路径的解析根**。
    ///
    /// 变异：删掉任一工具 `path.description` 里的 `workspace root` ⇒ 本用例变红。
    /// 这是「参数即契约」的守卫：系统提示里的 cwd 段可能被 `SYSTEM.md` 短路，
    /// 但工具 schema 始终随每次请求发给模型。
    #[test]
    fn path_parameters_state_workspace_relative_resolution() {
        let dir = unique_dir("path-contract");
        let (tools, _) = build_tools(&dir, offline_web());

        for tool in tools {
            let spec = tool.spec();
            let Some(path_description) =
                spec.parameters["properties"]["path"]["description"].as_str()
            else {
                continue;
            };
            assert!(
                path_description.contains("workspace root"),
                "{} 的 path 描述须说明相对 workspace 根解析；description = {path_description}",
                spec.name
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 断言三件：
    ///  1. 基础表（子 agent 的工具表）恰 11 个、顺序 == [`EXPECTED_ORDER_FOR_TESTS`]；
    ///  2. **同一份工具表构造两次，序列化后逐字节相同**（ADR-0022 决策 19 的抖动面：
    ///     子 agent 每次派发都重建表，`spec()` 若非纯函数会让子自己的缓存跨派发抖动）；
    ///  3. 系统提示（空 skill catalogue）的指纹可复现。
    ///
    /// # 两个指纹锚各覆盖什么
    ///
    /// - `fp_tools`：**只**覆盖基础 11 元（子 agent 的工具表）。它守的是子表自身的
    ///   字节稳定性，对追加段完全无感。
    /// - `fp_parent`：覆盖基础 11 元 + 追加段（`bash_bg` / `subagent_async` / `subagent_send` /
    ///   `todo`）共 **15 元**，与 `assembly.rs` 的生产父表同形。追加段工具的 schema 漂移
    ///   在 `fp_tools` 那里是**静默的**（本会话已实际发生过一次：给派发工具加
    ///   `mode` 参数后 `fp_tools` 纹丝不动），故必须由父表锚补上。
    ///
    /// # 变异
    ///
    /// 变异：把 [`build_tools`] 里 `grep`/`find` 互换 ⇒ 顺序断言与两个指纹都变红。
    /// 变异：改任一**基础**工具的 description ⇒ `fp_tools` 与 `fp_parent` 都变红。
    /// 变异：改 `subagent_async` / `subagent_send` / `todo` 的 description ⇒ `fp_parent`
    /// 变红、`fp_tools` 不变（这正是父表锚存在的理由）。
    #[test]
    fn prefix_fingerprint_is_anchored_and_stable() {
        let _guard = env_lock();
        let dir = unique_dir("fingerprint");

        let (tools, _) = build_tools(&dir, offline_web());
        let specs: Vec<ys_tool::ToolSpec> = tools.iter().map(|t| t.spec()).collect();
        assert_eq!(specs.len(), 11, "基础表应为 11 个（子 agent 的表）");
        let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, expected_order_for_tests(), "顺序即语义");

        let json = serde_json::to_string(&specs).unwrap();
        let fp_tools = fnv1a64(json.as_bytes());

        // **父表锚点**：父表 = 这 11 元 + `bash_bg` + `subagent_async` + `subagent_send`
        // + `todo` ⇒ **15 元**（与 `assembly.rs` 的生产父表同形），且父 JSON 是子 JSON 的
        // **字节前缀 + 追加项**（子表命中父的前缀缓存）。
        //
        // 四个追加项都取**真工具**的 `spec()`（不是手抄的 `ToolSpec`）—— 锚点要钉的是模型
        // 真看到的那份 schema。
        let mut parent_specs = specs.clone();
        parent_specs.push(bash_bg_spec(&dir));
        parent_specs.push(crate::subagent::SubagentAsyncTool::new().spec());
        parent_specs.push(crate::subagent::SubagentSendTool::new().spec());
        parent_specs.push(ys_tools_todo::todo_tool(ys_tools_todo::TodoStore::open(None)).spec());
        assert_eq!(
            parent_specs.len(),
            15,
            "父表应为 15 元（11 基础 + bash_bg + subagent_async + subagent_send + todo）"
        );
        // 合同：父表**不得**含已删除的同步 `subagent` 工具 —— 子 agent 只能靠
        // `subagent_async` 派发（`subagent_send` 只对**已在跑**的子 agent 插话，
        // 没有 handle 时 fail-closed，无法替代派发）。
        //
        // 这里的 `"subagent"` 是 `==` 的**精确相等**比较（不是 `contains` 子串匹配），
        // 故不会误伤 `subagent_async` / `subagent_send`。改动成 `contains` 会让本断言
        // 在两处永远为真 —— 那才是真正的误伤方向，别做。
        assert!(
            !parent_specs.iter().any(|s| s.name == "subagent"),
            "同步 `subagent` 工具已删除，父表不得含它（子 agent 靠 `subagent_async` 派发）"
        );
        assert_eq!(
            parent_specs[parent_specs.len() - 4..]
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<&str>>(),
            vec!["bash_bg", "subagent_async", "subagent_send", "todo"],
            "追加段顺序即契约：后台工具在派发工具**之前**，`todo` 收尾\
             （与 assembly.rs 尾断言逐项一致，否则子表不再是父的字节前缀）"
        );
        let parent_json = serde_json::to_string(&parent_specs).unwrap();
        assert!(
            parent_json.starts_with(&json[..json.len() - 1]),
            "父表 JSON 必须是子表 JSON 的字节前缀（去掉末尾 `]`）+ 追加项"
        );
        let fp_parent = fnv1a64(parent_json.as_bytes());

        // 2) 同一份表构造两次 → 逐字节相同。
        let (tools2, _) = build_tools(&dir, offline_web());
        let specs2: Vec<ys_tool::ToolSpec> = tools2.iter().map(|t| t.spec()).collect();
        let json2 = serde_json::to_string(&specs2).unwrap();
        assert_eq!(
            json, json2,
            "同一工具表构造两次必须逐字节相同（子 agent 缓存跨派发稳定）"
        );
        assert_eq!(fnv1a64(json2.as_bytes()), fp_tools);

        // 3) 系统提示指纹（空 catalogue）可复现。
        let cat = crate::skills::Catalogue {
            lines: String::new(),
            bytes: 0,
            omitted: 0,
            entries: Vec::new(),
        };
        let prompt = crate::prompt::build_system_prompt(&dir, &specs, &cat);
        let fp_prompt = fnv1a64(prompt.as_bytes());
        let prompt2 = crate::prompt::build_system_prompt(&dir, &specs2, &cat);
        assert_eq!(
            fp_prompt,
            fnv1a64(prompt2.as_bytes()),
            "两次构建的系统提示指纹必须一致"
        );

        // 指纹非零（占位断言，防止函数被误改成恒 0）。
        assert_ne!(fp_tools, 0);
        assert_ne!(fp_prompt, 0);
        // 工具表指纹锚：任何工具名 / 描述 / schema 的改动都会让它变化。
        // **变化本身即是信号** —— 它会平移模型看到的前缀、使所有用户的缓存冷启动一次，
        // 所以必须显式更新这个数字，让 diff 暴露这次漂移。
        // 只为**工具表**记值：`ToolSpec` 里不含任何路径，故 `fp_tools` 与运行环境无关。
        // `fp_prompt` **不入锚** —— 它含 `Current working directory: <临时目录>`，逐次运行都不同，
        // 只由「同一目录两次构建一致」那条断言守卫。
        assert_eq!(
            fp_tools, 0xf017256b30c9d6f7u64,
            "工具表指纹漂移：确认是有意的工具面改动，再更新此锚点"
        );
        // 父表指纹锚：覆盖基础 11 元 + 追加段（`bash_bg` / `subagent_async` / `subagent_send`
        // / `todo`）。
        // `fp_tools` 只覆盖子表，追加段工具的 schema 漂移在它那里是静默的 —— 这里补上。
        //
        // 最近一次漂移：追加段末位新增 `todo` 清单工具（父表 14 → 15 元，前缀 +1433 B）。
        // 上一轮漂移是 `view_image` 的 description 补上到 `ocr` 的分流指引。
        assert_eq!(
            fp_parent, 0xe29d34d132355915u64,
            "父表指纹漂移：确认是有意的工具面改动，再更新此锚点"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
