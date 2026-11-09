# ADR-0075：TUI 右侧栏 —— 固定宽、默认隐藏、几何单源、回合中增量推送

**状态**：proposed
**日期**：2026-11-10
**相关**：ADR-0054（subagent 可观测层，未落地 —— 本 ADR 是其严格子集）、
ADR-0022（窄 trait / 子事件不进父流）、ADR-0014（不建 hook 系统）、
ADR-0021（只留 `ToolPlugin` 装配缝）、ADR-0073（空闲期不引入周期工作）
**设计**：[`docs/arch/tui-right-panel/design.md`](../arch/tui-right-panel/design.md)

---

## 上下文

需求：TUI 右侧展示 subagent 工作状态与 todo 清单（两栏）。已澄清的边界（见 `context.md`）：
固定宽度、**默认隐藏 + 手动打开**、开关**不跨重启**、两栏各占 50% 高、空态框常驻、
本轮不展示 harness 状态、subagent 只要「谁在跑 + 主题」、主题取派发工具入参的 `description`。

设计过程中出现四个需要单独记录的决策点。它们都不是「选个顺眼的」，
其中前两个有**反直觉的实测证据**或**跨 crate 的契约后果**。

---

## 决策

### 决策 1：开关是纯 UI 本地状态，不进视图、不进协议、不落盘

`/panel [on|off]` → `LocalAction::Panel(Option<bool>)` → `App.panel_visible: bool`。
`CodingView` **不给**启动初值字段；`ys-protocol` 零改动；不写 `state.json`。

**与 `/thinking`、`/theme` 的差别是有意的**：那两个是长期显示偏好（真值归 app、要落盘），
故必须有「视图初值 → UI 运行期真值」的通道。侧栏是随时可开的重型面板，每次启动默认安静更符合
「不扰动」；给它视图字段只会制造「谁写谁」的歧义。

**后果**：`CodingView.panel`（数据）与 `App.panel_visible`（开关）名字相近、语义相反。
二者必须在注释里写死，否则后来者会把开关塞进视图。

### 决策 2：侧栏只切 Chat 带，不竖贯到底

右侧栏只占 Chat 那一段的高度；Input 与底部状态行保持整屏宽。

**这不是排版偏好，是正确性。** `input_height`（`draw.rs:202`）与 `input_inner_width`（`:209`）
都以整屏宽为入参（`draw.rs:172`、`:178` 传 `area.width`）。若竖栏通到底：

```text
开关 → 输入框可用宽度变 → 输入框折行行数变（1..=5）→ input_height 变
     → Chat 高度变 → 栏高变 → （回到第一行，栏宽是常量所以这里收敛，但几何已互相依赖）
```

取「只切 Chat 带」后，`app.input.wrap_width`（`draw.rs:178`）、`input_height`、
`status_text`（`draw.rs:967`）的入参**逐字节不变**，既有输入/状态行用例零改动。

**几何上的落实顺序不能反**：必须**先在整屏宽上做纵向切分，再只把 `v[0]`（Chat 带）横向让位给侧栏**。
若反过来（先横切再竖切），`input_height` 拿到的就是变窄后的宽 —— 反馈环以更隐蔽的形式回来。
这是本设计最易写反的一处。

**否决的替代**：整屏竖切（最小复杂度候选 A 的做法）要额外保证「输入框高度不随开关变化」，
而那条保证在几何上本来就不成立。

### 决策 3：几何收敛为单一纯算术函数；栏宽手写钳制，50/50 不写奇偶补偿

今天同一份纵向 `Layout::vertical` 在 `draw.rs:169-174`（画面）、`:246-253`（`chat_area`）、
`:220-245`（`input_geom`）**各写一遍**。ADR-0054 的否决理由正是「二者不同步会让滚动与画面错位」。
本 ADR 引入 `frame_areas(app, total) -> FrameAreas` 作为**一帧全部几何的唯一来源**，
`chat_viewport` / `chat_inner_rect` / `input_geom` 的签名与 8 个消费者**全部不动**。

两条实测结论（ratatui 0.30，60+ 组）**推翻或确认了手算假设**：

1. **`Layout::horizontal([Min(1), Length(30)])` 优先保栏、把 Chat 压到 1 列**
   （W=10 ⇒ chat 1 / 栏 9；W=20 ⇒ chat 1 / 栏 19）。
   ⇒ **必须手写** `SIDE_WIDTH.min(total.saturating_sub(MIN_WIDTH))`。意图是「保 Chat」，
   而 `Constraint` 给的语义恰好相反。
2. **`Percentage(50)/Percentage(50)`、`Fill(1)/Fill(1)`、`Ratio(1,2)/Ratio(1,2)` 三种写法逐格相同**，
   且两栏和恒等于 H：H=25 ⇒ 13/12（上栏多一行），H=7 ⇒ 4/3，H=5 ⇒ 3/2。
   ⇒ **不写奇偶补偿、不写 `H/2` 各算一次**（后者才漏行）。

**否决的替代**：`H/2` 各算一次、或按 `H - H/2` 手工配平 —— 都是重复实现 ratatui 已经做对的事。

### 决策 4：回合中按面板变更增量推送整视图（出站节奏的唯一新例外）

**已核实的事实**：`run_turn_chain`（`app_loop.rs:1841-2800`）区间内**零 `Outbound::View`**，
回合中只发 `Outbound::Event`（`:2203`、`:2288`）。而 todo 变更与 subagent 起落**都发生在回合中**。

⇒ 只靠现有节奏，回合进行中打开侧栏会看到**回合开始前**的内容，与场景 S2（回合内派发后要看得见）
和 S3（回合已结束、子 agent 仍在跑）直接冲突。而「看不到正在跑的运行状态栏」不成立。

**决策**：在回合中的事件转发处，用 `panel_view()` 重新投影，**`PartialEq` 变化检测 + 300ms 节流**，
变化时**复用上次已发的 `CodingView`、只替换 `panel` 字段**再发 `Outbound::View`。

**「上次已发的那份」需要新增载体（这是本决策的实际开量）。** 核实结果：
`run_turn_chain`（`app_loop.rs:1841`）函数体内**无任何 `CodingView` 局部量**，`TurnCtx`（`:1656`）
里也**无 `state_store` / `todo_store`**，而 `todo_store`（`:473`）活在 `run()` 里没传下去。
⇒ 必须给 `TurnCtx` + `run_turn_chain` 加四项：`todo` / `last_view` / `last_panel` / `last_push`，
并在**每次**发出 `Outbound::View` 时同步写 `last_view`（否则回合中复用的是陈旧基线）。

三条必须钉死的性质：

| 性质 | 要求 | 理由 |
|---|---|---|
| 只推「变了」的 | `next != last_panel` 才发 | 无变化零流量，**包括侧栏关着时**（app 侧不知道开关） |
| 不重建整视图 | 只改 `panel` 字段 | 其余字段与 UI 手上那份逐字相同 ⇒ 状态行不跳变 |
| **不得**改调 `build_view_with_loop` | 用复用值 | 它形参含 `&Agent`，而回合执行期 `ctx.agent` 已被 `&mut` 借走；且读 `Instant` / `state_store` |

**已知语义噪声**：`apply_outbound` 收到任何出站消息都清 `command_in_flight`（`app.rs:270-271`）。
回合中多推 `View` 会让它在回合期间反复被清。已核实**无实际影响**（该标志的读者只有 Esc 的取消分支
`events.rs:659`，回合中走 `app.is_turning` 那条），但记在这里 —— 将来若有人往这个标志上加语义，
会撞到一个幽灵。

**否决的替代**：
- **接受冻住**（最小复杂度候选 A）：S2/S3 是用户点名的场景，功能上等于坏。
- **新增 `Wake::PanelTick` 唤醒源**：等于给 app 循环加一个只为 UI 的 300ms 心跳，
  违反 ADR-0073 同一条纪律（空闲期不引入周期工作）。
- **让 UI 持 `Arc<TodoStore>` 直接读**：破「UI 不读后端真值」的快照范式，且 `ys-tui-coding`
  会依赖 `adapters/tools-todo` —— 破依赖方向。
- **新增 `Outbound` 变体**：ADR-0054 决策 4 已否决。

### 决策 5：`SubagentRunner::spawn` 增加 `label: &str` 形参

主题 `description` 在 `Tool::call` 处现成（`subagent.rs:2036`），但 trait 四个形参里没有位置。

```rust
fn spawn(
    &self,
    _prompt: &str,
    _label: &str,        // 新增：模型给的人类可读主题（schema 里 required）
    _model: Option<&str>,
    _mode: SubagentMode,
    _boundary: Option<&dyn BoundarySource>,
) -> Result<String, String>
```

用 `&str` 而非 `Option<&str>`：schema 里 `required`，不必四个实现点各写一次 `unwrap_or`；
缺失即 `ToolError::InvalidInput`，**不设默认值**（否则 UI 会出现「无标题项」这种静默）。

**波及面**：生产实现 1 处（`subagent.rs:1171`/`:1193`）、调用点 1 处（`:2042`）、用例 1 处（`:4370`）。
`FakeRunner`（`crates/ys-tool/src/context.rs:99`）与 `ProbeRunner` **零改动** —— 靠默认实现。

**键存在既有的 `SubagentHandles` 表里，不新开并行表**：`HashMap<String, (Weak<QueueBoundarySource>, Arc<str>)>`。
理由：**「谁在跑」必须只有一个真值**。两张表会有一张先过期（`retain` 只在读时清理），
产生「名单空了、主题还挂着」的幽灵行。

---

## 方案对比与取舍

完整矩阵见 `design.md` §1.4。核心取舍：

| 未采纳的做法 | 出自 | 否决理由 |
|---|---|---|
| `Arc<PanelModel>` + `PanelCache` | 性能优先候选 | 优化的是 v1 不存在的开销（每回合十几次推送）；换来跨线程共享模型 + 失效协议 + 「UI 何时重读」新问题。**但采纳其「条目数封顶」思想**（§4） |
| 生产侧 `trait SidebarSectionSource` + 装配表 | 可扩展优先候选 | 需求把 section 数钉死为 2；trait 的价值是运行期增删，本需求没有。**但采纳其几何**（按 `sections.len()` 均分 ⇒ 加第三栏只改数据不改几何） |
| 整屏竖切（Input 也变窄） | 最小复杂度候选 | 见决策 2，几何反馈环 |
| 接受回合中冻住 | 最小复杂度候选 | 见决策 4，场景 S2/S3 直接失效 |

**加第三个 section（harness / cost / MCP）的成本**：`panel_view()` 加一段投影 + `draw_sidebar`
加一次均分 + 测试 = **3 处**，不需重做几何。

---

## 后果

**正面**：

- 启动画面与今天**逐字节相同**（默认关 ⇒ 侧栏宽 0 ⇒ 整屏归主列）。既有视觉用例零改动。
- 几何从三份复算收敛为一份，`chat_viewport` / `chat_inner_rect` 的 8 个消费者零改动。
- 空闲态**不新增任何定时器**：帧节奏仍由 `FrameSchedule`（`run.rs:75-110`）决定（≈1 Hz）。
- `-p` / `--json` **零成本**：它们消费 `Envelope`（`main.rs:2213`），不读 `Outbound::View`。

**负面 / 接受的代价**：

- `SIDE_WIDTH = 30` 是魔术数：80 列终端下 Chat 从 80 → 50，折行行数 +74%。
  需求已定「静态大小 / 不做自适应 / 不做窄屏自动隐藏」⇒ 接受，改一个常量即可调整。
- 切换那一瞬有一次**全量重折行**（`layout.sync` 的宽度判据全清，`layout.rs:193-196`）：
  500 KB~1 MB 会话估 5~30 ms。不可消除（宽度变了必须重排），只在手敲命令那一下。
- 出站节奏多一个例外（决策 4），必须靠用例钉住。
- `TodoStatus` 是 `#[non_exhaustive]` ⇒ 生产侧 `match` 必须带 `_ => Plain` 兜底：
  将来新增状态会**静默**显示为 Plain（不编译报错）。接受（`is_terminal()` 仍决定清空语义）。

**放宽的既有结论**：`long-horizon-todo/design.md:217` 的「空列表 ⇒ 0 宽 ⇒ 逐字节不变」
在**启动场景**下仍成立（默认关）；只在「已打开且两栏都空」时被放宽 —— 那时框必须在（Q6b 用户答定）。

---

## 关联

- [`design.md`](../arch/tui-right-panel/design.md) —— 完整设计（含候选内嵌与测试矩阵）
- [`context.md`](../arch/tui-right-panel/context.md) —— 需求上下文与场景表
- `docs/adr/0054` —— subagent 可观测层（本 ADR 是其严格下游子集）
- `docs/arch/long-horizon-todo/design.md` §7.1 —— `TodoStore` 真值层
