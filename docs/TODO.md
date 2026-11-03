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
> 基线：`master` @ `9d156e4`（核实于 2026-11-04；重核全量重跑过一次）。A / B / C / D 各段在此基线重核过，
> 指针与全仓 grep 均按此基线修正。

---

## A. 已设计、按阶段主动推迟（离得最近）

### A1. `--debug` 结构化日志 P1 / P2

P0 已落地；P1、P2 是**设计文档自己排的阶段**，标注见
`docs/arch/agent-observability/design.md` §5（P1 / P2）。

| 阶段 | 缺什么 | 状态依据 |
|---|---|---|
| P1 | **适配器自检**（检测旁路静默退化的缓解措施） | `design.md:101` 定的是「P1 上线时加一条启动期自检」；全仓 `自检` / `self_check` 在 app 与适配器侧**零命中** |
| P2 | `state_change` 记录 | 全仓源码 grep `state_change` **零命中** |

> P1 的 `model_attempt` / `context_compacted` / `context_compaction_failed` / `response_truncated`
> **已落地**（`apps/coding-agent/src/debug_log.rs:508`、`main.rs:713`、`main.rs:723`），不在本清单内。
>
> P2 的 jq 配方**已落地**：`design.md:282` 给出 `jq 'select(.type=="tool_result" and .duration_ms>10000)'`
> 与「按 `tool_call_id` 到 session 取内容」的完整配方，不再挂在本文档 §5 的 ⏳ 里
>
> P1 的「进程被 `kill -9` 不 flush 退出」**已不成立**：`debug_log.rs:922` 的 `write_line`
> 直接 `write_all` 到文件、不经过 `BufWriter`，进程被杀最多丢整行、不留半行
> （`docs/reports/2026-10-17-debug-log-report.md:22` 已把这一条记为 P0 验收事实）。

### A2. subagent 成本数字回填（N8 / 刀 7）—— 只剩文档，不含代码

`docs/design-plans/completed/2026-10-09-subagent-landing.md:72-77` 的 N8：需真实凭证跑一次派发，把读数写回
`docs/arch/subagent-rebaseline/design.md` §6.4。标 `MANUAL_ACK_REQUIRED`。

> **这条的产品价值已被别的手段满足**：§6.4 自己要的是「让成本从估算变读数」，而该文件的结论
> 已改为把 P0 指纹做成**字节锚而非 token 数**（`design.md:360`）—— 字节锚不需要成本数字。
> 剩下来只剩一件事：**把 §6.4 那段作废说明里缺失的真实数字填上**。低优先级，不该阻塞任何工作。
>
> **回填前必须先改一处过时设计**：`docs/arch/subagent-rebaseline/design.md` §6.1（`353` 起）写的是
> `prompt_cache_hit_tokens: u32`，as-built 是 `Option<u32>`（`crates/ys-core/src/usage.rs:16`）——
> 回填时若照 §6.1 抄字段类型会写成错的。
>
> 该计划的刀 0–6 **已落地**（`subagent_handle` 在 `apps/coding-agent/src/subagent.rs:1223`；
> 父表断言 `toolset.rs:624` 的 `prefix_fingerprint_is_anchored_and_stable` 存在）。

### A2b. ADR-0054 —— 只落了决策 ⑤（且 ⑥ 部分落地）

`docs/adr/0054-subagent-progress-board.md`（proposed）共 9 条决策，**只落了决策 ⑤**（句柄全局唯一
`sa{turn}.{n}`，2026-11-01 随 ADR-0065 一起落地）；决策 ⑥ 的「增量落盘」有一个**形态不同**的既存实现。
其余仍未做。

| 决策 | 缺什么 | 状态依据 |
|---|---|---|
| ① 进程级状态板 | `SubagentBoard`（与 `PendingResults` 并置） | 全仓源码 grep `SubagentBoard` **零命中** |
| ⑤ 句柄全局唯一 | 句柄改 `sa{turn}.{n}` + 寻址用的注册表 | ✅ **已落**：`subagent_handle`（`apps/coding-agent/src/subagent.rs:1223`）与 `SubagentHandles`（`:420`） |
| ② 四态状态机（完成 ≠ 送达） | 无状态机 | 无 `SubagentState` 类类型 |
| ③ `ProgressTap` 采集点 | 包装 `EventSink` 的采集层 | 全仓源码 grep `ProgressTap` **零命中** |
| ④ `AgentEvent::SubagentProgress` 通道 | 事件变体 | 全仓源码 grep `SubagentProgress` **零命中** |
| ⑥ 路径先分配 + 按轮 append | `SubagentsDir::allocate() -> Artifact` / `append_round` | ⚠️ **增量落盘已有，形态不同**：`JournalingSink`（`apps/coding-agent/src/subagent.rs:721`，`EventSink` 实现在 `:822`）把结构性事件逐行写 `<pid>_<seq>.live.jsonl`（路径 `:702`），收尾时 `finish_transcript`（`:688`）删掉 journal。**缺的是**：路径未在 `spawn` 前分配（`seq` 在 `execute` 内取，`:1521`）、无 `Artifact` / `append_round` 两个类型、路径未进状态板（板本身未做） |
| ⑦ 只读工具 `subagent_status` | 工具本体 | 全仓源码 grep `subagent_status` **零命中**（`toolset.rs:40` 的 `EXPECTED_ORDER` 里没有它）；当前观察子 agent 只能读子会话与转录文件 |
| ⑧ TUI 状态行 | 状态行 + 三种转换行 | 无对应 `TranscriptLine` 产出 |
| ⑨ 模型侧结构化头 | 头 + 两条 guideline | 无 |

> 可观察性的现实路径：**子 agent 自己的 session / 转录文件**（ADR-0054 背景里定的载体）。
> 缺的是「不读文件也能看到」的推送式状态，不是「完全不可观察」。

### A3. 子 agent 只读角色（多角色定义）

`docs/arch/subagent/design.md:827` 的 **P3（条件触发，本轮不做）** 表列出「多角色 agent 定义
（frontmatter / 配置，含只读角色）」，触发条件 = 出现**第二个**内置角色的真实需求（`:834`）。
设计已留口子：**子 agent 能力完全由创建时给它的工具表决定，不需要新机制**（`:647` D14 裁定；
ADR-0022（accepted）`:132-133` 同文）。

| 缺什么 | 状态依据（核实于 `9d156e4`） |
|---|---|
| 多角色定义（frontmatter / 配置） | 全仓源码 grep `AgentDefinition` / `agent_role` **零命中**（2026-11-04 重核）。**但「角色概念」本身已部分落地**：`SubagentMode { General, Explore }` 已是公开类型（`crates/ys-tool/src/subagent.rs:18`），`Explore` 由工具表过滤实现（`apps/coding-agent/src/subagent.rs:1409` `child_tools` 按名字去掉 `write` / `edit`）。缺的是**第三个角色**与**配置驱动**，不是「只读」这件事 |
| `build_tools` 无法按角色换表 | 签名零策略参数（`apps/coding-agent/src/toolset.rs:88`）；生产路径全仓**仅两处**构建工具表：装配期 `apps/coding-agent/src/assembly.rs:149`（子表）与派发期 `apps/coding-agent/src/subagent.rs` 的 `child_tools`（每次派发重建） |

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

**仍然成立的那一半**：`Boundary` 仍无「下一回合」一格（`crates/ys-protocol/src/boundary.rs:12` 起仍是
`Steer` / `Deferred` / `Compact` / `Abort`），app 侧也无**跨回合的输入队列** ⇒ 系统发起的
follow-up 无处可去。`grep follow_up / FollowUp` 仍为 **0**。

> ⚠️ **ADR-0055 的等待链已拆除（ADR-0065，2026-11-01）**：子 agent 的结果现在**不靠**「父回合在收场前等」
> 送达 —— 子结果走与 `bash_bg` 同构的通道（`PendingResults` 投递表 + `Wake::SubagentReturned`）
> ⇒ **结果回来时自动起一个新回合**交给模型（`apps/coding-agent/src/app_loop.rs:413-445`）。
>
> ❗ **但这不是「系统推一回合」那一格**（2026-11-04 重核，推翻了本段此前的写法）：
> `PendingResults`（`apps/coding-agent/src/subagent.rs:268`）是一个**进程级投递表 + `Notify` 唤醒**，
> 不是 `Boundary` 变体；`take_pending` 的唯一入口仍是 `Request::Prompt`，
> 自动起回合只是把「用户下次敲字」提前成了 harness 自己发起。
> ⇒ 本条缺的「协议格 + 队列」**两个都仍是真的**，且**没有**现成通路可沿用 ——
> 这使下一条「先论证为什么不能沿用自动回合」的答案变成一个具体的结构问题：
> 把「推进」放进 `Boundary`（跨回合有序队列）还是再造一张表（如 `PendingResults`）。
> as-built 见 `docs/design-final/subagent-async.md`。

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

> **外部参考**：`docs/arch/superagent-reference/design.md` §4 —— 本条**就是**那个参考的
> 核心结论：缺的不是「投递通道」而是「**系统推一回合**」那一格。原有的判断
> （宜照 ADR-0051 `Boundary::Deferred` 的形状做）只对「结果回到会话」成立，
> 对「工作还没完、再推一次」不成立 —— 后者需要一格与 `Deferred` 语义不同的东西：
> `Deferred` 是「产出物到达」，而这一格的语义是「推进」。
> 设计要点（约束而非方案）见该文 §4.3，不得绕过 `Abort` 那一条尤要注意。

**未落地但已在别处补上的**：ADR-0021 的前置条件「prefix 指纹测试」已由 subagent 刀 0 于
`apps/coding-agent/src/toolset.rs:624`（`prefix_fingerprint_is_anchored_and_stable`）一带补齐，**不再计入**。

## B补. 有意不做 —— 已有触发条件，不进本清单

以下三项曾被列成待办，但它们都已有「明确不做 + 何时重开」的决策，按本文档头部判据属
**设计决策**，不是待办。留此一行供检索，**不要**再当成待开工项：

| 项 | 决策 | 重开条件 |
|---|---|---|
| PTC（程序化工具调用） | ADR-0048 决策；`docs/arch/session-latency/design.md` §4.3 决策点 1 | 上层项目给出执行边界，或轮数差距成为主要瓶颈 |
| `explore-speed` 三条非目标 | `docs/arch/explore-speed/design.md:41` | 需单独论证才重开 |
| bg-notify R1（把 `Delivery` 三态抽成 `ys-protocol` 的 `DeferredSink`） | `docs/arch/bg-notify/adr-bg-notify.md` | 出现**第二个**后台任务类型（MCP 长任务 / 后台 LSP / 另一种 subagent 形态） |

---

## C. 卡在「待用户裁决」（arch-explore 已完成，方案设计未启动）

以下目录**只有 `context.md`**，末尾均带一份未答的「未澄清问题」：

| 目录 | 卡点 | 指针 |
|---|---|---|
| `docs/arch/input-history/` | ⚠️ **会话内部分已解**（2026-10-23）：**仅进程内、不落盘、不跨会话** —— 输入框空时 `↑` 取回、`↓` 还原草稿（`crates/ys-tui-coding/src/history.rs`）。<br>· ✅ Q4（键位）已裁决：输入框为空/浏览态归历史，否则归聊天滚动；<br>· ⬜ Q2 / Q3 / Q5–Q8 未做：跨会话持久化、按 cwd 分片、去重上限以外的规则、隐私过滤、落盘时机、归属层 | `context.md:139` |
| `docs/arch/bash-cwd-contract/` | Q7（短路路径是否补 cwd，会改「用户全权接管提示词」契约）、Q8（头部信息块收哪几项） | `context.md:747` |
| `docs/arch/llm-api-unavailable/` | ✅ **两大缺口已关，仅剩一项**（2026-11-04 重核）：<br>· ✅ **超时**已落 —— `TimeoutConfig { connect_secs: 15, idle_secs: 120 }` + 流空闲超时（`adapters/model-openai-compatible/src/lib.rs:90-104`），覆盖原「黑洞」场景；<br>· ✅ **流中断可区分** —— `ModelError::StreamInterrupted`（`crates/ys-model/src/error.rs:19`）已加，覆盖原「半截当完整」场景；<br>· ❌ **仍缺**：200 + 非 SSE 错误体（网关登录页 / 502 页）仍产出 `Ok(空)`，只是从「静默 Completed」变成了 `StopReason::NoOutput`（`crates/ys-loop/src/basic.rs:1101-1112` —— 可见但**判定仍不是故障**）；<br>· ⬜ 其余未决：错误文案形态、超时后会话状态、Ctrl-C 语义；`ModelError` 已有 `StreamInterrupted` 先例 ⇒「加变体会否破外部」这个阻断题已实际作答 | `context.md:161` |
| `docs/arch/per-turn-model-switch/` | ⚠️ **已落地大半**（2026-11-04 重核）：<br>· ✅ **跨 provider 切换**已落 —— `Request::SetModel` 有 `provider: Option<String>`（`crates/ys-protocol/src/request.rs:28-32`），`capabilities::set_model`（`apps/coding-agent/src/capabilities.rs:227` 起）有三条归属来源与优先级；<br>· ✅ **回合归因**已落 —— `record_model_change` 写 `model_changed` 记录（`apps/coding-agent/src/app_loop.rs:1193`、`:626-636`）；<br>· ⬜ **只剩声明式形态**（B1 内联指令 / B2 workflow / B3 会话持久化）未决 —— 属**新特性**，不是缺实现 | `context.md:179` |
| `docs/arch/tool-call-efficiency/` | ⚠️ **Q2 已由实现作答**（2026-11-04 重核）：`grep` 与 `read` 均已有 `paths` 多目标（`adapters/tools-basic/src/grep.rs:115`、`read.rs:30`，as-built 见 `docs/design-final/session-latency-phase1.md:115-116`）。<br>剩余未决：Q1（本轮范围）、Q3（布局快照）、Q4（验收口径）、Q5（措辞归属 —— 与 `prompt-delivery` 同一个未决题） | `context.md:199` |
| `docs/arch/etf-display-tool/` | 报告目录是否可配、ECharts CDN 降级；**`adapters/tools-finance` crate 不存在**（`Cargo.toml` 成员表无此项） | `context.md:99` |
| `docs/arch/prompt-delivery/notes.md` | 唯一未决项：**产品内置 vs 项目可配** | `notes.md:98`（文档头自标「状态：未决」） |
| `docs/arch/subagent-budget/` | 🟡 **部分已决**（2026-10-31）：**`General` 不设硬上限**、`General` 软提醒 50 已定；`Explore` 软提醒 10 待定（修「继承 60」但硬上限 16"）的死角。未澄清：低轮次竞速是否正常、不活工具与空心轮的区分依据 | `context.md` 末尾「未澄清问题」 |
| `docs/arch/input-delivery-latency/` | ⚠️ **部分已解**（2026-10-22）：<br>· ✅ **G2**（F2）已修 —— 「模型正要收尾时到达的插话静默消失」已修，见 `docs/adr/0049-final-answer-boundary-drain.md`；<br>· ✅ **G4**（F4）已解 —— 随 `8a08bd6` 落地；<br>· ⬜ **G1 / G3 / G5 / G6** 未做 —— app 侧 `Delivery` 队列、降级投递的触发机制、`origin` 是否外送、状态机载体（`docs/arch/input-delivery-latency/design.md:16-17` 自标）；<br>· ⬜ 设计 §7 仍有 **3 项「待用户确认」**（命令与消息谁先 / 回合中命令是否即时拒绝 / 投递状态显示落点） | `review.md` §7 闸门表、`docs/adr/0047-input-delivery-target-and-queue.md`、`docs/adr/0049-final-answer-boundary-drain.md` |

---

## D. 其他尾巴

- ✅ **子 agent 异步收敛已落地**（2026-11-01）。删除同步 `subagent` 工具、拆除父回合等待链、
  子结果自动起回合、新增 `subagent_send`、句柄改 `sa{turn}.{n}`。
  决策 `docs/adr/0065-subagent-send-and-auto-turn.md`，as-built `docs/design-final/subagent-async.md`，
  设计 `docs/design-plans/2026-11-01-subagent-async-convergence.md`。
  ⬜ **已知边界（不在本轮修）**：
  1. **消息最大延迟 = 当前工具跑完** —— `subagent_send` 的 `Steer` 只在子的轮边界被消费，
     子 agent 正卡在一次长工具调用里时消息要等那次调用返回（与 ADR-0064 同一口径）。
  2. **广播语义未做** —— `handle` 是单个字符串，一次只投一个子 agent，没有「发给所有在跑的」。
  3. **`PendingResults` 是进程级、会话无关** —— 别的会话遗留的结果会投给当前会话。
     外部参考（`docs/arch/superagent-reference/design.md` §2.2）：同构问题按
     「送达 / 读到 / 处理」三段可判事实拆开，未投递的结果**退回上游并永不过期**。
  4. **`-p` / `--json` 不自动起回合** —— 单回合模式结束后才完成的结果留表即失效
     （`apps/coding-agent/src/main.rs:2027` 的绑定点与 `:2029-2030` 的注释）。
     同上 §2.2：对价是「先落盘再投递」。
  5. **`LiveSubagents` 降为纯计数** —— 等待链拆除后无控制流消费者，`count()` 现为 `#[cfg(test)]`
     （`apps/coding-agent/src/subagent.rs:524-525`）。删不删属设计决策，未定（与下文 Termux R1 的「准入闸」同一块代码，宜一并裁）。

- ✅ **插话不再打断工具与同步 subagent 已落地**（2026-10-31）。
  决策 `docs/adr/0064-steer-does-not-interrupt.md`，as-built `docs/design-final/steer-does-not-interrupt.md`。
  工具与子 agent 只响应 `is_aborted()`；`Steer` / `Compact` 仍留在队列里，由 `BasicLoop` 在轮边界
  `take` 后注入（语义不变，只延后生效）。净减 289 行。
  **已接受的代价**：`/compact` 也要等长工具跑完（ADR-0052 决策 3 随之作废）。
  ⬜ **遗留**：`BoundarySource::has_pending_steer()` 现无生产消费者（公共 API，删属破坏性变更）；
  确认无用后可删 —— 需先核外部实现方。

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
  R1（子 agent 并发闸）改 ADR-0022 决策 15 的语义边界 ⇒ **须先走 `arch-design`**。
  外部参考（`docs/arch/superagent-reference/notes.md` §1「槽满 ≠ 失败」）：做闸时要区分**准入拒绝与运行失败**
  （槽满不得计进健康统计），且**人发起的操作不被同一道闸挡住**。
  ⚠️ 主因归属**尚未确认**：本机读不到 cgroup / `dmesg`，需用户侧提供「Termux 是否打出 `signal 9`」
  与进程数峰值（报告 §5）。**确认前不要把 R1 当已定方案落地**。

- **TUI 鼠标**（[ADR-0050](adr/0050-tui-mouse-wheel-scroll.md) 滚轮；拖选 / 右键见 [ADR-0066](adr/0066-restore-in-app-text-selection.md)）：
  - ⬜ **模态浮层里的滚轮**：浮层自己读事件（`crates/ys-tui-coding/src/run.rs` 的
    `Prompter` 实现），`select_modal` 的事件匹配末尾 `_ => {}` 掉鼠标（`run.rs:719`）
    ⇒ 会话列表 / 模型列表超过一屏时**无法滚动**，
    只能靠方向键逐项移动（跨屏时会自动跟高亮）。补法：在 `select_modal` 的匹配里加
    `Event::Mouse` 臂改 `top` / 高亮。依据：ADR-0050 风险表的「模态浮层里的滚轮」一行标**未做**。

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

- **PTC（程序化工具调用）** —— 长期方向，**明确不做**，见上文 B 补表。
- **`docs/arch/session-latency/` 的 D2（subagent 工具表约束）**：与 **A3** 是同一件事的
  `session-latency` 表述（子 agent 拿到与父**完全相同的 9 个工具**，`apps/coding-agent/src/subagent.rs:1409`
  的 `child_tools` 每次派发重建），落地时合并处理；另需满足 D15「提示词不可见 + 执行时拒绝」双实现。

- **`toolset.rs` 的 `PATH` 改写会波及不持锁的用例**：`apps/coding-agent/src/toolset.rs:195`
  的 `set_path` 把进程 `PATH` 指向空目录（它自己持 `env_lock` + RAII 还原，纪律是对的），
  但**别的**用例若要 `spawn` 一个按 `PATH` 解析的二进制（`sh` 等）而**不持同一把锁**，
  就会在窗口期内拿到 ENOENT ⇒ 表现为「命令静默启动失败、留下孤儿进程」。
  本仓后台命令用例已整段持 `env_lock` 绕开，**隐患本身仍在**（`AGENTS.md`「测试隔离」条）。

## E. 待裁决摘要

本段**不是待办**，是一张**需要用户表态的短清单**；裁决后各自转入上面 A / B / C 或 ADR。

| # | 题 | 已核实的现状 |
|---|---|---|
| 1 | **ADR-0054 的状态板还做不做？** | ADR 状态是 `proposed`（非 accepted），9 条决策只落 ⑤。期间 `JournalingSink` + debug 日志已回答了「事后能不能看出子 agent 干了什么」的大部分 —— 剩下的是**推送式**状态（②③④⑦⑧⑨，含新 `AgentEvent` 变体）。选项：做全 / 缩到「TUI 状态行一行」/ 标 superseded |
| 2 | **A3 多角色还做不做？** | 设计自标「P3 条件触发」，触发条件 = 出现第二个内置角色的真实需求 —— 至今未出现；`Explore` 已用工具表过滤实现只读。建议移出本清单，回归设计文档的条件项 |
| 3 | **Termux R1 并发闸？** | 主因归属仍未确认（需「是否 `signal 9`」与进程峰值）；且 `e13c1f0` 已把 worker 线程数封顶 8 ⇒ 进程数画像已变，旧结论需重算 |
| 4 | **TUI 两份失效键合不合并？** | `entry_sig`（`crates/ys-tui-coding/src/layout.rs:658`）仍在，`STALE_SIG` 只是止痛。改动面在渲染层，无对外契约 |
| 5 | **TUI 模态浮层滚轮？** | `select_modal` 仍未处理 `Event::Mouse`（`crates/ys-tui-coding/src/run.rs:719` 的 `_ => {}`）；会话/模型列表超一屏时只能方向键逐项走 |

---
