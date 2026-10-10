# TUI assistant 文本 Markdown 渲染 — 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据** —— Phase 3 据此合并执行单元并划分波次。
> 任务编号对应 `docs/arch/tui-markdown-render/design.md` §6「交付顺序」的 10 步。

## 上下文引用

- 设计计划：`docs/design-plans/2026-10-10-tui-markdown-render.md`
- 设计：`docs/arch/tui-markdown-render/design.md`（§2 推荐设计、§5 风险、§6 交付顺序）
- 决策：`docs/adr/0030-tui-markdown-render.md`
- 复核：`docs/arch/tui-markdown-render/review.md`

## 基线

- **基线 ref**: `9e0b492faa1ce65108d257269904364db9dfe10d`（备份分支 `sw-backup/2026-10-10-tui-markdown-render`）
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含 git 中显示为 `??` 的新文件）

## 任务清单

### Task 1: crates/ys-tui-coding — md 渲染核心三件套 + 依赖接入 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/`
- **涉及文件**: `crates/ys-tui-coding/Cargo.toml`, `crates/ys-tui-coding/src/lib.rs`, `crates/ys-tui-coding/src/md.rs`, `crates/ys-tui-coding/src/styles.rs`, `crates/ys-tui-coding/src/cache.rs`
- **描述**: 加 `tui-markdown 0.3.10`（`default-features = false`）；`lib.rs` 加 `mod md; mod styles; mod cache;`（**只改这一处，只改这一次**）；`md.rs` 实现 `render(src, width, styles) -> Vec<Line<'static>>`（调 `from_str_with_options` + owned 转换 + **表格窄屏守卫：渲染后量行宽，任一行 > width 即整条退回 raw**）；`styles.rs` `impl tui_markdown::StyleSheet` **逐方法覆写**（以编译器给出的方法表为准，不引 Theme/Palette）；`cache.rs` 实现 `RenderCache`（纯 memo，键 `(槽下标, src.len(), width)`，**`now: Instant` 注入**，`RENDER_THROTTLE = 100ms` 节流，**该条结束时强制重算**）
- **关联测试任务**: Task 2
- **依赖**: 无
- **备注**: **本任务是 design §6 第 1 步的闸门** —— 它同时收敛「`Text` 类型是否统一 / `StyleSheet` 方法名 / `Options` 用法」三件未验证项。必须先跑通 `cargo build -p ys-tui-coding` 再写其余代码。

### Task 2: crates/ys-tui-coding — md 渲染核心测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/md.rs`, `crates/ys-tui-coding/src/styles.rs`, `crates/ys-tui-coding/src/cache.rs`
- **描述**: 在三个新模块内联 `#[cfg(test)] mod tests`，覆盖 N3（元素覆盖与样式映射）、N5（**表格行**窄屏守卫无超宽行，宽度 `1..=80`；**反面**：长段正文须保住 Markdown 样式）、N6（非表格 md 渲染无 `│`）、N7（注入 `now`，1 秒 1000 次 delta ⇒ 渲染 ≤ 11 次 + 条末完整渲染）、N1（最小渲染编译通过）；并**演示** M1 / M2 / M3 / M5 / M6 变异会令用例变红（M1、M6 都作用在 `md.rs` 的守卫上）
- **验证方法**:
  - `cargo build -p ys-tui-coding` — 期望退出 0（N1）
  - `cargo test -p ys-tui-coding` — 期望全 passed、0 failed（含既有 212 项）
  - `cargo clippy --all-targets -p ys-tui-coding` — 期望**除** `md` / `styles` / `cache` 三模块的
    `dead_code` 外无新增 warning。该 `dead_code` 是预期的：U2（Task 3）接线之前，这三个 `pub(crate)`
    API 在本轮内尚无调用者，而 `cargo build` 的分析看不到 `#[cfg(test)]` 内的使用。
    **终局零 warning 由 Task 7 把关**
- **关联 impl**: Task 1
- **依赖**: Task 1

### Task 3: crates/ys-tui-coding — 绘制接线与几何改道 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`, `crates/ys-tui-coding/src/app.rs`
- **描述**: `App` 增 `md_cache: RefCell<RenderCache>` 字段；`draw.rs` 的 `Assistant` 分支改走 `cache.lines_for(..)` → `md::render`（**在 `RefMut` 守卫作用域内 clone 进目标**，见设计 §2.3 借用纪律）；Chat 窗格 `Block::bordered()` → `Borders::TOP`；`max_scroll_offset` 与 `draw_transcript` 的内区**改为共用同一 `Block` 实例的 `inner(area)`**（**不要**逐个改 `saturating_sub(2)` 常数，共 4 站点）；`transcript_plain_lines` 回放改走 raw 路径（不复用 `build_text_with`，注意 `draw.rs` 内 4 个既有测试调用点）
- **关联测试任务**: Task 4
- **依赖**: Task 1（需要 `RenderCache` / `md::render` / `AssistantStyles` 存在）

### Task 4: crates/ys-tui-coding — 绘制接线测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/draw.rs`
- **描述**: 在 `draw.rs` 内联 `mod tests` 补/改：N4（宽度 `1..=80` × {含 CJK 表格, 不含表格} 断言 `Paragraph::line_count(w)` 与逐行折行一致）、N6 的 Chat 内区集成版（渲染非表格 md 后内区无 `│`）、N8（`transcript_plain_lines` 输出含原始 `#` / `**`）、贴底/滚动上限用例（守住几何改道）；并**演示** M4 变异会令用例变红（M1 属 Task 2 的守卫，不在本任务）
- **验证方法**:
  - `cargo test -p ys-tui-coding` — 期望全 passed、0 failed
  - `git diff <基线> -- crates/ys-tui-coding/src/draw.rs` — 期望 4 个 `saturating_sub(2)` 站点确已改道，未被逐个改成常数
- **关联 impl**: Task 3
- **依赖**: Task 3

### Task 5: crates/ys-tui-coding — panic 边界 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/run.rs`
- **描述**: 把终端状态恢复的**字节序列**抽成自由函数（供 `exit_payload` 与 panic hook 共用，`exit_payload` 的既有测试必须保持绿）；`run()` 入口装 panic hook —— hook 内 best-effort 写出恢复字节序列 + `disable_raw_mode()` + `show_cursor()`（每步吞 IO 错误，**绝不二次 panic**），**然后才**调用上游默认 hook；hook 体抽成**可注入恢复动作**的自由函数以便单测（不接触真终端）。panic 后**不让进程带伤续跑**
- **关联测试任务**: Task 6
- **依赖**: 无（独立于 md 渲染；design §6 第 7 步）

### Task 6: crates/ys-tui-coding — panic 边界测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `crates/ys-tui-coding/src/run.rs`
- **描述**: 在 `run.rs` 内联 `mod tests` 补：N9 —— ① 恢复字节序列含 `\x1b[?1049l` / `\x1b[?7h` / `\x1b[?2004l`；② `panic_hook_body` 在注入的恢复动作被调用后仍调用 delegate；③ 既有 `exit_payload_starts_with_terminal_restore` 保持绿
- **验证方法**:
  - `cargo test -p ys-tui-coding run::` — 期望全 passed
  - `cargo test -p ys-tui-coding` — 期望全 passed、0 failed
- **关联 impl**: Task 5
- **依赖**: Task 5

### Task 7: crates/ys-tui-coding — 全量回归与 release 体积 [type=test]
- **涉及目录**: `docs/reports/`
- **涉及文件**: `docs/reports/2026-10-10-tui-markdown-render-verify.md`
- **描述**: 全量回归 + clippy + release 体积对比，结论写进该报告文件
- **验证方法**:
  - `cargo test --workspace` — 期望 0 failed（基线 **975 passed / 7 ignored**，2026-10-10 实跑确认）（N2）
  - `cargo clippy --all-targets` — 期望无新增 warning
  - `cargo build --release` 前后比字节数 — 期望 < +500KB（N10，可推迟）
  - `grep -rn 'tui_markdown::' crates/ys-tui-coding/src/` — 期望只命中 `md.rs` 与 `styles.rs`（R6 可检验形式）
- **关联 impl**: Task 1, Task 3, Task 5
- **依赖**: Task 1, Task 3, Task 5（须在各单元 review 放行之后）
- **MANUAL_ACK_REQUIRED**（Phase 4 报告须原样保留、由用户勾选）:
  - [ ] 在真实终端喂一条 ≥100 KiB 的长回答（含表格），确认滚动与输入无可感卡顿（N11）
  - [ ] 确认 Chat 窗格去掉左右边框后的观感、以及表格极窄时退回 raw 的画面可接受

## 波次计划（Phase 3 计算后回填）

> Phase 3 开始时按 3.2 合并单元并分层，回填于此。

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `crates/ys-tui-coding/Cargo.toml`, `src/lib.rs`, `src/md.rs`, `src/styles.rs`, `src/cache.rs` | 无 | **W0** | deep |
| U2 | T3, T4 | `crates/ys-tui-coding/src/draw.rs`, `src/app.rs` | U1 | **W1** | deep |
| U3 | T5, T6 | `crates/ys-tui-coding/src/run.rs` | 无 | **W1** | deep |
| U4 | T7 | `docs/reports/2026-10-10-tui-markdown-render-verify.md` | U1, U2, U3 | **W2** | shallow |

**合并判定（3.2）**：R1 按文件相交合并 → U1(T1+T2) / U2(T3+T4) / U3(T5+T6)；
U1 与 U2 虽同 crate 但文件集不相交，**不**合并（R2 只针对「≤2 文件的小任务」，
U2 含 `draw.rs` 1500 行的几何改道与缓存接线，非小任务）。R3 通过：各单元 ≤5 文件。

**为什么 U3 不放进 W0**：U3 本身无依赖，但 U1 会改写 `Cargo.toml` / `Cargo.lock`；
与 U3 的 `cargo test` 并发会让后者的依赖解析撞上正在改写的 manifest。
更关键的是 design §6 把第 1 步定为**其余全部任务的闸门** —— W0 只跑闸门，符合该语义。

（同波内文件集必须互不相交；相交则顺延到下一波 —— W1 的 `{draw.rs, app.rs}` 与 `{run.rs}` 不相交 ✅）

## 验证清单

- [ ] 所有 (impl, test) 配对均已 completed —— Task1/Task2、Task3/Task4、Task5/Task6
- [ ] `cargo build -p ys-tui-coding` — 退出 0（N1，闸门）
- [ ] `cargo test -p ys-tui-coding` — 全 passed（含既有 212 项）
- [ ] `cargo test --workspace` — 0 failed（N2，基线 975 passed / 7 ignored）
- [ ] `cargo clippy --all-targets` — 无新增 warning
- [ ] N3–N9 逐一有对应用例且通过；N10 体积已测或显式标为推迟
- [ ] M1–M6 变异义务均已演示「注入 bug ⇒ 用例变红」
- [ ] `grep -rn 'tui_markdown::' crates/ys-tui-coding/src/` 只命中 `md.rs` / `styles.rs`
- [ ] N11 + 观感两项 MANUAL_ACK_REQUIRED 已原样呈报用户
