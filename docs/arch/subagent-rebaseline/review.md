# Subagent 落地复核 — 架构质量分析报告

> 产出：`/arch-validate`（2026-10-09）。
> 对象：[`design.md`](design.md)（落地复核）与 [ADR-0029](../../adr/0029-subagent-landing-decisions.md)（proposed）。
> 基线：工作区 `master`（`911fd72`）；证据带 `文件:行号`。
> 维度：全量（可行性 / 可维护性含基础软件视角 / 可理解性 / 性能与可靠性）。

---

## 分析范围

- **对象**：本轮复核产出的设计方案（不是代码 —— 本需求零落地）。
- **参照**：[`context.md`](context.md)（基线重估）、[`../subagent/design.md`](../subagent/design.md) 与 [ADR-0022](../../adr/0022-subagent-dispatch.md)（权威形态）。
- **不在范围**：ADR-0022 已定死的形态决策本身（窄 trait / 子事件不进父流 / 子表裁剪 / 结果契约 / 工具名）。

**结论预览**：方案整体**健康**（全加法式、依赖方向不破、机制均有仓内先例），但有**一个 P0 缺口**和**一批 ADR-0022 决策在复核中被丢掉**。

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|---|---|---|
| 1.1 技术可实现性 | 🟢 | 全部机制有仓内先例：窄能力注入照 `Summarizer`（`crates/ys-tool/src/summarizer.rs`）、超时/watchdog 照 `app_loop` 的父预算（`apps/coding-agent/src/app_loop.rs:131-133`）、并行 `buffered` 是标准库能。无「黑科技」。 |
| 1.2 依赖成熟度 | 🟢 | 唯一新增直接依赖 `futures-util` **已在 `Cargo.lock:345`（0.3.34）** ⇒ 无新下载、无新供应链面。 |
| 1.3 实现周期 | 🟢 | 刀 0/1/2 不依赖 subagent，可独立交付并单独验证；刀序依赖关系清晰。 |
| 2.1 模块边界 | 🟢 | 改动落 `ys-tool`（trait 默认方法）/`ys-component`（常量+字段）/`ys-runtime`（字段）/`ys-core`（`Usage` 字段），真活集中 app 侧 `subagent.rs`。**无循环依赖**。 |
| 2.2 接口稳定性 | 🟢 | 全部为 `#[non_exhaustive]` 结构加字段 + trait 默认方法 + `#[serde(default)]` ⇒ 非破坏变更。 |
| 2.3 错误处理 | 🟡 | 子「跑完但没跑好」走 `Ok` + 非 `Completed`（好）；但 ADR-0022 的 **fail-closed（`model` 名解析失败立即 `Err`、不静默回退）未在复核中复述**（见 R3）。 |
| 2.4 并发安全 | 🟡 | `buffered` 保序、单任务内并发、无 `'static` 要求 —— 选型正确；`ctx.events` 不可跨任务共享的处理（先串行 emit `ToolCall`、后保序 emit `ToolResult`）正确。但 watchdog 的 `tokio::spawn` 在 drop 路径上会**孤儿化**（见 R5）。 |
| 2.5 资源管理 | 🟡 | 产物目录 `~/.yushan/subagents/` 无清理策略、无测试覆盖口（见 R2）；R5 的孤儿任务（默认最长 48h）是泄漏路径。 |
| 3.1 概念一致性 | 🟢 | 不引新概念类别（`SubagentRunner` 是 `Summarizer` 同缝第 3 实例）；`StopReason` 复用不新造词汇。 |
| 3.2 上手成本 | 🟢 | 文档 412 行、结构清晰、`文件:行号` 齐全。仅一处过程噪声（R8）。 |
| 4.1 性能模型 | 🟡 | 成本数字**主动作废**并给出「先装读数」的诚实路径 —— 方向对，但本轮**无任何数字**，缓存/R1 的量化要到读数就位后才能完成。 |
| 4.2 故障模式 | 🟡 | 子失败不拖垮父（好）；但**失控成本**这条 ADR-0022 亲手点名的失败模式**防线被撤**（见 R1）。 |
| 4.3 可观测性 | 🟡 | 子 agent 内部在**任何输出通道都不可见**（`--json` 亦然）。这是**有意偏离**（用户答②），已记入 §9.1 —— 属**已接受**的取舍，非缺陷；但它是「排障难 + 成本不聚合」的共同根源，须持续持有。 |

---

## 风险排序

| 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|---|---|---|---|
| **R1 每回合派发总量上限缺失** | 高 | 中高 | **P0** | 补回 ADR-0022 决策 15（见下） |
| **R2 产物目录无清理 + 无测试覆盖口** | 中 | 高 | P1 | 补 sweep + `YUSHAN_SUBAGENTS_DIR` |
| **R3 fail-closed 未复述** | 中 | 中 | P1 | 在 design.md/ADR-0029 显式写入 |
| **R4 每次派发新建 model 实例（连接池/TLS 重复）** | 中 | 高 | P1 | 按名缓存，且缓存须持 `Arc<dyn Model>` |
| **R5 外闸 drop ⇒ 孤儿 watchdog 任务** | 中 | 中 | P1 | `AbortOnDropHandle` 或 `select!` |
| **R6 ChildBoundary 偏离 ADR-0022 决策 5 未记为修正** | 低 | 高 | P2 | §7 修正表补第 4 条 |
| **R7 测试矩阵未更新** | 低 | 中 | P2 | `docs/design.md` §12 加条目 |
| **R8 design.md 含过程噪声** | 低 | 高 | P2 | 删「agent thread limit reached」一段 |

### R1 详述（唯一的 P0）

ADR-0022 决策 15 专门加了「**每回合派发总量上限**（建议 16）」，理由写得很重：上界 = 父 10 轮 × 每轮 4 个 = 40 次/回合 ≈ 200 万 token，正是 Codex #9748（*"instantly drains entire Pro plan"*）那类事故的形状。

本轮复核里这条**不见了**（`design.md` 与 ADR-0029 均无「派发总量 / 决策 15」字样），而**两个前提同时变松了**：
- `Config::max_rounds()` 默认已是 **`u32::MAX`**（不设，`apps/coding-agent/src/config.rs:230`）；
- 按本轮裁定 5，**子也取同一份 `Config`** ⇒ 子的 `max_rounds` 同为不设。

⇒ 「子必有界」被裁定 5 废弃（复核已如实记账），而「**父侧派发总量**」这条独立防线又被漏掉 ⇒ 只剩 48h wall-clock 一根刹车。**两条防线同时消失**，而它们防护的正是同类软件被骂得最惨的失败模式。

**建议**：把决策 15 补回，且点明它在本轮更关键。实现零机制 —— runner 本就按回合构造（`app_loop` 现建），计数器做成 runner 字段（`AtomicU32`）即天然 per-turn；超限返回**响亮的 `Err`** ⇒ `is_error` 结果回喂父模型。

---

## 改进建议

### 易修复（低风险快速改进）

- **R5**：watchdog 用 `AbortOnDropHandle`（`tokio_util`）或改成 `tokio::select!` 与子 `run_turn` 同 future 竞速 —— 消除外闸 drop 时的孤儿任务。若用 `select!`，需保留「先 push `Abort`、让子在轮边界优雅收尾」的语义（不是直接 drop 子 future）。
- **R6**：§7 修正表加第 4 行 —— `ChildBoundary.take()` 从「恒 `None`」改为「取自己的、不碰父」。**顺带记一句**：功能上 `is_aborted()` 探针（`crates/ys-loop/src/basic.rs:95,205`）已足以让子收尾，`own` 队列是为压缩窗口防御路径（`basic.rs:213-236`）保留的一致形状。
- **R8**：删除 `design.md` §2 里「本轮 subagent 并发槽不可用（`agent thread limit reached`）」—— 那是过程信息，对读者无意义。
- **R7**：`docs/design.md` §12 的 `BasicLoop` 行补「并行批闸门与保序」。

### 需讨论（需要团队决策）—— ✅ 2026-10-09 已定：以下三项**均本轮做**（见文末「处置记录」）

- **R4**：ADR-0022 决策 17 的连带只落地了 `context_window_for`，「**按模型名缓存实例**」与「**build_model_for**」没进复核。且这里有一个借用形状要定：`SubagentRunner::run` 需要 `&dyn Model`，而缓存若用 `Mutex<HashMap<String, Box<dyn Model>>>` 拿不出稳定引用 ⇒ 缓存应持 **`Arc<dyn Model>`**（克隆 `Arc`、`&*arc` 传入）。**要不要本轮做**？（不做 ⇒ 每次带 `model` 参数的派发新建连接池 + TLS 握手。）
- **R3**：fail-closed 要不要写进「刀 4 验收」？（`model` 名解析失败 ⇒ 立即 `Err`；未知 provider 大声拒绝。）
- **4.1**：成本模型本轮是否接受「先装读数、数字留空」？若否，需要先做刀 2（成本读数）并跑一次真实派发回填。

### 架构级（影响面大，需要跨 phase 规划）

- **R1**：如上 —— 建议本轮就补，但它触及「谁拥有派发记账」（runner 的 per-turn 字段 vs 更上层的预算类型）。复核已用 ADR-0014 否掉了「预算一等类型」，故**推荐 runner 字段**这一最轻形态。
- **R2**：产物目录的保留策略需要一次**独立**的「`~/.yushan/` 各目录保留策略统一」评审 —— 因为 `sessions/` 的「永不清理」是**既存缺口**（ADR-0022 决策 4 已点明「那是既存缺口，不是该复制到新目录的先例」）。**本轮只做 `subagents/`，不动 `sessions/`。**

---

## 总体评价

**整体健康度：良好，可进入实现 —— 前提是先补 R1。**

方案的三个优点值得肯定：① **全加法式**，依赖方向与编译期边界一条不破；② **唯一的既有语义变更**（Step 9 并行）被 `is_concurrency_safe()` 默认 `false` 完全闸住，且给了 6 条含变异自检的验收；③ **诚实** —— 把「子必有界被废弃」「成本数字作废」「子不可观测」三条不利结论都写进了文档，没有粉饰。

**最大风险点是 R1**：复核在收紧「子预算」叙事的同时，**遗漏了 ADR-0022 唯一一条针对成本爆炸的总量闸**，而 `max_rounds` 此刻又恰好从 10 放宽到 `u32::MAX`。这是典型的「单看每条决策都对、合起来少了一层防线」。补它只需 runner 上一个 `AtomicU32`。

**推荐下一步**：先补 R1（设计层面，改动极小）→ 把 ADR-0029 转 accepted → 按刀序开工（刀 0/1/2 不依赖 subagent，可先并行推进）。

---

## 处置记录（2026-10-09，作者补充）

用户裁定「补充」后，本报告的 P0/P1/P2 项已并入 [`design.md`](design.md) 与 [ADR-0029](../../adr/0029-subagent-landing-decisions.md)：

| 项 | 处置 | 去处 |
|---|---|---|
| **R1** 每回合派发总量上限 | ✅ 补回（P0） | design.md §10.1、ADR-0029 §5① |
| **R2** 产物目录 sweep + 测试覆盖口 | ✅ 补回 | design.md §10.2、ADR-0029 §5② |
| **R3** fail-closed | ✅ 补回 | design.md §10.3、ADR-0029 §5③ |
| **R4** 模型实例缓存（`Arc<dyn Model>`） | ✅ 补回 | design.md §10.4、ADR-0029 §5④ |
| **R5** 孤儿 watchdog | ✅ 改为 `AbortOnDropHandle` | design.md §5.1、ADR-0029 Consequences |
| **R6** `ChildBoundary` 偏离未记 | ✅ 记为「修正 4」 | design.md §7、ADR-0029 §6 |
| **R7** 测试矩阵未更新 | ✅ 写入 | design.md §10.5、刀 4 验收列 |
| **R8** 过程噪声 | ✅ 删除 | design.md §2 |

**用户 2026-10-09 裁定：上面「需讨论」的三项全部「本轮做」** ——
R4 模型实例缓存纳入本轮（刀 2）、R3 fail-closed 进刀 4 验收、成本数字**本轮内**由真实派发回填（刀 7）。
据此 [ADR-0029](../../adr/0029-subagent-landing-decisions.md) 已由 proposed 转 **accepted**。

---

## 参考

- 被分析对象：[`design.md`](design.md)、[ADR-0029](../../adr/0029-subagent-landing-decisions.md)
- 基线：[`context.md`](context.md)
- 权威形态与原始决策：[ADR-0022](../../adr/0022-subagent-dispatch.md)、[`../subagent/design.md`](../subagent/design.md)
- 新增交互面：[ADR-0027](../../adr/0027-completion-check-and-round-budget.md)、[ADR-0028](../../adr/0028-model-call-retry-and-timeout.md)
- 测试矩阵口径：[`docs/design.md` §12](../../design.md)
