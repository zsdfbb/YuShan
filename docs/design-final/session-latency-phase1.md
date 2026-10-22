# 会话耗时治理（第一期）— As-built

> 性质：**实际建成记录**。设计见 [`docs/arch/session-latency/design.md`](../arch/session-latency/design.md)，
> 决策见 [`ADR-0048`](../adr/0048-session-latency-governance.md)，
> 核实报告见 [`docs/arch/session-latency/review.md`](../arch/session-latency/review.md)。
> 日期：2026-10-22

## 一句话

第一期落地**三项**：① `/status` 把「**存活时长**」与「**执行时长**」**分开报**；
② 换模型 / 换思考档位写一条 `model_changed` **debug 事件**（`run_start` 只是启动快照，不反映运行期切换）；
③ 压缩参数调优的**离线验证**已做，结论是「调参不是杠杆」。

第二期第一刀落地 **B1**：压缩事件的 token 口径**计入每轮固定开销** ⇒
「压完还剩多少」不再**系统性低估 ~10k**。

## 落点

```
apps/coding-agent/src/status.rs      TurnStats 增 executed: Duration + record_elapsed()
apps/coding-agent/src/app_loop.rs    回合末累加时长（成功与失败两条路径都算）
                                     + model_snapshot() / record_model_change()
                                     + /compact 臂取 agent.fixed_prompt_tokens()
apps/coding-agent/src/view.rs        build_view 透传 stats.executed
apps/coding-agent/src/capabilities.rs /compact 增 fixed_prompt_tokens 参数
crates/ys-tui-coding/src/view.rs     CodingView 增 executed + executed_duration_str()
crates/ys-tui-coding/src/commands.rs /status 增「executed」行
crates/ys-loop/src/compact.rs         execute() 增 fixed_prompt_tokens 参数
                                     + estimated_tokens_before/after 计入它
crates/ys-loop/src/basic.rs          自动压缩调用点传 ctx.limits.fixed_prompt_tokens
crates/ys-runtime/src/agent.rs        Agent::fixed_prompt_tokens() getter（ADR-0007 风格）
```

## 关键行为（实现期定死，勿凭直觉改）

**两个时长口径**：

| 口径 | 来源 | 含义 |
|---|---|---|
| `duration` | `CodingView.session_started.elapsed()` | **存活**：进程起止 |
| `executed` | Σ `TurnStats.executed` | **执行**：各回合实际占用的墙钟 |

**二者之差 = 用户不在时 TUI 空等输入的时长** —— **不是缺陷，是事实** ⇒ 分开报，不合并。
（实测形态：一次会话存活 15.10 h、执行 3.22 h。）

**失败回合也计入执行时长**：模型跑过了就是占用了墙钟 ⇒ 漏掉会让执行口径**小于**真实占用。

**`model_changed` 事件**（O1）：
- **不重发 `run_start`** —— 那是 **run 级**事件（`run` 序号 + `begin_run`），重发会破坏 run 划分
  与 `run_end` 的配对 ⇒ 另起事件类型。
- 载荷：`cause` / `from_provider` / `from_model` / `to_provider` / `to_model` /
  `reasoning_effort`（**实际生效值**，`None` = 该 provider 不接受该字段 ⇒ 请求里不带）/
  `supports_reasoning_effort`。
- **没变就不记**（`/effort` 设成同值不刷事件）。

## B1 的关键行为（实现期定死）

**压缩事件的 token 口径**：`estimated_tokens_before/after` 现在**含每轮固定开销**
（system prompt + 工具表 spec）⇒ 与「真实占用」**同口径**。

**为什么必须含**：那部分**每次请求都重发**却不在会话消息里 ⇒ 只数会话消息会
**系统性低估**（实测 8 次压缩一致低估 8,785~11,658 token，均值 ~10,260）。

**`execute()` 签名变了**（加了 `fixed_prompt_tokens: usize`）—— `pub` API，但
`ys-loop` 内部调用点 + app 侧 `/compact` 都已同步；传 `0` 表示「调用方不知道该值」
（此时数字仍只数会话消息 ⇒ 低估）。

**`/compact` 的固定开销取自 `agent.fixed_prompt_tokens()`**（不是自己算）⇒
与自动压缩**同一口径**；`Agent` 新增该 getter（ADR-0007 风格的只读查询）。

## 变异测试（9 处，全部被抓到）

| # | 变异 | 断言 |
|---|---|---|
| M1 | `saturating_add` → 赋值 | `status::executed_duration_accumulates_independently_of_token_stats` |
| M2 | 删掉 `/status` 的 `executed` 行 | `commands::status_text_reports_both_duration_notions` |
| M3 | 漏掉成功回合的时长累加 | `app_loop::executed_duration_grows_after_a_turn` |
| M4 | `build_view` 的 `executed` 恒 ZERO | `view::test_build_view_passes_stats_tools_and_session_path` |
| M5 | 删掉 `SetModel` 臂的 `record_model_change` | `app_loop::model_switch_is_observable_in_debug_log` |
| M6 | `record_model_change` 永远提前返回 | 同上 |
| M7 | `executed_duration_str` 恒返回 0 | `commands::status_text_reports_both_duration_notions` |
| M8 | `estimated_tokens_after` 不计固定开销 | `compact::estimated_tokens_include_fixed_prompt_overhead` |
| M9 | `estimated_tokens_before` 不计固定开销 | 同上 |

⚠️ **一个测试设计教训（已踩）**：M4 最初**漏网** ——
`/status` 的测试用 `CodingView::for_test()` **手工设** `executed`，绕过了 `build_view` 的传递链
⇒ 补了 `view::test_build_view_passes_stats_tools_and_session_path` 的 `executed` 断言才盖住。

⚠️ **另一个教训（已踩）**：`model_switch_is_observable_in_debug_log` 最初扫**整个日志目录**找
`model_changed` ⇒ 会命中**上一次运行的残留文件** ⇒ M5 漏网。
修法：**只认文件名含本进程 pid 的文件**。

## 第二期及以后：不在本文件范围

见 [`design.md`](../arch/session-latency/design.md) §9 的分期。**第一期只做度量与观测**，
不改变任何执行行为 ⇒ 风险最低，且为后续所有措施**建立了可验收的口径**。

## 已知的下一步（按实测后的优先级）

1. **B2 的算法口径决策**（`keep_budget` 是否含固定开销）—— 第二期开跑前必须定。
2. **C1/C2/C3**（`grep` 多目标 / `read` 目录 / `cd` 剥离）—— 第三期-A。
3. **C6/C7**（工具结果体积预算 / `read` 默认收窄）—— **收益待实跑 A/B 验证**，
   离线重放模型不可信（见 design.md §1.1 结论 4）。
