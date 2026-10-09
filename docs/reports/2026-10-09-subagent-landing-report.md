# Subagent 派发落地 — 修改报告

> 日期：2026-10-09。依据：[ADR-0022](../../adr/0022-subagent-dispatch.md)（形态）、
> [ADR-0029](../../adr/0029-subagent-landing-decisions.md)（落地裁定）、
> `docs/arch/subagent-rebaseline/{context,design,review}.md`。
> 执行计划：`docs/exec-plans/2026-10-09-subagent-landing.md`。

## 结论

设计七刀中的 **刀 0–6 全部落地并测试通过**；**刀 7（成本数字回填）待真实 API 凭证**（`MANUAL_ACK_REQUIRED`）。

## 改动概览

26 个文件、+1275 / −108。核心：

| 刀 | 改动 | 关键文件 |
|---|---|---|
| 0 | P0 前缀指纹（父 10 元 / 子 9 元 / 同表构造两次逐字节相同） | `apps/coding-agent/src/toolset.rs` |
| 1 | `Tool::timeout()` 默认方法 + 循环优先级链 | `crates/ys-tool/src/lib.rs`、`crates/ys-loop/src/basic.rs` |
| 2 | `Usage` 加缓存字段 + 适配器解析 + `context_window_for` / `build_model_for` | `crates/ys-core/src/usage.rs`、`adapters/model-openai-compatible/*`、`apps/coding-agent/src/config.rs` |
| 3 | `SubagentRunner` 窄 trait + 三跳注入 | `crates/ys-tool/src/{subagent.rs,context.rs}`、`ys-component/context.rs`、`ys-runtime/agent.rs` |
| 4 | runner 实现 + `subagent` 工具 + 三入口接线 + usage 上卷 + sweep + 派发上限 + fail-closed + `## Files` 保证 | `apps/coding-agent/src/subagent.rs`、`main.rs`、`app_loop.rs`、`toolset.rs` |
| 5 | 能力边界声明按工具表派生 + 防失控 guideline | `apps/coding-agent/src/prompt.rs` |
| 6 | `is_concurrency_safe()` + `max_parallel_tools` + Step 9 三段式（条件并行、保序） | `crates/ys-tool/src/lib.rs`、`ys-component/limits.rs`、`ys-loop/{Cargo.toml,src/basic.rs}` |

附带：TUI 工具行摘要键加 `description`（`crates/ys-tui-coding/src/transcript.rs`）。

## 验收

- `cargo test --workspace`：**45 组全 ok，0 failed**。
- `cargo clippy --all-targets`：本轮新增/改动文件**无警告**（仓内其余为既有警告）。
- 新增/强化的用例：P0 指纹（含父表 10 元重锚）、超时优先级链、`Usage` 缓存字段（解析/相加）、`context_window_for`、`build_model_for`、`SubagentRunner` object-safe、三跳注入可达、`ChildBoundary` 语义、`UsageTotals`、UTF-8 安全截断、`## Files` 保证、`subagent` 工具结果映射、**端到端跑一个隔离子 agent**、每回合派发上限、并行批四条（默认串行保序 / 并行 / 混合退回 / 上限）、**`app_loop` 接线端到端**（模型发 `subagent` → 子产出 → 结果回喂，覆盖 `TurnCtx → make_runner → AgentPorts::with_subagent` 那条链）。

## 与设计的偏差（如实记账）

1. **实现未按 skill 派发 subagent 执行**：本环境多 agent 任务载荷投递失败（子 agent 收不到任务、或在继承上下文后自我编排），改为在单 agent 上下文内直接实现。skill 的「实现必须派 subagent + 独立 review」红线因此**未满足**。
2. **`SubagentOutcome` 去掉了 `#[non_exhaustive]`**：实现方是 app crate，否则无法构造该结构体。
3. **新增依赖** `tokio-util`（`AbortOnDropHandle`）与 `futures-util`（`buffered`）：二者均已在 `Cargo.lock`，无新下载。
4. **`cargo fmt --check` 不作为门禁**：13 个**基线**文件本就存在 fmt 差异，且已禁止在 subagent 中裸跑 `cargo fmt`。

## 未完成

- **刀 7**：跑一次真实派发，把固定前缀 / 每轮的真实数字写回 `docs/arch/subagent-rebaseline/design.md` §6.4。需要 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`。
- 未做 git 提交（用户明确要求）。

## 已知缺口（如实记账）

- **`--json` 的 `RunFinished.usage` 不含子 agent usage**：子 usage 在回合**结束后**由 app 侧并入 `RunResult.usage`，而 `RunFinished` 由 `BasicLoop` 在回合内发出 ⇒ 事件流只算父。TUI 状态行（`TurnStats`）与 `-p` 的 meta 用的是合并后的总数，**正确**。详见 `docs/design-final/coding-agent-tui.md` §13.5。
- 子 agent 内部活动在任何输出通道不可见（ADR-0022 决策 2 的少数派偏离）。
- 并行子 agent 的写冲突无人管（本仓无沙箱），只能靠 guideline。
