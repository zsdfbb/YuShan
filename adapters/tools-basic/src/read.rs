use serde_json::{Value, json};
use std::path::PathBuf;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

pub struct ReadTool {
    workspace: PathBuf,
}

impl ReadTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read",
            "Read file contents. Supports text files.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read" },
                    "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                    "limit": { "type": "number", "description": "Maximum number of lines to read" }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'path'".into()))?;
        let offset = input["offset"].as_u64().map(|v| v as usize);
        let limit = input["limit"].as_u64().map(|v| v as usize);

        let abs_path = self.workspace.join(path);

        let content = tokio::fs::read_to_string(&abs_path)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read {path}: {e}")))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        if total_lines == 0 {
            return Ok(ToolResult {
                content: String::new(),
                is_error: false,
            });
        }

        let start = offset.unwrap_or(1).saturating_sub(1).min(total_lines);
        let end = limit
            .map(|l| (start + l).min(total_lines))
            .unwrap_or(total_lines);

        let selected = &lines[start..end];
        let mut output_lines: Vec<String> = Vec::new();
        let mut total_bytes = 0usize;

        for (i, line) in selected.iter().enumerate() {
            let line_num = start + i + 1;
            let formatted = format!("{line_num:>6}\t{line}");
            let line_bytes = formatted.len();

            if total_bytes + line_bytes > MAX_BYTES && !output_lines.is_empty() {
                break;
            }
            total_bytes += line_bytes;
            output_lines.push(formatted);

            if output_lines.len() >= MAX_LINES {
                break;
            }
        }

        let mut output = output_lines.join("\n");
        let shown_end = start + output_lines.len();

        if shown_end < total_lines {
            let next_offset = shown_end + 1;
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                start + 1,
                shown_end,
                total_lines,
                next_offset
            ));
        }

        Ok(ToolResult {
            content: output,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
    }

    fn test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agent_tools_basic_read_test_{id}_{}_{seq}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn test_read_file() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("test.txt");
        tokio::fs::write(&file, "line1\nline2\nline3\n")
            .await
            .unwrap();

        let tool = ReadTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "test.txt" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line2"));
        assert!(result.content.contains("line3"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_read_with_offset_limit() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("offset.txt");
        tokio::fs::write(&file, "a\nb\nc\nd\ne\n").await.unwrap();

        let tool = ReadTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "offset.txt", "offset": 2, "limit": 2 });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("b"));
        assert!(result.content.contains("c"));
        assert!(!result.content.contains("a"));
        assert!(!result.content.contains("d"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let dir = test_dir();
        let tool = ReadTool::new(dir);
        let ctx = make_ctx();
        let input = json!({ "path": "no_such_file.txt" });
        let result = tool.call(input, ctx).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_truncation_hint() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("many.txt");
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        tokio::fs::write(&file, &content).await.unwrap();

        let tool = ReadTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "many.txt", "limit": 3 });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("offset="));
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line3"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
