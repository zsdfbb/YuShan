# TUI 改进架构设计 — 常驻 footer + state.json + P1/P2/P3

## 概述

3 个并行 subagent 基于 [`context.md`](./context.md) + [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md) 给出三种解耦立场的方案。**user 特别强调「考虑项目的解耦问题」**——本文档从「解耦质量」维度对比择优。

## 三方案概览

| 方案 | 核心思想 | 解耦立场 | 新增抽象 | 性能开销 |
|------|---------|---------|---------|---------|
| **A** | 直接扩展 | **零抽象**——加 pub fn getter 就完事 | 0 trait | 零 |
| **B** | Trait 屏障 | 用 `StatusSource`/`ProviderMeta` trait 屏障具体类型 | 3 trait + 2 value type | dyn dispatch |
| **C** | 数据集中 | `AppView` 结构体聚合所有展示状态，单向数据流 | 1 struct + 1 view type | ~10 次 clone/rebuild |

## 对比矩阵（解耦为核心维度）

| 维度 | A 直接扩展 | B Trait 屏障 | C 数据集中 |
|------|----------|------------|----------|
| **解耦 render 与 sources** | ❌ render 仍吃 Config/Agent | ✅ 通过 trait 屏障 | ✅ render 只吃 AppView |
| **加新字段的改动范围** | format 签名 + 调用方 | format + trait method | AppView 字段 + from_sources |
| **format.rs 单测** | 构造 Config + Agent | 构造 FooterSnapshot | 构造 AppView::default() |
| **头less / IDE 复用** | ❌ print_* 强耦合 | ✅ 纯 Writer + Snapshot | ✅ 纯 Writer + AppView |
| **Agent 公开 API 改动** | 3 getter + 1 setter 暴露给所有调用方 | 同上但**仅 AppContext 内部使用** | 同上但**仅 from_sources 内部使用** |
| **概念数量** | +0 | +3 trait + +2 value type + +1 AppContext | +1 AppView + +1 CommandMeta |
| **运行时开销** | 零 | dyn dispatch（每个 render 1 次虚调用） | ~10 次 clone/rebuild（< 1μs） |
| **学习成本** | 低 | 中（多 1 个抽象层） | 中（多 1 个 view 类型） |
| **演进收敛性** | 中 | 高 | 高 |

### Back-of-envelope

| 项 | A | B | C |
|---|----|----|----|
| render 调用频率 | N/turn | N/turn | N/turn |
| 单次 render 开销 | 字符串拼接 | 字符串拼接 + 1 dyn call | 字符串拼接 + ~10 clone |
| 单 turn 总开销（5 render） | ~5μs | ~10μs（dyn） | ~15μs（clone） |
| 在 LLM 调用（秒级）背景下的占比 | 不可观测 | 不可观测 | 不可观测 |

**结论**：性能差异全部不可观测，选型应基于「解耦质量 + 演进成本」。

### 风险

| 风险 | 出现于 | 影响 |
|------|--------|------|
| 抽象过度，徒增学习成本 | B | trait 过多，未来读者困惑 |
| AppView 字段膨胀失控 | C | struct 字段超过 20 个时难维护 |
| Agent API 暴露面失控 | A | 所有调用方都能看见 getter，破坏封装 |
| Config "上帝对象" 化 | A 倾向 | CommandContext 塞满字段 |

---

## 推荐方案：**C — 数据集中 + 单向流（融合 A 的最小扩展风格）**

### 决策

**采纳 C 的核心（AppView 数据集中）+ A 的精简风格（不加 trait、不加 ProviderMeta）**：

1. **新增 `view.rs` 模块**：定义 `AppView` 结构体 + `AppView::from_sources(&Config, &Agent, &ProviderRegistry, &StateStore, &TurnStats, Instant)` 构造器
2. **format.rs 重构为纯函数**：所有 print_* 改为接收 `&AppView`，不再吃 Config/Agent
3. **CommandContext 加 view 字段**：`commands` 仍直接改 sources（避免反向 sync 复杂度），main 循环 rebuild view 自动反映
4. **Agent 公开 API 扩展最小集**（同 A）：`tool_names()` / `context_window()` / `cancel()` 3 个 getter + 1 个 action
5. **StateStore 与 ProviderRegistry 保持独立**：同 A 的论证——职责、敏感性、演进都不同

### 不采纳 B 的原因

B 引入 3 个 trait（`StatusSource` / `ProviderMeta` / `PersistentState`）+ 2 个 value type（`FooterSnapshot` / `StatusSnapshot`）+ 1 个 AppContext：

- **trait 多态的零成本优势在 Rust 里不成立**：dyn dispatch 不是单态化，每次调用都有虚表查找（v0 场景不可观测，但 v1/v2 可能）
- **`StatusSource` 与 `AppView` 是重复抽象**：B 的 `StatusSource::snapshot()` 返回 `FooterSnapshot` value type，与 C 的 `AppView::from_sources(...) -> AppView` 完全等价，但 trait 抽象多一层间接
- **`ProviderMeta` trait 是过度抽象**：3 个 getter 加 `&ProviderRegistry` 即可，`format.rs` 通过 trait 屏障屏蔽 `ProviderRegistry` 的可变方法——但 format 本来就是只读，加 trait 收益低
- **`PersistentState` trait 完全不必要**：B 的 subagent 自己承认"成本 > 收益"，已砍掉

### 不采纳纯 A 的原因

A 不引入 AppView 抽象：

- format.rs 仍吃 `Config` / `Agent`——加字段必须改 print 签名 + 改调用方（每次 2 处改动）
- 加 headless / IDE 复用时，print_* 函数无法脱离 Config 单独工作
- **演进收敛性差**：每次加字段都扩散到多个 print 函数

### 融合 C 的关键调整

C 原方案有 1 个问题：`from_sources` 涉及 ~10 次 clone，每次 print 前 rebuild。我的建议：

**只在「状态确实改变」时 rebuild view**，平时复用：

```rust
// main.rs
let mut view = AppView::from_sources(...); // 启动时 1 次

// tui.rs 主循环
loop {
    // 每轮开头：若 view 标记为 dirty 则 rebuild，否则直接用
    if view_dirty { view = AppView::from_sources(...); }
    format::print_footer(&mut out, &view)?;
    
    let line = rl.readline("> ")?;
    
    if command_input {
        commands.execute(...);  // 修改 sources
        view_dirty = true;      // 标记
    } else {
        agent.run_turn(...);
        stats.record(...);
        view_dirty = true;
    }
}
```

**收益**：避免每轮不必要 rebuild；性能完全等价 A。**代价**：多了 1 个 bool flag，但消除了 C 最大的性能异议。

---

## 最终结构

```
apps/coding-agent/src/
├── main.rs              [MODIFY] — 加载 StateStore；构造 view；恢复升级
├── view.rs              [NEW]    — AppView + from_sources + session_duration_str()
├── state.rs             [NEW]    — AppState + StateStore（独立于 auth.json）
├── format.rs            [MODIFY] — 所有 print_* 改为接收 &AppView
├── tui.rs               [MODIFY] — 持有 view_dirty；rustyline 替换 read_line；Ctrl-C
├── tui/
│   └── completer.rs     [NEW]    — CmdCompleter (rustyline)
├── prompt.rs            — 不变（format_cwd_tilde 已被 view 借用）
├── config.rs            — 不变
├── provider.rs          [MODIFY] — pub auth_path()/logged_in_count()/logged_in_providers()
├── status.rs            — 不变（TurnStats 是数据源之一，被 AppView 借用）
├── commands/
│   ├── mod.rs           [MODIFY] — CommandContext 加 view/state 字段
│   └── builtin.rs       [MODIFY] — Login/Model/Logout 写 state.json；输出精简；/status 改用 ctx.view
└── Cargo.toml           [MODIFY] — 加 rustyline = "14"

crates/agent-runtime/src/
└── agent.rs             [MODIFY] — pub tool_names()/context_window()/cancel()

crates/agent-tool/src/
└── registry.rs          [MODIFY] — pub fn names(&self) -> Vec<&str>（基于 specs 缓存，避免每次 clone String）

docs/adr/
└── 0007-agent-public-api.md   [NEW] — 标注 Agent API 扩展边界
```

---

## 关键数据结构

```rust
// apps/coding-agent/src/view.rs

#[derive(Clone, Debug)]
pub struct AppView {
    // 身份
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub config_path: PathBuf,
    pub logged_in_providers: Vec<String>,
    pub total_known_providers: usize,
    pub version: &'static str,
    
    // token 累计
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
    
    // session
    pub session_started: Instant,
    pub message_count: usize,
    
    // capabilities
    pub tools: Vec<String>,        // owned String, 来自 Agent::tool_names() (ToolSpec.name)
    pub context_window: Option<usize>,
    pub is_first_run: bool,

    // commands (for /help + completer)
    pub commands: Vec<CommandMeta>,
}

#[derive(Clone, Debug)]
pub struct CommandMeta {
    // &'static str 是合理的：Command 是 'static trait object，
    // name()/description()/arg_hint() 返回 &'static str（来自 impl 常量）
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
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
    
    pub fn session_duration_str(&self) -> String { ... }
}
```

```rust
// apps/coding-agent/src/format.rs (新签名)

pub fn print_banner<W: Write>(out: &mut W, view: &AppView) -> io::Result<()>;
pub fn print_footer<W: Write>(out: &mut W, view: &AppView) -> io::Result<()>;
pub fn print_turn_summary<W: Write>(
    out: &mut W,
    view: &AppView,
    rounds: u32,
    stop: &StopReason,
    elapsed_secs: f32,
) -> io::Result<()>;
pub fn render_status<W: Write>(out: &mut W, view: &AppView) -> io::Result<()>;
```

```rust
// apps/coding-agent/src/commands/mod.rs (改造)

pub struct CommandContext<'a> {
    pub agent: &'a mut Agent,
    pub config: &'a mut Config,
    pub state: &'a mut StateStore,        // 新
    pub stats: &'a mut TurnStats,         // 新
    pub view: &'a mut AppView,            // 新（commands 改 sources 后由 main rebuild；commands 可直接读 view）
}
```

---

## 关键数据流

### 启动恢复

```
main.rs
├─ Config::from_env()
├─ registry.load_auth()                     ──▶ AuthStore
├─ state_store.load()                       ──▶ AppState
├─ if !is_configured():
│    ├─ 优先 state.last_active_provider+auth_for → 恢复
│    └─ fallback: 第一个有 auth 的 provider
├─ session_started = Instant::now()
└─ view = AppView::from_sources(..., session_started, commands)
```

### Turn 循环

```
tui.rs
├─ view_dirty = true (启动)
loop {
    if view_dirty { view = AppView::from_sources(...) }
    
    format::print_footer(&mut out, &view)?;
    
    line = rl.readline("> ")?                  // rustyline + history + completer
    
    if command: commands.execute(args, &mut ctx)   // ctx 含 view，命令可读
        view_dirty = true
    
    else:
        let t0 = Instant::now()
        result = tokio::select! {
            r = agent.run_turn(input) => Some(r),
            _ = tokio::signal::ctrl_c() => { agent.cancel(); None }
        }
        stats.record(usage)
        view_dirty = true
        
        if Some(Ok(r)):
            format::print_turn_summary(view, elapsed=t0.elapsed, ...)
}
```

### 命令写 state

```
LoginCommand::execute
├─ registry.save_auth(provider, AuthEntry{...})  ──▶ auth.json
├─ config.{api_base,api_key,model,provider} = ...
├─ agent.set_model(Some(build_model))
└─ ctx.state.save(&AppState{
      last_active_provider: Some(provider.name),
      last_active_model:    Some(model_name),
  })                                             ──▶ state.json
```

---

## 解耦评估

### Agent 公开 API 改动（4 个）

| API | 必须？ | 论证 |
|-----|--------|------|
| `pub fn tool_names(&self) -> Vec<String>` | 是 | banner 加工具列表（P1-4）；返回 owned `String` 而非 `&'static str`，因为 `ToolSpec.name`（`crates/agent-tool/src/spec.rs:7`）类型是 `String`——不可借用 `&'static`。实现：`self.registry.specs().iter().map(\|s\| s.name.clone()).collect()`，或先在 `ToolRegistry` 新增 `pub fn names(&self) -> Vec<&str>` 返回 `&str`（基于 `specs` 缓存的 `name` 字段借用） |
| `pub fn context_window(&self) -> usize` | 是 | context 占比（P2-9）；RunLimits 已在 Component 层公开 |
| `pub fn cancel(&mut self)` | 是 | Ctrl-C（P1-14）；不暴露 `&mut CancelToken` 是因为 cancel 是用户意图触发，不该让 TUI 持有 token 长期借用。`CancelToken` 本身是 `Clone` 的（`Arc<AtomicBool>`），TUI 也可调用 `cancel_token().cancel()`——但需要新增 getter。本方案选 `Agent::cancel()` 单一入口 |
| `pub fn session_messages(&self) -> &[Message]` | 已有 | 复用 |

**零 mut 字段暴露**。TUI 拿到的是只读视图。

### format.rs 与 Config 的耦合

**当前**：format.rs 已吃 Config（`print_banner` / `render_status`）。

**改进**：所有 print_* 改为只吃 `&AppView`。`AppView` 已经是「从 Config + Agent + ... 拼装出的快照」，format.rs 不知道 Config 存在。

**测试**：`format.rs` 单测只需构造 `AppView::default()` + 设字段。

### StateStore 与 ProviderRegistry 边界

**不合并**——理由同 A：
1. auth.json 含 API key（0o600），state.json 无敏感数据（合并后策略无法分开演进）
2. ProviderRegistry 是 Config 字段（业务 = provider 目录）；StateStore 与 Config 平级（TUI 会话状态）
3. 演进独立：state 可能加 last_session_id/pinned_providers，auth 不需要

**抽象时机**：若第三种持久化文件出现（如 sessions/），再抽 paths.rs。YAGNI。

### tui.rs 与多模块的耦合

| 耦合对象 | 形式 | 备注 |
|---------|------|------|
| `Agent` | `&mut` | 必须（run_turn + cancel） |
| `Config` | `&mut` | 必须（cwd/provider/model 改） |
| `CommandRegistry` | `&` | 必须（命令分发） |
| `StateStore` | `&` | 启动时 load；commands 通过 ctx.save |
| `TurnStats` | `&mut` | 必须（footer + summary） |
| `Instant` (session_started) | owned | 必须（session 时长） |
| `CmdCompleter` | owned | rustyline 持有 |

**StateStore 通过 CommandContext 间接**——TUI 不直接调 state_store.save()，只有命令写。

---

## 演进路径

| 未来需求 | 改动范围 |
|---------|---------|
| **新 footer 字段**（git branch / active skill） | `AppView` 加字段 + `from_sources` 收集 + format 读——**3 处局部** |
| **新持久化**（theme.json / sessions/） | 新 store 类型；`AppView` 字段 + `from_sources` 读 + format 渲染 |
| **新命令** | `builtin_help_entries()` 加 entry + `build_registry()` 注册 + `AppView::from_sources` 自动捕获——**2 处** |
| **headless 模式** | `format.rs` 是 `Writer + AppView`，直接复用 `print_footer(&mut sink, &view)`——**0 处** |
| **Config 拆分** | format.rs 不依赖 Config 任何字段（只读 AppView），拆分零代价 |
| **Agent 字段改名**（model_id → active_model） | `from_sources` 内部 1 行——**0 扩散** |

**关键不变量**：Agent 公开 API 在本轮后**收敛**——`tool_names` / `context_window` / `cancel` 是终端集合，未来加 footer 字段不需扩展 Agent。

---

## 关键场景

### 场景 1 — 启动 + 重启恢复

```
$ yushan-coding-agent
YuShan Coding Agent v0.1.0
Provider: minimax | Model: abab-7-chat | Dir: ~/Develop/YuShan
Tools:     read, write, edit, bash
Config:    ~/.yushan/auth.json (3 providers, 2 logged in)
Type /help for commands, 'exit' to quit

┌─ ~/Develop/YuShan · minimax · abab-7-chat · ↑0 ↓0 · 0s ─┐
> _
```

（state.json 恢复 minimax/abab-7）

### 场景 2 — Turn 完成 + footer 更新

```
> 解释 main.rs

[AI 回复...]

✓ 1 round · ↑320 ↓1.2k tokens · 2.3s
┌─ ~/Develop/YuShan · minimax · abab-7-chat · ↑320 ↓1.2k · 4s ─┐
> _
```

### 场景 3 — 命令补全

```
> /mo█
     ↓ Tab
> /model █
   ──── hint: "Show or switch the current model" ────
```

### 场景 4 — Ctrl-C 中断

```
> 编译并修复错误
⏳ Working...
  → bash: cargo build
  ← result (failed: exit 1)
  → read: Cargo.toml
  ← result (ok, 47 lines)
[用户按 Ctrl-C]

✗ Cancelled · 2 rounds · ↑1.2k ↓340 tokens · 5.1s
┌─ ~/Develop/YuShan · minimax · abab-7-chat · ↑1.2k ↓340 · 12s ─┐
> _
```

---

## 落地分组（独立 PR）

| 组 | 工作量 | 包含 | 依赖 |
|----|--------|------|------|
| **A** | 0.5d | view.rs 骨架 + format.rs 改签名 + 第一次 banner/footer 落地 | view.rs |
| **B** | 1d | state.rs + StateStore + main.rs 恢复升级 + commands 写 state + 常驻 footer | A |
| **C** | 0.5d | Agent 公开 API 扩展（tool_names / context_window / cancel）+ ADR-0007 | 独立 |
| **D** | 1d | rustyline + history + CmdCompleter + Ctrl-C | C（需要 cancel） |
| **E** | 0.5d | P1/P2/P3 其余（turn 耗时、错误来源、welcome、/help 增强、context %、session 计时） | A |
| **F** | 0.3d | inline ANSI 颜色（P3-17） | A |

**总计**：~4d，6 个独立 PR。

---

## 实施 checklist（review.md R4/R6 缓解）

### Code doc comment 模板

在 `apps/coding-agent/src/commands/mod.rs::CommandContext` 字段处加：

```rust
/// Commands modify sources (Config / Agent / StateStore) directly — this is
/// the "mut 意图载体" layer. `view` is for **READ ONLY** — to avoid stale-data
/// bugs, never sync back from view to sources.
///
/// Why not bidirectional: double-write paths always have one stale side.
/// Sources-of-truth are Config / Agent / StateStore; view is rebuilt from
/// them by `AppView::from_sources` whenever `view_dirty = true` (set by the
/// main loop after any command execution).
pub view: &'a mut AppView,
```

### PR review checklist（每组 PR 必查）

- [ ] **任何修改 `Config` / `Agent` / `StateStore` 的命令** → 必须设 `view_dirty = true`（在 tui 主循环中）
- [ ] **任何修改 `Config.model` 的命令**（LoginCommand / ModelCommand） → 必须 `state.save(...)` 写 `state.json`
- [ ] **任何修改 `Config.api_key/api_base/provider` 的命令**（LoginCommand / LogoutCommand） → 必须 `state.save(...)` 清/写 `last_active_provider/model`
- [ ] **新加的 print_* 函数** → 签名接 `&AppView`，**不**接 `&Config` / `&Agent`
- [ ] **AppView 新加字段** → 同时改 `from_sources` 收集点 + 至少 1 个 format 渲染分支
- [ ] **Agent 公开 API 新加方法** → 必须有 ADR-0007 引用，否则不允许

### 单元测试要求（每组 PR 必跑）

- **A 组**：format::print_banner / print_footer / print_turn_summary / render_status 各 2 个 case（构造 AppView::default() + 设字段，断言输出）
- **B 组**：StateStore::load / save 完整 round-trip；main.rs 恢复逻辑的 mock 测试
- **C 组**：Agent::tool_names / context_window / cancel 的 mock 测试
- **D 组**：CmdCompleter prefix match（输入 `/mo` 返回 `/model`）；rustyline 集成 smoke test
- **E 组**：turn elapsed 误差 < 50ms；/status 输出含 tokens
- **F 组**：ANSI helper 在 `NO_COLOR=1` env 下不输出 escape

### 已知遗留（review.md 标记）

- AppView 字段数 v0 = 14；若超过 20 字段，按下表分组重构（**当前不分组**）：
  - `view.identity` { cwd, provider, model, config_path, logged_in_providers, version }
  - `view.session` { session_started, message_count, is_first_run }
  - `view.stats` { total_input_tokens, total_output_tokens, turn_count }
  - `view.capabilities` { tools, context_window }
  - `view.commands` { commands }
- 终端宽度截断：v0 不实现（footer 长度 < 80 字符足够大多数终端）
- `/status` 在 CommandContext 没拿到 `&mut TurnStats`（v0 决策）—— 下轮把 stats 纳入 CommandContext 或重构为 view-only

---

## 被否方案

### B（Trait 屏障）

**否决理由**：
- B 的 `StatusSource` 与 C 的 `AppView::from_sources(...) -> AppView` 完全等价——C 用结构体 + 函数实现同样解耦，零运行时开销
- B 的 `ProviderMeta` trait 是过度抽象：3 个 getter 加 `&ProviderRegistry` 已足够
- B 的 `PersistentState` trait 已被 B 的 subagent 自己承认"成本 > 收益"砍掉
- Rust dyn dispatch 不是零成本，且 trait object 不能放 `Vec` / `String`（只能 `&dyn`）——实际写起来噪声大

### A（直接扩展）

**否决理由**：
- format.rs 仍吃 Config/Agent——加字段必须改 print 签名 + 改调用方（每次 2 处）
- 无法 headless 复用——print_* 函数强耦合 Config
- 演进收敛性差：每加字段都扩散

---

## ADR 决策记录

详见 [`adr-tui-resident-status.md`](./adr-tui-resident-status.md)。

关键决策：
1. **view.rs 作为唯一展示数据源**——render 层零 source 耦合
2. **format.rs 重构为纯函数**——`Writer + AppView`，无 Config/Agent 依赖
3. **StateStore 与 ProviderRegistry 独立**——职责、安全、演进不同
4. **Agent API 扩展 3 个 getter + 1 个 action**——零 mut 字段暴露
5. **view rebuild 仅在 view_dirty 时**——避免无谓 clone
6. **commands 仍直接改 sources**——避免 view 反向 sync 复杂度
7. **rustyline 与 Agent::cancel 一起引入**——Ctrl-C 依赖

---

## 关联文档

- [`context.md`](./context.md) — 需求上下文
- [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md) — 11 项 P1-P2-P3 详细
- [`adr-tui-resident-status.md`](./adr-tui-resident-status.md) — 决策记录
- `docs/arch/tui-status-display/` — d73054f 已实现的 banner + turn summary
- `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts` — Pi footer 参照
