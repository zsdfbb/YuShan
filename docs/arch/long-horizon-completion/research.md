# Pi 调研 —— 长程循环 / 完成判定 / 重试与超时

> 产出：`/arch-design` Step 2（调研同类软件）。对象：`tmp/pi/` 本地克隆，HEAD `47236c8`（TypeScript monorepo）。
> 用途：为本目录 [`design.md`](design.md) 的 ①（放开单回合预算）/ ②（完工复核）/ ③（模型调用超时 + 重试）提供业界对照。
>
> **本文是「跨产品取舍对照」的唯一去处** —— ADR 与 `design.md` 只留结论与本设计自身的理由，不写「Pi 是怎么做的」
> （CLAUDE.md：注释/设计只写当前设计，不写竞品对照；对照归 `docs/arch/**/research.md`）。
> 文中所有 `文件:行号` 均相对 `tmp/pi/`；引用的是该 HEAD 的实际代码，不是文档描述。

---

## 0. 结论速览

| 本设计的项 | Pi 的做法 | 对本设计 |
|---|---|---|
| **① 放开单回合预算** | **没有轮数上限**（`agent-loop.ts:171` `while (true)`）；全仓 grep 无 `maxRounds` / `maxLoops` / `maxIterations` / `maxSteps` / `turnLimit` | **同向**。本仓取 `u32::MAX` 与 Pi 的实际行为一致。**Pi 没有 wall-clock 兜底**（它的兜底是 abort）⇒ 本仓的 48h 预算是**加强**，不是照抄 |
| **② 完工复核** | **没有**任何自动完工复核。`getFollowUpMessages` / `getSteeringMessages` 是**人**排队的队列 | ② 是**本仓自有**，非对齐。可佐证的是：Pi 用 `overflowRecoveryUsed` 布尔量表达「一次性恢复」、durable harness 用 checkpoint + continuation 表达「此处可续」——**「至多一次」是这类机制的通用形状** |
| **③ 超时 + 重试** | **三层**：provider 层（流开边界）、会话层（**整条 assistant 消息重放**）、durable harness 层（`retry_wait` 持久状态） | **可借鉴 4 条**（§4.1–§4.4）；**1 条代价过大**（§5.1「中途断流重试」需要渲染撤回） |

---

## 1. 循环与「完成」

### 1.1 没有轮数上限

`packages/agent/src/agent-loop.ts:171` 的 `runLoop`：

```ts
let pendingMessages: AgentMessage[] = (await config.getSteeringMessages?.()) || [];
while (true) {                                          // :171 外层：等 follow-up
  let hasMoreToolCalls = true;
  while (hasMoreToolCalls || pendingMessages.length > 0) {   // :175 内层：工具 + steering
```

内层退出条件是「本轮没有工具调用 && 没有待注入消息」，外层退出条件是「follow-up 队列空」。
**两个循环都没有计数上限。** 全仓搜索 `maxRounds|maxLoops|maxIterations|maxSteps|turnLimit` 零命中。

⇒ 本仓 ① 取 `u32::MAX`（= 不设）**与 Pi 的既有行为一致**；差异只在**兜底**：Pi 只有 abort，
本仓另加 wall-clock 预算（`design.md` §4.2.1）。**这是加强。**

### 1.2 终止的四种出路（`agent-loop.ts`）

| 出路 | 位置 | 语义 |
|---|---|---|
| `stopReason === "error"` / `"aborted"` | `:215` | 请求失败 / 被中止 ⇒ `return` |
| 本轮无工具调用且无待注入消息 | `:225` `hasMoreToolCalls = false` | **模型自称完工**（= 本仓 `Completed`） |
| 工具批要求终止 | `:235` `hasMoreToolCalls = !executedToolBatch.terminate` | 见 §1.4 |
| `shouldStopAfterTurn(turn)` 返回 `true` | `:252` | 一个**钩子**；`coding-agent` 层**未接线**（仅 `agent` 包自测用） |

### 1.3 「完成」= 模型不再调工具；**没有复核**

与 `failToolCallsFromTruncatedMessage`（`stopReason === "length"` 时整批工具作废）之外的判定为零。
**Pi 不会在模型停手后主动追问「你确定做完了吗」。**

`followUp` / `steering` 两条缝都是**人的输入队列**，不是系统判据：

- `packages/coding-agent/src/core/agent-session.ts:1424 _queueSteer` / `:1440 _queueFollowUp` —— push 的是用户文本；
- `followUp(text)` 会展开 skill 命令 / prompt 模板 —— 典型的**用户**入口；
- `getSteeringMessages()`（`:1603`）/ `getFollowUpMessages()`（`:1608`）把它们交给循环。

语义分工值得记录（本仓 ② 沿用同一分法）：

| 缝 | 注入时机 | 效果 |
|---|---|---|
| **steering** | 回合**之内**、轮边界 | 插话，**不增轮** |
| **follow-up** | 本该停**之后**、外层循环 | 再跑**一个完整回合** |

⇒ ② 的形状（回合边界续跑一个回合）与 Pi 的 follow-up 缝**同构**，只是**触发源从「人」换成「系统」**。
本仓不需要 Pi 的 hook 系统（ADR-0021 已否决），用一条纯函数判据即可。

### 1.4 工具级 `terminate`：让工具投票决定「本轮是否收尾」

`agent-loop.ts:590`：

```ts
function shouldTerminateToolBatch(finalizedCalls) {
  return finalizedCalls.length > 0 && finalizedCalls.every(f => f.result.terminate === true);
}
```

**全批一致**才终止。这是把「完成」的判定权**下放给工具**的一条缝
（对照本仓 `context.md` §5 **Q4「是否引入可验证验收」**）——比「模型自觉」硬，比「独立验证器」轻。
本仓工具结果无此字段 ⇒ **本轮不做**（记账，见 §5.2）。

### 1.5 durable harness 的「一次性」语义（佐证 ② 的形状）

`packages/agent/src/harness/runtime/drive/response.ts`——Pi 较新的、事件溯源的持久化执行引擎：

- `:228` `current.generationContext.overflowRecoveryUsed` —— **布尔量**，溢出恢复**至多一次**；
- `:239` `continuation: { kind: "need_assistant", overflowRecoveryUsed: true }` —— 恢复动作本身**写进 checkpoint**；
- `:315` `continuation: { kind: "may_finish", includeFinalAssistant: true }` —— 「此处可以收尾」也是一条**持久状态**。

⇒ 「某个恢复/续跑**至多发生一次**」在这类系统里**用一次性标志 + 结构早退表达**，不用计数器。
与本仓 `TurnKind` 二态枚举（`design.md` §4.1）是**同一形状**。

---

## 2. 重试（三层，各司其职）

| 层 | 文件 | 包住的边界 | 判据来源 |
|---|---|---|---|
| **L1 provider** | `packages/ai/src/utils/provider-retry.ts` | **流「打开」**（拿到响应头之前） | HTTP 状态码 + `x-should-retry` 头 |
| **L2 会话** | `packages/coding-agent/src/core/agent-session.ts:2890+` | **整条 assistant 消息** | 错误**文本**正则 |
| **L3 durable** | `packages/agent/src/harness/runtime/drive/response.ts:275-291` | 同上，但**状态落盘** | 同 L2 + 持久 `retry_wait` |

### 2.1 L1：`retryProviderRequest`（流开边界）

- **可重试判定**（`:23-35`）：`x-should-retry` 头**优先**（`"true"` / `"false"` 直接定论）⇒
  否则看状态码：`408 / 409 / 429 / >=500`；**`status === undefined`（网络层错误）⇒ 可重试**。
- **退避**（`:65-66`）：`min(0.5 * 2^n, 8) * 1000` 毫秒（**cap 8 s**）× `(1 - rand()*0.25)`（≤25% 抖动）。
- **服务端指定延迟**（`:37-49`）：`retry-after-ms` / `retry-after`（支持秒数与 HTTP-date）；
  **若超过 `maxRetryDelayMs`（默认 60 s）⇒ 立即抛错**，不傻等。
  `packages/ai/src/types.ts:186-193` 的注释说明了理由：失败要带 requested delay 冒泡，
  **让更高层带可见性地处置**。
- **可中止**（`:75-95` `abortableSleep`）：退避睡眠监听 `AbortSignal`。
- **为什么不直接用 SDK 自带重试**（`:97-104` 注释）：**SDK 的重试定时器忽略 `AbortSignal`** ⇒
  必须让 SDK `maxRetries: 0`、自建这层循环，退避才可中断。

### 2.2 L2：整条 assistant 消息重放（`_retryAttempt`）

`agent-session.ts:2890-2945`：

```ts
this._retryAttempt++;
if (this._retryAttempt > settings.maxRetries) { this._retryAttempt--; return false; }
const delayMs = settings.baseDelayMs * 2 ** (this._retryAttempt - 1);   // 指数退避
this._emit({ type: "auto_retry_start", attempt, maxAttempts, delayMs, errorMessage });
// Remove error message from agent state (keep in session for history)
this.agent.state.messages = messages.slice(0, -1);                       // ← 丢掉失败的那条
this._retryAbortController = new AbortController();
await sleep(delayMs, this._retryAbortController.signal);                 // ← 退避可中止
```

四个可借鉴点：

1. **重放单元是「整条 assistant 消息」**：失败的第 N 次尝试**从 agent state 摘掉**（注释明确「keep in session for history」），
   下次尝试从头生成 ⇒ **输出原子化**，不拼接两次尝试的碎片。
2. **重试对用户可见**：`auto_retry_start` / `auto_retry_end`（`:169-170`）是**事件**，UI 据此渲染「重试中」并清理上一段渲染。
   压缩等旁路调用用 `RetryCallbacks`（`onRetryScheduled` / `onRetryAttemptStart` / `onRetryFinished`）走同一套可见性。
3. **退避可中止**：独立 `AbortController`（`:2925`）+ `abortRetry()`（`:2950`）+ `isRetrying`（`:2955`）。
4. **溢出不走重试**：`_isRetryableError`（`:2853-2857`）先判 `isContextOverflow(...) ⇒ false`，
   注释写明「Context overflow is handled by compaction, not retry」。

**文本判据**（`packages/ai/src/utils/retry.ts`）——两个正则，**先否后肯**：

- `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN`（`:7-24`，**优先级更高**，`:227` 先测）：
  `GoUsageLimitError` / `FreeUsageLimitError` / `Monthly usage limit reached` / `available balance` /
  `insufficient_quota` / `out of budget` / `quota exceeded` / `billing`。
  注释点明这些是**订阅/账户额度**，不是瞬时限流。
- `RETRYABLE_PROVIDER_ERROR_PATTERN`（`:26-90`）：几十条，覆盖
  限流/5xx（`overloaded` / `rate.?limit` / `429` / `50x` / `524`）、上游透传（`provider.?returned.?error`）、
  网络（`socket hang up` / `fetch failed` / `getaddrinfo` / `ENOTFOUND` / `upstream.?connect` / `reset before headers`）、
  **半截流**（`ended without` / `stream ended before message_stop` / `http2 request did not get a response`）、
  显式重试指引（`you can retry your request`）、gRPC（`ResourceExhausted`）。

⇒ 文本判据的唯一目的：**状态码分不出、但语义上确定**的失败（provider 把错误塞进 200 body、上游把 5xx 包成 400 等）。

### 2.3 L3：持久化重试状态

`response.ts:275-291`：失败后若可重试且未超 `maxAttempts`，**不立即 sleep**，而是落到一个持久状态：

```ts
settled = { ...scope, at: "assistant.retry_wait", generationContext, nextAttempt,
            notBefore: retryNotBefore(retryPolicy.baseDelayMs, current.attempt), errorMessage };
```

配套：流式产出先写进 `pendingAssistantFrames`（未提交帧），
settle 时 `deleteList(pendingAssistantFrames(...))`（`response.ts:341-346`）——
**重试前把上一段未提交的输出删掉**。这是 Pi 能安全重放「已经吐过内容的尝试」的机制根源。

---

## 3. 超时

`packages/ai/src/types.ts`：

- `timeoutMs`（`:159-162`）：**HTTP 请求超时**；注释点明「OpenAI / Anthropic SDK 客户端默认 **10 分钟**」。
  ⇒ **HTTP 路径上 Pi 不自建超时，吃 SDK 默认的单一总超时。**
- `websocketConnectTimeoutMs`（`:210-215`）：**仅**覆盖 WebSocket 的连接/握手；
  注释明说「stream idleness after connection uses `timeoutMs`」——**只有 WebSocket 才分成 connect / idle 两个旋钮**。
- `maxRetries`（`:164-167`）：SDK 自带重试次数（SDK 默认 2）；Pi 自建 L1 时要求置 0。

⇒ 对长思考模型（首字节数十秒），Pi 的 HTTP 路径靠「10 分钟总超时」兜——**粗，但不误杀**。
本仓 ③ 的「connect + 流空闲两旋钮」（`design.md` §4.8）**比 Pi 细**，且理由（首字节延迟 ≠ 中途停顿）在本仓成立。

---

## 4. 可借鉴（本轮引入）

### 4.1 服务端指定延迟超过上限 ⇒ **立即失败**，不傻等

Pi 的 `retry-after` 处理是「超过 cap 就抛，让上层带可见性处置」（`provider-retry.ts:37-49`）。
本仓原设计写的是「与其上界取 max」——那会**被一条超长 `Retry-After` 拖死**（48h 无人值守下尤其危险）。
⇒ 改为：**超过 `max_delay_ms` 即放弃重试并如实上报**。

### 4.2 账单 / 配额类**永不重试**（即便状态码是 429）

`429` 在语义上是二义的：可能是瞬时限流（该重试），也可能是**账户额度耗尽**（重试一万次也没用）。
Pi 用「先测不可重试名单、再测可重试名单」的正则实现。
本仓落点更窄也够用：**从错误体文本里先过一遍「不可重试」名单**，命中即跳过重试分支。
⇒ 显式区分这两类，避免 48h 任务在额度耗尽后做 5 次无用的指数退避（并让 `RunFailed` 的消息更快到达人眼前）。

### 4.3 `x-should-retry` 存在时，以其为准

Pi 的判据里该头**优先于状态码**（`provider-retry.ts:24-26`）。
本仓采纳为一行 if：**头存在 ⇒ 直接定论；不存在 ⇒ 回落到状态码表**（`design.md` §4.8 规则 2）。
国内 OpenAI 兼容端点多半不发这个头，故它是**零成本的可选增强**。

### 4.4 重试必须**可见**

Pi 把每次重试作为事件发给 UI，并配 `RetryCallbacks` 三条回调（§2.2 第 2 点）。
本仓不许新增协议变体 ⇒ 落点改为**经既有 `logging` 落一行**（`design.md` §4.9）：
`retry attempt k/n delay=…ms reason=…`。48h 无人值守下，「静默重试 50 次」与「一次都没重试」必须可区分。

---

## 5. 不适用 / 代价过大（记账）

### 5.1 中途断流后重试（Pi 的原子消息重放）—— **本轮不引入**

Pi 之所以敢在「已经吐过 delta」之后重试，靠的是 §2.3 的 **未提交帧 + 重放时删除**。
本仓对应位置的状态是：

- **会话侧本来就安全**：`BasicLoop` 的 `Forwarder`（`crates/ys-loop/src/basic.rs:24-40`）
  只把 `ModelEvent::TextDelta` / `ThinkingDelta` 转发成 UI 事件，**从不写会话**；
  会话里的 assistant 内容来自适配器收敛出的 `ModelResponse`。
  ⇒ 重试**不会污染对话历史**（Pi 的「keep in session for history」在本仓天然成立，因为没有中间态可留）。
- **渲染侧不安全**：TUI 把 delta 累积进 `TranscriptLine::Assistant`（`crates/ys-tui-coding/src/app.rs:241-261`
  `append_stream`），`-p` 把它直接写 stdout（`apps/coding-agent/src/main.rs:727`）。
  ⇒ 重放会让**已经显示过的文本再显示一遍**；且 `-p` 的 stdout **无法撤回**。

要照搬 Pi，需要一条「本次尝试作废」的信号。技术上门开着 ——
`ModelEvent`（`crates/ys-model/src/event.rs:5`）与 `AgentEvent`（`crates/ys-event/src/event.rs:5`）
**都是 `#[non_exhaustive]`**，加变体在 Rust 层**不破坏下游**。
但它**违反本设计自设的不变量 #1「零协议变体」**（`design.md` §5.1），且 `-p` 无法撤回 ⇒ 两条路径行为会分叉。

**记账**：「吐过 delta 后断流」本质是**完整性判定**（流是否完整结束），
归 `docs/arch/llm-api-unavailable/` 的正主；本仓 ③ 的硬规则「只在未吐 delta 时重试」保持不变。
若将来要做，**与那份完整性设计一并**（它本来就要区分「完整 vs 半截」），并接受一次协议变体。

### 5.2 工具级 `terminate`（`shouldTerminateToolBatch`）—— 本轮不做

它是 `context.md` §5 **Q4「可验证验收」**的第三种形态（工具投票、非模型自觉、非独立验证器）。
本轮裁决是「模型自觉 + 复核」（`design.md` §4.8 与 `context.md` §7 Q1）⇒ **不做**；记账为将来形态。

### 5.3 durable 重试状态（`assistant.retry_wait` 落盘）—— 不做

它依赖一套事件溯源的持久执行引擎（Pi 的 `packages/agent/src/harness/`），
本仓对应的是 `BasicLoop`（最高爆炸半径，ADR-0014）⇒ 不做。本仓的重试**在进程内**，进程死则重试预算重置。

### 5.4 退避睡眠可中止 —— **本仓做不到，如实记账**

Pi 的 L1/L2 都能在退避期间响应 abort（§2.1 / §2.2 第 3 点），因为它把 `AbortSignal` **一路传进了 provider 调用**。
本仓 `Model::complete(&self, request, sink)`（`crates/ys-model/src/trait_def.rs:12-16`）**没有任何取消入参**，
`Boundary::Abort` 只在 `BasicLoop` 的轮边界被读 ⇒ **退避期间按 Ctrl+C 无效**。

传入取消源 = 改核心 trait = 违反不变量 #1（且 `ys-model` 不得引 tokio）。
⇒ **如实记账**，并给出可行的界：**总退避有界**（建议 ≤ ~60 s，`design.md` §4.8 规则 3），
每次尝试由**流空闲超时**包住 ⇒ 最坏「不可中断」时长是**有界的**，不是无限。

### 5.5 溢出错误路由到压缩 —— 不做

Pi 的 `_isRetryableError` 先判 overflow ⇒ 不重试、交给压缩（§2.2 第 4 点）。
本仓压缩由**轮边界的 token 估算**触发（ADR-0017），不靠错误驱动；
要让 provider 报的溢出也走压缩，需要 `ModelError` 结构化（`crates/ys-model/src/error.rs` 是纯 `String`，
加结构是 breaking）⇒ **不做**。
本仓的 `400` 已在「不重试」表内（`design.md` §4.8 规则 2），**语义上已经对了**（不浪费重试），只是不自动改走压缩。

---

## 6. 对 ①②③ 的净结论

| 项 | 结论 |
|---|---|
| **①** | Pi 佐证「不设轮数上限」是既有实践；本仓的 wall-clock 预算是**在其之上加兜底**，不是对齐品 |
| **②** | Pi **无**自动复核 ⇒ ② 是本仓自有增量；其 durable harness 的「一次性恢复标志」佐证了「只问一次由结构保证」的形状 |
| **③** | 采纳 §4.1–§4.4 四条（retry-after 上限即失败、账单类不重试、`x-should-retry` 优先、重试可见）；§5.1 的中途断流重试**不引入**，归 `llm-api-unavailable` 的完整性设计；§5.4 退避不可中止**如实记账** |
