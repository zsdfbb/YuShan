# 0058 — 单击不冻结视窗；插话提交恢复跟随

## Status

accepted（2026-10-24）

## 背景

用户报告两件事，同源于 `follow` 的**翻转条件与用户意图不符**：

1. 「新输出不展示出来，一直保持在之前的位置」—— 一旦发生，后续所有输出都看不见。
2. 「input 之后也立刻滚动输出」—— 提交了输入，输出却没有跟上来。

ADR-0045 把 `follow` 的语义定为「用户在上翻读历史」，并删掉了「新内容到达 ⇒ `follow = true`」。
那是**对的方向**（读历史不该被拽回底部），但两个翻转点的判据当时没审：

### 根因 1：单击即冻结（症状 1）

```rust
fn begin_selection(...) {
    app.selection.begin(top_row + rel_y, rel_x, Instant::now());
    app.follow = false;          // 无条件
}
```

按下左键就无条件冻结，而 `finish_selection` 在「两端点重合」（纯单击）时只 `clear()` 选区，
**不还原 `follow`**。于是**一次单击**（最典型：点一下终端窗口来获得焦点）就永久脱离跟随。

冻结之后增量补偿照章办事（ADR-0045 §2），代入数字立刻看出「永久停住」：

设旧总行数 `T`、内区高 `H`、新内容 Δ 行，则

- `max_scroll = (T+Δ) − H`
- `top_row = max_scroll − scroll_offset = (T+Δ−H) − Δ = T − H` = **旧 `top_row`**

视窗顶点一个格子都不动，新内容全堆在可见区下方。状态行会显示「↓ 有新内容」，
但那只是提示位、不抢视窗 —— 用户看到的就是「一直保持在之前的位置」。

**实测**（`probe`，80×24，回合中点一下后追加 40 段）：

```
[probe2] follow=false offset=80 below=true max_scroll=109 top_row=29
```

`top_row` 恒为单击那一刻的 29，`offset` 随内容逐步 +2 增长 —— 补偿完全生效，
视窗被钉死。

### 根因 2：插话提交不恢复跟随（症状 2）

`App::begin_local_turn`（空闲提交）会 `follow = true; scroll_offset = 0;`，
但 `App::note_steered`（**回合中**提交 ⇒ 轮边界插话）不碰这两个字段。

于是「上翻看过历史 → 回合中敲下一条消息」的组合下，`follow` 一直是 false：

```
[probe6] after scroll up: follow=false offset=10
[probe6] after steer:      follow=false offset=10   ← 提交了却没回底
[probe6] final             follow=false offset=35 top=33
```

用户自己那条回显与模型后续输出全落在视窗之下 —— 连「发出去了吗」都无从确认。

## 决策

**收口成一句**：`follow` 只在**用户真的在阅读历史**时脱离，只在**用户显式表达「我要看最新」**时恢复。

### 1. 单击不脱离跟随；真的拖动了才冻结

- `begin_selection` **删掉** `app.follow = false` —— 按下只是记锚点。
- `extend_selection` 的冻结判据是 **`anchor != cursor`**（指针确实离开了按下点），
  不是「收到一个 `Drag` 事件」：触控板 / 高精度鼠标在单击时会补发一个**坐标相同**的
  `Drag`，按事件判会把单击重新变成冻结。
- `finish_selection` **不还原** `follow`：用户确实动过手拖选过，之后的视窗位置由用户
  自己决定（`End` / `Ctrl+↓` / 滚回底部都能回跟随）。

ADR-0051 决策 8 的契约（拖动期间视窗必须冻结，否则已选中的屏幕坐标会指向别的内容）
**完整保留** —— 只是不再波及「没拖的按下」。

### 2. 插话提交恢复跟随

`App::note_steered` 末尾补 `follow = true; scroll_offset = 0;`，与 `begin_local_turn` 同口径。
「敲了回车」就是显式动作，无论它落在空闲（新回合）还是回合中（插话）。

## 后果

- 点窗口获得焦点、误触聊天区都不再让输出停摆。
- 上翻读历史时仍不会被拽回底部（ADR-0045 的核心诉求不变），状态行照常提示「↓ 有新内容」。
- **取舍**：「按下 → 拖开 → 拖回原点 → 松手」全程 `follow` 已冻结，松手后不自动还原。
  这正确 —— 用户确实动过手；回跟随有明确的键位（`End`）。
- 拖选仍然冻结视窗（ADR-0051 决策 8 不受影响）。

## 验证

| 项 | 位置 |
|---|---|
| 单击（含坐标相同的 `Drag`）不脱离跟随 | `events.rs` `v6b_bare_click_does_not_stop_following` |
| 真的拖动仍冻结视窗 | `events.rs` `v6c_real_drag_freezes_view` |
| 插话提交恢复跟随 | `events.rs` `note_steered_returns_to_following` |
| **画面级**：回合中单击后最新输出仍在屏上 | `draw.rs` `bare_click_mid_turn_keeps_new_output_visible` |

四条用例都做过变异验证（把 bug 注入回去 ⇒ 对应用例变红）：

| 变异 | 变红者 |
|---|---|
| `begin_selection` 里补回 `follow = false` | `v6b…` + `bare_click_mid_turn…` |
| `extend_selection` 改回无条件冻结 | `v6b…`（「指针未移动的 Drag」那条断言） |
| 删掉 `extend_selection` 的冻结 | `v6c…` |
| 删掉 `note_steered` 的跟随重置 | `note_steered_returns_to_following` |

**画面级用例的时序细节**：流式渲染缓存有 100 ms 节流窗口（`cache.rs::RENDER_THROTTLE`），
两步之间不推帧就看不到「内容长了但折行行数没变」这个假象，断言会假绿 / 假红。
用例因此在每一段之后 `sleep(110ms)` 并真实渲染一帧。
