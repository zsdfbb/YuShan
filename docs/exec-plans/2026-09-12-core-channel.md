# Execution Plan: 核心信道 + Actor 模型（第一刀：迁移步 0-4）

> 设计计划：`docs/design-plans/2026-09-12-core-channel.md`
> 每个 `type: impl` 任务紧跟一个 `type: test` 任务配对（共 9 对 / 18 条）。
> 任务编号前缀对应已批准计划的「步.子任务」编号（0.1 / 1.1 / 1.2 / 2.1 / 2.2 / 2.3 / 2.4 / 3.1 / 4.1）。
> 另有 1 对**前置修复**任务（Task 0 / 0T，标题带「前置」前缀），**不计入**上述 18 条，也不属于迁移步 0-4。

## Task 0: 前置 — 修 test_dir 竞态（既有 flaky 测试）
- type: impl
- files: `adapters/tools-basic/src/read.rs`, `write.rs`, `edit.rs`
- scope_note: |
    实施期经独立 review 扩展：本任务由原 3 个文件扩至 **5 个文件**（同类竞态），
    review 暴露的同类路径共 3 个：
    1. `apps/coding-agent/src/prompt.rs`（**已修**，即本任务新增的第 4 个文件）——
       `test_format_cwd_tilde_*` 4 个测试并行 `set_var`/`remove_var("HOME")` 造成竞态；
       修法：`static ENV_LOCK: Mutex<()>` + `EnvRestore`（Drop 恢复）；
       实测修复前 12%（36/300）→ 修复后 0/300。
    2. `apps/coding-agent/src/commands/builtin.rs`（**并行修复中**）——
       `test_config()` / `test_state_store()` 用 nanos-only 命名临时目录（同型缺陷），
       且从不清理；修法：原子计数器 + `process::id()`。
    3. `crates/agent-session/src/jsonl.rs`（**仅记录，本轮不修**）—— 4 处硬编码
       `test_jsonl_*.jsonl` 文件名；单次运行内各测试文件名互异、不互踩，
       仅并发 `cargo test` 进程互踩，低危。
    扩展原因：均为「进程内不唯一命名 / 并行改环境变量」的**同型竞态**，与 Task 0 同源，
    故并入同一前置修复。
- description: |
    每个文件的 `#[cfg(test)] mod tests` 里的 `test_dir()` 加进程内唯一性：
    - 加 `static COUNTER: AtomicU64 = AtomicU64::new(0);`
    - 目录名带上 `std::process::id()` 与 `COUNTER.fetch_add(1, Relaxed)`
    - 保持原有的 nanos 前缀（跨进程额外保险）
    - 三处改动保持逐字一致（现状是 3 份重复，本轮不抽公共 helper）
    另外：**调查并汇报**（不修改）其他测试是否有同类竞态 —— 重点看
    `crates/agent-session/src/jsonl.rs`（硬编码 `test_jsonl_*.jsonl` 文件名）
    与 `apps/coding-agent/src/{provider,state}.rs`（固定名 `yushan_test_{name}`）。
- priority: P0

## Task 0T: 前置 — test_dir 竞态回归验证
- type: test
- files: `adapters/tools-basic/src/{read,write,edit}.rs`
- description: |
    1. `cargo test -p agent-tools-basic --lib` 连跑 10 次，**要求 0 次失败**
       （修复前实测 10 次失败 2 次）
    2. 断言各 `test_dir()` 的目录名互不相同（可在任一文件加一条
       `#[test]`：连调两次 `test_dir()`，断言返回值不等）
    3. `cargo test --workspace` 全绿
- test_method: `for i in $(seq 1 10); do cargo test -p agent-tools-basic --lib; done`
- priority: P0

## Task 1: 步 0.1 — 重命名 agent-* → ys-*
- type: impl
- files: `crates/agent-* → crates/ys-*`（目录 `git mv`）、全仓 `Cargo.toml`（12 文件）、全仓 `.rs`（45 文件）、`Cargo.toml`（根）、`docs/design.md`（§9 crate 树）
- description: |
    全仓机械重命名，规模约 205 处 `.rs` / 45 文件 + 96 处 `.toml` / 12 文件。

    1. 目录 `git mv`（目录名与包名同构）：
       `crates/agent-core` → `crates/ys-core`、`crates/agent-event` → `crates/ys-event`、
       `crates/agent-model` → `crates/ys-model`、`crates/agent-tool` → `crates/ys-tool`、
       `crates/agent-session` → `crates/ys-session`、`crates/agent-component` → `crates/ys-component`、
       `crates/agent-loop` → `crates/ys-loop`、`crates/agent-runtime` → `crates/ys-runtime`。
       `adapters/model-openai-compatible`、`adapters/tools-basic` **目录名保持不变**（历史上不带 `agent-` 前缀），只改包名。

    2. 包名映射（`[package] name`）：
       `agent-core`…`agent-runtime` → `ys-core`…`ys-runtime`；
       `agent-model-openai-compatible` → `ys-model-openai-compat`（**缩短**，与前者不同构）；
       `agent-tools-basic` → `ys-tools-basic`；`yushan-coding-agent` → `ys-coding-agent`。

    3. `use` 标识符：`agent_core` → `ys_core`、`agent_model_openai_compatible` → `ys_model_openai_compat`，其余同构。
       ⚠ **替换顺序必须先长后短**（`agent-model-openai-compatible` 先于 `agent-model`），否则 blanket sed 会产出 `ys-openai-compatible` 之类的错名。

    4. **白名单（不得改动，逐条 `grep` 核对）**：
       标识符 7 个 —— `test_agent_model_id`×2、`test_agent_event_serde_roundtrip`、`test_agent_*`×5；
       字符串字面量 4 个 —— `"yushan_agent_test"`、`agent_tools_basic_*_test_`×3。
       实现方式：**只按包名全称（含连字符/下划线两种形态）逐条替换**，不做前缀模糊替换。

    5. 同步：根 `Cargo.toml` 的 `members`（8 个 `crates/*` 条目）；全仓所有 `path = "../../crates/agent-*"` 依赖；`docs/design.md` §9 crate 树。

    6. **不改**：`docs/adr/*`、`docs/arch/*`（保留为历史记录）。

    验收断言：
    - `grep -rn "agent_core\|agent_event\|agent_model\|agent_tool\|agent_session\|agent_component\|agent_loop\|agent_runtime" --include=*.rs --include=*.toml .` 无**包名形态**命中（白名单除外）。
    - `grep -rn "yushan-coding-agent" --include=*.toml .` 无命中。
- test_method: `cargo build --workspace && cargo test --workspace && cargo clippy --all-targets && cargo fmt --check`
- priority: P0

## Task 2: 步 0.1 — 重命名验证（无残留、无误伤）
- type: test
- files: 全仓（无新增文件，验证任务）
- description: |
    测试合同第 1 条。重命名是纯机械操作，**验证即测试**——无新增单测，靠全仓编译 + 测试 + 静态检查。

    验收断言（全部必须成立）：
    1. `cargo build --workspace` 成功，无 warning。
    2. `cargo test --workspace` 全绿，测试数与改名前的基线（178 个）**一致**（改名不改行为，数量少了说明误删、多了说明误加）。
    3. `cargo clippy --all-targets` 无 warning（尤其确认没有未使用的依赖残留）。
    4. 白名单核验（4 个字面量必须原样保留）：
       `grep -rn "yushan_agent_test" .` 有命中；
       `grep -rn "test_agent_model_id\|test_agent_event_serde_roundtrip" .` 有命中（各 2 处 / 1 处）；
       `grep -rn "agent_tools_basic_.*_test_" .` 有 3 处命中。
    5. 无误伤：`grep -rn "\bys_model_openai_compat\b" --include=*.rs .` 有命中，且 `grep -rn "ys-openai-compatible\|ys_openai_compatible" .` **零命中**（先长后短顺序的验证点）。
    6. `git status` 中 `docs/adr/`、`docs/arch/` 无改动。
- test_method: `cargo test --workspace && cargo clippy --all-targets`
- priority: P0

## Task 3: 步 1.1 — EventSink 改 try/await 双路径
- type: impl
- files: `crates/ys-event/src/sink.rs`、`crates/ys-event/src/noop.rs`、`crates/ys-event/src/collecting.rs`、`crates/ys-loop/src/basic.rs`、`crates/ys-event/Cargo.toml`
- description: |
    这是**原子改动**——trait 与全部 impl、全部调用点必须一次改完，否则编译不过。

    1. `crates/ys-event/src/sink.rs`（现仅 7 行、单方法）改为三方法：
       ```rust
       pub trait EventSink: Send {
           fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent>;
           fn emit<'a>(&'a mut self, event: AgentEvent)
               -> Pin<Box<dyn Future<Output = Result<(), EventError>> + Send + 'a>>;
           fn begin_turn(&mut self, _turn: u32) {}   // 默认 no-op
       }
       ```
       并在同文件新增自由函数（循环内唯一出口）：
       ```rust
       #[inline(always)]
       pub async fn emit(sink: &mut dyn EventSink, event: AgentEvent) -> Result<(), EventError> {
           match sink.try_emit(event) {
               Ok(()) => Ok(()),
               Err(event) => sink.emit(event).await,
           }
       }
       ```
       `use std::future::Future; use std::pin::Pin;`——`Pin`/`Future` 来自 std/core，**`crates/ys-event/Cargo.toml` 不得新增 tokio 生产依赖**（ADR-0012 的「纯契约层」硬约束）。

    2. `crates/ys-event/src/noop.rs`（现 10 行）：`try_emit` 返 `Ok(())`；`emit` 返 `Box::pin(async { Ok(()) })`。
       `crates/ys-event/src/collecting.rs`（现 40 行）：`try_emit` push 后返 `Ok(())`；`emit` 同样 `Box::pin` 包装。

    3. `crates/ys-loop/src/basic.rs` **8 处调用点**（`:47 :68 :78 :95 :136 :184 :221 :295`）由 `ctx.events.emit(..)` 改为 `emit(ctx.events, ..).await`（自由函数；注意 `:136` 处在 `.map_err` 闭包内，需把 await 提到闭包外或用 `let _ = emit(...).await`）。

    4. `crates/ys-loop/src/basic.rs:24-38` 的 `Forwarder`（`impl ModelEventSink`）：`:27` 的 `self.sink.emit(..)` 改为 `let _ = self.sink.try_emit(agent_event);`（**同步，1 处**；满/关闭都由 sink 内部处置，Forwarder 不做背压）。

    5. ⚠ **不得触碰 `ModelEventSink`**：`crates/ys-model/src/`（`trait_def.rs` / `event.rs`）零改动。若 `ModelEventSink` 被迫 async，说明 try/await 未被正确使用，实现已走偏。

    验收断言：`cargo build -p ys-event -p ys-loop` 通过；`grep -n "ctx.events.emit" crates/ys-loop/src/basic.rs` 零命中。
- test_method: `cargo test -p ys-event && cargo test -p ys-loop`
- priority: P0

## Task 4: 步 1.1 — EventSink 双路径测试 + ModelEventSink 未触碰核验
- type: test
- files: `crates/ys-event/src/lib.rs`（`#[cfg(test)] mod tests`）、`crates/ys-loop/src/basic.rs`（`#[cfg(test)] mod tests`）
- description: |
    测试合同第 2-5 条。

    1. **回归**（合同第 2 条）：存量 178 测试全绿。`crates/ys-event/src/lib.rs` 现有 `test_noop_sink` / `test_collecting_sink` / `test_agent_event_serde_roundtrip` 需按新 trait 形态适配（`try_emit` 直接调、`emit` 需 `.await`，故这两个存量测试可能需改 `#[tokio::test]`——**优先改成只调 `try_emit` 保持纯同步**）。
    2. **`try_emit` 满/关路径**（合同第 3 条）：新增单测覆盖 `try_emit` 返回 `Err(AgentEvent)` 的路径（用 Task 5 的 `FailingSink` 或内联 double），断言事件被**原样交回**（`Err(e)` 的 payload 与传入事件相等）。
    3. **`emit` 慢路径 + `begin_turn` 默认**（合同第 4 条）：
       - 用内联 double（`try_emit` 返 `Err`、`emit` 异步返 `Ok`）断言自由函数 `emit()` 会**落下到慢路径**（计数断言：fast 1 次 + slow 1 次）。
       - 用 `NoopEventSink` 直接调 `begin_turn(7)`，断言编译通过且无副作用（默认 no-op 生效）。
    4. **`ModelEventSink` 未被触碰**（合同第 5 条）：运行 `git diff --stat` 断言 `crates/ys-model/src/` 无改动；`git diff crates/ys-model/src/trait_def.rs` 输出为空。断言 `ModelEventSink::emit` 的签名与 6 个形参位零改动。
    5. 自由函数稳态不产生 Future：断言 `emit()` 在 `try_emit` 成功时**只走快路径**（slow 计数保持 0）。
- test_method: `cargo test -p ys-event && cargo test -p ys-loop`
- priority: P0

## Task 5: 步 1.2 — FailingSink（生产导出的失败替身）
- type: impl
- files: `crates/ys-event/src/failing.rs`（新建）、`crates/ys-event/src/lib.rs`（加 `mod failing; pub use failing::*;`）
- description: |
    本次最大测试债：`EventError::SendFailed` 路径当前 **0 覆盖**。

    1. 新建 `crates/ys-event/src/failing.rs`，**生产导出**（照 `MockModel` 先例——`crates/ys-model/src/mock.rs` 是生产导出、`lib.rs:11` 有 `pub use mock::*`，不是 `#[cfg(test)]`），理由：跨 crate 复用（`ys-loop`、`ys-runtime`、`ys-coding-agent` 的测试都要用），避免第 3 次重复定义。
    2. 形状：
       ```rust
       /// 快路径 `try_emit`：前 `succeed_first` 次成功，之后原样退回 `Err(event)`，
       /// 使自由函数 [`emit`] 能带同一事件回落慢路径。
       /// 慢路径 `emit`：**默认成功**（快失败、慢成功）；`set_fail_slow(true)` 后
       /// 按同一阈值返回 `Err(EventError::SendFailed)`。
       pub struct FailingSink {
           succeed_first: usize,
           try_calls: usize,
           slow_calls: usize,
           emitted: Vec<AgentEvent>,
           fail_slow: bool,   // 默认 false：慢路径成功
       }
       impl FailingSink {
           pub fn new(succeed_first: usize) -> Self { ... }   // 0 表示快路径首次即失败
           pub fn always_ok() -> Self { ... }
           pub fn set_fail_slow(&mut self, fail_slow: bool) { ... }
           pub fn count(&self) -> usize { ... }
       }
       impl EventSink for FailingSink {
           fn try_emit(&mut self, event: AgentEvent) -> Result<(), AgentEvent> { ... }
           fn emit<'a>(&'a mut self, event: AgentEvent) -> Pin<Box<...>> { Box::pin(async move { ... }) }
       }
       ```
       **勘误（实施后回改）**：原描述「两条路径均在第 N 次返回 `Err`」与 Task 4(b) 的
       「快失败、慢成功」不可兼得。实际实现引入 `fail_slow` 开关（默认 `false`）：
       快路径 `try_emit` 在失败时原样退回 `Err(event)`（`Err` 变体是 `AgentEvent`，
       无法承载 `EventError`），由自由函数转入 `emit` 慢路径；慢路径**默认成功**——
       `FailingSink::new(0)` 正是「快路径必失败 + 慢路径可成功」的回落场景。
       要测慢路径也失败（`SendFailed` 直达调用方），显式 `set_fail_slow(true)`。
    3. `crates/ys-event/src/lib.rs`：`mod failing;` + `pub use failing::*;`（与现有 `mod collecting; mod event; mod noop; mod sink;` 并列）。
    4. `crates/ys-event/Cargo.toml` 不变（仍无 tokio 生产依赖）。
- test_method: `cargo test -p ys-event failing`
- priority: P1

## Task 6: 步 1.2 — FailingSink 测试 + SendFailed → LoopError::Event 首个覆盖
- type: test
- files: `crates/ys-event/src/failing.rs`（`#[cfg(test)] mod tests`）、`crates/ys-loop/src/lib.rs`（`#[cfg(test)] mod tests`）、`crates/ys-runtime/src/agent.rs`（`#[cfg(test)] mod tests`）
- description: |
    测试合同第 6 条——`EventError::SendFailed → LoopError::Event` 的**首个**覆盖。

    1. `crates/ys-event/src/failing.rs` 内联单测：
       - `FailingSink::new(2)`：前 1 次 `emit().await` 返 `Ok`，第 2 次起返 `Err(EventError::SendFailed)`。
       - `FailingSink::new(0)`：首次即 `Err`。
       - `count()` 随调用递增。
    2. `crates/ys-loop/src/lib.rs` 集成断言：构建 `RuntimeContext` 时把 `events` 指向 `FailingSink::new(0)`，跑 `BasicLoop::run_turn`，断言返回 `Err(LoopError::Event(e))` 且 `matches!(e, EventError::SendFailed)`。
       ⚠ 注意：`ctx.events` 是 `&mut dyn EventSink`，`FailingSink` 需先 `Box::leak` 或放入 `Box` 后取 `&mut`（照现有 `make_ctx` helper 的写法）。
    3. 断言「终局事件不变式」在失败路径上的行为符合设计预期：`emit` 失败即终止当前 turn（复用既有语义），不静默续跑。
- test_method: `cargo test -p ys-event failing && cargo test -p ys-loop send_failed`
- priority: P1

## Task 7: 步 2.1 — 新建 crates/ys-channel 契约 crate
- type: impl
- files: `crates/ys-channel/Cargo.toml`（新建）、`crates/ys-channel/src/lib.rs`（新建）、`crates/ys-channel/src/envelope.rs`（新建）、`crates/ys-channel/src/inbox.rs`（新建）、`crates/ys-channel/src/lifecycle.rs`（新建）、`Cargo.toml`（根，`members` 加入）
- description: |
    契约层 crate：**纯数据 + 纯枚举，零 tokio**（ADR-0012）。

    1. `crates/ys-channel/Cargo.toml`：
       ```toml
       [dependencies]
       ys-core = { path = "../ys-core" }
       ys-event = { path = "../ys-event" }
       serde = { version = "1", features = ["derive"] }
       serde_json = "1"
       [dev-dependencies]
       tokio = { version = "1", features = ["full"] }   # 允许有，但单测「能不用就不用」
       ```
       **生产依赖不得出现 tokio**。
    2. `envelope.rs`：
       `pub struct Source(Arc<str>);`（`impl Source { pub fn agent() -> Self }`）+ `#[derive(Clone, Debug, Serialize, Deserialize)]`；
       `pub struct Envelope { pub source: Source, pub turn: u32, pub event: AgentEvent }`（同 derive）。`Arc<str>` 是 review R7 的落法：clone 仅一次原子加，且 `--json` 可读。
    3. `lifecycle.rs`：`#[non_exhaustive] #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum LifecyclePolicy { StopWhenConsumerGone, ContinueWithoutConsumer }`。
    4. `inbox.rs`：
       `pub enum Intent { Steering, FollowUp }`；`pub enum QueueMode { All, OneAtATime }`；
       `#[derive(Clone)] pub struct Inbox { inner: Arc<Mutex<Inner>> }`（`std::sync::Mutex`，非 tokio）；
       `struct Inner { steering: VecDeque<Message>, follow_up: VecDeque<Message>, steering_mode: QueueMode, follow_up_mode: QueueMode, closed: bool }`；
       `impl Inbox { pub fn push(&self, message: Message, kind: Intent); pub fn close(&self); pub fn take_steering(&self) -> Vec<Message>; pub fn take_followup(&self) -> Vec<Message>; pub fn is_empty(&self) -> bool; pub fn is_closed(&self) -> bool; }`
       **语义（review R1/R5）**：
       - `push(&self)`——内可变，使接线器可在 agent 运行期间投递（steering 的前提）。
       - `take_*` 取 `&self`、返回 **owned `Vec<Message>`**（取走即移出队列，是「转移」不是「拷贝」）；内部**无 await、不持锁跨 await**。
       - `QueueMode::OneAtATime`（默认）：一次取一条 → 每条一回合。
       - `QueueMode::All`：取走该队列全部 → **合并为一条 `Message`**（多个 `ContentBlock::Text` 依序），**单回合**。
       - steering 与 followUp **两队列分离**，互不干扰。
    5. `lib.rs`：`mod envelope; mod inbox; mod lifecycle; pub use ...::*;`
    6. 根 `Cargo.toml` `members` 加 `"crates/ys-channel"`。
- test_method: `cargo build -p ys-channel && cargo test -p ys-channel`
- priority: P0

## Task 8: 步 2.1 — ys-channel 契约类型与 Inbox 语义单测
- type: test
- files: `crates/ys-channel/src/lib.rs`（`#[cfg(test)] mod tests`，或各模块内联）
- description: |
    测试合同第 7-8 条。**纯同步 `#[test]`，不使用 `#[tokio::test]`**（照 `ys-event` 模板；用了反而掩盖「契约层无运行时依赖」这一约束）。

    1. **契约类型正确**（合同第 7 条）：
       - `Envelope` serde 往返：`serde_json::to_string` → `from_str` → `assert_eq!(env, back)`；断言 JSON 中 `source` 是可读字符串 `"agent"`（`--json` 契约）。
       - `Source::agent()` 构造 + `clone`（断言 clone 后相等）。
       - `LifecyclePolicy` 两变体构造 / `PartialEq` / `Copy`（`let b = a;` 后 `a` 仍可用）。
       - `QueueMode` 两变体构造 / 比较。
    2. **`Inbox` 语义**（合同第 8 条）：
       - steering / followUp **分离**：push 一条 Steering + 一条 FollowUp，断言 `take_steering()` 只出前者、`take_followup()` 只出后者。
       - `QueueMode::OneAtATime`：push 3 条 → `take_steering()` 返 `Vec` 长度 1；再取再返 1 条。
       - `QueueMode::All`：push 3 条 → `take_steering()` 返 **1 条 `Message`**，其 `content` 是 3 个 `ContentBlock::Text` 依序拼接。
       - **`push(&self)` 可在共享引用下调用**：`let inbox = Inbox::new(); let a = inbox.clone(); a.push(msg, Intent::Steering);`（证明接线器可在 `run(&Inbox)` 期间投递——这是 R1 的回归测试，**必须存在**）。
       - `take_*` 是转移而非拷贝：取走后再取返空。
       - `close()` 后 `is_closed()` 为 true；`is_empty()` 在队列空时 true。
- test_method: `cargo test -p ys-channel`
- priority: P0

## Task 9: 步 2.2 — ChannelSink 接线器
- type: impl
- files: `apps/coding-agent/src/channel.rs`（新建）、`apps/coding-agent/src/main.rs`（加 `mod channel;`）、`apps/coding-agent/Cargo.toml`（tokio 加 `"sync"`）
- description: |
    唯一持有 `tokio::sync::mpsc` 的地方（ADR-0011 点 1：契约不指定传输，实现在接线器）。

    1. `apps/coding-agent/Cargo.toml`：`tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-util", "fs", "sync"] }`
       —— **必须显式加 `"sync"`**。现能编译 `mpsc` 纯属 `reqwest→hyper` 传递启用，不能依赖。
    2. `apps/coding-agent/src/channel.rs`：
       ```rust
       pub struct ChannelSink {
           tx: mpsc::Sender<Envelope>,
           overflow: VecDeque<AgentEvent>,   // 仅 try_send 撞满时使用（review R2：overflow 归 sink，不归 Forwarder）
           source: Source,
           turn: u32,
           policy: LifecyclePolicy,          // 生命周期策略只在这里（review R8）
           // 背压计数（review R6）
       }
       pub struct ChannelStats { pub backpressure_waits: u64, pub buffered: usize, pub consumer_gone: bool }
       ```
       `try_emit` 语义：先 `drain_overflow_best_effort()`（同步 `try_send` 冲积压）→ `tx.try_send(self.wrap(event))`：
       - `Ok` → `Ok(())`
       - `Err(Full(env))` → `overflow.push_back(env.event)`，返 `Ok(())`（**撞满不算失败**），`backpressure_waits += 1`
       - `Err(Closed(_))` → 按 `policy`：`StopWhenConsumerGone` → `Err(event)`；`ContinueWithoutConsumer` → `Ok(())`（丢弃），置 `consumer_gone = true`
       `emit<'a>` 语义：`Box::pin(async move { ... })`——**先**把 `overflow` 全部 `tx.send(..).await`（背压点在此），**再**发本次；任一 send 失败 → `Err(EventError::SendFailed)`。
       `begin_turn(&mut self, turn: u32) { self.turn = turn; }`——turn **由 `Agent::run` 显式设置，不从 `UserMessage` 推导**（review R4）。
       `stats(&self) -> ChannelStats`。
       `wrap(event) -> Envelope { source: self.source.clone(), turn: self.turn, event }`。
    3. 构造函数：`ChannelSink::new(capacity: usize, policy: LifecyclePolicy) -> (Self, mpsc::Receiver<Envelope>)`（容量可配，Q11「足够大 + 可配」）。
- test_method: `cargo build -p ys-coding-agent && cargo test -p ys-coding-agent channel::`
- priority: P0

## Task 10: 步 2.2 — ChannelSink 行为测试
- type: test
- files: `apps/coding-agent/src/channel.rs`（`#[cfg(test)] mod tests`）
- description: |
    测试合同第 9 条——信道行为，逐条断言。

    1. **投递 → 收到 `Envelope`**：`#[tokio::test]` 中 `sink.try_emit(ev)` → `rx.recv()` 拿到 `Envelope`，断言 `source == Source::agent()`、`turn` 与 `begin_turn` 设定一致、`event == ev`。
    2. **满 → 缓冲不丢**：用小容量（如 `capacity = 1`）构造，不消费 `rx`，连续 `try_emit` N=5 次，断言全部返 `Ok(())`、`stats().buffered == 4`、`stats().backpressure_waits >= 4`。
    3. **`emit().await` 先冲 overflow**：承上，调用 `sink.emit(new_ev).await`，断言 `rx` 收到的**顺序**是「先 4 条积压、后本次」，且 `stats().buffered == 0`。
    4. **接收端关闭 → 策略处置**：
       - `StopWhenConsumerGone`：`drop(rx)` 后 `try_emit(ev)` 返 `Err(ev)`（payload 原样交回）。
       - `ContinueWithoutConsumer`：`drop(rx)` 后 `try_emit(ev)` 返 `Ok(())`，`stats().consumer_gone == true`。
    5. **`begin_turn` 生效**：`begin_turn(3)` 后 emit，断言收到的 `Envelope.turn == 3`；且**不改事件本身**。
    6. **容量可配**：`capacity = 8` 时 `backpressure_waits == 0`（8 条以内不触发）。
- test_method: `cargo test -p ys-coding-agent channel::`
- priority: P0

## Task 11: 步 2.3 — main.rs 模式分发重构 + --json
- type: impl
- files: `apps/coding-agent/src/main.rs`、`apps/coding-agent/src/format.rs`（若需放置 json writer）
- description: |
    现逻辑全内联在 `#[tokio::main] main()`（`main.rs:22` 起），且模式判定（`:94` 的 arg 循环）晚于 agent 构建（`:130-139`），需重排。

    1. **抽 `parse_args()`**：
       ```rust
       pub struct Args { pub task: Option<String>, pub print_only: bool, pub json: bool }
       pub fn parse_args(argv: &[String]) -> Result<Args, String>;
       ```
       脱离 `std::env::args()`（可单测）。
    2. **重排主流程为「先定模式 → 选消费者 → 建 agent」**：
       - 先 `parse_args()` 定模式。
       - 按模式选消费者并构造 `ChannelSink` + `Inbox`：`--json` → JSON 逐行消费者；`-p` → 流式打印消费者；否则 → 交互式/TUI。
       - 最后 `AgentBuilder::new()`，把 `:139` 的 `.events(NoopEventSink)` 换成 `.events(channel_sink)`（**这是「事件全丢」的修复点**）。
    3. **抽 `write_json_envelope<W: Write>(w: &mut W, env: &Envelope) -> io::Result<()>`**：写一行 `serde_json::to_string(env)` + `\n`；不依赖 argv、不依赖 stdout（可注入 writer）。
    4. **消费与 turn 必须并发**：用 `tokio::join!(run_future, consume_future)`。
       ⚠ 串行会**死锁**（有界信道 + 消费者不跑 = 生产者阻塞在 `emit().await`）。
    5. **消费循环以「终局事件」为终止条件**（`AgentEvent::RunFinished` / `RunFailed`），**不以信道关闭**——信道关闭在 agent 结束后才发生，等待它会导致 join 时机错误/漏末事件。
    6. `--json` 模式：每条 `Envelope` 一行 JSON 到 stdout，末行为终局事件。
- test_method: `cargo build -p ys-coding-agent && cargo test -p ys-coding-agent parse_args`
- priority: P0

## Task 12: 步 2.3 — parse_args 单测 + --json 输出测试
- type: test
- files: `apps/coding-agent/src/main.rs`（`#[cfg(test)] mod tests`）、`apps/coding-agent/tests/json_output.rs`（新建）
- description: |
    测试合同第 10-11 条。

    1. **`parse_args` 单测**（合同第 10 条，在 `main.rs` 内联 `mod tests`）：
       - 无参：`Args { task: None, print_only: false, json: false }`。
       - `["-p", "hello"]`：`print_only == true`、`task == Some("hello")`。
       - `["--json", "hello"]`：`json == true`、`task == Some("hello")`。
       - `["--json", "-p", "hello"]`：两 flag 同真（断言确定的优先级行为）。
       - 多 token 任务：`["-p", "a", "b"]` → `task == "a b"`（保持现有 `args[i+1..].join(" ")` 语义，`main.rs:98`）。
    2. **`write_json_envelope` 纯函数测**（合同第 11 条前半）：传入固定的 `Envelope`，断言输出**恰为一行**（含 `\n`）、`serde_json::from_str::<Envelope>` 可往返、`turn` 与 `source` 字段出现在行内。
    3. **`--json` 行序列集成测**（合同第 11 条后半，新建 `apps/coding-agent/tests/json_output.rs`）：MockModel + `ChannelSink` + 可注入 writer（`Vec<u8>`），断言：
       - 输出行数 == 事件数；
       - **每行都是合法 JSON**（逐行 `from_str::<Envelope>`）；
       - **末行是终局事件**（`RunFinished` / `RunFailed`）；
       - `turn` 单调不减。
       ⚠ 该集成测**不触碰磁盘**，故不受 `set_override` 为 `#[cfg(test)] pub(crate)` 的限制（见 design-plan §5.4）。
- test_method: `cargo test -p ys-coding-agent parse_args && cargo test -p ys-coding-agent --test json_output`
- priority: P0

## Task 13: 步 2.4 — -p 流式打印
- type: impl
- files: `apps/coding-agent/src/main.rs`
- description: |
    现状 `-p` 只 `println!` 最终文本（`main.rs:165` 附近），无增量。

    1. 抽**可注入 writer** 的流式打印函数：
       ```rust
       pub fn print_stream_event<W: Write>(w: &mut W, env: &Envelope) -> io::Result<()>;
       ```
       对 `AgentEvent::ModelTextDelta { text }` → 写 `text`（**不换行**，增量拼接）；对终局事件 → 写 `\n`；其余事件忽略。
    2. 在 `-p` 模式的消费循环里调用该函数（复用 Task 11 的消费循环骨架）。
    3. 不变式（design.md §8 / ADR-0011）：**「增量拼接 == 终态」**——把全部 delta 依次拼接，必须等于 `RunResult` 的最终文本。
    4. 不在 `-p` 模式引入任何额外格式（ANSI、前缀、时间戳）。
- test_method: `cargo build -p ys-coding-agent && cargo test -p ys-coding-agent stream`
- priority: P1

## Task 14: 步 2.4 — -p 流式集成测试
- type: test
- files: `apps/coding-agent/tests/stream_output.rs`（新建）
- description: |
    测试合同第 12 条——可注入 writer 的集成测。

    1. 用 MockModel 产出多个 `ModelTextDelta`（如 `["he", "llo", " world"]`）+ 终局事件。
    2. 消费循环写入 `Vec<u8>` writer，断言：
       - 三段增量**按序到达**，且是**分次写入**而非一次性（可用一个记录调用次数的 writer 包装断言）；
       - 拼接结果 == `"hello world"`（**「增量拼接 == 终态」不变式**，这是设计文档点明的核心不变量）；
       - 终局后写入了换行。
    3. 断言 `-p` 模式**不输出** JSON（与 `--json` 互斥）。
- test_method: `cargo test -p ys-coding-agent --test stream_output`
- priority: P1

## Task 15: 步 3.1 — RuntimeContext.inbox + BasicLoop 轮边界 steering
- type: impl
- files: `crates/ys-component/src/context.rs`、`crates/ys-loop/src/basic.rs`
- description: |
    1. `crates/ys-component/src/context.rs`（现 `#[non_exhaustive]` 结构体在 `:10-11`，`pub fn new(` 在 `:26`，10 参数）：
       - 加字段 `pub inbox: Option<&'a Inbox>,`（默认 `None`）。
       - 加链式 `pub fn with_inbox(mut self, inbox: &'a Inbox) -> Self { self.inbox = Some(inbox); self }`。
       - ⚠ **不加参数到 `new()`**——`new()` 是 10 参数 + `#[non_exhaustive]`，加参数会波及 4 个构造点；链式可把影响面压到 **1**（`crates/ys-runtime/src/agent.rs:67`）。
       - `crates/ys-component/Cargo.toml` 加 `ys-channel = { path = "../ys-channel" }`（`ys-channel` 零 tokio，不破坏 `ys-component` 的「纯契约层」约束）。
    2. `crates/ys-loop/src/basic.rs` **轮边界**（`:116` compact 块结束 `}` → `:119` `rounds += 1` **之前**）插入 steering drain：
       ```rust
       // 轮边界：拉取 steering（中途插话），移入会话并告知模型
       if let Some(inbox) = ctx.inbox {
           for msg in inbox.take_steering() {
               ctx.session.append(msg.clone()).await?;
               emit(ctx.events, AgentEvent::UserMessage { message: msg }).await?;
           }
       }
       ```
    3. ⚠ **不得增 `rounds`、不得改 turn**——steering 注入是「同一轮里多一条 user 消息」，不是新一轮（配合 `begin_turn` 显式化，review R4）。
    4. `inbox = None` 时**行为与今日逐字节一致**（验收点；存量测试即证，不得有任何分支副作用）。
- test_method: `cargo build -p ys-component -p ys-loop && cargo test -p ys-loop steering`
- priority: P0

## Task 16: 步 3.1 — 轮边界注入测试 + 向后兼容
- type: test
- files: `crates/ys-loop/src/basic.rs`（`#[cfg(test)] mod tests`）；共享 helper 维持现状，不新建 `ys-test-support` crate
- description: |
    测试合同第 13-14 条。

    1. **轮边界注入**（合同第 13 条）：
       - 构造 `RuntimeContext` + `with_inbox(&inbox)`；用 MockModel 捕获每轮的 `ModelRequest.messages`。
       - 在第一次 `model.complete()` 时（用 MockModel 的闭包/回调能力）`inbox.push(msg, Intent::Steering)`。
       - 断言：**下一轮**的 `ModelRequest.messages` 里出现了这条新 user 消息。
       - 断言：**`rounds` 未因注入而增加**（若一次注入就让 `rounds` +2，则测试失败）；注入后 emit 的 `UserMessage` 事件被 `CollectingSink` 收到，但 `Envelope.turn` 不变（用 `ChannelSink::begin_turn` 的值核）。
    2. **向后兼容**（合同第 14 条）：`cargo test --workspace` 存量测试全绿即证「`inbox = None` 时行为与今日一致」。
       额外补一条显式断言：`inbox = None` 时 `BasicLoop::run_turn` 的**事件序列**与 `CollectingSink` 基线逐条相等（可对同一输入跑两遍，一遍带空 inbox、一遍不带，断言事件序列相同）。
- test_method: `cargo test -p ys-loop steering && cargo test --workspace`
- priority: P0

## Task 17: 步 4.1 — Agent::run(inbox) 自转驱动
- type: impl
- files: `crates/ys-runtime/src/agent.rs`、`crates/ys-runtime/src/lib.rs`（导出 `RunSummary`）、`crates/ys-runtime/Cargo.toml`（加 `ys-channel`）
- description: |
    ADR-0010 留空的那块。

    1. `crates/ys-runtime/src/agent.rs`：
       ```rust
       pub struct RunSummary { pub turns: u32, pub usage: Usage, pub last_stop: Option<StopReason> }

       impl Agent {
           /// 自转：从 inbox 取消息 → 跑回合 → 投事件，直到 inbox 空闲。
           pub async fn run(&mut self, inbox: &Inbox) -> Result<RunSummary, LoopError>;
           /// 内部：跑一个回合（原 `run_turn`，语义保持，测试仍可直接调用）。
           async fn run_one_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError>;
       }
       ```
       - 现 `pub async fn run_turn` 在 `:61`、`RuntimeContext::new` 在 `:67`、`self.loop_impl.run_turn` 在 `:79` → 整体改名为 `run_one_turn`；`run_one_turn` 内部在 `:67` 的 `RuntimeContext::new(...)` 上链 `.with_inbox(inbox)`（需把 `inbox` 透传进 `run_one_turn`，或让 `run_one_turn` 通过 `self` 持有的 `Option<&Inbox>` 取——**取前者：`run_one_turn(&mut self, input, inbox: Option<&Inbox>)`**，避免在 `Agent` 上存跨 await 的引用）。
    2. `run()` 逻辑：
       - 循环：`take_followup()`（**回合边界**）取到消息 → 每条 `run_one_turn`。
       - `begin_turn(n)` 由 `run()` 在**每回合开始**调用（`n` 从 1 递增），**不从事件推导**（review R4）。
       - **inbox 空 → 返回**（agent 不休眠，ADR-0011 点 6）；返回 `RunSummary { turns, usage, last_stop }`。
       - **不收 policy**（review R8）——遇 `emit` 报错即终止，沿用既有语义。
    3. `crates/ys-runtime/Cargo.toml` 加 `ys-channel = { path = "../ys-channel" }`；`lib.rs` 导出 `RunSummary`。
    4. `/new` 用新 `Inbox`——**本轮不做命令层**（留步 5），仅在 `run()` 文档注释中标注该契约。
- test_method: `cargo build -p ys-runtime && cargo test -p ys-runtime`
- priority: P0

## Task 18: 步 4.1 — 自转语义测试（含跨 crate 集成）
- type: test
- files: `crates/ys-runtime/src/agent.rs`（`#[cfg(test)] mod tests`）、`crates/ys-runtime/tests/actor_run.rs`（新建）
- description: |
    测试合同第 15-16 条。

    1. **自转语义单测**（合同第 15 条，`agent.rs` 内联）：
       - `followUp 排队 → 自动下一趟`：`inbox.push(m1, FollowUp)` → `run()` 后 `inbox.push(m2, FollowUp)` 再 `run()`；断言 `RunSummary.turns` 累加、MockModel 收到两次 request。
       - `inbox 空 → 立即返回`：空 `Inbox` 调 `run()`，断言 `turns == 0`、**零次** `model.complete` 调用。
       - `run_one_turn 语义保持`：把现有 `run_turn` 相关断言原样迁移到 `run_one_turn`（改名不改行为）。
       - `QueueMode::All`：push 3 条 followUp → 断言**一个回合**处理完（`turns == 1`），模型只被调一次。
       - `QueueMode::OneAtATime`：push 3 条 → 断言 `turns == 3`，模型被调 3 次。
    2. **跨 crate 集成测**（合同第 16 条，新建 `crates/ys-runtime/tests/actor_run.rs`，照 `v0_integration.rs` 模板）：
       - `Agent::run` + `ys-coding-agent` 的 `ChannelSink` 等价物（在 `ys-runtime/tests/` 里可自建一个内联 sink，**不依赖 `apps/coding-agent`**——后者不可作为 dev-dep 反向依赖）。
       - 断言：**消费者消失 → 收摊**——`ChannelSink` 用 `StopWhenConsumerGone`、`drop(rx)` 后 `run()` 返回 `Err(LoopError::Event(SendFailed))`。
       - 断言：steering 经 `Inbox` 在 `run()` 期间注入成功（跨 crate 端到端）。
       - 磁盘隔离：本测**不需要磁盘**（design-plan §5.4），不得引用 `StateStore` / `ProviderRegistry`（它们是 `apps/coding-agent` 的私有类型，集成测也拿不到）。
    3. **MANUAL_ACK_REQUIRED（不在自动验收内）**：
       - 真实 API 端到端：`cargo test -p ys-coding-agent e2e -- --ignored`（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）。
       - TUI 终端观感：渲染层有 `TestBackend` 可测，但**实际终端观感**需人工确认（本轮 TUI 增量渲染明确不做，见 design-plan §6）。
- test_method: `cargo test -p ys-runtime && cargo test -p ys-runtime --test actor_run && cargo test --workspace`
- priority: P0

## 手工验收（MANUAL_ACK_REQUIRED，不在自动验收内）

1. **真实 API 端到端**：`cargo test -p ys-coding-agent e2e -- --ignored`（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）。`e2e_tools.rs` 保持 `#[ignore]`。
2. **TUI 终端观感**：`cargo run -p ys-coding-agent` 交互式跑一轮对话，人工确认渲染、滚动、Ctrl-C 中断观感。渲染有 `TestBackend` 可测，但实际终端观感无法自动断言——本轮 TUI 增量渲染明确不做（design-plan §6）。

## 全量验证（收尾）

1. `cargo build --workspace`
2. `cargo test --workspace`（期望：存量 178 + 新增 ≈ 210+ 全绿，1 ignored）
3. `cargo clippy --all-targets`（无 warning）
4. `cargo fmt --check`
5. 手工：`cargo run -p ys-coding-agent -- --json "hello"` → 逐行 JSON，末行为终局事件
6. 手工：`cargo run -p ys-coding-agent -- -p "hello"` → 文本增量可见
