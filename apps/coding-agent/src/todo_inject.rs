//! 回合边界的清单注入 —— 判据与渲染都是纯函数。
//!
//! # 契约
//!
//! - **判据**：清单非空 **且**（`revision` 变了 **或** 会话历史里已找不到 [`TODO_MARKER`]）。
//!   前者堵住「同一份清单在上下文里堆出 N 份互相矛盾的副本」；后者是**自愈**。
//! - **自愈覆盖压缩**：压缩把注入块摘要掉后，历史里随之找不到标记 ⇒ 下一个回合边界
//!   自动补一份。因此本模块**不监听** `ContextCompacted` 事件 —— 手敲 `/compact`
//!   走的是同一条压缩路径但**不发**那条事件，判据只看历史事实，两条路径都覆盖。
//! - **字节稳定**：[`injection_text`] 只由 `(items, revision)` 决定（说明行是常量，
//!   正文来自 [`ys_tools_todo::render_block`]），同内容两次调用逐字节相同。
//!
//! 注入消息的角色由调用点决定：用 `Role::Harness` —— 出站时映射成 user（模型可见
//! 效果与用户发言相同），但本地记录与 TUI 都不把它当作用户自己敲的话。

use ys_core::{ContentBlock, Message};
use ys_tools_todo::{TodoStore, render_block};

/// 注入块的定位标记 —— [`should_inject`] 靠它在会话历史里找「上一份注入还在不在」。
///
/// 取渲染块的开标签本身：要误命中它，模型得在正文里原样写出 `<todo_list`。
pub(crate) const TODO_MARKER: &str = "<todo_list";

/// 要不要在回合边界注入清单。
///
/// 语义严格是：`非空 && (revision 变 ∨ 会话里找不到 TODO_MARKER)`。
/// `last_injected_rev` 是**本进程**上一次注入时的 `revision`（`None` = 还没注入过）；
/// 它不跨进程 —— 重启后版本号从 0 重来，靠的是「找不到标记就补」那条分支兜底。
pub(crate) fn should_inject(
    store: &TodoStore,
    last_injected_rev: Option<u64>,
    session: &[Message],
) -> bool {
    if store.snapshot().is_empty() {
        return false;
    }
    if last_injected_rev != Some(store.revision()) {
        return true;
    }
    !session.iter().any(contains_marker)
}

/// 渲染注入块正文：一行说明 + [`render_block`] 的原样产物。首行含 [`TODO_MARKER`]。
///
/// 两次读（`snapshot` / `revision`）之间若有写入，最坏结果是这一份比真值旧一版；
/// 下一个回合边界 `revision` 不等，会再注一份。
pub(crate) fn injection_text(store: &TodoStore) -> String {
    let items = store.snapshot();
    let block = render_block(&items, store.revision());
    format!(
        "当前计划清单（由 `todo` 工具全量重写；`<todo_list>` 块即最新状态，未完成项优先）：\n{block}"
    )
}

/// 该消息是否已经带着清单。
///
/// 正文块与工具结果块**都算**：`todo` 工具回喂的块与 [`injection_text`] 里的渲染同源
/// （同 `items`、同 `revision`），它还在历史里就说明模型已经看得见这一版清单，
/// 再注一份只是重复计费。
fn contains_marker(message: &Message) -> bool {
    message.content.iter().any(|block| match block {
        ContentBlock::Text { text } => text.contains(TODO_MARKER),
        ContentBlock::ToolResult { content, .. } => content.contains(TODO_MARKER),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::Role;
    use ys_tools_todo::{TodoItem, TodoStatus};

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(content, status)
    }

    /// 纯内存 store（`open(None)`）—— 不落盘、不碰临时目录。
    fn store_with(items: Vec<TodoItem>) -> std::sync::Arc<TodoStore> {
        let store = TodoStore::open(None);
        store.apply(items).expect("测试用的清单应被接受");
        store
    }

    fn text_message(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 与调用点同形的一条注入消息。
    fn injected(store: &TodoStore) -> Message {
        Message {
            role: Role::Harness,
            content: vec![ContentBlock::Text {
                text: injection_text(store),
            }],
        }
    }

    /// 空清单 ⇒ 永不注入（`revision` 相等与否都不注入）。
    #[test]
    fn empty_plan_never_injects() {
        let store = TodoStore::open(None);

        assert!(
            !should_inject(&store, None, &[]),
            "没有计划就没有可注入的内容"
        );
        assert!(
            !should_inject(&store, Some(store.revision()), &[]),
            "版本号相等且清单为空 ⇒ 不注入"
        );
    }

    /// 非空 + 还没注入过（`last_injected_rev == None`）⇒ 注入。
    #[test]
    fn first_turn_with_plan_injects() {
        let store = store_with(vec![item("写注入判据", TodoStatus::Pending)]);

        assert!(should_inject(&store, None, &[]), "首次见到计划必须注入");
    }

    /// 非空 + `revision` 未变 + 历史里已有标记 ⇒ 不注入（稳态零成本）。
    #[test]
    fn unchanged_revision_with_marker_in_history_skips() {
        let store = store_with(vec![item("写注入判据", TodoStatus::InProgress)]);
        let rev = store.revision();
        let session = vec![
            text_message(Role::User, "开始吧"),
            injected(&store),
            text_message(Role::Assistant, "好的"),
        ];

        assert!(
            !should_inject(&store, Some(rev), &session),
            "历史里那份就是当前版本，再注一份只是重复计费"
        );
    }

    /// 非空 + `revision` 未变 + 历史里**没有**标记（模拟压缩吃掉）⇒ 注入（自愈）。
    #[test]
    fn lost_marker_reinjects() {
        let store = store_with(vec![item("写注入判据", TodoStatus::InProgress)]);
        let rev = store.revision();
        // 压缩后的历史：只剩摘要，注入块被吃掉了。
        let session = vec![
            text_message(Role::User, "开始吧"),
            text_message(Role::Assistant, "已读 design.md，正在写 store.rs"),
        ];

        assert!(
            should_inject(&store, Some(rev), &session),
            "标记丢失即补回 —— 压缩（含手敲 /compact）之后必须自愈"
        );
    }

    /// 非空 + `revision` 变了 ⇒ 注入（即便历史里还有旧标记）。
    #[test]
    fn revision_change_injects_even_if_marker_present() {
        let store = store_with(vec![item("写注入判据", TodoStatus::Pending)]);
        let session = vec![injected(&store)];
        let stale_rev = store.revision();

        store
            .apply(vec![item("写注入判据", TodoStatus::Completed)])
            .expect("全终态 ⇒ 清空");
        assert_ne!(store.revision(), stale_rev, "清空必须推进版本号");

        // 清空后清单为空 ⇒ 判据先被「非空」挡住。
        assert!(
            !should_inject(&store, Some(stale_rev), &session),
            "清空后不再注入"
        );

        // 再写一份新计划（版本号继续变）⇒ 注入。
        store
            .apply(vec![item("写自愈测试", TodoStatus::Pending)])
            .expect("写新计划");
        assert!(
            should_inject(&store, Some(stale_rev), &session),
            "版本号变了 ⇒ 必须注入，哪怕历史里还留着旧那份"
        );
    }

    /// 注入正文的首行携带标记，其后就是 `render_block` 的原样产物；同内容逐字节稳定。
    #[test]
    fn injection_text_first_line_carries_marker() {
        let store = store_with(vec![
            item("读 design.md", TodoStatus::Completed),
            item("写注入判据", TodoStatus::Pending),
        ]);

        let text = injection_text(&store);
        let (first, rest) = text.split_once('\n').expect("至少两行");
        assert!(first.contains(TODO_MARKER), "首行必须带定位标记：{first}");
        assert_eq!(
            rest,
            render_block(&store.snapshot(), store.revision()),
            "正文必须是 render_block 的原样产物"
        );
        assert_eq!(text, injection_text(&store), "同内容两次渲染必须逐字节相同");
    }
}
