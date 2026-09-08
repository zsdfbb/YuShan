# ADR: TUI 改进架构 — 数据集中 + 单向流

## 状态

提议（2026-09-08）

## 上下文

YuShan coding-agent TUI 三波改进需求：

1. **常驻状态栏** — 每次 prompt 前打印一行 footer（cwd · provider · model · 累计 tokens）
2. **持久化 last_active** — 启动时恢复上次 provider + model
3. **P1/P2/P3 改进** — 11 项低成本改进（banner 加字段、命令输出精简、Ctrl-C、turn 耗时、context %、输入补全等）

完整背景见 [`context.md`](./context.md) + [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md)。三方案对比见 [`design.md`](./design.md)。

**user 特别强调：考虑项目的解耦问题**。

## 决策

### 1. 新增 `AppView` 作为唯一展示数据源

```rust
// apps/coding-agent/src/view.rs
pub struct AppView {
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub config_path: PathBuf,
    pub logged_in_providers: Vec<String>,
    pub tools: Vec<String>,        // owned String, 来自 ToolSpec.name
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
    pub session_started: Instant,
    pub message_count: usize,
    pub context_window: Option<usize>,
    pub is_first_run: bool,
    pub version: &'static str,
    pub commands: Vec<CommandMeta>,
}

impl AppView {
    pub fn from_sources(
        cfg: &Config,
        agent: &Agent,
        registry: &ProviderRegistry,
        state: &StateStore,
        stats: &TurnStats,
        session_started: Instant,
        commands: Vec<CommandMeta>,
    ) -> Self { ... }
}
```

**Why**：
- format.rs / tui.rs / completer 全部只读 `&AppView`，**零 source 耦合**
- 加字段只需 3 处局部：AppView 字段 + from_sources 收集 + format 渲染分支
- headless / IDE 复用：format.rs 是 `Writer + AppView`，直接调 `print_footer(&mut sink, &view)`
- 单测：构造 `AppView::default()` + 设字段，不需 Config/Agent

**How to apply**：
- 命令仍直接改 sources（Config/Agent/StateStore），**不允许 view 反向 sync**——双写路径会引入 stale view bug
- view rebuild 仅在 `view_dirty` 标志位为 true 时触发（避免每轮不必要 clone）
- AppView 内数据全部 owned（PathBuf/String/Vec）—— 不持 source 引用，避免 lifetime 复杂化

### 2. format.rs 重构为纯函数

```rust
pub fn print_banner<W: Write>(out: &mut W, view: &AppView) -> io::Result<()>;
pub fn print_footer<W: Write>(out: &mut W, view: &AppView) -> io::Result<()>;
pub fn print_turn_summary<W: Write>(out: &mut W, view: &AppView, rounds: u32, stop: &StopReason, elapsed_secs: f32) -> io::Result<()>;
pub fn render_status<W: Write>(out: &mut W, view: &AppView) -> io::Result<()>;
```

**Why**：
- format 模块职责是「**怎么排版**」，不应是「**从哪取数**」
- 之前 format 吃 Config 是「启动 banner 时期」的简化，现在数据源多了（Stats / Agent / ProviderRegistry），不抽象会持续恶化
- 纯函数可单测、可复用、可并行（未来）

**How to apply**：
- 所有 print_* 都接 `&AppView`；不再接 Config/Agent/Stats
- 私有辅助函数（format_tokens / status_symbol）保留

### 3. StateStore 与 ProviderRegistry 保持独立

**两个文件、两个模块、不同的写入策略**：

| 文件 | 模块 | 写入者 | 内容 |
|------|------|--------|------|
| `~/.yushan/auth.json` | ProviderRegistry | /login、/logout | `HashMap<provider, AuthEntry{api_base, api_key, model}>` |
| `~/.yushan/state.json` | StateStore | /login、/model、/logout | `AppState{last_active_provider, last_active_model}` |

**Why 不合并**：
- auth.json 含 API key（必须 0o600）；state.json 无敏感数据。合并后安全策略无法分开演进
- ProviderRegistry 是 Config 字段（业务 = provider 目录）；StateStore 与 Config 平级（TUI 会话状态）。类型系统中**不应等价**
- 演进独立：state 可能加 `last_session_id`/`pinned_providers`；auth 不需要

**How to apply**：
- 两个 store 各自独立 path_override 测试
- 路径策略（HOME/USERPROFILE → ~/.yushan/）当前各写一份——**若第三种持久化文件出现再抽 paths.rs**（YAGNI）

### 4. Agent 公开 API 扩展最小集

```rust
// crates/agent-runtime/src/agent.rs
impl Agent {
    pub fn tool_names(&self) -> Vec<String>;     // ToolSpec.name 是 String, 不能借用 'static
    pub fn context_window(&self) -> usize;
    pub fn cancel(&mut self);
    // 已有：is_configured / model_id / set_model / clear_session / session_messages
}
```

**Why 必须暴露**：
- `tool_names()`：banner 加工具列表（P1-4）。返回 owned `String`——因为 `ToolSpec.name`（`crates/agent-tool/src/spec.rs:7`）类型是 `String`，无法借用 `&'static str`。实现可在 `ToolRegistry` 新增 `pub fn names(&self) -> Vec<&str>` 基于 `specs` 缓存借用，避免每次 clone
- `context_window()`：context 占比（P2-9）。RunLimits 已在 Component 层公开，TUI 拿不到等于把展示层逼回 Agent 内部
- `cancel()`：Ctrl-C（P1-14）。不暴露 `&mut CancelToken` 是因为 cancel 是用户意图触发，不该让 TUI 持有 token 长期借用

**Why 零 mut 字段暴露**：
- TUI 不应绕过 Builder 直接修改 Agent 内部
- 所有 getter 只读；唯一 mut 操作是 `cancel()`（单一意图）
- 维护「agent-runtime 仅暴露只读视图」边界

**How to apply**：
- 本轮新增 3 getter + 1 action 后，Agent 公开 API **收敛**——未来加 footer 字段不再扩 Agent
- 新增 ADR-0007 标注扩展边界（只读快照 vs mut 行为）

### 5. view_dirty 标志位优化 rebuild 时机

```rust
// tui.rs 主循环
let mut view = AppView::from_sources(...); // 启动时 1 次
let mut view_dirty = true;

loop {
    if view_dirty {
        view = AppView::from_sources(...);
        view_dirty = false;
    }
    format::print_footer(&mut out, &view)?;
    
    let line = rl.readline("> ")?;
    
    if command_input {
        commands.execute(...);
        view_dirty = true;  // 命令可能改了 Config/Agent
    } else {
        agent.run_turn(...).await;
        stats.record(usage);
        view_dirty = true;
    }
}
```

**Why**：
- 避免每轮不必要 rebuild（10 次 clone/rebuild 在大多数 turn 是浪费的）
- view 数据源改变后立即 rebuild，保证 footer / summary 反映最新状态
- 零额外状态（仅 1 个 bool flag）

**How to apply**：
- view_dirty 标志是「保守」的——任何可能改 sources 的操作都设 true
- rebuild 失败（极端情况）当前 panic——未来可加 Result 链

### 6. commands 仍直接改 sources，不通过 view 反向 sync

```rust
// commands/builtin.rs::LoginCommand
ctx.config.api_base = Some(api_base.clone());    // 直接改 source
ctx.config.model = model_name.clone();
ctx.agent.set_model(Some(model));

ctx.state.save(&AppState { ... });               // 写 state.json

// view 由 main 循环在下一轮 rebuild 时自动捕获最新 Config
```

**Why 不通过 view 反向写 sources**：
- view 是「展示快照」，命令是「mut 意图载体」——语义层不同
- 让命令改 view 字段 + view 反向 sync sources → 双写路径，必然有 stale bug
- 已有 commands 直接改 Config/Agent 的代码全部保留，零改动

**How to apply**：
- CommandContext 加 `view: &mut AppView` 字段仅用于**读**（如 /status 输出）
- commands 不修改 view 字段——只修改 sources（Config/Agent/StateStore）
- main 循环 rebuild view 捕获最新 sources

### 7. rustyline 与 Agent::cancel 一起引入

**Why**：
- Ctrl-C 处理依赖 Agent::cancel（P1-14）
- rustyline 的 `readline` 在 Ctrl-C 时返回 `Err(ReadlineError::Interrupted)`，**自然处理 Ctrl-C 而不需要 tokio::signal**
- 这两个特性高度耦合——rustyline 引入后才好做 Ctrl-C 设计

**How to apply**：
- Ctrl-C 实现：rustyline `readline` → `Interrupted` → 显示「(press Ctrl-D or type 'exit' to quit)」+ `continue`
- Ctrl-D：rustyline `Eof` → `break`（退出 REPL）
- 长 turn 中 Ctrl-C：通过 `tokio::select!` 同时监听 `run_turn` 和 `ctrl_c` 信号——需要独立的 `cancel_handle` 或 `&mut Agent`

## 备选方案

### 备选 B：Trait 屏障

| 维度 | B | 推荐 C |
|------|---|--------|
| 解耦 render 与 sources | ✅ trait 屏障 | ✅ struct + 函数屏障（等价） |
| 加新字段改动 | format + trait method | AppView 字段 + from_sources |
| 性能 | dyn dispatch | 函数调用 + clone |
| 概念数量 | +3 trait + +2 value type + +1 struct | +1 struct + +1 view type |
| 学习成本 | 中 | 中 |

**否决理由**：B 的 `StatusSource::snapshot() -> FooterSnapshot` 与 C 的 `AppView::from_sources() -> AppView` 完全等价——C 用结构体实现同样解耦且零运行时开销。B 的 `ProviderMeta` trait 是过度抽象：3 个 getter 加 `&ProviderRegistry` 已足够。B 的 `PersistentState` trait 已被 B 的 subagent 承认"成本 > 收益"砍掉。

### 备选 A：直接扩展

**否决理由**：
- format.rs 仍吃 Config/Agent——加字段必须改 print 签名 + 改调用方（每次 2 处）
- 无法 headless 复用——print_* 函数强耦合 Config
- 演进收敛性差：每加字段都扩散

## 后果

### 正面

- ✅ render 层（format / tui / completer）零 source 耦合——加字段只改 3 处
- ✅ format.rs 可独立单测、可被 headless/IDE 复用
- ✅ Agent 公开 API 收敛——本轮后不再扩张
- ✅ StateStore 与 ProviderRegistry 独立演进
- ✅ view_dirty 避免无谓 clone

### 负面

- ⚠️ AppView 字段会持续增长（v0 已有 14 字段）——需 review 时控制粒度
- ⚠️ from_sources 函数变长——可拆分为 helper（如 collect_identity / collect_stats）
- ⚠️ view_dirty 标志位可能漏设某处——靠 review 抓
- ⚠️ AppView 全部 owned 数据，每次 rebuild 是 10+ clone——v0 不优化（成本不可观测）

### 风险

| 风险 | 缓解 |
|------|------|
| AppView 字段膨胀失控 | review 阶段控制粒度；超过 20 字段时重构为分组 struct |
| view_dirty 漏设 | PR review 检查 + 单测覆盖（每个改 sources 的命令设 dirty） |
| Agent API 扩展破坏封装 | ADR-0007 标注；未来扩 API 必须有 ADR |
| 命令改 sources 后忘记 save state.json | 单元测试 + PR review |

## 演进收敛性证明

| 未来加 footer 字段 | 改动文件数 | 是否扩 Agent |
|-------------------|----------|-------------|
| Git branch | 1 新 + 1 改（format.rs 1 行 + tui.rs 调用 + view.rs 加字段） | ❌ |
| Active skill | 0 新 + 2 改（format.rs + view.rs） | ❌ |
| Theme | 1 新 + 2 改（ThemeStore + view.rs + format.rs） | ❌ |
| Headless 模式 | 0 新 + 0 改（format.rs 是纯 Writer） | ❌ |

**结论**：本轮完成后，Agent 公开 API 不再扩张；format 模块已收敛为纯函数层。

## 相关文档

- [`context.md`](./context.md) — 需求上下文
- [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md) — P1/P2/P3 详细清单
- [`design.md`](./design.md) — 三方案对比 + 最终方案

## 受 Pi 启发的来源

- `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts` — Pi footer 字段选择（cwd / tokens / model）
- `tmp/pi/packages/coding-agent/src/core/footer-data-provider.ts` — Pi 的数据源聚合模式（不直接模仿 trait，用 struct + 函数等价）
- `tmp/pi/packages/coding-agent/src/core/usage-totals.ts` — TurnStats 独立于 Usage 演进
