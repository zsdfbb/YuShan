# 0037 — `Ctrl-J` 普适换行键：`Shift+Enter` 依赖终端协议，Termux 下不可用

给 TUI 输入区补一个**终端无关**的「换行但不提交」键：`Ctrl-J`（终止控制符 0x0A）。原因：用户报告
「`Alt+Enter` 能换行、`Shift+Enter` 不行」—— 排查证明这不是应用缺陷，而是 **Termux 终端根本发不出
可区分的 `Shift+Enter`**（它把 `Shift+Enter` 与 `Enter` 都编码成同一个字节 `\r`）。已存在的 `Alt+Enter`
之所以能用，是因为 Alt 由终端编码成 **ESC 前缀**（`\x1b\r`），这是几乎所有终端都实现的机制。

## Status

accepted（2026-10-13）

## 背景

- **现象**（用户原话）：「coding-agent 的 tui input 有点问题，alt+enter 可以换行，但是 shift+enter 不行。」
- **应用侧本来是对的**：`crates/ys-tui-coding/src/events.rs` 早就有 `KeyCode::Enter if shift || alt => 换行`，
  且 `run.rs` 会 push `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`。**问题在字节来源**。
- **终端侧证据（Termux）**：`termux/termux-app` 的 `KeyHandler.getCode`：
  ```java
  case KEYCODE_ENTER:
      return ((keyMode & KEYMOD_ALT) == 0) ? "\r" : "\033\r";
  ```
  —— `KEYCODE_ENTER` **只看 ALT**，`SHIFT`（与 `CTRL`）被完全忽略。故 `Shift+Enter` 与 `Enter`
  发出**同一个 `\r`**；`Alt+Enter` 发出 `\033\r`。且 Termux 的 `TerminalEmulator.doCsiBiggerThan`
  只处理 `CSI > c`（DA2）与 `CSI > m`（modify-resource，**记日志后忽略**），**不处理 `CSI > … u`**
  ⇒ 它**不进入** Kitty 键盘增强协议 ⇒ 永远不会为 `Shift+Enter` 发 `CSI 13;2u`。
- **解码侧证据（crossterm 0.29，实测）**：raw 模式下 `\r`→`Enter`；`\x1b\r`→`Enter+ALT`；
  `CSI 13;2u`→`Enter+SHIFT`；`\x0a`（LF）→**`Char('j') + CONTROL`**（**不是** `Char('\n')`）。
- **缺口**：`\x0a`（即 `Ctrl-J`）到应用后落在 `_ => {}`（`Char('j')` 带 `CONTROL` 不匹配
  `Char(c) if !ctrl`）⇒ **当前是 no-op**。而它是 Termux 下**唯一**能发出的「不提交的换行」候选。

## 决策

**1. 新增 `Ctrl-J` ⇒ 插入换行（`crates/ys-tui-coding/src/events.rs`）**

- 一个匹配臂：`KeyCode::Char('j') if ctrl => app.input.insert_char('\n')`。**不提交**。
- 位置在 Enter 两臂之后、`Backspace` 之前；补全浮层打开时它先被 `handle_completion_key` 关掉浮层
  （与其它普通键同规），不吞键。

**2. `Shift`/`Alt` + Enter 保持不变**

- `Alt+Enter` 仍是**终端无关**的换行键（ESC 前缀 + `\r`）。
- `Shift+Enter` 仍在——**支持** Kitty 协议的终端（Kitty / WezTerm / iTerm2 等）会发 `CSI 13;2u`，
  应用照常插入换行；不支持的终端下它退化为 `Enter`（提交），这一点**无法在应用侧补救**。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 加 `Ctrl-J`（采纳）** | 补一个终止控制符换行键 | 终端无关；`Ctrl-J` 是社区惯用的「Shift+Enter 替代」（Claude Code 亦支持） | 多一个键位；`Ctrl-J` 在 readline 里本义是 accept-line（此处改义） |
| **B 只文档化，不改代码** | 告诉用户「用 `Alt+Enter`」 | 零改动 | 不解决问题：`Shift+Enter` 仍提交；没有新可用的换行键 |
| **C 自己实现 Kitty/modifyOtherKeys** | 手动 push 协议 + 解析 `CSI u` / `CSI 27;…~` | 理论上覆盖更多终端 | Termux **不实现**这两者 ⇒ 对它无效；纯属为一个走不通的终端加复杂度 |

**采纳 = A**。理由：病灶是「Termux 发不出可区分的 `Shift+Enter`」，应用侧唯一能做的是**提供一个它
发得出的换行键**；`Ctrl-J`（LF，0x0A）是任何终端都能发的终止控制符，且是业界常见的替代键。

## Consequences

- **正面**：Termux（及任何不支持键盘增强协议的终端）下，输入区多了一个可靠的「换行不提交」键；
  支持协议的终端行为不变（`Shift+Enter` 仍可用）。
- **负面 / 取舍**：`Ctrl-J` 在 readline 语义里等于 accept-line（回车），这里改义为「换行」；这是**有意**
  的取舍（TUI 是聊天输入框，不是 shell 行）。
- **行为变更（须写进 PR 描述）**：`Ctrl-J` 由 no-op 变为「插入换行」。
- **不可补救项（如实记账）**：`Shift+Enter` 在**不实现** Kitty 键盘增强协议的终端上无法工作 —— 这是
  终端能力问题，非应用缺陷；受影响的用户请用 `Alt+Enter` 或 `Ctrl-J`。

## 关联

- `crates/ys-tui-coding/src/events.rs` — 模块文档「换行键」一节记同一结论；`test_ctrl_j_inserts_newline_not_submit` 钉住行为
- [ADR-0035](./0035-truncation-continuation.md) / [ADR-0036](./0036-no-artificial-output-cap.md) — 同期相邻改动（同属一次排查会话）
- `docs/design-final/coding-agent-tui.md` — 输入区 as-built
