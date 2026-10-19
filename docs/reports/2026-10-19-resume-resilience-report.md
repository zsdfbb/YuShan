# 会话恢复韧性与工作过程可见性 — 修改报告

> 日期：2026-10-19｜基线：`f18b406`｜范围：已确认的 resume / 告警 / 过程叙述问题

## 结果

- 被中断的 session 会在打开时补齐悬空 `ToolUse` 的错误结果，并先原子落盘；resume 后可继续请求模型。
- 坏会话行不再由 `ys-session` 直写 stderr，而是计数后由应用层在对应输出通道提示。
- `stream_options` 兼容回退只对明确的“不支持该字段”400 生效；普通协议错误不重发。
- 模型适配器的 fallback / 非法参数 warning 全部经 `ModelLogger`，不再污染 TUI 输入区。
- 工作型工具在场时，system prompt 明确要求每个工具批次前提供一句用户可见进展说明，并在里程碑后说明完成与下一步。

## 代码落点

| 范围 | 文件 |
|---|---|
| 会话规范化 | `crates/ys-session/src/{jsonl.rs,lib.rs}` |
| resume 摘要接线 | `apps/coding-agent/src/{wiring.rs,capabilities.rs,main.rs,app_loop.rs}` |
| 400 分类与 logger | `adapters/model-openai-compatible/src/lib.rs` |
| 过程叙述 | `apps/coding-agent/src/prompt.rs` |
| 回归测试 | `adapters/model-openai-compatible/tests/retry.rs`、`apps/coding-agent/tests/{stream_usage_fallback.rs,truncation_guard.rs}`、相关 crate 内单测 |

## 验证

- `cargo test --workspace`：全绿。
- `cargo clippy -p ys-session -p ys-model-openai-compat --all-targets --no-deps -- -D warnings`：通过。
- `bash scripts/doc-consistency.sh`：通过。
- `git diff --check`：通过。
- 最新真实 session 的隔离副本复测：原 104 个 `ToolUse` / 103 个 `ToolResult`，打开后为 104 / 104；
  缺失的 `call_00_ET_KDRZ3ijKrkYYWRuIjdez7687` 被补为
  `Tool execution was interrupted; result unavailable.`，再次打开幂等。

## 已知边界

- 会话修复不重放工具，副作用无法恢复；模型只看到错误结果并自行决定下一步。
- 发生工具修复时会按既有 tmp + rename 重写整个 JSONL，无法解析的坏行会从文件中移除；数量会显示给用户。
- 会话中存在“结果位于其他非相邻位置”的复杂畸形历史时，现有策略按 id 视为已配对，不主动重排。
- 全 workspace `cargo clippy --all-targets -- -D warnings` 仍受基线已有的 `ys-protocol` / coding-agent lint
  阻塞；这些 lint 不在本次改动范围。

## 工作流说明

已加载 `sequential-workflow` 并建立基线。当前环境不支持其必需的 subagent 派发，用户明确确认该限制后，
后续实现切换为主 agent 直接执行；仍保留基线、测试合同、只读复核检查与文档一致性检查。
