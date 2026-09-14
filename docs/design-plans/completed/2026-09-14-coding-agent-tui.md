# Coding Agent TUI（路线 B）设计计划

> 决策依据：`docs/arch/coding-agent-tui/design.md`（已定稿）+ `review.md`（R1–R7）。
> 本文不重新讨论设计取舍，只把 design.md 落成可执行的任务划分。

## 背景

**当前状态**：TUI（`apps/coding-agent/src/ui/`，约 1600 行）与 agent 跑在同一个 tokio task 里，用一个 `biased` 的四路 `select!`（`ui/mod.rs:457-488`）交错 turn future / crossterm 事件 / 事件信道 / 300ms 节拍。

**问题**：
1. **边界靠自觉**：`ui/` 与 `Wiring`/`Agent`/`CommandRegistry` 直接互相持有可变借用（`ui/mod.rs:48-58` 七个参数），没有编译器强制的分层。
2. **slash 命令靠切屏**：`ui/mod.rs:262` `suspend_terminal()` 退出 alt-screen → 命令直接 `println!` 到真实终端 → `ui/mod.rs:273` `resume_terminal()`。`InquirePrompter`（`commands/builtin.rs:7-29`）依赖 inquire 阻塞读。
3. **14 处生产 `eprintln!` 会撕画面**（`main.rs` 3 处启动期、`builtin.rs` 4 处、`provider.rs:132`、`state.rs:36`）。
4. **补全浮层从未渲染**：`CompletionState`（`ui/app.rs:61-70`）被 `ui/events.rs` 写入，`ui/draw.rs` 中 grep 不到任何引用。
5. **中文退格 panic**：`ui/events.rs:131` `app.input.remove(app.input_cursor - 1)` 落在多字节字符中间。
6. **工具调用不渲染**：`ui/mod.rs:359-372` 只处理 `ModelTextDelta`，`ToolCall`/`ToolResult` 被忽略。
7. **滚动模型错**：`ui/draw.rs:176-179` assistant 不做 wrap（一条 = 一个 `Line`），而 `compute_visible`（`ui/draw.rs:65-99`）按「一条 transcript = 一行」算可见高度，与实际不符。
8. **assistant wrap 缺失 + `truncate`（`ui/draw.rs:220-226`）按字节切**，CJK 会 panic。

**为什么现在做**：design.md 已定稿路线 B（拆 crate + 分线程 + 三信道 + `ys-protocol`），review.md 的 R2/R3/R4 已在 design 中解掉。R1（库侧清理）是执行期必须落实的部分。

## 设计

### 目标依赖图（单向、无环）

```
ys-core ──► ys-event ──► ys-protocol ──► ys-tui-coding ──► apps/coding-agent
                    ▲         ▲                              │
                    └─────────┴──────────────────────────────┘
ys-protocol ◄── ys-component ◄── ys-loop ◄── ys-runtime
```

- **新增** `crates/ys-protocol/`（零 tokio）：`Request` / `Boundary` / `Outbound<V>` / `BoundarySource` / `Envelope` / `Source` / `LifecyclePolicy`
- **新增** `crates/ys-tui-coding/`（**不依赖 `ys-runtime`**，编译器强制）：`CodingView` / `run()`（同步阻塞事件循环）/ 渲染 / 输入 / 补全浮层 / `Prompter` + `FakePrompter` / 命令表
- **删除** `crates/ys-channel/`（迁空后整 crate 移除）
- **删除** `apps/coding-agent/src/ui/`、`ansi.rs`、`inquire` 依赖、`InquirePrompter`、`suspend_terminal`/`resume_terminal`

### 协议形状（design §6）

```rust
// ys-protocol
pub enum Request {
    Prompt(Message),
    SetModel { model: String },
    Login { provider: String, api_key: String },
    Logout,
    NewSession,
    Compact,
    Export { path: PathBuf },
}
pub enum Boundary { Steer(Message), Abort }
pub enum Outbound<V> { Event(Envelope), View(V), Output(String), Quit }
```

无 `Diagnostic` 变体：命令期用户可见诊断走 `Output`，库内部诊断走日志文件。

### 三信道（design §8）

| 信道 | 类型 | 方向 | 消费时机 |
|---|---|---|---|
| ① Request | `Request` | UI → app | app 线程 `recv().await`（回合边界） |
| ② Boundary | `Boundary` | UI → `BasicLoop` | 轮边界 + 模型调用前（`BoundarySource`） |
| ③ Outbound | `Outbound<CodingView>` | app → UI | UI 事件循环 `try_recv()` |

② 必须独立于 ①：回合跑动时 app 阻塞在 `run_turn` 里，`Steer`/`Abort` 要中途可见 → 由 `BasicLoop` 拉。

### 微决策（design.md 未写全、本次补，需在 design-final 回写）

1. **`BoundarySource` 形状与 Abort 的中途生效**
   design 只说「轮边界 `try_recv` → 置取消标志」。若只在轮边界拉，长回合中的 Ctrl-C 要等本轮到头。定义为 `&self` 的零 tokio trait：
   ```rust
   pub trait BoundarySource: Send + Sync {
       fn take(&self) -> Option<Boundary>;   // 非阻塞；轮边界调用
       fn is_aborted(&self) -> bool;         // 非破坏性；模型调用前 / 工具轮询
   }
   ```
   实现（app 侧，`Mutex<Inner { rx, aborted, stash }>`）：两个方法都先 `try_recv` 抽干；遇 `Abort` 置位；遇 `Steer` 入 `stash`（保序，不丢插话）。这样 `is_aborted()` 也能即时看到 abort。

2. **工具级取消的载体**：`ToolContext.cancel: &CancelToken` → `ToolContext.boundary: Option<&'a dyn BoundarySource>`，工具轮询 `is_aborted()`。行为与今天等价（Bash 的 kill 路径不变），只是信号来源从共享原子变成边界源。

3. **`Agent::run` 的去向**：`Inbox` 移除后它的语义由 app 循环承担 → 删除 `Agent::run` / `run_one_turn` / `RunSummary`，`run_turn` 成为唯一入口；`-p`/`--json` 改调 `run_turn`。actor 循环上移到 `apps/coding-agent`（Finalize 回写 ADR-0010 附注）。

4. **UI 循环是同步 poll 循环**（不是 design §7 图中的 async `select!`）
   §8 要求 `ys-tui-coding::run()` 阻塞在自己的同步事件循环里。故：`crossterm::event::poll(50ms)` → 处理按键 → `rx_out.try_recv()` 排空 → 到 300ms 节拍就重绘。**不在 UI 线程建 tokio runtime**。

5. **`/model` 无参时模型列表的来源**：`CodingView.available_models` / `providers` 由 app 在**启动期阻塞步骤**（design §7 初始化顺序第 3 步）填充（`available_models` 已有 5s 超时 + 静态回退）。TUI 保持「不认识网络」。

6. **`Request::Quit` 不存在**：design §6 表里 `/quit` 是本地命令，§7 的 `Quit` 分支与 §6 冲突。以 §6 为准：`/quit` 本地退出 UI 循环；app 侧 `request_rx.recv()` 返回 `None`（UI drop sender）时 app 循环正常退出。

### 输出与诊断三分（design §5）

| 时机 | 例子 | 去处 |
|---|---|---|
| 启动期（TUI 未起） | 容量 clamp 警告、`No model configured`、`--stats` 读数 | **stderr**（照旧） |
| 命令期间（有 TUI） | `/login` 的 "✓ Logged in…"、persist 失败警告 | **`Outbound::Output`** → transcript |
| 库内部 / 随时 | `auth.json` / `state.json` 解析失败 | **日志文件** `~/.yushan/logs/yushan.log` |

日志：手写极简 logger（打开追加、带时间戳、忽略错误），**不引新依赖**。

### 命令 → 能力映射（design §6，表在 TUI 侧）

| 命令 | TUI 做什么 | 发给 app |
|---|---|---|
| `/help` `/status` `/copy` `/quit` `/thinking` | 本地 | — |
| `/model [name]` | 无参时浮层选择 | `SetModel` |
| `/login` | 问 provider / api_key（`Prompter`） | `Login` |
| `/logout` | — | `Logout` |
| `/new` | — | `NewSession` |
| `/compact` | — | `Compact` |
| `/export [path]` | — | `Export` |

漏实现是编译错误（`Request` 变体必须 match）。

### 数据流

```
UI 线程                          app 线程
  │ 按键 → tx_req.send(Request) ──► recv().await（回合边界）
  │ 中途插话 → tx_boundary.send(Boundary) ──► BoundarySource（轮边界）
  │ ◄── rx_out.try_recv() ◄── Outbound<CodingView> ◄── agent emit → ChannelSink → 转发
```

**单一出站写者**：UI 只读 `Outbound` 一条流，保证 `Event` 与 `Output` 的先后确定；`ChannelSink` 不变（继续写自己的信道），只多一个转发循环。

## 涉及文件

- **新增**: `crates/ys-protocol/`（Cargo.toml + src/{lib,request,boundary,outbound,boundary_source,envelope,lifecycle}.rs）
- **新增**: `crates/ys-tui-coding/`（Cargo.toml + src/{lib,view,run,draw,events,input,completion,prompter,commands,format}.rs）
- **新增**: `apps/coding-agent/src/logging.rs`、`apps/coding-agent/src/app_loop.rs`
- **修改**: `Cargo.toml`（workspace members）、`crates/ys-core/src/{lib,cancel}.rs`、`crates/ys-component/src/context.rs`、`crates/ys-tool/src/context.rs`、`crates/ys-loop/src/basic.rs`、`crates/ys-runtime/src/{agent,builder,prelude}.rs`、`adapters/tools-basic/src/*.rs`（测试）、`crates/ys-runtime/tests/actor_run.rs`、`apps/coding-agent/src/{main,wiring,channel,view,status,format}.rs`、`apps/coding-agent/src/commands/{mod,builtin}.rs`、`apps/coding-agent/Cargo.toml`
- **删除**: `crates/ys-channel/`、`apps/coding-agent/src/ui/`、`apps/coding-agent/src/ansi.rs`

## 测试策略

### 测试合同（Phase 3 审查依据）

#### N1: `ys-protocol` 类型与迁移
- **验收方法**: `cargo test -p ys-protocol` — 期望全部通过；含 `Request`/`Boundary`/`Outbound` serde roundtrip + 从 `ys-channel` 迁来的 `Envelope`/`Source`/`LifecyclePolicy` 5 个测试
- **优先级**: 必测
- **验收人**: review subagent

#### N2: `BoundarySource` 契约
- **验收方法**: `cargo test -p ys-protocol boundary` — 期望 `take` 保序、`Steer` 不丢、`Abort` 置位后 `is_aborted()` 立即可见、abort 后仍能取到先前排队的 `Steer`
- **优先级**: 必测
- **验收人**: review subagent

#### N3: 库侧清理（Inbox/Intent/QueueMode/CancelToken 移除）
- **验收方法**: `cargo build --workspace` + `grep -rn "CancelToken\|QueueMode\|Intent::\|Inbox" crates/ adapters/ apps/ --include=*.rs` — 期望编译通过且无残留（`Intent` 仅可出现在 `ys-protocol::Boundary` 语义中）
- **优先级**: 必测
- **验收人**: review subagent

#### N4: `ys-loop` 轮边界 steer / abort
- **验收方法**: `cargo test -p ys-loop` — 期望轮中 `Steer` 注入进 `ModelRequest`（原 `SteeringProbeModel` 改写）、`Abort` → `StopReason::Cancelled`、工具轮询 `is_aborted()` 提前收场
- **优先级**: 必测
- **验收人**: review subagent

#### N5: actor 语义保住（`run_turn` + BoundarySource）
- **验收方法**: `cargo test -p ys-runtime` — 期望多回合、steer、abort、上下文压缩等原 `actor_run.rs` 场景全部在新驱动方式下通过
- **优先级**: 必测
- **验收人**: review subagent

#### N6: TUI 三 pane 与渲染
- **验收方法**: `cargo test -p ys-tui-coding` TestBackend 断言 — 期望 Chat/Input/Status 自上而下、Status 为底部 `Length(1)`、assistant 真 wrap 后滚动行数正确、工具调用压一行、失败工具显示结果首行、turn 摘要符号正确、错误行渲染
- **优先级**: 必测
- **验收人**: review subagent

#### N7: 状态行窄屏剥尾
- **验收方法**: TestBackend 在四种宽度下渲染 — 期望 宽 `model · ↑↓ · N 轮 · 时长` / 中 去时长 / 窄 去轮数 / 极窄 仅 model；turn 中 model 段换 `⏳ Working·`
- **优先级**: 必测
- **验收人**: review subagent

#### N8: 输入区（多行 + 粘贴 + 中文退格）
- **验收方法**: `cargo test -p ys-tui-coding input` + 变异测试（去掉 char 边界对齐 → 必 panic）— 期望中文退格不 panic、多行粘贴不被当多次提交
- **优先级**: 必测
- **验收人**: review subagent

#### N9: 补全浮层
- **验收方法**: TestBackend 断言 Tab 后浮层可见且盖在 Chat 上、↑↓ 改变选中、Enter 填入、Esc 关闭
- **优先级**: 必测
- **验收人**: review subagent

#### N10: app 侧循环与单一出站写者
- **验收方法**: `cargo test -p ys-coding-agent` 集成测试（MockModel）— 期望 `Outbound` 的 `Event`/`Output`/`View` 顺序确定、`/new` 后 `CodingView` 刷新、`Prompt` 中途 `Steer` 生效
- **优先级**: 必测
- **验收人**: review subagent

#### N11: 输出与诊断三分
- **验收方法**: `cargo test -p ys-coding-agent logging` + grep `eprintln!` — 期望库内部两处走日志文件（追加写、目录自建、写失败不影响运行）、命令期输出走 `Outbound::Output`、启动期三处仍在 stderr
- **优先级**: 必测
- **验收人**: review subagent

#### N12: 旧物清理
- **验收方法**: `ls apps/coding-agent/src/ui` 不存在、`grep -rn "inquire\|InquirePrompter\|suspend_terminal" apps/ --include=*.rs` 无命中、`cargo build --workspace` 通过
- **优先级**: 必测
- **验收人**: review subagent

#### N13: 真实终端观感（三 pane / 常驻 Status / 浮层 / 中文输入 / 退出 scrollback / 日志落盘）
- **验收方法**: `cargo run -p ys-coding-agent` 人工核对
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

#### N14: 端到端（真实凭证）
- **验收方法**: `cargo test -p ys-coding-agent e2e -- --ignored --nocapture`
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

### 测试框架与命令

- 单元/集成测试: `cargo test --workspace`（基线 280 passed）
- 单 crate: `cargo test -p <crate>`
- Lint: `cargo clippy --all-targets`
- 格式: `cargo fmt --check`
- 变异测试: 手工注入 bug 后跑对应测试，确认变红（重点：中文退格、`Outbound` 顺序、`Steer` 保序）

## 波及文档

- `docs/design-final/core-channel.md`（信道契约变更：`Envelope` 迁址、`Inbox` 移除）
- `docs/design-final/` 新增 `coding-agent-tui.md`（as-built：三信道 / 两线程 / `BoundarySource` / 日志文件 / 微决策）
- `docs/adr/0010` 附注（actor 循环上移）、新增 ADR（`CancelToken` 移除 + `BoundarySource` 取代 `Inbox`）
- `CLAUDE.md`（目录结构、测试数、核心模型章节）
- `docs/arch/tui-repl/context.md`（归档）

## 风险与注意事项

| 风险 | 应对 |
|---|---|
| R1 波及约 110 处，中间态不可编译 | A 阶段作为**单个原子改动**落地（移除 `CancelToken` 会同时破坏 component/tool/loop/runtime），一次改完再跑全量测试 |
| 两线程 + 三信道首版易死锁 | 单一出站写者；`/quit` 由 UI 侧 drop sender 驱动 app 退出；先写集成测试 |
| `ys-tui-coding` 需要 tokio 的 `mpsc` 类型 | 允许 —— design 只禁 `ys-runtime`；`run()` 同步阻塞，UI 线程不建 runtime |
| 旧 `ui/` 约 25 个 TestBackend 断言作废 | 新 crate 的测试逐条对照补回，宁多不少 |
| 中文退格修复易假绿 | 强制变异测试 |
| `Mutex` 在 `BoundarySource` 里跨线程 | 仅在 app 线程持有；`take`/`is_aborted` 都是短临界区，无 await 跨锁 |
