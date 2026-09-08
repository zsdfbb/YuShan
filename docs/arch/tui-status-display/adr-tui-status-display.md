# ADR: TUI 状态展示方案选型

## 状态

提议（2026-09-08）

## 上下文

YuShan coding-agent 的 TUI (`apps/coding-agent/src/tui.rs`) 是极简 REPL：启动 banner + `> ` 提示符 + 打印 AI 回复。**完全没有状态展示**——用户不知道当前用的是什么模型、累计消耗多少 token、本轮是正常完成还是触发了 MaxRounds。

每次 `RunResult` 在 `tui.rs:54-68` 被消费后，`usage` / `rounds` / `stop_reason` 字段被丢弃。

完整背景见 [`context.md`](./context.md)，最终方案见 [`design.md`](./design.md)。

## 决策

### 1. 采纳「A 最小 + C 集中格式化」的混合方案

| 来源 | 采纳 | 否决理由 |
|------|------|---------|
| A 最小 | 整体结构：单 tui.rs 主改动 + 栈上累加器 + 4 辅助函数 | — |
| B 可扩展 | 仅借鉴「数据聚合 vs 渲染分层」思想，但**不引入 trait** | v0 字段数 ≤ 4，trait 抽象成本 > 收益 |
| C 零分配 | 借鉴「集中格式化」「`&mut W: Write` 可注入 sink」「accumulate 纯函数」 | **不采纳** `CollectSink` 包装（v0 不消费 = 死代码） |
| — | **新增**：`format.rs` 集中字符串模板；在 `prompt.rs` 新增 `pub fn format_cwd_tilde`（review 阶段核查发现 prompt.rs 实际**没有**现成 `~` 缩放函数） | 原始 A / C 方案误以为 prompt.rs 已有此函数，本方案改为新增 |

**Why**：
- 字段数 ≤ 4 时，trait + `Box<dyn Source>` + `unsafe Send` 的成本远超收益
- 「接缝位置」比「抽象层级」更影响演进成本——format.rs + status.rs 已留好
- 单一来源新增 `format_cwd_tilde` 避免后续逻辑分叉

**How to apply**：字段数膨胀到 ≥ 8 或出现多 TUI 实例（headless/IDE 集成）时，升级为 `StatusSource` trait；其他情况下保持当前结构。

### 2. 累加器放在 TUI 层，不放 Config / Agent

`TurnStats` 是展示态，**不属于配置态**。

**Why**：
- Config 应保持「用什么」语义；「用了多少」是运行时统计
- Agent 公开 API 是稳定契约，加累计字段会污染 API
- 累加器生命周期与 `run_interactive` 调用栈一致，栈上即可，**无需 Arc/Mutex**

**How to apply**：若未来需要跨实例累计（多 session 持久化），抽到 session crate 单独存储；不混入 Config/Agent。

### 3. 不扩展 crate 公开 API

`RunResult.usage` / `RunResult.rounds` / `RunResult.stop_reason` / `Config.cwd` / `Config.provider` / `Agent.model_id()` **已够用**。

**Why**：
- context.md §「数据缺口」明确：聚焦方案不需要任何 API 扩展
- 扩展 crate API 是不可逆决策——一旦暴露 `Agent::limits()`，后续重构会牵涉所有调用方
- v0 不展示 context 占比，**无理由暴露 `RunLimits`**

**How to apply**：当且仅当出现明确需求（context 占比、cache 命中）时再扩展；不做"提前预留"。

### 4. v0 不实现 streaming 状态栏

`NoopEventSink` 保留，**不包装为 `CollectSink`**。

**Why**：
- v0 不消费 `AgentEvent`（tool call 列表 / 实时输出无需求）
- 包装 = 死代码 + 引入新模块
- 真实需求出现时再换 sink——`AgentEvent` 枚举已稳定，更换无破坏性

**How to apply**：当 TUI 需要展示 tool call 中间状态或实时 token 流时，新增 `collect_sink.rs`，在 `main.rs` 替换 `NoopEventSink`。`tui.rs` 主体可保持不变（format.rs 加新格式化函数即可）。

### 5. `format_tokens` / `format_cwd_tilde` 阈值与 Pi 对齐

```rust
// format_tokens
n < 1_000      → "n"
n < 10_000     → "X.Xk"
n < 1_000_000  → "Nk"
n < 10_000_000 → "X.XM"
else           → "NM"
```

```rust
// format_cwd_tilde: $HOME 前缀 → ~/<rel>, 否则原路径
```

**Why**：Pi 已验证过的格式阈值，跨工具一致（用户在 YuShan 和 Pi 之间切换无认知负担）。

**How to apply**：阈值在用户反馈「读不清」时再调，不要预先改。

### 6. StopReason → 符号映射

```
Completed  → ✓
MaxRounds  → ⚠ MaxRounds
Cancelled  → ✗ Cancelled
```

**Why**：与上下文提议一致；区分成功/警告/失败三态。

**How to apply**：新增 `StopReason` 变体时（如果发生），同步更新 `status_symbol` 函数；这是单点改动。

## 备选方案

### 备选 B：可扩展优先

| 维度 | B | 推荐 |
|------|---|------|
| 改 tui.rs | ~10 行 | ~40 行 |
| 新文件 | 5 | 2 |
| 新增 trait | 1 | 0 |
| unsafe 块 | 2 | 0 |
| 加 cache_read 字段改动 | formatter 加 match 分支 | format.rs 加字段（两者相当） |

**否决理由**：B 的核心价值是「加字段时改动局部化」，推荐方案通过 `format.rs` 集中已实现同样目标，且无 trait/dispatch 开销。

### 备选 C：零分配 + CollectingSink 包装

**否决理由**：`CollectSink` 包装在 v0 不被消费 = 死代码。其余优点（`&mut W: Write` 可注入、`accumulate` 纯函数、buffer 预分配）已并入推荐方案。

### 备选 D：完全照搬 Pi footer

**否决理由**：cache/cost/git/thinking/extension 5 项在 YuShan v0 无基建，照搬 = 造 5 个新特性。详见 context.md §「Pi 对照」。

## 后果

### 正面

- ✅ v0 投入：2 个新模块（status.rs / format.rs）+ tui.rs 主改动 + prompt.rs 新增 1 个 pub fn
- ✅ 测试容易：`format_tokens` / `status_symbol` / `format_cwd_tilde` 全部纯函数，单测覆盖边界
- ✅ 演进路径清晰：format.rs + status.rs + prompt.rs::format_cwd_tilde 三个接缝点

### 负面

- ⚠️ `format_cwd_tilde` 是**新增**（非抽取），需在 prompt.rs 加单测覆盖 `$HOME` 前缀、不在 HOME 下、Windows 路径三种场景
- ⚠️ 启动 banner 是固定展示，无法 toggle（用户希望 toggle 时需新增命令）
- ⚠️ `/status` 命令与新 banner 字段存在重复（StatusCommand 内硬编码展示字段）。**实施顺序已加 step 6**：改 `StatusCommand` 调 `format::render_full()` 读取，消除重复维护

### 风险

| 风险 | 缓解 |
|------|------|
| prompt.rs 新增 `format_cwd_tilde` 破坏现有行为 | prompt.rs 当前**没有**此函数，新增零回归风险；新增后跑 cargo test 确认 `build_system_prompt` 路径不变 |
| StatusCommand 漏改 | 实施顺序 step 6 已明确改 StatusCommand 调 `format::render_full()`；PR 评审检查 |
| StopReason 新增变体时漏改 status_symbol | 单测覆盖三变体，CI 防回归 |
| 用户期望 toggle banner | 未在 v0 范围；收到反馈时新增 /quiet-banner 命令 |

## 演进路径摘要

| 触发条件 | 改动 | 前置 API 扩展 |
|---------|------|--------------|
| 模型开始返回 cache_read / cache_write | `agent_core::Usage` 加字段 → `TurnStats` 加字段 → `format.rs::print_turn_summary` 加两段 | **是** — `Usage` 字段增破坏性变更，需 semver major |
| 用户要 context 占比 | `Agent::limits()` getter + format 加字段 | **是** — 增 `pub fn limits(&self) -> &RunLimits` |
| 用户要 git branch | 启动期 `git symbolic-ref` 探测 + format 拼接 | **否** — main.rs 启动期一次探测 |
| 用户要 streaming 状态栏 | 替换 `NoopEventSink` 为 `CollectSink` + tui.rs 在 turn 中读 events | **是** — `Agent` 需 `pub fn set_events(...)`，`CollectingSink` 需 `pub fn clear()` |
| 字段数 ≥ 8 | 升级为 `StatusSource` trait 抽象 | **否** — 内部重构，不影响外部 |

> review 修正：原 design.md 中"演进路径：streaming 状态栏（结构已留好）"是错的——`Agent` 无 `set_events`，`CollectingSink` 无 `clear()`。上表「前置 API 扩展」列明示了哪些是公开 API 变更。

## 相关文档

- [`context.md`](./context.md) — 现状分析 + Pi 对照 + 字段清单
- [`design.md`](./design.md) — 最终方案 + 对比矩阵 + 实施顺序

## 受 Pi 启发的来源

- `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts:24-30` — format_tokens 阈值
- `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts:32-44` — formatCwdForFooter 语义
- `tmp/pi/packages/coding-agent/src/core/usage-totals.ts:4-28` — 累加器独立于 Usage
