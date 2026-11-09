//! 回合结束后的**推进判据**（设计见 `docs/arch/goal-command/design.md` §5，
//! 决策见 ADR-0074）。
//!
//! 本模块只回答一个问题：**要不要再起一轮**。它是纯函数：不 await、不碰 model、
//! 不读 session —— 入参全是调用方已经拿到的事实。
//!
//! # 判据顺序即语义
//!
//! [`decide`] 的闸门**按顺序短路**，顺序本身承载语义：
//!
//! 1. 用户取消永远赢 —— 排在「清单已清空」之前，因为「达成」不能覆盖用户的 Ctrl+C；
//! 2. 回合收场形态不可续（截断 / 失败 / 轮数用尽 / 无产出 / 未知变体）；
//! 3. 清单无 open 项且**曾经写过**（`revision > 0`）⇒ 正常收场，
//!    有目标报 [`StopCause::GoalReached`]、无目标报 [`StopCause::AllDone`]；
//! 4. 清单**从未写过**（空且 `revision == 0`）：有目标 ⇒ `Push`，无目标 ⇒ [`StopCause::NoPlan`]；
//! 5. 其余 ⇒ `Push`。
//!
//! 判据 3 的「无 open 项」不是「全部 `Completed`」：被放弃（`cancelled`）的项同样终结，
//! 否则一个放弃的项会永久卡住收场条件。`revision > 0` 用来区分「刚干完」与「根本没计划」
//! —— `TodoStore::apply` 在无 open 项时清空列表，两种情形在快照上长得一样，只能靠
//! 「写过没有」分辨。
//!
//! # 判据 4：没有清单 ≠ 完成
//!
//! 无清单是**合法状态**：目标只需要一句话，模型完全可能还没调 `todo` 工具。
//! 无目标时把它报成 [`StopCause::NoPlan`] 是对的（交给一期的 git 兜底），但有目标时
//! 这么做会让目标被**静默忽略** —— 用户设了目标，运行时却因为模型没写清单就收场。
//! 因此有目标时空清单走 `Push`，代价是模型若坚持不拆清单会一路跑到回合预算耗尽。
//!
//! # 回合数上限不在这里
//!
//! 唯一的回合数上限是任务级的 [`crate::goal_state::GOAL_TURN_CAP`]，只在有目标时生效，
//! 由 [`crate::turn_advance::advance`] 判（它需要表达第三种动作「停下但先收卷」，
//! 而 [`FollowUp`] 只有 `Push` / `Stop` 两态）。无目标时不设任何回合数上限：
//! 收场由判据 2 / 3 / 4 兜住，不需要计数闸门。

use ys_core::StopReason;
use ys_protocol::{FollowUp, StopCause};
use ys_tools_todo::TodoStore;

use crate::goal_state::GoalState;

/// 回合结束后问一次：要不要再起一轮？**纯函数、同步、不 await、不碰 model**。
///
/// `goal` 只影响两个走向：无 open 项时报 `GoalReached` 还是 `AllDone`（判据 3），
/// 以及空清单时是 `Push` 还是 `Stop(NoPlan)`（判据 4）。
///
/// `used` 是**诊断计数，不参与任何闸门** —— 它只为日志与统计而传入，结论与它无关。
pub(crate) fn decide(
    store: &TodoStore,
    used: u32,
    last_stop: &StopReason,
    goal: Option<&GoalState>,
) -> FollowUp {
    // 诊断计数：只用于调试观察，不影响任何返回值。
    let _ = used;

    // ── 判据 1：用户取消永远赢 ────────────────────────────────────────
    // 排在「无 open 项」之前：清单恰好清空也不能覆盖用户的 Ctrl+C。
    if matches!(last_stop, StopReason::Cancelled) {
        return FollowUp::Stop(StopCause::Aborted);
    }

    // ── 判据 2：回合收场形态不可续 ────────────────────────────────────
    // `StopReason` 是 `#[non_exhaustive]`，通配臂兜住未来新增变体 ——
    // 未知收场一律按「不可续」处理，绝不多花钱。
    match last_stop {
        StopReason::Completed => {}
        StopReason::Truncated
        | StopReason::Failed
        | StopReason::MaxRounds
        | StopReason::NoOutput => {
            return FollowUp::Stop(StopCause::NotContinuable);
        }
        // `Cancelled` 已被判据 1 拦下；未知变体按不可续处理。
        _ => return FollowUp::Stop(StopCause::NotContinuable),
    }

    // ── 判据 3：无 open 项且曾经写过 ⇒ 正常收场 ───────────────────────
    // 有目标报「目标达成」、无目标报「清单清空」，UI 靠这个区分两种收场。
    if !store.has_open_items() && store.revision() > 0 {
        return FollowUp::Stop(match goal {
            Some(_) => StopCause::GoalReached,
            None => StopCause::AllDone,
        });
    }

    // ── 判据 4：清单从未写过 ──────────────────────────────────────────
    // 「无 open 项且清单非空」被 `apply` 的硬规则排除，故此处剩下的空清单只有
    // 「从未写过」一种来源。
    //
    // 有目标时**反转为 `Push`**：无清单不代表目标达成（见模块头「判据 4」）。
    // 无目标时报 `NoPlan`，把决定权交给 `advance` 的一期 git 兜底。
    if store.snapshot().is_empty() {
        return match goal {
            Some(_) => FollowUp::Push,
            None => FollowUp::Stop(StopCause::NoPlan),
        };
    }

    // ── 判据 5：其余 ⇒ 再起一轮 ───────────────────────────────────────
    FollowUp::Push
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ys_tools_todo::{TodoItem, TodoStatus};

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(content, status)
    }

    /// 纯内存 store（`open(None)`）—— 不落盘、不碰临时目录。
    fn store_with(items: Vec<TodoItem>) -> Arc<TodoStore> {
        let store = TodoStore::open(None);
        store.apply(items).expect("测试用的清单应被接受");
        store
    }

    /// 写一份清单再全部终结 —— 留下「曾经写过、现在无 open 项」的 store。
    fn store_after_finishing() -> Arc<TodoStore> {
        let store = store_with(vec![item("写完的", TodoStatus::Pending)]);
        store
            .apply(vec![
                item("写完的", TodoStatus::Completed),
                item("放弃的", TodoStatus::Cancelled),
            ])
            .expect("全终态 ⇒ 清空");
        store
    }

    /// 从未写过清单的 store。
    fn store_never_written() -> Arc<TodoStore> {
        TodoStore::open(None)
    }

    /// 一次调用所需的全部状态。
    struct Env {
        goal: Option<GoalState>,
    }

    impl Env {
        /// 无目标。
        fn plain() -> Self {
            Self { goal: None }
        }

        /// 有目标。
        fn with_goal(text: &str) -> Self {
            Self {
                goal: Some(GoalState::new(text.to_string())),
            }
        }

        fn decide(&self, store: &TodoStore, used: u32, stop: StopReason) -> FollowUp {
            decide(store, used, &stop, self.goal.as_ref())
        }
    }

    // ── 判据 1：取消永远赢 ────────────────────────────────────────────

    /// `Cancelled` ⇒ `Stop(Aborted)`，即便清单**已经清空**（判据 3 的条件也成立）。
    ///
    /// 变异：把判据 1 挪到判据 3（无 open 项）之后 ⇒ 本用例得到 `AllDone` /
    /// `GoalReached` 而变红。
    #[test]
    fn cancelled_wins_over_all_done() {
        let store = store_after_finishing();
        assert!(!store.has_open_items(), "前置：清单已无 open 项");

        for env in [Env::plain(), Env::with_goal("修好 flaky 测试")] {
            assert_eq!(
                env.decide(&store, 0, StopReason::Cancelled),
                FollowUp::Stop(StopCause::Aborted),
                "用户 Ctrl+C 永远优先于「达成」"
            );
        }
    }

    /// 取消也赢过「不可续」：`Cancelled` 不该被报成 `NotContinuable`。
    ///
    /// 变异：把判据 1 删掉 ⇒ `Cancelled` 落到判据 2 的通配臂，本用例变红。
    #[test]
    fn cancelled_is_not_not_continuable() {
        let store = store_with(vec![item("写判据", TodoStatus::InProgress)]);
        assert_eq!(
            Env::plain().decide(&store, 0, StopReason::Cancelled),
            FollowUp::Stop(StopCause::Aborted)
        );
    }

    // ── 判据 2：不可续的收场形态 ──────────────────────────────────────

    /// 四种不可续收场各触发一次 `Stop(NotContinuable)`，即使清单还有 open 项。
    ///
    /// 变异：整条判据 2 删掉 ⇒ 清单有 open 项 ⇒ `Push`，本用例变红。
    #[test]
    fn non_continuable_stop_reasons_stop() {
        for env in [Env::plain(), Env::with_goal("跑完整套测试")] {
            for stop in [
                StopReason::Truncated,
                StopReason::Failed,
                StopReason::MaxRounds,
                StopReason::NoOutput,
            ] {
                let store = store_with(vec![item("写判据", TodoStatus::InProgress)]);
                assert_eq!(
                    env.decide(&store, 0, stop.clone()),
                    FollowUp::Stop(StopCause::NotContinuable),
                    "{stop:?} 不该续跑"
                );
            }
        }
    }

    // ── 判据 3：正常收场（有目标 / 无目标）────────────────────────────

    /// 曾经写过清单、现在无 open 项 ⇒ 无目标报 `AllDone`，有目标报 `GoalReached`。
    ///
    /// 同时钉住「判据是无 open 项而非全部 `Completed`」：一项 `completed` + 一项
    /// `cancelled` 也算收场。
    ///
    /// 变异：把判据 3 的 `!has_open_items()` 改成「所有项都是 `Completed`」⇒
    /// 含 `cancelled` 的那份 store 会落到 `Push`，本用例变红；
    /// 或把 `goal` 分支写成恒 `AllDone` ⇒ 有目标那条断言变红。
    #[test]
    fn no_open_items_reports_goal_reached_or_all_done() {
        let store = store_after_finishing();
        assert!(!store.has_open_items(), "前置：apply 应清空全终态清单");
        assert!(store.revision() > 0, "前置：写过清单");

        assert_eq!(
            Env::plain().decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::AllDone),
            "无目标 ⇒ 清单清空"
        );
        assert_eq!(
            Env::with_goal("修好 flaky 测试").decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::GoalReached),
            "有目标 ⇒ 目标达成"
        );
    }

    // ── 判据 4：从没写过清单（有目标 / 无目标）────────────────────────

    /// 全新 store（从未写过）。
    ///
    /// **有目标 ⇒ `Push`**：无清单 ≠ 完成。这是 D-A 的核心修补 —— 报 `NoPlan` 会让
    /// 用户设的目标被静默忽略。
    /// **无目标 ⇒ `Stop(NoPlan)`**：交给 `advance` 的一期 git 兜底（至多一次）。
    ///
    /// 变异：把判据 4 的有目标分支改回 `Stop(NoPlan)` ⇒ 有目标那条断言变红；
    /// 把判据 3 的 `revision > 0` 去掉 ⇒ 无目标那条会得到 `AllDone` 而非 `NoPlan`。
    #[test]
    fn never_written_plan_pushes_only_with_goal() {
        let store = store_never_written();
        assert_eq!(store.revision(), 0, "前置：从未写过");
        assert!(store.snapshot().is_empty(), "前置：清单为空");

        assert_eq!(
            Env::with_goal("给 /goal 接上追问循环").decide(&store, 0, StopReason::Completed),
            FollowUp::Push,
            "有目标 + 空清单 ⇒ 续跑，绝不报 NoPlan"
        );
        assert_eq!(
            Env::plain().decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::NoPlan),
            "无目标 + 空清单 ⇒ 交给一期 git 兜底"
        );
    }

    // ── 判据 5：续跑 ──────────────────────────────────────────────────

    /// 清单有 open 项、无闸门拦下 ⇒ `Push`（有目标 / 无目标都一样）。
    ///
    /// 变异：在判据 5 之前插一条 `Stop(AllDone)` ⇒ 本用例变红。
    #[test]
    fn open_items_push() {
        let store = store_with(vec![item("写判据", TodoStatus::Pending)]);
        for env in [Env::plain(), Env::with_goal("写判据")] {
            assert_eq!(env.decide(&store, 0, StopReason::Completed), FollowUp::Push);
        }
    }

    // ── 反向守护：闸门真的没了 ────────────────────────────────────────

    /// `used = 999` 与 `used = 0` 在**同样输入**下结论相同 —— 回合数不参与任何判据。
    ///
    /// 覆盖判据 1 / 3 / 4 / 5 的出口 × 有目标 / 无目标，因此任何一条判据里重新出现的
    /// `used` 阈值都会被抓到，而不只是某一条。
    ///
    /// 变异：把 `used` 判据加回去（`if used >= N { return Stop(..) }`）⇒ 本用例变红。
    #[test]
    fn used_never_changes_the_verdict() {
        for env in [Env::plain(), Env::with_goal("跑完整套测试")] {
            for (label, store) in [
                ("已收场", store_after_finishing()),
                ("从未写过", store_never_written()),
                (
                    "有 open 项",
                    store_with(vec![item("写判据", TodoStatus::Pending)]),
                ),
            ] {
                for stop in [
                    StopReason::Completed,
                    StopReason::Cancelled,
                    StopReason::Truncated,
                ] {
                    assert_eq!(
                        env.decide(&store, 0, stop.clone()),
                        env.decide(&store, 999, stop.clone()),
                        "{label} / {stop:?}：回合数不得改变结论"
                    );
                }
            }
        }
    }
}
