# 0064 — 鼠标只接管滚轮：拖选归终端，滚轮归应用

在「鼠标全归终端」（上一轮的 `f39a122`）与「应用接管全部鼠标」（ADR-0050 + ADR-0051）
之间取第三条路：**只开滚轮上报，其余鼠标事件不绑**。

## Status

accepted（2026-11-02）

## 背景

用户报告：`f39a122` 把鼠标交还终端之后，**触控板 / 滚轮无法上下翻看对话上下文了**。

根因是那一轮的取舍本身：不开 `EnableMouseCapture` ⇒ 终端自己解释滚轮。两个后果都不理想：

1. 终端把它当成滚自己的 scrollback —— 本应用在 alt-screen 里整屏重绘，终端 scrollback
   里没有对话内容 ⇒ 滚了没反应；
2. 或终端把它译成 `↑` / `↓` 字节 ⇒ 落到 `events.rs` 的**输入历史**上，翻出来的是上一条
   提交的文本。这正是 ADR-0050 当初接管鼠标要解决的问题，等于原样回退。

键盘侧的替代入口在 macOS 上都不顺手：系统级快捷键占用 `Ctrl+↑` / `Ctrl+↓`
（Mission Control / App Exposé），MacBook 内置键盘没有独立 PageUp / PageDown。

## 决策

### 1. 只开滚轮上报

`run::setup_terminal` 用自定义的 `EnableWheelReporting`（`?1000h` + `?1006h`），
**不用** crossterm 的 `EnableMouseCapture`。后者连带开 `?1002h`（按下拖动上报）与
`?1003h`（任意移动上报）—— 对滚轮毫无用处，却让指针一动就持续产生 `Moved` 事件。
退出仍走 crossterm 的 `DisableMouseCapture`（反序关 1006 / 1003 / 1002 / 1000），
对「只开过两个」的情形是无害的超集。

### 2. 应用只认滚轮

`events::handle_mouse` 只处理 `ScrollUp` / `ScrollDown`，其余（`Down` / `Drag` / `Up` /
`Moved` / 水平滚动）一律忽略。滚轮 = 滚聊天，`WHEEL_ROWS` = 3 视觉行 / 事件。

### 3. 保留 ADR-0050 决策 2 的合批

触控板一次滑动爆发 8–30 个事件 ⇒ `run::coalesce_wheel` 排空紧跟的一串（上限
`MAX_WHEEL_BATCH` = 4096，保证排空循环有界），`events::fold_wheel` 折叠成净量，
`events::scroll_wheel_batch` 一次施加 ⇒ 只做一次 `layout.sync`，位移与逐事件累加等价。
净量为 0（方向来回抵消）⇒ 不动视窗。

### 4. 不建应用内选区

ADR-0051 的应用内选区（`selection.rs`，2077 行）**不再恢复**。选区与剪贴板归终端：
各终端（xterm.js / VTE / iTerm2 / tmux / SSH）实现差异极大，自建选区必须逐个适配，
且永远有终端截获事件。

**代价**：鼠标追踪期间终端的裸拖选与右键菜单不生效（事件送到应用后被忽略）。复制走
终端的「强制原生选择」逃生门（多数终端是 `⇧`+拖选）或 `/copy`（OSC 52 + 本机工具双发）。

## 与 ADR-0050 / 0051 的关系

ADR-0050 的「滚轮归应用」**恢复有效**；ADR-0051 的「拖选归应用」**被本条推翻**。
两处 ADR 各自保留当时的取舍记录，现状以本条与 `docs/design-final/coding-agent-tui.md`
§18.7 为准。

## 风险

| 风险 | 处置 |
|---|---|
| 部分终端在 `?1000h` 下连裸拖选也不响应 | 用 `⇧`+拖选（多数终端支持）或 `/copy` |
| `?1000h` 下滚轮是否会漏报（终端差异） | 已覆盖 xterm 系与 VTE；tmux 需 `set -g mouse on`，属终端侧配置 |
| 模态浮层不吃滚轮 | 已知缺口，`docs/TODO.md` §C 有补法 |

## 验证

- `run::tests::wheel_reporting_sequence_is_minimal`：初始化序列只含 `?1000h` + `?1006h`。
- `run::tests::restore_sequence_contains_terminal_state_restore`：退出必须关掉捕获。
- `events::tests::test_wheel_scrolls_chat_and_never_input_history`：滚轮不碰输入历史。
- `events::tests::test_fold_wheel_accumulates_and_cancels` / `test_null_wheel_batch_leaves_offset_untouched`：
  同向累加、反向抵消、净量为零不动。
- `events::tests::test_other_mouse_events_are_ignored`：非滚轮事件不滚动。
- PTY 实测（抓原始字节）：`?1000h` / `?1006h` 各 1 次，`?1002h` / `?1003h` / `?1015h` 均 0 次。
