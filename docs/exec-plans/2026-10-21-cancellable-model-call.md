# 可取消的模型调用 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据**。

## 上下文引用

参考设计计划：`docs/design-plans/2026-10-21-cancellable-model-call.md`
以及 `docs/adr/0046-cancellable-model-call-and-abort-feedback.md`（决策来源）

核心事实（已核实，实施时可直接采信）：

- `crates/ys-model/src/trait_def.rs:11-15`：`Model::complete(&self, request, sink) -> Result<ModelResponse, ModelError>` —— **trait 签名不变**。
- `crates/ys-model/src/error.rs:3-11`：`ModelError` 现有 `Sink` / `Provider(String)` / `Serde` 三变体，全仓 12 处引用、零穷尽匹配。
- `crates/ys-model/src/request.rs:6-16`：`ModelRequest` 派生 `Debug, Clone, Default, Serialize, Deserialize`。
- `crates/ys-model/src/request.rs:49-55`：`ModelResponse` **无** `Default` 派生 ⇒ 全仓 35 处构造点 ⇒ **本方案不给它加字段**。
- `crates/ys-protocol/src/boundary.rs:26-36`：`trait BoundarySource: Send + Sync { fn take(&self) -> Option<Boundary>; fn is_aborted(&self) -> bool; }` —— **无 `Debug` supertrait**（⇒ 决策 B′ 要加）。
- `crates/ys-component/src/context.rs:11-28`：`RuntimeContext`（不是 ADR 里的 `RunContext`），`pub boundary: Option<&'a dyn BoundarySource>`（`:24`），`#[non_exhaustive]`，不 Clone。
- `crates/ys-component/src/context.rs:59-62`：`with_boundary(mut self, boundary: &'a dyn BoundarySource) -> Self`。
- `crates/ys-runtime/src/agent.rs:30` / `:38-43`：`AgentPorts.boundary` 与 `AgentPorts::new(model, session, events, boundary)`。
- `apps/coding-agent/src/app_loop.rs:998` / `:1004`：边界源创建与注入点（TUI 交互模式）。
- `apps/coding-agent/src/main.rs:1716` / `:1723`：`ports_with_boundary` + `AgentPorts::new`（`-p` / `--json` 一次性模式的第二个接线点）。
- `crates/ys-loop/src/basic.rs:118-121` `run_turn(ctx: &mut RuntimeContext<'_>)`；`:137`、`:174` abort 探针（模型调用之前）；`:296-337` 轮边界 abort 分支；`:339-358` 组装 `ModelRequest`；`:360-377` Step 6 调 model（任何 Err ⇒ `RunFailed`）；`:379-401` append assistant + `RoundUsage`；`:403-409` 取 `text_content` 作 `final_message`；`:670`、`:724` 工具执行传 boundary。
- `adapters/tools-basic/src/bash.rs:14`（`ABORT_POLL_INTERVAL = 100ms`）、`:101-107`（`wait_abort` 形态）；`adapters/tools-basic/src/exec.rs:29-30` 同值。
- `adapters/model-openai-compatible/src/lib.rs:396-400`（`Model::complete`）、`:411-473`（重试循环）、`:312-324`（`AttemptError` 两支）、`:516-521`（`attempt`）、`:524-531`（`send`）、`:598-599`（`StreamAccumulator::new`，**全仓唯一生产调用点**）、`:601-609`（`parse_sse_stream`）、`:692-705`（`classify_stream_error`）、`:461-468`（重试抑制先例 `meta.stream_started`）、`:885-898`（`StreamAccumulator` 字段）、`:990-993`（reasoning 只 emit）、`:1007-1012`（`finish` / `finish_with_logger`）、`:1024-1033`（content_blocks 组装顺序）。
- `adapters/model-openai-compatible/src/stream.rs:110-149`：`parse_sse_stream` 签名与内部 `tokio::time::timeout(idle, stream.next())`。
- `crates/ys-tui-coding/src/events.rs:169-177`：Esc 处理分支。
- `crates/ys-tool/Cargo.toml` 已依赖 `ys-protocol` ⇒ `ys-model → ys-protocol` **不成环**；`ys-protocol` 零 tokio。

### 签名变更的真实爆炸半径（已 `grep` 全仓核实）

⚠️ **ADR 0046 §决策 4 声称「本仓内的调用点只有 `app_loop.rs:1004` 与 `main.rs:1718`」是事实错误** ⇒ 实为 **25+ 处**，且**大量在测试文件里**。**本 plan 以核实结果为准**（ADR 由 ADR 负责人另行更正，本 plan 不改）。

| 符号 | 调用点总数 | 分布 |
|---|---|---|
| `AgentPorts::new` | **17 处** | `apps/coding-agent/src/app_loop.rs:1004`、`apps/coding-agent/src/main.rs:1723`、`apps/coding-agent/src/wiring.rs:258`、`apps/coding-agent/src/subagent.rs:427`、`apps/coding-agent/tests/e2e_skills.rs:239`、`apps/coding-agent/tests/e2e_tools.rs:75`、`crates/ys-runtime/src/agent.rs:246`、`crates/ys-runtime/src/lib.rs:49/75/104/129/155/162`、`crates/ys-runtime/tests/run_turn_boundary.rs:147/194/219/233/267/313`、`crates/ys-runtime/tests/v0_integration.rs:92/118/145/171/308/384` |
| `RuntimeContext::with_boundary` | **8 处** | `crates/ys-loop/src/lib.rs:257/288/408/570/847/928/975`、`crates/ys-loop/src/basic.rs:2405` |
| `Wiring::ports_with_boundary` | **3 处** | `apps/coding-agent/src/main.rs:1716`、`apps/coding-agent/src/wiring.rs:243`（自调用）、`:254`（定义） |
| `impl BoundarySource` | **5 处** | `crates/ys-protocol/src/boundary.rs:99`、`crates/ys-loop/src/lib.rs:758`、`apps/coding-agent/src/subagent.rs:83`、`adapters/tools-web/tests/tools.rs:1872`、`adapters/tools-web/tests/tools.rs:1908` |

### 签名变更的采纳形态（设计计划决策 B）

- `RuntimeContext.boundary`：`Option<&'a dyn BoundarySource>` → `Option<Arc<dyn BoundarySource>>`；`with_boundary` 接收 `Arc<dyn BoundarySource>`。
- `AgentPorts.boundary`：同上；`AgentPorts::new` 第 4 参数同上。
- `Wiring::ports_with_boundary` 第 2 参数同上。
- **`ys-tool` 层零改动**：`ToolContext.boundary` 仍是 `Option<&'a dyn BoundarySource>`、`SubagentRunner::run(..., boundary: Option<&dyn BoundarySource>)` 签名不变 ⇒ 适配点只有 `ctx.boundary.as_deref()`。
- `ChildBoundary::new(parent)` 保持 `Option<&'a dyn BoundarySource>` 不变 ⇒ 调用点传 `ctx.boundary.as_deref()`。
- ⚠️ `Arc<dyn BoundarySource>` 默认即 `+'static` ⇒ app 层必须持有 `Arc`（不能是栈上临时借用）。

## 基线

- **基线 ref**: `2a75f35`（含 Phase 3 执行前的 plan 修正；原初始基线 `ed5494d5`）
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据

## 任务清单

### Task 1: crates/ys-model/ — 取消错误变体 + 请求字段 [type=impl]

- **涉及目录**: `crates/ys-model/`
- **涉及文件**: `crates/ys-model/Cargo.toml`, `crates/ys-model/src/error.rs`, `crates/ys-model/src/request.rs`
- **描述**:
  - `Cargo.toml`：加 `ys-protocol = { path = "../ys-protocol" }`（不成环：`ys-tool` 已依赖 `ys-protocol`，`ys-model` 已依赖 `ys-tool`）。
  - `error.rs`：加 `ModelError::Cancelled { partial: Option<ys_core::Message> }`（`thiserror` 的 `#[error("model call cancelled")]`；`ys_core::Message` 派生 `Debug` ✓；**不加** `#[non_exhaustive]`）。
  - `request.rs`：`ModelRequest` 加 `#[serde(skip, default)] pub boundary: Option<Arc<dyn BoundarySource>>`；`Debug` / `Default` / `Clone` / `Serialize` / `Deserialize` 五个派生**全部保留**（`Debug` 派生的保留依赖 T3 的 `Debug` supertrait ⇒ **T1 单独编译不过，必须与 T3 一起**）。
  - ⚠️ **`ModelRequest` 有 3 处「穷尽构造点」（无 `..Default::default()`）必须补 `boundary: None`** —— 加字段后这 3 处编译失败：
    | 位置 | 归属 |
    |---|---|
    | `crates/ys-loop/src/summarize.rs:61` | **本任务不属于此文件** ⇒ 该文件由 **T9** 适配（见 T9 描述） |
    | `adapters/model-openai-compatible/src/lib.rs:1211` | 属 **T12** |
    | `adapters/model-openai-compatible/src/lib.rs:1244` | 属 **T12** |
    ⇒ **T1 自身不得越界改这三个文件**（文件集冲突）；但 T1 必须在自己收尾时**报告**这三处待适配。
- **关键约束**:
  - `Arc<dyn BoundarySource>` 不是 `Serialize` ⇒ 必须 `#[serde(skip, default)]` ⇒ 既有序列化字节逐字节不变。
  - `Option<Arc<..>>` 的 `Default` 是 `None` ⇒ 既有 `..Default::default()` 构造点**不需要改**（全仓 13 处构造点里 10 处带 `..Default::default()`，只有上列 3 处是穷尽的）。
  - `ModelResponse` **不得**加字段（会改 35 处构造点）。
  - **不得**用「去掉 `Debug` 派生」绕过问题 ⇒ 那是对外 API 缩减（设计计划决策 B′）。
  - `ys-model` 加 `ys-protocol` 依赖后，`ys-model → ys-protocol` 不成环（`ys-tool` 已依赖 `ys-protocol`）。
- **关联测试任务**: T2
- **依赖**: 无

### Task 2: crates/ys-model/ — 测试 [type=test]

- **涉及目录**: `crates/ys-model/`
- **涉及文件**: `crates/ys-model/src/request.rs`（内联 `#[cfg(test)]` 模块）, `crates/ys-model/src/error.rs`（内联测试模块）
- **描述**:
  - **N10**：`ModelRequest` 序列化不含 `boundary` —— 含 `boundary: Some(..)` 的 request 序列化结果与不含时**逐字节相同**。
  - **N11**：`ModelRequest::clone` 保留同一 `Arc` —— `Arc::ptr_eq` 为真。
  - `ModelError::Cancelled { partial: None }` / `{ partial: Some(msg) }` 可构造、`matches!` 可匹配；`Display` 为 `"model call cancelled"`。
- **验证方法**:
  - `cargo test -p ys-model` — 期望全绿
- **关联 impl**: T1
- **依赖**: T1

### Task 3: crates/ys-protocol/ — `BoundarySource` 加 `Debug` supertrait [type=impl]

- **涉及目录**: `crates/ys-protocol/`
- **涉及文件**: `crates/ys-protocol/src/boundary.rs`
- **描述**:
  - `trait BoundarySource: Send + Sync` ⇒ 改为 `Send + Sync + std::fmt::Debug`（`:26`）。
  - `QueueBoundarySource`（`:51`）**手写** `impl Debug`（**不派生**）：只输出 `aborted`（bool）与 `queued`（队列长度）两项，**不输出队列内容**。
  - `Inner`（`:38`）**不**派生 `Debug`（其 `queue: VecDeque<Boundary>` 派生会把 steer 消息正文带进日志）⇒ 手写 `Debug` 时直接读 `self.lock()` 的 `aborted` 与 `queue.len()`。
  - 形态约定：
    ```rust
    impl std::fmt::Debug for QueueBoundarySource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let inner = self.lock();
            f.debug_struct("QueueBoundarySource")
                .field("aborted", &inner.aborted)
                .field("queued", &inner.queue.len())
                .finish()
        }
    }
    ```
- **关键约束**:
  - **必须手写、不得派生** —— 派生会把 `Boundary::Steer(Message)` 的正文写进日志 ⇒ 泄露用户内容。
  - 这是 `ModelRequest` 保留 `Debug` 派生的**前提**（设计计划决策 B′）⇒ T1 单独编译不过，**T1 与 T3 必须同时合入**才能编译。
  - `ys-protocol` 保持**零 tokio**、不新增依赖。
- **关联测试任务**: T4
- **依赖**: 无

### Task 4: crates/ys-protocol/ — 测试 [type=test]

- **涉及目录**: `crates/ys-protocol/`
- **涉及文件**: `crates/ys-protocol/src/boundary.rs`（内联 `#[cfg(test)]` 模块）
- **描述**:
  - **N13**：`QueueBoundarySource` 的 `Debug` 输出**不含** steer 消息正文 —— 推一条含**可识别文本**的 `Boundary::Steer(Message)`，断言 `format!("{:?}", source)` 不含该文本；断言输出含 `aborted` 与 `queued`。
  - 既有 `ys-protocol` 测试全绿（`QueueBoundarySource` 现在需 `Debug`，但既有测试不依赖其 `Debug` 输出）。
- **验证方法**:
  - `cargo test -p ys-protocol` — 期望全绿，且 N13 的断言生效
- **关联 impl**: T3
- **依赖**: T3

### Task 5: crates/ys-component/ — `RuntimeContext.boundary` Arc 化 [type=impl]

- **涉及目录**: `crates/ys-component/`
- **涉及文件**: `crates/ys-component/src/context.rs`
- **描述**:
  - `RuntimeContext.boundary`（`:24`）：`Option<&'a dyn BoundarySource>` ⇒ `Option<Arc<dyn BoundarySource>>`。
  - `with_boundary`（`:59-62`）签名 ⇒ 接收 `Arc<dyn BoundarySource>`：
    ```rust
    pub fn with_boundary(mut self, boundary: Arc<dyn BoundarySource>) -> Self {
        self.boundary = Some(boundary);
        self
    }
    ```
  - `RuntimeContext` 其余字段不动；仍是 `#[non_exhaustive]`、仍不 `Clone`。
  - 字段文档需说明 `Arc` 化的两个理由（`ModelRequest.boundary` 需 `'static`；同一份边界源同时给 loop / 工具层 / 模型层三处）。
- **关键约束**:
  - `Arc<dyn BoundarySource>` 需 `'static` ⇒ 不能借用栈上临时值（接线由 T14 负责）。
  - `'a` 仍约束 `model` / `session` / `events` / `subagent`；`boundary` 字段**不再受 `'a` 约束**（是**放宽**，不是收紧）。
  - `ys-component` 已依赖 `ys-protocol` ⇒ 不新增依赖。
- **关联测试任务**: T6
- **依赖**: T1, T3

### Task 6: crates/ys-component/ — 测试 [type=test]

- **涉及目录**: `crates/ys-component/`
- **涉及文件**: `crates/ys-component/src/context.rs`（内联测试模块）
- **描述**:
  - `with_boundary` 接受 `Arc<dyn BoundarySource>` 后，boundary 可见且 `is_aborted()` 语义透传。
  - 既有 `RuntimeContext` 构造点（`..Default` 或具名构造）全部仍能编译。
- **验证方法**:
  - `cargo test -p ys-component` — 期望全绿
- **关联 impl**: T5
- **依赖**: T5

### Task 7: crates/ys-runtime/ — `AgentPorts` 边界参数 Arc 化 + 6 处调用点适配 [type=impl]

- **涉及目录**: `crates/ys-runtime/`
- **涉及文件**: `crates/ys-runtime/src/agent.rs`, `crates/ys-runtime/src/lib.rs`
- **描述**:
  - `agent.rs`：`AgentPorts.boundary`（`:30`）改 `Option<Arc<dyn BoundarySource>>`；`AgentPorts::new`（`:38-43`）第 4 参数签名相应改。
  - `lib.rs`：**6 处** `AgentPorts::new` 调用点适配（`:49`、`:75`、`:104`、`:129`、`:155`、`:162`）—— 原本传 `None` 的保持 `None`；原本传 `Some(&boundary)` 的改为 `Some(boundary.clone())`（`boundary` 须已是 `Arc`）或 `Some(Arc::new(...))`。
  - `agent.rs:246`（内联测试区）的 `AgentPorts::new` 调用点同样适配。
  - `AgentPorts` 的 `'a` 仍约束 `model` / `session` / `events`；`boundary` 字段不再受 `'a` 约束。
- **关键约束**:
  - `AgentPorts::new` 是 `ys-runtime`（对外 facade）的**公开 API** ⇒ 此次改型是**对外 breaking 变更**，须在实现中保留文档说明。
  - 传入的 `Arc` 必须满足 `'static` ⇒ 不能 `Arc::new(&borrowed)`。
- **关联测试任务**: T8
- **依赖**: T1, T3, T5

### Task 8: crates/ys-runtime/ — 测试 + 集成测试调用点适配 [type=test]

- **涉及目录**: `crates/ys-runtime/`
- **涉及文件**: `crates/ys-runtime/src/lib.rs`（内联测试模块）, `crates/ys-runtime/src/agent.rs`（内联测试模块）, `crates/ys-runtime/tests/run_turn_boundary.rs`, `crates/ys-runtime/tests/v0_integration.rs`
- **描述**:
  - **测试文件也是调用点**（不得漏）：
    - `tests/run_turn_boundary.rs` — **6 处** `AgentPorts::new` 调用点适配（`:147`、`:194`、`:219`、`:233`、`:267`、`:313`）；
    - `tests/v0_integration.rs` — **6 处** `AgentPorts::new` 调用点适配（`:92`、`:118`、`:145`、`:171`、`:308`、`:384`）。
  - `src/lib.rs` / `src/agent.rs` 的内联测试模块：既有断言不变，另加一条「`AgentPorts::new` 接受 `Option<Arc<dyn BoundarySource>>` 可正常构造」。
- **验证方法**:
  - `cargo test -p ys-runtime` — 期望全绿
  - `cargo test -p ys-runtime --test run_turn_boundary --test v0_integration` — 期望全绿（确认集成测试目标已编译）
- **关联 impl**: T7
- **依赖**: T7

### Task 9: crates/ys-loop/ — 取消分支 + 边界源适配 + 工具间探针 [type=impl]

- **涉及目录**: `crates/ys-loop/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`, `crates/ys-loop/src/lib.rs`（`compact.rs` 已核实无需适配）, `crates/ys-loop/src/summarize.rs`
- **描述**:
  - **边界源适配**（因 T5 改了类型）：
    - `basic.rs:137`、`:174` 的 `is_some_and(|b| b.is_aborted())` 保持可编译（`Option<Arc<dyn BoundarySource>>` 上 `is_some_and` 的 `b: &Arc<dyn BoundarySource>` 自动 deref 到 `&dyn BoundarySource`）；
    - `basic.rs:305` 的 `if let Some(boundary) = ctx.boundary` ⇒ 改 `ctx.boundary.as_ref()`；
    - `basic.rs:670`、`:724` 传给 `ToolContext::new(...)` 的 boundary ⇒ 改 `ctx.boundary.as_deref()`（`ToolContext.boundary` 是 `Option<&dyn BoundarySource>`）⇒ **`ys-tool` 层零改动**。
  - **`with_boundary` 调用点适配（8 处）**：`lib.rs:257`、`:288`、`:408`、`:570`、`:847`、`:928`、`:975` 与 `basic.rs:2405` ⇒ 参数从 `&dyn` / `Option<&dyn>` 改为 `Arc<dyn>` / `Option<Arc<dyn>>`（必要时 `Arc::new(...)`）。
  - **`ModelRequest` 组装处**（`basic.rs:339-358`）挂上 `boundary: ctx.boundary.clone()`。
  - ⚠️ **`summarize.rs:61` 的 `ModelRequest` 是「穷尽构造点」**（列全了 `messages` / `tools` / `system` / `max_tokens` / `temperature`，**没有** `..Default::default()`）⇒ T1 给 `ModelRequest` 加 `boundary` 字段后，此处编译失败 ⇒ **必须补 `boundary: None`**（摘要调用不挂边界源）。
    - 该构造点在 `ModelSummarizer::summarize` 内 ⇒ 摘要路径**不参与取消**（与设计一致：取消只作用于主模型调用）。
  - **Step 6 取消分支**（`basic.rs:360-377` 的 match 加一支）：
    1. `Err(ModelError::Cancelled { partial })` ⇒ 若 `partial` 有内容 ⇒ **先** `ctx.session.append(partial)`（与 `:398-401` 同一路径）；
    2. **不发** `RunFailed`；**不发** `RoundUsage`；
    3. 发 `RunFinished { stop_reason: StopReason::Cancelled, usage: total_usage.clone(), rounds }`（形态与 `:317-333` 的轮边界 abort 一致）；
    4. `return Ok(RunResult { stop_reason: Cancelled, usage: total_usage, rounds, final_message: <partial 的文本或 None> })`。
  - **工具之间 abort 探针**（决策 H）：在 `basic.rs:670` 附近、`:724` 附近的工具执行处，在**每个工具调用前后**检查 `ctx.boundary.is_some_and(|b| b.is_aborted())` ⇒ 已中止则不再发起下一个工具。**不加轮询**。
  - `lib.rs`：`DecoupledBoundarySource`（`:758` 的 `impl BoundarySource`）加 `Debug`（测试替身）⇒ **只输出 aborted 状态与队列长度，不输出队列内容**。
  - `compact.rs`：**已 grep 核实无 `ctx.boundary` 使用点**（`boundary` 仅出现在测试名与注释里）⇒ 无需适配；若 Phase 3 发现遗漏，按同法适配。
- **关键约束**:
  - **先 append 再收场，顺序不可颠倒**（否则消费端可能已认为回合结束 ⇒ 部分输出丢失）。
  - `final_message` = partial 的文本（若有）；轮边界 abort 的 `final_message: None` **保持不变**。
  - `RunUsage`/usage 统计不产生假的 0-token 轮次。
  - ⚠️ **生命周期陷阱**：`ctx.boundary.as_deref()` 得到的 `&dyn BoundarySource` 借用自 `ctx`，**不是** `'static`；在 `run_turn(ctx: &mut RuntimeContext<'_>)` 里传给要求 `&'a dyn` 的 `ToolContext` **可能生命周期不足**。稳妥做法：先取
    ```rust
    let boundary_ref: Option<&dyn BoundarySource> = ctx.boundary.as_deref();
    ```
    再传给 `ToolContext::new(boundary_ref, ...)`，让编译器给出**精确**报错后调整。**不得想当然** ⇒ 本任务**必须先跑 `cargo build -p ys-loop` 验证这一处**（先编译，再跑测试）。
- **关联测试任务**: T10
- **依赖**: T1, T3, T5, T7

### Task 10: crates/ys-loop/ — 测试 [type=test]

- **涉及目录**: `crates/ys-loop/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`（内联 `#[cfg(test)]` 模块）, `crates/ys-loop/src/lib.rs`（内联测试模块）
- **描述**:
  - **N4**：mock model 返回 `Err(ModelError::Cancelled { partial: None })` ⇒ 断言事件流含 `RunFinished{stop_reason: Cancelled}`，且**不存在** `RunFailed`。
  - **N5**：mock model 返回 `Err(ModelError::Cancelled { partial: Some(msg) })` ⇒ 断言会话历史含该段文本，且 append 发生在 `RunFinished` **之前**。
  - **N6**：取消后 mock model 调用计数 == 1。
  - **N8**：模型计划调用 N 个工具，第一个之后 abort ⇒ 工具调用计数 == 1（< 计划数）。
  - `lib.rs` 内联测试：`DecoupledBoundarySource` 的 `Debug` 输出不含队列内容；既有 `lib.rs` 测试（`with_boundary` 相关）全部仍绿。
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全绿
- **关联 impl**: T9
- **依赖**: T9

### Task 11: adapters/model-openai-compatible/ — 适配器取消实现 [type=impl]

- **涉及目录**: `adapters/model-openai-compatible/`
- **涉及文件**: `adapters/model-openai-compatible/Cargo.toml`, `adapters/model-openai-compatible/src/lib.rs`
- **描述**:
  - ⚠️ **`Cargo.toml` 必须加 `ys-protocol = { path = "../../crates/ys-protocol" }`** —— 适配器当前**不依赖** `ys-protocol`，而 `wait_abort` 的参数类型是 `Option<&dyn BoundarySource>` ⇒ 不加依赖编译失败。
    - 不成环：`ys-protocol` 只依赖 `ys-core` / `ys-event` / `serde`，不依赖任何 adapter。
  - 新增 `wait_abort(boundary: Option<&dyn BoundarySource>)`：照抄 `adapters/tools-basic/src/bash.rs:101-107` 的形态（100ms 轮询 `is_aborted()`；无 boundary 时 `std::future::pending::<()>().await`）。轮询间隔常量参照 `bash.rs:14` / `exec.rs:29-30` 的 100ms。
  - `AttemptError`（`:312-324`）加第三支 `Cancelled { partial: Option<ys_core::Message> }`。
  - `complete`（`:396-400`）的 `match outcome`：`Err(AttemptError::Cancelled { partial })` ⇒ **立即** `return Err(ModelError::Cancelled { partial })`（不走重试、不 trace 为失败）。
  - **三处非嵌套 `select!`**：
    1. 发送阶段：`attempt` 内 `:524` `self.send(...)` 之前 ⇒ 取消 ⇒ `partial: None`；
    2. 流阶段：`attempt` 内 `:601` `parse_sse_stream` 之前 ⇒ 取消 ⇒ `accumulator.into_partial()` **可取到**；
    3. 退避阶段：`complete` 重试循环内的退避 `sleep` 处 ⇒ 取消 ⇒ `partial: None`。
  - `StreamAccumulator`（`:885-898`）新增 `into_partial(self) -> Option<Message>`：**只取 `self.text`**（非空则产出 `ContentBlock::Text`），**不碰 `self.tool_calls`**；reasoning 不入 partial（与 `:990-993`「Thinking 段只 emit」一致）。
  - 抽取 `fn text_block(text: String) -> Option<ContentBlock>`，与 `finish_with_logger`（`:1024-1033`，先 `Text` 后 `ToolUse`）共用 ⇒ 既有 `finish` 路径字节输出**完全不变**。
  - 边界源从 `request.boundary`（`Option<Arc<dyn BoundarySource>>`）取 ⇒ 传 `request.boundary.as_deref()` 给 `wait_abort`。
- **关键约束**:
  - **`accumulator` 必须声明在 `select!` 之外**（栈上存活）；`select!` 只 drop 借用它的闭包 future。
  - **不得嵌套 `select!`**（外层会 drop 内层 future ⇒ partial 丢失）。
  - 阶段 2 取消时 partial 必须可取（`into_partial`）。
  - 取消**不得**归入 `Retryable`（否则重试循环重放请求，违背用户意图）。
  - `Arc<dyn BoundarySource>` 是 `'static` ⇒ 借出 `&dyn` 用 `as_deref()`，其生命周期短于 `Arc` 持有期。
- **关联测试任务**: T12
- **依赖**: T1, T3

### Task 12: adapters/model-openai-compatible/ — 测试 [type=test]

- **涉及目录**: `adapters/model-openai-compatible/`
- **涉及文件**: `adapters/model-openai-compatible/src/lib.rs`（内联 `#[cfg(test)]` 模块；现有测试自 `:1350` 起）, `adapters/model-openai-compatible/tests/retry.rs`
- **描述**:
  - **N1**：已 abort 的 boundary ⇒ `Model::complete` 返回 `ModelError::Cancelled`。
  - **N2**：先吐 delta 再取消 ⇒ `partial` 含已吐文本。
  - **N3**：先吐 tool call 的部分参数再取消 ⇒ `partial` 的 content blocks **全部**是 `Text`，无 `ToolUse`。
  - ⚠️ **两处「穷尽构造点」必须补 `boundary: None`**（T1 加了 `ModelRequest.boundary` 字段后编译失败）：
    - `src/lib.rs:1211`（`translate_request` 的 system+user 用例）
    - `src/lib.rs:1244`（`translate_request` 的 tool 用例）
  - `tests/retry.rs`：确认是否需适配（`ModelRequest::default()` 构造点不受影响；若需适配则改）。
  - 既有测试全部仍绿（`finish` 路径 content blocks 顺序不变、序列化不变）。
- **验证方法**:
  - `cargo test -p ys-model-openai-compat` — 期望全绿
    （⚠️ **包名是 `ys-model-openai-compat`**，不是目录名 `model-openai-compatible`；同理 `tools-web` 的包名是 `ys-tools-web`）
- **关联 impl**: T11
- **依赖**: T11

### Task 13: adapters/tools-web/ — 测试替身加 `Debug` [type=test]

- **涉及目录**: `adapters/tools-web/`
- **涉及文件**: `adapters/tools-web/tests/tools.rs`
- **描述**:
  - `StoredBoundary`（`:1872` 的 `impl BoundarySource`）与 `ArmBoundary`（`:1908` 的 `impl BoundarySource`）加 `Debug`（因 T3 给 trait 加了 `Debug` supertrait ⇒ 编译失败）。
  - **只改测试文件** ⇒ 归 `type=test`。
  - `Debug` 输出**不得**包含父队列 / 抓取内容。
  - `adapters/tools-web/` 的**业务代码零改动**。
- **验证方法**:
  - `cargo test -p ys-tools-web` — 期望全绿
  - `cargo build -p ys-tools-web --tests` — 期望无错误
- **关联 impl**: T3
- **依赖**: T3

### Task 14: apps/coding-agent/ — 边界源 Arc 化接线 [type=impl]

- **涉及目录**: `apps/coding-agent/`
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`, `apps/coding-agent/src/main.rs`, `apps/coding-agent/src/wiring.rs`, `apps/coding-agent/src/subagent.rs`
- **描述**:
  - `app_loop.rs:998`：`let boundary = QueueBoundarySource::new();` ⇒ `let boundary = Arc::new(QueueBoundarySource::new());`；`:1004`：`Some(&boundary)` ⇒ `Some(boundary.clone())`。
  - `main.rs`：`:1716` `wiring.ports_with_boundary(Some(&boundary))` ⇒ `Some(boundary.clone())`；`:1723` `AgentPorts::new(..., Some(&boundary))` ⇒ `Some(boundary.clone())`；`:1715` 附近的边界源创建改为 `Arc::new(...)`。
  - `wiring.rs`：`:254` `ports_with_boundary` 的 `boundary` 参数类型改 `Option<Arc<dyn BoundarySource>>`；`:243`（`ports()` 自调用 `ports_with_boundary(None)`）无需改（`None` 仍成立）。
  - `subagent.rs`：`:427` `AgentPorts::new(..., Some(&child_boundary))` ⇒ 传 `Arc`。
    - ⚠️ **`ChildBoundary<'a>` 必须改成 `ChildBoundary`（去掉 lifetime 参数）**，否则**编译不过**：
      - 原因：`Arc<dyn BoundarySource>` 默认隐含 `+ 'static`（已用 rustc 最小复现验证：E0597 `parent does not live long enough`）。
      - 而现形态 `ChildBoundary<'a> { parent: Option<&'a dyn BoundarySource> }` 持**父的借用** ⇒ `Arc<ChildBoundary<'_>>` **不是** `'static` ⇒ 无法 coerce 成 `Arc<dyn BoundarySource>`。
      - 修法：`ChildBoundary` 的 `parent` 字段改为 `Option<Arc<dyn BoundarySource>>`（**去掉 `'a`**）⇒ 自身成为 `'static` ⇒ `Arc::new(ChildBoundary::new(parent_arc))` 可用。
      - 连带：`ChildBoundary::new(parent: Option<Arc<dyn BoundarySource>>)` 签名改；`is_aborted` 里的 `self.parent.is_some_and(...)` 改 `self.parent.as_ref().is_some_and(...)`（`Option<Arc<..>>` 非 `Copy` ⇒ 不加 `.as_ref()` 会 E0507）。
      - 调用点：`:409` `ChildBoundary::new(boundary)` ⇒ 传 `Option<Arc<dyn BoundarySource>>`；`:801`（测试）`ChildBoundary::new(Some(&parent))` ⇒ `Some(parent_arc.clone())`。
      - **父边界源从哪来**：`SubagentRunner::run` 的 `boundary: Option<&dyn BoundarySource>`（`ys-tool` 层，**签名不变**）⇒ 在 `AppSubagentRunner::run` 内 `boundary` 是借用 ⇒ 需 `Arc::clone` 不可能（只有 `&dyn`）⇒ **解法**：`AppSubagentRunner` 改持 `Option<Arc<dyn BoundarySource>>`（在 `make_runner` 时传入）⇒ `run` 里用 `self.parent_boundary.clone()`。
        - `make_runner`（`:317`）需增一个 `parent_boundary: Option<Arc<dyn BoundarySource>>` 参数；`app_loop.rs` 建 `runner` 处（`:1000` 附近）与 `main.rs` 建 `runner` 处传入 `Some(boundary.clone())`。
    - `:70` `ChildBoundary::new` **签名**随之改（见上）。
    - `ChildBoundary`（`:64`）加 `Debug`（因 T3 加了 supertrait）—— **必须手写**（派生会打印 `parent` 内部结构）：只输出「是否有父」+「自身队列长度」，**不打印父队列内容**。
  - `subagent.rs:765` `runner.run(prompt, model, ctx.boundary)` ⇒ `ctx.boundary` 是 `Option<&'a dyn BoundarySource>`（`ToolContext` 的公开类型未变）⇒ **此行无需改** ⇒ `ys-tool` 零改动 ✓。
- **关键约束**:
  - 边界源须**活过整个 turn** ⇒ app 层持有 `Arc`（`Arc<dyn BoundarySource>` 需 `'static`，不能用栈上临时值）。
  - **每回合必须新建边界源** —— `Abort` 的标志位永久置位，复用会立刻取消之后的回合。
  - `ChildBoundary` 的 `Debug` 不得打印父队列内容（避免泄露 steer 消息）。
  - ⚠️ **子 agent 的父边界源必须一路保持 `Arc`**（`ChildBoundary` 去掉 lifetime + `AppSubagentRunner` 持 `Option<Arc<dyn BoundarySource>>`）⇒ 否则 `Arc<ChildBoundary<'_>>` 非 `'static`，`AgentPorts::new` 收不下（E0597，已用 rustc 最小复现验证）。
- **关联测试任务**: T15
- **依赖**: T5, T7, T9

### Task 15: apps/coding-agent/ — 测试 + 集成测试调用点适配 [type=test]

- **涉及目录**: `apps/coding-agent/`
- **涉及文件**: `apps/coding-agent/tests/e2e_skills.rs`, `apps/coding-agent/tests/e2e_tools.rs`, `apps/coding-agent/src/app_loop.rs`（内联测试模块）
- **描述**:
  - **测试文件也是调用点**（不得漏）：
    - `tests/e2e_skills.rs:239` —— `AgentPorts::new(Some(model), &mut session, &mut events, None)` ⇒ 适配新签名（`None` 仍成立，类型推断可能需显式标注）；
    - `tests/e2e_tools.rs:75` —— `AgentPorts::new(Some(&model), &mut session, &mut events, None)` ⇒ 同上。
  - `app_loop.rs` 内联测试：接线后 turn 正常跑通；边界源在 turn 期间可被 abort 置位。
- **验证方法**:
  - `cargo test -p ys-coding-agent` — 期望全绿（`#[ignore]` 的 e2e 除外）
  - `cargo test -p ys-coding-agent --test e2e_skills --test e2e_tools --no-run` — 期望编译通过
- **关联 impl**: T14
- **依赖**: T14

### Task 16: crates/ys-tui-coding/ — Esc 可见反馈 [type=impl]

- **涉及目录**: `crates/ys-tui-coding/`
- **涉及文件**: `crates/ys-tui-coding/src/events.rs`
- **描述**:
  - `events.rs:169-177` 的 Esc 分支：回合中按 Esc 投 `Boundary::Abort` 后，**立即**在 transcript 追加一条系统行（如「已请求取消，正在停止…」）。
  - 目的：消除「Esc 是否有反应」的困惑（模型调用期间 abort 要等 100ms 轮询才被感知）。
  - 硬约束：`ys-tui-coding` **不依赖** `ys-runtime`（编译器强制）⇒ 不得引入新依赖；系统行渲染走既有 transcript 组件。
- **关联测试任务**: T17
- **依赖**: 无

### Task 17: crates/ys-tui-coding/ — 测试 [type=test]

- **涉及目录**: `crates/ys-tui-coding/`
- **涉及文件**: `crates/ys-tui-coding/src/events.rs`（内联测试模块）
- **描述**:
  - **N7**：驱动 Esc 事件 ⇒ 断言 transcript 末行是系统提示行。
  - 既有 TUI 测试全绿。
- **验证方法**:
  - `cargo test -p ys-tui-coding` — 期望全绿
- **关联 impl**: T16
- **依赖**: T16

### Task 18: 全仓验证 + 变异测试 [type=test]

- **涉及目录**: `crates/ys-model/`, `crates/ys-loop/`, `crates/ys-tui-coding/`, `adapters/model-openai-compatible/`
- **涉及文件**: 无新增文件；在上述目录内**临时**注入 mutation（验证后必须还原，最终 diff 不得含 mutation）
- **描述**:
  - 本任务是**收尾的全量验证与变异测试任务**，与前面「按目录划分、同目录一个 task」的原则有张力：涉及文件**跨目录**（因为变异测试需要把 bug 临时注入到被测 crate 内部）。Phase 3 应把它排在**最后一个波次**；且它与前面的 test 任务**存在文件冲突**（会临时改同一批文件）⇒ 因此**不可与任何其他任务并发**，必须独占末波。
  - **N14**：`cargo build --workspace` 无错误（**25+ 处调用点是否全部适配的最终判据**）；另跑 `cargo test --workspace --no-run` 覆盖**集成测试目标**（`cargo build --workspace` 不编译 `tests/`）。
  - **N9**：`cargo test --workspace` 全绿（基线：改动前全绿）。
  - **N12 变异测试**（AGENTS.md 约定，至少 4 处）：
    | # | 注入的 bug | 期望变红 |
    |---|---|---|
    | ① | 去掉适配器的 `select!`（退化为直接 await） | N1 红 |
    | ② | 去掉 `ys-loop` 的 `Cancelled` 分流（落回通用 `Err` ⇒ 发 `RunFailed`） | N4 红 |
    | ③ | 去掉 Esc 的系统行 | N7 红 |
    | ④ | 把「先 append partial 再收场」的顺序反了 | N5 红 |
  - `cargo clippy --all-targets` 无新增警告；`cargo fmt` 后无 diff。
- **验证方法**:
  - `cargo build --workspace` — 期望无错误（N14）
  - `cargo test --workspace --no-run` — 期望所有测试目标编译通过
  - `cargo test --workspace` — 期望全绿（N9）
  - `cargo clippy --all-targets` — 期望无新增警告
  - `cargo fmt --check` — 期望无 diff
- **关联 impl**: T1, T3, T5, T7, T9, T11, T14, T16
- **依赖**: T2, T4, T6, T8, T10, T12, T13, T15, T17

## 波次计划（Phase 3 计算后回填）

**基线 ref**：`2a75f35`（含 Phase 3 执行前的 plan 修正；原初始基线 `ed5494d5`，备份分支 `sw-backup/cancellable-model-call`）

**合并规则**（R1–R4）：impl 与其配对 test 合并为同一单元（R4，保持上下文连续）⇒ **10 个执行单元**。

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|---|---|---|---|---|---|
| **U1** | T1+T2 | `ys-model/Cargo.toml`, `ys-model/src/error.rs`, `ys-model/src/request.rs` | U2 | **W1** | deep |
| **U2** | T3+T4 | `ys-protocol/src/boundary.rs` | 无 | **W0** | deep |
| **U3** | T5+T6 | `ys-component/src/context.rs` | U1, U2 | **W2** | deep |
| **U4** | T7+T8 | `ys-runtime/src/agent.rs`, `ys-runtime/src/lib.rs`, `ys-runtime/tests/run_turn_boundary.rs`, `ys-runtime/tests/v0_integration.rs` | U1, U2, U3 | **W3** | deep |
| **U5** | T9+T10 | `ys-loop/src/basic.rs`, `ys-loop/src/lib.rs`, `ys-loop/src/summarize.rs` | U1, U2, U3, U4 | **W4** | deep |
| **U6** | T11+T12 | `model-openai-compatible/Cargo.toml`, `model-openai-compatible/src/lib.rs`, `model-openai-compatible/tests/retry.rs` | U1, U2 | **W2** | deep |
| **U7** | T13 | `tools-web/tests/tools.rs` | U2 | **W2** | shallow |
| **U8** | T14+T15 | `coding-agent/src/app_loop.rs`, `src/main.rs`, `src/wiring.rs`, `src/subagent.rs`, `tests/e2e_skills.rs`, `tests/e2e_tools.rs` | U3, U4, U5 | **W5** | deep |
| **U9** | T16+T17 | `ys-tui-coding/src/events.rs` | 无 | **W0** | shallow |
| **U10** | T18 | 临时注入 mutation 于 `ys-model/`, `ys-loop/`, `ys-tui-coding/`, `model-openai-compatible/` | 全部 | **W6（独占）** | deep |

**波次说明**：

- **W0**：U2（`ys-protocol`）、U9（`ys-tui-coding`）—— 文件集不相交 ⇒ 可并发。
- **W1**：U1 —— 必须在 U2 之后（依赖 `Debug` supertrait），**不得**与 U2 同波（U1 单独编译必失败）。
- **W2**：U3（`ys-component`）、U6（适配器）、U7（`tools-web` 测试）—— 三者文件集互不相交 ⇒ 可并发（上限 3）。
- **W3**：U4（`ys-runtime`）。
- **W4**：U5（`ys-loop`）。
- **W5**：U8（`apps/coding-agent`）。
- **W6**：U10（T18）—— **独占末波**，不得与任何任务并发。

**并发上限 3**：W2 恰好 3 个单元，W0 恰好 2 个 ⇒ 无排队。

> T18 与所有其他任务**文件集相交**（变异测试需临时改动被测文件）⇒ 必须独占末波，不得与任何任务并发。
>
> **并发冲突检测提示**（Phase 3 依「涉及文件」列做）：`T3`（`crates/ys-protocol/src/boundary.rs`）与 `T4`（同文件内联测试）**同文件** ⇒ 同一单元内串行；`T1` 与 `T2` 同文件（`crates/ys-model/src/request.rs`）⇒ 同一单元内串行；`T5` 与 `T6` 同文件 ⇒ 同一单元内串行；`T7` 与 `T8` 同文件（`crates/ys-runtime/src/lib.rs`、`src/agent.rs`）⇒ 同一单元内串行；`T9` 与 `T10` 同文件（`crates/ys-loop/src/basic.rs`、`src/lib.rs`）⇒ 同一单元内串行；`T11` 与 `T12` 同文件 ⇒ 同一单元内串行；`T14` 与 `T15` 同文件（`apps/coding-agent/src/app_loop.rs`）⇒ 同一单元内串行；`T16` 与 `T17` 同文件 ⇒ 同一单元内串行。
> **关键跨目录依赖**：T1（`ModelRequest` 加 `Arc<dyn BoundarySource>` 字段）单独编译**必然失败** ⇒ **T1 与 T3（`Debug` supertrait）必须同时合入**才能编译 ⇒ Phase 3 不得把二者拆到不同波次而中途尝试编译。
>
> **生命周期约束（已用 rustc 最小复现验证）**：`Arc<dyn BoundarySource>` 隐含 `+ 'static` ⇒ `ChildBoundary<'a>`（持父借用）**无法** Arc 化 ⇒ U8 内必须把 `ChildBoundary` 改为持 `Option<Arc<dyn BoundarySource>>`（详见 T14 描述）。
>
> **包名提醒**（`cargo -p` 用的是**包名**，不是目录名）：`model-openai-compatible` → `ys-model-openai-compat`；`tools-web` → `ys-tools-web`；`coding-agent` → `ys-coding-agent`。


## 验证清单

- [ ] `cargo build` 通过
- [ ] `cargo test --workspace` 全绿（N9）
- [ ] `cargo clippy --all-targets` 无新增警告
- [ ] `cargo fmt --check` 无 diff
- [ ] `Model` trait 签名**未变**（`crates/ys-model/src/trait_def.rs:11-15`）
- [ ] `ModelResponse` **未加字段**（35 处构造点未变）
- [ ] `ModelError::Cancelled { partial }` 存在且可构造（`crates/ys-model/src/error.rs`）
- [ ] `ModelRequest` 的 `Debug` 派生**保留**（未被「去掉 Debug」绕过）
- [ ] `BoundarySource` 已加 `Debug` supertrait（`crates/ys-protocol/src/boundary.rs:26`）
- [ ] `QueueBoundarySource` 的 `Debug` **手写**且只输出 aborted 状态与队列长度（N13）
- [ ] 5 个 `impl BoundarySource` 全部可编译（`ys-protocol` / `ys-loop` / `apps` / `tools-web`×2）
- [ ] `ModelRequest.boundary` 带 `#[serde(skip, default)]`；序列化字节与改动前逐字节一致（N10）
- [ ] `ModelRequest::clone` 后 `Arc::ptr_eq` 为真（N11）
- [ ] `RuntimeContext.boundary` / `AgentPorts.boundary` 已改 `Option<Arc<dyn BoundarySource>>`
- [ ] `with_boundary` / `ports_with_boundary` / `AgentPorts::new` 签名已改
- [ ] **`AgentPorts::new` 17 处、`with_boundary` 8 处、`ports_with_boundary` 3 处调用点全部适配**（N14）
- [ ] **集成测试文件已适配**：`crates/ys-runtime/tests/run_turn_boundary.rs`、`crates/ys-runtime/tests/v0_integration.rs`、`apps/coding-agent/tests/e2e_skills.rs`、`apps/coding-agent/tests/e2e_tools.rs`
- [ ] `crates/ys-tool/` 零改动（`ToolContext.boundary` 仍是 `Option<&'a dyn BoundarySource>`；`SubagentRunner::run` 签名不变）
- [ ] app 层持有 `Arc`（非栈上临时值），`Arc<dyn BoundarySource>` 满足 `'static`
- [ ] 适配器三处 `select!` **非嵌套**；accumulator 声明在 `select!` 之外
- [ ] `AttemptError::Cancelled` 独立于 `Retryable`；取消不重试
- [ ] `into_partial` 只取 `text`，不含 `ToolUse`、不含 reasoning（N3）
- [ ] `text_block` 抽取后 `finish_with_logger` 的 content blocks 顺序不变（先 `Text` 后 `ToolUse`）
- [ ] `ys-loop` 取消分支：先 `session.append(partial)` 再发 `RunFinished`（顺序已核对）
- [ ] 取消路径**不发** `RunFailed`、**不发** `RoundUsage`
- [ ] 取消路径 `final_message` = partial 文本；轮边界 abort 的 `final_message: None` 未变
- [ ] 工具之间 abort 探针已加（每个工具调用前后）；未在 IO 工具内加轮询
- [ ] Esc 追加系统行到 transcript（N7）
- [ ] `cargo build -p ys-loop` 已跑过（生命周期陷阱已实测，非想当然）
- [ ] 变异测试 ①②③④ 各自变红，且 mutation 已还原（最终 diff 无残留）
- [ ] 既有路径逐字节不变：事件序列、序列化、`finish` 输出
