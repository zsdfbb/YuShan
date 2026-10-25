# TUI 文本选区与复制 — 设计

> 上下文与需求见 [`context.md`](./context.md)。决策记录见
> [`docs/adr/0051-tui-text-selection.md`](../../adr/0051-tui-text-selection.md)。

## 0. 一句话

鼠标左键拖选 Chat 区 → 应用内高亮 → 松手把**屏幕字形**写进系统剪贴板（OSC 52）。
目的是消掉「复制内容要靠终端给不给的修饰键」这一跨终端差异（Alacritty 用 `⇧`、
VS Code macOS 用 `Option` + 一个默认关闭的设置）。

## 1. 候选方案对比

三个方向独立生成（并行 subagent，各自阅读了实际代码）。三者共享同一组已定前提
（M1 视口选区、Buffer 读回、拖到边缘自动滚动、`follow` 冻结、resize 清选区）。

| 维度 | **A 可扩展优先** | **B 最小复杂度** | **C 性能/资源优先** |
|---|---|---|---|
| 新增文件 | 6（`selection/{model,space,text,controller,highlight,mod}.rs`） | **1**（`selection.rs`） | 1（`selection.rs`） |
| 新概念 | 6 类型 + 2 trait + 3 枚举 | **3**（`Selection` / `Highlight` / `read_back`） | 6（`Selection` / `GlyphGrid` / `Extractor` / `Phase` / `RowRef` / `GlyphKey`） |
| 新代码 | ≈780 + 测试 320 | **≈320（含测试 110）** | ≈500 + 测试 |
| 坐标模型 | `Point` 语义外置到 trait `Space`，M2 换 impl | 屏幕内区相对 `(x,y)`，纯数据 | **全局视觉行 + x**（见 §4） |
| 每帧成本 | 未测算 | 未测算 | **0.2–6 µs（+0.1–4%）** |
| 全量重建 | 每次松手 1 次（推得） | 每次松手 1 次（推得） | **每次拖选 1 次**（缓存键 + 增量移位） |
| 自动滚动 | 16ms 可变 poll 周期 | `POLL_INTERVAL` 不变，20 行/s | 16ms + **步长加速至 8 行/帧** |
| 长距离拖选 | 支持（`shift()` 重定位 anchor） | **不支持**（锚点钉屏，最多一屏） | 支持（槽环旋转） |
| 演进缝 | M2 / 双击 / hover / 链接全留 | 全不留 | 部分留 |
| 风险 | 全仓唯一 `dyn`；6 文件偏重 | **选中能力受限一屏** | 复杂度最高（槽环 + 续格位图） |

### 关键分歧的裁决

**分歧 1：锚点要不要随内容移动？**

B 明确放弃：`拖到边缘一直滚最多选满一屏`（B §7 第 1 条）。理由是「零补偿逻辑与屏幕单元的纯粹性」。
A 用 `Space::shift()`、C 用 `order.rotate_left()` 都让锚点跟着内容走，因此**能选超过一屏**。

**裁决：必须支持超过一屏。** 依据是 `context.md` 已把「自动滚动」定为必做，而定它的唯一理由就是
「`Buffer` 只有视口大小，不做就选不了一屏以外的内容」。B 做了自动滚动却仍只能选一屏 ⇒ 那个
自动滚动退化为「在固定高度里滑动取景窗」，对「选一段长 diff / 长解释」没有帮助。**B 的这一条自相矛盾。**

**分歧 2：坐标用「屏幕行」还是「全局视觉行」？**

C 让 `Selection` 存**全局视觉行号**（`anchor_row: usize`，按下时由 `top_row + 局部 y` 钉住），
并用「槽环」把字形缓存与滚动的坐标解耦。这带来两个直接收益：

- 自动滚动的成本**与已滚行数无关**（C 实测估算：8–12 µs/步，200 行 ≈ 0.6 s / 总 0.25 ms）；
- 「选满一屏」的限制**自然消失** —— 全局行号本来就可以跨屏。

**裁决：采纳 C 的全局行号。** 这是本案最重要的技术选择。它让「自动滚动」从「必要但能力有限」
变成「必要且真能选长段」。注意这**仍然符合 M1「视口选区」的定义** —— 它不解绑「滚动后选区存活」
（决策 6 仍成立：用户主动滚动即清），只是把**拖动期间由自动滚动引起**的位移正确地表达出来。

**分歧 3：要不要为 M2 预留 trait？**

A 为「M1 → M2 零调用方改动」引入 `trait Space` + `Box<dyn Space>`，代价是全仓唯一 `dyn`、
6 个文件、≈780 行。C 的全局行号模型其实**已经是一个弱化的 M2** —— `anchor_row` 是逻辑坐标，
迁到 `TranscriptLayout` 的字符偏移只需换「行 → 字符」的解析，调用方仍只碰 `(row, x)`。

**裁决：不引 trait，取 C 的全局行号 + A 的少量缝。** 理由：A 的 `dyn` 是为「M2 换 impl」付的，
而 C 的全局行号已经把「M2 迁移面」压到「行号 → 字符偏移」这一处映射上；为一次尚未排期的迁移
预置 6 文件 + 全仓唯一 `dyn`，违反本仓「最小核心」取向。

**分歧 4：自动滚动的速率。**

B：`POLL_INTERVAL` 不变 ⇒ **20 行/s**。
C：`DRAG_TICK = 16ms` + `step = clamp(1 + held_ms/40, 1, 8)` ⇒ 200 行约 **0.6 s**。

**裁决：取 C。** 20 行/s 意味着 200 行要 10 秒，用户会以为卡死（B 自己写「不可用」—— 见 C §5）。

## 2. 采用方案

**以 C 为骨架，吸收 A 的「`⇧` 让回终端原生选择」，丢弃 A 的 trait 分层；吸收 B 的「`anchor == cursor`
不复制」防误触。**

### 2.1 模块

新增 **1 个文件** `crates/ys-tui-coding/src/selection.rs`（约 520 行含测试）。三处小改：

| 文件 | 改动 | 行数 |
|---|---|---|
| `layout.rs` | **纯追加**：`EntryMemo.rev` / `TranscriptLayout.rev` / `revision_range()` / `window_rows()` | ~40 |
| `events.rs` | `handle_mouse` 收整个 `MouseEvent`；三态分派；`scroll()` 改 `pub(crate)`（**本体保持纯净、不清选区**）；新增 `scroll_clearing_selection()` 供**用户主动**滚动入口；`Resize` 清选区 | ~60 |
| `draw.rs` | 抽 `chat_inner()` 唯一几何来源；新增 `chat_inner_rect()`；`ui()` 末尾挂 `draw_selection()` | ~35 |
| `run.rs` | `DRAG_TICK` 可变 poll 周期；鼠标批次合并；`drag_tick()` 调用 | ~30 |
| `app.rs` | `pub selection: selection::Selection` + 默认 + `jump_to_*` 里 `clear()` | ~15 |

既有函数**签名一律不动**（`ui` / `draw_transcript` / `window` / `sync` 全部保持原样）。

### 2.1b 内区几何：新增 `chat_inner_rect()`（Phase 1 核实时发现的设计盲区）

`handle_mouse` 要把 crossterm 的 `(column, row)` 转成内区相对坐标，就必须知道内区左上角在屏幕的
**绝对位置**。而 `chat_viewport()`（`draw.rs:173`）只返回 `(w, h)`，不返回原点；它依赖的
`chat_area()` / `chat_block()` 又都是**私有** `fn`。

不能把两个私有函数提 `pub(crate)` 让 `events.rs` 自己拼三段 —— `draw.rs:159` 已立纪律：

> 内区几何的**唯一来源** —— … **宽度与高度的逐项扣减都只发生在这一个调用里**，故绝不逐处手写减法。

且 `chat_viewport` 已经被迫把三段拼了一遍（只为给 `events.rs` 拿宽高）。故追加一个**内部唯一来源**：

```rust
/// 内区几何的**唯一**来源；两个 `pub(crate)` 包装都与它共用，不得另写第二条链路。
fn chat_inner(app: &App, area: Rect) -> Option<Rect>;   // None = 尺寸低于 MIN_*

pub(crate) fn chat_viewport(app: &App, w: u16, h: u16) -> (u16, u16);        // 既有签名不动
pub(crate) fn chat_inner_rect(app: &App, w: u16, h: u16) -> Option<Rect>;   // 新增，给 events.rs
```

`chat_inner_rect` 返回屏幕绝对坐标的 `Rect`（含 `x`/`y`），`events.rs` 用它做
`rel_x = column - rect.x`、`rel_y = row - rect.y`，落在 `rect` 外则 clamp + 设 `edge`。

### 2.2 核心类型

```rust
enum Phase { Idle, Dragging, Done }

pub(crate) struct Selection {
    anchor_row: usize, anchor_x: u16,   // **全局视觉行**（按下时 top_row + 局部 y）
    cursor_row: usize, cursor_x: u16,   // 同上；自动滚动时只有 anchor 动
    phase: Phase,
    edge: i8, edge_since: Option<Instant>,  // 自动滚动方向 + 按住时长（定步长）
}

/// 视窗字形缓存：一次分配、永不重分配；滚动 = 槽环旋转（不搬 cell）。
pub(crate) struct GlyphGrid {
    inner: Rect, width: u16, height: u16,
    base_row: usize,           // 屏幕第 0 行 = 全局视觉行 base_row
    slots: Buffer,             // w × h，Paragraph 直接渲染进来
    order: Vec<u16>,           // 屏幕行 → slot（环形置换）
    cont: Vec<u64>,            // 每 slot 的宽字符续格位图
    key: GlyphKey,
}

struct GlyphKey { layout_rev: u64, win_rev: u64, inner: Rect, base_row: usize, skip: u16 }
```

### 2.2b `window_rows()` —— 槽环的数据基础（**不可省**）

现有的 `layout.window(top_row, height)` 只返回 `Window { lines, skip_rows }`（`layout.rs:209`），
**不告诉你「屏幕第 y 行来自哪个 `Line` 的第几个视觉行」**。而槽环要只重建换进来的那 k 行，
就必须知道这个映射。故 `layout.rs` 追加：

```rust
/// 屏幕第 y 行（0 基，相对内区顶）的来源。
pub(crate) struct RowRef { pub line: u32, pub off: u16 }

/// 与 [`window`] **共用同一次遍历**，不得另写一份定位逻辑。
pub(crate) fn window_rows(&self, top_row: usize, height: usize) -> Vec<RowRef>
```

**纪律**：`window_rows` 与 `window` 必须由**同一个内部函数**产出（让 `window` 包一层丢掉 `RowRef`），
否则「画的位置」与「认的位置」会漂移 —— ADR-0045 已因这类错位返工过一次。

### 2.3 缓存失效集合（承重）

字形缓存 `GlyphGrid` 必须在且仅在下列时刻重建。这张表是实现的第一道正确性边界。

| # | 条件 | 动作 |
|---|---|---|
| 1 | `inner` Rect 变（resize / input 高度变 / 跨 MIN 尺寸） | 全量重建 **+ 清选区** |
| 2 | `layout.width` 变 | 全量重建 + 清选区 |
| 3 | `thinking_visible` 翻转 | 全量重建 + 清选区 |
| 4 | `reset()` / 回放替换 transcript | 全量重建 + 清选区 |
| 5 | `win_rev` 变（窗口内某条目 `render_sig` 换了） | 全量重建 |
| 6 | `base_row` 变（滚动 / 内容增长补偿） | **不重建**，`shift(k)` 增量 |
| 7 | 流式追加但产物哈希没变 | **零动作**（`sync` 里 `render_sig` 相等已短路） |
| 8 | 尾部增长、视窗原地不动（`follow == false` 的 Δ 补偿） | **零动作**（`base_row` 不变） |
| 9 | `Phase::Idle` 且无选区 | 释放 `slots`（`resize(Rect::ZERO)` 保留容量）+ 清 mask |

**拖选期间最坏 10 次/秒全量**（受 `RENDER_THROTTLE = 100ms` 限制）；无流式输出时
**每次拖选 1 次**。

### 2.4 数据流

**拖动中（每事件）**

1. `run.rs::event_loop` → `crossterm::event::poll(poll)` → `read`
2. `events::handle_event` → `Event::Mouse(m)` 传整个事件（现在只传 `kind`）
3. `handle_mouse`：
   - `m.modifiers.contains(SHIFT)` ⇒ **直接返回**，把原生选择让回终端（A 的建议，保留）
   - 减去 `chat_block().inner(chat_area(..))` 偏移得内区相对坐标；窗外则 clamp + 设 `edge`
   - `Down(Left)` ⇒ 建选区（`anchor_row = top_row + y`）、`app.follow = false`
   - `Drag(Left)` / `Up(Left)` ⇒ 更新 `cursor_row`
4. `draw::ui` → `draw_transcript` 之后调 `draw_selection(frame, inner, app)`
5. `draw_selection`：缓存命中 ⇒ 只写样式；未命中 ⇒ 先重建 `GlyphGrid`

**松手（copy-on-select）**

6. `anchor == cursor` ⇒ **不复制、清选区**（B 的防误触：按下未拖动就覆盖剪贴板是负体验）
7. 否则 → `Extractor::extract(sel)` → `app.pending_clipboard = Some(text)`
8. `run.rs:211` 已有的 `if let Some(text) = app.pending_clipboard.take() { write_clipboard(&text) }`
   当场消费 —— **零新增管线**
9. 清选区（高亮消失，文字已在剪贴板）

**取字形**（只在松手时跑一次）

10. 按 64 行分块：`layout.window(row, chunk)` → `Paragraph::{wrap(Wrap{trim:false}), scroll}` 渲进
    复用的 `grid_buf`
11. 逐行按 `[xa..=xb]` 读 `cell.symbol()`，**续格按 `skip = max(skip, cell_width) - 1` 跳过**
12. **按每行的非空白范围裁剪**再与选区区间求交（见下）
13. 行尾 `trim_end(' ')`，行间 `'\n'`

#### 提取必须按内容裁剪（高亮可以不裁）

`trim_end` 只救行尾，救不了「选区跨越了内容边界」：一行只有 3 个字而选区拖到 `x=99`，
或者选区纵跨到视窗底部的**空白填充行**（`Buffer` 在 `total_rows < inner_h` 或 `skip` 吃掉尾部行时
会出现），都会把空内容当成选中内容。

故提取时**逐行**先求该行实际的 `[first_non_blank, last_non_blank]`，再与 `[xa..=xb]` 求交；
交集为空则该行不产出内容（但**保留它作为换行**，否则段落会被拼在一行）。

**高亮仍画满矩形** —— 两者有意不一致：「块选择」的视觉直觉要矩形完整，而复制不应带出空白。

### 2.5 自动滚动

```rust
// run.rs::event_loop
let poll = if app.selection.is_dragging() && app.selection.edge() != 0
           { DRAG_TICK }          // 16ms
           else { POLL_INTERVAL }; // 50ms，与今天一致
if crossterm::event::poll(poll)? { /* … */ }
app.selection.drag_tick(&mut app /* … */);   // 在 compute_max_scroll 之前
```

- `step = clamp(1 + held_ms / 40, 1, MAX_STEP=8)` ⇒ 按住越久滚得越快，上限 ≈500 行/s。
  200 行约 **0.6 s**，总成本 **≈0.25 ms**。
- **单步成本与三层都无关**：槽环旋转 60 B（<0.1 µs）+ 8 个进入行的重置与折行（3–6 µs）+
  续格位图重建（2.4 µs）⇒ **8–12 µs/步**，与 `inner_h`、会话长度、已滚行数均无关。
- **用户主动滚动 ≠ 拖动自动滚动**：`ScrollUp/Down`、`PageUp`、`↑` 等入口调 `clear()`（决策 6）；
  只有 `drag_tick` 走 `shift()`（决策 3）。否则拖到边缘会自我取消。
- **`follow` 的夹取顺序（易错）**：`events::scroll()` 内部先 `compute_max_scroll` 再夹取，
  而 `sync_scroll_state` 在 `follow == true` 时**强制** `scroll_offset = 0`。
  故 `drag_tick` 必须**先置 `follow = false` 再调 `scroll()`**，并 `debug_assert!(!app.follow)`，
  否则 `anchor_row` 会与实际画面脱节。
- **鼠标批次合并（必须做）**：`poll()` 之后 `while poll(Duration::ZERO)` 排空，只应用**最后一个**
  `Drag`。触控板一次爆发 8–30 个事件，否则就是 8–30 次整屏 `draw`（每次 ~150 µs）。

### 2.6 高亮覆盖层

```rust
impl Widget for Highlight<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for r in self.rects {                    // 已与 area 求交
            for y in r.top()..r.bottom() { for x in r.left()..r.right() {
                buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
            }}
        }
    }
}
```

- **只改样式，不动 symbol** —— 宽字符不会被拆（续格一并 set_style 即可）。
- **不用 `Clear`** —— `Clear` 会清 symbol，正是要避免的。
- **不需要自建差集**：ratatui 每帧 `swap_buffers()` 会 reset 新 buffer ⇒ 帧内 buffer 从空开始，
  不存在「上一帧高亮残留」；`Buffer::diff_iter` 只下发真的变了的格 ⇒ 加高亮不增加 diff 格数。
- 颜色用 `REVERSED`：不猜主题，对已着色的错误行 / 代码块同样可见，不引依赖。
- **成本**：选中格数 × ~1.5 ns。3000 格 = 4.5 µs；典型 500 格 = 0.75 µs。
- 渲染位置：`chat_block().inner(chat_area(&app, area))`；在 `draw_transcript` 之后、`draw_input`
  之前；`completion` 浮层仍最后画，自然盖在上面。
- **签名用 `&mut App`**：`draw_selection(frame: &mut Frame, inner: Rect, app: &mut App)`，
  在 `ui()` 里**独立一行**调用（不要塞进 `draw_transcript`）。`draw_transcript` 拿 `&App`，
  而写 `GlyphGrid` 需要可变访问；用 `&mut` 就不必给 `grid` 再包一层 `RefCell`。

### 2.7 复制上限

OSC 52 载荷多数终端上限约 100 KB。设 `COPY_MAX_BYTES = 256 * 1024`：超限**不复制**，在 transcript
回一行提示（退化为「分段选择或用 `/export`」）。**不静默截断** —— 半截代码比复制失败更坏。

## 3. 为什么不选 A / B

- **A（可扩展优先）**：`trait Space` + `Box<dyn Space>` 换来「M2 零调用方改动」，代价 6 文件、
  ≈780 行、全仓唯一 `dyn`。而 C 的**全局视觉行**已经让 `anchor` 是逻辑坐标，M2 的迁移面只剩
  「行号 → 字符偏移」一处映射。为一个未排期的迁移预置整层抽象，与「最小核心」相悖。
  → **取它的 `⇧` 让回终端**（一条分支，零成本）与 `snap` 枚举的值（`SelectionMode` 暂不建）。
- **B（最小复杂度）**：≈320 行最省，但两个自相矛盾处致命 ——
  ① 做了自动滚动却仍只能选一屏（§1 分歧 1）；
  ② 20 行/s 使 200 行拖选需 10 秒（§1 分歧 4）。
  → **取它的 `anchor == cursor` 不复制**（防误触覆盖剪贴板，采纳）。

## 4. M1 → M2 的迁移面

采用方案里 M2 需要：

1. `anchor_row` / `cursor_row` 从「全局视觉行」改成「`TranscriptLayout` 的字符偏移」；
2. 「行 → 字符」的解析换一处；
3. `clear()` 在用户滚动时的调用去掉，改为 `shift()` 重定位。

调用方（`events.rs` / `run.rs` / `draw.rs`）**一行不改** —— 因为它们只碰 `(row, x)` 与 `Option<String>`。
这正是「不引 trait 也能保住迁移面」的依据。

**但要看清边界（避免后人误信“全部可复用”）**：M2 会**替掉 `Extractor`**（不再读 Buffer，
改从源字符串按字符区间取），**不会替掉 `Selection` 状态机与高亮层**。`x` 的语义会从
「屏幕列」变成「行内字符偏移」—— 同一字段、不同含义，故 M2 时 `x` 的每个使用点都要重审。

## 5. 已知缺口与代价

| 项 | 代价 | 处置 |
|---|---|---|
| 选区不跨**用户滚动**存活 | 滚一下高亮即清 | 有意（M1 定义，决策 6） |
| 选区不跨 **resize** 存活 | 拖到一半调窗口就丢 | 有意（决策 5）。松手后 resize 无害 |
| **流式拖选**时缓存 10 次/秒重建 | 每次 25–35 µs | 受 `RENDER_THROTTLE` 天然限流；可接受 |
| 双击选词 / 三击选行 | 未做 | 待定（`context.md` 未澄清问题） |
| 拖出 Chat 区 | 夹到边界（不取消选区） | 与 C 一致 |
| OSC 52 被终端禁用 | 静默失败 | 沿用 `/copy` 同口径，不加 `pbcopy` 兜底 |
| **字形存两份**（`layout.memo.lines` + `GlyphGrid.slots`） | 视窗期 +120 KB | `Phase::Idle` 时释放 `slots`（§2.3 第 9 条） |
| **`⇧` 退路在 VS Code+macOS 仍不存在** | 不是本设计缺陷，但别当通用退路 | xterm.js 判 Option 而非 Shift；ADR 已明确 |
| 触控板事件爆发 | 已用批次合并消除 | 见 §2.5 |
| 真实终端手感 | 单测覆盖不到 | `MANUAL_ACK`（ADR-0050 先例） |

## 6. 测试与变异测试

**单测**（`#[cfg(test)]`，用 `Buffer::empty` + `Paragraph::render`，不需真终端）：

- `GlyphGrid` 缓存失效九条（§2.3）逐条钉住
- `read_back` 的续格规则：CJK / 全角标点 / emoji / 混合 —— 已在 `context.md` 实测无损
- **逐行渲染 == 整窗渲染**（槽环的前提，已实测：短行 / 折行 / CJK / 空行四类逐格一致）
- **按内容裁剪**：选区跨到短行 / 跨到视窗底部空白填充行 ⇒ 复制结果不带空白
- `shift(k)` 与全量重建**逐格等价**（槽环正确性守卫，等价于 ADR-0045 的 N4）
- 三态迁移：`Down` 无 `Drag` ⇒ 不复制；`Down`→`Drag`→`Up` ⇒ 复制；`⇧` ⇒ 全程无副作用
- 自动滚动步长：`clamp(1 + held_ms/40, 1, 8)` 的边界，且**全程 `follow == false`**
- 复制上限：>256 KB 不复制并留提示

**限流闸门（把推论变成机制）**：`sync()` 里加 `recomputed_entries` 计数。拖选期断言
「该计数不因选区而增加」—— 否则「流式拖选最多 10 次/秒」只是推论，改
`RENDER_THROTTLE` 或 `render_sig` 语义时会静默退化而无人发现。

**既有用例必须原样全绿**：「视窗渲染 == 整段渲染」等价性、N4、`wheel` 三条。

**变异测试** `scripts/selection-mutation.py`（照 `wheel-mutation.py` 形态，注入 6 个 bug）：

| # | 注入 | 期望打红 |
|---|---|---|
| 1 | 缓存键漏 `width` | resize 后仍用旧字形 |
| 2 | `shift` 方向取反 | 拖到边缘内容反向 |
| 3 | 续格规则写成 `skip = w - 1`（丢 `max`） | CJK 复制用例 |
| 4 | 边界不调整宽字符 | CJK 半格露出 |
| 5 | 行尾不 `trim_end` | 复制尾部空格 |
| 6 | resize 不清选区 | 选区错位 |

**性能闸门**（临时值，测得基线后收紧）：稳态帧 p99 ≤ 基线 + 10 µs；无流式拖选时
`grid_rebuilds == 1`；流式拖选 ≤ 10 次/秒；200 行拖选 ≤ 5 ms；提取 200 行 ≤ 1.5 ms；
稳态帧分配次数 = 0。

## 7. 与既有决策的关系

- **第二次反转 ADR-0030 的「鼠标维持砍掉」**：滚轮已由 ADR-0050 反转；本设计反转**拖拽**。
  点击 / 悬停仍不做。
- **改口 ADR-0050 的「代价」段**：那里写「用户须 `⇧`+拖选或 `/copy`」；落地后裸拖选即可，
  `⇧` 反而成为**让回终端原生选择**的开关。
- **`docs/arch/tui-markdown-render/design.md` §2.8「拷贝友好」**：四条决定（去左右竖线 /
  表格保留框线 / 代码块无 gutter / 引用列表保持 `> ` `- `）**仍然有效** —— 应用内选区并不引
  字形过滤，拷出的仍是屏幕所见。
- **`docs/design-final/coding-agent-tui.md`** §18.4（键位）/ §18.6（已知缺口）需回写。
