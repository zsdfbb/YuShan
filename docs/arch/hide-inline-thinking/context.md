# hide-inline-thinking — 架构上下文

> 需求：coding-agent 的 TUI **不展示被 `<think>…</think>` 之类标签包裹的思考过程**，
> 只展示「具体轮次结果或操作内容」（工具调用 / 回合摘要 / 最终答复）。
>
> 注：`docs/arch/hide-inline-thinking/` 目录在本次探索前**已存在但为空**（无任何既有文件），
> 故在此新写入 `context.md`，未覆盖任何内容。

## 概述

思考内容目前有**两条来源**，二者待遇不同：

1. **独立字段**：provider 在 SSE 里给 `reasoning_content`，适配器发
   `ModelEvent::ThinkingDelta` → `AgentEvent::ModelThinkingDelta` → `TranscriptLine::Thinking`，
   **默认已隐藏**（`/thinking on` 才以暗色斜体显示）。
2. **内联标签**：provider 把思考塞进 `delta.content`，形如 `<think>…</think>` /
   ` Thinking…<｜end▁of▁thinking｜>` 等人话前缀。它走 `ModelEvent::TextDelta` →
   `AgentEvent::ModelTextDelta` → `TranscriptLine::Assistant`，**与正式答复不可区分，一律显示**。

本需求针对的是**第 2 条**：内联标签未被剥离，导致「思考过程」当作 assistant 正文进入对话。
`docs/arch/tui-stack-revisit/design.md` §3.5 已把它记为「用户报告的问题 #3 的真因」，
并在 P4 计划「think 标签剥离 + `/thinking` 开关」；**但该 P4 从未落地**
（全库仅 `ModelThinkingDelta` 路径有 thinking 处理，无任何标签剥离代码）。

## Provider 实证（DeepSeek / MiMo / MiniMax）

三类模型送思考的通道**并不相同**，逐家核实结果：

| provider | 思考通道 | 现状 | 需要内联标签剥离？ |
|---|---|---|---|
| **DeepSeek** | 独立字段 `reasoning_content`（官方 `deepseek-reasoner` / V3 thinking） | 已有 `ProviderCompat::deepseek().has_reasoning_content = true` → `ThinkingDelta` | **不需要**（官方路径）；第三方网关把 ` thinking` 塞进 content 的情况另算 |
| **Xiaomi MiMo** | 独立字段 `reasoning_content` | 本仓库**尚无该 provider**；pi changelog 记录 MiMo thinking 模式多轮用 `reasoning_content` 回放（#4678） | **不需要** |
| **MiniMax** | **内容内联标签** | `ProviderCompat::minimax()` 的 `has_reasoning_content = false`，标签**原样进对话** | **需要** |

MiniMax 是**本仓库内有真凭实据的一例**：`tmp/skill-probe/`（真实 `ys-coding-agent --json` + 真实凭证，
模型 MiniMax-M3）的输出中，`content` 里是**多段**内联思考，标签后再接正文：

```text
<think>The user is asking ... Let me load it.</think>

<think>The skill instructs me to run several git commands ...</think>

## 仓库状态摘要
| 项目 | 说明 |
...
```

⇒ 单条消息里 ` thinking…<｜end▁of▁thinking｜>` 会**反复出现**，过滤器必须支持**多块循环**，不是只处理开头一块。

参考实现（**同仓库 vendor 的 CodeWhale**：`tmp/Codewhale/`）：

- provider 级开关 `reasoning_stream_style`：`separate_field`（`reasoning_content`）/ `inline_tags`（`<think>` 在 `delta.content` 里）/ `none`。见 `tmp/Codewhale/docs/CONFIGURATION.md:1887`。
- 流式状态机 `InlineReasoningTagState { inside_think, pending }` + `trailing_tag_prefix_len`，
  用「扣住可能是标签前缀的尾部字节」解决**跨片标签**；见
  `tmp/Codewhale/crates/tui/src/client/chat.rs:3774-3840`。
- 其非流式 `strip_thinking_tags` 覆盖 **`think` / `thinking` / `reasoning`** 三种标签，见
  `tmp/Codewhale/crates/tui/src/session_manager.rs:3338`。
- 缺口：CodeWhale 的**流式**状态机只认 `<think>`（硬编码），非流式的三标签集合没有同步进去。
  本设计应把「标签表」做成状态机的输入，避免同样的分叉。

## 现有架构

### 模块边界

| 层 | crate / 文件 | 职责（与本需求相关） |
|---|---|---|
| 适配器 | `adapters/model-openai-compatible/src/lib.rs` | 解析 SSE，边收边 emit `ModelEvent`；流末收敛 `ContentBlock::Text` |
| 通用循环 | `crates/ys-loop/src/basic.rs` | `Forwarder` 把 `ModelEvent` 1:1 映射为 `AgentEvent` |
| TUI | `crates/ys-tui-coding/` | `App` 累积 transcript；`draw.rs` 渲染；`commands.rs` 处理 `/thinking`、`/copy` |
| 应用 | `apps/coding-agent/src/main.rs` | `-p` / `--json` 消费事件；退出时回放 transcript |

### 核心抽象

- `ModelEvent::{TextDelta, ThinkingDelta}`（`crates/ys-model/src/event.rs`）—— model 级增量。
- `AgentEvent::{ModelTextDelta, ModelThinkingDelta}`（`crates/ys-event/src/event.rs`）—— runtime 级事件。
- `TranscriptLine::{Assistant, Thinking}`（`crates/ys-tui-coding/src/transcript.rs:15,20`）——
  **结构化**记录；`Thinking` 是独立类型，不与 `Assistant` 混行（这正是能按开关隐藏的前提）。
- `ProviderCompat`（`adapters/model-openai-compatible/src/compat.rs`）—— 现有布尔式 provider 差异
  （`has_reasoning_content` / `tool_calls_as_text` / `supports_stream_usage`），**尚无标签相关字段**。

### 关键数据流（内联 thinking 的完整污染面）

```text
SSE delta.content  ──► StreamAccumulator::handle (lib.rs:211)
                        ├─(逐片即时)──► ModelEvent::TextDelta        ──┐
                        └─(累积)──► self.text ──► ContentBlock::Text ──┤
                                                                       │
        Forwarder (ys-loop/basic.rs:30) ──► AgentEvent::ModelTextDelta ┤
                                                                       │
   ┌───────────────────────────────────────────────────────────────────┘
   ├─► App::append_stream(Assistant)  (app.rs:238) ──► draw.rs 渲染（原样显示）
   ├─► session 历史（ContentBlock::Text）  ← 下回合会**回喂给模型**（token 成本 + 行为影响）
   ├─► /copy (commands.rs:404) 取最后一条 Assistant
   ├─► 退出回放 transcript_plain_lines (draw.rs:541)
   ├─► -p 模式 print_delta_text / consume_print_events (main.rs:210,504) 写 stdout
   └─► --json 模式逐行 Envelope（含原始 text）
```

要点：内联思考**不是纯显示问题** —— 它同时进入了会话历史（被持久化、被回喂）、
`-p` stdout 与 `--json` 输出。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| SSE / OpenAI 兼容协议 | 增量来源 | 标签可能被 TCP 分片切在任意位置 |
| `ProviderCompat` | provider 差异开关 | 现为布尔堆叠，设计文档提过要结构化（gap-closure F11） |

## 约束

- **技术**
  - 流式不可假设「一个 delta 内标签完整」：`<thi` + `nk>`、`</thi` + `nk>` 必须正确跨片。
    任何对单条 delta 做字符串匹配的方案都会把半截标签漏到屏幕上。
  - 适配器现有不变量：累积的 `text` 与逐片 emit 的 `TextDelta` **逐字节相等**（lib.rs:220 注释）。
    若在适配器剥离，必须同时维护这条不变量（累积与 emit 都要过滤）。
  - `ys-loop` 是**通用循环**，不含任何 provider 方言字符串（`normalize_finish_reason` 的分工注释：
    「方言归适配器，词汇归 `ys-model`」）。标签名属方言 ⇒ 不应下沉到 `ys-loop`。
  - 适配器里的 `tool_calls_as_text`（MiniMax）说明 `content` 里也可能出现工具调用 JSON ——
    过滤规则需避免误伤。
- **性能**：流式路径要求 O(1) 级状态机，不得为找闭合标签而无限缓冲（正常文本要立即透传）。
- **演进**：已知标签方言不止一种（`<think>`、`<thinking>`、DeepSeek 的
  `<｜begin▁of▁thinking｜>` 等）；新增 provider 不应改核心逻辑。
- **组织/约定**：新增模块/接口须对应 `docs/design.md` §12 测试矩阵；关键逻辑需变异测试
  （本项目多次出现「测试全绿但真 bug 仍在」）。

## 需求范围

> Q1–Q4 已经用户确认（见「已定决策」）；标注项仍待确认（见「未澄清问题」）。

### 范围内

- TUI 对话区**默认不显示**内联标签包裹的思考内容（assistant 正文只保留标签外的部分）；
  显式 `/thinking on` 时应可回看（见「已定决策」D4）。
- 流式过程中不得出现半截标签（`<thi`、`nk>` 等）闪烁。
- 标签之外的正式答复保持原样、实时流入。

### 范围外（明确不做的）

- 不做 thinking 级别控制（`/thinking high|low` 之类请求参数）——那是 gap-closure 的 S5 议题。
- 不引入 markdown / diff 等富渲染。
- 不删除 `/thinking` 开关本身（保留「想看时显式打开」的能力）。
- 不改 provider 请求参数（不注入 `reasoning_effort` 等）。

### 关键场景

1. **完整标签**：一次 delta 给出 `<think>分析</think>最终答复` → 只见「最终答复」。
2. **跨片标签**（核心回归）：`<thi` / `nk>分析</thi` / `nk>答复` → 不出现半截标签，答复完整。
3. **未闭合**：模型只输出 `<think>分析…`（被截断 / 流中断）→ 不得把 `<think>` 字面量显示出来。
4. **`/thinking on`**：显式开启后，被剥离的内联思考以 `TranscriptLine::Thinking` 回看（D4）。
5. **多方言**：`think` / `thinking` / `reasoning` 三对标签均生效（D3）。

## 已定决策

- **D1（范围）**：只剥离**被标签包裹**的思考；不压制未包裹的模型旁白（工具调用间的叙述照常显示）。
- **D2（剥离层）**：在**适配器层**（`StreamAccumulator`）剥离，而非 TUI 展示层。
  理由：会话历史 / `-p` / `--json` / 退出回放一并干净；内联思考从源头转成 `ThinkingDelta`，
  复用既有 `TranscriptLine::Thinking` 与 `/thinking` 开关。
- **D3（标签表）**：**内置**三对标签 —— `think` / `thinking` / `reasoning`，**默认开启**，
  不做 per-provider 配置。**不收** DeepSeek 特殊 token（`｜…thinking…｜`）：
  官方 API 走 `reasoning_content` 用不到，等真在网关实测到再补。
  依据：`tmp/Codewhale/crates/tui/src/session_manager.rs:3338` 的非流式标签集合。
- **D4（`/thinking on`）**：被剥离的内联思考**可回看** —— 适配器把它转成 `ThinkingDelta`，
  TUI 侧维持现状（默认 `thinking_visible = false`，`/thinking on` 以暗色斜体显示）。
- **D5（退出回放）**：退出回放**遵守 `thinking_visible`**（方案 A）—— 默认不打印 thinking，
  `/thinking on` 后再退出才打印。改动点：`transcript_plain_lines`（`draw.rs:541`）不再无条件
  `build_text_with(..., true)`。
- **D6（误伤取舍）**：接受「正文里讨论 `<think>` 字面量会被误剥」的风险（简单优先）；
  不为此加转义 / 白名单机制。

## 未澄清问题

（无剩余阻塞项。Q1–Q6 均已定：D1–D6。）

补充说明（`-p` / `--json`，由 D2 推导，无需再决策）：

- `-p`：内联思考转成 `ThinkingDelta` 后，`print_delta_text` 本就只认 `ModelTextDelta` ⇒ 自动隐藏。
- `--json`：会多出 `ModelThinkingDelta` 事件 —— 完整事件流语义下本应如此，保持不变。

## 后续建议

- **已产出**：[`design.md`](./design.md)（三候选对比 + 融合方案）与
  [`docs/adr/0025-hide-inline-thinking-tags.md`](../../adr/0025-hide-inline-thinking-tags.md)（决策记录）。
- 设计与实现应覆盖的**接缝问题**：状态机放适配器内（`StreamAccumulator`）还是抽成
  可复用的纯组件（`ys-model` 级 normalizer，供未来适配器 / `MockModel` 复用）——
  已在 `design.md` §4/§8 比较并**否决后者**（方言归适配器 + YAGNI）。
- **必测回归**（本需求最易实现错处）：跨片标签（`<thi` + `nk>`）、未闭合标签、**单条消息内多块**标签
  （MiniMax-M3 实测就是多块）、标签外正文实时透传、不破坏「累积 text == 逐片 emit 逐字节相等」不变量。
- 实施顺序建议：先补流式状态机单测（红）→ 再改适配器（绿）→ 处理 Q6 的回放可见性。
