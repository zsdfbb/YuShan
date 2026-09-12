use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

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

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
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

        let child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("Failed to spawn command: {e}")))?;

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

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

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

        // 检查退出码
        if let Some(code) = output.status.code() {
            if code != 0 {
                let truncated = tail_truncate(&combined);
                return Ok(ToolResult {
                    content: format!("{truncated}\n\nCommand exited with code {code}"),
                    is_error: true,
                });
            }
        }

        let truncated = tail_truncate(&combined);
        Ok(ToolResult {
            content: truncated,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::CancelToken;

    fn make_ctx() -> (CancelToken, ToolContext<'static>) {
        let token = Box::leak(Box::new(CancelToken::new()));
        let ctx = ToolContext::new(token, PathBuf::from("."), PathBuf::from("."));
        let ctx: ToolContext<'static> = unsafe { std::mem::transmute(ctx) };
        (CancelToken::new(), ctx)
    }

    #[tokio::test]
    async fn test_bash_echo() {
        let tool = BashTool::new(PathBuf::from("."));
        let (_cancel, ctx) = make_ctx();
        let input = json!({ "command": "echo hello" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn test_bash_nonzero_exit() {
        let tool = BashTool::new(PathBuf::from("."));
        let (_cancel, ctx) = make_ctx();
        let input = json!({ "command": "exit 42" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("exited with code 42"));
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let tool = BashTool::new(PathBuf::from("."));
        let (_cancel, ctx) = make_ctx();
        let input = json!({ "command": "sleep 30", "timeout": 1 });
        let result = tool.call(input, ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn test_bash_stderr() {
        let tool = BashTool::new(PathBuf::from("."));
        let (_cancel, ctx) = make_ctx();
        let input = json!({ "command": "echo err >&2" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("err"));
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
