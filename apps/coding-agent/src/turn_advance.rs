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

use crate::goal::StallTracker;
use crate::goal::decide;
use ys_core::{Message, StopReason};
use ys_model::Model;
use ys_tools_todo::TodoStore;

/// 一条回合链的推进状态（跨回合存活，故由调用方持有）。
///
/// **不是** `Default` 之外的可选项：一期与三期共用同一条回合链，状态必须一次建好。
pub(crate) struct AdvanceState {
    /// 已追加的追问回合数（**不含**首个回合）。
    pub(crate) used: u32,
    pub(crate) stall: StallTracker,
    /// 判官连续不可靠的次数。
    pub(crate) judge_failures: u32,
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
            stall: StallTracker::new(),
            judge_failures: 0,
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
pub(crate) fn advance(
    store: &TodoStore,
    state: &mut AdvanceState,
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    follow_up_on_commit: bool,
    head_before: Option<&str>,
    head_after: Option<String>,
) -> Advance {
    // `model_claims_done` 取 `last_stop == Completed`：模型主动停下（不再调工具、
    // 给出收尾正文）就是它的自陈。取结构事实而不是扫正文 —— 正文里「还没完成」
    // 随时会出现，而 `Completed` 不受措辞影响。
    let last_stop = match outcome {
        Ok(result) => result.stop_reason.clone(),
        // 失败回合：收场形态不可续，交给 `decide` 的第 2 条判据。
        Err(_) => StopReason::Failed,
    };
    let model_claims_done = matches!(last_stop, StopReason::Completed);
    let decision = decide(
        store,
        state.used,
        &last_stop,
        model_claims_done,
        &mut state.stall,
        &mut state.judge_failures,
    );
    match decision {
        FollowUp::Push => Advance::Push,
        // **一期兜底**：没有清单 ⇒ 交给一期的 HEAD 判据（**同一份实现**，
        // 不在这里重写一遍条件 —— 两份条件必然漂移）。
        //
        // 与一期一致**至多一次**：追问回合自己也可能推进 `HEAD`，而 `head_before`
        // 是首个回合之前的取值 ⇒ 不记标记就会每轮都放行，退化成「跑到上限为止」。
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

/// 判官证据段里的 **harness 事实**。
///
/// 这一段是 harness **自己观测到的**（不是模型产出的）：`HEAD` 前后、回合轮数、
/// 收场形态、剩余项数。它们是判官可以直接采信的前提 —— 比工具输出更硬，
/// 因为工具输出仍可能被模型伪造（它自己写了一份假日志）。
pub(crate) fn harness_facts(
    head_before: Option<&str>,
    head_after: &Option<String>,
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    open_items: usize,
    elapsed: Option<std::time::Duration>,
    tool_calls: usize,
) -> String {
    let head = match (head_before, head_after.as_deref()) {
        (Some(before), Some(after)) if before != after => {
            format!(
                "git_head: changed ({} -> {})",
                short_sha(before),
                short_sha(after)
            )
        }
        (Some(_), Some(_)) => "git_head: unchanged".to_string(),
        // 检测不可用（非 git 仓库 / 未装 git）—— 静默，不报错。
        _ => "git_head: unavailable".to_string(),
    };
    let (rounds, stop) = match outcome {
        Ok(result) => (result.rounds.to_string(), format!("{:?}", result.stop_reason)),
        Err(_) => ("?".to_string(), "Failed".to_string()),
    };
    // 耗时与工具调用数都是 **harness 观测到的事实**（不是模型自述）：
    // 前者能揭穿「一秒内完成了五项」这类断言，后者能揭穿「只调了一次就宣称全做完」。
    let elapsed = match elapsed {
        Some(d) => format!("{}ms", d.as_millis()),
        None => "unavailable".to_string(),
    };
    format!(
        "open_items: {open_items}\n{head}\nrounds: {rounds}\nstop_reason: {stop}\n\
         elapsed: {elapsed}\ntool_calls: {tool_calls}"
    )
}

/// `HEAD` 的短形式（前 12 字符）—— 判官不需要完整 sha，只需认出「变了」。
fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// 跑一次判官，把结论折算成 `judge_failures` 的**增量**。
///
/// - `0`：判官给出了可用结论（达成 / 未达成 / 阻塞），或**没有 open 项**（无需核对）；
/// - `1`：调用失败（重试后仍失败）、解析不可靠（含引用未命中），或无可用模型。
///
/// 两条入口（TUI 与 `-p` / `--json`）都调它 —— 判官接线只有这一份。
pub(crate) async fn judge_failure_delta(
    model: Option<&dyn Model>,
    session: &[Message],
    todo: &TodoStore,
    head_before: Option<&str>,
    head_after: &Option<String>,
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    elapsed: Option<std::time::Duration>,
) -> u32 {
    let open_items: Vec<_> = todo
        .snapshot()
        .into_iter()
        .filter(|item| !item.status.is_terminal())
        .collect();
    // 没清单 ⇒ 没有「要核对什么」，一次模型调用只会白花钱。
    if open_items.is_empty() {
        return 0;
    }
    let Some(model) = model else {
        return 1;
    };

    let window = crate::goal_judge_call::evidence_window(session);
    // 工具调用数从证据窗口里数：它就是「本回合真的调了几次工具」的直接读数，
    // 不另开一条统计通路（否则两份计数必然漂移）。
    let tool_calls = window.matches("[tool_call]").count();
    let facts = harness_facts(
        head_before,
        head_after,
        outcome,
        open_items.len(),
        elapsed,
        tool_calls,
    );
    let evidence = crate::goal_judge_call::evidence_text(&window, &facts);

    match crate::goal_judge_call::call_judge(model, &evidence, &open_items).await {
        Ok(raw) => match crate::goal_judge::parse_judge_output(&raw, &evidence, &open_items) {
            // 不可靠解析与调用失败同属「判据没给出可用结论」：连续两次即停。
            crate::goal_judge::JudgeParse::Unparseable => 1,
            _ => 0,
        },
        Err(_) => 1,
    }
}

/// [`advance`] 的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Advance {
    /// 再跑一轮。
    Push,
    /// 停下，理由见载荷。
    Stop(StopCause),
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
                Some("b".into())
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
                Some("a".into())
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
                Some("b".into())
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
                Some("b".into())
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
        );
        assert_eq!(first, Advance::Push);
        let second = advance(
            &store,
            &mut state,
            &ok_result(StopReason::Completed),
            true,
            Some("a"),
            Some("c".into()),
        );
        assert_eq!(
            second,
            Advance::Stop(StopCause::NoPlan),
            "一期追问至多一次"
        );
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
                Some("b".into())
            ),
            Advance::Stop(StopCause::Aborted)
        );
    }

    /// harness 事实必须含**耗时**与**工具调用数** —— 设计 §5.5 列的四项之二。
    ///
    /// 这两项是揭穿「模型自称完成」的硬事实：
    /// 耗时揭穿「一秒内做完了五项」，工具调用数揭穿「只调一次就宣称全做完」。
    #[test]
    fn harness_facts_carry_elapsed_and_tool_calls() {
        let facts = harness_facts(
            Some("aaaa"),
            &Some("bbbb".into()),
            &ok_result(StopReason::Completed),
            2,
            Some(std::time::Duration::from_millis(1500)),
            7,
        );
        assert!(facts.contains("open_items: 2"), "{facts}");
        assert!(facts.contains("git_head: changed"), "{facts}");
        assert!(facts.contains("rounds: "), "{facts}");
        assert!(facts.contains("stop_reason: Completed"), "{facts}");
        assert!(facts.contains("elapsed: 1500ms"), "{facts}");
        assert!(facts.contains("tool_calls: 7"), "{facts}");
    }

    /// 耗时未知时写 `unavailable` 而不是 `0ms`：后者是假事实，会让判官以为「瞬间完成」。
    #[test]
    fn harness_facts_never_fabricate_elapsed() {
        let facts = harness_facts(
            None,
            &None,
            &ok_result(StopReason::Completed),
            0,
            None,
            0,
        );
        assert!(facts.contains("elapsed: unavailable"), "{facts}");
        assert!(!facts.contains("elapsed: 0ms"), "不得编造零耗时：{facts}");
        assert!(facts.contains("git_head: unavailable"), "{facts}");
    }
}
