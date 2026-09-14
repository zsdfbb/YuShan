//! [`App`] —— UI 交互状态（设计 §4：数据/显示分离）。
//!
//! [`CodingView`] 是 **app 线程推来的快照**；`App` 是 **UI 自己的状态**：
//! transcript、滚动、输入、补全、Working 动画。二者互不越界 ——
//! UI 不读 `Session`，app 不碰 `scroll_offset`。

use std::collections::HashMap;
use std::time::Instant;

use ys_core::{ContentBlock, Message, ToolCallId};
use ys_event::AgentEvent;
use ys_protocol::{Envelope, Outbound};

use crate::commands::PromptKind;
use crate::completion::CompletionState;
use crate::input::InputBuffer;
use crate::transcript::{TranscriptLine, summarize_tool_args};
use crate::view::CodingView;

/// Working 动画的点数（1..=3）。
const WORKING_DOTS: u8 = 3;

pub struct App {
    pub view: CodingView,
    pub transcript: Vec<TranscriptLine>,
    /// 距底部向上滚动的**视觉行数**（0 = 贴底）。
    pub scroll_offset: usize,
    /// 贴底跟随（新内容到达时自动滚到底）。
    pub follow: bool,
    pub input: InputBuffer,
    pub completion: Option<CompletionState>,
    /// 命令要开模态浮层（`/login` / `/model` 无参）—— **由 `run.rs` 消费**。
    ///
    /// `events.rs` 只认识键与缓冲，画不了屏；需要终端的部分一律经这个字段
    /// 交还给持有 `Terminal` 的事件循环（设计 §3「模态循环在 run.rs 里跑」）。
    pub pending_prompt: Option<PromptKind>,
    pub is_turning: bool,
    /// `/thinking` 开关（默认隐藏思考内容）。
    pub thinking_visible: bool,
    pub should_quit: bool,
    pub turn_started_at: Option<Instant>,
    /// Working 动画相位（0..WORKING_DOTS）。
    pub working_dot: u8,
    /// `ToolCallId` → transcript 下标 —— 工具结果据此回填到对应的 Tool 行。
    pub(crate) tool_index: HashMap<ToolCallId, usize>,
    /// 本地回显抑制计数：提交时已本地 push 的那条 `UserMessage`，
    /// 不应被随后的 `AgentEvent::UserMessage` 重复推入。
    pub(crate) suppress_user_echo: usize,
}

impl App {
    pub fn new(view: CodingView) -> Self {
        Self {
            view,
            transcript: Vec::new(),
            scroll_offset: 0,
            follow: true,
            input: InputBuffer::new(),
            completion: None,
            pending_prompt: None,
            is_turning: false,
            thinking_visible: false,
            should_quit: false,
            turn_started_at: None,
            working_dot: 0,
            tool_index: HashMap::new(),
            suppress_user_echo: 0,
        }
    }

    /// 处理一条 app → UI 的出站消息。
    pub fn apply_outbound(&mut self, out: Outbound<CodingView>) {
        match out {
            Outbound::Event(env) => self.apply_event(env),
            Outbound::View(view) => self.view = view,
            Outbound::Output(text) => {
                // 命令期间的输出（如 `/login` 的 "✓ Logged in"）进 transcript（设计 §5）
                self.transcript.push(TranscriptLine::System(text));
                self.follow = true;
            }
            Outbound::Quit => self.should_quit = true,
        }
    }

    /// 把一个 agent 事件应用到 transcript / 状态。
    pub fn apply_event(&mut self, env: Envelope) {
        match env.event {
            AgentEvent::UserMessage { message } => {
                if self.suppress_user_echo > 0 {
                    // 本地已回显（提交时 push 过了）—— 跳过，别重复
                    self.suppress_user_echo -= 1;
                    return;
                }
                // 未本地回显的 UserMessage = 轮边界的 steering 插话
                self.transcript
                    .push(TranscriptLine::User(message_text(&message)));
                self.follow = true;
            }
            AgentEvent::ModelTextDelta { text } => self.append_stream(text, StreamKind::Assistant),
            AgentEvent::ModelThinkingDelta { text } => {
                self.append_stream(text, StreamKind::Thinking)
            }
            AgentEvent::ToolCall { call } => {
                let idx = self.transcript.len();
                self.transcript.push(TranscriptLine::Tool {
                    id: call.id.clone(),
                    summary: summarize_tool_args(&call.name, &call.arguments),
                    name: call.name,
                    result: None,
                    success: true,
                });
                // **已知取舍**：同一 `ToolCallId` 重复出现（模型重放同一 call）时，
                // 后写覆盖先写 —— 前面那条 Tool 行将永远拿不到结果。
                // v1 不处理重放（正常回合内 id 唯一）；若要支持，应改为保留首条
                // 或把 id 映射成多条下标。当前行为由
                // `test_duplicate_tool_call_id_overwrites_mapping` 钉住。
                self.tool_index.insert(call.id, idx);
                self.follow = true;
            }
            AgentEvent::ToolResult { id, result } => {
                if let Some(&idx) = self.tool_index.get(&id)
                    && let Some(TranscriptLine::Tool {
                        result: slot,
                        success,
                        ..
                    }) = self.transcript.get_mut(idx)
                {
                    *slot = Some(result.content);
                    *success = !result.is_error;
                }
                // 找不到对应 Tool 行：静默忽略（事件与 transcript 不同源也无所谓）
            }
            AgentEvent::RunFinished {
                stop_reason,
                rounds,
                ..
            } => {
                self.transcript.push(TranscriptLine::Summary {
                    rounds,
                    stop: stop_reason,
                    elapsed_secs: self.turn_elapsed_secs(),
                });
                self.finish_turn();
            }
            AgentEvent::RunFailed { error } => {
                self.transcript.push(TranscriptLine::Error(error));
                self.finish_turn();
            }
            _ => {} // AgentEvent 是 #[non_exhaustive]
        }
    }

    /// 本地提交一条输入：回显 + 进入 turn 态。
    ///
    /// 返回被提交的文本（调用方负责包成 `Request::Prompt`）。
    pub fn begin_local_turn(&mut self, text: String) {
        self.transcript.push(TranscriptLine::User(text));
        // 下一条 UserMessage 事件就是这条 —— 抑制掉，避免出现两遍
        self.suppress_user_echo += 1;
        self.is_turning = true;
        self.turn_started_at = Some(Instant::now());
        self.working_dot = 0;
        self.follow = true;
        self.scroll_offset = 0;
    }

    /// 推进 Working 动画相位。
    pub fn tick_working(&mut self) {
        self.working_dot = (self.working_dot + 1) % WORKING_DOTS;
    }

    /// Working 文案：`Working` + 1..=3 个点（与状态行共用）。
    pub fn working_text(&self) -> String {
        format!("Working{}", ".".repeat(1 + self.working_dot as usize))
    }

    /// 滚动上限（视觉行数）—— 委托 `draw`（那里才知道 wrap 规则）。
    pub fn compute_max_scroll(&self, width: u16, height: u16) -> usize {
        crate::draw::max_scroll_offset(self, width, height)
    }

    fn turn_elapsed_secs(&self) -> f32 {
        self.turn_started_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// 回合结束后的共同复位。
    ///
    /// **清空 `tool_index`**：回合已结束，不会再有待回填的 `ToolResult` ——
    /// 同一回合内 `ToolResult` 一定先于 `RunFinished` / `RunFailed` 到达
    /// （循环按「工具执行 → 汇总结果 → 收尾」的顺序发事件）。留着它只会跨回合
    /// 单调增长（每回合若干 `ToolCallId` + `String`）。
    fn finish_turn(&mut self) {
        self.is_turning = false;
        self.turn_started_at = None;
        self.suppress_user_echo = 0;
        self.tool_index.clear();
        self.follow = true;
    }

    /// 流式增量：末尾同为该类型则追加，否则新起一条。
    fn append_stream(&mut self, text: String, kind: StreamKind) {
        let same_kind = matches!(
            (self.transcript.last(), kind),
            (Some(TranscriptLine::Assistant(_)), StreamKind::Assistant)
                | (Some(TranscriptLine::Thinking(_)), StreamKind::Thinking)
        );
        if same_kind {
            match self.transcript.last_mut() {
                Some(TranscriptLine::Assistant(buf)) | Some(TranscriptLine::Thinking(buf)) => {
                    buf.push_str(&text)
                }
                _ => unreachable!("same_kind 已断言末尾类型"),
            }
        } else {
            self.transcript.push(match kind {
                StreamKind::Assistant => TranscriptLine::Assistant(text),
                StreamKind::Thinking => TranscriptLine::Thinking(text),
            });
        }
        self.follow = true;
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Assistant,
    Thinking,
}

/// 从 `Message` 里抽出可显示的文本（多个 Text 块用换行拼接）。
///
/// `pub(crate)`：`events.rs` 提交普通消息时用它回显 —— 回显文本与真正发出去的
/// 消息必须**同源**，否则 trim / 多块拼接上的差异会让两边对不上。
pub(crate) fn message_text(message: &Message) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for block in &message.content {
        if let ContentBlock::Text { text } = block {
            parts.push(text);
        }
    }
    parts.join("\n")
}

/// 测试辅助：构造一条用户文本消息。
#[cfg(test)]
pub(crate) fn user_message(text: &str) -> Message {
    Message {
        role: ys_core::Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::{Role, StopReason, ToolCall, ToolResult, Usage};
    use ys_protocol::Source;

    fn app() -> App {
        App::new(CodingView::for_test())
    }

    fn env(event: AgentEvent) -> Envelope {
        Envelope::new(Source::agent(), 1, event)
    }

    fn text_delta(text: &str) -> Envelope {
        env(AgentEvent::ModelTextDelta { text: text.into() })
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Envelope {
        env(AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId(id.into()),
                name: name.into(),
                arguments: args,
            },
        })
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> Envelope {
        env(AgentEvent::ToolResult {
            id: ToolCallId(id.into()),
            result: ToolResult {
                content: content.into(),
                is_error,
            },
        })
    }

    #[test]
    fn test_text_deltas_append_into_one_assistant_line() {
        let mut a = app();
        for t in ["Hel", "lo ", "world"] {
            a.apply_event(text_delta(t));
        }
        assert_eq!(a.transcript.len(), 1, "多个 delta 合成一条");
        match &a.transcript[0] {
            TranscriptLine::Assistant(s) => assert_eq!(s, "Hello world"),
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn test_thinking_deltas_append_into_one_thinking_line() {
        let mut a = app();
        a.apply_event(env(AgentEvent::ModelThinkingDelta { text: "嗯".into() }));
        a.apply_event(env(AgentEvent::ModelThinkingDelta { text: "…".into() }));
        assert_eq!(a.transcript.len(), 1);
        match &a.transcript[0] {
            TranscriptLine::Thinking(s) => assert_eq!(s, "嗯…"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    /// assistant 与 thinking 交替时必须**分段**，不能串到一条里。
    #[test]
    fn test_assistant_and_thinking_do_not_merge() {
        let mut a = app();
        a.apply_event(text_delta("answer"));
        a.apply_event(env(AgentEvent::ModelThinkingDelta { text: "hmm".into() }));
        a.apply_event(text_delta("more"));
        assert_eq!(a.transcript.len(), 3);
        assert!(matches!(&a.transcript[0], TranscriptLine::Assistant(s) if s == "answer"));
        assert!(matches!(&a.transcript[1], TranscriptLine::Thinking(s) if s == "hmm"));
        assert!(matches!(&a.transcript[2], TranscriptLine::Assistant(s) if s == "more"));
    }

    #[test]
    fn test_tool_call_then_result_backfills_by_id() {
        let mut a = app();
        a.apply_event(tool_call(
            "c1",
            "read",
            serde_json::json!({"path": "src/x.rs"}),
        ));
        a.apply_event(tool_call(
            "c2",
            "bash",
            serde_json::json!({"command": "ls"}),
        ));
        a.apply_event(tool_result("c1", "file body", false));

        assert_eq!(a.transcript.len(), 2, "两条 Tool 行");
        match &a.transcript[0] {
            TranscriptLine::Tool {
                id,
                name,
                summary,
                result,
                success,
            } => {
                assert_eq!(id, &ToolCallId("c1".into()));
                assert_eq!(name, "read");
                assert_eq!(summary, "src/x.rs");
                assert_eq!(result.as_deref(), Some("file body"));
                assert!(*success);
            }
            other => panic!("expected Tool, got {other:?}"),
        }
        // 未回填的那条保持 None
        match &a.transcript[1] {
            TranscriptLine::Tool { result, .. } => assert!(result.is_none()),
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    /// 乱序回填（结果先于/晚于其它工具）不得串行 —— 靠 `tool_index`，不靠「末尾」。
    #[test]
    fn test_tool_result_backfills_correct_index_regardless_of_order() {
        let mut a = app();
        a.apply_event(tool_call("a", "read", serde_json::json!({"path": "A"})));
        a.apply_event(tool_call("b", "read", serde_json::json!({"path": "B"})));
        a.apply_event(tool_result("b", "B-body", false));
        a.apply_event(tool_result("a", "A-body", true));

        let a_path = match &a.transcript[0] {
            TranscriptLine::Tool {
                summary,
                result,
                success,
                ..
            } => (summary.clone(), result.clone(), *success),
            other => panic!("expected Tool, got {other:?}"),
        };
        let b_path = match &a.transcript[1] {
            TranscriptLine::Tool {
                summary,
                result,
                success,
                ..
            } => (summary.clone(), result.clone(), *success),
            other => panic!("expected Tool, got {other:?}"),
        };
        assert_eq!(a_path, ("A".into(), Some("A-body".into()), false));
        assert_eq!(b_path, ("B".into(), Some("B-body".into()), true));
    }

    #[test]
    fn test_tool_result_for_unknown_id_is_ignored() {
        let mut a = app();
        a.apply_event(tool_result("nope", "body", false));
        assert!(a.transcript.is_empty(), "无处回填 → 静默忽略");
    }

    /// 同一 `ToolCallId` 重复（模型重放）—— **钉住已知取舍**：映射被后一条覆盖，
    /// 首条 Tool 行永远拿不到结果，`ToolResult` 回填到**第二条**。
    ///
    /// 若将来改成「保留首条」，本测试必须同步反转（并更新 `apply_event` 的注释）。
    #[test]
    fn test_duplicate_tool_call_id_overwrites_mapping() {
        let mut a = app();
        a.apply_event(tool_call(
            "dup",
            "read",
            serde_json::json!({"path": "first"}),
        ));
        a.apply_event(tool_call(
            "dup",
            "read",
            serde_json::json!({"path": "second"}),
        ));
        assert_eq!(a.transcript.len(), 2, "两条 Tool 行都在");
        assert_eq!(
            a.tool_index.get(&ToolCallId("dup".into())),
            Some(&1),
            "映射指向后出现的那条"
        );

        a.apply_event(tool_result("dup", "body", false));
        match &a.transcript[0] {
            TranscriptLine::Tool { result, .. } => {
                assert!(result.is_none(), "首条被覆盖后拿不到结果")
            }
            other => panic!("expected Tool, got {other:?}"),
        }
        match &a.transcript[1] {
            TranscriptLine::Tool {
                summary,
                result,
                success,
                ..
            } => {
                assert_eq!(summary, "second", "回填的是第二条");
                assert_eq!(result.as_deref(), Some("body"));
                assert!(*success, "is_error=false → success");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    /// 回合结束必须清空 `tool_index`（否则跨回合单调增长）。
    #[test]
    fn test_tool_index_cleared_at_turn_end() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(tool_call("c1", "read", serde_json::json!({"path": "x"})));
        a.apply_event(tool_result("c1", "body", false));
        assert_eq!(a.tool_index.len(), 1);
        a.apply_event(env(AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
            rounds: 1,
        }));
        assert!(a.tool_index.is_empty(), "RunFinished 清空 tool_index");
        // Tool 行本身保留，回填已生效
        assert!(matches!(
            a.transcript
                .iter()
                .find(|l| matches!(l, TranscriptLine::Tool { .. })),
            Some(TranscriptLine::Tool {
                result: Some(_),
                ..
            })
        ));

        // 失败路径同样清空
        a.begin_local_turn("q2".into());
        a.apply_event(tool_call("c2", "read", serde_json::json!({"path": "y"})));
        a.apply_event(env(AgentEvent::RunFailed {
            error: "boom".into(),
        }));
        assert!(a.tool_index.is_empty(), "RunFailed 也清空");
    }

    /// 清空之后迟到的（跨回合）`ToolResult` 只被静默忽略：不 panic、不写错行。
    #[test]
    fn test_late_tool_result_after_turn_end_is_ignored() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(tool_call("c1", "read", serde_json::json!({"path": "x"})));
        a.apply_event(env(AgentEvent::RunFailed {
            error: "boom".into(),
        }));
        a.apply_event(tool_result("c1", "late", false));
        assert!(
            matches!(
                a.transcript
                    .iter()
                    .find(|l| matches!(l, TranscriptLine::Tool { .. })),
                Some(TranscriptLine::Tool { result: None, .. })
            ),
            "回合结束后的迟到结果不回填：{:?}",
            a.transcript
        );
    }

    /// 本地回显抑制：提交后进来的那条 `UserMessage` 不得再 push 一遍。
    #[test]
    fn test_suppress_user_echo_skips_exactly_one() {
        let mut a = app();
        a.begin_local_turn("你好".into());
        assert_eq!(a.suppress_user_echo, 1);

        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("你好"),
        }));
        assert_eq!(a.transcript.len(), 1, "事件里那条被抑制，不重复");
        assert_eq!(a.suppress_user_echo, 0);
        assert!(matches!(&a.transcript[0], TranscriptLine::User(s) if s == "你好"));
    }

    /// 抑制用尽后的 `UserMessage`（steering 插话）必须**正常显示**。
    #[test]
    fn test_steering_user_message_after_echo_is_shown() {
        let mut a = app();
        a.begin_local_turn("first".into());
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("first"),
        }));
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("插话"),
        }));
        assert_eq!(a.transcript.len(), 2);
        assert!(matches!(&a.transcript[1], TranscriptLine::User(s) if s == "插话"));
    }

    #[test]
    fn test_run_finished_pushes_summary_and_resets_turn() {
        let mut a = app();
        a.begin_local_turn("q".into());
        assert!(a.is_turning);
        a.apply_event(env(AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
            },
            rounds: 3,
        }));
        assert!(!a.is_turning, "RunFinished 必须复位 is_turning");
        assert!(a.turn_started_at.is_none());
        assert_eq!(a.suppress_user_echo, 0, "抑制计数随回合结束清空");
        match a.transcript.last() {
            Some(TranscriptLine::Summary { rounds, stop, .. }) => {
                assert_eq!(*rounds, 3);
                assert_eq!(*stop, StopReason::Completed);
            }
            other => panic!("expected Summary, got {other:?}"),
        }
    }

    #[test]
    fn test_run_failed_pushes_error_and_resets_turn() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(env(AgentEvent::RunFailed {
            error: "boom".into(),
        }));
        assert!(!a.is_turning);
        assert!(a.turn_started_at.is_none());
        assert_eq!(a.suppress_user_echo, 0);
        assert!(matches!(a.transcript.last(), Some(TranscriptLine::Error(e)) if e == "boom"));
    }

    /// **行为级**断言（review 点名的边界）：`RunFailed` 必须复位抑制计数 ——
    /// 失败回合里那条 `UserMessage` 事件从未到达（错误发生在更早），若不复位，
    /// 残留的 `1` 会吞掉**下一回合**真正的 `UserMessage`（如轮边界 steering 插话），
    /// 用户消息凭空消失。
    ///
    /// 变异：去掉 `finish_turn` 里的 `suppress_user_echo = 0` → 本测试变红。
    #[test]
    fn test_run_failed_resets_suppress_so_next_user_message_is_shown() {
        let mut a = app();
        a.begin_local_turn("first".into());
        assert_eq!(a.suppress_user_echo, 1, "提交后待抑制一条回显");

        // 失败：没有 UserMessage 事件来消耗这个计数
        a.apply_event(env(AgentEvent::RunFailed {
            error: "boom".into(),
        }));
        assert_eq!(a.suppress_user_echo, 0, "RunFailed 必须复位计数");

        // 下一回合的 UserMessage（未本地回显）必须正常显示，不被误吞
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("next"),
        }));
        assert_eq!(
            a.transcript.len(),
            3,
            "User(first) + Error + User(next)：{:?}",
            a.transcript
        );
        assert!(matches!(a.transcript.last(), Some(TranscriptLine::User(s)) if s == "next"));
    }

    #[test]
    fn test_apply_outbound_view_and_output_and_quit() {
        let mut a = app();
        let mut v = CodingView::for_test();
        v.model = Some("new-model".into());
        a.apply_outbound(Outbound::View(v));
        assert_eq!(a.view.model.as_deref(), Some("new-model"));

        a.apply_outbound(Outbound::Output("✓ Logged in".into()));
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("Logged in"))
        );

        a.apply_outbound(Outbound::Quit);
        assert!(a.should_quit);
    }

    #[test]
    fn test_begin_local_turn_sets_follow_and_scroll() {
        let mut a = app();
        a.follow = false;
        a.scroll_offset = 7;
        a.begin_local_turn("hi".into());
        assert!(a.follow, "提交后回到贴底");
        assert_eq!(a.scroll_offset, 0);
        assert!(a.is_turning);
        assert_eq!(a.working_dot, 0);
    }

    #[test]
    fn test_working_text_cycles() {
        let mut a = app();
        a.is_turning = true;
        assert_eq!(a.working_text(), "Working.");
        a.tick_working();
        assert_eq!(a.working_text(), "Working..");
        a.tick_working();
        assert_eq!(a.working_text(), "Working...");
        a.tick_working();
        assert_eq!(a.working_text(), "Working.", "相位循环回 1 个点");
    }

    #[test]
    fn test_message_text_joins_text_blocks() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::ToolUse {
                    id: ToolCallId("t".into()),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
                ContentBlock::Text { text: "b".into() },
            ],
        };
        assert_eq!(message_text(&msg), "a\nb", "只取 Text 块，非 Text 块跳过");
    }
}
