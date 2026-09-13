# Coding Agent TUI — 设计

> **ratatui 定死**（不再讨论）；本文描述它**长什么样**与**作为独立 crate 的形态**。
> 前置：`../tui-repl/context.md`（**已反转**：那份文档描述的是行式 REPL。其中与 UI 库无关的架构结论 —— 协议、信道拓扑、命令分类、快照推送、`CancelToken` 移除 —— **已全部并入本文**，该文件其余部分作废待归档）
> `../gap-closure/context.md`（产品定位）、`../tui-pi-codewhale-borrow/design.md`（借鉴对照）

## 0. 定位

```
ys-protocol          能力平面（协议）        ← 所有产品共用
ys-tui-coding        coding agent 的 TUI     ← 本设计，独立 crate
ys-tui-invest        投资 agent 的 TUI       ← 将来的另一个独立 crate
```

**共享的**：协议（能力面 / 事件 / 通用消息）。
**各写各的**：`AppView` 字段（provider/tokens vs 净值/持仓）、命令表、**工具调用的摘要规则**。

---

## 1. 形态

```
┌ Chat ──────────────────────────┐
│ > 重构这个模块                  │
│                                 │
│ 我看看代码。它把读写混在一起，  │   ← assistant 文本（wrap）
│ 我拆成两个函数。                │
│                                 │
│   ⏺ read   src/x.rs      ✓     │   ← 工具调用压一行
│   ⏺ edit   src/x.rs      ✓     │
│                                 │
│ 拆好了。                       │
│ ✓ 3 rounds · 2.1s               │   ← turn 摘要
├────────────────────────────────┤
│ > /mo▏                           │   ← 输入（多行，高自适应）
│ ┌───────────────────────────┐  │
│ │ /model  [model_name] 切换  │  │   ← 补全浮层（盖在对话区上）
│ │ /compact              压缩 │  │
│ └───────────────────────────┘  │
├────────────────────────────────┤
│ deepseek-chat · ↑1.2k ↓345 · 2轮│   ← 状态行（最底，常驻）
└────────────────────────────────┘
```

**三个 pane（自上而下）**：

| pane | 约束 | 说明 |
|---|---|---|
| **Chat** | `Constraint::Min(3)` | 唯一可滚动的区域 |
| **Input** | `Constraint::Length(1..=N)` | 高度随内容自适应 |
| **Status** | `Constraint::Length(1)` | **最底**，常驻一行 |

> 补全浮层**独立于这三个 pane** —— 它 `Clear` 后盖在 Chat 区域上（或统一用 `Overlay`），不参与布局高度计算。

**「常驻在最底」是 ratatui 的默认行为**：三个 pane 都是**固定分区**，每次重绘整屏；会"滚走"的只有 **Chat 里的内容**。Status 与 Input 结构上不可能滚出视野。

---

## 2. 各 pane 规格

### 2.1 对话区（Chat）

| 元素 | 渲染 |
|---|---|
| 用户输入 | `> ` 前缀（绿） |
| assistant 文本 | **正常 wrap**；多行 |
| **工具调用** | **压一行**：`  ⏺ {name} {args 摘要}   {✓/✗}`，结果**默认不展开** |
| 工具结果 | 默认不显示；展开时挂在该行下方（`└ …`） |
| thinking | 默认不显示；`/thinking on` 后以**暗色/斜体**渲染（前缀 `⌁`） |
| 错误 | `Error: …`（红） |
| turn 摘要 | `✓ 2 rounds · 1.3s`（符号按 `StopReason`） |

**关键要求：transcript 必须是结构化的** —— 不能像现在这样只存预格式化的 `String`（`TranscriptLine::Tool { args: String, result: String }` 是"已经是字符串"的形态）。压成一行需要**从结构化参数里抽取摘要**，且**摘要规则由产品决定**（coding：`read src/x.rs`；投资：`market 510300`）。

### 2.2 状态行（Status）

```
宽屏：  deepseek-chat · ↑1.2k ↓345 · 2 轮 · 1m23s
中屏：  deepseek-chat · ↑1.2k ↓345 · 2 轮
窄屏：  deepseek-chat · ↑1.2k ↓345
极窄：  deepseek-chat
turn 中：⏳ Working· · ↑1.2k ↓345 · 2 轮 · 1m23s
```

- **窄屏从尾部剥**：时长 → 轮数 → tokens → model（model 最不可剥）
- **只显示有数据的字段**（`ctx%` / `$cost` / `ttft` / `tok/s` **已被否决**，见 `tui-status-display/context.md`）

### 2.3 输入区（Input）

- **多行**：`Shift+Enter` / `Alt+Enter` 换行，`Enter` 提交（`Alt+Enter` 是 macOS Terminal.app 的兜底）
- **高度自适应**：1 → N 行（上限另定）
- **bracketed paste**：多行粘贴**不被当多次提交**（现状 `Event::Paste` 被丢弃）
- 前置：`PushKeyboardEnhancementFlags`（crossterm 0.28 已确认支持）

### 2.4 补全浮层

- 盖在**对话区上方**（不改变布局高度）
- 条目：`/model  [model_name]  切换模型` —— 数据早就齐（`builtin_help_entries()` 的 `arg_hint`）
- **现状是 bug**：`CompletionState` 从未在 `draw.rs` 渲染 —— 按一次 Tab 屏幕毫无反应

---

## 3. slash 命令：**全在 TUI 内**

**不再用 `inquire`；不再 suspend/resume。**

| 命令 | 处理 |
|---|---|
| 纯文本类（`/help` `/status` `/new` `/compact` `/copy` `/export`） | 在 TUI 内执行，输出**进 transcript** |
| 需选择类（`/login` `/model` 无参） | **TUI 自己的浮层选择器**（不再切屏） |

**作废清单**（因这条决定）：

| 作废 | 原因 |
|---|---|
| `inquire` 依赖 | 由 TUI 浮层选择器取代 |
| **`InquirePrompter`**（`builtin.rs:6`） | 包装 inquire → 由 TUI 浮层选择器取代。**但 `Prompter` trait 与 `FakePrompter` 保留**（见下注） |
| `suspend_terminal` / `resume_terminal`（`ui/mod.rs:109-141`） | 无外部交互程序要与 alt-screen 让位 |
| `ec5e57c` 那次修 bug 的整个机制 | 同上 |

> **注（修正一处早先说错的）**：`Prompter` trait **不是**随 inquire 一起作废 —— 它的 `select`/`text` 正是「选择器可测」的答案：
> **TUI 浮层实现它**（新）、**`FakePrompter` 实现它**（保留，测试注入）。命令决策逻辑照样单测。
> **死的只是 `InquirePrompter`。**

---

## 4. 与现状的差异

### 保留

| | |
|---|---|
| `AppView` 快照（数据/显示分离） | **核心抽象，两套产品共用** |
| `ChannelSink` 事件消费 | |
| 四路 `select!`（turn / 键鼠 / 事件 / 节拍） | |
| `StopReason` 的符号映射 | |

### 新增 / 修改

| 项 | 说明 |
|---|---|
| 状态行常驻（`Length(1)`） | 现状 `show_status` 默认 `false`，且是**右侧栏**，非底部行 |
| 对话区 wrap 修正 | 现状 `line_to_text` 对 assistant **不做 wrap**（`draw.rs:176-179` 注释"简化"），外层 `Paragraph` 再 wrap → **滚动计算按 1 行/条算，与实际不符** |
| 工具调用压一行 | 现状是两行 + 结果截断 |
| 补全浮层 | 现状**从未渲染** |
| 多行输入 + 粘贴 | 现状单行、`Event::Paste` 丢弃 |
| 键盘增强协议 | 现状未启用 |
| TUI 内浮层选择器 | 替代 inquire |
| **中文退格 panic 修复** | `ui/events.rs:131` 的 `cursor - 1` 落在多字节字符中间 |

### 作废

- `inquire` / `InquirePrompter` / `suspend` / `resume`（见 §3）
- **`ansi.rs`（105 行，死代码）**：生产零消费者
- **`format.rs` 的现状**：只剩 `format_tokens`，且唯一消费者是 `ui/draw.rs`

---

## 5. 输出与诊断的去处（解 review R2）

**14 处生产 `eprintln!`** 在新拓扑下**都会冲掉 TUI 屏幕**。按**时机**三分：

| 时机 | 内容 | 去处 |
|---|---|---|
| **启动期**（TUI 未起） | 容量 clamp 警告、`No model configured` | **stderr**（照旧） |
| **命令期间**（有 TUI） | `/login` 的 "✓ Logged in…"、"Warning: Could not persist credentials" | **`Outbound::Output`** → transcript |
| **库内部 / 随时** | `auth.json` / `state.json` 解析失败 | **日志文件** |

### 14 处逐条归属

| 位置 | 内容 | 去处 |
|---|---|---|
| `main.rs:146` | 容量 clamp 警告 | 启动期 → **stderr** |
| `main.rs:239-244` | `No model configured` + 提示 | 启动期（`-p`/`--json` 路径）→ **stderr** |
| `main.rs:279` | `--stats` 背压读数 | 非 TUI 模式的设计行为 → **stderr** |
| `provider.rs:132` | `failed to parse auth.json` | 库内部（`load_auth`，**启动期**）→ **日志** |
| `state.rs:36` | `failed to parse state.json` | 库内部（`load`，**启动期**）→ **日志** |
| `builtin.rs:306` | `Could not persist credentials` | 命令期间 → **`Output`** |
| `builtin.rs:321` | `Could not build model` | 命令期间 → **`Output`** |
| `builtin.rs:342` | `Could not persist state` | 命令期间 → **`Output`** |
| `builtin.rs:376` | `Could not remove persisted credentials` | 命令期间 → **`Output`** |

> **只有 2 处（`provider.rs` / `state.rs`）严格需要日志文件** —— 其余靠「启动期 stderr / 命令期 `Output`」就够。
> 日志文件的价值是**通用兜底**：TUI 应用没有"历史输出"可看，诊断得有个落处。

### 日志文件

```
~/.yushan/logs/yushan.log     追加写，best-effort（写失败不影响运行）
```

**机制**：v1 手写一个极小 logger（打开追加、带时间戳、忽略错误），**不引新依赖** —— 只有 2 个调用点，用 `tracing` 是杀鸡用牛刀。将来若需要分级/结构化，再换。

> ⚠ **`/compact` 的 `generate_summary` 失败**也会产生一条诊断（`basic.rs`），它属"命令期间"还是"自动压缩期间"取决于触发者 —— 归入 `Output`（对显式 `/compact`）/ 日志（对自动压缩）。

---

## 6. 协议形状（解 review R3）

```rust
// ys-protocol
pub enum Request {
    Prompt(Message),                                 // → agent 的输入
    SetModel { model: String },
    Login { provider: String, api_key: String },
    Logout,
    NewSession,
    Compact,
    Export { path: PathBuf },
}

pub enum Boundary {
    Steer(Message),                                  // 轮边界注入
    Abort,                                           // 取代 CancelToken
}

pub enum Outbound<V> {
    Event(Envelope),                                 // 事件（已封壳）
    View(V),                                         // 快照（产品视图 V）
    Output(String),                                  // 命令输出 → transcript
    Quit,
}
```

**没有 `Diagnostic` 变体** —— 命令期间的用户可见诊断走 `Output`；库内部诊断走日志文件（见 §5）。

### 命令 → 能力的映射（**表在 TUI 侧**）

| 命令 | TUI 做什么 | 发给 app |
|---|---|---|
| `/help` | 本地打印命令表 | — |
| `/status` | 本地（`CodingView` 在 TUI 手里） | — |
| `/copy` | 本地（最后一条回复在 transcript） | — |
| `/quit` | 本地 | — |
| `/model [name]` | 无参时**浮层选择** | `SetModel` |
| `/login` | **问 provider / api_key**（`Prompter`） | `Login` |
| `/logout` | — | `Logout` |
| `/new` | — | `NewSession` |
| `/compact` | — | `Compact` |
| `/export [path]` | — | `Export`（会话文件在 app 侧） |

**为何表在 TUI 侧**：命令的**用户可见行为**（提示什么、问什么）归 UI；**能力实现**归 app。漏实现是**编译错误**（`Request` 变体必须 match），不会静默漂移。

---

## 7. 作为独立 crate 的形态（**定稿：路线 B**）

**路线 B** = 拆 crate + 分线程 + 信道 + `ys-protocol`。

```
ys-protocol          通用能力面：Request / Boundary / Outbound<V>    ← 所有产品共用
ys-tui-coding        CodingView（自己定义）+ ratatui 渲染/输入/补全    ← 本文描述的 crate
ys-tui-invest        InvestView + 自己的渲染                         ← 将来的第二个
apps/coding-agent    接线器：构造 CodingView、实现 app 侧循环、驱动 agent
```

依赖方向（单向、无环）：

```
ys-protocol ──► ys-tui-coding ──► apps/coding-agent
     ▲                                    │
     └────────────────────────────────────┘
        （app 也依赖 ys-protocol，用同一套类型）
```

### 为什么「拆 crate ⇒ 必须分线程」

`ys-tui-coding::run()` **阻塞在它自己的事件循环里**，且它**不认识 `Agent`**（否则拆 crate 没意义 —— 编译器边界形同虚设）。

→ **agent 不能在 `run()` 里跑，必须在另一个线程。**
（反向也成立：保持单线程 `select!` ⇔ 保持 `ui/` 模块。）

### 视图类型归**产品 TUI crate**

`CodingView` 定义在 `ys-tui-coding`，**不放进 `ys-protocol`**。理由：

- 它是「**UI 想显示什么**」的定义 —— UI 自己的事
- 各产品不同（coding：provider/tokens/tools/session；投资：净值/持仓/标的）
- `apps/coding-agent` 依赖 `ys-tui-coding` 来**构造**它（app 本就依赖该 crate 以调 `run()`）

所以 `ys-protocol` 里的 `Outbound<V>` 是**泛型**的 —— 通用壳 + 产品视图。

### 三条信道（`CancelToken` 已废，`Abort` 并入 ②）

| 信道 | 类型 | 方向 | 谁在什么时机消费 |
|---|---|---|---|
| **① Request** | `ys_protocol::Request` | UI → app | app 线程 `recv().await`（回合边界） |
| **② Boundary** | `ys_protocol::Boundary` | UI → `BasicLoop` | **轮边界** `try_recv`（`Steer` + `Abort`） |
| **③ Outbound** | `ys_protocol::Outbound<V>` | app → UI | UI 事件循环 `recv().await` |

**② 必须独立于 ①**：回合跑动时 app 线程阻塞在 `agent.run()` 里，不可能 `recv` ①；而 `Steer`/`Abort` 要**中途**被看到 → 只能由 `BasicLoop` 在轮边界拉。

### 边界纪律

- `ys-tui-coding` **不依赖 `ys-runtime`**（不认识 `Agent`）
- 要说话时用 `ys-protocol` 的类型 + 收发句柄
- **这是编译器强制的**（crate 边界），不靠自觉

### 待定

- **crate 放哪**：`crates/` 还是 `apps/coding-agent-tui/`（它是产品专属的，不是通用组件 —— 倾向前者放 `crates/` 以便与 `ys-protocol` 平级）


---

## 8. 决策汇总与未决

### 结构（路线 B）

| 项 | 结论 |
|---|---|
| UI 形态 | **拆 crate**：`ys-tui-coding`，**不依赖 `ys-runtime`** |
| 线程 | **2 个**：UI 线程（`run()` 阻塞在自己的循环里）+ app/agent 线程 |
| 信道 | **三条**：① Request（回合边界）/ ② Boundary（轮边界）/ ③ Outbound |
| `ys-protocol` | **必需** —— 通用能力面：`Request` / `Boundary` / `Outbound<V>` |
| 视图类型 | **`CodingView` 归 `ys-tui-coding`**（不进协议；各产品不同） |
| `CancelToken` | **移除**；`Abort` 并入 ② |
| `Envelope.turn` | 保留 |

> **「拆 crate ⇒ 必须分线程」的原因**：`run()` 阻塞在自己的事件循环里且不认识 `Agent`
> → agent 不能在 `run()` 里跑，只能在另一个线程。反向亦然（单线程 `select!` ⇔ `ui/` 模块）。

### UI 细节

| 项 | 结论 |
|---|---|
| 布局 | 三 pane 自上而下：Chat(`Min`) / Input(`1..N`) / **Status(`1`，最底)** |
| 对话区 | assistant 正常 wrap；**工具调用压一行**，结果默认不展开 |
| **工具结果** | v1 **不做交互式展开**；但**失败的工具显示结果首行**（成功只给 ✓）——「看不到任何结果」是真损失，而失败时最需要看 |
| 补全 | **浮层**盖在 Chat 上；数据早已齐全（`arg_hint`） |
| slash 命令 | **全在 TUI 内**；废弃 `inquire` 与 `InquirePrompter`，**保留 `Prompter` trait**（换 TUI 实现 + `FakePrompter`） |
| 输入 | 多行（Shift/Alt+Enter）+ bracketed paste + 高度自适应 |
| thinking | 默认隐藏 + `/thinking on` 开关（暗色渲染） |
| 状态行 | `model · ↑↓tokens · 轮数 · 时长`，窄屏尾部剥；**turn 中把 model 段换成 `⏳ Working·`** |
| 输出与诊断 | 启动期 → stderr；命令期 → `Outbound::Output`；库内部 → `~/.yushan/logs/yushan.log`（见 §5） |
| 退出 | **保留** `restore_terminal` 那套（alt-screen 仍在） |
| `Envelope.source` | **保留**（多 agent 占位，不占成本） |
| crate 位置 | **`crates/ys-tui-coding/`**（与 `ys-protocol` 平级；`apps/` 留给接线器） |
| 文档收尾 | `tui-repl/context.md` 的架构结论**已并入本文**，该文件作废待归档 |

### 未决（只剩三条）

- [ ] **`CodingView` 的字段清单**（含 `ToolCallId → ToolCall` 映射，供工具结果回填）—— 可在写 exec-plan 时一次给出
- [ ] **接线器 app 侧循环的形状**（review R4）—— 它是现有 `select!` 的**重写**：何时 `recv` ①（空闲才收）、如何驱动 `turn_fut`、如何把事件转成 `Outbound`、`CommandError` 怎么回给 UI。**这是设计还差的一块**
- [ ] **两个真 bug 与重构的先后** —— 可独立先修（半小时量级），但会落在**将被重写**的 `ui/` 上；倾向「随手在重构里修掉」

