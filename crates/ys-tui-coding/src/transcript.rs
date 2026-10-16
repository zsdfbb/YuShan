//! 结构化的对话记录（设计 §2.1「transcript 必须是结构化的」）。
//!
//! 不能像旧的 `ui/app.rs` 那样把参数与结果预先拍成 `String` —— 压成一行需要
//! **从结构化参数里抽取摘要**，而摘要规则**由产品决定**（coding：`read src/x.rs`；
//! 投资会是 `market 510300`）。

use serde_json::Value;
use ys_core::{ContentBlock, Message, Role, StopReason, ToolCallId};

use std::collections::HashMap;

/// 对话区的一「条」记录。
///
/// 注意：一条不一定等于一**行** —— assistant 文本会被 wrap 成多行，
/// 滚动计算按 wrap 后的视觉行数走（见 `draw.rs`）。
#[derive(Clone, Debug, PartialEq)]
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
const SUMMARY_KEYS: [&str; 7] = [
    "path",
    "file_path",
    "command",
    "pattern",
    "query",
    "url",
    // 派发工具的「给用户看」标签（ADR-0022 决策 11）—— 排在最后，只有前面都没有时才取它。
    "description",
];

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

/// 把持久化的会话历史（[`Message`] 的**复合**块）转成 transcript 行 —— `/resume` 回放专用。
///
/// **镜像 [`App::apply_event`] 的输出约定**，不是「共用同一套规则」：那边输入是**单块事件**
/// （`ModelTextDelta` / `ToolCall` / `ToolResult` 各自一条），这边输入是**复合 `Message`**
/// （一条可同时含 Text + ToolUse）。形状不同，故本函数独立实现；共享的只有
/// [`summarize_tool_args`]、[`crate::app::message_text`]（`User` 行取文本的规则同源）与
/// 「按 `ToolCallId` 配对」的约定。
///
/// **等价性边界**：回放**除下列已知形状分叉外**与 live 路径一致。「一致」是被测试钉住的
/// **当前形状**，不是定理 —— 每条分叉都由 `p1_*_diverges_*` 用例显式断言 `replay != live`
/// 的具体形状，任一侧改动都会立刻变红。分叉根源是**历史里没有 live 侧才有的状态**
/// （`is_turning`、提交原文、流式合并窗口），回放拿不到、也就无法复刻：
///
/// 1. **app 发起的复核回合**：主回合 `RunFinished` 后 `is_turning == false`，此时到达的
///    `UserMessage` 在 live 侧渲染成 [`System`](TranscriptLine::System)（`app.rs` 的 ③④ 分支），
///    回放只有「role = User」这一事实，产出 [`User`](TranscriptLine::User)。
/// 2. **`/skill-name` 提交**：live 侧本地回显**展开前原文**（`User("/myskill")`，随后到达的
///    `<skill …>` 信封回声被 `suppress_user_echo` 抑制），而落盘的是
///    `expand_skill_invocation` 改写的信封 ⇒ 回放产出 `User("<skill …>…</skill>")`。**空闲提交**
///    时 live 也已不同（`User("/myskill") ≠ User(信封)`）。
/// 3. **相邻两条 assistant 文本消息**：live 侧 `append_stream` 按「相邻同类合并」并成一条
///    （`Assistant("ab")`），回放按消息边界各起一条（`Assistant("a"), Assistant("b")`）。
///    本仓 agent loop **不会**产出相邻 assistant 消息 ⇒ 实际不可达，此条仅作**防御性断言**。
///
/// live 的 `is_turning` 启发式（分叉 1）/ 提交原文（分叉 2）/ 流式合并窗口（分叉 3）都**不
/// 在历史里**，回放侧无信息可依 ⇒ 有意不复刻，作为**已知代价**接受（设计 §7）。
///
/// 与 `apply_event` 的**两处有意差异**（设计 I11）：
/// - `Role::User` 且**没有非空 Text**（典型是只含 `ToolResult`）⇒ **不产出行**。这类消息在真实
///   会话里存在（工具结果作为一条 User 消息落盘）；若照搬 `apply_event` 会对空文本也推一条
///   `User("")`，屏上就多一条空用户行。
/// - `ToolUse` **无配对 `ToolResult`** ⇒ `success = false`，**不照抄** `apply_event` 的
///   `success: true` 默认值 —— 被中断的会话里悬空工具行不得看着像成功。
///
/// **不产出的行**：`Summary` / `System`（回合摘要、命令输出**不持久化**，历史里没有它们）；
/// `Thinking` 亦然 —— [`ContentBlock`] 没有 thinking 变体（思考内容不落盘）。
///
/// 配对按 `ToolCallId` 做**全局**预扫描（回填在第二遍），故不要求 `ToolResult` 紧跟其
/// `ToolUse`（真实历史里工具结果在**下一条** User 消息里）。重复 id 时结果落到**最后**一条
/// `Tool` 行（与 `apply_event` 覆盖 `tool_index` 的语义一致）。
pub fn replay_lines(messages: &[Message]) -> Vec<TranscriptLine> {
    let mut lines: Vec<TranscriptLine> = Vec::new();
    // `ToolCallId` → `lines` 下标（重复 id 由后出现者覆盖，镜像 `apply_event`）。
    let mut tool_index: HashMap<ToolCallId, usize> = HashMap::new();

    // 第一遍：产出行。`Tool` 行先按「无配对」落位（`success = false`、`result = None`）。
    for message in messages {
        match message.role {
            Role::Assistant => {
                // 一条 assistant 消息 → 一条 `Assistant` 行；Text 块直接拼接
                // （镜像流式期多个 `ModelTextDelta` 合并成一条的行为）。
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        // 非 `Text`/`ToolUse` 的变体**有意忽略**（回放只认识这两类）。
                        // `ContentBlock` 是 `#[non_exhaustive]`：**加变体时必须同步本函数与
                        // `crate::app::message_text`** —— 二者都按白名单取块，漏了就静默不渲染、
                        // 不报错也不告警（无编译期兜底）。
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    lines.push(TranscriptLine::Assistant(text));
                }
            }
            Role::User => {
                // 复用 `apply_event` 的取文本规则（只取 Text 块、按换行拼接），保证两路径同源。
                let text = crate::app::message_text(message);
                if !text.trim().is_empty() {
                    lines.push(TranscriptLine::User(text));
                }
            }
            // `Role::System`（系统提示等）不属 transcript，不产出行。
            Role::System => continue,
        }
        // `ToolUse` 两角色都处理（User 消息正常不含工具调用，含了也不丢信息）。
        // 其余变体在此同样**有意忽略**（白名单见上方 `filter_map` 的注释）。
        for block in &message.content {
            if let ContentBlock::ToolUse {
                id,
                name,
                arguments,
            } = block
            {
                let idx = lines.len();
                lines.push(TranscriptLine::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: summarize_tool_args(name, arguments),
                    result: None,
                    success: false,
                });
                tool_index.insert(id.clone(), idx);
            }
        }
    }

    // 第二遍：按 `ToolCallId` 回填结果（覆盖第一遍的「无配对」默认值）。
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } = block
                && let Some(&idx) = tool_index.get(tool_call_id)
                && let Some(TranscriptLine::Tool {
                    result, success, ..
                }) = lines.get_mut(idx)
            {
                *result = Some(content.clone());
                *success = !*is_error;
            }
        }
    }

    lines
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

    // ---- 回放（`/resume`）：N9 / N10 ----

    use crate::app::{App, user_message};
    use crate::view::CodingView;
    use ys_core::{ToolCall, ToolResult};
    use ys_event::AgentEvent;
    use ys_protocol::{Envelope, Source};

    fn assistant_msg(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content: blocks,
        }
    }

    fn user_msg(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            content: blocks,
        }
    }

    fn tool_use(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: ToolCallId(id.into()),
            name: name.into(),
            arguments: args,
        }
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_call_id: ToolCallId(id.into()),
            content: content.into(),
            is_error,
        }
    }

    /// 包一条 agent 事件成 `Envelope`（live 路径喂给 `App::apply_event`）。
    fn env(event: AgentEvent) -> Envelope {
        Envelope::new(Source::agent(), 1, event)
    }

    /// **N9 边界 ①**：`User` 消息只含 `ToolResult`（无 Text）时**不得**渲染成空用户行。
    ///
    /// 这种消息在真实会话里存在（工具结果作为一条 User 消息落盘）。变异：把
    /// `Role::User` 分支的 `!text.trim().is_empty()` 门槛去掉 ⇒ 本用例红（多一条 `User("")`）。
    #[test]
    fn n9_tool_result_only_user_message_makes_no_empty_user_line() {
        let msgs = vec![
            assistant_msg(vec![
                ContentBlock::Text {
                    text: "看下文件".into(),
                },
                tool_use("c1", "read", json!({"path": "src/x.rs"})),
            ]),
            user_msg(vec![tool_result("c1", "file body", false)]),
        ];
        let lines = replay_lines(&msgs);

        assert!(
            !lines
                .iter()
                .any(|l| matches!(l, TranscriptLine::User(s) if s.trim().is_empty())),
            "纯 ToolResult 的 User 消息不得渲染成空用户行：{lines:?}"
        );
        assert_eq!(lines.len(), 2, "只有 Assistant + Tool 两行：{lines:?}");
        assert!(matches!(&lines[0], TranscriptLine::Assistant(s) if s == "看下文件"));
    }

    /// **N9 边界 ②**：`ToolUse` 无配对 `ToolResult`（被中断的会话）**不得**显示为成功。
    ///
    /// 变异：把第一遍里的 `success: false` 改回 `apply_event` 的默认 `true` ⇒ 本用例红。
    #[test]
    fn n9_unpaired_tool_use_is_not_rendered_as_success() {
        let msgs = vec![assistant_msg(vec![tool_use(
            "c9",
            "bash",
            json!({"command": "ls"}),
        )])];
        let lines = replay_lines(&msgs);

        match &lines[0] {
            TranscriptLine::Tool {
                id,
                name,
                summary,
                result,
                success,
            } => {
                assert_eq!(id, &ToolCallId("c9".into()));
                assert_eq!(name, "bash");
                assert_eq!(summary, "ls");
                assert!(result.is_none(), "无配对 ⇒ 无结果");
                assert!(!*success, "悬空工具不得显示为成功（I11）：{lines:?}");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    /// 有配对且 `is_error = true` ⇒ `success = false`、`result = Some(内容)`、`summary` 走
    /// [`summarize_tool_args`]。
    #[test]
    fn n9_paired_tool_result_sets_success_and_result() {
        let msgs = vec![
            assistant_msg(vec![tool_use("c1", "read", json!({"path": "src/x.rs"}))]),
            user_msg(vec![tool_result("c1", "boom", true)]),
        ];
        let lines = replay_lines(&msgs);

        match &lines[0] {
            TranscriptLine::Tool {
                summary,
                result,
                success,
                ..
            } => {
                assert_eq!(summary, "src/x.rs");
                assert_eq!(result.as_deref(), Some("boom"));
                assert!(!*success, "is_error=true ⇒ 失败");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    /// **N10 跨路径逐行等价**：同一逻辑会话，live 走 `apply_event`、离线走 `replay_lines`，
    /// 二者输出必须**逐行相等**（钉住「镜像」不漂移）。
    ///
    /// live 侧用**真实的本地提交路径** `begin_local_turn`（它会登记回显抑制），随后投出的
    /// `UserMessage` 回声被正常抑制 —— 这正是生产里 User 行的产生方式，故两路径应无差异。
    /// 回合摘要（`RunFinished` → `Summary`）不喂：它不持久化，回放本就不产出（设计 §7）。
    #[test]
    fn n10_replay_matches_live_apply_event_line_for_line() {
        let user = user_message("hello");
        let args = json!({"path": "src/x.rs"});
        let assistant = assistant_msg(vec![
            ContentBlock::Text { text: "hi".into() },
            tool_use("c1", "read", args.clone()),
        ]);
        let results = user_msg(vec![tool_result("c1", "body", false)]);

        // live 路径。
        let mut a = App::new(CodingView::for_test());
        a.begin_local_turn("hello".into()); // 本地回显 User("hello") + 登记一条待抑制回声
        let env = |event| Envelope::new(Source::agent(), 1, event);
        a.apply_event(env(AgentEvent::UserMessage {
            message: user.clone(),
        })); // 回声：被抑制，不重复画
        a.apply_event(env(AgentEvent::ModelTextDelta { text: "hi".into() }));
        a.apply_event(env(AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId("c1".into()),
                name: "read".into(),
                arguments: args,
            },
        }));
        a.apply_event(env(AgentEvent::ToolResult {
            id: ToolCallId("c1".into()),
            result: ToolResult {
                content: "body".into(),
                is_error: false,
            },
        }));

        let replay = replay_lines(&[user, assistant, results]);
        assert_eq!(
            replay, a.transcript,
            "回放必须与 live 逐行相等：\nreplay={replay:?}\nlive={:?}",
            a.transcript
        );
    }

    /// **P2-3**：钉住 `replay_lines` 的**已处理块集合**（当前 = `Text` + `ToolUse`）。
    ///
    /// `ContentBlock` 是 `#[non_exhaustive]`：将来加变体若不在本函数白名单里，就会**静默不
    /// 渲染**（不报错、不告警）。本用例是「加变体时必须同步更新」的检查点 —— 块集合一变即红。
    #[test]
    fn p2_3_only_text_and_tool_use_produce_lines() {
        let msgs = vec![
            assistant_msg(vec![
                ContentBlock::Text {
                    text: "看下".into(),
                },
                tool_use("c1", "read", json!({"path": "a"})),
            ]),
            user_msg(vec![
                ContentBlock::Text {
                    text: "继续".into(),
                },
                // `ToolResult` 不单独产行 —— 只用于回填配对的 `Tool` 行。
                tool_result("c1", "body", false),
            ]),
        ];
        let kinds: Vec<&str> = replay_lines(&msgs)
            .iter()
            .map(|l| match l {
                TranscriptLine::User(_) => "user",
                TranscriptLine::Assistant(_) => "assistant",
                TranscriptLine::Tool { .. } => "tool",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["assistant", "tool", "user"],
            "块集合变化须同步本用例（并更新 replay_lines 的白名单注释）"
        );
    }

    // ---- 回放 ≠ 重演：已知形状分叉（P1-1） ----

    /// **P1-1 分叉 ①**：`is_turning == false` 时到达的 `UserMessage`（回合刚落幕、事件还在
    /// 信道里排队的那种收尾竞态）。
    ///
    /// live 侧渲染成 `System(text)`（`RunFinished` 已让 `finish_turn` 复位 `is_turning`），
    /// 回放侧只有「role = User」这一事实 ⇒ 渲染成 `User(text)`。历史里没有 `is_turning`，
    /// 回放**无法**复刻（有意不复刻，见函数文档与设计 §7）。本用例钉住该分叉。
    ///
    /// 注：这个分叉原先是「完工复核回合」的落点，复核已由 ADR-0041 撤除 ⇒ 现在只剩上述竞态。
    #[test]
    fn p1_1_idle_user_message_diverges_system_vs_user() {
        let review = user_message("审查一下上面的改动");
        assert!(
            !App::new(CodingView::for_test()).is_turning,
            "前提：新建 App 不在回合中"
        );

        // live：空闲时到达的 UserMessage ⇒ System 行。
        let mut a = App::new(CodingView::for_test());
        a.apply_event(env(AgentEvent::UserMessage {
            message: review.clone(),
        }));
        assert!(
            matches!(&a.transcript[..], [TranscriptLine::System(s)] if s == "审查一下上面的改动"),
            "live 复核回合应渲染成 System：{:?}",
            a.transcript
        );

        // 回放：同一条消息只有 role=User ⇒ User 行。
        let replay = replay_lines(&[review]);
        assert!(
            matches!(&replay[..], [TranscriptLine::User(s)] if s == "审查一下上面的改动"),
            "回放应渲染成 User：{replay:?}"
        );

        assert_ne!(
            replay, a.transcript,
            "P1-1 分叉 ①：live(System) ≠ replay(User)"
        );
    }

    /// **P1-1 分叉 ②**：`/skill-name` 提交。
    ///
    /// live 侧本地回显**展开前原文**（`User("/myskill")`），随后到达的 `<skill …>` 信封回声被
    /// `suppress_user_echo` 抑制；落盘的是 `expand_skill_invocation` 改写的信封 ⇒ 回放产出
    /// `User(信封)`。**空闲提交**时 live 就已不同 —— 是**内容级**分叉，不只是 System 回退。
    /// 本仓 agent loop 恒展开 skill，回放侧无从复刻原始输入。
    #[test]
    fn p1_1_skill_invocation_diverges_raw_vs_envelope() {
        let envelope = "<skill name=\"myskill\" location=\"/s/SKILL.md\">正文</skill>";
        let persisted = user_message(envelope);

        // live：空闲提交 —— 本地回显原文，回声（信封）被抑制。
        let mut a = App::new(CodingView::for_test());
        a.begin_local_turn("/myskill".into());
        a.apply_event(env(AgentEvent::UserMessage {
            message: persisted.clone(),
        }));
        assert!(
            matches!(&a.transcript[..], [TranscriptLine::User(s)] if s == "/myskill"),
            "live 应保留展开前原文：{:?}",
            a.transcript
        );

        // 回放：读落盘的信封。
        let replay = replay_lines(&[persisted]);
        assert!(
            matches!(&replay[..], [TranscriptLine::User(s)] if s == envelope),
            "回放应是展开后的信封：{replay:?}"
        );

        assert_ne!(
            replay, a.transcript,
            "P1-1 分叉 ②：live(原文) ≠ replay(信封)"
        );
    }

    /// **P1-1 分叉 ③**（防御性）：**相邻两条 assistant 文本消息**。
    ///
    /// live 侧 `append_stream` 按「相邻同类合并」并成一条（`Assistant("ab")`），回放按消息
    /// 边界各起一条（`Assistant("a")`、`Assistant("b")`）。**本仓 agent loop 不会产出相邻
    /// assistant 消息** ⇒ 实际不可达；此条为防御性断言：将来若真出现相邻 assistant 消息、或
    /// 任一侧的合并/分条规则变动，都会在此暴露。
    #[test]
    fn p1_1_adjacent_assistant_text_diverges_merged_vs_split() {
        // live：两条连续文本增量合并成一条。
        let mut a = App::new(CodingView::for_test());
        a.apply_event(env(AgentEvent::ModelTextDelta { text: "a".into() }));
        a.apply_event(env(AgentEvent::ModelTextDelta { text: "b".into() }));
        assert!(
            matches!(&a.transcript[..], [TranscriptLine::Assistant(s)] if s == "ab"),
            "live 相邻同类应合并：{:?}",
            a.transcript
        );

        // 回放：两条 assistant 消息各起一条。
        let replay = replay_lines(&[
            assistant_msg(vec![ContentBlock::Text { text: "a".into() }]),
            assistant_msg(vec![ContentBlock::Text { text: "b".into() }]),
        ]);
        assert!(
            matches!(
                &replay[..],
                [TranscriptLine::Assistant(x), TranscriptLine::Assistant(y)] if x == "a" && y == "b"
            ),
            "回放按消息边界分条：{replay:?}"
        );

        assert_ne!(
            replay, a.transcript,
            "P1-1 分叉 ③：live(合并) ≠ replay(分条)"
        );
    }
}
