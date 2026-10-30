# 0050 — TUI 接管鼠标：滚轮 = 滚聊天

让滚轮 / 触控板在 TUI 里有**自己的语义**，而不是经由终端被翻译成 `↑` / `↓` 落进输入历史。

## Status

accepted（2026-10-23）

## 背景

用户报告：macOS 下用触控板滑 `coding-agent` 窗口，翻出来的是**历史输入**，不是对话。

根因是**三层键位归属的合成**，不是任何一处绑错：

1. 终端把滚轮 / 触控板滑动译为 `↑` / `↓` 字节（不进应用层的鼠标事件）。
2. `events.rs` 把 `↑` 归**输入历史**（readline 语义，ADR-0045 的键位表）。
3. 全仓**不读鼠标事件** —— `handle_event` 的 `_ => {}` 把 `Event::Mouse` 直接丢掉，
   终端也从未开 `EnableMouseCapture`。

于是「滚轮滚动」这个动作在应用看来与「按了一下 `↑`」完全同形，必然走历史。
用户用的是 macOS 触控板，两个方向的滑动都会持续产生 `↑` / `↓`，表现为「一直翻历史」。

同一轮还有两个加重因素：macOS **系统级**快捷键占用了 `Ctrl+↑` / `Ctrl+↓`（Mission Control /
App Exposé，字节到不了终端），MacBook 内置键盘**没有独立的 PageUp / PageDown 键**
（须 `fn+↑` / `fn+↓`）。⇒ 键盘侧的滚动入口在 Mac 上都不顺手。

**为什么现在反转**：ADR-0030 把「鼠标」与「语法高亮 / Theme / Diff Viewer / 可配快捷键」
一起列为**维持砍掉**的纯 UI 花活。当时砍它的语境是「后台 agent + 简洁 TUI，终端原生选择够用」。
但滚轮不是花活 —— 它是用户在长对话里**最自然的滚动动作**，而当前实现把这个动作接到了
错误的对象上（改的是输入框，不是视窗）。**反转的只有滚轮一项**，点击、拖拽、悬停仍不做。

## 决策

### 1. 应用接管鼠标，但只处理滚动

- `run.rs::setup_terminal` 追加 `EnableMouseCapture`。
- `events::handle_mouse(kind, app, width, height)`：只认 `ScrollUp` / `ScrollDown`，
  其余（`ScrollLeft` / `ScrollRight` / `Down` / `Up` / `Drag` / `Moved`）**一律忽略**。
- `handle_event` 增加 `Event::Mouse(m) => handle_mouse(..)` 分支。

### 2. 滚轮 = 滚聊天，每事件 3 视觉行

单事件位移经 `WHEEL_ROWS`（`events.rs` = 3）折算，**唯一乘算点**是
`WheelDelta::rows()` —— 单事件路径（`handle_mouse`）与批路径（`scroll_wheel_batch`）
都读它，故不存在「一处滚 3 行、另一处滚 1 行」的偏差。

| 事件 | 行为 |
|---|---|
| `ScrollUp` | 滚 +3 视觉行 —— 往旧内容，脱离底部即 `follow = false` |
| `ScrollDown` | 滚 −3 视觉行 —— 往新内容，回到底部即恢复 `follow = true` |

**为什么不是 1 行**：`ScrollUp` / `ScrollDown` 是**离散的「滚一格」**事件，触控板
一次滑动爆发 8–30 个（见背景）。按 1 行算，整次滑动最多 30 行 —— 40 行终端上翻
不过一屏，观感就是「滑不动」。**为什么不是一页**：按页算一次轻滑就飞过整屏。
3 行是两头之间的取值（一事件约 1/10 屏）。

- 位移在**批内只施加一次**：净量 ×3 一次性落盘 ⇒ 一次 `layout.sync`，与逐事件
  累加等价（夹取单调）。
- 与 `↑↓`、`PageUp` / `PageDown` 共享同一个 `scroll()` —— 夹取、`follow` 翻转、
  `max_scroll` 计算全部只有一份。

### 3. 恢复序列必须关掉鼠标捕获

`restore_sequence()` 在 `DisableBracketedPaste` 之后加 `DisableMouseCapture`
（`?1006l ?1015l ?1003l ?1002l ?1000l`，由 crossterm 的 `DisableMouseCapture` 一次性发出）。

**放在共享序列里而不是各写一遍**：该序列被**正常退出**（`exit_payload`）与
**panic 退出**（`restore_terminal_on_panic`）共用。漏掉这条的后果是进程退出后主屏
**仍处于鼠标捕获模式** ⇒ 用户的终端原生拖选、滚 scrollback 全部失效，只能 `reset` 或重开窗口。
这是与 `PopKeyboardEnhancementFlags` 同类的「泄漏进 shell」缺陷，故同样进共享序列、同样有断言。

### 4. `/help` 增滚轮一行

键位段此前只列键盘。滚轮成为一等入口后必须可见 —— 否则用户仍然只会去试 `↑↓`。

## 代价（明确接受）

- **终端原生拖选失效**。捕获期间绝大多数终端不再响应鼠标拖选；
  **此代价已由 ADR-0051 消掉**：裸左键拖选 = 应用内选区 + 松手即复制，
  `⇧`+拖选则把原生选择**让回**终端（iTerm2 / Terminal.app / WezTerm / Alacritty 均可用）。
  本条保留为决策当时的代价记录；现状看 ADR-0051。
  与 `docs/arch/tui-markdown-render/design.md` §2.8「拷贝友好」的张力也因此解除 ——
  应用内选区不引字形过滤，拷出的仍是屏幕所见。
- **不绑点击**：`Down` / `Up` / `Drag` 在当时全部忽略。**后续被 ADR-0051 反转了拖拽**
  （应用内选区需要它们）；**点击 / 悬停仍不绑**。
  本条保留为决策当时的取舍记录。
- **鼠标捕获与「简洁 TUI」定位的张力**：只换来滚动一项能力，换来的是「不再需要终端侧
  键位映射」。判断是：滚动是高频动作，键位映射（iTerm2 Pointers / WezTerm `mouse_bindings`）
  是每台机器都要做一次的**用户侧配置**，不该由产品要求用户去做。

## Considered Options

### A. 不做应用改动，让用户在终端里把滚轮映射成 PageUp / PageDown

- 优点：零代码；一次配置对 vim / less 等所有 alt-screen 程序都有效。
- 否决理由：**Terminal.app 没有滚轮键位绑定入口**（只能绑键盘组合）⇒ 一部分用户根本做不到。
  且把「滚动该滚什么」的决定权交给终端的字节映射，应用永远无法区分「滚轮」与「按了 `↑`」。
  边缘情况仍在 `docs/design-final` 记录为补充手段（见 §与其它文档的关系）。

### B. 只把 `↑↓` 的历史语义在「输入框为空时」让给滚动

- 否决理由：这正是**当前**行为的一部分（`↑` 在历史为空时才回落滚动），而用户报告的场景里
  历史**非空** —— 冲突丝毫不减。且会破坏 readline 语义（ADR-0045 已定：`↑` 不设前置条件）。

### C. 绑点击 / 拖拽，做成完整的鼠标界面

- 否决理由：ADR-0030 的红线（点击、拖拽、悬停、可配快捷键不做）仍然有效。
  滚动是**一个动作**，点击是**一整套交互模型**（命中测试、焦点、选区），成本量级差两个数量级。

### D. 滚轮按页滚动（与 `PageUp` / `PageDown` 同粒度）

- 否决理由：见决策 2 的步长论证。实测触控板一次轻滑产生多个事件，按页会一次翻过整屏。

### E. 维持「每事件 1 视觉行」

- 否决理由：初版（2026-10-23）取值。实测触控板一次滑动 8–30 个事件 ⇒ 整次最多 30 行，
  在 40 行终端上**翻不过一屏** —— 与后来「滑动还是很慢」的反馈一致。ADR-0051 的批次
  合并修好了「整次只滚 1 行」，但没改这个基数。

## 风险与规避

| 风险 | 规避 |
|---|---|
| 退出后鼠标捕获泄漏，拖选失效 | 进**共享**恢复序列（正常 + panic 两条路径）；`restore_sequence_contains_terminal_state_restore` 断言 `?1006l` 与 `?1000l` 都在 |
| 滚轮未来被误接回输入历史 | `test_wheel_scrolls_chat_and_never_input_history` 的**前提是 history 非空** —— 若历史为空，滚轮行为与「`↑` 回落滚动」同形，反而抓不到这个 bug |
| 水平滚动 / 点击被顺手绑上动作 | `test_other_mouse_events_are_ignored` 逐个事件断言视窗不动 |
| 步长被改成整页 / 退回 1 行 | 两条用例断言精确的 `±WHEEL_ROWS` 与 `12 × WHEEL_ROWS`；后者另分断 `net()`（事件数）与 `rows()`（视觉行）⇒ 乘算丢了也红 |
| 分派分支被 `_ => {}` 吞掉 | `test_handle_event_routes_mouse_to_scroll` 走完整 `handle_event` |
| 模态浮层里的滚轮 | 浮层自读事件（`run.rs` 的 `Prompter` 实现），`_ => {}` 掉鼠标 —— 浮层内容不滚动，捕获开着只为不丢状态。**未做**，见 TODO |

## 验证

| 项 | 位置 |
|---|---|
| 滚轮滚聊天、绝不碰输入历史 | `events.rs` `test_wheel_scrolls_chat_and_never_input_history` |
| 其余鼠标事件不动视窗 | `events.rs` `test_other_mouse_events_are_ignored` |
| `Event::Mouse` 真的被分派 | `events.rs` `test_handle_event_routes_mouse_to_scroll` |
| 恢复序列含关鼠标捕获 | `run.rs` `restore_sequence_contains_terminal_state_restore` |
| 变异测试（6 处注入：接历史 / 改步长 / 吞其余事件 / 删分派分支 / 漏关捕获 / 方向反了） | `scripts/wheel-mutation.py`（`python3 scripts/wheel-mutation.py`；6 / 6 被抓到） |

**真实终端的观感仍是 `MANUAL_ACK`**：触控板滑动的跟手程度、`⇧`+拖选在各终端下的可用性，
均需人工确认，单测覆盖不到。

## 与其它文档的关系

- **修订 [ADR-0030](0030-tui-markdown-render.md) 的一条**：「鼠标**维持砍掉**」→
  「**滚轮做（ADR-0050）；点击 / 拖拽 / 悬停仍砍**」。
- **补 [ADR-0045](0045-tui-scroll-viewport.md) 的键位表**：滚轮成为聊天滚动的第四个入口。
- `docs/design-final/coding-agent-tui.md` §18.4 / §18.6 已回写。
- `docs/arch/tui-markdown-render/design.md` §2.8「拷贝友好」的前提需带交叉引用（原生选择在捕获期间失效）。
- **终端侧键位映射仍然有效**（iTerm2 Pointers / WezTerm `mouse_bindings` 把滚轮 → `PageUp` / `PageDown`）：
  它是**补充手段**，不是本决策的替代品 —— 应用接管后滚轮已由应用自己解释，无需终端配置。
