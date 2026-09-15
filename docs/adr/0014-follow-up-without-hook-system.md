# 0014 — 只做 follow-up，不建通用 hook 系统

`design.md` §6 完整规范了 Hook 系统（9 个 hook 点、Observer/Transform/Control 三类、
`HookDecision`、`RuntimeHook` trait），但**零代码**。为实现「提交后自动同步 `AGENTS.md`」这一需求，
我们**只实现 `app_loop` 层的 follow-up（追问）**，**有意不建通用 hook 系统** ——
唯一真实的消费者是一条 follow-up 路径；hook 系统真正的价值锚点（**插件缝**）当下同样没有消费者。
遵循「**不设计假想未来需求**」，等第二个真实消费者出现再抽。

## Status

accepted（2026-09-15）

## 背景

- `design.md` §6 / §10 早已规范 hook（`RuntimeHook`、`HookContext`、9 个点、`AgentBuilder::hook()`），
  `CONTEXT.md` 也定死了 Event / Hook 边界。**代码一行没有。**
- 提出的**真实需求**：git 提交后自动起一轮同步文档 —— 提示词 / 工作流都靠模型自觉，**不强制**，
  故触发必须是代码。
- 评估过的「未来理由」，两条都不成立：
  - **「投资 agent 将来要 hook」**：仓库里只有方向、**无需求文档**，且 `docs/arch/gap-closure/context.md:216`
    自己写明「**本轮不做**：投资 agent 等 Runtime 成熟后再组装」。且它想要的多半是**事件**
    （如 `ReportGenerated { path }`），**不是 hook**。
  - **「自动压缩」**：在**回合之内**且**已实现**（`basic.rs:130-137`）——既不是 hook，也不是 follow-up。

## 决策

1. **只做 follow-up**（见 `docs/design-plans/2026-09-15-follow-up.md`）：检测 HEAD 前后变化 →
   `app_loop` 在回合结束后起一轮「同步文档」；带防套娃 / 开关 / 上限三样护栏。
2. **不建 hook 系统**。`design.md` §6 保持有效，但**有意推迟**。

## Considered Options

- **立即建 hook 系统**（照 `design.md` §6 落地 9 个点）。**推迟** —— 唯一消费者是 follow-up 一条路径；
  按项目自身原则「不设计假想未来需求」，没有第二个真实消费者的通用机制不建。
- **用 Event 承载「要不要追问」**。**否决** —— `CONTEXT.md` 定义 Event「**只可观察，不可改变流程**」；
  追问是**控制**（改变流程），定义上就落在 Hook 那一栏，不能走 Event。
- **把 follow-up 放进 `BasicLoop`**（与 §6 的 9 个点同层）。**否决** —— ①「多回合驱动」本就归
  `app_loop`（ADR-0010 附注，actor 循环已上移）；② `RunResult` 住在 `ys-loop`，hook 若从 `ys-component`
  碰它会构成 `ys-component → ys-loop → ys-component` **依赖环**。
- **改 `git commit` 命令解析 / 装 `.git/hooks/post-commit`**（用于检测）。**否决** ——
  解析命令脆（别名 / `&&` / `bash -c` 可绕过）；装 git hook 要往用户仓库写文件，越界。
  改取 **HEAD 前后对比**（顺带覆盖用户自己的提交）。

## Consequences

- **`design.md` §6 仍然有效、但推迟**：本 ADR 记录其「**有意未实现**」，避免后人翻到 §6 以为遗漏。
- **follow-up 是一个可长成 hook 的 seam**：形状取「回合结束 → 问一次 → 可选下一轮」；
  将来要「可插」时，把这**一个函数**升级为列表即可（**加法，非重写**）。今天**不预埋抽象**。
- **两个「compact」澄清**（写入 `CONTEXT.md`）：**自动压缩已实现**（回合内）；**`/compact` 命令仍是占位**
  （实为 `clear_session`）——那是独立缺口，与本 ADR 无关。
- **将来若做 hook 系统**，两条约束已探明（勿重蹈）：
  1. 落点 **`ys-component`**（`RuntimeContext` 所在）；
  2. 签名**只用 `ys-core` / `ys-model` 类型**，**绝不用 `ys-loop` 类型**（否则依赖环）。
  参考实现：Pi 的 `packages/agent/src/harness/hooks.ts`（真·控制流 hook，11 点、逐点聚合/出错策略）
  是范本；**CodeWhale 的 `crates/hooks/` 是反例** —— 名字叫 hook，实为**事件**（其源码自注
  "observability, **not control flow**"），即本项目已存在的 `EventSink`。
