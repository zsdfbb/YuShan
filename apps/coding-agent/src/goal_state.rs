//! 一个任务的活动目标 —— 状态与两段文案，判据不在本模块。
//!
//! # 契约
//!
//! - **生命周期 = 一个任务**（D-O）：`/goal` 设下、`/goal clear` 作废。
//!   **不落盘**：`/resume` 之后目标是新任务的空白起点。
//! - **回合计数不含收卷回合**（D-N）：[`GoalState::consume`] 只由追问循环推进；
//!   收卷回合（[`GoalState::wrap_up_text`]）在循环之外跑，跑完不再 `consume`。
//! - **注入块字节只由目标原文决定**（[`GoalState::marker_text`]）：不含回合序号、
//!   不含剩余预算 —— 每回合都变的话每回合都是一笔全价尾字节（破前缀缓存），
//!   而模型知道这些也**不能**改变行为，结局由 harness 判定。
//! - **无清单提醒有上限**（[`GoalState::plan_remind`]）：它只改追问输入的文案，
//!   不改变判据结论、不占回合额度（设计 §5.2）。
//!
//! 达成判定靠「清单清空」⇒ 目标块必须把「先拆成可验收的清单项、每完成一项就更新清单」
//! 写进去：这是 R1（“达成”退化为“清单清空”）唯一的非技术缓解。

/// 唯一上限（D-J / D-K）。常量、不读配置（D-D）。
pub(crate) const GOAL_TURN_CAP: u32 = 200;

/// 连续 N 回合无清单提醒的上限。
pub(crate) const NO_PLAN_REMIND_LIMIT: u32 = 5;

/// 一个任务的**活动目标**。生命周期 = 一个任务（D-O）。
pub(crate) struct GoalState {
    /// 用户原话，逐字保留。注入块与收卷块都引它。
    text: String,
    /// 已消耗的回合数（**不含**收卷回合）。
    turns: u32,
    /// 已发出的无清单提醒次数（见 §5.2）。
    plan_reminders: u32,
}

impl GoalState {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            turns: 0,
            plan_reminders: 0,
        }
    }

    pub(crate) fn turns(&self) -> u32 {
        self.turns
    }

    /// 记一个已消耗的回合。饱和加 —— 计数只是诊断量，溢出没有意义。
    pub(crate) fn consume(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    /// 预算是否耗尽 ⇒ 调用方在循环外起收卷回合（D-M）。
    pub(crate) fn exhausted(&self) -> bool {
        self.turns >= GOAL_TURN_CAP
    }

    /// 每个回合注入的目标块（`Role::Harness`）。首行含 `<goal_brief`。
    ///
    /// **字节只由 `text` 决定**：不含回合序号、不含剩余预算、不含任何随时间变化的量。
    /// 每回合都变的话每回合都是一笔全价尾字节（破前缀缓存），而模型知道还剩多少回合
    /// 也不能改变行为 —— 结局由 harness 判定（[`Self::exhausted`]）。
    ///
    /// 这一段同时是达成判定的可观测面：判定靠清单清空，所以必须约束「先拆成可验收的
    /// 清单项、每完成一项就更新清单」（R1 的缓解）。
    pub(crate) fn marker_text(&self) -> String {
        format!(
            "<goal_brief>\n\
             本任务的目标（用户原话，逐字保留）：\n\
             {text}\n\
             \n\
             要求：\n\
             - 动手之前先把目标拆成可验收的清单项，用 `todo` 工具全量写入：\n\
             一项一条，写到「指着它就能说做完了」的粒度，不要只写笼统的一行。\n\
             - 每完成一项就更新清单，不要攒到最后一次性更新。\n\
             - 未完成的清单项优先推进；清单全部完成即视为目标达成。\n\
             - 目标是否达成由清单判定，不由你觉得「差不多了」判定。\n\
             </goal_brief>",
            text = self.text
        )
    }

    /// 收卷回合的输入（`Role::Harness`）：预算耗尽，要求把账讲清。
    ///
    /// **不含回合序号**：序号对收卷没有用，且写进去就成了与预算无关的易漂字节。
    pub(crate) fn wrap_up_text(&self) -> String {
        format!(
            "本任务的回合预算已耗尽，现在收尾 —— 不要再开新工作。\n\
             \n\
             目标（用户原话）：\n\
             {text}\n\
             \n\
             请一次性讲清三件事：\n\
             - 已经完成的部分：具体做了什么、改在哪些文件、怎么验证的；\n\
             - 仍未完成的部分：具体差什么，以及要继续做下去的话下一步是什么；\n\
             - 已知的问题或风险：没验证到的假设、绕过去的坑、需要人来定的取舍。\n\
             \n\
             清单里仍未完成的项要逐项点到，不要含糊带过。",
            text = self.text
        )
    }

    /// 无清单提醒：前 [`NO_PLAN_REMIND_LIMIT`] 次返回前缀文案，之后 `None`。
    ///
    /// 它**只改追问输入的文案**：判据（有目标 + 清单仍为空 ⇒ `Push`）的结论不变，
    /// 也不占回合额度 —— 提醒不发回合。上限的作用是别把同一句提醒无限重复进上下文。
    pub(crate) fn plan_remind(&mut self) -> Option<&'static str> {
        if self.plan_reminders >= NO_PLAN_REMIND_LIMIT {
            return None;
        }
        self.plan_reminders += 1;
        Some("还没写清单。先把这个目标拆成可验收的清单项（`todo` 工具全量写入），再动手。")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两次渲染**逐字节相同**，且不含任何随时间变化的量。
    ///
    /// 恒等是前缀缓存的前提：只要渲染里出现回合序号 / 剩余预算，每回合都是一笔
    /// 全价尾字节。断言「整段没有 ASCII 数字」比逐项挑更宽 —— 任何回合序号写法
    /// （第 7 回合 / turn 7 / 剩 193）都会带来数字。
    ///
    /// 变异：把 `turns` 写进 `marker_text` ⇒ 本用例变红。
    #[test]
    fn marker_text_is_byte_identical_across_calls() {
        let mut goal = GoalState::new("修好 flaky 测试".into());
        let first = goal.marker_text();
        for _ in 0..7 {
            goal.consume();
        }
        let later = goal.marker_text();

        assert_eq!(first, later, "注入块不得随回合数变化");
        assert!(
            !first.chars().any(|c| c.is_ascii_digit()),
            "注入块不得含数字（回合序号 / 剩余预算都会以数字形式出现）：{first}"
        );
        assert!(
            !first.to_lowercase().contains("turn"),
            "注入块不得含 turn 字样：{first}"
        );
        assert!(!first.contains("200"), "注入块不得含上限 200");
    }

    /// 首行含标记 —— 调用方靠它在会话历史里判存在性（与 `todo_inject::TODO_MARKER` 同契约）。
    #[test]
    fn marker_text_carries_marker() {
        let goal = GoalState::new("整理日志目录".into());
        let text = goal.marker_text();
        let first_line = text.lines().next().expect("注入块非空");

        assert!(
            first_line.contains("<goal_brief"),
            "首行必须是标记行：{first_line}"
        );
    }

    /// 第 199 次 `consume` 仍可续，第 200 次即耗尽（上限含当回合）。
    #[test]
    fn consume_advances_toward_cap() {
        let mut goal = GoalState::new("跑完整套测试".into());
        assert!(!goal.exhausted(), "新目标不该一上来就耗尽");

        for i in 0..199 {
            goal.consume();
            assert!(!goal.exhausted(), "第 {} 个回合不该耗尽", i + 1);
        }
        assert_eq!(goal.turns(), GOAL_TURN_CAP - 1);

        goal.consume();
        assert_eq!(goal.turns(), GOAL_TURN_CAP);
        assert!(goal.exhausted(), "第 200 个回合必须耗尽预算");
    }

    /// 前 [`NO_PLAN_REMIND_LIMIT`] 次给文案，第 6 次起静默。
    ///
    /// 变异：把上限判断改成 `> NO_PLAN_REMIND_LIMIT` ⇒ 第 6 次仍 `Some`，本用例变红。
    #[test]
    fn plan_remind_caps_at_limit() {
        let mut goal = GoalState::new("拆解需求".into());

        for i in 0..NO_PLAN_REMIND_LIMIT {
            let prefix = goal.plan_remind();
            assert!(prefix.is_some(), "第 {} 次提醒应给文案", i + 1);
        }
        assert_eq!(goal.plan_reminders, NO_PLAN_REMIND_LIMIT);

        assert!(
            goal.plan_remind().is_none(),
            "第 6 次起不再提醒 —— 静默推进，出口是 Esc"
        );
        assert!(goal.plan_remind().is_none(), "提醒一旦封顶就不再恢复");
        assert_eq!(
            goal.plan_reminders, NO_PLAN_REMIND_LIMIT,
            "返回 None 时计数不得再涨"
        );
    }

    /// 收卷块必须带着目标原文 —— 收卷回合看不到原话就只能凭记忆汇报。
    #[test]
    fn wrap_up_text_quotes_goal() {
        let text = "把 /goal 的收卷回合接上，并且写清还剩什么没做";
        let goal = GoalState::new(text.into());
        let wrap_up = goal.wrap_up_text();

        assert!(wrap_up.contains(text), "收卷块必须逐字引用目标原文");
        assert!(
            wrap_up.contains("未完成"),
            "收卷块必须要求讲清仍未完成的部分"
        );
        assert!(!wrap_up.contains("200"), "收卷块不得含回合序号 / 上限");
    }
}
