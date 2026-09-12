//! 有界信道 `EventSink`（接线器唯一持有 `tokio::mpsc` 的地方）。
//!
//! 把 `AgentEvent` 包成 [`Envelope`] 投进有界 `mpsc`。契约见
//! `docs/arch/gap-closure/design-core-channel.md` §3「移植 1」：
//! `try_emit` 同步快路径（满则内部缓冲，**不失败**），`emit` 异步慢路径
//! （先冲 overflow，再发本次；这是背压点）。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{SendError, TrySendError};

use ys_channel::{Envelope, LifecyclePolicy, Source};
use ys_core::EventError;
use ys_event::{AgentEvent, EventSink};

/// 背压可观测读数（设计修订 R6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelStats {
    /// `try_send` 撞满次数（backpressure 次数）。
    pub backpressure_waits: u64,
    /// 当前 overflow 深度（尚未投出的缓冲事件数）。
    pub buffered: usize,
    /// 消费者是否已消失。
    pub consumer_gone: bool,
}

/// 有界信道的 `EventSink`：把 `AgentEvent` 包成 `Envelope` 投递。
///
/// overflow 缓冲**长生命周期**（随 sink，不随 `Forwarder` drop），
/// 由每次 `emit().await` 自动冲掉，故撞满时快路径不失败。
#[allow(dead_code)] // 接线（main 装配）在后续任务，暂未接入
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
    // stats 计数
    backpressure_waits: u64,
    consumer_gone: bool,
}

#[allow(dead_code)] // 接线（main 装配）在后续任务，暂未接入
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
                backpressure_waits: 0,
                consumer_gone: false,
            },
            rx,
        )
    }

    /// 私有：把事件包成信封（来源 + 当前回合号）。
    #[inline]
    fn wrap(&self, event: AgentEvent) -> Envelope {
        Envelope::new(self.source.clone(), self.turn, event)
    }

    /// 顺手冲积压：同步 `try_send` 循环，按序尽力发送。
    /// 撞满即把**整个信封**放回队首并停止（保序）；消费者消失则记标记并停止。
    fn drain_overflow_best_effort(&mut self) {
        while let Some(env) = self.overflow.pop_front() {
            match self.tx.try_send(env) {
                Ok(()) => {}
                Err(TrySendError::Full(env)) => {
                    // 放回队首，保序；下次再冲
                    self.overflow.push_front(env);
                    break;
                }
                Err(TrySendError::Closed(env)) => {
                    // 消费者消失：保留事件（Stop 语义下不应静默丢弃），停止冲刷
                    self.consumer_gone = true;
                    self.overflow.push_front(env);
                    break;
                }
            }
        }
    }

    /// 按生命周期策略处置「消费者消失」。
    #[inline]
    fn on_consumer_gone(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        self.consumer_gone = true;
        match self.policy {
            LifecyclePolicy::ContinueWithoutConsumer => Ok(()), // 丢弃，继续跑
            // StopWhenConsumerGone 及未来新增变体：交回循环 → 终止
            // （`#[non_exhaustive]` 强制此处兜底；保守起见不静默丢弃）
            _ => Err(event),
        }
    }

    /// 背压可观测读数。
    pub fn stats(&self) -> ChannelStats {
        ChannelStats {
            backpressure_waits: self.backpressure_waits,
            buffered: self.overflow.len(),
            consumer_gone: self.consumer_gone,
        }
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
                self.backpressure_waits += 1;
                self.overflow.push_back(env);
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
            while let Some(env) = self.overflow.pop_front() {
                if let Err(SendError(env)) = self.tx.send(env).await {
                    // 消费者消失：把事件放回队首保留（与 drain 的 Closed 分支一致，
                    // 不静默丢弃），再按 policy 返回。
                    self.consumer_gone = true;
                    self.overflow.push_front(env);
                    return if self.policy == LifecyclePolicy::ContinueWithoutConsumer {
                        Ok(())
                    } else {
                        Err(EventError::SendFailed)
                    };
                }
            }
            // 再发本次事件（用当前回合号包装）
            if let Err(SendError(env)) = self.tx.send(self.wrap(event)).await {
                self.consumer_gone = true;
                // 本次事件同样保留回 overflow，不静默丢弃
                self.overflow.push_front(env);
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

    fn delta(text: &str) -> AgentEvent {
        AgentEvent::ModelTextDelta {
            text: text.to_string(),
        }
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
}
