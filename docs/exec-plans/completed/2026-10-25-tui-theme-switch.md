# TUI 配色主题（5 套内置 + `/theme` 实时切换）执行计划

> 依赖声明是**并发调度的依据**：「涉及文件」有交集的任务**不得同波**。
> 本计划的任务拆分依据是 `docs/design-plans/2026-10-25-tui-theme-switch.md`，其中测试合同编号 C1–C12
> 与下文每个任务的「关联测试」一一对应。设计取向（D1–D5）已由用户签字，实施阶段不重开方案选择。

## 上下文引用

- **设计计划**：`docs/design-plans/2026-10-25-tui-theme-switch.md`（**任务拆分的依据**，含 D1–D5 决策与 C1–C12 测试合同）
- **决策记录**：`docs/adr/0053-tui-color-themes.md`（本次新建；Phase 3 只写草稿，正式归档在 Phase 4）
- **as-built 文档**：`docs/design-final/coding-agent-tui.md`（`:277` 讲 ✓/✗ 绿/红语义、`:781-782` 讲 `styles.rs` 的 23 方法覆写 —— 两者都要按主题改写）
- **被反转的旧决策**：`docs/adr/0030-tui-markdown-render.md`（其中「不做 Theme/Palette」由 ADR-0053 反转；**本次不改 ADR-0030 正文**）
- **项目约定**：`AGENTS.md`（依赖方向、信道契约、测试命令、注释纪律）

### 已核实的事实（读码得出，实施时可直接采信）

| 符号 | 现状 | 位置 |
|---|---|---|
| `pub(crate) struct AssistantStyles;` | 零大小类型，`#[derive(Clone, Copy, Default)]` | `crates/ys-tui-coding/src/styles.rs:23-24` |
| `StyleSheet` 覆写 | 23 个方法；`AlertKind::Caution => Color::Red` | `styles.rs:26-...`；`:121` |
| `styles.rs` 模块头「不引 Theme」 | `:6-7` 明写「无 `Palette` 结构…不引 Theme」 | `styles.rs:6-7` |
| `draw.rs` 13 处 `Color::*` | 223/267/396/428/450/401/597/798/821/846/848/853/899/918/927 | `crates/ys-tui-coding/src/draw.rs` |
| `pub fn ui(frame, app)` | `:106`；`:106-110` 有 `MIN_WIDTH`/`MIN_HEIGHT` early return；`:117` 调 `sync_scroll_state`；`:779` 造 `AssistantStyles` | `draw.rs` |
| `RenderCache::Slot { len, width, lines, rendered_at, forced }` | 键 = `(len, width)`，**不含主题** | `crates/ys-tui-coding/src/cache.rs:41-55` |
| `RenderCache::reset()` | `pub fn` | `cache.rs:93` |
| `EntryMemo { sig, render_sig, rev, lines, row_starts }` | `entry_sig`（`:532`）只哈希条目文本，**不含主题** | `crates/ys-tui-coding/src/layout.rs:52-60` / `:532` |
| `TranscriptLayout::sync(transcript, cache, thinking_visible, width, now)` | 内部有「宽度变 / thinking 翻转 ⇒ 全量作废」分支 | `layout.rs:149`；`:158-166` |
| `App::sync_scroll_state(width, height, now)` | 内部调 `layout.sync(...)` | `crates/ys-tui-coding/src/app.rs:504` |
| `App::reset_for_replay()` | 内含 `md_cache.borrow_mut().reset(); layout.borrow_mut().reset();` | `app.rs:231` |
| `App::new` 消费 `view.thinking_visible` 一次 | `let thinking_visible = view.thinking_visible;` | `app.rs:139` |
| `CodingView.thinking_visible` | 注释：只在启动时被 `App::new` 消费一次，运行期真值在 `App` | `crates/ys-tui-coding/src/view.rs:120-123` |
| `CodingView::for_test()` | `:188` 起，`:239` 是 `thinking_visible` 一行 | `crates/ys-tui-coding/src/view.rs` |
| `static COMMANDS: [CommandSpec; 14]` | `CommandSpec { name, description, arg_hint }` | `crates/ys-tui-coding/src/commands.rs:52` |
| `enum Action` / `LocalAction` / `PromptKind` | `LocalAction` 有 `Help/Status/Copy/Thinking(Option<bool>)/Effort/Hint(String)` | `crates/ys-tui-coding/src/commands.rs` |
| `pub fn apply_local(app, action)` | 把文本写进 transcript | `commands.rs:517` |
| `/thinking` 的单向同步形态 | `matches!` 判类 → `apply_local` → `if is_thinking { send_request(Request::SetThinking {..}) }` | `crates/ys-tui-coding/src/events.rs:471-486` |
| `trait Prompter::select(&self, ..)` | `&self`、**无回调**、只回传显示串 | `crates/ys-tui-coding/src/prompter.rs:60-71` |
| `resolve_prompt(kind, prompter, view)` | `match kind` 四臂；`PickSession` 返回 `Err(PromptError::Other(..))` | `prompter.rs:99` |
| `fn select_modal(&mut self, prompt, options, page_size)` | 真终端模态循环；每帧 `draw::ui_with_modal`；逐键 Up/Down/Enter/Esc/Ctrl-C/Backspace/Delete/Left/Right/Home/End/Char/Paste | `crates/ys-tui-coding/src/run.rs:414` |
| `fn run_prompt(...)` | 调 `resolve_prompt` / `resolve_session_pick` → `apply_prompt_outcome` | `run.rs:249` |
| `md::render(src, width, styles: AssistantStyles)` | 收 `AssistantStyles` 为值 | `crates/ys-tui-coding/src/md.rs:38` |
| `fn default_signal(style) -> Option<&'static str>` | 靠具名色判「漏覆写」；**改用 RGB 后失效** | `md.rs:560-586` |
| `md.rs` 硬断言具名色的行 | 186, 211, 234, 236, 245, 262, 313, 414, 420, 426, 442, 450, 458, 466, 471, 477, 482, 487, 505-509, 537, 542, 547, 572-580 | `md.rs` |
| `ys_tui_coding::lib.rs` 的 `mod` 列表 | 字母序：… `run`, `styles`, `transcript`, `view` | `crates/ys-tui-coding/src/lib.rs:34-49` |
| `ys_protocol::Request` | 已有 `SetThinking { visible: bool }`（`:37`）；`#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]` | `crates/ys-protocol/src/request.rs:14-71` |
| `AppState { last_active_provider, last_active_model }` | **字段没有 `#[serde(default)]`** | `apps/coding-agent/src/state.rs:13-17` |
| `StateStore::load()` | 解析失败 → log + `AppState::default()`（**丢掉 `last_active_model`**） | `apps/coding-agent/src/state.rs:30` |
| `StateStore::save()` | 落盘 `~/.yushan/state.json`，0o600 | `apps/coding-agent/src/state.rs:44`；路径 `:26` |
| `state.save(&AppState { .. })` | `:157` / `:186`（`/logout` 用 `AppState::default()`）/ `:253`；测试 `:1189` | `apps/coding-agent/src/capabilities.rs` |
| `build_view(cfg, agent, wiring, registry, state, stats, session_started)` | 已持 `state: &StateStore`，**只调 `state.exists()`（`:134`），不调 `load()`** | `apps/coding-agent/src/view.rs:26`；`:136` |
| `app_loop.rs` 的 thinking 镜像 + `SetThinking` 臂 | `:333` 初始化；`:485` 匹配臂 | `apps/coding-agent/src/app_loop.rs` |
| `app_loop.rs` 模块文档伪码 | `:29` 含 `SetThinking{visible} => { ... }` | `apps/coding-agent/src/app_loop.rs` |
| 启动期已 `load()` 一次 | `let state_store = state::StateStore::new(); let saved_state = state_store.load();` | `apps/coding-agent/src/main.rs:2657-2658` |
| `thinking_visible` 持久化载体 | session meta（`pub thinking_visible: bool`） | `crates/ys-session/src/meta.rs:45` |
| **`apps/coding-agent` 是否依赖 `ys-tui-coding`** | **是** | `apps/coding-agent/Cargo.toml:21` |
| `ys-tui-coding` 是否有文件 IO | **无** —— crate 内无任何 `std::fs` / `File::` | `crates/ys-tui-coding/src/` |
| ADR 编号上界 | `0052-compact-at-round-boundary.md` ⇒ 本次 `0053` | `docs/adr/` |

### 本计划不做的（与设计计划「非目标」一致）

- 用户自定义主题文件、运行时加载的主题格式（不引 serde 配置驱动的主题）。
- 主题进 session meta（`/theme` 走 `state.json`，全局偏好）。
- 每套主题独立的「浅色」变体。

## 基线

- **基线 ref**：`adcda18cd1d0be1fd0e4bc0ab24491df8974e254`（Phase 3 起点，`baseline.sh tui-theme-switch` 建立；备份分支 `sw-backup/tui-theme-switch`）。
- 用途：review 执行 `git diff <基线 ref> -- <files>` 取字节级证据。
- **前置闸门**：`./scripts/par-test.sh` 必须全绿（否则基线不可信）。

## 任务清单

单元划分（与设计计划的 A–F 一致）：

| 单元 | 任务 | 波次 | 依赖 | 涉及文件集 |
|---|---|---|---|---|
| **A** | T1, T2 | W0 | 无 | `theme.rs`（新建）+ `lib.rs` |
| **B** | T3, T4 | W1 | A | `styles.rs` + `md.rs` |
| **C** | T5, T6, T7 | W1 | A | `draw.rs` + `cache.rs` + `layout.rs` + `app.rs` |
| **D** | T8, T9, T10, T11, T12 | W2 | A, C | `commands.rs` + `prompter.rs` + `run.rs` + `events.rs` + `completion.rs` |
| **E** | T13, T14, T15 | W2 | A | `request.rs` + `state.rs` + `view.rs`(app) + `app_loop.rs` + `capabilities.rs` |
| **F** | T16, T17 | W3 | A–E | 文档（`docs/adr/0053`、`docs/design-final/coding-agent-tui.md`） |

> **单元 B 与 C 的文件集不相交**（B: `styles.rs`/`md.rs`；C: `draw.rs`/`cache.rs`/`layout.rs`/`app.rs`）⇒ 可同波（W1）。
> **单元 D 与 E 的文件集不相交**（D 全在 `ys-tui-coding`；E 全在 `ys-protocol` + `apps/coding-agent`）⇒ 可同波（W2）。
> ⚠️ **`crates/ys-tui-coding/src/app.rs` 的潜在冲突**：C（T7）写 `App.theme` 与 `App::set_theme`；D（T8/T10/T11）**只读** `App::set_theme`、`app.theme`，**不改 `app.rs`**。
> 处置：`app.rs` 的全部改动集中在 T7（单元 C）里一次做完，D 的任务说明里显式写「本任务不改 `app.rs`」。若实施时发现 D 必须改 `app.rs`（例如 `set_theme` 的可见性或签名不够用），**则 D 依赖 C 且整体顺延到 W3**，并在 T7 补一条「预留给 D 的接口」说明。当前设计**不预期**发生这种情况：`App::set_theme` 定为 `pub(crate)`，`App.theme` 定为 `pub(crate)`，D 所需的读权限已足够。

---

### T1: `theme.rs` — `ThemeKind` / `Palette` / 5 套常量表 / `FromStr` / `is_purple`

- type: impl
- 依赖: 无
- 涉及文件: `crates/ys-tui-coding/src/theme.rs`（新建）, `crates/ys-tui-coding/src/lib.rs`
- 关联测试任务: T2（C1/C2/C3 单测）
- 说明:
  - 新建 `theme.rs`，内容按设计计划「模块落点」节：
    - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum ThemeKind { #[default] AyuMirage, GruvboxDark, Nord, EverforestDark, SolarizedDark }`
    - `pub const ALL: [ThemeKind; 5]`（顺序即浮层展示顺序：Ayu Mirage / Gruvbox Dark / Nord / Everforest Dark / Solarized Dark）。
    - `fn name(&self) -> &'static str`：`"ayu-mirage"` / `"gruvbox-dark"` / `"nord"` / `"everforest-dark"` / `"solarized-dark"`。
    - `fn label(&self) -> &'static str`：`"Ayu Mirage"` / `"Gruvbox Dark"` / `"Nord"` / `"Everforest Dark"` / `"Solarized Dark"`。
    - `fn next(&self) -> ThemeKind` / `fn prev(&self) -> ThemeKind`：环形（`ALL` 下标 ±1 取模）。
    - `impl FromStr for ThemeKind`：大小写不敏感（`eq_ignore_ascii_case`），`'_'` 先归一化为 `'-'`；未知名返回 `ThemeParseError`，其 `Display` 含输入原文。
    - `pub struct Palette { bg, fg, accent, code, muted, user, ok, warn, fail, info: Color }`，`#[derive(Clone, Copy, Debug, PartialEq, Eq)]`。**不给 `Palette` 实现 `Default`**（设计计划风险 12）。
    - `const fn rgb(hex: u32) -> Color`。
    - `impl ThemeKind { fn palette(&self) -> Palette }`，内部为 5 个 `static` 常量（编译期表，零运行时分配；**按值返回**，`Palette` 是 `Copy`）。
    - `pub(crate) fn is_purple(c: Color) -> bool`：`Color::Rgb` → sRGB → HSL，判色相 ∈ `[255.0, 330.0]`（左闭右闭）、`sat >= 0.20`、`0.15 <= light <= 0.90`；非 `Rgb` 变体返回 `false`。**端点是含还是不含必须在实现与测试里定死同一口径**（采用上面这组）。
  - 5 套取色严格照抄设计计划 D4 的表格（10 槽位 × 5 套），一字不改。
  - `lib.rs`：`mod theme;` 按字母序插在 `mod styles;` 与 `mod transcript;` 之间；`pub use theme::ThemeKind;` 加在 `pub use styles::...` 附近（当前无 `styles` 的 re-export，故放在 `pub use run::{TuiError, run};` 之后或按既有顺序）。
  - 模块头 `//!` 只写当前设计：主题是编译期常量色板表、`Palette` 的 10 槽位语义、为什么不做用户自定义主题（TUI crate 零文件 IO）、紫色守卫的判据出处（产品需求）。**不写**「以前没有主题」这类变更叙事。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo clippy -p ys-tui-coding --all-targets` 无 warning。
  - 关联测试 T2 全绿。

### T2: `theme.rs` — C1 / C2 / C3 单测

- type: test
- 依赖: T1
- 涉及文件: `crates/ys-tui-coding/src/theme.rs`
- 说明: 在 `theme.rs` 的 `#[cfg(test)] mod tests` 里写三条用例：
  - **C1**（`test_theme_names_roundtrip_and_parse`）：
    - `for kind in ThemeKind::ALL { assert_eq!(ThemeKind::from_str(kind.name()).unwrap(), kind); }`
    - 大小写不敏感：`"NORD"` / `"Nord"` / `"nord"` 三者同解为 `Nord`。
    - `'_'` 归一化：`"gruvbox_dark"` → `GruvboxDark`。
    - 未知名：`"nope"` → `Err`，且 `err.to_string()` 含 `"nope"`（原文回显）。
    - `label()` 非空且与 `name()` 不同（防复制粘贴）。
  - **C2**（`test_no_purple_in_any_palette`）：
    - 45 个色：`for kind in ALL { let p = kind.palette(); for (slot, c) in [("fg", p.fg), ("accent", p.accent), ("code", p.code), ("muted", p.muted), ("user", p.user), ("ok", p.ok), ("warn", p.warn), ("fail", p.fail), ("info", p.info)] { assert!(!is_purple(c), "{} 的 {} 落紫色区", kind.name(), slot); } }`
    - `is_purple` 自身用例：已知紫（如 `rgb(0x8b5cf6)`）为 `true`；`Color::Reset` / `Color::Red` 等非 `Rgb` 为 `false`；`Color::Rgb` 的纯灰（sat == 0）为 `false`。
  - **C3**（`test_palette_slots_are_all_rgb`）：
    - 遍历 5 套的 10 字段断言 `matches!(c, Color::Rgb(..))`。
    - 断言 5 套的 `bg` **互不相同**（防复制粘贴串行）。
  - 已知合法撞色（`gruvbox-dark` 的 `user == info`；`everforest-dark` / `solarized-dark` 的 `code == info`）**不写**「互不相同」式的全局断言，只在注释里说明这是调色板非紫色相不足所致、允许。
- 验收:
  - `cargo test -p ys-tui-coding --lib theme::` 全绿（3 条）。
  - **变异自查**：临时把 `is_purple` 的色相上界从 330 改成 300（或把 `rgb` 的 `>> 16` 写成 `>> 8`），C2/C3 必须变红；还原。
- 关联 impl: T1
- 关联测试合同: C1, C2, C3

### T3: `styles.rs` — `AssistantStyles` 携带调色板，23 方法改读槽位

- type: impl
- 依赖: T1
- 涉及文件: `crates/ys-tui-coding/src/styles.rs`
- 关联测试任务: T4（C4/C5 单测）
- 说明:
  - `pub(crate) struct AssistantStyles { palette: Palette }`，`#[derive(Clone, Copy)]`（去掉 `Default` —— 无字段时 `Default` 才合理，现在需显式主题）。
  - 新增 `pub(crate) fn new(kind: ThemeKind) -> Self { Self { palette: kind.palette() } }`。
  - 23 个 `StyleSheet` 方法全部改为读 `self.palette`，映射按设计计划「`styles.rs`：`AssistantStyles` 携带调色板」节的表：
    - `heading` → `accent` + `BOLD`（h1 另加 `UNDERLINED`）
    - `code` / `code_block` → `code`
    - `emphasis` / `strong` / `strikethrough` → `fg`（保留原 modifier）
    - `inline_math` / `math` → `accent`
    - `block_quote` / `table_borders` / `list_marker` / `footnote_marker` / `html` / `image` → `muted`
    - `link` / `link_text` → `info`
    - `alert` 四值 → `Warning` + `Important` = `warn`；`Caution` = `fail`；`Note` + `Tip` = `info`
  - `AlertKind::Caution => Color::Red`（`:121`）改为 `self.palette.fail`。
  - **模块头 `:6-7` 改写**：删掉「无 `Palette` 结构」「不引 Theme」两句，改为当前设计的描述 —— 颜色由 `ThemeKind` 的编译期色板唯一给出；仍必须逐项覆写全部 23 个方法，否则 `DefaultStyleSheet` 的默认色会漏进渲染。
  - 第 12 行「本实现的配色**刻意不使用蓝 / 绿**」这句的口径随之失效（新调色板确实有蓝/绿系）⇒ 改为「所有产出色都来自 `ThemeKind::palette()` 的 10 个槽位，落不进库默认色」（泄漏守卫移到 C5）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过（此时 `draw.rs:779` 的 `AssistantStyles` 构造点会报错 ⇒ 属预期：T5 修）。
  - **与 T5 的耦合**：`draw.rs:779` 是 `AssistantStyles` 的**唯一**生产构造点。若 T3 合入时 T5 未合入，编译会断。
    处置：T3 与 T5 **同波（W1）**，但文件集不相交；合入顺序为 T5 → T3（T5 先用 `AssistantStyles::new(ThemeKind::default())` 占位，T3 再改内部实现）。**若采用分支并行开发**，T3 的验收只跑 `cargo test -p ys-tui-coding --lib styles::`（`cargo check` 允许因 T5 未合入而暂时失败，需在 PR 描述里注明）。
  - 关联测试 T4 全绿。

### T4: `styles.rs` + `md.rs` — C4 / C5 单测

- type: test
- 依赖: T3
- 涉及文件: `crates/ys-tui-coding/src/styles.rs`, `crates/ys-tui-coding/src/md.rs`
- 说明:
  - **C4**（`styles.rs` 的 `test_each_theme_maps_key_methods_to_slots`）：
    - 5 主题 × 关键 8 方法 = 40 个断言。8 方法：`heading` / `code` / `code_block` / `block_quote` / `table_borders` / `alert(Warning)` / `alert(Caution)` / `alert(Note)`。
    - 断言 `style.fg == Some(palette.<对应槽位>)`。
  - **C5**（`md.rs` 的 `test_no_default_color_leak_per_theme`）：
    - 构造覆盖全部元素的样例 markdown：标题（h1–h3）/ 强调 / 粗体 / 删除线 / 行内码 / 围栏代码块 / 引用 / 有序+无序列表 / 表格 / 链接 / 行内数学 / 块数学 / 脚注 / HTML / 图片 / 四种 alert。
    - 每主题渲染后遍历**所有** `Span`：`fg` ∈ `{palette 的 10 个值}` 或 `None`；`bg` 仅 `palette.bg` 或 `None`。
    - **必须同时检查 fg 和 bg**：只看 fg 会漏掉 `code()` 的「白字黑底」（那是 bg 变化）。
  - `md.rs` 现有 25+ 处硬断言具名色（行 186, 211, 234, 236, 245, 262, 313, 414, 420, 426, 442, 450, 458, 466, 471, 477, 482, 487, 505-509, 537, 542, 547, 572-580）**改写成「等于某主题的某槽位」形态**，不得简单删除 —— 它们是漏覆写的守卫。建议引入测试 helper `fn fg_of(style: Style) -> Option<Color>` 与 `fn assert_slot(style, palette, slot_name)` 降低改写量。
  - **`default_signal`（`md.rs:560-586`）的处置**：删除该函数及其调用点，守卫职责完全由 C5 承担。若该函数还被非测试代码使用，则保留但改为「产出色是否 ∈ 当前主题调色板」的实现（实施时读码确认；设计计划预期它只在测试里用）。
- 验收:
  - `cargo test -p ys-tui-coding --lib styles::` 与 `--lib md::` 全绿。
  - **变异自查**：把 `heading` 的 `accent` 改成 `Color::Magenta`（一个不在任何调色板里的具名色），C4 与 C5 必须变红；还原。
- 关联 impl: T3
- 关联测试合同: C4, C5

### T5: `draw.rs` — 13 处 chrome 色 + 背景铺色 + 浮层取色

- type: impl
- 依赖: T1
- 涉及文件: `crates/ys-tui-coding/src/draw.rs`
- 说明:
  - 新增 `fn chrome(app: &App) -> Palette { app.theme.palette() }`（按值；`app.theme` 由 T7 提供；**本任务先用 `ThemeKind::default()` 占位**，T7 合入后切到 `app.theme`）。
  - 13 处 `Color::*` 按设计计划「`draw.rs`：13 处 chrome 色」节的表映射到槽位（223/267/396/428/450/401/597/798/821/846/848/853/899/918/927）。
  - **背景铺色（D2）**：在 `draw.rs:106-110` 的 `MIN_WIDTH`/`MIN_HEIGHT` early-return **之后**、pane 绘制**之前**：
    ```rust
    frame.render_widget(Block::default().style(Style::default().bg(palette.bg)), frame.area());
    ```
  - **`Clear` 之后补铺背景**：凡调用 `Clear` 的区域（输入框、选择器浮层、`ui_with_modal` 的模态区），必须在 `Clear` 之后再用 `bg(palette.bg)` 铺一次。`Clear` 把区域重置为终端默认底色，不补铺就会出现与主题背景不一致的矩形。
  - `ui_with_modal` 的浮层同样从 `chrome(app)` 取色（不为浮层另造色板）。
  - `:779` 的 `let styles = crate::styles::AssistantStyles;` 改为 `crate::styles::AssistantStyles::new(app.theme)`（T3 提供 `::new`；T3 未合入时先用 `::new(ThemeKind::default())` 占位）。
  - **不改** `App` 的任何字段（`app.theme` 由 T7 加）—— 若 T7 未合入，本任务用 `ThemeKind::default()` 常量，T7 合入后改一行。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过（与 T7 的合入顺序见上）。
  - `cargo test -p ys-tui-coding --lib draw::` 既有用例全绿。
  - 目检：启动 TUI 后整屏背景是主题 `bg`，`/theme` 不用管（命令尚未接线）。
- 关联测试任务: T5b（背景铺色与 `Clear` 补铺）
- 关联测试合同: C7 的渲染侧基础

### T5b: `draw.rs` — 背景铺色与 `Clear` 补铺

- type: test
- 依赖: T5
- 涉及文件: `crates/ys-tui-coding/src/draw.rs`
- 说明:
  - 用 `ratatui::backend::TestBackend` 渲染一帧（`draw::ui`），断言**每个**格子的 `bg` 都等于当前主题的 `bg`（含输入框内区、状态行、`MIN_WIDTH` 以上的边角）。
  - 开一次选择器浮层（`draw::ui_with_modal` + `Modal::Select`），断言浮层区域内的 `bg` **仍是**主题 `bg`（钉住「`Clear` 之后补铺」这条，否则浮层是一个黑洞）。
  - 切主题后再渲染一帧，断言全部格子 `bg` 换成新主题的 `bg`。
  - **变异点**：删掉 `Clear` 之后的补铺 ⇒ 第二条断言必须红。
- 验收:
  - `cargo test -p ys-tui-coding --lib draw::` 全绿。
  - 变异自查：删补铺后必须红。
- 关联 impl: T5

### T6: `cache.rs` + `layout.rs` — 主题纳入失效判据

- type: impl
- 依赖: T1
- 涉及文件: `crates/ys-tui-coding/src/cache.rs`, `crates/ys-tui-coding/src/layout.rs`
- 说明:
  - `cache.rs`：
    - `RenderCache` 加私有字段 `theme: Palette`（`Default` 里给 `ThemeKind::default().palette()`）。
    - 新增 `pub fn set_theme(&mut self, palette: Palette)`：**先比值**（`palette == self.theme` 则直接返回，不白清缓存），不等才更新字段并**清空 slots**（复用 `reset()` 的清空逻辑）。语义：「主题变了 ⇒ 所有渲染产物作废」。
    - **不引版本计数器**（`theme_rev`）：`Palette` 是 `Copy + PartialEq` 的值，比值即得结论；计数器会引入「漏加/重复加」这类与渲染无关的失效 bug。
  - `layout.rs`：
    - `TranscriptLayout` 加私有字段 `theme: Palette`；`reset()` 里同步重置（或更新为传入值）。
    - `pub fn sync(...)` 入参**追加** `theme: Palette`（放在 `thinking_visible` 之后、`width` 之前或末尾，以最小化调用点改动为准）。用 `PartialEq` 比值判「换了主题」。
    - 既有全量作废分支（`:158-166`）加 `|| theme != self.theme`，并在分支体内 `self.theme = theme;`。**注意**：即使不休分支也要更新 `self.theme`（否则同一主题连续调用会永远判「变了」）。
    - 加参数后 `layout.rs` 内的所有直接 `sync(..)` 调用（测试里多处）都要补参数。**定为必填参数**（不给默认值），让编译器找全所有调用点。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过（`app.rs:504` 的调用点由 T7 修；若 T7 未合入，先补 `ThemeKind::default()` 占位）。
  - `cargo test -p ys-tui-coding --lib layout::` 与 `--lib cache::` 全绿。
- 关联测试任务: T6b（缓存与布局的主题失效）
- 关联测试合同: C7 的失效机制基础

### T6b: `cache.rs` + `layout.rs` — 主题变更必须使缓存与布局失效

- type: test
- 依赖: T6
- 涉及文件: `crates/ys-tui-coding/src/cache.rs`, `crates/ys-tui-coding/src/layout.rs`
- 说明:
  - **缓存层**：`lines_for` 渲染一条 markdown（主题 A）→ `set_theme(B)` → 同一条同宽度同长度再取一次，断言产物**不是**旧切片（颜色不同）。
    - 变异：`set_theme` 改成空操作 ⇒ 必须红。
  - **布局层**：`sync(.., theme=A, width=W)` → `sync(.., theme=B, width=W)`，断言 `SyncResult.recomputed_entries == transcript 可见条目数`（全量重算）。
    - 同一主题连续 `sync` 两次（宽度不变）断言 `recomputed_entries == 0`（不得永远判「变了」—— 钉住「分支外也要更新 `self.theme`」这条）。
    - 变异：把 `|| theme != self.theme` 从全量作废分支删掉 ⇒ 必须红。
- 验收:
  - `cargo test -p ys-tui-coding --lib cache::` 与 `--lib layout::` 全绿。
  - 两个变异点各跑一次，都必须红。
- 关联 impl: T6

### T7: `app.rs` — `App.theme` 字段 + `App::set_theme` 唯一入口 + C7 用例

- type: impl
- 依赖: T1, T5, T6
- 涉及文件: `crates/ys-tui-coding/src/app.rs`
- 说明:
  - `App` 加 `pub(crate) theme: ThemeKind`（与 `thinking_visible` 对称）。`App::new` 里 `let theme = view.theme;` 消费一次（`view.theme` 由 T14 加；**未合入时用 `ThemeKind::default()` 占位**）。运行期**不被** `Outbound::View` 快照覆盖（与 `thinking_visible` 同一纪律，见 `view.rs:120-123` 的注释）。
  - 新增**唯一**的失效入口：
    ```rust
    pub(crate) fn set_theme(&mut self, kind: ThemeKind) {
        self.theme = kind;
        self.md_cache.borrow_mut().set_theme(kind);
        self.layout.borrow_mut().reset();
    }
    ```
    **禁止**在多个调用点各写一遍失效逻辑。
  - `sync_scroll_state`（`:504`）把 `self.theme` 传给 `layout.sync(...)`。
  - `reset_for_replay`（`:231`）**不重置主题**（`/resume` 换会话时主题不变，D5）—— 但 `layout.reset()` 已清空 `EntryMemo`，下一帧 `sync` 会用当前 `self.theme` 重新填充，无需额外动作。**须在该函数或字段旁注释说明「主题不随 replay 重置」**，防止后人顺手加一行。
  - **C7 用例单独成任务（T7b）**：本任务只产 impl，不写用例。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib app::` 既有用例全绿（不含 C7）。
- 关联测试任务: T7b（C7 渲染失效用例）

### T7b: `app.rs` — C7 切换主题必须真的重渲染

- type: test
- 依赖: T7
- 涉及文件: `crates/ys-tui-coding/src/app.rs`
- 说明:
  - 构造含 markdown + 工具结果的固定 transcript。
  - A 主题下取一次渲染行的 `fg` 集合（走 `sync_scroll_state` + `window` / `cache`）。
  - `app.set_theme(B)` 后再取一次，断言集合不同、且 ⊆ B 的调色板。
  - **变异点（两处分别验）**：
    - 注释掉 `App::set_theme` 里的 `md_cache.set_theme(...)` ⇒ **必须红**（缓存键不含色板，必须显式作废）。
    - 注释掉 `layout.reset()` ⇒ **等价变异体，原颜色断言仍绿**。原因：`TranscriptLayout::sync` 的失效判据已含 `theme != self.theme`（T6），下一帧 sync 会自行全量作废，故 `reset()` 对「重渲染」这件事是冗余的（保留它是为了立即释放已上色的行，并让不变量在 `set_theme` 处自明）。
      因此**「布局必须作废」这条不能靠颜色断言钉住**，改由一条直接断言 `layout.total_rows() == 0` 的用例钉住；而 `layout` 失效判据本身的守卫是 T6b 的变异（删 `|| theme_changed` ⇒ 红）。
      该等价性必须写进用例注释与 design-final 的分歧清单，不得伪装成「红」。
- 验收:
  - `cargo test -p ys-tui-coding --lib app::` 全绿。
  - 两个变异点各跑一次，两次都必须红。
- 关联 impl: T7

### T8: `commands.rs` — 第 15 条命令 + `LocalAction::Theme` + `PromptKind::PickTheme`

- type: impl
- 依赖: T1, T7
- 涉及文件: `crates/ys-tui-coding/src/commands.rs`
- 关联测试任务: T9（C6/C10 单测）
- 说明:
  - `static COMMANDS: [CommandSpec; 14]` → `[CommandSpec; 15]`，新增 `CommandSpec { name: "theme", description: "切换配色主题", arg_hint: Some("[name]") }`（按 `COMMANDS` 既有排序规则插入）。
  - `LocalAction` 加 `Theme(Option<String>)`。
  - `PromptKind` 加 `PickTheme`。
  - `parse` 的 `/theme` 三态：
    - **无参** → `Action::Prompt(PromptKind::PickTheme)`（开浮层）。
    - **有参合法** → `Action::Local(LocalAction::Theme(Some(归一化后的 kebab-case 名)))`。归一路径：`ThemeKind::from_str(arg)` 成功则取 `.name().to_string()`。
    - **有参非法** → `Action::Local(LocalAction::Hint(<含可用主题列表的提示>))`。
  - `apply_local`（`:517`）加 `Theme(Some(name))` 臂：解析成 `ThemeKind` 后调 `app.set_theme(kind)`，并把「theme: <name>」写进 transcript。`Theme(None)` 臂**不应出现**（无参走 `Prompt`），若出现则 `Hint` 提示（防御式，不 panic）。
  - `apply_local` 的 `match` 若穷举 `LocalAction`，新变体必须补臂（Rust 会强制）。
  - **不改 `app.rs`**（`set_theme` 由 T7 提供）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib commands::` 全绿。
- 关联测试合同: C6（用例在 T9）

### T9: `commands.rs` + `completion.rs` — C6 / C10 单测

- type: test
- 依赖: T8
- 涉及文件: `crates/ys-tui-coding/src/commands.rs`, `crates/ys-tui-coding/src/completion.rs`
- 说明:
  - **C6**（`commands.rs` 的 `test_theme_parse_three_forms`）：
    - `parse("/theme")` → `Action::Prompt(PromptKind::PickTheme)`
    - `parse("/theme nord")` → `Action::Local(LocalAction::Theme(Some("nord".into())))`
    - `parse("/theme NORD")` → 同上（大小写不敏感）
    - `parse("/theme gruvbox_dark")` → `Theme(Some("gruvbox-dark"))`（`_` 归一化）
    - `parse("/theme nope")` → `Action::Local(LocalAction::Hint(h))` 且 `h` 含可用主题名（至少含 `"nord"`）
  - **C10**（`completion.rs` 的 `test_theme_in_commands_and_completion`）：
    - `all_commands().len() == 15` 且含 `name == "theme"`。
    - `entries()` 在输入 `/th` 时产出 `theme` 项，且可被 Tab 接受。
    - `/help` 文本含 `theme`（`apply_local(app, LocalAction::Help)` 后检查 transcript 末条）。
    - 先读 `completion.rs` 确认它是否已按 `all_commands().len()` 自适应；**已核实倾向自适应**，若发现硬编码条数则改为自适应（属 T9 范围内的小改，文件集已包含 `completion.rs`）。
- 验收:
  - `cargo test -p ys-tui-coding --lib commands::` 与 `--lib completion::` 全绿。
- 关联 impl: T8
- 关联测试合同: C6, C10

### T10: `prompter.rs` — `PickTheme` 臂返回 `Err`

- type: impl
- 依赖: T8
- 涉及文件: `crates/ys-tui-coding/src/prompter.rs`
- 关联测试任务: T10b（PickTheme 拒绝通用路径）
- 说明:
  - `resolve_prompt`（`:99`）的 `match kind` 加臂：
    ```rust
    PromptKind::PickTheme => Err(PromptError::Other("PickTheme 须经 run.rs 专门模态处理".into())),
    ```
  - 与既有 `PromptKind::PickSession` 的处理形态一致（`prompter.rs` 末尾）。
  - **理由写进注释**：`Prompter::select` 是 `&self` 且无回调，**结构上**无法做实时预览（D3），故 `PickTheme` 不能走通用 `Prompter` 路径。
  - 若 `FakePrompter` 的测试需要断言「`PickTheme` 走到 `run.rs` 而非 `Prompter`」，在 T12 的用例里覆盖。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过。
  - `cargo test -p ys-tui-coding --lib prompter::` 全绿。

### T10b: `prompter.rs` — `PickTheme` 必须拒绝走通用 `Prompter` 路径

- type: test
- 依赖: T10
- 涉及文件: `crates/ys-tui-coding/src/prompter.rs`
- 说明:
  - 用 `FakePrompter` 调 `resolve_prompt(PromptKind::PickTheme, …)`，断言返回 `Err(PromptError::Other(..))`，且消息里含 `PickTheme`。
  - 断言 `FakePrompter` **没有**收到任何 `select` / `text` 调用（记录调用次数），钉住「结构上不走通用 Prompter」这条（D3 的根因）。
- 验收:
  - `cargo test -p ys-tui-coding --lib prompter::` 全绿。
  - 变异：把该臂改成 `Ok(PromptOutcome::NoOp(..))` ⇒ 用例红。
- 关联 impl: T10

### T11: `run.rs` — `theme_modal_step` 纯函数 + `apply_theme_modal` 模态 + `run_prompt` 分流

- type: impl
- 依赖: T8, T10
- 涉及文件: `crates/ys-tui-coding/src/run.rs`
- 说明:
  - **纯逻辑抽取**（C11 的落点）：
    ```rust
    pub(crate) struct ThemeModalState { opened_with: ThemeKind, selected: ThemeKind }
    pub(crate) enum ThemeKey { Up, Down, Enter, Esc, Other }
    pub(crate) enum ThemeModalStep { Preview(ThemeKind), Restore(ThemeKind), Confirm(ThemeKind), Ignore }
    pub(crate) fn theme_modal_step(state: ThemeModalState, key: ThemeKey) -> ThemeModalStep;
    ```
    转移规则：`Down` ⇒ `Preview(selected.next())`；`Up` ⇒ `Preview(selected.prev())`；`Esc` ⇒ `Restore(opened_with)`；`Enter` ⇒ `Confirm(selected)`；`Other` ⇒ `Ignore`。返回值**同时**携带下一个 `ThemeModalState`（或让调用方自行推进 `selected`），二者选一但**必须在文档注释里写清**，避免调用方与纯函数的推进逻辑分叉。
  - **`apply_theme_modal`**（仿 `select_modal`，`run.rs:414`）：
    - 每帧 `terminal.draw(|f| draw::ui_with_modal(f, app, draw::Modal::Select { .. }))`（**复用** `Modal::Select` 的渲染，不新造变体，避免浮层的 `Clear`/背景补铺逻辑出现两份）。
    - 逐键：`Up`/`Down` → `theme_modal_step` → `Preview(kind)` → `app.set_theme(kind)`（**立即**可见）。
    - `Esc` → `Restore(opened_with)` → `app.set_theme(opened_with)`，返回 `None`（**不落盘**）。
    - `Ctrl-C` → 同 `Esc` 的还原语义。
    - `Enter` → 返回 `Some(kind)`，由调用方发 `Request::SetTheme`。
    - 其余键（`Backspace` / `Delete` / `Left` / `Right` / `Home` / `End` / `Char` / `Paste`）→ `Ignore`（主题浮层无查询框，不做过滤）。
  - **`run_prompt`（`run.rs:249`）分流**：`PromptKind::PickTheme` 在调 `resolve_prompt` **之前**分流到 `apply_theme_modal`，拿到 `Some(kind)` 时 `send_request(Request::SetTheme { theme: kind.name().into() })` 并写一条 `Output`。
  - **不改 `app.rs`**（只调 `app.set_theme`）。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-tui-coding` 通过（`run.rs` 内既有用例全绿）。
  - 新用例由 T12b 交付。
- 关联测试任务: T12b（C11 浮层预览三态）
- 关联测试合同: C11

### T12: `events.rs` — `/theme <name>` 的本地生效 + 单向同步

- type: impl
- 依赖: T11
- 涉及文件: `crates/ys-tui-coding/src/events.rs`
- 说明:
  - **`events.rs` 的单向同步**（照 `/thinking` 的形态，`:471-486`）：
    ```rust
    let theme_arg = match &local { LocalAction::Theme(Some(n)) => Some(n.clone()), _ => None };
    apply_local(app, local);
    if let Some(name) = theme_arg {
        send_request(app, requests, Request::SetTheme { theme: name });
    }
    ```
    浮层确认路径在 `run.rs` 里发 `Request::SetTheme`（不走 `apply_local`，因为预览已把状态设好）。
  - 本任务只改 `events.rs`；`run.rs` 的用例由 T12b 写。
- 验收:
  - `cargo test -p ys-tui-coding --lib events::` 全绿。
- 关联测试任务: T12b

### T12b: `run.rs` + `scripts/theme-mutation.py` — C11 浮层预览三态 + 变异脚本

- type: test
- 依赖: T12
- 涉及文件: `crates/ys-tui-coding/src/run.rs`, `scripts/theme-mutation.py`
- 说明:
  - **C11 单测**（`run.rs` 的 `#[cfg(test)] mod tests`）：
    - 对 `(ThemeModalState, ThemeKey)` 穷举：5 个起始主题 × 5 个键。
    - `Down` ⇒ `Preview(next)`；`Up` ⇒ `Preview(prev)`；`Esc` ⇒ `Restore(opened_with)`（**含** `selected != opened_with` 的情形）；`Enter` ⇒ `Confirm(selected)`；`Other` ⇒ `Ignore`。
    - 环形：`SolarizedDark.next() == AyuMirage`；`AyuMirage.prev() == SolarizedDark`。
    - **宿主层**用可记录的 applier（或 `FakePrompter`-风格替身）断言：「每次 `Preview` 都调了 `app.set_theme`」「`Esc` 后 `app.theme == opened_with`」「`Esc` 后**没有**发出 `Request::SetTheme`」。
  - **变异脚本**：新建 `scripts/theme-mutation.py`（仿 `scripts/wheel-mutation.py` 的形态），注入三条 bug 并期望对应用例变红：
    - M1：删掉 `App::set_theme` 里的 `md_cache.set_theme`（或 `layout.reset`）⇒ C7 红。
    - M2：把 `AppState.theme` 改成**非 `Option` 且无默认值**（如 `String`）⇒ C9 红（该用例在 T15）。
      **不要**用「删 `#[serde(default)]`」当变异点：`Option<T>` 缺键本就解成 `None`，删了用例仍绿（实测）。
    - M3：`theme_modal_step` 的 `Esc` 改成 `Confirm` ⇒ C11 红。
  - 变异脚本的用例锚点（用例名）在此任务里定死，供脚本引用。
- 验收:
  - `cargo test -p ys-tui-coding --lib run::` 全绿。
  - `python3 scripts/theme-mutation.py` 的 M1 / M3 报「红（抓到）」（M2 待 T15 合入后一并验）。
- 关联 impl: T11, T12

### T13: `ys-protocol` — `Request::SetTheme { theme: String }`

- type: impl
- 依赖: T1（仅为命名口径一致；`ys-protocol` 本身不依赖 `ys-tui-coding`）
- 涉及文件: `crates/ys-protocol/src/request.rs`
- 说明:
  - `enum Request` 加 `SetTheme { theme: String }`，保持与既有变体同一 `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]` 风格。
  - **注释写清跨 crate 边界的理由**：`Request` 是跨动态库边界传递的纯数据；`ThemeKind` 定义在 `ys-tui-coding`，`ys-protocol` **不认识** UI crate 也不该认识。故协议只承载 kebab-case 名（这一层的最小足够表示），`apps/coding-agent` 收到后**不解析**，只写进 `state.json`；解析回 `ThemeKind` 发生在 `build_view` 时由 owner crate 的 `FromStr` 做。附带收益：新增主题不需要改协议。
  - 若 `ys-protocol` 有穷举 `Request` 的 `match`（如序列化测试、文档测试），补臂。
- 验收:
  - `./scripts/cargo-build.sh check -p ys-protocol` 通过。
  - `cargo test -p ys-protocol` 全绿（含 `Request` 的 serde 往返测试，若有则补 `SetTheme` 一例）。
- 关联测试任务: T13b（`Request::SetTheme` 的 serde 往返）
- 关联测试合同: C8 的协议基础

### T13b: `ys-protocol` — `Request::SetTheme` serde 往返

- type: test
- 依赖: T13
- 涉及文件: `crates/ys-protocol/src/request.rs`
- 说明:
  - 复用文件的 `assert_roundtrip` 辅助：`Request::SetTheme { theme: "nord".into() }` → JSON → 反序列化，断言与原值相等。
  - 断言 JSON 形态含 `"SetTheme"` 与 `"theme":"nord"`（钉住变体名与字段名，跨进程/跨日志兼容要靠它）。
  - 旧 JSON（无此变体的解析路径）不必测 —— `Request` 是 UI→app 的单向命令，不做向后兼容承诺；但**新变体不得改动既有变体的 JSON 形态**（若文件里有既有变体的形态断言，确认它们仍绿）。
- 验收:
  - `cargo test -p ys-protocol` 全绿。
- 关联 impl: T13

### T14: `state.rs` + `view.rs` + `app_loop.rs` + `capabilities.rs` — 持久化闭环

- type: impl
- 依赖: T13
- 涉及文件: `apps/coding-agent/src/state.rs`, `apps/coding-agent/src/view.rs`, `apps/coding-agent/src/app_loop.rs`, `apps/coding-agent/src/capabilities.rs`, `crates/ys-tui-coding/src/view.rs`
- 关联测试任务: T15（C8/C9 用例）
- 说明:
  - `apps/coding-agent/src/state.rs`：
    - `AppState` 加 `#[serde(default)] pub theme: Option<String>`。**注意**：`Option<T>` 缺键本就解成 `None`，该属性此处是**风格一致性 + 意图声明**（与 `SessionMeta` 的可选字段一致），不是正确性开关。真正的风险是有人把它改成非 `Option` 必填字段 —— 那时旧文件解析失败、`load()` 回落 default、`last_active_model` 一起丢（C9 钉这条行为）。
    - 顺序：`last_active_provider` / `last_active_model` 之后加 `theme`。
  - `crates/ys-tui-coding/src/view.rs`：
    - `CodingView` 加 `pub theme: ThemeKind`，**紧跟** `thinking_visible`（同一类「启动期消费一次」的字段），注释同款：「只在启动时被 `App::new` 消费一次；运行期真值是 `App.theme`」。
    - `CodingView::for_test()`（`:188` 起，`:239` 附近）补 `theme: ThemeKind::default()`。
  - `apps/coding-agent/src/view.rs`：
    - **`build_view` 加一个 `theme: ThemeKind` 参数**（在 `session_started` 之后，或按可读性放在 `stats` 之前 —— 实现时选一处并把 10+ 调用点一并补上）。
    - **不读盘**：`build_view` 在 `app_loop.rs:749` 是「每个请求处理完刷快照」时调用，读盘会变成每轮一次 `state.json` IO。取值来自 app 侧进程级镜像（见下）。
    - `CodingView { theme, .. }` 构造。
    - 代码旁注释写明这条取舍（为什么不像 `thinking_visible` 那样只做本地变量：主题必须进视图，UI 需要它）。
    - **`apps/coding-agent` 依赖 `ys-tui-coding` 已核实**（`apps/coding-agent/Cargo.toml:21`），故 `CodingView.theme` 直接用 `ThemeKind`，不用 `String` 往返。
  - `apps/coding-agent/src/app_loop.rs`：
    - 新增**进程级镜像**，与既有 `thinking_visible`（`:333`）并列：
      ```rust
      // 启动时由 state.json 初始化；`SetTheme` 单向更新，落盘后随进程存活。
      let mut theme: ThemeKind = state_store.load().theme.as_deref()
          .and_then(|s| ThemeKind::from_str(s).ok())
          .unwrap_or_default();
      ```
    - 新增 `Request::SetTheme { theme }` 臂：解析成 `ThemeKind`（非法名只产出 `Output` 警告、**不落盘**）→ 更新上面那个 `theme` 镜像 → 读现有 `AppState`、只改 `theme` 字段、`save`；产出 `Outbound::Output`（如 `theme: nord`）与 `Outbound::View(..)`。
    - `build_view` 的两处调用（`:359`、`:749`）把 `theme` 传进去。
    - `:29` 模块文档伪码加 `SetTheme{theme} => { ... }`（与既有 `SetThinking{visible}` 行同款）。
    - **`theme` 是全局偏好，不进 session meta**（与 `thinking_visible` 的载体不同，`crates/ys-session/src/meta.rs:45`）—— 须在代码注释里写明，防止后人「对齐」时顺手塞进 meta。
  - `apps/coding-agent/src/capabilities.rs`：
    - 三处 `state.save(&AppState { last_active_provider, last_active_model })`（`:157` / `:186` / `:253`）改为**保留已有字段**的形态：
      ```rust
      let mut st = state.load();
      st.last_active_provider = ...;
      st.last_active_model = ...;
      state.save(&st)?;
      ```
      `:186`（`/logout`）当前是 `state.save(&AppState::default())` —— 改为「`load()` 后只清 provider/model，保留 `theme`」。
    - 测试 `:1189` 的 `AppState { .. }` 字面量同样处理，或补 `..Default::default()`（字面量构造加 `..Default::default()` 即可，无需 `load()`）。
- 验收:
  - `./scripts/cargo-build.sh check --tests -p ys-coding-agent -p ys-tui-coding` 通过。
  - 关联测试 T15 全绿。
- 关联 impl: T14
- 关联测试合同: C8, C9

### T15: `state.rs` + `app_loop.rs` — C8 / C9 用例

- type: test
- 依赖: T14
- 关联 impl: T14
- 涉及文件: `apps/coding-agent/src/state.rs`, `apps/coding-agent/src/app_loop.rs`
- 说明:
  - **测试隔离**：临时目录名必须含「进程内原子序号 + `process::id()`」（`SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名）。不得碰真实 `~/.yushan/state.json`。
  - **C8**（`test_theme_persists_roundtrip`）：
    - 构造 `StateStore` 指向临时路径 → 写 `SetTheme { theme: "nord" }` 走 app 侧处理 → 断言文件 JSON 含 `"theme":"nord"`。
    - 新建 `StateStore` 读出 → `AppState.theme == Some("nord")`。
    - `build_view` 的 `CodingView.theme == ThemeKind::Nord`。
    - `App::new(view)` 后 `app.theme == ThemeKind::Nord`。
  - **C9**（`test_old_state_without_theme_keeps_last_model`）：
    - 手写 `{"last_active_provider":"p","last_active_model":"m"}`（**无** `theme` 键）→ `load()`。
    - 断言 `last_active_model == Some("m")` 且 `theme == None`。
    - **变异点**：把字段改成非 `Option` 且无默认值 ⇒ 必须红。（删 `#[serde(default)]` 对 `Option` 是等价变异，实测仍绿 —— 不要用它。）
  - **C8/C9 的 app 侧处理**：若 `Request::SetTheme` 的处理逻辑在 `capabilities.rs` 而非 `app_loop.rs`，用例放进 `capabilities.rs` 的 `#[cfg(test)]`（文件集不变，仍属单元 E）。
- 验收:
  - `cargo test -p ys-coding-agent --lib state::` 与 `--lib app_loop::` 全绿。
  - **变异自查**：把字段改成非 `Option` 必填后 C9 必须红；还原。
  - `python3 scripts/theme-mutation.py` 的 M2 报「红（抓到）」（此时三条变异全部可验）。
- 关联测试合同: C8, C9

### T16: `docs/adr/0053-tui-color-themes.md` — ADR 草稿

- type: impl
- 依赖: T1–T15（依赖实现的最终形态，故排 W3）
- 涉及文件: `docs/adr/0053-tui-color-themes.md`（新建）
- 说明:
  - **Phase 3 只写草稿**，正式归档（移入正式 ADR 编号体系、与 `docs/TODO.md` 对账）由 **Phase 4 归档阶段**完成。本任务交付的是一份内容完整的草稿文件，占住 `0053` 号（`docs/adr/` 当前上界是 `0052-compact-at-round-boundary.md`）。
  - 内容要点：
    - **反转对象**：ADR-0030 的「不做 Theme/Palette」。
    - **反转理由（只写当前约束，不写变更叙事）**：具名 16 色的明暗由终端决定，无法给出一套「背景 + 前景 + 强调」自洽的观感；语义槽位的对比度不可控；`md.rs` 的「漏覆写」判据依赖具名色，改用 RGB 后必须换成「产出色 ∈ 调色板」。
    - **范围限定**：编译期常量色板表 + 5 套内置主题。**不做**用户自定义主题文件、**不做**运行时加载的主题格式（不引 serde 配置驱动的主题）。理由：`ys-tui-coding` 零文件 IO，主题配置会破坏这条边界。
    - **10 槽位语义表**与 5 套取值表（照抄设计计划 D4）。
    - **紫色守卫**：非 bg 槽位不得落在 HSL 255°–330° / sat ≥ 0.20 / light 0.15–0.90（产品需求），由单测守卫。
    - **持久化载体**：`~/.yushan/state.json`（全局偏好），**不进** session meta。理由：`/new` 不重置、`/resume` 不跟会话变。
    - **跨 crate 边界**：`Request::SetTheme { theme: String }` 只传 kebab-case 名，协议层不解析、不校验。
    - **代价**（ADR 惯例）：5 套硬编码、用户不能自定义；已知合法撞色三处（`gruvbox-dark` 的 user/info；`everforest-dark` 与 `solarized-dark` 的 code/info）。
    - **与其它文档的关系**：ADR-0030 的「不做 Theme/Palette」由本文反转；`docs/design-final/coding-agent-tui.md` 是 as-built 承载体。
  - 格式照 `docs/adr/` 既有文件的形态（文件名即摘要，本文 `0053-tui-color-themes.md`）。
- 验收:
  - 文件存在、`0053` 号未被占用（`ls docs/adr/ | grep '^0053'` 只有本文）。
  - 内容覆盖上述 8 个要点（逐条对照）。
  - `git diff --name-only` 只含本文。
- 关联测试任务: T17b（文档一致性 grep 断言）

### T17: `docs/design-final/coding-agent-tui.md` 与模块头回写 + 一致性校验

- type: impl
- 依赖: T16
- 涉及文件: `docs/design-final/coding-agent-tui.md`, `crates/ys-tui-coding/src/styles.rs`, `crates/ys-tui-coding/src/lib.rs`
- 说明:
  - `docs/design-final/coding-agent-tui.md`：
    - `:277` 的 ✓/✗ 绿/红语义改为 `ok` / `fail` 槽位（不再说「绿」「红」）。
    - `:781-782` 的 `styles.rs`「23 方法覆写」补一句「颜色全部来自 `ThemeKind::palette()` 的 10 个槽位」。
    - 新增一节「配色主题 — as-built」：模块落点（`theme.rs`）、10 槽位语义、5 套名、`App::set_theme` 的唯一失效入口（`md_cache` 与 `layout` 两条链）、背景铺色与 `Clear` 补铺、`/theme` 三态与浮层预览、持久化到 `state.json`、紫色守卫。
    - **分歧清单**：若实现与设计计划有分歧（如 `build_view` 是否改签名、`cache` 是否按色板值比较、`is_purple` 端点口径），逐条记入该节 —— 缺一即为文档滞后。
  - `crates/ys-tui-coding/src/styles.rs` 模块头：**已在 T3 改写**，本任务只做**校验**（`grep` 确认不再出现「无 `Palette` 结构」「不引 Theme」）。
  - `crates/ys-tui-coding/src/lib.rs` 模块头：若提到「颜色只在渲染层决定」，补一句「由 `ThemeKind` 的编译期色板唯一给出」；`mod theme;` 行已在 T1 加。
  - **一致性校验**（可执行检查，代替人工目检）：
    - `grep` 断言 `docs/design-final/coding-agent-tui.md` 里新写的符号（`ThemeKind` / `Palette` / `set_theme` / `theme_modal_step` / `is_purple` / `state.json`）在 `crates/ys-tui-coding/src/` 与 `apps/coding-agent/src/` 里都**至少命中一次**。
    - `grep` 断言 `styles.rs` 不再含「无 `Palette` 结构」「不引 Theme」；design-final 不再含「✓ 绿」「✗ 红」这类具名色口径。
    - `git diff --name-only` 断言文档改动的文件集合 == `{docs/design-final/coding-agent-tui.md, docs/adr/0053-tui-color-themes.md, crates/ys-tui-coding/src/styles.rs, crates/ys-tui-coding/src/lib.rs}`（`styles.rs`/`lib.rs` 的模块头改动已在 T3/T1 落地，此处只校验）。
- 验收:
  - 上述 `grep` 断言全部成立（命令与输出贴进 PR 描述）。
  - 分歧清单逐条齐全。
  - `docs/architecture.md` **不改**（该文档不谈配色；若实施中发现它确实提到配色，则补一行指针到 ADR-0053 并在 PR 描述里说明为何不在架构层展开）。
- 关联测试任务: T17b（文档一致性 grep 断言）

### T17b: 文档一致性 — grep 断言脚本化

- type: test
- 依赖: T17
- 涉及文件: `scripts/theme-doc-check.sh`
- 说明:
  - 把 T17 的四组 `grep` 断言写成可重跑脚本（退出码 0/1），避免人工目检：
    1. `docs/design-final/coding-agent-tui.md` 里的新符号（`ThemeKind` / `Palette` / `set_theme` / `theme_modal_step` / `is_purple` / `state.json`）在 `crates/ys-tui-coding/src/` 与 `apps/coding-agent/src/` 各至少命中一次。
    2. `crates/ys-tui-coding/src/styles.rs` 不再含「无 `Palette` 结构」「不引 Theme」。
    3. `docs/design-final/coding-agent-tui.md` 不再含「✓ 绿」「✗ 红」这类具名色口径。
    4. `docs/adr/` 下 `0053` 前缀只有一个文件。
  - 断言 4 条各自独立报错（不短路），一次跑出全部违规。
- 验收:
  - `bash scripts/theme-doc-check.sh` 退出 0，输出每条 PASS。
  - 变异：在 `styles.rs` 模块头临时写回「不引 Theme」⇒ 脚本退出 1 且报第 2 条。
- 关联 impl: T16, T17

---

## 波次计划

> **Phase 3 已按「依赖」+「涉及文件交集」重算，权威排程见下表。**
> 下面的预览是**按当前声明的依赖手算的一致性快照**，不是权威排程。

| 波 | 任务（可并行） | 本波关键产出 | 说明 |
|---|---|---|---|
| W0 | T1 | `theme.rs` 骨架 + `mod theme` | 无依赖，deep review |
| W1 | T2, T3, T5, T6 | 主题单测（C1–C3）∥ `styles.rs` 调色板 ∥ `draw.rs` chrome+背景 ∥ `cache`/`layout` 失效 | T3 与 T5/T6 文件集不相交；**合入顺序 T5 → T3**（T5 用占位，T3 改内部） |
| W1′ | T4, T7 | C4/C5 ∥ `App.theme` + `set_theme` + C7 | 依赖 T3 / T5+T6 |
| W2 | T8, T10, T13 | 命令三态 ∥ `PickTheme` 臂 ∥ `SetTheme` 协议 | 文件集两两不相交 |
| W2′ | T9, T11, T14 | C6/C10 ∥ 模态 + 纯逻辑 ∥ 持久化接线 | 依赖各自 W2 前置 |
| W2″ | T12, T15 | C11 + 变异 ∥ C8/C9 | 依赖 T11 / T14 |
| W3 | T16 | ADR 草稿 | 依赖 A–E 收敛 |
| W3′ | T17 | design-final 回写 + 一致性校验 | 依赖 T16 |

> **Phase 3 请以每节的「依赖」与「涉及文件」两栏为准重算波次。**
> 同波任务若「涉及文件」出现交集，一律串行（后一条退到下一波）。
> 全量验收（`./scripts/par-test.sh` 全绿 + `python3 scripts/theme-mutation.py` 三条全红）在最后一波之后作为门禁。

## 验证清单

按顺序执行，全部通过才算完成：

- [ ] `./scripts/cargo-build.sh check --tests` 通过（全 workspace）。
- [ ] `cargo clippy --all-targets` 无 warning。
- [ ] `cargo fmt --check` 通过。
- [ ] **`./scripts/par-test.sh` 全绿**（全量测试**唯一**入口；`cargo test --workspace` 串行 ≈34s，脚本 ≈7.4s，不算跑过测试）。
- [ ] 测试合同 C1–C11 各有至少一条对应用例且全绿（C12 人工，不作门禁）。
- [ ] `python3 scripts/theme-mutation.py` 三条注入全部报「红（抓到）」：M1（删缓存失效 / C7）、M2（`theme` 改成非 `Option` 必填 / C9）、M3（`Esc` 改成 `Confirm` / C11）。
- [ ] 既有变异脚本不退化：`python3 scripts/cancellation-mutation.py`、`python3 scripts/wheel-mutation.py`。
- [ ] `grep` 确认 `crates/ys-tui-coding/src/` 下**仍无** `std::fs` / `File::` 使用（主题不得引入文件 IO）。
- [ ] `grep` 确认 `crates/ys-tui-coding/src/styles.rs` 不再含「无 `Palette` 结构」「不引 Theme」。
- [ ] `grep` 确认 `crates/ys-protocol/src/request.rs` 的 `SetTheme` 用的是 `String` 而非 `ThemeKind`（边界纪律）。
- [ ] `grep` 确认 `apps/coding-agent/src/capabilities.rs` 三处 `AppState { .. }` 不再是无 `..Default::default()` 的全字段字面量（否则会清掉 `theme`）。
- [ ] `ls docs/adr/ | grep '^0053'` 只有 `0053-tui-color-themes.md`。
- [ ] `git diff --name-only` 的文件集合 ⊆ 本计划「单元 A–F」声明的文件集（无越界改动）。
- [ ] 人工（C12，可推迟）：`cargo run -p ys-coding-agent`，逐个 `/theme <name>` 与无参浮层预览，检查 5 套对比度、浮层无黑洞、✓/✗ 可辨、markdown 各元素可读。
