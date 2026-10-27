# 长程任务的「完成」（① 预算可配 + ② 完工复核 + ③ 超时重试）执行计划

> 日期：2026-10-09
> 设计计划：[`../design-plans/2026-10-09-long-horizon-completion.md`](../../design-plans/completed/2026-10-09-long-horizon-completion.md)
> 设计定稿：[`../arch/long-horizon-completion/design.md`](../arch/long-horizon-completion/design.md)
> 决策：[ADR-0027](../adr/0027-completion-check-and-round-budget.md) · [ADR-0028](../adr/0028-model-call-retry-and-timeout.md)
> 基线：`master` @ `5eae60a`（工作区干净）
> 波次划分见 §3（同波文件集互不相交）。
> 性质：**实现前计划**。

## 基线

- **基线 ref**: `5eae60a`（`master`，工作区干净）
- 用途：review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含新文件）
- 包名对照：`apps/coding-agent` → 包名 **`ys-coding-agent`**；`adapters/model-openai-compatible` → 包名 **`ys-model-openai-compat`**（目录名 ≠ 包名）

---

## 1. 任务表

## Task 1: `config.rs` 加 `max_rounds` + `max_turn_duration_secs` + `model_retry` + `model_timeout`

- type: impl
- files: `apps/coding-agent/src/config.rs`
- depends: 无
- description: |
    逐字照 `context_window` / `compaction` 的 **Raw → 校验 → 规整**三段式（`config.rs:20-27` RawUserConfig、`:250-271` parse_user_config、`:277-289` sanitize_ratio）。
    1) `RawUserConfig` 增四个字段：`#[serde(default)] max_rounds: Option<serde_json::Value>`（**顶层标量**）；
       `#[serde(default)] max_turn_duration_secs: Option<serde_json::Value>`（**顶层标量**，单位**秒**）；
       `#[serde(default)] model_retry: RawRetry`（含 `max_attempts` / `base_delay_ms` / `max_delay_ms`，各 `Option<serde_json::Value>`）；
       `#[serde(default)] model_timeout: RawTimeout`（含 `connect_secs` / `idle_secs`）。
    2) `UserConfig` 增规整形态：`max_rounds: Option<u32>`、`max_turn_duration_secs: Option<u64>`、`retry: ModelRetry`、`timeout: ModelTimeout`（后二者用默认值 filled）。
    3) `parse_user_config` 增四支校验：`max_rounds` 走 `as_u64()` → `u32::try_from` → `>= 1`；`max_turn_duration_secs` 走 `as_u64()` → `>= 1`
       （0 / 负 / 浮点 / 字符串 / null 逐条丢弃）；
       `model_retry` / `model_timeout` 各字段各自校验（正整数，越界丢弃该字段回默认）。**非法值只丢该条 + `crate::logging::log`，不整份回退、不 panic**。
    4) 取数口：`Config::max_rounds(&self) -> u32`（覆盖 > `u32::MAX` 默认）、
       `Config::max_turn_duration(&self) -> std::time::Duration`（缺省回落 `Duration::from_secs(172_800)` = 48h）、
       `Config::model_retry(&self) -> ModelRetry`、`Config::model_timeout(&self) -> ModelTimeout`。
    默认值（**待真实环境校准的初值**）：`max_rounds = u32::MAX`；`max_turn_duration_secs = 172_800`（48h）；
    `model_retry = { max_attempts: 5, base_delay_ms: 500, max_delay_ms: 30_000 }`；`model_timeout = { connect_secs: 15, idle_secs: 120 }`。
- priority: P0

## Task 2: `config.rs` 配置解析测试

- type: test
- files: `apps/coding-agent/src/config.rs`（`#[cfg(test)]`）
- depends: Task 1
- description: |
    在 `config.rs` 的 `#[cfg(test)]` 内加用例（照既有 `test_invalid_entries_are_dropped_individually`）：
    - **合法采纳**：`max_rounds` / `max_turn_duration_secs` / `model_retry` / `model_timeout` 各字段合法值被采纳
      （`max_turn_duration_secs` 合法值经 `Config::max_turn_duration()` 取回 `Duration::from_secs(合法值)`）；
    - **非法逐条丢弃**：`max_rounds` 为 `0` / `-1` / `1.5` / `"5"` / `null` 各自被丢弃、回默认且**不整份回退**（其余合法字段仍在）；
      `max_turn_duration_secs` 为 `0` / `-1` / `1.5` / `"5"` / `null` 同理；`model_retry.max_attempts = 0`、`model_timeout.connect_secs = "x"` 同理；
    - **缺省取默认**：整份空 JSON ⇒ 取 `Config::max_rounds() == u32::MAX`、
      `Config::max_turn_duration() == Duration::from_secs(172_800)` 与两段默认；
    - 未知字段忽略。
- test_method: `cargo test -p ys-coding-agent config`
- priority: P0

## Task 3: 新建 `follow_up.rs`（`TurnKind` / `should_review` / `REVIEW_PROMPT`）

- type: impl
- files: `apps/coding-agent/src/follow_up.rs`（新）, `apps/coding-agent/src/main.rs`（加 `mod follow_up;`）
- depends: 无
- description: |
    新建 `apps/coding-agent/src/follow_up.rs`，只放三样（**不引 `trait` / registry / 列表**，ADR-0014/0021）：
    - `#[derive(Clone, Copy, PartialEq, Eq)] pub(crate) enum TurnKind { Initial, Review }`；
    - `pub(crate) const REVIEW_MIN_ROUNDS: u32 = 5;`（短回合防火墙，待校准初值）；
    - `pub(crate) fn should_review(kind: TurnKind, result: &RunResult) -> Option<AgentInput>`，**两段早退结构不可合并**：
      `if kind != TurnKind::Initial { return None }`（←「只问一次」的**全部**实现）→
      `if result.stop_reason != StopReason::Completed { return None }`（门 ①）→
      `if result.rounds < REVIEW_MIN_ROUNDS { return None }`（门 ②）→ `Some(AgentInput::text(REVIEW_PROMPT))`；
    - `pub(crate) const REVIEW_PROMPT: &str`（**英文常量，确定性字节**，无时间戳/回合号/计数），文案取自 design §4.5。
    在 `main.rs` 声明 `mod follow_up;`。
- priority: P0

## Task 4: `follow_up.rs` 纯函数测试

- type: test
- files: `apps/coding-agent/src/follow_up.rs`（`#[cfg(test)]`）
- depends: Task 3
- description: |
    加 `should_review` 四态真值表：
    - `Initial + Completed + rounds == REVIEW_MIN_ROUNDS → Some`，且返回的 input 文本 `== REVIEW_PROMPT`；
    - **`Initial + Completed + rounds == REVIEW_MIN_ROUNDS - 1 → None`**（短回合不复核）；
    - `Initial + Completed + rounds == 1 → None`；
    - `Initial + Cancelled / Truncated / MaxRounds → None`（门 ① 三态）；
    - **`Review + Completed + rounds >= REVIEW_MIN_ROUNDS → None`**（「不问第二次」）。
    再断言 `REVIEW_PROMPT` 两次取用**字节相等**（常量确定性）、不含数字/路径。
    **变异靶**：删 `kind != TurnKind::Initial` 早退 ⇒ `Review` 行变红；门槛改回 `2` ⇒ 短回合行变红；门 ① 写成「非 Cancelled 即可」⇒ `Truncated`/`MaxRounds` 行变红。
- test_method: `cargo test -p ys-coding-agent follow_up`
- priority: P0

## Task 5: `app_loop.rs` — 纯重构：抽 `run_turn_once` + 边界排空前移

- type: impl
- files: `apps/coding-agent/src/app_loop.rs`
- depends: 无
- description: |
    **纯重构，行为必须逐字节不变**。把现 `app_loop.rs:130-163` 的「`begin_turn` → 新建 `QueueBoundarySource` →
    `select!` 三臂（`turn` / `sink_rx` 事件转发 / `boundary_rx` 泵入）→ 回合末 `try_recv` 排空」抽成私有 `async fn run_turn_once(...)`；
    把**回合前的边界排空**移进 `run_turn_once` 开头（现在在 `app_loop.rs:100` 外层 loop 顶部、`request_rx.recv()` 之前，
    即**每个请求前都跑**，不是「仅首回合前一次」——这是核实偏差 D1，按 Task 16 记录）；`turns += 1` 随之下移进 `run_turn_once`。
    **守门员**：既有 15+ 条 app_loop 测试全部保持绿且断言不变（本任务**不**改 `Request::Prompt` 臂的循环形状、不引 `pending`）。
- priority: P0

## Task 5B: `app_loop.rs` — `pending.take()` 循环（复核回合）

- type: impl
- files: `apps/coding-agent/src/app_loop.rs`
- depends: Task 5, Task 3
- description: |
    把 `Request::Prompt` 臂（`app_loop.rs:109-186`）包成
    `let mut pending = Some((AgentInput::new(message), TurnKind::Initial)); while let Some((input, kind)) = pending.take() { ... }`
    （**逐字采用**设计稿形状，design §4.3c）：
    - 每个回合末用 `follow_up::should_review(kind, &result)` 决定是否 `pending = Some((review_input, TurnKind::Review))`；
    - `Ok(result)` 臂：`stats.record` + `finalize_turn_meta`（**标题素材仅取 `TurnKind::Initial` 那一回合**，复核回合传 `None`）后
      `pending = should_review(kind, &result).map(|next| (next, TurnKind::Review))`；`Err(e)` 臂只落日志、`pending` 保持 `None`（链终止）。
    复核只在「`Initial` + `Completed` + 够长」时触发 ⇒ 纯文本 / 短回合的既有测试行为不变；
    「`Completed` + 用过工具」的回合会多出一个复核回合（预期副作用，既有测试更新见 Task 6）。
- priority: P0

## Task 6: `app_loop.rs` 复核集成测试

- type: test
- files: `apps/coding-agent/src/app_loop.rs`（`#[cfg(test)]`）
- depends: Task 5B
- description: |
    复用既有 `Harness`（`:366`）+ `ProbeModel`（`:567`）+ `ProbePlan`（`:545`，`UntilMarker` / `AlwaysToolCall` / `ToolCallThenFinal`）：
    - **工具回合 ⇒ 恰好复核一次**：`ToolCallThenFinal`，断言 `RunFinished` 两条（`turn` 依次 1、2）、
      转发序列里恰一条 `UserMessage` 文本 `== REVIEW_PROMPT`、`view.turn_count == 2`、`meta.turns` 含复核；
    - **纯文本回合 ⇒ 不复核**（`UntilMarker`）；
    - **`Cancelled` / `Truncated` / `MaxRounds` ⇒ 不复核**；
    - **不问第二次**（核心变异靶）：主 + 复核回合都「用工具 + `Completed`」⇒ 断言**恰好两回合**；
    - **跨回合残留 Abort 不取消复核**：回合 1 结束后投一条 `Abort` ⇒ 复核**仍发生**（验证排空前移）；
    - **顺序**：复核 `RunFinished` 在主回合 `RunFinished` 之后；
    - **标题不被复核覆盖**。
    **更新两条既有测试（预期副作用）**：`steer_is_injected_at_round_boundary`（`:774`）、`stale_abort_queued_before_first_turn_is_discarded`（`:1233`）
    —— 其「`Completed` + 用过工具」回合现在多一个复核回合，断言改为「取**首个** `RunFinished` 为 `Completed`」，**保留原意图**。
    **回归护栏必须继续绿**：`outbound_sequence_is_ordered_with_view_last`、`every_request_variant_is_handled`、`abort_yields_cancelled_and_view_still_emitted`。
- test_method: `cargo test -p ys-coding-agent app_loop`
- priority: P0

## Task 7: `resolve_limits` 注入 `max_rounds` + `app_loop` 计时器分支

- type: impl
- files: `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/app_loop.rs`
- depends: Task 5B, Task 1
- description: |
    - `main.rs` `resolve_limits`（`:361-366`）加**一行** `limits.max_rounds = config.max_rounds();`（唯一接线点）。
    - `app_loop`：`run_turn_once` 的 `select!` 加一条 `tokio::time::sleep_until(deadline)` 分支；
      到点向当前回合边界源 `push(Boundary::Abort)`，并**在同一处先发一条** `Outbound::Output` 说明行（`budget exhausted: …`）。
    - `deadline` 由 **`config.max_turn_duration()`** 在 `Request::Prompt` **入口算一次**（`Instant::now() + duration`），
      覆盖整个 `pending.take()` 循环（含复核回合）——**不得写死常量**；缺省即 `Duration::from_secs(172_800)`（= 48h），
      经 `config.json` 的 `max_turn_duration_secs` 可调。
    - §4.9 可观测性一行：每个回合边界（`run_turn_once` 结束处）经 `logging` 落
      `turn=N rounds=R elapsed=Es cumulative_rounds=CR budget_remaining=Bs`。
- priority: P0

## Task 8: 预算与 `resolve_limits` 测试

- type: test
- files: `apps/coding-agent/src/app_loop.rs`（`#[cfg(test)]`）, `apps/coding-agent/src/main.rs`（`#[cfg(test)]`）
- depends: Task 7
- description: |
    - **预算到点（可配验收点）**：在**临时 `config.json`** 里写入**极短预算**（如 `max_turn_duration_secs: 1`），
      经 `config.load_user_config(path)` 加载后跑一个 `AlwaysToolCall` 探针回合 ⇒ 断言以 `StopReason::Cancelled` 收场
      **且**恰有一条 `Outbound::Output` 说明行（`budget exhausted` 措辞）。这正是「可配」的验收点，
      也是唯一能测 48h 兜底的途径；
    - **残留 `Abort` 不得取消复核回合**（**变异靶：排空留在 loop 顶不动 ⇒ 红**）；
    - `max_rounds = u32::MAX` 时不被核心默认 10 轮截断（长回合跑到 >10 轮仍 `Completed`）；
    - `resolve_limits` 断言 `limits.max_rounds == config.max_rounds()`，且计时器取 `config.max_turn_duration()`
      （扩展既有 `resolve_limits_reads_window_and_policy_from_config`）。
- test_method: `cargo test -p ys-coding-agent resolve_limits`
- priority: P0

## Task 9: `run_single_turn` 加合成边界源 + 计时器 + 非零退出

- type: impl
- files: `apps/coding-agent/src/main.rs`
- depends: Task 7
- description: |
    `run_single_turn`（`main.rs:717-725`，现**不带 boundary**）：
    - 新建一条合成 `QueueBoundarySource` 并在 `run_turn` 时经 `AgentPorts::new(.., Some(&boundary))` 传入；
    - 与 `app_loop` **同一个计时器分支**：`select!` 包住 `run_turn`，计时器**同样取 `config.max_turn_duration()`**
      （不得写死常量），到点 `push(Boundary::Abort)` + 一条 `Outbound::Output` 说明行；
    - 预算到点 ⇒ **非零退出**（`Cancelled` 语义）—— 不得以 `0` 退出；正常完成仍按既有退出码。
- priority: P0

## Task 10: `-p` / `--json` 预算到点非零退出测试

- type: test
- files: `apps/coding-agent/tests/`（新文件）
- depends: Task 9
- description: |
    新建集成测试（照 `apps/coding-agent/tests/` 既有 CLI 测试范式）：用极短预算跑 `-p` / `--json` 一次性任务，
    断言**进程非零退出**、stdout/stderr 含预算说明；正常完成路径退出码不变（回归护栏）。
- test_method: `cargo test -p ys-coding-agent --test <新文件名>`
- priority: P0

## Task 11: 适配器超时 + 有界重试（±新 `retry.rs`）

- type: impl
- files: `adapters/model-openai-compatible/src/lib.rs`（±新 `retry.rs`）
- depends: 无
- description: |
    落 `ys-model-openai-compat`（design §4.8 / ADR-0028），**零 `Model` / `ModelError` 改动**：
    - **超时两旋钮**：`connect_timeout`（`reqwest::Client` builder）+ **流空闲超时**（`tokio::time::timeout` 包住字节流，建议 ~120s 无字节）；
    - **sink 计数器**：`complete` 内包一层，只看 `TextDelta` / `ThinkingDelta`；一旦发出，其后任何失败**直接上报、不重试**；
    - **三级可重试判定**（从左到右短路）：① `x-should-retry` 响应头定论 → ② **额度名单否决**（常量：`insufficient_quota` / `quota exceeded` /
      `out of budget` / `billing` / `usage limit` / `available balance`，大小写不敏感子串；命中即**立即可不重试**）→
      ③ 状态码（连接/DNS/TLS/超时、`429`、`500/502/503/504` 可重；`400/401/403/404/413/422`、serde 解析失败、sink 错误不可重）；
    - **有界退避**：指数 base 500ms ×2 cap 30s、最大 5 次；`Retry-After` / `Retry-After-ms` 尊重但以 `max_delay_ms` 为**硬上限**，
      **超上限 ⇒ 放弃重试、立即上报**（错误文本含服务端要求秒数）；
    - **退避日志**：进入退避前经 `logging` 落一行 `retry attempt k/n delay=…ms reason=…`。
    构造期从 `OpenAICompatibleConfig` 取 `model_retry` / `model_timeout`（Task 13 注入）。
- priority: P0

## Task 12: 适配器回环 TCP 测试

- type: test
- files: `adapters/model-openai-compatible/src/lib.rs`（`#[cfg(test)]`，± `adapters/model-openai-compatible/tests/`）
- depends: Task 11
- description: |
    本地回环 `127.0.0.1:0` TCP mock（照 `apps/coding-agent/tests/stream_usage_fallback.rs:57-120` 的内联 `MockServer` 范式，`.no_proxy()` 直连）覆盖：
    - `429`×3 后 `200` ⇒ 恰好重试 3 次、最终 `Ok`（请求计数 == 4）；
    - `401` ⇒ 不重试（计数 == 1）；
    - **额度名单**：`429` + body `{"error":{"code":"insufficient_quota"}}` ⇒ 不重试（计数 == 1）；
    - **响应头优先**：`500` + `x-should-retry: false` ⇒ 不重试；`400` + `x-should-retry: true` ⇒ 重试；
    - **`Retry-After` 超限**：`429` + `Retry-After: 3600` ⇒ 立即失败（断言未睡满 ≤ 1s 返回、错误文本含 `3600`）；
    - **已吐 delta 后断流 ⇒ 不重试**（计数 == 1；**变异靶：去掉计数器 ⇒ 红**）；
    - 黑洞（accept 后不应答）⇒ 空闲超时触发 ⇒ 按可重试处理；
    - **重试日志**：重试路径恰落一行 `retry attempt …`，不重试路径零该行；
    - `4xx` 不重试。
- test_method: `cargo test -p ys-model-openai-compat`
- priority: P0

## Task 13: `build_agent` 传 `model_retry` / `model_timeout` 给适配器

- type: impl
- files: `apps/coding-agent/src/main.rs`
- depends: Task 1, Task 11
- description: |
    `build_agent`（`:329-342`）/ model factory 处，把 `config.model_retry()` / `config.model_timeout()` 传入 `OpenAICompatibleModel` 构造：
    经 `with_client`（`lib.rs:44`）注入一个带 `connect_timeout` 的 `reqwest::Client`（替代 `lib.rs:37` 的裸 `Client::new()`），
    并把重试参数填入 `OpenAICompatibleConfig`。`model_timeout.idle_secs` 传到流空闲超时处。
- priority: P0

## Task 14: `ys-tui-coding` 复核回合渲染分支

- type: impl
- files: `crates/ys-tui-coding/src/app.rs`
- depends: 无
- description: |
    `apply_event` 的 `UserMessage` 分支（现 `app.rs:91-101`）在 suppress 计数之后按 `is_turning` 分流：
    - `is_turning == true` ⇒ **保持原** `TranscriptLine::User(message_text(&message))`（steering 插话，既有行为）；
    - `is_turning == false` ⇒ push `TranscriptLine::System(message_text(&message))` + 置 `is_turning = true` +
      `turn_started_at = Some(Instant::now())`；
    - 两分支后均 `self.follow = true;`。
    **`ys-protocol` 一字节不改**（只改渲染分支）。
- priority: P1

## Task 15: `ys-tui-coding` 判别式两侧测试

- type: test
- files: `crates/ys-tui-coding/src/app.rs`（`#[cfg(test)]`）
- depends: Task 14
- description: |
    - **复核回合 ⇒ `System` 行 + 点亮**：`is_turning == false` 时投一条非回显 `UserMessage` ⇒ 出 `TranscriptLine::System`、
      `is_turning == true`、`turn_started_at.is_some()`；
    - **steering ⇒ `User` 行（回归护栏）**：`is_turning == true` 时投一条非回显 `UserMessage` ⇒ 仍 `TranscriptLine::User`、**不改** `is_turning`。
    **变异靶**：把判别式写成「非回显即 `System`」⇒ steering 用例变红。
- test_method: `cargo test -p ys-tui-coding app`
- priority: P1

## Task 16: 修正 design.md 与两 ADR 的核实偏差

- type: impl
- files: `docs/arch/long-horizon-completion/design.md`, `docs/adr/0027-completion-check-and-round-budget.md`, `docs/adr/0028-model-call-retry-and-timeout.md`
- depends: 无
- description: |
    按实现前核实结果修正已被代码证伪的措辞（**不改设计本身，只对齐事实**）：
    - **D1**：design §4.3b 称边界排空「只在首个回合前执行一次」→ 实测在**外层 loop 顶部、每个 `request_rx.recv()` 之前都跑**（`app_loop.rs:100`）；
    - **D2**：design §4.1 称「`rounds` 只在有 tool call 时递增」→ 实际 `rounds += 1` 在 **Step 5 无条件**（`crates/ys-loop/src/basic.rs:247`）；
      「`rounds >= 2` ⟺ 用过工具」的**结论不变**（仅工具调用/截断批才回环），需改的是**机制描述**；
    - **D3**：`.limits()` 在 `AgentBuilder` 上而非 `Agent`；
    - **D5**：行号 `resolve_limits` = `361-366`、`run_single_turn` = `717-725`；
    - **D6**：补一句「TUI `events.rs` 当前从不发 `Boundary::Steer`，判别式仍正确且前向兼容」；
    - **D7（命名细化）**：设计原文写 `max_turn_duration`，实现取**显式单位**形式 `max_turn_duration_secs`（避免单位歧义），
      属命名细化、语义一致（默认 48h / 可经 `config.json` 调不变）。
- priority: P1

## Task 17: 更新 as-built 与初值 debt

- type: impl
- files: `docs/design-final/coding-agent-tui.md`, `docs/adr/0027-completion-check-and-round-budget.md`, `docs/adr/0028-model-call-retry-and-timeout.md`
- depends: Task 1-16 + 5B（全部）
- description: |
    - 更新 `docs/design-final/coding-agent-tui.md`（as-built）记录本轮实际形态；
    - 把 **`REVIEW_MIN_ROUNDS = 5`、`connect 15s` / `idle 120s`、退避 `base 500ms ×2 cap 30s ≤5`** 记为
      **待真实环境校准的初值 debt**（本环境无 `YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`，**用户已裁定跳过 prototype**）；
    - 两 ADR 补 as-built 注记与 debt 节。
- priority: P1

---

## 2. 覆盖自查

- **每个 `type: impl` 都有配对的 `type: test`**：T1↔T2、T3↔T4、T5B↔T6、T7↔T8、T9↔T10、T11↔T12、T14↔T15；
  **T5（纯重构）无新增配对测试** —— 守门员是**既有 15+ 条 `app_loop` 测试**（review 判据：「行为逐字节不变」）；
  T13（接线）由 T8（`resolve_limits` 断言）与 T12（适配器构造生效）共同覆盖；T16 / T17 是文档任务，无配对测试。
- **`test` 类型任务**：T2、T4、T6、T8、T10、T12、T15（纯 `#[cfg(test)]` 或 `tests/`，不碰生产代码；T8 含 `main.rs` 断言）。
- **文件真实性**：既有路径均已核对存在；**新建** 2 处 —— `apps/coding-agent/src/follow_up.rs`（T3）、
  `apps/coding-agent/tests/<新文件>`（T10）；适配器 `retry.rs` 为**可选**新模块（T11）。
- **不变量守卫**：`crates/ys-core` / `ys-event` / `ys-protocol` / `ys-loop` 不出现在任何任务里（零改动）；
  `RunLimits::default().max_rounds` 仍 10（`crates/ys-component/src/lib.rs:18` 断言不动）；新协议变体 **0**。
- **依赖图无环**：根为 T1 / T3 / T5 / T11 / T14 / T16（T5 纯重构现**无依赖**）；T5B←T5,T3；T6←T5B；T7←T5B,T1；T9←T7；T13←T1,T11；T17 汇合全部。

## 3. 波次计划

同波内**文件集互不相交**（由 `files` + `depends` 推导）。

| 波次 | 单元 | 并集文件 | 说明 |
|---|---|---|---|
| **W0** | T1、T3、T5、T11、T14、T16 | `config.rs` · `{follow_up.rs, main.rs}` · `app_loop.rs` · `adapters/model-openai-compatible/src/{lib.rs,retry.rs}` · `crates/ys-tui-coding/src/app.rs` · `docs/{design.md,adr/0027,adr/0028}` | 六个**无依赖的根**，落点互不相交。 |
| **W1** | T2、T4、T5B、T12、T15 | `config.rs` · `follow_up.rs` · `app_loop.rs` · 适配器测试 · `app.rs` | 各自依赖落在 W0（T5B←T5,T3）；文件集互不相交。 |
| **W2** | T6、T13 | `app_loop.rs` · `main.rs` | T6←T5B；T13←T1,T11。文件集互不相交。 |
| **W3** | T7 | `main.rs`、`app_loop.rs` | T7←T5B,T1；与 W2 的 T6/T13 同文件 ⇒ 独立一波。 |
| **W4** | T8 | `app_loop.rs`、`main.rs` | T8←T7；独占。 |
| **W5** | T9 | `main.rs` | T9←T7；与 T8 同文件 ⇒ 独立一波。 |
| **W6** | T10 | `apps/coding-agent/tests/<新文件>` | T10←T9；新文件但与 T9 有依赖 ⇒ 独立一波。 |
| **W7** | T17 | `docs/design-final/coding-agent-tui.md`、两 ADR | T17←全部；文档收口。 |

**关键路径**：T1 → T7 → T9 → T10（或 T5 → T5B → T7 → T9），**最大并行度**：W0 的 6 个单元。
**注**：`main.rs` 是 T3（`mod` 声明）、T7、T9、T13 的共享文件，故它们必须落在不同波次；`app_loop.rs` 同理（T5/T5B/T6/T7/T8）。

## 4. 验证清单

- [ ] 所有 (impl, test) 配对均已 completed
- [ ] `cargo test --workspace` 全绿（基线口径写入 as-built；新增测试数在实现时统计）
- [ ] `cargo clippy --all-targets` 零 warning
- [ ] `cargo check --workspace --all-targets` 通过
- [ ] `cargo test -p ys-coding-agent follow_up` / `app_loop` / `config` / `resolve_limits` 通过
- [ ] `cargo test -p ys-model-openai-compat` 通过（回环 TCP mock）
- [ ] `cargo test -p ys-tui-coding app` 通过（判别式两侧）
- [ ] `cargo test -p ys-coding-agent --test <新文件名>` 通过（`-p` / `--json` 预算非零退出）
- [ ] **变异清单全部被对应用例抓到**且已还原（含：删 `kind != Initial` 早退 / 门槛改回 2 / 排空留在 loop 顶 /
      去掉 sink 计数器 / `Retry-After` 取上界 / 判别式写成「非回显即 System」）
- [ ] `git diff <基线 5eae60a>` 逐文件核对，无计划外改动
- [ ] **不再跑裸 `cargo fmt`**（本机 rustfmt 与该仓库 CJK 文本有既有分歧，会污染无关文件）
- [ ] **MANUAL_ACK_REQUIRED（未做）**：真实 provider 下的复核触发、超时/重试行为、48h 形态组合测试 ——
      需 `YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`；**用户已裁定跳过 prototype**，初值记 debt（T17）
