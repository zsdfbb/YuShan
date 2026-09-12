use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use ys_core::{Message, Role};

/// 入站消息的队列归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// 掌舵：agent 运行期间的注入，在**轮**边界被拉取。
    Steering,
    /// 跟进：agent 空闲后追加的输入，在**回合**边界被拉取。
    FollowUp,
}

/// 队列清空粒度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// 取走该队列全部 → 合并为**一条** Message（多个 `ContentBlock` 依序），单回合处理。
    All,
    /// 一次取一条 → 每条一回合（默认）。
    #[default]
    OneAtATime,
}

struct Inner {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    closed: bool,
}

/// 入站队列：**只装「尚未处理」的消息**。
///
/// 与「队列 = 日志 + 游标」的关系：pending（本结构）+ 会话历史（`Session`）
/// **合起来**才是那条日志；「消费」= 把 pending 移入会话（游标前移）。
/// 是**转移**，不是拷贝——故不存在消息重复持有。
///
/// 内可变（`Arc<Mutex<Inner>>`）：接线器需在 agent 运行期间投递（steering），
/// 故 `push` 取 `&self`。`std::sync::Mutex` 属 std，本 crate 保持运行时无关。
#[derive(Clone)]
pub struct Inbox {
    inner: Arc<Mutex<Inner>>,
}

impl Inbox {
    /// 两队列均为 [`QueueMode::OneAtATime`]。
    pub fn new() -> Self {
        Self::with_modes(QueueMode::OneAtATime, QueueMode::OneAtATime)
    }

    /// 指定两个队列各自的清空粒度。
    pub fn with_modes(steering: QueueMode, follow_up: QueueMode) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                steering: VecDeque::new(),
                follow_up: VecDeque::new(),
                steering_mode: steering,
                follow_up_mode: follow_up,
                closed: false,
            })),
        }
    }

    /// 锁中毒时取回内部数据（照 coding-agent `prompt.rs` 的 `ENV_LOCK` 先例）。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // —— 生产者侧（接线器；可在 agent 运行中调用）——

    /// 投递一条消息。`closed` 后**静默丢弃**（不 panic、不报错）。
    pub fn push(&self, message: Message, kind: Intent) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        match kind {
            Intent::Steering => inner.steering.push_back(message),
            Intent::FollowUp => inner.follow_up.push_back(message),
        }
    }

    /// 关闭入站：此后 `push` 静默丢弃。已入队的消息仍可被 `take_*` 取走。
    pub fn close(&self) {
        self.lock().closed = true;
    }

    // —— 消费者侧（agent）——

    /// 轮边界：按 `steering_mode` 取一批。取走即移出队列。
    pub fn take_steering(&self) -> Vec<Message> {
        let mut inner = self.lock();
        let mode = inner.steering_mode;
        take_from(&mut inner.steering, mode)
    }

    /// 回合边界：按 `follow_up_mode` 取一批。取走即移出队列。
    pub fn take_followup(&self) -> Vec<Message> {
        let mut inner = self.lock();
        let mode = inner.follow_up_mode;
        take_from(&mut inner.follow_up, mode)
    }

    pub fn is_empty(&self) -> bool {
        let inner = self.lock();
        inner.steering.is_empty() && inner.follow_up.is_empty()
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// 便于测试 / 断言。
    pub fn steering_mode(&self) -> QueueMode {
        self.lock().steering_mode
    }

    /// 便于测试 / 断言。
    pub fn follow_up_mode(&self) -> QueueMode {
        self.lock().follow_up_mode
    }
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 按粒度取走队列内容。`take_*` 只在锁内调用，无 await、不跨 await 持锁。
fn take_from(queue: &mut VecDeque<Message>, mode: QueueMode) -> Vec<Message> {
    match mode {
        QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
        QueueMode::All => {
            if queue.is_empty() {
                return Vec::new();
            }
            // 取走全部并合并为一条：各源消息的块依序推入（Text 与 ToolUse/ToolResult
            // 均按原样保留），role 固定为 User。
            let mut content = Vec::new();
            for msg in queue.drain(..) {
                content.extend(msg.content);
            }
            vec![Message {
                role: Role::User,
                content,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::ContentBlock;

    fn text_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn test_queue_mode_default_is_one_at_a_time() {
        assert_eq!(QueueMode::default(), QueueMode::OneAtATime);
    }

    #[test]
    fn test_empty_takes_return_empty() {
        let inbox = Inbox::new();
        assert!(inbox.is_empty());
        assert!(inbox.take_steering().is_empty());
        assert!(inbox.take_followup().is_empty());
    }

    #[test]
    fn test_queues_are_separated() {
        let inbox = Inbox::new();
        inbox.push(text_msg("steer"), Intent::Steering);
        inbox.push(text_msg("follow"), Intent::FollowUp);
        assert!(!inbox.is_empty());

        // 取 steering 不影响 followUp，反之亦然。
        let steering = inbox.take_steering();
        assert_eq!(steering.len(), 1);
        assert_eq!(
            steering[0].content,
            vec![ContentBlock::Text {
                text: "steer".into()
            }]
        );
        assert!(!inbox.is_empty(), "followUp 应仍在队列");

        let follow = inbox.take_followup();
        assert_eq!(follow.len(), 1);
        assert_eq!(
            follow[0].content,
            vec![ContentBlock::Text {
                text: "follow".into()
            }]
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn test_one_at_a_time_takes_one_per_call() {
        let inbox = Inbox::new();
        for t in ["a", "b", "c"] {
            inbox.push(text_msg(t), Intent::Steering);
        }
        for t in ["a", "b", "c"] {
            let batch = inbox.take_steering();
            assert_eq!(batch.len(), 1, "OneAtATime 每次只取一条");
            assert_eq!(
                batch[0].content,
                vec![ContentBlock::Text { text: t.into() }],
                "FIFO 顺序"
            );
        }
        assert!(inbox.take_steering().is_empty(), "第 4 次应为空");
    }

    #[test]
    fn test_all_merges_into_one_message_in_order() {
        let inbox = Inbox::with_modes(QueueMode::All, QueueMode::OneAtATime);
        for t in ["a", "b", "c"] {
            inbox.push(text_msg(t), Intent::Steering);
        }
        let batch = inbox.take_steering();
        assert_eq!(batch.len(), 1, "All 合并为一条 Message");
        assert_eq!(batch[0].role, Role::User);
        assert_eq!(
            batch[0].content,
            vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Text { text: "b".into() },
                ContentBlock::Text { text: "c".into() },
            ],
            "3 个 Text 块，顺序正确"
        );
        assert!(inbox.take_steering().is_empty(), "已全部取走");
    }

    #[test]
    fn test_all_preserves_non_text_blocks() {
        let inbox = Inbox::with_modes(QueueMode::All, QueueMode::OneAtATime);
        inbox.push(text_msg("head"), Intent::Steering);
        inbox.push(
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: ys_core::ToolCallId("tc-1".into()),
                    content: "out".into(),
                    is_error: false,
                }],
            },
            Intent::Steering,
        );
        let batch = inbox.take_steering();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].content.len(), 2);
        assert!(matches!(batch[0].content[0], ContentBlock::Text { .. }));
        assert!(matches!(
            batch[0].content[1],
            ContentBlock::ToolResult { .. }
        ));
    }

    #[test]
    fn test_push_works_through_shared_borrow() {
        // 证明内可变：在已有 &Inbox 借用时仍能 push（steering 的前提）。
        let inbox = Inbox::new();
        let a = inbox.clone();
        let b = &inbox;
        a.push(text_msg("x"), Intent::Steering);
        let taken = b.take_steering();
        assert_eq!(taken.len(), 1);
    }

    #[test]
    fn test_close_marks_closed_and_drops_later_pushes() {
        let inbox = Inbox::new();
        assert!(!inbox.is_closed());
        inbox.push(text_msg("before"), Intent::Steering);
        inbox.close();
        assert!(inbox.is_closed());

        // closed 后 push 静默丢弃
        inbox.push(text_msg("after"), Intent::Steering);
        let taken = inbox.take_steering();
        assert_eq!(taken.len(), 1, "close 前的消息仍可取；close 后的被丢弃");
        assert_eq!(
            taken[0].content,
            vec![ContentBlock::Text {
                text: "before".into()
            }]
        );
        assert!(inbox.take_steering().is_empty());
    }

    #[test]
    fn test_clone_shares_same_underlying() {
        let inbox = Inbox::new();
        let clone = inbox.clone();
        clone.push(text_msg("shared"), Intent::Steering);
        assert!(!inbox.is_empty(), "clone 的 push 对原 handle 可见");
        assert_eq!(inbox.take_steering().len(), 1);
        assert!(clone.is_empty());
    }

    #[test]
    fn test_modes_accessors() {
        let inbox = Inbox::with_modes(QueueMode::All, QueueMode::OneAtATime);
        assert_eq!(inbox.steering_mode(), QueueMode::All);
        assert_eq!(inbox.follow_up_mode(), QueueMode::OneAtATime);
    }
}
