# effort-per-call-type — 成本分析（DeepSeek，2026-10-28）

> 性质：**实证**。本文件只记录**测出来的数**与口径，不改设计。
> 设计见 [`design.md`](./design.md)，决策见 [`../../adr/0060-thinking-policy-per-call-kind.md`](../../adr/0060-thinking-policy-per-call-kind.md)。
> 复现：`python3 scripts/cost-audit.py`（**零 API 调用**）。
>
> 本文件存在的理由：token 数量与成本**不是一回事**。本需求从提出到定稿，
> 收益估算被推翻过**两次**，两次都是因为混用了「token 口径」与「金额口径」。
> 把正确的口径与来源钉在这里，避免第三次。
>
> ⚠️ **数字是快照**：样本是本机 `~/.yushan/` 下**当时已有**的日志，会随使用而增长。
> 引用时**必须区分「比例」与「绝对值」** —— **各构成的比例**是稳定的结论；
> **token 与元**会变。要当前值就重跑脚本（§8），不要引用本文的绝对值当事实。`

## 0. 结论速览

**唯一优化标准是 DeepSeek**（第一优选模型）。

| 成本构成 | token | 单价（元/M） | 元 | 占比 |
|---|---:|---:|---:|---:|
| 输入 · 缓存命中 | 375,295,041 | 0.02 | 7.51 | **35.7%** |
| 输入 · 未命中 | 6,265,683 | 1.00 | 6.27 | **29.8%** |
| 输出 · reasoning | 961,695 | 4.00 | 3.85 | **18.3%** |
| 输出 · 工具调用参数 | 816,672 | 4.00 | 3.27 | **15.5%** |
| 输出 · 可见正文 | 94,758 | 4.00 | 0.38 | **1.8%** |
| **合计** | | | **21.03** | 100% |

样本：**23 个会话 / ≈2,550 轮**，全部 `deepseek-v4.1-flash`（经 `opencode-go` 网关），
闲置时段价。缓存命中率 **98.4%**，reasoning 占 output **53.1%**。
（采集于 2026-10-28；绝对值会随使用增长，**比例是稳定结论**。）

**三条可操作的结论**：

1. **关思考可省 18.3%** —— 单项最大的可控杠杆。
2. **少调工具可省至多 15.5%** —— 工具调用参数按输出价计费，随调用次数线性增长。
3. **削减叙述只值 1.8%** —— **不值得**为它动提示词，更不值得为此破一次缓存前缀。

## 1. 口径与来源

### 1.1 单价

<https://api-docs.deepseek.com/zh-cn/quick_start/pricing>（`deepseek-flash`）：

| | 闲置时段 | 高峰时段 |
|---|---:|---:|
| 输入 · 缓存命中 | 0.02 | 0.04 |
| 输入 · 缓存未命中 | 1.00 | 2.00 |
| 输出 | 4.00 | 8.00 |

单位：元 / 百万 token。高峰时段（北京时间周一至周五 9:00–12:00、14:00–18:00）
**全部 ×2，各构成比例不变**。本文按**闲置时段**计（用户实际使用时段）。

**关键比值**：输出单价是缓存命中的 **200 倍**，是未命中的 **4 倍**。
未命中单价是缓存命中的 **50 倍**。

### 1.2 数据源

| 来源 | 用途 | 局限 |
|---|---|---|
| `~/.yushan/logs/debug-*.jsonl` 的 `round_usage` | token 读数（provider 上报） | provider 未上报的会话无读数，须显式排除，**不得当 0** |
| `~/.yushan/sessions/*.jsonl` | 把 output 再拆成 reasoning / 工具参数 / 正文 | 按 `chars/3` 估算，只用于**排序杠杆**，不是账单 |

`reasoning_tokens` **已包含在 `output_tokens` 内**，不重复计费
（与 `docs/arch/explore-speed/codewhale-cost-research.md:202` 记录的口径一致）。

### 1.3 为什么必须按金额

同一个 `reasoning 占 output 53.1%` 的数字，按 token 口径像是「大头」，按金额是 **18.3%**；
而 `可见正文占 content 12%`，按金额只有 **1.8%**。**倍率差 200 倍时，体量排序完全失效。**

## 2. 两次被推翻的收益估算（留档）

| # | 当时的说法 | 为什么错 | 正确值 |
|---|---|---|---|
| ① | 「reasoning 占 output 69% ⇒ token −69%」 | 把 **output 内部**的构成当成总量构成 | 按金额 −18.3% |
| ② | 「reasoning 不进历史 ⇒ 只省小头，−3~5%」 | 修正 ① 时把 output 当成了小头，忽略了 **200 倍计价倍率** | 按金额 −18.3% |

两次都错在**同一个地方**：拿 token 数量代替成本。这正是本文件存在的原因。

## 3. 结构事实：reasoning 不进会话历史

`adapters/model-openai-compatible/src/lib.rs:1137` 的 `finish_with_logger` 只把
`text` 与 `tool_calls` 装进 `ModelResponse.message`；`reasoning` 只 emit
`ThinkingDelta` 事件。`ContentBlock`（`crates/ys-core/src/message.rs:20`）**没有思考变体**。

⇒ **关思考不省任何一个后续请求的 input**，只省本轮 output 与本轮生成时间。

这不构成「省得少」的理由（输出按 4 元计），但**构成一个重要的边界**：
想省 input（占 65.5%）只能靠「减总量 / 提缓存命中 / 少调工具」，与思考开关无关。

## 4. 缓存：98.4% 命中，input 侧已无空间

| | 值 |
|---|---:|
| 输入总量 | 381,560,724 |
| 其中缓存命中 | 375,295,041（**98.4%**） |
| 未命中 | 6,265,683 |

命中部分按 1/50 计价，但体量是未命中的 60 倍 ⇒ **仍占 35.7%**。
⇒ 两个推论：

1. **不该为「省输入」去动前缀**：命中率已经 98.4%，没有空间；而且**一旦破前缀，
   未命中那 29.8% 会增长**（未命中单价是命中的 50 倍）。
2. **任何改 `system` / `tools` / 早期历史的改动都要单独提交、单独计价**，
   不能与别的杠杆混跑（`docs/arch/cost-optimization/context.md` 的约束）。

> 与 `docs/arch/session-latency/design.md:67`「命中率已 91%，成本不是瓶颈」的关系：
> 那条结论针对**延迟**，不是金额。按金额，input 两侧合计仍是 65.5%。

## 5. 工具调用参数：被忽略的第二项

| | 量 | 说明 |
|---|---:|---|
| 工具调用参数 | 816,672 tok | 按**输出价**（4 元/M）计费 |
| 占成本 | **15.5%** | |
| 对比：全部可见正文 | 94,758 tok / 1.8% | 工具参数是它的 **8.6 倍** |

参数也是模型生成的 token，与正文同价。⇒「少一次工具调用」同时省下：
一次完整往返的 input（未命中部分）+ 该次调用的参数。

这与既有结论同向：**轮数/调用数是乘数**（同题对照 33 轮 vs 6 轮，
`docs/arch/session-latency/design.md:173`）。

### 6. 前置条件：`opencode-go` 的能力位（**已置位，2026-10-28**）

第一优选模型 `deepseek-v4.1-flash` 经 **`opencode-go` 网关**接入
（`auth.json`：`https://opencode.ai/zen/go/v1`）。该方言原先**没有**
`supports_thinking_control`，已于本轮给 `ProviderCompat::opencode_go()` 置位
（`compat.rs:94`）——**在这之前，第 0 节那 18.3% 的杠杆不可达**。

### 已实测该字段被网关接受且真的生效

直接打网关（`POST /chat/completions`，各 n=3）：

**短问答**（`max_tokens=200`）：

| 请求体 | reasoning_tokens | completion_tokens |
|---|---:|---:|
| `thinking:{"type":"disabled"}` | **0 / 0 / 0** | 7 / 7 / 6 |
| 不发送（= 默认） | 49 / 54 / 42 | 57 / 61 / 49 |

**需要推理的任务**（「说出 3 个 Rust 所有权规则的名称」，`max_tokens=300`）：

| 请求体 | reasoning_tokens | completion_tokens |
|---|---:|---:|
| `disabled` | **0 / 0 / 0** | 141 / 130 / 149 |
| 不发送 | **229 / 98 / 103**（合计 430） | 299 / 148 / 165 |

⇒ reasoning **−100%**，任务越难省得越多。

### 已通过真实 agent 链路验证（不只是 curl）

| 配置 | 日志读数 |
|---|---|
| `YUSHAN_REASONING_EFFORT=off` | `system: opencode-go` / `effort: off` / **reasoning 0** |
| 默认（需推理任务） | **reasoning 67** / output 69 ⇒ **默认行为未变** |

⇒ 能力位置位 **不等于**开始发字段：不配 `off` 时仍照旧思考。
这条已由 `enabling_opencode_go_does_not_change_the_default_request_body`（`lib.rs`）钉住，
变异测试（删能力位 / 无条件发 / 去门控）均被抓。

> ⚠️ **仍是探测级证据，不是产品级验收**：单轮、短任务、**无工具调用**。
> 它证明的只是「字段不被拒且开关生效」。**质量与轮数必须实跑 A/B。**
> 同网关的 `minimax-m3.1-flash-free` 是否接受该字段**未测** ⇒
> 能力位仍按 provider 收紧（有反向断言 `opencode_go_and_deepseek_are_the_only_thinking_control_vendors`）。

### 网关自带计价读数

响应里带 `usage.opencode.billing_source` / `list_cost` / `allowance` 与 `usage.cost`。
这是一条比官方价目表**更准**的本地读数来源，但它是网关自己的计价，
与本文件用 `--price-*` 假设的价目**口径不同**，对账时须标清。

## 7. 未测与不做

| 项 | 状态 | 说明 |
|---|---|---|
| 主轮关思考的**质量**代价 | **未测** | `low` 已实测让 `fact/1` 从 3/3 掉到 2/3（ADR-0042），关思考只会更激进 |
| 主轮关思考的**轮数**影响 | **未测** | 轮数是乘数；若轮数 +1 即可能抵消 18.3% |
| 工具结果收窄（`window/4` → `window/8`） | 未计入 | 压缩次数几乎不变（6.3→4.8，实测）⇒ 墙钟收益极小 |
| skill 目录段重定价 | **不做** | 缓存命中后金钱 ≈0.1×；ADR-0019 是有意交易 |
| 削减叙述 | **不做** | 只值 1.8%，且会破一次前缀缓存 |
| 调 `keep_ratio` | **不做** | 实测无效（0.10→0.03 只多撑 8 轮） |
| per-turn 换模型 | **不做** | 代价远大于换开关 |
| 压缩降档 / 关思考 | **不做** | `docs/TODO.md:160`；压缩结果进会话，错了付后续每轮代价 |

## 8. 复现

```bash
# 当前状态（默认只统计 deepseek-* 模型，闲置时段价）
python3 scripts/cost-audit.py

# 高峰时段（全部 ×2，比例不变）
python3 scripts/cost-audit.py --peak

# 机器可读（A/B 对比用）
python3 scripts/cost-audit.py --json

# 看全量（金额列此时不可信，仅看 token 分布）
python3 scripts/cost-audit.py --provider-all
```

脚本会显式列出「有轮但 provider 未上报 usage」的会话，**绝不当 0**
（与 `Usage::reasoning_tokens` 的 `None` ≠ `Some(0)` 语义一致）。
