//! 一次性「派发 subagent」能力 —— 把一段**自包含任务文本**交给一个隔离的子 agent，
//! 拿回一个句柄；结果异步回到父会话。
//!
//! 形状刻意窄，与 [`crate::Summarizer`] 同层同形：`ys-model` 依赖 `ys-tool`
//! （为 `ToolSpec`）⇒ 反向依赖成环 ⇒ 此处**不得**出现 `Model` / `ModelRequest` /
//! `Session` / `AgentEvent`。允许 `ys-core` 的 `StopReason` / `Usage`（`ys-tool`
//! 已依赖 `ys-core`），也允许 `ys-protocol` 的 `BoundarySource`（`ToolContext` 已在用）。

use ys_core::{StopReason, Usage};
use ys_protocol::BoundarySource;

/// 子 agent 的用途。决定子 agent 的系统提示与工具集。
///
/// 只影响子 agent 自身：父回合的推进规则与结果通道不因模式而变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubagentMode {
    /// 通用派发：能读写，拿回摘要（默认）。
    #[default]
    General,
    /// 只读探索：调查并回报带 `文件:行号` 的结论，不给写工具。
    Explore,
}

/// 一次派发的结果。
///
/// **摘要已由实现方按预算硬截断** —— 截断责任在代码，不在提示词。
#[derive(Debug, Clone)]
pub struct SubagentOutcome {
    /// 给模型看的摘要（已截断到实现方选定的上限）。
    pub summary: String,
    /// 完整转录的**绝对路径**（落盘）；失败时若已写出部分内容也尽量给。
    pub artifact: Option<String>,
    /// 子 agent 的终止原因。**复用 `ys_core::StopReason`，不新造词汇**。
    ///
    /// 非 `Completed` ⇒ 父侧 `ToolResult{is_error: true}`（ADR-0001 错误结果通道），
    /// 父回合**不因此失败**。
    pub stop_reason: StopReason,
    /// 非 `Completed` 时的**安全失败详情**（provider 报错原文等），实现方须硬上限截断。
    pub diagnostic: Option<String>,
    /// 子 agent 累计的 token 用量 —— 供父**上卷进 `TurnStats`**。
    pub usage: Usage,
}

/// 后台派发能力：`&str` 进 / 句柄出，结果经边界源回到会话。
///
/// 与 [`crate::ApprovalHandler`] / [`crate::Summarizer`] 同层同形：由上层实现、只在
/// 工具里被读。`ToolContext.subagent` 以 `&dyn SubagentRunner` 持有它 ⇒ 本 trait 必须
/// **object-safe**。
pub trait SubagentRunner: Send + Sync {
    /// **后台派发**：立刻返回句柄，子 agent 在后台跑；**结果会自动注入会话**。
    ///
    /// `model = None` ⇒ 用**当前**模型（缓存最暖、零额外配置）。
    /// `mode` 只决定子 agent 的系统提示与工具集（见 [`SubagentMode`]），不改父回合语义。
    /// `boundary` = 父回合的边界源；实现方**只透传 `is_aborted`，绝不 `take`**。
    ///
    /// `Err` = **启动失败**（模型名解析失败 / 本回合额度用尽）。**绝不静默回退到父模型**
    /// —— 用户以为换了模型其实没换，是本仓明确反对的「静默的谎」。
    /// 子 agent 启动后「跑完了但没跑好」（撞 `max_rounds` / 被取消）**不是 `Err`**，
    /// 它随结果消息一起回到会话，以诊断正文 + 非 `Completed` 的 `stop_reason` 自陈。
    ///
    /// # 结果怎么回来 —— **不是 poll**
    ///
    /// 实现方在完成时把结果作为 `Boundary::Deferred` **推进父回合的边界源**，
    /// `BasicLoop` 在下一个**轮边界**把它 append 进会话并 emit `DeferredResult`。
    /// 模型不必轮询、也不可能「忘记取」。
    ///
    /// # 生命周期契约（**分离任务，不与 runner 同寿**）
    ///
    /// 后台任务是 `tokio::spawn` 的**分离任务**：回合结束后仍会继续跑，结果经实现方的
    /// 跨回合投递表（coding agent 的 `PendingResults`）在后续回合送达。
    ///
    /// ⇒ **回合末不等于任务死亡**：取消只来自实现方透传的父边界源与子自身的预算
    /// watchdog，回合结束本身不取消任何东西。
    ///
    /// 默认实现 = 不支持（返回 `Err`）⇒ 没有派发能力的实现方不必覆写。
    fn spawn(
        &self,
        _prompt: &str,
        _model: Option<&str>,
        _mode: SubagentMode,
        _boundary: Option<&dyn BoundarySource>,
    ) -> Result<String, String> {
        Err("本实现不支持后台派发".to_string())
    }

    /// 向**仍在运行**的子 agent 投递一条消息；它在子 agent 的下一个轮边界被看到。
    ///
    /// 投递只入队、不等待 ⇒ 本方法是**同步**的（`ys-tool` 不含 tokio）。
    ///
    /// `Err` = handle 未知 / 该子 agent 已收尾 / 本实现不支持。实现方**必须
    /// fail-closed**：投不出去就报错，绝不静默 `Ok`（否则模型会以为子 agent 收到了
    /// 一条它永远看不到的话）。
    fn send(&self, _handle: &str, _message: &str) -> Result<(), String> {
        Err("本实现不支持向子 agent 发送消息".to_string())
    }
}
