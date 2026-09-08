# TUI 状态展示 — 架构质量分析报告

## 分析范围

- **对象**：`docs/arch/tui-status-display/design.md` + `adr-tui-status-display.md` 提议的实施方案
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **来源**：design.md + context.md + 对现有源码的事实核查

## 事实核查（先于打分）

| design.md 声明 | 事实 | 影响 |
|----------------|------|------|
| "复用 prompt.rs 的 `~` 缩放" | ❌ prompt.rs **没有** `format_cwd_tilde` 函数，只有 `HOME` 用于拼路径 | design.md 写错了"已实现"语义，需新增而非抽取 |
| "替换 `NoopEventSink` 为 `CollectSink`（结构已留好）" | ⚠️ `Agent` **没有 `set_events` 方法**，且 `CollectingSink` **没有 `clear()`** | 演进路径中的"streaming 状态栏"实际需要扩展 `Agent` 公开 API + 加 `clear()` |
| "不做 `CollectingSink` 包装（v0 不消费 = 死代码）" | ✅ 与现状一致 | 推荐方案采纳 C 的接缝但丢弃实现，决策合理 |
| "`RunResult.usage` / `rounds` / `stop_reason` 已够用" | ✅ `agent-loop/src/result.rs:4-9` 三字段全有 | v0 0 API 扩展可实现 |
| "`Config.cwd` / `provider` 已存在" | ✅ `config.rs:14-16` | 同上 |
| "`Agent::model_id()` 公开可用" | ✅ `agent-runtime/src/agent.rs:81-83` | 同上 |

**2 处事实错误需要修正 design.md**（见末尾「易修复」）。

---

## 各维度判断

### 1. 可行性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 1.1 技术可实现性 | 🟢 | 全部依赖 Rust 标准库（`std::fmt::Write` / `std::path::Path::strip_prefix`）+ agent-core 的 `Usage` + agent-loop 的 `RunResult`。无新依赖，无并发原语，无 IO |
| 1.2 依赖成熟度 | 🟢 | 零新依赖。复用既有 `inquire`、`serde`、`tokio`（Cargo.toml 已有） |
| 1.3 实现周期 | 🟢 | design.md §「实施顺序」给出 6 步增量路径，每步可独立 `cargo test` 验证。预计 ≤ 200 行新增代码、≤ 4 个新单测文件 |

### 2. 可维护性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 2.1 模块边界 | 🟢 | `status.rs`（数据）+ `format.rs`（渲染）+ `tui.rs`（编排）三层职责清晰，单向依赖：`tui.rs → status.rs/format.rs → agent-core/agent-loop`。无循环依赖风险 |
| 2.2 接口稳定性 | 🟢 | **不扩展 crate 公共 API** 是核心决策，避免污染 `Agent` / `Usage` / `StopReason` 契约。新增模块全在 `apps/coding-agent/` 内，崩溃影响面有限 |
| 2.3 测试难度 | 🟢 | `format_tokens` / `status_symbol` / `format_cwd_tilde` / `TurnStats::accumulate` 全部纯函数 + 注入式 `&mut W: Write`，可灌 `Vec<u8>` 测试，无需 mock |
| 2.4 错误传播 | 🟢 | `&mut W: Write` 返回 `io::Result<()>`，错误可向上传播至 `run_interactive` 的 `Result<(), Box<dyn Error>>`。当前 tui.rs 已用相同模式（`io::stdout().flush()?`）|
| 2.5 并发安全 | 🟢 | REPL 单线程，无锁无 channel。`TurnStats` 栈上 owned，`&mut` 借用 |
| 2.6 资源管理 | 🟡 | 🟡 `format_cwd_tilde` 每次 banner 都调 `std::env::var_os("HOME")` + `PathBuf::from(home)` + `strip_prefix`。**每次 turn 不调**（banner 仅启动一次），所以实际不可观测——但需在实现时记得「home 在启动期解析一次后缓存」 |
| 2.7 配置注入 | 🟢 | `print_banner(&mut W, &Config, model_id)` 接受所有依赖为参数，不读全局状态 |

### 3. 可理解性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 3.1 概念一致性 | 🟢 | 术语与 codebase 对齐：`TurnStats`（vs Pi `usage-totals`）/ `RunResult` / `StopReason` / `Usage` 全部沿用现有命名 |
| 3.2 抽象层次 | 🟢 | 数据（status.rs）/ 渲染（format.rs）/ 编排（tui.rs）分层清晰；无 trait object 引入；不存在「过度抽象」 |
| 3.3 文档完整度 | 🟡 | 🟡 context.md / design.md / ADR 三件套齐全；但 `format.rs` / `status.rs` 的 doc comment 规范需在实现时明确（建议每个 pub 函数 1 行 `///`）|
| 3.4 新人上手 | 🟢 | 改动集中在 `apps/coding-agent/`，新人读 3 个新文件 + tui.rs 改动即可理解；`prompt.rs` 复用声明需修正（见事实核查）|

### 4. 性能与可靠性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 4.1 性能模型 | 🟢 | 文本输出量级 < 100 字节/秒；`saturating_add` 一次 / turn；零分配路径（`String::with_capacity(64)` 可选但非必须）。绝对热路径在 LLM 调用（秒级），展示层纳秒级无关紧要 |
| 4.2 故障模式 | 🟢 | 故障域隔离在 `apps/coding-agent/`，不影响 `crates/`；`format.rs` 内部 `writeln!` 失败 → `io::Result` 上抛 → 触发 REPL 退出（与现有 flush 失败同路径） |
| 4.3 可观测性 | 🟡 | 🟡 当前不记录 banner / summary 的输出到日志（仅 stdout）。如果未来加 telemetry，summary 中的 token 数字正是想要的 metric——但 v0 不需要 |
| 4.4 退化策略 | 🟢 | `format_cwd_tilde` 在 HOME 不存在时退化到绝对路径（设计明确）；`(not configured)` 在 provider/model 缺失时退化（设计明确）；`saturating_add` 防止累计溢出（设计明确） |

### 5. 与硬约束的契合度

| 硬约束 | 满足？ | 备注 |
|--------|-------|------|
| 不引入 TUI 框架 | ✅ | 纯 stdin/stdout + `writeln!` |
| 不扩展 crate 公共 API | ✅ | v0 数据全有 |
| v0 字段外不建基建 | ✅ | cache/cost/git/thinking/extension 全部延后 |
| 不做 streaming 状态栏 | ✅ | `NoopEventSink` 保留 |

---

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 缓解 |
|---|------|------|--------|--------|------|
| **R1** | design.md 写错 prompt.rs 复用点，导致实现时找不到现成函数 | 低（修复成本 5 分钟） | 高 | P2 | 在实现前修正 design.md，说明 `format_cwd_tilde` 是**新增**而非抽取 |
| **R2** | "演进路径：替换 `CollectSink`" 实际需要扩展 `Agent` 公开 API（加 `set_events`）+ 给 `CollectingSink` 加 `clear()` | 中（破坏 ADR 中"不扩展 API"决策） | 中 | P1 | ADR 中明确标注："此演进路径触发 `Agent::set_events()` 公开 API 扩展与 `CollectingSink::clear()` 新增" |
| **R3** | `format_cwd_tilde` 在 banner 期重复解析 `HOME` | 极低（一次启动） | 高（实现易错） | P3 | 在 main.rs 启动期解析一次，传 `Option<PathBuf>` 给 tui |
| **R4** | `/status` 命令（commands/builtin.rs:481-521）硬编码展示字段，新增字段需同步两处 | 中（重复维护） | 中 | P1 | design.md 已识别但未列入实施顺序；在「实施顺序」加 step 7：`StatusCommand` 改读 `format.rs` 的 `render_full()` |
| **R5** | `format_cwd_tilde` 在 Windows 上对 `C:\Users\foo` 路径的处理未明确 | 低（Windows 非优先平台） | 中 | P3 | 单测覆盖 `$HOME=/c/Users/foo` 与绝对路径两种情况 |
| **R6** | `writeln!` 失败（stdout closed）时整个 REPL 退出 | 低（与现有 flush 失败同路径） | 低 | P3 | 不修复——保持现有行为一致性 |

---

## 改进建议

### 易修复（低风险快速改进）

1. **修正 design.md 中"复用 prompt.rs"的事实错误**
   - 改为："新增 `prompt.rs::format_cwd_tilde(cwd: &Path) -> String` 函数（首次出现于 coding-agent 内，被 `format.rs` 复用）"
   - 同样修正 ADR

2. **补充设计中的演进路径前置条件**
   - ADR 中"演进路径"表中的每一项加一列「前置 API 扩展」，明确指出哪些会破坏"不扩展 crate API"决策

3. **把 `/status` 命令同步列入实施顺序**
   - design.md §「实施顺序」加 step 7：`StatusCommand` 改读 `format.rs::render_full()`

### 需讨论（需要团队决策）

4. **`HOME` 解析时机**
   - 选项 A：banner 渲染时解析（简单，每次 banner 都查 env）
   - 选项 B：main.rs 启动期解析一次，传 `Option<PathBuf>` 给 tui（更纯，但多 1 个参数）
   - 建议 **B**——体现「不在函数内隐式依赖 env var」的工程原则

5. **`writeln!` 失败是否要继续 REPL**
   - 当前 tui.rs:18 `io::stdout().flush()?` 已选择"失败即退出"
   - 设计保持一致即可，但建议在 ADR 中明示"输出失败等价于 stdout 不可用，整个 REPL 退出"

### 架构级（影响面大，需要跨 phase 规划）

无。当前设计在 v0 范围内已是最简。

---

## 总体评价

### 健康度

**整体🟢 良好**。v0 范围聚焦、实现成本低、演进路径已留接缝。3 个事实性小错误不影响方案方向，但需要在实现前修正以避免开发者读 design.md 时走弯路。

### 最大风险点

**R2（演进路径的 API 扩展前置条件未明示）**——如果未来真要做 streaming 状态栏，会发现"接缝已留好"是错的。需要在 ADR 中诚实标注。

### 推荐下一步

1. 按 P1 优先级修正 design.md + ADR（5 分钟工作量）
2. 按 design.md §「实施顺序」6 步实现，每步 `cargo test`
3. 实现完毕后跑 `cargo clippy --all-targets` + 手动验证 3 种 stop_reason
4. 在 PR 中标注「实现期间若发现 R1-R6 任一发生，回退到本 review 修正」

### 与 Pi 路径的契合度

设计借鉴了 Pi 的 `format_tokens` / `~` 缩放 / 累加器独立于 `Usage` 三点，但克制地丢弃了 cache/cost/git/thinking/extension 等 5 项基建——这一克制正是 v0 阶段最重要的判断。设计文档对此论述清晰，团队决策记录（ADR）也明示了否决理由。

**建议**：实施此方案。
