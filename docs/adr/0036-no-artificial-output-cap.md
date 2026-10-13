# 0036 — 不设人为输出上限：主循环请求省略 `max_tokens`，交由 provider 自身的模型上限

去掉 coding-agent 写死的单次输出上限：`apps/coding-agent/src/main.rs` 构造模型时的
`OpenAICompatibleConfig.max_tokens` 由 `Some(4096)` 改为 **`None`**。主循环请求
（`ModelRequest.max_tokens` 也是 `None`）经适配器的 `config.max_tokens.or(request.max_tokens)`
解析为 `None` ⇒ `ChatCompletionRequest` 的 `max_tokens` 字段**整条不序列化** ⇒ 请求交给
**provider 自身的模型上限**（DeepSeek 单次可达 ~200k）。零 core crate 改动、零协议变体。

## Status

accepted（2026-10-13）

## 背景

- **需求**（原话）：「不要限制这个上限，只跟 model 输出上限保持一致。DeepSeek 一次输出上限在 200 多 k。」
- **现状是一个全局覆盖**：`adapters/model-openai-compatible/src/lib.rs` 是
  `config.max_tokens.or(request.max_tokens)` —— **config 优先**。app 侧写死 `config.max_tokens = Some(4096)`
  ⇒ **每一类**外发调用（主循环、压缩摘要、工具摘要）都被强行压到 4096，`request` 侧的值形同虚设。
- **后果**：模型本可输出到 provider 上限（DeepSeek ~200k），却被我们人为掐在 4096 —— 长回答 / 大会话总结
  频繁触发 `finish_reason: length`（截断），多步任务更容易在半途被打断（正是 ADR-0035 那类故障的放大器）。
- **适配器早已支持「不设」**：`ChatCompletionRequest.max_tokens` 带
  `#[serde(skip_serializing_if = "Option::is_none")]` ⇒ 解析为 `None` 时整条字段不发，provider 用自身上限。

## 决策

**1. `config.max_tokens = None`（主循环 ⇒ provider 自身上限）**

- `main.rs` 的两个工厂（主模型 / 按名构造的 subagent 模型）都改 `Some(4096)` → `None`。
- 主循环请求 `max_tokens` 本就是 `None` ⇒ 解析结果 `None` ⇒ **字段不序列化** ⇒ 不人为截断。

**2. 辅助调用的上限各自在其请求里表达（config 不再是覆盖源）**

- **工具摘要**（`ys-loop/src/summarize.rs`，webfetch / subagent）：请求侧 `Some(SUMMARIZE_MAX_TOKENS = 1024)`。
  此前被 config 覆盖成 4096，现在**就是**线上生效值 —— 这是该常量的本意。
- **压缩摘要**（`ys-loop/src/compact.rs::generate_summary`）：请求侧**不设** ⇒ provider 上限。
  有意如此：压缩的目的是缩短历史，摘要若被偏小的上限截断会**丢历史**，与目的相悖；交由 provider 上限更稳。

**3. 明确不做**

| 项 | 为什么不做 |
|---|---|
| 把上限写死成 `200_000` 之类 | 把某个 provider 的数字烙进核心；provider/模型一换就错。**不设**才是「跟模型上限一致」 |
| 加 `config.json` 旋钮 | 需求是「不设」；加旋钮是加法、可后补（真要给高级用户一个上限时再加） |
| 同步放大压缩策略里的 `output_reserve`（4096） | 它是**压缩触发点的余量**，只在窗口极小时才约束 `trigger_at`；未实测前不动这个调过的值（见下） |

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 不设（采纳）** | `config.max_tokens = None`；辅助调用各自表达上限 | 与「跟模型上限一致」字面相符；适配器已支持；改动最小 | provider 若默认值偏小，需 provider 侧设对；失控响应可能很长（成本） |
| **B 保持 4096** | 不动 | 零改动、成本有界 | 正是被报告的故障：人为截断 |
| **C 写死一个「大数」** | 例如 `Some(200_000)` | 「看起来」放开了 | 把 provider 数字烙进代码；换模型即错；退化成另一种人为上限 |

**采纳 = A**。

## Consequences

- **正面**：主循环不再人为截断，模型可输出到 provider 上限；截断频率下降 ⇒ ADR-0035 的「续写一次」
  作为**兜底**仍在（真被 provider 掐断时能接着做），二者互补。
- **负面 / 取舍**：单次响应可能很长 ⇒ 单轮 token 成本与延迟上界变宽（由 provider 上限天然封顶）；
  一个「话痨」响应可能占用较多上下文（压缩按窗口触发，仍受约束）。
- **行为变更（须写进 PR 描述）**：coding-agent 请求**不再携带 `max_tokens`**；工具摘要的上限由
  被覆盖的 4096 变为**真正生效的 1024**；压缩摘要由 4096 变为 **provider 上限**（有意）。
- **未实测**：真实 provider（DeepSeek 等）在缺省 `max_tokens` 时的默认输出上限行为 —— 需真实凭证的
  `e2e_*`（`#[ignore]`）路径；本次以本地回环 mock 断言「请求体省略 `max_tokens`」为准。

## 关联

- [ADR-0020](./0020-truncation-guard.md) / [ADR-0035](./0035-truncation-continuation.md) — 截断保护与其续写兜底
  （本 ADR 降低截断发生频率，是它们的上游缓解）
- [ADR-0027](./0027-completion-check-and-round-budget.md) — 单回合预算/兜底（本 ADR 不改它）
- `adapters/model-openai-compatible/src/request.rs` — `max_tokens` 的 `skip_serializing_if` 契约
- `docs/design-final/tools-web.md` §偏离 8、`docs/design-final/compaction.md` — as-built 同步
