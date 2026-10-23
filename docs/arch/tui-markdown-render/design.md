# TUI Assistant 文本 Markdown 渲染 — 设计

> 前置：`context.md`（现状与范围）、`research.md`（同类软件调研）。
> 决策记录：`docs/adr/0030-tui-markdown-render.md`。

---

## 0. 决策摘要（TL;DR）

1. **采用现成库 `tui-markdown` 0.3.10**（`default-features = false`，彻底不引语法高亮），产出 ratatui `Text`，
   替换 `draw.rs:445-450` 的 `Assistant` 分支。
2. **前置：ratatui 0.29 → 0.30 升级**（`tui-markdown` 依赖 `ratatui-core 0.1`，与 0.29 的 `Text` 是**不同类型、
   不可互传**）。**升级本身的代价实测仅约 10 行**：`Cargo.toml` 2 行 + `run.rs` 8 处泛型 bound
   —— **升级单独不动 `draw.rs`**（`draw.rs` 的改动见 §2.2/§2.8，属**本方案**而非升级）。
   **该升级已编译实证；库集成本身尚未验证** —— 见 §2.1 的说明与 §6 第 1 步。
3. **样式映射点 = 新文件 `styles.rs` 里的一个 `impl StyleSheet`**（不是内联进 `draw.rs`）。`tui-markdown` 的
   `StyleSheet` 是**编译期 trait**、非主题系统 ⇒ 与「颜色只在渲染层决定」「不引 Theme」两条约束**相容**。
4. **拷贝友好是横切原则**（`context.md` 新增节）：Chat 窗格**只留顶部标题线**（去左右竖线）；表格**保留方框线**
   （有意接受的例外）；代码块**不用 `│` 左槽**；引用 `> `、列表 `- `（后者恰是库的默认）。
5. **缓存按 `(槽下标, 内容长度, 宽度)` 记忆渲染产物**；库把「解析」与「表格布局」合在一次调用里，
   故**做不到「resize 只重排表格」的 L1/L2 拆分**（自研路线可以）—— resize 那次帧会全量重算，见 §5 风险 R5。
6. **接受三项与库绑定的既成事实**（详见 §2.7）：① 解析选项由库写死、**表格无法关闭**；
   ② 若干字形写死（无序标记 `- `、引用 `>`、表格框线）；③ 库自述 *experimental Proof of Concept*。
7. **这是产品红线的反转**（Markdown 渲染原被「正式砍掉」）—— 由 ADR-0030 记录，并回改三处「已砍」文档。
8. **渲染路径加 panic 边界**（新，§2.11）：模型输出是**不可信输入**，而解析库自述 PoC。
   `run()` 装 panic hook 恢复终端，避免「库 panic ⇒ 终端留在 raw mode」。
9. **流式尾部重渲染做时间节流**（新，§2.5）：缓存键含 `src.len()` ⇒ 消息**增长期间**每条 delta 都把
   整条重解析（缓存对"正在生成的那条"收益为零）。以 `RENDER_THROTTLE`（~100ms）把解析成本与
   帧率解耦 —— 见 §2.5 与 §4 的成本估算。

---

## 1. 候选方案与对比

四个候选：三条自研（原 A/B/C）与一条用库（D）。

| 维度 | A 自研·最小 | B 自研·分层 | C 自研·性能 | **D 用库（采纳）** |
|---|---|---|---|---|
| 依赖 | +pulldown-cmark | +pulldown-cmark | +pulldown-cmark | **ratatui 0.29→0.30 + crossterm 0.28→0.29 + tui-markdown + pulldown-cmark** |
| 自研代码量 | ~180 行 | ~535 行 | ~800–950 行 | **~120–200 行**（`impl StyleSheet` + 接线 + 缓存）|
| 表格 | ✗ 需自研 | ✗ 需自研 | ✗ 需自研 | **✓ 库内置**（列宽分配/折行/对齐/CJK/框线，469 行实现 + 快照测试）|
| 现有代码改动 | 0 | 0 | 0 | **~10 行**（`run.rs` 泛型 bound）+ `draw.rs` 内区改道 / Assistant 分支 / 回放 raw 路径 |
| 样式控制 | 完全 | 完全 | 完全 | **受限**：粗/斜体颜色不可改、若干字形写死（§2.7）|
| 缓存 | 自定 | 自定 | L1/L2 分层 | **只能整体按 `(内容, 宽度)`**（库内不可分）+ 需补流式节流（§2.5）|
| 成熟度风险 | 自担 | 自担 | 自担 | **库自述 PoC、0.3.x API 未冻结**；解析不可信输入 ⇒ 需 panic 边界（§2.11）|
| 主要风险 | 长会话重解析 | 分层略厚 | 前缀冻结正确性 | **表格极窄时突破宽度**、**升级的连带**、**几何漂移**（§2.8）|

**决定性事实**（`research.md` §2.2）：与本仓 ratatui 0.29 兼容的最新 `tui-markdown`（0.3.6）
**不支持表格**（`lib.rs:154` `warn!("Table not yet supported")`）；带表格的 0.3.7+ 要求 ratatui 0.30。
**用户已授权升级依赖** ⇒ 这条断层消解，D 成为可行且被偏好（「优先成熟库」）。

### 采纳与不采纳

| 来源 | 决断 |
|---|---|
| **D 用库** | **采纳**：元素全覆盖（含表格）；`default-features = false` 后 syntect/onig 全消失（传递依赖 81→44）；流式/畸形输入**不 panic 不吞内容**；CJK 走 `Span::width()` 与 `Paragraph::line_count` **同源** |
| A/B/C 自研 | **不采纳**：核心理由是**省掉最贵且最易错的部分** —— 表格列宽分配/折行/对齐 + CJK 宽度一致性 + 一套快照测试，自研达同等质量在千行级。这是「优先成熟库」偏好的正解 |
| C 的 **L1/L2 分层缓存** | **放弃**：库把解析与表格布局合成一次 `from_str_with_options` 调用，外部**无法**只重排表格。代价见 §5 R5 |
| 自研方案的细节（若将来回退） | 原 A/B/C 的结论留档：`_ => {}` 兜底 `#[non_exhaustive]`、半截 fence 立即渲染、缓存纯 memo 契约 —— 这些**在 D 下同样适用** |

---

## 2. 推荐设计

### 2.1 依赖与版本变更

| crate | 现在 | 变为 | 说明 |
|---|---|---|---|
| `ratatui` | `0.29` | **`0.30`**，`default-features = false`，features `["crossterm", "unstable-rendered-line-info"]` | facade 路径全不变（`ratatui::{Frame,layout,style,text,widgets,backend}` 均 re-export）|
| `crossterm` | `0.28` | **`0.29`**（feature `event-stream`）| 否则 `ratatui-crossterm` 拉 0.29、本仓 0.28 **双版本并存**；0.28→0.29 对本仓用法**非破坏** |
| `tui-markdown` | — | **`0.3.10`**，`default-features = false` | 关掉 `highlight-code` ⇒ syntect/ansi-to-tui/onig(C 库)**全不进入** |

**ratatui 必须 `default-features = false`**：0.30 的默认 feature 集（`underline-color` /
`all-widgets` / `macros` / `layout-cache`）本仓一概不用，却会牵连 `termwiz → uuid v1.27`
（要求 **Rust ≥ 1.89**，把 MSRV 顶高）。只留 `crossterm`（后端）与
`unstable-rendered-line-info`（`Paragraph::line_count`）后，`termwiz` / `uuid` /
`deltae` / `palette` **全部消失**。

**唯一实质代码改动**（`run.rs`）：0.30 的 `Backend` 新增关联类型 `Error`（**无默认值**），
泛型 `B: Backend` + `?` 无法把 `B::Error` 转成 `TuiError`/`PromptError` ⇒ 8 处 bound 改为
`B: Backend<Error = std::io::Error> + Write`。**`draw.rs` 约 1500 行（含全部单测）零改动。**

> ✅ **前置升级已编译实证**（2026-10-09，Rust 1.99.0 / cargo 1.99.0）：
> `cargo build --workspace` 通过（`Finished in 1m 48s`）；
> `cargo test -p ys-tui-coding` **212 passed / 0 failed**。
> 该实测使「`Backend::Error` 必须加 bound」由源码推断升为结论。
>
> ⚠️ **但它覆盖的范围仅限升级本身** —— 那次构建时 `tui-markdown` **尚未进入依赖树**
> （`Cargo.lock` 无该条目）。因此以下三件**仍未验证**，属落地第一步的验收项（§6 第 1 步）：
> ① 库返回的 `Text` 与本仓 `ratatui::text::Text` 是否**同一类型**（依赖 `ratatui 0.30` 与库的
> `ratatui-core 0.1` 版本统一，是全案地基）；② `StyleSheet` 十余个方法的**实际名字与签名**；
> ③ `Options` 的 builder 用法与 `table_width` 语义。以下 §2.3 / §2.7 的接口与覆写清单**均来自阅读源码，
> 非编译结论** —— 落地时是最可能返工之处。

### 2.2 模块划分

```
crates/ys-tui-coding/src/
├── md.rs           （新增，薄）调用 tui_markdown::from_str_with_options，把 Text<'a> 转 owned
│                    Vec<Line<'static>>；构造 Options{styles, table_width}
├── styles.rs       （新增）impl StyleSheet for AssistantStyles —— 全仓唯一给 md 元素定色的地方
├── draw.rs         （改）Chat 窗格只留顶边框；Assistant 分支改走缓存 + md::render
├── cache.rs        （新增）RenderCache：纯 memo，按 (槽下标, 内容长度, 宽度) 记忆 Vec<Line<'static>>
└── app.rs          （改）App 增字段 md_cache: RefCell<RenderCache>
```

**为什么没有大 `md/` 模块**：渲染、表格、折行、CJK 全在库内 —— 本仓侧只剩「调库 + 定色 + 缓存 + 接线」。
这是 D 相对自研最直接的体现：自研的 ~2000 行变成了 ~150 行胶水。

**为什么样式实现在 `styles.rs` 而非 `draw.rs` 内联**：`impl StyleSheet` 有十余个方法，内联进 `draw.rs`
会淹没原有绘制逻辑；单开一个文件但仍属 TUI 渲染层，恪守「颜色只在渲染层决定」。

### 2.3 核心接口

```rust
// ---- md.rs ----
/// 调库渲染一条 assistant markdown。宽度用于表格列宽分配（库只对表格用宽度；
/// 正文 wrap 仍交给 Paragraph）。返回 owned 行，便于缓存。
pub(crate) fn render(src: &str, width: usize, styles: AssistantStyles) -> Vec<Line<'static>>;

// ---- styles.rs ----
/// 语义元素 → Style 的映射。必须是 Clone+Send+Sync+'static（库的 trait 要求）。
#[derive(Clone, Copy, Default)]
pub(crate) struct AssistantStyles;
impl tui_markdown::StyleSheet for AssistantStyles {
    fn heading(&self, level: u8) -> Style { /* 用现有硬编码色，h1 加 BOLD */ }
    fn code(&self) -> Style { /* 新增 inline code 前景色 */ }
    // …其余方法覆写为「本仓配色」。未覆写的会落到库的默认色 ⇒ 必须逐项覆写（§2.7）
}

// ---- cache.rs ----
/// 纯 memo：App 的业务逻辑从不读它；clear() 只损失 CPU，绝不改变输出。
#[derive(Default)]
pub(crate) struct RenderCache { /* slots 与 transcript 等长；仅 Assistant 槽位被填充 */ }
impl RenderCache {
    pub fn sync_len(&mut self, transcript_len: usize);
    /// 取该槽位的渲染行（未命中则 `md::render` 并记住）。
    ///
    /// **借用纪律（必须遵守）**：调用方经 `app.md_cache.borrow_mut()` 取得 `RefMut`，
    /// 返回的 `&[Line]` **生命周期绑在该守卫上** —— 不得带出守卫作用域。唯一正确写法是
    /// **在守卫内 clone 进目标**：
    /// ```ignore
    /// { let mut c = app.md_cache.borrow_mut();
    ///   out.extend(c.lines_for(i, s, w, styles).iter().cloned()); }
    /// ```
    /// 缓存省的是**解析**、不是拷贝；每次命中仍会 clone 一遍（CJK 行宽校验也须在守卫内做完）。
    ///
    /// **不得重入** —— 同一 `RefCell` 二次 `borrow_mut()` 即 panic（与 `run.rs` 的
    /// `TuiPrompter`「禁止重入 `select`/`text`」同性质）；本仓调用点无嵌套，靠此约束保持。
    pub fn lines_for(&mut self, idx: usize, src: &str, width: usize,
                     styles: AssistantStyles) -> &[Line<'static>];
}
```

### 2.4 数据流

```
ModelTextDelta ─► App::apply_event ─► append_stream(Assistant)      app.rs:147,293
                          │
            每帧 run.rs:155 terminal.draw(|f| draw::ui(f, app))
                          ▼
   draw_transcript → build_text_with(app, inner_w, thinking)        draw.rs:111,414
        └─ Assistant(s) ⇒ cache.lines_for(i, s, inner_w, AssistantStyles)
                            └─ 未命中 ⇒ md::render(s, inner_w, styles)
        └─ Paragraph::new(text).wrap(Wrap{trim:false}).scroll(..)   draw.rs:124-128
   total_rows → Paragraph::line_count(inner_w)                      draw.rs:102-106
```

**帧频事实**（`run.rs:58,155`）：`terminal.draw` 在**每次 `poll(50ms)` 超时或事件后**都执行
⇒ **空闲也 ~20fps 重绘**。故缓存是稳态必需，不是可选项；而**增长中的那条**由 §2.5 的节流兜住。
另注意 `build_text` 在**帧路径**内**只被调 1 次**（`draw_transcript` 内），滚动数学由同一处对同一份
`text` 取 `line_count` 得到；`total_rows` 只服务**事件路径**（`App::compute_max_scroll` ← ↑↓ / resize 夹取），
不在帧内调用。两路径的一致性不靠共用时钟，而由 §2.5 边界 ①（节流状态存在缓存内 ⇒ 读同一份产物）保证
（落地订正，2026-10-10）。

**宽度传递注意**：`table_width` 的语义是「**该表格可用的总列数**（含边框、内边距、外层列表/引用前缀）」，
不是整段文本宽度。故传 `inner_w`；库在 `end_table` 会自行扣除嵌套前缀。

### 2.5 缓存规格

| 项 | 规则 |
|---|---|
| 位置 | `App.md_cache: RefCell<RenderCache>` |
| 键 | `(槽下标, src.len(), width)` —— **必须含 width**：库输出对表格依赖宽度 |
| 覆盖 | **仅 `Assistant` 槽位**；其它变体每帧现算（O(1) 格式化，不解析）|
| 为何不含全量哈希 | Assistant 只经 `append_stream` 在**尾部 `push_str`**、transcript 只增 ⇒ 非尾部条目内容恒定，长度相等即内容相同。`debug_assert!` 钉住；将来若支持编辑历史须升级为内容哈希 |
| 为何 `RefCell` | `build_text_with`/`total_rows`/`compute_max_scroll` 现均为 `&App`；改 `&mut` 会波及 `draw_transcript` 与大量测试调用点。UI 侧单线程 ⇒ `RefCell` 低 churn（与 `TuiPrompter` 用 `Mutex` 同手法）|
| 内存上界 | ≈ 1.0–1.3 × assistant 源文本总字节；单条 > 256 KiB 不入缓存（退化每帧现算，不失控）|
| **流式节流** | **正在增长的那条**不逐 delta 重解析：距上次渲染 < `RENDER_THROTTLE`（100 ms）时**沿用上次产物**；到点、或该条**结束时**（新 delta 落到下一条 / turn 结算）**必须**重算一次 |

**为什么必须节流（缓存覆盖不到的那一段）**：键含 `src.len()` ⇒ **消息每长一个字节键就变，整条重解析**。
即 **缓存对「正在生成的那条」收益为零**，它只在消息**结束之后**的下一次重绘才生效。而流式正是文本
**持续增长**的时段：一条 200 KiB 的长回答（含表格）在 ~20fps 下等于 ~4 MB/s 的无谓解析。
节流把「解析成本」与「帧率」解耦 —— 代价是最末一个字最多晚 100 ms 成型（`POLL_INTERVAL` 本就 50 ms，
用户不可感），收益是超长回答不卡。

**两个必须做对的边界**：
1. 节流窗口内沿用旧产物时，**`total_rows` 与 `draw_transcript` 必须用同一个「上次产物」** —— 否则
   滚动上限按新内容算、画面按旧内容画，二者错位（本仓"滚动/宽度失配"前科类）。故节流状态
   **存在 `RenderCache` 内**（它自己记住"这条上次算出什么、什么时候"），**不由两个调用点各自判断**。
2. **该条结束时强制重算**，否则最后一段增量会永久停留不显示。

### 2.6 表格（库负责；本仓只做宽度传入与溢出守卫）

库的表格实现（`renderer/table.rs`）：先缓冲整表，`End(Table)` 时一次性算列宽 ——
自然宽取每列最大显示宽（非等分）；自然宽总和超预算则压到「单字素最小宽」再按最窄列优先逐列 +1；
单元格按**字素边界**折行；行高取该行最高单元格；框线用 Unicode `─│┌┬┐├┼┤└┴┘`；对齐由分隔行决定。

**⚠️ 溢出条件**：库**不保证**「输出行宽 ≤ 给定宽度」。溢出**当且仅当**
`width < (3N+1) + Σ(每列最宽单字素的显示宽)`（N=列数；每列 2 空格 padding + 1 右框线，外加 1 左框线）。
如 2 列 ASCII 的最小宽 = **9**（7+1+1），2 列 CJK = **11**（7+2+2）。低于此值库**不截断内容、直接超宽**。

> 该公式**仅供参考**，**守卫不依赖它**（见下：守卫是"渲染后量行宽"）。**切勿把它硬编码成阈值常数** ——
> 列数、字素宽度、嵌套前缀都会让常数失真。此处记它只为解释"为什么会溢出"。

**破坏后果**：本仓 `draw.rs:124-126` 用 `wrap(trim:false)` ⇒ 超宽行会被 `Paragraph` **二次折行**，
表格边框被切断。**但 `line_count` 与渲染自洽**（二者同用该 `Wrap`）⇒ 滚动数学不崩，**只是表格视觉损坏**。

**本仓守卫（保证不变量 3 恒成立）**：**渲染后校验** —— 量一遍产出行的显示宽度，
**凡「表格行」超宽即令该条退回 raw 文本**（`Assistant(src)` 原样切行）。比「固定阈值」更稳：
自动覆盖任意列数 / CJK 组合，且只需量宽度，代价可忽略。

> ⚠️ **越界判定必须只作用于表格行 —— 库不 wrap 正文**。库自己的文档写得很明确
> （`Options::table_width`，`options.rs:104-108`）：
>
> > *Wraps table cells to fit within `width` terminal columns. … Other Markdown blocks are
> > **unaffected and can be wrapped by the consuming widget**.*
>
> 且 `table_width` 在全库只有 `renderer/mod.rs:91` 一处消费点 —— **库只对表格用宽度**。
> 因此**任何长于窗宽的正文段落本身就是一条超宽行**：若按「任一行超宽就退 raw」实现，
> **每一条稍有长度的回答都会整条丢掉 Markdown 样式**，特性被静默关掉。
> 库自带 doctest 的 `all(|line| line.width() <= 20)` **只对「纯表格文档」成立**，不可推广。
>
> **表格行的判定**：库画表格只用 `│ ┌ ┬ ┐ ├ ┼ ┤ └ ┴ ┘`；产出行含其中任一字符即视为表格行
> （不单独用 `─` —— 分隔线 `---` 是三个普通连字符）。这样嵌套在列表 / 引用里的表格也能被覆盖。
>
> 推论：回退只在「表格窄到库用它自己的最小宽」时发生（即上文 ⚠️ 的溢出条件）；正文超宽**不再**触发回退
> —— 正文 wrap 一律交给 `Paragraph`（这正是本设计的意图）。

### 2.7 样式与「与库绑定的既成事实」

**样式**（`styles.rs` 逐方法覆写，沿用现有硬编码色 —— 不引 Theme/Palette）：
标题分级（h1–h6，去 `#` —— 库的 `heading_marker()` 可置空）、
链接下划线 + URL 尾注（**不用 OSC 8**）、代码块**去围栏行**（`code_block_fence()` 置空）
且**天然无左竖线**（实测：库结构 = 围栏行 + 原样代码行，从无 gutter）⇒ 拷出即纯代码；
`code()` 默认 `white on black` 须覆写为**单一 `cyan` 前景色**（否则整块白字黑底），
行内码与围栏代码块**同色、不分语言** —— 不引底色、不做语法高亮；引用/列表沿用库默认字形。

**`code()` 为什么是「单色、无底色、不分语言」**（2026-10-10 修订）：`code()` 是行内码与
围栏代码块**共用**的唯一钩子，而围栏块走**行级样式** ⇒ 底色会给整段代码铺一大片背景
（长块尤其明显），代码里的缩进 / 行尾空白也会随之显色。而按语言做逐 token 高亮又会把
`highlight-code`（`syntect` + `onig_sys` C 库）拉进依赖树。取中间态：**所有代码共用一个
前景色**，只求与正文可区分。代价：`cyan` 退出「默认配色泄漏」信号集（见 `md.rs` 测试注释）。

**必须逐项覆写的原因**：`DefaultStyleSheet` 自带一套主观配色（H1 白底青、链接蓝、引用绿、表格头 cyan…），
**未覆写的方法会把默认色漏进来** ⇒ 「无 Theme」要求我们**显式覆盖每一个方法**（约十余个，多为一行）。

**三项必须记账的绑定**：
1. **解析选项由库写死**（`mod.rs:76-88` 始终启用 tables/GFM/strikethrough/tasklists/heading-attrs/
   YAML-metadata/super-sub/math/footnotes/definition-list），**无法关闭表格**。比原计划的「元素子集」更宽 ——
   接受（多出的元素只在出现时才渲染，无害）；但 §7 的「元素子集」条目据此改口。
2. **若干字形写死**：无序列表 `- `、引用 `>`、任务 `[x]`、图片 `[img]`、表格框线 —— 只能改**样式**不能改**字符**
   （标题 `#`、代码围栏为例外，可改/可空）。**这恰与我们的拷贝友好选择一致**（`- ` / `>` 本就是我们要的）。
3. **粗体/斜体不可配色**：库把 `Strong`/`Emphasis` 硬编码为纯修饰符（`Style::new().bold()/italic()`）。
   本仓设计本就要 Strong=BOLD、Emphasis=ITALIC（**无颜色**）⇒ **此「缺口」对本仓不是缺口**。

### 2.8 拷贝友好（横切原则，用户 2026-10-09 提出）

**机制**：无鼠标处理，用户用**终端原生选择**；原生选择复制屏幕上实际字形，**应用无法过滤**。

> ⚠️ **前提已变**（[ADR-0050](../../adr/0050-tui-mouse-wheel-scroll.md)）：TUI 现在开
> `EnableMouseCapture`（为滚轮），捕获期间**多数终端不再响应裸拖选**，须 `⇧`+拖选或 `/copy`。
> 本节其余结论（字形即所见、表格框线会被拷贝、代码块无左侧 gutter）仍然有效。

| 位置 | 决定 | 库现状 |
|---|---|---|
| Chat 窗格边框 | **只留顶部标题线**（`draw.rs:125` 的 `Block::bordered()` → `Borders::TOP`）；**几何随之变化，见下** | 本仓改动 |
| 表格 | **保留方框线**（有意接受的例外：框线即可读性；拷贝带 `│─┌┐`）| 库默认即此 |
| 代码块 | **不用 `│` 左槽**，只用缩进（无底色）⇒ 拷贝得到纯代码 | **库天然无 gutter**；仅需 `code_block_fence()=""` 去围栏 + `code()` 只给单一 cyan 前景 |
| 引用 / 列表 | `> ` / `- `（拷出仍是有效 Markdown）| **恰是库默认** —— 白拿 |

**⚠️ 几何连带（不只是 `inner_w`，也不只一处）**：去掉左右竖线后内区从「宽 `w-2` / 高 `h-2`」
变为「宽 `w` / 高 `h-1`」——**宽度和高度的两个数都变了**，且同一约束在代码里有 **4 个站点**
（`max_scroll_offset` 与 `draw_transcript` 各 2 个），必须同进同退：

| 站点 | 现在 | `Borders::TOP` 后 |
|---|---|---|
| `draw.rs:96` | `chat.width.saturating_sub(2)` | `chat.width` |
| `draw.rs:97` | `chat.height.saturating_sub(2)` | `chat.height - 1` |
| `draw.rs:109` | `area.width.saturating_sub(2)` | `area.width` |
| `draw.rs:110` | `area.height.saturating_sub(2)` | `area.height - 1` |

**`inner_h` 漏改的后果比 `inner_w` 更隐蔽**：`follow` 贴底时最后一行被裁、`max_scroll_offset` 差 1 行
—— 而两处都是合法 `usize` 减法，**编译器不抓**。

**做法（不要改这 4 个常数）**：把内区改为**从同一个 `Block` 实例取**（`block.inner(area)`，
`draw.rs:133/327` 已是此写法），即 `max_scroll_offset` 与 `draw_transcript` **共用同一个 block 构造**，
让"算滚动 / 画帧 / `line_count`"三者的几何**不可能漂移**。这既是本项改动，也是顺手还掉的既有重复债
—— 成本低于逐个改常数。

### 2.9 流式半截输入

库对未闭合围栏**天然容错**：pulldown-cmark 视为「延到 EOF 的代码块」，库**照样画出开场+收场围栏行**
⇒ 未闭合期间每帧多一条「假收场围栏」，真内容到达时该行**跳动**（内容不丢）。
未闭合行内标记（`**bold`）按**字面**显示。**半截表格**（分隔行未到）按普通段落字面显示 `|`，分隔行到达后整串重解析才成表。
**策略：立即按解析结果渲染**，不缓冲、不合成临时闭合 —— 库已内建该行为，与自研路线的结论一致。

### 2.10 退出回放与 `/copy`

- `/copy`（`commands.rs:404`）**本就读原始串**，不受渲染影响；且**本就不带边框**。
- 退出回放 `transcript_plain_lines`（`draw.rs:541`）现复用 `build_text_with` ⇒ 会带上 md 样式。
  **决定：回放走 raw 路径**（`Assistant` 原样切行），主屏 scrollback 里留**原始 markdown 源**（可复制可搜）。
- **raw 路径一律「按 `\n` 原样切行」，不预折行** —— 回放与 §2.6 的回退同理。折行由 `Paragraph`
  （`wrap(trim:false)`）做，与既有 assistant 纯文本渲染的行为一致；不变量 3 只约束**表格行**，
  故 raw 行宽不受它约束（预折行会把单词 / URL 从中间截断，属无谓加码）。

### 2.11 panic 边界（新）

**问题**：`run()` 的形状是

```rust
let result = event_loop(...);          // run.rs:109  ← 绘制在循环内，panic 从这里传出
restore_terminal(&mut terminal, &app); // run.rs:113  ← 被 unwind 跳过
result
```

全仓 `set_hook` / `catch_unwind` **零命中** ⇒ **绘制路径 panic 后终端留在 raw mode + alt-screen**，
用户只能 `reset`（`docs/CONTEXT.md` 的"血的教训"类）。本方案之前绘制全是自家代码；本方案**第一次把
模型输出（不可信输入）喂给第三方解析器**，而该库自述 *experimental PoC*。

**决定：在 `run()` 入口装 panic hook**，hook 内执行终端恢复（`disable_raw_mode` + `?1049l` +
`EnableLineWrap` + `DisableBracketedPaste`，复用 `exit_payload` 的前段），**然后才调用上游默认 hook**。

- **为什么是 hook 而非 `catch_unwind` 包 `md::render`**：hook **覆盖全部绘制路径**（不止 md 一处），
  且与本仓"**恢复无条件发生**"的既有精神一致（`restore_terminal` 的注释已申明哪怕出错也不能把终端
  留在 raw mode —— 只是它没管 panic 这条出口）。`catch_unwind` 只能包住你**预先想到**的那个调用点。
- **恢复必须 best-effort**：hook 里每步都吞 IO 错误，绝不二次 panic。
- **`catch_unwind` 不用于"吞掉库 panic 继续跑"**：panic 后 `App`/缓存状态不保证一致，**失败即退出**
  （恢复终端后让进程结束）比带伤续跑安全。

> 这一项**独立于 md 渲染**（任何绘制 panic 都受益），但本方案是它最强的触发理由，故在此落地。

---

## 3. 不变量

1. `transcript.rs` 保持无样式；assistant markdown 的一切颜色只经 `styles.rs`。
2. 块级装饰只用**宽度 = 1** 的字符（避免 `•`/emoji 被判 2 列 ⇒ 滚动错位）。
3. **表格输出的每一行显示宽度 ≤ width** —— **库不保证，须由 §2.6 的守卫补上**。
   该约束**只及表格行**：正文行超宽是常态（库不 wrap 正文），由 `Paragraph` 折行，不触发任何回退。
4. `RenderCache` 是**纯 memo**：`clear()` 只损失 CPU，绝不改变输出。
5. `ys-tui-coding` **不依赖 `ys-runtime`**；UI 线程**无 runtime 上下文**（`RefCell` 单线程内使用）。
6. `tui-markdown` 的 `StyleSheet`/`Options` 分别是 trait 与 `#[non_exhaustive]` 结构：`Options` 只能用
   `new`/`default` + builder，**不可字段字面量构造**。
7. **内区几何只有一个来源**：`max_scroll_offset` / `draw_transcript` / 渲染三处的内区取自**同一个
   `Block` 实例**（§2.8），不得各自手写 `saturating_sub`。
8. **任何绘制路径 panic 后终端不得留在 raw mode / alt-screen**（§2.11 的 panic hook 保证）。

---

## 4. Back-of-envelope

| 项 | 估计 | 依据 |
|---|---|---|
| 现有代码改动 | **~10 行**（run.rs bound）+ `draw.rs` 内区改道 | 第二份调研逐 API 核对 |
| 新增代码 | **~120–200 行**（md.rs 胶水 + styles.rs 覆写 + cache.rs + app/draw 接线 + panic hook）+ 测试 | 对照：自研 A/B/C 为 180–950 行 |
| **每帧解析成本（热路径）** | **增长中的那条**：`O(当前消息字节数)` ÷ `RENDER_THROTTLE`(100ms) ⇒ 上界 ≈ **10 × 消息字节数/秒**（200 KiB 消息 ≈ 2 MB/s）；**已完成的消息**命中缓存 ⇒ O(clone) | §2.5；点估计，须以实测替换 |
| 每帧固定成本 | `build_text` ×1/帧（§2.4 落地订正）+ 每命中行 clone 一遍（§2.3 借用纪律）| `draw.rs` 现状 |
| 依赖增量（默认特性关掉后）| 传递依赖 **81 → 44** 个；**syntect/onig C 库全不进入** | `cargo tree` 实测 |
| 边际新依赖 | `pulldown-cmark` + `itertools` + `tracing`（其余 44 个多来自 ratatui-core，升级 ratatui 本就要）。**`tracing` 本仓不消费**（库的传递依赖，无 subscriber）| 实测 |
| release 体积增量 | **实测 +453,120 B（≈ 442.5 KiB）< 500 KiB**（基线 7,747,024 B → 8,200,144 B；同工具链、同 cargo 默认 release profile）| `cargo build --release -p ys-coding-agent` 前后 `stat -f%z` 对比 |

**可测验收目标**：
① **依赖接入 + 最小 `md::render` 编译通过**（`cargo add tui-markdown` 后 `cargo build -p ys-tui-coding`）——
   这是 §2.1 那三件未验证项的**唯一收敛方式**；
② `cargo build --workspace` + `cargo test -p ys-tui-coding` 全过（**前置升级已验证**，见 §2.1）；
③ `line_count(w)` 与实际渲染视觉行数一致（含 CJK + 表格），多宽度断言；
④ 表格窄屏守卫：**表格行**越界数 = 0（属性测试；**不断言固定阈值**，见 §2.6），
   且**长段正文保住 Markdown 样式**（反向用例，见 R13）；
⑤ 拷贝不含 `│`（S8）；
⑥ **长流式消息**（≥100 KiB）期间无可感卡顿（验证 §2.5 节流）；
⑦ release 体积增量 < +500KB。

---

## 5. 主要风险

| # | 风险 | 缓解 |
|---|---|---|
| R1 | **表格极窄时突破宽度** ⇒ `Paragraph` 二次折行 ⇒ 表格视觉损坏 | §2.6 守卫：渲染后校验行宽，越界该条退回 raw；属性测试 |
| R2 | **~~升级编译未实证~~** | **前置升级已消解**（2026-10-09 实测：workspace 构建 + 212 测试全过，见 §2.1）。**但库集成仍未验证** —— 见 §2.1 ⚠️ 与 §6 第 1 步；落地时 `StyleSheet` 方法名/`Options` 用法最可能返工 |
| R3 | **库默认配色漏进来** ⇒ 破坏「无 Theme」 | `styles.rs` **逐方法覆写**；测试断言产出样式不含库默认色 |
| R4 | **半截护栏视觉跳动** | 确定性、不 panic；若实测刺眼再议 |
| R5 | **resize 全量重算**（放弃了自研的 L1/L2 分层）| 库不可分；resize 是一次性帧抖动，稳态仍 O(1)。若实测卡顿，退路是只对**可见区**渲染 |
| R6 | **库成熟度**（PoC 自述、0.3.x API 未冻结）| ADR 记录；`md.rs` 把库隔离在一处薄胶水里，将来可换。**约束：`tui_markdown::` 路径只准出现在 `md.rs`/`styles.rs`**（可用 CI grep 断言钉住）|
| R7 | **产品红线反转 + 文档未回改** | 交付项含 ADR-0030 + 回改三处「已砍」文档（与功能 diff 分开）|
| R8 | **回放语义漂移**（忘做 raw 路径）| 回放走 raw；测试断言回放含原始 `#`/`**` |
| R9 | **库缺陷（0.3.10 实测）**：**4 空格缩进式**代码块会把多行内容**合并成一行**（围栏式不受影响）| LLM 输出基本是围栏式 ⇒ 影响小；用例钉住围栏式正常；若实测遇到，可在 `md.rs` 侧对 `Indented` 块补救或上报上游 |
| R10 | **几何漂移**：`inner_h` 或 `max_scroll_offset` 内的同源常数漏改 ⇒ 贴底裁行 / 滚动失配（**编译器不抓**）| §2.8：内区统一取自同一 `Block`（`block.inner(area)`），消灭手写减法；不变量 7 |
| R11 | **节流窗口内新旧产物混用** ⇒ 滚动按新、画面按旧 | §2.5 边界 ①：节流状态存 `RenderCache` 内，两调用点读**同一份**上次产物；边界 ②：该条结束时强制重算 |
| R12 | **库 panic ⇒ 终端留在 raw mode**（不可信输入 + PoC 库）| §2.11：`run()` 装 panic hook 做终端恢复；不变量 8 |
| R13 | **守卫范围过宽**：若按「任一行超宽即退 raw」实现 ⇒ 一切长于窗宽的正文段落都退 raw ⇒ Markdown 渲染被**静默关掉**（本轮实现中真实发生，非假设）| §2.6：越界判定**只作用于表格行**（按框线字符判定）；用例须**同时**钉住「长段正文保住样式」与「窄表退 raw」两个方向 |

**变异测试义务**：① 关掉窄屏守卫（超宽用例应红）；② 让缓存永不失效（流式增长用例应红）；
③ 覆写漏掉一个 `StyleSheet` 方法（默认色泄漏用例应红）；④ **`Borders::TOP` 后把内区改回
`saturating_sub(2)`**（贴底/滚动上限用例应红）；⑤ **摘掉节流、改回逐 delta 重渲染**
（"该条结束时内容完整"用例应红）；⑥ **把守卫的表格行判定放宽回「任意行」**
（「长段正文保住样式」用例应红 —— 见 R13）。

---

## 6. 交付顺序

1. **依赖接入 + 最小渲染跑通（最高优先）**：
   - `cargo add tui-markdown --no-default-features`（落 `Cargo.toml`）；
   - 写**最小** `md.rs`：一个 `from_str_with_options` 调用，把 `Text` 收成 owned；
   - 先只验证 **`Text` 类型统一 + `Options`/`StyleSheet` 能否编译** ⇒ `cargo build -p ys-tui-coding`。
   这一步把 §2.1 的三件未验证项一次性收敛，**结果未出前不更新 R2 的"已消解"措辞**。
2. **`styles.rs`**：`impl StyleSheet`，逐方法覆写（**以编译器给出的方法表为准**，含漏写检测测试）
3. **`md.rs`**：owned 转换 + `table_width`；接入 `draw.rs` `Assistant` 分支（先不缓存）
4. **几何改道（R10）**：Chat 窗格改 `Borders::TOP`；`max_scroll_offset` 与 `draw_transcript`
   改为**共用同一 `Block` 取内区**（§2.8），**不要**只去改那 4 个常数
5. **表格窄屏守卫**（§2.6；渲染后量行宽，越界退 raw）
6. **`cache.rs` + `App` 字段** + **流式节流**（§2.5 两条边界；`RENDER_THROTTLE`）
7. **panic hook**（§2.11）—— 独立于 md，但趁 `run.rs` 已动时一并做
8. **回放 raw 路径** + `/copy` 回归（`transcript_plain_lines` 改道；注意 `draw.rs` 内 4 个测试调用点）
9. 测试：S1 半截 fence / S2 代码块 / S3 CJK 列宽 / S4 长会话 / **S4b 长流式消息（节流）** /
   S8 拷贝无边框 / 表格窄屏降级 / **几何变异** / 变异
10. **ADR-0030 + 文档回改**

**前置升级（ratatui 0.30 + crossterm 0.29 + `run.rs` bound）已完成并实证**（commit `87bd1ec`），
是第 1 步的前提，不是第 1 步本身。

`prototype` 尚待验证两点：**表格窄屏守卫的观感**（越界退 raw 后的画面是否可接受）、
**`line_count` 一致性**（§2.4 的宽度传递与 §2.8 的几何改道合起来跑，含 CJK + 表格）。

---

## 7. 与既有决策的对账

| 原条目 | 本轮定稿 |
|---|---|
| 决策反转落地 | ADR-0030 + 回改 `gap-closure/context.md:10,390`、`tui-stack-revisit/design.md:152`、`tui-pi-codewhale-borrow/design.md:23,34,77,151` |
| 解析库选型 | **`tui-markdown` 0.3.10**（`default-features = false`）→ 内部用 `pulldown-cmark 0.13` |
| 元素子集 | **改口**：由库写死，比原计划更宽（表格/GFM/删除线/任务列表/math/footnote…始终开）|
| 链接展示 | 下划线 + URL 尾注；**不做 OSC 8** |
| 缓存策略 | `App.md_cache: RefCell<RenderCache>`，键 = (下标, 内容长度, 宽度)；**无 L1/L2 拆分** |
| 回放 / `/copy` | 保持纯文本；回放走 raw 路径 |
| 半截 fence 策略 | **立即渲染**（库内建行为，会画出假收场围栏）|
| 依赖体积 | 实测 +453,120 B（≈ 442.5 KiB）< 500 KiB（基线 7,747,024 B → 8,200,144 B）|
| **拷贝友好（新增）** | §2.8：去 Chat 左右边框；表格留框线；代码块无竖线 |
| **几何改道（复核新增）** | §2.8：内区不再手写 `saturating_sub`，改由同一 `Block` 取（含 `inner_h`，涉及 4 站点）|
| **流式节流（复核新增）** | §2.5：增长中的那条按 `RENDER_THROTTLE`(100ms) 节流，状态存缓存内 |
| **panic 边界（复核新增）** | §2.11：`run()` 装 panic hook 恢复终端（独立于 md，一并落地）|
| **实证口径（复核收窄）** | §2.1：已实证的只是**前置升级**；库集成为落地第 1 步的验收项 |
| **守卫范围订正（落地发现）** | §2.6 / 不变量 3：越界判定**只作用于表格行**（库不 wrap 正文）；raw 路径按 `\n` 原样切行、**不预折行**。见 R13 |
