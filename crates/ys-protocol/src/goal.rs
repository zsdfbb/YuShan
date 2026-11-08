//! 目标验收的**词汇层** —— 回合结束后的推进决定。
//!
//! 纯数据、零 IO、**零 tokio**（直接与间接都不含）：这些类型要能被任意运行时与任意产品
//! TUI crate 引用，因此不能带任何执行器类型。
//!
//! 本模块**只承载词汇**：判定怎么算出来（判官调用、引用校验）在 app 层
//! （`apps/coding-agent/src/goal_judge.rs`，那里产出 [`JudgeParse`]），闸门怎么排序在
//! `apps/coding-agent/src/goal.rs`。
//!
//! [`JudgeParse`]: ../../ys_coding_agent/goal_judge/enum.JudgeParse.html

/// 回合结束后的推进决定。
///
/// 只有两种动作：**再起一轮**（[`FollowUp::Push`]）或**停下**（[`FollowUp::Stop`]）。
/// 「停下」必须带原因 —— 停机理由要能出现在 UI 与统计里，否则用户无法区分
/// 「干完了」和「预算烧光了」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowUp {
    /// 再起一轮：仍有未达成的工作，且没有闸门拦下。
    Push,
    /// 停下，理由见载荷。
    Stop(StopCause),
}

/// 停下来的原因（用于 UI 提示与统计）。
///
/// 变体即**停机分类**：正常收场只有 [`StopCause::AllDone`] 一个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopCause {
    /// 没有计划（本回合从未写过清单）⇒ 按普通回合处理。
    NoPlan,
    /// 清单刚被清空（曾经有项、现在无 open 项）—— **唯一的正常收场**。
    AllDone,
    /// 追问轮数用尽（`used >= MAX_FOLLOW_UPS`）。
    CapReached,
    /// 连续多轮剩余集合指纹不变，且模型自称收尾 —— 原地打转。
    Stalled,
    /// 回合被取消（用户 Ctrl+C）。**用户意图永远优先于任何判据**。
    Aborted,
    /// 回合收场形态不可续（截断 / 失败 / 轮数用尽 / 无产出）。
    NotContinuable,
    /// 判官连续多次不可靠（解析失败 / 引用未命中）⇒ 没有判据就没有停止条件。
    JudgeUnreliable,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FollowUp` 的 `Push` 与每个 `Stop(..)` 都不相等，且 `Stop` 携带原因。
    ///
    /// 变异：把 `FollowUp` 的 `Stop` 载荷丢掉（改成单元变体）⇒ 本用例变红。
    #[test]
    fn follow_up_carries_stop_cause() {
        assert_ne!(FollowUp::Push, FollowUp::Stop(StopCause::AllDone));
        assert_ne!(
            FollowUp::Stop(StopCause::AllDone),
            FollowUp::Stop(StopCause::CapReached),
            "不同停机原因不得相等 —— UI 与统计靠它区分"
        );
        assert_eq!(
            FollowUp::Stop(StopCause::Aborted),
            FollowUp::Stop(StopCause::Aborted)
        );
    }
}
