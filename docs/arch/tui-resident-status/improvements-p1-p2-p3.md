# TUI 可改进清单 — P1/P2/P3 完整列表

## 范围

`arch-explore` 阶段扫描出 P0（核心缺口，Tool 不可见 + Ctrl-C）由 user 在前一轮已决定**单独立项**处理（结合「常驻 footer + last_active」三合一）。

**本附录列出 P1/P2/P3 共 11 项改进**，按 ROI 排序，可作为下一轮 `/arch-design` 输入。

---

## P1 — 高 ROI（推荐下轮一起做）

### 4. Banner 加工具列表

**当前**：banner 不告诉用户 agent 有什么工具。

```
YuShan Coding Agent
Provider: deepseek | Model: deepseek-chat | Dir: ~/Develop/YuShan
Type /help for commands, 'exit' to quit
```

**改进**：

```
YuShan Coding Agent
Provider: deepseek | Model: deepseek-chat | Dir: ~/Develop/YuShan
Tools:     read, write, edit, bash
Type /help for commands, 'exit' to quit
```

**数据源**：`Agent` 当前没暴露 `ToolRegistry`——`agent_runtime::Agent` 的 `registry` 字段私有（`agent.rs:14`）。需新增 `pub fn tool_names(&self) -> Vec<&str>` 或类似 API。**会扩展 Agent 公开 API**。

**改动**：`agent-runtime/src/agent.rs` 加 getter + `format.rs::print_banner` 加 Tools 行 + 单测。

### 5. Banner 加 Config 路径 + 版本

**当前**：banner 无版本号、无 config 文件位置。

**改进**：

```
YuShan Coding Agent v0.1.0
Provider: deepseek | Model: deepseek-chat | Dir: ~/Develop/YuShan
Config:   ~/.yushan/auth.json (3 providers, 2 logged in)
Type /help for commands, 'exit' to quit
```

**数据源**：
- 版本号：`env!("CARGO_PKG_VERSION")` 编译期常量，零成本
- Config 路径：`provider.rs:113-121` 的 `auth_path()` 当前私有，需 pub
- 已登录 provider 数：`ProviderRegistry::auth_store.len()`——需 pub getter

**改动**：`provider.rs` 暴露 `auth_path()` + `logged_in_count()`，`format.rs::print_banner` 用之。

### 6. `/login` 输出精简

**当前**（`commands/builtin.rs:239-268`，约 10 行输出）：

```
Provider:   deepseek
API base:   https://api.deepseek.com
API key:    sk-test-...
Model:      deepseek-chat

Logged in. Model deepseek-chat ready.

Fetched 3 model(s).
```

**改进**：保留 1 行关键信息，详细交给 banner：

```
✓ Logged in to deepseek (deepseek-chat). Fetched 3 models.
```

**改动**：仅改 `builtin.rs::LoginCommand::execute` 末尾 println 块。

### 7. `/model` 输出精简

**当前**（`commands/builtin.rs:384-410`）：

```
Model switched to: deepseek-reasoner
```

本身已简洁，**但**如果常驻 footer 已就位，这行是冗余（footer 立即反映新 model）。

**改进方向 A**：删除该行，让 footer 自解释。
**改进方向 B**：保留，作为确认信号。

**改动**：取决于常驻 footer 落地形式（看 `tui-resident-status/context.md` 设计）。

### 14. Ctrl-C 中断反馈

**当前**：`tui.rs` 不处理 SIGINT。Ctrl-C 直接终止进程，不输出任何提示。

**改进**：用 `tokio::signal::ctrl_c()` 监听 SIGINT → 设 `CancelToken` → 当前 turn 优雅退出 → 显示「✗ Cancelled after 2 rounds · ↑X ↓Y tokens」。

**数据源**：
- `agent_runtime::Agent` 当前无 `cancel_token` getter——`cancel` 字段私有。需扩展公开 API（增 `pub fn cancel_token_mut(&mut self) -> &mut CancelToken` 或 `pub fn cancel(&mut self)`）
- 或：在 `run_turn` 之外通过 channel 触发——更复杂

**改动**：
- `agent.rs` 加 `pub fn cancel(&mut self)` 一行
- `tui.rs` 用 `tokio::select!` 同时监听 `run_turn` 和 `ctrl_c`
- 加 prompt 前显示「(press Ctrl-C to cancel)」

**API 扩展影响**：中等（CancelToken 暴露）。review 时已记为 v0+ 议题。

---

## P2 — 中 ROI

### 8. Turn 耗时

**当前**：`print_turn_summary` 不含耗时。

```
✓ 1 round · ↑320 ↓1,247 tokens
```

**改进**：

```
✓ 1 round · ↑320 ↓1,247 tokens · 2.3s
```

**数据源**：`tui.rs` 主循环在 `agent.run_turn()` 前记 `Instant::now()`，结束后取 `elapsed().as_secs_f32()`。零扩展。

**改动**：`tui.rs` 加局部 timer + `format.rs::print_turn_summary` 加耗时参数。

### 9. Context 占比

**当前**：banner/footer 不显示会话上下文窗口使用情况。

**改进**：

```
✓ 1 round · ↑320 ↓1.247k tokens · 12.5%/128k
```

**数据源**：
- `token::estimate_session_tokens(&agent.session_messages())` —— 已存在 `crates/agent-loop/src/token.rs`
- `RunLimits::context_window` —— `Agent` 私有字段，**需扩展** `pub fn context_window(&self) -> usize`

**API 扩展影响**：高（触及 RunLimits）。review 已记 v0+ 议题。

**改动**：`agent.rs` 加 `context_window()` + `turn_stats.rs` 加 `fn estimate_tokens()` + `format.rs` 加 context% 字段。

### 10. 错误时显示来源

**当前**：`tui.rs:75` `eprintln!("Error: {e}")`——错误信息直接打印，不知道是 model 错还是 tool 错。

**改进**：区分错误来源。`LoopError` 的 `Model(_)` vs `Event(_)` vs `Config(_)` vs `Cancel`——可在打印时加前缀：

```
Error [model]: API key rejected (401)
Error [tool bash]: Command timed out after 300s
```

**数据源**：`LoopError` 是 `enum`，match 即可。零扩展。

**改动**：`tui.rs` 错误分支加 match。

### 11. Session 信息

**当前**：`/status` 不显示 session 起始时间、消息数。

**改进**：

```
Session:   started 2 minutes ago, 12 messages
```

**数据源**：
- 消息数：`Agent::session_messages().len()`（已公开）
- session 起始时间：MemorySession 不持久化。**用 `main.rs` 启动时间作为「本次会话」起点**（user 已决策）。零扩展。

**改动**：
- `main.rs` 加 `let session_started = std::time::Instant::now();`
- `tui.rs` 接收 `session_started: Instant` 参数
- `format.rs::render_status` 加 Session 行 + 用 `Instant::elapsed()` 渲染时长

---

## P3 — 体验优化

### 12. `/help` 文案增强

**当前**：`/help` 只列命令名 + 描述。

**改进**：加 usage hint、首次使用提示。例如 `/login [provider]` 旁加「Run without args for interactive picker」。

**改动**：仅改 `builtin.rs::builtin_help_entries()`。

### 13. 输入历史（↑/↓ 调出历史命令）

**当前**：单行输入，无历史。

**改进**：用 `rustyline` 替换 `io::stdin().read_line()`，启用默认 history（保存在 `~/.yushan/history.txt`）。

**依赖**：`rustyline = "14"`（~150 KB binary）。

**改动**：
- `Cargo.toml` 加依赖
- `tui.rs` 把 `io::stdin().read_line()` 替换为 `Editor::new()?.readline("> ")?`
- 用 `add_history_entry` 自动存

**user 决策**：引入 rustyline。

### 15. Session 计时

**当前**：session 持续时间只在 `/status` 显示。

**改进**：footer 或 banner 加 `2m` 这种短格式。

**改动**：同 11，render 时复用。

### 16. 多行输入

**当前**：`read_line()` 只能单行。

**改进**：rustyline 已支持多行模式（`MultiLineEditor`），但默认关。打开后可输入 `\ 续行`。

**改动**：rustyline 配置项。

### 17. ANSI 颜色

**当前**：全黑白。

**改进**：
- `✓` 绿、`⚠` 黄、`✗` 红
- banner 标题加粗

**依赖**：`termcolor` 或 `colored` crate。最小化可用 ANSI escape 序列（无需 crate，但解析复杂）。

**user 决策**：**暂不引入**——colored crate 是单文件 ~30 行 ANSI 工具，必要时直接 inline。

### 18. Welcome / First-run 提示

**当前**：`~/.yushan/auth.json` 不存在时，banner 直接显示 `(not configured)`。

**改进**：第一次启动时打印欢迎语 + 提示 `/login`：

```
Welcome to YuShan!
No API credentials found. Run /login to set up your provider.
YuShan Coding Agent
Provider: (not configured) ...
```

**数据源**：检测 `ProviderRegistry::auth_store.is_empty()`（需 pub getter）。

**改动**：`main.rs` 或 `tui.rs` 检测首次启动并 print 欢迎语。

---

## 汇总表

| # | 改动 | ROI | 工作量 | API 扩展 | 依赖 |
|---|------|-----|--------|---------|------|
| 4 | banner 加工具列表 | 🟢 高 | 中 | 需 `Agent::tool_names()` | 无 |
| 5 | banner 加 config 路径 + 版本 | 🟢 高 | 低 | 需 `ProviderRegistry::auth_path/pub fns` | 无 |
| 6 | `/login` 输出精简 | 🟢 高 | 低 | 无 | 无 |
| 7 | `/model` 输出精简 | 🟢 高 | 低 | 无 | 无 |
| 14 | Ctrl-C 中断反馈 | 🟢 高 | 中 | 需 `Agent::cancel()` | tokio::signal |
| 8 | turn 耗时 | 🟡 中 | 低 | 无 | 无 |
| 9 | context 占比 | 🟡 中 | 中 | **需 `Agent::context_window()`** | 无 |
| 10 | 错误时显示来源 | 🟡 中 | 低 | 无 | 无 |
| 11 | session 起始时间 | 🟡 中 | 低 | 无 | 无 |
| 12 | `/help` 增强 | 🟡 中 | 低 | 无 | 无 |
| 13 | 输入历史 | 🟢 高（体验）| 中 | 无 | **rustyline** |
| 15 | session 计时在 footer | 🟡 中 | 低 | 无 | 无 |
| 16 | 多行输入 | 🟡 中 | 中 | 无 | rustyline 已含 |
| 17 | ANSI 颜色 | 🟢 高（视觉）| 低 | 无 | 可 inline ANSI |
| 18 | first-run 欢迎语 | 🟡 中 | 低 | 需 `auth_store.len()` pub | 无 |

---

## 推荐分组

**分组 A — 一次性低成本打包**（无 API 扩展、无新依赖，可合并 PR）：
- 6（/login 精简）+ 7（/model 精简）+ 8（turn 耗时）+ 10（错误来源）+ 12（/help 增强）+ 15（session 计时）+ 18（welcome）

**分组 B — banner 加字段**（需小幅 API 扩展）：
- 4（tool 列表，需 `Agent::tool_names()`）+ 5（config 路径 + 版本，需 `ProviderRegistry` getter）

**分组 C — 中等改造**（API 扩展 + 新增逻辑）：
- 9（context 占比，需 `Agent::context_window()`）+ 11（session 时间，与 8/15 合并）+ 14（Ctrl-C，需 `Agent::cancel()`）

**分组 D — 新依赖引入**（独立 PR）：
- 13 + 16（rustyline 一并引入）

**分组 E — 视觉增强**：
- 17（ANSI 颜色，inline 实现）

---

## 与已选方向的关系

- **常驻 footer + last_active**：见 `tui-resident-status/context.md`，user 已决策
- **P0（Tool 可见 + Ctrl-C）**：user 在前一轮提到过，本次未列入但仍应排入后续
- **P1/P2/P3（本附录）**：user 明确「都需要做」

**推荐实施顺序**：

1. 分组 A（最低成本，立即收益）
2. 分组 B（banner 加字段，配合常驻 footer 落地）
3. 分组 C（API 扩展批次，需 ADR）
4. 分组 D（rustyline 独立 PR）
5. 分组 E（颜色最后做，与分组 A/B/C 同步视觉一致性）

**下一步**：用 `arch-design` 设计完整方案时，建议把 5 个分组合成一个 design.md，分章节呈现。
