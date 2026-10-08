# 长程任务的「完成」（① 预算可配 + ② 完工复核 + ③ 超时重试）设计计划

> 日期：2026-10-09
> 决策记录：ADR-0027（单回合预算可配 + 一次性完工复核）、ADR-0028（模型调用超时 + 有界重试）
> 设计定稿：[`../arch/long-horizon-completion/design.md`](../arch/long-horizon-completion/design.md)（`/arch-design`，核心输入）
> 需求与实证：[`../arch/long-horizon-completion/context.md`](../arch/long-horizon-completion/context.md)（`/arch-explore`）
> 评审：[`../arch/long-horizon-completion/review.md`](../arch/long-horizon-completion/review.md)（`/arch-validate`）
> 同类软件调研：[`../arch/long-horizon-completion/research.md`](../arch/long-horizon-completion/research.md)（Pi @ `47236c8`）
> 基线：`master` @ `5eae60a`（工作区干净）
> 性质：**实现前计划**（配套 exec-plan：[`../exec-plans/2026-10-09-long-horizon-completion.md`](../exec-plans/2026-10-09-long-horizon-completion.md)）

**名称说明**：本计划的「完成」指**长程任务的收尾**，不含 todo / subagent / follow-up 闭环（那三者是另一问题类别的 ADR-0024 / ADR-0022，明确不做，见 §2）。

---

## 1. 需求与背景

**需求原话**：「考虑提升 coding-agent 的长程执行能力」+（第二轮追加）「**需要支持 48 小时的长程任务**」。

**实证的失败**：一次 `/arch-explore`（对照 Pi 找差距）任务在 **`⚠ 10 rounds · 60.9s`** 处被切断、**未产出结果**。
`context.md` §1 定位到两个**相互独立**的根因：

| 根因 | 位置 | 现象 |
|---|---|---|
| **R1 预算写死** | `RunLimits.max_rounds = 10`（`crates/ys-component/src/limits.rs:39`），app 侧**无覆盖路径** —— `resolve_limits` 只写 `context_window` / `compaction`（`apps/coding-agent/src/main.rs:361-366`） | 撞硬顶 ⇒ `RunResult { stop_reason: MaxRounds, final_message: None, .. }`，**无收尾机会** |
| **R2 回合边界是死墙** | `app_loop` 的 `Request::Prompt` 臂恰好一个回合，且**不看 `stop_reason`**（`apps/coding-agent/src/app_loop.rs:109-186`） | 撞顶 / 正常完成**走同一条路**：回到 `request_rx.recv().await` 等下一条输入 |

**用户裁决**（原话）：

- ① 「**不设置 MaxRounds，或者可以设置的极大**」；
- ② 「**模型自觉完成，不过需要提示模型确认是否完工**——模型认为完工之后，我们再发条消息找他 check」，
  并追加一问「**为什么不在 agent 不继续执行后，我们主动问一次模型是否需要继续？**」，
  **明确反对**「循环 + `MAX_CHECKS` + 停滞判据」的复杂形态 ⇒ 形状 = **只问一次**；
- ③（第二轮）「**需要支持 48 小时的长程任务**」，并就「48h 无人值守的头号缺口」确认**把模型调用重试纳入本轮**。

**48h 改写了 ① 的性质**：48 小时 = 172,800 s，按实测样本（`⚠ 10 rounds · 60.9s` ⇒ ≈6.1 s/轮，读密集型）：

| 每轮耗时 | 48h 能跑的轮数 |
|---|---:|
| 6 s | ~28,800 |
| 10 s | ~17,300 |
| 30 s（bash 偏重） | ~5,800 |
| 60 s | ~2,900 |

⇒ 48h 的真实预期落在 **3k–29k 轮**（1,000 / 10,000 **都不够**）。**任何装得下 48h 的轮数上限都已大到不再是一个阀**，
且把「48 小时」悄悄绑死在「每轮多久」上（差 3 倍以上）。⇒ 轮数上限交还用户原话「不设」（`u32::MAX`），
真正的兜底**改用小时计量**（wall-clock 预算，§3.4）。

**③ 与既有上下文冲突并对齐**：`docs/arch/llm-api-unavailable/context.md` §范围外当时明确「**不做自动重试**」
（需求是「报错并终止」）；48h 无人值守要求自愈 ⇒ **本轮 supersede 该条**（仅限模型调用路径，见 ADR-0028）。

---

## 2. 目标与非目标

### 2.1 目标

1. **① 单回合预算可配**：`config.json` 加**顶层标量** `max_rounds`（默认 `u32::MAX` = 不设），经 `resolve_limits` 注入；
   **核心默认 `RunLimits::default().max_rounds` 保持 10 不动**。⇒ 一次改动修 TUI / `-p` / `--json` 三模式。
2. **② 完工复核（只问一次）**：模型自称完工（`stop_reason == Completed`）且本回合够长时，
   系统主动发**一条**复核消息、另起一个回合；复核回合无论怎么结束都**不再问第二次**。
3. **③ 模型调用超时 + 有界重试**：连接超时 + 流空闲超时两个旋钮；有界退避重试（只在未吐 delta 时）。
   落**适配器**，零 `Model` trait / `ModelError` / `BasicLoop` 改动。
4. **wall-clock 预算**：`app_loop` 回合 `select!` 加计时器分支（默认 **48h**，经 `config.json` 的 `max_turn_duration_secs` 可调），
   到点 `push(Boundary::Abort)` + 一条说明行；`-p` / `--json` 同样兜底且**预算到点非零退出**。
5. **UI 修复（§4.6.1，零协议变体）**：复核回合在 TUI 渲染成 `System` 行并点亮 Working，不伪装成用户消息。

### 2.2 非目标（明确不做）

- ❌ **不做 todo 闭环**（ADR-0024 的落盘 + 注入 + 按「有未完成项」推进）—— 是另一问题类别（有清单的长任务）；
  本例 `/arch-explore` **无清单**，已证其盖不住。
- ❌ **不做 subagent**（ADR-0022）—— 与本轮裁决无关。
- ❌ **不做 follow-up 的计数上限 / 停滞护栏 / 用户输入抢占 / 逃生开关** —— 复核结构上至多一次，用不上（用户明确反对）。
- ❌ **不修 `MAX_CONSECUTIVE_ERRORS` 的空分支**（`crates/ys-loop/src/basic.rs:514-520` 只有注释、行为为零）——
  在 `BasicLoop` 内，**最高爆炸半径**；**如实记账**为已知缺口。
- ❌ **不修 `llm-api-unavailable` 的两个完整性缺口**：**200 + 非 SSE 错误体被判成功**、
  **流干净结束但无 `[DONE]` 被判完整** —— 属「完整性判定」，仍归那份上下文的设计；
  **记账**：48h 下它们**比缺重试更危险**（静默把半截/空响应当 `Completed`，等于静默丢工作）。
- ❌ **不给 `-p` / `--json` 加复核** —— 其事件消费者 `consume_events` **以第一个终局事件为终止条件**，
  复核回合的第二个 `RunFinished` 会撞上「消费者已退出」的 `ChannelSink`（与 ADR-0024 同一类退化，明确接受）。
  **但 `max_rounds` 放开与 wall-clock 预算对它们生效。**
- ❌ **不新增 `StopReason` / `ModelError` 变体**（前者改 `BasicLoop`、后者 breaking）—— 时长预算收场复用 `Cancelled`（§3.4）。
- ❌ **不引「中途断流后重试」** —— 需一条「本次尝试作废」的渲染信号（违反不变量「零协议变体」），
  且 `-p` 的增量已直写 stdout、无法撤回；本质是完整性判定，归 `llm-api-unavailable`。
- ❌ **不把退避做成可中止** —— 需 `Model::complete` 收取消源 ⇒ 改核心 trait；以「有界退避 + 流空闲超时」作界。
- ❌ **不引 hook 系统 / `trait` / registry / `Vec<Box<dyn _>>`**（ADR-0014/0021）。
- ❌ **不凭估算调死默认值** —— 48h 的轮数/耗时是**单一实测样本**的估算；`REVIEW_MIN_ROUNDS` 与两个超时阈值为**待校准初值**。

---

## 3. 总体方案

总原则：**①②落 `apps/coding-agent`，③落适配器，UI 修复落 `ys-tui-coding` 的渲染分支。零 `BasicLoop` 改动、零 core crate 改动、零新协议变体。**

### 3.1 ① 单回合预算可配：`max_rounds`（默认 `u32::MAX` = 不设）

照 `context_window` / `compaction` 的既有「**Raw → 校验 → 规整**」三段式（`config.rs:20-27` / `:250-271` / `:277-289`）：

- `apps/coding-agent/src/config.rs`：
  - `RawUserConfig` 增 `#[serde(default)] max_rounds: Option<serde_json::Value>`（**顶层标量**，非按 model 的 map ——
    预算是回合属性，与 model 无关）；
  - `UserConfig` 增 `max_rounds: Option<u32>`；
  - `parse_user_config` 增一支校验：`as_u64()` → `u32::try_from` → `>= 1`；**非法只丢该条并落 `crate::logging::log`**
    （与既有宽容语义同规，不 panic、不整份回退）；
  - 取数口 `Config::max_rounds(&self) -> u32`（覆盖 > 默认 `u32::MAX`）。
- `apps/coding-agent/src/main.rs` `resolve_limits`（`:361-366`）加**一行** `limits.max_rounds = config.max_rounds();`
  —— **唯一接线点**，`build_agent` 已 `.limits(limits)`（`main.rs:340`）⇒ TUI / `-p` / `--json` 一次修复。
- `Agent` **不加 setter**：`max_rounds` 不随 model 变，启动定死即可。

**⚠ 核心默认值不动（结构性理由两条）**：

1. `crates/ys-component/src/lib.rs:18` 有断言 `assert_eq!(limits.max_rounds, 10)`；
2. `app_loop` 既有测试 `abort_yields_cancelled_and_view_still_emitted`（`app_loop.rs:823`）用
   `ProbePlan::AlwaysToolCall` 靠**核心默认 10** 在可接受轮数内撞 `MaxRounds`；改成极大值会让它空转。

⇒ **「库的保守默认」与「app 的产品默认」分离**，是既有的分层，本设计不破坏。

### 3.2 ② 完工复核：`follow_up.rs` 纯函数 + `app_loop`（**先纯重构、后 `pending.take()` 循环**两步）

**（a）新建 `apps/coding-agent/src/follow_up.rs`**（只放三样，**不引 `trait` / registry / 规则列表**），
并在 `main.rs` 声明 `mod follow_up;`：

```rust
/// 二态而非计数器：`Review` 永远不可能再产出续跑 —— 「只问一次」的**全部**实现。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnKind { Initial, Review }

/// 复核下限轮数：`rounds >= 2` ⟺ 本回合模型至少发出过一次 tool_use；
/// 取 `5`（≈4 批工具）是为把**短回合挡在门外** ——
/// 「复核噪音 ↔ 早收风险」的唯一旋钮，初值由 §6 校准。
pub(crate) const REVIEW_MIN_ROUNDS: u32 = 5;

/// 纯函数判据：无 I/O、无状态、无计数 —— 可脱离 app_loop 的重型测试台单测。
pub(crate) fn should_review(kind: TurnKind, result: &RunResult) -> Option<AgentInput> {
    if kind != TurnKind::Initial                  { return None; } // ←「只问一次」
    if result.stop_reason != StopReason::Completed { return None; } // ← 门 ①：Cancelled/Truncated/MaxRounds 一律不续
    if result.rounds < REVIEW_MIN_ROUNDS          { return None; } // ← 门 ②：本回合不够长
    Some(AgentInput::text(REVIEW_PROMPT))
}

/// 复核文案。**纯常量、确定性字节**（无时间戳 / 回合号 / 计数，照 ADR-0024 决策 2）。
pub(crate) const REVIEW_PROMPT: &str = "Before we wrap up: verify that the original request is \
    actually complete. Compare what was asked against what has been done. If anything is missing, \
    unfinished, or unverified, keep working and finish it now using the available tools. If it is \
    genuinely complete, reply with a short confirmation and stop.";
```

**为什么门 ② 用 `rounds` 计数**：`rounds >= 2` 等价于「本回合模型至少发出过一次 `tool_use`」
（`BasicLoop` 只有在上一轮 `tool_calls` 非空时才回环、进而使 `rounds` 越过 1）。它**直接读 `RunResult.rounds`**
（`crates/ys-loop/src/result.rs:7`，`pub`），**零事件管道、零新增状态** —— 规避「扫事件转发点会漏点」的风险。
**门槛取 `5` 而非 `2`**：`2` 只排除纯文本完工，「读一个文件就答」这类短回合仍满足它 ⇒ 复核会落在**几乎每次 coding 交互**上；
而短回合即便早收，代价也只是用户再补一句。抬到 `5` 把短回合挡在门外，长回合（早收代价大）仍全程受复核。

**（b）第一步（纯重构）：`app_loop` 抽出 `run_turn_once` + 回合前排空前移 —— 行为逐字节不变**：
把现在 `app_loop.rs:130-163` 的「`begin_turn` → 新建 `QueueBoundarySource` → `select!` 三臂 → 回合末 `try_recv` 排空」
抽成一个私有 async fn。**既有 15+ 条 `app_loop` 测试就是这次纯重构的守门员**（行为须逐字节不变，review 判据明确），
风险低；本步**不**改 `Request::Prompt` 臂的循环形状、不引 `pending`（循环是下一步）。
**把回合前的边界排空移进 `run_turn_once` 开头**（现在 `app_loop.rs:100` 的
`while boundary_rx.try_recv().is_ok() {}` 在**外层 `loop` 顶部、每个 `request_rx.recv()` 之前**都跑）——
复核回合也必须排空：否则上一回合残留的 `Boundary::Abort` 会泵进复核回合把它取消。
这正是模块注释 ③（`app_loop.rs:43-44`「`Abort` **永久**置位」）要守的不变量：**新建源只防标志位泄漏，防不了队列里那条消息。**

**（c）第二步（循环）：`Request::Prompt` 臂的新形状**（**逐字采用**设计稿的 `pending.take()` 循环）：

```rust
let mut pending = Some((AgentInput::new(message), TurnKind::Initial));
while let Some((input, kind)) = pending.take() {
    turns += 1;
    // 标题素材只取首个回合的用户原文；复核回合传 None（绝不把复核常量写成会话标题）
    let raw = (kind == TurnKind::Initial).then_some(raw_user_text.as_deref()).flatten();
    match run_turn_once(&mut agent, &mut wiring, /* … */ turns, input).await? {
        Ok(result) => {
            stats.record(&result.usage);
            finalize_turn_meta(/* … raw … */).await;                 // 失败落日志、不中断（既有）
            pending = should_review(kind, &result).map(|next| (next, TurnKind::Review));
        }
        Err(e) => logging::log(&format!("turn {turns} failed: {e}")), // BasicLoop 已发 RunFailed ⇒ 不续跑
    }
}
```

**「不问第二次」由两条结构保证**：`should_review` 的首个早退（`kind != Initial`），且 `pending` 只在 `Initial` 时会填。
**没有计数上限、没有停滞判据、没有抢占、没有逃生开关** —— 与用户裁决一致。

### 3.3 ③ 模型调用超时 + 有界重试（落适配器）

**为什么成对**：没有超时，第一次请求在「接受连接但不应答」的黑洞端点上**永久挂起**（`reqwest::Client::new()` 未设任何 timeout）
—— 根本没有「失败」可供重试。故 ③ = 超时 + 有界重试一起做。

落点全部在 `adapters/model-openai-compatible`（包名 `ys-model-openai-compat`）。三条理由：
只有适配器知道 HTTP 状态码与连接错误；`ys-model` / `ys-core` / `ys-event` / `ys-protocol` 不得引 tokio；
**零 `Model` trait / `ModelError` / `BasicLoop` 改动**。

**三条硬规则**：

1. **只在「尚未吐出任何 delta」时重试**。适配器在 `complete` 里包一层 **sink 计数器**：一旦发出过
   `TextDelta` / `ThinkingDelta`，其后的任何失败**直接上报、不重试** —— 否则重试会把已显示的文本**再吐一遍**
   （`-p` 把增量直写 stdout，**无法撤回**）。**计数器只看这两个事件**：工具参数增量**不冒泡**
   （`ModelEvent` 只有 Text / Thinking 两态，工具参数在适配器内部组装）⇒ 不构成「已显示」。
2. **可重试判据分三级，从左到右短路**（适配器侧）：

   | 级 | 情形 | 判定 |
   |---|---|---|
   | **① 响应头定论** | 存在 `x-should-retry` | 以其为准（`true`/`false`），**不看状态码** |
   | **② 额度否决** | 错误体文本命中**额度名单** | **不重试**（即便状态码是 `429`） |
   | **③ 状态码** | 连接 / DNS / TLS / 超时错误、`429`、`500/502/503/504` | **可重试** |
   | | `400/401/403/404/413/422`、serde 解析失败、sink 错误 | **不重试**（确定性失败）|

   额度名单（大小写不敏感子串，**开放常量**，扩词是加法、不得演变成规则引擎）：
   `insufficient_quota` / `quota exceeded` / `out of budget` / `billing` / `usage limit` / `available balance`。
   **`429` 语义二义**（瞬时限流 vs 账户额度耗尽），后者重试一万次也没用 ⇒ 命中即**立即 `RunFailed`**，
   让额度耗尽的消息尽快到达人眼前。
3. **有界退避**：指数退避 + 抖动，**次数与上限可配**（建议 base `500ms`、×2、cap `30s`、最大 5 次 ⇒ 总等待 ≤ ~60s）；
   带 `Retry-After` / `Retry-After-ms` 时**尊重它，但以 `max_delay_ms` 为硬上限**
   —— **服务端要求的等待超过上限 ⇒ 放弃重试、立即上报**（错误消息带上服务端要求的秒数）。
   **不取「与上界取 max」** —— 那等于让服务端**无界地**把回合挂住。

**超时用两个旋钮，不用单一总超时**（总超时会误杀长思考模型 —— `deepseek-reasoner` 首字节可达数十秒）：

- `connect_timeout`（连接建立，建议 ~15 s）；
- **流空闲超时**（建议 ~120 s 无字节）—— 首字节延迟与中途停顿语义不同，分开设。

两者都在适配器内实现：`reqwest` builder（`with_client` 注入带 timeout 的 `reqwest::Client`，替代 `lib.rs:37` 的裸
`Client::new()`）/ `RequestBuilder::timeout` + `tokio::time::timeout` 包住字节流。

**重试可见（本轮定）**：进入退避前经既有 `logging`（`apps/coding-agent/src/logging.rs::log`）落一行
`retry attempt k/n delay=…ms reason=…`。**落点是 `logging` 不是事件** —— 重试**对排障可见、对 TUI 不可见**
（有意取舍，护住零协议变体）。

**退避不可中止（如实记账）**：`Model::complete`（`crates/ys-model/src/trait_def.rs:12`）**没有取消入参**，
`Boundary::Abort` 只在轮边界被读 ⇒ **退避睡眠期间 Ctrl+C 无效**。可行的界是「总退避 ≤ ~60s + 每次尝试由流空闲超时包住」。

**配置**：`config.json` 增 `model_retry`（`max_attempts` / `base_delay_ms` / `max_delay_ms`）与
`model_timeout`（`connect_secs` / `idle_secs`）两段，经 `build_agent`（`main.rs:329-342`）传入适配器构造；
`max_delay_ms` **兼作 `Retry-After` 的硬上限**。

### 3.4 wall-clock 预算（默认 48h）—— 零 `BasicLoop` 改动

- **落点**：`run_turn_once` 的 `select!` 加一条 `tokio::time::sleep_until(deadline)` 分支，到点向当前回合的边界源
  `push(Boundary::Abort)`，并在**同一处先发一条** `Outbound::Output` 说明行（`budget exhausted: …`）。
  `deadline` 由 **`config.max_turn_duration()`** 在 `Request::Prompt` **入口算一次**，覆盖整个 `pending.take()` 循环（含复核回合）
  —— 即「一次用户请求 = 一段 48h 预算」。
- **默认 48h（172,800 s）、经 `config.json` 调**：`config.json` 增**顶层标量** `max_turn_duration_secs`（单位秒，默认 `172800`），
  照 `max_rounds` 同规（Raw → 校验 → 规整，非法值丢该条 + 落 `crate::logging::log`），
  取数口 `Config::max_turn_duration() -> std::time::Duration` 缺省回落 `Duration::from_secs(172_800)`。
  **不是硬编码常量** —— 配置化是落地的一部分（唯一能注入极短预算来测 48h 兜底的途径，见 §5.3）。
  它**只是 backstop** —— 正常任务在到点前早就由模型自己收尾，只有失控/挂起才会撞上它。
- **为什么能零 `BasicLoop` 改动**：`Boundary::Abort` 是既有机制，`BasicLoop` 在轮边界读取它（**已实现**）。
- **两条代价，如实**：
  1. 这样收场时 `StopReason` 只能是 **`Cancelled`**，与用户 Ctrl+C **同态** —— UI 分不出来，靠那条 `Outbound::Output` 行区分
     （要真正区分需新增 `StopReason` = 改 `BasicLoop`，本轮**不做**）。
  2. `Abort` **只在轮边界被读** —— 若到点时模型正卡在一次 `complete()` 里，真正的停止会**推迟到下一次轮边界**
     （最坏再多一轮，含单工具 300s 超时）。
- **`-p` / `--json`**：其 `run_single_turn`（`main.rs:717-725`，现**不带 boundary**）需加一条**合成 `QueueBoundarySource`**
  + 同一计时器分支，方能同样兜底；**预算到点非零退出**（与「用户中止」同路的 `Cancelled` 语义），
  原因由那条 `Outbound::Output` 行说明 —— **不得以 `0` 退出**（无人值守的调度器 / CI 必须能看出「没跑完」）。
- **这是一次「细阀换粗阀」（如实记账）**：去掉轮数上限后，回合内**唯一的**防失控阀就是这条 wall-clock 预算，
  而它**很粗** —— 小输出空转可持续数小时才到点（`MAX_CONSECUTIVE_ERRORS` 仍是空分支）。

### 3.5 可观测性（48h 无人值守的必要面）

- **每回合可见性已有**：TUI 的 `RunFinished` ⇒ `TranscriptLine::Summary { rounds, elapsed_secs, .. }`（`app.rs:136-147`）。
- **新增（低成本）**：每个回合边界（`run_turn_once` 结束处）经既有 `logging` 落一行
  `turn=N rounds=R elapsed=Es cumulative_rounds=CR budget_remaining=Bs`；headless / `-p` 下这是**唯一**的进度面。
- **预算到点**：§3.4 的那条 `Outbound::Output` 行**必须**存在 —— 它同时是「人停 vs 预算停」的唯一区分。
- **重试可见**：§3.3 的 `retry attempt …` 行。
- **不做**：不往 `Envelope` / `AgentEvent` 加遥测字段；不做实时 TUI 进度条。

### 3.6 UI 修复（§4.6.1，**零协议变体**）

TUI 侧**没有回合开始的信号** —— `is_turning` 只由 `begin_local_turn` 置位（`crates/ys-tui-coding/src/app.rs:194-199`），
而 app 侧的 `begin_turn` 是 sink 行为、**不发事件**；且**非回显 `UserMessage` 已被 TUI 占用**为「轮边界 steering 插话」
并渲染成**用户行**（`app.rs:97-100`）⇒「非回显 ⇒ 是复核」这条捷径**不成立**（steering 也是非回显）。

**判别式（零协议）**：用**非回显 `UserMessage` 到达时 `is_turning` 的真假**区分二者：

| 来源 | 到达时机 | `is_turning` |
|---|---|---|
| 轮边界 steering 插话 | 回合**进行中**（输入侧按 `is_turning` 分流，`events.rs:91,108`） | `true` |
| app 发起的复核回合 | 主回合 `RunFinished` **之后**（`finish_turn` 已复位） | `false` |

**改法**（`app.rs` 的 `apply_event`，非回显 `UserMessage` 分支，现 `app.rs:91-101`）：

```rust
AgentEvent::UserMessage { message } => {
    if self.suppress_user_echo > 0 {                 // 本地已回显，跳过（既有）
        self.suppress_user_echo -= 1;
        return;
    }
    if self.is_turning {
        // 轮边界 steering 插话（既有行为）
        self.transcript.push(TranscriptLine::User(message_text(&message)));
    } else {
        // app 发起的回合（完工复核）：主回合已 RunFinished ⇒ 不在 turn 态
        self.transcript.push(TranscriptLine::System(message_text(&message)));
        self.is_turning = true;                       // Working 指示器亮起
        self.turn_started_at = Some(Instant::now());  // 复核回合 elapsed 从此刻起算
    }
    self.follow = true;
}
```

- **渲染成 `System` 行**而非用户行：复核是**系统**发起的提示，用户在 transcript 里不该看到一条自己没打过的「用户消息」。
- **不改 `Outbound` / `AgentEvent` / `ys-protocol`** ⇒ 零协议变体仍成立（改的只是 `ys-tui-coding` 的**渲染分支**）。
- **代价 / 依赖**：判别式依赖一条**隐式不变量** ——「app 发起的回合总在主回合 `RunFinished` 之后」。
  事件在同一条信道内**有序**（`RunFinished` 必先于复核的 `UserMessage` 被处理），故成立；由一条 TUI 测试钉住。

---

## 4. 约束与不变量

**硬约束（新增代码违反任何一条都算设计偏离，需先改设计文档）**：

1. **零 `BasicLoop` 改动、零 core crate 改动、零新协议变体** —— ①②落 `apps/coding-agent`，③落适配器，
   **零 `Model` trait / `ModelError` 改动**；UI 修复（§3.6）只改 `ys-tui-coding` 的**渲染分支** ⇒ `ys-protocol` 一字节不改。
2. **`ys-core` / `ys-event` / `ys-protocol` 零 tokio**（直接与间接皆无）。
3. **`ys-tui-coding` 不依赖 `ys-runtime`**（编译器强制）；UI 线程无 runtime 上下文（`blocking_send` 在 async 上下文会 panic）。
4. **`system` / `tools` 一字节不改**；复核只走 `messages` 尾部（ADR-0021 决策 1）。
5. **每用户输入至多多出一个复核回合**（`TurnKind` 结构保证，非计数）。
6. **`Abort` 不跨回合泄漏**（队列残留也在每回合开头被排空）。
7. **复核文案字节确定**（无时间戳 / 回合号 / 计数）。
8. **预算按小时计量**（wall-clock），不按轮数。
9. **重试只在「未吐出任何 delta」时发生** —— 绝不重复输出。
10. **重试的判据与可见性都落在适配器内**：判据不上升到 `ys-loop`，痕迹只进 `logging`。
11. **`Retry-After` 不得让回合无界等待**：超过 `max_delay_ms` 即失败。

---

## 5. 测试与变异靶

本项目约定：**关键逻辑必须注入 bug 验证测试能抓到**（多个真 bug 都是「测试全绿时依然存在」）。

### 5.1 `follow_up.rs` 纯函数单测（脱离 `app_loop` 重型测试台）

- `should_review` 真值表：`Completed ∧ rounds≥REVIEW_MIN_ROUNDS ∧ Initial → Some`；
  **`rounds == REVIEW_MIN_ROUNDS - 1 → None`**（短回合不复核 —— 本轮新增的门）；`rounds == 1 → None`；
  `Cancelled / Truncated / MaxRounds → None`；**`Review + Completed + rounds≥REVIEW_MIN_ROUNDS → None`**。
- `REVIEW_PROMPT` **两次产出的字节相等**、不含数字/路径。
- **变异靶**：删掉 `kind != TurnKind::Initial` 早退 ⇒「不问第二次」断言变红；
  **把门槛改回 `2`（或删掉门 ②）⇒「短回合不复核」用例变红**；
  把门 ① 写成「非 `Cancelled` 即可」⇒ `Truncated` / `MaxRounds` 行变红。

### 5.2 `config.rs` / `main.rs`

- `max_rounds` 覆盖生效；缺省取默认 **且 `!= RunLimits::default().max_rounds`**（防恒真）；
  `0 / 负 / 浮点 / 字符串 / null` 各自被丢弃且**不整份回退**（照既有 `test_invalid_entries_are_dropped_individually`）；
  未知字段忽略。
- **`max_turn_duration_secs`（可配预算）**：合法值经 `Config::max_turn_duration()` 取回对应 `Duration`；
  缺省取 `Duration::from_secs(172_800)`（48h）；`0 / 负 / 浮点 / 字符串 / null` 各自被丢弃且**不整份回退**。
- 扩展 `resolve_limits_reads_window_and_policy_from_config` 断言 `limits.max_rounds`，且计时器取 `config.max_turn_duration()`。
- **变异靶**：`resolve_limits` 漏写 `max_rounds` ⇒ 断言变红；核心默认改回 10 之外的值 ⇒ `ys-component` 既有断言变红。

### 5.3 `app_loop` 集成（复用既有 `Harness` + `ProbeModel` + `ProbePlan`，`app_loop.rs:366-693`）

- **工具回合 ⇒ 恰好复核一次**：`ProbePlan::ToolCallThenFinal`。断言 `RunFinished` 两条（`turn` 依次 1、2）、
  转发序列里恰有一条 `UserMessage` 文本 `== REVIEW_PROMPT`、`view.turn_count == 2`。
- **纯文本回合 ⇒ 不复核**（`ProbePlan::UntilMarker` 收尾无工具）。
- **`Cancelled` / `Truncated` / `MaxRounds` ⇒ 不复核**。
- **不问第二次**（核心变异靶）：主回合 + 复核回合**都**是「用工具 + `Completed`」⇒ 断言**恰好两个回合**。
- **跨回合残留 Abort 不取消复核**：回合 1 结束后投一条 `Abort` ⇒ 复核**仍发生**（验证排空前移）。
- **顺序**：复核回合的 `RunFinished` 必须在主回合 `RunFinished` **之后**。
- **`meta.turns` 含复核回合**；**标题不被复核覆盖**（复核回合传 `None`）。
- **回归护栏（必须继续绿）**：`outbound_sequence_is_ordered_with_view_last`（`app_loop.rs:704`）、
  `every_request_variant_is_handled`（`:987`）、`abort_yields_cancelled_and_view_still_emitted`（`:823`）
  —— 它们的回合无工具 ⇒ 复核不触发 ⇒ 出站序列逐字节不变。
- **需要更新（预期副作用，非 bug）**：`steer_is_injected_at_round_boundary`（`:774`）、
  `stale_abort_queued_before_first_turn_is_discarded`（`:1233`）—— 二者构造了「`Completed` + 用过工具」的回合，
  现在会多出一个复核回合；把断言从 `stop_reasons == vec![Completed]` 改为「取**首个** `RunFinished` 为 `Completed`」，
  **保留原意图**。
- **wall-clock 预算（含「极短预算触发」）**：临时 `config.json` 写 `max_turn_duration_secs: 1`，经 `load_user_config(path)` 加载后
  跑 `AlwaysToolCall` 探针回合 ⇒ 以 `Cancelled` 收场 + 恰一条 `Outbound::Output` 说明行（`budget exhausted` 措辞）；
  `max_rounds = u32::MAX` 时不被 10 轮截断。**「极短预算触发」是唯一能测 48h 兜底的途径**（可配预算的验收点）。
- **变异靶**：把排空留在 loop 顶（不移入 `run_turn_once`）⇒「残留 Abort 不取消复核」用例必红。

### 5.4 ③ 适配器（本地回环 TCP mock，照 `apps/coding-agent/tests/stream_usage_fallback.rs:57-120` 的内联 `MockServer` 范式）

- `429`×3 后 `200` ⇒ 恰好重试 3 次、最终 `Ok`（请求计数 == 4）；
- `401` ⇒ **不**重试（计数 == 1）；
- **额度名单**：`429` + body `{"error":{"code":"insufficient_quota"}}` ⇒ **不**重试（计数 == 1）；
- **响应头优先**：`500` + `x-should-retry: false` ⇒ 不重试；`400` + `x-should-retry: true` ⇒ 重试；
- **`Retry-After` 超上限**：`429` + `Retry-After: 3600` ⇒ **立即失败**（断言未睡满 ≤ 1 s 即返回、错误文本含 `3600`）；
- **已吐 delta 后断流** ⇒ **不**重试、上报（计数 == 1）；
- 黑洞（accept 后不应答）⇒ 空闲超时触发 ⇒ 按可重试处理；
- 重试期间**恰好落一行** `retry attempt …` 日志，不重试的路径**零**该行。
- **变异靶**：删掉「已吐 delta 不重试」（去掉计数器）⇒ 重复输出用例变红；删掉额度否决 ⇒ 额度用例变红；
  把 `Retry-After` 换成「取上界」⇒ 超上限用例变红。

### 5.5 `ys-tui-coding`（§3.6 判别式两侧，缺一即退化）

- **复核回合 ⇒ `System` 行 + 点亮**：`!is_turning` 时投一条非回显 `UserMessage` ⇒ `TranscriptLine::System`、
  `is_turning == true`、`turn_started_at.is_some()`。
- **steering ⇒ `User` 行（回归护栏）**：`is_turning == true` 时投一条非回显 `UserMessage` ⇒ 仍是 `TranscriptLine::User`、
  **不改** `is_turning`。
- **变异靶**：把判别式写成「非回显即 `System`」⇒ steering 用例变红。

### 5.6 端到端「48h 形态」组合测试

- 一个回合跑到**远超 10 轮** + 触发**至少一次压缩** + 收尾 ⇒ 断言**无 `MaxRounds` 截断**、最终 `Completed`，
  且复核回合随后发生。这是唯一覆盖「①+②+压缩」交互的用例（既有 e2e / 集成台可复用，`--ignored`）。
- **`-p` / `--json` 预算到点** ⇒ **非零退出**（`apps/coding-agent/tests/` 新增文件）。

---

## 6. 风险与已接受的取舍

| # | 取舍 | 账 |
|---|---|---|
| **R1 细阀换粗阀** | 去掉轮数上限后，回合内**唯一**的防失控阀就是 wall-clock 预算，而它**很粗** —— 小输出空转可持续数小时才到点（`MAX_CONSECUTIVE_ERRORS` 仍是空分支）。 | 明写为交换，不是「无损失」。 |
| **R2 `Cancelled` 三义** | 时长预算到点、用户 Ctrl+C、回合内 Abort 收场**同为 `Cancelled`**，UI 分不出来。 | 靠那条 `Outbound::Output` 说明行区分「人停 / 预算停」；不拆 `StopReason`（不动 `BasicLoop`）。 |
| **R3 退避不可中止** | `Model::complete` 无取消入参 ⇒ **退避睡眠期间 Ctrl+C 无效**。 | 以「总退避 ≤ ~60s + 每次尝试由流空闲超时包住」作界；不可中断时长**有界**。 |
| **R4 重试治抖动不治停服** | 有界退避总等待 ≤ ~60s ⇒ **超过约一分钟的 provider 故障仍会终止回合**。 | 必要的界（无界重试会把「长期不可用」变成「永久占用会话」）；**不得**把 48h 无人值守理解成已能容忍长时间停服。 |
| **R5 无原型校准** | 本环境无 `YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`，**用户已裁定跳过 prototype** ⇒ `REVIEW_MIN_ROUNDS = 5`、`connect 15s` / `idle 120s`、退避参数均为**待真实环境校准的初值**（debt）。 | 记入 as-built 与 ADR，明确列为 debt。 |
| **R6 `-p` 不复核** | `-p` / `--json` 的事件消费者以第一个终局事件退出，复核回合的第二个 `RunFinished` 会撞死信道。 | 明确接受（与 ADR-0024 同一类退化）；`max_rounds` 放开与时长预算**仍对它们生效**。 |
| **R7 `rounds` 计数含截断作废批** | 「截断整批作废后回环」也会使 `rounds` 递增，可能带来门 ② 的轻微假阳性。 | 接受：仍代表够长的回合，轻微。 |
| **R8 适配器回环 TCP 测试** | 本环境难自验真实 provider 行为；回环 mock 是新增测试基础设施。 | 记账（写进测试基础设施前提）。 |
| **R9 会话落盘 O(n) 全量重写** | `crates/ys-session/src/jsonl.rs:128-131` 的 `append` 每次全量重写整个 JSONL ⇒ 48h ≈ 10⁵ 次全文件重写。 | **架构级记账**，另立设计。 |

---

## 7. 文件清单

**新增**：

- `apps/coding-agent/src/follow_up.rs` —— `TurnKind` / `should_review` / `REVIEW_PROMPT`（+ `#[cfg(test)]`）
- `apps/coding-agent/tests/`（新文件）—— `-p` / `--json` 预算到点非零退出
- （适配器可选）`adapters/model-openai-compatible/src/retry.rs` —— 若重试逻辑独立成模块

**改**：

- `apps/coding-agent/src/config.rs` —— `max_rounds` + `max_turn_duration_secs` + `model_retry` + `model_timeout`（Raw / 校验 / 取数口）
- `apps/coding-agent/src/main.rs` —— `mod follow_up;`；`resolve_limits` 加 `max_rounds`；
  `build_agent` 传 `model_retry` / `model_timeout` 给适配器；`run_single_turn` 加合成 `QueueBoundarySource` + 计时器（取 `config.max_turn_duration()`）
- `apps/coding-agent/src/app_loop.rs` —— **先纯重构**（`run_turn_once` 抽出 + 排空前移，既有测试守门）**后循环**（`pending.take()`）+ 计时器分支（取 `config.max_turn_duration()`）+ 可观测性行
- `adapters/model-openai-compatible/src/lib.rs`（±新 `retry.rs`）—— 超时两旋钮 + sink 计数器 + 三级判定 + 额度名单 +
  有界退避 + `Retry-After` 硬上限 + 退避日志
- `crates/ys-tui-coding/src/app.rs` —— `apply_event` 的 `UserMessage` 分支按 `is_turning` 分流
- `docs/arch/long-horizon-completion/design.md` + `docs/adr/0027-*.md` + `docs/adr/0028-*.md` —— 修正核实偏差
- `docs/design-final/coding-agent-tui.md` —— 更新 as-built + 记初值 debt
