pub mod bash;
pub mod background;
pub mod bg_output;
pub mod bg_tools;
pub mod edit;
pub mod exec;
pub mod find;
pub mod grep;
pub mod ls;
pub mod proc_group;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use background::{
    BgError, BgHandle, BgLimits, BgSnapshot, BgState, BackgroundTasks, SessionSlot,
};
pub use bg_tools::{BashBgTool, BashPollTool};
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// `read` / `write` / `edit` / `grep` / `find` / `ls` 共用的 workspace 路径提示。
///
/// 这些工具的相对路径都以 workspace 根为解析根。模型若仍传入 workspace 内的绝对路径，
/// 结果里追加一条**事实**，说明同一个位置可以用相对路径表达。提示只在违规时出现，
/// 不占正常调用的常驻成本；workspace 外的绝对路径（如 skill 文件）不提示。
pub(crate) mod workspace_path {
    use std::path::{Component, Path};

    use ys_core::ToolResult;

    /// 判断 `path` 是否为 workspace 内的绝对路径，并返回一条可追加到结果末尾的事实。
    ///
    /// `canonicalize` 能处理符号链接与 `..`；路径不存在时（例如 `write` 的新文件）
    /// 退化为字面比较。任何一侧解析失败都只可能导致少提示，不会影响工具执行。
    pub fn absolute_notice(tool: &str, path: &str, workspace: &Path) -> Option<String> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return None;
        }

        let workspace_canonical = workspace.canonicalize().ok();
        let relative = match path.canonicalize() {
            // 存在的路径按真实路径比较：符号链接指向 workspace 外时不误报。
            Ok(resolved) => {
                let base = workspace_canonical.as_deref().unwrap_or(workspace);
                resolved.strip_prefix(base).ok()?.to_path_buf()
            }
            // 不存在的路径（如 write 新文件）只能字面比较。含 `..` 时可能逃出
            // workspace，宁可少提示也不误报。
            Err(_) => {
                let relative = if let Ok(relative) = path.strip_prefix(workspace) {
                    relative
                } else {
                    let base = workspace_canonical.as_deref()?;
                    path.strip_prefix(base).ok()?
                };
                if relative
                    .components()
                    .any(|component| component == Component::ParentDir)
                {
                    return None;
                }
                relative.to_path_buf()
            }
        };
        let relative = if relative.as_os_str().is_empty() {
            "."
        } else {
            relative.to_str()?
        };

        Some(format!(
            "{tool}: this path is inside the workspace root; `{relative}` resolves to the same location"
        ))
    }

    /// 把路径事实附到非空结果后；空结果保持为空（`read` 空文件的 `NoOutput` 语义不变）。
    pub fn attach_notice(mut result: ToolResult, notice: Option<&str>) -> ToolResult {
        if let Some(notice) = notice
            && !result.content.is_empty()
        {
            ys_tool::append_notices(&mut result.content, &[notice.to_string()]);
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_support::test_dir;

        #[test]
        fn relative_paths_are_not_reported() {
            let dir = test_dir("workspace_path_relative");
            assert!(absolute_notice("read", "docs/TODO.md", &dir).is_none());
        }

        #[test]
        fn absolute_inside_path_reports_relative_form() {
            let dir = test_dir("workspace_path_inside");
            std::fs::create_dir_all(dir.join("docs")).unwrap();
            let file = dir.join("docs/TODO.md");
            std::fs::write(&file, "x").unwrap();

            let notice = absolute_notice("read", &file.to_string_lossy(), &dir).unwrap();
            assert!(notice.contains("read: this path is inside the workspace root"));
            assert!(notice.contains("`docs/TODO.md`"));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn absolute_outside_path_is_not_reported() {
            let dir = test_dir("workspace_path_outside");
            let outside = std::env::temp_dir().join("workspace_path_outside_file");
            assert!(absolute_notice("read", &outside.to_string_lossy(), &dir).is_none());
        }

        #[test]
        fn workspace_root_is_reported_as_dot() {
            let dir = test_dir("workspace_path_root");
            let notice = absolute_notice("ls", &dir.to_string_lossy(), &dir).unwrap();
            assert!(notice.contains("`.` resolves to the same location"));
        }

        #[test]
        fn nonexistent_inside_path_is_still_reported() {
            let dir = test_dir("workspace_path_new_file");
            let file = dir.join("new.txt");
            let notice = absolute_notice("write", &file.to_string_lossy(), &dir).unwrap();
            assert!(notice.contains("`new.txt`"));
        }

        #[test]
        fn nonexistent_parent_path_is_not_reported() {
            let dir = test_dir("workspace_path_parent");
            let file = dir.join("../outside.txt");
            assert!(absolute_notice("write", &file.to_string_lossy(), &dir).is_none());
        }

        #[test]
        fn sibling_with_shared_prefix_is_not_inside() {
            let dir = test_dir("workspace_path_sibling");
            let sibling = dir.with_file_name(format!(
                "{}-sibling",
                dir.file_name().unwrap().to_string_lossy()
            ));
            std::fs::create_dir_all(&sibling).unwrap();
            let file = sibling.join("x.txt");

            assert!(absolute_notice("read", &file.to_string_lossy(), &dir).is_none());
            let _ = std::fs::remove_dir_all(&sibling);
        }

        #[test]
        fn attach_notice_preserves_empty_content() {
            let result = attach_notice(
                ToolResult {
                    content: String::new(),
                    is_error: false,
                },
                Some("read: note"),
            );
            assert_eq!(result.content, "");
        }

        #[test]
        fn attach_notice_appends_fact_to_nonempty_content() {
            let result = attach_notice(
                ToolResult {
                    content: "body".into(),
                    is_error: false,
                },
                Some("read: note"),
            );
            assert_eq!(result.content, "body\n\n[read: note]");
        }
    }
}

/// `grep` / `find` / `ls` 共用的常量与路径归一。
///
/// 三个工具的后端不同（`rg`/`grep`、`fd`/`find`、纯 Rust），但**上限口径与显示路径口径
/// 必须一致**，故收拢在此，避免三处各写一份而漂移。notice 的**形态**（`relativize` 之外的
/// 那一半）早已上提到 `ys_tool::append_notices`（第 3 个消费者出现后，`pub(crate)` 跨不了 crate）。
pub(crate) mod search {
    use std::path::Path;
    use std::time::Duration;

    /// 一次搜索的兜底 deadline。
    ///
    /// 因 **B2**：`-p` / `--json` 下 `ctx.boundary` 恒为 `None`，没有别的中止通道 ——
    /// 这个固定 timeout 是唯一刹车（见 `exec::run_bounded` 的 `deadline`）。
    pub const DEADLINE: Duration = Duration::from_secs(60);

    /// stdout 读入上限（1 MiB）。防子进程（尤其 `rg -r`）刷爆内存/CPU；达上限即杀子进程。
    ///
    /// 调用方**必须**用 `bounded.stdout.len() >= BYTE_CAP` 判定是否触发，
    /// **不能依赖 `stop == Stop::Cap`** —— 越限后子进程可能在同一个 100ms 轮询窗口内自然退出，
    /// 此时 `stop == Eof` 而 stdout 已被截到 cap（T5 review 的 **F1**）。
    pub const BYTE_CAP: usize = 1024 * 1024;

    /// **降级（POSIX）路径**的排除目录。
    ///
    /// `rg` / `fd` 自带 `.gitignore` 语义，无需此表；`grep -r` / `find` 没有，若不带排除会搜进
    /// `.git/` 对象与依赖树（token 灾难）。
    pub const EXCLUDED_DIRS: &[&str] =
        &[".git", "target", "node_modules", "dist", "build", ".venv"];

    /// 把子进程输出的路径归一成「相对搜索根」的形态：剥掉搜索根前缀，再剥 `./`。
    ///
    /// 我们传给子进程的是 `workspace.join(path)`（与 read/write/bash 同口径，多为绝对路径），
    /// 子进程会把这个前缀原样回显（`rg`：`/w/src/a.rs`；`grep`：`/w/./src/a.rs`），
    /// 直接进上下文既是噪声又浪费 token。
    ///
    /// **搜索根本身是一个文件**时（`path` 指向文件），回显路径恰好**等于**根：此时没有可剥的
    /// 前缀，退化成**文件名**（与 Pi 在非目录根上的 `basename` 行为一致）—— 否则会把整条绝对
    /// 路径送进上下文。
    pub fn relativize(raw: &str, search_root: &Path) -> String {
        let root = search_root.to_string_lossy();
        // 根可能形如 `/w/.`（`path` 参数默认 `.`）：去掉尾部 `/.` 才能与 `/w/src/...` 对上。
        let root = root.trim_end_matches('/');
        let root = root.strip_suffix("/.").unwrap_or(root);
        if !root.is_empty()
            && let Some(rest) = raw.strip_prefix(root)
        {
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                return rest.trim_start_matches("./").to_string();
            }
            if let Some(name) = Path::new(raw).file_name() {
                return name.to_string_lossy().into_owned();
            }
        }
        raw.trim_start_matches("./").to_string()
    }
}

/// `grep` / `find` / `ls` 的**测试专用**助手（`#[cfg(test)]`，不进生产二进制）。
///
/// 三个模块共用同一套临时目录 / 假二进制 / 挂起护栏约定，避免各写一份而漂移。
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use ys_protocol::BoundarySource;
    use ys_tool::ToolContext;

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全的既有样板，见 `bash.rs`
    /// / `read.rs`；`as_nanos()` 在 macOS 只到微秒级，单靠它会撞名）。`tag` 再按用例分桶。
    pub fn test_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agent_tools_basic_search_test_{tag}_{}_{seq}",
            std::process::id()
        ))
    }

    /// 无边界控制的 ctx（三个搜索工具都不看 `cwd` / `workspace_root`）。
    pub fn ctx() -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
    }

    /// 挂了 `boundary` 的 ctx（`Abort` / 中止相关用例）。
    pub fn ctx_with(boundary: &dyn BoundarySource) -> ToolContext<'_> {
        ToolContext::new(Some(boundary), PathBuf::from("."), PathBuf::from("."))
    }

    /// 写一个**带 shebang 的假二进制**（`chmod 0o755`）—— 用它冒充 `rg` / `fd`，
    /// 即可在**不依赖开发机是否装了真 ripgrep/fd** 的前提下精确控制输出与退出码。
    ///
    /// 脚本随测试的临时目录一起清理（调用方 `remove_dir_all`）。
    #[cfg(unix)]
    pub fn fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// 把**整次工具调用**套进 `timeout`：挂起必须表现为**确定性 panic**，而不是测试永不结束。
    ///
    /// 本模块的风险形态（双管道死锁、排空任务等不到 EOF、deadline 不收敛）都是「不返回」，
    /// 没有这层护栏时 CI 上只会看到「测试挂死」且无定位信息。
    pub async fn guarded<F, T>(label: &str, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        match tokio::time::timeout(Duration::from_secs(30), fut).await {
            Ok(value) => value,
            Err(_) => panic!("{label} 未在 30s 内返回 = 挂起（死锁 / 收尸失败）"),
        }
    }

    /// 一行合法的 `rg --json` 事件（`kind` 取 `match` / `context` / `begin` / `end` / `summary`）。
    ///
    /// `path` 原样进 JSON —— 用例自行决定给绝对路径（验 `relativize`）还是相对路径。
    pub fn rg_event_json(kind: &str, path: &str, line: u64, text: &str) -> String {
        format!(
            "{{\"type\":\"{kind}\",\"data\":{{\"path\":{{\"text\":\"{path}\"}},\
             \"line_number\":{line},\"lines\":{{\"text\":\"{text}\"}}}}}}"
        )
    }

    /// 一行合法的 `rg --json` **命中**事件（最常用形态）。
    pub fn rg_match_jsonl(path: &str, line: u64, text: &str) -> String {
        rg_event_json("match", path, line, text)
    }

    /// 把工具返回内容截短到 200 字符 —— 只用于 panic 消息，避免刷屏。
    pub fn brief(s: &str) -> String {
        let taken: String = s.chars().take(200).collect();
        if taken.len() < s.len() {
            format!("{taken}…")
        } else {
            taken
        }
    }
}
