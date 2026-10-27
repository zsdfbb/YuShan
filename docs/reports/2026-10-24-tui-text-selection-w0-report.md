# W0 执行报告 — `layout.rs` 字形缓存地基

> 对应 `docs/exec-plans/completed/2026-10-24-tui-text-selection.md` 的 **T1 + T2**（波次 W0）。
> 设计来源 `docs/arch/tui-text-selection/design.md` §2.2b / §2.3。
> 基线：`d265f14`。

## 1. 交付内容

`crates/ys-tui-coding/src/layout.rs` **纯追加** 639 行（5 行删除全部是 `window()` 平移进
`window_impl` 的行）。既有 `window()` / `sync()` / `reset()` 行为**逐字节不变**。

| 新增 | 作用 |
|---|---|
| `struct RowRef { line: u32, off: u32 }` | 「屏幕第 y 行来自窗口内第几个 `Line` 的第几个视觉行」 |
| `window_with_rows(top, height) -> (Window, Vec<RowRef>)` | 槽环重建单行所需的数据 |
| `window_impl(.., rows: Option<&mut Vec<RowRef>>)`（私有） | **唯一**定位逻辑；`window()` 是它的 `None` 路径 |
| `revision_range(top, height) -> u64` | 窗口版本键（O(窗口覆盖条目)，不遍历全表） |
| `structural_rev()` / `total_rows()` / `width()` | 只读取值器 |
| `EntryMemo.rev` | 条目产物**真的换了**才 +1 |
| `TranscriptLayout.structural_rev` / `rev_alloc` | 结构版本 / 分配版本（单调，只要求不回退） |
| `SyncResult.recomputed_entries` | 真正重算的条目数 —— 用于把「流式拖选限流」从推论变成闸门 |

### 共用遍历（承重纪律）

`layout.rs` 的模块文档已立「内区几何唯一来源」的先例；这里同型：

```rust
pub fn window(&self, top_row: usize, height: usize) -> Window {
    self.window_impl(top_row, height, None)          // 不构造 Vec<RowRef> ⇒ 稳态帧 0 分配
}
pub(crate) fn window_with_rows(&self, ..) -> (Window, Vec<RowRef>) {
    let mut rows = Vec::new();
    let window = self.window_impl(top_row, height, Some(&mut rows));
    (window, rows)
}
```

`RowRef` 在原有遍历的同一个 push 点旁产生，**没有第二份折行/定位计算**。
`row_starts` 的口径仍是 `Paragraph::line_count(width)` —— N4「整段折行 == 逐行折行之和」未变
（`test_total_rows_equals_whole_line_count` 保绿）。

## 2. Review 结论与修复

deep review（独立 subagent，跑测试 + `git diff d265f14` 取字节证据）判 **2 P0 + 2 P1**，
全部已修。两条 P0 均经 review 实测复现、我另行读码确认。

### P0-1 `revision_range` 语义漏检

**症状**：`app.rs` 的增量补偿在「非贴底 + total 增长」时执行 `scroll_offset += Δ`，使
`top_row` **数值不变**。若增长发生在窗口**上方**的条目里，`starts[]` 整体下移、
`window_impl` 里 `rel = top - starts[idx]` 改变 ⇒ 窗口内容已变，但窗口覆盖条目的 `rev`
都没变 ⇒ 键不变 ⇒ 字形缓存不刷新 ⇒ **旧字形配新内容**（用户可见）。

**修法**：把窗口顶在首条目内的偏移折进键 —— `mix(0, 13, (top - starts[first]) as u64)`，
条目 `rev` 用不同 tag（14）经同一混合器。选混合器而非旧式 `rotate_left ^ rev`：
后者会让「末条目 rev」原样成为返回值，条目数不同时可能撞键。

**用例** `test_revision_range_folds_window_top_inner_offset`：6 个等高块，窗口盖住第 3 块
剩余部分，只给**窗口上方**的条目加 1 行。三条前置断言把「唯一变量」钉死（上方真长高 /
覆盖条目 `rev` 不变 / 窗口内容确实换了），再断言键必变。**修复前变红已验证。**

### P0-2 `RowRef.off` 是 `u16`，会静默截断

**症状**：`off` 是**条目内**视觉行号，无 `u16` 上界。单条 200 KB 文本在 `width = 1` 下可折出
20 万视觉行（`MAX_CACHED_SRC` 256 KB 并不阻止 `row_starts` 到这个量级）。`as u16` 让
`off` 100000 → 34464，指向该 `Line` 里完全不相干的一行 —— 且 `line` 正确，**失败是静默的**。

**修法**：`off` 改为 `u32`（与 `line` 同宽），写入点用 `row_off()` 做 `try_into` +
`debug_assert`，发布构建饱和而非回绕。

**用例** `test_row_ref_off_keeps_visual_row_beyond_u16`：`width = 1` + 70,002 字符单条，
断言 `rows[0].off == 70000`。**修复前变红已验证**（`left: 4464, right: 70000`，
即 `70000 mod 65536`）。

### P1-1 测试强度：M7 / M8 变异存活

原用例只断言内部自洽性（`rows.len() <= h`、`line` 单调），**没有一个把 `rows` 与真实折行对上**。
实测两个变异全绿通过：M7「每 `Line` 只记 1 行」、M8「`off` 起点错」。

**修法**：① `<=` 改 `==`（`h.min(total - top)`，一行即堵住「少记行」整类 bug）；
② 语义对照 —— 把 `rows[y]` 解码成该 `Line` 的第 `off` 个视觉行，与**整段渲染**的第
`top + y` 行逐字节比对。实现时踩到的坑：参考路径必须与布局的 thinking 可见性显式对齐，
否则参考会按隐藏渲染而错位。

### P1-2 `revision_range` 边界：M9 / M12 变异存活

M12（`first = 0`）尤其值得修 —— 它把键退化成「几乎全量折叠」，正是设计明令禁止的退化。

**修法**：窗口取**中部**且恰好盖住一整块，于是「上方 / 下方 / 窗口内」三个方向都真实存在，
分别断言「不变 / 不变 / 必变」。

## 3. 变异自检（四个全部打红）

| 变异 | 注入点 | 打红的用例 | 失败断言摘录 |
|---|---|---|---|
| M7 每 Line 只记 1 行 | `window_impl` | `test_window_rows_invariants` | `left: 1, right: 3` |
| M8 `off` 起点错 | `window_impl` | `test_window_rows_invariants` | 第 4 行与整段渲染第 11 行不一致 |
| M9 `end = height + 1` | `revision_range` | `test_revision_range_only_covers_window` | 窗口下方条目更新 ⇒ 键不得变 |
| M12 `first = 0` | `revision_range` | `test_revision_range_only_covers_window` | 窗口上方条目更新 ⇒ 键不得变 |

## 4. 验证

```
./scripts/cargo-build.sh test -p ys-tui-coding layout
  test result: ok. 12 passed; 0 failed   （其中 5 条本轮新增/强化）

./scripts/par-test.sh
  par-test: 39 个测试二进制，并行度 8
  par-test: 全部通过

cargo clippy -p ys-tui-coding --all-targets   → 本 crate 0 warning
cargo fmt -p ys-tui-coding --check            → clean
```

## 5. 遗留

- `#[allow(dead_code)]` 6 处：这批 API 的真实消费方在 `selection.rs`（W1 起）。会在接线
  单元里逐条删除。review 建议改用 `#[cfg(test)]`，但那是「接线前」的临时态，改动成本
  大于收益，故保留 `allow` 并在注释里写明消费方。
- 后续波次：W1 `selection.rs` 骨架 → … → W11 文档回写（共 11 波未做）。
