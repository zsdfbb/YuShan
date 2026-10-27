# effort-per-call-type — 架构上下文

> 性质：**arch-explore 产出**（现状 + 约束 + 需求澄清）。方案设计留给 `arch-design`。
> 日期：2026-10-27
> 上游材料：`docs/reports/2026-10-27-reasoning-effort-comparison.md`（跨产品对照）、
> `docs/design-final/effort.md`（本仓 as-built）、`docs/adr/0042-default-reasoning-effort-high.md`（默认值决策）。

## 概述

本仓的推理强度（`reasoning_effort`）是**进程级单值**：整个进程所有模型调用共用一个档位。
本需求要让档位成为**每次调用**的属性 —— 不同调用类型（主轮 / 压缩摘要 / 工具摘要 / 子 agent）
可以用不同档位。

## 现有架构

### 模块边界

档位存在**三层**，只有最外层可配：

| 层 | 承载物 | 可配？ | 证据 |
|---|---|---|---|
| 配置层 | `Config.effort`（`/effort` 覆盖）→ `Config.reasoning_effort`（配置文件/env 默认） | ✅ | `apps/coding-agent/src/config.rs:358`（`effective_reasoning_effort`） |
| 适配器配置层 | `OpenAICompatibleConfig.reasoning_effort: Option<String>` | 构造期定死 | `adapters/model-openai-compatible/src/lib.rs:48` |
| wire 层 | `ChatCompletionRequest.reasoning_effort` | 只读 | `adapters/model-openai-compatible/src/request.rs:24` |

**关键结构事实：`ModelRequest` 里没有 effort 字段。** 档位在**构造适配器时**被冻结进
`OpenAICompatibleConfig`，之后每次 `complete()` 都用同一个值。

### 核心抽象

```rust
// crates/ys-model/src/trait_def.rs:8-18
pub trait Model: Send + Sync {
    fn model_id(&self) -> &str;
    async fn complete(&self, request: ModelRequest, sink: &mut dyn ModelEventSink)
        -> Result<ModelResponse, ModelError>;
}
```

```rust
// crates/ys-model/src/request.rs:9-27
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(skip, default)]
    pub boundary: Option<Arc<dyn BoundarySource>>,  // 已有「不序列化的边信道」先例
}
```

`boundary` 是**同类问题的既有解法**：一个不参与序列化、不进 JSONL 会话契约、只在一次调用内
生效的运行期钩子。加 effort 覆盖可以照它的形态，而不动 `Model` trait 的签名。

### 关键数据流

档位从配置到 wire 的唯一路径（**每个 model 实例只有一条**）：

```
Config::effective_reasoning_effort()          // 会话覆盖优先，否则配置默认
  → reasoning_effort_for(compat, model, v)    // main.rs:1065，转发到 compat
  → ProviderCompat::effort_for(model, v)      // compat.rs:162，两层门控 + 方言折叠
  → OpenAICompatibleConfig.reasoning_effort   // 构造期写入，之后不变
  → ChatCompletionRequest.reasoning_effort    // lib.rs:1328，每次请求读出同一个值
```

**生产环境的 `Model::complete()` 调用点只有 3 处**（其余全是测试）：

| 调用点 | 场景 | 现用档位 |
|---|---|---|
| `crates/ys-loop/src/basic.rs:634` | 主循环的模型轮 | 全局值 |
| `crates/ys-loop/src/compact.rs:365` | 上下文压缩的摘要 | 全局值（**同一个 `ctx.model` 实例**） |
| `crates/ys-loop/src/summarize.rs:87` | 工具侧摘要（webfetch / subagent） | 全局值（**同一个 `ctx.model` 实例**） |

后两处与主轮**共用同一个 `Arc<dyn Model>`**（`basic.rs:1011` 的 `ModelSummarizer { model: ctx.model }`、
`basic.rs:244` 把 `ctx.model` 传给 `compact::execute`）。这正是「进程级档位」的结构根源：
**只要档位住在 model 实例里，共用实例的三类调用就不可能分档。**

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `ys-core::EffortLevel` | 档位枚举（7 档）+ `as_wire()` | `crates/ys-core/src/effort.rs:17`，不含 tokio |
| `ProviderCompat` | 能力门控 + 方言折叠 | `compat.rs`，GLM / MiMo / Verbatim 三方言 |

## 同类实现怎么做（源码直读）

> 方法：直读本机 `tmp/pi`（TS monorepo）与 `tmp/codewhale`（Rust workspace）源码，逐条带行号。
> Codex **无源码**，只有配置产物（`~/.codex/config.toml` 与模型目录 JSON），只能佐证其档位模型。

### pi：档位是**函数参数**，逐层传

档位从不往全局/单例上放，而是作为**形参**从调用点传到最后一个 wire 选项：

```ts
// packages/ai/src/api/simple-options.ts:57-62
// 档位不是枚举值，而是**具体的 token 预算**
export const DEFAULT_THINKING_BUDGETS = {
  minimal: 1024, low: 2048, medium: 8192, high: 16384,
};
export const MIN_ANSWER_TOKENS = 1024;   // 保证答案至少留 1024
```

分场景的实证：

| 场景 | 做法 | 证据 |
|---|---|---|
| 主对话 | `createLoopConfig` 把会话档位放进 config，每轮 `...config` 摊进 options | `agent/agent.ts:445-455`、`agent-loop.ts:306-309` |
| 压缩摘要 | **不降档**：把**当前会话档位原样传下去**（`thinkingLevel` 作形参逐层下传） | `harness/compaction/compaction.ts:586-590`；`runtime/drive/structural.ts:876` |
| 压缩摘要（关思考） | `thinkingLevel !== "off"` 才设 `options.reasoning` —— **关闭时整个字段不发** | `compaction.ts:586-590` |
| 子 agent 派发 | 仅当「未显式指定模型」且父有档位时，才把 `--thinking <level>` 拼进子进程命令行 | `examples/extensions/subagent/index.ts:302-306` |
| 逐轮改档 | 唯一入口是 `prepareNextTurn` 钩子（不是按调用类型分档） | `agent/agent-loop.ts:180-188` |
| eval / 基准 | 直接 `"off"` | `evals/src/pi-harness.ts:148` |

三条断言钉住了压缩的**传递**行为（`test/compaction-summary-reasoning.test.ts`）：
推理模型 → 带**调用方给的**档位（`:76,94`）；`off` → 不带 `reasoning` 字段（`:208,225`）；
非推理模型 → 不带 `reasoning` 字段（`:228,245`）。

> ⚠️ **两处纠正**（均经源码复核）：
>
> 1. `docs/reports/2026-10-27-reasoning-effort-comparison.md` §1 写「compaction 尊重 `thinkingLevel`」
>    —— 字面没错，但容易被误读为「压缩有自己的一套分档规则」。**实际是原样透传当前会话档位**，
>    用户设 `high` 则摘要也 `high` —— 与本章关心的问题（压缩该不该降档）**恰好相反**。
>    全仓无任何「压缩特设低档」的常量（grep `thinkingLevel: "off"|"low"` 仅命中测试）。
> 2. 同文说「子 agent 传 `--thinking`」—— 属实，但那是 **pi 自己的 CLI 顶层参数**
>    （`cli/args.ts:147,301`），由子进程启动时重新解析，**不是**子 agent 协议字段。
>    且只在**继承派发配置**时才传（指定了 `agent.model` 就不传）。

### pi 的另一可借之处：档位是受模型能力约束的

`clampThinkingLevel(model, level)` 在**构造会话时**就把不可用的档位折到可用区间
（`coding-agent/src/core/sdk.ts:245-256`；无模型时直接 `"off"`）。
本仓的对应机制在**适配器侧**（`compat.effort_for` 的两层门控 + 方言折叠）—— 位置更晚、
但同样 fail-closed。两者语义等价，只是切分点不同。

### codewhale：错误值**类型上不可表示**

codewhale 没有统一的调度器，但「让不该发生的值根本无法写出」这一点做得最硬：

```rust
// crates/workflow/src/reasoning_router.rs:61-73
/// The reasoning a **Router call itself** may run at.
/// Deliberately not `ReasoningTier`: this type exists precisely so that
/// `medium`/`high`/`max` are unrepresentable.
pub enum RouterCallReasoning { #[default] Off, Low }
```

三层强制，不是单一机制：

| 层 | 机制 | 位置 |
|---|---|---|
| 类型 | 窄枚举，`medium` 以上**没有变体** | `reasoning_router.rs:67-73` |
| 解析 | 显式报 `CallReasoningTooExpensive` 错误，**不钳制** | `reasoning_router.rs:97-113` |
| 提升 | `tier()` 只能产出 `Off` / `Low` | `reasoning_router.rs:86-91` |

错误文案把「拒绝静默替换」写成了契约：
> *"This is rejected rather than clamped: an operator who wrote `high` asked for something this
> service will not do, and quietly running at `off` while the file says `high` is exactly the kind
> of invisible substitution receipts exist to prevent."*

默认值常量确实在 `fleet_reasoning.rs:285`：`pub const ROUTER_CALL_REASONING: RouterCallReasoning = Off;`

⚠️ **另一处要点**：codewhale 的档位枚举**按层次分裂成三套** —— workflow 契约 `ReasoningTier`（5 档）、
TUI 界面 `ReasoningEffort`（**9 档**，含 `auto`，**默认 `max`**，`reasoning_preference.rs:22-34`）、
收据层 `ReasoningEffortTier`（11 档）。三者靠 `parse()` / 别名表手工对齐（`deepseek_effort.rs:83-100`
把 `medium`→`High`、`xhigh`/`ultra`→`Max`）。⇒ **按类型分档的代价就长在这里**。

### codex：只有全局单值（可作反例）

`~/.codex/config.toml` 只有一行 `model_reasoning_effort = "high"`；模型目录 JSON 也只给
`default_reasoning_level` / `supported_reasoning_levels`（实测该端点 3 档：`low`/`high`/`max`）。
⇒ **codex 在本文关心的这个维度上与本仓现状同构**（一个进程一个值）。

### Claude Code：按场景**换模型**，不是换档位

这是与前三家都不同的第三个轴。Claude Code 同时有「模型档位」与「思考档位」两套旋钮。

**模型档位**：三个内建档位，各对应一个可覆盖的模型 id：

```
ANTHROPIC_DEFAULT_HAIKU_MODEL    // 小/快
ANTHROPIC_DEFAULT_SONNET_MODEL   // 中
ANTHROPIC_DEFAULT_OPUS_MODEL     // 大
ANTHROPIC_SMALL_FAST_MODEL       // 显式覆盖「小快模型」
ANTHROPIC_MODEL                  // 主循环模型
```

（`~/.claude/settings.json` 的 `env` 段实测；本机三个档位均指向 `deepseek-flash`）

**小快模型服务的场景**（从 CLI bundle 的调用点归纳，函数 `getSmallFastModel`）：

| 场景 | 证据 |
|---|---|
| prompt hook 的模型调用 | 参数描述逐字：*"If not specified, uses the default small fast model"* |
| 配额检查 | `Yq({..., model: Tw(), source: "quota_check"})` |
| API key 校验 | `Yq({..., model: Tw(), source: "verify_api_key"})` |
| token 计数 | `source: "count_tokens"` |
| 后台分类器 | `useSmallFastModel: !0` 为默认；`false` 时回落主模型 |
| web search | 实验开关控制：`ct("tengu_plum_vx3",!1) ? Tw() : mainLoopModel` |

**压缩不用小模型** —— 这是关键事实：

```
Source:"compact", forkLabel:"reactive-compact", maxTurns:1,
fallbackModel: …… options.mainLoopModel ……
```

⇒ 压缩使用的是 `mainLoopModel`（与主对话同一个）。与 pi 行为一致：**两家都不给压缩降级**。

**思考档位**：`low` / `medium` / `high` / `xhigh` / `max`（`--effort` 或 `/effort`），
与模型档位**正交** —— 即 Claude Code 同时支持「换模型」与「换档位」两条路。

### 对本章的推论

| 做法 | pi | codewhale | codex | Claude Code | 本仓现状 |
|---|---|---|---|---|---|
| 档位是**调用级**属性 | ✅ 形参 | ✅ 窄类型 | ❌ | ✅ | ❌ 进程级 |
| **真按调用类型分档** | ❌ 压缩与主对话同档 | ✅ 仅 Router | ❌ | ❌ 压缩仍用主模型 | ❌ |
| **按调用类型换模型** | ⚠️ 仅子 agent | — | ❌ | ✅ 四类辅助调用用小模型 | ⚠️ 仅子 agent |
| 错误值不可表示 | — | ✅ 编译期 | — | — | ❌ |
| 关闭时不发字段 | ✅ | — | 未知 | ✅（`disableThinking`） | ✅ |

**三条硬结论**：

1. **「压缩降档」与「压缩换小模型」在实测的四家里都没有先例**。pi 与 Claude Code 都是
   「压缩用主模型同一档 / 同一模型」。⇒ 本仓的 Q0（压缩不需深推理）依然**孤证**。
2. **Claude Code 的辅助调用降级有明确的适用边界**：它降的是 **quota_check / verify_api_key /
   count_tokens / hook / 分类器 / web search** 这类**不重要且高频**的调用 —— 共同特征是
   「结果只用于判断或计数，不进对话上下文」。**压缩与摘要不属此类**（结果会进会话，
   错了要付出后续轮次的代价）。
3. ⇒ **本仓真正该对标的场景是工具摘要（场景 2），不是压缩（场景 1）**。
   工具摘要与 Claude Code 的 web search / hook 同构（结果短、失败可重试、不进主上下文）。
   本仓已有固定短 system 与 `max_tokens = 1024` 硬上限（`summarize.rs:23,40`）。

> **换模型 vs 换档位是两个轴，本仓两轴都有缺**：
> - 换档位：只有一个全局值（本章主体）。
> - 换模型：**子 agent 已支持**（`subagent` 工具的 `model` 参数，`subagent.rs:1404,1427`），
>   但**主轮的 per-turn 切换未做**（`docs/arch/per-turn-model-switch/` 的 Q1 未答）。
> 两者不互斥，但不该同一轮做 —— 换模型会带来跨 provider 的协议、凭证、缓存、成本统计一堆问题
> （`per-turn-model-switch/context.md` 的 Q4 / Q7 就是在问这些），比换档位重得多。

## 约束

### 技术

1. **`ys-core` / `ys-event` / `ys-protocol` 不含 tokio** —— 若档位类型要下沉到 `ys-core`，只能是纯数据。
2. **`ModelEventSink` 必须保持同步 `fn`**（跨动态库边界）—— 与档位本身的传递无关，但限制了
   「通过事件回调反向传参」这条路。
3. **既有序列化契约不可破**：`ModelRequest` 是 `Serialize + Deserialize`，JSONL 会话契约依赖它。
   加字段必须 `#[serde(skip)]`（照 `boundary` 先例），否则字节变化。
4. **两层门控必须保留**：provider 级 `supports_reasoning_effort` + 模型级 `supports_effort(model)`。
   任何按调用类型分档的新值都必须**照旧穿过 `effort_for()`**，不能绕过折叠。
5. **前缀缓存相干性**：压缩摘要刻意复用活对话的 system 与消息前缀以命中 KV cache
   （`compact.rs:317-325`，ADR-0017 决策⑤）。`reasoning_effort` **不进序列化前缀**
   ⇒ 理论上改它不破前缀；但这是本需求成本可控的**关键前提**，需在 arch-design 阶段
   用实测或 wire 字节对比复核（不能只靠推理）。

### 性能

- **实测杠杆是真实的**：本机一次长会话中 reasoning 占 `output_tokens` 的 **69%**（74,441 / 106,585）。
- **但不能全局降档**：`low` 让 `fact/1` 质量从 3/3 掉到 2/3（`docs/arch/session-latency/design.md` §8.5）。
  ⇒ 优化空间在**分档**，不在降档。
- **`high` vs 不发字段**：中位墙钟 100.7s vs 199.2s（近乎减半）⇒ 默认 `high` 有实测依据（ADR-0042）。

### 演进

- 档位语义已两次调整（`off` 折叠规则、MiMo 档位集），每次都要同步 compat 折叠表与
  `design-final/effort.md`。按调用类型分档会**再乘一层**（类型 × 模型代际）—— 折叠表可能爆炸。
- `SubagentModelCache` 的键是 `(模型名, Option<String> 档位)`（`subagent.rs:543`）。
  若档位变成调用级，**这个缓存键的语义要重新定义**（实例不再由档位区分）。

### 组织

- 单人维护。「类型 × 代际」的二维折叠表若靠手写维护，出错面大 —— 需要考虑结构上消除而非靠测试兜。

## 需求范围

### 范围内

- 让**不同的模型调用**可以用不同的推理档位（机制）。
- 为每一类调用给出**有依据的**默认档位（策略）—— 注：这不能从同类产品抄（见「对本章的推论」）。
- 用户可配，有合理默认。

### 范围外（明确不做的）

- **按调用类型换模型（而非换档位）** —— 那是另一个轴，且本仓**子 agent 已支持**
  （`subagent` 工具的 `model` 参数）。主轮的 per-turn 换模型属
  `docs/arch/per-turn-model-switch/`（Q1 未决），**不在本需求内**。
  理由：换模型带来跨 provider 的协议 / 凭证 / 缓存 / 成本统计一串问题，比换档位重得多。
- **全局降档换速度** —— 已实测掉质量（3/3 → 2/3）。
- **改 `Model` trait 的签名** —— `complete(request, sink)` 是跨 crate 的稳定接口；
  现状已有 `ModelRequest` 这条「按请求传参」的通路，不该新开一条。
- **给主线模型加 `thinking_budget` / `enable_thinking`** —— 那是另一套 wire 字段（pi 用），
  与本需求的「分档」正交。
- **PTC / 轮次优化** —— 另一条轴（`docs/TODO.md` D 段已单独记录）。

### 关键场景

- **场景 1（压缩）**：上下文接近上限，触发压缩摘要。假设是「缩写已有内容不需要深推理」
  ⇒ 用最低档或 `off`。压缩与主调用共用 model 实例，是当前最直接的成本浪费点。
  ⚠️ **这个假设本身未被验证** —— 而实测过的 pi 与 Claude Code 都**不给压缩降级**
  （两家都用主模型同一档）。
- **场景 2（工具摘要）**：`webfetch` 抓页面后要一句摘要、`subagent` 结果要压缩。
  比场景 1 **更安全**：这里已有固定短 system 提示（`SUMMARIZE_SYSTEM`，≤ 200 词）
  与 `max_tokens = 1024` 的硬上限（`summarize.rs:23,40`）⇒ 输出本就被压得很小，降档风险面窄。
- **场景 3（子 agent）**：派发的子任务若只是「读几个文件告诉我 X」，用父的 `high` 是浪费；
  但若是「设计一个模块」，`high` 是对的。**需要按任务分，而任务类型在派发时已知**。
- **场景 4（用户覆盖）**：用户在 TUI 里 `/effort low` 时，期望的是「主轮变 low」，
  还是「全部变 low」？这是未澄清问题之一。

## 快速验证结果（2026-10-27，真实 provider）

> 目的：在讨论「怎么分档」之前，先确认**分档这个杠杆在本仓的 provider 上到底存不存在**。
> 方法：直查 DeepSeek 官方文档 `zh-cn/guides/thinking_mode`，并用 `deepseek-flash` 实测。
> 脚本 `tmp/effort_probe.py`（未入库），原始输出在 `/tmp/`。

### 官方文档的权威口径

DeepSeek 的思考控制是**两个独立字段**：

| 字段 | 形态 | 用途 |
|---|---|---|
| `thinking` | `{"thinking": {"type": "enabled/disabled"}}` | **开关** |
| `reasoning_effort` | `{"reasoning_effort": "low/high/max"}` | **强度** |

关键事实：

1. **思考默认打开，且 effort 默认就是 `high`** —— 与 ADR-0042 的默认值巧合一致。
2. **`off` 不是合法 wire 值**（实测返回 422 `unknown variant 'off'`）。要关思考只能发
   `thinking: {"type": "disabled"}`（Anthropic 格式是 `reasoning.effort = "none"`）。
3. **`reasoning_effort` 的合法集只有 `low` / `high` / `max`**，其余是**别名折叠**：

   | 请求值 | 实际映射 |
   |---|---|
   | `minimal` | `low` |
   | `low` | `low` |
   | `medium` | `high` |
   | `high` | `high` |
   | `xhigh` | `high` |
   | `max` | `max` |
   | `ultra` | `max` |

   ⇒ **7 个配置档位实际只有 3 个 wire 档位**。
4. 思考模式下 `temperature` / `presence_penalty` / `frequency_penalty` **静默失效**（不报错）。

### 实测数据（`deepseek-flash`，n=3/档）

摘要任务（真实抓取页面 21.9 KB + `SUMMARIZE_SYSTEM` 逐字 + `max_tokens=1024`），
质量用「4 个必需精确串的命中数」判定：

| 配置 | 命中 | 墙钟中位 | reasoning tokens（3 次） | 词数 |
|---|---|---|---|---|
| 默认（thinking on, high） | 3.33/4 | 2.07s | 166, 53, 109 | 89-106 |
| **`thinking=disabled`** | **3.67/4** | **1.48s** | **0, 0, 0** | 96-117 |
| `thinking=disabled` + `effort=low` | 3.33/4 | 1.49s | 0, 0, 0 | 94-105 |
| `thinking=enabled` + `effort=low` | 4.00/4 | 2.05s | 56, 121, 60 | 105-116 |
| `thinking=enabled` + `effort=max` | 4.00/4 | 2.12s | 63, 232, 120 | 91-120 |

### 三条结论

**① 真正的开关是 `thinking`，不是 `reasoning_effort`。**
`thinking=disabled` 把 reasoning tokens 稳定打到 **0**；而 `effort=low` 仍有 56-121 个
reasoning tokens —— 因为 low 只是「少想」，不是「不想」。这推翻了「把档位调到 `off` 就等于关闭思考」
这条隐含假设。

**② `reasoning_effort` 在 low/high/max 之间的差异，在本任务上不显著。**
墙钟 2.05s vs 2.12s（low vs max），命中都是 4/4。n=3 太小，不能断言「完全无差异」，
但足以说明：**在摘要这类短输出任务上，调 effort 的收益远小于关 thinking**。

**③ 关掉思考后质量没有下降，反而命中略高（3.67 vs 3.33），墙钟快 ~28%。**
这对**工具摘要**是直接的正面证据：`thinking=disabled` 在这类任务上是一笔**稳赚**的交易。
⚠️ 但 n=3 且单任务，只能作方向性判断，不能当验收依据。

### 成本补充测量（2026-10-27 晚，n=1/档）

> 为省额度只跑 1 次/档，**只用来判断方向与口径，不作任何定量结论**。
> 前一组 n=3 的同配置方差极大（reasoning 53→166），所以下面百分比只能当量级看。

**先钉住一个计费口径**：`completion_tokens` **包含** `reasoning_tokens`（子集关系）。
证据：长任务里 `default` 与 `effort=max` 两组的 `completion_tokens` 恰等于
`reasoning_tokens`（都是 2048，即 `max_tokens` 上限），且正文为空。⇒ 计费按
`completion_tokens` 一项算，不要重复加 reasoning。

**短任务**（摘要，真实页面 21.9 KB，`max_tokens=1024`）：

| 配置 | completion | reasoning | 墙钟 | 质量命中 |
|---|---|---|---|---|
| 默认（thinking on, high） | 306 | 117 | 1.95s | 3/4 |
| **`thinking=disabled`** | **145（−53%）** | **0** | **1.26s（−36%）** | 3/4 |
| `effort=low` | 290（−5%） | 34 | 2.13s（**+9%**） | 4/4 |
| `effort=high` | 352（+15%） | 64 | 2.09s | 4/4 |
| `effort=max` | 374（+22%） | 75 | 2.19s | 4/4 |

**长任务**（推理型，`max_tokens=2048`）：

| 配置 | completion | reasoning | 墙钟 | 正文 |
|---|---|---|---|---|
| 默认 | **2048（触顶）** | 2048 | 11.3s | **空** |
| `effort=max` | **2048（触顶）** | 2048 | 11.8s | **空** |
| `effort=low` | 1660 | 1501 | 9.5s | 有（83 词） |
| `effort=high` | 957 | 810 | 5.2s | 有（71 词） |
| `thinking=disabled` | 106 | 0 | 1.0s | 有（53 词） |

### 四条结论

**① 真正的开关是 `thinking`，不是 `reasoning_effort`。**
`thinking=disabled` 把 reasoning tokens 稳定打到 **0**；`effort=low` 仍有几十到上千个
reasoning token —— low 只是「少想」，不是「不想」。这推翻了「把档位调到 `off` 就等于
关闭思考」这条隐含假设。

**② `effort=low` 的省钱效果很弱，而且不是单调的。**
短任务只省 5% completion，墙钟反而慢 9%。四档在摘要任务上的差异落在一倍以内，
**远小于 `thinking=disabled` 的 53%**。⇒ 「降 effort 省钱」这个直觉在本仓的实际
provider 上**基本不成立**。

**③ 关掉思考是唯一有显著收益的动作**：短任务 completion −53%、墙钟 −36%，质量未降；
长任务 completion −95%、墙钟 −91%。

**④ ⚠️ 长任务暴露一个独立的、可能更紧急的问题：`max_tokens` 偏小 + 默认 `high`
会让正文完全为空。**

默认档与 `max` 档都把 2048 的输出预算**全烧在思考上**，`finish_reason` 为长度截断，
正文 0 词。这与本仓直接相关：

- `crates/ys-loop/src/summarize.rs:40` 的 `SUMMARIZE_MAX_TOKENS = 1024`
- 而 `ProviderCompat::deepseek()` 不发 `reasoning_effort` ⇒ 服务端用默认 **`high`**

⇒ **工具摘要（webfetch / subagent）在内容较难时，存在「思考吃掉全部 1024 预算、
返回空摘要」的风险**。这不是理论推演：上表 `default` 行就是同一现象。

⚠️ 该组任务本身设计有问题（五档质量全为 0/4，含 `thinking=disabled`），
**不能用于比较质量**；它只用于证明「思考能吃光预算」这个现象存在。

### 对本仓的直接影响

**全仓没有任何地方发送 `thinking` 字段**（`grep thinking adapters/` 只命中
`has_reasoning_content` 的**响应侧**解析）。而：

- `ProviderCompat::deepseek()` **没有开** `supports_reasoning_effort`（`compat.rs:54-61`
  只有 `has_reasoning_content` 与 `supports_stream_usage`）⇒ 本仓对 deepseek **根本不发**
  `reasoning_effort`，用的是服务端默认 `high`。
- `opencode_go()` 开了 `supports_reasoning_effort`，但同样不发 `thinking`。

⇒ **本仓目前既不能关闭思考，也不能真正降档**：
`off` 配置面对 deepseek 是空操作（字段不发），对 opencode-go 会撞 422（`off` 非法）。
**这是比「缺按类型分档」更基础的一个缺陷**，且它不需要任何架构改动就能修
—— 只需在 wire 层加 `thinking` 字段。

---

## 未澄清问题

### 已由快速验证回答（2026-10-27）

- [x] ~~**Q0 压缩降不降档 / Q0b 先做压缩还是工具摘要**~~ ⇒ **先做工具摘要，且手段是
      `thinking=disabled` 而非调 `reasoning_effort`**。实测：摘要任务上 `thinking=disabled`
      命中 3.67/4（默认 3.33/4）、墙钟快 28%、reasoning tokens 归零；
      而 `effort` 在 low/high/max 间无显著差异。
      压缩仍不动 —— pi 与 Claude Code 都不给压缩降级，且压缩结果会进会话。
- [x] ~~**Q0c 为什么不换模型**~~ ⇒ **换模型与关思考不冲突，但先做后者**：关思考是 wire 层
      加一个字段（零架构改动、零凭证成本），换模型要新 provider 凭证 + 独立缓存。
      待 `thinking` 落地后再评估是否需要换模型。

### 仍需裁决

- [ ] **Q1 分档的粒度**：是按「调用类型」（枚举：Turn / Compaction / ToolSummary / Subagent）
      分，还是允许任意「每次调用传一个值」？
      **对照结论**：pi 走形参（任意值），codewhale 走窄类型（错值不可表示）。
      本仓若选枚举，**可以把「编译摘要只能 off/low」这类规则编进类型**（codewhale 的做法），
      而不只是运行时白名单 —— 这是当前设计里最值得借的一点。
- [ ] **Q1b 枚举要不要按调用类型收窄**：例如 `CompactionEffort` 只允许 `Off|Low`，
      与 `ReasoningTier` 并列。代价是本仓目前**只有一套 7 档枚举**（codewhale 是三套，
      靠手工别名表对齐，`deepseek_effort.rs:83-100`）—— 拆开就丢了那份统一。
- [ ] **Q2 `/effort` 的语义**：用户敲 `/effort low` 时改的是**主轮**还是**全部**？
      现状是**全部**，但是经由一个副作用实现的（不是设计出来的）：`set_effort`
      （`capabilities.rs:300`）只重建**主模型实例**，而压缩 / 工具摘要**共用同一个 `ctx.model`**
      ⇒ 档位跟着变了。子 agent 不受影响（它走 `SubagentModelCache`，键里的档位来自配置，
      模型实例重建前已缓存的仍用旧档位）。
      若引入按类型分档，「全部」这个行为就不再是自然结果，必须显式定义。
- [ ] **Q3 配置面**：新档位怎么配？环境变量（`YUSHAN_EFFORT_COMPACTION=off`）？
      还是 config.json 的嵌套表？还是干脆只给**默认值**、不开放配置？
- [ ] **Q4 「关闭思考」的语义在各 provider 上不统一**（已由文档确认，不再是猜测）：
      DeepSeek 用 `thinking: {type: disabled}`（**`off` 是非法值，422**），Anthropic 格式用
      `reasoning.effort = "none"`，MiMo 的合法集里有 `none`，GLM 只有 `none|minimal|low|...`。
      本仓的 `EffortLevel::Off` 目前**不是**这些字段中任何一个 —— 它是「配置层的一个值」，
      落到 wire 上要么被折叠成 `low`、要么整个字段不发。
      ⇒ 要真正支持关闭，得给 `ProviderCompat` 加一个「关闭思考」的 wire 表达，
      而不是复用 `EffortLevel::Off`。
- [ ] **Q5 质量回归风险**：压缩摘要降档会不会让压缩后的对话「跑偏」？
      `ADR-0017` 对压缩有约束，需要确认降档不在其禁止范围内。
- [ ] **Q6 验证方式**：`e2e_bench` 目前只有 `fact/1` 是可判定任务，且它测的是**主轮**质量。
      压缩 / 摘要类调用的质量**怎么判定**？（是否需要新的可判定任务？）
- [ ] **Q7**：`SubagentModelCache` 的键含档位（`subagent.rs:543`）。若档位变调用级，
      这个缓存键该保留档位、还是改成只按模型名？两种情况各有何代价？

## 后续建议

> 验证已经把首选项定了下来。**不再建议先做压缩降档**。

**第一步（无需 arch-design，可直接做）：给 wire 层加 `thinking` 开关。**

理由：

- 这是实测里**唯一真正起作用**的变量（`thinking=disabled` ⇒ reasoning tokens 归零、墙钟 −28%、
  质量未降）；而 `reasoning_effort` 在 low/high/max 间无显著差异。
- 零架构改动：`ChatCompletionRequest` 加一个 `thinking: Option<Thinking>` 字段，
  `ProviderCompat` 加一个能力位。不碰 `Model` trait、不碰 `ModelRequest`、不碰 model 实例缓存。
- **顺带修掉一个既有缺陷**：本仓现在对 deepseek 根本不发 `reasoning_effort`
  （`ProviderCompat::deepseek()` 未开该能力位），`/effort` 配了 `off` 也没有任何 wire 效果；
  对开了能力位的 opencode-go，发 `off` 会撞 422。两种都是「配置与行为不一致」。

**第二步：让工具摘要使用 `thinking=disabled`。**

- 已有可判定测试面（`SUMMARIZE_SYSTEM` 固定、`max_tokens=1024` 固定），验证成本低。
- 与 Claude Code 的降级场景同构（结果短、失败可重试、不进主上下文）。

**第三步（可选，需 arch-design）：按调用类型分发档位/开关。**

到这一步才需要回答 Q1（枚举 vs 任意值）与 Q2（`/effort` 语义）。核心分叉仍是那个结构障碍：
档位冻在 model 实例里，而主轮 / 压缩 / 工具摘要**共用一个实例**。
两条出路 —— 每类调用拿不同实例（贵：丢连接池共享），或把档位从实例搬到请求
（`ModelRequest` 边信道，照 `boundary` 先例）。

**明确不建议做的事**：

- **压缩降档/关思考**：pi 与 Claude Code 都不这么做，且压缩结果会进会话。
  除非有「压缩后可继续完成原任务」的可判定实验支持，否则不做。
- **先做 per-turn 换模型**：跨 provider 的协议 / 凭证 / 缓存 / 成本统计代价远大于此，
  且 `docs/arch/per-turn-model-switch/` 的 Q1 仍未决。

**验证的局限（必须记下）**：

本次 n=3、单任务、单 provider（`deepseek-flash`）。它足以**否定**「调 effort 能省很多」
和**支持**「关 thinking 有效」，但不足以作为验收依据。落地时仍需按 `AGENTS.md` 跑
`e2e_bench` 取基线。原始脚本 `tmp/effort_probe.py` 未入库（`tmp/` 被 gitignore）。
