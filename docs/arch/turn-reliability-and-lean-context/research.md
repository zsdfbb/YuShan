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
| **① 零产出 + 无终止信号 ⇒ `Err`** | Pi 只按 `stopReason === "error" \| "aborted"` 收场，对「有响应但内容为空」不特判（`packages/agent/src/agent-loop.ts:215,225`）。**deepseek-harness 两种都判错**：HTTP 层「no response body」与「`stop` + 零 content」都映射到同一个 `EMPTY_RESPONSE` 错误码（`llm-deepseek/src/adapter.ts:708`、`llm-deepseek/src/translate.ts:109-138`、`llm-pi-ai/src/stream.ts:95-105`）。opencode **#37852**：aborted stream 被记成正常结束 ⇒ 返回空且无错误 | 判据同格，但我们**分两层**：传输格（一个字节都没到）⇒ `Err` ⇒ 既有有界重试；语义格（有终止信号但零可见产出）⇒ **同回合重采样**（见 `design.md` 决策 3），最终仍以 `NoOutput` 收场 —— 保留本仓「空产出是可见的正常终止路径」这条既定语义 |
| **② 一次性恢复的形状** | Pi 的上下文溢出恢复 = **布尔量 `_overflowRecoveryAttempted` + 至多一次 compact-and-retry**，失败后给一句「已尝试一次，请缩上下文或换更大的模型」（`packages/coding-agent/src/core/agent-session.ts:2172-2196`）；durable harness 把 `overflowRecoveryUsed` 写进 checkpoint（`packages/agent/src/harness/session/types.ts:120`） | **同向**：本仓的「至多一次」由 `TurnKind` 二态结构保证（ADR-0027），本轮把同一形状扩到另两种收场 —— 形状被独立佐证 |
| **③ 回合级工具输出预算** | **没有任何一家有回合级/会话级的工具输出总量闸门**（在 Pi 全仓搜 `totalToolBytes` / `aggregate` / `sessionBudget` 零命中）。三家都用**单条上限**：Pi `DEFAULT_MAX_BYTES = 50 KiB` + `DEFAULT_LIMIT = 100` 命中 + 单行 500 字符（`core/tools/truncate.ts:12-13`、`core/tools/grep.ts:41`），且**把上限写进工具描述**（`core/tools/grep.ts:78`）；deepseek-harness 的 pwsh 截断提示写「为省上下文只展示一部分 + 先搜行号再取」（`packages/shell/tool-pwsh-persistent/src/index.ts:17`）；opencode 2000 行 / 50 KB + 落盘指针；Codex CLI 只在出错时截到 900 token | ③ 是**本仓增量**。两个形状本仓**已经具备**（`grep` 描述已写明 `limit` / 500 字符 / 2000 行 / 50 KB；截断 notice 已带收窄指引），本轮只补**回合级总量**这一层 |
| **④ 上下文占用用真实读数** | Pi 的阈值压缩与溢出判定都读 provider 返回的 `usage`：`calculateContextTokens(usage)`（`harness/compaction/compaction.ts:164`）、`isContextOverflow(message, contextWindow)`（`ai/src/utils/overflow.ts:145`）；deepseek-harness 用错误文案 + 错误码 `CONTEXT_WINDOW_EXCEEDED` 判定（`llm-deepseek/src/adapter.ts:346`） | 本仓目前用「文本估算 + `fixed_prompt_tokens`」（ADR-0017）。两家都用真实读数 ⇒ 记为**候选切片 B2**（与 ADR-0017 协调），见 `design.md` §未纳入项 |
| **⑤ 溢出 ⇒ 压缩 + 重试同一请求** | Pi：`overflow && !overflowRecoveryUsed` ⇒ 先 prepare compaction 再重试（`harness/runtime/drive/response.ts:186-235`；已用过则 `providerError`）；deepseek-harness：`agent/request-error` 命中 `CONTEXT_WINDOW_EXCEEDED` ⇒ 压缩后重试，`maxOverflowRetries` 有界（`compaction/compaction-basic/src/index.ts:184-205`） | 本仓**没有**这条路：压缩在请求**之前**（Step 4b），请求仍溢出时是确定性 4xx ⇒ 直接 `RunFailed`（ADR-0028 明确不重试确定性 4xx）。记为**候选切片 B3** |
| **⑥ 空产出的重试策略** | deepseek-harness 把 `EMPTY_RESPONSE_CODE` 放进**默认可重试集**，与 `RATE_LIMIT` / `SERVER` / `TIMEOUT` / `TRANSPORT` 并列：`maxRetries = 5`、初始 500 ms、上限 10 s、抖动 0.1（`packages/llm/llm/src/retry-policy.ts:18-24,42-47`） | 本设计**采纳其语义**（零产出 = 可重试的退化结果），但把重试放在**循环层**（重采同一请求、不落盘、不占追问额度），以保住 `NoOutput` 的终局语义 —— 见 `design.md` 决策 3 |

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

## 4. 本轮新增的源码级证据（2026-10-13，三份本机克隆直读）

> 克隆位置：`tmp/pi`、`tmp/deepseek-harness`、`tmp/Codewhale`（后者在本轮抽到的路径里
> **没有**与「响应截断 / 空产出 / 上下文预算」相关的判据，故未计成证据）。

### 4.1 deepseek-harness —— 「完成但零内容」是退化结果，不是成功

```ts
// packages/llm/llm-pi-ai/src/stream.ts:95-105（同形实现：llm-deepseek/src/translate.ts:130-140）
case 'stop':
  // A terminal stop that produced no content blocks is a degenerate
  // provider completion, not a successful (empty) assistant message.
  if (message.content.length === 0) {
    return { kind: 'error', failure: { message: `model "…" returned a completed response with no content`, code: EMPTY_RESPONSE_CODE } }
  }
  return { kind: 'stop' }
```

- 同一个 `EMPTY_RESPONSE` 码也用于**HTTP 层空体**：`llm-deepseek/src/adapter.ts:708`
  `throw new LlmError('DeepSeek API returned no response body', 'EMPTY_RESPONSE')`。
  ⇒ 两家形态（「网关没送东西」与「模型说了个空」）在它那里**收敛到同一个码**。
- **该码默认可重试**：`packages/llm/llm/src/retry-policy.ts:18-24`

  ```ts
  const DEFAULT_MAX_RETRIES = 5
  const DEFAULT_INITIAL_DELAY_MS = 500
  const DEFAULT_MAX_DELAY_MS = 10_000
  const DEFAULT_JITTER_RATIO = 0.1
  const DEFAULT_RETRYABLE_CODES = Object.freeze([EMPTY_RESPONSE_CODE, 'RATE_LIMIT', 'SERVER', 'TIMEOUT', 'TRANSPORT'])
  ```

  ⇒ 「空产出 ⇒ 有界重采样」是**它们的默认行为**，不是可选增强。
- 上下文超限走**错误码 + 压缩后重试**：`llm-deepseek/src/adapter.ts:346` 把窗口超限文案映射到
  `CONTEXT_WINDOW_EXCEEDED_CODE`；`packages/compaction/compaction-basic/src/index.ts:184-205`
  在 `agent/request-error` 上捕获该码 ⇒ 压缩并重试，`maxOverflowRetries` 有界，
  且「一次成功响应」会重置该计数（`:172-179`）。

### 4.2 Pi —— 溢出恢复至多一次，且用真实 usage

```ts
// packages/agent/src/harness/runtime/drive/response.ts:186-192
const overflow = intent.at === 'assistant.effect_pending'
  && (isContextOverflow(response, intent.contextWindow) || isRecoverableLength(response, intent.intendedOutputLimit))
const overflowPreparation = overflow && !intent.generationContext.overflowRecoveryUsed
  ? await prepareOverflowCompaction(lane, drive, intent) : undefined
```

- 已用过（`overflowRecoveryUsed === true`）⇒ 不再准备恢复，落到 `providerError`
  （`:228`、`drive/structural.ts:1159`）；该布尔量**持久在 checkpoint/continuation 里**
  （`harness/session/types.ts:120,147`）。
- 会话层的用户可见版本在 `packages/coding-agent/src/core/agent-session.ts:2172-2196`：
  第二次失败时给「已尝试一次 compact-and-retry，请缩上下文或换更大的模型」，且
  `:2190` 明确**把失败的那条 assistant 消息从重试上下文里摘掉**（历史里仍留）。
  ⇒ 与「重采同一请求时前缀逐字节不变、缓存仍命中」是同一个思路。
- 溢出判定读**真实 usage**：`packages/ai/src/utils/overflow.ts:145`
  `if (contextWindow && message.stopReason === "stop")` 分支用 `usage` 与窗口比；
  `harness/compaction/compaction.ts:164` `calculateContextTokens(usage) =
  usage.totalTokens || usage.input + usage.output + usage.cacheRead + usage.cacheWrite`。
- 工具面：单条上限 + **描述里明示** + bash 单独 2× 缓冲（§1.4）。
  Pi **没有**自动续跑 / 自动追问（`long-horizon-completion/research.md` §0）。

### 4.3 与本设计的关系（一句话）

| 竞品事实 | 本设计的动作 |
|---|---|
| 「完成但零内容」是退化结果、且默认可重试（dsh） | **采纳语义**：新增决策 3「同回合重采样」；但重试放循环层，保住 `NoOutput` 终局 |
| 「一个字节都没到」与「零内容」在它那里同码 | **不采纳合并**：本仓分两层（传输格 `Err` / 语义格重采样），理由见 `design.md` |
| 溢出恢复「至多一次 + 从重试上下文摘掉失败消息」（Pi） | 与本仓 `TurnKind` 二态同形；「摘掉失败消息」被决策 3（重采样时不落盘）借用 |
| 溢出 ⇒ 压缩 + 重试（Pi / dsh） | 本仓缺这条路 ⇒ 记候选切片 **B3** |
| 用真实 usage 判占用（Pi） | 记候选切片 **B2** |
| 回合级工具输出总量：**无人做** | 本设计的 **B1** 是本仓增量 |
