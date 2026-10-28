# effort-per-call-type — 架构设计：思考开关的按调用面分派

> 日期：2026-10-28
> 性质：arch-design 产出（承接 [`context.md`](./context.md) 的现状与约束）。
> 决策记录见 [`../../adr/0060-thinking-policy-per-call-kind.md`](../../adr/0060-thinking-policy-per-call-kind.md)。
> **成本口径与实测数据见 [`cost-analysis.md`](./cost-analysis.md)** —— 本文只引用结论，不复述推导。
> 目标（用户原话）：「再保持基本效果的情况下提升工作速度、降低 token 消耗。」
> **唯一优化标准：DeepSeek**（第一优选模型，经 `opencode-go` 网关接入）。

## 0. 一句话结论

**先把「思考开关」从实例搬到已存在的请求级边信道，然后按调用面分别设值；默认值与今天逐字节相同。**
收益的主项不是「省钱」而是「省时间 + 省 output」——**因为 reasoning token 从不进会话历史，关掉它不减少任何一个后续请求的 input**。

## 1. 需求与目标

### 1.1 关键场景

| # | 场景 | 期望 |
|---|---|---|
| S1 | 长回合（≥10 轮）的执行段 | 后段关思考，单轮 `gen` 段下降 |
| S2 | 子 agent 派发（检索-汇报型） | 关思考，与工具摘要同构 |
| S3 | 工具摘要（webfetch / subagent） | **保持现状**（已关，实测 −56% completion / −42% 墙钟 / 质量未降） |
| S4 | 压缩摘要 | **保持现状**（开思考）——压缩结果进会话，错了要付后续每轮的代价 |
| S5 | 用户 `/effort off` | **仍然是全局一刀切**（覆盖所有面），语义不变 |
| S6 | 非 DeepSeek provider | 静默不发该字段，行为逐字节不变 |

### 1.2 质量目标

- **不退化**：`fact/1` 的机械判据（`min_citations: 6`）连续 3 次 `Quality::Pass`。
- **不换轮数**：轮数中位不得高于基线中位 +1。轮数是乘数（同题对照：我们 33 轮 vs codex 6 轮，`docs/arch/session-latency/design.md:173`），多一轮就把省下的时间赔回去。
- **可归因**：每个杠杆单次单独提交、单独 A/B，不混跑。

### 1.3 非目标（明确不做）

| 不做 | 理由（已有决策） |
|---|---|
| 压缩降档 / 关思考 | `docs/TODO.md:160` 明确不做；pi 与 Claude Code 都让压缩用主模型同档 |
| per-turn 换模型 | `docs/arch/per-turn-model-switch/` Q1 未决，代价远大于换开关 |
| 动 `reasoning_effort` 的**强度**门控 | 那是 `ProviderCompat::effort_for` 的职责，本次不改（除把强度通道打通，见 §3.3） |
| 删 skill 目录段 | ADR-0019「代码不替用户裁」；且缓存命中后金钱成本 ≈0.1×（`cost-model.md` §10.3） |
| 调 `keep_ratio` | 实测无效：0.10→0.03 每次压缩只多撑 8 轮（`session-latency/design.md` §1.1） |

## 2. 事实基础：为什么收益口径必须按金额

这一节是本次设计最容易被凭直觉搞错的地方，先钉死。**完整表格、推导与数据源见
[`cost-analysis.md`](./cost-analysis.md)**；这里只留结论。

### 2.1 三个口径级事实

先钉死一条结构事实：**reasoning token 从不进会话历史。**
`adapters/model-openai-compatible/src/lib.rs:1137` 的 `finish_with_logger` 只把 `text` 与
`tool_calls` 装进 `ModelResponse.message`；`reasoning` 只 emit `ThinkingDelta` 事件，
`ContentBlock`（`crates/ys-core/src/message.rs:20`）里**没有思考变体**。
⇒ 关思考**只省本轮的 output 与 gen 时间，一个后续请求的 input 都不会省**。

但「不省 input」不等于「省得少」—— 输出按 **4 元/百万**计价，输入未命中 1 元、缓存命中 0.02 元。
**输出单价是缓存命中的 200 倍**，所以 output 在金额上**不是小头**。

#### 实测拆解（`scripts/cost-audit.py`，零 API 调用）

DeepSeek 会话 23 个 / ≈2,550 轮，闲置时段单价（完整版见 [`cost-analysis.md`](./cost-analysis.md) §0）：

| 成本构成 | 单价 | 占比 |
|---|---:|---:|
| 输入 · 缓存命中 | 0.02 | **35.7%** |
| 输入 · 未命中 | 1.00 | **29.8%** |
| 输出 · **reasoning** | 4.00 | **18.3%** |
| 输出 · 工具调用参数 | 4.00 | 15.5% |
| 输出 · 可见正文 | 4.00 | **1.8%** |

（价目出处：<https://api-docs.deepseek.com/zh-cn/quick_start/pricing>，闲置时段；高峰全部 ×2，比例不变。）

#### 三条由此确立的结论

1. **关思考的收益是 18.3%（DeepSeek），不是 −69% 也不是 −3%**。
   两个错误口径都出现过（见 [`cost-analysis.md`](./cost-analysis.md) §2）：
   `69%` 是 reasoning 占 **output** 的 token 比例；`−3~5%` 则把 output 当成了小头。
   **按金额，output 占 33.8%。**
2. **「削减叙述」不值得做 —— 只值 1.8%**。这才是 `prompt.rs:288` 那条 guideline 的真正定价：
   即使把全部可见正文压到零，也只省 0.38 元。历史上那「37 段 / 16,916 字符」的成本量级
   远小于直觉。⇒ **不要为省叙述去动提示词（更不该为此破一次缓存前缀）。**
3. **真正被忽略的第二项是工具调用参数（15.5%）**，比全部可见正文贵 **8.6 倍**。
   它也按输出价计费，且随调用次数线性增长 ⇒ 与「轮数/调用数是乘数」的既有结论同向。

#### 按模型差异极大

| 模型 | reasoning / output | 关思考可省（按其自家价） |
|---|---:|---:|
| `deepseek-v4.1-flash`（**唯一标准**） | 53.1% | **18.3%** |
| `MiniMax-M3.1-Flash` | 29.0% | 5.0% |
| `minimax-m3.1-flash-free` | 91.2% | 63.9% |

⇒ **不要把某一家测出的比例推广到所有 provider**，也不要把 MiniMax 的推理价套 DeepSeek。
**本设计的验收一律以 DeepSeek 为准，其余仅作参照、不纳入决策。**

> ⚠️ `minimax` 的 `supports_stream_usage = false` ⇒ 它的部分会话在 `round_usage` 里**没有读数**。
> 审计脚本把它们显式列成「无读数」，绝不推成 0（与 `Usage::reasoning_tokens` 的既有语义一致）。


### 2.2 前缀与缓存

`thinking` / `reasoning_effort` 都是**请求体顶层字段**，不参与 `messages` / `system` / `tools`
的序列化前缀（`context.md` 约束 5）。⇒ **改思考开关不破前缀缓存**，这是它相对「改 prompt 文案」
的决定性优势。但这条**必须用 wire 字节断言确认**，不能只靠推理（见 §6 验收 A1）。

### 2.3 能力位：第一优选链路上现在是 **false**，必须先打开

`ProviderCompat::supports_thinking_control` 默认 `false`，**只有 `ProviderCompat::deepseek()` 为 `true`**
（`compat.rs:69`）。本仓第一优选模型是 `deepseek-*`，但它实际经 **`opencode-go` 网关**接入
（`auth.json` 的 `opencode-go` → `https://opencode.ai/zen/go/v1`，模型 `deepseek-v4.1-flash`），
而 `ProviderCompat::opencode_go()`（`compat.rs:94`）**没有**置该位。

⇒ **不改这一位，本设计的全部收益为 0。**

#### 已实测：opencode-go 接受并**真的执行**该字段

直接打网关（`POST /chat/completions`，同一条 prompt，各 n=3，`max_tokens=200`）：

| 请求体 | reasoning_tokens | completion_tokens |
|---|---:|---:|
| `thinking:{"type":"disabled"}` | **0 / 0 / 0** | 7 / 7 / 6 |
| 不发送（= 今天的行为） | 49 / 54 / 42 | 57 / 61 / 49 |

⇒ **reasoning −100%、completion −88%，且正文答案相同**（「1+1 等于 2」）。
`thinking:{"type":"enabled"}` 也回 200（但那是默认值，本仓从不发，见 `9b573e4` 的取舍）。

> ⚠️ **这是探测级证据，不是产品级验收**：n=3、单轮短任务、无工具调用，证明的只是
> 「字段不被拒且开关生效」。**质量与轮数必须按 §6.2 实跑 A/B。**
> 另：同样经网关的 `minimax-m3.1-flash-free` 不一定接受该字段（未测），
> 能力位仍要按 provider 收紧，不能因为 deepseek 能用就全开。

#### 网关额外给了计价读数

opencode-go 的响应里带 `usage.opencode.billing_source` / `list_cost` / `allowance`，
且 `usage.cost` 字段存在。这是一条比官方价目表**更准**的本地读数来源 —— 但它是网关自己的
计价（与本仓 `--price-*` 假设价不同），对账时须标清口径。


## 3. 候选方案

三个方向各自独立生成（最小复杂度 / 可扩展优先 / 收益优先），下面按「改动面」重排。

### 3.1 方案 A · 轮次衰减（阈值写在循环里）

在 `basic.rs` 加常量 `THINKING_OFF_AFTER_ROUNDS`，`rounds > 阈值` 时请求带 `disable_thinking: Some(false)`。

| 优点 | 缺点 |
|---|---|
| 零新字段、零新类型，约 35 行 | 阈值是**行为学近似**，不是可判定状态：`Steer` 插话后模型已回到需推理状态而计数继续 ⇒ 会误关 |
| 子 agent 自动继承（同一 `run_turn`） | 无法表达「子 agent 从第 1 轮就关」——它与主轮共用同一阈值，**语义不同却被迫同值** |
| 不破前缀缓存 | 阈值无数据支撑；且**按轮序号**切分与「任务阶段」只是碰巧相关 |

**结论：作为最小前置可接受，但阈值语义错。** 尤其是「子 agent 起手就该关思考」这条
（它是检索-汇报型，与工具摘要同构）无法用轮次表达。

### 3.2 方案 B · `ThinkingPolicy` + 策略表（数据驱动）

新增 `CallKind` 枚举（Main / ToolSummarize / Compact / Subagent）与 `ThinkingPolicy`
（`effort` / `thinking` / `max_tokens` 三个可选覆盖），app 层填一张
`BTreeMap<CallKind, ThinkingPolicy>`，循环只打标签、适配器解析成 wire。

| 优点 | 缺点 |
|---|---|
| 加新调用类型不改循环核心；顺带把 `disable_thinking` 与冻在实例里的 effort 收敛成一个概念 | 当前只有 3 类调用（压缩已否决降档）⇒ **4 个槽位里 3 个恒为 `None`** |
| 用户可配、可 A/B、可回退 | 净增 ~80 行 + 3 个新名词；`BTreeMap` 这层间接在当前需求下**不产生任何行为差异** |
| 不破 `ModelRequest` 序列化契约（`#[serde(skip)]` 沿用） | 方案作者自己的诚实判断：「若未来只有 3 类调用，这套表**不值**」 |

**结论：买类型形状，不买映射表。** `Option<ThinkingPolicy>` 这个边信道形状值得要；
`BTreeMap<CallKind, …>` 与配置项等第二个真实需求再长出来。

### 3.3 方案 C · 多杠杆并列（关思考 + 工具收窄 + skill 目录 + 叙述降级）

| 杠杆 | 收益 | 判定 |
|---|---|---|
| L1 主轮关思考 | 墙钟 ≈ −10~15%、output ≈ −50% | **采纳**（§2.1 已修正口径） |
| L2 工具结果收窄（`window/4` → `window/8`、`read` 默认收窄） | 字节 −58%（离线实测）；墙钟**已实测≈极小**（压缩次数 6.3→4.8） | **本轮不采纳**：仓库自己已判「不计入合计」，需先实跑 |
| L3 skill 目录段重定价 | 每轮 −10 k token，但缓存命中后金钱 ≈0.1×，TTFT 由排队主导 | **本轮不采纳**：ADR-0019 是有意交易，且墙钟证据互相冲突 |
| L4 叙述降级（更早叙述压成指针） | 极端会话 ≈ −11% 输入（估计），占累计输入 <1% | **本轮不采纳**：会破一次前缀缓存，收益与代价不成比例 |

**结论：C 的杠杆清单是本次的收益地图，但它的可采纳子集只有 L1。** 其余三项都有仓库
自己写下的「证据不足，不得计入」的边界，硬推等于把噪声当收益。

## 4. 推荐方案

**取 A 的落地形态 + B 的类型形状：请求级 `thinking` 覆盖 + 按调用面设值，默认与今天逐字节相同。**

### 4.1 核心决定

**前置（P0）：给 `opencode_go()` 置 `supports_thinking_control = true`。**

没有这一步，后面全部是空操作（§2.3）。这是**独立的一行改动 + 一个测试**，
但它会让**立即生效**的行为发生变化：今天所有经 opencode-go 的调用都不发该字段，
置位后它们发不发取决于 `config.disable_thinking`（仍为 `false` by default）⇒
**只要不配 `/effort off`，行为仍然逐字节不变**。这条要在测试里钉住。

```rust
// crates/ys-core/src/effort.rs（已有 EffortLevel 处，同一拼写来源）
/// 一次调用的思考覆盖。**三个字段都是可选覆盖**，`None` = 不干预。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingOverride {
    /// 开关覆盖。`None` = 不干预（沿用 config 侧 `/effort off` 的全局决定）；
    /// `Some(false)` = 确定关闭思考。
    pub thinking: Option<bool>,
    /// 强度覆盖。`None` = 沿用实例冻结的档位（= 今天的字节）。
    pub effort: Option<String>,
}
```

**不引入 `CallKind` 枚举与策略表。** 调用面只有三处且都在看得见的地方
（`basic.rs` 主轮、`summarize.rs` 摘要、`compact.rs` 压缩），**显式字面量比看不见的查表好**：

| 调用面 | 变更后 | 语义 |
|---|---|---|
| 主轮（`basic.rs` Step 5 组装处） | `thinking: ctx.limits.main_thinking_override()` | 由 `RunLimits` 决定，见 §4.2 |
| 工具摘要（`summarize.rs:93`） | `thinking: Some(ThinkingOverride { thinking: Some(false), .. })` | **等价于今天的 `disable_thinking: Some(false)`** |
| 压缩（`compact.rs`） | `None` | 现状 |

`ModelRequest.disable_thinking: Option<bool>` 改为 `thinking: Option<ThinkingOverride>`
（`#[serde(skip, default)]` 不变 ⇒ 序列化字节逐字节不变）。**不并存旧字段** ——
并存会留下两个语义重叠的字段（`disable_thinking: Some(false)` 与
`thinking: Some(ThinkingOverride { thinking: Some(true) })` 冲突时谁赢？无从回答）。

> ⚠️ 机械改动面：`summarize.rs:262` 与适配器三处测试（`lib.rs:1384/1436/1465/1503`、
> `tests/retry.rs:269/689`、`apps/coding-agent/tests/*.rs` 四处）的 `disable_thinking:` 字面量
> 需同步改。**编译器会全数报出**，不算风险，但要计入工作量。

### 4.2 主轮的开关由谁定

`RunLimits` 加一个字段（沿用 `#\[non_exhaustive]` + `default()` + 字段赋值的既有惯例）：

```rust
/// 主轮是否关思考。`None` = 不干预（默认，逐字节不变）；`Some(false)` = 关。
///
/// **默认 `None` 是承重默认**：既有消费者（不设该字段）的行为逐字节不变，
/// 与 `fixed_prompt_tokens = 0` / `max_turn_tool_result_tokens = 0` 同一惯例。
pub main_disable_thinking: Option<bool>,
```

**默认值必须是 `None`。** 这是本设计唯一不能动的行为兼容点：把默认设成 `Some(false)`
等于对所有老调用方做一次静默的思考能力削减。

app 侧由配置决定填什么 ——**第一版不暴露配置项**，只在 `resolve_limits` 里读一个可选 env
供 A/B 用（`YUSHAN_MAIN_THINKING=off|on`，未设置 ⇒ `None`）。理由：先拿到 A/B 读数，
再决定的是「默认值」，而不是「机制」。

### 4.3 为什么不做「按轮序号衰减」

方案 A 的阈值语义是错的（§3.1）。**若 A/B 证明主轮全程关思考会掉质量**，
再引入「前 N 轮开、之后关」时，它应该长成**调用点的一行判断**，而不是 `RunLimits` 里的新维度：

```rust
// 只有真的需要时才加，且理由必须带上 A/B 读数
thinking: round_is_early.then(|| main_override()).flatten()
```

把两种语义（「全程关」与「后段关」）都塞进 `RunLimits` 会让字段互相影响，
而 A/B 还没给出「哪个对」的答案。

## 5. 数据流

```
/effort off（会话）或 YUSHAN_REASONING_EFFORT=off（配置）
  └─► Config::disable_thinking() == true ──► OpenAICompatibleConfig.disable_thinking（构造期）
                                                   │
                                                   ├─（取或）──► thinking: {"type":"disabled"}
YUSHAN_MAIN_THINKING=off（A/B 用）                 │
  └─► resolve_limits → RunLimits.main_disable_thinking = Some(false)
        └─► basic.rs Step 5: ModelRequest.thinking = Some(ThinkingOverride{ thinking: Some(false) })
              └─► translate_request：（config ∨ request）∧ supports_thinking_control
                    └─► 只有 deepseek 方言发出该字段；其余 provider 静默不发
```

三个不变量：

1. **`config` 侧与 `request` 侧取或**，保持今天的语义（`/effort off` 必须能压过任何面的设置）。
2. **能力位门控不能绕过**：所有新值照旧穿过 `supports_thinking_control`，
   GLM「始终思考」不会因为这次改动收到它不认识的字段。
3. **`None` 与 `Some(false)` 语义不同**，不得合并（这是 `9b573e4` 立下的规矩）。

## 6. 验收

### 6.1 测试矩阵（对齐 `docs/design.md` §12 与既有惯例）

| # | 层 | 用例 | 变异（必须让它变红） |
|---|---|---|---|
| A1 | adapter | **wire 字节断言**：`thinking: Some(ThinkingOverride{thinking: Some(false)})` ⇒ 请求体含 `{"type":"disabled"}`；`None` ⇒ 该键**完全不出现** | 把 `skip_serializing_if` 去掉 / 把 `None` 当关闭 |
| A2 | adapter | **前缀相干性**：同一 messages + 两种 `thinking` 值 ⇒ 序列化后的 `messages`/`system`/`tools` **逐字节相同** | 让 `thinking` 影响任何前缀字段 |
| A3 | adapter | 能力位门控：`standard()` / `glm()` 即使配置要求也不发 | 去掉 `supports_thinking_control` |
| A4 | ys-component | `main_disable_thinking` 默认 `None`；可显式设 `Some(false)` | 默认值改成 `Some(false)` |
| A5 | ys-loop | 主轮请求的 `thinking` 来自 `RunLimits`；压缩仍为 `None`；摘要仍为「关」 | 把压缩也接上 / 把默认值接错 |
| A6 | ys-loop | 既有 `summarize` 断言（`:262`）改写后仍钉住「摘要关思考」 | 改成 `None` |

### 6.2 实跑 A/B（分层：先用零成本与最便宜的判据把不成立的分支砍掉）

⚠️ **不要一上来就 `fact/1` × 3 次 × 2 臂** —— 那是本仓最贵的基准，而它要回答的问题
大多可以先被更便宜的层砍掉。

**L0 · 零成本（必做，先做）**

```bash
python3 scripts/cost-audit.py --model deepseek    # 当前 provider 上关思考能省多少
```

回答「你这个 provider 上值不值」，**零 API 调用**。实测：DeepSeek 18.4%，MiniMax 5.0%。
**若 <10%，不建议为此开一个开关注入点** —— 先去打工具调用参数（15.4%）。

**L1 · 单次质量冒烟（约 1 分钟）**

用 `daily/2`（「找出 `is_concurrency_safe` 的所有实现与调用点」）—— 答案可**机械核对**
（当前 22 处 / 11 个文件），而它只需 2–3 轮，比 `fact/1` 的 11 轮便宜一个量级：

```bash
cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture --
# 默认集里就包含 daily/2；两臂各 1 次，不是 3 次
```

它是**冒烟闸门**：一次就掉质量（漏报明显或编造行号）⇒ 直接否决，不必花 3 次的钱。

**L2 · 仅在 L1 通过后**，才跑 `fact/1`；`REPEATS=2` 起步（不是 3）。

```bash
YUSHAN_BENCH_ONLY=fact/1 YUSHAN_BENCH_REPEATS=2 \
  cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture
```

**判「基本效果保持」**（三条件全中）：

1. 全部 `Quality::Pass`（`min_citations: 6`，且无引用指向不存在的位置 ⇒ 无 `Partial`）。
   **`Partial` 即退化** —— 编造引用是关思考最典型的失败形态。
2. 轮数中位 **≤ 基线 +1**。
3. `retries = 0` 且 `tools_ms` 同量级（排除归因混淆）。

**判「可判」**：基线本身跨 64.4–201.4 s（3× 跨度，`ADR-0042`），
**中位差 <20% 一律记为「不可判」，不得写成收益**。

> **为什么不用 `t4`**：已从基准移除 —— 291–324 s、轮数 33–246 摆动、无可判定完成标准。

### 6.3 变异测试

`AGENTS.md` 要求关键逻辑做变异测试。本次至少三处：
去掉能力位门控、把 `None` 当关闭、把 `main_disable_thinking` 默认值改成 `Some(false)`。

## 7. 实施顺序

| 步 | 内容 | 单独提交？ | 理由 |
|---|---|---|---|
| P0 | **审计基线**：`python3 scripts/cost-audit.py`（零 API 调用） | — | 当前的真实金额构成：关思考 18.3%、工具参数 15.5%、正文 1.8% |
| P1 | `opencode_go()` 置 `supports_thinking_control = true` + 测试 | ✅ **已落地** | 前置：不改这一位，全部收益为 0（§2.3） |
| P2 | `ThinkingOverride` 泛化 + 调用面接线（默认逐字节不变） | ✅ | 不破前缀、不改行为 |
| P3 | `RunLimits.main_disable_thinking` + `YUSHAN_MAIN_THINKING` | ✅ | 机制与策略分开，便于回退 |
| P4 | L1 冒烟（`daily/2`，两臂各 1 次）→ 过了才跑 L2 | — | 便宜的先跑 |
| P5 | 按读数决定**默认值**是否改 | — | 默认值必须是数据决定的 |

每一步跑 `./scripts/par-test.sh`（全量测试入口，不是裸 `cargo test`）。

> **P1 的验收不能只靠探测**：§2.3 的 n=3 只证明了「字段不被拒且开关生效」。
> P1 的单元测试要钉的是**本仓自己的接线**：能力位置位后，`/effort off` 能落到 wire；
> **不配 `off` 时请求体仍然没有 `thinking` 键**（逐字节不变）。

## 8. 风险

| # | 风险 | 严重度 | 缓解 |
|---|---|---|---|
| S1 | **关思考 → 轮数上升**，把 −15% 变成正收益 | **高** | 轮数中位 +1 作为硬闸门（§6.2）；`low` 已有质量 2/3 的前车之鉴 |
| S2 | **收益被高估**：reasoning 不进历史，关它不省 input | **高** | §2.1 已写进口径；任何「token −69%」的说法都是错的 |
| S3 | 基线噪声大于杠杆收益 | 中 | 中位差 <20% 记为不可判 |
| S4 | `fact/1` 是检索汇型，对它安全推不出「改代码类任务安全」 | 中 | 补一次真实改代码会话的人工 diff 复核；`YUSHAN_BENCH_HEAVY=1` 长跑项 |
| S5 | 机械改动面（~8 处字面量）漏改 | 低 | 编译器全数报出 |
| S6 | GLM 等 provider 收不到收益却被误认为「已优化」 | 低 | S6 场景写进 `docs/TODO.md` 的已知边界 |

## 9. 被否决的方案（留档，防后人重提）

- **A · 纯轮次衰减**：阈值语义错（`Steer` 后误关），且无法表达「子 agent 起手就关」。可作为 P4 的一行补充，不单独成立。
- **B · `CallKind` + `BTreeMap` 策略表 + 用户配置项**：当前 4 个槽位 3 个恒 `None`，间接层不产生行为差异。买类型形状即可，表等第二个真实需求。
- **C 的 L2（工具收窄）/ L3（skill 目录段）/ L4（叙述降级）**：仓库自己已写下「证据不足，不得计入合计」（`session-latency/design.md` §1.1 诚实边界、`cost-model.md` §9/§10）。硬推等于把噪声当收益。
- **不动 `Model::complete` 签名**：会波及 `mock.rs` 与跨动态库边界（`AGENTS.md` 禁止跨边界传 trait object），且要求每个 `Model` 实现认识编排词汇 —— 方向错了。

## 10. 与既有决策的关系

| 既有决策 | 本设计的关系 |
|---|---|
| `9b573e4`（`disable_thinking: Option<bool>` + `None`≠`false`） | **泛化**，不推翻。语义原样保留，只是从 `bool` 变成结构的一个字段 |
| `e269f82`（`/effort off` → `thinking.disabled`） | 不变，仍是全局一刀切，且优先级最高 |
| ADR-0042（默认 `high`） | 不变。本设计只动开关，不动强度默认值 |
| `docs/TODO.md:160`（压缩不降档） | 遵守，压缩保持 `None` |
| `docs/TODO.md:155`（按调用类型分发的待办） | **本设计是其最小落地**：补上了 `ModelRequest` 边信道，但**没有**建策略表。待办应改写为「策略表按需再加」 |
