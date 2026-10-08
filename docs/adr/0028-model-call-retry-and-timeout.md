# 0028 — 模型调用超时 + 有界重试（48h 无人值守的自愈面，落适配器）

给「48 小时长程任务」补上**自愈能力**：模型调用加 **① 超时**（连接超时 + 流空闲超时，两个旋钮，不用单一总超时）
与 **② 有界重试/退避**，两者**成对**做（无超时则第一次请求在黑洞端点永久挂起，根本轮不到重试），
统一落在**适配器**（`adapters/model-openai-compatible`）。三条硬规则：**只在尚未吐出任何 delta 时重试**（否则重复输出）、
**可重试性三级判定**（`x-should-retry` 头定论 → 账单/配额名单否决 → HTTP 状态码；连接/超时/429/5xx 可重，
4xx 与解析失败不可重）、**有界退避**（服务端要求的等待超过上限即失败，不傻等）。另加**一条可见性日志**（决策 7）。
**零 `Model` trait 改动、零 `ModelError` 改动、零 `BasicLoop` 改动、零 core crate 改动。**
本 ADR **supersede** [`../arch/llm-api-unavailable/context.md`](../arch/llm-api-unavailable/context.md) §范围外 的「不做自动重试」一条（仅限模型调用路径）。

## Status

accepted（2026-10-08；同类软件调研增补 2026-10-09）

## 背景

- **需求**（原话）：「**需要支持 48 小时的长程任务**」；并就「48h 无人值守的头号缺口」确认**把模型调用重试纳入本轮**。
- **实测缺口**：全仓**没有**模型调用重试（唯一的 "retrying once" 在 `adapters/model-openai-compatible/src/lib.rs:99`，
  是「丢掉不支持的参数后重试」，与限流/网络无关）。48 小时里一次 429 / 网络抖动 / 5xx 就会让**整个回合**
  以 `RunFailed` 收场（`crates/ys-loop/src/basic.rs` Step 6），**几十小时的工作白做**。
- **与 `llm-api-unavailable` 的关系**：那份上下文（commit `2f2d905`）的需求是「**API 不可用时必须报错并终止**」，
  故当时 §范围外 明确「**不做自动重试 / 退避 / 多 provider 故障转移** —— 需求是「报错并终止」，不是「自愈」」。
  **48h 无人值守改写了前提**：无人看着 ⇒ 必须自愈。故本 ADR 只 supersede 该一条，其余（超时、完整性判定）仍归那份设计。
- **为什么超时是重试的前置**：`llm-api-unavailable/context.md` §现状实测矩阵实测 —— `reqwest::Client::new()`
  **未设任何 timeout**，端点「接受连接但不应答」（黑洞/半开）时 `complete` **永久挂起**；
  TUI 下 `Ctrl-C` 此刻走 `Abort` 而 `Abort` 只在轮边界被读 ⇒ **按了没反应**。没有超时，就没有「失败」可供重试。
- **落点的既有约束**（`llm-api-unavailable/context.md` §约束，逐条沿用）：
  `ys-model` / `ys-core` / `ys-event` / `ys-protocol` **不得引入 tokio** ⇒ 超时只能落适配器或 `ys-loop`；
  `ModelEventSink` 必须保持同步 `fn`；`ModelError` **非 `#[non_exhaustive]`**，加变体是 breaking；
  方言归适配器、词汇归 `ys-model`。
- **同类软件佐证**（[`../arch/long-horizon-completion/research.md`](../arch/long-horizon-completion/research.md)
  §2/§3，Pi @ `47236c8`）：重试的三层落点（provider 流开边界 / 整条 assistant 消息 / 持久状态机）、
  「状态码 + 响应头 + 错误文本」三重判据、「服务端要求的长等待即失败」、「退避可中止」、「重试对用户可见」。
  本 ADR 采纳其中四条（决策 3 的两条增补、决策 4 的修订、决策 7）；**明确不采纳**的两条见 Considered Options ——
  理由均出自本仓自身的约束（不变量 #1 与核心 trait 冻结），不是对同类做法的评价。

## 决策

### 1. 落点 = 适配器（不是 `ys-loop`、不是 `ys-model` 的新抽象）

三条理由：

1. **只有适配器知道** HTTP 状态码与连接错误 —— 而重试判据正是这些。`ModelError::Provider(String)`
   （`crates/ys-model/src/error.rs:8`）**没有结构**，上层无法判定「可否重试」。
2. **该文 §约束定死** `ys-model` 不得引 tokio ⇒ 超时/退避放不进 `Model` trait；而 `ys-loop` 是**最高爆炸半径**（ADR-0014）。
3. **零破坏面**：不改 `Model` trait、不改 `ModelError`、不改 `BasicLoop` —— 加变体是 breaking，且那份上下文把它列为未澄清问题。

> **不引 `RetryingModel<M>` 泛型装饰器**：今天只有 1 个适配器，且它需要 `ModelError` 暴露可重试性（= 打破第 3 条）。
> 按 ADR-0014/0021「零消费者抽象退化为具体实现」**否决**；将来第二个适配器出现时再提，是加法。

### 2. 只在「尚未吐出任何 delta」时重试

- 适配器在 `complete` 里包一层 **sink 计数器**：一旦发出过 `TextDelta` / `ThinkingDelta`，
  其后的任何失败**直接上报、不重试**。**计数器只看这两个**（`ModelEvent` 只有这两态；工具参数增量
  **不冒泡**，在适配器内部组装 ⇒ 不构成「已显示」）。
- **理由**：重试会重放整个请求，把**已经渲染出去的文本再渲染一遍**。**受损的只有渲染**，不是会话 ——
  `BasicLoop` 的 `Forwarder` 只把 delta 转成 UI 事件（`crates/ys-loop/src/basic.rs:24-40`），
  会话里的 assistant 内容来自适配器收敛出的 `ModelResponse`，**delta 从不进会话**。
  但渲染侧的重复同样不可接受：TUI 会把它接进同一条 `Assistant` 行（`app.rs` 的 `append_stream`），
  而 `-p` 把增量**直写 stdout、无法撤回**。
- 推论：**中途断流不重试**（那是「半截流」缺口，属 `llm-api-unavailable` 的完整性判定，本 ADR 不覆盖）。
  要覆盖它需一条「本次尝试作废」的渲染信号 —— 见 Considered Options。

### 3. 可重试性按 HTTP 语义判定

判定**分三级**，从左到右短路（先定论、再否决、最后才看状态码）：

| 级 | 情形 | 判定 |
|---|---|---|
| **① 响应头定论** | 存在 `x-should-retry` | 以其为准（`true` ⇒ 可重试，`false` ⇒ 不重试），**不看状态码** |
| **② 账单 / 配额否决** | 错误体文本命中额度名单（见下） | **不重试**（即便状态码是 `429`） |
| **③ 状态码** | 连接 / DNS / TLS 失败、超时 | **可重试** |
| | `429`、`500` / `502` / `503` / `504` | **可重试** |
| | `400` / `401` / `403` / `404` / `413` / `422` | **不重试**（确定性失败） |
| | serde 解析失败、`ModelEventSink` 错误 | **不重试**（前者确定性；后者是消费者已消失） |

**为什么 ① 在状态码之前**：`x-should-retry` 是端点**自陈**的可重试性，比从状态码**推断**更权威；
不存在时回落状态码表 ⇒ 是零成本的可选增强（国内 OpenAI 兼容端点多半不发它）。

**为什么 ② 要单独否决**：`429` 在语义上**二义** —— 可能是瞬时限流（该重试），
也可能是**账户额度 / 计费耗尽**（重试一万次也没有用）。二者状态码相同、**重试代价却相反**。
故从错误体文本先过一遍**额度名单**（大小写不敏感的子串匹配，取自 provider 的实际措辞）：
`insufficient_quota` / `quota exceeded` / `out of budget` / `billing` / `usage limit` / `available balance`。
命中即**立即 `RunFailed`**（不做退避），让额度耗尽的消息尽快到达人眼前，而不是在 5 次无用的指数退避后才浮现。
名单是**开放的常量**：新增 provider 措辞是加法，不得演变成规则引擎。

### 4. 有界退避

- 指数退避 + 抖动，**次数与上限可配**；建议 **base 500 ms、×2、cap 30 s、最大 5 次**（总等待 ≤ ~60 s）。
- `429` 带 `Retry-After` / `Retry-After-ms` 时**尊重它**，但**以 `max_delay_ms`（默认同 cap）为硬上限**：
  **服务端要求的等待超过上限 ⇒ 放弃重试、立即上报**（错误消息带上服务端要求的秒数）。
  **不取「与上界取 max」** —— 那等于允许服务端让本仓**无界地等下去**，与「必须有界」自相矛盾，
  且 48h 无人值守下一条超长 `Retry-After` 就能把回合挂住。
- **必须有界**：无界重试会把「provider 长期不可用」变成「永久占用会话」。
- **退避不可中止（如实记账）**：`Model::complete`（`crates/ys-model/src/trait_def.rs:12`）**没有取消入参**，
  `Boundary::Abort` 只在轮边界被读 ⇒ **退避睡眠期间 Ctrl+C 无效**。传入取消源要改核心 trait（违反零破坏面），
  故本轮不做；可行的界是「总退避 ≤ ~60 s + 每次尝试由流空闲超时包住」⇒ 不可中断时长**有界**。

### 5. 超时用两个旋钮，不用单一总超时

- `connect_timeout`（连接建立，建议 ~15 s）；
- **流空闲超时**（建议 ~120 s 无字节）。
- **理由**（`llm-api-unavailable` §约束「性能」）：长思考模型（`deepseek-reasoner`）**首字节可达数十秒**，
  单一总超时会**误杀**；「首字节延迟」与「中途停顿」语义不同，必须分开设。
- 实现：适配器内 `reqwest` builder / `RequestBuilder::timeout` + `tokio::time::timeout` 包住字节流。

### 6. 配置经 `config.json`，随适配器构造传入

- 新增 `model_retry`（`max_attempts` / `base_delay_ms` / `max_delay_ms`）与 `model_timeout`（`connect_secs` / `idle_secs`）两段。
- 由 `build_agent` 传给 `OpenAICompatibleModel::new`（适配器构造期持有，TUI 下一个 client 存活整个会话 —— 沿用该文 §约束）。

### 7. 每次重试必须留下可见痕迹

- 适配器在**进入退避前**经既有 `logging` 落一行：`retry attempt k/n delay=…ms reason=…`；
  最后一次放弃时另落一行说明最终原因（含服务端要求的延迟，若因决策 4 的硬上限放弃）。
- **理由**：48h 无人值守下，「静默重试 50 次」与「一次都没重试」在事后必须**可区分** ——
  前者说明 provider 一直在抖（该换端点），后者说明链路健康。没有这行日志，`RunFailed` 只剩一个孤零零的字符串。
- **落点是 `logging`，不是事件**：不往 `Envelope` / `AgentEvent` 加遥测变体
  （护住「事件只观察」的边界与零协议变体，同 `design.md` §4.9）。
  即：**重试对 TUI 不可见，对排障可见** —— 这是本轮有意接受的取舍。

## Considered Options

- **在 `ys-loop` Step 6 重试**：需 `ModelError` 暴露可重试性（breaking）且触及最高爆炸半径。**否决**。
- **新增 `ModelError::Timeout` / `Unavailable` 变体**（该文的未澄清问题）：能治「上层无法区分网络不可达与明确报错」，
  但 breaking、且与重试落点无关（适配器自身就能分类）。**本轮不做**，记为后续。
- **`RetryingModel<M>` 泛型装饰器**：见决策 1 的否决理由。
- **单一总超时**：误杀长思考模型。**否决**，改两个旋钮。
- **失败时非流式回退**（`stream: false` 重试一次，该文的未澄清问题）：与「只在未吐 delta 时重试」重叠，
  且需要另一条响应解析路径。**本轮不做**，记为后续。
- **顺带修「200 + 非 SSE 错误体」「无 `[DONE]` 的半截流」两个完整性缺口**：属 `llm-api-unavailable` 的设计。
  **本轮不做**（避免两套设计缠绕）；**如实记账**：48h 下它们**比缺重试更危险**（静默丢工作）。
- **中途断流后重试（把「整条 assistant 消息」当原子单元重放）**：能治「吐了一半断流」，
  但需要一条「本次尝试作废」的渲染信号 —— `ModelEvent` / `AgentEvent` 都是 `#[non_exhaustive]`（加变体不破坏下游），
  可这违反本设计的不变量 #1「零协议变体」，且 `-p` 把增量直写 stdout（**无法撤回**）⇒ 两条路径行为分叉。
  更关键的是：**「吐过 delta 后断流」本质是完整性判定**（流是否完整结束），归 `llm-api-unavailable` 的正主。
  **本轮不做**；硬规则 2 保持不变。若将来做，与那份完整性设计**一并**（详见 research.md §5.1）。
- **把退避睡眠做成可中止**：需要 `Model::complete` 接收取消源 ⇒ 改核心 trait、且 `ys-model` 不得引 tokio。
  **不做**（见决策 4 末条）；以「有界退避 + 流空闲超时」作界。
- **溢出错误（context overflow）路由到压缩**：能省掉一次无用重试，但需要 `ModelError` 能结构化区分溢出
  （现为纯 `String`，加结构是 breaking），且本仓压缩本就由 token 估算在轮边界驱动（ADR-0017）。
  **不做**：`400` 已在决策 3 的「不重试」表内，语义已经对了。
- **工具结果带 `terminate` 字段（工具投票决定回合收尾）**：属「可验证验收」的另一种形态，
  与本轮「模型自觉 + 完工复核」的裁决冲突。**不做**，记账（research.md §5.2）。

## Consequences

- **`adapters/model-openai-compatible` 新增**超时与重试两条路径 + sink 计数器 + **一张额度名单常量** + **一条重试日志**；
  `config.rs` 新增两段配置（`model_retry` 增 `max_delay_ms` 兼作 `Retry-After` 硬上限）；`build_agent` 多传参数。
- **48h 任务的抗抖动能力**从 0 变为「有界重试」；一次 429/网络抖动不再毁掉整个回合。
- **`-p` / `--json` 同样受益**（重试在适配器内，与驱动路径无关）。
- **TUI 黑洞挂起不再是无限**：空闲超时让 `complete` 必然返回 ⇒ `RunFailed` ⇒ `is_turning` 复位（此前需 Ctrl-D 强退）。
- **重试治「抖动」不治「停服」（如实）**：有界退避总等待 ≤ ~60 s ⇒ **超过约一分钟的 provider 故障仍会终止回合**。
  这是必要的界（无界重试会把「长期不可用」变成「永久占用会话」），但**不得**把 48h 无人值守理解成已能容忍长时间停服。
- **额度耗尽不再退避**：命中额度名单的 `429` 立即失败 ⇒ 48h 任务不会在账户欠费后在退避里空转，`RunFailed` 更快到达。
- **重试对排障可见、对 TUI 不可见**（决策 7）：一行 `logging`；不进事件流。
- **遗留（记账）**：`ModelError` 仍是纯 `String`（上层不可区分）—— 这是**长期的既成事实**（加结构是 breaking，
  也意味着重试决策**永远留在适配器内、无法上升到 `ys-loop`**）；两个完整性缺口未修；
  **退避期间 Ctrl+C 无效**（决策 4 末条：`complete` 无取消入参，改核心 trait 才治）；
  「吐过 delta 后断流」不重试（Considered Options，归 `llm-api-unavailable`）。
- **测试需要本地回环 TCP 服务器**（该文 §约束：`.no_proxy()` 直连、并行安全端口），是新增的测试基础设施。
- 上位设计：[`../arch/long-horizon-completion/design.md`](../arch/long-horizon-completion/design.md) §4.8；
  需求上下文：[`../arch/long-horizon-completion/context.md`](../arch/long-horizon-completion/context.md)；
  被 supersede 的原始范围界定：[`../arch/llm-api-unavailable/context.md`](../arch/llm-api-unavailable/context.md) §范围外。
