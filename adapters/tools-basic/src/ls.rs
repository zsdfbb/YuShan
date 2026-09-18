//! `ls` —— 列目录。
//!
//! **纯 Rust，不起子进程**（`tokio::fs::read_dir`）：没有后端分歧，也就没有 argv、没有退出码、
//! 没有 deadline。三个上限里的两个仍然适用 —— 条目数 `limit` 与 `head_lines(2000 行 / 50KB)`。
//!
//! 另有一个**安全上限**：`read_dir` 边读边计数，**超 [`SCAN_CAP`] 即停**。
//! 超大目录（几十万条）若先全部读入再排序，是一次无界读；停在上限处是刻意的取舍。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec, append_notices};

/// 一次调用返回的条目数上限（可被 `limit` 覆盖）。
const DEFAULT_LIMIT: usize = 500;

/// `read_dir` 的条数扫描上限（防超大目录无界读）。
const SCAN_CAP: usize = 20_000;

pub struct LsTool {
    workspace: PathBuf,
}

impl LsTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for LsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "ls",
            "List directory contents. Returns entries sorted case-insensitively, with a '/' suffix \
             for directories. Includes dotfiles. Returns at most `limit` entries (default 500), \
             capped at 2000 lines / 50KB.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                    "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
                }
            }),
        )
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let path = input["path"].as_str().unwrap_or(".");
        // `limit` 至少为 1：0 会让「已截断」判定恒真、并产出 `Use limit=0 for more` 这类废话。
        let limit = (input["limit"].as_u64().unwrap_or(DEFAULT_LIMIT as u64) as usize).max(1);
        let dir = self.workspace.join(path);

        // 路径不存在 / 不是目录：**模型可见的错误结果**（不是基础设施失败）——
        // 模型给出错误路径是它的输入问题，应由它自己改正，不该中断循环。
        let meta = match tokio::fs::metadata(&dir).await {
            Ok(meta) => meta,
            Err(e) => {
                return Ok(error_result(format!(
                    "Path not found: {} ({e})",
                    dir.display()
                )));
            }
        };
        if !meta.is_dir() {
            return Ok(error_result(format!("Not a directory: {}", dir.display())));
        }

        let (mut entries, scan_capped) = match read_entries(&dir).await {
            Ok(entries) => entries,
            Err(e) => {
                return Ok(error_result(format!(
                    "Cannot read directory {}: {e}",
                    dir.display()
                )));
            }
        };

        // 大小写不敏感排序（`/` 后缀在排序**之后**加，避免符号参与比较）。
        entries.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });

        let limit_reached = entries.len() > limit;
        entries.truncate(limit);

        let lines: Vec<String> = entries
            .iter()
            .map(|e| {
                if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                }
            })
            .collect();
        let truncated = ys_tool::head_lines(&lines, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);

        let mut notices: Vec<String> = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{limit} entries limit reached. Use limit={} for more",
                limit.saturating_mul(2)
            ));
        }
        if scan_capped {
            notices.push(format!(
                "Directory scan stopped after {SCAN_CAP} entries; results may be incomplete"
            ));
        }
        if truncated.hit.is_some() {
            notices.push(format!(
                "Truncated {} lines, showing first {}",
                truncated.total_lines - truncated.shown_lines,
                truncated.shown_lines
            ));
        }

        let mut content = truncated.content;
        if content.is_empty() && !limit_reached {
            content = "(empty directory)".into();
        }
        append_notices(&mut content, &notices);

        Ok(ToolResult {
            content,
            is_error: false,
        })
    }
}

/// 一个目录项的名字 + 是否目录。
struct Entry {
    name: String,
    is_dir: bool,
}

/// 读目录项，**边读边计数**，到 [`SCAN_CAP`] 即停。返回 `(条目, 是否因上限截停)`。
///
/// 条目类型用 `DirEntry::file_type()`（多数平台不起额外 syscall）；只对符号链接回退到
/// `metadata()`（跟随链接，与 `ls` 的直觉一致）。取不到类型的条目**跳过**（不因一个坏条目
/// 让整次列举失败）。
async fn read_entries(dir: &Path) -> std::io::Result<(Vec<Entry>, bool)> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut capped = false;

    while let Some(entry) = rd.next_entry().await? {
        if entries.len() >= SCAN_CAP {
            capped = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = match entry.file_type().await {
            Ok(file_type) if file_type.is_symlink() => tokio::fs::metadata(entry.path())
                .await
                .map(|meta| meta.is_dir())
                .unwrap_or(false),
            Ok(file_type) => file_type.is_dir(),
            Err(_) => continue,
        };
        entries.push(Entry { name, is_dir });
    }

    Ok((entries, capped))
}

/// 模型可见的错误结果（`is_error = true`，不是 `Err`）。
fn error_result(content: String) -> ToolResult {
    ToolResult {
        content,
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{brief, ctx, guarded, test_dir};

    /// 调用 + **挂起护栏**（不返回 = panic，而非测试挂死）。
    async fn call(tool: &LsTool, input: Value) -> ToolResult {
        guarded("ls::call", tool.call(input, ctx()))
            .await
            .expect("工具调用不应返回 Err（错误一律走 is_error 结果）")
    }

    /// 造 `n` 个空文件（`ls` 扫描上限用例需要 >20000 条目）。
    fn write_many_files(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::File::create(dir.join(format!("f{i:05}.txt"))).unwrap();
        }
    }

    // ── ① 基本语义 ───────────────────────────────────────────────────────

    /// 大小写不敏感排序、目录带 `/` 后缀、**含 dotfile**。
    ///
    /// 变异：把 `sort_by` 换成 `a.name.cmp(&b.name)`（区分大小写）→ 顺序断言变红；
    /// 把 dotfile 过滤掉（`!name.starts_with('.')`）→ `.hidden` 断言变红。
    #[tokio::test]
    async fn test_ls_sorts_case_insensitively_with_dir_suffix_and_dotfiles() {
        let dir = test_dir("ls_basic");
        std::fs::create_dir_all(dir.join("adir")).unwrap();
        std::fs::create_dir_all(dir.join("DirB")).unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::write(dir.join("A.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();

        let result = call(&LsTool::new(dir.clone()), json!({})).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(
            lines,
            // 大小写不敏感（a.txt < adir，'.' < 'a'）且目录带 `/`。
            vec![".hidden", "A.txt", "adir/", "b.txt", "DirB/"],
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空目录 → `(empty directory)`（不是空内容、也不是错误）。
    #[tokio::test]
    async fn test_ls_empty_directory_message() {
        let dir = test_dir("ls_empty");
        std::fs::create_dir_all(&dir).unwrap();

        let result = call(&LsTool::new(dir.clone()), json!({})).await;

        assert!(!result.is_error);
        assert_eq!(result.content.trim(), "(empty directory)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `limit` 截断 + notice（上限一律显式报出，不得静默）。
    ///
    /// 变异：删掉 `limit_reached` 的 notice → 本用例的文案断言变红。
    #[tokio::test]
    async fn test_ls_limit_truncates_and_notices() {
        let dir = test_dir("ls_limit");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), "").unwrap();
        }

        let result = call(&LsTool::new(dir.clone()), json!({ "limit": 2 })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert_eq!(
            result
                .content
                .lines()
                .filter(|l| l.ends_with(".txt"))
                .count(),
            2,
            "实际：{}",
            brief(&result.content)
        );
        assert!(
            result
                .content
                .contains("2 entries limit reached. Use limit=4 for more"),
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 路径不存在 / 不是目录 → **模型可见的错误结果**（不是基础设置失败 `Err`）：
    /// 模型给错路径该由它自己改正，不该中断循环。
    #[tokio::test]
    async fn test_ls_missing_path_and_file_path_are_error_results() {
        let dir = test_dir("ls_errors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        let tool = LsTool::new(dir.clone());

        let missing = call(&tool, json!({ "path": "nope" })).await;
        assert!(missing.is_error);
        assert!(
            missing.content.contains("Path not found"),
            "实际：{}",
            brief(&missing.content)
        );

        let file = call(&tool, json!({ "path": "a.txt" })).await;
        assert!(file.is_error);
        assert!(
            file.content.contains("Not a directory"),
            "实际：{}",
            brief(&file.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ② 扫描上限（`SCAN_CAP = 20000`） ─────────────────────────────────

    /// 恰好 **20000** 条：**不**报扫描上限；再加 1 条（20001）→ **报**。
    ///
    /// 这是「`read_dir` 边读边计数、**超** 20000 即停」的边界正身（阈值写死在这里，
    /// 与 `SCAN_CAP` 常量一致）。`limit` 给足（> 20000）以免被条数上限掩盖。
    ///
    /// 变异：把 `SCAN_CAP` 调大（如 30000）或删掉 `if entries.len() >= SCAN_CAP { … }`
    /// → 20001 那次不再报 notice → 本用例变红；调小（如 19999）→ 20000 那次误报 → 变红。
    #[tokio::test]
    async fn test_ls_scan_cap_notice_exactly_at_20001_entries() {
        let dir = test_dir("ls_scancap");
        std::fs::create_dir_all(&dir).unwrap();
        write_many_files(&dir, 20_000);
        let tool = LsTool::new(dir.clone());

        let at_cap = call(&tool, json!({ "limit": 100_000 })).await;
        assert!(!at_cap.is_error, "{}", brief(&at_cap.content));
        assert!(
            !at_cap.content.contains("Directory scan stopped"),
            "恰好 20000 条不该报扫描上限；实际：{}",
            brief(&at_cap.content)
        );

        // 再加一条 → 超过扫描上限。
        std::fs::File::create(dir.join("f20000.txt")).unwrap();
        let over_cap = call(&tool, json!({ "limit": 100_000 })).await;
        assert!(!over_cap.is_error, "{}", brief(&over_cap.content));
        assert!(
            over_cap
                .content
                .contains("Directory scan stopped after 20000 entries"),
            "超过扫描上限必须显式报出；实际：{}",
            brief(&over_cap.content)
        );
        // 结果**有界**：20000 条目进 `head_lines`（2000 行 / 50KB）→ 恰好渲染 2000 行。
        assert_eq!(
            over_cap
                .content
                .lines()
                .filter(|l| l.ends_with(".txt"))
                .count(),
            ys_tool::MAX_LINES,
            "条目输出必须被 head_lines 截到上限；实际：{}",
            brief(&over_cap.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
