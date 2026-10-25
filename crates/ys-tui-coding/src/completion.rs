//! 补全浮层 —— Tab 触发、↑↓ 选择、Tab/Enter 填入、Esc 关闭（设计 §2.4）。
//!
//! **候选有两段来源，与 [`crate::commands::parse_with_skills`] 同源、同硬序**：
//!
//! ```text
//! 内置命令表（all_commands，顺序即表序）  →  在前
//! CodingView.skills（按 command 升序）   →  在后
//! ```
//!
//! 「内置 > skill」在补全面也成立：`command` 与内置命令同名的 skill **不进候选**
//! （否则浮层会出现两行一模一样的 `/status`，而敲下去走的必然是内置那条）。
//!
//! 浮层本身「盖在 Chat 上、不参与布局高度计算」的渲染在 `draw.rs`
//! （[`crate::draw`]），本模块只负责**候选集与选中态**这类纯逻辑。

use crate::commands::{CommandSpec, all_commands, spec_line};
use crate::view::SkillEntry;

/// 一个补全候选项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// 浮层里显示的一行（如 `/model [model_name]  — 切换模型`）。
    pub display: String,
    /// 选中后写回输入框的文本（如 `/model `）。
    pub replacement: String,
}

/// 浮层的交互状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionState {
    pub items: Vec<CompletionItem>,
    /// 当前高亮项（`items` 的下标；空列表时无意义）。
    pub selected: usize,
}

impl CompletionState {
    /// 当前选中项（越界返回 `None`）。
    pub fn current(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    /// 高亮上移（到头回卷）。
    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// 高亮下移（到尾回卷）。
    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }
}

/// 一条命令 → 一个候选项。
pub fn item_for(spec: &CommandSpec) -> CompletionItem {
    CompletionItem {
        display: spec_line(spec),
        // 尾随空格：填进输入框后接着敲参数，不必再按一次空格
        replacement: format!("/{} ", spec.name),
    }
}

/// skill 在「挑一条来用」的面上的显示文本：`<command>  — <description>`。
///
/// **这里列 description 是有意的** —— 两个面都是「挑一条来用」，描述正是挑的依据
/// （`/help` 那个面才只列名字，见 [`crate::commands::help_text_with_skills`]）。
/// 宽度由各自的浮层兜住：补全面是 `truncate_to_width` + `COMPLETION_MAX_WIDTH`，
/// 选择器是 `truncate_to_width` + `MODAL_MAX_WIDTH`（真实语料的 description 是
/// CJK 长句，两边都会被截断 —— 这是有意的「一屏一条」取舍，不是缺陷）。
///
/// 补全浮层与 `/skills` 选择器（[`crate::prompter::resolve_prompt`]）**共用**本函数 ——
/// 两个面列的是同一批 skill、同一套措辞，不会漂移。选择器还要靠它**反解**回
/// `command`（`Prompter::select` 只回传显示串），故这条格式是**承重的**。
pub fn skill_label(skill: &SkillEntry) -> String {
    format!("/{}  — {}", skill.command, skill.description)
}

/// 一条 skill → 一个候选项。
pub fn skill_item_for(skill: &SkillEntry) -> CompletionItem {
    CompletionItem {
        display: skill_label(skill),
        // 尾随空格：接着敲参数（与内置命令同形）
        replacement: format!("/{} ", skill.command),
    }
}

/// 可被用户调用、且**未被内置命令遮蔽**的 skill，按 `command` 升序。
///
/// 内置命令 > skill：同名的 skill 敲下去也永远走不到（`parse_with_skills` 先查内置表），
/// 故不进任何候选面。`/` 浮层与 `/skills` 选择器**共用**本函数 —— 两个面列同一批。
pub fn selectable_skills(skills: &[SkillEntry]) -> Vec<&SkillEntry> {
    let mut picked: Vec<&SkillEntry> = skills
        .iter()
        .filter(|s| !all_commands().iter().any(|c| c.name == s.command))
        .collect();
    picked.sort_by(|a, b| a.command.cmp(&b.command));
    picked
}

/// skill 段的候选项（`bare` 是**不含前导 `/`** 的前缀，`""` = 全部）。
fn skill_items(skills: &[SkillEntry], bare: &str) -> Vec<CompletionItem> {
    selectable_skills(skills)
        .into_iter()
        .filter(|s| s.command.starts_with(bare))
        .map(skill_item_for)
        .collect()
}

/// 全部候选条目（= 全部内置命令），顺序与 [`all_commands`] 一致。
///
/// **不含 skill** —— 等价于 [`entries_with_skills`]`(&[])`。
pub fn entries() -> Vec<CompletionItem> {
    entries_with_skills(&[])
}

/// 全部候选条目：**内置命令（在前）+ skill（在后，按 `command` 升序）**。
pub fn entries_with_skills(skills: &[SkillEntry]) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = all_commands().iter().map(item_for).collect();
    items.extend(skill_items(skills, ""));
    items
}

/// 按前缀过滤（`prefix` 是**含前导 `/`** 的第一个词，如 `"/mo"`）。
///
/// `"/"` → 全部；`"/zzz"` → 空。**不含 skill** —— 等价于
/// [`matches_with_skills`]`(prefix, &[])`。
///
/// 保留的薄包装：生产路径（`events.rs` 的 Tab 与 `draw.rs`）已统一走 `_with_skills`
/// 变体，本函数只剩既有测试在用 —— 但它与 [`entries`] / [`open`] 同为一组，
/// 是「无 skill 的调用形式」，**不删**。
#[allow(dead_code)]
pub fn matches(prefix: &str) -> Vec<CompletionItem> {
    matches_with_skills(prefix, &[])
}

/// 按前缀过滤内置命令 + skill（顺序同 [`entries_with_skills`]）。
pub fn matches_with_skills(prefix: &str, skills: &[SkillEntry]) -> Vec<CompletionItem> {
    let bare = prefix.strip_prefix('/').unwrap_or(prefix);
    let mut items: Vec<CompletionItem> = all_commands()
        .iter()
        .filter(|spec| spec.name.starts_with(bare))
        .map(item_for)
        .collect();
    items.extend(skill_items(skills, bare));
    items
}

/// 尝试打开浮层：输入必须 `/` 开头且至少命中一条命令，否则 `None`（Tab 无效）。
///
/// **不含 skill** —— 等价于 [`open_with_skills`]`(input, &[])`。保留的薄包装：
/// 生产路径（`events.rs` 的 Tab）已走 `open_with_skills`，本函数只剩既有测试在用。
#[allow(dead_code)]
pub fn open(input: &str) -> Option<CompletionState> {
    open_with_skills(input, &[])
}

/// 同 [`open`]，但候选里带上 skill（设计与 [`crate::commands::parse_with_skills`] 同源）。
pub fn open_with_skills(input: &str, skills: &[SkillEntry]) -> Option<CompletionState> {
    if !input.starts_with('/') {
        return None;
    }
    let token = input.split_whitespace().next().unwrap_or("");
    let items = matches_with_skills(token, skills);
    if items.is_empty() {
        return None;
    }
    Some(CompletionState { items, selected: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entries_mirror_the_command_table() {
        let items = entries();
        assert_eq!(items.len(), all_commands().len());
        for (item, spec) in items.iter().zip(all_commands()) {
            assert_eq!(
                item.display,
                format!("{}  — {}", prefix_with_hint(spec), spec.description)
            );
            assert_eq!(item.replacement, format!("/{} ", spec.name));
        }
    }

    fn prefix_with_hint(spec: &CommandSpec) -> String {
        match spec.arg_hint {
            Some(h) => format!("/{} {h}", spec.name),
            None => format!("/{}", spec.name),
        }
    }

    #[test]
    fn test_display_format_matches_design_mockup() {
        let model = entries()
            .into_iter()
            .find(|i| i.display.starts_with("/model"))
            .expect("命令表里应有 /model");
        assert_eq!(
            model.display,
            "/model [model_name]  — 切换模型（无参时浮层选择）"
        );
        assert_eq!(model.replacement, "/model ");
    }

    #[test]
    fn test_matches_filters_by_prefix() {
        let mo = matches("/mo");
        assert_eq!(mo.len(), 1, "只有 model：{mo:?}");
        assert_eq!(mo[0].replacement, "/model ");

        assert_eq!(matches("/").len(), all_commands().len(), "光 `/` 给全部");
        assert!(matches("/zzz").is_empty(), "无命中即空");
        assert!(matches("/c").iter().any(|i| i.replacement == "/copy "));
        assert!(matches("/c").iter().any(|i| i.replacement == "/compact "));
    }

    #[test]
    fn test_open_requires_slash_and_a_hit() {
        assert!(open("hello").is_none(), "非 / 开头不补全");
        assert!(open("").is_none());
        assert!(open("/zzz").is_none(), "无命中不开浮层");
        let state = open("/").expect("`/` 开全部");
        assert_eq!(state.selected, 0);
        assert_eq!(state.items.len(), all_commands().len());
    }

    /// 已经带上参数时只看**第一个词**，浮层照样能开（`/model ` 后按 Tab）。
    #[test]
    fn test_open_uses_first_token_only() {
        let state = open("/model ").expect("第一个词是 /model");
        assert_eq!(state.items.len(), 1);
        assert!(open("/model gpt-4o").is_some());
    }

    #[test]
    fn test_current_respects_bounds() {
        let state = CompletionState::default();
        assert!(state.current().is_none(), "空列表无选中项");

        let state = CompletionState {
            items: vec![CompletionItem {
                display: "/help".into(),
                replacement: "/help ".into(),
            }],
            selected: 1, // 越界
        };
        assert!(state.current().is_none());
    }

    #[test]
    fn test_current_returns_selected() {
        let state = CompletionState {
            items: vec![
                CompletionItem {
                    display: "a".into(),
                    replacement: "a".into(),
                },
                CompletionItem {
                    display: "b".into(),
                    replacement: "b".into(),
                },
            ],
            selected: 1,
        };
        assert_eq!(state.current().unwrap().display, "b");
    }

    #[test]
    fn test_move_up_down_wrap_around() {
        let mut state = open("/").unwrap();
        let n = state.items.len();
        assert!(n >= 3, "至少三条命令");

        state.move_down();
        assert_eq!(state.selected, 1);
        state.move_up();
        assert_eq!(state.selected, 0);
        state.move_up();
        assert_eq!(state.selected, n - 1, "到头回卷到尾");
        state.move_down();
        assert_eq!(state.selected, 0, "到尾回卷到头");
    }

    #[test]
    fn test_move_on_empty_state_is_noop() {
        let mut state = CompletionState::default();
        state.move_up();
        state.move_down();
        assert_eq!(state.selected, 0);
        assert!(state.current().is_none());
    }

    // -----------------------------------------------------------------------
    // skill 候选（与 `parse_with_skills` 同源、同硬序）
    // -----------------------------------------------------------------------

    fn skill(command: &str, description: &str) -> SkillEntry {
        SkillEntry {
            command: command.into(),
            label: command.into(),
            description: description.into(),
        }
    }

    /// 内置在前、skill 在后，且 skill 段按 `command` **升序**（与传入顺序无关）。
    ///
    /// 变异：把 skill 段 `extend` 到内置之前 ⇒ 本测试变红。
    #[test]
    fn test_entries_with_skills_orders_builtin_first_then_sorted_skills() {
        let skills = [skill("zeta", "z"), skill("alpha", "a"), skill("mid", "m")];
        let items = entries_with_skills(&skills);
        let builtin_len = all_commands().len();
        assert_eq!(items.len(), builtin_len + 3);

        for (i, spec) in all_commands().iter().enumerate() {
            assert_eq!(
                items[i].replacement,
                format!("/{} ", spec.name),
                "内置段保持表序"
            );
        }
        let skill_section: Vec<&str> = items[builtin_len..]
            .iter()
            .map(|i| i.replacement.as_str())
            .collect();
        assert_eq!(
            skill_section,
            vec!["/alpha ", "/mid ", "/zeta "],
            "skill 段按 command 升序"
        );

        let one = entries_with_skills(&[skill("alpha", "甲")]);
        assert_eq!(one[builtin_len].display, "/alpha  — 甲");
        assert_eq!(one[builtin_len].replacement, "/alpha ");
    }

    /// 与内置命令同名的 skill **不进候选**（「内置命令 > skill」在补全面同样成立）。
    ///
    /// 变异：去掉 `skill_items` 里的同名过滤 ⇒ 浮层出现两行 `/status`，本测试变红。
    #[test]
    fn test_skill_shadowed_by_builtin_is_not_offered() {
        let skills = [skill("status", "想遮蔽 /status"), skill("zeta", "z")];
        let items = entries_with_skills(&skills);
        assert_eq!(items.len(), all_commands().len() + 1, "同名 skill 被吃掉");
        assert_eq!(
            items.iter().filter(|i| i.replacement == "/status ").count(),
            1
        );
        assert!(items.iter().any(|i| i.replacement == "/zeta "));
    }

    /// `matches_with_skills` 两段都按前缀过滤。
    #[test]
    fn test_matches_with_skills_filters_both_sections() {
        let skills = [skill("commit", "提交"), skill("compact-notes", "笔记")];
        let got: Vec<String> = matches_with_skills("/co", &skills)
            .into_iter()
            .map(|i| i.replacement)
            .collect();
        assert_eq!(
            got,
            vec!["/copy ", "/compact ", "/commit ", "/compact-notes "],
            "内置（表序）在前，skill（升序）在后"
        );

        let only: Vec<String> = matches_with_skills("/commit", &skills)
            .into_iter()
            .map(|i| i.replacement)
            .collect();
        assert_eq!(only, vec!["/commit "], "只有 skill 命中时也返回");
        assert!(matches_with_skills("/zzz", &skills).is_empty());
    }

    /// `open_with_skills`：skill 命中也能开浮层；无命中仍 `None`（Tab 无效）。
    #[test]
    fn test_open_with_skills_hits_and_misses() {
        let skills = [skill("git-status", "看仓库状态")];
        assert!(
            open_with_skills("hello", &skills).is_none(),
            "非 / 开头不补全"
        );
        assert!(
            open_with_skills("/zzz", &skills).is_none(),
            "无命中不开浮层"
        );

        let state = open_with_skills("/git", &skills).expect("skill 命中即开浮层");
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].replacement, "/git-status ");
        assert_eq!(state.items[0].display, "/git-status  — 看仓库状态");
        assert_eq!(state.selected, 0);

        assert!(
            open_with_skills("/git-status x", &skills).is_some(),
            "只看第一个词"
        );
    }

    /// 薄包装的正确性：`entries` / `matches` / `open` ≡ 各自的 `_with_skills(.., &[])`。
    #[test]
    fn test_legacy_wrappers_equal_empty_skills_variants() {
        assert_eq!(entries(), entries_with_skills(&[]));
        for prefix in ["/", "/m", "/mo", "/zzz", "mo"] {
            assert_eq!(
                matches(prefix),
                matches_with_skills(prefix, &[]),
                "前缀 `{prefix}`"
            );
        }
        for input in ["/", "/mo", "/model ", "/zzz", "hello", ""] {
            assert_eq!(open(input), open_with_skills(input, &[]), "输入 `{input}`");
        }
    }

    /// `/theme` 在**三个面**上同源：命令表（15 条）、补全浮层（`/them` → `theme`、
    /// `/th` → thinking + theme）、`/help` 正文。
    ///
    /// 变异：从 `COMMANDS` 里删掉 `theme` 条目 ⇒ 下面三条断言全部变红。
    #[test]
    fn test_theme_in_commands_and_completion() {
        let specs = all_commands();
        assert_eq!(specs.len(), 15, "命令表应有 15 条（新增 theme）");
        let theme = specs
            .iter()
            .find(|s| s.name == "theme")
            .expect("命令表里必须有 theme");
        assert_eq!(theme.arg_hint, Some("[name]"));
        assert!(!theme.description.is_empty());

        // 输入 `/them`：唯一命中就是 theme（`/th` 也会命中 `/thinking`），且 Tab 接受后
        // 的回填串带尾随空格。
        let hits = open("/them").expect("`/them` 应开浮层");
        assert_eq!(hits.items.len(), 1, "只有 theme：{hits:?}");
        assert_eq!(hits.items[0].replacement, "/theme ");
        assert_eq!(hits.items[0].display, "/theme [name]  — 切换配色主题");
        // 短前缀 `/th` 则不应漏掉 theme（与 thinking 并列命中，两者都在）。
        let both: Vec<String> = matches_with_skills("/th", &[])
            .into_iter()
            .map(|i| i.replacement)
            .collect();
        assert_eq!(both, vec!["/thinking ", "/theme "], "表序：thinking 在前");

        // `/help` 正文同样看得到（与浮层同源）。
        assert!(
            crate::commands::help_text().contains("/theme [name]"),
            "`/help` 必须列出 /theme"
        );
    }
}
