# YuShan TUI 方案

> 目标：**轻量、稳定**。基本的状态展示与命令提示，不做全屏、不做渲染器、不做花活。
> 前置：`analysis.md`（ratatui vs 行式 REPL 的存量分析）；`../../gap-closure/context.md`（产品定位：后台 agent + 简洁 TUI）。

---

## 0. 设计原则

三条，按优先级：

| 原则 | 含义 | 反例（我们不做） |
|---|---|---|
| **不做渲染器** | 对话是**文本流**，直接写 stdout。不自绘、不管理帧 | ratatui 的 `Frame`/`Buffer`/cell diff |
| **不做全屏** | 不用 alt-screen。对话进**终端原生 scrollback** | 现在这套：进出全屏 + 退出重打 |
| **能删的都删** | 每一行代码都要能回答「为什么」 | 见 §5 的删除清单 |

**「稳定」的具体含义**（可度量）：

- 无自定义帧管理 → 不会「界面冻结」
- 无 raw-mode 事件路由 → **`^C` 就是真 SIGINT**，不会假死
- 无 alt-screen → 不会「退出残影」、不会「命令时闪屏」
- 对话在 scrollback → 崩溃/超时也不丢历史

---

## 1. 形态

```
⏺ 帮你把 TUI 改成行式。                                    ← assistant（逐块流式写入主屏）
                                                            
  主要问题是 alt-screen 与长会话回看的冲突……                
                                                            
> 那命令提示怎么办？                                        ← 用户输入（历史可回看）
                                                            
⏺ 用 rustyline 的补全，带参数说明：                          
                                                            
  /model <model_name>   切换模型                            
  /new                  开新会话                            
                                                            
  ✓ 2 rounds · 1.3s · ↑1.2k ↓345 tokens                    ← turn 摘要（一行）
                                                            

> /mo▏                                                      ← 输入区（rustyline 管理）
  ┌────────────────────────────────────┐                    
  │ /model  [model_name]  切换模型      │                    ← 候选（Tab 触发，含参数提示）
  │ /compact              压缩上下文    │                    
  └────────────────────────────────────┘                    
```

**三块 chrome，都极简**：

| 元素 | 形态 | 何时出现 |
|---|---|---|
| 对话 | 主屏文本流，`⏺ ` / `> ` 前缀 | 始终（自然滚动） |
| 状态 | **一行**：turn 中 spinner（`\r` 原地刷），turn 后摘要 | turn 期间 / 结束 |
| 候选 | rustyline 内建列表（含参数说明） | Tab 或输入 `/` 时 |

**没有**：常驻面板、侧栏、状态栏、滚动区、鼠标、主题。

---

## 2. 组件

```
apps/coding-agent/src/
  repl.rs          ← 新增：行式 REPL 主循环（替代 ui/mod.rs）
  repl_complete.rs ← 新增：Completer + Hinter（替代 ui/events.rs 的补全部分）
  format.rs        ← 恢复 print_banner / print_turn_summary / print_status 等纯文本输出
  ui/              ← 删除（5 文件 1684 行）
```

| 组件 | 职责 | 预计 |
|---|---|---|
| `repl.rs` | readline 循环；slash 分发；turn 驱动（流式打印 + spinner + 摘要）；SIGINT 取消 | ~230 行 |
| `repl_complete.rs` | 命令名补全（含 `arg_hint` 描述）；inline 灰字提示 | ~170 行 |
| `format.rs` | 纯文本格式化（banner、turn 摘要、`/status`） | ~80 行 |
| **净变化** | | **约 -1150 行** |

**复用的既有件**（不用改）：`Wiring` / `AgentPorts` / `Inbox` / `ChannelSink` / `AppView` / `Prompter`。

---

## 3. 交互设计

### 3.1 输入

| 能力 | 实现 |
|---|---|
| 行编辑（光标、Home/End、Ctrl+W、Ctrl+U） | rustyline 内建 |
| **多行**：`Enter` 提交，`Shift+Enter` / `Alt+Enter` 换行 | 见下「键盘协议」 |
| **历史**（↑↓，持久化 `~/.yushan/history`） | rustyline 内建 —— **↑↓ 从「滚动」还给历史** |
| 粘贴（多行不被当多次提交） | rustyline 的 bracketed paste |

**键盘协议**（Shift+Enter 的前提）：

```rust
// 启动时（先探测，不支持则跳过）
execute!(stdout, PushKeyboardEnhancementFlags(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
))?;
// 退出时**必须** Pop，否则污染用户终端
```

- 支持 kitty 协议的终端（kitty / WezTerm / Ghostty / Alacritty / iTerm2 / VS Code / Warp）→ `Shift+Enter` 可辨识
- **macOS Terminal.app 不支持** → 绑 `Alt+Enter` 兜底（需用户开 "Use Option as Meta Key"）
- 首次检测到不支持的终端时，**提示一次**（照 Claude Code 的 `/terminal-setup` 思路，我们不自动改用户的终端配置）

### 3.2 命令提示（三层，都是基本款）

| 层 | 形态 | 触发 |
|---|---|---|
| **候选列表** | 名称 + `arg_hint` + 描述（rustyline `Pair { display, replacement }`） | 输入 `/` 后自动弹 + Tab 补全 |
| **inline 提示** | 光标后灰字补全剩余（如 `/mo` → 灰显 `del [model_name]`） | 输入时实时 |
| **完整帮助** | `/help` 打印命令表；`/help <cmd>` 打印单条 | 显式 |

**数据早就有**：`builtin_help_entries()`（`commands/builtin.rs:48-101`）已含 `name` / `description` / `arg_hint`。现在的问题是 `CompletionState` **从未被渲染**——换成 rustyline 后由它渲染，问题消失。

### 3.3 状态展示（三处，非常驻）

| 场景 | 输出 |
|---|---|
| **turn 进行中** | 一行 spinner：`⏳ Working… 3.2s`，`\r` 原地刷新，turn 结束擦除 |
| **turn 结束** | 一行摘要：`✓ 2 rounds · 1.3s · ↑1.2k ↓345 tokens`（符号复用现有 `StopReason` 映射） |
| **想看详情** | `/status` 打印多行（provider / model / tokens / session 路径 / cwd / tools） |

**不做常驻状态栏** —— 那需要全屏或 sticky 机制，是 alt-screen 的原始动机，已明确放弃。

### 3.4 打断与命令

| 场景 | 行为 |
|---|---|
| turn 中 `^C` | **真 SIGINT** → `select!(agent.run(ports, &inbox), tokio::signal::ctrl_c())` → `cancel_handle().cancel()` → turn 以 `Cancelled` 收场 |
| idle `^C` | rustyline 的 `Interrupted` → 清空当前行 |
| idle `^D` | 退出 |
| **slash 命令** | **直接执行，无 suspend/resume**（没有 alt-screen 可让）。`/login` 的 inquire 在真终端正常跑；命令输出自然留在 scrollback |

### 3.5 thinking

**默认不显示**（与现在一致）。补两件事：

1. **剥离内联 think 标签**（` thinking…<｜end▁of▁thinking｜>` 等）—— 当前 provider 若把思考塞进 `content`，会原样进对话（这是用户报告的问题 #3 的真因）
2. `/thinking on|off` —— 想看时显式打开

---

## 4. 与两个参考实现的取舍

| 能力 | Claude Code | CodeWhale | **YuShan** |
|---|---|---|---|
| 全屏 / alt-screen | 有（后加，**造成 scrollback 回归**，需逃生口） | ratatui | **不做** |
| 富对话渲染（markdown / 高亮 / diff 卡片） | 有（重写 Ink 渲染引擎） | 有 | **不做**（已砍） |
| 常驻状态栏 / 侧栏 / 模式指示 | 有 | 有（`underwater.rs` 做 chrome） | **不做**（`/status` 按需） |
| 鼠标 | 有 | 有 | **不做**（终端原生选择/复制够用） |
| 审批弹窗 | 有 | 有 | 用 `inquire` 在真终端做（已有 `Prompter` 抽象） |
| 主题 / 多语言 | 有 | 有（五语言 + 主题选择器） | **不做** |
| 多行输入 / Shift+Enter | 有（`/terminal-setup`） | 有 | **做** |
| 命令候选 + 参数提示 | 有 | 有 | **做**（rustyline 内建） |
| 输入历史 | 有 | 有 | **做**（rustyline 白送） |
| 流式输出 | 有 | 有 | **做**（已有事件信道） |
| 体量 | 巨型 | 80+ TUI 文件 | **~480 行**（净删 1150） |

**一句话**：取两个参考的**交互骨架**（多行输入、命令提示、流式），**丢弃它们的展示层**。

---

## 5. 删除清单

| 删除 | 行数 | 为什么可以删 |
|---|---|---|
| `ui/draw.rs` 全部 | 597 | 不自绘 → 无渲染层（含 307 行 TestBackend 测试） |
| `ui/mod.rs` 终端生命周期（`setup`/`suspend`/`resume`/`restore`） | 79 | 无 alt-screen / raw mode |
| `ui/mod.rs` 事件循环 + `run_turn_with_ticks` + 三个 helper | 193 | 无帧管理、无 turn 期键鼠路由 |
| `ui/events.rs` 键处理 + popup 导航 + 滚动 | 160 | rustyline 接管行编辑与补全 |
| `ui/app.rs` ratatui 专属字段（`scroll_offset`/`follow`/`working_dot`/…） | ~80 | 无滚动区、无动画 |
| `ui/draw.rs` 退出回灌链（`transcript_to_lines`/`plain_line_rows`/`wrap_plain`） | 60 | 对话本就在主屏 |
| `ui/completion.rs` 空占位 | 8 | 无意义 |
| 相关测试 | ~500 | 随渲染层消失（改为「stdout writer 快照 + 纯函数」） |

---

## 6. 依赖

- **`rustyline`**（重新引入）—— 行编辑 / 历史 / 补全 / hint / bracketed paste。手写这些约 10k 行，不值得
- **移除** `ratatui` / `crossterm` 的 `event-stream`（`crossterm` 仍需，用于键盘协议 push/pop 与终端尺寸）
- `futures` 视是否还需要 `StreamExt` 而定

---

## 7. 实施分期

| 阶段 | 内容 | 风险 | 交付 |
|---|---|---|---|
| **P0** | **修两个真 bug**（与架构无关）<br>① `events.rs:131` 中文退格 **panic**<br>② 补全 popup **从未渲染** | 极低 | 立即止血 |
| **P1** | **去 alt-screen 改行式**：`ui/` → `repl.rs`，对话进 scrollback，命令直接跑 | **中**（分水岭） | 解决 #1 |
| **P2** | 键盘协议 + `Shift/Alt+Enter` 多行 + 历史 + 粘贴 | 低 | 解决 #2 |
| **P3** | 补全候选（含 `arg_hint`）+ inline 提示 | 低 | 解决 #4 |
| **P4** | think 标签剥离 + `/thinking` 开关 | 低 | 解决 #3 |

**P0 与 P1 独立** —— 即使 P1 延后，P0 也该立刻做。

---

## 8. 开工前必须先验证的三件事

P1 的两条核心论断依赖 rustyline 的行为，**必须用最小原型确认**（可放 `tmp/`）：

| # | 要验证 | 若不过 |
|---|---|---|
| 1 | `readline()` 返回后终端**恢复 cooked mode** → turn 期间的 `print!` 与 SIGINT 正常 | 流式/取消要另想办法，收益大打折扣 |
| 2 | `select!(agent.run(ports, &inbox), tokio::signal::ctrl_c())` 能**立即中断** turn 并用 `cancel_handle` 收尾 | 同上 |
| 3 | `PushKeyboardEnhancementFlags` 在**本机终端**的实际支持情况 | `Shift+Enter` 只能靠 `Alt+Enter` 兜底 |

**三项全过** → 按 §7 推进；**任一不过** → 回头评估 hybrid（rustyline 输入 + 线程渲染），但那会吃掉大部分收益。

---

## 9. 成功标准

| 用户问题 | 验收 |
|---|---|
| 命令时退出 TUI | 跑 `/login` 全程无切屏、无闪烁；命令输出留在 scrollback |
| 无法换行 | `Shift+Enter`（支持的终端）或 `Alt+Enter` 插入换行；`Enter` 提交 |
| thinking 进对话 | 用返回内联 think 标签的 provider 时，思考**不出现在对话**；`/thinking on` 可显式打开 |
| 命令无提示 | 输入 `/` 弹出候选（含描述）；Tab 补全生效；`/help` 打印完整表 |
| 整体 | `ui/` 消失，`repl*.rs` 合计 ≲500 行；`cargo test --workspace` 全绿 |

**稳定性回归项**（新增测试）：长会话（多轮）下对话不重复、不闪烁；`^C` 在 turn 中与 idle 下行为正确；`PushKeyboardEnhancementFlags` 在异常退出路径也能 Pop。
