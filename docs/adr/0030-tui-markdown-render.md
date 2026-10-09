# 0030 — TUI 对话区 Markdown 渲染：解除红线、采用 tui-markdown、升级 ratatui 0.30

## Status

accepted（2026-10-09）

## 背景

`ys-tui-coding` 把每条 `TranscriptLine::Assistant(String)` **按显式 `\n` 切行、整行单色**渲染
（`crates/ys-tui-coding/src/draw.rs:445-450`）—— 模型输出里的 `# 标题`、`**粗体**`、`` ``` `` 围栏、
表格**原样作为字符显示**。用户要求把 assistant 文本升级为**结构化 Markdown 渲染**，并明确
**标题 / 加粗 / 斜体 / 表格必须支持**；深度为**结构化、不做语法高亮**。

**这是一次产品红线的反转。** Markdown 渲染此前被本仓**正式列为「已砍」**，至少三处文档在案：
`gap-closure/context.md:10,390`、`tui-stack-revisit/design.md:152`、
`tui-pi-codewhale-borrow/design.md:23,34,77,151`。原砍掉的理由是成本（pi ~1k 行 / CodeWhale ~4k 行 renderer）
与「后台 agent + 简洁 TUI」的定位。**本轮反转的理由**：coding agent 的主要产出就是 Markdown 文本，
把它当纯字符显示是对话区可读性的主要短板 —— 这不是「花活」，是核心内容通道的渲染质量。
红线其余部分（**语法高亮 / Theme / Diff Viewer / 鼠标 / 可配快捷键**）**维持砍掉**。

**本轮用户新增两项输入**：①「**可以升级依赖库**」；②「**我们优先使用成熟的、性能好的库**」。
这两条消解了原设计的关键阻塞 —— 见下。

**决定性事实**（`research.md` §2.2）：与本仓 `ratatui 0.29` 兼容的最新 `tui-markdown`（0.3.6）
**不支持表格**（其 `src/lib.rs:154` 明写 `Tag::Table(_) => warn!("Table not yet supported")`）；
**表格只在 0.3.7+，而 0.3.7+ 要求 `ratatui 0.30` + `ratatui-core 0.1`**。授权升级后此路打通。

## 决策

1. **采用 `tui-markdown 0.3.10`**（`default-features = false`）渲染 assistant markdown，替换
   `draw.rs` 的 `Assistant` 分支。它覆盖全部必需元素（标题 h1–h6 / 粗体 / 斜体 / **GFM 表格** /
   列表 / 引用 / 代码块 / 行内码 / 链接 / 分隔线）。`default-features = false` **彻底关掉语法高亮**：
   实测 syntect / ansi-to-tui / **onig(C 库)** 全部不进入，传递依赖由 81 降到 **44** 个。

2. **前置：`ratatui 0.29 → 0.30`（+ `crossterm 0.28 → 0.29`）。** `tui-markdown` 依赖 `ratatui-core 0.1`，
   其 `Text`/`Style` 与 0.29 的**是不同类型、不可互传**。**代价实测仅约 10 行**：
   0.30 的 `Backend` 新增关联类型 `Error`（无默认值）⇒ `run.rs` 的 8 处泛型 bound 改为
   `B: Backend<Error = std::io::Error>`；**`draw.rs` 约 1500 行（含全部单测）零改动**；
   `unstable-rendered-line-info`（`Paragraph::line_count`）**feature 名与写法一字不改**，
   无需显式依赖 `ratatui-widgets`。
   **必须 `default-features = false` + `["crossterm", "unstable-rendered-line-info"]`** —— 0.30 的默认
   feature 集会牵连 `termwiz → uuid v1.27`（要求 Rust ≥ 1.89，顶高 MSRV），关掉后该链（含
   `deltae`/`palette`）全部消失。

3. **样式映射点 = 在渲染层写一个 `impl tui_markdown::StyleSheet`**（新文件 `styles.rs`）。
   该 trait 是**编译期实现、非主题系统**（无配置、无持久化、无 Palette 结构）⇒ 与「颜色只在渲染层决定」
   「不引 Theme」两条约束相容。**但 `DefaultStyleSheet` 自带一套主观配色，未覆写的方法会把默认色漏进来**
   ⇒ 必须**逐方法覆写**（约十余个，多为一行）。粗体/斜体由库硬编码为纯修饰符、不可配色 ——
   本仓设计本就要 Strong=BOLD / Emphasis=ITALIC（无颜色），**此「缺口」对本仓不是缺口**。

4. **拷贝友好是横切原则**（用户提出：终端原生选择会把窗格边框 `│` 一并拷入剪贴板；原生选择复制屏幕字形、
   **应用无法过滤**）：
   - **Chat 窗格只留顶部标题线**（`draw.rs:125` 的 `Block::bordered()` → 仅 TOP）；
   - **表格保留方框线**（有意接受的例外 —— 框线即可读性，且是库的默认形态）；
   - **代码块**：库**天然无左竖线**（结构 = 围栏行 + 原样代码行）⇒ 只需 `code_block_fence()` 置空去围栏
     + `code()` 设背景（默认为 `white on black`，须覆写），拷出即纯代码；
   - 引用 `> `、列表 `- ` —— **恰是库的默认**，白拿。

5. **不做**：语法高亮、Theme/Palette、OSC 8 超链接（仅下划线 + URL 尾注）、Diff Viewer、
   其它 `TranscriptLine` 变体的渲染改造、协议/信道/线程模型/`App` 状态机改动、`-p`/`--json` 输出格式。
   退出回放与 `/copy` **保持纯文本**（回放走 raw 路径）。

6. **表格溢出必须由本仓补守卫**：库**不保证**「输出行宽 ≤ 给定宽度」—— 溢出**当且仅当**
   `width < (3N+1) + Σ(每列最宽单字素宽)`（2 列 ASCII 最小 = 9，CJK = 10），此时它**不截断、直接超宽**。
   本仓 `wrap(trim:false)` 会把超宽行二次折行（表格视觉损坏；`line_count` 与渲染仍自洽，滚动不崩）。
   **守卫 = 渲染后校验行宽，越界则令该条退回 raw 文本**（自动覆盖任意列数 / CJK 组合）。

## Considered Options

- **自研 renderer（原推荐，三条候选 A/B/C）—— 否**。理由是本轮用户偏好（优先成熟库）+ 成本对比：
  自研达同等表格/CJK 质量在**千行级 + 一套快照测试**，而库省下的正是这部分；升级代价仅约 10 行。
  原自研方案的若干结论**在采用库后依然适用**并已保留：`_ => {}` 兜底 `#[non_exhaustive]`、
  半截 fence 立即渲染、缓存纯 memo 契约、退出回放走 raw。
- **`tui-markdown` ≤0.3.6（保持 ratatui 0.29）—— 否**：该版本**无表格**，不满足硬需求。
- **`tui-markdown` 默认特性 —— 否**：`highlight-code` 会拉入 `syntect` + **`onig_sys`（oniguruma C 库，
  需本地 C 工具链）**，与「不做高亮」相悖且体积膨胀。
- **放弃 L1/L2 分层缓存（自研候选 C 的成果）**：库把解析与表格布局合成一次 `from_str_with_options`
  调用，外部**无法**只重排表格 ⇒ 缓存只能整体按 `(内容, 宽度)`。代价：resize 那次帧全量重算（一次性抖动），
  稳态仍 O(1)。

## Consequences

- **`tui-markdown` 是继 `ratatui` 之后 TUI 层第二个外部渲染依赖**；其 README 自述 *experimental
  Proof of Concept*，且处于 0.3.x 活跃演进（0.3.9 才加表格），**API 未冻结**。缓解：库被隔离在
  `md.rs` 一处薄胶水（~150 行）里，将来可换。
- **三项与库绑定的既成事实（明确接受）**：① 解析选项由库写死、**表格无法关闭**（比原计划的元素子集更宽，
  无害）；② 无序标记 `- `、引用 `>`、任务 `[x]`、表格框线**字形写死**（只能改样式不能改字符）；
  ③ 粗体/斜体不可配色。
- ⚠️ **库缺陷（0.3.10 实测）**：**4 空格缩进式**代码块会把多行内容**合并成一行**（围栏式不受影响，
  内容不丢只丢换行，上游无对应测试）。LLM 输出基本是围栏式 ⇒ 影响小，但须记档并在用例中钉住围栏式行为。
- **`draw.rs` 零改动，`run.rs` 8 处 bound**；`Cargo.toml` 两处版本 + 一个新依赖。
  **依赖方向不变**：仍不依赖 `ys-runtime`；UI 线程仍无 runtime 上下文；`ys-protocol` 仍零 tokio。
- ✅ **升级的编译影响已实证**（2026-10-09，Rust 1.99.0）：`cargo build --workspace` 通过
  （`Finished in 1m 48s`），`cargo test -p ys-tui-coding` **212 passed / 0 failed**。
  `Backend::Error` 必须加 bound 由编译日志证实，非仅源码推断。
- ⚠️ **文档回改是本 ADR 的交付项**：三处「已砍」表述须对齐新事实，并写明**只反转 Markdown 渲染，
  高亮/Theme/Diff/鼠标/快捷键仍砍**。不改则文档自相矛盾。
- **已知风险**（详见 design.md §5）：表格窄屏溢出（R1，须加守卫）、库默认配色泄漏（R3，须逐项覆写）、
  resize 全量重算（R5）、库成熟度（R6）。
- **变异测试义务**：关掉窄屏守卫（超宽用例应红）、让缓存永不失效（流式增长用例应红）、
  漏覆写一个 `StyleSheet` 方法（默认色泄漏用例应红）。

## 参考

- 设计与调研：[`docs/arch/tui-markdown-render/design.md`](../arch/tui-markdown-render/design.md)、
  [`context.md`](../arch/tui-markdown-render/context.md)、[`research.md`](../arch/tui-markdown-render/research.md)
- 被反转的红线：[`docs/arch/gap-closure/context.md`](../arch/gap-closure/context.md)、
  [`docs/arch/tui-stack-revisit/design.md`](../arch/tui-stack-revisit/design.md)、
  [`docs/arch/tui-pi-codewhale-borrow/design.md`](../arch/tui-pi-codewhale-borrow/design.md)
- 库：[joshka/tui-markdown](https://github.com/joshka/tui-markdown)（README 自述 PoC）、
  [ratatui BREAKING-CHANGES (v0.30)](https://raw.githubusercontent.com/ratatui/ratatui/main/BREAKING-CHANGES.md)
