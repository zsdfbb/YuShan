# 0053 — TUI 配色主题：10 槽位语义色板 + 5 套内置主题 + `/theme` 实时切换

把「界面用什么颜色」从终端配色方案手里收回来：渲染层只认 10 个**语义槽位**，主题是**编译期常量表**，
用户用 `/theme` 就地切换、偏好落在 `~/.yushan/state.json`。

## Status

accepted（2026-10-25）—— **已实施**：5 套内置主题（`ayu-mirage` / `gruvbox-dark` / `nord` /
`everforest-dark` / `solarized-dark`）与 `/theme` 实时切换已落地。

落点：`crates/ys-tui-coding/src/theme.rs`（`ThemeKind` / `Palette` / 5 套常量表 / `is_purple`）、
`styles.rs`（`AssistantStyles` 持 `Palette`）、`draw.rs`（`chrome(app)` + 背景铺色）、
`cache.rs`（`RenderCache::set_theme`）、`layout.rs`（`sync` 的 `theme` 判据）、
`app.rs`（`App::set_theme` 唯一失效入口）、`commands.rs` / `run.rs` / `events.rs`（`/theme` 三态与浮层）、
`crates/ys-protocol/src/request.rs`（`SetTheme { theme: String }`）、
`apps/coding-agent/src/{state,view,app_loop,capabilities}.rs`（持久化与视图透传）。

## 背景与问题

`ys-tui-coding` 交付的是**整块自绘界面**：画布背景、正文、标题、代码、状态行、工具成功 / 失败标记。
若颜色只用 ratatui 的**具名 16 色**（`Color::Red` / `Color::Cyan` / `Color::DarkGray` …），其明暗由
**终端配色方案**决定，本仓一行代码也调不动，于是有三个走不通的地方：

1. **给不出自洽的整体观感。** 「背景 + 前景 + 强调」是一组相对关系；具名色下背景永远取决于用户的终端，
   换主题只有前景变、背景不变，观感上等于没换。
2. **语义槽位的对比度不可控。** 「成功 / 警告 / 失败」是三档语义，具名色在不同终端上可能就是同一片暗绿。
3. **「是否回落到库默认配色」只能用具名色写判据。** 助手 markdown 的样式表（`impl tui_markdown::StyleSheet`）
   要判「某个方法是否漏覆写」，其可用信号只能是「产出色是否属于某几个具名色」—— 而「绿 / cyan / 白字黑底」
   这类判据随配色变化就失真。色板化之后应当有的判据是「产出色是否 ∈ 本主题色板」。

**本 ADR 反转 [ADR-0030](0030-tui-markdown-render.md) 的一条决策。** ADR-0030 的决策 5 把
**Theme/Palette** 与**语法高亮 / Diff Viewer / 可配快捷键 / OSC 8 超链接**并列写在「不做」里。
本文**只反转配色这一项**，其余红线**仍然维持砍掉**（见「与其它 ADR 的关系」）。

## 决策

### 1. 10 个语义槽位的色板，`Palette` 按值传递

`Palette` 是 10 个 `Color::Rgb`（`bg` / `fg` / `accent` / `code` / `muted` / `user` / `ok` / `warn` /
`fail` / `info`），`Copy` 且 40 字节。`ThemeKind::palette()` **按值**返回它，不是 `&'static Palette`。

**10 个槽位是「语义 → 颜色」的唯一映射来源**：渲染层（`styles.rs` / `draw.rs` / `md.rs`）只准从槽位取色，
不得出现具名色 —— 否则「默认色泄漏」与「主题漂移」都失去单一判据。

`Palette` **不实现 `Default`**：没有「默认色板」这个概念，只有默认主题（`ThemeKind::default()`）；
不给 `Default` 是为了让「漏传主题」变成编译错误，而不是被一份静默的默认色掩盖。

### 2. 整屏铺 `bg`，且每个 `Clear` 之后补铺

`draw::ui` 在 `MIN_WIDTH` / `MIN_HEIGHT` 的 early-return **之后**先用 `bg` 铺满 `frame.area()`，
再由各 pane 在上面画。`Clear` 会把区域重置为**终端默认底色**（不是主题 `bg`）⇒ 凡调用 `Clear` 的区域，
必须在 `Clear` **之后**重新铺一次 `bg`，否则浮层会出现一个与主题背景不一致的黑洞 / 亮洞。
这是对所有 `Clear` 调用点的硬约束，不是可选优化。

### 3. 5 套内置主题 = 编译期常量表，零运行时分配

色板用 `const fn rgb(0xRRGGBB)` 在 `static` 上构造，`ThemeKind::ALL` 的顺序即浮层展示顺序，
且与变体声明顺序一致（`next` / `prev` 以判别值取下标，依赖这条不变量）。

**不做**用户自定义主题文件，**不做**运行时加载的主题格式。理由见「范围与代价」。

### 4. 失效按**色板值比较**，不引版本计数器

缓存里存的是**已上色**的产物，而缓存键不含主题：

- `RenderCache` 的键是 `(槽下标, src.len(), width)`；
- `TranscriptLayout` 的 `EntryMemo` 签名只哈希条目**文本**，`EntryMemo.lines` 是已烘好颜色的 `Vec<Line>`。

故失效判据统一按 `Palette: PartialEq` **比值**：「色板变了」是 40 字节一次比较即得的结论，
不需要维护 `theme_rev` 计数器 —— 计数器会引入「漏加 / 重复加」这类与渲染完全无关的失效 bug。

**唯一失效入口**是 `App::set_theme(kind)`：更新运行期真值 + 作废 markdown 缓存 + 作废布局记忆，三件事
绑在一起。禁止在调用点各写一遍（漏一处即表现为「某条路径切换后部分内容还是旧色」，且只在缓存恰好命中时现形）。

### 5. `/theme` 三态 + 浮层逐键实时预览

| 输入 | 行为 |
|---|---|
| `/theme`（无参） | 开浮层（`PromptKind::PickTheme`），`↑` / `↓` **立即应用**并重绘 ⇒ 实时预览 |
| `/theme <name>`（合法） | 本地立即生效（`LocalAction::Theme(kind)` → `App::set_theme`），再单向同步 `Request::SetTheme` |
| `/theme <name>`（非法） | `Hint` 一行，列出全部合法名，**不改任何状态** |

浮层出口用类型区分「要不要落盘」：`ThemeModalClose::Confirmed` 才发 `Request::SetTheme`，
`Cancelled` 不发。`Esc` → 还原到打开前的主题（`Restore(opened_with)`）、**不落盘**；
`Ctrl-C` → **先还原再** `Err(Quit)`（让退出回放的 scrollback 用回用户原来那套配色）；
`Enter` → 确认并落盘。

浮层复用既有 `draw::Modal::Select` 渲染，不新造 `Modal` 变体 —— 浮层框架（居中尺寸、`Clear` + 背景补铺、
页脚键位、滚动与计数）只该有一份。代价见「范围与代价」。

**逐键预览必须在 `run.rs` 的专用模态里做**：`Prompter::select` 是 `&self`、**无回调**、只能回传最终选中项的
显示串 —— 结构上感知不到中间过程。故 `resolve_prompt` 对 `PickTheme` 一律返回 `Err`（与 `PickSession` 同形态），
由 `apply_theme_modal` 自己跑循环。「按键 → 预览状态转移」抽成纯函数 `theme_modal_step`，宿主只负责把
`ThemeModalStep` 施加到 `app` 上 —— 模态循环要真终端，语义只能靠纯函数钉进单测。

### 6. 持久化到 `~/.yushan/state.json`（全局偏好），不进 session meta

主题是**全局偏好**：跨会话保留，`/new` 不重置，`/resume` 换会话时主题不变。故载体是 `AppState.theme`
（`Option<String>`，kebab-case 名），不是 `SessionMeta` —— 后者装的是会话级状态（如 `thinking_visible`）。

`build_view` **不读盘**：它在 app 循环里「每个请求处理完刷新快照」时被调，读盘会变成每轮对话一次 IO。
取值来自 app 侧的**进程级镜像**（`app_loop::run` 里 `let mut theme: ThemeKind`，启动期由 `state.json` 初始化、
`Request::SetTheme` 单向更新），与 `thinking_visible` 同一形态。

### 7. 协议只承载 kebab-case 名

`Request::SetTheme { theme: String }` —— 载荷是主题名，**不是枚举**：

- `Request` 是跨动态库边界的纯数据；`ThemeKind` 定义在 `ys-tui-coding`，`ys-protocol` 不认识 UI crate、也不该认识。
- 协议只承载这一层的最小足够表示（一个字串）⇒ 新增主题**不需要改协议**；**协议层不解析、不校验**。
- 名字的解释权归 owner crate：app 侧收到后自己调 `ThemeKind::from_str` 校验与归一，非法名只给一行 `Warning`，
  **不落盘、不改镜像**。两个校验点（用户输入、app 侧）都调**同一个** `FromStr`，不各自写一套判据。

### 8. 紫色守卫（产品需求）

任何主题的 **9 个非 `bg` 槽位**（5 套 × 9 = 45 色）都不得落在紫色色相区：HSL 色相 `[255°, 330°]`
**闭区间**、饱和度 `>= 0.20`、亮度 `∈ [0.15, 0.90]` **闭区间**，三者同时满足才判紫。由 `is_purple` +
单测守卫；改色板时若触发它，**改色板而不是放宽判据**。

非 `Color::Rgb` 变体返回 `false`：本设计不产生具名色 / 索引色，守卫对它们没有「落紫色区」可言；
「色板全是 RGB」由另一条断言负责。

## 理由

- **具名 16 色给不出自洽观感**，语义色也必须由本仓定义 —— 这是「主题」这件事本身的前提。
- **语义槽位让「换主题」只改一处**：主题是一张 10 行常量表，任何 UI 元素都只说「我要 `muted`」，
  不说「我要暗灰」。渲染层因此没有具名色，「默认色泄漏」可以用「产出色 ∈ 色板」一句话判定。
- **按值比较避免计数器类失效 bug**：`Palette` 是 `Copy + PartialEq` 的 40 字节值，比值即得结论；
  版本计数器要人记得加、且加法对不对与渲染无关，正是容易长期错误的形状。
- **浮层的实时预览只能自己做**：`Prompter::select` 的签名（`&self`、无回调、只回最终串）决定了
  「逐键应用」无法经它完成；把它扩展成回调式会把「决策 / 渲染」的分离拆掉。故走专用模态 + 纯函数。
- **state.json 而非 session meta**：`/new` 不重置、`/resume` 不跟会话变，这两条要求排除了会话级载体。
- **`build_view` 收参数而非读盘**：快照刷新频率是「每请求」，读盘会把它变成每轮一次 IO；
  进程级镜像与 `thinking_visible` 同一形态，代价是多一个形参（见「范围与代价」）。

## 范围与代价（明确接受）

- **不做用户自定义主题文件、不做运行时加载的主题格式。** `ys-tui-coding` **零文件 IO**（crate 内不出现
  `std::fs` / `File`）：主题若来自文件，读写、校验、热重载、错误呈现全部要落进 UI crate，与其职责
  （渲染 + 输入）不符。把读取放 app 侧又会让每次视图快照多传一份不受协议约束的数据。
  **代价：5 套硬编码，用户不能增删。**
- **已知合法撞色三处**：`gruvbox-dark` 的 `user` 与 `info` 同色；`everforest-dark` 与 `solarized-dark`
  的 `code` 与 `info` 同色。这几套调色板的非紫色相不足以再拉开一档，硬拆会破坏原本的配色语言。
- **浮层复用 `Modal::Select` ⇒ 顶部多一行无效筛选框。** 主题浮层不筛（候选恒为全体），但共用渲染
  就带上了那行 `> ` 输入框，敲字被 `ThemeKey::Other` 吞掉（不生效、不报错）。这是可用性上的毛刺，
  换掉的是一整套浮层样式的第二份实现 —— 取舍偏向后者。
- **`App::set_theme` 里的 `layout.reset()` 对「重渲染」是冗余的。** 判据里已有 `theme != self.theme`，
  下一帧 `sync` 会自行全量作废，故它是一处**已知的等价变异**：删掉后颜色断言仍绿。
  保留它是为了**立即释放已上色的行**，并让「切主题必须两条链一起作废」这条不变量在 `set_theme` 处自明。
  正因如此，它**不得**依赖颜色断言兜底，而由直接断言 `layout.total_rows() == 0` 的用例钉住。
- **`build_view` 形参达 8 个**（已 `#[allow(clippy::too_many_arguments)]`）。这是「不读盘」的对价：
  将来再有「每请求都要刷新的单个偏好」，应连同 `stats` / `session_started` / `theme` 一起收进一个
  `ViewInputs` 结构体，而不是继续拉长签名。
- **`AppState.theme` 上的 `#[serde(default)]` 对 `Option<T>` 是冗余的**：缺键本就解成 `None`。
  它是风格一致性 + 意图声明，不是正确性开关。真正会让旧 `state.json` 读不出来的是把字段写成
  **非 `Option` 且无默认值**（那时缺键 ⇒ `from_str` 失败 ⇒ `load()` 回落 `AppState::default()`
  ⇒ `last_active_model` 一起丢）。
- **主题只覆盖「语义 → 单色」**：语法高亮仍未做（不引 `highlight-code`），`code` 槽位是行内码与围栏块的
  同一个前景色。

## Considered Options

### A. 继续用具名 16 色，靠终端配色方案承担观感 —— 否

具名色无法给出「背景 + 前景 + 强调」自洽的一组关系；而且「漏覆写」判据本身就是靠具名色写的，
语义色一旦要区分三档（ok / warn / fail），具名色在不同终端上的对比度完全不可控。

### B. 主题从配置文件 / 运行时格式加载（用户可自定义）—— 否

见「范围与代价」：`ys-tui-coding` 零文件 IO 是它作为渲染 crate 的边界，主题配置会把它变成
「渲染 + 配置管理」。**放弃的是用户自定义能力**，换来的是 crate 边界与「主题只有一张常量表」的简单性。

### C. 用版本计数器（`theme_rev`）驱动失效 —— 否

计数器要求每个改主题的点都记得 `+1`，且「加对了吗」与渲染结果无因果关系 ⇒ 漏加/重加都是
「偶发旧色」。`Palette: PartialEq` 把判据变成直接可观测的值比较。

### D. 让 `Prompter::select` 支持逐键回调（替代专用模态）—— 否

回调式 `select` 会把「决策逻辑（`resolve_prompt`，纯函数、可注入 `FakePrompter`）」与「渲染宿主」
重新搅在一起，且影响全部模态命令。专用的 `apply_theme_modal` 只多一个循环，语义还能靠纯函数 `theme_modal_step` 单测。

## 验证

| 项 | 位置 |
|---|---|
| 名称往返 / 大小写不敏感 / `_` 归一化 / 未知名回显原文 / `next`-`prev` 互逆 | `theme.rs` `test_theme_names_roundtrip_and_parse` |
| 45 个非 `bg` 色不落紫色区；三道具门限各有「只有它决定结果」的样本 | `theme.rs` `test_no_purple_in_any_palette` |
| 10 槽位齐全且全为 `Color::Rgb` | `theme.rs` `test_palette_slots_are_all_rgb` |
| 5 主题 × 关键方法 → 槽位映射 | `styles.rs` `test_each_theme_maps_key_methods_to_slots` |
| 无库默认色泄漏（fg **与** bg 都扫） | `md.rs` `test_no_default_color_leak_per_theme` |
| 换主题后同键也必须重算；同值 `set_theme` 不白清缓存 | `cache.rs` `set_theme_invalidates_rendered_slots` / `set_theme_with_same_palette_keeps_cache` |
| 换主题 ⇒ 全部条目重算；同主题连续 `sync` 不重算 | `layout.rs` `theme_change_invalidates_all_entries` / `same_theme_repeated_sync_recomputes_nothing` |
| 切主题整屏换 `bg`；浮层区域不是 `Color::Reset` | `draw.rs` `test_ui_paints_theme_bg_everywhere` / `test_modal_overlay_uses_theme_bg_not_default` / `test_theme_switch_repaints_whole_screen_bg` |
| 两个记忆都被作废（直接断言 `total_rows() == 0`） | `app.rs` `test_set_theme_invalidates_both_caches` |
| `/theme` 三态；本地生效 + 恰好一条 `SetTheme` | `commands.rs` `test_theme_parse_three_forms`；`events.rs` `test_theme_command_applies_locally_and_syncs_once` |
| 浮层 `Esc` 还原、`Enter` 确认、环形移动 | `run.rs` `test_theme_modal_escape_restores_opened_with` / `test_theme_modal_confirm_requests_set_theme` / `test_theme_modal_wraps_around` |
| 旧 `state.json`（无 `theme` 键）仍可解析且不丢老字段 | `apps/coding-agent/src/state.rs` `test_old_state_without_theme_keeps_last_model` |
| `state.json` → 视图 → `App`；未知主题名回落默认 | `apps/coding-agent/src/view.rs` `test_state_theme_reaches_view_and_app` |
| `SetTheme` 真落盘、真更新镜像、非法名不改镜像 | `apps/coding-agent/src/app_loop.rs` `theme_request_persists_and_updates_mirror` |
| `/login`、`/logout` 不清掉 `theme` | `apps/coding-agent/src/capabilities.rs` `test_login_and_logout_preserve_theme` |
| 协议往返（变体名 + 字段名 + 逐字节保真） | `ys-protocol/src/request.rs` `test_request_set_theme_roundtrip` |

**变异测试**：`python3 scripts/theme-mutation.py` 三条注入（M1 删 `App::set_theme` 里的
`md_cache.set_theme` → 缓存用例红；M2 `login` 整份重建 `AppState` 清掉 `theme` → 能力用例红；
M3 `theme_modal_step` 的 `Esc` 改成 `Confirm` → 还原用例红），且只认「预期用例名出现在 FAILED 列表」
（防「空测试骗过」）。`ys-coding-agent` **没有 lib target**，对该 crate 必须用 `--bins`，
否则 `cargo test --lib` 显示「running 0 tests」而静默通过。

**真实终端的观感仍是人工项**：5 套的对比度、浮层无黑洞、`✓` / `✗` 可辨、markdown 各元素可读 ——
单测只看色值与格子，看不出好不好看。

## 与其它文档 / ADR 的关系

- **反转 [ADR-0030](0030-tui-markdown-render.md) 的「不做 Theme/Palette」一项。** ADR-0030 的其余红线
  **仍然维持砍掉**：语法高亮（不引 `highlight-code` / `syntect`）、Diff Viewer、可配快捷键、OSC 8 超链接。
  ADR-0030 的技术结论（`tui-markdown` 0.3.10、表格窄屏守卫、缓存纯 memo、退出回放走 raw）一条未变。
- **`Request::SetTheme` 与 `SetThinking` 同族**：都是「UI 本地即时生效 + 单向同步给 app 持久化」的显示偏好。
  推论：app 侧的 `SetTheme` 臂**不回显**用户可见输出（UI 侧已写 `theme: <name>`），否则每条命令打两遍。
- **as-built 承载体**：`docs/design-final/coding-agent-tui.md` §14.9「配色主题 — as-built」。
- **与 [ADR-0045](0045-tui-scroll-viewport.md) / [ADR-0050](0050-tui-mouse-wheel-scroll.md) /
  [ADR-0051](0051-tui-text-selection.md) 无耦合**：主题只改颜色，不改几何、不改鼠标捕获、不改选区判定。
  唯一的交点是 `TranscriptLayout::sync` 的全量作废判据（ADR-0045 的视窗化布局）—— 主题在其上**多一个**
  作废条件，判据结构不变。

## 参考

- 设计计划：[`docs/design-plans/completed/2026-10-25-tui-theme-switch.md`](../design-plans/completed/2026-10-25-tui-theme-switch.md)
  （D1–D5 决策、C1–C12 测试合同、5 套色值表）
- 执行计划：[`docs/exec-plans/completed/2026-10-25-tui-theme-switch.md`](../exec-plans/completed/2026-10-25-tui-theme-switch.md)（T1–T17）
- 被反转的红线：[`docs/adr/0030-tui-markdown-render.md`](0030-tui-markdown-render.md)
- as-built：[`docs/design-final/coding-agent-tui.md`](../design-final/coding-agent-tui.md) §14.9
