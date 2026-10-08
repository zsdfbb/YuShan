# 长程任务的「完成」— 设计方案

> 产出：`/arch-design`（2026-10-08）。上游为同目录 [`context.md`](context.md)（`/arch-explore`）。
> 基线：`master` @ `8af5e03`。所有落点均带 `文件:行号`。
> **评审修订（2026-10-09）**：按 [`review.md`](review.md) 收敛 —— 逐条处置见文末 §9。
>
> **范围已收敛**（用户裁决，见 `context.md` §7）：本轮只做
> **① 放开单回合预算** + **② 完工复核（只问一次）**。
> todo 闭环（ADR-0024）、subagent（ADR-0022）、follow-up 的计数/停滞护栏 —— **一律不做**。
>
> 本文件是**设计**，不是实现；对应决策记录 [`../../adr/0027-*.md`](../../adr/)（编号 0027）。

---

## 1. 需求与已裁决范围

**实证需求的失败**：一次 `/arch-explore` 任务在 `⚠ 10 rounds · 60.9s` 处被切断、**未产出结果**。
`context.md` §1 定位到两个**相互独立**的根因：

| 根因 | 位置 | 现象 |
|---|---|---|
| **R1 预算写死** | `RunLimits.max_rounds = 10`（`crates/ys-component/src/limits.rs:39`），app 侧无覆盖路径（`resolve_limits` 只写 `context_window` / `compaction`，`apps/coding-agent/src/main.rs:361-366`） | 撞硬顶，`final_message: None`，**无收尾机会** |
| **R2 回合边界是死墙** | `app_loop` 的 `Request::Prompt` 臂恰好一个回合，且**不看 `stop_reason`**（`apps/coding-agent/src/app_loop.rs:109-186`） | 撞顶 / 正常完成**走同一条路**：回到 `request_rx.recv().await` 等下一条输入 |

**用户裁决**（原话）：

- ① 「**不设置 MaxRounds，或者可以设置的极大**」
- ② 「**模型自觉完成，不过需要提示模型确认是否完工**——模型认为完工之后，我们再发条消息找他 check」
  以及「**为什么不在 agent 不继续执行后，我们主动问一次模型是否需要继续？**」
- **③（第二轮追加）** 「**需要支持 48 小时的长程任务**」；并就「48h 无人值守的头号缺口」确认
  **把模型调用重试纳入本轮**。

⇒ ② 的形状 = **只问一次**（用户明确反对「循环 + `MAX_CHECKS` + 停滞判据」的复杂设计）。
⇒ **48h 改变了 ① 的性质**（见 §4.2）：轮数上限若大到装得下 48h，它就不再是阀 —— 故阀改用**小时**计量。
⇒ **③ 与已有上下文冲突需对齐**：`docs/arch/llm-api-unavailable/context.md` §范围外当时明确
  「**不做自动重试**」（需求是「报错并终止」）；48h 无人值守要求自愈 ⇒ **本轮 supersede 该条**（见 §4.8、ADR-0028）。

---

## 2. 调研

### 2.1 同类软件：Claude Code 的 Stop hook

Claude Code 的 **Stop hook** 在「主 agent 结束一次回应、即将把控制权交还用户」时触发（**不只是任务完成时**）。
其续跑设计与本需求的 ② **同构**：

- Stop hook 输入里有一个布尔量 **`stop_hook_active`** —— 含义是「当前**已经处于一次强制续跑中**」。
  hook **必须先检查它、为 `true` 就直接放行**，从而保证**至多强制续跑一次**、不死循环。
- 另有「**连续 block 8 次即强制放行**」的上限（可由 `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` 调整）。
- Stop hook **不在用户中断时触发**（API 错误走 `StopFailure`）—— 与「`Cancelled` 绝不续跑」同一条纪律。
- 通用模式（`stop-hook-auto-continue`）：`generate → verify → 命中则续跑`，源自 Reflexion / Self-Refine；
  其公认代价是**失控成本**，因此**必配** `max_iterations` / `max_duration` / `max_cost` 硬限。

> **对本设计的两个直接结论**：
> ① 「只问一次」**不是临时决定，是业界标准护栏形状** —— `stop_hook_active` 就是它的实现；
> ② Claude Code 把续跑做成通用 hook 缝（需要动态插件）—— 但本仓 ADR-0021 **明确否决**了 hook 系统（消费者为 0），
> 故本仓走「**一个具体判据 + 一个纯函数**」，不做通用缝。

### 2.2 同类软件：Pi —— 长程循环 / 完成判定 / 重试与超时

**详细对照与 `文件:行号` 见 [`research.md`](research.md)**（本文只留结论）。三条结论直接影响本设计：

- **轮数**：Pi 的 agent 循环是**无上限**的 `while (true)`（内层「无工具调用即退出」，外层「无 follow-up 即退出」），
  全仓无 `maxRounds` 一类字段 ⇒ 本仓 ① 取 `u32::MAX` 与既有实践一致。**Pi 没有时长兜底**
  （它的兜底是 abort）⇒ 本仓的 wall-clock 预算是**在其之上加一层**，不是照抄。
- **完成**：Pi **不**在模型停手后自动追问；它的 `followUp` / `steering` 两条缝都是**人**排队的消息队列
  （语义分工与本仓 ② 同构：steering = 回合内插话，follow-up = 停后再跑一个完整回合）⇒
  **② 是本仓自有增量**。可佐证的是它的 `overflowRecoveryUsed` 一次性布尔量与 checkpoint 里的
  `continuation` 状态：**「某类续跑至多一次」在这类系统里用一次性标志 + 结构早退表达，不用计数器** ——
  与本仓 `TurnKind` 二态枚举同一形状。
- **重试 / 超时**：Pi 分三层（provider 流开边界 / 整条 assistant 消息 / 持久化 `retry_wait`），
  可重试性判据是「响应头 → 错误文本 → 状态码」，退避 cap 8 s 且**可被 abort**，
  服务端要求的长等待**超过上限即失败**。本设计采纳其中四条（§4.8），
  **明确不采纳**两条（中途断流重试、退避可中止）—— 理由都是本仓自身的约束（零协议变体、核心 trait 冻结）。
- **超时**：Pi 的 HTTP 路径吃 SDK 默认的**单一总超时**（10 分钟），只有 WebSocket 才分 connect / idle。
  本仓 ③ 的「两旋钮」**比它细**，且理由（首字节延迟 ≠ 中途停顿）在本仓成立 ⇒ 保持。

### 2.3 本仓已有的落点：follow-up 设计稿 + ADR-0024

- **`docs/design-plans/2026-09-15-follow-up.md` §3.1 已给出缝的形状**（设计完成、**未实现**）：
  ```rust
  let mut pending = Some(message);
  while let Some(msg) = pending.take() { /* 跑一个回合 */ }
  ```
  并规定「**一个函数一个 seam**：不引 `trait`、不引 registry、不引 `HashMap`。今天不预埋任何抽象。」
- **ADR-0024 决策 3** 明确：该 `while let Some(msg) = pending.take()` 循环是 todo 推进的落点，
  「将来 commit-doc 追问只是**同一循环里的第二个判据**（加法）」。
- ADR-0024 决策 2 的硬约束：注入内容**必须是纯函数、同内容恒同字节**（不得含时间戳 / 回合号 / session id）。

⇒ **② 应当就地兑现这条既有缝**，只换判据；**不另起第二套「回合边界续跑」机制**。

---

## 3. 候选方案与对比

三个 subagent 按不同倾向独立产出，摘要如下。

### 3.1 候选甲（最小复杂度）
`Request::Prompt` 臂内联包 `loop` + `reviewed: bool`；`ToolResult` 是否出现改由 `RunResult.rounds` 计数判定；
不新建文件、不抽函数；`max_rounds` 默认 `u32::MAX`。

### 3.2 候选乙（可扩展优先）
新建 `apps/coding-agent/src/follow_up.rs`：`enum TurnKind { Initial, Review }` + 纯函数
`should_review(kind, &RunResult) -> Option<AgentInput>` + 常量 `REVIEW_PROMPT`；
`app_loop` 抽出 `run_turn_once(...)` 并把回合前的边界排空**移进**该函数；
循环**逐字**采用设计稿的 `while let Some((input, kind)) = pending.take()`。
「只问一次」由 `kind != Initial ⇒ None` 保证。

### 3.3 候选丙（成本 / 资源优先）
新建 `apps/coding-agent/src/completion_check.rs`；`Request::Prompt` 臂**直线式（`if`，非循环）** 跑两个回合，
复核回合的 `used_tools` / `stop_reason` 直接丢弃 —— 「不问第二次」由**控制流形状**保证。
`used_tools` 在**两处事件转发点**扫描 `AgentEvent::ToolCall` 置位。
`max_rounds` 默认取**有限大** `1_000`（见 §4.2 的定量理由），并建议补一条软警告。

### 3.4 对比矩阵

| 维度 | 甲 最小复杂度 | 乙 可扩展优先 | 丙 成本优先 |
|---|---|---|---|
| 实现复杂度 | **最低**（内联，无新文件） | 中（新模块 + 抽函数） | 中（新模块 + 抽函数） |
| 「只问一次」的保证 | `reviewed` 布尔量 | `TurnKind` 二态枚举 | **控制流形状**（直线，无循环） |
| 与既有设计稿 / ADR-0024 的关系 | 另起一套（非 `pending.take()`） | **逐字兑现**（同一循环，加判据） | 另起一套（直线，非循环） |
| 工具活动判定 | `rounds` 计数（零新增状态） | `rounds` 计数（零新增状态） | 扫两处事件转发点（**有漏点风险**） |
| 演进灵活性 | 低（判据写死在内联处） | **高**（未来 commit-doc / todo 是同一循环加判据） | 低（直线形状限制第二个判据） |
| 成本分析 | 无 | 有定性 | **有定量**（失控成本曲线、复核成本闭式） |
| 主要风险 | 与既有缝并行、日后要合并 | 需一次纯重构（`run_turn_once`） | 事件扫描漏点 ⇒ 静默失准 |

---

## 4. 推荐方案：**以乙为骨架，吸收丙的成本结论与甲的判据**

融合三者的**长处**：乙的缝形状（兑现既有设计稿）、甲的 `rounds` 计数判据（零新增状态，门槛见 §4.1）、
丙的定量成本结论（决定 `max_rounds` 默认值）。**零 `BasicLoop` 改动、零 core crate 改动。**

### 4.1 缝的形状

新建 `apps/coding-agent/src/follow_up.rs`，只放三样（**不引 `trait` / registry / 规则列表** —— ADR-0014/0021）：

```rust
/// 二态而非计数器：`Review` 永远不可能再产出续跑 —— 「只问一次」的**全部**实现。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnKind { Initial, Review }

/// 复核下限轮数：`rounds >= 2` ⟺ 本回合调用过工具；取 `5` 是为把**短回合挡在门外**（理由见下）。
/// **这是「复核噪音 ↔ 早收风险」的唯一旋钮**，默认值由 prototype 校准（§7 步骤 0）。
pub(crate) const REVIEW_MIN_ROUNDS: u32 = 5;

/// 纯函数判据：无 I/O、无状态、无计数 —— 可脱离 `app_loop` 的重型测试台单测。
pub(crate) fn should_review(kind: TurnKind, result: &RunResult) -> Option<AgentInput> {
    if kind != TurnKind::Initial                 { return None; } // ←「只问一次」
    if result.stop_reason != StopReason::Completed { return None; } // ← 门 ①：Cancelled/Truncated/MaxRounds 一律不续
    if result.rounds < REVIEW_MIN_ROUNDS         { return None; } // ← 门 ②：本回合没调用过工具
    Some(AgentInput::text(REVIEW_PROMPT))
}

/// 复核文案。**纯常量、确定性字节**（无时间戳 / 回合号 / 计数）。
pub(crate) const REVIEW_PROMPT: &str = "..."; // §4.5
```

**门 ② 为何用 `rounds` 计数**：`BasicLoop` 里唯一能让 `rounds` 递增的路径，是上一轮 `tool_calls` 非空
（截断整批作废后的回环，或工具执行后的回环；`crates/ys-loop/src/basic.rs`）。
故 **`rounds >= 2` ⟺ 本回合模型至少发出过一次 `tool_use`**。
它直接读 `RunResult.rounds`（`crates/ys-loop/src/result.rs:7`，`pub`），**零事件管道、零新增状态** ——
这正是候选丙「扫两处事件转发点」方案的**漏点风险**所规避的（候选乙也独立给出同一结论）。

**但门槛不止 `2`：取 `REVIEW_MIN_ROUNDS = 5`（评审风险 #1 / 建议 #6，本轮定）**：
`rounds >= 2` 只排除「纯文本完工」，而「读一个文件就答」这类**一两批工具**的回合**也满足 `>= 2`** ——
复核会落在**几乎所有 coding 交互**上（成本见 §4.7），而这类短回合即便早收，代价也只是用户再补一句。
⇒ 门槛抬到 `5`（≈ 4 批工具），复核**只落在够长的回合**上：短回合的噪音挡在门外，长回合（早收代价大）仍全程受复核。
**取舍如实**：门槛越高，「跑了两三步就自称完工」越可能漏检 —— `5` 是**待 prototype 校准的初值**，不是终值。

> **为什么 `pending` 是 `Option` 而不是队列**：设计稿本就是 `Option::take()` 语义，同一时刻只可能有一条待跑输入。
> 将来第二、第三个判据要并列时，升级为 `VecDeque` 是**加法**，不是重写。

### 4.2 ① 放开单回合预算：`max_rounds` 可配、默认**不设（`u32::MAX`）**

**落点**（照 `context_window` / `compaction` 的既有「Raw → 校验 → 规整」形状）：

- `apps/coding-agent/src/config.rs`：`RawUserConfig` 增 `#[serde(default)] max_rounds: Option<serde_json::Value>`
  （**顶层标量**，非按 model 的 map —— 预算是回合属性，与 model 无关）；`UserConfig` 增 `max_rounds: Option<u32>`；
  `parse_user_config` 增一支校验（`as_u64()` → `u32::try_from` → `>= 1`；非法**只丢该条并落日志**，
  与既有宽容语义同规）；`Config::max_rounds()` 取数口。
- `apps/coding-agent/src/main.rs` `resolve_limits`（`:361`）加一行 `limits.max_rounds = config.max_rounds();`。
  **唯一接线点**，`build_agent` 已 `.limits(limits)` ⇒ **TUI / `-p` / `--json` 三模式一次修复**。
- `Agent` **不加 setter**：`max_rounds` 不随 model 变，启动定死即可（不像 `context_window` 需每回合刷新）。

**⚠️ 核心默认值 `RunLimits::default().max_rounds` 保持 `10`，不动。** 结构性理由两条：

1. `crates/ys-component/src/lib.rs:18` 有断言 `assert_eq!(limits.max_rounds, 10)`；
2. `app_loop` 既有测试 `abort_yields_cancelled_and_view_still_emitted`（`apps/coding-agent/src/app_loop.rs:847-851`）
   用 `ProbePlan::AlwaysToolCall` 靠**核心默认 10** 在可接受轮数内撞 `MaxRounds`；改成极大值会让它空转。
   ⇒ **「库的保守默认」与「app 的产品默认」分离**，是既有的分层，不该被本设计破坏。

**`DEFAULT_MAX_ROUNDS` 取多少？—— 需求是 48h，故答案变了。**

48 小时 = 172,800 s。按实测样本（`⚠ 10 rounds · 60.9s` ⇒ ≈6.1 s/轮，读密集型任务）：

| 每轮耗时 | 48h 能跑的轮数 |
|---|---:|
| 6 s | ~28,800 |
| 10 s | ~17,300 |
| 30 s（bash 偏重） | ~5,800 |
| 60 s | ~2,900 |

⇒ 48h 长任务的真实预期落在 **3k–29k 轮**。**1,000 与 10,000 都不够**（10,000 在 6 s/轮下只有约 17 小时）。

**关键结论：为 48h 配的任何轮数上限都已大到不再是一个阀** —— 且它把「48 小时」悄悄绑死在「每轮多久」上
（模型/工具不同，每轮耗时差 3 倍以上）。**需求用小时陈述，阀就该用小时计量。**

⇒ **`DEFAULT_MAX_ROUNDS = u32::MAX`（即「不设」，回到用户原话），真正的兜底改为 wall-clock 预算（§4.2.1）。**

> 成本如实：按稳态 ≈10k token-当量/轮，48h ≈ 29k 轮 ≈ **~300M token-当量**（不含周期性压缩，每次 ~87k）。
> 这是 48h 需求的**正常开销**，不是意外 —— 所以时长/成本必须**可观测、可配**。

#### 4.2.1 wall-clock 预算（`max_turn_duration`，默认 48h）—— 零 `BasicLoop` 改动

- **落点**：`app_loop` 的 `run_turn_once` 的 `select!` 加一条 `tokio::time::sleep_until(deadline)` 分支，
  到点向当前回合的边界源 `push(Boundary::Abort)`，并在**同一处**先发一条 `Outbound::Output`
  说明行（`budget exhausted: …`）。`deadline` 在 `Request::Prompt` **入口**算一次，
  覆盖整个 `pending.take()` 循环（含复核回合）—— 即「一次用户请求 = 一段 48h 预算」。
- **为什么能零 `BasicLoop` 改动**：`Boundary::Abort` 是既有机制，`BasicLoop` 在轮边界读取它（**已实现**）。
- **两条代价，如实**：
  1. 这样收场时 `StopReason` 只能是 **`Cancelled`**，与用户 Ctrl+C **同态** —— UI 分不出来，
     靠上面那条 `Outbound::Output` 行区分。要真正区分需新增 `StopReason`（改 `BasicLoop`，本轮**不做**）。
  2. `Abort` **只在轮边界被读**（见 `llm-api-unavailable/context.md` §关键数据流的挂起路径）——
     若到点时模型正卡在一次 `complete()` 里，真正的停止会**推迟到下一次轮边界**（最坏再多一轮，含单工具 300s 超时）。
- **`-p` / `--json`**：这两条路径的 `run_single_turn` **不传 `boundary`**（`main.rs:717`）。
  要同样兜底，需给它一条**合成边界源** + 同一个计时器分支（同一处改动，属加法）；
  本轮**建议一并做**，否则 `-p` 的 48h 任务没有任何时长上限。
- **数值**：默认 `48h`（172,800 s），可经 `config.json` 调；它**只是 backstop** ——
  正常任务在到点前早就由模型自己收尾，只有失控/挂起才会撞上它。
- **`-p` / `--json` 的退出码（本轮定）**：预算到点 ⇒ 与「用户中止」同路的**非零退出**（`Cancelled` 语义），
  原因由那条 `Outbound::Output` 行说明；**不得以 `0` 退出** —— 无人值守的调度器 / CI 必须能看出「没跑完」。
- **这是一次「细阀换粗阀」（如实记账）**：去掉轮数上限后，回合内**唯一的**防失控阀就是这条 wall-clock 预算，
  而它**很粗** —— 小输出空转可持续数小时才到点（`MAX_CONSECUTIVE_ERRORS` 仍是空分支，见 §6）。
  不是「无损失」，是「用一条小时级阀门换掉了轮数级阀门」。

### 4.3 ② 完工复核：`app_loop` 的回合循环

**(a) 抽出 `run_turn_once(...)`**（候选乙 / 丙一致建议的纯重构）：把现在 `app_loop.rs:130-163` 的
「`begin_turn` → 新建 `QueueBoundarySource` → `select!` 三臂 → 回合末 `try_recv` 排空」抽成一个私有 async fn。
**15+ 条既有 app_loop 测试就是这次纯重构的守门员**（行为须逐字节不变），故风险低。

**(b) 回合前的边界排空移进 `run_turn_once`**：现在 `app_loop.rs:100` 的
`while boundary_rx.try_recv().is_ok() {}` 只在**首个回合前**执行一次。复核回合也必须排空 ——
否则上一回合残留的 `Boundary::Abort` 会泵进复核回合把它取消。这正是模块注释 ③（`app_loop.rs:43-44`：
「`Abort` **永久**置位」）要守的不变量：**新建源只防标志位泄漏，防不了队列里那条消息**。

**(c) `Request::Prompt` 臂的新形状**（逐字采用设计稿的 `pending.take()` 循环）：

```rust
let mut pending = Some((AgentInput::new(message), TurnKind::Initial));
while let Some((input, kind)) = pending.take() {
    turns += 1;
    // 标题素材只取首个回合的用户原文；复核回合传 None（绝不把复核常量写成会话标题）
    let raw = (kind == TurnKind::Initial).then_some(raw_user_text.as_deref()).flatten();
    let outcome = run_turn_once(&mut agent, &mut wiring, /* … */ turns, input).await?;
    match outcome {
        Ok(result) => {
            stats.record(&result.usage);
            finalize_turn_meta(/* … raw … */).await; // 失败落日志、不中断（既有）
            pending = should_review(kind, &result).map(|next| (next, TurnKind::Review));
        }
        Err(e) => logging::log(&format!("turn {turns} failed: {e}")), // BasicLoop 已发 RunFailed ⇒ 不续跑
    }
}
```

**「不问第二次」由两条结构保证**：`should_review` 的首个早退（`kind != Initial`），且 `pending` 只在
`Initial` 时会填。**没有计数上限、没有停滞判据、没有抢占、没有逃生开关** —— 与用户裁决一致。

### 4.4 数据流（一次带复核的 `Request::Prompt`）

```
Request::Prompt(msg)
├─ raw_user_text = sessions::user_text(msg)          // 标题素材（skill 展开前，C3，既有）
├─ msg = expand_skill_invocation(msg, &config.skills) // 既有
└─ pending = Some((AgentInput::new(msg), TurnKind::Initial))
   while let Some((input, kind)) = pending.take():
     ├─ turns += 1                                    // 主回合 = 1，复核回合 = 2
     ├─ run_turn_once(turns, input):
     │    ├─ 排空 boundary_rx（丢弃跨回合残留 Abort）   ← (§4.3b 新增落点)
     │    ├─ agent.set_context_window(config.context_window())   // 既有：唯一调用点
     │    ├─ ports.events.begin_turn(turns)           // ⇒ Envelope.turn = turns
     │    ├─ boundary = QueueBoundarySource::new()     // 每回合新建（comment ③）
     │    ├─ select! { turn | sink_rx→Outbound::Event | boundary_rx→push }
     │    └─ while sink_rx.try_recv() → 转发           // 回合末排空（既有 :161-163）
     ├─ Ok(result): stats.record(usage); finalize_turn_meta(raw)
     │    └─ pending = should_review(kind, &result)（仅 Initial + Completed + rounds≥REVIEW_MIN_ROUNDS ⇒ Some）
     └─ Err: 落日志，pending 保持 None ⇒ 链终止
   // 外层仍按 request 发一次 View 快照
```

**事件侧可见序列（两个回合）**：`UserMessage(原文)` → `ToolCall…` → `RunFinished{Completed, turn=1}`
→ `UserMessage(复核文案)` → （可能的工具）→ `RunFinished{Completed, turn=2}`。

**既有步骤的去向**：`turns += 1`、`set_context_window`、`begin_turn`、每回合新建 `boundary`、事件排空、
`stats.record`、`finalize_turn_meta` —— **全部保留在原层次**，只是现在位于循环体内。

### 4.5 复核消息的形状

```rust
/// 完工复核文案。**纯常量、确定性字节** —— 同内容恒同字节，不得含时间戳 / 回合号 / 计数。
pub(crate) const REVIEW_PROMPT: &str = "Before we wrap up: verify that the original request is \
    actually complete. Compare what was asked against what has been done. If anything is missing, \
    unfinished, or unverified, keep working and finish it now using the available tools. If it is \
    genuinely complete, reply with a short confirmation and stop.";
```

- **语言英文**：与 system prompt（`apps/coding-agent/src/prompt.rs` 全英文：`You are a coding agent…`）一致。
- **角色 `Role::User`，经 `AgentInput::text(REVIEW_PROMPT)` 进入会话**：
  `run_turn` 的 Step 2 强制 `session.append(input.message)` 并 emit `AgentEvent::UserMessage` ⇒
  复核与普通回合**同一条路径**，且复核消息在 transcript 里**可见**（需求要的就是「主动发一条消息」）。
  （模型侧是 `User` 角色；**TUI 侧渲染成 `System` 行**，见 §4.6.1 —— 二者不矛盾：角色归模型，呈现归 UI。）
  - **不用 `Boundary::Steer`**：那是**回合之内**轮边界的插话（不增回合数），且上一回合的边界源已作废 —— 语义错位。
  - **不直接 `session.append`**：`run_turn` 仍要一条 `AgentInput`，直写会与 Step 2 重复 append（且跳过 `UserMessage` 事件）。
- **「纯常量」与缓存是什么关系（说准）**：**常量性本身不省钱** —— 缓存是字节前缀，任何新的尾部段都按 1.0× 付一次。
  常量买到的是**防护性**的四点：① 测试可逐字节 fixture、不 flaky；② **永不需就地改写历史**（中段改写才会废掉其后全部缓存）
  ⇒ 复核结构上不可能成为前缀失效源；③ 断掉「为了让消息正确而去改 `system`」这条唯一禁忌路径；④ 复核成本有固定闭式。
  **真正承重的硬规则是**：**只在 `messages` 尾部追加，绝不碰 `system` / `tools`**（ADR-0021 决策 1）。

### 4.6 边界与异常

| 场景 | 行为 | 机制 |
|---|---|---|
| **`-p` / `--json`** | **不复核**（仍单回合） | 走 `run_single_turn`，不经 `app_loop`。且其事件消费者 `consume_events` **以第一个终局事件为终止条件** —— 复核回合的第二个 `RunFinished` 会撞上「消费者已退出」的 `ChannelSink`。**明确接受**（与 ADR-0024 同一类退化）。**但 `max_rounds` 放开对它们生效**（共用 `resolve_limits`）⇒ 「10 轮被切断」不再发生 |
| 首回合 `Cancelled` / `Truncated` / `MaxRounds` | 不复核 | 门 ①（`stop_reason == Completed`） |
| 首回合短于复核下限（`rounds < REVIEW_MIN_ROUNDS`） | 不复核 | 门 ②（§4.1）—— 含零工具的纯文本回合 |
| 首回合 `run_turn` 返回 `Err` | 不复核、不写复核 | `Err` 臂只落日志 |
| 复核回合中 Ctrl+C | 复核回合 `Cancelled` 收尾，链终止 | 复核回合**自己的**边界源收到 `Abort`；`Review` 恒不续 |
| 复核回合撞 `MaxRounds` | 复核回合 `MaxRounds` 收尾，链终止 | 安全阀仍有效；`MaxRounds` 不过门 |
| **复核回合又调工具后停** | **不再问第二次** | `kind == Review` ⇒ `should_review` 首个早退 |
| `max_rounds` 配成非法值 | 忽略该条、回退默认 | `parse_user_config` 的宽容校验（落日志，不 panic） |
| 运行中改 `config.json` 的 `max_rounds` | **不即时生效**（下个进程才生效） | 只在启动装配时读；有意边界 |

#### 4.6.1 完工复核的 UI 呈现（本轮修，**零协议变体**）

评审实测确认：TUI 侧**没有回合开始的信号** —— `is_turning` 只由 `begin_local_turn` 置位
（`crates/ys-tui-coding/src/app.rs:194-199`），而 app 侧的 `begin_turn` 是 sink 行为、**不发事件**（`apps/coding-agent/src/channel.rs`）；
且**非回显 `UserMessage` 已被 TUI 占用**为「轮边界 steering 插话」并渲染成**用户行**（`app.rs:97-100`）。
⇒ 「非回显 ⇒ 是复核」这条捷径**不成立**（steering 也是非回显）。

**判别式（零协议）**：用**非回显 `UserMessage` 到达时 `is_turning` 的真假**区分二者 ——

| 来源 | 到达时机 | `is_turning` |
|---|---|---|
| 轮边界 steering 插话 | 回合**进行中**（输入侧按 `is_turning` 分流，`events.rs:91,108`） | `true` |
| app 发起的复核回合 | 主回合 `RunFinished` **之后**（`finish_turn` 已复位） | `false` |

**改法**（`app.rs` 的 `apply_event`，非回显 `UserMessage` 分支）：

```rust
// 未本地回显的 UserMessage（上面 suppress 分支已排除本地提交那条）：
if self.is_turning {
    // 轮边界 steering 插话（既有行为）
    self.transcript.push(TranscriptLine::User(message_text(&message)));
} else {
    // app 发起的回合（完工复核）：主回合已 RunFinished ⇒ 不在 turn 态
    self.transcript.push(TranscriptLine::System(message_text(&message)));
    self.is_turning = true;                        // Working 指示器亮起
    self.turn_started_at = Some(Instant::now());   // 复核回合的 elapsed 从此刻起算
}
self.follow = true;
```

- **渲染成 `System` 行**而非用户行：复核是**系统**发起的提示，用户在 transcript 里不该看到一条自己没打过的「用户消息」。
- **不改 `Outbound` / `AgentEvent` / `ys-protocol`** ⇒ §5.1 不变量 #1 的「零协议变体」仍成立（改的只是 `ys-tui-coding` 的**渲染分支**）。
- **代价 / 依赖**：判别式依赖一条**隐式不变量** ——「app 发起的回合总在主回合 `RunFinished` 之后」。
  事件在同一条信道内**有序**（`RunFinished` 必先于复核的 `UserMessage` 被处理），故成立；用一条 TUI 测试钉住（§5.2）。

### 4.7 成本

- **复核回合本身**：`ΔC ≈ h·(F + S) + 1.0·L + output ≈ 0.1×(9.6k + S) + ~80 + ~150`。
  缓存命中下它比一个普通工具轮**还便宜**（尾部不追加工具结果，只追加一段常量）。
  取 `S = 50k` ⇒ ≈ **6.2k 当量**；被它拦下的「假完工」是一个长回合白做（≥100k 当量）⇒ 只要**≥6%** 的工具回合有早收风险，期望收益为正。
- **成本偏斜有利**：复核是「按回合收一个固定轮」的税 —— 对短回合 +50~60%，对长回合仅 +5%，**在最该守的贵回合上相对最便宜**。
- **每回合 +1 轮的税（评审风险 #1）**：门 ② 若只取 `rounds >= 2`，则上述 +50~60% 会**落在几乎每一次 coding 交互上**
  （读一个文件、跑一条命令都算）——「单次复核便宜」≠「总开销低」。§4.1 把门槛抬到 `REVIEW_MIN_ROUNDS = 5`
  ⇒ 短回合**不再被追问**，税只落在够长的回合上（那里相对占比仅 +5%）。**门槛正是消掉这个乘法的旋钮。**
- **附带成本（如实记账）**：复核回合是会话**最大**的时刻，最易撞压缩触发点。若主回合已顶到触发线，
  复核轮 1 的 `+L` 可能越线 ⇒ 触发一次 ~87k 的压缩；对「本来还会继续说下一句」的用户净中性。
- **失控上界**：轮数上限已取消（§4.2）⇒ 成本上界改由 §4.2.1 的 **wall-clock 预算**兜底
  （上界 ≈ 预算时长 ÷ 每轮耗时 × 稳态 ≈ 10–11k 当量/轮）—— 这是**小时级**的粗界，见 §4.2.1 的「细阀换粗阀」。
- **背压**：两回合**顺序**执行 ⇒ 内存峰值不叠加；唯一的新风险是**顺序**（主回合事件必须先排空再跑复核回合）——`run_turn_once` 末尾的排空天然满足。

### 4.8 ③ 模型调用重试 + 超时（48h 无人值守的自愈能力）

**为什么必须成对**：没有超时，第一次请求在「接受连接但不应答」的黑洞端点上**永久挂起**
（`llm-api-unavailable/context.md` §现状实测矩阵）—— 根本没有「失败」可供重试。故 **③ = 超时 + 有界重试/退避**一起做。

**落点：适配器（`adapters/model-openai-compatible`）**，三条理由：

1. **只有适配器知道** HTTP 状态码与连接错误 —— 而重试判据正是这些。`ModelError::Provider(String)`
   （`crates/ys-model/src/error.rs:8`）在 `ys-model` 里**没有结构**，上层无法判定「可否重试」。
2. `llm-api-unavailable/context.md` §约束已定死：`ys-model` / `ys-core` / `ys-event` / `ys-protocol`
   **不得引入 tokio** ⇒ 超时只能落适配器或 `ys-loop`；而 `ys-loop` 是**最高爆炸半径**。
3. **零 `Model` trait 改动、零 `ModelError` 改动、零 `BasicLoop` 改动**
   （`ModelError` 非 `#[non_exhaustive]`，加变体是 breaking —— 见该文 §约束）。

**三条硬规则**：

1. **只在「尚未吐出任何 delta」时重试**。适配器包一层 sink 计数器；一旦有 `TextDelta` / `ThinkingDelta`
   发出，任何后续失败**直接上报、不重试** —— 否则重试会把已显示的文本**再吐一遍**
   （`-p` 把增量直写 stdout，**无法撤回**）。
   **计数器只看这两个事件**：工具参数增量**不冒泡**（`ModelEvent` 只有 Text / Thinking 两态，
   工具参数在适配器内部组装）⇒ 它不构成「已显示」。
2. **可重试判据分三级，从左到右短路**（适配器侧）：
   ① **响应头定论** —— 存在 `x-should-retry` ⇒ 以其为准，不看状态码；
   ② **额度否决** —— 错误体文本命中**额度名单**（`insufficient_quota` / `quota exceeded` / `out of budget` /
      `billing` / `usage limit` / `available balance`）⇒ **不重试，即便状态码是 `429`**。
      理由：`429` 语义二义（瞬时限流 vs 账户额度耗尽），后者重试一万次也没用，**立即失败**让消息更快到达人眼前；
      名单是**开放常量**，扩词是加法，不得演变成规则引擎；
   ③ **状态码** —— 连接 / DNS / TLS / 超时错误、`429`、`500/502/503/504` ⇒ **可重试**；
      `400/401/403/404/413/422`、serde 解析失败、sink 错误 ⇒ **不重试**（确定性失败）。
3. **有界退避**：指数退避 + 抖动，次数与上限可配（建议 base `500ms`、×2、cap `30s`、最大 5 次 ⇒ 总等待 ≤ ~60s）；
   带 `Retry-After` / `Retry-After-ms` 时**尊重它，但以 `max_delay_ms` 为硬上限** ——
   **服务端要求的等待超过上限 ⇒ 放弃重试、立即上报**（消息带上服务端要求的秒数）。
   **不取「与上界取 max」**：那等于让服务端**无界地**把回合挂住，与「必须有界」自相矛盾。

**重试可见（本轮定）**：进入退避前经既有 `logging` 落一行 `retry attempt k/n delay=…ms reason=…`（§4.9）。
48h 无人值守下，「静默重试 50 次」与「一次都没重试」必须可区分。
**落点是 `logging` 不是事件** —— 重试**对排障可见、对 TUI 不可见**（有意取舍，护住零协议变体）。

**退避不可中止（如实记账）**：`Model::complete`（`crates/ys-model/src/trait_def.rs:12`）**没有取消入参**，
`Boundary::Abort` 只在轮边界被读 ⇒ **退避睡眠期间 Ctrl+C 无效**。传入取消源要改核心 trait（违反零破坏面），
本轮不做；可行的界是「总退避 ≤ ~60s + 每次尝试由流空闲超时包住」⇒ 不可中断时长**有界**。

**超时用两个旋钮，不用单一总超时**（总超时会误杀长思考模型 —— `deepseek-reasoner` 首字节可达数十秒，
见该文 §约束「性能」）：

- `connect_timeout`（连接建立，建议 ~15 s）；
- **流空闲超时**（建议 ~120 s 无字节）—— 首字节延迟与中途停顿语义不同，分开设。

两者都在适配器内实现（该 crate 有 tokio）：`reqwest` builder / `RequestBuilder::timeout` + `tokio::time::timeout` 包住字节流。

**与 `llm-api-unavailable` 的分工（重要）**：本设计**只补「超时 + 重试」**；那份上下文里的另外两个缺口 ——
**200 + 非 SSE 错误体被判成功**、**流干净结束但无 `[DONE]` 被判完整** —— 属「**完整性判定**」，
**仍归 `llm-api-unavailable` 的设计，本轮不做**（否则两套设计会缠在一起）。
**如实记账**：这两个缺口在 48h 下**比缺重试更危险**（静默把半截/空响应当 `Completed`，等于**静默丢工作**），
应尽快单独设计。

**配置**：`config.json` 增 `model_retry`（`max_attempts` / `base_delay_ms` / `max_delay_ms`）与
`model_timeout`（`connect_secs` / `idle_secs`）两段，经 `build_agent` 传入适配器构造。
`max_delay_ms` **兼作 `Retry-After` 的硬上限**（规则 3）。

**测试**（本地回环 TCP 服务器；做法照该文 §约束：`.no_proxy()` 直连、并行安全端口）：
- `429`×3 后 `200` ⇒ 恰好重试 3 次、最终 `Ok`（请求计数 == 4）；
- `401` ⇒ **不**重试（请求计数 == 1）；
- **额度名单**：`429` + body `{"error":{"code":"insufficient_quota"}}` ⇒ **不**重试（计数 == 1）；
- **响应头优先**：`500` + `x-should-retry: false` ⇒ 不重试；`400` + `x-should-retry: true` ⇒ 重试（各计数 1 / 2）；
- **`Retry-After` 超上限**：`429` + `Retry-After: 3600` ⇒ **立即失败**（断言未睡满 ≤ 1 s 即返回、错误文本含 `3600`）；
- **已吐 delta 后断流** ⇒ **不**重试、上报（请求计数 == 1）；
- 黑洞（accept 后不应答）⇒ 空闲超时触发 ⇒ 按可重试处理；
- 重试期间**恰好落一行** `retry attempt …` 日志（§4.9），不重试的路径**零**该行；
- **变异靶**：删掉「已吐 delta 不重试」⇒ 重复输出用例变红；删掉额度否决 ⇒ 额度用例变红（计数从 1 变 2）；
  把 `Retry-After` 换成「取上界」⇒ 超上限用例变红（会睡满而不是立即返回）。

### 4.9 可观测性（48h 无人值守的必要面，评审风险 #5 / 建议 #2 —— 从 §6「不做」提升为本轮做）

48h 一次 ≈ **300M token-当量**（§4.2），且无人看着 ⇒ 「花了多少、还剩多少」必须可见。**数据源已在手，全是加法、零协议：**

- **每回合可见性已有**：TUI 的 `RunFinished` ⇒ `TranscriptLine::Summary { rounds, elapsed_secs, .. }`（`app.rs:136-147`）
  已经逐回合给出轮数与耗时 —— 无需新增。
- **新增（低成本）**：在每个回合边界（`run_turn_once` 结束处）经既有 `logging` 落一行
  `turn=N rounds=R elapsed=Es cumulative_rounds=CR budget_remaining=Bs`；headless / `-p` 下这是**唯一**的进度面。
- **预算到点**：§4.2.1 的那条 `Outbound::Output` 行**必须**存在 —— 它同时是「人停 vs 预算停」的唯一区分（§4.6 代价 ①）。
- **重试可见（§4.8）**：适配器进入退避前落一行 `retry attempt k/n delay=…ms reason=…`。
  48h 无人值守下这是**唯一**能区分「provider 一直在抖（该换端点）」与「链路健康」的痕迹；
  它**不进事件流** ⇒ TUI 里看不到重试（有意取舍）。
- **不做**：不往 `Envelope` / `AgentEvent` 加遥测字段（护住「事件只观察」的边界与零协议变体）；
  不做实时 TUI 预算进度条（超范围）。

---

## 5. 不变量与测试

### 5.1 不变量

1. **零 `BasicLoop` 改动、零 core crate 改动、零协议变体** —— ①②落 `apps/coding-agent`，③落适配器，
   **零 `Model` trait / `ModelError` 改动**；UI 修复（§4.6.1）只改 `ys-tui-coding` 的**渲染分支** ⇒ `ys-protocol` 一字节不改。
2. **`system` / `tools` 一字节不改**；复核只走 `messages` 尾部。
3. **每用户输入至多多出一个复核回合**（结构保证，非计数）。
4. **`Abort` 不跨回合泄漏**（队列残留也在每回合开头被排空）。
5. **复核文案字节确定**（无时间戳 / 回合号 / 计数）。
6. **预算按小时计量**（wall-clock），不按轮数（§4.2.1）。
7. **重试只在「未吐出任何 delta」时发生**（§4.8 规则 1）—— 绝不重复输出。
8. **重试的判据与可见性都落在适配器内**（§4.8）：判据不上升到 `ys-loop`，痕迹只进 `logging` ⇒ 零协议变体、零核心 trait 改动。
9. **`Retry-After` 不得让回合无界等待**（§4.8 规则 3）：超过 `max_delay_ms` 即失败。

### 5.2 测试（含变异测试）

**`follow_up.rs` 纯函数单测**（脱离 `app_loop` 重型测试台）
- `should_review` 真值表：`Completed ∧ rounds≥REVIEW_MIN_ROUNDS ∧ Initial → Some`；
  **`rounds == REVIEW_MIN_ROUNDS - 1 → None`（短回合不复核 —— 本轮新增的门）**；`rounds == 1 → None`；
  `Cancelled/Truncated/MaxRounds → None`；**`Review + Completed + rounds≥REVIEW_MIN_ROUNDS → None`**。
- `REVIEW_PROMPT` 两次产出的字节相等、不含数字/路径。
- **变异靶**：删掉 `kind != Initial` 早退 ⇒ 「不问第二次」断言变红；**把门槛改回 `2`（或删掉该门）⇒「短回合不复核」用例变红**；
  把门 ① 写成「非 `Cancelled` 即可」⇒ `Truncated`/`MaxRounds` 行变红。

**`config.rs` / `main.rs`**
- `max_rounds` 覆盖生效；缺省取 `DEFAULT_MAX_ROUNDS` **且 `!= RunLimits::default().max_rounds`**（防恒真）；
  `0 / 负 / 浮点 / 字符串 / null` 各自被丢弃且**不整份回退**；未知字段忽略。
- 扩展 `resolve_limits_reads_window_and_policy_from_config` 断言 `limits.max_rounds`。
- **变异靶**：`resolve_limits` 漏写 `max_rounds`（回归到「无覆盖路径」）⇒ 断言变红；核心默认改回 10 之外的值 ⇒ `ys-component` 既有断言变红。

**`app_loop` 集成（复用既有 `Harness`）**
- **工具回合 ⇒ 恰好复核一次**：`ProbeModel` 先回不存在的工具调用再收尾。断言：`RunFinished` 两条（`turn` 依次 1、2）、
  转发序列里恰有一条 `UserMessage` 文本 `== REVIEW_PROMPT`、`view.turn_count == 2`。
- **纯文本回合 ⇒ 不复核**；**`Cancelled` / `Truncated` / `MaxRounds` ⇒ 不复核**。
- **不问第二次**（核心变异靶）：主回合 + 复核回合**都**是「用工具 + `Completed`」⇒ 断言**恰好两个回合**。
- **跨回合残留 Abort 不取消复核**：回合 1 结束后投一条 `Abort` ⇒ 复核**仍发生**（验证 §4.3b 排空前移）。
- **顺序**：复核回合的 `RunFinished` 必须在主回合 `RunFinished` **之后**（验证不混序）。
- **回归护栏（必须继续绿）**：`outbound_sequence_is_ordered_with_view_last`、`every_request_variant_is_handled`、
  `abort_yields_cancelled_and_view_still_emitted` —— 它们的回合无工具 ⇒ 复核不触发 ⇒ 出站序列逐字节不变。
- **需要更新（预期副作用，非 bug）**：`steer_is_injected_at_round_boundary`、`stale_abort_queued_before_first_turn_is_discarded`
  —— 二者构造了「`Completed` + 用过工具」的回合，现在会多出一个复核回合；把断言从
  `stop_reasons == vec![Completed]` 改为「取**首个** `RunFinished` 为 `Completed`」（或显式期望两段），**保留原意图**。

**`ys-tui-coding` 测试（§4.6.1 的判别式两侧，缺一即退化）**
- **复核回合 ⇒ `System` 行 + 点亮**：`!is_turning` 时投一条非回显 `UserMessage` ⇒ 出 `TranscriptLine::System`、
  `is_turning == true`、`turn_started_at.is_some()`。
- **steering ⇒ `User` 行（回归护栏）**：`is_turning == true` 时投一条非回显 `UserMessage` ⇒ 仍是 `TranscriptLine::User`、**不改** `is_turning`。
- **变异靶**：把判别式写成「非回显即 `System`」⇒ steering 用例变红（这正是评审指出的「不能靠非回显判定」）。

**端到端「48h 形态」组合测试（评审 可维护性 #4，缺此则组合 bug 无网）**
- 一个回合跑到**远超 10 轮** + 触发**至少一次压缩** + 收尾 ⇒ 断言**无 `MaxRounds` 截断**、最终 `Completed`，
  且复核回合随后发生。既有 e2e / 集成台可复用（`--ignored`）。这是唯一覆盖「①+②+压缩」交互的用例。

---

## 6. 明确不做 / 记账

- **不做 todo 闭环**（ADR-0024 的注入 + 按「有未完成项」推进）、**不做 subagent**、**不做 git HEAD 追问**（设计稿）——
  与本轮裁决无关；将来都是往**同一个 `pending.take()` 循环**里加判据。
- **不做计数上限 / 停滞护栏 / 用户输入抢占** —— 复核结构上至多一次，用不上（用户明确反对）。
- **不修 `MAX_CONSECUTIVE_ERRORS` 的空分支**（`basic.rs:514-520`）—— 在 `BasicLoop` 内，最高爆炸半径。
  **如实记账**：它是**已知缺口**，「模型原地打转」本轮只靠 `max_rounds` 有限阀兜底。
- **不做会话落盘的 I/O 改造**（评审风险 #3，**架构级记账**）：`crates/ys-session/src/jsonl.rs:128-131` 的 `append`
  **每次全量重写整个 JSONL**（tmp + rename）⇒ 48h（~29k 轮 × 2–3 条消息）≈ **10⁵ 次全文件重写**。
  压缩把文件大小压在 MB 级，但**总 I/O 量与耐用性（无 fsync，断电可能丢最后若干条）未评估**；
  正解是「追加 + 周期性压实」，**另立设计**。
- **不给 `-p` / `--json` 加复核**（消费者以首个终局事件退出，会撞死信道）；但 §4.2.1 的时长预算**要加**（否则这两条路径无时长上限）。
- **不引 hook 系统 / `trait` / registry / `Vec<Box<dyn _>>`**（ADR-0014/0021）。
- **不修 `llm-api-unavailable` 的两个完整性缺口**（200 + 非 SSE 错误体、无 `[DONE]` 的半截流）—— 属「完整性判定」，
  仍归那份上下文的设计；**记账**：48h 下它比缺重试更危险（静默丢工作）。
- **不新增 `StopReason` / `ModelError` 变体**（前者改 `BasicLoop`、后者 breaking）—— 时长预算收场复用 `Cancelled`（§4.6 代价 ①）。
- **不引「中途断流后重试」**（§4.8 规则 1 保持硬规则）：它需要一条「本次尝试作废」的渲染信号
  （`ModelEvent` / `AgentEvent` **都是 `#[non_exhaustive]`**，加变体在 Rust 层不破坏下游，但**违反不变量 #1**），
  且 `-p` 的增量已直写 stdout、**无法撤回** ⇒ 两条路径行为分叉。
  更关键：**「吐过 delta 后断流」本质是完整性判定**（流是否完整结束），归 `llm-api-unavailable` 的正主 ⇒ 记账。
- **不把退避做成可中止**（§4.8 末条）：需要 `Model::complete` 收取消源 ⇒ 改核心 trait。记账，并以「有界退避 + 流空闲超时」作界。
- **不把溢出错误路由到压缩**：需 `ModelError` 结构化（breaking）；且本仓压缩由 token 估算驱动（ADR-0017）。
  `400` 已在「不重试」表内，语义已对。
- **不引工具级 `terminate`**（工具投票决定回合收尾）：属「可验证验收」的另一种形态，与本轮裁决冲突（research.md §5.2）。
- **不凭估算调死默认值**：48h 的轮数/耗时是**估算**（单一实测样本）；实施中先 prototype 再定值（§7 步骤 0、§8）。

---

## 7. 落地顺序

0. **prototype（前置，评审风险 #4）**：用现有 e2e 复跑一个**工具偏重**的长任务，量出**真实 s/轮**与**压缩触发频率**，
   校准 §4.2 的轮数估算、`REVIEW_MIN_ROUNDS` 的合理性、③ 的两个超时阈值。**先量后定值。**
1. **纯重构**：抽 `run_turn_once` + 边界排空移入（既有 15+ 测试守门，行为逐字节不变）。
2. **① `max_rounds`**：`config.rs` → `resolve_limits` → 测试。
3. **③ 超时 + 重试**（适配器）：先超时、后重试（无超时则重试无从触发）；
   重试内再分三步 —— **三级判据**（头 / 额度名单 / 状态码）→ **有界退避 + `Retry-After` 硬上限** → **`logging` 一行**；
   本地回环 TCP 测试（§4.8）。
4. **② `follow_up.rs`** + `app_loop` 循环 → 更新两条既有测试 → 新增复核测试。
5. **§4.6.1 UI 修复**（`ys-tui-coding` 渲染分支）→ 判别式两侧测试。
6. **wall-clock 预算**（`app_loop` 计时器 + `-p` 合成边界源 + 退出码）+ **§4.9 可观测性**一行日志 → 测试。
7. **文档**：ADR-0027 / ADR-0028 + `context.md` 收敛记录 + 本轮评审修订 + 同类软件调研（`research.md`）。
8. 全量 `cargo test --workspace` + `cargo clippy --all-targets`。

---

## 8. 未决点与已裁决（评审后收敛）

**已裁决（本轮定，评审建议 #6/#7 收敛）**：

- **`DEFAULT_MAX_ROUNDS = u32::MAX`（不设）**；兜底改 wall-clock 预算，默认 **48h**、可配（§4.2 / §4.2.1）。
- **`-p` / `--json` 一并加 wall-clock 预算**（合成边界源）+ **预算到点非零退出**（§4.2.1）。
- **§4.6.1 UI 缺口本轮修**：判别式（非回显 `UserMessage` × `!is_turning`）→ `System` 行 + 点亮 Working，**零协议变体**。
- **② 门取 `REVIEW_MIN_ROUNDS = 5`**（不含短回合；采纳评审风险 #1 的收窄），旋钮留在一行常量，终值由 prototype 校准。
- **不拆 `Cancelled`**（不新增 `StopReason` ⇒ 不动 `BasicLoop`）；靠 §4.2.1 的说明行区分「人停 / 预算停」。
- **可观测性本轮做**（§4.9：一行日志 + 预算说明行）。

**仍需 prototype 后定值**：

- **③ 的两个超时阈值**（`connect` / 流空闲）—— `deepseek-reasoner` 首字节可达数十秒，先 prototype 后定值。
- **48h 的轮数/耗时估算**（§4.2）与 **`REVIEW_MIN_ROUNDS` 的合理性** —— 同一 prototype 一并校准（§7 步骤 0）。

---

## 9. 评审修订（2026-10-09，逐条对 [`review.md`](review.md)）

| 评审项 | 处置 | 落点 |
|---|---|---|
| **风险 1** ② 高频成本（每次用工具的请求 +1 轮） | 记账做全（把乘法写出来）；**门槛抬到 `REVIEW_MIN_ROUNDS = 5`**（不再落在每次工具回合） | §4.1、§4.7 |
| **风险 2** UI 修复与「零协议变体」冲突 | **给出判别式修法**（非回显 `UserMessage` × `!is_turning`） | §4.6.1 |
| **风险 3** 会话落盘 O(n) 全量重写 × 48h | **架构级记账**、另立设计 | §6 |
| **风险 4** 48h 是估算、无 prototype | **加为落地步骤 0**（先量后定值） | §7、§8 |
| **风险 5** 成本 / 时长不可观测 | **从「不做」提升为「做」** | §4.9 |
| **风险 6** 去掉轮数上限后防打转只剩粗阀 | 明写为「细阀换粗阀」的交换 | §4.2.1、§6 |
| **风险 7** `-p` 预算非「加法」、退出码未定 | **定退出码**（预算到点非零退出） | §4.2.1 |
| **风险 8** ③ 的本地回环 TCP 测试本环境难自验 | 记账（写进测试基础设施前提） | §4.8、ADR-0028 |
| **风险 9** 重试不覆盖 >~60 s 停服 | 明写边界（治抖动不治停服） | ADR-0028 |
| **风险 10** `rounds` 在截断批次下的假阳性 | 接受（`rounds` 计数含「截断作废后回环」，仍代表够长的回合，轻微） | 本表 |
| **建议 4** 测试矩阵缺端到端「48h 形态」 | **加一条组合测试**（长回合 + 压缩 + 复核） | §5.2 |
| **建议 10** `MAX_CONSECUTIVE_ERRORS` 空分支 | 记账（在 `BasicLoop` 内，不动） | §6 |
| **建议 11** `llm-api-unavailable` 的两个完整性缺口 | 记账（仍归那份上下文的设计） | §6 |

**未采纳（有意保留）**：② 的门不收窄（风险 1 —— 需实测依据）；`Cancelled` 不拆三义（不动 `BasicLoop`，风险/代价见 §4.6）。

---

## 10. 同类软件调研引入（2026-10-09，逐条对 [`research.md`](research.md)）

对象：Pi @ `47236c8`（本地克隆）。完整对照与 `文件:行号` 见 `research.md`，本节只记**处置**。

| 调研项 | 处置 | 落点 |
|---|---|---|
| **无轮数上限**（`while (true)`，无 `maxRounds`） | **佐证 ①** —— 本仓取 `u32::MAX` 与既有实践一致 | §2.2 |
| **无时长兜底**（Pi 只有 abort） | 本仓的 wall-clock 预算是**加强**，不是照抄；如实写明 | §2.2、§4.2.1 |
| **无自动完工复核**（follow-up / steering 是**人**的队列） | ② 是**本仓自有增量**；据此不再把 ② 写成「对齐业界」 | §2.2 |
| **一次性恢复标志**（`overflowRecoveryUsed`、checkpoint 的 `continuation`） | **佐证 ②**：「至多一次」用标志 + 结构早退，不用计数器 —— 与本仓 `TurnKind` 同一形状 | §2.2、§4.1 |
| **`x-should-retry` 头优先于状态码** | **采纳**（三级判据的第 ① 级） | §4.8 规则 2 |
| **账单 / 配额名单（先否后肯）** | **采纳**：`429` 命中额度名单 ⇒ 不重试、立即上报 | §4.8 规则 2 ② |
| **服务端要求的长等待超过上限即失败** | **采纳**（改原「与上界取 max」的写法） | §4.8 规则 3 |
| **重试对用户/排障可见** | **部分采纳**：落 `logging` 一行；**不进事件流**（护零协议变体） | §4.8、§4.9 |
| **退避睡眠可中止** | **不采纳**：`Model::complete` 无取消入参，改核心 trait ⇒ 记账 + 有界退避作界 | §4.8 末条、§6 |
| **中途断流后重试（原子 assistant 消息重放）** | **不采纳**：需渲染撤回信号（违反不变量 #1）+ `-p` 无法撤回；本质是完整性判定，归 `llm-api-unavailable` | §4.8 规则 1、§6 |
| **溢出错误路由到压缩** | **不采纳**：需 `ModelError` 结构化；`400` 已在「不重试」表内，语义已对 | §6 |
| **工具级 `terminate`（工具投票收尾）** | **不采纳**：属「可验证验收」，与本轮裁决冲突 ⇒ 记账 | §6 |
| **durable `retry_wait` 持久重试状态** | **不采纳**：依赖事件溯源引擎；本仓重试在进程内，进程死则预算重置 | §6 |
| **超时：HTTP 吃 SDK 单一总超时（10 min）** | **不采纳其粗**：本仓保持 connect + 流空闲两旋钮 | §4.8 |

**净结果**：① 获得佐证、② 明确为本仓增量、③ 增强四条（判据三级 / 额度否决 / `Retry-After` 硬上限 / 重试可见），
并**如实记账**三条做不到的（退避不可中止、中途断流不重试、溢出不路由）。
