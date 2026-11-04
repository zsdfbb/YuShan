//! 回合级工具输出总量闸门：**算法唯一宿主**。
//!
//! 单条工具结果已由第一批收进边界（grep 专用预算 / `ys_tool::MAX_BYTES`），但一个
//! 回合里的**多条**结果仍可累积放大（真实会话 37 条 ≈ 48k token）。本模块给出回合级判据：
//! 一个回合内所有工具结果估算 token 之和超预算时，降级**更早**的结果（头部截断 + 续读指针），
//! 把当轮回喂 token 压下来。
//!
//! 形状与 [`crate::compact`] 一致 —— 纯函数、零 IO、无 async、与
//! [`ys_component::RuntimeContext`] 完全解耦（只吃 `&[Message]`），故可直接单元测试，
//! 并由 `basic.rs` 的 Step 10 之后接线。
//!
//! **不改变块类型与 `tool_call_id`**（ADR-0001 配对完整性）：降级只缩 `content`，
//! 绝不增删消息或工具结果块 —— 会话消息条数不变。

use ys_core::{ContentBlock, Message};

/// 降级后保留的**头部行数**。
pub const ELIDE_HEAD_LINES: usize = 40;
/// 降级后保留的**头部字节数**。
pub const ELIDE_HEAD_BYTES: usize = 2 * 1024;
/// 降级标记（**幂等哨兵**）：已含本标记的内容不再二次降级。
pub const ELIDED_MARKER: &str = "[earlier tool result elided";

/// 一段消息里所有 `ToolResult` 块的估算 token 之和。
///
/// 只数工具结果块（正文 / 工具调用参数不计），以免把回合里的用户正文也算进预算。
pub fn tool_result_tokens(messages: &[Message]) -> usize {
    messages.iter().map(message_tool_result_tokens).sum()
}

/// 单条消息里 `ToolResult` 块的估算 token 之和。
fn message_tool_result_tokens(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult { content, .. } => crate::token::estimate_text_tokens(content),
            // 图片**有意**不计入（`_ => 0`）：`turn_budget` 的超支靠 `elide_message` 缩短
            // `ToolResult` 的文本来降，而 `Image` 块在 `elide_message` 里无法缩短。若计入，
            // 超支源于图片时 `plan_elisions` 会给出「降不掉」的方案 ⇒ 每回合空转。
            // 图片成本由 `crate::token::IMAGE_TOKEN_ESTIMATE` 反映。
            _ => 0,
        })
        .sum()
}

/// 该内容是否**需要降级**：非空、未含降级标记、且超过头部预算（行数或字节数其一超限）。
pub fn needs_elision(content: &str) -> bool {
    if content.is_empty() || content.contains(ELIDED_MARKER) {
        return false;
    }
    let lines: Vec<&str> = content.split('\n').collect();
    ys_tool::head_lines(&lines, ELIDE_HEAD_LINES, ELIDE_HEAD_BYTES)
        .hit
        .is_some()
}

/// 固定指针文案（便于断言）：报出被丢弃的 KiB 数，并给一句可执行的下一步。
fn elision_notice(dropped_bytes: usize) -> String {
    let kib = dropped_bytes.div_ceil(1024);
    format!(
        "[earlier tool result elided to keep this turn's output bounded (~{kib} KiB dropped). \
         Re-run the tool with a narrower scope (pattern/path/context, or read a line range) if \
         you need the full result]"
    )
}

/// 把一条消息里所有需要降级的 `ToolResult` 块降级为「头部 + 指针 notice」。
///
/// **不改变块类型与 `tool_call_id`**；无可降级块时返回 `None`（调用方据此跳过重写）。
pub fn elide_message(msg: &Message) -> Option<Message> {
    let mut changed = false;
    let content = msg
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if needs_elision(content) => {
                changed = true;
                let lines: Vec<&str> = content.split('\n').collect();
                let head = ys_tool::head_lines(&lines, ELIDE_HEAD_LINES, ELIDE_HEAD_BYTES);
                let dropped = content.len().saturating_sub(head.content.len());
                let mut new_content = head.content;
                ys_tool::append_notices(&mut new_content, &[elision_notice(dropped)]);
                ContentBlock::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    content: new_content,
                    is_error: *is_error,
                }
            }
            other => other.clone(),
        })
        .collect();
    if changed {
        Some(Message {
            role: msg.role.clone(),
            content,
        })
    } else {
        None
    }
}

/// 计划本回合要降级的消息下标。
///
/// 只看 `messages[turn_start..]`：`budget == 0` 或该区间工具结果估算 token 不超预算 ⇒
/// 返回空。否则从**最旧**到最新逐个降级（计入降级后的残量），直到剩余估算 token ≤ `budget`；
/// 返回**升序绝对下标**。**区间内最新一条永不降级**。
pub fn plan_elisions(messages: &[Message], turn_start: usize, budget: usize) -> Vec<usize> {
    if budget == 0 || turn_start >= messages.len() {
        return Vec::new();
    }
    let last = messages.len() - 1;
    if tool_result_tokens(&messages[turn_start..]) <= budget {
        return Vec::new();
    }

    // 模拟降级：每步都按**降级后**的残量判定，保证能压到预算内就压到预算内。
    let mut sim: Vec<Message> = messages.to_vec();
    let mut plan: Vec<usize> = Vec::new();
    for idx in turn_start..messages.len() {
        if idx == last || tool_result_tokens(&sim[turn_start..]) <= budget {
            break;
        }
        if let Some(elided) = elide_message(&sim[idx]) {
            sim[idx] = elided;
            plan.push(idx);
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::Role;

    fn big_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i:04} ................................................"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_result_msg(id: &str, content: String) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ys_core::ToolCallId(id.to_string()),
                content,
                is_error: false,
            }],
        }
    }

    fn plain_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn result_content(msg: &Message) -> &str {
        msg.content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .unwrap()
    }

    fn image_block(path: &str) -> ContentBlock {
        ContentBlock::Image {
            path: path.to_string(),
            detail: Some(ys_core::ImageDetail::High),
        }
    }

    /// 带 `media` 折叠后的形态：一条消息 = `ToolResult` + N 个 `Image`。
    fn tool_result_msg_with_images(id: &str, content: String, images: usize) -> Message {
        let mut msg = tool_result_msg(id, content);
        for i in 0..images {
            msg.content.push(image_block(&format!("/tmp/img-{i}.png")));
        }
        msg
    }

    /// N15 / 边界：`needs_elision` 只在「非空 + 未降级 + 超头部预算」时为真。
    ///
    /// 变异自检：去掉 `content.contains(ELIDED_MARKER)` 那条 ⇒ 已降级内容会被二次降级，
    /// 幂等用例与「已降级」断言变红；把 `is_empty()` 去掉 ⇒ 空内容误判为需要降级。
    #[test]
    fn needs_elision_boundaries() {
        assert!(!needs_elision(""), "空内容无需降级");
        assert!(!needs_elision("short\ncontent"), "预算内的小内容无需降级");
        assert!(
            needs_elision(&big_lines(200)),
            "行数/字节超限应判定需要降级"
        );
        // 已降级（含哨兵）的内容不再降级 —— 幂等的关键。
        let once = elide_message(&tool_result_msg("c1", big_lines(200))).unwrap();
        assert!(!needs_elision(result_content(&once)));
    }

    /// N10（算法内核）：超预算只降**更早**的结果、**保留最新**，且不改消息条数。
    ///
    /// 变异自检：把 `idx == last` 的保护去掉 ⇒ 最新一条会被降级，`plan` 断言变红；
    /// 把 `tool_result_tokens(&sim..) <= budget` 判定拿掉 ⇒ 会无谓降级全部更早结果，
    /// 只在必要时降级的断言变红。
    #[test]
    fn plan_elisions_elides_oldest_keeps_newest() {
        let msgs = vec![
            plain_msg("hello"),                    // 0：非工具结果
            tool_result_msg("c1", big_lines(200)), // 1：旧的大结果
            tool_result_msg("c2", big_lines(200)), // 2：最新的大结果
        ];
        let total = tool_result_tokens(&msgs[1..]);
        let budget = total / 2; // 只用降级一条即可压到预算内
        let plan = plan_elisions(&msgs, 1, budget);

        assert_eq!(
            plan,
            vec![1],
            "应降最旧的 1，且最新 2 永不降级；plan = {plan:?}"
        );
        assert!(!plan.contains(&2), "最新一条永不降级");
        assert!(!plan.contains(&0), "区间外 / 非工具结果不降级");

        // 实际降级后：条数不变、最新一条逐字原样、旧的一条含指针标记且变小。
        let mut applied = msgs.clone();
        for &i in &plan {
            applied[i] = elide_message(&applied[i]).unwrap();
        }
        assert_eq!(applied.len(), msgs.len(), "降级不得增删消息");
        assert_eq!(
            result_content(&applied[2]),
            result_content(&msgs[2]),
            "最新逐字原样"
        );
        assert!(
            result_content(&applied[1]).contains(ELIDED_MARKER),
            "旧结果应含指针标记"
        );
        assert!(
            result_content(&applied[1]).len() < result_content(&msgs[1]).len(),
            "旧结果应真的变小"
        );
        // 降级后要么压进预算，要么只剩最新未降级。
        let after = tool_result_tokens(&applied[1..]);
        assert!(
            after <= budget || plan.len() == 1,
            "after = {after}, budget = {budget}"
        );
    }

    /// 当轮的大批量结果**只保住一轮**。
    ///
    /// 保护条件是「是**最新**一条消息」（`idx == last`），而最新那条恒为本轮刚追加的工具
    /// 结果消息。故：一批结果在**紧随其后的那次决策**里是完整的，但下一轮再追加一条之后，
    /// 它就不再是最新 ⇒ 被降级成头部 + 续读指针。一次要得越多，掉得越多。
    ///
    /// 变异自检：把 `plan_elisions` 的 `idx == last` 改成 `idx == messages.len()` ⇒ 最新一条
    /// 也会被降级，第一条断言变红。
    #[test]
    fn plan_elisions_protects_only_the_newest_message() {
        let msgs = vec![
            plain_msg("hello"),                    // 0：非工具结果
            tool_result_msg("c1", big_lines(200)), // 1：第一轮的大批量
            tool_result_msg("c2", big_lines(200)), // 2：第二轮
        ];
        // 取总量的 1/4 ⇒ 即使只剩一条未降级也仍超预算，排除「本来就没超」的假绿。
        let budget = tool_result_tokens(&msgs[1..]) / 4;

        // 第一轮刚结束时：区间里只有刚追加的那条，它是最新 ⇒ 受保护，一条都不降。
        let after_first = plan_elisions(&msgs[..2], 1, budget);
        assert!(
            after_first.is_empty(),
            "当轮结果恒受最新保护，即便超预算也不降：{after_first:?}"
        );

        // 第二轮结束：第一轮那条已不再是最新 ⇒ 被降级。
        let after_second = plan_elisions(&msgs, 1, budget);
        assert_eq!(
            after_second,
            vec![1],
            "上一轮的大批量此时才被削成头部 + 指针：{after_second:?}"
        );
    }

    /// N15 / 边界：`budget == 0`（闸门关闭）或未超预算 ⇒ 计划为空（会话逐字节不变）。
    ///
    /// 变异自检：把 `budget == 0` 的早退删掉 ⇒ 关闭闸门时仍会降级，本用例变红。
    #[test]
    fn plan_elisions_empty_when_disabled_or_within_budget() {
        let msgs = vec![
            plain_msg("hi"),
            tool_result_msg("c1", big_lines(200)),
            tool_result_msg("c2", big_lines(200)),
        ];
        assert!(plan_elisions(&msgs, 1, 0).is_empty(), "budget=0 必须空");
        let total = tool_result_tokens(&msgs[1..]);
        assert!(
            plan_elisions(&msgs, 1, total).is_empty(),
            "刚好等于预算 ⇒ 不降级"
        );
        assert!(
            plan_elisions(&msgs, 1, total + 1000).is_empty(),
            "预算内 ⇒ 不降级"
        );
    }

    /// N15：`elide_message` 保留 `tool_call_id` 与块类型；对非工具结果 / 已降级内容返回 `None`。
    ///
    /// 变异自检：让降级改写 `tool_call_id` ⇒ 配对完整性断言变红；让非工具结果也返回 `Some`
    /// ⇒ 「不误伤」断言变红。
    #[test]
    fn elide_message_preserves_identity_and_is_idempotent() {
        let original = tool_result_msg("call-42", big_lines(200));
        let elided = elide_message(&original).expect("大结果应可降级");

        assert_eq!(elided.content.len(), 1, "块个数不变");
        match &elided.content[0] {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id.0, "call-42", "tool_call_id 必须不变");
                assert!(!*is_error, "is_error 原样保留");
                assert!(content.contains(ELIDED_MARKER), "应含指针标记");
            }
            other => panic!("块类型被改写：{other:?}"),
        }

        // 幂等：对已降级消息再调用 ⇒ 返回 None（内容不再变）。
        assert!(elide_message(&elided).is_none(), "已降级内容不得二次降级");

        // 不误伤：非工具结果消息返回 None。
        assert!(elide_message(&plain_msg("just text")).is_none());
        // 小工具结果（预算内）返回 None。
        assert!(elide_message(&tool_result_msg("c", "tiny".into())).is_none());
    }

    // ==================== T16t：图片**不计入**回合预算（C7，反向用例） ====================
    //
    // 这里把「不做」钉成要求：图片既然降不掉，就不能进入闸门的分子。

    /// T16t-1：含 `Image` 块（且有对应 `ToolResult`）的消息 ⇒ `tool_result_tokens` 只算
    /// `ToolResult` 文本，加图前后**相等**。
    ///
    /// 变异自检：把 `_ => 0` 改成计入 `crate::token::IMAGE_TOKEN_ESTIMATE` ⇒ 两值不再相等，
    /// 本用例红。
    #[test]
    fn tool_result_tokens_ignores_image_blocks() {
        let text = "https://example.com/a.png".to_string();
        let without_images = vec![tool_result_msg("c1", text.clone())];
        let with_images = vec![tool_result_msg_with_images("c1", text, 3)];

        assert!(
            crate::token::estimate_tokens(&with_images[0]) > 0,
            "前置：图片确实被 token 估算器看见"
        );
        assert_eq!(
            tool_result_tokens(&with_images),
            tool_result_tokens(&without_images),
            "回合预算只看 ToolResult 文本：加 3 张图不得改变分子"
        );
    }

    /// T16t-2：只有 `Image` 块、文本为空的`ToolResult` ⇒ `tool_result_tokens == 0`。
    #[test]
    fn tool_result_tokens_zero_for_empty_text_with_images() {
        let msgs = vec![tool_result_msg_with_images("c1", String::new(), 2)];
        assert_eq!(
            tool_result_tokens(&msgs),
            0,
            "文本为空、只有图片 ⇒ 回合预算分子为 0"
        );
    }

    /// T16t-3：超支完全由图片体积造成 ⇒ `plan_elisions` 返回空。
    ///
    /// 场景：本回合两条工具结果的**文本**恰好刚好用完预算（Get 图前不超），但还各带 25 张图。
    /// 若图片计入分子，闸门会认为超预算，而降 `ToolResult` 文本降不下来 ⇒ 给出「降不掉」
    /// 的计划，执行后仍“超” ⇒ 每回合空转重试。
    ///
    /// 变异自检：把 `_ => 0` 改成计入 `IMAGE_TOKEN_ESTIMATE` ⇒ 计划变为 `[1]`，本用例红。
    #[test]
    fn plan_elisions_empty_when_overage_is_caused_only_by_images() {
        let text = big_lines(200);
        let msgs = vec![
            plain_msg("hello"),
            tool_result_msg_with_images("c1", text.clone(), 25),
            tool_result_msg_with_images("c2", text.clone(), 25),
        ];
        // 预算按**剥掉图片**的同一批文本算 —— 不依赖被测函数的图片口径，
        // 否则变异会在前置处先红，掩盖真正要钉的那条断言。
        let text_only = [
            msgs[0].clone(),
            tool_result_msg("c1", text.clone()),
            tool_result_msg("c2", text),
        ];
        let budget = tool_result_tokens(&text_only[1..]);
        assert!(
            tool_result_tokens(&text_only[1..]) <= budget,
            "前置：文本部分本就在预算内（超支完全来自图片）"
        );
        assert_eq!(
            plan_elisions(&msgs, 1, budget),
            Vec::<usize>::new(),
            "图片降不掉 ⇒ 不得计划降级（否则每回合空转）"
        );
    }
}
