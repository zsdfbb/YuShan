# ADR-0048: 会话耗时治理走「调参与既有机制加固」，不做 PTC

## 状态

提议（2026-10-22）

> 依据：`docs/arch/session-latency/context.md`（实测账单）、`design.md`（候选对比与分期）、
> [`review.md`](../arch/session-latency/review.md)（`arch-validate` 核实结果）。
>
> ⚠️ **本 ADR 经核实修订**：原采纳项里的「补 `TokenLedger`」**已实现**（`fixed_prompt_tokens`
> 已落地并接入压缩触发判定）⇒ 改为「修正压缩事件的 token 口径」；
> 「保留段构成是回弹主因」**被实测推翻**（真实主因是固定开销口径，8/8 次一致低估 ~10k）；
> `cd` 剥离的**判定函数已存在**（`redundant_root_cd`）⇒ 改为复用；
> `bash_bg` **已存在** ⇒ C4 降级为 guideline 引导；
> **收益估算从 −2.0 h 修正为 −1.7 h**。
>
> ⚠️ **再经 A2/V1 离线验证修订**（详见 `design.md` §1.1）：
> **A2（压缩参数调参）实测几乎无效** —— `keep_ratio` 0.10→0.03 只把「距下次压缩触发」
> 从 81 轮延到 89 轮 ⇒ **调参不是杠杆**，A2 降级为低成本微调；
> **增长源实测为「工具结果体积」**（工具结果 516k token vs 模型输出 463k token）；
> **但截断工具结果几乎不省墙钟**（压缩次数仅 6.3→4.8）⇒ 新增的 C6/C7
> **收益待实跑验证，不计入合计**；
> **合计从 −1.7 h 修正为 −1.6 h**。

## 上下文

一次真实会话（`1792579023_125340094`，2026-10-21 晚）进程存活 **15.10 h**，但
**实际执行只有 3.22 h**（687 轮 / 21 回合 / 818 工具调用）—— 其余 11.88 h 是
「用户离开、TUI 空等输入」的正常空闲（已核实 `run_end` 由 `app_loop.rs:346` 的
`flush_debug(..., 0)` 在 UI drop 发送端时写入，`exit_code: 0`）。

**执行期**的时间账单：工具 **41.5%** / 生成 30.3% / TTFT **28.2%**。
三个独立病：上下文膨胀（每次请求 80–96k，压缩后 40 轮又涨回）、工具形态低效
（`bash` 占 68%，其中 56% 是 grep、99% 带 `cd` 前缀）、subagent 同步阻塞
（14 次累计 3,605 s，单次最大 965 s）。

仓内已有两份同类调研（`explore-speed/research.md`、`cost-model.md`），收敛出的关键事实：

1. **同题对照（codex vs 我们）**：每请求输入两边都 ~39k，**差距几乎全部是轮数**（6 vs 33）。
   ⇒ **轮数是乘数**。
2. **`cost-model.md` §8.4 已作废**「1.8 vs 5.5 工具调用/轮」这个指标（数的是调用不是覆盖面）。
3. **PTC（程序化工具调用）是四家中唯一能改变轮次数量级的机制**（dsh `core/tools/src/ptc.ts:1-8`），
   本仓零。
4. **前缀瘦身的天花板是输入 token 的 ~38%** ⇒ 前缀不是主战场。
5. **`cost-model.md` §8.3 的 A/B 已落地**（`Start wide` + 允许 shell 面操作）⇒ 本轮不重复做。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A · 最小复杂度** | 调参 + 既有机制加固：`keep_ratio` 可调、修正压缩事件 token 口径、摘要 `tool_choice:none`、grep/read 增强、`cd` 剥离、长命令后台化、subagent 异步化 | ≈400 行；估算可达 **−53%**（修正自 −62%）；与仓内既有设计重叠最少 | 不解决「模型每轮只发 1–2 个调用」这个根因 |
| **B · 可扩展优先** | 引入 **PTC**（模型写代码调工具，只有 curated 结果进历史）+ 独立任务队列 + metrics 聚合层 | **唯一能改变轮次数量级**；最接近 codex 水平 | ≈2500+ 行；需执行边界 ⇒ **与「不做安全」的硬约束正面冲突** |
| **C · 性能/资源优先** | 只补可观测性 + 修明确性能缺陷：分段并行、压缩回弹机理直击、`cd` 靠提示词 | 风险最低；与 `explore-speed` 未落地项重叠 | **分段并行提速收益近零**（只读工具中位 0.00–0.10 s）；**`cd` 靠提示词已被 ADR-0039 证伪** |

## 决策

**选择「方案 A 为主体 + 从 C 吸收三项」的融合方案**，并**明确不做 PTC**。

采纳：

1. **调参优先** —— `keep_ratio`/`trigger_ratio` 已在 `Config::compact_policy()`
   （`apps/coding-agent/src/config.rs:303`）可配 ⇒ 零代码改动，先离线验证再上。
   ⚠️ **但实测显示调参收益极低**（§1.1 结论 1）⇒ **只作低成本微调，不作主要杠杆**。
2. **修正压缩事件的 token 口径**（**而非新增 `TokenLedger`** ——
   `RunLimits.fixed_prompt_tokens` + `main.rs:1085` `fixed_prompt_tokens()` **已实现**，
   且 `basic.rs:226-228` **已计入压缩触发判定**）——
   但 `ContextCompacted.estimated_tokens_after`（`compact.rs:259`）**只数会话消息**，
   实测 8 次压缩**一致低估 8,785~11,658 token**（均值 ~10,260）
   ⇒ **这才是「压完又涨回 90k」的真实机理**（压完真实占用 = 估算 + 固定开销）。
   连带需决策：**`keep_budget` 的切分是否应含固定开销**（当前不含 ⇒ 压完必然超预算）。
3. **摘要调用加 `tool_choice: none`** —— ADR-0017 决策 5 的最小切片，
   直接消除本次 3 次「压缩失败：模型返回 tool call」（每次失败都带 92k 上下文）。
4. **摘要调用复用活对话前缀** —— ADR-0017 决策 5，同时改善 KV cache 命中。
5. **工具形态增强** —— `grep` 多目标、`read` 目录/glob。
6. **`cd` 前缀在工具层剥离** —— ADR-0039 的「事实回显」实测无效（本次会话前缀率仍 99%），
   该手段**已被证伪** ⇒ 换手段，但**必须带等价性守卫 + 逃生口**。
   **判定复用已有的 `redundant_root_cd()`**（`adapters/tools-basic/src/bash.rs:213`，
   已含 canonicalize 比对、引号处理、语法错误排除）⇒ **只加剥离动作，不重写判定**。
7. **长命令走 `bash_bg`** —— 复用已存在的后台命令机制。
   ⚠️ **修正**：`bash_bg` **已存在**，其 spec 描述里已点名 builds / test suites
   （`bg_tools.rs:146-149`）⇒ **本项主要是 guideline 引导**（「模型不知道该用它」），不是代码工作。
8. **subagent 异步化** —— ⚠️ **修正**：`bash_bg` 与 subagent **不同构**
   （前者是进程组 + 落盘 + sweeper；后者是独立 Agent 实例 + 独立会话 / 事件出口 / 边界源）
   ⇒ **不能简单复用**，需先定形态（`SubagentHandle` 的存储、失败/取消/泄漏三条路径）。

**不做**：

- **PTC / `run_code`** ⇒ **记为长期方向写入 `docs/TODO.md`**（防反复讨论）。
  理由：需执行边界，与本仓「不做安全（沙箱/隔离/权限/审批由上层解决）」的硬约束
  正面冲突；且 2,500+ 行。
- **独立任务队列抽象** ⇒ `bash_bg`/`bash_poll` 已存在（`adapters/tools-basic/src/bg_tools.rs`），
  重复造轮子。
- **分段并行作为提速手段** ⇒ 只读工具是毫秒级，收益近零 ⇒ **降级为「消除退化」**，
  且依赖工具形态改造先落地。
- **`cd` 靠提示词** ⇒ ADR-0039 已证伪。
- **429 退避调整** ⇒ 实测指数退避 + 抖动 + `Retry-After` **全部符合 ADR-0028**；
  9 次退避合计 4.5 s，占执行时长 **0.039%** ⇒ **不改**。
- **`reasoning_effort` 修复** ⇒ **不是功能 bug**：`run_start` 是**启动快照**（本次会话启动时
  provider = `deepseek` ⇒ `supports_reasoning_effort: false` ⇒ `null` 正确）；
  实际请求路径（`main.rs:2693` 的 `reasoning_effort_for`）**每次重建 model 时都重算** ⇒
  中途 `/model` 切到 `opencode-go` 后**确实有下发** ⇒ 真实问题只是
  **`run_start` 不反映运行期换模型** ⇒ 降级为观测性项（O1）。

## 后果

**正面**：

- 预计活跃期墙钟 **−1.7 h（−53%）**（修正自核实前的 −2.0 h），
  其中 subagent 异步化（−40 min）与长命令后台化（−35 min）占大头。
- 全部措施**不动核心契约**（除 `grep`/`read` 参数面），不引新依赖，不破前缀指纹锚
  （⚠️ 但 C1/C2 的参数面扩展**必须**同步更新指纹锚与前缀不变量测试 —— review.md R3）。
- 严格分三期（第三期再拆两批），每期独立可验收、可归因。

**负面 / 放弃的**：

- **轮数的根因不动** ⇒ 与 codex（6 轮 vs 33 轮）的差距**仍在**。
  本设计是「在现有架构内把已知杠杆做完」，不是「达到同类水平」。
- `cd` 剥离有**误判风险** ⇒ 需等价性测试 + `YUSHAN_KEEP_CD=1` 逃生口。
- subagent 异步化引入**生命周期复杂度**（句柄泄漏、结果丢失）⇒ 复用 `BackgroundTasks` 缓解。
- 各项收益**不独立**（`grep`/`read` 增强需与 `prompt.rs` guideline 同步改，
  否则模型可能仍偏好 bash —— `cost-model.md` §8.3 已证「只改一处反而更糟」）。

**未决**：

- **`keep_budget` 是否含固定开销**（算法口径决策，**第二期开跑前必须定**）：
  不含（现状）⇒ 压完真实占用 = keep + fixed，**必然超预算**；
  含 ⇒ 压完真实占用 ≈ `keep_budget`。⇒ 这是设计决策，不是实现细节。
- D2（subagent 工具表约束）**不在本轮** ⇒ 需先走 `arch-design`
  （缓存相干性 D20 + 前缀指纹锚 + D15「提示词不可见 + 执行时拒绝」双实现）。
- 会话内缓存命中率 64% 的原因（`cost-model.md` §10.4）⇒ 本轮只补观测点。
- **轮数差距未被本决策覆盖**：`cost-model.md` §7 的 codex 对照（6 轮 vs 33 轮）是最大的未覆盖面
  ⇒ 本决策的 13 条措施**全部降的是「每轮成本」与「工具形态」**，
  **不能把轮数降到 codex 水平** ⇒ 若 G1 达成不了，下一个战场是 PTC 或 guideline 层的轮数控制。

## 关联

- [`design.md`](../arch/session-latency/design.md) —— 完整设计、对比矩阵、实施分期
- [`context.md`](../arch/session-latency/context.md) —— 实测账单与约束
- `docs/adr/0017-compaction-hardening.md` —— 压缩 ADR（决策 5 = 摘要复用前缀 + tool_choice）
- `docs/adr/0028-model-call-retry-and-timeout.md` —— 重试退避（核对后不改）
- `docs/adr/0039-bash-redundant-cd-fact-echo.md` —— 冗余 `cd`（事实回显已证伪）
- `docs/adr/0022-subagent-dispatch.md` —— subagent 决策（D14 = 能力由工具表决定）
- `docs/design-final/compaction.md` —— 压缩 as-built（遗留 1 = `TokenLedger`）
