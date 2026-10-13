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
}
