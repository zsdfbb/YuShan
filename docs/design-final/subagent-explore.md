# subagent `explore` 模式 — As-built

> 性质：**实际建成记录**。设计见 [`docs/arch/subagent/design.md`](../arch/subagent/design.md)
> 的 **D14「只读子 agent 作为决策点」**（该文档记为「本轮待定」，本文是它的落地）；
> 探索速度的归因见 [`docs/arch/explore-speed/context.md`](../arch/explore-speed/context.md)，
> 决策见 [`ADR-0022`](../adr/0022-subagent-dispatch.md) / [`ADR-0029`](../adr/0029-subagent-landing-decisions.md)。
> 日期：2026-10-30

## 一句话

`subagent` 的 `mode="explore"` 是一个**只读、有硬预算、按固定契约回报 `path:line`** 的子 agent，
供父 agent 在「多个独立、每个都要多轮」的探索问题上成批并行派发，把调查过程挡在父上下文之外。

## 落点

```text
crates/ys-tool/src/subagent.rs        SubagentMode（:18；General 默认 / Explore）
apps/coding-agent/src/subagent.rs     EXPLORE_MAX_ROUNDS（:42）、EXPLORE_WALL_CLOCK（:49）、
                                      EXPLORE_FINAL_REPORT_GRACE（:55）、
                                      AppSubagentRunner（:587）、child_limits_for（:1005）、
                                      ChildTask（:1044）、child_tools（:1068）、
                                      parse_subagent_mode（:1561）
apps/coding-agent/src/prompt.rs       EXPLORE_REPORT_FORMAT（:105）、EXPLORE_SEARCH_METHOD（:121）、
                                      build_explore_prompt（:147）、render_guidelines（:285）
apps/coding-agent/src/assembly.rs     build 装配 explore_specs / explore_prompt（:141-171）
```

## 与 `general` 的三处差异

**工具集**：基础 9 元去掉 `write` / `edit` ⇒ 7 元（`child_tools`，`subagent.rs:1068`）。
保留 `bash` —— 脚本聚合是唯一被证明有效的探索机制；代价是 `bash` 仍可写，只读靠系统提示约束，不做沙箱。

**系统提示**（`build_explore_prompt`）：角色行改为「调查并带引用回报」；`render_guidelines(.., false)`
去掉 narration 指引；**不加载 `<project_context>`**（AGENTS.md ≈ 6.8k token 的每轮前缀成本）；
追加检索方法论 `EXPLORE_SEARCH_METHOD` 与有界输出契约 `EXPLORE_REPORT_FORMAT`。

**硬预算**：`EXPLORE_MAX_ROUNDS = 16` 与 `EXPLORE_WALL_CLOCK = 300s`，分别与父配置的
`max_rounds` / 子墙钟预算取 `min`（`child_limits_for`、`ChildTask::execute`）；`general` 不受影响。
两者是**或**关系：任一先到即触发收卷。

**收卷轮**（`final_report_prompt`）：预算到点后**不直接切断**，而是注入一条收卷指令
（`EXPLORE_FINAL_REPORT_PROMPT` + 固定尾句，要求先回答、再列 `file:line` 级发现、并标明
未能确认的部分），再跑一轮。收卷轮**保留原工具表** —— `tools` 在请求最前面，一变即整段
前缀失效，一次收卷会把几十万 token 按全价重算；但保留工具表意味着模型仍可发起工具调用，
故收卷轮**只给一次**：之后无论产出什么（含又调了工具）都强制收场。
`EXPLORE_FINAL_REPORT_GRACE = 60s` 是给收卷轮自己的墙钟宽限，避免一个在预算到点前一瞬
进入的收卷轮立刻被判超时。归因：轮数到点 ⇒ `StopReason::MaxRounds`；墙钟到点 ⇒
`StopReason::Cancelled`（预算到点与子 agent watchdog 同口径）。

## 派发策略

父 agent 的 guideline（门控 `subagent` 在场，`prompt.rs:472`、`:528`）与两个派发工具的 `mode`
参数描述（`subagent.rs:1601`、`:1697`）都写明：只在**多个独立、每个都要多轮**的探索上成批派发
（同一条消息里的多个调用进并行段）；一两个小查找自己做，冷启动比查找本身更贵。

## 交付链路

父 agent 收到的是 [`transcript_summary`] 的返回值（非整份转录），依次回退：

1. `final_message`（回合最终答复）；
2. 最后一条有正文的 `Role::Assistant` 消息；
3. **最后一条工具结果**（`tool_text_of`）。

第 3 条是关键的：`text_of` 只认 `ContentBlock::Text`，而探索的成果绝大多数落在
`ToolResult` 里 —— 实测 8 轮里只有第 1 轮有 assistant 正文（一句「我先读设计文档」），
其余全是工具输出。缺了第 3 条，撞上限时父 agent 只会拿到那句开场白。

## 实测读数

3 个有界问题、同机同模型：

| 做法 | 墙钟 | 轮数 | 子 agent 时长 | 子 agent 转录 |
|---|---|---|---|---|
| 父 agent 自己做 3 问 | **37 s** | 6 | — | — |
| 派 3 个 explore 子 agent | 100 s | 4 | 32 / 37 / 55 s | 23 / 42 / 69 KB |

修复 `bash` 继承 stdin 之前，同样配置下子 agent 会卡在 300 s 预算上（`rg` 无路径时读 stdin），
转录达 217–532 KB。

## 已知边界

- 每问固定开销约 25–30 s ⇒ 小任务比父 agent 自己做慢（上表 2.7×），只在大规模独立探索上划算。
- `bash` 未被结构性限制为只读（本仓不做沙箱），只读靠系统提示。
- 轮数是真正的约束而非时间：8 轮实测只用 15–37 s，远未逼近 300 s。故上限取 16 轮 ——
  多数探索在第 8 轮时已把「知道去哪找」变成「还没读完」。
- 收卷轮只跑一轮，故它救得回的是**已经拿到的内容**，救不回「还差两轮就能确认」的部分；
  后者需要下一次派发时把问题切得更小。
- `fp_tools` 指纹锚（`toolset.rs:611`、`:687`）只覆盖基础 9 元表，不含父表追加段
  （`subagent` / `subagent_async` / `bash_bg`）⇒ 这些工具的 schema 漂移目前不触发锚点。
