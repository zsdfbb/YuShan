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
    /// 理由：`try_emit` 的**唯一**调用者是 `Forwarder`（`ModelEventSink` 的同步
    /// 回调，不能 `await`），它以 `let _ =` 丢弃返回值。若信道满时返回 `Err`，
    /// 事件就会被静默丢弃；故满时必须内部缓冲，把「满」的处置收敛在 sink 内。
    /// 注意：自由函数 [`emit`](crate::emit) **已不再调用本方法**——它总走异步
    /// 慢路径，以保背压生效与终局事件必送达（见该函数文档）。
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

/// 循环内唯一出口：**总是**走异步慢路径。
///
/// 注意：这里**不再先试同步快路径**。理由：同步快路径在信道满时会把事件
/// 缓冲进 sink 内部（`overflow`）并返回 `Ok`，调用方会误以为已投递；而这条
/// 缓冲只有在**下一次异步慢路径**（`emit().await`）时才会被冲刷。若最后一个
/// 事件（如终局事件 `RunFinished`）恰好被缓冲，其后不再有任何 `emit().await`，
/// 就会永久滞留 → 消费者等终局事件 → `tokio::join!` 死锁；同时 `overflow`
/// 无界增长，背压彻底失效。
///
/// 既然本函数的**所有**调用点都在 async 上下文（`ys-loop` 的循环内），
/// 就没有理由走同步快路径——直接 await 慢路径，让背压与冲刷语义只有一个出口。
///
/// 同步快路径的**唯一**消费者是 `Forwarder`（`ModelEventSink` 的同步回调，
/// 不能 `await`），它直接调 [`EventSink::try_emit`]，不经本函数。该例外由上层
/// 实现持有，契约层不得依赖它（`ys-event` 不认识 `Forwarder`）。
///
/// 代价：每个事件多一次 `Box::pin`。相对流式 20–200 events/s 的频率可忽略。
///
/// 本函数（异步路径）**保证**背压生效与终局事件送达。但注意：`Forwarder`
/// （同步 `ModelEventSink` 回调，不能 `await`）直调 [`EventSink::try_emit`]，
/// **不走本路径**——消费者停滞时，一次模型响应期间的增量事件会堆在 sink 的
/// `overflow` 里（上界 ≈ 单次响应的增量数）。这是「同步回调无法背压」的固有
/// 取舍，非缺陷。
#[inline]
pub async fn emit(sink: &mut dyn EventSink, event: AgentEvent) -> Result<(), EventError> {
    sink.emit(event).await
}
