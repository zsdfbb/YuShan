# YuShan — 轻量、组件化的 Rust Agent Runtime

用于组装 Agent 的运行时（harness），不是自带全部功能的完整 Agent 产品。项目只负责模型调用、消息状态、工具调用循环和事件流；沙箱、隔离、权限、审批和多租户安全由其他项目独立解决。

**当前状态**：`cargo test --workspace` **280 passed / 0 failed**（1 个 e2e 需真实凭证，`#[ignore]`）。已完成：actor 模型（agent 自转 + 无状态化）、有界事件信道、流式响应、TUI 增量渲染、`/new` 换队列、JSONL 会话落盘。

## 目录结构

```
YuShan/
├── crates/                        # 核心库
│   ├── ys-core                    # 共享原语：Message / Role / Usage / EventError / CancelToken，零外部依赖
│   ├── ys-event                   # AgentEvent + EventSink（try/await 双路径）+ Noop/Collecting/Failing
│   ├── ys-channel                 # 信道契约：Envelope / Source / Inbox / Intent / QueueMode / LifecyclePolicy
│   ├── ys-model                   # Model trait（complete）+ ModelEvent/ModelEventSink + MockModel
│   ├── ys-tool                    # Tool trait + ToolRegistry + 审批处理
│   ├── ys-session                 # Session trait + JsonlSession（原子写）/ MemorySession
│   ├── ys-component               # RuntimeContext 容器 + RunLimits
│   ├── ys-loop                    # AgentLoop trait + BasicLoop（含轮边界 steering）
│   └── ys-runtime                 # AgentBuilder + Agent（无状态）+ AgentPorts + prelude
├── adapters/                      # 可选适配器
│   ├── model-openai-compatible    # OpenAI 兼容后端（流式 SSE + compat 差异）
│   └── tools-basic                # BashTool / ReadTool / WriteTool / EditTool
├── apps/                          # 产品层应用
│   └── coding-agent               # YuShan Coding Agent（bin crate）
│       ├── src/
│       │   ├── main.rs            # 入口：模式分发（TUI / -p / --json）、参数解析
│       │   ├── wiring.rs          # 接线器：持有 model + session + inbox（/new 换的就是它）
│       │   ├── channel.rs         # ChannelSink：有界 mpsc + overflow + 背压统计
│       │   ├── config.rs          # Config + ProviderRegistry 持有 + model factory
│       │   ├── provider.rs        # ProviderRegistry：provider 目录、auth.json、/v1/models、ProviderCompat
│       │   ├── commands/          # Command trait + CommandRegistry + 内置命令
│       │   ├── prompt.rs          # 系统提示词构建
│       │   ├── view.rs            # AppView 显示快照（唯一显示数据源）
│       │   ├── state.rs           # state.json（last_active_provider / model）
│       │   ├── status.rs          # TurnStats 累计
│       │   ├── format.rs / ansi.rs# 显示格式化 / 颜色
│       │   ├── test_env.rs        # #[cfg(test)] 统一 env 锁（测试隔离）
│       │   └── ui/                # ratatui TUI（feature `tui-ratatui`，默认开）
│       │       ├── mod.rs         # 事件循环、模式桥接、退出打印完整对话
│       │       ├── app.rs         # App（UI 交互状态）
│       │       ├── draw.rs        # 渲染 + TestBackend 测试
│       │       ├── events.rs      # 键鼠事件 → App
│       │       └── completion.rs  # Tab 补全
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
- `cargo test --workspace` — 全量测试（当前 280 passed）
- `cargo run -p ys-coding-agent` — 交互式 TUI
- `cargo run -p ys-coding-agent -- -p "任务"` — 单次 Print 模式（边生成边打印文本增量）
- `cargo run -p ys-coding-agent -- --json "任务"` — JSON 事件模式（逐行 `Envelope`）
- `cargo run -p ys-coding-agent -- --json --stats "任务"` — 额外输出背压统计到 stderr
- `cargo test -p ys-coding-agent e2e -- --ignored --nocapture` — 端到端（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）

环境变量：`YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`；`YUSHAN_CHANNEL_CAPACITY`（信道容量，下限 16）；`YUSHAN_SESSIONS_DIR`（会话目录覆盖，测试用）。

## 核心模型（理解本项目的关键）

### Agent 是无状态执行器（ADR-0010）

`Agent` **不持有**会话、事件出口或模型。这三者由接线器持有，每次运行时经端口传入：

```rust
pub struct AgentPorts<'a> {
    pub model:   Option<&'a dyn Model>,   // None = 未配置，run 返回 ConfigError
    pub session: &'a mut dyn Session,     // 归接线器；/new 换的就是它
    pub events:  &'a mut dyn EventSink,   // 归接线器
}

pub async fn run(&mut self, ports: AgentPorts<'_>, inbox: &Inbox) -> Result<RunSummary, LoopError>;
pub async fn run_turn(&mut self, input: AgentInput, ports: AgentPorts<'_>) -> Result<RunResult, LoopError>;
```

`Agent` 保留的字段只有执行能力：`loop_impl / registry / cancel / limits / cwd / workspace_root / approval / system_prompt`。

### Agent 自己转（actor 模型）

`Agent::run(inbox)` 从队列取消息、跑回合、投事件，直到队列空闲才返回（**不阻塞等待**，接线器按需重驱动）：

- **回合（Turn）边界**拉 `followUp`（`Intent::FollowUp`）—— 排队等下一趟
- **轮（Round）边界**拉 `steering`（`Intent::Steering`，由 `BasicLoop` 处理）—— 跑动中插话
- 每个回合开始调 `events.begin_turn(n)`，使 `Envelope.turn` 正确（从 1 起）
- `QueueMode::OneAtATime`（默认）= 一条一回合；`All` = 合并为**一条** `Message`、单回合

### 队列 = 会话 = 历史

`Inbox`（只装**尚未处理**的消息）+ `Session`（历史）**合起来**才是那条日志；消费 = 把 pending **转移**进 Session（游标前移）。是转移，不是拷贝。

- `Inbox` 用 `Arc<Mutex<Inner>>` 内可变：`push(&self)` 使接线器可在 agent 运行期间投递
- `/new` = 换整个 `Wiring`（新 Session + 新空 Inbox，pending 丢弃，旧文件保留）；**agent 全程不知情**
- 会话落盘 `~/.yushan/sessions/{unix秒}_{纳秒}.jsonl`；启动恢复最近一个；`/new` 时**立即落盘空文件**（否则重启恢复不到新会话）

### 事件信道

`Envelope { source, turn, event }` —— 不传裸事件，`source` 为多 agent 协作预留。

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

## coding-agent 模块说明

| 模块 | 职责 |
|------|------|
| `wiring` | **接线器**：持有 `model` + `session` + `inbox`，`ports()` 产出 `AgentPorts`；`/new` / `set_model` / `session_messages` |
| `channel` | `ChannelSink`：有界 mpsc + overflow 缓冲 + `begin_turn` + 背压统计（`format_stats`） |
| `config` | Config 结构体，持有 ProviderRegistry，管理运行时配置 |
| `provider` | ProviderRegistry：内置 provider 目录、auth.json 持久化（`~/.yushan/`）、GET /v1/models 动态获取、ProviderCompat 映射 |
| `commands` | Command trait + CommandRegistry + 内置命令（/login /logout /model /help /new /compact /status /copy /export /quit） |
| `prompt` | 系统提示词构建 |
| `view` | `AppView` 显示快照（provider/model/tokens/turn_count/session/cwd/tools…），唯一显示数据源 |
| `ui` | ratatui TUI：四路 `select!`（turn / 键鼠 / 事件 / 动画节拍）、增量渲染、退出打印完整对话 |

关键设计：

- **`CommandContext` 只持有 `&mut Wiring` + `&mut Config` + `&mut StateStore` + `&dyn Prompter`** —— **不持有 `Agent`**（slash 命令不伸手进运行时）
- `ProviderRegistry` 作为 `Config` 的 pub 字段，命令通过 `ctx.config.registry` 访问
- 凭证持久化到 `~/.yushan/auth.json`（0o600），启动时自动恢复
- **三种入口互斥**：TUI（默认）/ `-p` / `--json`；`-p`/`--json` 用 `Wiring::ephemeral`（MemorySession 不落盘）
- **`-p`/`--json` 的消费必须与 turn 并发**（`tokio::join!`）—— 有界信道若无并发消费者，第一次撞满即死锁
- **消费循环以终局事件（`RunFinished`/`RunFailed`）为终止条件**，不是等信道关闭（sender 在 Agent 里，等关闭必死锁）
- 数据/显示分离：`AppView::from_sources` 是唯一显示数据源快照；`App` 管 UI 交互状态

## 硬性约束

以下是 `docs/design.md` 不变量 + 历次 ADR 的摘要。**新增代码违反任何一条都算设计偏离，需先改设计文档。**

- **依赖方向单向**：`ys-core` ← 组件接口 ← loop/runtime ← 应用适配器
  - **`ys-core` / `ys-event` / `ys-channel` 不含 tokio**（直接与间接皆无）—— 它们只装数据与纯 trait
  - **`ys-component` 不直接依赖 tokio**（自身代码不用 tokio API），但经 `ys-session` **间接**引入（ADR-0012）
  - `ys-session` / `ys-loop` / `ys-runtime` 可直接依赖 tokio（文件 IO、工具超时）
- **最小核心**：Runtime 只提供一次 Agent Turn 所需能力；文件系统、Shell、MCP、TUI、记忆、子 Agent 均为可选组件
- **不做安全**：沙箱、隔离、权限、审批、多租户由上层项目解决；`ToolRegistry` 只按名称查找工具
- **四者边界不可混淆**：Event 观察「发生了什么」，Hook 决定「下一步怎么处理」，Component 提供能力，Loop 决定推进规则
- **静态组合优先**：能力通过 crate + Cargo feature 组合；动态插件只做运行时扩展，第一版不支持热插拔
- **不跨动态库边界传递 Rust trait object、Tokio 类型或跨库所有权对象**
  - 推论：**`ModelEventSink` 必须保持同步 `fn`**（async trait 跨动态库边界困难）—— 这是 try/await 双路径存在的首要理由
- **Agent 无状态**（ADR-0010）：不持有 session / events / model；会话、配置、历史归接线器
- **信道契约**（ADR-0009 / 0011）：有界、容量可配；`try_emit` 的 `Err` 仅表示消费者消失；满时内部缓冲
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
| 架构决策的 why | `docs/adr/`：0001 工具失败双通道、0002 协作取消、0003 Session::append 异步、0004 v0 分层骨架、0007 Agent 公开 API、0008 cancel_handle、0009 事件信道异步化、0010 actor 模型（Agent 无状态）、0011 信道设计、0012 运行时依赖边界 |
| **当前实现的实际形态**（含实现与设计的分歧） | `docs/design-final/core-channel.md` |
| 核心信道的设计推导与质量分析 | `docs/arch/gap-closure/{context,design-core-channel,review}.md` |
| 各 crate 的 API 和实现细节 | 对应 crate 的 `src/lib.rs` |
| coding-agent 产品层设计 | `apps/coding-agent/src/{main,wiring,channel}.rs` + `docs/arch/` |
| /login & /model 行为设计 | `docs/arch/commands/login-model-behavior/` |
