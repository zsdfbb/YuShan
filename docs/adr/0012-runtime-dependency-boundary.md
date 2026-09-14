# 0012 — 运行时依赖边界：「零 tokio」只对 `ys-core` 及纯契约层成立

ADR-0004 点 7 曾写「v0 工具串行、**零 tokio 生产依赖**、`runtime-tokio` feature 随 v1 引入」。**该表述已被 v1 实践突破，需更正**：`ys-session` 与 `ys-loop` 自 v1 起已**直接**依赖 tokio。本 ADR 澄清真正的边界在哪，并说明为何不做 `runtime-tokio` feature 门控。

## Status

accepted（2026-09-12）

## 背景（核实过的现状）

| crate | `[dependencies]` 含 tokio | 用途 |
|---|---|---|
| `ys-core` | ❌ | 纯原语（`Arc`/`AtomicBool`/serde） |
| `ys-event` | ❌ | 纯事件定义 |
| `ys-component` | ❌ | `RuntimeContext`/`RunLimits` |
| `ys-session` | ✅ | `tokio::fs`（`JsonlSession` 文件 IO） |
| `ys-loop` | ✅ | `tokio::time`（工具超时，`basic.rs:239`） |
| `ys-runtime` | ❌ 直接，✅ 间接 | 依赖 `ys-loop` |
| `apps/coding-agent` | ✅ | `#[tokio::main]` |

CLAUDE.md 的硬约束原话是「**`agent-core`** 不依赖 Tokio、HTTP、数据库、TUI 或具体模型 SDK」——**主语是 `agent-core`，不是全仓**。这一点此前被误读为「全仓零 tokio」，进而在设计讨论中引出错误论据（见 ADR-0011 的更正）。

## 决策

**真正的边界是：纯契约层不得依赖运行时；执行层可以。**

1. **不直接依赖运行时**（硬约束，不得违反）：`ys-core`、`ys-event`、`ys-protocol`、`ys-component`。
   判据：这些 crate **自身的代码不使用 tokio API**——它们装的是数据、枚举、纯 trait。
   **精确化（实施期回改）**：`ys-component` 虽不直接声明 tokio，但**经 `ys-session` 间接引入**
   （`RuntimeContext` 持有 `&mut dyn Session`，而 `ys-session` 用 `tokio::fs`）。
   真正「直接与间接皆无 tokio」的是 `ys-core` / `ys-event` / `ys-protocol` 三者。
2. **允许依赖 tokio**：`ys-session`、`ys-loop`、`ys-runtime`、`apps/*`。
   理由：工具超时需要定时器、JSONL 需要异步文件 IO，二者都**没有运行时无关的实现**（Rust 的异步 IO/定时器本质上是 runtime 提供的）。项目当前 tokio-only，为假想的多运行时支持付费不划算。

## Considered Options

- **加 `runtime-tokio` feature 门控**（ADR-0004 点 7 的原意）：让 tokio 变可选。**否决**——需要把超时与文件 IO 都变成可选能力，牵动 `RunLimits`/`JsonlSession`/`ToolContext` 多个面；而收益是"支持非 tokio 运行时"，**当前无此需求**。
- **把 tokio 依赖的**实现**抽出到 adapter**（`JsonlSession` → `adapters/session-jsonl`）：可让 `ys-session` 回到纯 trait + `MemorySession`。**不采纳（记为此后可选）**——`design.md` §9 本就规划了 `session-jsonl` adapter 而代码放进了 `ys-session`，属轻微漂移；但因 `ys-loop` 无论如何都会拖进 tokio，抽出的收益仅是概念纯粹性，不解决任何实际问题。
- **维持现状并澄清边界（采纳）**：明确"纯契约层 vs 执行层"这条线，其余照旧。

## Consequences

- **ADR-0004 点 7 的「零 tokio 生产依赖」作废**——见该 ADR 的修订小节。
- **`ys-protocol`（原 `ys-channel`）不含 tokio 的理由是「契约不该指定传输」，不是「避免拖进 tokio」**——后者不成立（`ys-loop` 已直接依赖 tokio）。
- **`design.md` §9 与代码的轻微漂移（`JsonlSession` 位置）**记录在案，不作为本轮工作。
- 后续若真出现非 tokio 运行时需求，回访本 ADR 的第二个「Considered Option」。
