# 0061 — 用量口径与归因：`input_tokens` 锚定 provider 总量，非主轮消耗另立结算字段

把 `Usage.input_tokens` 的语义钉死为「provider 回报的 prompt 总量（含缓存命中）」，
缓存明细降为可选桶；同时给 `RunResult` 加一个**兄弟字段**承载压缩消耗，
使 `RunFinished.usage` 的含义保持不变。

## Status

proposed（2026-10-28）。设计见 `docs/arch/usage-observability/design.md`。
**本 ADR 记录了三个并行方案的分歧与最终裁决**，其中 P0-1（口径 A vs B）是最主要的一处。

## 背景

`Usage` 今天有五个字段，但**没有任何一处文档说明 `input_tokens` 含不含缓存**：

```rust
pub input_tokens: u32,                                    // 语义未定义
pub output_tokens: u32,
#[serde(default)] pub prompt_cache_hit_tokens: u32,
#[serde(default)] pub prompt_cache_miss_tokens: u32,
#[serde(default, skip_serializing_if = "Option::is_none")] pub reasoning_tokens: Option<u32>,
```

适配器 `adapters/model-openai-compatible/src/lib.rs:1080` 把 OpenAI 的 `prompt_tokens`
原样填进去，而它**包含**缓存命中（`response.rs:53` 的 `cache_tokens()` 给出
`hit + miss == prompt`）⇒ 现状是「含缓存」。但这条事实在仓里只以代码间接体现，从未被写下。

### 两个具体后果

1. **想算命中率的人在猜分母。** 本仓 4,741 次调用实测：`input_tokens` 合计
   614,443,074，其中 `cache_read` 603,026,301（**98.14%**）。若实现方把分母取成
   `input + cache_read`（把 `input` 误当成「未缓存部分」），会算出 **49.53%** ——
   同一个 prompt 的命中率差一倍，而两种写法都「看起来对」。
2. **非主轮调用的成本不可归因。** `run_compaction`（`basic.rs:235`）拿到
   `outcome.usage` 后只 emit 一条 `RoundUsage{purpose: Compaction}`，**从不碰**
   `run_turn` 的 `total_usage`（`:427`）⇒ `RunFinished.usage`、`TurnStats`、`meta.json`
   三处都偏小。工具侧摘要器（`summarize.rs:98`）更彻底：`response.usage` 直接丢弃。

### 同类软件的共同结论

三家（Codewhale / pi / deepseek-harness，均在本仓 `tmp/`）：

- **「未上报」必须与「真 0」区分**，不得显示成 0（Codewhale `SessionUsage` 的 `Option<u64>`
  注释原文：*"do NOT display as 0, which would be indistinguishable"*）；
- **辅助调用的 usage 必须单独归因**（pi 持久化在压缩条目上、Codewhale 用 `runtime_cost_owner`）；
- 三家的 `input` 口径**都是「不含缓存」**（pi `faux.ts:267`、Codewhale `pricing.rs:154`
  注释 `/// Non-cached input (prompt) tokens.`、harness `translate.ts:159` 做减法）。

前两条被本 ADR 全盘采纳。第三条**被否决**，理由见下 —— 这是本决策唯一故意偏离业界之处。

## 方案对比

三个方案由独立 subagent 并行生成，代表三种设计倾向。

| | 最小复杂度 | 可扩展优先 | 性能/资源优先 |
|---|---|---|---|
| **P0-1 立场** | **A**（不含缓存） | **B**（含缓存） | **B**（含缓存） |
| **归因机制** | `RunResult.compaction_usage: Usage` | `AdditionalUsage` 键控账本 | `RunResult.compaction_usage: Option<Usage>` |
| 新增类型 | 0 新类型，1 个 `cache_split()` | 2 个（`AdditionalUsage`、`UsageEntry`） | 1 个 `CacheReadout`（8 B `Copy`） |
| 生产代码 | ~125 行 | ~640 行 | 未给出总量 |
| 命中率计算位置 | app（`Option<u8>` 进 View） | app（`CacheView`） | app（每回合算一次） |
| 加新 purpose | 改聚合 | **不动聚合** | 改聚合 |

三份方案在**归因机制上收敛**（都用 `RunResult` 加兄弟字段，都不往
`RunFinished.usage` 里塞），在**P0-1 上分裂**（2:1 选 B）。

### 内存实测（本机 `rustc -O`，`size_of`）

| 形状 | 字节 |
|---|---|
| 现状 `Usage`（`u32×4 + Option<u32>`） | **24 B** |
| `Option<Usage>` | **24 B**（niche 优化） |
| 缓存两桶都改 `Option<u32>`（B 保留 miss） | **32 B** |
| **A + 丢掉可推导的 miss**（`u32,u32,Option<u32>,Option<u32>`） | **24 B** |
| **B + 丢掉可推导的 miss**（同上形状） | **24 B** |

**结论**：A 与 B 的内存布局可以**完全相同**。性能方案主张的「B 必须是 24 B、A 会涨到 40 B」
不成立（40 B 对所有形状都不成立）—— 真正决定字节数的是**是否保留可推导的 `miss`**，
与 A/B 无关。因此 **P0-1 不是性能决策**，三个方案中的性能论据在此处无效。

## 决策

### D1 · `input_tokens` 采用 **B**（provider 回报的 prompt 总量），并写进字段 doc

`input_tokens` = provider 说的「这次处理了多少 prompt token」，**含**缓存命中部分；
`prompt_cache_hit_tokens` 是它的可选明细。字段 doc 必须写明这条。

#### 为什么否决业界一致选择的 A

**决定性理由：A 会让同一个 prompt 报出两个数，取决于 provider 报不报缓存明细。**

| 场景 | B 下 `input` | A 下 `input` |
|---|---|---|
| provider 报 `prompt=23848, cached=23590`（真实样本） | **23,848** | **258** |
| 同一 prompt，provider 不报 `cached` | **23,848** | **23,848** |

A 下这个字段的数值成了「provider 上报完整度的函数」。实测：**4,854 次调用里 704 次
（14.9%）完全不上报 usage**，其中 minimax 的 15 个会话 `input_tokens` **全部为 0**
（`compat.rs:79` 关闭了 `supports_stream_usage`）。跨 provider 比较、`meta.json` 跨会话
累计、同比 —— 这些都会静默得出错误结论，**而本需求要消灭的正是这类不可信**。

B 下该字段与上报完整度无关，是唯一能无条件比较的量。

#### 对「最小复杂度方案」三条反驳的回应

1. **「B 靠约定、A 靠类型」** —— 反了。B 只有一个约定：*分母是 `hit + miss`*。
   A 多一个约定：*调用方必须知道 provider 报没报缓存*，否则会把 `258` 与 `23,848`
   当成同一量纲。A 的约定更难被发现，因为两值类型相同、字段名相同。
2. **「B 下 `hit+miss==input` 是恒等，断言等于没断言」** —— 这句本身是误读。
   B 下要断的是 **`hit + miss <= input`**（真不变量，实测 4,153 次调用 0 次违反，
   *可检*）；A 下 `input := prompt − hit − miss` 才是**构造恒等**，它检不出任何 provider 异常。
3. **「`miss` 是推导值就该消失」** —— 这条**被采纳**，但与 A/B 无关：
   `miss = input − hit` 在两种口径下都可推导。本决策**删掉落地的 `miss`**
   （保留字段名仅为老 JSON 反序列化，见 D2），于是 A 与 B 的 `Usage` 都是 24 B。

**同时全盘采纳 A 派与各家共有的纪律**：明细桶一律 `Option<u32>`；`Add` 对所有 Option 桶
用同一条规则（两侧都 `Some` 才相加，否则继承已上报侧，**不得**把 `Some(0)` 折叠成 `None`）。

#### 什么会推翻 B

若接入一家「按类别单独计价、且总量不含缓存」的 provider（如 `cache_write` 单独按时长计费），
届时正确做法不是把 B 翻成 A，而是**并列新增**一个「计费类别」视图，让 B 的锚定总量继续存在
—— 两个轴正交。

### D2 · 缓存明细桶改 `Option<u32>`，并删掉落地的 `miss`

**改 `Option` 的理由**：实测 **14.9%** 的调用完全不报 usage、**19.4%** 的会话全程无读数。
「两字段皆 0 且 `input > 0`」的代理判别（`round_clock.rs:429` 现状）在这 14.9% 上会给出
错误结论。Codewhale 与 pi 都用 `Option` 表达同一区分；本仓 `reasoning_tokens` 已有先例。

**删 `miss` 落地的理由**：它是 `input − hit` 的推导值。持久化可再生数据会在每层引入
「必须在相加时也正确」的额外桶。`prompt_cache_miss_tokens` 保留为恒 `None` 的死字段，
只为老 JSONL 能反序列化（删除字段会让老会话静默丢值）。

`Add` 规则照抄 `usage.rs:33-39`，抽成私有 helper 一处定义。

### D3 · 压缩消耗走 `RunResult.compaction_usage`（兄弟字段），不用键控账本

三个方案都收敛于此形态，差异只在 `Option<Usage>` vs `Usage`（零值 = 没压过）。
本 ADR 选 **`Usage`（非 `Option`）**：压缩的「没发生」与「发生但为 0」在展示上等价，
不承载语义；而 `Option<Usage>` 会让人误以为是「未上报」语义（与 `reasoning_tokens` 混）。

```rust
pub struct RunResult {
    pub usage: Usage,              // 含义不变：本回合**主对话轮**消耗
    pub compaction_usage: Usage,   // 本回合压缩（自动 + 手动 /compact）消耗；零值 = 没压过
    ...
}
```

`run_compaction` / `drain_boundary` 通过 `&mut Usage` 回填，与 `total_usage` **同一 sink 类型、
同一套 `Add`**。`RunFinished` **不加键** ⇒ 无压缩回合的 `--json` 逐字节不变。

**为什么否决键控账本（可扩展方案）**：它主张「加新 purpose 不改聚合」，代价是 2 个新类型、
~640 行、及一条「字符串键失去编译期校验」的新风险。实测三个用途的频次是压缩 10、
工具侧摘要 ≤15、子 agent 55 —— 其中子 agent **已平铺进 `RunResult.usage`**
（`app_loop.rs:1533`），是三者中唯一不丢数据的。真实需求只确认了**压缩**一个。
账本是为「第二个用途」预付的复杂度；而按本设计，接入第二个用途只是再加一个兄弟字段（~6 行），
比键控账本便宜。**若未来用途超过三个，再回来改账本。**

**为什么否决 app 侧按 `purpose` 事件分流**：它能给出正确的新数，却把
`RunFinished.usage` / TUI 累计 / `meta.json` / `run_end` 四处偏小的旧数留在原地 ——
恰好违反验收要求「两者之和与 `purpose` 分组求和一致」。且 3 个消费者各写一遍分流，
覆盖面靠自觉；改 `RunResult` 字面量则编译器逐点报错。

**为什么不抄 pi 的「持久化到压缩条目」**：本仓 session 是 `Vec<Message>`，
没有可挂 usage 的条目类型；为此新增一种消息类型是为一件事故引入的长期结构。

### D4 · 持久化：**不动 `meta.json`**

`meta.input_tokens` 保持既有含义（B 口径，与本决策一致），因此**播种口径与
新累计口径统一**，无需迁移、无需升 `meta.v`。这是选 B 的一处实质收益。

压缩消耗**不进 meta**：它在 `RunResult` 里可查，而 meta 的用途是跨会话播种累计。
若将来确实需要，「再加一个兄弟键」与 D3 的扩展方式一致。

### D5 · 压缩失败路径的成本口径：**不修**

压缩失败可能已真实消耗 token。修它要改 `CompactError` 的公开形态并引入「失败也有 usage」
的新消费者，超出本需求。缓解：`/status` 用中性措辞（`压缩`），不写成「全部额外消耗」；
`design-final` 写明这条已知缺口。

### D6 · 澄清 `RoundUsage` 的契约

`event.rs:79` 注释称「`RoundUsage` 那个变体会进 usage 累计」，但实测**没有任何消费者
累加它**（只有 `round_clock` 记日志、`main.rs` 原样转发给 `--json`）。这是注释与实现的矛盾。

本决策：**把注释改成与实现一致**，并明确累计责任在 `ys-loop`（`run_turn` 的
`total_usage` / `compaction_usage`）与 app 侧 `TurnStats`，不靠事件消费者。
这样 `ModelStreamDiscarded` 不借用 `RoundUsage` 的理由也更清楚：它**零消耗**，
进了 `RoundUsage` 就会被 loop 侧记账。

## 后果

### 正面

- `input_tokens` 有确定语义，命中率有唯一分母，**跨 provider 可比较**。
- 压缩消耗在 `RunResult` / `/status` / `--json`（经既有 `RoundUsage{purpose}`）可归因。
- `meta.json` 口径**不需要迁移**（选 B 的直接收益）。
- 「未上报」与「真 0」在类型层面可分。
- `Usage` 仍是 **24 B**（删掉可推导的 `miss`，抵消 `Option` 化的 +8 B）。

### 负面（已接受）

1. **与三家主流实现的 `input` 语义不同**。跨项目对照需换算
   （`their_input = our_input − our_hit`）。写进字段 doc 与 `design-final`，避免下一个人重新吵。
2. **`Option` 化的静默降级面**：约 45 处引用、7 个文件。风险不是编译错误，而是
   「某处把 `None` 当 `0` 用了」。缓解：`cache_split()` / `is_reported()` 是唯一读取入口。
3. **`RunFinished.usage ≠ 本回合总花费` 修好后更显眼**（R4）。有意选择：`RunFinished.usage`
   的逐字节断言与「含义不变」是硬约束。必须在 `RunResult` doc 里把
   `usage` / `compaction_usage` / 两者之和的关系写死。
4. **`RunResult` 加字段触及 18 处构造点**（10 处 `basic.rs`、4 处 `main.rs` 测试、
   2 处 `round_clock` 测试、1 处 `app_loop`）。机械但需逐点确认，建议单独 commit。
5. **压缩失败路径仍漏账**（D5）。
6. **R8 标定（0.704）在 B 下数值不变**，无需重跑 —— 这是相对 A 的又一处省下的代价。

### 被否决的方案

- **A（跟随业界「不含缓存」）**：见 D1。否决的是**符号**，不是它背后的「桶集开放」思想
  —— 后者已完整采纳（D2）。
- **`AdditionalUsage` 键控账本**：见 D3，为未确认的第二用途预付复杂度。
- **app 侧按 `purpose` 事件分流**：见 D3，会把四处偏小的旧数留在原地。
- **pi 的「持久化到压缩条目」**：见 D3，本仓无可承载的条目类型。
- **把 `Compaction` 并入 `RunFinished.usage`**：会改变该字段含义，破坏
  `main.rs:5835`/`:5843` 的逐字节断言与既有消费者。
- **`TokenClass` 枚举 / `CostSink` trait / 价格层 / 按 provider 分组**：无需求支撑。

## 关联

- [design.md](../arch/usage-observability/design.md) — 完整设计
- [context.md](../arch/usage-observability/context.md) — 现状与约束
- [ADR-0060](0060-thinking-policy-per-call-kind.md) — 邻接的「按调用面分派」决策
- [cost-optimization/context.md](../arch/cost-optimization/context.md) — 重叠需求；
  D1/D2 与它的第 1 项是同一处改动，应由本需求落地
