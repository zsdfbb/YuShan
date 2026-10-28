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

**前置（P1，已落地）：给 `opencode_go()` 置 `supports_thinking_control = true`。**

没有这一步，后面全部是空操作（§2.3）。它是独立的一行改动 + 两个测试，
而它**不改变默认行为**：置位后请求体是否带 `thinking` 仍取决于配置层
`off` 与调用层覆盖（两源默认都是「不干预」）。这条已用测试钉住，并经真实链路核实。

**主决定：请求级 `ThinkingOverride`，不建策略表。**

```rust
// crates/ys-core/src/effort.rs（已有 EffortLevel 处，同一拼写来源）
/// 一次调用的思考覆盖。**两个字段都是可选覆盖**，`None` = 不干预。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingOverride {
    /// 开关覆盖。`None` = 不干预（沿用 config 侧 `/effort off` 的全局决定）；
    /// `Some(false)` = 确定关闭思考。
    pub thinking: Option<bool>,
    /// 强度覆盖。`None` = 沿用实例冻结的档位（= 改动前的字节）。
    pub effort: Option<String>,
}
```

**不引入 `CallKind` 枚举与策略表。** 调用面只有三处且都在看得见的地方，
**显式字面量比看不见的查表好**：

| 调用面 | 变更后 | 语义 |
|---|---|---|
| 主轮（`basic.rs` Step 5 组装处） | 由 `RunLimits::main_disable_thinking` 派生 | 见 §4.2 |
| 工具摘要（`summarize.rs`） | `Some(ThinkingOverride::off())` | **等价于改动前的 `disable_thinking: Some(false)`** |
| 压缩（`compact.rs`） | `None` | 维持开思考 |

`ModelRequest.disable_thinking: Option<bool>` 已改为 `thinking: Option<ThinkingOverride>`
（`#[serde(skip, default)]` 不变 ⇒ 序列化字节逐字节不变）。**不并存旧字段** ——
并存会留下两个语义重叠的字段，冲突时谁赢无从回答。

> ⚠️ **强度覆盖的门控位置不对称**（本次的一个易错点）：
> config 侧的 `reasoning_effort` 在实例构造时**已由 app 门控过**，适配器须**原样透传**
> （再调一次 `effort_for` 会改变既有字节，曾被一条既有测试抓到）；
> 而请求侧的覆盖值 app 看不到，**必须在适配器里**穿过 `effort_for` 的
> 能力位 + 模型级代际 + 方言折叠，否则 GLM 之类会把不认识的档位当 400 拒。

### 4.2 主轮的开关由谁定

`RunLimits` 新增字段（沿用 `#[non_exhaustive]` + `default()` + 字段赋值的惯例）：

```rust
/// 主轮是否关思考。`None` = 不干预（默认，逐字节不变）；`Some(false)` = 关。
pub main_disable_thinking: Option<bool>,
```

**默认值必须是 `None`。** 这是本设计唯一不能动的行为兼容点：设成 `Some(false)`
等于对所有老调用方做一次静默的思考能力削减。

app 侧只读一个可选 env（`YUSHAN_MAIN_THINKING=off`），未设 ⇒ `None`。
第一版**不暴露配置项**：先拿 A/B 读数，再决定「默认值」与「要不要进 `config.json`」。
`on` 刻意**不是**有效值 —— provider 默认即开，主动表达「开」只给不认识该字段的端点添风险。

### 4.3 实测 A/B（2026-10-28，DeepSeek 经 opencode-go，真实链路）

| 判据 | 任务 | 基线（默认思考） | 关思考 | 结论 |
|---|---|---|---|---|
| 质量位 | `daily/2` | **Q+** | **Q+** | 不退化 |
| 质量位 | `fact/1` | **Q+**（2/2 次） | **Q+**（2/3 次，一次 Q~） | **有方差，见下** |
| 轮数 | `daily/2` | 10 | **5** | 未增加，反而减半 |
| 轮数 | `fact/1` | 12 / 10 | 18 / 8 / 10 | 双向摆动，无方向性 |
| 墙钟 | `daily/2` | 43.3 s | **27.2 s（−37%）** | 改善 |
| 墙钟 | `fact/1` | 106.8 s / 90.9 s | 63.0 s / 53.8 s / 42.0 s | 改善 |
| 成本 | 短检索任务 | 0.0276 元 | 0.0279 元 | **+1.1%（无收益）** |
| 成本 | 推理密集型 | 0.0575 元 | **0.0388 元（−32.4%）** | 显著改善 |

**两条必须记住的结论**：

1. **收益高度依赖任务形态**。短检索任务上关思考**不省钱**（−100% reasoning 被
   `tool_args` 与 input 抵消）；推理密集型才吃掉那 18.3% 的大头。
2. **`fact/1` 有一次 `Q~`**（有引用指向不存在的位置）。3 次里 1 次 ⇒
   **说明质量余量不为零，不能直接改默认值**。这也是「先用便宜判据、再上 `fact/1`」
   的分层验收的价值：`daily/2` 三次都是 Q+，但只有 `fact/1` 暴露了这条方差。

> ⚠️ 基线本身跨 53–107 s（2× 跨度）。按本设计定的「中位差 <20% 记为不可判」，
> **墙钟收益在这批样本里已可判（−37%）**；但 `fact/1` 的 `Q~` 意味着**质量判据尚未通过**。

## 5. 数据流

```
/effort off（会话）或 YUSHAN_REASONING_EFFORT=off（配置）
  └─► Config::disable_thinking() == true ──► OpenAICompatibleConfig.disable_thinking（构造期）
                                                   │
YUSHAN_MAIN_THINKING=off（A/B 用）                │
  └─► resolve_limits → RunLimits.main_disable_thinking = Some(true)
        └─► basic.rs Step 5: ModelRequest.thinking = Some(ThinkingOverride::off())
              │                                  │
              └──────────── 两者取或 ────────────┘
                              ▼
              translate_request：（config ∨ request）∧ supports_thinking_control
                              ▼
              只有 deepseek / opencode-go 两家方言发出该字段；其余静默不发
```

三个不变量：

1. **`config` 侧与 `request` 侧取或**：`/effort off` 是用户的**全局意图**，不能被任何单个
   调用面推翻；反之单个调用面要求关时也必须生效。
2. **能力位门控不能绕过**：所有新值照旧穿过 `supports_thinking_control`，
   GLM「始终思考」不会因为这次改动收到它不认识的字段。
3. **`None` 与 `Some(false)` 语义不同**，不得合并（这是 `9b573e4` 立下的规矩）。

## 6. 验收

### 6.1 测试矩阵（全部已落地）

| # | 层 | 用例 | 变异（已验证会让它变红） |
|---|---|---|---|
| A1 | ys-core | `thinking_override_default_is_no_intervention`：`Default` 全 `None`；`off()` 才是关；`Some(true)` 不算关 | `Default` 改成关闭 |
| A2 | adapter | `thinking_control_is_gated_and_optional_on_the_wire`：`None` ⇒ 键不出现；`off()` + 支持 ⇒ `disabled`；不支持 ⇒ 不发 | `None` 当关闭 / 去掉能力位门控 / `disabled` 写成 `off` |
| A3 | adapter | `enabling_opencode_go_does_not_change_the_default_request_body`：能力位置位但未要求关 ⇒ 请求体不变；配 `off` 才落地 | 能力位开即无条件关 / 只看 request 侧 |
| A4 | adapter | `opencode_go_and_deepseek_are_the_only_thinking_control_vendors`（含**反向断言**） | 删 `opencode_go` 的能力位 |
| A5 | adapter | `request_level_effort_override_is_gated_but_config_side_passes_through` | 请求侧绕过门控 / config 侧也门控 |
| A6 | ys-component | `main_disable_thinking` 默认 `None` | 默认值改成 `Some(false)` |
| A7 | ys-loop | `main_round_thinking_override_follows_limits`：`None`/`Some(true)`/`Some(false)` 三种设置下发到请求的内容 | 无条件关 / `Some(false)` 语义反转 |
| A8 | ys-loop | 既有 `summarize` 断言仍钉住「摘要关思考」 | 改成 `None` |
| A9 | app | `resolve_limits_reads_main_thinking_env`：只有 `off` 生效，`on`/`high`/`""` 都不生效 | 改默认值 / 改匹配臂 |

> **A2 的「前缀相干性」用结构化断言代替了字节对比**：`thinking` 是 `#[serde(skip)]`，
> 从不进序列化 ⇒ 前缀逐字节不变是由**类型契约**保证的，不需要再跑一次序列化比较。

### 6.2 实跑 A/B（分层：先用零成本与最便宜的判据把不成立的分支砍掉）

⚠️ **不要一上来就 `fact/1` × 3 次 × 2 臂** —— 那是本仓最贵的基准，而它要回答的问题
大多可以先被更便宜的层砍掉。

**L0 · 零成本（必做，先做）**

```bash
python3 scripts/cost-audit.py    # 当前 provider 上关思考能省多少（默认只统计 deepseek-*）
```

回答「你这个 provider 上值不值」，**零 API 调用**。实测：DeepSeek 18.3%，MiniMax 5.0%。
**若 <10%，不建议为此开一个开关注入点** —— 先去打工具调用参数（15.5%）。

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

## 7. 实施顺序（全部已落地，2026-10-28）

| 步 | 内容 | 状态 | 验证 |
|---|---|---|---|
| P0 | **审计基线**：`python3 scripts/cost-audit.py`（零 API 调用） | ✅ | 关思考 18.3% / 工具参数 15.5% / 正文 1.8% |
| P1 | `opencode_go()` 置 `supports_thinking_control = true` | ✅ | 2 个新测试；3 处变异被抓；真实链路核实 |
| P2 | `ThinkingOverride` 泛化 + 三处调用面接线 | ✅ | 请求级强度门控不对称用一个新测试钉住 |
| P3 | `RunLimits.main_disable_thinking` + `YUSHAN_MAIN_THINKING` | ✅ | 主轮探针测试（3 种设置）；env 测试（5 个输入） |
| P4 | A/B 实跑（`daily/2` + `fact/1`） | ✅ | 见 §4.3 |
| P5 | 按读数决定**默认值** | ⬜ **未改** | §4.3 的 `Q~` ⇒ **暂不改默认值** |

每一步跑 `./scripts/par-test.sh`（全量测试入口，不是裸 `cargo test`）。

**P5 的结论是「先不改」**。理由不是收益不够（墙钟 −37%、推理密集任务成本 −32% 都成立），
而是 **`fact/1` 出现过一次 `Q~`** ⇒ 质量余量不为零，而默认值是**面向所有人**的。
当前形态（env 显式开启）已经能让愿意接受该风险的用户拿到收益，且随时可关。

> **变异测试记录**（本次新增断言全部验证过）：
> 删能力位（2 用例红）/ 能力位开启即无条件关思考（3 用例红）/ 去掉能力位门控（2 用例红）/
> 主轮无条件关思考（1 红）/ `Some(false)` 语义反转（1 红）/ 请求级强度绕过门控（1 红）。

## 8. 风险（按实测更新）

| # | 风险 | 严重度 | 实测状态 |
|---|---|---|---|
| S1 | 关思考 → 轮数上升，把收益吃掉 | 中 | **未发生**：`daily/2` 10→5 轮；`fact/1` 双向摆动（12/10 vs 18/8/10），无方向性 |
| S2 | 收益被高估（reasoning 不进历史） | 高 | **已量化**：短检索 +1.1%，推理密集 −32.4%。**按任务形态分化** |
| S3 | 基线噪声大于收益 | 中 | 基线跨 53–107 s（2×）。墙钟 −37% 仍可判 |
| S4 | `fact/1` 是检索汇型，推不出「改代码类任务安全」 | **高** | **已实证**：3 次里 1 次 `Q~` ⇒ 见 S7 |
| S5 | 机械改动面漏改 | 低 | 已消除（编译器全数报出，已改完） |
| S6 | 非 DeepSeek provider 收不到收益却被误认为「已优化」 | 低 | 能力位只对两家方言开；有反向断言 |
| **S7** | **质量余量不为零**：`Q~` 意味着关思考会产出「指向不存在位置」的引用 | **高** | **未缓解，仅靠默认关闭**。这是 P5 不改默认值的唯一理由 |

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
