# ADR-0051 — 后台命令完成通知：删除 `bash_poll`，结果由 `Boundary::Deferred` 自动注入

把后台 shell 命令的结果回收方式从**模型显式轮询**（`bash_poll`）改为**完成时自动注入对话**。
`bash_poll` 删除；`bash` 超时不再杀进程，改为过继给后台并同样通知。

> 编号说明：本仓 `docs/adr/` 已排到 0049（`0049-final-answer-boundary-drain.md`）。
> 0050 已被占用为下一待定编号时，本决策取 **0051**。

## Status

proposed（2026-10-23，rev2）

> **rev2 变更**（据 `docs/arch/bg-notify/review.md`）：
> ① `sweep()` 拆三分段（锁内列收集 / 锁外 fork+渲染 / 锁内置态）；
> ② 投递改**三态** `Delivery::{Pending, InFlight, Delivered}`（`可选重复，绝不丢失`）；
> ③ **Print / JSON 模式不挂后台能力**（本轮范围收窄）；
> ④ 滞留完成项与在跑项**分开计数**。

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
`deferred: Mutex<Option<Arc<QueueBoundarySource>>>`。既有 100ms sweeper 把「已终态且
`Delivery::Pending`」的条目投递成 `Boundary::Deferred`，成功后置 `Delivered`
（状态机见 3b）。

**3. 落点每回合刷新**：`app_loop` 在新建边界源后 `bind()`，RAII 守卫 drop 时解绑。
**不给 `ToolContext` 加第 5 个挂载点** —— 它只对同回合内完成的任务成立，而 `bash_bg`
的存在理由恰恰是跨回合。

**3a. `sweep()` 拆三分段**：锁内只做「状态推进 + 收集待办 + 置 `InFlight`」，**不做 IO、
不 fork**；`kill_group` 与 `render_completion` 在锁外；最后重新取锁落状态。

> 理由：`sweep()` 当前**已在锁内**同步执行 `sh -c "kill -9"`（`background.rs:531` 持锁
> → `proc_group.rs:27` 的 `.status()`）。同一把锁是 `snapshot()` / `spawn()` 的必经之路
> ⇒ 低端设备上会拖慢所有工具调用。新设计若把渲染也放进锁内，等于**锁内读文件**。

**3b. 投递三态**：`Delivery::{Pending, InFlight, Delivered}`，配 tick 修复
（`InFlight` 超 2 tick 回 `Pending`）。

> 理由：「两态 + 顺序不变量」在「取候选 → 渲染 → push → mark」四步链上无正确顺序：
> 先 mark 则 push 前切回合 ⇒ **静默丢失**；先 push 则 mark 前崩溃 ⇒ **重复投递**。
> 三态把方向选成：**宁可重复，绝不丢失**。

**4. 跨回合滞留**：回合先结束时完成项**留在注册表**，下一回合开头（沿用既有 `carry_over`
位置，`app_loop.rs:1079-1084`）一次性补投。

> ⚠️ **本决策修正了探索阶段的 Q1**。原答「回合结束则不送达」。但删掉 `bash_poll` 后
> 若不滞留，结果就是**静默丢失** —— 那比现状（浪费往返）更糟。故改为滞留 + 补投。

**5. `bash` 超时过继**：`deadline` 到点不再 `kill_process_group`（`bash.rs:304`），
改为登记到收养表（`Child`, `session`），保留既有管道排空路径，由同一 sweeper 检测退出。
外层 `tool_budget` 到点仍 drop future，但进程组已登记 ⇒ 不漏杀。

**6. Print / JSON 模式不挂后台能力**：`backgrounds` 的构造从「无条件」改为
「仅 `Mode::Interactive`」。这些模式下 `bash` 超时**保持原行为**（杀进程组 +
`ToolError::Timeout`）。

> 理由：`run_print_mode`（`main.rs:1868`）与 `run_json_mode`（`:1950`）**没有回合循环**，
> `Deferred` 只在 `BasicLoop` 的轮边界被消费；`-p` 是单回合，回合结束即退出 ⇒
> 转后台的命令**结果无人接收**。若不改，`-p` 用户会看到「超时但报成功，然后什么都没发生」。

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
- **可能重复投递**：三态下 `InFlight` 被中断会重投。这是**有意选择** —— 重复对用户可见、
  可自行纠偏；静默丢失不可见。
- **滞留项占名额**：rev2 改为与在跑项**分开计数**（未投出上限 8），不再互斥淘汰。
- 通知正文进上下文，一次大输出仍可能吃掉可观 token（50 KiB 上限 + 路径兜底）。
  补投限流：每回合最多 2 条。
- 删 `bash_poll` 使「中途看一眼进展」不再可能，需靠 `read` 输出文件。
- **`-p` / `--json` 模式无后台能力** —— 超时行为保持现状（明记为范围限制）。
- **R1 技术债**：机制不外溢。须在 `docs/TODO.md` 记一条，写明触发条件
  「第三个后台任务类型出现时，抽成 `ys-protocol` 的 `DeferredSink`」。
- **行为契约变更**：`bash` 超时在 TUI 模式下不再是 `Err`，既有用例（`bash.rs:592` / `:623`）
  必须重写。

**实现纪律（三条，不可省）**
1. **锁内绝不 fork / exec / 读文件**（`try_wait` 除外）—— 见设计 §2.4；
2. `InFlight` 只在段 1 设、段 3 清；段 2 失败 ⇒ 回 `Pending`；
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
