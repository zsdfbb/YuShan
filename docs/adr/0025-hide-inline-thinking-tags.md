# 0025 — 内联思考标签在适配器剥离：内置三标签 + 跨片扣留状态机，思考改走 `ThinkingDelta`

把「provider 塞进 `delta.content` 的思考标签」在**适配器层**剥出来，转成既有的
`ModelEvent::ThinkingDelta`。标签表**内置**三对（`think`/`thinking`/`reasoning`）、**默认开启**，
不做 per-provider 配置；流式用**跨片扣留**状态机（只扣「可能是标签前缀」的尾部，≤11B），
无标签热路径保持零额外分配。**TUI / `ys-loop` / `ys-event` 零改动**（除退出回放一行）。

## Status

accepted（2026-10-03）

## 背景

- **需求**（原话）：「coding-agent 不要展示思考的过程，只展示具体轮次结果或者操作内容」；
  后续澄清限定为「**`<think>` 之类包裹的**思考过程」。
- **实测三类 provider 的通道不同**（见 [`../arch/hide-inline-thinking/context.md`](../arch/hide-inline-thinking/context.md)）：
  DeepSeek / 小米 MiMo 用**独立字段** `reasoning_content`（适配器已处理）；
  **MiniMax-M3 走 `delta.content` 内联多段 `<think>`** —— 这是本仓库内有真凭实据的一例
  （`tmp/skill-probe/`，真实 `ys-coding-agent --json` + 真实凭证）。内联标签因此被当作正文。
- **污染面不止 TUI**：`content` 会经 `ContentBlock::Text` 进**会话历史**（下回合回喂给模型），
  并由 `-p` 的 `print_delta_text` 写 stdout、由 `--json` 逐行输出。
- **既有能力可复用**：`reasoning_content` 已经走 `ModelThinkingDelta` → `TranscriptLine::Thinking`，
  TUI 默认隐藏、`/thinking on` 以暗色斜体显示。缺的只是把内联标签接进同一条链路。
- **同仓库参考实现**：CodeWhale（`tmp/Codewhale/`）用 provider 级 `reasoning_stream_style`
  （`separate_field`/`inline_tags`/`none`）+ `trailing_tag_prefix_len` 扣留内核；
  但其**流式**只硬编码 `<think>`，与其**非流式**三标签 `strip_thinking_tags` 分叉（教训）。
- `docs/arch/tui-stack-revisit/design.md` §3.5 早已把它记为「问题 #3 真因」并在 P4 列过，
  **从未落地**（全库无任何标签剥离代码）。

## 决策

### 1. 剥离层 = 模型适配器，不是 TUI

在 `StreamAccumulator`（`adapters/model-openai-compatible/src/lib.rs`）里把 `content` 切分为
「正文 / 思考」后，思考改发 `ThinkingDelta`。**否决 TUI 展示层剥离**：那只清对话区，
会话历史、`-p`、`--json` 仍带思考，其中「历史回喂」是隐性 token 成本。

### 2. 标签表内置三对，默认开启，不做 per-provider 配置

`<think>` / `<thinking>` / `<reasoning>`（精确成对；三者互不为前缀，精确匹配无歧义）。
恒开、不受 `has_reasoning_content` 门控：适配器要服务 `custom`/未知端点，per-provider 开关
在未知端点上等于默认失效。**不收** DeepSeek 特殊 token（`｜…thinking…｜`）—— 官方 API 用不到，
等真在网关实测到再补。

### 3. 思考复用既有 Thinking 链路（默认隐藏，`/thinking on` 可回看）

不新增 `AgentEvent` / `TranscriptLine` 变体。内联思考 → `ModelThinkingDelta` →
`TranscriptLine::Thinking`，继承现有的默认隐藏与开关语义。

### 4. 跨片扣留状态机（`inline_thinking.rs`）

单条 delta 可能把标签切成 `<thi` + `nk>`，且单条消息内可有多段。状态机两个字段：
`inside: Option<usize>`（配对闭合，`<think>a<reasoning>b</reasoning>c</think>` 的内层标签
不提前关闭外层）+ `hold: String`（只存当前模式下某分隔符的**真前缀**，上界 = 最长分隔符
`</reasoning>` 12B − 1 = 11B）。**无 `<` 且 `hold` 空**走快路径：原文直接 move 进事件，
保持现状的零额外分配；只有含 `<` 的片走切分慢路径。

### 5. 未闭合与假前缀的收敛

`finish` 先 `flush`：思考态残留判 `Thinking`（未闭合的 `<think>` 绝不以字面量进入答复）；
正文态的假前缀（`hi <thi`）判 `Text`（不吞字）。

### 6. 不变量：Text 片段「累积 + emit」成对

唯一写 `self.text` 的点与 `emit(TextDelta)` 在同一分支、用同一份切分结果；`hold` 中的字节
既不进 `text` 也不 emit。⇒ 既有不变量「`ContentBlock::Text` == 逐片 `TextDelta` 拼接」
在所有观察点成立（`lib.rs:533` 的回归继续覆盖无标签路径）。
内联思考**不写入 `self.reasoning`**（该字段从未被读取，避免为思考再留一份死内存）。

### 7. 退出回放遵守 thinking 开关（supersede `design-final` 的「总是打印」）

`transcript_plain_lines`（`draw.rs:541`）由 `build_text_with(app, width, true)`（无条件打印）
改为 `build_text_with(app, width, app.thinking_visible)`。原「退出后没有 `/thinking` 开关可用了」
的理由，在「用户明确要求默认不显示思考」下不再成立。

### 8. 只剥标签包裹的，不压未包裹旁白

工具调用之间的模型叙述（「我先看看 X」）没有可靠信号，且事后判据（文本后是否跟工具调用）
**只能等整轮结束**，会牺牲实时流式。明确不做。

## Considered Options

- **TUI 展示层剥离**：改动最小，但会话历史 / `-p` / `--json` 照旧泄漏；历史回喂还有 token 成本。**否决**。
- **把状态机放 `ys-model` 并公开可配置 API**（可扩展优先候选）：与既有分工「方言归适配器，
  词汇归 `ys-model`」（`normalize_finish_reason` 注释）冲突；D3 已定不做配置，公开泛型 +
  可配置标签表是投机抽象（当前仅 1 个适配器，`MockModel` 不需要内联剥离）。**否决**；
  独立文件 + `pub(crate)` 已足够解耦与单测。
- **状态机内联进 `lib.rs`**（最小复杂度候选）：省一个文件，但 ~120 行实现 + ~200 行测试塞进
  已 ~430 行的 `lib.rs`，增重明显。**否决**。
- **每 delta 先 `push_str` 进 `pending` 再 drain、逐段 `to_string()`**（A/B 的形态）：相对现状
  （`content` 直接 move 进事件、零额外分配）是新增拷贝与分配。**否决**，改走无 `<` 快路径。
- **用 `bool` 表示「在思考内」**（性能候选）：多标签下无法配对闭合。**否决**，用 `Option<usize>`。
- **加常驻 `scratch` 复用缓冲**：只服务「上片停在半标签」这一罕见路径，一次性 `format!` 足够。**否决**。
- **per-provider 开关 / 结构化 `ProviderCompat` 字段（gap-closure F11）**：本轮**不做**，
  但接口留好 —— 将来 `ReasoningStreamStyle{None, SeparateField, InlineTags(&[ThinkingTag])}`
  只需换 `const` 表的实参，组件零改动。
- **误伤防护（转义 / 白名单）**：模型正文里讨论 `<think>` 字面量会被误剥。**接受**（简单优先）。

## Consequences

- **会话历史不再含思考** ⇒ 后续回合回喂的 token 减少，且不再强化模型输出标签。
- **`-p` 输出不变**（只认 `ModelTextDelta`）；**`--json` 会多出 `ModelThinkingDelta` 事件** ——
  完整事件流语义下本应如此，保持不变。
- **已知风险（接受）**：正文里出现**未闭合** `<think>` 会把其后全部内容吞进 Thinking（隐藏）。
  截断回复同理。D6 明确接受，不加转义 / 白名单。
- **新增文件** `adapters/model-openai-compatible/src/inline_thinking.rs`（~120 行 + 测试）；
  `draw.rs` 一行（D5）；`StreamAccumulator` 增一个字段与 `finish` 的 flush。
- **`self.reasoning` 仍为只写不读**（既有遗留）；本设计不为它新增写入。
- 方案对比矩阵与实施步骤：[`../arch/hide-inline-thinking/design.md`](../arch/hide-inline-thinking/design.md)；
  需求上下文与实证：[`../arch/hide-inline-thinking/context.md`](../arch/hide-inline-thinking/context.md)。
