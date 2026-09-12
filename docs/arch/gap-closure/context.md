# YuShan 基础能力补齐（Pi 差距收敛）— 架构上下文

## 概述

以 `docs/reports/gap-analysis-pi-comparison.md` 为输入，确定「必须补齐」（Tier 1）与「高价值增强」（Tier 2 精选）工作的**落地顺序与解耦边界**。目标是补能力，不破坏 `docs/design.md` 已锁定的四条不变量（依赖单向、最小核心、四者边界、静态组合）。

**产品定位（已决策）**：YuShan 是**后台 agent**，TUI 展示保持简洁（只覆盖基本 coding 功能；pi 的展示本也不复杂）。因此：

- 事件出口（`--json` / `-p`）是流式工作的**首个消费者**，TUI 是次要消费者且从简；
- 纯 UI 花活（Markdown 渲染、Diff Viewer、Theme、鼠标、可配快捷键）**正式砍掉**，不再列为暂缓项。

**交互 REPL 入口（待决，本轮不动）**：当前交互模式用 ratatui（`tui-ratatui` feature 默认开），历史上 rustyline 已被 ratatui 替换并彻底删除（`c9a4177`→`645e339`）。后台 agent 定位下，**rustyline 可能是更合适的方向**——它能删掉 `suspend_terminal`/`resume_terminal` dance（`ui/mod.rs:97-128`）、raw-mode 下的取消路由复杂度、手写 `complete_inline` 补全，并白拿持久 history；代价是反转旧决策。**本轮先不替换**，只记录为待决方向，把精力放在后台事件出口那半（见「未澄清问题」Q7）。

### 架构模型：生产者 / 消费者（本轮定稿）

**Agent 是生产者，只产出事件；界面是消费者，只读事件。** 这是对 `design.md`「事件是一等接口……可用于 UI、日志、持久化、测试和回放」的落地形态。

```
  Agent（生产者）              信道                消费者
  ─────────────              ──────              ──────
  BasicLoop                   channel            -p      （取 Text → 打印）
    │                            │               --json  （编码 JSON → 输出）
    └── emit(Envelope) ─────────►├──────────────►ys-tui  （取事件 → 画屏）
                                 │               回放/测试（读文件 → 重现）
                         Envelope{ source, turn, event }
```

**前提认知**：**Agent 不具备交互能力**——没人给它输入，它就不运行。信道里的流量是「被驱动的产物」，不是「agent 自己产生的」。这决定了「消费者消失」的正确语义是**歇着**（结束本轮），而非死等。

**已定规则**：

1. **信道保留**，作为通用的生产者/消费者解耦机制（用户明确要求保留，供后续 TUI 优化拓展）。
2. **信道传「信封」（`Envelope`）**，不传裸事件：
   ```
   Envelope {
       source: <谁发的>     // 为多 agent 协作预留；当前只有单一值
       turn:   <哪一轮>     // 把一轮的事件归成一组
       event:  AgentEvent   // 事件本身，「信纸」不改
   }
   ```
   好处：`ys-event` 保持纯事件定义不受污染；`--json` 的**信封格式现在就能定死**，将来加多 agent 只换 `source` 的值，不构成破坏性变更。
3. **信道是有界的，容量可配**（不是无界）。理由：防 DoS（内存被撑爆）。默认值给宽裕配置，具体数值在设计阶段定。
4. **`emit` 改为异步**（推翻 ADR-0004 点 3）——见下节「推翻既有决策」。信道满了，agent **等待**，不丢弃、不崩溃。这正是 ADR-0004 想要的背压语义，只是实现路径变了。
5. **消费者消失 → agent 收摊**：等待中发现接收端已关闭（信道报错），agent **结束本轮、正常收场**（与「用户取消」同类，非故障）。无需额外检测机制——信道自己会报错。
6. **异步形态**：沿用现有 `tokio::select!` 结构（`ui/mod.rs:344-366`），turn future 与事件收取在同一任务内交错，不 `spawn`（`run_turn` 需要 `&mut Agent`，不能搬到后台线程）。

### 推翻既有决策：`EventSink::emit` 从同步改为异步

**背景**：ADR-0004 点 3 明确把 `EventSink::emit` 定为**同步** `fn`，理由是「热路径零 Future 装箱、慢 sink 天然背压」。

**问题（本会话核实）**：同步 `emit` **无法与有界异步信道共存**。

- `EventSink::emit` 是同步的（`crates/agent-event/src/sink.rs:6`）
- `BasicLoop::run_turn` 是 async，跑在 `#[tokio::main]` 里
- 全仓**没有任何 channel**（`mpsc`/`Sender` 零命中）

在 async 运行时里，同步函数要「等待一个有界信道腾出空位」只有两条路：`blocking_send`（**在 runtime 内会 panic**）或 `try_send`（满了**丢事件**，违反 ADR-0004 点 5「每个 run 恰好一个终局事件」）。**两条都不可接受。**

**决策**：`emit` 改为 async，agent 在信道满时**异步等待**——这正是需求要的背压。

**代价评估**：ADR-0004 当年拒绝异步的理由是「热路径零装箱」。**该理由已不成立**——中间本来就要走信道，这层开销跑不掉。其余设计（双事件口、终局事件不变式、协作取消）**均不受影响**。

**待办**：起草 ADR 记录此修订（「难以逆转 + 出乎意料 + 真实权衡」三条俱备）。

### 消息模型：agent 自己转（本轮定稿）

**Agent 不交互，只消费输入**：没人给它输入就不运行。据此，agent 从「被调用的库」变为**自己转的循环**（演员模型）——没消息就休眠，有消息就起来干活。

```
         ┌──────────────┐
  入 ───►│  inbound     │───► Agent（自己转：取消息 → 跑 → 再取）
         │  队列        │
         └──────────────┘
         ┌──────────────┐
  出 ◄───│  outbound    │◄─── Agent 报事件
         │  队列        │
         └──────────────┘
```

**四类流入，各走各的**：

| 东西 | 走哪 | 形态 |
|---|---|---|
| 用户说的话 | **入站队列** | 推（自然语言，**只有自然语言**） |
| provider / model / 凭证 | **agent 自取** | 拉（取用凭据） |
| 重置会话（`/new`） | **换队列**（agent 不知情） | — |
| Esc / Ctrl-C 打断 | **`CancelToken`** | 信号（即时，非消息） |

**关键决策**：

1. **入站分两条队列**（学 pi 的语义区分，而非我们早先的单队列）：
   - **`steering`** —— "agent 正在跑，我插句话"（中途掌舵）
   - **`followUp`** —— "等它跑完再说"（排队等下一趟）
   两条队列让消费端明确表达意图，且**清空策略可各自不同**。
2. **清空粒度可配**（学 pi 的 `QueueMode`）：`All`（一次全灌）/ `OneAtATime`（一条一条）。用户上轮提的"同来源合并处理"即 `All` 模式。
3. **入站队列只装自然语言**。配置不进消息（"和 agent 约定好，它自己需要就自取"）；控制指令也不进（不设"控制消息"协议）。
4. **`/new` 与 agent 无关**——它是**换个队列**。agent 从头到尾不知道发生过 `/new`。旧队列自行保存或丢弃。
5. **队列 = 会话 = 历史**（一个东西，不是两个）。队列既是"待处理消息"，也是"历史日志"。"保存"就是存成 JSONL（`JsonlSession` 天然就是这个可保存的日志）。
   **此处刻意不学 pi**：pi 的 `followUpQueue` 只是 agent 内部"待处理"，历史另存 `session-backends`，二者分离。我们的统一形态让 `/new` 只需换一个东西。
6. **一队列一 agent**。多 agent = **N 个 (agent, 队列) 对 + 路由器**，**不是**共享队列、**也不是**一个 agent 看多个队列。
7. **中途收消息：每轮（Round）边界查队列**，不是每回合。用户可在 agent 干活中途插话纠正。
8. **模型在回合开始时取一次**（不每轮重取）——避免一个回合被拆成两个模型跑，导致工具调用配对错乱。
9. **Esc 只中断当前回合，不清空队列**（排队的话仍要处理）。
10. **`run_turn()` 的「一次调用 = 一回合」接口退休**，换成"自己转"的循环。

### 生命周期：可配策略（绑定消费者 / 无消费者续跑）

**pi 的形态**（`agent/src/agent-loop.ts`）：`agentLoop()` 内部 `void runAgentLoop(...)` 自己往下跑，但**返回一个事件流**；**流的生命周期挂在调用方的 `await` 上**——没人 await，agent 自然结束。

**我们采纳这一条，但它不是唯一形态**——不同产品形态需要不同策略：

```rust
enum LifecyclePolicy {
    /// 消费者消失 → 干完当前轮就收摊（交互式默认）
    StopWhenConsumerGone,
    /// 消费者消失 → 继续跑（后台长任务）
    ContinueWithoutConsumer,
}
```

| 产品形态 | 默认策略 |
|---|---|
| 交互式 CLI / `-p` | `StopWhenConsumerGone` |
| 后台长任务（投资调研等） | `ContinueWithoutConsumer` |

**关键规则（学 pi-server）**：**消费者消失时，不打断正在进行的轮**——干完当前轮再看策略。pi-server 的原话是 *"releases its attachment only after admitted service calls settle"*。

- 交互式：干完这轮 → 停
- 长任务：干完这轮 → 继续

**同一条规则、不同策略**，比"立刻收摊"更稳（不会把正在写的文件砍一半）。

**`ContinueWithoutConsumer` 的配套要求**：此时事件无人消费，必须靠**会话日志兜底**（事件落盘 JSONL，回来再读）。当前 `JsonlSession` 只存消息、不存事件——需扩展。

**收益**（对 `StopWhenConsumerGone`）：早先那条「收信人消失 → 专门检测」的逻辑不需要了——pi 的形态天然就是这样。

**不做的事**：**「关掉笔记本还能继续跑」需要 daemon 化**（launchd 常驻 + 断点续跑），是产品形态的转变，与"拆进程"同级，**等真需要再说**。

**技术核实（中转收消息的 role 冲突已排除）**：回合中途 session 末尾是工具结果（内部标 `User`），插入用户消息看似产生"连续两条 User"。但 `translate_request`（`lib.rs:99-166`）把工具结果翻成 `role="tool"`、新消息翻成 `role="user"`，实际发出去的序列是 `assistant → tool → tool → user`——**合法**，无需补丁。

**`max_rounds` 必须可配**：当前默认 **10**（`crates/agent-component/src/limits.rs:25`），且 `main.rs` 从未覆盖。10 轮是"单次任务"的安全帽，**长任务轻松跑几十上百轮**，远不够用。

**`bash_timeout` 默认 `None` 对长任务是隐患**：长任务里一条挂住的命令会让 agent 永远卡住，而且没人看着。目前 `basic.rs:238` 有个硬编码的 `unwrap_or(300)` 兜底，应改为显式配置。

**连带影响（待处理）**：

- `Agent::clear_session()` **消失**——"清空会话"这个动作不存在了（换队列代替）。
- **ADR-0007**（`tool_names`/`context_window`/`cancel`）与 **ADR-0008**（`cancel_handle`）**大半被架空**——这些是"外面伸手进 agent"的接口，演员模型下应由「出站事件」与「自取凭据」取代。需评估后修订或废弃。
- **F8/F9 的别扭自动消解**：`JsonlSession::open` 是 async 而 `AgentBuilder` 是同步的问题，变成"**消费者开 session**"后不复存在。
- `/model`、`/logout` 等命令不再直接改 agent 状态；`Config::build_model()` 从"外面造好塞进来"变为"**agent 按凭据自取**"。

### 群聊（多 agent 阶段课题，本轮不设计）

**`Envelope.source` 是群聊的地基**——不标来源就不知道谁在说话。本轮不做，但设计不拦它。

**核心难点（留档备查）**：

1. **同一消息对不同 agent 的角色不同**：A 说的话，在 A 自己历史里是 `assistant`，在 B/C 眼里是 `user`。路由器必须**按视角重写角色**，不是简单转发。
2. **发言权与终止**：需定规则（轮流 / 主持人 / 点名 / 最多 N 轮兜底），否则无限循环。
3. **形态**：共享**带 `source` 的原始日志** + **每个 agent 各自投影**（一份日志、N 份视图），优于扇出 N 份拷贝。注意 ≥3 个 agent 时非本人发言会出现连续 `user`，处理方式同"合并"。
4. **每个 agent 仍是一队列一 agent**——群聊时那"一队列"是从共享日志投影出来的视图。

**对应 `design.md` Subagent（Phase 3，报告标 Medium/Hard）**：群聊属这一档，路由器住接线层，不进 `ys-channel` 契约。

### Pi 对照（`tmp/pi`，已验证实现）

pi **已经实现了本轮推导出的模型**，是方向正确的活证据。逐项对照：

| 维度 | pi（实际实现） | YuShan（本轮设计） | 结论 |
|---|---|---|---|
| 入站 | **两条队列**：`steeringQueue`（跑动中插话）+ `followUpQueue`（跑完排队） | 原为单队列 → **已改为两条** | **学 pi** |
| 清空粒度 | `QueueMode = "all" \| "one-at-a-time"` | 原未定 → **已定为可配** | **学 pi** |
| 队列与历史 | 队列只管"待处理"，历史另存 `session-backends` | **队列 = 会话 = 历史** | **刻意不学** |
| 出站 | `EventStream`（AsyncIterable）+ listener Set | 有界信道 + `Envelope` | 各自实现，语义等价 |
| 打断 | `AbortController` / `AbortSignal` | `CancelToken` | 等价（Rust 侧 std 原语，见 ADR-0002） |
| 生命周期 | 循环在内部跑，**挂在调用方 `await` 上** | 原为"独立 actor" → **已改为绑定消费者** | **学 pi**（省掉收摊检测） |
| 界面边界 | `pi-tui` 不认识 agent，靠**拆进程** | `ys-tui` 不认识 runtime，靠 **crate 边界** | 同目标、不同手段 |
| 组装 | TS 对象图 | 静态组合 + `AgentBuilder` | 各自实现 |

**学 pi 的三条**：入站两队列、`QueueMode` 可配、生命周期策略（见上节）。

**澄清「队列 = 会话 = 历史」的准确含义**（早先表述过于笼统，易被误解为"队列对象自己就是存储"）：

> **一个会话 = 一条消息日志（可落 JSONL）+ 一个「处理到哪了」的游标。**
> 「队列」是这条日志里**游标之后**那一段的**视图**，不是独立的容器。

pi 的三层（`agent._state.messages` 工作上下文 / `steeringQueue`+`followUpQueue` 瞬时暂存 / `session-backends/sqlite` 持久化）**我们并没有丢**——对应的正是：**当前加载的 JSONL**（工作上下文）/ **游标**（待处理）/ **JSONL 文件**（持久化）。差别只在记法：**我们的记法少一个概念**，不必同时维护"队列"和"历史"两个容器。

**同时必须守住边界**：待处理消息与已落盘历史若混成一个对象、不复用游标概念，持久化边界就会模糊。上面的准确表述避免了这一点。

### 第二个产品形态：投资调研 Agent（设计不拦，本轮不做）

`docs/arch/etf-display-tool/context.md` 已记录该场景。**它是第二个产品，不是 coding agent 的一个模式**：

| | Coding Agent | 投资调研 Agent |
|---|---|---|
| 工具集 | read/write/edit/bash | `adapters/tools-finance`（MarketData / Indicator / Display） |
| 产物 | 改了哪些文件 | **HTML 报告**（`~/.yushan/finance/reports/`） |
| 交互 | 你一句我一句 | **丢一个任务跑很久** |
| 验收 | 看 diff | 打开浏览器看报告 |
| 生命周期 | `StopWhenConsumerGone` | **`ContinueWithoutConsumer`** |

**这恰是 `design.md` 的"独立组合"模式**：同一个通用 Runtime，两套工具 + 两套 prompt = 两个产品。`AgentBuilder` 已支持——`main.rs` 换掉几个 `.tool(...)` 即可。**不需要为它改 Runtime**，验证了架构方向正确。

**Runtime 需补的能力（设计不拦，但要记）**：

1. **生命周期策略**（`ContinueWithoutConsumer`）——见上节
2. **`max_rounds` 可配**——长任务远超默认 10 轮
3. **事件落盘**——无人消费时事件须进 JSONL；且**中断后能回看发生了什么**
4. **工具按领域分 crate**——`ys-tools-basic` / `ys-tools-finance`
5. **产物事件的显式表达**——投资 agent 的现状是让 agent 用自然语言说"报告已生成: /path/..."（见 etf-display-tool 的「关键约束：ToolResult.content 是扁平字符串，HTML 不进入消息流」）。长任务下这条**不可靠**（可能漏、可能写错路径），宜有 `ReportGenerated { path }` 之类的事件让消费者直接拿到产物路径。

**本轮不做**：投资 agent 等 Runtime 成熟后再组装；这里只确保设计不拦它。

**pi 的组装层参考**：`packages/protocol`（RPC 协议 + CBOR 编解码）↔ `ys-channel`；`packages/tui`（零 agent 依赖）↔ `ys-tui`；`packages/coding-agent`（总装）↔ `ys-coding-agent`。

### 存储：pi 两种都用，主线是 JSONL

**结论先行**：pi 的 coding-agent **主线用的是 JSONL**，不是 SQLite。

| pi 的组件 | 存储 | 场景 |
|---|---|---|
| `packages/coding-agent`（主线） | **JSONL** —— `~/.pi/sessions/*.jsonl`，一行一 JSON | 单机 CLI |
| `packages/session-backends/sqlite-node` | **SQLite**（`node:sqlite`） | 多客户端并存 |
| `packages/server` + `pi-client` | 服务端 | **实验性**（README 首行 `Experimental`；coding-agent 仅在 `src/experimental/` 下使用） |

**服务端场景很具体**：多个界面（TUI + 网页 + IDE）**并发附着同一个会话**，需要数据库级并发控制。传输层**只有 unix socket**（本地进程间，非远程服务）；键词是 *"A Session may have multiple presentation attachments"*。

**对 YuShan**：

- **定位是"单一前端 + 后台长任务"** → **JSONL 足够**，SQLite 不需要
- 印证"拆进程等前端变多再说"——**pi 是到四个前端才拆的**，且拆得很重（worker 进程管理、attach/detach、路由、超时回收）
- `Session` trait 已是可换实现（`JsonlSession` / `MemorySession`），**与 pi 用独立 package 换后端同思路**

**可直接抄的细节**：pi 的会话文件名 `{fileTimestamp}_{sessionId}.jsonl`——时间戳前缀让 `ls` 出来天然按时间排序，即会话列表。

**留意**：pi 的 `session-manager.ts` 已膨胀出 `CURRENT_SESSION_VERSION = 3`（版本迁移）、`parentSession`（分支）、复杂文件读写。**这说明 JSONL 到后期会长东西**——印证了报告里"Session 树做独立 adapter crate"的判断，砍它没错。

### 新增 crate：`ys-channel`（契约层）

**装契约，不装实现。** 与 `ys-event` 平级，都只依赖 `ys-core`。

| crate | 装什么 | 已有？ |
|---|---|---|
| `ys-event` | 出了什么事（`AgentEvent`）—— 只读观察 | ✅ |
| **`ys-channel`**（新） | 收发契约：`Envelope`、`Source`、入站消息类型、收发接口 | ❌ 需新建 |

**为什么必须是独立 crate**：`CONTEXT.md` 定义"**事件 = 已经发生之事的记录，只可观察**"，而**发给 agent 的消息是请求**（想让它发生）——二者是不同概念，不能同 crate。且 `ys-tui` 要认信封但**不许依赖 runtime**，信封类型必须在它下面。

**为什么不装实现**：`tokio::mpsc` 的实现放接线器（`ChannelSink`）。理由是**契约不该指定传输**——若把 `tokio::sync::mpsc` 焊进 `ys-channel`，就等于声明该信道只能是 tokio 的；将来换传输（测试用同步实现、别的 runtime）就得改契约。纯数据 + 纯枚举的契约天然与传输无关。

（**更正**：早先此处写作「避免 `ys-loop` 间接拖进 tokio，违反 `ys-core` 零 Tokio」——**该理由不成立**，因为 `ys-loop` 自 v1 起已直接依赖 `tokio`（`tokio::time::timeout`，`basic.rs:239`），`ys-session` 亦然（`tokio::fs`）。详见 ADR-0012。）

### 界面 crate 边界（本轮定稿）

**`ys-tui` 做成纯渲染消费者，不认识 `ys-runtime`。** 这一点由参考项目 pi 验证可行——`pi-tui` 的依赖列表里**根本没有 agent**（只有 `marked` + `get-east-asian-width`）。

**边界切法（看 ↔ 开）**：

- **"看"** → `ys-tui`：只认 `AppView`（只读快照）、`AgentEvent`（事件）、自身 UI 状态。
- **"开"** → 留在 `ys-coding-agent`（接线器）：持有 `Agent`/`Config`，真正调 `run_turn()`。

**当前耦合点**（已核对代码）：整条脏边界压在 **`ui/mod.rs` 一个文件**——它直接持 `&mut Agent`(L30)、调 `agent.run_turn()`(L344)、`agent.cancel_handle()`(L61,286)、`&mut Config`/`StateStore`/`CommandRegistry`。`ui/app.rs`（纯状态）、`ui/draw.rs`（纯渲染）、`ui/events.rs`（纯输入）三个文件**已经干净**。

**两个卡点**（抽取时需解决）：

1. `AppView::from_sources` 的参数含 `ProviderRegistry`/`StateStore`/`TurnStats`（`view.rs:67-73`），快照不干净。
2. `ui/mod.rs` 亲自"开车"，需改为"喊意图"、由接线器驱动。

**本轮决定**：**边界规矩现在守，crate 等流式落地后再切**（避免两个大改同时合入）。守则：① 界面代码永不碰 `Agent`；② 界面要 agent 干活只喊意图。

### 命名方案：`ys-` 前缀（已批准，待执行）

| 层 | 现名 | 新名 |
|----|------|------|
| 核心 | `agent-core` … `agent-runtime` | `ys-core` `ys-event` `ys-model` `ys-tool` `ys-session` `ys-component` `ys-loop` `ys-runtime` |
| 信道契约（新） | — | `ys-channel` |
| 适配器 | `agent-model-openai-compatible` | `ys-model-openai-compat` |
| | `agent-tools-basic` | `ys-tools-basic` |
| 界面（新） | — | `ys-tui` |
| 接线器 | `yushan-coding-agent` | `ys-coding-agent` |

采用**方案 A**：包名与 `use` 标识符**全改**（`agent_core` → `ys_core`），不加 `[lib] name` 覆盖。

### 参考项目调研结论（pi / deepseek-harness）

| 维度 | pi | deepseek-harness | YuShan 取舍 |
|------|----|------------------|------------|
| 界面 crate 不认识 agent | ✅（靠**真的拆进程**：`pi-tui`/`pi-client`/`pi-protocol`/`pi-server`） | ✅（靠 60+ 小包边界） | ✅ 学，但用**进程内 channel** 实现 |
| 事件/协议是唯一契约 | ✅ `pi-protocol`（RPC 协议，v8，CBOR 编解码） | ✅ schema 包（`schemastery`/`zod`） | ✅ 即 `ys-event` |
| bundle 组合 | ✅ `pi-coding-agent` | ✅ `packages/bundle/*` | ✅ 已有（`AgentBuilder`） |
| DI 插件框架 | — | ✅ cordis（热插拔/HMR） | ❌ 不做（`design.md` Phase 4） |
| 真的拆进程 | ✅（为 TUI+web+desktop+RPC 四前端共用后端） | ✅ | ⏸ 等前端变多再说 |

**关键对应关系**：`pi-tui` ↔ `ys-tui`；`pi-client` ↔ 接线器里的胶水；**`pi-protocol` ↔ `ys-event`**；`pi-server` ↔ 持有 Agent 的那一半。**将来若拆进程，只需在 `ys-event` 上加编解码层，界面代码不动。**

本文件不逐条复述报告，只回答三个问题：

1. 每一项工作**动哪一层**、**不动哪一层**；
2. 哪些工作是**解耦枢纽**（先做能解锁其他项）；
3. 哪些边界必须先定，否则会埋下耦合债。

---

## 现有架构（关键事实，均已核对代码）

### 模块边界

```
agent-core        Message/ContentBlock/ToolCall/ToolResult/Usage/StopReason/CancelToken
agent-model       Model trait + ModelEvent{TextDelta} + ModelEventSink + ModelRequest/Response
agent-event       AgentEvent + EventSink（同步 push trait）+ Noop/Collecting
agent-session     Session trait + MemorySession + JsonlSession（已完整实现）
agent-component   RuntimeContext + RunLimits
agent-loop        AgentLoop + BasicLoop（含 compact_session 私有实现 + Forwarder 桥接）
agent-runtime     AgentBuilder + Agent（set_model 热替换、clear_session）
adapters/          model-openai-compatible（stream 解析已写未接）、tools-basic（4 工具）
apps/coding-agent config/provider/commands/ui/view/state
```

### 与本次工作强相关的现状事实

| # | 事实 | 位置 |
|---|------|------|
| F1 | `ModelEvent` 只有 `TextDelta`，无 `ToolCallDelta` / `ThinkingDelta` | `crates/agent-model/src/event.rs:8` |
| F2 | SSE 解析 `parse_sse_stream` **返回缓冲的 `Vec`**，且**从未被调用**；请求硬编码 `stream: Some(false)` | `adapters/model-openai-compatible/src/stream.rs:16`、`lib.rs:192` |
| F3 | 流式响应类型 `ChunkToolCall`/`ChunkFunction` **已存在**（工具增量解析数据结构就绪） | `adapters/model-openai-compatible/src/response.rs:49-63` |
| F4 | `Forwarder` 只把 `TextDelta` 映射到 `AgentEvent::ModelTextDelta`，其余 `_ => Ok(())` | `crates/agent-loop/src/basic.rs:24-31` |
| F5 | `AgentEvent` 只有 `ModelTextDelta` 一个模型增量事件 | `crates/agent-event/src/event.rs:11` |
| F6 | **生产环境挂的是 `NoopEventSink`** —— 即使发出 `ModelTextDelta` 也被丢弃 | `apps/coding-agent/src/main.rs:139` |
| F7 | TUI turn 期间只画「Working 动画」，turn 结束后一次性追加 `final_message`；不消费增量事件 | `apps/coding-agent/src/ui/mod.rs:290-316`、`draw.rs:176-179` |
| F8 | `main.rs` 硬编码 `MemorySession::new()`；`JsonlSession` 已完整实现（原子写 + 损坏行跳过） | `main.rs:138`、`crates/agent-session/src/jsonl.rs` |
| F9 | `JsonlSession::open` 是 **async**，而 `AgentBuilder.session()` 是同步链式调用 | `jsonl.rs:14`、`builder.rs:52` |
| F10 | `model_factory` 是**单一闭包**，永远构造 `OpenAICompatibleModel`；新增 Anthropic 意味着闭包必须按 provider 分支 | `main.rs:72-86`、`config.rs:9,50-66` |
| F11 | `ProviderCompat` 只有 `has_reasoning_content` / `tool_calls_as_text` 两个布尔 | `compat.rs:1-27` |
| F12 | `ModelRequest` 无 thinking 参数 | `crates/agent-model/src/request.rs:7` |
| F13 | `BasicLoop::compact_session` **已做真摘要**（`generate_summary` 调模型）；但 `/compact` 命令只调 `clear_session()`，二者没接上 | `basic.rs:323-388`、`commands/builtin.rs:528-534` |
| F14 | `RuntimeContext` 无 hooks 字段；HookDispatcher 零代码 | `crates/agent-component/src/context.rs` |
| F15 | 工具仅 4 个：read/write/edit/bash | `adapters/tools-basic/src/lib.rs` |
| F16 | TUI 对 assistant 文本是**纯文本单行**渲染，无 Markdown/代码高亮/diff（本轮明确不做） | `apps/coding-agent/src/ui/draw.rs:176-179` |
| F17 | `EventSink::emit` 是**同步** push 接口（ADR-0004 点 3）；**全仓无任何 channel**（`mpsc`/`Sender` 零命中） | `crates/agent-event/src/sink.rs:6` |

### 外部依赖

| 依赖 | 用途 | 备注 |
|------|------|------|
| `reqwest` | HTTP 客户端（model-openai-compatible、provider） | 已锁 |
| `futures::StreamExt` | SSE 流消费 | stream.rs 已用 |
| `ratatui` + `crossterm` | TUI | `tui-ratatui` feature 默认开 |
| `tokio` | 运行时 | 全链 async |
| `serde` / `serde_json` | 序列化 | core 层已依赖 |
| `inquire` | slash 命令交互式选择器 | 仅 coding-agent |

---

## 约束

- **技术**：`Model` trait **不改**（`complete() + &mut dyn ModelEventSink` 已是流式就绪）；流式是适配器实现细节。新参数加到 `ModelRequest`，不加到 `Model`。
- **依赖单向**：**纯契约层**（`ys-core`/`ys-event`/`ys-channel`/`ys-component`）**不依赖 Tokio**/HTTP/TUI；执行层（`ys-session`/`ys-loop`/`ys-runtime`）可直接用 tokio（`ys-session` 用 `tokio::fs`、`ys-loop` 用 `tokio::time`——见 ADR-0012）。任何 UI 逻辑不进 core/loop。
- **事件是一等接口**：UI、持久化、日志都走事件流，UI 不直接读写运行时可变对象（coding-agent 已用 `AppView` 快照隔离，本约束延续到增量事件）。
- **最小核心**：Session 文件管理、Markdown 渲染、provider 目录都是 coding-agent / adapter 层，不进通用核心。
- **演进**：`ModelEvent`/`AgentEvent` 均已 `#[non_exhaustive]`，新增变体是向后兼容扩展，不破坏既有 match（F4 的 `_ =>` 兜底正好利用这一点）。
- **组织**：单人维护节奏，优先「低耦合、可独立验证」的切分，每项落地都应对得上 `design.md §12` 测试矩阵。

---

## 需求范围

### 范围内（必须 = Tier 1 全量 + 高价值 = Tier 2 精选）

**Tier 1（必须）**：
1. 流式模型响应（文本 + 工具增量 + 思考增量）
2. `JsonlSession` 接入生产 + `/new` 建新会话文件
3. 主流 Provider 接入（OpenAI 零成本、Anthropic 新适配器、Google 走 OpenAI 兼容端点）
4. Thinking/Reasoning Level 控制
5. 上下文压缩闭环（把已实现但悬空的 `compact_session` 接到 `/compact`）

**Tier 2（高价值，本轮纳入）**：
- 2.1 更多工具（grep/glob/git）——复用 `Tool` trait，零核心改动
- 2.4 Hook 系统——**解耦枢纽**，见下文
- 2.5 分层配置（`settings.json` 全局 < 项目）

**Tier 2 暂缓（本轮不纳入，见「范围外」）**：2.6 Session 树分支。

### 范围外（明确不做，本轮）

- **Markdown 渲染 / Diff Viewer / Theme / 鼠标事件 / 可配快捷键**：纯 UI 花活。TUI 定位为「简洁 coding 界面」，这些不纳入；不影响后台 agent 能力。
- **Session 树分支**：独立 adapter crate（`session-jsonl-tree`），不修改现有 `JsonlSession`；本轮只做平面 `JsonlSession` 接入，树结构留给后续。
- **动态插件系统 / Client-Server RPC / Lanes**：报告 Tier 4 已明确跳过，符合静态组合哲学。
- **安全/沙箱/审批**：设计文档「不做安全」，`ToolRegistry` 仍只按名查找。

### 关键场景

- **S1 流式文本（后台事件出口）**：用户 `-p "task"` 或 `--json` → turn 期间逐 token 增量输出 assistant 文本 → 结束时终态与增量拼接一致（对齐测试矩阵「增量拼接与终态消息一致」）。TUI 仅做简洁显示（Working 动画 + turn 结束一次性显示，维持现状）。
- **S2 流式工具调用**：模型先出文本、再出 tool_calls → 增量组装工具参数 → 进入工具执行 → 结果回喂。事件出口逐个发出 `ToolCall`/`ToolResult`。
- **S3 会话恢复**：启动时 `JsonlSession::open` 恢复上次对话；`/new` 生成新 id 的新文件，旧文件保留在 `~/.yushan/sessions/`。
- **S4 多 Provider 切换**：`/login anthropic` 与 `/login openai` 走不同适配器，但 `Model` trait 统一；`/model` 热替换不重建会话。
- **S5 Thinking 控制**：`/thinking high` 设置思考级别 → 适配器翻译为 provider 特有参数 → 无思考参数的 provider 忽略（优雅降级）。
- **S6 主动压缩**：用户 `/compact` → 走 `compact_session` 真摘要路径（而非清空）→ 摘要以系统上下文注入。
- **S7 Hook 注入**：`before_model_request` 注入项目上下文/Coding Prompt；`before_tool_call` 更新操作状态。上下文注入不写进通用 Runtime。
- **S8 后台长任务**（第二产品形态）：用户 `-p "调研 510300"` 启动 → 关掉界面 → agent 继续跑（`ContinueWithoutConsumer`）→ 产物落盘（HTML 报告）→ 回来后从会话日志回看过程。**本轮不做，但设计不拦**（见「第二个产品形态」）。

### 产品定位（本轮澄清）

**YuShan 支持两种产品形态，共用同一个 Runtime**：

| | 形态 A：交互式 Coding | 形态 B：后台长任务（投资调研等） |
|---|---|---|
| 前端 | 单一（TUI / `-p` / `--json`） | 可能无前端 |
| 时长 | 短（分钟级） | 长（时级，几十上百轮） |
| 生命周期 | `StopWhenConsumerGone` | `ContinueWithoutConsumer` |
| 存储 | JSONL | JSONL（+ 事件落盘） |
| 产物 | 文件改动 | HTML 报告等 |

**共同点**：都**不需要**拆进程、**不需要** SQLite、**不需要**多客户端附着。"服务端"场景（多界面并发看同一会话）**不在定位内**。

---

## 实现与解耦策略（核心）

### 解耦枢纽 1：事件通道先通（F6 是流式的真正瓶颈，首个消费者是事件出口）

流式渲染**不是**「适配器发不发 SSE」的问题——适配器侧 `parse_sse_stream` 早已就绪（F3），`Model` trait 也早已流式就绪。真正的断点在：

- 生产挂 `NoopEventSink`（F6）→ 增量事件发出来就被丢；
- 现有消费者（TUI）turn 期间只 poll 键鼠事件、不消费模型事件（F7），且「后台 agent」定位下 TUI 不是重点。

**解耦做法**：

1. **`EventSink::emit` 改为 async**（见「推翻既有决策」）——这是前提，否则有界信道无法接入。
2. 新增 **channel 适配器 EventSink**：有界 `tokio::sync::mpsc`，容量可配，`emit` 异步投递 `Envelope`。**契约（`Envelope`/`Source`/接口）在 `ys-channel`；`mpsc` 实现在接线器**（见「新增 crate：`ys-channel`」），**不进 core/loop**。
3. 用它替换 `main.rs` 的 `NoopEventSink`（F6），使所有事件出口共享同一信道。
4. **首个消费者 = 事件出口**：`--json` 逐事件序列化输出（机器接口）；`-p` 边生成边打印文本增量（人/管道消费）。二者复用同一信道，边际成本低。
5. TUI 保持现状（Working 动画 + turn 结束一次性显示），**本轮不升级为增量渲染**；若将来需要，信道已就位——把 `NoopEventSink` 也换成接收端，并在 `run_turn_with_ticks` 的 select 里加一路收信即可。
6. **`main.rs` 分流重构**：当前是「先建 agent（挂 `NoopEventSink`）→ 再分支 `-p`/交互」，需改为「**先定模式 → 选对应消费者 → 再建 agent**」。

**为什么放第一**：它是后台 agent 的标准事件出口，服务 print / JSON / 日志 / 测试 / 回放所有消费者，且不触碰任何核心接口，风险最低、可独立验证。

**注**：`-p` 流式需要新增「文本增量逐块打印」路径（当前 `-p` 是 turn 结束后一次性 `println!`，见 `main.rs:161-168`）；`--json` 模式当前未实现，需新增。

### 解耦枢纽 2：Hook 系统（2.4）是多项功能的公共底座

design.md §6 已把 Hook 规范完整（Observer/Transform/Control + 8 个 hook 点）。观察本次范围，多项工作如果各自在 `BasicLoop` 里加分支会互相纠缠：

- Thinking 的 `reasoning_effort` 翻译 → 可作 `before_model_request` Transform；
- 上下文压缩注入 → 可作 `before_model_request` Transform（对齐 design.md「上下文通过 before_model_request Hook 注入」）；
- Coding Prompt / 项目上下文注入（design.md §10）→ 同上；
- 工具调用 UI 状态更新 → `before_tool_call` / `after_tool_result`。

**解耦做法**：`HookDispatcher` 放 `agent-component`（`RuntimeContext` 增加 `hooks` 字段，F14 位置），**`BasicLoop` 只留分发点**，不感知 hook 来自静态 crate 还是动态库。先做 `before_model_request` 一个 hook 点的 Observer/Transform，作为「最小可验证切片」，其余 hook 点按需补齐。

**风险提示**：Hook 是横切改动，若与 Tier 1 并行会放大 surface。建议**流式（枢纽 1）与 Hook（枢纽 2）分两条独立切**，各自可单独合并、单独测试。

### Tier 1 逐项落点与「动哪层/不动哪层」

**1.1 流式**（依赖枢纽 1）：
- **前置**：`EventSink::emit` 改 async（见「推翻既有决策」）。
- 动 `agent-event`：新增 `Envelope { source, turn, event }`；`AgentEvent` 增 `ModelThinkingDelta`（F5）。**注意**：`Envelope` 放哪一层需定——放 `ys-event` 会让「事件 crate」多一个协作概念；放消费者侧（coding-agent）则 `ys-event` 保持纯净。倾向后者。
- 动 `agent-model/event.rs`：`ModelEvent` 增 `ToolCallDelta{index,id?,name?,arguments_delta}`、`ThinkingDelta{text}`（F1）。**工具参数不冒泡到 `AgentEvent`**（见「已定」流式粒度）。
- 动 `model-openai-compatible`：`stream: Some(true)` + 把 `parse_sse_stream` 从「缓冲 Vec」改为「边解析边 push 到 sink」（F2/F3）。**不动 `Model` trait**。
- 动 `basic.rs::Forwarder`：补映射新变体（F4，利用 `#[non_exhaustive]` 兜底做增量工具参数组装）。
- 动 `main.rs`：`-p` 改为边生成边打印；新增 `--json` 逐事件序列化输出（首个消费者，见枢纽 1）。**TUI 本轮不改**（维持 Working 动画 + 一次性显示）。

**1.2 JsonlSession**（低风险）：
- 动 `main.rs`：`JsonlSession::open(...).await` 替换 `MemorySession::new()`（F8/F9，注意 open 是 async，需在 `build()` 前 await）。
- 动 `commands/builtin.rs` `/new`：生成新 session id + 新文件，替换当前 `clear_session()` 语义。
- **不动** `agent-session` 本体（`JsonlSession` 已实现完毕）；session 目录/id 管理是 coding-agent 产品层职责，不进 core。

**1.3 Provider**（解耦重点，F10/F11）：
- **引入 adapter 分发**：把「provider 名 → Model 构造」从单一闭包解耦。OpenAI 零成本进 `ProviderRegistry`（api_base 常量）；Anthropic 新建 `adapters/model-anthropic/`（独立 crate，实现 `Model` trait，含自有 SSE 事件类型翻译）；Google 用 OpenAI 兼容端点（零新适配器）。
- `ProviderCompat` 扩展为**结构化的 provider 差异描述**（thinking 参数名、cache_control 支持、流式事件形状），替代继续堆布尔（F11）。
- 建议方向（供决策）：`config.model_factory` 从「单闭包」改为「provider → adapter 构造器」的映射，或引入最小 `ModelAdapter` 门面。**不动 `Model` trait**。
- 数据驱动（报告架构建议 #5，provider 目录从 TOML/JSON 加载）**本轮不做**，仅预留方向——它属于纯配置层重构，与能力补全可解耦。

**1.4 Thinking**：
- 动 `agent-model/request.rs`：`ModelRequest` 增 `thinking_level: Option<ThinkingLevel>`（F12），枚举 `None/Minimal/Low/Medium/High/XHigh/Max`。
- 各适配器翻译（DeepSeek→`reasoning_effort`，Anthropic→`thinking.budget_tokens`），无支持者静默忽略。
- 动 `commands`：新增 `/thinking`。
- **不动 `Model`**；参数进请求层，保持适配器向后兼容。

**1.5 压缩闭环**：
- 核心逻辑已存在（F13），缺的是「从命令层触达」的入口。两条路二选一（见「未澄清问题」）：
  - (a) 给 `Agent`/`AgentLoop` 暴露 `compact()` 方法，`/compact` 调它；
  - (b) 依赖枢纽 2，把压缩做成 `before_model_request` Hook，`/compact` 只置标志。
- 优先 (b) 以复用 Hook 底座、避免在命令层复刻压缩逻辑；但 (b) 受枢纽 2 进度约束。

### Tier 2 精选落点

- **2.1 grep/glob/git**：新增 `adapters/tools-basic` 内三个 `Tool` 实现，复用 `Tool` trait + `ToolContext`（F15）。零核心改动，最高性价比。
- **2.4 Hook**：见枢纽 2。
- **2.5 分层配置**：`~/.yushan/settings.json` < `.yushan/settings.json`。建议做成 coding-agent 层独立的 `SettingsStore`，与 `ProviderRegistry`/`StateStore` 平级，不耦合进 core。与 provider 数据驱动（1.3 的预留项）共用同一加载器更佳，可合并设计。

---

## 未澄清问题

**已定（本轮）**：

- **顺序**：串行——先做「事件信道 + `-p`/`--json` 流式」，Hook 与 `/compact` 接线作为第二刀独立后合。
- **流式粒度**：`ModelEvent` 层加 `ToolCallDelta` / `ThinkingDelta`；`AgentEvent` 层**只让「产物型」delta 冒泡**（`ModelTextDelta` 已有、`ModelThinkingDelta` 新增），**工具参数不冒泡**——仅在参数完整时发一个 `ToolCall`（保留现有「一调用一事件」不变量，`agent-loop/src/lib.rs:174`）。带来好处：开流式后工具事件序列与非流式完全一致。
- **信道形态**：有界 + 容量可配；传 `Envelope{source, turn, event}`（不传裸事件）；`source` 字段现在占位，为多 agent 协作/群聊预留。
- **`emit` 语义**：改为 async，满时等待（背压），不丢弃不崩溃。推翻 ADR-0004 点 3，见 ADR-0009。
- **生命周期是策略**：`StopWhenConsumerGone`（交互式，默认）/ `ContinueWithoutConsumer`（后台长任务）。**消费者消失时不打断正在进行的轮**——干完当前轮再看策略（学 pi-server）。
- **消息模型**：agent 自己转；入站**两条队列**（steering / followUp）+ **`QueueMode` 可配**；`/new` = 换队列（agent 不知情）；**队列 = 日志 + 游标**（不是独立容器）；一队列一 agent；每轮边界查队列；模型回合开始取一次；Esc 不清队列。
- **`max_rounds` 必须可配**；`bash_timeout` 默认 `None` 对长任务是隐患。
- **`ys-channel`（新）**：装契约不装实现，只依赖 `ys-core`。
- **存储**：JSONL 足够（pi 主线也是 JSONL）；**SQLite / 服务端 / 拆进程均不做**。
- **产品形态**：两种（交互式 Coding / 后台长任务），共用同一 Runtime；**服务端多客户端场景不在定位内**。
- **界面边界**：`ys-tui` 纯消费者，不认识 `ys-runtime`（见「界面 crate 边界」）。
- **命名**：`ys-` 前缀方案 A（见「命名方案」）。

**仍待决**：

- [ ] **Q2（压缩入口）**：1.5 走 (a) `Agent::compact()` 方法还是 (b) Hook 注入？影响与第二刀的先后依赖。倾向 (b) 以复用 Hook 底座。
- [ ] **Q3（Provider 分发）**：1.3 的 adapter 分发用「`ModelAdapter` 门面」还是「provider→构造器 HashMap」？影响 `config::ModelFactory` 现有签名是否要动。
- [ ] **Q4（Anthropic 范围）**：本轮 Anthropic 只做 `messages` 基础（无 thinking、无 cache_control），还是直接覆盖 thinking？影响 1.3 与 1.4 的合流方式。
- [ ] **Q7（交互入口，本轮不动）**：rustyline 是否替换 ratatui。本轮已决定**先不动**，留到事件出口那一半落地后再评估。
- [ ] **Q8（接线器拆分）**：`ys-coding-agent`（库/接线器）与 `ys-cli`（薄 bin）是否拆成两个 crate？倾向拆（照 pi：`packages/coding-agent` 是库、`bin/` 是入口），使装配逻辑可测。
- [ ] **Q9（改名时机）**：改名与「新增 `ys-tui`/`ys-channel` crate」的先后。倾向**先改名**（新增前统一，避免新旧混名），一次全仓机械替换 + `cargo test` 验证。
- [ ] **Q10（`Envelope` 放哪层）**：已定放**新增的 `ys-channel`**（不塞进 `ys-event`），与 `ys-event` 平级。
- [ ] **Q11（信道容量默认值）**：`-p` / `--json` / TUI 各自的默认容量。已定「足够大 + 可配」，具体数值在设计阶段定。
- [ ] **Q12（与并行会话协调）**：`docs/arch/tui-interaction-test/`（`Prompter`/`TuiSurface` trait）改动同一块地（`ui/mod.rs` + `commands/mod.rs`）。其 `Prompter` 实现**已进主干**（`commands/mod.rs:49`，commit `c212d9b`）。改名前须先跑基线 `cargo test`。
- [ ] **Q13（演员模型的连带修订）**：agent 自己转后，`Agent::clear_session` 消失、ADR-0007/0008 大半被架空、`/model` 等命令不再直接改 agent。需评估这些决策如何修订，并决定 `run_turn` 退休后的新接口形态（自转循环放 `ys-loop` 还是接线器）。
- [ ] **Q14（长任务的落盘与恢复）**：`ContinueWithoutConsumer` 下事件无人消费，需落盘 JSONL；`JsonlSession` 当前只存消息不存事件，需扩展。另：中断后如何回看过程。属第二产品形态的前置，本轮不做。
- [ ] **Q15（产物事件）**：投资 agent 现靠 agent 自然语言转述产物路径（etf-display-tool 的「ToolResult.content 是扁平字符串」约束），长任务下不可靠。是否引入 `ReportGenerated { path }` 之类事件。属第二产品形态。
- [ ] **Q16（`/compact` 与 lifecycle 的交互）**：长任务下自动压缩（`basic.rs:109-116`）会重写会话，与"事件落盘回看"可能冲突。未讨论。

---

## 后续建议

- 用 `arch-design` 分别对**枢纽 1（事件通道 + 流式）**和**枢纽 2（HookDispatcher 最小切片）**做方案设计与 ADR，二者各自成文档、各自可独立合并。
- 用 `prototype` 验证 S1/S2 的「增量拼接与终态一致」假设（测试矩阵已要求该不变式）。
- 每项落地补测试矩阵条目：`ModelEvent` 序列化、SSE 增量组装、channel sink 事件顺序、`compact` 闭环、`/new` 会话文件恢复、provider compat 映射、Hook 顺序/Observer 容错。
