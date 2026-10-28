# 0060 — 思考开关按调用面分派：请求级 `ThinkingOverride`，不建策略表

把「思考开关」从 model 实例搬到已经存在的请求级边信道（`ModelRequest.disable_thinking`
的先例），让主轮 / 工具摘要 / 压缩各自设值；**默认值与今天逐字节相同**，
真正的收益开关由 A/B 读数决定是否打开。

## Status

proposed（2026-10-28；设计见 `docs/arch/effort-per-call-type/design.md`）

## 背景

现状（as-built）：

- `ModelRequest.disable_thinking: Option<bool>` 已是请求级边信道（`9b573e4`）。
- **只有工具摘要**设了 `Some(false)`（`crates/ys-loop/src/summarize.rs:93`）；主轮与压缩是 `None`。
- 用户面只能 `/effort off` **全局一刀切**（`e269f82`），无法表达「摘要关、主轮开」以外的组合。
- `reasoning_effort`（**强度**）仍冻在 `OpenAICompatibleConfig`，主轮/压缩/摘要共用一个
  `Arc<dyn Model>`（`basic.rs:244/1011`、`compact.rs:365`、`summarize.rs:100`）。

两处诱因：

1. `docs/TODO.md:155` 的待办要求「按调用类型分发档位/开关」，并点名结构障碍与两条出路。
2. 用户观察到「模型一两次工具调用后就输出工作介绍」，量级达每次长探索 37 段 / ≈4,229 token。

## 决策

0. **前置：`opencode_go()` 置 `supports_thinking_control = true` —— 已落地（2026-10-28）。**
   第一优选模型是 `deepseek-*`，但它实际经 `opencode-go` 网关接入
   （`auth.json` 的 `opencode-go` → `https://opencode.ai/zen/go/v1`），而
   `ProviderCompat::opencode_go()`（`compat.rs:94`）原先**没有**置该位 ⇒
   在这之前，本 ADR 的全部收益为 0。

   已直接打网关验证（各 n=3）：短任务 reasoning 145→0；需推理任务 **430→0**。
   并经**真实 agent 链路**核实：`YUSHAN_REASONING_EFFORT=off` ⇒ `reasoning 0`；
   **不配 `off` 时仍照旧思考**（reasoning 67）⇒ **默认行为未变**。

   测试与变异：`enabling_opencode_go_does_not_change_the_default_request_body`
   （`lib.rs`）钉住「能力位 ≠ 发送」；`opencode_go_and_deepseek_are_the_only_thinking_control_vendors`
   （`compat.rs`）含**反向断言**（不向 GLM / MiMo / standard / minimax 扩散）。
   三处变异（删能力位 / 无条件发 / 去门控）均被抓。

   ⚠️ **仍是探测级证据**（单轮、无工具调用）：**质量与轮数必须实跑 A/B。**

1. **`ModelRequest.disable_thinking: Option<bool>` 泛化为 `thinking: Option<ThinkingOverride>`****
   （`thinking` / `effort` 两个可选覆盖），`#[serde(skip, default)]` 契约不变。
   **不与旧字段并存** —— 两个语义重叠的字面量会让「谁赢」无从回答。
2. **不引入 `CallKind` 枚举与策略表。** 调用面只有三处，且都在看得见的位置：
   主轮查 `RunLimits`、摘要写字面量、压缩保持 `None`。等第二个真实策略需求出现再建表。
3. **主轮的开关由 `RunLimits.main_disable_thinking: Option<bool>` 决定，默认 `None`。**
   `None` 是**承重默认**：老调用方行为逐字节不变（沿用 `fixed_prompt_tokens = 0` /
   `max_turn_tool_result_tokens = 0` 的惯例）。
4. **第一版不暴露配置项**，只用 `YUSHAN_MAIN_THINKING` 供 A/B；拿到读数后再决定**默认值**。
5. **不做「按轮序号衰减」**（方案 A）：阈值是行为学近似，`Steer` 插话后模型已回到需推理
   状态而计数继续 ⇒ 会误关；且无法表达「子 agent 起手就关」。若 A/B 证明需要分段，
   它应是一行调用点判断，不是 `RunLimits` 的新维度。
6. **压缩保持开思考**（`docs/TODO.md:160` 既定）。**强度的门控与方言折叠一行不改**
   （仍是 `ProviderCompat::effort_for`）。

## 理由：收益口径必须按金额算

**reasoning token 从不进会话历史。** `finish_with_logger`
（`adapters/model-openai-compatible/src/lib.rs:1137`）只装 `text` 与 `tool_calls`；
`ContentBlock`（`crates/ys-core/src/message.rs:20`）没有思考变体。

⇒ 关思考**不省任何一个后续请求的 input**，只省本轮 output 与本轮 gen 时间。

**但「不省 input」不等于「省得少」**：输出 4 元/百万，是缓存命中（0.02）的 **200 倍**。

实测拆解（`scripts/cost-audit.py`，DeepSeek 会话 23 个 / 2,518 轮，闲置时段价）：

| 构成 | token | 元 | 占比 |
|---|---:|---:|---:|
| 输入 · 缓存命中 | 370,528,961 | 7.41 | 35.6% |
| 输入 · 未命中 | 6,243,008 | 6.24 | 30.0% |
| 输出 · reasoning | 958,861 | 3.84 | **18.4%** |
| 输出 · 工具参数 | 801,162 | 3.20 | 15.4% |
| 输出 · 可见正文 | 93,836 | 0.38 | **1.8%** |
| 合计 | | **20.84** | 100% |

⇒ **关思考可省 18.4%（DeepSeek）**。两个错误口径均被推翻：
`69%` 是 reasoning 占 output 的**token** 比例（忽略计价倍率）；
`−3~5%` 则错误地把 output 当成小头。

两条副产物：

- **削减叙述只值 1.8%** —— 即使把全部可见正文压到零也只省 0.38 元。⇒ **不为省叙述动提示词，更不为它破缓存前缀。**
- **工具调用参数占 15.4%**（比全部可见正文贵 8.4 倍），是被忽略的第二项。

provider 差异极大（`deepseek` 18.4% vs `MiniMax-M3.1-Flash` 5.0%）⇒ **不跨 provider 外推**。
关思考仍**不能外推到主轮质量** —— `low` 已实测让 `fact/1` 从 3/3 掉到 2/3。

## Consequences

- **正面**：主轮与子 agent 拿到「关思考」的可达路径，不必再全局 `/effort off`；
  不破前缀缓存（`thinking` 是请求体顶层字段，不进 `messages`/`system`/`tools` 前缀）。
- **负面**：新增一个 `RunLimits` 字段与一个 `ThinkingOverride` 类型；~8 处 `disable_thinking`
  字面量需机械同步改（编译器全数报出）。
- **已知边界 A**：能力位**已置位**（`opencode-go` + `deepseek` 两家）。未配 `/effort off` 时
  请求体**仍逐字节不变**（`config.disable_thinking = false`，已用真实链路验证）。
  未实测的方言（GLM / MiMo / standard / minimax）仍为 `false`，有反向断言钉住。
- **已知边界 B**：用 `YUSHAN_API_BASE` 直连时 provider 名取 `custom`（`config.rs:515`），
  拿`ProviderCompat::standard()` ⇒ 同样不发该字段。只有内置 provider 名才有能力位。
- **未决**：主轮默认是否关思考，必须由 `fact/1` A/B（n≥3）决定；基线本身跨 64.4–201.4 s
  （`ADR-0042`）⇒ **中位差 <20% 一律记为「不可判」**。

## 关联

- `crates/ys-core/src/effort.rs` — `ThinkingOverride` 定义位置（已有 `EffortLevel`）
- `crates/ys-model/src/request.rs` — 字段泛化点
- `crates/ys-component/src/limits.rs` — `main_disable_thinking`
- `crates/ys-loop/src/{basic,summarize,compact}.rs` — 三个调用面
- `adapters/model-openai-compatible/src/lib.rs:1347` — wire 合成（能力位门控不绕过）
- `apps/coding-agent/src/{config,main}.rs` — `resolve_limits` 与 A/B env
- `docs/arch/effort-per-call-type/design.md` — 完整设计、测试矩阵、被否决方案
