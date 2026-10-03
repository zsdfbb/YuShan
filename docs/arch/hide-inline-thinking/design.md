# hide-inline-thinking — 设计方案

> 需求与约束见 [`context.md`](./context.md)（决策 D1–D6）。本文件记录方案对比、
> 最终设计（融合 A/C）与被否方案的理由。决策记录见
> [`docs/adr/0025-hide-inline-thinking-tags.md`](../../adr/0025-hide-inline-thinking-tags.md)。

## 1. 问题一句话

部分 OpenAI 兼容 provider 把思考塞在 `delta.content` 里（MiniMax-M3 实测：单条消息内
**多段** `<think>…</think>`），现有适配器原样当作正文 → 对话区、会话历史、`-p` 都出现思考。

## 2. 业界调研

| 参考 | 做法 | 借鉴 / 教训 |
|---|---|---|
| **CodeWhale**（同仓库 `tmp/Codewhale/`） | provider 级 `reasoning_stream_style`（`separate_field` / `inline_tags` / `none`）；流式状态机 `InlineReasoningTagState{inside_think, pending}` + `trailing_tag_prefix_len` 扣留「可能是标签前缀的尾部」 | **借鉴其扣留内核**。教训：其**流式**路径硬编码只认 `<think>`，而**非流式** `strip_thinking_tags` 认 `think`/`thinking`/`reasoning` —— 两处逻辑分叉。本设计用「**同一 const 表驱动同一状态机**」消除分叉 |
| **pi**（同仓库 `tmp/pi/`） | 依赖独立字段 `reasoning_content` / `reasoning` / `reasoning_text`，不做内联标签解析 | 印证「独立字段是主流」；内联标签是网关/兼容层的补充路径，故本过滤器**只作用于 `content`**，不动 `reasoning_content` 既有分支 |
| 我们自己的实测 | MiniMax-M3 走 `delta.content` 内联多段 `<think>`；DeepSeek / MiMo 走 `reasoning_content` 独立字段 | 见 `context.md` 的「Provider 实证」 |

## 3. 三个候选方案

三个候选都在**适配器层**、都用**跨片扣留状态机**，主要差异在三处：归属、API 面、热路径分配。

| | **A · 最小复杂度** | **B · 可扩展优先** | **C · 性能优先** |
|---|---|---|---|
| 归属 | `model-openai-compatible/src/lib.rs` 内私有 | **`ys-model` 新模块**，公开 | 适配器新增独立文件 |
| API | 私有，`const` 标签表 | `pub InlineThinkingFilter<'t>`，标签表可配置 | `pub(crate)`，`const` 表 |
| 状态 | `inside: Option<usize>` + `pending` | 同 A | `in_thinking: bool` + `hold`(≤11B) + `scratch` |
| 热路径 | 每 delta 先 `push_str` 进 pending 再 drain、逐段 `to_string()` | 同 A | **无 `<` 快路径：原文 move，零额外分配** |
| 净值 | ~120 行，0 新文件 | ~170 行，跨 crate | ~120–160 行，新文件 |

## 4. 对比

| 维度 | A | B | C |
|---|---|---|---|
| 实现复杂度 | **最低** | 中（公开泛型 + lifetime） | 中高（下标算术） |
| 流式热路径开销 | 每 delta 多 ~2 次拷贝 + 1 次分配 | 同 A | **≈ 基线（0 额外分配）** |
| 演进灵活性 | 低 | **高**（可配置、可复用） | 低 |
| 依赖方向 | ✅ 方言留在适配器 | ⚠️ 把 provider 方言放进 `ys-model` | ✅ 同 A |
| 风险 | 低 | **过度抽象**（当前只有 1 个适配器、D3 明确不做配置） | 可读性（slow path 下标算术） |

**关键观察**：A/B 的「每 delta `pending.push_str` + drain + `to_string`」相对现状是一次**新增拷贝与分配**
—— 现状是 `content` 直接 move 进事件、零额外分配（`lib.rs` 内容分支）。流式回复每片仅几字符，
这个代价虽小但可直接避免，且**只有 C 避免了它**。

## 5. 最终方案（融合 A + C）

取 **A 的归属与 API 面**（适配器本地、私有、`const` 表、`inside: Option<usize>` 配对闭合）
+ **C 的快路径**（无 `<` 时零拷贝 move）；**弃 C 的 `bool` 状态与 `scratch` 字段**（前者不满足配对闭合，
后者为罕见路径增复杂度）。

### 5.1 模块划分

| 位置 | 内容 | 职责 |
|---|---|---|
| `adapters/model-openai-compatible/src/inline_thinking.rs`（新增） | `InlineThinkingFilter`、`Segment`、`SegmentKind`、`const TAGS` / `MAX_HOLD`、`scan` / `push_seg` / `match_delimiter` / `is_proper_prefix` | 纯逻辑、零依赖、可独立单测；不知道 SSE / 网络 / tokio |
| `adapters/model-openai-compatible/src/lib.rs`（改） | `StreamAccumulator` 增 `filter` 字段；`handle` 的 `content` 分支改走过滤器；`finish` 先 `flush` | 把片段落到「累积 `text` + emit 事件」两点 |
| `crates/ys-tui-coding/src/draw.rs`（改） | `transcript_plain_lines` 改传 `app.thinking_visible`（D5） | 退出回放遵守 thinking 开关 |

`ys-loop` / `ys-event` / `ys-tui-coding` 的 Thinking 链路 / `-p` **零改动**：适配器发出
`ModelEvent::ThinkingDelta` 后，既有链路自动把它送进 `TranscriptLine::Thinking`（默认隐藏、
`/thinking on` 可见）；`-p` 的 `print_delta_text` 只认 `ModelTextDelta`，天然不打印。

`--json` 会因此对标签型 provider **多出 `ModelThinkingDelta` 事件**（既有 `AgentEvent` 为
`#[non_exhaustive]`、消费端按事件类型分派，属契约内增量，无需改事件定义）。

### 5.2 数据结构与状态机

```rust
/// D3：内置三对标签（精确、成对；默认开启，不做 per-provider 配置）。
/// 注：`<think>` 与 `<thinking>` 互不为前缀（第 7 字节 `>` vs `i`），精确匹配无歧义。
const TAGS: [(&str, &str); 3] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];
/// 最长分隔符 `</reasoning>` = 12 字节 ⇒ 真前缀上界 = 11。
const MAX_HOLD: usize = 11;

pub(crate) enum Segment { Text(String), Thinking(String) }

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentKind { Text, Thinking }

pub(crate) struct InlineThinkingFilter {
    /// Some(i) = 正在 TAGS[i] 包裹的思考内；None = 正文。
    /// 用下标而非 bool：闭标签**严格配对**，`<think>a<reasoning>b</reasoning>c</think>`
    /// 的内层标签原样留在 Thinking 中，不会被 `</reasoning>` 提前关闭。
    inside: Option<usize>,
    /// 跨片扣留：当前模式下某分隔符的**真前缀**。恒为 ASCII，≤ MAX_HOLD 字节。
    hold: String,
}

impl InlineThinkingFilter {
    pub(crate) fn new() -> Self;

    /// 快路径：`hold` 空且本片不含 `<` ⇒ 整片同属当前段，调用方可直接 move 转发。
    /// （分隔符必含 `<`，故无 `<` 必无标签。）
    pub(crate) fn passthrough(&self, content: &str) -> Option<SegmentKind>;

    /// 慢路径：切分本片（含此前扣留的前缀），返回本片可确定的段。
    pub(crate) fn feed(&mut self, content: &str) -> Vec<Segment>;

    /// 流末冲刷 `hold`：思考态 → Thinking（未闭合思考不外泄）；
    /// 正文态 → Text（末尾半截 `<thi` 确属字面文本，不吞字）。幂等。
    pub(crate) fn flush(&mut self) -> Vec<Segment>;
}
```

**精确规格**（实现须逐行与之一致；`kind` = `inside.is_some()` 决定 Text/Thinking）：

```rust
/// 扫一片 hay（可能已含此前扣留的前缀），就地切分；返回需扣留的尾部（`""` = 无）。
/// 前置不变量：`pos` 单调、每个切点都在 char 边界、返回值是当前模式某分隔符的真前缀。
fn scan<'a>(hay: &'a str, inside: &mut Option<usize>, out: &mut Vec<Segment>) -> &'a str {
    let mut pos = 0usize;
    while pos < hay.len() {
        let Some(rel) = hay[pos..].find('<') else {
            push_seg(out, kind(inside), &hay[pos..]);           // 无 '<' ⇒ 必无标签
            return "";
        };
        let lt = pos + rel;                                    // '<' 的绝对下标（char 边界）
        match match_delimiter(&hay[lt..], *inside) {           // ① 先试「完整命中」
            Some((d, idx)) => {
                if lt > pos { push_seg(out, kind(inside), &hay[pos..lt]); }
                *inside = if inside.is_some() { None } else { Some(idx) };  // 开↔闭 翻转
                pos = lt + d.len();
            }
            None if is_proper_prefix(&hay[lt..], *inside) => {  // ② 再试「真前缀」
                if lt > pos { push_seg(out, kind(inside), &hay[pos..lt]); }
                return &hay[lt..];                             // 可能是跨片标签 → 整段扣留
            }
            None => {                                          // ③ 都不是 ⇒ '<' 是字面量
                push_seg(out, kind(inside), &hay[pos..lt + 1]);
                pos = lt + 1;
            }
        }
    }
    ""
}

/// 追加一段，并合并同 kind 的相邻段（否则「字面量 `<`」会把一段正文切成两段）。
/// 跨 feed 不合并 —— 下游 TUI 的 append_stream 本就会合并相邻同 kind 行。
fn push_seg(out: &mut Vec<Segment>, kind: SegmentKind, s: &str) {
    if s.is_empty() { return; }
    match (out.last_mut(), kind) {
        (Some(Segment::Text(t)), SegmentKind::Text) => t.push_str(s),
        (Some(Segment::Thinking(t)), SegmentKind::Thinking) => t.push_str(s),
        _ => out.push(Segment::new(kind, s.to_string())),
    }
}

fn feed(&mut self, content: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    if self.hold.is_empty() {
        let leftover = scan(content, &mut self.inside, &mut out);
        self.hold.push_str(leftover);                          // ≤ MAX_HOLD
    } else {                                                   // 仅「上片停在半个标签里」走这里
        let buf = format!("{}{}", self.hold, content);
        let leftover = scan(&buf, &mut self.inside, &mut out);
        self.hold.clear();
        self.hold.push_str(leftover);
    }
    out
}

fn flush(&mut self) -> Vec<Segment> {
    if self.hold.is_empty() { return Vec::new(); }
    let k = if self.inside.is_some() { SegmentKind::Thinking } else { SegmentKind::Text };
    vec![Segment::new(k, std::mem::take(&mut self.hold))]      // 非空
}
```

关键约定（实现者必读）：
- **判定顺序不可换**：`match_delimiter` 先于 `is_proper_prefix`；否则 `s == d` 会被误判成前缀。
- `match_delimiter(s, mode)`：按**当前模式**的分隔符表取**第一个** `d` 满足 `s.starts_with(d)`；
  正文模式 = 三开标签，思考模式 = **该标签自己的**闭标签。
- `is_proper_prefix(s, mode)`：`s.len() < d.len() && d.starts_with(s)`，`d` 同样取当前模式的表。
- **段必须合并**：`scan` 的所有产出经 `push_seg`，同 kind 的**相邻**段合并。否则真值表里
  `</think>plain` 的字面量 `<` 与后续正文会成为两个 `Text` 段（字节内容等价，但事件数不符）。
- 段恒非空：`push_seg` 对空串直接跳过；切点非空由 `lt > pos`（段前）与 `pos < hay.len()`
  （段尾）结构性保证。实现里加 `debug_assert!(!s.is_empty())` 兜底。
- 翻转规则：`None → Some(idx)`（开）/ `Some(_) → None`（本标签的闭）。**思考态只匹配自己的闭标签**，
  故内层异名标签不动状态（见真值表末行）。

真值表（`feed` 一次喂入，`inside` 初值 `None`；`flush` 单列）：

| 输入 | 产出段 | 结束时 `hold` |
|---|---|---|
| `a<think>b</think>c` | Text("a")、Thinking("b")、Text("c") | `""` |
| `<think>a</think>` | Thinking("a") | `""` |
| `<think></think>` | （无段） | `""` |
| `</think>plain` | Text("</think>plain") | `""` |
| `a<thinkx` | Text("a<thinkx") | `""` |
| `<<think>a</think>` | Text("<")、Thinking("a") | `""` |
| `a<thi` | Text("a") | `"<thi"` |
| `a<` | Text("a") | `"<"` |
| `<think>ab<reasoning>cd</reasoning>ef</think>g` | Thinking("ab<reasoning>cd</reasoning>ef")、Text("g") | `""` |
| `hi <thi` + `flush` | Text("hi ")、后 Text("<thi") | `""` |
| `pre<think>abc</thi` + `flush` | Text("pre")、Thinking("abc")、后 Thinking("</thi") | `""` |

契约注释要求：`TAGS` 旁注明「`reasoning` 标签映射到 `ThinkingDelta`（与领域词 `Thinking` 同义）」。

**规格已验证**：§5.2 的三函数 + `push_seg` 合并规则已逐行照抄为一次性影子模型并跑通 ——
真值表全 11 行、10 个含标签串在**每个 char 边界**的 2 段 / 3 段切分、5000 组「随机文本 × 随机 1–5 片
切分」、`hold` 上界（思考态 11 / 正文态 10），均满足 C1、`flush` 后 `hold == ""`，且**归一化（合并相邻同 kind 段）后**段序列与「一次性 `feed`」等价。合并规则为必需项：
去掉它，真值表第 4/5 行（字面量 `<` 后接正文）的段序列即不自洽。

### 5.3 数据流

```text
SSE delta.content ─► StreamAccumulator::handle
  filter.passthrough(&content)
    ├─ Some(Text)     ─► self.text.push_str(&content); emit(TextDelta{content})   // move，零拷贝
    ├─ Some(Thinking) ─► emit(ThinkingDelta{content})
    └─ None           ─► for seg in filter.feed(&content) { emit_segment(seg) }
                          Text(t)     ⇒ self.text.push_str(&t); emit(TextDelta{t})
                          Thinking(t) ⇒ emit(ThinkingDelta{t})      // 不写 self.reasoning
Done / finish ─► for seg in filter.flush() { emit_segment(seg) }
                └─► ContentBlock::Text = self.text（已不含思考）→ 会话历史 / 回放干净
```

注意：内联思考**不写入 `self.reasoning`**（该字段只在 `reasoning_content` 分支被写、
且从未被读取，属只写状态）；事件是唯一出口，避免为思考再留一份死内存。

实现约束（来自 `review.md` 的易修复项）：

- **保留** `handle` 里既有的 `if !content.is_empty()` 守卫 —— 否则空 delta 会走快路径发出空
  `TextDelta`，TUI 会落一条空 Assistant 行。
- `finish` 改为 `fn finish(mut self)`；`flush` 在**错误检查之后、构造 `content_blocks` 之前**执行，且幂等。

### 5.4 四个边界

| 边界 | 处理 |
|---|---|
| **跨片标签** | `hold` 扣留「可能是某分隔符真前缀」的尾部（≤ `MAX_HOLD` = 11B），下片拼接后复判。`<thi`+`nk>`、`</thi`+`nk>`、`<reas`+`oning>` 都不漏；切点恒在 char 边界（分隔符全 ASCII，`<` 不可能是多字节序列的一部分） |
| **单消息多块** | `feed` 是循环，每闭合一块继续扫；MiniMax-M3 的 `<think>a</think>\n\n<think>b</think>\n\n正文` 逐块剥离，正文原样拼接 |
| **未闭合** | `finish` 的 `flush`：思考态残留 → Thinking（`<think>` 不会以字面量进入答复）；正文态的假前缀 → Text（不丢字） |
| **标签外实时透传** | 无 `<` 走快路径整片立即 emit；慢路径每确认一段立即 emit。最坏延迟 = 多扣 11 字节（一个 delta），**不整条消息缓冲** |

### 5.5 正确性论证

**C1 · 不丢字节**：任意切分下，`concat(Text 段) + concat(Thinking 段)` = 原始 `content` 去掉标签字面量。
理由：`scan` 中每个字节恰好被覆盖一次 —— 要么落在 `[pos, lt)` / `<` 的产出段里，要么被 `hold` 扣留，
而 `hold` 必由下一次 `feed` 或 `flush` 释放。⇒ 既不静默吞字节，也不重复产出。

**C2 · 累积 == 逐片 emit**：`self.text` 恒等于所有已 emit `TextDelta` 的拼接。
理由：唯一写 `self.text` 的点与 `emit(TextDelta)` 在 `Text` 分支内成对、同源；`hold` 中的字节
既不进 `text` 也不 emit，释放时同样成对。⇒ 任意观察点（每 delta 后、`finish` 后）成立。
既有回归 `streamed_text_deltas_concatenate_to_final_text_byte_for_byte`（`lib.rs:533`）继续覆盖无标签路径。

**C3 · 思考不入历史**：`ContentBlock::Text` 只由 `self.text` 构造，`Thinking` 段永不进入它。
⇒ 会话历史 / 退出回放 / `-p` 不含思考。

**D6 代价（误剥是双重后果）**：内联剥离发生在写入 `self.text` **之前**，故被误判为思考的
**普通正文**不仅 TUI 隐藏，还会**从 `ContentBlock::Text` / 会话历史中一并消失** —— 模型后续
轮次也看不到。这是 D6「简单优先、不加边界约束」明确接受的取舍；复访触发条件见 §8。

**边界收敛**：`flush` 必把 `hold` 归零（实现后为 `""`）；`inside` **不跨响应**（每个响应一个新过滤器）。

> C1/C2 是纯函数性质，可脱离网络用「随机文本 + 随机切分」的属性测试钉住（见 §6）。

### 5.6 D5：退出回放

`transcript_plain_lines`（`draw.rs:541`）现为 `build_text_with(app, width, true)`（无条件打印）。
改为 `build_text_with(app, width, app.thinking_visible)`，并更新其文档注释
（原文「thinking 总是打印 —— 退出后没有 `/thinking` 开关可用了」不再成立）。

## 6. 测试计划

**过滤器单测（`inline_thinking.rs` 内，无网络）**
1. 单块：`a<think>b</think>c` → `[Text("a"), Thinking("b"), Text("c")]`
2. 三标签各一：`think`/`thinking`/`reasoning` 均生效
3. 多块：`<think>a</think>mid<thinking>b</thinking>tail` → Text=`midtail`
4. **跨片穷举**：对 `<think>b</think>` 的**每个 char 边界切点**切两片，断言段序列与一次性 `feed` 相等
5. 配对闭合：`<think>a<reasoning>b</reasoning>c</think>d` → Thinking=`a<reasoning>b</reasoning>c`、Text=`d`
6. 未闭合思考：`pre<think>abc` + `flush` → Text=`pre`、Thinking=`abc`（`abc` 在 feed 阶段
   即作为 Thinking 发出，`flush` 无产出）；`pre<think>abc</thi` + `flush` → Thinking 拼接=`abc</thi`
7. 尾部半截开标签：`hi <thi` + `flush` → Text=`hi <thi`（不丢字）
8. 实时性：`passthrough("hello")` 命中，且 `hold` 为空
9. **真值表逐行**（§5.2 全 11 行，含 `<<think>`、`</think>plain`、`a<thinkx`）
10. **同 kind 合并**：`</think>plain` → 单段 Text(`</think>plain`)；`a<thinkx` → 单段 Text(`a<thinkx`)
    （防止「字面量 `<`」把一段正文切成两段）
11. **属性测试**：随机文本 × 随机切分，断言 C1（按 kind 拼接 == 去标签原文）、`flush` 后 `hold == ""`，
    以及「**把相邻同 kind 段合并后**，段序列与一次性 `feed` 等价」——原始段序列会因切分点而异
    （`feed("a")`+`feed("b")` 是两个 `Text` 段，一次性 `feed("ab")` 是一个），故须先归一化
12. 扣留上界：进入 `<reasoning>` 思考态后逐字符喂 `</reasoning`，断言 hold 峰值 = 11
    （正文态最长真前缀为 10 = len(`<reasoning>`)-1）
13. UTF-8：`你好<think>思考</think>世界` 多切点无损坏

**适配器单测**：跨片内联流驱动 `StreamAccumulator`，断言无任何 `TextDelta` 含思考、
`ContentBlock::Text == concat(TextDelta)`、`ThinkingDelta` 拼接 = 思考原文；
与 `reasoning_content` 并存不串扰；无标签流仍只发 `TextDelta`（守 `thinking_delta_only_emitted_when_compat_enabled`）。

**D5**：`transcript_plain_lines` 在 `thinking_visible=false` 时无 Thinking 行、`=true` 时有。

**变异测试**（注入 bug → 期望变红）：

| 注入 | 变红 |
|---|---|
| 去掉尾部扣留（半标签立即 emit） | 4 |
| `feed` 去掉循环（只处理第一块） | 3 |
| `flush` 把思考态残留判为 Text | 6 |
| `inside` 用 bool 且固定找 `</think>` | 5 |
| `Text` 分支只 `push_str` 不 emit（或反之） | 适配器不变量 |
| `passthrough` 漏判 `hold.is_empty()` | 4 |
| `finish` 不调 `flush` | 7 |
| 切分点少/多前进 1 字节 | 1 / UTF-8 |
| `transcript_plain_lines` 写死 `true` | D5 |

## 7. 实施步骤

1. 新增 `inline_thinking.rs`，先写过滤器单测（红）
2. 实现状态机 → 单测绿
3. 接入 `StreamAccumulator`（`passthrough` / `feed` / `finish` flush）+ 不变量测试
4. `draw.rs` D5 + 测试；并**同步文档**：`design-final/coding-agent-tui.md` §5 的「thinking 总是打印」、
   `docs/design.md` §12 Coding Agent 行的「退出打印完整对话」措辞、`draw.rs:539` 注释
5. `cargo test --workspace` + `cargo clippy --all-targets` + `cargo fmt`

## 8. 被否方案与否决理由

- **B（放 `ys-model` + 公开可配置 API）**：否决。① 与既有分工「方言归适配器」冲突
  （`normalize_finish_reason` 的注释原则）；② D3 已明确「内置、不做 per-provider 配置」，
  可配置标签表与公开泛型是**投机抽象**（当前只有 1 个适配器，`MockModel` 不需要内联剥离）；
  ③ 状态机与 IO 的解耦，靠「独立文件 + `pub(crate)`」已足够，无需跨 crate 公开。
- **多块思考之间的纯空白 `Text` 段（如 `\n\n`）**：**不折叠**。折叠需要在跨 delta 上维持
  「夹在 Thinking 之间」的判定，成本高、收益仅为少一行空白；TUI 上表现为若干空行，接受。
- **剥离的可观测性**：**不做**。加计数需经日志文件（不能 `eprintln`，会冲花 TUI），
  收益有限；真出现「答案莫名缺失」再按复访触发条件处理。
- **`tool_calls_as_text`（MiniMax）内容含标签被误剥**：理论风险、非现实风险 —— 该字段在生产代码中
  **从未被使用**（仅 `compat.rs` 定义 + 测试断言），`content` 不会因它变成工具 JSON。记录备查。
- **加「开标签须位于文本边界（起始 / `\n` 后）」约束**以缓解误剥与未闭合吞正文：**暂缓**，
  作为复访触发条件（真出现误剥投诉时再加；过滤器需多记「上一字符是否换行」）。
- **A 的 lib.rs 内联放置**：否决。~120 行状态机 + ~200 行测试塞进已 ~430 行的 `lib.rs` 会明显增重；
  独立文件同样满足「私有、可单测」。
- **A/B 的每 delta 拷贝**：否决。现状是 move 进事件、零额外分配；无 `<` 快路径能以极低成本保住该特性。
- **C 的 `bool` 状态与 `scratch`**：否决。`bool` 不满足「不同标签配对闭合」；`scratch` 只服务
  「上片停在半标签」这一罕见路径，用一次性 `format!` 足够，不值得加常驻字段。

## 参考

- `tmp/Codewhale/crates/tui/src/client/chat.rs:3774-3840`（跨片扣留内核）
- `tmp/Codewhale/crates/tui/src/session_manager.rs:3338`（三标签集合）
- `tmp/Codewhale/docs/CONFIGURATION.md:1887`（`reasoning_stream_style` 三态）
- `adapters/model-openai-compatible/src/lib.rs`（`StreamAccumulator`）
- `crates/ys-tui-coding/src/draw.rs:541`（退出回放）
