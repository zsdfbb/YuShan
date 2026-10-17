# 0042 — 默认推理强度取 `high`，只在 provider 声明支持时发送

`ys-coding-agent` 默认向支持该字段的 provider 发送 `reasoning_effort: "high"`。
`YUSHAN_REASONING_EFFORT` 可覆盖为 `minimal|low|medium|high|xhigh|max`；
未知 provider / 方言默认**不发送**该字段。

## Status

accepted（2026-10-17）

## 背景

真实基准表明，探索型任务的墙钟主要花在模型生成，而不是工具或重试：

- `fact/1` 的 7 次真实运行全部 `retries=0`、`retry_wait=0`，工具耗时约 1%；
- 不发送推理强度字段时，3 次墙钟为 150.6 / 199.2 / 224.5s；
- `high` 三次为 64.4 / 100.7 / 201.4s，中位 100.7s，质量均 3/3；
- `low` 两次为 72.4 / 88.6s，但两次都只交出 2/3 个有效发现。

因此默认不应停留在“由 provider 隐式决定”，也不应为了速度直接取 `low`。

## 决策

1. **默认 `high`**：在速度与可判定任务质量之间取已验证的折中点。
2. **显式覆盖**：`YUSHAN_REASONING_EFFORT` 保留 `low` 等档位，供实验或其他模型使用。
3. **能力门控**：字段只在 `ProviderCompat::supports_reasoning_effort` 为真时进入请求体。
   当前 `opencode-go` 为真；`deepseek` / `minimax` / custom 默认不发送，避免严格端点
   因未知顶层字段返回 400。
4. **可观测**：debug `run_start` 记录实际生效的
   `gen_ai.request.reasoning_effort`；不支持的 provider 写 `null`。

## Consequences

- 默认请求体在支持的 provider 上多一个字段；不支持者逐字节不变。
- `high` 仍保留明显方差（7–16 轮），推理强度只降低单轮生成成本，不能消除模型
  对“继续探索还是交卷”的选择差异。
- `low` 不作为默认值：现有样本中质量下降是稳定可见的。

## 关联

- `apps/coding-agent/src/config.rs` — 默认值与环境变量校验
- `apps/coding-agent/src/main.rs` — provider 能力门控与 `run_start`
- `adapters/model-openai-compatible/src/request.rs` — wire 字段
- `adapters/model-openai-compatible/src/compat.rs` — provider 能力声明
- `docs/arch/explore-speed/cost-model.md` §8.5 — 真实 A/B 口径
