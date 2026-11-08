//! 回合结束后的**推进判据**（三期「清单 + 证据验收」，设计见
//! `docs/arch/cross-turn-advance/design.md` §5.8，决策见 ADR-0072 D10）。
//!
//! 本模块只回答一个问题：**要不要再起一轮**。它**不问判官**（判官在 `goal_judge.rs`），
//! 不 await、不碰 model、不读 session —— 入参全是调用方已经拿到的事实。
//!
//! # 判据顺序即语义
//!
//! [`decide`] 的闸门**按顺序短路**，顺序本身承载语义：
//!
//! 1. 用户取消永远赢 —— 排在「清单已清空」之前，因为「达成」不能覆盖用户的 Ctrl+C；
//! 2. 回合收场形态不可续（截断 / 失败 / 轮数用尽 / 无产出）；
//! 3. 判官连续不可靠（没有判据就没有停止条件 ⇒ 放行等于跑飞）；
//! 4. 无 open 项 ⇒ **唯一的正常收场**；
//! 5. 从没写过清单 ⇒ 普通回合；
//! 6. 追问轮数用尽；
//! 7. 原地打转（集合指纹连续不变 **且** 模型自称收尾）；
//! 8. 其余 ⇒ 再起一轮。
//!
//! 3 与 4–5 的先后是刻意的：判官坏了但清单恰好清空时，报「达成」会掩盖判官故障，
//! 而报「判官不可靠」让人能去查判官 —— 后者才是需要人介入的那件事。
//!
//! # stall 判据与推进判据同源
//!
//! 停滞信号来自**清单集合指纹**（[`stall_digest`]），不是命令输出：反复跑同一条测试
//! 命令时输出带不同耗时（`3 failed (2.1s)` / `(2.3s)`）⇒ 哈希输出会漏检，而清单是
//! 离散集合，不受日志噪声影响。


use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ys_core::StopReason;
use ys_protocol::{FollowUp, StopCause};
use ys_tools_todo::{TodoItem, TodoStore, normalize_content};

/// 一个回合内最多再起几轮。到点即 [`StopCause::CapReached`]。
///
/// 这是**硬闸门**：判据若还能放行而轮数无上界，一次跑飞就是无上界的钱。
pub const MAX_FOLLOW_UPS: u32 = 8;

/// 集合指纹连续不变的回合数达到它 ⇒ 判定原地打转（还需模型自称收尾）。
///
/// 取 3 而非 2：修 bug 常见的「改 → 测 → 再改 → 再测」需要至少两轮往返，
/// 2 轮会把正常的多轮迭代误判成卡死。
pub const STALL_LIMIT: u32 = 3;

/// 判官连续不可靠的次数达到它 ⇒ 停止（不再起回合）。
pub const JUDGE_FAILURE_LIMIT: u32 = 2;

/// 剩余 todo 的集合指纹（**只算非终态项**）。
///
/// 归一化链逐字照设计：折叠空白 → 小写 → 排序 → 去重 → 喂 [`DefaultHasher`]。
/// 每一项的哈希输入是「归一化内容 ‖ 状态」—— 状态进摘要，`Pending → InProgress`
/// 才算「有进展」；只哈希内容会让状态推进被当成原地打转。
///
/// **只算非终态项**：已完成的项不参与。否则模型把已完成项重新描述一遍就会重置 stall，
/// 而它一个字的新活也没干。
///
/// 用 `DefaultHasher` 而非 `sha256`：本机制只比**相邻两轮**（O(1)、不存历史、不落盘、
/// 不跨进程）⇒ 不需要密码学强度，也不需要跨 Rust 版本的稳定性保证。
pub fn stall_digest(items: &[TodoItem]) -> u64 {
    let mut keys: Vec<(String, &'static str)> = items
        .iter()
        .filter(|item| !item.status.is_terminal())
        .map(|item| {
            (
                normalize_content(&item.content).to_lowercase(),
                item.status.as_str(),
            )
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();

    let mut hasher = DefaultHasher::new();
    for (content, status) in &keys {
        content.hash(&mut hasher);
        status.hash(&mut hasher);
    }
    hasher.finish()
}

/// 连续未变的计数（stall 闸门的唯一状态）。
///
/// 只比**相邻两轮**：不存历史、不落盘、不跨进程 —— 重启后从「首轮观察」重新开始，
/// 与 [`stall_digest`] 不需要跨版本稳定性的理由同源。
#[derive(Debug, Default, Clone)]
pub struct StallTracker {
    last: Option<u64>,
    unchanged_turns: u32,
}

impl StallTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一轮的集合指纹，返回**连续观察到同一指纹的回合数**（首次观察为 1）。
    ///
    /// 指纹变了 ⇒ 计数归零后重新从 1 计 —— 「有进展」与「没进展」是同一把尺子的两端。
    pub fn observe(&mut self, digest: u64) -> u32 {
        match self.last {
            Some(prev) if prev == digest => self.unchanged_turns += 1,
            _ => {
                self.last = Some(digest);
                self.unchanged_turns = 1;
            }
        }
        self.unchanged_turns
    }

    /// 连续观察到同一指纹的回合数。
    pub fn unchanged_turns(&self) -> u32 {
        self.unchanged_turns
    }
}

/// 回合结束后问一次：要不要再起一轮？**纯函数、同步、不 await、不碰 model**。
///
/// 会**就地推进** `stall` 的计数（每回合恰好观察一次集合指纹），其余入参只读。
///
/// `model_claims_done` 是「模型自称收尾」：本实现取 `last_stop == StopReason::Completed`
/// —— 模型主动停下（不再调工具、给出收尾正文）就是它的自陈。取 `stop_reason` 而不是
/// 扫描助手正文：正文里「完成了」三个字随时会出现在**否定句**里（「还没完成」），
/// 而 `Completed` 是运行时观测到的结构事实，不受措辞影响。
///
/// 它与 stall 取 **AND**（ADR-0024 决策 d）：只靠集合不变会误杀「一轮里干了活但还没
/// 更新状态」的情形；加上模型也说自己收尾了，才判定为真打转。
pub fn decide(
    store: &TodoStore,
    used: u32,
    last_stop: &StopReason,
    model_claims_done: bool,
    stall: &mut StallTracker,
    judge_failures: &mut u32,
) -> FollowUp {
    // 每回合观察一次集合指纹。放在最前面是为了让 stall 计数与「本回合结束时的清单」
    // 严格一一对应 —— 闸门短路后不再观察，计数就不会因停机路径而错位。
    stall.observe(stall_digest(&store.snapshot()));

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

    // ── 判据 3：判官连续不可靠 ────────────────────────────────────────
    // 没有判据就没有停止条件 ⇒ 放行等于跑飞。计数由判官调用点维护（成功即归零）。
    if *judge_failures >= JUDGE_FAILURE_LIMIT {
        return FollowUp::Stop(StopCause::JudgeUnreliable);
    }

    // ── 判据 4：无 open 项 ⇒ 唯一的正常收场 ──────────────────────────
    // 判据是**无 open 项**，不是「全部 `Completed`」：被放弃（`cancelled`）的项同样终结，
    // 否则一个放弃的项会永久卡住收场条件。
    //
    // `revision > 0` 区分「刚干完」与「根本没计划」：`TodoStore::apply` 在无 open 项时
    // 清空列表，因此两种情形在快照上长得一样，只能靠「写过没有」分辨。
    if !store.has_open_items() && store.revision() > 0 {
        return FollowUp::Stop(StopCause::AllDone);
    }

    // ── 判据 5：从没写过清单 ⇒ 普通回合 ──────────────────────────────
    // 「无 open 项且清单非空」被 `apply` 的硬规则 2 排除，故此处剩下的空清单只有
    // 「从未写过」一种来源。
    if store.snapshot().is_empty() {
        return FollowUp::Stop(StopCause::NoPlan);
    }

    // ── 判据 6：追问轮数用尽 ──────────────────────────────────────────
    if used >= MAX_FOLLOW_UPS {
        return FollowUp::Stop(StopCause::CapReached);
    }

    // ── 判据 7：原地打转 ──────────────────────────────────────────────
    if stall.unchanged_turns() >= STALL_LIMIT && model_claims_done {
        return FollowUp::Stop(StopCause::Stalled);
    }

    // ── 判据 8：再起一轮 ──────────────────────────────────────────────
    FollowUp::Push
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ys_tools_todo::TodoStatus;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(content, status)
    }

    /// 纯内存 store（`open(None)`）—— 不落盘、不碰临时目录。
    fn store_with(items: Vec<TodoItem>) -> Arc<TodoStore> {
        let store = TodoStore::open(None);
        store.apply(items).expect("测试用的清单应被接受");
        store
    }

    /// 一次调用所需的全部状态。
    struct Env {
        stall: StallTracker,
        judge_failures: u32,
    }

    impl Env {
        fn new() -> Self {
            Self {
                stall: StallTracker::new(),
                judge_failures: 0,
            }
        }

        /// 「模型自称收尾」的默认取法：`Completed` 即自陈。
        fn decide(&mut self, store: &TodoStore, used: u32, stop: StopReason) -> FollowUp {
            let claims_done = stop == StopReason::Completed;
            decide(
                store,
                used,
                &stop,
                claims_done,
                &mut self.stall,
                &mut self.judge_failures,
            )
        }
    }

    // ── 判据 1：取消永远赢 ────────────────────────────────────────────

    /// `Cancelled` ⇒ `Stop(Aborted)`，即便清单**已经清空**（AllDone 的条件也成立）。
    ///
    /// 变异：把判据 1 挪到判据 4（无 open 项）之后 ⇒ 本用例得到 `AllDone` 而变红。
    #[test]
    fn cancelled_wins_over_all_done() {
        let store = store_with(vec![item("写判据", TodoStatus::Pending)]);
        store
            .apply(vec![item("写判据", TodoStatus::Completed)])
            .expect("全终态 ⇒ 清空");
        assert!(!store.has_open_items(), "前置：清单已无 open 项");

        let mut env = Env::new();
        assert_eq!(
            env.decide(&store, 0, StopReason::Cancelled),
            FollowUp::Stop(StopCause::Aborted),
            "用户 Ctrl+C 永远优先于「达成」"
        );
    }

    // ── 判据 2：不可续的收场形态 ──────────────────────────────────────

    /// 四种不可续收场各触发一次 `Stop(NotContinuable)`，即使清单还有 open 项。
    ///
    /// 变异：把 `Truncated` 从 match 里删掉 ⇒ 它会落到通配臂，本用例仍绿；
    /// 故变异取「整条判据 2 删掉」⇒ 本用例变红。
    #[test]
    fn non_continuable_stop_reasons_stop() {
        for stop in [
            StopReason::Truncated,
            StopReason::Failed,
            StopReason::MaxRounds,
            StopReason::NoOutput,
        ] {
            let store = store_with(vec![item("写判据", TodoStatus::InProgress)]);
            let mut env = Env::new();
            assert_eq!(
                env.decide(&store, 0, stop.clone()),
                FollowUp::Stop(StopCause::NotContinuable),
                "{stop:?} 不该续跑"
            );
        }
    }

    // ── 判据 3：判官不可靠 ────────────────────────────────────────────

    /// 判官连续失败达阈值 ⇒ `Stop(JudgeUnreliable)`，优先于「清单还开着」。
    ///
    /// 变异：把判据 3 的 `>=` 改成 `>`（少一次）⇒ 本用例变红。
    #[test]
    fn judge_failures_reaching_limit_stop() {
        let store = store_with(vec![item("写判据", TodoStatus::Pending)]);
        let mut env = Env::new();
        env.judge_failures = JUDGE_FAILURE_LIMIT;

        assert_eq!(
            env.decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::JudgeUnreliable)
        );

        // 少一次失败 ⇒ 仍可续跑（阈值是「连续 N 次」）。
        let mut env = Env::new();
        env.judge_failures = JUDGE_FAILURE_LIMIT - 1;
        assert_eq!(
            env.decide(&store, 0, StopReason::Completed),
            FollowUp::Push,
            "未达阈值不得停"
        );
    }

    // ── 判据 4：唯一的正常收场 ────────────────────────────────────────

    /// 曾经写过清单、现在无 open 项 ⇒ `Stop(AllDone)`。
    ///
    /// 同时钉住「判据是无 open 项而非全部 `Completed`」：一项 `completed` + 一项
    /// `cancelled` 也算收场。
    ///
    /// 变异：把判据 4 的 `!has_open_items()` 改成「所有项都是 `Completed`」⇒
    /// 含 `cancelled` 的那次断言变红。
    #[test]
    fn no_open_items_is_all_done() {
        // 先写一份**开着**的计划，再把它全部终结 —— `apply` 的硬规则 2 会清空清单，
        // 于是「曾经写过、现在无 open 项」与「从没写过」在快照上长得一样。
        let store = store_with(vec![item("写完的", TodoStatus::Pending)]);
        store
            .apply(vec![
                item("写完的", TodoStatus::Completed),
                item("放弃的", TodoStatus::Cancelled),
            ])
            .expect("全终态 ⇒ 清空");
        assert!(!store.has_open_items(), "前置：apply 应清空全终态清单");
        assert!(store.revision() > 0, "前置：写过清单");

        let mut env = Env::new();
        assert_eq!(
            env.decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::AllDone)
        );
    }

    // ── 判据 5：从没写过清单 ──────────────────────────────────────────

    /// 全新 store（从未写过）⇒ `Stop(NoPlan)`，不是 `AllDone`。
    ///
    /// 变异：把判据 4 的 `revision > 0` 去掉 ⇒ 全新 store 会被判成 `AllDone`，本用例变红。
    #[test]
    fn never_written_plan_is_no_plan() {
        let store = TodoStore::open(None);
        assert_eq!(store.revision(), 0, "前置：从未写过");
        assert!(store.snapshot().is_empty(), "前置：清单为空");

        let mut env = Env::new();
        assert_eq!(
            env.decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::NoPlan)
        );
    }

    // ── 判据 6：轮数用尽 ──────────────────────────────────────────────

    /// `used >= MAX_FOLLOW_UPS` ⇒ `Stop(CapReached)`。
    ///
    /// 变异：把 `>=` 改成 `>` ⇒ 恰好等于阈值那一次变红。
    #[test]
    fn used_up_to_cap_stops() {
        let store = store_with(vec![item("写判据", TodoStatus::Pending)]);
        let mut env = Env::new();
        assert_eq!(
            env.decide(&store, MAX_FOLLOW_UPS, StopReason::Completed),
            FollowUp::Stop(StopCause::CapReached)
        );
        assert_eq!(
            env.decide(&store, MAX_FOLLOW_UPS - 1, StopReason::Completed),
            FollowUp::Push,
            "未到上限不得停"
        );
    }

    // ── 判据 7：原地打转 ──────────────────────────────────────────────

    /// 集合指纹连续 `STALL_LIMIT` 轮不变 **且** 模型自称收尾 ⇒ `Stop(Stalled)`。
    ///
    /// 变异：去掉 `model_claims_done`（只靠集合不变）⇒ 本用例第一条断言变红
    /// （第 2 轮就会停）。
    #[test]
    fn stalled_requires_unchanged_set_and_model_claims_done() {
        let store = store_with(vec![item("改同一行", TodoStatus::InProgress)]);
        let mut env = Env::new();

        // 前 `STALL_LIMIT` 轮：集合不变，但模型没说收尾 ⇒ 即便计数已到阈值也**不得**判打转
        // （「一轮里干了活但还没更新状态」正是要避开的情形）。
        for round in 1..=STALL_LIMIT {
            assert_eq!(
                decide(
                    &store,
                    0,
                    &StopReason::Completed,
                    false,
                    &mut env.stall,
                    &mut env.judge_failures,
                ),
                FollowUp::Push,
                "模型没自称收尾时不得判打转（第 {round} 轮）"
            );
        }
        assert_eq!(
            env.stall.unchanged_turns(),
            STALL_LIMIT,
            "前置：计数已达阈值"
        );
        // 集合仍不变，且模型自称收尾 ⇒ 打转。
        assert_eq!(
            env.decide(&store, 0, StopReason::Completed),
            FollowUp::Stop(StopCause::Stalled),
            "连续 {} 轮集合不变 + 自称收尾 ⇒ 打转",
            STALL_LIMIT
        );
    }

    /// 集合变了 ⇒ stall 计数归零，即使判官仍说 `NOT_MET`（有进展就是有进展）。
    ///
    /// 变异：把 `observe` 的归零分支删掉（永远 `+= 1`）⇒ 本用例变红。
    #[test]
    fn changed_set_resets_stall() {
        let store = store_with(vec![item("第一步", TodoStatus::InProgress)]);
        let mut env = Env::new();

        // 先攒到 STALL_LIMIT - 1 轮不变。
        for _ in 0..(STALL_LIMIT - 1) {
            env.decide(&store, 0, StopReason::Completed);
        }
        assert_eq!(env.stall.unchanged_turns(), STALL_LIMIT - 1);

        // 清单变了（推进一项、加一项）⇒ 计数归零。
        store
            .apply(vec![
                item("第一步", TodoStatus::Completed),
                item("第二步", TodoStatus::Pending),
            ])
            .expect("清单可推进");
        assert_eq!(env.decide(&store, 0, StopReason::Completed), FollowUp::Push);
        assert_eq!(
            env.stall.unchanged_turns(),
            1,
            "集合变了必须归零重计，否则打转判据会跨着不同集合累加"
        );
    }

    // ── 判据 8：续跑 ──────────────────────────────────────────────────

    /// 清单有 open 项、无闸门拦下 ⇒ `Push`。
    #[test]
    fn open_items_push() {
        let store = store_with(vec![item("写判据", TodoStatus::Pending)]);
        let mut env = Env::new();
        assert_eq!(env.decide(&store, 0, StopReason::Completed), FollowUp::Push);
    }

    // ── 指纹 ──────────────────────────────────────────────────────────

    /// 同一集合、不同顺序 ⇒ 同值（排序）。
    ///
    /// 变异：删掉 `keys.sort_unstable()` ⇒ 本用例变红。
    #[test]
    fn digest_is_order_insensitive() {
        let a = vec![
            item("读设计", TodoStatus::Pending),
            item("写实现", TodoStatus::InProgress),
        ];
        let b = vec![
            item("写实现", TodoStatus::InProgress),
            item("读设计", TodoStatus::Pending),
        ];
        assert_eq!(stall_digest(&a), stall_digest(&b));
    }

    /// 空白差异 ⇒ 同值（折叠空白）。
    ///
    /// 变异：把 `normalize_content` 从链上删掉 ⇒ 本用例变红。
    #[test]
    fn digest_ignores_whitespace() {
        let a = vec![item("读  设计\n并 实现", TodoStatus::Pending)];
        let b = vec![item("读 设计 并 实现", TodoStatus::Pending)];
        assert_eq!(stall_digest(&a), stall_digest(&b));
    }

    /// 大小写差异 ⇒ 同值（小写）。
    ///
    /// 变异：删掉 `.to_lowercase()` ⇒ 本用例变红。
    #[test]
    fn digest_ignores_case() {
        let a = vec![item("Fix The Bug", TodoStatus::Pending)];
        let b = vec![item("fix the bug", TodoStatus::Pending)];
        assert_eq!(stall_digest(&a), stall_digest(&b));
    }

    /// 状态变化（`Pending → InProgress`）⇒ 变值：状态推进就是有进展。
    ///
    /// 变异：把 `status` 从哈希输入里删掉（只哈希内容）⇒ 本用例变红。
    #[test]
    fn digest_changes_with_status() {
        let a = vec![item("写判据", TodoStatus::Pending)];
        let b = vec![item("写判据", TodoStatus::InProgress)];
        assert_ne!(stall_digest(&a), stall_digest(&b));
    }

    /// **已完成项被重述 ⇒ 值不变**（只算非终态项）。
    ///
    /// 这是 stall 判据不被模型绕过的关键：把已完成项换个措辞重写一遍不算进展。
    ///
    /// 变异：去掉 `!item.status.is_terminal()` 过滤（算全部项）⇒ 本用例变红。
    #[test]
    fn digest_ignores_terminal_items() {
        let a = vec![
            item("已完成的事", TodoStatus::Completed),
            item("还在做的事", TodoStatus::InProgress),
        ];
        let b = vec![
            item("已完成的事（重述一遍）", TodoStatus::Completed),
            item("还在做的事", TodoStatus::InProgress),
        ];
        let c = vec![
            item("已完成的事", TodoStatus::Completed),
            item("另一件已完成的事", TodoStatus::Cancelled),
            item("还在做的事", TodoStatus::InProgress),
        ];
        assert_eq!(
            stall_digest(&a),
            stall_digest(&b),
            "重述已完成项不得重置 stall"
        );
        assert_eq!(
            stall_digest(&a),
            stall_digest(&c),
            "新增终态项不得重置 stall"
        );
    }

    /// 空清单的指纹是常量 —— 它只在停机路径上被观察到，不参与「有进展」判定。
    #[test]
    fn digest_of_empty_plan_is_stable() {
        assert_eq!(stall_digest(&[]), stall_digest(&[]));
        assert_ne!(
            stall_digest(&[]),
            stall_digest(&[item("有活", TodoStatus::Pending)])
        );
    }
}
