//! app 侧循环（设计 §7）。
//!
//! 它是「UI 线程」与「agent 线程」之间的那一半 —— 一个线程上跑两层循环：
//!
//! ```text
//! loop {                            ① 回合之间：等一条 Request
//!   let req = request_rx.recv().await?;
//!   match req {
//!     Prompt(msg) => {
//!       turns += 1;
//!       let res = {                  ② 回合之内：turn 与事件转发交错
//!         let ports = wiring.ports();
//!         ports.events.begin_turn(turns);
//!         let boundary = QueueBoundarySource::new();
//!         let turn = agent.run_turn(input, AgentPorts::new(.., Some(&boundary)));
//!         loop { select! {
//!           r   = &mut turn            => break r,       // 回合结束
//!           Some(env) = sink_rx.recv() => out.send(Event(env)).await?,  // 事件转发
//!           Some(b) = boundary_rx.recv() => boundary.push(b),            // 轮边界
//!         }}
//!       };
//!       stats.record(&res.usage);
//!     }
//!     SetModel/Login/Logout/NewSession/Compact/Export => { 能力函数 → Output }
//!   }
//!   emit_view();                     // 每个请求处理完刷一次快照
//! }
//! ```
//!
//! # 三个要点
//!
//! **① `ports` 的借用必须包在块里**：`wiring.ports()` 可变借用 `wiring`，且被
//! `turn` future 持有整个回合。若写在 `match` 臂里跨到外层循环，外层就再也不能
//! 碰 `wiring`（`SetModel` / `NewSession` 都要碰）。`{ … }` 让借用随块结束。
//!
//! **② 事件经 app 转发（而非 UI 直读事件信道）**：`Outbound<V>` 是 UI 唯一的
//! 输入流 —— **单一出站写者**。UI 若同时直读事件信道，事件与 `Output` 的先后
//! 就没了保证（两条信道各排各的）。代价是每事件多一跳，可忽略。
//!
//! **③ 一次性 BoundarySource 每回合新建**：`Abort` 会**永久**置位 `is_aborted`，
//! 复用同一个源会让「上一回合的取消」把之后每个回合立刻取消。

use std::time::Instant;

use tokio::sync::mpsc;
use ys_loop::AgentInput;
use ys_protocol::{Boundary, Envelope, Outbound, QueueBoundarySource, Request};
use ys_runtime::{Agent, AgentPorts};
use ys_tui_coding::CodingView;

use crate::capabilities;
use crate::config::Config;
use crate::logging;
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::view::build_view;
use crate::wiring::Wiring;

/// app 循环的失败：只有「UI 侧已经收摊」这一种（`Outbound` 信道关闭）。
///
/// 模型 / 工具失败不是循环的失败 —— `BasicLoop` 已发 `RunFailed`（UI 显示 Error
/// 行），循环继续等下一条 `Request`。
pub type AppLoopError = Box<dyn std::error::Error + Send + Sync>;

/// 跑 app 侧循环，直到 UI 侧把所有发送端 drop 掉（= 正常收摊）。
///
/// 参数里的三条信道方向见设计 §8：`request_rx` / `boundary_rx` 是 UI → app，
/// `out_tx` 是 app → UI，`sink_rx` 是 `ChannelSink` → app（app 再转成
/// `Outbound::Event`）。
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut agent: Agent,
    mut wiring: Wiring,
    mut config: Config,
    state_store: StateStore,
    mut stats: TurnStats,
    mut session_started: Instant,
    mut request_rx: mpsc::Receiver<Request>,
    mut boundary_rx: mpsc::Receiver<Boundary>,
    out_tx: mpsc::Sender<Outbound<CodingView>>,
    mut sink_rx: mpsc::Receiver<Envelope>,
) -> Result<(), AppLoopError> {
    // 回合号从 1 起（`Envelope.turn` 靠 `begin_turn` 携带）。
    let mut turns: u32 = 0;

    loop {
        // 空闲期丢掉「上一回合结束后才到的」边界消息。UI 的 `is_turning` 要等
        // 它排空 `Outbound` 才复位（最长一个 poll 周期），期间一次 Ctrl+C 就会
        // 投出一条来不及被本回合消费的 `Abort` —— 留着它会把**下一个**回合
        // 立刻取消。回合已结束，这些消息在语义上就是废的。
        while boundary_rx.try_recv().is_ok() {}

        let Some(req) = request_rx.recv().await else {
            return Ok(()); // UI 侧 drop 了发送端 → 正常退出
        };

        let mut lines: Vec<String> = Vec::new();

        match req {
            Request::Prompt(message) => {
                turns += 1;
                let outcome = {
                    let ports = wiring.ports();
                    ports.events.begin_turn(turns);
                    // 本回合专属的边界源：`Abort` 的标志位随回合作废。
                    let boundary = QueueBoundarySource::new();
                    let turn = agent.run_turn(
                        AgentInput::new(message),
                        AgentPorts::new(ports.model, ports.session, ports.events, Some(&boundary)),
                    );
                    tokio::pin!(turn);
                    loop {
                        tokio::select! {
                            result = &mut turn => break result,
                            // 事件转发。**必须用 `Some(..) = …` 模式形式**：
                            // 信道关闭后 `recv()` 会立即返回 `None`，裸分支
                            // （`env = … => match`）会让 select 每次都立刻选中它
                            // → 忙等。模式不匹配时 tokio 会**禁用**该分支。
                            Some(env) = sink_rx.recv() => {
                                send(&out_tx, Outbound::Event(env)).await?;
                            }
                            // 轮边界中途插话 / 中止：UI 写信道 ②，这里泵进
                            // `BoundarySource`，由 `BasicLoop` 在轮边界拉取。
                            Some(b) = boundary_rx.recv() => boundary.push(b),
                        }
                    }
                };
                // **回合结束后必须排空事件信道**：`select!` 分支的选择是随机的，
                // 它完全可能先选中 `turn` 分支 —— 此刻 `RunFinished` 等收尾事件
                // 还躺在信道里。不排空就会晚一整回合才转发（UI 的 `is_turning`
                // 不复位、turn 摘要不显示），且会插到本条 `View` 之后。
                // 回合已结束 → 生产者不会再发，`try_recv` 排到 `Empty` 即止。
                while let Ok(env) = sink_rx.try_recv() {
                    send(&out_tx, Outbound::Event(env)).await?;
                }
                match outcome {
                    Ok(result) => stats.record(&result.usage),
                    // `BasicLoop` 在返回错误前已发 `RunFailed` —— UI 已经看到
                    // Error 行，这里不再往 transcript 重复刷一条，只落日志。
                    Err(e) => logging::log(&format!("turn {turns} failed: {e}")),
                }
            }

            Request::SetModel { model } => {
                lines = capabilities::set_model(&mut config, &mut wiring, &state_store, &model);
            }

            Request::Login {
                provider,
                api_key,
                api_base,
            } => {
                lines = capabilities::login(
                    &mut config,
                    &mut wiring,
                    &state_store,
                    &provider,
                    &api_key,
                    api_base.as_deref(),
                );
            }

            Request::Logout => {
                lines = capabilities::logout(&mut config, &mut wiring, &state_store);
            }

            Request::NewSession => {
                lines = capabilities::new_session(&mut wiring).await;
                // 新会话 = 新的会话起点（状态行的时长据此重置）。
                session_started = Instant::now();
            }

            Request::Compact => {
                lines = capabilities::compact(&mut wiring).await;
            }

            Request::Export { path } => {
                lines = capabilities::export(&wiring, path);
            }
        }

        // 命令输出走 transcript（设计 §5：命令期间的诊断 → `Outbound::Output`）。
        if !lines.is_empty() {
            send(&out_tx, Outbound::Output(lines.join("\n"))).await?;
        }

        // 每个请求处理完刷新快照 —— 状态行（provider / model / tokens）才不会滞后。
        let view = build_view(
            &config,
            &agent,
            &wiring,
            &config.registry,
            &state_store,
            &stats,
            session_started,
        );
        send(&out_tx, Outbound::View(view)).await?;
    }
}

/// 发送一条出站消息；UI 侧已收摊（信道关闭）时返回 `Err` 让循环退出。
async fn send(
    tx: &mpsc::Sender<Outbound<CodingView>>,
    out: Outbound<CodingView>,
) -> Result<(), AppLoopError> {
    tx.send(out)
        .await
        .map_err(|_| AppLoopError::from("outbound channel closed (UI exited)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ys_core::{ContentBlock, Message, Role, StopReason, ToolCallId, Usage};
    use ys_event::AgentEvent;
    use ys_model::{
        MockModel, Model, ModelError, ModelEvent, ModelEventSink, ModelRequest, ModelResponse,
    };

    use crate::channel::ChannelSink;

    /// 所有跨任务 `await` 都套这个超时：死锁回归必须表现为**失败**，而不是把测试挂住。
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// 等「第 1 次 model 调用开始」的上限。**只影响失败有多快，不影响正确性**：
    /// 超时后照样放行门控（见 `stale_abort_...` 测试）。取一个远大于「任务已被
    /// 唤醒 → 跑到首次 model 调用」所需时间的值，正常路径下不可能撞上。
    const ENTRY_WAIT: Duration = Duration::from_millis(500);

    // ------------------------------------------------------------------ 测试台

    /// 临时目录（`Drop` 自动清理）。
    ///
    /// 名字含**进程内原子序号 + `process::id()`**（并行安全，血的教训）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "yushan_app_loop_{tag}_{}_{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// app 循环 + 三条信道的对端。
    ///
    /// 故意**不实现 `Drop`**：测试 7 要把各字段移出去（`drop(request_tx)` 模拟 UI
    /// 收摊），而有 `Drop` 的类型不允许字段被部分移出。任务随 runtime 一起回收。
    struct Harness {
        requests: mpsc::Sender<Request>,
        boundaries: mpsc::Sender<Boundary>,
        outbound: mpsc::Receiver<Outbound<CodingView>>,
        handle: tokio::task::JoinHandle<Result<(), AppLoopError>>,
        tmp: TempDir,
    }

    impl Harness {
        fn temp_path(&self, name: &str) -> PathBuf {
            self.tmp.join(name)
        }
    }

    /// 起 app 循环。
    ///
    /// - `persistent`：`true` = 会话落盘到临时 `sessions/`（`/new` 换文件可见）。
    /// - `prefilled`：在**循环开跑之前**就躺进边界信道的消息（复现「上一回合残留」）。
    ///
    /// 测试绝不碰真实的 `~/.yushan`：auth / state / 会话文件全部改到临时目录。
    async fn spawn_app_loop(
        model: Box<dyn Model>,
        persistent: bool,
        prefilled: Vec<Boundary>,
    ) -> Harness {
        let tmp = TempDir::new("harness");
        let (sink, sink_rx) =
            ChannelSink::new(64, ys_protocol::LifecyclePolicy::StopWhenConsumerGone);
        let agent = ys_runtime::AgentBuilder::new().build().unwrap();

        let mut config = Config::from_env().unwrap();
        config.registry.set_auth_override(tmp.join("auth.json"));
        config.api_base = None;
        config.api_key = None;
        config.provider = None;

        let mut state_store = StateStore::new();
        state_store.set_override(tmp.join("state.json"));

        let wiring = if persistent {
            Wiring::persistent(Some(model), Box::new(sink), tmp.join("sessions"))
                .await
                .unwrap()
        } else {
            Wiring::ephemeral(Some(model), Box::new(sink))
        };

        let (requests, request_rx) = mpsc::channel(8);
        let (boundaries, boundary_rx) = mpsc::channel(8);
        let (out_tx, outbound) = mpsc::channel(64);

        for b in prefilled {
            boundaries.try_send(b).unwrap();
        }

        let handle = tokio::spawn(run(
            agent,
            wiring,
            config,
            state_store,
            TurnStats::default(),
            Instant::now(),
            request_rx,
            boundary_rx,
            out_tx,
            sink_rx,
        ));

        Harness {
            requests,
            boundaries,
            outbound,
            handle,
            tmp,
        }
    }

    /// 便捷：一次性会话 + 单个 [`MockModel`]。
    async fn spawn_with_mock(model: MockModel) -> Harness {
        spawn_app_loop(Box::new(model), false, Vec::new()).await
    }

    fn user_message(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn prompt(text: &str) -> Request {
        Request::Prompt(user_message(text))
    }

    /// 收一条出站消息（带超时 —— 挂起即失败）。
    async fn recv_out(rx: &mut mpsc::Receiver<Outbound<CodingView>>) -> Outbound<CodingView> {
        tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("收取 Outbound 超时（疑似死锁）")
            .expect("app 循环不应提前收摊")
    }

    /// 收出站消息直到一条 `View`（每个请求处理完必刷一次）。
    async fn collect_until_view(h: &mut Harness) -> Vec<Outbound<CodingView>> {
        let mut msgs = Vec::new();
        loop {
            let m = recv_out(&mut h.outbound).await;
            let is_view = matches!(m, Outbound::View(_));
            msgs.push(m);
            if is_view {
                return msgs;
            }
        }
    }

    /// 取出末尾的 `View`（每个请求处理完必有），剩下的留给顺序断言。
    fn take_view(msgs: &mut Vec<Outbound<CodingView>>) -> CodingView {
        match msgs.pop() {
            Some(Outbound::View(view)) => view,
            other => panic!("最后一条出站消息应为 View，得到 {other:?}"),
        }
    }

    fn events_of(msgs: &[Outbound<CodingView>]) -> Vec<&Envelope> {
        msgs.iter()
            .filter_map(|m| match m {
                Outbound::Event(env) => Some(env),
                _ => None,
            })
            .collect()
    }

    /// 事件的**种类序列** —— 锁定顺序，而不只是集合。
    fn event_kinds(msgs: &[Outbound<CodingView>]) -> Vec<&'static str> {
        events_of(msgs)
            .iter()
            .map(|e| match &e.event {
                AgentEvent::UserMessage { .. } => "UserMessage",
                AgentEvent::ModelTextDelta { .. } => "ModelTextDelta",
                AgentEvent::RunFinished { .. } => "RunFinished",
                AgentEvent::RunFailed { .. } => "RunFailed",
                _ => "Other",
            })
            .collect()
    }

    fn stop_reasons(events: &[&Envelope]) -> Vec<StopReason> {
        events
            .iter()
            .filter_map(|e| match &e.event {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect()
    }

    // ------------------------------------- probe 型 model（供「回合进行中」注入边界）

    /// probe model 的行为计划。
    #[derive(Clone, Copy)]
    enum ProbePlan {
        /// 直到请求里出现含 `marker` 的文本消息才收尾；之前每轮回一个**不存在**的
        /// tool call，把回合推过一轮又一轮的轮边界。
        UntilMarker(&'static str),
        /// 恒回不存在的 tool call：`Abort` 场景下，下一轮的 abort 探针先于 model 调用命中。
        AlwaysToolCall,
        /// 第 1 次（被 gate 卡住的那次）回不存在的 tool call，第 2 次回结束文本。
        ///
        /// 供「残留 Abort 是否被丢弃」的测试用：**必须**让回合跨过一个轮边界，
        /// 泄漏的 `Abort` 才有机会被 `BasicLoop` 的探针看见（否则第 1 轮直接
        /// 正常收尾，泄漏与不泄漏都产出 `Completed`，测试就分不出红绿）。
        ToolCallThenFinal,
    }

    struct ProbeInner {
        plan: ProbePlan,
        /// 第 1 次 `complete` 阻塞在此 —— 测试得以在「回合进行中」投递 `Boundary`。
        gate: tokio::sync::Notify,
        entered: mpsc::UnboundedSender<u32>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    struct ProbeModel {
        inner: Arc<ProbeInner>,
    }

    struct ProbeHandle {
        entered: mpsc::UnboundedReceiver<u32>,
        inner: Arc<ProbeInner>,
    }

    fn probe_model(plan: ProbePlan) -> (ProbeModel, ProbeHandle) {
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let inner = Arc::new(ProbeInner {
            plan,
            gate: tokio::sync::Notify::new(),
            entered: entered_tx,
            requests: Mutex::new(Vec::new()),
        });
        (
            ProbeModel {
                inner: Arc::clone(&inner),
            },
            ProbeHandle { entered, inner },
        )
    }

    impl ProbeHandle {
        /// 等到第 `n` 次 model 调用**已经开始**（此刻回合正卡在本轮 model 调用里）。
        async fn wait_entered(&mut self, n: u32) {
            loop {
                let call = tokio::time::timeout(TIMEOUT, self.entered.recv())
                    .await
                    .expect("model 未被调用：超时")
                    .expect("probe 通道不应关闭");
                if call == n {
                    return;
                }
            }
        }

        /// 放行第 1 次 `complete`。`notify_one` 会留存一个 permit，
        /// 「先放行、后 await」也不会丢通知。
        fn release(&self) {
            self.inner.gate.notify_one();
        }

        /// 让调度器把 app 任务跑一会儿 —— 确保刚投进边界信道的消息已被泵进本轮的
        /// `BoundarySource`（此刻 model 仍阻塞在 gate 上，`select!` 必走边界分支）。
        ///
        /// 默认 `#[tokio::test]` 是 current-thread runtime：`yield_now` 会让出给
        /// app 任务，故这一步是**确定性**的，不是定时赌博。
        async fn let_app_task_pump(&self) {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.inner.requests.lock().unwrap().clone()
        }
    }

    fn has_text(request: &ModelRequest, needle: &str) -> bool {
        request.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains(needle)))
        })
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
            usage: Usage::default(),
            stop_reason: None,
        }
    }

    #[async_trait::async_trait]
    impl Model for ProbeModel {
        fn model_id(&self) -> &str {
            "probe"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let call = {
                let mut reqs = self.inner.requests.lock().unwrap();
                reqs.push(request.clone());
                reqs.len() as u32
            };
            let _ = self.inner.entered.send(call);
            if call == 1 {
                self.inner.gate.notified().await;
            }

            let done = match self.inner.plan {
                ProbePlan::UntilMarker(marker) => has_text(&request, marker),
                ProbePlan::AlwaysToolCall => false,
                ProbePlan::ToolCallThenFinal => call > 1,
            };

            if done {
                sink.emit(ModelEvent::TextDelta {
                    text: "done".into(),
                })?;
                Ok(text_response("done"))
            } else {
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: ToolCallId("probe-1".into()),
                            name: "no_such_tool".into(),
                            arguments: serde_json::Value::Null,
                        }],
                    },
                    usage: Usage::default(),
                    stop_reason: None,
                })
            }
        }
    }

    // -------------------------------------------------------------------- 测试

    /// 1. **单一出站写者 / 顺序确定性**（设计 §8）：事件按到达顺序、终局 `View`
    ///    落在最后、`Output` 绝不插到事件中间、`Envelope.turn` 随回合单调。
    ///
    ///    变异：让事件绕过 app 转发（在 `select!` 分支里丢弃 `env`）→ 本测试的
    ///    `event_kinds` 断言变红。
    #[tokio::test]
    async fn outbound_sequence_is_ordered_with_view_last() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        model.push_text("again");
        let mut h = spawn_with_mock(model).await;

        // 回合 1：事件（种类顺序固定）→ View。
        h.requests.send(prompt("one")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.turn_count, 1);
        assert!(
            msgs.iter().all(|m| matches!(m, Outbound::Event(_))),
            "回合期间只应有事件（唯一的 View 在末尾）：{msgs:?}"
        );
        assert_eq!(
            event_kinds(&msgs),
            vec!["UserMessage", "ModelTextDelta", "RunFinished"],
            "事件顺序必须与循环产出顺序一致：{msgs:?}"
        );
        assert!(events_of(&msgs).iter().all(|e| e.turn == 1));

        // 回合 2：新事件全部排在**上一个 View 之后**（顺序整体单调，无交叉）。
        h.requests.send(prompt("two")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.turn_count, 2);
        assert!(
            events_of(&msgs).iter().all(|e| e.turn == 2),
            "第二个回合的信封 turn 应为 2：{msgs:?}"
        );

        // 命令：出站序列恰为 `[Output]` + 末尾 `View` —— 事件不得插队。
        h.requests.send(Request::NewSession).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.message_count, 0, "换会话后消息数归零");
        match msgs.as_slice() {
            [Outbound::Output(text)] => {
                assert!(text.contains("New conversation started"), "{text}")
            }
            other => panic!("命令的出站序列应恰为 [Output]：{other:?}"),
        }
    }

    /// 2. **回合驱动**：每来一条 `Prompt` 回合号 +1，且信封的 `turn` 与之同步。
    #[tokio::test]
    async fn each_prompt_advances_turn_count() {
        let model = MockModel::new("mock");
        model.push_text("a");
        model.push_text("b");
        let mut h = spawn_with_mock(model).await;

        for expected in 1..=2u32 {
            h.requests.send(prompt("hi")).await.unwrap();
            let mut msgs = collect_until_view(&mut h).await;
            let view = take_view(&mut msgs);
            assert_eq!(view.turn_count, expected, "回合数应从 1 起递增");
            let turns: Vec<u32> = events_of(&msgs).iter().map(|e| e.turn).collect();
            assert!(!turns.is_empty(), "应至少收到一个事件");
            assert!(
                turns.iter().all(|&t| t == expected),
                "Envelope.turn 应与回合号一致：{turns:?}"
            );
        }
    }

    /// 3. `Boundary::Steer` 中途注入：该消息出现在**后续轮**的 `ModelRequest` 里
    ///    （第 1 轮请求发出时它还不在），并以 `UserMessage` 事件转发给 UI。
    #[tokio::test]
    async fn steer_is_injected_at_round_boundary() {
        const MARKER: &str = "STEER-MARKER";
        let (model, mut probe) = probe_model(ProbePlan::UntilMarker(MARKER));
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries
            .send(Boundary::Steer(user_message(MARKER)))
            .await
            .unwrap();
        probe.let_app_task_pump().await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Completed],
            "steer 之后回合应正常收尾：{msgs:?}"
        );

        let requests = probe.requests();
        assert!(
            requests.len() >= 2,
            "steer 至少应让回合多跑一轮：{}",
            requests.len()
        );
        assert!(
            !has_text(&requests[0], MARKER),
            "第 1 轮请求发出时 steer 尚未投递"
        );
        assert!(
            has_text(&requests[1], MARKER),
            "steer 必须出现在后续轮的 ModelRequest 里"
        );
        assert!(
            events_of(&msgs).iter().any(|e| matches!(
                &e.event,
                AgentEvent::UserMessage { message } if message.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.contains(MARKER)))
            )),
            "steer 也应作为 UserMessage 事件转发给 UI：{msgs:?}"
        );
    }

    /// 4. `Boundary::Abort` → 终局 `RunFinished { Cancelled }`，且 `View` 仍然发出
    ///    （UI 的 `is_turning` 才有复位依据）；下一回合不受上一回合 `Abort` 影响。
    #[tokio::test]
    async fn abort_yields_cancelled_and_view_still_emitted() {
        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries.send(Boundary::Abort).await.unwrap();
        probe.let_app_task_pump().await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Cancelled],
            "中途 Abort 应以 Cancelled 收场：{msgs:?}"
        );
        assert_eq!(view.turn_count, 1, "被取消的回合仍计入回合数");

        // 回合复位：`Abort` 不得溢出到下一回合 —— 新回合的 `QueueBoundarySource`
        // 从干净状态起步，故这一回合会一直跑到轮数上限，而不是立刻被取消。
        h.requests.send(prompt("again")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::MaxRounds],
            "上一回合的 Abort 不得溢出到下一回合：{msgs:?}"
        );
    }

    /// 5. `NewSession`：会话换新（`message_count == 0`）、快照刷新；持久化路径下
    ///    **新会话文件立即落盘且为空**，旧文件保留。
    #[tokio::test]
    async fn new_session_swaps_session_and_refreshes_view() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let before = take_view(&mut msgs);
        assert_eq!(before.message_count, 2, "user + assistant");
        let old_path = before.session_path.clone().expect("持久会话应有文件");
        assert!(old_path.exists(), "回合结束后会话文件应落盘");

        h.requests.send(Request::NewSession).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let after = take_view(&mut msgs);
        assert_eq!(after.message_count, 0, "换会话后消息数归零");
        let new_path = after.session_path.clone().expect("持久会话应有文件");
        assert_ne!(old_path, new_path, "应换到新会话文件");
        assert!(old_path.exists(), "旧会话文件保留");
        assert!(new_path.exists(), "/new 应立即落盘空文件");
        assert!(
            std::fs::read_to_string(&new_path).unwrap().is_empty(),
            "预建的新会话文件应为空"
        );
    }

    /// 6. **穷尽 `Request` 变体**：7 个变体各发一次，app 循环都必须处理并给出出站
    ///    消息、不得 panic（`Prompt` → 事件；6 个能力 → `Output` + `View`）。
    #[tokio::test]
    async fn every_request_variant_is_handled() {
        let model = MockModel::new("mock");
        model.push_text("hi");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;
        let export_path = h.temp_path("exported.jsonl");

        // `Prompt` 之外的 6 个变体 —— 与 `Request` 一一对应（漏一个就少一条断言）。
        let cases = vec![
            Request::SetModel {
                model: "gpt-x".into(),
            },
            Request::Login {
                provider: "custom".into(),
                api_key: "sk-test".into(),
                // `custom` 没有内置 base —— 走 UI 追问到的那条路（`Some`）。
                api_base: Some("https://custom.example.com".into()),
            },
            Request::Logout,
            Request::NewSession,
            Request::Compact,
            Request::Export {
                path: Some(export_path.clone()),
            },
        ];
        assert_eq!(cases.len(), 6, "Request 共 7 个变体：6 个能力 + Prompt");

        // `Prompt` 先跑：会话有内容，`/export` 才有东西可导。
        h.requests.send(prompt("hello")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            msgs.iter().all(|m| matches!(m, Outbound::Event(_))),
            "Prompt 期间只应有事件：{msgs:?}"
        );

        for req in cases {
            let label = format!("{req:?}");
            h.requests.send(req).await.unwrap();
            let mut msgs = collect_until_view(&mut h).await;
            let _ = take_view(&mut msgs);
            match msgs.as_slice() {
                [Outbound::Output(_)] => {}
                other => panic!("{label} 的出站序列应恰为 [Output]：{other:?}"),
            }
        }

        assert!(
            export_path.exists(),
            "/export 应真的把会话文件复制到目标：{}",
            export_path.display()
        );
    }

    /// 7. **UI 退出**：drop `request_tx` → app 循环在超时内**正常返回**（不挂起）。
    #[tokio::test]
    async fn dropping_request_sender_shuts_loop_down() {
        let model = MockModel::new("mock");
        model.push_text("bye");
        let mut h = spawn_with_mock(model).await;
        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        // 拆包，好把发送端 drop 掉（`Harness` 故意没有 `Drop`）。
        let Harness {
            requests,
            boundaries,
            outbound,
            mut handle,
            tmp,
        } = h;
        drop(requests);
        drop(boundaries);
        drop(outbound);

        let joined = tokio::time::timeout(TIMEOUT, &mut handle)
            .await
            .expect("请求信关闭后 app 循环应立即收摊（不得挂起）")
            .expect("app 循环任务不应 panic");
        assert!(joined.is_ok(), "正常收摊应为 Ok：{joined:?}");
        drop(tmp);
    }

    /// **空闲期丢弃残留边界消息**：循环开跑前就躺在边界信道里的 `Abort` 不得把
    /// 第一个回合取消。
    ///
    /// # 为什么要门控（它曾经是个时间赌博）
    ///
    /// 不加门控时 `select!` 的分支选择是**随机**的：`turn` 完全可能在
    /// `boundary_rx` 分支被评估之前就跑完（`MockModel` 一次 poll 即 Ready），
    /// 残留的 `Abort` 于是**碰巧**没被泵进本回合 —— 删掉排空逻辑也未必变红
    /// （实测：改坏后连跑 12 次只抓到 10 次）。
    ///
    /// 现在第 1 次 `complete` 被 gate 卡住：`turn` 在首次 poll 必然 `Pending`，
    /// `select!` 因此**必定**在同一趟 poll 里评估 `boundary_rx` 分支 ——
    /// 「残留 Abort 有没有泄漏进本回合」不再取决于调度顺序。
    ///
    /// 泄漏一旦发生，`BasicLoop` 会在回合入口探针（`rounds: 0`）或第 2 轮的
    /// 轮边界探针（`rounds: 1`）看到它，回合以 `Cancelled` 收场 ——
    /// [`ProbePlan::ToolCallThenFinal`] 特地为后者跨过一个轮边界。故断言 `Completed`。
    ///
    /// 变异：删掉循环顶部的 `while boundary_rx.try_recv().is_ok() {}` → 本测试
    /// **确定性**变红（回合以 `Cancelled` 收场）。
    #[tokio::test]
    async fn stale_abort_queued_before_first_turn_is_discarded() {
        let (model, mut probe) = probe_model(ProbePlan::ToolCallThenFinal);
        let mut h = spawn_app_loop(Box::new(model), false, vec![Boundary::Abort]).await;

        h.requests.send(prompt("hi")).await.unwrap();

        // 等第 1 次 model 调用开始（此刻首次 select 已把所有分支评估完毕，残留
        // Abort 是否泄漏已成定局），然后放行。
        //
        // **等不到也无妨**：残留 Abort 若在回合入口就被命中，model 永不被调用，
        // 那时回合已经结束 —— 照样放行（多余的 permit 无害，model 不会再被调用），
        // 结论交给下面的断言。`ENTRY_WAIT` 只决定这条失败路径有多快，
        // **不会**把「等不到 model」变成另一种失败形态（那曾让失败信息变得难懂）。
        let _ = tokio::time::timeout(ENTRY_WAIT, probe.wait_entered(1)).await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Completed],
            "残留的 Abort 不该影响本回合：{msgs:?}"
        );
    }
}
