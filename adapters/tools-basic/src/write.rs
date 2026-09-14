use serde_json::{Value, json};
use std::path::PathBuf;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

pub struct WriteTool {
    workspace: PathBuf,
}

impl WriteTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write",
            "Create or overwrite files. Automatically creates parent directories.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"]
            }),
        )
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'path'".into()))?;
        let content = input["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'content'".into()))?;

        let abs_path = self.workspace.join(path);

        // 创建父目录
        if let Some(parent) = abs_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Execution(format!("Failed to create directories: {e}")))?;
        }

        tokio::fs::write(&abs_path, content)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write {path}: {e}")))?;

        Ok(ToolResult {
            content: format!("Successfully wrote to {path}"),
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
            "agent_tools_basic_write_test_{id}_{}_{seq}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn test_write_new_file() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("new.txt");

        let tool = WriteTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "new.txt", "content": "hello world" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Successfully wrote to new.txt"));

        let written = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(written, "hello world");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_write_overwrite() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("overwrite.txt");
        tokio::fs::write(&file, "old content").await.unwrap();

        let tool = WriteTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "overwrite.txt", "content": "new content" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        let written = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(written, "new content");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_write_creates_parent_dirs() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("sub/nested/file.txt");

        let tool = WriteTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "sub/nested/file.txt", "content": "nested" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        let written = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(written, "nested");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
