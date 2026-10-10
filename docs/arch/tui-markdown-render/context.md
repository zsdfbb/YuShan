# TUI Assistant 文本 Markdown 渲染 — 架构上下文

> 需求一句话：把 `ys-tui-coding` 对话区里 assistant 文本的**纯文本渲染**，升级为
> **结构化 Markdown 渲染（无语法高亮）**，解析用成熟库（方向已定：pulldown-cmark 一类）。
>
> **⚠ 这是一次决策反转**：Markdown 渲染在本仓被**明确列为已砍的产品红线**
> （`docs/arch/gap-closure/context.md:10,390`、`docs/arch/tui-stack-revisit/design.md:152`、
> `docs/arch/tui-pi-codewhale-borrow/design.md:23,34,77,151`）。本轮若落地，须显式记录反转
> 并回改相关文档 —— 见「未澄清问题」。

---

## 概述

`ys-tui-coding` 当前把 `AgentEvent::ModelTextDelta` 的增量文本合并成一条
`TranscriptLine::Assistant(String)`，渲染时**只按显式 `\n` 切行、整行单色**，模型输出里的
`` ``` code ``` ``、`` `inline` ``、`**bold**`、`# 标题`、`- 列表` 全部**原样作为字符显示**。
本需求引入 Markdown 解析，把这些结构转成 ratatui 的带样式 `Line`/`Span`，
让对话区可读性接近用户对「coding agent」的预期。

---

## 现有架构

### 模块边界

- **唯一 TUI crate**：`crates/ys-tui-coding/`（不存在独立的 `ys-tui`）。
- **依赖方向（编译器强制）**：`ys-protocol → ys-tui-coding → apps/coding-agent`。
  `ys-tui-coding` **不依赖 `ys-runtime`**（不认识 `Agent`）——由 crate 边界强制（`CLAUDE.md:33`）。
- 对外只经 `ys-protocol` 的 `Request` / `Boundary` / `Outbound<V>` 与 app 线程通话。
- **UI 线程是普通同步 `fn`，无 runtime 上下文**（用 `blocking_send`，async 上下文会 panic）。

### 核心抽象

| 抽象 | 位置 | 说明 |
|---|---|---|
| `App` | `src/app.rs:23-63` | 持有 `transcript: Vec<TranscriptLine>`、`scroll_offset`、`follow`、`input`、`completion` |
| `TranscriptLine` | `src/transcript.rs:15-37` | `User`/`Assistant(String)`/`Thinking`/`Tool{..}`/`Summary`/`Error`/`System` —— **结构化、未渲染**的中间表示 |
| `App::apply_event` | `src/app.rs:104` | AgentEvent → transcript 的状态机 |
| `App::append_stream` | `src/app.rs:293-313` | 流式增量：末尾同类则 `push_str` 续写，否则新起一条 |
| `build_text_with` | `src/draw.rs:414-427` | transcript → `Text`（**每帧重建的纯函数**） |
| `push_transcript_line` | `src/draw.rs:429+` | 逐条 `TranscriptLine` → `Vec<Line<'static>>` |
| `transcript_plain_lines` | `src/draw.rs:541-552` | **纯文本**回放路径（退出写回主屏 / `/copy`） |

`Assistant` 分支现状（`src/draw.rs:445-450`）：

```rust
TranscriptLine::Assistant(s) => {
    // 真 wrap 交给 Paragraph；这里只把显式换行拆成独立的 Line
    for part in s.split('\n') {
        out.push(Line::from(part.to_string()));   // ← 无 Span、无样式
    }
}
```

### 关键数据流

```
ModelTextDelta{text}  (ys-event/src/event.rs:11-13)
  └─ app 侧包成 Outbound::Event(Envelope)      apps/coding-agent/src/app_loop.rs
       └─ mpsc → UI 线程 rx_out
            └─ run.rs event_loop 排空: app.apply_outbound(out)   run.rs:161
                 └─ App::apply_event → append_stream(Assistant)   app.rs:147,293
                      └─ TranscriptLine::Assistant(String) 追加/续写
           ← 下一帧 terminal.draw(|f| draw::ui(f, app))          run.rs:155
                └─ draw_transcript → build_text_with → push_transcript_line
                     └─ Paragraph::new(text).wrap(Wrap{trim:false}).scroll(..)  draw.rs:124-128
```

**渲染接缝**：`build_text_with` 是**每帧重建的纯函数**，`push_transcript_line` 是**逐条纯函数**
—— Markdown 渲染天然只需替换 `Assistant` 分支，无需改动状态机、信道或 `App`。

**滚动与 wrap 的耦合**：滚动以**视觉行**为单位，行数由 `Paragraph::line_count(width)`
（`src/draw.rs:102-106`，靠 ratatui `unstable-rendered-line-info`）算出 —— 用的是 ratatui
内部 `WordWrapper` 的同一实现，保证 CJK 列宽与渲染严格一致。**新增样式不得改变字符宽度**，
否则滚动上限与画面失配。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| `ratatui` | 三 pane 渲染 | 0.29（feature `unstable-rendered-line-info`） | 唯一渲染落点 `draw.rs` |
| `crossterm` | 事件流 / raw mode / alt-screen / 括号粘贴 | 0.28（feature `event-stream`） | |
| `tokio` | 仅 `mpsc` 类型 | 1（feature 仅 `sync`） | UI 线程不跑 runtime |
| `ys-protocol` / `ys-core` / `ys-event` / `serde_json` | 内部 | path | |
| **Markdown 解析库** | **本轮拟新增** | 待定（方向：pulldown-cmark 一类） | `Cargo.lock` 现无任何 markdown/高亮/tree-sitter 依赖 |

**样式现状**：**无 `Style`/`Theme`/`Palette` 抽象**（全仓零命中）。所有颜色/修饰
**硬编码内联在 `draw.rs`**（`Color::Green` 用户前缀、`Color::Red` 错误、`Color::DarkGray`
状态/系统/思考、`Modifier::ITALIC`/`REVERSED`），数据层（`transcript.rs`/`view.rs`）零样式
—— 符合「数据/显示分离」。

---

## 约束

### 技术

- **不依赖 `ys-runtime`**；UI 线程无 runtime 上下文；`ys-protocol` 零 tokio —— 均不可破。
- 渲染产物是 `ratatui::text::Line<'static>` / `Span<'static>`，**必须 owned**（`Text<'static>`）。
- **样式只在 `draw.rs` 决定**；`TranscriptLine` 保持「结构化、未渲染」不变（数据/显示分离）。
- **流向是增量的**：assistant 文本由多个 `ModelTextDelta` 拼接而成，渲染器**每一帧看到的是
  累积到当前为止的字符串** —— 必须容忍**半截 Markdown**（未闭合 fence / 悬空的 `**` /
  只写了 `##` 还没换行）。
- **不得改动 `transcript_plain_lines` 的语义**（退出回放 + `/copy`）：它是纯文本通道，
  与 alt-screen 渲染是两条路（是否也让回放带样式见「未澄清问题」）。
- 现有设计文档（`docs/design-final/coding-agent-tui.md`）§10 已列若干缺口，本轮**不应**顺带
  扩大范围。

### 性能

- `build_text_with` **每帧重建**（UI 循环约 50ms 排空 + 300ms 节拍）。若对每条 assistant 文本
  每帧做一次 Markdown 解析，长会话下是**平方级**开销 —— 需要**缓存**（解析结果与宽度无关，
  只有 wrap 依赖宽度，故可按「文本内容」缓存解析产物，见「未澄清问题」）。
- 帧预算参考：现渲染开销 <1ms（`docs/arch/ratatui-replace/context.md:117`）。
- 二进制体积敏感：历史基准 release ~5.7MB / stripped debug ~9.7MB；引入解析库的增量需测量。

### 演进

- **这是产品红线的反转**：Markdown 渲染此前被正式砍掉，理由是 pi ~1k 行 / CodeWhale ~4k 行
  的成本与「后台 agent + 简洁 TUI」的定位。**本轮须显式记录反转**（本仓惯例：写 ADR），
  并回改至少三处记载「已砍」的文档。
- 本仓有「注释只写当前设计、变更叙事进 git log / ADR」的硬约定（`CLAUDE.md` 工作约定）。
- 新增依赖与「最小核心 / 静态组合 / 处处手写」的风格存在张力 —— 需在 ADR 中说明为何选库而非手写。

### 组织

- 单人维护 → 切分须「低耦合、可独立验证」，每项对应 `docs/design.md §12` 测试矩阵条目。
- 关键逻辑要做**变异测试**（本仓血的教训），流式/UTF-8 边界尤其注意。

---

## 需求范围

> 以下「范围内 / 范围外」按用户本轮回答确定：
> **范围 = 仅 assistant 文本**；**深度 = 结构化、无语法高亮**；**实现 = 引入成熟解析库**。

### 范围内

- 仅 **`TranscriptLine::Assistant`** 的渲染升级为结构化 Markdown。
- 元素覆盖（结构化，**不做语法高亮**）：
  - **块级**：标题、无序/有序列表、引用块、围栏代码块（无边框/无底色，**整块单一代码色**，**不做语法高亮**）、分隔线。
  - **行内**：行内代码、粗体、斜体、链接（展示形式待定，见未澄清问题）。
  - 保持 **CJK 列宽正确**（依赖 ratatui 宽度机制，不得偏移）。
- 引入 Markdown 解析库（方向：pulldown-cmark 一类），**首次为非 TUI 逻辑新增依赖**。
- 容忍**流式半截输入**（每帧对累积串重解析）。
- 必要的**缓存**以避免每帧重解析的平方开销。

### 范围外（明确不做）

- **语法高亮**（syntect / tree-sitter）—— 本轮用户明确排除。
- **Theme / Palette / 配色系统抽象** —— 仍用现有硬编码色（红线之一，不在此轮反转）。
- **Diff Viewer**、工具输出（`TranscriptLine::Tool`）的渲染改造、状态栏/布局改动。
- 其他 `TranscriptLine` 变体（`User`/`Thinking`/`Summary`/`Error`/`System`）的渲染改动。
- 鼠标、可配快捷键、多语言/多主题。
- 改动 `Request`/`Boundary`/`Outbound` 协议、信道、线程模型、`App` 状态机。
- `-p` / `--json` 的输出格式（它们是纯文本/JSON 通道，不涉 TUI 渲染）。

### 关键场景

- **S1 流式生成**：文本逐块到达，含未闭合 ` ``` ` fence 时不得 panic、不得错乱；闭合后样式正确落位。
- **S2 代码块**：``` ```rust ... ``` ``` 显示为带边界的块（内容不染色），缩进与非代码正文可区分。
- **S3 中文混排**：`**加粗**中文`、CJK 列表项等在 wrap 后列宽对齐，滚动上限与画面一致。
- **S4 长会话**：数百条 assistant 消息下滚动/重绘无可感卡顿（验证缓存有效）。
- **S5 退化输入**：畸形 / 嵌套过深 / 超长单行 / emoji 的 Markdown 不 panic、不越界。
- **S6 退出回放**：退出 TUI 写回主屏 scrollback 的内容仍可读（与渲染是否一致见未澄清问题）。
- **S7 `/thinking on`**：thinking 行渲染不受影响；assistant 与 thinking 的样式互不串扰。
- **S8 跨行拷贝**：用终端原生选择跨多行拷贝时，剪贴板里**不含任何边框竖线**；拷贝得到的正文
  可直接粘进编辑器 / 文档。

---

## 拷贝友好（2026-10-09 新增的横切约束）

**现象**：用户跨行选择时，Chat 窗格左右两条 `│`（`draw.rs:125` 的 `Block::bordered()`）被一并拷入剪贴板。

**根因（不可绕过的机制）**：全仓**无鼠标处理**，用户用的是**终端原生选择**；原生选择复制的是
**屏幕上实际存在的字形**——「所见即所拷」，**应用无法过滤**。故只要装饰字符与内容同处一行，
跨行选择就必然带上它。

**由此确立的渲染原则 —— 装饰字符即被拷贝内容**：

| 位置 | 决定 |
|---|---|
| Chat 窗格边框 | **只去左右竖线，保留顶部标题线**（`Block` 改用 `Borders::TOP` 一类）；内容行两侧不再有 `│` |
| 表格 | **保留方框线**（`┌─┬─┐`）—— 表格是唯一例外：框线即其可读性，拷贝带框线被接受（且与采用现成库的默认形态一致） |
| 代码块 | **不用 `│` 左槽**，只用缩进 + **单一前景色**区分（无底色，2026-10-10 修订）⇒ 拷贝代码得到纯代码 |
| 引用 / 列表 | 标记选**拷贝友好**形式：引用 `> `、列表 `- `（而非 `│ ` / `• `）⇒ 拷出来仍是有效 Markdown |
| 标题 | 去 `#` 前缀时**拷贝得到纯文本标题**（可接受）；是否保留 `#` 见设计文档 |

**这条原则会反向约束每个元素的画法**，故必须在设计阶段就并入元素 → 样式映射表，不能落地时再补。
它与「让终端原生 scrollback / selection 干活」的既有哲学一致：**渲染越克制，可拷贝性越好**。

---

## 未澄清问题

- [ ] **决策反转的落地**：是否写 ADR 正式记录「Markdown 渲染红线解除」？（本仓惯例倾向：是）
      并回改 `docs/arch/gap-closure/context.md:10,390`、`docs/arch/tui-stack-revisit/design.md:152`、
      `docs/arch/tui-pi-codewhale-borrow/design.md:23,34,77,151` 的「已砍」表述。**待用户确认**
- [ ] **解析库选型**：pulldown-cmark（CommonMark + GFM 扩展，事件流式 API，无依赖）vs comrak（更全但更重）？
      版本与 feature（是否启用 GFM 表格/删除线/任务列表）。**待用户确认**
- [ ] **元素子集**：表格、任务列表、删除线、脚注是否纳入？（用户定了「结构化、无高亮」但未列元素清单）**待用户确认**
- [ ] **链接展示**：仅加下划线/颜色，还是用 OSC 8 终端超链接（点击可开）？OSC 8 与 alt-screen/回放兼容性待查。**待用户确认**
- [ ] **缓存策略**：按「assistant 文本内容」缓存 `Vec<Line<'static>>`（内容不变则复用）？
      缓存放 `App`（需改状态结构）还是 `draw.rs` 侧（需 `App` 之外的可变槽）？**本轮最需定的实现契约**
- [ ] **退出回放 / `/copy` 是否也渲染 Markdown**：倾向**否**（保持纯文本、可复制可搜），
      但需确认 `transcript_plain_lines` 是否复用同一解析（若复用会改变回放观感）。**待用户确认**
- [ ] **半截 fence 的显示策略**：流式中未闭合的代码块**立即按代码块渲染**（会随闭合变化）
      还是**先按纯文本、闭合后再转样式**（避免闪烁）？**待用户确认**
- [ ] **未闭合行内标记**（`**bold` 无收尾）：`**` 原样显示还是不显示？**待用户确认**
- [ ] **性能验收口径**：以什么度量「可接受」？（如 S4 的帧时间阈值 / 与现状对比倍数）
- [ ] **依赖体积**：新增解析库对 release 体积的增量是否可接受（历史基准 ~5.7MB）？

---

## 后续建议

1. 由本仓惯例，**先定「决策反转」与「解析库选型」两项**（需用户回答上表前两条），
   再进 `arch-design`。
2. 建议用 `arch-design` 设计：
   - `draw.rs` 内的 Markdown 渲染接缝（`push_transcript_line` 的 Assistant 分支 → 新模块 `md_render`）
   - 缓存的所有权与失效规则
   - 元素 → `Style` 的映射表（用现有硬编码色，不引 Palette）
   - 半截输入的容错与显示策略
3. 建议用 `prototype` 验证两个高风险假设：
   - **半截 Markdown 每帧重解析**在长会话下的帧开销（含缓存收益）
   - **CJK + 样式 span 的 wrap 一致性**（`Paragraph::line_count` 与画面是否仍严格一致）
4. 落地后按本仓约定补**变异测试**（流式边界、UTF-8 切分、未闭合结构）。
