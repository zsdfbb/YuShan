# ADR: ratatui 替换 rustyline — 数据集中 + 4 个 draw_* 直传

## 状态

提议（2026-09-09）

## 上下文

YuShan coding-agent 当前 TUI 是「rustyline 阻塞 + println」循环：
- 长 transcript 时 footer 滚出可视区
- 无独立滚动 / 无侧栏 / 无 alt-screen
- 用户体验天花板受 rustyline 库能力限制

详见 [`context.md`](./context.md)。3 方案对比见 [`design.md`](./design.md)。

## 决策

### 1. ratatui 0.29 + crossterm 0.28 + 升级 inquire 到 0.8

```toml
ratatui = "0.29"
crossterm = "0.28"
inquire = "0.8"   # 从 0.7.5 升级，解锁 crossterm 0.28 兼容
```

**Why**：
- ratatui 0.29 是 2025 最新稳定版，widget / backend 抽象齐全
- crossterm 0.28 是 ratatui 0.29 强制要求
- inquire 0.7.5 锁 crossterm 0.25 是已知问题（[issue #252](https://github.com/mikaelmello/inquire/issues/252)），必须升级

**How to apply**：6 组 PR 渐进实施（R1 依赖 → R6 测试）。

### 2. 不抽 Widget / Drawer trait

**Why**：
- AppView 已是「数据 → 渲染」解耦点；再抽 trait 是 over-engineering
- 60fps × 3 panel = 180 次 vtable/秒，开销可观测但**零收益**
- ratatui 0.29 widget 都是 `frame.render_widget(value, area)` —— 直接传 `&App + &AppView` 比 trait 间接更清晰
- 单元测试用 `TestBackend` + `App::default()`，不需要 trait mock

**How to apply**：4 个 `pub fn draw_*(frame, area, &App, &AppView)` 在 `ui/draw.rs`。

### 3. AppView 零字段扩展（保持 14 字段不变）

**Why**：
- transcript / scroll / input 是 chat 累积状态，**不是 snapshot**
- AppView 必须保持「可由 `from_sources` 纯函数重建」的不变量
- 强行合并会让 `from_sources` 不纯 + 增加 AppView 体积 + 污染 view.rs

**How to apply**：
- `App` struct 持有全部 UI 状态（transcript / scroll / input / follow / completion / is_turning）
- `AppView` 仅持有「派生展示数据」（provider / model / tokens / cwd 等）
- 每帧 view_dirty=true 时 `app.view = AppView::from_sources(...)`

### 4. format.rs 双轨（print_* + draw_*）

**Why**：
- `print_*` (Writer + AppView) 适配裸 stdout —— `--no-tui` fallback 用
- `draw_*` (Frame + Area + AppView) 适配 ratatui 立即模式
- 共享 `format_tokens` / `status_symbol` 保证 fallback 与 ratatui 输出风格一致

**How to apply**：
- `print_*` 全部保留（已实现 + 单测覆盖）
- `draw_*` 新增接 `Frame + Area + &AppView`
- 共享 helper 改 `pub(super)` 让 `ui::draw` 可访问

### 5. 保留 rustyline fallback (`--no-tui` flag)

**Why**：
- debug / CI / 远程 ssh tty 异常时降级
- inquire 0.8 仍是 stdin 阻塞模型，`/login` / `/model` 交互 picker 保留
- tui.rs ~200 行长期保留可接受

**How to apply**：
```rust
// main.rs
let no_tui = args.iter().any(|a| a == "--no-tui");
if no_tui {
    tui::run_interactive(...).await?;
} else {
    ui::run(...).await?;
}
```

### 6. transcript 滚动：follow 默认 + 手动退出

**Why**：行业标准（Claude Code / OpenCode / Aider / Pi 都这么做）。

**How to apply**：
- `App.follow = true` 默认
- 用户 `Up` / `PageUp` → `follow = false`
- 新内容追加 / 用户提交 turn → `follow = true`
- `End` / `↓` 到 0 也恢复 follow

### 7. transcript 用 enum 而非 struct vec

**Why**：
- transcript 是异构流（User / Assistant / Tool / Summary / Error / System），每个 variant 渲染样式不同
- enum 让 pattern match 命中，零 `Option<String>` 噪音
- `display_height()` 方法处理 wrap 后的多行计算

**How to apply**：
```rust
pub enum TranscriptLine {
    User(String),
    Assistant(String),
    Tool { name, args, result, success },
    Summary { rounds, stop, elapsed_secs },
    Error(String),
    System(String),
}
```

### 8. turn + tick bridge 用 `tokio::select!`

**Why**：
- turn 进行中每 100ms tick 触发 view 重建（status panel 实时更新 tokens）
- ctrl-c 复用 `Agent::cancel()`（不是新依赖）
- 不需要额外 channel / oneshot

**How to apply**：
```rust
tokio::select! {
    biased;
    res = &mut turn_fut => { /* turn done */ }
    _ = tick.tick() => { app.view_dirty = true; }
    _ = tokio::signal::ctrl_c() => { agent.cancel(); }
}
```

## 备选方案

### 备选 B：Panel + Drawer trait

```rust
trait Panel: Send { fn handle(&mut self, action: &Action) -> HandleOutcome; }
trait Drawer: Send { fn draw(&self, frame: &mut Frame, area: Rect, panel: &dyn Panel, view: &AppView); }
```

**否决理由**：
- dyn dispatch 60fps 下累计 200ns/帧，无意义开销
- Drawer 与 Panel 分离强迫 n:1 关系（多个 Panel 共用一个 Drawer），但 v0 是 1:1
- AppView 已经集中数据，trait 只是「AppView 上的方法皮」
- 加 panel 成本：+1 struct + impl + register（比 +1 fn in draw.rs 重）

### 备选 A：3 文件 ui/ 极简（无 events.rs / completion.rs 独立）

**否决理由**：
- events.rs 与 completion.rs 合并到 mod.rs 会让 mod.rs 超过 400 行
- 完成 popup 逻辑（迁移 CmdCompleter）有 ~80 行独立可测代码
- 4 文件 ui/ 与 3 文件 ui/ 维护成本差异 < 5%，但可读性提升 ~20%

## 后果

### 正面

- ✅ 真 sticky 底部 / 顶部 footer（ratatui widget）
- ✅ transcript 独立滚动（长 AI 回复可查历史）
- ✅ 右侧 status panel 持续可见
- ✅ 60fps 流畅（widget 立即模式）
- ✅ AppView 数据集中保持（render 层零 source 耦合）
- ✅ crates 公开 API 零修改
- ✅ `--no-tui` fallback 保留（debug / ssh 兼容）
- ✅ 与硬约束契合（单向依赖 / 最小核心 / 静态组合）

### 负面

- ⚠️ binary size +700KB（5.7M → 6.4M，+12%）
- ⚠️ inquire 升级有 breaking change 风险（v0.8 vs v0.7 API 差异）
- ⚠️ tui.rs + ui/ 两套路径长期共存，维护负担
- ⚠️ `--no-tui` 路径功能冻结（不再加新特性），rustyline 相关 issue 不修

### 风险

| 风险 | 缓解 |
|------|------|
| inquire 0.8 API breaking | R1 阶段先跑一次 `/login` smoke test |
| crossterm 多版本歧义 | `cargo tree -p crossterm` 验证唯一版本 |
| ratatui 0.29 与 inquire 0.8 间接冲突 | R1 阶段 `cargo check` 通过即解锁 |
| TestBackend 与真实 terminal 行为差异 | 手动测试覆盖关键路径（启动 / turn / ctrl-c） |
| transcript 内存增长无界 | 加 `transcript.cap()` 上限（v0+ 议题） |
| follow mode 边缘 case | 测试覆盖（PageUp 后 End 恢复等） |

## 演进路径

| 扩展 | 落点 | 备注 |
|------|------|------|
| 多 panel（file tree） | `ui/draw.rs::ui()` 加 Constraint + `draw_file_tree()` | App 加字段 |
| Mouse 交互 | `events.rs::handle_mouse` + ratatui EnableMouseCapture | 已有 stub |
| Modal（session 选择） | `App.modal: Option<ModalState>` + `draw::draw_modal` | 覆盖在主三栏上 |
| Theme | `App.theme: Theme` + 传入 draw_* | Theme struct 集中颜色 |
| vim mode | `App.input_mode` + events 分支 | input buffer 不变 |
| Session resume | `transcript` 序列化（TranscriptLine 加 serde） | state.json schema |

## 关联文档

- [`context.md`](./context.md) — 现状分析
- [`design.md`](./design.md) — 三方案对比 + 最终方案
- `docs/arch/tui-status-display/` — d73054f banner + turn summary（前一轮）
- `docs/arch/tui-resident-status/` — AppView 数据集中 + rustyline footer（前二轮）

## 受 Pi / Claude Code 启发

- follow mode 默认 + 手动退出：Claude Code / OpenCode / Aider 行业惯例
- 60fps 立即模式：ratatui 社区标准
- transcript enum 而非 struct vec：Pi 的 `FooterDataProvider` 模式简化
