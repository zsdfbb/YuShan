//! `render_block` —— 把清单渲染成推进上下文的那一段文本。**纯函数**。
//!
//! # 字节稳定是硬要求
//!
//! 同内容两次渲染必须**逐字节相同**。因此本模块**不得**引入任何进程外状态：
//! 没有时间戳、没有 turn 号、没有 session id、没有路径、没有随机量、没有 `HashMap` 迭代序。
//! 这是「状态没变就不注入」判据成立的前提 —— 判据比较的是内容，一旦渲染引入易变量，
//! 同样内容就会产出不同字节，判据随之失效。
//!
//! # 有界
//!
//! 清单进上下文的成本必须可算：块大小封顶 [`MAX_BLOCK_TOKENS`]（≈4 B/token ⇒
//! [`MAX_BLOCK_BYTES`]），单条内容另封顶 [`MAX_CONTENT_CHARS`] —— 否则一条超长内容就能
//! 把整块挤空，模型反而看不到计划。

use crate::item::{TodoItem, normalize_content};

/// 注入块的 token 上限（量级估算，4 B/token）。
pub const MAX_BLOCK_TOKENS: usize = 600;

/// 注入块的字节上限（由 [`MAX_BLOCK_TOKENS`] 折算）。
pub const MAX_BLOCK_BYTES: usize = MAX_BLOCK_TOKENS * 4;

/// 单条内容进入渲染时的字符上限（超出部分以 `…` 收尾）。
///
/// 这是**显示**上限，不影响 `TodoStore` 里存的原文 —— 截断只发生在这里。
pub const MAX_CONTENT_CHARS: usize = 240;

/// 渲染成 `<todo_list revision="…">` + 每项一行 + 省略统计。
///
/// 输出只由 `(items, revision)` 决定；`items` 的顺序即模型给的顺序（不排序、不去重）。
///
/// ⚠️ **不得含时间戳 / turn 号 / session id** —— 那些会让同样内容产出不同字节，
/// 而「字节稳定」是「状态没变就不注入」判据成立的前提。
pub fn render_block(items: &[TodoItem], revision: u64) -> String {
    let open = items
        .iter()
        .filter(|item| !item.status.is_terminal())
        .count();
    let terminal = items.len() - open;

    let mut out = String::with_capacity(64 + items.len() * 48);
    out.push_str(&format!("<todo_list revision=\"{revision}\">\n"));

    // 省略行按**最坏情况**（全部条目都被省略）预留长度：随展示条数增加它只会变短，
    // 故一次预留即可，不必迭代求解。
    let reserve = omission_line(items.len(), open, terminal).len() + 1;
    let closing = "</todo_list>\n".len();

    let mut shown = 0usize;
    for (index, item) in items.iter().enumerate() {
        let line = format!(
            "{}. [{}] {}\n",
            index + 1,
            item.status.as_str(),
            truncate_chars(&normalize_content(&item.content), MAX_CONTENT_CHARS)
        );
        if out.len() + line.len() + reserve + closing > MAX_BLOCK_BYTES {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }

    if shown < items.len() {
        let rest = &items[shown..];
        let rest_open = rest
            .iter()
            .filter(|item| !item.status.is_terminal())
            .count();
        let rest_terminal = rest.len() - rest_open;
        out.push_str(&omission_line(rest.len(), rest_open, rest_terminal));
    }

    out.push_str("</todo_list>\n");
    out
}

/// 省略统计行。**只由三个计数决定** —— 不含被省略项的内容，故块有界。
fn omission_line(omitted: usize, open: usize, terminal: usize) -> String {
    format!("… {omitted} more omitted ({open} open, {terminal} terminal)\n")
}

/// 按**字符**截断（不是字节）—— 字节切片会切在多字节字符中间，产出非法 UTF-8。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => {
            let mut truncated = text[..cut].to_string();
            truncated.push('…');
            truncated
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::TodoStatus;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(content, status)
    }

    /// 同内容两次渲染**逐字节相同**。
    ///
    /// 变异：往块里加时间戳 / turn 号 / session id / 随机量 ⇒ 本用例变红。
    /// 这是「状态没变就不注入」判据成立的前提，故它是本模块的第一条不变量。
    #[test]
    fn render_is_byte_stable_for_same_input() {
        let items = vec![
            item("读 design.md", TodoStatus::Completed),
            item("写 store.rs", TodoStatus::InProgress),
            item("写测试", TodoStatus::Pending),
        ];

        let first = render_block(&items, 7);
        // 中间夹一次无关渲染，确保没有隐藏的全局游标 / 计数器参与。
        let _ = render_block(&[], 0);
        let second = render_block(&items, 7);

        assert_eq!(first, second, "同内容两次渲染必须逐字节相同");
        assert_eq!(
            first.as_bytes(),
            second.as_bytes(),
            "字节级比较（`String` 相等已蕴含，此处显式钉住意图）"
        );
    }

    /// 块的形状：头 + 每项一行（`序号. [状态] 内容`）+ 尾；顺序即入参顺序。
    ///
    /// 变异：改成按内容排序 / 去重 ⇒ 顺序断言变红；漏掉状态名 ⇒ 逐行断言变红。
    #[test]
    fn render_shape_and_order() {
        let items = vec![
            item("第二项", TodoStatus::Cancelled),
            item("第一项", TodoStatus::Pending),
        ];

        let block = render_block(&items, 3);

        assert_eq!(
            block,
            "<todo_list revision=\"3\">\n\
             1. [cancelled] 第二项\n\
             2. [pending] 第一项\n\
             </todo_list>\n"
        );
    }

    /// 空清单也能渲染（头 + 尾），且同样字节稳定。
    #[test]
    fn render_empty_list() {
        assert_eq!(
            render_block(&[], 0),
            "<todo_list revision=\"0\">\n</todo_list>\n"
        );
    }

    /// 内容里的换行 / 连续空白被折叠成单个空格 ⇒ **一项恒占一行**。
    ///
    /// 变异：不折叠 ⇒ 一项变多行，块的行结构（以及按行解析的下游）失效。
    #[test]
    fn render_collapses_whitespace_so_each_item_is_one_line() {
        let items = vec![item("  第一行\n\t第二行   第三段  ", TodoStatus::Pending)];

        let block = render_block(&items, 1);

        assert_eq!(block.lines().count(), 3, "头 + 1 行 + 尾；实际：\n{block}");
        assert!(
            block.contains("1. [pending] 第一行 第二行 第三段\n"),
            "{block}"
        );
    }

    /// 超长内容按**字符**截断并加 `…`，绝不切在多字节字符中间。
    ///
    /// 变异：改成按字节切片 ⇒ 中文内容会 panic 或产出非法 UTF-8 ⇒ 本用例变红。
    #[test]
    fn render_truncates_long_content_on_char_boundary() {
        let long = "中".repeat(MAX_CONTENT_CHARS + 50);
        let block = render_block(&[item(&long, TodoStatus::Pending)], 1);

        assert!(block.contains('…'), "超长内容必须显式标出截断：{block}");
        // 截断后仍是一行，且仍是合法 UTF-8（`String` 本身即证明）。
        assert_eq!(block.lines().count(), 3);
        let content_part = block
            .lines()
            .nth(1)
            .and_then(|line| line.split_once("[pending] "))
            .map(|(_, content)| content.to_string())
            .expect("第二行是条目行");
        assert_eq!(content_part.chars().count(), MAX_CONTENT_CHARS + 1);
    }

    /// 条目多到装不下时：块**不超上限**，并给出省略统计。
    ///
    /// 变异：去掉上限（无条件渲染全部）⇒ 断言变红。
    #[test]
    fn render_caps_block_size_and_reports_omissions() {
        let items: Vec<TodoItem> = (0..400)
            .map(|i| {
                item(
                    &format!("第 {i} 项：把这件事做完并写清楚"),
                    TodoStatus::Pending,
                )
            })
            .collect();

        let block = render_block(&items, 9);

        assert!(
            block.len() <= MAX_BLOCK_BYTES,
            "块必须封顶 {} B，实际 {} B",
            MAX_BLOCK_BYTES,
            block.len()
        );
        assert!(
            block.contains("more omitted ("),
            "必须显式报出省略：{block}"
        );
        assert!(block.starts_with("<todo_list revision=\"9\">\n"));
        assert!(block.ends_with("</todo_list>\n"));
    }

    /// 省略行只由计数决定 ⇒ 同样输入同样省略行（字节稳定扩展到截断路径）。
    #[test]
    fn render_omission_is_deterministic() {
        let items: Vec<TodoItem> = (0..200)
            .map(|i| item(&format!("条目 {i}"), TodoStatus::Pending))
            .collect();
        assert_eq!(render_block(&items, 2), render_block(&items, 2));
    }
}
