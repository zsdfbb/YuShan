# tui-text-selection — 架构上下文

## 概述

在 `ys-tui-coding` 里做**应用内文本选区**：鼠标拖选 → 应用自己画高亮 → 松手写剪贴板，
使「复制对话内容」不再依赖终端提供的修饰键逃生（`⇧` 拖选 / Option+拖选）。

对应 `docs/arch/` 内的需求名 **D**（见 `docs/adr/0050-tui-mouse-wheel-scroll.md` 的「代价」段与
`docs/arch/tui-markdown-render/design.md` §2.8 的「前提已变」脚注）。

## 现有架构

### 模块边界

```text
ys-protocol ──► ys-tui-coding ──► apps/coding-agent
```

选区的落点**全在 `ys-tui-coding` 内部**：

- **不认识 `Agent`**（crate 边界强制，不依赖 `ys-runtime`）—— 选区与 agent 状态无关，纯 UI 态。
- **`commands.rs` 是纯逻辑、零终端**（模块头纪律）—— 它不能发转义序列，故 `/copy` 现在走
  `app.pending_clipboard` → `run.rs::write_clipboard` 的交接。新选区若也产生剪贴板载荷，
  必须复用**同一条交接缝**，不得在 `commands.rs` 里直接 `println!` / 写 stdout。

涉及文件：

| 文件 | 现状职责 | 与本需求的关系 |
|---|---|---|
| `run.rs` | 终端 setup / 事件循环 / 恢复序列 / `/copy` 的 OSC 52 | 捕获序列要改；剪贴板写到这一层 |
| `events.rs` | `handle_event` / `handle_key` / `handle_mouse`（**只认滚轮**） | 鼠标分支要扩成「按下 → 拖动 → 松开」 |
| `draw.rs` | 三 pane 渲染；`chat_block()` 内区几何 | 高亮要叠在已渲染的 Chat 区域上 |
| `layout.rs` | transcript 视窗化（`TranscriptLayout`），**唯一的视觉行号空间** | 命中测试的行号来源 |
| `app.rs` | `App` 状态容器（`transcript` / `scroll_offset` / `follow` / `layout` / `pending_clipboard`） | 选区态要挂在这里 |
| `prompter.rs` | 模态浮层的 `Prompter` 实现，自己 `read()` 事件 | 浮层开着时选区必须让位 |

### 核心抽象

- **`TranscriptLayout`（`layout.rs`）**：`starts[i]` = 第 `i` 个条目的**块起始全局视觉行号**，
  `entries[i].row_starts[k]` = 条目内第 `k` 行首的视觉行号，末位 = 条目总行数。
  **这是全仓唯一的「视觉行」编号空间**，滚动数学与渲染都查它。选区若要按行/按格定位，必须接在
  同一份数据上，否则会出现「画的位置」与「选的位置」错位（ADR-0045 已因这类错位返工一次）。
- **`Window`（`layout.rs`）**：`window(top_row, height)` 返回覆盖视窗的行 + `skip_rows`（首行被
  上沿切开的偏移）。渲染用 `Paragraph::scroll((skip_rows, 0))`。
- **`App`（`app.rs`）**：可变状态容器，单线程（UI 线程）持有；`commands.rs` 的 `apply_local`
  是唯一「命令 → 状态变更」入口。
- **`pending_clipboard: Option<String>`（`app.rs`）**：UI → `run.rs` 的剪贴板交接槽，
  消费点在 `event_loop` 里（`app.pending_clipboard.take()` → `write_clipboard`）。

### 关键数据流

```text
crossterm Event::Mouse / Event::Key
        │
        ▼  events.rs::handle_event
  handle_mouse（现状：只认 ScrollUp/ScrollDown）
        │
        ▼
  App 状态变更（scroll_offset / follow / …）
        │
        ▼  run.rs::event_loop 每帧
  terminal.draw(|f| draw::ui(f, app))
        │
        ├── chat_area() → chat_block().inner() → 内区几何
        ├── layout.window(top_row, inner_h) → 视窗行
        └── Paragraph::scroll(skip_rows) → 屏幕上的一段
```

**关键的既有事实**：`draw::ui` 拿到的 `Frame` 在 ratatui 0.30 里**不暴露已渲染的 buffer**
（`Frame` 无 `buffer()` / `buffer_mut()`；`CompletedFrame.buffer` 只在 `terminal.draw` **返回后**
才拿得到）。⇒ 「渲染完之后再去读屏、反查某格是什么字」这条路**在当前帧内不通**。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| `ratatui` | 渲染 / `Paragraph` / `line_count` 折行 | 0.30 | `default-features = false`，只留 crossterm + `unstable-rendered-line-info` |
| `ratatui-core` | `Frame` / `Buffer` / `Terminal::draw` | 0.1.2 | 间接依赖，`Frame` 就是 `ratatui_core::terminal::Frame` |
| `crossterm` | 事件与转义序列 | 0.29 | `MouseEvent { kind, column, row, modifiers }`；`KeyModifiers` 在鼠标事件里**可用** |
| `tui-markdown` | assistant 文本渲染 | 0.3.10 | 写死字形（`- ` / `>` / 表格框线），无法过滤 —— 影响「拷出来的文本」 |
| `tokio` | 仅 mpsc | 1 | UI 线程**不跑 async**（`blocking_send`；有 runtime 上下文会 panic） |

## 约束

### 技术约束

1. **折行必须与渲染严格一致**：wrap 用 `Paragraph::line_count(width)`（ratatui 内部
   `WordWrapper` 同一实现）。自写折行会 CJK 列宽错位。
2. **`ys-core` / `ys-event` / `ys-protocol` 不含 tokio**；`ys-tui-coding` **不依赖 `ys-runtime`**。
   新模块不得破坏这两条。
3. **UI 线程必须无 runtime 上下文**（`blocking_send` 会 panic）—— 选区逻辑不得引入 async。
4. **`Frame` 不提供渲染时的 buffer 访问**（见上）。要在某格上画高亮，只能：
   （a）把高亮**烘进** `Line`/`Span` 的 style（渲染前就知道要选什么），或
   （b）渲染后由 `run.rs` 拿 `CompletedFrame.buffer` 改写并**重绘一帧**（撕裂 + 下一帧被
   `draw::ui` 覆写），或（c）另建 `Buffer` 自绘并 `frame.render_widget` 覆盖（遮挡未选中部分）。
5. **crossterm 的 `EnableMouseCapture` 一次发出 `?1000h?1002h?1003h?1015h?1006h`**（无法只开
   1002 不开 1003），故「只要拖拽事件」这个粒度**做不到** —— 要么全开（含 `Moved` 悬停），
   要么自己手写 `execute!` 序列。
6. **浮层自读事件**：`prompter.rs` 自己 `read()`，`_ => {}` 掉鼠标。浮层期间主循环不跑，
   故选区状态必须在浮层结束时清掉，否则会拿旧选区去对应新画面。

### 已实测事实（探针已跑通，代码已删）

1. **Buffer 里宽字符的续格是普通空格，不是空串**（`Cell::symbol() == " "`）。按「空 symbol 就跳过」读回会得到 `"中 文"` —— 每个 CJK 后多一个空格。`Cell::skip` 在 ratatui 0.30 已废弃且恒 `false`，**不可用**。
2. **正确读法是复现 diff-writer 的宽度规则**（`ratatui-core-0.1.2/src/buffer/buffer.rs:589-598`：
   `skip = max(skip, cell_width) - 1`）。按它读回**无损**，实测覆盖 CJK / 全角标点 / emoji。
3. **「独立渲染内区」与「带 Block 常规渲染」在内区逐格一致**（`parity = true`）⇒ 重画一遍内区取字形不会漂移。

由此得到一条比「烘进 Span」更便宜的路径：**正常渲染后，把同一份 `window.lines` + 同一宽度再渲染进
自己的 `Buffer`**，按上述规则读出字形。它**不需要改 `layout.rs` / `draw.rs` 的签名**，且窗口内容 /
宽度 / `top_row` 不变时该 Buffer 可缓存 ⇒ 拖选是 O(1) 重读 + O(可见行) 覆盖。

### 选用方案（M1：视口选区）的边界

- **`Buffer` 只有视口大小**，没有 scrollback ⇒ **不加自动滚动则一次最多只能选一屏**。
  自动滚动因此不是锦上添花，而是「能否选长段」的开关。
- **拖动期间必须把 `follow` 置 `false`**，否则流式输出会持续下移视口、屏幕坐标失效。
  这条**白拿**：`app.rs::sync_scroll_state` 的增量补偿（`!follow && total_rows > previous
  ⇒ scroll_offset += Δ`）本就保证「脱离底部后视窗原地不动」。
- **resize 必须清选区**：
  - **宽度变** ⇒ `layout.sync` 清空全部折行记忆（`layout.rs:120`），`starts[]` 全部重编，
    补偿被 `!width_changed` 显式排除 ⇒ 同一屏幕坐标下是**完全不同的字**，无映射关系可救。
  - **高度变** ⇒ 折行不变、行号稳定，只是 `top_row = max_scroll - scroll_offset` 变了
    （`max_scroll = total_rows - inner_h`）⇒ 理论上可救（重算 `y = global_row - top_row`），
    但实现收益低，第一版按「一律清」处理。
  - **松手之后 resize 无害** —— 文本已进剪贴板。损失仅限「拖到一半调整窗口」这一罕见组合。

### 性能约束

1. 滚动 / 重绘成本必须**与会话长度解耦**（ADR-0045 的 N4 不变式 + 视窗化）。选区不得退化为
   O(整段 transcript)。
2. 拖选期间每一帧都要重绘 —— 高亮的计算必须是 **O(可见行)**，不能整段重排。
3. 现有渲染有 ~100ms 的流式节流（`RENDER_THROTTLE`）；选区高亮**不能**被这个节流拖住（否则
   拖动手感像掉帧）。

### 演进约束

1. **ADR-0030 的红线**把「点击 / 拖拽 / 悬停」列为维持砍掉；ADR-0050 已反转「滚轮」一项。
   本需求要反转「拖拽」，属**第二次反转**，必须新开 ADR 并回改 ADR-0030 / ADR-0050 的措辞。
2. **ADR-0050 的「代价」段**明写用户须 `⇧`+拖选或 `/copy`；本需求落地后该段要改口。
3. `docs/arch/tui-markdown-render/design.md` §2.8「拷贝友好」的前提（无鼠标处理）已变，
   本需求再变一次（有选区 ⇒ 能过滤字形）。该节四条决定（去左右竖线 / 表格保留框线 /
   代码块无 gutter / 引用列表保持 `> ` `- `）在「应用内选区」下**仍应保留**：过滤字形是
   应用可以做的，但保留可读性优先。
4. `docs/design-final/coding-agent-tui.md` §18.4（键位）/ §18.6（已知缺口）需回写。

### 组织约束

- 项目要求「关键逻辑做变异测试」（注入 bug 验证测试能抓到）。选区有三个高危逻辑点必然要覆盖：
  **屏幕上沿半行**（`skip_rows`）、**宽字符（CJK）列偏移**、**选区反向拖拽**（anchor 在 focus 之后）。
- 单测已能覆盖：`layout.rs` 的行号空间、`draw.rs` 的等价性用例（实体 `TestBackend` 可读 buffer）。
  ⇒ **命中测试与提取可以纯逻辑测**，不需要真终端。真实终端观感仍属 `MANUAL_ACK`（ADR-0050 先例）。

## 需求范围

### 范围内（D 的最小版，依据上一轮对话的三家对照）

- 鼠标**左键拖选** Chat 区域 → 应用内高亮（含跨条目、跨 wrap 后的视觉行）。
- **松手即复制**（copy-on-select）：松手时把选中文本写进系统剪贴板，走 OSC 52（复用
  `run.rs` 现有的 `clipboard_sequence` / `write_clipboard`）。
- 选中文本的**提取**：按 `TranscriptLayout` 的视觉行 + 宽度折行，拷出**屏幕上的字形**
  （保留 `- ` / `>` / 框线；即「拷贝友好」原则不变）。
- 双击选词 / 三击选行 → **待定**（见未澄清问题）。
- 选区随内容变化**失效**（流式新增行、`/new`、`/resume`）—— 避免高亮漂到别的文字上。
- 滚轮**行为不变**（仍滚聊天）。滚轮与拖选共存。
- **保留 `/copy`**，并与拖选形成明确分工（见下节「与 `/copy` 的分工」）。

### 拖选拷出的是**纯文本**，不是 Markdown 原文

这一条是**既有设计的延续**，不是 M1 的缺陷。`styles.rs` 刻意把装饰字形置空：

| 方法 | 返回值 | 效果 |
|---|---|---|
| `heading_marker(level)` | `""` | 屏幕上没有 `#` |
| `code_block_fence()` | `""` | 屏幕上没有 ` ``` ` 围栏 |
| `alert_icon(kind)` | `""` | 不引入多列宽 emoji（保护「块级装饰宽度 = 1」不变量） |

理由是 `docs/arch/tui-markdown-render/design.md` §2.8「拷贝友好」：复制走屏幕字形，故屏幕
不该显示 nécessaire 噪声。后果是：

- 拷出 **保留**：`- ` 列表、`> ` 引用、表格框线、缩进、换行 —— 仍是可读、可粘贴的纯文本。
- 拷出 **丢失**：`#` 标题、` ``` ` 围栏、`**` 粗体标记、`[text](url)` 链接语法。

M1 复制屏幕字形 ⇒ **完全继承上述行为**。若需要 Markdown 原文，那是另一条通路（`/copy`）。

### 与 `/copy` 的分工（两者互补，不冗余）

| | `/copy` | 拖选（M1） |
|---|---|---|
| 内容来源 | `TranscriptLine::Assistant(String)` 的**原始字符串**（`commands.rs:510`） | 屏幕字形（Buffer 读回） |
| 格式 | **Markdown 原文**（`#`、` ``` `、`**` 都在） | 去标记的纯文本 |
| 范围 | 最后一条非空 assistant 回复 | 任意可见内容（含提问、工具输出） |
| 入口 | 4 个字符 + Enter | 一次拖拽 |

**唯一会让 `/copy` 变成死命令的场景**：拖选也改成复制 Markdown 原文。那样技术路线要整体推翻
（不读 Buffer，改为「视觉行 → 源字符范围」映射 = M2 的映射问题），成本远高于 M1。**不采纳。**

### 范围外（明确不做的）

- **不做点击**（`Down/Up` 不带拖动 ⇒ 不触发任何动作）。ADR-0030「不绑点击」仍有效。
- **不做悬停**（`Moved` 仍忽略）—— 不做 hover 高亮 / tooltip / 链接点击。
- **不做右侧滚动条**。
- **选区不跨滚动存活**（M1 的自然行为；跨滚动需转 M2）。
- **选区不跨 resize 存活**（宽度变则坐标系整个换掉；见上「选用方案的边界」）。
- **不做搜索**、不做「按条目选中」、不做 OSC 52 的**读**（`?52;c;?`）。
- **不改 `commands.rs` 的纯逻辑纪律**，不改 crate 依赖方向。

### 关键场景

- 场景 1：鼠标从第 3 行拖到第 7 行松手 ⇒ 7 行内容进剪贴板，高亮保留到下一次点击/滚动。
- 场景 2：拖选出跨 `User` → `Assistant` 两个条目 ⇒ 拷出两段，中间带条目间留白对应的换行。
- 场景 3：向上滚动后拖选（`top_row` 非 0，首行被视窗上沿切开）⇒ 选中文本**不含**未显示的
  上半行。
- 场景 4：拖到 Input / Status 区 ⇒ 不选（或钳制到 Chat 边界）——语义待定。
- 场景 5：选区存在时模型继续流式输出 ⇒ 选区失效（不留「高亮在动」的错觉）。
- 场景 6：选中一段带 ``` 的代码块 ⇒ 拷出**纯代码**（无围栏、无左侧 gutter）；需要围栏时
  用 `/copy` 拿 Markdown 原文。

### 异常/边界场景

- 终端尺寸 resize（宽度变 ⇒ 折行全变 ⇒ 视觉行号空间整个换掉）：选区必须失效或按字符偏移重算。
- CJK / emoji（`宽=2`）：列偏移落在宽字符中间。
- 选区跨「不可见条目」（`Thinking` 被 `/thinking` 收起、工具结果折叠）—— 隐藏内容不应被拷出。
- 浮层打开 / 关闭。
- OSC 52 被终端禁用（已有 `run.rs` 文档承认的静默失败）⇒ 是否要同时写 `pbcopy` 之类兜底？
- 鼠标捕获关闭后（退出时）残留选区。

## 未澄清问题

- [ ] **待用户确认**：松手即复制（copy-on-select）是**强制**还是**可配**？VS Code / pi / Codex 默认
      都开，但「选中就会覆盖剪贴板」对某些用户是负体验。
- [ ] **待用户确认**：双击选词 / 三击选行 是否进第一版？（三家都有；成本是要做词边界判定，
      且 CJK 没有空格分词，边界规则要单独定。）
- [ ] **待用户确认**：拖出 Chat 区（进入 Input / Status）是**钳制**还是**不选**？
- [ ] OSC 52 失败时的兜底策略：要不要在 macOS 走 `pbcopy`（pi 的做法）？这会引入平台分支
      与子进程，ADR-0050 曾以「三平台三份实现」为理由选了 OSC 52。**倾向**：不加，维持 OSC 52
      单向写 + `/copy` 的不探测策略。
- [ ] **文档同步**：`/help` 两处 `/copy` 文案已过时（
      `crates/ys-tui-coding/src/commands.rs` 的 `KEY_HELP` 与 `apps/coding-agent/src/help.rs:52`
      均写「复制最后一条回复（v1：写进对话区，手动选中）」）。实际实现早已走 OSC 52，
      且未说明复制的是 **Markdown 原文**。→ 改为「复制最后一条回复的 Markdown 原文到系统剪贴板」。
- [ ] ADR-0030 / ADR-0050 / `docs/arch/tui-markdown-render/design.md` §2.8 /
      `docs/design-final/coding-agent-tui.md` §18.4・§18.6 的回写（见「演进约束」）。

## 后续建议

- 建议用 `arch-design` 做方案设计，重点比较**高亮层级**的三个候选（上）与**命中测试的数据源**
  （`TranscriptLayout` vs 另建一份「字符级」布局）。
- 建议用 `prototype` 先验证最不确定的一环：**CJK 宽字符的列 → 字符偏移映射**是否能用
  `Paragraph::line_count` 的口径做准（这是全案唯一没有既有代码可抄的地方）。
- 变异测试清单先攒起来（三条高危：`skip_rows` 半行、宽字符中点、反向拖选）。
