# 0017 — 压缩加固：算法搬家与 `RuntimeContext` 解耦、阈值比例化、窗口三级配置、失败不再静默失忆

给已存在的自动压缩做加固 —— ① 把算法从 `BasicLoop` **搬到 `ys-loop::compact`** 并让 `plan()`/`execute()` 分离、**与 `RuntimeContext` 解耦**（这是 `/compact` 能复用同一份实现的**前提**）；② 触发阈值与保留量**比例化**（`ratio + floor`，默认值在 128k 下逐位等价）；③ `context_window` 建立**三级配置链**（`config.json` > 内置 provider 表 > 默认 128k）；④ 压缩失败**不再把历史截断成一行占位符**；⑤ 顺手修两个真 bug。**算法结构（切片 + 一次摘要调用）不重写。**

## Status

accepted（2026-09-19）

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

**2. 阈值与保留量比例化：`max(window × ratio, floor)`**

```
触发 = window − max(window × reserve_ratio, reserve_floor)   // 0.125 / 16_384
保留 =        max(window × keep_ratio,    keep_floor)        // 0.15625 / 20_000
```

**默认值在 128k 下与今日逐位等价**（`max(16000, 16384)=16384`；`0.15625×128000 = 20000` 精确）→ 迁移零行为变化，风险被压到「只影响非 128k 窗口」。1M 下自然变成「875k 触发、156k 保留」。

- 选 `ratio + floor` 而非整数分数（候选 A 的形式）：后者在小窗口会退化，A 自己在风险清单里承认了。`floor` 是护栏，不是调参旋钮。
- **`keep` 本轮只设 floor、不设 ceiling**：keep 段是**每个后续回合都要重付**的输入 token（1M 下 156k/轮），但工作集大小是**任务的属性、不是模型的属性**。先用本轮新加的事件（`before/after_tokens`）收集真实数据再调。
- **诚实标注**：这两个数是从 128k 的 `16384`/`20000` **反推**的近似，**不是测量值** —— 注释与文档都要写明。

**3. `context_window` 三级配置链（单值全局）**

```
config.json 的 context_window（全局）  >  ProviderInfo.context_window（内置表，按 provider）  >  128_000（默认）
```

- 新增 `~/.yushan/config.json`（**用户写、app 只读**）。**绝不能放进 `state.json`** —— 那是 `StateStore::save()` 会**覆盖**的文件（`state.rs:44-64`）。
- 解析失败/畸形 → **落日志文件**（照 `state.rs:38-42` 的 `logging::log`）+ 回退，**绝不阻断启动**。
- 需要路径覆盖钩子给测试（照 `StateStore::set_override` / `ProviderRegistry::set_auth_override` 的现成模式）。
- **粒度是单值全局**：同一 provider 下的多模型若窗口不同，本轮不处理（已知欠账）。

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
| 结构化摘要 / `UPDATE` 增量合并 | 是**摘要 prompt 的内容**，被 D3 明确延后。留口子：`CompactPolicy` 加 `summary_system_prompt` 字段即可随时换 |
| `Compactor` / `TriggerSource` trait、压缩 hook | ADR-0014：不设计假想未来需求。4 个扩展点里只有 1.5 个有真实内容，其余是假想。**升级路径**见决策 9 |
| 删或改 `Session::clear` / 常态 append 的 O(n) 全文件写 | 需要 append-only 日志语义的独立设计（追加写 + 显式 fsync/崩溃恢复），与本 ADR 正交。`replace_all` 正是它的**前置接缝** |
| `keep` 比例的上限钳制 | 见决策 2：先用事件收数据 |
| 同一 provider 下的 per-model 窗口 | 已知欠账，`config.json` 是全局单值 |

**9. 升级路径（写下来防止后人重走）**

真有第二种压缩算法时，把 `plan`/`execute` 的分派点收拢成 **`enum CompactStrategy { Summary, Prune, … }`**（照 ADR-0016「差异只是数据 → 用表不用 trait」的判据），`basic.rs` 的调用点**一行不改**。若届时确需运行时装实现，再升级为 trait —— 那时它是**加法**。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 最小复杂度** | 1 新文件，算法搬出，整数分数比例，2 抽象，触及 4 crate | 最小改动；论证「陈旧 usage 抖动的守卫**不需要**」（破坏性重建结构上不可能抖动，省掉 pi 的复杂守卫）；发现 `every_request_variant_is_handled` 隐式耦合 | 整数分数在小窗口退化；未发现 B-1 |
| **B 可扩展优先** | 3 新文件，`CompactPolicy` 落 `ys-component`，3 事件 | `ratio + floor` 同时满足「128k 等价」与「小窗口不退化」；探明两条结构性约束；诚实裁决了假想扩展点 | 抽象层数超出「最小核心」 |
| **C 正确性/资源优先** | 3 新文件，5 抽象，触及 **7 crate** | 锚点式对账；**发现 B-1**；无正则溢出表；back-of-envelope 证明 1M 下摘要输入 900k → 200k | 面最广；3 项与已定决策 D3 冲突（估算器/摘要策略/Session API） |

**采纳 = A 骨架 + B 的策略数据形状 + C 的三项 bug 修复**。理由：骨架取 A（最小、且被 `/compact` 复用强制）；策略形状取 B（`ratio + floor` 兼顾迁移安全与小窗口稳健）；事件取 2 个而非 B 的 3 个（避免悬挂状态）；从 C 只取**不改变算法结构**的三项 bug 修复（D3 仍成立）。

## Consequences

- **正面**：1M 用户的实际可用上下文从 ~111k 提升到 ~875k；压缩从隐形副作用变成**有事件、有测试、可被 `/compact` 复用**的显式操作；压缩一次落盘从 **N+1 次全文件写 → 1 次**；两个静默丢历史的 bug 修掉。
- **负面**：`ys-loop` / `ys-component` / `ys-event` / `ys-session` / `ys-runtime` / app 六个位置各有一处改动；估算器仍不准（**接受**，见下）；比例默认值是反推近似而非测量值。
- **行为变更（须写进 PR 描述与发布说明）**：决策 7 的 B-3 —— 压缩失败时从「带失忆地成功」（截断历史）变为「明确失败」（session 不动、照常继续）。少数场景下用户会从「静默成功」变为「看到一次明确失败」。
- **最大遗留风险**：**估算器未与真实 usage 对账**，且它**不计 system prompt 与工具表**（系统性低估）。窗口可配之后固定低估量（几 k～十几 k）在 1M 下占比极小，但 **skills（D1）落地会再涨 ~9k，风险放大** ⇒ `TokenLedger` 应作为紧随其后的第二刀，**且在 D1 之前完成**。
- **未验证**：128k 下逐位等价、1M 下触发点后移 —— 建议先用 `prototype` 拿假 `usage` 跑通这两条断言，再进入实现。

## 关联

- [`docs/arch/skills-and-compaction/context.md`](../arch/skills-and-compaction/context.md) — 上游上下文与决策 D1–D6
- [`docs/arch/skills-and-compaction/design-compaction.md`](../arch/skills-and-compaction/design-compaction.md) — 融合方案 + 三候选对比 + 测试计划
- [ADR-0007](./0007-agent-public-api.md) — 新增 `Agent::set_context_window` 的依据
- [ADR-0010](./0010-actor-model-agent-api-revision.md) — `Agent` 持「执行能力」（含 limits）
- [ADR-0014](./0014-follow-up-without-hook-system.md) — 不建 hook、不为假想需求设计
- [ADR-0015](./0015-search-tools-via-external-binaries.md) — 依赖基线（`regex` 硬否决）
- [ADR-0016](./0016-web-tools-design.md) — 「差异只是数据 → 用表不用 trait」
- `docs/extension-points.md` — 压缩归 Compaction 桶
