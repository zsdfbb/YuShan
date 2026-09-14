use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use ys_core::Message;

/// 轮边界上的控制消息。
///
/// UI → `BasicLoop`，在**轮**边界被拉取（设计 §8 信道 ②）。
/// `Abort` 取代了旧的 `CancelToken`：取消不再是一个跨线程原子，而是
/// 队列里的一条消息 —— 因此「取消」与「插话」共享同一条有序通道。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Boundary {
    /// 掌舵：运行期间的注入，追加进会话后进入本轮 `ModelRequest`。
    Steer(Message),
    /// 中止：当前 turn 以 `StopReason::Cancelled` 收场。
    Abort,
}

/// 轮边界控制器 —— `BasicLoop` 在轮边界看到的唯一接口。
///
/// **零 tokio**：本 trait 只描述「取一条 / 看一眼」，不指定传输方式；
/// mpsc 实现在 app 侧（接线器），拉取由 agent 线程同步完成。
///
/// 两个方法都是 `&self`（内可变）：`BasicLoop` 经 `&dyn BoundarySource` 访问，
/// 而生产者可在 agent 运行期间投递。
pub trait BoundarySource: Send + Sync {
    /// 轮边界：非阻塞拉取下一条边界消息（`Steer` 或 `Abort`），保持入队顺序。
    ///
    /// 无消息时返回 `None`（不阻塞 —— 轮边界是轮询点，不是等待点）。
    fn take(&self) -> Option<Boundary>;

    /// 非破坏性探针：是否已收到 `Abort`（供模型调用前 / 工具轮询）。
    ///
    /// 可重复调用；与 `take` 互不影响（`take` 不会清除该标记）。
    fn is_aborted(&self) -> bool;
}

struct Inner {
    queue: VecDeque<Boundary>,
    aborted: bool,
}

/// [`BoundarySource`] 的唯一具体实现：纯内存队列。
///
/// 生产者（接线器）`push` 入队，消费者（`BasicLoop`）`take` 取出 ——
/// 均为 `&self`，因为 `Mutex` 给了内可变。跨线程真实 mpsc 适配由 app 侧
/// 的 pump 线程完成（把 `recv` 到的边界 `push` 进来）。
///
/// 选 `std::sync::Mutex` 而非 tokio 锁：本 crate 零 tokio，且临界区
/// 只有队列操作，不跨 await。
pub struct QueueBoundarySource {
    inner: Mutex<Inner>,
}

impl QueueBoundarySource {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                aborted: false,
            }),
        }
    }

    /// 锁中毒时取回内部数据（照 `ys-core` / 原 `Inbox` 既有风格）。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 投递一条边界消息（可在 agent 运行期间调用）。
    ///
    /// `Abort` **同时**置 `aborted` 标记并**照常入队** —— 保序，且 `Abort`
    /// 只在整个 turn 真正结束时才终结；`take` 仍会把它交还给消费者。
    pub fn push(&self, boundary: Boundary) {
        let mut inner = self.lock();
        if matches!(boundary, Boundary::Abort) {
            inner.aborted = true;
        }
        inner.queue.push_back(boundary);
    }

    /// 非破坏性探针（同 trait 方法；具体类型上也有一份，免去 dyn 转换）。
    pub fn is_aborted(&self) -> bool {
        self.lock().aborted
    }

    /// 队列是否为空。
    pub fn is_empty(&self) -> bool {
        self.lock().queue.is_empty()
    }
}

impl Default for QueueBoundarySource {
    fn default() -> Self {
        Self::new()
    }
}

impl BoundarySource for QueueBoundarySource {
    fn take(&self) -> Option<Boundary> {
        self.lock().queue.pop_front()
    }

    fn is_aborted(&self) -> bool {
        self.lock().aborted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::{ContentBlock, Role};

    fn steer_msg(text: &str) -> Boundary {
        Boundary::Steer(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        })
    }

    #[test]
    fn test_empty_source_returns_none_and_not_aborted() {
        let src = QueueBoundarySource::new();
        assert!(src.is_empty());
        assert!(src.take().is_none());
        assert!(!src.is_aborted());
    }

    #[test]
    fn test_take_preserves_fifo_order() {
        let src = QueueBoundarySource::new();
        src.push(steer_msg("a"));
        src.push(steer_msg("b"));
        src.push(Boundary::Abort);

        assert_eq!(src.take(), Some(steer_msg("a")), "先入先出");
        assert_eq!(src.take(), Some(steer_msg("b")));
        assert_eq!(src.take(), Some(Boundary::Abort));
        assert_eq!(src.take(), None, "取空后为 None");
    }

    #[test]
    fn test_push_abort_sets_flag_immediately() {
        let src = QueueBoundarySource::new();
        assert!(!src.is_aborted());
        src.push(Boundary::Abort);
        assert!(src.is_aborted(), "Abort 入队即置位（不必等到 take）");
    }

    #[test]
    fn test_is_aborted_is_non_destructive() {
        let src = QueueBoundarySource::new();
        src.push(Boundary::Abort);
        // 反复探针不消耗消息
        assert!(src.is_aborted());
        assert!(src.is_aborted());
        assert!(!src.is_empty(), "探针不得取走队列内容");
        assert_eq!(src.take(), Some(Boundary::Abort), "Abort 仍可取回");
    }

    #[test]
    fn test_steer_does_not_set_abort_flag() {
        let src = QueueBoundarySource::new();
        src.push(steer_msg("steer"));
        assert!(!src.is_aborted(), "Steer 不置 abort 标记");
        assert_eq!(src.take(), Some(steer_msg("steer")));
    }

    #[test]
    fn test_steer_then_abort_order_and_flag() {
        // brief 的语义要点：push(Steer) 后 push(Abort) → take 先 Steer 再 Abort
        let src = QueueBoundarySource::new();
        src.push(steer_msg("steer"));
        src.push(Boundary::Abort);
        assert!(src.is_aborted(), "Abort 入队后立即可见");
        assert_eq!(src.take(), Some(steer_msg("steer")));
        assert_eq!(src.take(), Some(Boundary::Abort));
    }

    #[test]
    fn test_producer_push_through_shared_borrow() {
        // 证明内可变：持 &QueueBoundarySource 时仍能 push（生产者与消费者并存的前提）
        let src = QueueBoundarySource::new();
        let producer = &src;
        producer.push(Boundary::Abort);
        let consumer: &dyn BoundarySource = &src;
        assert!(consumer.is_aborted());
        assert_eq!(consumer.take(), Some(Boundary::Abort));
    }

    #[test]
    fn test_steer_abort_steer_full_fifo_order() {
        // Steer(a) → Abort → Steer(b)：三者全程按入队顺序交还，且 Abort 入队即置位
        let src = QueueBoundarySource::new();
        src.push(steer_msg("a"));
        assert!(!src.is_aborted(), "仅 Steer 时未 abort");
        src.push(Boundary::Abort);
        assert!(src.is_aborted(), "Abort 入队后 is_aborted 立即为 true");
        src.push(steer_msg("b"));

        assert_eq!(src.take(), Some(steer_msg("a")), "FIFO：先 Steer(a)");
        assert_eq!(src.take(), Some(Boundary::Abort), "再 Abort");
        assert_eq!(src.take(), Some(steer_msg("b")), "最后 Steer(b)");
        assert_eq!(src.take(), None, "取空后为 None");
        // 从队列取走 Abort 不改变已置位的标记
        assert!(
            src.is_aborted(),
            "标记随 Abort 入队永久置位，不随 take 清除"
        );
    }

    #[test]
    fn test_abort_then_steer_abort_still_first() {
        // Abort 先入：标记立即 true，但 FIFO 仍先交还 Abort 再 Steer
        let src = QueueBoundarySource::new();
        src.push(Boundary::Abort);
        assert!(src.is_aborted(), "Abort 入队即置位");
        src.push(steer_msg("later"));

        assert_eq!(src.take(), Some(Boundary::Abort), "FIFO：Abort 在前");
        assert_eq!(src.take(), Some(steer_msg("later")), "Steer 在后");
        assert_eq!(src.take(), None);
    }

    #[test]
    fn test_is_aborted_repeated_calls_do_not_consume_queue() {
        // 反复探针不得消耗队列：之后仍能取到全部排队项
        let src = QueueBoundarySource::new();
        src.push(steer_msg("x"));
        src.push(Boundary::Abort);
        for _ in 0..5 {
            assert!(src.is_aborted());
        }
        assert_eq!(src.take(), Some(steer_msg("x")));
        assert_eq!(src.take(), Some(Boundary::Abort));
        assert_eq!(src.take(), None, "探针未吃掉任何排队的消息");
    }

    #[test]
    fn test_boundary_source_object_is_send_sync() {
        // 两个方法都是 &self：trait 对象可 Send + Sync，供 &dyn BoundarySource 跨线程共享
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn BoundarySource>();
    }
}
