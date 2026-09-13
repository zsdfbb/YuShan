# TUI 整体方案

> 输入：用户报告的 5 个实际使用问题 + 对 CodeWhale / Claude Code 的调研。
> 前置：`docs/arch/tui-stack-revisit/analysis.md`（ratatui vs 行式 REPL 的存量分析）。
> 本文给出推荐形态与迁移路径。

## 0. 结论

**去掉 alt-screen，改为行式 REPL（对话进 native scrollback，输入区用 rustyline）**，并按 §5 逐项解决 5 个问题。

**最有力的外部证据**：Claude Code 后来加了 alt-screen（`/tui fullscreen`），结果成了**重大回归**——GitHub 上 12+ 个 issue 抱怨 scrollback 丢失（[#42002](https://github.com/anthropics/claude-code/issues/42002)、[#42670](https://github.com/anthropics/claude-code/issues/42670)），官方被迫加 `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` 当逃生口。

**agent 类应用的输出是长会话，用户必须能回看**——这是 alt-screen 的致命伤，也正是用户问题 #1 的根。

---

## 1. 五个问题的根因

| # | 现象 | 根因（文件:行号） | 性质 |
|---|---|---|---|
| 1 | slash 命令时 TUI「退出」 | `ui/mod.rs:261-275` **无条件** `suspend → execute → resume`；`LeaveAlternateScreen`(:120) → 命令跑 → `EnterAlternateScreen`(:136) + `terminal.clear()`(:138)。**2 次切屏 + 1 次强制清屏** | **alt-screen 的产物**；且对 `/help`/`/status`/`/new` 这类**非交互**命令一刀切，属过度使用 |
| 2 | 无 Shift+Enter 换行 | `ui/events.rs:121-127`：`KeyCode::Enter` **完全不看 modifiers**；`mod.rs:93-104` **未启用任何键盘增强协议**；`App.input` 是**单行** String | **bug + 缺实现** |
| 3 | thinking 显示到对话 | `reasoning_content` 路径被**双重挡住**（`ui/mod.rs:359` 只认 `ModelTextDelta`；`lib.rs:264-293` 只把 `self.text` 收进 Text）——**不该显示**。真正泄漏是 **provider 把思考内联进 `content`**（`response.rs:34` → `lib.rs:218-224` → 逐字进对话），代码**无任何 think-tag 剥离** | **provider 差异未处理的 bug** |
| 4 | 命令无提示 | 补全**只在 Tab 触发**（`events.rs:100-118`）；**更严重：`CompletionState` 从未在 `draw.rs` 渲染**——按一次 Tab 屏幕毫无反应，得按两次。`CmdEntry.arg_hint` 数据**早就齐了**（`builtin.rs:48-101`） | **bug（补全状态机存在，缺渲染层）** |
| 5 | — | 整体方案（本文） | — |

**顺带发现的两个真 bug**（不在用户列表里）：

- `ui/events.rs:131` `app.input.remove(app.input_cursor - 1)` —— 光标恒在字符边界，若末字符是**多字节**（中文），`cursor-1` 落在字符中间 → `String::remove` **panic**。输入中文后退格会崩。
- `ProviderCompat::tool_calls_as_text`（`compat.rs:5`）MiniMax 置 true，但**全仓无消费点**——未实现的 compat 分支。

**输入能力现状**（决定了「差多少」）：无光标左右移动、无 Home/End 行内语义、无词级跳转、**无输入历史**（↑↓ 被滚动占用）、**无粘贴**（`Event::Paste` 被丢弃）、单行。

---

## 2. 参考实现调研

| | Claude Code | CodeWhale（前 DeepSeek TUI） |
|---|---|---|
| 语言 / UI | TypeScript + **React/Ink**（深度 fork：packed 数组、双缓冲、cell 级 diff） | **Rust + ratatui**（80+ TUI 文件，`underwater.rs` 做 shell chrome） |
| 全屏策略 | **两套 renderer**：classic（inline，进 scrollback）/ fullscreen（alt-screen）。fullscreen 是后加的，**造成 scrollback 回归**，需 `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` 逃生 | ratatui（未查到 alt-screen 争议） |
| Shift+Enter | `/terminal-setup` 自动配置；**macOS Terminal.app 用 `Option+Enter` 兜底** | `Shift+Tab` 循环推理强度（说明它能拿到带修饰键的事件） |
| 经验 | **alt-screen 对长会话是陷阱**；inline 模式下流式重绘有已知的 flicker/重复 bug（[#52825](https://github.com/anthropics/claude-code/issues/52825)） | ratatui 在 Rust 生态可行，但要 80+ 文件的体量才撑起完整体验 |

**Shift+Enter 的技术事实**（[终端键盘协议的历史](https://blog.fsck.com/agent-blog/2026/02/26/terminal-keyboard-protocol)）：

- 默认情况下终端**发不出** Shift+Enter（Enter 恒为 `\r`）
- 需 **kitty 键盘协议（CSI u）** 或 **xterm modifyOtherKeys**
- 支持情况：kitty / foot / WezTerm / Ghostty / Alacritty / iTerm2 / VS Code terminal / Warp ✓；**macOS Terminal.app ✗**（苹果无计划支持）
- Terminal.app 的兜底：`Option+Enter`（需开启 "Use Option as Meta Key"，正是 Claude Code `/terminal-setup` 自动做的事）
- **crossterm 0.28 已内置**：`PushKeyboardEnhancementFlags` / `PopKeyboardEnhancementFlags` / `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`（已验证存在于本仓库依赖）
- **ratatui 0.29 已内置** `Viewport::Inline(u16)` + `Terminal::insert_before()`（若走 inline 路线的可行性已验证）

---

## 3. 核心决策：alt-screen 去留

**推荐：去掉。**

| 维度 | 保留 alt-screen | 去掉（行式 / inline） |
|---|---|---|
| slash 命令 | 每次进出全屏（问题 #1） | **无感** |
| 长会话回看 | **不可能**（alt buffer 无 scrollback） | native scrollback，`Cmd+F`/鼠标可用 |
| 退出残影 | 需 `transcript_to_lines` + 逐行重打（60 行 + 30 行 hack） | **不存在**（对话本来就在主屏） |
| 冻结/重绘 | 立即模式须显式 `draw`，需 ticker/frame 管理（~250 行） | 无「重绘」概念 |
| 代价 | 换来的「面板/鼠标/sticky footer」**已被产品决策砍掉或默认关闭**（`ui/app.rs:90-91`） | 失去「可编程滚动区」与富对话渲染 |

**一句话**：alt-screen 是为「面板 + 鼠标 + sticky footer」付的，这三样现在一个都不想要；而它的代价（问题 #1、回看不可能、退出 hack）天天在付。

---

## 4. 推荐形态

```
  > 用户输入区（rustyline 管理）
    · 多行：Shift+Enter / Alt+Enter 插入换行，Enter 提交
    · 输入 / 时在下方列出候选（名称 + arg_hint）
    · ↑↓ 翻历史（持久化到 ~/.yushan/history）
    · 粘贴走 bracketed paste（多行粘贴不再被当多次提交）
  ─────────────────────────────────────────────
  ⏺ assistant 输出逐块流式写入主屏（进 native scrollback）
  [spinner] Working…                      ← 一行，\r 原地刷新，turn 结束擦除
  ✓ 2 rounds · 1.3s · ↑12 ↓345 tokens     ← turn 结束打印一行摘要
```

**关键设计点**

1. **对话直接 `print!` 到主屏** —— 不缓冲、不重放；退出时**不需要**任何回灌逻辑（`transcript_to_lines` / `plain_line_rows` / `wrap_plain` 60 行 + `restore_terminal` 33 行全删）
2. **turn 期间不在 `readline` 内** —— 终端是 cooked mode，流式打印就是普通 stdout 写；**不需要 `select!`、ticker、帧管理**
3. **Ctrl-C 是真 SIGINT** —— `tokio::select!(agent.run(ports, &inbox), tokio::signal::ctrl_c())` 直接用 `cancel_handle` 取消（机制已存在）
4. **slash 命令直接执行** —— 无 alt-screen 可让，`inquire` 在真终端正常跑
5. **状态不常驻** —— 需要时 `/status` 打印；turn 结束打一行摘要（现状面板本就默认关闭）

---

## 5. 逐问题解法

### #1 slash 命令不再「退出」

去掉 alt-screen 后**自动消失**。附带收益：命令输出（`/help` 的列表、`/status` 的详情）自然留在 scrollback，而不是像现在这样在 `resume` 后从视野消失。

### #2 Shift+Enter 换行

**两步**：

1. **启用键盘增强协议**（`mod.rs` 的终端 setup）：
   ```rust
   execute!(stdout, PushKeyboardEnhancementFlags(
       KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
   ))?;
   ```
   先探测（crossterm 有 `supports_keyboard_enhancement`），不支持则跳过。**退出时必须 `PopKeyboardEnhancementFlags`**（否则污染用户的终端）。
2. **绑两个键**：`Shift+Enter`（协议支持下）**与 `Alt+Enter`（兜底，macOS Terminal.app 可用）**都插入换行而非提交；输入模型改为支持多行。

> Terminal.app 需用户开 "Use Option as Meta Key" —— 可照 Claude Code 做一次 `/terminal-setup` 之类的提示，或在首次检测到 Terminal.app 时提示一句。

### #3 thinking 不进对话

代码侧**不显示 reasoning 是设计**（`ui/mod.rs:350-351` 注释）。用户看到的是 **provider 把思考内联进 `content`**。解法：

- **剥离常见 think 标签**（` thinking…<｜end▁of▁thinking｜>` / `<thinking>` / `<|thinking|>`）：在适配器累积 `content` 时识别并**只累积正文**，同时把被剥离的内容经 `ModelEvent::ThinkingDelta` 发出
- **`tool_calls_as_text` 的消费点补上**（MiniMax 分支，目前是死配置）
- 提供 `/thinking on|off` 开关 —— 想看思考时显式打开，默认关

### #4 命令提示

三件事，按价值排序：

1. **修 bug：让补全真的显示出来** —— `CompletionState` 现在**从未被渲染**，按一次 Tab 无反应。这是最低成本的修复（有数据、有状态机，只缺画）。
2. **输入 `/` 自动弹候选**（不必先按 Tab）
3. **inline hint**：光标后显示灰色的参数提示（如 `/model <model_name>`）

### #5 输入能力补齐

按优先级：**输入历史（↑↓）> 光标左右移动/Home/End > 粘贴（bracketed paste）> 词级跳转（Ctrl+W/Alt+B）**。

> **注意**：↑↓ 当前被「滚动 transcript」占用 —— 去掉 alt-screen 后滚动由终端原生提供（鼠标/滚轮/Shift+PageUp），**↑↓ 可以还给历史**。

### 附带：修 Backspace 的 UTF-8 panic

`events.rs:131` 的 `cursor - 1` 在多字节字符上会 panic。修法：按 `char_indices` 回退一个**字符**而非一个字节。

---

## 6. 迁移路径（分阶段，每阶段可独立交付）

| 阶段 | 内容 | 交付价值 |
|---|---|---|
| **P0** | **修两个真 bug**：Backspace UTF-8 panic；补全 popup 不渲染 | 立即止血，不改架构 |
| **P1** | **去 alt-screen 改行式**：删 `ui/`，新增 `repl.rs`；对话进 scrollback；slash 命令直接跑 | 解决 #1（最大痛点）、#3 的显示、退出 hack |
| **P2** | **输入能力**：键盘增强协议 + Shift/Alt+Enter 多行 + 历史 + 粘贴 | 解决 #2 |
| **P3** | **补全体验**：`/` 自动弹候选 + arg_hint + inline hint | 解决 #4 |
| **P4** | **thinking 处理**：think 标签剥离 + `tool_calls_as_text` 消费 + `/thinking` 开关 | 解决 #3 |

**P0 可立即做**（半小时量级，且不依赖任何架构决策）。**P1 是分水岭**——它决定后面 P2/P3 的具体写法。

---

## 7. 什么时候该改主意（保留 ratatui）

| 若 | 则 |
|---|---|
| 想要**富渲染的对话区**（markdown、代码高亮、diff 卡片、工具调用卡片） | 走 **ratatui inline viewport**（`Viewport::Inline` + `insert_before`，已验证 API 存在）。**但注意**：inline + 流式重绘有已知难点（Claude Code 的 classic renderer 就有重复/闪烁 bug），比行式难得多 |
| 想要**权限审批弹窗**、模式指示条这类「chrome」 | CodeWhale 路线（ratatui，80+ 文件）。成本高 |
| 需要**鼠标选择/复制** | alt-screen 或 inline 都可支持，行式下由终端原生提供 |

**当前定位（后台 agent + 简洁展示 + 富展示已砍）指向行式**。若这个定位变了，P1 之前是回头的最佳时机。

---

## 8. 需要原型验证的三件事（与 analysis.md 一致）

1. **rustyline `readline()` 返回后终端是否恢复 cooked mode** → 决定流式打印与 Ctrl-C 是否如本文所述工作
2. **turn 中 `select!(run, ctrl_c)` 能否立即中断** → 决定 turn 取消体验
3. **`PushKeyboardEnhancementFlags` 在本机终端的实际支持情况** → 决定 Shift+Enter 是否需要走 Alt+Enter 兜底

三项全过 → 按 §6 推进；任一不过 → 重新评估（可能需要混合方案，会吃掉部分收益）。
