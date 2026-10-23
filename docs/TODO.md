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
> 基线：`master` @ `7fd655b`（核实于 2026-10-21）。A1–A4 / B / C / D 沿用 `5af317e`（2026-10-17）的核实结果。
> A5（execution-efficiency 的落地）已在 `825cd82` 落地并删除（合并进 `master`）。
> A1–A4 / B / C / D 的核实结果**未**对 `825cd82` 重新核过 —— 基线仍是上行的 `7fd655b`。

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

## B. ADR 已 accepted，但代码零落地

| 项 | ADR（accepted） | 全仓源码证据 | 计划 |
|---|---|---|---|
| **Follow-up（追问）** | ADR-0014（`0014-...md:11`，2026-09-15）；**协议格已由 ADR-0047 备好**（`Delivery{origin: System}`） | grep `follow_up` / `FollowUp` = **0** | `docs/design-plans/2026-09-15-follow-up.md`；协议见 `docs/adr/0047` |
| **长程 todo + `ToolPlugin` 装配缝** | ADR-0024（`:7`）supersede 0021 决策 5；ADR-0021（`:7`）决策 ③ | grep `ToolPlugin` = **0**、`todo.json` = **0**、`should_inject` = **0**；`adapters/tools-basic/src/` 只有 7 个文件工具，无 `todo.rs` | `docs/arch/long-horizon-todo/design.md`、`docs/arch/hooks-and-plugins/design.md` |

**这两项相互咬合**（0024 的推动判据「有未完成项」依赖 0014 的 follow-up 机制），
宜按 **follow-up → todo** 的顺序推进。

**Follow-up 的根因（已查明，2026-10-22 核实）**：「排到下一回合」这一路**不是缺实现，是缺协议格**——
ADR-0013 删掉了旧 `Inbox` 的 `follow_up` 路（`docs/adr/0013-cancel-token-removal-boundary-source.md:37`），
而 `Boundary` 枚举只有 `Steer` / `Abort` 两格（`crates/ys-protocol/src/boundary.rs:12-16`）。
ADR-0013 发生在 coding-agent TUI 落地**之前** ⇒ 产品需要该语义时协议已被收敛掉。
详见 `docs/arch/input-delivery-latency/context.md`。

⚠️ **本段描述的是 2026-10-22 上午的 as-built，当日傍晚已部分过时**：
「回合中提交的输入被排到整个回合结束」「`Steer` 在生产侧无生产者」这两条**已不成立** ——
`8a08bd6` 接上了 `Steer` 生产者（回合中提交普通消息 ⇒ 投 `Boundary::Steer`），
并把回合头部排空从「无差别丢弃」改为「只丢 `Abort`、保留 `Steer`」。

**仍然成立的那一半**：**「下一回合」语义仍缺协议格与队列** ——
`Boundary` 仍无 `NextTurn` 一格，app 侧无跨回合队列 ⇒ 系统发起的 follow-up 无处可去。
`grep follow_up / FollowUp` 仍为 **0**。

**根因的解法已定（2026-10-22，ADR-0047）**：
投递去向做成协议一等公民（`DeliveryTarget` / `Delivery`，`ys-protocol` 新文件），
队列住在 **app 侧**（`app_loop`）**跨回合存活** ⇒ 既是「绝不丢失」的保证，
也是 follow-up 缺的**协议格**（`Delivery{origin: System}`）。
⇒ 本条从「代码零落地」变为「**协议已备，实现待做**」。

**已落地的相关修复（2026-10-22，ADR-0049）**：
「模型正要收尾时到达的插话**静默消失**」这个丢消息故障已修 ——
轮边界拉取抽成 `drain_boundary(ctx)`，在**每轮开头**与**收场前**两处调用
⇒「轮边界总是存在」成为真不变量，ADR-0047 决策 1 的依据成立。
详见 `docs/adr/0049-final-answer-boundary-drain.md`。

设计与变更清单：`docs/arch/input-delivery-latency/design.md` §6。

**未落地但已在别处补上的**：ADR-0021 的前置条件「prefix 指纹测试」已由 subagent 刀 0 于
`apps/coding-agent/src/toolset.rs:592` 一带补齐，**不再计入**。

---

## C. 卡在「待用户裁决」（arch-explore 已完成，方案设计未启动）

以下目录**只有 `context.md`**，末尾均带一份未答的「未澄清问题」：

| 目录 | 卡点 | 指针 |
|---|---|---|
| `docs/arch/input-history/` | ⚠️ **会话内部分已解**（2026-10-23）：**仅进程内、不落盘、不跨会话** —— 输入框空时 `↑` 取回、`↓` 还原草稿（`crates/ys-tui-coding/src/history.rs`）。<br>· ✅ Q4（键位）已裁决：输入框为空/浏览态归历史，否则归聊天滚动；<br>· ⬜ Q2 / Q3 / Q5–Q8 未做：跨会话持久化、按 cwd 分片、去重上限以外的规则、隐私过滤、落盘时机、归属层 | `context.md:139` |
| `docs/arch/bash-cwd-contract/` | Q7（短路路径是否补 cwd，会改「用户全权接管提示词」契约）、Q8（头部信息块收哪几项） | `context.md:747` |
| `docs/arch/llm-api-unavailable/` | `ModelError` 是否加变体（breaking，需确认外部影响面）、错误文案形态、超时后会话状态、Ctrl-C 语义 | `context.md:161` |
| `docs/arch/per-turn-model-switch/` | 形态未定：B1 内联指令 vs B2 workflow 文件；回合归因形态未定 | `context.md:179` |
| `docs/arch/tool-call-efficiency/` | Q1 范围、Q2 多目标参数面、Q3 布局快照、Q4 验收口径、Q5 措辞归属 | `context.md:199` |
| `docs/arch/etf-display-tool/` | 报告目录是否可配、ECharts CDN 降级；**`adapters/tools-finance` crate 不存在**（`Cargo.toml` 成员表无此项） | `context.md:99` |
| `docs/arch/prompt-delivery/notes.md` | 唯一未决项：**产品内置 vs 项目可配** | `notes.md:98`（文档头自标「状态：未决」） |
| `docs/arch/input-delivery-latency/` | ⚠️ **部分已解**（2026-10-22）：<br>· ✅ **G2**（F2）已修 —— 「模型正要收尾时到达的插话静默消失」已修，见 `docs/adr/0049-final-answer-boundary-drain.md`；<br>· ✅ **G4**（F4）已解 —— 随 `8a08bd6` 落地；<br>· ⬜ **G1 / G3 / G5 / G6** 未做 —— app 侧 `Delivery` 队列、降级投递的触发机制、`origin` 是否外送、状态机载体；<br>· ⬜ 设计 §7 仍有 **3 项「待用户确认」**（命令与消息谁先 / 回合中命令是否即时拒绝 / 投递状态显示落点） | `review.md` §7 闸门表、`docs/adr/0047-input-delivery-target-and-queue.md`、`docs/adr/0049-final-answer-boundary-drain.md` |

---

## D. 其他尾巴

- **TUI 鼠标：只做了滚轮**（[ADR-0050](../adr/0050-tui-mouse-wheel-scroll.md)）：
  - ⬜ **模态浮层里的滚轮**：浮层自己读事件（`crates/ys-tui-coding/src/run.rs` 的
    `Prompter` 实现），`_ => {}` 掉鼠标 ⇒ 会话列表 / 模型列表超过一屏时**无法滚动**，
    只能靠方向键逐项移动（跨屏时会自动跟高亮）。补法：在 `select_modal` 的匹配里加
    `Event::Mouse` 臂改 `top` / 高亮。依据：ADR 风险表的「模态浮层里的滚轮」一行标**未做**。
  - ⬜ **`⇧`+拖选可用性的人工确认**：捕获期间裸拖选失效，`⇧`+拖选在 iTerm2 /
    Terminal.app / WezTerm / VSCode 集成终端下的实际表现均**未验证**（ADR-0050 的
    `MANUAL_ACK` 项）。不达标时的退路：只在滚轮事件发生时才临时关捕获（成本高，需实测）。

- **bg-notify 的 R1 技术债：投递机制不外溢**（ADR `docs/arch/bg-notify/adr-bg-notify.md`）：
  「落点格 + `Delivery` 三态 + take/claim/Finish」现在长在 `adapters/tools-basic/src/background.rs`
  上，只为后台 `bash` 一个生产用例服务。**触发重开的条件**：出现**第二个**后台任务类型
  （MCP 长任务、后台 LSP、后台 subagent 的另一种形态）⇒ 那时把这三块抽成
  `ys-protocol` 的 `DeferredSink`（候选 B 的 §2.2）。不抽的代价：那 30 行会被复制第二遍。
- **`kill_group` 仍是同步 fork**：已从锁内挪到锁外（ADR 同上），但仍是
  `Command::new("sh")...status()` 的同步等待。`proc_group::kill_group_async` 已存在，
  要彻底解决需把 `sweep` / `snapshot` 的签名改异步。

- **PTC（程序化工具调用）—— 长期方向，本轮明确不做**（ADR-0048 决策）：
  模型写一段代码调工具、**只有 curated 结果进模型历史** ⇒ N 次工具调用收敛为**一轮**模型往返。
  是四家同类（deepseek-harness `core/tools/src/ptc.ts:1-8`）里**唯一能改变轮次数量级**的机制，
  本仓零。**不做它的理由**：需执行边界，与本仓「不做安全（沙箱/隔离/权限/审批由上层解决）」
  的硬约束正面冲突，且约 2,500+ 行。**触发重开的条件**：上层项目给出执行边界，
  或轮数差距（同题对照 codex 6 轮 vs 我们 33 轮）成为主要瓶颈。
  设计依据 `docs/arch/session-latency/design.md` §4.3 决策点 1。
- **`docs/arch/session-latency/` 的 D2（subagent 工具表约束）未做**：子 agent 现状拿到与父
  **完全相同的 9 个工具**（`apps/coding-agent/src/subagent.rs:421`），需按角色换表
  （ADR-0022 D14 已裁定「能力 = 创建时给的工具表」）。**但受前缀指纹锚约束**
  （`apps/coding-agent/src/toolset.rs:568`）⇒ 须**装配期预建多张表**，
  不可把角色做成 `subagent` 工具入参（会动 `ToolSpec`、破「子表 JSON 是父表字节前缀」）。
  另需满足 D15「提示词不可见 + 执行时拒绝」双实现。**需先走 `arch-design`**。

- **`toolset.rs` 的 `PATH` 改写会波及不持锁的用例**：`apps/coding-agent/src/toolset.rs:193`
  的 `set_path` 把进程 `PATH` 指向空目录（它自己持 `env_lock` + RAII 还原，纪律是对的），
  但**别的**用例若要 `spawn` 一个按 `PATH` 解析的二进制（`sh` 等）而**不持同一把锁**，
  就会在窗口期内拿到 ENOENT ⇒ 表现为「命令静默启动失败、留下孤儿进程」。
  本仓后台命令用例已整段持 `env_lock` 绕开，**隐患本身仍在**（`AGENTS.md`「测试隔离」条）。
- **`explore-speed` 的三条非目标**（`docs/arch/explore-speed/design.md:41`）：按 bash 命令契约判
  只读、拆第二个并发常量、动 `Tool::timeout()` 契约面 —— 本轮明确不做，需单独论证才重开。
- **ADR 编号断档**：`docs/adr/` 缺 `0005` / `0006` / `0040`（`0005` 实际在
  `docs/arch/coding-agent/adr-0005-infrastructure-approach.md`）。需确认是历史沿革还是遗漏。
- **`docs/arch/tui-turn-spacing/` 已实施但未归档**：代码在
  `crates/ys-tui-coding/src/draw.rs:56`（`USER_GAP_ROWS = 2`），文档停在 `context.md`。
