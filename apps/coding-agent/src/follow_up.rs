//! 完工复核（设计 §4.1 / ADR-0027）。
//!
//! 模型自称完工（`StopReason::Completed`）后，系统**主动追问恰好一次**「真的完工了吗」，
//! 另起一个回合。判据是一条纯函数，「只问一次」由 `TurnKind` 的**结构**保证 ——
//! 没有计数上限、没有停滞判据、没有抢占、没有逃生开关。

use ys_core::StopReason;
use ys_loop::{AgentInput, RunResult};

/// 本回合至少跑到这么多轮才值得复核。
///
/// 抬到 5 而非 2：`rounds >= 2` 只排除纯文本完工，「读一个文件就答」这类短回合仍满足它
/// ⇒ 复核会落在几乎每次 coding 交互上，把「单次便宜」乘成高频成本。短回合早收的代价
/// 只是用户补一句，长回合早收的代价是整段工作白做。**这是待真实环境校准的初值。**
pub(crate) const REVIEW_MIN_ROUNDS: u32 = 5;

/// 完工复核文案。**纯常量、确定性字节** —— 同内容恒同字节，
/// 不得含时间戳 / 回合号 / 计数（否则测试无法逐字节 fixture，且会污染缓存前缀）。
pub(crate) const REVIEW_PROMPT: &str = "Before we wrap up: verify that the original request is \
    actually complete. Compare what was asked against what has been done. If anything is missing, \
    unfinished, or unverified, keep working and finish it now using the available tools. If it is \
    genuinely complete, reply with a short confirmation and stop.";

/// 一个回合的来历：用户输入的首个回合，还是系统发起的复核回合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnKind {
    Initial,
    Review,
}

/// 本回合结束后是否要追问一次「真的完工了吗」。
///
/// 三道门（从左到右短路）：
/// 1. **只问一次**：`Review` 回合恒 `None` ⇒ 每用户输入至多多出一个复核回合；
/// 2. **只在自称完工后**：`Cancelled`（含 wall-clock 预算到点）/ `Truncated` / `MaxRounds` 一律不问
///    —— 用户中止永远赢；
/// 3. **只在够长的回合**：`rounds >= REVIEW_MIN_ROUNDS`。
pub(crate) fn should_review(kind: TurnKind, result: &RunResult) -> Option<AgentInput> {
    if kind != TurnKind::Initial {
        return None;
    }
    if result.stop_reason != StopReason::Completed {
        return None;
    }
    if result.rounds < REVIEW_MIN_ROUNDS {
        return None;
    }
    Some(AgentInput::text(REVIEW_PROMPT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::{ContentBlock, Role, Usage};

    /// 构造一个 `RunResult`；`usage` 用默认值（复核判据不看 token）。
    fn result(stop_reason: StopReason, rounds: u32) -> RunResult {
        RunResult {
            stop_reason,
            usage: Usage::default(),
            rounds,
            final_message: None,
        }
    }

    /// 1. `Review` 回合恒 `None` —— 不管已 `Completed` 且 `rounds` 很大。
    ///
    /// 这是「只问一次」的**全部**保证：删掉 `should_review` 的首个早退，本用例变红。
    #[test]
    fn review_turn_never_reviews_again() {
        assert!(
            should_review(
                TurnKind::Review,
                &result(StopReason::Completed, REVIEW_MIN_ROUNDS + 100)
            )
            .is_none(),
            "Review 回合恒不追问，无论 rounds 多大"
        );
    }

    /// 2. 非 `Completed` 的 stop_reason 一律不追问 —— 用户中止永远赢。
    #[test]
    fn non_completed_stop_reasons_never_review() {
        for stop in [
            StopReason::Cancelled,
            StopReason::Truncated,
            StopReason::MaxRounds,
        ] {
            assert!(
                should_review(TurnKind::Initial, &result(stop.clone(), REVIEW_MIN_ROUNDS + 100))
                    .is_none(),
                "{stop:?} 不得触发复核（用户中止 / 截断 / 撞顶都不续）"
            );
        }
    }

    /// 3. 门槛两侧：`rounds == REVIEW_MIN_ROUNDS - 1` ⇒ `None`；`== REVIEW_MIN_ROUNDS` ⇒ `Some`。
    ///
    /// 变异：把 `result.rounds < REVIEW_MIN_ROUNDS` 改成 `<=` ⇒ 恰好达标那条断言变红。
    #[test]
    fn review_requires_min_rounds_boundary() {
        assert!(
            should_review(
                TurnKind::Initial,
                &result(StopReason::Completed, REVIEW_MIN_ROUNDS - 1)
            )
            .is_none(),
            "差一轮不足门槛 ⇒ 不复核"
        );
        assert!(
            should_review(
                TurnKind::Initial,
                &result(StopReason::Completed, REVIEW_MIN_ROUNDS)
            )
            .is_some(),
            "恰好达标 ⇒ 复核"
        );
    }

    /// 4. 正例：复核输入的文本**逐字节**等于 `REVIEW_PROMPT`，角色为 `User`，
    ///    content 是单个 `Text` 块。
    #[test]
    fn review_input_is_exact_user_text_message() {
        let input = should_review(
            TurnKind::Initial,
            &result(StopReason::Completed, REVIEW_MIN_ROUNDS),
        )
        .expect("Initial + Completed + 够长 ⇒ 应复核");

        assert_eq!(input.message.role, Role::User, "复核消息是 User 角色");
        assert_eq!(input.message.content.len(), 1, "恰为单个内容块");
        match &input.message.content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, REVIEW_PROMPT, "文本必须逐字节等于常量");
            }
            other => panic!("应为单个 Text 块，实际为：{other:?}"),
        }
    }
}
