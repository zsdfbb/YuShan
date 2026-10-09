# 0029 — Subagent 落地裁定：超时闸形态、并行实现、子独立预算、成本读数

给 [ADR-0022](./0022-subagent-dispatch.md)（subagent 派发的**形态**，accepted 2026-09-23）补上**落地裁定**。
ADR-0022 的七条形态决策（窄 trait 注入 / 子事件不进父流 / 子表 = 父表去掉派发工具且追加末位 /
结果 = 硬截断摘要 + 产物路径 + `StopReason` / 取消 = `ChildBoundary` 只透传 `is_aborted` /
子 usage 上卷 `TurnStats` / 工具名 = `subagent`）**本文不重开**。
本文只做三件事：① 把决策对齐到**当前代码**（`911fd72`）；② 裁定 ADR-0022 之后才出现的交互面；③ 修正 ADR-0022 的三处论断。
完整复核与刀序见 [`docs/arch/subagent-rebaseline/design.md`](../arch/subagent-rebaseline/design.md)。

## Status

accepted（2026-10-09；本轮四项待决已全部裁定为「本轮做」，见 §5）

## 背景

- **ADR-0022 零落地**：`SubagentRunner` / `SubagentOutcome` 在源码里不存在；`subagent` 只作为字符串出现在系统提示的
  能力边界声明（`apps/coding-agent/src/prompt.rs:107`）与两份测试拷贝里。`ToolContext` / `RuntimeContext` / `AgentPorts`
  都没有派发字段。
- **设计后代码库已演进**：[ADR-0027](./0027-completion-check-and-round-budget.md)（单回合预算可配，默认 `u32::MAX`；48h wall-clock 预算；完工复核回合）、
  [ADR-0028](./0028-model-call-retry-and-timeout.md)（模型调用重试/超时）、skills 上线（ADR-0019 ⇒ 常驻 skill 目录进 `RunLimits::fixed_prompt_tokens`）。
  ⇒ ADR-0022 引用的「follow-up 已设计未实现」**现已实现**（`apps/coding-agent/src/follow_up.rs`），其成本数字**前提已变**。
- **本轮（2026-10-09）用户裁定**：① **先复核**、不落地；② **需要并行**；③ per-tool 超时闸**先于** `subagent` 工具落地；
  ④ **子独立预算**；⑤ 子预算**取值与主 agent 一致**；⑥ 成本读数**本轮做**。
- **ADR-0022 自认的前置阻塞项仍未解**：`crates/ys-loop/src/basic.rs:470` 仍用 `ctx.limits.bash_timeout.unwrap_or(300)`
  包住每一个 `tool.call`，且超时会 **drop future** ⇒ 超过 5 分钟的派发被静默杀掉。

## 决策

### 1. per-tool 超时闸（先于 `subagent` 工具落地）

`Tool` 加**默认方法** `fn timeout(&self) -> Option<Duration> { None }`（与 `spec()` / `call()` 并列，**不进 `ToolSpec` ⇒ 不改 prompt 前缀**）。
`BasicLoop` 的超时闸改为优先级链：**工具声明值 → 否则 `RunLimits.bash_timeout` → 否则 300s**。
默认 `None` ⇒ 既有工具**行为逐字节不变**。「工具声明**优先于** `bash_timeout`（不是取 `min`）」是承重的：`bash_timeout` 的语义是
「shell 命令多久算卡死」，取 `min` 会让「把 bash 限到 60s」顺手砍掉派发。

### 2. 并行派发（Step 9 条件并行）

`Tool` 加**默认方法** `fn is_concurrency_safe(&self) -> bool { false }`；`RunLimits` 加字段 `max_parallel_tools`。
**准入条件（承重）**：批次 `len > 1` **且**每一个工具都命中且 `is_concurrency_safe() == true` 才走并行；否则**原串行路径一字不改**。
⇒ 今天没有任何工具为 `true` ⇒ 既有的一切批次仍走原路径（「逐字节不变」的全部依据）。本轮只有 `subagent` 覆写为 `true`；混合批次自动退回串行。

实现用 `futures_util::stream::iter(...).buffered(n)`（**保序**、单任务内并发、不需 `'static`）；**不用** `tokio::spawn` / `JoinSet`
（`Tool::call` 的 future 借了 `&dyn Tool` / `ToolContext<'_>`，非 `'static`）。`ctx.events: &mut dyn EventSink` 不能跨并发任务共享 ⇒
**先串行 emit 全部 `ToolCall`，并发执行，再按原始顺序串行 emit 全部 `ToolResult`**；落盘 `ContentBlock` 顺序同理（ADR-0001 配对完整性）；审批检查提前到并发之前。

### 3. 子独立预算：独立**实例**，取值**与主一致**

子用**全新** `RunLimits`（各值取自同一份 `Config`：`max_rounds` / `max_turn_duration` / `context_window_for(child_model)` /
`compact_policy`；`fixed_prompt_tokens` **必须填子表的真值**）+ **全新** `QueueBoundarySource`（子自己那份）+
`ChildBoundary`（`take()` 只取自己的、**绝不消费父队列**；`is_aborted()` = 自己的 **或** 父的）+ watchdog
（`sleep(budget)` 后向子边界源 `push(Boundary::Abort)`，机制与 ADR-0027 的父预算逐字同构）。**每次派发新建** ⇒ 满足「`BoundarySource` 每回合必须新建」。

⚠️ **裁定 4+5 的联合推论必须显式记账**：父子同值 ⇒ **子没有比父更紧的独立上限**；父的 deadline **先起算** ⇒
**父回合预算是子的真实天花板**（父到点 `push(Abort)`，子经 `is_aborted()` 在下一轮边界停下）。
ADR-0022 写的「子必有界」在「取值与主一致」下**实际废弃**，替代保证就是这一条。

### 4. 成本读数（本轮做）

`ys_core::Usage` 加两个**加法式**字段 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`（`#[serde(default)]`），
`Add` 同步相加；适配器解析这两个字段。**JSONL 会话契约零改动**（session 存 `Message`、不存 `Usage`）；`--json` 事件流是加法式变更
（`RunFinished { usage }` 多两字段，现有消费者的既有键断言不受影响）。`Config` 加 `context_window_for(name)`（把今天 `context_window()` 的三级查表抽出）
—— 与 ADR-0022 决策 17（模型可指定）是**同一把刀**。

⚠️ **成本数字全部作废**：ADR-0022 §4.3/4.4 的「子 system ~18,100 / 每轮 ~24,630」是在 `CLAUDE.md` 仍被加载、且 skills 未上线的基线上量的。
**先装读数**（没有读数就只能再估一次，且已错三次），并在**本轮内**首次真实派发后回填（见 design.md §8 刀 7）。

### 5. 复核补齐：ADR-0022 写入但首轮复核漏掉的四条决策

> 来源：`/arch-validate`（[`docs/arch/subagent-rebaseline/review.md`](../arch/subagent-rebaseline/review.md)）。
> 首轮复核同时丢了「子必有界」（被裁定 5 废弃）与下面第 ① 条 —— **两层成本防线一起消失**，故一并补回。

① **每回合派发总量上限（ADR-0022 决策 15，P0）**：每回合派发次数上限（建议 **16**），超限返回
**响亮的 `Err`** ⇒ `is_error` 结果回喂父模型，**父回合不因此失败**。ADR-0022 的原话理由（父 10 轮 × 每轮 4 = 40 次/回合 ≈ 200 万 token）
正是 Codex #9748（*"instantly drains entire Pro plan"*）的形状；而本轮 `Config::max_rounds()` 默认已是 `u32::MAX`
（`apps/coding-agent/src/config.rs:230`）且子也取同一份 `Config` ⇒ 这是**仅剩的代码级成本防线**。
机制零新增：runner 本按回合构造 ⇒ 计数器做成 runner 字段（`AtomicU32`）即天然 per-turn。

② **产物目录保留策略 + 测试覆盖口（ADR-0022 决策 4）**：`~/.yushan/subagents/` 启动期 sweep 超过 7 天
（只清 `Primary` 档）；配套 `YUSHAN_SUBAGENTS_DIR`。**不照 `sessions/` 的「永不清理」**（既存缺口，本轮不动 `sessions/`）。

③ **fail-closed（ADR-0022 决策 15 / D15）**：`model` 名解析失败 / 未登录 provider ⇒ **立即 `Err`**，
**绝不静默回退到父模型**；未知工具名 ⇒ 大声拒绝。

④ **「模型可指定」的完整连带（ADR-0022 决策 17）**：除 `context_window_for(name)` 外，还要 `build_model_for(name)`
与**按模型名缓存实例**；缓存须持 **`Arc<dyn Model>`**（`SubagentRunner::run` 需要 `&dyn Model`，
`Mutex<HashMap<String, Box<dyn Model>>>` 拿不出稳定引用）。

> ✅ **用户 2026-10-09 裁定：上述四项全部「本轮做」** —— 含 ④ 的模型实例缓存与 ① 的派发总量上限，
> 以及 ④ 的连带项「成本读数装上后**本轮内**首次真实派发回填数字」（见 design.md 刀 7）。

### 6. 对 ADR-0022 的四处修正

| # | ADR-0022 原文 | 修正 | 依据 |
|---|---|---|---|
| 1 | 决策 14：`DEFAULT_MAX_PARALLEL_TOOLS` 放 **`ys-loop`** | 必须放 **`ys-component`**（`RunLimits` 所在 crate） | `ys-loop` 依赖 `ys-component`，反向不成立 ⇒ 常量放 `ys-loop` 时 `RunLimits::default()` 引用不到它，「单一真值」失效 |
| 2 | 决策 6：`subagent` 声明「如 1800s」 | `subagent` 的外闸 = **子预算 + 宽限**，构造期由 `Config` 注入 | 与本轮裁定 5（子预算 = 主预算，默认 48h）冲突；1800s 会让外闸先响、drop 掉子 future ⇒ **复现它自己要修的 bug** |
| 3 | 决策 12/18 引用的固定开销数字 | **作废**，改为「先装读数、后重算」 | ADR-0022 自标「待重算」，且 skills 上线改变了基线 |
| 4 | 决策 5：`ChildBoundary` 的 `take()` **恒 `None`** | `take()` 改为取**子自己**那份（仍绝不消费父队列），以便 watchdog 的 `Abort` 能被交还 | 功能上 `is_aborted()` 探针已足以让子收尾；`own` 队列是为压缩窗口防御路径（`crates/ys-loop/src/basic.rs:213-236`）保留的一致形状 |

## Considered Options

三个候选（同一组决策点的三种哲学）：

- **A 最小复杂度**（**取**）：2 个 trait 默认方法 + 1 个 `RunLimits` 字段；Step 9 就地分叉；子预算直接调 `Config` 访问器，**不加 `Budget` 类型**。
- **B 可扩展优先**（**否**）：把预算升为一等类型 `RunBudget { deadline, rounds_left, dispatches_left }` 穿过 `ys-component → ys-loop`；把工具批收成 `ToolBatchExecutor`；成本用 `CostLedger`。
  否的理由（ADR-0014「零消费者 ⇒ 不建」）：ADR-0027 的复核回合**不是**第二个预算消费者 —— 它与主回合**共享同一条 deadline**（`apps/coding-agent/src/app_loop.rs:131-133` 在 `pending` 循环之外算一次），今天已被正确覆盖；todo（ADR-0024）尚未驱动模型调用 ⇒ 第三个消费者是假想的。
- **C 性能/资源优先**（**吸收三项**）：启动期冻结父子 spec 与 system prompt；`buffered` 保序；`AtomicU64` 记账；三条守卫测试。
  其中三项被吸收 —— 它们不是性能优化，是**正确性守卫**：父/子前缀 `sha256` 锚（P0 指纹）；「同一子表构造两次逐字节相同」（ADR-0022 决策 19 的新抖动面）；并行必须用 `buffered`、**不得**用 `buffer_unordered` 再手工重排。

## Consequences

- **全为加法式**：`ys-tool` +2 trait 默认方法 +1 字段；`ys-component` +1 常量 +1 字段；`ys-runtime` +1 字段；`ys-core` `Usage` +2 字段。
  **零破坏性变更、零 tokio 越界、零依赖方向变化。**
- ⚠️ **Step 9 是本 ADR 唯一动既有执行语义的地方**，其行为变更被 `is_concurrency_safe()` 默认 `false` **完全闸住**（准入条件里的「每一个」）。
- `Source` / `Envelope` / `AgentEvent` / `ys-tui-coding` / 三个事件消费者**零改动**（ADR-0022 决策 2：子事件不发射 ⇒ `Source::subagent(id)` 不实现）。
- **已知缺口（明确接受）**：① 子 agent 内部不可观测（只有落盘转录）；② 并行子 agent 的写冲突无人管（本仓无 branch/worktree、无沙箱，只能靠 guideline「写目标不相交」）；
  ③ 子预算名义独立、实际由父回合 deadline 兜底；④ `Tool::timeout()` 依赖构造期注入 ⇒ `toolset::build_tools` 签名变更；
  ⑤ 成本读数对未上报缓存字段的 provider 恒 `0`（`u32` 而非 `Option<u32>`，与「真的 0 命中」不可区分）；⑥ `futures-util` 成为 `ys-loop` 直接依赖（已在 `Cargo.lock`，无新下载）；
  ⑦ `Tool::spec()` 必须是纯函数 —— 今天**没有**测试守卫它。
- ⚠️ **质量分析补回的一处**（`/arch-validate`，见 §5①）：**每回合派发总量上限是本轮仅剩的代码级成本防线** ——
  因为 `max_rounds` 默认已放宽到 `u32::MAX`、且子取同一份 `Config`（子自身无独立上界）。**不要把它当作可选优化删掉。**
- ⚠️ **watchdog 必须用 `AbortOnDropHandle`（或自写 `Drop` 守卫）**：`tokio::spawn` 的任务不随 `JoinHandle` 的 drop 取消；
  外闸（`Tool::timeout`）真响时若无守卫，子预算 watchdog 会孤儿化并沉睡到 `budget` 到点（默认 48h）。这是本设计唯一的资源泄漏路径。

## 参考

- 形态设计：[ADR-0022](./0022-subagent-dispatch.md)、[`docs/arch/subagent/design.md`](../arch/subagent/design.md)
- 本轮复核：[`docs/arch/subagent-rebaseline/context.md`](../arch/subagent-rebaseline/context.md)、[`docs/arch/subagent-rebaseline/design.md`](../arch/subagent-rebaseline/design.md)
- 新增交互面：[ADR-0027](./0027-completion-check-and-round-budget.md)、[ADR-0028](./0028-model-call-retry-and-timeout.md)
