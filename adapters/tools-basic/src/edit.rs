use serde_json::{Value, json};
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use crate::workspace_path::{absolute_notice, attach_notice};

pub struct EditTool {
    workspace: PathBuf,
}

impl EditTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

/// 为模糊匹配规范化文本的**单字符层**：NFKC + 智能引号 + 破折号 + 不间断空格 + 零宽字符。
///
/// 不做 `trim`、不折叠换行 —— 逐字符推进时依赖这一点：否则正文里的空格在累积时会丢掉，
/// 含空白的 `oldText` 永远命中不了。
fn normalize_chars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.nfkc() {
        match ch {
            // 智能引号转为 ASCII
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2039}' | '\u{203A}'
            | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => result.push('"'),
            // En/Em 破折号转为 ASCII 连字符
            '\u{2013}' | '\u{2014}' | '\u{2015}' => result.push('-'),
            // 不间断空格转为 ASCII 空格
            '\u{00A0}' => result.push(' '),
            // 零宽空格与连接符 —— 移除
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => {}
            _ => result.push(ch),
        }
    }
    result
}

/// 模糊比较的规范形：单字符规范化 + 行尾折叠（`\r\n` / `\r` → `\n`）+ `trim`。
///
/// 折叠与 `trim` 都会**改变文本长度**，因此只可用于比较，不可用于定位区间。
fn fuzzy_normalize(s: &str) -> String {
    normalize_chars(s)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

/// 检测换行风格：CRLF 占主导时返回 true
fn detect_crlf(content: &str) -> bool {
    let crlf_count = content.matches("\r\n").count();
    let lf_count = content.matches('\n').count() - crlf_count;
    crlf_count > lf_count
}

/// 去掉内容开头的 BOM
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

    // 尝试旧版顶层 oldText/newText
    if let Some(old) = input.get("oldText").and_then(|v| v.as_str()) {
        let new = input.get("newText").and_then(|v| v.as_str()).unwrap_or("");
        edits.push((old.to_string(), new.to_string()));
        return Ok(edits);
    }

    // 尝试 edits 字段
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
            // edits 是字符串 —— 当作单个 oldText 处理
            edits.push((s.clone(), String::new()));
        }
        Some(Value::Object(_)) => {
            // 单个对象当作数组处理
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

#[async_trait::async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit",
            "Edit files with precise or fuzzy text matching. Supports batch edits.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit. Relative paths resolve against the workspace root; use an absolute path only for files outside it." },
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
        let path_notice = absolute_notice("edit", &path, &self.workspace);
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

        // 在原始内容上应用所有 edits（非增量匹配）
        let mut result = content.to_string();
        let mut replaced_count = 0u32;
        let mut fuzzy_ranges = Vec::new();

        for (old_text, new_text) in &edits {
            // 替换前统计层数，确保唯一性
            let count = result.matches(old_text.as_str()).count();

            if count == 0 {
                // 精确匹配零命中，退到归一化匹配
                let hits = fuzzy_find_all(&result, old_text);
                for (offset, len) in hits.iter() {
                    fuzzy_ranges.push(result[*offset..*offset + *len].to_string());
                }
                match hits.as_slice() {
                    // 多处模糊命中 ⇒ 不知道改哪一处，拒绝而不是猜
                    [_, _, ..] => {
                        return Err(ToolError::Execution(format!(
                            "oldText matches {} times in {path} after normalizing whitespace and \
                             quotes, must match exactly once: {old_text:?}",
                            hits.len()
                        )));
                    }
                    // 替换范围用**实际匹配到的**字节长度：`old_text` 归一化前后的长度
                    // 可以不相等（`trim` 与零宽字符移除都会改变长度）
                    [(offset, len)] => {
                        result.replace_range(*offset..*offset + *len, new_text);
                        replaced_count += 1;
                        continue;
                    }
                    [] => {}
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

            // 单一精确匹配 —— 替换
            if let Some(idx) = result.find(old_text.as_str()) {
                let end = idx + old_text.len();
                result.replace_range(idx..end, new_text);
                replaced_count += 1;
            }
        }

        // 恢复换行符
        if crlf {
            result = result.replace('\n', "\r\n");
        }

        // 若原文有 BOM 则恢复
        if raw_content.starts_with('\u{FEFF}') {
            result.insert(0, '\u{FEFF}');
        }

        tokio::fs::write(&abs_path, &result)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write {path}: {e}")))?;

        // 模糊命中必须在结果里说明：模型否则会以为自己精确命中，继续基于错误假设改文件
        let fuzzy_notices: Vec<String> = fuzzy_ranges
            .iter()
            .map(|matched| {
                format!(
                    "edit: matched {matched:?} by normalizing whitespace and quotes (not an exact match)"
                )
            })
            .collect();

        let mut tool_result = ToolResult {
            content: format!("Successfully replaced {replaced_count} block(s) in {path}."),
            is_error: false,
        };
        ys_tool::append_notices(&mut tool_result.content, &fuzzy_notices);
        Ok(attach_notice(tool_result, path_notice.as_deref()))
    }
}

/// 在 content 中查找模糊命中的全部区间，每项为 `(byte_offset, byte_len)`。
///
/// 区间由**原文**逐字符推进得到，`byte_len` 是实际匹配到的字节长度，
/// 与 `old_text.len()` **不保证相等**（`trim`、零宽字符移除、智能引号、不间断空格
/// 都会让两侧长度不同）。调用方切片必须用它，不能用 `old_text.len()`。
fn fuzzy_find_all(content: &str, old_text: &str) -> Vec<(usize, usize)> {
    let target = fuzzy_normalize(old_text);
    if target.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<(usize, usize)> = Vec::new();
    for (start, start_char) in content.char_indices() {
        // 起点空白属于被 `trim` 丢掉的部分，不属于命中区间
        if start_char.is_whitespace() {
            continue;
        }

        let mut norm = String::new();
        for (idx, ch) in content[start..].char_indices() {
            // 行尾折叠：`\r\n` / `\r` 与 `\n` 等义（与 `fuzzy_normalize` 一致）
            if ch == '\r' {
                continue;
            }
            norm.push_str(&normalize_chars(&ch.to_string()));

            if norm == target {
                hits.push((start, idx + ch.len_utf8()));
                break;
            }
            // 已不是目标前缀 ⇒ 本起点不可能再命中
            if !target.starts_with(&norm) {
                break;
            }
        }
    }

    hits
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
            "agent_tools_basic_edit_test_{id}_{}_{seq}",
            std::process::id()
        ))
    }

    #[test]
    fn test_test_dir_is_unique() {
        let a = test_dir();
        let b = test_dir();
        assert_ne!(a, b, "consecutive test_dir() calls must not collide");
    }

    #[tokio::test]
    async fn test_edit_exact_match() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exact.txt");
        std::fs::write(&file, "hello world").unwrap();

        let tool = EditTool::new(dir.clone());
        let ctx = make_ctx();
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

    /// workspace 内绝对路径成功替换后追加相对路径事实。
    ///
    /// 变异：删掉 `absolute_notice` 或 `attach_notice` ⇒ 断言变红。
    #[tokio::test]
    async fn absolute_workspace_path_adds_relative_hint() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("absolute.txt");
        std::fs::write(&file, "hello world").unwrap();

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": file.to_string_lossy(),
                    "edits": [{ "oldText": "world", "newText": "rust" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(
            result
                .content
                .contains("edit: this path is inside the workspace root"),
            "content = {}",
            result.content
        );
        assert!(result.content.contains("`absolute.txt`"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello rust");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_batch() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("batch.txt");
        std::fs::write(&file, "aaa bbb ccc").unwrap();

        let tool = EditTool::new(dir.clone());
        let ctx = make_ctx();
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
        let ctx = make_ctx();
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
        let ctx = make_ctx();
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
        let ctx = make_ctx();
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
        let ctx = make_ctx();
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

    /// 模糊命中时的替换范围必须按**实际匹配到的文本**计算。
    ///
    /// 变异：把切片长度换回 `old_text.len()` ⇒ 本例 panic（越界）。
    #[tokio::test]
    async fn fuzzy_match_uses_actual_matched_length() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fuzzy.txt");
        // 模型多带首尾空白 ⇒ 精确匹配 0 次，模糊命中范围比原串短 4 字节
        std::fs::write(&file, "hello world").unwrap();

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "fuzzy.txt",
                    "edits": [{ "oldText": "  hello world  ", "newText": "bye" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bye");
        // 模糊命中必须在结果里说明，模型才知道自己没精确命中
        assert!(
            result.content.contains("normaliz"),
            "content = {}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 命中范围跨多字节字符（NBSP 占 2 字节）时，切片端点仍须落在字符边界上。
    ///
    /// 变异：仍用 `old_text.len()`（3 字节）⇒ 端点落进 NBSP 内部 ⇒ panic。
    #[tokio::test]
    async fn fuzzy_match_range_keeps_char_boundaries() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("nbsp.txt");
        // 文件里是不间断空格，模型打的是直空格
        std::fs::write(&file, "x a\u{00A0}b y").unwrap();

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "nbsp.txt",
                    "edits": [{ "oldText": "a b", "newText": "Z" }]
                }),
                make_ctx(),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x Z y");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模糊命中保留原文边界字符（被 `trim` 吃掉的首尾空白不属于命中范围）。
    #[tokio::test]
    async fn fuzzy_match_preserves_original_boundary_whitespace() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("boundary.txt");
        std::fs::write(&file, "hello world\nsecond line\n").unwrap();

        let tool = EditTool::new(dir.clone());
        tool.call(
            json!({
                "path": "boundary.txt",
                "edits": [{ "oldText": "  hello world  \n", "newText": "bye" }]
            }),
            make_ctx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "bye\nsecond line\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模糊匹配命中多处时同样必须拒绝，不能猜改哪一处。
    #[tokio::test]
    async fn fuzzy_match_multiple_locations_error() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fuzzy_multi.txt");
        std::fs::write(&file, "a\u{00A0}b and a\u{00A0}b").unwrap();

        let tool = EditTool::new(dir.clone());
        let result = tool
            .call(
                json!({
                    "path": "fuzzy_multi.txt",
                    "edits": [{ "oldText": "a b", "newText": "Z" }]
                }),
                make_ctx(),
            )
            .await;

        let err = result.expect_err("ambiguous fuzzy match must be rejected");
        assert!(err.to_string().contains("2 times"), "err = {err}");
        // 拒绝时文件必须保持原样
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "a\u{00A0}b and a\u{00A0}b"
        );

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
