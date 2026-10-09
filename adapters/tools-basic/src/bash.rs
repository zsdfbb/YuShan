use serde_json::{Value, json};
use std::path::PathBuf;
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
fn render_result(stdout: &[u8], stderr: &[u8], exit_code: Option<i32>) -> ToolResult {
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
            "Execute shell commands. Supports timeout.",
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
                Ok(render_result(&stdout, &stderr, status.code()))
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
}
