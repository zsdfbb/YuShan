# Subagent 异步收敛设计计划

> 日期：2026-11-01
> 决策记录：本轮需新 ADR —— **取代 ADR-0055**；**落实 ADR-0054 决策 ⑤**（其余推迟）
> 性质：**结构性改造**。删除一个工具、拆掉一条等待链、改一条唤醒语义、新增一个工具。
> 范围：`apps/coding-agent` 与 `crates/ys-loop` / `ys-component` / `ys-runtime`；不动 `ys-core` / `ys-event` / `ys-protocol`。

## 1. 背景与动机

需求由四条要求递进而成，每一条都在拆掉上一条的前提：

1. **删除同步派发**
   > 「我计划不再提供同步 subagent，从用户体验上，同步的 subagent 体验很差。我们用主 agent 无法观察同步 subagent 的操作。」

   同步 `subagent` 的痛点是**可观察性**：子 agent 在父回合内跑，父模型只能阻塞等待，
   用户看不到中间状态，父模型也拿不到句柄去干预。

2. **子会话可被观察**
   > 「后续 subagent 会写自己的 session 文件，所以主 agent 能够观察到异步 subagent 的最新状态」

   这条把「观察」的载体定为**子 agent 自己的 session 文件** —— 主 agent 读文件即读状态。
   故本设计**不需要**在协议层新增状态推送（对应 ADR-0054 的 `SubagentBoard` / `ProgressTap`
   本轮推迟，见 §8）。

3. **结果不能等用户开口**
   > 「异步 subagent 给主 agent 的回复不能在用户下一轮主动问时才被响应，应该和 bash_bg 走类似的通道。」

   现状正是「等用户开口」：`Wake::SubagentReturned` 只推一行提示（`app_loop.rs:410-446`），
   真正的注入归下一回合开头的补投（`app_loop.rs:1564-1567`），而那一回合的入口是
   `Request::Prompt`（`app_loop.rs:390`）。用户不说话，结果就一直躺着。

4. **父可向运行中的子发话**
   > 「如果真的异步，我希望主 agent 能够和 subagent 通话，比如通知他们尽快收敛。」

   这是**唯一需要新能力**的一条，但它需要的通道**已经存在**（§2.1）。

**四条如何收敛成一件改动**：一旦承认「父回合不等子 agent」（第 1 条的用户裁定 Q1），
就必须给结果一条**不依赖用户输入**的回程（第 3 条）；而结果自动回来会带来一个
新问题 —— 父 agent 现在能看见子 agent 在跑，却无法影响它，于是需要第 4 条。
第 2 条则决定了「观察」用文件而不是新协议，从而把工作量的重心压在**生命周期**上，
而不是**状态推送**上。

**用户裁定**（本轮全部采纳）：
- **Q1 不用同步等待** —— 废弃 ADR-0055「父回合等待子 agent」。
- **Q2 `subagent_send` 不限制语义** —— 就是一条普通消息进子会话，不特判成「收敛指令」。
- **Q3 四块一起完成** —— 删同步工具 / 去等待 / 自动起回合 / 对话工具。

## 2. 现状

### 2.1 父 → 子通道：协议层已通，零改动

这是本设计最重要的事实：**给子 agent 发消息不需要新增信道、不需要新增协议格。**

| 环节 | 位置 | 行为 |
|---|---|---|
| 子的边界源 | `subagent.rs:119` `ChildBoundary::new` | 新建 `own: Arc<QueueBoundarySource>` |
| 只取自己那份 | `subagent.rs:142-143` `fn take(&self) -> Option<Boundary> { self.own.take() }` | **绝不消费父队列** |
| 交出自身 `Arc` | `subagent.rs:127-128` `pub fn own(&self) -> Arc<QueueBoundarySource>` | 供外部持有句柄 |
| 交给子 agent | `subagent.rs:1339` `ChildBoundary::new(...)`、`:1367` `Some(Arc::new(child_boundary))` | 进 `AgentPorts::new` |
| **子的消费点** | `crates/ys-loop/src/basic.rs:273-282` `Boundary::Steer(msg)` | `session.append(msg)` + emit `AgentEvent::UserMessage` + `consumed = true` |

⇒ **往子的 `own` 队列 push 一条 `Boundary::Steer`，子模型下一轮就会看到它**。
`basic.rs:252-254` 明确写着「取到 `Steer` / `Deferred` 就**继续本轮循环**（模型下一轮就能看到它）」。

注意 `subagent.rs:155-157` 的 `has_pending_steer()` 恒 `false`：它只控制
「父的插话是否打断子**正在跑的工具**」，注释（`subagent.rs:148-154`）的理由仍成立，
且**不阻断 `take()` 通路**（`take` 取的是 `own`，与探针无关）。**本轮不改它。**

### 2.2 缺陷：句柄跨回合重复

```rust
// apps/coding-agent/src/subagent.rs:1084
let handle = format!("sa{}", self.dispatched.load(Ordering::Relaxed));
```

`dispatched` 是 `AppSubagentRunner` 的字段（`subagent.rs:804`，`:862` 初始化为 `0`），
而 runner **每回合新建**（`subagent.rs:978-1000` `make_runner`；生产调用点 `app_loop.rs:1576-1583`
与 `main.rs:1973-1980`）⇒ **每个回合的第一个子 agent 都叫 `sa1`**。

`subagent.rs:1082-1083` 的注释自认「**不需要全局唯一**」—— 那条判断在「句柄只用于显示」时成立，
在「句柄用于寻址」时失效。`subagent_send` 正是按 handle 寻址。

`PendingResults` 也按 handle 索引（`subagent.rs:300-338`），但它是队列（`take_pending`
全量取走，`subagent.rs:358-370`），不去重 ⇒ 重名被掩盖。

**handle 用途全清单**（本轮全部要跟着改或确认）：

| 用途 | 位置 |
|---|---|
| 生成 | `subagent.rs:1084` |
| 返回给工具 | `subagent.rs:1099` `Ok(handle)` |
| 拼进 `ToolResult` | `subagent.rs:1955-1963`（`handle: {handle}`） |
| 测试断言 `"sa1"` / `"sa2"` | `subagent.rs:490`、`:501`、`:519`、`:2012`、`:2018`、`:2021`、`:2028`、`:2047`、`:4175`、`:4189` |

### 2.3 要拆的等待链（ADR-0055）

| 部件 | 位置 |
|---|---|
| `enum SubagentWait` | `crates/ys-loop/src/basic.rs:167-180` |
| `async fn wait_for_subagents` | `basic.rs:199-240` |
| `fn has_running_subagents` | `basic.rs:242-244` |
| 调用点（`BoundaryDrain::Empty` 分支，`basic.rs:1219` 起） | `basic.rs:1221-1252` |
| `SubagentRunner::running` / `settled` | `crates/ys-tool/src/subagent.rs:113-120` / `:124-127`（默认实现 `0` / `pending()`） |
| `RunLimits::subagent_wait` / `subagent_poll` | `crates/ys-component/src/limits.rs:88`、`:95`、默认值 `:230`、`:232` |
| 默认值测试 | `limits.rs:327-346` |
| `Agent::set_subagent_wait` / `subagent_wait` | `crates/ys-runtime/src/agent.rs:204`、`:211` |
| 每回合收紧 | `apps/coding-agent/src/app_loop.rs:1484-1500` |
| `-p` 路径收紧 | `apps/coding-agent/src/main.rs:1956-1966` |
| `Config::subagent_wait` / `subagent_wait_secs` | `apps/coding-agent/src/config.rs:51-52`、`:161-166`、`:426-436`、`:679` |
| 实现方 | `subagent.rs:1041-1048`（读 `self.live`，**不是**自己的计数器） |
| 测试（含两条变异说明） | `basic.rs:5288-5358`（`final_answer_waits_for_running_subagents`）、`:5295` 变异注、`:5360-5370`（`final_answer_returns_immediately_without_running_subagents`）、`:5362` 变异注、`:5405-5420`（超时收场） |

`FakeSubagentRunner`（`basic.rs:5233`）与 `ProbeRunner`（`basic.rs:2580`）都实现了
`run` / `spawn` / `running` / `settled`；删字段后 `running`（若删）也要跟着删。

### 2.4 `LiveSubagents` 现状

`subagent.rs:413-465`：`running: AtomicUsize` + `settled: Notify` + `enter()` 返回 `LiveGuard`
（`Drop` 自减并 `notify_waiters`）。跨 `/resume` 以 `Arc` 共享（`subagent.rs:893-897`、`:914-916`）。

**`settled_notified` 的唯一用户**是 `SubagentRunner::settled`（`subagent.rs:1048`），
而 `settled` 的唯一用户是 `wait_for_subagents`（`basic.rs:232`）。⇒ 删等待链后，
`settled` 通路整条失效。

### 2.5 自动起回合的现成模板（`bash_bg`）

| 环节 | 位置 |
|---|---|
| 唤醒源 | `app_loop.rs:400` `Some(()) = optional_wake(background) => Wake::BackgroundReturned` |
| `optional_wake` | `app_loop.rs:148-153` |
| 复核 | `app_loop.rs:448-455` → `should_start_background_turn`（`:1396-1398`）→ `tasks.has_pending(session)` |
| 空则跳过 | `app_loop.rs:456-458` `if !has_pending { continue; }` |
| 占位输入 | `app_loop.rs:1407-1414` `background_placeholder()`，`Role::Harness` |
| 起回合 | `app_loop.rs:467-...` 调 `run_turn_chain(...)`（签名 `:1295-1318`） |
| 回合链共用 | `run_turn_chain` 的两个调用点：`Request::Prompt` 与 `Wake::BackgroundReturned` |

`Wake::SubagentReturned` 现状（`app_loop.rs:410-446`）：复核 `s.pending_results().has_pending()`
（`subagent.rs:346`），**只 `send_or_flush` 一行 `Outbound::Output`**，然后 `continue`。
同处注释（`:411-412`）写明「投递仍归下一回合开头的补投（`take_pending_results`）」，
`:4373` 附近与 `:4790` / `:4932` 的测试仍以 ADR-0055 为前提。

**三个 pending 名字要分清**（易错点，核实结果）：

| 名字 | 位置 | 语义 |
|---|---|---|
| `PendingResults::has_pending` | `subagent.rs:346` | 表里是否有未投出结果 |
| `PendingResults::take_pending` | `subagent.rs:363` | 真正取走（内部） |
| `PendingResults::notified` | `subagent.rs:354` | 空闲分支等待的唤醒（`Notify`，存 permit） |
| `SubagentSetup::take_pending_results` | `subagent.rs:973-975` | **外层转发**，`run_turn_once` 补投用（`app_loop.rs:1564`） |
| `SubagentSetup::pending_results` | `subagent.rs:1004-1006` | 拿表本体（`has_pending` 复核用，`app_loop.rs:413`） |

⇒ 自动起回合臂里用 `pending_results().has_pending()` 复核，用 `run_turn_chain` 起回合；
**补投仍留在 `run_turn_once`**（`:1564-1567`），起回合后由它把结果 `push(Boundary::Deferred)`
进新回合的边界源 —— 这与 `bash_bg` 的形状完全一致（`bash_bg` 的补投也在同一处）。

### 2.6 同步工具与父表锚点

| 项 | 位置 |
|---|---|
| `SubagentTool`（同步） | `subagent.rs:1814-1824`（结构体 + `new`）、`:1826` 的 `impl Tool` |
| `SubagentAsyncTool` | `subagent.rs:1902`、`impl Tool` `:1917` |
| 装配 | `assembly.rs:201` push 同步、`:202` push 异步 |
| 装配注释（追加顺序是契约） | `assembly.rs:198-200` |
| 父表锚点 | `toolset.rs:644`（`bash_bg`）、`:645`（`SubagentTool`）、`:646`（`SubagentAsyncTool`）、`:647-651`（`parent_specs.len() == 12`）、`:652-660`（`vec!["bash_bg", "subagent", "subagent_async"]`） |
| 子表锚点 | `toolset.rs:630`（9 元）、`:40-50` `EXPECTED_ORDER` |
| `SubagentRunner::run` | `crates/ys-tool/src/subagent.rs:61-68`；`spawn` 默认实现在 `:91-` |
| `run` 的生产调用点 | **只有** `subagent.rs:1867-1870`（`SubagentTool::call` 内） |
| `run` 的测试实现 | `ys-tool/src/subagent.rs:137`、`ys-tool/src/context.rs:97`、`basic.rs:2581`、`basic.rs:5234`、`subagent.rs:2272`、`subagent.rs:2342` |
| `SubagentTool` 的其余引用 | `toolset.rs:645`、`assembly.rs:46`、`assembly.rs:201`、`app_loop.rs:4393`（测试）、`subagent.rs:2290`/`:2374`/`:2441`/`:2467`/`:2587`（测试） |

### 2.7 提示词门控

| 位置 | 内容 |
|---|---|
| `prompt.rs:73-79` | `skill_section(&catalogue.lines, tools.iter().any(|t| t.name == "subagent"))` |
| `prompt.rs:165-169` | 同上（另一条提示构造路径） |
| `prompt.rs:222-227` | `fn skill_section(..., has_subagent: bool)` → 能力边界声明 |
| `prompt.rs:468-484` | `if tools.contains(&"subagent_async")` 段（含「turn keeps running until the sub-agent reports back」—— **ADR-0055 的话，本轮要改**） |
| `prompt.rs:485-545` | `if tools.contains(&"subagent")` 整段指引（规模、依赖、失败诊断、成本） |

## 3. 设计

四块，按依赖顺序：先删（3.1）、再拆等待（3.2）、再改唤醒（3.3）、最后加新工具（3.4）。
§2.2 的句柄修复是 3.4 的前提，写在 3.4 里。

### 3.1 删除同步 `subagent` 工具

**删什么**

1. `SubagentTool` 结构体与 `impl Tool`（`subagent.rs:1814-1824`、`:1826` 至 `SubagentAsyncTool` 之前的整段）。
2. `assembly.rs:201` 的 `tools.push(Box::new(SubagentTool::new(...)))` 与 `assembly.rs:46` 的 import。
3. `toolset.rs:645` 的 `parent_specs.push(SubagentTool::new(...).spec())`。
4. `prompt.rs:485-545` 的 `if tools.contains(&"subagent")` 整段 —— **不是**改成
   `subagent_async` 的副本。理由：那段的四条建议（并行槽位数、依赖与写入范围、失败先读诊断、
   成本提醒）中，前两条已被 `:468-484` 的 `subagent_async` 段覆盖（`:468-484` 已含
   「dispatched in ONE message ⇒ 并行」与「do NOT poll」）；后两条与「同一条消息里多个调用
   进同一批」的措辞是 `subagent` 特有的（同步调用才会「一次拿到多个结果」）。
   处置：**把仍然成立的两条**（失败先读诊断 / 成本提醒）**并入 `subagent_async` 段**，
   其余删掉。具体并哪几条由实现者在 impl 任务里按 `prompt.rs:485-545` 原文逐条判定，
   判定结果写进该任务的 review note。
5. `prompt.rs:73-79` / `:165-169` / `:222-227` 的 `has_subagent` 判据：`"subagent"` →
   `"subagent_async"`。**必须改**，否则子表的 `skill_section` 会永远报「no subagents」
   （子表本来就没有派发工具，那对子 agent 是真话；但父表现在也没有 `subagent`，
   不改就会让父 agent 的提示自相矛盾）。
6. `prompt.rs:468-484` 段内「the turn keeps running until the sub-agent reports back,
   so never claim it is still running」一句 —— ADR-0055 的承诺，本轮**换成**
   「result arrives as a new turn automatically」。

**`SubagentRunner::run` 的处置裁定：一并删除。**

理由（三条，都可核）：
- 生产调用点**只有** `subagent.rs:1867` 一处，就在被删的 `SubagentTool::call` 内（§2.6 已核）。
- 留着它就是留一条「同步等待子 agent」的能力缝 —— 正是用户 Q1 否决的形态。
  `ys-tool` 的 trait 一旦保留，实现方（`subagent.rs:1011`）必须继续维护一条无人调用的
  阻塞路径，且它携带的「父回合等待」语义与 `boundary` 参数会持续误导读者。
- 它的存在会让 3.2「去等待」不彻底：读者仍会问「run 里是不是也在等」。

**代价**（明确接受）：`ys-tool` 的 `SubagentRunner` 变成单方法 trait（只剩 `spawn`），
6 处测试 fake 要删 `run` 实现（§2.6 清单）。这是机械改动，且删完 trait 更窄。

**`SubagentOutcome` 不删**：`spawn` 的后台任务仍产出它（`outcome_message` 用，
`subagent.rs:1107-1114`），只是不再经 `run` 返回。

**父表元数**：9 基础元 + `bash_bg` + `subagent_async` + 新增 `subagent_send` ⇒ **仍为 12 元**。
注意：`toolset.rs:647-660` 的锚点断言的是**元数与追加段名字列表**，本轮净变化是
「`subagent` → `subagent_send`」，`parent_specs.len()` 保持 `12`，`vec![...]` 里的
`"subagent"` 换成 `"subagent_send"`。这与任务简报里的「12 元 → 11 元」不同 ——
**以实际代码为准**：只做 3.1 时中间态是 11 元，3.4 落地后回到 12 元。
为避免中间态长期存在，exec-plan 把两处锚点改动排成相邻的两波（U2 → U4 → U5）。

### 3.2 去等待：父回合不再阻塞

**删什么**

1. `basic.rs` 的 `enum SubagentWait`（`:166-180`）、`wait_for_subagents`（`:199-240`）、
   `has_running_subagents`（`:242-244`）。
2. `basic.rs:1220-1252` 的整段 `if has_running_subagents(ctx) { match wait_for_subagents(...) }`
   —— `BoundaryDrain::Empty` 分支**直接收场**（保持 `:1253` 之后原有的收场路径）。
3. `RunLimits::subagent_wait` / `subagent_poll`（`limits.rs:80-100`）、默认值（`:230`、`:232`）、
   测试（`:319-346`）。
4. `Agent::set_subagent_wait` / `subagent_wait`（`agent.rs:198-213`）。
5. `app_loop.rs:1484-1497` 的每回合收紧、`main.rs:1956-1966` 的 `-p` 收紧。
6. `Config::subagent_wait` / `subagent_wait_secs`（`config.rs:51-52`、`:161-166`、`:426-436`、`:679`）
   —— **要删**，否则留下一个读不出效果的配置项（用户改它，行为不变）。
   兼容性：**已核实** `apps/coding-agent/src/config.rs` 全文无 `deny_unknown_fields` ⇒
   删掉字段后老 `config.json` 里的 `subagent_wait_secs` 被 serde 静默忽略，不影响启动。
7. `SubagentRunner::running` / `settled`（`ys-tool/src/subagent.rs:113-127`）与
   `subagent.rs:1041-1048` 的实现。

**`LiveSubagents` 处置裁定：保留（但降级为纯计数）。**

理由：
- `LiveGuard` 的 RAII 减法（`subagent.rs:451-458`）覆盖 `ChildTask::execute` 的四条退出路径
  （组装失败 / `Ok` / `Err` / future 被 drop），这条**不变量与是否等待无关** ——
  它保证「进程级在跑数」准确；删了守卫，未来任何读这个数的消费者都要自己重造它。
- 它已经是 `SubagentSetup` 的共享字段（`subagent.rs:893-897`、`:931`），跨 `/resume` 与
  已分离的后台任务共表；删它要动 `shared` / `new` / `make_runner` 三处签名与 9 处测试构造
  （`subagent.rs:2548`、`:2656`、`:2730`、`:2832`、`:2862`、`:3060`，及 `app_loop.rs` 的 5 处
  `SubagentSetup::shared`）。
- ADR-0054 的 TUI「N 个在跑」与本轮之后「主 agent 知道还有谁在跑」都要读这个数。

**具体降级动作**（最小）：
- `settled: Notify` 与 `settled_notified()`（`subagent.rs:441-443`）**删除** —— 唯一用户
  `SubagentRunner::settled` 已随 3.2 删除。`LiveGuard::drop` 里的 `notify_waiters()`
  （`subagent.rs:456`）一并删。
- `running: AtomicUsize` + `enter()` + `count()` **保留**。
- `subagent.rs:460-476` 的计数测试保留（它钉的是守卫不变量，不是等待）。
- 结构体文档里「ADR-0055」与「父回合钉死在等待里」的措辞改写为「在跑计数」，不写变更史。

> **不引入**「主 agent 查询在跑数」的工具（ADR-0054 决策 ⑦ 的 `subagent_status`）——
> 用户的观察载体是子会话文件（§1 第 2 条），本轮不做。

### 3.3 自动起回合

把 `Wake::SubagentReturned` 改成与 `Wake::BackgroundReturned` **同构**。

```
Wake::SubagentReturned => {
    // 复核：唤醒只说「可能有」（Notify 存 permit，可跨回合残留 —— subagent.rs:266-280）
    let has_pending = subagent.as_ref().is_some_and(|s| s.pending_results().has_pending());
    if !has_pending { continue; }
    // 占位输入：Role::Harness（与 background_placeholder 同形，app_loop.rs:1407-1414）
    run_turn_chain(..., subagent_placeholder(), None).await?;
    continue;
}
```

**三个设计点**：

1. **复核必须留**。`PendingResults` 的 `Notify` 存 permit（`subagent.rs:266-280`），
   跨回合残留会造成「假醒」。`has_pending`（`subagent.rs:346`）是唯一可靠的复核。
   与 `bash_bg` 侧 `should_start_background_turn`（`app_loop.rs:1396-1398`）同理。

2. **占位消息走 `Role::Harness`，正文说明「这只是占位」**。与 `background_placeholder`
   （`app_loop.rs:1407-1414`）同一条理由：真正的输入是随后经 `Boundary::Deferred` 注入的
   结果（`run_turn_once` 的补投，`app_loop.rs:1564-1567`），这一行只是让回合有输入。
   `raw_user_text` 传 `None`（`run_turn_chain` 的 `:1318`；该参数是标题素材，
   harness 占位没有原文 —— `:1311-1313` 的注释已定）。

3. **不重复投递**。起回合后，补投仍在 `run_turn_once` 开头做（`:1564-1567`），
   它 `take_pending_results`（`subagent.rs:973`）全量取走并 `push(Boundary::Deferred)`。
   ⇒ `Wake::SubagentReturned` 臂里**不得**再取一次（`has_pending` 只读、不取）。
   这与现状的「只告知不取」纪律一致，只是把「告知」换成「起回合」。

**`-p` / `--json` 路径（`main.rs:1908-1990`）不做自动起回合**：那是一次性单回合模式
（`main.rs:1958-1961` 注释已说明「回合结束后才完成的结果没有下一回合可补投」）。
本轮保持该行为，并在 `main.rs` 该处注释里写清结论（不写变更史，只写「单回合模式无自动回合」）。

**需要同步改的既有测试**：
- `app_loop.rs:4717-4860` `late_subagent_result_is_announced_while_idle` —— 它断言
  「输出一行提示」，现在应断言「起了一个新回合」。整条用例要重写（见 §7 合同 5）。
- `app_loop.rs:4868-4960` `parent_waits_for_slow_subagent_within_the_same_turn` ——
  **删除**（它验证的正是被废弃的 ADR-0055 等待），由 §7 合同 4 的反向用例替代。
- `app_loop.rs:4785-4795`、`:4928-4940` 里 `limits.subagent_wait` 的赋值随字段删除一起删。

### 3.4 `subagent_send` 工具 + 句柄全局唯一

#### 3.4.1 句柄全局唯一 `sa{turn}.{n}`（ADR-0054 决策 ⑤）

**改法**：`AppSubagentRunner` 增加一个「本回合序号」的入参（`make_runner` 已能拿到回合相关状态，
`app_loop.rs:1576-1583` 的调用点在 `run_turn_once` 内，`*ctx.turns` 在 `:1460` 已自增；
`main.rs:1973-1980` 的单回合模式固定为 `1`），生成：

```rust
let handle = format!("sa{}.{}", self.turn, self.dispatched.load(Ordering::Relaxed));
```

`n` 仍是 per-runner 的原子计数（`subagent.rs:804`、`:862`）⇒ **计数逻辑零改动**，
只加前缀。`sa1.1` / `sa1.2` / `sa2.1` 唯一且可读（一眼对上第几回合）。

**不用进程级原子计数器**：`sa17` 跨会话不可读，且 `/resume` 后序号无意义（ADR-0054 决策 ⑤ 已论）。

**连带的测试改动**：`subagent.rs:4175`、`:4189` 的 `assert_eq!(handle, "sa1")` →
按新形态断言；`subagent.rs:490`/`:501`/`:519`/`:2012`/`:2018`/`:2021`/`:2028`/`:2047`
是 `PendingResults` 的单元测试，用**任意字符串**作 handle ⇒ **不必改**（核实：它们只验证
「按 handle 取出并保序」，与格式无关）。

#### 3.4.2 句柄注册表（新）

**问题**：`subagent_send` 要按 handle 找到**那个子 agent 的 `own` 队列**，而 `own` 是
`ChildBoundary::new` 时（`subagent.rs:119`）在 `ChildTask::execute` 内部创建的
（`subagent.rs:1339`、`:1342` `let child_own = child_boundary.own();`）——**spawn 时还不存在**。
且 `ChildTask::execute` 跑在分离的 `tokio::spawn` 里（`subagent.rs:1092-1101`）。

**形态**（最小）：与 `PendingResults` **并列、同形、同共享路径**（`Arc` 经 `SubagentSetup` 跨重建）：

```rust
/// handle → 子 agent 自己那份边界源（`send` 的投递目标）。
#[derive(Default)]
pub struct SubagentHandles {
    inner: Mutex<HashMap<String, Weak<QueueBoundarySource>>>,
}
```

- **`Weak` 而非 `Arc`**：子 agent 收尾后 `ChildBoundary` 被 drop（`child_boundary` move 进
  `Arc` 交给 `AgentPorts`，`subagent.rs:1367`，`run_turn` 结束即释放）⇒ 注册项**自动**
  不再有效，无需「完成后清理」的簿记。`upgrade()` 失败即视为「该子 agent 已收尾」。
  （若用 `Arc` 持有，注册表会把每个子 agent 的边界源钉到进程结束 —— 泄漏，且会让
  `is_aborted` 之类的语义跨回合残留。）
- **注册时机**：`subagent.rs:1342` 取到 `child_own` 之后、`tokio::spawn` 之前，
  用 `ChildTask` 携带的 handle 注册。⇒ handle 在 `spawn`（`:1070-1099`）返回时就已可见？
  **不**：`ChildTask::execute` 在 spawn 之后才跑，存在一个「handle 已返回、注册未完成」的窗口
  （父 agent 拿到 handle 立刻 `send` 会 miss）。
  **处置**：把注册提前到 `spawn` 内、`tokio::spawn` 之前 —— `own` 的 `Arc` 由
  **spawn 侧**创建并传入 `ChildTask`（`ChildBoundary::new` 拆成「外部给 `own`」的形态），
  于是注册与 spawn 在同一同步段内完成，无窗口。
  这是本设计对现有代码的**唯一结构性侵入**：`ChildBoundary::new` 改成
  `new_with_own(parent, own: Arc<QueueBoundarySource>)`（或让 `ChildTask` 持 `own` 并在
  `execute` 里用它构造 `ChildBoundary`；两者等价，取后者更小 —— 只改 `execute` 的两行）。
- **注销**：不需要。`Weak` 失效即自然过期。

**`subagent_send` 的语义（Q2：不限制语义）**：把入参 `message` 包成
`Message { role: Role::User, content: [Text { message }] }`，`push(Boundary::Steer(msg))`
进目标子 agent 的 `own`（`QueueBoundarySource::push`，`crates/ys-protocol/src/boundary.rs:119-123`）。

**不做的事**（明确列出，防止实现时扩权）：
- 不检查子 agent 是否该收敛、不注入任何模板文本、不等待回应。
- 不设「只能发一条」「发完转为 Abort」之类的特判 —— 用户 Q2 明确不要。

**fail-closed（合同 2）**：`upgrade()` 失败、或 handle 不在表里 ⇒
`Err(ToolError::InvalidInput(...))`，错误正文列出**当前有效**的 handle 列表。
绝不静默 `Ok` —— 静默会让父模型以为话已送到。

#### 3.4.3 工具形状

```rust
ToolSpec::new(
    "subagent_send",
    "Send a message to a sub-agent that is still running; it sees it on its next round",
    json!({"type":"object","properties":{
        "handle": {"type":"string","description":"Handle printed by subagent_async (e.g. sa3.2)"},
        "message": {"type":"string","description":"What to tell it"}
    },"required":["handle","message"]}),
)
```

- `is_concurrency_safe()`：**`true`** —— 它只往队列 push 一条消息，与 `SubagentAsyncTool`
  （`subagent.rs:1941-1943`）同理。
- 挂载点：`ToolContext` **不加字段**。`subagent_send` 需要的是注册表，而注册表归
  `SubagentSetup`；`SubagentRunner` 已是 `ToolContext` 的第 3 个同形挂载点
  （`ys-tool/src/context.rs:23-28`），给它加一个 `send(&self, handle, message) -> Result<(), String>`
  方法即可（**trait 方法，不是新字段**），实现方 `AppSubagentRunner` 转给注册表。
  理由：避免第 4 个挂载点与 `ys-tool` 的公共 API 扩张；且 `send` 的语义天然属于「派发能力」。
- 返回值：`ToolResult { content: format!("Message queued for {handle}."), is_error: false }`。

**为什么不用 `subagent_async` 的入参承载 `send`**：工具入参是模型每次调用都要产出的 JSON，
把「派发」与「发消息」混成一个工具会让 schema 长两支、`prompt.rs` 的门控也说不清。
独立工具让 `prompt.rs:468-484` 段只需加一句用法（「用 `subagent_send` 告诉它收敛」）。

## 4. 关键设计决策与理由

| # | 决策 | 理由 | 证据 |
|---|---|---|---|
| D1 | 父 → 子用 `Boundary::Steer` 走子自己的 `own` 队列，**不改协议** | 通道已通：`take()` 只取 `own`，`basic.rs:273` 处理 `Steer` 即 append + `consumed` | `subagent.rs:142`、`basic.rs:273-282` |
| D2 | **不改** `ChildBoundary::has_pending_steer()` 的恒 `false` | 它只控制「父插话是否打断子正在跑的工具」；理由（父插话不该表现为子命令莫名中止）仍成立，且不阻断 `take()` | `subagent.rs:148-157` |
| D3 | 删 `SubagentRunner::run`（不只是删工具） | 生产调用点只有 `subagent.rs:1867` 一处；留着就是留同步等待的能力缝，与 Q1 冲突 | §2.6 已核 |
| D4 | `LiveSubagents` 保留、只删 `settled` / `settled_notified` | 守卫覆盖四条退出路径，是不变量而非等待专用；且是 ADR-0054 TUI 计数的现成基座 | `subagent.rs:451-458`、`:441`、`:1048` |
| D5 | 句柄注册表用 `Weak`，无注销逻辑 | 子收尾后 `ChildBoundary` 随 `run_turn` 释放 ⇒ `upgrade()` 失败即「已收尾」，无需簿记；`Arc` 会钉住到进程结束 | `subagent.rs:1339-1367` |
| D6 | 注册与 `spawn` 在同一同步段（`own` 由 spawn 侧创建后传入） | 否则「handle 已返回、注册未完成」窗口里 `send` 必 miss | `subagent.rs:1087-1099` |
| D7 | 未知 handle 返回 `Err` 并列有效列表 | 静默 `Ok` 会让父模型以为话送到了（与 `parse_subagent_mode` 的 fail-closed 同一条纪律） | `subagent.rs:1804-1812` |
| D8 | 自动回合的复核用 `has_pending`，取走仍归 `run_turn_once` 补投 | `Notify` 存 permit 会假醒；双取会重复送达 | `subagent.rs:266-280`、`app_loop.rs:1564` |
| D9 | 占位消息 `Role::Harness` + 正文自述「占位」 | 与 `background_placeholder` 同形：真内容随后经 `Deferred` 注入；`Harness` 保证 TUI 不画成用户发言 | `app_loop.rs:1402-1413`、`ys-tui-coding/src/app.rs:332` |
| D10 | 父表仍是 12 元（`subagent` → `subagent_send`） | `subagent_async` + `subagent_send` + `bash_bg` + 9 基础 = 12；锚点断言的是元数与名字列表 | `toolset.rs:645-657` |
| D11 | `prompt.rs` 的 `has_subagent` 判据改 `subagent_async` | 否则父表没有 `subagent` ⇒ 能力声明谎报「no subagents」 | `prompt.rs:77`、`:167`、`:222-227` |
| D12 | `-p` / `--json` 不自动起回合 | 单回合模式没有「下一回合」，且该路径已有注释定过此结论 | `main.rs:1956-1961` |

## 5. 与既有决策的关系

| 决策 | 本轮处置 |
|---|---|
| **ADR-0055**（父回合等得起） | **被本设计取代**，需新 ADR（`0065-subagent-send-and-auto-turn.md` 或同级编号，由实现者按当时最大编号取）。新 ADR 记：为何放弃等待（可观察性优先、用户裁定 Q1）、自动回合为何不是「替用户花钱」（它是 `bash_bg` 同构的既有形态）、`subagent_send` 为何不限制语义（Q2）。ADR-0055 正文加 `Status: superseded by <新编号>`，**不删**（记 why）。 |
| **ADR-0054** 决策 ⑤（句柄全局唯一） | **本轮落实**，`sa{turn}.{n}`。ADR-0054 的 Status 从 `proposed` 改为 `partially implemented`（决策 ⑤ 已落），并在正文标注哪几项已落。 |
| **ADR-0054** 其余（`SubagentBoard` / `ProgressTap` / `AgentEvent::SubagentProgress` / 四态状态机 / 决策 ⑥ 增量落盘 / 决策 ⑦ `subagent_status`） | **本轮不做**（见 §8）。ADR-0054 保持 `proposed`（部分）。 |
| **ADR-0029 §6 修正 4**（`take()` 从「恒 None」改为取自己的） | 本设计**依赖**它；D1 是它的直接延伸。ADR 不改。 |
| **ADR-0013 / ADR-0047**（取消即消息、投递目标） | 不改。`Steer` 的既有语义（轮内额外输入，`basic.rs:249-254`）被复用，不新增语义。 |
| **ADR-0049**（收场前 drain） | **强化**：`drain_boundary` 的收场前那次 drain 仍在（`basic.rs:1208` 注释），只是不再有 `wait_for_subagents` 跟在后面。 |

## 6. 波及文档

| 文件 | 动作 |
|---|---|
| `docs/adr/0055-subagent-continuation.md` | Status → `superseded by <新 ADR>` |
| `docs/adr/0054-subagent-progress-board.md` | Status → 部分落地；标注决策 ⑤ 已落、其余待做 |
| `docs/adr/<新编号>-subagent-send-and-auto-turn.md` | **新建**：本设计的决策记录 |
| `docs/design-final/subagent-continuation.md` | 标 `[已被取代]`，指向新 ADR 与本设计 |
| `docs/design-final/subagent.md`（若存在） | 更新工具表与「同步/异步」两节 |
| `docs/architecture.md` | §输出三分 / 时序图里若有「父回合等待子 agent」的段落要改 |
| `docs/design.md` | 测试矩阵里 subagent 相关条目按 §7 更新 |
| `docs/TODO.md` | 删掉 ADR-0055 等待链相关条目（`:91` 附近）；ADR-0054 条目改为「决策 ⑤ 已落，其余待做」 |
| `AGENTS.md` | **不改**（它不含 subagent 工具的元数/等待语义；改它违反「每一轮都需要吗」的判据） |
| `apps/coding-agent/README.md` | 若列了工具清单，删 `subagent` 加 `subagent_send` |

## 7. 测试策略（8 条合同）

每条给**可自动化验收方法**。「验收人」一律为 review subagent（`shallow` 或 `deep` 按行内标注）。

| # | 合同 | 优先级 | 验收方法 | 验收人 |
|---|---|---|---|---|
| 1 | `subagent_send` 把 `Steer` 推进指定子 agent 队列，子下一轮看到它 | 必测 | **单测**（`subagent.rs` 的 tests mod）：构造 `ChildBoundary` 与注册表，`send("sa1.1", "收敛")` 后 `own.take()` 得到 `Boundary::Steer(msg)` 且正文 == `"收敛"`。**集成测**（`crates/ys-loop`）：`MockModel` 第一次 `complete` 时向 `own` push `Steer`（仿 `basic.rs:4440-4458` 的 `SteerDuringFinalModel`），断言第二次请求的 `messages` 里含该文本。变异：把 `push` 改成推给父的边界源 ⇒ 单测红 | deep |
| 2 | 未知 handle 返回 `Err` 而非静默 | 必测 | **单测**：`send("sa9.9", ...)` ⇒ `is_err()`，错误正文包含有效 handle 列表；`send` 一个**已收尾**（`Weak` 已失效）的 handle ⇒ 同样是 `Err`。变异：返回 `Ok` ⇒ 红 | shallow |
| 3 | handle 跨回合唯一 | 必测 | **单测**（`subagent.rs`）：两个不同 `turn` 的 runner 各 `spawn` 一次 ⇒ 断言 `assert_ne!(h1, h2)` 且形如 `sa1.1` / `sa2.1`（正则 `^sa\d+\.\d+$`）。**变异**：把 `turn` 硬编码为 `1` ⇒ 红 | shallow |
| 4 | 父回合在子 agent 仍在跑时**正常收场** | 必测 | **集成测**（`app_loop.rs`）：把现有 `parent_waits_for_slow_subagent_within_the_same_turn`（`:4868`）改写成反向断言 —— 子模型睡 400ms，父模型第一次 `complete` 立刻给最终答复 ⇒ 断言父回合在 **< 200ms** 内收场，且收场时子 agent 仍在跑（`live.count() == 1`）。变异：把 `wait_for_subagents` 加回来 ⇒ 墙钟断言红 | deep |
| 5 | 子结果自动起回合，不经用户发言 | 必测 | **集成测**：改写 `late_subagent_result_is_announced_while_idle`（`:4717`）—— 父回合收场后子 agent 才返回，断言 ① 出现**第二个**回合（`Envelope.turn` 递增 / 父模型 `complete` 被再次调用）；② 该回合的请求里含子结果正文；③ 全程**没有** `Request::Prompt`。变异：把臂改成 `continue`（只提示）⇒ ② 红 | deep |
| 6 | 父表 11+1 元、无 `subagent`、`subagent_async` 仍可并发 | 必测 | **单测**（`toolset.rs`）：`parent_specs.len() == 12`；名字列表含 `subagent_async` / `subagent_send`，**不含** `subagent`（显式 `assert!(!names.contains(&"subagent"))`）；子表仍 9 元（`:630` 不动）。**并发**：`SubagentAsyncTool::is_concurrency_safe() == true`（沿用既有断言）。变异：把 `subagent` 的 spec push 回来 ⇒ 前两条红 | deep |
| 7 | 子 agent 被 `send` 后仍能正常收尾 | 必测 | **集成测**：子 agent 跑 ≥2 轮，中途 `send` 一条 ⇒ 断言 ① 子 `RunResult.stop_reason == Completed`；② 子会话 `messages` 的相邻序列允许出现 `assistant → user`（见下方核实结论），且**不出现** `LoopError`；③ 转录 `.md` 已落盘。变异：让 `take()` 在 `Steer` 后返回 `None`（吞掉） ⇒ ① 仍绿但「子未看到消息」的子断言红 | deep |
| 8 | `/resume` 后旧会话遗留结果不注入新会话 | **可推迟** | 推迟理由：`PendingResults` 是**进程级、会话无关**（`subagent.rs:358-362` 注释已定：「表是进程级的、会话无关……跨会话串台的风险由『一条结果只入表一次』保证」）⇒ 现状本就会把同进程内别的会话的结果投给新会话。本设计的自动起回合会**放大**这个既有边界（无人发言也会起回合）。本轮记录为已知缺口，不实现会话归属。验收（推迟后）：构造两个会话共享 `SubagentSetup`，在会话 A 派发、`/resume` 到 B，断言 B 的回合里**不**出现 A 的结果 | shallow |

### 合同 7 的核实结论（第 7 条是本轮新风险点）

**问题**：子 agent 收到 `Steer` 会 `session.append(msg)`（`basic.rs:274-277`），子会话的相邻
序列变成 `assistant → user`（`Role::User`，不是 `Harness`）。子侧会不会因此报错？

**核实链**：

1. **会话层不校验交替**。`MemorySession::append` 只是 `Vec::push`
   （`crates/ys-session/src/memory.rs:29-33`），没有任何角色交替检查。
2. **请求组装不合并、不校验**。`basic.rs:785` `let mut request_messages = ctx.session.messages().to_vec();`
   —— 逐条搬运，无合并逻辑。
3. **出站映射允许连续 user**。`adapters/model-openai-compatible/src/lib.rs:1214-1232`
   逐条 push `ChatMessage`，`Role::User | Role::Harness => "user"`（`:1228`）。
   ⇒ 连续两条 `user` 的请求是**合法**的 OpenAI 形请求（该协议不要求严格交替；
   `assistant` 带 `tool_calls` 时随后的 `tool` 才是有顺序要求的）。
4. **子 agent 的 `Deferred` 与 `Steer` 走同一条 append 路径**（`basic.rs:283-294`），
   而「后台子结果进子会话」在既有实现里已存在（父把 `Deferred` 推进父会话，形态相同）。

**结论：子 agent 被 `send` 后不会报错。** 风险不在协议合法性，而在**语义重复**：
子模型可能把「父告诉它收敛」理解为「新的用户任务」而继续展开。这是提示词问题
（`subagent_send` 的 `description` 与 `prompt.rs` 的用法句要写清「它是一句话，不是新任务」），
不是代码缺陷。**合同 7 因此断言「能收尾」，不断言「按期望收敛」** —— 后者不可判定。

**补充判定**：`Steer` 在子侧的落点是 `Role::User`（`basic.rs:273` 的 `Boundary::Steer(msg)`，
msg 的 role 由投递方给）。本设计给 `Role::User`（不是 `Harness`），理由：子模型的
「谁是说话的人」只有 user / assistant 两档，用 `Harness` 会经 `:1228` 同样映射成 `user`，
但会让子侧的转录（`transcript_body` 打印 `## {:?}`，`subagent.rs:1664`）显示 `Harness`
—— 那是**父 harness** 的语义，会误导读转录的人。用 `User` 更准确：对子而言，
这确实是「外部输入」。

## 8. 已知缺口 / 本轮不做

| 项 | 为何不做 | 留下的缝 |
|---|---|---|
| `SubagentBoard` / `ProgressTap` / `AgentEvent::SubagentProgress` / 四态状态机（ADR-0054） | 用户第 2 条已定观察载体是**子会话文件**；再加一条状态推送通道是并行机制 | 子会话文件已由 `SubagentOutcome.artifact`（`ys-tool/src/subagent.rs` 的字段）与 `SubagentsDir`（`subagent.rs:618-633`）提供；将来要状态板时，`LiveSubagents` 的计数与注册表的 handle 列表是现成基座 |
| 决策 ⑥ 产物路径先分配 + 按轮 append（ADR-0054） | 与本轮四条无关；子会话文件本身已按轮写（`finish_transcript`，`subagent.rs:1439`） | `SubagentsDir::finish_transcript` 的签名不改 |
| `subagent_status` 工具（ADR-0054 决策 ⑦） | 同上；且用户偏好最简 | 注册表已能列有效 handle（`subagent_send` 的错误正文就用它） |
| 合同 8 会话归属 | `PendingResults` 是进程级、会话无关（既有设计），改动面大于本轮收益 | 将来若要，入口是 `PendingResults::deliver`（`subagent.rs:300`）加会话 id |
| `subagent_send` 的多条 / 定向广播（发给全部在跑的子 agent） | Q2 只要「一条普通消息」；广播可用模型多次调用实现 | handle 列表已在注册表里 |
| `-p` / `--json` 自动起回合 | 单回合模式无下一回合（`main.rs:1956-1961`） | 该模式本就是「跑完即退出」 |
| `subagent_send` 是否进入子 agent 的**工具表** | 子 agent 不需要向它的子 agent 发消息（它没有派发工具，子表 9 元，`toolset.rs:630`） | 若将来嵌套派发，`assembly.rs:143` 的子表构造点加一行 |

## 9. 影响面估计

- **删除行数** > 新增行数：删一条等待链（`basic.rs` ~45 行 + 测试 ~120 行）、一个同步工具
  （~90 行）、一个 trait 方法及其 6 处实现、两个 `RunLimits` 字段与两处配置项。
- 新增：注册表（~50 行）、`subagent_send` 工具（~60 行）、`Wake` 臂改写（~25 行）、
  `send` 的 trait 方法（~8 行）。
- **净效果**：少一个工具、少两个配置项、少一条阻塞路径；多一个工具、一份注册表、一条自动回合。
