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
       ports.model == None → ["Cannot compact: no model configured. Use /login."]
       plan(...) → None → ["Nothing to compact."]
       execute(...) → wiring 已由 replace_all 更新
       → ["Compacted: 112k → 21k tokens (48 messages summarized, 6 kept)."]
```

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
  "context_window": 1000000,
  "compaction": { "reserve_ratio": 0.125, "keep_ratio": 0.15625 }
}
```

### 比例默认值 —— **128k 窗口下与今日逐位等价**（迁移护栏）

| 量 | 今日常量 | 新公式 | @128k | @1M |
|---|---|---|---|---|
| 触发余量 | `16384`（`basic.rs:134` 字面量） | `max(win × reserve_ratio, reserve_floor)`，`0.125` / floor `16_384` | `max(16000, 16384)` = **16384** ✅ | `125_000` |
| 尾保留 | `20_000`（`COMPACT_KEEP_TOKENS`） | `max(win × keep_ratio, keep_floor)`，`0.15625` / floor `20_000` | `max(20000, 20000)` = **20000** ✅ | `156_250` |

> 按 **D8**：keep **只设 floor、不设 ceiling**，先用第一刀新加的事件（`before/after_tokens`）收集真实数据再调。
> 这两个数是从 128k 的 `16384`/`20000` **反推**的近似，**不是测量值** —— 注释须写明。

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
    pub reserve_ratio: f64,    // 0.125
    pub reserve_floor: usize,  // 16_384
    pub keep_ratio:    f64,    // 0.15625
    pub keep_floor:    usize,  // 20_000
}
impl CompactPolicy {
    pub fn trigger_at(&self, window: usize) -> usize;   // window - max(window*ratio, floor)
    pub fn keep_budget(&self, window: usize) -> usize;  //     max(window*ratio, floor)
}
impl Default for CompactPolicy { /* 上表默认值 */ }

// RunLimits 加一个字段（#[non_exhaustive] + pub 字段 ⇒ 跨 crate 只能
// `let mut l = RunLimits::default(); l.compaction = …`，不能字面量构造）
pub struct RunLimits { pub max_rounds, pub bash_timeout, pub context_window, pub compaction: CompactPolicy }

// ── M1: ys-loop::compact —— 算法本体，具体，不建 trait ──
pub struct CompactPlan { pub summarize: Range<usize>, pub keep: Range<usize> }

/// 纯函数、同步、无 IO —— 变异测试的主靶
pub fn plan(messages: &[Message], policy: &CompactPolicy, window: usize) -> Option<CompactPlan>;

#[non_exhaustive]
pub struct CompactOutcome {
    pub messages: Vec<Message>,   // 新消息表：summary + 合成 assistant + 保留段
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

1. **比例数值钉死**：`window = 128_000` → `trigger_at = 111_616`、`keep_budget = 20_000`（锁「默认窗口行为不变」）
2. `plan()` 早退 → `None`、**model 未被调用**（变异：把早退移到 model 调用之后 → `MockModel` 空队列 panic）
3. **B-1**：末条巨大 → `keep_from == len−1`，保留段含末条；首条巨大 → `keep_from == 1`，**产出真实进展**（变异：删 `min(len-1)` → 红）
4. **B-2**：`MockModel` 回 tool-call（无文本）→ `Err`、**session 未动**（变异：还原 `Ok("")` → 红）
5. **B-3**：`model.push_error` → `Err`、**session 逐字节未变**、不发 `ContextCompacted`（变异：改成写占位符 → 红）
6. 快乐路径：新消息表 = `[summary(User), ack(Assistant), 保留…]`，`after < before`，摘要**恰好 1 条**
7. 切分边界精确（变异：累积循环 `>` 改 `>=` → 红）

`ys-session`：用**计数替身**断言 `replace_all == 1 && append == 0`（变异：JsonlSession 覆写退回默认实现 → 红）

`ys-loop` 集成（复用现成 `make_ctx`）：小窗口逼出压缩 → `CollectingSink` 里 `ContextCompacted` **排在 `RunFinished` 之前**，mock 两次响应顺序为「摘要 → 正文」

app 侧：`context_window()` 三级 + 畸形 JSON 不 panic；`capabilities::compact` 断言会话变成 3 条（summary/ack/kept）**而非空**（变异：退回 `clear_session` → 红）+ 无 model 时的明确提示

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

**推荐 = A 的骨架 + B 的策略数据形状 + C 的三项 bug 修复。**

- **骨架取 A**：`ys-loop/compact.rs` 单文件、算法平移、签名与 `RuntimeContext` 解耦。这一刀**被需求强制**（`/compact` 必须与阈值路径共用算法，而 capability 拿不到 `RuntimeContext`），不是为未来预埋 —— 符合本项目「最小核心」与 ADR-0014。
- **策略形状取 B 的 `ratio + floor`**：它同时做到「128k 逐位等价」（迁移零风险）与「小窗口不退化」（A 的整数分数在小窗口会退化，A 自己在 R3 承认了）。代价只是 `ys-component` 里一个零依赖的纯数据结构。
- **事件取 2 个而非 B 的 3 个**：**不发 `Started`**。A 的 R1 指出：`Started` 会引入**悬挂状态** —— 压缩中途被 `Abort` 时终止事件永不发出，transcript 会永远挂着「Compacting…」。而 B-3 让失败成为**一等结局**（session 不动 + 明确失败），所以 `Compacted` / `Failed` 两个终局就够，不需要开始态。
- **从 C 取 3 项**：B-1/B-2/B-3 是**修 bug，不是加功能** —— 它们都不改变算法结构（D3 仍然成立）。C 的论证有说服力：「把 800k token 的历史换成一行占位符不是降级，是静默失忆」。

**明确不取 C 的三项**（D3 延后，且都有独立价值，见下）：

| 项 | 为什么延后 |
|---|---|
| `TokenLedger` 锚点对账 | 最高价值的**第二刀**（见风险 1）。它要动 `AgentPorts` + `RuntimeContext` + 新增账本类型，跨 crate 面大；且依赖 provider 可靠回报 `usage`（minimax 恒回报 0 → 该 provider 用户拿不到收益） |
| `overflow` 溢出恢复 | 本轮无消费者：窗口可配之后溢出更罕见。**留形状**（`ModelError` 上加一个表驱动的 `is_context_overflow()`，3 行接上），不现在写 |
| 结构化摘要 / 增量合并 | 是**摘要 prompt 的内容**，D3 明确延后。留口子：`CompactPolicy` 加 `summary_system_prompt` 字段即可随时换 |

**代价的诚实表达**：选择 A 骨架意味着**接受估算器不准** —— 1M 窗口下触发点仍会有系统性偏移（估算器**不计 system prompt 与工具表**，是低估）。缓解：① 窗口可配之后，固定的低估量（几 k～十几 k）在 1M 下占比极小；② 溢出留了恢复形状。**但这个缺口是真的，且会随 D1（skills 描述常驻 ~9k）变大** —— 见风险 1。

---

## 已知风险与待验证项

- [ ] **风险 1（最高）：估算器未对账 + 不计 system prompt/工具表 = 系统性低估。** 128k 下低估十几 k 就可能撞 provider 上限；**D1 落地后 system prompt 会再涨 ~9k**，风险放大。缓解：窗口可配（本轮）+ 溢出留形状。**建议 `TokenLedger` 作为紧随其后的第二刀**，且在 D1 之前完成
- [ ] **风险 2：`/compact` 的静默降级。** 现状 `/compact` = `clear_session`。新路径若在 `model == None` 或 `execute` 失败时**退回 clear**，就是静默数据丢失。必须写成硬断言 + 变异测试，失败时**什么都不做并明说**
- [ ] **风险 3：keep 段是每轮重付的 token。** 1M 下 `keep_ratio = 0.15625` → 保留 ~156k，**此后每轮白付 156k 输入**。按 D8 先只设 floor、用事件调 —— 但**必须在第二刀之前调掉**，否则 1M 的收益被 per-turn 成本吃掉
- [ ] **风险 4：内置 `context_window` 数字会过时。** deepseek/minimax 的值是今天的事实。填错的方向是「过早压缩」（浪费一次摘要调用）或「溢出」（靠风险 1 的恢复形状兜底）。这正是必须保留 `config.json` 覆盖入口的原因
- [ ] **风险 5：一处有意行为变更**（B-3）。需在 PR 描述与发布说明里明说
- [ ] **风险 6：`keep` 比例化的语义**。工作集大小是**任务的属性、不是模型的属性**。这两个默认值是从 128k **反推**的近似，不是测量值 —— 注释与 ADR 都要写明
- [ ] **待验证：`Agent::set_context_window` 的 ADR-0007 合规性。** 已写入 ADR-0017；实现时须逐条对齐 `docs/design.md` 的依赖方向不变量
- [ ] **待验证：每回合刷新 window 的耦合。** app_loop 现在依赖 `Config` 的解析结果；窗口错了是**静默的**（不报错，只是压缩时机不对）。缓解：单一调用点 + `/status` 已显示生效值
- [ ] **建议 `prototype` 先行**：用假 `usage` 喂一轮，验证「128k 下逐位等价」与「1M 下触发点后移」两条断言 —— 这是整个迁移的护栏

## 相关文档

- [`context.md`](./context.md) — 上下文、约束、决策 D1–D6
- [`adr/0017-compaction-hardening.md`](../../adr/0017-compaction-hardening.md) — 决策记录
- `docs/extension-points.md` — 压缩归 Compaction 桶（回合内，已有）
- `docs/adr/0014-follow-up-without-hook-system.md` — 为什么不建 hook
- `docs/adr/0016-web-tools-design.md` — 「差异只是数据 → 用表不用 trait」的判据
