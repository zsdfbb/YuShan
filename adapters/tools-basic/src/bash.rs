use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

/// 挂载了 `boundary` 时的中止轮询间隔 —— 命令中止的感知延迟上界。
const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct BashTool {
    workspace: PathBuf,
}

impl BashTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

/// 把子进程输出组装成工具结果：stdout + stderr 合并、超限尾截断、非零退码标错。
///
/// `note` 是这次调用的一条**事实**（当前唯一来源：[`redundant_root_cd`]），追加在
/// 正文之后、退码行之前。它**不影响** `is_error` —— 命令本身失败时，注记与
/// `Command exited with code …` 并存，且退码行仍是最后一行。
fn render_result(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
    note: Option<&str>,
) -> ToolResult {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);

    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    // 尾截断由 `ys_tool::truncate` 统一实现；「丢弃了多少行」的通知由本工具前插。
    // `total_lines - shown_lines` 即被丢弃的行数（= 原实现里的 `skipped`）。
    let truncated = ys_tool::tail(&combined, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
    let mut content = truncated.content;
    if truncated.shown_lines < truncated.total_lines {
        content.insert_str(
            0,
            &format!(
                "[Truncated {} lines.]\n\n",
                truncated.total_lines - truncated.shown_lines
            ),
        );
    }

    // 注记追加在**正文之后、退码行之前**（两条「关于本次执行」的元信息相邻）：
    // 截断通知紧贴它要说明的正文、不被隔开，且既有不变量「失败结果的最后一行是
    // 退码行」不发生位移。空输出（`cd <目录> && true`）时不前插空行。
    if let Some(note) = note {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(note);
    }

    // 检查退出码（被信号杀死时 code 为 None，不视作「非零退码」）
    if let Some(code) = exit_code {
        if code != 0 {
            return ToolResult {
                content: format!("{content}\n\nCommand exited with code {code}"),
                is_error: true,
            };
        }
    }

    ToolResult {
        content,
        is_error: false,
    }
}

/// 命令是否以「`cd` 到本工具 workspace 同一个目录」开头 —— 是即判为冗余。
///
/// 本工具每次调用都在 `self.workspace` 起 shell（见 `call` 里的 `current_dir`），
/// 故这条 `cd` 不产生任何位移，判定的唯一用途是**在结果里回一行事实**（模型无从
/// 核对「我在哪」，只能自己每轮钉一次位置）。
///
/// 只认**第一段**的 `cd <path> && 余下`：`<path>` 允许裸串 / 单引号 / 双引号；
/// `余下` 必须非空（`cd <目录> &&` 是语法错误，不是零位移）。两侧 `canonicalize`
/// 后比较，任一侧失败则退化为「相对路径按 `workspace` 拼接后字面比较」（此时
/// `cd .` 一类判不出来，只是少一行注记，不影响正确性）。
///
/// **不认**：`cd ~/…`、`cd $VAR`、`cd -`、`;` 分隔，以及 `cd <目录>` 之后不带命令的
/// 裸 `cd` —— 前几者要展开才知道指向何处，后者没有「下文命令」可省，报了没意义。
/// 指向 workspace **子目录**的 `cd` 是有意义的位移，一律不认。
fn redundant_root_cd(command: &str, workspace: &Path) -> bool {
    let rest = command.trim_start();
    let Some(after) = rest.strip_prefix("cd") else {
        return false;
    };
    // `cd` 之后必须是空白，否则是 `cdrom` / `cdparanoia` 这类命令词的开头
    if !after.starts_with(char::is_whitespace) {
        return false;
    }
    let Some((token, tail)) = take_shell_token(after.trim_start()) else {
        return false;
    };
    // 只认 `&&` 分隔：`;` 后的命令与这条 `cd` 不在同一「若非成功则不执行」链上。
    // 其后必须**真的有命令** —— `cd <目录> &&` 会被 `sh` 判为语法错误，不是零位移的 no-op。
    let Some(rest) = tail.trim_start().strip_prefix("&&") else {
        return false;
    };
    if rest.trim().is_empty() {
        return false;
    }

    let target = Path::new(token);
    // 空串（`cd "" && …`）在 `sh` 里是**失败**的切目录，不是 no-op；且
    // `workspace.join("")` 会退化成 `workspace/`，若不加这道守卫会被误判成冗余。
    if target.as_os_str().is_empty() {
        return false;
    }
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        workspace.join(target)
    };
    match (resolved.canonicalize(), workspace.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // 任一侧解析不到（目录不存在等）⇒ 退化为字面比较：宁可少说一行，不说错一句
        _ => resolved == workspace,
    }
}

/// 从头切出一个 shell 词：裸串读到空白或 shell 元字符（`&;|<>()`），
/// 引号串读到配对的同类引号。返回 `(词, 余下)`；引号未闭合时返回 `None`
/// （判不出就不判）。元字符也要断词，否则 `cd /a&&ls` 会把 `&&ls` 吃进路径。
fn take_shell_token(s: &str) -> Option<(&str, &str)> {
    let first = s.chars().next()?;
    if first == '\'' || first == '"' {
        let body = &s[first.len_utf8()..];
        let end = body.find(first)?;
        Some((&body[..end], &body[end + first.len_utf8()..]))
    } else {
        let end = s
            .find(|c: char| c.is_whitespace() || "&;|<>()".contains(c))
            .unwrap_or(s.len());
        (end > 0).then(|| (&s[..end], &s[end..]))
    }
}

/// 尽力终止整个进程组（spawn 时已 `process_group(0)`，故进程组 id == 子进程 pid）。
///
/// **`exec::run_bounded` 复用同一份** —— 两处的前置条件相同（都 `process_group(0)`），
/// 各写一份会漂移。调用方**必须**在 `wait()` 收尸**之前**调用：leader 一旦被收尸，
/// 它的 pid 就可能被复用，`kill -9 -<pid>` 便可能打到无关的进程组。
///
/// 不引入 `libc` 依赖：借助本工具已依赖的 `sh` 内建 `kill`，向负 pid（= 进程组）
/// 发 `SIGKILL`。仅覆盖 `sh` fork 出的孙进程（如 `sh -c "npm test"` 的 node 子进程）；
/// 失败时静默 —— 调用方仍有 `Child::start_kill()` 兜底。
#[cfg(unix)]
pub(crate) async fn kill_process_group(pid: u32) {
    let _ = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -9 -{pid}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash",
            "Execute shell commands via `sh -c` (POSIX sh, not the user's interactive shell). \
             Supports timeout. Every call starts a fresh shell in the workspace root; relative \
             paths resolve against it, and `cd` does NOT carry over between calls.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Timeout in seconds" }
                },
                "required": ["command"]
            }),
        )
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'command'".into()))?;
        let timeout_secs = input["timeout"].as_u64();

        // 工作目录不成立 ⇒ 硬报错。放行到 `spawn` 只会得到一句
        // `Failed to spawn command`，与「命令本身有问题」无法区分。
        match tokio::fs::metadata(&self.workspace).await {
            Ok(meta) if !meta.is_dir() => {
                return Err(ToolError::Execution(format!(
                    "Working directory is not a directory: {}",
                    self.workspace.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Execution(format!(
                    "Working directory does not exist: {}",
                    self.workspace.display()
                )));
            }
            // 存在且是目录，或「读不到但不是 NotFound」⇒ 交给原有 spawn 路径报
            _ => {}
        }

        // 命令以 `cd <本目录> && …` 开头 ⇒ 结果里回一行事实（判定规则见
        // [`redundant_root_cd`]）。命令**照原样执行**，本变量只影响结果文案。
        // `[bash: …]` 前缀标明这句是**工具说的**，不是命令输出（命令可以打印任意文本）。
        let note = redundant_root_cd(command, &self.workspace).then(|| {
            format!(
                "[bash: this shell already started in {}; the leading cd changed nothing]",
                self.workspace.display()
            )
        });

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // 超时/中止后 `Child` 在 `wait_with_output` 的 future 被 drop 时随之 drop，
            // 若无此设置子进程会继续跑到自然结束。
            .kill_on_drop(true);

        // Unix 上使用进程组
        #[cfg(unix)]
        {
            #[allow(unused_imports)]
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("Failed to spawn command: {e}")))?;

        // ── 无边界控制：保持既有整体等待路径（行为不变） ──
        let Some(boundary) = ctx.boundary else {
            let output = if let Some(secs) = timeout_secs {
                let timeout = Duration::from_secs(secs);
                match tokio::time::timeout(timeout, child.wait_with_output()).await {
                    Ok(result) => result.map_err(|e| {
                        ToolError::Execution(format!("Failed to wait for command: {e}"))
                    })?,
                    Err(_timeout) => {
                        // 超时时杀死进程树
                        #[cfg(unix)]
                        {
                            // child 在该点已被 drop，但仍可尝试向进程组发送信号来终止
                        }
                        return Err(ToolError::Timeout(format!(
                            "Command timed out after {secs} seconds"
                        )));
                    }
                }
            } else {
                child
                    .wait_with_output()
                    .await
                    .map_err(|e| ToolError::Execution(format!("Failed to wait for command: {e}")))?
            };

            return Ok(render_result(
                &output.stdout,
                &output.stderr,
                output.status.code(),
                note.as_deref(),
            ));
        };

        // ── 有边界控制：接管管道 + 轮询 `try_wait`/`is_aborted`，命令可被中途中止 ──
        let pid = child.id();
        let mut stdout = child.stdout.take().expect("stdout 已 piped");
        let mut stderr = child.stderr.take().expect("stderr 已 piped");

        // 读取与等待必须并发：管道写满会阻塞子进程，若不持续排空则死锁。
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let deadline = timeout_secs.map(|s| (Instant::now() + Duration::from_secs(s), s));

        let outcome = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => {
                    break Err(ToolError::Execution(format!(
                        "Failed to wait for command: {e}"
                    )));
                }
            }

            if boundary.is_aborted() {
                break Err(ToolError::Execution("aborted by user".into()));
            }

            if let Some((dl, secs)) = deadline {
                if Instant::now() >= dl {
                    break Err(ToolError::Timeout(format!(
                        "Command timed out after {secs} seconds"
                    )));
                }
            }

            // 有 deadline 时按剩余时间收敛，避免超时被轮询间隔拉长
            let mut nap = ABORT_POLL_INTERVAL;
            if let Some((dl, _)) = deadline {
                nap = nap.min(dl.saturating_duration_since(Instant::now()));
            }
            tokio::time::sleep(nap).await;
        };

        match outcome {
            Ok(status) => {
                let stdout = out_task.await.unwrap_or_default();
                let stderr = err_task.await.unwrap_or_default();
                Ok(render_result(
                    &stdout,
                    &stderr,
                    status.code(),
                    note.as_deref(),
                ))
            }
            Err(err) => {
                // 中止 / 超时 / 等待失败：先杀进程组（含孙进程），再杀子进程兜底
                if let Some(pid) = pid {
                    #[cfg(unix)]
                    kill_process_group(pid).await;
                }
                let _ = child.start_kill();
                let _ = child.wait().await;
                // 已丢弃输出，读取任务不必再等（孙进程可能仍持写端）
                out_task.abort();
                err_task.abort();
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
    }

    /// **描述即契约，但只陈述事实**：`bash` 每次调用都在 `workspace` 起新 shell、
    /// 相对路径按该目录解析、`cd` 不跨调用 —— 这三条事实必须留在模型读得到的
    /// `description` 里（`ToolSpec` 随每轮请求发给模型，命中前缀缓存，零边际成本）。
    ///
    /// **刻意不写禁令**（不写「不要 cd」），也不给 `cd dir && cmd` 这类许可 ——
    /// 禁令里夹一个许可会让模型把「这一条也需要」判给自己。实测依据见
    /// `docs/arch/bash-cwd-contract/context.md`。
    ///
    /// 变异：① 描述改回一句 `Execute shell commands. Supports timeout.` ⇒ 前两条断言变红；
    /// ② 把旧文本的 `do not prefix … or chain \`cd dir && cmd\`` 加回来 ⇒ 第三条变红。
    #[test]
    fn spec_description_states_the_cwd_contract() {
        let tool = BashTool::new(PathBuf::from("."));
        let d = tool.spec().description;
        assert!(
            d.contains("workspace root"),
            "描述须说明每次调用的工作目录；description = {d}"
        );
        assert!(
            d.contains("`cd` does NOT") && d.contains("carry over"),
            "描述须说明 `cd` 不跨调用保留（否则模型仍会防御性加前缀）；description = {d}"
        );
        assert!(
            !d.contains("system prompt"),
            "描述不得依赖可能被 SYSTEM.md 短路掉的系统提示文本；description = {d}"
        );
        assert!(
            !d.contains("do not prefix") && !d.contains("or chain"),
            "描述只陈述事实，不写禁令与逃生口；description = {d}"
        );
    }

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全的既有样板，见 `read.rs`）。
    fn test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agent_tools_basic_bash_test_{id}_{}_{seq}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn test_bash_echo() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "echo hello" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn test_bash_nonzero_exit() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "exit 42" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("exited with code 42"));
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "sleep 30", "timeout": 1 });
        let result = tool.call(input, ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    /// N3（不泄漏）：超时后**直接子进程必须已被杀死**，不得继续执行后续命令。
    ///
    /// 原理：`cmd.kill_on_drop(true)` 在 `wait_with_output()` 的 future 被 timeout drop 时
    /// 连带 drop `Child`，从而 SIGKILL 直接子进程 `sh`；`sh` 死于 1s，来不及在 2s 执行
    /// `touch`。故 3s 后 sentinel 必须不存在。
    ///
    /// ⚠️ 本用例只用「`sh` 亲自执行 `touch`」的形式 —— `kill_on_drop` 只杀直接子进程，
    /// `sh -c '(… ) & wait'` 之类**孙进程**仍会存活（已登记的后续项 P2-2，不在此编码）。
    ///
    /// 变异：删掉 `.kill_on_drop(true)` → `sh` 活过 2s 并 `touch` → sentinel 存在 → 红。
    #[tokio::test]
    async fn test_bash_timeout_does_not_leak_child_process() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let sentinel = dir.join("leaked.txt");

        let tool = BashTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({
            "command": format!("sleep 2; touch {}", sentinel.display()),
            "timeout": 1
        });

        let started = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(10), tool.call(input, ctx))
            .await
            .expect("call 应在 ~1s 内返回；超过 10s 视为挂起（死锁必须表现为失败）");
        let elapsed = started.elapsed();

        let err = result.expect_err("超时必须返回 Err");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按 timeout 收敛（1s），实际耗时 {elapsed:?}"
        );

        // 等过命令里 `sleep 2` 的时点：若子进程没被杀死，此刻 sentinel 已被 touch 出来。
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !sentinel.exists(),
            "超时后子进程仍存活并执行了 touch（kill_on_drop 未生效）：{}",
            sentinel.display()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_bash_stderr() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "echo err >&2" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("err"));
    }

    // ── 边界控制：`ToolContext.boundary` 报 Abort 时中止命令 ──────────────

    /// 挂载了 `QueueBoundarySource` 的 ctx 构造辅助。
    fn ctx_with(boundary: &ys_protocol::QueueBoundarySource) -> ToolContext<'_> {
        ToolContext::new(Some(boundary), PathBuf::from("."), PathBuf::from("."))
    }

    /// 没有任何边界信号时，挂载 `boundary` 不得改变既有行为。
    #[tokio::test]
    async fn test_bash_boundary_present_without_abort_runs_normally() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "echo ok" });

        let result = tool.call(input, ctx_with(&boundary)).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("ok"));
    }

    /// 命令启动前已置 Abort → 立刻收场，不跑满命令。
    #[tokio::test]
    async fn test_bash_aborted_before_start_returns_immediately() {
        use ys_protocol::Boundary;

        let boundary = ys_protocol::QueueBoundarySource::new();
        boundary.push(Boundary::Abort);
        assert!(boundary.is_aborted());

        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "sleep 10" });

        let started = Instant::now();
        let result = tool.call(input, ctx_with(&boundary)).await;
        let elapsed = started.elapsed();

        let err = result.expect_err("Abort 后不应成功返回");
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("abort")),
            "expected abort Execution error, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "Abort 应立即中止，实际耗时 {elapsed:?}"
        );
    }

    /// 命令运行**期间**收到 Abort → 进程被终止，`call` 很快返回（不等 10 秒）。
    #[tokio::test]
    async fn test_bash_abort_mid_command_kills_it() {
        use std::sync::Arc;
        use ys_protocol::{Boundary, QueueBoundarySource};

        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(Boundary::Abort);
        });

        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "sleep 10" });

        let started = Instant::now();
        let result = tool.call(input, ctx_with(&boundary)).await;
        let elapsed = started.elapsed();

        let err = result.expect_err("运行中 Abort 应终止命令");
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("abort")),
            "expected abort Execution error, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "不应等到命令自然结束（10s），实际耗时 {elapsed:?}"
        );
    }

    /// 挂载 `boundary` 时，既有的 `timeout` 语义仍生效（未投 Abort 也必须在到期时中止）。
    #[tokio::test]
    async fn test_bash_timeout_still_applies_with_boundary() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "sleep 10", "timeout": 1 });

        let started = Instant::now();
        let result = tool.call(input, ctx_with(&boundary)).await;
        let elapsed = started.elapsed();

        let err = result.expect_err("超时应中止");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按 timeout 收敛（1s），实际耗时 {elapsed:?}"
        );
    }

    /// 未超限：`truncate::tail` 原样返回（含尾随换行），无需通知。
    #[test]
    fn test_tail_truncate_short() {
        let input = "line1\nline2\nline3\n";
        let result = ys_tool::tail(input, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
        assert_eq!(result.content, input);
        assert_eq!(result.shown_lines, result.total_lines);
    }

    /// 超限：保留末尾（含最后一行），且确有行被丢弃 —— 后者等价于「调用方会拼出
    /// `[Truncated N lines.]` 通知」（文案由调用方拼，不在本模块）。
    #[test]
    fn test_tail_truncate_long() {
        let lines: Vec<String> = (1..=3000).map(|i| format!("line{i}")).collect();
        let input = lines.join("\n");
        let result = ys_tool::tail(&input, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
        assert!(result.shown_lines < result.total_lines);
        assert!(result.content.contains("line3000"));
    }

    /// 走**完整 `call` 路径**，钉住 `[Truncated N lines.]` 的字面文案与**具体 N**。
    ///
    /// 关键构造：输入必须让**字节闸**（51200）而非行数闸（2000）成为约束 ——
    /// 3000 行 × 40 字节 = 123000 字节，行数（3000）并未超过行数上限（2000），
    /// 故 `shown_lines` 完全由 `tail` 的字节规则（单行成本 `len() + 1`，含换行）决定：
    /// `51200 / 41 = 1248` 行可留 ⇒ 丢弃 `3000 - 1248 = 1752` 行。
    ///
    /// 变异：`crates/ys-tool/src/truncate.rs::tail` 里 `lines[i].len() + 1` 改成 `len()`
    /// → `51200 / 40 = 1280` 行可留 ⇒ N 变 1720 → 本测试的 `1752` 断言变红。
    #[tokio::test]
    async fn test_bash_truncation_notice_reports_exact_line_count() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        // 输出 3000 行、每行恰好 40 个 'x'（纯 ASCII，字节数 = 字符数）。
        let input = json!({
            "command":
                "awk 'BEGIN { s=\"\"; for (i=0;i<40;i++) s=s \"x\"; for (i=0;i<3000;i++) print s }'"
        });

        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(
            result.content.starts_with("[Truncated 1752 lines.]\n\n"),
            "通知文案（含丢弃行数）必须精确；实际开头：{:?}",
            result.content.chars().take(80).collect::<String>()
        );
        // 通知 2 行（正文 + 空行）+ 保留的 1248 行。
        assert_eq!(
            result.content.lines().count(),
            2 + 1248,
            "通知后应恰好保留 1248 行正文"
        );
        // 尾截断的语义：保留的是**末尾**。
        assert!(
            result.content.lines().skip(2).all(|l| l == "x".repeat(40)),
            "保留正文应全为 40 字节行"
        );
    }

    // ==================== 冗余 `cd` 的事实回显（见 `redundant_root_cd`） ====================

    /// 判定只认「命令开头的 `cd` 落回 workspace 本身」= 零位移。纯函数用例，不起进程。
    ///
    /// 变异：① `(Ok(a), Ok(b)) => a == b` 改成恒真 ⇒ 下面「子目录不算冗余」那条变红；
    /// ② 丢掉 canonicalize、直接比 `resolved == workspace` ⇒ `cd <ws>/.` 那条变红。
    #[test]
    fn test_redundant_root_cd_accepts_only_zero_displacement_forms() {
        let dir = test_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();
        let name = ws.file_name().unwrap().to_string_lossy().to_string();

        for cmd in [
            format!("cd {abs} && ls"),
            format!("cd {abs}&&ls"),
            format!("cd '{abs}' && ls"),
            format!("cd \"{abs}\" && ls"),
            format!("cd {abs}/. && ls"),
            format!("cd {abs}/../{name} && ls"),
            format!("  cd {abs} && ls"),
            format!("\ncd {abs} && ls"),
            "cd . && ls".to_string(),
            format!("cd {abs} && cd sub && ls"),
        ] {
            assert!(redundant_root_cd(&cmd, &ws), "应判为冗余：{cmd}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 判定的**反例**：有真实位移的、不在开头的、静态判不出指向的，一律不判冗余。
    ///
    /// 变异：去掉 `after.starts_with(char::is_whitespace)` 词边界 ⇒ `cd<ws> …` 变红；
    /// 去掉 `&&` 之后的非空检查 ⇒ `cd <ws> &&` 变红。
    ///
    /// `ls && cd <ws> && make` 那条**不具变异敏感性**：实现是 `strip_prefix` 在
    /// `trim_start` 之后直接取首段，结构上就不可能匹配中段 —— 该用例只作意图记录。
    #[test]
    fn test_redundant_root_cd_rejects_everything_else() {
        let dir = test_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();

        for cmd in [
            // 真实位移
            format!("cd {abs}/sub && ls"),
            "cd .. && ls".to_string(),
            "cd /tmp && ls".to_string(),
            // 不在开头：本模块只判开头的「开局钉位置」
            format!("ls && cd {abs} && make"),
            // 静态判不出指向何处，或语义不是路径
            "cd ~/x && ls".to_string(),
            "cd $ROOT && ls".to_string(),
            "cd - && ls".to_string(),
            "cd && ls".to_string(),
            // 词边界与空输入
            "cdx && ls".to_string(),
            // ↑ 这条其实是被「路径不等于 ws」挡下的；**下面这条**才是词边界守卫
            //   唯一的可观测点：没有守卫时 `cd<abs>` 会被切成 `cd` + `<abs>` ⇒ 误报。
            format!("cd{abs} && ls"),
            "cd".to_string(),
            "echo hi".to_string(),
            // 空串：`sh` 里 `cd ""` 是**失败**的切目录，不是 no-op
            "cd \"\" && ls".to_string(),
            // 没有下文命令可省 / 只认 `&&` 分隔
            format!("cd {abs}"),
            format!("cd {abs} ; ls"),
            format!("cd {abs} &&"),
        ] {
            assert!(!redundant_root_cd(&cmd, &ws), "不应判为冗余：{cmd}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 走完整 `call` 路径：冗余 `cd` 的那一次在结果里回一行**工具自撰的事实**，
    /// 其余调用一概没有；且命令自身的输出**逐字节不变**，`is_error` 语义不变。
    ///
    /// 变异：① 删掉 `render_result` 里的注记追加 ⇒ 第 ②③④ 段全红；
    /// ② 把追加改成 `insert_str(0, …)` ⇒ 「退码行仍在最后一行」变红。
    #[tokio::test]
    async fn test_redundant_cd_note_is_added_only_when_the_cd_has_no_displacement() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();
        let note =
            format!("[bash: this shell already started in {abs}; the leading cd changed nothing]");
        let tool = BashTool::new(ws.clone());

        // ① 无前缀 ⇒ 无注记，输出就是命令输出
        let plain = tool
            .call(json!({ "command": "echo hi" }), make_ctx())
            .await
            .unwrap();
        assert_eq!(plain.content, "hi\n");
        assert!(!plain.content.contains("[bash:"));

        // ② 有前缀 ⇒ 正文逐字节不变，注记追加其后
        let prefixed = tool
            .call(
                json!({ "command": format!("cd {abs} && echo hi") }),
                make_ctx(),
            )
            .await
            .unwrap();
        assert!(!prefixed.is_error);
        assert_eq!(prefixed.content, format!("hi\n\n\n{note}"));

        // ③ 命令失败：注记在，退码行仍是**最后一行**（既有不变量）
        let failed = tool
            .call(
                json!({ "command": format!("cd {abs} && exit 7") }),
                make_ctx(),
            )
            .await
            .unwrap();
        assert!(failed.is_error);
        assert!(failed.content.contains(&note));
        assert_eq!(
            failed.content.lines().last().unwrap(),
            "Command exited with code 7"
        );

        // ④ 空输出：注记独占一行，前面不空行
        let silent = tool
            .call(
                json!({ "command": format!("cd {abs} && true") }),
                make_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(silent.content, note);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 注记与既有 `[Truncated N lines.]` 通知同时出现时的**相对顺序**：
    /// 截断通知在最前（必须紧贴它说明的正文），注记在最后。
    ///
    /// 变异：把注记改成前插 ⇒ 本用例的前两条断言变红。
    #[tokio::test]
    async fn test_redundant_cd_note_does_not_displace_the_truncation_notice() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();
        let tool = BashTool::new(ws.clone());

        let input = json!({
            "command": format!(
                "cd {abs} && awk 'BEGIN {{ s=\"\"; for (i=0;i<40;i++) s=s \"x\"; \
                 for (i=0;i<3000;i++) print s }}'"
            )
        });
        let result = tool.call(input, make_ctx()).await.unwrap();

        assert!(
            result.content.starts_with("[Truncated 1752 lines.]\n\n"),
            "截断通知仍须在最前；实际开头：{:?}",
            result.content.chars().take(60).collect::<String>()
        );
        assert!(
            result.content.ends_with(&format!(
                "[bash: this shell already started in {abs}; the leading cd changed nothing]"
            )),
            "注记应在最末"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 工作目录不成立 ⇒ **硬报错**，而不是让 `spawn` 报一句与「命令本身有问题」
    /// 无法区分的 `Failed to spawn command`（对齐 pi 的 `Working directory does not exist`）。
    ///
    /// 变异：删掉 `call` 里的前置校验 ⇒ 退回 spawn 失败路径 ⇒ 两条断言都变红。
    #[tokio::test]
    async fn test_missing_workspace_is_an_explicit_error() {
        let dir = test_dir();
        assert!(!dir.exists(), "本用例要求目录不存在");
        let tool = BashTool::new(dir.clone());

        let err = tool
            .call(json!({ "command": "echo hi" }), make_ctx())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Working directory does not exist"), "{msg}");
        assert!(msg.contains(&dir.display().to_string()), "{msg}");
    }

    /// 工作目录**是文件**（不是目录）⇒ 文案必须与「不存在」区分开，否则用户查错方向。
    #[tokio::test]
    async fn test_workspace_that_is_not_a_directory_is_an_explicit_error() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join("not-a-dir");
        tokio::fs::write(&file, "x").await.unwrap();
        let tool = BashTool::new(file.clone());

        let err = tool
            .call(json!({ "command": "echo hi" }), make_ctx())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("is not a directory"), "{msg}");
        assert!(msg.contains(&file.display().to_string()), "{msg}");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
