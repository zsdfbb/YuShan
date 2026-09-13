//! 有界信道 `EventSink`（接线器唯一持有 `tokio::mpsc` 的地方）。
//!
//! 把 `AgentEvent` 包成 [`Envelope`] 投进有界 `mpsc`。契约见
//! `docs/arch/gap-closure/design-core-channel.md` §3「移植 1」：
//! `try_emit` 同步快路径（满则内部缓冲，**不失败**），`emit` 异步慢路径
//! （先冲 overflow，再发本次；这是背压点）。
//!
//! **死锁勘误（实施后）**：自由函数 `ys_event::emit()` 现在**总是**走
//! `emit().await`，不再先试 `try_emit`。若走同步快路径，信道满时终局事件会被
//! 缓冲进 `overflow` 并返回 `Ok`，其后无人再冲刷 → 消费者等不到终局事件 →
//! 死锁，且背压失效。`try_emit` 的唯一消费者是 `Forwarder`（同步回调，不能 await）。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{SendError, TrySendError};

use ys_channel::{Envelope, LifecyclePolicy, Source};
use ys_core::EventError;
use ys_event::{AgentEvent, EventSink};

/// 背压可观测读数（设计修订 R6）。
///
/// 由 [`ChannelSink::stats`] 读取，`-p`/`--json` 收尾时经 [`format_stats`]
/// 渲染到 **stderr**（不污染 stdout 的 JSON / 文本流）。见 `--stats`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelStats {
    /// `try_send` 撞满次数（backpressure 次数）。
    pub backpressure_waits: u64,
    /// 当前 overflow 深度（尚未投出的缓冲事件数）。
    pub buffered: usize,
    /// 消费者是否已消失。
    pub consumer_gone: bool,
}

/// `ChannelStats` 的共享内核：`ChannelSink` 装箱为 `Box<dyn EventSink>` 后，
/// 调用方仍能经 [`ChannelStatsHandle`] 读取读数（Arc 共享 + 原子计数）。
#[derive(Debug, Default)]
struct StatsCell {
    backpressure_waits: AtomicU64,
    buffered: AtomicUsize,
    consumer_gone: AtomicBool,
}

/// 背压读数的跨生命周期句柄：在 `ChannelSink` 被装箱/移交后仍可观测。
///
/// 构造方式：[`ChannelSink::stats_handle`]。每次 [`snapshot`](Self::snapshot)
/// 取当前值（非缓存），故可在 turn 结束后读到终态。
#[derive(Debug, Clone, Default)]
pub struct ChannelStatsHandle {
    cell: Arc<StatsCell>,
}

impl ChannelStatsHandle {
    /// 当前读数快照。
    pub fn snapshot(&self) -> ChannelStats {
        ChannelStats {
            backpressure_waits: self.cell.backpressure_waits.load(Ordering::Relaxed),
            buffered: self.cell.buffered.load(Ordering::Relaxed),
            consumer_gone: self.cell.consumer_gone.load(Ordering::Relaxed),
        }
    }
}

/// 把背压读数渲染为一行 stderr 摘要；三项全为「无事发生」时返回 `None`
/// （正常运行时保持 stderr 干净，不刷屏）。
///
/// 有事发生的判据：`backpressure_waits > 0 || buffered > 0 || consumer_gone`。
pub fn format_stats(s: &ChannelStats) -> Option<String> {
    if s.backpressure_waits == 0 && s.buffered == 0 && !s.consumer_gone {
        return None;
    }
    Some(render_stats(s))
}

/// 无条件渲染读数摘要（`--stats` 显式要求时用，哪怕全零）。
pub fn format_stats_forced(s: &ChannelStats) -> String {
    render_stats(s)
}

/// 渲染本体：固定前缀 + 两个计数；`consumer_gone` 时追加标记。
fn render_stats(s: &ChannelStats) -> String {
    let mut line = format!(
        "[channel] backpressure waits: {}, buffered at end: {}",
        s.backpressure_waits, s.buffered
    );
    if s.consumer_gone {
        line.push_str(", consumer gone");
    }
    line
}

/// 有界信道的 `EventSink`：把 `AgentEvent` 包成 `Envelope` 投递。
///
/// overflow 缓冲**长生命周期**（随 sink，不随 `Forwarder` drop），
/// 由每次 `emit().await` 自动冲掉，故撞满时快路径不失败。
pub struct ChannelSink {
    tx: mpsc::Sender<Envelope>,
    /// 仅 `try_send` 撞满时使用；长生命周期，不随 `Forwarder` drop。
    ///
    /// 存**已包装好的 `Envelope`**（而非裸 `AgentEvent`）：信封携带包装时的
    /// `turn`，冲刷时原样发送，故积压跨回合存活也不会被改写成当前回合号（R4）。
    overflow: VecDeque<Envelope>,
    source: Source,
    turn: u32,
    policy: LifecyclePolicy,
    /// 背压读数（Arc 共享，装箱后仍可经 [`ChannelStatsHandle`] 读取）。
    stats: ChannelStatsHandle,
}

impl ChannelSink {
    /// 新建 sink 与配套接收端。容量由调用方传：
    /// 交互模式（`-p`/`--json`/TUI）建议 1024，后台长任务 4096。
    pub fn new(capacity: usize, policy: LifecyclePolicy) -> (Self, mpsc::Receiver<Envelope>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                overflow: VecDeque::new(),
                source: Source::agent(),
                turn: 0,
                policy,
                stats: ChannelStatsHandle::default(),
            },
            rx,
        )
    }

    /// 私有：把事件包成信封（来源 + 当前回合号）。
    #[inline]
    fn wrap(&self, event: AgentEvent) -> Envelope {
        Envelope::new(self.source.clone(), self.turn, event)
    }

    /// overflow 变更后同步 `buffered` 读数。
    ///
    /// 所有 overflow 写操作都必须经下面三个 `overflow_*` 包装，保证读数不漏更新。
    #[inline]
    fn sync_buffered(&self) {
        self.stats
            .cell
            .buffered
            .store(self.overflow.len(), Ordering::Relaxed);
    }

    #[inline]
    fn overflow_push_back(&mut self, env: Envelope) {
        self.overflow.push_back(env);
        self.sync_buffered();
    }

    #[inline]
    fn overflow_push_front(&mut self, env: Envelope) {
        self.overflow.push_front(env);
        self.sync_buffered();
    }

    #[inline]
    fn overflow_pop_front(&mut self) -> Option<Envelope> {
        let env = self.overflow.pop_front();
        self.sync_buffered();
        env
    }

    /// 顺手冲积压：同步 `try_send` 循环，按序尽力发送。
    /// 撞满即把**整个信封**放回队首并停止（保序）；消费者消失则记标记并停止。
    fn drain_overflow_best_effort(&mut self) {
        while let Some(env) = self.overflow_pop_front() {
            match self.tx.try_send(env) {
                Ok(()) => {}
                Err(TrySendError::Full(env)) => {
                    // 放回队首，保序；下次再冲
                    self.overflow_push_front(env);
                    break;
                }
                Err(TrySendError::Closed(env)) => {
                    // 消费者消失：保留事件（Stop 语义下不应静默丢弃），停止冲刷
                    self.mark_consumer_gone();
                    self.overflow_push_front(env);
                    break;
                }
            }
        }
    }

    /// 标记消费者已消失（读数 + 内部一致性的唯一写点）。
    #[inline]
    fn mark_consumer_gone(&mut self) {
        self.stats.cell.consumer_gone.store(true, Ordering::Relaxed);
    }

    /// 按生命周期策略处置「消费者消失」。
    #[inline]
    fn on_consumer_gone(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.mark_consumer_gone();
        match self.policy {
            LifecyclePolicy::ContinueWithoutConsumer => Ok(()), // 丢弃，继续跑
            // StopWhenConsumerGone 及未来新增变体：交回循环 → 终止
            // （`#[non_exhaustive]` 强制此处兜底；保守起见不静默丢弃）
            _ => Err(event),
        }
    }

    /// 背压可观测读数（持有**具体** sink 时的便捷读取）。
    ///
    /// 仅测试直接调用；生产路径中 sink 已被装箱为 `dyn EventSink`，
    /// 读数经 [`Self::stats_handle`] 的 [`ChannelStatsHandle`] 读取。
    #[allow(dead_code)] // 生产读计数走 stats_handle()，此便捷方法目前仅测试用
    pub fn stats(&self) -> ChannelStats {
        self.stats.snapshot()
    }

    /// 取跨生命周期句柄：`ChannelSink` 装箱为 `dyn EventSink` 后仍可读读数。
    pub fn stats_handle(&self) -> ChannelStatsHandle {
        self.stats.clone()
    }
}

impl EventSink for ChannelSink {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.drain_overflow_best_effort(); // 顺手冲积压（同步 try_send）
        match self.tx.try_send(self.wrap(event)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(env)) => {
                // 撞满 → 缓冲，**不算失败**（契约：满时内部缓冲，不得返回 Err）
                // 存整个信封（含包装时的 turn），冲刷时原样发送
                self.stats
                    .cell
                    .backpressure_waits
                    .fetch_add(1, Ordering::Relaxed);
                self.overflow_push_back(env);
                Ok(())
            }
            Err(TrySendError::Closed(env)) => self.on_consumer_gone(env.event), // 消费者消失
        }
    }

    fn emit<'a>(
        &'a mut self,
        event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>> {
        Box::pin(async move {
            // 先按序冲干 overflow —— 背压点在此；信封原样发送，不重新包装，
            // 故积压事件的 turn 保持包装时的值（不随当前回合漂移）。
            while let Some(env) = self.overflow_pop_front() {
                if let Err(SendError(env)) = self.tx.send(env).await {
                    // 消费者消失：把事件放回队首保留（与 drain 的 Closed 分支一致，
                    // 不静默丢弃），再按 policy 返回。
                    self.mark_consumer_gone();
                    self.overflow_push_front(env);
                    return if self.policy == LifecyclePolicy::ContinueWithoutConsumer {
                        Ok(())
                    } else {
                        Err(EventError::SendFailed)
                    };
                }
            }
            // 再发本次事件（用当前回合号包装）
            if let Err(SendError(env)) = self.tx.send(self.wrap(event)).await {
                self.mark_consumer_gone();
                // 本次事件同样保留回 overflow，不静默丢弃
                self.overflow_push_front(env);
                return if self.policy == LifecyclePolicy::ContinueWithoutConsumer {
                    Ok(())
                } else {
                    Err(EventError::SendFailed)
                };
            }
            Ok(())
        })
    }

    fn begin_turn(&mut self, turn: u32) {
        self.turn = turn;
    }
}

#[cfg(test)]
impl ChannelSink {
    /// 测试专用：查看 overflow 队首（不消费），用于断言「保留而非丢弃」。
    fn overflow_front(&self) -> Option<&Envelope> {
        self.overflow.front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use ys_core::{StopReason, Usage};
    use ys_event::emit;

    fn delta(text: &str) -> AgentEvent {
        AgentEvent::ModelTextDelta {
            text: text.to_string(),
        }
    }

    fn finished() -> AgentEvent {
        AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage::default(),
            rounds: 1,
        }
    }

    fn is_terminal(ev: &AgentEvent) -> bool {
        matches!(
            ev,
            AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
        )
    }

    /// 1. 投递 → 收到 `Envelope`，字段与来源正确。
    #[tokio::test]
    async fn try_emit_delivers_envelope() {
        let (mut sink, mut rx) = ChannelSink::new(4, LifecyclePolicy::StopWhenConsumerGone);
        let e1 = delta("a");
        let e2 = delta("b");
        assert!(sink.try_emit(e1.clone()).is_ok());
        assert!(sink.try_emit(e2.clone()).is_ok());

        let env1 = rx.recv().await.unwrap();
        let env2 = rx.recv().await.unwrap();
        assert_eq!(env1.event, e1);
        assert_eq!(env2.event, e2);
        assert_eq!(env1.source, Source::agent());
        assert_eq!(env2.source, Source::agent());
    }

    /// 2. 满 → 内部缓冲不丢；`try_emit` 全部返回 Ok；`emit` 后按序收全。
    #[tokio::test]
    async fn full_buffers_internally_without_loss() {
        let (mut sink, rx) = ChannelSink::new(2, LifecyclePolicy::StopWhenConsumerGone);
        let events: Vec<AgentEvent> = (0..6).map(|i| delta(&format!("e{i}"))).collect();

        // 容量 2，连续 5 个 → 全部 Ok（不得返回 Err）
        for ev in events.iter().take(5) {
            assert!(sink.try_emit(ev.clone()).is_ok());
        }
        // 2 个在信道缓冲，3 个 overflow
        assert_eq!(sink.stats().buffered, 3);

        // 并发消费者 + 冲积压
        let collector = tokio::spawn(async move {
            let mut rx = rx;
            let mut got = Vec::new();
            for _ in 0..6 {
                got.push(rx.recv().await.unwrap());
            }
            got
        });
        assert!(sink.emit(events[5].clone()).await.is_ok());
        let got = collector.await.unwrap();

        let got_events: Vec<AgentEvent> = got.into_iter().map(|e| e.event).collect();
        assert_eq!(got_events, events); // 顺序与发出顺序一致，且无丢失
    }

    /// 3. `emit` 先冲 overflow：顺序保持，终局事件在最后。
    #[tokio::test]
    async fn emit_flushes_overflow_first() {
        let (mut sink, rx) = ChannelSink::new(1, LifecyclePolicy::StopWhenConsumerGone);
        let events: Vec<AgentEvent> = (0..4).map(|i| delta(&format!("e{i}"))).collect();
        let terminal = delta("terminal");

        for ev in &events {
            assert!(sink.try_emit(ev.clone()).is_ok());
        }
        assert_eq!(sink.stats().buffered, 3); // 1 进信道，3 overflow

        let collector = tokio::spawn(async move {
            let mut rx = rx;
            let mut got = Vec::new();
            for _ in 0..5 {
                got.push(rx.recv().await.unwrap());
            }
            got
        });
        assert!(sink.emit(terminal.clone()).await.is_ok());
        let got = collector.await.unwrap();

        let got_events: Vec<AgentEvent> = got.into_iter().map(|e| e.event).collect();
        let expected: Vec<AgentEvent> = events
            .iter()
            .cloned()
            .chain(std::iter::once(terminal))
            .collect();
        assert_eq!(got_events, expected);
        assert_eq!(got_events.last().unwrap(), &delta("terminal")); // 终局事件在最后
    }

    /// 4. 接收端关闭 + `StopWhenConsumerGone` → `Err(event)`，载荷原样退回。
    #[tokio::test]
    async fn closed_stop_returns_event() {
        let (mut sink, rx) = ChannelSink::new(4, LifecyclePolicy::StopWhenConsumerGone);
        drop(rx);

        let ev = delta("gone");
        let returned = sink.try_emit(ev.clone()).unwrap_err();
        assert_eq!(returned, ev);
        assert!(sink.stats().consumer_gone);
    }

    /// 5. 接收端关闭 + `ContinueWithoutConsumer` → `Ok(())`（吞掉），并记标记。
    #[tokio::test]
    async fn closed_continue_swallows() {
        let (mut sink, rx) = ChannelSink::new(4, LifecyclePolicy::ContinueWithoutConsumer);
        drop(rx);

        assert!(sink.try_emit(delta("gone")).is_ok());
        assert!(sink.stats().consumer_gone);
    }

    /// 6. `begin_turn` 生效：信封携带显式回合号。
    #[tokio::test]
    async fn begin_turn_sets_envelope_turn() {
        let (mut sink, mut rx) = ChannelSink::new(4, LifecyclePolicy::StopWhenConsumerGone);
        sink.begin_turn(3);
        assert!(sink.try_emit(delta("t")).is_ok());

        let env = rx.recv().await.unwrap();
        assert_eq!(env.turn, 3);
    }

    /// 7. 撞满后 `backpressure_waits` > 0。
    #[tokio::test]
    async fn backpressure_waits_counted() {
        let (mut sink, _rx) = ChannelSink::new(1, LifecyclePolicy::StopWhenConsumerGone);
        for i in 0..4 {
            assert!(sink.try_emit(delta(&format!("e{i}"))).is_ok());
        }
        let stats = sink.stats();
        assert!(stats.backpressure_waits > 0);
        assert_eq!(stats.backpressure_waits, 3); // 首个占满信道，其余 3 次撞满
        assert_eq!(stats.buffered, 3);
    }

    /// 8. 跨回合保真：overflow 事件按**包装时**的 turn 原样投出，
    ///    不会被冲刷时的当前回合号改写（R4）。
    #[tokio::test]
    async fn overflow_preserves_turn_across_turns() {
        let (mut sink, rx) = ChannelSink::new(1, LifecyclePolicy::StopWhenConsumerGone);
        sink.begin_turn(1);
        assert!(sink.try_emit(delta("A")).is_ok()); // 入信道
        assert!(sink.try_emit(delta("B")).is_ok()); // 撞满 → 入 overflow（turn=1）
        assert_eq!(sink.stats().buffered, 1);

        sink.begin_turn(2); // 模拟进入下一回合

        let collector = tokio::spawn(async move {
            let mut rx = rx;
            let mut got = Vec::new();
            for _ in 0..3 {
                got.push(rx.recv().await.unwrap());
            }
            got
        });
        assert!(sink.emit(delta("C")).await.is_ok()); // 先冲 B，再发 C
        let got = collector.await.unwrap();

        assert_eq!(got[0].event, delta("A"));
        assert_eq!(got[0].turn, 1);
        assert_eq!(got[1].event, delta("B"));
        assert_eq!(
            got[1].turn, 1,
            "overflow 事件应保留包装时的 turn，而非冲刷时的当前 turn"
        );
        assert_eq!(got[2].event, delta("C"));
        assert_eq!(got[2].turn, 2);
    }

    /// 9. `emit` 冲刷失败（消费者消失）时事件保留回 overflow，不静默丢弃。
    #[tokio::test]
    async fn emit_flush_failure_retains_event() {
        let (mut sink, rx) = ChannelSink::new(1, LifecyclePolicy::ContinueWithoutConsumer);
        assert!(sink.try_emit(delta("A")).is_ok()); // 入信道
        assert!(sink.try_emit(delta("B")).is_ok()); // 撞满 → 入 overflow
        assert_eq!(sink.stats().buffered, 1);

        drop(rx); // 消费者消失

        assert!(sink.emit(delta("C")).await.is_ok()); // Continue → Ok
        assert!(sink.stats().consumer_gone);
        // 冲刷失败的 B 被保留（与 drain 的 Closed 分支一致），未被丢弃
        assert_eq!(sink.stats().buffered, 1);
        let retained = sink.overflow_front().expect("overflow 应保留失败事件");
        assert_eq!(retained.event, delta("B"));
        assert_eq!(retained.turn, 0);
    }

    /// **死锁回归（关键）**：小容量下经**自由函数** `emit`（总走慢路径）
    /// 连续投递多于容量的事件，含终局事件在最后，并发消费。
    ///
    /// 断言：不挂起（timeout）、全部按序送达、无丢失、终局事件在最后。
    ///
    /// 旧实现下本测试必死锁：自由函数先试 `try_emit`，信道满时把事件
    /// （含终局事件）缓冲进 `overflow` 并返回 `Ok`。`try_emit` 不 yield，
    /// 生产者一口气跑完；此后无人再冲刷 `overflow`，消费者永远等不到终局事件。
    async fn assert_no_deadlock_at_capacity(capacity: usize, deltas: usize) {
        let (mut sink, mut rx) = ChannelSink::new(capacity, LifecyclePolicy::StopWhenConsumerGone);

        let events: Vec<AgentEvent> = (0..deltas).map(|i| delta(&format!("e{i}"))).collect();
        let terminal = finished();

        // 并发消费者：收到终局事件即退出（与 main 的 consume_events 同义）。
        let consumer = tokio::spawn(async move {
            let mut got = Vec::new();
            while let Some(env) = rx.recv().await {
                let term = is_terminal(&env.event);
                got.push(env.event);
                if term {
                    break;
                }
            }
            got
        });

        // 生产者：事件数 > 容量，最后一个是终局事件。
        let produce = async {
            for ev in events.iter().cloned() {
                emit(&mut sink, ev).await.unwrap();
            }
            emit(&mut sink, terminal.clone()).await.unwrap();
        };
        let produced = tokio::time::timeout(Duration::from_secs(5), produce).await;
        assert!(produced.is_ok(), "capacity={capacity} 连续 emit 不得挂起");

        let got = tokio::time::timeout(Duration::from_secs(5), consumer)
            .await
            .expect("消费者应收到终局事件，不得挂起")
            .unwrap();

        let expected: Vec<AgentEvent> = events
            .iter()
            .cloned()
            .chain(std::iter::once(terminal))
            .collect();
        assert_eq!(got, expected, "事件应按序送达、无丢失");
        assert!(is_terminal(got.last().unwrap()), "终局事件必须在最后");
    }

    #[tokio::test]
    async fn small_channel_no_deadlock_capacity_1() {
        assert_no_deadlock_at_capacity(1, 5).await;
    }

    #[tokio::test]
    async fn small_channel_no_deadlock_capacity_2() {
        assert_no_deadlock_at_capacity(2, 6).await;
    }

    /// 自由函数走慢路径时背压确实生效：无消费者时，投递多于容量后
    /// overflow 不再无界增长（生产者阻塞在 `send().await`，而非无限缓冲）。
    ///
    /// 用 `timeout` 证明生产者**确实被阻塞**（这正是背压），
    /// 而非「假成功」地继续把事件塞进 overflow。
    #[tokio::test]
    async fn slow_path_applies_backpressure_when_consumer_stalls() {
        let (mut sink, _rx) = ChannelSink::new(2, LifecyclePolicy::StopWhenConsumerGone);
        // 接收端不消费（仅持有）。容量 2 填满后，第 3 次 `emit` 应阻塞。
        let send_third = async {
            for i in 0..3 {
                let _ = emit(&mut sink, delta(&format!("e{i}"))).await;
            }
        };
        let r = tokio::time::timeout(Duration::from_millis(200), send_third).await;
        assert!(
            r.is_err(),
            "容量 2 且无消费者时，第 3 次 emit 应阻塞在背压点，而非无限缓冲"
        );
        // 阻塞期间前两个事件在信道、无 overflow 积压
        assert_eq!(sink.stats().buffered, 0, "背压生效时不应有 overflow 积压");
        assert_eq!(
            sink.stats().backpressure_waits,
            0,
            "慢路径不增快路径撞满计数"
        );
    }

    /// 10. `format_stats`：三项全零 → `None`（不输出，保持 stderr 干净）。
    #[test]
    fn format_stats_all_zero_is_none() {
        let s = ChannelStats {
            backpressure_waits: 0,
            buffered: 0,
            consumer_gone: false,
        };
        assert_eq!(format_stats(&s), None);
    }

    /// 11. 有背压 → 含 `backpressure` 字样与具体数字。
    #[test]
    fn format_stats_reports_backpressure_with_numbers() {
        let s = ChannelStats {
            backpressure_waits: 3,
            buffered: 0,
            consumer_gone: false,
        };
        let line = format_stats(&s).expect("有背压时应输出");
        assert_eq!(line, "[channel] backpressure waits: 3, buffered at end: 0");
        assert!(line.contains("backpressure"), "line = {line}");
        assert!(line.contains('3'), "line = {line}");
    }

    /// 12. 仅 `buffered > 0`（没撞满但仍有积压）也要输出。
    #[test]
    fn format_stats_reports_buffered_only() {
        let s = ChannelStats {
            backpressure_waits: 0,
            buffered: 2,
            consumer_gone: false,
        };
        let line = format_stats(&s).expect("有积压时应输出");
        assert!(line.contains("buffered at end: 2"), "line = {line}");
    }

    /// 13. `consumer_gone` → 含相应字样。
    #[test]
    fn format_stats_reports_consumer_gone() {
        let s = ChannelStats {
            backpressure_waits: 0,
            buffered: 0,
            consumer_gone: true,
        };
        let line = format_stats(&s).expect("消费者消失时应输出");
        assert!(line.contains("consumer gone"), "line = {line}");
    }

    /// 14. `format_stats_forced`：全零也输出（`--stats` 显式要求时用）。
    #[test]
    fn format_stats_forced_always_renders() {
        let s = ChannelStats {
            backpressure_waits: 0,
            buffered: 0,
            consumer_gone: false,
        };
        assert_eq!(
            format_stats_forced(&s),
            "[channel] backpressure waits: 0, buffered at end: 0"
        );
    }

    /// 15. **接线关键不变量**：`ChannelSink` 装箱/移交后，句柄仍能读到终态读数。
    #[tokio::test]
    async fn stats_handle_observes_after_boxing() {
        let (mut sink, _rx) = ChannelSink::new(1, LifecyclePolicy::StopWhenConsumerGone);
        let handle = sink.stats_handle();
        for i in 0..3 {
            assert!(sink.try_emit(delta(&format!("e{i}"))).is_ok());
        }

        // 装箱（生产路径：`Wiring::ephemeral(model, Box::new(sink))`）并 drop。
        let boxed: Box<dyn EventSink> = Box::new(sink);
        drop(boxed);

        let s = handle.snapshot();
        assert_eq!(s.backpressure_waits, 2, "容量 1、投 3 个应撞满 2 次");
        assert_eq!(s.buffered, 2, "撞满的事件仍在 overflow 中");
    }
}
