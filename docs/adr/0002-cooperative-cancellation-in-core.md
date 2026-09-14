# 0002 — 取消是 agent-core 的协作式概念，取消不是错误

> **部分修订（2026-09-14）**：本 ADR 的**语义结论仍然成立**——取消是协作式的、是正常终止路径
> （`StopReason::Cancelled`）而非错误，检查点仍在步骤边界与工具轮询处。
> 但**载体变了**：`CancelToken`（`Arc<AtomicBool>`）已移除，改由零 tokio 的
> `BoundarySource::is_aborted()` 提供探针（信号源是 `Boundary::Abort` 消息）。
> 见 [ADR-0013](0013-cancel-token-removal-boundary-source.md)。下文对 `CancelToken` 的实现描述按此读。

agent-core 用 std 原语（Arc + AtomicBool）实现 CancelToken，Loop 只在步骤边界（模型调用之间、工具执行之间）检查取消；被取消的回合产出 RunResult（stop_reason = cancelled）而非 Err。Runtime 层未来可在 runtime-tokio feature 下增强（如借助 tokio-util 强中断 in-flight 调用），core 语义不变。

理由：agent-core 禁止依赖 Tokio，而测试矩阵要求 BasicLoop 可取消；协作式检查点是两者兼得的最低成本方案。「取消 = 正常终止路径」还让上层 UI 与持久化不必把取消当异常处理。

## Considered Options

- 直接用 tokio-util CancellationToken：破坏 agent-core 零 Tokio 依赖的约束。
- v0 不做取消：与测试矩阵冲突，且后补取消会穿透所有 trait 签名。

## Consequences

- 进行中的模型调用/工具执行不可中断，最坏取消延迟 = 单次调用耗时。
- ToolContext 携带取消句柄，长任务工具可自愿检查。
