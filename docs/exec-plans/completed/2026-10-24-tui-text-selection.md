# TUI 文本选区与松手复制 执行计划

> 依赖声明是**并发调度的依据**：「涉及文件」有交集的任务**不得同波**（本计划里 `selection.rs`
> 上的 10 条任务因此严格串行）。

## 上下文引用

- 设计计划：`docs/design-plans/completed/2026-10-24-tui-text-selection.md`（**任务拆分的依据，含 V1–V14 不变量编号**）
- 权威设计：`docs/arch/tui-text-selection/design.md`
- 上下文与实测事实：`docs/arch/tui-text-selection/context.md`
- 审核与修复记录：`docs/arch/tui-text-selection/review.md`
- 决策记录：`docs/adr/0051-tui-text-selection.md`

### 已核实的事实（读码得出，实施时可直接采信）

| 符号 | 现状 | 位置 |
|---|---|---|
| `handle_mouse(kind: MouseEventKind, app, width, height)` | 只认 `ScrollUp` / `ScrollDown`，其余 `_ => {}` | `events.rs:147-154` |
| `handle_event` 的 `Event::Mouse(m)` 臂 | 只传 `m.kind`（丢 `column` / `row` / `modifiers`） | `events.rs:135` |
| `Event::Resize(_, _) => {}` | 空臂 | `events.rs:137` |
| `fn scroll(app, width, height, delta: isize)` | **私有**；`delta > 0` = 向上；`next == 0` 时置 `follow = true` | `events.rs:378-388` |
| `chat_viewport(app, w, h) -> (u16, u16)` | `pub(crate)`，只给宽高；`area.width < MIN_WIDTH \|\| area.height < MIN_HEIGHT` ⇒ `(0, 0)` | `draw.rs:173-181` |
| `chat_area` / `chat_block` | **私有** `fn`；`chat_block()` = `Borders::TOP` + `Padding::bottom(INPUT_GAP_ROWS)` + title | `draw.rs:147` / `:163` |
| `MIN_WIDTH = 10` / `MIN_HEIGHT = 5` | 常量 | `draw.rs:81-82` |
| `TranscriptLayout::window(top_row, height) -> Window` | 内部 `locate_entry` + 逐条目遍历 + `memo.locate(rel)` | `layout.rs:209-275` |
| `Window { lines, skip_rows }` | `lines: Vec<Line<'static>>`；`skip_rows` 是**窗口首行内部**跳过的视觉行数 | `layout.rs:52-57` |
| `EntryMemo { sig, render_sig, lines, row_starts }` | 全部私有 | `layout.rs:31-40` |
| `TranscriptLayout` 私有字段 | `width` / `thinking_visible` / `entries` / `gaps` / `starts` / `total_rows` | `layout.rs:68-81` |
| `layout.sync(..) -> SyncResult { total_rows, width_changed }` | 宽度变或 thinking 翻转时 `entries.clear()` + `starts.clear()` | `layout.rs:118-127` / `:60-65` |
| `TranscriptLayout::reset()` | 清 entries / gaps / starts / total_rows | `layout.rs:98-103` |
| `App::sync_scroll_state` | `follow == true` ⇒ **强制** `scroll_offset = 0`；返回 `ScrollState { max_scroll, top_row }` | `app.rs:504-550`（`:539-541`） |
| `App::compute_max_scroll` | `= draw::max_scroll_offset`，**只回 `max_scroll` 不回 `top_row`** | `app.rs:490-492` |
| `App::jump_to_end` / `jump_to_start` | 改 `scroll_offset` / `follow` / `new_content_below` | `app.rs:563-573` |
| `App::reset_for_replay` | 清 transcript / md_cache / layout / tool_index，`follow = true` | `app.rs:231-245` |
| `App.layout` / `App.md_cache` | `pub(crate)` 的 `RefCell` | `app.rs:109` / `:113` |
| `App.pending_clipboard: Option<String>` | `run.rs:211` 当场 `take()` → `write_clipboard` | `app.rs:63` / `run.rs:211-213` |
| `POLL_INTERVAL = 50ms` | 模块常量 | `run.rs:59` |
| `event_loop` 结构 | `draw` → 排空 outbound → poll → read → `handle_event` → `pending_prompt` → `pending_clipboard` → tick → `compute_max_scroll` → quit 检查 | `run.rs:152-235` |
| `write_clipboard` / `clipboard_sequence` | `run.rs` 私有 `fn`（OSC 52） | `run.rs:588` / `:620` |
| `RENDER_THROTTLE = 100ms` | 只作用于 `RenderCache::lines_for` 的「源在增长」分支 | `cache.rs:29` / `:145` |
| `lib.rs` 的 `mod` 列表 | 字母序；`selection` 要插在 `run` 与 `styles` 之间 | `lib.rs:34-49` |

### 已核实的既有测试（不得改坏；只有 T18 允许按说明收窄一处）

| 用例 | 位置 | 与本计划的关系 |
|---|---|---|
| `test_total_rows_equals_whole_line_count` | `layout.rs:438` | N4；V14 |
| `test_window_anchors_at_both_ends` | `layout.rs:471` | `window()` 签名不动的守卫 |
| `test_unchanged_entries_are_not_re_rendered` | `layout.rs:514` | 用 `crate::md::render_calls()` 计数（**V12 的同型先例**） |
| `test_windowed_render_matches_full_render` | `draw.rs`（`mod tests` 内，`rows_of` helper 在 `:1102`） | 视窗渲染 ≡ 整段渲染；V14 |
| `test_wheel_scrolls_chat_and_never_input_history` | `events.rs:1602` | 滚轮语义不变 |
| `test_other_mouse_events_are_ignored` | `events.rs:1634` | **T18 必须收窄**（见 T18 说明） |
| `test_handle_event_routes_mouse_to_scroll` | `events.rs:1662` | 构造 `MouseEvent { kind, column, row, modifiers }` 的现成写法（`:1670`） |
| `pure_memo_rerender_is_identical` | `cache.rs:212` | 不相关，但基线须绿 |

## 基线

- **基线 ref**: 待 Phase 3 开始时用 `git rev-parse HEAD` 填入。
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据。
- **前置闸门**：`./scripts/par-test.sh` 必须全绿（否则基线不可信）。

## 波次预览

按「依赖」+「涉及文件交集」两栏**算出来**的（不是手排）。`selection.rs` 上的 T3–T12 因同文件而严格串行。

| 波 | 任务（可并行） | 本波关键产出 |
|---|---|---|
| 1 | T1 | `layout.rs` 追加 API |
| 2 | T2, T3 | layout 不变量测试 ∥ `selection.rs` 骨架 + `events::scroll` 可见性 |
| 3 | T4, T15 | 状态机测试 ∥ `App.selection` 字段 + 清理点 |
| 4 | T5, T16 | 最小 `GlyphGrid` + `extract` ∥ `app.rs` 清理点测试 |
| 5 | T6 | `extract` / `read_back` 测试 |
| 6 | T7 | 槽环（`shift` / `release` / 缓存三档） |
| 7 | T8 | 槽环正确性测试（V2 / V3） |
| 8 | T9 | `Highlight` + `rects` / `ensure_grid` / `edge_row` |
| 9 | T10, T13 | 高亮测试 ∥ `draw.rs` 接线 |
| 10 | T11, T14, T17 | `drag_tick` ∥ `draw.rs` 测试 ∥ `events.rs` 三态 |
| 11 | T12, T18, T19 | 自动滚动测试 ∥ `events.rs` 测试 ∥ `run.rs` 接线 |
| 12 | T20, T21 | `run.rs` 测试 ∥ 性能基准 |
| 13 | T22 | 变异测试脚本 |
| 14 | T23 | 全量回归 |
| 15 | T24 | 文档回写 |
| 16 | T25 | 文档指针校验 |

> Phase 3 请**以每节的「依赖」与「涉及文件」两栏为准重算**，本表只是校验用的一致性快照。
> 同波任务若「涉及文件」出现交集，一律串行（后一条退到下一波）。

---

### T1: layout.rs — 追加 `RowRef` / 版本号 / `window_with_rows` / `revision_range` / `recomputed_entries`

- type: impl
- 依赖: 无
- 涉及文件: `crates/ys-tui-coding/src/layout.rs`
- 说明:
  - 追加 `pub(crate) struct RowRef { pub line: u32, pub off: u16 }`（`Debug + Clone + Copy + PartialEq + Eq`）。**`line` 的语义定死为「窗口内第几个 `Line`」**（与 `Window::lines` 同序同下标），不是全局行号 —— 这样重建单行时不必再查条目表。
  - `EntryMemo` 加 `rev: u64`；`TranscriptLayout` 加 `structural_rev: u64` 与 `rev_alloc: u64`（版本号**分配器**，与 `EntryMemo.rev` 分开命名以免混淆）。`Default` 里三者都给 `0`。
  - `structural_rev` **只在** `reset()`（`layout.rs:98`）、宽度变、`thinking_visible` 翻转（即 `sync` 里 `entries.clear()` 的那个分支，`layout.rs:120-127`）时 `+1`。**不随条目内容变**。
  - `EntryMemo.rev` 的赋值：重算 `lines` 之后，若 `previous.render_sig == render_sig`（流式节流窗口内产物没变）⇒ **沿用旧的 `rev`**；否则 `self.rev_alloc += 1; memo.rev = self.rev_alloc;`。首次渲染同样分配新号。
  - 把 `window()` 的遍历体抽成私有 `fn window_impl(&self, top_row: usize, height: usize, rows: Option<&mut Vec<RowRef>>) -> Window`。`window()` 改为 `self.window_impl(top_row, height, None)` —— **签名与行为逐字节不动**，`None` 路径**不得**构造 `Vec<RowRef>`（稳态帧分配 0 的要求）。
  - 新增 `pub(crate) fn window_with_rows(&self, top_row: usize, height: usize) -> (Window, Vec<RowRef>)` = `window_impl(.., Some(&mut rows))`。
  - `RowRef` 的填充规则（在 `window_impl` 的同一遍历里，**不得另写第二份定位逻辑**）：
    - 每 push 一个 `Line`（`layout.rs:256`）就为该 `Line` 贡献的每个**可见**视觉行 push 一个 `RowRef { line: 窗口内 Line 下标, off: 该 Line 内的视觉行偏移 }`。
    - 首行被上沿切开时 `off` 从 `line_off` 起、只 push `rows - line_off` 个 ⇒ **`rows[0].off == skip_rows`**（`first_is_content` 为真的场合）。
    - 留白行（`layout.rs:232-235` 的 `Line::from("")`）也要 push：`off = 0`。
  - 新增 `pub(crate) fn revision_range(&self, top_row: usize, height: usize) -> u64`：只折叠**窗口覆盖的条目**的 `rev`（`hash = hash.rotate_left(7) ^ rev`）。**不得**遍历整个 `entries` —— 设计 §2.3 第 8 条（窗口外条目更新 ⇒ 零动作）靠这一点。
  - 新增三个只读取值器：`pub(crate) fn structural_rev(&self) -> u64`、`pub(crate) fn total_rows(&self) -> usize`、`pub(crate) fn width(&self) -> u16`（`extract` 需要折行宽度来造 `Buffer`）。
  - `SyncResult` 加 `pub recomputed_entries: usize`：统计「`previous.render_sig != render_sig`（含 `previous` 为 `None`）」的次数。既有 `total_rows` / `width_changed` **只加不改**。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib layout::` 全绿（既有 3 条用例**不改一行**）。

### T2: layout.rs — 新增 API 的不变量测试

- type: test
- 依赖: T1
- 涉及文件: `crates/ys-tui-coding/src/layout.rs`
- 说明: 在 `layout.rs` 既有 `#[cfg(test)] mod tests` 里追加（复用 `sample()` / `app_with()` helper）：
  - **V1**：多组 `(top, h)`（`top ∈ {0, 3, total/2, total-1}`；`h ∈ {1, 3, inner_h}`）× `sample()`（含 CJK / 折行 / 表格 / thinking）断言：`rows.len() <= h`；`first_is_content` 场合下 `rows[0].off == skip_rows`；`rows[y].line` 单调不减；`rows[y].line < window.lines.len()`；若 `y > 0 && rows[y].line == rows[y-1].line` 则 `rows[y].off == rows[y-1].off + 1`。
  - **同一遍历**：同一 `(top, h)` 下 `window()` 与 `window_with_rows().0` 的 `lines` / `skip_rows` 逐字段相同。
  - **`revision_range` 只覆盖窗口**：两次 `sync`，第二次**只改窗口外条目**（把 `transcript` 末尾一条换成长文本，`top_row = 0` 且 `height` 不含它）⇒ `revision_range(..)` **不变**；再改窗口内条目 ⇒ **变了**。
  - **`recomputed_entries`**：内容全不变 ⇒ 第二次 `sync` 后为 `0`；改一条 ⇒ 为 `1`。
  - **`structural_rev`**：`reset()` / 宽度变 / `thinking_visible` 翻转各 `+1`；仅内容变 ⇒ 不变。
  - **`rev_alloc` 单调**：连续 `sync` 后 `rev_alloc` 不减；产物没变的那次**不**消耗新号（`EntryMemo.rev` 不变）。
- 验收:
  - `cargo test -p ys-tui-coding --lib layout::` 全绿。
  - 手工变异自检：把 `rows[0].off` 改成恒 `0` ⇒ V1 用例变红。

### T3: selection.rs 骨架 + `lib.rs` 挂载 + `events::scroll` 可见性

- type: impl
- 依赖: T1
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`, `crates/ys-tui-coding/src/lib.rs`, `crates/ys-tui-coding/src/events.rs`
- 说明:
  - 新建 `crates/ys-tui-coding/src/selection.rs`。模块头 `//!` 写清：**为什么坐标用全局视觉行**（ADR-0051 决策 2）、**为什么字形从 Buffer 读回**（决策 3）、**槽环的等价性前提**。注释只写当前契约与设计自身的取舍，不写变更叙事、不写竞品对照。
  - `lib.rs` 的 `mod` 列表按字母序插入 `mod selection;`（`run` 与 `styles` 之间）。**不得** `pub use selection::*`（全部 `pub(crate)`）。
  - `events.rs`：`fn scroll` 改 `pub(crate) fn scroll`（一行；让 `selection::drag_tick` 能复用夹取逻辑）。**签名与语义不动，函数体一字节不改，且不清选区**（见 T17 说明第 4 条）。
    > 本任务把这一行放进 `T3` 而不是 `T17`，是为了让 T11 只依赖 T3/T9 —— 否则 `selection.rs` 的整条链要等 `events.rs` 做完。
  - 落地 `Phase { Idle, Dragging }`（`#[derive(Default)]`，`#[default] Idle`）。
  - 落地 `Selection` 的字段（`anchor_row` / `anchor_x` / `cursor_row` / `cursor_x` / `phase` / `edge` / `edge_since` / `grid` / `rects`）。`grid` 在本任务先给 `#[derive(Default)] pub(crate) struct GlyphGrid { /* T5 填充 */ }` 的空壳，T5 换成真实现。
  - 实现 `is_dragging()` / `edge()` / `is_active()` / `bounds() -> ((usize, u16), (usize, u16))`（返回 **Copy 值**，调用方不必同时持 `&app.selection` 与 `&app`）/ `begin(row, x, now)` / `extend(row, x, edge, now)` / `clear()`。
    - `begin`：`phase = Dragging`；`anchor_row = cursor_row = row`；`anchor_x = cursor_x = x`；`edge = 0`；`edge_since = None`。
    - `extend`：`cursor_row = row`、`cursor_x = x`；`edge` 变更时重置 `edge_since = Some(now)`（`edge == 0` ⇒ `None`）。
    - `clear`：`*self = Selection::default()`（连 `grid` 一起释放；`rects` 的容量随之丢失 —— 可接受，清选区是低频动作）。
  - 加 `pub(crate) fn edge_held(&self, now: Instant) -> Duration`（`edge_since` 的只读取用，供 `drag_tick` 与测试）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo clippy -p ys-tui-coding --all-targets` 无新增 warning。
  - `cargo test -p ys-tui-coding --lib events::` 全绿（`scroll` 只改可见性）。

### T4: selection.rs — 状态机测试

- type: test
- 依赖: T3
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明: 在 `selection.rs` 的 `#[cfg(test)] mod tests` 里追加：
  - `begin(5, 3)` 后 `bounds()` 的两端相等；`is_active()` / `is_dragging()` 为真；`clear()` 后 `!is_active()` 且 `!is_dragging()`。
  - **反向拖选的端点等价**：`begin(9, 20)` + `extend(2, 4, ..)` 与 `begin(2, 4)` + `extend(9, 20, ..)`，把两对端点各自 min/max 归一化后**相同**。
  - `edge` 语义：`extend(.., edge = 1, now)` ⇒ `edge() == 1` 且 `edge_held(now) == ZERO`；同 `edge` 再 `extend` 时 `edge_held` **不重置**（`edge_since` 只在方向变更时重置 —— 这是加速的前提）；`edge = 0` ⇒ `edge() == 0`。
- 验收:
  - `cargo test -p ys-tui-coding --lib selection::` 全绿。

### T5: selection.rs — 最小 `GlyphGrid`（每次整窗重渲）+ `Extractor` / `read_back` / `extract`

- type: impl
- 依赖: T3
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - **本任务是正确性优先的版本**：`GlyphGrid` 每次 `ensure` 都把整窗重渲一遍（`layout.window_with_rows` → `Paragraph` 渲进 `slots`），**不做槽环**。T7 在同一套接口上换成熟缓存，T8 用 V2/V3 守等价。
  - `GlyphGrid` 接口（T7 必须保持，不得改名/改签名）：
    - `fn ensure(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize)`
    - `fn symbol(&self, y: u16, x: u16) -> &str`
    - `fn is_cont(&self, y: u16, x: u16) -> bool`（该格是否为宽字符的**续格**）
    - `fn head_col(&self, y: u16, x: u16) -> u16`（该格所属字符的**首格**列号；非续格 ⇒ 自身）
    - 字段：`inner` / `width` / `height` / `base_row` / `slots: Buffer` / `order: Vec<u16>`（本任务恒 `(0..h)`）/ `cont: Vec<u64>` / `words_per_row` / `key: GlyphKey`。
    - `cont` 的构造：渲染后逐行扫描 `slots`，若 `cell.symbol()` 是宽字符（`unicode-width` 的判据用 ratatui 已有的 `Span::width` 或 `str` 的 `chars().count()` 与 `cell` 占位比较；**不要新引依赖**）⇒ 置 `x+1` 位为续格。
  - `const COPY_MAX_BYTES: usize = 256 * 1024;` / `const EXTRACT_CHUNK_ROWS: usize = 64;`
  - `pub(crate) enum ExtractOutcome { Copied(String), Empty, TooLarge(usize) }`
  - `pub(crate) fn extract(layout: &TranscriptLayout, anchor: (usize, u16), cursor: (usize, u16)) -> ExtractOutcome`：
    1. 端点归一化：`row_lo = min`、`row_hi = max`。跨行时首行 `xa = 行号较小者的 x`（到行尾）、末行 `xb = 另一端点 x`（从 0 起）；单行时 `xa = min(x)`、`xb = max(x)`。
    2. `anchor == cursor`（行与列都相等）⇒ `ExtractOutcome::Empty`。
    3. **逐 `EXTRACT_CHUNK_ROWS` 行分块**：`layout.window(block_row, chunk_len)` → `Paragraph::new(Text::from(window.lines)).wrap(Wrap { trim: false }).scroll((window.skip_rows, 0))` → 渲进**复用**的 `Buffer::empty(Rect::new(0, 0, layout.width(), chunk_len))`（宽度必须 = 折行宽度，否则折行不同 ⇒ 字形错位）。
    4. 每行先求该行实际的 `[first_non_blank, last_non_blank]`（空白 = `" "`），再与 `[xa..=xb]` **求交**；交集为空 ⇒ 该行产出**空串但保留换行**（否则段落会被拼成一行）。`xa > xb` ⇒ 空串（不 panic）。
    5. 区间内逐格读，单独成函数 `fn read_back(grid: &GlyphGrid, y: u16, xa: u16, xb: u16) -> String`，规则：`skip = max(skip, cell_width) - 1`，其中 `cell_width` 由 `grid.is_cont(y, x + 1)` 判定（`true` ⇒ 该格后有一格续格）。**不写 `cell.symbol().is_empty()` 之类判断**（`Cell::skip` 在 ratatui 0.30 已废弃且恒 `false`）。
    6. 行尾 `trim_end_matches(' ')`；行间 `'\n'`。
    7. 累计字节超过 `COPY_MAX_BYTES` ⇒ 立即返回 `ExtractOutcome::TooLarge(n)`，**不返回半截文本**（ADR-0051「不静默截断」）。
  - 本任务**不**接 `Selection::ensure_grid`（T9 接）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。

### T6: selection.rs — `extract` / `read_back` 测试（V4 / V5 / V10 / V13）

- type: test
- 依赖: T5
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明: 在 `selection.rs` 测试模块追加（helper：用 `TranscriptLayout::sync` 造真实行号空间）：
  - **V4 续格无损**：transcript 含 `"中文测试"` / 全角标点 `"，。！"` / emoji（`"🙂🙃"`）/ 混合行，把整行作为选区 ⇒ `Copied` 的文本**逐字等于**源文本（无多余空格）。
  - **V5 按内容裁剪**：① 一行只有 `"abc"` 而选区拖到 `x = 99` ⇒ `"abc"`（无尾部空格）；② 选区纵跨到视窗底部的**空白填充行**（review.md §1.3 的复现条件：`top` 靠底且 `h` 大于剩余行数）⇒ 不含大片空白；③ 选区跨「长折行段落 + 其后短行」⇒ 短行不带尾随空白。
  - **V10 上限**：造 > `COPY_MAX_BYTES` 的选区（`"x".repeat(300 * 1024)` 的单条 User 行）⇒ `matches!(outcome, ExtractOutcome::TooLarge(_))`，且**没有** `Copied`。
  - **V13 反向等价**：正向与反向拖选的 `Copied` 文本**逐字节相同**。
  - **V6 半边**：`anchor == cursor` ⇒ `ExtractOutcome::Empty`。
- 验收:
  - `cargo test -p ys-tui-coding --lib selection::` 全绿。
  - 手工变异自检：`skip = max(skip, w) - 1` 改成 `skip = w - 1` ⇒ V4 红。

### T7: selection.rs — 槽环：`shift(k)` / `release()` / 缓存三档

- type: impl
- 依赖: T5
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - 保持 T5 定下的 `GlyphGrid` 接口不变；只把 `ensure` 换成三档判定，并实现 `shift` / `release`：
    ```text
    ensure(layout, inner, top_row):
      1. 算 key = GlyphKey { layout_rev: layout.structural_rev(),
                             win_rev: layout.revision_range(top_row, h),
                             inner, base_row: top_row, skip: 0 }
      2. if 仅 base_row 不同（layout_rev / win_rev / inner 全等）⇒ shift(k = top_row - self.base_row)
      3. else ⇒ rebuild()
    ```
    **判定顺序不可换**：先算 key 再判，且「仅 `base_row` 变」这一档必须在全量判定**之前**排除掉结构变化。
  - `fn rebuild(..)`：`layout.window_with_rows(top_row, height)`；`slots.resize(Rect::new(0, 0, width, height))`（**尺寸不变时不重分配**；`resize` 同尺寸是 no-op 的语义要实测确认，若不是则加 `if self.width != w || self.height != h` 守卫）；`order = (0..height)`；逐行渲 `window.lines[r.line]` + `.scroll((r.off, 0))` 到 **1 行高、同宽** 的小 `Rect`；重建 `cont`。
  - `fn shift(&mut self, k: isize, window: &Window, rows: &[RowRef])`：`k > 0` ⇒ `order.rotate_left(k)`（内容向上移动 ⇒ 屏幕行 i 看到 slot i+k）、`k < 0` ⇒ `order.rotate_right(-k)`。方向搞反就是「拖到边缘内容反向」（变异注入 #2）。然后**只重建换进来的那 `|k|` 行**。`|k| as usize >= height` ⇒ 退化为 `rebuild`。
  - `fn release(&mut self)`：`slots.resize(Rect::ZERO)`（保留容量）+ `cont.clear()` + `key = GlyphKey::default()`。
  - `symbol(y, x)` / `is_cont(y, x)` / `head_col(y, x)`：一律经 `order[y]` 映射到 slot 行，`x` 越界不 panic（返回 `" "`）。
  - 加 `#[cfg(test)] rebuild_count: usize`（`rebuild` 入口 +1），供 T8/T14 的「稳态零重建」断言。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib selection::` 全绿（T4/T6 的既有断言不得因换实现而改）。

### T8: selection.rs — 槽环正确性测试（V2 / V3）

- type: test
- 依赖: T7
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - **V2 逐行 == 整窗**：对四类样本（短行 / 折行 / CJK / 空行，review.md §1.1 的实测样本可直接搬）与多组 `(top_row, height)`，把 `GlyphGrid::rebuild` 的 `slots` 与「同一 `window.lines` 整窗渲一次」的 `Buffer` **逐格**比较（`symbol()` 与 `fg` / `Modifier` 都比）。
  - **V3 `shift(k)` == 全量重建**：`ensure` 到 `top_row = t`，再 `ensure` 到 `t + k`（`k ∈ {-3..=3}`，跳过越界组合）⇒ 逐格与「直接在 `t+k` 上 `rebuild`」的 `GlyphGrid` 相同（**含 `cont` 位图**）。
  - **缓存键漏 `width` 的守卫**：`inner` 只变 `width` ⇒ `rebuild_count` +1（走 `rebuild`）；`inner` 只变 `height` ⇒ 同样 `rebuild`。
  - **只变 `base_row`** ⇒ `rebuild_count` **不变**（走 `shift`）。
  - **`release()`**：`release` 后 `slots` 面积为空、容量保留；再 `ensure` 能正常重建。
- 验收:
  - `cargo test -p ys-tui-coding --lib selection::` 全绿。
  - 手工变异自检：`rotate_left` → `rotate_right` ⇒ V3 红。

### T9: selection.rs — `Highlight` widget + `Selection::rects` / `ensure_grid` / `edge_row`

- type: impl
- 依赖: T7
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - `pub(crate) struct Highlight<'a> { rects: &'a [Rect] }` + `impl Widget`：**只** `set_style(Style::default().add_modifier(Modifier::REVERSED))`，**不动 symbol**、**不用 `Clear`**（`Clear` 会清 symbol）。矩形先与 `area` 求交再遍历。
  - `Selection::rects(&mut self, top_row: usize, inner: Rect) -> &[Rect]`：把 `(anchor, cursor)` 转成**内区局部**矩形（`rects` 复用缓冲：`clear()` 后 push，稳态零分配）。跨行 ⇒ 首行 `[xa..inner.width)`、末行 `[0..=xb]`、中间行满宽；单行 ⇒ 一个矩形。`xa` / `xb` 都要经 `grid.head_col` 收拢到字符首格（**V11 的宽字符边界**）。
  - `Selection::ensure_grid(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize)`：`Idle` ⇒ `grid.release()` 并 return；`Dragging` ⇒ `grid.ensure(layout, inner, top_row)`。
  - `fn edge_row(&self, top_row: usize, inner_h: u16) -> usize`：`edge > 0` ⇒ `top_row`；`edge < 0` ⇒ `top_row + inner_h - 1`；`0` ⇒ `cursor_row`。供 `drag_tick` 用。
  - `rects` 依赖 `grid` 已 `ensure` 过 ⇒ 文档注释写清「调用顺序：`ensure_grid` 之后才能 `rects`」。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。

### T10: selection.rs — 高亮测试（V11）

- type: test
- 依赖: T9
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - **V11 只改 style 不动 symbol**：造含 CJK 的 `Buffer`，先记 `symbol()` 序列；对任意矩形渲 `Highlight`；断言 `symbol()` 序列**逐格不变**，矩形内每格 `Modifier::REVERSED` 置位、矩形外未置位。
  - **宽字符边界**：选区右端落在 CJK 续格上 ⇒ `rects()` 给出的右边界是**首格 + 1**（即整字宽）。
  - **与 `area` 求交**：`rects` 含超出 `inner` 的成分时渲 `Highlight` 不 panic。
  - **`ensure_grid` 的 Idle 分支**：`Idle` ⇒ `grid` 被 `release`（`slots` 面积为空）。
  - **`edge_row`** 三档（`edge > 0` / `< 0` / `0`）各断言一次。
- 验收:
  - `cargo test -p ys-tui-coding --lib selection::` 全绿。

### T11: selection.rs — `drag_tick` / `drag_step`

- type: impl
- 依赖: T3, T9
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - `const MAX_STEP: usize = 8;` / `fn drag_step(held: Duration) -> usize { (1 + held.as_millis() as usize / 40).clamp(1, MAX_STEP) }`
  - `pub(crate) fn drag_tick(app: &mut App, width: u16, height: u16, now: Instant)`，**六步顺序不可换**（设计计划 §5.5；review.md R6）：
    1. `if !app.selection.is_dragging() || app.selection.edge() == 0 { return; }`
    2. `app.follow = false;`（**先于**任何 `compute_max_scroll`）
    3. `let max = app.compute_max_scroll(width, height); if max == 0 { return; }`（内容不足一屏 ⇒ 无可滚，也**不能**让 `scroll()` 把 `follow` 置回）
    4. `let step = drag_step(app.selection.edge_held(now)); crate::events::scroll(app, width, height, app.selection.edge() as isize * step as isize);` —— `edge > 0` 向上（与 `scroll` 的 `delta > 0` 语义一致，**不得**反写）
    5. `app.follow = false; debug_assert!(!app.follow);`（`scroll()` 在 `next == 0` 时会置真 —— 少这一步 ⇒ `sync_scroll_state` 强制 `scroll_offset = 0` ⇒ `anchor_row` 与实际画面脱节）
    6. 更新 `cursor_row`：`let top_row = app.sync_scroll_state(width, height, now).top_row; let inner_h = crate::draw::chat_viewport(app, width, height).1; app.selection.cursor_row = app.selection.edge_row(top_row, inner_h);`
  - **不得**调 `app.selection.clear()`（自动滚动 ≠ 用户滚动，设计计划 §5.3）。
  - 借用提示：第 6 步要 `app.selection.cursor_row = ...` 且 RHS 借 `app.selection` ⇒ 先 `let r = app.selection.edge_row(..);` 再赋值。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib selection::` 全绿（既有断言不改）。

### T12: selection.rs — 自动滚动测试（V8）

- type: test
- 依赖: T11
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`
- 说明:
  - **步长边界**：`drag_step(ZERO) == 1`；`39ms ⇒ 1`；`40ms ⇒ 2`；`280ms ⇒ 8`；`10s ⇒ 8`。
  - **V8 全程 `follow == false`**：200+ 行内容、`scroll_offset == 0`、`follow == true`；`begin` + `extend` 到下沿并置 `edge = -1`；连调 `drag_tick`（注入递增的 `Instant`）直到 `scroll_offset == max` 或 60 次上限；**每一步**断言 `!app.follow`。这条直接抓「`scroll()` 在 `next == 0` 时置 `follow = true`」的回归。
  - **方向**：`edge = -1` ⇒ `scroll_offset` 单调递减；`edge = 1` ⇒ 单调递增。
  - **不动锚点**：`drag_tick` 全程 `anchor_row` 不变、`is_dragging()` 为真、`cursor_row` 跟随边缘行。
  - **`max == 0`**：内容不足一屏 ⇒ `scroll_offset` 不动，`follow` 保持被置 `false`。
  - **不依赖真实时钟**：全部注入 `Instant`。
- 验收:
  - `cargo test -p ys-tui-coding --lib selection::` 全绿。
  - 手工变异自检：删掉第 5 步 ⇒ V8 红。

### T13: draw.rs — `chat_inner` / `chat_inner_rect` / `window_lines` / `draw_selection` 挂载

- type: impl
- 依赖: T9
- 涉及文件: `crates/ys-tui-coding/src/draw.rs`
- 说明:
  - 新增 `fn chat_inner(app: &App, area: Rect) -> Option<Rect>`：`area.width < MIN_WIDTH || area.height < MIN_HEIGHT` ⇒ `None`；否则 `Some(chat_block().inner(chat_area(app, area)))`。**内区几何的唯一来源** —— `chat_viewport` 与 `chat_inner_rect` 都经它，不得各写一条链（`draw.rs:159-162` 的纪律原文保留并扩写）。
  - `chat_viewport(app, width, height)` 改写为 `chat_inner` 的包装（`None ⇒ (0, 0)`；`Some(r) ⇒ (r.width.max(1), r.height)`）。**签名与返回值逐字节不动**。
  - 新增 `pub(crate) fn chat_inner_rect(app: &App, width: u16, height: u16) -> Option<Rect>`（`Rect::new(0, 0, width, height)` 喂 `chat_inner`）。返回**屏幕绝对坐标**（含 `x` / `y`）。
  - 新增 `fn window_lines(app: &App, top_row: usize, inner_h: usize) -> Vec<Line<'static>>`：把 `draw_transcript` 现有 `layout.window(..)` + 「空 transcript ⇒ `(no messages yet)` 占位行」两段（`draw.rs:211-220`）原样搬入，`draw_transcript` 改为调它（**行为逐字节不变**）。`draw_selection` 需要的行**也**从这里取（占位行在两条路径上必须一致）。
  - 新增 `fn draw_selection(frame: &mut Frame, area: Rect, top_row: usize, app: &mut App)`：
    ```rust
    let Some(inner) = chat_inner(app, area) else { return };
    if app.selection.is_active() {
        let layout = app.layout.borrow();
        app.selection.ensure_grid(&layout, inner, top_row);
        drop(layout);
        let rects = app.selection.rects(top_row, inner);
        frame.render_widget(Highlight { rects }, inner);
    } else {
        let layout = app.layout.borrow();
        app.selection.ensure_grid(&layout, inner, top_row); // Idle ⇒ release()
    }
    ```
    ⚠️ `app.selection` 与 `app.layout` 是**不相交字段** ⇒ 字段级借用应成立；若编译器仍拒，用 `let mut grid = std::mem::take(&mut app.selection.grid); … ; app.selection.grid = grid;`。**不得**给 `GlyphGrid` 包 `RefCell`（review.md R5 明确反对）。
  - `ui()`（`draw.rs:99`）在 `draw_transcript` 之后、`draw_input` **之前**加**独立一行** `draw_selection(frame, chunks[0], scroll.top_row, app);`。**不得**塞进 `draw_transcript`（它拿 `&App`）。
  - `completion` 浮层仍最后画（自然盖住高亮）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib draw::` 全绿（既等价性用例**不改一行**）。

### T14: draw.rs — 接线测试（`chat_inner_rect` 原点 / 高亮落在正确格 / 稳态零重建）

- type: test
- 依赖: T13
- 涉及文件: `crates/ys-tui-coding/src/draw.rs`
- 说明: 复用 `rows_of` / `TestBackend` 的既有 helper（`draw.rs:1102`）：
  - **`chat_inner_rect` 的原点**：`x == 0`（`Borders::TOP` 无左右竖线）、`y == 1`（标题线占第 0 行）；`width == w`。
  - **与 `chat_viewport` 同源**：对 `(w, h)` 多组取值断言 `rect.width.max(1) == viewport.0 && rect.height == viewport.1`；`w = MIN_WIDTH-1` / `h = MIN_HEIGHT-1` 时 `None` 且 `viewport == (0, 0)`。
  - **高亮落在正确格**：`TestBackend` 渲一帧，造选区，断言被 `REVERSED` 的格集合恰好等于 `chat_inner_rect` 内的对应矩形（`buffer.cell((x, y)).modifier`）。CJK 场景断言不会只亮半个字。
  - **稳态零重建**：同一选区连续渲两帧，`rebuild_count` 第二帧为 0。
  - **`window_lines` 与 `draw_transcript` 同源**：空 transcript 时 `window_lines` 返回占位行（`(no messages yet)`）。
- 验收:
  - `cargo test -p ys-tui-coding --lib draw::` 全绿。

### T15: app.rs — `App.selection` 字段与清理点

- type: impl
- 依赖: T3
- 涉及文件: `crates/ys-tui-coding/src/app.rs`
- 说明:
  - `App` 加 `pub selection: crate::selection::Selection`；`App::new` 里 `selection: Selection::default()`。
  - `jump_to_end()`（`app.rs:563`）/ `jump_to_start(max_scroll)`（`:570`）末尾 `self.selection.clear()`（用户主动跳转 ⇒ ADR-0051 决策 6）。
  - `reset_for_replay()`（`app.rs:231`）里 `self.selection.clear()`（回放替换 transcript ⇒ 设计 §2.3 第 4 条）。
  - **不加在 `sync_scroll_state` 里**：帧路径不能清选区（否则拖动期间每帧自我取消）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。

### T16: app.rs — 选区清理点测试

- type: test
- 依赖: T15
- 涉及文件: `crates/ys-tui-coding/src/app.rs`
- 说明:
  - `jump_to_end` / `jump_to_start` 之前造 active 选区 ⇒ 调用后 `!a.selection.is_active()`。
  - `reset_for_replay` 之前造 active 选区 ⇒ 调用后 `!a.selection.is_active()`。
  - **反例守卫**：单独调 `sync_scroll_state(..)` **不清**选区（拖动期间每帧都会走到它）。
- 验收:
  - `cargo test -p ys-tui-coding --lib app::` 全绿。

### T17: events.rs — `handle_mouse` 收整个 `MouseEvent` / 三态 / `Resize` 清选区

- type: impl
- 依赖: T3, T13, T15
- 涉及文件: `crates/ys-tui-coding/src/events.rs`
- 说明:
  - 签名改 `pub fn handle_mouse(m: MouseEvent, app: &mut App, width: u16, height: u16)`（`use crossterm::event::{MouseButton, MouseEvent, MouseEventKind}`）。`handle_event` 的 `Event::Mouse(m)` 臂改成传整个 `m`。
  - `Event::Resize(_, _) => {}` 改为 `Event::Resize(_, _) => app.selection.clear()`（ADR-0051 决策 5 / 设计 §2.3 第 1 条）。
  - `handle_mouse` 的分派顺序：
    1. **最先**：`if m.modifiers.contains(KeyModifiers::SHIFT) { return; }` —— 把原生选择让回终端（决策 1）。放在任何状态变更之前。
    2. 滚轮两臂原样保留，但改调 `scroll_clearing_selection(app, width, height, ±1)`（见第 4 条）。
    3. 左键三态：
       - `Down(MouseButton::Left)`：`let Some(inner) = draw::chat_inner_rect(app, width, height) else { return };`。若 `m.column` / `m.row` **落在 `inner` 外** ⇒ **不开始选区**（`context.md` 场景 4 的「不选」侧 —— 按下点在内区外，钳制语义无意义）。否则 `rel_x = m.column - inner.x`、`rel_y = m.row - inner.y`；`let top_row = app.sync_scroll_state(width, height, Instant::now()).top_row;`（`compute_max_scroll` **不回** `top_row`）；`app.selection.begin(top_row + rel_y as usize, rel_x, Instant::now()); app.follow = false;`
       - `Drag(MouseButton::Left)` 与 `Up(MouseButton::Left)`：先把 `m.column` / `m.row` clamp 进 `inner`，并据 clamp 方向定 `edge`（`row < inner.y` ⇒ `1`；`row > inner.bottom()-1` ⇒ `-1`；否则 `0`）；`extend(top_row + rel_y, rel_x, edge, now)`。
       - `Up(Left)` 额外收尾：`let (a, c) = app.selection.bounds();`
         - `a == c` ⇒ `clear()`（**不复制**，B 的防误触）。
         - 否则 `match selection::extract(&app.layout.borrow(), a, c)`：`Copied(t)` ⇒ `app.pending_clipboard = Some(t)`；`TooLarge(n)` ⇒ `app.transcript.push(TranscriptLine::System(format!(..)))`（文案含「超过复制上限 256 KB；请分段选择或用 /copy」）；`Empty` ⇒ 什么都不做。**然后** `clear()`。
    4. ⚠️ **`scroll()` 不得在内部清选区**（`drag_tick` 也调它 ⇒ 会自我取消，「拖到边缘继续扩选」全废）。新增 `fn scroll_clearing_selection(app, w, h, delta)`（`app.selection.clear(); scroll(app, w, h, delta)`），所有**用户主动**滚动入口改调它：`handle_mouse` 的滚轮两臂、`handle_key` 里 `PageUp` / `PageDown` / `↑` 回落 / `↓` 回落 / `End` / `Home` / `Ctrl-Home` / `Ctrl-End` 等分支。`scroll()` 本体保持纯净。
    5. `_ => {}`（`ScrollLeft` / `ScrollRight` / `Moved` / 其它按钮）一律不动 —— **`Moved` 仍忽略**（不做 hover）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo clippy -p ys-tui-coding --all-targets` 无新增 warning。

### T18: events.rs — 三态迁移与副作用测试（V6 / V7 / V9 / V10）+ 收窄既有用例

- type: test
- 依赖: T17
- 涉及文件: `crates/ys-tui-coding/src/events.rs`
- 说明:
  - ⚠️ **既有用例必须收窄**（`events.rs:1634` `test_other_mouse_events_are_ignored`）：它现在把 `Down(Left)` / `Drag(Left)` / `Up(Left)` / `Moved` 一起断言「一律不动」。新行为下左键三态**会**建选区（但仍不改 `scroll_offset` / `follow`）。⇒ **保留用例名与原断言**，只把数组收窄为 `[ScrollLeft, ScrollRight, Moved]`；左键三态移到本任务的新用例。**不得删掉原用例**（`scripts/wheel-mutation.py` 的 M3 锚点依赖它）。
  - 新增用例（用 `channels()` / `app()` / `MouseEvent { kind, column, row, modifiers }` 的既有写法，`events.rs:1670`）：
    - **V6 无拖动不复制**：`Down(Left)` 在 `chat_inner_rect` 内的某格 ⇒ `is_active()`；直接 `Up(Left)` 同格 ⇒ `pending_clipboard.is_none()` 且 `!is_active()`。
    - **`Down`→`Drag`→`Up` ⇒ 复制**：拖过 3 行 ⇒ `pending_clipboard.is_some()`、内容非空、`!is_active()`。
    - **V7 `⇧` 全程无副作用**：带 `KeyModifiers::SHIFT` 的 `Down` / `Drag` / `Up` ⇒ `scroll_offset` / `follow` / `pending_clipboard` / `is_active()` **四者全不变**。
    - **窗外 clamp + edge**：`Drag` 到 `row < inner.y` ⇒ `edge() == 1` 且 `cursor_row == top_row`；到 `row > inner.bottom()-1` ⇒ `edge() == -1`。
    - **按在窗外不开始选区**：`Down` 在 `row = 0`（Chat 顶边框）⇒ `!is_active()`。
    - **V9 resize 清选区**：active 选区后 `handle_event(Event::Resize(80, 24), ..)` ⇒ `!is_active()`。
    - **用户滚动清选区**：active 选区后 `ScrollUp` ⇒ 清；active 选区后 `PageUp` ⇒ 清。
    - **V10 超限提示**：造 > 256 KB 选区并松手 ⇒ `pending_clipboard.is_none()`，`transcript` 末行是 `System` 提示。
- 验收:
  - `cargo test -p ys-tui-coding --lib events::` 全绿。
  - `python3 scripts/wheel-mutation.py` 仍全绿（M3 锚点未被破坏）。

### T19: run.rs — `DRAG_TICK` 可变 poll + 鼠标批次合并 + `drag_tick` 调用

- type: impl
- 依赖: T11, T17
- 涉及文件: `crates/ys-tui-coding/src/run.rs`
- 说明:
  - 新增 `const DRAG_TICK: Duration = Duration::from_millis(16);`（与 `POLL_INTERVAL` 并列，`run.rs:59` 附近）。
  - 抽两个**纯函数**（便于 T20 测）：`fn poll_interval(dragging_with_edge: bool) -> Duration`（真 ⇒ `DRAG_TICK`，假 ⇒ `POLL_INTERVAL`）；`fn coalesce(first: Event, next: impl FnMut() -> Option<Event>) -> (Option<Event>, Option<Event>)`（返回「最后一个鼠标事件」+「被推迟的非鼠标事件」）。
  - `event_loop`（`run.rs:194-214`）改造：
    ```rust
    let poll = poll_interval(app.selection.is_dragging() && app.selection.edge() != 0);
    if crossterm::event::poll(poll)? {
        let first = crossterm::event::read()?;
        let (mouse, deferred) = coalesce(first, || {
            if crossterm::event::poll(Duration::ZERO).ok()? == false { return None; }
            crossterm::event::read().ok()
        });
        if let Some(m) = mouse { events::handle_event(m, app, requests, boundaries, size.width, size.height); }
        // …既有 pending_prompt / pending_clipboard 消费，**顺序与内容不动**（run.rs:200-213）…
        if let Some(other) = deferred { events::handle_event(other, app, requests, boundaries, size.width, size.height); }
    }
    ```
    - ⚠️ **`pending_clipboard` 的消费必须早于 `deferred`**：松手复制 → 当场写剪贴板，不等下一个非鼠标事件。
    - ⚠️ **不得丢事件**：`coalesce` 遇非鼠标即停并把它返回；`Key` / `Paste` / `Resize` 一律进 `deferred`。若实现中 `deferred` 只能放一个，必须保证**只有一次**「排空」调用（第二个非鼠标事件留在内核队列里，下一轮自然读到）—— **不得** `read()` 后丢弃。
  - `drag_tick` 调用点：在 `app.compute_max_scroll(size.width, size.height);`（`run.rs:224`）**之前**加 `selection::drag_tick(app, size.width, size.height, Instant::now());`（顺序原因见设计计划 §5.5）。
  - **不得**改 `TICK_INTERVAL` / outbound 排空 / `should_quit` / `outbound_open` 条件。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib run::` 全绿（既有 `restore_sequence` 等用例不改）。

### T20: run.rs — poll 周期与批次合并的测试

- type: test
- 依赖: T19
- 涉及文件: `crates/ys-tui-coding/src/run.rs`
- 说明:
  - `poll_interval(true) == DRAG_TICK`（16ms）、`poll_interval(false) == POLL_INTERVAL`（50ms）、`DRAG_TICK < POLL_INTERVAL`（防抄反）。
  - `coalesce` 的三种形态（用现成的 `Event::Mouse(MouseEvent { .. })` / `Event::Key(..)` 构造）：
    - `[Mouse, Mouse, Mouse]` ⇒ 最后一个 `Mouse` + `None`；
    - `[Mouse, Key]` ⇒ `Mouse` + `Key` 进 deferred；
    - `[Key, Mouse]` ⇒ 无 `Mouse` + `Key` 进 deferred（**不丢按键**）。
- 验收:
  - `cargo test -p ys-tui-coding --lib run::` 全绿。

### T21: 性能基准（3 个 `#[ignore]` 用例）

- type: test
- 依赖: T13, T19
- 涉及文件: `crates/ys-tui-coding/src/selection.rs`, `crates/ys-tui-coding/src/draw.rs`
- 说明: 照 `apps/coding-agent/tests/e2e_bench.rs` 的 `#[ignore]` 先例，写 3 个基准（放 `selection.rs` 的测试模块，`#[ignore = "性能基准：-- --ignored perf_"]`）：
  - `perf_drag_steady`：500 行 transcript + 稳态选区，渲 100 帧取 p99；同时断言 `rebuild_count == 1`。流式场景（每帧追加 delta）断言由 `recomputed_entries` 派生的全量重建频率 **≤ 10 次/秒**。
  - `perf_drag_scroll200`：`drag_tick` 200 次（模拟 200 行自动滚动）总耗时 **≤ 5 ms**。
  - `perf_extract_200rows`：`extract` 一个跨 200 行的选区 **≤ 1.5 ms**。
  - 每个基准开头打印实测值（`--nocapture` 可见）。阈值先按设计计划的**临时值**，测得基线后在用例注释里记录收紧后的值（review.md §6 要求明确「谁在什么时候测」—— 本任务就是那个时点）。
- 验收:
  - `cargo test -p ys-tui-coding --lib -- --ignored perf_ --nocapture` 输出三组实测值与阈值对比，全部通过。

### T22: scripts/selection-mutation.py — 7 处注入的变异测试

- type: test
- 依赖: T14, T18, T20, T21
- 涉及文件: `scripts/selection-mutation.py`
- 说明: 照 `scripts/wheel-mutation.py` 的形态（`git diff --quiet` 前置 → 基线 `cargo test -p ys-tui-coding --lib` 全绿 → 逐条 `orig.replace(old, new, 1)` 注入 → 跑测试 → `git checkout --` 还原 → Markdown 汇总表 + 非 0 退出码）。照 `scripts/cancellation-mutation.py` 加 `--list` 与「位置参数只跑指定编号」。`MUTATIONS` 七条：

  | # | 注入 | 锚点文件 | 期望打红 |
  |---|---|---|---|
  | 1 | 缓存键漏 `inner.width`：`ensure` 的键比较只比 `height` | `selection.rs` | T8 的「`inner` 只变 `width` ⇒ rebuild」 |
  | 2 | `shift` 方向取反：`order.rotate_left(k)` → `rotate_right(k)` | `selection.rs` | T8 的 V3 等价用例 |
  | 3 | 续格规则丢 `max`：`skip = max(skip, w) - 1` → `skip = w - 1` | `selection.rs` | T6 的 V4 CJK 用例 |
  | 4 | 高亮边界不调整宽字符：`head_col` 的调用点改为直接返回 `x` | `selection.rs` | T10 的宽字符边界用例 |
  | 5 | 行尾不 `trim_end`：删掉 `read_back` 的 `trim_end_matches(' ')` | `selection.rs` | T6 的 V5 裁剪用例 |
  | 6 | `Resize` 不清选区：`Event::Resize(_, _) => app.selection.clear()` → 空臂 | `events.rs` | T18 的 V9 用例 |
  | 7 | 自动滚动不压回 `follow`：删掉 `drag_tick` 第 5 步 | `selection.rs` | T12 的 V8 用例 |

  - 锚点未命中 ⇒ 记「锚点未命中（脚本失效）」并让退出码非 0。
  - `finally` 里必须 `git checkout --` 还原（绝不留下注入态）。
- 验收:
  - `python3 scripts/selection-mutation.py` 输出 7 行汇总，**7 处全部「红（抓到）」**，退出码 0。
  - 若任一条「仍绿（BUG：测试抓不到）」⇒ 回补对应用例，**不得**从清单里删条目。

### T23: 全量回归 — 既有用例逐条核对 + 全套绿

- type: test
- 依赖: T18, T20, T22
- 涉及文件: `crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/layout.rs`, `crates/ys-tui-coding/src/events.rs`, `crates/ys-tui-coding/src/run.rs`
- 说明:
  - 逐条核对设计计划「承重不变量清单」的 **V14**，确认 8 条既有用例全部存在且**断言未削弱**：
    `test_total_rows_equals_whole_line_count` / `test_window_anchors_at_both_ends` /
    `test_unchanged_entries_are_not_re_rendered` / `test_windowed_render_matches_full_render` /
    `test_wheel_scrolls_chat_and_never_input_history` / `test_other_mouse_events_are_ignored`（T18 只收窄数组）/
    `test_handle_event_routes_mouse_to_scroll` / `pure_memo_rerender_is_identical`。
  - 跑**全量**：`./scripts/par-test.sh`（项目唯一入口；直接 `cargo test --workspace` **不算跑过**）。
  - `./scripts/cargo-build.sh check --tests`（确保 `tests/` 目标也编译）；`cargo clippy --all-targets`；`cargo fmt --check`。
- 验收:
  - `./scripts/par-test.sh` 退出码 0。
  - `./scripts/cargo-build.sh check --tests` 通过，`cargo clippy --all-targets` 无新增 warning，`cargo fmt --check` 干净。

### T24: 文档回写

- type: impl
- 依赖: T23
- 涉及文件: `docs/design-final/coding-agent-tui.md`, `docs/adr/0050-tui-mouse-wheel-scroll.md`, `docs/TODO.md`
- 说明:
  - `docs/design-final/coding-agent-tui.md`
    - §18.4「键位」：把「滚轮归应用…代价：终端原生拖选失效，需 `⇧`+拖选或 `/copy`」改为「**左键拖选 = 应用内选区 + 松手即复制**（ADR-0051）；`⇧`+拖选把原生选择让回终端」。
    - §18.6「已知缺口」：删掉「鼠标捕获期间终端原生拖选失效 ⇒ 拷贝须 `⇧`+拖选或 `/copy`」，改为 ADR-0051 的代价条目（选区不跨用户滚动 / 不跨 resize；流式拖选缓存重建；双击选词不做；字形存两份）。
    - 新增一节 `§18.x 文本选区与松手复制 — as-built`：模块划分、九条失效表、`COPY_MAX_BYTES`、`DRAG_TICK`、`⇧` 语义。
  - `docs/adr/0050-tui-mouse-wheel-scroll.md`：只改**「代价」段**措辞（裸拖选已可用，`⇧` 成为让回开关）。**不改其决策**。
  - `docs/TODO.md`：删掉「TUI 鼠标：只做了滚轮」里已完成的条目（`docs/TODO.md:131` 附近），保留浮层滚轮等未做项。
  - ⚠️ **不改** `docs/adr/0030-tui-markdown-render.md`（其「鼠标维持砍掉」的反转已在 ADR-0051 的「与其它文档的关系」声明，属 ADR 负责人范畴）。
- 验收:
  - `git diff --name-only` 只含上述三个文件。
  - T25 的指针校验通过。

### T25: 文档回写的一致性校验

- type: test
- 依赖: T24
- 涉及文件: `docs/design-final/coding-agent-tui.md`
- 说明: AGENTS.md 的「事实性断言的指针纪律」要求静态断言必须可指向代码。本任务用可执行的检查代替人工目检：
  - `grep` 断言 `docs/design-final/coding-agent-tui.md` 里新写的每个符号（`Selection` / `GlyphGrid` / `Extractor` / `COPY_MAX_BYTES` / `DRAG_TICK` / `chat_inner_rect` / `window_with_rows`）在 `crates/ys-tui-coding/src/` 里都**至少命中一次**。
  - `grep` 断言 design-final 里**不再**出现旧口径「拷贝须 `⇧`+拖选或 `/copy`」「只处理滚动」。
  - `git diff --name-only` 断言改动文件集合 == `{docs/design-final/coding-agent-tui.md, docs/adr/0050-tui-mouse-wheel-scroll.md, docs/TODO.md}`。
  - 检查实现与文档的**分歧清单**是否已记入 design-final（`Phase` 两态 / `structural_rev` 与 `rev_alloc` 拆分 / 松手后高亮消失 / `drag_tick` 只动 `cursor`）—— 四处缺一即为文档滞后。
- 验收:
  - 上述 grep 断言全部成立（可在 PR 描述里贴命令与输出）。
  - 分歧清单四项齐全。

---

## 测试框架与命令

- 单元测试**内联**在各 crate 的 `#[cfg(test)]` 模块（沿用既有风格）。`selection.rs` 的测试需要真字形 ⇒ `Buffer::empty(Rect::new(0, 0, w, h))` + `Paragraph::render`（不需要真终端）；行号空间用 `App::new(CodingView::for_test())` + `layout.sync(..)`（`layout.rs` 测试的现成写法）。
- **全量**：`./scripts/par-test.sh`（**唯一入口**；`cargo test --workspace` 串行 ≈34s，脚本 ≈7.4s）
- **单 crate**：`cargo test -p ys-tui-coding`
- **只跑选区**：`cargo test -p ys-tui-coding --lib selection::`
- **基准**：`cargo test -p ys-tui-coding --lib -- --ignored perf_ --nocapture`
- **变异**：`python3 scripts/selection-mutation.py`（必须全「红（抓到）」）
- **既有变异不许退化**：`python3 scripts/wheel-mutation.py`、`python3 scripts/cancellation-mutation.py`
- **构建**：`./scripts/cargo-build.sh check --tests`、`cargo clippy --all-targets`、`cargo fmt --check`
- 测试隔离：临时目录名须含「进程内原子序号 + `process::id()`」；进程级 env 改写须走 `crate::test_env::env_lock()` + `EnvRestore`。本计划不新增 env 改写 ⇒ 仅作提醒。

## 波及文档

| 文档 | 动作 | 任务 |
|---|---|---|
| `docs/design-final/coding-agent-tui.md` | §18.4 键位、§18.6 已知缺口回写 + 新增 as-built 节 | T24 / T25 |
| `docs/adr/0050-tui-mouse-wheel-scroll.md` | 「代价」段改口（不改决策） | T25 |
| `docs/TODO.md` | 删除已完成条目 | T25 |
| `docs/adr/0030-tui-markdown-render.md` | **不改**（反转已在 ADR-0051 声明） | — |
| `docs/arch/tui-markdown-render/design.md` §2.8 | **不改**（四条字形决定仍有效） | — |
| `docs/design.md` §12 测试矩阵 | 若 V1–V14 需要挂到矩阵上，补一行 | 可选 |

## 风险与注意事项

1. **`RefCell` 借用（最高频返工点）**：`ensure_grid` 要同时碰 `&mut app.selection` 与 `&app.layout`（`RefCell`）。字段级 disjoint 借用**应当**成立；若编译器拒绝，用 `std::mem::take(&mut app.selection.grid)` 再放回。**不得**为此给 `GlyphGrid` 包 `RefCell`（review.md R5 明确反对）。
2. **`scroll()` 与清选区的关系**（T17 第 4 条）：`scroll()` **不得**在内部清选区 —— 否则 `drag_tick` 每拍自我取消。清必须发生在**用户主动**滚动入口（`scroll_clearing_selection`）。这一条直接决定「能不能拖到边缘继续扩选」。
3. **`follow` 夹取顺序**：`drag_tick` 必须 `follow = false` → `compute_max_scroll` → `scroll()` → **再** `follow = false`。少最后一步 ⇒ `scroll()` 在 `next == 0` 时置真 ⇒ `sync_scroll_state` 强制 `scroll_offset = 0` ⇒ `anchor_row` 与实际画面脱节（review.md R6）。
4. **`window_impl(.., None)` 零分配**：不得为了省事让它总构造 `Vec<RowRef>` —— 否则性能闸门「稳态帧分配 0」红。
5. **`RowRef.line` 的语义**：**窗口内**的 `Line` 下标，不是全局行号。写错会让 `shift` 重建到别的行上 —— 现有用例抓不到（字形只是错，不一定 panic），只有 V2/V3 抓得到。
6. **批次合并不得丢事件**：`while poll(Duration::ZERO)` 期间读到的 `Key` / `Paste` / `Resize` 必须进 `deferred` 并在同一轮处理。丢一个 `Key` 的症状是「快速敲键时偶发丢字」—— 最难复现的一类 bug。
7. **`⇧` 判断必须最先**：放在任何 `scroll` / 状态变更之前。
8. **`test_other_mouse_events_are_ignored` 不得删**：`scripts/wheel-mutation.py` 的 M3 锚点依赖它。T18 只收窄数组元素，保留用例名与断言强度。
9. **`chat_inner` 是唯一几何来源**：`chat_viewport` / `chat_inner_rect` / `draw_transcript` / `draw_selection` 全部经它。任何一处手写 `w - 2` / `h - 1` 都算偏离（`draw.rs:159-162` 的纪律）。
10. **`COPY_MAX_BYTES` 超限不得截断**：只 `TooLarge` + 提示，**不**返回半截文本（ADR-0051「半截代码比复制失败更坏」）。
11. **既有签名一律不动**：`ui` / `draw_transcript` / `window` / `sync` / `sync_scroll_state` / `compute_max_scroll` / `chat_viewport`。`SyncResult` **只加字段**。
12. **不引 async / tokio**：`selection.rs` 只用 `std::time::Instant`。`ys-tui-coding` 的 `tokio` 只有 `sync` feature，UI 线程**无 runtime 上下文**（`blocking_send` 会 panic）。
13. **`Frame` 不暴露 buffer**：ratatui 0.30 的 `Frame` 没有 `buffer()` / `buffer_mut()` ⇒ 高亮**必须**是「样式覆盖层」，不能读屏后改写。试图 `frame.buffer_mut()` 是在用不存在的 API。
14. **`extract` 的 Buffer 宽度必须等于折行宽度**：`Buffer::empty(Rect::new(0, 0, layout.width(), chunk_len))`。窄一号就换行不同 ⇒ 字形纵向错位。
