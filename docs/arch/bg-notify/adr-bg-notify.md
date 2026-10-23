# ADR-0051 — 后台命令完成通知：删除 `bash_poll`，结果由 `Boundary::Deferred` 自动注入

把后台 shell 命令的结果回收方式从**模型显式轮询**（`bash_poll`）改为**完成时自动注入对话**。
`bash_poll` 删除；`bash` 超时不再杀进程，改为过继给后台并同样通知。

> 编号说明：本仓 `docs/adr/` 已排到 0049（`0049-final-answer-boundary-drain.md`）。
> 0050 已被占用为下一待定编号时，本决策取 **0051**。

## Status

proposed（2026-10-23）

## 背景

`bash_bg` + `bash_poll` 落地后，模型被工具契约逼着反复轮询。实测会话
`1792727582_325572909`：

| 指标 | 数值 |
|---|---|
| `bash_bg` 启动 | 16 |
| `bash_poll` 调用 | **68** |
| 返回 `running`（零新信息） | **54（79%）** |
| `wait_seconds = 20` | 60 / 68 |
| 声明等待累计 | **1243 s** |
| 单句柄最多被 poll | **24 次** |

根因：`MAX_WAIT_SECONDS = 20`（`bg_tools.rs:57`）小于典型任务时长 ⇒ 超过 20s 的命令
注定 poll 多次。该上限意图是「别等超过一次模型往返」，实际效果是**在制造往返**。

## 决策

**1. 删除 `BashPollTool`**（`bg_tools.rs:93`）及其专用件（`read_incremental` /
`pending_note` / 增量交付偏移机器）。

**2. 完成时注入**：`BackgroundTasks` 上新增**每回合可绑定的落点**
`deferred: Mutex<Option<Arc<QueueBoundarySource>>>`。既有 100ms sweeper 在 `sweep()`
之后把「已终态且 `notified == false`」的条目 `push` 成 `Boundary::Deferred`，再
`mark_notified`（**顺序不可颠倒**，见 Consequences）。

**3. 落点每回合刷新**：`app_loop` 在新建边界源后 `bind()`，RAII 守卫 drop 时解绑。
**不给 `ToolContext` 加第 5 个挂载点** —— 它只对同回合内完成的任务成立，而 `bash_bg`
的存在理由恰恰是跨回合。

**4. 跨回合滞留**：回合先结束时完成项**留在注册表**，下一回合开头（沿用既有 `carry_over`
位置，`app_loop.rs:1079-1084`）一次性补投。

> ⚠️ **本决策修正了探索阶段的 Q1**。原答「回合结束则不送达」。但删掉 `bash_poll` 后
> 若不滞留，结果就是**静默丢失** —— 那比现状（浪费往返）更糟。故改为滞留 + 补投。

**5. `bash` 超时过继**：`deadline` 到点不再 `kill_process_group`（`bash.rs:304`），
改为登记到收养表（`pgid`, `session`），保留既有管道排空路径，由同一 sweeper 检测退出。
外层 `tool_budget` 到点仍 drop future，但进程组已登记 ⇒ 不漏杀。

## 方案对比

| 方案 | 优点 | 代价 | 结论 |
|---|---|---|---|
| **A 落点挂 `BackgroundTasks`**（采纳） | 约 30 行核心；零新增 trait/crate/协议类型；4 个 crate 零改动 | 机制不外溢，第二个任务类型要重写 30 行 | **采纳** |
| B `ys-protocol` 建通用投递平面（`DeferredSink`/`DeferredOutlet`/`Parking`/`BindGuard`，~200 行） | 新任务类型零接线；顺带消除一处类型泄漏 | 只有一个生产用例时过度设计（B 自己标为「建议砍」）；多一个需与 `SessionSlot` 同步的对象 | 否。待第二个任务类型出现再付 |
| 保留 `bash_poll` 作为可选主动查询 | 兼容既有用法 | 与 guideline 的合批要求结构性矛盾；用户明确要求删除 | 否 |
| Claude Code 式默认全异步 | 彻底消灭阻塞 | loop 严格交替、无「工具未完成」状态，需重写轮次模型 | 否 |
| 回合结束前等待所有后台命令 | 结果必然送达 | 回合可能被拉长到不可控；Ctrl-C 语义复杂化 | 否 |

## Consequences

**正面**
- 消灭 79% 的空轮询。按实测推算该会话可省 **~54 次模型往返 / 约 15 分钟**。
- 工具表 12 → 11 元（少一个 spec + 一条 guideline）⇒ 每轮上下文更小。
- 顺带消除既有矛盾：`bash_poll` 被判 `is_concurrency_safe() == false`（`bg_tools.rs:253`）
  却要求「合批调用」，一批里有它整批退化串行；删除后消失。

**负面 / 取舍**
- **滞留项占句柄表名额**（上限 8）。极端情况下「一直不补投」会挤掉新任务。
- 通知正文进上下文，一次大输出仍可能吃掉可观 token（50 KiB 上限 + 完整路径兜底）。
- 删 `bash_poll` 使「中途看一眼进展」不再可能，需靠 `read` 输出文件。
- **R1 技术债**：机制不外溢。须在 `docs/TODO.md` 记一条，写明触发条件
  「第三个后台任务类型出现时，抽成 `ys-protocol` 的 `DeferredSink`」。
- **行为契约变更**：`bash` 超时不再是 `Err`，既有用例（`bash.rs:592` / `:623`）必须重写。

**实现纪律（R6 窄竞态的三道缓解，缺一不可）**
1. `mark_notified` **在 `push` 之后** ⇒ 竞态从「静默丢失」降为「下回合重投」；
2. 回合开头的补投复用**同一个** `take_completions`，不另写路径；
3. 回合收尾顺序写死：先 `drop(binding)` 再排空事件信道。

**须同步更新的文档与锚点**
- `apps/coding-agent/src/prompt.rs` 的两条 bg guideline
- `apps/coding-agent/src/toolset.rs` 的父/子工具表指纹锚点（12 → 11 元）
- `docs/arch/execution-efficiency/design.md` §4.6 / §4.7
- `docs/design-final/coding-agent-tui.md` 的工具表描述

## 关联

- [`docs/arch/bg-notify/design.md`](../arch/bg-notify/design.md) — 完整设计
- [`docs/arch/bg-notify/context.md`](../arch/bg-notify/context.md) — 现状与实测数据
- [ADR-0017](./0017-compaction-hardening.md) — 决策 12：系统注入走 `User` 角色
- [ADR-0049](./0049-final-answer-boundary-drain.md) — `Boundary` 消费语义
- `crates/ys-protocol/src/boundary.rs:23` — `Boundary::Deferred` 既有定义
- `apps/coding-agent/src/subagent.rs` — 异步 subagent「不需要 poll」的先例
