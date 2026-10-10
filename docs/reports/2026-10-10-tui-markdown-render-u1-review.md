# U1（md 渲染核心三件套）独立审查报告

> 审查对象：`crates/ys-tui-coding` 的 `md.rs` / `styles.rs` / `cache.rs`（新增）+ `lib.rs` / `Cargo.toml`（M）
> 基线 ref：`9e0b492faa1ce65108d257269904364db9dfe10d`
> 审查模式：**只读**。变异测试在 `/tmp/ysmut` 副本中完成，仓库工作区未留下任何改动。

## 结论

**需返工。** 存在 1 个 P0：表格窄屏守卫按「**任一行** > width」触发整条 raw 回退，而 `tui-markdown`
**不对正文段落做宽度 wrap**（`table_width` 只喂给表格）—— 在 80/120 列的常见终端下，任何含「长于窗宽的
正文段落 / 代码行」的回答都会整条退回 raw，**markdown 渲染（本特性的主要价值）被静默关掉**（已实测复现）。
其余为 P1/P2，不动摇库选型与三件套结构。

---

## 实测证据

### 1. `cargo build -p ys-tui-coding` — 退出码 0 ✅
```
$ cargo build -p ys-tui-coding ; echo exit=$?
warning: ... ys-tui-coding (lib) generated 8 warnings
exit=0
```
8 条 warning **全部**是 `md.rs` / `styles.rs` / `cache.rs` 的 `dead_code`（`render` / `raw_plain_lines`
/ `AssistantStyles` / `Slot` / `RenderCache` / `sync_len`+`finish`+`lines_for` / `RENDER_THROTTLE` /
`MAX_CACHED_SRC` 从未被使用）。这与实现者自报、`exec-plan` Task 2「预期 `dead_code`，终局零 warning 由
Task 7 把关」一致 —— U1 未接线（U2/Task 3 才有调用者），**不算缺陷**。

### 2. `cargo test -p ys-tui-coding` — 232 passed / 0 failed ✅（与基线一致）
```
test result: ok. 232 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
   Doc-tests ys_tui_coding
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; ...
```
实现者自报 232 passed 属实。其中 U1 新增 20 个单测（md 9 + styles 2 + cache 9）+ 1 个 `ignore` 文档测试。

### 3. `cargo test --workspace` — 995 passed / 0 failed / 8 ignored ✅
```
TOTAL passed=995 failed=0 ignored=8
```
改动前基线 975 passed / 7 ignored。差值 **+20 passed / +1 ignored** 恰为 U1 新增用例数（20 单测）+ `cache.rs`
`lines_for` docstring 的 1 个 `ignore` 文档测试 ⇒ 无回归，且增量完全可归因。

### 4. `cargo clippy --all-targets` — 新增文件零新增 lint ✅
- `md.rs` / `styles.rs` / `cache.rs` 命中行**只有** `dead_code`（第 1 条），无其它 lint。
- 其余 warning（15×`cloned_ref_to_slice_refs`、15×`collapsible_if`、2×`too_many_arguments`、1×`derivable_impls`、
  1×`sort_by_key`、1×`filter_next`）**全部落在既有文件**（`ys-protocol/lifecycle.rs`、`tools-basic/*`、`ys-loop/basic.rs`、
  `coding-agent/*` 等），属**既有 warning**，与本次改动无关。

### 5. `grep -n 'name = "ratatui-core"' -A1 Cargo.lock` — 只有一条 ✅
```
1249:name = "ratatui-core"
1250-version = "0.1.2"
```
两份 `Text` 类型的地基成立（`ratatui 0.30` 与库的 `ratatui-core 0.1.2` 同一 `Text`），`md.rs` 能把库返回的
`Text` 收进本仓 `ratatui::text::Line`（已编译实证）。

### 6. `grep -c "syntect\|ansi-to-tui\|onig" Cargo.lock` — 0 ✅
`default-features = false` 生效，语法高亮链（含 onig C 库）完全未进入。

### 7. `grep -rn "tui_markdown::" crates/ys-tui-coding/src/` — 只命中 md.rs / styles.rs ✅
```
md.rs:10:use tui_markdown::Options;
md.rs:28:    let text = tui_markdown::from_str_with_options(
styles.rs:17:use tui_markdown::{AlertKind, StyleSheet};
```
（其余命中为文档注释里的字符串，非代码路径。）R6 缓解的可检验形式成立。

### 8. 变异测试 M2 / M3 / M5（+ 附带 M1）— 全部变红 ✅
在 `/tmp/ysmut`（`rsync` 副本，`CARGO_TARGET_DIR` 指向仓库 `target/` 复用依赖）中注入，**仓库工作区零改动**
（见文末）。详见「变异义务复核」表。

---

## P0

### P0-1 窄屏守卫把「长正文行」也当成越界 ⇒ 整条退回 raw ⇒ markdown 渲染被静默关掉
- **位置**：`crates/ys-tui-coding/src/md.rs:56`
  `if lines.iter().any(|line| line.width() > width) { return raw_plain_lines(src, width); }`
- **事实（读库源码求证）**：`tui-markdown 0.3.10` 的 `table_width` **只被表格消费**。
  `~/.cargo/.../tui-markdown-0.3.10/src/renderer/mod.rs:91` 把 `options.table_width` 存入
  `writer.table_width`，唯一读取点是 `renderer/table.rs:70-73`（`end_table` 内）。库的 `Options::table_width`
  文档亦明写 *“Other Markdown blocks are unaffected and can be wrapped by the consuming widget.”*
  ⇒ **正文段落、代码块、标题**根本不按 `width` 折行（折行交消费方 `Paragraph`，见 `draw.rs:126`）。
- **后果**：只要回答里有一行（正文段落 / 长代码行 / 长链接行）显示宽度 > `inner_w`，`md::render` 就返回
  **整条 raw**（无样式、字面 `#` / `**` / ```` ``` ````）。终端宽 80 或 120 时这是**绝大多数真实回答**。
- **实测复现**（沙箱副本内临时用例，`render("# Title\n\n<166 字形长段落>\n\n- a\n- b\n", w, AssistantStyles)`）：

  | width | H1 是否品红（=已渲染） | 是否出现字面 `#`（=退回 raw） | 最大行宽 | 行数 |
  |---|---|---|---|---|
  | 200 | `true` | `false` | 166 | 6 |
  | **120** | **`false`** | **`true`** | 120 | 8 |
  | **80** | **`false`** | **`true`** | 80 | 9 |

  width=200（段落 166 能放下）→ 正常渲染；width≤166 → **整条退回 raw**：H1 丢失品红、字面 `#` 重新出现、
  列表 `- a` 不再渲染。即：宽度越「正常」，越容易触发，特性越「关」。
- **根因（含设计不自洽）**：design §3 不变量 3 原文是「**表格输出的每一行**显示宽度 ≤ width」；而 design §2.6
  的实现指引写成「量一遍产出行的显示宽度，**任一行** > inner_w 即令该条退回 raw 文本」。实现者照 §2.6 的粗措辞
  实现，得到「任何输出行 ≤ width」的更严语义，恰好踩中库不折正文这一事实。
- **建议方向**（需设计确认，属 U2 之前的必修项）：把守卫收敛到「表格溢出」——例如仅当越界行含表格框线字符
  （`─│┌┬┐├┼┤└┴┘`）时才回退，或干脆对正文不再量宽（交给 `Paragraph` 二次折行本就安全），使不变量 3 回到它
  的字面范围（表格行）。

---

## P1

### P1-1 256 KiB 上界只挡「命中」，不挡「存储」——超长单条仍被缓存
- **位置**：`cache.rs:110-138`。`oversized` 只参与 `hit` 判定；`if !hit { … self.slots[idx] = Some(Slot{…}); }`
  在 oversized 时依然执行 ⇒ **>256 KiB 的单条仍会写入缓存并常驻一格**（每帧重算并覆盖同一格）。
- **违反**：design §2.5「单条 > 256 KiB **不入缓存**（退化每帧现算，不失控）」。实际是「每帧现算 **但仍存一份**」，
  内存上界不再是「不入缓存」。
- **测试缺口**：`cache.rs:267` `oversized_src_is_not_cached` 只断言 `render_calls == 2`（不命中），**未断言未存储**
  ⇒ 该 case 抓不到此偏差。

### P1-2 `finish(idx)` 在 U1 内无调用者，§2.5 边界②只被一半覆盖
- `cache.rs:77-81` 的 `finish` 与 `sync_len` 增长路径（`cache.rs:57-64`）互补。U1 仅测了 `sync_len` 增长
  触发重算（`sync_len_growth_finishes_previous_line`）与 `finish` 本身（`explicit_finish_forces_rerender`），
  但**没有任何调用点**。design §2.5 边界②的「**turn 结算而不追加 transcript 行**」这一真实触发场景，须在
  U2/Task 3 接线并测试，否则尾部增量可能永久不显示。属接口细化（见文末裁定 3），本身不算缺陷，但**必须**在
  U2 落地。

### P1-3 N6 用例偏弱（近乎恒真）
- `md.rs:308` `n6_non_table_has_no_vertical_border` 断言一封**不含表格**的文档渲染后无 `│`。而库里唯一产 `│`
  的地方就是 `renderer/table.rs` ⇒ 该断言对「非表文档」天然成立，无法被任何合理变异击穿。真正的 N6（Chat 内区
  集成、拷贝友好）在 U2/Task 4。U1 这条只算占位。

### P1-4 「默认色泄漏」只检测了 DOC 里出现过的元素
- `md.rs:265` `n3_no_default_palette_leak` 遍历 `DOC` 产出的样式，只拦 Blue/Green/Cyan/白字黑底。未进入该用例的
  元素：**GFM alert**、footnote、definition-list、math（inline/display）、html、image、strikethrough 等。
  例如 `alert()` 的默认 Note=Blue / Tip=Green 是否泄漏**未被测**（当前实现已正确覆写为 Magenta/Yellow/Red，
  但测试没钉住）。M3 之所以能红，靠的是 `link`（在 DOC 内）——覆盖存在，但**并不覆盖全部 23 个方法**。

### P1-5 M2 的声明波及面在 U1 无法全验
- 合同 M2 声明涉及「N7、N4」。U1 无 N4（`line_count` 一致性属 U2）。M2 在 U1 只由 `width_change_forces_rerender`
  单独覆盖。属任务边界，不判缺陷，仅记录。

---

## P2

### P2-1 `record_render_call()` 作为 `#[cfg(test)]` 语句置于生产函数体内（轻微味道）
- `md.rs:23-24`。本 crate **无同类先例**：全 crate 缩进的 `#[cfg(test)]` 只有两处 —— 本处，以及
  `view.rs:130` 的 `#[cfg(test)] pub(crate) fn for_test()`（那是**测试专用构造函数**，不是在生产函数体内打点）。
  `draw.rs` / `run.rs` / `app.rs` 的 `#[cfg(test)]` 全是模块级 `mod tests`。该写法编译期消除、无运行时成本，
  可接受；「更稳做法」是把计数放到 `cache.rs` 里包一层（但同样是指标桩），或注入回调 —— 收益不大。

### P2-2 「只增不减」前提靠 `debug_assert!` 且对 `/new` 不成立
- `cache.rs:124-129` 的前提断言在 release 关闭。键为 `(idx, src.len(), width)`，其正确性依赖 transcript
  **只增不减、且只尾部 `push_str`**。`/new` 换 Wiring 时若 transcript 被替换为**等长异内容**，键不变 ⇒ 命中旧产物
  （返回错内容）。概率极低，但建议 `/new`/会话重置时显式 `RenderCache::default()`。

### P2-3 `raw_plain_lines` 逐字符构造 `Line` 求宽
- `md.rs:76-77` `Line::from(ch.to_string()).width()` 每字符一次分配。可直接用 `unicode-width`，但非热路径，忽略。

### P2-4 三处「覆写」值等于默认，实质非覆写
- `styles.rs`：`table_cell()`（`md.rs` 无关）返回 `Style::default()` 同默认；`definition_description()`
  同默认；`alert_label()` 返回 `kind.label()` 同默认。**23 个方法确实逐一写出**（见 D），但这三处等价默认；
  无害（默认值无色，不构成 R3 泄漏）。

---

## 测试合同逐条核对

| 条目 | 是否真被覆盖 | 证据（测试名 + 断言） | 缺口 |
|---|---|---|---|
| **N1** 依赖接入+最小渲染 | ✅（命令级） | `cargo build -p ys-tui-coding` exit 0；`Cargo.lock` 有 `tui-markdown 0.3.10` | 无（U1 无内联测试，靠命令验收，符合合同） |
| **N3** 元素覆盖与样式映射 | ✅ 强 | `md.rs::n3_heading_levels_…`（品红+BOLD+UNDERLINED+去 `#`）、`n3_bold_italic_inline_code_link`（Strong/Emph/行内码纯背景/链接纯下划线）、`n3_blockquote_list_fenced_code_table`（引用暗灰、有序标记暗灰、围栏行去除、表头 BOLD、框线暗灰）、`n3_no_default_palette_leak`；另有 `styles.rs::overrides_do_not_match_default_palette`、`chosen_shapes_are_as_designed` | 未覆盖 alert/footnote/definition-list/math/html/image 的默认色泄漏（见 P1-4） |
| **N5** 表格窄屏守卫无超宽行 | ✅ 强 | `md.rs::n5_table_never_exceeds_width`（width `1..=80` × {2/3 列 ASCII, CJK}，断言 `line.width() <= width`，CJK 用 `<= width.max(2)`）；`long_single_line_falls_back_to_raw_and_wraps`、`edge_inputs_…` 佐证 | CJK 断言在 width=1 放松为 `max(2)`（见裁定 2） |
| **N6** 非表格无 `│` | ⚠️ 弱（占位） | `md.rs::n6_non_table_has_no_vertical_border` | 近乎恒真，不可被变异击穿；真集成版属 U2（P1-3） |
| **N7** 流式节流 ≤11 次 + 条末完整 | ✅ 强 | `cache.rs::n7_streaming_throttle_bounds_calls_and_finishes_complete`（1000 delta 断言 in-loop ≤10、total ≤11、`flat_len==1000`） | 该用例输入是 1000×`x`，恒走 raw 回退，只验「调用计数 + 不丢尾」，未在「含表格的增长」上验 |

辅助用例（超出合同但良好）：`pure_memo_rerender_is_identical`、`same_key_within_window_reuses`、
`len_change_forces_rerender_after_window`、`width_change_forces_rerender`、`different_idx_are_independent_slots`、
`sync_len_growth_finishes_previous_line`、`explicit_finish_forces_rerender`、`oversized_src_is_not_cached`、
`empty_input_yields_no_overwide_lines`。

---

## 变异义务复核

| # | 我实际做的注入（在 `/tmp/ysmut` 副本） | 变红的用例（原文） | 与实现者自报 |
|---|---|---|---|
| **M2** | `cache.rs` 命中键删去 `slot.width == width`（把 `&& slot.width == width` 去掉） | `cache::tests::width_change_forces_rerender` — `assertion left == right failed: 宽度变化应重算（键必须含 width）`（1 failed / 8 passed） | 一致（自报 N7/N4；U1 落点为 width 用例） |
| **M3** | `styles.rs` 删去 `fn link()` 覆写（回落默认蓝） | **3 红**：`styles::overrides_do_not_match_default_palette`（`链接不得回落默认蓝`）、`md::tests::n3_bold_italic_inline_code_link`（md.rs:215）、`md::tests::n3_no_default_palette_leak`（md.rs:268 `漏进默认链接蓝`） | 一致（且比自报更强：3 处覆盖） |
| **M5** | `cache.rs` 摘除节流（增长分支恒为 `false`） | `cache::tests::n7_streaming_throttle_bounds_calls_and_finishes_complete` — `1 秒窗口内渲染次数应 ≤ 10（实际 1000）` | 一致 |
| **M1**（附带） | `md.rs:56` 守卫短路为 `if false && …` | `md::tests::n5_table_never_exceeds_width` — `ASCII 表格在 width=1 出现超宽行：┌───┬───┐` | — |

注入后均**先确认基线绿、再注入、再确认红**；副本已删除，仓库工作区未改动。

---

## 对实现者自报三处「收窄/偏离」的裁定

### 裁定 1 —— 「窄屏守卫可能对长正文也触发」：**不成立为「可接受」，实为 P0，不可接受**
- **不是「可能」，是「常态」**：库对正文**完全不折行**（`table_width` 只进表格），故只要有一行正文/代码
  长于窗宽即触发。实测：120/80 列下，一段 166 字形的正常回答整条退回 raw，H1 品红与去 `#` 全部失效（见 P0-1）。
- 这是「**特性的主要价值被静默关掉**」，符合 P0 判据。须在 U2 前收敛守卫范围（或修 design §2.6 措辞使之与
  §3 不变量 3 一致）。

### 裁定 2 —— 「raw 回退按显示宽度折行，N5 对 CJK 放松为 `≤ width.max(2)`」：**折行偏离成立、可辩，但引入次要缺陷；放松可接受但要讲清**
- **折行本身**：design §2.6 说 raw = 「`Assistant(src)` **原样切行**」（即按 `\n` 切、不折行，交给 `Paragraph`）。
  实现改成**按显示宽度预折行**（`md.rs:66-88`），偏离了「原样切行」。它使返回值满足「每行 ≤ width」的**字面**
  不变量，代价是引入了 design 未要求的折行。
- **次要缺陷（实测）**：预折行按**字素**而非**词/空格**边界切。`render("see https://example.com/very/long/path for details", 20)` →
  `["see https://example.", "com/very/long/path f", "or details"]` —— URL 从中间断开、`for` 被切成 `f`/`or`。
  若维持「原样切行 + `Paragraph::wrap`」，`Paragraph` 会在空格处断行、仅对超长 token 才硬切，观感更好。
- **放宽判定**：`width.max(2)` 只在 `width=1` 生效（单个 CJK 字素宽=2，放不进 1 列，物理上无法满足 `≤1`），
  属**必要**放宽，可接受；但应在用例注释里写明「仅 width=1 为物理下限」，当前注释（`md.rs:283`）已近似说明。
- **结论**：偏离真实存在、方向可辩（为满足不变量 3 的字面读法），但**建议回到「原样切行」**，并把不变量 3 收敛
  到表格行（与 P0-1 的修法同源）。

### 裁定 3 —— 「新增 `finish(idx)` 入口」：**成立且必要，可接受**
- design §2.5 边界②明确要求「该条结束时强制重算（新 delta 落到下一条 / **turn 结算**）」。「turn 结算而不追加
  transcript 行」这一场景**无法**由 `sync_len` 增长路径覆盖 ⇒ `finish` 是补全边界的**必要**细化，与 design 意图
  一致，属计划已授权的「接口在实现层面的必要细化」级改动。
- **附带条件**：U1 内 `finish` 无调用者/无集成测试，必须在 U2/Task 3 接线并补测（见 P1-2），否则边界②落空。

---

## A–G 七点结论（正文同款）

- **A. 窄屏守卫真实行为**：库**不对正文**折行（`table_width` 仅 `renderer/mod.rs:91` → `table_render::end_table`），
  故「任一行 > width 即退 raw」会在 80/120 列下把含长段落/长代码行的**整条**回答退成 raw，**静默关闭特性的主要价值**。
  ⇒ P0。
- **B. 不变量 3 的语义**：design §3 不变量 3 的字面是「**表格输出**的每一行 ≤ width」；实现按「**任何**输出行 ≤ width」
  做，属**实现者（照 §2.6 粗措辞）过度加码**，非不变量的要求。
- **C. raw 回退与既有行为**：`raw_plain_lines` 的预折行**非必要**（`draw.rs:126` 的 `Paragraph + wrap(trim:false)`
  本就会折），且引入**中词/URL 断裂**；对照 `draw.rs:445-450` 的现状（按 `\n` 切、交给 `Paragraph`）与 design §2.10
  （回放走 raw），应**原样切行**、去掉预折行。
- **D. styles.rs 23 处覆写**：`StyleSheet` trait 恰 **23** 个方法（脚本比对：trait 23 / impl 23 / missing 0 / extra 0），
  **逐项覆写齐全**。仅 3 处（`table_cell` / `definition_description` / `alert_label`）值等于默认，但均为无色，不构成 R3 泄漏。
- **E. cache.rs**：(a) 纯 memo ✅；(b) 键 = (idx, len, width) ✅；(c) 256 KiB 上界 **只挡命中不挡存储**（P1-1）；
  (d) `now: Instant` 注入 ✅（生产代码无 `Instant::now()`，仅测试用）；(e) 节流状态在缓存内 ✅（`Slot.rendered_at` + `streaming`，
  两调用点读同一份）；(f) `finish`/`sync_len` 语义自洽，但 `finish` 无接线（P1-2）；R11 特殊检查：**未发现失配** ——
  节流窗口内两个调用点（`total_rows` / `draw_transcript`）读的是**同一格**旧产物，滚动与画面自洽，仅最新 ≤100 ms 增量不显示（符合设计）。
- **F. 测试强度**：N3 / N5 / N7 强（可被 M3 / M1 / M5 击穿）✅；**N6 弱**（近乎恒真，P1-3）；默认色泄漏只覆盖 DOC 内元素（P1-4）。
  `md.rs` 的 `#[cfg(test)] record_render_call()` 置于生产函数体内属**本 crate 首例**（`view.rs:130` 不是同型先例），
  但编译期消除、可接受（P2-1）。
- **G. 依赖与边界纪律**：`tui-markdown default-features = false` ✅；`ys-tui-coding` 仍**不依赖 `ys-runtime`** ✅；
  `ys-protocol` 仍**零 tokio** ✅；`ratatui 0.30` / `crossterm 0.29` **未被改动**（`Cargo.toml` diff 只有新增 `tui-markdown`）；
  `ratatui-core` 仅 1 条 ✅；`syntect`/`ansi-to-tui`/`onig` = 0 ✅。

---

## 工作区洁净性说明

- 本审查全程**只读**（唯一写操作是本报告）。变异测试在 `/tmp/ysmut` 副本完成，副本已 `rm -rf`。
- U1 代码文件与会话开始时**逐字节一致**：`md.rs 5db2d697…`、`cache.rs a2dd3f65…`、`styles.rs 12d4589b…`。
- 注：当前 `git status` 比**会话开始的快照**多出 3 个 docs 改动（`docs/adr/0030-…`、`docs/arch/tui-markdown-render/design.md`、
  `docs/design-plans/2026-10-10-…`）。这些**非本审查所为**（本审查只 `Read` 过它们，未 `Write`/`Edit`；其 mtime 早于审查命令执行）。
  与之相对，U1 的代码/清单文件（`Cargo.lock`、`Cargo.toml`、`lib.rs`、三新增文件）保持审查开始时状态。
