# Coding Agent TUI（路线 B）— As-built 设计

> 性质：**实际建成记录**（as-built），非计划复述。写「最终是什么样」及「实现中改掉了什么」。
> 范围：`docs/design-plans/completed/2026-09-14-coding-agent-tui.md`（设计）+
> `docs/exec-plans/completed/2026-09-14-coding-agent-tui.md`（执行计划 T1–T18）。
> 日期：2026-09-14
> 上游文档：
> - 设计 `docs/arch/coding-agent-tui/design.md`（路线 B：拆 crate + 分线程 + 信道 + `ys-protocol`）
> - 质量分析 `docs/arch/coding-agent-tui/review.md`（R1–R7，全部落实）
> - 前身（**已作废**）`docs/arch/coding-agent-tui/tui-repl-context.md`（行式 REPL 方案，结论已并入本文与 design.md）
> - 前一把刀 `docs/design-final/core-channel.md`（本文件取代它的信道/actor 部分，见该文 §8）
>
> 一句话结论：**路线 B 全量落地，且编译器边界真的生效** —— `ys-tui-coding` 不认识 `Agent`、
> `ys-protocol` 零 tokio；`Inbox`/`Intent`/`QueueMode`/`CancelToken`/`Agent::run` 全部移除，
> 由「三信道 + `BoundarySource`」取代；旧 `ui/` 的 1684 行按新拓扑重写为独立 crate。

---

## 1. 定位与依赖图

```
ys-protocol          能力平面（协议）—— Request / Boundary / Outbound<V> / BoundarySource    ← 所有产品共用
ys-tui-coding        coding agent 的 TUI —— CodingView + ratatui 渲染/输入/补全               ← 本设计，独立 crate
ys-tui-invest        投资 agent 的 TUI（将来）
apps/coding-agent    接线器 —— 构造 CodingView、实现 app 侧循环、驱动 agent
```

依赖方向（单向、无环，**编译器强制**）：

```
ys-protocol ──► ys-tui-coding ──► apps/coding-agent
     ▲                                    │
     └────────────────────────────────────┘   （app 也依赖 ys-protocol，用同一套类型）
```

| 纪律 | 落点 | 强制方式 |
|---|---|---|
| `ys-tui-coding` **不依赖 `ys-runtime`** | `Cargo.toml` 无该依赖项 | 编译器（crate 边界）；注释里写死「本 crate 不认识 `Agent`」 |
| `ys-protocol` **零 tokio** | `Cargo.toml` 生产与 dev 依赖皆无 tokio | `cargo tree` 可验；`Mutex` 用 `std::sync` |
| 视图类型归**产品 TUI crate** | `CodingView` 定义在 `ys-tui-coding`，**不进协议** | `Outbound<V>` 泛型化 |

**`ys-protocol` 公开面**：`Request` / `Boundary` / `Outbound<V>` / `BoundarySource` /
`QueueBoundarySource` / `Envelope` / `Source` / `LifecyclePolicy`。

> **`Envelope` / `Source` / `LifecyclePolicy` 的迁移**：它们原属 `crates/ys-channel`。
> `ys-channel` 整 crate 已删除，三型随迁 `ys-protocol`（纯数据，零改动）。
> 详见 `core-channel.md` §8。

---

## 2. 三信道与两线程

```
┌─ UI 线程（普通 fn，无 runtime）────────────────────────────────┐
│  ys_tui_coding::run(view₀, rx_out, tx_req, tx_boundary)        │
│  loop {                                                         │
│    draw(三 pane)                                                │
│    排空 rx_out（try_recv，非阻塞）→ 更新 transcript / view       │
│    poll(50ms) → 按键 / bracketed paste                          │
│    300ms 节拍 → Working 动画（仅 turn 中）                       │
│    提交输入 → tx_req.blocking_send(Request)                     │
│    中途插话 / Ctrl+C → tx_boundary.blocking_send(Boundary)       │
│  }                                                              │
└────────────────┬──────────────────────────────▲────────────────┘
     ① Request / ② Boundary              ③ Outbound<CodingView>
                 │                              │
                 ▼                              │
┌─ app 线程（tokio multi-thread worker）─────────┴────────────────┐
│  app_loop::run(agent, wiring, …)                                │
│  loop {                          // ① 回合之间                   │
│    空闲期丢弃残留边界消息（防上一回合的 Abort 误伤下一回合）      │
│    let req = request_rx.recv().await                            │
│    match req {                                                  │
│      Prompt(msg) => { turns += 1;                               │
│        { let ports = wiring.ports();                            │
│          ports.events.begin_turn(turns);                        │
│          let boundary = QueueBoundarySource::new();  // 每回合新建│
│          turn = agent.run_turn(input, AgentPorts::new(.., Some(&boundary)))│
│          select! { turn | sink_rx → out.send(Event) | boundary_rx → boundary.push }│
│        }                                                        │
│        排空 sink_rx（回合末收尾事件，防晚一回合转发）             │
│      }                                                          │
│      SetModel/Login/Logout/NewSession/Compact/Export =>          │
│        capabilities::*（→ Vec<String>）                          │
│    }                                                            │
│    Outbound::Output(lines) + Outbound::View(build_view())       │
│  }                                                              │
└─────────────────────────────────────────────────────────────────┘
```

| 信道 | 类型 | 方向 | 目的地 / 消费时机 | 容量 |
|---|---|---|---|---|
| ① Request | `ys_protocol::Request` | UI → app | app 线程 `recv().await`（**回合边界**） | 64 |
| ② Boundary | `ys_protocol::Boundary` | UI → `BasicLoop` | **轮边界**拉取（`take`）；模型调用前 / 工具轮询用 `is_aborted` | 64 |
| ③ Outbound | `ys_protocol::Outbound<CodingView>` | app → UI | UI 事件循环 `try_recv` 排空 | 1024 |

**为何 ② 必须独立于 ①**：回合跑动时 app 线程阻塞在 `agent.run_turn()` 里，不可能 `recv` ①；
而 `Steer` / `Abort` 要**中途**被看到 → 只能由 `BasicLoop` 在轮边界拉。这一条是设计的承重点，实现严格照做。

**为何分线程**（路线 B 的必然）：`ys_tui_coding::run()` 阻塞在自己的事件循环里（用
`Sender::blocking_send`），且**不认识 `Agent`** —— 若在同一线程跑 agent，拆 crate 就成了摆设。
反向也成立：保持单线程 `select!` ⇔ 保持 `ui/` 模块。

**单一出站写者**：`Outbound<V>` 是 UI 的唯一输入流。`ChannelSink` 仍写自己的 `Envelope` 信道，
app 侧多跑一个转发循环把它转成 `Outbound::Event`。代价是每事件多一跳（~0.02% CPU，可忽略），
换来事件与 `Output` 的确定顺序（两条信道各排各的话，顺序无保证）。

**`ChannelSink` 本身零改动**（设计 §7「顺带」）：仍是有界 mpsc + overflow + `begin_turn` + 背压统计，
只是 `Envelope` 的 `use` 从 `ys-channel` 改成 `ys-protocol`。

---

## 3. 协议形状

```rust
// ys-protocol
pub enum Request {
    Prompt(Message),                              // → agent 的输入
    SetModel { model: String },
    Login { provider: String, api_key: String, api_base: Option<String> },
                                                  // api_base = 用户显式填的 URL（见 §8 M3）
    Logout,
    NewSession,
    Compact,
    Export { path: Option<PathBuf> },             // 实现是 Option（见 §8）
}

pub enum Boundary {
    Steer(Message),   // 轮边界注入（追加进会话 + 发 UserMessage，不增 rounds）
    Abort,            // 取代 CancelToken
}

pub enum Outbound<V> {
    Event(Envelope),  // 事件（已封壳）
    View(V),          // 快照（产品视图 V）
    Output(String),   // 命令输出 → transcript
    Quit,
}
```

**没有 `Diagnostic` 变体**（采纳 design §6；**否决** review R2 的「加 `Diagnostic{level,text}`」倾向）：
命令期间的用户可见诊断走 `Output`；库内部诊断走日志文件（§7）。

**Drop 语义即退出信号**：UI 退出时 drop 掉 `tx_req` / `tx_boundary` → app 线程 `recv()` 得 `None`
→ `app_loop::run` 返回 `Ok(())`。无需显式关机协议。

---

## 4. `BoundarySource` 与 Abort 语义

```rust
// ys-protocol（零 tokio）
pub trait BoundarySource: Send + Sync {
    fn take(&self) -> Option<Boundary>;   // 非阻塞、保序
    fn is_aborted(&self) -> bool;          // 非破坏性探针（可重复调用）
}

pub struct QueueBoundarySource { inner: Mutex<Inner { queue: VecDeque<Boundary>, aborted: bool }> }
impl QueueBoundarySource {
    pub fn new() -> Self;
    pub fn push(&self, boundary: Boundary);   // 生产者可在 agent 运行期间投递
}
```

**这是实现期的微决策**（design.md 只写了「`BasicLoop` 在轮边界 `try_recv`」，未定 trait 形状）。
要点：

- 两方法都是 **`&self`（内可变）**：`BasicLoop` 经 `&dyn BoundarySource` 访问，生产者（app 侧）
  却可并发 `push` —— 旧 `Inbox` 用 `Arc<Mutex<Inner>>` 达到同样效果，这里用 `Mutex` 足够（trait 对象本身可 `&` 共享）。
- **`Abort` 同时置位 + 照常入队**（保序）：`is_aborted()` 立刻为真（供模型调用前 / 工具轮询的
  「随时探针」），而 `take()` 仍会在轮边界把它交还给消费者。标记**不随 `take` 清除**。
- 选 `std::sync::Mutex` 而非 tokio 锁：本 crate 零 tokio，且临界区只有队列操作、不跨 `await`。

**Abort 的三条出路**（`BasicLoop`，`crates/ys-loop/src/basic.rs`）：

| 位置 | 检查 | 动作 |
|---|---|---|
| Step 1（入口） | `ctx.boundary.is_some_and(\|b\| b.is_aborted())` | emit `RunFinished{Cancelled}` 并返回 |
| Step 3（模型调用前） | 同上 | 同上 |
| Step 4b（轮边界） | `boundary.take()` 匹配 `Steer`（append + emit `UserMessage`，**不增 rounds**）/ `Abort`（`RunFinished{Cancelled}`） | 见下 |

> 保留 Step 4b 的 `Abort` 分支是**压缩窗口的防御路径**：正常时序下 Abort 入队即置位，
> Step 3 的探针已能命中；只有「上下文压缩 `await` 期间 Abort 才到达」时，探针已过、消息刚到，
> 靠这条路兜住。

**工具级取消载体**：`ToolContext.cancel: &CancelToken` → `ToolContext.boundary: Option<&dyn BoundarySource>`
（`ys-component::ToolContext`，构造函数 `ToolContext::new(boundary, cwd, workspace_root)`）。
`BashTool` 在挂载 `boundary` 时**以 100ms 间隔轮询 `is_aborted()`**，命中则杀**进程组**
（`sh -c "kill -9 -{pid}"`，不引 libc）并返回中止错误；未挂载时行为与改动前完全一致（有测试钉住）。
`timeout` 语义在挂载 `boundary` 时仍生效。

**每回合新建 `QueueBoundarySource`**（**实现 ≠ design 伪代码**，见 §8）：`Abort` 会**永久**置位
`is_aborted`，复用同一个源会让「上一回合的取消」把之后每个回合立刻取消。app 循环顶部还有
`while boundary_rx.try_recv().is_ok() {}`，在空闲期丢掉「上一回合结束后才到」的残留边界消息
（UI 的 `is_turning` 复位有最长一个 poll 周期的滞后，期间一次 Ctrl+C 会投出一条来不及被消费的 Abort）。
有变异测试钉住：删掉排空语句 → 残留 Abort 把首个回合取消 → 测试变红。

---

## 5. UI 形态

三个 pane 自上而下，均为**固定分区**，每次重绘整屏；会「滚走」的只有 Chat 里的内容：

| pane | 约束 | 说明 |
|---|---|---|
| **Chat** | `Constraint::Min(3)` | 唯一可滚动区域；滚动以 **wrap 后的视觉行**为单位 |
| **Input** | `Constraint::Length(1..=5)` | 高度随内容自适应（`INPUT_MAX_LINES = 5`） |
| **Status** | `Constraint::Length(1)` | **最底**，常驻一行 |

补全浮层 / 模态选择器**独立于三 pane**：`Clear` 后盖在 Chat 区域上，不参与布局高度计算。

**对话区**：`TranscriptLine` 是**结构化**的（`User` / `Assistant` / `Thinking` / `Tool{id,name,summary,result,success}` /
`Summary{rounds,stop,elapsed_secs}` / `Error` / `System`）—— 工具调用压一行渲染，摘要由
`summarize_tool_args`（`SUMMARY_KEYS = [path, file_path, command, pattern, query, url]`）从结构化参数抽取，
**摘要规则属产品知识，故在 TUI crate 而非协议**。失败的工具显示结果首行，成功的只给 ✓。
thinking 默认隐藏，`/thinking on` 后以暗色渲染。

**状态行**：`model · ↑↓tokens · N 轮 · 时长`，窄屏**从尾部剥**（时长 → 轮数 → tokens → model）；
turn 中把 model 段换成 `⏳ Working·`（300ms 节拍前进，宽字符占两格）。四档宽度各有测试。

**输入区**：多行（`Shift/Alt+Enter` 换行、`Enter` 提交）；启用 bracketed paste（`Event::Paste` 原样
`insert_str`，不被当多次提交）；启用 `PushKeyboardEnhancementFlags`。
`InputBuffer.cursor` 是 **char 索引**，退格走字节边界求区间再 `replace_range` —— 修掉了旧
`ui/events.rs:131` 的 `cursor - 1` 落在多字节字符中间导致的**中文退格 panic**。

**键位归属**：竖直方向归聊天（`Up/Down/PageUp/PageDown`），水平方向归输入（`Left/Right/Delete/Home`）；
`End` 回聊天底部（早于 `Home` 绑定，故不做对称）。补全浮层打开时上下归浮层。

**退出**：无条件恢复终端 —— 把完整对话按当前宽度 wrap 后逐行写回**主屏 scrollback**，再离开
alt-screen（pi 语义：退出后对话仍在终端历史里可翻）。thinking **总是**打印（退出后没有 `/thinking`
开关可用了）。

---

## 6. 命令划分：TUI 侧表 vs app 侧能力

**表在 TUI 侧**（`ys-tui-coding/src/commands.rs`，11 条 `CommandSpec`）：命令的**用户可见行为**
（提示什么、问什么）归 UI；**能力实现**归 app。二者唯一契约是 `Request` —— 漏实现是**编译错误**
（app 循环对 `Request` 的 `match` 必须穷尽），不会静默漂移。

```text
输入 ──parse()──► Action ──┬─► Local(LocalAction)   TUI 自己处理（写 transcript）
                           ├─► Request(Request)     → app 线程（信道 ①）
                           ├─► Prompt(PromptKind)    → 模态浮层 → resolve_prompt → Request
                           ├─► Abort                 → 轮边界（信道 ②）
                           └─► Quit                  退出 UI
```

| 命令 | TUI 做什么 | 发给 app |
|---|---|---|
| `/help [command]` | 本地打印命令表（有参只给该条） | — |
| `/status` | 本地（`CodingView` 在 TUI 手里） | — |
| `/copy` | 本地（最后一条 assistant 回复写进 transcript，**无剪贴板依赖**） | — |
| `/quit` | 本地 | — |
| `/thinking [on\|off]` | 本地开关 **（不在 design §6 表里，实现新增，见 §8）** | — |
| `/model [name]` | 无参时**浮层选择**（`available_models`） | `SetModel` |
| `/login` | **问 provider / api_key**（`Prompter` 浮层） | `Login` |
| `/logout` | — | `Logout` |
| `/new` | — | `NewSession` |
| `/compact` | — | `Compact` |
| `/export [path]` | — | `Export` |

**`Prompter` trait 保留**（design §3 注；死的只是 `InquirePrompter`）：
`select` / `text` 是「选择器可测」的答案。**决策逻辑**抽在纯函数 `resolve_prompt(PromptKind, &dyn Prompter, &CodingView)`
里，单测注入 `FakePrompter` 即可覆盖全部路径；TUI 实现 `TuiPrompter`（`run.rs`）只把同一套问答画成
嵌套事件循环的模态浮层。任一问答步骤取消（Esc / 空 key）→ **不产生任何 `Request`**（半截登录比什么都不做更糟）。

**app 侧能力**（`apps/coding-agent/src/capabilities.rs`）：`login` / `logout` / `set_model` /
`new_session` / `compact` / `export`，每个返回 `Vec<String>`（每项一行），由 `app_loop` 包成
`Outbound::Output`。**本模块绝不 print**（TUI 起屏后 stderr/stdout 会冲掉整屏）。
`new_session` / `compact` 是 `async`（碰 `Session`），`export` 是**同步**（见 §8）。

> **占位语义**（与旧行为一致）：`/compact` 仍是 `clear_session`（真压缩 TBD）；`/copy` 无剪贴板；
> `/export` 无参时落点由 app 决定 = 会话文件自身（**带自拷贝护栏**：`fs::copy` 对「源 == 目标」
> 会先截断目标 —— 那是清空会话而非导出）。

---

## 7. 输出与诊断三分（解 review R2）

| 时机 | 内容 | 去处 |
|---|---|---|
| **启动期**（TUI 未起） | 容量 clamp 警告、`No model configured` | **stderr**（照旧） |
| **命令期间**（有 TUI） | `/login` 的 "✓ Logged in…"、"Warning: Could not persist credentials" 等 | **`Outbound::Output`** → transcript |
| **库内部 / 随时** | `auth.json` / `state.json` 解析失败 | **日志文件** `~/.yushan/logs/yushan.log` |

**日志文件**（`apps/coding-agent/src/logging.rs`）：手写极简 logger —— 追加写、行首 `[unix秒]` 前缀、
**best-effort**（目录创建 / 打开 / 写入任何 IO 错误一律静默忽略，返回值 `()`，不 panic、不传播）。
**不引新依赖**（只有两个调用点，`tracing` 杀鸡用牛刀；时间戳用 `SystemTime` 的 unix 秒，不引 `chrono`）。
`app_loop` 也用它记录「回合失败」与「app 循环异常退出」（不打扰用户）。

> **只有 2 处（`provider.rs` / `state.rs`）严格需要日志文件** —— 其余靠「启动期 stderr / 命令期
> `Output`」就够。日志文件的价值是**通用兜底**：TUI 应用没有「历史输出」可看。

`--stats` 的背压读数**只在非 TUI 模式**出现，写 stderr，不污染 stdout 的 JSON / 文本流。

---

## 8. 与 `design.md` 的分歧与微决策（**承重部分**）

### 8.1 实现 ≠ design 原文

| # | design.md 原文 | 实际实现 | 原因 |
|---|---|---|---|
| D1 | `Request::Export { path: PathBuf }`（§6） | `Export { path: Option<PathBuf> }` | 无参 `/export` 的**默认落点由 app 按会话文件决定**（TUI 不替 app 猜一个相对 cwd 的路径，否则会静默覆盖同名文件）；`None` 即「确保会话已落盘」 |
| D2 | 未定 `BoundarySource` 形状（§8 只写轮边界 `try_recv`） | `&self` 的 `take() -> Option<Boundary>` + `is_aborted() -> bool`；实现 `QueueBoundarySource` | 见 §4。零 tokio trait；产物是 `ys-protocol` 的公开面 |
| D3 | app 循环伪代码把 `boundary_rx` 直接交给 `BasicLoop`（§7） | app 侧在 `select!` 里把 `boundary_rx` 收到的 `Boundary` **push 进每回合新建的 `QueueBoundarySource`** | ① app 循环在回合中已经 `select!` 着 `boundary_rx`，顺手泵进源即可；② 一次性源避免 `Abort` 标记跨回合污染（§4）。另加空闲期排空残留 |
| D4 | app 循环伪代码在回合末**不排空**事件信道（§7） | 回合结束后 `while let Ok(env) = sink_rx.try_recv()` 排空 | `select!` 分支选择是随机的，可能先选中 `turn` 分支 —— 此刻 `RunFinished` 等收尾事件还躺在信道里；不排空会晚一整回合转发（`is_turning` 不复位、turn 摘要不显示） |
| D5 | `CodingView` 字段清单「含 `ToolCallId → ToolCall` 映射，供工具结果回填」（§9 未决） | **视图不含映射**；结果回填靠 **UI 侧** `App` 自己的 `tool_index`（transcript 下标） | 视图只承载「状态行 / `/status`」要显示的东西；回填是渲染关注点，归 UI |
| D6 | 命令表只有 §6 的 9 条 | 表里有 **11 条**，多出 `/thinking` 与 `/copy`（后者 §6 表里写「本地」但表头只有 9 行） | `/thinking` 在 §2.1 UI 规格里出现（`/thinking on`），实现把它做成**本地命令**（`Thinking(Option<bool>)`，无参 toggle）；`/copy` 是 §6 表里的「本地」项 |
| D7 | 初始化顺序编号为 1.信道 2.sink 3.阻塞 Wiring 4.Agent 5.view₀ 6.spawn（§7） | 实际 1+2.channel/sink → **4.Agent** → **3.阻塞 Wiring** → 5.view₀ → 6.spawn | Agent 构建不阻塞、不依赖 Wiring；先建 Agent 后建 Wiring 不破坏 R7 的承重结论（**阻塞早退发生在 spawn 之前**，`begin_turn` 只在 agent 真跑时发生） |
| D8 | `ys-protocol` 里的 `Envelope` / `LifecyclePolicy`「迁往 ys-protocol」（review R1 建议） | 已迁；`ys-channel` 整 crate 删除 | 见 `core-channel.md` §8 |
| D9 | 设计 §4 说 app 的 `format.rs`「只剩 `format_tokens`」 | app 的 `format.rs` **删除**；`format_tokens` / `format_duration` 落在 `ys-tui-coding/src/format.rs` | 唯一消费者（渲染）已在 TUI crate |
| D10 | review R2「倾向」在 `Outbound` 加 `Diagnostic { level, text }` | **未采纳** —— 采纳 design §6「没有 `Diagnostic` 变体」 | 命令期诊断走 `Output`、库内部走日志文件，通用变体无消费者 |

### 8.2 design.md 未写、实现时补的微决策

| # | 决策 | 落点 |
|---|---|---|
| M1 | **`resolve_prompt` 纯函数**：模态命令的决策逻辑（问什么、按什么顺序、取消怎么办）与渲染分离 | `ys-tui-coding/src/prompter.rs` |
| M2 | **`TuiPrompter` 用嵌套事件循环**把问答画成模态浮层（不切屏、不 suspend/resume） | `ys-tui-coding/src/run.rs` |
| M3 | **`/login` 的 api_base 解析**：TUI 在所选 provider **没有内置 base** 时（`custom` 这类）**追加一问 URL**，随 `Request::Login { api_base: Option<String> }` 交给 app；app 侧按 ① 用户显式 URL → ② provider 内置 base → ③ 回退 `config.api_base`（`YUSHAN_API_BASE`）→ ④ 都没有则**明确提示**要设环境变量（不静默失败、不写空 base 的 auth）。**协议因此比 design §6 多一个 `api_base` 字段**（初版实现漏了它，导致 `custom` 的交互式 URL 失效——已修） | `ys-tui-coding::prompter::resolve_prompt` + `capabilities::login` |
| M4 | **`/model` 无参的模型列表来自 `CodingView.available_models`**，由**静态** registry 表填充（`ProviderRegistry::known_models_static()`）；**TUI 路径不发 `/v1/models` 请求**（重绘不得触发 HTTP） | `apps/coding-agent/src/view.rs` |
| M5 | **`Action::Abort` 目前是保留变体**：`parse()` 不产出它；Ctrl+C / Esc 在 `handle_key` 里直接投 `Boundary::Abort`。留作将来 `/abort` | `ys-tui-coding/src/commands.rs` |
| M6 | **`app_loop` 的回合失败只落日志、不刷 transcript**：`BasicLoop` 返回错误前已发 `RunFailed`（UI 已看到 Error 行） | `app_loop::run` |
| M7 | **`/new` 后 `session_started` 重置**（状态行时长据此归零） | `app_loop::run` |
| M8 | **`export` 为同步 `std::fs::copy`**：会话是本地小文件；把一个非 `Sync` 的 `&Wiring` 借进 app 循环的 future 跨 `await` 会让整个循环不再 `Send`、无法 `spawn` | `capabilities::export` |

---

## 9. 测试资产（搬走了什么、删了什么、新增了什么）

### 9.1 删 / 搬

| 资产 | 处置 |
|---|---|
| `crates/ys-channel` 全部单测（15 条） | **删**（`Inbox`/`Intent`/`QueueMode` 语义消失）；`Envelope`/`LifecyclePolicy` 的相关断言随之迁到 `ys-protocol` |
| `apps/coding-agent/src/ui/`（含 `draw.rs` 的 ~500 行 `TestBackend` 断言） | **重写**进 `ys-tui-coding`（按新 pane 布局与结构化 transcript） |
| `apps/coding-agent/src/ui/` 的 `TranscriptLine` 快照测试 | **搬**进 `ys-tui-coding/src/transcript.rs`（改为结构化形态） |
| `commands/builtin.rs` 的命令单测 | **拆**：用户可见行为 → `ys-tui-coding/src/commands.rs`；能力实现 → `capabilities.rs` |
| `ys-runtime/tests/actor_run.rs`（7 条，测 `Agent::run` 自转） | **删 / 重写**：`Agent::run` 已不存在；多回合语义改由 app 循环承担，等价测试落在 `app_loop.rs` 与 `crates/ys-runtime/tests/run_turn_boundary.rs` |

### 9.2 新增（当前文件内 `#[test]` / `#[tokio::test]` 计数）

| 落点 | 条数 | 覆盖 |
|---|---|---|
| `crates/ys-protocol` | 28 | `Boundary` FIFO / `Abort` 置位即见 / `is_aborted` 非破坏性 / Steer 不置位 / `Send+Sync` trait 对象；`Request`/`Outbound`/`Envelope` 序列化 |
| `crates/ys-tui-coding` | 166 | 三 pane `TestBackend` 渲染、状态行四档宽度、输入区（多行 + 粘贴 + 中文退格）、补全浮层、命令表逐条映射、`resolve_prompt` 决策路径、`FakePrompter` |
| `crates/ys-runtime/tests/run_turn_boundary.rs` | 5 | `run_turn` + `AgentPorts` 带 `BoundarySource` 的轮边界语义（跨 crate，不反向依赖 app） |
| `apps/coding-agent/src/app_loop.rs` | 2 | Prompt → 事件（turn 从 1）→ View；`NewSession` → Output + 刷新快照；**残留 `Abort` 被空闲期丢弃**（变异测试钉住） |
| `apps/coding-agent/src/capabilities.rs` | 13 | `/login`（成功 / 未知 provider / custom 无 base / 回退）、`/logout`、`/model`（成功 / 缺凭证置空）、`/new`、`/compact`、`/export`（含**自拷贝护栏**变异测试） |
| `apps/coding-agent/src/logging.rs` | 5 | 追加语义、父目录自建、不可写静默、无 HOME、`[unix秒]` 前缀 |

### 9.3 变异测试（本次重点）

按项目约定，关键逻辑注入 bug 验证测试能抓到：

- 删 `app_loop` 顶部的空闲期排空 → 残留 `Abort` 把首个回合取消 → `stale_abort_queued_before_first_turn_is_discarded` 变红。
- 去掉 `/export` 的 `is_same_file` 护栏 → `fs::copy` 自拷贝截断会话文件 → `export_without_path_does_not_truncate_session_file` 变红。
- `/model` 无参改成直发 `SetModel` → `test_bare_model_never_sends_set_model_directly` 与命令映射表测试同时变红。
- `/help <cmd>` 改回无条件打全表 → `test_help_with_command_shows_only_that_command` 变红。

---

## 10. 已知缺口（诚实清单）

- `/compact` 仍是 `clear_session` 占位（真压缩 TBD，与旧行为一致）。
- `/copy` 无剪贴板集成（v1 把内容写进 transcript，手动选中）。
- `/new` 后 `App.transcript` 不清（旧对话行仍在屏上；`CodingView.message_count` 已归零）—— 与
  `core-channel.md` §7.7 记录的同一行为，本版未改。
- `-p` / `--json` 无 steering 生产者：轮边界机制代码路径完整、有单测与跨 crate 集成覆盖，
  但 CLI 形态下无人投 `Boundary`（交互 TUI 才用得上）。
- `ys-tui-coding` 的模态浮层与补全浮层共享 `Clear` 后覆盖渲染，未做统一的 `Overlay` 抽象
  （design §1 注允许二选一）。
