# Subagent 回复后如何继续推进 — 改进设计（rev2）

> `/arch-design`（2026-10-25 第二稿）。第一稿见 git history；质量分析见 [`review.md`](review.md)。
>
> **本稿按 review 的 6 条阻塞项重做。** 形态判断不变（**父回合等得起**），
> 但把三处会导致实现即错的地方改掉了，并去掉了一半的实现量。
>
> **与第一稿的实质差异**：
> 1. 判据从「runner 自己的计数器」改为**进程级**（B3）；
> 2. 等待的唤醒源改为 `ctx.subagent` 的 `wait_notified()` —— **不新增 `ToolContext` 挂载点**；
> 3. 取消等待**不需要轮询**（`Abort` 会经 Step 3 探针在下一轮命中，review 此处判断有误）；
> 4. 超时**不引入系统续跑**（去掉 B2 的整串复杂度），改为「让滞留可见」；
> 5. 去掉 `subagent_status` 与增量落盘出本轮（降 P1/P2）。

---

## 0. 一句话

**父回合在「还有子 agent 在跑」时不许收场**；等它回来 → 结果作为**同一回合的下一轮**的输入 →
模型汇报并继续。等不到上限才收场，且**结果不会静默滞留**（会醒一次、明确告诉用户它在等谁）。

---

## 1. 现状（复核后，三条修正）

### 1.1 回合内已经是「新的一轮」

```text
父模型第 N 轮发 ToolUse{subagent_async} + ACK
  ↓
父模型第 N+1 轮（可能直接说"我做完了"）
  ↓ Step 9c 收场前 drain_boundary（basic.rs:769-782）
  ↓ 结果已在边界源 ⇒ Deferred → append 会话 + DeferredResult 事件 ⇒ Consumed ⇒ continue
  ↓
父模型第 N+2 轮：看见结果
```

**这条通路不用改。** 缺的只是「父回合不许提前收场」。

### 1.2 病在回合**之后**

父回合跑完才回来的子 agent：落点已解绑（`app_loop.rs:1243` 的守卫随块 drop）⇒
结果进 `PendingResults` 表（`subagent.rs:234-256`）⇒ **表在回合外无读者**
（`take_pending_results` 唯一生产调用点在 `run_turn_once` 内 `app_loop.rs:1229`，
入口是 `Request::Prompt` `:448`）。

### 1.3 三条对第一稿的修正

| 第一稿的说法 | 实际 | 后果 |
|---|---|---|
| 「等待期间 Ctrl-C 失效，因为 `is_aborted` 不在 Step 9c」 | **错。** `Abort` 入队即置 `is_aborted`（`boundary.rs:117-125`），而等待结束时必然 `continue` → **Step 3 探针（`basic.rs:379`）命中 → `Cancelled` 收场**。最多晚一个 tick | 取消不需要额外的轮询分支；只需让等待能被**结果唤醒**或**短 tick** 唤醒 |
| 「`has_outstanding` 挂 runner」 | **错。** runner 每回合新建（`app_loop.rs:1244`）⇒ 上一回合派的子 agent 不在本回合 runner 视野 | 判据必须是**进程级**的（状态板 / 共享计数器），经 `SubagentSetup` 跨重建共享 |
| 「结果可能被 `drop` 丢失」 | 部分错。`tokio::spawn` 的任务体**保证**在 `execute` 返回后 `deliver`（`subagent.rs:600-616`） | 真正的丢失路径只剩「进程退出」；不必为 `Drop` 兜底单独设计结果交付 |

---

## 2. 核心设计

### 2.1 状态机

```text
        父模型这一轮没有工具调用
                  │
      Step 9c：drain_boundary（既有）
                  │
     ┌────────────┼──────────────┐
   Consumed     Aborted         Empty
     │             │              │
  continue      return Cancelled   ▼
                            还有子在跑？（进程级判据）
                              │            │
                             否 │            │ 是
                              ▼            ▼
                          收场（现状）   【新增】等一拍
                                            │
                              ┌─────────────┼─────────────┐
                        结果到了        到上限        无变化
                              │             │             │
                          continue      收场 +         继续等
                       （新的一轮）   告知「还在跑」    （短 tick）
```

### 2.2 判据：进程级「在跑计数」

**不复用完整状态板**（它是 ADR-0054 的 P1 项），本轮只需要一个**进程级计数**：

```rust
// apps/coding-agent/src/subagent.rs
/// 进程级的「在跑子 agent 数」—— **不是** runner 的字段（runner 每回合新建）。
///
/// 与 `PendingResults` 并列、同样以 `Arc` 经 `SubagentSetup` 跨 `/resume` 重建共享。
#[derive(Default)]
pub struct LiveSubagents {
    running: AtomicUsize,
    /// 任一子 agent 收尾时唤醒等待方（判据从「>0」变成「==0」的那一刻）。
    settled: tokio::sync::Notify,
}

impl LiveSubagents {
    /// 派发成功后调用；返回的守卫 drop 时自减。
    ///
    /// **守卫而不是手工配对**：`ChildTask` 的每一条退出路径（正常 / 错误 /
    /// 组装失败 / future 被 drop）都靠 `Drop` 回归，结构上不可能漏减。
    pub fn enter(self: &Arc<Self>) -> LiveGuard;

    pub fn count(&self) -> usize;
    /// 等到「计数变化」——**不分方向**，调用方自己复核。
    pub async fn notified(&self);
}

pub struct LiveGuard(Arc<LiveSubagents>);
impl Drop for LiveGuard { /* running -= 1; settled.notify_waiters() */ }
```

**为什么守卫而不是 `fetch_add/fetch_sub` 手工配对**：`ChildTask::execute` 有四条退出路径
（`subagent.rs:774` 组装失败 / `:844` Ok / `:873` Err / 被 drop），手工减必然漏一条。

**`notify_waiters` 而非 `notify_one`**：等待方在 `run_turn` 里只有一个，但 `-p` 路径与测试可能并存；
`notify_waiters` 不存 permit，而等待循环是「定时 tick + 唤醒」双源 ⇒ 漏一次唤醒的成本只是
一个 tick（200ms），可接受。**这比 permit 语义简单，且不会有「迟到 permit 让下一回合立即醒来」的怪象。**

### 2.3 等待循环

新函数，与 `drain_boundary` 并列：

```rust
enum SubagentWait { Arrived, Settled, TimedOut, Aborted }

/// 上限取 `min(limits.subagent_wait, 父 deadline 剩余)` —— 见 §2.5。
async fn wait_for_subagents(
    ctx: &mut RuntimeContext<'_>,
    rounds: u32,
) -> Result<SubagentWait, LoopError> {
    let started = Instant::now();
    loop {
        // ① 结果可能已经到了 —— 复用同一条 drain（不另写注入逻辑）。
        match drain_boundary(ctx, rounds).await? {
            BoundaryDrain::Consumed => return Ok(SubagentWait::Arrived),
            BoundaryDrain::Aborted => return Ok(SubagentWait::Aborted),
            BoundaryDrain::Empty => {}
        }
        // ② 全settle 了 ⇒ 收场（可能全部失败 / 被取消）。
        if !ctx.subagent.is_some_and(|s| s.has_running()) {
            return Ok(SubagentWait::Settled);
        }
        // ③ 到上限。
        if started.elapsed() >= ctx.limits.subagent_wait {
            return Ok(SubagentWait::TimedOut);
        }
        // ④ 等一拍：唤醒 or 超时 tick，两者取先到。
        //
        // **不需要 abort 分支**：Abort 到点后本次等待最多再等一个
        // `SUBAGENT_POLL`（200ms）就回到 Step 3 探针 ⇒ `Cancelled` 收场。
        // 若将来探针位置变化，这个 200ms 是兜底上界（见 §5 的测试 T4）。
        let notify = ctx.subagent.map(|s| s.settled_notified());
        tokio::select! {
            _ = async { match notify { Some(f) => f.await, None => std::future::pending().await } } => {}
            _ = tokio::time::sleep(SUBAGENT_POLL) => {}
        }
    }
}
```

**`SUBAGENT_POLL = 200ms` 的职责**：它**不是**为了轮询状态（那是唤醒的活），
而是为了①让 abort 兜底、②让「结果进了边界源但唤醒源不在结果上」这条缝隙有上界
（`QueueBoundarySource::push` 是同步的、不通知任何人，`boundary.rs:119-125`）。

**Step 9c 插入**（`basic.rs:780` 的 `BoundaryDrain::Empty => {}` 处）：

```rust
BoundaryDrain::Empty => {
    if ctx.subagent.is_some_and(|s| s.has_running()) {
        match wait_for_subagents(ctx, rounds).await? {
            SubagentWait::Arrived => continue,     // 有结果 ⇒ 下一轮
            SubagentWait::Aborted => { /* 与既有 Aborted 分支同形：Cancelled 收场 */ }
            SubagentWait::Settled | SubagentWait::TimedOut => { /* 落到正常收场 */ }
        }
    }
}
```

### 2.4 为什么不需要「系统再起一个回合」

| | 回合内等待（本设计） | 系统再起回合 |
|---|---|---|
| 上下文连续 | ✅ 同一个 `RuntimeContext`，模型知道刚才在做什么 | ❌ 新回合重建 `RuntimeContext`（`agent.rs:118-131`），看不到上一回合派发的句柄 |
| 重复注入 | 无（`drain_boundary` 是唯一注入点） | ⚠️ 回合开头补投 + drain 两路 |
| 三入口一致 | ✅ 循环内改动，`-p` / `--json` / TUI 同时生效 | ❌ 只在 `app_loop`（重演 ADR-0041 的缺陷） |
| 协议改动 | **零** | 需要 `NextTurn` 格（ADR-0047，未落地） |
| 新概念 | 一个 `AtomicUsize` + 一个 `Notify` | 跨回合队列 + 「无消息回合」的输入形状 |

### 2.5 上限与两处时间预算的协调

**双计时器问题**：app_loop 的 budget 到点会 `push(Abort)` 并打「budget exhausted」
（`app_loop.rs:1301-1318`）。若 `subagent_wait` > 剩余 deadline，用户会看到「预算耗尽」
但回合其实正常在等。

**修法**：`subagent_wait` 由**装配点**按剩余预算收紧一次：
`RunLimits::subagent_wait = min(config.subagent_wait(), config.max_turn_duration())`。
在 `resolve_limits`（`main.rs:365-371` 的既有单点注入）与 `app_loop` 每回合刷新窗口的地方
（`run_turn_once` 开头，与 `context_window` 同一处）各设一次 —— **同一份真值，不新增传递路径**。

**默认值 30 min**（第一稿的 15 min 偏小，实测最长派发 965 s，`subagent.rs:1234`：
一个 30 min 上限留 2 倍余量；再长就该由用户调 `config.json`）。

### 2.6 超时之后：让滞留**可见**，而不是自动花你的钱

等满上限仍没回来 ⇒ **收场**（不自动续跑）。结果稍后到达时走既有路径进
`PendingResults` 表，等用户下次输入时补投 —— **语义与今天一样**。

但今天的问题是**它静默**。修法极小，且**不引入任何回合**：

```text
子 agent 收尾 → deliver() 未绑定 ⇒ 入表 + pending.notified()（已有 Notify）
                                        ↓
app_loop 空闲 select 多一臂 → 收到唤醒 → 复核 has_pending() →
    推一行 Outbound::Output + 一次 Outbound::View
                                        ↓
TUI：「↓ sa3.1 已返回，将在你下次发言时进入上下文」
```

**为什么不是自动起一轮**：那等于在用户没开口时替他花一次模型调用的钱，且要处理
「合成消息会落盘、`/resume` 回放会把它画成用户行」这个坑（第一稿 H6）。**告知 ≠ 代劳。**

---

## 3. 三个候选（重新对比，因为判据变了）

| | A. 进程级计数 + 回合内等待（**推荐**） | B. 完整状态板 + 等待 | C. 系统再起回合 |
|---|---|---|---|
| 新概念 | 1 个 `AtomicUsize` + 1 个 `Notify` | 状态板 + `ProgressTap` + 事件变体 + 抽屉 | 跨回合队列 + 输入形状 |
| 代码量 | **~180 行 / 4 文件** | ~550 行 / 7 文件 | ~300 行 / 6 文件 |
| 能否报告「在等谁」 | ❌ 只能说「等待 1 个子任务」 | ✅ `等待 sa3.1 · 3m` | — |
| 上下文连续 | ✅ | ✅ | ❌ |
| 三入口一致 | ✅ | ✅ | ❌ |
| 会不会自动花钱 | ❌ 不会（超时只告知） | ❌ 不会 | ✅ 会 |
| 可否平滑升级到 B | ✅ 计数换成板即可，`has_running` 签名不变（§4.3） | — | — |

**取 A。** 理由：用户要的「回复后立即汇报并继续」只依赖 A；B 的额外能力（报告具体是谁、TUI
进度条、增量落盘、状态工具）是**另一个需求**（ADR-0054），不应绑在这一刀上。
A → B 的升级路径已定（`has_running()` 的实现从计数器换成板，签名不变）。

---

## 4. 每一处改动

### 4.1 `crates/ys-component/src/limits.rs`（+6 行）

```rust
/// 收场前等待在跑子 agent 的**上限**。
///
/// 默认 30 min：实测最长派发 965 s，留 ~2 倍余量。到上限即收场（结果稍后到达时
/// 经 `PendingResults` 在用户下次输入时补投，并**明确告知**，不静默）。
/// 装配点按 `min(本值, max_turn_duration)` 收紧一次（避免与父预算双计时器打架）。
pub subagent_wait: Duration,     // 默认 Duration::from_secs(30 * 60)
/// 等待期间的唤醒兜底间隔。**不是状态轮询** —— 只为 abort 与「结果已入边界源」兜底。
pub subagent_poll: Duration,     // 默认 200ms
```

`#[non_exhaustive]` 结构体加字段 ⇒ `Default` impl 更新即可，`..Default::default()` 的调用方不受影响。

### 4.2 `crates/ys-tool/src/subagent.rs`（+18 行，**全部默认实现**）

```rust
pub trait SubagentRunner: Send + Sync {
    // ... run / spawn 不变

    /// 本**进程**范围内仍在跑的子 agent 数。
    ///
    /// **实现方必须读进程级状态，不得读本 runner 自己的计数器** —— runner 每回合
    /// 新建（`make_runner`），上一回合派发、本回合仍在跑的子 agent 不在它的视野里。
    /// 默认 `0` ⇒ 不挂派发的实现方与既有测试 fake **零改动**。
    fn running(&self) -> usize { 0 }

    /// 等到「`running()` 发生变化」—— **不分方向**，调用方必须复核。
    ///
    /// 默认实现永不就绪（配合 `running() == 0` ⇒ 调用方根本不会进入等待）。
    fn settled<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::pending())
    }
}
```

**`settled` 返回 `Pin<Box<dyn Future>>` 而非 `async fn`**：与 `Summarizer` /
`SubagentRunner::run` 的 `#[async_trait]` 不同 —— 这里**不引入 `#[async_trait]`**，
避免给 `ys-tool` 加 tokio 运行时依赖（它现在**没有** runtime 依赖，只有 dev-dependency）。
返回值形状与 `EventSink::emit`（`crates/ys-event/src/sink.rs:28-31`）**逐字同形**，
是仓内既有先例。

**同时修既有契约文档**（review B6）：`subagent.rs:63-69` 现在写「后台任务的存活期不超过创建它的
runner，runner drop ⇒ 取消」。**这与实现相反**（`tokio::spawn` 是分离任务，runner drop 后继续跑，
`subagent.rs:569-577` 有实测记录）。改为：

> 后台任务**不与 runner 同生命周期**（`tokio::spawn` 分离任务）：回合结束后仍会继续跑，
> 结果经 `PendingResults` 跨回合送达。取消靠 `ChildBoundary` 的 `is_aborted` 透传与子预算 watchdog。

### 4.3 `apps/coding-agent/src/subagent.rs`（+45 行）

1. `LiveSubagents` / `LiveGuard`（§2.2）；
2. `SubagentSetup` 加 `live: Arc<LiveSubagents>` 字段，与 `results` 并列以 `Arc` 跨重建共享；
   `AgentFactory` 同步（`assembly.rs:66-70` / `:152` 的现成模式）；
3. `AppSubagentRunner` 持有 `live` 克隆，实现：
   ```rust
   fn running(&self) -> usize { self.live.count() }
   fn settled<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
       Box::pin(self.live.settled_notified())
   }
   ```
4. **`spawn` 与 `run` 成功占额后立刻 `live.enter()`**，守卫移进 `ChildTask` ——
   `ChildTask::execute` 的每条退出路径都由 `Drop` 自减。

> **`enter()` 的位置必须早于解析模型之后、`tokio::spawn` 之前** —— 否则「已 spawn 但未计数」
> 的窗口里父回合可能收场。

### 4.4 `apps/coding-agent/src/app_loop.rs`（+35 行 / 2 处）

1. **每回合刷新 `subagent_wait`**：与 `set_context_window` 同一处（`run_turn_once` 开头），
   取 `min(config.subagent_wait(), 剩余 deadline)`；
2. **空闲分支加一臂**（`app_loop.rs:357-363` 的 `select!`）：
   ```rust
   Wake::SubagentReturned => { /* 复核 has_pending()；推一行 Output + 一次 View */ }
   ```
   用 `subagent.pending_results().notified()`（`Notify`，与 §4.2 的 `settled` 是**两个**
   不同的信号：一个报「有结果待投」，一个报「在跑数变了」）。

### 4.5 `crates/ys-loop/src/basic.rs`（+95 行 / 1 处插入）

`wait_for_subagents` + `SubagentWait` + Step 9c 的 `Empty` 分支（§2.3）。
**不动** `drain_boundary`、不动 `BoundaryDrain` 枚举（review H2 提的「区分 Steer/Deferred」
本轮**不做** —— 理由是等待循环 `continue` 后模型看到插话会自然先响应人，
而多跑一轮的代价在一次等待里最多一次，可接受；若要精确区分，`BoundaryDrain` 需加变体，
影响 4 个匹配点，留 P1）。

### 4.6 提示词（`apps/coding-agent/src/prompt.rs`，+8 行）

`subagent_async` 段（`:321-332`）三处：

1. **删假承诺**：`its result is delivered at the start of your next turn` →
   `you will be woken up when it finishes — you do not need to keep checking`。
2. **删无法遵守的**：`never claim a dispatched sub-agent is still running without checking` →
   `do not claim a sub-agent is still running unless you are waiting for it`（等待期间模型本来就知道）。
3. **加汇报纪律**：`When a sub-agent's result arrives, say in one line what it found and what
   you are doing next, then continue.`

**落点**：必须插在 `subagent` 段**之前** —— `subagent_guideline_block`（`:1169-1179`）用
「成本提醒」当段尾锚。

### 4.7 TUI（`crates/ys-tui-coding`，+12 行）

**本轮只加一行**：`DeferredResult` 的文案从 `↓ {handle} finished: {head}`（`app.rs:442`）
改为 `↓ {handle} 结果已注入会话 · {head}` —— 把「跑完」与「送达」区分开（用户要的第三件事）。

**不做**：状态行等待段、`CodingView.subagents`、转换行 —— 它们需要 `SubagentProgress`
事件与状态板（P1）。

---

## 5. 验收与变异测试

| # | 用例 | 变异 |
|---|---|---|
| **T1** | 父模型答完但子仍在跑 ⇒ **不收场**；子回来后 `continue`，模型下一轮看到结果（事件序断言：`RunFinished` 之前出现 `DeferredResult`） | 让 `has_running` 恒 `false` ⇒ 变红 |
| **T2** | `running() == 0` ⇒ 立即收场，**不睡**（断言墙钟 < 100ms） | 让 `has_running` 恒 `true` ⇒ 变红（睡满上限） |
| **T3** | 到 `subagent_wait` ⇒ 收场 + 结果稍后进表（`pending_len() == 1`） | 把上限判断挪到循环外 ⇒ 变红 |
| **T4** | 等待期间 `Abort` ⇒ **≤ 一个 poll 周期**内以 `Cancelled` 收场 | 删掉 `sleep(SUBAGENT_POLL)` 分支 ⇒ 变红（永不醒） |
| **T5** | 子 agent 组装失败 ⇒ `Drop` 自减 ⇒ 父回合**不挂在等待**（`Settled` 收场） | 去掉 `LiveGuard` 的 `Drop` ⇒ 变红（计数不减） |
| **T6** | 两个并发子 agent：一个先回来 ⇒ `Arrived` + `continue`；另一个仍在跑 ⇒ 下一轮收场前**再等** | 让 `enter()` 只在 `run` 不覆盖 `spawn` ⇒ 变红 |
| **T7** | 超时收场后子 agent 返回 ⇒ 空闲 `select` 被唤醒 ⇒ 收到 `Outbound::Output` 一行 | 删掉 `Wake::SubagentReturned` 臂 ⇒ 变红 |
| **T8** | 默认值：`subagent_wait == 30min`、`subagent_poll == 200ms`；装配后 `min(∈, max_turn_duration)` | 改默认值 ⇒ 变红 |

**测试约束（写进代码注释）**：**不得引入 `tokio` 的 `test-util` feature**
（`crates/ys-tool/Cargo.toml` 与 `crates/ys-event/Cargo.toml` 的 dev-dependencies 都明确禁止 ——
会让 `build` 与 `test` 的 tokio 编译单元不一致，每次切换重编整棵依赖树 7~14s）。
⇒ 等待预算用**毫秒级真实值**（如 `subagent_wait = 80ms`、`subagent_poll = 10ms`），
不用 `tokio::time::pause`。

---

## 6. 成本

| 项 | 量级 |
|---|---|
| 新增代码 | **~180 行**（含测试约 350 行） |
| 改动文件 | **4 个**（`limits.rs` / `ys-tool/subagent.rs` / `app/subagent.rs` / `basic.rs`）+ 2 处小改（`app_loop.rs` / `prompt.rs`）+ TUI 1 行 |
| 新依赖 | **0**（`AtomicUsize` / `Notify` 全在既有依赖里；`ys-tool` **不加** runtime 依赖） |
| 公共 API 破坏 | **无**（trait 方法全带默认实现；`RunLimits` 是 `#[non_exhaustive]`） |
| 每轮固定前缀 | +~120 字节（三条 guideline 改写） |
| 行为变化 | 父回合可能晚于模型收工结束（**有意**），受 `subagent_wait` 与父 deadline 双重约束 |

---

## 7. 本轮不做（明确降级）

| 项 | 为什么降级 |
|---|---|
| `subagent_status` 工具（review H5） | 等待设计生效后，模型不需要「问能不能换任务」—— 它自己会等。省 5 处装配断言 + ~100 行 |
| 完整状态板 / `ProgressTap` / `SubagentProgress` 事件 | ADR-0054 是**展示**需求，与推进解耦；`has_running()` 签名不变 ⇒ 可平滑替换 |
| 增量落盘（ADR-0029 §3.4 欠账） | 独立欠账；`tokio::spawn` 任务体已保交付，不是本需求的阻塞项 |
| 报告「在等谁」（`等待 sa3.1`） | 需状态板。本轮只能说「等待 1 个子任务」 |
| `BoundaryDrain` 区分 `Steer` / `Deferred` | 影响 4 个匹配点；本轮靠「插话后 `continue` ⇒ 模型自然先响应人」 |
| 系统自动续跑 | 见 §2.6：告知 ≠ 代劳 |

---

## 8. 与既有决策的关系

| 既有决策 | 本设计 |
|---|---|
| **ADR-0041**（一个请求恰好一个回合） | **保持**。不增回合，只收紧收场条件；且循环内改动天然三入口一致（正是 ADR-0041 记下的前置条件） |
| **ADR-0047**（`DeliveryTarget::NextTurn`） | **不用** —— 它是「系统再起回合」的协议格 |
| **ADR-0014**（零消费者 ⇒ 不建） | 满足：`LiveSubagents` 的消费者是**循环收场判据**（控制流），不是观测 |
| **ADR-0022** 决策 2（子事件不进父流） | 不变 |
| **ADR-0027**（wall-clock 预算） | 不变；`subagent_wait` 取 min 后不与之冲突 |
| **ADR-0049**（收场前 drain） | 本设计的等待就是它的自然延伸 |
| **ADR-0054**（状态板） | **降为 P1**，但 `has_running()` 的签名与它兼容 ⇒ 届时只换实现 |

---

## 9. 待确认（默认值，可直接施工）

| 项 | 默认 | 可改点 |
|---|---|---|
| `subagent_wait` | **30 min** | `config.json` 的 `subagent_wait_secs`（与 `max_turn_duration_secs` 同层） |
| `subagent_poll` | 200 ms | 常量 |
| 超时后 | **不自动续跑，只告知** | 若日后要自动，见 §2.6 的取舍 |
| 等待期间插话 | 立即结束等待（`continue` 后模型先响应人） | — |
