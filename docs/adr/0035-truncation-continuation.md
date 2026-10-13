# 0035 — 截断续写：纯文本被截断时回喂「续写」提示并继续一次，而非就地终结回合

给「模型响应被输出预算截断（`finish_reason = length`）」这条路径补上**续写**行为：纯文本被截断时，
不再是「诚实但**立刻**收场」（ADR-0020 决策 4），而是回喂一条续写催促、`continue` 让模型从断点接着把任务做完；
**只续一次**（一次性标志），第二次截断仍如实收场为 `StopReason::Truncated`。**不改**被截断工具批的既有语义
（仍整批作废 + 回喂错误结果 + 循环继续，ADR-0020 决策 3）。零 core crate 改动、零协议变体、零新依赖。

## Status

accepted（2026-10-13）

## 背景

- **实测故障**：coding-agent 每次遇到「model response was truncated (output limit reached)」后**停止执行**，
  没有完成预期任务。这行提示只由 `dropped_tool_calls == 0` 的分支产出
  （`crates/ys-tui-coding/src/app.rs` 的 `ResponseTruncated` 臂）⇒ 命中的是**纯文本截断**路径。
- **根因是一条刻意的终止**：`crates/ys-loop/src/basic.rs` Step 8b 在
  `finish_reason.is_truncated() && tool_calls.is_empty()` 时**直接**发 `RunFinished { Truncated }` 并返回
  （ADR-0020 决策 4：「纯文本被截断时不伪装 `Completed`」）。该决策针对的是**诚实性**（不得在被砍断的回答上打 ✓），
  但它把「这一轮没说完」与「回合该不该结束」**绑死**了 —— 于是模型在半句回答处停住、多步任务烂尾。
- **两条截断路径本就不对称**：带工具调用的截断**已经**回喂错误结果并 `continue`（ADR-0020 决策 3），
  循环继续、模型改小步重发；只有纯文本截断是「死墙」。同一事实（被输出预算掐断）不该有「一条能自愈、一条直接停」的分叉。
- **`StopReason::Truncated` 的价值不变**：它表达的仍是「这一轮模型没说完」，只是**不再**等价于「回合必须在此结束」。
  当续写也无法自愈（第二次仍被截断）时，它仍是终局原因 —— 诚实性由「绝不因截断而判 `Completed`」保证，而非由「立刻停」保证。
- **必须有界**：app 侧 `run_limits.max_rounds` 默认 `u32::MAX`（ADR-0027，「不设」），**不是**可靠的阀 ⇒
  对截断续写必须自带上限，否则一个总触顶的模型会空烧 token 直到 wall-clock 预算。

## 决策

**1. 纯文本被截断 ⇒ 回喂续写催促、`continue`（`crates/ys-loop/src/basic.rs` Step 8b）**

- 触发条件：`response.finish_reason.is_truncated()` 且 `tool_calls.is_empty()` 且本回合**尚未**续写过。
- 动作：向会话 append 一条 `Role::User` 的续写催促（常量 `TRUNCATED_CONTINUE_PROMPT`），`continue`（消耗一个 round）。
- 截断的文本已在本轮 Step 7 作为 assistant 消息落进会话 ⇒ 模型看得到自己说到一半的样子。

**2. 只续一次，用一次性标志封顶（不给无限续写）**

- 循环内布尔量 `truncated_continued`：置位后，**再**遇到纯文本截断即按 ADR-0020 的旧语义收场（`StopReason::Truncated`）。
- 形状照既有 `empty_retried`（空响应只重试一次）：**一次性标志 + 结构早退**，不用计数器（同 ADR-0027 借鉴 Pi 的写法）。
- **与工具批截断的区别（有意保留）**：带工具的截断**不设**此标志 —— 它回喂的是**逐工具的 `is_error` 结果**，模型有
  明确的自愈信号（改小 payload / 拆多次），继续是正常重试；纯文本截断没有工具可配对，无限续写只是空烧 token。

**3. 续写催促话术：纯常量、英文、`Role::User`（`TRUNCATED_CONTINUE_PROMPT`）**

- 点明「上一轮被输出预算掐断、这不是回合结束」，要求「从断点接着做、别重复已说过的」，并提示「正要发工具调用就用更小的 payload」。
- 与 `TRUNCATED_TOOL_NOTICE` 同源但**不同形态**：后者是 `ToolResult` 错误（有工具可配对），本项是 `User` 催促消息。

**4. 明确不做**

| 项 | 为什么不做 |
|---|---|
| 给工具批截断也加「只续一次」上限 | 它的回喂是逐工具 `is_error` + 收窄指引，属正常自愈重试；上限由 `max_rounds` / 时长预算兜底（既有语义，本 ADR 不动） |
| 无限续写（不设一次性标志） | app 默认 `max_rounds = u32::MAX`，一个总触顶的模型会一路空烧到 wall-clock 预算 |
| 把截断当错误（`RunFailed`） | 截断是模型侧**输出预算限制**，不是基础设施失败；继续走 `RunResult`（同 ADR-0020） |
| 新增 `StopReason` / 事件变体 | 现有 `Truncated` 与 `ResponseTruncated` 已足够表达；本 ADR 是行为变更，不是词汇变更 |

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 续写一次（采纳）** | 纯文本截断 ⇒ 回喂续写提示 + `continue`；一次性标志封顶 | 直接治「一撞截断就停」；与工具批路径对齐「能自愈就自愈」；有界 | `BasicLoop` 多一处分支 + 一个常量；N2 用例语义随之更新 |
| **B 保持终结，靠 app 层 follow-up 续一回合** | `should_review` 对 `Truncated` 也产出「续跑」输入，另起一个回合 | 零 `BasicLoop` 改动 | 与「工具批截断在同一回合内自愈」分叉成两套机制；且 `-p` / `--json` 单回合路径覆盖不到 ⇒ 只修一半 |
| **C 仍记 `Completed`** | 截断时判 `Completed` | 零改动 | 正是 ADR-0020 否决过的「在被砍断的回答上打 ✓」的静默谎 |

**采纳 = A**。理由：故障就在「回合被提前终结」这一条分支上，就地修最直接；B 会把同一事实拆成两条不同层级的机制，
且 `-p` / `--json` 拿不到修复；C 与 ADR-0020 的诚实性立场直接冲突。

## Consequences

- **正面**：命中纯文本截断后，模型拿到一次「继续」的机会 —— 长篇回答/未吐出的工具调用得以接着推进，
  「一撞截断任务就烂尾」被治；诚实性不破（仍绝不因截断判 `Completed`）；有界（至多一次）。
- **负面**：截断路径多一次 model 调用（仅当纯文本被截断时）；`truncated_text_ends_turn_as_truncated`（N2）语义更新
  （现在描述「续写一次仍截断才收场」），并新增一条「首次截断续写后完成」的回归用例。
- **行为变更（须写进 PR 描述）**：纯文本截断后，回合**不再立刻结束**，而是回喂一条续写催促再跑一轮；
  仍被截断才以 `StopReason::Truncated` 收场。带工具调用的截断行为**逐字节不变**。
- **未验证**：真实 provider 连续返回 `finish_reason: "length"` 的端到端行为（需真实截断，靠 `MockModel::push_truncated_text`
  在测试层覆盖；真实验证需 `e2e_*` 那条 `#[ignore]` 路径）。

## 关联

- [ADR-0020](./0020-truncation-guard.md) — 截断保护出处：本 ADR **修订其决策 4**（纯文本截断「诚实收场」→「先续写一次、仍截断才收场」）；
  决策 3（工具批整批作废 + 循环继续）与决策 5（`ResponseTruncated` 事件）**不变**
- [ADR-0027](./0027-completion-check-and-round-budget.md) — 「某类恢复至多一次用一次性标志 + 结构早退」的形状出处（本 ADR 沿用）
- `docs/design-final/truncation-guard.md` — 截断保护的 as-built（本 ADR 追加续写一条）
- `docs/design.md` §5 失败与终止语义 / §12 测试矩阵
