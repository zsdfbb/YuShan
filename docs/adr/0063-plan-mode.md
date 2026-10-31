# 0063 — Plan 模式：工具恒定 + 调用期门禁 + 一条单向应答信道

给主 agent 一个会话级只读模式 `Plan | Yolo`（默认 `Yolo`）。工具表**不变**，
`write` / `edit` / `bash_bg` 在 `call()` 开头读状态并拒绝；`ExitPlanMode` 在回合内
挂住等用户回答，为此新开**一条** UI → app 的应答信道 —— 这是全部新增通信路径。

## Status

proposed（2026-10-31；设计见 [`docs/arch/plan-mode/design.md`](../arch/plan-mode/design.md)，
需求见 [`context.md`](../arch/plan-mode/context.md)）

## 背景

**要做的事**：让模型能主动进入「只读规划」状态 —— 先读代码、写计划，等用户批准后
再动手改。上游 Claude Code 有同名能力，本项目 `skills/` 下多个 skill 正文引用它，
而 `prompt.rs:224-228` 当前对模型声明「no plan mode」。

**结构障碍**（`context.md` §约束 已论证）：

1. **工具表恒定**（`toolset.rs:96`）。不能靠「Plan 时从 registry 摘掉写工具」实现 ——
   工具面一变，模型会把「工具消失」误读成「我记错了」。降级必须发生在**调用期**。
2. **`Request` 只在回合边界消费**（`ys-protocol/src/request.rs:11`）。因此「工具问到
   一半要用户回答」**不可能**走 `Request`：等它到时工具早已超时。
3. **`run_turn_once` 的 `select!`（`app_loop.rs:1457`）只等三路**（turn 本体 /
   `sink_rx` / 预算计时器），**不读 `request_rx`**。这解释了第 2 条为什么是硬的。
4. **`ToolContext` 的四个既有挂载点全是只读 `&'a`**（`crates/ys-tool/src/context.rs`）。
   `EnterPlanMode` 要**写**状态 ⇒ 需要第一个带内部可变的挂载点。

## 决策

0. **前提（已定，见 `context.md` 决策 5）**：沙箱由 seabox 解决，本仓只消费；
   **不定义沙箱抽象层**。seabox 是独立切片（换三处 spawn 调用），先于本设计落地。
   本 ADR **不含**沙箱内容 —— `Plan` 在无沙箱时也照常进入，只是它的「只读」
   由门禁而非内核保证。

1. **两档，默认 `Yolo`。** `Yolo` 就是本设计之前的生产行为（`main.rs:1112` 注入
   `AutoApprove`、seabox 未接、`bash` 无边界）—— 给它一个名字，而不是发明新语义。
   `PlanMode` 放 `ys-core`（`Request::SetPlanMode` 的载荷要它，而 `ys-protocol`
   已依赖 `ys-core`）；`#[serde(rename_all = "lowercase")]`。

2. **工具表不变，门禁在 `call()` 第一行。** `deny_if_plan(ctx.plan_mode, "<tool>")?`
   在 `write.rs:35` / `edit.rs:1072` / `bg_tools.rs:84` 各自开头。`None` 挂载点 ⇒
   放行（既有 `None` 约定同规）。**门禁立即生效**：`ToolContext` 每次调用新建，
   故 `EnterPlanMode` 本轮写入、本轮后续调用即可读到 —— 只有**状态栏**滞后到回合末。

3. **两个工具都不对称：进入自己改，退出等用户。**

   | 工具 | 改模式吗 | 返回什么 |
   |---|---|---|
   | `EnterPlanMode` | **自己改**，立即生效、幂等 | 「已进入」 |
   | `ExitPlanMode` | **不改**，问用户 | `switched` + 当前档位 + 行动指引 |

   不对称是刻意的：**进入是收紧**（模型误判的代价只是它自己写不了文件），
   **退出是放宽**（若模型能自行退出，那 plan 模式的保证就落在模型手里，等于自证无效）。

   工具结果第三句（`-> 按当前模式工作；Plan 不要再调本工具`）**不能省**：
   否则模型拿到 `switched: no` 会反复重试同一个工具。

4. **第四通道：一条单向应答信道，且任何 `select!` 都不加分支。**

   这是本 ADR **唯一**的新通信路径，拓扑：

   ```text
   app → UI   Outbound::PlanPrompt { text, resolved }   （复用既有 out_tx）
   UI → app   mpsc::Sender<PlanAnswer>                   （新，容量 4）
   ```

   **关键取舍**：`ExitPlanMode` 内部自己 `select!` 两路 —— 应答 `recv()` +
   250ms 定时器轮询 `boundary.is_aborted()`。`app_loop` 的 `run_turn_once` `select!`
   **不加第四路**。

   **为什么不用另一条路**（把等待挪到 `app_loop` 的 `select!` 里分发）：
   现有三路各带独立的 `if` 前置条件与公平性论证（事件那路必须用 `Some(..) =` 模式
   形式，否则信道关闭后忙等）；加第四路要重新论证全部四条，而收益只是「多一层间接」。
   唤醒源数量的代价是结构性的，不随收益线性增长。

   **为什么必须有 250ms 轮询**：`Boundary::Abort` 只**置标记**、不 drop future
   （ADR-0013「取消即消息」）。若工具不主动看标记，用户按 Esc 就毫无反应，
   只能等 180s 超时。

5. **`Outbound::PlanPrompt` 用 `resolved: bool` 双态，不用两个变体。**
   `false` = 提问，`true` = **收回**。收回信号防「工具已超时但 UI 还挂着」；
   两个变体会让「忘了处理其中一个」成为可能，一个变体 + 一个 bool 则漏不掉。

6. **超时不是新机制。** `basic.rs:456` 已有 `tokio::time::timeout(budget, tool.call(..))`，
   `budget = Tool::timeout() → limits.bash_timeout → 300s`（`basic.rs:109-116`）。
   本设计只实现 `ExitPlanMode::timeout() = Some(180s)`。取值理由：读一份长计划 + 想 +
   打 `yes` 的 p99 远小于 180s；300s 太久（用户离开会白等 5 分钟），60s 太短。

7. **`ExitPlanMode` 不收 `summary` 参数。** 输入 schema 为空对象 ⇒ 无长度上限问题
   （`context.md` Q6 的答案）。模型在本回合 assistant 消息里已写完计划、用户已读过；
   再复制一遍进工具参数是重复 token，且引入「超限时截断还是拒绝」这个两难
   —— 截断的确认文案比没有更糟。`MAX_SUMMARY_BYTES`（`subagent.rs:32`）**不是**先例：
   它限的是跨 agent 边界、父没看过原文的摘要。

8. **`Plan` 不是安全边界。** 它不传染子 agent：`child_tools`（`subagent.rs:1082`）
   独立构造，两个新工具**都不进子表** ⇒ 子 agent 没有模式工具，也就不会去改一个它
   读不到的状态。`Plan` 下派出的 `General` 子 agent **可写**。此句不进系统提示
   （模型不需要知道边界之外还有什么），但**必须**进用户文档与代码注释。

9. **模式是进程级状态，`/new` / `/clear` / `/resume` 都不改它。** 照
   `thinking_visible`（`app_loop.rs:341-345`）与 `theme`（`:347-359`）的先例：
   用户的期望是「换个话题」而非「退出 plan」。但 `/clear` 的输出行必须点明模式仍在
   —— 否则用户会疑惑。

10. **模式不进系统提示。** 系统提示在会话内不变（前缀缓存）。若提示里写「你在 plan
    模式」，用户 `/yolo` 后提示就是错的，且改提示会击穿整个缓存。模式说明由
    `EnterPlanMode` / `ExitPlanMode` 的 `ToolSpec.description` 承担 —— 那是随工具表
    走的、本来就要付缓存代价的位置。

11. **`prompt.rs` 的 `no plan mode` 从句必须删。** 工具落地后那句话变成谎话。
    **不**改成「有 plan mode」的新句子 —— 理由同决策 10。连带改
    `prompt.rs:1310` 与 `e2e_skills.rs:90` 两处逐字真值。

12. **`/plan` 幂等，`/yolo` 是「设成」而非 toggle**，两者都经
    `Request::SetPlanMode { plan }`（形状照 `SetThinking`）。**不做 `LocalAction`
    变体**：真值在 app 侧，TUI 只有镜像（`CodingView.plan`），走本地动作会让 TUI
    先改镜像再发请求 ⇒ 两条真值必然漂移。与 `SetThinking` 的唯一差别：`/plan`
    **要**在 transcript 里留一行（模式是安全属性，静默切换不可接受）。

13. **`build_view` 引入 `ViewInputs`，不加第 9 个形参。** `view.rs:39-40` 的既有注释
    已经写着「形参已达 8 个。**不再加参数**」，并要求「连同 `stats` /
    `session_started` / `theme` 一起收进 `ViewInputs`」。本设计触发那条注释。
    这是**唯一**的结构性重构，非本设计偏好。

## 后果

**正面**

- 工具表恒定 ⇒ 模型永远看得到 `write`，只是 `Plan` 下调用会被拒（带行动指引）。
- 回合热路径增量 **0**：`app_loop` / `run_turn_once` 的 `select!` 分支数不变。
- 门禁增量 ~2–5ns（一次虚调用），相对 `write::call` 的 20–200µs < 0.03%。
- 阻塞被结构性圈死：`ExitPlanMode` 的 `is_concurrency_safe() == false` ⇒ 落在
  `Segment::Serial` ⇒ 一次只有一个、且不与其他工具并发。这是编译期保证。

**负面 / 已知代价**（全部接受，不新增机制缓解）

| 代价 | 量化 | 理由 |
|---|---|---|
| 父表 12 → 14 ⇒ 所有既存会话前缀缓存一次性全 miss | 每会话一次 | 任何工具表变动都如此 |
| 等待期间占住 `Segment::Serial` | 上界 180s | 用户不在时超时 |
| 250ms 轮询 | 5s 内 20 次唤醒、< 20µs CPU | 相对模型调用可忽略 |
| 外层 `timeout` drop future ⇒ 收回信号可能不发 | UI 短暂挂在 `pending_plan_prompt` | 由 `Outbound::View.plan` 兜底，用户再敲一次即可 |
| `Plan` 不传染子 agent | `General` 子 agent 可写 | `Plan` 是防误触门禁，非安全边界 |

## 备选方案与拒绝理由

| 方案 | 做法 | 拒绝理由 |
|---|---|---|
| 工具表随模式增删 | `Plan` 时从 registry 摘掉写工具 | 违反 `toolset.rs:96`「工具面恒定」：模型会把「工具消失」误读成「我记错了」。降级一律在调用期 |
| `Request::SetPlanMode` 承载 `ExitPlanMode` 的等待 | 走既有请求信道 | `Request` 只在回合边界消费（`request.rs:11`），而 `run_turn_once` 的 `select!` 不读它 ⇒ 等它到时工具早超时 |
| 在 `run_turn_once` 的 `select!` 加第四路 | 由 `app_loop` 分发提问 | 要重新论证现有三路的全部前置条件与公平性；收益只是多一层间接（见决策 4） |
| `PlanPrompt` 通用化成 `PromptKind` 枚举 + 三个协议类型 | 为将来 `ApprovalHandler` 接 TUI 铺路 | 只有一个提问者、一个问题、一种答案 ⇒ 一个结构体够。`#[non_exhaustive]` 枚举是**猜**将来的形状 |
| 复用 `Outbound::Output` + 约定前缀 | 少一个变体 | UI 需要**结构化的 `resolved`** 才能清位；字符串前缀要 UI 解析协议，`SessionList` 的空列表语义正是因此做了变体 |
| UI 侧改模式真值 | 用户答 `yes` 时 TUI 自己改镜像再同步 | 两条写路径必然漂移（`/thinking` 是先例，但那条不需要即时真值）。单一写者：`app_loop` 的命令分支与 `PlanPromptImpl` |
| `ExitPlanMode` 收 `summary` 参数 | 让模型把计划正文传给工具 | 重复 token；且超限时截断（骗人）或拒绝（要重写）两难。用户已经读过原文 |
| 模式写进系统提示 | 让模型「始终知道」自己在哪档 | 系统提示会话内不变（前缀缓存）；`/yolo` 后提示就错，改提示击穿缓存 |

## 测试与变异

完整矩阵见 `design.md` §9（A 门禁 / B 阻塞有界 / C 第四通道 / D 装配契约 /
E 命令与状态栏 / F 模式存续 / G 变异脚本）。

三条**承重**断言，各自钉住一个决策：

1. `exit_plan_mode_declares_its_budget`：`timeout() == Some(180s)` —— 钉决策 6
   （变异：返回 `None` ⇒ 外层回落到 300s ⇒ 红）。
2. `abort_pierces_the_wait`：push `Abort` ⇒ < 400ms 返回「未切」—— 钉决策 4 的
   轮询分支（变异：删轮询 ⇒ 要等满 180s ⇒ 红）。
3. `submit_yes_sends_only_plan_answer`：`submit("yes")` 只发 `PlanAnswer`、不发
   `Prompt` —— 钉决策 4 的信道方向与 `submit` 的 `return`（变异：删 `return` ⇒ 红）。

建议 `python3 scripts/plan-mode-mutation.py` 注入 6 处，与
`cancellation-mutation.py` / `wheel-mutation.py` 同规。

## 关联

- `docs/arch/plan-mode/context.md` — 需求与已定决策（权威来源）
- `docs/arch/plan-mode/design.md` — 落地设计（本文的实现细节全在那里）
- ADR-0013 — 取消即消息（`Boundary::Abort` 只置标记、不 drop future ⇒ 本设计必须轮询）
- ADR-0010 — Agent 无状态（会话状态归接线器 ⇒ 模式归 `app_loop` 的进程级镜像）
- ADR-0021 — `ToolPlugin` 装配缝（本设计**不**建通用审批 UI，理由同 ADR-0014）
