# TUI 行动块层次（tui-action-blocks）— 架构上下文

## 概述

让 TUI 对话区里的**行动块**（模型思考 + 工具调用）用**缩进 + 颜色 + 块间空白行**与
assistant 正文区分开，使「模型在做什么」与「模型说了什么」在扫视时可分。

承接 [`../tui-turn-spacing/context.md`](../tui-turn-spacing/context.md)：那份文档只做了
**问答边界**（`User` 之前留 2 行空白），并在其「后续建议」里明确留了后路 ——
「若要进一步做『按块类型分别配间距』（如 tool 行前后也留白），再回到 `arch-design` 做方案对比」。
本轮就是那件事。

## 现有架构

### 模块边界

改动全部落在 `crates/ys-tui-coding`（**不依赖 `ys-runtime`，编译器强制**）：

| 位置 | 职责 | 本轮是否改 |
|---|---|---|
| `src/draw.rs` | 三 pane 布局 + `TranscriptLine` → ratatui `Line` 的渲染管线 + 滚动数学 | **改**（核心） |
| `src/transcript.rs` | `TranscriptLine` 枚举定义；`/resume` 回放建行 | 不改（枚举形状不动） |
| `src/app.rs` | 事件 → transcript 的推入 | 不改 |
| `src/cache.rs` | assistant markdown 的按下标缓存 | 不改 |

`TranscriptLine` 是**结构化**的（`transcript.rs:17`）：`User` / `Assistant` / `Thinking` /
`Tool{…}` / `Summary{…}` / `Error` / `System`。**不是** `#[non_exhaustive]`。

### 核心抽象

- **`Vec<Line<'static>>` 是「屏上行」的唯一中间表示**。两条消费路径共用同一份渲染函数：
  1. **TUI 路径**：`build_text_with`（`draw.rs:554`）→ `draw_transcript` → `Paragraph`。
     同一份 `Text` 还被 `max_scroll_offset` 用 `Paragraph::line_count(width)` 数**视觉行数**。
  2. **退出回放路径**：`transcript_plain_lines`（`draw.rs:737`）→ 写回主屏 scrollback
     （纯文本，assistant 走 raw）。
- **`push_non_assistant`（`draw.rs:614`）** 是两条路径共用的「非 assistant 变体 → 行」函数。
- **`USER_GAP_ROWS = 2`（`draw.rs:56`）**：问答边界，走 `user_gap_rows(item, has_visible_before)`，
  两个渲染循环各调一次。**与本轮正交**（那条管「问答之间」，本轮管「块类型之间」）。

### 关键数据流

```
AgentEvent::{ModelTextDelta / ToolCall / RunFinished …}  →  app.apply_event   ┐
本地提交 begin_local_turn                                  →  app.transcript.push ┤
/resume 回放 replay_lines                                  →  app.transcript      ┘
                                         ↓
                          Vec<TranscriptLine>（唯一数据源）
                                         ↓
                   build_text_with ──┬── draw_transcript → Paragraph（TUI 画）
                                     └── max_scroll_offset → line_count（滚动数学）
                   transcript_plain_lines（退出写回主屏）
```

**关键不变量**：滚动上限与实际画面必须基于**同一份** `Text`。
间距/缩进只要加在两者共用的渲染函数里，`max_scroll_offset` 与 `draw_transcript` **自动**同步；
若在任一侧单独加行，滚动静默错位 —— **这是本改动最需要钉住的点**。

## 约束

- **技术**：`ys-tui-coding` 不得依赖 `ys-runtime`；渲染路径**不得 panic**（绘制期 panic 会把终端
  留在 raw mode）；`push_non_assistant` 的 `Assistant` 分支是防御性降级路径，不得因改动被误触。
- **技术（缓存）**：`md_cache.sync_len(app.transcript.len())` 按 **transcript 下标**缓存 assistant 行。
  间距行不占 transcript 下标 ⇒ 缓存键不受影响，**不得**把空行插进 transcript 本身。
- **技术（拷贝）**：Chat 窗格**只留顶部标题线**（`draw.rs` 的 `chat_block()`），底部用
  `Padding::bottom(INPUT_GAP_ROWS)` 留白；`INPUT_GAP_ROWS` 与 `USER_GAP_ROWS` 的取向都是
  **空行不添视觉噪音、拷贝不留痕**。本轮的新留白沿用该取向。
- **性能**：每条多 1 行 `Line`；`line_count` 成本随行数线性增长，无实质影响。
- **演进**：间距规则必须**只依赖条目类型**，不得依赖 `app` 的回合态（`is_turning` 等）——
  回放侧没有这些状态。
- **字符宽度**：`·` (U+00B7) 的 East Asian Width 是 **A（歧义）** —— CJK 字体占 2 列、ASCII 字体占 1 列。
  新加的符号**不得**用歧义宽度字符，否则不同终端下错位。（状态行现有的 `·` 是历史遗留。）

## 需求范围

### 范围内

- **行动块 = `Thinking` + `Tool`**（模型的「过程」），**正文块 = `Assistant`**（模型的「产物」）。
- 行动块整体**缩进 2 格**并**降低视觉权重**（暗灰），与顶格的正文块区分。
- 行动块内部再分两级：思考行缩进 2、工具行缩进 4（层次：`思考` ⊂ `工具`）。
- **块边界留 1 行空白**：条目「类别」变化时插入 1 行空白（`User` / 行动块 / 正文 之间）。
- 留白与缩进加在**两条渲染路径共用**的位置，保证 TUI 画面、滚动数学、退出回放三者一致。

### 范围外（明确不做的）

- **不改 `USER_GAP_ROWS = 2`**（问答边界）—— 与本轮正交，两条规则叠加。
- **不改 `INPUT_GAP_ROWS`**（Chat 底 → Input 顶边框）。
- 不改 `TranscriptLine` 枚举形状、不改持久化格式。
- **不改「思考默认隐藏」** —— ADR-0025 明确记着「用户明确要求默认不显示思考」，
  本轮只优化 `/thinking on` 后的观感。
- 不引入「可配置间距 / 主题化」—— 常量即可。
- 不动 assistant markdown 渲染与缓存。

### 关键场景

- **场景 1（典型回合）**：`> 提问` / 〔空行〕/ `  ⌁ 思考` / `    ⏺ 工具 ✓` / 〔空行〕/ `正文`。
- **场景 2（思考隐藏）**：`/thinking off` 时 `Thinking` 不产出行 ⇒ 行动块只剩工具行，
  缩进仍为 4（块内层级不因可见性而塌陷）。
- **场景 3（退出回放）**：缩进与留白与 TUI 内一致（`transcript_plain_lines` 走同一套规则）。
- **场景 4（滚动一致性）**：缩进不改变**行数**（只改行内列偏移），留白增加行数 ⇒
  `max_scroll_offset` 必须把留白计入。

### 边界场景

- **首条可见行**：不额外留白（顶部凭空多行）。
- **`/thinking off` 的首条 `Thinking`**：不产出行 ⇒ 其后的 `User` 不应被误判成「非首条」。
  沿用 `user_gap_rows` 已有的「已有可见行」判据。
- **连续的 `Tool`**：属同一行动块，**块内不留白**（工具之间的空行会切碎一串调用）。
- **`Assistant` 尾部带空行**：markdown 渲染可能产出尾部空行 ⇒ 视觉空白多于 1 行。
  **不额外去重**（保持规则简单）。

## 未澄清问题（**已按默认值实现**）

- [x] **思考默认隐藏要不要改？** —— 取**不改**。ADR-0025 记着「用户明确要求默认不显示思考」；
      本轮只优化 `/thinking on` 后的观感。
- [x] **行动块用什么颜色？** —— 取**暗灰**（`Color::DarkGray`）。与既有 thinking 风格一致，
      行动块退到背景里，不抢正文的注意力。（`cyan` 与 assistant 代码块同色，会混淆。）
- [x] **缩进用空格还是制表符？** —— 取**空格**（2 格 / 4 格）。制表符宽度随终端设置变化，
      会让同一份内容在不同终端下错位。
- [x] **符号要不要换？** —— 取**不换**。`⌁` (U+2301) 字形覆盖率低是事实，但**缩进与颜色不依赖字形**，
      换符号解决不了根本问题；符号留着（`⏺` 覆盖率好，`⌁` 缺失时缩进仍能传达类型）。

## 实施结果（2026-10-19）

- 生产代码：`crates/ys-tui-coding/src/draw.rs` —— 常量 `BLOCK_GAP_ROWS = 1` / `ACTION_INDENT = 2` /
  `ACTION_TOOL_INDENT = 2`；枚举 `LineClass`（`Prompt` / `Action` / `Content`）与 `LineClass::of` /
  `LineClass::indent`（缩进的**唯一真相源**）；纯函数 `is_visible`、`user_gap_rows`、
  `block_gap_rows`、`gap_rows`（问答边界 + 块边界之和，两条路径共用）。
  两个渲染循环各调一次 `gap_rows`（`build_text_with` / `transcript_plain_lines`）⇒ 画面、滚动数学、
  退出回放三者一致。`max_scroll_offset` 零改动（留白行经 `build_text` 进入同一份 `Text`，自动计入）。
- 实现中修掉一个**真 bug**：隐藏的思考（`/thinking off`）原先**不留白也不推进状态**的缺失 ——
  它虽不产出行，却仍在自己之前插空白、且仍推进「上一条类别」⇒ 留下**孤立空行**、并多切一个块边界。
  现由 `is_visible` 统一：不可见条目既不留白、也不推进状态。判据**不能**用「实际产出了几行」
  代替（回放侧空 assistant 走 raw 产 1 行、TUI 侧 `md::render("")` 产 0 行）。
- 测试：新增 4 条（`test_block_gap_only_between_different_classes` /
  `test_action_block_indent_levels` / `test_both_render_paths_agree_on_gap_rows` /
  `test_hidden_thinking_does_not_shift_block_boundary`）；更新既有 2 条的契约 ——
  `test_user_gap_rows_before_each_user_but_not_the_first`（`User` 前从 2 行变 3 行 = 块边界 1 + 问答 2）、
  `test_rendered_screen_shows_blank_rows_above_second_user_line`（同上；终端高度 12 → 16，
  因内容高 6 → 7 行，原高度下 `follow` 贴底会把首行滚出视野）。
  6 个变异全部被捕获：①块边界恒返 0；②缩进恒返 0；③思考与工具拆成不同类；
  ④回放路径不算块边界（路径分叉）；⑤`is_visible` 忽略 `thinking_visible`；
  ⑥不可见条目也留白。⑤⑥**首轮未被捕获**（断言只数行数、未钉住确切形状），补强后可捕获。
- 文档回写：`docs/design-final/coding-agent-tui.md` §5（行动块与块边界）、`docs/design.md` §12 测试矩阵「Coding Agent」行。
