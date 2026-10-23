# bg-notify — 实现后质量复核（W3）

> 产出：`/sequential-workflow` Phase 3.4 的 deep review。
> 基线 `b73d379`（本次改动前）→ 实现后。
> 上游：[`review.md`](./review.md)（设计评审）、[`design.md`](./design.md)（rev2）。
>
> **说明**：原计划的独立 review subagent 跑 46 分钟后因 provider 的 SSE 流错误中断，
> 未产出结论。本文件由 orchestrator 逐条自查（读实现 + 跑命令 + 变异验证）写成。

## 总判：**通过**

`cargo build --workspace` ✅ ｜ `./scripts/par-test.sh` 39 个二进制全绿 ✅ ｜
改动文件 fmt 干净 ✅ ｜ 无遗留桩 ✅

---

## P0（阻塞项）—— 全部已解决

### P0-1 锁内绝不 fork / 读文件 ✅
`background.rs` 的 `sweep()` 已拆三段：段 1 只做 `repair_inflight` /
`enforce_output_cap`（标记）/ `reap` / `claim_completions`，**不 fork**；
`kill_outside(to_kill)` 与 `deliver(claimed)` 在锁外。
`snapshot()`（`:694`）与 `mark_and_kill_expired()`（`:1032`）同样是「锁内标记、锁外杀」。
`kill_entry` 只在 `kill_outside` 里被调用。

### P0-2 投递恰好一次 / 不丢 ✅
`Delivery::{Pending, InFlight, Delivered}` + `repair_inflight`（`:1074`，超
`SWEEP_TICK * INFLIGHT_TICKS` 回 `Pending`）。
`deliver`（`:1095`）在**同一处**先 `push` 再 `finish_delivery` —— 不存在「状态落了但 push 没发生」。
`claim_completions`（`:1045`）**不取 `InFlight`** ⇒ 重复认领被结构排除。

### P0-3 `bash` 超时转后台 ✅（本轮修复了一处真实缺陷）
`Err(WaitOutcome::TimedOut(_)) if self.backgrounds.is_some()` 分支：
- abort 走 `WaitOutcome::Aborted`，**不进**该分支（仍是杀进程组 + 报错）✅
- `pid` 为 `None` 时返回明确错误，不静默 ✅
- Print/JSON 模式不挂 `backgrounds`（`main.rs:2810` 的 `Mode::Interactive` 判断）⇒ 走原路径 ✅

**发现并修复的缺陷**：watcher 原先调用 `child.stdout.take()`，但管道已被
`out_task` / `err_task` `take()` 走 ⇒ 恒 `None` ⇒ **超时之后的输出全部丢失**。
改为让 watcher 直接接管那两个既有任务（`tokio::join!(out_task, err_task)`）后再 `wait`。
新增 `test_handed_off_command_keeps_output_written_after_the_timeout` 钉住；
变异（回到重新 `take` 管道）已验证变红。

### P0-4 注释与实现一致 ✅
`background.rs` 模块头写明「锁内绝不 fork/exec/读输出文件（`try_wait` 与 `stat` 除外）」，
与实际实现一致。

---

## P1

### P1-5 `snapshot()` 在 `InFlight` 期间看到什么 ✅
`BgSnapshot` **不暴露** `delivery` 字段（`:257`）⇒ 外部观察不到投递态，
`state` 仍是 `BgState` 的终态 ⇒ 无不一致。

### P1-6 会话隔离 ✅
`take_completions(session)` 按会话过滤（`:1052`）。
`test` 覆盖：`after_new_session_the_old_completions_are_not_delivered`。

### P1-7 补投限流 ✅
`BG_CATCHUP_PER_TURN = 2`（`app_loop.rs:84`）；`take_completions(..).take(2)`
⇒ 取出的置 `InFlight` 并被 `finish_delivery` 落状态，**未取到的仍是 `Pending`**。

### P1-8 渲染器真的装上了 ✅
`main.rs:2814` `tasks.set_completion_renderer(ys_tools_basic::render_completion)` —— 
是**真实实现**，不是返回空正文的桩（该桩在实现过程中出现过，已替换）。

---

## P2

### P2-9 文档与实现的分歧（**待回写**）
`design.md` §2.7 的表格有 `is_error` 列，但 `render_completion` 返回 `Message`，
**没有 error 位**。实际行为：失败/被杀只在**正文措辞**上区分。
→ 需在 `design.md` 回写（见 Phase 4）。

### P2-10 `prompt.rs` 与新工具面一致 ✅
guideline 守卫已改为只看 `bash_bg`；`tool_snippet` 删除了 `bash_poll` 分支。
测试 `test` 断言子提示不含 `bash_poll`。

---

## 已知未覆盖

| 项 | 原因 |
|---|---|
| 「锁内不读输出文件」的计数器探针 | 读取发生在外挂的渲染回调（`fn` 指针，黑盒），要测需改业务代码 |
| `BG_CATCHUP_PER_TURN` 的限流 | 逻辑在 `app_loop.rs` 的回合循环体内，未抽成可测函数 |
| watcher 任务在进程退出时的清理 | 进程退出即终结，无泄漏面；未单独测 |

## 改动统计

11 个文件，**+2190 / −1525**（净减约 665 行，主要来自 `bg_tools.rs` 1486→520）。
