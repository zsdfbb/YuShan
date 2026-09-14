# Coding Agent TUI（路线 B）执行计划

> 参考设计计划：`docs/design-plans/2026-09-14-coding-agent-tui.md`
> 决策基线：`docs/arch/coding-agent-tui/design.md` + `review.md`
> 用户的编排授权：full，**直到完成不再逐任务确认**。

## 上下文引用

- 设计详案（形态、协议形状、app 侧循环、诊断三分）：`docs/arch/coding-agent-tui/design.md` §1–§9
- 质量分析与 R1–R7：`docs/arch/coding-agent-tui/review.md`
- 现有实现坐标：`apps/coding-agent/src/{ui/*,main,wiring,channel,view,status,format,commands/*}.rs`

## 执行纪律

1. **A 阶段是一个原子改动**：`CancelToken`/`Inbox` 的移除会同时破坏 `ys-core`/`ys-tool`/`ys-component`/`ys-loop`/`ys-runtime`/`tools-basic`，中间态不可编译。A1–A5 必须由**同一个实现 subagent** 一次改完。
2. B/C 阶段任务按依赖顺序串行。
3. 每个 `type=impl` 任务必须写测试并跑通；review subagent 独立复核。

## 任务清单

### Task 1: crates/ys-protocol + 库侧原子清理 [type=impl]
- **涉及目录**: `crates/ys-protocol/`, `crates/ys-core/`, `crates/ys-component/`, `crates/ys-tool/`, `crates/ys-loop/`, `crates/ys-runtime/`, `adapters/tools-basic/`, `Cargo.toml`
- **涉及文件**:
  - 新增 `crates/ys-protocol/src/{lib,request,boundary,outbound,boundary_source,envelope,lifecycle}.rs` + `Cargo.toml`
  - 删除 `crates/ys-channel/`（整目录）+ workspace members 条目
  - 改 `crates/ys-core/src/{lib.rs,cancel.rs}`（删 `CancelToken`）
  - 改 `crates/ys-component/src/context.rs`（`cancel` 去、`inbox`→`boundary: Option<&'a dyn BoundarySource>`、`with_inbox`→`with_boundary`）
  - 改 `crates/ys-tool/src/context.rs`（`cancel`→`boundary: Option<&'a dyn BoundarySource>`）
  - 改 `crates/ys-loop/src/basic.rs`（轮边界 `ctx.boundary.take()` 匹配 `Steer`/`Abort`；取消检查改 `is_aborted()`；`ToolContext` 构造改传 boundary）
  - 改 `crates/ys-runtime/src/{agent,builder,prelude,lib}.rs`（删 `cancel`/`cancel_handle`/`cancel_token()`/`run`/`run_one_run`→`run`/`RunSummary`；`AgentPorts` 加 `boundary`）
  - 改 `adapters/tools-basic/src/{bash,read,write,edit}.rs` 测试构造
  - 改各 crate 的 `Cargo.toml` 依赖 `ys-channel` → `ys-protocol`
- **描述**: 新建 `ys-protocol`（零 tokio），把 `Envelope`/`Source`/`LifecyclePolicy` 迁入；删除 `ys-channel`；移除 `CancelToken` 与 `Inbox` 家族；`RuntimeContext`/`ToolContext` 改用 `BoundarySource`；`BasicLoop` 轮边界改写；`Agent::run` 删除、`run_turn` 唯一入口。**一次改完，保证 `cargo build --workspace` 通过。**
- **关联测试任务**: T2, T3, T4

### Task 2: crates/ys-protocol — 协议与 BoundarySource 测试 [type=test]
- **涉及目录**: `crates/ys-protocol/src/`
- **涉及文件**: `crates/ys-protocol/src/{request,boundary,outbound,envelope,lifecycle,boundary_source}.rs` 内嵌 `#[cfg(test)] mod tests`
- **描述**: `Request`/`Boundary`/`Outbound` 的构造与 serde roundtrip；`Envelope`/`Source`/`LifecyclePolicy` 迁移过来的 5 个测试；`BoundarySource` 契约（`take` 保序、`Steer` 不丢、`Abort` 置位后 `is_aborted()` 立即可见、abort 后仍能取到先前排队的 Steer）
- **验证方法**:
  - `cargo test -p ys-protocol` — 期望全部通过（≥ 15 passed）
  - `cargo clippy -p ys-protocol --all-targets` — 期望无 warning
- **关联 impl**: T1

### Task 3: crates/ys-loop + ys-runtime — 轮边界与 actor 语义测试 [type=test]
- **涉及目录**: `crates/ys-loop/src/`, `crates/ys-runtime/tests/`
- **涉及文件**: `crates/ys-loop/src/lib.rs`（测试模块）、`crates/ys-runtime/tests/actor_run.rs`（重写）
- **描述**: 轮中 `Steer` 注入进 `ModelRequest`（原 `SteeringProbeModel` 改写为 `BoundarySource` 驱动）；`Abort` → `StopReason::Cancelled`；工具轮询 `is_aborted()` 提前收场；`actor_run.rs` 改由 `run_turn` + `BoundarySource` 驱动，保住多回合 / steer / abort 场景（`QueueMode::All` 合并语义按设计移除）
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全部通过
  - `cargo test -p ys-runtime` — 期望全部通过
  - **变异测试**：把 `BasicLoop` 的 `take()` 改成丢弃 `Steer` → `cargo test -p ys-loop` 必须变红
- **关联 impl**: T1

### Task 4: adapters/tools-basic — 取消信号源测试 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/{bash,read,write,edit}.rs`
- **描述**: 四个工具的测试构造改用 `BoundarySource`（或 `None`）；`BashTool` 的 `is_aborted()` → kill 路径有一条断言
- **验证方法**:
  - `cargo test -p ys-tools-basic` — 期望全部通过
- **关联 impl**: T1

### Task 5: crates/ys-tui-coding — crate 骨架 + CodingView + run() + 三 pane 渲染 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/`
- **涉及文件**: `Cargo.toml`, `src/{lib,view,run,draw,format}.rs`
- **描述**: 新建 crate（依赖 `ys-protocol`、`ys-core`、`ys-event`、`ratatui 0.29`、`crossterm 0.28`（`event-stream`）、`tokio`（`sync` 只要 mpsc 类型）、`serde`；**不得依赖 `ys-runtime`**）。`CodingView` 字段：cwd / provider / model / logged_in_providers / providers / available_models / total_input_tokens / total_output_tokens / turn_count / session_started / message_count / tools / context_window / session_path / is_first_run。`run(view₀, rx_out, tx_req, tx_boundary)` 同步阻塞循环（`crossterm::event::poll(50ms)` → 按键 → `rx_out.try_recv()` 排空 → 300ms 节拍重绘）；alt-screen 建立 + 退出恢复（含把完整对话打到主屏 scrollback）。三 pane 布局 Chat `Min(3)` / Input `Length(1..=N)` / Status `Length(1)`。Chat 渲染：user `> `（绿）、assistant **真 wrap**（wrap 后行数参与滚动计算）、工具调用压一行 `⏺ name 摘要 ✓/✗`（失败显示结果首行）、turn 摘要符号按 `StopReason`、错误行红、thinking 默认隐藏。
- **关联测试任务**: T6, T7
- **依赖**: T1

### Task 6: crates/ys-tui-coding — 渲染 TestBackend 测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `src/draw.rs` 内嵌测试模块
- **描述**: 三 pane 自上而下；Status 为底部 `Length(1)` 且不随 Chat 滚动移动；assistant wrap 后可见行数与滚动一致（宽窄两种宽度）；工具调用单行渲染；失败工具显示结果首行；turn 摘要 `✓`/`⚠`/`✗` 符号；错误行；空 transcript 占位
- **验证方法**:
  - `cargo test -p ys-tui-coding draw` — 期望全部通过
  - **变异测试**：让 assistant 不做 wrap（还原 `draw.rs:176-179` 的简化）→ 滚动断言必须变红
- **关联 impl**: T5

### Task 7: crates/ys-tui-coding — 状态行四档宽度 + Working 动画 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `src/draw.rs` 内嵌测试模块
- **描述**: 宽屏 `model · ↑1.2k ↓345 · 2 轮 · 1m23s`；中屏去时长；窄屏去轮数；极窄仅 model；turn 中 model 段换 `⏳ Working·`；`format_tokens` 的 k/M 进位
- **验证方法**:
  - `cargo test -p ys-tui-coding status` — 期望全部通过
- **关联 impl**: T5

### Task 8: crates/ys-tui-coding — 输入区（多行 + 粘贴 + 中文退格） [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `src/{input,events}.rs`
- **描述**: 多行输入（`Enter` 提交 / `Shift+Enter`、`Alt+Enter` 换行）；高度自适应 1..N；bracketed paste（`EnableBracketedPaste` + `Event::Paste` 追加到输入缓冲区，不被当多次提交）；`PushKeyboardEnhancementFlags`；**中文退格修复**（cursor 以 `char` 边界推进/回退，退格删整个字符）
- **关联测试任务**: T9
- **依赖**: T5

### Task 9: crates/ys-tui-coding — 输入区测试 + 变异测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `src/{input,events}.rs` 内嵌测试模块
- **描述**: 输入「中文」后退格一次不 panic 且删除整个字符；光标停在字符边界；多行粘贴（含 `\n`）只进缓冲区不提交；`Shift+Enter`/`Alt+Enter` 换行、`Enter` 提交
- **验证方法**:
  - `cargo test -p ys-tui-coding input` — 期望全部通过
  - **变异测试**：把退格改回 `remove(cursor - 1)` 的字节偏移 → 期望该测试 panic/失败（若不变红则测试无效）
- **关联 impl**: T8

### Task 10: crates/ys-tui-coding — 补全浮层 + 命令表 + Prompter/模态选择器 [type=impl]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `src/{completion,commands,prompter}.rs`
- **描述**: 补全浮层（`Clear` 后盖在 Chat 上，不参与布局高度）：Tab 触发、↑↓ 选择、Enter 填入、Esc 关闭；条目 `/{name} {arg_hint}  — {desc}`。命令表（TUI 侧）：`/help` `/status` `/copy` `/quit` `/thinking` 本地；`/model` 无参 → 浮层选择 → `Request::SetModel`；`/login` → `Prompter` 问答 → `Request::Login`；`/logout` `/new` `/compact` `/export` → 对应 `Request`。`Prompter` trait（sync `select`/`text`）+ `FakePrompter` 移到本 crate；TUI 实现为**嵌套事件循环的模态浮层**（同 alt-screen，不切屏）。
- **关联测试任务**: T11
- **依赖**: T5

### Task 11: crates/ys-tui-coding — 浮层与命令表测试 [type=test]
- **涉及目录**: `crates/ys-tui-coding/src/`
- **涉及文件**: `src/{completion,commands,prompter}.rs` 内嵌测试模块
- **描述**: 补全浮层 TestBackend 可见且盖在 Chat 上、↑↓ 改选中、Enter 填入、Esc 关闭；命令表把每条命令映射到正确动作（本地 / `Request` 变体），**穷举 `Request` 变体确保无遗漏**；`/login` 用 `FakePrompter` 走通取消与成功两条路径
- **验证方法**:
  - `cargo test -p ys-tui-coding` — 期望全部通过
- **关联 impl**: T10

### Task 12: apps/coding-agent — app 侧循环 + ChannelSink 适配 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `src/{app_loop,channel}.rs`
- **描述**: `app_loop`：`loop { request_rx.recv().await }`；`Prompt` 分支用 `{ … }` 块让 `ports` 借用提前结束，内层 `select! { turn, event_rx.recv() → out.send(Outbound::Event) }`；`SetModel`/`Login`/`Logout`/`NewSession`/`Compact`/`Export` 分支后 `emit_view()`；`request_rx` 返回 `None` 时正常退出。`BoundarySource` 实现（`Mutex<Inner{rx, aborted, stash}>`）。`ChannelSink` 改产 `ys_protocol::Envelope`，`LifecyclePolicy` 从 `ys-protocol` 引。
- **关联测试任务**: T13
- **依赖**: T1

### Task 13: apps/coding-agent — app 循环集成测试 [type=test]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `tests/app_loop.rs`（新增）
- **描述**: MockModel 驱动：`Outbound` 的 `Event`/`Output`/`View` 顺序确定（单一出站写者）；`Prompt` 回合中 `Boundary::Steer` 被注入；`Boundary::Abort` → `Cancelled` 且 UI 收到终局事件；`/new`（`NewSession`）后 `CodingView` 刷新且 session 换新
- **验证方法**:
  - `cargo test -p ys-coding-agent --test app_loop` — 期望全部通过
  - **变异测试**：让事件绕过 app 转发、由 UI 直读 → 顺序断言必须变红
- **关联 impl**: T12

### Task 14: apps/coding-agent — 输出与诊断三分 + 日志文件 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `src/logging.rs`（新增）, `src/{provider,state,commands/builtin}.rs`, `main.rs`（`mod logging;`）
- **描述**: 极简 logger → `~/.yushan/logs/yushan.log`（追加、带时间戳、`create_dir_all`、忽略错误、不引新依赖）；`provider.rs`/`state.rs` 两处 `eprintln!` 改日志；`builtin.rs` 的全部 `println!`/`eprintln!`（约 36 处）改为返回 `Outbound::Output` 文本；`main.rs` 三处启动期 `eprintln!` 保持 stderr。
- **关联测试任务**: T15
- **依赖**: T12

### Task 15: apps/coding-agent — 日志与输出路由测试 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `src/logging.rs` 内嵌测试模块
- **描述**: 追加写（两次调用两行）；目录不存在时自建；目标不可写时不影响调用方（返回 `()` 不 panic）；时间戳前缀格式
- **验证方法**:
  - `cargo test -p ys-coding-agent logging` — 期望全部通过
  - `grep -rn "eprintln!" apps/coding-agent/src/` — 期望仅剩 `main.rs` 启动期 3 处
- **关联 impl**: T14

### Task 16: apps/coding-agent — 命令能力实现 + CodingView 构造 + main 分发 + 删旧物 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `src/{main,wiring,commands/mod,commands/builtin,view}.rs`；删除 `src/ui/`、`src/ansi.rs`；`Cargo.toml`（去 `inquire`）
- **描述**: `commands/` 从「注册表 + `Command` trait + `CommandContext`」收缩为 app 侧能力函数（Login/Logout/Model/New/Compact/Export），输出走 `Outbound::Output`；`CodingView` 由 app 侧构造（取代 `view.rs` 的 `AppView::from_sources`）；`main.rs` 初始化顺序按 design §7（建三信道 → 建 `ChannelSink` → **阻塞**构造 `Wiring` 与 `available_models` → 建 `Agent` → 构造 `view₀` → `spawn(app_loop)` + 主线程 `ys_tui_coding::run`）；`-p`/`--json` 改调 `run_turn`、`Wiring` 去掉 `inbox`；删除 `src/ui/`、`ansi.rs`、inquire 依赖、`InquirePrompter`、`suspend/resume_terminal`。
- **关联测试任务**: T17
- **依赖**: T12, T14

### Task 17: apps/coding-agent — 端到端与既有测试保持 [type=test]
- **涉及目录**: `apps/coding-agent/tests/`, `apps/coding-agent/src/`
- **涉及文件**: `tests/integration.rs`, `tests/stream_usage_fallback.rs`, `src/{wiring,main}.rs` 内嵌测试
- **描述**: 既有集成测试（工具层、流式 usage 回退）在新 `run_turn` 驱动下保持通过；`/new` 换 Session 后落盘空文件的行为保持；`-p`/`--json` 输出不变
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 期望全部通过
  - `cargo build --workspace` + `cargo clippy --all-targets` + `cargo fmt --check` — 期望干净
- **关联 impl**: T16

### Task 18: 全量回归与归档核对 [type=test]
- **涉及目录**: 全仓库、`docs/`
- **涉及文件**: 全部
- **描述**: `cargo test --workspace` 全绿且总数 ≥ 280；`grep` 确认 `CancelToken`/`Inbox`/`QueueMode`/`inquire`/`ys-channel` 无残留；测试合同 N1–N12 逐条核对
- **验证方法**:
  - `cargo test --workspace` — 期望 0 failed，总数 ≥ 280
  - `cargo clippy --all-targets` — 期望无 warning
  - `cargo fmt --check` — 期望干净
- **关联 impl**: T1, T5, T8, T10, T12, T14, T16
- **MANUAL_ACK_REQUIRED**:
  - [ ] `cargo run -p ys-coding-agent` 观察三 pane、Status 常驻最底不滚走、补全浮层盖在 Chat 上
  - [ ] 中文输入法下退格与光标位置正常
  - [ ] 退出后主屏 scrollback 打印完整对话
  - [ ] `~/.yushan/logs/yushan.log` 实际落盘（可临时损坏 `auth.json` 观察）
  - [ ] `cargo test -p ys-coding-agent e2e -- --ignored --nocapture`（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）

## 验证清单

- [ ] 所有 (impl, test) 配对均已 completed
- [ ] `cargo test --workspace` — 通过，总数 ≥ 280
- [ ] `cargo clippy --all-targets` — 无 warning
- [ ] `cargo fmt --check` — 干净
- [ ] `cargo build --workspace` — 通过
- [ ] 变异测试三项（中文退格 / `Outbound` 顺序 / `Steer` 保序）均已确认会变红
