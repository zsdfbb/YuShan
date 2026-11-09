//! 回合结束后「要不要再来一轮」的**两条判据**及其合并规则。
//!
//! 两条判据互斥地服务于两个消费者（`docs/arch/cross-turn-advance/design.md` §2）：
//!
//! - **三期（清单 + 证据验收）**：`goal::decide` —— 只要本回合写过清单就由它说了算；
//! - **一期（`git HEAD` 前后不等）**：`post_turn::should_follow_up` —— 只在**没有计划**
//!   时兜底（`StopCause::NoPlan`），行为与一期完全一致：至多一次。
//!
//! # 为什么「无计划」必须退回一期而不是直接停
//!
//! `decide` 的第 5 条判据是 `snapshot().is_empty() ⇒ Stop(NoPlan)`，它回答的是
//! 「三期这条线要不要继续」。而普通编码回合（没写清单）本来就是一期的主场 ——
//! 把 `NoPlan` 当成终止会让一期的 git 追问在接线后静默失效。
//!
//! 反过来，**有计划时一期的判据不参与**：清单已经给出「还剩什么」的精确答案，
//! 再叠一条「HEAD 变了」只会多跑一轮没有 todo 依据的回合。

use ys_protocol::{FollowUp, StopCause};

use crate::goal::decide;
use crate::goal_state::GoalState;
use ys_core::StopReason;
use ys_tools_todo::TodoStore;

/// 一条回合链的推进状态（跨回合存活，故由调用方持有）。
///
/// **不是** `Default` 之外的可选项：一期与三期共用同一条回合链，状态必须一次建好。
pub(crate) struct AdvanceState {
    /// 已追加的追问回合数（**不含**首个回合）。
    ///
    /// 诊断计数，**不参与任何闸门**——上限判据在 `goal.exhausted()`。
    pub(crate) used: u32,
    /// 一期的 `HEAD` 追问是否已用过。
    ///
    /// **一期是「至多一次」**，而本循环每轮都拿同一个 `head_before` 重新比 ——
    /// 不加这个标记，追问回合自己又提交一次就会无限追问下去。
    pub(crate) git_follow_up_used: bool,
}

impl AdvanceState {
    pub(crate) fn new() -> Self {
        Self {
            used: 0,
            git_follow_up_used: false,
        }
    }
}

/// 一个回合结束后问一次：**还要不要再来一轮**。
///
/// `follow_up_on_commit` 是一期开关；关着时 `NoPlan` 直接终止（与一期「开关关 ⇒
/// 不追问」同一语义）。
///
/// 返回 [`Advance`] 而不是裸 `FollowUp`：调用方需要知道「这一轮该喂什么输入」。
///
/// `goal` 只在**判据给出「再跑一轮」之后**参与判断：预算耗尽时把它改写成
/// [`Advance::FinalReport`]（停下，但先跑一个收卷回合）。
pub(crate) fn advance(
    store: &TodoStore,
    state: &mut AdvanceState,
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    follow_up_on_commit: bool,
    head_before: Option<&str>,
    head_after: Option<String>,
    goal: Option<&GoalState>,
) -> Advance {
    let last_stop = match outcome {
        Ok(result) => result.stop_reason.clone(),
        // 失败回合：收场形态不可续，交给 `decide` 的第 2 条判据。
        Err(_) => StopReason::Failed,
    };
    let decision = decide(store, state.used, &last_stop, goal);
    match decision {
        // 预算耗尽的判定**必须留在 `Push` 分支内部**：`Stop` 是「已经决定收场」，
        // 若在 `match` 之前先判 `exhausted()`，「用户按 Esc」与「清单已清空」也会被
        // 改写成 `FinalReport` —— 本该直接收场的回合反而多跑一个回合。
        FollowUp::Push => match goal {
            Some(g) if g.exhausted() => Advance::FinalReport,
            _ => Advance::Push,
        },
        // **一期兜底**：没有清单 ⇒ 交给一期的 HEAD 判据（**同一份实现**，
        // 不在这里重写一遍条件 —— 两份条件必然漂移）。
        //
        // 与一期一致**至多一次**：追问回合自己也可能推进 `HEAD`，而 `head_before`
        // 是首个回合之前的取值 ⇒ 不记标记就会每轮都放行，退化成「一直追问」。
        //
        // 这里返回的 `Push` **不参与**预算判定：一期追问不属于三期的回合预算，
        // 且它至多一次。
        FollowUp::Stop(StopCause::NoPlan) => {
            if state.git_follow_up_used {
                return Advance::Stop(StopCause::NoPlan);
            }
            if crate::post_turn::should_follow_up(
                outcome,
                head_before,
                head_after,
                follow_up_on_commit,
            ) {
                state.git_follow_up_used = true;
                Advance::Push
            } else {
                Advance::Stop(StopCause::NoPlan)
            }
        }
        FollowUp::Stop(cause) => Advance::Stop(cause),
    }
}

/// [`advance`] 的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Advance {
    /// 再跑一轮。
    Push,
    /// 停下，理由见载荷。
    Stop(StopCause),
    /// 预算耗尽 ⇒ 停下，但**先跑一个收卷回合**。
    ///
    /// 与 [`Advance::Stop`] **互斥**，调用方必须把两者当不同动作处理：
    /// `Stop` = 直接收场；`FinalReport` = 再跑一个**不计入预算**的回合后再收场。
    FinalReport,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use ys_tools_todo::{TodoItem, TodoStatus};

    fn store_with(items: Vec<TodoItem>) -> Arc<TodoStore> {
        let store = TodoStore::open(None);
        store.apply(items).unwrap();
        store
    }

    fn ok_result(stop: StopReason) -> Result<ys_loop::RunResult, ys_loop::LoopError> {
        Ok(ys_loop::RunResult {
            stop_reason: stop,
            usage: ys_core::Usage::default(),
            cache_reading: None,
            compaction_usage: ys_core::Usage::default(),
            rounds: 1,
            final_message: None,
        })
    }

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    /// **一期行为不变**：没有清单 + 开关开 + HEAD 变了 ⇒ 追问一轮（与接线前一致）。
    #[test]
    fn no_plan_falls_back_to_git_follow_up() {
        let store = TodoStore::open(None);
        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                true,
                Some("a"),
                Some("b".into()),
                None
            ),
            Advance::Push
        );
    }

    /// 没有清单 + HEAD 没变 ⇒ 停（`NoPlan`），不白跑一轮。
    #[test]
    fn no_plan_without_head_change_stops() {
        let store = TodoStore::open(None);
        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                true,
                Some("a"),
                Some("a".into()),
                None
            ),
            Advance::Stop(StopCause::NoPlan)
        );
    }

    /// 开关关 ⇒ 一期不追问（与接线前 `should_follow_up(enabled=false)` 一致）。
    #[test]
    fn follow_up_disabled_stops() {
        let store = TodoStore::open(None);
        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                false,
                Some("a"),
                Some("b".into()),
                None
            ),
            Advance::Stop(StopCause::NoPlan)
        );
    }

    /// 有计划 ⇒ 三期说了算，一期的 HEAD 变化不参与（清单有 open 项 ⇒ 继续推）。
    #[test]
    fn plan_takes_precedence_over_git() {
        let store = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                false,
                None,
                None,
                None
            ),
            Advance::Push
        );
    }

    /// 清单清空 ⇒ 达成，即使 HEAD 变了也不再跑（三期优先）。
    ///
    /// 必须**先写入一项再标完成**：`apply` 在全终态时直接清空且**不推进 revision**
    /// （空表 ⇒ 无变化），而 `decide` 靠 `revision > 0` 区分「刚干完」与「从没写过清单」。
    #[test]
    fn all_done_stops_even_if_head_changed() {
        let store = TodoStore::open(None);
        store
            .apply(vec![item("修好登录超时", TodoStatus::InProgress)])
            .unwrap();
        store
            .apply(vec![item("修好登录超时", TodoStatus::Completed)])
            .unwrap();
        assert!(store.snapshot().is_empty(), "全终态后清单应被清空");
        assert!(store.revision() > 0, "清空必须留下 revision 痕迹");

        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                true,
                Some("a"),
                Some("b".into()),
                None
            ),
            Advance::Stop(StopCause::AllDone)
        );
    }

    /// **一期仍至多一次**：追问回合又推进了 `HEAD`（`head_before` 不变），
    /// 第二问必须停。变异：去掉 `git_follow_up_used` 标记 ⇒ 本用例红（返回 `Push`）。
    #[test]
    fn git_follow_up_happens_at_most_once() {
        let store = TodoStore::open(None);
        let mut state = AdvanceState::new();
        let first = advance(
            &store,
            &mut state,
            &ok_result(StopReason::Completed),
            true,
            Some("a"),
            Some("b".into()),
            None,
        );
        assert_eq!(first, Advance::Push);
        let second = advance(
            &store,
            &mut state,
            &ok_result(StopReason::Completed),
            true,
            Some("a"),
            Some("c".into()),
            None,
        );
        assert_eq!(second, Advance::Stop(StopCause::NoPlan), "一期追问至多一次");
    }

    /// 用户取消永远赢：清单还有 open 项也不续跑。
    #[test]
    fn cancelled_stops_even_with_open_items() {
        let store = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Cancelled),
                true,
                Some("a"),
                Some("b".into()),
                None
            ),
            Advance::Stop(StopCause::Aborted)
        );
    }

    /// 预算未耗尽（`turns = 199`）⇒ 照常继续推。
    ///
    /// 变异：把上限判据写成 `>= GOAL_TURN_CAP - 1` ⇒ 本用例变红（`FinalReport`）。
    #[test]
    fn goal_under_cap_pushes() {
        let store = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut goal = GoalState::new("修好 flaky 测试".into());
        for _ in 0..(crate::goal_state::GOAL_TURN_CAP - 1) {
            goal.consume();
        }
        assert!(!goal.exhausted(), "前置：预算还有一回合");

        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                false,
                None,
                None,
                Some(&goal)
            ),
            Advance::Push
        );
    }

    /// 预算耗尽（`turns = 200`）⇒ 停下，但先收卷。
    ///
    /// 变异：去掉 `Push` 分支里的 `exhausted()` 判定 ⇒ 本用例得到 `Push` 而变红。
    #[test]
    fn goal_at_cap_final_reports() {
        let store = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut goal = GoalState::new("修好 flaky 测试".into());
        for _ in 0..crate::goal_state::GOAL_TURN_CAP {
            goal.consume();
        }
        assert!(goal.exhausted(), "前置：预算已耗尽");

        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Completed),
                false,
                None,
                None,
                Some(&goal)
            ),
            Advance::FinalReport
        );
    }

    /// **无目标 ⇒ 绝不收卷**：`used` 再大也走清单逻辑（上限判据只在 `goal.exhausted()`）。
    ///
    /// 变异：把 `exhausted()` 判定挪到 `match decision` 之前、并改用 `used` 判上限 ⇒
    /// 本用例会看到 `FinalReport` 而变红。
    #[test]
    fn no_goal_never_final_reports() {
        let open = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut state = AdvanceState::new();
        state.used = 999;
        assert_eq!(
            advance(
                &open,
                &mut state,
                &ok_result(StopReason::Completed),
                false,
                None,
                None,
                None
            ),
            Advance::Push,
            "无目标 ⇒ 有 open 项就继续推"
        );

        // 全终态 ⇒ `Stop(AllDone)`，同样不是 `FinalReport`。
        let done = TodoStore::open(None);
        done.apply(vec![item("修好登录超时", TodoStatus::InProgress)])
            .unwrap();
        done.apply(vec![item("修好登录超时", TodoStatus::Completed)])
            .unwrap();
        let mut state = AdvanceState::new();
        state.used = 999;
        assert_eq!(
            advance(
                &done,
                &mut state,
                &ok_result(StopReason::Completed),
                false,
                None,
                None,
                None
            ),
            Advance::Stop(StopCause::AllDone),
            "无目标 ⇒ 清单收场就是 AllDone"
        );
    }

    /// **`Stop` 赢过预算耗尽**：已决定收场（用户 Esc）就不该再起收卷回合。
    ///
    /// 变异：把上限判定挪到 `decide` 之前（`exhausted()` 先返回 `FinalReport`）⇒
    /// 本用例得到 `FinalReport` 而变红。
    #[test]
    fn stop_wins_over_exhausted() {
        let store = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut goal = GoalState::new("修好 flaky 测试".into());
        for _ in 0..crate::goal_state::GOAL_TURN_CAP {
            goal.consume();
        }
        assert!(goal.exhausted(), "前置：预算已耗尽");

        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Cancelled),
                true,
                Some("a"),
                Some("b".into()),
                Some(&goal)
            ),
            Advance::Stop(StopCause::Aborted),
            "用户取消 ⇒ 直接收场，不再跑收卷回合"
        );
    }

    /// **取消不动账**：`Esc` 停下后目标仍在那儿，下一回合能接着干。
    ///
    /// “停下当前回合”与“作废目标”是两件事（只有 `/goal clear` 做后者）；本用例锁住
    /// 前者不会顺手把后者也做了。
    ///
    /// **守护的是接线契约，不是可注入的变异**：`advance` 收的是 `Option<&GoalState>`，
    /// 类型系统已经让它无法改预算（把形参改成 `&mut` 时本用例会先编译失败）。本用例
    /// 锁住的是**调用方的读法**：取消分支返回后，`turns()` 与 `marker_text()` 都没变。
    #[test]
    fn aborted_leaves_goal_state_untouched() {
        let store = store_with(vec![item("修好登录超时", TodoStatus::InProgress)]);
        let mut goal = GoalState::new("修好 flaky 测试".into());
        goal.consume();
        goal.consume();
        let turns_before = goal.turns();

        let mut state = AdvanceState::new();
        assert_eq!(
            advance(
                &store,
                &mut state,
                &ok_result(StopReason::Cancelled),
                true,
                Some("a"),
                Some("b".into()),
                Some(&goal)
            ),
            Advance::Stop(StopCause::Aborted),
            "前置：取消即收场"
        );

        assert_eq!(
            goal.turns(),
            turns_before,
            "取消不得消耗预算 —— 它是「停下这一回合」，不是「烧掉一个回合」"
        );
        assert_eq!(
            goal.marker_text(),
            GoalState::new("修好 flaky 测试".into()).marker_text(),
            "取消不得改动目标的注入块 —— 下个回合注入的还是同一份"
        );
    }
}
