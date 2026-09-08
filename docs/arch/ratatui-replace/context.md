# TUI ratatui 替换 — 架构上下文

## 概述

将 `apps/coding-agent/src/tui.rs` 从「rustyline + println 循环」重构为「ratatui 立即模式 + crossterm event loop」，实现**真 sticky 底部 footer、右侧 status 面板、transcript 独立滚动**——rustyline 方案无法做到的事。

## 现有架构

### TUI 现状（commit 1ee2d57 后）

`apps/coding-agent/src/tui.rs`（205 行）—— **rustyline + println 循环**：

```
启动 banner + first-run 提示
  ↓
loop {
    if view_dirty { rebuild view from sources }
    print_footer()                      ← 看似常驻但会滚出屏
    line = rl.readline("> ")?           ← rustyline 阻塞 stdin
    dispatch(command or agent turn)
}
```

**限制**（rustyline 根本性天花板）：

| 限制 | 后果 |
|------|------|
| rustyline 无 alt-screen | footer / banner / transcript 全挤一行行 |
| 无持久位置 | 长 transcript 时 footer 滚出可视区 |
| 无独立滚动区 | 历史依赖 terminal scrollback |
| 无侧栏 / 多组件布局 | 只能纯纵向 |
| 无鼠标交互 | 只有键盘 |

### 当前模块边界

| 文件 | 职责 |
|------|------|
| `apps/coding-agent/src/main.rs` (177 行) | 启动入口 + 启动恢复 + 构造 agent |
| `apps/coding-agent/src/tui.rs` (205 行) | REPL 主循环（rustyline 阻塞 + dispatch） |
| `apps/coding-agent/src/tui_completer.rs` | rustyline Completer impl |
| `apps/coding-agent/src/format.rs` (393 行) | 4 个 print_* 函数（**print_banner / print_footer / print_turn_summary / render_status**） |
| `apps/coding-agent/src/view.rs` | `AppView` 数据集中层（14 字段） |
| `apps/coding-agent/src/status.rs` | `TurnStats` 累加器 |
| `apps/coding-agent/src/state.rs` | `AppState` + `StateStore`（~/.yushan/state.json） |
| `apps/coding-agent/src/ansi.rs` | inline ANSI helpers |
| `apps/coding-agent/src/provider.rs` | `ProviderRegistry` + auth.json |
| `apps/coding-agent/src/commands/{mod,builtin}.rs` | Command trait + 10 个命令 |
| `apps/coding-agent/src/{config,prompt}.rs` | Config + system prompt |

### 已有 `AppView` 数据层

ratatui 替换**不影响** AppView——view 是纯数据，所有 renderer（rustyline 风格 print_* / ratatui 风格 widget draw）都可以接 `&AppView`：

```rust
pub struct AppView {
    pub cwd: PathBuf, pub provider: Option<String>, pub model: Option<String>,
    pub config_path: PathBuf, pub logged_in_providers: Vec<String>,
    pub total_known_providers: usize, pub version: &'static str,
    pub total_input_tokens: u32, pub total_output_tokens: u32,
    pub turn_count: u32, pub session_started: Instant, pub message_count: usize,
    pub tools: Vec<String>, pub context_window: Option<usize>,
    pub is_first_run: bool, pub commands: Vec<CommandMeta>,
}
```

**这是 ratatui 替换能轻量实施的关键**：所有数据已在 view 里准备好，重构只改渲染层。

### 关键依赖现状

```toml
[dependencies]
agent-*        (path)        # crates
tokio          = "1"         # async runtime
inquire        = "0.7"       # /login 与 /model 的交互式 picker
rustyline      = "14"        # 主 REPL 输入循环
```

**Cargo.lock 关键发现**：

- `inquire 0.7.5` **已经**拉了 `crossterm 0.25.0` 作为 backend（依赖锁定）
- `rustyline 14` 是纯阻塞 stdin 实现，不依赖 crossterm
- 当前**没有** ratatui

### 外部依赖（Cargo.lock 已有）

```
crossterm 0.25.0          (via inquire)
crossterm_winapi 0.9.1    (via inquire)
```

### Binary size 基准

- 当前 debug build：`64M`
- 当前 stripped debug：`9.7M`
- 当前 release build：`5.7M`（5949920 bytes）

### 已有 crates 内部资源（ratatui 化时可复用）

- `AppView` 数据层（不变）
- `format.rs` 4 个 `print_*` 函数 → 改成 `draw_*` widget 函数（结构略改）
- `tui_completer.rs` 内的 `CmdCompleter` 逻辑 → 可迁移到 ratatui popup 补全
- `state.rs` / `status.rs` / `view.rs` / `commands/*` — **不变**

## 约束

### 技术

- ratatui 最新稳定版 0.29（2025）
- crossterm 0.28 是 ratatui 0.29 官方推荐 backend
- ratatui 自带 backend 抽象（`Backend` trait）—— 切换 backend 不影响 widget 代码
- 异步友好：ratatui event loop 可用 `tokio::select!` 监听 stdin + agent turn 完成 + Ctrl-C
- 测试：`ratatui::backend::TestBackend` 不依赖真实 terminal

### 性能

- ratatui 立即模式：每帧重建 widget —— 但 widget 数据是 `AppView`（~14 owned 字段），构建开销 < 1μs
- 60fps target：frame budget 16ms —— 实际渲染开销 < 1ms，agent turn 占秒级
- alt-screen 切换：1ms（一次性）
- memory：`AppView` 每帧 clone —— 14 字段 ~500 bytes，60fps = 30KB/s，可忽略

### 演进

- AppView / crates API 不变 —— ratatui 是纯 `apps/coding-agent/` 内渲染层
- rustyline 命令兼容：ratatui 替换只动 tui.rs + 新增 ui/ 模块
- 旧 print_* 函数可保留为「rustyline fallback mode」（debug 用）

### 组织

- 引入 2 个新依赖：ratatui + crossterm（后者**已被 inquire 锁定到 0.25**——需选 ratatui 的 crossterm 兼容版本）
- ratatui 社区成熟（30k stars），无需内部维护 TUI 框架
- ~5-6 天工作量（参考上一轮 arch-design 估算）

## 需求范围

### 范围内

1. **ratatui 替换 rustyline 主循环**：
   - `apps/coding-agent/src/ui/mod.rs` — ratatui 主入口
   - `apps/coding-agent/src/ui/app.rs` — `App` struct（transcript + view + input）
   - `apps/coding-agent/src/ui/draw.rs` — `ui(&mut Frame, &mut App)` 函数
   - `apps/coding-agent/src/ui/events.rs` — crossterm event → App action
2. **三栏布局**（垂直分割）：
   - **顶部**：transcript 区（滚动，70% 宽 × 顶部高度）
   - **底部**：input + footer（30% 高度）
   - **右侧**：status panel（30% 宽，显示 provider/model/tokens/session/cwd）
3. **alt-screen 切换**：进入 ratatui 模式自动 `EnterAlternateScreen` + 隐藏光标；退出 `LeaveAlternateScreen`
4. **复用 AppView**：所有渲染数据从 `&AppView` 读，format.rs 函数改为接受 `Frame` + `Area` + `&AppView`
5. **保留 rustyline fallback**（debug mode）：`--no-tui` flag 切回旧循环（保留 print_* 输出格式兼容）
6. **保留 rustyline Completer 的逻辑**：迁移到 ratatui popup 补全（keybindings 实现）

### 范围外（明确不做的）

- ❌ 不做 vim 模式 / emacs 模式（rustyline 的高级编辑功能 ratatui 不默认提供）
- ❌ 不做 mouse 交互（v0 键盘足够）
- ❌ 不做 file tree / agent tree 等多面板布局（侧栏只显示 status，不做交互）
- ❌ 不做 session 选择器 / theme 选择器等 modals（v0 单 session）
- ❌ 不改 AppView / crates 公开 API（ratatui 替换纯渲染层）
- ❌ 不引入 reedline（与 rustyline 重复投入）

### 关键场景

**场景 1 — 启动（已登录 deepseek）**：

```
┌─ YuShan ───────────────────────────────────────┬─ Status ─────────┐
│ ┌─ Chat ────────────────────────────────────┐ │ Provider: deepseek│
│ │                                          │ │ Model: deepseek-c│
│ │ [transcript scrolling area]              │ │                 │
│ │                                          │ │ ↑0  ↓0           │
│ │                                          │ │ Session: 0s      │
│ │                                          │ │ Tools: read,writ..│
│ │                                          │ │                 │
│ │                                          │ │ Cwd: ~/YuShan    │
│ └──────────────────────────────────────────┘ │                 │
│ ┌─ Input ───────────────────────────────────┐ │                 │
│ │ > _                                       │ │                 │
│ └──────────────────────────────────────────┘ │                 │
├──────────────────────────────────────────────┴─────────────────┤
│ Ready · Ctrl-D to quit · Tab to autocomplete                   │
└──────────────────────────────────────────────────────────────┘
```

**场景 2 — Turn 进行中**：

- transcript 顶部有新内容
- status panel `↑X ↓Y` 实时更新（每 100ms poll 一次 view）
- input 区显示 `Working...`
- footer 显示 turn 进度

**场景 3 — Turn 完成**：

- transcript 追加 AI 回复
- status 更新累计 tokens
- input 区清空等待输入
- footer 显示 ✓ Completed

**场景 4 — Ctrl-C**：

- 立即停止当前 turn（ratatui `tokio::select!` + cancel token）
- footer 显示 ✗ Cancelled

## 关键依赖决策

### 选项 1：ratatui 0.29 + crossterm 0.28（推荐）

```toml
ratatui = "0.29"
crossterm = "0.28"
```

**问题**：`inquire 0.7.5` 锁 `crossterm 0.25.0`——会和 `crossterm 0.28` 冲突。

**解决**：
- 方案 A：让 ratatui 0.29 用 crossterm 0.25（向下兼容）
- 方案 B：升级 inquire 到 0.8+（支持 crossterm 0.28）
- 方案 C：ratatui 用 termion backend（Linux/macOS only）

需要验证 ratatui 0.29 是否兼容 crossterm 0.25——查 ratatui CHANGELOG。

### 选项 2：ratatui 0.27 + crossterm 0.27（保守）

```toml
ratatui = "0.27"
crossterm = "0.27"
```

crossterm 0.27 接近 0.28 API，但 ratatui 0.27 widget 略少。

### 选项 3：ratatui 0.29 + ratatui 自带 crossterm feature

```toml
ratatui = { version = "0.29", features = ["crossterm"] }
```

ratatui 0.29 提供 feature flag 选 backend——但仍依赖具体版本。

**初步推荐**：**ratatui 0.29 + 让 cargo 解决依赖**——crossterm 0.28 与 inquire 0.7.5 的 crossterm 0.25 冲突时，cargo 会选较高版本（0.28），inquire 兼容性需测试。

## 模块结构（目标）

```
apps/coding-agent/src/
├── main.rs             [MODIFY]   启动时按 cfg 选 rustyline 或 ratatui 路径
├── tui.rs              [REFACTOR] rustyline 路径保留为 `--no-tui` fallback
├── format.rs           [KEEP]     print_* 函数保留给 fallback；新加 draw_* 函数
└── ui/                 [NEW]      ratatui 路径
    ├── mod.rs          [NEW]      pub fn run(...)  入口
    ├── app.rs          [NEW]      App { transcript, view, input, scroll }
    ├── draw.rs         [NEW]      ui(&mut Frame, &mut App) — 三栏布局
    ├── events.rs       [NEW]      handle_key / handle_resize
    └── completion.rs   [NEW]      ratatui popup 补全（迁移自 tui_completer.rs）
```

## 实施分组（估算）

| 组 | 内容 | 工作量 |
|----|------|--------|
| **R1** | ratatui + crossterm 依赖引入 + 兼容 inquire | 0.5d |
| **R2** | `ui/` 模块骨架 + alt-screen 切换 + 三栏布局 | 1d |
| **R3** | App + event loop + 集成 AppView | 1d |
| **R4** | transcript 滚动 + Ctrl-C + Ctrl-D + Tab 补全 | 1d |
| **R5** | format.rs draw_* 函数（替换 print_* 在 ratatui 路径） | 0.5d |
| **R6** | 测试（TestBackend）+ 旧路径 `--no-tui` flag 兼容 | 1d |
| **总计** | | **~5d** |

## 关键文件清单

| 文件 | 改动 |
|------|------|
| `apps/coding-agent/Cargo.toml` | MODIFY — 加 ratatui + crossterm |
| `apps/coding-agent/src/main.rs` | MODIFY — 按 flag 选 ui 入口 |
| `apps/coding-agent/src/tui.rs` | KEEP — fallback（功能不变） |
| `apps/coding-agent/src/tui_completer.rs` | KEEP — fallback |
| `apps/coding-agent/src/format.rs` | MODIFY — 加 draw_* widget 函数；print_* 保留 |
| `apps/coding-agent/src/ui/mod.rs` | NEW — pub fn run() |
| `apps/coding-agent/src/ui/app.rs` | NEW — App struct |
| `apps/coding-agent/src/ui/draw.rs` | NEW — ui() function |
| `apps/coding-agent/src/ui/events.rs` | NEW — handle_key / handle_resize |
| `apps/coding-agent/src/ui/completion.rs` | NEW — popup 补全 |

## 复用与约束

### 复用现有代码（不变）

- `AppView` / `TurnStats` / `AppState` / `StateStore` / `ProviderRegistry` —— **零修改**
- `format::format_tokens` / `status_symbol` —— 私有 helper，ratatui draw 直接调
- `commands/*` —— Command trait + 10 个命令，零修改
- `crates/*` 公开 API —— 零修改（这是硬约束）

### 与硬约束契合度

- ✅ 单向依赖方向：`apps/` → `crates/*` 不变
- ✅ 最小核心：ratatui 在 `apps/` 内是渲染层，可替换
- ✅ 不跨动态库边界传 Rust trait/Tokio 类型
- ✅ 静态组合优先：ratatui 是组合在 `apps/` 的可替换渲染层

### 验证

```bash
cargo build -p yushan-coding-agent --release
ls -lh target/release/yushan-coding-agent       # binary size
cargo test -p yushan-coding-agent               # TestBackend 单测
yushan-coding-agent --no-tui                    # rustyline fallback 仍工作
```

## 未澄清问题

- [ ] ratatui 版本：0.29（最新）还是 0.27（稳定）？需验证与 inquire 0.7.5 的 crossterm 0.25 兼容性
- [ ] rustyline fallback 是否保留？保留好处：debug / CI / 远程 ssh tty 检测失败时降级；保留成本：tui.rs 长期保留两套
- [ ] transcript 滚动方向：自动滚到底部（follow mode，类似 Pi）vs 手动滚动？建议默认 follow + 滚动手动时禁用
- [ ] 鼠标支持：v0 不要，但 ratatui 后端能力已有——未来成本接近 0，要不要现在预留？
- [ ] status panel 宽度：固定 30 列 vs 跟随窗口（min 25 / max 50）？
- [ ] binary size 影响：ratatui + crossterm 加 ~600KB-1MB release，9.7MB → 10.7MB，可接受吗？
- [ ] AppView 字段是否需要扩展：transcript 滚动状态 / 当前光标位置 / 焦点组件（input vs history）？这些是 ratatui 需要的，可能要加 2-3 字段

## 后续建议

1. **本轮**：`/arch-design` 设计具体 ui 模块接口 + ratatui/crossterm 版本选择 + inquire 兼容性方案
2. **实施**：按 R1-R6 6 组 PR 渐进实施
3. **验证**：TestBackend 单测 + 手动测试三栏布局 + transcript 滚动 + Ctrl-C
4. **保留**：rustyline `--no-tui` fallback 兼容路径
