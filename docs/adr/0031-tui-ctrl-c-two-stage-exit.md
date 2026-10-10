# 0031 — TUI 里 Ctrl-C 两段式：第一次请求取消，第二次退出

## Status

accepted（2026-10-10）

## 背景

- **需求（原话）**：「进入交互阶段后，我没有办法杀死 coding-agent」。
- **复现**（本地黑洞模型端点 + PTY 真跑二进制）：turn 进行中按 Ctrl-C —— UI 不动、进程不退；
  连按多次同样无效。对照组：**空闲态** Ctrl-C 正常退出；turn 中 **Ctrl-D** / `/quit` 也能退出
  （但两者对用户不可见，前者任何提示都没有，后者要先想到敲命令）。
- **根因**：`events.rs` 把「turn 中的 Ctrl-C」路由成 `Boundary::Abort`。`Abort` 是**消息**，
  只在**轮边界**（`crates/ys-loop/src/basic.rs` Step 1 / Step 3 / Step 4b）与**工具轮询**
  （`adapters/tools-basic/src/exec.rs`，100ms）里被读 —— **模型调用期间无人读它**。
  默认 `model_timeout.idle_secs = 120` × 有界重试 ⇒「按了没反应」的窗口以**分钟**计。
  ADR-0028 已把这条约束写进 Consequences（「退避期间 Ctrl+C 无效」「TUI 黑洞挂起…此前需
  Ctrl-D 强退」），但 UI 侧**没有**把它变成可发现、可执行的退出手段 —— 于是在用户手里，
  Ctrl-C 这个「任何程序都能杀死」的键失效了。
- **不可在本 ADR 里动的约束**：`Model::complete` 没有取消入参（ADR-0028 决策 4 末条明确
  「不做」——改核心 trait 才治）⇒ **模型调用期间无法立刻让 turn 收场**。本决策只保证
  「总有办法杀死进程」，不承诺「立刻取消」。

## 决策

1. **turn 中 Ctrl-C 两段式**：第一次 → 保持现状（投 `Boundary::Abort`）并写一行 transcript
   回执（「已请求取消当前回合；若长时间无响应，再按一次 Ctrl-C 退出」）；第二次 → `should_quit = true`，
   走**与 Ctrl-D 完全相同**的退出路径（UI 循环 break → 恢复终端 → app 侧 `rt.shutdown_timeout` 收摊）。
   空闲态 Ctrl-C 退出不变。
2. **`App.abort_requested` 随回合生命周期复位**：任何进入 turn 的路径（`begin_local_turn`、
   迟到回声、复核回合）与 `finish_turn` 都清零。跨回合残留会让**下一回合的第一次** Ctrl-C 直接退出。
3. **不在本轮给 `Model::complete` 加取消入参**（维持 ADR-0028 决策 4）。「模型调用期间取消延迟」
   的根治另案（见 Considered Options 第三条）。

## Considered Options

- **Ctrl-C 一律退出，Esc 独占「取消当前回合」**：一步到位、最符合终端惯例，但夺走 ADR-0008 起
  就存在的「Ctrl-C 取消当前回合」，且长回合误按一次即丢失整个回合。**不采纳**。
- **只加提示、不改语义**（例如状态行常驻「Ctrl-D 退出」）：杀死进程仍是「知道内幕的人才能做的事」，
  不满足需求。**不采纳**（提示仍保留，作为第一段的可见回执）。
- **让 `BasicLoop` 在模型调用期间轮询 `is_aborted`**（与 `exec.rs` 同款，命中即丢弃 in-flight
  model future + `RunFinished{Cancelled}`）：**这才是治本**，能让第一段 Ctrl-C 立刻生效；但它改的是
  最高爆炸半径的 crate、且与 ADR-0028 决策 4 的结论正面相交（需自己的 ADR）。本轮**不做**，
  作为后续候选记账 —— 用户当前诉求是「有办法杀死」，不是「取消要够快」。

## Consequences

- Ctrl-C 在**任意状态**（空闲 / turn 中 / 模型调用挂住 / 工具长跑）最多再按一次即可退出进程；
  「取消」语义与「退出」语义各自保留，互不遮蔽。
- transcript 会多一行回执（只在 turn 中第一次按 Ctrl-C 时出现一次，不刷屏）；`App` 多一个
  随回合复位的字段。
- **「取消即消息」不变量未变**：第一段 Ctrl-C 在模型调用期间依然可能长时间不生效 —— 差别是
  现在有可见回执 + 可靠的第二段。
- 遗留（记账）：模型调用期间「取消」仍要等到该次调用返回（或超时）才被读到；根治见 Considered Options。
- **回归测试**：`events::tests::test_ctrl_c_second_press_quits_while_turning`、
  `app::tests::test_abort_request_resets_with_turn`（两者都做过变异验证：去掉第二段退出 / 去掉
  回合结束复位 ⇒ 变红）。

## 参考

- As-built 形态与微决策表：[`docs/design-final/coding-agent-tui.md`](../design-final/coding-agent-tui.md) §2、§8.2 M17
- 取消语义：[`0013-cancel-token-removal-boundary-source.md`](0013-cancel-token-removal-boundary-source.md)、
  [`0008-agent-cancel-handle.md`](0008-agent-cancel-handle.md)
- 模型调用超时/重试与「退避不可中止」：[`0028-model-call-retry-and-timeout.md`](0028-model-call-retry-and-timeout.md)
