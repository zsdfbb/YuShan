pub mod bash;
pub mod edit;
pub mod exec;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// `grep` / `find` / `ls` 共用的常量、路径归一与 notice 拼接。
///
/// 三个工具的后端不同（`rg`/`grep`、`fd`/`find`、纯 Rust），但**上限口径、显示路径口径与
/// 通知形态必须一致**，故收拢在此，避免三处各写一份而漂移。
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

    /// 把 notice 拼成 `\n\n[… . …]` 附在正文后；正文为空时 notice 独占内容（不留下前导空行）。
    ///
    /// 上限一律**显式报出**，不得静默（设计 §1.4）—— 是否报由调用方判定，形态由这里统一。
    pub fn append_notices(content: &mut String, notices: &[String]) {
        if notices.is_empty() {
            return;
        }
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push('[');
        content.push_str(&notices.join(". "));
        content.push(']');
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
