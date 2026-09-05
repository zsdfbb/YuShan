use agent_core::ToolResult;
use agent_tool::{Tool, ToolContext, ToolError, ToolSpec};
use serde_json::{Value, json};
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;

pub struct EditTool {
    workspace: PathBuf,
}

impl EditTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

/// Normalize text for fuzzy matching: NFKC + smart quotes + dash + trim
fn fuzzy_normalize(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.nfkc() {
        match ch {
            // Smart quotes -> ASCII
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2039}' | '\u{203A}'
            | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => result.push('"'),
            // En/em dash -> ASCII dash
            '\u{2013}' | '\u{2014}' | '\u{2015}' => result.push('-'),
            // Non-breaking space -> ASCII space
            '\u{00A0}' => result.push(' '),
            // Zero-width spaces and joiners -> remove
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => {}
            _ => result.push(ch),
        }
    }
    result.trim().to_string()
}

/// Detect line ending style: returns true if CRLF is dominant
fn detect_crlf(content: &str) -> bool {
    let crlf_count = content.matches("\r\n").count();
    let lf_count = content.matches('\n').count() - crlf_count;
    crlf_count > lf_count
}

/// Strip BOM from the beginning of content
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}

fn resolve_path(input: &Value, path_field: &str) -> Result<String, ToolError> {
    input[path_field]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{path_field}'")))
        .map(String::from)
}

fn parse_edits(input: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let mut edits: Vec<(String, String)> = Vec::new();

    // Try legacy top-level oldText/newText
    if let Some(old) = input.get("oldText").and_then(|v| v.as_str()) {
        let new = input.get("newText").and_then(|v| v.as_str()).unwrap_or("");
        edits.push((old.to_string(), new.to_string()));
        return Ok(edits);
    }

    // Try edits field
    match input.get("edits") {
        Some(Value::Array(arr)) => {
            for (i, item) in arr.iter().enumerate() {
                let old = item
                    .get("oldText")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput(format!("edits[{i}]: missing 'oldText'"))
                    })?;
                let new = item.get("newText").and_then(|v| v.as_str()).unwrap_or("");
                edits.push((old.to_string(), new.to_string()));
            }
        }
        Some(Value::String(s)) => {
            // edits is a string — treat as single oldText
            edits.push((s.clone(), String::new()));
        }
        Some(Value::Object(_)) => {
            // Single object treated as array
            let old = input["edits"]["oldText"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidInput("edits: missing 'oldText'".into()))?;
            let new = input["edits"]["newText"].as_str().unwrap_or("");
            edits.push((old.to_string(), new.to_string()));
        }
        _ => {
            return Err(ToolError::InvalidInput(
                "missing 'edits' or 'oldText'".into(),
            ));
        }
    }

    Ok(edits)
}

/// Try to find old_text in content. First exact, then fuzzy.
/// Returns (byte_offset, was_fuzzy) or None.
fn find_match(content: &str, old_text: &str) -> Option<(usize, bool)> {
    // Exact match first
    if let Some(idx) = content.find(old_text) {
        return Some((idx, false));
    }

    // Fuzzy match — search character by character
    if let Some(idx) = find_match_fuzzy(content, old_text) {
        return Some((idx, true));
    }

    None
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit",
            "Edit files with precise or fuzzy text matching. Supports batch edits.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit" },
                    "edits": {
                        "description": "Array of edit operations, each with oldText and newText",
                        "oneOf": [
                            {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "oldText": { "type": "string" },
                                        "newText": { "type": "string" }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "oldText": { "type": "string" },
                                    "newText": { "type": "string" }
                                }
                            }
                        ]
                    },
                    "oldText": { "type": "string", "description": "Legacy: text to find" },
                    "newText": { "type": "string", "description": "Legacy: replacement text" }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let path = resolve_path(&input, "path")?;
        let edits = parse_edits(&input)?;

        if edits.is_empty() {
            return Err(ToolError::InvalidInput("no edits provided".into()));
        }

        let abs_path = self.workspace.join(&path);

        let raw_content = tokio::fs::read_to_string(&abs_path)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read {path}: {e}")))?;

        let crlf = detect_crlf(&raw_content);
        let content = strip_bom(&raw_content);

        // Apply all edits against the original content (non-incremental matching)
        let mut result = content.to_string();
        let mut replaced_count = 0u32;

        for (old_text, new_text) in &edits {
            // Count occurrences before replacing to ensure uniqueness
            let count = result.matches(old_text.as_str()).count();

            if count == 0 {
                // Try fuzzy match
                if let Some((_offset, _was_fuzzy)) = find_match(content, old_text) {
                    // For fuzzy, we re-find in the current result
                    if let Some(fuzzy_offset) = find_match_fuzzy(&result, old_text) {
                        let end = fuzzy_offset + old_text.len();
                        result.replace_range(fuzzy_offset..end, new_text);
                        replaced_count += 1;
                        continue;
                    }
                }
                return Err(ToolError::Execution(format!(
                    "oldText not found in {path}: {old_text:?}"
                )));
            }

            if count > 1 {
                return Err(ToolError::Execution(format!(
                    "oldText matches {count} times in {path}, must match exactly once: {old_text:?}"
                )));
            }

            // Single exact match — replace
            if let Some(idx) = result.find(old_text.as_str()) {
                let end = idx + old_text.len();
                result.replace_range(idx..end, new_text);
                replaced_count += 1;
            }
        }

        // Restore line endings
        if crlf {
            result = result.replace('\n', "\r\n");
        }

        // Restore BOM if original had it
        if raw_content.starts_with('\u{FEFF}') {
            result.insert(0, '\u{FEFF}');
        }

        tokio::fs::write(&abs_path, &result)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write {path}: {e}")))?;

        Ok(ToolResult {
            content: format!("Successfully replaced {replaced_count} block(s) in {path}."),
            is_error: false,
        })
    }
}

/// Find old_text using fuzzy matching in the given content
fn find_match_fuzzy(content: &str, old_text: &str) -> Option<usize> {
    let fuzzy_old = fuzzy_normalize(old_text);

    // Walk through content character by character, checking fuzzy matches
    let chars: Vec<char> = content.chars().collect();
    let fuzzy_old_chars: Vec<char> = fuzzy_old.chars().collect();

    for start in 0..chars.len() {
        if start + fuzzy_old_chars.len() > chars.len() {
            break;
        }

        let candidate: String = chars[start..start + fuzzy_old_chars.len()].iter().collect();
        let fuzzy_candidate = fuzzy_normalize(&candidate);

        if fuzzy_candidate == fuzzy_old {
            // Convert char index to byte index
            let byte_offset: usize = chars[..start].iter().map(|c| c.len_utf8()).sum();
            return Some(byte_offset);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::CancelToken;

    fn make_ctx() -> (CancelToken, ToolContext<'static>) {
        let token = Box::leak(Box::new(CancelToken::new()));
        let ctx = ToolContext::new(token, PathBuf::from("."), PathBuf::from("."));
        let ctx: ToolContext<'static> = unsafe { std::mem::transmute(ctx) };
        (CancelToken::new(), ctx)
    }

    fn test_dir() -> PathBuf {
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent_tools_basic_edit_test_{id}"))
    }

    #[tokio::test]
    async fn test_edit_exact_match() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exact.txt");
        std::fs::write(&file, "hello world").unwrap();

        let tool = EditTool::new(dir.clone());
        let (_cancel, ctx) = make_ctx();
        let input = json!({
            "path": "exact.txt",
            "edits": [{ "oldText": "world", "newText": "rust" }]
        });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("1 block"));
        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "hello rust");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_batch() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("batch.txt");
        std::fs::write(&file, "aaa bbb ccc").unwrap();

        let tool = EditTool::new(dir.clone());
        let (_cancel, ctx) = make_ctx();
        let input = json!({
            "path": "batch.txt",
            "edits": [
                { "oldText": "aaa", "newText": "111" },
                { "oldText": "ccc", "newText": "333" }
            ]
        });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "111 bbb 333");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_legacy_format() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("legacy.txt");
        std::fs::write(&file, "foo bar").unwrap();

        let tool = EditTool::new(dir.clone());
        let (_cancel, ctx) = make_ctx();
        let input = json!({
            "path": "legacy.txt",
            "oldText": "foo",
            "newText": "baz"
        });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "baz bar");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notfound.txt");
        std::fs::write(&file, "hello").unwrap();

        let tool = EditTool::new(dir.clone());
        let (_cancel, ctx) = make_ctx();
        let input = json!({
            "path": "notfound.txt",
            "edits": [{ "oldText": "xyz", "newText": "abc" }]
        });
        let result = tool.call(input, ctx).await;

        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_multiple_matches_error() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("multi.txt");
        std::fs::write(&file, "aaa aaa aaa").unwrap();

        let tool = EditTool::new(dir.clone());
        let (_cancel, ctx) = make_ctx();
        let input = json!({
            "path": "multi.txt",
            "edits": [{ "oldText": "aaa", "newText": "bbb" }]
        });
        let result = tool.call(input, ctx).await;

        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_preserves_crlf() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("crlf.txt");
        std::fs::write(&file, "hello\r\nworld\r\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let (_cancel, ctx) = make_ctx();
        let input = json!({
            "path": "crlf.txt",
            "edits": [{ "oldText": "hello", "newText": "hi" }]
        });
        let result = tool.call(input, ctx).await.unwrap();
        assert!(!result.is_error);

        let written = std::fs::read(&file).unwrap();
        let written_str = String::from_utf8(written).unwrap();
        assert!(written_str.contains("\r\n"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fuzzy_normalize() {
        assert_eq!(fuzzy_normalize("\u{201C}hello\u{201D}"), "\"hello\"");
        assert_eq!(fuzzy_normalize("a\u{2013}b"), "a-b");
        assert_eq!(fuzzy_normalize("a\u{00A0}b"), "a b");
        assert_eq!(fuzzy_normalize("hello\u{200B}world"), "helloworld");
    }
}
