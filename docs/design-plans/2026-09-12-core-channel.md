# Design Plan: 核心信道 + Actor 模型（第一刀：迁移步 0-4）

> 输入：已批准计划 `temporal-inventing-mist`（权威任务分解与测试合同）
> 设计：`docs/arch/gap-closure/design-core-channel.md`（§3/§4/§5）
> 决策：`docs/adr/0011-core-channel-design.md`、`docs/adr/0012-runtime-dependency-boundary.md`
> 质量分析：`docs/arch/gap-closure/review.md`（3 个 🔴 / 5 个 🟡 已修订为 R1-R8）
> 执行计划：`docs/exec-plans/2026-09-12-core-channel.md`
> 日期：2026-09-12
> 编排：`full`（多 crate + 新 crate + trait 变更 + 集成测试，需 design-plan + exec-plan 留档）

## 1. 背景与目标

### 1.1 现状缺口（均已核对代码）

| 缺口 | 证据 |
|---|---|
| `EventSink` 仍是**同步单方法** | `crates/agent-event/src/sink.rs`：`fn emit(&mut self, event: AgentEvent) -> Result<(), EventError>` 仅 1 个方法 |
| `--json` 全仓**零代码** | `apps/coding-agent/src/main.rs` 无任何 JSON 输出分支 |
| `-p` 只 `println` 最终文本 | `main.rs:165` 附近，仅打印 `text`，无增量 |
| `main.rs` 的 `NoopEventSink` 写死 → **事件全丢** | `main.rs:139` `.events(NoopEventSink)` |
| TUI 硬编码 `EventStream`，**无 seam** | `apps/coding-agent/src/ui/` |
| `tokio` 的 `sync` feature **全仓无人显式声明** | `apps/coding-agent/Cargo.toml`：`features = ["rt-multi-thread", "macros", "io-util", "fs"]`，`mpsc` 现在能编译纯属 `reqwest→hyper` 传递启用 |

### 1.2 目标（第一刀 = 迁移步 0-4）

把设计的第一刀落地：**重命名 → 事件出口打通 → 消息模型自转**。

1. **步 0**：`agent-*` → `ys-*` 全仓改名，为新增 `ys-channel` 统一命名空间。
2. **步 1**：`EventSink` 改 **try/await 双路径** + `begin_turn`。这是保住 `ModelEventSink` 同步性（ADR-0004 点 2 的最小 ABI 面）的前提。
3. **步 2**：新建 `ys-channel` 契约 crate + `ChannelSink` 接线器 + `main.rs` 模式分发（`--json` / `-p` 流式 / 交互式）。
4. **步 3**：`RuntimeContext.inbox` + `BasicLoop` **轮边界 steering**。
5. **步 4**：`Agent::run(inbox)` 自转驱动，`run_turn` 降级为内部 `run_one_turn`。

### 1.3 非目标

见 §6「明确不做」。要点：步 5（ADR-0010 所有权收敛）、步 6（正交优化）、TUI 增量渲染、`docs/adr/*` 与 `docs/arch/*` 的文档重命名，**均不在本轮**。

## 2. 关键前置发现（决定实现方式）

| 发现 | 处置 |
|---|---|
| `tokio` 的 `sync` feature **全仓无人显式声明**，`mpsc` 现在能编译纯属 `reqwest→hyper` 传递启用 | 步 2 必须在 `apps/coding-agent/Cargo.toml` **显式加** `"sync"`（别依赖传递启用） |
| 重命名目标 `ys-model-openai-compat` 是**缩短**的（与 `agent-model`→`ys-model` 不同构） | 替换须**先长后短**，不可 blanket sed |
| **7 个非包名标识符 + 4 个字符串字面量**会被前缀规则误伤（如 `"yushan_agent_test"`） | 只按**包名全称**逐条替换；白名单见 §3.0 |
| `ModelEventSink` 侧可**零改动**（try/await 的功劳） | **这是验收点**：若它被迫 async，说明实现走偏 |
| `EventError::SendFailed` 路径**当前 0 覆盖** | 需新建失败 sink 替身（任务 1.2） |
| `RuntimeContext` 是 `#[non_exhaustive]` + 10 参数 `new()`；构造点恰 4 处 | 用 `with_inbox()` 链式，把步 3 影响面从 4 压到 **1**（`ys-runtime/src/agent.rs:67`） |
| 轮边界落点：`basic.rs:116`（compact 块结束）→ `:119`（`rounds += 1`）之间 | steering 注入点 |
| `main.rs` 逻辑全内联在 `#[tokio::main] main()`，模式判定在 agent 构建**之后** | 步 2 重排为「**先定模式 → 选消费者 → 建 agent**」 |
| `basic.rs` 的 `ctx.events.emit(..)` 调用点恰 **8 处**（`:47 :68 :78 :95 :136 :184 :221 :295`） | 步 1 逐处改为 `emit(ctx.events, ..).await` |
| **既有 flaky 测试**（与本轮改造无关）：`adapters/tools-basic/src/{read,write,edit}.rs` 各有逐字重复的 `test_dir()`，目录名只用 `SystemTime::now()` 的纳秒；macOS 时钟仅微秒精度 → 并行/连跑时多个测试拿到同一 `id`、共用同一临时目录，先跑完的 `remove_dir_all` 删掉他人文件 → `Failed to read ...: No such file or directory`（实测 10 次连跑失败 2 次，`--workspace` 下更易触发） | 列为本轮的**前置任务 Task 0 / 0T**（见 exec-plan）：`test_dir()` 加**进程内原子计数器 + `std::process::id()`**，保留 nanos 前缀作跨进程保险，三处保持一致（本轮不抽公共 helper）。**不修则「`cargo test` 全绿」这条验收门不可靠** |
| **该缺陷有 3 处同型实例，均已修**：① `adapters/tools-basic/src/{read,write,edit}.rs` 的 `test_dir()`（即本行所述，Task 0）；② `apps/coding-agent/src/prompt.rs` 的 `test_format_cwd_tilde_*`（4 个测试并行 `set_var`/`remove_var("HOME")` 竞态，`static ENV_LOCK: Mutex<()>` + `EnvRestore` 修复，实测 12% → 0/300）；③ `apps/coding-agent/src/commands/builtin.rs` 的 `test_config()` / `test_state_store()`（nanos-only 命名且从不清理，原子计数器 + `process::id()` 修复，并行进行中） | 三处**均已修**，并入本轮前置任务 Task 0 / 0T（详见 exec-plan Task 0 的 `scope_note`） |
| **已知但本轮不修**的同类脆弱点：`crates/agent-session/src/jsonl.rs:99/144/173/197` 的 4 个硬编码 `test_jsonl_*.jsonl` 文件名；`apps/coding-agent/src/state.rs:86`、`provider.rs:273`、`tests/integration.rs:66/121` 的固定名 | jsonl.rs：单次运行内各测试文件名互异、不互踩，仅**并发 `cargo test` 进程**互踩，低危；固定名者每测试独有、无同 tick 碰撞。本轮**仅记录** |

## 3. 分步设计

> **任务分解补充**：exec-plan 里的 **Task 0 / Task 0T** 为**前置修复**，**不属于**迁移步 0-4（不改变本节各步的内容与顺序），但它是「`cargo test` 全绿」这条验收门的必要条件，故排在最前执行。

### 3.0 步 0 — 重命名（任务 0.1）

**动作**：目录 `git mv` + 包名 + `use` 标识符。规模：205 处 `.rs` / 45 文件；96 处 `.toml` / 12 文件。

**映射表**（与 context.md「命名方案 A」一致）：

| 现名 | 新名 |
|---|---|
| `agent-core` … `agent-runtime` | `ys-core` `ys-event` `ys-model` `ys-tool` `ys-session` `ys-component` `ys-loop` `ys-runtime` |
| `agent-model-openai-compatible` | `ys-model-openai-compat`（**缩短**） |
| `agent-tools-basic` | `ys-tools-basic` |
| `yushan-coding-agent` | `ys-coding-agent` |

`use` 标识符同步：`agent_core` → `ys_core`，以此类推（`agent_model_openai_compatible` → `ys_model_openai_compat`）。

**目录**：`crates/agent-*` → `crates/ys-*`（目录名与包名同构）。`adapters/model-openai-compatible`、`adapters/tools-basic` 目录名**保持不变**（历史上就不带 `agent-` 前缀），仅改包名与 `use` 标识符。

**白名单（不得改动）**：`test_agent_model_id`×2、`test_agent_event_serde_roundtrip`、`test_agent_*`×5、`"yushan_agent_test"`、`agent_tools_basic_*_test_`×3。（按已批准计划口径：7 个标识符 + 4 个字符串字面量。）

**同步更新**：根 `Cargo.toml` `members`、全仓所有 `path = "..."` 依赖、`docs/design.md` §9 crate 树。

**保留原名**：`docs/adr/*`、`docs/arch/*`（历史记录，不改）。

### 3.1 步 1 — `EventSink` 改 try/await 双路径（任务 1.1 / 1.2）

**任务 1.1｜trait + impls + loop 适配**（**原子**：不一起改编译不过）

- `crates/ys-event/src/sink.rs`：`EventSink` 三方法——
  - `fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>`（同步快路径）
  - `fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>>`（异步慢路径）
  - `fn begin_turn(&mut self, _turn: u32) {}`（默认 no-op）
- 新增自由函数 `pub async fn emit(sink: &mut dyn EventSink, event: AgentEvent) -> Result<(), EventError>`：组合两条路径，稳态不产生 Future。
- `NoopEventSink` / `CollectingSink` **拆两方法**实现。
- `crates/ys-loop/src/basic.rs`：8 处 `ctx.events.emit(..)` → `emit(ctx.events, ..).await`；`Forwarder::emit`（`:24-38`）改为调 `try_emit`（**同步，1 处**）。
- `ys-event` **不加 tokio 依赖**（`Pin`/`Future` 来自 std/core）——符合 ADR-0012 的「纯契约层」边界。
- ⚠ **不得触碰 `ModelEventSink`**。这是任务 1.1 的核心验收点。

**任务 1.2｜失败 sink 替身**（测试基建，本次最大测试债）

- 在 `ys-event` **生产导出** `FailingSink`（照 `MockModel` 先例：`ys-model` 的 `mock.rs` 是生产导出而非 `#[cfg(test)]`），支持「从第 N 次 emit 起返回 `Err(EventError::SendFailed)`」。
- 补 `EventError::SendFailed → LoopError::Event` 的**首个**覆盖测试。

### 3.2 步 2 — `ys-channel` 契约 + 信道 + 输出（任务 2.1 - 2.4）

**任务 2.1｜新建 `crates/ys-channel`**

- 装 `Envelope { source, turn, event }` / `Source(Arc<str>)` / `Inbox`(`Arc<Mutex<Inner>>`) / `QueueMode` / `LifecyclePolicy`。
- 依赖：`ys-core` + `ys-event` + serde。**零 tokio**（ADR-0012：契约层不得依赖运行时；理由不是「避免拖进 tokio」，那个理由不成立）。
- 单测照 `ys-event` 模板：内联 `#[cfg(test)]`、**纯同步 `#[test]`**。
- 文件：`lib.rs` / `envelope.rs` / `inbox.rs` / `lifecycle.rs`。

**任务 2.2｜`ChannelSink`（接线器）**

- 有界 `tokio::sync::mpsc` + `overflow: VecDeque<AgentEvent>` + `policy` + `begin_turn` + `stats()`。
- overflow **在 sink 内、不在 `Forwarder`**（review R2：`Forwarder` 生命周期是单次 `model.complete()`，`basic.rs:129-140`，缓冲随其 drop 会丢终局事件）。
- `try_emit` 撞满 → 缓冲，**不算失败**；只有「消费者消失」才失败（按 `policy` 处置）。
- `apps/coding-agent/Cargo.toml` 显式加 tokio `"sync"`。
- 落点：`apps/coding-agent/src/channel.rs`（新建）。

**任务 2.3｜`main.rs` 模式分发重构 + `--json`**

- 抽 `parse_args() -> Args`（可单测，脱离 argv）。
- 重排为「**先定模式 → 选消费者 → 建 agent**」（现逻辑：模式判定在 agent 构建之后，`main.rs:94` 起的 arg 循环晚于 `:130` 的 builder）。
- 抽 `write_json_envelope<W: Write>(w, &Envelope)`（可单测，**不依赖 argv**）。
- **消费与 turn 必须 `tokio::join!` 并发**——串行会死锁（有界信道 + 消费者不跑 = 生产者阻塞）。
- **消费以「终局事件」为终止条件，不以信道关闭**（信道关闭在 agent 结束后才发生，会漏掉协程 join 时机）。

**任务 2.4｜`-p` 流式**

- 抽**可注入 writer** 的流式打印函数；增量文本（`AgentEvent::ModelTextDelta`）边收边打。
- 不变式：**增量拼接 == 终态**。

### 3.3 步 3 — `Inbox` 接入 + 轮边界 steering（任务 3.1）

- `ys-component` 的 `RuntimeContext` 加 `pub inbox: Option<&'a Inbox>` + `with_inbox()`（链式，把 10 参数 `new()` 的影响面压到 1 个构造点）。
- `basic.rs` **轮边界**（`:116` compact 块结束 → `:119` `rounds += 1` 之前）drain steering → `session.append` + emit `UserMessage`。
- **不得增 `rounds`、不得改 turn**（配合 `begin_turn` 显式化——review R4：从 `UserMessage` 推导 turn 会被 steering 注入误增）。
- `inbox = None` 时行为与今日**逐字节一致**（验收点，存量测试即证）。

### 3.4 步 4 — 自转驱动（任务 4.1）

- `ys-runtime`：`run(&mut self, inbox: &Inbox) -> Result<RunSummary, LoopError>`。
- `run_turn` 降级为**内部** `run_one_turn`（原语义保持，测试仍可直接调用）。
- **回合边界** drain followUp；inbox 空 → 返回。
- **不收 policy**（review R8：策略只由 sink 持有，它才知道消费者是否消失）。
- `/new` 用新 `Inbox`（本轮不做命令层，留步 5）。

### 3.5 涉及文件汇总

```
crates/ys-channel/**                            新建（lib.rs / envelope.rs / inbox.rs / lifecycle.rs）
crates/ys-event/src/{sink,noop,collecting}.rs   trait 三方法 + 两个 impl + FailingSink
crates/ys-loop/src/basic.rs                     8 处 emit + Forwarder + 轮边界 steering
crates/ys-component/src/context.rs              inbox 字段 + with_inbox
crates/ys-runtime/src/{agent,builder}.rs        run / run_one_turn / RunSummary
apps/coding-agent/src/main.rs                   parse_args + 模式分发 + --json + -p
apps/coding-agent/src/channel.rs                新建（ChannelSink）
apps/coding-agent/Cargo.toml                    加 tokio "sync"
Cargo.toml（根）                                 members
docs/design.md                                   §9 crate 树
```

## 4. 关键契约（接口形状）

```rust
// ============ crates/ys-channel/（契约层：纯数据 + 枚举，依赖 ys-core + ys-event）============

// envelope.rs
/// 事件来源。`Arc<str>` 使 clone 只做一次原子加（避免每事件一次堆分配），
/// 同时序列化为可读字符串（`--json` 需要）。（review R7）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source(Arc<str>);              // v0: 单值 "agent"
impl Source { pub fn agent() -> Self { Self("agent".into()) } }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope { pub source: Source, pub turn: u32, pub event: AgentEvent }

// lifecycle.rs
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecyclePolicy {
    StopWhenConsumerGone,      // 消费者消失 → 干完当前轮收摊（交互式默认）
    ContinueWithoutConsumer,   // 消费者消失 → 继续跑（事件落盘兜底；后台长任务）
}

// inbox.rs
pub enum Intent { Steering, FollowUp }
pub enum QueueMode { All, OneAtATime }     // OneAtATime 默认：每条一回合
                                           // All：取走该队列全部 → 合并为一条 Message → 单回合
/// 入站队列：只装「尚未处理」的消息。pending + 会话历史合起来才是「队列 = 日志 + 游标」里的那条日志；
/// 消费 = 转移进 Session，故无重复持有。（review R1/R3）
#[derive(Clone)]
pub struct Inbox { inner: Arc<Mutex<Inner>> }
impl Inbox {
    // 生产者侧（接线器；可在 agent 运行中调用）
    pub fn push(&self, message: Message, kind: Intent);
    pub fn close(&self);
    // 消费者侧（agent）：返回 owned（取走即移出队列）
    pub fn take_steering(&self) -> Vec<Message>;
    pub fn take_followup(&self) -> Vec<Message>;
    pub fn is_empty(&self) -> bool;
    pub fn is_closed(&self) -> bool;
}
```

```rust
// ============ ys-component：RuntimeContext 增字段 ============
pub struct RuntimeContext<'a> {
    // ... 现有字段不变 ...
    /// 轮边界可查的掌舵队列（None = 行为与今日逐字节一致）
    pub inbox: Option<&'a Inbox>,
}
impl<'a> RuntimeContext<'a> {
    pub fn with_inbox(self, inbox: &'a Inbox) -> Self;   // 链式
}
```

```rust
// ============ ys-runtime：自转驱动（ADR-0010 留空的那块）============
impl Agent {
    /// 自转：从 inbox 取消息 → 跑回合 → 投事件，直到 inbox 空闲。
    /// 不收 policy（review R8）——策略只由 sink 持有。遇 `emit` 报错即终止。
    pub async fn run(&mut self, inbox: &Inbox) -> Result<RunSummary, LoopError>;
    /// 内部：跑一个回合。原 `run_turn`，保持可用（测试直接调用）。
    async fn run_one_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError>;
}
pub struct RunSummary { pub turns: u32, pub usage: Usage, pub last_stop: Option<StopReason> }
```

```rust
// ============ 接线器（apps/coding-agent/src/channel.rs，唯一持有 tokio::mpsc）============
pub struct ChannelSink { tx, overflow, source, turn, policy }
impl EventSink for ChannelSink {
    fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>;  // 满→缓冲；关闭→按 policy
    fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<Box<...>>;        // 先冲 overflow，再发本次
    fn begin_turn(&mut self, turn: u32);                                  // 显式设 turn（不从事件推导）
}
/// 背压可观测（review R6）
pub struct ChannelStats { pub backpressure_waits: u64, pub buffered: usize, pub consumer_gone: bool }
impl ChannelSink { pub fn stats(&self) -> ChannelStats; }
```

## 5. 测试策略（本章为本次重点）

### 5.1 分层原则（分 crate 测试）

沿用现状，**不引入新基建**：

| 层 | crate | 测试方式 | tokio |
|---|---|---|---|
| 契约层 | `ys-core` `ys-event` **`ys-channel`** `ys-component` | 内联 `#[cfg(test)]`，**纯同步 `#[test]` 优先** | dev-dep 有，但**能不用就不用** |
| 执行层 | `ys-loop` `ys-session` `ys-runtime` | 内联 + `#[tokio::test]` | 生产依赖 |
| 产品层 | `ys-coding-agent` | 内联 `mod tests`（现 11 个） | 生产依赖 |

**理由**：契约层装的是纯数据 + 枚举 + 纯 trait，同步可测——用了 `#[tokio::test]` 反而掩盖「契约层是否真无运行时依赖」这一约束（ADR-0012）。

### 5.2 共享 test double（本轮不新建 `ys-test-support` crate）

现状是**各写各的**（`make_ctx` 在 `ys-component` 与 `ys-loop` 重复 2 次、`tools-basic` 里重复 4 次）。本轮**不新建 `ys-test-support` crate**（避免 scope creep），但：

- `FailingSink`（任务 1.2）**放 `ys-event` 生产导出**——照 `MockModel`（`ys-model/src/mock.rs` 生产导出，`lib.rs:11 pub use mock::*`）的既有先例，避免第 3 次重复定义。
- 其余 helper **维持现状**。

### 5.3 集成测试（两个落点）

| 落点 | 内容 | 为何在这 |
|---|---|---|
| `crates/ys-runtime/tests/` | 自转语义（`Agent::run` / steering / 消费者消失） | 已有 `v0_integration.rs`（9 测试）是唯一跨 crate 集成测，模板现成 |
| `apps/coding-agent/tests/` | 输出模式（`--json` 行序列 / `-p` 流式） | 已有 `integration.rs`；产品层行为在这测 |

新增建议文件名：`crates/ys-runtime/tests/actor_run.rs`、`apps/coding-agent/tests/json_output.rs`。

### 5.4 磁盘隔离的坑

`StateStore::set_override` / `ProviderRegistry::set_auth_override` 是 `#[cfg(test)] pub(crate)`，**集成测试（外部 crate）用不了**。

- 本轮集成测试**不需要磁盘**（测的是事件流），故**不踩此坑**。
- 若将来需要，走 `HOME` 重定向（Rust 2024 下 `set_var` 为 `unsafe`），不要试图直接用那两个 override。

### 5.5 测试合同（16 条必测 + 2 条 MANUAL_ACK）

| # | 合同项 | 验收项 | 方法 | 优先级 |
|---|---|---|---|---|
| 1 | 0.1 | 重命名无残留、无误伤 | `cargo build && cargo test --workspace && cargo clippy --all-targets` 全绿；`grep` 确认 4 个白名单字面量未被改 | **必测** |
| 2 | 1.1 | try/await 双路径回归 | 存量 **178** 测试全绿 | **必测** |
| 3 | 1.1 | `try_emit` 满/关路径 | 新增单测 | **必测** |
| 4 | 1.1 | `emit` 慢路径、`begin_turn` 默认 | 新增单测 | **必测** |
| 5 | 1.1 | **`ModelEventSink` 未被触碰** | `git diff` 确认其签名与 6 个形参位零改动 | **必测** |
| 6 | 1.2 | 失败路径首次覆盖 | `FailingSink` → 断言 `LoopError::Event(SendFailed)` | **必测** |
| 7 | 2.1 | 契约类型正确 | `Envelope` serde 往返、`Source`、`QueueMode`、`LifecyclePolicy` 构造/比较 | **必测** |
| 8 | 2.1 | `Inbox` 语义 | steering/followUp 分离；`All` 合并为一条 `Message` vs `OneAtATime` 每条一回合；`push(&self)` 可在 `run` 期间调用 | **必测** |
| 9 | 2.2 | 信道行为 | 投递→收 `Envelope`；满→**缓冲不丢**；接收端关闭→`StopWhenConsumerGone` 报错 / `ContinueWithoutConsumer` 吞掉；`emit().await` 先冲 overflow；`begin_turn` 生效 | **必测** |
| 10 | 2.3 | 参数解析 | `parse_args` 单测：`-p` / `--json` / 无参 | **必测** |
| 11 | 2.3 | `--json` 输出 | 纯函数测序列化形态 + 集成测行序列（MockModel + ChannelSink） | **必测** |
| 12 | 2.4 | `-p` 流式 | 集成测：增量文本按序到达（可注入 writer） | **必测** |
| 13 | 3.1 | 轮边界注入 | 轮边界注入后模型看到新 user 消息；**不增 rounds** | **必测** |
| 14 | 3.1 | 向后兼容 | `inbox = None` 时行为与今日一致（存量测试即证） | **必测** |
| 15 | 4.1 | 自转语义 | followUp 排队→自动下一趟；inbox 空→立即返回；消费者消失→收摊；`run_one_turn` 语义保持 | **必测** |
| 16 | 4.1 | 自转集成 | `crates/ys-runtime/tests/` 跨 crate 集成测（`Agent::run` + ChannelSink） | **必测** |
| — | — | 真实 API 端到端 | `cargo test -p ys-coding-agent e2e -- --ignored` 需凭证 | MANUAL_ACK_REQUIRED |
| — | — | TUI 终端观感 | 渲染有 `TestBackend` 可测，但**实际终端观感**需人工确认 | MANUAL_ACK_REQUIRED |

合同项与 exec-plan 任务的对应关系：1→Task 2；2-5→Task 4；6→Task 6；7-8→Task 8；9→Task 10；10-11→Task 12；12→Task 14；13-14→Task 16；15-16→Task 18。

### 5.6 验证（端到端）

1. `cargo build --workspace`
2. `cargo test --workspace`（期望：存量 178 + 新增 ≈ 210+ 全绿，1 ignored）
3. `cargo clippy --all-targets`（无 warning）
4. `cargo fmt --check`
5. **手工验证 `--json`**：`cargo run -p ys-coding-agent -- --json "hello"` → 逐行 JSON 事件，末行为终局事件
6. **手工验证 `-p` 流式**：`cargo run -p ys-coding-agent -- -p "hello"` → 文本增量可见
7. （可选，需凭证）`cargo test -p ys-coding-agent e2e -- --ignored`

## 6. 明确不做（本轮）

| 不做项 | 原因 |
|---|---|
| **步 5**（ADR-0010 所有权收敛：session/events 移出 `Agent`、`/new` 命令层、`CommandContext` 不再持 `&mut Agent`） | 与信道解耦，可最后做或不做；一次改动过大 |
| **步 6**（正交优化：`Arc<Vec<Message>>` 请求快照、`Bytes` delta、`Usage`/`StopReason: Copy`、`max_rounds`/`bash_timeout` 可配、事件落盘） | 与「信道 + Actor 模型」正交；混入会放大改动、稀释可评审性 |
| **TUI 增量渲染** | 设计明确维持现状 |
| **`docs/adr/*`、`docs/arch/*` 的文档重命名** | 保留为历史记录 |

**扩展点（记录，不在本轮建）**：多消费者 `EventBus` fan-out、群聊 `Projection`/`SharedLog`、第三四种队列、第三种生命周期策略、agent 休眠唤醒、可替换 `TurnDriver`——见 design-core-channel.md §6，将来按需回访，**不改本设计的结构**。
