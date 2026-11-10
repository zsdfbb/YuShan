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
> 基线：`master` @ `296dcc8`（核实于 2026-11-05）。A / B / C / D 各段在此基线重核过，
> 指针与全仓 grep 均按此基线修正。

---

## A. 已设计、按阶段主动推迟（离得最近）

### A1. `--debug` 结构化日志 P1 / P2 的收尾项

P0 已落地；P1、P2 是**设计文档自己排的阶段**，`docs/arch/agent-observability/design.md:299-300`
的阶段表自标 ⏳。

| 阶段 | 缺什么 | 状态依据 |
|---|---|---|
| P1 | **适配器自检**（检测旁路静默退化的缓解措施） | `design.md:101` 定的是「P1 上线时加一条启动期自检」；全仓 `自检` / `self_check` 只命中测试注释，app 与适配器侧**零生产命中** |
| P2 | `state_change` 记录 | 全仓源码 grep `state_change` **零命中** |

> P1 的 `model_attempt` 已落（`apps/coding-agent/src/debug_log.rs:563`），
> `context_compacted` / `context_compaction_failed` / `response_truncated` 已落
> （`apps/coding-agent/src/main.rs:826`、`:830`、`:834`）；P2 的启动期 sweep 已落
> （默认目录 7 天清理、Override 不清理，`design.md:300`）。三者都不在本清单内。
>
> P2 的 jq 配方**已落进设计**（`design.md:282`），不再算待办。

### A2. subagent 成本数字回填（N8 / 刀 7）—— 只剩文档，不含代码

`docs/design-plans/completed/2026-10-09-subagent-landing.md:74-77` 的 N8：需真实凭证跑一次派发，把读数写回
`docs/arch/subagent-rebaseline/design.md` §6.4（`:354` 起）。标 `MANUAL_ACK_REQUIRED`。

> **这条的产品价值已被别的手段满足**：§6.4 自己要的是「让成本从估算变读数」，而该文件已把 P0 指纹
> 做成**字节锚而非 token 数**（`design.md:360`）—— 字节锚不需要成本数字。剩下来只剩一件事：
> **把 §6.4 那段作废说明里缺失的真实数字填上**。低优先级，不该阻塞任何工作。
>
> **回填前必须先改一处过时设计**：`design.md:333` 写的是
> `prompt_cache_hit_tokens: u32`，as-built 是 `Option<u32>`（`crates/ys-core/src/usage.rs:16`）——
> 照现在的 §6.1 抄字段类型会写成错的。
>
> 该计划的刀 0–6 **已落地**（`subagent_handle` 在 `apps/coding-agent/src/subagent.rs:1223`；
> 父表锚断言 `prefix_fingerprint_is_anchored_and_stable` 在 `apps/coding-agent/src/toolset.rs:635`）。

### A2b. ADR-0054 —— 只落了决策 ⑤（且 ⑥ 部分落地）

`docs/adr/0054-subagent-progress-board.md`（proposed）共 9 条决策，**只落了决策 ⑤**；决策 ⑥ 的
「增量落盘」有一个**形态不同**的既存实现。其余仍未做。

| 决策 | 缺什么 | 状态依据 |
|---|---|---|
| ① 进程级状态板 | `SubagentBoard`（与 `PendingResults` 并置，后者在 `apps/coding-agent/src/subagent.rs:268`） | 全仓源码 grep `SubagentBoard` **零命中** |
| ② 四态状态机（完成 ≠ 送达） | 无状态机 | 无 `SubagentState` 类类型 |
| ③ `ProgressTap` 采集点 | 包装 `EventSink` 的采集层 | 全仓源码 grep `ProgressTap` **零命中** |
| ④ `AgentEvent::SubagentProgress` 通道 | 事件变体 | 全仓源码 grep `SubagentProgress` **零命中** |
| ⑤ 句柄全局唯一 | — | ✅ **已落**：`subagent_handle`（`apps/coding-agent/src/subagent.rs:1223`）与 `SubagentHandles`（`:420`） |
| ⑥ 路径先分配 + 按轮 append | `SubagentsDir::allocate() -> Artifact` / `append_round` | ⚠️ **增量落盘已有，形态不同**：`JournalingSink`（`apps/coding-agent/src/subagent.rs:721`，`EventSink` 实现在 `:822`）把结构性事件逐行写 `<pid>_<seq>.live.jsonl`（路径 `:702`），收尾时 `finish_transcript`（`:688`）删掉 journal。**缺的是**：路径未在 `spawn` 前分配（`seq` 在 `execute` 内取，`:1487`、`:1521`）、无 `Artifact` / `append_round` 两个类型、路径未进状态板（板本身未做） |
| ⑦ 只读工具 `subagent_status` | 工具本体 | 全仓源码 grep `subagent_status` **零命中**（`apps/coding-agent/src/toolset.rs:42` 的 `EXPECTED_ORDER` 里没有它）；当前观察子 agent 只能读子会话与转录文件 |
| ⑧ TUI 状态行 | 状态行 + 三种转换行 | 无对应 `TranscriptLine` 产出 |
| ⑨ 模型侧结构化头 | 头 + 两条 guideline | 无 |

> 可观察性的现实路径：**子 agent 自己的 session / 转录文件**（ADR-0054 背景里定的载体）。
> 缺的是「不读文件也能看到」的推送式状态，不是「完全不可观察」。

### A3. 子 agent 只读角色（多角色定义）

`docs/arch/subagent/design.md:827` 的 **P3（条件触发，本轮不做）** 表列出「多角色 agent 定义
（frontmatter / 配置，含只读角色）」，触发条件 = 出现**第二个**内置角色的真实需求（`:834`）。
设计已留口子：**子 agent 能力完全由创建时给它的工具表决定，不需要新机制**（`:647` D14 裁定；
ADR-0022（accepted）`:132-133` 同文）。

| 缺什么 | 状态依据（核实于 `296dcc8`） |
|---|---|
| 多角色定义（frontmatter / 配置） | 全仓源码 grep `AgentDefinition` / `agent_role` **零命中**。**但「角色概念」本身已部分落地**：`SubagentMode { General, Explore }` 已是公开类型（`crates/ys-tool/src/subagent.rs:16`），`Explore` 由工具表过滤实现（`apps/coding-agent/src/subagent.rs:1409-1413` 的 `child_tools` 按名字去掉 `write` / `edit`）。缺的是**第三个角色**与**配置驱动**，不是「只读」这件事 |
| `build_tools` 无法按角色换表 | 签名零策略参数（`apps/coding-agent/src/toolset.rs:92`）；生产路径全仓**仅两处**构建工具表：装配期 `apps/coding-agent/src/assembly.rs:149`（子表）与派发期 `apps/coding-agent/src/subagent.rs:1410` 的 `child_tools`（每次派发重建） |

> ⚠️ **落地约束**：不可把角色做成 `subagent` 工具入参 —— 那会改 `ToolSpec`，动
> `apps/coding-agent/src/toolset.rs:635` 的**前缀指纹锚**，且破掉「子表 JSON 是父表字节前缀」
> 这条缓存优化（`apps/coding-agent/src/toolset.rs:648-661` 的断言即为此）。
> 宜做成**装配期多张预建表**（指纹只增不改）。
>
> **边界**：同表 `:835`「按任务类型动态裁剪子工具表」是**另一项**（R > 5 才回本，且破前缀），
> 不随本项一起做。见 §D「`session-latency` 的 D2」。
>
> 是否继续挂在本清单，见 §E 第 2 条。

### A4. 「按调用类型分档强度」（`reasoning_effort`）—— 通道已通，无调用面

开关那一维（主轮思考 on/off）已随 ADR-0060 落地；**强度**这一维只做了一半。

| 缺什么 | 状态依据 |
|---|---|
| 有调用面去设置 `ThinkingOverride.effort` | 请求级强度通道已打通：`ThinkingOverride` 带 `effort` 字段（`crates/ys-core/src/effort.rs:75`），适配层已在消费它（`adapters/model-openai-compatible/src/lib.rs:1632` 把 `request.thinking.effort` 翻成 `reasoning_effort`）。但 `crates/ys-loop/` 全仓**没有一处**给 `effort` 赋值：主轮（`basic.rs:822`）、压缩（`compact.rs:366`）、摘要（`summarize.rs:95` 恒 `ThinkingOverride::off()`）都只用开关，强度仍由 model 实例上的 `reasoning_effort` 决定（`adapters/model-openai-compatible/src/lib.rs:50`） |

> 触发条件（写成 ADR 的是开关那一维，这一维没写）：出现「同一会话里主轮要低档、压缩/摘要要高档」的真实需求。
> 没有该需求前，多接一层只会让三处调用面各自分叉。

## B. ADR 已 accepted，但代码零落地

| 项 | ADR（accepted） | 全仓源码证据 | 计划 |
|---|---|---|---|
| **Follow-up（追问）** | ADR-0014（`0014-...md:11`，2026-09-15） | grep `follow_up` / `FollowUp` = **0**；`DeliveryTarget` 也是 **0**（ADR-0047 的协议格**并未**落地） | `docs/design-plans/2026-09-15-follow-up.md`；协议见 `docs/adr/0047` |

**Follow-up 的根因（已查明）**：「排到下一回合」这一路**不是缺实现，是缺协议格**——
ADR-0013 删掉了旧 `Inbox` 的 `follow_up` 路（`docs/adr/0013-cancel-token-removal-boundary-source.md:37`），
而 `Boundary` 枚举今天只有 `Steer` / `Deferred` / `Compact` / `Abort`（`crates/ys-protocol/src/boundary.rs:12`）。
ADR-0013 发生在 coding-agent TUI 落地**之前** ⇒ 产品需要该语义时协议已被收敛掉。
详见 `docs/arch/input-delivery-latency/context.md`。

**仍然成立的那一半**（回合中插话已不属此列）：`8a08bd6` 接上了 `Steer` 生产者，
`drain_boundary(ctx)` 让「轮边界总是存在」成为真不变量（ADR-0049）。但 `Boundary` 仍无「下一回合」一格
（`crates/ys-protocol/src/boundary.rs:12` 仍是四格），app 侧也无**跨回合的输入队列** ⇒
系统发起的 follow-up 无处可去。

> ⚠️ **ADR-0055 的等待链已拆（ADR-0065，2026-11-01）**：子 agent 结果现在走与 `bash_bg` 同构的通道
> （`PendingResults`（`apps/coding-agent/src/subagent.rs:268`）投递表 + `Wake::SubagentReturned`）
> ⇒ **结果回来时自动起一个新回合**交给模型（`apps/coding-agent/src/app_loop.rs:413`）。
>
> ❗ **但这不是「系统推一回合」那一格**：`PendingResults` 是**进程级投递表 + `Notify` 唤醒**，
> 不是 `Boundary` 变体；`take_pending` 的唯一入口仍是 `Request::Prompt`，
> 自动起回合只是把「用户下次敲字」提前成了 harness 自己发起。
> ⇒ 本条缺的「协议格 + 队列」**两个都仍是真的**，且**没有**现成通路可沿用 ——
> 这使下一条的答案变成一个具体的结构问题：把「推进」放进 `Boundary`（跨回合有序队列）
> 还是再造一张表（如 `PendingResults`）。as-built 见 `docs/design-final/subagent-async.md`。

**根因的解法已定（ADR-0047）**：投递去向做成协议一等公民（`DeliveryTarget` / `Delivery`，
`ys-protocol` 新文件），队列住在 **app 侧**（`app_loop`）**跨回合存活** ⇒ 既是「绝不丢失」的保证，
也是 follow-up 缺的**协议格**（`Delivery{origin: System}`）。
⚠️ 但 **ADR-0047 自己标的是 `accepted (partial)`**，代码里 `DeliveryTarget` / `Delivery`
**零命中**（`adapters/tools-basic/src/background.rs` 的 `Delivery` 是同名私有枚举，与协议无关）。
⇒ 本条应读作「**协议仍缺，G1 / G3 / G5 / G6 未做**」，不是「协议已备」。

设计与变更清单：`docs/arch/input-delivery-latency/design.md` §6。

> **外部参考**：`docs/arch/superagent-reference/design.md` §4 —— 本条**就是**那个参考的
> 核心结论：缺的不是「投递通道」而是「**系统推一回合**」那一格。原有的判断
> （宜照 ADR-0051 `Boundary::Deferred` 的形状做）只对「结果回到会话」成立，
> 对「工作还没完、再推一次」不成立 —— 后者需要一格与 `Deferred` 语义不同的东西：
> `Deferred` 是「产出物到达」，而这一格的语义是「推进」。
> 设计要点（约束而非方案）见该文 §4.3，不得绕过 `Abort` 那一条尤要注意。

## B补. 有意不做 —— 已有触发条件，不进本清单

以下各项曾被列成待办，但它们都已有「明确不做 + 何时重开」的决策，按本文档头部判据属
**设计决策**，不是待办。留此一行供检索，**不要**再当成待开工项：

| 项 | 决策 | 重开条件 |
|---|---|---|
| PTC（程序化工具调用） | ADR-0048 决策；`docs/arch/session-latency/design.md` §4.3 决策点 1 | 上层项目给出执行边界，或轮数差距成为主要瓶颈 |
| `explore-speed` 三条非目标 | `docs/arch/explore-speed/design.md:41` | 需单独论证才重开 |
| bg-notify R1（把 `Delivery` 三态抽成 `ys-protocol` 的 `DeferredSink`） | `docs/arch/bg-notify/adr-bg-notify.md` | 出现**第二个**后台任务类型（MCP 长任务 / 后台 LSP / 另一种 subagent 形态） |
| 主轮**默认**关思考 | ADR-0060；`crates/ys-component/src/limits.rs:116` 的字段注释 | `fact/1` 连 5 次 `Q+`（现为 4 次里 1 次 `Q~`，编造引用），或换一个能覆盖改代码类任务的判据 |

## C. 卡在「待用户裁决」（arch-explore 已完成，方案设计未启动）

以下目录**只有 `context.md`**，末尾均带一份未答的「未澄清问题」：

| 目录 | 卡点 | 指针 |
|---|---|---|
| `docs/arch/input-history/` | ⚠️ **会话内部分已解**：**仅进程内、不落盘、不跨会话** —— 输入框空时 `↑` 取回、`↓` 还原草稿（`crates/ys-tui-coding/src/history.rs:27` 的 `InputHistory`）。<br>· ✅ Q4（键位）已裁决：输入框为空/浏览态归历史，否则归聊天滚动；<br>· ⬜ Q2 / Q3 / Q5–Q8 未做：跨会话持久化、按 cwd 分片、去重上限以外的规则、隐私过滤、落盘时机、归属层 | `context.md:139` |
| `docs/arch/bash-cwd-contract/` | Q7（短路路径是否补 cwd，会改「用户全权接管提示词」契约）、Q8（头部信息块收哪几项） | `context.md:747` |
| `docs/arch/llm-api-unavailable/` | ✅ **两大缺口已关，仅剩一项**：<br>· ✅ **超时**已落 —— `TimeoutConfig { connect_secs: 15, idle_secs: 120 }`（`adapters/model-openai-compatible/src/lib.rs:92-104`）+ 流空闲超时（`:767`），覆盖原「黑洞」场景；<br>· ✅ **流中断可区分** —— `ModelError::StreamInterrupted`（`crates/ys-model/src/error.rs:20`），覆盖原「半截当完整」场景；<br>· ❌ **仍缺**：200 + 非 SSE 错误体（网关登录页 / 502 页）仍产出 `Ok(空)`，只是从「静默 Completed」变成了 `StopReason::NoOutput`（`crates/ys-loop/src/basic.rs:1091-1099`、`:1153` —— 可见但**判定仍不是故障**）；<br>· ⬜ 其余未决：错误文案形态、超时后会话状态、Ctrl-C 语义 | `context.md:161` |
| `docs/arch/per-turn-model-switch/` | ⚠️ **已落地大半**：<br>· ✅ **跨 provider 切换**已落 —— `Request::SetModel` 有 `provider: Option<String>`（`crates/ys-protocol/src/request.rs:31`），`capabilities::set_model`（`apps/coding-agent/src/capabilities.rs:227`）有三条归属来源与优先级；<br>· ✅ **回合归因**已落 —— `record_model_change`（`apps/coding-agent/src/app_loop.rs:1204`）写 `model_changed`（`:1240`），三处调用点含 `Request::SetModel` 与 `SetEffort`（`:624`、`:634`）；<br>· ⬜ **只剩声明式形态**（B1 内联指令 / B2 workflow / B3 会话持久化）未决 —— 属**新特性**，不是缺实现 | `context.md:223` |
| `docs/arch/tool-call-efficiency/` | ⚠️ **Q2 已由实现作答**：`grep` 与 `read` 均已有 `paths` 多目标（`adapters/tools-basic/src/grep.rs:115`、`read.rs:30`，as-built 见 `docs/design-final/session-latency-phase1.md:115-116`）。<br>剩余未决：Q1（本轮范围）、Q3（布局快照）、Q4（验收口径）、Q5（措辞归属 —— 与 `prompt-delivery` 同一个未决题） | `context.md:199` |
| `docs/arch/etf-display-tool/` | 报告目录是否可配、ECharts CDN 降级；**`adapters/tools-finance` crate 不存在**（`Cargo.toml` 成员表无此项） | `context.md:99` |
| `docs/arch/prompt-delivery/notes.md` | 唯一未决项：**产品内置 vs 项目可配** | `notes.md:98`（文档头自标「状态：未决」） |
| `docs/arch/subagent-budget/` | 🟡 **部分已决**（2026-10-31）：**`General` 不设硬上限**、`General` 软提醒 50 已定；`Explore` 软提醒 10 待定（修「继承 60」但硬上限 16（`apps/coding-agent/src/subagent.rs:37`、`:45`）的死角）。未澄清：低轮次竞速是否正常、不活工具与空心轮的区分依据 | `context.md:289` |
| `docs/arch/input-delivery-latency/` | ⚠️ **部分已解**：<br>· ✅ **G2**（F2）已修，见 `docs/adr/0049-final-answer-boundary-drain.md`；<br>· ✅ **G4**（F4）已解 —— 随 `8a08bd6` 落地；<br>· ⬜ **G1 / G3 / G5 / G6** 未做 —— app 侧 `Delivery` 队列、降级投递的触发机制、`origin` 是否外送、状态机载体（`docs/arch/input-delivery-latency/design.md:16-17` 自标）；<br>· ⬜ 设计 §7 仍有 **3 项「待用户确认」**（命令与消息谁先 / 回合中命令是否即时拒绝 / 投递状态显示落点） | `review.md` §7 闸门表、`docs/adr/0047-input-delivery-target-and-queue.md` |
| `docs/arch/headless-worker-api/` | ✅ **已实现**（2026-11-10，as-built 见 `docs/design-final/headless-worker-contract.md`）。消费方：`../YuShan-lark-agent-bridge`（飞书 ↔ 本地 agent 桥，已有 claude / codex 两套适配器）。三件已落地：<br>· ✅ **会话 id 上报** —— `--json` 首行 `{"type":"session","id":…}`（`main.rs:2646`）；**golden 未改，仍 4 行** —— 正确修法是扩展过滤器 `without_round_usage`（`debug_log.rs:591`），改 golden 等于伪造基线证据；<br>· ✅ **`SIGTERM` 收尾** —— tokio 加 `signal` feature，接进既有 `select!`（`main.rs:2211`），只投 `Abort`、不新增退出路径，走唯一退出落点 `exit_with_run_end`；<br>· ✅ **退出码分三档** —— `0` 正常 / `1` 故障 / **`75`** 可重试（`single_turn_exit_code`，`main.rs:2804`），`README.md:267` 已同步。<br>· ⚠️ 已明确不做：指定会话 id、饿汉落盘、退出码 `2`（让给 `goal-command`）、stdin 提示词、同 id 并发锁。<br>· ⏳ 待复访（不阻塞）：**无 5s 强制计时器**（有意 —— 强计时器会造出新退出路径；bridge 侧 `stopGraceMs` 默认 5000ms 后 `SIGKILL` 兜底）、信号监听器注册失败退化默认处置、`sigterm_aborts_turn_and_exits_75` 的 2s 余量 | `docs/design-final/headless-worker-contract.md`、`design.md`、`adr-001` |
| `docs/arch/screenshot-tool/` | ⚠️ **需求已澄清、实测已完成，候选已比完，只缺用户拍板**：<br>· 两条需求不可合并 —— **页面渲染截图**（G2，`goals.md:18`；两平台同一条命令，实测应为 **S** 而非 goals 里标的 M）与 **屏幕捕获**（三套工具 × 三套权限模型）；<br>· ❗ **决定性事实**：`CGPreflightScreenCaptureAccess() == false` 时 `screencapture` 仍 **exit 0 且写出假图**，写不进去时也 exit 0 且无图 ⇒ **必须自验后置条件**；<br>· `xcap`（唯一一份代码覆盖三平台）实测两个扣分项：**不是 CLI**（无 `[[bin]]`）、**全屏截图不做权限判定**（`impl_monitor.rs:205`，静默返回黑图），且 Linux 依赖锁在 **git `branch = \"main\"`、仓库无 `Cargo.lock`**；<br>· ⬜ 未决：是否新增 `apps/yushan-shot/`、依赖取舍（纯 FFI vs `xcap`）、是否限定桌面环境、skill 合一个还是拆两个 | `context.md:123`（§5 优先级依据）、`:172`（§7 未澄清问题） |

---

## D. 其他尾巴

- **子 agent 异步的已知边界（ADR-0065 之后留下的 5 条）**，均未定是否本轮修：
  1. **消息最大延迟 = 当前工具跑完** —— `subagent_send` 的 `Steer` 只在子的轮边界被消费，
     子 agent 正卡在一次长工具调用里时消息要等那次调用返回（与 ADR-0064 同一口径）。
  2. **广播语义未做** —— `handle` 是单个字符串，一次只投一个子 agent，没有「发给所有在跑的」。
  3. **`PendingResults` 是进程级、会话无关** —— 别的会话遗留的结果会投给当前会话。
     外部参考（`docs/arch/superagent-reference/design.md` §2.2）：同构问题按
     「送达 / 读到 / 处理」三段可判事实拆开，未投递的结果**退回上游并永不过期**。
  4. **`-p` / `--json` 不自动起回合** —— 单回合模式里结果绑在本回合的边界源上
     （`apps/coding-agent/src/main.rs:2090` 的 `bind_results`），回合结束后才完成的结果留表即失效。
     同上 §2.2：对价是「先落盘再投递」。
  5. **`LiveSubagents` 降为纯计数** —— 等待链拆除后无控制流消费者，`count()` 现为 `#[cfg(test)]`
     （`apps/coding-agent/src/subagent.rs:508`、`:523-528`）。删不删属设计决策，未定
     （与下条 Termux R1 的「准入闸」同一块代码，宜一并裁）。

- **`BoundarySource::has_pending_steer()` 无生产消费者**：唯一消费者是
  `adapters/tools-basic/src/bash.rs:1650` 起的**测试**（`is_aborted()` 的变异对照）；
  `crates/ys-tool/src/context.rs:12` 的注释已自标「可选能力，生产代码无消费者」。
  公共 API，删属破坏性变更 —— 需先核外部实现方。

- **Termux / Android 下 subagent 派发会把整个会话拖死（Android 12 phantom process killer）**：
  排查记录见 `docs/reports/2026-10-27-termux-restart-under-subagent-report.md`。
  **代码侧四条放大因素**（均已核实）：每回合最多 16 次派发（`apps/coding-agent/src/subagent.rs:37`）
  且两个派发工具都声明并发安全（`:2027`、`:2093`）；`LiveSubagents` **只有计数器没有准入闸**
  （`apps/coding-agent/src/subagent.rs:508`、`:512`，全仓 `Semaphore` **零命中**）；`bash` 输出
  `read_to_end` **无界累积**（`adapters/tools-basic/src/bash.rs:507`、`:512`，截断在 `:207` 才做）；
  子 agent 的 `CollectingSink` / `MemorySession` / 转录字符串**全量留内存**
  （`crates/ys-event/src/collecting.rs:8`、`crates/ys-session/src/memory.rs:11`、`subagent.rs:1521`）。
  ⬜ **待办**：R2（`bash` 有界收集，照 `adapters/tools-basic/src/exec.rs` 的 `run_bounded` 语义）
  无契约变更，可直接做；R1（子 agent 并发闸）改 ADR-0022 决策 15 的语义边界 ⇒
  **须先走 `arch-design`**。
  外部参考（`docs/arch/superagent-reference/notes.md` §1「槽满 ≠ 失败」）：做闸时要区分**准入拒绝与运行失败**
  （槽满不得计进健康统计），且**人发起的操作不被同一道闸挡住**。
  ⚠️ 主因归属**尚未确认**：本机读不到 cgroup / `dmesg`，需用户侧提供「Termux 是否打出 `signal 9`」
  与进程数峰值（报告 §5）。**确认前不要把 R1 当已定方案落地**。

- **TUI 鼠标**（[ADR-0050](adr/0050-tui-mouse-wheel-scroll.md) 滚轮；拖选 / 右键已由
  [ADR-0066](adr/0066-restore-in-app-text-selection.md) 恢复，选区实现在 `crates/ys-tui-coding/src/selection.rs`）：
  - ⬜ **模态浮层里的滚轮**：浮层自己读事件（`crates/ys-tui-coding/src/run.rs:796` 的
    `select_modal`），键位匹配后**没有**滚动分支、末尾 `_ => {}`（`:865`）掉鼠标
    ⇒ 会话列表 / 模型列表超过一屏时**无法滚动**，只能靠方向键逐项移动（跨屏时会自动跟高亮）。
    补法：在 `select_modal` 的匹配里加 `Event::Mouse` 臂改 `top` / 高亮。
    依据：`crates/ys-tui-coding/src/events.rs:130` 的模块头自标「模态浮层不吃滚轮（见 `docs/TODO.md` §C）」，
    ADR-0050 风险表同一行标**未做**。

- **TUI 渲染有两份独立失效键 —— 已出过一次真 bug，建议合并**：
  `RenderCache`（`crates/ys-tui-coding/src/cache.rs`）按 `(槽, src.len(), width)` 缓存 md 产物、
  且对「增长中」的条目**有意节流**（100ms 内沿用旧产物）；`TranscriptLayout`
  （`crates/ys-tui-coding/src/layout.rs`）另按 `entry_sig`（`:658`，assistant 只哈希 `s.len()`）
  决定要不要重渲染。两者叠加时「源已更新 + 产物滞后」会被第二份键误判成「已是最新」
  ⇒ 该条**永久停在半句话**。
  已修（`cc26be3`：`lines_for` 返回 `stale`，滞后时写 `STALE_SIG` 哨兵，`layout.rs:715`），
  但**根因是两份键**。⬜ **建议**：让 `TranscriptLayout` 完全依赖 `RenderCache` 的命中判定，
  删掉 `entry_sig` —— 否则两份键迟早再次漂移（下次可能不再是「能看出来」的形态）。
  诊断与排除过程见 `docs/reports/2026-10-28-tui-stream-tail-diagnosis.md` §6。
  改动面：`layout.rs` 的 memo 结构 + `cache.rs` 暴露一个「这一槽位是否最新」的查询，
  生产路径只有 `draw.rs` 一处调用 ⇒ 不涉及对外契约。

- **`kill_group` 仍是同步 fork**：已从锁内挪到锁外（段 2 无锁，`adapters/tools-basic/src/background.rs:765`），
  但 `snapshot`（`:726`）与 `sweep`（`:771`）仍调**同步** `kill_group`
  （`Command::new("sh")...status()`；`:218`、`:1191` 两处收尸同）。`proc_group::kill_group_async`
  （`adapters/tools-basic/src/proc_group.rs:40`）已存在，`bash` 已用它（`bash.rs:381`），
  要彻底解决需把 `sweep` / `snapshot` 的签名改异步。

- **`docs/arch/session-latency/` 的 D2（subagent 工具表约束）**：与 **A3** 是同一件事的
  `session-latency` 表述（子 agent 拿到与父**完全相同的**基础工具表，
  `apps/coding-agent/src/subagent.rs:1409-1413` 的 `child_tools` 每次派发重建），落地时合并处理；
  另需满足 D15「提示词不可见 + 执行时拒绝」双实现。
  ⚠️ 同文件 `:200` 把 ADR-0017 决策 5（摘要调用复用活前缀 + `tool_choice: none`）记为「未落地」，
  而 as-built 是**有意绕开**（`crates/ys-loop/src/compact.rs:328-329`：不复用 `tools` ⇒ 不需要
  `tool_choice`，`ModelRequest` 无此字段）—— 这属设计偏离，该进 ADR，**不是本清单的待办**。

- **`toolset.rs` 的 `PATH` 改写会波及不持锁的用例**：`apps/coding-agent/src/toolset.rs:205`
  的 `set_path` 把进程 `PATH` 指向空目录（它自己持 `env_lock` + RAII 还原，纪律是对的），
  但**别的**用例若要 `spawn` 一个按 `PATH` 解析的二进制（`sh` 等）而**不持同一把锁**，
  就会在窗口期内拿到 ENOENT ⇒ 表现为「命令静默启动失败、留下孤儿进程」。
  本仓后台命令用例已整段持 `env_lock` 绕开，**隐患本身仍在**（`AGENTS.md`「测试隔离」条）。

## E. 待裁决摘要

本段**不是待办**，是一张**需要用户表态的短清单**；裁决后各自转入上面 A / B / C 或 ADR。

| # | 题 | 已核实的现状 |
|---|---|---|
| 1 | **ADR-0054 的状态板还做不做？** | ADR 状态是 `proposed`（非 accepted），9 条决策只落 ⑤（§A2b）。期间 `JournalingSink` + debug 日志已回答了「事后能不能看出子 agent 干了什么」的大部分 —— 剩下的是**推送式**状态（②③④⑦⑧⑨，含新 `AgentEvent` 变体）。选项：做全 / 缩到「TUI 状态行一行」/ 标 superseded |
| 2 | **A3 多角色还做不做？** | 设计自标「P3 条件触发」，触发条件 = 出现第二个内置角色的真实需求 —— 至今未出现；`Explore` 已用工具表过滤实现只读（`subagent.rs:1409-1413`）。建议移出本清单，回归设计文档的条件项 |
| 3 | **Termux R1 并发闸？** | 主因归属仍未确认（需「是否 `signal 9`」与进程峰值）；且 `e13c1f0` 已把 worker 线程数封顶 8 ⇒ 进程数画像已变，旧结论需重算 |
| 4 | **TUI 两份失效键合不合并？** | `entry_sig`（`crates/ys-tui-coding/src/layout.rs:658`）仍在，`STALE_SIG`（`:715`）只是止痛。改动面在渲染层，无对外契约 |
| 5 | **TUI 模态浮层滚轮？** | `select_modal`（`crates/ys-tui-coding/src/run.rs:796`）仍无滚动分支、`_ => {}`（`:865`）吃掉鼠标；会话/模型列表超一屏时只能方向键逐项走 |
| 6 | **A4 按调用类型分档强度接不接？** | 通道已通（`lib.rs:1632` 消费 `thinking.effort`），但 `crates/ys-loop/` 无一处赋值；需求是否真实存在未定 |
| 7 | **截图工具做不做、放哪？** | 需求已证实（页面渲染 + 屏幕捕获两条），实测：页面截图两平台同一条命令、应为 **S**（goals 原估 M）；屏幕捕获的权限判定是**唯一静默失败源**（无权限时 `screencapture` 仍 exit 0 且出假图，`xcap` 全屏路径同样不判权限）。选项：`apps/yushan-shot/` workspace 成员（进得进 `par-test.sh` 与变异测试）/ `skills/screenshot/scripts/` 自举（进不了测试体系）/ 只用 shell 不做二进制。据 `docs/arch/screenshot-tool/context.md` §5（优先级证据）与 §7（未澄清） |
