# 任务延迟优化（第一刀 A1–A7）— As-built

> 性质：**实际建成记录**。设计见 [`docs/arch/task-latency-optimization/design.md`](../arch/task-latency-optimization/design.md)，
> 评审见 [`review.md`](../arch/task-latency-optimization/review.md)，
> 决策见 [`ADR-0062`](../adr/0062-task-latency-levers.md)，
> 计划见 [`design-plans/2026-10-29-task-latency-optimization.md`](../design-plans/2026-10-29-task-latency-optimization.md)
> 与 [`exec-plans/2026-10-29-task-latency-optimization.md`](../exec-plans/2026-10-29-task-latency-optimization.md)。
> 日期：2026-10-29 ｜ 基线 `1f3e72b`（本轮改动在其上，尚未 commit）。

## 一句话

第一刀只做**低风险、可回滚**的七项：**A1** 回合级读数（一条定形行 + 四个出口）、
**A2** `read` schema 与 `paths` 对齐、**A3** 探索聚合命令 + 写入结构化工具指引、
**A4** 回合内累计调用数批处理提醒（`tpr` 过滤）、**A5** 有效窗口封顶（默认 200k）、
**A6** 轮数软提醒、**A7** 任务边界 guideline。
第二刀 **B1 `batch`** 与 **B2 三层重复验证防止**未实现（门禁读数未出）。

> A3 与 A4 在首轮实测后做了方向性返工，本文按**当前代码**记录；返工依据见「实测读数」。

## 落点

```text
apps/coding-agent/src/round_clock.rs   TurnTotals 累加；summary_fields / summary_line / cache_pct
apps/coding-agent/src/main.rs          finish_after_reading（stderr 出口）、record_run_reading[_fields]
                                       （debug 出口）、resolve_limits（effective + A4/A6 默认）
apps/coding-agent/src/app_loop.rs      TUI 回合末读数行走 Outbound::Output + debug run_reading；
                                       /compact 与每回合 set_context_window 改 effective
apps/coding-agent/src/config.rs        context_cap_tokens / batch_nudge_at_tool_calls /
                                       soft_round_advice_at 三个顶层标量 + effective_context_window[_for]
apps/coding-agent/src/subagent.rs      child_limits_for 改 effective_context_window_for
apps/coding-agent/src/view.rs          build_view 的 effective 与 declared 均取自 cfg
apps/coding-agent/src/capabilities.rs  /model 的窗口行同口径（effective / declared (capped)）
apps/coding-agent/src/prompt.rs        A3 探索聚合 / 写入结构化两条指引 + A7 任务边界 guideline
adapters/tools-basic/src/read.rs       schema 的 required 整键省略
crates/ys-component/src/limits.rs      RunLimits +batch_nudge_at_tool_calls / soft_round_advice_at（核心默认 0）
crates/ys-loop/src/basic.rs            BATCH_NUDGE_PROMPT / SOFT_ROUND_ADVICE、
                                       advance_batch_nudge、优先级注入
crates/ys-tui-coding/src/view.rs       CodingView +context_window_declared
crates/ys-tui-coding/src/commands.rs   /status 的 context 行 `(capped)` 形态 + format_context_window
apps/coding-agent/tests/e2e_bench.rs   Reading 新字段、bench-summary.json、YUSHAN_BENCH_OUT、损坏备份
skills/sequential-workflow/SKILL.md    Phase 4/5 验证范围限定
```

## A1 · 度量

### 累加与定形

`apps/coding-agent/src/round_clock.rs`：

- `TurnTotals`（`:65-78`）累加 `rounds` / `tool_calls` / `ttft_ms` / `gen_ms` / `tools_ms` /
  `input_tokens` / `cache_hit` / `cache_miss` / `cache_reported`。
- `close_round`（`:433-437`）在**同一处**把本轮三段耗时与计数并入 `totals`；`RoundUsage` 分支
  （`:259-271`）追加 `input_tokens`，只在 `usage.cache_split()` 为 `Some` 时累加缓存桶并置
  `cache_reported`。
- `reset_round`（`:508-523`）只清逐轮状态，**不清 `totals`** —— 回合级累加跨轮保留。
- `summary_fields(turn) -> Vec<(&'static str, u128)>`（`:351-378`）：固定顺序 8 个数字键
  `turn / rounds / tools / tpr_x100 / ttft_ms / gen_ms / tools_ms / input_med`。
  `tpr_x100 = tool_calls × 100 / rounds`（`rounds == 0` 时取 0，不做除零）；`input_med` 偶数个时取
  中间两个的**较小者**。
- `summary_line(turn)`（`:385-392`）**由 `summary_fields` 拼出**，末尾再追加 `cache_pct` ——
  文本与结构化字段同源，不会出现两套口径。
- `cache_pct()`（`:395-403`）两态：未上报（`cache_reported == false`）或分母为 0 ⇒ `n/a`；
  否则整数百分比。分母只含上报过缓存读数的轮，未上报轮**不伪造成 0% 命中**。

### 四个出口

| 出口 | 机制 |
|---|---|
| `-p` / `--json` | 终局事件后写 **stderr**：`finish_after_reading`（`main.rs:2111-2128`）**先**写读数行，**再**传播 `run_result` / `consume_result` 的 `Err` ⇒ 失败回合仍输出读数；错误优先级为**回合错误先于消费错误**。调用点 `main.rs:2217` / `:2312`。stdout 保持干净。 |
| TUI | `app_loop.rs:1570` 算 `summary_line(*ctx.turns)`，`:1589` 经**既有** `Outbound::Output` 展示 —— **未新增协议变体**。 |
| debug | `ev="run_reading"`：`record_run_reading` / `record_run_reading_fields`（`main.rs:926` / `:936`）写 `line` **加**顶层结构化字段 `turn / rounds / tools / tpr_x100 / ttft_ms / gen_ms / tools_ms / input_med`。TUI 路径只拿到文本，故从 `line` 反解数字字段（`:951-982`），键顺序与 `summary_fields` 一致。 |
| bench | `apps/coding-agent/tests/e2e_bench.rs` 每条结果追加一条到 `bench-summary.json`；`YUSHAN_BENCH_OUT` 非空时改指固定路径（`:547-552`）。文件存在但不是合法 JSON 数组时**改名备份**为 `<name>.corrupt-<pid>-<序号>`（`:579-634`），不静默清空历史基准。 |

**既有轮级文本行逐字节不变**：`observe` 与 `observe_debug` 共享同一状态机，测试
`observe_and_observe_debug_emit_identical_lines` 钉住两条路径的文本行一致；bench 的 stdout 表格
也未改动。

### bench 的三字段

`Reading` 新增 `tpr_x100` / `input_med` / `single_call_rounds`（`:176-185`）。
前两者从 `run_reading` 的顶层字段读，顶层缺失时从 `line` 反解（`:238-251`）；
`single_call_rounds` 生产侧**不写**，由同一份 debug 里 `round_end.tool_calls == 1` 的轮数
**兜底派生**（`:248-250`、`:355-357`）。没有 `run_reading` 记录时三字段在 JSON 里写 `null`
（`run_reading_seen == false`），与表格的 `-` 同义，不伪装成 0。

## A2 · `read` schema

`adapters/tools-basic/src/read.rs:37`：**整键省略** `required`（不是 `"required": []`），
`path` 与 `paths` 二选一。四种输入行为：

1. 只给 `path` ⇒ 单文件读，行号形态不变；
2. `paths` ≥ 2 项 ⇒ `read_many`，带 `=== <path> ===` 分隔头；
3. `paths` 恰 1 项 ⇒ **回落单文件读**（无分隔头）；
4. 两者都不给 ⇒ `ToolError::InvalidInput("missing 'path'")`（运行时兜底仍在，不会静默读到根目录）。

## A3 · 探索聚合与写入工具

`apps/coding-agent/src/prompt.rs` 的 `tool_guidelines`（`:194`）分两侧：

### 探索侧：一条聚合命令

门控：`read` / `grep` / `find` / `ls` **任一在场**（`prompt.rs:239-242`）。渲染两条：

1. `Explore with one command, not one round per look. ...`（`prompt.rs:243-251`）：
   面对「找所有 X」「按 crate 统计 Y」「带行号列出 Z」这类检索，**先规划整次取数、写一条
   命令一次吐全** —— 对路径 `for` 循环、`rg -n --context 2`、`awk`。理由是每条消息都是
   一次完整的模型往返；并写明 `rg` 默认尊重 `.gitignore`、用 `-m` / `-l` / `| head -n`
   约束输出，避免聚合把上下文撑爆。
2. `Reach for read/grep/find/ls when ...`（`prompt.rs:252-257`）：结构化工具的**胜出场景**
   —— 下一步真正依赖刚看到的内容、需要对单文件做 `offset`/`limit` 续读、或一次定向查询
   就是全部答案；`read paths=[...]` 一次覆盖若干已知文件。

### 写入侧：仍用 edit / write，且允许拆开

门控：`edit` / `write` **任一在场**（`prompt.rs:217-225`）。渲染
`Write with edit and write, never with scripts: ...`：

- 每个文件一条 `edit` 调用，同一文件的多个位置放进**该调用的多个 entry**；
- 每个新文件一条 `write`；
- **拆成多次调用没有问题**（每次仍是一次往返），因为脚本化写入（`sed` / `awk` /
  `python -c`）会绕过 `edits[].oldText` 的精确匹配 —— 那是变更安全性的唯一校验点。

### 为什么删掉四条逐工具禁令

原提示词有 `Use read to examine files instead of cat or sed`、
`Use grep ... instead of running grep through bash`、`Use find ...`、`Use ls ...` 四条。
实测（见「实测读数」）表明它们把本可聚合的检索**拆成逐个结构化调用**，反而抬高轮数：
`fact/1` 的形态从 `grep → grep → read/read → bash ×N` 变成几乎全是 `bash` + `rg`/`for`/`sed`
的聚合命令。工具执行只占墙钟极小部分，**轮数才是墙钟与 token 的共同乘数**，因此方向反转：
探索鼓励聚合，写入仍坚持结构化工具、只允许拆开。回归钉
`explore_guideline_prefers_one_aggregating_command` / `explore_guideline_appears_only_with_read_only_tools`
（`prompt.rs:818` / `:860`）同时做正向与反向断言。

### grep 截断提示：字节上限下不暗示加大 limit

`adapters/tools-basic/src/grep.rs` 的 `build_result`（`:255`）按**两个互斥分支**给截断建议：

- `byte_truncated`（`:302`，`head_lines` 以 `GREP_MAX_RESULT_BYTES = 16 KiB`（`:55`）截断，
  或子进程 stdout 触达 `BYTE_CAP`）：**不再**输出 `Use limit=…`，只发一条合并 notice
  （`:319-324`）——报总数、实际展示量、「加大 limit 无用」与收窄指引：

  ```text
  {total} matches found; showing the first {shown} lines (output capped at 16 KiB).
  Raising `limit` will not return more — narrow with `glob`, `path`, or a tighter `pattern`.
  ```

  既有的 `Truncated {n} lines, showing first {shown}. Narrow the search ...`
  （`:339-346`）与之同向并存。
- **仅** `limit_reached`（`:325-331`）：总字节远未触顶，加大 limit 真能多拿，故保留
  `Use limit={2N} for more`，并前置总数：

  ```text
  {total} matches found; showing {limit}. Use limit={2N} for more
  ```

两条建议在字节上限下会互相矛盾：`limit=100` 时 `Use limit=200 for more` 是**错误建议**，
因为 16 KiB 预算在第 77 条命中处就用尽，加大 limit 一条也多拿不到。总数由
`render_entries`（`:591-649`）返回 —— 它在丢弃 limit 之外的命中**之前**统计
`total_matches`（`:596`），故 notice 能报出真实总数。

**不调大 `DEFAULT_LIMIT`（保持 100，`:46`）**：`daily/2` 实测中 `is_concurrency_safe`
全仓 129 条命中、合计 26,292 字节（`rg --json` 形态），16 KiB 预算在第 77 条命中处用尽
（`Truncated 52 lines, showing first 77`）。`limit=100` 已超过预算能装下的量，
`limit=200` 不会多返回任何内容，只多烧一轮与更多 token。字节上限是瓶颈时，正确的
降载手段是收窄搜索面（`glob` / `path` / 更紧的 `pattern`），不是抬高条数上限。

## A4 / A6 / 注入优先级

### 配置面

- `crates/ys-component/src/limits.rs`：新增 `batch_nudge_at_tool_calls`（`:164`）/
  `soft_round_advice_at`（`:169`），**核心默认均为 0（关闭）** ⇒ 老调用方逐字节不变；
  应用默认常量 `DEFAULT_BATCH_NUDGE_AT_TOOL_CALLS = 12`（`:29`）/
  `DEFAULT_SOFT_ROUND_ADVICE_AT = 60`（`:34`）。
- `apps/coding-agent/src/config.rs`：两者是 `config.json` **顶层标量**，缺省走应用默认常量
  （访问器 `batch_nudge_at_tool_calls` `:382` / `soft_round_advice_at` `:391`）；
  `resolve_limits`（`main.rs:1149-1150`）写入 `RunLimits`。

### A4 判据：累计调用数 + `tpr` 过滤

纯函数 `advance_batch_nudge(calls, rounds, cumulative, threshold)`
（`crates/ys-loop/src/basic.rs:90-108`），每轮工具执行后调用（`:1414-1425`）：

| 条件 | 行为 |
|---|---|
| `threshold == 0` 或 `cumulative < threshold` | 只累加，不触发 |
| `cumulative >= threshold` **且** `tpr < 1.5` | 置 `batch_nudge`、**阈值 ×2**、返回 `true` |
| `cumulative >= threshold` **且** `tpr >= 1.5` | **不触发、也不推进阈值**，下一轮再判 |

- `cumulative` 是回合内工具调用数的**单调累加，永不重置**（多调用轮不清零）；
- `tpr = cumulative / rounds`，判据用整数比较 `cumulative * 2 < rounds * 3`（等价于
  `tpr < 1.5`，`rounds >= 1`，`u64` 相乘避免溢出，`basic.rs:100-107`）；
- 阈值按 **12 → 24 → 48 → …** 翻倍。

**旧判据为什么被替换**：旧版按「连续 N 轮只有 1 个工具调用」计数，中间夹任一多调用轮就
清零，到阈值 4 即触发。实测 10 次运行只触发 4 次，其中 3 次落在回合最后 1–2 轮（此时提醒
改变不了任何东西），单调用轮占比 49% → 51% 未变；轮序列形如 `1 1 2 1 1 1 1 1 1 0`，
一次 2 调用就把前面积累全部作废。新判据改用单调累计，并用 `tpr < 1.5` 避免对**本来就在
批处理**的回合发出错误建议。

### A6 判定

在 `rounds += 1` **之后**、请求组装**之前**（`basic.rs:697-702`）：`soft_round_advice_at > 0 &&
rounds as usize >= soft_round_advice_at` ⇒ 置标志并 `saturating_mul(2)`（60 → 120 → 240）。
`> 0` 短路是必需的，否则 `0 >= 0` 恒真。每回合重置（变量为回合局部状态），不产生事件、不落 session、
不中断回合。

### 优先级

`narration > A4 > A6`，`if / else if / else if`（`basic.rs:711-736`），**每轮至多注入一条**；
未选中的标志**保留到下一轮**（不清零）。三条都只 push 进 `request_messages`（会话消息的 clone），
**不落 session**、不碰 system/tools。

## A5 · 有效窗口

### 解析

`apps/coding-agent/src/config.rs`：

- `DEFAULT_CONTEXT_CAP_TOKENS = 200_000`（`:15`）；
- `effective_context_window_for(model)`（`:359-365`）：`match context_cap_tokens() { 0 => declared,
  cap => declared.min(cap) }` —— `0` = 不封顶。用 `match` 而非 `min(declared, 0)`，避免把
  `0` 当成真实上限；
- `effective_context_window()`（`:351`）取当前 model；`context_cap_tokens()`（`:373`）缺省 200k。

### 五个生产调用点

| 调用点 | 用途 |
|---|---|
| `main.rs:1138` | `limits.context_window` |
| `main.rs:1146` | `max_turn_tool_result_tokens = effective / 4`（200k ⇒ 50k） |
| `app_loop.rs:798` | `/compact` 的窗口 |
| `app_loop.rs:1341` | 每回合 `set_context_window` |
| `subagent.rs:966` | `child_limits_for`（子 agent 不绕过父级封顶） |

### 展示

- `CodingView` 新增 `context_window_declared`（`crates/ys-tui-coding/src/view.rs`）；
- `/status` 的 context 行（`crates/ys-tui-coding/src/commands.rs:797-805`）：
  declared 与 effective **都存在且不等**时渲染 `200k / 1M (capped)`；相等或 declared 缺失时
  **逐字节退回原样**单值；
- `/model` 同口径（`apps/coding-agent/src/capabilities.rs:289-297`）；
- `format_context_window`（`commands.rs:737-750`）的 k / M 档均为**截断**（`999_999` ⇒ `999k`，
  `1_999_999` ⇒ `1.9M`，不进位）；
- 启动期 debug `run_start` 并列 `context_window`（声明）与 `context_window_effective`
  （`main.rs:850-858`）。

**`build_view` 的 effective 与 declared 都从 `cfg` 派生**（`apps/coding-agent/src/view.rs:158-165`），
**不是** `agent.context_window()`。理由：`cfg` 在 `/model` / `/login` 时立即更新，而 agent 的窗口
要到回合开头才刷新；若 effective 取 agent，从 1M（effective 200k）切到 128k 后会显示
`200k / 128k (capped)` —— effective 大于 declared 却标 capped，与 `/model` 自己的输出打架。

## A7 · 任务边界

- `apps/coding-agent/src/prompt.rs:419-429`：**无条件渲染**（不挂任何工具分支），文案含
  「必要步骤免问」与「不能问就只做被要求的部分并列出未做项」两句；
- `skills/sequential-workflow/SKILL.md` 的 Phase 4/5：验证范围限定为「由该 workflow 驱动的
  实现任务」，不适用于用户临时插入的纯 git 请求。

## 实测读数

**方法**：`cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture`，
`YUSHAN_BENCH_REPEATS=3`；基线侧为独立 worktree `/tmp/ys-baseline-wt` @ `1f3e72b`；
两侧**串行**跑，避免争抢 CPU / API 并发。模型 `opencode-go` / `deepseek-v4.1-flash`，
协议 `chat`。完整报告见
[`docs/reports/2026-10-30-task-latency-first-knife-report.md`](../reports/2026-10-30-task-latency-first-knife-report.md)。

### 臂 1：原始基线（无本期改动）

| 提示 | 轮数 | 墙钟 s | cache | 质量 |
|---|---|---|---|---|
| `daily/2` | 24 / 6 / 6 | 60.8 / 28.4 / 28.6 | 97/80/88% | Q+ ×3 |
| `fact/1` | 18 / 10 / 11 | 226.3 / 93.0 / 86.4 | 91/84/82% | Q+ ×3 |
| `exp/t1` | 3 / 3 / 2 | 11.0 / 15.6 / 7.8 | 66/65/98% | Q? ×3 |

### 臂 2：本期改动 + 旧提示词

| 提示 | 轮数 | 墙钟 s | cache | 质量 |
|---|---|---|---|---|
| `daily/2` | 8 / 10 / 11 | 40.0 / 45.2 / 45.4 | 84/93/88% | Q+ ×3 |
| `fact/1` | 15 / 14 / 11 | 115.7 / 146.3 / 197.1 | 90/88/86% | Q+ ×3 |
| `exp/t1` | 3 / 3 / 2 | 13.0 / 13.0 / 9.2 | 66/66/49% | Q? ×3 |

### 臂 3：本期改动 + 新脚本指引（只重跑 `daily/2` 与 `fact/1`）

| 提示 | 轮数 | 墙钟 s | cache | 质量 |
|---|---|---|---|---|
| `daily/2` | 7 / 6 / 4 | 44.6 / 36.2 / 23.6 | 81/78/69% | Q+ ×3 |
| `fact/1` | 10 / 14 / 13 | 121.3 / 152.1 / 113.7 | 89/93/91% | Q+ ×3 |

合计（`daily/2` + `fact/1` 共 6 次）：轮数 75 → 69 → **54**；
墙钟 523.5 → 589.7 → **491.5 s**。

### 结论

1. **质量位全程无回退**：18 次运行全部保持基线水平（`daily/2` / `fact/1` 为 Q+，
   `exp/t1` 两侧同为 Q?）。
2. **A5 与 A6 在本次基准里从未触发**：压缩事件 0 次（输入中位 20–33k，触发点 170k）；
   最长回合 24 轮（软提醒阈值 60）。**因此对这两项的有效性没有任何证据**，既未证明有效
   也未证明无效。A5 的主要收益是 token，而基准不测 token。
3. **A4 旧判据实测无效**：10 次运行中只触发 4 次，其中 3 次落在回合最后 1–2 轮；
   单调用轮占比 49% → 51%，未变。根因是「连续」计数会被中间任一多调用轮清零。
4. **A4 新判据逻辑正确但仍无空间**：6 次运行只触发 1 次（第 12 轮 / 共 14 轮）；其余被
   `tpr >= 1.5` 正确抑制（`fact/1` tpr 1.60 / 1.69）或累计不足 12（`daily/2` 仅 3–7 次调用）。
5. **A3 是唯一测出机制性改变的杠杆**：工具形态从 `grep → grep → read/read → bash ×N` 变为
   几乎全是 `bash` + `rg`/`for`/`sed` 的聚合命令（`fact/1` 三次分别 16/13/20 次调用中
   `read`+`grep` 仅出现 2 次）；`daily/2` 轮数中位 10 → 6。
6. **墙钟 −6% 不可作为结论**：单次运行方差达 ±50%（`fact/1` 基线 86–226 s，`daily/2` 轮数
   6–24），3 次 repeat 无法分辨该量级差异。**可信的是轮数与工具形态**，不是墙钟。
7. **`daily/2` 的 cache 从 88% 降到 78%**（`fact/1` 反而 84% → 91%），方向不一致，需后续
   单独排查。
8. **门禁（design §5）无法判定**：判据依赖的 A5/A6 在基准任务上不触发，A4 几乎不触发。
   **该基准测不了这三项**，需要长程提示或按 design §9.2 用真实长会话的 `run_reading`
   跨会话对比。
9. **grep 截断提示改为「字节截断时只报总数 + 收窄指引」**：旧提示在 16 KiB 预算下同时给出
   `Use limit=200 for more`（错误建议）与 `Truncated … Narrow the search`（正确建议），
   `daily/2` 的模型选了前者、白烧一轮。**该修复在当前基准上的效果不可测** —— 新提示词
   让模型改用 `bash` + `rg`，3 次 `daily/2` 运行里 grep 工具只出现 1 次，结构化 grep
   已不是主通道。

## 明确未实现

| 项 | 状态 |
|---|---|
| **B1 `batch`**（受限 JSON DAG） | **未实现**。门禁读数（`fact/1` / `exp/t1` / `daily/2` 各 ≥3 次）未出，未启动。 |
| **B2 重复验证的三层防止** | **未实现**。同受门禁约束；A7 的「未请求写操作占比」读数也未出。 |

## 与设计的偏差

设计文档/计划被证实有 5 处与基线代码不符（D1–D5），另有 3 处评审阶段发现的实现缺口
（已在本轮修掉，记录其最终形态）。此外有 **2 处实测后的方向性返工**（A3 / A4），
它们不是实现与设计的偏差，而是**设计本身被读数推翻**。

| # | 设计/计划的假设 | 实际 |
|---|---|---|
| **A3 方向反转** | 设计：删掉 `one shell loop is fine` 逃生口，要求「一条消息发 2–4 个独立调用」 | 实测后**反转为「探索用一条聚合命令」**，并删除 `Use read ... instead of cat/sed` 等四条逐工具禁令；写入侧改为「用 edit/write、禁止脚本，但允许拆开」。见「实测读数」结论 5。 |
| **A4 判据重写** | 设计：连续 N 轮只有 1 个工具调用、阈值 4 | 实测后**改为回合内累计工具调用数 + `tpr < 1.5` 过滤、阈值 12**；旧判据 10 次仅触发 4 次且多在回合末尾，单调用轮占比未变。见「实测读数」结论 3–4。 |
| **D1** | `RoundClock` 已有 `input_tokens` / 缓存累加字段 | 原本**没有**；这些值只在 `RoundUsage` debug 记录里路过。A1 必须在 `RoundClock` 内新增 `TurnTotals` 累加。 |
| **D2** | `e2e_bench` 已有 JSON 落盘与 `YUSHAN_BENCH_OUT` | 原本**都没有**。A1 新增 `bench-summary.json`、env 覆盖与损坏备份，且不改 stdout 表格。 |
| **D3** | `child_limits_for` 已用 effective | 原本用 `cfg.context_window_for(name)` ⇒ **子 agent 绕过父级封顶**。已改为 `effective_context_window_for`，回归钉 `child_limits_for_uses_effective_window_not_declared`。 |
| **D4** | `/status` 渲染在 `apps/coding-agent/src/view.rs` | 实际渲染在 `crates/ys-tui-coding/src/commands.rs`；`CodingView` 只带 effective，故新增 `context_window_declared` 跨 crate 传递。 |
| **D5** | `main.rs:4434/4440` 是启动日志 | 那是**测试断言**；真实启动出口是 debug `run_start` 的 `limits` 字段（`main.rs:850-858`）。 |
| **R-bench** | bench 读 `run_reading` 顶层键 | 生产一度只写 `line` ⇒ bench 会读成**假 0**。现生产**同时**写顶层数字字段，bench 顶层缺失时再从 `line` 反解。 |
| **R-corrupt** | `bench-summary.json` 损坏时新建数组 | 原实现会把非法 JSON **静默当空数组覆盖**（丢掉付费历史基准）。现先改名备份，备份失败则跳过写入。 |
| **R-fail-turn** | 读数行在终局事件后写 | 若 `run_result` 先 `?` 传播，**失败回合的读数会丢失**。现 `finish_after_reading` **先**写读数行再传播错误。 |

## 测试与验证

- **round_clock**：多轮累加、`summary_line` 定形正则、缓存分母只算上报轮、全未上报 `n/a`、
  零轮不除零、中位数奇偶。
- **config**：三标量解析与非法值丢弃、`min` 语义（1M+200k / cap=0 / 128k+200k）、
  `effective_context_window_for(子模型)`。
- **read**：`required` 不含 `path`、只给 `paths` 成功、两者都缺 `InvalidInput`、
  单元素 `paths` 回落单文件。
- **prompt**：聚合指引含 `Explore with one command` / `ONE command` / `rg -n --context` /
  `.gitignore`，且**不含**旧四条逐工具禁令；只在只读工具（`read|grep|find|ls`）在场时出现，
  只有 `bash` 时不得出现；写入指引在 `edit|write` 在场时出现、含 `never with scripts` 与
  「拆开多次调用没问题」；A7 任务边界 guideline 在 `bash` 在场 / 空工具表时均出现。
- **ys-loop**：A4 累计单调（多调用轮不清零）、到阈值且 `tpr < 1.5` 触发并 12→24→48 翻倍、
  `tpr >= 1.5` 不触发且阈值不变、阈值 0 永不触发；A6 60→120→240 与每回合重置；
  三条同时置位只发一条且为 narration、标志顺延、不落 session。
- **接线**：`finish_after_reading` 先写读数再传播错误且回合错误优先、`run_reading` 带结构化字段、
  `child_limits_for` 回归钉、`/status` 的 `(capped)` 与单值两态。
- **bench**：`Reading` JSON 字段齐全、缺失 `run_reading` 时写 `null`、`YUSHAN_BENCH_OUT`
  固定路径追加、损坏备份。
- **变异**：A4 说话清零 / A6 不翻倍 / A5 用 declared / 缓存分母算全部轮 / 优先级取最后一个
  `Some` —— 每项注入后对应用例变红，回滚后无残留。
- **N16 门禁（MANUAL_ACK_REQUIRED，未执行）**：`fact/1` / `exp/t1` / `daily/2` 各 ≥3 次，
  对比 `bench-summary.json` 的 `rounds` 降幅 ≥15%、`cache_hit` 不低于 98.3%、`QUALITY_SPECS`
  不回退。它只决定「第一刀是否达成目标、是否启动第二刀」，代码正确性由上面的用例覆盖。
