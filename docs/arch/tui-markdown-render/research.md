# TUI Markdown 渲染 — 同类软件与依赖调研

> 本文是 `arch-design` Step 2 的产出：业界如何把 Markdown 渲染进终端，以及本仓可采用的具体依赖。
> 结论驱动设计：见 `design.md`。事实核对时间 2026-10-09，版本号以当日 crates.io 为准。

---

## 1. 结论速览

| 结论 | 依据 |
|---|---|
| **现成 ratatui markdown 库存在，但它与本仓的 ratatui 0.29 处于「版本断层」** | 「版本断层」§ |
| **表格是成本主项**（占 `tui-markdown` 0.3.10 源码 ~26%） | §2.1 |
| **不做语法高亮规避了一整类流式崩溃** | pi #274，§4 |
| **流式半截输入有成熟解法：缓冲未闭合块 或 每帧合成临时闭合** | §4 |
| **OSC 8 超链接在 Linux/macOS 主流终端已普及，但终止符有兼容坑** | §5 |

---

## 2. 现成的 ratatui markdown 库

### 2.1 `tui-markdown`（joshka/tui-markdown）— 最贴近的候选

把 Markdown 转成 `ratatui::text::Text`。API：`from_str` / `from_str_with_options(Options)`；
`Options` 可给 `StyleSheet`（自定义样式与符号）、`ImageFallback`、`table_width`。

**源码体量（0.3.10）**：5344 行，分布说明「难点在哪」：

| 文件 | 行数 |
|---|---:|
| `renderer/table.rs` | **1380** |
| `renderer/mod.rs` | 541 |
| `renderer/image.rs` | 437 |
| `renderer/blockquote.rs` | 397 |
| `renderer/code.rs` | 357 |
| `renderer/list.rs` | 352 |
| `style_sheet.rs` | 248 |
| `options.rs` | 253 |
| `renderer/heading.rs` | 204 |
| `renderer/formatting.rs` | 70 |
| 其余（math/footnote/html/link/definition_list…） | ~1000 |

**表格占全crate ~26%** —— 与用户「表格必须支持」的需求正面相关：自研表格渲染是**大头**。

**支持的元素**（0.3.10 文档）：标题、段落、引用（含嵌套）、粗体、斜体、删除线、有序/无序列表、
代码块、GFM 表格（Unicode box-drawing + 对齐）、GFM alert、math（保留分隔符）、图片（文本回退）。

**官方自述**：README 称 *experimental Proof of Concept*。维护者 joshka 是 ratatui 生态核心贡献者。

### 2.2 ⚠ 版本断层（本次调研的**决定性发现**）

`tui-markdown` 各版的 `ratatui` 依赖（查 crates.io sparse index）：

| tui-markdown | 依赖 `ratatui` | 依赖 `ratatui-core` | `pulldown-cmark` | MSRV | 表格 |
|---|---|---|---|---|---|
| 0.2.12 | ^0.28.1 | — | ^0.12.2 | 1.80 | ✗ |
| 0.3.0–0.3.6 | **^0.29.0** | — | 0.12/0.13 | 1.80–1.82 | **✗** |
| 0.3.7–0.3.10 | **^0.30** | ^0.1 | ^0.13 | 1.86–1.88 | ✓ |

本仓现状：`ratatui = "0.29"`（`Cargo.lock` 亦仅 0.29.0；ratatui 最新为 **0.30.2**）。

**关键事实**：**与 ratatui 0.29 兼容的最新版 tui-markdown（0.3.6）不支持表格** ——
其 `src/lib.rs:154-157` 明写 `Tag::Table(_) => warn!("Table not yet supported")`。
**表格支持只在 0.3.7+ 存在，而 0.3.7+ 要求 ratatui 0.30 + `ratatui-core` 0.1。**

⇒ 采用 `tui-markdown` 只有两条路：
- **(a)** 钉 `≤0.3.6`：与 0.29 兼容，但**没有表格**（不满足本轮需求）。
- **(b)** 把 ratatui 0.29 升到 **0.30**：获得表格，代价是一次**跨大版本升级**
  （ratatui 0.30 拆分为 `ratatui-core` / `ratatui-widgets` / `ratatui-crossterm`，
  默认 feature 变为 `[crossterm, underline-color, all-widgets, macros, layout-cache]`；
  `unstable-rendered-line-info` 仍在但移入 `ratatui-widgets`）。**这是一项独立于「md 展示」的架构决策。**

**其余依赖代价**（`default-features = false` 时）：非可选依赖 `itertools`、`tracing`、
`pulldown-cmark`（0.3.0–0.3.6 为 0.12/0.13）。**默认 feature `highlight-code` 会拉入 `syntect` + `ansi-to-tui`**
（须显式 `default-features = false` 关掉，否则与「不做高亮」相悖且体积膨胀）。

### 2.3 其它现成库

| 库 | 说明 | 与我们的关系 |
|---|---|---|
| `ratatui-markdown`（celestia-island） | 非 CommonMark 的 `MarkdownRenderer{new(max_width), parse, render}`，产出 `Vec<Line<'static>>` | 需核对其 ratatui 版本 |
| `ratatui-markdown`（lib.rs 同名，另一作者） | 更重：Mermaid、tree-sitter 高亮、图片、折叠树 | 过重，含我们明确排除的高亮 |
| `cosh-tui` | `MarkdownRenderable`，pulldown-cmark + **可主题化调色板** | 引入 Theme 抽象，与「Theme 已砍」红线冲突 |

---

## 3. 参考实现（自研路线的样板）

| 实现 | 要点 | 借鉴价值 |
|---|---|---|
| **openai/codex `codex-rs/tui/src/markdown_render.rs`** | `render_markdown_text(input) -> Text<'static>` + 宽度感知变体；`Parser::new_ext(...).into_offset_iter()` + `Writer`；**表格按列宽分配，窄时降级 pipe-table** | **同场景（coding agent TUI + ratatui + pulldown-cmark）的最直接样板**；尤其「窄屏表格降级」思路 |
| **grain-ai-agent-tui `md_render`** | **流式**（仿 streamdown）；`MarkdownCache` 缓存已完成行、**只重解析流式尾**；`MdStyleKind`（调色板无关）由调用方映射为 `ratatui::Style` | 性能与「样式语义/颜色分层」的样板；注：ratatui 无 bold+italic 组合 |
| **entanglement ADR-0015** | pulldown-cmark → ratatui Text；选 syntect 而非 tree-sitter；**明确指出 md 渲染 CPU 密集 → 缓存渲染后的 `Text`** | 「必须缓存」的独立佐证 |
| **marquee-markdown** | pulldown-cmark events → Document → block tree → RenderedDoc；**一个布局引擎 + 两个序列化器**（ratatui buffer / ANSI stdout）；不变量「每行恰等于内容宽度，转义序列不进宽度计算」；**链接在序列化期才变 OSC 8** | 若将来要「回放也带样式」，这是双序列化器的样板 |
| `semantic-diff/preview/markdown.rs` | 事件驱动，逐 `Event` 建 `Span::styled` | 事件映射的简单范例 |

---

## 4. 流式半截输入（本需求的真难点）

模型输出是**增量**的，渲染器每帧看到的是**累积到当前的字符串**，会包含半截结构
（未闭合 ``` fence、悬空 `**`、只写了一半的表格）。

| 方案 | 做法 | 代价 |
|---|---|---|
| **缓冲未闭合块** | 未闭合的块（fence/表格/列表）先不渲染，等闭合 | 延迟显示；末块可能长时间不出现 |
| **每帧合成临时闭合** | 对未闭合 fence 补一个临时闭合符，真闭合到达后合成符消失 | 观感可能随闭合跳变；需保证合成符**不进最终文本**（回放/`/copy` 不受影响） |
| **pull-parser 天然容错** | pulldown-cmark 是 pull parser，未闭合结构只会被当作「尚未结束的块」 | 需实测各版行为（见下） |

**实测教训**：
- **pi issue #274**：语法高亮在**畸形 markdown**（无换行的 fence、语言标识是一整句话）下崩溃 →
  修复是「语言标识无效则跳过高亮，按纯代码块渲染」。**这条直接支持本轮「不做高亮」的决策**：
  不解析语言标识，就整类错误都不存在。
- `termflow-md` 用 `parse_line()` + `finalize()`（结束时空块闭合）；
  `flutter_markdown_stream` / `laminar-ui` / `streamtty` 均对未闭合 fence **自动补闭合**，
  且强调「合成闭合符是**非破坏性**的，真闭合到达后消失」。

---

## 5. OSC 8 超链接（链接展示的备选）

- **采用度**（[Alhadis/OSC8-Adoption](https://github.com/Alhadis/OSC8-Adoption)）：iTerm2、Kitty（≥0.19）、
  Alacritty（≥0.11）、WezTerm、Windows Terminal（≥1.4）、Ghostty、foot、VTE 系
  （GNOME Terminal / Tilix / xfce4-terminal…）、**Apple Terminal.app（≥421 / macOS 12+，需按
  `TERM_PROGRAM_VERSION` 版本门控）**、tmux（**≥3.43**）、Zellij。CLI 侧：Cargo、coreutils `ls`、fd、fzf、delta。
- **坑**：
  - **终止符** `ST`(`ESC \`) vs `BEL`(`\x07`) —— 老 tmux（如 3.2a）**只认 BEL 不认 ST**，多数实现为兼容用 BEL。
  - Konsole **默认关闭**；**GNU Screen 不支持**（feature request 自 2017 未实现）。
- **与本仓关系**：平台仅 Linux/macOS。若上 OSC 8，需**版本门控**（Apple Terminal.app）并选 BEL 终止符；
  否则退化为「下划线 + 颜色」的纯样式链接（**零兼容风险，且不破坏终端原生选择/复制**）。
  **本轮倾向不做 OSC 8**，仅样式化 —— 见 `design.md`。

---

## 6. 不适用之处（场景/技术栈差异）

- **pi 的 `marked` + 自定义着色器（TS）**、**CodeWhale 自研 tag-parse + 4k 行 renderer（Rust）**：
  体量与语言都不可直接移植；其价值在「它们踩过的坑」（#274）而非实现。
- **Nushell / bat / glow** 等：bat/glow 是**独立终端渲染器**（不嵌进 ratatui 的 `Text`），
  与本仓「渲染进 ratatui Buffer」的模型不同，不可直接复用。
- **marquee-markdown 的双序列化器**：只有在「退出回放也要带样式」时才值得；
  本仓回放走 `transcript_plain_lines` 纯文本路径，**当前不需要**。

---

## 7. 引用

- [tui-markdown (crates.io)](https://crates.io/crates/tui-markdown) · [joshka/tui-markdown](https://github.com/joshka/tui-markdown)
- [pulldown-cmark (crates.io)](https://crates.io/crates/pulldown-cmark)
- [openai/codex markdown_render.rs](https://github.com/openai/codex/blob/main/codex-rs/tui/src/markdown_render.rs)
- [grain-ai-agent-tui md_render (docs.rs)](https://docs.rs/grain-ai-agent-tui/latest/src/grain_ai_agent_tui/md_render.rs.html)
- [entanglement ADR-0015 rich-text pipeline](https://github.com/xmiksay/entanglement/blob/main/docs/adr/0015-rich-text-pipeline-syntect.md)
- [pi issue #274 — streaming + highlighting](https://github.com/earendil-works/pi/issues/274)
- [pi issue #3103 — OSC 8 links](https://github.com/earendil-works/pi/issues/3103)
- [Alhadis/OSC8-Adoption](https://github.com/Alhadis/OSC8-Adoption)
- [ratatui-markdown (celestia-island)](https://github.com/celestia-island/ratatui-markdown)
