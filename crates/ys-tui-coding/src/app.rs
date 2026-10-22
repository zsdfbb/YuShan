//! [`App`] —— UI 交互状态（设计 §4：数据/显示分离）。
//!
//! [`CodingView`] 是 **app 线程推来的快照**；`App` 是 **UI 自己的状态**：
//! transcript、滚动、输入、补全、Working 动画。二者互不越界 ——
//! UI 不读 `Session`，app 不碰 `scroll_offset`。

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use ys_core::{ContentBlock, Message, StopReason, ToolCallId};
use ys_event::AgentEvent;
use ys_protocol::{Envelope, Outbound, Request, SessionSummary};

use crate::commands::PromptKind;
use crate::completion::CompletionState;
use crate::input::InputBuffer;
use crate::transcript::{TranscriptLine, replay_lines, summarize_tool_args};
use crate::view::CodingView;

/// Working 动画的点数（1..=3）。
const WORKING_DOTS: u8 = 3;

pub struct App {
    pub view: CodingView,
    pub transcript: Vec<TranscriptLine>,
    /// 距底部向上滚动的**视觉行数**（0 = 贴底）。
    pub scroll_offset: usize,
    /// 贴底跟随（`scroll_offset` 恒为 0）。
    ///
    /// **新内容到达不再强制置真** —— 用户往上翻后，视窗原地不动（见
    /// [`App::sync_scroll_state`] 的增量补偿），只有「跳到底部」这类显式动作才回到跟随。
    pub follow: bool,
    /// 脱离底部期间**下方又来了新内容** —— 状态行据此提示「↓ 有新内容」。
    ///
    /// 只由「内容追加到尾部」置真（流式增量、工具行、回合结算行…）；
    /// 原地改写（工具结果回填）不置真 —— 那不是「下方的新内容」。
    pub new_content_below: bool,
    /// 上一次同步布局时的总视觉行数 —— 增量补偿的基准（见 [`App::sync_scroll_state`]）。
    ///
    /// `None` = **还没同步过**：首次同步不补偿（此前没有「上一帧的行号空间」可对）。
    last_total_rows: Option<usize>,
    pub input: InputBuffer,
    pub completion: Option<CompletionState>,
    /// 命令要开模态浮层（`/login` / `/model` 无参）—— **由 `run.rs` 消费**。
    ///
    /// `events.rs` 只认识键与缓冲，画不了屏；需要终端的部分一律经这个字段
    /// 交还给持有 `Terminal` 的事件循环（设计 §3「模态循环在 run.rs 里跑」）。
    pub pending_prompt: Option<PromptKind>,
    /// `/resume` 的候选会话（`Outbound::SessionList` 送来）。
    ///
    /// 与 `pending_prompt` **同 take 同清**：`run.rs` 在开 `/resume` 浮层时把它
    /// `take()` 走，浮层结束（选中 / Esc）后即空，不会残留旧列表被下一次浮层读到。
    pub pending_sessions: Option<Vec<SessionSummary>>,
    /// 命令类请求**在飞**：已投给 app 线程、尚未收到任何出站消息。
    ///
    /// 只有 [`Request::Prompt`] 以外的请求会置位，而它们**不经过 `BasicLoop`** ——
    /// 没有轮边界去读 `Boundary`，故命令里一旦有网络往返（`/login` 拉 `/v1/models`
    /// 最长 5s，TCP 挂起时可更久），界面上按 Esc 就毫无反应。置位期间 Esc 投
    /// `Boundary::Abort`，由 `app_loop` 在命令内部 `select!` 它（`cancel_on_abort`）。
    ///
    /// 清位口径：**任意一条出站消息**。`app_loop` 处理完每个请求都会刷一条
    /// `Outbound::View`，故不会漏清；反过来若某个请求什么都不发（当前不存在），
    /// 最坏也只是多投一条 `Abort` —— 它会被下一回合起点的排空丢弃，无副作用。
    pub(crate) command_in_flight: bool,
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
    /// 本回合**已开始的最新轮号**（`RoundStarted` 每轮推一次）。
    ///
    /// `None` = 本回合还没有任何一轮开始。生命周期与 turn 对齐（进入回合与
    /// `finish_turn` 都清零）—— 否则上一回合的轮号会漏进下一回合的状态行。
    ///
    /// 它存在的唯一理由：一个回合可以跑几十轮、每轮数秒，而 `rounds` 此前只在
    /// **回合结束**的摘要里出现 ⇒ 回合进行中界面只能显示一个不动的动画。
    pub current_round: Option<u32>,
    /// assistant 渲染产物缓存 + 流式节流状态（设计 §2.5）。
    ///
    /// `RefCell`：`layout.rs` 的条目渲染走 `&App`，而缓存需要可变访问；
    /// UI 侧单线程、无重入 ⇒ 单借用、无竞态（不变量 5）。缓存是**纯 memo**，
    /// 清空只损失 CPU、绝不改变输出。
    pub(crate) md_cache: std::cell::RefCell<crate::cache::RenderCache>,
    /// transcript 的**视窗化布局**记忆（逐条目记折行行数）—— 见 [`crate::layout`]。
    ///
    /// `RefCell`：`layout` 走 `&App`（帧路径与事件路径共用），内部需要可变访问。
    pub(crate) layout: std::cell::RefCell<crate::layout::TranscriptLayout>,
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
            new_content_below: false,
            last_total_rows: None,
            input: InputBuffer::new(),
            completion: None,
            pending_prompt: None,
            pending_sessions: None,
            command_in_flight: false,
            is_turning: false,
            abort_requested: false,
            thinking_visible,
            should_quit: false,
            turn_started_at: None,
            working_dot: 0,
            current_round: None,
            md_cache: std::cell::RefCell::new(crate::cache::RenderCache::default()),
            layout: std::cell::RefCell::new(crate::layout::TranscriptLayout::default()),
            tool_index: HashMap::new(),
            suppress_user_echo: 0,
            pending_echo_texts: VecDeque::new(),
        }
    }

    /// 处理一条 app → UI 的出站消息。
    pub fn apply_outbound(&mut self, out: Outbound<CodingView>) {
        // 任何出站消息都意味着「在飞的那条请求已经收场」（见 `command_in_flight`）。
        self.command_in_flight = false;
        match out {
            Outbound::Event(env) => self.apply_event(env),
            Outbound::View(view) => self.view = view,
            Outbound::Output(text) => {
                // 命令期间的输出（如 `/login` 的 "✓ Logged in"）进 transcript（设计 §5）
                self.transcript.push(TranscriptLine::System(text));
                // 内容追加在**尾部** ⇒ 脱离底部时视窗原地不动（只置提示位）。
                self.note_appended_below();
            }
            Outbound::Quit => self.should_quit = true,
            // 会话列表（`/resume` 无参的响应）。**空列表不弹空浮层**，只提示（I4）——
            // 与启动期 `resolve_resume` 的空列表分支对齐。
            Outbound::SessionList(items) => {
                if items.is_empty() {
                    // 空列表连**既有候选**一并清掉：同一轮 drain 内可能先到非空、再到空
                    // 的一条（例如两次 `/resume` 的响应交错抵达），若只推提示而不清字段，
                    // 旧的候选会残留在 `pending_sessions` / `pending_prompt` 上，主循环
                    // 随后的检查点就会弹出**过期**列表。
                    self.pending_sessions = None;
                    self.pending_prompt = None;
                    self.transcript
                        .push(TranscriptLine::System("没有可恢复的会话".into()));
                    self.follow = true;
                } else {
                    // 由 `pending_prompt` 驱动浮层；候选数据挂在 `pending_sessions`。
                    self.pending_sessions = Some(items);
                    self.pending_prompt = Some(PromptKind::PickSession);
                }
            }
            // 会话已恢复：清屏 + 回放（设计 §3.3 第 6 步）。
            Outbound::SessionResumed {
                summary: _,
                thinking_visible,
                history,
            } => {
                self.reset_for_replay();
                self.transcript = replay_lines(&history);
                // **对「UI 运行期真值」的一次有意破例**：`/resume` **不是** `/new`，
                // 其语义是「打开该会话」，故 thinking 跟随目标会话的 meta（与启动期
                // `--resume` 同源），而非保留旧值。三方真值对齐见设计 I9 —— app 侧
                // 另有进程级镜像，不在这里。
                self.thinking_visible = thinking_visible;
                self.follow = true;
            }
        }
    }

    /// 回放（`/resume`）前的**清屏**：一次性重置全部与 transcript 同源的状态。
    ///
    /// 缺一项都会留下跨会话的脏状态。**不在此重置 `thinking_visible`** —— 它由
    /// [`App::apply_outbound`] 的 `SessionResumed` 臂单独按目标会话的 meta 设定
    /// （语义不同：这里是清屏，那里是「跟随目标会话」）。
    ///
    /// **也不清回合态**（`is_turning` / `turn_started_at` / `working_dot`）：清屏发生在
    /// [`App::apply_outbound`] 收到 `SessionResumed` 时，**依赖该事件恒在回合边界抵达**
    /// （app 线程在 `run_turn` 中阻塞、请求只在回合边界被处理），故此刻本无进行中的回合，
    /// 回合态本就是空闲值。**若该前提将来变化**（设计 §7 已把「回合中拿到列表」列为遗留），
    /// 这里必须重审 —— 否则旧回合的 `is_turning` / `turn_started_at` 会泄漏进回放后的新会话。
    fn reset_for_replay(&mut self) {
        // transcript 整体替换，任何按「槽下标」记忆的渲染产物都已失效（cache.rs 的
        // 等长碰撞陷阱 ⇒ 必须显式 reset，不能指望 `sync_len` 兜住）。
        self.transcript.clear();
        self.md_cache.borrow_mut().reset();
        // 布局记忆（逐条目折行行数）同样作废 —— transcript 整体被替换。
        self.layout.borrow_mut().reset();
        self.last_total_rows = None;
        self.tool_index.clear();
        self.suppress_user_echo = 0;
        self.pending_echo_texts.clear();
        self.follow = true;
        self.scroll_offset = 0;
    }

    /// 记一条「已投出的请求」：`Prompt` 之外的请求进入**在飞态**
    /// （见 [`App::command_in_flight`]）。
    ///
    /// 投放方有两处：主循环的 `send_request`（`/model`、`/resume` 等直发命令）与
    /// 模态浮层的 `apply_prompt_outcome`（`/login`、`/model` 无参选完之后的请求）。
    /// 两条路径都得记 —— 否则浮层发出去的 `/login` 拿不到 Esc 取消。
    pub(crate) fn note_request_sent(&mut self, req: &Request) {
        if !matches!(req, Request::Prompt(_)) {
            self.command_in_flight = true;
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
                    self.current_round = None;
                    return;
                }
                // ③④ 未本地回显、也不属于任何待回声提交的 `UserMessage`，仅以 `is_turning`
                //     的**真假**区分 —— 绝不按内容匹配。契约：
                // - `is_turning == true` ⇒ 回合进行中的轮边界 steering 插话，仍是用户行；
                // - `is_turning == false` ⇒ 回合**刚落幕**时到达的那条（`RunFinished` 已让
                //   `finish_turn` 复位 `is_turning`，而事件还在信道里排队），渲染成 System
                //   行并点亮 Working —— 它同样属于一个回合，谁也不是谁的特例。
                //
                // 注：这个分支原先是「app 发起的完工复核回合」的落点；复核已于 ADR-0041
                // 撤除，故它现在只覆盖上述那条**收尾竞态**。
                if self.is_turning {
                    self.transcript.push(TranscriptLine::User(text));
                } else {
                    self.transcript.push(TranscriptLine::System(text));
                    self.is_turning = true;
                    self.abort_requested = false;
                    self.turn_started_at = Some(Instant::now());
                    self.current_round = None;
                }
                // 内容追加在**尾部** ⇒ 脱离底部时视窗原地不动（只置提示位）。
                self.note_appended_below();
            }
            // 轮开始：回合进行中唯一的「进度刻度」。**不产生任何显示行** —— 只更新
            // 状态行用的轮号（`draw.rs` 的 `status_text` 在回合中显示它）。
            AgentEvent::RoundStarted { round } => self.current_round = Some(round),
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
                // 内容追加在**尾部** ⇒ 脱离底部时视窗原地不动（只置提示位）。
                self.note_appended_below();
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
                // 「空产出」要给**可见**说明：单靠 Summary 行的 ∅ 符号用户看不懂（实测会继续追问）。
                if stop_reason == StopReason::NoOutput {
                    self.transcript.push(TranscriptLine::System(
                        "模型本轮没有返回可见内容（可能只输出了思考）。可重试或换个说法。".into(),
                    ));
                }
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
                self.note_appended_below();
            }
            AgentEvent::ContextCompactionFailed { reason } => {
                // 压缩失败不终止回合（session 未改动，本轮照常继续）——
                // 故这里只推一条 Error 行，**不调** `finish_turn`。
                self.transcript.push(TranscriptLine::Error(format!(
                    "context compaction failed: {reason}"
                )));
                self.note_appended_below();
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
                self.note_appended_below();
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
        self.current_round = None;
        self.working_dot = 0;
        self.follow = true;
        self.scroll_offset = 0;
    }

    /// 本地回显一条**回合中插话**（steering）。
    ///
    /// 与 [`Self::begin_local_turn`] 的差别是**只回显、不动回合态**：
    /// 插话是**本回合内的额外输入**，不是新回合 ⇒ 绝不能
    /// `turn_started_at = now` / `current_round = None` / `abort_requested = false`
    /// （那会把回合时钟、轮号、已按下的 Ctrl-C 全部重置 —— 用户看到的是
    /// 「回合重新开始了」）。
    ///
    /// **回显即时**：插话真正进上下文要等到下一个**轮边界**（工具执行期间不在边界上），
    /// 若只在事件到达时画，工具跑 16 分钟就 16 分钟看不到自己刚发的消息。
    ///
    /// **抑制回声**：底下那条 `UserMessage` 事件就是这条的回声（`BasicLoop` 在
    /// 轮边界 append 后 emit），靠 `suppress_user_echo` 抑制掉，避免出现两遍。
    pub fn note_steered(&mut self, text: String) {
        self.transcript.push(TranscriptLine::User(text));
        self.suppress_user_echo += 1;
    }

    /// 推进 Working 动画相位。
    pub fn tick_working(&mut self) {
        self.working_dot = (self.working_dot + 1) % WORKING_DOTS;
    }

    /// Working 文案：`Working` + 1..=3 个点（状态行用）。
    pub fn working_text(&self) -> String {
        format!("Working{}", ".".repeat(1 + self.working_dot as usize))
    }

    /// 滚动上限（视觉行数）—— 同步布局后返回。
    ///
    /// 顺带把滚动状态夹到有效范围（resize 后行号空间变了）—— 见 [`App::sync_scroll_state`]。
    pub fn compute_max_scroll(&mut self, width: u16, height: u16) -> usize {
        crate::draw::max_scroll_offset(self, width, height)
    }

    /// 同步布局（逐条目的折行记忆）并维护滚动状态。
    ///
    /// 返回视窗顶部的**全局视觉行号**与滚动上限 —— 帧路径据此渲染视窗，
    /// 事件路径据此夹取 `scroll_offset`。两处读**同一份**布局 ⇒ 画面与滚动必然一致。
    ///
    /// # 「视窗原地不动」
    ///
    /// 脱离底部（`follow == false`）时，内容在**尾部**增长 Δ 行不应把用户拽下去 ——
    /// 故把 `scroll_offset` 也加 Δ（它是「距底部行数」）：视窗顶部那一行不动。
    /// 宽度变化时行号空间整个换掉 ⇒ 不补偿（宽度变化本就重排内容）。
    pub(crate) fn sync_scroll_state(
        &mut self,
        width: u16,
        height: u16,
        now: Instant,
    ) -> crate::layout::ScrollState {
        let (inner_w, inner_h) = crate::draw::chat_viewport(self, width, height);
        if inner_h == 0 {
            return crate::layout::ScrollState::default();
        }
        let (total_rows, width_changed) = {
            let mut cache = self.md_cache.borrow_mut();
            let mut layout = self.layout.borrow_mut();
            let result = layout.sync(
                &self.transcript,
                &mut cache,
                self.thinking_visible,
                inner_w,
                now,
            );
            (result.total_rows, result.width_changed)
        };

        // 增量补偿：内容在尾部增长 ⇒ 视窗原地不动。
        // 首次同步不补偿（没有「上一帧的行号空间」可对）。
        if let Some(previous) = self.last_total_rows
            && !self.follow
            && !width_changed
            && total_rows > previous
        {
            self.scroll_offset += total_rows - previous;
        }
        self.last_total_rows = Some(total_rows);

        let max_scroll = total_rows.saturating_sub(inner_h as usize);
        if self.follow {
            self.scroll_offset = 0;
            self.new_content_below = false;
        } else {
            self.scroll_offset = self.scroll_offset.min(max_scroll);
        }
        let top_row = max_scroll.saturating_sub(self.scroll_offset);
        crate::layout::ScrollState {
            max_scroll,
            top_row,
        }
    }

    /// 记「有新内容追加到**下方**」—— 脱离底部时用于状态行提示。
    ///
    /// 只在**尾部追加**时调用；原地改写（工具结果回填）不调用 ——
    /// 那不是「下方的新内容」。
    fn note_appended_below(&mut self) {
        if !self.follow {
            self.new_content_below = true;
        }
    }

    /// 跳到**最新内容**（末尾）—— 恢复贴底跟随，并清掉「有新内容」提示。
    pub fn jump_to_end(&mut self) {
        self.scroll_offset = 0;
        self.follow = true;
        self.new_content_below = false;
    }

    /// 跳到**最早内容**（顶部）—— 脱离跟随。
    pub fn jump_to_start(&mut self, max_scroll: usize) {
        self.scroll_offset = max_scroll;
        self.follow = false;
    }

    /// 本回合已用秒数（回合未开始时为 0）。`draw.rs` 的状态行要用 ⇒ `pub(crate)`。
    pub(crate) fn turn_elapsed_secs(&self) -> f32 {
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
        self.current_round = None;
        self.suppress_user_echo = 0;
        self.tool_index.clear();
        // **不**强制贴底：回合结算行（Summary）已按「尾部追加」记过提示位。
        // 视窗在底部时靠 `follow` 继续跟随；用户翻上去时视窗原地不动。
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
        // 流式增量追加在**尾部** ⇒ 脱离底部时视窗原地不动（只置提示位）。
        self.note_appended_below();
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

    /// **N7**：`RunFinished{NoOutput}` ⇒ transcript 出现一条**可见的 System 说明**（不是只给符号），
    /// 且回合正常收尾（`is_turning` 复位）。
    ///
    /// 变异：删掉 `app.rs` 里对 `NoOutput` 的 System 推送 ⇒ 本用例变红。
    #[test]
    fn test_no_output_turn_pushes_visible_system_note() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(env(AgentEvent::RunFinished {
            stop_reason: StopReason::NoOutput,
            usage: Usage::default(),
            rounds: 1,
        }));
        assert!(!a.is_turning, "终局事件必须复位 turn 态");
        let note = a.transcript.iter().find_map(|l| match l {
            TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        });
        assert!(
            note.as_deref().is_some_and(|s| s.contains("可见内容")),
            "NoOutput 必须推一条可见 System 说明；transcript={:?}",
            a.transcript
        );
        assert!(
            a.transcript.iter().any(|l| matches!(
                l,
                TranscriptLine::Summary {
                    stop: StopReason::NoOutput,
                    ..
                }
            )),
            "Summary 行应记录 NoOutput"
        );
    }

    /// 反向：正常 `Completed` **不得**多出「空产出」提示。
    ///
    /// 变异：把 `if stop_reason == StopReason::NoOutput` 写成恒真 ⇒ 本用例变红。
    #[test]
    fn completed_turn_does_not_push_no_output_note() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(env(AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage::default(),
            rounds: 1,
        }));
        assert!(
            !a.transcript
                .iter()
                .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("可见内容"))),
            "Completed 不得带空产出提示；transcript={:?}",
            a.transcript
        );
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
        a.follow = false;
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
        assert!(
            !a.follow,
            "新内容到达**不得**把视窗拽回底部（视窗原地不动）"
        );
        assert!(a.new_content_below, "脱离底部时提示「下方有新内容」");
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
        assert_eq!(
            a.turn_started_at, started_before,
            "steering 不动 turn_started_at"
        );
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
        a.follow = false;
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
        assert!(!a.follow, "新内容到达**不得**把视窗拽回底部");
        assert!(a.new_content_below, "脱离底部时提示「下方有新内容」");
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

    /// `RoundStarted` 是**非终局**事件：只更新状态行的轮号，**不产生显示行、不结束回合**。
    ///
    /// 变异：① 删掉 `apply_event` 的 `RoundStarted` 臂 ⇒ `current_round` 断言变红；
    /// ② 在该臂里误调 `finish_turn()` ⇒ `is_turning` / `turn_started_at` 断言变红；
    /// ③ 把它当成「新起一条显示行」⇒ transcript 长度断言变红。
    #[test]
    fn test_round_started_tracks_round_without_ending_turn() {
        let mut a = app();
        a.begin_local_turn("q".into());
        let before = a.transcript.len();
        assert_eq!(a.current_round, None, "前置：回合刚开始，还没有轮");

        a.apply_event(env(AgentEvent::RoundStarted { round: 1 }));
        assert_eq!(a.current_round, Some(1));

        a.apply_event(env(AgentEvent::RoundStarted { round: 3 }));
        assert_eq!(a.current_round, Some(3), "轮号取最新值，不累加");

        assert_eq!(a.transcript.len(), before, "轮事件不得产生显示行");
        // **非终局**：本轮还没跑完，不得复位 turn 态
        assert!(a.is_turning, "RoundStarted 不得调 finish_turn");
        assert!(a.turn_started_at.is_some());
    }

    /// 轮号随回合结束清零。
    ///
    /// 生命周期不变量：`current_round` 与 `is_turning` / `turn_started_at` 同进退。
    /// 变异：把 `finish_turn` 里的 `current_round = None` 删掉 ⇒ 本用例变红，
    /// 且下一回合的状态行会显示**上一回合**的轮号。
    #[test]
    fn test_current_round_resets_at_turn_end() {
        let mut a = app();
        a.begin_local_turn("q".into());
        a.apply_event(env(AgentEvent::RoundStarted { round: 7 }));
        assert_eq!(a.current_round, Some(7));

        a.apply_event(run_finished(1));
        assert_eq!(a.current_round, None, "回合结束必须清零轮号");
    }

    /// 纯文本被截断（0 个工具调用）：文案同样警告，但**不**提工具调用。
    ///
    /// 变异：把 `dropped_tool_calls == 0` 的分支去掉（统一走带工具数的文案）→
    /// `!s.contains("tool call(s)")` 断言变红。
    #[test]
    fn test_response_truncated_without_tool_calls_omits_tool_wording() {
        let mut a = app();
        a.follow = false;
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
        assert!(!a.follow, "新内容到达**不得**把视窗拽回底部");
        assert!(a.new_content_below, "脱离底部时提示「下方有新内容」");
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

    // -----------------------------------------------------------------------
    // /resume：SessionList / SessionResumed / reset_for_replay（N8 / I4 / I8）
    // -----------------------------------------------------------------------

    fn summary(id: &str, title: &str) -> SessionSummary {
        use std::time::{Duration, UNIX_EPOCH};
        SessionSummary {
            id: id.into(),
            path: std::path::PathBuf::from(format!("/sessions/{id}.jsonl")),
            mtime: UNIX_EPOCH + Duration::from_secs(1_760_000_000),
            title: title.into(),
        }
    }

    fn assistant_message(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// **N8**：`reset_for_replay()` **逐项**清空与 transcript 同源的状态。
    ///
    /// 每一项都被单独断言 ⇒ 删掉 `reset_for_replay` 里对应的任一行，本用例即变红：
    /// - 删 `transcript.clear()` ⇒ `transcript` 断言红
    /// - 删 `md_cache.reset()`   ⇒ 等长碰撞断言红（下面用 `AAAA`/`BBBB` 钉住，见注释）
    /// - 删 `tool_index.clear()` ⇒ `tool_index` 断言红
    /// - 删 `suppress_user_echo = 0` ⇒ 计数断言红
    /// - 删 `pending_echo_texts.clear()` ⇒ 队列断言红
    /// - 删 `follow = true` ⇒ `follow` 断言红
    /// - 删 `scroll_offset = 0` ⇒ 滚动断言红
    #[test]
    fn test_reset_for_replay_clears_every_same_source_state() {
        use crate::styles::AssistantStyles;
        use std::time::Instant;

        let mut a = app();
        // 各项先填成「非默认」态。
        a.transcript.push(TranscriptLine::User("q".into()));
        a.transcript.push(TranscriptLine::Assistant("AAAA".into()));
        a.tool_index.insert(ToolCallId("c1".into()), 0);
        a.suppress_user_echo = 2;
        a.pending_echo_texts.push_back("pending".into());
        a.follow = false;
        a.scroll_offset = 5;
        // md_cache：先渲染 "AAAA" 占住槽位（长度 4、宽度 60）。
        crate::md::reset_render_calls();
        {
            let mut c = a.md_cache.borrow_mut();
            c.sync_len(a.transcript.len());
            c.lines_for(1, "AAAA", 60, AssistantStyles, Instant::now());
        }
        assert_eq!(crate::md::render_calls(), 1, "前置：AAAA 已入缓存");

        a.reset_for_replay();

        assert!(a.transcript.is_empty(), "transcript 清空");
        assert!(a.tool_index.is_empty(), "tool_index 清空");
        assert_eq!(a.suppress_user_echo, 0, "回显抑制计数清零");
        assert!(a.pending_echo_texts.is_empty(), "待回声文本清空");
        assert!(a.follow, "回到贴底");
        assert_eq!(a.scroll_offset, 0, "滚动归零");

        // md_cache 已失效：**等长**的新内容必须重渲染，不得复用旧槽位（I6 等长碰撞）。
        // 若把 `reset()` 删掉，`lines_for` 会命中 "AAAA" 的产物（键 = 槽下标+len+width 全同）。
        {
            let mut c = a.md_cache.borrow_mut();
            let lines = c.lines_for(1, "BBBB", 60, AssistantStyles, Instant::now());
            let flattened: String = lines
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                .collect();
            assert_eq!(flattened, "BBBB", "reset_for_replay 必须使 md_cache 失效");
        }
        assert_eq!(crate::md::render_calls(), 2, "reset 后等长内容必须重算");
    }

    /// `SessionList(非空)` ⇒ **同时**设 `pending_sessions` 与 `pending_prompt`
    /// （钉住「谁负责开浮层」：是 `pending_prompt`，候选取自 `pending_sessions`）。
    ///
    /// 变异：只设 `pending_sessions`、不设 `pending_prompt`（或反之）⇒ 本用例红。
    #[test]
    fn test_apply_outbound_session_list_nonempty_sets_both_fields() {
        let mut a = app();
        let items = vec![summary("a", "会话 A"), summary("b", "会话 B")];
        a.apply_outbound(Outbound::SessionList(items.clone()));

        assert_eq!(
            a.pending_sessions.as_deref(),
            Some(items.as_slice()),
            "候选会话已设"
        );
        assert_eq!(
            a.pending_prompt,
            Some(PromptKind::PickSession),
            "由 pending_prompt 驱动浮层"
        );
    }

    /// **I4**：空列表 ⇒ 只往 transcript 推一条提示，**不**设候选、**不**开浮层。
    ///
    /// 变异：把空列表也走非空分支（设 `pending_prompt`）⇒ 本用例的 `pending_prompt`
    /// 断言红；把提示去掉 ⇒ transcript 断言红。
    #[test]
    fn test_apply_outbound_empty_session_list_hints_without_modal() {
        let mut a = app();
        a.apply_outbound(Outbound::SessionList(vec![]));

        assert!(a.pending_sessions.is_none(), "空列表不设候选");
        assert!(a.pending_prompt.is_none(), "空列表不弹浮层（I4）");
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("没有可恢复的会话")),
            "空列表要给一句提示：{:?}",
            a.transcript
        );
    }

    /// **P2-7**：同一轮 drain 内先到**非空** `SessionList`、再到**空**的一条 ⇒ 旧候选必须
    /// 被清掉（两字段归 `None`），只留提示 —— 否则主循环会弹出过期列表。
    ///
    /// 变异：删掉空分支里的 `pending_sessions = None` / `pending_prompt = None` ⇒ 本用例红
    /// （旧候选仍在，`pending_prompt` 仍指 `PickSession`）。
    #[test]
    fn test_empty_session_list_after_nonempty_clears_stale_candidates() {
        let mut a = app();
        a.apply_outbound(Outbound::SessionList(vec![summary("a", "会话 A")]));
        assert!(a.pending_sessions.is_some(), "前置：非空列表已设候选");
        assert_eq!(a.pending_prompt, Some(PromptKind::PickSession));

        a.apply_outbound(Outbound::SessionList(vec![]));

        assert!(
            a.pending_sessions.is_none(),
            "随后的空列表必须清掉旧候选：{:?}",
            a.pending_sessions
        );
        assert!(a.pending_prompt.is_none(), "也不得留下降层指令");
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("没有可恢复的会话")),
            "仍要给一句提示：{:?}",
            a.transcript
        );
    }

    /// `SessionResumed` ⇒ 清屏 + 回放目标历史、`thinking_visible` 跟随载荷、贴底。
    ///
    /// 变异：① 不调 `reset_for_replay()` ⇒ 旧行残留，「清屏」断言红（旧 Q 行还在）；
    /// ② 不设 `thinking_visible` ⇒ 跟随断言红；③ 不设 `follow` ⇒ 贴底断言红。
    #[test]
    fn test_apply_outbound_session_resumed_replays_and_follows_thinking() {
        let mut a = app();
        a.transcript.push(TranscriptLine::User("旧的 Q".into()));
        a.follow = false;
        assert!(!a.thinking_visible, "前置：UI 真值为隐藏");

        let history = vec![user_message("hi"), assistant_message("hello")];
        a.apply_outbound(Outbound::SessionResumed {
            summary: summary("target", "目标会话"),
            thinking_visible: true,
            history: history.clone(),
        });

        assert_eq!(
            a.transcript,
            replay_lines(&history),
            "transcript 必须变成回放内容"
        );
        assert!(
            !a.transcript
                .iter()
                .any(|l| matches!(l, TranscriptLine::User(s) if s == "旧的 Q")),
            "回放前必须清屏（旧会话的行不得残留）：{:?}",
            a.transcript
        );
        assert!(
            a.thinking_visible,
            "thinking 跟随目标会话的 meta（有意破例）"
        );
        assert!(a.follow, "回放后回到贴底");
    }

    /// **视窗原地不动**：用户往上翻之后，新内容到达**不得**把视窗拽回底部。
    ///
    /// 这是「翻到前面却被自动刷回输入处」那个问题的正解：
    /// 视窗顶部那一行（`top_row`）在新内容到达前后必须**完全相同**。
    #[test]
    fn test_detached_view_stays_put_when_new_content_arrives() {
        let mut a = app();
        for i in 0..60 {
            a.transcript.push(TranscriptLine::User(format!("行 {i}")));
        }
        let now = Instant::now();
        let base = a.sync_scroll_state(120, 40, now);
        assert!(a.follow, "前置：默认贴底");

        // 脱离底部：向上滚 10 行。
        a.follow = false;
        a.scroll_offset = 10;
        let before = a.sync_scroll_state(120, 40, now);
        assert_eq!(
            before.top_row,
            base.max_scroll - 10,
            "视窗顶部 = 上限 − 偏移"
        );

        // 新内容到达（流式增量）⇒ 视窗原地不动。
        a.apply_event(env(AgentEvent::ModelTextDelta {
            text: "新的一行内容".into(),
        }));
        let after = a.sync_scroll_state(120, 40, now);
        assert_eq!(after.top_row, before.top_row, "新内容到达不得把视窗拽走");
        assert!(!a.follow, "不得脱离跟随地回到贴底");
        assert!(a.new_content_below, "脱离底部时提示「下方有新内容」");
    }

    /// 跳到最新内容 ⇒ 恢复贴底并清掉「有新内容」提示。
    #[test]
    fn test_jump_to_end_restores_follow_and_clears_hint() {
        let mut a = app();
        for i in 0..60 {
            a.transcript.push(TranscriptLine::User(format!("行 {i}")));
        }
        let now = Instant::now();
        a.sync_scroll_state(120, 40, now);
        a.follow = false;
        a.scroll_offset = 10;
        a.new_content_below = true;

        a.jump_to_end();

        assert!(a.follow, "跳到底部 ⇒ 恢复贴底");
        assert_eq!(a.scroll_offset, 0, "跳到底部 ⇒ 偏移归零");
        assert!(!a.new_content_below, "跳到底部 ⇒ 提示清零");
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
