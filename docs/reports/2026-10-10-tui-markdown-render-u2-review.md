# U2 review —— 缓存接线 / 几何改道 / 回放走 raw

**单元**：U2 = Task 3 [impl] + Task 4 [test]
**涉及文件**：`crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/app.rs`
**基线 ref**：`9e0b492faa1ce65108d257269904364db9dfe10d`
**审查模式**：只读；变异在 `/tmp/ys-u2-mut`（源副本 + 独立 `CARGO_TARGET_DIR`）上做，做完已删。

---

## 结论

**可放行；0 个 P0，0 个 P1。** 自述 7 条全部属实；M4 及「两处几何不一致」的加强变异均有用例抓到（无缺口）。唯一的实质发现是 **R11 一致性用例的「同一份产物」断言近乎恒真**，且它所模拟的「同帧两处取产物」场景在生产代码里不存在 —— 但这是**被测属性过弱**（P2），**不是**真实缺陷：R11 本身**未被违反**（论证见 §3）。

---

## P0 / P1 / P2 清单

### P0
无。

### P1
无。

### P2

**P2-1（draw.rs:1377-1416）节流用例的「同一份产物」断言近乎恒真，且未触及生产场景。**
`throttle_consistency_scroll_and_draw_share_product` 的第二条断言是：
```rust
let expected = Paragraph::new(text).wrap(...).line_count(40).saturating_sub(inner_h);
assert_eq!(max_scroll_offset_at(&app, 40, 40, t1), expected, "滚动上限必须按同一份（旧）产物算");
```
其中 `text = build_text_with(app,40,false,t1)`。而 `max_scroll_offset_at` 内部是
`total_rows(app, inner_w, t1) = Paragraph::new(build_text(app,w,t1)).line_count(w)` —— 两者
**字面上是同一个函数在同一缓存、同一 `now` 下的两次调用**，故断言恒等（除 width 映射外）。
更关键：`ui()` **从不调用 `max_scroll_offset`**（draw.rs:49-75），事件路径
`compute_max_scroll`（app.rs:275→draw.rs:108）**自取 `Instant::now()`**。因此该用例所声明的场景
（「同一帧内画面与滚动上限共用同一个 now」）在生产中**不发生**，用例无法击穿「两个调用点时钟不同」这类缺陷。
*但*：第三条断言 `render_calls()==1` **有真牙齿** —— 若两处各持独立缓存/节流状态，第二次取产物会重算，
`render_calls` 变 2 即红（M4 变异实测正是这一条以 `left 2 right 1` 变红，见 §5）。故用例非全盘恒真，
仅「同一份产物」那半条弱。定级 P2（弱断言），非 P0/P1（R11 无真实缺口，见 §3）。

**P2-2（draw.rs:55-56、106-107）注释宣称的「帧时钟在滚动/画面两处共用」与生产行为不符。**
`ui()` 内 `now`（draw.rs:57）只传给 `draw_transcript`，且 `draw_transcript` 只调一次 `build_text`——
「两处」实际只有一处。生产帧路径里 `build_text` **每帧只被调 1 次**（`draw_transcript` 内），
`design.md` §2.4 的「`build_text` 每帧被调 2 次（`max_scroll_offset` 一次 + `draw_transcript` 一次）」
已是**过时描述**（本单元把滚动计算移出 `ui()` 后）。属注释/文档准确性，非行为缺陷。

**P2-3（draw.rs:501）`unreachable!()` 位于绘制路径上。**
`push_non_assistant` 的 `TranscriptLine::Assistant(_) => unreachable!(...)` 只在调用方**提前分流** Assistant
时才安全。当前**两个**调用点都分流了：
- `push_transcript_line`（draw.rs:464-479）先 `if let Assistant` 走缓存后 `return`；
- `transcript_plain_lines`（draw.rs:597-601）先 `if let Assistant` 输出 raw 后 `continue`。
故当前不可达。但它是**潜在 panic 点**：任何第三个调用者（或将来 `build_text_with` 的另一变体）误传 `Assistant`
即 panic。缓解：U3 的 panic hook 会恢复终端，但绘制仍中断。建议将来改为 debug 断言 + 优雅回退或直接复用
`Assistant` 的 raw 分支。定级 P2（防御性）。

**P2-4（draw.rs:137-141）缓存命中仍每帧 clone，且 `line_count` 前再 clone 一次。**
`draw_transcript`：`let text = build_text(...)`（内部对每条 Assistant 从缓存 `iter().cloned()` ⇒ 第 1 次 clone），
随后 `Paragraph::new(text.clone()).line_count(inner_w)`（⇒ 第 2 次 clone），最后 `Paragraph::new(text)`。
即长回答内容**每帧被复制约 2 遍**。量级估算：一条 100 KiB 的 assistant 源文本，按 ~80 列/行约 1.2k 行、
每行 ~2 span；按 `Line(Span(String))` 的堆分配估，每帧约 0.3–0.6 MB 的分配/拷贝；20 fps 下约
6–12 MB/s 的分配抖动（亚毫秒~毫秒级/帧）。`MAX_CACHED_SRC = 256 KiB` 上限时约翻倍。
设计已声明「缓存省的是解析、不是拷贝」（cache.rs:101），故这是**已记账**的代价，非缺陷；仅提示 P2。

**P2-5（draw.rs:1-10）模块头 mockup 已过时。**
顶部示意仍画成四面带框（`├────┤` 等），与 `Borders::TOP`（仅顶线）不符。纯装饰性注释。

---

## 1. 逐条核实自述

| # | 自述 | 裁定 | 证据 |
|---|---|---|---|
| 1 | `App.md_cache: RefCell<RenderCache>`（app.rs:49，:89 init）；`finish_turn()`（:291）`rposition(Assistant)` 取末尾槽位 `finish(idx)`，无则跳过 | **属实** | diff `app.rs` +19 行：字段与 `RefCell::new(RenderCache::default())`；`finish_turn` 内 `rposition(|l| matches!(l, Assistant(_)))` → `borrow_mut().finish(idx)`。`finish` 仅在槽位已存在时置 `forced`（cache.rs:80-84），无 Assistant 时 `rposition=None` 跳过 |
| 2 | `ui()` 顶部 `now = Instant::now()`（draw.rs:57），帧内一路传递；`ui` 内无第二处 `Instant::now()` | **属实** | `grep -n Instant::now draw.rs` 仅在 57（`ui`）、109（`max_scroll_offset`，**不在** `ui` 内）及测试内命中 |
| 3 | Assistant 分支在 `borrow_mut()` 守卫内 `sync_len` + `lines_for(...).iter().cloned()`；非 Assistant 抽成 `push_non_assistant`（:484） | **属实** | draw.rs:464-481 守卫块；:483-484 `fn push_non_assistant` |
| 4 | 新增 `chat_block() -> Block<'static>`（:97）；`max_scroll_offset_at`（:113）与 `draw_transcript`（:132）都取 `chat_block().inner(area)`；原 4 处 `saturating_sub(2)` 站点消失 | **属实** | 两处均 `chat_block().inner(...)`；`grep saturating_sub` 残余：:310（`prompt_select` 模态框内宽，与本任务无关）、:158（`draw_input` 用 `Block::bordered()`，有意保留）、:375/:376（`window_start` 补全/模态）、:538（工具失败首行预算）、:629（`truncate_to_width`）——**均非 Chat 内区几何** |
| 5 | `pub fn max_scroll_offset`（:108）保留 3 参签名，内部自取 `Instant::now()` 后委托 `_at` | **属实** | draw.rs:108-110 `max_scroll_offset_at(app, width, height, Instant::now())` |
| 6 | `transcript_plain_lines`（:594）Assistant 按 `\n` 原样切、不预折行、不经缓存 | **属实** | draw.rs:597-601 `s.split('\n').map(str::to_string)` + `continue`；不碰 `md_cache` |
| 7 | 新增 6 用例（N4/N6/N8/两几何/节流一致性）；改动 3 既有用例 | **属实** | 新增：`n8_plain_lines_contain_raw_markdown`、`n4_line_count_equals_per_line_accumulation`、`n6_chat_inner_has_no_vertical_border_for_non_table_md`、`geometry_top_border_inner_is_h_minus_1_by_w`、`geometry_follow_keeps_last_line_and_scroll_matches_line_count`、`throttle_consistency_scroll_and_draw_share_product` = 6。改动：`test_scroll_uses_wrapped_rows_not_transcript_entries`（改常量 38/34→40/35）、`test_plain_lines_mirror_transcript_and_assistant_raw`（改名+语义）、`test_plain_lines_cjk_wrap_by_display_width`（Assistant→User） |

### `finish_turn` 的全部调用点（取证）

```
$ grep -rn "finish_turn" crates/ys-tui-coding/src/
crates/ys-tui-coding/src/app.rs:198:                self.finish_turn();   // RunFinished 臂
crates/ys-tui-coding/src/app.rs:202:                self.finish_turn();   // RunFailed  臂
crates/ys-tui-coding/src/app.rs:291:    fn finish_turn(&mut self) {
（其余为注释/测试）
```
`finish_turn` 是私有 `fn`，**仅在 `apply_event` 的两个终局事件臂**被调，`apply_event` 只由
`apply_outbound` 驱动 ⇒ **没有任何每帧路径**走到它。且 `lines_for` 重算后清 `forced`
（cache.rs:141-147 `forced: false`）、`sync_len` 只在 transcript **增长**时置 `forced`
（cache.rs:60-74）⇒ 不存在「每帧置 forced ⇒ 缓存全失效」的隐患。**属实。**

---

## 2. 几何改道（M4 方向）

- 服务于 Chat 内区的 `saturating_sub(2)` 已全部消失：`max_scroll_offset_at` 与 `draw_transcript` 都经
  `chat_block().inner(...)`。残余 `:310`（`prompt_select` 模态框）与 `:158`（`draw_input` 的
  `Block::bordered()`）**确与本任务无关**：前者是浮层自身的内宽（`w-2` 是 `Block::bordered()` 的常态几何，
  模态框保留四边），后者是 Input 窗格**有意**保留完整边框（只有 Chat 改 `Borders::TOP`）。
- `MIN_WIDTH`/`MIN_HEIGHT` 闸门、`input_height`、`Constraint::Min(3)` 自洽：`Borders::TOP` 后内区高从 `h-2`
  变 `h-1`、宽从 `w-2` 变 `w`；`ui` 的最小尺寸闸门（10×5）与 `max_scroll_offset_at` 的闸门一致；
  5 行终端下 Chat 内区高=0（与旧 `bordered` 的 `1-2=0` 相同）—— 无新冲突、无旧假定残留。
- 关键：**内区宽也变了**（`w-2`→`w`），两处都取 `inner(area)` 并 `.max(1)`，未漏。

---

## 3. R11 一致性裁定 —— **不成立（未被违反）**

设计 §2.5 边界 ① 的原话即：「节流状态**存在 `RenderCache` 内**……令 `total_rows` 与 `draw_transcript`
读同一份上次产物」——即一致性由**缓存**保证，而**不是**靠两处共用一个时钟。实现与这句话一致
（单字段 `app.md_cache`）。实现者披露的「事件路径自取 `Instant::now()`」是事实，但**不构成错位**，理由：

**单调性引理**：对某槽位（固定 width），源文本只经尾部 `push_str` 增长（cache.rs:9-10），
且 `lines_for` 命中返存储产物（内容长度 = 存储 `len`）、未命中才按**当前** `src.len` 重算并写回
（cache.rs:132-147）。故**先后两次调用的返回产物，其内容长度单调不减，且互为前缀**
（`oversized` 退化路径亦然：恒按当前长度现算，仍单调）。

**推论**：设事件路径在时刻 `t_E` 得 `total_rows_E`，其后（下一次 `terminal.draw`）帧路径在 `t_F > t_E`
得 `total_rows_F`，则 `total_rows_F ≥ total_rows_E`，故
`max_E = total_rows_E - inner_h ≤ total_rows_F - inner_h = bottom_F`。
而事件路径只把 `scroll_offset` **钳到** `max_E`；该值**只被下一次 `draw_transcript` 使用**，
彼时 `offset = bottom_F.saturating_sub(scroll_offset) ≥ 0` —— **永远合法，不会「滚动按新 / 画面按旧」**。

**具体交错（`RENDER_THROTTLE=100ms`，`run.rs` 事件循环：draw → 排空 Outbound → poll 按键 → 钳 scroll）**：
1. 帧 A 于 `t=T` 画 `P100`（`len=100`，写 `rendered_at=T`）。
2. `T+50ms` 一个 delta 令 `len=150`。按键 ↑ → 事件路径 `lines_for(len=150, now=T+50)`：
   `150>100` 且 `50<100` ⇒ **命中旧产物 `P100`** ⇒ `max_E` 按 `P100` 算（节流生效，未重算）。
3. 同/下一帧 `draw_transcript` `lines_for(len=150, now≈T+50)` ⇒ 仍命中 `P100` ⇒ 画面亦按 `P100`。
   两者一致（都是旧）。
4. `T+120ms` delta 令 `len=200`。帧路径 `lines_for(200, now=T+120)`：`120≥100` ⇒ **未命中** ⇒ 重算 `P200`、
   写回 `len=200`。此后事件路径 `lines_for(200)` 因 `len==200` **永久命中** ⇒ 得 `P200`。两者一致（都是新）。
**任何「一新一旧」的组合里，新的一定是后发生的调用**，而它更新的 `scroll_offset` 只作用于**更晚**的
`draw` ⇒ 不渲染出任何错位。「滚动按新、画面按旧」**不可复现**。

> 事件路径与帧路径确有两个时钟，但**同一份产物由缓存（`len`+`rendered_at`+`forced`）统一裁决**，
> 时钟差异只影响「这一拍算不算到点了」这一类**非关键**抖动（最多晚/早一帧成型），不影响一致性。
> 结论：R11 未被违反；实现者披露的局限**无害**。

---

## 4. 帧内一致性与节流用例强度

- `draw_transcript`（draw.rs:132-155）**同一个 `now` 同时服务 `build_text` 与 `line_count`**：
  ```rust
  let text = build_text(app, inner_w as usize, now);        // :137  now 用于取产物
  let total = Paragraph::new(text.clone())                  // :139
      .wrap(Wrap { trim: false }).line_count(inner_w);      // :141 对同一 text 计数
  ```
  `line_count` 作用在**已构建的 `text`** 上，不再次 `lines_for` ⇒ 帧内画面/行数**结构上**同源。**属实。**
- 节流用例强度：**（见 P2-1）** 能击穿「两处各持独立节流状态」（`render_calls==1`；M4 变异实测该断言
  以 `left 2 right 1` 变红），但「同一份产物」那半条在当前实现下近乎恒真，且所模拟的「`ui` 内两处同帧取产物」
  在生产中不发生（`ui` 不调 `max_scroll_offset`）。定级 **P2（弱断言）**，非「全盘恒真」。

---

## 5. 变异结果

命令（在 `/tmp/ys-u2-mut`，`cp -a` 源副本 + 独立 target）：
`CARGO_TARGET_DIR=/tmp/ys-u2-mut/target cargo test -p ys-tui-coding --lib`

### M4（两处内区一并改回 `saturating_sub(2)` / `h-2`）⇒ 3 条用例红，原文：
```
thread 'draw::tests::geometry_follow_keeps_last_line_and_scroll_matches_line_count' panicked at draw.rs:1347:9:
assertion `left == right` failed: 滚动上限 = 总视觉行 - 可见行（35）
  left: 16
 right: 15

thread 'draw::tests::test_scroll_uses_wrapped_rows_not_transcript_entries' panicked at draw.rs:777:13:
assertion `left == right` failed: N=1400：35 视觉行 - 35 可见行
  left: 3
 right: 0

thread 'draw::tests::throttle_consistency_scroll_and_draw_share_product' panicked at draw.rs:1410:9:
assertion `left == right` failed: 窗口内两处取产物共享缓存、均不重算
  left: 2
 right: 1

test result: FAILED. 239 passed; 3 failed
```

### 「两处不一致」（加强）：只把 `draw_transcript` 的 `inner_h` 改成 `h-2`，`max_scroll_offset_at` 保持正确 `h-1`
⇒ **有用例抓到**，原文：
```
thread 'draw::tests::geometry_follow_keeps_last_line_and_scroll_matches_line_count' panicked at draw.rs:1358:9:
贴底时最后一行必须可见：row 35 = "                                        "      ← 末行留空
test result: FAILED. 241 passed; 1 failed
```
**无缺口**。反向（`max` 错 `h-2`、`draw` 正确）同样被 `geometry_follow`（等式断言）+
`test_scroll_uses_wrapped_rows`（`N=1400`）+ `throttle_consistency`（width 变 ⇒ 重算 ⇒ `render_calls 2≠1`）抓到。

> 说明：若把两处改成**等值**形式（`max` 硬编码 `h-1`，恰等于 `inner(area)` 的结果），没有也不应有用例变红
> —— 那是**值等价**的写法，不产生任何实际漂移。

---

## 6. 测试合同 N4 / N6 / N8

| 合同 | 裁定 | 用例与断言 |
|---|---|---|
| **N4**（`line_count(w)` == 逐行累计，宽度 `1..=80` × {含 CJK 表格, 不含表格}） | **真被覆盖** | `draw.rs::n4_line_count_equals_per_line_accumulation`（:1263）：两源 × `w in 1..=80`，`build_text_with` 走缓存+md 渲染得 `text`，断言 `Paragraph::new(text).line_count(w) == Σ line_count(单行)`。这是滚动数学（用 `line_count` 数视觉行）赖以成立的前提 |
| **N6 占位版**（`md.rs` 层） | **真被覆盖（U1 范围）** | `md.rs::n6_non_table_has_no_vertical_border`（:540）：`md::render(非表格 md)` 产出行不含 `│` |
| **N6 集成版**（draw 层） | **真被覆盖** | `draw.rs::n6_chat_inner_has_no_vertical_border_for_non_table_md`（:1297）：`rows_of(120×40)` 实际渲染后，遍历 `chat_block().inner(chat)` 各行断言不含 `│`。**该用例能钉住 `Block::bordered()`→`Borders::TOP`**：若保留左右竖线，内区首/末列即出现 `│` 而变红 |
| **N8**（回放保留原始 markdown 源） | **真被覆盖** | `draw.rs::n8_plain_lines_contain_raw_markdown`（:1248）：`transcript_plain_lines` 输出含 `#`、`**`、`` `code` ``，且有一行恰为 `# 标题`（`\n` 原样成行） |

---

## 7. 对抗性核查（实现者未提及处）

- **`unreachable!()`（draw.rs:501）**：见 P2-3。当前两调用点均提前分流，**不可达**；潜在 panic 点。
- **回放 raw 与 `exit_payload` 不变量**：raw 行是模型原文，**exit_payload 自身不追加任何光标定位转义**
  （run.rs:500-512 只加 `\r\x1b[2K` / `\x1b[0m` / `\r\n`）。既有测试
  `exit_payload_replays_without_absolute_positioning`（run.rs:611）用硬编码行输入，**仍绿**（全量 242 passed 含之）。
  raw 不预折行 ⇒ 超长行交终端软折行，**不破坏**该不变量（也不新增转义注入面 —— 模型原文经终端输出的暴露本就存在）。
- **`finish_turn` 每帧风险**：见 §1，不存在每帧路径。
- **`sync_len` 每帧调用**：`push_transcript_line` 每条 Assistant 每帧调 `sync_len(app.transcript.len())`，
  但**仅在 transcript 增长时**才置 `forced`（cache.rs:60-74），同长度为空操作 ⇒ 不会每帧失效缓存。
- **重入 `borrow_mut`**：`push_transcript_line` 单次 `borrow_mut`，clone 后立即出作用域；`finish_turn` 另一处
  独立借用，无嵌套 ⇒ 无二次 `borrow_mut` panic 风险（不变量 5 成立）。
- **`/new` 会话重置**：`App` 在 `run.rs:112` 由 `App::new` 新建（`/new` 走新 `App`/重新接线），
  缓存随 `App` 重建，不会跨会话复用旧槽位。

---

## 8. 命令与输出

**`cargo test -p ys-tui-coding`**（真实工作区）：
```
test result: ok. 242 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
（doc-tests：0 passed / 1 ignored）
[exited with code 0]
```
（无并发干扰；未出现落在 `md.rs`/`cache.rs`/`run.rs` 的失败。）

**`cargo clippy --all-targets -p ys-tui-coding`**：
```
warning: this `impl` can be derived   →  crates/ys-protocol/src/lifecycle.rs:13
warning: `ys-protocol` (lib) generated 1 warning
```
`ys-tui-coding` **零 warning**（唯一 warning 在 `ys-protocol`，与 U2 无关的既有项）。

**`git diff <baseline> --stat`** —— 工作区**同时含 U1/U3/U2 三单元未提交改动**：
```
 Cargo.lock | Cargo.toml | app.rs(+19) | draw.rs(+282/-37) | lib.rs(+3) | run.rs(+139/-10) | docs/*
```
**U2 归属 = `app.rs` + `draw.rs`**：`app.rs` 仅新增 `md_cache` 字段/初始化/`finish_turn` 钩子（19 行）；
`draw.rs` 为缓存接线 + `Borders::TOP` 几何改道 + 回放 raw。其余文件分别属 U1（`Cargo*`/`lib.rs`/新 `md,styles,cache`）
与 U3（`run.rs`）。**U2 未越界。** 审查期间仅对 `draw.rs` 做过 `touch`（改 mtime，内容/`git diff` 不变）。

---

## 附：可否放行

**可放行。** 无 P0/P1；P2 均为弱断言 / 注释准确性 / 已记账的 clone 代价 / 潜在防御点，不阻塞。
