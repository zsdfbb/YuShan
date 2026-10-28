use ys_core::{Message, StopReason, Usage};

/// 单次 agent turn 的结果
pub struct RunResult {
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// 本回合的缓存读数 `(hit, miss)`，**逐轮累加**（只统计上报过缓存的轮）。
    ///
    /// **为什么不从 `usage` 派生**：`miss = input − hit` 只在**单轮**里成立。
    /// `usage` 是回合累计，其 `input_tokens` 无条件相加、而 `hit` 缺席时不参与相加
    /// —— 两者不对称 ⇒ 在聚合值上派生会把「缺报轮」的整个 input 当成未命中。
    /// 故在 loop 里逐轮 `Usage::cache_split()` 累加，此处只带走结果。
    ///
    /// `None` = 本回合没有任何一轮上报过缓存读数。
    pub cache_reading: Option<(u32, u32)>,
    /// 本回合**成功**的压缩消耗（自动 + 手动 `/compact`，可能压多次 ⇒ 累加）。
    ///
    /// 全零 = 没压过**或压失败**。与 `usage` 是**兄弟**而非子集：
    /// `RunFinished.usage` **不含**它（逐字节承诺）。
    ///
    /// ⚠️ **失败路径不计**（已知缺口，ADR-0061 D5）：压缩失败时摘要调用可能已真实
    /// 消耗 token，但 `Err` 分支拿不到 `outcome.usage`。故本字段**不是**
    /// 「本回合压缩总花费」。
    ///
    /// 本回合总花费 ≈ `usage + compaction_usage`（失败压缩的漏计是已知偏差）。
    pub compaction_usage: Usage,
    pub rounds: u32,
    pub final_message: Option<Message>,
}
