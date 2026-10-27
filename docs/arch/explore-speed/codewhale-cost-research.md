# Codewhale 的 token 降本机制 —— 对照与可借鉴项

> 日期：2026-10-27
> 性质：**竞品机制研究**（`docs/arch/**/research.md` 类产物，不进架构决策）。
> 对象：`tmp/Codewhale`（本地检出，未固定 commit —— 以下行号对该检出有效）。
> 缘起：一次 9 turn / 159 次模型往返的会话把 MiMo 余额耗尽（复盘见本文件 §4）。
> **本文不含决策**：要落地的项须另走 `arch-design`。

---

## 0. 一句话

Codewhale 的降本不是「省着用」，而是**把「前缀不变」变成有测试强制的架构不变量**，
再配一套**拒绝编造**的成本可见性。但它的「上下文有界」只做到**单条有界**，
**没有**绝对 token 上限，也**没有**回合级累计闸门 —— 它和本仓一样，**大窗口下
压缩触发点仍然随窗口线性抬升**（1 M 窗口 ⇒ 800 K 才压缩）。

两家的真实差异集中在三处：① 它管住了前缀（我们只测不管）；
② 它有成本可见性（我们事故期间界面上一个数都没有）；
③ 它把轮数收敛交给了 code mode（我们没做）。

---

## 1. 七层机制

### 1.1 前缀缓存：把「命中」变成不变量

`docs/CACHE.md:13` 写明不变量：

> 会话开始后，system prompt 与 tool catalog 是**冻结的字节**。历史只增长。
> 缓存未命中**只有在能说出原因时才允许**。

**指纹与三种判定**（`crates/core/src/prefix_cache.rs:546` `PrefixStabilityManager::check`）：
system 文本与工具目录 JSON 各算 SHA-256，再算组合哈希，比对得三态：

| 判定 | 条件 | 处置 |
|---|---|---|
| `Stable` | 组合哈希相同 | 不动 |
| `Repinned { reason }` | 变了，**且调用方声明了原因** | 把 pin 移到新前缀，记 `change:<reason>` |
| drift（未声明） | 变了，**无原因** | 记 `drift:<component>`，**pin 故意不动** |

第三点是设计上最锋利的一处。旧行为是「检测到漂移就重新 pin」，于是 `/cache stats`
显示「稳定」，而 provider 侧缓存早已死透。现在**保留原 pin ⇒ 同一未声明前缀持续计为
miss ⇒ 它一直显示为异常**，无法被静默吸收（`docs/CACHE.md:56-64`）。

已声明的 header 变更白名单（`docs/CACHE.md:38-50`）：

| 操作 | reason |
|---|---|
| `/model` | `change:model` |
| 模式切换（agent/plan/operate/yolo） | `change:mode` |
| goal 设置/暂停/恢复/清除 | `change:goal` |
| 回合中工具面变化（deferred 准入/驱逐、tool-search 激活、运行时 MCP 到达） | `change:tool_surface` |
| 会话同步/恢复 | `resume` |
| 会话构造 | `initial` |

历史重置另计 `reset:<what>`（`reset:compaction` / `reset:clear` / `reset:image_window`）。

**`<context_update>`：把易变信息从「改写前缀」改成「追加消息」**

工作区漂移（新文件、AGENTS.md 改动、新增 skill、memory 条目）**不**重组合 system prompt，
而是等到**下一个用户回合开始时**（绝不在工具循环中途）算出 `+`/`-` 行级差分，作为
**一条 user-role 消息**追加在用户消息之前。
实现在 `crates/tui/src/core/engine.rs:8966`（`refresh_pinned_header_for_turn`），
差分算法在 `crates/core/src/prefix_cache.rs:753`（`context_update_message`）。

边界（`prefix_cache.rs:750`）：`CONTEXT_UPDATE_MAX_LINES = 80`、
`CONTEXT_UPDATE_MAX_BYTES = 6_000`；超出写一行
`(+N more changed lines; re-read the files you need)`。
同一差分**只投递一次**（`session.context_update_baseline`）。

测试直接钉住这条（`crates/tui/src/core/engine/tests/test_cases_16.rs:120`
`workspace_drift_arrives_as_one_context_update_and_never_moves_the_header`）：
写文件后 header 字节不变 + 恰好一条 `<context_update>` + 下一回合不再重发。

**把义务写进 review 判据**（`AGENTS.md:167`、`docs/CACHE.md:94`）：

> 任何新的会话上下文贡献者都必须声明它的 **KV-cache 影响**：属于冻结前缀，
> 还是属于只追加历史。**绝不**把易变事实拼接进前缀 —— 追加为 user-role 消息。

`crates/tui/src/tools/codemode.rs:40` 这类模块头部真的照写了。

### 1.2 `cache_control`：Anthropic 侧显式断点，含 4 个上限

DeepSeek 系靠服务端自动前缀缓存（无需标记）；Anthropic 系必须显式打点。
`crates/tui/src/client/anthropic.rs:834` 的 `apply_anthropic_cache_breakpoints` 定策略：

1. **断点 1**：最后一个 tool（无 tool 时用最后一个 system block）—— 缓存不可变前缀
2. **断点 2**：最新 user 消息的最后一个 content block —— 缓存只追加历史
3. 调用方自带的断点保留，但总数**封顶 `MAX_CACHE_BREAKPOINTS = 4`**
   （`anthropic.rs:43`），超出时**从最早的开始丢**（最新标记覆盖最长前缀）

另有 `crates/tui/src/tools/registry.rs:349` `to_api_tools_with_cache(enable_cache)`
在工具表末位打一个 `ephemeral` 断点。

### 1.3 工具面：12 个常驻 + 无限可搜，且「发现不激活」

`docs/TOOL_SURFACE.md:25`：新回合只带 **12 个 eager 原生工具 + 1 个合成 `tool_search`**
（`read`/`write`/`edit`/`file_search`/`bash`/`agent`/`workflow`/`todo_write`/`create_goal`/
`get_goal`/`update_goal`/`load_skill`）。其余全部 deferred：`Web`、`github`、`automation`、
`rlm`、MCP、plugin。

**它如何绕开自己设下的矛盾**（加工具即破前缀）—— 把「发现」与「激活」分成两步：

- 在程序内 `tools.call('tool_search', {query})` 返回匹配 schema 但**不激活** ⇒
  「请求的 tool 数组与会话 pin 前缀都不变」（`docs/TOOL_SURFACE.md:151`）
- 真正激活时，环境缓存上限 **8 个名字 / 16 KiB 序列化 schema**，LRU 驱逐
  （`TOOL_SURFACE.md:191`）
- 被驱逐的工具在搜索结果里以 `evicted_tool_references` **显式告知**，而非下次请求里静默消失

MCP 工具默认全部 deferred，除非用户在 `[tools].always_load` 点名。

### 1.4 Code mode：把 N 轮压成 1 轮

`execute_tools`（`crates/tui/src/tools/codemode.rs:1`）让模型写一段 JS，用
`await tools.call(name, args)` 编排多次调用。第 5 行的关键句：

> 中间结果留在 VM 里，只有**有界返回值 + 宿主收据**回到模型。

权限不让步：嵌套调用**逐个回到 turn loop 走与直接调用完全相同的门**
（`codemode.rs:12`）；批准这个程序本身不授予任何权限。
`code_mode` 默认开（`[features] code_mode = true`）⇒ `execute_tools` 从第一次请求就 eager，
且它是会话配置 ⇒ 前缀在会话内稳定。

### 1.5 输出与工具结果：能力上限 ≠ 默认请求

`crates/tui/src/route_budget.rs:155` 的注释是这一层的思想来源：

> 目录里的值是**能力上限**，不一定是合理的默认请求大小。DeepSeek V4 标称 384K，
> 把这个上限当默认，会让 Codewhale 在一个 262K/327K 的 route 上**还没算一个输入
> token 就先索要几乎整个上下文作为输出**（#5516/#5518）。

落地：`API_MAX_OUTPUT_TOKENS = 65_536`（`route_budget.rs:8`），目录值 `.min(65536)`；
无目录时 `(window/2).min(65536)`。**上限描述 provider 允许什么，不是每次响应该预留什么。**

工具结果单条内联预算（`route_budget.rs:54`）：窗口的 **3%** × 4 字符/token，
**上限 100,000 字符**；操作员可 opt-in 抬到 2 MiB。超出则切成 head+tail，中间放 footer
指明完整输出位置 + `art_<id>` 引用，模型可 `retrieve_tool_result` 读回。

**关键克制**：`crates/tui/src/core/engine/context.rs:641` 有专门分支**不**二次压缩
`read` 的结果 —— 因为它自带续读指针，再压会破坏契约并丢掉调用方刻意预算的内容。

推理档位的预算语义（`route_budget.rs:377`）：推理模型的 `max_tokens` 是思考与可见文本
**共用额度**，wire 上无法分离（未接 `thinking.budget_tokens`），所以「预留」= 声明预留量
**并且**限制推理档位能吃掉多少。实测教训（#6285）：某些模型把整个 64K 额度烧在思考上，
**返回零可见文本、stop reason = `length`** ⇒ 这类家族预留一半，其它推理模型预留四分之一
（`route_budget.rs:406`）。

### 1.6 skill 索引：显式预算 + `explicit-only` 零占用

`crates/tui/src/skills/mod.rs:56-62`：

| 常量 | 值 | 含义 |
|---|---|---|
| `MIN_AVAILABLE_SKILLS_CHARS` | 2_400 | 小窗口下限，避免索引被抹掉 |
| `MAX_AVAILABLE_SKILLS_CHARS_CEILING` | 40_000 | 上限 |
| `SKILL_BUDGET_CONTEXT_PERCENT` | 5 | 占窗口比例（×4 字符/token） |

预算函数 `skills_prompt_budget_chars(window)`（`skills/mod.rs:85`）。
上限的注释直白：「超过这个数，`load_skill name="list"` 比常驻一页更划算，哪怕在 1M 窗口上」。

单条描述封顶 400 字符，且缩短时**优先保住 `Use when:` 触发短语**
（`skills/mod.rs:1901` `split_trigger`、`description_split`：两者都存在时
trigger 不得低于一半）—— 那是模型真正路由的依据。

分级（`docs/SKILLS.md:147-149, 224`）：
- `invocation: model+user`（默认）：进模型目录
- `invocation: explicit-only`：**不进目录，ambiente prompt 预算为 0**，仍可按名加载
- `aliases-for:`：别名不是独立目录条目，**不重复 prompt 内容**

会话内 pin：`skills/mod.rs:80` 注明窗口每 route 固定 ⇒ 渲染块跨回合字节稳定，
不会自己动 KV 前缀。

### 1.7 成本可见性：拒绝编造数字

- 状态栏实时花费 chip（`crates/tui/src/route_billing.rs:932`）
- `/cost`、`/cache stats`（`/cache` 报告 pin 原因、最近 miss 原因、drift 计数、
  `<context_update>` 数量、聚合命中率）
- 三态 chip：`Money`（有定价）/ `Unknown(reasons)`（列出为什么未定价）/
  `Hidden`（真无可报）
- `UnpricedReason` 枚举（`crates/tui/src/pricing.rs:1235`）：
  `NoPricingRow` / `RetiredAlias` / `UnrepresentedTier` / `UnverifiedLivePricing` /
  `InconsistentUsage` / `LateUsageUnavailable` / `LateUsageOverflow` / `UnrecordedCoverage`
- **明确不把「未覆盖」当 0 元**（`pricing.rs:1297`：这些变体不计入金额覆盖率）
- 建模阶梯价（`crates/config/src/pricing.rs:271` `context_tiers`）—— 长上下文跨档涨价可见

**usage 字段的「未上报 vs 真 0」可区分**（`crates/models/src/lib.rs:166-190`）：

```rust
pub prompt_cache_hit_tokens: Option<u32>,     // None = provider 未上报
pub prompt_cache_miss_tokens: Option<u32>,
pub prompt_cache_write_tokens: Option<u32>,   // Anthropic cache_creation_input_tokens
pub reasoning_tokens: Option<u32>,
pub reasoning_replay_tokens: Option<u32>,     // 重放 reasoning_content 的估算成本
```

计价分类（`pricing.rs:2603` `request_token_usage_for_pricing`）：
`input_tokens` 是权威总量，缓存读/写/未命中三类依次**夹逼**（`min` 扣除），
畸形遥测不会产生总和超过 parent 的 token 类。
计价沿用三档单价（`pricing.rs:224`）：`input_cache_hit_per_million` /
`input_cache_miss_per_million` / `output_per_million` + `cache_write` 策略。
`reasoning_tokens` **已含在 `output_tokens` 内**，不重复计费（`pricing.rs:2620`）。

---

## 2. 它**没有**做的

- **没有按金额/按 token 的硬性中止**。预算只有墙钟（`turn_wall_clock_secs`）与
  step 预算（`budget.max_steps`，缺省 0 = 不设上限）。
  取舍可辩护：coding agent 中途停下的代价常高于多花钱。
- **压缩触发同样是窗口百分比，且生产默认 80%**：装配点取
  `settings.auto_compact_threshold_percent`（`crates/tui/src/exec_agent.rs:457`），
  其默认 **80.0**（`crates/tui/src/settings.rs:542`）。
  库层另有 `DEFAULT_COMPACTION_TRIGGER_PERCENT = 75.0`
  （`crates/runtime/src/context_budget.rs:49`），但那是兜底，生产不用。
  `DEFAULT_COMPACTION_TOKEN_THRESHOLD = 102_400`（`crates/models/src/lib.rs:38`）
  也只在**模型不可识别、窗口解析不出**时才用，不是对所有模型的绝对上限。
  `CompactionConfig::default()` 的 `token_threshold: 800_000`
  （`crates/tui/src/compaction.rs:173`）是「1M 窗口的 80%」的推导值，注释明说
  「Real call sites override this」（`:166`）⇒ **同为相对量，不是绝对闸门**。

  ⇒ **它也没解决大窗口下「压缩永不触发」的问题**。更准确地说：**窗口越大，绝对压缩点
  越高**，1M 窗口可以让 input 累积到 800 K 才动手。`docs/CONFIGURATION.md:1129`
  自己写了这句：「Raising the window without touching this raises the absolute
  compaction point along with it.」

#### 压缩触发点：钳制表

```text
trigger = min(window × 0.80, input_budget_ceiling)
input_budget_ceiling = window − clamp(output_cap, ≤65536) − 1024
```

| window | output_cap | input ceiling | 实际 trigger | 占窗口 |
|---|---|---|---|---|
| 32,768 | 30,720 | 1,024 | 1,024 | **3.1%** |
| 128,000 | 65,536 | 61,440 | 61,440 | 48.0% |
| 262,144 | 65,536 | 195,584 | 195,584 | 74.6% |
| 1,000,000 | 65,536 | 933,440 | **800,000** | 80.0% |

来源：公式 `crates/runtime/src/context_budget.rs:204`；65536 见
`crates/tui/src/route_budget.rs:8`；262144 的实测断言在 `route_budget.rs:1314`。

**读法**：只有在 ~328 K 以上窗口，80% 才真正生效；中等窗口（128 K–262 K）因 64 K
输出预留被钳到 48%–75%；小窗口会钳到只剩 1,024 token 的极端值，
`route_budget.rs:530` 的注释承认这曾导致「每回合在第一次请求前就触发紧急压缩」（#6540）。

> ⚠️ **对本仓的直接含义**：只把 `context_window` 调小（例如 60000）会踩同一个坑 ——
> 必须同时调整 `output_reserve` 与 `max_turn_tool_result_tokens`，否则压缩会在第一次
> 请求前就触发。

- **没有回合级累计工具输出闸门**：全仓 `per_turn_tool` / `turn_tool_result` /
  `turn_output_budget` **零命中**；`turn_budget.rs` 的 `turn_budget` 指的是 **step 数**
  与 **wall clock**，与字节无关。它只有三层**单条**闸门：

| 层 | 值 | 位置 |
|---|---|---|
| 通用单条内联 | 窗口 3% × 4 字符，上限 100,000 字符 | `route_budget.rs:54-96` |
| 子 agent 单条 | **10,000 token**（按 `bytes/3` 估算，并受 1 MiB 字节上限） | `subagent/mod.rs:479-497` |
| 各工具自有截断 | shell 30 KB（head 1/5 + tail 4/5）/ spillover 100 KiB / web 1 MB / session 12 K 字符 | `shell_output.rs:4,6-8`、`truncate.rs:68`、`tools/web/fetch.rs:24`、`session.rs:20` |

  shell 的切分比例值得一提：head 只取 `threshold / 5`，**大头留给 tail** ——
  因为编译错误与测试失败都在尾部（`shell_output.rs:6-8`）。工具结果的截断位置
  直接影响模型下一轮是否需要重跑，这一点比比例本身更值钱。

  ⇒ 这一点**我们反而更强**：本仓有 `max_turn_tool_result_tokens`
  （`crates/ys-component/src/limits.rs`，装配为 `window/4`）。

---

## 3. 对照 YuShan

| 环节 | Codewhale | YuShan 现状 | 缺口性质 |
|---|---|---|---|
| 前缀稳定性 | SHA-256 pin + drift 不重 pin + contributor 义务 | 有序渲染 + 指纹锚测试（`apps/coding-agent/src/toolset.rs:572`） | **有测试、无运行时检测**：无 pin、无 drift 计数、无 `/cache stats` |
| 工作区漂移 | `<context_update>` 追加，80 行/6 KB | 拼进 system prompt（`apps/coding-agent/src/prompt.rs:57`） | 漂移**改写前缀** ⇒ 读 AGENTS.md 相关场景可能破缓存 |
| `cache_control` | Anthropic 最多 4 断点 + 工具末位 1 个 | 无（OpenAI 兼容协议，靠服务端自动） | 协议不同，**不构成缺口** |
| 工具面 | 12 eager + deferred + 程序内查询不激活 | 父表 12 / 子表 9，全常驻 | 无 deferred 概念 |
| 多步编排 | `execute_tools` 代码模式 | 无（`docs/TODO.md` 的 PTC 条明确不做） | 轮数是乘数，**无收敛机制** |
| 输出默认 | `min(目录, 65536)` | `max_tokens` 永远 `None`（`apps/coding-agent/src/main.rs:2601`） | 完全交给服务端默认 |
| 工具结果 | 窗口 3%、上限 100 K 字符、可读回 | 50 KiB 硬截断（`crates/ys-tool/src/truncate.rs:26`） | 量级接近；无窗口自适应、无读回闭环 |
| 回合级工具预算 | **无** | `max_turn_tool_result_tokens = window/4` | **我们更强** |
| skill 索引 | 窗口 5%、上限 40 K 字符、`explicit-only` 零占用 | 固定 ~9.6 k token 常驻 | 无上限、无零占用档位 |
| 成本可见 | 实时 chip + 未定价原因 + 阶梯价 | usage 字段有；minimax 硬关 ⇒ 全 0；TUI 总量 0 时整行不显示（`crates/ys-tui-coding/src/draw.rs:768`） | 事故期间界面**无任何花费提示** |
| usage 语义 | `Option<u32>`，未上报 ≠ 0 | `u32` + `#[serde(default)]`（`crates/ys-core/src/usage.rs:12`） | 未上报与真 0 **不可区分** |
| 金额硬闸 | 无 | 无 | 两者皆缺 |

---

## 4. 事故复盘（`1793068612_812222000`，作为量化锚点）

provider = minimax / `MiniMax-M3.1-Flash`，**9 turn / 159 次模型往返 / 单 turn 最多 64 轮**。

| 项 | 量 | 来源 |
|---|---|---|
| agent 自身累计输入 | **≈14.6 M token** | 会话 JSONL 逐条消息累计（未折缓存）；末次请求上下文 137 k |
| agent 输出 | ≈31 k token | 可忽略 |
| **模型用 `curl` 直连 MiMo 的探测 payload** | **≈9.4 M token** | tool 参数逐条还原：`probe_window` 5 次 2.80 M、`probe2` 3×1.05 M、2 次 needle（服务端截到 1,048,570）、`probe3` 0.4 M、早期 `mimo_big`/250k 词探测 0.96 M |

结构性缺口（按金额排序）：

1. **上下文 × 轮数 = 主项，压缩未按成本调校**：本仓 `trigger_at` = 窗口 0.85
   （`crates/ys-component/src/compact.rs:45`），1 M 窗口 ⇒ 850 k 才触发，实测最高 137 k
   ⇒ **全程零压缩**。
2. **外部直连调用完全不在账上**：`bash` 只对**结果**做 50 KiB 截断
   （`crates/ys-tool/src/truncate.rs:26`），对**请求**方向零约束。
3. **minimax 的 usage 被硬关**（`adapters/model-openai-compatible/src/compat.rs:63`
   `supports_stream_usage: false`）⇒ `cache_hit=0 cache_miss=0` ⇒ 成本完全不可见。
   适配器本就有 400 一次回退（`adapters/model-openai-compatible/src/lib.rs:603`）。
4. **无预算型闸门**：`max_rounds` 缺省 `u32::MAX`（`apps/coding-agent/src/config.rs:326`）、
   墙钟 48 h、`max_tokens` 永远 `None`。

> 🔐 **凭据泄漏**：该会话把 MiMo API key 以明文写进了几十条
> `export MIMO_KEY='sk-...'` 命令，落在 `~/.yushan/sessions/1793068612_812222000.jsonl`
> 与对应 `~/.yushan/logs/debug-*.jsonl`。余额已归零，但**仍需吊销该 key**。

---

## 5. 可借鉴项（按性价比，**未决策**）

| 优先 | 项 | 对应 Codewhale 证据 | 预估收益（基于 §4） |
|---|---|---|---|
| P0 | `<context_update>` 式追加，替代「漂移改写前缀」 | `engine.rs:8966`、`prefix_cache.rs:753` | 保住前缀 ⇒ 命中率不因工作区变化而塌 |
| P0 | `max_tokens` 给 `min(目录, 65536)` 默认 | `route_budget.rs:8,155` | 避免「索要几乎整个窗口当输出」 |
| P0 | 打开 minimax usage 上报；未定价显式化 | `compat.rs:63`、`pricing.rs:1235` | 没有读数，前两条无法验证 |
| P1 | skill 索引上限 + `explicit-only` 零占用档 | `skills/mod.rs:56-62`、`docs/SKILLS.md:224` | 砍每轮 ~9.6 k 固定开销 |
| P1 | `Usage` 缓存字段改 `Option<u32>` | `crates/models/src/lib.rs:170` | 未上报 vs 真 0 可区分（否则成本数字不可信） |
| P1 | 成本触发压缩（与窗口无关的绝对上限） | **Codewhale 也缺**（触发点纯窗口相对量，1M ⇒ 800 K） | 唯一随轮数线性放大的乘数 |
| P2 | 外部调用可见性（`bash` 向模型端点发请求时提示） | 无对应机制 | 本次 9.4 M 的主项完全绕过 usage |
| P2 | 前缀 pin / drift 运行时检测 + `/cache stats` | `prefix_cache.rs:546` | 让「命中率下降」可见 |

**可以走在它前面的地方**：§2 说明 Codewhale 同样没有金额硬闸、同样用窗口百分比触发压缩
⇒ **成本触发压缩与 token 预算闸门是可差异化项**，且正好对应 §4 的根因。

**我们已经有而它没有的**：回合级工具结果累计预算
（`max_turn_tool_result_tokens`）。它只有单条闸门，没有回合累计 —— 这一点不必照抄它的形态。

---

## 6. 未核实

- `tmp/Codewhale` 未固定 commit，行号随上游变化会漂移。
- Codewhale 未给出「实测缓存命中率」的量化数字；`docs/CACHE.md:82` 只给了手工验证方法
  （真实 key、≥3 个工具步、`/cache inspect` 看 `prompt_cache_hit_tokens > 0`），
  **无实测读数**。因此「它省了多少」目前无法与本仓 `docs/arch/explore-speed/cost-model.md`
  §10.2 的 99% 命中率做量化对比。
- **工具面裁剪也没有给出 token 数字**：`docs/TOOL_SURFACE.md` 只说 12 个 eager +
  deferred，没有「裁剪前/后各多少 token」的读数。
- 四个并行调查中三个被 provider 的 SSE 流解码错误中断（`stop_reason=cancelled`）：
  前缀缓存细节（读 103 次工具调用后断）、工具面/成本面（115 次后断）、可观测面。
  本文对应段落改由直接读源码补全，已逐条核对行号。

**已发生的更正**（留着以防后人重蹈）：

- 最初读 `crates/runtime/src/context_budget.rs:47` 的
  `DEFAULT_COMPACTION_TRIGGER_PERCENT = 75.0` 就写成「默认 75%」。
  **错** —— 那是库层兜底；生产装配点取 `settings.auto_compact_threshold_percent`，
  默认 **80**（`crates/tui/src/exec_agent.rs:457`、`settings.rs:542`、
  `docs/CONFIGURATION.md:1127` 三处一致）。校正后 1 M 窗口的触发点是 **800 K**，
  不是 850 K。
- 最初以为 `cache_control` 从不发送。**错** —— 生产代码有 5 处赋值，
  Anthropic 路径有完整断点策略（`anthropic.rs:834`）。
- 最初把 YuShan 侧 `compat.rs` 的 minimax 硬关写成 `:76`。**错** —— 实际在 `:63`。
