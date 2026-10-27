use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    Completed,
    MaxRounds,
    Cancelled,
    /// 模型输出被输出预算截断（provider 的 finish_reason 为 length/max_tokens）。
    Truncated,
    /// 模型调用**失败**了（provider 侧错误：流中断、传输失败等）。
    ///
    /// 与 `Cancelled` 分开是**必须的**：取消是用户的意图（或预算到点），
    /// 而本变体是**故障**。两者都“没跑完”，但前者不该被当成异常追查，
    /// 后者必须。合并二者会让诊断行变成 `stop_reason=cancelled` 而
    /// `error=` 里却写着 provider 错误 —— 自相矛盾，且把真故障埋在“用户取消”里。
    ///
    /// 本变体**没有** `CancelCause`：既然不是取消，就不该有取消归因。
    Failed,
    /// 模型本轮**没有产出可见文本、也没有工具调用**（例如只输出了思考）。
    ///
    /// 与 `Completed` 区分：`Completed` 表示「模型给出了可用的收尾」；本变体表示「回合结束了、
    /// 但没有可见产出」—— 一次性模式据此返回非零退出码，UI 据此给一行可见说明。
    NoOutput,
}
