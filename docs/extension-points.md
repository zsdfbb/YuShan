# 扩展点归类指引

> 用途：拿到一个「想让 agent 做 X」的需求时，**先判断它落在哪个桶**，再决定怎么实现。
> 依据：`docs/CONTEXT.md`（Event / Hook / Component / Loop 四者边界）、[ADR-0014](./adr/0014-follow-up-without-hook-system.md)。
> 一句话动机：本项目**有意不建通用 hook 系统**；绝大多数"看起来要 hook"的需求，其实是 **Event** 或 **Tool**。

## 四者边界（既有）

```text
Event      发生了什么        （只观察，不可改流程）
Hook       下一步怎么处理      （可改、可控制；本项目**有意推迟**，见 ADR-0014）
Component  谁来提供能力        （Model / Tool / Session / EventSink）
Loop       按什么规则推进
```

## 判断链（从上往下走，停在第一条命中）

1. **它只是"想知道某事发生了"，还是要**改变**流程？**
   - 只想知道 → **Event 的消费者**。停。
   - 要改流程 → 下一条。
2. **是"模型决定何时做"，还是"无论模型怎么想都必然发生"？**
   - 模型决定 → **Tool**。停。
   - 必然发生 → 下一条。
3. **发生在回合之内，还是回合之间？**
   - **回合之内** → 要改循环内部的转瞬数据：**工具在你自己手里 → 工具内部做**；**只有"不能改工具、又要加工它的结果"才需要 Hook**。
   - **回合之间** → **Follow-up（追问）**。

## 五个桶 + 判断要点

| 桶 | 何时用 | 关键特征 |
|---|---|---|
| **Event 消费者（sink）** | 在"某事发生时"做**副作用**：通知、日志、webhook、往外部发消息 | **不改流程**；不决定下一步 |
| **Tool** | 给 agent **新能力**，由**模型**决定何时用 | 进 `registry`；写进系统提示；模型主动调 |
| **Follow-up（追问）** | **回合结束后**，**系统**自动再起一轮 | 增回合数；在 `app_loop`（回合之间） |
| **Compaction（回合内）** | 回合**之内**、轮边界的家务（缩上下文） | 不增回合数；在 `BasicLoop` |
| **Hook（回合内变换/控制）** | **仅当**要加工/拦截一个**你不拥有的**工具结果或模型请求 | 本项目**当前无消费者**（ADR-0014） |

## 那条硬边界

> **Hook 只在「不能改那个工具、却又要加工它的结果」时不可替代。**
> 工具在你自己手里 → **工具内部做**，别建 hook。

这条是区分"该做 hook"与"该改工具"的唯一判据。本项目所有工具都是自己的，所以目前**轮不到 hook**。

## 已走过的六个用例（供对照）

| 用例 | 落桶 | 说明 |
|---|---|---|
| 投资 agent 要 hook | — | **假想**：仓库里只有方向、无需求文档；且它想要的多半是**事件** |
| commit 后同步 `AGENTS.md` | **Follow-up** | 回合之间、系统发起 |
| 自动压缩上下文 | **Compaction（已有）** | 回合之内；且**已实现** |
| 完成后 macOS 通知 | **Event** | 观察 `RunFinished` + 副作用 |
| 调其他进程发消息 | **Event**（系统发）/ **Tool**（模型发） | 看**谁决定** |
| LSP 查询 | **Tool** | 长驻进程 → 更适合做成 **adapter crate** |
| LSP 改完自动查诊断 | **工具内部** | 形似 hook，但 `EditTool` 是我们的 → 工具自己做 |

> **六个用例，零个需要 Hook 系统。**

## 命名警告（血泪）

生态里 "hook" 被**严重滥用**。"通知 / 日志 / 往外部发"一律是 **Event**，不是 Hook。

- **反例：CodeWhale 的 `crates/hooks/`** —— 名字叫 hook，实为**事件扇出**（`HookSink` + `HookDispatcher`）。
  其源码自注：*"hook sinks are best-effort observability, **not control flow**"*。
  它就是本项目 `EventSink` 的东西，借了个错名字。
- **范本：Pi 的 `packages/agent/src/harness/hooks.ts`** —— 真·控制流 hook（11 点、逐点聚合与出错策略）。
  **将来真要做 hook 时的形状参考。**（两条约束已探明：落点为 `ys-component`；签名只用 `ys-core`/`ys-model` 类型，避免依赖环。）

## 参考

- `docs/CONTEXT.md` —— 术语定义（回合 / 轮 / 追问 / 压缩 / Event / Hook…）
- `docs/design.md` §6 —— Hook / Event 边界规范（**有意推迟**，见其状态注）
- [ADR-0014](./adr/0014-follow-up-without-hook-system.md) —— 为什么只做 follow-up
- `docs/design-plans/2026-09-15-follow-up.md` —— follow-up 的具体设计
