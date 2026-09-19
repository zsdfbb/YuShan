# 自动压缩加固 — 架构质量分析报告

## 处置状态（2026-09-19 更新）

**12 条发现全部已处置**，设计与 ADR 已同步更新：

| 发现 | 处置 |
|---|---|
| **R1** `tool_choice` 缺失 | ✅ **加** `ModelRequest.tool_choice`，摘要调用传 `"none"`（新决策 D14，ADR 决策 10） |
| **R2** `summary_max_tokens` 死旋钮 | ✅ **删字段**（新决策 D17），照 `summarize.rs:37-38` 既有原则 |
| **R3** 模块公开形状不一致 | ✅ `execute` 不持有 session（约束定死）；两条路径各自 `replace_all`；措辞统一 |
| **R4** 触发点未减输出预留 | ✅ `trigger_at = min(w × 0.85, w − output_reserve − 1024)`（新决策 D15） |
| **R5** 窗口粒度按 provider | ✅ **解析键提到 `model`**（新决策 D12，修订 D5） |
| **R6** 压缩期无超时/中断 | ✅ **本轮只加超时**；中断显式延后（新决策 D16） |
| **R7** 摘要角色 + 伪造 ack | ✅ **删掉合成 ack**，交替**由构造保证**（新决策 D18）。**`Role::User` 保留**（改角色另议） |
| **R8** `--json` 契约未标注 | ✅ 文档补注 |
| **R9** 事件 token 命名 | ✅ 改名 `estimated_tokens_*` |
| **R10** `plan`/`execute` 索引错配 | ✅ `CompactPlan` 带 `len` + `debug_assert_eq!` |
| **R11** 默认 `replace_all` 非原子 | ✅ doc comment 写明 |
| **R12** 文档残留旧数字 | ✅ 已清理 |

**另有一条决策推翻**：原 **D11**（摘要「只留最主要的、引用类可回找」）被 **D13** 推翻 —— 改为采用 **pi / Codewhale 的结构化骨架 + 显式保留精确路径/命令/错误文本**。连带效果：**「结构化摘要」从延后项进入第一刀**。

---

## 分析范围

- **对象**：`docs/arch/skills-and-compaction/design-compaction.md` + `docs/adr/0017-compaction-hardening.md`
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **来源**：设计方案 + **对当前代码的实证核对**（每条发现都附 `文件:行号`）
- **方法**：逐条核实设计所依赖的代码事实；对既有代码做过一轮**可执行验证**（7 条断言，含 3 个 bug 实测复现）
- **未覆盖**：skills 部分（另一份设计）

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | 算法是**平移既有代码**（`basic.rs:401-500`），非新写。无新依赖、无黑科技。`plan`（纯函数）+ `execute`（async）的分法在本项目有先例（`truncate.rs` / `notice.rs` 都是「纯函数 + 调用方拼装」） |
| 1.2 依赖成熟度 | 🟢 | **零新增第三方依赖**（实测 `Cargo.lock` 无 YAML crate、app 层已有 `serde`/`serde_json`、`ys-component` 零外部依赖）。守住 ADR-0015/0016 基线 |
| 1.3 实现周期 | 🟢 | 增量路径清晰，接线点文件级列全（13 个文件）。测试计划覆盖 4 条路径 + 变异点 |
| 2.1 模块边界 | 🟢 | **由结构性约束决定的切法，不是审美**：`ys-component` 零依赖放不下 async trait、`ys-event` 不能依赖 `ys-component`（成环）⇒ 策略数据 / 算法 / 事件三者必须分居。这个推导是对的 |
| 2.2 接口稳定性 | 🟡 | `Session::replace_all` 带默认实现、`AgentEvent`/`RunLimits` 均 `#[non_exhaustive]` ⇒ 零破坏 ✅。但**默认实现非原子**（见 R11），且 `Agent::set_context_window` 是新公开 API（ADR-0007 已覆盖 ✅） |
| 2.3 错误处理 | 🟡 | **B-3 是本设计最大的质量改善** —— 压缩失败从「静默失忆」变为「session 一个字节不动 + 明确失败」，错误处理从「破坏性」翻转为「无损」。但压缩调用**无超时**（见 R6） |
| 2.4 并发安全 | 🟢 | 压缩全程在 `run_turn` 内单线程；`Agent` 不共享；**无新增锁** |
| 2.5 资源管理 | 🟢 | 峰值内存 ≈ 2× session（旧 + 新 `Vec`），1M 上下文下 ~8MB，可接受。无泄露路径（全部 ownership 转移） |
| 3.1 概念一致性 | 🟡 | 术语对齐 `CONTEXT.md`，整体好。但**文档内残留施工痕迹**（见 R12） |
| 3.2 抽象层次 | 🟢 | 纯数据（`CompactPolicy`）/ 纯函数（`plan`）/ 副作用（`execute`）三层清晰 |
| 3.3 文档 | 🟢 | 965 行三份文档，含三候选对比、被否方案记录、量化估算 —— **远超本项目既往水准** |
| 3.4 上手成本 | 🟢 | 数据流图 + 文件级落点表 + 测试清单，新人可直接开工 |
| 4.1 性能模型 | 🟢 | **KV cache 量化是本次设计的亮点**：识别出「压缩击穿前缀缓存」这个非显然的成本，并给出了 4 倍杠杆的判断 |
| 4.2 故障模式 | 🟡 | 见 R4（小窗口溢出）、R6（无超时无中断） |
| 4.3 可观测性 | 🟢 | 从**零事件**到两个事件 + `Usage` 暴露缓存命中率。这是一次实质跃升 |

---

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 |
|---|---|---|---|---|
| **R1** | **K-1 的修复缺少前提：`ModelRequest` 没有 `tool_choice`** | 高 | 中高 | **P0** |
| **R2** | **`summary_max_tokens` 在线上是死旋钮** | 中 | **必然** | **P0** |
| **R3** | **模块公开形状不一致**（`plan`/`execute`/`replace_all` 的调用方归属没定死） | 中 | 高 | **P0** |
| **R4** | **触发点未减输出预留** ⇒ 小窗口溢出（且溢出恢复已延后） | 高 | 中 | P1 |
| **R5** | **窗口配置粒度不组合**：单值全局 + 按 provider 内置表 | 中 | 中高 | P1 |
| **R6** | **压缩期间无超时、无中断**（`-p`/`--json` 下 `boundary = None`） | 中 | 中 | P1 |
| **R7** | **摘要以 `Role::User` 写回，且伪造的 assistant 消息被持久化** | 中 | 中 | P1 |
| **R8** | **`--json` 契约变化未标注**（会新增事件行） | 低 | **必然** | P1 |
| **R9** | 事件里的 `tokens_before/after` 是**估算值**，但命名成权威值 | 中 | 必然 | P2 |
| **R10** | `plan()` / `execute()` 各收一次 `messages` ⇒ 索引错配风险 | 中 | 低 | P2 |
| **R11** | 默认 `replace_all`（clear + append）**非原子** | 中 | 低 | P2 |
| **R12** | 文档残留旧数字与旧术语 | 低 | 必然 | P2 |

---

### R1（P0）K-1 的修复在本项目缺少必要前提：`ModelRequest` 没有 `tool_choice`

设计决策 10（ADR）要求摘要调用「**不改 `tools`**」以复用缓存前缀 + 复用 Codewhale 的「活对话 + 末尾指令」形状。

但：**`ModelRequest` 只有 `{ messages, tools, system, max_tokens, temperature }`**（`crates/ys-model/src/request.rs:7-16`），**没有 `tool_choice`**。而 Codewhale 在同一处**明确传了 `tool_choice: "none"`**（`tmp/Codewhale/crates/tui/src/compaction.rs:1533-1536`）。

**后果**：带着完整工具集发「请总结这段对话」的指令，模型**可能回一个 tool call 而不是文本** → `generate_summary` 拿到无 `Text` 块 → 返回 `Ok("")` → 触发 B-2。B-2 的修复会把它变成**一次失败的压缩**（安全，不丢数据），但**可能每次压缩都失败** ⇒ 功能系统性不可用。

> 讽刺的是：K-1 的修复（不复用 tools ⇒ 缓存未命中 vs 复用 tools ⇒ 可能被当工具调用）把这个坑**从「慢」变成了「坏」**。

**建议**：`ModelRequest` 加 `tool_choice: Option<String>`（或枚举），adapter 直通。`#[serde(default)]` 加法式改动。`tool_choice` 是 API 顶层参数、**不进 prompt 前缀**，不影响缓存。**这条必须进第一刀**，否则 K-1 不能安全落地。

---

### R2（P0）`summary_max_tokens` 在线上是死旋钮

设计的 `CompactPolicy` 有字段 `summary_max_tokens: usize // 1024（今日值）`。

但 adapter 的取值是 **`config.max_tokens.or(request.max_tokens)`**（`adapters/model-openai-compatible/src/lib.rs:415`），而 app 装配写死 **`max_tokens: Some(4096)`**（`apps/coding-agent/src/main.rs:610`）⇒ **config 覆盖 request** ⇒ 请求里设的 `1024` 被静默丢弃，**线上真实发出的是 4096**。

> 这不是新发现 —— `crates/ys-loop/src/summarize.rs:37-38, 71-77, 234-241` **已经把这个坑记录在案并明确选择「不设值」**（"在这里设值会被静默覆盖 —— 那是句假话，故不设"）。同样的判断**没有传导到压缩设计**。

**建议**：三选一 —— ① 删掉该字段（最诚实）；② 让摘要调用走 request-first 优先级；③ 文档明确标注「**仅库路径生效，app 装配下被 4096 覆盖**」。**绝不能留一个静默无效的旋钮。**

---

### R3（P0）模块公开形状与调用方归属没定死

设计里出现了**三种说法**：

| 位置 | 说法 |
|---|---|
| 模块划分 M1 / 接口概要 | 公开入口是 `plan()` + `execute()` |
| 数据流（自动） | `plan(...)` → `execute(...)` → **调用方** `session.replace_all(...)` |
| 数据流（手动） | `execute(...)` → **"wiring 已由 replace_all 更新"**（读起来像 `execute` 自己更新） |
| ADR 背景 / 决策 1 | 又叫 `compact_session` |

`execute(messages, plan, model, policy) -> Result<CompactOutcome, _>` **不持有 session**（这是它对的原因），所以 `replace_all` 只能由调用方做 —— 但手动路径的文字与此矛盾。

**建议**：把公开形状一次定死并在两处用例中一致。推荐：`plan` / `execute` 都保持无 session；**每条路径各自 `replace_all`**；`execute` 的返回值里带 `messages`。文案改成「execute(...) → `ports.session.replace_all(outcome.messages)`」。

---

### R4（P1）触发点没有为输出预留空间 ⇒ 小窗口会溢出

`trigger_at(w) = w × 0.85` 只留 15% 给「不是输入的东西」。但窗口还需要容纳**模型输出**（`max_tokens = 4096`）与 provider 侧的 framing 开销。

- 1M：15% = 150K ≫ 4096 ✅ 无问题
- **8K（现在可配了）：触发 6800 输入 + 4096 输出 = 10896 > 8192 ❌ 溢出**

而**溢出恢复（`is_context_overflow`）按决策 8 已延后** ⇒ 撞上就是硬失败。

**同类实现都做了这件事**：Codewhale `input_budget_ceiling = window − output_cap − 1024` 并把触发钳制其上（`context_budget.rs:203`）；Claude Code 预留 33–45K。

**建议**（3 行）：`trigger_at(w) = min(w × 0.85, w.saturating_sub(output_reserve + CONTEXT_HEADROOM))`。**低成本、消掉一整类故障**。

---

### R5（P1）窗口配置粒度不组合

D5/决策 3 定的是：**`config.json` 单值全局** > **`provider.rs` 内置表（按 provider）** > 默认。

问题：**窗口是模型的属性，不是 provider 的属性**。
- `ProviderInfo` 每 provider 只有**一个** `default_model`（`provider.rs:9-15, 73, 79`），但 DeepSeek 至少两个模型（`deepseek-chat` / `deepseek-reasoner`，见测试夹具 `provider.rs:260-268`）
- 于是：`/model` 在**同一 provider 内**切换模型时，窗口**不会变**（内置表按 provider 查、config.json 是全局）
- 且全局覆盖会**跨 provider 误用**：`config.json` 设 1M，之后 `/login` 到一个 128k 的 provider → 用错窗口 → 溢出（R4）或过早压缩

**建议**：本轮至少把解析键从 `provider` 提到 **`model`**（`config.json` 与内置表都用 model id 作键）。成本极低（都是一个 `HashMap` 查表），消掉一整类误配。若坚持按 provider，须在设计中**显式写明这条欠账的后果**。

---

### R6（P1）压缩期间无超时、无中断

摘要调用是一次**完整的模型往返**（设计自估 5–30s，1M 输入下可能更久）。期间：

- 无超时 —— `bash_timeout` 名为 bash、施于工具路径，**摘要调用不走工具路径**
- 无中断 —— `-p`/`--json` 下 `boundary` 恒 `None`（`wiring.rs:115-122`）
- 无 `Started` 事件（设计有意为之，避免悬挂态）⇒ **UI 上只有一个 spinner**，用户不知道卡在哪

`context.md` 的 Q16 / 范围 A7 把这条列为「**是否本轮处理待定**」—— 但设计文档**没有给出结论**，而它是 P1。

**建议**：把「`-p`/`--json` 下也给 `boundary` 或加一个压缩超时」显式定性为**本轮做 / 明确不做**，不要悬空。至少给摘要调用一个超时。

---

### R7（P1）摘要角色与伪造 assistant 消息

设计保持现状：`[summary(Role::User), ack(Role::Assistant), ...kept]`。两个后果：

1. **伪造的 assistant 消息被持久化** —— `I have the context summary. Continuing.` 会**写进 JSONL 会话文件**，重启恢复后当作模型真实输出重放。对话记录失真（`/export` 也会带出去）。这条**在设计的任何一节都没被讨论**。
2. **可能破坏角色交替** —— 若 `kept[0]` 是 Assistant，则出现 `Assistant(ack), Assistant(kept[0])` 连续两条。多数 OpenAI 兼容端点容忍，但**这是未验证假设**。

`context.md` 的 Q8（摘要消息用什么角色）**始终未决**，设计**静默沿用了现状**。

**建议**：显式决策 Q8。至少回答「合成 ack 消息是否必要」（它的存在疑似只是为了维持交替 —— 若如此，应记录理由；若非必要，删掉）。

**✅ 已处置（决策 D18）：删掉合成 ack，交替由构造保证。**

关键论据有二，都比「改动小」更有力：

1. **它根本没解决交替，只是挪了失败位置**：

| | `kept[0]` 是 User | `kept[0]` 是 Assistant |
|---|---|---|
| 无 ack | `User, User` ❌ | `User, Assistant` ✅ |
| **有 ack** | `User, A, User` ✅ | `User, A, Assistant` ❌ |

2. **pi 与 Codewhale 都不加合成 assistant 消息**（实测：pi 的 `COMPACTION_SUMMARY_PREFIX` + summary 只产出**一条 user 消息**；Codewhale 是**一条 user 角色的摘要 checkpoint**；grep 两仓无对应文案）—— **三家实现里只有 YuShan 加**。

**构造保证**：`kept[0]` 是 User ⇒ 摘要**并入**它（前缀）；否则摘要作为独立 User 消息插在最前。零伪造数据、交替恒成立。`Role::User` 保留（改角色需另议）。

---

### R8（P1）`--json` 契约变化未标注

设计称新事件是「**非终局事件**（`is_terminal` 不匹配 → `-p` 输出逐字节不变）」。**对 `-p` 成立 ✅**（`print_delta_text` 只匹配 `ModelTextDelta`，`main.rs:209-214`）。

但 **`--json` 是逐行打印每个 `Envelope`**（`write_json_envelope`，`main.rs:201`）⇒ **新事件会新增输出行**。这是 `--json` 消费者的**契约变化**（加法式，但需明说）。

**建议**：文档补一句「`--json` 会增加两类事件行」；若 `--json` 有 golden 测试，需同步更新。

---

### R9（P2）事件里的 token 数是估算值

`ContextCompacted { tokens_before, tokens_after }` 由 `estimate_session_tokens`（启发式 `ASCII字节/4` + CJK `1.5/字`，`crates/ys-loop/src/token.rs`）算出，**不是 provider 的真实 usage**。

而按 D8，**这两个数正是用来调 `keep_ratio` 的唯一数据源**。用未校准的估算值当调参依据，会让调参结论带上估算的系统偏差。

**建议**：字段改名 `estimated_tokens_before/after`（诚实的命名成本为零），或同时带上有锚点时的真实 usage。

---

### R10（P2）`plan()` / `execute()` 各收一次 `messages` ⇒ 索引错配风险

`plan(messages, ...) -> CompactPlan { summarize: Range<usize>, keep: Range<usize> }`，随后 `execute(messages, &plan, ...)`。Ranges 只对**同一个 slice** 有效，但类型上没有任何东西绑定二者。

**建议**：`CompactPlan` 存 `len: usize`，`execute` 入口 `debug_assert_eq!(plan.len, messages.len())`。一行成本。

---

### R11（P2）默认 `replace_all` 非原子

`Session::replace_all` 的默认实现 = `clear()` + 逐条 `append()`。对 `JsonlSession` 覆写为单次原子写 ✅，但**默认实现本身非原子**：`clear` 成功后某个 `append` 失败 ⇒ 会话被截断。

当前只有 `MemorySession`（不会失败）用默认实现，所以**现实风险低**；但 trait 是公开的，第三方实现会继承这个语义。

**建议**：doc comment 明确写「默认实现非原子；要求原子性的实现须覆写」。

---

### R12（P2）文档残留施工痕迹

| 位置 | 问题 |
|---|---|
| `design-compaction.md:68` | `["Compacted: 112k → 21k tokens (48 messages summarized, 6 kept)."]` —— **旧参数**（对应被放弃的 `111616/20000`），应为 `850k → ~50k` 量级 |
| 多处候选描述 | 「128k 逐位等价」作为 A/B 的*历史*贡献保留 —— 准确，但与已放弃的决策并置易误读（已有标注 ✅） |
| 模块命名 | `compact_session`（ADR）vs `plan`/`execute`（设计）并存（见 R3） |

---

## 改进建议

### 易修复（低风险快速改进）

1. **统一 `plan`/`execute`/`replace_all` 的公开形状与调用方归属**（R3），并把 `compact_session` 这个名字从 ADR 里去掉或明确为别名
2. **修 `design-compaction.md:68` 的旧数字**（R12）
3. **`CompactPlan` 带 `len` + `debug_assert`**（R10）
4. **事件字段改名 `estimated_tokens_*`**（R9）
5. **文档补 `--json` 会增加事件行**（R8）
6. **`replace_all` 的 doc comment 写明默认实现非原子**（R11）

### 需讨论（需要决策）

7. **`ModelRequest` 加 `tool_choice`**（R1）—— K-1 能安全落地的前提。**建议本轮做**
8. **`summary_max_tokens` 的取舍**（R2）—— 删字段 / 改优先级 / 明确标注。**必须给结论，不能留死旋钮**
9. **触发点减去输出预留**（R4）—— Codewhale 形状，3 行。**建议本轮做**
10. **窗口解析键从 `provider` 提到 `model`**（R5）
11. **压缩期的超时与中断定性**（R6）—— 至少加超时；`-p`/`--json` 的中断可显式延后但要写明
12. ~~**摘要角色 Q8 + 合成 ack 消息是否必要**（R7）~~ → **已决（D18）：删 ack，交替由构造保证**

### 架构级（影响面大）

13. **`TokenLedger` 锚点对账（第二刀）** —— 它同时解决 R9（事件数字是估算）与「触发点系统性偏移」。设计已把它列为第二刀并指出**应在 D1（skills）之前完成**，这个排序是对的，**建议保持并在第一刀落地后立刻启动**
14. **`Session` 的常态 append 也是 O(n) 全文件重写**（`jsonl.rs:50-84`）—— 本设计只修了压缩路径的 O(n²)（`replace_all`），**常态路径的写放大仍在**。设计已明确列为待独立的 ADR，边界划得对

---

## 总体评价

**整体健康度：良好偏优。** 这份设计的方法论质量明显高于本项目既往水准 —— 三候选并行生成 + 实证验证 + 被否方案留档 + KV cache 量化，且**发现了三个既有真 bug**（其中 `keep_from == len` 与「压缩失败 = 静默失忆」两个是 A/B 两个候选都没发现、靠实证验证暴露的）。B-3（失败不再截断历史）单项就把压缩的错误处理从破坏性翻转为无损。

**但存在三个 P0，共同特征是「设计文档里的假设与代码事实不符」**：

- **R1** 假设可以「复用 tools 不改」—— 但本项目没有 `tool_choice` 来抑制工具调用，会让 K-1 从「慢」变成「坏」
- **R2** 假设 `summary_max_tokens` 是个有效旋钮 —— 但 app 装配的 `config.max_tokens` 会静默覆盖它，**这个坑本仓库已经记录在案却没传导过来**
- **R3** 假设模块形状已定 —— 但文档里三种说法并存

**这三个都不是算法问题，是「设计与代码的接缝没核对到底」**。修起来都不贵（R1 加一个字段、R2 删一个字段或加一句注释、R3 统一措辞），但**不修就会在第一刀落地时暴露**：R1 会让压缩在真实 provider 上间歇性失败，R2 会让调参的人困惑于「改了没反应」。

**最大风险点是 R1**，因为它把一个设计决策（复用前缀以命中缓存）与一个代码能力缺口（无法抑制工具调用）绑在了一起 —— 而**设计文档把 K-1 描述为「零下行风险」**，实际上它引入了 B-2 的系统性触发面。这一点需要更正。

**推荐下一步**：

1. 先修 R1/R2/R3 三个 P0（都是小改动，但改的是设计的正确性）
2. 补 R4/R5 两条（触发点减预留 + 窗口按 model 解析），都是低成本、消一类故障
3. 然后按设计进入实现；**第一刀落地后立刻启动第二刀 `TokenLedger`**（赶在 D1 skills 之前）

---

## 相关文档

- [`design-compaction.md`](./design-compaction.md) — 被分析的设计方案
- [`context.md`](./context.md) — 上下文与决策 D1–D11
- [`../../adr/0017-compaction-hardening.md`](../../adr/0017-compaction-hardening.md) — 决策记录
