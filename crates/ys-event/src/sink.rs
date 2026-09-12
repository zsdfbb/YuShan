use std::future::Future;
use std::pin::Pin;

use super::AgentEvent;
use ys_core::EventError;

/// 基于 push 的 event sink，提供同步 / 异步双路径投递。
///
/// 双路径的意义：快路径同步、零装箱，保住 `ModelEventSink` 的同步性
/// （模型适配器最小 ABI 面）；慢路径是背压点，供有界信道使用。
pub trait EventSink: Send {
    /// 快速路径：同步尝试投递。**不阻塞、不丢事件**。
    ///
    /// **契约（已定）**：`Err(event)` **仅表示「消费者已消失」**，事件原样退回。
    /// 信道满时实现**必须内部缓冲（overflow），不得返回 `Err`**。
    ///
    /// 理由：同一个 `Err` 在两条调用路径上被相反地处置——自由函数 [`emit`](crate::emit)
    /// 视之为「走慢路径重试」（保留事件），而 `Forwarder`（同步回调）以 `let _ =` 丢弃之。
    /// 只有把 `Err` 收窄成「消费者消失」，二者才自洽：消费者没了，丢弃才对。
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>;

    /// 慢速路径：异步投递（背压点）。
    ///
    /// `Err` 同样是「消费者已消失 / 不可恢复的投递失败」（`EventError::SendFailed`），
    /// 而非瞬时背压——瞬时背压由实现内部缓冲消化，不上升到此。
    fn emit<'a>(
        &'a mut self,
        event: AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>>;

    /// 回合边界告知（默认 no-op）。由 `Agent::run` 在每个回合开始时调用。
    fn begin_turn(&mut self, _turn: u32) {}
}

/// 循环内唯一出口：自由函数组合两条路径。
///
/// 稳态（`try_emit` 成功）**不装箱**——`async fn` 本身必然构造一个 Future，
/// 但这条 Future 不含任何 `Box::pin`；只有快路径失败时才付出装箱代价。
///
/// 例外：`ys-loop` 的 `Forwarder`（`ModelEventSink` 的同步回调，不能 `await`）
/// 直接调 `try_emit`，不经本函数。该例外由上层实现持有，契约层不得依赖它
/// （`ys-event` 不认识 `Forwarder`）。
#[inline(always)]
pub async fn emit(sink: &mut dyn EventSink, event: AgentEvent) -> Result<(), EventError> {
    match sink.try_emit(event) {
        Ok(()) => Ok(()),
        Err(event) => sink.emit(event).await,
    }
}
