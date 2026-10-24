# 0051 — TUI 接管拖拽：应用内选区 + 松手即复制

让「复制对话内容」不再依赖终端提供的修饰键逃生，改由应用自己做选区与提取。

## Status

accepted（2026-10-24）

## 背景

ADR-0050 为滚轮接管了鼠标（`run.rs` 常开 `EnableMouseCapture`），代价是**终端原生拖选失效**，
当时给的退路是「`⇧`+拖选或 `/copy`」。实际使用中这条退路是坏的：

| 终端（macOS） | 静默压过 mouse mode 的键 |
|---|---|
| Alacritty | **Shift**（`input/mod.rs:500` 无条件优先） |
| VS Code 终端（xterm.js 6.1.0-beta） | **Option**，且需 `terminal.integrated.macOptionClickForcesSelection`（默认 `false`） |
| VS Code 终端（Win/Linux） | Shift |

xterm.js 的判断（`node_modules.asar` 里的 `@xterm/xterm/lib/xterm.js`）：

```js
shouldForceSelection(e) {
  return this._optionsService.rawOptions.mouseEventsRequireAlt && … ? !e.altKey
    : f.isMac ? e.altKey && this._optionsService.rawOptions.macOptionClickForcesSelection
              : e.shiftKey
}
```

VS Code 从不设 `mouseEventsRequireAlt`（整个 `workbench.desktop.main.js` 出现 0 次）、
该设置默认 `false` ⇒ macOS 分支下 `shouldForceSelection` **恒为 false**，**没有任何修饰键能启动选择**。

**同类产品的做法**（从本机安装物抽取）：Claude Code / Codex / pi **全部**接管鼠标并自建选区
（`copySelection` / `text_selection.rs` / `selectionPressActive`），因此从不暴露这个终端差异。

## 决策

### 1. 接管拖拽，但只做「拖选 → 复制」

- `handle_mouse` 从「只认滚轮」扩为三态：`Down(Left)` / `Drag(Left)` / `Up(Left)`。
- `Moved`（悬停）仍忽略；点击仍不绑任何动作 —— ADR-0030 的「点击 / 悬停不做」**仍然有效**，
  本次只反转**拖拽**一项。
- `⇧` 按住时**直接放行**，把原生选择让回终端（保留 ADR-0050 原有的退路）。

### 2. 坐标用全局视觉行，不用屏幕行

选区存 `(全局视觉行, x)`，按下时由 `top_row + 局部 y` 求出。
**这是本决策最容易做错的一点**：若改用屏幕相对坐标，拖到边缘自动滚动时锚点会钉在格子上，
于是「一直滚」也只能选满一屏 —— 而自动滚动存在的**唯一理由**就是 `Buffer` 只有视口大小、
不滚就选不了一屏以外的内容。全局行号让两者自洽。

### 3. 字形从 Buffer 读回，不重建字符级布局

正常渲染后，把同一份 `window.lines` + 同一宽度**再渲染进自己的 `Buffer`**，逐格 `symbol()` 读出。

**关键细节（易错）**：宽字符的续格是**普通空格**，不是空串。必须在读回时复现 diff-writer 的
宽度规则（`ratatui-core-0.1.2/src/buffer/buffer.rs:589-598`）：

```rust
skip = max(skip, cell_width) - 1;   // 少了 max 就会丢续格，CJK 每个字后多一个空格
```

`Cell::skip` 在 0.30 已废弃且恒 `false`，**不可用**。按上述规则读回，CJK / 全角标点 / emoji
均已实测无损。

### 4. 字形缓存：一次分配、滚动靠槽环旋转

`GlyphGrid` 常驻 `App`，`slots: Buffer` 永不重分配；`order: Vec<u16>` 是「屏幕行 → slot」的
环形置换，滚动 `k` 行 = `order.rotate_left(k)` + 只重建**换进来的 k 行**。
单步成本 **8–12 µs**，与视窗高度、会话长度、已滚行数**都无关**。

字形只在三种时刻产生：拖选开始、布局/内容真变、滚动进入新行。**帧路径只写样式**
（选中格 × ~1.5 ns），不读字形。

### 5. 松手即复制，且零长度不复制

`anchor == cursor`（按下未拖动，或拖回原点）⇒ **不复制、清选区**。
理由：选中就覆盖剪贴板已是负体验，误触覆盖更坏。

复制走**现有**通路 —— `app.pending_clipboard` → `run.rs::write_clipboard`（OSC 52），
与 `/copy` 共用同一条管线，零新增接线。

### 6. 选区不跨用户滚动 / 不跨 resize

用户主动滚动（滚轮、`PageUp`、`↑`）⇒ 清选区。resize（尤其宽度变）⇒ 清选区，因为
`layout.sync` 会清空全部折行记忆、行号空间整个换掉（补偿被 `!width_changed` 显式排除）。

**注意区分**：拖动期间由**自动滚动**引起的位移不走这条 —— 那条走 `shift()`，选区照常延伸。
若两者混淆，拖到边缘会自我取消。

### 7. 自动滚动：16ms 节拍 + 步长加速

- 拖动时 `poll` 周期切到 `DRAG_TICK = 16ms`（否则 `POLL_INTERVAL = 50ms` 只能 20 行/s，
  200 行要 10 秒，用户会以为卡死）。
- `step = clamp(1 + held_ms / 40, 1, 8)` ⇒ 上限 ≈500 行/s；200 行约 0.6 s、总成本 ≈0.25 ms。
- **鼠标批次合并必须做**：`poll()` 之后 `while poll(Duration::ZERO)` 排空，只应用最后一个 `Drag`。
  触控板一次爆发 8–30 个事件，否则是 8–30 次整屏 `draw`（每次 ~150 µs）。

## 代价（明确接受）

- **与 ADR-0045 的张力**：ADR-0045 的全部价值在「渲染成本与会话长度解耦」。本决策引入了
  「再渲染一遍内区」，靠**缓存键把重建压到每次拖选 1 次**（无流式时）来维持该目标；
  流式拖选时最坏 10 次/秒全量（受 `RENDER_THROTTLE = 100ms` 天然限流），每次 25–35 µs。
- **选区不跨用户滚动存活**：滚一下高亮即清。跨滚动需把坐标换成字符偏移（M2），
  那要动 `layout` / `draw` 的渲染路径，本轮不做。
- **选区不跨 resize 存活**：宽度变则坐标系换掉，无映射可救。松手后 resize 无害。
- **拷出的是纯文本，不是 Markdown 原文**：`styles.rs` 刻意把 `#` / ``` ``` `` / alert emoji
  置空（`docs/arch/tui-markdown-render/design.md` §2.8），屏幕字形本就没有这些标记。
  需要 Markdown 原文走 `/copy`（它取 `TranscriptLine::Assistant(String)` 的原始字符串）。
  **两者互补，`/copy` 不因此变成死命令。**
- **复制上限 256 KB**：OSC 52 载荷多数终端约 100 KB 封顶。超限不复制、留一行提示，
  **不静默截断**（半截代码比复制失败更坏）。
- **不做双击选词 / 三击选行**：词边界在 CJK 下无空格可依，规则要单独定。
  枚举值先不建，避免「留了字段永远不用」。
- **真实终端手感仍是 `MANUAL_ACK`**：拖拽跟手程度、自动滚动速率需人工确认。

## Considered Options

### A. 继续靠终端原生选择（维持 ADR-0050）

- 否决理由：这条退路在 VS Code + macOS 上**物理上不存在**（见背景）。跨终端不一致不是文档
  能解决的 —— 用户不会为了复制一段话去改终端设置。

### B. 只发 `?1000h?1006h`（"scroll-only" 模式），把拖拽还给终端

- 否决理由：对 VS Code **无效**。xterm.js 只要 `areMouseEventsActive` 为真就
  `_selectionService.disable()`，与 tracking 级别无关（1000/1002/1003 无差别）。
  对 iTerm2 / Alacritty 这类按级别区分解释的终端有效，但不解决报告问题的场景。

### C. 用 `trait Space` 做坐标抽象，为未来 M2 预留

- 优点：M1 → M2 零调用方改动。
- 否决理由：代价是 6 文件、≈780 行、**全仓唯一 `dyn`**。而决策 2 的全局视觉行已经让 `anchor`
  是逻辑坐标，M2 迁移面只剩「行号 → 字符偏移」一处映射，调用方本来就不用改。
  为一个未排期的迁移预置整层抽象，与「最小核心」相悖。

### D. 在 `main` buffer 里做选区（放弃 alt-screen）

- 否决理由：本项目 TUI 的形态（固定三 pane + 自绘状态行）依赖 alt-screen；退回 inline 会丢掉
  布局。代码量也远大于本方案。

## 验证

| 项 | 位置 |
|---|---|
| 缓存失效九条逐条 | `selection.rs` 单测 |
| 续格规则（CJK / 全角 / emoji / 混合） | `selection.rs` `read_back` 单测 |
| `shift(k)` 与全量重建逐格等价 | `selection.rs` 单测（槽环正确性守卫） |
| 三态迁移（无拖动不复制 / `⇧` 无副作用） | `events.rs` 单测 |
| 自动滚动步长边界 | `selection.rs` 单测 |
| 既有「视窗渲染 == 整段渲染」与 N4 用例 | `draw.rs` / `layout.rs`（**必须原样全绿**） |
| 6 处注入变异 | `scripts/selection-mutation.py`（照 `wheel-mutation.py` 形态） |
| 性能闸门 | `perf_drag_steady` / `perf_drag_scroll200` / `perf_extract_200rows` |

## 与其它文档的关系

- **二次反转 [ADR-0030](0030-tui-markdown-render.md) 的「鼠标维持砍掉」**：滚轮（ADR-0050）+
  **拖拽（本条）**；点击 / 悬停 / 可配快捷键仍砍。
- **改口 [ADR-0050](0050-tui-mouse-wheel-scroll.md) 的「代价」段**：那里写用户须 `⇧`+拖选或
  `/copy`；落地后裸拖选即可，`⇧` 反而成为「让回终端原生选择」的开关。
- **`docs/arch/tui-markdown-render/design.md` §2.8「拷贝友好」**：四条字形决定仍有效 ——
  应用内选区不引字形过滤，拷出仍是屏幕所见。
- **`docs/design-final/coding-agent-tui.md`** §18.4（键位）/ §18.6（已知缺口）需回写。
- 设计与候选对比见 `docs/arch/tui-text-selection/{context,design}.md`。
