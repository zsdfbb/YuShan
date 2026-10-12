use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    Completed,
    MaxRounds,
    Cancelled,
    /// 模型输出被输出预算截断（provider 的 finish_reason 为 length/max_tokens）。
    Truncated,
    /// 模型本轮**没有产出可见文本、也没有工具调用**（例如只输出了思考）。
    ///
    /// 与 `Completed` 区分：`Completed` 表示「模型给出了可用的收尾」；本变体表示「回合结束了、
    /// 但没有可见产出」—— 一次性模式据此返回非零退出码，UI 据此给一行可见说明。
    NoOutput,
}
