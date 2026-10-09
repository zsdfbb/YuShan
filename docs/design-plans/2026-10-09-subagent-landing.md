# Subagent 派发落地 设计计划

## 背景

- 权威形态设计 [ADR-0022](../../adr/0022-subagent-dispatch.md)（accepted 2026-09-23）**零落地**：
  `SubagentRunner` / `SubagentOutcome` 在源码里不存在，`subagent` 只作为字符串出现在系统提示的能力边界声明里。
- 本轮复核与裁定：[ADR-0029](../../adr/0029-subagent-landing-decisions.md)（accepted 2026-10-09）、
  `docs/arch/subagent-rebaseline/design.md`（7 刀刀序）、`review.md`（质量分析，含 R1–R8 处置）。
- 目标：**按设计文档实现 subagent 派发**（含并行、per-tool 超时、子独立预算、成本读数），并做**全面测试**。

## 设计

**不重述** —— 见 `docs/arch/subagent-rebaseline/design.md` 全文。七刀：

| 刀 | 内容 |
|---|---|
| 0 | P0 前缀指纹测试（父/子 + 同表构造两次逐字节相同） |
| 1 | per-tool 超时闸（`Tool::timeout()` 默认方法 + 优先级链） |
| 2 | 成本读数（`Usage` 缓存字段 + 适配器解析 + `context_window_for`/`build_model_for`） |
| 3 | 窄 trait `SubagentRunner` + 三跳注入 |
| 4 | runner 实现 + `subagent` 工具（含派发总量上限、sweep、fail-closed） |
| 5 | 提示词（`prompt.rs:107` 条件式 + guideline） |
| 6 | 并行派发（`is_concurrency_safe()` + `max_parallel_tools` + Step 9） |
| 7 | 成本数字回填（真实派发后写回 §6.4） |

**必须遵守的裁定**：`Tool::timeout()`/`is_concurrency_safe()` 为默认方法；`DEFAULT_MAX_PARALLEL_TOOLS` 放 `ys-component`；
子预算与主 agent 一致（父回合 deadline 兜底）；**每回合派发总量上限**（建议 16）；并行用 `buffered`（保序、不 spawn）；
watchdog 用 `AbortOnDropHandle`；`model` 名解析失败 fail-closed；产物目录 sweep 7 天 + `YUSHAN_SUBAGENTS_DIR`；
模型实例缓存持 `Arc<dyn Model>`。

## 涉及文件

- **新增**：`crates/ys-tool/src/subagent.rs`、`apps/coding-agent/src/subagent.rs`、`apps/coding-agent/tests/prefix_fingerprint.rs`
- **修改**：`crates/ys-tool/src/{lib.rs,context.rs}`、`crates/ys-component/src/{limits.rs,context.rs}`、`crates/ys-runtime/src/agent.rs`、
  `crates/ys-core/src/usage.rs`、`crates/ys-loop/src/basic.rs`、`crates/ys-loop/Cargo.toml`、
  `adapters/model-openai-compatible/src/{response.rs,lib.rs}`、
  `apps/coding-agent/src/{config.rs,toolset.rs,main.rs,status.rs,app_loop.rs,prompt.rs}`、`apps/coding-agent/tests/e2e_skills.rs`

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。

#### N1: P0 前缀指纹（刀 0）
- **验收方法**: `cargo test -p ys-coding-agent --test prefix_fingerprint` — 期望 passed；**变异**（交换 `toolset.rs` 里 grep/find 顺序）应使用例变红
- **优先级**: 必测
- **验收人**: review subagent

#### N2: per-tool 超时闸（刀 1）
- **验收方法**: `cargo test -p ys-loop` — 期望新增「声明 600s 的工具不被 300s 杀」通过、既有超时用例全绿
- **优先级**: 必测

#### N3: 成本读数（刀 2）
- **验收方法**: `cargo test -p ys-core -p ys-model-openai-compatible -p ys-coding-agent` — 适配器解析 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`；`context_window_for`/`build_model_for` 单测通过
- **优先级**: 必测

#### N4: 窄 trait 与三跳注入（刀 3）
- **验收方法**: `cargo build --workspace` + `cargo test -p ys-tool -p ys-runtime` — 期望假 `SubagentRunner` 注入后工具可读到；`AgentPorts::new` 签名不变、既有调用点全绿
- **优先级**: 必测

#### N5: runner 与 `subagent` 工具（刀 4）
- **验收方法**: `cargo test -p ys-coding-agent` — 期望：子撞 `max_rounds` 父回合不失败且 `is_error`；usage 上卷进 `TurnStats`；超派发上限返回 `Err`；`YUSHAN_SUBAGENTS_DIR` 生效且 sweep 只清超龄；`model` 名解析失败返回 `Err` 不静默回退
- **优先级**: 必测

#### N6: 提示词（刀 5）
- **验收方法**: `cargo test -p ys-coding-agent` — 父提示含 guideline 且不含「no subagents」；子提示含该句
- **优先级**: 必测

#### N7: 并行派发（刀 6）
- **验收方法**: `cargo test -p ys-loop` — `design.md` §4.4 的六条（默认串行不变、开并行更快、混合退回、保序、上限、变异）全部通过
- **优先级**: 必测

#### N8: 成本数字回填（刀 7）
- **验收方法**: 需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY` 跑一次真实派发，把真实读数写回 `docs/arch/subagent-rebaseline/design.md` §6.4。**当前环境无凭证** ⇒ 人工确认
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

### 测试框架与命令

- 全量: `cargo test --workspace`
- 静态: `cargo clippy --all-targets` / `cargo fmt --check`

## 波及文档

- `docs/design-final/coding-agent-tui.md`（新增 subagent 派发章节）
- `docs/architecture.md`（工具表 10 元、并行批）

## 风险与注意事项

- 加第 10 个工具会改 prompt 前缀字节（P0 指纹先锚 9 元、刀 4 后重锚 10 元）。
- 并行是本轮**唯一**动既有执行语义处，必须被 `is_concurrency_safe()` 默认 `false` 闸住。
- 依赖方向：`ys-tool` 不得依赖 `ys-model` / `ys-runtime`；`ys-core` / `ys-event` / `ys-protocol` 零 tokio。
- 每条契约改动都要有变异自检（本仓多个真 bug 是「测试全绿时依然存在」）。
