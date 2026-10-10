# TUI Assistant Markdown 渲染 — 验收报告（Phase 3 / U4）

- 日期：2026-10-10
- 基线 ref：`9e0b492faa1ce65108d257269904364db9dfe10d`（`chore(sw): baseline before 2026-10-10-tui-markdown-render`）
- 工具链：Rust / cargo 1.99.0（`stable-aarch64-apple-darwin`），平台 macOS（darwin 27.0.0）
- 验收范围：U1（`md.rs` / `styles.rs` / `cache.rs` + `Cargo.toml` + `lib.rs`）、U2（`draw.rs` / `app.rs`）、U3（`run.rs`）
- 本报告只做「验证 + 记录」；未修改任何业务代码（`crates/` 下 `.rs` 未动）。
- 基线对比一律用 **`git worktree add /tmp/ys-clippy-base <基线>`** 得到干净副本（`git stash` 全程未用）。
  同一副本同时用于 clippy 与 release 体积基线；用完已 `git worktree remove`。

---

## 1. 全量回归（N2）

**命令**：`cargo test --workspace`（主工作区）

**原始输出（聚合）**：

```
exit=0
passed=1005 failed=0 ignored=8
（test result 行数：45）
grep -E 'FAILED|panicked' → 无输出
```

各 target 均为 `test result: ok. … 0 failed …`，无任一 `FAILED` / `panicked`。

**基线复测**（在 `/tmp/ys-clippy-base` 内同命令）：

```
exit=0
BASE passed=975 failed=0 ignored=7
```

**结论**：**通过**。当前 **1005 passed / 0 failed / 8 ignored**，基线 **975 / 0 / 7**。
差值 **+30 passed、+1 ignored**（与「975 不是简单 +28」的预期一致，逐项来源见 §2）。

---

## 2. 测试数差值表（基线 → 现在）

| 项 | 基线 | 现在 | 差值 |
|---|---|---|---|
| workspace passed | 975 | 1005 | **+30** |
| workspace ignored | 7 | 8 | **+1** |
| `ys-tui-coding` 全 crate passed | 212 | 242 | **+30** |
| `ys-tui-coding` lib 用例数（`--list`） | 212 tests | 242 tests | **+30** |

**全部 +30 个新增 passed 都落在 `ys-tui-coding` lib**（其它 crate 用例数不变）。逐项来源：

| 模块（新/改） | 新增用例数 | 用例名 |
|---|---|---|
| `cache.rs`（**新文件**） | 9 | `different_idx_are_independent_slots`、`explicit_finish_forces_rerender`、`len_change_forces_rerender_after_window`、`n7_streaming_throttle_bounds_calls_and_finishes_complete`、`oversized_src_is_not_cached`、`pure_memo_rerender_is_identical`、`same_key_within_window_reuses`、`sync_len_growth_finishes_previous_line`、`width_change_forces_rerender` |
| `md.rs`（**新文件**） | 11 | `edge_inputs_do_not_panic_and_table_stays_within_width`、`empty_input_yields_no_overwide_lines`、`long_plain_line_is_not_prewrapped`、`n3_blockquote_list_fenced_code_table`、`n3_bold_italic_inline_code_link`、`n3_heading_levels_have_explicit_style_and_no_marker`、`n3_no_default_palette_leak`、`n5_table_never_exceeds_width`、`n6_non_table_has_no_vertical_border`、`narrow_table_falls_back_to_raw`、`narrow_width_long_prose_keeps_markdown_styles` |
| `styles.rs`（**新文件**） | 2 | `chosen_shapes_are_as_designed`、`overrides_do_not_match_default_palette` |
| `draw.rs`（改） | 6 | `geometry_follow_keeps_last_line_and_scroll_matches_line_count`、`geometry_top_border_inner_is_h_minus_1_by_w`、`n4_line_count_equals_per_line_accumulation`、`n6_chat_inner_has_no_vertical_border_for_non_table_md`、`n8_plain_lines_contain_raw_markdown`、`throttle_consistency_scroll_and_draw_share_product` |
| `run.rs`（改） | 2 | `panic_hook_body_restores_then_delegates`、`restore_sequence_contains_terminal_state_restore` |
| **合计** | **+30** | |

其它变更（净计数为 0，但已核对）：

- **改名（1 处，净 0）**：`draw::tests::test_plain_lines_mirror_transcript_and_wrap`
  → `draw::tests::test_plain_lines_mirror_transcript_and_assistant_raw`。
- **改名后仍存在、非新增**：`draw::tests::test_scroll_uses_wrapped_rows_not_transcript_entries`
  在基线与现在**同名同在**（属「就地改写」，不产生计数差）。
- **任务描述里提到、但基线中并不存在**（故对本 delta 无净影响）：
  - `long_single_line_falls_back_to_raw_and_wraps`：基线与现在**均无此名**（其功能被替换为
    `md::tests::long_plain_line_is_not_prewrapped`；该名只在 U1 开发过程中短暂存在，未进基线）。
  - `edge_inputs_do_not_panic_and_stay_within_width`：基线无；现在名为
    `edge_inputs_do_not_panic_and_table_stays_within_width`（作为 `md::tests` 新用例计入 +1）。
  - `n5_table_never_exceeds_width`：基线无；现在为 `md::tests` 新用例（计入 +1）。
  - 说明：因为这些用例所属的 `md.rs` **整个文件**在基线尚不存在，任务口径里的「重写 / 删除」
    相对基线只体现为「新增」，这是 +30 与预估 28 的差异来源之一。

**+1 ignored 的来源**（实测定位，非猜测）：

```
Doc-tests ys_tui_coding
running 1 test
test crates/ys-tui-coding/src/cache.rs - cache::RenderCache::lines_for (line 97) ... ignored
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
```

基线该 target 为 `running 0 tests` ⇒ 新增的 1 个 ignored 是 `cache.rs` 中
`RenderCache::lines_for` 的 `ignore` 文档测试（**在 `cache.rs`，不在 `md.rs`**）。

**与任务预估的差异解释**：任务预估「U1 20 + U2 6 + U3 2 = 28」。实测 U1 覆盖的三个新文件合计
**22** 个（`md` 11 + `cache` 9 + `styles` 2）—— 预估的 20 少算了 `styles.rs` 的 2 个；加 `draw` 6、`run` 2
= **30**。`+1 ignored` 为新增的 `cache.rs` 文档测试。差值已全部对上，非「正常波动」。

---

## 3. Lint（N10 前置）

**命令（主工作区）**：`cargo clippy --all-targets`（全 workspace）
**命令（基线）**：在 `/tmp/ys-clippy-base` 内同命令，另按任务要求跑
`cargo clippy --all-targets -p ys-protocol 2>&1 | tail -5`。

**ys-protocol 专项（基线副本）原始输出 tail**：

```
warning: `ys-protocol` (lib) generated 1 warning (run `cargo clippy --fix --lib -p ys-protocol -- ` to apply 1 suggestion)
warning: `ys-protocol` (lib test) generated 1 warning (1 duplicate)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.99s
```

该 1 条即 `derivable_impls`：`crates/ys-protocol/src/lifecycle.rs:13:1`
（`impl Default for LifecyclePolicy` → 可 `#[derive(Default)]`）。

**两侧 warning 清单**（全 workspace，去重后的 lint 类型）：

| lint 类型 | 主工作区 | 基线副本 |
|---|---|---|
| `unnecessary use of \`clone\` to create a slice from a reference` | 15 | 15 |
| `this \`if\` statement can be collapsed` | 15 | 15 |
| `this function has too many arguments (9/7)` | 2 | 2 |
| `this \`impl\` can be derived` | 1 | 1 |
| `consider using \`sort_by_key\`` | 1 | 1 |
| `called \`filter(..).next_back()\` on an \`DoubleEndedIterator\`` | 1 | 1 |
| **warning 行合计** | **49** | **49** |

**逐站点比对**：

```
base sites: 35, main sites: 35   （去重后的 `-->` 位置集合）
diff base -> main → NO DIFF IN WARNING SITES   （IDENTICAL）
```

即两侧的 warning **位置集合完全一致**（`-->` 指向的文件:行:列无差异）。`cargo clippy` 退出码 0，
无 `error`。**无一新增 warning**。

**结论**：**通过**。`ys-protocol` 的 `this impl can be derived`（`lifecycle.rs:13`）在**基线与现在都存在**
（已在基线副本实测复核），确认为**既有** warning，未被本轮触碰。

> 注：clippy 命中 `crates/ys-tui-coding` 新代码（`Checking ys-tui-coding …`）后，
> **未产生任何新 warning**（该 crate 在两侧清单中均未出现新的 lint 站点）。

---

## 4. release 体积（N10）

**做法**（先在干净基线量、再在主工作区量；两次 `target` 目录不同、互不污染）：

```
# 基线（worktree /tmp/ys-clippy-base @ 9e0b492）
cargo build --release -p ys-coding-agent   → Finished `release` … (exit 0)
stat -f%z /tmp/ys-clippy-base/target/release/ys-coding-agent

# 主工作区
cargo build --release -p ys-coding-agent   → Finished `release` profile [optimized] target(s) in 19.02s
stat -f%z /Users/zs/Develop/YuShan/target/release/ys-coding-agent
```

**原始数字**：

| 侧 | 字节数 | 约 |
|---|---|---|
| 基线（9e0b492） | **7,747,024** | 7.39 MiB |
| 现在 | **8,200,144** | 7.82 MiB |
| **差值** | **+453,120** | **+442.5 KiB（≈ +453 KB 十进制）** |

**判定**：`453,120 B < 500 KiB (512,000 B)`（亦 < 500,000 B）⇒ **通过**（< +500KB）。

**差值主要来源**（区分：本轮**未引语法高亮**）：

- 唯一 crate 依赖增量：`tui-markdown 0.3.10`（`default-features = false`）+ 其传递依赖
  （`pulldown-cmark` / `itertools` / `tracing` 等）；以及 ratatui 0.30 的子 crate
  （`ratatui-core` / `ratatui-widgets` / `ratatui-crossterm`）。
- 独立包数（`cargo tree -p ys-coding-agent --prefix none` 去重）：**基线 194 → 现在 201（+7）**。
- `cargo-bloat` **未安装**，无法给出符号级归因；以 `cargo tree` 的包数增量作为来源证据。

**⚠️ 与历史值不符（如实记录）**：任务给出的「历史 ~5.7 MB」在本机**无法复现** —— 基线与现在实测
都在 7.7–8.2 MB 区间（均远高于 5.7 MB）。工作区 `Cargo.toml` **没有 `[profile]` 段**（即 cargo 默认
release profile，`debug=false`）。历史 5.7 MB 很可能来自**不同的工具链 / 构建配置或测量口径**，
本轮两测同工具链、同配置，故**只以「两侧差值 < 500KB」作为判据**（该项通过）。

---

## 5. 越界断言（R6 的可检验形式）

**命令与原文**：

```
$ grep -rn 'tui_markdown::' crates/ys-tui-coding/src/
crates/ys-tui-coding/src/md.rs:8://! `tui_markdown::` 路径只准出现在 `md.rs` / `styles.rs`（R6 缓解）。
crates/ys-tui-coding/src/md.rs:11:use tui_markdown::Options;
crates/ys-tui-coding/src/md.rs:44:    let text = tui_markdown::from_str_with_options(
crates/ys-tui-coding/src/styles.rs:6://! `tui_markdown::StyleSheet` 是**编译期 trait、不是主题系统** —— 无配置、无持久化、
crates/ys-tui-coding/src/styles.rs:14://! `tui_markdown::` 路径只准出现在 `md.rs` / `styles.rs`（R6 缓解）。
crates/ys-tui-coding/src/styles.rs:17:use tui_markdown::{AlertKind, StyleSheet};
```

**宽口径复核**（`grep -rn 'tui_markdown' crates/ys-tui-coding --include='*.rs'`，去掉 `::` 与文档注释后）：

```
crates/ys-tui-coding/src/md.rs      : 唯一调用点（use / from_str_with_options）
crates/ys-tui-coding/src/styles.rs  : StyleSheet / AlertKind
（其余：apps/ 及其它 crate 的 .rs 无命中）
```

代码级（非注释）命中**只有 `md.rs` 与 `styles.rs`**，与 R6 约束一致。

**依赖形态**（`grep -n 'tui_markdown\|tui-markdown' crates/ys-tui-coding/Cargo.toml` 原文）：

```
crates/ys-tui-coding/Cargo.toml:30:tui-markdown = { version = "0.3.10", default-features = false }
```

确认为 `default-features = false`（关掉 `highlight-code`，不进语法高亮栈）。

**结论**：**通过**。

---

## 6. 依赖树关键断言

**`ratatui-core` 在 `Cargo.lock` 里只有 1 条**：

```
$ grep -n 'name = "ratatui-core"' Cargo.lock
1249:name = "ratatui-core"
```

`cargo tree -p ys-coding-agent --prefix none` 中 `ratatui-core` 唯一版本：

```
ratatui-core v0.1.2
```

（`ratatui` facade 为 `ratatui v0.30.0`，其子 crate `ratatui-core/-widgets/-crossterm v0.1.2/0.3.2`
经同一 `ratatui-core v0.1.2` 复用，`(*)` 表示去重 ⇒ 全树单版本，`Text`/`Style` 类型可互传。）

**`syntect` / `ansi-to-tui` / `onig` 均不出现**：

```
$ grep -niE 'syntect|ansi-to-tui|onig' Cargo.lock
NONE FOUND

$ cargo tree -p ys-coding-agent | grep -iE 'syntect|ansi-to-tui|onig'
NONE IN TREE
```

**结论**：**通过**（`ratatui-core` 单条；三个重型/C 库依赖全不在依赖树）。

> 说明：`cargo tree -d` 显示的重复版本（`core-foundation`、`fastrand` 等）均为**既有**的
> `reqwest`/HTML 解析链重复，与本轮无关。

---

## 7. MANUAL_ACK_REQUIRED（须由用户勾选，原文保留）

- [ ] 在真实终端喂一条 ≥100 KiB 的长回答（含表格），确认滚动与输入无可感卡顿（N11）
- [ ] 确认 Chat 窗格去掉左右边框后的观感、以及表格极窄时退回 raw 的画面可接受

---

## 8. 「未能机器验证、留给人工」清单

以下各项**本轮未做机器验证**（未跑一律标注「未跑」/「未验证」，不作猜测）：

1. **panic hook 的真终端恢复路径本身**：单测只验证了「恢复动作先于 delegate 发生」的**顺序**
   （`run::tests::panic_hook_body_restores_then_delegates`）与 `restore_sequence()` 的**字节内容**
   （`restore_sequence_contains_terminal_state_restore`）。未在真实 TTY 上触发一次绘制 panic，
   以确认 `disable_raw_mode()`（系统调用）与转义序列**实际**把终端复位。属**未验证**。
2. **N11（≥100 KiB 长回答的流式卡顿）**：需真实终端 + 真实模型输出，属 MANUAL_ACK，**未跑**。
3. **观感项**：Chat 窗格去左右边框、表格极窄退 raw 的画面 —— MANUAL_ACK，**未跑**。
4. **库对「4 空格缩进式代码块合并多行」的已知缺陷（R9）**：现有断言只钉住**围栏式**代码块正常；
   缩进式是否在真实模型输出中出现、合并行是否可感 —— **未在真实输出中观察**。
5. **半截 fence 的视觉跳动（R4）**：属观感，**未人工确认**。
6. **resize 全量重算（R5）**：库不可分，resize 那次帧是否可感卡顿 —— **未在真实终端测量**。
7. **release 体积历史值 ~5.7 MB**：本机两测均 ~7.7–8.2 MB，**无法复现**该历史数字
   （原因未知，疑为历史工具链/配置差异）；仅「两侧差值 <500KB」通过。
8. **`cargo-bloat` 符号级体积归因**：工具**未安装**，**未跑**；来源判断仅基于 `cargo tree` 包数增量。

---

## 9. 四项结论速览

| # | 项目 | 命令 | 结论 | 关键数字 |
|---|---|---|---|---|
| 1 | 全量回归（N2） | `cargo test --workspace` | **通过** | 1005 passed / 0 failed / 8 ignored（基线 975/0/7，+30/+1） |
| 2 | Lint（N10 前置） | `cargo clippy --all-targets`（+基线副本对比） | **通过** | 两侧 warning 站点集合 IDENTICAL；无新增 |
| 3 | release 体积（N10） | `cargo build --release -p ys-coding-agent`×2 | **通过** | +453,120 B（≈+442.5 KiB）< 500 KiB |
| 4 | 越界断言（R6） | `grep -rn 'tui_markdown::' crates/ys-tui-coding/src/` | **通过** | 仅命中 `md.rs` / `styles.rs`；依赖 `default-features = false` |

附加：依赖树断言通过（`ratatui-core` 单条；`syntect`/`ansi-to-tui`/`onig` 全不出现）。
