# coding-agent TUI 显示问题 — 架构上下文

## 概述

修复 ratatui TUI 的两个显示问题：**① turn 执行期间界面零刷新**（用户提交后"历史不刷新"）；**② 退出后 alt-screen 残影残留**（"关闭后残留"）。本文是对此前诊断的事实核查与根因修正，供 `arch-design` 出方案。

## 现有架构

### 模块边界

```
apps/coding-agent/src/ui/
├── mod.rs       # 入口 run() + setup/restore_terminal + event_loop + dispatch_input + run_turn_with_ticks
├── app.rs       # App（UI 全部状态）+ TranscriptLine + CompletionState
├── events.rs    # 键盘事件 → App 状态变更（Char/Esc/Tab/Enter/滚动/Ctrl-C/Ctrl-D）
├── draw.rs      # ratatui 渲染：transcript / input / footer / status 四块
└── completion.rs# Tab 补全（slash command 列表）
apps/coding-agent/src/view.rs  # AppView 显示快照（transcript 之外的 status 数据源）
apps/coding-agent/src/main.rs  # 入口：交互模式调用 ui::run(...).await
```

feature flag：`tui-ratatui`（默认开启）。`ui/` 模块整个在 `#![cfg(feature = "tui-ratatui")]` 下编译。

### 核心抽象

| 抽象 | 位置 | 职责 |
|------|------|------|
| `App` | `app.rs:10` | UI 全部状态：transcript / scroll / input / is_turning / should_quit / pending_submit / cancel_token / **view 快照** |
| `AppView` | `view.rs:34` | 显示侧数据源快照（provider/model/tokens/turn_count/session…），`AppView::from_sources` 重建 |
| `TranscriptLine` | `app.rs:35` | 聊天历史一行（User/Assistant/Tool/Summary/Error/System） |
| `event_loop` | `mod.rs:105` | 主循环：draw + `tokio::select!(events, tick)` |
| `dispatch_input` | `mod.rs:152` | 提交分发：记录 User 行 → slash 命令 或 agent turn |
| `run_turn_with_ticks` | `mod.rs:254` | turn 执行 + 自身 100ms tick（空）+ 内层 Ctrl-C 捕获 |
| `restore_terminal` | `mod.rs:91` | 退出恢复：EnableLineWrap → LeaveAlternateScreen → disable_raw_mode → show_cursor |

### 关键数据流

**主循环（idle 时）**：`terminal.draw`（loop 顶部，每轮必画）→ `select!`：
- `events.next()` → `handle_event` → 若 `should_quit` break → 若有 `pending_submit` → `dispatch_input(...).await` ← **整个 loop 被阻塞**
- `tick.tick()`（100ms）→ 仅当 `is_turning` 时重建 `app.view`

**提交路径**：Enter（events.rs:121 置 `pending_submit`）→ 下一事件触发 `dispatch_input` → `transcript.push(User)`（mod.rs:177）→ slash 命令（命令分发，可能重建 view）或 agent turn：
- turn 开始：`is_turning = true`，进入 `run_turn_with_ticks` 的**独立 select 循环**，外层 loop **完全挂起**，内层 tick 分支**空**（mod.rs:272，仅留注释"view 由 event_loop refresh"）
- turn 结束：`is_turning = false`，push summary + 重建 view（mod.rs:232）

**退出路径**：`Ctrl-D`（events.rs:48 直接置 should_quit）或 `exit`/`quit`/`/quit`（dispatch_input:171 / builtin.rs:649 置 should_quit）→ event_loop 下一轮事件后才检查 `should_quit` break → `restore_terminal`。

**渲染数据源**（关键区分）：
- **chat 历史** ← `app.transcript`（draw.rs:60-64，live，每轮 draw 都取当前值）
- **status 面板 + footer** ← `app.view` 快照（draw.rs:122-142），**只在重建时更新**

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|------|------|------|------|
| ratatui | Terminal / Frame / 渲染 | workspace 锁定 | `Terminal::draw` 每次全帧 |
| crossterm | raw mode / alt-screen / 键盘事件 | workspace 锁定 | `LeaveAlternateScreen` 不清屏 |
| tokio | event loop / interval / ctrl_c | workspace 锁定 | 双 select 嵌套是问题核心 |
| futures | `EventStream` | workspace 锁定 | |

## 根因核查（对原诊断的事实修正）

### 症状 1："历史不刷新" — 原诊断部分不准确 ✅→⚠

**原诊断**：`App` 无 `view_dirty` 字段；tick 分支仅 `is_turning` 时重建 view；`dispatch_input` 完成后未触发重绘 → 新内容下一帧才显示。

**核查结果**：
- ✅ **无 `view_dirty`** 属实：`App` 结构体（app.rs:10-32）无该字段；`view_dirty` 只出现在注释里（view.rs:5-6 文档声称"view_dirty = true (set by tui.rs)"、mod.rs:271 注释"view_dirty 由 is_turning 在 event_loop 外层判断"）—— **stale doc vs 实现**。
- ⚠️ **"新内容不重绘"不成立**：`terminal.draw` 每轮 loop 顶部必执行（mod.rs:122），且 **transcript 是 live 的**（draw.rs:60-64 直接读 `app.transcript`）。因此 slash 命令的错误输出、`/help` 结果等**都会在下一轮显示**，路径上 even 不需要重建 view。原诊断的"用户输入后 view 不重绘 → 100ms 内才看到"与代码不符。
- 🔴 **真正的根因是架构性冻结**：`dispatch_input(...).await` 在 event_loop 的 events 分支内 **内联 await**（mod.rs:131），整个外层 select（含 100ms tick 的 view 重建）在 turn 期间**完全挂起**；而 `run_turn_with_ticks` 的内层 tick 分支**是空的**（mod.rs:272），turn 期间 **零 `terminal.draw()`**。
  - 效果：按 Enter → 屏幕停在提交前一帧（input 框已清空、用户自己的消息行已 push 到 transcript 但**不可见**）→ 整个 turn（秒级到分钟级）界面冻结 → turn 结束后一次性全部渲染。
  - 所有数据流在内存都是"对"的（transcript 一直在更新），**缺的只是渲染机会**。
  - `is_turning` 时重建 view 的机制（mod.rs:135）在 turn 期间**实际不可达**（loop 被阻塞），是死代码。

**次级问题**（真实但影响小）：`app.view` 快照只在 4 条路径重建——slash Continue（mod.rs:192）、turn Ok（mod.rs:232）、tick（死路径）。以下路径**不重建**：slash Err（mod.rs:202）、slash Exit（mod.rs:201）、turn Err（mod.rs:242）、`exit`/`quit` 裸词（mod.rs:171-174）。这些路径下 status 面板可能 stale（但 chat 历史正常）。

### 症状 2："关闭后残留" — 原诊断准确 ✅

**核查结果**：
- ✅ `restore_terminal`（mod.rs:91-103）：EnableLineWrap → **LeaveAlternateScreen** → disable_raw_mode → show_cursor。**无任何 `terminal.clear()` / `Clear(ClearType::All)` / 显式 flush**（grep 全 ui/ 目录确认）。
- ✅ ratatui 的 `LeaveAlternateScreen`（`\x1b[?1049l`）只切回主屏幕，**不清空 alt 屏最后一帧**。主屏幕内容未重绘，shell scrollback 会把 alt 屏最后一帧（footer + input 框"> "提示符 + 状态栏）并入主屏历史，紧贴退出后的新 shell prompt → "残影"。
- ✅ main.rs 交互模式 `ui::run(...).await?` 后直接 `Ok(())`（main.rs:189），无任何清屏/换行/提示输出，残影与 prompt 直接相邻。
- ⚠️ 残留是否复现**依赖终端模拟器**的 scrollback 合并策略；需在目标终端实测确认（部分终端切回主屏时丢弃 alt 屏内容）。

### 附带发现：`exit`/`quit`/`/quit` 需二次按键才退出 ⚠️

`should_quit` 检查在 **dispatch_input 之前**（mod.rs:127-129），而 `dispatch_input` 才设置 `should_quit`。因此 `exit`、`quit`、`/quit`（均经 dispatch_input 置 should_quit）返回后，loop 继续，**必须再按任意键**才触发 break。仅 `Ctrl-D`（events.rs:48 直接在 handle_event 置位）立即退出。属小 bug，建议一并修复（在 dispatch_input 后再检查一次）。

## 约束

- **技术**：Rust + Tokio（单线程 select 模型）；ratatui/crossterm 是既定渲染栈；feature flag `tui-ratatui` 为交互模式唯一实现（`tui-stdout` 已删）。`agent.run_turn` 是 async；`Agent` 是唯一数据来源，`Config`/`ProviderRegistry`/`TurnStats` 主线程持有。
- **性能**：turn 内 100ms 心跳刷新；渲染走 ratatui `Terminal::draw`（全帧 diff）。刷新粒度无硬指标，但"秒级无反馈"不可接受。
- **演进**：事件 → App 状态 → 渲染的解耦方向保持不变（e 阶段未破坏该方向即可）；view.rs 文档已陈旧（`tui.rs` 模块名不存在、`view_dirty` 未实现），需同步修正文档或实现。
- **组织**：维护/讨论/commit 中文（仓库约定）；改动须对应 `docs/design.md` §12 测试矩阵；`ui/` 已有 TestBackend 单测（draw.rs:209-351、events.rs:222-302），修复应保持单测可覆盖。

## 需求范围

### 范围内（本次要解决的）

- **turn 期间刷新**：运行 agent turn 时界面持续渲染（用户提交的行立即可见 + status/输入框状态更新），不再整段冻结。两种可行方向：外层 event_loop 不阻塞（turn 与事件循环并行）或内层 tick 接到 draw（需要共享 `&mut Terminal` / `&mut App`）。
- **退出清屏**：`restore_terminal` 增加清屏（退出前清 alt 屏或 `Clear(Cleared)` 后 `rs1`），消除 shell scrollback 残影。
- **退出即时性（小）**：`exit`/`quit`/`/quit` 无需二次按键。

### 范围外（明确不做的）

- 修改终端模拟器的 scrollback 行为（不可控）。
- 流式 token 渲染 / 逐 token 显示 assistant 输出（本问题只要求 turn 期间界面刷新，不要求内容级流式；流式属另一需求）。
- `tui-stdout` / rustyline 回退（c 阶段已彻底删除，见提交 645e339）。
- 沙箱 / 多租户 / 安全（仓库硬性约束，见 CLAUDE.md）。

### 关键场景

- 场景 1（主）：用户输入问题 → Enter → **立刻看到自己的消息行 + input 框变 "Working…"** → turn 进行中 status/tokens 周期性刷新 → 完成后 summary + assistant 输出出现。
- 场景 2（主）：用户 Ctrl-C / Esc 取消 turn → 秒级恢复交互，界面同步。
- 场景 3（清理）：用户 `/quit` 退出 → **一次按键即退**，shell 回到干净的 prompt，scrollback 无 TUI 残影。
- 场景 4（边界）：slash 命令报错（如未知命令）→ Error 行立刻显示，status 面板不 stale。

## 未澄清问题

- [ ] turn 期间刷新粒度：只做"消息行立即可见 + 心跳 status"（最小），还是要逐 tool call / 逐 token 追加？（决定方案复杂度）
- [ ] "残留"在当前目标终端上实测是否复现？若目标终端不合并 scrollback，症状 2 优先级可降。
- [ ] 退出后是否要求**主屏幕保持用户原有 scrollback**（干净 prompt）还是**全清**？社区两种偏好都有，需确认。
- [ ] turn 与事件循环解耦后，`cancel_token`（Esc/Ctrl-C 在 turn 中）的并发路径是否受影响——`run_turn_with_ticks` 内层 Ctrl-C 捕获会被外层吞掉还是仍可达？
- [ ] 是否引入 `view_dirty` 标志（恢复 view.rs 文档意图）以替代"每次 draw 都重建"？还是维持"事件后按需重建"？

## 后续建议

- 用 `arch-design` 就"turn 期间刷新"做多方案对比（推荐重点：**仅把 turn 移出 event_loop 阻塞点 / 内层 tick 直接持有 draw 权限**两个方向；附"内层 select 与 process 连体"取舍）。
- 用 `prototype` + TestBackend 验证：模拟长 turn（sleeping MockModel）观察 draw 计数，确认修复后 turn 期间有持续 draw。
- `exit` 二次按键与 `restore_terminal` 清屏属低风险小改动，可随手或随方案一并提交。
- view.rs 顶部文档（`tui.rs` / `view_dirty`）随实现修正同步更新，避免继续误导。