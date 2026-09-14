//! 结构化的对话记录（设计 §2.1「transcript 必须是结构化的」）。
//!
//! 不能像旧的 `ui/app.rs` 那样把参数与结果预先拍成 `String` —— 压成一行需要
//! **从结构化参数里抽取摘要**，而摘要规则**由产品决定**（coding：`read src/x.rs`；
//! 投资会是 `market 510300`）。

use serde_json::Value;
use ys_core::{StopReason, ToolCallId};

/// 对话区的一「条」记录。
///
/// 注意：一条不一定等于一**行** —— assistant 文本会被 wrap 成多行，
/// 滚动计算按 wrap 后的视觉行数走（见 `draw.rs`）。
#[derive(Clone, Debug)]
pub enum TranscriptLine {
    User(String),
    Assistant(String),
    /// 模型思考（默认隐藏，`/thinking on` 后以暗色斜体渲染）。
    Thinking(String),
    Tool {
        id: ToolCallId,
        name: String,
        /// 参数摘要（由 [`summarize_tool_args`] 从结构化参数抽取）。
        summary: String,
        /// 工具结果：`None` = 还没回来。
        result: Option<String>,
        /// 失败时 `false`（渲染 ✗；失败才显示结果首行）。
        success: bool,
    },
    Summary {
        rounds: u32,
        stop: StopReason,
        elapsed_secs: f32,
    },
    Error(String),
    System(String),
}

/// 摘要里优先取用的 key —— 依次尝试，取**第一个存在的字符串值**。
///
/// 这是 coding agent 的产品知识（`read`/`write` 的 `path`、`bash` 的 `command`…），
/// 故放在本 crate 而不是通用协议里。
const SUMMARY_KEYS: [&str; 6] = ["path", "file_path", "command", "pattern", "query", "url"];

/// 摘要的最大字符数（**字符**，不是字节 —— 中文按 1 个算）。
const SUMMARY_MAX_CHARS: usize = 40;

/// 从工具参数里抽一行摘要。
///
/// 顺序：
/// 1. [`SUMMARY_KEYS`] 里的 key，取第一个字符串值
/// 2. 否则取参数里递归找到的第一个字符串值
/// 3. 否则用紧凑 JSON（`serde_json::Value::to_string`）
///
/// 末了按**字符边界**截断到 [`SUMMARY_MAX_CHARS`]，超长补 `…`。
pub fn summarize_tool_args(_name: &str, args: &Value) -> String {
    for key in SUMMARY_KEYS {
        if let Some(Value::String(s)) = args.get(key) {
            return truncate_chars(s, SUMMARY_MAX_CHARS);
        }
    }
    if let Some(s) = first_string(args) {
        return truncate_chars(&s, SUMMARY_MAX_CHARS);
    }
    truncate_chars(&args.to_string(), SUMMARY_MAX_CHARS)
}

/// 深度优先找第一个字符串值（对象按 key 序、数组按序 —— 都是确定的）。
fn first_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map.values().find_map(first_string),
        Value::Array(items) => items.iter().find_map(first_string),
        _ => None,
    }
}

/// 按 **char 边界**截断（超长补 `…`）。
///
/// **绝不允许** `&s[..n]` 这种字节切片 —— 多字节字符中间会 panic
/// （旧 `ui/events.rs:131` 的中文退格 panic 是同一类错误的另一面）。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    // 留一位给省略号，保证总长恰好是 max_chars
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_summarize_prefers_known_keys_in_order() {
        // path 优先于 command（顺序即优先级）
        let args = json!({ "command": "rm -rf /", "path": "src/x.rs" });
        assert_eq!(summarize_tool_args("edit", &args), "src/x.rs");
        assert_eq!(
            summarize_tool_args("read", &json!({"file_path": "a/b.md"})),
            "a/b.md"
        );
        assert_eq!(
            summarize_tool_args("bash", &json!({"command": "ls -la"})),
            "ls -la"
        );
        assert_eq!(
            summarize_tool_args("grep", &json!({"pattern": "fn main"})),
            "fn main"
        );
    }

    #[test]
    fn test_summarize_falls_back_to_first_string() {
        // 没有已知 key：取第一个字符串值（不确定 key 名也能给点上下文）
        let args = json!({ "cmd": "ls", "flag": true });
        assert_eq!(summarize_tool_args("bash", &args), "ls");
    }

    #[test]
    fn test_summarize_falls_back_to_compact_json() {
        // 没有任何字符串：退回紧凑 JSON
        let args = json!({ "n": 3, "deep": { "flag": true } });
        let s = summarize_tool_args("weird", &args);
        assert!(s.contains("\"n\":3"), "got {s}");
    }

    #[test]
    fn test_summarize_empty_object() {
        assert_eq!(summarize_tool_args("noop", &json!({})), "{}");
    }

    #[test]
    fn test_summarize_truncates_long_ascii_at_char_boundary() {
        let long = "a".repeat(200);
        let s = summarize_tool_args("read", &json!({ "path": long }));
        assert_eq!(s.chars().count(), SUMMARY_MAX_CHARS);
        assert!(s.ends_with('…'));
    }

    /// 回归：超长 CJK 摘要**不得 panic**（按 char，不按字节）。
    #[test]
    fn test_summarize_truncates_long_cjk_without_panic() {
        let long = "中文路径很长的目录/文件.rs".repeat(20);
        let s = summarize_tool_args("read", &json!({ "path": long }));
        assert_eq!(s.chars().count(), SUMMARY_MAX_CHARS);
        assert!(s.ends_with('…'));
        // 截断后的内容必须仍是合法 UTF-8 前缀（能走到这里就没 panic）
        assert!(s.starts_with("中文路径"));
    }

    /// 45 个汉字（每字 3 字节 = 135 字节）：旧的按字节截断必然 panic。
    #[test]
    fn test_truncate_boundary_exactly_one_over() {
        let s = "汉".repeat(41);
        let out = truncate_chars(&s, SUMMARY_MAX_CHARS);
        assert_eq!(out.chars().count(), SUMMARY_MAX_CHARS);
        assert_eq!(out.chars().filter(|c| *c == '汉').count(), 39);
    }

    #[test]
    fn test_truncate_short_string_untouched() {
        assert_eq!(truncate_chars("短", 40), "短");
        assert_eq!(truncate_chars("", 40), "");
    }
}
