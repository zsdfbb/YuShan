# 0017 — 压缩加固：算法搬家与 `RuntimeContext` 解耦、阈值比例化、窗口三级配置、失败不再静默失忆

给已存在的自动压缩做加固 —— ① 把算法从 `BasicLoop` **搬到 `ys-loop::compact`** 并让 `plan()`/`execute()` 分离、**与 `RuntimeContext` 解耦**（这是 `/compact` 能复用同一份实现的**前提**）；② 阈值与保留量**参数化**（触发 = `min(窗口 × 85%, 窗口 − 输出预留)`，保留 = 窗口的 1%~10%，带内取 10%）；③ `context_window` 建立**三级配置链（按 `model` 解析）**；④ 压缩失败**不再把历史截断成一行占位符**；⑤ 摘要调用复用活对话前缀以命中 KV cache（+ `tool_choice` 抑制工具调用）；⑥ 摘要改为**结构化骨架 + 显式保留精确路径**；⑦ **删掉伪造的 assistant ack 消息**；⑧ 顺手修三个真 bug。**算法结构（切片 + 一次摘要调用）不重写。**

## Status

accepted（2026-09-19）· **修订（2026-09-19）**：并入 `arch-validate` 的 12 条发现处置（新增决策 12「删 ack」、13「实施分期」；修订决策 2/3/8/10）。完整报告见 `docs/arch/skills-and-compaction/review.md`。

## 背景

- 上游：`docs/arch/skills-and-compaction/context.md`（约束 + 决策 D1–D6）、`design-compaction.md`（融合方案 + 三候选对比）、`docs/arch/skills-and-compaction/` 的 pi / Codewhale 调研。
- **压缩早已实现且在跑**（`crates/ys-loop/src/basic.rs:130-137` Step 4b）—— 本 ADR 不引入新机制，只加固既有的。`docs/extension-points.md:51` 已经把「自动压缩上下文」归入 **Compaction（回合内，已有）**。
- **驱动本 ADR 的两个事实**：
  1. `context_window` **恒为 `128_000`**：`apps/coding-agent/src/main.rs::build_agent`（`:257-269`）**从不调 `.limits()`**，`ApiModel`（`provider.rs:26-27`）只解析 `id`，且无任何 env/文件入口。**用户跑 1M 上下文的模型，实际在 ≈111,616 token 就触发压缩**，其余 ~89 万 token 从未被使用；`/status` 还显示错的窗口值。
  2. **压缩零测试、零事件**：全仓 `compact_session`/`generate_summary` 只出现在 `basic.rs` 自身；压缩不发任何 `AgentEvent` → transcript 完全看不见，静默丢历史。
- 关键约束（决定了模块边界，不是审美）：
  - **`ys-component` 零外部依赖**（`Cargo.toml` 实测只有 5 个 `ys-*`，无 `async-trait`）→ 它**放不下** async 策略 trait。
  - **依赖环**：`ys-component` 已被 `ys-event` 依赖（`RuntimeContext.events: &mut dyn EventSink`）⇒ `ys-event` **不能**反向依赖 `ys-component` ⇒ 新事件**只能带平铺字段**。
  - `Session` trait（`ys-session/src/lib.rs:14-27`）只有 `messages`/`append`/`clear`，**无重写 API**；`JsonlSession` 每次 `append` **全量重写文件**（`jsonl.rs:50-84`）。
  - **ADR-0007**：给 `Agent` 加新公开 API 必须先写 ADR（本 ADR 即为其覆盖）。
  - **ADR-0015 的依赖基线**：`regex` 是硬否决项。
- 同类实现（pi / Codewhale 的完整调研见 `design-compaction.md`）：**pi 用的正是 `window − reserve(16384)` / `keep(20000)`** —— 与 YuShan 现状一字不差，即现状是从 pi 抄来的；两家都做了 YuShan 没做的对账与溢出恢复。

## 决策

**1. 算法搬到 `ys-loop::compact`，`plan()` / `execute()` 分离**

`plan(messages, policy, window) -> Option<CompactPlan>` 是**纯函数、同步、无 IO**；`execute(...)` 是**唯一 async 点**（一次 model 调用）。

- **为什么必须搬**：`capabilities::compact`（app 侧）拿不到 `RuntimeContext`，只有把算法与容器解耦，`/compact` 才可能复用同一份实现。**这是需求强制的缝，不是为未来预埋。**
- **为什么 `plan()` 必须单独存在且早于 model 调用**：① 「要不要压、压多少」在**发事件之前**就已知，事件不会撒谎；② 一个**承重的既有耦合** —— `app_loop.rs:770-822` 的 `every_request_variant_is_handled` 会走到这里，它安全**只因为**会话只有 2 条小消息 → `plan()` 返回 `None` → 不调用 model（否则 `MockModel::remove(0)` 空队列 panic）。早退必须留在 model 调用**之前**，实现时须在 `compact.rs` 注释里写明。

**2. 阈值与保留量：触发 = min(窗口 × 85%, 窗口 − 输出预留), 保留 = clamp(默认, 窗口 × 1%, 窗口 × 10%)**

```
触发   = min(window × 0.85, window − output_reserve − CONTEXT_HEADROOM)   // output_reserve = 4096
保留   = clamp(window × 0.10, window × 0.01, window × 0.10)               // 1%~10% 硬带，带内取 10%
```

| 窗口 | 触发 | 保留（1%~10%） |
|---|---|---|
| 1M | 850_000（85% 胜出） | **100_000** |
| 200k | 170_000（85% 胜出） | **20_000** |
| 8k | 3_072（**下限钳制胜出**） | 800 |

- **「1~10%」建成一个带（band）而非单点**：`keep_min_ratio` / `keep_max_ratio` 是**结构上的硬边界**，`keep_ratio = 0.10` 是带内的调参点。**带内取上沿的三条理由**：① 带内成本差仅约 10%（1M 下 1% vs 10% 合计 430K vs 475K eq/轮），对比 50% 那档的 1.66x 可忽略；② 三家收敛点就是 ~10%（20k ÷ 200k）；③ **先保真后调低比先有损后调高安全** —— 从高往低调，调过头有事件数据兜底。
- **触发点 85% 的依据**：pi / Codewhale（80%）/ Claude Code（~83.5%）三家落在 80%~92%，85% 居中。
- **保留量 1%~10% 的依据**：三家独立收敛到「约 10% 窗口」（都用固定 20k）；且 KV cache 量化显示**保留 50% 窗口比保留 1% 贵 1.66x，两项成本都偏向小保留**。两条独立证据同向。
- **减输出预留（评审 R4）**：窗口还要容纳模型输出（app 装配 `max_tokens = 4096`，`main.rs:610`）。1M 下 85% 更小、无影响；但**窗口现在可配** —— 8K 下 `6800 + 4096 > 8192` ⇒ 溢出，而溢出恢复已延后 ⇒ 硬失败。Codewhale 同形状（`input_budget_ceiling = window − output_cap − 1024`）。
- **与三家的唯一分歧是缩放方式**：三家用「固定 20k」（小窗口占比高、大窗口占比低），本设计**按窗口比例**（大窗口保留更多，符合 1M 的意图）。**这是有意偏离。**
- **D-1 由构造消除**：`trigger ≤ 0.85w`、`keep ≤ 0.10w` ⇒ `trigger > keep` 恒成立 ⇒ `keep_from` 永不为 0 ⇒ 不再有「阈值已超但保留预算更大」的永久空转（该 bug 在验证阶段实测复现过）。
- ⚠️ **「128k 下与今日逐位等价」这条迁移护栏正式放弃** —— 旧值 `111616 / 20000` 是从 pi 抄来的、**无测量依据**；128k 下新值变为 `108800 / ~12800`。**这是有意的行为变更，须写进 PR 描述。**

**摘要内容策略（评审 D13，推翻本节初稿）**：采用 **Codex → pi → Codewhale 一脉相承的形状**。
- **结构化骨架**：`## Goal` / `## Constraints & Preferences` / `## Progress (Done / In Progress / Blocked)` / `## Key Decisions` / `## Next Steps` / `## Critical Context`（pi 的固定节）
- **显式保留**：**精确文件路径、命令、错误文本**（Codewhale `COMPACT_PROMPT` 原文 *"exact file paths, commands, and error text"*）
- **追加清单**：`<read-files>` / `<modified-files>`（pi）
- **明确排除**：meta 叙述 —— Codewhale 要求 *"Summarize the task, not the checkpoint machinery"*
- ⚠️ **初稿曾定「只描述最主要的、引用类可回找」，现已推翻**：**「改过哪些文件」是重复劳动 / 覆盖文件的唯一直接入口**，重读 token 成本远高于留几个路径；三家独立收敛是强证据。
- **后果**：**「结构化摘要」从「明确不做」进入第一刀**（见决策 8 的修订）。落法：`CompactPolicy.summary_system_prompt` 为**覆盖口子**，默认值为上述结构化 prompt 常量。
- **仍延后**：pi 的 `UPDATE_SUMMARIZATION_PROMPT` 增量合并（另一件事）。

**3. `context_window` 三级配置链（**按 `model` 解析**，评审 R5 修订）**

```
config.json 的 context_window（按 model id）  >  ProviderInfo.context_window（按 model id）  >  128_000（默认）
```

- 新增 `~/.yushan/config.json`（**用户写、app 只读**）。**绝不能放进 `state.json`** —— 那是 `StateStore::save()` 会**覆盖**的文件（`state.rs:44-64`）。
- 解析失败/畸形 → **落日志文件**（照 `state.rs:38-42` 的 `logging::log`）+ 回退，**绝不阻断启动**。
- 需要路径覆盖钩子给测试（照 `StateStore::set_override` / `ProviderRegistry::set_auth_override` 的现成模式）。
- ⚠️ **解析键是 `model`，不是 `provider`（R5 修订）**：初稿定为「单值全局 + 按 provider 内置表」，但 **`ProviderInfo` 每 provider 只有一个 `default_model`**（`provider.rs:9-15, 73, 79`），而 DeepSeek 至少两个模型（`deepseek-chat` / `deepseek-reasoner`，见测试夹具 `provider.rs:260-268`）⇒ 同 provider 内 `/model` 切换时窗口**不会变**；且全局单值会**跨 provider 误用**（设了 1M 后 `/login` 到 128k 的 provider ⇒ 溢出或过早压缩）。**窗口是模型的属性，不是 provider 的。**

**4. 每回合刷新窗口（`Agent::set_context_window`）**

`/login`、`/model` 会在运行中换 provider（`capabilities.rs:103/168`），窗口差可达 16 倍。只在 `build_agent` 解析的话，用户 `/login` 到 1M 模型后**仍按 128k 压缩** —— 这正是要修的 bug 的另一半。

选**在 `app_loop` 的 `Prompt` 臂开头刷新**（而非在 `Login`/`SetModel` 各加一处）：**单一调用点，结构上不可能漏**，代价是每回合一次纯内存查表（文件只在启动读一次）。

> ADR-0007：这是新增的 `Agent` 公开 API（与既有只读 `context_window()` 对称），已由本 ADR 覆盖。选最小面（只加 window 一个字段的 setter），而非通用的 `set_limits`。

**5. `Session::replace_all`（带默认实现）**

```rust
async fn replace_all(&mut self, messages: Vec<Message>) -> Result<(), SessionError> {
    self.clear().await?;
    for m in messages { self.append(m).await?; }   // 默认实现
    Ok(())
}
```

`JsonlSession` 覆写为「赋值一次 + 单次 `flush_to_file()`」（复用既有 tmp + rename 原子路径）。

- **这是快路径，不是抽象**：现序列 `clear()` + N+2 次 `append`，而 `JsonlSession::append` 每次**全量重写文件** ⇒ 压缩（恰好发生在会话最大时）是 **O(n²)**，且磁盘上会短暂出现「空会话」，**中途崩溃即丢历史**。
- 带默认实现 ⇒ 对既有实现**零破坏**；测试用计数替身钉住 `replace_all == 1 && append == 0`。

**6. 两个事件，不发 `Started`**

```rust
AgentEvent::ContextCompacted      { tokens_before, tokens_after, summarized, kept }
AgentEvent::ContextCompactionFailed { reason }
```

- **不发开始事件**：`Started` 会引入**悬挂状态** —— 压缩中途被 `Abort`（`basic.rs:148-180` 承认的那个窗口）时终止事件永不发出，transcript 会永远挂着「Compacting…」。而决策 7 让失败成为**一等结局**，两个终局就够。
- **只带平铺字段**：`ys-event` 不能依赖 `ys-component`（成环），故不能带策略结构体。
- 两条都是**非终局事件**（`is_terminal` 不匹配）⇒ `-p` / `--json` 输出逐字节不变。
- **手动路径不发事件**：`app_loop` 的 `Request::Compact` 臂**不在** `select!` 循环里（`app_loop.rs:119-126`），emit 的事件会躺在 `sink_rx` 里直到下一个 `Prompt` 才被转发 → transcript 顺序错乱。手动路径走 `Outbound::Output`，但**底层是同一份 `CompactionOutcome`**。

**7. 三个 bug 修复（修 bug，不改算法结构）**

| # | Bug | 修复 |
|---|---|---|
| **B-1** | 当**最新一条消息自己**超过保留预算时，尾扫永不 break → 保留段为空 → **整个会话连同本轮用户消息被摘要掉，一条不留** | 硬不变量：**摘要段与保留段都必须非空**（`1 ≤ keep_from ≤ len−1`；`len < 2` 返回 `None`） |
| **B-2** | `generate_summary` 在响应无 `Text` 块（如模型回 tool-call）时返回 `Ok("")` → 写出**空摘要**，历史被静默丢掉 | 空/全空白 → 视为**失败** |
| **B-3** | 压缩失败时 `clear()` + 写入一行 `[Compression failed — proceeding with truncated history]` —— 把 800k token 的历史换成一行占位符 | 失败 → **session 一个字节都不动** + 发 `ContextCompactionFailed` + 本轮照常继续 |

> **B-1 由候选方案 C 发现，A/B 两个方案均未发现。** B-3 的判据是：「把 800k token 的历史换成一行占位符**不是降级，是静默失忆**」，且此后每个回合都带着这个谎继续跑。

**8. 明确不做**（本轮范围外，防范围蔓延）

| 项 | 为什么不做 |
|---|---|
| `TokenLedger`（真实 usage 锚点对账） | **最高价值的第二刀**。它要动 `AgentPorts` + `RuntimeContext` + 新类型，跨 crate 面大；且依赖 provider 可靠回报 `usage`（minimax `supports_stream_usage = false` 恒回报 0 → 该 provider 拿不到收益）。**但风险随 skills 落地放大，应在 D1 之前完成** |
| `overflow` 溢出恢复 | 本轮无消费者（窗口可配后溢出更罕见）。**留形状**（`ModelError` 上加表驱动的 `is_context_overflow()`，3 行接上），**不现在写 30 条 provider 正则**（且 `regex` 是硬否决项） |
| ~~结构化摘要~~ | **已移出「不做」** —— 评审 D13 修订：采用 pi / Codewhale 的结构化骨架，**进入第一刀**（见决策 2 末段） |
| `UPDATE` 增量合并（pi） | 与结构化摘要不同：它处理的是**重复压缩时把上一条摘要合并进新的**，需要追踪 `previousSummary` 与回退起点。**另一件事，仍延后** |
| `Compactor` / `TriggerSource` trait、压缩 hook | ADR-0014：不设计假想未来需求。4 个扩展点里只有 1.5 个有真实内容，其余是假想。**升级路径**见决策 11 |
| 删或改 `Session::clear` / 常态 append 的 O(n) 全文件写 | 需要 append-only 日志语义的独立设计（追加写 + 显式 fsync/崩溃恢复），与本 ADR 正交。`replace_all` 正是它的**前置接缝** |
| ~~`keep` 比例的上限钳制~~ | **已做** —— 决策 2 的 1%~10% 带本身就带 `keep_max_ratio` 上界 |
| ~~同一 provider 下的 per-model 窗口~~ | **已做** —— 评审 R5 把解析键提到 `model`（决策 3） |
| 压缩期**中断**（`-p`/`--json` 接 Ctrl+C） | 评审 R6：本轮只加**超时**；中断要在 app 层引入进程级信号管线，成本高于本轮收益。**显式记为已知限制**（B-3 已保证中止不丢数据） |
| ~~摘要消息的**角色形态**（`Role::User` + 合成 ack）~~ | **已做（删 ack）** —— 见决策 12。**但 `Role::User` 保留**（改角色需要新决策） |

**12. 删掉合成 assistant ack，user/assistant 交替由构造保证**

今日重建出的 `[summary(User), ack(Assistant), …kept]` 里的 `I have the context summary. Continuing.` 是**伪造的模型输出**。它会被：
- 写进 JSONL 会话文件，**重启后当作模型真实发言重放**
- `/export` 带出去
- 下次压缩时**当作真实历史再摘要一遍**

**而且它要解决的「交替」根本没解决** —— 只是把失败位置挪了：

| | `kept[0]` 是 User | `kept[0]` 是 Assistant |
|---|---|---|
| 无 ack | `User, User` ❌ | `User, Assistant` ✅ |
| **有 ack** | `User, A, User` ✅ | `User, A, Assistant` ❌ |

**改用构造保证**：`kept[0]` 是 User ⇒ 摘要**并入**它；否则摘要作为独立 User 消息插在最前（其后是 Assistant，交替自然成立）。

- **证据**：pi（`COMPACTION_SUMMARY_PREFIX` + summary，只产出一条 user 消息）与 Codewhale（一条 user 角色的摘要 checkpoint）**重建后都是 `[summary(user), …kept]`，都不加合成 assistant 消息** —— 三家实现里只有 YuShan 加。
- 这条在**本来就要搬的那个函数**里，是**减代码**不是加代码。

**13. 实施分期**（拆两片 + 第二刀）

- **片 1（先做）：配置 + 可见性 + 正确性** —— 窗口三级配置（按 model）、比例、每回合刷新、算法搬家、`Session::replace_all`、两个事件、B-1/B-2/B-3、删 ack、`/compact` 接真压缩。
  **验收：用户立刻可感知的三件事** —— ① 1M 生效；② 压缩可见；③ 不再静默丢历史。
- **片 2：成本 + 摘要质量** —— K-1 + `tool_choice`、K-2 `Usage` cache 字段、结构化摘要、压缩超时。
  **理由**：`tool_choice` 动的是 `ys-model` **公开 API**，值得单独过评审 + ADR 补记；且片 2 依赖片 1 的事件数据。
- **第二刀：`TokenLedger`** —— 必须在 D1（skills）之前完成。

**10. KV cache：摘要调用复用活对话前缀；`Usage` 追踪缓存命中**

- **摘要调用改成「活对话 + 一条末尾 user 指令」**：不改 `system`、不改 `tools`，摘要指令作为**最后一条 `Role::User` 消息**追加。
  - **理由**：今天的 `generate_summary`（`basic.rs:469-500`）构造**全新 `ModelRequest`**（`system` 换成 summarizer 提示、`tools: vec![]`）⇒ 序列化后第一个 token 就不同 ⇒ **整段上下文 100% 缓存未命中**，而这是系统里**最大的单次输入**（1M/触发 850K 下 350K–840K token 全价）。
  - **证据**：Codewhale 的源码有明确注释 —— *"The summarization request IS the live conversation plus one final user message asking for the handoff summary, so the provider's prefix cache covers everything already sent this session."*（`tmp/Codewhale/crates/tui/src/compaction.rs:1495-1500`）。**pi 与本项目现状都做反了。**
  - ⚠️ **前提：必须同时传 `tool_choice: "none"`（评审 R1 修订）** —— 复用 `tools` 会让模型**有可能回 tool call 而不是文本** → `generate_summary` 拿到无 `Text` 块 → 空摘要 → 触发 B-2。Codewhale 在同一处正是这么做的。**`ModelRequest` 当前没有 `tool_choice` 字段，必须新增（本 ADR 即为其依据）。**
  - ⚠️ **初稿称此举「零下行风险」，已被评审推翻**：没有 `tool_choice` 时它会把一个缓存问题（慢）变成一个正确性问题（坏）。加上 `tool_choice` 之后才成立。
  - **缓存未命中时**（超出 provider TTL）退化为今天的行为，无额外损失。
- **`Usage` 加 `cache_read_tokens`**，adapter 解析 DeepSeek 的 `prompt_cache_hit_tokens` 与 OpenAI 的 `prompt_tokens_details.cached_tokens`。
  - **理由**：现状只读 `prompt_tokens`/`completion_tokens` ⇒ **对 KV cache 完全盲**，无法验证上面那条是否生效、也无法据此调 keep。**它是所有 cache 优化的前提。**
- **量化（详见 `design-compaction.md`）**：主项是「每轮背着大上下文」，命中把成本从全价降到 ×0.25 —— **4 倍杠杆**，比调参重要得多。摘要那条是零风险净赚，但只改约 4% 的量级，**不应夸大**。
- **顺带**：`工具面必须恒定`（ADR-0016）**本来就是缓存保住规则** —— system prompt 与 tools 是前缀最前两块，抖动即全失效。D1（skills 注入）须遵守同一规则。

**11. 升级路径（写下来防止后人重走）**

真有第二种压缩算法时，把 `plan`/`execute` 的分派点收拢成 **`enum CompactStrategy { Summary, Prune, … }`**（照 ADR-0016「差异只是数据 → 用表不用 trait」的判据），`basic.rs` 的调用点**一行不改**。若届时确需运行时装实现，再升级为 trait —— 那时它是**加法**。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 最小复杂度** | 1 新文件，算法搬出，整数分数比例，2 抽象，触及 4 crate | 最小改动；论证「陈旧 usage 抖动的守卫**不需要**」（破坏性重建结构上不可能抖动，省掉 pi 的复杂守卫）；发现 `every_request_variant_is_handled` 隐式耦合 | 整数分数在小窗口退化；未发现 B-1 |
| **B 可扩展优先** | 3 新文件，`CompactPolicy` 落 `ys-component`，3 事件 | `ratio + floor` 同时满足「128k 等价」与「小窗口不退化」；探明两条结构性约束；诚实裁决了假想扩展点 | 抽象层数超出「最小核心」 |
| **C 正确性/资源优先** | 3 新文件，5 抽象，触及 **7 crate** | 锚点式对账；**发现 B-1**；无正则溢出表；back-of-envelope 证明 1M 下摘要输入 900k → 200k | 面最广；3 项与已定决策 D3 冲突（估算器/摘要策略/Session API） |

**采纳 = A 骨架 + B 的策略数据形状 + C 的三项 bug 修复**。理由：骨架取 A（最小、且被 `/compact` 复用强制）；策略形状取 B（`ratio + floor` 兼顾迁移安全与小窗口稳健）；事件取 2 个而非 B 的 3 个（避免悬挂状态）；从 C 只取**不改变算法结构**的三项 bug 修复（D3 仍成立）。

## Consequences

- **正面**：1M 用户的实际可用上下文从 ~111k 提升到 850k；压缩从隐形副作用变成**有事件、有测试、可被 `/compact` 复用**的显式操作；压缩一次落盘从 **N+1 次全文件写 → 1 次**；三个静默丢历史的 bug 修掉；**摘要调用从「必然 100% 缓存未命中」变为可命中**，且 `Usage` 开始暴露缓存命中率（在此之前是盲的）。
- **负面**：`ys-loop` / `ys-component` / `ys-event` / `ys-session` / `ys-runtime` / app 六个位置各有一处改动；估算器仍不准（**接受**，见下）；比例默认值是反推近似而非测量值。
- **行为变更（须写进 PR 描述与发布说明）**：决策 7 的 B-3 —— 压缩失败时从「带失忆地成功」（截断历史）变为「明确失败」（session 不动、照常继续）。少数场景下用户会从「静默成功」变为「看到一次明确失败」。
- **最大遗留风险**：**估算器未与真实 usage 对账**，且它**不计 system prompt 与工具表**（系统性低估）。窗口可配之后固定低估量（几 k～十几 k）在 1M 下占比极小，但 **skills（D1）落地会再涨 ~9k，风险放大** ⇒ `TokenLedger` 应作为紧随其后的第二刀，**且在 D1 之前完成**。
- **未验证**：`trigger > keep` 恒成立（D-1 退化不可能）、保留落在 1%~10% 带内、摘要调用确实命中缓存 —— 建议先用 `prototype` 跑通前两条（纯算术），K-1 的命中需真实 provider 才能验证。
- **行为变更清单**（须进 PR 描述）：① 128k 下触发点 `111616 → 108800`、保留 `20000 → ~12800`（放弃「逐位等价」护栏）；② 压缩失败不再截断历史；③ **摘要从「一句话 prompt」换成结构化骨架 + 显式保留精确路径/命令/错误文本**（对齐 pi / Codewhale）；④ **删掉合成的 assistant ack 消息**（决策 12）—— 会话文件里不再出现非模型产出的 `I have the context summary. Continuing.`。
- **评审修订（`arch-validate` 2026-09-19）**：本 ADR 已并入 12 条发现的处置 —— 新增 `tool_choice`（决策 10 / R1）、删 `summary_max_tokens`（R2）、触发点减输出预留（决策 2 / R4）、窗口解析键提到 `model`（决策 3 / R5）、压缩期加超时（决策 8 / R6）、摘要改为结构化（决策 2 末段 / D13）；R7/R9–R12 见 `docs/arch/skills-and-compaction/review.md`。

## 关联

- [`docs/arch/skills-and-compaction/context.md`](../arch/skills-and-compaction/context.md) — 上游上下文与决策 D1–D6
- [`docs/arch/skills-and-compaction/design-compaction.md`](../arch/skills-and-compaction/design-compaction.md) — 融合方案 + 三候选对比 + 测试计划
- [ADR-0007](./0007-agent-public-api.md) — 新增 `Agent::set_context_window` 的依据
- [ADR-0010](./0010-actor-model-agent-api-revision.md) — `Agent` 持「执行能力」（含 limits）
- [ADR-0014](./0014-follow-up-without-hook-system.md) — 不建 hook、不为假想需求设计
- [ADR-0015](./0015-search-tools-via-external-binaries.md) — 依赖基线（`regex` 硬否决）
- [ADR-0016](./0016-web-tools-design.md) — 「差异只是数据 → 用表不用 trait」
- `docs/extension-points.md` — 压缩归 Compaction 桶
