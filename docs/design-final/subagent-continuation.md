# Subagent 回复后继续推进 — As-built

> 性质：**实际建成记录**。设计见 `docs/arch/subagent-continuation/design.md`（rev2），
> 决策见 `docs/adr/0055-subagent-continuation.md`，质量分析见
> `docs/arch/subagent-continuation/review.md`。
> 日期：2026-10-25

## 一句话

**父回合在还有子 agent 在跑时不许收场** —— 等它回来、把结果作为**同一回合的下一轮**的输入，
模型汇报后继续；等到上限才收场，且结果**不会静默滞留**。零新依赖、零协议变体、零公共 API 破坏。

## 交付物

| 路径 | 角色 | 新建？ |
|---|---|---|
| `crates/ys-loop/src/basic.rs` | `wait_for_subagents` + `SubagentWait` + Step 9c 插入。**唯一控制流改动点** | 改（+95 实现 / +230 测试） |
| `crates/ys-component/src/limits.rs` | `subagent_wait`（30 min）/ `subagent_poll`（200 ms） | 改（+50） |
| `crates/ys-tool/src/subagent.rs` | `SubagentRunner::running` / `settled`，**两个都带默认实现** | 改（+39） |
| `crates/ys-runtime/src/agent.rs` | `Agent::set_subagent_wait` / `subagent_wait`（getter 供取 min） | 改（+17） |
| `apps/coding-agent/src/subagent.rs` | `LiveSubagents` / `LiveGuard`；`PendingResults::notify` + `has_pending` + `notified` | 改（+245） |
| `apps/coding-agent/src/app_loop.rs` | 每回合收紧等待上限；空闲态 `Wake::SubagentReturned` | 改（+401） |
| `apps/coding-agent/src/main.rs` | `-p` / `--json` 路径也收紧等待上限 | 改（+11） |
| `apps/coding-agent/src/assembly.rs` / `config.rs` | `subagent_live` 跨重建共享；`subagent_wait_secs` | 改（+27） |
| `apps/coding-agent/src/prompt.rs` | 删假承诺、加汇报纪律 | 改（+43） |
| `crates/ys-tui-coding/src/app.rs` | `↓ finished` → `↓ 结果已注入会话` | 改（1 行） |

**没有新增文件。** 全部落在既有模块里。

## 承重的三个不变量

### 1. 判据必须是进程级

`LiveSubagents { running: AtomicUsize, settled: Notify }` 以 `Arc` 经 `SubagentSetup`
跨 `/resume` 重建共享（与 `PendingResults` 逐字同形）。

**为什么**：`AppSubagentRunner` 每回合新建（`app_loop.rs` 的 `make_runner`）⇒ 上一回合派发、
本回合仍在跑的子 agent 不在它的视野里。读 runner 自己的计数器会恒为 `0`，父回合立刻收场。

`synthetic` 验证：删掉 Step 9c 的 `has_running_subagents` 判断 ⇒
`final_answer_waits_for_running_subagents` 与
`parent_waits_for_slow_subagent_within_the_same_turn` 同时变红。

### 2. 计数必须有出口（RAII 守卫）

`LiveGuard` 进 `ChildTask`，`Drop` 时自减并 `notify_waiters`。

**为什么**：`ChildTask::execute` 有四条退出路径（组装失败 / `Ok` / `Err` / future 被 drop，
`subagent.rs:774` / `:844` / `:873`）。手工 `fetch_sub` 必漏一条，而漏减会让父回合钉死到
`subagent_wait` 上限。

`synthetic` 验证：删掉 `LiveGuard::drop` 的 `fetch_sub` ⇒ `live_guard_returns_the_slot_on_drop` 变红。

### 3. 唤醒必须存 permit

`PendingResults::wake` 用 **`notify_one`**，不是 `notify_waiters`。

**为什么**：消费端（app 循环空闲分支）是事件驱动的，大部分时间阻塞在 `request_rx.recv()` 上、
并不挂在 `notified()` 上。`notify_waiters` **不存 permit** ⇒ 结果在那段间隙里落地时唤醒落空，
「已返回」提示要等下一条结果（可能永远不来）。

已接受的代价：permit 可跨回合残留 ⇒ 空闲等待可能「假醒」一次；消费端用 `has_pending()`
复核，空则无动作。

`synthetic` 验证：换回 `notify_waiters` ⇒ `pending_results_wakeup_survives_no_waiter` 超时失败。

## 关键路径

```
父模型给出最终答复（无工具调用）
  └─ Step 9c：drain_boundary
       ├─ Consumed  → continue（既有）
       ├─ Aborted   → Cancelled 收场（既有）
       └─ Empty
            └─ 【新增】has_running_subagents()?
                 ├─ 否 → 收场（既有行为，逐字节不变）
                 └─ 是 → wait_for_subagents
                          ├─ drain_boundary → Consumed  → continue（结果成了下一轮的输入）
                          ├─ running() == 0              → 收场（Settled）
                          ├─ elapsed >= subagent_wait    → 收场（TimedOut）
                          └─ 否则 select!{ settled() | sleep(subagent_poll) }
```

**取消不需要额外分支**：`Abort` 入队即置 `is_aborted`，等待返回后必然 `continue` ⇒
下一轮开头就是 Step 3 探针 ⇒ `Cancelled` 收场。取消延迟上界 = 一个 `subagent_poll`。

## 超时之后

**不自动续跑**，但**不静默**：结果进 `PendingResults` ⇒ `notify_one` 唤醒 app 循环空闲分支
⇒ 推一行 `Outbound::Output`「↓ 后台子 agent 已返回；它的结果会在你的下一次发言时进入上下文」
+ 一次 `View`。

**为什么不做自动续跑**：那等于在用户没开口时替他花一次模型调用，且要处理「合成消息会落盘、
`/resume` 回放会把它画成用户行」这个坑。**告知 ≠ 代劳。**

`synthetic` 验证：删掉 `Wake::SubagentReturned` 的告知分支 ⇒
`late_subagent_result_is_announced_while_idle` 挂 5s 后失败。

## 实现期发现的两个真 bug

### 1. `notify_waiters` 丢通知

初版用 `notify_waiters` 并在注释里断言「多一次/少一次唤醒只是晚一拍」—— **错的**。
详见上文不变量 3。改 `notify_one` + 补测试。

### 2. 装配期设置被静默覆盖

`app_loop` 每回合用 `set_subagent_wait(config.subagent_wait().min(deadline_remaining()))`
**直接赋值** ⇒ 把装配期（`AgentBuilder` / 测试）设好的更紧上限静默抬回默认 30 min。

改成取三者最小，并给 `Agent` 加对称 getter：

```rust
let wait = ctx.config.subagent_wait()
    .min(ctx.deadline_remaining())
    .min(ctx.agent.subagent_wait());   // ← 不覆盖已设值
```

这不是测试便利问题：任何在 `AgentBuilder` 上显式设过 `subagent_wait` 的调用方都会被覆盖。

## 测试

新增 8 个用例，7 个做过变异验证。

| 用例 | 钉住什么 | 变异验证 |
|---|---|---|
| `final_answer_waits_for_running_subagents`（`ys-loop`） | 有子在跑 ⇒ 不收场、多跑一轮、结果进会话 | ✅ |
| `final_answer_returns_immediately_without_running_subagents` | 无子在跑 ⇒ **不白等**（墙钟 < 100ms） | ✅ |
| `wait_times_out_and_closes_the_turn` | 到上限如实收场、`StopReason` 不被污染 | — |
| `parent_waits_for_slow_subagent_within_the_same_turn`（app 端到端） | **单次请求**内父模型多跑一轮并看到结果 | ✅ |
| `late_subagent_result_is_announced_while_idle` | 超时收场后空闲态主动告知 | ✅ |
| `live_guard_returns_the_slot_on_drop` | 守卫归零 | ✅ |
| `pending_results_wakeup_survives_no_waiter` | `notify_one` 的 permit 语义 | ✅ |
| `pending_results_has_pending_matches_the_queue` | `has_pending` 与 `take_pending` 口径一致 | — |

**测试约束**：不得引入 tokio 的 `test-util` feature（会让 build 与 test 的 tokio 编译单元
不一致，每次切换重编整棵依赖树 7~14s）。等待预算用毫秒级真实值。

## 已知限制

| 限制 | 说明 |
|---|---|
| **不报告「在等谁」** | 计数器只答「几个在跑」，说不出 `sa3.1`。要它需要状态板（ADR-0054，未实现） |
| **`-p` / `--json` 无空闲告知** | 那两种模式「跑完即退出」，没有空闲态。超时后的结果只能留在表里，进程退出即丢 |
| **`BoundaryDrain` 不区分 `Steer` / `Deferred`** | 等待期间用户插话会让模型多跑一轮（一次等待里最多一次）。要精确区分需给枚举加变体、影响 4 个匹配点 |
| **无 `YUSHAN_*` 环境变量** | `subagent_wait` 只在 `config.json` 的 `subagent_wait_secs`（与 `max_turn_duration_secs` 一致） |

## 与设计的偏差

| 设计写的 | 实际 | 原因 |
|---|---|---|
| 15 min 上限 | **30 min** | 实测最长派发 965 s，15 min 会切断合法长派发 |
| 四道闸含「abort 探针」 | 无独立探针 | review 复核：`Abort` 经下一轮 Step 3 命中，只需等待能被唤醒 |
| `BoundaryDrain` 区分两种事件 | 未区分 | 影响 4 个匹配点，收益不足以本轮做 |
| 完整状态板 + 进度事件 + 状态工具 | **全部未做** | 降为 P1/P2（见 ADR-0054 的 Status 段与 `review.md`） |
