use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

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

/// 尾截断输出：保留末尾的 MAX_LINES / MAX_BYTES
fn tail_truncate(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();

    if total_lines <= MAX_LINES {
        // 检查字节上限
        if output.len() <= MAX_BYTES {
            return output.to_string();
        }
    }

    // 找出多少尾行能放入 MAX_BYTES
    let mut byte_count = 0usize;
    let mut start = total_lines;

    for i in (0..total_lines).rev() {
        let line_len = lines[i].len() + 1; // +1 计入换行符
        if byte_count + line_len > MAX_BYTES {
            break;
        }
        byte_count += line_len;
        start = i;
    }

    // 同时遵守 MAX_LINES 上限
    let min_start = total_lines.saturating_sub(MAX_LINES);
    start = start.max(min_start);

    let shown = &lines[start..];
    let mut result = shown.join("\n");
    let skipped = start;
    if skipped > 0 {
        result.insert_str(0, &format!("[Truncated {skipped} lines.]\n\n"));
    }
    result
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

    // 检查退出码（被信号杀死时 code 为 None，不视作「非零退码」）
    if let Some(code) = exit_code {
        if code != 0 {
            let truncated = tail_truncate(&combined);
            return ToolResult {
                content: format!("{truncated}\n\nCommand exited with code {code}"),
                is_error: true,
            };
        }
    }

    ToolResult {
        content: tail_truncate(&combined),
        is_error: false,
    }
}

/// 尽力终止整个进程组（spawn 时已 `process_group(0)`，故进程组 id == 子进程 pid）。
///
/// 不引入 `libc` 依赖：借助本工具已依赖的 `sh` 内建 `kill`，向负 pid（= 进程组）
/// 发 `SIGKILL`。仅覆盖 `sh` fork 出的孙进程（如 `sh -c "npm test"` 的 node 子进程）；
/// 失败时静默 —— 调用方仍有 `Child::start_kill()` 兜底。
#[cfg(unix)]
async fn kill_process_group(pid: u32) {
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
            .stderr(std::process::Stdio::piped());

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

    #[test]
    fn test_tail_truncate_short() {
        let input = "line1\nline2\nline3\n";
        let result = tail_truncate(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_tail_truncate_long() {
        let lines: Vec<String> = (1..=3000).map(|i| format!("line{i}")).collect();
        let input = lines.join("\n");
        let result = tail_truncate(&input);
        assert!(result.contains("Truncated"));
        assert!(result.contains("line3000"));
    }
}
