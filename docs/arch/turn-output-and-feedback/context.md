# turn-output-and-feedback — 架构上下文

> 日期：2026-10-12
> 产出：`/arch-explore`。证据均带 `文件:行号` 或会话 JSONL 行号（基线 `3c9aef5`）。
> 缘起（用户原话）：看 `ys-coding-agent` 的会话 `1791776859_054505669`，两个问题需要改进：
> ① **一次性读取太多内容**；② **改进过程迟迟没有回复**。
> **用户决策（本轮）**：A、B **一起做**、作为**同一需求**；改进要**可量化、可测试**（见「量化验收」）。

## 概述

一句话：一个真实会话暴露了两类与「**一次 Agent Turn 的输出规模与可见性**」有关的问题 ——
工具结果在单回合内可以**无声膨胀**（单条可达 ~11k token，且没有回合级总量闸门），
而模型返回**空 / 纯思考响应**时回合**静默以 `Completed` 收场**、长工具循环又缺乏过程叙述，
用户看到的是「等了很久、没有任何答复」。

- **子问题 A · 一次性读取太多内容**（工具面：单条结果偏大无提示、`context` 成倍放大、无回合级闸门）。
- **子问题 B · 改进过程迟迟没有回复**（循环 / 渲染面：空响应静默收场、长循环无过程叙述）。

这两个子问题**同源**（都是「单回合的信息量没有边界 / 不可见」），故**合并为一个需求一起做**；
但两者**变量不同**（A 动工具与 prompt，B 动循环与 TUI），设计时按两个子问题分别给方案。

本文件只做**现状 + 约束 + 范围 + 量化验收**，**不做方案设计**（方案交 `arch-design`）。

## 现状（问题证据）

### 采样口径

会话文件 `~/.yushan/sessions/1791776859_054505669.jsonl`
（provider `opencode-go` / model `deepseek-flash` / `thinking_visible=false`）。
统计脚本按 `content[]` 里的 `ToolUse` / `ToolResult` / `Text` 计数（纯读，不改会话文件）。
复算脚本见文末「量化验收 · 复算」。

| 指标 | 值 |
|---|---|
| 回合数（`meta.turns`） | 4 |
| 累计 `input_tokens` / `output_tokens` | 1,141,152 / 36,688（≈285k input / 回合） |
| 工具结果条数 / 合计字符 | 37 / 191,599（≈48k est token） |
| **单条最大工具结果** | **45,255 字符（≈11k token）** —— 一条 `grep{pattern:"M4", context:4}`（JSONL 第 8 行），**未触发任何 notice** |
| 次大 | 24,253 字符 —— `grep{pattern:"起屏", context:2}`（JSONL 第 24 行） |
| **空 assistant 消息** | `{"role":"Assistant","content":[]}` 两条（JSONL 第 35、37 行） |

> ⚠️ 单会话、单模型，只能作**定性**依据；但 45KB 级的单条结果、≈285k input/回合、
> 两条空 assistant 都是可指到代码的形态，不是比例问题。

### 子问题 A · 一次性读取太多内容

- **单条结果上限很宽，且在途中没有「偏大」信号**。`read` / `grep` 输出的最终闸门是
  `ys_tool::MAX_LINES = 2000` / `ys_tool::MAX_BYTES = 50KB`（`crates/ys-tool/src/truncate.rs:22,24`）；
  `grep` 只在**命中 `limit`** 或**渲染后超 50KB** 时才拼 notice（`adapters/tools-basic/src/grep.rs:230-256`）。
  45KB 的结果两次都落在闸门**之下**，于是模型完全不知道自己刚吞了 1 万多 token。
- **`context` 参数会成倍放大输出**。`grep` 默认 `limit=100`、`context=0`
  （`adapters/tools-basic/src/grep.rs:45,48`），但 `context=N` 把每条命中扩成 `2N+1` 行；
  短而宽的 pattern（如 `M4`）配 `context:4` 是输出爆炸的典型形态。
- **没有回合级总量闸门**。上限全是**单条**的（`limit` 条 / `MAX_LINES` 行 / `MAX_BYTES` 字节），
  循环里没有「一个回合内累积工具结果超过 N token 就降级更早结果」的判据；
  `crates/ys-loop/src/compact.rs` 的压缩是**窗口触发**的，看不见单回合内的膨胀。

### 子问题 B · 改进过程迟迟没有回复

- **空 / 纯思考响应被当成正常 `Completed`**。适配器只把「正文 text + tool_calls」装进消息，
  `reasoning_content` 只发事件、**不进 content**（`adapters/model-openai-compatible/src/lib.rs:668`、
  `:731-745`）；于是「只有思考、没有正文、没有工具调用」的响应收敛成
  `content == []`。`BasicLoop` 走 `tool_calls.is_empty()` 分支时，`text_content` 也为空 ⇒
  直接发 `StopReason::Completed` + `final_message: None`，**静默收场**
  （`crates/ys-loop/src/basic.rs:448-469`）。用户看到的是「Working 转很久 → 没有任何答复」，
  于是连着追问「汇报状态」「汇报改进状态」。
- **同一形态还会经「内联思考标签」产生**：MiniMax 那类把 ` thinking…` 塞进 `delta.content` 的模型，
  整段被 `InlineThinkingFilter` 判为 Thinking ⇒ `text` 为空 ⇒ 同样落进空消息路径
  （见 `docs/arch/hide-inline-thinking/context.md`）。
- **长工具循环缺少过程叙述**。状态行有 `⏳ Working. · N rounds · x.xs`
  （`crates/ys-tui-coding/src/draw.rs:573-579`，随 `RunFinished` 才落一行 Summary，
  `crates/ys-tui-coding/src/app.rs:263-274`），但 transcript 里**没有「正在做什么」的一句话**；
  thinking 默认隐藏（`crates/ys-tui-coding/src/app.rs:52`）。模型不主动写过程，用户就只看到一堆工具行。

### 既有相关决策（本需求必须与之对齐，勿重复劳动）

| 现成产物 | 覆盖了什么 | 与本需求的关系 |
|---|---|---|
| `docs/arch/tool-call-efficiency/context.md`（2026-10-10） | 工具调用形态（`cd` 前缀、多目标、并行只读）；约束「**不放宽任何输出上限**」 | A 的正向：只在**形状 / 提示**上收窄，不放大上限 |
| `docs/arch/llm-api-unavailable/context.md` | **传输层**空响应（200 + 非 SSE 体）、超时、半截流；**场景 4 明确「合法空回复（无内容、无工具、`finish_reason=stop`）是正常收尾，不得误伤」**（该文件 152-153 行） | B 的**边界必须与它对齐** —— 本需求管的是「合法但仍无可见产出」那一档，**有意收窄其「正常」判定** |
| `docs/arch/long-horizon-completion/design.md` | 长程「完成」判据；445 行记「静默把半截 / 空响应当 `Completed` = 静默丢工作」 | B 的同源账；`StopReason` 至今仍 4 态（`crates/ys-core/src/stop.rs`），说明**未落地** |
| `docs/arch/tui-resident-status/`、`tui-status-display/` | 常驻 footer / 状态行形态 | 过程叙述若要落到状态行，须与它们的形态约定一致 |

## 现有架构

### 模块边界

| 层 | 关注点 | 关键文件 |
|---|---|---|
| 工具 | 单条输出上限 / notice（`ys_tool::truncate` + `append_notices`） | `adapters/tools-basic/src/{read,grep,find,ls,bash}.rs` |
| 循环 | Step 5 组装请求（工具表每轮重发）/ Step 6 `model.complete` / Step 7 append / Step 8 判 `tool_calls` / Step 9 执行 | `crates/ys-loop/src/basic.rs` |
| 模型适配器 | SSE 累积 → `ModelResponse`（只含 text + tool_calls） | `adapters/model-openai-compatible/src/{lib,stream}.rs` |
| 事件 | 观察层：`AgentEvent`（含非终局 `ResponseTruncated` / `ContextCompacted`） | `crates/ys-event/src/event.rs` |
| 应用 / TUI | transcript 渲染、状态行、`/thinking` 开关 | `apps/coding-agent/src/`、`crates/ys-tui-coding/src/{app,draw}.rs` |

### 核心抽象

```rust
// crates/ys-core/src/stop.rs —— 4 态，#[non_exhaustive]
pub enum StopReason { Completed, MaxRounds, Cancelled, Truncated }

// crates/ys-model —— 方言归一（适配器产出，ys-loop 不解读 provider 字符串）
pub enum FinishReason { Stop, ToolCalls, Length, ContentFilter, Other(String) }
```

- `RunLimits`（`crates/ys-component/src/limits.rs`）：`max_rounds` 默认 **10**、`context_window` 128k、
  `fixed_prompt_tokens`（system prompt + 工具表固定开销，参与压缩触发判定）。
- 工具上限常量集中在 `crates/ys-tool/src/truncate.rs`（`MAX_LINES` / `MAX_BYTES` /
  `GREP_MAX_LINE_CHARS=500`）与 `adapters/tools-basic/src/lib.rs:38`（`BYTE_CAP = 1MiB`，判 rg stdout 用）。

### 关键数据流

**空响应路径（子问题 B 的承重路径）：**

```text
adapters/…/lib.rs  StreamAccumulator::finish()
  text + tool_calls ─────► Message{ role: Assistant, content: [Text?…, ToolUse?…] }
  reasoning_content ─────► 只 emit ThinkingDelta，不进 content          ← 空消息的来源
        │
ys-loop basic.rs:448  if tool_calls.is_empty()
        ├─ text 非空 ──► RunFinished{ Completed } + final_message = Some(msg)
        └─ text 为空 ──► RunFinished{ Completed } + final_message = None  ← 静默，无可见产出
```

**工具输出上限与 notice 路径（子问题 A）：**

```text
grep.rs call → run_bounded(BYTE_CAP=1MiB, DEADLINE) → parse → build_result
  render_entries(entries, limit, context)   ← context 成倍放大
  head_lines(lines, MAX_LINES=2000, MAX_BYTES=50KB)
  notice: 仅当命中 limit / 渲染超 50KB / 单行超 500 字符 / 被中止 / 超时
```

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `rg` / `fd` | `grep` / `find` 原生后端 | 缺失走 POSIX 降级（ADR-0015）；本需求不引新依赖 |
| `sh` | `bash` 工具 | 每次新进程，`cd` 不跨调用 |
| （无新 crate） | — | 沿用「不增依赖」口径 |

## 约束

- **技术**：`ys-model` / `ys-core` / `ys-event` / `ys-protocol` **不得**引 tokio；`ModelEventSink` 保持**同步 `fn`**；
  不动「Agent 无状态 / Boundary / 信道 / 取消即消息（ADR-0013）」四条不变式。
- **设计不变量**：**不放宽任何输出上限**（`tool-call-efficiency` 已定）—— A 只能靠**更好形状 / 更早提示 / 收紧**，
  不能靠加大 `limit` / `MAX_BYTES`。
- **契约**：`FinishReason` / `ModelError` 的语义在**适配器**解读，`ys-loop` 不认识 provider 方言。
  加 `StopReason` 变体是**加法式**（`#[non_exhaustive]`，下游已有 `_ =>` 兜底，如 `draw.rs:611`），
  有先例可抄：`Truncated` 的 serde roundtrip 用例在 `crates/ys-core/src/lib.rs:107`。
- **与既有决策的冲突面（设计阶段定边界）**：`llm-api-unavailable` 把「合法空回复」判为**正常**；
  本需求要把它判为**可见的非正常收尾**（用户明确说这是要改的问题），属**有意的语义收窄**，
  设计时需在 `llm-api-unavailable`（场景 4）与本需求之间写明谁说了算。
- **文档 / 流程**：新增接口须能对应 `docs/design.md` §12 测试矩阵；设计偏离需**先改设计文档**
  （`docs/CONTEXT.md` 终止原因定义、`docs/design.md` §5）。
- **测试**：并行安全（临时目录含进程内原子序号 + `process::id()`）；关键逻辑做**变异测试**
  （每个新分支都要写清「删掉它哪条用例变红」）。
- **组织**：小团队、无 sandbox（安全外置）。

## 需求范围

### 范围内（A、B 一起做）

**子问题 A · 收窄单回合工具输出**

1. **`grep` 专用结果预算（真收窄，主手段）**：`GREP_MAX_RESULT_BYTES = 16 KiB`（**收紧**，与「不放宽上限」不冲突）。
   超预算即按 `head_lines` 截断，**截断 notice 必须带可执行的下一步**（「收窄 pattern/path/context 或加 `glob`」）。
   治「一次性读取太多」靠的是它 —— 把观察到的 45KB（≈11k token）压到 ~4k token。落点
   `adapters/tools-basic/src/grep.rs::build_result`。
2. **软阈值提示（辅助）**：在专用预算**之下**再设软阈值（8 KiB），渲染 ≥ 软阈值且未被截断时附一条
   notice（大小 + 命中数 + 收窄建议）；`read` 同样给软阈值提示（`read` 已有 `offset/limit`，**不设**专用预算）。
   落点 `grep.rs::build_result` + `crates/ys-tool/src/notice.rs`（形态统一）。
3. 给 `grep` 的 `context` 参数设**上限**（建议 `MAX_CONTEXT = 5`），超限时钳制并附 notice。
   **注意**：它**不覆盖**观察用例（`context=4`）；目的是防更宽的放大，**不是**主手段（主手段是 #1）。
4. **回合级工具输出总量闸门**（第二批 · **已落地**，`crates/ys-loop/src/turn_budget.rs` + `basic.rs` Step 11）：
   一个回合内累积工具结果超 `RunLimits::max_turn_tool_result_tokens` 时，把**更早**的结果降级为
   「头部 + 显式续读指针」；最新一条永不降级，配对完整，`budget = 0` 逐字节不变。
5. `apps/coding-agent/src/prompt.rs` 的 `tool_guidelines`（`:154`）补一条
   「先窄后宽：先 `grep` 定位再 `read` 指定区间，避免整文件 / 大 `context`」。**进 `fixed_prompt_tokens`**，按 token 计价。

**子问题 B · 让回合输出与进展可见**

6. **空 / 纯思考响应不再静默收场**（第一批 · **已落地**）：加 `StopReason::NoOutput`（判定用 `text.trim()`，覆盖空白响应），
   TUI 在 `RunFinished` 臂推一条可见 System 行（`app.rs:263-274` 就近）；**一次性模式退出码 = 1**（无可用产出 ≠ 成功）、
   并在 stderr 写一行说明 —— **不可静默装作成功**。与 `llm-api-unavailable` 的边界见 `design.md`（谁负责把传输层空转成错误）。
7. **空响应重试一次**（第二批 · **已落地**，`basic.rs` 空产出分支 + `RunLimits::retry_empty_response`）：
   无正文无工具调用时带一句催促重发一次（消耗一个轮），仍空才如实上报；`false` 时与第一批逐字节一致。
8. **长循环过程叙述**（第二批 · **已落地**，`apps/coding-agent/src/prompt.rs::tool_guidelines`）：
   加「多轮工具任务每批前用一句话说明正在做什么」（便宜、可测），仅工作型工具在场时出现；
   （叠加）状态行补「当前工具」后置。

### 范围外（明确不做）

- **传输层**空响应 / 超时 / 非 SSE 错误体 / 半截流 —— 归 `llm-api-unavailable`。
- **自动重试 / 退避 / 多 provider 故障转移** —— `llm-api-unavailable` 明确不做
  （「空响应重试一次」列第二批，见上）。
- **放宽任何输出上限**；**引入新依赖**；**持久 shell / 跨调用 cwd**。
- 改模型侧（每轮注入常量文本）—— 缓存经济学，见 `docs/arch/prompt-delivery/notes.md`。

### 关键场景（成功路径）

- **S1 · 窄探索**：模型先 `grep` 定位再 `read` 区间；单条结果远低于软阈值，不再出现 45KB 级结果。
- **S2 · 工具结果偏大**：即便出现大结果，也带一条提示，模型下一轮自行收窄。
- **S3 · 空 / 纯思考响应**：回合不再静默 `Completed`，用户拿到一条可见说明（或经重试拿到真实回复）。
- **S4 · 长工具循环**：每批前有过程叙述，用户能判断 agent 在推进而非卡死。

### 异常 / 边界场景

- **合法空回复的处置**：本需求**有意**把「无内容、无工具、`finish_reason=stop`」判为可见的非正常收尾
  （覆盖 `llm-api-unavailable` 场景 4 的「正常」判定）；边界须在设计里写死，避免两处各说各话。
- **内联思考标签全被剥离**（整段 `delta.content` 是 ` thinking`）：与「纯 reasoning_content」应当**同一条路径**处理。
- **只读批次并行 / 混合批次串行**：任何新增 guideline 不得破坏既有准入规则。
- **回合级闸门与压缩的关系**：降级更早工具结果时，不得与 `compact.rs` 的窗口触发生冲突。
- **非空正常收尾不受影响**：这是**反向验收**（见下），既有全量用例必须保持绿。

## 量化验收（可测目标）

> 每个改动都要有「**指标 + 基线 + 目标 + 测法 + 变异点**」。这是本需求「可量化、可测试」的落点。

### 基线（来自本会话，可复算）

见「采样口径」表：单条最大 45,255 字符；累计 191,599 字符 / 37 条；空 assistant 2 条；≈285k input / 回合。
**这些既是问题证据，也是改进前的对照基线**（改动后用同一脚本复算、比对）。

### A · 工具输出（可单测 + 可变异）

| 目标 | 测法 | 变异点（注入 bug 应使其变红） |
|---|---|---|
| **A0 单条结果字节 ≤ 预算（真收窄 · 主手段）** | `grep` 单测：造 ≥16 KiB 命中集 ⇒ 断渲染输出 ≤ `GREP_MAX_RESULT_BYTES` 且截断 notice 含「收窄」指引；**复算本会话用例 45KB → ≤16KB** | 去掉专用预算（退回 50KB）/ 去掉截断指引 |
| A1 结果 ≥ 软阈值 ⇒ 必带 notice | `grep` 单测：造 ≥8 KiB 命中集 ⇒ 断 notice 存在；小集合 ⇒ 断不存在 | 删掉软阈值 notice 分支 |
| A2 `context` 超限被钳制 + notice | 传 `context: 999` ⇒ 断渲染行数 ≤ `2·MAX_CONTEXT+1` 且 notice 出现（**注**：不覆盖 `context=4` 的观察用例） | 去掉钳制 / 去掉 notice |
| A4 guideline 就位 | `prompt.rs` 单测断子串；工具表顺序 / 漂移断言不变 | 删该 guideline 行 |

### B · 空响应可见（可单测 + 可 e2e）

| 目标 | 测法 | 变异点 |
|---|---|---|
| B1 空响应 ⇒ 专门停止原因（非 `Completed`） | `ys-loop` 单测：`MockModel` 返回 `content=[]` ⇒ 断 `RunResult.stop_reason == NoOutput`（及 `AgentEvent::RunFinished.stop_reason` 同值） | 把空分支改回 `Completed` |
| B1 TUI 有可见说明 | `ys-tui-coding::app` 单测：喂该 `RunFinished` ⇒ 断 transcript 出现非空 System 行 | 不推 System 行 |
| B1 `-p` / `--json` 行为 | e2e：空响应 ⇒ 终局事件 `NoOutput`、**退出码 1**、stderr 一行说明（`-p` 不再静默成功）；`--json` 的 Envelope 带 `NoOutput` | 退出码改回 0 / 不打 stderr 行 |
| 反向：非空正常收尾不受影响 | `cargo test --workspace` 保持全绿（**不降级**；`AGENTS.md` 记 975/7，实测疑已过期，以运行结果为准） | — |

### 汇总（指标 → 目标 → 测）

| # | 指标 | 基线 | 目标 |
|---|---|---|---|
| 1 | **单条工具结果字节（max / p95）** | max **45,255**（会话内） | `grep` max **≤ 16 KiB**（≈4k token），p95 低于预算 |
| 2 | **当轮回喂 token（工具结果合计）** | ~48k est token（会话内 37 条） | 同任务显著下降（会话复算对照） |
| 3 | 单条「偏大且无 notice」的工具结果数 | 2（会话内） | **0** |
| 4 | 空 assistant 回合数 | 2（会话内） | **0**（改为 `NoOutput` + 可见行） |
| 5 | 全量测试 | 见上（`AGENTS.md` 记 975，疑过期） | 不降级，新增用例各自带变异点 |

### 回归口径

- `cargo test --workspace` 全绿（不低于是次基线）。
- 每条新增用例在文档注释里写明**变异点**（删 / 改哪行会使它变红）。
- `cargo clippy --all-targets` 与 `cargo fmt --check` 干净。
- 会话复算脚本：对本会话重跑「采样口径」表，改动前后各一次，数值入档。

### 复算（内联脚本，仅读、不改会话）

```python
# 用法：python3 - ~/.yushan/sessions/1791776859_054505669.jsonl
import json, sys
L = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
meta = L[0]
res = [b["ToolResult"] for d in L[1:] if isinstance(d.get("content"), list)
       for b in d["content"] if isinstance(b, dict) and "ToolResult" in b]
sizes = [len(x.get("content", "")) for x in res]
empty = [i + 1 for i, d in enumerate(L)
         if d.get("role") == "Assistant" and d.get("content") == []]
print("turns:", meta.get("turns"), "input:", meta.get("input_tokens"),
      "output:", meta.get("output_tokens"))
print("tool_results:", len(res), "total_chars:", sum(sizes),
      "max_single:", max(sizes, default=0))
print("empty_assistant_lines:", empty)
```

## 决策与未澄清问题

### 已定（用户确认 / 采纳建议）

- **Q1 拆分**：A、B **一起做**，作为**同一需求**（用户确认）。
- **验收口径**：**可量化、可测试**（用户确认）—— 落成「量化验收」的指标表。
- **Q3 出口形态**：**倾向**加 `StopReason` 变体 + TUI 可见行（加法式、最可测）；design 阶段可对比「复用非终局 System 行」。
- **Q4 重试**：**第二批**（先只做可见出口）。
- **Q5 回合级闸门**：**第二批**（先做单条软阈值 + `context` 上限 + guideline）。
- **Q6 过程叙述**：**先加 guideline**，状态行改动后置。

### 仍待确认（design / 实现前拍板）

- [ ] **soft 阈值与 `MAX_CONTEXT` 的具体取值**：软阈值 8 KiB、`MAX_CONTEXT = 5` 是建议初值，
      需用真实会话复算校准（可能过宽或过严）。
- [ ] **`StopReason` 变体的命名**：`NoOutput` / `Empty` / `NoContent`？需与 `docs/CONTEXT.md` 术语风格一致。
- [ ] **与 `llm-api-unavailable` 的边界落笔**：本需求「有意收窄」场景 4，写进哪份文档、由谁兜住，
      需要在两份文档间显式交叉引用。
- [ ] **`read` 是否同批处理**：A1 是否一并给 `read` 加软阈值提示（本需求建议「是」）。
- [ ] **会话健康指标是否固化成脚本**：单条结果分布 / 空回合数要不要进仓库内的脚本。

## 后续建议

- 建议用 `arch-design` 做方案设计，覆盖两个分叉点：**空响应的出口形态**（Q3）与
  **A 的软阈值 / 钳制落点**（工具 `build_result` vs `ys_tool` 通用层）。
- **落地顺序**（与「量化验收」对齐）：
  1. **第一批**：B5（空响应可见，限 `basic.rs:448` 一个分支）+ A1 / A2 / A4（都在工具 / prompt 内部，独立可测）。
  2. **第二批**：A3 回合级闸门、B6 重试。
  3. B7 过程叙述的状态行部分随 TUI 形态一并定。
- 设计定稿后需同步：`docs/CONTEXT.md`（终止原因定义）、`docs/design.md` §5（失败与终止语义）
  与 §12（测试矩阵），以及上文「既有相关决策」表里被触及的旧文档。
