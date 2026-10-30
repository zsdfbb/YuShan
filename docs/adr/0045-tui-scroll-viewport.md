# 0045 — TUI 滚动：视窗化渲染 + 视窗原地不动

## Status

accepted（2026-10-21）

## 背景

三条用户诉求，同源于**滚动路径把整段 transcript 重做一遍**：

1. **上下翻页很慢**；
2. **跳到末尾缺一个说得清的快捷键**；
3. **往上翻之后被自动刷回输入框那里**（不合理）。

第 1 条的根因（实测，release，100 列宽）：

| 规模 | 旧：一次 `compute_max_scroll` | 旧：一次按键（走 3～4 遍） |
|---|---|---|
| 800 行 / 525 KB | 5.6 ms | ~22 ms |
| 3200 行 / 2.1 MB | 23 ms | ~90 ms |

旧实现每帧把整段 transcript 拼成一份 `Text`（`build_text`），再交给 `Paragraph` **全量折行**
+ `line_count` 计数；而**滚动一次也要重做一遍**（事件路径 2 次 + 帧路径 1 次 + 帧内渲染 1 次）。
成本是 *O(整段 transcript 的字节数)*，与内容多长成正比。

第 3 条的根因：`App` 里 13 处「新内容到达 ⇒ `follow = true`」。用户一旦向上翻（`follow = false`），
下一个流式增量 / 工具行 / 回合结算行就把视窗拽回底部 —— 读历史的行为被反复打断。

第 2 条：`End` **早就**绑着「回到最新消息」（T5 起有测试），但**没有任何界面出口** ——
`/help` 只列斜杠命令，键位表只在源码里。等于没有。

## 决策

### 1. 滚动数学与渲染都走「逐条目记忆的折行结果」（视窗化）

新增 `crates/ys-tui-coding/src/layout.rs`：

- **逐条目记忆** `EntryMemo { sig, render_sig, lines, row_starts }`：
  `sig` = 条目内容签名（**廉价**：变体 + 各字符串长度），变了才重渲染；
  `render_sig` = 已渲染内容的哈希 —— 流式节流窗口内「源变了、产物没变」时**连折行结果一起复用**；
  `row_starts` = 该条目每个渲染行折成几个视觉行（用 ratatui 的 `Paragraph::line_count` 逐行算）。
- **全局行号**：`starts`（每条目的块起始行号）+ `gaps`（前置留白行数）+ `total_rows`。
- **视窗**：`window(top_row, height)` 二分定位条目与行内偏移，取出覆盖视窗的那些行，
  外加「首行内部要跳过几行」（`Paragraph::scroll` 的纵向分量）。

于是：**滚动数学 = 查表**（O(1)），**渲染 = 只折视窗覆盖的行**（O(视窗高度)）。
实测同一条 3200 行 / 2.1 MB 的会话：滚动数学 **23 ms → 43 µs**，且**与内容长度无关**。

**不自写折行**：仍用 ratatui 的 `Paragraph::line_count`（CJK 列宽口径与渲染严格一致），
不引第二套折行实现。视窗切片依赖「整段折行 == 逐行折行之和」（N4 不变式）——
`draw.rs` 的等价性用例（视窗渲染与整段渲染**逐行相同**）是它的守卫。

**代价**：布局记忆与 md 缓存各存一份渲染行（内存 ×2）；`line_count` 逐行调用有分配开销
（每条目只在**内容变化**时算一次）。

### 2. 滚动状态：`scroll_offset`（距底部行数）不变，跟随语义改掉

- **保留** `scroll_offset`（距底部的视觉行数）与 `follow` 两个字段与含义。
- **删掉**「新内容到达 ⇒ `follow = true`」的 6 处：流式增量、工具调用行、模型侧 User 回声、
  模型 System/Error 行、`Outbound::Output` 行、回合结算。
- **保留** `follow = true` 的 5 处：提交新回合、`/resume` 回放、`/thinking` 切换、
  命令类本地输出、Ctrl-C 取消提示 —— 这些是**用户显式动作**，回到底部是期望行为。
- **视窗原地不动**：脱离底部时，内容在**尾部**增长 Δ 行 ⇒ `scroll_offset += Δ`
  （它是「距底部行数」）⇒ 视窗顶部那一行不动。宽度变化时行号空间整个换掉 ⇒ 不补偿。
- **提示位** `new_content_below`：脱离底部且下方来了新内容时置真，状态行显示「↓ 有新内容」；
  跳回底部即清零。

### 3. 键位：Mac / Linux 通用的一组

| 键 | 行为 |
|---|---|
| `End` / `Ctrl-End` / `Ctrl-Down` | 跳到最新消息（末尾） |
| `Ctrl-Home` / `Ctrl-Up` | 跳到最早消息（顶部） |
| `↑` `↓` | 逐视觉行滚动（脱离底部即停止跟随） |
| `PageUp` / `PageDown` | 翻页，步长 = **视窗高度 − 1**（原为写死的 10 行） |
| 滚轮 / 触控板上下 | 逐 **3 视觉行**滚动（[ADR-0050](0050-tui-mouse-wheel-scroll.md)；需 `EnableMouseCapture`，故裸拖选失效） |
| `Home` | 输入行首（**裸** `Home` 仍归输入光标，与 `End` 刻意不对称） |

**不绑 `⌘` 组合**：macOS 终端自己消费 `⌘`，字节到不了应用（`Terminal.app` / `iTerm2` 的出厂映射
把 `⌘+←/→` 映射成 `Ctrl-A/E`，正是这个道理）。**不绑 `⌥+↓`**：Terminal.app 发 `ESC[B` ——
与普通 `↓` **同形**，无法区分。`Ctrl+↑/↓` 与 `Ctrl+Home/End` 是 `CSI 1;5A` 一类，终端无关。

**`/help` 增「键位」段**：键位此前没有任何界面出口（`/help` 只列斜杠命令），
「跳到最新消息」这种高频操作试不出来就等于没有。

## 后果

- 滚动与翻页的成本**与会话长度解耦**；长会话里按键跟手。
- 用户翻上去读历史时**不会被拽回底部**；新内容以「↓ 有新内容」提示，不抢视窗。
- 布局记忆的失效判定依赖**廉价签名**（长度）：内容变了但长度没变（极少见，且只原地改写）
  会漏检 —— 那种情况下折行结果多半也不变，代价是「可能没跟上」而非「算错」。
- 逐条目的折行记忆是**纯 memo**：清空只损失 CPU，不改变输出。
- 退出回放路径（`transcript_plain_lines`）**未变**（它不走 wrap）。

## 验证

| 项 | 位置 |
|---|---|
| 视窗渲染 ≡ 整段渲染（逐行相同） | `draw.rs` `test_windowed_render_matches_full_render` |
| 视窗有界（行数 ≤ 视窗高 + 1，与内容长度无关） | `draw.rs` `test_window_is_bounded_by_viewport` |
| N4（逐条目折行之和 == 整段折行） | `layout.rs` `test_total_rows_equals_whole_line_count` |
| 内容未变 ⇒ 不重新解析 | `layout.rs` `test_unchanged_entries_are_not_re_rendered` |
| 视窗原地不动 + 提示位 | `app.rs` `test_detached_view_stays_put_when_new_content_arrives` |
| 跳到底部清提示 | `app.rs` `test_jump_to_end_restores_follow_and_clears_hint` |
| 新键位 | `events.rs` `test_ctrl_arrows_and_home_end_jump_to_ends` / `test_end_jumps_to_latest_but_bare_home_stays_in_input` |

真实终端的观感（≥100 KiB 长回答下翻页是否跟手）仍需人工确认 ——
见 `docs/design-final/coding-agent-tui.md` §10 的 `MANUAL_ACK` 清单。
