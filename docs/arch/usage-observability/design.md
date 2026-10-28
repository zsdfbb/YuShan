# usage-observability — 架构设计

> 日期：2026-10-28
> 性质：`arch-design` 产出。上游见 [`context.md`](context.md)（现状与约束）。
> 决策记录：[ADR-0061](../../adr/0061-usage-buckets-and-attribution.md)（D1–D6，含分歧裁决）。
> 范围：P0-1（口径）/ P0-2（可见）/ P0-3（归因）。**不含任何省钱策略**。

---

## 0. 一句话方案

`input_tokens` 的语义钉为「provider 回报的 **prompt 总量**，含缓存命中」；
缓存明细降为**可选桶**（`Option<u32>`），删掉可推导的 `miss` 以抵消字节数；
压缩消耗走 `RunResult` 的**兄弟字段**，`RunFinished.usage` 含义不变。

**由此得到**：命中率有唯一分母、跨 provider 可比较；`Usage` 仍是 **24 B**；
`meta.json` 口径无需迁移。

---

## 1. 模块划分与职责

改动落在**已有四层**，0 新 crate、0 新 trait。

| # | 模块 | 本轮职责 | 文件 |
|---|---|---|---|
| M1 | 口径层（零 tokio） | `input_tokens` 含义的唯一出处；`cache_split()` / `is_cache_reported()` 是**唯一**读取入口 | `crates/ys-core/src/usage.rs` |
| M2 | 方言归一 | 两种缓存方言 → `(prompt, Option<hit>)`；`miss` 不落地 | `adapters/model-openai-compatible/src/{response,lib}.rs` |
| M3 | 回合累计 | 主对话与压缩各一个累计对象，随 `RunResult` 出去 | `crates/ys-loop/src/{basic,result}.rs` |
| M4 | 汇总 + 呈现 | app 算结论（`Option<u8>`），TUI 只画 | `apps/coding-agent/src/*`、`crates/ys-tui-coding/src/{view,draw,commands}.rs` |

**承重边界**：

- 判别只写在 `Usage`（把 `round_clock.rs:429` 的日志专用版本搬进去），app 侧复用它。
- `ys-tui-coding` **不依赖 `ys-runtime`** ⇒ TUI 不认识 `Usage`，只能由 app 推**已投影的结论**。
- 压缩归因**不新建通道**：`RoundUsage{purpose}` 早已带 usage，本轮只是让 loop 也记账。

---

## 2. 关键数据流

### 2.1 P0-1 · 口径在适配器一次落定

```
SSE 尾包 { prompt_tokens: 9760, prompt_tokens_details.cached_tokens: 9728 }
  └─ ChatUsage::cache_tokens()          O(1)  0 alloc   两方言归一
      └─ Usage {
           input_tokens: 9760,          ← B：provider 的 prompt 总量，原样
           prompt_cache_hit_tokens: Some(9728),
           prompt_cache_miss_tokens: None,   ← 推导值，不落地
           output_tokens, reasoning_tokens,
         }
```

`miss` 不落地的连锁收益：判据从「两栏是否为 0」变成 **`hit.is_some()`**，
一举消灭 `round_clock.rs:276-286` 列出的全部误判形态（`input=0 && hit>0`、
provider 只填 `total_tokens` 等）。

### 2.2 P0-2 · app 算结论，TUI 只画

```
TurnStats::record(&result.usage)        O(1)  0 alloc
  └─ TurnStats 内部：hit/miss 计数 + reported 标志（照抄 reasoning_tokens 的既有模式）
      └─ cache_hit_pct() -> Option<u8>   ← 分母恒为 hit + miss，绝不取 input_tokens
          └─ CodingView { cache: Option<(u32,u32)>, cache_hit_pct: Option<u8> }
              └─ draw / commands：`缓存 99%`  vs  `缓存 -`
```

**`None` → `-`、`Some(0)` → `0%` 只判一次**，在 app 侧。TUI 不做除法、不判「未上报」。

### 2.3 P0-3 · 压缩消耗进兄弟字段

```
Step 4b  estimated + fixed > trigger          ← O(messages) 扫描，无分配（既有，不碰）
  └─ run_compaction(ctx, rounds, &mut compaction_usage)
      ├─ generate_summary(...)                ← 网络
      ├─ emit RoundUsage{purpose: Compaction}  ← 既有事件，0 新增
      └─ *compaction += outcome.usage          ← 同一 sink 类型、同一套 Add

回合收尾：RunResult { usage: total_usage, compaction_usage, .. }
         RunFinished { usage: total_usage, .. }   ← 不含压缩（逐字节承诺不破）
app 记账：stats.record(&result.usage); stats.record_compaction(&result.compaction_usage)
```

| 项 | 复杂度 | 分配 |
|---|---|---|
| 压缩路径每回合增量 | O(1) | 0（一次 `Add` + 24 B memcpy） |
| 非压缩路径每回合增量 | **0** | **0**（零值） |
| 事件数增量 | — | **0** |

---

## 3. 核心接口

```rust
// ── M1 · crates/ys-core/src/usage.rs ────────────────────────────────────────
pub struct Usage {
    /// provider 回报的 prompt **总量**（含缓存命中部分）。
    ///
    /// 与 provider 是否上报缓存明细**无关** —— 这是唯一能跨 provider 比较的量。
    /// 缓存命中率的分母**不得**用它，用 [`Self::cache_split`]。
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 命中缓存的 prompt token 数；`None` = **provider 未上报**（≠ 0 命中）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u32>,
    /// **恒 `None`**：`miss = input_tokens - hit` 可推导，不再落地。
    /// 保留字段仅为老 JSONL 能反序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

impl Usage {
    /// `(hit, miss)`；`None` = 没有可用读数。
    /// `miss = input_tokens.saturating_sub(hit)` —— 可推导，故不存储。
    #[inline] pub fn cache_split(&self) -> Option<(u32, u32)>;
    #[inline] pub fn is_cache_reported(&self) -> bool;   // = hit.is_some()
}

// ── M2 · adapters/model-openai-compatible/src/response.rs ───────────────────
impl ChatUsage {
    /// 两方言 → `(prompt_total, Option<hit>)`。
    /// DeepSeek: 直接取两栏；OpenAI: `prompt_tokens_details.cached_tokens`；
    /// 两样都无 ⇒ `None`（**不是** 0 命中）。
    pub fn cache_tokens(&self) -> (u32, Option<u32>);
}

// ── M3 · crates/ys-loop/src/result.rs ───────────────────────────────────────
pub struct RunResult {
    pub stop_reason: StopReason,
    /// 本回合**主对话轮**消耗。含义不变。
    pub usage: Usage,
    /// 本回合压缩（自动 + 手动 `/compact`）消耗；**全零 = 没压过**。
    ///
    /// 与 `usage` 是**兄弟**，不是子集：`RunFinished.usage` 不含它（逐字节承诺）。
    /// 本回合总花费 = `usage + compaction_usage`。
    pub compaction_usage: Usage,
    pub rounds: u32,
    pub final_message: Option<Message>,
}

// 私有签名变化（两个调用点：自动压缩 / 手动 `/compact`）
async fn run_compaction(
    ctx: &mut RuntimeContext<'_>,
    rounds: u32,
    compaction: &mut Usage,     // 回填；`&mut` 而非返回值 ⇒ 调用点无需 `?` 后处理
) -> Result<bool, LoopError>;

// ── M4 · apps/coding-agent/src/status.rs + ys-tui-coding/src/view.rs ────────
impl TurnStats {
    /// 命中率千分位 → 整数百分比；`None` = 无读数（渲染成 `-`）。
    /// 只有 `hit == hit + miss` 才给 100，避免四舍五入把 99.x 说成满命中。
    pub fn cache_hit_pct(&self) -> Option<u8>;
}
pub struct CodingView {
    pub cache_tokens: Option<(u32, u32)>,   // (hit, miss)，已解算
    pub cache_hit_pct: Option<u8>,
    pub compaction_usage: Option<Usage>,     // 已投影，TUI 直接格式化
    ...
}
```

**刻意不加**：`billed_total()`、`CacheStatus` 枚举、`RunFinished.compaction_usage` 键、
`meta` 新键、价格层。

---

## 4. P0-1 的裁决：选 **B**（`input_tokens` = prompt 总量）

三个候选方案在此**分裂**：最小复杂度选 A，可扩展与性能方案选 B。最终选 B。

### 4.1 决定性理由：A 会让同一个 prompt 报出两个数

| 场景 | B 下 `input` | A 下 `input` |
|---|---|---|
| provider 报 `prompt=23848, cached=23590`（真实样本） | **23,848** | **258** |
| 同一 prompt，provider 不报 `cached` | **23,848** | **23,848** |

A 下这个字段成为「provider 上报完整度的函数」。实测 **4,854 次调用里 704 次（14.9%）
完全不上报 usage**；minimax 的 15 个会话 `input_tokens` **全部为 0**
（`compat.rs:79` 关闭 `supports_stream_usage`）。跨 provider 比较、`meta.json` 跨会话累计、
同比 —— 都会静默出错，**而本需求要消灭的正是这类不可信**。

B 下该字段与上报完整度无关，是唯一能无条件比较的量。

### 4.2 对 A 派三条反驳的回应

1. **「B 靠约定、A 靠类型」——反了。** B 只有一个约定：*分母是 `hit + miss`*，
   而它由 `cache_split()` 独占实现，违反不了。A 多一个更难发现的约定：
   *调用方必须知道 provider 报没报缓存*，否则把 `258` 与 `23,848` 当同一量纲 ——
   两者类型相同、字段名相同，编译器与测试都抓不到。
2. **「B 下 `hit+miss==input` 是恒等，断言等于没断言」——误读。**
   B 要断的是 **`hit + miss <= input`**：真不变量，实测 4,153 次 0 违反，**可检**。
   A 下 `input := prompt − hit − miss` 才是构造恒等，检不出任何 provider 异常。
3. **「`miss` 是推导值就该消失」——采纳，但与 A/B 无关。**
   `miss = input − hit` 在两种口径下都可推导。本设计**删掉落地的 `miss`**（见 §4.3），
   于是 A 与 B 的 `Usage` **同为 24 B**。

### 4.3 内存：A 与 B 可以完全相同

本机 `rustc -O`，`size_of` 实测：

| 形状 | 字节 |
|---|---|
| 现状（`u32×4 + Option<u32>`） | **24 B** |
| 缓存两桶都 `Option` 且**保留** `miss`（`u32,u32,Option,Option,Option`） | **32 B** |
| **删掉落地的 `miss`**（`u32,u32,Option<hit>,Option<reasoning>`） | **24 B** |

**决定字节数的是「是否保留可推导的 `miss`」，不是 A/B。**
⇒ P0-1 **不是**性能决策；性能候选方案中「A 会涨到 40 B」的说法不成立（40 B 对任何形状都不成立）。

### 4.4 采纳自 A 派与三家的共同纪律

- 明细桶一律 `Option<u32>`；
- `Add` 对所有 Option 桶用同一条规则：两侧都 `Some` 才相加，否则继承已上报侧，
  **不得**把 `Some(0)` 折叠成 `None`（抽成私有 helper，一处定义）；
- 新增桶 = 加字段，不动消费方。

### 4.5 选 B 省下的三处代价

| 项 | 在 B 下 |
|---|---|
| `e2e_bench.rs:275-279` 的 `cache_miss = input − hit` | **无需改** —— 它本来就按 B 在算（仓里早有人这么写，只是没写下约定） |
| R8 标定 `docs/design-final/skills.md:244`（0.704） | **数值不变，无需重跑** |
| `meta.json` 播种口径 | **不变，无需迁移** |

> A 派自己在风险清单里写「R1 标定必须重跑，**且重跑可能无效**」——因为标定用的是
> minimax，而 minimax 不上报 usage（本仓 15 个 minimax 会话 `input_tokens` 全为 0）。
> 这使 A 的代价从「重跑一次」变成「**可能永远补不回来**」。

### 4.6 什么会推翻 B

接入「按类别单独计价、且总量不含缓存」的 provider（如 `cache_write` 按时长计费）时，
正确做法不是把 B 翻成 A，而是**并列新增**一个计费类别视图，让 B 的锚定总量继续存在。
写在此处，供下次决策有据可依。

---

## 5. P0-3 的归因机制：改 `RunResult`（兄弟字段）

三个候选方案在此**收敛**（都否决「并入 `RunFinished.usage`」、都否决「持久化到压缩条目」），
差异只在 `Option<Usage>` vs `Usage`。选后者：压缩的「没发生」与「发生但为 0」在展示上等价，
`Option` 只会让人误以为是「未上报」语义（与 `reasoning_tokens` 混淆）。

### 为什么不选 app 侧按 `purpose` 事件分流

能给出正确的新数，但会把 `RunFinished.usage` / TUI 累计 / `meta.json` / `run_end`
四处偏小的旧数**留在原地** —— 恰好违反验收要求「两者之和与 `purpose` 分组求和一致」。
且 3 个消费者各写一遍分流，覆盖面靠自觉；改 `RunResult` 字面量则编译器逐点报错。

### 为什么不选键控账本（可扩展候选）

它主张「加新 purpose 不改聚合」，代价是 2 个新类型、~640 行、以及「字符串键失去编译期校验」
的新风险。实测三个用途的频次：压缩 **10**、工具侧摘要 **≤15**、子 agent **55**
（共 5,738 次工具调用）—— 其中子 agent **已平铺进 `RunResult.usage`**
（`app_loop.rs:1533`），是三者中**唯一不丢数据**的，故不构成账本的必要条件。
真实需求只确认了**压缩**一个。

账本是为「第二个用途」预付的复杂度；而按本设计，接入第二个用途只是再加一个兄弟字段（~6 行），
比键控账本便宜。**若未来用途超过三个，再回来改账本** —— 这是可证伪的触发条件。

### 为什么不抄 pi 的「持久化到压缩条目」

本仓 session 是 `Vec<Message>`，没有可挂 usage 的条目类型；为此新增一种消息类型
是为一件事故引入的长期结构。pi 能这么做是因为它的 session 本身就是「类型化条目流」。

### 归因语义的准确边界（必须写进注释）

- `RunResult.usage`：**只含主对话轮**。子 agent 已上卷（`app_loop.rs:1533`）。
- `RunResult.compaction_usage`：**只含压缩**。全零 = 没压过。
- `RunFinished.usage`：等于 `usage`，**不含**压缩（逐字节兼容承诺）。
- 本回合总花费 = `usage + compaction_usage`。

三个数写死，否则下一个消费者还会猜。

---

## 6. 受哪些同类软件启发

照抄**语义与纪律**，不抄**机制**。

| 来源 | 拿走的 | 丢弃的 |
|---|---|---|
| Codewhale | `Option<u64>` 表达「未上报 ≠ 0」；reasoning 是 output 子集的警告；辅助调用归到所属 turn 的**结论** | 逐轮 `Option::map` 累加（一次缺报会永久钉死），改本仓 `Add` 四分支 |
| pi | 压缩消耗与主对话**分开且归属回合**的结论；`input = prompt − cacheRead` 的**减法提醒**（用于推导 `miss`） | 「持久化到压缩条目」（本仓无承载物） |
| deepseek-harness | 「估算不进账单」——`contextBreakdown` 是估算、`totalTokens` 是账单，两者不混 | O(surface) 的 `measure()` clone；`CostSink` 式价格层 |

**故意偏离**：三家的 `input` 都不含缓存，本设计选「含缓存」。理由见 §4.1 ——
它们面对的是「单一 provider、缓存必报」的场景，我们面对的是「多 provider、14.9% 不报」。

---

## 7. 主要代价与风险（诚实清单）

**R1 · 与主流实现的 `input` 语义不同。** 跨项目对照需换算
（`their_input = our_input − our_hit`）。缓解：写进 `usage.rs` 字段 doc 与 `design-final`，
并在此 ADR 留档，避免下一个人重新吵一遍。

**R2 · `Option` 化的静默降级面。** 约 45 处引用、7 个文件。风险不是编译错误
（那些编译器会抓），而是**某处把 `None` 当 `0` 用了**。缓解：`cache_split()` /
`is_cache_reported()` 是唯一读取入口；`UsageTotals` 的 `reasoning_reported`
模式（`AtomicBool`，`subagent.rs:131/147/157`）**照抄**，不发明新写法。

**R3 · `RunFinished.usage ≠ 本回合总花费` 修好后更显眼。** 有意选择（逐字节承诺 +
「含义不变」是硬约束）。代价是必须在 `RunResult` doc 里把三个数的关系写死（§5 末）。

**R4 · `RunResult` 加字段触及 18 处构造点。** 实测分布：10 处 `basic.rs`、
4 处 `main.rs` 测试、2 处 `round_clock` 测试、1 处 `app_loop`、1 处 `result.rs`。
机械但需逐点确认，建议单独 commit。不加 `#[non_exhaustive]`（那会让 app 侧测试无法构造字面量）。

**R5 · 压缩失败路径仍漏账。** `Err` 分支拿不到 `outcome.usage`。本设计不修（见 D5），
在 `design-final` 写明缺口。

**R6 · `prompt_cache_miss_tokens` 成死字段。** 恒 `None`，保留只为老 JSONL 反序列化。
删除会让老会话静默丢值，故留。

**R7 · 命中率截断。** 只有 `hit == hit + miss` 才给 100%，避免 `99.6%` 四舍五入成满命中。

**R8 · `prompt_tokens: 0` 且 `cached: 0` 的角落。** 显示 `-`（正确的诚实），
但用户可能读成「没测到缓存」。缓解：`/status` 把 `-` 展开成 `(provider 未上报)`。

---

## 8. 代码量估算与改动文件清单

### 8.1 估算

| 分期 | 内容 | 生产 | 测试 | 合计 |
|---|---|---|---|---|
| ① P0-1 | `Usage` 口径 + `Option` 化 + 适配器归一 + 删 `miss` 落地 | ~110 | ~120 | ~230 |
| ② P0-3 | `RunResult.compaction_usage` + `run_compaction` 回填 + 18 处字面量 | ~40 | ~55 | ~95 |
| ③ P0-2 | `TurnStats` / `CodingView` / `/status` / 状态栏 | ~130 | ~110 | ~240 |
| **合计** | | **~280** | **~285** | **~565 行** |

顺序：**① → ② → ③**（③ 依赖前两者的数据）。每期单独跑
`./scripts/cargo-build.sh` + `./scripts/par-test.sh` 可绿。

### 8.2 改动文件清单

**一期 · P0-1（口径）**

| 文件 | 改动 |
|---|---|
| `crates/ys-core/src/usage.rs` | 字段 doc（B 口径）；缓存两桶 → `Option<u32>`；新增 `cache_split()` / `is_cache_reported()`；`Add` 抽 `add_opt`；不变量用例（`hit+miss <= input`）与「未上报 ≠ 真 0」用例 |
| `crates/ys-core/src/lib.rs` | 既有 `Usage` 用例改 `Some(_)`；新增「三个 Option 桶同一条 Add 规则」用例 |
| `adapters/model-openai-compatible/src/response.rs` | `ChatUsage` 缓存字段 → `Option<u32>`；`cache_tokens()` → `(u32, Option<u32>)`；doc 写明两种方言 |
| `adapters/model-openai-compatible/src/lib.rs` | `Usage` 构造改用新签名；补「只报 hit」「都不报」「显式 0」三态单测 |
| `apps/coding-agent/src/subagent.rs` | `UsageTotals` 两个缓存桶加 `reported` 位（照抄 `reasoning_reported`） |
| `apps/coding-agent/src/round_clock.rs` | `turn_cache()` 改用 `cache_split()`；日志按 `Option` 写 `null` |

**二期 · P0-3（归因）**

| 文件 | 改动 |
|---|---|
| `crates/ys-loop/src/result.rs` | `RunResult.compaction_usage` |
| `crates/ys-loop/src/basic.rs` | `run_compaction` 加 `&mut Usage` 参数；`run_turn` 局部 `compaction_usage`；`drain_boundary` 透传；5 处 `RunResult` / `RunFinished` 字面量；3 条用例 |
| `apps/coding-agent/src/{main,app_loop}.rs` | 6 处字面量补字段；`stats.record_compaction(...)` |

**三期 · P0-2（可见）**

| 文件 | 改动 |
|---|---|
| `apps/coding-agent/src/status.rs` | `TurnStats` 缓存计数 + `reported` 标志 + `compaction` 桶；`cache_hit_pct()` |
| `apps/coding-agent/src/view.rs` | 投影 `cache_tokens` / `cache_hit_pct` / `compaction_usage`；三态透传用例 |
| `crates/ys-tui-coding/src/view.rs` | `CodingView` 加三字段；`Default` / `for_test()` 补字段 |
| `crates/ys-tui-coding/src/draw.rs` | 状态栏隐藏条件由「总量为 0」改为「无读数」；追加 `· 缓存 N%` |
| `crates/ys-tui-coding/src/commands.rs` | `/status` 新增 `cache` 行（`-` 展开成「provider 未上报」）与 `compaction` 行 |

**不做改动（刻意）**：`crates/ys-loop/src/compact.rs`（`CompactOutcome.usage` 已存在）、
`crates/ys-loop/src/token.rs`、`crates/ys-runtime`（`Agent` 公开面不变）、
`apps/coding-agent/tests/{e2e_bench,e2e_skills}.rs`（B 下数值不变）、
`crates/ys-session/src/meta.rs`（口径不变，无需迁移）。

### 8.3 变异测试清单（`AGENTS.md` 硬性要求）

> 所有用例基于**构造读数**（不依赖真实 provider，真实值会变）。

| # | 注入 | 必须变红的用例 |
|---|---|---|
| M1 | `cache_split()` 把 `None` 当 `(0, input)` | 场景 2：状态栏显示 `0%` 而非 `-` |
| M2 | `cache_split()` 的 `miss` 用 `prompt − hit` 之外的值（如恒 0） | 场景 1（`input=100, hit=98`）：正确 `98/100=98%`，注入后 `98/98=100%` ⇒ 必红 |
| M3 | `run_compaction` 只 emit 不 `*= outcome.usage` | 场景 4：压缩计数为 0 |
| M4 | `RunResult.usage` 并入 `compaction_usage` | `RunFinished.usage` 不含压缩的用例 |
| M5 | `Some(0)` 被折叠成 `None` | 「首调用真 0 命中」用例显示 `-` 而非 `0%` |
| M6 | `hit == total` 才 `100%` 的判据改成四舍五入 | R7 用例（99.6% 必须显示 99） |

---

## 9. 已知风险与待验证项

- [ ] **R1 口径差异需在 `design-final` 留档**：跨项目对照的换算公式。
- [ ] **R5 压缩失败路径**：建议单独立项，本次只留缺口说明。
- [ ] **工具侧摘要（`summarize.rs`）的成本接入**：需 `ys_tool::Summarizer` 返回值带 usage，
      属公开 trait 变更，不在本需求。若它先落地，D3 的「兄弟字段」需再加一个。
- [ ] **`e2e_skills` 复核**：B 下数值不变，但建议在 ① 落地后跑一次
      `cargo test -p ys-coding-agent --test e2e_skills -- --ignored` 确认。
      ⚠️ 若当前凭证仍是 minimax，则该用例只会打印「未校准」——这是 provider 限制，不是回归。

---

## 10. 候选方案存档

三个方案由独立 subagent 并行生成。此处只记**结论与取舍**；详细论证见
各自 transcript（`~/.yushan/subagents/7429_{0,1,2}.md`）。

| | 最小复杂度 | 可扩展优先 | 性能/资源优先 |
|---|---|---|---|
| P0-1 | A（不含缓存） | B（含缓存） | B（含缓存） |
| P0-3 | `RunResult.compaction_usage: Usage` | `AdditionalUsage` 键控账本 | `RunResult.compaction_usage: Option<Usage>` |
| 新类型 | 0（1 个纯函数） | 2（`AdditionalUsage` / `UsageEntry`） | 1（`CacheReadout`，8 B） |
| 生产代码 | ~125 行 | ~640 行 | 未给总量 |
| 命中率 | app 算 `Option<u8>` | app 算 `CacheView` | app 每回合算一次 |
| 加新 purpose | 改聚合 | **不动聚合** | 改聚合 |

**采纳**：最小复杂度的「删可推导的 `miss`」「app 算结论」、性能方案的「内存实测纪律」、
可扩展方案的「兄弟字段不污染 `RunFinished`」。
**否决**：A 口径（§4.1）、键控账本（§5，为未确认的第二用途预付）、
`CacheReadout` 独立类型（`cache_split()` 返回元组即可，不必新类型）。

**三个方案共同确认的两点**（可视为交叉验证）：
① 归因绝不并入 `RunFinished.usage`；② 命中率的判别必须在 app 侧完成（UI 线程约束）。
