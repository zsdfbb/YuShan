# turn-output-and-feedback — 架构质量分析报告

> 日期：2026-10-12 ｜ 产出：`/arch-validate`（基线 `3c9aef5`）
> 分析对象：[`design.md`](./design.md) + [`adr-turn-output-and-feedback.md`](./adr-turn-output-and-feedback.md)（辅以 [`context.md`](./context.md)）
> 方法：文档分析 + **代码核对**（结论均带 `文件:行号`，不作纯文档转述）

## 分析范围

- **对象**：本需求的设计方案（第一批：`StopReason::NoOutput` + 软阈值提示 / `context` 钳制）。
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性），并启用基础软件重点（错误处理、资源管理）。
- **来源**：`design.md` / `adr` / `context.md`，并对照 `crates/ys-loop/src/basic.rs`、`crates/ys-core/src/stop.rs`、
  `adapters/model-openai-compatible/src/lib.rs`、`apps/coding-agent/src/main.rs`、`crates/ys-tool/src/*` 等实现。

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | `StopReason` 加法式新增有先例（`Truncated`，`crates/ys-core/src/lib.rs:107`），`#[non_exhaustive]` + 下游 `_ =>` 兜底；软阈值 / 钳制是局部改动；无新依赖 |
| 1.2 依赖成熟度 | 🟢 | 不引新依赖（硬约束） |
| 1.3 实现周期 | 🟡 | 代码量小，但需同步 `docs/CONTEXT.md`、`docs/design.md` §5/§12 与多份 `#[non_exhaustive]` 消费者 |
| 2.1 模块边界 | 🟢 | 改动集中在 loop 一处 + tools + TUI；`ys-tool` 复用点有 grep/read 两个真实消费者 |
| 2.2 接口稳定性 | 🟢 | `StopReason` 为加法式；`size_notice` / `SOFT_RESULT_BYTES` 为内部新增 |
| 2.3 错误处理 | 🟡 | **见 R2 / R3** —— `-p/--json` 在 `NoOutput` 下目前会「exit 0 且无输出」；传输层空响应与合法空响应在 loop 层**同形**，边界未落笔 |
| 2.4 并发安全 | 🟢 | 不涉及并发路径，无共享状态改动 |
| 2.5 资源管理 | 🟢 | 无新资源；量级不变（结果仍 ≤ 2000 行 / 50KB） |
| 3.1 概念一致性 | 🟡 | `NoOutput` 与 `llm-api-unavailable` 的「合法空回复 = 正常」存在**语义张力**，同一情形两处叫法不同 |
| 3.2 抽象层次 | 🟢 | 词汇层（`ys-core`）/ 控制流层（`ys-loop`）/ 形态层（`ys-tool`）分层清晰 |
| 3.3 文档完整度 | 🟡 | `design.md` 顶部混入**过程性注记**（subagent 未送达）；`NoOutput` 尚未进 `CONTEXT.md`/§5 |
| 3.4 上手成本 | 🟢 | 改动面窄、命名直白 |
| 4.1 性能模型 | 🟡 | **见 R1** —— 验收指标弱于问题：warn-only 不减少**当轮** token；观察到的 45KB 用例**既不触发钳制也不触发硬截断** |
| 4.2 故障模式 | 🟢 | 无单点 / 无级联；失败路径不变 |
| 4.3 可观测性 | 🟡 | 空响应有了**可见行**（进步），但**无计数 / 指标**，「空回合率」仍不可量化 |

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|------|------|--------|--------|------|
| R1 | **warn-only 不减少当轮 token，验收指标可被「加提示」满足而不解决体积问题** | 高 | 高 | **P1** | 指标补「单条结果字节 p95 / 当轮回喂 token」，或对 `grep` 设更低的**专用上限**（收紧合规） |
| R2 | **`-p/--json` 在 `NoOutput` 下 exit 0 且无输出**（`single_turn_exit_code` 只对 `Cancelled` 返 1，`main.rs:1170-1176`）——「无产出」被当成功 | 中 | 高 | **P1** | 明确定义 `NoOutput` 的退出码 / stderr 行，并更新该函数的 doc 注释 |
| R3 | **传输层空响应与合法空响应在 loop 层同形**，本设计把两者都判 `NoOutput`；`llm-api-unavailable` 要求前者是**错误** | 中 | 中 | **P1** | 在两份文档写死边界与**落地顺序依赖**（谁负责把传输层空转成 `Err`） |
| R4 | 会话仍**落盘**空 assistant 消息（`basic.rs:326-330` 先 append） | 低 | 高 | P2 | 已核对 `translate_request` **会跳过**空消息（`lib.rs:838`），不会发给 provider；仅 resume 回放留空条 |
| R5 | 空白字符（`" "`）响应不算 `NoOutput`，仍以 `Completed` + 空白展示 | 低 | 中 | P2 | 改 `text_content.trim().is_empty()` 判定 |
| R6 | `read` 软提示可能与既有 `offset/limit` 提示重复噪音 | 低 | 中 | P2 | 真实会话对照后再定 |
| R7 | `design.md` 顶部过程性注记（subagent 未送达） | 低 | 确定 | P3 | 移到提交信息或删去 |

### 关键证据

- **R1（最要命）**：观察用例是 `grep{pattern:"M4", context:4}` → 45,255 字符。
  `MAX_CONTEXT = 5` ⇒ `context=4` **不触发钳制**；`MAX_BYTES = 50KB`（`crates/ys-tool/src/truncate.rs:24`）⇒ 45KB **不触发硬截断**
  （`grep.rs:230-256` 的 notice 只在命中 `limit` 或超 50KB 时出）。**⇒ 该次调用只有新增的软阈值提示会触发，
  而 45KB（≈11k token）已先行进入上下文**。设计选择「不削内容」是有意的，但 `context.md` 的验收指标
  「单条偏大且无 notice → 0」**只测到「有没有提示」，测不到「体积是否受控」** —— 指标可被满足而危害仍在。
- **R2**：`apps/coding-agent/src/main.rs:1166-1176` 的 `single_turn_exit_code`：`Cancelled ⇒ 1`，其余（含新的 `NoOutput`）⇒ **0**；
  `-p` 只打印流式文本增量，空响应 ⇒ stdout 空、exit 0。
- **R3**：适配器 `finish()` 对「200 + 非 SSE 体 / 零 delta」无条件返回 `Ok(空响应)`（`docs/arch/llm-api-unavailable/context.md:44,72`），
  到 loop 层与「合法空回复」**不可区分**。
- **R4**：空消息被 `translate_request` 跳过（`adapters/model-openai-compatible/src/lib.rs:838` 的
  `if !text_content.is_empty() || !tool_calls.is_empty()`）—— 故**不**会造成 provider 拒收，风险降级为「落盘留空条」。

## 改进建议

### 易修复（低风险快速改进）

1. **更新 `single_turn_exit_code` 的语义与注释**（R2）：明确 `NoOutput` 的退出码；至少 `-p` 在 stderr 打一行说明，避免「静默成功」。同函数 doc 注释里列举的原因集合要补 `NoOutput`。
2. **补一条体积指标**（R1）：在 `context.md` 的量化验收里加「单条工具结果字节 p95 / 当轮回喂 token 上限」这类**直接测危害**的指标，或显式写明「本批只治可解释性、不治体积，体积留第二批」。
3. **写明 `-p/--json` 在 `NoOutput` 下的期望行为**（R2）：进 `design.md` 的量化验收表，别只写「与 llm-api-unavailable 口径一致」。
4. **改 `text_content.trim().is_empty()`**（R5），把空白响应也归入 `NoOutput`。
5. **删掉 `design.md` 顶部的过程性注记**（R7），保持设计文档只写当前设计。

### 需讨论（需要拍板）

1. **warn-only vs 真收窄**（R1）：是否给 `grep` 一个更低的**专用字节上限**（例如 16KB）。这与「不放宽上限」不冲突（是收紧），但会**丢部分有用结果** —— 是「提示」与「兜底」之间的取舍。
2. **`MAX_CONTEXT = 5` 的价值**：对观察用例（`context=4`）无效。是保留（防更宽），还是按 `limit` 联动（如 `context ≤ clamp(20 / limit)`）让放大因子可控？
3. **`NoOutput` 是否纳入 `finish_reason`**：若把判定收敛为「`finish_reason == Stop` 且无文本」，可与 `llm-api-unavailable` 清晰分工（传输层空 ⇒ `Err`；合法空 ⇒ `NoOutput`），但需适配器先落地那部分。
4. **`NoOutput` 命名**是否用 `Empty` / `NoContent`，与 `CONTEXT.md` 术语风格一致。

### 架构级（影响面大）

1. **「回合产出可见性」的观测指标**：把「空回合率 / 单条结果字节分布」纳入 `yushan.log` 或统计脚本，否则「可量化、可测试」只停在单测层，线上趋势不可控。
2. **与 `llm-api-unavailable` 的边界统一**：两张文档需要一处**共同的真值**说明「传输层空 = 错误、合法空 = NoOutput」，以及两批改动的**落地顺序依赖**（本需求不应静默吞掉传输层错误）。

## 总体评价

**整体健康度：中上（🟢×8 / 🟡×5 / 🔴×0）。** 设计在**诚实性**（新终止原因而非复用 `RunFailed` / `Completed`）与
**边界纪律**（不引依赖、不放宽上限、加法式变体）上是对的，落点清晰、可测、可变异测，实现风险低。

**最大风险是「验收指标与被治的害不一致」**（R1）：第一批的 A 只加提示、不减体积，而观察用例恰好
**既不触发钳制也不触发截断**，于是「指标达标」并不等于「问题缓解」。建议先把指标改成直接测体积 / token，
或显式把「体积收窄」提前到第一批（哪怕只给 `grep` 一个专用上限）。

**次大风险是 `-p/--json` 的静默成功**（R2）：与 `llm-api-unavailable` 的「不可静默装作成功」精神相悖，
属易修复，应在本批一起做。

**推荐下一步**：先做「易修复」1–5（半天），再把「需讨论」1（是否真收窄）拍板后定稿 `docs/CONTEXT.md` 与
`docs/design.md` §5/§12；实现顺序不变（B 的空响应可见 + A 的提示先行，体积收窄视拍板结果决定是否并入第一批）。
