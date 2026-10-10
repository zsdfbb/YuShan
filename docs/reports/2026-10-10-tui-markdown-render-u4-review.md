# U4（Task 7）验收报告 — Shallow Review

- 日期：2026-10-10
- 被审报告：`docs/reports/2026-10-10-tui-markdown-render-verify.md`
- 审查档位：shallow（**只报 P0 / P1**）
- 审查方式：独立重跑 + 独立计数，不采信被审报告结论

---

## 结论

**无 P0，无 P1。被审报告属实** —— 其声称的四项验证与全部关键数字均能在本次独立重跑中复现，
未发现任何「说了没做」或数字面失真。

---

## 复核记录（本次实测原文）

### 1. `cargo test --workspace`（重跑项 1）

```
exit=0
test result 行数：45
passed=1005  failed=0  ignored=8
grep -E 'FAILED|panicked' → 无
```

与被审报告 §1 声称的 **1005 / 0 / 8（45 行）** 逐项一致。

**归因抽查（用基线 worktree `git worktree add /tmp/ys-u4-base 9e0b492` 独立计数）**：

| 模块 | 基线（lib `--list`） | 现在 | 差值 | 报告声称 |
|---|---|---|---|---|
| `cache::` | 0 | 9 | +9 | +9 ✓ |
| `md::` | 0 | 11 | +11 | +11 ✓ |
| `styles::` | 0 | 2 | +2 | +2 ✓ |
| `draw::` | 35 | 41 | +6 | +6 ✓ |
| `run::` | 6 | 8 | +2 | +2 ✓ |
| ys-tui-coding 合计 | **212** | **242** | **+30** | +30 ✓ |

- 其余模块（app/commands/completion/events/format/input/prompter/transcript/view）两测完全不变，确认 +30 全部落在上述五处。
- 报告点名的 `styles.rs` **+2** 确实存在，用例名实测为
  `styles::tests::chosen_shapes_are_as_designed` 与 `styles::tests::overrides_do_not_match_default_palette`。
- `+1 ignored` 实测定位：

```
running 1 test
test crates/ys-tui-coding/src/cache.rs - cache::RenderCache::lines_for (line 97) ... ignored
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
```

  与报告一致：是新增的 `cache.rs` 文档测试（在 `cache.rs`，非 `md.rs`）。

### 2. 越界断言（重跑项 4）

```
$ grep -rn 'tui_markdown::' crates/ys-tui-coding/src/
crates/ys-tui-coding/src/md.rs:8://! …（文档注释）
crates/ys-tui-coding/src/md.rs:11:use tui_markdown::Options;
crates/ys-tui-coding/src/md.rs:44:    let text = tui_markdown::from_str_with_options(
crates/ys-tui-coding/src/styles.rs:6://! …（文档注释）
crates/ys-tui-coding/src/styles.rs:14://! …（文档注释）
crates/ys-tui-coding/src/styles.rs:17:use tui_markdown::{AlertKind, StyleSheet};

$ grep -n 'tui-markdown' crates/ys-tui-coding/Cargo.toml
30:tui-markdown = { version = "0.3.10", default-features = false }
```

去注释后的**代码级**命中只有 `md.rs` / `styles.rs`，与报告 §5 一致。

### 3. Lint（重跑项 2，部分）

- `cargo clippy --all-targets -p ys-tui-coding`：`ys-tui-coding` **自身 0 warning**
  （全 workspace clippy 输出中，`-->` 站点集合亦无一条指向 `crates/ys-tui-coding`）。
- 唯一一条 warning 来自 **`ys-protocol`**：`crates/ys-protocol/src/lifecycle.rs:13:1`
  `this impl can be derived`（`impl Default for LifecyclePolicy`）。该 crate 是本地 path 依赖，clippy 一并 lint。
  实测确认它与本变更无关（TUI / markdown 无涉），为既有 warning。
- 全 workspace 站点集合比对（本次自行重建基线 worktree 跑 `cargo clippy --all-targets`）：

```
base sites: 35, main sites: 35
diff base -> main → IDENTICAL（无 diff）
两侧 lint 类型分布均：clone→slice 15 / collapsible_if 15 / too_many_arguments 2 / derivable 1 / sort_by_key 1 / filter_next 1
```

  报告中「主工作区 49 / 基线 49」一栏经核对为**全部 `^warning:` 行（含 14 条 `generated N warnings` 汇总行）**之计数
  （35 个站点 + 14 汇总 = 49，两侧皆 49），与「站点 35」不矛盾，属不同口径，均真实。

### 4. release 体积（重跑项 3 —— 按要求**未重建**，仅逻辑核对）

报告推理链核对：

- **两测 target 目录确为独立**：本次确认 `CARGO_TARGET_DIR` 为空（默认 `<workdir>/target`），
  基线用的是 `/tmp/ys-clippy-base` worktree（自带独立 `target/`）。报告工作区 `Cargo.toml` **无 `[profile]` 段**
  （实测 `grep profile Cargo.toml` 无命中）⇒ 两测同为 cargo 默认 release profile，可比。
- **两侧量的是同一可执行文件**：命令均为 `cargo build --release -p ys-coding-agent` → 同一 bin 名。
- **「现在」侧可独立复现**：`stat -f%z target/release/ys-coding-agent` = **8,200,144**，与报告完全一致。
- **差值算法为「主 − 基线」**：8,200,144 − 7,747,024 = **453,120** = 442.5 KiB，确 < 500 KiB（且 < 500,000 B）。
- **基线侧 7,747,024 无法在本次复核中重测**（报告用过的 worktree 已 `remove`，且执行模式禁止再跑 release 构建），
  但报告在其 §4/§8.7 已如实标注历史 ~5.7 MB 不可复现、仅以「两侧差值 < 500 KiB」为判据，推理链无断点。
- 佐证：独立包数（`cargo tree -p ys-coding-agent --prefix none | sed 's/ (\*)$//' | sort -u | wc -l`）
  实测 **基线 194 → 现在 201（+7）**，与报告相符。

---

## 观察（非 P0/P1，不计入结论）

无。本次未发现任何需要定级为 P0 / P1 的缺陷。
