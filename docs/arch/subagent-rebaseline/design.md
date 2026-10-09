# Subagent 派发 — 落地复核与刀序（基线重估）

> 产出：`/arch-design`（2026-10-09）。输入：本轮 [`context.md`](context.md)、权威设计
> [`../subagent/design.md`](../subagent/design.md) 与 [ADR-0022](../../adr/0022-subagent-dispatch.md)。
> 证据均对应工作区 `master`（`911fd72`），带 `文件:行号`。
>
> **定位**：本文**不改** ADR-0022 的形态决策。以下七条仍是权威、不重开 ——
> ① 派发能力走窄 trait 注入（照 `Summarizer`）；② 子事件不进父事件流；
> ③ 子工具表 = 父表去掉派发工具、派发工具追加**末位**；④ 结果 = 硬截断摘要 + 产物路径 +
> `StopReason`；⑤ 取消 = `ChildBoundary` 只透传 `is_aborted`；⑥ 子 usage 上卷 `TurnStats`；
> ⑦ 工具名 = `subagent`。
>
> 本文只做三件事：**① 把这些决策对齐到当前代码**；**② 裁定 ADR-0022 之后才出现的交互面**；
> **③ 给出重排后的刀序与每刀的验收测试**。裁定结论见
> [ADR-0029](../../adr/0029-subagent-landing-decisions.md)（状态 proposed，待用户确认）。

---

## 0. 本轮用户裁定（不得重开）

| # | 裁定（2026-10-09） | 后果 |
|---|---|---|
| 1 | **先复核**，不落地 | 本文是施工前的差异与裁定清单，不是代码 |
| 2 | **需要并行** | `is_concurrency_safe()` + `max_parallel_tools` + Step 9 改造**必做**，非可选 |
| 3 | **per-tool 超时先于** `subagent` 工具 | 独立可交付刀；且是长派发的前置阻塞项 |
| 4 | **子独立预算** | 子**不计入**父 `max_turn_duration`，子有自己的预算实例 |
| 5 | **子预算取值与主 agent 一致** | 同一份 `Config`：`max_rounds` / `max_turn_duration` / `context_window` |
| 6 | **成本读数本轮做** | `Usage` 缓存字段 + 适配器解析 |

⚠️ 裁定 4 + 5 的联合推论必须显式记账（详见 §5）：
**父子预算「独立的」是所有权，不是数值差** —— 两者取同一份配置 ⇒ 子**没有**比父更紧的独立上限，
真正的兜底仍是父回合那条 deadline（父先起算 ⇒ 父先到点）。ADR-0022 写的「子必有界」被这两条裁定
**实际废弃**，本轮的替代保证是 §5.3 的「父预算经 `is_aborted` 向下传播 + 子在轮边界停下」。

---

## 1. 结论先行：四个决策点的裁定

| 决策点 | 裁定 | 相对 ADR-0022 |
|---|---|---|
| **① per-tool 超时** | `Tool::timeout() -> Option<Duration>`（默认 `None`），优先级 `工具声明 → limits.bash_timeout → 300s`；**`subagent` 的外闸 = 子预算 + 宽限**（构造期由 `Config` 注入） | 决策 6 的机制保留；**「subagent 声明 1800s」被推翻**（与裁定 5 冲突） |
| **② 并行派发** | Step 9 条件并行：`len > 1` 且**每一个**工具都 `is_concurrency_safe()` 才走并行；`buffered(max_parallel_tools)`（保序、单任务内并发、不需要 `'static`） | 决策 11/13 保留；决策 14 的**常量位置修正**（见 §7） |
| **③ 子独立预算** | 子 = 全新 `RunLimits`（各值取自同一 `Config`）+ 全新 `QueueBoundarySource`（`Arc`）+ `ChildBoundary`（`take()` 恒空、`is_aborted()` 或父）+ watchdog（`sleep(budget)` 后 `push(Abort)`） | 决策 5 形状保留并**补上「子自己的 deadline」**（ADR-0022 时 ADR-0027 还不存在） |
| **④ 成本读数** | `Usage` 加两个 `#[serde(default)]` 字段 + 适配器解析；JSONL 会话契约**零改动**；`context_window_for(name)` 归「模型可指定」那把刀 | 决策 18 保留；**成本数字全部作废**（先装读数、后重算） |

> **另有四条 ADR-0022 决策在首轮复核中被漏掉，已补回并见 §10**：每回合派发总量上限（决策 15，**P0**）、
> 产物目录 sweep + 测试覆盖口（决策 4）、fail-closed（决策 15/D15）、「模型可指定」的完整连带（决策 17）。
> 来源：`/arch-validate`（[`review.md`](review.md)）。

---

## 2. 三个候选（同一组决策的三种哲学）

> 三个候选**只覆盖上面四个决策点**，不覆盖 ADR-0022 已定死的形态；对比见 §2.4。

### 2.1 候选 A — 最小复杂度优先

- 新增概念 **2 个**：`Tool::timeout()` / `Tool::is_concurrency_safe()` 两个**默认方法**（零默认行为）。
- `RunLimits` 加 1 个字段 `max_parallel_tools`（默认 = 常量，只有新分支读它）。
- Step 9 **就地分叉**：审批与 `ToolCall` 事件仍串行预发（保持原顺序），执行段按闸门二选一。
- 子预算 = 直接调 `Config` 的既有访问器各取一次（`max_rounds()` / `max_turn_duration()` /
  `context_window_for(child_model)` / `compact_policy()`），**不加 `Budget` 类型**。
- 子 watchdog = 一个 `tokio::spawn` + `sleep + push(Abort)`，退出时 `abort()` 句柄。
- usage 上卷 = `Arc<Mutex<Usage>>`（4 并发下锁串行，但每子只写一次，可忽略）。
- 判据：**一个 PR 能讲清的最小改动集合**；读者是单人维护的小仓。

### 2.2 候选 B — 可扩展优先

- 把**预算**升为一等类型 `RunBudget { deadline, rounds_left, dispatches_left }`，父子各持一份，
  经 `ys-component` 传进 `RuntimeContext`；`BasicLoop` 的 `max_rounds` 判定改读它。
- 把**工具批执行**收成一个可测单元 `ToolBatchExecutor`（审批 → 并发 → 保序），
  `BasicLoop` 只调用它。
- 成本用 `CostLedger`（缓存命中/未命中/输入/输出四元记账），`TurnStats` 从它派生。
- 论据：ADR-0027 的**复核回合**与 subagent **都在额外驱动模型调用** ⇒ 已 ≥2 个消费者。
- 代价：新概念 ≈5 个；`deadline` 是**库层计时**这一新职责（今天计时全在 `app_loop`，
  `crates/ys-loop` 只消费 `max_rounds` 这个计数）；`RunBudget` 要穿过
  `ys-component` → `ys-loop` 两跳，而 `ys-core` 零 tokio 的约束会让 deadline 只能以
  `std::time::Instant` 形式下潜（可行，但把「谁计时」这件事从 1 处变 3 处）。

### 2.3 候选 C — 性能 / 资源优先

- 核心衡量：**prompt cache 的字节稳定** 与 **每次派发零重复构造**。
- 启动期**冻结**父 / 子的 spec 与 system prompt；子表由
  `build_tools(workspace, web, skip: &["subagent"])` **确定性重建**（不克隆 `Box<dyn Tool>`，
  靠 `Tool::spec()` 的纯函数性保证字节一致）。
- 并行用 `buffered`（**保序**）而非 `buffer_unordered` ⇒ 落盘与 `ToolResult` 事件顺序天然正确。
- usage 用 `AtomicU64` ×4（命中/未命中/输入/输出），4 并发下无锁争用。
- 三条守卫测试：父前缀 `sha256` 锚 / 子前缀 `sha256` 锚 / **同一子表构造两次逐字节相同**。
- back-of-envelope：单次派发固定前缀 ≈ 子 system（含 skill 段）+ 10 工具表；
  4 并发下额外内存 ≈ 4 ×（`MemorySession` + registry ≈ 10 个小结构体的 `HashMap`）≈ 数十 KB 量级 ——
  与「4 个并发模型连接」相比可忽略。**真正的资源成本在 provider 侧，不在本进程**。

### 2.4 对比矩阵

| 维度 | A 最小复杂度 | B 可扩展优先 | C 性能/资源优先 |
|---|---|---|---|
| 新增概念数 | 2 方法 + 1 字段 | ≈5 类型/缝 | 2 方法 + 1 字段 + 冻结层 |
| 实现复杂度 | **低** | 高 | 中（冻结层 + 3 条守卫测试） |
| 性能特征 | 与今天同量级；4 并发下 usage 写有锁 | 同 A，多一层间接 | **前缀最稳、零重复构造** |
| 演进灵活性 | 第二个消费者出现时要重构 | **最高** | 中 |
| 风险 | 子表重建依赖 `spec()` 纯函数性（未加测试） | `deadline` 下潜引入库层计时，与「app_loop 单点计时」冲突 | 冻结层若与 `request.tools` 不同源会**静默**不一致 |
| 与 ADR-0014（不设计假想未来） | **一致** | 需论证「第二个消费者已存在」 | 一致 |

### 2.5 推荐：A 为骨架 + 吸收 C 的三项，明确否掉 B

**取 A** 的理由：ADR-0022 已经定死形态 ⇒ 本次是**落地**，落地阶段的价值在「少而准」；
A 的改动面可在一次 `cargo test --workspace` 里验完。

**吸收 C 的三项**（它们不是「性能优化」，是**正确性守卫**）：
1. 父 / 子前缀 `sha256` 锚（P0 指纹）；
2. 「同一子表构造两次逐字节相同」测试（ADR-0022 决策 19 的新抖动面）；
3. 并行用 `buffered`（保序），**不得**用 `buffer_unordered` 再手工重排。

**否掉 B** 的理由（ADR-0014「零消费者 ⇒ 不建」，逐条给证据）：
- ADR-0027 的复核回合**不是**第二个预算消费者 —— 它是**同一个** `Request::Prompt` 里的第二个
  回合，与主回合**共享同一条 deadline**（`apps/coding-agent/src/app_loop.rs:131-133`：
  `deadline` 在 `pending` 循环**之外**算一次）。它今天已被正确覆盖，无须抽象。
- todo（ADR-0024）尚未驱动模型调用 ⇒ 第三个消费者是**假想的**。
- 吸收 B 的一条**已成立**的部分：子预算与父预算**共用同一份 `Config`** —— 这条不需要
  `RunBudget` 类型，只需要「子 runner 调同一批访问器」（§5.2）。

---

## 3. 决策点一：per-tool 超时闸（先于 `subagent` 工具落地）

### 3.1 今天的形态

`crates/ys-loop/src/basic.rs:459-502`：**每一个** `tool.call` 都被
`tokio::time::timeout(Duration::from_secs(ctx.limits.bash_timeout.unwrap_or(300)), …)` 包住；
超时走 `:498-501` 的 `Err(_elapsed) => "Command timed out"`。

两个后果：① 工具没有发言权（`bash_timeout` 是**全局**值，`resolve_limits` 根本不设它 ⇒ 恒 300s，
见 `apps/coding-agent/src/main.rs:365-371`）；② **超时会 drop future** ⇒ 被杀的派发产物不落盘。

### 3.2 裁定

```rust
// crates/ys-tool/src/lib.rs —— 与 Tool::spec()/call() 并列的第三个**默认方法**
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// 单次 `call` 的墙钟上限。`None` = 不声明 ⇒ 回落到 `RunLimits::bash_timeout`，
    /// 再回落 300s（既有行为逐字节不变）。
    fn timeout(&self) -> Option<std::time::Duration> { None }
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}
```

`basic.rs:470` 改为：

```rust
let budget = tool.timeout()
    .unwrap_or_else(|| Duration::from_secs(ctx.limits.bash_timeout.unwrap_or(300)));
```

三条性质：
- **默认 `None` ⇒ 既有工具零行为变化**（今天没有任何工具覆写）。`timeout()` **不进 `ToolSpec`**
  ⇒ 不改 prompt 前缀（与 `is_concurrency_safe()` 同一条纪律）。
- **工具声明优先于 `bash_timeout`**：`bash_timeout` 的语义是「shell 命令多久算卡死」，
  不是「一个工具能跑多久」。若取 `min`，用户把 bash 限到 60s 就会把派发一起砍掉。
- 超时文案改为带工具名与生效秒数（保留 `timed out` 子串）；仓内**无**用例断言
  `"Command timed out"`（仅 `crates/ys-loop/src/basic.rs:499` 一处产出、
  `adapters/tools-basic/src/bash.rs:145,200` 是另一条 bash 自检路径）⇒ 改文案不破测试。

### 3.3 `subagent` 该声明多长 —— ADR-0022 的 1800s 被推翻

ADR-0022 决策 6 写「`subagent` 声明一个长的（如 1800s）」。**这与本轮裁定 5 直接冲突**：
子的墙钟预算 = 主的墙钟预算 = `config.max_turn_duration()`，**默认 48h**
（`apps/coding-agent/src/config.rs:237-240`）。若外闸 1800s，则 30 分钟时外闸先响、
drop 掉子 future ⇒ 长派发被静默杀，**正是决策 6 要修的那个 bug 复现一遍**。

**裁定**：`subagent` 的外闸 = **子自己的预算 + 宽限**，且该值在**构造期**由 `Config` 注入。

- `GRACE`（建议 60s）的用途：让**协作路径先赢** —— 子的 watchdog 到点后经
  `Boundary::Abort` 在**轮边界**停下，返回一个带产物路径的完整 `SubagentOutcome`；
  外闸只用于「连轮边界都到不了」的挂死兜底。
- 代价（如实记账）：`toolset::build_tools(workspace, web)`（`apps/coding-agent/src/toolset.rs:88`）
  需要多收一个参数（承载该时长），连带更新 `prompt.rs:456` 与 `toolset.rs` 内的 ~8 处测试调用。
  **否掉的替代**：写死常量 48h+宽限 —— 用户把 `max_turn_duration_secs` 调大后**静默失效**。

### 3.4 drop future 的缓解（三层，缺一不可）

1. **协作路径在内、外闸在外**（上一条）：正常超时由 watchdog 走轮边界，产物完整。
2. **产物路径由工具先分配、再交给 runner**：外闸真响时，`ToolResult` 仍能带回一个
   **已知存在**的部分转录路径（而不是「什么都没有」）。
3. **runner 增量落盘**（ADR-0022 决策 6 的原话「叠加增量落盘」）：`~/.yushan/subagents/`
   的转录按轮 append，不是收尾时一次写。

---

## 4. 决策点二：并行派发

### 4.1 闸门与信号量

```rust
// crates/ys-component/src/limits.rs —— 与 RunLimits 同 crate（位置见 §7 修正 1）
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;
// RunLimits 新增字段（默认取常量；只有 Step 9 的新分支读它 ⇒ 既有行为逐字节不变）
pub max_parallel_tools: usize,
```

```rust
// crates/ys-tool/src/lib.rs
fn is_concurrency_safe(&self) -> bool { false }   // 默认 false ⇒ 既有工具逐字节不变
```

**准入条件（承重）**：`tool_calls.len() > 1` **且**每一个 name 都在 registry 里命中
**且**每个命中工具的 `is_concurrency_safe()` 为 `true` ⇒ 走并行；否则**原串行路径一字不改**。
⇒ 今天没有任何工具为 `true` ⇒ **既有的一切批次仍走原路径**（这是「逐字节不变」的全部依据）。
本轮只有 `subagent` 覆写为 `true`；混合批次（`subagent` + `bash`）自动退回串行。

### 4.2 为什么是 `buffered` 而不是 `spawn`

`Tool::call` 的 future 借用了 `ctx.registry` / `ctx.boundary` / `ctx.model`（`&dyn`，非 `'static`）
⇒ `tokio::spawn` / `JoinSet` **用不了**（需要 `'static`），除非把整个 registry 改成 `Arc` 并给
`SubagentRunner` 加 `'static` 约束 —— 那是为并行付出的不必要代价。

`futures_util::stream::iter(...).buffered(n)` 满足全部三条需求，且在**单任务内**并发
（工具是 IO 等待型，正是 `buffered` 的适用面）：
- 并发上限 = `n`；
- **保序**（`buffered` 不是 `buffer_unordered`）⇒ `ToolResult` 事件与落盘 `ContentBlock`
  天然按原始 id 顺序；
- 无 `'static` 要求 ⇒ `&dyn Tool` / `ToolContext<'_>` 照用。

依赖：`futures-util 0.3.34` **已在 `Cargo.lock`**（`Cargo.lock:345`）⇒ `ys-loop` 加一条
`futures-util = { version = "0.3", default-features = false, features = ["std"] }` 即可，
不引入新的下载。**否掉的替代**：手写 `poll_fn` + `Vec<Pin<Box<dyn Future>>>`（约 50 行
waker 管理，单测覆盖不到竞态）。

### 4.3 Step 9 的三段形状

```
9a 串行：逐个 → 审批 → emit ToolCall →（denied ⇒ 直接落一条 is_error 结果，不发事件）
9b 执行：all-safe ⇒ buffered(n)；否则原串行
9c 保序：结果按下标回填 Vec<Option<CoreToolResult>>，按原始顺序 emit ToolResult，
        再组装**一条** Role::User 消息（ADR-0001 配对完整性）
```

两个必须保真的既有形状（否则「逐字节不变」是假的）：
- **denied 的工具今天不发 `ToolCall` 事件、也不发 `ToolResult` 事件**，只在回喂消息里留一条
  `is_error`（`basic.rs:428-442` 的 `continue` 在 `emit(ToolCall)` **之前**）。并行路径沿用此形状。
- **`consecutive_errors` 计数**（`basic.rs:513-523`）在 9c 内按原始顺序更新，语义不变。

### 4.4 验收（这是 Step 9 唯一的既有语义变更，必须显式证明）

1. **默认不改**：两个 `is_concurrency_safe() == false` 的 spy 工具同批 ⇒ 事件序列仍是
   `C1,R1,C2,R2`，且第二只在第一只结束后才开始（用时间戳/顺序计数器断言）。
2. **开了才并行**：两个 `true` 的 spy（各睡 200ms）⇒ 墙钟 < 400ms（≈ max 而非 sum），
   事件序列 `C1,C2,R1,R2`。
3. **混合退回**：`true` + `false` 同批 ⇒ 串行路径。
4. **保序**：并行批的 `ContentBlock::ToolResult` 顺序与 `tool_use` 顺序一致（ADR-0001）。
5. **上限**：`max_parallel_tools = 2`、4 个 safe 工具 ⇒ 峰值并发 ≤ 2（计数器断言）。
6. 变异自检：把「每一个都 safe」写成「至少一个 safe」⇒ 用例 3 变红。

---

## 5. 决策点三：子独立预算

### 5.1 形状

```rust
// apps/coding-agent/src/subagent.rs（新）—— app 侧，不是库
pub struct ChildBoundary<'a> {
    own: Arc<QueueBoundarySource>,      // 子自己的轮边界（watchdog 往这里 push(Abort)）
    parent: Option<&'a dyn BoundarySource>,
}
impl BoundarySource for ChildBoundary<'_> {
    fn take(&self) -> Option<Boundary> { self.own.take() }        // **绝不消费父队列**
    fn is_aborted(&self) -> bool {
        self.own.is_aborted() || self.parent.is_some_and(|p| p.is_aborted())
    }
}
```

- `take()` 恒不碰父 ⇒ 用户在子跑动期间的 `Steer` 不会被子吃掉（ADR-0022 决策 5 的原语义）。
- 新增的 `own` 只为**子自己的 deadline**：`tokio::spawn(sleep(budget) → own.push(Abort))`。
  **机制与 ADR-0027 的父预算逐字同构**（`app_loop.rs:351` 附近同样是「到点向本回合的
  边界源 `push(Boundary::Abort)`」）。
  ⚠️ **必须用 `AbortOnDropHandle`（`tokio_util`）或自写 `Drop` 守卫包住该 `JoinHandle`** ——
  `tokio::spawn` 的任务**不随 `JoinHandle` 的 drop 而取消**。外闸（§3，`Tool::timeout`）真响时，
  整个 `tool.call` future 被 drop，若没有守卫，这个任务会**孤儿化**并沉睡到 `budget` 到点
  （子预算 = 主预算 ⇒ 默认 48h）。这是本设计唯一的资源泄漏路径。
- **每次派发新建** ⇒ 满足「`Abort` 永久置位、`BoundarySource` 每回合必须新建」的硬约束。

### 5.2 子 `RunLimits`：各值取自**同一份 `Config`**

| 字段 | 取值 | 备注 |
|---|---|---|
| `max_rounds` | `config.max_rounds()`（默认 `u32::MAX` = 不设） | 与主一致（裁定 5） |
| `context_window` | `config.context_window_for(child_model)`（**新方法**） | 按**子模型**；沿用今天「每回合刷新」的纪律 |
| `bash_timeout` | 与主相同（今天恒 `None` ⇒ 300s 兜底） | |
| `compaction` | `config.compact_policy()` | 与主一致 |
| `fixed_prompt_tokens` | `fixed_prompt_tokens(&child_prompt, &child_specs)` | ⚠️ **必须是子表的值**，否则子会话系统性晚触发压缩 |
| `max_parallel_tools` | `DEFAULT_MAX_PARALLEL_TOOLS` | 子表无 safe 工具 ⇒ 实际恒走串行 |

`Config` 需新增 `context_window_for(&self, model: &str) -> usize` —— 把今天
`context_window()`（`config.rs:202-217`）里的三级查表抽出来，`context_window()` 变成
`self.context_window_for(&self.model)`。这条与 ADR-0022 决策 17 是**同一把刀**
（模型可指定），不重复计入本文四个决策点。

### 5.3 父子预算为何互不污染，且子为何仍有界

- **不污染**：父的 deadline 是 `app_loop` 里的一个 `Instant`（`app_loop.rs:131-133`），
  子的是子 runner 里的 `sleep` + 子边界源；两者只有 `is_aborted()` 一条**单向**读路径。
  子的 `Abort` **不向父 push** ⇒ 子撞自己的预算只让**父回合继续**（服务成 `is_error` 结果）。
- **子为何仍有界**（裁定 4+5 下「子必有界」已废弃，这里是替代保证）：
  父的 deadline **先起算**，故 `父到期时刻 < 子到期时刻`；父到点后在父边界源 `push(Abort)`
  ⇒ 子经 `is_aborted()` 在**下一个轮边界**停下。**父回合预算是子的真实天花板。**
  ⚠️ 落下的两处残余暴露，如实记录：① 若用户把 `max_turn_duration_secs` 设得极小
  （如 `1`），子会与父同样快地到点 —— 这是「取值与主一致」的直接后果；
  ② 「轮边界」意味着一个**正在飞的模型调用**不会被立刻打断（靠 ADR-0028 的模型调用超时兜底），
  这与父今天承担的是**同一份**暴露，不新增。

### 5.4 usage 上卷与「预算独立」不矛盾

预算 = 刹车（独立），记账 = 成本（上卷）。runner 持 `Arc<AtomicU64>` ×4
（输入 / 输出 / 缓存命中 / 缓存未命中），`app_loop` 在**每个 `run_turn_once` 之后** drain 进
`TurnStats`；`TurnStats` 需加一个**不增 `turn_count`** 的记账入口
（今天的 `record` 同时 `turn_count += 1`，`apps/coding-agent/src/status.rs:11-18`）。

---

## 6. 决策点四：成本读数

### 6.1 `Usage` 加两个加法式字段

```rust
// crates/ys-core/src/usage.rs
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)] pub prompt_cache_hit_tokens: u32,
    #[serde(default)] pub prompt_cache_miss_tokens: u32,
}
```

`Add` 同步相加。适配器侧：`ChatUsage`（`adapters/model-openai-compatible/src/response.rs:3-11`）
加同名字段（`#[serde(default)]`），在 `lib.rs:646-652`（非流式/尾包）与流式赋值点各映射一次。

### 6.2 两个契约

- **JSONL 会话契约零改动**：session 存 `Message`、不存 `Usage` ⇒ 加字段不影响落盘与恢复。
- **`--json` 事件流是加法式变更**：`RunFinished { usage }` 多两个字段。
  `apps/coding-agent/tests/stream_usage_fallback.rs` / `budget_exit.rs` / `truncation_guard.rs`
  都只取 `usage` 里的既有键做断言（`stream_usage_fallback.rs:281-286` 等）⇒ 不破。

### 6.3 已知缺口（如实记账）

两个字段是 `u32` 而非 `Option<u32>` ⇒ 对**不返回**缓存字段的 provider，读数为 `0`，
与「真的 0 命中」不可区分。ADR-0022 决策 18 选的就是 `u32` + `#[serde(default)]`
（换取 `Add` / 比较 / 展示三处的零分支）；本轮**沿用**，缺口记录在此。

### 6.4 成本数字全部作废

ADR-0022 §4.3/4.4 的「子 system ~18,100 / 每轮 ~24,630」是在
`CLAUDE.md` 仍被加载、且 skills 未上线的基线上量的。今天两项都变了
（ADR-0019 的 skill 目录常驻 + `RunLimits::fixed_prompt_tokens`）。
**先装上 §6.1 的读数**，再在**本轮内**首次真实派发后回填（见刀 7）—— 没有读数就只能再估一次，且已错三次。
这也是把 P0 指纹做成**字节锚**而非 token 数的原因。

---

## 7. 与 ADR-0022 的四处修正

| # | ADR-0022 原文 | 修正 | 依据 |
|---|---|---|---|
| **修正 1** | 决策 14：`DEFAULT_MAX_PARALLEL_TOOLS` 放 **`ys-loop`** | 必须放 **`ys-component`**（`RunLimits` 所在 crate） | `ys-loop` 依赖 `ys-component`，反向不成立 ⇒ 常量放 `ys-loop` 时 `RunLimits::default()` **无法**引用它，而「单一真值」正是决策 14 的全部意义 |
| **修正 2** | 决策 6：`subagent` 声明「如 1800s」 | `subagent` 的外闸 = **子预算 + 宽限**，构造期由 `Config` 注入（§3.3） | 与本轮裁定 5（子预算 = 主预算，默认 48h）冲突；1800s 会让外闸先响 |
| **修正 3** | 决策 12/18 引用的固定开销数字（18,100 / 24,630） | **作废**，改为「先装读数（§6）、后重算」 | ADR-0022 自己已标注「成本数字待重算」，且 skills 上线改变了基线 |
| **修正 4** | 决策 5：`ChildBoundary` 的 `take()` **恒 `None`** | `take()` 改为 `self.own.take()`（**仍绝不消费父队列**） | 子自己的 watchdog 需要一条可被 `take()` 交还的 `Abort`；`own` 队列只由 watchdog 写入。功能上 `is_aborted()` 探针（`crates/ys-loop/src/basic.rs:95,205`）已足以让子收尾，`own` 是为压缩窗口防御路径（`basic.rs:213-236`）保留的一致形状 |

另有一处**沿用但需显式写下**的推论：`Source::subagent(id)` 缝**不实现**
（ADR-0022 决策 2：子事件不发射 ⇒ 零消费者）。`crates/ys-protocol/src/envelope.rs:17-23`
今天只有 `Source::agent()`，保持原样。

---

## 8. 刀序与验收

> 相对 ADR-0022 的「四刀」重排：**第 1 刀「身份」已不存在**（决策 2 判定子事件不发射）。

| 刀 | 内容 | 依赖 | 关键测试（含变异自检） |
|---|---|---|---|
| **0** | **P0 前缀指纹**：父 `sha256(tools_json + system_prompt)` + 子一份 + **同一子表构造两次逐字节相同** | 无 | 指纹快照；`build_tools` 两次的 `spec()` JSON 逐字节相同。**无 `sha2` 依赖**（`Cargo.lock` 里没有）⇒ 用测试内手写的 FNV-1a 64（锚点的目的是**发现漂移**，不是抗碰撞） |
| **1** | **per-tool 超时闸**（§3）：`Tool::timeout()` + 优先级链 + 文案 | 无 | 声明 600s 的工具不被 300s 杀；`None` ⇒ 既有超时用例全绿 |
| **2** | **成本读数**（§6）：`Usage` 字段 + 适配器解析 + `Config::context_window_for` +（决策 17 连带）`build_model_for` + `Arc<dyn Model>` 按名缓存 | 无 | 适配器解析命中/未命中；`--json` 多两字段；per-name 窗口查表；同 name 两次构造复用同一 `Arc` |
| **3** | **窄能力 trait `SubagentRunner` + 三跳注入**（`AgentPorts` → `RuntimeContext` → `ToolContext`），先挂空实现 | 1 | 假 runner 注入后工具能读到（照 `FakeSummarizer` 先例）；`AgentPorts::new` 签名不变（用 `with_subagent` 链式，照 `RuntimeContext::with_boundary`） |
| **4** | **runner 实现 + `subagent` 工具**：子 `Agent`/`MemorySession`/`NoopEventSink`/`ChildBoundary`/watchdog（`AbortOnDropHandle`）/子 `RunLimits`/增量落盘/**每回合派发总量上限**（§10.1）/usage 上卷（§10.4） | 0,2,3 | 子撞 `max_rounds` ⇒ 父回合**不失败**、`ToolResult{is_error}`；子被 abort ⇒ `Cancelled` + 产物路径存在；usage 出现在父 `TurnStats`；**超派发上限 ⇒ 响亮 `Err`**；`YUSHAN_SUBAGENTS_DIR` 生效、sweep 只清超龄（§10.2）；`model` 名解析失败 ⇒ `Err`、不静默回退（§10.3） |
| **5** | **提示词**：`prompt.rs:107` 条件式派生（父表有派发工具 ⇒ 不说「no subagents」）+ guideline（规模规则/四要素/**并行写目标不相交**/并发槽位数）+ 两份测试拷贝（`prompt.rs:634`、`e2e_skills.rs:90`） | 4 | 父提示含 guideline 且**不含**「no subagents」；子提示**含**该句 |
| **6** | **并行派发**（§4）：`is_concurrency_safe()` + `max_parallel_tools` + Step 9 三段改造 | 可独立于 4 实现（用 spy 验证）；**验收**在 4 之后 | §4.4 六条 |
| **7** | **成本回填**（§6.4）：装完读数 + 跑一次真实派发 ⇒ 把固定前缀 / 每轮的真实数字**写回 §6.4** | 2, 4 | 真实派发的 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` 非 0 且可解释；回填后的数字替换本文件的估算 |

**加第 10 个工具 = 一次预期内的前缀漂移**：`EXPECTED_ORDER`
（`apps/coding-agent/src/toolset.rs:40-52`）从 9 元变 10 元，`subagent` **追加末位**
⇒ 子的 tools JSON 仍是父的字节前缀。刀 0 先锚 9 元、刀 4 后重新锚 10 元。

---

## 9. 已知缺口与风险（明确接受）

1. **子 agent 内部不可观测** —— ADR-0022 决策 2 的少数派偏离，本轮不变（只有落盘转录可查）。
   `Source` 缝保留、不实现。
2. **并行子 agent 的写冲突无人管** —— 本仓无 branch/worktree、无沙箱 ⇒ 只能靠 guideline
   约束「写目标不相交」。**这是并行的直接代价**（裁定 2 强制接受）。
3. **子预算名义独立、实际由父回合 deadline 兜底**（裁定 4+5 的联合推论，§5.3）；
   `max_turn_duration_secs` 设得极小 ⇒ 子同步变短。
4. **`Tool::timeout()` 依赖工具构造期注入的值** ⇒ `toolset::build_tools` 签名变更。
5. **成本读数对未上报缓存字段的 provider 恒 0**（§6.3）。
6. **`futures-util` 成为 `ys-loop` 的直接依赖**（已在 lock，无新下载）。
7. **`Tool::spec()` 必须是纯函数** —— 刀 0 的第 3 条测试是它的守卫；它今天**没有**测试。

---

## 10. 复核补齐（arch-validate，2026-10-09）

> 来源：[`review.md`](review.md)。以下四条是 **ADR-0022 里写过、但首轮复核漏掉的决策**，现补回。
> 它们是设计的一部分，不是「待办」。

### 10.1 每回合派发总量上限（决策 15，**P0**）

每回合派发次数上限（建议 **16**），超限返回**响亮的 `Err`** ⇒ `is_error` 结果回喂父模型 ——
**父回合不因此失败**。ADR-0022 决策 15 的原话理由：上界 = 父 10 轮 × 每轮 4 = **40 次/回合 ≈ 200 万 token**，
正是 Codex #9748（*"instantly drains entire Pro plan"*）那类事故的形状。

**本轮它比写 ADR-0022 时更关键**：`Config::max_rounds()` 默认已是 `u32::MAX`（`apps/coding-agent/src/config.rs:230`），
且裁定 5 让**子也取同一份 `Config`** ⇒「子必有界」被废弃，**这条总量闸是仅剩的代码级成本防线**
（另一根只是 48h wall-clock）。⚠️ 首轮复核同时丢了「子必有界」与这条闸 —— **两层防线一起消失**，
这正是 review 判它为唯一 P0 的原因。

**机制零新增**：runner 本按回合构造（`app_loop` 现建）⇒ 计数器做成 runner 的字段（`AtomicU32`）即
**天然 per-turn**，不需 reset；检查在 runner 内部，工具不用知道。

### 10.2 产物目录的保留策略与测试覆盖口（决策 4）

`~/.yushan/subagents/` 的转录**启动期 sweep 超过 7 天**（只清 `Primary` 档，`Override` 档只写不删）；
理由是它是**派生物**（源子会话已终止），且**悬空指针是优雅降级**（`read` 失败，模型自会 adapt），不是正确性 bug。
配套 **`YUSHAN_SUBAGENTS_DIR`**（照 `YUSHAN_SESSIONS_DIR` / `YUSHAN_FETCHED_DIR`）—— 否则测试会写进用户真实的
`~/.yushan/subagents/`。
⚠️ **不照 `sessions/` 的「永不清理」**（那是既存缺口，不是该复制到新目录的先例）；**本轮不动 `sessions/`**。

### 10.3 fail-closed（决策 15 / D15）

- `model` 名解析失败 / 未登录 provider ⇒ **立即 `Err`**，**绝不静默回退到父模型** —— 否则用户以为换了模型
  其实没换，是本仓明确反对的「静默的谎」。
- 未知工具名 ⇒ 大声拒绝（走既有「tool not found」分支，`crates/ys-loop/src/basic.rs:504-510`）。

### 10.4 「模型可指定」的完整连带（决策 17）

除 `Config::context_window_for(name)`（§5.2）外，**还要**：
① `Config::build_model_for(name)`（现有 `build_model` 是「按 `config.model`」的单例形态，`config.rs:346`）；
② **按模型名缓存实例** —— `OpenAICompatibleModel::new` 内部 `reqwest::Client::new()`，
不缓存则每次带 `model` 参数的派发新建连接池 + TLS 握手。

⚠️ **缓存形状（承重）**：`SubagentRunner::run` 需要 `&dyn Model`，而 `Mutex<HashMap<String, Box<dyn Model>>>`
拿不出稳定引用 ⇒ 缓存须持 **`Arc<dyn Model>`**（克隆 `Arc`、`&*arc` 传入）。
⚠️ **代价（不缓解，只记录）**：`model: Some(不同模型)` 使 prompt cache 全失效（缓存按模型分池，含兄弟间共享）
—— 用户显式选择的必然代价，默认继承时自动消失。

✅ **用户 2026-10-09 裁定：本节四项（含模型实例缓存）全部「本轮做」。**

### 10.5 测试矩阵条目

`docs/design.md` §12 的 `BasicLoop` 行需补 **「并行批闸门与保序」**；`Coding Agent` 行需补
**「subagent 派发：子失败不拖垮父 / usage 上卷 / 每回合派发总量上限」**（AGENTS.md：新模块和接口改动
须能对应上 §12 的条目）。

---

## 11. 参考

- 权威设计：[`../subagent/design.md`](../subagent/design.md)、[ADR-0022](../../adr/0022-subagent-dispatch.md)
- 本轮基线：[`context.md`](context.md)
- 新裁定：[ADR-0029](../../adr/0029-subagent-landing-decisions.md)
- 新增交互面：[ADR-0027](../../adr/0027-completion-check-and-round-budget.md)、[ADR-0028](../../adr/0028-model-call-retry-and-timeout.md)
- 判据：[`../../extension-points.md`](../../extension-points.md)
