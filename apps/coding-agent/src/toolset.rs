//! 工具装配的**唯一接缝**：启动期探测一次后端 → 建有序工具表 → 暴露降级提示。
//!
//! # 为什么收拢成一个模块
//!
//! 工具集的「有哪些工具 / 什么顺序 / 探测到什么后端」在三个地方被消费：
//! 注册进 `ToolRegistry`（`spec()` 顺序由此而定）、将来的系统提示词工具清单、
//! 以及启动期给用户的后端降级提示。若三处各自装配，顺序与探测结果会漂移
//! （典型症状：提示词里列了 `grep`，注册表里没有；或两次探测结果不一致）。
//!
//! # 探测只做一次
//!
//! [`SearchBackends::detect`] 是**纯路径扫描**（不 fork、不跑 `--version`，见 ADR-0015），
//! 但在语义上仍是「启动期一次」的事：同一进程内两次探测可能给出不同答案（PATH 被改），
//! 且 `grep` / `find` 必须看到**同一份**后端认知。故此处探测一次，clone 给两者。
//!
//! # 不 print
//!
//! 与 `capabilities.rs` 同规：本模块只**产出文案**，打印由 `main` 在启动期做
//! （输出三分，见 `docs/design-final/coding-agent-tui.md`）。

use std::path::Path;

use ys_tool::Tool;
use ys_tools_basic::exec::SearchBackends;
use ys_tools_basic::{BashTool, EditTool, FindTool, GrepTool, LsTool, ReadTool, WriteTool};

/// 装配顺序的**唯一真值**：注册顺序 == 提示词工具清单顺序 == `agent.tool_names()`。
///
/// 放在生产段（而非 `#[cfg(test)]`）是为了让**提示词侧**的漂移测试也引用同一份
/// 真值 —— 顺序若在测试里各抄一份，「提示词清单」与「装配顺序」的对齐就只能靠
/// 人眼，而这两者恰恰是本项目最怕漂移的一对。
///
/// 与 [`build_tools`] 里 `vec![...]` 的字面量顺序需人工保持一致：Rust 类型系统
/// 无法把 `vec!` 的元素顺序绑定到常量上。不一致会立刻被两侧的顺序断言抓到。
///
/// 目前仅测试消费（生产路径的顺序由 [`build_tools`] 的 `vec!` 字面量决定），
/// 故非测试构建下允许未使用 —— 保留在生产段正是为了跨模块共享这一份真值。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const EXPECTED_ORDER: &[&str] = &["read", "write", "edit", "bash", "grep", "find", "ls"];

/// 探测一次搜索后端，据此建**有序**工具表；一并交还降级提示。
///
/// # 顺序即语义
///
/// 返回的 `Vec` 顺序 **就是**注册顺序，决定 `ToolRegistry::specs()` 的顺序，
/// 进而决定将来系统提示词里工具清单的顺序。固定为：
///
/// ```text
/// read, write, edit, bash, grep, find, ls
/// ```
///
/// 即「读写改 → 执行 → 检索 → 列目录」：模型最常用的文件读写与 shell 排在最前。
/// 与 [`EXPECTED_ORDER`] 同源（测试侧引用后者），**改一处必须改两处**。
/// **不要**随意重排（会改动 prompt 文本与测试基线）。
///
/// # 降级提示
///
/// 第二个返回值是 [`SearchBackends::degraded_hint`] 的**逐字透传**（后端齐全时为
/// `None`）。本模块**不**另写文案、**不**打印 —— 何时打印、打到哪由 `main` 决定
/// （启动期 → stderr）。
pub fn build_tools(workspace: &Path) -> (Vec<Box<dyn Tool>>, Option<String>) {
    let backends = SearchBackends::detect();
    // 先取提示：`backends` 随即被移入 `find`（`grep` 拿 clone）。
    let hint = backends.degraded_hint();

    let ws = workspace.to_path_buf();
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ReadTool::new(ws.clone())),
        Box::new(WriteTool::new(ws.clone())),
        Box::new(EditTool::new(ws.clone())),
        Box::new(BashTool::new(ws.clone())),
        Box::new(GrepTool::new(ws.clone(), backends.clone())),
        Box::new(FindTool::new(ws.clone(), backends)),
        Box::new(LsTool::new(ws)),
    ];

    (tools, hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ys_runtime::AgentBuilder;
    use ys_tool::AutoApprove;

    use crate::test_env::{EnvRestore, env_lock};

    /// [`EXPECTED_ORDER`] 的 owned 形式（`spec().name` / `tool_names()` 都是 `String`）。
    /// [`EXPECTED_ORDER`]: super::EXPECTED_ORDER
    fn expected_order() -> Vec<String> {
        EXPECTED_ORDER.iter().map(|s| s.to_string()).collect()
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

    /// 1. `build_tools` 返回恰好 **7** 个工具，且 name 序列**逐字**等于固定顺序。
    ///
    /// 变异：`toolset.rs` 里把 `grep` / `find` 互换 → 本用例变红。
    #[test]
    fn build_tools_returns_fixed_order_of_seven() {
        let _guard = env_lock();
        let dir = unique_dir("order");

        let (tools, _hint) = build_tools(&dir);
        assert_eq!(tools.len(), 7, "工具表应为 7 个");

        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(
            names,
            expected_order(),
            "装配顺序即语义（决定 prompt 工具清单顺序）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2. **顺序穿透到 agent**：`build_tools` → `AgentBuilder::tools` →
    ///    `agent.tool_names()` 同为该 7 元序列。
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

        let (tools, _hint) = build_tools(&dir);
        let agent = AgentBuilder::new()
            .tools(tools)
            .system_prompt("")
            .working_dir(dir.clone(), dir.clone())
            .approval(AutoApprove)
            .build()
            .expect("装配 7 个基本工具应能建成 agent");

        assert_eq!(agent.tool_names(), expected_order());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 3. **降级提示内容**（`PATH` 指向空目录 → 两后端皆缺）。
    ///
    ///    断言的不只是 `is_some()`，而是**内容片段**：`rg` / `fd` / `.gitignore`。
    ///    这是「逐字透传 `degraded_hint()`」的**唯一**能抓变异的形式 ——
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

        let (tools, hint) = build_tools(&dir);
        assert_eq!(
            tools.len(),
            7,
            "降级不影响工具数量（grep/find 退 POSIX 路径）"
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

    /// 4. **无降级**：`PATH` 上放齐假 `rg` 与 `fd` → `hint == None`（无提示即零噪声）。
    ///
    ///    假二进制只需**能被探测认出**（文件 + 可执行位），不必真能跑 ——
    ///    探测是纯路径扫描，本用例因此不依赖机器上装了 ripgrep。
    #[cfg(unix)]
    #[test]
    fn no_hint_when_both_backends_present() {
        let _guard = env_lock();
        let dir = unique_dir("both_backends");
        write_fake_backend(&dir, "rg");
        write_fake_backend(&dir, "fd");
        let _env = set_path(&dir);

        let (tools, hint) = build_tools(&dir);
        assert_eq!(tools.len(), 7);
        assert_eq!(hint, None, "后端齐全时不得产出任何提示");

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
}
