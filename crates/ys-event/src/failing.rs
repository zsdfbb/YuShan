use std::future::Future;
use std::pin::Pin;

use super::{AgentEvent, EventSink};
use ys_core::EventError;

/// 测试用的失败替身：从第 `succeed_first` 次调用起让投递失败。
///
/// **生产导出**（照 `ys_model::MockModel` 先例，非 `#[cfg(test)]`）：跨 crate 复用，
/// `ys-loop` / `ys-runtime` / `ys-coding-agent` 的测试都需要一个能注入
/// 「投递失败 → `EventError::SendFailed`」的 sink。
///
/// 两条路径各自独立计数：
/// - 快路径 `try_emit`：前 `succeed_first` 次成功，之后返回 `Err(event)`（原样退回）。
/// - 慢路径 `emit`：仅当已开启 [`set_fail_slow`](FailingSink::set_fail_slow) 且自身调用次数
///   达到同一阈值时返回 `Err(EventError::SendFailed)`。
///
/// **慢路径默认成功**（`fail_slow == false`），故 `FailingSink::new(0)` 恰是
/// 「快路径必失败、慢路径可成功」——用于直接测 `try_emit` / `emit` 两条路径本身；
/// 要测慢路径失败（`SendFailed` 直达调用方），显式 `set_fail_slow(true)`。
///
/// **注意**：自由函数 [`emit`](crate::emit) **已不再**「先试快路径、失败再回落慢路径」，
/// 而是总走慢路径。因此本替身的 `try_emit` 不再服务于自由函数的回落逻辑，只服务于
/// 直接调用 `try_emit` 的场景（如 `Forwarder`）。
///
/// **与 [`EventSink`] 契约的关系（测试工具的宽松语义）**：生产实现须遵守
/// 「`Err` 仅表示消费者消失、满时内部缓冲、不得返回 `Err`」的契约；而本替身是
/// **通用失败注入器**，其 `try_emit` 返回 `Err(event)` 为模拟「投递失败」，
/// 语义比契约更宽松，仅供测试注入失败，不代表生产实现应照此返回 `Err`。
pub struct FailingSink {
    /// 前 `succeed_first` 次调用成功，之后失败
    succeed_first: usize,
    /// 快路径累计调用次数
    try_calls: usize,
    /// 慢路径累计调用次数
    slow_calls: usize,
    /// 记录已成功投递的事件（两条路径合计），便于断言
    emitted: Vec<AgentEvent>,
    /// 是否让**慢路径**也失败（默认 `false`：慢路径成功）
    fail_slow: bool,
}

impl FailingSink {
    /// 前 `succeed_first` 次快路径调用成功，之后 `try_emit` 返回 `Err(event)`。
    /// `new(0)` 表示首次即失败。
    pub fn new(succeed_first: usize) -> Self {
        Self {
            succeed_first,
            try_calls: 0,
            slow_calls: 0,
            emitted: Vec::new(),
            fail_slow: false,
        }
    }

    /// 永不失败的便捷构造（快路径恒 `Ok`）。
    pub fn always_ok() -> Self {
        Self::new(usize::MAX)
    }

    /// 置慢路径是否按同一阈值失败（默认 `false`，即慢路径成功）。
    pub fn set_fail_slow(&mut self, fail_slow: bool) {
        self.fail_slow = fail_slow;
    }

    /// 已成功投递的事件（快慢两条路径合计）。
    pub fn emitted(&self) -> &[AgentEvent] {
        &self.emitted
    }

    /// 快慢两路径的调用次数之和。
    pub fn call_count(&self) -> usize {
        self.try_calls + self.slow_calls
    }

    /// [`call_count`](FailingSink::call_count) 的别名（执行计划 Task 6 的合同名）。
    pub fn count(&self) -> usize {
        self.call_count()
    }

    /// 快路径（`try_emit`）被调用的次数。
    pub fn try_calls(&self) -> usize {
        self.try_calls
    }

    /// 慢路径（`emit`）被调用的次数。
    pub fn slow_calls(&self) -> usize {
        self.slow_calls
    }
}

impl EventSink for FailingSink {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> {
        let index = self.try_calls;
        self.try_calls += 1;
        if index < self.succeed_first {
            self.emitted.push(event);
            Ok(())
        } else {
            // 原样退回事件（供调用方按需处置）
            Err(event)
        }
    }

    fn emit<'a>(
        &'a mut self,
        event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>> {
        Box::pin(async move {
            let index = self.slow_calls;
            self.slow_calls += 1;
            if self.fail_slow && index >= self.succeed_first {
                return Err(EventError::SendFailed);
            }
            self.emitted.push(event);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(text: &str) -> AgentEvent {
        AgentEvent::ModelTextDelta { text: text.into() }
    }

    // 快路径：前 N 次成功，第 N+1 次起原样退回事件
    #[test]
    fn test_try_emit_succeeds_until_threshold() {
        let mut sink = FailingSink::new(2);
        assert!(sink.try_emit(ev("a")).is_ok());
        assert!(sink.try_emit(ev("b")).is_ok());
        assert_eq!(
            sink.try_emit(ev("c")),
            Err(ev("c")),
            "第 3 次应原样退回事件"
        );
        assert_eq!(sink.emitted().len(), 2, "只有前两次被投递");
        assert_eq!(sink.call_count(), 3);
        assert_eq!(sink.try_calls(), 3);
        assert_eq!(sink.slow_calls(), 0);
    }

    // 快路径：new(0) 首次即失败
    #[test]
    fn test_new_zero_fails_immediately() {
        let mut sink = FailingSink::new(0);
        assert_eq!(sink.try_emit(ev("a")), Err(ev("a")));
        assert!(sink.emitted().is_empty());
        assert_eq!(sink.count(), 1);
    }

    // always_ok：快路径恒 Ok，慢路径不被需要
    #[test]
    fn test_always_ok_never_fails() {
        let mut sink = FailingSink::always_ok();
        for _ in 0..5 {
            assert!(sink.try_emit(ev("x")).is_ok());
        }
        assert_eq!(sink.emitted().len(), 5);
        assert_eq!(sink.count(), 5);
        assert_eq!(sink.slow_calls(), 0);
    }

    // 慢路径默认成功（回落路径可成功投递）
    #[tokio::test]
    async fn test_slow_path_succeeds_by_default() {
        let mut sink = FailingSink::new(0);
        assert!(sink.emit(ev("a")).await.is_ok());
        assert_eq!(sink.slow_calls(), 1);
        assert_eq!(sink.emitted().len(), 1);
    }

    // 慢路径：开启 fail_slow 后按同一阈值失败（前 2 次 Ok，第 3 次 SendFailed）
    #[tokio::test]
    async fn test_slow_path_fails_after_threshold_when_enabled() {
        let mut sink = FailingSink::new(2);
        sink.set_fail_slow(true);
        assert!(sink.emit(ev("a")).await.is_ok());
        assert!(sink.emit(ev("b")).await.is_ok());
        assert!(matches!(
            sink.emit(ev("c")).await,
            Err(EventError::SendFailed)
        ));
        assert_eq!(sink.slow_calls(), 3);
        assert_eq!(sink.emitted().len(), 2);
        assert_eq!(sink.try_calls(), 0, "只走慢路径时快路径计数为 0");
    }

    // 慢路径：new(0) + fail_slow 首次即 SendFailed
    #[tokio::test]
    async fn test_slow_path_fails_immediately_with_new_zero() {
        let mut sink = FailingSink::new(0);
        sink.set_fail_slow(true);
        assert!(matches!(
            sink.emit(ev("a")).await,
            Err(EventError::SendFailed)
        ));
        assert_eq!(sink.call_count(), 1);
        assert!(sink.emitted().is_empty());
    }
}
