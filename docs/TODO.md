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
> 基线：`master` @ `39f4f95`（核实于 2026-10-27）。A1–A4 / B / C / D 的核实结果均在此基线重核过。

---

## A. 已设计、按阶段主动推迟（离得最近）

### A1. `--debug` 结构化日志 P1 / P2

P0 已落地；P1、P2 是**设计文档自己排的阶段**，标注见
`docs/arch/agent-observability/design.md` §5（P1 / P2）。

| 阶段 | 缺什么 | 状态依据 |
|---|---|---|
| P1 | **适配器自检**（启动期检测旁路静默退化的缓解措施） | `design.md` §5 标 ⏳，全仓无自检代码 |
| P2 | `state_change` 记录 | 全仓源码 grep `state_change` **零命中** |
| P2 | jq 配方文档 | `design.md` §5 标 ⏳ |

> P1 的 `model_attempt` / `context_compacted` / `context_compaction_failed` / `response_truncated`
> **已落地**（`apps/coding-agent/src/debug_log.rs:508`、`main.rs:713`、`main.rs:723`），不在本清单内。
>
> P1 的「进程被 `kill -9` 不 flush 退出」**已不成立**：`debug_log.rs:922` 的 `write_line`
> 直接 `write_all` 到文件、不经过 `BufWriter`，进程被杀最多丢整行、不留半行
> （`docs/reports/2026-10-17-debug-log-report.md:22` 已把这一条记为 P0 验收事实）。

### A2. subagent 落地的成本数字回填（N8 / 刀 7）

`docs/design-plans/completed/2026-10-09-subagent-landing.md:74` —— 需真实凭证跑一次派发，把读数写回
`docs/arch/subagent-rebaseline/design.md` §6.4。标记 `MANUAL_ACK_REQUIRED`。

> 该计划的刀 0–6 **已落地**（`SubagentRunner` 全仓 28 处命中，`toolset.rs:592` 的父表 10 元断言存在）。

### A3. 子 agent 只读角色（多角色定义）

`docs/arch/subagent/design.md:827` 的 **P3（条件触发，本轮不做）** 表列出「多角色 agent 定义
（frontmatter / 配置，含只读角色）」，触发条件 = 出现**第二个**内置角色的真实需求（`:834`）。
设计已留口子：**子 agent 能力完全由创建时给它的工具表决定，不需要新机制**（`:647` D14 裁定；
ADR-0022（accepted）`:132-133` 同文）。

| 缺什么 | 状态依据（核实于 `39f4f95`） |
|---|---|
| 多角色定义（frontmatter / 配置） | 全仓源码 grep `AgentDefinition` / `agent_role` **零命中**（2026-10-30 重核）。**但「角色概念」本身已部分落地**：`SubagentMode { General, Explore }` 已是公开类型（`crates/ys-tool/src/subagent.rs:18`），`Explore` 由工具表过滤实现（`apps/coding-agent/src/subagent.rs:1060` `child_tools` 按名字去掉 `write` / `edit`）。缺的是**第三个角色**与**配置驱动**，不是「只读」这件事 |
| ~~「只读开关」零命中~~ | 旧依据 `toolset.rs:517` / `prompt.rs:782` **已失效**（2026-10-30 重核：前者现在是 `only_read_only_tools_are_concurrency_safe` 的文档注释、后者是 `tool_list_is_derived_from_specs_order_verbatim`；现存的 `read_only` 用例是 `prompt.rs:997`）。该依据曾把 explore 的落地读成「零命中」，误判过一项状态 |
| `build_tools` 无法按角色换表 | 签名零策略参数（`apps/coding-agent/src/toolset.rs:88`）；生产路径全仓**仅两处**构建工具表：装配期 `apps/coding-agent/src/assembly.rs:143`（子表）与派发期 `apps/coding-agent/src/subagent.rs:980`（每次派发重建） |

> ⚠️ **落地约束**：不可把角色做成 `subagent` 工具入参 —— 那会改 `ToolSpec`，动
> `apps/coding-agent/src/toolset.rs:570` 的**前缀指纹锚**，且破掉「子表 JSON 是父表字节前缀」
> 这条缓存优化（`apps/coding-agent/src/toolset.rs:647` 的断言即为此）。
> 宜做成**装配期多张预建表**（指纹只增不改）。
>
> **边界**：同表 `:835`「按任务类型动态裁剪子工具表」是**另一项**（R > 5 才回本，且破前缀），
> 不随本项一起做。

## B. ADR 已 accepted，但代码零落地

| 项 | ADR（accepted） | 全仓源码证据 | 计划 |
|---|---|---|---|
| **Follow-up（追问）** | ADR-0014（`0014-...md:11`，2026-09-15） | grep `follow_up` / `FollowUp` = **0**；`DeliveryTarget` 也是 **0**（ADR-0047 的协议格**并未**落地） | `docs/design-plans/2026-09-15-follow-up.md`；协议见 `docs/adr/0047` |
| **长程 todo + `ToolPlugin` 装配缝** | ADR-0024（`:7`）supersede 0021 决策 5；ADR-0021（`:7`）决策 ③ | grep `ToolPlugin` = **0**、`todo.json` = **0**、`should_inject` = **0**；`adapters/tools-basic/src/` 只有 7 个文件工具，无 `todo.rs` | `docs/arch/long-horizon-todo/design.md`、`docs/arch/hooks-and-plugins/design.md` |

**这两项相互咬合**（0024 的推动判据「有未完成项」依赖 0014 的 follow-up 机制），
宜按 **follow-up → todo** 的顺序推进。

**Follow-up 的根因（已查明，2026-10-22 核实）**：「排到下一回合」这一路**不是缺实现，是缺协议格**——
ADR-0013 删掉了旧 `Inbox` 的 `follow_up` 路（`docs/adr/0013-cancel-token-removal-boundary-source.md:37`），
而 `Boundary` 枚举今天只有 `Steer` / `Deferred` / `Compact` / `Abort`（`crates/ys-protocol/src/boundary.rs:12`）。
ADR-0013 发生在 coding-agent TUI 落地**之前** ⇒ 产品需要该语义时协议已被收敛掉。
详见 `docs/arch/input-delivery-latency/context.md`。

⚠️ **本段描述的是 2026-10-22 上午的 as-built，当日傍晚已部分过时**：
「回合中提交的输入被排到整个回合结束」「`Steer` 在生产侧无生产者」这两条**已不成立** ——
`8a08bd6` 接上了 `Steer` 生产者（回合中提交普通消息 ⇒ 投 `Boundary::Steer`），
并把回合头部排空从「无差别丢弃」改为「只丢 `Abort`、保留 `Steer`」。

**仍然成立的那一半**：**「下一回合」语义仍缺协议格与队列** ——
`Boundary` 仍无 `NextTurn` 一格，app 侧无跨回合队列 ⇒ 系统发起的 follow-up 无处可去。
`grep follow_up / FollowUp` 仍为 **0**。

> ⚠️ **ADR-0055（2026-10-25）改变了这里的结论边界**：子 agent 的结果现在**不再需要**
> 「再起一个回合」—— 父回合会在收场前等子 agent，把结果作为**同一回合的下一轮**的输入
> （`crates/ys-loop/src/basic.rs` 的 `wait_for_subagents`）。
>
> ⇒ 本条缺的「协议格 + 队列」仍是真的，但它的**第一个假想消费者已不存在**：
> ADR-0055 明确否决了「系统再起回合」这条路（四条硬代价，见该 ADR 的 Considered Options）。
> 将来若真要做 follow-up（如 `todo` 的推动），需重新论证「为什么不能在回合内等」，
> 而不能直接引用本段的「协议已备」。as-built 见 `docs/design-final/subagent-continuation.md`。

**根因的解法已定（2026-10-22，ADR-0047）**：
投递去向做成协议一等公民（`DeliveryTarget` / `Delivery`，`ys-protocol` 新文件），
队列住在 **app 侧**（`app_loop`）**跨回合存活** ⇒ 既是「绝不丢失」的保证，
也是 follow-up 缺的**协议格**（`Delivery{origin: System}`）。
⚠️ 但 **ADR-0047 自己标的是 `accepted (partial)`**，代码里 `DeliveryTarget` / `Delivery`
**零命中**（`adapters/tools-basic/src/background.rs` 的 `Delivery` 是同名私有枚举，与协议无关）。
⇒ 本条应读作「**协议仍缺，G1 / G3 / G5 / G6 未做**」，不是「协议已备」。

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

- ✅ **推理强度的「按调用类型分档」已落地**（2026-10-28，P1–P4）。
  完整探索 `docs/arch/effort-per-call-type/context.md`，设计 `design.md`，
  成本分析 `cost-analysis.md`，决策 `docs/adr/0060-thinking-policy-per-call-kind.md`。

  **已落地**：
  - `ProviderCompat::opencode_go()` 置 `supports_thinking_control`（第一优选模型
    `deepseek-*` 经该网关接入；置位前这条杠杆**不可达**）；
  - `ModelRequest.disable_thinking: Option<bool>` 泛化为 `thinking: Option<ThinkingOverride>`
    （`ys-core` 新类型，带 `effort` 覆盖）。**不建** `CallKind` + 策略表 ——
    三个槽位里两个恒 `None`，间接层不产生行为差异；
  - 主轮开关进 `RunLimits.main_disable_thinking`，**默认 `None`**（承重默认）；
  - 三处调用面：主轮随 `RunLimits`、摘要恒关、压缩保持开。

  **成本口径**（`python3 scripts/cost-audit.py`，零 API 调用）：
  缓存命中 35.7% / 未命中 29.8% / reasoning 18.3% / 工具参数 15.5% / 可见正文 1.8%。
  ⚠️ reasoning 占 output 的 70% 但 output 只占成本 33.8% —— **按金额排才看得出杠杆**。
  削减叙述只值 1.8%，**不值得**为它改提示词。

  **实测 A/B（DeepSeek）**：`daily/2` 质量 Q+（持平）、轮数 10→5、墙钟 43.3→27.2 s（−37%）；
  推理密集任务成本 −32.4%；**短检索任务 +1.1%（无收益）**；
  `fact/1` 3 次里 **1 次 `Q~`**（编造引用）⇒ **质量余量不为零**。

  ⬜ **剩余（明确不做，等新证据）**：主轮默认关思考。当前只由 `YUSHAN_MAIN_THINKING=off`
  显式开启 —— 默认值面向所有人，`Q~` 那条不解决就不动。要推进需
  「`fact/1` 连 5 次 Q+」或换一个能覆盖改代码类任务的判据。

  **结构状态**：**开关**这一维已走请求边信道解决；**强度**（`reasoning_effort`）
  仍冻在 model 实例里，主轮/压缩/工具摘要共用一个 `ctx.model`（`basic.rs:244/1011`、
  `compact.rs:365`、`summarize.rs:100`）。请求级强度通道已在 `ThinkingOverride.effort`
  里打通（含门控），但**没有任何调用面使用它**。

- **Termux / Android 下 subagent 派发会把整个会话拖死（Android 12 phantom process killer）**：
  排查记录见 `docs/reports/2026-10-27-termux-restart-under-subagent-report.md`。
  **代码侧四条放大因素**（均已核实）：每回合最多 16 次派发且两个派发工具都声明并发安全
  （`apps/coding-agent/src/subagent.rs:33`、`:1418`、`:1505`）；`LiveSubagents` **只有计数器没有准入闸**
  （`apps/coding-agent/src/subagent.rs:347`、`:362`，全仓 `Semaphore` **零命中**）；`bash` 输出
  `read_to_end` **无界累积**（`adapters/tools-basic/src/bash.rs:513`，截断在 `:209` 才做）；
  子 agent 的 `CollectingSink` / `MemorySession` / 转录字符串**全量留内存**
  （`crates/ys-event/src/collecting.rs:8`、`crates/ys-session/src/memory.rs:8`、`subagent.rs:1246`）。
  ⬜ **待办**：R2（`bash` 有界收集，照 `exec.rs:218` 的 `run_bounded` 语义）无契约变更，可直接做；
  R1（子 agent 并发闸）改 ADR-0022 决策 15 / ADR-0055 的语义边界 ⇒ **须先走 `arch-design`**。
  ⚠️ 主因归属**尚未确认**：本机读不到 cgroup / `dmesg`，需用户侧提供「Termux 是否打出 `signal 9`」
  与进程数峰值（报告 §5）。**确认前不要把 R1 当已定方案落地**。

- **TUI 鼠标**（[ADR-0050](adr/0050-tui-mouse-wheel-scroll.md) 滚轮 +
  [ADR-0051](adr/0051-tui-text-selection.md) 拖选）：
  - ⬜ **模态浮层里的滚轮**：浮层自己读事件（`crates/ys-tui-coding/src/run.rs` 的
    `Prompter` 实现），`select_modal` 的 `match` 末尾 `_ => {}` 掉鼠标（`run.rs:613`）
    ⇒ 会话列表 / 模型列表超过一屏时**无法滚动**，
    只能靠方向键逐项移动（跨屏时会自动跟高亮）。补法：在 `select_modal` 的匹配里加
    `Event::Mouse` 臂改 `top` / 高亮。依据：ADR 风险表的「模态浮层里的滚轮」一行标**未做**。

- **bg-notify 的 R1 技术债：投递机制不外溢**（ADR `docs/arch/bg-notify/adr-bg-notify.md`）：
  「落点格 + `Delivery` 三态 + take/claim/Finish」现在长在 `adapters/tools-basic/src/background.rs`
  上，只为后台 `bash` 一个生产用例服务。**触发重开的条件**：出现**第二个**后台任务类型
  （MCP 长任务、后台 LSP、后台 subagent 的另一种形态）⇒ 那时把这三块抽成
  `ys-protocol` 的 `DeferredSink`（候选 B 的 §2.2）。不抽的代价：那 30 行会被复制第二遍。
- **TUI 渲染有两份独立失效键 —— 已出过一次真 bug，建议合并**（2026-10-28）：
  `RenderCache`（`crates/ys-tui-coding/src/cache.rs`）按 `(槽, src.len(), width)` 缓存 md 产物、
  且对「增长中」的条目**有意节流**（100ms 内沿用旧产物）；`TranscriptLayout`
  （`crates/ys-tui-coding/src/layout.rs`）另按 `entry_sig`（assistant 只哈希 `s.len()`）决定要不要重渲染。
  两者叠加时「源已更新 + 产物滞后」会被第二份键误判成「已是最新」⇒ 该条**永久停在半句话**。
  已修（`cc26be3`：`lines_for` 返回 `stale`，滞后时写 `STALE_SIG` 哨兵），但**根因是两份键**。
  ⬜ **建议**：让 `TranscriptLayout` 完全依赖 `RenderCache` 的命中判定，删掉 `entry_sig`
  —— 否则两份键迟早再次漂移（下次可能不再是「能看出来」的形态）。
  诊断与排除过程见 `docs/reports/2026-10-28-tui-stream-tail-diagnosis.md` §6。
  改动面：`layout.rs` 的 memo 结构 + `cache.rs` 暴露一个「这一槽位是否最新」的查询，
  生产路径只有 `draw.rs` 一处调用 ⇒ 不涉及对外契约。

- **`kill_group` 仍是同步 fork**：已从锁内挪到锁外（ADR 同上），但仍是
  `Command::new("sh")...status()` 的同步等待。`proc_group::kill_group_async:40` 已存在，
  要彻底解决需把 `sweep` / `snapshot` 的签名改异步。

- **PTC（程序化工具调用）—— 长期方向，本轮明确不做**（ADR-0048 决策）：
  模型写一段代码调工具、**只有 curated 结果进模型历史** ⇒ N 次工具调用收敛为**一轮**模型往返。
  是四家同类（deepseek-harness `core/tools/src/ptc.ts:1-8`）里**唯一能改变轮次数量级**的机制，
  本仓零。**不做它的理由**：需执行边界，与本仓「不做安全（沙箱/隔离/权限/审批由上层解决）」
  的硬约束正面冲突，且约 2,500+ 行。**触发重开的条件**：上层项目给出执行边界，
  或轮数差距（同题对照 codex 6 轮 vs 我们 33 轮）成为主要瓶颈。
  设计依据 `docs/arch/session-latency/design.md` §4.3 决策点 1。

- **`docs/arch/session-latency/` 的 D2（subagent 工具表约束）**：与 **A3** 是同一件事的
  `session-latency` 表述（子 agent 拿到与父**完全相同的 9 个工具**，`apps/coding-agent/src/subagent.rs:980`
  每次派发重建），落地时合并处理；另需满足 D15「提示词不可见 + 执行时拒绝」双实现。

- **`toolset.rs` 的 `PATH` 改写会波及不持锁的用例**：`apps/coding-agent/src/toolset.rs:195`
  的 `set_path` 把进程 `PATH` 指向空目录（它自己持 `env_lock` + RAII 还原，纪律是对的），
  但**别的**用例若要 `spawn` 一个按 `PATH` 解析的二进制（`sh` 等）而**不持同一把锁**，
  就会在窗口期内拿到 ENOENT ⇒ 表现为「命令静默启动失败、留下孤儿进程」。
  本仓后台命令用例已整段持 `env_lock` 绕开，**隐患本身仍在**（`AGENTS.md`「测试隔离」条）。
- **`explore-speed` 的三条非目标**（`docs/arch/explore-speed/design.md:41`）：按 bash 命令契约判
  只读、拆第二个并发常量、动 `Tool::timeout()` 契约面 —— 本轮明确不做，需单独论证才重开。
- **ADR 编号断档**：`docs/adr/` 缺 `0005` / `0006`（`0040` 已存在：
  `docs/adr/0040-per-segment-tool-scheduling.md`；`0005` 实际在
  `docs/arch/coding-agent/adr-0005-infrastructure-approach.md`）。需确认 `0006` 是历史沿革还是遗漏。
- **`docs/arch/tui-turn-spacing/` 已实施但未归档**：代码在
  `crates/ys-tui-coding/src/draw.rs:61`（`USER_GAP_ROWS = 2`），文档停在 `context.md`。


