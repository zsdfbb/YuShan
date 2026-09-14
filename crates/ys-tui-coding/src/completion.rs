//! 补全浮层 —— Tab 触发、↑↓ 选择、Tab/Enter 填入、Esc 关闭（设计 §2.4）。
//!
//! **条目直接从命令表派生**（[`crate::commands::all_commands`]）：浮层里
//! 看得见的命令与 `parse` 认识的命令是同一张表，结构上不可能漂移。
//!
//! 浮层本身「盖在 Chat 上、不参与布局高度计算」的渲染在 `draw.rs`
//! （[`crate::draw`]），本模块只负责**候选集与选中态**这类纯逻辑。

use crate::commands::{CommandSpec, all_commands, spec_line};

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

/// 全部候选条目（= 全部命令），顺序与 [`all_commands`] 一致。
pub fn entries() -> Vec<CompletionItem> {
    all_commands().iter().map(item_for).collect()
}

/// 按前缀过滤（`prefix` 是**含前导 `/`** 的第一个词，如 `"/mo"`）。
///
/// `"/"` → 全部；`"/zzz"` → 空。
pub fn matches(prefix: &str) -> Vec<CompletionItem> {
    let bare = prefix.strip_prefix('/').unwrap_or(prefix);
    all_commands()
        .iter()
        .filter(|spec| spec.name.starts_with(bare))
        .map(item_for)
        .collect()
}

/// 尝试打开浮层：输入必须 `/` 开头且至少命中一条命令，否则 `None`（Tab 无效）。
pub fn open(input: &str) -> Option<CompletionState> {
    if !input.starts_with('/') {
        return None;
    }
    let token = input.split_whitespace().next().unwrap_or("");
    let items = matches(token);
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
}
