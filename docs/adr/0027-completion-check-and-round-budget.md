# 0027 — 长程完成：单回合预算可配（默认 `u32::MAX` = 不设）+ 一次性「完工复核」

给「长程任务撞预算后烂尾」定两处改动：① **单回合预算 `max_rounds` 变为可配**（走 `config.json`，默认 **`u32::MAX` = 不设**），
在 app 侧经 `resolve_limits` 注入，**核心默认保持 10 不动**；② **模型自称完工后，系统主动追问恰好一次**
「是否真的完工」，另起一个回合。②把仓里**已有但未实现**的 follow-up 缝（`pending.take()` 循环）就地兑现，
判据是一条纯函数（`Completed ∧ rounds ≥ REVIEW_MIN_ROUNDS ∧ 首个回合`），**「只问一次」由二态枚举 `TurnKind` 结构保证**，
不引计数上限 / 停滞判据 / 抢占 / 逃生开关。因需求是 **48 小时长程任务**（任何装得下 48h 的轮数上限都不再是阀），
真正的兜底改为 **wall-clock 预算**（`app_loop` 计时器 → `Boundary::Abort`，默认 48h），随之**零 `BasicLoop` 改动、零 core crate 改动、零新协议变体**
（复核的 UI 呈现改 `ys-tui-coding` 的渲染分支，仍零协议变体 —— 见决策 2 后的 Consequences 与设计 §4.6.1）。

## Status

accepted（2026-10-08；评审修订 2026-10-09）

## 背景

- **需求**（原话）：「考虑提升 coding-agent 的长程执行能力」。**实证**：一次 `/arch-explore` 在
  `⚠ 10 rounds · 60.9s` 处被切断、**未产出结果**。两个独立根因见 [`../arch/long-horizon-completion/context.md`](../arch/long-horizon-completion/context.md) §1：
  R1 预算写死（`crates/ys-component/src/limits.rs:39`，app 侧无覆盖路径）；R2 `app_loop` 的回合边界是死墙
  （`Request::Prompt` 臂恰好一回合、且**不看 `stop_reason`**）。
- **用户裁决**：「**不设置 MaxRounds，或者可以设置的极大**」；「**模型自觉完成，不过需要提示模型确认是否完工**」；
  「**为什么不在 agent 不继续执行后，我们主动问一次模型是否需要继续？**」并**明确反对**「循环 + `MAX_CHECKS` + 停滞判据」的复杂形态。
- **同类软件佐证**：Claude Code 的 **Stop hook** 用布尔量 `stop_hook_active`（「当前已处于一次强制续跑中」）
  保证**至多续跑一次**，并配「连续 block 8 次强制放行」的上限；Stop 也**不在用户中断时触发**。
  ⇒「只问一次」是业界标准护栏形状。（本仓 ADR-0021 已否决 hook 系统，故走「具体判据 + 纯函数」而非通用缝。）
  Pi（agent runtime @ `47236c8`）**无轮数上限**、**无自动完工复核**（其 `follow-up` / `steering` 都是**人**排队的队列），
  且用一次性布尔量表达「某类恢复至多一次」（对照 [`../arch/long-horizon-completion/research.md`](../arch/long-horizon-completion/research.md) §1）——
  ⇒ ① 与既有实践一致，② 是本仓增量，「只问一次由结构保证」的形状则被独立佐证。
- **本仓既有落点**：[`../design-plans/2026-09-15-follow-up.md`](../design-plans/2026-09-15-follow-up.md) §3.1 **已给出**缝的形状
  （`while let Some(msg) = pending.take()`，设计完成、未实现）；ADR-0024 决策 3 认定该循环是 todo 推进的落点、
  「将来 commit-doc 追问只是**同一循环里的第二个判据**」。ADR-0024 决策 2 要求注入内容**纯函数、同内容恒同字节**。
- **`MAX_CONSECUTIVE_ERRORS` 是空守卫**（`basic.rs:514-520` 只有注释、无行为）⇒ 放开轮数后，
  「模型原地打转」的**唯一**代码级防线就是 `max_rounds`。这是「默认必须有限」的理由。

## 决策

### 1. 单回合预算可配；核心默认不动，app 默认不设（`u32::MAX`）

- `apps/coding-agent/src/config.rs` 按 `context_window` / `compaction` 的既有「Raw → 校验 → 规整」形状加
  **顶层标量** `max_rounds`（非按 model 的 map —— 预算是回合属性，与 model 无关）；非法值**只丢该条并落日志**。
- `apps/coding-agent/src/main.rs` 的 `resolve_limits`（TUI / `-p` / `--json` 三入口共用的唯一 limits 接线点）加一行 `limits.max_rounds = config.max_rounds();`
  —— **唯一接线点**，三种模式（TUI / `-p` / `--json`）一次修复。
- **`RunLimits::default().max_rounds` 保持 `10`**（结构性理由：`crates/ys-component/src/lib.rs:18` 有断言；
  且 app_loop 测试 `abort_yields_cancelled_and_view_still_emitted` 靠核心默认 10 收尾）。⇒「库的保守默认」与
  「app 的产品默认」分离。
- **默认值取 `u32::MAX`（= 不设）**：需求是 **48 小时长程任务** ⇒ 48h ≈ 3k–29k 轮（按 6–30 s/轮），
  任何能装下 48h 的轮数上限（≥ ~50k）都已大到**不再是一个阀**，且把「48 小时」绑死在「每轮多久」上（差 3 倍以上）。
  故轮数上限交还给用户原话「不设」，**兜底改用小时计量**（决策 6）。

### 2. 完工复核 = 就地兑现 follow-up 缝，判据是一条纯函数

- 新建 `apps/coding-agent/src/follow_up.rs`：`enum TurnKind { Initial, Review }` +
  `fn should_review(kind, &RunResult) -> Option<AgentInput>` + 常量 `REVIEW_PROMPT`。**不引 `trait` / registry**（ADR-0014/0021）。
- `app_loop` 的 `Request::Prompt` 臂包成 `while let Some((input, kind)) = pending.take()`（**逐字采用设计稿**），
  抽出 `run_turn_once(...)` 消除重复。**不另起第二套「回合边界续跑」机制。**
- **两道门**：只在 `stop_reason == Completed` 后问（`Cancelled` / `Truncated` / `MaxRounds` 一律不续 —— 用户中止永远赢）；
  只在**本回合调用过工具且够长**时问。后者用 **`result.rounds >= REVIEW_MIN_ROUNDS`（`REVIEW_MIN_ROUNDS = 5`）** 判定
  —— `rounds` 是 `BasicLoop` 里**每次模型调用**无条件加一的计数（回环仅在上一轮有 tool call 时继续），直接读 `RunResult.rounds`（`pub`），
  **零事件管道、零新增状态**，规避「扫事件转发点会漏点」的风险。
  **门槛取 `5` 而非 `2`**：`rounds >= 2` 只排除纯文本完工，「读一个文件就答」这类短回合仍满足它 ⇒ 复核会落在
  几乎每次 coding 交互上（成本见设计 §4.7）；而短回合早收的代价只是用户补一句。⇒ 抬到 `5` 把短回合挡在门外，
  只复核够长的回合。**取舍**：门槛越高越可能漏检「跑两三步就自称完工」；`5` 是**待 prototype 校准的初值**。

### 3. 「只问一次」由**结构**保证，不用计数

- `should_review` 首个早退即 `if kind != TurnKind::Initial { return None }`，且 `pending` 只在 `Initial` 时才会填
  ⇒ **每用户输入至多多出一个复核回合**。删掉这一行，测试必红。
- 因此**不需要** ADR-0024 五道护栏中的四道（计数上限 / 停滞护栏 / 用户输入抢占 / 防逃逸）——
  量上（结构 +1 回合）与质上（不存在多回合链，无从「打转」）都已封死。**只保留 `stop_reason` 门。**

### 4. 复核消息：纯常量、`Role::User`、只走 messages 尾部

- 文案为 `const REVIEW_PROMPT: &str`（**英文**，与 `prompt.rs` 的 system prompt 同语言），
  **纯常量、确定性字节**（无时间戳 / 回合号 / 计数，照 ADR-0024 决策 2）。
- 经 `AgentInput::text(REVIEW_PROMPT)` 交给 `run_turn` ⇒ Step 2 强制 `session.append` 为 `Role::User` 并 emit `UserMessage`
  ⇒ 复核与普通回合**同一条路径**、且在 transcript 可见。
  **不用 `Boundary::Steer`**（那是回合**之内**的插话，不增回合数）；**不直接 `session.append`**（会与 Step 2 重复 append 且跳过事件）。
- **绝不碰 `system` / `tools`**（ADR-0021 决策 1）。常量性不直接省钱（任何尾部新段都按 1.0× 付一次），
  它买到的是**防护性**：测试可逐字节 fixture、**永不需就地改写历史**（中段改写才废缓存）、断掉「为改消息而去动 `system`」的禁忌路径。

### 5. 每回合开头排空边界队列（兑现 comment ③）

- 现有 `app_loop` 外层 `loop` 顶部的 `while boundary_rx.try_recv().is_ok() {}` 位于**每次 `request_rx.recv()` 之前**
  —— 即每个请求进入时都排空一次（单回合制下即每个回合前）。
  把它移进 `run_turn_once` 开头 ⇒ 上一回合残留的 `Abort` 不会取消复核回合。
  这是模块注释 ③（「`Abort` **永久**置位」）的必然推论：**新建 `BoundarySource` 只防标志位泄漏，防不了队列里那条消息。**

### 6. 兜底改用 wall-clock 预算（默认 48h），零 `BasicLoop` 改动

- **在 `app_loop` 的回合 `select!` 加一条计时器分支**，到点向当前回合的边界源 `push(Boundary::Abort)`，
  并先发一条 `Outbound::Output` 说明行；`deadline` 在 `Request::Prompt` 入口算一次，覆盖整个回合链。
- **为什么是小时而非轮数**：需求用小时陈述（见决策 1）；`Boundary::Abort` 是既有机制、`BasicLoop` 在轮边界读取它 ⇒ **零 `BasicLoop` 改动**。
- **两条代价如实**：① 这样收场 `StopReason` 只能是 `Cancelled`，与用户 Ctrl+C **同态**，靠那条 `Outbound::Output` 行区分
  （要真区分需新增 `StopReason` = 改 `BasicLoop`，本轮不做）；② `Abort` **只在轮边界被读**
  （`BasicLoop` 的 Step 1 / Step 3 / 轮边界 `take()`），而**模型调用发生在 Step 6** ⇒ 若该次调用不返回，
  到点推入的 `Abort` **不会被观察**，停止实际推迟到**该次在途模型调用返回**、由**适配器的流空闲超时**（默认 120s）界住。
  ⇒ 最坏 = **一次在途模型调用**（**非**「多一轮」）。
- **`-p` / `--json`**：其 `run_single_turn`（`apps/coding-agent/src/main.rs`）不传 `boundary` ⇒ 需给它一条**合成边界源** + 同一计时器分支，方能同样兜底；
  预算到点**非零退出**（无人值守的调度器 / CI 必须能看出「没跑完」）。
- **这是一次「细阀换粗阀」（如实记账）**：去掉轮数上限后，回合内**唯一的**防失控阀就是这条 wall-clock 预算，而它**很粗**
  —— 小输出空转可持续数小时才到点（`MAX_CONSECUTIVE_ERRORS` 仍是空分支）。不是「无损失」。

## Considered Options

- **候选甲（最小复杂度）**：`Request::Prompt` 内联 `loop` + `reviewed: bool`，不新建文件。**部分采纳** ——
  `rounds` 计数判据被采纳（门槛见决策 2）；**拒其落点**：设计稿与 ADR-0024 已指定 `follow_up.rs` 与 `pending.take()` 循环，
  内联会另起第二套缝、日后要合并；且 `reviewed: bool` 是伪装的计数器，不如二态 `TurnKind` 干净。
- **候选丙（成本优先）**：直线式 `if`（非循环）跑两个回合、`used_tools` 扫**两处**事件转发点置位、
  `max_rounds` 默认 `1_000` + 软警告。**部分采纳** —— **定量成本结论**（→ 默认 `u32::MAX`，见决策 1）与
  「控制流形状保证不问第二次」的精神被采纳；**拒其判据来源**（扫事件转发点有漏点风险，见决策 2）与
  **拒其形状**（直线 `if` 无法容纳将来的第二个判据）；**软警告本轮不做**（超范围，记账）。
- **通用 follow-up 框架（`trait FollowUpRule` / registry / `Vec<Box<dyn _>>`）**：今天**只有 1 个消费者**
  ⇒ 按 ADR-0014/0021「零消费者抽象退化为具体类型 + 纯函数」**否决**。升级为队列是 2 行加法。
- **复刻 Claude Code 的 `stop_hook_active` 为可配置 hook**：本仓**无 hook 系统**（ADR-0021 否决）。
  用 `TurnKind` 二态**内联**表达同一语义，零插件面 ⇒ 否决。
- **顺带修 `MAX_CONSECUTIVE_ERRORS` 的空分支**：在 `BasicLoop` 内，**最高爆炸半径**（ADR-0014）。
  **否决**（超出本轮裁决）；**如实记账**为已知缺口。
- **给 `-p` / `--json` 也加复核**：其事件消费者 `consume_events` **以第一个终局事件为终止条件**，
  复核回合的第二个 `RunFinished` 会撞上「消费者已退出」的 `ChannelSink`。**否决**（与 ADR-0024 同一类已接受的退化）。
- **把 `pending` 做成 `Vec<AgentInput>` 队列**：设计稿本就是 `Option::take()` 语义，同一时刻只有一条待跑输入。
  **否决**（假想未来需求）；需要时是 2 行加法。

## Consequences

- **`apps/coding-agent` 三处新增责任**：`config.rs` 多一个解析项与取数口；`main.rs` `resolve_limits` 多一行；
  `app_loop` 的 `Request::Prompt` 臂多一层循环、抽出 `run_turn_once`、排空前移。**零 core crate 改动。**
- **`follow_up.rs` 新文件**（~25 行 + 测试）：`TurnKind` + `should_review` + `REVIEW_PROMPT`。
- **既有测试需更新（预期副作用，非 bug）**：`steer_is_injected_at_round_boundary`、
  `stale_abort_queued_before_first_turn_is_discarded` —— 其回合「`Completed` + 用过工具」现在会多出一个复核回合，
  断言改为「取首个 `RunFinished` 为 `Completed`」，**保留原意图**。
- **一次用户请求可能产生两条 turn 摘要行**（`Envelope.turn` 递增，`meta.turns` 含复核回合）—— 透明、可接受。
- **UI 呈现本轮修（零协议变体）**：TUI 用「非回显 `UserMessage` 到达时 `is_turning` 的真假」区分
  **steering 插话**（回合中 ⇒ `User` 行）与**复核回合**（主回合 `RunFinished` 后 ⇒ `System` 行 + 点亮 Working）；
  只改 `ys-tui-coding` 渲染分支，`ys-protocol` 一字节不改（设计 §4.6.1）。
- **可观测性本轮做**（设计 §4.9）：回合边界经既有 `logging` 落一行累计轮数 / 耗时 / 剩余预算；`-p` 预算到点**非零退出**。
- **已知缺口（记账）**：`MAX_CONSECUTIVE_ERRORS` 空分支仍在；去掉轮数上限后回合内防失控只剩 wall-clock 预算这一条**粗阀**。
- **架构级记账**：会话 `append` 每次全量重写 JSONL（`crates/ys-session/src/jsonl.rs:128`）⇒ 48h ≈ 10⁵ 次全文件重写，**另立设计**。
- **未决点（已收敛）**：默认 `u32::MAX` + wall-clock `48h`；`-p` 一并加预算；UI 本轮修 —— 见
  [`../arch/long-horizon-completion/design.md`](../arch/long-horizon-completion/design.md) §8。
- **模型调用重试 + 超时**另立 [`0028`](0028-model-call-retry-and-timeout.md)（48h 无人值守的自愈面，落适配器）。
- 设计全文（含三候选对比矩阵、成本分析、测试与变异靶）：[`../arch/long-horizon-completion/design.md`](../arch/long-horizon-completion/design.md)；
  上下文与实证：[`../arch/long-horizon-completion/context.md`](../arch/long-horizon-completion/context.md)；
  同类软件调研（Pi）：[`../arch/long-horizon-completion/research.md`](../arch/long-horizon-completion/research.md)。

## As-built 注记（2026-10-09）

实际落点（符号 → 文件）：

- **复核缝**：`apps/coding-agent/src/follow_up.rs` —— `enum TurnKind { Initial, Review }`、`fn should_review(kind, &RunResult) -> Option<AgentInput>`、常量 `REVIEW_MIN_ROUNDS = 5` 与 `REVIEW_PROMPT`；`app_loop` 的 `Request::Prompt` 臂包成 `while let Some((input, kind)) = pending.take()`，回合体抽为 `run_turn_once`（`apps/coding-agent/src/app_loop.rs`）。
- **`resolve_limits` 一行**：`apps/coding-agent/src/main.rs::resolve_limits` 加 `limits.max_rounds = config.max_rounds();`（三入口唯一接线点）。`Config::max_rounds()`（`config.rs`，缺省 `u32::MAX`）与 `Config::max_turn_duration()`（缺省 `172_800`s）为两个新取数口。
- **预算计时器形状**：`deadline` 在 `Request::Prompt` 入口算一次（覆盖整个 `pending` 循环）；每回合新建 `QueueBoundarySource`；`select!` 计时器臂带 `if !budget_exhausted` 前置条件，到点**先**发 `Outbound::Output` 说明行、**再** `boundary.push(Boundary::Abort)`（`app_loop.rs`）。
- **`-p` / `--json` 兜底**：`main.rs::run_single_turn` 合成 `QueueBoundarySource` + 同一计时器，到点先写说明行再 push `Abort`；`single_turn_exit_code` 令 `Cancelled ⇒ 1`（其余 `⇒ 0`）；`exit_with_code` 非零时先 `flush` stdout 再 `exit`。
- **UI 判别式**：`ys-tui-coding/src/app.rs` 的 `AgentEvent::UserMessage` 分支按 `is_turning` 真假区分 steering 插话（`User` 行）与复核回合（`System` 行 + 点亮 `Working`），零 `ys-protocol` 改动。

新增测试：`follow_up.rs`（4）、`app_loop.rs`（11：复核正例 / 四类 `stop_reason` 不复核 / 不问第二次 / 跨回合残留 `Abort` / 标题不被复核覆盖 / 复核不交标题 / wall-clock 到点 / `u32::MAX` 不被核心默认 10 截断）、`config.rs`（6）、`main.rs`（4，含 `resolve_limits` 与 `single_turn_exit_code`）、`ys-tui-coding/src/app.rs`（7）。

### 债务

- **A. 未校准初值**（用户裁定跳过 prototype、按设计初值实现并记账；本环境无模型凭证）：`REVIEW_MIN_ROUNDS = 5`。
- **B. 设计层缺口 —— 预算管不住在途模型调用**：`Boundary::Abort` 只在轮边界被读（`crates/ys-loop/src/basic.rs:58` Step 1、`:95` Step 3、`:212` 轮边界 `take()`），模型调用在 Step 6（`basic.rs:258`）；调用不返回则到点的 `Abort` 不被观察，实际由适配器流空闲超时（`stream.rs` 的 `timeout(idle, …)`，默认 120s）界住 ⇒ 最坏 = 一次在途模型调用。
- **C. `-p` / `--json` 说明行落点与计划不符**：计划写 `Outbound::Output`，实际走 **stderr**（该路径无 outbound 信道、stdout 须保持干净），`apps/coding-agent/tests/budget_exit.rs` 进程级钉住。
- **D. 复核层残余**：`app_loop` 的 §4.9 可观测行在 `Err` 路径记 `rounds = 0`（低估真实轮数，仅影响排障行）；计时器说明行同一 poll 内至多一条（正例，非债务）。详见 `docs/design-final/coding-agent-tui.md` §12。
