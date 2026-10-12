# turn-reliability-and-lean-context — 同类软件调研

> 产出：`/arch-design` Step 2（调研同类软件）。用途：为 [`design.md`](design.md) 的
> ① 零产出判定与接力、② 回合级工具输出预算 提供业界对照。
>
> **本文是「跨产品取舍对照」的唯一去处** —— `design.md` 与 ADR 只留结论与本设计自身的理由，
> 不写「别人是怎么做的」（`AGENTS.md` 工作约定）。
>
> **外部检索受限**：本机 `TAVILY_API_KEY` 已超配额（`This request exceeds this API key's set
> usage limit`），故本轮**未做新的外部检索**，以仓内既有多产品调研 + 本机克隆源码直读为准
> （同 [`../turn-output-and-feedback/design.md`](../turn-output-and-feedback/design.md) 的处理）。

## 0. 结论速览

| 本设计的项 | 同类软件的做法 | 对本设计 |
|---|---|---|
| **① 零产出 + 无终止信号 ⇒ `Err`** | Pi 只按 `stopReason === "error" \| "aborted"` 收场；**对「有响应但内容为空」没有专门判据**（`packages/agent/src/agent-loop.ts:215,225`）。三家（opencode / Codex CLI / Cline）的 issue 里，「空响应被记成正常结束」「上下文超限报错」是**已知事故形态** | 本仓判据**更严**：把「一个字节都没到」判成失败并交给既有重试 —— 是补别人漏掉的那一格，不是照抄 |
| **② 一次性恢复的形状** | Pi 的上下文溢出恢复 = **布尔量 `_overflowRecoveryAttempted` + 至多一次 compact-and-retry**，失败后给一句「已尝试一次，请缩上下文或换更大的模型」（`packages/coding-agent/src/core/agent-session.ts:2172-2196`）；durable harness 把 `overflowRecoveryUsed` 写进 checkpoint（`packages/agent/src/harness/session/types.ts:120`） | **同向**：本仓的「至多一次」由 `TurnKind` 二态结构保证（ADR-0027），本轮把同一形状扩到另两种收场 —— 形状被独立佐证 |
| **③ 回合级工具输出预算** | **没有任何一家有回合级/会话级的工具输出总量闸门**（在 Pi 全仓搜 `totalToolBytes` / `aggregate` / `sessionBudget` 零命中）。三家都用**单条上限**：Pi `DEFAULT_MAX_BYTES = 50 KiB` + `DEFAULT_LIMIT = 100` 命中 + 单行 500 字符（`core/tools/truncate.ts:12,13`、`core/tools/grep.ts:41`）；opencode 2000 行 / 50 KB + 落盘指针；Codex CLI 只在出错时截到 900 token | ③ 是**本仓增量**。可借鉴的是两件形状：**把上限写进工具描述**（Pi 在 `grep` 描述里明写「截到 100 命中或 50 KB，先到先算」），以及 **单条上限 + 明确 notice** 的既有分工 |
| **④ 上下文占用用真实读数** | Pi 的阈值压缩读 provider 返回的 `usage`（`calculateContextTokens(usage)`，`packages/agent/src/harness/compaction/compaction.ts:164`），不做文本估算 | 本仓目前用「文本估算 + `fixed_prompt_tokens`」（ADR-0017）。Pi 这条路更准且更便宜 —— 记为**候选改进**（本设计不纳入，见 `design.md` §未纳入项） |

## 1. Pi（本机克隆 `tmp/pi`，HEAD `47236c8`）

### 1.1 收场只有两条出路，空内容不特殊

`packages/agent/src/agent-loop.ts:215` 在 `stopReason === "error" | "aborted"` 时直接 `return`；
`:225` 在「本轮无工具调用」时把 `hasMoreToolCalls` 置 false、内层循环退出。
**没有**「有响应但零内容」的分支 —— 记成什么就是什么。

### 1.2 溢出恢复：布尔量 + 至多一次 + 明确失败文案

`packages/coding-agent/src/core/agent-session.ts:2172`：

```ts
if (this._overflowRecoveryAttempted) {
  const errorMessage = contextOverflow
    ? "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
    : "Truncated response recovery failed after one compact-and-retry attempt.";
  …
}
…
// Case 1: remove the failed or truncated message from agent state, compact, and retry once.
this._overflowRecoveryAttempted = true;
```

三条可借鉴之处：

1. **至多一次**由**布尔量**表达，不是计数上限；
2. 恢复动作**把「失败的那条 assistant 消息」从重试上下文里摘掉**（历史里仍留）——
   避免把已失败的内容再喂一遍；
3. 第二次失败给**面向人的可执行文案**（缩上下文 / 换模型），不是只报错码。

### 1.3 可恢复的截断 vs 真截断

同文件把「被 provider 夹过、长度低于模型原本期望上限」判为 `isRecoverableLength(...)`
（独立于配置的上下文大小），与「撞上 `max_tokens`」区分处理。**这是本设计不做、但值得记下的一条**
（本仓的 `max_tokens=4096` 写死，尚无这层区分）。

### 1.4 工具输出：单条上限 + 描述里明示预算

- `core/tools/truncate.ts:12` `DEFAULT_MAX_BYTES = 50 * 1024`；`:13` `GREP_MAX_LINE_LENGTH = 500`；
- `core/tools/grep.ts:41` `DEFAULT_LIMIT = 100`；`:78` 的工具描述**把预算写给模型看**：
  「Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.」
- `core/bash-executor.ts:58,85` 单独给 bash 一条 `DEFAULT_MAX_BYTES * 2` 的缓冲与 50 KB 判据。

**没有任何回合级累计**：每条结果各自守上限，理论上一轮可以合法堆进 N × 50 KB。

## 2. 其他项目的相关事实（来自仓内既有调研，不重复搜集）

| 项目 | 与「上下文体积 / 空响应」有关的做法 | 来源 |
|---|---|---|
| opencode | 所有工具输出统一截（2000 行 / 50 KB），超限**落盘 + 回喂指针**，并在结果里写 *"Do NOT read the full file yourself"*；Kimi/opencode 的 **#37852**：aborted stream 被记成正常结束 ⇒ 返回空且无错误 | [`../turn-output-and-feedback/design.md`](../turn-output-and-feedback/design.md) §同类软件调研 |
| Codex CLI | 只在**出错时**把子 agent 结果截到 900 token | 同上 |
| Roo Code | 返回无上限 ⇒ #11364「子返回被截断 ⇒ 父不得不自己重读」——**截断而无指针 = 最坏形态** | 同上 |
| Cline | #9637：因 context-window 超限报错 | 同上 |
| Claude Code | Stop hook 用布尔量 `stop_hook_active` 保证「至多续跑一次」，且**用户中断时不触发** | [`../../adr/0027-completion-check-and-round-budget.md`](../../adr/0027-completion-check-and-round-budget.md) |

## 3. 本仓既有落点（不要重做）

| 既有能力 | 落点 | 与本设计的关系 |
|---|---|---|
| 截断保护（整批作废 + 回喂话术） | ADR-0020 / `crates/ys-loop/src/basic.rs` Step 8b | 本设计**沿用**，不动 |
| 完工复核（至多一次） | ADR-0027 / `apps/coding-agent/src/follow_up.rs` | 本设计**扩它的门**，不改它的结构 |
| 模型调用超时 + 有界重试（仅 `Err`） | ADR-0028 / `adapters/model-openai-compatible/src/lib.rs` | 本设计的 `Err` 自动落入它 |
| 单条结果收窄（grep 16 KiB / 软阈值 8 KiB） | `docs/arch/turn-output-and-feedback/`、`crates/ys-tool/src/{truncate,notice}.rs` | 本设计**复用**这两个常量与 notice 形态 |
| 落盘 + 指针（唯一实现） | `adapters/tools-web/src/store.rs`（`webfetch`） | 本设计 v1 **不做**，列为 v2 的第一候选 |
| 工具的 cwd 契约、只读并行 | `bash.rs` 描述、`is_concurrency_safe`（`tool-call-efficiency` R1/R3） | **已闭合**，不在本设计范围 |
