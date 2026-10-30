//! `grep` —— 在文件内容里搜模式。
//!
//! # 两条后端，同一套控制流
//!
//! | 条件 | 程序 | argv 形态 | 输出形态 |
//! |---|---|---|---|
//! | `backends.rg` 命中 | `rg` | `--json …` | JSON Lines（`type:"match"` / `"context"`） |
//! | 否则（**降级**） | `grep` | `-r -n -I …` | `path:line:text` / `path-line-text` |
//!
//! 后端只影响 **argv 构造 + 输出解析**；spawn / 排空 / 早杀 / 截断全在 [`run_bounded`]，
//! 故此处用 `match` 两个分支而非 trait（本项目「不设计假想未来需求」）。
//!
//! # 三条硬约束（来自 T5 review，违反会造成真 bug）
//!
//! - **F2 直接 spawn 二进制**：`Command::new("rg")` / `("grep")`，**绝不 `sh -c` 包一层** ——
//!   孙进程会攥住管道写端，`deadline` 就不再是返回时间的硬上界。
//! - **F3 argv 总带路径参数**：否则 `grep` / `rg` 在非 tty 下会读 stdin 挂到 deadline。
//! - **F1 上限判定看 stdout 长度**：见 [`BYTE_CAP`]。
//!
//! # 退出码：先看 `stop`，再看 `code`
//!
//! 子进程可能是**我们杀的**（达 `byte_cap` / 被中止 / 超时）。那种情况下退出码没有语义
//! （多半是 `None`），已解析到的结果照常渲染 + notice，**不是错误** ——
//! 「数够 limit 主动停」若被当成失败，工具就废了（设计 §1.4）。
//!
//! # glob：两条后端都不许静默给错答案
//!
//! `--glob` 在 `rg` 与 `fd` 上都是**按原样**匹配，而搜索根带了路径前缀，故含 `/` 的 glob
//! （`src/*.rs`）**一条都匹配不到**却退出码为 0 —— 静默空集，比报错更坏。
//!
//! - 原生 `rg`：补 `**/` 前缀归一（见 `normalize_rg_glob`）；以 `/` 开头无法表达
//!   「锚定搜索根」（rg 锚定 cwd）→ **明确报错**。
//! - 降级 `grep`：`--include` 只匹配 basename，含 `/` 的 glob 一律**明确报错**。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec, append_notices};

use crate::exec::{Bounded, SearchBackends, Stop, run_bounded};
use crate::search::{BYTE_CAP, DEADLINE, EXCLUDED_DIRS, relativize};
use crate::workspace_path::{absolute_notice, attach_notice};

/// 一次调用返回的命中**条数**上限（与 `byte_cap` / `head` 是三个不同单位的上限）。
const DEFAULT_LIMIT: usize = 100;

/// 命中行上下文的行数（`rg --context` / `grep -C`）。
const DEFAULT_CONTEXT: usize = 0;

/// `grep` 单条结果的**专用字节预算**（比通用 `ys_tool::MAX_BYTES` 更紧）。
///
/// 收紧是刻意的：搜索结果一旦偏大，回喂给模型的 token 就是主要成本，而模型很少用得上全部命中。
/// 超预算即截断，并在截断 notice 里给出**可执行的收窄指引**（否则「截断而无指针」是最坏形态）。
const GREP_MAX_RESULT_BYTES: usize = 16 * 1024;

/// `context` 参数上限：`context=N` 把每条命中扩成 `2N+1` 行，是输出爆炸的主因之一，故设硬上限。
const MAX_CONTEXT: usize = 5;

pub struct GrepTool {
    workspace: PathBuf,
    /// **注入**的探测结果（不在工具内部调 `detect()` 读进程 env，否则降级路径无法确定性测试）。
    backends: SearchBackends,
}

impl GrepTool {
    pub fn new(workspace: PathBuf, backends: SearchBackends) -> Self {
        Self {
            workspace,
            backends,
        }
    }
}

/// 一条 `begin`/`end`/`summary` 之外的事件。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Match,
    Context,
}

struct Entry {
    kind: Kind,
    /// **已归一**（相对搜索根）的展示路径。
    path: String,
    line: u64,
    text: String,
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        // 描述里的输出上限必须由实现常量插值：模型按描述规划取数，字面量一旦与常量分叉，
        // 模型就会按错误预算规划。grep 的字节上限是**专用**预算 `GREP_MAX_RESULT_BYTES`
        // （比通用 `ys_tool::MAX_BYTES` 更紧），行数上限仍是通用的 `ys_tool::MAX_LINES`。
        let description = format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line \
             numbers. Respects .gitignore when ripgrep (rg) is available; the POSIX fallback \
             (grep -r) does NOT and instead skips .git/target/node_modules/dist/build/.venv. \
             A `glob` containing '/' (e.g. 'src/*.rs') matches at any depth under the search \
             target when rg is available; the POSIX fallback rejects such globs (use a basename \
             glob like '*.rs' there). Returns at most `limit` matches (default 100); long lines \
             are cut to 500 chars and output is capped to {} lines / {} KiB.",
            ys_tool::MAX_LINES,
            GREP_MAX_RESULT_BYTES / 1024,
        );
        ToolSpec::new(
            "grep",
            description,
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                    "path": { "type": "string", "description": "Single directory or file to search (default: the workspace root). Relative paths resolve against the workspace root. For several targets use `paths` instead - do NOT chain greps through bash." },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Several directories/files to search in ONE call (e.g. [\"crates/a\", \"docs\"]). Relative paths resolve against the workspace root. Supersedes `path`. Prefer this over chaining `grep a && grep b` through bash."
                    },
                    "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                    "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                    "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                    "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                    "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
                },
                "required": ["pattern"]
            }),
        )
    }

    /// 只读工具：同一批里可与其它只读工具并发执行（`BasicLoop` Step 9 要求批内**每一个**
    /// 都为 `true` 才走并行 ⇒ 混入 `bash`/`write`/`edit` 的批次自动退回串行）。
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let pattern = input["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'pattern'".into()))?;
        // **多目标**：`paths` 非空时优先（`path` 被它覆盖）。
        //
        // 存在理由（实测）：真实会话里 316 条 bash 命令含 grep，其中 56 条在**一条命令里
        // 串了多个 grep**（`grep a && grep b`）或给了**多个目标目录**
        // （`grep -rn pat dir1 dir2 dir3`）—— 结构化 `grep` 只吃一个 `path`，
        // 模型只能退回 bash，代价是丢掉 `.gitignore` 语义与统一截断形态。
        let targets: Vec<String> = input
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let path = input["path"].as_str().unwrap_or(".");
        let multi = targets.len() >= 2;
        // 单目标：保持既有取数（`path`，缺省 `.`）；多目标：用 `paths`。
        let search_paths: Vec<std::path::PathBuf> = if multi {
            targets.iter().map(|t| self.workspace.join(t)).collect()
        } else {
            vec![self.workspace.join(path)]
        };
        // 相对化根：**多目标时用 workspace**（所有目标都在其下 ⇒ 回显带目标前缀，
        // 不会出现「三个目标下的 `lib.rs` 看起来同名」的歧义）；
        // 单目标时沿用该目标本身（**既有行为逐字节不变**）。
        let rel_root = if multi {
            self.workspace.clone()
        } else {
            search_paths[0].clone()
        };
        // 绝对路径提示：多目标时查第一条绝对路径。
        let notice_probe = if multi {
            targets.first().map(String::as_str).unwrap_or(path)
        } else {
            path
        };
        let path_notice = absolute_notice("grep", notice_probe, &self.workspace);
        let glob = input["glob"].as_str();
        let ignore_case = input["ignoreCase"].as_bool().unwrap_or(false);
        let literal = input["literal"].as_bool().unwrap_or(false);
        let requested_context =
            input["context"].as_u64().unwrap_or(DEFAULT_CONTEXT as u64) as usize;
        let context = requested_context.min(MAX_CONTEXT);
        let context_clamped = requested_context > MAX_CONTEXT;
        // `limit` 至少为 1：0 会让「已截断」判定恒真、并产出 `Use limit=0 for more` 这类废话。
        let limit = (input["limit"].as_u64().unwrap_or(DEFAULT_LIMIT as u64) as usize).max(1);

        let (label, argv, native) = match &self.backends.rg {
            Some(rg) => {
                // 以 `/` 开头的 glob 在 rg 里**锚定 cwd**（= workspace，见 `run_bounded` 的
                // `current_dir`），**不是**搜索根 —— `path=apps` + `/src/*.rs` 会去搜
                // `<workspace>/src/` 而非 `apps/src/`。rg 的 glob 无法表达「锚定搜索根」，
                // 故**明确报错**（与降级路径同款处理），绝不静默给错答案。
                if let Some(g) = glob.filter(|g| g.starts_with('/')) {
                    return Ok(attach_notice(
                        ToolResult {
                            content: format!(
                                "glob '{g}' starts with '/' — ripgrep anchors such globs to the \
                                 working directory, not to the search root. Drop the leading '/' \
                                 (e.g. 'src/*.rs') to match at any depth under the search root."
                            ),
                            is_error: true,
                        },
                        path_notice.as_deref(),
                    ));
                }
                (
                    "ripgrep",
                    rg_argv(pattern, &search_paths, glob, ignore_case, literal, context),
                    Some(rg.as_path()),
                )
            }
            None => {
                // `grep --include=GLOB` 只按 **basename** 匹配，含 `/` 的 glob（如 `src/*.rs`）
                // 会一条都匹配不到。**不能静默给错答案** —— 返回明确的错误结果。
                if glob.is_some_and(|g| g.contains('/')) {
                    return Ok(attach_notice(
                        ToolResult {
                            content: format!(
                                "glob '{g}' contains '/' which the POSIX fallback (grep --include) \
                                 cannot express — it matches basenames only. Retry with a basename \
                                 glob (e.g. '*.rs'), or install ripgrep (rg) for path-aware globs.",
                                g = glob.unwrap_or_default()
                            ),
                            is_error: true,
                        },
                        path_notice.as_deref(),
                    ));
                }
                (
                    "grep",
                    grep_argv(pattern, &search_paths, glob, ignore_case, literal, context),
                    None,
                )
            }
        };

        let prog = native.unwrap_or_else(|| Path::new(label));
        let bounded = run_bounded(
            prog,
            &argv,
            &self.workspace,
            BYTE_CAP,
            DEADLINE,
            ctx.boundary,
        )
        .await?;

        let entries = parse(&bounded.stdout, native.is_some(), &rel_root);

        Ok(attach_notice(
            self.build_result(&bounded, &entries, limit, context, context_clamped, label),
            path_notice.as_deref(),
        ))
    }
}

impl GrepTool {
    /// 退出码判定 + 渲染。**先看 `stop` 再看 `code`**（设计 §1.4）。
    fn build_result(
        &self,
        bounded: &Bounded,
        entries: &[Entry],
        limit: usize,
        context: usize,
        context_clamped: bool,
        label: &str,
    ) -> ToolResult {
        let killed_by_us = bounded.stop != Stop::Eof;

        if !killed_by_us {
            match bounded.code {
                Some(0) => {}
                // 1 = 无命中。**不是错误** —— 否则模型每次搜空都以为工具坏了。
                Some(1) => {
                    return ToolResult {
                        content: "No matches found".into(),
                        is_error: false,
                    };
                }
                // ≥2 = 真出错（路径不存在、正则非法…）：stderr 前 4 KiB 是给模型看的线索。
                _ => return exit_error(label, bounded),
            }
        }

        let matches = entries.iter().filter(|e| e.kind == Kind::Match).count();

        // ★ 退出码 0 却一个 `type:"match"` 都解析不出来 = **输出格式漂移**。
        // 这正是「静默当无命中」最危险的形态：模型会据此断言「代码里没有这个东西」。
        if matches == 0 && !killed_by_us {
            return ToolResult {
                content: format!(
                    "{label} exited 0 but produced no parseable match events — its output format \
                     may have changed. Refusing to report a false 'no matches'.",
                ),
                is_error: true,
            };
        }

        let (lines, limit_reached, lines_truncated, total_matches) =
            render_entries(entries, limit, context);
        // ★ 专用预算（收紧）：grep 单条结果按 `GREP_MAX_RESULT_BYTES` 截断，低于通用 `MAX_BYTES`。
        let truncated = ys_tool::head_lines(&lines, ys_tool::MAX_LINES, GREP_MAX_RESULT_BYTES);
        let rendered_bytes: usize = lines.iter().map(|l| l.len()).sum();
        // 字节/行截断一旦发生，`limit` 就不再是瓶颈：此时再建议加大 limit 是**错误建议**
        // （16 KiB 预算先于 limit 用尽，加大 limit 一条也多拿不到，只多烧一轮）。
        let byte_truncated = truncated.hit.is_some() || bounded.stdout.len() >= BYTE_CAP;

        let mut notices: Vec<String> = Vec::new();
        if context_clamped {
            notices.push(format!(
                "context capped to {MAX_CONTEXT} (you asked for more). Use a tighter pattern to see \
                 more surrounding lines"
            ));
        }
        if byte_truncated {
            // 合并为**一条可执行**的 notice：总数 + 实际展示量 + 「加大 limit 无用」+ 收窄指引。
            // 报告的是渲染后的**行数**（含 context 行），与 `head_lines` 的计数口径一致。
            let cap = if truncated.hit.is_some() {
                format!("{} KiB", GREP_MAX_RESULT_BYTES / 1024)
            } else {
                format!("{} MiB", BYTE_CAP / (1024 * 1024))
            };
            notices.push(format!(
                "{total_matches} matches found; showing the first {} lines (output capped at {cap}). \
                 Raising `limit` will not return more — narrow with `glob`, `path`, or a tighter \
                 `pattern`.",
                truncated.shown_lines
            ));
        } else if limit_reached {
            // 仅 limit 触发时加大 limit 是**正确**建议；顺带报出总数，让模型知道还差多少。
            notices.push(format!(
                "{total_matches} matches found; showing {limit}. Use limit={} for more",
                limit.saturating_mul(2)
            ));
        }
        // ★ F1：用 stdout 长度判定，不能依赖 `stop == Cap`（越限与自然退出可能撞在同一轮询窗口）。
        if bounded.stdout.len() >= BYTE_CAP {
            notices.push(format!(
                "{}MB output limit reached, results may be incomplete",
                BYTE_CAP / (1024 * 1024)
            ));
        }
        if truncated.hit.is_some() {
            notices.push(format!(
                "Truncated {} lines, showing first {}. Narrow the search (smaller context, \
                 tighter pattern/path, or add a `glob`) to see the rest",
                truncated.total_lines - truncated.shown_lines,
                truncated.shown_lines
            ));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {} chars. Use the read tool to see full lines",
                ys_tool::GREP_MAX_LINE_CHARS
            ));
        }
        // 软阈值（**仅未硬截断时**）：提示模型「结果偏大、下次收窄」，内容不变。
        //
        // 硬截断时不再重复软提示 —— 截断 notice 已带收窄指引。
        if truncated.hit.is_none() && rendered_bytes >= ys_tool::SOFT_RESULT_BYTES {
            notices.push(ys_tool::size_notice(rendered_bytes, Some(matches)));
        }
        match bounded.stop {
            Stop::Aborted => notices.push("Search aborted".into()),
            // 插话提前收口：如实说明搜索**没跑完**，不得让它看起来像「无命中」。
            Stop::Steered => notices.push(
                "Search stopped early because the user sent a message — results are 
                 incomplete. That message will enter your context as soon as this batch of tool 
                 calls finishes; read it before re-running the search"
                    .into(),
            ),
            Stop::TimedOut => notices.push(format!(
                "Search timed out after {}s, results may be incomplete",
                DEADLINE.as_secs()
            )),
            Stop::Eof | Stop::Cap => {}
        }

        // 零命中时**只在未被我们杀**的情况下说「无命中」：被中止/超时收场时
        // 「No matches found」是误导（搜索根本没跑完），此时内容仅剩 notice，如实说明。
        let mut content = truncated.content;
        if content.is_empty() && matches == 0 && !killed_by_us {
            content = "No matches found".into();
        }
        append_notices(&mut content, &notices);

        ToolResult {
            content,
            is_error: false,
        }
    }
}

/// `rg --json --line-number --color=never --hidden --glob=!.git/ […] -- PATTERN PATH`。
///
/// **`--hidden` 必须配 `--glob=!.git/`**：前者让 `.github/workflows/*.yml` 这类
/// 隐藏路径可搜（它们恰是配置排查的高频目标），后者挡住 `.git/` 对象库
/// （否则一次搜索就把 commit 内容倒进上下文）。两个参数是一对，单独出现任一个都是错的。
///
/// 用户给的 `--glob` **不覆盖**这个排除：排除用 `--glob=!.git/`（等号形式）单独占一个
/// 位置参数，不跟用户的值争参数位。
///
/// **绝不省路径参数**（F3）：`rg` 在非 tty 下会改读 stdin，挂到 deadline。
fn rg_argv(
    pattern: &str,
    paths: &[std::path::PathBuf],
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: usize,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "--json".into(),
        "--line-number".into(),
        "--color=never".into(),
        "--hidden".into(),
        "--glob=!.git/".into(),
    ];
    if ignore_case {
        argv.push("--ignore-case".into());
    }
    if literal {
        argv.push("--fixed-strings".into());
    }
    if context > 0 {
        argv.push("--context".into());
        argv.push(context.to_string());
    }
    if let Some(g) = glob {
        argv.push("--glob".into());
        argv.push(normalize_rg_glob(g));
    }
    argv.push("--".into());
    argv.push(pattern.into());
    // **多目标**：rg 原生接受多个搜索路径（`rg PATTERN d1 d2 d3`）。
    for p in paths {
        argv.push(p.to_string_lossy().into_owned());
    }
    argv
}

/// 归一含 `/` 的 glob：补 `**/` 前缀，让它匹配「搜索根之下的**任意深度**」。
///
/// `rg --glob` 与 `fd --glob` 同病：模式含 `/` 时**按原样**匹配，而搜索根带了前缀
/// （`-- PATTERN PREFIX/PATH`，回显形如 `apps/coding-agent/src/main.rs`），故 `src/*.rs`
/// 一条都匹配不到 —— **静默返回空集**，比报错更坏（`find.rs::fd_argv` 的同类问题与同源修法）。
///
/// 实测（rg 15.1，搜索根为绝对路径）：`src/*.rs` 命中 0 条；`**/src/*.rs` 命中
/// `apps/coding-agent/src/*.rs` 等目标，且**不过匹配**（`mysrc/x.rs` / `xxsrc/y.rs` 不中）。
/// 不含 `/` 的 glob（`*.rs`）保持 rg 的 basename 语义**不动**。
///
/// 以 `/` 开头的 glob 在调用方**已被拒收**（rg 会锚定 cwd 而非搜索根），故此处不处理；
/// 已带 `**/` 前缀的本就是「任意深度」语义，原样传递。
fn normalize_rg_glob(glob: &str) -> String {
    if glob.contains('/') && !glob.starts_with("**/") {
        format!("**/{glob}")
    } else {
        glob.to_string()
    }
}

/// `grep -r -n -I [--exclude-dir=…] [--include=GLOB] [-i] [-F] [-C N] -- PATTERN PATH`。
///
/// 排除目录**只作用于降级路径**：`rg` 自带 `.gitignore`，POSIX `grep` 没有。
fn grep_argv(
    pattern: &str,
    paths: &[std::path::PathBuf],
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: usize,
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["-r".into(), "-n".into(), "-I".into()];
    for dir in EXCLUDED_DIRS {
        argv.push(format!("--exclude-dir={dir}"));
    }
    if let Some(g) = glob {
        argv.push(format!("--include={g}"));
    }
    if ignore_case {
        argv.push("-i".into());
    }
    if literal {
        argv.push("-F".into());
    }
    if context > 0 {
        argv.push("-C".into());
        argv.push(context.to_string());
    }
    argv.push("--".into());
    argv.push(pattern.into());
    // **多目标**：GNU grep 也接受多个搜索路径（`grep -rn PATTERN d1 d2`）。
    for p in paths {
        argv.push(p.to_string_lossy().into_owned());
    }
    argv
}

/// 解析 stdout：`rg --json` 走 JSON Lines，`grep` 走 `path:line:text` / `path-line-text`。
///
/// 一律**按完整行**解码（TCP/管道分片可能切在多字节字符中间 —— 字节级切分只在这里发生）。
fn parse(stdout: &[u8], native: bool, search_root: &Path) -> Vec<Entry> {
    let text = String::from_utf8_lossy(stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        let entry = if native {
            parse_rg_event(line, search_root)
        } else {
            parse_grep_line(line, search_root)
        };
        if let Some(entry) = entry {
            entries.push(entry);
        }
    }
    entries
}

/// 一行 `rg --json` 事件 → 条目（`begin` / `end` / `summary` 返回 `None`）。
fn parse_rg_event(line: &str, search_root: &Path) -> Option<Entry> {
    let event: Value = serde_json::from_str(line).ok()?;
    let kind = match event["type"].as_str()? {
        "match" => Kind::Match,
        "context" => Kind::Context,
        _ => return None,
    };
    let data = &event["data"];
    let raw_path = data["path"]["text"].as_str()?;
    let line_number = data["line_number"].as_u64()?;
    // `lines.text` 可含多行（`-U` 多行匹配），此处只取本次事件的行。
    let text = data["lines"]["text"]
        .as_str()?
        .trim_end_matches(['\n', '\r']);
    Some(Entry {
        kind,
        path: relativize(raw_path, search_root),
        line: line_number,
        text: text.to_string(),
    })
}

/// 一行 `grep` 输出 → 条目。
///
/// 形态：命中 `path:LINE:text`；上下文 `path-LINE-text`（`-C N`）。路径里可能含 `:`（
/// Windows 盘符、含冒号的文件名），故**不用 `splitn`**，而是找「分隔符 + 纯数字 + 同种分隔符」
/// 的位置。先找命中形态（`:`）再找上下文形态（`-`）：`-` 形态判别力弱，
/// 形如 `2026-09-16-x.md:12:foo` 的路径会被 `-` 规则误判，故必须让 `:` 形态优先。
fn parse_grep_line(line: &str, search_root: &Path) -> Option<Entry> {
    let bytes = line.as_bytes();
    let candidate = |sep: u8| -> Option<(usize, usize, u64)> {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == sep {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                // 数字串溢出 u64（>20 位）只可能出现在文本里，跳过该候选继续找，
                // **不能**因此放弃整行（`?` 会让扫描提前收束）。
                if j > i + 1
                    && j < bytes.len()
                    && bytes[j] == sep
                    && let Ok(number) = line[i + 1..j].parse::<u64>()
                {
                    return Some((i, j, number));
                }
            }
            i += 1;
        }
        None
    };

    let (kind, start, sep_len, number) = match candidate(b':') {
        Some((i, j, n)) => (Kind::Match, i, j, n),
        None => match candidate(b'-') {
            Some((i, j, n)) => (Kind::Context, i, j, n),
            None => return None,
        },
    };

    let path = relativize(&line[..start], search_root);
    let text = line[sep_len + 1..].to_string();
    Some(Entry {
        kind,
        path,
        line: number,
        text,
    })
}

/// 把条目渲染成输出行；`limit` 之外的命中丢弃（**context 行不计入 limit**）。
///
/// 返回 `(行, 是否因 limit 截断, 是否有超长行被截, 命中总数)`。末位是**截断前**的命中总数：
/// notice 需要区分「limit 之外还有命中」与「字节预算装不下」，而 `limit` 只能看到前者。
/// 上下文窗口按「同一文件、行号连续」收集，且不重复输出已被上一条命中的后置上下文覆盖过的行。
fn render_entries(
    entries: &[Entry],
    limit: usize,
    context: usize,
) -> (Vec<String>, bool, bool, usize) {
    let total_matches = entries.iter().filter(|e| e.kind == Kind::Match).count();
    let limit_reached = total_matches > limit;

    let mut lines: Vec<String> = Vec::new();
    let mut lines_truncated = false;
    let mut emitted_upto = 0usize;
    let mut taken = 0usize;
    let mut i = 0usize;

    while i < entries.len() {
        if entries[i].kind != Kind::Match {
            i += 1;
            continue;
        }
        if taken >= limit {
            break;
        }

        // 前置上下文：向前收集同文件、行号连续、至多 `context` 行，且不越过已输出处。
        let mut start = i;
        while start > emitted_upto && i - start < context {
            let prev = &entries[start - 1];
            if prev.kind == Kind::Context
                && prev.path == entries[i].path
                && prev.line + 1 == entries[start].line
            {
                start -= 1;
            } else {
                break;
            }
        }

        for entry in &entries[start..i] {
            push_line(&mut lines, entry, false, &mut lines_truncated);
        }
        push_line(&mut lines, &entries[i], true, &mut lines_truncated);
        taken += 1;

        // 后置上下文：紧随命中、同文件、至多 `context` 行。
        let mut j = i + 1;
        while j < entries.len() && j - i <= context {
            let next = &entries[j];
            if next.kind != Kind::Context || next.path != entries[i].path {
                break;
            }
            push_line(&mut lines, next, false, &mut lines_truncated);
            j += 1;
        }
        emitted_upto = j;
        i = j;
    }

    (lines, limit_reached, lines_truncated, total_matches)
}

/// 一行渲染：命中 `path:line: text`，上下文 `path-line- text`（与 Pi 同形）。
/// 超长行先过 `truncate::line`（**按字符**截，不切碎 UTF-8）。
fn push_line(out: &mut Vec<String>, entry: &Entry, is_match: bool, truncated: &mut bool) {
    let (text, cut) = ys_tool::line(&entry.text, ys_tool::GREP_MAX_LINE_CHARS);
    if cut {
        *truncated = true;
    }
    let sep = if is_match { ':' } else { '-' };
    out.push(format!("{}{sep}{}{sep} {}", entry.path, entry.line, text));
}

/// 非零退码（≥2）→ 模型可见的错误结果，附 stderr 前 4 KiB。
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

    use crate::test_support::{
        brief, ctx, ctx_with, fake_bin, guarded, rg_event_json, rg_match_jsonl, test_dir,
    };

    /// **注入假 `rg`** 的工具 —— 输出与退出码完全受控，不依赖开发机是否装了真 ripgrep。
    #[cfg(unix)]
    fn rg_tool(rg: &Path, workspace: &Path) -> GrepTool {
        GrepTool::new(
            workspace.to_path_buf(),
            SearchBackends {
                rg: Some(rg.to_path_buf()),
                fd: None,
            },
        )
    }

    /// **描述即契约**：spec 文案里的字节上限必须等于 `GREP_MAX_RESULT_BYTES`。
    ///
    /// 模型按描述里的预算规划取数；若文案写成通用上限（`ys_tool::MAX_BYTES` = 50 KiB）
    /// 而实现按 16 KiB 截断，模型会低估截断概率、给出错误的后续策略。
    ///
    /// 变异：把描述里的插值改回字面量 `50KB`（或任何与常量不符的值）⇒ 本用例变红。
    #[test]
    fn spec_byte_cap_matches_result_budget() {
        let tool = GrepTool::new(PathBuf::from("."), SearchBackends::posix_only());
        let d = tool.spec().description;
        let expected = format!("{} KiB", GREP_MAX_RESULT_BYTES / 1024);
        assert!(
            d.contains(&expected),
            "描述里的字节上限必须由 GREP_MAX_RESULT_BYTES 插值（应含 `{expected}`）；description = {d}"
        );
    }

    /// `--hidden` 与 `--glob=!.git/` 必须**成对出现**。
    ///
    /// 单独 `--hidden` ⇒ 搜进 `.git/` 对象库（token 灾难）；单独排除而无 `--hidden`
    /// ⇒ `.github/workflows/*.yml` 这类隐藏路径永远搜不到（配置排查的高频目标）。
    ///
    /// 变异：删掉 `--hidden` ⇒ 第一条断言变红；删掉 `--glob=!.git/` ⇒ 第二条变红。
    #[test]
    fn rg_argv_pairs_hidden_with_git_exclusion() {
        let argv = rg_argv("pat", &[PathBuf::from(".")], None, false, false, 0);
        assert!(argv.iter().any(|a| a == "--hidden"), "argv = {argv:?}");
        assert!(
            argv.iter().any(|a| a == "--glob=!.git/"),
            "必须挡住 .git/；argv = {argv:?}"
        );

        // 用户给 `glob` 时，排除仍在、且不与用户值争参数位。
        let with_glob = rg_argv("pat", &[PathBuf::from(".")], Some("*.rs"), false, false, 0);
        assert!(with_glob.iter().any(|a| a == "--glob=!.git/"));
        assert!(with_glob.windows(2).any(|w| w == ["--glob", "*.rs"]));
    }

    /// 端到端：真的能搜到隐藏路径，且真的搜不进 `.git/`。
    ///
    /// 用真 `rg` 跑一个临时 git 仓库（无 rg 时跳过 —— 这是后端缺失，不是本用例要验的）。
    ///
    /// 变异：从 `rg_argv` 删掉 `--glob=!.git/` ⇒ 第二段「不得出现 `.git/`」变红；
    /// 删掉 `--hidden` ⇒ 第一段「必须找到 ci.yml」变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn rg_searches_hidden_paths_but_not_dot_git() {
        let Some(rg) = SearchBackends::detect().rg else {
            return;
        };
        let dir = test_dir("grep-hidden");
        std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        std::fs::write(dir.join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        // 造一个真的 `.git` 目录（不是 worktree 的 gitfile），否则挡不挡都看不出来。
        std::fs::create_dir_all(dir.join(".git/refs")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let tool = rg_tool(&rg, &dir);

        let hidden = call(&tool, json!({ "pattern": "name: ci" })).await;
        assert!(
            hidden.content.contains("ci.yml"),
            "隐藏路径必须可搜；content = {}",
            hidden.content
        );

        let git = call(&tool, json!({ "pattern": "refs/heads" })).await;
        assert!(
            !git.content.contains(".git/"),
            ".git/ 内的内容不得进入结果；content = {}",
            git.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 走**降级路径**（真 `grep`）的工具。
    fn posix_tool(workspace: &Path) -> GrepTool {
        GrepTool::new(workspace.to_path_buf(), SearchBackends::posix_only())
    }

    /// 调用 + **挂起护栏**（不返回 = panic，而非测试挂死）。
    async fn call(tool: &GrepTool, input: Value) -> ToolResult {
        guarded("grep::call", tool.call(input, ctx()))
            .await
            .expect("工具调用不应返回 Err（错误一律走 is_error 结果）")
    }

    /// 造一份 **>1 MiB** 的**合法** `rg --json` match JSONL，返回字节数。
    #[cfg(unix)]
    fn write_big_jsonl(path: &Path) -> usize {
        let unit = format!(
            "{}\n",
            rg_match_jsonl("big/f.txt", 1, &"padding ".repeat(8))
        );
        let mut buf = String::with_capacity(BYTE_CAP + 2 * unit.len());
        while buf.len() <= BYTE_CAP {
            buf.push_str(&unit);
        }
        std::fs::write(path, &buf).unwrap();
        buf.len()
    }

    // ── ⓪ C1：多目标（`paths`）─────────────────────────────────────────────

    /// **C1**：`paths` 把多个目标一次带给 rg（而不是让模型串 bash）。
    ///
    /// 承重的三点：
    ///  1. **argv 真的带上了每个目标** —— 假 `rg` 用 `printf '...%s...' "$*"` 把
    ///     **收到的全部参数**写进命中文本，故断言文本里同时出现两个目标
    ///     （光看回显路径验不出 argv —— 早期版本的假 rg 忽略 argv，变异漏网）；
    ///  2. 回显路径相对化到 **workspace**（不是第一个目标）⇒ 多目标的文件名不歧义；
    ///  3. 退出码 0 ⇒ 不报错。
    ///
    /// 变异：① `search_paths` 只取第一个目标 ⇒ 断言 1 变红；
    ///       ② `argv` 只推第一个路径 ⇒ 断言 1 变红；
    ///       ③ `rel_root` 换成第一个目标 ⇒ 断言 2 变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn multi_target_paths_reach_rg_argv_and_relativize_to_workspace() {
        let dir = test_dir("multi_target");
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join("crates/a")).unwrap();
        std::fs::create_dir_all(ws.join("docs")).unwrap();

        // 假 rg：一行合法 `rg --json` 命中事件，`text` 用 `$*` 填（= 收到的全部参数）。
        // 路径用**绝对**形态（在 workspace 下）⇒ 相对化根不同会给出不同回显。
        let abs = ws.join("crates/a/f.txt");
        let rg = fake_bin(
            &dir,
            "rg",
            &format!(
                "printf '{{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"{}\"}},\
                 \"line_number\":7,\"lines\":{{\"text\":\"%s\"}}}}}}\\n' \"$*\"\nexit 0\n",
                abs.display()
            ),
        );

        let tool = rg_tool(&rg, &ws);
        let res = call(
            &tool,
            json!({ "pattern": "x", "paths": ["crates/a", "docs"] }),
        )
        .await;
        assert!(!res.is_error, "多目标不应报错：{}", res.content);

        // ① argv 含**两个**目标（不是只有第一个）。
        assert!(
            res.content.contains("crates/a") && res.content.contains("docs"),
            "argv 必须同时带上两个目标，实际：{}",
            res.content
        );
        // ② 相对化到 workspace ⇒ 带 `crates/a/` 前缀（单目标会被剥掉）。
        assert!(
            res.content.contains("crates/a/f.txt"),
            "多目标应相对化到 workspace（带目标前缀），实际：{}",
            res.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 单目标：**既有行为逐字节不变**（相对化根仍是该目标本身）。
    ///
    /// 变异：把 `rel_root` 无条件改成 workspace ⇒ 本用例与既有用例一起变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn single_target_still_relativizes_to_that_target() {
        let dir = test_dir("single_target");
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join("crates/a")).unwrap();

        let abs = ws.join("crates/a/f.txt");
        let out = rg_match_jsonl(&abs.to_string_lossy(), 7, "hit");
        let rg = fake_bin(&dir, "rg", &format!("cat <<'EOF'\n{out}\nEOF\nexit 0\n"));

        let tool = rg_tool(&rg, &ws);
        let res = call(&tool, json!({ "pattern": "x", "path": "crates/a" })).await;
        assert!(!res.is_error, "{}", res.content);
        // 单目标：相对化根 = 该目标 ⇒ **不带** `crates/a/` 前缀。
        assert!(
            !res.content.contains("crates/a/"),
            "单目标应相对化到该目标本身（既有行为不变），实际：{}",
            res.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── ① 退出码四态（设计 §1.4）：**先看 stop，再看 code** ────────────────

    /// `Eof + 1` = **无命中**，不是错误 —— 否则模型每次搜空都会以为工具坏了。
    ///
    /// 变异：把 `Some(1) =>` 分支并进 `_ => exit_error(..)`（即「退出码 1 当错误」）
    /// → 本用例的 `is_error == false` 与 `No matches found` 双双变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_eof_exit_1_is_no_matches_not_error() {
        let dir = test_dir("grep_exit1");
        std::fs::create_dir_all(&dir).unwrap();
        let rg = fake_bin(&dir, "rg", "exit 1\n");

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "needle" })).await;

        assert!(
            !result.is_error,
            "退出码 1 = 无命中，绝不是错误；实际：{}",
            brief(&result.content)
        );
        assert_eq!(result.content.trim(), "No matches found");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// workspace 内绝对搜索路径追加相对路径事实；相对路径不追加。
    ///
    /// 变异：删掉 `absolute_notice` 或 `build_result` 里的 `attach_notice` ⇒ 第一条断言变红。
    #[tokio::test]
    async fn absolute_workspace_path_adds_relative_hint() {
        let dir = test_dir("grep_absolute_hint");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "needle\n").unwrap();
        let tool = posix_tool(&dir);

        let absolute = call(
            &tool,
            json!({
                "pattern": "needle",
                "path": file.to_string_lossy()
            }),
        )
        .await;
        assert!(
            absolute
                .content
                .contains("grep: this path is inside the workspace root"),
            "content = {}",
            brief(&absolute.content)
        );
        assert!(absolute.content.contains("`a.txt`"));

        let relative = call(&tool, json!({ "pattern": "needle", "path": "a.txt" })).await;
        assert!(
            !relative.content.contains("workspace root"),
            "相对路径不应触发注记；content = {}",
            brief(&relative.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Eof + ≥2` = 真出错（路径不存在 / 正则非法…）：`is_error == true` + stderr 原样可见。
    ///
    /// 变异：把 `_ => return exit_error(label, bounded)` 改成与 `Some(1)` 同款的
    /// 「无命中」→ 本用例变红（`is_error` 变 false、stderr 丢失）。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_eof_exit_2_is_error_and_surfaces_stderr() {
        let dir = test_dir("grep_exit2");
        std::fs::create_dir_all(&dir).unwrap();
        let rg = fake_bin(
            &dir,
            "rg",
            "echo 'regex parse error: unclosed group' >&2\nexit 2\n",
        );

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "(" })).await;

        assert!(result.is_error, "退出码 2 = 出错");
        assert!(
            result.content.contains("regex parse error: unclosed group"),
            "stderr 是给模型的唯一线索；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Eof + 0` 却**零 `type:"match"` 事件** = 输出格式漂移 → **报错**。
    ///
    /// 这是「静默当无命中」最危险的形态：模型会据此断言「代码里没有这个东西」。
    ///
    /// 变异：删掉 `if matches == 0 && !killed_by_us { … }` 那段 → 本用例变红
    /// （内容变成 `No matches found`、`is_error` 变 false）。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_eof_0_without_match_events_is_format_drift_error() {
        let dir = test_dir("grep_drift");
        std::fs::create_dir_all(&dir).unwrap();
        // 合法 JSON、也是 rg 真会吐的事件类型（begin / summary），但没有一条 match。
        let body = concat!(
            "printf '%s\\n' '{\"type\":\"begin\",\"data\":{\"path\":{\"text\":\"a.rs\"}}}'\n",
            "printf '%s\\n' '{\"type\":\"summary\",\"data\":{\"stats\":{}}}'\n",
            "exit 0\n",
        );
        let rg = fake_bin(&dir, "rg", body);

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "needle" })).await;

        assert!(
            result.is_error,
            "退出码 0 但零 match 事件 = 格式漂移，必须报错而不是「无命中」；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("no parseable match events"),
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`stop != Eof`（是我们杀的）→ 忽略退出码**，已解析到的结果照常渲染，**不是错误**。
    ///
    /// 构造：假 rg 先吐一条命中、`touch` 出哨兵（证明写入**已落进管道**），再挂住
    /// （`exec` 让 `sh` 自身变成 `sleep` —— 避免孙进程攥住管道写端）；测试**等哨兵出现后**
    /// 才投 `Boundary::Abort` → `stop == Aborted` 且 `code == None`。
    /// 「数够 limit 主动停」走的正是这条路径，若把这种 `code` 当回事，工具就废了。
    ///
    /// 用哨兵而不是「睡固定 200ms 再投 Abort」：后者在负载高的机器上（并行跑造 2 万文件那类
    /// 用例）可能在子进程被调度到**之前**就把它杀掉，stdout 恒空 → 用例 flaky（实测出现过）。
    ///
    /// 变异：去掉 `let killed_by_us = bounded.stop != Stop::Eof;` 守卫（无条件按 `code` 判定）
    /// → `code == None` 落进 `_ => exit_error` → 本用例的 `is_error == false` 变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_killed_by_us_ignores_code_and_keeps_results() {
        let dir = test_dir("grep_killed");
        std::fs::create_dir_all(&dir).unwrap();
        let hit = rg_match_jsonl("src/a.rs", 7, "hello");
        let wrote = dir.join("wrote.sentinel");
        let rg = fake_bin(
            &dir,
            "rg",
            &format!(
                "printf '%s\\n' '{hit}'\ntouch '{}'\nexec sleep 30\n",
                wrote.display()
            ),
        );

        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        let wait_for = wrote.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                if wait_for.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            producer.push(Boundary::Abort);
        });

        let tool = rg_tool(&rg, &dir);
        let result = guarded(
            "grep::call(abort)",
            tool.call(json!({ "pattern": "hello" }), ctx_with(&*boundary)),
        )
        .await
        .expect("工具调用不应返回 Err");

        assert!(
            wrote.exists(),
            "前置条件：假 rg 必须先吐出命中（哨兵 {} 未出现）",
            wrote.display()
        );
        assert!(
            !result.is_error,
            "被我们自己杀掉不是错误；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("src/a.rs:7: hello"),
            "已解析到的结果必须照常渲染；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("Search aborted"),
            "收场原因必须显式报出；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `stop != Eof` 且零命中 → 收场原因是「被中止」，**不得**拼出 `No matches found`
    /// （搜索根本没跑完，那是误导）。
    ///
    /// 变异：去掉 `content.is_empty() && matches == 0 && !killed_by_us` 里的 `!killed_by_us`
    /// → 本用例变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_aborted_run_does_not_claim_no_matches() {
        let dir = test_dir("grep_aborted");
        std::fs::create_dir_all(&dir).unwrap();
        // 不吐任何东西就挂住；`exec` 让 sh 自身变成 sleep（不留攥着管道写端的孙进程）。
        let rg = fake_bin(&dir, "rg", "exec sleep 30\n");

        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(Boundary::Abort);
        });

        let tool = rg_tool(&rg, &dir);
        let result = guarded(
            "grep::call(abort)",
            tool.call(json!({ "pattern": "needle" }), ctx_with(&*boundary)),
        )
        .await
        .expect("工具调用不应返回 Err");

        assert!(!result.is_error, "被我们自己杀掉不是错误");
        assert!(
            !result.content.contains("No matches found"),
            "被中止时不得声称「无命中」；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("Search aborted"),
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ② F1：上限判定看 **`stdout` 长度**，不看 `stop == Cap` ─────────────
    /// 假 rg「吐 >1 MiB 合法 JSONL 后**自然退出**」→ `stop == Eof` 而 stdout 已被截到
    /// `BYTE_CAP`。工具**仍必须**报出 byte_cap notice —— 证明用的是 `stdout.len() >= BYTE_CAP`。
    ///
    /// 前半段直接跑 [`run_bounded`] 把「`stop == Eof` 且 `stdout.len() == BYTE_CAP`」这个
    /// 前提**钉死在测试里**（越限后子进程在下一次轮询窗口（100ms）之前就退了，
    /// 主循环先查 `try_wait`，故先观察到的是 `Eof`）。
    ///
    /// 变异：把 `grep.rs` 的 `bounded.stdout.len() >= BYTE_CAP` 改成 `bounded.stop == Stop::Cap`
    /// → notice 消失 → 本用例变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_byte_cap_notice_fires_even_when_stop_is_eof() {
        let dir = test_dir("grep_bytecap");
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.jsonl");
        let size = write_big_jsonl(&big);
        assert!(
            size > BYTE_CAP,
            "前置条件：{size} 字节 > BYTE_CAP {BYTE_CAP}"
        );

        // 假 rg：一次 `cat` 把超限输出吐完（`cat` 极快，且脚本随后自然结束）。
        let rg = fake_bin(&dir, "rg", &format!("cat '{}'\n", big.display()));

        let bounded = guarded(
            "run_bounded",
            run_bounded(&rg, &[], &dir, BYTE_CAP, DEADLINE, None),
        )
        .await
        .expect("自然退出必须是 Ok");
        assert_eq!(
            bounded.stop,
            Stop::Eof,
            "越限后在同一轮询窗口内自然退出 → 先观察到的是 try_wait 的 Eof"
        );
        assert_eq!(bounded.stdout.len(), BYTE_CAP, "stdout 仍被截到上限");

        let result = call(
            &rg_tool(&rg, &dir),
            json!({ "pattern": "padding", "limit": 1_000_000u64 }),
        )
        .await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("1MB output limit reached"),
            "上限必须显式报出（判定依据是 stdout.len()，不是 stop == Cap）；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ③ 原生（`rg --json`）解析路径 ────────────────────────────────────

    /// 命中事件 → `path:line: text`，且路径按搜索根归一（剥掉绝对前缀）。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_native_renders_match_as_path_line_text() {
        let dir = test_dir("grep_native_match");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs");
        let hit = rg_match_jsonl(&abs.to_string_lossy(), 12, "let x = 1;");
        let rg = fake_bin(&dir, "rg", &format!("printf '%s\\n' '{hit}'\n"));

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "x" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert_eq!(
            result.content.trim(),
            "src/a.rs:12: let x = 1;",
            "路径必须相对搜索根归一（剥掉绝对前缀）；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 上下文事件按 `-C` 窗口收集，渲染用**不同的分隔符**（命中 `:`、上下文 `-`）。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_native_renders_context_window_with_dash_separator() {
        let dir = test_dir("grep_native_context");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs").to_string_lossy().into_owned();
        let events = [
            rg_event_json("context", &abs, 11, "before"),
            rg_event_json("match", &abs, 12, "let x = 1;"),
            rg_event_json("context", &abs, 13, "after"),
        ];
        let body: String = events
            .iter()
            .map(|e| format!("printf '%s\\n' '{e}'\n"))
            .collect();
        let rg = fake_bin(&dir, "rg", &body);

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "x", "context": 1 })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert_eq!(
            result.content.trim(),
            "src/a.rs-11- before\nsrc/a.rs:12: let x = 1;\nsrc/a.rs-13- after",
            "上下文窗口的渲染形态（`: ` vs `- `）；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `limit` 的单位是**条命中**：超出的命中被丢弃，且 notice **必须显式报出**（不静默）。
    /// 仅 limit 触发（总字节远小于 16 KiB）时，加大 limit 是**正确**建议，故保留
    /// `Use limit=…`，并顺带报出命中总数。
    ///
    /// 变异：把 `limit_reached` 的 notice 删掉 → 本用例的文案断言变红
    /// （结果集不变，正是「静默截断」的危险形态）。
    /// 变异：把「仅 limit」也走字节截断的合并文案（含 `Raising \`limit\` will not return more`）
    /// → 本用例「不含 Raising」断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_native_limit_truncates_and_notices() {
        let dir = test_dir("grep_native_limit");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs").to_string_lossy().into_owned();
        let body: String = (1..=3)
            .map(|i| {
                format!(
                    "printf '%s\\n' '{}'\n",
                    rg_match_jsonl(&abs, i, &format!("hit{i}"))
                )
            })
            .collect();
        let rg = fake_bin(&dir, "rg", &body);

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "hit", "limit": 2 })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert_eq!(
            result
                .content
                .lines()
                .filter(|l| l.contains(": hit"))
                .count(),
            2,
            "只渲染 limit 条；实际：{}",
            brief(&result.content)
        );
        assert!(
            result
                .content
                .contains("3 matches found; showing 2. Use limit=4 for more"),
            "截断必须显式报出；实际：{}",
            brief(&result.content)
        );
        assert!(
            !result
                .content
                .contains("Raising `limit` will not return more"),
            "仅 limit 触发时加大 limit 有效，不得说它无用；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **两条 notice 并存**时的拼接形态（S10：输出逐字节不变的另一半证力）。
    ///
    /// 既有 notice 用例全是单条，`append_notices` 内部的 `notices.join(". ")` 不参与
    /// → **分隔符不可观测**（变异成全绿）。本用例同时触发 `limit` 截断与长行截断，
    /// 让真实路径产出两条 notice，钉住 `[a. b]`（`.` + 空格）。
    ///
    /// 变异：`append_notices` 的 `join(". ")` → `join("; ")` → 本用例的尾部断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_two_notices_joined_by_dot_space() {
        let dir = test_dir("grep_two_notices");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs").to_string_lossy().into_owned();
        // 长于 `GREP_MAX_LINE_CHARS`(500) 的命中行 → 触发「超长行被截」notice。
        let long = "a".repeat(ys_tool::GREP_MAX_LINE_CHARS + 100);
        let body: String = (1..=2)
            .map(|i| format!("printf '%s\\n' '{}'\n", rg_match_jsonl(&abs, i, &long)))
            .collect();
        let rg = fake_bin(&dir, "rg", &body);

        // limit=1 且有 2 条命中 → 触发 `limit` notice；两条 notice 同时出现。
        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "a", "limit": 1 })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        let tail = result
            .content
            .rsplit_once("\n\n[")
            .map(|(_, rest)| rest)
            .expect("正文后应附 `\\n\\n[… . …]`");
        assert_eq!(
            tail,
            format!(
                "2 matches found; showing 1. Use limit=2 for more. \
                 Some lines truncated to {} chars. Use the read tool to see full lines]",
                ys_tool::GREP_MAX_LINE_CHARS
            ),
            "两条 notice 必须用 `. ` 连接；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 含 `/` 的 glob 必须**归一为 `**/` 前缀**后再交给 rg（T7 的 I-1）。
    ///
    /// 不归一时 rg 会按原样匹配「带搜索根前缀的路径」→ 一条都不中却退出码 0 = **静默空集**。
    /// 本用例断言两点：**argv 里传的是 `**/src/*.rs`**，且该次调用**报出了命中**
    /// （不是 `No matches found`）。
    ///
    /// 变异：`normalize_rg_glob` 改成原样返回 → argv 断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_native_glob_with_slash_is_normalized_to_double_star() {
        let dir = test_dir("grep_native_glob");
        std::fs::create_dir_all(&dir).unwrap();
        let dump = dir.join("argv.txt");
        let hit = rg_match_jsonl("src/a.rs", 1, "hit");
        let rg = fake_bin(
            &dir,
            "rg",
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{hit}'\n",
                dump.display()
            ),
        );

        let result = call(
            &rg_tool(&rg, &dir),
            json!({ "pattern": "hit", "glob": "src/*.rs" }),
        )
        .await;

        let dump_text = std::fs::read_to_string(&dump).expect("假 rg 应把 argv 落到 dump");
        let args: Vec<&str> = dump_text.lines().collect();
        let gi = args
            .iter()
            .position(|a| *a == "--glob")
            .expect("glob 必须传给 rg");
        assert_eq!(
            args[gi + 1],
            "**/src/*.rs",
            "含 '/' 的 glob 必须补 `**/` 前缀；实际 argv：{args:?}"
        );
        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/a.rs:1: hit"),
            "归一后的 glob 应能命中，不得静默报无命中；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 以 `/` 开头的 glob：rg 会把它**锚定到 cwd**（= workspace）而非搜索根 → **明确报错**，
    /// 且**在 spawn 之前**就拒收（不静默给错答案）。
    ///
    /// 变异：删掉那个 `glob.filter(|g| g.starts_with('/'))` 的 early return → 本用例变红
    /// （`is_error` 变 false，且假 rg 的哨兵文件出现 = 真去跑了）。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_native_glob_starting_with_slash_is_rejected_before_spawn() {
        let dir = test_dir("grep_native_absglob");
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = dir.join("spawned.txt");
        let rg = fake_bin(&dir, "rg", &format!("touch '{}'\n", sentinel.display()));

        let result = call(
            &rg_tool(&rg, &dir),
            json!({ "pattern": "x", "glob": "/src/*.rs" }),
        )
        .await;

        assert!(result.is_error, "必须明确报错，不能静默给错答案");
        assert!(
            result.content.contains("starts with '/'"),
            "实际：{}",
            brief(&result.content)
        );
        assert!(
            !sentinel.exists(),
            "拒收必须在 spawn 之前（哨兵 {} 被创建了）",
            sentinel.display()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **真 rg** 端到端：`glob=src/*.rs` 必须命中 `src/a.rs`，且**不过匹配**
    /// （`mysrc/b.rs`、`other/c.rs` 不中）—— 这是 I-1 的语义正身，假 rg 只能验 argv。
    ///
    /// 开发机没装 rg 时跳过（不依赖 PATH 的确定性用例已在上面）。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_real_rg_glob_with_slash_matches_at_depth_without_overmatching() {
        let Some(rg) = SearchBackends::detect().rg else {
            eprintln!("跳过：本机 PATH 上没有 rg");
            return;
        };
        let dir = test_dir("grep_real_rg");
        for sub in ["src", "mysrc", "other"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("src/a.rs"), "NEEDLE\n").unwrap();
        std::fs::write(dir.join("mysrc/b.rs"), "NEEDLE\n").unwrap();
        std::fs::write(dir.join("other/c.rs"), "NEEDLE\n").unwrap();

        let tool = rg_tool(&rg, &dir);
        let result = call(&tool, json!({ "pattern": "NEEDLE", "glob": "src/*.rs" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/a.rs"),
            "含 '/' 的 glob 必须命中搜索根之下的 src/；实际：{}",
            brief(&result.content)
        );
        assert!(
            !result.content.contains("mysrc") && !result.content.contains("other"),
            "不得过匹配；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ④ 降级（POSIX `grep`）路径 ───────────────────────────────────────

    /// 降级路径的**排除目录**生效：`node_modules/` 里的命中不得出现。
    ///
    /// `rg` 自带 `.gitignore`，POSIX `grep` 没有 —— 漏排除就是「搜进依赖树」的 token 灾难。
    ///
    /// 变异：`grep_argv` 不传 `--exclude-dir=node_modules`（或 `EXCLUDED_DIRS` 里删掉它）
    /// → 本用例变红。
    #[tokio::test]
    async fn test_grep_posix_excludes_node_modules() {
        let dir = test_dir("grep_posix_exclude");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("src/xx.txt"), "NEEDLE here\n").unwrap();
        std::fs::write(dir.join("node_modules/xx.txt"), "NEEDLE here\n").unwrap();

        let result = call(&posix_tool(&dir), json!({ "pattern": "NEEDLE" })).await;

        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/xx.txt"),
            "降级路径必须命中 src/；实际：{}",
            brief(&result.content)
        );
        assert!(
            !result.content.contains("node_modules"),
            "降级路径必须排除 node_modules；实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 降级路径**无命中**（`grep` 退出码 1）→ 不是错误。
    #[tokio::test]
    async fn test_grep_posix_no_match_is_not_error() {
        let dir = test_dir("grep_posix_nomatch");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/xx.txt"), "nothing here\n").unwrap();

        let result = call(&posix_tool(&dir), json!({ "pattern": "NEEDLE" })).await;

        assert!(
            !result.is_error,
            "无命中不是错误；实际：{}",
            brief(&result.content)
        );
        assert_eq!(result.content.trim(), "No matches found");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 降级路径：`--include` 只匹配 **basename**，含 `/` 的 glob 会**一条都不中**
    /// → 必须**明确报错**，不静默给错答案。
    ///
    /// 变异：删掉 `glob.is_some_and(|g| g.contains('/'))` 的 early return → 本用例变红
    /// （退化成 `No matches found`，而目录里其实**有**命中）。
    #[tokio::test]
    async fn test_grep_posix_glob_with_slash_is_rejected_not_silently_empty() {
        let dir = test_dir("grep_posix_glob");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "NEEDLE\n").unwrap();

        let result = call(
            &posix_tool(&dir),
            json!({ "pattern": "NEEDLE", "glob": "src/*.rs" }),
        )
        .await;

        assert!(
            result.is_error,
            "含 '/' 的 glob 在降级路径无法表达，必须报错；实际：{}",
            brief(&result.content)
        );
        assert!(
            result.content.contains("cannot express"),
            "实际：{}",
            brief(&result.content)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 结果规模：专用预算（真收窄）+ 软阈值 + context 上限 ──────────────

    /// **N1**：渲染超 `GREP_MAX_RESULT_BYTES`(16 KiB) ⇒ 正文被截到预算内，且 notice **带收窄指引**。
    ///
    /// 字节截断时 `limit` 已不是瓶颈（16 KiB 先于 limit 用尽），故 notice **不得**再给
    /// `Use limit=…` 的错误建议，必须报出总数并明确「加大 limit 不会返回更多」。
    ///
    /// 变异：把 `head_lines(.., GREP_MAX_RESULT_BYTES)` 换回 `ys_tool::MAX_BYTES`（50 KiB）
    /// → 「正文字节 ≤ 预算」断言变红（100 行 × ~390 B ≈ 38 KiB 不再被截）。
    /// 变异：删掉截断 notice 里的收窄措辞 → 「含 Narrow the search」断言变红。
    /// 变异：在字节截断时仍输出 `Use limit=…`（即把 `byte_truncated` 分支改回无条件
    /// `if limit_reached`）→ 「notice 不含 Use limit=」断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_truncates_to_result_budget_with_narrowing_hint() {
        let dir = test_dir("grep_budget");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs").to_string_lossy().into_owned();
        let body: String = (1..=100)
            .map(|i| {
                format!(
                    "printf '%s\\n' '{}'\n",
                    rg_match_jsonl(&abs, i, &"y".repeat(300))
                )
            })
            .collect();
        let rg = fake_bin(&dir, "rg", &body);

        let result = call(&rg_tool(&rg, &dir), json!({ "pattern": "y", "limit": 100 })).await;
        assert!(!result.is_error, "{}", brief(&result.content));

        let (body_text, notices) = result.content.rsplit_once("\n\n[").expect("应带 notice 块");
        // 按 `head_lines` 的计数规则（单行成本 = len，不含换行）核对字节预算。
        let body_bytes: usize = body_text.lines().map(|l| l.len()).sum();
        assert!(
            body_bytes <= GREP_MAX_RESULT_BYTES,
            "正文必须被截到专用预算内：{body_bytes} > {GREP_MAX_RESULT_BYTES}"
        );
        assert!(
            body_text.lines().count() < 100,
            "应确实发生了截断，实际行数 {}",
            body_text.lines().count()
        );
        assert!(
            notices.contains("Truncated") && notices.contains("Narrow the search"),
            "截断 notice 必须带收窄指引；实际：{notices}"
        );
        // 字节截断下加大 limit 拿不到更多：不得给出这条错误建议。
        assert!(
            !notices.contains("Use limit="),
            "字节截断时不得再建议加大 limit；实际：{notices}"
        );
        assert!(
            notices.contains("100 matches found")
                && notices.contains("Raising `limit` will not return more"),
            "合并 notice 必须报出总数并说明加大 limit 无收益；实际：{notices}"
        );
        assert!(
            notices.contains("`glob`")
                && notices.contains("`path`")
                && notices.contains("`pattern`"),
            "合并 notice 必须给出可执行的收窄指引；实际：{notices}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **N2**：结果偏大但**未被截断** ⇒ 软阈值 notice；小结果 ⇒ 无。
    ///
    /// 变异：去掉软阈值分支 → 大结果断言变红；把阈值判断写成恒真 → 小结果断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_soft_size_notice_only_when_large_and_not_truncated() {
        // 大但未超预算：~30 行 × ~390 B ≈ 12 KiB ∈ [8 KiB, 16 KiB)
        let big_dir = test_dir("grep_soft_big");
        std::fs::create_dir_all(&big_dir).unwrap();
        let big_abs = big_dir.join("src/a.rs").to_string_lossy().into_owned();
        let big_body: String = (1..=30)
            .map(|i| {
                format!(
                    "printf '%s\\n' '{}'\n",
                    rg_match_jsonl(&big_abs, i, &"z".repeat(300))
                )
            })
            .collect();
        let rg_big = fake_bin(&big_dir, "rg", &big_body);
        let big = call(
            &rg_tool(&rg_big, &big_dir),
            json!({ "pattern": "z", "limit": 100 }),
        )
        .await;
        assert!(!big.is_error, "{}", brief(&big.content));
        assert!(
            big.content.contains("result is large"),
            "大结果应带软阈值提示：{}",
            brief(&big.content)
        );
        assert!(
            !big.content.contains("Truncated"),
            "软提示不应与硬截断共存：{}",
            brief(&big.content)
        );

        // 小结果：3 行 ⇒ 无 size notice
        let small_dir = test_dir("grep_soft_small");
        std::fs::create_dir_all(&small_dir).unwrap();
        let small_abs = small_dir.join("src/a.rs").to_string_lossy().into_owned();
        let small_body: String = (1..=3)
            .map(|i| {
                format!(
                    "printf '%s\\n' '{}'\n",
                    rg_match_jsonl(&small_abs, i, "hit")
                )
            })
            .collect();
        let rg_small = fake_bin(&small_dir, "rg", &small_body);
        let small = call(&rg_tool(&rg_small, &small_dir), json!({ "pattern": "hit" })).await;
        assert!(
            !small.content.contains("result is large"),
            "小结果不得带软阈值提示：{}",
            brief(&small.content)
        );

        let _ = std::fs::remove_dir_all(&big_dir);
        let _ = std::fs::remove_dir_all(&small_dir);
    }

    /// **N2b**：命中数 ≤ limit 且总字节远小于预算 ⇒ **无任何规模 notice**。
    ///
    /// 变异：把 `limit_reached` 判成 `total_matches >= limit`（等号）⇒ 本用例变红；
    /// 变异：让 `byte_truncated` 恒真（或误用 `bounded.stdout.len() > 0`）⇒ 本用例变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_no_notice_when_neither_limit_nor_bytes_trigger() {
        let dir = test_dir("grep_no_notice");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs").to_string_lossy().into_owned();
        let body: String = (1..=3)
            .map(|i| {
                format!(
                    "printf '%s\\n' '{}'\n",
                    rg_match_jsonl(&abs, i, &format!("hit{i}"))
                )
            })
            .collect();
        let rg = fake_bin(&dir, "rg", &body);

        let result = call(
            &rg_tool(&rg, &dir),
            json!({ "pattern": "hit", "limit": 100 }),
        )
        .await;
        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("src/a.rs:1: hit1"),
            "正常命中必须渲染；实际：{}",
            brief(&result.content)
        );
        for forbidden in [
            "Use limit=",
            "matches found",
            "Raising `limit`",
            "Truncated",
            "result is large",
        ] {
            assert!(
                !result.content.contains(forbidden),
                "未触发截断时不得出现 `{forbidden}`；实际：{}",
                brief(&result.content)
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **N3**：`context` 超 `MAX_CONTEXT`(5) ⇒ 被钳制 + notice；后置 context 行数 ≤ 5。
    ///
    /// 变异：去掉 `requested_context.min(MAX_CONTEXT)` → 上下文行数变 7，「≤ 5」断言变红。
    /// 变异：去掉 `context_clamped` notice → 「context capped」断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_grep_context_is_capped_with_notice() {
        let dir = test_dir("grep_ctxcap");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("src/a.rs").to_string_lossy().into_owned();
        let mut body = format!("printf '%s\\n' '{}'\n", rg_match_jsonl(&abs, 1, "hit"));
        for i in 2..=8 {
            body.push_str(&format!(
                "printf '%s\\n' '{}'\n",
                rg_event_json("context", &abs, i, "ctx")
            ));
        }
        let rg = fake_bin(&dir, "rg", &body);

        let result = call(
            &rg_tool(&rg, &dir),
            json!({ "pattern": "hit", "context": 999 }),
        )
        .await;
        assert!(!result.is_error, "{}", brief(&result.content));
        assert!(
            result.content.contains("context capped to 5"),
            "应报出 context 被钳制：{}",
            brief(&result.content)
        );
        let ctx_lines = result
            .content
            .lines()
            .filter(|l| l.contains("- ctx"))
            .count();
        assert!(
            ctx_lines <= MAX_CONTEXT,
            "后置 context 行数必须 ≤ {MAX_CONTEXT}，实际 {ctx_lines}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
