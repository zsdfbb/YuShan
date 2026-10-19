# TUI 回合间距（tui-turn-spacing）— 架构上下文

## 概述

让 TUI 对话区里**每条用户输入之前**留出**两个空白行**，把「上一回合的产出（assistant 文本 / 工具行 / 回合摘要）」
与「本轮用户提问」在视觉上分开，便于人眼扫读多轮对话。

## 现有架构

### 模块边界

改动全部落在 `crates/ys-tui-coding`（**不依赖 `ys-runtime`，编译器强制**）：

| 位置 | 职责 |
|---|---|
| `src/draw.rs` | 三 pane 布局 + `TranscriptLine` → ratatui `Line` 的渲染管线 + 滚动数学 |
| `src/transcript.rs` | `TranscriptLine` 枚举定义；`/resume` 回放建行（`replay_lines`） |
| `src/app.rs` | 事件 → transcript 的推入（`apply_event` / `begin_local_turn`） |
| `src/cache.rs` | assistant markdown 的**按下标**缓存（`md_cache`） |

`TranscriptLine` 是**结构化**的（`transcript.rs:21`）：`User` / `Assistant` / `Thinking` / `Tool{…}` /
`Summary{rounds,stop,elapsed_secs}` / `Error` / `System`。**条目之间目前没有任何空行** ——
`build_text_with` 逐条调 `push_transcript_line`，后者把行直接 `extend` / `push` 进 `Vec<Line>`。

### 核心抽象

- **`Vec<Line<'static>>` 是「屏上行」的唯一中间表示**。两条消费路径共用同一份渲染函数：
  1. **TUI 路径**：`build_text_with`（`draw.rs:461`）→ `draw_transcript`（`draw.rs:153`）→ `Paragraph`。
     同一份 `Text` 还被 `max_scroll_offset` 用 `Paragraph::line_count(width)` 数**视觉行数**（滚动单位）。
  2. **退出回放路径**：`transcript_plain_lines`（`draw.rs:623`）→ 写回主屏 scrollback（纯文本、assistant 走 raw）。
     `Assistant` 一条 `\n` 原样切行，其余变体复用 `push_non_assistant`。
- **`push_non_assistant`（`draw.rs:500`）** 是两条路径共用的「非 assistant 变体 → 行」函数 —— 间距若加在这里，
  两处自动一致；若只加在 `build_text_with` 的循环里，回放会与 TUI 分叉。
- **`INPUT_GAP_ROWS = 1`（`draw.rs:48`）**：Chat 内区底部与 Input 顶边框之间的空白行，走
  `chat_block()` 的 `Padding::bottom`，**不在 transcript 内部**。它解决的是「上轮输出不贴输入框」，
  与本次需求（transcript **内部**的问答边界）正交，不改。

### 关键数据流

```
AgentEvent::{ModelTextDelta / ToolCall / RunFinished …}   →  app.apply_event     ┐
本地提交 begin_local_turn                                  →  app.transcript.push ┤
/resume 回放 replay_lines                                  →  app.transcript      ┘
                                         ↓
                          Vec<TranscriptLine>（唯一数据源）
                                         ↓
                   build_text_with ──┬── draw_transcript → Paragraph（TUI 画）
                                     └── max_scroll_offset → line_count（滚动数学）
                   transcript_plain_lines（退出写回主屏）
```

**关键不变量**：滚动上限与实际画面必须基于**同一份** `Text`（`draw.rs:73-78` 的帧时钟注释）。
间距只要加在两者共用的渲染函数里，`max_scroll_offset` 与 `draw_transcript` **自动**同步；
若在任一侧单独加行（例如只在 `draw_transcript` 里塞空行），滚动静默错位 —— **这是本次改动最需要钉住的点**。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| `ratatui` | `Line` / `Text` / `Paragraph` / `Line::from("")` | workspace | 空白行 = `Line::from(vec![])` 或 `Line::from("")`，宽 0 高 1 |
| — | — | — | 本次**不引入**新依赖 |

## 约束

- **技术**：`ys-tui-coding` 不得依赖 `ys-runtime`；渲染路径**不得 panic**（绘制期 panic 会把终端留在 raw mode，
  `draw.rs:517` 注释）；`push_non_assistant` 的 `Assistant` 分支是防御性降级路径，不得因改动被误触。
- **技术（缓存）**：`md_cache.sync_len(app.transcript.len())` 按 **transcript 下标**缓存 assistant 行
  （`draw.rs:490`）。间距行不占 transcript 下标 ⇒ 缓存键不受影响，**不得**把空行插进 transcript 本身。
- **性能**：每条 User 多 2 行 `Line`，一屏新增可视行数 ≤ 2 × User 条数，忽略不计；`line_count` 成本随行数线性增长，无实质影响。
- **演进**：`TranscriptLine` 是 `pub` 枚举、TUI 与回放共用 ⇒ 间距规则必须**只依赖条目类型**（是否 `User`、
  `out` 是否为空），不得依赖 `app` 的回合态（`is_turning` 等）—— 回放侧没有这些状态（`transcript.rs:87-104`）。
- **组织**：单 crate 局部改动；无对外协议改动、无持久化格式改动（间距**不落盘**）。

## 需求范围

### 范围内

- 每条 `User` 条目**之前**插入**两个空白行**（= 2 个空 `Line`，即用户行的上一行之上还有两行视觉空白）。
- 间距规则加在**两条渲染路径共用**的位置，保证 TUI 画面、滚动数学、退出回放三者一致。
- 会话**第一条可见**用户输入之前**不加**空白（避免开局白占 2 行）。
- 新增/更新测试：钉住间距行数、钉住滚动数学已含间距、钉住回放与 TUI 同形。

### 范围外（明确不做的）

- **不给 `Assistant` / `Tool` / `Summary` / `Thinking` / `Error` / `System` 之间加间距** —— 保持紧密。
- 不动 `INPUT_GAP_ROWS`（Chat 底部到 Input 边框的空白）与三 pane 布局。
- 不改 `TranscriptLine` 枚举形状、不改持久化格式、不动 assistant markdown 渲染与缓存。
- 不做「可配置间距」「主题化间距」—— 常量即可。

### 关键场景

- **场景 1（成功路径）**：多轮问答。第 2 轮起，`> 用户输入` 上方恒有 2 行空白；`✓ N rounds · X.Xs` 摘要行
  与下一轮 `>` 之间即这 2 行。一屏能正常显示、滚动贴底（`follow`）时新提问不被挤出视野。
- **场景 2（会话首条）**：transcript 首条是 `User`（或首条可见行就是它）时，之前**无**空白行。
- **场景 3（退出回放）**：退出时写回主屏的完整对话，问答边界与 TUI 内一致（同样 2 行）。
- **场景 4（滚动一致性）**：内容超过一屏后可上滚，`max_scroll_offset` 把间距行计入视觉行数（否则末行会隐性截断）。

### 边界场景

- **首条被隐藏的 `Thinking`**：`/thinking off` 时 `Thinking` 条目不产出行。若规则写成「`idx > 0` 就加空白」，
  则「隐藏 thinking + 首条 User」会在顶部多出 2 行空白。⇒ 规则宜取 **`out` 当前非空**（已有可见行）才加空白。
- **相邻两条 `User`**：回合中插话（steering，`app.rs:220`）或排队回声（`begin_local_turn`，`app.rs:335`）
  会造成两条 `User` 相邻。按「非首条可见即加」的规则，二者之间也会有 2 行空白 —— 待确认（见下）。
- **末条 `User` 尚无产出**：刚提交时 `> 问题` 之后紧接 `Working` 状态行（状态行在 pane 底部，不属 transcript），
  2 行空白在 `>` **上方**，不受影响。
- **`Assistant` 尾部带空行**：markdown 渲染若产出尾部空行，视觉空白会多于 2 行。**不额外去重**（保持规则简单）。

## 未澄清问题（**已按默认值实现**，2026-10-12）

- [x] **相邻两条 `User` 之间是否也要 2 行空白？** —— 取**要**（规则统一为「每条非首条可见 `User` 之前恒 2 行」），
      不按「上一条是不是 `User`」特判。理由：特判会多一条分支、且插话场景下「两条用户行紧贴」并不比留白更易读。
- [x] **空白行是否含样式？** —— 取**纯空白行**（`Line::from("")`，无样式），与 `INPUT_GAP_ROWS` 同形
      （不添视觉噪音、拷贝不留痕）。
- [x] **退出回放的 assistant raw 路径下空行插在哪？** —— 取**插在 `wrap_plain` 之前**（落在 `out` 上），
      即「先空行、后内容」；空行经 `wrap_plain` 仍是 1 行空串。

## 实施结果（2026-10-12）

- 生产代码：`crates/ys-tui-coding/src/draw.rs` —— 常量 `USER_GAP_ROWS = 2`、纯函数
  `user_gap_rows(item, has_visible_before) -> usize`、两个渲染循环各一处调用（`build_text_with` / `transcript_plain_lines`）。
  `max_scroll_offset` 零改动（复用同一份 `build_text`，间距自动计入）。
- 测试：新增 3 条（`test_user_gap_rows_before_each_user_but_not_the_first` /
  `test_user_gap_is_counted_in_scroll_math` / `test_rendered_screen_shows_blank_rows_above_second_user_line`），
  两条变异均被三个用例同时抓到（①`user_gap_rows` 恒返 0；②去掉 `has_visible_before` 判据）。
- 文档回写：`docs/design-final/coding-agent-tui.md` §5、`docs/design.md` §12 测试矩阵「Coding Agent」行。

## 后续建议

- `arch-design` 的多方案对比在本需求上收益有限（单点小改），未启用。
- 若要进一步做「按块类型分别配间距」（如 tool 行前后也留白），再回到 `arch-design` 做方案对比。
  —— **已落地**：见 `docs/arch/tui-action-blocks/context.md`（行动块缩进降色 + 块边界留白）。
