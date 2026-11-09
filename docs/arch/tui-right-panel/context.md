# TUI 右侧双栏（subagent 状态 + todo 清单）— 架构上下文

> 产出：`/arch-explore`（2026-11-10）。**需求原话**：
> 「我希望在 tui 的右侧展示 subagent 的工作状态和 todo 清单的情况（分成两栏）」
>
> **Q1 / Q2 / Q4 / Q6 / Q8 已澄清（同日），Q6b 答定空态，Q3 定为可开关且默认关闭**：
> - 形态：「右侧有一个比较窄的**靠边栏**，然后它再分成上下几个**横栏**」⇒ 两列 + 右列内 section 栈。
> - 尺寸：「可以先保持右侧边栏**静态大小**」⇒ 固定宽度，不自适应。
> - 空态：「**固定大小就行，没有的话就不展示内容即可，但是框框还是需要保留**，只是内容可以空着」
>   ⇒ 栏**打开时**框常驻（固定宽 + 固定 50/50 高），**不随内容有无而变化**；空栏只空内容、不丢外框。
>   因为**默认是关的**，S1「空闲时画面逐字节不变」仍然成立 —— 打开后才变窄。
>   `long-horizon-todo/design.md:217` 的「空列表 ⇒ 0 宽 ⇒ 逐字节不变」在**启动场景**下依然一致；
>   它只在「已打开且两栏都空」时被放宽（那时框要在，见 §约束·空态不塌陷）。
> - 内容：「**harness 状态可以先不展示**」⇒ v1 栈里不出现 harness 栏，也就与底部 `status_text` 零重叠。
> - 开关：「**可以默认隐藏，然后手动打开。开关不用跨状态保持**」
>   ⇒ 侧栏**默认不显示**，用户敲命令才出现；开关状态**只活在当前进程里**（`App` 上的 `bool`），
>   不落盘、不进 `state.json` ⇒ **不动 `ys-protocol`**。
> - subagent：「只展示谁在跑和它的**工作主题**」，且主题 = **生成提示词时一并产出的 `description`**，
>   展示时只取其**开头一段**（栏宽不够就截断）。⇒ **不需要 ADR-0054 的整套可观测层**。
>
> **本轮的关键实现事实**：主题选用的 `description` 现成但**断在两层**：
> ① 它只被父侧工具行摘要用掉（`transcript.rs` 的 `SUMMARY_KEYS` 末位）；
> ② `Tool::call` 没把它交给 runner —— `spawn(prompt, model, mode, boundary)` 四个形参里没有位置（`subagent.rs:2042`）。
> ⇒ 必须动一次 `ys-tool::SubagentRunner` 的签名。波及面已核实：**生产实现方只有一处**
> （`apps/coding-agent/src/subagent.rs:1151`），另有 `debug_log.rs:374` 的转发层、
> `crates/ys-tool/src/context.rs:99` 的测试 FakeRunner、`subagent.rs:4370` 的用例调用。
>
> 本文只记录**现状基线**与**约束**，不含方案。证据均带 `文件:行号`，基线为本工作区当前 HEAD。

---

## 概述

一句话：**TUI 右侧要加一条窄边栏，栏内再纵向切成多个可扩展的横栏**。v1 的两栏是
**todo 清单 + subagent 运行情况**（harness 状态已明确本轮不做）；而这两份数据今天
**都没有**以 UI 可消费的形状存在于 `CodingView` 里。

现状拆成两个不对称的缺口：

| 面 | 后端真值 | TUI 可见性 |
|---|---|---|
| todo 清单 | ✅ 完整（`TodoStore`，`Arc` 共享，有 `snapshot` / `revision` / `has_open_items`） | ❌ 零：`CodingView` 无 `todos` 字段 |
| subagent 状态 | 🟡 部分（有在跑**计数**与**句柄列表**，没有逐条明细） | ❌ 零：`CodingView` 无 `subagents` 字段 |

`rg -n "subagent|todo" crates/ys-tui-coding/src/*.rs` **零命中** —— UI 层至今不认识这两个概念中的任何一个。

### 为什么现在提：两份既有文档都在等这一步

- `docs/arch/subagent-progress/context.md:223`（Q2 展示形态）与 `design.md:534` 明确写了：
  「**TUI 右侧栏（roster）** 要改**两处**纵向切分……收益本轮不需要；**且 TODO 的 todo 侧栏也在等同一处改动，宜合并做**」。
- `docs/arch/long-horizon-todo/context.md:247`（Q7）已定：「**TUI 侧栏简单展示**」（v1 原计划底部面板，改为侧栏），
  `design.md:217` 给了形状：`CodingView.todos` + 侧栏，**空列表 ⇒ 0 宽 ⇒ 逐字节不变**。
  ⇒ **Q3 定为「默认隐藏」后，这条既有纪律与本次需求重新一致**：启动时栏不画，
    所以「空闲画面与今天逐字节相同」依然成立（见 §约束·默认隐藏的收益）。

本需求正是那个被两次推迟、要求「合并做」的合并项。

---

## 现有架构

### 模块边界

```text
ys-tui-coding                  App（transcript + is_turning + current_round）
  ├── draw.rs      ui_with / chat_area / chat_inner / status_text  —— 布局与画面
  ├── layout.rs    TranscriptLayout —— 逐条目折行记忆、滚动数学、视窗切片
  ├── events.rs    鼠标/键盘命中（消费 chat_inner_rect）
  └── view.rs      CodingView / LoopView / GoalView —— **纯显示快照**
     ↑ 只认 Outbound<CodingView>（事件 + 视图），**不依赖 ys-runtime，也不依赖任何 adapter**
apps/coding-agent
  ├── view.rs            build_view / build_view_with_loop —— 快照的唯一生产者
  ├── app_loop.rs        run 主循环；18 处 `Outbound::View(...)` 推送点
  ├── todo_inject.rs     回合边界注入（**不是** UI 通路）
  └── subagent.rs        SubagentSetup { results, live, turn, handles } —— 后端真值
adapters/tools-todo      TodoStore（状态唯一真值 + 落盘 + `todo` 工具）
```

**两条硬边界**（决定方案形状）：

- `crates/ys-tui-coding/Cargo.toml` 的依赖只有 `ys-protocol` / `ys-core` / `ys-event` / `serde_json` /
  `ratatui` / `crossterm` / `tui-markdown` / `tokio(sync)` / `libc` —— **没有 `ys-runtime`，没有 `ys-tools-todo`**。
  ⇒ TUI 拿不到 `TodoStore`，**只能**经 `CodingView` 由 app 侧投影过来（与 `loop_state` / `goal` 同一条路）。
- `view.rs:1-7` 的纪律：**视图类型归产品 TUI crate，不进 `ys-protocol`**。

### 核心抽象

| 类型 | 位置 | 与需求的关系 |
|---|---|---|
| `CodingView` | `crates/ys-tui-coding/src/view.rs:83` | 唯一入 UI 的数据载体。**今天是 subagent / todo 的零字段** |
| `LoopView` / `GoalView` | `view.rs:164` / `:179` | **「已投影的显示值」范式**：UI 不算时间、不做判断，app 算好字符串（见 `LoopView.next_label` 的注释） |
| `TodoItem` / `TodoStatus` | `adapters/tools-todo/src/item.rs:50` / `:15` | `Pending` / `InProgress` / `Completed` / `Cancelled`；`is_terminal()` 是唯一终态判据（`item.rs:27`） |
| `TodoStore` | `adapters/tools-todo/src/store.rs` | `snapshot()`(`:160`) / `revision()`(`:165`) / `has_open_items()`(`:170`)；`Arc` 共享，`/new` 时 `retarget` 而非重建 |
| `LiveSubagents` | `apps/coding-agent/src/subagent.rs:508` | **只有计数**（RAII `LiveGuard`）；`count()` 是 `#[cfg(test)]`(`:524`) ⇒ **生产侧今天连读数接口都没有** |
| `SubagentHandles` | `subagent.rs:420` | `live_handles() -> Vec<String>`(`:468`) —— **唯一现成的「谁在跑」名单**，`Weak` 失效即视为收尾 |
| `PendingResults` | `subagent.rs:268` | 已完成但未投递的结果（`has_pending`(`:357`)）；「已回来但主 agent 还没看到」的唯一真值 |
| `SubagentSetup` | `subagent.rs:986` | `{ results, live, turn, handles }` 四元，`Arc` 跨 `/resume` 重建共享 |
| `JournalingSink` | `subagent.rs:721` | 旁路日志 `<pid>_<seq>.live.jsonl`(`:699-704`) —— **逐条明细唯一的落盘处**，但是文件不是内存 |
| `SubagentProgress`（未落地） | `docs/adr/0054`（proposed） | 计划中的逐条进度事件；**全仓源码 grep 零命中** |

### 关键数据流

**今天的通路（侧栏要复用它）：**

```text
回合/请求/唤醒点
  └─ app_loop::run 的 18 处 `Outbound::View(build_view_with_loop(...))`
       └─ 信道（有界，容量可配）
            └─ run.rs:423 `app.apply_outbound(View)`   ← 整表替换
                 └─ draw::ui(frame, app)              ← 每帧重画
```

`build_view_with_loop`（`apps/coding-agent/src/view.rs:194`）已有 9 个形参，并在注释（`:35-40`）里立了规矩：
**「不再加参数」—— 再加就收进 `ViewInputs`**。侧栏要的新数据（todos + subagents）正是第 10、11 个。

**subagent 结果回到前台的现成通路（与侧栏无直接关系，但决定「已送达」语义）：**

```text
子 agent 收尾 → PendingResults::deliver（未绑定时静默入表 + notify_one）
  └─ app_loop 空闲分支 Wake::SubagentReturned（:499）
       └─ has_pending 复核（:515-518）→ 下一回合开头 take_pending_results（:2151）
```

**todo 的现有可见性通路（**不是** UI 通路）：** `todo_inject.rs` 在回合边界把 `<todo_list>` 块作为
`Role::Harness` 消息追加进会话（给**模型**看），TUI 侧只会把它当一条 System/用户行渲染
—— 侧栏要做的是**另一条**、给**人**看的静态视图。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| `ratatui` | 布局与绘制 | 0.30（`default-features = false`，`crossterm` + `unstable-rendered-line-info`） | `Layout::vertical` / `Constraint` |
| `crossterm` | 事件与终端尺寸 | 0.29 | 鼠标命中测试依赖 `chat_inner_rect` |
| `ys-tools-todo` | todo 真值 | 本地 path | **只能经 app 侧投影进 `CodingView`** |
| `tokio(sync)` | 信道 | 1 | UI 线程**无 runtime 上下文**（`blocking_send` 会 panic） |

---

## 约束

### 技术约束

- **依赖方向**：`ys-tui-coding` 不得新增 `ys-runtime` / `ys-tools-todo` / `ys-model` 依赖。
  侧栏数据一律以 **`view.rs` 里的自建投影类型**（owned `String`）传入 —— 名字、状态、耗时格式化都在 app 侧完成
  （`LoopView` 就是范例：`next_label` 已是 `"14 分后"` 这种成品串）。
- **`CodingView` 加字段是加法，但有四处同步成本**：`Default` impl（`view.rs:198`）、
  `for_test()`（`view.rs:250` 起）、`build_view` 的两条路径、以及 `build_view_with_loop` 的**12 处调用点**
  （`app_loop.rs` 的 18 处 `Outbound::View` 中绝大多数经它）。
- **按字符切、不按字节切**：侧栏必然要截断长文案（todo 内容、提示词首行）。既有前科写在
  `crates/ys-tui-coding/src/commands.rs:482-487`：`split_at(len - 1)` 落在多字节字符内部会 **panic**
  （TUI 的 `install_panic_hook` 有意不 `catch_unwind` ⇒ 丢全部会话现场）。统一走 `truncate_to_width`
  （CJK 算 2 列，`draw.rs:113` 附近）。

### 性能约束

- **`layout.rs` 的承重目标是与会话长度解耦**：滚动数学与渲染都是 O(视窗高度)。侧栏**不得**引入任何
  全量折行或全表扫描；todo 项与 subagent 条数都必须**有上界**（既有先例：`MAX_BOARD_ENTRIES = 32`、
  `MAX_PENDING_RESULTS`、`DEFAULT_MAX_DISPATCHES_PER_TURN = 16`）。
- **重绘节奏**：`run.rs:463` 的 tick（`TICK_INTERVAL = 300ms`）**只在 `app.is_turning` 时**触发
  `tick_working()`；空闲态不重绘。⇒ 侧栏里「已跑 2m10s」这类**时间文字**若由 UI 自己计时，
  空闲态会冻在旧值。既有解法是让 app 在快照里给成品串（`LoopView.next_label` 用**相对时长**而非钟点，
  理由是 `Instant → 本地时间` 的转换函数是 `pub(crate)`，见 `view.rs:166-174`）。
- **屏幕宽度是稀缺资源**：右侧开栏 ⇒ Chat 的 wrap 宽度变小 ⇒ 同样的历史折出更多行 ⇒
  滚动上限与画面必须同步（见下条）。窄终端（`MIN_WIDTH = 10`、`MIN_HEIGHT = 5`）必须有退化路径。

### 演进约束（本轮最硬的一条）

**纵向切分在两个地方各算一遍，且都被下游消费：**

| 位置 | 形态 | 消费者 |
|---|---|---|
| `draw.rs:169-174`（`ui_with`） | `Layout::vertical([Min(3), Length(input_h), Length(1)])` | 画面本身 |
| `draw.rs:246-253`（`chat_area`） | **同一条 `Layout::vertical` 复算一遍** | `chat_inner`(`:274`) → `chat_viewport` / `chat_inner_rect` |
| `draw.rs:167` | `sync_scroll_state(area.width, area.height, now)` | 滚动数学（整屏宽高 → 内区） |
| `events.rs:373` / `:430` | `chat_inner_rect` | 鼠标命中 / 选区坐标 |

`docs/adr/0054` 的否决理由原话：**「二者不同步会让滚动与画面错位（`app.rs:547` / `events.rs:368` 消费后者）」**。
另有两处宽度真值要跟着走：`app.input.wrap_width = input_inner_width(area.width)`（`draw.rs:178`，输入框折行与光标）
与补全浮层的覆盖区域（`draw.rs:195`，盖在 `chunks[0]` 上）。

`crates/ys-tui-coding` 有大量「两条路径必须同源」的守卫用例（如 `chat_inner_rect_agrees_with_viewport`
在 `draw.rs:4569`、`window_lines` 与 `draw_transcript` 等价性用例）—— 侧栏切分必须建同型守卫。

- **不新增 `TranscriptLine` 变体**（`subagent-progress/design.md` §3.6(2) 的既有结论）：新变体会打红三处编译
  （`LineClass::of` / `push_non_assistant` / `layout.rs::entry_sig`）并被四处 `_ =>` **静默**漏掉
  （`is_visible` / `indent` / `copy_text` / `replay_lines`）。
- `todo` 终态语义不可在 UI 侧重造：**无 open 项 ⇒ 清空 + 删文件**（`adapters/tools-todo/src/lib.rs:28` 的硬规则 2）
  ⇒ 「完成后从清单里消失」是既有语义，侧栏没有「已完成计数」可用，除非 app 侧另存。

### 组织约束

- 单人维护、单仓 workspace；无多版本兼容要求。变更面预计 =
  `ys-tui-coding`（`view.rs` / `draw.rs` / `layout.rs` / `events.rs`）+ `apps/coding-agent`（`view.rs` / `app_loop.rs`）。
- 构建用 `./scripts/cargo-build.sh`，全量测试用 `./scripts/par-test.sh`（见 `AGENTS.md`）。
- 「关键逻辑要做变异测试」是既有约定：切分同源、空栏不塌陷（框照画）、UTF-8 截断三处都应有变异用例。

- **空态不塌陷**（仅适用于**栏已打开**时）：框常驻、内容可空。
  ⇒ 栏打开后，Chat 的可用宽度就变窄了（即使两栏都空），历史折行行数随之改变，
  `sync_scroll_state` 的 max_scroll 与「贴在底部」的定位都会与之前不同。
- **默认隐藏的收益（比空态细节重要得多）**：默认关闭 ⇒ **启动画面与今天逐字节相同**，
  既有视觉类用例不受影响。`long-horizon-todo/design.md:217` 的「空 ⇒ 0 宽 ⇒ 逐字节不变」
  在**启动场景**下重新成立，不再需要为侧栏做特例。
- **但「逐字节不变」只保护了启动**：用例里只要出现一次「打开开关」，宽度就会变。
  ⇒ 既有用例**默认不碰开关**即可全绿；新用例则显式打开。
  默认打开的话，所有视觉用例都要跟着改宽度假设 —— 那才是真正的噪声源。
- **开关的落点已有现成范式可照抄**：UI 侧的**运行期真值在 `App`**，`CodingView` 只给启动初值
  （`App::new` 消费一次），`Outbound::View` 快照**不覆盖**它 —— `/thinking` 的 `thinking_visible`
  （`app.rs:118`、`commands.rs:1094`）与 `/theme` 的 `App.theme`（`app.rs:335`）都是这个形状。
  侧栏开关按同一形：`App` 上一个 `bool` + 一个 `LocalAction` 变体 + 命令表一条。
- **开关本身不需要动协议 crate**：`LocalAction` 在 `ys-tui-coding` 内闭环；`/theme` 额外发 `Request`
  是为了**落盘**（`events.rs:995`），不是为了生效。只有选了「跨重启保持」（Q3b）才要加 `Request` 变体。
- **切换宽度不会把布局与滚动弄脏**（已逐处核实，这条容易误判所以写死）：
  `layout.sync` 自己把 `width` 入失效判据（`layout.rs:188`），宽度一变就 `entries`/`gaps`/`starts`
  全清并 `structural_rev += 1`；而 `sync_scroll_state` 的增量补偿显式要求 `!width_changed`
  （`app.rs:743`）—— 所以「变宽/变窄导致总行数变」不会被误会成「下方来了新内容」而把视窗推歪。
  ⇒ 开/关只靠**一个宽度真值**的变化就能自然收敛，不需要额外的手工作废。
- **宽度真值必须仍然只有一处**：今天 Chat 内宽来自 `chat_viewport` → `chat_inner` → `chat_area`
  （`draw.rs:274-289`，渲染与滚动共用）；`input_inner_width`（`draw.rs:209`）独立算整宽。
  开关**只能**影响 `chat_area` 返回的矩形宽度，**不得**在 `draw_transcript` / `events.rs` 里再减一次栏宽
  —— 那正好是本节开头那个错位 bug 的成因。
- **高度预算自动消失**：宽固定、高固定（50/50 不随内容变）⇒ 不需要「谁先缩 / 谁可折到 0」的优先级。
  但 **溢出仍需内容级解法**：不能靠滚动、也不能把内容顶出去（否则会越界画到 Input 上）。
  溢出时取「前 N 项 + 一行 `+M`」这类截断，**必须**保留那条 `+M`（否则用户以为清单只有 N 项）。
- **同源要求随之翻倍**：可滚动区只有 Chat（`draw.rs:4` 的注释：「唯一可滚动」），右列第一版是**纯静态、不滚动**。
  若以后要做栏内滚动，需新开滚动状态与命中分支（`events.rs` 的现有命中测试全部假设只有一个可滚动区）。

### 「主题」的取法：已定为 `description`，代价落在 trait 签名上

**取法已定（用户澄清）**：主题 = 生成提示词时就产出的 `description`，不是从 `prompt` 猜。
这与既有设施完全吻合：

- `subagent_async` 的 spec 里 `description` 本就是 **required**，原文是「3-5 word label for the user
  (shown in the tool line)」（`apps/coding-agent/src/subagent.rs:2015`、`:2021`）—— 它**就是**为展示造的词。
- `transcript.rs` 的 `SUMMARY_KEYS` 已把 `description` 当作工具行摘要的末位候选（该数组注释就引了
  ADR-0022 决策 11），所以「主题」这个概念在 UI 侧已有先例，不需要新词汇。

**代价落在 trait 签名，不在 UI**：

```rust
// 今天（crates/ys-tool/src/subagent.rs:76-82）
fn spawn(&self, prompt: &str, model: Option<&str>, mode: SubagentMode,
         boundary: Option<&dyn BoundarySource>) -> Result<String, String>;
```

`description` 没有位置。波及面（已逐处核实，**比想象小**）：

| 位置 | 性质 | 改动 |
|---|---|---|
| `crates/ys-tool/src/subagent.rs:76` | trait 默认实现 | 加形参（默认实现返回 `Err`，不动逻辑） |
| `apps/coding-agent/src/subagent.rs:1171` | **唯一生产实现** | 收下主题并记录 |
| `apps/coding-agent/src/debug_log.rs:374` | 转发/包装层 | 跟着透传 |
| `crates/ys-tool/src/context.rs:99` | 测试 `FakeRunner {}` | 默认实现 ⇒ 不一定要改 |
| `apps/coding-agent/src/subagent.rs:2042` | 调用点（`SubagentAsyncTool::call`） | 把已有的 `description` 传下去 |
| `apps/coding-agent/src/subagent.rs:4370` | 用例里的直接调用 | 补一个实参 |

> **签名的兼容性退路**（若不想动 `spawn`）：在 trait 上**新增一个带默认实现的平行方法**（如
> `spawn_labeled(...)` 默认返回「不支持」）。但那样会得到两条派发路径，且模型工具只有一个入口
> ⇒ 实际只会走一条。建议直接改 `spawn`：**它已有默认实现**，加形参不会让既有实现方编译不过（除非它们覆写了它）。

**「主题」住在哪**：`SubagentHandles::register`（`subagent.rs:429`）今天只存
`handle → Weak<QueueBoundarySource>`。主题字段必须与 `Weak` **同生命周期**：
注册项在子 agent 收尾时失效（`weak.strong_count() == 0` ⇒ `live_handles` 自动清理），
主题必须跟着同进同出，否则会出现「名单空了、主题还挂着」的悬置幽灵行。

**展示形态（用户已定）**：只展示主题的**开头一段**，超出栏宽就截断。⇒ 不需要换行、不需要省略多段，
截断必须走 `truncate_to_width`（字符边界 + CJK 算 2 列），不得用 `split_at`（见上）。

### 两栏 50/50 定下后剩下的细节

用户已定**高度各占 50%**、**空也不塌陷**。分母与奇偶必须在实现前钉死：

- **50% 是「右列内容高的 50%」还是「整个 Chat 高的 50%」** —— 若每栏内部还要画标题行
  （如 `todo` / `subagents`），标题占哪一份？剩下给正文的行数才是真正可用的行数。
- **奇数除不尽**：`H = 25` 时两栏要分别得到 12 / 13（或 13 / 12），**不能靠 `H/2` 各算一次**
  （会漏掉一行，且两栏合起来不等于 H ⇒ ratatui 的 `Constraint::Fill/Percentage` 行为要实测确认，
  不能靠手算假设）。
- 空态问题已由用户答定：框常驻、内容可空（见上条「空态不塌陷」）。

### 与状态行的重叠（**本需求已解除**）

用户已明确「harness 状态可以先不展示」⇒ 侧栏 v1 **不碰**底部 `status_text`
（`draw.rs:967`，含 `⏳ Working` / `status_note` / `[loop]` / `[goal]` / model / `↓ 有新内容` / 轮号 / tokens / 时长）。

这条记在这里是为了**守住边界**，而不是当待办：

- **不得为了凑 harmony 顺手搬段**。它的「剥尾顺序」有既有用例守着（`draw.rs:3177` 起），改动它不在本需求范围内。
- 但从「信息重复」角度看，落地后 `[loop]` / `[goal]` 与侧栏内容**并不重叠**（侧栏是 todo + subagent）⇒ 无冲突。
- 若将来要搬，那是独立需求：它要重新回答「窄终端先丢什么」。

---

## 需求范围

### 范围内

- Chat 区域右侧新增**一条窄靠边栏**（两列布局），**固定宽度**（「静态大小」，不做自适应），
  栏内**纵向再切成多个横栏**（section 栈）。v1 的栈内容：
  - `todo 清单`（项 + 状态）
  - `subagent 运行情况`：**只展示谁在跑 + 它的工作主题**（不要轮号 / 工具名 / token / 耗时）
  - **不包含 harness 状态**（用户已明确可先不展示）
  - 栈仍按「一列 section」组织（用户原话带「等信息」），不为将来加栏把形状写死。
- 数据经 `CodingView` 新字段由 app 侧投影；TUI 侧只负责渲染与几何。
- 侧栏与 Chat / Input / Status 三块的几何**同源**（滚动、鼠标命中、选区、输入折行全部跟着走）。

### 范围外（明确不做的）

- subagent 正文 / 逐条工具调用的实时流（ADR-0022 决策 2；翻案条件是「UI 上要看逐条工具调用」，
  若本需求只要「谁在跑 + 到什么程度」则仍不翻）。
- 侧栏内的交互（聚焦、点选、折叠、跳转到子 agent 转录）—— 第一版只读展示。
- 把 `SubagentBoard` / `ProgressTap` / `subagent_status` 工具整套补齐（ADR-0054 未落地的其余 6 条决策）。
- `-p` / `--json` 模式的对应输出（它们不是 TUI 形态）。
- `/resume` 回放侧栏历史（侧栏是**当下状态**视图，不是历史）。

### 关键场景

| # | 场景 | 期望 |
|---|---|---|
| S1 | 空闲、无 subagent、无 todo（**未打开侧栏**） | 无右栏、Chat 整宽 ⇒ **与今天逐字节相同**（默认关闭，这是常态） |
| S2 | 主回合内派发 2 个异步子 agent | subagent 栏两行 `句柄 · 主题开头`；用户不必翻 transcript 才知道它们在跑 |
| S3 | 主回合结束、子 agent 仍在跑（既有实测：转录在回合结束后 2~6 分钟才落盘） | 该横栏**仍然显示它们在跑**（这正是「用户要看的反例」） |
| S4 | 子 agent 回来但主 agent 还没取走结果 | 横栏区分「已完成 / 待送达」与「在跑」 |
| S5 | 目标驱动的长任务跑了 20 轮、todo 有 8 项（3 项 in_progress） | todo 横栏在**窄列**里可读（一行一项 + 状态），项数超栏高时有确定行为 |
| S6 | 窄终端（如 80×24，或 40 列）**且已打开侧栏** | 有明确退化：要么内部截断 / 折叠，要么整栏隐藏但 Chat 不被压坏（开关本身也是一条退路） |
| S9 | 主题很长（模型忽略「3-5 词」写了整句） | 截断到栏宽，且**不得 panic**（字符边界） |
| S10 | 只有 todo、没有 subagent（或反之） | 空的那栏**保留框**、内容为空（不塌陷、不让位给另一栏） |
| S11 | 已打开侧栏、但两栏都空（刚 `/clear`） | 两个空框并列（不塌陷） |
| S12 | 用户敲打开命令（窗口宽度不变）：**默认关 ⇒ 打开** | 右栏出现、Chat 变窄并**全量重折行**（`layout.rs:188`）；视窗不跳（宽度变化不触发增量补偿，`app.rs:743`） |
| S13 | 用户敲关闭命令：**打开 ⇒ 关** | 右栏消失、Chat 变宽并全量重折行；同样不跳视窗；回到 S1 的逐字节基线 |
| S7 | `/clear`、`/new`、`/resume` 换会话 | todo 横栏跟着换（`TodoStore::retarget`）；subagent 横栏**不清理**（子 agent 不随会话重建） |
| S8 | 两栏都有内容、但 Chat 行数很少（如 10 行高的窗口） | 无高度竞争（50/50 固定），但**每栏内部已有确定的溢出截断**（前 N 项 + `+M`） |

### 异常/边界场景

- todo 单项内容极长（`MAX_CONTENT_CHARS` 已有限制，仍是几十到几百列）⇒ 必须截断到栏宽。
- 子 agent 面板与 `PendingResults` 的淘汰（`MAX_PENDING_RESULTS`）同时发生 ⇒ 侧栏显示与真值可能短暂不一致。
- 终端 resize 过程中（宽度抖动）⇒ 不能 panic，也不能让滚动位置与画面错位。
- `MIN_WIDTH` / `MIN_HEIGHT` 边界（`draw.rs:91-92`：10 / 5）。

---

## 未澄清问题

- [x] **Q1 · 几何形态**（已答）：右侧**一条窄靠边栏**，栏内**纵向切成多个横栏**（section 栈）。
- [x] **Q2 · 宽度策略**（已答）：**固定宽度**（「静态大小」），不做比例/自适应。
      遗留小问 **Q2b**：固定宽度与终端的矛盾怎么收——同 `MIN_WIDTH = 10`（`draw.rs:91`）时
      「栏宽 + Chat 最小可读宽」可能超过屏宽。我按「屏宽不足 ⇒ 整栏隐藏」理解，请确认。
- [x] **Q4 · subagent 粒度**（已答）：**只要「谁在跑 + 工作主题」**，不要轮号/工具名/token/耗时。
      ⇒ **不需要** ADR-0054 的 `SubagentBoard` / `ProgressTap` / `AgentEvent::SubagentProgress`。
- [x] **Q4b · 主题来源**（已答）：用**生成提示词时的 `description`**，展示时取开头截断。
      ⇒ 需动 `ys-tool::SubagentRunner::spawn` 签名（生产实现方仅一处，波及 `debug_log.rs` 转发层）。
- [x] **Q6 · section 高度**（已答）：**各占 50%**（todo / subagent 均分）。
- [x] **Q6b · 空态**（已答）：**框常驻、内容可空**（「固定大小就行，没有的话就不展示内容即可，
      但是框框还是需要保留」）。⇒ 仅在**已打开**时生效：不塌陷、不让位。
- [x] **Q8 · harness 段与状态行的分工**（已答）：v1 **不展示 harness 状态**，`status_text` 一个像素不动。
- [x] **Q3 · 默认显隐与开关**（已答）：**默认隐藏，手动打开**。
      ⇒ 需新增本地动作 + 命令表条目 + `App` 上的开关位（默认 `false`）。
      ⇒ 默认关闭带来的关键收益：**启动画面与今天逐字节相同**，既有视觉用例不必改。
- [x] **Q3b · 开关是否跨重启保持**（已答）：**不保持**。开关状态只活在当前进程（`App` 上的 `bool`），
      **不落盘、不进 `state.json`** ⇒ 不需要 `Request` 变体，不动 `ys-protocol`。
      与 `thinking_visible` / `theme` 的区别是**有意的**：那两个是长期偏好的显示习惯，
      而侧栏是个随时可开的重型面板，每次启动默认安静更符合“不扰动”。
- [ ] **Q5 · todo 栏的取舍**：显示全部项（含刚完成、即将被清空的）还是只显示非终态？需要「✓ 3/8 完成」这类
      计数吗？（`TodoStore` 在无 open 项时**清空**，所以计数需 app 侧另存。）
- [ ] **Q1b · 竖栏覆盖哪些行**：这条窄栏只占 **Chat 那一段**高度（Input 与底部状态行仍占满整宽），
      还是一路通到底（Input 也变窄）？我按前者理解（Input 保持整宽更合理），请确认。
- [ ] **Q6c · 50/50 的三个细节**：**溢出**（截断 `+N` / 继续增长压 Chat）？
      **奇数除不尽**（`H = 25` 时 12/13 还是 13/12）？**标题行占不占一份**？
- [ ] **Q7 · 刷新时机**：只随 `Outbound::View` 快照刷新（现成的 18 个推送点），还是需要回合内实时刷新？
      后者要动 `run.rs:463` 的 tick 条件（今天只在 `is_turning` 时 tick，且只在 `is_turning` 时 mark 重绘）。
- [ ] **Q9 · section 集合的可配性**：v1 就写死 todo + subagent 两栏，还是留一个「谁在栈里」的装配点？
      （即便 v1 只两栏，渲染层也应按「一列 section」组织，不为将来加栏把形状写死。）

---

## 后续建议

- 本需求**只剩 Q1b / Q2b / Q5 / Q6c / Q7 / Q9 待答**
  （Q1 / Q2 / Q3 / Q3b / Q4 / Q4b / Q6 / Q6b / Q8 已定）。
  **Q9（命令叫什么名字）** 不属于待答项，但应在 `arch-design` 一并定 —— 它影响 `/help` 与补全列表。
  剩下的里面只有 **Q2b（窄屏退化）** 会改架构形状；其余四条都是纯排版取舍，
  可在设计文档里先给默认值，看到实物再改。

- **默认隐藏把风险削掉了一大块**：启动画面回到零扰动，既有视觉用例不用改（它们不碰开关）；
  宽度变化只发生在用户主动切换的那一瞬，而那条路径已被 `layout.rs:188` 的全量重折行
  与 `app.rs:743` 的「不加补偿」覆盖。**那条“列宽开关”的建议已不再需要**。
- **开关也让 Q2b 少了一半压力**：用户手上有退路。但仍需定「窄到放不下时到底隐不隐」——
  不能拿「你自己敲命令关它」当默认行为。
- 建议用 `prototype` 先验证几何假设：在 `draw.rs` 里做一次「两处纵向切分同源」的最小实验，
  确认滚动 / 鼠标命中 / 选区在开栏后仍然对齐（这是本需求唯一的**正确性**风险，其余都是排版取舍）。
- 与既有决策的关系（设计时须显式交代）：
  - `ADR-0054` 决策 4（不新增 `Outbound` 变体）与决策 1（进程级状态板）—— 侧栏是它们的**下游消费者**，
    不改变其形态。**本轮不实现它们**：主题走 `description`，不需要板上其余字段。
  - `ADR-0022` 决策 1（窄 trait）—— 本轮要为 `spawn` 加主题形参，属于对它的**最小扩充**；
    扩完仍不出现 `Model` / `Session` / `AgentEvent`，故「不得成环」的理由依旧成立。
    决策 2（子事件不进父流）**不变** —— 本轮只要「谁在跑 + 主题」，正好不需要翻案。
  - `long-horizon-todo` Q7（侧栏简单展示）与 `subagent-progress` Q2（右侧栏宜与 todo 合并）——
    本需求就是这两条悬空决策的兑现，设计应引用它们而非重新论证。
