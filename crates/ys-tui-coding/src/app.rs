//! [`App`] —— UI 交互状态（设计 §4：数据/显示分离）。
//!
//! [`CodingView`] 是 **app 线程推来的快照**；`App` 是 **UI 自己的状态**：
//! transcript、滚动、输入、补全、Working 动画。二者互不越界 ——
//! UI 不读 `Session`，app 不碰 `scroll_offset`。

use std::collections::{HashMap, VecDeque};
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
    /// 本回合是否已经按过一次 Ctrl-C（取消已请求）。
    ///
    /// 用途单一：让 `Ctrl-C` 在 turn 中**第二段**有语义（第一次请求取消，第二次退出）。
    /// 取消是消息（`Boundary::Abort` 只在轮边界 / 工具轮询里被读到），而模型调用期间
    /// 无人读它 ⇒ 只做「取消」时 Ctrl-C 会长时间毫无反应，用户失去杀死本进程的手段。
    ///
    /// 生命周期与 turn 对齐：任何进入 turn 的路径都清零，`finish_turn` 也清零 ——
    /// 否则上一回合按过的 Ctrl-C 会让**下一回合的第一次** Ctrl-C 直接退出。
    pub(crate) abort_requested: bool,
    /// `/thinking` 开关（默认隐藏思考内容）。
    pub thinking_visible: bool,
    pub should_quit: bool,
    pub turn_started_at: Option<Instant>,
    /// Working 动画相位（0..WORKING_DOTS）。
    pub working_dot: u8,
    /// assistant 渲染产物缓存 + 流式节流状态（设计 §2.5）。
    ///
    /// `RefCell`：`draw.rs` 的 `build_text_with` / `total_rows` 走 `&App`，而缓存需要
    /// 可变访问；UI 侧单线程、无重入 ⇒ 单借用、无竞态（不变量 5）。缓存是**纯 memo**，
    /// 清空只损失 CPU、绝不改变输出。
    pub(crate) md_cache: std::cell::RefCell<crate::cache::RenderCache>,
    /// `ToolCallId` → transcript 下标 —— 工具结果据此回填到对应的 Tool 行。
    pub(crate) tool_index: HashMap<ToolCallId, usize>,
    /// 本地回显抑制计数：提交时已本地 push 的那条 `UserMessage`，
    /// 不应被随后的 `AgentEvent::UserMessage` 重复推入。
    pub(crate) suppress_user_echo: usize,
    /// 「本地已渲染、但 `UserMessage` 回声尚未出现」的提交文本（FIFO）。
    ///
    /// 回合进行中提交的消息会被排到**后续回合**：其回声抵达时，上一回合的
    /// `finish_turn` 已把 `suppress_user_echo` 清零。凭**提交文本**认领它，才不会把它
    /// 误判成 steering（再画一条 User 行）或复核回合（误标成 System 行）。
    ///
    /// 之所以按文本而非回合号认领：完工复核回合会占用一个回合号，而
    /// `begin_local_turn` 提交时并不知道它 —— 排队提交的**真实**回合因此整体后移一格，
    /// 推定的回合号会撞上复核回合，凭号认领会把复核提示错当回声吞掉。
    ///
    /// **边界**：`/skill-name` 提交在 app 侧被 `expand_skill_invocation` 改写成
    /// `<skill …>` 信封后才进会话，而这里登记的是**展开前**的原文 —— 该回声文本不等、
    /// 认领不到，会退回「`is_turning == false` ⇒ 误标 `System`」。
    pending_echo_texts: VecDeque<String>,
}

impl App {
    pub fn new(view: CodingView) -> Self {
        // 启动时消费一次视图初值；运行期真值是 `App.thinking_visible` 自身，
        // 不会随 `Outbound::View` 快照覆盖（设计 §5.6）。
        let thinking_visible = view.thinking_visible;
        Self {
            view,
            transcript: Vec::new(),
            scroll_offset: 0,
            follow: true,
            input: InputBuffer::new(),
            completion: None,
            pending_prompt: None,
            is_turning: false,
            abort_requested: false,
            thinking_visible,
            should_quit: false,
            turn_started_at: None,
            working_dot: 0,
            md_cache: std::cell::RefCell::new(crate::cache::RenderCache::default()),
            tool_index: HashMap::new(),
            suppress_user_echo: 0,
            pending_echo_texts: VecDeque::new(),
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
                // ① 既有回显抑制：提交时已本地 push 过那条 User 行，这条事件就是它的
                //    回声 —— 递减计数并跳过，别重复画。
                if self.suppress_user_echo > 0 {
                    self.suppress_user_echo -= 1;
                    return;
                }
                let text = message_text(&message);
                // ② 迟到的回声：回合进行中提交的消息被**排到后续回合**，其回声抵达时
                //    上一回合的 `finish_turn` 已把 `suppress_user_echo` 清零 —— 凭**提交文本**
                //    认出「这条正是我自己刚画过的提交」，消费关联、不重复画那条 User 行。
                //    该回合此刻才真正开始，故点亮 Working（否则本回合会以 `is_turning ==
                //    false` 运行，连它期间提交的下一条也会被误判）。
                //
                //    以 `!is_turning` 为闸：队列提交的回声恒在前一回合 `finish_turn` 复位后抵达
                //    （此刻 `is_turning == false`）；而 `is_turning == true` 时的 `UserMessage`
                //    语义是轮边界 steering 插话，即便文本偶然与某条待回声提交相同，也归 ③ 的用户行。
                if !self.is_turning
                    && let Some(pos) = self.pending_echo_texts.iter().position(|t| t == &text)
                {
                    self.pending_echo_texts.remove(pos);
                    self.is_turning = true;
                    self.abort_requested = false;
                    self.turn_started_at = Some(Instant::now());
                    return;
                }
                // ③④ 未本地回显、也不属于任何待回声提交的 `UserMessage` 有两种来源，
                //     仅以 `is_turning` 的**真假**区分 —— 绝不按内容匹配（不嗅探文案里
                //     是不是复核提示）。契约：
                // - `is_turning == true` ⇒ 回合进行中的轮边界 steering 插话，仍是用户行；
                // - `is_turning == false` ⇒ app 发起的复核回合（主回合已 `RunFinished`，
                //   `finish_turn` 已复位 `is_turning`），渲染成 System 行并点亮 Working ——
                //   复核同样是一个回合，谁也不是谁的特例。
                if self.is_turning {
                    self.transcript.push(TranscriptLine::User(text));
                } else {
                    self.transcript.push(TranscriptLine::System(text));
                    self.is_turning = true;
                    self.abort_requested = false;
                    self.turn_started_at = Some(Instant::now());
                }
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
            AgentEvent::ContextCompacted {
                estimated_tokens_before,
                estimated_tokens_after,
                summarized,
                kept,
            } => {
                // `~` 前缀是有意的：这两个数是启发式**估算**，不是 provider 真实
                // usage，文案不得暗示精确（见 `ys-event` 的 `ContextCompacted` 文档）。
                self.transcript.push(TranscriptLine::System(format!(
                    "context compacted: ~{estimated_tokens_before} → ~{estimated_tokens_after} tokens ({summarized} summarized, {kept} kept)"
                )));
                self.follow = true;
            }
            AgentEvent::ContextCompactionFailed { reason } => {
                // 压缩失败不终止回合（session 未改动，本轮照常继续）——
                // 故这里只推一条 Error 行，**不调** `finish_turn`。
                self.transcript.push(TranscriptLine::Error(format!(
                    "context compaction failed: {reason}"
                )));
                self.follow = true;
            }
            AgentEvent::ResponseTruncated { dropped_tool_calls } => {
                // 非终局事件：有工具调用时循环会**继续**（那批工具被作废、未执行），
                // 故这里只推一行警告、**不调** `finish_turn`。
                let msg = if dropped_tool_calls == 0 {
                    "model response was truncated (output limit reached)".to_string()
                } else {
                    format!(
                        "model response was truncated (output limit reached): \
                         {dropped_tool_calls} tool call(s) were NOT executed"
                    )
                };
                self.transcript.push(TranscriptLine::System(msg));
                self.follow = true;
            }
            _ => {} // AgentEvent 是 #[non_exhaustive]
        }
    }

    /// 本地提交一条输入：回显 + 进入 turn 态。
    ///
    /// 返回被提交的文本（调用方负责包成 `Request::Prompt`）。
    pub fn begin_local_turn(&mut self, text: String) {
        // 仅**回合进行中**的提交需要登记待回声：它被排到后续回合，回声晚于本回合的
        // `finish_turn`（届时 `suppress_user_echo` 已清零）抵达。空闲时的提交就是下一个
        // 回合，回声在其自身 `finish_turn` **之前**到达，由 `suppress_user_echo` 抑制即可
        // —— 不登记，免得留下永不消掉、还会与同回合 steering 撞车的条目。
        if self.is_turning {
            self.pending_echo_texts.push_back(text.clone());
        }
        self.transcript.push(TranscriptLine::User(text));
        // 下一条 UserMessage 事件就是这条 —— 抑制掉，避免出现两遍
        self.suppress_user_echo += 1;
        self.is_turning = true;
        self.abort_requested = false;
        self.turn_started_at = Some(Instant::now());
        self.working_dot = 0;
        self.follow = true;
        self.scroll_offset = 0;
    }

    /// 推进 Working 动画相位。
    pub fn tick_working(&mut self) {
        self.working_dot = (self.working_dot + 1) % WORKING_DOTS;
    }

    /// Working 文案：`Working` + 1..=3 个点（状态行用）。
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
        // 该回合最后一条 assistant 到此定型：标记其缓存槽位「结束」，令下一次取用忽略节流、
        // 做一次完整渲染 —— 否则节流窗口内最后一段增量会永久停留不显示（设计 §2.5 边界 ②）。
        //
        // **只在回合结算调一次**：`finish` 置 `forced`，而 `lines_for` 重算后会清掉它；
        // 每帧调用等于「每条 assistant 每帧整条重解析」，缓存完全失效。
        if let Some(idx) = self
            .transcript
            .iter()
            .rposition(|l| matches!(l, TranscriptLine::Assistant(_)))
        {
            self.md_cache.borrow_mut().finish(idx);
        }
        self.is_turning = false;
        self.abort_requested = false;
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

    /// `App::new` 消费一次视图初值：`CodingView.thinking_visible = true` ⇒ App 为真。
    ///
    /// 变异：`App::new` 里硬编码 `thinking_visible: false` ⇒ 本测试变红。
    #[test]
    fn test_app_new_consumes_view_thinking_initial() {
        let mut view = CodingView::for_test();
        assert!(!view.thinking_visible, "for_test 默认隐藏");
        view.thinking_visible = true;
        let a = App::new(view);
        assert!(a.thinking_visible, "启动初值应进 App");
    }

    /// 运行期 `Outbound::View` 快照**不覆盖** `thinking_visible` —— UI 自身才是真值。
    #[test]
    fn test_view_snapshot_does_not_override_thinking() {
        let mut a = app();
        a.thinking_visible = true;
        let mut snapshot = CodingView::for_test();
        snapshot.thinking_visible = false;
        a.apply_outbound(Outbound::View(snapshot));
        assert!(a.thinking_visible, "快照不得覆盖运行期真值");
    }

    fn env(event: AgentEvent) -> Envelope {
        Envelope::new(Source::agent(), 1, event)
    }

    /// 指定回合号的信封 —— 用于构造跨回合的序列（回声关联凭 `Envelope.turn`）。
    fn env_at(turn: u32, event: AgentEvent) -> Envelope {
        Envelope::new(Source::agent(), turn, event)
    }

    fn run_finished(turn: u32) -> Envelope {
        env_at(
            turn,
            AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 1,
            },
        )
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
                ..Default::default()
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

    /// 复核回合（app 发起）：`is_turning == false` 时到达的非回显 `UserMessage`
    /// 渲染成 `System` 行，并**点亮 Working**（置 `is_turning` + `turn_started_at`）。
    ///
    /// 判别式只读 `is_turning`，绝不按内容匹配 —— 这里用一句普通文案验证。
    ///
    /// 变异：把判别式改成「非回显即 `System`」（恒走 `System` 分支）→ 本测试仍绿，
    /// 但 `test_steering_user_message_is_still_user_line_regardless` 变红。
    #[test]
    fn test_non_echo_user_message_while_idle_is_review_system_line() {
        let mut a = app();
        assert!(!a.is_turning, "前置：不在 turn 态（主回合已 RunFinished）");
        assert!(a.turn_started_at.is_none());

        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("复核：任务真的完成了吗？"),
        }));

        assert_eq!(a.transcript.len(), 1);
        match &a.transcript[0] {
            TranscriptLine::System(s) => assert_eq!(s, "复核：任务真的完成了吗？"),
            other => panic!("expected System, got {other:?}"),
        }
        assert!(a.is_turning, "复核回合同样是一个回合 ⇒ 点亮 Working");
        assert!(a.turn_started_at.is_some(), "复核回合 elapsed 从此刻起算");
        assert!(a.follow, "新内容到达 → 回到贴底");
    }

    /// **回归护栏**：`is_turning == true` 时到达的非回显 `UserMessage` 仍是
    /// **用户行**（轮边界 steering 插话），且 `is_turning` / `turn_started_at`
    /// **不被改动**。
    ///
    /// **合成性说明**：生产侧暂无 `Boundary::Steer` 生产者（steering 通路备而不用），
    /// 故这里手工造态 —— `begin_local_turn` 是**空闲**提交（`is_turning == false`），
    /// 不登记待回声，故这条 `UserMessage` 既非 echo 也非迟到的回声，正落在 steering 的
    /// 判别格（`is_turning == true`）上。reset `suppress_user_echo` 只是把「回显已被
    /// 消耗」这一步跳过，不影响判别。
    ///
    /// 变异：把判别式写成「非回显即 `System`」（恒走 `System` 分支）→ 本测试变红。
    #[test]
    fn test_steering_user_message_is_still_user_line_regardless() {
        let mut a = app();
        a.begin_local_turn("first".into());
        let started_before = a.turn_started_at;
        assert!(a.is_turning);

        // 非回显（此处未消耗回显计数前先投一条）—— 显式造出「非回显」的情形：
        // 重置抑制计数，使这条 UserMessage 不走 echo 分支。
        a.suppress_user_echo = 0;
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("插话：顺便改下这里"),
        }));

        assert_eq!(a.transcript.len(), 2, "User(first) + User(插话)");
        assert!(
            matches!(&a.transcript[1], TranscriptLine::User(s) if s == "插话：顺便改下这里"),
            "steering 插话仍是用户行：{:?}",
            a.transcript
        );
        assert!(a.is_turning, "steering 不改变 is_turning");
        assert_eq!(a.turn_started_at, started_before, "steering 不动 turn_started_at");
    }

    /// 回显路径不受影响：抑制计数未耗尽时投 `UserMessage` ⇒ 仍被跳过（行为与改动前一致），
    /// 且 **不**进入任何渲染分支（`is_turning` / `turn_started_at` 不动）。
    #[test]
    fn test_echo_suppression_path_unaffected_by_discriminant() {
        let mut a = app();
        a.begin_local_turn("你好".into());
        assert_eq!(a.suppress_user_echo, 1);
        let started_before = a.turn_started_at;

        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("你好"),
        }));

        assert_eq!(a.transcript.len(), 1, "事件里那条被抑制，不重复");
        assert_eq!(a.suppress_user_echo, 0);
        assert!(a.is_turning, "回显抑制不改 turn 态");
        assert_eq!(a.turn_started_at, started_before);
    }

    /// **回归护栏（本轮 P0）**：回合进行中提交的消息会排到后续回合，其回声抵达时
    /// 上一回合的 `finish_turn` 已把 `suppress_user_echo` 清零 —— 若不凭回合号认领，
    /// 就会落进「非回显 ⇒ `is_turning == false` ⇒ 复核回合」的分支，把用户自己的话
    /// 误标成 `System`。
    ///
    /// 场景：回合 A 进行中本地提交 "B"（排队）→ A 的 `RunFinished` 抵达 → 回合 B 的
    /// `UserMessage("B")` 抵达。
    ///
    /// 变异：删掉 `apply_event` 里的「迟到的回声」判定（第 ② 条）⇒ 本测试变红
    /// （出现 `System("B")`）。
    #[test]
    fn test_queued_submission_echo_is_not_mislabeled_as_system() {
        let mut a = app();

        // 回合 A：空闲提交 "A"（其回声由 suppress 抑制）
        a.begin_local_turn("A".into());
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("A"),
        }));
        assert_eq!(a.suppress_user_echo, 0, "A 的回声已消耗抑制计数");

        // 回合 A 仍进行中：用户再提交 "B" —— 排队等下一回合
        a.begin_local_turn("B".into());
        assert_eq!(a.suppress_user_echo, 1);

        // 回合 A 收尾（turn 1）
        a.apply_event(run_finished(1));
        assert!(!a.is_turning, "A 的 RunFinished 复位 turn 态");
        assert_eq!(a.suppress_user_echo, 0, "finish_turn 清零抑制计数");

        // 回合 B 开始（turn 2）：其 UserMessage("B") 此刻才到
        a.apply_event(env_at(
            2,
            AgentEvent::UserMessage {
                message: user_message("B"),
            },
        ));

        assert!(
            !a.transcript
                .iter()
                .any(|l| matches!(l, TranscriptLine::System(s) if s == "B")),
            "用户自己的话不得被误标成 System：{:?}",
            a.transcript
        );
        assert_eq!(
            a.transcript
                .iter()
                .filter(|l| matches!(l, TranscriptLine::User(s) if s == "B"))
                .count(),
            1,
            "本地那条 User(B) 已在，回声不得再画一遍：{:?}",
            a.transcript
        );
        assert!(a.is_turning, "回声抵达即回合 B 开始 ⇒ 点亮 Working");
    }

    /// 连排两条：推定回合依次 +1、+2，两条回声都被各自认领 —— 均不重复画、不误标。
    #[test]
    fn test_consecutive_queued_submissions_are_both_claimed() {
        let mut a = app();
        a.begin_local_turn("A".into()); // 空闲提交：推定 turn 1
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("A"),
        }));
        assert_eq!(a.suppress_user_echo, 0);

        // 回合 A 进行中连排两条
        a.begin_local_turn("B".into()); // 推定 turn 2
        a.begin_local_turn("C".into()); // 紧随其后：推定 turn 3
        assert_eq!(a.suppress_user_echo, 2);

        a.apply_event(run_finished(1)); // 回合 A 收尾
        a.apply_event(env_at(
            2,
            AgentEvent::UserMessage {
                message: user_message("B"),
            },
        ));
        a.apply_event(run_finished(2)); // 回合 B 收尾
        a.apply_event(env_at(
            3,
            AgentEvent::UserMessage {
                message: user_message("C"),
            },
        ));

        for text in ["B", "C"] {
            assert_eq!(
                a.transcript
                    .iter()
                    .filter(|l| matches!(l, TranscriptLine::User(s) if s == text))
                    .count(),
                1,
                "{text}：本地一条，回声不再画：{:?}",
                a.transcript
            );
            assert!(
                !a.transcript
                    .iter()
                    .any(|l| matches!(l, TranscriptLine::System(s) if s == text)),
                "{text}：不得被误标成 System：{:?}",
                a.transcript
            );
        }
    }

    /// 回声抵达即视为**该回合开始**（点亮 Working）—— 于是该回合进行中再提交的消息
    /// 仍被正确判为「排队提交」，其回声同样被认领，不误标。
    ///
    /// 变异：删掉「迟到的回声」分支里的 `is_turning = true` ⇒ 本测试变红
    /// （B 回合运行期间再提交的 C 落入复核分支，被误标成 `System("C")`）。
    #[test]
    fn test_queued_turn_stays_turning_so_further_submissions_are_claimed() {
        let mut a = app();
        // A：空闲提交 → turn 1
        a.begin_local_turn("A".into());
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("A"),
        }));
        // B：A 进行中提交 → 排队，推定 turn 2
        a.begin_local_turn("B".into());
        // A 收尾
        a.apply_event(run_finished(1));
        // B 的回声（turn 2）：认领并点亮
        a.apply_event(env_at(
            2,
            AgentEvent::UserMessage {
                message: user_message("B"),
            },
        ));
        assert!(a.is_turning, "B 回合由此开始");
        assert!(
            a.turn_started_at.is_some(),
            "认领回声即点亮 turn_started_at（Working 计时的另一半）"
        );

        // B 进行中再提交 C → 推定 turn 3
        a.begin_local_turn("C".into());
        // B 收尾
        a.apply_event(run_finished(2));
        // C 的回声（turn 3）：应被认领，不误标
        a.apply_event(env_at(
            3,
            AgentEvent::UserMessage {
                message: user_message("C"),
            },
        ));

        assert!(
            !a.transcript
                .iter()
                .any(|l| matches!(l, TranscriptLine::System(s) if s == "C")),
            "C 不得被误标成 System：{:?}",
            a.transcript
        );
        assert_eq!(
            a.transcript
                .iter()
                .filter(|l| matches!(l, TranscriptLine::User(s) if s == "C"))
                .count(),
            1,
            "{:?}",
            a.transcript
        );
    }

    /// transcript 一行的「形状」签名：只区分变体与文本，忽略 `Summary` 的
    /// `elapsed_secs`（它由 `Instant::now()` 起算，不可逐字节 fixture）。
    fn line_shape(line: &TranscriptLine) -> String {
        match line {
            TranscriptLine::User(s) => format!("User({s})"),
            TranscriptLine::System(s) => format!("System({s})"),
            TranscriptLine::Summary { .. } => "Summary".to_string(),
            other => format!("{other:?}"),
        }
    }

    /// **回归护栏（碰撞场景 P1）**：回合进行中提交 "B" 后，该回合**触发完工复核** ——
    /// 复核回合占用的回合号与提交时推定的 "B" 回合号**撞号**，"B" 的真实回合整体后移一格。
    ///
    /// 若凭回合号认领：复核提示（命中推定号）会被当迟到回声**吞掉**，而 "B" 的真实回声
    /// （号已错位、无登记命中）在 `is_turning == false` 下落进复核分支被误标 `System`。
    /// 凭**提交文本**认领即可把二者分开。
    ///
    /// 场景（回合号）：turn 1 = 主回合（运行中提交 "B"）→ app 触发复核 ⇒ turn 2 = 复核回合
    /// → turn 3 = "B" 的真实回合。复核文案用**字面量**代表（本单元不认识 app 侧常量）。
    ///
    /// 变异：删掉分支 ② 的**文本相等**判据（改成「有待回声即消费」）⇒ 复核提示被吞 ⇒
    /// `System(REVIEW)` 断言变红。
    #[test]
    fn test_queued_submission_survives_colliding_review_turn() {
        const REVIEW: &str = "复核：原始需求真的都完成了吗？";
        let mut a = app();

        // turn 1：空闲提交 "A"（其回声由 suppress 抑制）
        a.begin_local_turn("A".into());
        a.apply_event(env_at(
            1,
            AgentEvent::UserMessage {
                message: user_message("A"),
            },
        ));
        assert_eq!(a.suppress_user_echo, 0, "A 的回声已消耗抑制计数");

        // turn 1 仍进行中：提交 "B" —— 排队等后续回合，登记其文本
        a.begin_local_turn("B".into());
        assert_eq!(a.suppress_user_echo, 1);

        // turn 1 收尾（StopReason::Completed + rounds >= 5 ⇒ app 侧触发复核）
        a.apply_event(env_at(
            1,
            AgentEvent::RunFinished {
                stop_reason: StopReason::Completed,
                usage: Usage::default(),
                rounds: 5,
            },
        ));
        assert!(!a.is_turning, "turn 1 收尾复位 turn 态");

        // turn 2：复核回合的 UserMessage（文案与 "B" 不等 ⇒ 不得被当回声认领）
        a.apply_event(env_at(
            2,
            AgentEvent::UserMessage {
                message: user_message(REVIEW),
            },
        ));
        // turn 2 收尾
        a.apply_event(run_finished(2));

        // turn 3："B" 的真实回声（回合号因复核后移一格）
        a.apply_event(env_at(
            3,
            AgentEvent::UserMessage {
                message: user_message("B"),
            },
        ));

        let shapes: Vec<String> = a.transcript.iter().map(line_shape).collect();
        assert_eq!(
            shapes,
            vec![
                "User(A)".to_string(),
                "User(B)".to_string(),
                "Summary".to_string(),
                format!("System({REVIEW})"),
                "Summary".to_string(),
            ],
            "逐行形状：{:?}",
            a.transcript
        );
        // 至少这三条显式钉住验收口径
        assert!(
            a.transcript
                .iter()
                .any(|l| matches!(l, TranscriptLine::System(s) if s == REVIEW)),
            "复核提示必须可见（不得被当迟到回声吞掉）：{:?}",
            a.transcript
        );
        assert!(
            !a.transcript
                .iter()
                .any(|l| matches!(l, TranscriptLine::System(s) if s == "B")),
            "用户自己的话不得被误标成 System：{:?}",
            a.transcript
        );
        assert_eq!(
            a.transcript
                .iter()
                .filter(|l| matches!(l, TranscriptLine::User(s) if s == "B"))
                .count(),
            1,
            "本地那条 User(B) 已在，回声不得再画一遍：{:?}",
            a.transcript
        );
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
                ..Default::default()
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

    /// **回归**：「取消已请求」标记必须随回合生命周期复位。
    ///
    /// 若它跨回合残留，**下一回合的第一次** Ctrl-C 就落在「第二次」分支上 ⇒ 直接退出；
    /// 用户只是想在长回合里取消一次，进程却没了。两个入口都要复位：回合结束（`finish_turn`）
    /// 与回合开始（`begin_local_turn` / 迟到回声点亮 Working 的那两处）。
    #[test]
    fn test_abort_request_resets_with_turn() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.abort_requested = true;
        a.apply_event(run_finished(1));
        assert!(!a.is_turning, "前置：回合已结束");
        assert!(!a.abort_requested, "回合结束必须复位");

        a.abort_requested = true;
        a.begin_local_turn("q2".into());
        assert!(!a.abort_requested, "新回合起始必须复位");
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
    /// 残留的 `1` 会吞掉**下一回合**真正的 `UserMessage`，用户消息凭空消失。
    ///
    /// `RunFailed` 同时复位 `is_turning`（见 `finish_turn`），故其后到达的这条
    /// 非回显 `UserMessage` 落入 `is_turning == false` 分支，渲染为 `System`
    /// （§4.6.1：idle 下的非回显 `UserMessage` = app 发起的回合）。本测试的核心
    /// 不变量是**它不被误吞**（进程里仍出现这条文本），而非它渲染成哪种行。
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

        // 其后到达的 UserMessage（未本地回显）必须正常显示，不被误吞
        a.apply_event(env(AgentEvent::UserMessage {
            message: user_message("next"),
        }));
        assert_eq!(
            a.transcript.len(),
            3,
            "User(first) + Error + System(next)：{:?}",
            a.transcript
        );
        assert!(matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s == "next"));
    }

    /// 压缩成功：推一条 `System` 行，文案带 `~` 估算前缀与四个计数。
    ///
    /// 变异：把 `apply_event` 里的 `ContextCompacted` 臂改回 `_ => {}` → 本测试变红。
    #[test]
    fn test_context_compacted_pushes_system_line() {
        let mut a = app();
        a.apply_event(env(AgentEvent::ContextCompacted {
            estimated_tokens_before: 12_000,
            estimated_tokens_after: 3_500,
            summarized: 20,
            kept: 4,
        }));
        assert_eq!(a.transcript.len(), 1);
        match &a.transcript[0] {
            TranscriptLine::System(s) => {
                assert_eq!(
                    s,
                    "context compacted: ~12000 → ~3500 tokens (20 summarized, 4 kept)"
                );
                assert!(s.contains('~'), "估算值必须带 ~ 前缀，不得暗示精确：{s}");
            }
            other => panic!("expected System, got {other:?}"),
        }
        assert!(a.follow, "新内容到达 → 回到贴底");
    }

    /// 压缩失败：推一条 `Error` 行，且**不结束回合**（session 未改动，本轮继续）。
    ///
    /// 变异：把 `apply_event` 里的 `ContextCompactionFailed` 臂改回 `_ => {}` → 本测试变红。
    #[test]
    fn test_context_compaction_failed_pushes_error_line_and_keeps_turn() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(env(AgentEvent::ContextCompactionFailed {
            reason: "provider 挂了".into(),
        }));

        assert!(
            matches!(
                a.transcript.last(),
                Some(TranscriptLine::Error(e)) if e == "context compaction failed: provider 挂了"
            ),
            "transcript={:?}",
            a.transcript
        );
        assert!(a.is_turning, "压缩失败不终止回合");
        assert!(a.turn_started_at.is_some());
    }

    /// 截断且有工具调用被作废：推一条 `System` 警告行，且**不结束回合**
    /// （非终局事件 —— 循环会带着「工具未执行」这一事实继续下一轮）。
    ///
    /// 变异：① 把 `apply_event` 里的 `ResponseTruncated` 臂改回 `_ => {}` →
    /// 前两条断言变红；② 在臂里补上 `finish_turn()` → `is_turning` 断言变红。
    #[test]
    fn test_response_truncated_with_dropped_tool_calls_keeps_turn() {
        let mut a = app();
        a.begin_local_turn("q".into());
        assert!(a.is_turning, "前置：回合进行中");

        a.apply_event(env(AgentEvent::ResponseTruncated {
            dropped_tool_calls: 2,
        }));

        match a.transcript.last() {
            Some(TranscriptLine::System(s)) => {
                assert!(s.contains("2 tool call(s)"), "要报出被作废的工具数：{s}");
                assert!(s.contains("NOT executed"), "要明说那批工具没执行：{s}");
            }
            other => panic!("expected System, got {other:?}"),
        }
        // **非终局**：回合可能还在继续，不得复位 turn 态
        assert!(a.is_turning, "ResponseTruncated 不得调 finish_turn");
        assert!(a.turn_started_at.is_some());
    }

    /// 纯文本被截断（0 个工具调用）：文案同样警告，但**不**提工具调用。
    ///
    /// 变异：把 `dropped_tool_calls == 0` 的分支去掉（统一走带工具数的文案）→
    /// `!s.contains("tool call(s)")` 断言变红。
    #[test]
    fn test_response_truncated_without_tool_calls_omits_tool_wording() {
        let mut a = app();
        a.apply_event(env(AgentEvent::ResponseTruncated {
            dropped_tool_calls: 0,
        }));

        assert_eq!(a.transcript.len(), 1);
        match &a.transcript[0] {
            TranscriptLine::System(s) => {
                assert!(s.contains("truncated"), "要报出截断这一事实：{s}");
                assert!(!s.contains("tool call(s)"), "纯文本截断不该提工具调用：{s}");
            }
            other => panic!("expected System, got {other:?}"),
        }
        assert!(a.follow, "新内容到达 → 回到贴底");
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
