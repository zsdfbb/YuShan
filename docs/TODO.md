# 未完成工作清单

> 性质：**跨轮次的工作台账**，不是设计文档。每条只回答「还差什么」+「凭什么说它还没做」，**不复述设计**。
>
> 收录判据：ADR 已 accepted 或设计已写完，但**代码 / 文档里确实还没有**的部分。每条必须带指针 ——
> 断言「X 还没做」时引 ADR / design 的**待办标注**，或引一次可复现的**全仓 grep**。
>
> 维护约定：**完成即删条目**（不写「已完成」），对应设计计划归档到 `docs/design-plans/completed/`
> 或 `docs/exec-plans/completed/`。新增条目前先问一句：这是**真的待办**，还是**有意不做**？
> 后者属于设计决策，该进 ADR，不进本清单。
>
> 基线：`master` @ `5af317e`（核实于 2026-10-17）。

---

## A. 已设计、按阶段主动推迟（离得最近）

### A1. `--debug` 结构化日志 P1 / P2

P0 已落地；P1、P2 是**设计文档自己排的阶段**，标注见
`docs/arch/agent-observability/design.md` §5（P1 / P2）。

| 阶段 | 缺什么 | 状态依据 |
|---|---|---|
| P1 | **适配器自检**（启动期检测旁路静默退化的缓解措施） | `design.md` §5 标 ⏳，全仓无自检代码 |
| P1 | T5 的「进程被 `kill -9`」不 flush 退出 | `docs/exec-plans/2026-10-17-debug-log.md:454` 仍为 `[ ]` |
| P2 | `state_change` 记录 | 全仓源码 grep `state_change` **零命中** |
| P2 | jq 配方文档 | `design.md` §5 标 ⏳ |

> P1 的 `model_attempt` / `context_compacted` / `context_compaction_failed` / `response_truncated`
> **已落地**（`apps/coding-agent/src/debug_log.rs:508`、`main.rs:713`、`main.rs:723`），不在本清单内。

### A2. debug-log 执行计划的文档归档

`docs/exec-plans/2026-10-17-debug-log.md` 仍在 `exec-plans/` 根下（对比：其余十二份已在
`exec-plans/completed/`）。验收报告见 `docs/reports/2026-10-17-debug-log-report.md`。

### A3. subagent 落地的成本数字回填（N8 / 刀 7）

`docs/design-plans/2026-10-09-subagent-landing.md:74` —— 需真实凭证跑一次派发，把读数写回
`docs/arch/subagent-rebaseline/design.md` §6.4。标记 `MANUAL_ACK_REQUIRED`。

> 该计划的刀 0–6 **已落地**（`SubagentRunner` 全仓 28 处命中，`toolset.rs:592` 的父表 10 元断言存在）。
> 因此 `docs/design-plans/2026-10-09-subagent-landing.md` 本身待归档到 `design-plans/completed/`。

### A4. 子 agent 只读角色（多角色定义）

`docs/arch/subagent/design.md:827` 的 **P3（条件触发，本轮不做）** 表列出「多角色 agent 定义
（frontmatter / 配置，含只读角色）」，触发条件 = 出现**第二个**内置角色的真实需求（`:834`）。
设计已留口子：**子 agent 能力完全由创建时给它的工具表决定，不需要新机制**（`:647` D14 裁定；
ADR-0022（accepted）`:132-133` 同文）。

| 缺什么 | 状态依据（核实于 `e6f5f13`） |
|---|---|
| 角色概念 / 只读开关 | 全仓源码 grep `AgentDefinition` / `agent_role` **零命中**（`apps/coding-agent/src/toolset.rs:515`、`apps/coding-agent/src/prompt.rs:699` 的 `read_only` 命中是**并发安全**测试名，非角色定义） |
| `build_tools` 无法按角色换表 | 签名零策略参数（`apps/coding-agent/src/toolset.rs:88`）；全仓**仅两处**构建工具表：装配期 `apps/coding-agent/src/assembly.rs:100`（子表）与派发期 `apps/coding-agent/src/subagent.rs:378`（每次派发重建） |

> ⚠️ **落地约束**：不可把角色做成 `subagent` 工具入参 —— 那会改 `ToolSpec`，动
> `apps/coding-agent/src/toolset.rs:568` 的**前缀指纹锚**，且破掉「子表 JSON 是父表字节前缀」
> 这条缓存优化（`apps/coding-agent/src/assembly.rs:119` 派发工具追加末位即为此）。
> 宜做成**装配期多张预建表**（指纹只增不改）。
>
> **边界**：同表 `:835`「按任务类型动态裁剪子工具表」是**另一项**（R > 5 才回本，且破前缀），
> 不随本项一起做。

---

## B. ADR 已 accepted，但代码零落地

| 项 | ADR（accepted） | 全仓源码证据 | 计划 |
|---|---|---|---|
| **Follow-up（追问）** | ADR-0014（`0014-...md:11`，2026-09-15） | grep `follow_up` / `FollowUp` = **0** | `docs/design-plans/2026-09-15-follow-up.md` |
| **长程 todo + `ToolPlugin` 装配缝** | ADR-0024（`:7`）supersede 0021 决策 5；ADR-0021（`:7`）决策 ③ | grep `ToolPlugin` = **0**、`todo.json` = **0**、`should_inject` = **0**；`adapters/tools-basic/src/` 只有 7 个文件工具，无 `todo.rs` | `docs/arch/long-horizon-todo/design.md`、`docs/arch/hooks-and-plugins/design.md` |

**这两项相互咬合**（0024 的推动判据「有未完成项」依赖 0014 的 follow-up 机制），
宜按 **follow-up → todo** 的顺序推进。

**未落地但已在别处补上的**：ADR-0021 的前置条件「prefix 指纹测试」已由 subagent 刀 0 于
`apps/coding-agent/src/toolset.rs:592` 一带补齐，**不再计入**。

---

## C. 卡在「待用户裁决」（arch-explore 已完成，方案设计未启动）

以下目录**只有 `context.md`**，末尾均带一份未答的「未澄清问题」：

| 目录 | 卡点 | 指针 |
|---|---|---|
| `docs/arch/input-history/` | 8 个未决问题；**Q4 与现行 as-built 硬冲突**（`Up`/`Down` 现归聊天滚动，`crates/ys-tui-coding/src/events.rs:187`） | `context.md:139` |
| `docs/arch/bash-cwd-contract/` | Q7（短路路径是否补 cwd，会改「用户全权接管提示词」契约）、Q8（头部信息块收哪几项） | `context.md:747` |
| `docs/arch/llm-api-unavailable/` | `ModelError` 是否加变体（breaking，需确认外部影响面）、错误文案形态、超时后会话状态、Ctrl-C 语义 | `context.md:161` |
| `docs/arch/per-turn-model-switch/` | 形态未定：B1 内联指令 vs B2 workflow 文件；回合归因形态未定 | `context.md:179` |
| `docs/arch/tool-call-efficiency/` | Q1 范围、Q2 多目标参数面、Q3 布局快照、Q4 验收口径、Q5 措辞归属 | `context.md:199` |
| `docs/arch/etf-display-tool/` | 报告目录是否可配、ECharts CDN 降级；**`adapters/tools-finance` crate 不存在**（`Cargo.toml` 成员表无此项） | `context.md:99` |
| `docs/arch/prompt-delivery/notes.md` | 唯一未决项：**产品内置 vs 项目可配** | `notes.md:98`（文档头自标「状态：未决」） |

---

## D. 其他尾巴

- **`explore-speed` 的三条非目标**（`docs/arch/explore-speed/design.md:41`）：按 bash 命令契约判
  只读、拆第二个并发常量、动 `Tool::timeout()` 契约面 —— 本轮明确不做，需单独论证才重开。
- **ADR 编号断档**：`docs/adr/` 缺 `0005` / `0006` / `0040`（`0005` 实际在
  `docs/arch/coding-agent/adr-0005-infrastructure-approach.md`）。需确认是历史沿革还是遗漏。
- **`docs/arch/tui-turn-spacing/` 已实施但未归档**：代码在
  `crates/ys-tui-coding/src/draw.rs:56`（`USER_GAP_ROWS = 2`），文档停在 `context.md`。
