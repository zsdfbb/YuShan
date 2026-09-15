# YuShan — 轻量、组件化的 Rust Agent Runtime

用于组装 Agent 的运行时（harness），不是自带全部功能的完整 Agent 产品。项目只负责模型调用、消息状态、工具调用循环和事件流；沙箱、隔离、权限、审批和多租户安全由其他项目独立解决。

**当前状态**：`cargo test --workspace` 全绿 —— **435 passed**（1 个 e2e 需真实凭证，`#[ignore]`）；`cargo clippy --all-targets` 零 warning。已完成：路线 B 重构（拆 crate + 分线程 + 三信道 + `ys-protocol`）、`CancelToken`/`Inbox` 移除（改 `BoundarySource`）、TUI 独立成 `ys-tui-coding`、有界事件信道、流式响应、`/new` 换会话、JSONL 会话落盘。

## 目录结构

```
YuShan/
├── crates/                        # 核心库
│   ├── ys-core                    # 共享原语：Message / Role / ContentBlock / Usage / StopReason / ToolCall / EventError，零外部依赖
│   ├── ys-event                   # AgentEvent + EventSink（try/await 双路径）+ Noop/Collecting/Failing
│   ├── ys-protocol                # 能力平面（协议）：Request / Boundary / Outbound<V> / BoundarySource / Envelope / Source / LifecyclePolicy，**零 tokio**
│   ├── ys-tui-coding              # coding agent 的 TUI（独立 crate）：CodingView + 三 pane 渲染 / 输入 / 补全 / 浮层选择器，**不依赖 ys-runtime**
│   ├── ys-model                   # Model trait（complete）+ ModelEvent/ModelEventSink + MockModel
│   ├── ys-tool                    # Tool trait + ToolRegistry + 审批处理
│   ├── ys-session                 # Session trait + JsonlSession（原子写）/ MemorySession
│   ├── ys-component               # RuntimeContext 容器（含 boundary: Option<&dyn BoundarySource>）+ RunLimits
│   ├── ys-loop                    # AgentLoop trait + BasicLoop（含轮边界 steering / Abort）
│   └── ys-runtime                 # AgentBuilder + Agent（无状态）+ AgentPorts + prelude
├── adapters/                      # 可选适配器
│   ├── model-openai-compatible    # OpenAI 兼容后端（流式 SSE + compat 差异）
│   └── tools-basic                # BashTool / ReadTool / WriteTool / EditTool
├── apps/                          # 产品层应用
│   └── coding-agent               # YuShan Coding Agent（bin crate，接线器）
│       ├── src/
│       │   ├── main.rs            # 入口：模式分发（TUI / -p / --json）、启动组装（设计 §7 初始化顺序）
│       │   ├── app_loop.rs        # app 侧循环：回合边界 recv Request、跑 turn、转发事件（actor 循环承载者）
│       │   ├── capabilities.rs    # 命令的**能力实现**（login/logout/set_model/new_session/compact/export），返回 Vec<String> → Outbound::Output
│       │   ├── wiring.rs          # 接线器：持有 model + session + events（/new 换的就是它）；ports() 产出 AgentPorts
│       │   ├── channel.rs         # ChannelSink：有界 mpsc + overflow + 背压统计
│       │   ├── logging.rs         # 极简日志文件 ~/.yushan/logs/yushan.log（库内部诊断的落处）
│       │   ├── config.rs          # Config + ProviderRegistry 持有 + model factory
│       │   ├── provider.rs        # ProviderRegistry：provider 目录、auth.json、/v1/models、ProviderCompat
│       │   ├── prompt.rs          # 系统提示词构建
│       │   ├── view.rs            # CodingView 显示快照构建（唯一显示数据源）
│       │   ├── state.rs           # state.json（last_active_provider / model）
│       │   ├── status.rs          # TurnStats 累计
│       │   └── test_env.rs        # #[cfg(test)] 统一 env 锁（测试隔离）
│       └── tests/
│           ├── integration.rs         # 工具层集成（MockModel，无网络）
│           ├── stream_usage_fallback.rs # 流式 usage 门控与 400 回退
│           └── e2e_tools.rs           # 真实 API 端到端（#[ignore]）
├── docs/
│   ├── design.md              # 总体设计（原则、crate 划分、核心 trait、Hook/Event 边界、路线图、测试矩阵）
│   ├── CONTEXT.md             # 领域术语表（Turn / Round / StopReason / 错误结果 vs 基础设施失败…）
│   ├── adr/                   # 架构决策记录
│   ├── arch/                  # 架构分析（context / design / review）
│   ├── design-plans/          # 设计方案（completed/ 为已归档）
│   ├── exec-plans/            # 执行计划（completed/ 为已归档）
│   ├── design-final/          # as-built 最终设计（实现后回写）
│   └── reports/               # 分析报告
└── CLAUDE.md -> AGENTS.md
```

## 常用命令

- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt`
- `cargo test --workspace` — 全量测试（435 passed）
- `cargo run -p ys-coding-agent` — 交互式 TUI
- `cargo run -p ys-coding-agent -- -p "任务"` — 单次 Print 模式（边生成边打印文本增量）
- `cargo run -p ys-coding-agent -- --json "任务"` — JSON 事件模式（逐行 `Envelope`）
- `cargo run -p ys-coding-agent -- --json --stats "任务"` — 额外输出背压统计到 stderr
- `cargo test -p ys-coding-agent e2e -- --ignored --nocapture` — 端到端（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）

环境变量：`YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`；`YUSHAN_CHANNEL_CAPACITY`（信道容量，下限 16）；`YUSHAN_SESSIONS_DIR`（会话目录覆盖，测试用）。

## 核心模型（理解本项目的关键）

### Agent 是无状态执行器（ADR-0010，结论不变）

`Agent` **不持有**会话、事件出口或模型。这三者由接线器持有，每次运行时经端口传入：

```rust
pub struct AgentPorts<'a> {
    pub model:    Option<&'a dyn Model>,        // None = 未配置，run_turn 返回 ConfigError
    pub session:  &'a mut dyn Session,           // 归接线器；/new 换的就是它
    pub events:   &'a mut dyn EventSink,         // 归接线器
    pub boundary: Option<&'a dyn BoundarySource>,// 轮边界控制（Steer/Abort），每回合新建
}

pub async fn run_turn(&mut self, input: AgentInput, ports: AgentPorts<'_>) -> Result<RunResult, LoopError>;
```

`Agent` 保留的字段只有执行能力：`loop_impl / registry / limits / cwd / workspace_root / approval / system_prompt`。

> **附注（2026-09-14）**：`Agent::run(inbox)` 与 `RunSummary` **已删除**。**actor 循环**（多回合 /
> followUp 的驱动）**上移到 `apps/coding-agent/src/app_loop.rs`**（见 ADR-0010 附注、ADR-0013）。
> `Agent` 仍是「只持执行能力、不持会话」的无状态执行器 —— 结论不变，只是承载者换了。
> `-p` / `--json` 不再经 `Agent::run`：直接 `run_turn` + `begin_turn(1)`，一次运行恒为 turn 1。

### 三信道 + 两线程（路线 B）

`ys-tui-coding` **不认识 `Agent`**（不依赖 `ys-runtime`，编译器强制），故 agent 必须在另一个线程：

| 信道 | 类型 | 方向 | 消费时机 |
|---|---|---|---|
| ① Request | `ys_protocol::Request` | UI → app | app 线程 `recv().await`（**回合边界**） |
| ② Boundary | `ys_protocol::Boundary` | UI → `BasicLoop` | **轮边界**拉取（`Steer` 插话 / `Abort` 取消） |
| ③ Outbound | `ys_protocol::Outbound<V>` | app → UI | UI 事件循环（**单一出站写者**） |

- **UI 线程**跑 `ys_tui_coding::run`（**同步阻塞**，用 `blocking_send`，必须在非 async 线程）
- **app 线程**跑 `app_loop::run`（async，`rt.spawn`）；UI 退出 → drop 发送端 → app 线程 `recv()` 得 `None` → 正常收摊
- **② 必须独立于 ①**：回合跑动时 app 线程阻塞在 `run_turn()` 里，不可能 `recv` ①，而插话/取消要**中途**被看到

### `BoundarySource` 与 Abort 语义（取代 `CancelToken` 与 `Inbox`）

```rust
// ys-protocol，零 tokio
pub trait BoundarySource: Send + Sync {
    fn take(&self) -> Option<Boundary>;   // 轮边界：非阻塞、保序
    fn is_aborted(&self) -> bool;          // 非破坏性探针（模型调用前 / 工具轮询）
}
```

- **取消不再是跨线程原子**，而是队列里的一条消息（`Boundary::Abort`）—— 与插话（`Steer`）**共享同一条有序通道**
- `QueueBoundarySource`（`Mutex<Inner{queue, aborted}>`）是唯一实现；两方法都是 `&self`（内可变），生产者可在 agent 运行期间 `push`
- `Abort` 入队**即置位** `is_aborted`（探针立即可见），但**照常入队**（保序，`take` 仍会交还）；标记不随 `take` 清除
- **每回合必须新建 `BoundarySource`**：标记永久置位，复用会让「上一回合的取消」立刻取消之后每个回合；app 循环还须在空闲期丢弃残留边界消息
- `ToolContext.cancel: &CancelToken` → `ToolContext.boundary: Option<&dyn BoundarySource>`；`BashTool` 100ms 轮询 `is_aborted()`，命中则杀**进程组**（`sh -c "kill -9 -{pid}"`，不引 libc）
- `/new` 仍是**换整个 `Wiring`**（新 Session，旧文件保留）；会话落盘 `~/.yushan/sessions/{unix秒}_{纳秒}.jsonl`，启动恢复最近一个，`/new` 时**立即落盘空文件**（否则重启恢复不到新会话）

### 事件信道

`Envelope { source, turn, event }`（现居 `ys-protocol`）—— 不传裸事件，`source` 为多 agent 协作预留。

**`EventSink` 是 try/await 双路径**：

```rust
fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>;  // 同步快路径
fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<Box<dyn Future<...> + Send + 'a>>;  // 异步慢路径
fn begin_turn(&mut self, _turn: u32) {}                              // 默认 no-op
pub async fn emit(sink: &mut dyn EventSink, event: AgentEvent) -> Result<(), EventError>;  // 循环内唯一出口
```

- **同步快路径的唯一消费者是 `Forwarder`**（实现同步的 `ModelEventSink`，不能 await）
- 循环内的自由函数 `emit()` **总是走异步路径** —— 若让它先试快路径，满时 `try_emit` 缓冲后返回 `Ok`，冲刷永不发生，终局事件会滞留、背压失效（这是修过的死锁）
- **`try_emit` 的 `Err` 仅表示「消费者已消失」**；信道满时实现**必须内部缓冲**，不得返回 `Err`

### 输出与诊断三分

TUI 起屏后 stderr/stdout 会冲掉整屏，故按**时机**三分（解 review R2）：

| 时机 | 内容 | 去处 |
|---|---|---|
| **启动期**（TUI 未起） | 容量 clamp 警告、`No model configured` | **stderr**（照旧） |
| **命令期间**（有 TUI） | `/login` 的 "✓ Logged in…"、"Could not persist credentials" 等 | **`Outbound::Output`** → transcript |
| **库内部 / 随时** | `auth.json` / `state.json` 解析失败 | **日志文件** `~/.yushan/logs/yushan.log`（`src/logging.rs`，手写极简 logger，追加写、`[unix秒]` 前缀、best-effort、不引依赖） |

**`capabilities.rs` 绝不 print** —— 命令期间的失败也走 `Output`。

## coding-agent 模块说明

| 模块 | 职责 |
|------|------|
| `main` | 入口：模式分发（TUI / `-p` / `--json`）、参数解析、启动组装（三信道 → sink → wiring → agent → view₀ → spawn） |
| `app_loop` | **app 侧循环**（actor 循环的承载者）：回合边界 `recv` `Request`、跑 `run_turn`、转发事件为 `Outbound`；空闲期丢弃残留边界消息 |
| `capabilities` | 命令的**能力实现**（`login` / `logout` / `set_model` / `new_session` / `compact` / `export`），各返回 `Vec<String>` → `Outbound::Output`；**绝不 print** |
| `wiring` | **接线器**：持有 `model` + `session` + `events`，`ports()` 产出 `AgentPorts`（boundary 由 app 循环每回合传入）；`/new` / `set_model` / `session_messages` |
| `channel` | `ChannelSink`：有界 mpsc + overflow 缓冲 + `begin_turn` + 背压统计（`format_stats`） |
| `logging` | 极简日志文件 `~/.yushan/logs/yushan.log`（库内部诊断 + app 循环异常，best-effort、不引依赖） |
| `config` | Config 结构体，持有 ProviderRegistry + model factory，管理运行时配置 |
| `provider` | ProviderRegistry：内置 provider 目录、auth.json 持久化（`~/.yushan/`）、GET /v1/models 动态获取、ProviderCompat 映射 |
| `prompt` | 系统提示词构建 |
| `view` | `build_view` → `CodingView` 显示快照（provider/model/tokens/turn_count/session/cwd/tools/available_models…），唯一显示数据源 |
| `state` | state.json（last_active_provider / model） |
| `status` | `TurnStats` 累计 |

命令的**用户可见行为**（命令表、提示、浮层）在 `ys-tui-coding/src/commands.rs`（TUI 侧表）；
**能力实现**在 app 侧 `capabilities.rs`。二者唯一契约是 `Request` —— 漏实现是**编译错误**。

关键设计：

- **协议在 `ys-protocol`**：`Request` / `Boundary` / `Outbound<V>` / `BoundarySource` / `Envelope` / `Source` / `LifecyclePolicy`
- **`CodingView` 归 `ys-tui-coding`**（各产品视图不同，不进协议）；`Outbound<V>` 泛型化
- **`Prompter` trait 保留**（`ys-tui-coding`）：TUI 实现 `TuiPrompter` 画模态浮层，测试注入 `FakePrompter`；决策逻辑抽在纯函数 `resolve_prompt` 里
- 凭证持久化到 `~/.yushan/auth.json`（0o600），启动时自动恢复
- **三种入口互斥**：TUI（默认）/ `-p` / `--json`；`-p`/`--json` 用 `Wiring::ephemeral`（MemorySession 不落盘）
- **`-p`/`--json` 的消费必须与 turn 并发**（`tokio::join!`）—— 有界信道若无并发消费者，第一次撞满即死锁
- **消费循环以终局事件（`RunFinished`/`RunFailed`）为终止条件**，不是等信道关闭（sender 在 agent 侧，等关闭必死锁）
- 数据/显示分离：`build_view` 是唯一显示数据源快照；`App`（`ys-tui-coding`）管 UI 交互状态
- **`/login` 的 api_base 不再交互填**：协议只带 provider + api_key，api_base 从 provider 内置值解析，回退 `YUSHAN_API_BASE`；`custom` provider 需靠环境变量（见 `docs/design-final/coding-agent-tui.md` §8 M3）

## 硬性约束

以下是 `docs/design.md` 不变量 + 历次 ADR 的摘要。**新增代码违反任何一条都算设计偏离，需先改设计文档。**

- **依赖方向单向**：`ys-core` ← 组件接口 ← loop/runtime ← 应用适配器
  - **`ys-core` / `ys-event` / `ys-protocol` 不含 tokio**（直接与间接皆无）—— 它们只装数据与纯 trait
  - **`ys-tui-coding` 不依赖 `ys-runtime`**（不认识 `Agent`）—— **编译器强制**（crate 边界），不靠自觉
  - **`ys-component` 不直接依赖 tokio**（自身代码不用 tokio API），但经 `ys-session` **间接**引入（ADR-0012）
  - `ys-session` / `ys-loop` / `ys-runtime` 可直接依赖 tokio（文件 IO、工具超时）
- **最小核心**：Runtime 只提供一次 Agent Turn 所需能力；文件系统、Shell、MCP、TUI、记忆、子 Agent 均为可选组件
- **不做安全**：沙箱、隔离、权限、审批、多租户由上层项目解决；`ToolRegistry` 只按名称查找工具
- **四者边界不可混淆**：Event 观察「发生了什么」，Hook 决定「下一步怎么处理」，Component 提供能力，Loop 决定推进规则
- **静态组合优先**：能力通过 crate + Cargo feature 组合；动态插件只做运行时扩展，第一版不支持热插拔
- **不跨动态库边界传递 Rust trait object、Tokio 类型或跨库所有权对象**
  - 推论：**`ModelEventSink` 必须保持同步 `fn`**（async trait 跨动态库边界困难）—— 这是 try/await 双路径存在的首要理由
- **Agent 无状态**（ADR-0010）：不持有 session / events / model；会话、配置、历史归接线器。**actor 循环已上移到 `app_loop`**（ADR-0010 附注）
- **取消即消息**（ADR-0013）：`CancelToken` / `Agent::cancel()` 已删；`Boundary::Abort` + `BoundarySource` 取代 `Inbox` / `Intent` / `QueueMode`
- **信道契约**（ADR-0009 / 0011 / 0013）：有界、容量可配；`try_emit` 的 `Err` 仅表示消费者消失；满时内部缓冲
- **事件只观察，Hook 才控制流程**：`AgentEvent` 不可用于改变执行路径

## 工作约定

- **文档、注释、讨论、commit message、PR 描述用中文**；代码标识符用英文（技术术语/专名可夹英文）
- 实现顺序遵循路线图：最小闭环 → 可用适配器 → 静态组件生态 → 动态插件 → Coding Agent MVP
- 新模块和接口改动须能对应上 `docs/design.md` §12 测试矩阵中的条目
- **测试隔离（并行安全，血的教训）**：
  - 临时目录名必须含**进程内原子序号 + `process::id()`**（`SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名 → 互删文件）
  - **进程级 env 改写必须走 `crate::test_env::env_lock()` + `EnvRestore`**（RAII 恢复）；直接把 `set_var` 散在测试里会造成 getenv/unsetenv 数据竞争
- **关键逻辑要做变异测试**：注入 bug 验证测试能抓到。本项目多个真 bug（死锁、UTF-8 损坏、`/new` 失效）都是「测试全绿时依然存在」，靠变异测试与端到端复现才暴露
- **流式/字节路径注意 UTF-8 边界**：TCP 分片可能切在多字节字符中间；缓冲必须是**字节级**，只在完整行上解码
- `tmp/` 已被 `.gitignore` 忽略，勿将正式内容放入

## 参考资料

| 需要了解... | 阅读... |
|---|---|
| **架构全貌的图解**（组件关系、数据流、时序） | `docs/architecture.md` |
| 架构规范、核心 trait、Hook/Event 边界、测试矩阵 | `docs/design.md` |
| 术语定义（Turn、Round、StopReason、错误结果 vs 基础设施失败…） | `docs/CONTEXT.md` |
| **给新需求归类**（该用 Event / Tool / Follow-up / 还是真需要 Hook） | `docs/extension-points.md` |
| 架构决策的 why | `docs/adr/`：0001 工具失败双通道、0002 协作取消（2026-09-14 部分修订）、0003 Session::append 异步、0004 v0 分层骨架、0007 Agent 公开 API、0008 cancel_handle（**已作废**）、0009 事件信道异步化、0010 actor 模型（Agent 无状态，含 2026-09-14 附注）、0011 信道设计（2026-09-14 部分修订）、0012 运行时依赖边界、0013 取消即消息（`CancelToken` 移除 / `BoundarySource` 取代 `Inbox`）、0014 只做 follow-up（**不建 hook 系统**，有意推迟） |
| **当前实现的实际形态**（含实现与设计的分歧） | `docs/design-final/coding-agent-tui.md`（路线 B as-built，最新）+ `docs/design-final/core-channel.md`（核心信道第一刀，部分已被上文取代，见其 §8） |
| coding agent TUI 的设计推导与质量分析 | `docs/arch/coding-agent-tui/{design,review}.md`；已作废的前身 `docs/arch/coding-agent-tui/tui-repl-context.md` |
| 核心信道的设计推导与质量分析 | `docs/arch/gap-closure/{context,design-core-channel,review}.md` |
| 各 crate 的 API 和实现细节 | 对应 crate 的 `src/lib.rs` |
| coding-agent 产品层设计 | `apps/coding-agent/src/{main,app_loop,capabilities,wiring,channel}.rs` + `crates/ys-tui-coding/src/` + `docs/arch/` |
| /login & /model 行为设计 | `docs/arch/commands/login-model-behavior/` |
