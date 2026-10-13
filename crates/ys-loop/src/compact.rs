//! 上下文压缩：**算法唯一宿主**。
//!
//! 本模块与 [`ys_component::RuntimeContext`] **完全解耦** —— `plan()` / `execute()` 只吃
//! `&[Message]`，不吃 session、不吃容器。这不是为未来预埋：`/compact`（app 侧
//! `capabilities::compact`）拿不到 `RuntimeContext`，只有算法与容器分家，两个入口
//! （Step 4b 自动触发 / 手动命令）才可能共用同一份实现（ADR-0017 决策 1）。
//!
//! 形状是两段式：
//!
//! ```text
//! plan(messages, policy, window) -> Option<CompactPlan>   // 纯函数、同步、无 IO，变异测试主靶
//! execute(messages, &plan, model, policy).await           // 唯一 await 点（一次 model 调用）
//! ```
//!
//! ⚠️ **承重耦合（ADR-0017 决策 1）**：`plan()` 是纯同步函数、**绝不碰 model**，且必须在
//! 调用方的 `model.complete` **之前**收场。`apps/coding-agent/src/app_loop.rs` 的既有测试
//! `every_request_variant_is_handled` 会走到 `basic.rs` 的 Step 4b，它安全**只因为**会话
//! 只有 1 条小消息 ⇒ 触发阈值不满足 / `plan()` 返回 `None` ⇒ `MockModel` 的响应队列不被
//! 消费（空队列 `remove(0)` 会 panic）。护栏是**叠加**的三道：基本 `trigger_at` 阈值判断、
//! `plan()` 对 `len < 2` 的 `None`、`execute()` 拒绝空区间 —— 实测把三道同时拿掉，
//! 该测试即 panic 在 `ys-model/src/mock.rs` 的 `remove(0)`（空队列）。
//! **任何让压缩路径可能无条件调用 model 的改动都会打破它。**
//!
//! ⚠️ **`execute()` 不持有 session**（评审 R3）：失败时它自己**什么都不改**，
//! `session.replace_all(outcome.messages)` 由调用方做 —— 这正是不静默丢历史（B-3）的结构保证。

use std::ops::Range;

use ys_component::CompactPolicy;
use ys_core::{ContentBlock, Message, Role};
use ys_model::{Model, ModelRequest};

use crate::summarize::NoopModelEventSink;

/// 摘要文本的包裹标记（pi / Codewhale 同形：一条 `Role::User` 角色的摘要 checkpoint）。
const SUMMARY_OPEN: &str = "[Context Summary]";
const SUMMARY_CLOSE: &str = "[/Context Summary]";

/// 切分计划。**纯数据**，可逐字段构造（测试与调用方都靠它对齐索引）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactPlan {
    /// 要被摘要掉的消息区间（`0..keep_from`）
    pub summarize: Range<usize>,
    /// 原样保留的消息区间（`keep_from..len`）
    pub keep: Range<usize>,
    /// 生成本计划时 `messages` 的长度 —— 供 `execute` 校验索引对齐（评审 R10）。
    ///
    /// ⚠️ 该校验是**显式的、release 下同样生效**（不是 `debug_assert`）：`summarize` /
    /// `keep` 还必须无缝覆盖 `0..len`（见 `execute` 的 P1-1 校验），否则返回 `Err`。
    pub len: usize,
}

/// 规划切分点：**纯函数、同步、无 IO**。
///
/// 从最新一条消息往回扫，累积到「保留预算」（`policy.keep_budget(window)`）为止；
/// 预算本身是窗口的 1%~10%（见 `CompactPolicy`）。
///
/// 返回 `None` = **不压缩**（消息太少，或结构上无法同时保证两段非空）。
///
/// # B-1 硬不变量：摘要段与保留段**都必须非空**
///
/// 旧实现只做尾扫，当**最新一条消息自己**就超过保留预算时 `break` 永不触发 ⇒
/// `keep_from == len` ⇒ 保留段为空 ⇒ **整个会话连同本轮用户消息被摘要掉，一条不留**
/// （实测复现过）。故这里强制 `1 ≤ keep_from ≤ len − 1`：
///
/// - `len < 2` → `None`（两条都凑不齐，谈不上切分）
/// - `keep_from` 上钳到 `len − 1` ⇒ 保留段至少含最新一条
/// - `keep_from` 下钳到 `1` ⇒ 摘要段至少含首条，**压缩必然产出真实进展**
///   （否则每轮空转、永不推进）
///
/// # P1-2 硬不变量：保留段**首条不得含 `ToolResult` 块**
///
/// 切点只按 token 预算尾扫，**不看消息语义** ⇒ 保留段可能以一条 `ToolResult` 消息开头，
/// 而它的对应 `ToolUse` 已被摘要掉。适配器会为它推 `{role:"tool", tool_call_id}`，该消息
/// **前无 `assistant.tool_calls`** ⇒ OpenAI 兼容端返回 400。而且会话已被调用方 `replace_all`
/// 成这个形状并落盘 ⇒ **之后每个回合都 400，会话被锁死到 `/new` 为止** —— 不是「压缩失败」，
/// 是「压缩成功后主调用硬失败」。
///
/// `BasicLoop` 的 Step 10 把工具结果打包成一条 `Role::User` 消息 append，所以会话里的
/// 倒数第二条**通常**正是 tool_result，切点很容易落在它身上，这不是罕见形状。
///
/// 处理：算出 `keep_from` 后**向后推进**，跳过所有「含 `ToolResult` 块的消息」。只前推、
/// 不回退，故上面的 `keep_from ≥ 1` 仍然成立；推进上界是 `len − 1`，故 `keep_from ≤ len − 1`
/// 也仍然成立。若推到 `len − 1` 时**末条自己**就是 tool_result ⇒ 没有合法切点，
/// **返回 `None`（不压缩）**：不压缩永远优于压出一个非法会话。
pub fn plan(messages: &[Message], policy: &CompactPolicy, window: usize) -> Option<CompactPlan> {
    let len = messages.len();
    if len < 2 {
        return None;
    }

    let keep_budget = policy.keep_budget(window);
    let mut keep_from = len;
    let mut kept_tokens = 0usize;
    for msg in messages.iter().rev() {
        let msg_tokens = crate::token::estimate_tokens(msg);
        // 严格大于：**恰好用满预算**的消息要留下（变异 `>` → `>=` 会被
        // `partition_boundary_is_exact` 抓到）。
        if kept_tokens + msg_tokens > keep_budget {
            break;
        }
        kept_tokens += msg_tokens;
        keep_from -= 1;
    }

    // B-1：两段都非空。`max(1)` 保证摘要段非空，`min(len - 1)` 保证保留段非空。
    // ⚠️ 顺序是承重的：先套 B-1 的钳制，再在其上做 P1-2 的**前推** —— 前推只增大
    // `keep_from`，故 `≥ 1` 不回退、`≤ len − 1` 由循环上界维持，两个不变量同时成立。
    let mut keep_from = keep_from.max(1).min(len - 1);

    // P1-2：保留段首条不得含 `ToolResult`（其 `ToolUse` 已被摘要掉 ⇒ 适配器推出孤立的
    // `role:"tool"` ⇒ provider 400 ⇒ 会话锁死）。前推跳过这类消息。
    while keep_from < len - 1 && contains_tool_result(&messages[keep_from]) {
        keep_from += 1;
    }
    // 推到 `len − 1` 仍非法（末条自己就是 tool_result）⇒ 无合法切点。
    // 返回 `None`：不压缩永远优于压出一个非法会话。
    if contains_tool_result(&messages[keep_from]) {
        return None;
    }

    Some(CompactPlan {
        summarize: 0..keep_from,
        keep: keep_from..len,
        len,
    })
}

/// 消息里是否含 `ToolResult` 块（P1-2 的判定：保留段首条若含它，其 `ToolUse` 必在摘要段）。
fn contains_tool_result(msg: &Message) -> bool {
    msg.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// 一次压缩的产物：**新消息表** + 供事件/命令汇报的估算数据。
#[non_exhaustive]
#[derive(Debug)]
pub struct CompactOutcome {
    /// 新消息表（摘要 + 保留段）—— 调用方拿它 `session.replace_all(...)`
    pub messages: Vec<Message>,
    /// 压缩前的**估算** token 数（启发式，非 provider 真实 usage）
    pub estimated_tokens_before: usize,
    /// 压缩后的**估算** token 数
    pub estimated_tokens_after: usize,
    /// 被摘要掉的消息条数
    pub summarized: usize,
    /// 原样保留的消息条数
    pub kept: usize,
}

/// 压缩失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    /// 摘要调用失败（模型报错，或返回里没有可用文本 —— B-2）
    #[error("summarization failed: {0}")]
    Summarize(String),
    /// 会话落盘失败（由调用方的 `replace_all` 归类）
    #[error("storage error: {0}")]
    Storage(String),
}

/// **自动触发与 `/compact` 共用的唯一实现**。
///
/// ⚠️ **不持有 session** —— 调用方拿 `outcome.messages` 自己写回（评审 R3）。
///
/// 失败语义（B-3）：**返回 `Err`，且绝不修改任何东西**。调用方必须保证失败时 session
/// 一个字节都不动 —— 旧实现在这里 `clear()` + 写一行
/// `[Compression failed — proceeding with truncated history]`，把几十万 token 的历史
/// 换成一行占位符，那是**静默失忆**而不是降级。
pub async fn execute(
    messages: &[Message],
    plan: &CompactPlan,
    model: &dyn Model,
    policy: &CompactPolicy,
) -> Result<CompactOutcome, CompactError> {
    // 评审 R10 / P1-1：`plan()` 与 `execute()` 各收一次 `messages` ⇒ 索引可能错配。
    //
    // ⚠️ 这里必须是**显式校验**，不能是 `debug_assert_eq!`：后者在 release 构建下被编译掉，
    // 而 `CompactPlan` 是 `pub`、doc 明确说它「纯数据、可逐字段构造」（`len` 字段注释还写着
    // 「供 `execute` 校验索引对齐」），下一片 app 侧 `/compact` 正是被鼓励「手搓 plan」的
    // 第二个调用方 —— 那个承诺只有在 debug 下才成立是**假承诺**。
    //
    // release 实测：`{summarize:0..1, keep:1..3, len:3}` + 5 条消息 ⇒ 旧实现返回 `Ok`，
    // 尾部 2 条被**静默丢弃**（调用方随后 `replace_all` 即真实丢历史，违反 B-3 的
    // 「压缩绝不能丢历史」）。
    let expected = messages.len();
    if plan.len != expected {
        return Err(CompactError::Summarize(format!(
            "compact plan is stale: plan.len = {} but messages.len = {expected}",
            plan.len
        )));
    }
    // 两道越界闸之外，区间还须**无缝且完整覆盖** `0..len`：
    //   `summarize.start == 0`、`summarize.end == keep.start`、`keep.end == len`。
    // 只查「越界 + 非空」拦不住「覆盖不全」（尾部消息被静默丢弃）与「区间有缝」
    // （中间的消息被静默丢弃）两种畸形 —— 二者同样违反 B-3。
    if plan.summarize.start != 0
        || plan.summarize.end != plan.keep.start
        || plan.keep.end != expected
    {
        return Err(CompactError::Summarize(format!(
            "compact plan must cover 0..{expected} without gaps: summarize {:?}, keep {:?}",
            plan.summarize, plan.keep
        )));
    }

    let to_summarize = messages.get(plan.summarize.clone()).ok_or_else(|| {
        CompactError::Summarize(format!(
            "compact plan out of range: summarize {:?} of {} messages",
            plan.summarize,
            messages.len()
        ))
    })?;
    let to_keep = messages.get(plan.keep.clone()).ok_or_else(|| {
        CompactError::Summarize(format!(
            "compact plan out of range: keep {:?} of {} messages",
            plan.keep,
            messages.len()
        ))
    })?;
    // B-1 的第二道闸：即便调用方手搓了一个畸形 plan，也绝不产出「空保留段」。
    if to_summarize.is_empty() || to_keep.is_empty() {
        return Err(CompactError::Summarize(
            "compact plan must leave both a summarize range and a keep range".into(),
        ));
    }

    let estimated_tokens_before = crate::token::estimate_session_tokens(messages);

    // B-3：**先摘要，成功之后才构造新消息表** —— 任何失败都在 `?` 处返回，
    // 此时本函数尚未产出任何可变状态，调用方的 session 自然一个字节未动。
    let summary = generate_summary(model, to_summarize, &policy.summary_system_prompt).await?;
    let summary_text = format!("{SUMMARY_OPEN}\n{summary}\n{SUMMARY_CLOSE}");

    // R7：删掉伪造的 assistant ack（`"I have the context summary. Continuing."`）。
    // 它是**伪造的模型输出**：会被写进 JSONL、重启后当真实发言重放、`/export` 带出去、
    // 下次压缩还当真实历史再摘要一遍。而它想解决的「user/assistant 交替」由**构造**保证：
    //
    //   kept[0] 是 User      → 摘要**并入**它（前缀）  ⇒ `User(含摘要), …kept[1..]` 交替
    //   否则（Assistant 等）  → 摘要独立成一条 User    ⇒ `User(摘要), Assistant, …` 交替
    //
    // ⚠️ 摘要消息的角色仍是 `Role::User`（ADR-0017 决策 12：改角色需要新决策）。
    let mut out: Vec<Message> = Vec::with_capacity(to_keep.len() + 1);
    if to_keep[0].role == Role::User {
        out.push(merge_summary_into(&to_keep[0], &summary_text));
        out.extend(to_keep[1..].iter().cloned());
    } else {
        out.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: summary_text }],
        });
        out.extend(to_keep.iter().cloned());
    }

    let estimated_tokens_after = crate::token::estimate_session_tokens(&out);
    Ok(CompactOutcome {
        messages: out,
        estimated_tokens_before,
        estimated_tokens_after,
        summarized: to_summarize.len(),
        kept: to_keep.len(),
    })
}

/// 把摘要**并入**一条消息（作为文本前缀）。
///
/// 首块是 `Text` 时与它合并（`\n\n` 连接，保持「摘要是一段前言」的直觉）；
/// 首块不是 `Text`（例如工具结果消息）时在最前面插一个 `Text` 块 —— 两条路径都
/// **逐字保留原有 `content`**，不裁剪、不重写。
fn merge_summary_into(msg: &Message, summary_text: &str) -> Message {
    let mut content = msg.content.clone();
    match content.first_mut() {
        Some(ContentBlock::Text { text }) => {
            let merged = format!("{summary_text}\n\n{text}");
            *text = merged;
        }
        _ => content.insert(
            0,
            ContentBlock::Text {
                text: summary_text.to_string(),
            },
        ),
    }
    Message {
        role: msg.role.clone(),
        content,
    }
}

/// 用模型生成旧消息的摘要。
///
/// ⚠️ **请求形状刻意保持「独立 `ModelRequest` + `policy.summary_system_prompt`」**：
/// 「复用活对话前缀 + `tool_choice`」是下一片（ADR-0017 决策 10 / 片 2）的事，本片不做。
async fn generate_summary(
    model: &dyn Model,
    messages: &[Message],
    system_prompt: &str,
) -> Result<String, CompactError> {
    // sink 与 `summarize::ModelSummarizer` 共用同一份（R9：同 crate 内不该有两份相同 impl）。
    let mut sink = NoopModelEventSink;
    let request = ModelRequest {
        messages: messages.to_vec(),
        tools: vec![],
        system: Some(system_prompt.to_string()),
        // `max_tokens` **刻意不设**：摘要只受 system prompt 的长度约束，不额外掐上限 ——
        // 交由 provider 自身的模型上限。**不设**比设一个偏小的值更稳：摘要若被截断会丢失
        // 被压缩的历史，反而违背压缩的目的。
        ..Default::default()
    };
    let response = model
        .complete(request, &mut sink)
        .await
        .map_err(|e| CompactError::Summarize(e.to_string()))?;

    // 拼接**全部** `Text` block —— 有的 provider 会把一次回答拆成多块。
    let mut text = String::new();
    for block in &response.message.content {
        if let ContentBlock::Text { text: t } = block {
            text.push_str(t);
        }
    }

    // B-2：空（或全空白）响应是**失败**，不是「成功的空摘要」。旧实现这里 `Ok(String::new())`，
    // 于是模型回 tool call（响应无 `Text` 块）时写出空摘要 —— 历史被静默丢掉。
    if text.trim().is_empty() {
        return Err(CompactError::Summarize("the model returned no text".into()));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::Usage;
    use ys_model::{FinishReason, MockModel, ModelError, ModelEventSink, ModelResponse};

    /// 400 字节 ASCII ≈ 100 token（估算器：字节 / 4）。
    /// 用「字符数 = token 数 × 4」把预算算成整数，切分边界才可精确断言。
    fn text_msg(role: Role, tokens: usize) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: "a".repeat(tokens * 4),
            }],
        }
    }

    /// 交替角色的会话（偶数 index = User，奇数 = Assistant）。
    fn conversation(n: usize, tokens: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                text_msg(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    tokens,
                )
            })
            .collect()
    }

    /// 带 `ToolUse` 块的 Assistant 消息（`arguments` 撑到 `tokens` token）。
    fn tool_use_msg(tokens: usize) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: ys_core::ToolCallId("tc-1".into()),
                name: "bash".into(),
                arguments: serde_json::Value::String("a".repeat(tokens * 4)),
            }],
        }
    }

    /// 带 `ToolResult` 块的 User 消息（`BasicLoop` Step 10 就是这么打包工具结果的）。
    fn tool_result_msg(tokens: usize) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ys_core::ToolCallId("tc-1".into()),
                content: "a".repeat(tokens * 4),
                is_error: false,
            }],
        }
    }

    fn text_of(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 重建结果里不得出现**连续同角色**（user/assistant 交替是 provider 的硬要求）。
    fn assert_alternates(msgs: &[Message]) {
        for w in msgs.windows(2) {
            assert_ne!(
                w[0].role, w[1].role,
                "出现连续同角色消息，交替被破坏：{msgs:?}"
            );
        }
    }

    /// 重建结果里的每一条 Assistant 消息都必须是**模型真实产出**（即在输入里逐字存在）。
    /// 这是「零伪造数据」的直接断言 —— 加回合成 ack 立刻变红。
    fn assert_no_synthetic_assistant(out: &[Message], input: &[Message]) {
        for m in out.iter().filter(|m| m.role == Role::Assistant) {
            assert!(
                input.contains(m),
                "重建结果里出现非模型产出的 Assistant 消息（伪造）：{m:?}"
            );
        }
    }

    fn summary_count(msgs: &[Message]) -> usize {
        msgs.iter()
            .filter(|m| text_of(m).contains(SUMMARY_OPEN))
            .count()
    }

    // ==================== plan()：早退与切分 ====================

    /// 单条消息（= `app_loop::every_request_variant_is_handled` 的会话形状）⇒ `None`，
    /// 且据此**不调用 model**（用「一被调用就 panic」的替身把这条耦合钉成可执行断言）。
    #[tokio::test]
    async fn plan_early_exit_means_model_never_called() {
        struct NeverCalledModel;

        #[async_trait::async_trait]
        impl Model for NeverCalledModel {
            fn model_id(&self) -> &str {
                "never-called"
            }
            async fn complete(
                &self,
                _request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                panic!("plan() 早退时不得调用 model（MockModel 空队列会 panic）");
            }
        }

        let policy = CompactPolicy::default();
        let messages = vec![text_msg(Role::User, 1)];
        assert!(
            plan(&messages, &policy, 128_000).is_none(),
            "单条消息无从切分，必须返回 None"
        );

        // 照抄 `basic.rs` Step 4b 的调用形状：只有 plan 给出计划才轮到 model。
        // 若早退被挪走（例如无条件 execute），上面的 panic 会把本测试打红。
        let model = NeverCalledModel;
        if let Some(p) = plan(&messages, &policy, 128_000) {
            let _ = execute(&messages, &p, &model, &policy).await;
        }
    }

    /// B-1（一）：**末条自己就超预算** ⇒ `keep_from == len − 1`，保留段含末条。
    ///
    /// 变异自检：删掉 `plan()` 里的 `.min(len - 1)` ⇒ `keep_from == len` ⇒ 保留段为空 ⇒ 红。
    #[test]
    fn b1_oversized_last_message_is_kept() {
        let policy = CompactPolicy::default();
        let messages = vec![
            text_msg(Role::User, 10),
            text_msg(Role::Assistant, 10),
            text_msg(Role::User, 100_000), // 单条就远超 keep_budget(128_000) = 12_800
        ];

        let p = plan(&messages, &policy, 128_000).expect("应产出计划（两段都非空）");

        assert_eq!(p.keep.start, messages.len() - 1, "上钳到 len − 1");
        assert_eq!(p.keep.end, messages.len());
        assert!(!p.keep.is_empty(), "保留段不得为空");
        assert!(
            text_of(&messages[p.keep.start]).starts_with("aaaa"),
            "保留段必须含最新那条（本轮用户消息）"
        );
        assert_eq!(p.summarize, 0..messages.len() - 1);
    }

    /// B-1（二）：**首条巨大** ⇒ `keep_from == 1`，且**产出真实进展**（摘要段非空）。
    ///
    /// 旧实现在 `keep_from <= 1` 时静默 `Ok(())` —— 那条巨大的首消息永远压在头上，
    /// 压缩每轮空转。
    #[test]
    fn b1_oversized_first_message_still_makes_progress() {
        let policy = CompactPolicy::default();
        let messages = vec![
            text_msg(Role::User, 100_000),
            text_msg(Role::Assistant, 10),
            text_msg(Role::User, 10),
        ];

        let p = plan(&messages, &policy, 128_000).expect("应产出计划");

        assert_eq!(p.keep.start, 1, "下钳到 1 ⇒ 首条（巨大者）进摘要段");
        assert_eq!(p.summarize, 0..1);
        assert!(!p.summarize.is_empty(), "摘要段不得为空（否则零进展）");
        assert!(!p.keep.is_empty(), "保留段不得为空");
    }

    /// 切分边界精确：**恰好用满预算**的消息必须被保留。
    ///
    /// window = 2000 ⇒ keep_budget = 200；三条各 100 token、总计 300。
    /// 尾扫：末条 100 ≤ 200；倒数第二条 100 + 100 = 200 **不大于** 200 ⇒ 留下。
    /// 变异自检：累积判断的 `>` 改成 `>=` ⇒ `keep_from` 变成 2 ⇒ 红。
    #[test]
    fn partition_boundary_is_exact() {
        let policy = CompactPolicy::default();
        let messages = conversation(3, 100);

        let p = plan(&messages, &policy, 2000).expect("应产出计划");

        assert_eq!(p.keep, 1..3, "恰好用满预算的两条都要留下");
        assert_eq!(p.summarize, 0..1);
    }

    /// `len < 2` 一律 `None`（B-1 的前置闸）。
    #[test]
    fn plan_returns_none_below_two_messages() {
        let policy = CompactPolicy::default();
        assert!(plan(&[], &policy, 128_000).is_none());
        assert!(plan(&[text_msg(Role::User, 1)], &policy, 128_000).is_none());
    }

    // ==================== P1-2：保留段首条不得含 ToolResult ====================

    /// P1-2 复现（评审员构造）：`[U, A(tool_use tc-1), U(tool_result tc-1), A(text), U(text)]`。
    ///
    /// window = 2000 ⇒ keep_budget = 200。尾扫恰好留 3 条 ⇒ `keep_from = 2`，
    /// 而 `messages[2]` 正是含 `ToolResult` 的 User 消息（它的 `ToolUse` 在摘要段里）。
    ///
    /// 不修的话：适配器会为它推 `{role:"tool", tool_call_id:"tc-1"}`，前无
    /// `assistant.tool_calls` ⇒ provider 400 ⇒ 会话被锁死到 `/new`。
    ///
    /// 变异自检：删掉 `plan()` 里那段 `while … contains_tool_result { keep_from += 1 }`
    /// ⇒ `keep.start` 变回 2 ⇒ 本测试变红。
    #[test]
    fn p1_2_keep_head_never_starts_with_tool_result() {
        let policy = CompactPolicy::default();
        let messages = vec![
            text_msg(Role::User, 10),
            tool_use_msg(100),   // 单条超预算 ⇒ 尾扫在此 break
            tool_result_msg(50), // ⇐ 尾扫给出的切点落在这里（非法）
            text_msg(Role::Assistant, 50),
            text_msg(Role::User, 50),
        ];

        let p = plan(&messages, &policy, 2000).expect("跳过之后有合法切点");

        assert_eq!(p.keep, 3..5, "切点必须推过 tool_result 消息");
        assert_eq!(p.summarize, 0..3);
        assert!(
            !contains_tool_result(&messages[p.keep.start]),
            "保留段首条不得含 ToolResult（其 ToolUse 已被摘要掉）"
        );
        // B-1 的两条不变量仍成立
        assert!(p.keep.start >= 1, "摘要段非空");
        assert!(p.keep.start < messages.len(), "保留段非空");
    }

    /// P1-2 边界：推进到 `len − 1` 时**末条自己**就是 tool_result ⇒ 无合法切点 ⇒ `None`。
    ///
    /// 不压缩永远优于压出一个非法会话（设计不变量）。
    #[test]
    fn p1_2_returns_none_when_no_legal_cut_point_exists() {
        let policy = CompactPolicy::default();
        let messages = vec![
            text_msg(Role::User, 10),
            tool_use_msg(1000),  // 超预算 ⇒ 尾扫只能留下末条
            tool_result_msg(10), // 末条自己就是 tool_result（无合法切点）
        ];

        assert!(
            plan(&messages, &policy, 2000).is_none(),
            "唯一的保留段候选以 ToolResult 开头 ⇒ 必须返回 None（不压缩）"
        );
    }

    /// P1-2 不误伤：**不含** `ToolResult` 的普通会话切点不动（尾扫结果原样保留）。
    #[test]
    fn p1_2_does_not_move_boundary_without_tool_results() {
        let policy = CompactPolicy::default();
        let messages = conversation(5, 100);
        let p = plan(&messages, &policy, 2000).expect("应产出计划");
        assert_eq!(p.keep, 3..5, "无 ToolResult 时切点与尾扫一致");
    }

    /// `execute()` 拒绝畸形 plan（手搓的越界 / 空区间）—— 第二道 B-1 闸。
    #[tokio::test]
    async fn execute_rejects_degenerate_plans() {
        let policy = CompactPolicy::default();
        let messages = conversation(2, 10);
        let model = MockModel::new("m");
        model.push_text("摘要");

        // 保留段为空
        let empty_keep = CompactPlan {
            summarize: 0..2,
            keep: 2..2,
            len: 2,
        };
        assert!(matches!(
            execute(&messages, &empty_keep, &model, &policy).await,
            Err(CompactError::Summarize(_))
        ));

        // 区间越界（`len` 对齐但区间超出 —— `debug_assert` 之外的第二道闸）
        let out_of_range = CompactPlan {
            summarize: 0..5,
            keep: 1..2,
            len: 2,
        };
        assert!(matches!(
            execute(&messages, &out_of_range, &model, &policy).await,
            Err(CompactError::Summarize(_))
        ));
    }

    // ==================== P1-1：plan/messages 对齐必须是显式校验 ====================

    /// P1-1（一）：`plan.len != messages.len()` ⇒ `Err`，**release 下也生效**。
    ///
    /// 这是评审的实测形状：`{summarize:0..1, keep:1..3, len:3}` + 5 条消息。旧实现用
    /// `debug_assert_eq!`，release 下被编译掉 ⇒ 返回 `Ok` 且 `out.messages.len() == 3`，
    /// **尾部 2 条被静默丢弃**（调用方 `replace_all` 即真实丢历史）。
    ///
    /// 变异自检：把 `execute` 的显式校验收回 `debug_assert_eq!` ⇒ 在
    /// `cargo test --release -p ys-loop` 下本测试变红（debug 下仍绿 —— 这正是漏网的机理）。
    #[tokio::test]
    async fn p1_1_execute_rejects_stale_plan_len() {
        let policy = CompactPolicy::default();
        let messages = conversation(5, 10);
        let model = MockModel::new("m");
        model.push_text("摘要");

        let stale = CompactPlan {
            summarize: 0..1,
            keep: 1..3,
            len: 3, // 与 messages.len() = 5 不符 ⇒ 尾部 2 条会被静默丢弃
        };

        let err = execute(&messages, &stale, &model, &policy)
            .await
            .expect_err("plan.len 错配必须返回 Err（release 下同样生效）");
        let reason = err.to_string();
        assert!(
            reason.contains("3") && reason.contains('5'),
            "错误须可读：{reason}"
        );
    }

    /// P1-1（二）：区间**有缝**或**覆盖不全** ⇒ `Err`（两种都会静默丢中间/尾部消息）。
    #[tokio::test]
    async fn p1_1_execute_rejects_gap_and_partial_coverage() {
        let policy = CompactPolicy::default();
        let messages = conversation(5, 10);
        let model = MockModel::new("m");
        model.push_text("摘要");

        // 有缝：summarize.end(1) != keep.start(2) ⇒ messages[1] 被静默丢弃
        let gap = CompactPlan {
            summarize: 0..1,
            keep: 2..5,
            len: 5,
        };
        // 覆盖不全（尾部）：keep.end(4) != len(5) ⇒ messages[4] 被静默丢弃
        let partial = CompactPlan {
            summarize: 0..1,
            keep: 1..4,
            len: 5,
        };
        // 覆盖不全（头部）：summarize.start(1) != 0 ⇒ messages[0] 被静默丢弃
        let not_from_zero = CompactPlan {
            summarize: 1..2,
            keep: 2..5,
            len: 5,
        };

        for (name, p) in [
            ("gap", gap),
            ("partial", partial),
            ("not_from_zero", not_from_zero),
        ] {
            assert!(
                matches!(
                    execute(&messages, &p, &model, &policy).await,
                    Err(CompactError::Summarize(_))
                ),
                "{name} 畸形 plan 必须被拒绝：{p:?}"
            );
        }
    }

    /// P1-1 不误伤：**合法**的手搓 plan（无缝覆盖 `0..len`）照常执行。
    /// 护栏的职责是挡住畸形，不是把所有手搓 plan 都拒了。
    #[tokio::test]
    async fn p1_1_execute_still_accepts_a_well_formed_hand_built_plan() {
        let policy = CompactPolicy::default();
        let messages = conversation(5, 10);
        let model = MockModel::new("m");
        model.push_text("摘要");

        let well_formed = CompactPlan {
            summarize: 0..2,
            keep: 2..5,
            len: 5,
        };
        let out = execute(&messages, &well_formed, &model, &policy)
            .await
            .expect("无缝覆盖 0..len 的 plan 必须被接受");
        assert_eq!(out.summarized, 2);
        assert_eq!(out.kept, 3);
    }

    // ==================== execute()：快乐路径 ====================

    /// 快乐路径：`after < before`、摘要恰好 1 条、保留段与输入切片**逐字相同**。
    ///
    /// window = 3000 ⇒ keep_budget = 300；四条各 100 token ⇒ 尾扫恰好留 3 条，
    /// `kept[0]` 是 Assistant ⇒ 摘要独立成条，保留段不受任何改写。
    #[tokio::test]
    async fn happy_path_summarizes_head_and_keeps_tail_verbatim() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100);
        let p = plan(&messages, &policy, 3000).expect("应产出计划");
        assert_eq!(p.summarize, 0..1);
        assert_eq!(p.keep, 1..4);

        let model = MockModel::new("m");
        model.push_text("简短摘要");

        let out = execute(&messages, &p, &model, &policy).await.unwrap();

        assert_eq!(out.summarized, 1);
        assert_eq!(out.kept, 3);
        assert!(
            out.estimated_tokens_after < out.estimated_tokens_before,
            "压缩后必须更小：{} → {}",
            out.estimated_tokens_before,
            out.estimated_tokens_after
        );
        assert_eq!(summary_count(&out.messages), 1, "摘要恰好 1 条");
        assert_eq!(out.messages.len(), 4, "3 条保留 + 1 条摘要");
        // 保留段逐字相同（截取口径：kept[0] 是 Assistant ⇒ 摘要独立成条，不改写保留段）
        for (i, m) in out.messages[1..].iter().enumerate() {
            assert_eq!(m, &messages[1 + i], "保留段必须逐字等于输入的对应切片");
        }
        assert!(text_of(&out.messages[0]).contains("简短摘要"));
    }

    // ==================== B-2：空摘要 ====================

    /// 模型回 tool call（响应里没有 `Text` 块）⇒ `Err`，不得是 `Ok("")`。
    ///
    /// 变异自检：把 `generate_summary` 的判空还原成 `Ok(String::new())` ⇒ 红。
    #[tokio::test]
    async fn b2_tool_call_only_response_is_err() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100);
        let p = plan(&messages, &policy, 3000).unwrap();

        let model = MockModel::new("m");
        model.push_tool_call("bash", serde_json::json!({"command": "ls"}));

        let err = execute(&messages, &p, &model, &policy)
            .await
            .expect_err("无文本的响应必须是 Err，否则会写出空摘要静默丢历史");
        assert!(matches!(err, CompactError::Summarize(_)), "实际 = {err:?}");
    }

    /// 全空白文本同样算失败（trim 后为空）。
    #[tokio::test]
    async fn b2_whitespace_only_response_is_err() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100);
        let p = plan(&messages, &policy, 3000).unwrap();

        let model = MockModel::new("m");
        model.push_text("  \n\t  ");

        assert!(matches!(
            execute(&messages, &p, &model, &policy).await,
            Err(CompactError::Summarize(_))
        ));
    }

    // ==================== B-3：失败即全无副作用 ====================

    /// 模型报错 ⇒ `Err`，且**什么都被没动**：输入消息表逐字未变，
    /// 错误响应之后的响应也**没被多消费**（execute 在第一个失败处就停了）。
    ///
    /// 变异自检：改回「先 clear 再写占位符」⇒ 失败不再是纯函数式的 ⇒ 红。
    #[tokio::test]
    async fn b3_model_error_leaves_everything_untouched() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100);
        let before = messages.clone();
        let p = plan(&messages, &policy, 3000).unwrap();

        let model = MockModel::new("m");
        model.push_error("provider 挂了");
        model.push_text("later");

        let err = execute(&messages, &p, &model, &policy)
            .await
            .expect_err("模型报错必须冒泡成 Err");
        match err {
            CompactError::Summarize(reason) => {
                assert!(reason.contains("provider 挂了"), "原因应可读：{reason:?}")
            }
            other => panic!("expected Summarize, got {other:?}"),
        }

        assert_eq!(messages, before, "失败时输入消息表必须逐字未变");

        // execute 只消费了那一条错误响应 —— 队列里剩下的响应仍在。
        let mut sink = NoopModelEventSink;
        let resp = model
            .complete(ModelRequest::default(), &mut sink)
            .await
            .expect("失败后队列里应还剩下一条响应（execute 不得多消费）");
        assert_eq!(text_of(&resp.message), "later");
    }

    // ==================== R7：交替由构造保证 ====================

    /// R7（kept[0] 是 User）：摘要**并入**它，不新增任何消息。
    #[tokio::test]
    async fn r7_summary_merges_into_user_kept_head() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100); // [U, A, U, A]
        let p = CompactPlan {
            summarize: 0..2,
            keep: 2..4, // kept[0] = messages[2] = User
            len: 4,
        };

        let model = MockModel::new("m");
        model.push_text("摘要正文");

        let out = execute(&messages, &p, &model, &policy).await.unwrap();

        assert_eq!(out.messages.len(), 2, "并入而非单插一条");
        assert_eq!(out.messages[0].role, Role::User);
        let head = text_of(&out.messages[0]);
        assert!(head.contains(SUMMARY_OPEN), "摘要应在首条里：{head:?}");
        assert!(head.contains("摘要正文"));
        assert!(
            head.contains(&"a".repeat(400)),
            "原消息文本必须逐字保留（摘要只是前缀）"
        );
        // 首条仍等于原消息（角色不变），且其后的保留段逐字相同。
        assert_eq!(out.messages[0].role, messages[2].role);
        assert_eq!(out.messages[1], messages[3]);

        assert_alternates(&out.messages);
        assert_no_synthetic_assistant(&out.messages, &messages);
    }

    /// R7（kept[0] 是 Assistant）：摘要作为**独立 User 消息**插在最前，交替自然成立。
    #[tokio::test]
    async fn r7_summary_is_separate_when_kept_head_is_assistant() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100); // [U, A, U, A]
        let p = CompactPlan {
            summarize: 0..1,
            keep: 1..4, // kept[0] = messages[1] = Assistant
            len: 4,
        };

        let model = MockModel::new("m");
        model.push_text("摘要正文");

        let out = execute(&messages, &p, &model, &policy).await.unwrap();

        assert_eq!(out.messages.len(), 4, "3 条保留 + 1 条独立摘要");
        assert_eq!(out.messages[0].role, Role::User);
        assert!(text_of(&out.messages[0]).contains(SUMMARY_OPEN));
        assert!(text_of(&out.messages[0]).contains("摘要正文"));
        // 保留段逐字未变
        for (i, m) in out.messages[1..].iter().enumerate() {
            assert_eq!(m, &messages[1 + i]);
        }

        assert_alternates(&out.messages);
        // 变异自检：加回合成 ack（Assistant）⇒ 这里变红（且交替也被破坏）
        assert_no_synthetic_assistant(&out.messages, &messages);
    }

    /// 摘要消息的角色必须是 `Role::User`（ADR-0017 决策 12：角色不变）。
    #[tokio::test]
    async fn summary_message_keeps_user_role() {
        let policy = CompactPolicy::default();
        let messages = conversation(4, 100);
        let p = plan(&messages, &policy, 3000).unwrap();
        let model = MockModel::new("m");
        model.push_text("摘要");

        let out = execute(&messages, &p, &model, &policy).await.unwrap();
        assert_eq!(out.messages[0].role, Role::User);
        assert!(
            !out.messages.iter().any(|m| m.role == Role::System),
            "不得把摘要改成 System"
        );
    }

    /// 首块不是 `Text`（工具结果消息）时，摘要以**新 `Text` 块**前置，原有块逐字保留。
    #[tokio::test]
    async fn merge_prepends_block_when_head_has_no_text() {
        let tool_result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ys_core::ToolCallId("tc-1".into()),
                content: "命令输出".into(),
                is_error: false,
            }],
        };
        let merged =
            merge_summary_into(&tool_result, "[Context Summary]\n摘要\n[/Context Summary]");

        assert_eq!(merged.role, Role::User);
        assert_eq!(merged.content.len(), 2, "原块保留 + 摘要块前置");
        assert!(matches!(&merged.content[0], ContentBlock::Text { text } if text.contains("摘要")));
        assert_eq!(&merged.content[1], &tool_result.content[0], "原块逐字保留");
    }

    /// 摘要请求的形状：不带工具表、带 `policy.summary_system_prompt`、**不设 `max_tokens`**。
    ///
    /// ⚠️ 本片**有意不做**「复用活对话前缀 + `tool_choice`」（那片 2 的事）——
    /// 这条测试正是为了钉住「本片没越界」，同时钉住 `max_tokens` **不设**
    /// （摘要交由 provider 的上限，避免被截断而丢失被压缩的历史）。
    #[tokio::test]
    async fn summary_request_shape_is_unchanged_in_this_slice() {
        use std::sync::Mutex;

        struct RecordingModel {
            last: Mutex<Option<ModelRequest>>,
        }

        #[async_trait::async_trait]
        impl Model for RecordingModel {
            fn model_id(&self) -> &str {
                "recording"
            }
            async fn complete(
                &self,
                request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "摘要".into(),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                })
            }
        }

        let policy = CompactPolicy::default();
        let messages = conversation(4, 100);
        let p = plan(&messages, &policy, 3000).unwrap();
        let model = RecordingModel {
            last: Mutex::new(None),
        };

        execute(&messages, &p, &model, &policy).await.unwrap();

        let req = model
            .last
            .lock()
            .unwrap()
            .take()
            .expect("应已调用一次 model");
        assert!(req.tools.is_empty(), "本片摘要调用仍不带工具表");
        assert_eq!(
            req.system.as_deref(),
            Some(policy.summary_system_prompt.as_str()),
            "摘要调用仍用 policy 的 system prompt"
        );
        assert_eq!(
            req.max_tokens, None,
            "max_tokens 刻意不设（摘要交由 provider 上限，不被截断）"
        );
        assert_eq!(req.temperature, None, "temperature 刻意不设");
    }
}
