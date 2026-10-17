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
│      （turn 中第二段：再按一次 Ctrl+C → should_quit，见 §8.2 M17）│
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
| **Chat** | `Constraint::Min(3)` | 唯一可滚动区域；滚动以 **wrap 后的视觉行**为单位；窗格**只留顶部标题线**（见下） |
| **Input** | `Constraint::Length(1..=5)` | 高度随内容自适应（`INPUT_MAX_LINES = 5`）；窗格**四边带框**（有意保留） |
| **Status** | `Constraint::Length(1)` | **最底**，常驻一行 |

补全浮层 / 模态选择器**独立于三 pane**：`Clear` 后盖在 Chat 区域上，不参与布局高度计算。

**Chat 窗格几何（拷贝友好）**：Chat 外框为
`Block::new().borders(Borders::TOP).padding(Padding::bottom(INPUT_GAP_ROWS))`
（只留顶部标题线，底部再留 `INPUT_GAP_ROWS = 1` 行空白）—— 终端原生选择不再把窗格竖线拷进剪贴板；
底部那行空白让**上一轮的输出不贴输入框**。用**空行**而不是给 Chat 加一条底边框：Input 自带
带标题的四边框（见下），再画一条就成了相邻的两条横线；横线还会被拷进剪贴板（与去竖线同一取向）。
内区因此为「宽 `w` / 高 `h − 1（顶边框） − INPUT_GAP_ROWS`」，且**统一取自同一个
`chat_block()` 实例的 `inner(area)`**（`max_scroll_offset` 与 `draw_transcript` 共用同一构造，
杜绝散落的手写 `saturating_sub(2)` 漂移 —— `inner_h` 漏改是合法 `usize` 减法、编译器不抓）。
间距也走同一个 `inner`：它计入 padding，故取用方无需各自记得减它。
**Input 窗格仍是 `Block::bordered()`（有意保留，只有 Chat 去边框）**；表格**保留方框线**
（有意接受的例外）。详见 §14.4。

**对话区**：`TranscriptLine` 是**结构化**的（`User` / `Assistant` / `Thinking` / `Tool{id,name,summary,result,success}` /
`Summary{rounds,stop,elapsed_secs}` / `Error` / `System`）—— 工具调用压一行渲染，摘要由
`summarize_tool_args`（`SUMMARY_KEYS = [path, file_path, command, pattern, query, url]`）从结构化参数抽取，
**摘要规则属产品知识，故在 TUI crate 而非协议**。失败的工具显示结果首行，成功的只给 ✓。
thinking 默认隐藏，`/thinking on` 后以暗色渲染。
provider 把思考内联在 `delta.content` 的 `<think>`/`<thinking>`/`<reasoning>` 标签里时，由适配器剥离并转成同样的 Thinking 行（见 `docs/adr/0025-hide-inline-thinking-tags.md`）。

**问答边界（`USER_GAP_ROWS = 2`）**：每条**非首条可见** `User` 条目之前恒留 **2 行空白**，把上一回合的产出
（assistant 文本 / 工具行 / 回合摘要）与本轮提问分开 —— 多轮对话里一眼看得到「这一轮从哪开始」。
与上面的 `INPUT_GAP_ROWS` 正交：那个在 transcript **外部**（内区底 → Input 顶边框），管不到问答边界。
判据是「该条目之前**已有可见行**」而**不是**「下标 > 0」：`/thinking off` 时首条若是 `Thinking`，
它不产出行，按下标会把紧接的首条提问误判成非首条、顶部凭空多两行。
相邻两条 `User`（回合中插话 / 排队回声）之间**同样留白** —— 规则统一为「非首条可见即留」，不按上一条是不是 `User` 特判。
`user_gap_rows` **只返回计数**，两个渲染循环各自落到自己那层容器（TUI 的 `Vec<Line>` / 退出回放的 `Vec<String>`）；
**不得**把判断下沉进 `push_non_assistant` —— 回放每条都新开一个局部 `Vec<Line>` 再 extend，在其上判空恒真，
会给**每条**用户行都加空行。空白行经 `build_text` 进入那份唯一的 `Text`，故 `max_scroll_offset` 的
视觉行数**自动含它**（TUI 画面与滚动数学无分叉可能）。

**状态行**：`model · ↑↓tokens · N 轮 · 时长`，窄屏**从尾部剥**（时长 → 轮数 → tokens → model）；
turn 中把 model 段换成 `⏳ Working·`（300ms 节拍前进，宽字符占两格）。四档宽度各有测试。

**输入区**：多行（`Enter` 提交；`Shift`/`Alt` + Enter 换行，另加 **`Ctrl-J` 普适兜底** ——
`Shift+Enter` 依赖终端的 Kitty 键盘增强协议，Termux 等不支持的终端下它与 `Enter` 发出**同一个
字节 `\r`**、无从区分，见 ADR-0037）；启用 bracketed paste（`Event::Paste` 原样 `insert_str`，
不被当多次提交）；启用 `PushKeyboardEnhancementFlags`（尽力而为，不支持的终端静默降级）。
turn 进行中照常显示输入缓冲与光标（键入立即可见，`Enter` 排队到下一回合）；
**`Working` 心跳只出现在状态行**。
`InputBuffer.cursor` 是 **char 索引**，退格走字节边界求区间再 `replace_range` —— 修掉了旧
`ui/events.rs:131` 的 `cursor - 1` 落在多字节字符中间导致的**中文退格 panic**。

**键位归属**：竖直方向归聊天（`Up/Down/PageUp/PageDown`），水平方向归输入（`Left/Right/Delete/Home`）；
`Ctrl`·`⌥` + 左右（或 `⌥b` / `⌥f`）让输入光标**按词**移动，`Ctrl-A` / `Ctrl-E` 到**输入行首 / 行尾**。
`End` 回聊天底部（早于 `Home` 绑定，故不做对称）—— 输入行尾**不占 `End`**，走 `Ctrl-E`。
补全浮层打开时上下归浮层。

**macOS 的 `⌘` 补记**：终端自己消费 `⌘` 组合，**从不把它交给应用** —— 所以 `⌘←`/`⌘→` 能否生效
只取决于终端把它们映射成了什么字节。Terminal.app 与 iTerm2 的**出厂映射**恰好是 `Ctrl-A` /
`Ctrl-E`（Cocoa 文本框的老规矩），故绑这两个即等价于「`⌘` + 左右」可用；要按**词**移动则用
`Ctrl`/`⌥` + 左右。用户若改过键映射，可在终端里跑 `cat -v` 看实际发出的字节序列自查。

**退出**：无条件恢复终端 —— 把完整对话按当前宽度 wrap 后逐行写回**主屏 scrollback**，再离开
alt-screen（pi 语义：退出后对话仍在终端历史里可翻）。thinking 遵守 `thinking_visible`（默认隐藏，`/thinking on` 后退出可见）。
**assistant 行的回放走 raw 路径**（按 `\n` 原样切行、不经 md 渲染 + 缓存）⇒ 主屏 scrollback 里留
**原始 markdown 源**（可复制、可搜索、可 grep），折行交给终端；其余变体行为不变（详见 §14.6）。

> **回放不做绝对定位**：`LeaveAlternateScreen`（`CSI ?1049l`）已把光标恢复到**进入 alt-screen 时**
> 的位置 —— 也就是本 app 自己那段启动输出的下一行。回放从那里往下写即可。**不得** `MoveTo(0, 0)`：
> 那是**视口绝对第 0 行**，会把屏幕顶部（往往是与本 app 无关的上一条命令输出）整片啃掉。
> 该不变量由 `run.rs` 的 `exit_payload_replays_without_absolute_positioning` 钉住 ——
> `restore_terminal` 除 `exit_payload()` 的产物外不写任何东西，故测试盯住那个纯函数即可覆盖。

---

## 6. 命令划分：TUI 侧表 vs app 侧能力

**表在 TUI 侧**（`ys-tui-coding/src/commands.rs`，13 条 `CommandSpec`）：命令的**用户可见行为**
（提示什么、问什么）归 UI；**能力实现**归 app。二者唯一契约是 `Request` —— 漏实现是**编译错误**
（app 循环对 `Request` 的 `match` 必须穷尽），不会静默漂移。

```text
输入 ──parse()──► Action ──┬─► Local(LocalAction)   TUI 自己处理（写 transcript）
                           ├─► Request(Request)     → app 线程（信道 ①）
                           ├─► Prompt(PromptKind)    → 模态浮层 ┬─ Request      → app 线程
                           │                                    └─ 回填输入框    （不发送）
                           ├─► Abort                 → 轮边界（信道 ②）
                           └─► Quit                  退出 UI
```

| 命令 | TUI 做什么 | 发给 app |
|---|---|---|
| `/help [command]` | 本地打印命令表（有参只给该条） | — |
| `/status` | 本地（`CodingView` 在 TUI 手里） | — |
| `/skills` | **浮层选择**（列全部 skill + description，按 `command` 升序、含**内置遮蔽过滤**）；选中**回填输入框** `/<command> `（尾随空格），由用户补参数后自行回车 | — |
| `/copy` | 本地（最后一条 assistant 回复写进 transcript，**无剪贴板依赖**） | — |
| `/quit` | 本地 | — |
| `/thinking [on\|off]` | 本地开关 **（不在 design §6 表里，实现新增，见 §8）** | — |
| `/model [name]` | 无参时**浮层选择**（`available_models`，每条显示 `provider/model`、可输入筛选）；有参直发，支持 `provider/model` 限定名 | `SetModel` |
| `/login` | **问 provider / api_key**（`Prompter` 浮层） | `Login` |
| `/logout` | — | `Logout` |
| `/new` | — | `NewSession` |
| `/resume [id]` | 无参时**浮层列出现有会话**供选择（Esc 取消、空列表只提示）；有参直切 | `ListSessions` / `ResumeSession` |
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

> **降级语义**：`/compact` **已接真压缩**（`capabilities::compact` → `ys_loop::plan` + `execute`：
> 摘要成一条 checkpoint、尾部原样保留）；四条路径里**三条失败（无 model / 无事可压 / 摘要失败）
> 都不碰 session 一个字节**，各给一行明确提示 —— 靠「什么都不做」而非「清空」降级。`/copy` 无剪贴板；
> `/export` 无参时落点由 app 决定 = 会话文件自身（**带自拷贝护栏**：`fs::copy` 对「源 == 目标」
> 会先截断目标 —— 那是清空会话而非导出）。

---

## 7. 输出与诊断三分（解 review R2）

| 时机 | 内容 | 去处 |
|---|---|---|
| **启动期**（TUI 未起） | 容量 clamp 警告、`No model configured` | **stderr**（照旧） |
| **启动期 · 降级提示** | `toolset` 的降级提示（搜索后端 / web 侧） | **TUI 模式 → `Outbound::Output`（transcript）**；`-p`/`--json` → **stderr**（见下） |
| **命令期间**（有 TUI） | `/login` 的 "✓ Logged in…"、"Warning: Could not persist credentials" 等 | **`Outbound::Output`** → transcript |
| **库内部 / 随时** | `auth.json` / `state.json` 解析失败 | **日志文件** `~/.yushan/logs/yushan.log` |

> **降级提示在 TUI 模式下为什么不走 stderr**：起屏**之前**写到 stderr 的行会留在终端的主屏
> 缓冲区里；TUI 用 alt-screen 盖住它们，退出时主屏还原 —— 于是它们又回到屏幕上，成为**退出后的
> 残渣**（与「对话回放到 scrollback」叠加，看起来像 app 又打了一遍启动横幅）。故 TUI 模式改投
> `Outbound::Output`（在 `spawn` 之前投递 ⇒ 信道第一条，稳排对话之前）。`-p`/`--json` 没有 TUI，
> stderr 仍是对的去处 —— 且**绝不能**混进 stdout 的逐行 `Envelope`（`tests/degraded_search.rs` 钉住）。
>
> **`/v1/models` 拉取已移到后台（2026-10-12）**：起屏不再等网络 —— 拉取在起屏后 `rt.spawn`
> 到后台，目录到手经信道送进 `app_loop` 写回 registry 再刷一次视图（见 §8.2 M4）。故启动期
> 不再有 `Fetching models…` 这类 stderr 提示：没有停顿要解释。
>
> **唯一例外**：`YUSHAN_API_BASE` + `YUSHAN_API_KEY` 直连且**没给** `YUSHAN_MODEL` 时，
> 模型名无从可写死（见 M4），启动期**同步**拉一次该端点的目录取第一条才建得出 model。
> 给了 `YUSHAN_MODEL`、或凭证来自 auth.json 且带着模型名，就完全不走这条路。

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
| D6 | 命令表只有 §6 的 9 条 | 表里有 **13 条**，多出 `/thinking`、`/copy`（后者 §6 表里写「本地」但表头只有 9 行）、**`/skills`**（skills 特性落地后补，**已同时写回 design §6 表**，故不是偏离）与 **`/resume`**（运行期切换会话落地后补，见 §8.2 M19） | `/thinking` 在 §2.1 UI 规格里出现（`/thinking on`），实现把它做成**本地命令**（`Thinking(Option<bool>)`，无参 toggle）；`/copy` 是 §6 表里的「本地」项；`/skills` 见 §8.2 M4；`/resume` 见 `docs/design-final/runtime-resume.md` |
| D7 | 初始化顺序编号为 1.信道 2.sink 3.阻塞 Wiring 4.Agent 5.view₀ 6.spawn（§7） | 实际 1+2.channel/sink → **4.Agent** → **3.阻塞 Wiring** → 5.view₀ → 6.spawn | Agent 构建不阻塞、不依赖 Wiring；先建 Agent 后建 Wiring 不破坏 R7 的承重结论（**阻塞早退发生在 spawn 之前**，`begin_turn` 只在 agent 真跑时发生） |
| D8 | `ys-protocol` 里的 `Envelope` / `LifecyclePolicy`「迁往 ys-protocol」（review R1 建议） | 已迁；`ys-channel` 整 crate 删除 | 见 `core-channel.md` §8 |
| D9 | 设计 §4 说 app 的 `format.rs`「只剩 `format_tokens`」 | app 的 `format.rs` **删除**；`format_tokens` / `format_duration` 落在 `ys-tui-coding/src/format.rs` | 唯一消费者（渲染）已在 TUI crate |
| D10 | review R2「倾向」在 `Outbound` 加 `Diagnostic { level, text }` | **未采纳** —— 采纳 design §6「没有 `Diagnostic` 变体」 | 命令期诊断走 `Output`、库内部走日志文件，通用变体无消费者 |
| D11 | 本篇未写（会话默认新会话特性落地后补） | 初始化顺序：`--resume` 解析 + `pick`/`resolve` 与**读回 meta** 必须排在**模型解析之前**（meta 参与模型选择），且都在建 runtime / 起屏之前（picker 输出走 stderr） | 见 `docs/design-final/session-default-new.md`；ADR-0026 |
| D12 | md 渲染设计 §2.6 / 不变量 3：「量一遍产出行的显示宽度，**任一行** > `inner_w` 即令该条退回 raw 文本」 | 越界判定**只作用于表格行**（行内含框线字符 `│┌┬┐├┼┤└┴┘` 之一即视为表格行；**不含 `─`** —— 分隔线 `---` 是普通连字符） | 库的 `Options::table_width` **只 wrap 表格单元格、不 wrap 正文**（`options.rs:104-108`；`table_width` 全库仅 `renderer/mod.rs:91` 一处消费）⇒ 按「任一行」判会让任何长于窗宽的正文段落把整条回答退成纯文本，**特性静默失效** |
| D13 | md 渲染设计 §2.4：「`build_text` 每帧被调 2 次（`max_scroll_offset` 一次 + `draw_transcript` 一次）」 | 帧路径 `build_text` **每帧只被调 1 次**（`draw_transcript` 内），其后的 `line_count` 与它基于**同一份**产物 | `ui` 顶部取一次 `now` 只传给它下面的绘制链；`total_rows` / `max_scroll_offset` **只服务事件路径**（↑↓ 滚动 / resize 夹取 `scroll_offset`），不在帧内调用 |

### 8.2 design.md 未写、实现时补的微决策

| # | 决策 | 落点 |
|---|---|---|
| M1 | **`resolve_prompt` 纯函数**：模态命令的决策逻辑（问什么、按什么顺序、取消怎么办）与渲染分离 | `ys-tui-coding/src/prompter.rs` |
| M2 | **`TuiPrompter` 用嵌套事件循环**把问答画成模态浮层（不切屏、不 suspend/resume） | `ys-tui-coding/src/run.rs` |
| M3 | **`/login` 的 api_base 解析**：TUI 在所选 provider **没有内置 base** 时（`custom` 这类）**追加一问 URL**，随 `Request::Login { api_base: Option<String> }` 交给 app；app 侧按 ① 用户显式 URL → ② provider 内置 base → ③ 回退 `config.api_base`（`YUSHAN_API_BASE`）→ ④ 都没有则**明确提示**要设环境变量（不静默失败、不写空 base 的 auth）。**协议因此比 design §6 多一个 `api_base` 字段**（初版实现漏了它，导致 `custom` 的交互式 URL 失效——已修） | `ys-tui-coding::prompter::resolve_prompt` + `capabilities::login` |
| M4 | **`/model` 无参的模型列表来自 `CodingView.available_models`** —— 由**后台拉取**的目录填充：`main` 在起屏**前**只从 registry 摘出要拉的 `(api_base, api_key)`（`fetch_targets`：每个已登录 provider + env 直连端点），起屏**后** `rt.spawn` 一个后台任务 `ProviderRegistry::fetch_all`（`JoinSet` 并发，5s 超时），结果经 `mpsc` 送回 `app_loop`，由它 `apply_model_cache` 写回 registry 并**自发刷一次视图**（无 `Request`）。**模型名只有「该端点 `/v1/models` 的目录」这一个来源**（`/login` 也拉一次该 provider 的目录取第一条当起始模型）—— 代码里没有任何写死的模型名，拉不到就没有那一家，**不做静态回退**。候选 = **所有已登录 provider 已拉到的目录** + env 直连端点按 base 取的目录，**按 `(provider, model)` 去重**（同名 id 跨 provider **各占一行** —— 按 id 去重会把其中一条吃掉，用户既看不到也选不到）+ 当前正在跑的模型。每条是 `ModelEntry { provider, model, current }`：浮层显示 `provider/model`，归属未知（无 `cfg.provider`，如 env 直连）时退化为裸模型名，当前那条缀 `[current]`。**正在跑那条的归属取 `cfg.provider`（权威）**，不靠反查 —— 接线器的 model 就是用它那套凭证建出来的。**起屏不再等网络**（开屏即用；唯一例外是 env 直连且未给 `YUSHAN_MODEL`，见上）；**渲染路径一次 HTTP 都不发**（重绘不得触发 HTTP）—— registry 的写与 `build_view` 的读同在 app 循环那一线程，是唯一写者 | `apps/coding-agent/src/{provider,view,main,app_loop}.rs` |
| M9 | **跨 provider 选模型**：候选是多个 provider 的并集，故归属**显式带过来** —— `Request::SetModel` 带 `provider: Option<String>`（浮层选中时由 UI 带上；`/model provider/model` 限定名由 `capabilities::set_model` 拆出，**仅当 `/` 前逐字命中已知 provider 名**才拆，否则网关那种 `vendor/model` 形态的 id 会被误切）。**只有裸模型名**才回退 `ProviderRegistry::provider_for_model` 反查 —— 它遍历 `model_cache`（`HashMap`，迭代顺序不确定）返回第一个含该 id 的 provider，**同名 id 的归属是任意的**，会静默把端点切到错的一家。归属与当前 provider 不同则**连端点一起切**（api_base / api_key / provider，取自该 provider 的 auth 条目）。目录里都没有的手敲名字**保持当前 provider 不变** —— 只改 `config.model` 会把 A 家的模型名发到 B 家的 base | `apps/coding-agent/src/capabilities.rs`；`crates/ys-protocol/src/request.rs` |
| M5 | **`Action::Abort` 目前是保留变体**：`parse()` 不产出它；Ctrl+C / Esc 在 `handle_key` 里直接投 `Boundary::Abort`。留作将来 `/abort`。turn 中 Ctrl+C 是**两段式**（第一次取消、第二次退出），见 M17 | `ys-tui-coding/src/commands.rs` |
| M6 | **`app_loop` 的回合失败只落日志、不刷 transcript**：`BasicLoop` 返回错误前已发 `RunFailed`（UI 已看到 Error 行） | `app_loop::run` |
| M7 | **`/new` 后 `session_started` 重置**（状态行时长据此归零） | `app_loop::run` |
| M8 | **`export` 为同步 `std::fs::copy`**：会话是本地小文件；把一个非 `Sync` 的 `&Wiring` 借进 app 循环的 future 跨 `await` 会让整个循环不再 `Send`、无法 `spawn` | `capabilities::export` |
| M10 | **`/skills` 命令（skills 特性落地后补）**：TUI 浮层列全部 skill（`/command  — description`，按 `command` 升序、**排除被内置命令遮蔽的**——与 `/` 浮层同一条规则），选中后**回填输入框** `/<command> `（尾随空格）而**不发 `Request`**：skill 常带参数（`/git-status --short`），用户补完再回车；`-p` / `--json` 无 TUI，不涉及此路。为此 `resolve_prompt` 的返回类型由 `Result<Request, _>` 改为 **`Result<PromptOutcome, _>`**（`PromptOutcome::Request` \| `PromptOutcome::Prefill(String)`），`run_prompt` 据此分支「发送」或「回填输入框」。选项文本与 `/` 补全**共用** `completion::skill_label` —— `Prompter::select` 只回传**显示串**，反解回 `command` 靠的就是这条格式（**故它是承重的**） | `ys-tui-coding/src/{commands,prompter,run,completion}.rs` |
| M11 | **`/new` 惰性不预建会话文件**：默认新会话 + 显式 `--resume` 落地后 `latest_session_path` 已删，「预建空文件以免重启选回旧文件」的旧理由失效 —— `/new` 只换 `session` 句柄，新文件首次 `append` 才落盘（旧文件保留） | `apps/coding-agent/src/{wiring,capabilities}.rs`；见 `docs/design-final/session-default-new.md` |
| M12 | **`chat_block() -> Block<'static>`**：Chat 内区统一取自同一 `Block` 实例的 `inner(area)` —— `max_scroll_offset_at` 与 `draw_transcript` 共用同一构造，杜绝各处手写 `saturating_sub(2)` 漂移（`inner_h` 漏改是合法 `usize` 减法、编译器不抓） | `ys-tui-coding/src/draw.rs` |
| M13 | **表格行判定 `is_table_line`**：行内含 `│┌┬┐├┼┤└┴┘` 任一即视为表格行（**不含 `─`**）；raw 路径按 `\n` **原样切行、无样式、不预折行**（正文折行归消费方 `Paragraph`，预折行会把单词 / URL 从中间截断）。`tui_markdown::` 路径**只出现在** `md.rs` 与 `styles.rs`（可 grep 断言） | `ys-tui-coding/src/md.rs` |
| M14 | **缓存 + 节流状态落在 `RenderCache` 内**：键 `(槽下标, src.len(), width)`；`now: Instant` **由调用方注入**（缓存内不调 `Instant::now()`，否则无法确定性单测）；`RENDER_THROTTLE = 100ms`；单条 > `MAX_CACHED_SRC`(256 KiB) 渲染进 `scratch` 暂存并返回、**不写 slot**（真的不入缓存）；`finish(idx)` 由 `App::finish_turn` 调用，而 `finish_turn` **只在 `RunFinished` / `RunFailed` 两个分支**被调 —— **不是每帧**（每帧调会让每条消息每帧整条重解析，缓存完全失效） | `ys-tui-coding/src/{cache,app}.rs` |
| M15 | **panic hook 的 `panic_hook_body<T>(restore, delegate, info)` 泛型化**：泛型 `T` 是为**可单测**（`PanicHookInfo` 无法在测试里构造，单测传一个假 `T` 即可断言「先恢复、后 delegate」）；`restore_sequence()` 由 `exit_payload`（正常退出）与 panic hook **共用**（四条命令各改一个互不相干的模式位；唯一先后约束是「退 alt-screen 必须在回放段之前」—— 回放落点靠 `?1049l` 恢复） | `ys-tui-coding/src/run.rs` |
| M16 | **绘制路径不得 panic**：`push_non_assistant` 的 `Assistant` 分支从 `unreachable!()` 改为**降级为 raw** + `debug_assert!(false, …)`（测试期大声、生产期降级）—— 绘制 panic 会把终端留在 raw mode（§14.7）。该分支正常不可达（两个调用点都提前分流 Assistant） | `ys-tui-coding/src/draw.rs` |
| M17 | **turn 中 Ctrl+C 两段式**（ADR-0031）：第一次投 `Boundary::Abort` + 写一行 transcript 回执「再按一次 Ctrl-C 退出」；第二次 `should_quit = true`（与 Ctrl-D 同一条退出路径）。标记 `App.abort_requested` **随回合生命周期复位**（进 turn 的三处 + `finish_turn`），否则上一回合按过的 Ctrl-C 会让下一回合的第一次直接退出 | `ys-tui-coding/src/{events,app}.rs` |
| M18 | **退出后打 `session: <id>`**（ADR-0032）：TUI 退出（alt-screen 已恢复、transcript 已回放）后由 `main` 往 **stderr** 写一行 `session: <id>`，与 `-p` / `--json` 同文案同去处；只在**已落盘**时打（惰性新会话没跑过回合就没有文件）。会话号取自 `app_loop::SessionPathHandle` —— 启动态由 `run_interactive` 从 `Wiring` 播种、`/new` 后由 app 循环刷新，**不等 app 线程收摊**（长回合中途退出时正卡在模型调用里，等就取不到） | `apps/coding-agent/src/{main,app_loop}.rs` |
| M19 | **`/resume` 运行期切换会话**（ADR-0033，取代 ADR-0026 决策 6）：无参 → 浮层（候选挂 `App.pending_sessions`，由 `resolve_session_pick` 决策、标签含**完整 id** 以供反解、Esc 取消、空列表只提示不弹浮层）；有参 → 直切。app 侧 `resume_session` **可失败步全部前置**（解析 → 读 meta → 纯解析模型四字段 → 构建候选 model），`wiring.resume_into` **open 成功才替换**句柄（失败 ⇒ 当前会话全不变）。UI 侧回放经 `Outbound::SessionResumed` → `reset_for_replay()` + `replay_lines()`，**必须显式 `RenderCache::reset()`**（回放是本 app **第一个**清屏/缩短 transcript 的操作）。启动期 `--resume` 复用同一通路、顺带修掉空屏（出站顺序 `SessionResumed` 先于启动提示）。**`-p` / `--json` 不受影响**（不收 `Outbound`） | `ys-protocol/src/{session,request,lib}.rs`；`ys-tui-coding/src/{cache,transcript,commands,prompter,app,run,lib}.rs`；`apps/coding-agent/src/{wiring,capabilities,main,app_loop,sessions}.rs`；`apps/coding-agent/tests/resume_cli.rs`；见 `docs/design-final/runtime-resume.md` |
| M20 | **模态选择器带常驻筛选输入框**：`select_modal`（`run.rs`）在候选列表**之上**加一行输入框 —— 打开即可敲字，大小写不敏感**子串**匹配**整条候选串**，↑↓ 在**过滤后**的列表里移动，Enter 确认（**无匹配时不动作**，取消的唯一出口是 Esc），Esc 取消；`Backspace/Delete/←→/Home/End` 与 `Paste` 编辑筛选串。空查询 ⇒ 过滤结果 = 全体 ⇒ 行为与「纯列表」逐字一致。四个模态命令（provider / model / skill / session）**共用** `select_modal`，故筛选对它们一并生效。两条纯函数（`filter_indices` / `clamp_selection`）与 `modal_move` 并列抽出，理由相同：模态循环要真终端，语义只能靠纯函数钉进单测 —— 尤其 `clamp_selection`，查询收窄后不收敛 `selected` 会让 `filtered[selected]` 越界 panic。**筛选框为空时显示占位文案「输入以筛选…」**（可筛选这件事在输入框里说，**不进标题** —— 标题塞提示会把框撑到 60+ 列，窄终端下反被裁掉） | `crates/ys-tui-coding/src/{run,draw}.rs` |

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
| `crates/ys-tui-coding` | 242（**当前值**） | 三 pane `TestBackend` 渲染、状态行四档宽度、输入区（多行 + 粘贴 + 中文退格）、补全浮层、命令表逐条映射、`resolve_prompt` 决策路径、`FakePrompter`、`/skills` 选择器（回填 / 遮蔽过滤 / 取消）；**本轮 md 渲染 +30**（逐项见 §9.4） |
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
- `/skills` 选中改回「直接发 `Request::Prompt`」→ `test_pick_skill_prefills_slash_command_with_trailing_space` 变红（它断言的是 `PromptOutcome::Prefill`）。
- `/skills` 候选取消 `selectable_skills` 的内置遮蔽过滤 → `test_pick_skill_excludes_skill_shadowed_by_builtin_command` 变红。

### 9.4 本轮 md 渲染：新增用例（+30）与变异

**新增 30 条单测全部落在 `ys-tui-coding` lib**（其它 crate 用例数不变）：212 → 242；
workspace 975 → 1005 passed，ignored 7 → 8（新增的 1 个 `ignore` 是 `cache.rs` 中
`RenderCache::lines_for` 的文档测试）。

| 模块（新/改） | 条数 | 用例 |
|---|---|---|
| `cache.rs`（**新**） | 9 | `pure_memo_rerender_is_identical` / `same_key_within_window_reuses` / `len_change_forces_rerender_after_window` / `width_change_forces_rerender` / `different_idx_are_independent_slots` / `sync_len_growth_finishes_previous_line` / `explicit_finish_forces_rerender` / `oversized_src_is_not_cached` / `n7_streaming_throttle_bounds_calls_and_finishes_complete` |
| `md.rs`（**新**） | 11 | `n3_heading_levels_…` / `n3_bold_italic_inline_code_link` / `n3_blockquote_list_fenced_code_table` / `n3_no_default_palette_leak` / `n5_table_never_exceeds_width` / `n6_non_table_has_no_vertical_border` / `edge_inputs_do_not_panic_and_table_stays_within_width` / `empty_input_yields_no_overwide_lines` / `long_plain_line_is_not_prewrapped` / `narrow_width_long_prose_keeps_markdown_styles` / `narrow_table_falls_back_to_raw` |
| `styles.rs`（**新**） | 2 | `overrides_do_not_match_default_palette` / `chosen_shapes_are_as_designed` |
| `draw.rs`（**改**） | 6 | `n4_line_count_equals_per_line_accumulation` / `n6_chat_inner_has_no_vertical_border_for_non_table_md` / `n8_plain_lines_contain_raw_markdown` / `geometry_top_border_inner_is_h_minus_1_by_w` / `geometry_follow_keeps_last_line_and_scroll_matches_line_count` / `throttle_consistency_scroll_and_draw_share_product` |
| `run.rs`（**改**） | 2 | `restore_sequence_contains_terminal_state_restore` / `panic_hook_body_restores_then_delegates` |

**变异义务（六条 + U3 的 panic hook 变异）—— 全部演示「注入 bug ⇒ 用例变红」**：

- **M1** 关掉表格窄屏守卫（`if false && …`）→ **2 条**用例变红：`n5_table_never_exceeds_width`
  与 `narrow_table_falls_back_to_raw`。
- **M2** 让缓存永不失效 / 命中键去掉 `width` → `width_change_forces_rerender` 变红。
- **M3** 漏覆写一个 `StyleSheet` 方法（删 `fn link`）→ **3 条**用例变红：`overrides_do_not_match_default_palette`、
  `n3_bold_italic_inline_code_link`、`n3_no_default_palette_leak`。
- **M4** 内区改回 `saturating_sub(2)` / `h-2` → **3 条**用例变红：`geometry_follow_keeps_last_line_and_scroll_matches_line_count`、
  `test_scroll_uses_wrapped_rows_not_transcript_entries`、`throttle_consistency_scroll_and_draw_share_product`。
- **M5** 摘掉节流、改回逐 delta 重渲染 → `n7_streaming_throttle_bounds_calls_and_finishes_complete` 变红。
- **M6** 把守卫判定放宽回「任意行」（去掉 `is_table_line`）→ `narrow_width_long_prose_keeps_markdown_styles` 变红。
- **U3 的 panic hook 变异 A–D**（先 delegate 后 restore / 删 `LeaveAlternateScreen` / 删 `restore()` / 删 `delegate()`）
  均变红；**变异 G**（删 `restore_sequence()` 的 `PopKeyboardEnhancementFlags`）在补断言后由「存活」转为被捕获。

---

## 10. 已知缺口（诚实清单）

- `/compact` **已接真压缩**（`ys_loop::plan` + `execute`）：摘要成一条 checkpoint、尾部保留；
  无 model / 无事可压 / 摘要失败三条路径**都不动 session**，只给一行提示。（此条原为「占位」缺口，已闭合。）
- `/copy` 无剪贴板集成（v1 把内容写进 transcript，手动选中）。
- `/new` 后 `App.transcript` 不清（旧对话行仍在屏上；`CodingView.message_count` 已归零）—— 与
  `core-channel.md` §7.7 记录的同一行为，本版未改。
- `-p` / `--json` 无 steering 生产者：轮边界机制代码路径完整、有单测与跨 crate 集成覆盖，
  但 CLI 形态下无人投 `Boundary`（交互 TUI 才用得上）。
- `ys-tui-coding` 的模态浮层与补全浮层共享 `Clear` 后覆盖渲染，未做统一的 `Overlay` 抽象
  （design §1 注允许二选一）。
- **长程完成（ADR-0027 / 0028）本轮的债务**（未校准初值 / 设计层缺口 / 复核层残余）见 §12。

**md 渲染（§14）的诚实清单**：

- **字面 `│`(U+2502) 会被误判为表格行**：正文里若出现字面 `│` 且该行超窗宽，会整条退 raw ——
  **设计有意接受**（代价极低；ASCII `|` 不触发）。
- **「整条退」的粒度**：文档里只要含**一张窄表**（渲染后某表格行超窗宽），**整条消息（含其中正文）**
  都退 raw（设计即「整条退」，不做逐行降级）。
- **缓存命中时每帧仍 clone 一遍** `Vec<Line>`：100 KiB 量级约 0.3–0.6 MB/帧（设计明写「缓存省的是解析、
  不是拷贝」）。
- **`n6_non_table_has_no_vertical_border`（`md.rs` 层）近乎恒真**：库唯一产 `│` 处是表格渲染，故该断言
  对非表文档天然成立；真判据在 draw 层集成版 `n6_chat_inner_has_no_vertical_border_for_non_table_md`。
- **7 个 `StyleSheet` 方法本仓配色与默认相同**（`table_cell` / `definition_description` / `alert_label` 等）⇒
  「默认色泄漏」在它们身上**不可观测**（漏写也不产生差异）—— 已在测试注释与文档中标注，**未假装覆盖**。
- **panic hook 里 `disable_raw_mode()` / `Show` 无字节断言**（系统调用 / 无字节）⇒ 归**代码走查**；
  单测只覆盖恢复序列的字节与「先恢复、后 delegate」的顺序。
- **已知库缺陷（`tui-markdown` 0.3.10 实测）**：**4 空格缩进式**代码块会把多行内容**合并成一行**
  （围栏式不受影响）。
- **resize 后该帧全量重算**（库把解析与表格布局合在一次调用里，无法只重排表格）—— 一次性抖动；
  库自述 experimental PoC、0.3.x API 未冻结 ⇒ 库被**隔离在 `md.rs` 这层薄胶水**（将来可换）。
- **panic hook 写的是 `stdout`，而 std 的 stdout 是 `ReentrantLock`**：若 panic 恰好发生在当前线程
  **已持有 stdout 锁**的时刻，hook 内的写会触发「cannot recursively lock stdout」而**二次 panic ⇒ abort**。
  实际绘制路径下该窗口很窄 —— `draw::ui` 的闭包在 ratatui 缓冲阶段执行，那时 stdout **未加锁**，
  故 markdown 解析 panic 这条主线不受影响；只有 std/crossterm 内部在 `write_fmt` 里 panic 才可能命中。
  彻底消除须改为直写 `/dev/tty`（引入平台分支），当前**接受**此残余。

**MANUAL_ACK_REQUIRED（须由用户勾选）**：

- [ ] 在真实终端喂一条 ≥100 KiB 的长回答（含表格），确认滚动与输入无可感卡顿（N11）
- [ ] 确认 Chat 窗格去掉左右边框后的观感、以及表格极窄时退回 raw 的画面可接受

**未能机器验证的清单**：panic hook 的真终端恢复路径本身（`disable_raw_mode()` / 转义序列是否真的复位终端）、
N11 卡顿观感、Chat 去边框后的整体观感、库对「4 空格缩进式代码块合并多行」在真实输出中的表现。

---

## 11. 长程完成（ADR-0027 / 0028）— as-built

三条生产改动：① `max_rounds` 可配 + **wall-clock 预算**；② 一次性**完工复核**；③ 适配器模型调用**超时 + 有界重试**。
**零 `ys-protocol` 改动、零 `BasicLoop` 改动、零 core crate 改动**（复核的 UI 呈现只改 `ys-tui-coding` 的渲染分支）。

### 11.1 配置层：四个取数口（`apps/coding-agent/src/config.rs`）

| 取数口 | `config.json` 键 | 缺省 | 校验 |
|---|---|---|---|
| `Config::max_rounds() -> u32` | 顶层标量 `max_rounds` | `u32::MAX`（= 不设） | 正整数（`min = 1`） |
| `Config::max_turn_duration() -> Duration` | 顶层标量 `max_turn_duration_secs` | `172_800`s（48h） | 正整数（`min = 1`） |
| `Config::model_retry() -> ModelRetryConfig` | 对象段 `model_retry` | `{ max_attempts: 5, base_delay_ms: 500, max_delay_ms: 30_000 }` | 三项**逐字段独立**，允许 0 |
| `Config::model_timeout() -> ModelTimeoutConfig` | 对象段 `model_timeout` | `{ connect_secs: 15, idle_secs: 120 }` | 两项**逐字段独立**，`min = 1` |

- 校验走共用的 `sanitize_uint`（`as_u64` 挡下负值 / 浮点 / 字符串 / 布尔 / 对象 / 数组）：非法值**只丢该条 + 落日志**，同文件其余条目照常生效；未给静默回落默认。重试三项取 `min = 0`（`max_attempts: 0` = 不重试、延迟 0 = 不退避是合法意图）。
- `max_rounds` **刻意与库默认分离**：`RunLimits::default().max_rounds` 保持 `10`（库的保守默认），app 的产品默认是 `u32::MAX`（不设）。
- `model_retry` / `model_timeout` 是**回合 / 进程属性**，落顶层对象段，**不做「按 model 的 map」**（同 `max_rounds` 的理由：重试 / 超时不属于某个 model 的方言）。

### 11.2 limits 注入（`apps/coding-agent/src/main.rs`）

- `resolve_limits` 只注入三行：`context_window` / `compaction` / `max_rounds`（本轮新增最后一行）。**`model_retry` / `model_timeout` 不经 `resolve_limits`** —— 它们是适配器构造参数，经模型工厂 `Config::set_model_factory`（定义于 `config.rs`，由 `main.rs` 调用；`with_retry(cfg.model_retry())` / `with_timeouts(cfg.model_timeout())` 的调用在 `main.rs` 的工厂闭包内）显式传入（生产路径**总是**显式调用，故生效的是 app 侧常量）。
- `resolve_limits` 是 TUI / `-p` / `--json` 三入口**唯一** limits 接线点，一处修复三模式。

### 11.3 完工复核缝 —— **已撤除**（ADR-0041）

原先这里是一个「模型自称完工后追问一次」的第二个回合（`follow_up.rs` 的 `should_review` +
`app_loop` 的 `pending.take()` 循环，三道门 `Initial` / `Completed` / `rounds >= 5`）。

**已整块删除**（`docs/adr/0041-remove-completion-review-turn.md`）。现状：

- **一个 `Request::Prompt` 恰好跑一个回合** —— `app_loop` 的该臂直接 `run_turn_once` 一次，
  随后写穿 meta；`follow_up.rs` 不再存在。
- 撤除的两条理由：① 同类（codex / Pi）都只靠「模型不再发工具调用」收场，无追问回合；
  ② **它只在 TUI 生效** —— `main.rs` 的 `run_single_turn` 里从来没有 `should_review`，
  故基准（`-p`）度量的流程比真实使用少一环，而我们所有性能结论都出自基准。
- **保留**：`run_turn_once` 开头的边界排空（挡的是**上一个请求**残留的 `Abort`，与回合数无关）。

### 11.4 wall-clock 预算（`app_loop` + `main.rs`）

- **`deadline` 在 `Request::Prompt` 入口算一次**（`request_started + config.max_turn_duration()`），覆盖整个 `pending` 循环 —— 主回合与复核回合**共享同一截止点**；时长只来自 `Config::max_turn_duration()`，绝不写死。
- **每回合新建 `QueueBoundarySource`**（`Abort` 永久置位，复用会让上一回合的取消取消之后每个回合）；`run_turn_once` **第一步**排空 `boundary_rx` 残留（防队列里那条已投递的 `Abort` 取消本回合）。
- **计时器臂**：`sleep_until(deadline)` 分支带 `if !budget_exhausted` 前置条件；到点**先**发 `Outbound::Output`（`budget exhausted: wall-clock budget of {secs}s elapsed — aborting this turn`），**再** `boundary.push(Boundary::Abort)`。该回合以 `Cancelled` 收场 ⇒ `should_review` 返回 `None`（预算耗尽不触发复核）。
- **§4.9 可观测行**：每个回合边界经 `logging` 落一行 `turn=… rounds=… elapsed=…s cumulative_rounds=… budget_remaining=…s`（`budget_remaining` 走 `saturating_duration_since`，到点后为 0）。
- **`-p` / `--json`（`run_single_turn`）**：合成 `QueueBoundarySource` + **同一**计时器形状；到点**先**写说明行**再** push `Abort`。说明行走 **stderr** —— 该路径**无 outbound 信道**（`stdout` 承载模型文本增量 / 逐行 JSON 流，必须保持干净）。`single_turn_exit_code`：`Cancelled ⇒ 1`，其余（`Completed` / `MaxRounds` / `Truncated`…）`⇒ 0`；`exit_with_code` 非零时先 `flush` stdout 再 `exit`。

### 11.5 适配器超时 + 有界重试（`adapters/model-openai-compatible`）

- `RetryConfig` / `TimeoutConfig`（`lib.rs`）+ builder `with_retry` / `with_timeouts` / `with_logger`。`with_timeouts` 仅在 client 自建（`new`）时按新值重建（`owns_client` 守卫）；`with_client` 注入的 client 连接策略归调用方。
- **重试循环**（`complete`）：`for attempt in 0..=max_attempts`（`max_attempts` 是**额外**次数，总尝试 = max_attempts + 1）；每次尝试包 `DeltaWatchingSink`。
- **三级可重试判定**（`classify_error_response`，从左到右短路）：① 响应头 `x-should-retry` 定论（不看状态码）→ ② `QUOTA_MARKERS` 额度名单否决（`insufficient_quota` / `quota exceeded` / `out of budget` / `billing` / `usage limit` / `available balance`，大小写不敏感子串）→ ③ 状态码（连接 / DNS / TLS / 超时、`429`、`5xx` 可重；其余 4xx 与解析失败不重）。
- **有界退避**（`backoff_delay` / `backoff_delay_with_fraction`）：指数 + 抖动，**先抖后夹**到 `max_delay_ms`（绝对上界）；`429` 的 `Retry-After` / `Retry-After-ms` 被尊重，但**超过 `max_delay_ms` 硬上限即放弃**（不取「与上界取 max」）。
- **`DeltaWatchingSink`**：只看 `TextDelta` / `ThinkingDelta`；一旦吐出即 `.emitted = true`，其后失败**直接上报、不重试**（重放会把已渲染文本再渲染一遍 —— `-p` 直写 stdout 无法撤回）。
- **重试日志**经注入的 `ModelLogger` 回调（生产接 `crate::logging::log`）：进入退避前落 `retry attempt k/n delay=…ms reason=…`，放弃时另落一行。**重试日志只经该回调落，不经 stdout / stderr**（TUI 起屏后直写会冲掉整屏）；重试**对 TUI 不可见、对排障可见**。
- **超时**：连接建立经 `build_client` 的 `.connect_timeout(connect_secs)`；**流空闲超时**落 `stream.rs` 的 `parse_sse_stream` —— `tokio::time::timeout(idle, stream.next())` 包住每次字节读取，超时错误带 `IDLE_TIMEOUT_MARKER`（`classify_stream_error` 据此归为可重试）。黑洞端点「接受连接但不应答」否则会让 `complete` 永久挂起。

### 11.6 TUI 判别式（零 `ys-protocol` 改动）

- `ys-tui-coding/src/app.rs` 的 `AgentEvent::UserMessage` 分支三段：① 回显抑制（`suppress_user_echo`）→ ② 迟到回声认领（`pending_echo_texts`，以 `!is_turning` 为闸）→ ③ **判别式**。
- **判别式只读 `is_turning` 的真假，绝不按内容匹配**（不嗅探文案是否为复核提示）：
  - `is_turning == true` ⇒ 轮边界 steering 插话 ⇒ `TranscriptLine::User`；
  - `is_turning == false` ⇒ app 发起的**复核回合**（主回合已 `RunFinished`，`finish_turn` 已复位）⇒ `TranscriptLine::System` 行 + 点亮 `Working`（置 `is_turning = true` + `turn_started_at`）—— 复核同样是一个回合，谁也不是谁的特例。
- 全部改动落在 `ys-tui-coding` 渲染分支，**`ys-protocol` 一字节不改**。

### 11.7 新增测试

| 落点 | 条数 | 覆盖 |
|---|---|---|
| `apps/coding-agent/src/follow_up.rs` | 4 | `Review` 恒不复核 / 非 `Completed` 不续 / 门槛两侧边界 / 复核输入逐字节 `== REVIEW_PROMPT` |
| `apps/coding-agent/src/app_loop.rs` | 11 | 复核正例（主回合 5 轮 ⇒ 恰一个复核回合）/ 纯文本·`Cancelled`·`MaxRounds`·`Truncated` 不复核 / **不问第二次** / 跨回合残留 `Abort` 不取消复核 / 标题不被复核覆盖 / 复核回合不交自己的输入作标题 / wall-clock 到点发说明行 + `Cancelled` / `max_rounds = u32::MAX` 不被核心默认 10 截断 |
| `apps/coding-agent/src/config.rs` | 6 | 四个取数口的采纳 / 非法值逐条丢弃 / 重试·超时逐字段独立 |
| `apps/coding-agent/src/main.rs` | 4（含 1 条更新） | `resolve_limits` 注入 `max_rounds` 与缺省 `u32::MAX` / `single_turn_exit_code`（`Cancelled ⇒ 1`）/ `budget_exhaustion_cancels_single_turn` / 适配器与 app 侧默认常数不漂移 |
| `adapters/model-openai-compatible/tests/retry.rs` | 15 | 回环 TCP：空闲超时上报 / `429` 后成功 / `x-should-retry` 双向定论 / 额度否决 `429` / `Retry-After` 超硬上限放弃 / 吐 delta（含 thinking）后不重 / 确定性 4xx 不重 / 重试日志可见 / 两组默认常数 pinned / `max_attempts = 0` / 注入 client 保超时 |
| `crates/ys-tui-coding/src/app.rs` | 7 | 判别式两侧（`!is_turning` ⇒ `System` 行 + 点亮；`is_turning` ⇒ `User` 行）+ 回显抑制路径不受影响；`pending_echo_texts` 迟到回声认领四条（§11.6 已记为 as-built）：迟到的排队回声不被误标 `System` / 连续排队提交都被认领 / 排队回合保持 `is_turning` / 与复核回合撞车时仍能认领 |
| `apps/coding-agent/tests/budget_exit.rs` | 4 | **子进程级**（真实二进制）：`-p` / `--json` 预算到点非零退出 / 说明行落 stderr / `--json` 的 stdout 每行皆合法 JSON（说明行不污染）/ 正常路径退出码 0 |

---

## 12. 本轮的已知债务（debt 账）

### 12.A 未校准初值

用户裁定「跳过 prototype、按设计初值实现、记账」；本环境**无** `YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`。以下三个初值**待真实环境校准**：

- `REVIEW_MIN_ROUNDS = 5`；
- `model_timeout = { connect_secs: 15, idle_secs: 120 }`；
- `model_retry = { max_attempts: 5, base_delay_ms: 500, max_delay_ms: 30_000 }`。

### 12.B 设计层缺口

- **预算管不住在途模型调用**（`apps/coding-agent/tests/budget_exit.rs` 实证）：`Boundary::Abort` **只在轮边界被 `BasicLoop` 读取** —— Step 1 入口（`crates/ys-loop/src/basic.rs:137`）、Step 3 模型调用前（`basic.rs:174`）、轮边界 `take()`（`basic.rs:292`，`Abort` 分支在 `:303`）；而**模型调用发生在 Step 6**（`basic.rs:348`）。若该调用不返回，预算到点推入的 `Abort` **不会被观察** ⇒ 该调用实际由**适配器的流空闲超时**（`stream.rs:123` 的 `tokio::time::timeout(idle, …)`，默认 120s）界住。预算不打断在途模型调用；最坏 = **一次在途模型调用**（由该空闲超时界住）。
- **`-p` / `--json` 说明行落点与计划不符**：exec-plan 写 `Outbound::Output`，实际走 **stderr**（`main.rs::run_single_turn`）—— 因为该路径**无 outbound 信道**；这是「stdout 承载模型增量 / JSON 流必须保持干净」的必然结果（`tests/budget_exit.rs` 用例 2 钉住）。
- **记账**（指向既有 ADR，不展开）：细阀换粗阀（`MAX_CONSECUTIVE_ERRORS` 空分支仍在 —— `basic.rs:14` 常量、`basic.rs:517` 空体只有注释）；`Cancelled` 三义（预算到点与 Ctrl+C 同态，靠一行说明区分）；退避不可中止（`Model::complete` 无取消入参）；重试治抖动不治停服（>~150s 故障仍终止回合）；会话 `append` 每次全量重写 JSONL（48h ≈ 10⁵ 次全文件重写）。

### 12.C 复核层残余

- **错误路径下 `rounds` 记 0**：`app_loop` 的 §4.9 可观测行取 `rounds = match &outcome { Ok(result) => result.rounds, Err(_) => 0 }`（`app_loop.rs:372-375`）；`Err` 路径下 `rounds` 记 0，**低估**真实轮数（`cumulative_rounds` 因此也少加）。仅影响该排障行，不影响执行。
- **同一 poll 内计时器至多一条说明行**（核实结论，**非**债务）：`budget_exhausted` 在计时器分支体内**先**同步置位（`app_loop.rs:350`）**再** `await` 发送，`select!` 的 `if !budget_exhausted` 前置条件随即禁用该分支 ⇒ 说明行至多一条、`Abort` 至多 push 一次。`-p` / `--json` 的 `run_single_turn` 同形。

---

## 13. subagent 派发（ADR-0022 / 0029）— as-built

形态决策见 [ADR-0022](../adr/0022-subagent-dispatch.md)（accepted），落地裁定见 [ADR-0029](../adr/0029-subagent-landing-decisions.md)（accepted）。本节只记**实际形态**。

### 13.1 装配（启动期一次，之后冻结）

- **父工具表 = 基础 9 元 + `SubagentTool` 追加末位**（`main.rs`：`toolset::build_tools` 之后 `tools.push(Box::new(subagent::SubagentTool::new(setup.outer_timeout())))`）。末位保证**子 tools JSON 是父的字节前缀** ⇒ 命中 prompt cache。
- **子工具表 = 基础 9 元**（无 `subagent`）⇒ 派发**结构上不可能递归**（`registry.get("subagent")` 未命中 ⇒ 走 `tool not found`）。
- **两份系统提示**：父（10 工具，能力边界声明改为「CAN delegate tasks to subagents」）、子（9 工具，逐字保留「no subagents」）。由 `prompt::build_system_prompt` 按**工具表派生**，不是常量。
- **子 `RunLimits`**：与主 agent 同源（同一份 `Config`），但 `fixed_prompt_tokens` 填**子表**的真值（否则子会话系统性晚触发压缩）。

### 13.2 每回合的注入与运行

- 三条入口（TUI `app_loop` / `-p` / `--json`）**每回合**造一份 `AppSubagentRunner`（派发计数器因此天然 per-turn），经 `AgentPorts::with_subagent` → `RuntimeContext.subagent` → `ToolContext.subagent` 三跳送达工具。
- 一次派发：建**全新子 `Agent`**（`Agent::run_turn` 取 `&mut self`，实例不可共享） + `MemorySession` + `NoopEventSink`（**子事件不进父事件流**）+ `ChildBoundary`。
- **取消**：`ChildBoundary::take()` 只取自己那份、`is_aborted()` = 自己 **或** 父 —— 父 Abort 经探针向下传播，子吃不到父的 `Steer`。
- **子预算**：watchdog（`tokio::spawn(sleep(budget) → 子边界源 push(Abort))`，用 **`AbortOnDropHandle`** 防孤儿任务）；子预算与主 agent 同值 ⇒ 真实天花板是父回合那条 deadline。
- **超时**：`Tool::timeout()` 的优先级链（工具声明 → `bash_timeout` → 300s）；`subagent` 的外闸 = 子预算 + 60s 宽限，**构造期由 `Config` 注入**（ADR-0029 修正 2）。

### 13.3 结果与产物

- **回传**：摘要硬截断 ≤ 8 KB（`MAX_SUMMARY_BYTES`，UTF-8 安全）+ 完整转录落 `~/.yushan/subagents/`（`YUSHAN_SUBAGENTS_DIR` 覆盖口；Primary 档启动期 sweep 7 天）+ **`## Files` 节由 runner 保证存在**（子不报写 `(none)`）。
- **失败**：子撞 `max_rounds` / 被取消 ⇒ `Ok` + 非 `Completed` 的 `stop_reason` + `diagnostic` ⇒ 父侧 `ToolResult{is_error}`，**父回合不失败**。只有「跑都跑不起来」（如 `model` 名构造失败）才让 `run` 返回 `Err`。
- **`model` 参数**：按名构造（`Config::build_model_for`）+ `Arc<dyn Model>` 缓存（复用连接池/TLS）；构造失败 **fail-closed**，绝不静默回退父模型。
- **usage 上卷**：子 usage 累加进 `Arc<UsageTotals>`，回合末 drain 并追加进 `RunResult.usage` ⇒ `TurnStats` / meta / 退出码看到真实总消耗。

### 13.4 并行批（Step 9）

- 准入：批次 `len > 1` **且每一个**工具都 `is_concurrency_safe()` ⇒ 走 `futures_util::stream::iter(..).buffered(max_parallel_tools)`（**保序**、单任务内并发、不需 `'static`）；否则**原串行路径**。
- 事件顺序：并行批先保序 emit 全部 `ToolCall`、执行、再保序 emit 全部 `ToolResult`；**串行批保持逐个 `C1,R1,C2,R2`**（既有行为逐字节不变）。默认 `is_concurrency_safe() == false` 是这条的守卫；今天只有 `subagent` 覆写为 `true`。

### 13.5 已知缺口（承接 §10/§12 的口径）

- 子 agent 内部活动在**任何输出通道都不可见**（`--json` 亦然），只有落盘转录可查 —— ADR-0022 决策 2 的少数派偏离。
- 并行子 agent 的**写冲突无人管**（本仓无 branch/worktree、无沙箱），只能靠 guideline「写目标不相交」。
- 每回合派发总量上限（`DEFAULT_MAX_DISPATCHES_PER_TURN = 16`）是**仅剩的代码级成本防线**：`max_rounds` 默认已放宽到 `u32::MAX`，且子取同一份 `Config`（子自身无独立上界）。
- ⚠️ **`--json` 的 `RunFinished.usage` 不含子 agent usage**：子 usage 是在 app 侧、回合**结束后**追加进 `RunResult.usage` 的（`merge` 点在 `app_loop::run_turn_once` / `main::run_single_turn`），而 `RunFinished` 由 `BasicLoop` 在回合内发出 ⇒ **事件流里的 usage 只算父**。TUI 的状态行（`TurnStats`）与 `-p` 的 meta 走的是合并后的 `RunResult.usage`，**是正确总数**。⇒ 脚本消费者若要精确总成本，需自行减去或改用 meta；这是「循环拥有 `RunFinished`」这一结构的直接代价，记在此处。

---

## 14. assistant 文本 Markdown 渲染 — as-built

形态决策见 [ADR-0030](../adr/0030-tui-markdown-render.md)（accepted）；设计与风险见
`docs/arch/tui-markdown-render/design.md`。本节只记**实际形态**（「实现 ≠ 设计原文」的两条已入 §8.1 D12 / D13，
微决策入 §8.2 M12–M16）。

### 14.1 依赖与渲染管线

- **依赖**：`tui-markdown = { version = "0.3.10", default-features = false }` —— 关掉 `highlight-code`
  ⇒ `syntect` / `ansi-to-tui` / `onig`（C 库）**全不进入**依赖树。前置已升级 `ratatui 0.30` + `crossterm 0.29`
  （否则库的 `ratatui-core 0.1.2` 与本仓 `Text` 是**不同类型、不可互传**）。
- **管线**：`Assistant(src)` → `md::render(src, inner_w, AssistantStyles) -> Vec<Line<'static>>`
  → `Paragraph::new(..).wrap(Wrap { trim: false })`。
- `md.rs` 是库的**唯一调用点**：把库返回的 `Text<'a>` 收成 owned `Vec<Line<'static>>`（缓存需要 owned），
  保留 `Line::style` 与 `Line::alignment`（库会写这两者）。
- `styles.rs`：`impl tui_markdown::StyleSheet`，**23 个方法逐一覆写** —— `DefaultStyleSheet` 自带主观配色，
  漏写会把默认色漏进来。粗体 / 斜体由库硬编码为纯修饰符、**不可配色**，本仓本就要 Strong=BOLD / Emphasis=ITALIC（无颜色）。
  `code()`（行内码 + 围栏代码块）**只用单一 `cyan` 前景色，不分语言**（2026-10-10 修订）——
  整块底色会在长代码块上糊成一大片、并给缩进染色，故不用底色；也不引 `highlight-code`（`syntect` + `onig_sys` C 库）。
- `tui_markdown::` 路径**只出现在** `md.rs` 与 `styles.rs`（可 grep 断言；库成熟度风险的缓解）。

### 14.2 表格窄屏守卫（只作用于表格行）

- 库的 `Options::table_width` **只 wrap 表格单元格、不 wrap 正文**（`options.rs:104-108`；`table_width`
  全库仅 `renderer/mod.rs:91` 一处消费）⇒ 长于窗宽的正文段落本身就是一条超宽行。
- **当前实现**：越界判定**只作用于表格行** —— 行内含框线字符 `│┌┬┐├┼┤└┴┘` 之一即视为表格行
  （**不含 `─`** —— 分隔线 `---` 是普通连字符）；命中则**整条**退回 raw。
- **raw 路径**：按 `\n` 原样切行、无样式、**不预折行**（正文折行归消费方 `Paragraph`；预折行会把单词 / URL
  从中间截断）。
- 反之：若按「任一行超宽即退 raw」判，任何长于窗宽的正文段落都会把整条回答退成纯文本 ⇒ 特性静默失效
  （§8 D12）。

### 14.3 缓存与流式节流

- `App.md_cache: RefCell<RenderCache>`；键 `(槽下标, src.len(), width)`；**只覆盖 `Assistant` 槽位**
  （其余变体每帧现算，O(1) 格式化、不解析）。
- `RENDER_THROTTLE = 100ms`；`now: Instant` **由调用方注入**（缓存内不调 `Instant::now()`，否则无法确定性单测）。
  正在增长的那条：距上次渲染 < 100ms 时沿用上次产物，到点或该条结束时重算。
- 节流状态**存在 `RenderCache` 内** ⇒ 滚动数学（事件路径）与画面（帧路径）读到**同一份**上次产物；
  两路径一致性**不靠共用时钟**。
- 单条 > 256 KiB（`MAX_CACHED_SRC`）：渲染进 `scratch` 暂存并返回，**不写 slot**（真的不入缓存，退化每帧现算）。
- `finish(idx)` 由 `App::finish_turn` 调用，而 `finish_turn` **只在 `RunFinished` / `RunFailed` 两个分支**被调
  —— **不是每帧**（每帧调会让每条消息每帧整条重解析，缓存完全失效）。
- **借用纪律**：`lines_for` 返回切片绑在 `RefMut` 守卫上，须在守卫内 clone 进目标，不得带出、不得重入 `borrow_mut`。

### 14.4 几何改道（拷贝友好）

- Chat 窗格 `Block::bordered()` → `Block::new().borders(Borders::TOP)`（**只留顶部标题线**）——
  终端原生选择不会再把窗格竖线拷进剪贴板。
- 内区**统一取自同一个 `Block` 实例**：`fn chat_block() -> Block<'static>` 的 `inner(area)`
  （`max_scroll_offset_at` 与 `draw_transcript` 共用）—— 各处不再手写 `saturating_sub(2)`；
  `inner_h` 漏改是合法 `usize` 减法、**编译器不抓**。
- **Input 窗格仍是 `Block::bordered()`**（有意保留，只有 Chat 去边框）。
- 表格**保留方框线**（有意接受的例外：框线即可读性）。

### 14.5 帧时钟

- `ui` 顶部取一次 `now`，传给它下面的绘制链。**帧内 `build_text` 只被调 1 次**（`draw_transcript` 内），
  其后的 `line_count` 与它基于**同一份**产物。
- `total_rows` / `draw::max_scroll_offset` **只服务事件路径**（`App::compute_max_scroll` ← ↑↓ 滚动 / resize
  夹取 `scroll_offset`），该路径**自取** `Instant::now()`。

### 14.6 回放走 raw

- `transcript_plain_lines` 的 `Assistant` 分支走 raw 路径：按 `\n` 原样切行、无样式、不经缓存
  ⇒ 退出后主屏 scrollback 留**原始 markdown 源**（可复制、可搜索）。其他变体行为不变。
- `/copy` 本就读原始串，未改。

### 14.7 panic 边界

- **动机**：`run()` 在 `event_loop` panic 时会**跳过** `restore_terminal`，终端会留在 raw mode / alt-screen；
  而模型输出是**不可信输入**、且被喂给自述 PoC 的第三方解析器。
- `fn restore_sequence() -> String`：`EnableLineWrap` → `LeaveAlternateScreen` → `DisableBracketedPaste`
  → `PopKeyboardEnhancementFlags` 的字节序列，由 `exit_payload` 与 panic hook **共用**。（四条各改互不相干的
  模式位；唯一先后约束是「退 alt-screen 必须在回放段之前」—— 回放落点靠 `?1049l` 恢复。）
- `install_panic_hook()` 在 `run()` **入口**（`setup_terminal()` **之前**）调用；先 `take_hook()` 取上游默认 hook，
  再 `set_hook`，hook 体最后调用它 ⇒ 保留默认的 panic 信息打印与 unwind 语义。
- `fn panic_hook_body<T>(restore: impl FnOnce(), delegate: &(dyn Fn(&T) + Sync), info: &T)`：先恢复再 delegate。
  泛型 `T` 是为**可单测**（`PanicHookInfo` 无法在测试里构造）。
- `restore_terminal_on_panic()`：写 `restore_sequence()` → flush → `disable_raw_mode()` → `execute!(out, Show)`，
  **每步吞 IO 错误、绝不二次 panic**；**不** `process::exit`；**不**用 `catch_unwind` 续跑（panic 后状态不保证一致）。
- 绘制路径**不得 panic**：`push_non_assistant` 的 `Assistant` 分支已从 `unreachable!()` 改为**降级为 raw**
  + `debug_assert!(false, …)`（测试期大声、生产期降级）。

### 14.8 落地数字

- `cargo test --workspace`：基线 **975 passed / 0 failed / 7 ignored** → 现在 **1005 passed / 0 failed / 8 ignored**。
- `cargo test -p ys-tui-coding`：**212 → 242**（增量分布见 §9.4）。
- `cargo clippy --all-targets`：**无新增 warning**（与基线副本的 warning 站点集合逐一相同）；
  `ys-protocol` 的 `this impl can be derived`（`lifecycle.rs`）为**既有**、与本变更无关。
- release 体积：基线 **7,747,024 B** → 现在 **8,200,144 B**，差 **+453,120 B（≈ 442.5 KiB）< 500 KiB**。
  （仓库 `Cargo.toml` 无 `[profile]` 段，两侧同工具链、同 cargo 默认 release profile。）
- 依赖：`Cargo.lock` 中 `ratatui-core` **只有 1 条**（v0.1.2 单版本）；`syntect` / `ansi-to-tui` / `onig` 均不出现；
  独立包数 194 → 201（+7）。

---

## 15. `--debug` 结构化执行日志（P0）— as-built

设计见 `docs/arch/agent-observability/design.md` + `adr-debug-log.md`；执行计划
`docs/exec-plans/2026-10-17-debug-log.md`。本节只记实际形态。

### 15.1 开关与文件

- `--debug` 在 TUI 首部 flag 区与 `-p` / `--json` 模式 flag 之后都可出现，与
  `--stats` / `--resume` **顺序无关**（`split_leading_flags` 循环剥离，重复 last-wins）。
  任务文本一旦开始就不再剥离（`-p "任务 --debug"` 的文本逐字保留）。
- `YUSHAN_DEBUG` 非空时**优先于命令行**（`1/true/yes/on` 开，其余非空值关；空串视为未设置）。
  `YUSHAN_DEBUG_ARGS` 控制工具参数摘要档位（`none` / `head` / `full`，默认 `head`）。
- 文件：`logging::log_dir()/debug-<session stem>.jsonl`（`YUSHAN_LOG_DIR` 一次覆盖日志与
  debug 两个产物）；会话尚未落盘时为 `debug-<pid>-<unix_ms>.jsonl`。追加写。
- 关闭时 `Option<DebugSink>` 为 `None`：**零文件、零线程、零路径解析**。

### 15.2 记录形状

一行一条 JSON，信封含 `schema`（`yushan.debug/1`）/ `ev` / `t_ms` / `dt_ms` / `seq` /
`run` / `session_id` / `turn` / `round` / `tool_call_id`（按需）。记录类型：

| 段 | 记录 |
|---|---|
| 运行 | `run_start`（版本 / pid / os / cwd / session_file / model / api_base 只留 scheme+host / limits / tools / skills / env 变量名）、`run_end`（stop_reason + `debug{records_written,dropped,write_errors,bytes_written,hot_path_ms}`） |
| 回合 | `turn_start`（input_bytes / budget_ms）、`turn_end`（rounds / elapsed_ms / budget_remaining_ms / cache 读数） |
| 轮 | `round_start`、`round_end`（ttft/gen/tools/total、thinking/text 首时刻与段长、增量计数、tool_calls） |
| 调用 | `round_usage`（`purpose=turn/compaction` + 逐轮 usage，含 `reasoning_tokens` 三态）、`tool_call`（参数摘要 + `args_*`）、`tool_result`（耗时 / 大小 / 错误位，**不记内容**） |
| 边界 | `boundary {phase: enqueued/observed, kind, origin}`（CLI 的预算 Abort 在 `push` 同刻用可克隆 `DebugHandle` 写 `enqueued`） |

`ModelTextDelta` / `ModelThinkingDelta` **零记录**（只累计计数）；`t_ms - dt_ms` 恒等于
`run_start_unix_ms`。`run_end` 另带 `rounds` / `duration_ms` / `exit_code` / 累计 `usage`
（`stop_reason` 是终局事件名，预算取消 / 无输出由 `exit_code` 区分）。

`/new` 换文件；同一 session 的 `/resume`（含**跨进程** CLI `--resume`）追加并在打开时按文件里
最大 `run` 递增；跨 session 的 `/resume` 切到目标会话的文件。

### 15.3 退出与既有行为

- `main.rs` 的 `process::exit` 收敛为 **1 处**（`exit_with_run_end`：`flush_run(meta)` →
  `process::exit`；`exit_with_debug` 是它的简化入口）。`-p` / `--json` 的**模型错误 / 持久化
  失败也走同一出口**，因此失败运行同样有 `run_end`。
- TUI 正常退出 / 出站信道关闭（UI 已收摊）前都 flush；`APP_THREAD_GRACE`（3 s）大于
  `FLUSH_TIMEOUT`（2 s），慢盘下 runtime 不会先掐掉 `run_end`。
- `yushan.log` 的逐轮文本行**逐字节不变**（`RoundClock::observe` 与 `observe_debug` 共享状态机，
  文本行由同一函数产出）；debug 内容**只进文件**，不占 stdout / stderr。

### 15.4 落地数字

- `cargo test --workspace`：**1247 passed / 0 failed / 11 ignored**。
- 新增集成验收 `apps/coding-agent/tests/debug_log.rs`（7 个用例 + 1 个手动 golden 生成器）：
  T1（debug 关零文件、既有诊断不变、stdout/stderr 不被污染）、T2（过滤 `RoundUsage` 后与
  基线 golden 逐字节相同）、T5（flush 后文件完整、末行 `run_end`、run_end 字段齐全）、
  T7（强制压缩时 `turn` 与 `compaction` 同时出现）、失败退出仍有 `run_end`、预算 Abort 的
  `enqueued`/`observed`、跨进程 `--resume` 的 `run` 递增。子进程带 30 s 看门狗。
- `cargo clippy --all-targets`：**无新增 warning**（当前 28 个站点，基线 34 个；差异为既有 warning
  的行号漂移、5 处 `collapsible_if` 与 2 处 `too_many_arguments` 消失）。
