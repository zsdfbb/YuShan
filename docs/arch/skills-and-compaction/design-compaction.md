# 自动压缩上下文加固 — 架构设计

> 上承：[`context.md`](./context.md)（现状、约束、决策 D1–D6）。
> 本设计只覆盖 **D4 的第一刀（压缩加固）**；skills 另有独立设计。

## 概述

把已经跑着但**不可见、无测试、窗口恒 128k、带三个真 bug** 的自动压缩，加固成一个**可配置、可观测、可测试、能被 `/compact` 复用**的显式操作。**算法本身（切片 + 一次摘要调用）不重写**（D3）。

---

## 设计方案

### 模块划分

| # | 模块 | 职责 | 新增? |
|---|---|---|---|
| M1 | `crates/ys-loop/src/compact.rs` | 压缩算法唯一宿主：`plan()` 纯函数定切点 + `execute()` 跑摘要并产出新消息表。**与 `RuntimeContext` 解耦** | **新（~180 行）** |
| M2 | `crates/ys-component/src/compact.rs` | `CompactPolicy` 纯数据 + `trigger_at()` / `keep_budget()` 两个纯函数（零外部依赖，无 async） | **新（~50 行）** |
| M3 | `crates/ys-loop/src/basic.rs` | Step 4b 唯一自动触发点：算账 → 调 M1 → emit 事件 | 改（~10 行） |
| M4 | `crates/ys-event` | `ContextCompacted` / `ContextCompactionFailed` 两个变体 | 改（+2 变体） |
| M5 | `crates/ys-session` | `Session::replace_all`（**带默认实现**，零破坏）+ `JsonlSession` 覆写为单次原子写 | 改（+1 trait 方法） |
| M6 | `apps/coding-agent` | `config.json` 读取 + 三级解析；`build_agent` 接 `.limits()`；每回合刷新；`/compact` 接真压缩 | 改 |

**为什么不建 `Compactor` trait / `TriggerSource` trait / 压缩 hook**：

- `ys-component` **零外部依赖**（实测 `Cargo.toml` 只有 5 个 `ys-*`，无 `async-trait`）→ 它**放不下** async 策略 trait，结构上被迫把「策略数据」与「算法」分开——这恰好是对的那刀
- 依赖环：`ys-component` 已被 `ys-event` 依赖（`RuntimeContext.events: &mut dyn EventSink`）⇒ **`ys-event` 不能反向依赖 `ys-component`** ⇒ 新事件只能带**平铺字段**，不能带策略结构体
- 策略 trait 若定义在 `ys-loop`，`RuntimeContext` 就再也装不下它（成环）→ 唯一落点是 `Agent` 字段 → 触发 ADR-0007，把「执行器的字段」扩成「执行策略容器」。成本高、收益为零
- 升级路径（写进 ADR 防后人重走）：真有第二种算法时，把 `plan`/`execute` 的分派点收拢成 `enum CompactStrategy { Summary, … }`（照 ADR-0016「差异只是数据 → 用表不用 trait」），**`basic.rs` 调用点一行不改**

### 关键数据流

#### 自动压缩路径（`BasicLoop` Step 4b，每轮 model 调用前）

```
Step 4b（替代 basic.rs:130-137）
  win    = ctx.limits.context_window
  policy = &ctx.limits.compaction
  est    = estimate_session_tokens(session.messages())

  if est <= policy.trigger_at(win)  →  跳过（绝大多数轮，零事件、零行为变化）

  plan = compact::plan(messages, policy, win)          ← 纯函数、同步、微秒级
    │  None → 跳过
    ▼
  execute(messages, &plan, ctx.model, policy).await    ← 唯一 await 点（一次 model 调用）
    ├─ Ok(outcome) → session.replace_all(outcome.messages)
    │                emit ContextCompacted { before, after, summarized, kept }
    └─ Err(e)      → **session 一个字节都不动**
                     emit ContextCompactionFailed { reason }
  → Step 5 组装 ModelRequest（messages 已是压缩后的）
```

**`plan()` 必须在 `execute()` 之前、且是纯同步的** —— 两个理由：
1. 「要不要压、压多少」在**发事件之前**就已知，事件不会撒谎
2. **承重的既有耦合**：`app_loop.rs:770-822` 的 `every_request_variant_is_handled` 会走到这里，它安全**只因为**会话只有 2 条小消息 → `plan()` 返回 `None` → **不调用 model**（否则 `MockModel::remove(0)` 空队列 panic）。早退必须留在 model 调用**之前**，这条要写进 `compact.rs` 的注释

#### 手动压缩路径（`/compact`）

```
app_loop: Request::Compact
  → capabilities::compact(&mut wiring, policy, window).await
       let ports = wiring.ports();              ← 现成的 AgentPorts 同时给出 model + session
       ports.model == None → ["Cannot compact: no model configured. Use /login."]   ← 绝不静默退回 clear
       plan(session.messages(), ...) → None → ["Nothing to compact."]
       execute(session.messages(), &plan, model, policy).await
          Err(e)      → ["Compaction failed: {e}"]                    ← session 未动
          Ok(outcome) → ports.session.replace_all(outcome.messages)   ← 调用方做写入（R3）
                        → ["Compacted: ~850k → ~50k tokens (N summarized, M kept)."]
```

> **R3**：`execute` **不持有 session**（它对的原因），所以 `replace_all` 由**调用方**做 —— 自动路径是 `basic.rs`，手动路径是 `capabilities::compact`。两条路径形状一致，只是报告机制不同（事件 vs `Output`）。

**手动路径只返回 `Vec<String>`（→ `Outbound::Output`），不发事件**。理由：`app_loop` 的 `Request::Compact` 臂**不在** `select!` 循环里（`app_loop.rs:119-126`），若 emit 事件，它会躺在 `sink_rx` 里直到下一个 `Prompt` 才被转发 → transcript 顺序错乱。两个入口报告机制不同，但底层是**同一份 `CompactionOutcome`**。

#### 配置解析路径

```
启动期（main，一次）
  config.load_user_config(~/.yushan/config.json)     ← 只读；不存在 = 全默认；失败落日志、绝不阻断启动
  …
  window = config.context_window()                    ← 三级解析
       ① config.json 的 context_window                （全局，最具体）
       ② ProviderInfo.context_window（当前 provider）  （内置表）
       ③ RunLimits::default().context_window = 128_000（兜底）

每回合（app_loop 的 Prompt 臂开头，一行）
  agent.set_context_window(config.context_window())
     ← 必须在每回合刷新：/login、/model 会在运行中换 provider（capabilities.rs:103/168），
       只在 build_agent 解析的话，用户 /login 到 1M 模型后仍按 128k 压缩 —— 这正是要修的 bug。
       单一调用点，结构上不可能漏。
```

`config.json` 形状（全部字段可选，缺省即默认）：

```json
{
  "context_window": { "deepseek-chat": 128000, "my-1m-model": 1000000 },
  "compaction": { "trigger_ratio": 0.85, "keep_ratio": 0.10 }
}
```

### 比例（2026-09-19 定稿：触发 85%，保留 1~10%）

```rust
trigger_at(w) = min(w × trigger_ratio,
                    w − output_reserve − CONTEXT_HEADROOM)   // 评审 R4
keep_budget(w) = clamp(w × keep_ratio, w × keep_min_ratio, w × keep_max_ratio)
// trigger_ratio = 0.85、keep_ratio = 0.10、keep_min_ratio = 0.01、keep_max_ratio = 0.10
```

**`keep_ratio` 取 10%（带上沿，2026-09-19 定）**，理由三条：

1. **带内成本差很小** —— 1M 下 1% vs 10%：均值上下文 430K vs 475K、重建项 0.12K vs 1.33K eq/轮，合计差约 **10%**。对比 50% 那档的 **1.66x**，带内怎么选都不贵
2. **三家收敛点就是 ~10%** —— pi/Codewhale/Claude Code 都用 20k 固定值，÷ 200k 窗口 = 10%
3. **先保真后调低，比先有损后调高安全** —— 按 D8 用事件调；从高往低调，调过头有事件数据兜底；从 1% 起步则在收到数据前**一直在丢细节**

> 体感换算（1M）：10% = 100k ≈ **最近 10~20 次工具调用**（合理的工作集）；1% = 10k ≈ **1~2 次工具调用**（太薄）。
> 注意：`keep_min_ratio` / `keep_max_ratio` 仍是**硬边界**（结构上保证落带内），`keep_ratio` 是带内的调参点。

**为什么要减 `output_reserve`（评审 R4）**：窗口同时要容纳**模型输出**（app 装配 `max_tokens = 4096`，`main.rs:610`）与 provider 的 framing 开销。只按 85% 触发时：1M 下剩余 150K ≫ 4096 ✅，但**窗口现在可配了** —— 8K 窗口下 `6800 输入 + 4096 输出 > 8192` ⇒ 溢出，而**溢出恢复已按决策 8 延后** ⇒ 硬失败。Codewhale 正是这么做的（`input_budget_ceiling = window − output_cap − 1024`，`context_budget.rs:203`）。

> `output_reserve` 与 `CONTEXT_HEADROOM`（1024，照 Codewhale）进 `RunLimits`；「减输出预留」是**下限钳制**，不影响 1M 场景（那里 85% 更小）。

| 窗口 | 触发（85%） | 保留（1%~10% 带内） |
|---|---|---|
| 1M | 850_000 | 10_000 ~ 100_000 |
| 200k | 170_000 | 2_000 ~ 20_000 |
| 128k | 108_800 | 1_280 ~ 12_800 |

**「1~10%」实现为一个带（band），不是单点** —— `keep_min_ratio` / `keep_max_ratio` 是**硬边界**（结构上保证 keep 落在此带内），`KEEP_DEFAULT` 是带内的调参点。

**为什么这样能同时修掉 D-1**：`trigger = 0.85w`、`keep ≤ 0.10w` ⇒ **`trigger > keep` 在任何窗口下恒成立** ⇒ `keep_from` 永不为 0 ⇒ 不再有「阈值已超但保留预算更大」的永久空转。**D-1 由构造消除，不需要额外的钳制。**

> ⚠️ **「128k 逐位等价」这条迁移护栏正式放弃**（D6 修订）。旧值 `111616 / 20000` 是从 pi 抄来的、无测量依据，且 128k 下也会变成 `108800 / ~12800`。**这是有意的行为变更，须写进 PR 描述。**

**对照三家同类实现**（本设计定稿时的依据）：

| | 触发 | 保留 | 保留占窗口 |
|---|---|---|---|
| pi | 窗口 − 16384 | 20_000 固定 | ~10%（@200k） |
| Codewhale | 窗口 × 80% | 20_000 固定（clamp 2k–200k） | ~10%（@200k） |
| Claude Code | ~83.5% | 2_000–20_000 | ~1%~10%（@200k） |
| **本设计** | **窗口 × 85%** | **窗口 × 1%~10%** | **1%~10%** |

> 三家在**触发点**上落在 80%~92%，85% 居中；在**保留量**上独立收敛到「约 10% 窗口」。**唯一的分歧是缩放方式**：三家用「固定 20k」（小窗口下占比很高、大窗口下占比很低），本设计用**按窗口比例**（大窗口下保留更多，符合 1M 的意图）。**这是有意的偏离，理由见 KV cache 一节。**

### 摘要内容策略（2026-09-19 定稿：采用 pi / Codewhale 的形状）

采用 Codex → pi → Codewhale 一脉相承的做法：**结构化骨架 + 显式保留精确引用**。

- **骨架**（pi 的固定 Markdown 节）：`## Goal` / `## Constraints & Preferences` / `## Progress (Done / In Progress / Blocked)` / `## Key Decisions` / `## Next Steps` / `## Critical Context`
- **显式保留**：**精确文件路径、命令、错误文本**（Codewhale `COMPACT_PROMPT` 原文：*"exact file paths, commands, and error text"*）
- **追加文件清单**：`<read-files>` / `<modified-files>`（pi 的做法；Codewhale 亦有 `details { readFiles, modifiedFiles }`）
- **明确排除**：meta 叙述（"我压缩了上下文"之类）—— Codewhale 原文要求 *"Summarize the task, not the checkpoint machinery"*

> ⚠️ **这推翻了 D11 的初稿**（初稿为「只描述最主要的、引用类可回找」）。推翻理由：**「改过哪些文件」是重复劳动 / 覆盖文件的唯一直接入口**，而重读文件的 token 成本通常远高于在摘要里留几个路径。Codex / pi / Codewhale 三家独立收敛到同一形状，是强证据。
>
> **这把「结构化摘要」从 D3 的延后项提进了第一刀** —— 它不再是「换一句话」，而是一段有明确结构的 prompt。
>
> **落法**：`CompactPolicy.summary_system_prompt` 保留为**覆盖口子**，默认值为上述结构化 prompt（常量）。**不做** pi 的 `UPDATE_SUMMARIZATION_PROMPT` 增量合并（另一件事，仍延后）。
> **不做** `summary_max_tokens` 字段 —— 见「评审处置 R2」。

### KV cache 成本（2026-09-19 增补）

对 agent 循环而言，**KV / 前缀缓存是最大的成本杠杆**：命中时按全价的 10%~25% 计费，未命中则全价。YuShan 现状有三处漏。

| # | 漏 | 证据 | 处置 |
|---|---|---|---|
| **K-1** | **摘要调用必然 100% 缓存未命中** —— `generate_summary` 构造**全新 `ModelRequest`**（`system` 换成 summarizer 提示、`tools: vec![]`）⇒ 序列化后第一个 token 就不同 ⇒ 整段上下文全价重发。这是系统里**最大的单次输入**（1M/触发 850K 下 350K–840K token） | `basic.rs:469-500` | **改成「活对话 + 一条末尾指令」** —— 不改 `system`、不改 `tools`，把摘要指令作为**最后一条 `Role::User` 消息**追加。前缀 `[system, tools, m1..m_k]` 与上一轮主调用**逐字节相同** ⇒ 命中。**Codewhale 的做法，其源码有明确注释**（`tmp/Codewhale/crates/tui/src/compaction.rs:1495-1500`） |
| **K-2** | **`Usage` 无缓存字段 ⇒ 完全盲** —— adapter 只解析 `prompt_tokens`/`completion_tokens`；DeepSeek 的 `prompt_cache_hit_tokens`、OpenAI 的 `prompt_tokens_details.cached_tokens` 都没读 | `crates/ys-core/src/usage.rs`、`adapters/model-openai-compatible/src/lib.rs:245-250` | `Usage` 加 `cache_read_tokens: u32`（+ 可选 `cache_write_tokens`），adapter 解析两种形状；`/status` 与压缩事件都带上。**这是 K-1/K-3 能否验证的前提** |
| **K-3** | **压缩必然击穿缓存** —— 压缩后前缀变 `[summary, kept…]`，与压缩前从第一位就不同 ⇒ 保留段整段重发一次全价 | 设计所决定（无法消除） | 只能**减小**：keep 越小，这次重发越省。**不可消除**（任何删头操作都会改变前缀） |

#### 澄清：主循环命中，只有「压缩后那一轮」不命中

**前缀缓存是「从第 0 个 token 起逐位匹配」的，按位置索引，不按内容。**

- **主循环 ✅ 命中**：turn N+1 的请求 = turn N 的请求 + 追加消息 ⇒ 前缀 `[system, tools, m₁..m_k]` 逐字节相同 ⇒ **命中长度随轮数持续增长**。这正是 agent 循环便宜的原因。下表「① 每轮缓存读」用的就是命中价（×0.25）；若不命中，该项应为 700K（全价）⇒ 合计 760K eq/轮，**4 倍差距**。
- **压缩后那一轮 ❌ 必然 miss**：

  ```
  压缩前缓存的前缀:  [system, tools, m₁, m₂, m₃, …, m_j, …, m_N]
  压缩后的请求:      [system, tools,[summary], m_j, …, m_N]
                                ↑ 第 3 个位置就不同
  ```

  从那一位起**全部作废** —— 包括**逐字原样保留**的 `m_j..m_N`：它们内容没变，但**位置变了**。

- **且不可消除**：任何「缩短上下文」的操作都必然改变前缀起点 —— 摘要式压缩 `[summary, kept]` ❌、只截头部 `[m_j..m_N]` ❌、不删头只追加摘要 → 上下文没缩短，等于没压缩。
  ⇒ **压缩的真实成本 = 摘要调用 + 保留段的一次全价重发**，不只是摘要那一次。**这是「keep 越大代价越高」的主要依据。**
- **K-1 与循环无关**：摘要调用是**独立请求**，且 `system`/`tools` 都被换掉 ⇒ 它自己 100% miss，与「循环增长」无关。

**量化**（1M 窗口、触发 85%、缓存读 = 全价 ×0.25、每轮新增 10K token）—— **这是定稿 keep 带的直接依据**：

| | 保留 50% 窗口（曾议） | 保留 ~1% 窗口 |
|---|---|---|
| 循环周期 | 350K 新内容（35 轮） | 840K（84 轮） |
| 每轮上下文均值 | 675K | 435K |
| ① 每轮缓存读 | 169K eq | 109K eq |
| ② 压缩后重发（摊薄） | 14.3K eq | 0.12K eq |
| ③ 摘要调用（K-1 修好后） | 2.5K eq | 2.5K eq |
| ③ 摘要调用（**今天，全价**） | 10K eq | 10K eq |
| **合计（K-1 修好后）** | **186K eq/轮** | **112K eq/轮** |

> **保留 50% 窗口比保留 ~1% 贵约 1.66x**，且 ①② 两项**都偏向小保留** —— 大保留的唯一收益是保真度，**成本上是纯亏**。加上 pi / Codewhale / Claude Code **三家独立收敛到「约 10% 窗口」**，两条独立证据同向 ⇒ 最终采用 **1%~10% 带**。

**三条结论**：

1. **主项是「每轮背着大上下文」（①②），缓存命中把它从全价变成 ×0.25 —— 这是 4 倍杠杆，比任何调参都大。** 所以第一优先级是**保证命中**（前缀稳定），不是调 keep。
2. **K-1 是零风险的净赚**（不改摘要质量、不改行为），但只改约 4% 的量级 —— 值得做，别夸大。
3. **keep=500K 的成本代价约 1.6x**（190 vs 118），且 ①② 两项**都偏向小 keep**。keep 大的唯一收益是**保真度**（逐字保留近期历史），**成本上是纯亏** —— 这个数应进 keep 决策的依据。

> **已有的好消息**：`工具面必须恒定`（ADR-0016）**本来就是一条缓存保住规则** —— system prompt 与 tools 是前缀最前两块，任何抖动都会让整条缓存失效。**D1（skills 注入 57 条描述）必须遵守同一规则**。

### 从 C 移植的三项 bug 修复

| # | Bug | 今日行为 | 修复 |
|---|---|---|---|
| **B-1** | **`keep_from == len`**：当**最新一条消息自己**超过保留预算时，尾扫永不 break → 保留段为空 → **整个会话连同本轮用户消息被摘要掉，一条不留** | 静默丢当前输入 | 硬不变量：**摘要段与保留段都必须非空**（`1 ≤ keep_from ≤ len−1`，`len < 2` 时返回 `None`） |
| **B-2** | **空摘要**：`generate_summary` 在响应无 `Text` 块时返回 `Ok("")`（如模型回了 tool-call）→ 写出一条**空摘要**，历史被静默丢掉 | 静默失忆 | 空/全空白 → 视为**失败**（走 B-3） |
| **B-3** | **压缩失败 = 静默失忆**：失败时 `clear()` + 写入一行 `[Compression failed — proceeding with truncated history]`，把 800k token 的历史换成一行占位符 | 此后每回合带着这个谎跑 | 失败 → **session 一个字节都不动** + 发 `ContextCompactionFailed` + 本轮照常继续 |

**B-3 是一处有意的行为变更**（少数场景从「带失忆地成功」变为「明确失败」）—— **必须写进 PR 描述**。

### 接口概要

```rust
// ── M2: ys-component::compact —— 纯数据 + 纯函数（零依赖，无 async）──
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CompactPolicy {
    pub trigger_ratio: f64,      // 0.85
    pub keep_ratio: f64,         // 0.10 —— 带内调参点（2026-09-19 定）
    pub keep_min_ratio: f64,     // 0.01 —— 保留带下界（硬边界）
    pub keep_max_ratio: f64,     // 0.10 —— 保留带上界（硬边界）
    pub output_reserve: usize,   // 4096 —— 触发点下限钳制用（评审 R4）
    pub summary_system_prompt: String, // 结构化摘要 prompt（见「摘要内容策略」）
}
impl CompactPolicy {
    pub fn trigger_at(&self, w: usize) -> usize;   // min(w × trigger_ratio, w − output_reserve − 1024)
    pub fn keep_budget(&self, w: usize) -> usize;  // clamp(w × keep_ratio, w×min, w×max)
}
impl Default for CompactPolicy { /* 上表默认值 */ }

// ── ys-model：为摘要调用加 `tool_choice`（评审 R1，需 ADR-0017 补记）──
pub struct ModelRequest {
    // …既有字段…
    #[serde(default)]
    pub tool_choice: Option<String>,   // 摘要调用传 Some("none")，抑制工具调用
}
```

// RunLimits 加一个字段（#[non_exhaustive] + pub 字段 ⇒ 跨 crate 只能
// `let mut l = RunLimits::default(); l.compaction = …`，不能字面量构造）
pub struct RunLimits { pub max_rounds, pub bash_timeout, pub context_window, pub compaction: CompactPolicy }

// ── M1: ys-loop::compact —— 算法本体，具体，不建 trait ──
pub struct CompactPlan { pub summarize: Range<usize>, pub keep: Range<usize> }

/// 纯函数、同步、无 IO —— 变异测试的主靶
pub fn plan(messages: &[Message], policy: &CompactPolicy, window: usize) -> Option<CompactPlan>;

#[non_exhaustive]
pub struct CompactOutcome {
    pub messages: Vec<Message>,   // 新消息表：摘要（独立 User 或并入 kept[0]）+ 保留段
    pub tokens_before: usize, pub tokens_after: usize,
    pub summarized: usize, pub kept: usize,
}
#[derive(Debug)]
pub enum CompactError { NoModel, Summarize(String), Storage(String) }

/// 自动触发与 /compact **共用的唯一实现**
pub async fn execute(
    messages: &[Message], plan: &CompactPlan,
    model: &dyn Model, policy: &CompactPolicy,
) -> Result<CompactOutcome, CompactError>;

// ── M4: ys-event —— 只带平铺字段（ys-event 不可依赖 ys-component，成环）──
AgentEvent::ContextCompacted {
    tokens_before: usize, tokens_after: usize, summarized: usize, kept: usize,
},
AgentEvent::ContextCompactionFailed { reason: String },

// ── M5: ys-session —— 带默认实现，零破坏 ──
#[async_trait]
pub trait Session: Send {
    // messages / append / clear 不变
    /// 整体替换。默认实现 = clear + 逐条 append；JsonlSession 覆写为「赋值一次 + flush_to_file 一次」。
    async fn replace_all(&mut self, messages: Vec<Message>) -> Result<(), SessionError> { … }
}

// ── M6: app 侧 ──
impl Config {
    pub fn load_user_config(&mut self, path: &Path);   // 不存在/畸形 → 默认 + logging::log
    pub fn context_window(&self) -> usize;             // 三级解析
    pub fn compact_policy(&self) -> CompactPolicy;
}
impl Agent { pub fn set_context_window(&mut self, window: usize); }  // 与既有 context_window() 对称
```

### 接线落点（文件级）

| 文件 | 落点 | 改动 |
|---|---|---|
| `crates/ys-component/src/compact.rs` | **新增** | `CompactPolicy` + `trigger_at`/`keep_budget` |
| `crates/ys-component/src/limits.rs:4-30` | `RunLimits` | +`pub compaction: CompactPolicy` |
| `crates/ys-loop/src/compact.rs` | **新增** | 由 `basic.rs:401-500` **平移改造**（算法不变，签名与 `RuntimeContext` 解耦）+ B-1/B-2/B-3 + 单测 |
| `crates/ys-loop/src/basic.rs` | 16 / 130-137 / 401-500 | 删 `COMPACT_KEEP_TOKENS` 与两个函数；Step 4b 改调 M1 + emit |
| `crates/ys-event/src/event.rs` | ~34 | +2 变体（**非终局事件**：`is_terminal` 不匹配它们 → `-p` 输出逐字节不变） |
| `crates/ys-session/src/lib.rs:14-27` | trait | +`replace_all` 默认实现 |
| `crates/ys-session/src/jsonl.rs` | `impl Session` | 覆写 `replace_all` → 赋值 + 单次 `flush_to_file()`（复用既有 tmp+rename 原子路径） |
| `crates/ys-runtime/src/agent.rs:131` | `context_window()` 旁 | +`set_context_window` |
| **`crates/ys-core/src/usage.rs`** | `Usage` | **+`cache_read_tokens: u32`**（默认 0，`Add` 一并累加）—— K-2 |
| **`adapters/model-openai-compatible/src/lib.rs:245-250`** | usage 尾包解析 | **额外读 `prompt_cache_hit_tokens`（DeepSeek）与 `prompt_tokens_details.cached_tokens`（OpenAI）** —— K-2 |
| **`crates/ys-model/src/request.rs:7-16`** | `ModelRequest` | **+`tool_choice: Option<String>`**（`#[serde(default)]`）—— 评审 R1，需 ADR 补记 |
| **`adapters/model-openai-compatible/src/lib.rs:400-420`** | 请求序列化 | `tool_choice` 直通（API 顶层参数，**不进 prompt 前缀、不影响缓存**）—— 评审 R1 |
| **`crates/ys-loop/src/compact.rs`** | 摘要请求构造 | **复用活对话前缀**：不改 `system`、不改 `tools`，摘要指令作**最后一条 user 消息** + **`tool_choice: Some("none")`** —— K-1 / R1 |
| `apps/coding-agent/src/provider.rs:9-14, 69-88` | `ProviderInfo` / `new()` | +`context_window: Option<usize>`；deepseek/minimax 填值，`custom` = `None`（见风险 4） |
| `apps/coding-agent/src/config.rs` | 字段 / 新方法 | `context_window`/`compaction` 覆盖 + `load_user_config` + `context_window()` |
| `apps/coding-agent/src/main.rs` | `build_agent`(258-269) + 3 个调用点 | +`limits` 形参 → `.limits(limits)`；`main` 里紧随 `load_auth` 后加载 config.json |
| `apps/coding-agent/src/app_loop.rs` | `Prompt` 臂开头(100-102) | 一行 `agent.set_context_window(config.context_window())` |
| 同上 | `Request::Compact` 臂(173-175) | 传 window + policy |
| `apps/coding-agent/src/capabilities.rs` | `compact`(194-200) + 测试 | 签名改 `(wiring, policy, window)`；调 M1；**无 model 时明确提示，绝不静默退回 `clear_session`** |
| `crates/ys-tui-coding/src/app.rs` | `apply_event`(88-150) | +2 臂 → `TranscriptLine::System`（失败用 `Error`）。`transcript.rs`/`draw.rs` 零改动 |
| `docs/adr/0017-*.md` | **新增** | 决策记录（见下） |

> `/status` 已经显示 `context_window`（`view.rs:77` ← `agent.context_window()`）⇒ **用户零成本验证配置吃进去了**，配置链路的可观测性是白送的。

### 测试计划（当前零测试 → 覆盖全部路径）

`compact.rs` 单测（`MemorySession` + `MockModel`，**不需要 `RuntimeContext`** —— 这正是签名解耦的收益）：

1. **比例数值钉死**：`window = 1_000_000` → `trigger_at = 850_000`、`keep_budget ∈ [10_000, 100_000]`；`window = 128_000` → `108_800 / [1_280, 12_800]`
1b. **带的不变量**：`keep_budget ≤ 0.10 × window < 0.85 × window = trigger_at` **恒成立** ⇒ D-1 退化不可能（变异：把 `keep_max_ratio` 改成 0.9 → 红）
2. `plan()` 早退 → `None`、**model 未被调用**（变异：把早退移到 model 调用之后 → `MockModel` 空队列 panic）
3. **B-1**：末条巨大 → `keep_from == len−1`，保留段含末条；首条巨大 → `keep_from == 1`，**产出真实进展**（变异：删 `min(len-1)` → 红）
4. **B-2**：`MockModel` 回 tool-call（无文本）→ `Err`、**session 未动**（变异：还原 `Ok("")` → 红）
5. **B-3**：`model.push_error` → `Err`、**session 逐字节未变**、不发 `ContextCompacted`（变异：改成写占位符 → 红）
6. 快乐路径：新消息表 = 摘要 + 保留段，`after < before`，摘要**恰好 1 条**
6b. **R7 交替不变量**：`kept[0]` 是 User 时摘要并入它、是 Assistant 时摘要独立成条 —— 两种情形都**不得出现连续同角色**，且**绝不产生非模型产出的 Assistant 消息**（变异：加回合成 ack → 红）
7. 切分边界精确（变异：累积循环 `>` 改 `>=` → 红）

`ys-session`：用**计数替身**断言 `replace_all == 1 && append == 0`（变异：JsonlSession 覆写退回默认实现 → 红）

`ys-loop` 集成（复用现成 `make_ctx`）：小窗口逼出压缩 → `CollectingSink` 里 `ContextCompacted` **排在 `RunFinished` 之前**，mock 两次响应顺序为「摘要 → 正文」

app 侧：`context_window()` 三级 + 畸形 JSON 不 panic；`capabilities::compact` 断言会话**变小且含摘要**（**而非空** —— 变异：退回 `clear_session` → 红）+ 无 model 时的明确提示

**KV cache（K-1/K-2）**：
- **K-1 前缀一致性**：mock 捕获摘要请求与上一轮主请求，断言 `system`、`tools` **逐字节相同**，且摘要请求 = 主请求消息序列 + 一条末尾 user 指令（变异：把 `system` 换回 summarizer 提示 → 红）
- **K-2 解析**：两种 usage 形状（DeepSeek `prompt_cache_hit_tokens` / OpenAI `prompt_tokens_details.cached_tokens`）各一条；缺字段时 `cache_read_tokens` 为 0 而非报错（变异：删掉解析 → 红）

**测试隔离**（`AGENTS.md` 硬约定）：临时目录名含**进程内原子序号 + `process::id()`**；进程级 env 改写走 `test_env::env_lock()` + `EnvRestore`

---

## 候选方案对比

### 方案 A — 最小复杂度（**采纳为骨架**）
1 个新文件（`ys-loop/compact.rs`），算法搬出 + 签名解耦，2 个新抽象，触及 4 个 crate。
**关键贡献**：① 用整数分数保证 128k 逐位等价；② **论证「陈旧 usage 抖动的守卫不需要」** —— pi 需要它是因为 append-only + 指针重建，而 YuShan 是**破坏性重建**（压完占用 ≈ keep 15.6% ≪ 触发 87.2%），结构上不可能抖动。省掉一个复杂守卫；③ 发现 `every_request_variant_is_handled` 的隐式耦合（R8）。

### 方案 B — 可扩展优先
3 个新文件，4 个新抽象，`CompactPolicy` 落在 `ys-component`。**关键贡献**：① `ratio + floor` 形式**同时**做到「128k 逐位等价」与「小窗口不退化」（比 A 的整数分数更稳健）；② 探明两条结构性约束（`ys-component` 零依赖放不下 async trait；`ys-event` 不能依赖 `ys-component` 否则成环）；③ 诚实裁决了哪些扩展点是假想未来（4 个里 1.5 个有真实需求）。

### 方案 C — 正确性/资源优先
3 个新文件，5 个新抽象，触及 **7 个 crate**。**关键贡献**：① `TokenLedger` 锚点式对账（真实 usage + 尾部估算）—— 后续最高价值项，见「已知风险」；② **发现 `keep_from == len` bug**（A/B 均未发现）；③ 发现「压缩失败 = 静默失忆」；④ `overflow` 无正则标记表（守住 ADR-0015 基线）；⑤ back-of-envelope 证明 1M 会话下摘要输入 900k → 200k。

### 对比矩阵

| 维度 | A 最小复杂度 | B 可扩展优先 | C 正确性/资源优先 |
|---|---|---|---|
| 新增文件 / 抽象 | 1 / 2 | 3 / 4 | 3 / 5 |
| 触及 crate | 4 | 5 | 7 |
| 1M 触发点修正 | ✅ | ✅ | ✅ |
| O(n²) 落盘修复 | ✅ | ❌ | ✅ |
| `keep_from == len` bug | ❌ | ❌ | ✅ |
| 空摘要静默丢历史 | ✅ | ❌ | ✅ |
| 压缩失败不再静默失忆 | ❌ | ❌ | ✅ |
| 估算器与真实 usage 对账 | ❌ | ❌ | ✅ |
| 溢出恢复 | ❌ | 留形状 | ✅ |
| 结构化摘要 / 增量合并 | ❌ | ❌ | ✅ |
| 代码量估算 | ~200 行 | ~300 行 | ~500 行 |
| 与 D3 冲突项 | 无 | 无 | **3**（估算器/摘要策略/Session API） |
| 风险 | 比例公式在小窗口退化（floor 已兜） | 抽象层数超出「最小核心」 | 面最广、行为变更最多 |

---

## 选择理由

**采纳 = A 的骨架 + C 的三项 bug 修复 + B 的 `CompactPolicy` 数据形状**。数值（触发 85% / 保留 1%~10%）与摘要内容策略由用户定稿。

- **骨架取 A**：`ys-loop/compact.rs` 单文件、算法平移、签名与 `RuntimeContext` 解耦。这一刀**被需求强制**（`/compact` 必须与阈值路径共用算法，而 capability 拿不到 `RuntimeContext`），不是为未来预埋 —— 符合本项目「最小核心」与 ADR-0014。
- **策略形状取 B**：`CompactPolicy` 是零依赖纯数据，加旋钮不改签名。**注意**：B 原案的 `ratio + floor` 默认值是为了「128k 逐位等价」，那条护栏**已被放弃**（见「比例」一节），本设计只借用它的**数据结构形状**，不沿用它的数值。
- **从 C 取 3 项**：B-1/B-2/B-3 是**修 bug，不是加功能** —— 它们都不改变算法结构（D3 仍然成立）。C 的论证有说服力：「把 800k token 的历史换成一行占位符不是降级，是静默失忆」。
- **从 A 取一条洞见**：**「陈旧 usage 抖动的守卫不需要」** —— pi 需要它是因为 append-only + 指针重建，而 YuShan 是**破坏性重建**（压完占用 ≤ keep ≤ 10% ≪ 触发 85%），结构上不可能抖动。省掉一个复杂守卫。**代价是：将来若改 append-only 存储，这条守卫必须补回来。**
- **事件取 2 个而非 B 的 3 个**：**不发 `Started`**。A 的 R1 指出：`Started` 会引入**悬挂状态** —— 压缩中途被 `Abort` 时终止事件永不发出，transcript 会永远挂着「Compacting…」。而 B-3 让失败成为**一等结局**（session 不动 + 明确失败），所以 `Compacted` / `Failed` 两个终局就够，不需要开始态。

**明确不取 C 的三项**（D3 延后，且都有独立价值，见下）：

| 项 | 为什么延后 |
|---|---|
| `TokenLedger` 锚点对账 | 最高价值的**第二刀**（见风险 1）。它要动 `AgentPorts` + `RuntimeContext` + 新增账本类型，跨 crate 面大；且依赖 provider 可靠回报 `usage`（minimax 恒回报 0 → 该 provider 用户拿不到收益） |
| `overflow` 溢出恢复 | 本轮无消费者：窗口可配之后溢出更罕见。**留形状**（`ModelError` 上加一个表驱动的 `is_context_overflow()`，3 行接上），不现在写 |
| 结构化摘要 / 增量合并 | 是**摘要 prompt 的内容**，D3 明确延后。留口子：`CompactPolicy` 加 `summary_system_prompt` 字段即可随时换 |

**代价的诚实表达**：选择 A 骨架意味着**接受估算器不准** —— 1M 窗口下触发点仍会有系统性偏移（估算器**不计 system prompt 与工具表**，是低估）。缓解：① 窗口可配之后，固定的低估量（几 k～十几 k）在 1M 下占比极小；② 溢出留了恢复形状。**但这个缺口是真的，且会随 D1（skills 描述常驻 ~9k）变大** —— 见风险 1。

---

## 评审处置（`arch-validate`，2026-09-19）

完整报告见 [`review.md`](./review.md)。12 条发现全部处置完毕：

| # | 发现 | 处置 |
|---|---|---|
| **R1** | K-1 缺 `tool_choice` 前提 ⇒ 摘要可能被当工具调用 | ✅ **`ModelRequest` 加 `tool_choice`**，摘要调用传 `Some("none")`；需 ADR 补记 |
| **R2** | `summary_max_tokens` 是死旋钮（被 app 的 `config.max_tokens` 覆盖） | ✅ **删字段**；不设 `max_tokens`（照 `summarize.rs:37-38` 既有原则） |
| **R3** | `plan`/`execute`/`replace_all` 的公开形状不一致 | ✅ **`execute` 不持有 session**（约束定死）；两条路径各自 `replace_all`；措辞统一 |
| **R4** | 触发点未减输出预留 ⇒ 小窗口溢出 | ✅ `trigger_at = min(w × 0.85, w − output_reserve − 1024)` |
| **R5** | 窗口粒度按 provider 不组合 | ✅ **解析键从 `provider` 提到 `model`**（修订 D5） |
| **R6** | 压缩期无超时、无中断 | ✅ **本轮只加超时**（超时按 B-3 处理：session 不动）；中断显式延后 |
| **R7** | 摘要以 `Role::User` 写回；伪造 assistant 消息被持久化 | ✅ **删掉合成 ack**，交替**由构造保证**（见下） |
| **R8** | `--json` 会新增事件行未标注 | ✅ 文档补注 |
| **R9** | 事件 token 数是估算值却命名成权威值 | ✅ 改名 `estimated_tokens_before/after` |
| **R10** | `plan`/`execute` 各收一次 `messages` ⇒ 索引错配 | ✅ `CompactPlan` 带 `len` + `debug_assert_eq!` |
| **R11** | 默认 `replace_all` 非原子 | ✅ doc comment 写明「默认实现非原子，要求原子性须覆写」 |
| **R12** | 文档残留旧数字/旧术语 | ✅ 已清理 |

**R7 处置：删掉合成 ack，交替由构造保证**

今日的 `[summary(User), ack(Assistant), …kept]` 里的 `I have the context summary. Continuing.` 是**伪造的模型输出**，会被写进 JSONL、重启后当作模型真实发言重放、`/export` 带出去、下次压缩还被当真实历史再摘要一遍。

**它原本要解决的「user/assistant 交替」也根本没解决** —— 只是把失败位置挪了：

| | `kept[0]` 是 User | `kept[0]` 是 Assistant |
|---|---|---|
| 无 ack | `User, User` ❌ | `User, Assistant` ✅ |
| **有 ack** | `User, A, User` ✅ | `User, A, Assistant` ❌ **连续两条** |

**改用构造保证**（零伪造数据）：

```
若 kept[0] 是 User  → 摘要文本并入 kept[0]（作为前缀），不再单插一条消息
否则                → 摘要作为独立 User 消息插在最前（其后是 Assistant，交替自然成立）
```

- **证据**：pi（`COMPACTION_SUMMARY_PREFIX` + summary，只产出一条 user 消息）与 Codewhale（一条 user 角色的摘要 checkpoint）**重建后都是 `[summary(user), …kept]`，都不加合成 assistant 消息** —— 三家实现里只有 YuShan 加。
- 这条在我们**本来就要搬的那个函数**里，是**减代码**不是加代码。
- ⚠️ 仍保留 `Role::User`（改角色需要新决策；且交替已由构造保证）。

**D11 已被推翻**（见「摘要内容策略」）：摘要改为**采用 pi / Codewhale 的结构化骨架 + 显式保留精确路径/命令/错误文本**。「结构化摘要」因此从 D3 的延后项**进入第一刀**。

## 已知风险与待验证项

- [ ] **风险 1（最高）：估算器未对账 + 不计 system prompt/工具表 = 系统性低估。** 128k 下低估十几 k 就可能撞 provider 上限；**D1 落地后 system prompt 会再涨 ~9k**，风险放大。缓解：窗口可配（本轮）+ 触发点减输出预留（R4）+ 溢出留形状。**建议 `TokenLedger` 作为紧随其后的第二刀**，且在 D1 之前完成
- [ ] **风险 2：`/compact` 的静默降级。** 现状 `/compact` = `clear_session`。新路径若在 `model == None` 或 `execute` 失败时**退回 clear**，就是静默数据丢失。必须写成硬断言 + 变异测试，失败时**什么都不做并明说**
- [ ] **风险 3：`keep_ratio = 0.10` 是否合适。** 已定为带上沿（理由见「比例」一节），但**没有实测**。按 D8 用第一刀的事件（`estimated_tokens_before/after`）收数据后再调；`keep_min_ratio`/`keep_max_ratio` 的硬边界保证它永远落在 1%~10% 内。**注意：保留段是每轮重付的，50% 窗口比 1% 贵 1.66x（见 KV 表）**
- [ ] **风险 7：K-1 的修复依赖「上一轮主调用的缓存仍热」。** 摘要在 Step 4b 执行，上一轮的主调用已过去一个轮的时间 —— 若超出 provider 的缓存 TTL（DeepSeek 数小时、OpenAI 5~10 分钟），仍会 miss。缓解：**这是净赚项，miss 时退化为今天的行为**，无下行风险
- [ ] **风险 8：K-1 要求 tools 与 system 与主调用逐字节一致。** 少传 tools、或改 `max_tokens` 之外的任何前缀元素都会破坏命中。**须有测试钉住「摘要请求的前缀 == 主调用前缀」**（可用 mock 捕获两次 `ModelRequest` 并比较前 N 条消息与 system/tools）
- [ ] **待验证：K-2 的字段形状。** DeepSeek 用 `prompt_cache_hit_tokens`，OpenAI 用 `prompt_tokens_details.cached_tokens` —— 需实测确认两家实际返回什么（`supports_stream_usage` 为 false 的 provider 连 usage 都没有，此时缓存统计恒为 0）
- [ ] **风险 4：内置 `context_window` 数字会过时。** deepseek/minimax 的值是今天的事实。填错的方向是「过早压缩」（浪费一次摘要调用）或「溢出」（靠风险 1 的恢复形状兜底）。这正是必须保留 `config.json` 覆盖入口的原因
- [ ] **风险 5：一处有意行为变更**（B-3）。需在 PR 描述与发布说明里明说
- [ ] **风险 6：`keep` 比例化的语义**。工作集大小是**任务的属性、不是模型的属性**。这两个默认值是从 128k **反推**的近似，不是测量值 —— 注释与 ADR 都要写明
- [ ] **待验证：`Agent::set_context_window` 的 ADR-0007 合规性。** 已写入 ADR-0017；实现时须逐条对齐 `docs/design.md` 的依赖方向不变量
- [ ] **待验证：每回合刷新 window 的耦合。** app_loop 现在依赖 `Config` 的解析结果；窗口错了是**静默的**（不报错，只是压缩时机不对）。缓解：单一调用点 + `/status` 已显示生效值
- [ ] **建议 `prototype` 先行**：用假 `usage` 喂一轮，验证「触发点 = 窗口 × 85%」「保留落在 1%~10% 带内」「`trigger > keep` 恒成立」三条断言 —— 这是整个改造的护栏

## 实施分期（2026-09-19 定：拆成两片 + 第二刀）

本设计覆盖的范围已比「加固」大一圈（含**核心 crate 公开 API 变更**与**结构化摘要**）。按可独立验证性拆：

### 片 1 — 配置 + 可见性 + 正确性（**先做**）

| 内容 | 对应决策 |
|---|---|
| 窗口三级配置链（**按 `model` 解析**）+ `~/.yushan/config.json` | D5 / D12 |
| 比例：触发 `min(w×0.85, w−输出预留−1024)`、保留 `clamp(w×0.10, 1%, 10%)` | D6 / D10 / D15 |
| 每回合刷新 + `Agent::set_context_window` | D5 |
| 算法搬家 `plan`/`execute`（与 `RuntimeContext` 解耦） | ADR 决策 1 |
| `Session::replace_all`（默认实现 + `JsonlSession` 原子覆写） | ADR 决策 5 |
| 两个事件（`ContextCompacted` / `ContextCompactionFailed`） | ADR 决策 6 |
| **三个 bug 修复** B-1 / B-2 / B-3 | ADR 决策 7 |
| **删合成 ack**，交替由构造保证 | R7 |
| `/compact` 接真压缩 | 范围 A5 |

**验收标准：用户立刻可感知的三件事** ——① 1M 窗口生效（`/status` 正确）；② 压缩可见（transcript 有事件）；③ 不再静默丢历史（失败时 session 不动）。

### 片 2 — 成本 + 摘要质量（**片 1 之后**）

| 内容 | 对应决策 |
|---|---|
| K-1 摘要复用活对话前缀 + **`tool_choice`**（含 `ys-model` / adapter 改动） | ADR 决策 10 / D14 |
| K-2 `Usage` 加 `cache_read_tokens` + adapter 解析两种形状 | ADR 决策 10 |
| **结构化摘要 prompt**（pi/Codewhale 骨架 + 精确路径/命令/错误文本） | D13 |
| 压缩超时 | D16 |

**为什么拆**：`tool_choice` 动的是 `ys-model` 的**公开 API**，值得单独过一轮评审 + ADR 补记；而片 2 的收益依赖片 1 的事件数据。

### 第二刀 — `TokenLedger` 锚点对账

**必须在 D1（skills）之前完成** —— skills 注入 57 条描述会让 system prompt 再涨 ~9k，放大估算器的系统性低估（见风险 1）。

## 相关文档

- [`context.md`](./context.md) — 上下文、约束、决策 D1–D14
- [`review.md`](./review.md) — `arch-validate` 质量分析（12 条发现的完整报告）
- [`adr/0017-compaction-hardening.md`](../../adr/0017-compaction-hardening.md) — 决策记录
- `docs/extension-points.md` — 压缩归 Compaction 桶（回合内，已有）
- `docs/adr/0014-follow-up-without-hook-system.md` — 为什么不建 hook
- `docs/adr/0016-web-tools-design.md` — 「差异只是数据 → 用表不用 trait」的判据
