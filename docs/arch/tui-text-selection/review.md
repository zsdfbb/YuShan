# ADR-0051 / tui-text-selection 设计质量审核

# ADR-0051 / tui-text-selection 设计质量审核

> **修复记录（Phase 1 放行后追写）**
>
> 本报告的 R1 / R2 已在实现前**修进 `design.md`**，但仍保留原文与当时的判定 ——
> 删掉论证会让后人看不懂为什么要做那两个选择。
>
> | ID | 原判定 | 当前状态 | 修复位置 |
> |---|---|---|---|
> | R1 选区不按内容裁剪 | 🔴 必现 | ✅ **已吸收** | `design.md` §2.4「提取必须按内容裁剪」 |
> | R2 `window_rows()` 无签名 | 🔴 会返工 | ✅ **已吸收** | `design.md` §2.2b（含「共用遍历」纪律） |
> | R3 限流是推论 | 🟡 | ✅ 已吸收 | `design.md` §6 加 `recomputed_entries` 闸门 |
> | R5 借 `App` 结构 | 🟡 | ✅ 已吸收 | `design.md` §2.6 签名定为 `&mut App` |
> | R6 夹取顺序 | 🟡 | ✅ 已吸收 | `design.md` §2.5 先 `follow=false` 再 `scroll()` |
> | R4 内存 ×2 | 🟡 | ✅ 已记录 | `design.md` §5 代价表 |
> | — | — | 🆕 新发现 | `design.md` §2.1b `chat_inner_rect()`（核实 Phase 1 时发现） |
>
> **MANUAL_ACK 处置**：真实终端手感（拖拽跟手、自动滚动速率）不阻塞 —— 客观部分由
> `perf_drag_steady` / `perf_drag_scroll200` / `perf_extract_200rows` 三个 `#[ignore]` 基准
> 作替代闸门，主观部分本不应进测试合同。
>
> 下面保留审核当时的原始全文。

---

审核对象：`docs/arch/tui-text-selection/design.md`（配套 `context.md` 与
`docs/adr/0051-tui-text-selection.md`）。维度：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）。

审核方式：不只看文档，**对承重假设做了实测或读码核对**。下面每条结论都带指针。

---

## 0. 总评

| 维度 | 判定 | 一句话 |
|---|---|---|
| 可行性 | 🟡 **黄** | 核心假设经实测成立，但**两处缺陷在实现时必然踩到**（见 R1/R2） |
| 可维护性 | 🟢 **绿** | 模块边界干净、既有函数签名不动、纯函数可测 |
| 可理解性 | 🟢 **绿** | 概念少（3 个新概念），九条失效表可直接对着写测试 |
| 性能与可靠性 | 🟡 **黄** | 模型正确但**缺一条硬性预算闸门**；流式拖选的限流是推论而非机制 |
| 并发/资源安全 | 🟢 **绿** | 单线程 UI、无锁、无 tokio；`RefCell` 借用模式已规划 |

**结论：可以进入实现，但必须先修 R1、R2。** 这两条不是「可能有问题」，是照文档写就会出错。

---

## 1. 实测/读码核对结果（不靠推理的部分）

### 1.1 ✅ 「逐行渲染 == 整窗渲染」成立

ADR-0051 §4 的槽环依赖「只重建换进来的那一行」与整窗渲染等价。实测：

```
整窗渲染           逐行拼接
 0: "short"        0: "short"
 1: "this is a long line"   1: 同
 2: "that will wrap"        2: 同
 3: "around"       3: 同
 4: "中 文 测 试 …"  4: 同
 5: "测 试 中 文 …"  5: 同
```

覆盖短行 / 折行 / CJK / 空行四类，**逐格一致**。

### 1.2 ✅ 滚动时「内容纯上移一行」成立

`top_row` 变化时 `skip_rows` 会跟着变（同一 `Line` 折成 2 行时，`top` 偶数 `skip=0`、
奇数 `skip=1`），但实测最终画面满足「`r1[0..h-1] == r0[1..h]`」—— 即槽环的
`shift(k)` 假设成立，`skip` 的变化被 `Paragraph::scroll` 正确吸收。

### 1.3 🔴 新发现：滚到底部会产生**空白填充行**

实测 `top=3`（`total_rows=6`, `h=4`）时：

```
屏幕行0: "gh"
屏幕行1: "line2-abcdef"
屏幕行2: "gh"
屏幕行3: ""          ← 空
```

`max_scroll = total_rows - inner_h` 保证 `top_row` 不会超过它，但在**内容总行数 < 视窗高度**、
或 `skip` 吃掉尾部行时，视窗底部会出现空行。这不是 bug（当前 `draw_transcript` 行为相同），
但**正是 R1 的成因**：选区如果包含这些空行，复制会带出空行。

---

## 2. 风险排序（影响 × 可能性）

| ID | 风险 | 影响 | 可能性 | 定级 |
|---|---|---|---|---|
| **R1** | 选区矩形不分空白/非空白格，复制带出大片尾部空格与空行 | 中 | **高（必现）** | 🔴 |
| **R2** | `window()` 只返回 `lines + skip_rows`，槽环无法按全局行重渲染单行 | 高（返工） | **高（必现）** | 🔴 |
| R3 | 「流式拖选 ≤10 次/秒」是**推论**不是机制 | 中 | 中 | 🟡 |
| R4 | `GlyphGrid` 与 `layout` 各存一份字形，内存 ×2 | 低 | 高 | 🟡 |
| R5 | 拖拽期间借 `App` 的多重 `RefCell` + `&mut App` | 中 | 中 | 🟡 |
| R6 | 自动滚动累加 `scroll_offset` 会被 `compute_max_scroll` 夹回 | 中 | 低 | 🟡 |
| R7 | `events.rs` 里 `scroll()` 是私有 `fn`，设计说改 `pub(crate)` 但没写连带影响 | 低 | 高 | 🟢 |

---

## 3. 逐条分析

### R1 🔴 选区矩形不分空白格 —— 照文档写必错

design §2.6 的 `Highlight::render` 把**矩形内所有格**都 `set_style(REVERSED)`，
`Extractor` 也按 `[xa..=xb]` 读整段。实测后果：

- 一行只有 3 个字、选区从 `x=0` 拖到 `x=99` ⇒ 复制得到 `"abc"` + 96 个空格（`trim_end` 救回）
- 但**中间的空行**（实测 §1.3 底部那行 `""`）在选区里 ⇒ 复制得到 `"\n"`
- 更严重：一个长折行段落后面的短行，选区纵跨到它时，`xa..xb` 会跨出该行真实长度

`trim_end` 只救行尾，救不了「选区跨越了内容边界」。

**建议修法（易修复）**：读回时按**每行实际非空白范围**裁剪 —— 即对该行先算出
`[first_non_blank, last_non_blank]`，再与选区区间求交。高亮可仍画满矩形（视觉上更符合
「块选择」直觉），但**提取**必须按内容裁剪。两者可以不一致，这是有意的。

### R2 🔴 `window()` 的输出不足以支撑槽环

design §2.3 第 6 条要求「`base_row` 变 ⇒ `shift(k)` 增量，只重建换进来的 k 行」。
但 `layout.rs:209` 的 `window()` **只返回 `lines + skip_rows`**：

```rust
pub fn window(&self, top_row: usize, height: usize) -> Window   // Window { lines, skip_rows }
```

它不告诉你「屏幕第 y 行来自哪个 `Line` 的第几个视觉行」。槽环要重建单个 slot，必须知道
`RowRef { line: u32, off: u16 }` —— 而这需要遍历 `entries[idx].row_starts`，即访问
`TranscriptLayout` 的**私有字段**（`entries` / `starts` / `gaps`，`layout.rs:70-78`）。

design §2.1 说「`layout.rs` **纯追加**：`EntryMemo.rev` / `TranscriptLayout.rev` /
`revision_range()` / `window_rows()`」—— 方向对了，但**没写 `window_rows()` 返回什么**，
而这正是整个槽环的数据基础。实现时若临时决定，很可能做成「每个 slot 存一份 `Line` clone」，
那就等于放弃槽环、退回每帧全量。

**建议修法（需讨论）**：把 `window_rows()` 的签名写进设计：

```rust
/// 屏幕第 y 行（0-based，相对内区顶）对应哪个条目的第几条 Line、行内第几个视觉行。
pub(crate) struct RowRef { pub line: u32, pub off: u16 }
pub(crate) fn window_rows(&self, top_row: usize, height: usize) -> Vec<RowRef>
```

并明确：它**复用** `window()` 的遍历逻辑（避免两份定位代码漂移）—— 否则就会出现
ADR-0045 踩过的「画的位置 ≠ 认的位置」同型缺陷。

### R3 🟡 流式限流是推论，不是机制

design 说「流式拖选最坏 10 次/秒全量（受 `RENDER_THROTTLE = 100ms` 限制）」。
但 `RENDER_THROTTLE` 作用于 **md 渲染缓存**（`cache.rs:145`），它的效果是
「`render_sig` 不变的条目复用 `lines`」。而 `GlyphGrid` 的失效条件是 `win_rev` 变
（某条目 `render_sig` 换了）—— 所以限流确实**间接**生效。

问题在于：这个链条是**推出来的**，没有断言守卫。若将来有人改 `RENDER_THROTTLE` 或
`render_sig` 的语义，`GlyphGrid` 的重建频率会静默上升，而**没有任何测试会红**。

**建议（易修复）**：在 `sync()` 里加计数器（`recomputed_entries`），拖选期断言
「该计数不因选区增加」—— 3 个方案的 C 也提了同一条。这是把推论变成闸门的最小成本。

### R4 🟡 内存 ×2 是已知代价但没量化

`layout` 存 `memo.lines`（`Vec<Line>`），`GlyphGrid.slots` 存 `Buffer`（30×100 格 × 40 B
≈ 120 KB）。design §2.5 引了 120 KB，但没说明「同一屏字形存两份」。

诚实记录即可（`docs/design-final/coding-agent-tui.md` §18.6 已有同型条目「渲染行内存 ×2」）。
**不改设计**，但应加一行：`GlyphGrid` 在 `Phase::Idle` 时释放（design §2.3 第 9 条已写，
只是没说这是为了控内存）。

### R5 🟡 拖拽期的借用结构有风险

`App` 已有两个 `RefCell`（`md_cache` / `layout`，`app.rs:109/113`），而帧路径走 `&App`、
事件路径走 `&mut App`。`GlyphGrid` 要放进 `App`，那么：

- `draw_selection(frame, inner, app)` 拿 `&App`，但要**写** `grid` ⇒ 需要 `RefCell<GlyphGrid>`
  或 `&mut App`。而 `ui(frame, app: &mut App)` 确实是 `&mut`（`draw.rs:99`）⇒ 可以直接用
  `&mut app.grid`。
- 但 `draw_transcript(frame, area, app: &App, ...)` 是 `&App`。若 `draw_selection` 在它之后
  调用且需要 `&mut`，**顺序上没问题**（先搞定 `&App` 借用再拿 `&mut`），但 Rust 的 NLL 要求
  调用点分开写，不能在一句里同时持有。

**建议（易修复）**：`draw_selection` 明确签名 `fn draw_selection(frame: &mut Frame, inner: Rect, app: &mut App)`，
并在 `ui()` 里**独立一行**调用（不要塞进 `draw_transcript`）。design 说「`ui()` 末尾挂
`draw_selection()`」，方向对，但没说是 `&mut`，实现时容易写成 `&App` 再补 `RefCell`，
凭空多一个内部可变性。

### R6 🟡 自动滚动的夹取顺序

`events::scroll()`（`events.rs:378`）内部**先** `app.compute_max_scroll(width, height)` 再夹取，
并在回到底部时置 `follow = true`。design §2.5 说「拖动期间按住 `follow`」。但
`compute_max_scroll` 会调 `sync_scroll_state`，而后者在 `follow == true` 时**强制**
`scroll_offset = 0`。

⇒ 若自动滚动途中 `follow` 被任何路径置回 `true`，选区的 `anchor_row` 就会与实际画面脱节。

**建议（易修复）**：`drag_tick` 里先置 `follow = false` **再**调 `scroll()`；
并在 `scroll()` 之后断言（debug_assert）`!app.follow`。

### R7 🟢 `scroll()` 可见性

`events.rs:378` 是 `fn scroll(...)`（私有）。design §2.1 说要改 `pub(crate)`。
影响面小（只在 `events.rs` 内部被 `handle_key` / `handle_mouse` 调用），改可见性无风险。
**照着做即可**，不算问题，列出仅为确认已核对。

---

## 4. 改进建议（按修复成本分层）

### 易修复（实现前直接改文档/代码）

1. **R1**：提取时按每行非空白范围裁剪；高亮仍画满矩形。两者有意不一致。
2. **R3**：`sync()` 加 `recomputed_entries` 计数 + 拖选期断言。
3. **R5**：`draw_selection` 签名定为 `&mut App`，在 `ui()` 里独立一行调用。
4. **R6**：`drag_tick` 先 `follow = false` 再 `scroll()`，加 `debug_assert!(!app.follow)`。
5. **R4**：在设计里补一句「`Phase::Idle` 释放 `slots` 是为控内存」。

### 需讨论

6. **R2**：`window_rows() -> Vec<RowRef>` 的签名与「复用 `window()` 遍历、不写第二份定位逻辑」
   这条纪律，应写进 design.md。这是**唯一会导致返工的风险** —— 若实现时才发现，
   槽环的整个增量机制要重做。
7. **双击/三击**：`context.md` 仍列为「待定」。设计里 `SelectionMode` 枚举已被裁掉
   （design §3 明确「枚举值先不建」）。若近期要加，`snap()` 的缝已无 —— 需重新评估。
8. **复制上限 256 KB 的位置**：design 说「超限不复制、留一行提示」。但提示写进
   transcript 会**触发一次布局同步**（行数变化），而此刻正处在 `Up` 事件处理中 ——
   需确认不会与 `pending_clipboard` 的消费顺序打架。

### 架构级（本轮不做，但需留指针）

9. **M2 迁移面**：design §4 说「调用方一行不改」。这个结论依赖「M2 的 `anchor` 仍是
   `(row, x)` 形状」。若 M2 改成字符偏移，`x` 的语义从「列」变成「行内偏移」，
   `Extractor` 的读字形方式要整体换掉（不再读 Buffer）。建议在 ADR 里明说
   **「M2 会替换 Extractor，不会替换 Controller」**，避免后人误以为全都能复用。
10. **`⇧` 放行与终端差异**：`⇧` 让回终端在 Alacritty 有效（原生 `⇧` 选择），但在
    VS Code + macOS 上 `⇧` **也不生效**（xterm.js 判 Option）。这不是本设计的缺陷，
    但意味着「`⇧` 退路」在报告问题的那台机器上依然不存在。建议 ADR 明确这一条，
    避免用户以为 `⇧` 是通用退路。

---

## 5. 可理解性检查

- **概念数**：`Selection` / `GlyphGrid` / `Extractor` 三个新概念，与「最小复杂度」方案持平，
  远少于「可扩展优先」的 9 个。✅
- **九条失效表**（design §2.3）是全篇最有价值的部分 —— 它可以直接一对一翻译成测试。
  ✅
- **薄弱处**：§2.5 的自动滚动与 §2.3 第 6 条（`base_row` 变）之间的关系没有画在一起。
  读者需要自己把「滚动改变 `base_row`」与「槽环 `shift(k)`」连起来。建议在 §2.5 加一句
  交叉引用。

## 6. 测试覆盖审查

design §6 列了 6 条单测 + 6 个变异注入 + 6 个性能闸门。评估：

- ✅ 续格规则（CJK/全角/emoji）有变异注入，且我已实测该规则正确。
- ✅ `shift(k)` 与全量重建逐格等价 —— 这是槽环的正确性守卫，**必须有**。
- ⚠️ **缺 R1 的用例**：选区跨到短行 / 跨到空白行，复制结果应为裁剪后的内容。
- ⚠️ **缺 R6 的用例**：自动滚动期间 `follow` 不被置回 true。
- ⚠️ 性能闸门是「临时值，测得基线后收紧」—— 需明确**谁在什么时候测**，
  否则会像 ADR-0050 的 `MANUAL_ACK` 一样长期悬空。
