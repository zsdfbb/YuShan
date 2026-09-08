# ratatui 替换 — 架构设计

## 概述

将 `apps/coding-agent/src/tui.rs` 从「rustyline 阻塞 + println」迁移到「ratatui 立即模式 + crossterm event loop」。**所有数据已在 `AppView` 数据集中层准备好**，ratatui 替换只动渲染层；crates/* 公开 API 零修改。

3 个并行 subagent 基于 [`context.md`](./context.md) + WebSearch 调研（[ratatui BREAKING-CHANGES](https://docs.rs/crate/ratatui/latest/source/BREAKING-CHANGES.md) / [inquire issue #252](https://github.com/mikaelmello/inquire/issues/252)）给出三方案。本文综合后推荐**方案 C**——数据集中 + 4 个 draw_* 直传。

## 三方案对比

| 维度 | A 最小 | B Trait 抽象 | **C 数据集中（推荐）** |
|------|--------|------------|--------------|
| ui/ 文件数 | 3 | 5 | **5**（更清晰） |
| 抽 trait / dyn | 无 | Panel + Drawer 双 trait | **无** |
| AppView 字段扩展 | 0 | +4（focus / follow / last_turn_elapsed / last_stop_reason） | **0** |
| format.rs 双轨 | 是 | 是 | **是** |
| transcript 模型 | Vec<TranscriptLine> + display_height | Trait 抽象 | **Vec<TranscriptLine> + 4-variant enum** |
| 间接调用/帧 | 0 | 3-6 次 vtable | **0** |
| 与 AppView 耦合度 | 高（额外字段） | 中（trait 中转） | **低（直传 &AppView + &App）** |
| 60fps 渲染开销 | < 1ms | < 1ms + 200ns dyn | **< 1ms** |
| 加新 panel 成本 | +1 fn in draw.rs | +1 struct + impl + register | **+1 fn in draw.rs** |
| Rust 风格契合度 | ⭐⭐⭐ | ⭐⭐ | **⭐⭐⭐⭐** |

## 关键事实（Step 2 调研）

| 事实 | 来源 |
|------|------|
| ratatui 0.29 要求 crossterm 0.28 | [ratatui BREAKING-CHANGES](https://docs.rs/crate/ratatui/latest/source/BREAKING-CHANGES.md) |
| inquire 0.7.5 锁 crossterm 0.25 | [inquire issue #252](https://github.com/mikaelmello/inquire/issues/252) |
| ratatui 自带 `ratatui::crossterm` re-export 避免版本歧义 | ratatui docs |
| user 决策 | 升级 inquire 到 0.8 解锁 crossterm 0.28 |

## 推荐方案

### 核心命题

**ratatui 是 renderer，不是 framework。** AppView 已是渲染所需全部数据的单一来源；ratatui 化只是把 `print_*` 改成 `draw_*`，把 `rustyline 阻塞 stdin` 改成 `crossterm event poll + Frame::render`。

**三不原则**：

| 不做 | 理由 |
|------|------|
| 不抽 Widget/Drawer trait | dyn dispatch 成本 + ratatui 0.29 widget 都是直接构造 |
| 不引入 Elm/Redux 风格 Action enum 全局化 | App 字段直读直写，单向流 |
| 不改 AppView 14 字段结构 | transcript / scroll / input 是 chat 状态不是 snapshot |

### 模块边界

```
apps/coding-agent/
├── Cargo.toml                        [MODIFY] +ratatui=0.29, +crossterm=0.28, inquire=0.7→0.8
└── src/
    ├── main.rs                       [MODIFY] 按 --no-tui flag 选 ui::run 或 tui::run_interactive
    ├── tui.rs                        [KEEP]   rustyline fallback（功能冻结）
    ├── tui_completer.rs              [KEEP]   rustyline 路径依赖
    ├── format.rs                     [MODIFY] 双轨：保留 print_* + 新增 draw_*
    ├── view.rs                       [KEEP]   AppView 14 字段零修改
    ├── commands/, config.rs, ...     [KEEP]
    └── ui/                           [NEW]
        ├── mod.rs                    [NEW]   run() 入口 + alt-screen guard
        ├── app.rs                    [NEW]   App struct + TranscriptLine enum
        ├── draw.rs                   [NEW]   ui() + 4 个 draw_*
        ├── events.rs                 [NEW]   crossterm Event → App action
        └── completion.rs             [NEW]   popup 补全
```

依赖方向：
```
ui/* → view::AppView (read-only)
ui/* → format::{draw_*, format_tokens, status_symbol}
ui/* → ratatui::Frame + crossterm::event
ui/* → crates/* (agent for run_turn/cancel)
```

### 关键数据结构

**`App` struct（ui/app.rs）**：

```rust
pub struct App {
    // 数据源
    pub view: AppView,                // 重建时由 from_sources 填充
    pub view_built_at: Instant,

    // transcript
    pub transcript: Vec<TranscriptLine>,
    pub scroll_offset: usize,
    pub follow: bool,                 // 默认 true；用户 Up/Down → false

    // input
    pub input: String,
    pub input_cursor: usize,
    pub completion: Option<CompletionState>,

    // 控制
    pub is_turning: bool,
    pub cancel_requested: bool,
    pub should_quit: bool,
}
```

**`TranscriptLine` enum（ui/app.rs）**：

```rust
pub enum TranscriptLine {
    User(String),
    Assistant(String),
    Tool { name: String, args: String, result: String, success: bool },
    Summary { rounds: u32, stop: StopReason, elapsed_secs: f32 },
    Error(String),
    System(String),
}
```

**为什么不放 AppView**：
- transcript 来源是 ratatui loop（chat 累积），不是 Agent
- `AppView::from_sources` 仍是纯函数，不接受外部累积状态
- 强合并导致 view.rs 与 ui/app.rs 职责混杂

### ui() 函数（三栏布局）

```rust
pub fn ui(frame: &mut Frame, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(frame.area());

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),     // transcript
            Constraint::Length(3),  // input
            Constraint::Length(1),  // footer
        ])
        .split(cols[0]);

    draw_transcript(frame, left[0], app);
    draw_input(frame, left[1], app);
    draw_footer(frame, left[2], &app.view, app.is_turning);
    draw_status_panel(frame, cols[1], &app.view);
}
```

### Event loop（ui/mod.rs）

```rust
let mut events = crossterm::event::EventStream::new();
let mut tick = tokio::time::interval(Duration::from_millis(100));

loop {
    terminal.draw(|f| ui(f, &mut app))?;

    tokio::select! {
        Some(event) = events.next() => {
            handle_event(event, &mut app)?;
            if app.should_quit { break; }
            if let Some(input) = app.take_submitted() {
                dispatch(input, agent, config, commands, stats, state_store, &mut app).await;
                app.view_dirty = true;
            }
        }
        _ = tick.tick() => {
            if app.is_turning {
                app.view = AppView::from_sources(...);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            app.cancel_requested = true;
        }
    }
}
```

### format.rs 双轨

```rust
// 保留 (--no-tui fallback)
pub fn print_banner<W: Write>(out: &mut W, view: &AppView) -> io::Result<()> { ... }
pub fn print_footer<W: Write>(out: &mut W, view: &AppView) -> io::Result<()> { ... }
pub fn print_turn_summary<W: Write>(...) -> io::Result<()> { ... }
pub fn render_status<W: Write>(...) -> io::Result<()> { ... }

// 新增 (ratatui 路径)
pub fn draw_status_panel(frame: &mut Frame, area: Rect, view: &AppView) { ... }
pub fn draw_footer_line(frame: &mut Frame, area: Rect, view: &AppView, is_turning: bool) { ... }
// ...

// 共享 helper
pub fn format_tokens(n: u32) -> String { ... }    // 已是 pub
fn status_symbol(stop: &StopReason) -> String { ... }  // 改 pub(super)
```

## 关键决策

### 1. 不抽 Widget trait

**Why**：
- AppView 已是「数据 → 渲染」解耦点
- 60fps × 3 个 panel = 180 次虚调用/秒 ≈ 不可见开销但零收益
- 单元测试可直接构造 `App::default()` + `TestBackend`，不依赖 trait mock

**How**：每个 panel 一个 `pub fn draw_*(frame, area, &App, &AppView)`，App 直传。

### 2. AppView 零字段扩展

**Why**：
- transcript / scroll / input 是 chat 状态（user 累积产生），不是 snapshot（agent/config 派生）
- 强行合并会污染 `from_sources` 纯函数性 + 增加 AppView 体积
- App 是「UI 全部状态」单一容器，AppView 是「展示快照」——两者严格分离

**How**：view.rs 一行不改；scroll / follow / cursor / completion 全归 `App` struct。

### 3. format.rs 双轨

**Why**：
- `print_*` (Writer + AppView) → 适配裸 stdout（`--no-tui` fallback）
- `draw_*` (Frame + Area + AppView) → 适配 ratatui 立即模式
- 共享 `format_tokens` / `status_symbol` 保证输出风格一致

**How**：`print_*` 不删（fallback 用），`draw_*` 新增；测试可分别 `Vec<u8>` 和 `TestBackend` 覆盖。

### 4. 保留 rustyline fallback (`--no-tui`)

**Why**：
- debug / CI / 远程 ssh tty 失败时降级
- 5 个命令交互（`/login` / `/model`）依赖 inquire，ratatui 不能完全替代
- 长期保留 tui.rs ~200 行可接受

**How**：main.rs 检测 `--no-tui` flag → 选 `ui::run` 或 `tui::run_interactive`。

### 5. transcript 滚动：follow 默认 + 手动退出

**Why**：Claude Code / OpenCode / Aider 行业标准；用户新消息进来自动滚到底，但用户主动上滚查看历史时不强制跳回。

**How**：`App.follow = true` 默认；`Up`/`PageUp` → `follow=false`；新内容追加 / `End` / 用户提交 turn → `follow=true`。

### 6. 状态 panel 宽度：30% 固定

**Why**：最小终端 80 列时 24 列，足够放 `Provider: deepseek-chat` 等字段。

**How**：`Constraint::Percentage(30)` 横向 Layout；窄终端由 ratatui 自动收缩。

## 实施分组（R1-R6，~5d）

| 组 | 内容 | 文件 | 时间 |
|----|------|------|------|
| R1 | Cargo.toml + 升级 inquire 0.8 | `Cargo.toml` | 0.5d |
| R2 | ui/ 骨架 + alt-screen + 三栏静态 | `ui/{mod,app,draw}.rs` | 1d |
| R3 | event loop + AppView 集成 + view_dirty tick | `ui/mod.rs` | 1d |
| R4 | transcript 滚动 + Ctrl-C/D + Tab 补全 + follow mode | `ui/{app,events,completion}.rs` | 1d |
| R5 | format.rs draw_* 函数 | `format.rs` | 0.5d |
| R6 | TestBackend 单测 + `--no-tui` flag | `main.rs` `tests/` | 1d |

## 关键文件清单

| 文件 | 改动 |
|------|------|
| `apps/coding-agent/Cargo.toml` | +ratatui=0.29 +crossterm=0.28 +inquire=0.8 |
| `apps/coding-agent/src/main.rs` | +5 行（`--no-tui` flag + `ui::run` 入口） |
| `apps/coding-agent/src/format.rs` | +draw_* 函数（~120 行）；print_* 不删 |
| `apps/coding-agent/src/ui/mod.rs` | NEW — `run()` + alt-screen guard |
| `apps/coding-agent/src/ui/app.rs` | NEW — App + TranscriptLine enum |
| `apps/coding-agent/src/ui/draw.rs` | NEW — `ui()` + 4 个 `draw_*` |
| `apps/coding-agent/src/ui/events.rs` | NEW — crossterm Event → App action |
| `apps/coding-agent/src/ui/completion.rs` | NEW — popup 补全 |
| **总计** | **+5 NEW + 3 MODIFY** |

## 验证清单

- [ ] `cargo build -p yushan-coding-agent --release` 通过
- [ ] `cargo test -p yushan-coding-agent` 全绿（含 TestBackend 单测）
- [ ] binary size < 7MB release
- [ ] 手动：启动显示三栏布局；turn 完成 transcript 滚动；Ctrl-C 中断 turn；Tab 触发补全
- [ ] `yushan-coding-agent --no-tui` 切回 rustyline 路径且功能不变
- [ ] 60fps 下无卡顿（agent turn 在 select 内独立运行）
- [ ] follow mode：默认滚到底，Up 滚动后禁用 follow，End 恢复

## 未澄清问题回答（user 已决策）

| # | 问题 | 答案 |
|---|------|------|
| 1 | ratatui 版本 | **0.29 + crossterm 0.28 + 升级 inquire 到 0.8** |
| 2 | 保留 rustyline fallback | **是**（`--no-tui` flag） |
| 3 | transcript 滚动 | **默认 follow + 滚动手动禁用** |
| 4 | 鼠标支持 | **v0 不要**，但 `events.rs` 预留 `handle_mouse` stub |
| 5 | status panel 宽度 | **30% 固定** |
| 6 | binary size | **+700KB release，可接受**（5.7M → 6.4M） |
| 7 | AppView 字段扩展 | **不扩展**（transcript / scroll / input 归 App） |

## 关联文档

- [`context.md`](./context.md) — 需求上下文
- `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts` — Pi footer 参照
- [ratatui BREAKING-CHANGES](https://docs.rs/crate/ratatui/latest/source/BREAKING-CHANGES.md)
- [inquire issue #252](https://github.com/mikaelmello/inquire/issues/252)

## ADR

详见 [`adr-ratatui-replace.md`](./adr-ratatui-replace.md)。
