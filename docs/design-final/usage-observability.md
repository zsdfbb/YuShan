# 用量口径与归因 — As-built

> 性质：**实际建成记录**。设计见 `docs/arch/usage-observability/design.md`，
> 决策见 [`ADR-0061`](../adr/0061-usage-buckets-and-attribution.md)（D1–D6 + D3b），
> 质量分析见 `docs/arch/usage-observability/review.md`。
> 对外契约已写入 [`architecture.md` §13](../architecture.md)。

## 一句话

`input_tokens` 的含义钉死为「provider 回报的 prompt **总量（含缓存命中）**」；
缓存明细降为**可选桶**（`Option<u32>`，`None` = 未上报 ≠ 真 0），命中率由
**逐轮累加**的 `(hit, miss)` 给出、分母恒为 `hit + miss`；压缩消耗走 `RunResult`
的**兄弟字段**，`RunFinished.usage` 含义不变。

## 口径：五个数的定义

| 字段 | 定义 | 未上报时 | 出处 |
|---|---|---|---|
| `input_tokens` | provider 回报的 prompt **总量，含**缓存命中部分 | `0`（与 provider 上报完整度无关） | `crates/ys-core/src/usage.rs:9` |
| `output_tokens` | 完成侧 token 数（原样取 provider 的 `completion_tokens`） | `0` | `crates/ys-core/src/usage.rs:10` |
| `prompt_cache_hit_tokens` | 命中缓存的 prompt token 数 | `None` = **未上报**；`Some(0)` = **真 0 命中** | `crates/ys-core/src/usage.rs:16` |
| `prompt_cache_miss_tokens` | **恒 `None`**（`miss = input − hit` 可推导，不落地）；仅保留以让老 JSONL 反序列化 | `None` | `crates/ys-core/src/usage.rs:20` |
| `reasoning_tokens` | 推理 token 数（provider 若上报） | `None` | `crates/ys-core/src/usage.rs:26` |

**唯一读取入口**（app / TUI / 日志都不得自己判 `Option`）：

- `Usage::cache_split() -> Option<(u32, u32)>` —— `(hit, miss)`，`None` = 本对象不自带读数
  （`crates/ys-core/src/usage.rs:57`）。`miss` 由 `input_tokens.saturating_sub(hit)` 推导
  （`:59`），provider 报 `hit > input` 时饱和到 0、不 panic。
- `Usage::is_cache_reported() -> bool` —— 等价于 `hit.is_some()`，`Some(0)` 也算上报
  （`crates/ys-core/src/usage.rs:64`）。

**`Add` 的单一规则**（`crates/ys-core/src/usage.rs:34` `add_opt`）：所有 `Option` 桶共用 ——
两侧都 `Some` 才相加，否则继承已上报的那一侧，**不把 `Some(0)` 折叠成 `None`**
（`Some(0) + None ⇒ Some(0)`）。`input/output` 无条件相加（`:74`）。

**与主流实现的差异（必须记住的换算）**：业界三家的 `input` **不含**缓存命中，本仓**含**。
跨项目对照用 `their_input = our_input − our_hit`（ADR-0061 D1）。

## 数据流

```
SSE 尾包 usage
  └─ ChatUsage::cache_reading() -> (prompt_total, Option<hit>)
        adapters/model-openai-compatible/src/response.rs:58
        两方言归一：DeepSeek 认 prompt_cache_*（只报 miss 时 hit = prompt − miss），
                   OpenAI 认 prompt_tokens_details.cached_tokens，
                   两样都无 ⇒ None（不是 0 命中）
     └─ Usage { input_tokens: prompt_total, prompt_cache_hit_tokens, prompt_cache_miss_tokens: None, .. }
            adapters/model-openai-compatible/src/lib.rs:1078

逐轮（BasicLoop::run_turn）
  ├─ cache_reading 逐轮累加：round_usage.cache_split() 累进局部 Option<(u32,u32)>
  │     crates/ys-loop/src/basic.rs:745-752
  ├─ total_usage += response.usage                  （主对话轮累计）
  └─ compaction 成功后 *compaction += outcome.usage （压缩独立累计）
        crates/ys-loop/src/basic.rs:267

回合收尾 → RunResult { usage, cache_reading, compaction_usage, .. }
        crates/ys-loop/src/result.rs:4

上卷 + 记账
  ├─ 子 agent：UsageTotals::drain() ⇒ (usage, Option<(hit,miss)>)，合并进 RunResult
  │     apps/coding-agent/src/app_loop.rs:1533-1543 / apps/coding-agent/src/main.rs:1924-1931
  └─ TurnStats::record_result(&RunResult)
        apps/coding-agent/src/status.rs:58

投影 → CodingView { cache_tokens, cache_hit_pct, compaction_tokens }
        apps/coding-agent/src/view.rs:118-127

渲染（TUI 不做判别、不做除法）
  ├─ 状态栏  crates/ys-tui-coding/src/draw.rs:773-797
  └─ /status crates/ys-tui-coding/src/commands.rs:742-797
```

## 回合级读数记录：`run_reading`（2026-10-29，A1）

debug 日志新增一条**段记录** `ev="run_reading"`，每回合一条、随回合收尾产生：

- **载荷**：`line`（人读的定形行，与 `-p` / TUI 的文本出口同一串）+ 顶层结构化数字字段
  `turn / rounds / tools / tpr_x100 / ttft_ms / gen_ms / tools_ms / input_med`；
  `cache_pct` 只在 `line` 里（它可能是 `n/a`，不是数字）。
- **落点**：`apps/coding-agent/src/main.rs` 的 `record_run_reading[_fields]`；
  TUI 路径（`app_loop.rs`）只拿到文本，故从 `line` 反解数字字段，键顺序与
  `RoundClock::summary_fields` 一致。
- **消费者**：`e2e_bench` 的 `Reading` 从中取 `tpr_x100` / `input_med`；`single_call_rounds`
  由同份 debug 的 `round_end.tool_calls == 1` 兜底派生（生产记录不写该键）。
- **允许名单**：`tests/debug_log.rs` 的段记录白名单已加入 `run_reading`（它随回合收尾产生，
  不是逐增量事件）。
- 逐轮 `round_end` 的既有字段（`ttft_ms` / `gen_ms` / `tools_ms` / `tool_calls` 等）不变。

## 三个用途的归因

| 用途 | 落在哪 | 是否计入 `RunFinished.usage` | 记账点 |
|---|---|---|---|
| 主对话轮 | `RunResult.usage` | ✅ 是 | `crates/ys-loop/src/basic.rs:752` |
| 压缩（自动 Step 4b + 手动 `/compact`） | `RunResult.compaction_usage` | ❌ **不含** | `crates/ys-loop/src/basic.rs:267` |
| 子 agent | 上卷进 `RunResult.usage`（含其缓存读数） | ✅ 是 | `apps/coding-agent/src/app_loop.rs:1533` |
| 工具侧摘要（`summarize.rs`） | **未接入**（见「已知缺口」） | ❌ 无数据 | — |

**三个数的关系**（与 `architecture.md` §13 同一份契约）：

- `RunFinished.usage` = 本回合**主对话轮**消耗（含已上卷的子 agent），**不含**压缩。
- `RunResult.compaction_usage` = 本回合**成功**压缩的累计；**全零 = 没压过或压失败**。
- 本回合总花费 ≈ `usage + compaction_usage`（压缩失败的漏计是已知偏差）。
- 两者是**兄弟**而非子集：`compaction_usage` 不得并入 `usage`，那会破坏 `RunFinished.usage`
  的逐字节承诺与既有消费者。

**压缩归因不透传事件消费者**：`run_compaction` / `drain_boundary` / `wait_for_subagents`
三处都加 `&mut Usage` 参数回填（依次 `crates/ys-loop/src/basic.rs:237`、`:176`、`:115`），
与 `total_usage` **同一 sink 类型、同一套 `Add`**。累计责任在 `ys-loop` 与 app 侧
`TurnStats`，**不靠** `RoundUsage{purpose}` 的事件消费者（ADR-0061 D6）。

## 命中的口径：逐轮，永远不从聚合值派生

`miss = input − hit` **只在单轮里**是恒等式。跨轮聚合不成立，因为
`input_tokens` 无条件相加、而 `hit` 缺席时不参与相加 —— **两者不对称**。

| 场景（1 轮 `(1000, Some(900))` + 1 轮 `(1000, None)`） | 命中率 |
|---|---|
| 逐轮累加（**本仓做法**） | **90%** |
| 在聚合 `Usage` 上派生 | 45%（把缺报轮的 1000 当未命中） |

故三处累加器全部**逐轮/逐次**累加，从不二次派生：

- `RunResult.cache_reading` —— loop 内逐轮累加（`crates/ys-loop/src/basic.rs:745`），
  `None` = 本回合没有任何一轮上报过。
- `TurnStats` —— `record_result` 只取 `result.cache_reading`
  （`apps/coding-agent/src/status.rs:59`）；派生只发生在**明确的单轮入口** `record()`
  （`:48`，`#[cfg_attr(not(test), allow(dead_code))]`，生产路径不走它）。
- `UsageTotals` —— `hit` 与 `miss` 在**同一条件**下累加（`apps/coding-agent/src/subagent.rs:157-160`），
  `drain()` 把 `(hit, miss)` **单独**带出（`:191`），不从聚合 `input` 派生。

`Usage::cache_split()` 的 doc 写死了这条禁令（`crates/ys-core/src/usage.rs:53`：
「**永远不要在聚合后的 `Usage` 上调本函数**」）。

## 展示契约（三态）

**`TurnStats::cache_hit_pct() -> Option<u8>`**（`apps/coding-agent/src/status.rs:115`）：

| 条件 | 结果 | 显示 |
|---|---|---|
| 一次都没上报（`cache_reported == false`） | `None` | `-` |
| 上报过但分母为 0（全零） | `None` | `-` |
| 有读数 | `Some(hit*100/(hit+miss))` | `N%` |
| `hit >= hit + miss` | `Some(100)` | `100%` |

分母恒为 `hit + miss`（**只含上报过的轮**），**不是** `input_tokens`。只有
`hit == total` 才给 100 —— 99.6% 必须显示 99（`status.rs:124`）。

**状态栏**（`crates/ys-tui-coding/src/draw.rs:768-797`）：

- 隐藏条件从「token 总量为 0」改为「**完全无读数**」：`cache_tokens.is_some() ||
  compaction_tokens.is_some() || total_input_tokens > 0 || total_output_tokens > 0`
  （`:773`）。全程不上报 usage 的会话，只要 provider 报过一次缓存也仍然显示 token 段。
- token 段两侧走 `format_tokens_or_dash`（`crates/ys-tui-coding/src/format.rs:26`）：零写 `-`。
- 缓存子段**只在有读数时追加**：有百分比 ⇒ `缓存 N%`，有读数但算不出 ⇒ `缓存 -`（`:784`）。
- 压缩段与主对话轮**并列**、单独成段，取 `input + output`（`:791`）；`None`（没压过）时省略。

**`/status`**（`crates/ys-tui-coding/src/commands.rs:731`）：

| 行 | 三态 |
|---|---|
| `tokens` | 永远出现，零写 `-` |
| `cache` | 无读数 ⇒ `- (provider 未上报)`；有读数可算 ⇒ `900 / 100（90%）`；有读数不可算 ⇒ `0 / 0（-）`（`:742`） |
| `compaction` | **仅压缩过时出现**（`None` 时整行不给）：`compaction ↑4.0k ↓600`（`:791`） |

**TUI 刻意不认识的语义**：`CodingView` 只收**已投影的显示值** ——
`cache_tokens: Option<(u32,u32)>` / `cache_hit_pct: Option<u8>` / `compaction_tokens: Option<(u32,u32)>`
（`crates/ys-tui-coding/src/view.rs:99`、`:101`、`:106`），**不收 `Usage`**。
理由不是「看不到」（`ys-tui-coding` 依赖 `ys-core`，技术上能读），而是 ①UI 线程无 runtime
上下文（它用 `blocking_send`）②provider 语义的解释权集中在 `ys-core` 一处，不撒到渲染层。
三态的拍定点在 `apps/coding-agent/src/view.rs:118-127`。

## 对外 JSON 契约

`RunFinished.usage` 的序列化形态由 `skip_serializing_if = "Option::is_none"` 决定
（`crates/ys-core/src/usage.rs:15`、`:19`、`:25`）：

| 情形 | 旧形态 | **当前形态** |
|---|---|---|
| provider 上报缓存（`Some(N)`） | `"prompt_cache_hit_tokens":N` | 同左（不变） |
| provider **未上报** | `"prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":0` | 两键**都不出现** |
| `prompt_cache_miss_tokens` | 有时出现 | **永不出现**（恒 `None`） |
| `reasoning_tokens` | 未上报时不出现 | 同左（不变） |

**这不是放宽契约，是修掉一个真实缺陷**：旧形态把「未上报」写成 `0`，消费者无法与
「真 0 命中」区分 —— 正是本需求要消灭的那类不可信。`input_tokens` / `output_tokens`
两个键永远存在，`total_tokens` 不单独保留（落进 `Usage` 时已折算为 input + output）。

**迁移提示（读 `--json` 的外部消费方）**：

1. `usage.prompt_cache_hit_tokens` **可能缺席** —— 缺席即「provider 未上报」。
   用 `opt_u32 = usage.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64())`，
   不要 `unwrap_or(0)`（那会退回旧的错误语义）。
2. 命中率的分母是 `hit + miss`，其中 `miss = input_tokens − hit`（**逐条** `usage` 内成立）。
   拿 `input_tokens` 当分母会算出错误的百分比。
3. `prompt_cache_miss_tokens` **不再出现**；若旧代码读它，改用 `input_tokens − hit`。
4. `RunFinished.usage` **不是**本回合总花费；总花费 ≈ `RunFinished.usage`
   + 所有 `RoundUsage{purpose:"compaction"}` 行的和。

golden 文件 `apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl` 存的是
**过滤后**的形态（`without_round_usage`），与 T2 的比较对象同形；`tests/debug_log.rs`
同时反向钉住「未上报时不得写出这两个键」。`run_end` 的 `usage` 走同一个 `Usage` 序列化器
（`apps/coding-agent/src/debug_log.rs:1041`），故 debug 日志同口径变化。

## 落点

```text
crates/ys-core/src/usage.rs             字段 doc（B 口径）+ Option 化 + cache_split/is_cache_reported
                                        私有 add_opt（所有 Option 桶一条规则）
crates/ys-core/src/lib.rs               跨桶同一条 Add 规则用例
adapters/model-openai-compatible/src/response.rs   ChatUsage 缓存两桶 Option 化；cache_reading()
adapters/model-openai-compatible/src/lib.rs       Usage 构造改用 cache_reading()；三态单测
crates/ys-loop/src/result.rs            RunResult +cache_reading +compaction_usage
crates/ys-loop/src/basic.rs             逐轮累加 cache_reading；run_compaction/drain_boundary/
                                        wait_for_subagents 加 &mut Usage 回填
apps/coding-agent/src/status.rs         TurnStats +cache_reported +total_compaction_usage；
                                        record_result / record_with_cache / cache_hit_pct；
                                        Copy → Clone（Usage 只 Clone）
apps/coding-agent/src/view.rs           三态投影（cache_tokens / cache_hit_pct / compaction_tokens）
apps/coding-agent/src/subagent.rs        UsageTotals +miss 原子桶 +hit_reported；
                                        add/add_run/drain 的 (hit, miss) 单独带出
apps/coding-agent/src/app_loop.rs       stats.record_result；子 agent 缓存读数上卷
apps/coding-agent/src/main.rs           同上（-p/--json 路径）
apps/coding-agent/src/round_clock.rs    turn_cache() 直接取 RunResult.cache_reading；
                                        日志未上报写 null
crates/ys-tui-coding/src/view.rs        CodingView 三个已投影字段
crates/ys-tui-coding/src/draw.rs        隐藏条件改为「完全无读数」；缓存/压缩子段
crates/ys-tui-coding/src/commands.rs    /status 的 cache 行（三态）与 compaction 行
crates/ys-tui-coding/src/format.rs      format_tokens_or_dash（draw / commands 共用）
```

**刻意不动**：`crates/ys-session/src/meta.rs`（B 口径与既有 `meta.input_tokens` 一致，
播种口径无需迁移、不升 `meta.v`）；`crates/ys-loop/src/compact.rs`（`CompactOutcome.usage`
早已存在）；`crates/ys-runtime`（`Agent` 公开面不变）；`RunFinished` 的键集合（不加
`compaction_usage`）。

## 已知缺口

1. **压缩失败路径不计成本。** `Err` 分支拿不到 `outcome.usage`
   （`crates/ys-loop/src/basic.rs:237` 起的 `run_compaction`）。故 `compaction_usage` 全零
   = **没压过 或 压失败**，两者在展示上不可区分；`has_compaction()` 同理
   （`apps/coding-agent/src/status.rs:96`）。失败的压缩可能已真实烧掉 token。
   修它要改 `CompactError` 的公开形态（ADR-0061 D5，本次不修）。
2. **`prompt_cache_miss_tokens` 是死字段。** 恒 `None`，只为老 JSONL 能反序列化
   （`crates/ys-core/src/usage.rs:20`）。删除会让老会话静默丢值。
3. **`cache_split()` 的聚合前提。** `miss = input − hit` 只在「缓存读数缺席时 `input` 也为 0」
   时才能跨轮成立。若某 provider 回 prompt 却不回缓存明细，那些轮的整个 `input` 会被算成
   未命中 ⇒ 命中率偏低。实测（本仓 4,915 条 `round_usage`）缺报轮**全部**是 `(0, 0)`，
   当前数据不触发；但这是**观察归纳，不是 provider 契约**，3 个会话存在同会话混合上报、
   `/model` 切换可跨 provider ⇒ 功能上可达。缓解是逐轮累加（见上节），不是消除前提。
4. **工具侧摘要的成本仍未接入。** `ys_tool::Summarizer::summarize` 返回
   `Result<String, String>`（`crates/ys-tool/src/summarizer.rs:15`），**不带 usage**；
   `ModelSummarizer` 拿到 `response` 后只拼文本、把 `response.usage` 丢弃
   （`crates/ys-loop/src/summarize.rs:100-117`）。接入它要改公开 trait ⇒ 需再加一个兄弟字段。
5. **子 agent 的 `cache_reading` 是逐次累加的，不是从聚合 `Usage` 派生。**
   `UsageTotals::drain()` 同时返回 `(Usage, Option<(u32,u32)>)`
   （`apps/coding-agent/src/subagent.rs:175`），上卷时与回合自身的 `cache_reading` 相加
   （`apps/coding-agent/src/app_loop.rs:1538` 与 `apps/coding-agent/src/main.rs:1926`）。故这条路径**不受**缺口 3 影响，
   但反过来也不能用 `RunResult.usage` 复算它。
6. **`TurnStats` 不再 `Copy`。** `total_compaction_usage: Usage` 只实现 `Clone`
   （`apps/coding-agent/src/status.rs:7`）；调用方按引用传，别处若依赖 `Copy` 需改成 `clone()`。

## 测试覆盖

全部用例基于**构造读数**，不依赖真实 provider。

| # | 注入的 bug | 抓它的用例 | 位置 |
|---|---|---|---|
| **M1** | `cache_split()` 把 `None` 当 `(0, input)` | `cache_split_returns_none_without_reading` —— 未上报必须是 `None`，否则上层把「测不到」渲染成 `0%` | `crates/ys-core/src/usage.rs:184` |
| **M2** | `miss` 不用 `input − hit`（如恒 0） | `cache_split_derives_miss` —— `input=100, hit=98` ⇒ `(98, 2)`，恒 0 会让命中率永远 100% | `crates/ys-core/src/usage.rs:198` |
| **M3** | `run_compaction` 只 emit 不 `+= outcome.usage` | `compaction_usage_has_compaction_purpose_and_summary_usage` —— `compaction_usage` 变零值 | `crates/ys-loop/src/basic.rs:1860`（断言在 `:1892`） |
| **M4** | `RunResult.usage` 并入 `compaction_usage` | 同上用例的后两条断言 —— `usage` 只含主对话轮、两者必然不同 | `crates/ys-loop/src/basic.rs:1896` |
| **M5** | `Some(0)` 被折叠成 `None` | `cache_hit_pct_zero_is_distinct_from_unreported` + `streamed_usage_with_explicit_zero_cache_is_reported` | `apps/coding-agent/src/status.rs:331`、`adapters/model-openai-compatible/src/lib.rs:2265` |
| **M6** | 「`hit == total` 才 100%」改成四舍五入 | `cache_hit_pct_exact_only_at_full` —— 99.6% 必须是 99 | `apps/coding-agent/src/status.rs:414` |
| **M7** | `TurnStats` 在回合聚合 `Usage` 上算 `input − hit` | `cache_hit_pct_accumulates_per_round_not_per_turn` —— 90% 跌到 45% | `apps/coding-agent/src/status.rs:387` |
| **M8** | `UsageTotals::drain` 的第二个返回值改成 `usage.cache_split()` | `usage_totals_cache_readings_accumulate_per_call` —— 子 agent 的 `(hit, miss)` 必须逐次累加 | `apps/coding-agent/src/subagent.rs:1711` |
| **M9** | 删掉 `run_turn` 里 `round_usage.cache_split()` 的累加 | `cache_reading_accumulates_per_round_not_from_total` —— `cache_reading` 变 `None` 或错成 `(900, 1100)` | `crates/ys-loop/src/basic.rs:1669` |

**其余钉住的行为**：

- `add_opt` 的六种组合（含 `Some(0) + None` 不折叠）—— `crates/ys-core/src/lib.rs:102`
  `test_usage_add_shares_one_rule_across_option_buckets`；缓存桶随 `Add` 累加 —— `:73`。
- 两方言归一（DeepSeek / OpenAI / 都没有 / 只报 miss）——
  `adapters/model-openai-compatible/src/response.rs:147` `cache_reading_handles_both_dialects`。
- 三态投影（无读数 / 真 0 / 有百分比）——
  `apps/coding-agent/src/view.rs:465` `test_build_view_projects_cache_three_states`；
  压缩投影 `:531` `test_build_view_projects_compaction_tokens`。
- 状态栏三态（隐藏条件、`↑- ↓-`、`缓存 -`、`压缩 4.6k`）——
  `crates/ys-tui-coding/src/draw.rs:2431`、`:2443`、`:2458`、`:2470` 四条用例。
- `/status` 的 `cache` 行三态与 `compaction` 行「仅压缩过时出现」——
  `crates/ys-tui-coding/src/commands.rs:1273` `status_text_reports_cache_readings_and_unreported`、
  `:1337` `status_text_compaction_line_only_when_compacted`。
- 日志侧「未上报 ≠ 0 命中」—— `apps/coding-agent/src/round_clock.rs:709`
  `turn_cache_separates_unreported_from_zero_hit`。
- `--json` 契约的逐字节断言与「未上报不得写键」——
  `apps/coding-agent/tests/debug_log.rs:862`（T2 用例 `t2_json_is_additive_and_filtered_output_matches_baseline_golden`，`raw` 自 `:834`）与 golden 文件。

## 文件清单

| 文件 | 变更 |
|---|---|
| `crates/ys-core/src/usage.rs` | 字段 doc（B 口径）、缓存两桶 `Option<u32>`、`cache_split` / `is_cache_reported`、私有 `add_opt`、M1/M2 与守恒/饱和用例 |
| `crates/ys-core/src/lib.rs` | 既有用例改 `Some(_)`；新增「三个 Option 桶同一条 Add 规则」 |
| `adapters/model-openai-compatible/src/{response,lib}.rs` | `cache_tokens()` → `cache_reading()`；判据 `> 0` → `is_some()`；只报 miss 时反推 hit；三态单测 |
| `crates/ys-loop/src/result.rs` | `+cache_reading`、`+compaction_usage`（各带「不要从聚合派生」「成功限定」doc） |
| `crates/ys-loop/src/basic.rs` | 逐轮累加缓存读数、压缩回填、三处签名透传、M3/M4/M9 用例 |
| `apps/coding-agent/src/status.rs` | `TurnStats` +两个字段、`record_result` / `record_with_cache`、`cache_hit_pct`；M5/M6/M7 |
| `apps/coding-agent/src/view.rs` | 三态投影 + 用例 |
| `apps/coding-agent/src/subagent.rs` | `UsageTotals` +`miss` / `hit_reported`、`add` / `add_run` / `drain`；M8 |
| `apps/coding-agent/src/{app_loop,main}.rs` | `record_result`；子 agent 缓存读数上卷（两处） |
| `apps/coding-agent/src/round_clock.rs` | `turn_cache()` 取 `cache_reading`；日志未上报写 `null` |
| `crates/ys-tui-coding/src/{view,draw,commands,format}.rs` | `CodingView` 三字段、隐藏条件改「完全无读数」、`/status` 两行、`format_tokens_or_dash` |
| `apps/coding-agent/tests/{debug_log.rs,golden/*.jsonl}` | 反向断言 + golden 更新到新形态 |
