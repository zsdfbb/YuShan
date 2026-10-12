use serde_json::{Value, json};
use std::path::PathBuf;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

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
            "Read file contents (text). Use `offset` (1-indexed) and `limit` to read a line \
             range of a long file instead of shelling out to cat/sed.",
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

    /// 只读工具：同一批里可与其它只读工具并发执行（`BasicLoop` Step 9 要求批内**每一个**
    /// 都为 `true` 才走并行 ⇒ 混入 `bash`/`write`/`edit` 的批次自动退回串行）。
    fn is_concurrency_safe(&self) -> bool {
        true
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
        // 先渲染成最终形态（`{行号:>6}\t{内容}`），再交给 `ys_tool::truncate::head_lines` ——
        // 字节预算按**渲染后**的字节数计（不含换行），与原循环逐字节一致。
        //
        // `head_lines` 至多取 `MAX_LINES` 行，故先把待格式化的区间裁到 `MAX_LINES`：
        // 否则无 `limit` 的大文件会先物化全文件行数的 `String`（2M 行 → ~8× 回退）。
        // `selected[..cap]` 保留前缀，`enumerate()` 的下标 `i` 不变，行号一致。
        let cap = selected.len().min(ys_tool::MAX_LINES);
        let formatted: Vec<String> = selected[..cap]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{line}", start + i + 1))
            .collect();

        let truncated = ys_tool::head_lines(&formatted, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);

        // 软阈值（**仅未硬截断时**）：整段结果偏大就提示模型「用 offset/limit 读更窄的区间」。
        // 与下方「Showing lines …」互斥 —— 后者只在被截断时出现，故不会出现两个方括号块。
        let rendered_bytes: usize = formatted.iter().map(|l| l.len()).sum();
        let mut output = truncated.content;
        let shown_end = start + truncated.shown_lines;

        if shown_end < total_lines {
            let next_offset = shown_end + 1;
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                start + 1,
                shown_end,
                total_lines,
                next_offset
            ));
        } else if rendered_bytes >= ys_tool::SOFT_RESULT_BYTES {
            ys_tool::append_notices(&mut output, &[ys_tool::size_notice(rendered_bytes, None)]);
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

    /// **描述即契约**：`read` 支持 `offset` / `limit` 读区间 —— 这句话必须留在模型读得到的
    /// `description` 里。
    ///
    /// 依据（实测）：本机 5 个会话里 `sed -n a,bp file` 出现 13 次，而 `read` 只被调用 2 次；
    /// 模型不是不能用 `read`，是不知道它能读区间（参数 schema 里有，但描述里没提）。
    ///
    /// 变异：把描述改回 `Read file contents. Supports text files.` ⇒ 本用例变红。
    #[test]
    fn spec_description_points_at_offset_and_limit() {
        let tool = ReadTool::new(PathBuf::from("."));
        let d = tool.spec().description;
        assert!(
            d.contains("`offset`") && d.contains("`limit`") && d.contains("line range"),
            "描述须提示用 offset/limit 读区间（替代 sed/cat）；description = {d}"
        );
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

    /// 钉住续读通知的**完整形态与具体数字**（既有 `test_read_truncation_hint` 只
    /// `contains("offset=")`，太弱）：10 行文件取前 3 行 ⇒ `1-3`、总数 `10`、`offset=4`。
    ///
    /// 变异：`next_offset = shown_end + 1` 改成 `shown_end` → `offset=3` → 红。
    /// 变异：`start + 1` 改成 `start` → 起始行变 `0` → 红。
    /// （`shown_end` 漏加 `start` 的变异由下面的子区间用例精确捕获。）
    #[tokio::test]
    async fn test_read_hint_reports_exact_bounds_and_numbers() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("exact.txt");
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        tokio::fs::write(&file, &content).await.unwrap();

        let tool = ReadTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "exact.txt", "limit": 3 });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(
            result
                .content
                .ends_with("\n\n[Showing lines 1-3 of 10. Use offset=4 to continue.]"),
            "通知形态与数字必须逐字精确；实际结尾：{:?}",
            result
                .content
                .chars()
                .rev()
                .take(60)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
        // 渲染形态：行号右对齐 6 宽 + 制表符（内容 3 行 + 空行 + 通知 1 行）。
        assert!(result.content.starts_with("     1\tline1\n"));
        assert!(result.content.contains("\n     3\tline3"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// P1 陷阱回归网：`offset`/`limit` 取**子区间**且**未触顶**时，只要文件还有后续行，
    /// 就**仍须**追加续读通知（判定依据是「文件剩余行」而非「本次选取是否被截断」）。
    ///
    /// 这里 offset=5 / limit=2 ⇒ 选中 `line5..line6`，`head_lines` 未丢弃任何行
    /// （`shown_lines == total_lines`），但文件还剩 `7..10` ⇒ 通知必须是 `5-6 … offset=7`。
    ///
    /// 变异：把守卫 `shown_end < total_lines` 改成 `truncated.shown_lines <
    /// truncated.total_lines`（「本次选取被截断」）→ 此处不再追加通知 → 红。
    /// 变异：`shown_end = start + truncated.shown_lines` 改成 `truncated.shown_lines`
    /// → 通知变成 `1-2 … offset=3` → 红。
    #[tokio::test]
    async fn test_read_subrange_without_truncation_still_hints() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("subrange.txt");
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        tokio::fs::write(&file, &content).await.unwrap();

        let tool = ReadTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({ "path": "subrange.txt", "offset": 5, "limit": 2 });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("line5"));
        assert!(result.content.contains("line6"));
        assert!(!result.content.contains("line7"));
        assert!(
            result
                .content
                .ends_with("\n\n[Showing lines 5-6 of 10. Use offset=7 to continue.]"),
            "子区间未触顶但文件有剩余行时仍须通知；实际结尾：{:?}",
            result
                .content
                .chars()
                .rev()
                .take(60)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// **N4**：整段结果 ≥ 软阈值且**未被硬截断** ⇒ 附一条软阈值提示（`offset`/`limit` 措辞）。
    ///
    /// 变异：去掉 `else if rendered_bytes >= ys_tool::SOFT_RESULT_BYTES` 分支 → 本用例变红。
    /// 变异：把该分支写在 `if shown_end < total_lines` 之前（恒附）→ 「未截断时不得出现续读通知」
    /// 与本用例的 `contains("result is large")` 组合会暴露两个块并存的形态。
    #[tokio::test]
    async fn test_read_large_range_gets_soft_size_notice() {
        let dir = test_dir();
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file = dir.join("big.txt");
        // 40 行 × ~307 B（6 位行号 + Tab + 300 字符）≈ 12 KiB ∈ [8 KiB, 50 KiB)：
        // **未**触发硬截断，应出软提示。
        let content = format!("{}\n", "x".repeat(300)).repeat(40);
        tokio::fs::write(&file, &content).await.unwrap();

        let tool = ReadTool::new(dir.clone());
        let result = tool
            .call(json!({ "path": "big.txt" }), make_ctx())
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(
            result.content.contains("result is large"),
            "大结果应带软阈值提示：{}",
            result.content.chars().take(120).collect::<String>()
        );
        assert!(
            !result.content.contains("Showing lines"),
            "未截断时不应出现续读通知：{}",
            result.content.chars().rev().take(120).collect::<String>()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
