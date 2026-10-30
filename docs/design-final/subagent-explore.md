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
apps/coding-agent/src/subagent.rs     EXPLORE_MAX_ROUNDS（:37）、EXPLORE_WALL_CLOCK（:40）、
                                      AppSubagentRunner（:579）、child_limits_for（:992）、
                                      ChildTask（:1036）、child_tools（:1060）、
                                      parse_subagent_mode（:1498）
apps/coding-agent/src/prompt.rs       EXPLORE_REPORT_FORMAT（:105）、EXPLORE_SEARCH_METHOD（:121）、
                                      build_explore_prompt（:147）、render_guidelines（:285）
apps/coding-agent/src/assembly.rs     build 装配 explore_specs / explore_prompt（:141-171）
```

## 与 `general` 的三处差异

**工具集**：基础 9 元去掉 `write` / `edit` ⇒ 7 元（`child_tools`，`subagent.rs:1060`）。
保留 `bash` —— 脚本聚合是唯一被证明有效的探索机制；代价是 `bash` 仍可写，只读靠系统提示约束，不做沙箱。

**系统提示**（`build_explore_prompt`）：角色行改为「调查并带引用回报」；`render_guidelines(.., false)`
去掉 narration 指引；**不加载 `<project_context>`**（AGENTS.md ≈ 6.8k token 的每轮前缀成本）；
追加检索方法论 `EXPLORE_SEARCH_METHOD` 与有界输出契约 `EXPLORE_REPORT_FORMAT`。

**硬预算**：`EXPLORE_MAX_ROUNDS = 8` 与 `EXPLORE_WALL_CLOCK = 300s`，分别与父配置的
`max_rounds` / 子墙钟预算取 `min`（`child_limits_for`、`ChildTask::execute`）；`general` 不受影响。

## 派发策略

父 agent 的 guideline（门控 `subagent` 在场，`prompt.rs:472`、`:528`）与两个派发工具的 `mode`
参数描述（`subagent.rs:1533`、`:1629`）都写明：只在**多个独立、每个都要多轮**的探索上成批派发
（同一条消息里的多个调用进并行段）；一两个小查找自己做，冷启动比查找本身更贵。

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
- `fp_tools` 指纹锚（`toolset.rs:611`、`:687`）只覆盖基础 9 元表，不含父表追加段
  （`subagent` / `subagent_async` / `bash_bg`）⇒ 这些工具的 schema 漂移目前不触发锚点。
