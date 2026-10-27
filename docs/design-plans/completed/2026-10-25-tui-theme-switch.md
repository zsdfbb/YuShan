# TUI 配色主题（5 套内置 + `/theme` 实时切换）设计计划

> 本文是**任务拆分的依据**：测试合同编号 C1–C12 在 `docs/exec-plans/completed/2026-10-25-tui-theme-switch.md`
> 里逐条落到任务上。设计取向已由用户签字（D1–D5），实施阶段不再重开方案选择。

## 背景

当前 TUI 的配色散落在两处，且全部是**具名 16 色**（`Color::Red` / `Color::Cyan` / `Color::DarkGray` …）：

| 位置 | 内容 | 规模 |
|---|---|---|
| `crates/ys-tui-coding/src/styles.rs` | markdown 元素 → `Style` 映射（`impl tui_markdown::StyleSheet`） | 覆写 23 个方法，`AssistantStyles` 是零大小类型（`styles.rs:23-24`） |
| `crates/ys-tui-coding/src/draw.rs` | chrome（状态行 / 选择器 / 用户前缀 / ✓✗ / 行动块 / 错误前缀 / 系统行 / 空态占位） | 13 处 `Color::*` 字面量 |

具名 16 色的**明暗由终端配色方案决定**，本仓无法调整。后果：

1. 无法给出一套「背景 + 前景 + 强调」自洽的整体观感 —— 背景永远取决于用户的终端。
2. 语义槽位（成功 / 警告 / 失败）在不同终端上对比度不可控。
3. `md.rs:560-586` 的 `default_signal()` 靠「出现 `Blue` / `Green` / `LightCyan` / `LightYellow` / `LightBlue` / 白字黑底 / `DIM`」判定「`StyleSheet` 漏覆写、回落库默认配色」。改用 RGB 后该判据**必然失效**，必须换成「产出色是否 ∈ 本主题调色板」。

`styles.rs` 模块头（`:6-7`）当前写着「`StyleSheet` 是编译期 trait、不是主题系统 —— 无配置、无持久化、无 `Palette` 结构，与『颜色只在渲染层决定』『不引 Theme』两条约束相容」。本次**推翻**这条约束（D1），该段注释须改写为当前设计的描述。

### 与本设计相关的既有结构（读码核实）

| 符号 | 现状 | 位置 |
|---|---|---|
| `pub(crate) struct AssistantStyles;` | 零大小类型，`#[derive(Clone, Copy, Default)]` | `styles.rs:23-24` |
| `StyleSheet` 覆写 | 23 个方法；`code()` 有意用 cyan；`AlertKind::Caution => Color::Red` | `styles.rs:26-...`；`:121` |
| `draw.rs` 的 13 处 `Color::*` | 223 空态占位 `DarkGray`、267 状态行 `DarkGray`、396/428/450 选择器占位/无匹配/计数 `DarkGray`、401 查询行 `Cyan`、597 `build_text` 空态 `DarkGray`、798 用户前缀 `Green`、821 思考 `DarkGray`、846 ✓ `Green`、848 ✗ `Red`、853 行动块 `DarkGray`、899 工具失败首行 `Red`、918 `Error:` 前缀 `Red`、927 `System` `DarkGray` | `draw.rs` |
| `pub fn ui(frame: &mut Frame, app: &mut App)` | `:106`；`:106-110` 有 `area.width < MIN_WIDTH \|\| area.height < MIN_HEIGHT` 的 early return；`:117` 调 `app.sync_scroll_state(..)`；`:779` 造 `AssistantStyles` 传给 `cache.lines_for` | `draw.rs` |
| `RenderCache::Slot { len, width, lines, rendered_at, forced }` | 键 = `(len, width)`，**不含主题** | `cache.rs:41-55` |
| `RenderCache::reset()` | `pub fn` | `cache.rs:93` |
| `EntryMemo { sig, render_sig, rev, lines, row_starts }` | `entry_sig`（`layout.rs:532`）只哈希条目文本，**不含主题** | `layout.rs:52-60` / `:532` |
| `TranscriptLayout::sync(..)` | 签名见下；内部有「宽度变 / thinking 翻转 ⇒ 全量作废」分支 | `layout.rs:149`；`:158-166` |
| `App::sync_scroll_state(width, height, now)` | 内部调 `layout.sync(...)` | `app.rs:504` |
| `App::reset_for_replay()` | `/resume` 清屏，内含 `md_cache.borrow_mut().reset(); layout.borrow_mut().reset();` | `app.rs:231` |
| `CodingView.thinking_visible: bool` | 注释写明「只在启动时被 `App::new` 消费一次；运行期真值是 `App.thinking_visible`」 | `crates/ys-tui-coding/src/view.rs:120-123` |
| `App::new` | `let thinking_visible = view.thinking_visible;` 消费一次 | `app.rs:139` |
| `CodingView::for_test()` | 字段全量构造 | `crates/ys-tui-coding/src/view.rs:188` 起，`:239` 是 `thinking_visible` 一行 |
| `static COMMANDS: [CommandSpec; 14]` | `CommandSpec { name, description, arg_hint }` | `crates/ys-tui-coding/src/commands.rs:52` |
| `enum Action { None, Request(Request), Abort, Quit, Local(LocalAction), Prompt(PromptKind) }` | — | `commands.rs` |
| `enum LocalAction { Help, Status, Copy, Thinking(Option<bool>), Effort, Hint(String) }` | — | `commands.rs` |
| `enum PromptKind { Login, PickModel, PickSkill, PickSession }` | — | `commands.rs` |
| `pub fn apply_local(app: &mut App, action: LocalAction)` | 把文本写进 transcript | `commands.rs:517` |
| `/thinking` 的处理形态 | `let is_thinking = matches!(local, LocalAction::Thinking(_)); apply_local(app, local); if is_thinking { send_request(app, requests, Request::SetThinking { visible: app.thinking_visible }); }` | `crates/ys-tui-coding/src/events.rs:471-486` |
| `trait Prompter { fn select(&self, ..); fn text(&self, ..) }` | `&self`、**无回调**、只能回传选中项的显示串 | `crates/ys-tui-coding/src/prompter.rs:60-71` |
| `enum PromptOutcome { Request(Request), Prefill(String), NoOp(String) }` | — | `prompter.rs:79-89` |
| `pub fn resolve_prompt(kind, prompter, view)` | `match kind` 四臂；`PickSession` 返回 `Err(PromptError::Other(..))` | `prompter.rs:99` |
| `fn select_modal(&mut self, prompt, options, page_size)` | 真终端模态循环，每帧 `draw::ui_with_modal`，逐键 Up/Down/Enter/Esc/Ctrl-C/Backspace/Delete/Left/Right/Home/End/Char/Paste | `crates/ys-tui-coding/src/run.rs:414` |
| `fn run_prompt(...)` | 调 `resolve_prompt` / `resolve_session_pick`，再 `apply_prompt_outcome` | `run.rs:249` |
| `md::render(src, width, styles)` | 收 `AssistantStyles` 为值 | `crates/ys-tui-coding/src/md.rs:38` |
| `fn default_signal(style) -> Option<&'static str>` | 靠具名色判定「漏覆写」 | `md.rs:560-586` |
| `enum Request` | 已有 `SetThinking { visible: bool }`（`:37`）；带 `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]` | `crates/ys-protocol/src/request.rs:14-71` |
| `AppState { last_active_provider, last_active_model }` | `#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]`；**字段没有 `#[serde(default)]`** | `apps/coding-agent/src/state.rs:13-17` |
| `StateStore::load()` | 读 → `serde_json::from_str`；失败则 log + `AppState::default()`（**这条路径会丢掉 `last_active_model`**） | `state.rs:30` |
| `StateStore::save()` | 落盘 `~/.yushan/state.json`，设 0o600 | `state.rs:44`；路径 `:26` |
| `state.save(&AppState { .. })` 字面量构造 | 三处：`:157`、`:186`（`/logout` 里 `state.save(&AppState::default())`）、`:253`；测试里 `:1189` | `apps/coding-agent/src/capabilities.rs` |
| `build_view(cfg, agent, wiring, registry, state, stats, session_started) -> CodingView` | 已持有 `state: &StateStore`，但**只调用 `state.exists()`（`:134`），不调用 `state.load()`**；且有 10+ 调用点、在 app_loop 每请求刷一次 | `apps/coding-agent/src/view.rs:26`；`:136`；`app_loop.rs:749` |
| 进程级 thinking 镜像 | `let mut thinking_visible = wiring.meta()...;`；`Request::SetThinking { visible }` 臂在 `:485` | `apps/coding-agent/src/app_loop.rs:333` / `:485` |
| `app_loop.rs` 模块文档伪码 | 含 `SetThinking{visible} => { ... }` | `app_loop.rs:29` |
| 启动期已 load 一次 | `let state_store = state::StateStore::new(); let saved_state = state_store.load();` | `apps/coding-agent/src/main.rs:2657-2658` |
| `thinking_visible` 的持久化载体 | session meta（`pub thinking_visible: bool`） | `crates/ys-session/src/meta.rs:45` |
| `CodingView` 所在 crate 是否被 app 依赖 | **是** —— `ys-tui-coding = { path = "../../crates/ys-tui-coding" }` | `apps/coding-agent/Cargo.toml:21` |
| TUI crate 是否做文件 IO | **否** —— `crates/ys-tui-coding/src/` 下**无任何** `std::fs` / `File::` 使用 | — |

## 设计

### 架构决策与理由

#### D1 反转「不引 Theme / 不建 Palette」，范围硬限定

新建 `docs/adr/0053-tui-color-themes.md` 记录对 ADR-0030「不做 Theme/Palette」的反转。范围**只有**：

- 「编译期常量色板表 + 5 套内置主题」。
- **不做**用户自定义主题文件。
- **不做**运行时加载的主题格式（不引 serde 配置驱动的主题）。

理由：`ys-tui-coding` 当前**零文件 IO**（crate 内无 `std::fs` / `File::`）。主题若可配置，主题文件读取就要落在 UI crate，破坏这条边界；把读取放 app 侧又会在每次 `View` 快照里多传一份不受协议约束的数据。**5 套常量表**把主题彻底留在 UI crate 内部，代价是用户不能自定义 —— 这是本设计明确接受的取舍。

`styles.rs` 模块头 `:6-7` 的「无 `Palette` 结构」「不引 Theme」两句必须改写为**当前**设计的描述（`Palette` 存在、颜色由 `ThemeKind` 决定）。按项目约定只写现状，不写「曾经如何」。

#### D2 每帧铺主题背景色

`draw::ui` 在 `area.width < MIN_WIDTH || area.height < MIN_HEIGHT` 时直接 `return`（`draw.rs:106-110`）。铺背景**必须**在该 early-return **之后**、pane 绘制**之前**：

```rust
// early return 之后
frame.render_widget(
    Block::default().style(Style::default().bg(palette.bg)),
    frame.area(),
);
```

理由：具名 16 色下背景由终端决定，换主题只有前景变、背景不变，观感上「没换」。整屏铺 `bg` 让主题切换视觉彻底。

**浮层与 `Clear` 的坑**：`Clear` 把区域重置为**终端默认底色**，不是主题 `bg`。因此输入框 / 选择器浮层凡调用 `Clear` 的区域，必须在 `Clear` **之后**重新铺一次 `bg`，否则浮层区域会出现一个与主题背景不一致的黑洞（或亮洞）。这条对所有 `Clear` 调用点都是硬约束。

#### D3 无参 `/theme` 开浮层并**实时预览**

语义：

| 按键 | 行为 |
|---|---|
| `↑` / `↓` | 移动选中项，**立即应用**该主题（重渲染可见） |
| `Esc` | 还原到打开前的主题，**不落盘**，关闭浮层 |
| `Enter` | 确认当前选中项，**落盘**（发 `Request::SetTheme`），关闭浮层 |

实现**只能**在 `run.rs`：`Prompter::select` 是 `&self`，且**无回调**，只能回传选中项的显示串 —— 感知不到逐键移动。故新增一条专门的模态路径（不复用 `Prompter::select`）。

为让 C11 可自动化，把「按键 → 预览状态转移」抽成**纯函数**（如 `fn theme_modal_step(state: ThemeModalState, key: ThemeKey) -> ThemeModalStep`），模态宿主只负责「把 `ThemeModalStep` 的实际效果施加到 `app` 上」。纯函数可单测，宿主层用可记录的 applier 回调（或 `FakePrompter`-风格替身）验证调用序列。

#### D4 5 套配色与 10 槽位语义

`/theme` 的合法取值即 kebab-case 名（大小写不敏感）：

| 主题 | bg | fg | accent | code | muted | user | ok | warn | fail | info |
|---|---|---|---|---|---|---|---|---|---|---|
| `ayu-mirage` | #1f2430 | #cccac2 | #ffcc66 | #5ccfe6 | #6c7986 | #73b8ff | #bae67e | #ffd173 | #f28779 | #95e6cb |
| `gruvbox-dark` | #1d2021 | #ebdbb2 | #fabd2f | #8ec07c | #928374 | #83a598 | #b8bb26 | #fe8019 | #fb4934 | #83a598 |
| `nord` | #2e3440 | #d8dee9 | #88c0d0 | #8fbcbb | #4c566a | #81a1c1 | #a3be8c | #ebcb8b | #bf616a | #5e81ac |
| `everforest-dark` | #2d353b | #d3c6aa | #a7c080 | #83c092 | #859289 | #7fbbb3 | #dbbc7f | #e69875 | #e67e80 | #83c092 |
| `solarized-dark` | #002b36 | #93a1a1 | #b58900 | #2aa198 | #586e75 | #268bd2 | #859900 | #cb4b16 | #dc322f | #2aa198 |

槽位语义（10 个，**唯一**的语义到颜色的映射来源）：

| 槽位 | 用途 |
|---|---|
| `bg` | 画布背景（D2） |
| `fg` | 正文 |
| `accent` | 标题 / 数学 |
| `code` | 行内码 / 围栏代码块 |
| `muted` | 引用 / 系统 / 状态行 / 行动块 / 表格框线 / 列表标记 / 脚注 / HTML / 图片占位 |
| `user` | 用户消息前缀 `> ` |
| `ok` | 工具 ✓ |
| `warn` | alert `Warning` + `Important` |
| `fail` | 工具 ✗ / `Error:` 前缀 / alert `Caution` |
| `info` | alert `Note` + `Tip` |

**已知合法撞色**（不在紫色守卫范围内，允许）：`gruvbox-dark` 的 `user` 与 `info` 同色；`everforest-dark` 与 `solarized-dark` 的 `code` 与 `info` 同色。原因是这几套调色板的非紫色相不足，硬要拉开会破坏原本的配色语言。

**硬性需求：五套调色板的全部非 bg 槽位（9 个 × 5 套 = 45 个色）都不得落在紫色色相区** —— HSL 色相 255°–330°，且饱和度 ≥ 0.20、亮度 0.15–0.90。由 `theme.rs` 单测守卫（C2）。

#### D5 持久化到 `~/.yushan/state.json`（全局偏好）

主题是**全局偏好**，不随会话：跨会话保留，`/new` 不重置，`/resume` 换会话时主题不变。故**不进** session meta（`crates/ys-session/src/meta.rs:45` 的 `thinking_visible` 是会话级，属另一类）。

`AppState` 新字段写成 `Option<String>` 并显式带 `#[serde(default)]`（与 `SessionMeta` 的 `title` / `provider` 等可选字段同风格）。

**一处需要写清的事实**：`Option<T>` 字段在 serde 里缺键本就解成 `None`，故 `#[serde(default)]` 对它是**冗余的**；它在此处是风格一致性与意图声明，不是正确性开关。真正会让旧文件读不出来的是把字段写成**非 `Option` 且无默认值**的形态（那时缺键 ⇒ `from_str` 报 missing field ⇒ `load()` 回落 `AppState::default()` ⇒ `last_active_model` 一起丢）。

因此 C9 验证的是**行为**（旧文件仍可解析、不丢老字段），它的变异点不是「删 `#[serde(default)]`」（删了用例仍绿，已实测），而是「把字段改成非 `Option` 且无默认值」。

另：`capabilities.rs` 三处 `AppState { .. }` 字面量构造（`:157` / `:186` / `:253`）必须改成**保留已有字段**的形态（先 `load()` 再改自己那两个字段），否则保存时会清掉 `theme`。`:186` 的 `/logout` 尤其危险（当前是 `AppState::default()`）。

### 模块落点

新增 **1 个文件** `crates/ys-tui-coding/src/theme.rs`。

```rust
// crates/ys-tui-coding/src/theme.rs

/// 内置主题。`/theme` 的合法取值即 `name()`（kebab-case，大小写不敏感）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemeKind {
    #[default]
    AyuMirage,
    GruvboxDark,
    Nord,
    EverforestDark,
    SolarizedDark,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 5] = [ /* 顺序即浮层展示顺序 */ ];
    pub fn name(&self) -> &'static str;          // "ayu-mirage" / "gruvbox-dark" / ...
    pub fn label(&self) -> &'static str;         // "Ayu Mirage" / "Gruvbox Dark" / ...
    pub fn next(&self) -> ThemeKind;             // 环形，浮层 ↑↓ 用
    pub fn prev(&self) -> ThemeKind;
    pub fn palette(&self) -> Palette;            // 按值返回（Palette: Copy，40 字节）
}

impl FromStr for ThemeKind {
    type Err = ThemeParseError;                  // 未知名带原文回显
    // 大小写不敏感；'_' 归一化为 '-'
}

/// 10 槽位色板。全部 `Color::Rgb`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color, pub fg: Color, pub accent: Color, pub code: Color, pub muted: Color,
    pub user: Color, pub ok: Color, pub warn: Color, pub fail: Color, pub info: Color,
}

const fn rgb(hex: u32) -> Color { Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8) }

/// 紫色守卫：`Color::Rgb` → HSL，判色相 ∈ [255, 330] 且 sat ≥ 0.20 且 light ∈ [0.15, 0.90]。
/// 非 `Rgb` 变体返回 false（本设计不产生非 RGB 色）。
pub(crate) fn is_purple(c: Color) -> bool;
```

设计要点：

- `Palette` 用 `const fn rgb()` 构造，整套表是 `static` 常量 ⇒ 零运行时分配，`palette()` **按值**返回 `Palette`（`Copy`，10 个 `Color::Rgb` = 40 字节）。
  按值而非 `&'static` 的理由：① 引用约束 `'static`，会把「运行时来源的色板」永久排除在外；② 按值可自由持有 / 比较，缓存与布局的「主题是否变了」可直接用 `Palette` 的 `PartialEq` 比值，不必维护版本计数器。
- `Default for ThemeKind` = `AyuMirage`（`#[default]`）。
- `is_purple` 只判 `Color::Rgb`；HSL 转换用最小实现（sRGB → HSL，纯浮点，无依赖）。
- `ThemeKind` 与 `Palette` 都是 `Copy`（`Palette` 10 个 `Color` 全是 `Copy` 枚举），满足 `StyleSheet: Clone + Send + Sync + 'static`。

### 渲染层接线

#### `styles.rs`：`AssistantStyles` 携带调色板

从零大小类型变为携带主题：

```rust
#[derive(Clone, Copy)]
pub(crate) struct AssistantStyles {
    palette: Palette,
}
impl AssistantStyles {
    pub(crate) fn new(kind: ThemeKind) -> Self { Self { palette: kind.palette() } }
}
```

仍保持 `Clone + Copy + Send + Sync + 'static`（`Palette` 是 `Copy`，自动满足）。`tui_markdown::StyleSheet` 要求的那组 bound 不变，故 `md::render(src, width, styles)` 的签名**不动**，只是 `AssistantStyles` 不再是 unit struct —— 所有 `crate::styles::AssistantStyles` 的构造点改为 `AssistantStyles::new(kind)`（`draw.rs:779` 一处）。

23 个方法的映射（语义见 D4 槽位表；`AlertKind` 四值 → `warn` / `fail` / `info`）：

| 方法 | 槽位 |
|---|---|
| `heading` | `accent` + `BOLD`（h1 另加 `UNDERLINED`） |
| `code` / `code_block` | `code` |
| `emphasis` / `strong` / `strikethrough` | `fg`（保留 modifier） |
| `inline_math` / `math` | `accent` |
| `block_quote` / `table_borders` / `list_marker` / `footnote_marker` / `html` / `image` | `muted` |
| `link` / `link_text` | `info` |
| `alert` 四值 | `Warning`+`Important` → `warn`；`Caution` → `fail`；`Note`+`Tip` → `info` |

`AlertKind::Caution => Color::Red`（`styles.rs:121`）改为 `self.palette.fail`。

#### `draw.rs`：13 处 chrome 色 + 背景铺色 + 浮层取色

统一入口：`fn chrome(app: &App) -> Palette { app.theme.palette() }`（按值，`Copy` 无开销）。13 处映射：

| 行 | 现状 | 槽位 |
|---|---|---|
| 223 | 空态占位 `DarkGray` | `muted` |
| 267 | 状态行 `DarkGray` | `muted` |
| 396 / 428 / 450 | 选择器占位 / 无匹配 / 计数 `DarkGray` | `muted` |
| 401 | 选择器查询行 `Cyan` | `accent` |
| 597 | `build_text` 空态 `DarkGray` | `muted` |
| 798 | 用户前缀 `Green` | `user` |
| 821 | 思考 `DarkGray` | `muted` |
| 846 | ✓ `Green` | `ok` |
| 848 | ✗ `Red` | `fail` |
| 853 | 行动块 `DarkGray` | `muted` |
| 899 | 工具失败结果首行 `Red` | `fail` |
| 918 | `Error:` 前缀 `Red` | `fail` |
| 927 | `System` `DarkGray` | `muted` |

背景铺色在 `draw.rs:106-110` 的 early-return **之后**（见 D2）。`ui_with_modal` 的浮层同样从 `app.theme.palette()` 取色，且 `Clear` 之后补铺 `bg`。

#### 缓存与布局失效（C7 的落点）

**这是本需求最容易出错的地方**：`RenderCache` 的键是 `(len, width)`（`cache.rs:41-55`），`EntryMemo` 的 `sig` 只哈希条目文本（`layout.rs:532`）—— 两者都**不含主题**。若不显式作废，切主题后：

- `md_cache` 命中旧槽位 ⇒ markdown 保持旧配色；
- `layout` 的 `EntryMemo.lines` 是**已渲染**的 `Vec<Line<'static>>`（颜色已烘进去）⇒ 正文保持旧配色。

处置：

失效判据**统一按色板值比较**，不引版本计数器 —— `Palette` 是 `Copy + PartialEq` 的 40 字节值，
比值即得「是否换了主题」，不会出现「计数器漏加 / 加两次」这类与渲染无关的失效 bug。

1. `RenderCache` 增加 `fn set_theme(&mut self, palette: Palette)`：内部记住该色板并**清空 slots**
   （复用既有 `reset()` 的清空逻辑）。语义上「主题变了 ⇒ 所有渲染产物作废」。
   清空而非逐槽比对：槽位里存的是已渲染的 `Vec<Line>`，逐槽记色板会多一份每槽状态，
   而切主题是低频操作、全量作废的代价可接受。
2. `TranscriptLayout::sync` 入参增加 `theme: Palette`，在既有全量作废分支上加一个判据：

```rust
// layout.rs:158-166 现有形态
if width_changed || thinking_visible != self.thinking_visible { entries.clear(); ... structural_rev += 1; }
// 变为
if width_changed || thinking_visible != self.thinking_visible || theme != self.theme {
    entries.clear(); gaps.clear(); starts.clear(); ... structural_rev += 1;
}
```

`TranscriptLayout` 增私有字段 `theme: Palette`，在 `reset()` 里同步更新。

3. `App` 增字段 `theme: ThemeKind`（运行期真值），与 `thinking_visible` **对称**：`App::new` 从 `view.theme` 消费一次，运行期**不被** `Outbound::View` 快照覆盖。

4. `App::set_theme(&mut self, kind: ThemeKind)` —— **唯一**的失效入口：

```rust
pub(crate) fn set_theme(&mut self, kind: ThemeKind) {
    self.theme = kind;
    self.md_cache.borrow_mut().set_theme(kind);
    self.layout.borrow_mut().reset();
}
```

**禁止**在多个调用点各写一遍失效逻辑（浮层预览、有参 `/theme`、`App::new` 之后的首次同步都走它或与其等价）。理由：漏一处就表现为「某条路径切换后部分内容还是旧色」，且这类 bug 在视觉上偶发（只在缓存恰好命中时出现）。

### 命令与协议

#### `commands.rs`

- `static COMMANDS: [CommandSpec; 14]` → `[CommandSpec; 15]`，新增：
  ```rust
  CommandSpec { name: "theme", description: "切换配色主题", arg_hint: Some("[name]") }
  ```
- `LocalAction` 加 `Theme(Option<String>)`。
- `PromptKind` 加 `PickTheme`。
- 解析三态：
  - **无参** → `Action::Prompt(PromptKind::PickTheme)`（开浮层，D3）。
  - **有参合法** → `Action::Local(LocalAction::Theme(Some(parsed_name)))`；`apply_local` 里本地立即生效，`events.rs` 再单向同步（见下）。
  - **有参非法** → `LocalAction::Hint(<可用主题列表>)`，不改状态。

  注意 `LocalAction` 携带的是**归一化后的 kebab-case 名**（`String`），不是 `ThemeKind` —— `LocalAction` 是 `commands.rs` 的公开枚举，携 `String` 使「非法输入」可以在进入本地应用前被拦下（解析在 `commands::parse` 里做）。

#### `prompter.rs`

`resolve_prompt` 的 `PickTheme` 臂返回：

```rust
PromptKind::PickTheme => Err(PromptError::Other("PickTheme 须经 run.rs 专门模态处理".into())),
```

与既有 `PromptKind::PickSession` 的处理形态一致（`prompter.rs` 末尾）。理由：`Prompter::select` 是 `&self` 且无回调，**结构上**无法做实时预览（D3）。

#### `run.rs`

新增 `apply_theme_modal(...)`（仿 `select_modal`：`run.rs:414`）：每帧 `terminal.draw(|f| draw::ui_with_modal(f, app, draw::Modal::Select { .. }))`，逐键处理。差别：

- `↑` / `↓` 移动后**立即** `app.set_theme(candidate)`（预览）。
- `Esc` → `app.set_theme(opened_with)` 并返回 `None`（不落盘）。
- `Enter` → 返回 `Some(kind)`，由调用方发 `Request::SetTheme`。
- `Ctrl-C` → 同 `Esc` 的还原语义（不落盘）。

**可测性抽取**：把「按键 → 预览状态转移」抽成纯函数：

```rust
pub(crate) struct ThemeModalState { opened_with: ThemeKind, selected: ThemeKind }
pub(crate) enum ThemeKey { Up, Down, Enter, Esc, Other }
pub(crate) enum ThemeModalStep { Preview(ThemeKind), Restore(ThemeKind), Confirm(ThemeKind), Ignore }

pub(crate) fn theme_modal_step(state: ThemeModalState, key: ThemeKey) -> ThemeModalStep;
```

模态宿主只负责把 `ThemeModalStep` 施加到 `app` / 返回值上。C11 直接对 `theme_modal_step` 做穷举单测，宿主层用可记录的 applier 验证调用序列。

`run_prompt`（`run.rs:249`）增加分支：`PromptKind::PickTheme` 走 `apply_theme_modal`（在 `resolve_prompt` 之前分流）。

#### `events.rs`

`/theme <name>` 的单向同步照 `/thinking` 的形态（`events.rs:471-486`）：

```rust
let theme_arg = match &local { LocalAction::Theme(Some(n)) => Some(n.clone()), _ => None };
apply_local(app, local);                       // 本地立即生效（内部走 app.set_theme）
if let Some(name) = theme_arg {
    send_request(app, requests, Request::SetTheme { theme: name });   // 单向同步给 app 持久化
}
```

浮层确认路径在 `run.rs` 里发 `Request::SetTheme`（不走 `apply_local`，因为预览已经把状态设好了）。

#### `ys-protocol/src/request.rs`

```rust
SetTheme { theme: String },
```

保持 `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]` 与既有变体同一风格。**不用** `ThemeKind` 枚举的理由是**跨 crate 边界**：

- `Request` 是**跨动态库边界**传递的纯数据（AGENTS.md 硬性约束：不跨动态库边界传 Rust trait object / 跨库所有权对象）；`ThemeKind` 定义在 `ys-tui-coding`，`ys-protocol` **不认识** UI crate，也不该认识。
- 协议只承载 **kebab-case 名**（`String`），是这一层的最小足够表示。
- `apps/coding-agent` 收到后**不解析**，只把字符串写进 `state.json`；解析回 `ThemeKind` 发生在 `build_view` 时由 `ys-tui-coding` 的 `FromStr` 做（`ThemeKind` 只有它的 owner 能解释）。

这同时保证了：协议层不随主题集合增减而变（新增主题不需要改协议）。

#### `apps/coding-agent` 侧

| 文件 | 改动 |
|---|---|
| `state.rs` | `AppState` 加 `#[serde(default)] pub theme: Option<String>` |
| `capabilities.rs` / `app_loop.rs` | 新增 `Request::SetTheme { theme }` 臂：读现有 `AppState` → 只改 `theme` 字段 → `save`；产出 `Outbound::Output`（如 `theme: nord`）与 `Outbound::View` |
| `capabilities.rs` 三处 `AppState { .. }` 字面量 | 改为「先 `load()` 再改自己那两个字段」，保留 `theme` |
| `view.rs` | `CodingView` 加 `pub theme: ThemeKind`；`build_view` 里取值 |
| `view.rs` 的 `for_test()` | 补 `theme: ThemeKind::default()` |
| `app_loop.rs` 模块文档伪码（`:29`） | 加 `SetTheme{theme} => { ... }` |

**`build_view` 的取值方式**：`build_view` 已持有 `state: &StateStore`，但当前**只调 `state.exists()`（`view.rs:134`），不调 `state.load()`**。三种做法：

- 在 `build_view` 里 `state.load().theme` —— 每次调用多一次读盘。
- 给 `build_view` 加参数传入已加载的 state —— 波及 10+ 调用点与测试替身。
- **进程级镜像**（选定）：`app_loop` 主循环持有 `let mut theme: ThemeKind`，启动时由 `state.load()` 初始化，`Request::SetTheme` 单向更新它并落盘；`build_view` 从镜像取值。

**选择进程级镜像**。决定性理由是实测频率：`build_view` 在 `app_loop.rs:749` 是「**每个请求处理完**刷新快照」时调用（不是启动一次），再加一次读盘等于每轮对话读一次 `state.json`。而且项目已有同一形态的先例 —— `thinking_visible`（`app_loop.rs:333`）就是进程级镜像、由 `SetThinking` 单向更新，`out_tx` 每请求刷一次 `View` 也不读盘。主题与它同类（会话级偏好、UI 本地即时生效、app 侧只作持久化镜像），照抄即可。

代价：`build_view` 的签名要加一个 `theme: ThemeKind` 参数（10+ 调用点）。这与「`thinking_visible` 不在 `build_view` 签名里」不同 —— 后者在 app 侧是 `let mut thinking_visible`，只在 `run_turn` 路径用、不需要进视图。主题**必须**进视图（UI 要它），故只能走参数或读盘；参数是正确选择。

**`CodingView.theme` 的类型**：`apps/coding-agent/Cargo.toml:21` 依赖 `ys-tui-coding`，且 `CodingView` **本身定义在 `ys-tui-coding`**（`crates/ys-tui-coding/src/view.rs:82`）。故字段类型**直接**是 `ThemeKind`，不存在「app 侧拿不到类型」的问题。（若写成 `String` 反而要在 UI 侧每次解析，属于无谓的往返。）此结论已核实：依赖成立。

### 数据流

#### 有参 `/theme nord`

```
用户输入 "/theme nord"
  → commands::parse → Action::Local(LocalAction::Theme(Some("nord")))
  → events::handle_event
      apply_local(app, local)              // 内部 app.set_theme(Nord)
        ├─ app.theme = Nord
        ├─ md_cache.set_theme(Nord)        // 清空 slots
        └─ layout.reset()                  // 清空 entries/gaps/starts
      send_request(Request::SetTheme { theme: "nord" })
  → app_loop 收到 Request::SetTheme
      state.load() → 改 theme → state.save()
      Outbound::Output("theme: nord") + Outbound::View(..)
  → 下一帧 draw::ui
      铺 bg(Nord)；layout.sync(.., theme=Nord, ..) 走全量作废；
      cache.lines_for(.., AssistantStyles::new(Nord)) 重新渲染
```

#### 无参 `/theme`（浮层预览）

```
用户输入 "/theme"
  → commands::parse → Action::Prompt(PromptKind::PickTheme)
  → run_prompt 分流到 apply_theme_modal
      ThemeModalState { opened_with: <当前>, selected: <当前> }
      每帧 draw::ui_with_modal(..)
      ↑/↓ → theme_modal_step → Preview(kind) → app.set_theme(kind)   // 立即看见
      Esc → theme_modal_step → Restore(opened_with) → app.set_theme(opened_with)；返回 None（不落盘）
      Enter → Confirm(selected) → 返回 Some(selected) → send_request(Request::SetTheme { .. })
```

### 接口变更汇总

| 签名 | 变更 | 兼容性 |
|---|---|---|
| `md::render(src, width, styles: AssistantStyles)` | **不变**（`AssistantStyles` 内部变了） | 源兼容 |
| `AssistantStyles` | unit struct → `{ palette: Palette }` + `::new(kind)` | 所有构造点须改（`draw.rs:779`） |
| `RenderCache::set_theme(&mut self, kind)` | 新增 | 纯追加 |
| `TranscriptLayout::sync(..)` | 入参加 `theme: Palette` | 调用点 `app.rs:504` 内一处（传 `app.theme.palette()`） |
| `App.theme` / `App::set_theme` | 新增 | 纯追加 |
| `COMMANDS` | `14` → `15` | 长度断言处须改 |
| `LocalAction::Theme(Option<String>)` | 新增变体 | `match` 穷举处须补臂 |
| `PromptKind::PickTheme` | 新增变体 | `match` 穷举处须补臂（`resolve_prompt`、`run_prompt`、补全） |
| `ys_protocol::Request::SetTheme { theme: String }` | 新增变体 | app 侧 `match` 须补臂 |
| `AppState.theme: Option<String>` | 新增字段 + `#[serde(default)]` | 旧文件可解析（C9） |
| `CodingView.theme: ThemeKind` | 新增字段 | 所有构造点须补（含 `for_test()`） |

## 涉及文件

### 新增

| 路径 | 内容 |
|---|---|
| `crates/ys-tui-coding/src/theme.rs` | `ThemeKind`（`ALL` / `name` / `label` / `next` / `prev` / `palette` / `FromStr` / `Default`）、`Palette`（10 槽位）、`const fn rgb`、`is_purple`、5 套常量表、单测 |
| `docs/adr/0053-tui-color-themes.md` | ADR：反转 ADR-0030 的「不做 Theme/Palette」，范围限定为编译期常量色板 |

### 修改

| 路径 | 内容 |
|---|---|
| `crates/ys-tui-coding/src/lib.rs` | `mod theme;`（字母序插在 `styles` 与 `transcript` 之间）；模块头补主题相关一句；`pub use theme::ThemeKind;` |
| `crates/ys-tui-coding/src/styles.rs` | `AssistantStyles` 变携带 `Palette`（按值）；23 方法改读 `self.palette`；`Caution` → `fail`；模块头 `:6-7` 改写为当前设计 |
| `crates/ys-tui-coding/src/md.rs` | `default_signal` 换成「产出色 ∈ 本主题调色板」；相关硬断言具名色的用例改写（行 186, 211, 234, 236, 245, 262, 313, 414, 420, 426, 442, 450, 458, 466, 471, 477, 482, 487, 505-509, 537, 542, 547, 572-580） |
| `crates/ys-tui-coding/src/draw.rs` | 13 处 `Color::*` → 槽位色；`chrome(app)` 帮助函数；`ui` 里 early-return 之后铺 `bg`；`ui_with_modal` 浮层取色 + `Clear` 后补铺 `bg`；`:779` 的 `AssistantStyles::new(app.theme)` |
| `crates/ys-tui-coding/src/cache.rs` | 新增 `set_theme(&mut self, palette)`：比色板值 + 清空 slots（无版本计数器） |
| `crates/ys-tui-coding/src/layout.rs` | `TranscriptLayout` 加 `theme` 字段；`sync` 入参加 `theme`；全量作废分支加 `theme != self.theme`；`reset()` 同步更新 |
| `crates/ys-tui-coding/src/app.rs` | `App.theme` 字段（`App::new` 从 `view.theme` 消费一次）；`App::set_theme`；`sync_scroll_state` 把 `theme` 传给 `layout.sync` |
| `crates/ys-tui-coding/src/commands.rs` | `COMMANDS` → `[CommandSpec; 15]`；`local` 项；`LocalAction::Theme(Option<String>)`；`PromptKind::PickTheme`；`parse` 三态；`apply_local` 的 `Theme(Some)` 臂调 `app.set_theme` |
| `crates/ys-tui-coding/src/prompter.rs` | `resolve_prompt` 补 `PickTheme` 臂（返回 `Err`，与 `PickSession` 同形态） |
| `crates/ys-tui-coding/src/run.rs` | `apply_theme_modal`；`theme_modal_step` 纯函数 + `ThemeModalState` / `ThemeKey` / `ThemeModalStep`；`run_prompt` 分流 |
| `crates/ys-tui-coding/src/events.rs` | `/theme <name>` 的「本地生效 + 单向同步」（照 `/thinking` 形态，`:471-486`） |
| `crates/ys-tui-coding/src/completion.rs` | 若有硬编码命令条数则改为自适应（已核实 `all_commands().len()` 自适应，可能无需改） |
| `crates/ys-tui-coding/src/view.rs` | `CodingView` 加 `pub theme: ThemeKind`；`for_test()` 补默认值 |
| `crates/ys-protocol/src/request.rs` | `Request::SetTheme { theme: String }` |
| `apps/coding-agent/src/state.rs` | `AppState` 加 `#[serde(default)] pub theme: Option<String>` |
| `apps/coding-agent/src/view.rs` | `build_view` 加 `theme: ThemeKind` 参数（不读盘）；`CodingView { theme, .. }` |
| `apps/coding-agent/src/app_loop.rs` | `Request::SetTheme` 臂 + 模块文档伪码（`:29`） |
| `apps/coding-agent/src/capabilities.rs` | 三处 `AppState { .. }` 字面量（`:157` / `:186` / `:253`）改为保留已有字段；测试 `:1189` 同步 |
| `docs/design-final/coding-agent-tui.md` | 配色段回写（`:277` 的 ✓/✗ 语义、`:781-782` 的 styles.rs 23 方法）+ 新增主题 as-built 节 |

### 删除

无。

## 测试策略

### 测试合同 C1–C12

| # | 合同 | 验收方法 | 优先级 | 验收人 |
|---|---|---|---|---|
| **C1** | 5 个主题名解析/回显、大小写不敏感、未知名报错 | `theme.rs` 单测：`ThemeKind::ALL` 遍历 `name()` → `FromStr` 往返；`"NORD"` / `"Nord"` / `"nord"` 同解；`"nord_dark"` 经 `_`→`-` 归一化后仍为未知名并回显原文；`ThemeParseError` 的 Display 含输入原文 | 必测 | impl 作者 |
| **C2** | 五套调色板全部非 bg 槽位不在紫色色相区（HSL 255°–330°、sat ≥0.20、light 0.15–0.90） | `theme.rs` 单测：`for kind in ALL { for slot in [fg,accent,code,muted,user,ok,warn,fail,info] { assert!(!is_purple(slot)) } }`（45 个色）。另附 `is_purple` 自身用例：造一个已知紫（如 `rgb(0x8b5cf6)`）为 true、`bg` 各值为 false | 必测 | impl 作者 |
| **C3** | 每套调色板 10 槽位齐全、均为 `Color::Rgb` | `theme.rs` 单测：遍历 5 套的 10 字段断言 `matches!(c, Color::Rgb(..))`；另断言 5 套的 `bg` 互不相同（防复制粘贴串行） | 必测 | impl 作者 |
| **C4** | 每个主题下 `StyleSheet` 各方法产出对应槽位色（5 主题 × 关键 8 方法） | `styles.rs` 单测：`fn style_fg(style) -> Option<Color>` 取 `style.fg`；5 主题 × {`heading`, `code`, `code_block`, `block_quote`, `table_borders`, `alert(Warning)`, `alert(Caution)`, `alert(Note)`} 断言等于对应槽位 | 必测 | impl 作者 |
| **C5** | 无库默认色泄漏：5 主题各渲染一份全元素 markdown，所有产出 fg/bg ∈ 该主题调色板（替代 `md.rs` 的 `default_signal`） | `md.rs` 单测：构造覆盖全部元素的样例（标题/强调/行内码/代码块/引用/列表/表格/链接/数学/脚注/HTML/图片/四种 alert）；每主题渲染后遍历所有 `Span`，断言 `fg` ∈ `{palette 的 10 值}` 或 `None`，且 `bg` 仅出现 `palette.bg` 或 `None`。**删掉 `default_signal` 后**此用例是唯一的泄漏守卫 | 必测 | impl 作者 |
| **C6** | `/theme` 解析三态（无参→浮层、合法名→切换+发 SetTheme、非法名→提示） | `commands.rs` 单测：`parse("/theme")` → `Action::Prompt(PromptKind::PickTheme)`；`parse("/theme nord")` → `Action::Local(LocalAction::Theme(Some("nord")))`；`parse("/theme nope")` → `Action::Local(LocalAction::Hint(..))` 且 `Hint` 文本含可用主题名；`parse("/theme NORD")` 同样合法 | 必测 | impl 作者 |
| **C7** | 切换主题必须真的重渲染：A 渲染 → 切 B → 同一 transcript 的行色变化；**变异：删掉缓存失效 ⇒ 必须红** | `draw.rs` / `app.rs` 用例：构造含 markdown + 工具结果的固定 transcript，A 主题下取一次渲染行的 `fg` 集合，`app.set_theme(B)` 后再取一次，断言集合不同且 ⊆ B 的调色板。变异点：注释掉 `set_theme` 里的 `md_cache.set_theme`（或 `layout.reset`），用例必须失败 | 必测 | impl 作者 + 变异脚本 |
| **C8** | 持久化闭环：SetTheme → state.json 有字段 → 重启 view 带回 → App 生效 | `state.rs` + `app_loop.rs` 用例：用临时目录（名含进程内原子序号 + `process::id()`）覆盖 `YUSHAN_SESSIONS_DIR` 同款手法或直接构造 `StateStore` 指向临时路径；写 `SetTheme{theme:"nord"}` → 断言文件 JSON 含 `"theme":"nord"` → 新建 `StateStore` 读出 → `AppState.theme == Some("nord")` → `build_view` 的 `CodingView.theme == Nord` → `App::new` 后 `app.theme == Nord` | 必测 | impl 作者 |
| **C9** | 旧 state.json（无 theme 键）仍能解析且不丢 `last_active_model` | `state.rs` 用例：写一份 `{"last_active_provider":"p","last_active_model":"m"}`（**无** `theme` 键）→ `load()` → 断言 `last_active_model == Some("m")` 且 `theme == None`。变异点：把字段改成非 `Option` 且无默认值（删 `#[serde(default)]` 对 `Option` 是等价变异，实测仍绿）| 必测 | impl 作者 |
| **C10** | `/theme` 出现在命令表与 `/help`，补全可 Tab | `completion.rs` 用例：`all_commands()` 含 name == `"theme"`；`entries()` 在输入 `/th` 时能产出 `theme` 项；`/help` 文本含 `theme` | 必测 | impl 作者 |
| **C11** | 浮层预览：↑↓ 生效、Esc 还原、Enter 确认 | `run.rs` 纯逻辑单测：对 `theme_modal_step` 穷举 `(ThemeModalState, ThemeKey)` 组合 —— `Down` ⇒ `Preview(next)`；`Up` ⇒ `Preview(prev)`；`Esc` ⇒ `Restore(opened_with)`（且 `opened_with` 与当前 `selected` 不同时仍回 `opened_with`）；`Enter` ⇒ `Confirm(selected)`。宿主层用可记录 applier 断言「每帧预览都调了 `set_theme`」「Esc 后 `app.theme == opened_with`」。变异点：`Esc` 改成 `Confirm` ⇒ 必须红 | 必测 | impl 作者 + 变异脚本 |
| **C12** | 5 套在真实终端的肉眼观感 | 人工：`cargo run -p ys-coding-agent`，逐个 `/theme <name>` 与无参浮层预览，检查对比度、浮层无黑洞、✓/✗ 可辨、markdown 各元素可读 | 可推迟（**不作门禁**） | 用户 |

### 测试框架与命令

- **构建入口（唯一）**：`./scripts/cargo-build.sh`（不是裸 `cargo`）。常用：`./scripts/cargo-build.sh check --tests`。
- **全量测试入口（唯一，验收必须走）**：`./scripts/par-test.sh`（跨二进制并行，≈7.4s）。选项：`--jobs N` 改并行度、`--no-build` 跳过构建、`-- --ignored` 透传。
  - ⚠️ 直接 `cargo test --workspace` **不算跑过测试**（串行 ≈34s）。
- **单 crate 聚焦**：
  - `./scripts/cargo-build.sh test -p ys-tui-coding`
  - `cargo test -p ys-tui-coding --lib theme::` （C1/C2/C3）
  - `cargo test -p ys-tui-coding --lib styles::` （C4）
  - `cargo test -p ys-tui-coding --lib md::` （C5）
  - `cargo test -p ys-tui-coding --lib commands::` （C6）
  - `cargo test -p ys-tui-coding --lib run::tests::theme` （C11）
  - `cargo test -p ys-coding-agent --lib state::` （C8/C9）
- **静态检查**：`cargo clippy --all-targets`、`cargo fmt --check`。
- **变异测试**：本计划新增 `scripts/theme-mutation.py`，注入三条 bug（删缓存失效 / 把 `theme` 改成非 `Option` 且六默认值 / `Esc` 改成 `Confirm`），期望 C7 / C9 / C11 变红。既有变异脚本不许退化：`python3 scripts/cancellation-mutation.py`、`python3 scripts/wheel-mutation.py`。
- **测试隔离**：临时目录名必须含「进程内原子序号 + `process::id()`」；进程级 env 改写须走 `crate::test_env::env_lock()` + `EnvRestore`。C8/C9 会写 state 文件 ⇒ **必须**用临时目录，不得碰真实 `~/.yushan/state.json`。

## 波及文档

| 文档 | 动作 | 说明 |
|---|---|---|
| `docs/adr/0053-tui-color-themes.md` | **新建** | 记录对 ADR-0030「不做 Theme/Palette」的反转；范围限定为编译期常量色板表 + 5 套内置主题；明确不做用户自定义主题文件、不做运行时主题格式 |
| `docs/design-final/coding-agent-tui.md` | **更新** | `:277` 的 ✓/✗ 绿/红语义改为 `ok` / `fail` 槽位；`:781-782` 的 styles.rs「23 方法覆写」补「颜色来自 `ThemeKind::palette()`」；新增一节讲主题切换与失效路径（as-built，含实现与设计的实际分歧） |
| `crates/ys-tui-coding/src/styles.rs` 模块头 `:6-7` | **改写** | 删掉「无 `Palette` 结构」「不引 Theme」两句，改为当前设计（`Palette` 存在、颜色由 `ThemeKind` 决定、每个方法仍须逐项覆写以免库默认色泄漏） |
| `crates/ys-tui-coding/src/lib.rs` 模块头 | **补充** | 模块列表加 `theme`；若提到「颜色只在渲染层决定」则补一句「颜色由 `ThemeKind` 的编译期色板唯一给出」 |
| `docs/architecture.md` | **不更新** | 该文档不谈配色（颜色属渲染层内部细节）；如实施中发现它确实提到配色，则补一行指针到 ADR-0053 并说明为何不在架构层展开 |
| `docs/TODO.md` | **不更新** | 本需求一次做完，无遗留条目。若 C12 人工观感发现需调色，另开条目 |

写作纪律提醒：以上文档只写**当前**设计。ADR-0053 记「为什么反转」属 ADR 职责（决策的 why），但措辞须是「当前约束要求 X，故反转」而非「以前是 Y，后来改成 X」；`styles.rs` / `lib.rs` 的模块头只写现状。

## 风险与注意事项

1. **缓存失效漏一处 = 偶发旧色**（最高风险）。`md_cache` 与 `layout` 是**两条独立的缓存链**：`md_cache` 缓存 markdown 渲染产物，`layout` 缓存折行后的 `Vec<Line>`（颜色已烘进去）。只清一个的症状是「代码块变色了但标题没变」或反之。**统一入口**是唯一防线：所有切主题路径（`/theme <name>`、浮层预览、浮层确认）都必须经 `App::set_theme`。C7 的变异点必须**分别**删两个清理各验一次。

2. **`layout.sync` 的入参顺序**：加 `theme` 参数会触及 `app.rs:504` 唯一调用点，但 `layout.rs` 内的测试有多个直接调用（`layout.rs` 的 `sync(..)` 调用点）。加参数后这些调用都要补 —— 建议作为**必填**参数而非默认值，让编译器找全所有调用点。

3. **`Clear` 与背景色的交互**：`Clear` 重置为终端默认底色。凡调用 `Clear` 的地方（输入框、选择器浮层、`ui_with_modal` 的模态区）都必须在 `Clear` **之后**补铺 `palette.bg`。漏一处就出现一个与主题背景不一致的矩形。这条**没有单例会抓到**（需要读 buffer 断言），所以实施时按「所有 `Clear` 调用点逐个过一遍」的方式自查，并在 C12 人工环节重点看。

4. **`md.rs` 的大量硬断言会红**：`md.rs` 现有 25+ 处硬断言 `Color::Magenta` / `Color::Cyan` / `Color::DarkGray`（行 186, 211, 234, 236, 245, 262, 313, 414, 420, 426, 442, 450, 458, 466, 471, 477, 482, 487, 505-509, 537, 542, 547, 572-580）。这些**必须**改写为「等于某主题的某槽位」形态，不能简单删除 —— 它们是「漏覆写」的守卫。工作量集中在这一个文件，是 B 单元的主要成本。

5. **`default_signal` 的替代不能弱化守卫**：原判据靠「出现 Blue / Green / LightCyan / LightYellow / LightBlue / 白字黑底 / DIM」判「回落到库默认」。换成「产出色 ∈ 主题调色板」后，**必须**遍历 `Span` 的 fg **和** bg 两者 —— 只看 fg 会漏掉 `code()` 的「白字黑底」（那是 bg 变化）。C5 的断言里 `bg` 只允许 `palette.bg` 或 `None` 正是为此。

6. **`AppState` 新字段的形态**：写成 `Option<String>` 时缺键天然可解，但若将来有人把它改成非 `Option` 的必填字段，`load()` 会因解析失败静默回落 `AppState::default()`，**丢掉** `last_active_model`。C9 钉的是「旧文件仍可解析」这条行为。同理 `capabilities.rs` 三处字面量构造漏改会清掉 `theme`，且症状是「重启后主题丢了」——用户不会发现是 save 的问题。

7. **`/theme` 不走 session meta 是刻意的**：`thinking_visible` 的载体是 `crates/ys-session/src/meta.rs:45`（会话级），`theme` 是全局偏好。**不要**因为「`/thinking` 存 meta」就顺手把 theme 也塞进 meta —— 那样 `/new` 会重置主题、`/resume` 换会话会跟会话变，与 D5 相反。

8. **协议层只传 `String`**：`Request::SetTheme { theme: String }` 不解析、不校验。校验发生在 `commands::parse`（用户输入）与 `ThemeKind::from_str`（`build_view`）。app 侧收到未知名时仍照写 state.json（不拦截）—— 风险是 state.json 里出现非法值，`build_view` 的 `unwrap_or_default()` 会回落 `AyuMirage`。这是**有意的宽容**：协议层不该成为第二个校验点（两个校验点会不一致）。若实施时决定 app 侧也校验，须在 ADR-0053 里记明取舍。

9. **浮层的 `Modal::Select` 复用**：`apply_theme_modal` 应复用 `draw::Modal::Select` 的渲染（避免第二套浮层样式），仅**行为**不同（逐键预览）。不要在 `draw.rs` 里为 `Theme` 再造一个 `Modal` 变体 —— 那会让浮层的 `Clear`/背景补铺逻辑出现两份。

10. **`is_purple` 的边值**：色相 255°/330°、sat 0.20、light 0.15/0.90 都是**含端点**还是不含，必须在实现与测试里定死同一口径（建议左闭右闭 `255.0 <= h <= 330.0`、`sat >= 0.20`、`0.15 <= light <= 0.90`）。若某套主题的某个槽位恰好落在边界上，C2 会红 —— 此时**改调色板**而不是放宽判据（判据来自用户需求）。

11. **`ayu-mirage` 的 `fail` 与 `warn` 对比度**：`#f28779`（fail）与 `#ffd173`（warn）色相接近橙/黄区，小字号下可能难辨。C12 人工环节须确认；若不达标，改调色板（`fail` 往红推）而不是改槽位语义。

12. **不要给 `Palette` 加 `Default`**：`Palette` 的 `Default` 无意义（没有「默认色板」这个概念，只有「默认主题」）。`ThemeKind::default()` 才是唯一入口，`Palette` 只能经 `kind.palette()` 获得 —— 防止某处误用 `Palette::default()` 得到一套与当前主题无关的颜色。

13. **`AssistantStyles` 从 unit 变有字段会波及 `md.rs` 测试**：`md.rs` 的测试大量直接构造 `AssistantStyles`。改成 `AssistantStyles::new(kind)` 后，测试里的构造点都要补主题参数 —— 这正好是 C4/C5 需要的「多主题矩阵」的落点，不是障碍。
