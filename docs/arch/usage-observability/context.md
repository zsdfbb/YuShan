# usage-observability — 架构上下文

> 日期：2026-10-28
> 性质：`arch-explore` 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起：对照 `tmp/deepseek-harness` 的 `packages/llm/token-meter` 之后，把此前讨论出的三项
> 「成本相关」改进合并成**一个需求**：三者本质相同 —— 都是**让已经采到的用量数据变得可信、
> 可见、可归因**，而不是新增任何省钱算法。

## 概述

把「本次会话用掉多少、其中多少是免费的、其中多少是压缩烧的」这三件事，从
**只存在于日志里的死数据**，变成**有确定语义、能进界面、能跨会话恢复**的观测面。

一句话：**不改变任何省钱策略，只让读数可信且可见。**

## 现有架构

### 模块边界

用量从 provider 到屏幕跨五层，每一层的职责与当前状态：

| 关注点 | 层 | 关键文件 | 现状 |
|---|---|---|---|
| provider 方言归一 | adapter | `adapters/model-openai-compatible/src/response.rs:53`（`cache_tokens()`） | ✅ 两种方言都认 |
| 用量词汇 | 核心（零 tokio） | `crates/ys-core/src/usage.rs:4`（`Usage`） | ⚠️ 字段语义未写全（见 P0-1） |
| 逐轮上报 | loop | `crates/ys-loop/src/basic.rs:720`（`RoundUsage`） | ✅ 每轮一条，带 `purpose` |
| 回合累计 | loop | `crates/ys-loop/src/basic.rs:427`（`total_usage`） | ⚠️ **只累计主对话轮**（见 P0-3） |
| app 侧累计 | app | `apps/coding-agent/src/status.rs:28`（`TurnStats::record`） | ✅ 缓存桶已累加（`:31`、`:35`） |
| 进 View | app → TUI | `apps/coding-agent/src/view.rs:114` | ❌ **只推 ↑↓ 总量** |
| 渲染 | TUI | `crates/ys-tui-coding/src/draw.rs:768`、`commands.rs:734` | ❌ 无缓存段；总量为 0 整段隐藏 |
| 落盘 | 持久化 | `crates/ys-session/src/meta.rs:52-56` | ❌ **meta 只有 input/output/turns** |
| 结构化日志 | app | `apps/coding-agent/src/round_clock.rs:236-290` | ✅ **唯一完整的一处**（带 `purpose`） |

> 关键观察：**`round_clock.rs` 的调试日志已经是完整的**——它逐轮记 `purpose`、
> `gen_ai.usage.cache_read.input_tokens`、`reasoning_tokens`（未上报写 `null`）。
> 断裂全部发生在**日志之外**：进 View、进状态栏、进 meta、进 `RunResult`。

### 核心抽象

**`Usage`**（`crates/ys-core/src/usage.rs:4`）—— 逐轮与累计两用：

```rust
pub input_tokens: u32,        // :5  ← 语义未定义（P0-1）
pub output_tokens: u32,
#[serde(default)] pub prompt_cache_hit_tokens: u32,   // :12
#[serde(default)] pub prompt_cache_miss_tokens: u32,   // :15
#[serde(default, skip_serializing_if = "Option::is_none")] pub reasoning_tokens: Option<u32>, // :21
```

`reasoning_tokens` 已经是 `Option`，注释（`:18`）明确「`None` = 未上报；`Some(0)` = 上报了真 0」。
**两个缓存字段没有遵循同一条已在本仓确立的规则** —— 这是 P0-1 的诱因之一。

**`RoundPurpose`**（`crates/ys-event/src/event.rs:8`）—— `#[non_exhaustive]`，当前 `Turn` / `Compaction`。
`RoundUsage{purpose}`（`:60`）是**唯一**能区分「主对话花的」与「压缩花的」的信号，
且它**不影响**消费循环退出条件（非终局）。

**`TurnStats`**（`apps/coding-agent/src/status.rs:6`）—— app 侧累加器：
已含 `total_cache_hit_tokens` / `total_cache_miss_tokens`（`:11`、`:13`）与
`total_reasoning_tokens: Option<u32>`，`cumulative_usage()`（`:56`）能原样吐出四个桶 + Option。

**`CodingView`**（`crates/ys-tui-coding/src/view.rs:83`）—— TUI 只读快照：
`total_input_tokens` / `total_output_tokens`（`:93`、`:94`），**无任何缓存字段**。
TUI 不依赖 `ys-runtime`，靠 app 推送，故加字段 = app 侧一处 + TUI 侧两处。

### 关键数据流

**P0-1 · 语义分歧点（先看这个）**

```text
SSE 末包  prompt_tokens = 9760, cached_tokens = 9728
  └─► ChatUsage::cache_tokens()  →  (hit 9728, miss 32)
      └─► lib.rs:1079  Usage {
            input_tokens: prompt_tokens,   // = 9760，**含**缓存命中部分
            prompt_cache_hit_tokens: 9728,
            prompt_cache_miss_tokens: 32,  // 三者不 disjoint：hit + miss == input_tokens
          }
```

而 OpenAI 形状下 `miss` 是**推导**出来的（`response.rs:57-60`：`prompt_tokens - cached`），
所以「`input_tokens` 含缓存」这件事**在两种方言下都成立**，但它**没有任何一处文档写过**：
`usage.rs` 不提，`docs/design.md` / `docs/architecture.md` 里搜 `input_tokens` 只有 JSON 形状的列举。
⇒ 任何消费方想算命中率，都在**猜分母**。

**P0-2 · 送到屏幕**

```text
TurnStats.record()            status.rs:28
  └─► 已累计 total_cache_hit/miss_tokens（:31、:35）
      └─► ✗ coding view.rs:114-115 只推 total_input/output_tokens
          └─► ✗ CodingView（tui view.rs:93-94）无缓存字段
              └─► ✗ draw.rs:768 总量为 0 时整段不渲染；commands.rs:734 只拼 ↑↓
```

同一份判别逻辑**已经写好但只服务于日志**：`round_clock.rs:429` 的 `turn_cache()`
把「用量存在但缓存两字段为 0」判为 `None`（**不是** 0% 命中），并在 `turn_line()`（`:447`）
里渲染成 `-`。P0-2 要做的就是把这条判别**从日志层挪成公共函数**。

**P0-3 · 压缩成本漏账（最实质的一处）**

```text
run_compaction(ctx, rounds)                       basic.rs:235
  ├─ 拿到 outcome.usage（摘要调用的真实 provider usage）
  ├─ emit RoundUsage{ purpose: Compaction, ... }  basic.rs:262-271   ← 只进事件流
  └─ ✗ 从不碰 run_turn 的局部变量 total_usage（basic.rs:427）

主对话轮：total_usage = total_usage + response.usage   basic.rs:719
RunFinished{ usage: total_usage }                      ← 因此**不含**压缩

app 侧：stats.record(&result.usage)                    app_loop.rs:514 / main.rs:1754
                                                   ← 因此同样不含压缩
```

⇒ **只要发生过压缩，`RunFinished.usage`、TUI 累计、`meta.json` 三处的数字都偏小**，
偏小的量恰好是最想衡量的那部分（摘要要重发被压缩的历史，是输入侧的贵调用）。
今天唯一能看到它的地方是 `--debug` 日志里的 `round_usage{purpose: "compaction"}`。

> ⚠️ 与 `event.rs:73-82` 的注释存在**张力**：那里说 `ModelStreamDiscarded` 不能借用
> `RoundUsage`，理由是「那个变体会进 usage 累计」。但实际上 `RoundUsage` 事件
> **并不**进 `total_usage`（只有 `response.usage` 进）。该注释描述的是**消费端的预期**，
> 不是 loop 的现状 —— 设计阶段须澄清「`RoundUsage` 是否承诺被累计」，因为它现在是灰的。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `serde` | `Usage` / `meta` / 事件 JSON | 新字段一律 `#[serde(default)]` |
| provider 的 `stream_options.include_usage` | 是否上报 usage | **minimax 当前关闭**（`compat.rs:79`，注释：「未知顶层字段是否放行未经验证」），该家 usage 恒为 0 |
| `ys-core` 零 tokio | 词汇层约束 | 改动必须是纯数据 / 纯函数 |

## 约束

- **技术**
  - `ys-core` / `ys-event` / `ys-protocol` **不含 tokio**（`AGENTS.md`）⇒ `Usage` 改动只能是纯数据。
  - **`ys-tui-coding` 不依赖 `ys-runtime`**（编译器强制）⇒ TUI 侧的缓存字段必须由 app 经
    `CodingView` 推入，TUI **不能**自己去读 `Usage`。
  - **UI 线程必须无 runtime 上下文**（`blocking_send`）⇒ 任何「向 UI 异步请求」的设计都不可行。
  - `round_clock.rs` 的 `turn_cache()` 是 `pub(crate)`（`apps/coding-agent`）⇒ 复用它不需要改 API，
    但**若要在 TUI 侧显示同样的判别，判别必须先在 app 侧完成**（符合上一条）。
- **语义（本需求最要紧的约束）**
  - **P0-1 未定则 P0-2 必错**：命中率的分母取决于 `input_tokens` 是否含缓存。
  - 缓存字段若改 `Option<u32>`（`docs/arch/cost-optimization/context.md` 已提议），
    **累加语义必须照 `reasoning_tokens` 的既有规则**（`usage.rs:33-39`：两侧都上报才相加，
    否则继承已上报的一侧）。**不得**折叠 `Some(0)` 成 `None`。
  - `#[serde(default)]` 的旧 JSON（老会话、老日志）必须仍能反序列化。
- **行为兼容**
  - `RunFinished.usage` 的 JSON 形态有逐字节断言（`apps/coding-agent/src/main.rs:5835`、
    `:5843`：`reasoning_tokens: None` 时该键**不出现**）。P0-3 **不得**改变该字段的含义，
    否则破坏既有消费者与这些断言。
  - `docs/design-final/skills.md:244` 的 R8 对账（真实/估算 = 0.704）以 `Usage.input_tokens`
    为基准实测。**P0-1 若选「input 不含缓存」，该标定即失效**，须重跑
    `e2e_skills`（`cargo test -p ys-coding-agent --test e2e_skills -- --ignored`）。
  - `meta.json` 的 `input_tokens` 是**跨会话播种值**（`main.rs:1254-1262` → `seed_stats`）。
    语义一改，老会话恢复后 `total_input_tokens` 与新会话**不同口径**，需决定是否迁移。
- **组织**
  - 本需求**不含任何省钱策略变更**（不改阈值、不改 `compact` 触发、不加价格表）。

## 需求范围

### 范围内

**P0-1 · 定义 `input_tokens` 与缓存桶的关系（**前置，其余两项依赖它**）**

- 二选一，并写进 `crates/ys-core/src/usage.rs:4-22` 的字段 doc：
  - **A（对齐生态）**：`input_tokens` = **未缓存**输入；适配器做 `prompt_tokens - cached`
    （`lib.rs:1079`）。三个桶 disjoint，`billed = input + hit + miss`。
  - **B（现状显式化）**：`input_tokens` = **含缓存**的 prompt 总量；写明
    「`hit + miss` 是它的缓存明细；两者皆 0 = 未上报」。展示分母用 `hit + miss`。
- 无论 A / B，都要在 `usage.rs` 补一条不变量注释 + 一组断言该不变量的用例
  （上报时 `hit + miss == input_tokens`，或 A 下 `input + hit + miss == prompt`）。
- `reasoning_tokens` 已有 `Option` 先例 ⇒ 顺带决定缓存两字段是否也改 `Option`
  （与 `cost-optimization` 第 1 项的重叠，见「跨需求重叠」）。

**P0-2 · 把缓存读数送到界面（数据已在内存，缺的是两跳）**

- `CodingView` 增加缓存命中 / 未命中字段（`view.rs:93` 旁），由 `view.rs:114` 从 `TurnStats` 透传。
- `/status`（`commands.rs:734`）增加缓存一行；状态栏（`draw.rs:768`）**不再因总量为 0 而隐藏**
  —— 「没有读数」与「读数是 0」必须显示成两种不同的东西（前者 `-`，后者 `0%`）。
- 把 `round_clock.rs:429` 的「未上报」判别抽成 app 侧公共函数，TUI 只渲染 app 给的结论。
- 命中率分母取 P0-1 的结论，**不得**写死 `input_tokens`。

**P0-3 · 让压缩成本可归因**

- 目标：`RunFinished.usage` 之外，有一条能读到「本回合压缩烧了多少」的路径。
- 候选形态（**留给 `arch-design` 选**）：
  - **(a)** `RunResult` 加 `compaction_usage: Option<Usage>`（`crates/ys-loop/src/result.rs:4`），
    `run_compaction` 通过 `&mut` 参数回填；`RunFinished` 加一个可选的
    `compaction_usage` 键（`skip_serializing_if` ⇒ 无压缩时 JSON 逐字节不变）。
  - **(b)** app 侧在事件流里按 `purpose` 分流累计（`round_clock.rs:236` 已有该信号；
    `app_loop` 也已在转发事件）。
  - **(c)** 把压缩 usage 并入 `total_usage` —— **建议否决**：会让 `RunFinished.usage`
    的含义从「本回合模型轮消耗」漂移成「本回合所有花费」，破坏既有消费者与 JSON 断言。
- 落点要同时覆盖 **TUI `/status`** 与 **`--json`**；`meta.json` 是否也持久化压缩成本见未澄清问题。

### 范围外（明确不做的）

- **价格表 / 货币换算 / 金额中止**。理由：价格随 provider 调整，硬编码必过期；
  且本仓没有余额查询接口（`tmp/deepseek-harness` 的 `balance` 走它自家 Platform 服务）。
- **`contextPressure` / O(1) projection fold 那套基础设施**。理由：那是为
  「持久化事件日志 + 冷启动重建 + 客户端按需重放」付的复杂度；本仓 session 就在内存里
  （`Session::messages() -> &[Message]`），直接算即可。
- **用启发式估算当账单展示**。`ys-loop/src/token.rs` 的 ASCII/4 + CJK×1.5 对 JSON schema、
  URL、代码块的偏差很大；它只配做**触发判定**。
- **改压缩阈值 / 触发点 / 保留比例**。归 `docs/arch/cost-optimization/` 第 2 项。
- **上下文组成拆分**（system / tools / messages 各占多少）。价值高，但**依赖 P0-1 的正确口径**，
  见「后续建议」。

### 关键场景

- **场景 1（缓存生效）**：DeepSeek 方言，第二轮起 `hit = 9728`、`miss = 32`。
  状态栏显示「缓存 99%」（或「`↑9.7k ↓320 · 缓存 99%`」），`/status` 显示命中/未命中原始数。
- **场景 2（provider 不上报）**：minimax（`compat.rs:79` 关闭 `include_usage`）⇒ usage 全 0。
  界面显示「无读数 `-`」，**不得**显示 `0%`。
- **场景 3（首调用真 0 命中）**：OpenAI 形状显式 `{"cached_tokens":0}` ⇒ `(0, 9760)`。
  界面显示 `0%`（与场景 2 必须**视觉可区分**）。
- **场景 4（压缩花了钱）**：一个回合中途触发压缩。`/status` 与 `--json` 都能看到
  「压缩 X token」独立于「主对话 Y token」；两者之和与 `--debug` 日志里
  `purpose` 分组的求和一致。
- **场景 5（跨会话恢复）**：`--resume` 一个发生过压缩的会话。缓存累计（若已持久化）
  与压缩成本要么都恢复、要么都在 `/status` 明示「本会话统计」，**不得**静默显示成 0。

### 异常 / 边界

- **`hit + miss != input_tokens`**（provider 只报其中一侧，或只报 `total_tokens`）：
  命中率要么不显示，要么显式标注分母来源。**不得**用 `hit / input_tokens` 蒙一个数。
- **`input_tokens = 0` 而 `hit + miss > 0`**（`round_clock.rs:276-286` 已列出此形态）：
  属于 provider 异常上报，按「无读数」处理。
- **压缩失败**（`ContextCompactionFailed`）：摘要调用可能已真实消耗 token。
  `basic.rs` 的现状是**失败不上报**（只在 `Ok` 分支 emit）。需要决定失败路径是否也有成本口径。
- **`ModelStreamDiscarded`**（`event.rs:80`）：零消耗，**绝不能**混进任何用量桶。
- **子 agent 用量上卷**（`main.rs:1921-1924`）：`r.usage = r.usage + totals.drain()` ——
  压缩成本若加进 `RunResult`，子 agent 路径也要覆盖，否则父子口径不一致。

## 跨需求重叠（设计阶段必须先决定归属）

| 既有需求 | 重叠点 | 建议 |
|---|---|---|
| `docs/arch/cost-optimization/context.md` 范围内第 1 项 | 「`CodingView` 增加缓存字段 + `draw.rs` 不再整段隐藏」与本需求 **P0-2 几乎是同一件事** | **合并**：P0-2 视为该项的细化（多出「口径定义」「未上报判别公共化」两点）。避免两处各做一半 |
| 同上，`Usage` 缓存字段改 `Option<u32>` | 与 P0-1 的同一条改动 | 归 **P0-1** 决策，cost-optimization 引用本结论 |
| 同上，`minimax` 打开 `supports_stream_usage` | P0-2 的「无读数」场景直接由它决定 | 归 cost-optimization（它已记录安全网：`lib.rs:603` 剥参重发） |
| `docs/arch/agent-observability/` | 逐轮 usage 已由 `round_clock.rs` 落地 | **无重叠**，该需求已完成的部分即本需求的「日志侧已完整」前提 |
| `docs/arch/tui-status-display/` | 状态栏形态（`context.md:87-88` 已列「cache hit % ❌ 无」） | 本需求补数据，**形态沿用**该需求既有结论，不重开 |

## 未澄清问题

- [ ] **P0-1 选 A 还是 B？** 倾向 **B**（`input_tokens` 保持「含缓存总量」）：
      改动只落在注释与展示层，不触碰适配器、`meta.json` 播种口径、
      以及 `docs/design-final/skills.md:244` 的 R8 标定。选 A 则上述三处都要重跑 / 迁移。
      **待用户确认。**
- [ ] **缓存两字段是否也改 `Option<u32>`？** 与 A/B 正交，但两者一起做能省一次格式变动。
      若只做展示、不做 `Option`，则「未上报」只能靠「两字段皆 0 且 input > 0」推断 ——
      该推断在「真 0 命中且首调用 input = 0」时失效（`round_clock.rs:276-286` 已列此坑）。
      **待用户确认是否值得为它改 `Option`。**
- [ ] **压缩用量是否持久化进 `meta.json`？** 不持久化 ⇒ `--resume` 后压缩成本归零，
      与 `total_input_tokens`（已播种）口径不一致。持久化 ⇒ `meta` 新增两三个键，
      老会话读到 0，需在 `/status` 区分「本会话累计」与「历史累计」。
      **待用户确认。**
- [ ] **`RoundUsage` 是否承诺「被累计」？** `event.rs:73-82` 的注释暗示承诺，但 loop 现状不满足
      （P0-3 的前提）。这是**契约级**问题：若不承诺，注释要改；若承诺，P0-3 就必须修。
      **待用户确认取哪一种。**
- [ ] **压缩失败路径要不要成本口径？** 见「异常 / 边界」最后一条。**待用户确认。**
- [ ] **P0-3 选 (a) `RunResult` 回填 / (b) app 侧事件分流？** (b) 不动 `ys-loop` 的公开类型，
      但每个 app 消费者都要自己实现一遍；(a) 一处修好、所有消费者受益，但要改 `RunResult`
      与 `RunFinished`。**留给 `arch-design` 对比，但需要用户先给偏好方向。**

## 后续建议

- **顺序是硬约束**：P0-1 → P0-2 / P0-3（后两者互相独立，可并行派发。
  **P0-2 必须在 P0-1 之后**，否则命中率分母靠猜；P0-3 与 P0-1 无依赖，可先做）。
- 用 `arch-design` 做方案设计，建议**拆成两个设计单元**：
  ① `Usage` 语义 + 展示面（P0-1 + P0-2，同一条数据链）；
  ② 压缩成本归因（P0-3，动 `ys-loop` 的公开类型，风险更高）。
- 用 `arch-validate` 复核 P0-3 对 `RunFinished` / `RunResult` / 子 agent 上卷 /
  `--json` 逐字节断言的影响面。
- **关键逻辑要做变异测试**（`AGENTS.md` 硬性要求）：
  - 删掉 `round_clock.rs:429` 的「未上报」判别 ⇒ 场景 2 必须变红（显示成 `0%`）。
  - 把命中率分母从 `hit + miss` 改回 `input_tokens` ⇒ 场景 1 必须变红。
  - 把压缩 usage 从归因路径删掉 ⇒ 场景 4 必须变红（压缩计数归 0 而非静默通过）。
- **建议先落 P0-1 + P0-2，跑若干真实会话**，用 `cache_miss` 的分布与
  「未上报比例」来支撑 `cost-optimization` 里悬着的 `cap` 缺省值 ——
  那项目前是「拍一个数」，本需求能给它第一手数据。
- 若本需求落地，「上下文组成拆分」（system / tools / messages）可接着单独立项 ——
  它能把 `RunLimits::fixed_prompt_tokens`（`apps/coding-agent/src/main.rs:1089` 的手工派生值）
  从「调用方必填的魔数」降级为「兜底覆盖值」。
