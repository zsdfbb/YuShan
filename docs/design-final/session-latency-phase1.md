# 会话耗时治理（第一期）— As-built

> 性质：**实际建成记录**。设计见 [`docs/arch/session-latency/design.md`](../arch/session-latency/design.md)，
> 决策见 [`ADR-0048`](../adr/0048-session-latency-governance.md)，
> 核实报告见 [`docs/arch/session-latency/review.md`](../arch/session-latency/review.md)。
> 日期：2026-10-22

## 一句话

第一期落地**三项**：① `/status` 把「**存活时长**」与「**执行时长**」**分开报**；
② 换模型 / 换思考档位写一条 `model_changed` **debug 事件**（`run_start` 只是启动快照，不反映运行期切换）；
③ 压缩参数调优的**离线验证**已做，结论是「调参不是杠杆」。

第二期落地两刀：**B1** 压缩事件的 token 口径**计入每轮固定开销** ⇒
「压完还剩多少」不再**系统性低估 ~10k**；**B3** 摘要失败**降级到思考内容**
⇒ 消除实测的 3 次「压缩失败」（真实成因**不是** tool-call ⇒ 设计原定的
`tool_choice: none` **解决不了**，已换解法）。

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

## B3 的关键行为（**解法与设计原定不同**）

**真实成因**（实测 3 次压缩失败，真实会话 `1792579023_125340094`）：
3 次的 `model_attempt` 全部 `outcome=success` / `http=200` ⇒ **模型确实回了**。
适配器把 reasoning 走 `ThinkingDelta`、**不进** `ContentBlock::Text`
⇒ 若模型该次**只输出思考、没输出正文**，终态就**没有** `Text` block
⇒ 旧实现报「the model returned no text」。

⚠️ **设计原定的 `tool_choice: none` 解决不了**：
摘要请求本来就 `tools: vec![]`（适配器在空表时**不发** `tools` 字段）
⇒ 模型**不可能**回 tool_call ⇒ 失败与 tool-choice 无关。

**采用���解法**：摘要调用改用**收集型 sink**（`ThinkingFallbackSink`），
正文为空时**降级到思考内容**；**既无正文又无思考**才报失败（B-2 语义保住）。

**为什么降级是安全的**：thinking 至少是**模型为这段历史写的文字**，
比「压缩失败、历史原地不动（并带 92k 上下文重试）」更有用。

## B4 的关键行为（**与 ADR-0017 决策 10 有差异**）

**摘要请求现在复用「活对话前缀 + 末尾一条 user 指令」**：
- `system` = **活对话的 system**（与主调用**逐字相同**）⇒ 前缀不断 ⇒ 命中 KV cache；
- `messages` = **待摘要段**（`0..keep_from`，是主调用消息的**真前缀**）+ **末尾追加的摘要指令**
  （作 `Role::User` 消息 —— **不是** system：system 一旦不同，前缀就断了）；
- `tools` = **空**；`max_tokens` / `temperature` 不设。

⚠️ **与 ADR 决策 10 的差异**：ADR 说要「复用 `tools` + `tool_choice: none`」。
本实现**不复用 `tools`** ⇒ **不需要** `tool_choice`（`ModelRequest` 无此字段）：
摘要模型看不到工具表 ⇒ **不可能**回 tool call ⇒ 不会触发 B-2。

**实测依据**（真实会话 `1792579023_125340094`，8 次压缩调用的 cache 命中）：
前 6 次**只有 0.3%**（186 / 64949）—— 因为旧形状是**独立请求**（system 被换掉 ⇒
第一个 token 就不同 ⇒ 整段未命中）；后 2 次 98.7% / 99.1%（那两次的摘要段恰好是
上次压缩后的保留段 ⇒ 前缀恰好相同）。⇒ **B4 正是补上这 6 次的缺口**。

**`Agent::system_prompt()` getter**（ADR-0007 风格）—— `/compact` 用它取活对话的 system
⇒ 与自动压缩**同一口径**。

## C 组（工具形态，C1–C7）— 全部落地

| # | 措施 | 落点 | 实测依据 |
|---|---|---|---|
| **C1** | `grep` 多目标 `paths` | `adapters/tools-basic/src/grep.rs` | 316 条 bash 含 grep，56 条串了多个 |
| **C2** | `read` 多文件 `paths` | `adapters/tools-basic/src/read.rs` | 10 条 bash 用 `cat a && cat b` |
| **C3** | 冗余 `cd <ws>` **真剥离** | `adapters/tools-basic/src/bash.rs` | 554/560（99%）带前缀 |
| **C4** | 长命令指引**点名**构建/测试 | `apps/coding-agent/src/prompt.rs` | 44 次 `cargo test` 走 bash，`bash_bg` 仅 7 次 |
| **C5** | 连续只读段调度 | `crates/ys-loop/src/basic.rs` + **ADR-0040** | 97% 的多调用轮拿不到并行 |
| **C6** | 结果预算是**可配**的（`YUSHAN_RESULT_BUDGET_BYTES`） | `crates/ys-tool/src/truncate.rs` | 为 A/B 留口子 |
| **C7** | `read` 预算收窄到 **16 KiB** | `crates/ys-tool/src/truncate.rs` + `read.rs` | read 占字节 40.8%；50 KiB 从未撞到 |

### C6/C7 被实测否掉的两个方向（重要）

| 候选 | 实测结论 |
|---|---|
| 降 `read` 的 50 KiB 硬闸门 | **从未撞到**（0/118 次）⇒ 单改它等于没改 |
| 给 `bash` 也加预算 | 中位仅 1,221 B，8 KiB 上限只省 **3%**；而构建日志**尾部**正是错误所在（`bash` 用 `tail()` 保尾）⇒ 风险 > 收益 |

⇒ 只有 `read` 值得收窄（占 40.8% 字节、截断可恢复）。16 KiB 下约 **5%** 的 read 被截断、
省约 **13%** read 字节。⚠️ **收益仍待实跑 A/B**（截断会增加轮数，抵消多少未知）。

### 一个设计陷阱（已钉住）

软阈值提示与硬截断**互斥** ⇒ 预算若等于软阈值（8 KiB），软提示对 `read` 成**死代码**。
故预算必须**严格大于**软阈值（取 16 KiB），由 `const { assert!(..) }` 在编译期钉住。

## 变异测试（35 处，全部被抓到）

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
| M10 | 删掉 thinking 降级分支 | `compact::reasoning_only_response_falls_back_to_thinking` |
| M11 | sink 不收 `ThinkingDelta` | 同上 |
| M12 | 「两者都空」的判定改成永远成功 | `compact::empty_response_without_thinking_still_fails` |
| M13 | `system` 不复用活对话（换回 summary prompt） | `compact::summary_request_reuses_live_prefix_with_trailing_instruction` |
| M14 | 不追加末尾 user 指令 | 同上 |
| M15 | 指令插在**最前**（破坏前缀） | 同上 |
| M16 | 摘要请求带上 `tools` | 同上 |
| M17–M19 | grep 多目标（只取第一个 / 相对化根 / argv 只推一个） | `grep::multi_target_*` + `single_target_*` |
| M20–M23 | read 多文件（只读一个 / 无分隔头 / 遇错中断 / 单文件走多文件分支） | `read::multi_file_reads_*` + `single_file_*` |
| M24–M27 | `cd` 剥离（不剥 / 文案 / 逃生口 / 误剥有位移） | `bash::plan_cd_strip_*` |
| M28–M29 | 长命令点名（删列表 / 只点一部分） | `prompt::background_command_*` |
| M30–M31 | 分段调度（回旧逻辑 / 未命中当安全） | `basic::leading_safe_run_*` + `unknown_tool_*` |
| M32–M35 | read 预算（换回 50 KiB / 等于软阈值 / env 不钳 / 宽于硬闸门） | `read::read_caps_result_*` + `truncate::read_*` |

⚠️ **一个测试设计教训（已踩）**：M4 最初**漏网** ——
`/status` 的测试用 `CodingView::for_test()` **手工设** `executed`，绕过了 `build_view` 的传递链
⇒ 补了 `view::test_build_view_passes_stats_tools_and_session_path` 的 `executed` 断言才盖住。

⚠️ **另一个教训（已踩）**：`model_switch_is_observable_in_debug_log` 最初扫**整个日志目录**找
`model_changed` ⇒ 会命中**上一次运行的残留文件** ⇒ M5 漏网。
修法：**只认文件名含本进程 pid 的文件**。

⚠️ **第三个教训（已踩，且表现为 flaky）**：该用例最初用**默认日志目录**
（`~/.yushan/logs`）⇒ 同进程内**并行测试**的 debug 文件名都是
`debug-<pid>-<ts>.jsonl` ⇒ **撞名互相覆盖** ⇒ 用例 **2/3 次失败**。
修法：照既有 debug 用例的模式，把 `YUSHAN_LOG_DIR` 指到**临时目录**
（`TempDir::new` + `DebugEnv::set`）⇒ 连跑 5 次全绿。

## 第二期及以后：不在本文件范围

见 [`design.md`](../arch/session-latency/design.md) §9 的分期。**第一期只做度量与观测**，
不改变任何执行行为 ⇒ 风险最低，且为后续所有措施**建立了可验收的口径**。

## 已知的下一步（按实测后的优先级）

1. **B2 的算法口径决策**（`keep_budget` 是否含固定开销）—— 第二期开跑前必须定。
2. **C1/C2/C3**（`grep` 多目标 / `read` 目录 / `cd` 剥离）—— 第三期-A。
3. **C6/C7**（工具结果体积预算 / `read` 默认收窄）—— **收益待实跑 A/B 验证**，
   离线重放模型不可信（见 design.md §1.1 结论 4）。
