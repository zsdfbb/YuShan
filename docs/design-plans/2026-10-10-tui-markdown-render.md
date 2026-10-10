# TUI assistant 文本 Markdown 渲染 — 设计计划

> 设计来源：`docs/arch/tui-markdown-render/design.md`（§6 交付顺序）、
> `docs/adr/0030-tui-markdown-render.md`（决策 1–8）、
> `docs/arch/tui-markdown-render/review.md`（复核结论 A1–A5 / B1–B2）。
> 本计划**不重新设计**，只把已定稿的设计转成可并发调度的任务分解。
> slug：`2026-10-10-tui-markdown-render`

## 背景

- **当前状态**：`ys-tui-coding` 把每条 `TranscriptLine::Assistant(String)` 按显式 `\n` 切行、
  整行单色渲染（`crates/ys-tui-coding/src/draw.rs:445-450`）—— 模型输出里的 `# 标题`、
  `**粗体**`、围栏代码、GFM 表格**原样当字符显示**。
- **为什么改**：coding agent 的主要产出就是 Markdown 文本，把它当纯字符显示是对话区可读性的主要短板。
  本轮把 assistant 文本升级为**结构化 Markdown 渲染（不做语法高亮、不引 Theme）**，
  并把「拷贝友好」（终端原生选择不再把窗格竖线拷进剪贴板）作为横切原则一并落地。
- **前置条件已完成**：`ratatui 0.29 → 0.30` + `crossterm 0.28 → 0.29` + `run.rs` 8 处
  `Backend<Error = std::io::Error>` bound 已落地并编译实证（commit `87bd1ec`）。
  **库集成（本方案的核心动作）尚未编译验证** —— 本计划 Task 1 即该验证的闸门。

## 设计

### 方案要点

1. **依赖**：`tui-markdown = { version = "0.3.10", default-features = false }`。
   `default-features = false` 关掉 `highlight-code` ⇒ `syntect` / `ansi-to-tui` / `onig`（C 库）全不进入。
   **不改** ratatui / crossterm 版本（已在前置升级中定稿）。
2. **渲染管线**：`Assistant(src)` → `md::render(src, inner_w, AssistantStyles)` → `Vec<Line<'static>>`
   → `Paragraph::new(..).wrap(Wrap { trim: false })`。
3. **表格窄屏守卫内置在 `md::render` 内**：渲染后量每行显示宽度，**任一行 > `width` 即整条退回 raw**
   （原样按 `\n` 切行）。返回值因此**已是最终产物**，缓存键仍只需 `(槽下标, 内容长度, 宽度)`。
   这样 `draw.rs` 不必再分支出第二条路径，守卫也可被单测直接打到。
4. **样式映射点 = `styles.rs` 的一个 `impl tui_markdown::StyleSheet`**（编译期 trait，非主题系统）。
   `DefaultStyleSheet` 自带主观配色 ⇒ **必须逐方法覆写**（约十余个，多为一行），否则默认色漏进来。
5. **缓存**：`App.md_cache: RefCell<RenderCache>`，键 `(槽下标, src.len(), width)`，
   **只覆盖 `Assistant` 槽位**；其余变体每帧现算（O(1) 格式化）。
6. **流式节流**：`RENDER_THROTTLE = 100 ms`。键含 `src.len()` ⇒ 增长中的那条每来一个 delta 就整条重解析，
   缓存对它收益为零，故必须节流。两条边界：① 节流状态**存在 `RenderCache` 内**，
   令 `total_rows` 与 `draw_transcript` 读**同一份**上次产物；② **该条结束时必须重算**
   （新 delta 落到下一条 / turn 结算），否则尾部增量永久不显示。
7. **几何改道**：Chat 窗格 `Block::bordered()` → `Borders::TOP`（拷贝友好）；
   `max_scroll_offset` 与 `draw_transcript` 的内区改为**共用同一 `Block` 实例的 `inner(area)`**，
   **不**逐个去改 `saturating_sub(2)` 常数（4 个站点，其中 `inner_h` 漏改编译器不抓）。
8. **panic 边界**：`run()` 入口装 panic hook —— 恢复终端（best-effort）后再转交上游默认 hook。
   选 hook 而非 `catch_unwind` 包 `md::render`：前者覆盖全部绘制路径。
9. **回放走 raw 路径**：`transcript_plain_lines` 不再复用 `build_text_with`，主屏 scrollback 留
   **原始 markdown 源**（可复制、可搜索）。`/copy` 本就读原始串，**不改**。

### 架构决策（已由 ADR-0030 定案，本计划不复议）

- 用现成库而非自研；升级 ratatui 0.30 换取表格支持；样式映射点抽成独立 `styles.rs`；
  拷贝友好为横切原则；表格保留框线（有意接受的例外）。

### 数据流

```
ModelTextDelta ─► App::apply_event ─► append_stream(Assistant)        app.rs:147,293
                          │
            每帧 run.rs:155 terminal.draw(|f| draw::ui(f, app))
                          ▼
   draw_transcript → build_text_with(app, inner_w, thinking)          draw.rs:111,414
        └─ Assistant(s) ⇒ cache.lines_for(i, s, inner_w, styles, now)
                            └─ 未命中 ⇒ md::render(s, inner_w, styles)
        └─ Paragraph::new(text).wrap(Wrap{trim:false}).scroll(..)     draw.rs:124-128
   total_rows → Paragraph::line_count(inner_w)                        draw.rs:102-106
```

**宽度传递注意**：库的 `table_width` 语义是「该表格可用的总列数（含边框、内边距、外层列表/引用前缀）」
⇒ 传 `inner_w`，库在 `end_table` 自行扣除嵌套前缀。

### 接口变更

- **新增**（crate 内部，`pub(crate)`，不进 `lib.rs` 的 `pub use`）：
  - `md::render(src: &str, width: usize, styles: AssistantStyles) -> Vec<Line<'static>>`；
  - `styles::AssistantStyles`（`impl tui_markdown::StyleSheet`，须 `Clone + Send + Sync + 'static`）；
  - `cache::RenderCache` 及其 `sync_len` / `lines_for`。
- **`App` 新增私有字段** `md_cache: RefCell<RenderCache>`。
- **现有公开 API（`lib.rs` 的 `pub use`）不变** —— `md` / `styles` / `cache` 均为私有模块，
  声明为 `mod md; mod styles; mod cache;`（与既有 `mod draw; mod run;` 同形）。
- **`RenderCache::lines_for` 的时钟必须可注入**：签名为
  `lines_for(&mut self, idx: usize, src: &str, width: usize, styles: AssistantStyles, now: Instant) -> &[Line<'static>]`，
  内部**不得**直接调用 `Instant::now()` —— 否则 §2.5 的节流无法确定性单测（N7）。
  这是设计 §2.3 签名在实现层面的必要细化，不改变设计的任何决策。
- **借用纪律（设计 §2.3，必须遵守）**：`lines_for` 返回切片的生命周期**绑在 `RefMut` 守卫上**，
  唯一正确写法是在守卫作用域内 clone 进目标：
  ```ignore
  { let mut c = app.md_cache.borrow_mut();
    out.extend(c.lines_for(i, s, w, styles, now).iter().cloned()); }
  ```
  同一 `RefCell` **不得重入** `borrow_mut()`。

## 涉及文件

- **新增**：
  - `crates/ys-tui-coding/src/md.rs` —— 调库渲染 + owned 转换 + 表格窄屏守卫
  - `crates/ys-tui-coding/src/styles.rs` —— `impl tui_markdown::StyleSheet`（本仓配色）
  - `crates/ys-tui-coding/src/cache.rs` —— `RenderCache`（纯 memo + 流式节流）
- **修改**：
  - `crates/ys-tui-coding/Cargo.toml` —— 新增 `tui-markdown` 依赖
  - `crates/ys-tui-coding/src/lib.rs` —— 三个 `mod` 声明（**只改这一次**）
  - `crates/ys-tui-coding/src/draw.rs` —— Assistant 分支走缓存 + md 渲染、几何改道、回放 raw
  - `crates/ys-tui-coding/src/app.rs` —— `md_cache` 字段
  - `crates/ys-tui-coding/src/run.rs` —— panic hook + 恢复序列抽取
- **删除**：无

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。
> 合同由用户签字确认（Phase 1 已放行）。

#### N1: 依赖接入 + 最小渲染跑通
- **验收方法**: `cargo build -p ys-tui-coding` — 期望退出 0；`grep -c 'tui-markdown' Cargo.lock` — 期望 ≥ 1
- **优先级**: 必测
- **验收人**: review subagent

#### N2: 不回归
- **验收方法**: `cargo test --workspace` — 期望 0 failed（基线 **975 passed / 7 ignored**，
  2026-10-10 实跑确认）
- **优先级**: 必测
- **验收人**: review subagent

#### N3: 元素覆盖（标题 / 粗体 / 斜体 / 表格 / 列表 / 引用 / 围栏码 / 行内码 / 链接）
- **验收方法**: 单测 —— 固定 md 输入 → 断言产出 `Span` 的 `Modifier` / 前景色符合 `styles.rs` 的映射
- **优先级**: 必测
- **验收人**: review subagent

#### N4: `line_count(w)` 与渲染视觉行数一致（含 CJK + 表格）
- **验收方法**: 参数化单测 —— 宽度 `1..=80` × {含 CJK 表格, 不含表格}，
  断言 `Paragraph::line_count(w)` == 逐行累计折行数
- **优先级**: 必测
- **验收人**: review subagent

#### N5: 表格窄屏守卫：无超宽行
- **验收方法**: 单测 —— 宽度 `1..=80` × 若干表格，断言 `md::render` 产出的每行 `Line::width() <= w`
  （越界时已退回 raw，同样须满足）
- **优先级**: 必测
- **验收人**: review subagent

#### N6: 拷贝友好：Chat 内区无 `│`
- **验收方法**: 单测 —— 渲染非表格 md，断言各行不含 `│`（表格框线为有意例外）
- **优先级**: 必测
- **验收人**: review subagent

#### N7: 流式节流：窗口内重渲染次数有上限
- **验收方法**: 单测 —— 注入 `now`，模拟 1 秒内 1000 次 delta（每次 `src` 增长），
  断言渲染调用 **≤ 11 次**；且**该条结束时必有一次完整渲染**
- **优先级**: 必测
- **验收人**: review subagent

#### N8: 回放走 raw（主屏留原始 markdown 源）
- **验收方法**: 单测 —— `transcript_plain_lines` 输出含原始 `#` / `**`
- **优先级**: 必测
- **验收人**: review subagent

#### N9: panic 恢复载荷正确 + hook 转交上游
- **验收方法**: 单测 —— ① 恢复**字节序列**自由函数含 `\x1b[?1049l`（退 alt-screen）、
  `\x1b[?7h`（开换行）、`\x1b[?2004l`（关括号粘贴）；② `panic_hook_body` 在注入的恢复动作被调用后
  **仍调用** delegate；③ `disable_raw_mode()` 是**系统调用**（非字节序列），不在字节断言内 ——
  由 review subagent 代码走查确认它排在恢复序列之后
- **优先级**: 必测
- **验收人**: review subagent

#### N10: release 体积增量 < +500KB
- **验收方法**: `cargo build --release` 前后比字节数（基线 ~5.7MB）
- **优先级**: 可推迟
- **验收人**: review subagent

#### N11: 长流式消息观感无卡顿
- **验收方法**: 人工 —— 在真实终端喂一条 ≥100 KiB 的长回答（含表格），观察滚动与输入延迟
- **优先级**: MANUAL_ACK_REQUIRED（**机制**已由 N7 机器覆盖，此项只验观感）
- **验收人**: 用户

### 变异义务（本仓硬约定，review 时以「注入 bug → 测试应红」验证）

| # | 注入的 bug | 应变红的用例 |
|---|---|---|
| M1 | 关掉表格窄屏守卫 | 超宽用例（N5）|
| M2 | 让缓存永不失效 / 键去掉 `width` | 流式增长 + resize 用例（N7、N4）|
| M3 | 漏覆写一个 `StyleSheet` 方法 | 默认色泄漏用例（N3）|
| M4 | `Borders::TOP` 后把内区改回 `saturating_sub(2)` | 贴底 / 滚动上限用例（N4）|
| M5 | 摘掉节流，改回逐 delta 重渲染 | 「窗口内调用次数 ≤ 11」与「条末完整渲染」用例（N7）|

### 测试框架与命令

- 单元测试（内联 `#[cfg(test)] mod tests`，本 crate **无 dev-dependencies**）：
  `cargo test -p ys-tui-coding`
- 全量回归：`cargo test --workspace`
- Lint：`cargo clippy --all-targets`
- 编译闸门：`cargo build -p ys-tui-coding`
- **禁止裸 `cargo fmt`** —— 本机 rustfmt 与该仓库 CJK 文本有既有分歧，会污染无关文件。

## 波及文档

- `docs/design-final/coding-agent-tui.md` —— **须更新**（Phase 4）：assistant markdown 渲染、
  Chat 窗格几何改道、panic hook、流式节流四项 as-built 形态。
- `docs/design-final/` 下其余 7 篇（`compaction` / `core-channel` / `search-tools` /
  `session-default-new` / `skills` / `tools-web` / `truncation-guard`）与本变更**无关**。
- 三处「已砍」文档（`gap-closure/context.md`、`tui-stack-revisit/design.md`、
  `tui-pi-codewhale-borrow/design.md`）的红线反转回改**已在设计阶段完成**（各文件内已有 2026-10-09 修订注），
  本轮**无需再动**。

## 风险与注意事项

1. **Task 1 是闸门**：`StyleSheet` 方法名 / `Options` builder 用法 / `Text` 类型统一三者
   **均来自阅读源码，未经编译**。`cargo build -p ys-tui-coding` 未过之前，
   **不得**宣称「库集成已验证」（ADR-0030 Consequences 的口径已按此收窄）。
2. **`lib.rs` 只由 Task 1 触碰一次**（三行 `mod` 声明）—— 保持单一所有者，
   否则 Phase 3 会因文件相交把无关任务合并成一个大单元。
3. **`RefCell` 借用纪律**（设计 §2.3）：返回值生命周期绑在 `RefMut` 上；不得重入 `borrow_mut()`。
4. **时钟注入**：`RenderCache` 内不得直接调 `Instant::now()`，否则 N7 无法确定性验证。
5. **`tui_markdown::` 路径只准出现在 `md.rs` / `styles.rs`**（R6 缓解的可检验形式，可用 grep 断言）。
6. **注释只写当前设计** —— 不写变更叙事、不拿竞品做法当理由；文档 / 注释 / commit message 用中文，
   代码标识符用英文。
7. **依赖方向不变**：仍不依赖 `ys-runtime`；UI 线程仍无 runtime 上下文；`ys-protocol` 仍零 tokio。
8. **库缺陷备忘（0.3.10 实测）**：4 空格缩进式代码块会把多行内容合并成一行（围栏式不受影响）。
   用例只需钉住围栏式正常。
