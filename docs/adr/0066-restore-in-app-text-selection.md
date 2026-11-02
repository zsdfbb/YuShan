# 0066 — 恢复应用内拖选与右键复制：鼠标归应用，`?1002h` 不可省

`f39a122` + `7033140` 导出的第四条路（「只开滚轮、拖选归终端」）在实践中**不可用**：
终端在鼠标追踪开启后不保证提供原生拖选逃生门，且守卫这条取舍的测试本身把故障写成了
契约。本 ADR 决定**恢复** `84812af` / `042a116` 的形态：应用接管左键三态与右键，
终端只提供鼠标事件的原始上报。

## Status

accepted（2026-11-03）

取代 [ADR-0064 决策 4](0064-mouse-wheel-only.md)（「不建应用内选区」）。
[ADR-0051](0051-tui-text-selection.md) 的拖选决策**恢复有效**。
ADR-0064 的决策 1 被**修正**（见下「与 ADR-0064 的关系」）。

## 背景

### 失效链条（`git log` 实证）

| 时间 | 提交 | 结果 |
|---|---|---|
| 10-25 17:42 | `84812af` 应用内选区 | 拖选可用 |
| 11-01 23:24 | `0a2d8d9` 快速拖选被丢弃 | ✅ 修复 |
| 11-01 23:44 | `bdaf2c9` ⇧ 拖选不一致 | ✅ 修复 |
| 11-02 00:13 | `042a116` 拖选只标记、右键才复制 | 拖选可用 |
| **11-02 00:31** | **`f39a122` 删掉整个选区（2077 行 + 16 个用例）** | ❌ 拖选消失 |
| 11-02 11:10 | `7033140` 只接管滚轮（为修滚轮） | ❌ 仍未恢复拖选 |

`7033140` 的动机完全合理：`f39a122` 把鼠标交还终端后滚轮滚不动了。但它为了修滚轮，
**继续沿用了 `f39a122` 的「不建选区」决策**，未意识到这会让拖选整体消失。

### 为什么测试没拦住

这是本次最该记住的部分 —— **不是「测试没覆盖」，是「测试断言的正是故障」**。

`7033140` 新增了两处断言，它们把坏状态写成了正确契约：

```rust
assert_eq!(MOUSE_MODE_ON, "\x1b[?1000h\x1b[?1006h");                     // run.rs
assert!(!seq.contains("\x1b[?1002h") && !seq.contains("\x1b[?1003h"));  // 旧 setup_sequence_enables_wheel_only
```

`?1002h`（按下拖动上报）**正是拖选的命根子** —— 不开它终端根本不产生 `Drag` 事件。
于是：测试恒绿，回归被固化成预期行为，此后任何「按测试办事」的人都会继续删掉选区。

同时 `f39a122` 删掉的 16 个用例里包含 `drag_then_release_copies_text`、
`v6c_real_drag_freezes_view` 等 —— 它们正是这轮要恢复的行为的定义者。

### 更深的漏洞

四份变异脚本（`cancellation` / `wheel` / `selection` / `theme`）**没有任何自动化调用**，
全靠人记得敲；且锚点未命中时脚本静默跳过、退出码 0。事后核查发现
`cancellation-mutation.py` 的 [10] / [12] **早已失效**（缩进漂移、漏字段导致编译失败），
无人发现。已被本 ADR 一并修复（见「验证」）。

## 决策

### 1. 鼠标模式三条缺一不可

`run::MOUSE_MODE_ON` = `?1000h` + `?1002h` + `?1006h`。

| 序列 | 作用 | 缺了会怎样 |
|---|---|---|
| `?1000h` | 按下 / 松开 + 滚轮 | 收不到 `Down`，选区无法建立 |
| `?1002h` | **按住拖动时的移动上报** | **收不到 `Drag`，拖选停在按下点** |
| `?1006h` | SGR 编码（列号 + 按键号） | 只有 223 列上限，且分不清左右键 |

`?1003h`（任意移动上报）**仍不开**：它对拖选无用，却让指针一动就持续产生 `Moved`。
这条正是 ADR-0064 决策 1 想避免的，本 ADR **保留该目的**，但结论从「只开 1000」
修正为「开 1000 + 1002、不开 1003」—— 因为 `?1002h` 承载拖选，不是可选项。

### 2. 拖选三态 + 右键复制

- `Down(Left)` 建选区（仅在 Chat 内区；点在外区不开始）
- `Drag(Left)` 扩展选区；指针离开按下点那一刻才冻结视窗（单击不得冻结跟随）
- `Up(Left)` 只转为「已标记」，**不复制**
- `Down(Right)` 把已标记选区写入剪贴板并清高亮

复制上限 256 KB，超限**不截断**、只在 transcript 回一行提示。

### 3. 选区状态的清理点

清理是**契约**，多一处少一处都会出真 bug：

| 触发 | 为什么清 |
|---|---|
| `Event::Resize` | wrap 行数全变 ⇒ 全局视觉行号空间作废 |
| `PageUp` / `PageDown` / 滚轮 | 用户主动滚动 ⇒ 行号对不上屏幕内容 |
| `jump_to_end` / `jump_to_start` | 跳转改 `scroll_offset` |
| `reset_for_replay` | transcript 整体替换，行号空间作废 |
| 右键复制后 | 文字已进剪贴板，高亮无意义 |
| 两端点重合的 `Up` | 单击不是选区 |

**`drag_tick` 的自动滚动不得清选区** —— 否则拖到边缘即自我取消。

## 与既有 ADR 的关系

| ADR | 状态 | 说明 |
|---|---|---|
| [0050](0050-tui-mouse-wheel-scroll.md) | accepted | 「滚轮归应用」未变 |
| [0051](0051-tui-text-selection.md) | accepted（恢复有效） | 曾被 0064 决策 4 推翻，现恢复 |
| [0064 mouse-wheel-only](0064-mouse-wheel-only.md) | **superseded by 本 ADR** | 决策 4 被完全推翻；决策 1 被修正；决策 2/3（滚轮语义与合批）保留有效 |
| [0064 steer](0064-steer-does-not-interrupt.md) | accepted | 与本主题无关（编号重号，见下） |

### 编号重号

`docs/adr/` 里 `0064-mouse-wheel-only.md` 与 `0064-steer-does-not-interrupt.md`
**共用 0064 号**。本 ADR 占 0066，未重编既有文件（重命名会让既有文档的
`path:line` 指针失效）。`0064-mouse-wheel-only.md` 的文件名保持原样，靠 Status
区分是否仍有效。

## 风险

| 风险 | 处置 |
|---|---|
| 鼠标追踪期间终端原生拖选 / 右键菜单不生效 | 这是接管的固有代价；复制由应用提供（拖选 + 右键），另有 `/copy`（OSC 52） |
| 某些终端在 `?1002h` 下不上报 `Drag` | `?1000h` 仍在，`Down` / `Up` 可用；退化为「点一段」 Anchors 仍可复制单点行 |
| 选区高亮与 markdown 缓存的字形缓存漂移 | `selection.rs` 自带 `GlyphGrid` 重建键测试（8 处变异全覆盖） |
| 再次被「只开滚轮」的改动误删 | 见「验证」—— 锚点失效即失败 |

## 验证

`./scripts/verify.sh`（本 ADR 新增）是提交前唯一入口：全量测试 + 4 个变异脚本。
锚点失效、非预期失败一律非零退出。

实跑结果（2026-11-03）：

```
全量测试:              39 个测试二进制，全部通过
cancellation-mutation: 12/12 被抓到（修复了此前失效的 [10] / [12]）
wheel-mutation:        11/11 被抓到（M10/M11 锚点已随常量改名）
selection-mutation:     8/8  被抓到
theme-mutation:         3/3  被抓到
```

钉住本次决策的关键用例：

- `run::tests::setup_sequence_enables_drag_and_wheel` —— **必须**含 `?1002h`，**不得**含 `?1003h`
- `run::tests::wheel_reporting_command_is_minimal` —— `MOUSE_MODE_ON` 逐字节相等
- `events::tests::drag_then_release_copies_text` —— 三态后右键产出剪贴板文本
- `events::tests::v6_click_without_drag_does_not_copy` —— 单击不复制
- `events::tests::v6b_bare_click_does_not_stop_following` —— 单击不冻结跟随
- `events::tests::v6c_real_drag_freezes_view` —— 真拖动必须冻结
- `events::tests::v9_resize_clears_selection` —— resize 清选区
- `events::tests::user_scroll_clears_selection` —— 主动滚动清选区
- `events::tests::v10_oversized_selection_reports_instead_of_copying` —— 超限不截断
- `selection::tests::v8_drag_tick_never_restores_follow` —— 自动滚动不夺回跟随

> ⚠️ **改这块代码前先读本节**：`setup_sequence_enables_drag_and_wheel` 的第 1 条断言
> 一旦被改成「不得含 `?1002h`」，拖选就会再次静默失效。它存在的唯一理由就是防止这件事。
