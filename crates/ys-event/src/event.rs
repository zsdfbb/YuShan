use serde::{Deserialize, Serialize};
use ys_core::{Message, StopReason, ToolCall, ToolCallId, ToolResult, Usage};

/// 一次模型调用的用途，用于区分 `RoundUsage` 的来源。
///
/// `#[non_exhaustive]`：将来新增 `Summarize` 等变体属**加法**，不会破坏下游的穷举匹配；
/// P0 只发 `Turn` / `Compaction`。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundPurpose {
    /// 主循环的模型轮。
    Turn,
    /// 自动压缩的摘要模型调用。
    Compaction,
}

/// agent run 期间发出的 runtime 级事件。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    UserMessage {
        message: Message,
    },
    /// 一轮开始：每轮**模型调用之前**发出一次。
    ///
    /// **非终局**：终局事件仍只有 `RunFinished` / `RunFailed`，本事件不影响
    /// `-p` / `--json` 消费循环的退出条件。
    ///
    /// `round` 从 `1` 起，与该回合 `RunFinished.rounds` / `RunResult.rounds` **同口径**。
    /// 存在的理由是**回合进行中的可见性**：一个回合可能跑几十轮、每轮数秒，此前消费者
    /// 只能等终局事件才知道「跑到第几轮、还要多久」，UI 因此只能显示一个不动的进度点。
    RoundStarted {
        round: u32,
    },
    /// 一次模型调用消耗的 usage（逐轮上报）。
    ///
    /// **非终局**：终局事件仍只有 `RunFinished` / `RunFailed`，本事件不影响
    /// `-p` / `--json` 消费循环的退出条件（消费循环以终局事件为终止条件）。
    ///
    /// `round` 与 `RoundStarted.round` / `RunFinished.rounds` **同口径**（从 `1` 起，
    /// 计的是该回合内的模型轮）。`purpose` 为 `Compaction` 时，`round` 记**它正在准备的
    /// 下一轮** —— 压缩发生在该轮的 `RoundStarted` 之前，若记「已完成的轮数」会与随后
    /// 发出的 `RoundStarted.round` 差一。
    RoundUsage {
        purpose: RoundPurpose,
        round: u32,
        usage: Usage,
    },
    ModelTextDelta {
        text: String,
    },
    /// 模型思考内容增量（`reasoning_content`）。由 `Forwarder` 从
    /// `ModelEvent::ThinkingDelta` 映射而来。
    ModelThinkingDelta {
        text: String,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        id: ToolCallId,
        result: ToolResult,
    },
    RunFinished {
        stop_reason: StopReason,
        usage: Usage,
        rounds: u32,
    },
    RunFailed {
        error: String,
    },
    /// 一次自动压缩完成。
    ///
    /// **只发终局、不发「开始」** —— `Started` 会引入悬挂状态：压缩中途被
    /// `Abort` 时终止事件永不发出，transcript 会永远挂着「Compacting…」。
    ///
    /// 字段名里的 `estimated_` 前缀是**有意的**：这两个数来自启发式估算器
    /// （`ys-loop` 的 `token.rs`：ASCII 字节 / 4 + CJK 每字 1.5），
    /// 不是 provider 回报的真实 `usage`，命名不得简写成 `tokens_before/after`。
    ///
    /// 非终局事件：不影响 `-p` / `--json` 的消费循环退出条件。
    ContextCompacted {
        /// 压缩前的**估算** token 数（启发式，非 provider 真实 usage）
        estimated_tokens_before: usize,
        /// 压缩后的**估算** token 数
        estimated_tokens_after: usize,
        /// 被摘要掉的消息条数
        summarized: usize,
        /// 原样保留的消息条数
        kept: usize,
    },
    /// 一次压缩失败（session 未被修改）。
    ContextCompactionFailed {
        reason: String,
    },
    /// 模型输出被截断（provider finish_reason = length/max_tokens）。
    ///
    /// **非终局**：终局事件仍只有 `RunFinished` / `RunFailed`。
    /// 有工具调用时该批**全部不执行**，循环继续；无工具调用时回合以
    /// `StopReason::Truncated` 收场。
    ResponseTruncated {
        /// 被作废、**未执行**的工具调用数（0 = 纯文本被截断）。
        dropped_tool_calls: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(reasoning_tokens: Option<u32>) -> Usage {
        Usage {
            input_tokens: 10,
            output_tokens: 20,
            reasoning_tokens,
            ..Default::default()
        }
    }

    // 逐轮 usage 事件：加法式新增，序列化 / 反序列化 roundtrip 必须稳定
    // （`--json` 模式逐行打印 `Envelope`，字段名即对外契约）。
    #[test]
    fn test_agent_event_round_usage_serde_roundtrip() {
        for purpose in [RoundPurpose::Turn, RoundPurpose::Compaction] {
            let event = AgentEvent::RoundUsage {
                purpose: purpose.clone(),
                round: 5,
                usage: usage(Some(4)),
            };
            let json = serde_json::to_string(&event).unwrap();
            let back: AgentEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back, "roundtrip 必须稳定：{json}");
        }
    }

    // `purpose` 的对外形态是 snake_case（`"turn"` / `"compaction"`）。
    //
    // 变异说明：去掉 `#[serde(rename_all = "snake_case")]` ⇒ serde 回落到变体原名，
    // 输出 `"Turn"` / `"Compaction"` ⇒ 两条断言变红。
    // 注意：把它改成 `"camelCase"` **不会**变红 —— 本枚举两个变体都是单词，
    // camelCase 与 snake_case 的输出同为小写；钉住该契约的变异是「去掉 rename_all」。
    #[test]
    fn test_round_usage_purpose_serializes_as_snake_case() {
        let turn = serde_json::to_value(AgentEvent::RoundUsage {
            purpose: RoundPurpose::Turn,
            round: 1,
            usage: usage(None),
        })
        .unwrap();
        assert_eq!(
            turn.pointer("/RoundUsage/purpose"),
            Some(&serde_json::json!("turn")),
            "purpose 是对外契约，必须是 \"turn\"：{turn}"
        );

        let compaction = serde_json::to_value(AgentEvent::RoundUsage {
            purpose: RoundPurpose::Compaction,
            round: 2,
            usage: usage(None),
        })
        .unwrap();
        assert_eq!(
            compaction.pointer("/RoundUsage/purpose"),
            Some(&serde_json::json!("compaction")),
            "purpose 是对外契约，必须是 \"compaction\"：{compaction}"
        );
    }

    // `reasoning_tokens` 的「未上报」与「真 0」在 JSON 里必须可区分：
    // `Some(4)` 写出该键，`None` 不写（依赖 `Usage` 的 `skip_serializing_if`）。
    //
    // 变异说明：去掉 `Usage::reasoning_tokens` 的
    // `skip_serializing_if = "Option::is_none"` ⇒ 第二条断言变红（`None` 也会写出键）。
    #[test]
    fn test_round_usage_reasoning_tokens_presence_in_json() {
        let with = serde_json::to_value(AgentEvent::RoundUsage {
            purpose: RoundPurpose::Turn,
            round: 1,
            usage: usage(Some(4)),
        })
        .unwrap();
        assert_eq!(
            with.pointer("/RoundUsage/usage/reasoning_tokens"),
            Some(&serde_json::json!(4)),
            "上报了 4 个推理 token ⇒ 必须含该键：{with}"
        );

        let without = serde_json::to_value(AgentEvent::RoundUsage {
            purpose: RoundPurpose::Turn,
            round: 1,
            usage: usage(None),
        })
        .unwrap();
        assert!(
            without
                .pointer("/RoundUsage/usage/reasoning_tokens")
                .is_none(),
            "未上报 ⇒ 不得写出 reasoning_tokens 键：{without}"
        );
    }

    // `RoundUsage` 是**非终局**事件 —— 消费循环（`-p` / `--json`）的退出条件只认
    // `RunFinished` / `RunFailed`。
    //
    // ⚠️ 本用例只钉**事件形状**（能解构、字段对得上），**不钉**「非终局」契约本身：
    // `is_terminal` 在 app 侧（`apps/coding-agent/src/main.rs`），`ys-event` 看不到它。
    // 那条契约由集成用例 `is_terminal(RoundUsage) == false` 保证（exec-plan Task 14）。
    #[test]
    fn test_round_usage_event_destructures_as_non_terminal() {
        let event = AgentEvent::RoundUsage {
            purpose: RoundPurpose::Compaction,
            round: 4,
            usage: usage(Some(4)),
        };
        match event {
            AgentEvent::RoundUsage {
                purpose,
                round,
                usage,
            } => {
                assert_eq!(purpose, RoundPurpose::Compaction);
                assert_eq!(round, 4);
                assert_eq!(usage.reasoning_tokens, Some(4));
            }
            other => panic!("期望 RoundUsage，实得 {other:?}"),
        }
    }
}
