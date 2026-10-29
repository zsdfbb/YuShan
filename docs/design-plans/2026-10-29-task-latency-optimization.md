# 任务延迟优化（第一刀 A1–A7）设计计划

> 日期：2026-10-29
> 上游：`docs/arch/task-latency-optimization/{context.md, design.md, review.md, adr-latency-levers.md}`
> 实现核实：`~/.claude/plans/noble-kindling-lake.md`（含 5 处设计-现实偏差）
> 范围：**仅第一刀 A1–A7**。第二刀 B1 `batch` / B2 三层防止重复验证不在本期（design §5 门禁要求先有 bench 读数）。

## 背景

- **当前状态**：同一任务比 Codex / 另一 harness 慢 2–5 倍。会话 `1793181762` 的测量把差距定位到**轮数 × 每轮固定延迟**：
  - 555 轮 / 626 次工具调用；主循环模型耗时 54.0 min，工具执行 15.2 min。
  - 每轮中位耗时 4.49 s（TTFT 2.23 s + 生成约 2.26 s）。
  - 每轮输入中位 254,313 token（缓存命中 99.3%）；每轮工具调用数仅 1.13。
  - 每条 `bash` 内部平均 4.17 个 shell 步骤（573/575 含 `&&` / `|` / `;`）——模型其实在 bash 内批处理，真正稀缺的是**跨轮依赖层数**。
- **为什么需要这个变更**：目标是**不回退质量**地把主循环模型耗时砍掉 20–35%（54 min → 35–43 min），token 作为约束（不得变差，方向应更省）。第一刀只做低风险、可回滚、每项独立开关的杠杆；真实下界约 2 min（只有 A5 几乎确定），行为杠杆可能全部为 0。
- **不在本期**：`batch` 工具、三层重复验证防止、子 agent 不等待、按调用类型分推理强度、固定前缀裁剪、显式 cache 字段、运行中 `/model` 换模型后重算回合预算（既有行为，仅加注释）。

## 设计

### 总体方案

第一刀是「一条读数 + 一次 schema 修正 + 两条文案 + 两条提醒 + 一个窗口上限」，全部落在既有机制上，不新增 crate / trait / 工具。七项逐条如下。

| 项 | 机制 | 默认 | 开关 |
|---|---|---|---|
| **A1 度量** | `RoundClock` 扩展 `TurnTotals` + `summary_line`；四出口：`-p` / `--json` 的 stderr、TUI `Outbound::Output`、debug `ev="run_reading"`、bench `bench-summary.json` | 开 | 纯输出，无开关 |
| **A2 `read` schema** | `required` 移除 `path`，与已有 `paths` 对齐（`call` 已支持只给 `paths`，两者都缺时由 `InvalidInput` 兜底） | 修正 | 无 |
| **A3 批处理指引** | 删掉「a whole directory or a glob — one shell loop is fine」逃生口；给出 `read paths=[...]` / `grep paths=[...]` 形状；保留「dependent steps separate」 | 开 | 文案 |
| **A4 单调用提醒** | 连续 N 轮只有 1 个工具调用 ⇒ 下一轮请求末尾注入常量提醒，阈值翻倍 | 4 | `single_call_nudge_rounds=0` 关闭 |
| **A5 有效窗口** | `Config::effective_context_window() = min(声明窗口, context_cap_tokens)` | 200_000 | `context_cap_tokens=0` = 不封顶 |
| **A6 轮数软提醒** | 回合内到 60 / 120 / 240 轮各注入一次「汇报进展或收敛」，不中断 | 60 | `soft_round_advice_at=0` 关闭 |
| **A7 任务边界** | guidelines 新增「只做用户要求的事；必要步骤免问，其余先提议并等待；不能问就只做被要求的部分」 | 开 | 文案 |

**配置面（推断补充，需签字确认）**：`context_cap_tokens` / `single_call_nudge_rounds` / `soft_round_advice_at` 都是 `config.json` 顶层标量，沿用 `RawUserConfig` → `sanitize_uint` → `UserConfig` 的既有形状，使 §8 回滚表里的「A4/A6 设 0」无需重新编译。库面 `RunLimits` 的两个新字段默认 **0**（关闭），app 侧默认 4 / 60 ⇒ 老调用方逐字节不变。`context_cap_tokens` 只存在于 app 的 `Config`，不进 `RunLimits`。

### A1 度量

`apps/coding-agent/src/round_clock.rs` 已接收 `RoundStarted` / `ToolCall` / `ToolResult` / `RoundUsage`，在其上新增累加结构：

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
```

`RoundClock::summary_line(turn)` 输出定形、整数、grep 友好的一行，不含浮点：

```
turn=3 rounds=27 tools=41 tpr_x100=151 ttft_ms=61200 gen_ms=118400 tools_ms=9800 input_med=121344 cache_pct=98
```

缓存分母**只算上报过 usage 的轮**；全部未上报时输出 `cache_pct=n/a`。四出口：`-p` / `--json` 在终局事件后写 stderr（紧邻既有 `report_channel_stats`，stdout 保持干净）；TUI 走 `Outbound::Output`（不新增协议变体）；`flush_debug` 落一条 `ev="run_reading"`（schema 加法）；`e2e_bench` 的 `Reading` 增加 `tpr_x100` / `input_med` / `single_call_rounds` 并写 `bench-summary.json`（`YUSHAN_BENCH_OUT` 可覆盖路径，stdout 表格逐字节不变）。

### A2 `read` schema 修正

`adapters/tools-basic/src/read.rs` 的 `required` 当前是 `["path"]`，但 `properties` 含 `paths`，且 `call` 已支持只给 `paths`。改为不强制 `path`（两者至少一个，由 `call` 的 `InvalidInput("missing 'path'")` 兜底）。零语义变更，只消除 schema 与实现的矛盾。

### A3 批处理指引

`apps/coding-agent/src/prompt.rs` 的批处理段：保留「read-only calls in a batch run in parallel」；明确「一次消息里发 2–4 个彼此独立的调用」，`read` 用 `paths=[...]`、`grep` 用 `paths=[...]`；删除 shell-loop 逃生口，改为「读整目录先 `ls` / `find` 拿路径，再用 `read paths=[...]`」。不新增术语、不新增条目（前缀成本按字节计）。

### A4 单调用提醒

复用 `narration_nudge` 的既有通道（`crates/ys-loop/src/basic.rs` 的 Step 5 注入、Step 11 计数），新开**独立**计数：

- 本轮 `tool_calls.len() == 1` ⇒ `single_call_rounds += 1`；`>= 2` ⇒ 归零。
- **不因模型说话而归零**（与 narration 的判据相反，字段注释写明）。
- 到阈值 ⇒ 下一轮 Step 5 注入常量 `SINGLE_CALL_NUDGE_PROMPT`，阈值翻倍。

文案只陈述代价、不规定形状（review R1），末尾保留「Keep dependent steps separate」，避免诱导把一条 4 步管道拆成 4 个 `bash`。

### A5 有效窗口上限

```rust
pub const DEFAULT_CONTEXT_CAP_TOKENS: usize = 200_000;

impl Config {
    /// 压缩与回合预算实际使用的窗口：min(context_window(), context_cap_tokens)。
    /// context_cap_tokens == 0 ⇒ 不封顶。
    pub fn effective_context_window(&self) -> usize;
    pub fn effective_context_window_for(&self, model: &str) -> usize;
}
```

生产调用点全部改走 effective：`main.rs` 的 `limits.context_window` 与 `max_turn_tool_result_tokens = effective/4`、`app_loop.rs` 的 `/compact` 与每回合 `set_context_window`、`subagent.rs` 的 `child_limits_for`（偏差 D3）。**理由**：`CompactPolicy::trigger_at` / `keep_budget` / 回合工具预算三处都从同一个窗口派生，改一处等于同时改三处；引入 `min()` 后三者自动对齐（200k 窗口 ⇒ 触发 170k、保留 20k、回合预算 50k），`CompactPolicy` 的算法与不变量一行不改，且与「保留 20k」的绝对口径一致。

展示口径（review R3）：`/status` 同时读声明值与 effective，两者不等时显示 `200k / 1M (capped)`；`/model` 同口径；启动期 debug `run_start` 并列 declared/effective。子 agent 克隆父 `limits` 时，`child_limits_for` 必须改用 `effective_context_window_for(name)`，否则会绕过封顶（偏差 D3，回归钉见 N8）。

### A6 轮数软提醒

与 A4 共用注入通道，另开计数：阈值 `soft_round_advice_at`（默认 60），触发后 ×2（60 → 120 → 240）。每回合重置；不产生事件、不落 session、不中断回合。`fact/1` 常态 10–33 轮、`exp/t1` 常态 2–3 轮，60 不会命中正常任务，而失控回合 65 / 93 / 166 轮分别在第 1–3 次提醒内被覆盖。

### A7 任务边界

`guidelines` 新增一条：只做用户要求的事；读取/搜索、对改动代码跑检查、修复自身改动造成的破坏属「必要步骤免问」；无关重构、新文件/文档、依赖变更、大范围格式化、额外测试套件、超出要求的 commit/push 必须先一行提议并等待；单发模式不能问时只做被要求的部分并列出未做项。配套：纯 `git status/add/commit/push` 不重跑「自上次验证后工作区未变」的测试；`skills/sequential-workflow/SKILL.md` 的 Phase 4/5 验证范围限定为「由该 workflow 驱动的实现任务」。

### 注入优先级与数据流

三条提醒都追加到 `request_messages` 末尾、仅本请求、不落 session。优先级固定为 **`narration > A4 > A6`，每轮至多一条**；未发出的标志**顺延到下一轮，不清零**。实现上按优先级取第一个 `Some`，其余标志保留。

```
Step 4b  compact: window = min(声明, 200k) ⇒ 触发 170k、保留 20k
Step 5   组装请求:
            session.messages().to_vec()
            + narration_nudge?            （既有，最高优先）
            + single_call_nudge?          （A4）
            + soft_round_advice?          （A6，最低优先）
Step 11  回合工具预算 = effective/4 = 50k
回合末   RoundClock.summary_line → stderr / TUI / debug / bench JSON
```

## 涉及文件

- 新增: `docs/adr/0062-task-latency-levers.md`（从 `adr-latency-levers.md` 提升，status `proposed`）
- 修改: `apps/coding-agent/src/round_clock.rs`（`TurnTotals`、`summary_line`、`RoundUsage` 累加）
- 修改: `apps/coding-agent/src/debug_log.rs`（`ev="run_reading"` 记录）
- 修改: `apps/coding-agent/src/main.rs`（stderr 出口、debug `run_reading`、`resolve_limits` 默认 4/60、窗口改 effective、启动日志口径）
- 修改: `apps/coding-agent/src/app_loop.rs`（TUI 回合末一行、`flush_debug`、`/compact` 与每回合窗口改 effective）
- 修改: `apps/coding-agent/src/subagent.rs`（`child_limits_for` 改用 effective，D3）
- 修改: `apps/coding-agent/src/config.rs`（`context_cap_tokens` / `single_call_nudge_rounds` / `soft_round_advice_at` 三个顶层标量 + `effective_context_window[_for]()`）
- 修改: `apps/coding-agent/src/view.rs`（`build_view` 加 declared 值）
- 修改: `apps/coding-agent/src/capabilities.rs`（`/model` 输出同口径）
- 修改: `apps/coding-agent/src/prompt.rs`（批处理指引重写、任务边界 guideline）
- 修改: `apps/coding-agent/tests/e2e_bench.rs`（`Reading` 新字段、`bench-summary.json`、`YUSHAN_BENCH_OUT`）
- 修改: `adapters/tools-basic/src/read.rs`（`required` 移除 `path`）
- 修改: `crates/ys-component/src/limits.rs`（`RunLimits` 两个新字段默认 0 + 两个默认阈值常量）
- 修改: `crates/ys-loop/src/basic.rs`（两个计数、两个提示常量、优先级取一注入）
- 修改: `crates/ys-tui-coding/src/view.rs`（`CodingView` 加 `context_window_declared`）
- 修改: `crates/ys-tui-coding/src/commands.rs`（`/status` 渲染 `(capped)`）
- 修改: `skills/sequential-workflow/SKILL.md`（Phase 4/5 验证范围限定）
- 修改: `docs/design.md`（§12 测试矩阵新增条目）

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。
> 合同由用户签字确认，或经公约 C.3 的自动放行判据确认。N16 为 `MANUAL_ACK_REQUIRED`。

#### N1: A1 `TurnTotals` 累加与定形一行
- **验收方法**: 单测：多轮累加后 `rounds` / `tool_calls` / `ttft_ms` / `gen_ms` / `tools_ms` / `input_med` 正确；`summary_line` 匹配 `^turn=\d+ rounds=\d+ tools=\d+ tpr_x100=\d+ ttft_ms=\d+ gen_ms=\d+ tools_ms=\d+ input_med=\d+ cache_pct=(\d+|n/a)$`；缓存分母**只算上报过 usage 的轮**（全未上报 ⇒ `cache_pct=n/a`）
- **优先级**: 必测
- **验收人**: review subagent

#### N2: A1 四出口
- **验收方法**: 单测断言读数行格式化的**纯函数**输出（不发起真实模型调用）；grep 断言 `main.rs` 在终局事件后调用 stderr 出口、`app_loop.rs` 在回合末把该行送进 `Outbound::Output`（**不新增协议变体**）；debug 开启时日志含 `"ev":"run_reading"`；TUI 路径单测断言 `Outbound::Output` 收到该行。真实 `-p` 端到端跑一次为**可推迟**（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`，与 N16 同一次人工验收）
- **优先级**: 必测（其中真实 `-p` 端到端部分：可推迟）
- **验收人**: review subagent

#### N3: A1 bench 落盘
- **验收方法**: 单测：`Reading` → JSON 序列化字段齐全；`YUSHAN_BENCH_OUT` 指向固定路径时写该路径，未设时写 run dir；stdout 表格输出逐字节不变
- **优先级**: 必测
- **验收人**: review subagent

#### N4: A2 `read` schema
- **验收方法**: 单测三条：`required` 不再含 `path`；只给 `paths` 成功（既有 `multi_file_reads_in_one_call_with_headers` 保持绿）；`path` 与 `paths` 都不给 ⇒ `ToolError::InvalidInput("missing 'path'")`
- **优先级**: 必测
- **验收人**: review subagent

#### N5: A3 文案
- **验收方法**: `prompt.rs` 单测：批处理段**不含** `one shell loop is fine`，含 `paths=[...]` 与 `Keep dependent steps separate`；`multi_target_guideline_appears_only_with_read_only_tools` 同步更新并绿
- **优先级**: 必测
- **验收人**: review subagent

#### N6: A4 计数
- **验收方法**: 单测：本轮 1 个调用 ⇒ 计数 +1；≥2 ⇒ 归零；**模型说话不清零**；到阈值 ⇒ 下一轮请求末尾多一条 `Role::User` 且 `session.messages()` 长度不变；阈值翻倍
- **优先级**: 必测
- **验收人**: review subagent

#### N7: A5 `min` 语义
- **验收方法**: 单测：声明 1M + cap 200k ⇒ 200k；cap `0` ⇒ 1M；声明 128k + cap 200k ⇒ 128k；`effective_context_window_for(子模型)` 同判
- **优先级**: 必测
- **验收人**: review subagent

#### N8: A5 生产调用点
- **验收方法**: grep 断言：`main.rs` / `app_loop.rs` / `subagent.rs` 的 5 处窗口读取全部走 effective；`child_limits_for` 单测：父 cap 200k + 子模型声明 1M ⇒ 子 `limits.context_window == 200_000`（D3 回归钉）
- **优先级**: 必测
- **验收人**: review subagent

#### N9: A5 展示口径
- **验收方法**: 单测：`/status` 在 declared ≠ effective 时输出含 `(capped)` 与两个值；相等时只输出一个值
- **优先级**: 必测
- **验收人**: review subagent

#### N10: A6 阈值与重置
- **验收方法**: 单测：60 → 120 → 240 翻倍；每回合重置；不落 session
- **优先级**: 必测
- **验收人**: review subagent

#### N11: 注入优先级（R5）
- **验收方法**: 单测：三条同时置位 ⇒ 本次请求**只有一条**且是 narration；A4/A6 标志保留到下一轮；顺序稳定
- **优先级**: 必测
- **验收人**: review subagent

#### N12: A7 任务边界
- **验收方法**: `prompt.rs` 单测：该 guideline 在 `bash` 在场时出现；含「必要步骤免问」与「不能问就只做被要求的部分」两句
- **优先级**: 必测
- **验收人**: review subagent

#### N13: 前缀平移
- **验收方法**: `toolset.rs` 指纹锚（`tools = 0x...`）更新为新值且 `prefix_fingerprint_is_anchored_and_stable` 绿；`assembly.rs` 尾断言（`bash_bg + subagent + subagent_async`）**不变**（本期不新增工具）
- **优先级**: 必测
- **验收人**: review subagent

#### N14: 变异测试（AGENTS.md 要求）
- **验收方法**: 逐项注入 bug 必须被抓：A4 说话也清零 / A6 不翻倍 / A5 用 declared 而非 effective / `summary_line` 缓存分母算全部轮 / 优先级取最后一个 `Some` —— 每个注入后对应用例变红，注入后**必须回滚**且 `git diff` 无残留
- **优先级**: 必测
- **验收人**: 变异任务 subagent（注入 → 跑对应用例 → 记录红/绿 → 回滚），结果由 review subagent 核对残留与报告

#### N15: 全量回归
- **验收方法**: `./scripts/par-test.sh` 全绿（退出码 0）；`cargo clippy --all-targets` 无新警告；`cargo fmt --check` 干净
- **优先级**: 必测
- **验收人**: review subagent

#### N16: 门禁读数（design §5）
- **验收方法**: `YUSHAN_BENCH_REPEATS=3 cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture`（`fact/1` / `exp/t1` / `daily/2`）跑前后对比，读 `bench-summary.json` 的 `rounds` / `tpr_x100` / `input_med` / `cache_hit` / `QUALITY_SPECS`
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

> N16 说明：bench 是 `#[ignore]`，需要 `YUSHAN_API_BASE` + `YUSHAN_API_KEY` 与真实花费，orchestrator 无法执行。它也是 design §9.1 的主验收指标（`rounds/turn` 降幅 ≥15%、`cache_hit` 不低于 98.3%、质量位不回退）。代码正确性由 N1–N15 全覆盖，N16 只决定「第一刀是否达成目标、是否启动第二刀」。

### 测试框架与命令

- 全量测试唯一入口: `./scripts/par-test.sh`（跨测试二进制并行；`cargo test --workspace` 不算跑过测试）
- 构建 / 检查入口: `./scripts/cargo-build.sh`（`./scripts/cargo-build.sh [<子命令>] [cargo 参数...]`）
- 单测定位: `cargo test -p <crate> <name>`
- Lint: `cargo clippy --all-targets`
- 格式化: `cargo fmt` / `cargo fmt --check`
- 基准: `YUSHAN_BENCH_REPEATS=3 cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture`（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）

## 波及文档

- `docs/design.md` §12 测试矩阵 — 新增本特性条目（A1/A2/A3/A6/A7 与优先级注入）
- `docs/adr/0062-task-latency-levers.md` — 新增；**必须写明 A6 反转 ADR-0027「软警告本轮不做」的理由与边界**（`max_rounds` 默认已是 `u32::MAX`、唯一兜底是 48h wall-clock，软提醒缺口成立），并写明第一刀真实下界约 2 min
- `docs/design-final/` — Phase 4 按实际实现归档（预期涉及 `session-latency-phase1.md` / `usage-observability.md` / `compaction.md`）

## 风险与注意事项

### 设计风险（design §8）

- **提醒被模型当成错误 / 过度反应**：纯常量、无错误词、不落 session、阈值翻倍；回滚为 A4/A6 设 `0`。
- **任务边界规则导致过度提问**：文案显式列出「必要步骤免问」；`-p` 模式禁止提问；A/B 统计「提问次数 / 回合」；回滚为删该 guideline。
- **「提议并等待」在 `-p` / CI 下无法继续**：文案已写「不能问就只做被要求的部分并列出未做项」。
- **200k 上限过早压缩导致重读**：取 200k 而非 128k；`rounds` 反弹 >10% 即回滚（`context_cap_tokens=0`），可先调 300k 再判断。
- **回合预算从 250k 降到 50k 使降级频繁触发、断前缀缓存**：`cache_hit` 下降 >1 个百分点 ⇒ 预算改为 `max(effective/8, 64k)` 或回退上限。
- **一次提示词 + schema 前缀平移**：A2/A3 同一次提交，锚点同步更新；此后一段时间不再动工具面。
- **质量回退**：`fact/1` / `exp/t1` / `daily/2` 各 ≥3 次，`QUALITY_SPECS` 全部保持；否则逐项配置回滚。

### 设计-现实偏差（实现必须遵守）

- **D1**：`RoundClock` 目前没有 `input_tokens` / `cache_hit` / `cache_miss` 累加字段（这些值只在 `RoundUsage` debug 记录里路过）。A1 必须在 `RoundClock` 内新增累加字段，不能假设已有。
- **D2**：`e2e_bench` 的 `Reading` 目前**没有**任何 JSON 落盘，`YUSHAN_BENCH_OUT` 也不存在。A1 需新增 `bench-summary.json` 与该 env 覆盖，且**不改 stdout 表格**。
- **D3（重点）**：`subagent.rs` 的 `child_limits_for` 用 `cfg.context_window_for(name)` 覆盖子模型窗口 ⇒ **子 agent 会绕过父级封顶**。A5 必须同时改这一处为 `effective_context_window_for(name)`，并用 N8 的回归钉锁住。
- **D4**：`/status` 实际渲染在 `crates/ys-tui-coding/src/commands.rs`，`CodingView` 只带 effective 值。`(capped)` 显示需要给 snapshot 加 `context_window_declared` 字段并跨 crate 渲染。
- **D5**：`main.rs` 中对应位置是测试断言，不是启动日志；真实出口是 debug `run_start` 与 `fixed_cost_note` 启动警告。同口径改这两处。

### 其他约束

- `max_turn_tool_result_tokens` 只在 `resolve_limits` 算一次；运行中 `/model` 换模型时 `set_context_window` **不重算**回合预算。这是既有行为，本期不改，但需在注释里写清。
- `RunLimits` 是 `#[non_exhaustive]`，新增字段不破坏老调用方；库面默认 0、app 默认 4/60 的分层是「老调用方逐字节不变」的承重设计。
- 测试隔离：临时目录名含进程内原子序号 + `process::id()`；进程级 env 改写走 `crate::test_env::env_lock()` + `EnvRestore`。
- 注释只写当前设计，不写变更叙事与竞品对照；设计 why 落 `docs/adr/`。
