//! `find` —— 按 glob 找文件。
//!
//! | 条件 | 程序 | argv 形态 | 输出形态 |
//! |---|---|---|---|
//! | `backends.fd` 命中 | `fd` | `--glob …` | 每行一个路径 |
//! | 否则（**降级**） | `find` | `\( -name … \) -prune -o -name PAT -print` | 每行一个路径 |
//!
//! 与 `grep` 同款：后端只差 argv 与解析，控制流（spawn / 排空 / 早杀 / 截断）全在
//! [`run_bounded`]。硬约束 F1 / F2 / F3 见 `grep` 的模块文档 —— 这里同样**直接 spawn 二进制**
//! （绝不 `sh -c`）、**argv 总带路径参数**、**上限判定看 stdout 长度**。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec, append_notices};

use crate::exec::{Bounded, SearchBackends, Stop, run_bounded};
use crate::search::{BYTE_CAP, DEADLINE, EXCLUDED_DIRS, relativize};

/// 一次调用返回的**结果条数**上限。
const DEFAULT_LIMIT: usize = 1000;

pub struct FindTool {
    workspace: PathBuf,
    /// **注入**的探测结果（不在工具内部调 `detect()`）。
    backends: SearchBackends,
}

impl FindTool {
    pub fn new(workspace: PathBuf, backends: SearchBackends) -> Self {
        Self {
            workspace,
            backends,
        }
    }
}

#[async_trait::async_trait]
impl Tool for FindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "find",
            "Search for files by glob pattern. Returns matching paths relative to the search \
             directory. Respects .gitignore when fd is available; the POSIX fallback (find) does \
             NOT and instead prunes .git/target/node_modules/dist/build/.venv. Returns at most \
             `limit` results (default 1000), capped at 2000 lines / 50KB.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
                    "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                    "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let pattern = input["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'pattern'".into()))?;
        let path = input["path"].as_str().unwrap_or(".");
        // `limit` 至少为 1：0 会让「已截断」判定恒真、并产出 `Use limit=0 for more` 这类废话。
        let limit = (input["limit"].as_u64().unwrap_or(DEFAULT_LIMIT as u64) as usize).max(1);

        let search_root = self.workspace.join(path);

        let (label, prog, argv) = match &self.backends.fd {
            Some(fd) => ("fd", fd.clone(), fd_argv(pattern, &search_root, limit)),
            None => (
                "find",
                PathBuf::from("find"),
                find_argv(pattern, &search_root),
            ),
        };

        let bounded = run_bounded(
            &prog,
            &argv,
            &self.workspace,
            BYTE_CAP,
            DEADLINE,
            ctx.boundary,
        )
        .await?;

        Ok(build_result(&bounded, &search_root, limit, label))
    }
}

/// `fd --glob --color=never [--full-path] --max-results N -- PATTERN PATH`。
///
/// 含 `/` 的模式（如 `src/*.rs`）必须加 `--full-path`：`fd --glob` 默认只匹配 **basename**，
/// 否则这类模式会**静默返回空**（错答案，比报错更坏）。`--full-path` 下模式要能匹配整条路径，
/// 故按 Pi 的做法补 `**/` 前缀。
fn fd_argv(pattern: &str, path: &Path, limit: usize) -> Vec<String> {
    let mut argv: Vec<String> = vec!["--glob".into(), "--color=never".into()];
    let mut effective = pattern.to_string();
    if pattern.contains('/') {
        argv.push("--full-path".into());
        if !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
            effective = format!("**/{pattern}");
        }
    }
    argv.push("--max-results".into());
    argv.push(limit.to_string());
    argv.push("--".into());
    argv.push(effective);
    argv.push(path.to_string_lossy().into_owned());
    argv
}

/// `find PATH \( -name <每个排除目录> -o … \) -prune -o [-name|-path] PAT -print`。
///
/// 排除目录**只作用于降级路径**（`fd` 自带 `.gitignore`）。`-name` 只匹配 basename，
/// 含 `/` 的模式必须走 `-path`（其 `*` 可跨 `/`，故补 `*/` 前缀让模式匹配到带根的整条路径）。
fn find_argv(pattern: &str, path: &Path) -> Vec<String> {
    let mut argv: Vec<String> = vec![path.to_string_lossy().into_owned(), "(".into()];
    for (i, dir) in EXCLUDED_DIRS.iter().enumerate() {
        if i > 0 {
            argv.push("-o".into());
        }
        argv.push("-name".into());
        argv.push((*dir).into());
    }
    argv.push(")".into());
    argv.push("-prune".into());
    argv.push("-o".into());
    if pattern.contains('/') {
        let effective = if pattern.starts_with('*') || pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("*/{pattern}")
        };
        argv.push("-path".into());
        argv.push(effective);
    } else {
        argv.push("-name".into());
        argv.push(pattern.to_string());
    }
    argv.push("-print".into());
    argv
}

/// 退出码判定 + 渲染（**先看 `stop` 再看 `code`**）。
fn build_result(bounded: &Bounded, search_root: &Path, limit: usize, label: &str) -> ToolResult {
    let killed_by_us = bounded.stop != Stop::Eof;

    let mut lines: Vec<String> = String::from_utf8_lossy(&bounded.stdout)
        .lines()
        .map(|line| line.trim_end_matches('\r').trim())
        .filter(|line| !line.is_empty())
        .map(|line| relativize(line, search_root))
        .collect();

    if !killed_by_us {
        if let Some(code) = bounded.code {
            if code != 0 {
                // 有结果就照给（并说明可能不全）；一条都没有才算失败。
                if lines.is_empty() {
                    return exit_error(label, bounded);
                }
            }
        }
    }

    // `fd --max-results N` 一到 N 就停，我们无法区分「恰好 N 条」与「多于 N 条」，
    // 故按 `>= limit` 判定（与 Pi 同口径，可能误报 —— 但绝不漏报"被截断"）。
    let limit_reached = lines.len() >= limit;
    lines.truncate(limit);

    let truncated = ys_tool::head_lines(&lines, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);

    let mut notices: Vec<String> = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    // ★ F1：用 stdout 长度判定，不看 `stop == Cap`。
    if bounded.stdout.len() >= BYTE_CAP {
        notices.push(format!(
            "{}MB output limit reached, results may be incomplete",
            BYTE_CAP / (1024 * 1024)
        ));
    }
    if truncated.hit.is_some() {
        notices.push(format!(
            "Truncated {} lines, showing first {}",
            truncated.total_lines - truncated.shown_lines,
            truncated.shown_lines
        ));
    }
    match bounded.stop {
        Stop::Aborted => notices.push("Search aborted".into()),
        Stop::TimedOut => notices.push(format!(
            "Search timed out after {}s, results may be incomplete",
            DEADLINE.as_secs()
        )),
        Stop::Eof | Stop::Cap => {}
    }
    if !killed_by_us
        && let Some(code) = bounded.code
        && code != 0
    {
        notices.push(format!(
            "{label} exited with code {code}, results may be incomplete"
        ));
    }

    let mut content = truncated.content;
    if content.is_empty() && !killed_by_us {
        content = "No files found matching pattern".into();
    }
    append_notices(&mut content, &notices);

    ToolResult {
        content,
        is_error: false,
    }
}

/// 非零退码且无结果 → 模型可见的错误结果，附 stderr 前 4 KiB。
fn exit_error(label: &str, bounded: &Bounded) -> ToolResult {
    let stderr = String::from_utf8_lossy(&bounded.stderr);
    let stderr = stderr.trim();
    let content = if stderr.is_empty() {
        match bounded.code {
            Some(code) => format!("{label} exited with code {code}"),
            None => format!("{label} was terminated by a signal"),
        }
    } else {
        stderr.to_string()
    };
    ToolResult {
        content,
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use ys_protocol::{Boundary, QueueBoundarySource};

    use crate::test_support::{brief, ctx, ctx_with, fake_bin, guarded, test_dir};

    /// **注入假 `fd`** 的工具 —— 输出与退出码完全受控，不依赖开发机是否装了真 fd。
    #[cfg(unix)]
    fn fd_tool(fd: &Path, workspace: &Path) -> FindTool {
        FindTool::new(
            workspace.to_path_buf(),
            SearchBackends {
                rg: None,
                fd: Some(fd.to_path_buf()),
            },
        )
    }

    /// 走**降级路径**（真 `find`）的工具。
    fn posix_tool(workspace: &Path) -> FindTool {
        FindTool::new(workspace.to_path_buf(), SearchBackends::posix_only())
    }

    /// 调用 + **挂起护栏**（不返回 = panic，而非测试挂死）。
    async fn call(tool: &FindTool, input: Value) -> ToolResult {
        guarded("find::call", tool.call(input, ctx()))
            .await
            .expect("工具调用不应返回 Err（错误一律走 is_error 结果）")
    }

    /// 造一份 **>1 MiB** 的路径清单（给 F1 用例当假 fd 的输出）。
    #[cfg(unix)]
    fn write_big_path_list(path: &Path) -> usize {
        let mut buf = String::with_capacity(BYTE_CAP + 128);
        let mut i = 0u64;
        while buf.len() <= BYTE_CAP {
            buf.push_str(&format!(
                "big/deep/nested/dir/file_with_a_long_name_{i:06}.txt\n"
            ));
            i += 1;
        }
        std::fs::write(path, &buf).unwrap();
        buf.len()
    }

    // ── ① 降级（POSIX `find`）路径 ───────────────────────────────────────

    /// 降级 find 命中 + 路径相对搜索根归一（剥掉绝对前缀与 `./`）。
    #[tokio::test]
    async fn test_find_posix_finds_matching_files() {
        let dir = test_dir("find_posix_hit");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("src/b.txt"), "").unwrap();

        let result = call(&posix_tool(&dir), json!({ "pattern": "*.rs" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/a.rs"),
            "命中路径必须相对搜索根；实际：{}",
            brief(&result.content)
        );
        assert!(
            !result.content.contains("b.txt"),
            "非 *.rs 不该出现；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 降级 find **无命中** → 不是错误（`find` 退出码 0 + 空输出）。
    ///
    /// 变异：把「空结果」也当失败（`exit_error`）→ 本用例的 `is_error == false` 变红。
    #[tokio::test]
    async fn test_find_posix_no_match_is_not_error() {
        let dir = test_dir("find_posix_nomatch");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "").unwrap();

        let result = call(&posix_tool(&dir), json!({ "pattern": "*.rs" })).await;

        assert!(
            !result.is_error,
            "无命中不是错误；实际：{}",
            brief(&result.content)
        );
        assert_eq!(result.content.trim(), "No files found matching pattern");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 含 `/` 的模式必须走 `-path`（`-name` 只匹配 basename，会静默空集）→ 真能命中，
    /// 且**不过匹配**兄弟目录。
    ///
    /// 变异：`find_argv` 对含 `/` 的模式也用 `-name` → 本用例的命中断言变红。
    #[tokio::test]
    async fn test_find_posix_pattern_with_slash_matches_at_depth() {
        let dir = test_dir("find_posix_slash");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("other/c.rs"), "").unwrap();

        let result = call(&posix_tool(&dir), json!({ "pattern": "src/*.rs" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/a.rs"),
            "含 '/' 的模式必须按路径匹配；实际：{}",
            brief(&result.content)
        );
        assert!(
            !result.content.contains("other/c.rs"),
            "不得过匹配；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 降级路径的**排除目录**生效（`find` 与 `grep` 各有一份 argv 构造，各钉一条）。
    ///
    /// 变异：`find_argv` 不拼 `-name node_modules … -prune`（或 `EXCLUDED_DIRS` 里删掉它）
    /// → 本用例变红。
    #[tokio::test]
    async fn test_find_posix_excludes_node_modules() {
        let dir = test_dir("find_posix_exclude");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("node_modules/dep.rs"), "").unwrap();

        let result = call(&posix_tool(&dir), json!({ "pattern": "*.rs" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/a.rs"),
            "{}",
            brief(&result.content)
        );
        assert!(
            !result.content.contains("node_modules"),
            "降级路径必须 prune node_modules；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ② 原生（`fd`）路径 ───────────────────────────────────────────────

    /// 原生 fd：每行一个路径 → 相对搜索根归一后逐行渲染。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_find_native_fd_renders_relativized_paths() {
        let dir = test_dir("find_native_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs");
        let fd = fake_bin(&dir, "fd", &format!("printf '%s\\n' '{}'\n", abs.display()));

        let result = call(&fd_tool(&fd, &dir), json!({ "pattern": "*.rs" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert_eq!(
            result.content.trim(),
            "src/a.rs",
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原生 fd 的 `limit` 口径是 `>= limit`（fd 一到 N 条就停，无法区分「恰好 N」与「多于 N」，
    /// 宁可误报也不漏报）→ 恰好 N 条时**也要**报 notice + 结果截到 N。
    ///
    /// 变异：`limit_reached` 改成 `lines.len() > limit` → 本用例（恰好 limit 条）变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_find_native_fd_limit_reached_notices_at_exact_limit() {
        let dir = test_dir("find_native_limit");
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (1..=3)
            .map(|i| {
                format!(
                    "printf '%s\\n' '{}'\n",
                    dir.join(format!("src/a{i}.rs")).display()
                )
            })
            .collect();
        let fd = fake_bin(&dir, "fd", &body);

        let result = call(
            &fd_tool(&fd, &dir),
            json!({ "pattern": "*.rs", "limit": 3 }),
        )
        .await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert_eq!(
            result
                .content
                .lines()
                .filter(|l| l.ends_with(".rs"))
                .count(),
            3,
            "实际：{}",
            brief(&result.content)
        );
        assert!(
            result
                .content
                .contains("3 results limit reached. Use limit=6 for more"),
            "截断必须显式报出；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 含 `/` 的模式必须带 `--full-path` **且补 `**/` 前缀**：`fd --glob` 默认只匹配
    /// basename，否则这类模式会**静默返回空**（错答案，比报错更坏）。
    ///
    /// 变异：删掉 `--full-path` 或 `**/` 归一 → argv 断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_find_native_fd_pattern_with_slash_uses_full_path_and_double_star() {
        let dir = test_dir("find_native_fullpath");
        std::fs::create_dir_all(&dir).unwrap();
        let dump = dir.join("argv.txt");
        let fd = fake_bin(
            &dir,
            "fd",
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{}'\n",
                dump.display(),
                dir.join("src/a.rs").display()
            ),
        );

        let result = call(
            &fd_tool(&fd, &dir),
            json!({ "pattern": "src/*.rs", "limit": 10 }),
        )
        .await;

        let dump_text = std::fs::read_to_string(&dump).expect("假 fd 应把 argv 落到 dump");
        let args: Vec<&str> = dump_text.lines().collect();
        assert!(
            args.contains(&"--full-path"),
            "含 '/' 的模式必须带 --full-path；实际 argv：{args:?}"
        );
        let sep = args
            .iter()
            .position(|a| *a == "--")
            .expect("argv 必须带 `--` 分隔符");
        assert_eq!(
            args[sep + 1],
            "**/src/*.rs",
            "含 '/' 的模式必须补 `**/` 前缀；实际 argv：{args:?}"
        );
        assert!(!result.is_error, "{}", brief(&result.content));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非零退码**但有结果** → 照给结果 + 说明可能不全（**不是**错误）；
    /// 非零退码**且零结果** → 才是错误。
    ///
    /// 变异：把 `if lines.is_empty() { return exit_error(..) }` 的外层判定去掉（无条件当失败）
    /// → 本用例前半段变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_find_native_fd_nonzero_exit_with_results_is_not_error() {
        let dir = test_dir("find_native_nonzero");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs");

        let with_hits = fake_bin(
            &dir,
            "fd_hits",
            &format!("printf '%s\\n' '{}'\nexit 2\n", abs.display()),
        );
        let result = call(&fd_tool(&with_hits, &dir), json!({ "pattern": "*.rs" })).await;
        assert!(
            !result.is_error,
            "有结果就不是失败；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("src/a.rs"),
            "{}",
            brief(&result.content)
        );
        assert!(
            result
                .content
                .contains("fd exited with code 2, results may be incomplete"),
            "退码异常必须显式报出（结果可能不全）；实际：{}",
            brief(&result.content)
        );

        let no_hits = fake_bin(
            &dir,
            "fd_empty",
            "echo 'boom: permission denied' >&2\nexit 2\n",
        );
        let result = call(&fd_tool(&no_hits, &dir), json!({ "pattern": "*.rs" })).await;
        assert!(
            result.is_error,
            "零结果 + 非零退码 = 失败；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("boom: permission denied"),
            "stderr 是给模型的线索；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ③ F1：上限判定看 **`stdout` 长度**，不看 `stop == Cap` ─────────────

    /// 假 fd「吐 >1 MiB 路径后**自然退出**」→ `stop == Eof` 而 stdout 已被截到 `BYTE_CAP`；
    /// byte_cap notice **仍必须报出**。
    ///
    /// 变异：把 `find.rs` 的 `bounded.stdout.len() >= BYTE_CAP` 改成 `bounded.stop == Stop::Cap`
    /// → notice 消失 → 本用例变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_find_byte_cap_notice_fires_even_when_stop_is_eof() {
        let dir = test_dir("find_bytecap");
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("paths.txt");
        let size = write_big_path_list(&big);
        assert!(
            size > BYTE_CAP,
            "前置条件：{size} 字节 > BYTE_CAP {BYTE_CAP}"
        );

        let fd = fake_bin(&dir, "fd", &format!("cat '{}'\n", big.display()));

        let bounded = guarded(
            "run_bounded",
            run_bounded(&fd, &[], &dir, BYTE_CAP, DEADLINE, None),
        )
        .await
        .expect("自然退出必须是 Ok");
        assert_eq!(bounded.stop, Stop::Eof, "越限后自然退出 → stop 报 Eof");
        assert_eq!(bounded.stdout.len(), BYTE_CAP, "stdout 仍被截到上限");

        let result = call(
            &fd_tool(&fd, &dir),
            json!({ "pattern": "*.txt", "limit": 1_000_000u64 }),
        )
        .await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("1MB output limit reached"),
            "上限必须显式报出（判定依据是 stdout.len()）；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ④ 被我们自己杀掉时不得说「无命中」 ────────────────────────────────

    /// `stop != Eof` 且零输出 → 收场原因是「被中止」，**不得**拼出
    /// `No files found matching pattern`（搜索根本没跑完，那是误导）。
    ///
    /// 变异：去掉 `content.is_empty() && !killed_by_us` 里的 `!killed_by_us` → 本用例变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_find_aborted_run_does_not_claim_no_files_found() {
        let dir = test_dir("find_aborted");
        std::fs::create_dir_all(&dir).unwrap();
        // 不吐任何东西就挂住；`exec` 让 sh 自身变成 sleep（不留攥着管道写端的孙进程）。
        let fd = fake_bin(&dir, "fd", "exec sleep 30\n");

        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(Boundary::Abort);
        });

        let tool = fd_tool(&fd, &dir);
        let result = guarded(
            "find::call(abort)",
            tool.call(json!({ "pattern": "*.rs" }), ctx_with(&*boundary)),
        )
        .await
        .expect("工具调用不应返回 Err");

        assert!(!result.is_error, "被我们自己杀掉不是错误");
        assert!(
            !result.content.contains("No files found"),
            "被中止时不得声称「没找到文件」；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("Search aborted"),
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
