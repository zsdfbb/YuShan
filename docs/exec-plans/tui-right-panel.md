# 执行计划：TUI 右侧双栏（todo + subagent 状态）

> 对应设计：`docs/arch/tui-right-panel/design.md`（权威）、`docs/design-plans/tui-right-panel.md`
> 决策记录：`docs/adr/0075-tui-right-panel.md`
> 基线（review 用）：`1b27d3c`
> 纪律：每个 `impl` 与它的 `test` 成对；同波文件集**互不相交**；单单元 ≤5 文件 / ≤10 分钟。
> 构建入口：`./scripts/cargo-build.sh`；全量测试入口：`./scripts/par-test.sh`（**不用**裸 `cargo`）。

## 波次总览

波次按依赖深度编号。同波内文件集互不相交，可并发派发（上限 3）。

| 波次 | 单元 | 文件集 | 说明 |
|---|---|---|---|
| **W0** | **U1 → T1** | `crates/ys-tool/src/subagent.rs` | trait 加形参。**必须先做**：它一改，全仓实现立刻编译失败 |
| **W1** | **U2 → T2** | `apps/coding-agent/src/subagent.rs` | 补上 trait 实现（让 workspace 重新可编译）+ `HandleEntry` + `live_labels` |
| | **U3 → T3** | `crates/ys-tui-coding/src/view.rs`、`crates/ys-tui-coding/src/lib.rs` | 线上类型 + 导出 |
| **W2** | **U4 → T4** | `crates/ys-tui-coding/src/draw.rs` | 几何单源 + `draw_sidebar`（依赖 U3 的类型） |
| | **U5 → T5** | `apps/coding-agent/src/view.rs` | `panel_view` + `build_view_with_loop` 加形参（依赖 U3、U2） |
| **W3** | **U6 → T6** | `crates/ys-tui-coding/src/app.rs`、`crates/ys-tui-coding/src/commands.rs` | 开关 + `panel_visible` + `selection.clear`（依赖 U4） |
| **W4** | **U7 → T7** | `apps/coding-agent/src/app_loop.rs` | 六处调用点 + `last_view` 同步 + `TurnCtx` 四形参 + 回合中推送（依赖 U4、U5、U6） |

**关键串行**：U1 必须先于 U2。`SubagentRunner` 唯一生产实现就在 `apps/coding-agent/src/subagent.rs`，
U1 改 trait 后 U2 落地前 workspace **不可编译** —— 这两个单元是「串行对」，不能进同一波。

**为什么 U3 与 U5 能并行**：U3 只写 `crates/ys-tui-coding/src/{view,lib}.rs`，U5 只写
`apps/coding-agent/src/view.rs`，文件集不相交。U5 依赖 U3 定义的类型——它在 W2 派发，此时 U3 已完成。

**为什么 U7 必须最后**：它是唯一同时依赖 `frame_areas`（U4）、`panel_view` 与 `build_view_with_loop`
新形参（U5）、`App.panel_visible`（U6）的单元。提前派发会造出「形参还没加」的编译错误。

---

## W0 — trait 形参

### U1: `SubagentRunner::spawn` 加 `_label: &str`
- **type**: impl
- **依赖**: 无
- **涉及文件**: `crates/ys-tool/src/subagent.rs`
- **工作量**: 10 分钟
- **验收方法**: `./scripts/cargo-build.sh check -p ys-tool` — 期望通过；`cargo test -p ys-tool` 通过
- **优先级**: 必测

变更点（唯一文件）：
- `:76-81` 的 `fn spawn` 签名，在 `_prompt: &str` 之后插入 `_label: &str`
- 文档注释补一句：`label` 是模型给的人类可读主题（schema 里 `required`），用于 UI 展示

**不要**改默认实现体（仍是 `Err("本实现不支持后台派发")`）。
**不要**在这个单元里改任何调用点 —— 它们全在 `apps/coding-agent`，属 U2。

### T1: trait 默认实现与 object-safety 的回归
- **type**: test
- **依赖**: U1
- **涉及文件**: `crates/ys-tool/src/subagent.rs`（同一文件的 `mod tests` 追加）
- **工作量**: 10 分钟
- **验收方法**: `cargo test -p ys-tool subagent` — 期望新用例通过，既有用例全绿
- **优先级**: 必测

断言：
1. 默认实现对**新签名**返回 `Err`（证明加形参没破坏默认路径）
2. `dyn SubagentRunner` 仍可构造（object-safety 未被 `&str` 形参破坏）
3. `FakeRunner`（`context.rs:99`）与 `ProbeRunner`（`basic.rs:2464`）**零改动**仍编译 ——
   这两个是 `impl SubagentRunner for X {}` 空 impl，由 `cargo test -p ys-loop` 顺带证明

**变异**：把 `_label` 改成 `Option<&str>` 且给默认 `None` ⇒ 无用例会红（这正是设计拒绝 `Option` 的理由：
它是「静默的无标题项」）。故本用例**不测**这一点，只在注释里说明。

---

## W1 — 两个并行单元

### U2: `HandleEntry` + `live_labels` + 实现新 `spawn` 签名
- **type**: impl
- **依赖**: U1
- **涉及文件**: `apps/coding-agent/src/subagent.rs`
- **工作量**: 10 分钟
- **验收方法**: `./scripts/cargo-build.sh check --all-targets` — **全量 workspace 必须重新变绿**（这是 W0→W1 的硬门槛）
- **优先级**: 必测

变更点（唯一文件）：
1. `:420-421` `SubagentHandles{ inner: Mutex<HashMap<String, Weak<QueueBoundarySource>>> }`
   → value 改为 `HandleEntry { target: Weak<QueueBoundarySource>, label: Arc<str> }`
2. `:431` `register(handle, own)` → `register(handle, own, label: Arc<str>)`
3. `:439` `send` 里 `Weak::upgrade` 改为 `entry.target.upgrade()`；`retain(|_, e| e.target.strong_count() > 0)`
4. `:468` `live_handles` 同步改字段访问（**行为不变**，`live_handles` 的三个既有用例必须仍绿）
5. **新增** `pub fn live_labels(&self) -> Vec<(String, Arc<str>)>` —— 与 `live_handles` 同形：
   先 `retain` 清理死项，再收 `(handle, label.clone())`，按 handle 升序
6. `:1171` 生产 `spawn` 加 `label: &str` 形参；`:1193` `register(handle, own, label.into())`
7. `:2036` `SubagentAsyncTool::call` 取出 `description` 并透传给 `spawn`：
   ```rust
   let description = input.get("description").and_then(|v| v.as_str())
       .ok_or_else(|| ToolError::InvalidInput("`description` 必填".into()))?;
   ```
   调用改为 `.spawn(prompt, description, model, mode, ctx.boundary)`
8. `:4370` 的既有用例补一个 `label` 实参
9. `SubagentSetup` 新增 `pub fn live_labels(&self) -> Vec<(String, Arc<str>)> { self.handles.live_labels() }`
   （仿 `pending_results()` 在 `:1146` 的访问器形态）

**必须保持**：`send` 的「先取目标、放锁、再 push」纪律（`:441` 附近的注释）—— 临界区内不得有外部调用。

### T2: `live_labels` 生命周期与 label 透传
- **type**: test
- **依赖**: U2
- **涉及文件**: `apps/coding-agent/src/subagent.rs`（`mod tests` 追加）
- **工作量**: 10 分钟
- **验收方法**: `cargo test -p ys-coding-agent subagent` — 新用例通过
- **优先级**: 必测

断言（对应设计 G12 + 测试策略补充）：
1. `register(h, own, "查日志".into())` ⇒ `live_labels()` 含 `("h", "查日志")`
2. `drop(own)` ⇒ `live_labels()` 不含它（`Weak` 死即消失）
3. 多个 handle ⇒ 结果按 handle 升序
4. `live_handles()` 与 `live_labels()` 的**键集合恒等**（同一张表，不会分叉）
5. `SubagentAsyncTool::call` 透传：入参 `description: "探索"` ⇒ `spawn` 收到的 label 是 `"探索"`

**变异**：`register` 存 `Arc<QueueBoundarySource>`（强引用）⇒ 用例 2 变红。
`retain` 前不 `upgrade`（改判 `label` 非空）⇒ 用例 2 变红。

### U3: 线上类型 `PanelView` / `TodoLine` / `RunningLine` / `RowMark`
- **type**: impl
- **依赖**: 无（与 W1 的 U2 并行安全：文件集不相交）
- **涉及文件**: `crates/ys-tui-coding/src/view.rs`、`crates/ys-tui-coding/src/lib.rs`
- **工作量**: 10 分钟
- **验收方法**: `./scripts/cargo-build.sh check -p ys-tui-coding` — 通过
- **优先级**: 必测

变更点：
1. `view.rs` 追加四个类型（形态见 design-plan §3.2，逐字照抄）：
   `PanelView { todos, todos_hidden, running, running_hidden }`、`TodoLine { text, mark }`、
   `RunningLine { handle, label }`、`RowMark { Plain, Active, Done }`
   全部 `#[derive(Clone, Debug, PartialEq, Eq)]`（`RowMark` 加 `Copy`）
2. `PanelView` 实现 `Default`（两个空 `Vec` + 两个 `0`）
3. `CodingView` 加字段 `pub panel: PanelView`（**非 `Option`** —— 空态是两个空 Vec）
4. `impl Default for CodingView`（`:188`）补 `panel: PanelView::default()`
5. 三个常量就位并 `pub`：`PANEL_TODO_MAX = 24`、`PANEL_RUNNING_MAX = 8`（`view.rs` 内）
6. `lib.rs:66` 的 `pub use view::{...}` 追加 `PanelView, TodoLine, RunningLine, RowMark`

**注意**：`CodingView` 是 `#[derive(PartialEq)]` —— 加 `panel` 后自动对 `panel` 敏感，
G7 的变更检测因此成立，无需手写 `PartialEq`。

### T3: 类型层的不变量
- **type**: test
- **依赖**: U3
- **涉及文件**: `crates/ys-tui-coding/src/view.rs`（`mod tests` 追加）
- **工作量**: 10 分钟
- **验收方法**: `cargo test -p ys-tui-coding view` — 通过
- **优先级**: 必测

断言：
1. `CodingView::default().panel == PanelView::default()`（两个空 Vec）
2. `CodingView` 的 `PartialEq` **对 `panel` 敏感**：只改一个 `TodoLine.text` ⇒ 两值不等
   （这是 G7 变更检测的地基，必须是真断言）
3. `RowMark` 三态互不相等且可 `Copy`

---

## W2 — 两个并行单元

### U4: 几何单源 `frame_areas` + `draw_sidebar`
- **type**: impl
- **依赖**: U3
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`
- **工作量**: 10 分钟
- **验收方法**: `./scripts/cargo-build.sh check -p ys-tui-coding` 通过 + `cargo test -p ys-tui-coding draw` 既有用例全绿
- **优先级**: 必测

变更点（同一文件，5 处）：
1. `MIN_WIDTH`（`:91`）旁新增 `const SIDE_WIDTH: u16 = 30;` 与 `fn side_width(total: u16) -> u16`
   —— `SIDE_WIDTH.min(total.saturating_sub(MIN_WIDTH))`。注释必须写明：
   **不用 `Layout::horizontal([Min(1), Length(30)])`**，实测它会优先满足 `Length`、把 Chat 压到 1 列（W=20 ⇒ chat 1 / 栏 19）
2. 新增 `struct FrameAreas { chat, input, status, side: Option<Rect> }` 与 `fn frame_areas(app, total) -> FrameAreas`。
   **切分顺序不可反**：先在 `total` 上 `Layout::vertical`（`input_height(app, total.width)`），
   再只把 `v[0]` 横向让位给侧栏。注释写明「顺序反了会把决策 2 消除的反馈环引回来」
3. `ui_with`（`:169-195`）：`chunks` 换成 `let a = frame_areas(app, area);`；
   `chunks[0]/[1]/[2]` → `a.chat / a.input / a.status`；补全浮层 `:195` 改盖 `a.chat`
4. `chat_area`（`:246`）体改为 `frame_areas(app, area).chat`（**保留函数**，签名不动）
5. `input_geom`（`:228-233` 的内层 `Layout`）改为取 `frame_areas(app, Rect::new(0,0,width,height)).input`
   的 `[1]`（**仍按整屏 `width` 构造**，与今天就一致）
6. 新增 `fn draw_sidebar(frame, area: Rect, view: &PanelView, palette: Palette)`：
   纵向 `Layout::vertical([Percentage(50), Percentage(50)])` 切两栏（实测三种写法逐格相同，
   **不写奇偶补偿**）；每栏 `Block::bordered().title(...)` + `Paragraph`；
   每行内容先过 `truncate_to_width`（显示列截断）；空态仍画框；溢出画 `+N`
7. `ui_with` 末尾：`if let Some(r) = a.side { draw_sidebar(frame, r, &app.view.panel, palette); }`

**边界**：`draw_sidebar` 只读 `PanelView`，不碰 `app.view` 的其它字段，不读 `TodoStore`。

### T4: 几何不变量（G1–G6）
- **type**: test
- **依赖**: U4
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`（`mod tests` 追加）
- **工作量**: 10 分钟
- **验收方法**: `cargo test -p ys-tui-coding draw` — 新用例通过
- **优先级**: 必测

断言（design-plan §7 的 G1–G6）：
1. **G1 同源恒等**：同一帧多次调 `frame_areas` 结果相同
2. **G2 关栏逐字段相同**：`panel_visible == false` ⇒ `chat/input/status` 与「改造前的三段切分」逐字段相等；
   `input_height` 入参恒为整屏宽
3. **G3 钳制**：W=80 ⇒ 30、W=35 ⇒ 25、W=30 ⇒ 20、W≤10 ⇒ `side == None`
4. **G4 50/50**：H ∈ {5,7,25,40} ⇒ 两栏和 == 栏高、上 ≥ 下、差 ≤ 1
5. **G5 三路同源**：开栏后 `chat_viewport` / `chat_inner_rect` / `input_geom` 与 `frame_areas` 的
   `.chat` / `.input` 一致（把既有 `chat_inner_rect_agrees_with_viewport`（`:4569`）扩成
   `panel_visible ∈ {false,true} × 4 组尺寸`）
6. **G6 浮层不越界**：开栏 + 弹补全 ⇒ 浮层右边界 ≤ `a.chat.right()`

**变异**（逐个注入，期望红）：
- `frame_areas` 里改传 `main.width` 给 `input_height` ⇒ G2 红
- `saturating_sub` 换普通减法 ⇒ G3 在 W≤10 时 panic 或错值
- `H/2` 各算一次 ⇒ G4 和 ≠ 栏高
- 浮层仍盖 `chunks[0]` ⇒ G6 红

### U5: `panel_view` 投影 + `build_view_with_loop` 加形参
- **type**: impl
- **依赖**: U3（类型）、U2（`live_labels`）
- **涉及文件**: `apps/coding-agent/src/view.rs`
- **工作量**: 10 分钟
- **验收方法**: `./scripts/cargo-build.sh check -p ys-coding-agent` 通过
- **优先级**: 必测

变更点：
1. 新增 `pub fn panel_view(todo: &ys_tools_todo::TodoStore, subagent: Option<&crate::subagent::SubagentSetup>) -> PanelView`
   （**2 参，无 `theme`** —— 形态与规则见 design-plan §3.1，逐字照抄）
   - 常量：`MAX_TODO_CONTENT_CHARS = 96`、`MAX_DESCRIPTION_CHARS = 120`（本文件）
   - 截断按**字符**（`chars().take(n)`），**不是**字节、不是列
   - `TodoStatus` 的 `match` 必须带 `_ => RowMark::Plain`，注释写明「新增状态会静默显示为 Plain」
   - 溢出计数填 `todos_hidden` / `running_hidden`
2. `build_view_with_loop`（`:194`）第 11 个形参 `panel: PanelView`，末尾 `view.panel = panel;`
3. **改文件头 `:37-40` 的注释**：原文「形参已达 8 个。**不再加参数**……」——
   本需求加了第 11 个，注释必须改为说明为什么仍走形参（`panel` 是每请求都要刷新的数据投影，
   与 `stats`/`theme` 同属「每请求变化」，收进 `ViewInputs` 是更大的重构、不在本需求范围）

### T5: 投影规则与封顶（G10）
- **type**: test
- **依赖**: U5
- **涉及文件**: `apps/coding-agent/src/view.rs`（`mod tests` 追加）
- **工作量**: 10 分钟
- **验收方法**: `cargo test -p ys-coding-agent view` — 通过
- **优先级**: 必测

断言（G10 + 补充）：
1. 25 条 todo ⇒ `todos.len() == 24`、`todos_hidden == 1`
2. 9 个 running ⇒ `running.len() == 8`、`running_hidden == 1`
3. 96 字符截断按**字符**：200 个 CJK 字符 ⇒ 结果 `chars().count() == 96`，且不 panic
4. 状态映射：`Pending`/`InProgress`/`Completed`/`Cancelled` → `Plain`/`Active`/`Done`/`Done`
5. 空 store ⇒ 两个空 Vec + 两个 0（**不是** `None`）

**变异**：改字节截断（`&s[..96]`）⇒ 用例 3 panic 或切坏 CJK。
去掉封顶 ⇒ 用例 1/2 红。

---

## W3 — 开关

### U6: `/panel` 命令 + `panel_visible` + `selection.clear`
- **type**: impl
- **依赖**: U4（`draw_sidebar` 要读 `panel_visible`）
- **涉及文件**: `crates/ys-tui-coding/src/app.rs`、`crates/ys-tui-coding/src/commands.rs`
- **工作量**: 10 分钟
- **验收方法**: `./scripts/cargo-build.sh check -p ys-tui-coding` 通过
- **优先级**: 必测

变更点：
- `app.rs`：`App` 加 `pub(crate) panel_visible: bool`（`thinking_visible`（`:118`）旁，注释写明
  「纯 UI 本地状态，不落盘、不被 `Outbound::View` 覆盖、不给视图初值字段」）；
  `App::new`（`:222`）在 `theme`（`:227`）之后写 `panel_visible: false` —— **不**从 `view` 读初值
  （与 `thinking_visible`（`:225`）的「启动消费一次」刻意不同：侧栏没有持久化镜像可读）
- `commands.rs`：
  - `static COMMANDS: [CommandSpec; 19]`（`:53`）→ `; 20]`，数组内加 `/panel [on|off]` 条目
  - `LocalAction` 加 `Panel(Option<bool>)`（`:185` 的 `Thinking` 旁）
  - 解析臂照抄 `Thinking`（`:294-303`）三分
  - `apply_local`（`:1068`）加一臂：
    ```rust
    LocalAction::Panel(explicit) => {
        app.panel_visible = explicit.unwrap_or(!app.panel_visible);
        // 宽度变了 ⇒ 重折行 ⇒ 全局视觉行号空间整体作废：残留锚点会高亮到无关文字。
        app.selection.clear();
        format!("panel: {}", if app.panel_visible { "on" } else { "off" })
    }
    ```
- 既有用例 `all_commands_len_is_19`（`:1331`）改成 20

**不要**改 `events.rs` —— `Action::Local` 已把动作交给 `apply_local`，侧栏没有对应的 `Request`。

### T6: 开关语义（G11 + selection）
- **type**: test
- **依赖**: U6
- **涉及文件**: `crates/ys-tui-coding/src/commands.rs`（`mod tests` 追加）
- **工作量**: 10 分钟
- **验收方法**: `cargo test -p ys-tui-coding commands` — 通过
- **优先级**: 必测

断言（G11）：
1. `/panel` 无参 ⇒ 翻转（false→true→false）
2. `/panel on` / `/panel off` ⇒ 显式设置
3. `/panel xx` ⇒ `Hint` 且 `panel_visible` **一字不改**
4. `apply_local` 写的是 `app.panel_visible`，**不写** `app.view.panel`
5. 收到 `Outbound::View`（任意快照）后 `panel_visible` **仍是**原值
   —— `Outbound::View` 臂只写 `self.view`（`app.rs:272`），不碰独立字段；
   `Outbound::SessionResumed` 臂**有**一次破例（`:313` 写 `thinking_visible`），
   但侧栏**不加**同类破例：开关是纯 UI 偏好，换会话不改它
   （变异：`App::new` 从 `view` 取初值、或 `apply_outbound` 覆盖它 ⇒ 红）
6. 开栏时 `app.selection` 已 `clear()`（变异：不 clear ⇒ 红）
7. `all_commands_len_is_20`

---

## W4 — 回合中推送

### U7: 六处调用点 + `last_view` 同步 + `TurnCtx` 四形参 + 回合中推送
- **type**: impl
- **依赖**: U4、U5、U6
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`
- **工作量**: 15 分钟
- **验收方法**: `./scripts/cargo-build.sh check --all-targets` 通过
- **优先级**: 必测

变更点：
1. `run()`（`:383`）里、`last_injected_rev`（`:461`）旁声明四个局部量：
   ```rust
   let mut last_view = <首份 CodingView>;   // 循环前先造一份，或惰性初始化
   let mut last_panel: Option<PanelView> = None;
   let mut last_push = Instant::now() - PANEL_THROTTLE;   // 首次立即允许
   ```
   新增 `const PANEL_THROTTLE: Duration = Duration::from_millis(300);`
2. 六处 `build_view_with_loop`（`:548`/`:608`/`:646`/`:687`/`:713`/`:1486`）：
   - 补第 11 形参 `panel`（值取 `panel_view(&todo_store, subagent.as_ref())`）
   - **发出后同步写** `last_view = view.clone()`（或先写再发）——
     漏了会让回合中复用的是陈旧基线
3. `TurnCtx`（`:1656`）加四字段；`run_turn_chain`（`:1841`）加四形参；六个调用点
   （`:526`/`:586`/`:664`/`:822`/`:934`/`:970`）传 `&todo_store` 与四个 `&mut`
4. 事件转发臂（`:2203` 与 `:2288` 的 `send(ctx.out_tx, Outbound::Event(env))` 之后）加：
   ```rust
   let next = crate::view::panel_view(ctx.todo, ctx.subagent);
   if ctx.last_panel.as_ref() != Some(&next) && ctx.last_push.elapsed() >= PANEL_THROTTLE {
       ctx.last_view.panel = next.clone();
       *ctx.last_panel = Some(next);
       *ctx.last_push = Instant::now();
       send(ctx.out_tx, Outbound::View(ctx.last_view.clone())).await?;
   }
   ```
   **只在事件臂里做，不得改调 `build_view_with_loop`**（它要 `&Agent`，而回合执行期 `ctx.agent`
   已被 `&mut` 借走；且会读 `Instant`/`state_store`）

### T7: 回合中推送（G7–G9）
- **type**: test
- **依赖**: U7
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`（`mod tests` 追加）
- **工作量**: 15 分钟
- **验收方法**: `cargo test -p ys-coding-agent app_loop` — 通过
- **优先级**: 必测

断言（G7–G9）：
1. **G7 只推变了**：回合内 `PanelView` 不变 ⇒ 「有人打开侧栏」（用 `toggle`）不产生额外 `View`；
   todo `apply` 一次 ⇒ 恰 1 条额外 `View` 且其 `panel` 是新值
2. **G8 不重建整视图**：那条额外 `View` 的 `turn_count` / `executed` / `message_count` 与
   推送前那份**逐字相同**（变异：改调 `build_view_with_loop` ⇒ 红）
3. **G9 节流**：注入 `last_push` 使其未满 300ms ⇒ 不发；跨过 ⇒ 发（**不 sleep**，直接构造 `Instant`）

**变异**：
- 去掉 `next != last_panel` 比较 ⇒ G7 红
- 去掉 `elapsed()` 判据 ⇒ G9 红
- 推送时改调 `build_view_with_loop` ⇒ G8 红

---

## 验收总门槛

每个单元结束时：
1. `./scripts/cargo-build.sh check --all-targets` — **全量 workspace 绿**（不接受「本包能编就行」）
2. `cargo clippy --all-targets` — 无新增 warning
3. 该单元自己的测试文件绿

全部单元完成后：
- `./scripts/par-test.sh` — 全量绿（**唯一认可的全量测试入口**）
- `cargo fmt --check`

## 风险与已知取舍

| 项 | 说明 |
|---|---|
| `SIDE_WIDTH = 30` 是魔术数 | 80 列终端 Chat 从 80 → 50。需求已定「静态大小」，改常量即可调整 |
| 切换瞬间全量重折行 | 宽度变 ⇒ `layout.sync` 全清，500 KB~1 MB 会话估 5~30 ms。不可消除 |
| `command_in_flight` 被反复清 | `apply_outbound`（`app.rs:270`）收到任何出站消息都清。已核实无实际影响（读者只有 Esc 分支），属语义噪声 |
| `TodoStatus` 是 `#[non_exhaustive]` | 新增状态会**静默**显示为 Plain（不编译报错）。接受 |
