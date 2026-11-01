# Subagent 异步派发、结果自动起回合与插话 — As-built

> 性质：**实际建成记录**（as-built）。只写当前代码的样子。
> 决策依据见 [ADR-0065](../adr/0065-subagent-send-and-auto-turn.md)（取代 ADR-0055；
> 落实 ADR-0054 决策 ⑤，其余仍待做，见 `docs/TODO.md`）。
> 设计输入 `docs/design-plans/2026-11-01-subagent-async-convergence.md`。
> 前身（**已取代**）`docs/design-final/subagent-continuation.md` —— 「父回合等待子 agent」已拆除。
> 日期：2026-11-01

## 一句话

派发只有一个工具（`subagent_async`），**父回合不等子 agent**；结果由进程级投递表跨回合送达，
回合已结束则**自动起一个新回合**把它交给模型；`subagent_send` 可给仍在跑的子 agent 插话。

## 1. 工具表形态

| 表 | 元数 | 内容 |
|---|---|---|
| 父表 | **12** | 基础 9 元（`read` `write` `edit` `bash` `grep` `find` `ls` `websearch` `webfetch`）+ 追加段 `bash_bg` → `subagent_async` → `subagent_send` |
| 子表（general） | **9** | 即基础 9 元，原样 |
| 探索子表（explore） | **7** | 基础 9 元去掉 `write` / `edit` |

- 基础 9 元由 `apps/coding-agent/src/toolset.rs:88` 的 `build_tools` 产出（列表在 `:94`）。
- 追加段在 `apps/coding-agent/src/assembly.rs:199`（`bash_bg`）→ `:208`（`subagent_async`）
  → `:209`（`subagent_send`）。同一段注释（`:180-187`、`:205-207`）写明**追加顺序是契约**。
- 子表由 `apps/coding-agent/src/subagent.rs:1443` 的 `child_tools` 现建；`Explore` 在 `:1447-1450`
  按名字过滤掉 `write` / `edit`（⇒ 7 元）。
- 同步 `subagent` 工具已不存在。契约由 `apps/coding-agent/src/toolset.rs:660-663` 断言钉住
  （`assert!(!parent_specs.iter().any(|s| s.name == "subagent"))`）。

**为什么追加段必须在末位**：子表是父表 JSON 的**字节前缀**，父表 = 子表去掉末尾 `]` + `,{bash_bg},{subagent_async},{subagent_send}]`。
父回合与子 agent 的首轮请求因此共享同一段工具 JSON 前缀 ⇒ 命中 prompt cache。追加项一旦插到中间，
前缀就不成立。锚点：`apps/coding-agent/src/toolset.rs:624` 的 `prefix_fingerprint_is_anchored_and_stable`
（`:637-651`：12 元 + 追加段顺序 + 不含 `subagent`）。

## 2. 派发通路

```
SubagentAsyncTool::call            apps/coding-agent/src/subagent.rs:2026
  └─ SubagentRunner::spawn         apps/coding-agent/src/subagent.rs:1206
       ├─ resolve_model / take_dispatch_slot / child_limits_for   :1213-1215
       ├─ 句柄 = sa{turn}.{n}                                     :1221-1224
       ├─ own = Arc<QueueBoundarySource> 并 register 进 handles    :1227-1228
       ├─ ChildTask（持 LiveGuard）                                :1231
       └─ tokio::spawn(execute → results.deliver)                 :1235-1243
```

- **分离任务**：`tokio::spawn` 的任务不随回合结束被取消 —— 回合收场后子 agent 继续跑。
- **模型与额度在 spawn 之前解析**：`resolve_model` / `take_dispatch_slot` 都要读 `config`，
  不能带进后台；失败 ⇒ `Err`（`spawn` 返回值），**绝不静默回退到父模型**。
- **投递表 `PendingResults`**（`apps/coding-agent/src/subagent.rs:268`）是**进程级**的：
  以 `Arc` 经 `SubagentSetup`（`:1003`）跨 `/resume` 重建共享。
- **补投**：每回合开头 `take_pending_results`（`apps/coding-agent/src/subagent.rs:1113`）
  取走全部遗留结果，由 `apps/coding-agent/src/app_loop.rs:1574-1578` 推成 `Boundary::Deferred`
  进本回合边界源。
- **本回合落点每回合绑定**：`bind_results`（`apps/coding-agent/src/subagent.rs:1105`）
  → `bind`（`:303`）返回 `PendingResultsBinding`（`:397`），drop 时按代数解绑（`unbind`，`:316`）。
  守卫写 `_subagent_binding`（`apps/coding-agent/src/app_loop.rs:1589`、`apps/coding-agent/src/main.rs:1955`）
  —— 写成 `_` 会立即 drop，落点当回合就没了。

## 3. 结果的两种到达路径

`PendingResults::deliver`（`apps/coding-agent/src/subagent.rs:326`）按「此刻是否已绑定落点」二分：

| 情形 | 行为 | 指针 |
|---|---|---|
| **(a) 回合还活着**（已绑定） | 直接 `sink.push(Boundary::Deferred { handle, message })` ⇒ `BasicLoop` 在下一个轮边界 append 进会话并 emit `DeferredResult` ⇒ 同一回合的下一轮就送到 | `subagent.rs:349`；`crates/ys-loop/src/basic.rs:206-215`（`Deferred` 走 append + 事件） |
| **(b) 回合已结束**（未绑定） | 留表（入表顺序，最旧在前）+ `wake.notify_one()` ⇒ app 循环空闲分支拿到 `Wake::SubagentReturned` ⇒ **自动起一个新回合**（`run_turn_chain`，`apps/coding-agent/src/app_loop.rs:1304`），该回合开头补投 | `subagent.rs:333-346`；`apps/coding-agent/src/app_loop.rs:402`（唤醒）、`:413-445`（自动起回合） |

自动回合的占位输入是 `Role::Harness` 的一条固定文本（`apps/coding-agent/src/app_loop.rs:1429`
的 `subagent_placeholder`），真正内容随后经 `Boundary::Deferred` 注入 —— 用户没有发言。

**`has_pending()` 只读不取**（`apps/coding-agent/src/subagent.rs:357`）。它只回答「表里还有没有」，
取走永远是 `take_pending` / `take_pending_results`。唤醒只是「可能有」：
`notify_one` 存 permit，permit 可能跨回合残留 ⇒ `apps/coding-agent/src/app_loop.rs:417-422`
必须复核，空则 `continue`。若 `has_pending` 改成「读出即取」，补投再取一次
就会把同一条结果送达两遍。

**表容量**：`MAX_PENDING_RESULTS = 32`（`apps/coding-agent/src/subagent.rs:85`）。超限时淘汰最旧一条
并落一条日志（`:334-341`）—— 完整转录仍在产物文件里。

## 4. `subagent_send` 通路

```
SubagentSendTool::call                          apps/coding-agent/src/subagent.rs:2092
  └─ AppSubagentRunner::send                    apps/coding-agent/src/subagent.rs:1250
       └─ SubagentHandles::send                 apps/coding-agent/src/subagent.rs:439
            ├─ Weak::upgrade 取子那份 QueueBoundarySource   :441-449
            └─ own.push(Boundary::Steer(Message{role: User})) :452-457
```

- **注册表**：`SubagentHandles`（`apps/coding-agent/src/subagent.rs:420`），
  `inner: Mutex<HashMap<String, Weak<QueueBoundarySource>>>`（`:421`）。
  登记在 `spawn` 里、**`tokio::spawn` 之前**（`:1228`）—— 晚一步则父从拿到句柄到子建边界之间
  有窗口，那条消息会落空。
- **子的消费端**：`ChildBoundary::take()` 只取自己那份（`apps/coding-agent/src/subagent.rs:153-155`），
  `is_aborted` 才透传父（`:156-158`）。
- **轮的消费端**：`drain_boundary`（`crates/ys-loop/src/basic.rs:180`）的
  `Boundary::Steer` 臂（`:193-202`）append + emit `UserMessage` + `consumed = true` ⇒ 调用点
  `continue` 进入下一轮，模型下一轮就看到它（收场前那次 drain 在 `:1134-1137`）。
- **角色是 `Role::User`**（不是 `Harness`）：子转录由 `transcript_body` 按角色名渲染，
  `Harness` 会让子转录出现「父 harness 说过话」的误导。
- **错误类别是 `ToolError::Execution`**（`SubagentSendTool::call`，`apps/coding-agent/src/subagent.rs:2092`）：
  句柄不存在或子 agent 已收尾都是**运行期状态**（`Weak` 已失效），不是参数形态错误 ⇒
  走 `Execution`，不走 `InvalidInput`。
- **fail-closed**：handle 不存在或已收尾 ⇒ `Err`（`:460-463`），正文由 `describe_live`
  （`:477-484`）给出当前有效 handle 列表；顺手 `retain` 掉失效项（`:444-447`），
  使正文里的列表就是当下真在跑的那批（`live_handles`，`:468`）。

**为什么是 `Weak` 而不是 `Arc`**：子 agent 收尾后它的 `ChildBoundary` 被 drop ⇒ 注册项
自动失效（`upgrade()` 失败即视为「已收尾」），无需手动注销；用 `Arc` 会把每个子 agent
的边界源钉到进程结束（泄漏），且要额外维护一张「何时该删」的表。

## 5. 句柄形态 `sa{turn}.{n}`

- 唯一定义：`subagent_handle`（`apps/coding-agent/src/subagent.rs:1258`），`format!("sa{turn}.{n}")`。
- 生成点：`spawn` 里取 `turn` 与 `dispatched` 两个原子（`:1221-1224`）。
- **回合段写入点**：`apps/coding-agent/src/app_loop.rs:1481-1487` —— `run_turn_once` 开头
  `*ctx.turns += 1` 之后立刻 `s.set_turn(*ctx.turns)`。两个数字同源 ⇒ 句柄的回合段与
  `Envelope.turn` 一致。
- 唯一性理由：回合内序号 `n` 每回合从 1 重来，只有 `{turn}.{n}` 合起来才全局唯一；
  否则 `subagent_send("sa3")` 会寻到上一个回合的同序号任务。
- 生命周期跨 `/resume`：`turn` 是与 `results` / `live` / `handles` 同样共享的 `Arc<AtomicU32>`
  （`apps/coding-agent/src/subagent.rs:1013`），经 `SubagentSetup::make_runner` 转递给每回合新建的
  `AppSubagentRunner`。

## 6. 明确的边界

1. **消息最大延迟 = 当前工具跑完**。插话（`Boundary::Steer`）只在轮边界被消费，任何正在跑的
   工具、任何子 agent 都**不因插话而中止** —— 只探 `is_aborted()`。详见
   `docs/design-final/steer-does-not-interrupt.md` 与 ADR-0064。
   推论：`subagent_send` 的那一刻，子 agent 可能正卡在一次长工具调用里，消息要等那次调用返回才被看到。
2. **`has_pending_steer()` 恒 `false`**（`apps/coding-agent/src/subagent.rs:166`）。它是
   `BoundarySource` 的可选探针，「是否有插话待消费」；当前**无生产消费者**
   （`crates/ys-tool/src/context.rs:12` 已注明）。子侧恒 `false`，因为插话是给子模型下一轮的输入，
   不是给工具的中止信号 —— 透传只会让父的插话表现成「孩子的命令莫名提前中止」。
3. **`-p` / `--json` 单回合模式不自动起回合**。一次性模式只有一个回合（`apps/coding-agent/src/main.rs:1952-1957`）：
   落点照绑，回合结束后才完成的结果留表、无后续回合补投（进程随即退出）。
   自动回合只存在于 TUI 的 app 循环（`Wake::SubagentReturned`）。
4. **广播语义未做**。`subagent_send` 的 `handle` 是单个字符串，一次只投一个子 agent；
   没有「发给所有在跑的子 agent」的形态。
5. **`PendingResults` 是进程级、会话无关**。同一进程内别的会话遗留的结果会投给当前会话
   （`/resume` 跨会话串台属已知缺口，未做会话归属）。
6. **两个不同的上限**。回合总**派发次数**由 `take_dispatch_slot`（`apps/coding-agent/src/subagent.rs:1344`）封顶（`DEFAULT_MAX_DISPATCHES_PER_TURN = 16`，`:37`），
   超出 ⇒ `Err`（**不排队**）；回合内**并行槽位**数另由 `max_parallel_tools` 控制，
   超出的**工具调用**在批内排队（提示词里的 “extra calls queue” 说的是后者）。

## 7. `LiveSubagents` 只是纯计数（当前无生产消费者）

`LiveSubagents`（`apps/coding-agent/src/subagent.rs:508`）—— `count()`（`:525`）与
`enter()` 返回的 RAII 守卫（`:516`，drop 时自减，`:536`）。

- **没有控制流消费者**。等待链拆除后，不再有任何代码用「在跑几个」决定收场或推进。
  `count()` 带 `#[cfg(test)]`（`:524-525`）⇒ **生产路径里连读者都没有**，唯一读者是
  `live_guard_returns_the_slot_on_drop`（`apps/coding-agent/src/subagent.rs:549`）。
  TUI 目前也不读这个数；在跑句柄的口径看 `SubagentHandles::live_handles`（`:468`），不看这个计数。
- 保留理由有两个，**都不依赖当前有消费者**：① `LiveGuard` 覆盖 `ChildTask::execute` 的
  **四条退出路径**（组装失败 / `Ok` / `Err` / future 被 drop），保证「进程级在跑数」准确，
  删掉守卫则将来任何读这个数的消费者都要重造它；② 将来做 TUI 计数 / 状态板时直接可用，
  无需重新引入计数设施。
- 与 `results` / `handles` 同样以 `Arc` 跨 `/resume` 重建共享（`apps/coding-agent/src/subagent.rs:1008`、
  `apps/coding-agent/src/assembly.rs:126`）。

## 8. 验收锚点

| 合同 | 用例 |
|---|---|
| 本回合内送达 | `apps/coding-agent/src/app_loop.rs:4386` `async_subagent_result_delivered_within_the_same_turn` |
| 跨回合不丢 | `apps/coding-agent/src/app_loop.rs:4517` `async_subagent_result_survives_turn_boundary` |
| 表语义（投递 / 留表 / 代数） | `apps/coding-agent/src/subagent.rs:2282` `pending_results_delivers_when_bound_and_queues_when_not`、`:2311` `pending_results_stale_binding_does_not_clobber_newer_one` |
| `has_pending` 与队列同口径 | `apps/coding-agent/src/subagent.rs:566` `pending_results_has_pending_matches_the_queue` |
| 唤醒无等待者时不丢 | `apps/coding-agent/src/subagent.rs:594` `pending_results_wakeup_survives_no_waiter` |
| `send` 推进子的队列 | `apps/coding-agent/src/subagent.rs:2183` `subagent_send_queues_steer_into_the_child_queue` |
| `send` fail-closed | `apps/coding-agent/src/subagent.rs:2216` `subagent_send_fails_closed_when_handle_is_unknown_or_finished` |
| 句柄形如 `sa{turn}.{n}` | `apps/coding-agent/src/subagent.rs:2121` `subagent_handle_carries_turn_and_sequence` |
| 父表 12 元 / 无 `subagent` / 前缀 | `apps/coding-agent/src/toolset.rs:624` `prefix_fingerprint_is_anchored_and_stable` |
