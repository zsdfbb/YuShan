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
| ② Boundary | `ys_protocol::Boundary` | UI → `BasicLoop` | **轮边界**拉取（`take`）；模型调用前 / 工具轮询用 `is_aborted` / `has_pending_steer` | 64 |
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
`BashTool` 在挂载 `boundary` 时**以 100ms 间隔轮询**两个信号：

- `is_aborted()` 命中 ⇒ 杀**进程组**（`sh -c "kill -9 -{pid}"`，不引 libc）并返回中止错误；
- `has_pending_steer()` 命中（用户插话）⇒ 同样杀进程组、停止等待，但返回**成功结果**：
  已捕获的 stdout/stderr 照给，末尾说明「命令因插话提前中止，插话将在本批工具调用结束后
  进入上下文」。插话**不被工具消费**，仍由轮边界 `take`。

于是「长工具跑多久，插话就等多久」这一延迟被压到**一个轮询周期**（100ms 量级）。
未挂载 `boundary` 时行为与改动前完全一致（有测试钉住）。`timeout` 语义在挂载 `boundary`
时仍生效。

**UI 侧的两个时刻要分开说**（`App::note_steered`）：提交后立即画用户行 + 紧跟一条系统行
「已插话：将在当前工具调用结束后生效」。用户行只证明「已送达」，不代表「模型已看到」——
把两者混为一谈，用户会以为模型故意忽略自己。用 `TranscriptLine::System` 而不是新增
`App` 字段：屏幕上就是它该在的地方，不必多一份投影状态。

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
| **Chat** | `Constraint::Min(3)` | 唯一可滚动区域；滚动以 **wrap 后的视觉行**为单位；窗格**只留顶部标题线**（见下）；**视窗化渲染**（只画视窗覆盖的行）见 §18 |
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
`Summary{rounds,stop,elapsed_secs}` / `Error` / `System`）—— 工具调用最多 **两行**（`TOOL_MAX_ROWS = 2`）渲染，
摘要由 `summarize_tool_args`（`SUMMARY_KEYS = [path, file_path, command, pattern, query, url, description]`）
从结构化参数抽取，**摘要规则属产品知识，故在 TUI crate 而非协议**。

**工具行的宽度与行数**：正文按 Chat 内区**当前可用宽度**折行（`wrap_limited`：优先在空格处断，
单个词超宽才按列硬断），行首 `⏺ ` 与续行对齐位按**实际显示宽度**计算（`⏺` 的宽度随 unicode-width
版本可能是 1 或 2，硬编码列数会让续行错位）。`✓` / `✗` 落在**末行**行尾，两行时跟着走、不停在半截内容后。
失败时正文只占 **1** 行（`└ 结果首行` 另起一行，合计仍 ≤ 2 行）。超出两行的部分**只截不续**、
末行补 `…` —— 工具调用是「过程」的刻度而非正文，一条长 `bash` 命令不该把一串调用摊满半屏；全文在工具结果与日志里。
`SUMMARY_MAX_CHARS = 240` 只是**上界护栏**（防一条巨型 `write` 正文被抄进 transcript），**不是显示宽度**；
参数里的空白（含 `\n` / `\t`）先压成单个空格，故一条工具调用不会因参数内嵌换行而多占行。
`Summary` 渲染为 `{symbol} {stop-label} · N rounds · X.Ys`：已知 `StopReason` 的标签为
`completed` / `max rounds` / `cancelled` / `truncated` / `no output`，未知变体显示 `unknown`；
TUI 绘制与退出 scrollback 使用同一格式。
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

**行动块（`LineClass`）**：transcript 条目按「**类别**」分三档，决定**缩进**、**降色**与**块边界留白**：

| 类别 | 条目 | 含义 |
|---|---|---|
| `Prompt` | `User` | 用户提问 |
| `Action` | `Thinking` + `Tool` | 行动块 = 模型的**过程** |
| `Content` | `Assistant` / `Summary` / `Error` / `System` | 正文块 = 模型的**产物** |

`Thinking` 与 `Tool` **同属** `Action` —— 它们是同一件事的两面（想 → 做）；拆成两类会在两者之间
插空白，把一串工具调用切碎。块**内部**的层次由缩进表达：思考缩进 **2 格**、工具缩进 **4 格**
（层次：想 ⊂ 做），整体前景色取 **`muted`** 槽位，与顶格的正文块拉开视觉重量。
工具行的 `✓` / `✗` 分别取 **`ok`** / **`fail`** 槽位 —— 那是**语义**信号（成功与否），不属于「行动」的类别色；
具体明暗由当前主题决定（见 §14.9），不假设「绿 / 红」。
缩进用**空格**而非制表符（制表宽度随终端设置变化，同一份内容会错位），且**不依赖任何特殊字形** ——
字体缺 `⌁` (U+2301) 时缩进照样成立。`LineClass::indent` 是缩进的**唯一真相源**
（思考行与工具行都经它取值），不在两处各写一遍魔法数。

**块边界（`BLOCK_GAP_ROWS = 1`）**：条目**类别**变化时插入 **1 行空白**（`User` / 行动块 / 正文块之间）。
与上面的 `USER_GAP_ROWS` **正交** —— 那条按**条目类型**（问答之间），这条按**块类别**（块类型之间）；
两条规则**叠加**（`User` 之前最多 2 + 1 = 3 行）。`Thinking` → `Tool` 同类**不留白**（同一行动块）。

留白与缩进由 `gap_rows` **统一计算**，两条渲染路径（TUI 的 `build_text_with`、退出回放的
`transcript_plain_lines`）共用同一套规则 ⇒ TUI 画面、滚动数学、退出回放三者一致
（回放侧 assistant 走 raw、与 TUI 内不同形是**有意**的，但留白与缩进必须一致）。
「可见」判据是 `is_visible`：**不可见条目**（`/thinking off` 下的思考、空 assistant）既**不留白**
（否则留下无人消费的**孤立空行**）、也**不推进**「上一条类别」（否则多切一个块边界）——
它们在屏上不存在。注意该判据**不能**用「实际产出了几行」代替：回放侧空 assistant 走 raw
会产出 1 个空行，TUI 侧 `md::render("")` 产出 0 行，用产出行数会让两边在空 assistant 之后
给出不同留白。

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

**键位归属**：`Up/Down` 归**输入历史**（readline 语义：`Up` 拿更旧一条、`Down` 往新并在越过最新时还原草稿），
聊天滚动让给 `PageUp/PageDown`（逐页）、`Ctrl+Up/Ctrl+Down`（跳到最早 / 最新）与
**滚轮 / 触控板上下**（逐行，[ADR-0050](../../adr/0050-tui-mouse-wheel-scroll.md)）；
水平方向归输入（`Left/Right/Delete/Home`）；
`Ctrl`·`⌥` + 左右（或 `⌥b` / `⌥f`）让输入光标**按词**移动，`Home` / `End` 到**输入当前行的行首 / 行尾**
（`Ctrl-E` 与 `End` 同义；`Ctrl-A` 已让给**全选**，[ADR-0059](../../adr/0059-ctrl-a-select-all-and-ctrl-v-paste.md)）。
补全浮层打开时上下归浮层。

> 本段是 §18.4 之前的**历史叙述**（`↑↓` 归历史、`End` 归聊天底部的时期），已被 ADR-0045 /
> ADR-0057 / ADR-0059 依次改写；**当前键位以 §18.4 的表为准**。

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

**表在 TUI 侧**（`ys-tui-coding/src/commands.rs`，15 条 `CommandSpec`）：命令的**用户可见行为**
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
| `/effort [level]` | 无参本地显示当前档位与用法（**不发请求**）；有参直发；`default` 清除设置（发 `None`） | `SetEffort` |
| `/theme [name]` | 无参时**浮层选择 + 逐键实时预览**（Esc 还原不落盘、Enter 确认落盘）；有参本地立即生效、再单向同步；非法值只提示（见 §14.9） | `SetTheme` |
| `/model [name]` | 无参时**浮层选择**（`available_models`，每条显示 `provider/model`、可输入筛选）；有参直发，支持 `provider/model` 限定名 | `SetModel` |
| `/login` | **问 provider / api_key**（`Prompter` 浮层） | `Login` |
| `/logout` | — | `Logout` |
| `/new` | — | `NewSession` |
| `/resume [id]` | 无参时**浮层列出现有会话**供选择（Esc 取消、空列表只提示）；有参直切 | `ListSessions` / `ResumeSession` |
| `/compact` | 空闲 ⇒ 直发；**回合中 ⇒ 改投 `Boundary::Compact`**（轮边界执行，见 ADR-0052） | `Compact` |
| `/export [path]` | — | `Export` |

**`Prompter` trait 保留**（design §3 注；死的只是 `InquirePrompter`）：
`select` / `text` 是「选择器可测」的答案。**决策逻辑**抽在纯函数 `resolve_prompt(PromptKind, &dyn Prompter, &CodingView)`
里，单测注入 `FakePrompter` 即可覆盖全部路径；TUI 实现 `TuiPrompter`（`run.rs`）只把同一套问答画成
嵌套事件循环的模态浮层。任一问答步骤取消（Esc / 空 key）→ **不产生任何 `Request`**（半截登录比什么都不做更糟）。

**app 侧能力**（`apps/coding-agent/src/capabilities.rs`）：`login` / `logout` / `set_model` /
`set_effort` / `new_session` / `compact` / `export`，每个返回 `Vec<String>`（每项一行），由 `app_loop` 包成
`Outbound::Output`。**本模块绝不 print**（TUI 起屏后 stderr/stdout 会冲掉整屏）。
`new_session` / `compact` 是 `async`（碰 `Session`），`export` 是**同步**（见 §8）。

> **回合中投递**：`/compact` 是**唯一**按 `is_turning` 分流去向的命令 —— 空闲走 `Request::Compact`
> （外层循环立即处理），回合中改投 `Boundary::Compact`（下一个轮边界执行），并在提交时当场
> 补一行「已请求压缩，将在下一轮边界执行…」。理由：`request_rx` 只在回合之间被消费，
> 回合中投 `Request` 要等整个回合跑完（实测 7 分钟）。见 ADR-0052。
>
> **开始 / 结束两行提示**（两条路径都有）：`/compact` 会先给一行「Compacting context (~N tokens)…」，
> 压完再给「Compacted: ~N → ~M tokens (… summarized, … kept)」。开始行必须在压缩那次 `await`
> **之前**落地 —— 摘要是一次独立模型调用（实测数十秒），`capabilities::compact` 中途没有出站口，
> 攒到最后一起发就等于压缩全程冻屏。压不了（无 model / 无事可压）时**只给一行**收场提示：
> 判定与文案在 `capabilities::compact_opening`（`CompactOpening`），与 `compact` 共用同一个
> `plan()` ⇒ 不会出现「先打了正在压缩、再说无事可压」。
>
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
> **启动期同步拉目录的条件**：凭证齐全但模型名为空。两种来源：
> · `YUSHAN_API_BASE` + `YUSHAN_API_KEY` 直连且**没给** `YUSHAN_MODEL`；
> · auth.json 里有凭证但 `model` 为空（上次 `/login` 时没能读到目录）。
> 成功 ⇒ 取目录第一条当起始模型并并入缓存。
> **失败按模式分流**（ADR-[0056](../adr/0056-startup-catalog-failure-tui-degrades.md)）：
> TUI ⇒ 以「无模型」状态照常起屏，失败原因作为**启动期提示**（`Outbound::Output`）进
> transcript，用户可用 `/login` / `/model` 修复；`-p` / `--json` ⇒ 早失败（stderr）并提示
> 显式给 `YUSHAN_MODEL` —— 没有交互窗口，缺模型跑不了。
> **空模型名绝不构造模型实例**：接线器进入真正的「未配置模型」状态（状态行 `(no model)`，
> 发消息由运行时的 `No model configured` 拦下），不会拿空名去撞端点。
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
| D6 | 命令表只有 §6 的 9 条 | 表里有 **15 条**，多出 `/thinking`、`/copy`（后者 §6 表里写「本地」但表头只有 9 行）、**`/skills`**（skills 特性落地后补，**已同时写回 design §6 表**，故不是偏离）、**`/resume`**（运行期切换会话落地后补，见 §8.2 M19）、**`/effort`**（思考档位落地后补，见 §8.2 M21）与 **`/theme`**（配色主题落地后补，见 §14.9） | `/thinking` 在 §2.1 UI 规格里出现（`/thinking on`），实现把它做成**本地命令**（`Thinking(Option<bool>)`，无参 toggle）；`/copy` 是 §6 表里的「本地」项；`/skills` 见 §8.2 M4；`/resume` 见 `docs/design-final/runtime-resume.md`；`/effort` 见 `docs/design-final/effort.md`；`/theme` 见 §14.9 与 ADR-0053 |
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
| M4 | **`/model` 无参的模型列表来自 `CodingView.available_models`** —— 由**后台拉取**的目录填充：`main` 在起屏**前**只从 registry 摘出要拉的 `(api_base, api_key)`（`fetch_targets`：每个已登录 provider + env 直连端点），起屏**后** `rt.spawn` 一个后台任务 `ProviderRegistry::fetch_all`（`JoinSet` 并发，5s 超时），结果经 `mpsc` 送回 `app_loop`，由它 `apply_model_cache` 写回 registry 并**自发刷一次视图**（无 `Request`）。**模型名只有「该端点 `/v1/models` 的目录」这一个来源**（`/login` 也拉一次该 provider 的目录取第一条当起始模型）—— 代码里没有任何写死的模型名，拉不到就没有那一家，**不做静态回退**。候选 = **所有已登录 provider 已拉到的目录** + env 直连端点按 base 取的目录，**按 `(provider, model)` 去重**（同名 id 跨 provider **各占一行** —— 按 id 去重会把其中一条吃掉，用户既看不到也选不到）+ 当前正在跑的模型。每条是 `ModelEntry { provider, model, current }`：浮层显示 `provider/model`，归属未知（无 `cfg.provider`，如 env 直连）时退化为裸模型名，当前那条缀 `[current]`。**正在跑那条的归属取 `cfg.provider`（权威）**，不靠反查 —— 接线器的 model 就是用它那套凭证建出来的。**起屏不再等网络**（开屏即用；例外是「有凭证、无模型名」的启动期同步拉取，其失败在 TUI 下降级为无模型启动，见上）；**渲染路径一次 HTTP 都不发**（重绘不得触发 HTTP）—— registry 的写与 `build_view` 的读同在 app 循环那一线程，是唯一写者 | `apps/coding-agent/src/{provider,view,main,app_loop}.rs` |
| M9 | **跨 provider 选模型**：候选是多个 provider 的并集，故归属**显式带过来** —— `Request::SetModel` 带 `provider: Option<String>`（浮层选中时由 UI 带上；`/model provider/model` 限定名由 `capabilities::set_model` 拆出，**仅当 `/` 前逐字命中已知 provider 名**才拆，否则网关那种 `vendor/model` 形态的 id 会被误切）。**只有裸模型名**才回退 `ProviderRegistry::provider_for_model` 反查 —— 它遍历 `model_cache`（`HashMap`，迭代顺序不确定）返回第一个含该 id 的 provider，**同名 id 的归属是任意的**，会静默把端点切到错的一家。归属与当前 provider 不同则**连端点一起切**（api_base / api_key / provider，取自该 provider 的 auth 条目）。目录里都没有的手敲名字**保持当前 provider 不变** —— 只改 `config.model` 会把 A 家的模型名发到 B 家的 base | `apps/coding-agent/src/capabilities.rs`；`crates/ys-protocol/src/request.rs` |
| M5 | **`Action::Abort` 目前是保留变体**：`parse()` 不产出它；Ctrl+C / Esc 在 `handle_key` 里直接投 `Boundary::Abort`。留作将来 `/abort`。turn 中 Ctrl+C 是**两段式**（第一次取消、第二次退出），见 M17 | `ys-tui-coding/src/commands.rs` |
| M6 | **`app_loop` 的回合失败只落日志、不刷 transcript**：`BasicLoop` 返回错误前已发 `RunFailed`（UI 已看到 Error 行）；**开跑前**的无模型错误由 `Agent::run_turn` 自己发同一条 `RunFailed`（不启动循环，但终局事件不变量照旧） | `app_loop::run`、`ys-runtime::agent` |
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
| M21 | **`/effort` 不镜像本地状态，且设置走模型重建**：① 与 `/thinking` 不同，`/effort <level>` **不经**「本地先写一行 + 镜像 Request」——档位真值在 app 侧 `Config.effort`，TUI 只读 `CodingView.effort`；本地先改会让 UI 早于 app 显示一个**尚未重建模型**的档位，命令成功与否也无从区分。无参显示是纯本地读快照。② 档位**随会话 meta 持久化**（`SessionMeta.effort`）：它描述「本会话想用什么思考深度」，与 `/thinking` 的显示偏好同属会话级状态；`/new` 不重置（同 `thinking_visible`），`/resume` 从目标 meta 还原并重建，`-p` / `--json` **只读不写**（不覆盖既有会话）。③ 有效档位经 `Config::effective_reasoning_effort()`（会话覆盖优先，否则配置默认 `high`）交给 `ProviderCompat::effort_for(model, ..)`（provider 级能力位 + 模型级代际两层门控，并按方言折叠），产出写进 `OpenAICompatibleConfig.reasoning_effort`；只有**重建 model** 才能进请求；`set_effort` 与 `set_model` 同路径（无凭证 ⇒ `wiring.set_model(None)`），故下一个回合（含 subagent / compact 的工厂构造）自动继承，回合内不热切换 | `ys-tui-coding/src/{commands,events,view}.rs`；`apps/coding-agent/src/{config,capabilities,app_loop,main,view}.rs`；`crates/ys-protocol/src/request.rs`；`crates/ys-session/src/meta.rs`；见 `docs/design-final/effort.md` |

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
| `crates/ys-protocol` | 46（其中 `boundary` 18） | `Boundary` FIFO / `Abort` 置位即见 / `is_aborted` 非破坏性 / Steer 不置位 / `has_pending_steer`（只看 Steer、不消费、不泄露正文、默认 false）/ `Send+Sync` trait 对象；`Request`/`Outbound`/`Envelope` 序列化 |
| `crates/ys-tui-coding` | 501（**当前值**） | 三 pane `TestBackend` 渲染、状态行四档宽度、输入区（多行 + 粘贴 + 中文退格）、补全浮层、命令表逐条映射、`resolve_prompt` 决策路径、`FakePrompter`、`/skills` 选择器（回填 / 遮蔽过滤 / 取消）；**本轮 md 渲染 +30**（逐项见 §9.4） |
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

### 9.5 `/effort`：新增用例与变异

测试按层分布（逐条契约见 `docs/design-final/effort.md` §6）：

| 落点 | 覆盖 | 变异点（注入 ⇒ 变红） |
|---|---|---|
| `crates/ys-core/src/effort.rs` | 七档 `parse → as_wire → Display` 往返；serde 与 `as_wire` 逐档一致；`default` / 空串 / 未知值被拒并列出合法值；大小写与 trim | 改任一 wire 拼写 ⇒ 往返用例；删 `rename_all = "lowercase"` ⇒ serde 用例；把 `default` 当合法档位 ⇒ 非法值用例 |
| `crates/ys-protocol/src/request.rs` | `SetEffort { Some(..) }` / `{ None }` 双向往返；老 JSON 形态仍可反序列化（外部标签 + 小写拼写） | 删 `None` 形态的往返 ⇒ 真实信道上清除请求解析失败；改变体标签 ⇒ legacy 用例 |
| `crates/ys-session/src/meta.rs` | `effort` 往返（落盘为小写）；缺键 / `null` ⇒ `None`，旧首行仍可反序列化 | 删 `#[serde(default)]` / 把字段改成非 `Option` ⇒ 缺键用例 |
| `adapters/model-openai-compatible/src/lib.rs` | `reasoning_effort` 显式值时原样序列化、`None` 时整个字段不出现；`constructors_pin_reasoning_effort_support` 钉住哪些 provider 声明支持（当前 `opencode-go` 与 `glm`），`effort_dialect_defaults_to_verbatim` 钉住默认方言 | 去掉 `skip_serializing_if` ⇒ 默认请求多出 `null`；关掉 `opencode-go` 能力位 ⇒ 构造器用例 |
| `apps/coding-agent/src/capabilities.rs` | `set_effort` 保存 + 重建（factory 收到有效档位）+ 输出 `effort: high`；`None` 清除覆盖并输出配置默认 `high`；无凭证 ⇒ `wiring.model == None`；不发该字段时（provider 级或模型代际级）仍保存并给 warning，文案含 provider 与 model；`/resume` 从目标 meta 还原并重建 | 只写 `config.effort` 不重建 ⇒ factory 断言；无凭证仍保留旧 model ⇒ 置空断言；漏 `cfg.effort = effort` ⇒ resume 用例；把 `accepts_reasoning_effort()` 换回 `supports_reasoning_effort()` ⇒ 无法解析模型的 warning 用例 |
| `apps/coding-agent/src/app_loop.rs` | `SetEffort` 臂产出 `Output` + 刷新 `View.effort`；`None` 清除覆盖后视图回到默认 `high`；`every_request_variant_is_handled` 覆盖该变体；回合末写入 / `/new` 保留 | 漏 `SetEffort` 臂 ⇒ `match` 不穷尽编译失败；只改 config 不刷视图 ⇒ `set_effort_updates_view`；回合末不写 ⇒ meta 用例 |
| `apps/coding-agent/src/{main,view}.rs` | `finalize_turn_meta` 三态（写入 / 清除 / 不写）；启动期 `restore_effort_from_meta`；`CodingView.effort` 透传**有效**档位（覆盖或配置默认） | 无条件写 ⇒ 三态用例；视图写死 `None` ⇒ `test_build_view_passes_effort` |
| `crates/ys-tui-coding/src/{commands,events}.rs` | 命令表 15 条；`/effort` 四形态（无参本地、合法档位、`default`、非法值）；无参恰好一条 System 行且不发请求；有参只发一条 `SetEffort`、不改 transcript；`/status` 用规范拼写 | 删 `default` 分支 ⇒ 四形态用例；把 `/effort` 接进 `/thinking` 的镜像路径 ⇒ 事件用例 |
| `crates/ys-protocol/src/boundary.rs` | `Compact` 不置 `aborted`、参与 FIFO、`Debug` 不泄露正文、`has_pending_steer` **包含** `Compact` | 把 `push` 的 `Abort` 判定放宽到 `Compact` ⇒ abort 用例；扫描改回只匹配 `Steer(_)` ⇒ 探针用例 |
| `crates/ys-loop/src/basic.rs` | 轮边界收到 `Compact` ⇒ 就地压（**不判阈值**）+ `purpose=Compaction` 上报 + 本轮只跑一轮；**收场前**收到 `Compact` ⇒ 压了但 `rounds` 不增 | 删 `Boundary::Compact` 臂 ⇒ 不压缩；把它设成 `consumed = true` ⇒ `rounds` 1→2 |
| `crates/ys-tui-coding/src/events.rs` | 回合中 `/compact` ⇒ 投 `Boundary::Compact`、**不**投 `Request`、当场出一行反馈；空闲 `/compact` ⇒ 仍投 `Request::Compact` | 删 `is_turning` 分流分支 ⇒ 回合中用例；去掉守卫 ⇒ 空闲用例 |
| `apps/coding-agent/src/app_loop.rs` | 回合中投 `Boundary::Compact` ⇒ 本回合出站序列里出现 `ContextCompacted`（不等回合结束） | 删 `Compact` 臂 ⇒ 不压缩 ⇒ 红 |
| `scripts/doc-consistency.sh` | architecture.md 命令表含 `/effort` 且计数为 14 条 | 计数回退 / 命令表漏行 ⇒ 脚本非零退出 |

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
      —— **口径已随 ADR-0045 更新**：视窗化之后滚动成本与会话长度解耦，
      这一条要验的是「翻页跟手」与「往上翻不被拽回底部」（见 §18）
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
- **stream_options 兼容回退**只在 400 响应体**明确**把 `stream_options` 归为不支持 / 未知时触发；
  普通 400（包括 tool call 配对错误）原样失败、不额外重发。回退 warning 走同一 `ModelLogger`，
  不得直写 stderr；非法 tool-call 参数 JSON 的降级 warning 同样走 logger。
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

- **父工具表 = 基础 9 元 + `bash_bg` + `bash_poll` + `SubagentTool` 追加末位**（`apps/coding-agent/src/assembly.rs` 的 `AgentFactory::build()`：基础 9 元之后依次 `push` 两个后台工具，最后 `push` 派发工具）。末位保证**子 tools JSON 是父的字节前缀** ⇒ 命中 prompt cache。
- **子工具表 = 基础 9 元**（无 `subagent`）⇒ 派发**结构上不可能递归**（`registry.get("subagent")` 未命中 ⇒ 走 `tool not found`）。
- **两份系统提示**：父（12 工具，能力边界声明改为「CAN delegate tasks to subagents」）、子（9 工具，逐字保留「no subagents」）。由 `prompt::build_system_prompt` 按**工具表派生**，不是常量。
- **子 `RunLimits`**：与主 agent 同源（同一份 `Config`），但 `fixed_prompt_tokens` 填**子表**的真值（否则子会话系统性晚触发压缩）。

### 13.2 每回合的注入与运行

- 三条入口（TUI `app_loop` / `-p` / `--json`）**每回合**造一份 `AppSubagentRunner`（派发计数器因此天然 per-turn），经 `AgentPorts::with_subagent` → `RuntimeContext.subagent` → `ToolContext.subagent` 三跳送达工具。
- 一次派发：建**全新子 `Agent`**（`Agent::run_turn` 取 `&mut self`，实例不可共享） + `MemorySession` + 本地 `CollectingSink`（**子事件只用于本地转录，不进父事件流**）+ `ChildBoundary`。
- **取消**：`ChildBoundary::take()` 只取自己那份、`is_aborted()` = 自己 **或** 父 —— 父 Abort 经探针向下传播，子吃不到父的 `Steer`；
  `has_pending_steer()` **恒 `false`**（不透传）—— 插话是给父模型的输入，若透传，父的插话会表现为
  「子 agent 的命令莫名提前中止」并让孩子那边生成一条语义矛盾的「用户插话了」结果。
- **子预算**：watchdog（`tokio::spawn(sleep(budget) → 子边界源 push(Abort))`，用 **`AbortOnDropHandle`** 防孤儿任务）；子预算与主 agent 同值 ⇒ 真实天花板是父回合那条 deadline。
- **超时**：`Tool::timeout()` 的优先级链（工具声明 → `bash_timeout` → 300s）；`subagent` 的外闸 = 子预算 + 60s 宽限，**构造期由 `Config` 注入**（ADR-0029 修正 2）。
- **插话**：同步 `subagent` 的 `call` 用 `tokio::select!` 同时等 `runner.run(..)` 与「父边界源上的
  `has_pending_steer()` 轮询」（`STEER_POLL_INTERVAL = 100ms`，与 `tools-basic` 同口径）。命中则
  **放弃等待** —— drop 掉 `runner` future ⇒ 子回合取消（其内部子进程靠 `kill_on_drop` 收尸）——
  并返回 `is_error: false` 的结果说清「派发被插话中止、子 agent 未完成」。插话不被消费，
  仍由父轮的轮边界 `take`。`subagent_async` **不需此机制**（它本来就不阻塞父回合）。

### 13.3 结果与产物

- **回传**：摘要文本主体按 `MAX_SUMMARY_BYTES = 8 KiB` 做 UTF-8 安全截断；截断标记与随后由 runner 保证的 `## Files` 节不在这 8 KiB 预算内，因此最终 `summary` 可略超 8 KiB。完整转录落 `~/.yushan/subagents/`（`YUSHAN_SUBAGENTS_DIR` 覆盖口；Primary 档启动期 sweep 7 天）。
- **完整转录**：按子会话内容块保留 `Text`、`ToolUse` 的 id/name/参数 JSON、`ToolResult` 的 id/is_error/正文。工具结果正文优先取 `CollectingSink` 中的原始 `ToolResult` 事件，只有事件缺失时才回退会话正文（turn budget 可能已把较早结果改写为降级文本）。末尾记录 `stop_reason`、取消来源、`elapsed_ms` 与诊断。
- **失败**：子撞 `max_rounds` / 被取消 ⇒ `Ok` + 非 `Completed` 的 `stop_reason` + `diagnostic` ⇒ 父侧 `ToolResult{is_error}`，**父回合不失败**。`SubagentTool` 的结果正文顺序为诊断 → 摘要 → 转录路径（无诊断时摘要在前）。诊断固定以 `[diagnostic: ` 开头，控制字符替换为空格，整条最多 1024 字节并在 UTF-8 边界截断；`Cancelled` 带 `cause=parent-turn-cancelled|child-budget|unknown`，父取消优先于子预算。转录写失败不 panic，错误并入诊断；摘要保留。
- **Runner 错误分界**：只有子回合已经启动、`child.run_turn` 返回 `Err` 时，runner 才构造 `SubagentOutcome{stop_reason: Cancelled}` 并生成 `stop_reason=cancelled cause=unknown` 诊断。无可用模型、派发次数超限、`AgentBuilder` 组装失败等发生在子回合之前的错误直接从 `AppSubagentRunner::run` 返回 `Err`；`SubagentTool` 将其映射为 `ToolError::Execution`，不生成 diagnostic 前缀。
- **`model` 参数**：按名构造（`Config::build_model_for`）+ `Arc<dyn Model>` 缓存（复用连接池/TLS）；构造失败 **fail-closed**，绝不静默回退父模型。
- **usage 上卷**：子 usage 累加进 `Arc<UsageTotals>`，回合末 drain 并追加进 `RunResult.usage` ⇒ `TurnStats` / meta / 退出码看到真实总消耗。

### 13.3b 异步派发（`subagent_async`）的投递

- **派发即返回**：`SubagentAsyncTool` 只登记后台任务（`tokio::spawn`），立刻回句柄 `sa{N}`；取消机制**不适用**（它本来就不阻塞父回合）。
- **任务是真的分离**：`spawn` 的 `JoinHandle` 被丢弃、没有 abort 路径 ⇒ 父回合结束后子 agent **继续运行**（实测转录在回合结束后 2~6 分钟才落盘）。「回合结束即取消」从来不是实现行为。
- **结果跨回合送达**：完成结果交给 `PendingResults`（**进程级**、`Arc` 跨工作区重建共享）——
  - 本回合**已绑定**落点（`SubagentSetup::bind_results`，守卫活到回合块末）⇒ 直接 `push` 成 `Boundary::Deferred`，`BasicLoop` 在轮边界 append 并 emit `DeferredResult`；
  - **未绑定**（回合已结束）⇒ 留表（上限 `MAX_PENDING_RESULTS = 32`，超限淘汰最旧并留日志），由**下一回合开头**一次性补投。
- **补投不限流**（与后台命令的 `BG_CATCHUP_PER_TURN = 2` 不同）：派发数量已被 `DEFAULT_MAX_DISPATCHES_PER_TURN` 封顶，且每条结果都在等一个明确答复 —— 限流只会让模型继续误以为「还没回来」。
- **提示词据此约束模型行为**：`subagent_async` 在场时的 guideline 明确「结束回合不丢结果，下一回合开头送达」，并禁止未核实就声称「还在跑」。

### 13.4 并行批（Step 9）

- 准入：批次 `len > 1` **且每一个**工具都 `is_concurrency_safe()` ⇒ 走 `futures_util::stream::iter(..).buffer_unordered(max_parallel_tools.max(1))`（单任务内并发、完成即交出一个结果、不需 `'static`）；否则**原串行路径**。`max_parallel_tools = 0` 钳为 1。
- 事件顺序：并行批先按原始调用顺序 emit 全部 `ToolCall`；执行阶段每个工具完成时立即 emit 对应 `ToolResult`，因此结果事件按完成顺序出现。结果同时写回原始 index，Step 9c 仍按原 `tool_calls` 顺序组装单条回喂消息。**串行批保持逐个 `C1,R1,C2,R2`**。默认 `is_concurrency_safe() == false` 是准入守卫；当前声明为 `true` 的集合是 `subagent` 与 `read` / `find` / `ls` / `grep`。

### 13.5 已知缺口（承接 §10/§12 的口径）

- 子 agent 内部活动在**任何输出通道都不可见**（`--json` 亦然），只有落盘完整转录可查 —— ADR-0022 决策 2 的少数派偏离。
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
  **颜色全部来自 `ThemeKind::palette()` 的 10 个槽位**；`AssistantStyles` 持有 `Palette` **值**（`AssistantStyles::new(palette)`），
  方法体内只出现 `self.palette.<槽位>`，不出现任何具名色（见 §14.9）。
  `code()`（行内码 + 围栏代码块）**只用单一 `code` 槽位前景色，不分语言**（2026-10-10 修订）——
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

### 14.9 配色主题 — as-built

形态决策见 [ADR-0053](../adr/0053-tui-color-themes.md)。本节的范围**大于** §14 的 markdown 样式表：
整屏画布、状态行、工具标记、模态浮层与 markdown 各元素同属一套配色。

#### 模块落点与类型

| 符号 | 形态 | 位置 |
|---|---|---|
| `ThemeKind` | 5 变体枚举，`Copy + Default`（默认 `AyuMirage`）；`ALL` 的**顺序即浮层展示顺序**，且与变体声明顺序一致（`next` / `prev` 以判别值取下标） | `crates/ys-tui-coding/src/theme.rs` |
| `ThemeKind::name()` / `label()` | kebab-case 机器名（`/theme` 的取值、`state.json` 的存储值）/ 人读名（浮层逐行显示） | 同上 |
| `Palette` | 10 个 `Color::Rgb`，`Copy + PartialEq`，40 字节。**不实现 `Default`**：构造必须显式给出 `ThemeKind`，否则「漏传主题」会被一份静默色板掩盖 | 同上 |
| `ThemeKind::palette() -> Palette` | **按值**返回，不是 `&'static Palette` —— 调用方可自由持有 / 比较一份色板值 | 同上 |
| `is_purple(Color)` | 紫色守卫，仅 `pub(crate)`，只在单测调用 | 同上 |
| `AssistantStyles::new(palette: Palette)` | 收**色板值**而非 `ThemeKind`：渲染层只需要 10 个颜色槽位，中间不出现「色板 → 主题 → 色板」的反查 | `styles.rs` |
| `draw::chrome(app) -> Palette` | 渲染层**唯一**取色入口（`app.theme.palette()`） | `draw.rs` |
| `App::set_theme(kind)` | **唯一**失效入口（见下） | `app.rs` |
| `AppState.theme: Option<String>` | 全局偏好的持久化载体 | `apps/coding-agent/src/state.rs` |

**`ThemeKind` 的解析**（`FromStr`）：大小写不敏感，`'_'` 归一化为 `'-'`；未知名返回 `ThemeParseError`，
其 `Display` **回显输入原文**并列出全部合法名。

#### 10 槽位语义表（语义 → 颜色的唯一映射来源）

| 槽位 | 落到哪些 UI 元素 |
|---|---|
| `bg` | 整屏画布；每个 `Clear` 之后的补铺 |
| `fg` | 正文：`md::render` 给库未上色的 span 补前景；用户消息正文、`Error:` 正文、定义列表、表格表头与单元格 |
| `accent` | markdown 标题（h1–h6）与数学（行内 / 块） |
| `code` | 行内码、围栏代码块；模态选择器的筛选框在**已输入**时的查询文本 |
| `muted` | 引用、系统行、状态行、行动块（思考 + 工具正文）、表格框线、列表标记、脚注、HTML、标题属性、前沿元数据、图片占位、选择器占位 / 计数、空态占位 |
| `user` | 用户消息前缀 `> ` |
| `ok` | 工具成功标记 `✓` |
| `warn` | GFM alert 的 `Important` / `Warning` |
| `fail` | 工具失败标记 `✗`、工具失败结果首行、`Error: ` 前缀、alert `Caution` |
| `info` | GFM alert 的 `Note` / `Tip` |

**链接与表格表头有意不着色**（前者只加下划线、后者只加粗），故它们不落在任何槽位上。

#### 5 套内置主题

`/theme` 的合法取值即 kebab-case 名（大小写不敏感，`_` 视同 `-`）：

| 名（= `/theme` 取值） | `label` | bg | fg | accent | code | muted | user | ok | warn | fail | info |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `ayu-mirage` | Ayu Mirage | #1f2430 | #cccac2 | #ffcc66 | #5ccfe6 | #6c7986 | #73b8ff | #bae67e | #ffd173 | #f28779 | #95e6cb |
| `gruvbox-dark` | Gruvbox Dark | #1d2021 | #ebdbb2 | #fabd2f | #8ec07c | #928374 | #83a598 | #b8bb26 | #fe8019 | #fb4934 | #83a598 |
| `nord` | Nord | #2e3440 | #d8dee9 | #88c0d0 | #8fbcbb | #4c566a | #81a1c1 | #a3be8c | #ebcb8b | #bf616a | #5e81ac |
| `everforest-dark` | Everforest Dark | #2d353b | #d3c6aa | #a7c080 | #83c092 | #859289 | #7fbbb3 | #dbbc7f | #e69875 | #e67e80 | #83c092 |
| `solarized-dark` | Solarized Dark | #002b36 | #93a1a1 | #b58900 | #2aa198 | #586e75 | #268bd2 | #859900 | #cb4b16 | #dc322f | #2aa198 |

色板是 `static` 常量、由 `const fn rgb(0xRRGGBB)` 在编译期展开 ⇒ 零运行时分配、零文件 IO。

**已知合法撞色**（属有意接受）：`gruvbox-dark` 的 `user` 与 `info` 同色；`everforest-dark` 与
`solarized-dark` 的 `code` 与 `info` 同色 —— 这几套调色板的非紫色相不足以再拉开一档。

#### 唯一失效入口与两条失效链

缓存里存的是**已上色**的产物，而两条缓存的键都**不含主题**（`RenderCache` 的键是
`(槽下标, src.len(), width)`；`TranscriptLayout` 的 `EntryMemo` 签名只哈希条目文本）。故切主题
**必须显式作废**，且两条链要一起：

```
App::set_theme(kind)                       // 唯一入口
├── self.theme = kind                      // 运行期真值
├── md_cache.set_theme(kind.palette())     // 链 ①：色板同值早退；不同则清 slots
└── layout.reset()                         // 链 ②：清 entries / gaps / starts
       └─ 下一帧 layout.sync(.., theme=..) 按色板值再判一次（自愈）
```

- **`RenderCache::set_theme(palette)`**：`palette == self.theme` 时**直接返回**（浮层逐键预览会反复调用它，
  同值重入不该白清一屏产物）；不同则记住新色板并清空全部槽位。它与 `RenderCache::reset()` 的区别只有
  一条：`reset()` 不改色板（`/resume` 的回放清屏与主题无关，两者语义不得混）。
- **`TranscriptLayout::sync(.., theme: Palette, ..)`**：`theme != self.theme` 并入既有的「宽度变 /
  `thinking_visible` 翻转」全量作废分支；`self.theme` **在分支之外**更新 —— 否则同一主题连续调用会永远
  判「变了」，每帧全量重算。`theme` 是**必填**参数（不给默认值）：漏传就是「屏上还是旧配色」这种只在
  缓存命中时才现形的偶发 bug。
- **`layout.reset()` 对「重渲染」是冗余的**（`sync` 的判据已能自愈），保留它是为了立即释放已上色的行、
  并让不变量在 `set_theme` 处自明。见「分歧与已知限制」第 1 条。

**失效判据按色板值比较（`Palette: PartialEq`），不引 `theme_rev` 版本计数器**：40 字节一次比值即得
「是否换了主题」，而计数器要求每个改主题的点都记得 `+1`、且「加对了吗」与渲染结果无因果关系
⇒ 漏加 / 重加都表现为「偶发旧色」。

**不许在调用点各写一遍这三行**：`/theme <name>`（`apply_local`）、浮层逐键预览与 `Esc` 还原
（`run.rs`）全部走 `App::set_theme`。漏一处即「某条路径切换后部分内容还是旧色」。

**`/resume` 不重置主题**：`reset_for_replay()` 不动 `theme`，且上一行的 `layout.reset()` 已清空
`EntryMemo`，下一帧 `sync` 会按当前主题重建 —— 无需为此补任何一行。

#### 背景铺色与 `Clear` 补铺

- `draw::chrome(app)` **一帧取一次**（`ui` / `ui_with_modal` 在 `&mut app` 借用**之前**取色），沿参数链
  下传 ⇒ 同一帧内所有 pane 与浮层必然读到同一份色板。
- `ui_with` 在 `area.width < MIN_WIDTH || area.height < MIN_HEIGHT` 的 **early-return 之后**用 `bg` 铺满
  `frame.area()`，再由各 pane 在上面画；终端过小时一个格子都不画。
- **每个 `Clear` 之后必须补铺 `bg`**（三处：`draw_completion` / `prompt_select` / `prompt_text`）：
  `Clear` 把区域重置为**终端默认底色**，不补铺就是屏上一个与主题背景不一致的黑洞 / 亮洞。
- 取色**按值**（`Palette` 是 `Copy`）：`push_transcript_line` / `push_non_assistant` 拿不到 `App`，
  而颜色是已烘进 `Line` 的产物 ⇒ 主题只能由持有真值的一方显式传入，不得在渲染层重新取默认色板。

**测试里有一处有意放宽**：背景断言逐格比对**除「宽字符尾随列」以外的每个格子**。ratatui 写宽字符时
会把紧随其后的列 `reset()` 回 `Color::Reset`（`Buffer::set_stringn`）—— 那一列在终端上被前一个字形
覆盖、底色本就不可见，无法补铺。判定条件极窄（`Span::raw(buf.content()[i-1].symbol()).width() > 1`），
其余格子一律必须等于主题 `bg`。（逐格而非抽查：`bg` 泄漏可能出现在任意没被 widget 覆盖的边角。）

#### `/theme` 三态与浮层实时预览

| 输入 | `parse()` 产出 | 行为 |
|---|---|---|
| `/theme`（含纯空白） | `Action::Prompt(PromptKind::PickTheme)` | 开浮层，`↑` / `↓` 立即应用 ⇒ 实时预览 |
| `/theme <合法名>` | `Action::Local(LocalAction::Theme(ThemeKind))` | `apply_local` 调 `App::set_theme` 本地立即生效，再由 `events.rs` 单向同步 `Request::SetTheme { theme }` |
| `/theme <非法名>` | `Action::Local(LocalAction::Hint(..))` | 一行提示（含原文回显 + 全部合法名），**不改任何状态** |

`LocalAction::Theme` 携带的是**已解析的 `ThemeKind`** 而不是原始 `String`：非法输入必须在进入本地应用
**之前**被拦下。归一化（大小写 / `_`）由 `ThemeKind::from_str` 决定，同步给 app 的载荷因此恒为 kebab-case。

**浮层实现**（`run.rs::apply_theme_modal`）：

- 复用 `draw::Modal::Select` 渲染，**不新造 `Modal` 变体** —— 浮层框架（居中尺寸、`Clear` + 背景补铺、
  页脚键位、滚动与计数）只该有一份。候选是 `ThemeKind::ALL` 的 `label()`；选中项按同一数组的
  `position` 反查回 `ThemeKind`（`theme_index`）。
- 逐键转移抽成纯函数 `theme_modal_step(ThemeModalState, ThemeKey) -> ThemeModalStep`
  （`Preview` / `Restore` / `Confirm` / `Ignore`）。**纯函数不改状态**：宿主收到 `Preview(k)` 时
  自己写 `state.selected = k` 并 `app.set_theme(k)` —— 否则预览与高亮会各走一条道。
- 出口由 `ThemeModalClose` 在**类型上**区分「要不要落盘」：

| 键 | 转移 | 效果 |
|---|---|---|
| `↑` / `↓` | `Preview(prev/next)` | 立即 `set_theme`（环形，`ALL` 顺序即展示顺序） |
| `Esc` | `Restore(opened_with)` | 还原到打开前的主题，**不落盘**（`Cancelled` ⇒ 不发请求） |
| `Ctrl-C` | 先还原，再 `Err(PromptError::Quit)` | 与浮层其余模态一致（`Ctrl-C` = 关程序）；先还原是为了让退出回放的 scrollback 用回原配色 |
| `Enter` | `Confirm(selected)` | `Confirmed` ⇒ 调用方发 `Request::SetTheme`（落盘）并补一行 `theme: <name>` |
| 其余 | `Ignore` | 本浮层无查询框，不做过滤 |

`resolve_prompt` 对 `PickTheme` **一律返回 `Err`**（与 `PickSession` 同形态）：`Prompter::select` 是
`&self`、无回调、只回传最终选中项的显示串，结构上做不了逐键预览。

**复用的代价**：浮层顶部因此有一行常驻筛选输入框 `> `，敲字被 `ThemeKey::Other` 吞掉（不生效、不报错）。
见「分歧与已知限制」第 4 条。

#### 持久化与进程级镜像

- **载体 `~/.yushan/state.json`**（`AppState.theme: Option<String>`，kebab-case 名），**不进 session meta**：
  主题是**全局偏好** —— 跨会话保留，`/new` 不重置，`/resume` 换会话时主题不变。
- **app 侧进程级镜像**：`app_loop::run` 持 `let mut theme: ThemeKind`，启动期由 `state_store.load()`
  初始化（未知 / 缺失 ⇒ `ThemeKind::default()`），`Request::SetTheme` 单向更新并落盘。
  与 `thinking_visible` 同类，但**载体不同**（后者在 `SessionMeta`）。
- **`build_view` 收 `theme: ThemeKind` 参数、不读盘**：它在 app 循环里「每个请求处理完刷新快照」时被调，
  在里面 `state.load()` 会变成每轮对话一次 IO。主题**必须**进视图（UI 要它上色），故只能走参数。
- **`login` / `logout` 保留主题**：`capabilities.rs` 的三处 `AppState` 序列化一律**先 `load()` 再改自己那几个字段**，
  不得整份构造字面量 —— 整份构造会把用户选的 `theme` 一起抹掉（症状是「重启后主题丢了」，且几乎无法归因到 `login`）。
- **app 侧的 `SetTheme` 臂不回显**：UI 侧（`/theme <name>` 经 `apply_local`、浮层确认经 `run.rs`）已经写了
  一条 `theme: <name>`，本臂只更新镜像 + 落盘。与 `SetThinking` 同一纪律 —— 否则每条命令打两遍。
  非法名（协议层不校验）在此只给一行 `Warning`，**不落盘、不改镜像**。
- **运行期真值在 UI 侧**：`App.theme` 由 `App::new` 从视图快照消费**一次**，此后**不被** `Outbound::View`
  覆盖 —— 切主题是 UI 本地动作（`/theme`、浮层预览），app 推送的快照只是持久化镜像的载体。
  与 `thinking_visible` 同一纪律，由 `test_view_snapshot_does_not_override_theme` 钉住。
- **协议只承载名**：`Request::SetTheme { theme: String }` —— **协议层**不解析、不校验（`ThemeKind` 定义在
  `ys-tui-coding`，`ys-protocol` 不认识 UI crate）。名字的解释权归 owner crate：app 侧自己调同一个
  `ThemeKind::from_str` 校验 + 归一，两个校验点不各自写一套判据。推论：新增主题不需要改协议。

#### 命令表

`COMMANDS` 为 **15 条**（新增 `theme`，`arg_hint = Some("[name]")`）。`/theme` 在**三个面**上同源：
命令表、补全浮层（`/them` → 唯一命中）、`/help` 正文。

#### 紫色守卫

产品需求：任何主题的 **9 个非 `bg` 槽位**（5 套 × 9 = 45 色）都不得落在紫色色相区。判据
`is_purple`：`Color::Rgb` → sRGB → HSL，色相 `∈ [255°, 330°]` **闭区间**、饱和度 `>= 0.20`、亮度
`∈ [0.15, 0.90]` **闭区间**，三者同时满足才判紫。非 `Rgb` 变体返回 `false`（本设计不产生具名色 /
索引色，「是否漏用 RGB」由色板断言负责）。HSL 是最小纯浮点实现，不引色彩库。

改色板时若触发守卫，**改色板而不是放宽判据**。

#### 测试与变异

| 落点 | 覆盖 | 变异点（注入 ⇒ 变红） |
|---|---|---|
| `theme.rs` | 名称往返 / 大小写 / `_` 归一化 / 未知名回显；`next`-`prev` 互逆（同时钉住 `ALL` 顺序 == 变体声明顺序）；45 色不落紫色区（三道具门限**各有**「只有它决定结果」的样本）；10 槽位全为 `Color::Rgb` | 删某道门限 ⇒ 边界样本用例；放宽判据 ⇒ 45 色用例 |
| `styles.rs` | 5 主题 × 关键方法 → 槽位（逐主题跑，防「槽位写死成默认主题的色」）；关键方法不等于 `DefaultStyleSheet` 的主观配色 | 删任一方法的取色 ⇒ 槽位用例 |
| `md.rs` | 无库默认色泄漏：5 主题各渲染一份全元素样例，`fg` ∈ 色板、`bg` 仅 `palette.bg` 或 `None`（**只看 `fg` 会漏掉「白字黑底」**） | 漏覆写 `code()` ⇒ 泄漏用例 |
| `cache.rs` | 换主题后同键也必须重算、新产物只用新色板槽位；同值 `set_theme` 不白清缓存 | `set_theme` 改空操作 / 删同值早退 |
| `layout.rs` | 换主题 ⇒ `recomputed_entries` = 可见条目数；同主题连续 `sync` 不重算（`self.theme` 必须在分支外更新） | 删 `\|\| theme_changed` / 把 `self.theme = theme` 包进分支 |
| `draw.rs` | 整屏每格 `bg`（含输入框内区、状态行、边角，多尺寸）；浮层区域内仍是 `bg`；切主题整屏换 `bg` 走生产路径；chrome 六类条目各取对应槽位 | 删整屏铺色 / 删某处 `Clear` 后补铺 / 漏接 `chrome(app)` |
| `app.rs` | 两处记忆都被作废（**直接断言** `layout.total_rows() == 0`）；切主题真的重渲染；`/resume` 不改主题；`App::new` 消费视图初值；快照不覆盖 | 删 `md_cache.set_theme` ⇒ 重渲染用例 |
| `commands.rs` / `events.rs` | `/theme` 三态；本地立即生效 + **恰好一条** `SetTheme`、无参不发请求；归一化名同步 | 无参改直发 / 删同步块 / 载荷传原始串 |
| `run.rs` | `theme_modal_step` 穷举 `(ThemeModalState, ThemeKey)`；`Esc` 还原 `opened_with`；环形；每帧预览都调 `set_theme`；确认发请求；其余键 `Ignore` | `Esc` 改成 `Confirm` |
| `prompter.rs` | `PickTheme` **结构上**拒绝走通用 `Prompter` 路径（返回 `Err`，且不向 `Prompter` 发问） | 放行 `PickTheme` ⇒ 用例 |
| `state.rs` / `view.rs` / `app_loop.rs` / `capabilities.rs` | 旧 `state.json`（无 `theme` 键）仍解析且不丢老字段；`state.json` → 视图 → `App`；未知主题名回落默认；`SetTheme` 真落盘 + 真更新镜像 + 非法名不改镜像；`login` / `logout` 不清 `theme` | `login` 整份重建 `AppState` |
| `ys-protocol/src/request.rs` | 往返逐字节保真，且钉住线格式（变体名 `SetTheme` + 字段名 `theme`） | 改变体名 / 字段名 |

**变异脚本 `scripts/theme-mutation.py`** 三条注入，全部实测「红（抓到）」：

1. **M1** 删 `App::set_theme` 里的 `md_cache.set_theme(...)` → `app::tests::test_set_theme_invalidates_both_caches`
2. **M2** `login` 整份重建 `AppState` 清掉 `theme` → `capabilities::tests::test_login_and_logout_preserve_theme`
3. **M3** `theme_modal_step` 的 `Esc` 改成 `Confirm` → `run::tests::test_theme_modal_escape_restores_opened_with`

三条脚本级纪律：

- **只认「预期用例名出现在 FAILED 列表」**，不看退出码 —— 否则「过滤后 0 个用例」的 `cargo test`
  会以退出码 0 静默通过，被当成「测试抓不到」的反面（假红）。
- **`ys-coding-agent` 没有 lib target**（单测长在 bin crate 里）⇒ 对该 crate 必须用 `--bins`；
  `cargo test --lib` 会显示「running 0 tests」并**静默通过**。
- 与 `wheel-mutation.py` 不同，本脚本**不用 `git checkout --` 还原**（注入落在尚未提交的工作区，整份还原则
  会抹掉整批改动）：它在内存里保存原文、`finally` 写回，粒度是「只还原自己动过的那一处」。

#### 分歧与已知限制（逐条，缺一即文档滞后）

1. **`layout.reset()` 对「重渲染」是冗余的 —— 一处已知的等价变异。** `TranscriptLayout::sync` 的
   `theme != self.theme` 判据已能自愈，故删掉 `set_theme` 里的 `layout.reset()` 后**颜色断言仍绿**。
   保留它的理由：立即释放已上色的行，并让「两条链一起作废」这条不变量在 `set_theme` 处自明。
   正因如此它**不得**伪装成被颜色用例捕获 —— 兜底的是直接断言 `layout.total_rows() == 0` 的
   `test_set_theme_invalidates_both_caches`。
2. **`Palette` 按值而非 `&'static`，`AssistantStyles::new` 收 `Palette` 而非 `ThemeKind`。** 前者让
   调用方可自由持有 / 比较色板（缓存失效直接比值）；后者消除了「色板 → 主题 → 色板」的反查 ——
   渲染层只需要 10 个颜色槽位，不需要知道它来自哪套内置主题。
3. **`build_view` 形参达 8 个**（已 `#[allow(clippy::too_many_arguments)]`）。这是「不读盘」的对价：
   函数签名旁的文档明写**不再加参数** —— 再有「每请求都要刷新的单个偏好」应连同
   `stats` / `session_started` / `theme` 一起收进一个 `ViewInputs` 结构体。
4. **浮层顶部多一行无效筛选框。** 主题浮层复用 `Modal::Select`，故带上了它常驻的 `> ` 输入框；敲字被
   `ThemeKey::Other` 吞掉（不生效、不报错）。换来的是浮层框架（`Clear` + 背景补铺、页脚键位、滚动）
   只有一份实现。
5. **`AppState.theme` 的 `#[serde(default)]` 对 `Option<T>` 是冗余的**：缺键本就解成 `None`。它是
   风格一致性 + 意图声明，**不是正确性开关**。真正会让旧 `state.json` 读不出来的是把字段写成
   **非 `Option` 且无默认值**（那时缺键 ⇒ `from_str` 失败 ⇒ `load()` 回落 `AppState::default()`
   ⇒ `last_active_model` 一起丢）。故这一条的变异点**不是**「删 `#[serde(default)]`」（删了用例仍绿）。
6. **紫色守卫的端点口径**：色相 `[255, 330]`、亮度 `[0.15, 0.90]` 均**含端点**，饱和度 `>= 0.20`。
   实现与测试须同一口径；`is_purple` 另有「只有某一道具门限决定结果」的边界样本，否则删掉某道门限仍可能全绿。
7. **正文补色在 `md::render` 里做，不在 `StyleSheet` 里。** `tui_markdown::StyleSheet` 没有
   「正文」方法 —— 库对正文 span 产出空样式（继承终端前景）。故 `render` 对「库未上色（`fg.is_none()`）」
   的 span 统一补 `palette.fg`，`styles.rs` 侧则给需要**非**正文色的元素显式上色（标题 / 代码 / 引用 /
   链接 / 表格 / 定义列表）。**推论**：任何新增的 `StyleSheet` 方法若有意「不上色」，必须显式写成
   `fg(palette.fg)`，否则会被补成正文色。
8. **链接与表格显式取色。** `link()` 取 `info` + 下划线（若不上色，会被正文 `fg` 吞掉、只剩下划线可辨）；
   `table_header` / `table_cell` / `definition_term` / `definition_description` 取 `fg`，表头与术语的
   层次只靠 `BOLD`，不引第二套前景色。

---

## 15. debug 结构化执行日志（默认开启）— as-built

设计见 `docs/arch/agent-observability/design.md` + `adr-debug-log.md`；执行计划
`docs/exec-plans/2026-10-17-debug-log.md`。本节只记实际形态。

### 15.1 开关与文件

- `--debug` 在 TUI 首部 flag 区与 `-p` / `--json` 模式 flag 之后都可出现，与
  `--stats` / `--resume` **顺序无关**（`split_leading_flags` 循环剥离，重复 last-wins）。
  任务文本一旦开始就不再剥离（`-p "任务 --debug"` 的文本逐字保留）。
- `YUSHAN_DEBUG` 未设置或为空串时**默认开启**；非空值去除首尾空白并忽略大小写后，
  `1/true/yes/on` 开启，其余非空值关闭，且**优先于命令行 `--debug`**。`--debug` 仍被解析器接受以兼容脚本，但不是创建 sink 的必要条件。
  `YUSHAN_DEBUG_ARGS` 控制工具参数摘要档位（`none` / `head` / `full`，默认 `head`）。
- 文件：`logging::resolve_log_dir()` 一次解析出 `path + source`，随后 sweep、初始 sink 与
  `/new` / `/resume` 的后续路径都消费同一结果；路径为 `path/debug-<session stem>.jsonl`
  （`YUSHAN_LOG_DIR` 一次覆盖日志与 debug 两个产物）；会话尚未落盘时为
  `debug-<pid>-<unix_ms>.jsonl`。追加写。
- 显式关闭时 `open_enabled` 返回 `None`：**不创建 debug 文件、writer 线程或 sink**。
  正常启动仍会在 help / 进程级 worktree 早退之后解析一次 `LogDir`，并对 `Default` 目录
  执行启动期 sweep；`help` 与进程级 `worktree create/list/remove` 在创建 sink 前早退，
  不产生 debug 文件。
- 正常启动在 help / 进程级 worktree 早退之后、sink 创建之前清理一次 `Default` 目录：
  只删除修改时间超过 7 天、文件名匹配 `debug-*.jsonl` 的普通文件，不跟随符号链接；
  同名目录、符号链接、近期文件、非匹配文件与 `yushan.log` 保留。`YUSHAN_LOG_DIR` 非空为
  `Override`，照常写入但不清理。清理独立于开关，`YUSHAN_DEBUG=0` 时仍执行，失败静默跳过。

### 15.2 记录形状

一行一条 JSON，信封含 `schema`（`yushan.debug/1`）/ `ev` / `t_ms` / `dt_ms` / `seq` /
`run` / `session_id` / `turn` / `round` / `tool_call_id`（按需）。记录类型：

| 段 | 记录 |
|---|---|
| 运行 | `run_start`（版本 / pid / os / cwd / session_file / model / api_base 只留 scheme+host / limits / tools / skills / env 变量名）、`run_end`（stop_reason + `debug{records_written,dropped,write_errors,bytes_written,hot_path_ms}`） |
| 回合 | `turn_start`（input_bytes / budget_ms）、`turn_end`（rounds / elapsed_ms / budget_remaining_ms / cache 读数） |
| 轮 | `round_start`、`round_end`（ttft/gen/tools/total、thinking/text 首时刻与段长、增量计数、tool_calls） |
| 调用 | `round_usage`（`purpose=turn/compaction` + 逐轮 usage，含 `reasoning_tokens` 三态）、`tool_call`（参数摘要 + `args_*`）、`tool_result`（耗时 / 大小 / 错误位，**不记内容**）、`model_attempt`（每次 HTTP 尝试的 outcome / reason_class / http_status / ttfb / headers / duration / backoff / 抑制原因） |
| 边界 | `boundary {phase: enqueued/observed, kind, origin}`（CLI 的预算 Abort 在 `push` 同刻用可克隆 `DebugHandle` 写 `enqueued`）、`context_compacted`、`context_compaction_failed`、`response_truncated` |

`ModelTextDelta` / `ModelThinkingDelta` **零记录**（只累计计数）；`t_ms - dt_ms` 恒等于
`run_start_unix_ms`。`run_end` 另带 `rounds` / `duration_ms` / `exit_code` / 累计 `usage`
（`stop_reason` 是终局事件名，预算取消 / 无输出由 `exit_code` 区分）。

`/new` 换文件；同一 session 的 `/resume`（含**跨进程** CLI `--resume`）追加并在打开时按文件里
最大 `run` 递增；跨 session 的 `/resume` 切到目标会话的文件。

`model_attempt` 由适配器的同步回调经 `ModelTraceRouter` 写入当前 debug 文件。模型 factory
早于 debug 文件打开，故路由器先保持空态，各入口在 sink 就绪后 `attach`；`/new` /
`/resume` 后重新绑定。`attempt` 从 1 起，`max_attempts` 是总尝试数（配置的额外重试数 + 1）；
`ttfb_ms` 只取响应体首个非空字节，HTTP 错误与传输失败为 `null`，另由
`response_headers_ms` / `duration_ms` 解释。回调只挂在主模型 factory；子 agent 使用的是
按模型名构造的 factory，其尝试仍不可见（与 §4.8 的既有边界一致）。该记录不带
`turn` / `round`：适配器看不到循环状态；消费侧记录与适配器旁路写入是并发的，
文件顺序不能保证严格夹在 `round_start` / `round_end` 之间。聚合时按时间邻近理解，
不得把它当轮次强外键。

### 15.3 退出与既有行为

- `main.rs` 的 `process::exit` 收敛为 **1 处**（`exit_with_run_end`：`flush_run(meta)` →
  `process::exit`；`exit_with_debug` 是它的简化入口）。`-p` / `--json` 的**模型错误 / 持久化
  失败也走同一出口**，因此失败运行同样有 `run_end`。
- TUI 正常退出 / 出站信道关闭（UI 已收摊）前都 flush；`APP_THREAD_GRACE`（3 s）大于
  `FLUSH_TIMEOUT`（2 s），慢盘下 runtime 不会先掐掉 `run_end`。
- `yushan.log` 的逐轮文本行**逐字节不变**（`RoundClock::observe` 与 `observe_debug` 共享状态机，
  文本行由同一函数产出）；debug 内容**只进文件**，不占 stdout / stderr。

### 15.4 落地数字

- `cargo test --workspace`：**1257 passed / 0 failed / 11 ignored**。
- 新增集成验收 `apps/coding-agent/tests/debug_log.rs`（7 个用例 + 1 个手动 golden 生成器）：
  T1（debug 关零文件、既有诊断不变、stdout/stderr 不被污染）、T2（过滤 `RoundUsage` 后与
  基线 golden 逐字节相同）、T5（flush 后文件完整、末行 `run_end`、run_end 字段齐全）、
  T7（强制压缩时 `turn` 与 `compaction` 同时出现）、失败退出仍有 `run_end`、预算 Abort 的
  `enqueued`/`observed`、跨进程 `--resume` 的 `run` 递增；T5 另断言成功模型调用产生
  `model_attempt`。子进程带 30 s 看门狗。
- `cargo clippy --all-targets`：**无新增 warning**（当前 28 个站点，基线 34 个；差异为既有 warning
  的行号漂移、5 处 `collapsible_if` 与 2 处 `too_many_arguments` 消失）。

### 15.5 基准接线

`apps/coding-agent/tests/e2e_bench.rs` 现在以 `-p --debug` 运行，并**优先解析本运行会话的
debug JSONL**（按 `run_start.pid` 选中，避免模型自起子进程的文件混入）；文本日志仅作回退。
逐次运行与中位数汇总新增：

- `attempts`：HTTP 尝试数（含重试与 `stream_options` 回退）；
- `retries`：实际进入退避的重试次数；
- `retry_wait`：实际进入退避的累计等待；
- `cache`：`round_usage` 的 `cache_read / input_tokens`。

这把 `ttft` 长尾拆成「排队/prefill」「重试退避」「传输失败」三类，并保留
`round_end` 的 thinking/text 分段作为原始证据。

## 16. 推理强度 `reasoning_effort` — as-built

决策与 A/B 依据见 `docs/adr/0042-default-reasoning-effort-high.md`。

- `YUSHAN_REASONING_EFFORT` 取 `minimal|low|medium|high|xhigh|max`，大小写/空白归一化；
  未设置或非法值回落默认 **`high`**。非法值落一行诊断。
- `Config::reasoning_effort()` 给出配置值；模型 factory 再经**两层**门控决定是否真正发送、
  以及发什么值：
  - **provider 级** `ProviderCompat::supports_reasoning_effort` —— 当前 `opencode-go` 与
    `glm` / `glm-coding` 声明支持；`deepseek` / `minimax` / custom 与未知端点不发送该字段。
  - **模型级** `ProviderCompat::supports_effort(model)`（`Glm` 方言要求代际 ≥ 5.2，
    无法解析代际时 fail-closed），实际下发值再由 `effort_for` 按方言折叠：
    `glm-5.3` 上 `off|minimal|low → low`、`medium|high → high`、`xhigh|max → max`；
    `glm-5.2` 上 `off|minimal → minimal`、`low|medium|high → high`、`xhigh|max → max`。
    默认方言 `Verbatim` 逐字透传。详见 `docs/design-final/effort.md` §3 与
    `docs/design-final/glm.md`。
- `ChatCompletionRequest.reasoning_effort` 带 `skip_serializing_if`：库测试里 `None` 时
  wire JSON 不出现该键，`Some("low")` 时发送 `"reasoning_effort":"low"`。
- `run_start` 的 `gen_ai.request.reasoning_effort` 记 **wire 值**（门控与折叠之后）；
  字段不进请求时写 `null`。`app_loop` 的 `model_changed.reasoning_effort` 同一口径。

真实 A/B（同模型 `opencode-go/deepseek-v4.1-flash`、`fact/1`）：旧行为中位 199.2s，
`low` 为 72.4 / 88.6s 但质量 2/3，默认 `high` 三次为 64.4 / 100.7 / 201.4s，
中位 100.7s，且质量均 3/3。
默认因此取 `high`；详细口径见 `docs/arch/explore-speed/cost-model.md` §8.5。

## 17. CLI help — as-built

`HELP_TEXT` 是纯文本常量，**自身不带尾换行**；`main` 用 `println!("{}", help::help_text())`
输出，因此 stdout 恰以一个 `\n` 结尾，且不以 `\n\n` 或 `\r\n` 结尾。`--help`、`-h` 与
`help` 三种触发形态逐字节一致。help 在 `Config::from_env`、运行时、凭证读取与 debug sink
创建之前早退；即使 `YUSHAN_DEBUG` 未设置（默认开启），help 也不产生 debug 文件。

## 18. 滚动：视窗化渲染 + 视窗原地不动 — as-built（ADR-0045）

### 18.1 滚动模型

| 概念 | 含义 |
|---|---|
| `App::scroll_offset` | 距底部的**视觉行数**（0 = 贴底）—— 语义未变 |
| `App::follow` | 贴底跟随 —— 语义**变了**：不再被新内容强制置真 |
| `App::new_content_below` | 脱离底部期间下方来了新内容 ⇒ 状态行显示「↓ 有新内容」 |
| `App::last_total_rows` | 上次同步布局时的总行数 —— 增量补偿的基准 |

**视窗原地不动**：脱离底部时，内容在**尾部**增长 Δ 行 ⇒ `scroll_offset += Δ`
（它是「距底部行数」）⇒ 视窗顶部那一行不动。宽度变化时行号空间整个换掉 ⇒ 不补偿。
`follow == true` 时 `scroll_offset` 恒为 0 ⇒ 视窗贴底。

`follow = true` 只在**用户显式动作**后恢复：提交新回合、回合中插话提交、`/resume` 回放、
`/thinking` 切换、命令类本地输出、Ctrl-C 取消提示、跳到底部。**新内容到达不再拽视窗**。

`follow = false` 同理只在**用户真的在阅读历史**时发生：滚动（`↑` / `PageUp` / 滚轮 /
`Ctrl+Home`）、**指针确实移动**的拖选。

**单击不算**：`Down` 只记锚点，不碰 `follow`；`extend_selection` 里以「指针离开了按下点」
（`anchor != cursor`）为判据才冻结视窗 —— 不按「收到一个 `Drag` 事件」判，因为触控板 /
高精度鼠标在单击时也会补发一个坐标相同的 `Drag`（见 ADR-0058）。

反过来，**回合中插话提交也必须恢复跟随**（ADR-0058）：用户敲了回车就期望看到自己那条与
模型的回应，不恢复的话会被永久留在旧位置。

### 18.2 视窗化布局（`crates/ys-tui-coding/src/layout.rs`）

- `TranscriptLayout` 逐条目记住折行结果：`EntryMemo { sig, render_sig, lines, row_starts }`。
  - `sig` = 条目内容签名（**廉价**：变体 + 各字符串长度）⇒ 变了才重渲染。
  - `render_sig` = 已渲染内容哈希 ⇒ 流式节流窗口内「源变、产物没变」时**连折行结果一起复用**。
  - `row_starts` = 该条目每个渲染行折成几个视觉行（ratatui `Paragraph::line_count` 逐行算，
    **不自写折行** —— CJK 列宽口径必须与渲染一致）。
- `starts`（每条目块起始行号）+ `gaps`（前置留白行数）+ `total_rows` = 全局行号空间。
- `window(top_row, height)`：二分定位条目与行内偏移 ⇒ 取出覆盖视窗的行 +
  「首行内部要跳过几行」（`Paragraph::scroll` 的纵向分量）。

**承重不变量**：「整段折行 == 逐行折行之和」（N4）。视窗切片依赖它 ——
`draw.rs` 的 `test_windowed_render_matches_full_render`（视窗渲染与整段渲染**逐行相同**）是守卫。

**失效判定**：宽度变化 / thinking 可见性变化 ⇒ 全量作废；条目 `sig` 变化 ⇒ 该条目重渲染 +
重算折行；`render_sig` 未变 ⇒ 折行结果复用。**只重算变化的那一条**。

### 18.3 实测

同一条会话（100 列宽），一次滚动数学（`compute_max_scroll`）：

| 规模 | 旧 | 新 |
|---|---|---|
| 800 行 / 525 KB | 5.6 ms | ~53 µs |
| 3200 行 / 2.1 MB | 23 ms | ~43 µs |

新实现**与内容长度无关**（3200 行反而略快，纯属噪声）。
流式增长时（末尾那条持续变长）同样在 ~50 µs 量级 —— 折行结果按条目记住，只重算变化的那一条。

### 18.4 键位

> **§18.4 已被 ADR-0057 改写**：`↑↓` 先归输入光标、裸 `End` 归输入行尾、新增输入框选区。
> 下表是当前形态。

| 键 | 行为 |
|---|---|
| `↑` | 输入框**按视觉行上移**（软换行也算一行）→ 到首行才取会话内历史 → 历史空则逐视觉行滚动 | 
| `↓` | 输入框**按视觉行下移** → 到底才回落：浏览态内取历史下一条（越过最新即还原草稿并退出浏览态），否则逐视觉行滚动 |
| `PageUp` / `PageDown` | 翻页，步长 = **视窗高度 − 1**（聊天滚动的主入口） |
| 滚轮 / 触控板上下 | 逐 **1 视觉行**滚动聊天（与输入历史**无关**）；脱离底部即停止跟随 |
| `Home` / `End` | 输入**当前行**的行首 / 行尾（`Ctrl-E` 与 `End` 同义；`Ctrl-A` **不是**行首，见下） |
| `Ctrl-End` / `Ctrl-Down` | 跳到最新消息 |
| `Ctrl-Home` / `Ctrl-Up` | 跳到最早消息 |
| `←` / `→` | 输入光标（`Ctrl` 或 `⌥` 按词；`⌥b` / `⌥f` 同义） |
| `Shift` + `←→↑↓ Home End` | **扩选**（锚点钉在移动前的位置；端点重合即收起） |
| `Ctrl+A` | **全选输入框**（`Ctrl+Shift+A` 等价；随后 `Backspace` / 打字替换整段） |
| `Ctrl+V` | **粘贴系统剪贴板**到输入框（有选区则替换整段；读不到时无反应） |
| `Ctrl+C` | **有选区 ⇒ 复制到系统剪贴板**；无选区 ⇒ 取消回合（回合中，再按一次退出）/ 退出 |
| `Backspace` / `Delete` / 打字 · 粘贴 | 有选区 ⇒ 替换整段；否则删/插一个 char |
| `Enter` / `Shift`·`Alt+Enter` / `Ctrl+J` | 提交 / 换行 / 换行（终端无关的换行键） |
| `Tab` | 补全浮层（`/` 开头时） |
| `Esc` | 取消回合（或命令在飞）→ 否则清空输入 |
| 左键拖选（Chat） | 应用内选区 + 松手即复制；`⇧`+拖选把原生选择让回终端 |

**`↑↓` 与历史的走廊规则变了**（ADR-0057 决策 4）：软换行让 `↑` 在长草稿里成了编辑操作，
「无条件先问历史」会把草稿整段换掉。现在输入框有得动就先动它，到边界才回落。
**代价**：`↑` 取历史要多按几下；`PageUp` / `Ctrl+↑` 不受影响。

**多行输入框现在上下移动输入光标**（`InputBuffer::visual_row_col` / `index_at`，与
`draw::wrap_input` 同一个折行口径）。折行宽度由 `ui_with` 每帧写进 `InputBuffer::wrap_width`
—— 缓冲层**不猜**宽度：未写过时 `up()` / `down()` 返回 `false`，直接回落历史/滚动。

**全选走裸 `Ctrl+A`**（ADR-0059）：它在 raw 模式下是 `0x01`，**所有终端都送得到**；
`Ctrl+Shift+A` 依赖 `DISAMBIGUATE_ESCAPE_CODES`（支持的终端发 `CSI 97;6u`），压不成修饰键
的终端把它退化成 `Ctrl+A` —— 两档都是全选，所以压不压得成都按得出来。**代价**：
readline 的 `beginning-of-line` 没了，行首走 `Home`（`Ctrl+E` 仍保留为行尾，与 `End` 同义）。

**`Ctrl+V` 是应用内粘贴**（ADR-0059）：终端自己的粘贴键（`Ctrl+Shift+V` / `⌘V`）由终端读
剪贴板并塞成 bracketed paste（`Event::Paste`）；裸 `Ctrl+V` 是控制字符 `0x16`，终端不解释
它 ⇒ 字节到得了应用，于是这里接住它：置 `App::pending_paste`，由 `run.rs` 在本轮末尾用本机
工具（`wl-paste` / `xsel --clipboard --output` / `xclip -o` / `pbpaste`）同步读出来插入缓冲。
**不发 OSC 52 读请求** —— 那要求终端回写到 stdin，与 crossterm 的事件读取抢同一字节流。
读不到（无工具 / SSH 远端 / 空内容）⇒ 什么都不发生，改用终端的粘贴键。

**不绑 `⌘` 组合**（macOS 终端自己消费 `⌘`，字节到不了应用）；**不绑 `⌥+↓`**
（Terminal.app 发 `ESC[B`，与普通 `↓` 同形）。`Ctrl+↑/↓`、`Ctrl+Home/End` 终端无关。

**滚轮归应用**（ADR-0050）：`run::setup_terminal` 开 `EnableMouseCapture`，
`events::handle_mouse` 只认 `ScrollUp` / `ScrollDown`（其余鼠标事件忽略）。
不开捕获时终端把滚轮译成 `↑` / `↓` ⇒ 滑触控板翻的是**输入历史**而不是对话。
捕获期间终端原生拖选失效的代价，已由**应用内选区 + 松手即复制**（ADR-0051）消掉 ——
裸左键拖选即可复制，`⇧`+拖选则把原生选择**让回**终端。

**macOS 的两个坑**（键盘侧入口为何不够）：系统级快捷键占用了 `Ctrl+↑` / `Ctrl+↓`
（Mission Control / App Exposé，字节到不了终端，需在「系统设置 → 键盘 → 键盘快捷键 →
Mission Control」里取消勾选）；MacBook 内置键盘**没有独立 PageUp / PageDown**
（须 `fn+↑` / `fn+↓`）。这也是滚轮进应用的直接理由。

**键位表是结构化数据，不是一段字符串**（ADR-0057 决策 6）：真源是
`commands::all_keybindings()`（`KeyBinding { keys, action, group, note }`），两个出口
从它派生：

| 出口 | 内容 |
|---|---|
| `/keybind` | **完整表**，按 `KeyGroup`（输入编辑 / 提交与退出 / 滚动与跳转 / 剪贴板 / 鼠标）分段，键列按显示宽度对齐，注意事项单独续行 |
| `/help` | 命令索引 + 一段**高频摘要**（`KEY_HELP_BRIEF`），结尾指向 `/keybind` |

分两个出口的理由：`/help` 是命令索引，二十条键位会把它要查的命令挤出屏幕；而键位是
「试不出来就等于没有」的那类信息，需要一个能整屏读完、且与实现同源的出口。

**上一轮的偏差正是这个结构缺口的产物**：`/help` 里那段写死为字符串，实现改了（`End`
归输入行尾、`↑` 先归输入光标）而文案没跟上。结构化之后两个出口共用同一批事实，
`/keybind` 用例逐条断言「每一条 `all_keybindings()` 都出现在输出里」，漂移即红。

### 18.5 会话内输入历史 — as-built

`crates/ys-tui-coding/src/history.rs` 的 `InputHistory`，挂在 `App.history`：

- **只记进程内的提交原文**（`events::submit` 在非空白时才调 `record`）—— 含 `/` 命令与多行原文，不改写。**不落盘、不跨会话、不跨 `--resume`**。
- **`/new` / `/resume` 都不清空它**：它记的是「本进程里敲过什么」，与当前打开哪个会话无关。
- **走廊规则（ADR-0057 改写）**：`↑` 先给**输入框**（按视觉行上移，到首行才继续）→ 再问历史（非空即取，与输入框是否已有内容无关）→ 历史空则回落逐视觉行滚动。`↓` 对称：输入框下移 → 到底才回落（浏览态内取历史下一条，否则滚动）—— 聊天滚动的主入口仍是 `PageUp` / `PageDown` 与 `Ctrl+↑/↓`。
- **草稿**：首次进入浏览时存下输入框原文（含多行 / CJK，原样），`↓` 越过最新一条就还原它并退出浏览态（readline 语义）。
- **去重与上限**：与上一条逐字相同的**不重复入栈**；`MAX_ENTRIES = 200`，超出丢最旧。

**范围外**（未做）：跨会话持久化（`~/.yushan/`）、按 cwd 分片、隐私过滤（密钥/大粘贴）、`Ctrl-R` 搜索。未决问题见 `docs/arch/input-history/context.md` 的 Q2 / Q3 / Q5–Q8，台账 `docs/TODO.md` §C。

### 18.6 已知缺口

- 布局记忆与 md 缓存各存一份渲染行 ⇒ 渲染行内存 ×2。
- 廉价签名只看**长度**：内容变了但长度没变会漏检（极少见，且只原地改写）⇒ 代价是
  「可能没跟上」，不是「算错」。
- 逐行 `line_count` 有分配开销（每条目只在内容变化时算一次）。
- 真实终端的「翻页跟手」观感仍是 `MANUAL_ACK` 项（见 §10）。
- **鼠标捕获期间终端原生拖选失效** ⇒ 已由应用内选区补上（ADR-0051：裸左键拖选 =
  应用内选区 + 松手即复制）。选区自身的代价见 §18.7。
- **模态浮层不吃滚轮**：浮层自读事件、`_ => {}` 掉鼠标 —— 浮层内容尚不滚动（见 `docs/TODO.md` §C）。
- **`Ctrl+V` 在 SSH / 容器里无反应**：远端没有本机剪贴板工具（读取只走工具，不发 OSC 52 读）。
  那时用终端自己的粘贴键（`Ctrl+Shift+V` / `⌘V`）。工具缺失时`Ctrl+V`会走完整条候选链（逐个
  `ENOENT`），延迟以毫秒计但确实在按键路径上 —— 不做能力缓存，否则用户装了工具也没反应。
- **`Ctrl+A` 不再是 readline 的行首**（ADR-0059）：行首走 `Home`。
- **`↑` 取历史的路径变长**：输入框多行 / 软折行时须先回到首行（ADR-0057 决策 4 的代价）。
- **`xsel` 复制后不退出**：X11 selection 的拥有者必须是个活进程 ⇒ 下一个 `xsel` 接管时它才退。
  这不是泄漏，但 `ps` 里会看到它。

### 18.7 文本选区与松手复制 — as-built

鼠标左键拖选 Chat 区 ⇒ 应用内反显高亮；松手把**屏幕字形**写进系统剪贴板。
消掉了「复制内容要靠终端给不给修饰键」这一跳终端差异（ADR-0051）。

**出口（ADR-0057 已改）**：不再只发 OSC 52，而是**本机工具 + OSC 52 双发**（见上节
「复制出口」）。原实现只发 OSC 52，而 VS Code 的 xterm.js 不实现它 —— 那正是主要使用场景，
于是「复制成功」在这类终端上一直是幻觉。

**输入框也有选区了**（ADR-0057）：`InputBuffer::anchor`，`Ctrl+A` 全选、
`Shift+方向` 扩选、有选区时 `Ctrl+C` 复制、`Ctrl+V` 粘贴替换、编辑键替换整段。高亮走同一口径
（`Modifier::REVERSED`）且与 `draw::wrap_input` **共用折行结果** —— 另算一份就会错位。

**模块划分**（唯一新文件 `crates/ys-tui-coding/src/selection.rs`）：

| 项 | 职责 |
|---|---|
| `Selection` | 状态机：`anchor_*` / `cursor_*` / `edge` / `edge_since` + `rects` 复用缓冲 |
| `GlyphGrid` | 视窗字形缓存：`slots: Buffer` + `order: Vec<u16>` 槽环 + `cont` 续格位图 |
| `extract` | 按 `EXTRACT_CHUNK_ROWS` 分块重渲 → 逐行**按内容裁剪** → 读回 |
| `Highlight` | 只改 `Modifier::REVERSED` 样式，**不动 symbol**、**不用 `Clear`** |
| `drag_tick` | 贴边自动滚动（自由函数，六步顺序不可换） |

**坐标模型**：`anchor_row` / `cursor_row` 是 `TranscriptLayout` 的**全局视觉行号**
（按下时 = `top_row` + 内区相对 y），`x` 是内区相对屏幕列。因此选区能**超过一屏**，
且 `anchor_row` 是逻辑坐标、不随滚动改变。**自动滚动只改 `cursor_row`。**

**内区几何只有一个来源**：`chat_inner(app, area)` 期望的是**整屏** `area`，它内部自己切
Chat / Input / Status 三块。`ui_with` 已切出 `chunks[]`，故高亮层**必须**接收
`chat_inner(app, area)` 的结果而非 `chunks[0]` —— 把已切好的 Chat 分块再喂给 `chat_inner`
会二次切分，内区被多扣掉「Input 高度 + 状态行」，高亮与 `GlyphGrid` 随之短掉那几行
（贴 Input 的几行拖过不变色）。守卫：`draw::tests::selection_covers_every_row_of_chat_inner`。

**提取不受上面那条影响**：`extract` 另建一份 `GlyphGrid` 按 `layout` 重渲，读的是布局而非
视窗缓存 —— 故几何算短时「剪贴板里有、屏幕上看不见」，而不是复制缺内容。

**九条缓存失效**（设计 §2.3 的落地）：键 = `layout_rev`（`structural_rev`）+ `win_rev`
（`rev_alloc`，内容代次）+ `inner` Rect + `base_row`。仅 `base_row` 变 ⇒ `shift(k)`
槽环旋转；其余 ⇒ 全量 `rebuild`；`Idle` ⇒ `release()`（保容量、还内存）。

> ⚠️ **`win_rev` 用的是 `rev_alloc` 而不是 `revision_range`**：后者把「窗口首条目内偏移」
> 折进了哈希，而滚动本身就会改这个偏移 —— 拿它做键的话「纯滚动」永远被判成内容变化，
> 槽环永远不会生效。代价是窗口**外**条目重渲也会打一次全量重建（正确性优先）。

**复制出口**（ADR-0057 决策 1）：`write_clipboard` = **本机工具 + OSC 52 双发**。
本机工具按序试 `wl-copy`（仅 Wayland）→ `xsel --clipboard --input` → `xclip -selection clipboard` →
`pbcopy` → `wl-copy` → `clip.exe`，命中即返回，文本写进子进程 stdin 后**立即关 stdin**
（`xsel` 靠 EOF 判断粘完，但它写完不自己退 —— X11 selection 的拥有者必须是个活进程，
故不 wait、不 kill）。OSC 52 仍然发：那是 SSH / 容器 / tmux 里**唯一**可用的通路。
两条都失败时静默（写入无应答可等，探测做不到），用户退路是 `⇧`+拖选。

**复制上限** `COPY_MAX_BYTES = 256 KiB`：超限**不复制**、不静默截断，transcript 回一行提示。
**自动滚动**：`DRAG_TICK = 16ms` 可变 poll 周期 + `step = clamp(1 + held_ms/40, 1, 8)`，
200 拍实测选区增量 ≈ **366 µs**（固定 UI 开销另计）。

**`⇧` 语义**：拖选的 `⇧` = 把原生选择**让回终端**（不是退路）。

**已知代价**：

| 项 | 代价 |
|---|---|
| 选区不跨**用户滚动** / **resize** 存活 | 有意（决策 5/6）；松手后 resize 无害 |
| 流式拖选时缓存重建 ≤ 10 次/秒 | 受 `RENDER_THROTTLE` 天然限流 |
| 字形存两份（`layout` + `GlyphGrid`） | 视窗期 ≈ 120 KB；`Idle` 时释放 |
| 双击选词 / 三击选行 | 未做（`SelectionMode` 不建） |
| `⇧` 退路在 VS Code+macOS 仍不存在 | 不是本设计缺陷；`⇧` 只是「让回」开关 |
| 真实终端手感 | `MANUAL_ACK`；客观替代闸门是 3 个 `#[ignore]` 基准 |

**实现的四处明确化**（与 `docs/arch/tui-text-selection/design.md` 的差异）：

1. `Phase` **两态**（`Idle` / `Dragging`）：`Done` 无观测点（松手即 `clear()`）。
2. `TranscriptLayout.rev` 拆成 `structural_rev`（结构）+ `rev_alloc`（内容代次）。
3. `window_rows()` 落地为 `window_with_rows() -> (Window, Vec<RowRef>)`：`window()` 传
   `None` 保持零分配，二者共用 `window_impl` 一次遍历。
4. `drag_tick` 是**自由函数**（避 `app.selection` 与 `&mut app` 的借用冲突），
   且**只动 `cursor_*`**、不动 `anchor_*`。

**变异测试**：`python3 scripts/selection-mutation.py`（8 处注入，全部可被抓到）。

## 19. 后台命令执行（execution-efficiency）— as-built

> ⚠️ **本轮已被 §20 部分取代**：`bash_poll` 已删除，结果回收从「模型轮询」改为
> 「完成时自动注入」。本节保留**仍然成立**的部分（落盘目录、输出上限、进程组清理、
> `bash_bg` 的启动语义），凡提到 `bash_poll` / 轮询 / 增量交付 / 父表 12 元的描述**以 §20 为准**。

本节只记**已成立**的事实。落盘目录契约见
`docs/arch/execution-efficiency/design.md` §4.5。

### 19.1 父/子工具表元素数

- **父表 = 11 元**：基础 9 元（`apps/coding-agent/src/toolset.rs:88` 的 `build_tools`）+
  `bash_bg` + `SubagentTool` **追加末位**（`apps/coding-agent/src/assembly.rs`
  的 `AgentFactory::build()`）；`bash_bg` 排在 `subagent` **之前** —— 顺序即契约。
  （`bash_poll` 已删除，见 §20。）
- **子表 = 9 元**（不含 `bash_bg` / `subagent`）⇒ 派发**结构上**不可能递归，
  子 agent 天然没有后台能力。
- 前缀性质：子表 JSON 是父表 JSON 的**字节前缀**（父表 = 子表去掉末尾 `]` +
  追加段 `,{bash_bg…},{subagent…}]`）⇒ 命中 prompt cache。
  真实装配产物的字节级断言在 `apps/coding-agent/src/assembly.rs` 的
  `real_parent_table_keeps_the_child_table_as_its_byte_prefix`（元素数 + 前缀 + 追加段逐字节）
  与 `real_parent_table_ends_with_the_subagent_tool`（末位）。
  `apps/coding-agent/src/toolset.rs` 那条顺序用例**自拼**父表 ⇒ 不描述真实父表。
- **装配条件**：后台上下文未注入时父表少两项；生产装配路径恒注入 ⇒ 父表恒为 12 元。

### 19.2 六个清理点

后台命令的**注册表**（`adapters/tools-basic/src/background.rs` 的 `BackgroundTasks`，
进程级单例经 `apps/coding-agent/src/wiring.rs` 的 `BackgroundContext` 持有）与
**回收机制**（app 侧的六个显式清理点）**均已落地**；六处时机各不相同：

| 时机 | 落点 | 动作 |
|---|---|---|
| abort | `apps/coding-agent/src/app_loop.rs:1034`（`Boundary::Abort` 臂） | `kill_all(session)` |
| 回合预算到点 | `apps/coding-agent/src/app_loop.rs:1062`（`sleep_until(deadline)` 臂，同样 push `Boundary::Abort`） | 同一路径 ⇒ 同一动作 |
| `/new` | `apps/coding-agent/src/app_loop.rs:555`（换会话**之前**） | `kill_all(session)` + 删除该会话输出目录 |
| `/resume` | `apps/coding-agent/src/app_loop.rs:620`（恢复成功后） | `kill_all(session)` |
| 进程退出 | `apps/coding-agent/src/main.rs:2058` **之前**（清理调用在 `main.rs:2057`） | `kill_all(全部)`（`std::process::exit` 不跑析构 ⇒ `Drop` 兜底在此路径无效） |
| panic（unwind） | `impl Drop for BackgroundTasks` | 兜底杀全部进程组（`Drop` 只能用同步 kill） |

- `kill_all` **按会话过滤** ⇒ 漏掉的清理点不会波及别的会话。
- **会话 id 的取法**：全部取自**会话槽**（`wiring.backgrounds().session_id()`）——
  与工具给注册表条目打标签用的是**同一份**槽 ⇒ 必然命中。
  `/new` 与 `/resume` 都是**先存旧 id、再换会话**：拿换之后的 id 去清理会**打空**。
- `/resume` **仅在会话 id 变化时**才清理（同会话追加同一文件 ⇒ 不动，
  否则会杀掉本会话自己的命令）。
- `/new` 的删目录走 `BackgroundTasks::remove_session_dir`
  （`adapters/tools-basic/src/background.rs:587`）：只允许删 `<root>` 的**直接子目录**，
  会话 id 含路径分隔符或 `..` 一律拒绝；目录删不掉（权限 / 并发占用）**不影响控制流**。
- **共享 sweeper**：`apps/coding-agent/src/app_loop.rs:106`
  （`ensure_background_sweeper`），在 `app_loop::run`（`app_loop.rs:321`）启动 ——
  **全局 1 个** tokio 任务，100ms tick 调 `BackgroundTasks::sweep`；
  已有活着的就不重复启，注册表被丢弃后任务自行收摊（只持**弱引用**）。
- **回合末计数**：`apps/coding-agent/src/app_loop.rs:1163` 记 debug 记录
  `ev = "background_tasks"`、字段 `running` = 本会话仍在跑的条数 ——
  **只进 debug 文件**，不进模型上下文、不进 transcript。

### 19.3 `/model` 的窗口来源

- 窗口值与来源**成对**产出：`Config::context_window_with_source_for`
  （`apps/coding-agent/src/config.rs:280`）在**一次判定**里同时给出两个值 ——
  分开两次判定会漂移，值与来源会对不上。
- 四级来源（`ContextWindowSource`，`apps/coding-agent/src/config.rs:149`）：

  | 级 | 来源 | 标签 |
  |---|---|---|
  | ① | `~/.yushan/config.json` 的 `context_window` 覆盖 | `user override` |
  | ② | provider 目录（`/v1/models`）自报的窗口 | `provider catalog` |
  | ③ | 内建兜底表 `KNOWN_MODEL_WINDOWS`（`apps/coding-agent/src/provider.rs:488`） | `builtin table` |
  | ④ | `RunLimits::default().context_window` = 1000000 | `default` |

- `/model` 的输出追加一行 `Context window: <值> (source: <标签>)`
  （`apps/coding-agent/src/capabilities.rs:279`）；走 `Outbound::Output` → transcript，**绝不 print**。
- 内建兜底表是**精确匹配**，只收录 `MiniMax-M3` / `MiniMax-M2.7`；其余模型落 ④。
  用户级覆盖的 JSON 键是 `context_window`（内部字段名才叫 `windows`，
  `apps/coding-agent/src/config.rs:31`）。
- **② 级的目录在三种模式下的到达方式不同**（窗口能否命中 ② 级的关键）：
  - TUI：起屏后**后台**拉目录（`run_interactive` 的 2b 段），到手后由 `app_loop`
    每回合重算窗口。
  - `-p` / `--json`：**同步**拉一次当前端点的目录（`main.rs` 的「`-p` / `--json`：
    **同步**拉一次」段），在 `resolve_limits` **之前**写入 registry。一次性模式
    只跑一个回合，后台任务来不及送达 ⇒ 不同步拉就整个停在 ④。拉取失败
    **静默降级**到 ④（拉目录只为把窗口估准，不为选模型）。

### 19.4 两个新工具的形态

`bash_bg` / `bash_poll` **已注册**（`apps/coding-agent/src/assembly.rs` 的
`AgentFactory::build()`，排在 `subagent` **之前**）。契约位有两处：

- 提示词侧：工具说明与两条 guideline 已按名字备好
  （`apps/coding-agent/src/prompt.rs:170`、`:173`、`:306`），沿用既有工具的
  **「在场才出现」**守卫形态 ⇒ 子表不含这两个工具 ⇒ 子 agent 天然看不到那两条 guideline。
- debug 侧：命令串指纹已把 `bash_bg` 计入命令类工具
  （`apps/coding-agent/src/debug_log.rs:1088`）。

形态要点：

- `bash_bg` 启动后台命令并**立即**返回句柄；`bash_poll` 查状态 + **只给上次轮询之后的增量输出**。
- 落盘 `<bg_root>/<session_id>/bg<N>.log`；`bg_root` = `$YUSHAN_BG_DIR`
  （**须绝对路径**，与 `YUSHAN_FETCHED_DIR` 同规矩）或 `<temp_dir>/yushan-bg`。
- 句柄 `BgHandle(u32)` 进程内**单调递增、永不复用**；条目带 `session` 字段，poll 时核对
  ⇒ 跨会话的旧句柄命中不到本会话的条目。
- 两个工具**均非** `is_concurrency_safe`（`bash_poll` 会推进注册表里的交付偏移）。
- 触发只取显式（`bash_bg`），回收只取显式轮询（`bash_poll`）；**不做**通知注入。

### 19.5 交付语义（三条，缺一不可）

1. **已送达的字节不重复送达** —— `consumed` 单调推进（`advance_consumed` 只进不退）
   ⇒ 总拷贝 O(n)。
2. **未送达的字节必须明示** —— 头尾裁掉的中间段报出**字节数**与**完整输出路径**。
3. **不静默丢弃** —— 裁剪与字节预算都**不放宽**，裁掉就裁掉，但**要报出来**。

**末行（无换行结尾）的两态规则**：

- **命令仍在跑** ⇒ 末段**不是完整行**，**不交付**，`consumed` 停在最后一个完整行的边界；
  未交付的字节数**明示**（「还有 N B 待交付」）。
- **命令已结束且进程已收尸** ⇒ 输出文件不会再增长 ⇒ 末段**就是最后一行**
  ⇒ **整段交付**，`consumed` 推到文件尾。
  判据是「**进程已收尸**」而非「状态非 Running」：`Killed` 但尚未收尸时子进程仍可能在写
  ⇒ 此刻把半行当完整行交付会交付出**截断的多字节字符**。

**模型可见形态**（`bash_poll` 的真实返回）：

```
[bg1] running — 0.0s elapsed — 0 B produced, 0 B new
[bash_poll] full output: <bg_root>/<session_id>/bg1.log

[bg1] finished — exit code 0 — 2.0s — 11 B produced, 11 B new
alpha
beta

[bash_poll] full output: <bg_root>/<session_id>/bg1.log

[bash_poll] unknown handle "bg1" — no running handles
```

「被 harness 杀掉」与「命令自身非零退码」用**不同措辞**（前者是 harness 的闸门，
后者是命令自身的退码）⇒ 模型不会把被杀误报成「测试挂了」。

### 19.6 输出上限的两个闸门（量纲不同，不可互换）

| 闸门 | 值 | 归因措辞 | 落点 |
|---|---|---|---|
| **精确闸门** | **2 MiB** | 「harness 的输出上限」 | sweeper 的 `stat`（`BackgroundTasks::sweep`）⇒ 命中即**杀进程组 + 记失败** |
| **粗闸门** | `max(2 MiB, 64 MiB)` | 「内核 SIGXFSZ」 | 启动命令时的 `ulimit -f`（内核侧文件写入上限） |

⚠ **`ulimit -f` 是进程级上限，不是「本命令的输出上限」** ⇒ 取 2 MiB 会把
`cargo build` 写 `target/` 的正常构建一起杀掉 ⇒ 粗闸门取 `max(2 MiB, 64 MiB)`，
精确的 2 MiB 由 sweeper 的 `stat` 闸门负责。

### 19.7 展示裁剪（`ys_tool::head_tail`）

- 头尾混合：头 **1/4**（12.5 KiB）+ 尾 **3/4**（37.5 KiB）= `MAX_BYTES` 50 KiB，
  行数预算 `MAX_LINES = 2000` —— **都不放宽**。
- `omitted_bytes` = **源文本中未出现在展示内容里的字节数**（展示内容 = 头 + 尾，
  两段互不重叠；头尾之间的换行符与源文本的**尾随换行**都计入）
  ⇒ 恒等式 `omitted_bytes + head.len() + tail.len() == 源文本字节数`。
- **单行超预算** ⇒ 头部给出该行的**前缀**（≤ 头部预算、字符边界安全），尾部为空
  （同一行不得同时出现在头与尾）⇒ 被裁字节计入 `omitted_bytes` 并报出。
- **输出的拼装只有一份**：`adapters/tools-basic/src/bg_output.rs` 的
  `assemble(text, handle_label, output_path, pending_note)` 是**唯一真源** ——
  行的顺序是「头 → 省略标记 → 尾 → 未送达报出 → 待交付提示 → 完整输出路径」；
  `bg_tools` 只提供「待交付」提示的**措辞**（读域知识），**插入位置**由 `assemble` 决定。

## 20. 后台命令完成通知（bg-notify）— as-built

**ADR**：[`0050`/`adr-bg-notify`](../arch/bg-notify/adr-bg-notify.md)（状态 accepted）。
**设计**：[`docs/arch/bg-notify/design.md`](../arch/bg-notify/design.md)。
**本节取代 §19 里关于「模型显式轮询」的描述** —— `bash_poll` 已删除。

### 20.1 形态

后台命令结束时，由既有的 100ms sweeper 把结果 `push` 成 `Boundary::Deferred`；
`BasicLoop` 在下一个轮边界 append 进会话并 emit `AgentEvent::DeferredResult`
⇒ 模型**不需要轮询**。`Boundary::Deferred` 与 `DeferredResult` 是既有机制，
异步 subagent 早就在用同一条路径。

**父表 = 11 元**：基础 9 元 + `bash_bg` + `SubagentTool`（`bash_poll` 已删）。
子表仍是 9 元 ⇒ 子 agent 无后台能力。

### 20.2 三个部件

| 部件 | 位置 | 职责 |
|---|---|---|
| 落点格 | `BackgroundTasks.deferred` | 每回合由 `app_loop` 用 `bind_deferred` 绑；RAII 守卫 drop 时解绑 |
| 投递状态机 | `Delivery::{Pending, InFlight, Delivered}` | 与 `BgState` **正交**；`repair_inflight` 在每 tick 开头把超 2 tick 的 `InFlight` 回退 |
| 正文渲染 | `bg_output::render_completion` | 唯一渲染点。**无 `is_error` 位** —— 失败与被杀靠措辞区分 |

**为什么落点每回合刷新**：工具表是进程级的，边界源每回合新建。存启动那刻的 `Arc`
会在回合切换后把结果推进一个无人消费的队列（看起来成功，实则永久沉底）。

### 20.3 锁纪律

`sweep()` **三段式**：
1. 段 1（持锁）：`repair_inflight` + `enforce_output_cap`（标记）+ `reap` + `claim_completions`
   —— **不 fork、不读输出文件**（`try_wait` / `stat` 除外）；
2. 段 2（无锁）：`kill_outside` + `deliver`（渲染要读文件，最多 50 KiB）；
3. 段 3（持锁）：淘汰超额完成项。

`snapshot()` / `mark_and_kill_expired()` 同规：锁内标记、锁外杀。

### 20.4 名额

| 计数 | 上限 | 超限 |
|---|---|---|
| 在跑 | 4（`max_running`） | `bash_bg` 同步报错 |
| 未投出的完成项 | 8（独立计数） | 淘汰最旧的（删输出文件） |

两者**不再共享**名额 —— 否则「启动构建 → 回合结束 → 下回合再启动」这种正常用法
会把滞留的结果挤掉。

### 20.5 `bash` 超时转后台

已挂后台能力时（**仅交互式 TUI**），`bash` 的 deadline 到点不再杀进程组，而是：
`adopt_pid` 登记 → watcher **接管既有的两个排空任务**（管道已被它们 `take()` 走，
不能再 `child.stdout.take()`）→ 等进程退出 → `mark_finished` 推终态 → 下一 tick 投递。

`-p` / `--json` 模式**不挂**后台能力（`main.rs` 的 `Mode::Interactive` 判断）
⇒ 超时走原路径（杀进程组 + `ToolError::Timeout`）。

### 20.6 跨回合补投

回合先结束时完成项留在注册表（`Pending`），下一回合开头由 `app_loop` 补投，
**每回合最多 2 条**（`BG_CATCHUP_PER_TURN`）；超出部分继续留表。

### 20.7 已知缺口

- 「锁内不读输出文件」只有人工审查，缺自动化探针（渲染是外挂 `fn` 指针，黑盒）。
- `BG_CATCHUP_PER_TURN` 的限流逻辑在回合循环体内，未抽成可测函数。
- `kill_group` 仍是同步 fork（已挪到锁外，但未异步化）。
