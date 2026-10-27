# TUI 文本选区与松手复制 设计计划

## 背景

ADR-0050 为滚轮接管了鼠标（`run.rs::setup_terminal` 常开 `EnableMouseCapture`），代价是终端原生
拖选失效，当时给的退路是「`⇧`+拖选或 `/copy`」。ADR-0051 已核实这条退路在
**VS Code 终端 + macOS 上物理不存在**（xterm.js 判 `Option` 且该设置默认 `false`；
`shouldForceSelection` 恒为 `false`）⇒ 用户没有任何修饰键能启动原生选择。

现状（已读码核实）：

| 事实 | 位置 |
|---|---|
| `handle_mouse(kind: MouseEventKind, ..)` **只认** `ScrollUp` / `ScrollDown`，其余 `_ => {}` | `crates/ys-tui-coding/src/events.rs:147-154` |
| `handle_event` 只把 `m.kind` 传下去（丢掉 `column` / `row` / `modifiers`） | `events.rs:135` |
| `scroll(app, width, height, delta)` **私有**，`delta > 0` = 向上 | `events.rs:378-388` |
| `chat_viewport(app, w, h) -> (u16, u16)` 只给**宽高**，不给原点；`chat_area` / `chat_block` 是私有 `fn` | `draw.rs:147` / `:163` / `:173` |
| `window(top_row, height) -> Window { lines, skip_rows }` **不告诉**「屏幕第 y 行来自哪个 `Line` 的第几个视觉行」 | `layout.rs:209` / `Window` 定义在 `:52` |
| `POLL_INTERVAL = 50ms` 是**常量**，整个 `event_loop` 共用 | `run.rs:59` / `:195` |
| `app.pending_clipboard: Option<String>` → `run.rs:211` 当场 `write_clipboard` | `app.rs:63` / `run.rs:620` |
| `RENDER_THROTTLE = 100ms` 只作用于 md 渲染缓存（`src.len()` 增长期） | `cache.rs:29` / `:145` |
| `Frame` 在 ratatui 0.30 **不暴露已渲染 buffer** | `docs/arch/tui-text-selection/context.md` §「关键的既有事实」 |

本设计的权威来源是 `docs/arch/tui-text-selection/design.md` 与
`docs/adr/0051-tui-text-selection.md`；审核修复记录见 `docs/arch/tui-text-selection/review.md`。

## 目标与非目标

### 目标（对应 design.md 的决策编号）

| # | 目标 | 依据 |
|---|---|---|
| G1 | 鼠标左键拖选 Chat 区 → 应用内高亮；**松手即复制**（copy-on-select） | ADR-0051 决策 1 / 5 |
| G2 | 选区坐标用**全局视觉行**（`anchor_row = top_row + 局部 y`），因此能选**超过一屏** | ADR-0051 决策 2；design §1 分歧 1/2 的裁决 |
| G3 | 字形从 `Buffer` **读回**，复现 diff-writer 的宽度规则（`skip = max(skip, cell_width) - 1`） | ADR-0051 决策 3 |
| G4 | 字形缓存一次分配、滚动靠**槽环旋转**；单步成本与 `inner_h`、会话长度、已滚行数**均无关** | ADR-0051 决策 4；ADR-0045 的 N4 取向 |
| G5 | 拖到边缘自动滚动：`DRAG_TICK = 16ms` + `step = clamp(1 + held_ms/40, 1, 8)` | ADR-0051 决策 7 |
| G6 | 复制走**既有** `pending_clipboard` → `write_clipboard`（OSC 52）管线，零新增接线 | design §2.4 步骤 8 |

### 非目标（明确不做，且与 design.md §5 的表一致）

| # | 不做 | 理由 |
|---|---|---|
| N1 | 双击选词 / 三击选行 | `context.md` 未澄清问题；词边界在 CJK 下无空格可依。`SelectionMode` 枚举**不建**（ADR-0051「代价」段末条） |
| N2 | 点击（`Down` 无 `Drag`）触发任何动作 | ADR-0030「不绑点击」仍有效；本次只反转**拖拽**一项 |
| N3 | 悬停（`Moved`）/ 右侧滚动条 / 搜索 / OSC 52 读 | design §5、ADR-0051 决策 1 |
| N4 | 选区跨**用户滚动**存活 | ADR-0051 决策 6：用户主动滚动即清（`drag_tick` 的自动滚动走另一条路，见 §5.3） |
| N5 | 选区跨 **resize** 存活 | 宽度变 ⇒ `layout.sync` 清空全部折行记忆（`layout.rs:120`），行号空间整个换掉 |
| N6 | 拷出 Markdown 原文 / 字形过滤 | 拖选复制**屏幕字形**（`styles.rs` 已把 `#` / 围栏置空）；要原文走 `/copy` |
| N7 | `pbcopy` 兜底、OSC 52 探测 | 沿用 ADR-0050 的「单向写、不探测」口径 |

## 方案与模块划分

**以 design.md §1 的候选 C 为骨架，吸收 A 的「`⇧` 让回终端」与 B 的「`anchor == cursor` 不复制」。**

新增 **1 个文件**，改动 **5 个既有文件**。既有函数签名**一律不动**
（`ui` / `draw_transcript` / `window` / `sync` / `sync_scroll_state` / `compute_max_scroll`）。

| 文件 | 职责（本需求新增/变更的部分） | 依赖 |
|---|---|---|
| `crates/ys-tui-coding/src/selection.rs` | **新增**。`Phase` / `Selection`（状态机 + 坐标）/ `GlyphGrid`（槽环字形缓存）/ `Extractor`（读回 + 裁剪 + 上限）/ `Highlight`（样式覆盖层）/ `drag_tick`（自动滚动） | `layout.rs`（`window_with_rows` / `revision_range`）、`events::scroll`、`draw::chat_viewport` |
| `crates/ys-tui-coding/src/layout.rs` | **纯追加**：`RowRef` / `EntryMemo.rev` / `TranscriptLayout.structural_rev`+`rev_alloc` / `structural_rev()` / `revision_range()` / `window_with_rows()` / `total_rows()`；`window()` 改为薄包装（签名不变）；`SyncResult.recomputed_entries` | 无（只加，不改既有语义） |
| `crates/ys-tui-coding/src/draw.rs` | 抽 `chat_inner()`（内区几何**唯一来源**）；新增 `chat_inner_rect()`；抽 `window_lines()`；新增 `draw_selection(&mut App)`；`ui()` 里独立一行调用 | `selection.rs` |
| `crates/ys-tui-coding/src/events.rs` | `handle_mouse` 收**整个** `MouseEvent`；三态分派；`scroll()` 改 `pub(crate)`（**本体保持纯净，不清选区**）；新增 `scroll_clearing_selection()` 供**用户主动**滚动入口用；`Resize` 清选区 | `selection.rs`、`draw::chat_inner_rect` |
| `crates/ys-tui-coding/src/run.rs` | `DRAG_TICK` 可变 poll 周期；鼠标批次合并；`drag_tick()` 调用点 | `selection::drag_tick` |
| `crates/ys-tui-coding/src/app.rs` | `pub selection: selection::Selection`；`jump_to_start` / `jump_to_end` / `reset_for_replay`（`app.rs:231`）里 `clear()` | `selection.rs` |

**模块依赖方向不变**：`ys-tui-coding` 仍**不依赖** `ys-runtime`（编译器强制）；新增代码全在 UI 线程，
**无 async、无 tokio**（`selection.rs` 只用 `std::time::Instant`）。

### 数据流

```
拖动中（每个鼠标事件）
  run.rs::event_loop
    poll(poll)  where poll = DRAG_TICK(16ms) if 拖动且贴边 else POLL_INTERVAL(50ms)
    read() → 批次合并（连续 Mouse 只留最后一个）→ handle_event
      events::handle_mouse(MouseEvent)
        ⇧ 按住 ⇒ return（让回终端原生选择）
        chat_inner_rect() → 绝对内区 Rect；减去得内区相对 (rel_x, rel_y)
          Down(Left)  ⇒ Selection::begin(top_row + rel_y, rel_x); follow = false
          Drag(Left)  ⇒ Selection::extend(..)；窗外则 clamp + 置 edge
          Up(Left)    ⇒ 判 anchor == cursor → extract → pending_clipboard → clear
    drag_tick(app, ..)（在 compute_max_scroll 之前）
  帧：
    draw::ui → sync_scroll_state → draw_transcript → draw_selection(inner, top_row, &mut app)
      Selection::ensure_grid(&layout, inner, top_row)   // 缓存键命中 ⇒ 只写样式
      Highlight / rects → 逐格 set_style(REVERSED)
```

## 关键数据结构

以下为**采纳形态**（含对 design.md 的落地明确化，见下文「对 design.md 的实现期明确化」）。

```rust
// ── layout.rs（纯追加） ──────────────────────────────────────────────

/// 屏幕第 y 行（0 基，相对内区顶）的来源。
///
/// `line` 是**窗口内第几个 `Line`**（与 `Window::lines` 同序、同下标），
/// `off` 是该 `Line` 内部的第几个视觉行。两者一起足以把那一行单独重渲染出来
/// （「逐行渲染 == 整窗渲染」已实测成立，见 review.md §1.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowRef {
    pub line: u32,
    pub off: u16,
}

struct EntryMemo {
    // …既有字段：sig / render_sig / lines / row_starts…
    /// 条目级版本号：`render_sig` 变（或首次渲染）时从 `TranscriptLayout.rev_alloc` 分配。
    /// **产物没变就不换号** —— 这样「流式节流窗口内源变、产物没变」不会打红任何缓存。
    rev: u64,
}

pub(crate) struct TranscriptLayout {
    // …既有字段：width / thinking_visible / entries / gaps / starts / total_rows…
    /// **结构版本**：`reset()` / 宽度变 / `thinking_visible` 翻转时 +1。
    /// **不随条目内容变** —— 否则窗口外条目一更新就会全量重建字形缓存。
    structural_rev: u64,
    /// 条目版本号**分配器**（单调递增）。
    rev_alloc: u64,
}

pub(crate) struct SyncResult {
    pub total_rows: usize,
    pub width_changed: bool,
    /// 本次同步**真正重算了折行**的条目数（= `render_sig` 变了的条目数）。
    ///
    /// 存在唯一目的：把「流式拖选最多 10 次/秒」从**推论**变成**机制**
    /// （review.md R3）—— 拖选期断言该计数不因选区而增加。
    pub recomputed_entries: usize,
}

impl TranscriptLayout {
    /// 屏幕第 y 行来自哪个 `Line` 的第几个视觉行。`len == min(height, 剩余可见行数)`。
    ///
    /// **与 `window()` 共用同一次遍历**（`window_impl`），不得另写一份定位逻辑 ——
    /// 否则「画的位置」与「认的位置」会漂移（ADR-0045 已因这类错位返工过一次）。
    pub(crate) fn window_with_rows(&self, top_row: usize, height: usize)
        -> (Window, Vec<RowRef>);

    /// 窗口覆盖条目的版本号**折叠**成一个 u64（无分配）—— `GlyphKey.win_rev` 的来源。
    pub(crate) fn revision_range(&self, top_row: usize, height: usize) -> u64;

    pub(crate) fn structural_rev(&self) -> u64;
    pub(crate) fn total_rows(&self) -> usize;

    // 既有，签名不动：
    pub fn window(&self, top_row: usize, height: usize) -> Window;   // = window_impl(top, h, None)
    fn window_impl(&self, top_row: usize, height: usize, rows: Option<&mut Vec<RowRef>>) -> Window;
}
```

```rust
// ── selection.rs（新增文件） ────────────────────────────────────────

/// 一次拖选的生命周期。**两态**，不是 design.md §2.2 的三态 —— 理由见 §7 第 1 条。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    #[default]
    Idle,     // 无选区（初始 / 已清 / resize / 用户滚动）
    Dragging, // 左键已按下（可能尚未移动 ⇒ `anchor == cursor`，松手不复制）
}

/// 贴边方向。`+1` = 上沿（要看**更早**的行，`scroll` 的 `delta > 0`）；
/// `-1` = 下沿（要看**更新**的行）；`0` = 未贴边。
type Edge = i8;

#[derive(Debug, Default)]
pub(crate) struct Selection {
    /// 按下点：**全局视觉行**（`top_row + 内区相对 y`），与屏幕滚到哪里无关。
    anchor_row: usize,
    anchor_x: u16,
    /// 当前点：同上口径。**自动滚动只改 `cursor_*`，`anchor_*` 永不因滚动而变**（见「对 design.md 的实现期明确化」第 4 条）。
    cursor_row: usize,
    cursor_x: u16,
    phase: Phase,
    edge: Edge,
    /// 贴边开始时刻 —— `step = clamp(1 + held_ms/40, 1, 8)` 的输入。
    edge_since: Option<std::time::Instant>,
    /// 视窗字形缓存（design.md §2.3 的九条失效规则都作用在它身上）。
    grid: GlyphGrid,
    /// 高亮矩形复用缓冲：容量复用 ⇒ 稳态帧零分配。
    rects: Vec<Rect>,
}

impl Selection {
    pub(crate) fn is_dragging(&self) -> bool;         // phase == Dragging
    pub(crate) fn edge(&self) -> Edge;
    pub(crate) fn is_active(&self) -> bool;           // phase != Idle
    pub(crate) fn bounds(&self) -> ((usize, u16), (usize, u16)); // (anchor, cursor)，Copy 出栈
    pub(crate) fn begin(&mut self, row: usize, x: u16, now: Instant);
    pub(crate) fn extend(&mut self, row: usize, x: u16, edge: Edge, now: Instant);
    pub(crate) fn clear(&mut self);
    pub(crate) fn ensure_grid(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize);
    pub(crate) fn rects(&mut self, top_row: usize, inner: Rect) -> &[Rect];
}
```

```rust
// ── selection.rs: 字形缓存与槽环 ────────────────────────────────────

/// 字形缓存的键。任一项不等都要处理（`base_row` 走 `shift`，其余走 `rebuild`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GlyphKey {
    /// `TranscriptLayout::structural_rev()` —— 结构变化（reset / 宽度 / thinking）。
    layout_rev: u64,
    /// `TranscriptLayout::revision_range(top_row, inner_h)` —— **窗口内**条目版本折叠。
    win_rev: u64,
    /// 内区矩形（含 `x` / `y`）。
    inner: Rect,
    /// 屏幕第 0 行 = 全局视觉行 `base_row`。
    base_row: usize,
    /// `Window::skip_rows`。
    skip: u16,
}

/// 视窗字形缓存：一次分配、尺寸不变时永不重分配；滚动 = 槽环旋转（不搬 cell）。
pub(crate) struct GlyphGrid {
    inner: Rect,
    width: u16,
    height: u16,
    base_row: usize,
    /// `width × height`；slot `s` 占第 `s` 行。
    slots: Buffer,
    /// 屏幕行 → slot（环形置换）。`shift(k)` 只做 `order.rotate_left(k)`。
    order: Vec<u16>,
    /// 每 slot 的宽字符续格位图（每行 `words_per_row` 个 u64，低位 = 小列号）。
    /// 用途只有一个：高亮边界落在宽字符中间时把边界修到首格（变异注入 #4）。
    cont: Vec<u64>,
    words_per_row: usize,
    key: GlyphKey,
}

impl GlyphGrid {
    /// 缓存三档判定：全同 ⇒ 命中；仅 `base_row` 变 ⇒ `shift(k)`；其余 ⇒ `rebuild()`。
    /// `Phase::Idle` 且无选区 ⇒ `release()`（`slots.resize(Rect::ZERO)` 保留容量）。
    fn ensure(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize);
    fn rebuild(&mut self, layout: &TranscriptLayout, window: &Window, rows: &[RowRef]);
    fn shift(&mut self, layout: &TranscriptLayout, k: isize, window: &Window, rows: &[RowRef]);
    fn release(&mut self);

    /// 屏幕第 y 行第 x 列的字形（续格也返回 `" "` —— 与 `Buffer` 的实际内容一致）。
    fn symbol(&self, y: u16, x: u16) -> &str;
    /// 屏幕第 y 行第 x 列是否**宽字符的续格**。
    fn is_cont(&self, y: u16, x: u16) -> bool;
    /// 该格所属字符的首格列号（非续格 ⇒ 自身）。
    fn head_col(&self, y: u16, x: u16) -> u16;
}
```

```rust
// ── selection.rs: 提取 ─────────────────────────────────────────────

/// OSC 52 载荷多数终端约 100 KB 封顶。超限**不复制**、不静默截断（ADR-0051「代价」）。
const COPY_MAX_BYTES: usize = 256 * 1024;
/// 提取分块行数：每块用 `layout.window(row, chunk)` 取行，渲进复用 Buffer 后读回。
const EXTRACT_CHUNK_ROWS: usize = 64;

pub(crate) enum ExtractOutcome {
    /// 要写进 `app.pending_clipboard` 的文本。
    Copied(String),
    /// `anchor == cursor` ⇒ 不复制（B 的防误触）。
    Empty,
    /// 超过 `COPY_MAX_BYTES` —— 调用方在 transcript 回一行提示。
    TooLarge(usize),
}

/// **只吃 `&TranscriptLayout` + 两个坐标**，不碰 `App`
/// —— 既好测，也避开「`&mut app.selection` 与 `&app` 同时借用」。
pub(crate) fn extract(
    layout: &TranscriptLayout,
    anchor: (usize, u16),
    cursor: (usize, u16),
) -> ExtractOutcome;

/// 读回单行的 `[xa..=xb]`：续格按 `skip = max(skip, cell_width) - 1` 跳过，
/// 行尾 `trim_end_matches(' ')`。`xa > xb` ⇒ 空串。
fn read_back(grid: &GlyphGrid, y: u16, xa: u16, xb: u16) -> String;
```

```rust
// ── selection.rs: 高亮与自动滚动 ───────────────────────────────────

/// 只改样式、不动 symbol：宽字符不会被拆（续格一并 `set_style`）。
/// **不用 `Clear`** —— 它会清 symbol，正是要避免的。
pub(crate) struct Highlight<'a> {
    rects: &'a [Rect],
}

impl Widget for Highlight<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for r in self.rects {
            let r = r.intersection(area);
            for y in r.top()..r.bottom() {
                for x in r.left()..r.right() {
                    buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// 自动滚动一拍。**先 `follow = false` 再滚**（review.md R6）；
/// 滚完把 `follow` 压回 `false`（`events::scroll` 在 `next == 0` 时会置真）。
pub(crate) fn drag_tick(app: &mut App, width: u16, height: u16, now: Instant);

/// `step = clamp(1 + held_ms / 40, 1, MAX_STEP)`，`MAX_STEP = 8`（≈500 行/s）。
fn drag_step(held: Duration) -> usize;
```

```rust
// ── draw.rs ───────────────────────────────────────────────────────

/// 内区几何的**唯一**来源；两个 `pub(crate)` 包装都与它共用，不得另写第二条链路。
/// `None` = 尺寸低于 `MIN_WIDTH` / `MIN_HEIGHT`（`draw.rs:81-82`）。
fn chat_inner(app: &App, area: Rect) -> Option<Rect>;

pub(crate) fn chat_viewport(app: &App, width: u16, height: u16) -> (u16, u16);       // 既有签名不动
pub(crate) fn chat_inner_rect(app: &App, width: u16, height: u16) -> Option<Rect>;  // 新增，给 events.rs

/// 视窗覆盖的行（含「空 transcript ⇒ 占位一行」）—— `draw_transcript` 与字形缓存**同源**。
fn window_lines(app: &App, top_row: usize, inner_h: usize) -> Vec<Line<'static>>;

/// `&mut App`：写 `GlyphGrid` 需要可变访问，用 `RefCell` 就凭空多一层内部可变性（review.md R5）。
fn draw_selection(frame: &mut Frame, area: Rect, top_row: usize, app: &mut App);
```

## 几何与坐标模型（承重）

### 5.1 全局视觉行

`anchor_row` / `cursor_row` 都是**全局视觉行号**（`TranscriptLayout` 的子空间：
0 = 第一条可见条目的第一行）。屏幕第 0 行 = 全局行 `top_row`。于是：

- `Down` 时 `anchor_row = top_row + 内区相对 y`；
- 滚动（`top_row` 变）**不需要**改 `anchor_row`；
- 「选满一屏」的限制自然消失（ADR-0051 决策 2）。

`anchor_x` / `cursor_x` 是**内区相对列**（0 = 内区左边界）。`x` 在 M2 会变成「行内字符偏移」——
同一字段、不同含义（design.md §4 的边界声明）。

### 5.2 `RowRef` 的语义（槽环的数据基础）

窗口内第 `k` 个 `Line` 折成 `rows` 个视觉行。给定 `top_row`，屏幕第 `y` 行的来源是：

- 若 `y == 0` 且首行被上沿切开 ⇒ `RowRef { line: 0, off: skip_rows }`；
- 否则沿 `window_impl` 的遍历推进：`line` = 窗口内 `Line` 下标、`off` = 该 `Line` 内的视觉行偏移。

于是 **`rows[0].off == window.skip_rows`**（可断言的不变量），且单行重渲染是
`Paragraph::new(Text::from(vec![lines[r.line].clone()])).wrap(Wrap { trim: false }).scroll((r.off, 0))`
渲进 1 行高的小区域 —— 与 `draw_transcript` 的整窗渲染等价（review.md §1.1/§1.2 已实测）。

### 5.3 两种滚动的分工（易混淆，必须分清）

| 触发 | 路径 | 对选区的影响 |
|---|---|---|
| 滚轮 / `PageUp` / `PageDown` / `↑↓` 回落 | `events::scroll_clearing_selection()`（= `clear()` + `scroll()`） | **`clear()`** —— 决策 6：用户主动滚动即清 |
| 拖到边缘 | `selection::drag_tick()` | **不清**，只把 `cursor_row` 推到新的 `top_row` / 底行 —— 走 `shift()`，选区照常延伸 |

若两者混淆（自动滚动也清选区），拖到边缘会**自我取消**。故：

- `drag_tick` **不得**调 `Selection::clear()`，只调 `events::scroll()`（**本体纯净、不清选区**）；
- 清必须发生在**用户主动**滚动的入口 —— 新增 `scroll_clearing_selection()`（`clear()` + `scroll()`），
  `handle_mouse` 的滚轮两臂与 `handle_key` 的 `PageUp` / `PageDown` / 前后跳转改调它。

若把 `clear()` 塞进 `scroll()` 内部，`drag_tick` 每拍都会清掉选区 —— 「拖到边缘继续扩选」全废。

### 5.4 宽字符与续格

`Buffer` 里宽字符的续格是**普通空格**（`Cell::symbol() == " "`），`Cell::skip` 在 ratatui 0.30
已废弃且恒 `false`。正确读法（`ratatui-core-0.1.2/src/buffer/buffer.rs:589-598`）：

```rust
skip = max(skip, cell_width) - 1;   // 少了 max 就会丢续格 ⇒ CJK 每字后多一个空格
```

`cont` 位图给**高亮**用：选区右端落在宽字符续格上 ⇒ `xb = head_col(y, xb)`；左端同理。
两处都要做，否则 CJK 半格露出（变异注入 #4）。

### 5.5 `follow` 的夹取顺序

`events::scroll()` 内部先 `compute_max_scroll` 再夹取，**并在 `next == 0` 时置 `follow = true`**；
而 `sync_scroll_state` 在 `follow == true` 时**强制** `scroll_offset = 0`（`app.rs:539-541`）。
故 `drag_tick` 的顺序必须是：

1. `app.follow = false`（**先于**任何 `compute_max_scroll`）；
2. `max == 0` ⇒ 直接返回（内容不足一屏：无可滚，也**不能**让 `scroll()` 把 `follow` 置回）；
3. `events::scroll(app, w, h, edge as isize * step as isize)` —— 夹取逻辑的**唯一来源**（`delta > 0` = 向上，与 `edge > 0` 同向，不得反写）；
4. `app.follow = false`（压回 —— 步骤 3 在 `next == 0` 时会置真）；
5. `debug_assert!(!app.follow)`；
6. 把 `cursor_row` 推到新的边缘行（`edge > 0` ⇒ 新 `top_row`；`edge < 0` ⇒ `top_row + inner_h - 1`），`anchor_row` **不动**。

## 缓存失效集合（design.md §2.3 的九条，逐条落到代码）

| # | 条件 | 动作 | 落在哪里 |
|---|---|---|---|
| 1 | `inner` Rect 变（resize / input 高度变 / 跨 `MIN_*`） | `rebuild` + 清选区 | `ensure` 比 `key.inner`；`handle_event` 的 `Resize` 分支 |
| 2 | `layout.width` 变 | `rebuild` + 清选区 | `structural_rev` +1（`layout.rs:120`） |
| 3 | `thinking_visible` 翻转 | `rebuild` + 清选区 | `structural_rev` +1（`layout.rs:120`） |
| 4 | `reset()` / 回放替换 transcript | `rebuild` + 清选区 | `structural_rev` +1（`layout.rs:98` 的 `reset`）；`app.rs:231` 的 `reset_for_replay` 里 `selection.clear()` |
| 5 | `win_rev` 变（窗口内某条目 `render_sig` 换了） | `rebuild` | `revision_range()` |
| 6 | `base_row` 变（滚动 / 内容增长补偿） | **不重建**，`shift(k)` | `ensure` 的第二档判定 |
| 7 | 流式追加但产物哈希没变 | **零动作** | `layout.rs:160` 的 `render_sig` 相等已短路 ⇒ `rev` 不换 |
| 8 | 尾部增长、视窗原地不动（`follow == false` 的 Δ 补偿） | **零动作** | `revision_range` 只折叠**窗口内**条目 ⇒ `win_rev` 不变 |
| 9 | `Phase::Idle` 且无选区 | `release()`：`slots.resize(Rect::ZERO)` + 清 `cont` | `ensure` 开头 |

**拖选期间最坏 10 次/秒全量**（受 `RENDER_THROTTLE = 100ms` 天然限流）；无流式输出时
**每次拖选 1 次**（`grid_rebuilds == 1`，性能闸门）。

## 承重不变量清单（每条都能被测试断言）

| # | 不变量 | 断言方式 |
|---|---|---|
| **V1** | `window_with_rows` 与 `window` 由**同一次遍历**产出：`rows[0].off == window.skip_rows`；`rows[y].line` 单调不减；`rows[y].line < window.lines.len()` | `layout.rs` 单测，多组 `(top_row, height)` × CJK/折行样本 |
| **V2** | **逐行渲染 == 整窗渲染**（槽环的前提）：对每 `y`，单行渲染 `rows[y]` 与整窗渲染的第 `y` 行**逐格**相同 | `selection.rs` 单测，用 `Buffer::empty` + `Paragraph::render`，短行/折行/CJK/空行四类 |
| **V3** | **`shift(k)` == 全量重建**（逐格，含 `cont` 位图） | `selection.rs` 单测，`k ∈ {-3..=3}`，每次与新 `GlyphGrid::rebuild` 比 |
| **V4** | 续格读回**无损**：CJK / 全角标点 / emoji / 混合 | `read_back` 单测，逐字断言无多余空格 |
| **V5** | 提取按**每行非空白范围**裁剪：选区跨到短行 / 视窗底部空白填充行 ⇒ 不带尾部空白与空行 | `extract` 单测（review.md R1 的用例） |
| **V6** | `anchor == cursor` ⇒ **不复制**（`ExtractOutcome::Empty`，不动 `pending_clipboard`） | `events.rs` 单测：`Down` 无 `Drag` ⇒ `pending_clipboard.is_none()` |
| **V7** | `⇧` 按住 ⇒ **全程无副作用**（不改 scroll / 不建选区 / 不动剪贴板） | `events.rs` 单测 |
| **V8** | 自动滚动**全程** `follow == false`；`step = clamp(1 + held_ms/40, 1, 8)` 的边界（0ms ⇒ 1、280ms ⇒ 8、>10s ⇒ 8） | `selection.rs` / `draw.rs` 单测（注入 `Instant`） |
| **V9** | `resize` / 用户滚动 / `/new` / `/resume` ⇒ 选区清 | `events.rs` / `app.rs` 单测 |
| **V10** | 复制上限：`> COPY_MAX_BYTES` ⇒ `TooLarge`，**不**写 `pending_clipboard`，transcript 回一行提示 | `extract` 单测 + `events.rs` 单测 |
| **V11** | 高亮**只改 style、不改 symbol**：高亮后每格 `symbol()` 与高亮前逐格相同（宽字符未被拆） | `draw.rs` 单测（`TestBackend` buffer） |
| **V12** | 拖选期 `recomputed_entries` **不因选区**增加 | `draw.rs` / `selection.rs` 单测（`layout::tests::render_calls()` 同型计数器） |
| **V13** | 反向拖选（`cursor_row < anchor_row` / `cursor_x < anchor_x`）与正向**逐字节等价** | `extract` 单测 |
| **V14** | 既有用例**原样全绿**：`test_total_rows_equals_whole_line_count`（N4）、`test_window_anchors_at_both_ends`、`test_unchanged_entries_are_not_re_rendered`、`test_wheel_scrolls_chat_and_never_input_history`、`test_other_mouse_events_are_ignored`、`test_handle_event_routes_mouse_to_scroll` | `./scripts/par-test.sh` |

> ⚠️ V14 里的 `test_other_mouse_events_are_ignored`（`events.rs:1634`）**会与新行为冲突**：
> 它把 `Down(Left)` / `Drag(Left)` / `Up(Left)` / `Moved` 一起断言「不得改变滚动位置、不得恢复跟随」。
> 新实现仍不改滚动位置 ⇒ 滚动断言本身成立；但该用例的**枚举名义**（「一律不动」）已不成立，
> 必须拆成两条：`ScrollLeft` / `ScrollRight` / `Moved` 仍「一律不动」（用例名与断言都不改）；
> 左键三态改由新用例（T18）覆盖三态迁移 + 剪贴板副作用。**不得只改断言不改名字。**

## 与既有 ADR 的关系

| ADR | 关系 |
|---|---|
| **ADR-0045**（TUI 滚动：视窗化 + 视窗原地不动） | 选区**接在同一条视觉行号空间**（`TranscriptLayout`）与**同一份 `window()` 遍历**上（`window_with_rows` 由 `window_impl` 产出，V1 守住）——这正是「不让画的位置与认的位置漂移」的落实。N4（逐条目折行之和 == 整段折行）是槽环的**前置**：V2 依赖它。本需求引入了「把内区再渲染一遍」，与 0045 的「渲染成本与会话长度解耦」有张力，靠 §4 第 6/7/8 条把重建压到**每次拖选 1 次**（无流式时）来维持该目标 |
| **ADR-0050**（滚轮 = 滚聊天） | 沿用其 `EnableMouseCapture`、`⇧` 让回、`POLL_INTERVAL` 常量。**三处扩展**：`handle_mouse` 的参数从 `MouseEventKind` 变 `MouseEvent`；poll 周期可变；`POLL_INTERVAL` 只作非拖动态的默认值。**改口其「代价」段**：那里写「捕获期间裸拖选失效，须 `⇧`+拖选或 `/copy`」，落地后裸拖选即可，`⇧` 反而成为**让回终端原生选择**的开关 |
| **ADR-0030**（TUI markdown 渲染，砍掉「鼠标」） | **第二次反转**「鼠标维持砍掉」：滚轮（ADR-0050）+ 拖拽（本条）。**点击 / 悬停 / 可配快捷键 / 语法高亮仍砍**。其 §2.8「拷贝友好」四条字形决定**仍有效** —— 应用内选区不引字形过滤，拷出的仍是屏幕所见 |
| **ADR-0051**（本需求的决策记录） | 决策 1–7 全部落进本计划（G1–G6 + §5.3 的两种滚动分工）。「代价」段的 6 条进本文件 §8。设计 §7 要求的文档回写见 exec-plan 的 T24 / T25 |

## 对 design.md 的实现期明确化（4 处偏离，均带理由）

设计文档是权威来源；以下四处是核实代码后必须明确化的点，**不改变其任何决策**。

### 1. `Phase` 用**两态**，不建 `Done`

design.md §2.2 写 `enum Phase { Idle, Dragging, Done }`，但 §2.4 第 9 步明确「清选区（高亮消失，
文字已在剪贴板）」。⇒ `Done` **没有观测点**：`finish()` 之后立即 `clear()` 回 `Idle`。

保留一个永不驻留的枚举值违背 ADR-0051 自己的取向（「`SelectionMode` 枚举值先不建，避免留了字段
永远不用」）。故取 **`{ Idle, Dragging }`**。

**交互后果**：松手后高亮消失。这与 `context.md` 场景 1 的「高亮保留到下一次点击/滚动」不同 ——
以 design.md §2.4 第 9 步为准（`context.md` 只记需求来源，design.md 是定案）。
若日后要「松手后保留高亮」，只需加回 `Done` 并在 `is_active()` 里包含它。

### 2. `TranscriptLayout.rev` 拆成 `structural_rev` + `rev_alloc`

design.md §2.1 只列了 `TranscriptLayout.rev`。单字段做不到 §2.3 第 8 条
（「尾部增长、视窗原地不动 ⇒ 零动作」）：若 `rev` 是「任一条目变化就 +1」的全局计数器，
`GlyphKey.layout_rev` 会跟着变 ⇒ 窗口外条目更新也触发**全量重建**。

⇒ 拆成两个：

- `structural_rev`（只在 reset / 宽度 / thinking 变时 +1）→ `GlyphKey.layout_rev`；
- `rev_alloc`（条目版本号分配器）→ `EntryMemo.rev` → `revision_range()` 折叠成 `GlyphKey.win_rev`。

第 8 条靠「`revision_range` 只折叠**窗口内**条目」成立。

### 3. `window_rows()` 的落地形态是 `window_with_rows() -> (Window, Vec<RowRef>)`

design.md §2.2b 定了 `window_rows(&self, top_row, height) -> Vec<RowRef>`，并纪律要求
「与 `window()` 共用同一次遍历」。若两者各调一次，就是两次遍历（违反纪律）；
若 `window()` 包 `window_rows()` 再丢 `refs`，则正常渲染路径每帧多分配一个 `Vec<RowRef>`
（破坏「稳态帧分配 0」）。

⇒ 内部 `window_impl(top, h, rows: Option<&mut Vec<RowRef>>)`：`window()` 传 `None`（零分配），
`window_with_rows()` 传 `Some(..)`（一次遍历同时给出两者）。**`RowRef.line` 的语义**也在此定死：
**窗口内第几个 `Line`**（与 `Window::lines` 同序同下标），不是全局行号 —— 这样重建单行时
不必再查条目表。

### 4. `drag_tick` 是**自由函数**，`anchor_*` 不因滚动而变

- design.md §2.5 的伪码写 `app.selection.drag_tick(&mut app …)` —— 那是**借用冲突**写法
  （`app.selection` 的可变借用与 `&mut app` 同时存在）。落地为
  `pub(crate) fn drag_tick(app: &mut App, width: u16, height: u16, now: Instant)`。
- design.md §2.2 的字段注释写「自动滚动时只有 anchor 动」。按全局行号模型应为
  **只有 `cursor_*` 动**：拖到上沿要选**更早**的内容 ⇒ `cursor_row = top_row`；
  拖到下沿 ⇒ `cursor_row = top_row + inner_h - 1`。`anchor_row` 是按下点，永不因滚动改变。
  照原文实现会让选区从「按下的那一端」开始反向伸缩 —— 与「拖到边缘继续扩选」的直觉相反。

## 已知代价（ADR-0051「代价」段的落地展开）

| 项 | 代价 | 处置 |
|---|---|---|
| 选区不跨**用户滚动**存活 | 滚一下高亮即清 | 有意（决策 6）；`drag_tick` 走另一条路（§5.3） |
| 选区不跨 **resize** 存活 | 拖到一半调窗口就丢 | 有意（决策 5）。松手后 resize 无害 —— 文本已在剪贴板 |
| **流式拖选**时缓存 10 次/秒全量重建 | 每次 25–35 µs | 受 `RENDER_THROTTLE` 天然限流；V12 的 `recomputed_entries` 闸门守住 |
| **字形存两份**（`layout.entries[i].lines` + `GlyphGrid.slots`） | 视窗期 ≈ 120 KB（30×100 格 × 40 B） | `Phase::Idle` 时 `release()`（`resize(Rect::ZERO)` 保留容量） |
| 双击选词 / 三击选行 | 未做 | 待定（`context.md` 未澄清问题）；`SelectionMode` 不建 |
| 拖出 Chat 区 | 夹到边界（不取消选区） | 与候选 C 一致；按下点在内区外 ⇒ **不**开始选区 |
| OSC 52 被终端禁用 | 静默失败 | 沿用 `/copy` 同口径，不加 `pbcopy` 兜底 |
| **`⇧` 退路在 VS Code+macOS 仍不存在** | 不是本设计缺陷，但别当通用退路 | ADR-0051 已明确 |
| 触控板事件爆发 | 已用批次合并消除（`run.rs`） | 见 exec-plan T19 |
| 真实终端手感 | 单测覆盖不到 | `MANUAL_ACK`（ADR-0050 先例），客观部分由 3 个 `#[ignore]` 基准作替代闸门 |
| `x` 的语义在 M2 会变 | 同一字段、不同含义 | design.md §4 已声明：**M2 会替换 `Extractor`，不会替换 `Selection` 状态机与高亮层** |

## 测试策略与闸门

### 单测（全部 `#[cfg(test)]` 内联，不需要真终端）

`Buffer::empty(Rect::new(0, 0, w, h))` + `Paragraph::render` 即可造出与屏幕一致的字形；
`App::new(CodingView::for_test())` + `layout.sync(..)` 即可造出真实行号空间。

覆盖清单见「承重不变量清单」的 V1–V14。**不得漏**：V2（逐行 == 整窗）、V3（`shift` == 全量）、
V5（按内容裁剪）、V8（`follow` 全程 false）。

### 变异测试 `scripts/selection-mutation.py`

照 `scripts/wheel-mutation.py` 的形态（`git diff --quiet` 前置 → 基线全绿 → 逐条注入 →
`git checkout --` 还原 → 汇总表），注入 **7** 处：

| # | 注入 | 期望打红 |
|---|---|---|
| 1 | 缓存键漏 `inner` 的 `width`（`key.inner` 比较改成只比 `height`） | resize 后仍用旧字形 |
| 2 | `shift` 方向取反（`rotate_left(k)` → `rotate_right(k)`） | 拖到边缘内容反向 |
| 3 | 续格规则写成 `skip = cell_width - 1`（丢 `max`） | CJK 复制用例 |
| 4 | 高亮边界不调整宽字符（去掉 `head_col`） | CJK 半格露出 |
| 5 | 行尾不 `trim_end` | 复制尾部空格 |
| 6 | `Resize` 不清选区 | 选区错位 |
| 7 | `drag_tick` 里去掉「`follow = false` 压回」 | V8 自动滚动全程跟随断言红 |

### 性能闸门（临时值，测得基线后收紧）

| 闸门 | 阈值 | 用例 |
|---|---|---|
| 稳态帧 p99 | ≤ 基线 + 10 µs | `perf_drag_steady` |
| 无流式拖选 | `grid_rebuilds == 1` | `perf_drag_steady` |
| 流式拖选 | ≤ 10 次/秒（`recomputed_entries` 派生） | `perf_drag_steady` |
| 200 行拖选 | ≤ 5 ms | `perf_drag_scroll200` |
| 提取 200 行 | ≤ 1.5 ms | `perf_extract_200rows` |
| 稳态帧分配 | 0 | `rects` 缓冲复用 + `window()` 的 `None` 路径 |

三个基准**都是 `#[ignore]`**（照 `e2e_bench` 的先例），`cargo test -p ys-tui-coding -- --ignored` 才跑。

## 风险

| # | 风险 | 处置 |
|---|---|---|
| R1 | `RefCell` 借用冲突：`ensure_grid(&layout, ..)` 同时要 `&mut app.selection` 与 `&app.layout` | 字段级 disjoint 借用成立；若编译器拒绝，退化 `std::mem::take(&mut app.selection.grid)` 再放回（T13 说明里已写） |
| R2 | `drag_tick` 把 `follow` 弄成 `true` ⇒ `scroll_offset` 被强制归零 ⇒ `anchor_row` 与实际画面脱节 | §5.5 的六步顺序 + `debug_assert!(!app.follow)` + V8 + 变异注入 #7 |
| R3 | 批次合并丢事件（`while poll(0)` 排空时读到按键） | 用 `deferred: Option<Event>` 把非鼠标事件推迟到下一轮，**不丢**（T19） |
| R4 | `window_with_rows` 与 `window` 出现第二份定位逻辑 | 只有 `window_impl` 一份；`window()` 是薄包装（V1 守住） |
| R5 | `COPY_MAX_BYTES` 超限时往 transcript 推提示行 ⇒ 触发一次布局同步，可能与 `pending_clipboard` 的消费顺序打架 | `run.rs:211` 的消费在 `handle_event` **返回之后**，与 transcript 追加同一轮内先后无冲突；提示行只影响下一帧 |
| R6 | 真实终端 `EnableMouseCapture` 一次开 `1000/1002/1003/1015/1006`，`Moved`（悬停）会持续到达 | `Moved` 仍忽略（`_ => {}`）；拖拽期间只有 `Drag` 有语义 |
| R7 | `⇧` 在 VS Code + macOS 不生效 | 已在 ADR-0051 明确；`⇧` 只是「让回」的开关，不是退路 |
