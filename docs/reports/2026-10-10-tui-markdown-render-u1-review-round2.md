# U1（md 渲染核心）第 2 轮独立复审报告

> 审查对象：`crates/ys-tui-coding` 的 `md.rs` / `cache.rs`（返工后）+ 相关 `styles.rs`
> 基线 ref：`9e0b492faa1ce65108d257269904364db9dfe10d`
> 审查模式：**只读**。独立复现与变异在 `/tmp/ysu1r2` 副本中完成，仓库工作区未留改动。
> 前置：第 1 轮报告 `docs/reports/2026-10-10-tui-markdown-render-u1-review.md`（P0-1 + 两条附带建议）。

## 结论

**可放行（无 P0）。** 第 1 轮 P0-1 及两条附带建议**全部已修**，并已亲手复现「旧红场景现绿」「旧绿场景仍绿」。
返工仅触及 `md.rs` / `cache.rs`（`styles.rs` 逐字节未动）。未发现 P0/P1 级新问题；余下为 3 条 P2（均沿用或轻度覆盖不足，不阻断 U2）。

---

## 一、测量方法说明（重要）

任务建议的字节级证据命令**在本仓不可用**：

```
$ git diff 9e0b492 --stat
 Cargo.lock … app.rs … draw.rs … lib.rs … run.rs … docs/…    # 无 md.rs / cache.rs
$ git ls-tree 9e0b492 crates/ys-tui-coding/src/ | grep -E "md.rs|cache.rs|styles.rs"
NOT in baseline commit
```

`md.rs` / `cache.rs` / `styles.rs` 在基线 commit **不存在**，现为**未跟踪新增**（`??`）——
`git diff 9e0b492 -- <这两文件>` 恒为空，**不是**「本单元没碰」。因此改用**内容哈希**对比第 1 轮：

| 文件 | 第 1 轮哈希 | 现哈希 | 判定 |
|---|---|---|---|
| `md.rs` | `5db2d697…` | `7b680a0c…` | **已改**（返工） |
| `cache.rs` | `a2dd3f65…` | `108e275b…` | **已改**（返工） |
| `styles.rs` | `12d4589b…` | `12d4589b…` | **未动** |

⇒ 返工范围 = `md.rs` + `cache.rs`，与实现者自述一致。
（`git status` 中 `app.rs` / `draw.rs` / `run.rs` / `lib.rs` 的 M 属**并发单元**的接线，非本单元。）

---

## 二、第 1 轮 P0-1 及两条附带建议：逐条裁定

### P0-1（窄屏守卫把长正文当越界）——**已修** ✅

- **代码**：`md.rs:72-77`
  ```rust
  if lines.iter().any(|line| is_table_line(line) && line.width() > width) {
      return raw_plain_lines(src);
  }
  ```
  `is_table_line`（`md.rs:22-26`）只认框线字符 `│┌┬┐├┼┤└┴┘`（不含 `─`）。守卫从「任一行」收敛为「**表格行**」。
- **证据（独立复现，非采信自述）**：在 `/tmp/ysu1r2` 副本内置 `mod review_repro`，
  一段 166+ 字形的正文 + `# Title` + `**bold**` + 列表，在 width = 80 / 120 / 200 下：

  ```
  test md::review_repro::repro_long_prose_keeps_styles_all_widths ... ok
  ```
  断言（三宽度全过）：标题行 `style.fg == Some(Magenta)`、标题不含字面 `#`、`bold` span 仍有
  `Modifier::BOLD`、产物不含字面 `**`。**这正是第 1 轮 width=80/120 会整条退 raw 的场景，现保住 Markdown 样式。**

### 附带建议 ①（raw 回退不预折行）——**已修** ✅

- `raw_plain_lines`（`md.rs:87-91`）已去掉 `width` 形参，改为 `src.split('\n')` 原样切行：
  ```rust
  fn raw_plain_lines(src: &str) -> Vec<Line<'static>> {
      src.split('\n').map(|logical| Line::from(logical.to_owned())).collect()
  }
  ```
- 证据：`md::tests::narrow_table_falls_back_to_raw` 断言 `lines.len() == src.split('\n').count()`；
  新增 `md::tests::long_plain_line_is_not_prewrapped` 断言 `"z"×100` @ width 10 是**一条**未折行行。
  消费方 `draw.rs:127-128` 仍用 `Paragraph::wrap(Wrap{trim:false})` 折正文 —— 职责边界在
  `md.rs:81-86` 与 `draw.rs:431` 注释中已写明，无「双重折行 / 不折行」错误。

### 附带建议 ②（256 KiB 上界只挡命中不挡存储）——**已修** ✅

- `cache.rs:125-131`：oversized 分支渲染进 `scratch` 并**提前 return，不写 slot**：
  ```rust
  if oversized {
      self.scratch = md::render(src, width, styles);
      self.streaming = Some(idx);
      return &self.scratch;
  }
  ```
- `cache.rs:267` 起 `oversized_src_is_not_cached` 新增 `slot_is_none(0)` 断言（**真的没进缓存**），
  非仅「不命中」。`slot_is_none` 为 `#[cfg(test)]`（`cache.rs:157-160`），生产构建中不存在，无副作用路径。

---

## 三、P0 / P1 / P2（新问题）

### P0
无。

### P1
无。

### P2

- **P2-1（沿用第 1 轮）N6 用例仍近乎恒真。** `md::tests::n6_non_table_has_no_vertical_border`
  渲染一封无表文档后断言无 `│`，而库**唯一**产 `│` 处是 `renderer/table.rs`，该断言不可被合理变异击穿。
  真 N6（Chat 内区集成 / 拷贝友好）属 U2。非本单元缺陷。
- **P2-2（覆盖偏薄）M6 仅被**一条**用例捕获。** 见 §四。反向用例存在（`narrow_width_long_prose_keeps_markdown_styles`），
  但 `long_plain_line_is_not_prewrapped` **不能**捕获 M6（下详）——「不预折行」方向只有单点保护。
- **P2-3（设计已接受，非缺陷）** 含**字面框线字符** `│`(U+2502) 的正文、且该行 > 窗宽 ⇒ **整条退 raw**。
  已实测（`repro_literal_vertical_bar_prose_misclassification`）：
  ```
  [repro_box_prose] joined="# Title\n\nfoo │ bar …" ; heading fg = Some(None)   # 退 raw
  [repro_ascii_pipe] heading magenta = Some(Some(Magenta))                       # ASCII | 不触发
  ```
  设计 §2.6 明确接受此口径（代价极低）。**如实记录：它不是「没被发现」，而是被有意接受。**
  另：`md.rs:515` 注释「raw 行是源文本（ASCII `|`），不含框线字符」在源含框线字符时不成立，
  属注释口径偏窄（无功能影响）。

（第 1 轮 P2-1 `record_render_call()` 置于生产函数体、P2-2 `/new` 不重置 `RenderCache` —— 均未变，
后者属 U2 接线范畴。第 1 轮 P1-2「`finish` 无调用者」**已被并发 U2 接线解决**：`app.rs:302` 在
`finish_turn()` 中调用，`ys-tui-coding` clippy 现已**零 warning**，无 `dead_code`。）

---

## 四、变异 M1 / M6（在 `/tmp/ysu1r2` 副本）

| 变异 | 注入 | 变红的**仓库**用例（原文） | 与自述 |
|---|---|---|---|
| **M1** | `md.rs` 守卫改恒 false（`if false && …`） | **2 红**：<br>`md::tests::n5_table_never_exceeds_width` — `表格行在 width=1 出现超宽：┌───┬───┐`（md.rs:528）<br>`md::tests::narrow_table_falls_back_to_raw` — `退 raw 后不应含框线字符：┌───┬───┬───┐`（md.rs:620） | **比自述更强**（自述只列 `narrow_table_falls_back_to_raw`；实际 `n5` 也红） |
| **M6** | 判定放宽回「任意行」（去掉 `is_table_line`） | **1 红**：`md::tests::narrow_width_long_prose_keeps_markdown_styles` — `窄窗下标题行应保留行级粗体`（md.rs:591） | **一致**（确为 `narrow_width_long_prose_keeps_markdown_styles`） |

- 两次注入后另各有我方 `mod review_repro` 用例变红，属探针、不计入仓库用例。
- M6 补充观察：`long_plain_line_is_not_prewrapped` 在 M6 下**不红**（「任意行」守卫把 `z×100` 也退 raw，
  而 raw 也是 1 行 ⇒ 断言仍成立）—— 即该用例**不能**反向捕获 M6，唯一捕获点是 `narrow_width_long_prose…`。
  这是 §三 P2-2 的依据。

---

## 五、对抗性检查（deep）

- **表格行能否绕过 `is_table_line`？——未找到绕过路径。**
  读库 `renderer/table.rs` 证实：每条边框行 = 单 span（`Line::from(Span::styled(border,…))`，含 `┌┬┐├┼┤└┴┘`）,
  每个数据行 = 以独立 `Span::styled("│", …)` 包裹（`:442-447`）⇒ **每条渲染出的表格行都含框线字符**。
  独立于 `is_table_line` 的判据（直接扫 `─│┌┬┐├┼┤└┴┘`，含 `─`）在 9 种表（普通 / 单列 / 多列 / 对齐 /
  CJK / emoji / 列表内嵌 / 引用内嵌 / 单元格内含框线） × width 1..=60 上**零漏网**
  （`repro_no_overwide_box_line_bypasses_guard ... ok`，跳过 raw 产出以避开假阳性）。
- **`scratch` 会否返回上一次遗留？——不会。** 非 oversized 路径返回的是 `self.slots[idx]…lines`
  （`cache.rs:150-154`），唯一读 `scratch` 的分支先 `self.scratch = md::render(...)` 覆写再返回。
  行为实测：oversized 后以非 oversized 内容取同 idx ⇒ 走 slot、不含遗留内容
  （`repro_no_stale_scratch_after_oversized ... ok`）。
- **`slot_is_none` 生产副作用？——无。** `#[cfg(test)]`（`cache.rs:157-160`），release 不编译。
- **`n3_no_default_palette_leak` 的非空性断言是否真防 vacuous？——真防。**
  23 个样式点**逐条**有显式触发 `assert!`（`md.rs:340-477`）。实测：把 `styles.rs` 的 `alert_icon`
  还原为库默认 emoji（emoji **无颜色信号**，泄漏扫描抓不到）⇒ 用例仍**红**：
  ```
  md::tests::n3_no_default_palette_leak — alert Note 未触发/未着色，或 alert_icon 未置空 (md.rs:439)
  ```
  ⇒ 非空性断言确实在起作用，`n3` 非 vacuous。
- **`raw_plain_lines` 后果（`Paragraph` 仍折非常长单行）：职责边界已在注释写明**（`md.rs:81-86`，
  `draw.rs:431`），无双重折行 / 不折行错误（见 §二 附带建议①）。
- **width=0 是否 panic？** 不 panic（`repro_zero_width_does_not_panic ... ok`，表格溢出→raw，空串→0 行）。

---

## 六、测试合同 N5 / N3 / N7 逐条

| 合同 | 判定 | 证据 |
|---|---|---|
| **N5**（表格窄屏守卫 + **反面：长段正文保住样式**） | **真被覆盖（强）** | 正面：`n5_table_never_exceeds_width`（width `1..=80` × 3 源，`is_table_line` 行 ≤ width；M1 可击穿）＋`narrow_table_falls_back_to_raw`（退 raw 的完整断言）。**反面**：`narrow_width_long_prose_keeps_markdown_styles`（窄窗下标题/粗体保住、无字面 `**`；M6 唯一击穿点）。我方复现 width 80/120/200 亦全绿。 |
| **N3**（元素覆盖与样式映射） | **真被覆盖（强，返工后加强）** | `n3_no_default_palette_leak` 现覆盖**全部 23 个 `StyleSheet` 方法**（逐条触发断言 + 专属信号扫描）；对 default≠本仓的每个方法，去掉覆写即红（M3 已证）。**非空性经注入验证有效**（见 §五）。另 `n3_heading_levels…` / `n3_bold_italic…` / `n3_blockquote_list…` / `styles::overrides_do_not_match_default_palette`。 |
| **N7**（流式节流 ≤11 次 + 条末完整） | **真被覆盖（强）** | `cache::tests::n7_streaming_throttle_bounds_calls_and_finishes_complete`（1000 delta：in-loop ≤10、total ≤11、`flat_len==1000`）；M5（摘除节流）可击穿。缺口沿用：输入恒走 raw 路径，未在「含表格的增长」上验（非本单元问题）。 |

---

## 七、命令原文

- `cargo test -p ys-tui-coding` — **242 passed / 0 failed / 1 ignored** ✅（命令期间另有并发单元改
  `draw.rs`/`app.rs`/`run.rs`，曾一度捕获 `run.rs:688` unclosed delimiter 的**中间态**编译失败；
  等待其落定后重跑即绿，与 `md.rs`/`cache.rs` 无关，已如实标注）。
- `cargo clippy --all-targets -p ys-tui-coding` — `ys-tui-coding` **零 warning** ✅
  （唯一 warning 来自既有 `ys-protocol/lifecycle.rs:13` 的 `derivable_impls`，与本单元无关）。
  第 1 轮的 8 条 `dead_code` 已随并发 U2 接线消失。
- `git diff 9e0b492 --stat` — 见 §一：**不能**用于隔离本单元（新文件未跟踪）。

## 八、洁净性

- 本审查只读，唯一写操作是本报告与 `/tmp/ysu1r2` 副本（含 `mod review_repro` 与临时变异）。
- 仓库 U1 文件哈希与本轮开始时一致：`md.rs 7b680a0c…`、`cache.rs 108e275b…`、`styles.rs 12d4589b…`；
  `git status --short crates/ys-tui-coding/src/` 与开始时相同（仅并发单元的 app/draw/lib/run 为 M）。
- `md.rs:748` 等临时探针与变异**只在 `/tmp` 副本**，仓库未落。
