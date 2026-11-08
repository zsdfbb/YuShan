//! 判官的**真实接线**：证据窗口（纯函数）+ 一次模型调用（async）。
//!
//! 设计见 `docs/arch/cross-turn-advance/design.md` §5.5（证据窗口）/ §5.6（调用形态）
//! / §5.7（输出格式），决策见 ADR-0072 D4（助手自述不算证据）/ D8（判官调用不落盘）。
//!
//! 本模块只产出**原始字符串**；判定规则全在 [`crate::goal_judge::parse_judge_output`]。
//! 分成两层是为了可测：网络替身只需返回一个字符串，引用校验由既有单测守着。
//!
//! # 证据窗口的硬契约
//!
//! 窗口 = **最后一轮**的工具调用（名字 + 参数）+ 该轮全部工具输出 + harness 事实。
//! **绝不含助手正文**：助手自述若能进匹配池，「模型自称完成」就能靠判官引用它自己而
//! 通关，引用校验形同虚设（ADR-0072 D4）。
//!
//! 按 [`ContentBlock`] **变体**区分，不靠 [`Role`]：工具结果以 `Role::User` 进 session
//! （`crates/ys-loop/src/basic.rs`），按角色切会把工具输出和用户发言混作一谈。
//!
//! # 只读最后一轮
//!
//! 判官输入因此是 **O(1)**，与总轮数无关。代价是跨轮证据不可见 —— 判官只能核对
//! 本轮可验的项，这是设计接受的取舍（§5.5）。


use std::sync::Arc;

use ys_core::{ContentBlock, Message, Role, ThinkingOverride};
use ys_model::{Model, ModelEventSink, ModelRequest};
use ys_tools_todo::TodoItem;

/// 工具参数的截断上限（字节）。参数只用来让判官认出「跑的是哪条命令」，
/// 全量参数（`write_file` 的整个正文）对判定无益且会把窗口撑爆。
pub(crate) const ARG_LIMIT: usize = 200;

/// 单条工具输出的截断上限（字节）。测试输出常常几千字节，取开头一段足以核对
/// 「测试是否通过」，超出部分对判官没有增量信息。
pub(crate) const OUTPUT_LIMIT: usize = 1200;

/// 判官调用的 token 上限。判官输出是结构化短文本（`VERDICT` + 逐项 + 一条引用），
/// 1024 对它有充裕余量，同时挡住「判官自己写一篇报告」这种跑偏。
pub(crate) const JUDGE_MAX_TOKENS: u32 = 1024;

/// 判官的系统提示词（设计 §5.7 的输出契约）。
///
/// 三条硬要求都写进提示词，但**真正的强制在解析侧**（`parse_judge_output` 的引用
/// 校验）：提示词只是让判官一次就说对，解析器才是兜底。
pub(crate) const JUDGE_SYSTEM: &str = "\
You verify whether an agent's todo items are actually finished, using only the evidence given.

You are given two sections:
- TODO: the items that are still open. These are QUESTIONS to check, never evidence.
- EVIDENCE: tool calls, tool outputs and harness facts from the last round.

Rules:
- An item counts as done only if EVIDENCE contains its proof. The agent saying it is done is \
not proof; EVIDENCE never contains the agent's own prose.
- For each open item, quote the exact text from EVIDENCE that proves it. The quote must be \
copied verbatim (at least 12 characters) and must appear in EVIDENCE character for character.
- If any item has no proof, the verdict is NOT_MET.
- Use BLOCKED only when the goal cannot be reached at all (missing access, impossible request).

Reply in exactly this format, nothing else:

VERDICT: MET | NOT_MET | BLOCKED
ITEMS:
- <item text> | OK | <verbatim quote from EVIDENCE, at least 12 characters>
- <item text> | MISSING | -
EVIDENCE: \"<one verbatim quote from EVIDENCE, at least 12 characters>\"";

/// 按**字符边界**把 `s` 截到至多 `limit` 字节。
///
/// 直接在字节上切会切坏多字节字符（中文工具输出里极常见），下游 JSON / 日志就会
/// 出现替换符。这里退到最近的 `char` 边界。
fn truncate_bytes(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 把工具参数渲染成一行文本：`{"command":"cargo test"}` 原样，非字符串值走 `to_string`。
fn render_arguments(arguments: &serde_json::Value) -> String {
    truncate_bytes(&arguments.to_string(), ARG_LIMIT).to_string()
}

/// 从 session 历史里取「最后一轮」的**证据窗口**。
///
/// 口径（设计 §5.5，逐条落实）：
///
/// 1. 从尾部反向找**最后一条含 `ToolUse` 的 assistant 消息**；它的 `ToolUse` 块即本轮
///    的工具调用（名字 + 参数，参数截 [`ARG_LIMIT`] 字节）；
/// 2. 该消息**之后**的全部 `ToolResult` 块是本轮的工具输出（每条截 [`OUTPUT_LIMIT`]）；
/// 3. **不含任何 `Text` 块** —— 助手正文与用户发言都不进窗口（ADR-0072 D4）。
///
/// 找不到 `ToolUse`（纯聊天回合）⇒ 空字符串：没有工具调用就没有可核对的凭据，
/// 判官对任何项都只能回 `MISSING`。
///
/// harness 事实（HEAD / 轮数 / 耗时 / 工具调用数）由调用方拼在窗口**之外**：
/// 本函数只读 session，不 spawn 进程、不读时钟，故可单测。
pub(crate) fn evidence_window(messages: &[Message]) -> String {
    let Some(start) = messages.iter().rposition(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    }) else {
        return String::new();
    };

    let mut out = String::new();
    for block in &messages[start].content {
        if let ContentBlock::ToolUse {
            name, arguments, ..
        } = block
        {
            out.push_str(&format!("[tool_call] {name} {}\n", render_arguments(arguments)));
        }
    }
    // 只扫**最后一条 ToolUse 消息之后**的块：更早的轮次不属于本窗口。
    for message in &messages[start + 1..] {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            {
                let status = if *is_error { "error" } else { "ok" };
                out.push_str(&format!(
                    "[tool_result:{status}] {}\n",
                    truncate_bytes(content, OUTPUT_LIMIT)
                ));
            }
        }
    }
    out
}

/// 拼出交给判官的完整证据段：证据窗口 + harness 事实。
///
/// harness 事实单独一段，因为它们是**harness 自己观测到的**（不是模型产出的），
/// 判官可以把它们当作可信前提；而工具输出虽然也可信，却需要逐字引用。
pub(crate) fn evidence_text(window: &str, harness_facts: &str) -> String {
    format!(
        "<evidence>\n<harness_facts>\n{harness_facts}\n</harness_facts>\n<tool_trace>\n{window}\
</tool_trace>\n</evidence>"
    )
}

/// 拼出清单段（**待核对的问题**，不是证据）。
pub(crate) fn todo_text(open_items: &[TodoItem]) -> String {
    let mut out = String::from("<todo>\n");
    for item in open_items {
        out.push_str(&format!("- {} [{}]\n", item.content, item.status.as_str()));
    }
    out.push_str("</todo>");
    out
}

/// 判官调用失败（模型侧）。调用点据此计一次可靠性失败。
#[derive(Debug)]
pub(crate) struct JudgeCallError(pub(crate) String);

impl std::fmt::Display for JudgeCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 一次判官请求的构造（纯函数，便于断言「不带工具表 / 关思考」）。
pub(crate) fn judge_request(evidence: &str, open_items: &[TodoItem]) -> ModelRequest {
    let prompt = format!("{}\n\n{}", todo_text(open_items), evidence);
    ModelRequest {
        messages: Arc::new(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }]),
        // 不带工具表：省 schema 开销，且**杜绝判官吐 `tool_call`** —— 它拿不到工具结果，
        // 吐了只会污染这一次调用（与 `ys_loop::summarize` 同形）。
        tools: Arc::new(vec![]),
        system: Some(JUDGE_SYSTEM.into()),
        max_tokens: Some(JUDGE_MAX_TOKENS),
        // `temperature` 刻意不设：适配器是 `config.X.or(request.X)`，在这里设值会被
        // config 的全局温度静默覆盖。
        temperature: None,
        // 判官调用**不挂边界源**：取消只作用于主模型调用。
        boundary: None,
        // **关闭思考**：判官是一个「照格式填表」的短任务，思考链只增加成本与延迟。
        thinking: Some(ThinkingOverride::off()),
    }
}

/// 调判官模型，返回**原始输出**（不解析）。
///
/// 失败重试一次：单次网络抖动不该被判成「判官不可靠」（那会触发停机闸门）。
/// 重试仍失败 ⇒ `Err`，由调用点累加 `judge_failures`。
///
/// **调用不落盘**（ADR-0072 D8）：请求与响应都不进 session、不发事件。
pub(crate) async fn call_judge(
    model: &dyn Model,
    evidence: &str,
    open_items: &[TodoItem],
) -> Result<String, JudgeCallError> {
    let request = judge_request(evidence, open_items);
    // 两次尝试：判官偶发输出空文本或瞬时错误时再给一次机会；仍失败则报不可用，
    // 由调用点计一次 `judge_failures`（连续两次即停机）。
    let mut last = String::new();
    for _ in 0..2 {
        let mut sink = DiscardSink;
        match model.complete(request.clone(), &mut sink).await {
            Ok(response) => {
                let text = response.message.joined_text(ys_core::TextJoin::Newline);
                if !text.trim().is_empty() {
                    return Ok(text);
                }
                last = "judge returned no text".to_string();
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(JudgeCallError(last))
}

/// 判官调用**不产出事件**：增量与思考全部丢弃。
struct DiscardSink;

impl ModelEventSink for DiscardSink {
    fn emit(&mut self, _event: ys_model::ModelEvent) -> Result<(), ys_core::EventError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::ToolCallId;

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn assistant_call(id: &str, name: &str, arguments: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: ToolCallId(id.into()),
                name: name.into(),
                arguments,
            }],
        }
    }

    fn tool_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ToolCallId(id.into()),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    /// **ADR-0072 D4**：助手正文不进证据窗口。
    ///
    /// 变异：把 `Text` 块也拼进窗口 ⇒ 本用例红（`我已全部完成` 会出现在结果里）。
    #[test]
    fn evidence_window_excludes_assistant_prose() {
        let messages = vec![
            user_text("修好登录超时"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "我已全部完成，测试全绿，无需再验".into(),
                    },
                    ContentBlock::ToolUse {
                        id: ToolCallId("c1".into()),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "cargo test"}),
                    },
                ],
            },
            tool_result("c1", "test result: ok. 12 passed"),
        ];
        let window = evidence_window(&messages);
        assert!(
            !window.contains("我已全部完成"),
            "助手正文不得进证据窗口（ADR-0072 D4）：{window}"
        );
        assert!(window.contains("cargo test"), "工具调用应在窗口里：{window}");
        assert!(
            window.contains("12 passed"),
            "工具输出应在窗口里：{window}"
        );
    }

    /// 只取**最后一轮**：更早轮次的调用与输出都不进窗口。
    ///
    /// 变异：把 `start + 1..` 改成 `0..` ⇒ 本用例红（第一轮的 `first-round-output` 出现）。
    #[test]
    fn evidence_window_takes_only_last_round() {
        let messages = vec![
            user_text("修好登录超时"),
            assistant_call("c1", "bash", serde_json::json!({"command": "echo first"})),
            tool_result("c1", "first-round-output"),
            assistant_text("第一轮做完一部分"),
            assistant_call("c2", "bash", serde_json::json!({"command": "cargo test"})),
            tool_result("c2", "last-round-output"),
        ];
        let window = evidence_window(&messages);
        assert!(
            !window.contains("first-round-output"),
            "上一轮的输出不属于本窗口：{window}"
        );
        assert!(
            !window.contains("echo first"),
            "上一轮的调用不属于本窗口：{window}"
        );
        assert!(window.contains("cargo test"), "本轮调用应在窗口里：{window}");
        assert!(
            window.contains("last-round-output"),
            "本轮输出应在窗口里：{window}"
        );
    }

    /// 超长输出截到 [`OUTPUT_LIMIT`] 字节，且结果仍是合法 UTF-8。
    ///
    /// 用中文构造：每个字 3 字节，`OUTPUT_LIMIT = 1200` 不是 3 的倍数 ⇒ 朴素字节切
    /// 会落在字符中间。变异：把 [`truncate_bytes`] 换成 `&s[..limit]` ⇒ 本用例 panic。
    #[test]
    fn evidence_window_truncates_long_output() {
        // 1000 个汉字 = 3000 字节，远超上限。
        let long: String = "测试输出".repeat(250);
        assert!(long.len() > OUTPUT_LIMIT);
        let messages = vec![
            assistant_call("c1", "bash", serde_json::json!({"command": "cargo test"})),
            tool_result("c1", &long),
        ];
        let window = evidence_window(&messages);

        // 窗口里那段输出必须是合法 UTF-8 且不超过上限 —— 用字节切片再解码验证，
        // 同时确认 `to_string()` 没有 panic（非法边界会在上面就炸）。
        let body = window
            .split("[tool_result:ok] ")
            .nth(1)
            .expect("窗口里应有工具输出段");
        let body = body.trim_end_matches('\n');
        assert!(
            body.len() <= OUTPUT_LIMIT,
            "输出应被截到 {OUTPUT_LIMIT} 字节以内，实际 {}",
            body.len()
        );
        assert!(
            body.chars().all(|c| c == '测' || c == '试' || c == '输' || c == '出'),
            "截断不得切坏多字节字符：{body}"
        );
    }

    /// 判官请求**不带工具表**且**关闭思考**（省 schema 开销、杜绝 tool_call）。
    #[test]
    fn judge_request_disables_tools_and_thinking() {
        let request = judge_request("evidence", &[]);
        assert!(request.tools.is_empty(), "判官调用不得带工具表");
        assert!(
            matches!(request.thinking, Some(ref t) if t.disables_thinking()),
            "判官调用必须关闭思考"
        );
        assert_eq!(request.max_tokens, Some(JUDGE_MAX_TOKENS));
    }
}
