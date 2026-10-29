# task-latency-optimization — 设计方案

- **日期**：2026-10-29
- **状态**：proposed
- **上游**：`docs/arch/task-latency-optimization/context.md`（杠杆 L1–L19）、
  `docs/reports/2026-10-29-task-latency-analysis.md`（测量与归因）
- **决策记录**：[`adr-latency-levers.md`](adr-latency-levers.md)
- **已确认取舍**：墙钟优先；允许结构性改动；允许改默认值，但须有对照读数。

---

## 1. 问题与目标

同一任务比 Codex / 另一 harness 慢 2–5 倍。已有测量把差距定位到**轮数 × 每轮固定延迟**：

| 指标（会话 `1793181762`） | 值 |
|---|---:|
| 轮数 / 工具调用 | 555 / 626 |
| 主循环模型耗时 | 54.0 min |
| 工具执行耗时 | 15.2 min |
| 每轮中位耗时 | 4.49 s（TTFT 2.23 s + 生成 ~2.26 s） |
| 每轮输入中位 | 254,313 token（缓存命中 99.3%） |
| 每轮工具调用数 | 1.13 |
| **每条 `bash` 的 shell 步骤数（均值）** | **4.17** |

目标：**在不回退质量的前提下，把主循环模型耗时砍掉 20–35%**（54 min → 35–43 min），
token 作为约束（不得显著变差，方向应更省）。

⚠️ **不承诺「2–5 倍」**。轮数是 555；即使 `batch` 合并 35%，仍剩 ~360 轮 × 4.5 s ≈ 27 min。
要再往下压必须动主轮思考强度（`daily/2` 实测关思考轮数 10→5、墙钟 −37%），
但那有 `fact/1` 质量退化读数，不在本设计的默认改动里（见 §10）。

### 1.1 两处必须纠正的既有认知

1. **「tools/轮 = 1.1」不等于「模型没在批处理」**。每条 bash 管道平均 4.17 步
   （573/575 含 `&&` / `|` / `;`），模型的有效操作数已 ≈4.2/轮。它只是把批处理放在了
   **bash 内部**。因此「让外层一条消息发多个工具」的收益被高估；真正稀缺的是
   **跨轮依赖层数**。
2. **固定前缀 ≈11.9k token，不是 21.4k**。实测首请求 input 为 11,856–11,985；
   其中工具表 10,216 B ≈ 2.55k token、`AGENTS.md` 18,480 B ≈ 6.78k token、
   skill 目录 ≈1.3k token、基础提示 ≈1.2k。裁工具描述的墙钟收益在实测中不可分辨。
3. **TTFT 在 20k–300k 之间几乎平坦**：本会话分桶中位 2.22 s（20–50k）→ 2.46 s（200–300k），
   到 300–500k 才跳到 **3.20 s**。因此压缩上下文只在 >300k 的那 217 轮上有明显收益，
   它**不是**墙钟主杠杆，主要价值在 token 与尾部。
4. **1M 窗口是当前模型的标配**（用户 2026-10-29 确认），不是兜底默认或解析错误。
   因此 A5 是**主动封顶**——模型能吃 1M，我们选择只喂 200k，换取更低的 prefill 与 token；
   不是修一个错报的窗口。

---

## 2. 同类调研（Step 2）

| 问题 | pi | Codewhale | deepseek-harness | 结论 |
|---|---|---|---|---|
| 一条消息多工具 | 运行时默认并行；无通用提示要求 | 运行时按只读/权限分桶并行 | 原生批 + 并发上限 10 | **都不靠提示词** |
| 代码执行式批处理 | `codemode`（QuickJS/WASM 沙箱，默认不激活） | `execute_tools` | `run_code` (PTC) | 都需要沙箱；本仓不做沙箱 |
| bash 查看器检测 | 无 | 无 | 无 | **无同类先例** |
| 压缩触发 | `W − 16k`（绝对 reserve），保留 20k | `80%` 且受可花费上限钳制 | `min(W×0.8, W−O−65536)`，保留 16% | **都用绝对约束**，不是纯比例 |
| 轮数护栏 | 无 | step 默认无上限；**80% 处 soft landing** | 无 turn budget；goal 256 轮 | soft landing 有先例 |
| 验证结果复用 | 未找到 | 只有 RunReceipt | 未找到 | **无同类先例** |
| rounds/TTFT 读数 | 只有 token/cost | **有 TTFT / steps / tools 台账** | 有 session-stats、实验 UI | 度量有先例 |

**两条对方案有实质影响的调研结论**：

1. **结果内注记改变行为缺少正面证据**。本仓自己的 ADR-0039（冗余 `cd` 回显）登记了
   「无效就删」判据，最终 master 落地的是 `f6885fa`「执行前剥离」——即靠 harness 消除，
   而不是靠提示或回显。⇒ 任何「加一句话让模型改」的杠杆都必须带**可回滚门禁**。
2. **绝对压缩上限有同类依据**，可以直接改默认；**轮数软提醒有同类依据**
   （Codewhale 的 step budget soft landing），可以直接加。

---

## 3. 候选方案（节内嵌）

### 候选甲 · 最小复杂度

一句话：不新增 crate / trait / 工具，只加「一条读数 + 一次提醒 + 一个上限」。

- **W0 读数**：复用 `RoundClock`（已吃全部事件）累加 `TurnTotals`，回合末输出一行；
  `e2e_bench` 把 `Reading` 落盘 JSON。
- **W1 压轮数**：① 修 `read` schema 的 `required:["path"]` 与 `paths` 矛盾；
  ② 重写批处理 guideline、删掉「整目录可用 shell 循环」的口子；
  ③ 复用 `narration_nudge` 通道，新增「连续单调用轮」计数提醒（默认阈值 4）。
- **W2 压成本**：`Config::effective_context_window() = min(声明窗口, 上限)`，
  默认上限 200k；压缩触发点、保留量、回合工具预算全部随之下移，`CompactPolicy` 一行不改。
- **W4 收尾**：只在 guidelines 与 `sequential-workflow` 加边界文案。
- **明确不做**：命令分类、目录/glob、只读 bash 并发、按调用类型分档、证据存储、L7/L15。

### 候选乙 · 机制优先

一句话：三个带真实消费者的设施 + 一张配置面。

- **`TurnAdvisor`**：`ys-component` 里的同步 trait（第 5 个可选挂载点），
  在 Step 5 向接线器索取一条**纯常量**提醒，以 `Role::User` 追加到**本次请求末尾**，
  不落 session。两个消费者：bash 查看器纠正、轮数软护栏。
- **`EvidenceLog`**：记录 `(树指纹, 命令, 结果, 时间)`，在 git 收尾类命令的结果里附
  「上次验证在同一棵树上通过」的 notice。进程内存、`/new` 清空。
- **`read glob`**：给 `read` 加 `glob` 参数，把「读一个目录下全部 markdown」收回结构化工具；
  同时删掉 shell-loop 逃生口。
- **压缩**：`CompactPolicy.trigger_cap: Option<usize>`，绝对值上限受 `keep+1` 下限约束；
  回合工具结果上限改为配置（默认 60k）。
- **度量**：`RunReading` + 三出口（TUI `/status`、`-p` stderr、debug `run_reading`）+ bench JSON。
- **演进路径**：advisor 后续可加「重复失败命令」「批处理提醒」两条规则。

### 候选丙 · 性能/资源优先

一句话：把探索动作从「每轮一条 shell 管道」改成「每轮一个可表达依赖的可执行计划」。

- **`batch` 工具**：纯数据 JSON DAG（**不执行模型生成的代码**，无沙箱前提下唯一合法的组合能力）。
  只调度 `read` / `grep` / `find` / `ls` 四个并发安全工具；1–6 步；参数可用
  `$<id>.paths` / `$<id>.text` 引用更早步骤；`on_error=continue|stop`。
  新增 `ToolInvoker` 纯 trait 与 `ToolContext.tools` 挂载点。
- **上下文阶梯**：`C = min(声明窗口, hard_cap)`；`0.75C` 确定性降级（只缩 ToolResult）、
  `0.85C` LLM 摘要（保留 20%）；**工具结果创建时封顶**，历史写入后不再重写。
- **轮数软/硬限**：soft 60 / hard 120，走 narration 同通道。
- **子 agent 不等**：主答复给出后 5 s 宽限，其余跨回合投递。
- **验证台账**：`VerifyingBash` 包装器，同命令同树指纹命中直接回放。
- **明确不做**：显式 prompt cache 字段（命中已 99.3%）、固定前缀裁剪（收益不可分辨）。

---

## 4. 对比矩阵

| 维度 | 甲 最小 | 乙 机制 | 丙 性能 |
|---|---|---|---|
| 新增概念 | 0（2 个 `usize` + 1 个配置） | 1 trait + 1 证据类型 + 1 工具参数 | 1 trait + 1 工具 + 1 阶梯 |
| 改动面 | `ys-loop` / app / `Config` | `ys-component` / `ys-loop` / `ys-tool` / app | `ys-tool` / `ys-loop` / `tools-basic` / app |
| 改工具前缀 | 是（`read` schema + prompt），一次 | 是（`read` schema + prompt），一次 | 是（新增 `batch` + prompt），一次 |
| 墙钟收益（主循环 54 min） | **4–13 min** | **6–15 min** | **6–15 min** |
| token 影响 | 明显变省 | 明显变省 | 明显变省 |
| 采纳风险 | 中（纯提示） | 中（提示 + 结果注记） | **高**（新工具要模型改用） |
| 无同类先例的机制 | 无 | 证据层 | `batch` DAG（接近 pi codemode） |
| 回滚成本 | 极低 | 中（advisor 不挂即回滚） | 中（撤 schema + 回滚一次前缀） |
| 与既有不变量冲突 | 无 | 无 | **`batch` 绕过审批边界**（只读工具，可接受但须写明） |

**关键判断**：

- 甲的「单调用提醒」收益被 §1.1 第 1 条打折——模型已经在 bash 内批处理，外层调用合并的
  空间有限。它的真实价值是**低风险地基**，不是主杠杆。
- 乙的 `EvidenceLog` 与 bash 查看器纠正**都没有同类先例**，且 ADR-0039 提供了
  「注记未改变行为」的负面先例；本期不采纳，留作数据出来后的第二刀。
- 丙的 `batch` 是唯一能直接压缩**依赖层**的结构性杠杆，但采纳风险最高。
  它必须带明确的终止判据，不能默认上线。

---

## 5. 推荐方案：两刀 + 一道门禁

> **第一刀（低风险、可立刻验收）→ 看读数 → 第二刀（结构性，带终止判据）**

### 第一刀 · 地基与低风险杠杆（≈600 行）

| 项 | 机制 | 默认 | 净墙钟（估） | 回滚 |
|---|---|---|---|---|
| **A1 度量** | `RoundClock` 扩展 `TurnTotals`，回合末一行；bench 落盘 JSON | 开 | 0（验收前提） | 无（纯输出） |
| **A2 `read` schema 修正** | `required` 移除 `path`，与 `paths` 对齐（代码已支持） | 修正 | 0（修矛盾） | 无 |
| **A3 批处理指引重写** | 明确「独立调用放一条消息」+ `paths` 示例；删掉「整目录用 shell 循环」的口子 | 开 | 含在 A4 | 文案 |
| **A4 单调用提醒** | 连续 N 轮只有 1 个工具调用 ⇒ 下一轮请求尾部注入常量提醒（复用 `narration_nudge` 通道） | 阈值 4 | **2–5 min** | `0` 关闭 |
| **A5 有效窗口上限** | `Config::effective_context_window() = min(声明窗口, context_cap_tokens)`，默认 **200_000** | 200k | **1.5–2.5 min**（已扣压缩成本） | `0` = 不封顶 |
| **A6 轮数软提醒** | 回合内到 60 / 120 / 240 轮各注入一次「汇报进展或收敛」提醒；不中断 | 60 | **0–3.5 min**（高度不确定） | `0` 关闭 |
| **A7 任务边界** | guidelines：只做用户要求的事；必要步骤免问，其余先提议并等待；含「纯 git 操作不重跑未变工作区的验证」 | 开 | **0.5–2.5 min**（外加省掉跑偏的整段工作） | 文案 |

**为什么 A5 用「有效窗口」而不是给 `CompactPolicy` 加 `trigger_cap`**：
`keep_budget` / `trigger_at` / 回合预算三处都从同一个窗口派生，改一处等于同时改三处；
引入 `min()` 后三者自动一致（200k 窗口 ⇒ 触发 170k、保留 20k、回合预算 50k），
且与 pi 的「保留 20k」绝对口径对得上。`CompactPolicy` 的算法与不变量一行不改。

### 门禁（第一刀之后）

用 `e2e_bench` 的 `fact/1` / `exp/t1` / `daily/2` 各跑 ≥3 次。**先判失败，再判继续**
（评审 R9：原文把「做砸了」和「做完了但不够」混在同一条里）：

**① 失败判据（任一成立 ⇒ 回滚对应项，不进第二刀）**

| 读数 | 含义 |
|---|---|
| 质量位低于 Q+ | 提醒/压缩损害了正确性 |
| `rounds` 比基线**上升** | 提醒诱发重读或拆管道 |
| `cache_hit` 下降 > 1 个百分点 | A5 的降级频率过高（R4） |

**② 继续判据（质量位保持、且轮数降幅 < 15% ⇒ 启动第二刀）**

| 读数 | 含义 |
|---|---|
| `rounds/turn` 降幅 < 15% | 第一刀没解决轮数问题 |
| `single_call_round_ratio` > 50% | 单调用轮仍是主体 |
| `rounds` 未上升、质量保持 | 可以叠加结构性杠杆 |

**③ 停止条件**：若第一刀已达到目标（`rounds` 降幅 ≥ 15% 且质量保持），
**不启动第二刀**——`batch` 的冷启动与采纳风险不值得再加。

### 第二刀 · 结构性杠杆（≈800–1,000 行，带终止判据）

**B1 `batch` 工具（受限 JSON DAG，不执行代码）**

- 只调度 `read` / `grep` / `find` / `ls`；1–6 步；参数引用只允许指向**更早**步骤 ⇒ 无环。
- 无引用步骤并发（≤4），有引用步骤串行。
- **v1 声明 `is_concurrency_safe = false`（评审 R7）**：否则「外层 4 路并行 × 内层 4 路」
  = 16 个进程（`grep`/`find` 各自 spawn），低配机器会抖动。作为串行屏障，它与其他工具
  的关系退化为「先后执行」，不影响轮数收益。若实测内层串行成为瓶颈，再改为
  `true` 并把内层并发压到 `max_parallel_tools / 2`。
- 输出预算：单步 8 KiB、整批 16 KiB；超限头部截断 + 续读指针。
- 事件：会话里只出现**一个** `ToolCall` / `ToolResult`；内层步骤不进事件流（v1 取舍）。
- **挂载位置（评审 R6）**：追加段**末尾**，即
  `bash_bg + subagent + subagent_async + batch`。理由：
  - 父表仍以子表 JSON 为字节前缀（`toolset.rs:610` 起）；
  - **子 agent 不提供 `batch`** —— 子 agent 的 `ToolContext` 不挂 `ToolInvoker`，
    若让它看见一个「可见但不可用」的工具，正是本仓反对的形态
    （`toolset.rs:647` 的前缀契约与 `subagent` 的能力边界）；
  - 需同步修改：`EXPECTED_ORDER`（`toolset.rs:40-50`）、
    `assembly.rs:379-397` 的尾断言、`toolset.rs:680` 的指纹锚。
- **终止判据**：3 次 bench 后 `batch` 调用占比 < 20% ⇒ 回滚该工具（代价只是一次冷启动）。

**B2 重复验证的三层防止（不是缓存）**

目标是**让模型不必重跑**，而不是让重跑变便宜。三层按成本递增：

1. **规则（A7，已在前置）**：guidelines + `sequential-workflow` 明确「纯 git 操作不重跑
   自上次验证后工作区未变的测试」。这是唯一零机制成本的一层，但单靠它不可靠
   （ADR-0039 的先例：提示对行为的影响不稳定）。
2. **保留证据（核心）**：让验证结果在**压缩**后仍然可见。
   现状：回合级降级只作用于「超过 2 KiB / 40 行」的大结果
   （`turn_budget.rs:44-52` 的 `needs_elision`），验证结果通常只有几行，
   **本来就不会被降级**；真正的风险是**压缩**——摘要可能不保留「某条测试已通过」。
   理由：模型重跑往往不是因为习惯，而是因为「刚测过」这条证据被压缩掉了。

   ⚠️ **不能写成「keep 区间优先包含验证结果」**（评审 R2）：`CompactPlan` 只有
   `summarize: Range` 与 `keep: Range` 两个**连续区间**
   （`crates/ys-loop/src/compact.rs:44-57`、`:84-134`），验证结果散布在历史中间，
   无法单独挑出。两条可行路径：

   - **v1（低成本）**：在压缩摘要的 system prompt 里加一个固定小节
     `## Verification`，要求「逐字保留已通过的命令、时间与树指纹」。
     不改任何数据结构，只改 `DEFAULT_SUMMARY_SYSTEM_PROMPT`
     （`crates/ys-component/src/compact.rs:80` 起）。
   - **v2（若 v1 的摘要漏项）**：`execute()` 生成摘要后，把匹配 `verify_commands`
     的最近一条结果**原样追加**为一条独立 `Role::User` 消息。
     要给 `CompactOutcome` 加字段，成本更高，但可靠。

   判定沿用配置的 `verify_commands` 前缀表，**不硬编码任何工具链**（默认 `./scripts/`）。
3. **硬拦截（兜底）**：若模型**仍然**发出同一条验证命令，且树指纹未变、上次成功，
   harness **拒绝执行**并返回一行事实：

   ```
   [not run: `./scripts/par-test.sh` already passed on the same working tree at 12:03 \
    (HEAD abc1234, 0 uncommitted, 0 untracked). No files changed since.]
   ```

   - **不回放旧输出**：模型需要的是「通过了」，不是上一次的日志。
   - 树指纹 = `git rev-parse HEAD` + `git status --porcelain=v2 -uall`
     + 未跟踪文件 `(path, size, mtime_ns)`；每次调用前现算（10–20 ms）。
   - 只在 git 仓库启用；非 git 直接关闭；只认成功记录；宁可漏拦截，不可错拦截。
   - 仍花一轮模型往返，但不花 30–50 s 的命令时间。

**与缓存的区别**：缓存是「重跑但很快返回旧输出」；这里第 2 层让模型不再想重跑，
第 3 层让重跑不执行。两者都不回放旧结果。

---

## 6. 详细设计（第一刀）

### 6.1 A1 度量

**落点**：`apps/coding-agent/src/round_clock.rs`（已接收 `RoundStarted` / `ToolCall` /
`ToolResult` / `RoundUsage`，`round_clock.rs:139-305`）。

```rust
#[derive(Default, Clone, Debug)]
pub(crate) struct TurnTotals {
    pub rounds: u32,
    pub tool_calls: u32,
    pub ttft_ms: u128,
    pub gen_ms: u128,
    pub tools_ms: u128,
    pub input_tokens: Vec<u32>,   // 供中位数；有界（≤ 轮数）
    pub cache_hit: u64,
    pub cache_miss: u64,
    pub cache_reported: bool,
}

impl RoundClock {
    /// 定形、整数、grep 友好；不含浮点以避免漂移。
    pub(crate) fn summary_line(&self, turn: u32) -> String;
}
```

输出形态（示例）：

```
turn=3 rounds=27 tools=41 tpr_x100=151 ttft_ms=61200 gen_ms=118400 tools_ms=9800 \
input_med=121344 cache_pct=98
```

**出口**：

- `-p` / `--json`：终局事件后写 **stderr**（stdout 保持干净，与既有 `report_channel_stats` 同位置）。
- TUI：走 `Outbound::Output`（不新增协议变体），回合末一行。
- debug：`flush_debug` 时落一条 `ev="run_reading"`（schema 加法），供 bench 与离线分析。
- `e2e_bench`：`Reading` 增加 `tpr_x100` / `input_med` / `single_call_rounds` 字段，
  每条结果写 `bench-summary.json`（`YUSHAN_BENCH_OUT` 可指向固定路径）。

### 6.2 A2 `read` schema 修正

`adapters/tools-basic/src/read.rs:21-38` 的 `required` 当前是 `["path"]`，但
`properties` 里有 `paths`，且 `call` 在 `:72-75` 已支持只给 `paths`。
改为不强制 `path`（两者至少一个，由 `call` 的 `InvalidInput` 兜底）。
**零语义变更**，只消除 schema 与实现的矛盾。

### 6.3 A3 批处理指引

`apps/coding-agent/src/prompt.rs:227-245`：

- 保留「read-only calls in a batch run in parallel」。
- 明确给出形状：**一次消息里发 2–4 个彼此独立的调用**；`read` 用 `paths=[...]`、
  `grep` 用 `paths=[...]`。
- 删除「a whole directory or a glob — one shell loop is fine」：改为
  「读整目录先 `ls`/`find` 拿路径，再用 `read paths=[...]`」。
- **不新增术语、不新增条目**（前缀成本按字节计）。

### 6.4 A4 单调用提醒

复用 `narration_nudge` 的既有形态（`crates/ys-loop/src/basic.rs:452-464`、`:635-650`），
新开一个**独立**计数：

- 本轮 `tool_calls.len() == 1` ⇒ `single_call_rounds += 1`；`>= 2` ⇒ 归零。
- **不因模型说话而归零**（与 narration 的判据相反，需在字段注释写明）。
- 到阈值 ⇒ 下一轮 Step 5 注入常量 `SINGLE_CALL_NUDGE_PROMPT`，阈值翻倍。

```rust
// crates/ys-component/src/limits.rs
pub const DEFAULT_SINGLE_CALL_NUDGE_ROUNDS: usize = 4;

#[non_exhaustive]
pub struct RunLimits {
    /// 连续「只有 1 个工具调用」的轮数阈值；到阀后在**下一轮请求**末尾补一条瞬时提醒。
    /// `0` = 关闭（核心默认）⇒ 老调用方逐字节不变。app 默认 4。
    pub single_call_nudge_rounds: usize,
}
```

文案（纯常量、English、不含错误词）。**只陈述代价，不规定形状**（评审 R1）：

```rust
const SINGLE_CALL_NUDGE_PROMPT: &str =
    "Your recent messages each carried a single tool call, and every message is a full model \
     round-trip. If the next lookups are independent of each other, cover them in one message \
     or one call — `read` accepts `paths=[...]` and `grep` accepts `paths=[...]`, and \
     independent read-only calls sent together run in parallel. Keep dependent steps separate.";
```

**为什么不写「一次发 4 个调用」**：模型每条 `bash` 已平均含 4.17 个 shell 步骤，
它本来就在批处理。硬性规定「一条消息多个调用」可能诱导它把一条 4 步管道拆成 4 个
`bash`（`Segment::Serial`，`basic.rs:1195`），多花工具 token、每次重启 shell，
拆错还白拿 3 条无用结果。末尾的「Keep dependent steps separate」是必需句，
否则会诱导它对有依赖的步骤做无效批处理。

**A4 必须先单独 pilot**：只开 A4、不开其他项，测 `rounds` 与 `bash` 调用数是否同时下降；
若 `bash` 调用数上升而 `rounds` 不降，说明模型在拆管道，应回滚该文案。

### 6.5 A5 有效窗口上限

**落点**：`apps/coding-agent/src/config.rs:275-307`（四级窗口解析）。

```rust
pub const DEFAULT_CONTEXT_CAP_TOKENS: usize = 200_000;

impl Config {
    /// 压缩与回合预算实际使用的窗口：`min(context_window(), context_cap_tokens)`。
    /// `context_cap_tokens == 0` ⇒ 不封顶。
    pub fn effective_context_window(&self) -> usize;
    pub fn effective_context_window_for(&self, model: &str) -> usize;
}
```

生产调用点全部换成 effective：

- `apps/coding-agent/src/main.rs:1048`（`limits.context_window`）
- `apps/coding-agent/src/main.rs:1053`（`max_turn_tool_result_tokens = effective/4`）
- `apps/coding-agent/src/app_loop.rs:798`（`/compact`）
- `apps/coding-agent/src/app_loop.rs:1340`（每回合 `set_context_window`）

**展示口径（评审 R3）**：`agent.context_window()` 直接读 `limits.context_window`
（`crates/ys-runtime/src/agent.rs:160-162`），TUI `/status` 由它渲染
（`apps/coding-agent/src/view.rs:158`）。因此设了上限后界面会显示 200k。
**这是有意的**，但必须同时显示声明值，避免用户误以为模型只有 200k：

```
context: 200k / 1M (capped)        // 或 "1M model, 200k working window"
```

实现上 `view.rs` 同时读 `config.context_window()`（声明）与 `agent.context_window()`
（effective），两者不等时才加 `(capped)`。`main.rs:4434/4440` 的启动日志同口径。

`config.json` 新键 `context_cap_tokens`（顶层标量，`Raw → 校验 → 规整` 既有形状，
`config.rs:33-53` / `:593` 一带）。子 agent 克隆父 `limits`，上限随之下传，无需单独接线。

**为什么是 200k（用户确认「至少 200k」，且这是 Claude Code 量级的工作窗口）**：

- 模型能吃 1M，但实测 TTFT 在 20k–300k 平坦，喂更多不会更快，只会更贵；
- 200k 给出 keep 20k、压缩周期 150k ≈ 8–9 个回合，一个 24 回合会话约 2–3 次摘要，
  成本已计入 §7.1；
- 128k 太激进：保留量按 10% 缩放只剩 12.8k，而本会话每回合工具输出增长约 17k，
  会退化成「每回合压一次、每次烧 25–45 s」，净收益为负；
- 300k 是可选上界：压缩次数降到 1–2 次、细节损失更小，但 token 基本不降
  （中位输入仍在 250k 附近）。**若 A/B 显示 200k 诱发重读（`rounds` 反弹 >10%），
  先把它调到 300k 再判断是否回退。**

**该上限的收益主要是 token（−40%~−50%），墙钟只有 1.5–2.5 min。**

### 6.6 A6 轮数软提醒

与 A4 共用注入通道，再开一个计数：

- 阈值 `soft_round_advice_at`（默认 **60**），触发后 ×2（60 → 120 → 240）。
  依据：`fact/1` 常态 10–33 轮、`exp/t1` 常态 2–3 轮，60 不会命中正常任务；
  而失控回合是 65 / 93 / 166 轮，三条分别在第 1–3 次提醒内被覆盖。
- 每回合重置；不产生事件、不落 session、不中断回合。
- **注入优先级（评审 R5）**：`narration > A4 > A6`，**每轮至多注入一条**。
  三条提醒都走 `request_messages` 末尾，若同时置位会出现「一次请求带三条指令」——
  token 浪费且互相稀释。实现上按优先级取第一个 `Some`，其余标志**保留到下一轮**
  （不清零），下一轮再判。需单测钉住「三条同时置位时只发一条、且顺序稳定」。

```rust
const SOFT_ROUND_ADVICE: &str =
    "Harness hint: this turn has run for many rounds without converging. If you are still \
     exploring, stop and report: what is done, what is blocked, and what you need from the \
     user. Do not keep probing without telling the user where things stand.";
```

依据：Codewhale 的 step budget soft landing（`turn_loop/preparation.rs:138-154`）；
ADR-0027 当时把「软警告」记为本轮不做，但那是在还有 `max_rounds` 硬上限与复核回合的语境下；
今天 `max_rounds` 默认 `u32::MAX`，唯一兜底是 48h wall-clock，软提醒的缺口已成立。

### 6.7 A7 任务边界：只做用户要求的事，多余动作先提议

用户观察到的核心症状：要「commit+push」，模型先重跑测试、再 check 一遍。
本质是**任务边界失守**——把「实现阶段的收尾仪式」带进了「纯 git 操作」。
A7 因此从一条收尾规则扩展为**任务边界规则**，仍然只用一条 guideline（前缀按字节计）：

```text
Do exactly what the user asked; do not add work they did not request.
Necessary steps are allowed without asking: reading/searching to understand the task,
running checks on code you changed, and fixing breakage your own change caused.
Everything else must be proposed first — unrelated refactors, new files or docs,
dependency changes, broad reformatting, extra test suites, or committing/pushing
beyond what was asked. State the proposal in one line and wait for the user's answer.
If you cannot ask (single-shot run), do only what was asked and list the extra work as not done.
```

配套两条：

- **纯 git 操作**：`status` / `add` / `commit` / `push` 不重跑「自上次验证后工作区未变」的
  测试；只用 `git status` / `git diff --stat` 确认范围。
- `skills/sequential-workflow/SKILL.md:129`、`:154`：Phase 4/5 的验证限定为
  「由该 workflow 驱动的实现任务」，不适用于用户临时插入的 git 请求。

**代价与风险**：

- 「提议并等待」会**结束当前回合**，用户回复后是**新回合**——交互模式下多一次往返，
  但省掉的是跑错方向的整段工作；`-p` / `--json` 没有用户，故文案里显式给出
  「不能问就只做被要求的部分并列出未做项」。
- 这条是纯行为约束，ADR-0039 的证据说明提示不保证生效。**必须用 A1 读数验收**：
  统计「用户请求后第一个工具调用」到「第一次写文件」之间的动作数，以及
  「未请求的写操作」占比；若读数不动，再考虑机制（例如把 git 操作拆成独立子命令）
  而不是继续加文案。

### 6.8 数据流（第一刀后）

```
Step 4b  compact: window = min(声明, 200k) ⇒ 触发 170k、保留 20k
Step 5   组装请求:
            session.messages().to_vec()
            + narration_nudge?            （既有）
            + single_call_nudge?          （A4，新）
            + soft_round_advice?          （A6，新）
Step 11  回合工具预算 = effective/4 = 50k
回合末   RoundClock.summary_line → stderr / TUI / debug / bench JSON
```

三条提醒都是**纯常量、仅本请求、不落 session、不碰 system/tools**，
满足 ADR-0024 决策 2 与 ADR-0021 决策 1。

---

## 7. 收益估算（口径修正后）

### 7.1 第一刀

| 项 | 算法 | 净墙钟 |
|---|---|---|
| A3+A4 批处理 | 单调用轮 426 中 10–20% 可合并（§1.1 已打折）：43–85 轮 × 4.49 s | **3.2–6.4 min** |
| A5 有效窗口 | 只有 >300k 的 217 轮 + 部分 200–300k：约 173 s 毛收益；扣 2–3 次摘要（30–45 s/次） | **1.5–2.5 min** |
| A6 轮数软提醒 | 三个长 turn（166/93/65）中若减少 0–20%：0–65 轮 × 4.49 s | **0–3.5 min** |
| A7 收尾文案 | 1–3 次 × 30–50 s | **0.5–2.5 min** |
| A1 度量 | 纯观测 | 0 |

**第一刀合计：约 5–13 min**（主循环 54 min 的 9–24%），中位 **≈8 min（15%）**。
token 方向同向变省（轮数减少 + 上下文变小）。

### 7.2 第二刀（若门禁通过）

| 项 | 估算 |
|---|---|
| B1 `batch` | **净 3.5–7.6 min**（毛 8.8–15.0 min − plan 生成 5.3–7.4 min） |
| B2 重复验证防止 | 0.5–1.5 min（命令执行时间；第 2 层生效后趋近 0，且可能顺带减轮） |

**两刀合计：约 9–21 min（17–39%），中位 ≈13 min（24%）**。

### 7.3 三处被高估的收益

| 原估 | 修正 | 原因 |
|---|---|---|
| 压缩提前 9 min（报告 §5） | **1.5–2.5 min** | TTFT 在 20k–300k 平坦，仅在 >300k 有收益；还要扣摘要调用成本 |
| 批处理提醒 10–18 min（候选甲） | **3–6 min** | 模型已在 bash 内平均 4.17 步/条，外层合并空间有限 |
| `batch` 6–14 min（候选丙初版） | **3.5–7.6 min** | plan 的生成本身要多出 120–180 token/次 |

报告用的 `session-latency` 斜率是 76.7 ms/1k（单会话、12k–96k、缓存命中较低）；
本机全部 4,972 轮拟合为 **7.08 ms/1k**，长会话分桶中位约 **3.4 ms/1k**。
本设计统一采用分桶中位数，不再使用单一斜率外推。

---

## 8. 风险与回滚

| 风险 | 缓解 | 回滚 |
|---|---|---|
| 提醒被模型当成错误 / 过度反应 | 纯常量、无错误词、不落 session、阈值翻倍 | A4/A6 设 `0` |
| 任务边界规则导致**过度提问**（每件小事都问） | 文案显式列出「必要步骤免问」；`-p` 模式禁止提问；A/B 统计「提问次数 / 回合」 | 删该 guideline |
| 「提议并等待」在 `-p` / CI 下无法继续 | 文案已写「不能问就只做被要求的部分并列出未做项」 | 同上 |
| 200k 上限过早压缩，丢细节导致重读 | 200k 而非 128k；`rounds` 反弹 >10% 即回滚 | `context_cap_tokens=0` |
| 回合预算从 250k 降到 50k，**降级频繁触发** ⇒ 每次改写历史断一次前缀缓存 | 降级次数与 `cache_hit` 进读数；下降 >1 个百分点 ⇒ 把预算改为 `max(effective/8, 64k)` | 调预算或回退上限 |
| 一次提示词 + schema 前缀平移 | A2/A3 同一次提交；锚点同步更新；此后一段时间不再动工具面 | 撤销提交 |
| 质量回退 | `fact/1` / `exp/t1` / `daily/2` 各 ≥3 次，`QUALITY_SPECS` 全部保持 | 逐项配置 |
| `batch` 采纳率低 | 门禁 + 终止判据（<20% 回滚） | 撤工具 |
| `batch` 绕过审批 | 只调度 `is_concurrency_safe` 工具；文档写明「要求审批 `read` 的部署必须关闭 batch」 | 不装配 |
| 证据台账误命中 | 默认关闭；只认成功记录；树指纹含未跟踪文件；非 git 不启用 | 配置关闭 |

---

## 9. 验收

### 9.1 指标

| 指标 | 当前 | 第一刀目标 | 第二刀目标 | 说明 |
|---|---:|---:|---:|---|
| **`rounds` / turn** | ~23 | **降幅 ≥15%** | 降幅 ≥30% | **主指标** |
| 每轮工具调用数（`tpr`） | 1.13 | ≥1.3 | —— | **第二刀后失效**（batch 外层恒 ~1.1），只作第一刀口径 |
| **内层操作数 / 轮** | ~4.2（shell 步） | 同左 | **≥6** | 第二刀从 `batch` 结果头解析 |
| 单调用轮占比 | 81% | <70% | <60% | |
| 每轮输入中位 | 254k | **≤120k** | ≤120k | 有效窗口 200k ⇒ 触发点 170k，压缩后约 20k 起涨 |
| TTFT 中位 | 2.23 s | ≤2.2 s | ≤2.1 s | 只回收 >300k 的尾部 |
| `cache_hit` | 99.3% | **不低于 98.3%** | 同左 | 下降 >1 个百分点 ⇒ A5 回滚（R4） |
| 长回合轮数（p100） | 166 | **≤120** | ≤120 | 软提醒生效 |
| 收尾 git 额外轮数 | 5–8 | **≤3** | ≤3 | |
| **未请求的写操作占比** | 未测 | **≤5%** | ≤5% | A7 的读数：用户请求后、非必要且未被要求的写 |
| 质量位 | 现状 | **不回退** | 不回退 | `QUALITY_SPECS` |

### 9.2 测量方法

1. `e2e_bench`：`fact/1` / `exp/t1` / `daily/2`，每个 ≥3 次，落 `bench-summary.json`。
2. 真实会话：用 `~/.yushan/logs` 的 `run_reading` 行做跨会话对比，不依赖单次。
3. 变异测试（新逻辑必须能被注入 bug 抓到）：
   - A2 schema：只给 `paths` 能通过、两个都不给时 `call` 报 `InvalidInput`、
     `required` 不再含 `path`；
   - A3 文案：不再含「shell loop is fine」的逃生口，且含「dependent steps separate」；
     `prompt.rs` 既有守卫测试同步；
   - A4 计数：说话不清零、多调用清零、阈值翻倍；
   - A5 `min`：封顶 / 不封顶 / `0` 关闭；
   - A6：60 → 120 → 240 翻倍、每回合重置、**不落 session**；
   - A7：测「任务边界」guideline 在 `bash` 在场时出现、不依赖被 `SYSTEM.md` 短路的文本；
   - `summary_line` 的缓存分母（只算上报过的轮）；
   - B1：非法 plan **不执行任何步**、引用只许向前、嵌套拒绝、单步/整批预算截断、
     失败步按 `on_error` 处理；
   - B2：树指纹变化即不拦截、非 git 关闭、失败记录不命中、
     压缩摘要 prompt 含 `## Verification` 节。

---

## 10. 明确不做

| 项 | 理由 |
|---|---|
| codemode / 任意代码执行 | 本仓不做沙箱（项目定位）；`batch` 只解释数据 |
| bash 查看器运行时检测/纠正 | 无同类先例；ADR-0039 的注记最终被「执行前剥离」取代 |
| 只读 bash 并发判定 | 与 `Tool::is_concurrency_safe` 的「`bash` 永远不行」准入规则冲突，需先改设计文档 |
| 显式 prompt cache 字段 | 只走 OpenAI 兼容 `/chat/completions`；命中已 99.3% |
| 固定前缀裁剪 | 实测 20k–300k 区间 TTFT 平坦（2.22→2.46 s），收益不可分辨 |
| 按调用类型分推理强度 | ADR-0060 默认不改；工具摘要已恒关；压缩保持开 |
| 子 agent 不等待 | 改 ADR-0055 语义，需独立 ADR，不与性能改动同一提交 |
| JSONL 增量写 / read 分段 / 日志异步 | 对墙钟影响小，单列工程项 |
| 生成吞吐 43 vs 76.8 tok/s | 根因未隔离，单列实验 |

---

## 11. 未决问题

- [x] **已确认（2026-10-29）**：1M 窗口是当前模型的标配，不是兜底默认。
      ⇒ A5 定位为「主动封顶」，不是修窗口来源。
- [x] **已确认（2026-10-29）**：有效窗口至少 200k，取 **200k** 为默认；
      300k 作为「观察重读后」的备选，不默认。
- [x] **已确认（2026-10-29）**：A6 软提醒阈值取 **60**（60 → 120 → 240），
      不命中 `fact/1`（10–33 轮）的正常范围。
- [x] **已确认（2026-10-29）**：B2 不做「结果缓存」，改为**三层防止重复验证**
      （规则 → 保留证据 → 硬拦截）。缓存仍要花一轮模型往返，且掩盖根因；
      「保留证据」才是让模型**不必**重跑的机制。详见 §5 第二刀 B2。
- [x] **已确认（2026-10-29）**：A7 扩展为「任务边界」——只做用户要求的事，
      必要步骤免问，其余先提议并等待；不能问时只做被要求的部分。详见 §6.7。

---

## 12. 评审修订（arch-validate 2026-10-29）

按 `review.md` 的 14 条发现修订（全部为文档变更，无代码改动）：

| 发现 | 修订 |
|---|---|
| R1 A4 文案可能诱导拆管道 | 文案改为「只陈述代价、不规定形状」，补「dependent steps separate」，并要求 A4 单独 pilot |
| R2 keep 区间无法容纳验证证据 | 改为摘要 prompt 新增 `## Verification` 节（v1）；v2 才追加独立消息 |
| R3 展示口径矛盾 | 明确 `/status` 显示 `effective / declared (capped)`；`view.rs` 同时读两者 |
| R4 降级频率上升断缓存 | 增加 `cache_hit` 不低于 98.3% 的读数与 `max(effective/8, 64k)` 备选 |
| R5 提醒注入优先级 | 写死 `narration > A4 > A6`，每轮至多一条，其余顺延 |
| R6 batch 可见性 | 放追加段末尾，子 agent 不提供；列出要同步的三处断言 |
| R7 batch 并发尖峰 | v1 声明 `is_concurrency_safe = false` |
| R8 `tpr` 指标失效 | 主指标改为 `rounds/turn`，新增 `inner_ops/round` |
| R9 门禁歧义 | 拆成「失败判据 / 继续判据 / 停止条件」三组 |
| R10 配置键命名 | `max_context_tokens` → `context_cap_tokens` |
| R11 硬拦截与 ADR-0039 相反 | ADR 决策 7 写明窄类反转与边界 |
| R12 read schema 校验缺口 | §9.2 增加三条单测 |
| R13 测试矩阵缺失 | §9.2 补齐 A2/A3/A6/A7/B1/B2 的条目 |
| R14 收益置信度 | ADR Consequences 写明第一刀真实下界 ~2 min |
