# TUI 状态展示 — 架构方案

## 概述

为 coding-agent TUI 添加启动 banner 和 turn 结束 summary。v0 只展示 4 个字段：**cwd / provider · model / 累计 ↑↓tokens / rounds · stop_reason**。展示层限定在 `apps/coding-agent/` 内，**不动任何 crate 公开 API**。

完整背景见 [`context.md`](./context.md)。Pi 全功能 footer 的对照也在其中。

---

## 三方案概览

| 方案 | 倾向 | 改 tui.rs | 新增模块 | trait/抽象 |
|------|------|----------|---------|-----------|
| **A** | 最小复杂度 | 是（添加累加器+4 函数） | 无 | 无 |
| **B** | 可扩展优先 | 是（添加调用点） | `status/` 子模块（5 文件） | `StatusSource` trait + `StatusField` 枚举 |
| **C** | 资源效率 + 演进 | 是（重写） | `status.rs` + `format.rs` + `collect_sink.rs` | 无（用 `CollectingSink` 包装） |

---

## 对比矩阵

| 维度 | A 最小 | B 可扩展 | C 零分配演进 |
|------|--------|---------|--------------|
| **新增文件数** | 0 | 5（`status/` 子模块） | 3（`status.rs` / `format.rs` / `collect_sink.rs`） |
| **tui.rs 改动行数** | ~40 行 | ~10 行（仅调用点） | ~60 行（重写） |
| **新 trait / 类型** | 1 struct（TurnStats） | 1 trait + 1 enum + 4 struct | 1 struct + 1 wrapper |
| **unsafe** | 无 | 有（`*const Config/Agent` + `unsafe impl Send`） | 无 |
| **运行时开销** | 零 | dyn dispatch + Box 分配 | 零 |
| **加 cache_read 字段时改动** | 改 tui.rs 主体 | 改 formatter 加 match 分支 | 改 format.rs 加字段（tui.rs 不动） |
| **加 git branch 时改动** | 改 tui.rs + 加探测代码 | 新 source 文件 + formatter 分支 | 改 main.rs + format.rs |
| **未来 streaming 状态栏** | 完全重写（NoopEventSink → sink 需要穿透 tui） | 较易（StatusSource 可包含 EventSink） | 易（CollectSink 已就位） |
| **可测性** | 差（依赖 stdout 副作用） | 好（trait 可 mock） | 好（format 接受 `&mut W: IoWrite`） |
| **单测** | 难 | 中（mock source） | 易（用 `Vec<u8>` 作 sink） |
| **新维护者上手** | 直接（一个文件） | 需理解 trait | 需理解 3 个新模块 |
| **~ 行代码** | ~80 | ~300 | ~250 |

### Back-of-envelope

TUI 每秒输出量级：
- Banner：3 行（一次性）
- Summary：1 行 / turn
- 一次 turn 模型延迟通常 1–60s

→ 总文本量 < 100 字节/秒。三种方案在此量级下**性能差异不可观测**，选型应基于「可维护性 + 演进成本」，不是性能。

### 风险

| 风险 | 出现于 | 影响 |
|------|--------|------|
| `unsafe impl Send` 数据竞争 | B | REPL 单线程现状下安全，但语义脆弱；多线程扩展时易出错 |
| StatusField 枚举扩展爆炸 | B | 加字段需改枚举 + 每个 source 的 fields() + formatter match，三处同步 |
| format.rs 单点膨胀 | C | 字段 ≥ 10 时集中模块难维护 |
| `format_cwd_tilde` 与 `prompt.rs` 重复 | A / C | review 阶段核查：prompt.rs:100/148 实际**没有** cwd `~` 缩放逻辑，只有 `HOME` 用于拼 `.yushan/` 路径；A/C 误以为可复用 |

---

## 推荐方案：**A 最小复杂度 + C 的集中格式化**

### 决策

采纳 **A** 的整体结构（单文件、零抽象、栈上累加器），但引入 **C** 的两个关键工程改进：

1. **新增 `format.rs` 模块**集中所有字符串模板（`print_banner` / `print_turn_summary` / `format_tokens` / `format_cwd_tilde` / `status_symbol`），让未来加字段只改一处
2. **新增 `status.rs` 模块**承载 `TurnStats` + `accumulate`，与 `format.rs` 解耦（数据 vs 渲染）
3. **新增 `prompt.rs::format_cwd_tilde`**：review 阶段核查发现 prompt.rs 当前**没有**现成的 cwd `~` 缩放函数——只有 `HOME` 用于拼 `.yushan/AGENTS.md` / `SYSTEM.md` 路径（`prompt.rs:100, 148`）。本方案在 prompt.rs 新增 `pub fn format_cwd_tilde(cwd: &Path) -> String`，被 `format.rs` 复用，保持 cwd 缩放逻辑单一来源

不采纳 B 的 trait 抽象（**v0 字段数 ≤ 5** 时抽象成本 > 收益），不采纳 C 的 `CollectingSink` 包装（**v0 不消费事件**，包装只是死代码）。

### 最终结构

```
apps/coding-agent/src/
├── tui.rs          [MODIFY]   run_interactive 添加 TurnStats 句柄 + 调 format 模块
├── main.rs         [MODIFY]   创建 TurnStats 传入 tui，1-2 行改动
├── status.rs       [NEW]      TurnStats 结构 + accumulate 纯函数
├── format.rs       [NEW]      print_banner / print_turn_summary / format_tokens / format_cwd_tilde / status_symbol
└── prompt.rs       [MODIFY]   新增 pub fn format_cwd_tilde（被 format.rs 复用）
```

### 关键代码骨架

```rust
// status.rs
use agent_core::Usage;

#[derive(Default, Clone, Copy, Debug)]
pub struct TurnStats {
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}

impl TurnStats {
    #[inline]
    pub fn accumulate(&mut self, usage: &Usage) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(usage.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(usage.output_tokens);
    }
}
```

```rust
// format.rs（核心片段）
use std::io::{self, Write};
use agent_core::StopReason;
use crate::config::Config;
use crate::prompt;
use crate::status::TurnStats;

pub fn print_banner<W: Write>(out: &mut W, cfg: &Config, model_id: Option<&str>) -> io::Result<()> {
    writeln!(out, "YuShan Coding Agent")?;
    writeln!(
        out,
        "Provider: {} | Model: {} | Dir: {}",
        cfg.provider.as_deref().unwrap_or("(not configured)"),
        model_id.unwrap_or("(not configured)"),
        prompt::format_cwd_tilde(&cfg.cwd),
    )?;
    writeln!(out, "Type /help for commands, 'exit' to quit")?;
    writeln!(out)
}

pub fn print_turn_summary<W: Write>(
    out: &mut W,
    stats: &TurnStats,
    rounds: u32,
    stop: &StopReason,
) -> io::Result<()> {
    writeln!(
        out,
        "{} {} rounds · ↑{} ↓{} tokens",
        status_symbol(stop),
        rounds,
        format_tokens(stats.total_input_tokens),
        format_tokens(stats.total_output_tokens),
    )
}

pub fn format_tokens(n: u32) -> String {
    if n < 1_000 { n.to_string() }
    else if n < 10_000 { format!("{:.1}k", n as f64 / 1_000.0) }
    else if n < 1_000_000 { format!("{}k", n / 1_000) }
    else { format!("{:.1}M", n as f64 / 1_000_000.0) }
}

pub fn status_symbol(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::Completed => "✓",
        StopReason::MaxRounds => "⚠ MaxRounds",
        StopReason::Cancelled => "✗ Cancelled",
    }
}
```

```rust
// prompt.rs（新增；首次出现，被 format.rs 复用）
pub fn format_cwd_tilde(cwd: &Path) -> String {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(home) = home {
        let home_path = PathBuf::from(home);
        if let Ok(rel) = cwd.strip_prefix(&home_path) {
            return format!("~/{}", rel.display());
        }
    }
    cwd.display().to_string()
}
```

### 展示效果

启动（已配置）：
```
YuShan Coding Agent
Provider: deepseek | Model: deepseek-chat | Dir: ~/Develop/YuShan
Type /help for commands, 'exit' to quit

> _
```

启动（未配置）：
```
YuShan Coding Agent
Provider: (not configured) | Model: (not configured) | Dir: ~/Develop/YuShan
Type /help for commands, 'exit' to quit

> _
```

Turn 结束（不同 stop_reason）：
```
> 解释 main.rs 的结构

[AI 回复...]

✓ 1 round · ↑320 ↓1,247 tokens
> _
```

```
⚠ MaxRounds · 10 rounds · ↑12.4k ↓4.2k
```

```
✗ Cancelled · 2 rounds · ↑1.2k ↓340
```

### 演进路径（v0+1, v0+2）

| 何时 | 加什么 | 改动位置 | 前置 API 扩展 |
|------|--------|---------|--------------|
| 用户要求 context 占比 | 暴露 `Agent::limits()` + format 加字段 | `agent-runtime` 加 getter + format.rs | **需扩展** `Agent` 公开 API（增 `pub fn limits() -> &RunLimits`） |
| 模型开始报 cache_read/cache_write | `Usage` 加字段 + `TurnStats` 加字段 + format 加两段 | agent-core 加字段 → status.rs → format.rs（**tui.rs 不动**） | **需扩展** `agent_core::Usage`（增 `cache_read: u32, cache_write: u32`）—— 是破坏性变更，需走 semver major |
| 用户要 git branch | `git_branch: Option<String>` 字段 + 启动期探测 + format 拼接 | main.rs + format.rs | **无 API 扩展**；main.rs 启动期调 `git symbolic-ref` |
| 需要 streaming 状态栏 | 替换 `NoopEventSink` 为 `CollectSink` + tui.rs 在 turn 中读 events | tui.rs + 新增 collect_sink.rs | **需扩展** `Agent`（增 `pub fn set_events(...)`）**且** `agent_event::CollectingSink`（增 `pub fn clear()`）；否则只能在 build 期一次性绑定，与「REPL 运行时切换」冲突 |

> review 修正：原 design.md 中"结构已留好"是错的——`Agent` 当前**没有** `set_events`，`CollectingSink` **没有** `clear()`。这俩是公开 API 扩展点，触发时需走 ADR。

### 测试

- `format.rs` 单元测试：`format_tokens` 各档边界（999/1000/1001/9999/10000/...）、`status_symbol` 三变体、`format_cwd_tilde` 用 `tempdir`
- `status.rs` 单元测试：`accumulate` saturating 边界
- `tui.rs` 集成测试（可选）：用 `Vec<u8>` 替换 stdout，断言 banner 文本

---

## 被否方案的理由

### B 可扩展优先 — 暂时不需要

- v0 字段数 = 4，trait 抽象成本（5 个新文件、unsafe impl Send、`*const` 裸指针）远高于收益
- 真正的可扩展成本在「接缝位置」（数据聚合点、格式化集中点），而 A + format.rs 已经把这俩接缝留好了
- 等字段数 ≥ 8 或出现多 TUI 实例（headless/IDE 集成）时，再升级为 trait

### C 零分配演进 — 多余的接缝

- `CollectSink` 包装在 v0 不被消费，是死代码；v0+1 才需要
- "可注入 `&mut W: Write`"的签名是好实践，已并入推荐方案
- "buffer 64 字节预分配"是过早优化——输出量级 < 100 字节/秒

### 不复用 prompt.rs 已有 `~` 缩放

review 阶段核查修正：prompt.rs:100, 148 **没有** `~` 缩放逻辑——只用 `HOME` 拼 `.yushan/AGENTS.md` 与 `.yushan/SYSTEM.md` 路径。原始 A / C 方案的"复用"陈述是错的。推荐方案改为**新增** `prompt.rs::format_cwd_tilde`，避免后续出现「假装在复用，实际在复制」的逻辑分叉。

---

## 关键决策摘要

| 决策 | 选 | 否 | 理由 |
|------|----|----|------|
| 模块化程度 | 单 tui.rs + format/status 两新模块 | 5 文件 status/ 子模块 | 字段数 ≤ 4 不值 trait |
| 累加器位置 | TUI 栈上 | Config/Agent 字段 | 展示态不属于配置态 |
| 字符串模板 | 集中 format.rs | 散落 tui.rs | 未来加字段只改一处 |
| `~` 缩放 | 新增 `prompt.rs::format_cwd_tilde` | 复制实现 | 单一来源 |
| `CollectingSink` 包装 | 不做 | 预先包装 | v0 不消费 = 死代码 |
| trait 抽象 | 不做 | 预先抽象 | 等字段 ≥ 8 再升级 |
| `Usage` / `Agent` API 扩展 | 不做 | 预先暴露 `limits()` | v0 不展示 context 占比 |
| `Agent::set_events` / `CollectingSink::clear` | 不做（v0） | 预先扩展 | v0 不消费事件；streaming 状态栏是 v0+ 议题 |

---

## 受 Pi 启发的点

| 设计 | 来源 |
|------|------|
| `format_tokens` 三档/四档阈值 | `tmp/pi/.../footer.ts:24-30` |
| `formatCwdForFooter` 的 `~` 缩放语义 | `tmp/pi/.../footer.ts:32-44`（YuShan 内部**未实现**，本方案新增并复用其语义） |
| 累计 tokens 与 `Usage` 解耦 | `tmp/pi/.../usage-totals.ts:4-28` |
| Banner 三行布局（标题 / 状态 / 提示） | 通用 TUI 惯例 |

## 不照搬 Pi 的部分

- ❌ 不做 11 字段 footer（cache/cost/git/thinking/extension 5 项无基建）
- ❌ 不引入 `FooterDataProvider` 抽象（v0 不值）
- ❌ 不引入 alt-screen layout（保持纯 stdin/stdout）
- ❌ 不做 streaming 状态栏

---

## 实施顺序

1. 新增 `prompt.rs::format_cwd_tilde` 为 `pub fn` + 单测
2. 新建 `format.rs` + 单测（`format_tokens` / `status_symbol` / `format_cwd_tilde` 间接 / `render_full` 用于 `/status`）
3. 新建 `status.rs` + 单测（`TurnStats::accumulate` saturating 边界）
4. 改 `tui.rs`：调用 `format::print_banner` + `format::print_turn_summary` + `status::accumulate`，接收 `&mut TurnStats` 参数
5. 改 `main.rs`：创建 `TurnStats::default()` 并传入 `run_interactive`
6. **改 `commands/builtin.rs::StatusCommand`**：删除内联字段展示（`api_base` masking / `cwd` 等硬编码），改为 `ctx.tui_stats` 与 `format::render_full()` 读取——避免新增 banner 字段时 `/status` 命令静默遗漏
7. `cargo test` + `cargo clippy --all-targets` + 手动验证 3 种 stop_reason + `/status` 输出
