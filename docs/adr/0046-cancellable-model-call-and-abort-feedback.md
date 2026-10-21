# 0046 — 打断与响应：模型调用可取消 + TUI 取消反馈

把两条交互链路一起收口：

1. **输入后尽快得到界面反馈** —— 现状：TUI 侧按 Enter 到 `⏳ Working…` 点亮是**纯本地、零等待**的
   （`crates/ys-tui-coding/src/app.rs:394-412` 的 `begin_local_turn`），这条已经成立。
   真正的等待在模型侧（`ttft` ≈ 每轮 2.5s，占墙钟 ≈ 80%），而**等待期间用户唯一能做的「尽快」操作
   就是打断** —— 打断不生效，则「反馈」在用户体感上就是「卡死」。
2. **Esc 能尽快打断当前操作** —— 现状：`Boundary::Abort` 是**消息**，只在**轮边界**被读
   （`crates/ys-loop/src/basic.rs:137` 回合入口、`:174` 每轮 model 调用前、`:305` 压缩窗口）。
   `Model::complete` **无取消入参**（`crates/ys-model/src/trait_def.rs:12-15`）⇒ **模型调用期间 abort 不生效**。
   这是 ADR-0028 决策 4 末条记账的缺口，至今未治。

本 ADR 的核心结论：**这两条链路是同一个缺口的两面**。模型调用占墙钟 80%，而它恰好是不可打断的那 80%。
修「打断」顺带修「长尾轮逃不掉」（实测 33 轮里 3 轮长尾合计 36% 墙钟），
不需要为「反馈」引入任何新的机制。

**零新增 crate、零新增依赖、零新增配置项。**

## Status

proposed（2026-10-21；依据 `docs/arch/explore-speed/cost-model.md` 实测与 `docs/adr/0028` 决策 4 末条）

## 背景

### 打断链路的现状（as-built，2026-10-21 核实）

Esc 的入口（`crates/ys-tui-coding/src/events.rs:169-177`）：

```rust
KeyCode::Esc => {
    if app.is_turning || app.command_in_flight {
        request_abort(boundaries);      // ← 只投消息，**无任何 UI 反馈**
    } else {
        app.input.clear();
    }
}
```

`request_abort`（`events.rs:360-366`）只做 `try_send(Boundary::Abort)`，失败则 `blocking_send`。

**abort 的生效点只有 3 处，全部在 `model.complete` 之前**（`crates/ys-loop/src/basic.rs`）：

| 行 | 位置 | 说明 |
|---|---|---|
| `:137` | 回合入口 | 回合刚开始时已 aborted |
| `:174` | 每轮 model 调用**前** | 上一轮结束后才检查 |
| `:305-317` | 轮边界（压缩 `await` 窗口） | 防御性路径，正常时序不可达 |

全 crate 的 `is_aborted()` 探针**只有 `:137` 与 `:174` 两处**。`basic.rs:301` 的注释称
「轮内的工具轮询」也会命中探针 —— **与实现不符**（`ys-loop` 内不存在该探针）。

**模型调用期间无取消入参**（`crates/ys-model/src/trait_def.rs:12-15`）：

```rust
async fn complete(&self, request: ModelRequest, sink: &mut dyn ModelEventSink)
    -> Result<ModelResponse, ModelError>;
```

**中断延迟的来源链**：

```
Esc ⇒ request_abort ⇒ Abort 入队（is_aborted 立即置位）
    ⇒ 等 ys-loop 下一个探针点
       ├─ 模型调用中 → 必须等 complete 返回  ★ 阻塞点
       │    ├─ 正常流式：等本轮剩余 token（实测 1.6–3.5s）
       │    ├─ 挂死流：等流空闲超时 idle_secs = 120s（`apps/coding-agent/src/config.rs:115`）
       │    └─ 退避重试：`tokio::time::sleep(delay)` 不可中断（`adapters/model-openai-compatible/src/lib.rs:503`）
       │         最坏 = max_attempts(5) × max_delay_ms(30s) = 150s
       ├─ 工具执行中 → 取决于工具是否自查
       │    ├─ bash / exec / webfetch → 每 100ms 探一次（≤100ms 中断）✓
       │    └─ read / write / edit / find / grep / ls → 0 处探针 ✗
       │         只能等外层 budget（默认 300s）
       └─ 轮边界 → 立即生效 ✓
```

**最坏中断延迟：约 150s（退避）～ 300s（无探针工具撞满 budget）。**

### 反馈链路的现状（as-built）

TUI 侧按 Enter → `submit`（`events.rs:296`）→ `dispatch` → `begin_local_turn`（`app.rs:394-412`）
**本地立即**点亮 `is_turning = true` 并 push 用户行。状态行立刻显示 `⏳ Working…`（`draw.rs:534-551`）。
事件泵每轮 `POLL_INTERVAL = 50ms`（`run.rs:59`）排空 Outbound，渲染节流 100ms。

⇒ **TUI 侧反馈已经是「立即」的，无缺口。** 缺口在模型侧，且不在「首字节快慢」上，而在**轮数**上：

`docs/arch/explore-speed/cost-model.md:154-170` 三次实测：`ttft` 9477ms（**≈ 80%**）/ `gen` 2286ms（19%）/ `tools` 104ms（1%）。
§6.1 逐轮 ttft：30 轮稳定 1.6–3.5s，**3 轮长尾（16.4s / 47.2s / 22.0s）合计 105s = 36% 墙钟**。
§7 外部对照（同一 t4 提示）：codex 128s/≈6 轮 vs 我们 291s/**33 轮** ⇒ **墙钟差距的主因是轮数，不是单轮 ttft**。
§8.5 推理强度 A/B：`high`（现行默认）墙钟中位 100.7s / 9 轮。

### 约束

- `ys-core` / `ys-event` / `ys-protocol` **零 tokio**（直接与间接皆无）。
- `ys-model` 当前依赖 `ys-core` / `ys-event` / `ys-tool`，**不依赖 `ys-protocol`**。
- `ModelEventSink` 必须保持同步 `fn`（ADR-0009 推论：跨动态库边界传 trait object 困难）。
- `ModelError` **非 `#[non_exhaustive]`** ⇒ 加变体是 breaking（ADR-0028 背景已记）。
- `ys-tui-coding` 不依赖 `ys-runtime`。
- 「不跨动态库边界传递 Rust trait object、Tokio 类型」—— 故取消信号**不能**用 tokio 的 `CancellationToken` 传给模型层。

## 决策

### 决策 1：取消信号用 `ys-protocol` 的 `BoundarySource`，**不改 `Model` trait 签名**

**做法**：把 `BoundarySource` 挂进 `ModelRequest`。

```rust
// crates/ys-model/src/request.rs
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// 轮边界控制源：模型层可轮询 `is_aborted()` 以感知中止。
    ///
    /// `None` = 无边界控制；模型层应视作「未中止」。
    /// **不进序列化**（`#[serde(skip)]`）—— 它是运行时控制面，不是请求内容。
    #[serde(skip)]
    pub boundary: Option<std::sync::Arc<dyn BoundarySource>>,
}
```

**为什么不是「给 `Model::complete` 加一个取消入参」**：

那会改 `Model` trait 的方法签名 ⇒ 改所有实现（适配器 + `MockModel` + `ys-loop` 里的测试替身），
且 `ModelEventSink` 已是 trait object（`&mut dyn ModelEventSink`），再加一个 trait object 入参会让
「跨动态库边界不传 trait object」的约束变得更紧。**挂进 `ModelRequest` 是同一件事的另一种表达**：
请求对象已经随调用跨越边界，边界源作为请求的**一部分**随行，不需要新增第二个跨边界的 trait object。

**为什么 `Arc`**：`ModelRequest` 需 `Clone`（`ys-loop` 复用请求、`ys-loop` 的 `..Default::default()` 构造），
`Option<&dyn BoundarySource>` 借用无法 `Clone`，`Arc` 是唯一既 `Clone` 又能跨 `'static` 边界的形态。

**为什么 `ys-model` 要新增 `ys-protocol` 依赖**：`ys-protocol` 零 tokio、只依赖 `ys-core` + `ys-event`，
与 `ys-model` 同层，**不违反依赖方向**（`ys-core` ← `ys-protocol` ← `ys-model` ← `ys-loop`）。
它已经是 `ys-tool` 的依赖，而 `ys-model` 依赖 `ys-tool` ⇒ 依赖图无环。

**`ModelRequest` 的 `Clone` 派生**：`#[serde(skip)]` 的字段需要 `Default` for `Option<Arc<...>>` ⇒
`Option` 的 `Default` 是 `None` ⇒ 派生 `Clone` 要求该字段 `Clone` ⇒ `Arc<dyn BoundarySource>` 是 `Clone` ✓。

**`serde(skip)` 的双向问题**：`#[serde(skip)]` 同时跳过序列化**与**反序列化 ⇒ 需要
`#[serde(skip, default)]` 才能反序列化（`skip` 已隐含 `default`，但显式写更清晰）。
`ModelRequest` 现有字段 `system` / `max_tokens` / `temperature` 用的是 `#[serde(default)]`，
而 `messages` / `tools` 没有 ⇒ 序列化时 `boundary` 被跳过，反序列化时为 `None`。**既有序列化字节逐字节不变** ✓

**为什么不引入 `ys-protocol` 依赖而是把 `BoundarySource` 复制到 `ys-model`**：重复定义同一个 trait 是
两份真相源，适配器无法用同一个对象同时喂给工具和模型。**不复制。**

### 决策 2：模型层用「轮询 + select!」实现取消，`ModelError` **加 1 个携带型变体**

**做法**（`adapters/model-openai-compatible/src/lib.rs`）：把 `complete` 的请求-响应体放进
`tokio::select!`，与一个「abort 监视 future」赛跑。

```rust
// 伪代码（示意结构，不是最终代码）
let body = async { /* 现有请求体：send + bytes_stream 消费 */ };
tokio::select! {
    biased;                        // 优先看 abort
    _ = wait_abort(req.boundary.as_deref()) => {
        // 累积器里可能已有部分内容 —— 带上它（见决策 3′）
        return Err(ModelError::Cancelled { partial: accumulator.into_partial() });
    }
    r = body => r,
}
```

`wait_abort` 的形态**照抄** `adapters/tools-basic/src/exec.rs` 已有的实现
（`exec.rs:305` + `ABORT_POLL_INTERVAL = 100ms`，`exec.rs:29-30`）：

```rust
async fn wait_abort(boundary: Option<&dyn BoundarySource>) {
    match boundary {
        Some(boundary) => loop {
            if boundary.is_aborted() { return; }
            tokio::time::sleep(ABORT_POLL_INTERVAL).await;
        },
        None => std::future::pending::<()>().await,
    }
}
```

**关键点**：`select!` 丢弃 `body` future ⇒ **底层 HTTP 连接随之 drop**（reqwest 的 future 被 drop
会取消该请求）⇒ **无需任何额外清理**。适配器内**无 detached spawn**（核实：全文件无 `tokio::spawn`），
故不存在「孤儿请求继续跑」的问题。

**取消的感知延迟上界 = 100ms**（一个 `ABORT_POLL_INTERVAL`）。

#### select! 的**落点**（决定 partial 能否取到）

`select!` 丢弃被选中分支之外的 future ⇒ **若 accumulator 在那个 future 里，它会被 drop**
⇒ partial 丢失。**落点必须让 accumulator 活过 select!。**

**as-built 的调用链**（核实）：

```
Model::complete                       (lib.rs:396)
  └─ for attempt in 0..max_attempts   (lib.rs:414)   ← 有界重试循环
       └─ self.attempt(...)           (lib.rs:518)
            ├─ self.send(url, ...)                        ← HTTP 请求（可被 select! 包住）
            └─ let mut accumulator = StreamAccumulator::new(&mut *sink, ...)   (lib.rs:598)
               └─ parse_sse_stream(response, idle, |ev| accumulator.handle(ev), ...)
                                                      (lib.rs:601)  ← 流消费
```

**三个可选落点**：

| 落点 | 能包住什么 | accumulator 存活？ | 评价 |
|---|---|---|---|
| A. `complete` 外层包住整个 `for` 循环 | 发送 + 流消费 + **退避 sleep** | ✅ 存活（但在 loop 外） | **覆盖最全**：连退避期间也可打断（ADR-0028 决策 4 的缺口） |
| B. `attempt` 内包住 `parse_sse_stream` | 仅流消费 | ✅ 存活（同作用域） | 覆盖流阶段，但**盖不住** `send` 与退避 |
| C. `attempt` 内包住 `send` | 仅 HTTP 发送 | ✅ 存活 | 只盖住握手阶段 |

**采纳落点 A**（在 `complete` 内、`for` 循环**外**层）：

```rust
// 伪代码
let body = async {
    for attempt in 0..max_attempts {
        // ... 现有重试循环（含退避 sleep）
    }
};
tokio::select! {
    biased;
    _ = wait_abort(req.boundary.as_deref()) => {
        // accumulator 在 body 内，已随 future 被 drop ⇒ 此处取不到 partial
        return Err(ModelError::Cancelled { partial: None });
    }
    r = body => return r,
}
```

**落点 A 的问题**：accumulator 在 `body` future **内部** ⇒ `select!` 丢弃 body 时**一并 drop** ⇒
**partial 取不到** ❌

⇒ **落点 A 与 partial 需求相冲**。

**采纳落点 B**（在 `attempt` 内、包住 `parse_sse_stream`）：

```rust
// attempt 内，accumulator 已创建之后
let streamed = async {
    parse_sse_stream(response, idle, |ev| accumulator.handle(ev), || { ... })
        .await
};
let parse_result = tokio::select! {
    biased;
    _ = wait_abort(boundary) => {
        // ✅ accumulator 在本作用域、未被 drop（它在 select! 之外声明）
        let partial = accumulator.into_partial();
        return AttemptCall {
            result: Err(AttemptError::Cancelled { partial }),
            meta,
        };
    }
    r = streamed => r,
};
```

**关键**：accumulator **在 `select!` 之前声明**（栈上）⇒ `select!` 只 drop `streamed` future
（它只是**借用** accumulator 的闭包）⇒ **accumulator 存活** ⇒ `into_partial()` 可用 ✓

**落点 B 的缺口**：`send` 阶段（HTTP 握手）与退避 `sleep` 阶段**盖不住**。
⇒ **拆成两处**：

- **B1（带 partial）**：`attempt` 内包住 `parse_sse_stream` ⇒ **流阶段可取消且能取 partial** ✓
- **B2（不带 partial）**：`complete` 内包住**整个重试循环** ⇒ 发送 / 退避阶段可取消
  （此时没有累积器，partial 恒为 `None`）✓

**但 B1 + B2 嵌套**：外层 B2 的 `select!` 会 drop 内层 B1 的 future ⇒ B1 的 accumulator 被 drop ⇒
**B1 的 partial 也丢了** ❌

⇒ **不能嵌套两个 select!**。

**解法**：**单层 select!，落点在 `complete` 内的重试循环外**（落点 A），
但**把 accumulator 提到 `complete` 层**，让它活在 select! 之外：

```rust
// complete 内，select! 之前
let mut accumulator: Option<StreamAccumulator> = None;
let body = async {
    for attempt in 0..max_attempts {
        // attempt 内把 accumulator 借出去：
        //   attempt 每次新建一个，结束后归还
    }
};
```

**这个借用模型很繁复**（accumulator 借用 sink，而 sink 又被 `DeltaWatchingSink` 包了一层）。

**更简单的解法**：**接受「partial 只在流阶段可得」**，
把 select! 做成**单层、落点在 `attempt` 内**（落点 B），并**把 `send` 与退避的取消**
作为**独立的小 select!** 处理：

- **流阶段**（B1）：`select!` 在 `attempt` 内 ⇒ partial 可取 ✓
- **发送阶段**：`send()` 之前包一个 `select!` ⇒ 取消 ⇒ `partial: None` ✓
- **退避阶段**：退避 `sleep` 处包一个 `select!` ⇒ 取消 ⇒ `partial: None` ✓

**三处 select! 不嵌套**（发送/退避在流之前或之后，彼此不重叠）⇒ **无 drop 竞争** ✓

**这是最终的落点方案**：**三处非嵌套 select!**，其中只有流阶段能拿到 partial。

**为何三处而不做「统一的外层」**：统一外层需要 accumulator 活在 select! 外，
而 accumulator **借用 sink** 且**在每次重试中重建** ⇒ 借用关系跨 `await` 且跨循环迭代
⇒ 借用检查器与 `select!` 的 drop 语义交织，**复杂且易错**。
三处非嵌套 select! 各自局部、互不干扰 ⇒ **可读、可测**。

**`AttemptError` 需要第三支**：核实 `AttemptError` 只有 `Retryable` / `Fatal` 两支
（`lib.rs:312-324`）⇒ 取消若被归入 `Retryable` ⇒ **重试循环会重放**（取消后重试 = 违背用户意图，
且可能无限重试）⇒ 必须新增：

```rust
enum AttemptError {
    Retryable { .. },
    Fatal { .. },
    /// **用户主动取消**：不重试、不上报为失败。
    Cancelled { partial: Option<ys_core::Message> },
}
```

**重试循环的处理**：`complete` 的 `match outcome` 里，`Err(AttemptError::Cancelled { partial })`
⇒ **立即 `return Err(ModelError::Cancelled { partial })`**（不走重试、不 trace 为失败）✓

**`complete` 的重试循环已有 `meta.stream_started` 抑制重试的先例**（`lib.rs:461-468`）
⇒ 取消路径与它语义一致：**已吐过 delta 就不该重放**。取消是更强的理由（用户明确要求停）✓

#### 为什么把部分响应放进 `ModelError`，而不加 `ModelResponse` 字段

`ModelResponse` 加一个 `cancelled: bool` 字段看似更「干净」，但代价是**35 处结构体字面量全要改**
（核实：`grep -rn "ModelResponse {"` 命中 35 处，**全部是字面量、无一处用 `..Default::default()`**
—— 因为 `ModelResponse` **没有** `Default` 派生）。35 处机械修改 = 35 个可被漏掉的编译点
（漏一个就是编译错误，还算好；真正的问题是**审阅负担**与**后续合并冲突**）。

而 `ModelError` 加变体的代价（核实）：全仓 **12 处**引用，**无一处穷尽匹配**（全是构造）⇒
**只需改 1 处定义 + 1 处 `ys-loop` 的 match**。

⇒ **采纳 `ModelError::Cancelled` 携带部分消息**。

**变体定义**（`crates/ys-model/src/error.rs`）：

```rust
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("event sink error: {0}")]
    Sink(#[from] EventError),
    #[error("model provider error: {0}")]
    Provider(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// **用户主动取消**：模型调用在完成前被中止。
    ///
    /// `partial` 携带**已产生的部分内容**（可能为空）—— 上层**应当**把它追加进
    /// 会话历史，否则屏幕上看得见的文本会与下一轮请求的上下文不一致。
    #[error("model call cancelled{}", if partial.is_empty() { "" } else { " (partial output kept)" })]
    Cancelled { partial: Option<ys_core::Message> },
}
```

**为什么 `partial` 是 `Option<Message>` 而不是 `Message`**：

- 取消可能发生在**任何**时刻 —— 包括**一个字节都没吐**（排队中、连接中）⇒ 无内容可带 ⇒ `None`。
- `None` 与 `Some(空消息)` 语义不同：前者「从未有输出」，后者「有输出但为空」⇒ 用 `Option` 区分。

**`thiserror` 的 `#[error(...)]` 里不能用 `if` 表达式配 `partial.is_empty()`** ——
`thiserror` 的 format 串支持条件：`#[error("... {}", if .partial.is_some() { "x" } else { "y" })]`
但语法较绕。**简化**：错误文案统一为固定串，**不区分有无部分内容**：

```rust
#[error("model call cancelled")]
Cancelled { partial: Option<ys_core::Message> },
```

**文案简化是刻意的**：错误文案会进 `RunFailed` 的 `error` 字段与 debug 日志，
而 `partial` 的存在与否由**结构**表达（`Option`），不需要在文案里重复。

**`thiserror` 对含字段变体的要求**：`Cancelled { partial: Option<Message> }` 需要 `Message: Debug`
（`Message` 派生 `Debug` ✓）。

**`ModelError` 是否加 `#[non_exhaustive]`**：**不加**。加了会让**所有** `match` 都要加 `_` 臂
（包括 `ys-loop` 的），反而增加噪音。仓内无发布件 ⇒ 12 处构造点全在本仓 ⇒ 可控。

**`ModelError` 加变体是否 breaking**：对**穷尽匹配**是 breaking。核实：全仓**无**对 `ModelError` 的
穷尽匹配（12 处引用全是构造 + `matches!` 断言）⇒ **仓内不 breaking** ✓
（`crates/ys-model/src/lib.rs:72` 是 `matches!(err, ModelError::Provider(_))` ⇒ 不受影响）。

**`ModelEventSink` 不变** ✓（`Model` trait 的方法签名完全不变 —— 边界源走 `ModelRequest`）。

**决策 2 的净效果**：`Model` trait **零改动**、`ModelEventSink` **零改动**、
`ModelResponse` **零改动**（35 处构造点全不动）、`ModelError` **加 1 个变体**（1 处定义 + 1 处 match）。

### 决策 3：`ys-loop` 区分「取消」与「失败」，产出 `RunFinished{Cancelled}` 而非 `RunFailed`

**做法**（`crates/ys-loop/src/basic.rs` Step 6 之后）：`model.complete` 返回
`Err(ModelError::Cancelled { partial })` 时，**不发 `RunFailed`**，改为：

1. 若 `partial` 有内容 ⇒ **先把它追加进会话历史**（见决策 3′）；
2. 发 `RunFinished { stop_reason: StopReason::Cancelled, usage: total_usage, rounds }`；
3. 返回 `Ok(RunResult { stop_reason: Cancelled, final_message: <partial 的文本或 None>, ... })`。

**as-built 落点（核实 `basic.rs`）**：

- **错误 match 在 `basic.rs:363-377`**：现有形态是「任何 `Err(e)` ⇒ 发 `RunFailed` ⇒ `return Err`」。
  ⇒ 在这个 match 里**加一支**：`Err(ModelError::Cancelled { partial })` ⇒ **走取消路径**，
  **不发** `RunFailed`。
- **追加历史的既有路径在 `basic.rs:398-401`**（`ctx.session.append(assistant_message)`）——
  但它在 **`Ok(response)` 之后**，**取消分支进不去** ⇒ 取消路径需要一个**独立的 append 调用**。

**⚠️ 关键：`final_message` 与 `text_content` 的关系**

`basic.rs:403-409` 在 append 之后从 `assistant_message` 提取 `text_content` ⇒ 用作 `final_message`。
取消路径下：

- **若 partial 存在** ⇒ 那段文本**已经 append 进历史** ⇒ `final_message` **应当**是 partial 的文本
  （否则「历史里有这段文本，但 `final_message` 为 `None`」⇒ 下游（`-p` 模式的最终输出、
  TUI 的结束行）**看不到**用户已经看到的内容）。
- **若 partial 为 `None`** ⇒ `final_message: None`（确实没有输出）✓

⇒ **取消路径的 `final_message` = partial 的文本（若有）**，与 `StopReason::Cancelled` **并存**
（不是二选一）。轮边界 abort 路径（`basic.rs:328-333`）用 `final_message: None` 是对的 ——
那里**确实**没有输出。**两处形态不同不是不一致，而是事实不同** ✓

**为什么不在 `Model` 层直接返回 `Ok`**：模型层不负责决定回合的收场形态（那是 loop 的职责），
且「取消」与「模型自己失败」必须能被区分。`ModelError::Cancelled` 正是这个区分点。

**usage 记账**：取消时无 usage 可记（流被中断，provider 未给 usage）⇒ `total_usage` 用到当前轮为止的值
（与轮边界 abort 的处理一致，`basic.rs:322`）。**本轮的 `RoundUsage` 不发** ——
本轮**没有完整的 usage** ⇒ 发一个假的 usage 会让计费统计多出一轮 0-token 记录，
反而更乱。⇒ **取消路径不发 `RoundUsage`**（与轮边界 abort 一致：abort 发生在轮边界，压根没进模型调用）。

**「先 append 再发终局事件」的顺序不可颠倒**：若先发 `RunFinished` 再 append，
消费端可能已认为回合结束 ⇒ 后到的 append 无人观察 ⇒ 部分输出丢失。

### 决策 3′：取消时**已产生的部分输出**必须保留（**不可丢**）

**这是本 ADR 最容易引入 bug 的地方，必须显式定义。**

事实链：

- 适配器的流式累积器（`adapters/model-openai-compatible/src/lib.rs:881` `StreamAccumulator`）
  **边收增量边发** `ModelEvent::TextDelta` ⇒ **取消前已发出的 delta 已经进了 TUI 的 transcript**
  （`crates/ys-tui-coding/src/app.rs` 的 `append_stream`），**屏幕上看得见**。
- 但取消时流被中断 ⇒ 累积器**没有** `finish()` 过 ⇒ 若不把它交出来，**已可见的文本会丢失**。

⇒ 若照「错误路径」处理（直接 return 不管累积器），**屏幕上留着一段 assistant 文本，但会话历史里没有它**。
下一次请求的上下文就与用户看到的**不一致** —— 这是一个真实的数据不一致 bug
（比「取消慢」严重得多）。

**已有的正确形态**：`basic.rs:379` 的注释「调用中取消 -> 仍要追加」说明**既有约定就是保留部分输出**。

**做法（三处配合，缺一不可）**：

1. **适配器**：`StreamAccumulator` 需一个「未 finish 就被中止」的出口 —— 把累积器当前状态
   收成 `Option<Message>`（空则 `None`），随 `ModelError::Cancelled { partial }` 交出。

   **只收文本，不收 tool call**（见决策 3″）。

   **`StreamAccumulator` 的可复用性（as-built 核实）**：

   - **结构**（`adapters/model-openai-compatible/src/lib.rs:885-898`）：持有 `text` / `reasoning` /
     `tool_calls` / `finish_reason` / `usage` 五个累积字段 + `sink: &mut dyn ModelEventSink`。
   - **生产调用点只有 1 处**：`lib.rs:598-599`（在 `attempt` 内）。其余 16 处全在
     `#[cfg(test)]` 模块（`lib.rs:1350` 起）。
   - **生命周期**：`StreamAccumulator::new(&mut *sink, ...)` 借用 sink；`finish(self)`
     **消费 self** ⇒ 只能在**流的末尾**调用一次。
   - **已有出口**：只有 `finish()` / `finish_with_logger()`（`lib.rs:1007` / `1012`），
     **二者都返回 `Result<ModelResponse, ModelError>`** ⇒ **不能直接复用为 partial 出口**：
     它们会把 `tool_calls` 一并放进 `content_blocks`（`lib.rs:1028-1033`），
     而 partial 必须**丢弃** tool call（决策 3″）。

   ⇒ **需要新增一个方法**：`fn into_partial(self) -> Option<Message>` ——
   只取 `self.text`（非空则产出 `ContentBlock::Text`），**不碰 `tool_calls`**。

   **`finish()` 与 `into_partial()` 的关系**：两者**共用**「从 `text` 造 `ContentBlock::Text`」
   这一步（`lib.rs:1025-1027`）。⇒ 把该步抽成一个小函数，两处共用 ⇒ 避免两处逻辑漂移。

   **新方法的爆炸半径**：**1 个新方法 + 1 处小函数抽取**，**不改任何既有行为** ✓
   （`finish` 路径的字节输出完全不变）。

2. **`ys-loop`**：`Err(ModelError::Cancelled { partial })` ⇒ 若 `partial` 有内容，
   **先 `ctx.session.append(partial)`**（与 Step 7 的正常追加同一路径），
   **再**发 `RunFinished{Cancelled}` 收场。

   **顺序至关重要**：必须**先 append 再收场**，否则部分输出丢失。

3. **TUI 侧无需改动**：partial 的文本**已经在屏幕上**（delta 已发过）⇒ 追加进历史后，
   屏幕与历史**一致** ✓

**这个决策解决了什么**：避免了「屏幕有文本、历史无文本」的不一致 —— 后者会让下一轮模型
**看到与用户不同的对话**，产生难以复现的行为偏差。

### 决策 3″：取消时**工具调用**不执行、不入历史

若模型在吐出 tool call 的**参数增量途中**被取消，累积器里可能有**不完整**的 tool call。

**做法**：

- 适配器收 partial 时**只取文本块**（`ContentBlock::Text`），**丢弃** `ContentBlock::ToolUse`。
- `ys-loop` 收到取消后**不进入 Step 8**（工具调用）⇒ 不会执行半截工具调用 ✓

**为什么丢弃而不是保留**：半截 tool_use block 进了历史 ⇒ 下一轮请求会带上**不完整**的
tool_use ⇒ provider 可能报错（ADR-0043「resume 修复中断的工具调用」是同类问题的既有处理）。
**丢弃半截块比留一个坏块更安全**。

**「文本」的准确定义**（核实 `crates/ys-core/src/message.rs:20-33`）：
`ContentBlock` 只有 `Text` / `ToolUse` / `ToolResult` **三个变体**，**没有 Thinking 变体**。
⇒ partial 里能保留的只有 `ContentBlock::Text`。
**思考内容（reasoning）不入历史** —— 它本就**只经事件出口**（`StreamAccumulator` 的
`reasoning` 字段只喂 usage 统计，不写进 `content_blocks`，见 `lib.rs:992` 注释
「Thinking 段只 emit，不写 `self.reasoning`」）⇒ **行为一致**：partial 也只取 `Text`。

### 决策 4：把边界源传进 `ModelRequest`（接线，`ys-loop` 一处 + `ys-runtime` 一处）

`ys-loop` Step 5 组装 `ModelRequest` 处（`basic.rs:340-360`）加一个字段：

```rust
let request = ModelRequest {
    messages: ...,
    tools,
    system: ctx.system_prompt.clone(),
    boundary: ctx.boundary.map(|b| b.clone_arc()),   // 见下
    ..Default::default()
};
```

**问题**：`ctx.boundary` 的类型是 `Option<&dyn BoundarySource>`（借用），
而 `ModelRequest` 要 `Option<Arc<dyn BoundarySource>>` ⇒ 需要一个 `Arc` 化的来源。

**做法**：`AgentPorts` / `RuntimeContext` 持有的边界源改为 `Arc<QueueBoundarySource>`（**具体类型**），
`BoundarySource` trait object 在**构造时**就 `Arc` 化。

**但 `ys-loop` 的 `ctx.boundary` 是 `Option<&dyn BoundarySource>`**（`ys-loop/src/lib.rs` 的 `RunContext`），
且 `QueueBoundarySource` 只在 `ys-protocol`（`apps/coding-agent` 与 `ys-runtime` 都用它）。

**更简单的做法（采纳）**：**不改 `RunContext.boundary` 的类型**，而是在 `ModelRequest` 里放
`Option<Arc<dyn BoundarySource>>`，由 **`ys-loop` 之外**（app 层）在构造 `AgentPorts` 时
就把 `Arc` 化的边界源交给 `ys-loop`。

**核实当前接线**：`apps/coding-agent/src/app_loop.rs:998` 建 `QueueBoundarySource::new()`，
`:1004` 交给 `AgentPorts::new(..., Some(&boundary))` ⇒ `ys-loop` 拿到 `&dyn BoundarySource`。

⇒ **改动**：`app_loop.rs:998` 改为 `let boundary = Arc::new(QueueBoundarySource::new());`，
`:1004` 传 `Some(boundary.clone())`（`Arc<QueueBoundarySource>` 需 `Deref` 到 `dyn BoundarySource`），
`ys-loop` 的 `RunContext.boundary` 类型从 `Option<&dyn BoundarySource>` 改成
`Option<Arc<dyn BoundarySource>>`。

**这个改动的影响面**：`RunContext.boundary` 被 `ys-loop` 内部多处用（`basic.rs:137/174/305/670/724`），
其中 `basic.rs:670` 与 `:724` 把它传给 `ToolContext::new(ctx.boundary, ...)` ——
`ToolContext.boundary` 是 `Option<&'a dyn BoundarySource>`。

⇒ **`ToolContext` 也要改**：`Option<&'a dyn BoundarySource>` → `Option<&'a dyn BoundarySource>` 保持不变，
但**传参处**要 `boundary.as_deref()`。这**不改变 `ToolContext` 的公开类型** ⇒ **工具层零改动** ✓
（`ToolContext` 的生命周期 `'a` 与 `Arc` 借用的关系：`Arc<dyn BoundarySource>` 借出 `&dyn` 需 `as_deref()`，
其生命周期短于 `Arc` 的持有期 ⇒ 只要 `ctx` 活得比 `tool_ctx` 久即可，当前结构已满足）。

**`AgentPorts` 的类型**：`ys-runtime` 的 `AgentPorts::new(model, session, events, boundary)` ——
最后一个参数的类型需从 `Option<&dyn BoundarySource>` 改成 `Option<Arc<dyn BoundarySource>>`。
`ys-runtime` 是**对外的 facade**（锁定外部导入路径）⇒ 这是**对外 API 的 breaking 变更**。

**规避**：`AgentPorts::new` 保留原签名（`Option<&dyn BoundarySource>`），
**新增** `AgentPorts::new_with_boundary(Option<Arc<dyn BoundarySource>>)`。
旧签名内部 `Arc::new` 一次（`Option<&dyn>` 的生命周期短于 `Arc` 的持有期 ⇒ 不能 `Arc::new` 一个借用！）

**借用不能 `Arc::new`** ⇒ 旧签名**无法**转成 `Arc` ⇒ 必须 breaking 或换路子。

**换路子（采纳）**：**不改 `AgentPorts` 的 boundary 参数类型**，改为在 `ys-loop` 内部
把 `Option<&dyn BoundarySource>` **升级**为 `Arc`：

```rust
// ys-loop 内部：把借用转成拥有
let boundary: Option<Arc<dyn BoundarySource>> = ctx.boundary.map(|b| Arc::new(BorrowedSource(b)));
```

需要一个 `BorrowedSource<'a>(&'a dyn BoundarySource)` 适配器 —— 但它的生命周期短于 `'static`，
`Arc<dyn BoundarySource>` 要求 `'static` ⇒ **不可行**。

⇒ **结论**：要让边界源进 `ModelRequest`（`'static` 要求的 `Arc`），
**边界源本身必须以 `Arc` 形式被 `ys-loop` 持有**。

**最终接线方案（采纳）**：

1. `ys-protocol`：`QueueBoundarySource` 保持不变（`Mutex` 内可变，无需 `Arc`）。
2. **`RunContext.boundary` / `AgentPorts` 的 boundary 参数：`Option<&dyn BoundarySource>` → `Option<Arc<dyn BoundarySource>>`。**
3. `ys-loop` 内部：`ctx.boundary.as_deref()` 传给 `ToolContext`（工具层公开类型不变 ⇒ 工具零改动）。
4. app 层：`Arc::new(QueueBoundarySource::new())`，`boundary.clone()` 传给 `AgentPorts`，
   `boundary.push(...)` 处改为 `boundary.push(...)`（`Arc` 的 `Deref` 让方法调用不变）⇒ **app 层改动极小**。

**`AgentPorts` breaking 的影响面**：`ys-runtime` 是对外 facade，但**本仓内**的调用点只有
`app_loop.rs:1004` 与 `main.rs:1718`（`-p` 路径）⇒ 仓内可控。
本仓之外若有人用 `ys-runtime` ⇒ breaking，但**本仓无发布件**（`version.workspace = true`，无 `publish = true`）⇒ 可接受。

**核实**：`Cargo.toml` 各 crate 是否有 `publish = false`？需要确认。若无，则 `ys-runtime` 的
「锁定外部导入路径」定位仍成立，但**发布状态**需查。

### 决策 5：`ys-loop` 组装 `ModelRequest` 时挂边界源；**`ModelRequest` 的 `Clone` 保持**

`basic.rs:340-360` 的 `ModelRequest { ... ..Default::default() }` 加 `boundary` 字段。
`ModelRequest` 派生 `Clone`（核实：`request.rs:7` 有 `#[derive(...)]`，需确认含 `Clone`）⇒
`Option<Arc<dyn BoundarySource>>` 是 `Clone` ✓

### 决策 6：TUI 侧 Esc 立即给出可见反馈（**不改 `Model` trait 就能做**）

**这是本 ADR 爆炸半径最小、收益最直接的一条。**

现状：`Esc`（`events.rs:169-177`）调 `request_abort(boundaries)` 后**界面完全不变** ——
状态行仍是 `⏳ Working…`，transcript 无新行。用户无法区分「Esc 没生效」与「正在取消」。

而 Ctrl-C 有明确反馈（`events.rs:136-145`）：

```rust
app.abort_requested = true;
request_abort(boundaries);
app.transcript.push(TranscriptLine::System(
    "已请求取消当前回合；若长时间无响应，再按一次 Ctrl-C 退出".into(),
));
```

**做法**：`Esc` 分支补一条**同样的**系统行。**复用 `request_abort` 的调用点，不改 `request_abort` 本身**
（`request_abort` 被 3 处调用：`events.rs:136`（Ctrl-C）、`:175`（Esc）、`:328`（`Action::Abort`））。

**新文案**（与 Ctrl-C 的不同：Esc 不承担「二次退出」语义）：

```
已请求取消当前回合；若长时间无响应，再按一次 Ctrl-C 退出
```

**与 Ctrl-C 相同还是不同？** ⇒ **相同**。理由：Esc 与 Ctrl-C 的第一段语义**完全一致**
（都是「请求取消当前回合」），文案一致减少认知负担；且 Esc 不设置 `abort_requested`
（那是 Ctrl-C 两段式的判据），所以 Esc 按多次不会误退出。

**`abort_requested` 置位？** ⇒ **不置**。`abort_requested` 是 Ctrl-C 两段式的判据
（`events.rs:132`），Esc 不参与。保持 `Esc` 的现有语义不变 ⇒ **零行为变更**。

**爆炸半径**：`crates/ys-tui-coding/src/events.rs` **1 处 3 行** + 2 条测试
（一条钉「产生系统行」，一条钉「**不置** `abort_requested`」）。

### 决策 7：补齐 6 个文件类工具的 abort 探针

现状：`bash` / `exec` / `webfetch` 都有 abort 自查（`bash.rs:104`、`exec.rs:305`、`fetch.rs:409`，
轮询间隔 100ms）。而 `read` / `write` / `edit` / `find` / `grep` / `ls` **0 处探针**。

⇒ 这 6 个工具**跑到外层 budget（默认 300s）才停**。

**做法**：这 6 个工具是**短时同步 IO**（读文件、grep），实际耗时在毫秒～秒级。
**不加轮询**（轮询对毫秒级操作是纯开销）—— 改为**在 `ys-loop` 侧**补：

`basic.rs` 的工具执行处（`:640-740`），在**每个工具调用前后**检查一次 `is_aborted()`：

```rust
// 发起工具调用前：已中止则不再发起
if ctx.boundary.is_some_and(|b| b.is_aborted()) { ... 收场 ... }
// 每个工具结果返回后：再查一次
```

**为什么在 loop 侧而不是工具侧**：短时工具不需要 100ms 轮询；「工具之间」才是可中断的粒度。
`bash` / `exec` 这类**长时**工具已在工具侧自查（`select!` 打断进程）⇒ 两层互补、不重复。

**爆炸半径**：`crates/ys-loop/src/basic.rs` **1 处**（Step 9 内加 2 个探针）。

### 决策 8：修 `basic.rs:301` 的注释与实现不符

注释称「轮内的工具轮询」会命中 abort 探针，但 `ys-loop` 内 `is_aborted()` 只有 2 处，
均在 model 调用前。⇒ **改成如实描述**（决策 7 落地后，工具之间有探针，轮内工具执行期间仍无）。

### 决策 9（不采纳）：给 `Model::complete` 加取消入参

见「Considered Options」。核心理由：改 trait 方法签名 ⇒ 改所有实现 + 跨边界的 trait object 更多；
而挂进 `ModelRequest` 达到同一效果且 **`Model` trait 零改动**。

### 决策 10（不采纳）：引入 tokio `CancellationToken`

违反「`ys-core` / `ys-event` / `ys-protocol` 不含 tokio」+「不跨动态库边界传递 Tokio 类型」
（`ys-protocol` 零 tokio 是硬约束）。⇒ 复用既有的 `BoundarySource`（`ys-protocol` 零 tokio）✓

## 变更清单（按落地顺序）

| # | 改动 | 文件 | 爆炸半径 |
|---|---|---|---|
| 1 | Esc 补系统行反馈 | `crates/ys-tui-coding/src/events.rs:169-177` | 1 文件 3 行 |
| 2 | 修注释与实现不符 | `crates/ys-loop/src/basic.rs:301` | 1 处注释 |
| 3 | `ModelError::Cancelled { partial }` 变体 | `crates/ys-model/src/error.rs` | 加 1 变体（仓内封闭） |
| 4 | `ModelRequest.boundary` 字段 | `crates/ys-model/src/request.rs` | 1 字段 + `ys-model` 加 `ys-protocol` 依赖 |
| 5 | `ys-loop` 挂边界源进请求 | `crates/ys-loop/src/basic.rs:340-360` | 1 处构造 |
| 6 | 边界源类型 `&dyn` → `Arc<dyn>` | `ys-loop`(`RunContext`) / `ys-runtime`(`AgentPorts`) / app 层 | 3 crate，仓内调用点 2 处 |
| 7 | 适配器 `select!` 实现取消 | `adapters/model-openai-compatible/src/lib.rs` | 1 处 + 新增 `wait_abort` |
| 8 | 适配器收出部分输出（`StreamAccumulator` 中止出口） | 同上 | 1 处（`partial` 只含文本块） |
| 9 | `ys-loop` 收到 `Cancelled` 走 `RunFinished` + 先 append partial | `crates/ys-loop/src/basic.rs:365-379` | 1 处 match + 1 处 append |
| 10 | 工具之间补 abort 探针 | `crates/ys-loop/src/basic.rs` Step 9 | 1 处 |

**依赖顺序**：3 → 4 → 5 → 7 → 8 → 9 一组（取消链路）；1、2、6、10 各自独立。

**「先落地后观测」的最小集**：**#1 + #2** 立刻可做（消除「Esc 是否有反应」的困惑，零风险）；
**#3–#9** 是取消链路的主体；**#10** 补最坏等待的短板。

## 风险与规避

| 风险 | 规避 |
|---|---|
| `Arc<dyn BoundarySource>` 要求 `'static`，而 `ctx.boundary` 是借用 | 决策 4 已把类型改为 `Arc`；借用只在 `as_deref()` 处出现，且其生命周期短于持有期 |
| `ModelRequest` 的 `Clone` 派生因新字段失效 | `Arc<dyn ...>` 是 `Clone` ✓；`#[serde(skip)]` 不影响 `Clone` |
| 取消时 reqwest 请求未真正断开 | `select!` 丢弃 future ⇒ future drop ⇒ reqwest 取消请求。适配器内无 detached spawn（已核实）⇒ 无孤儿请求 |
| 取消的 `ModelError::Cancelled` 被误当 provider 故障 | 决策 3 在 `ys-loop` 显式分流 ⇒ `RunFinished{Cancelled}` 而非 `RunFailed` |
| `ModelError` 加变体是 breaking | 仓内无发布件；`Model` trait **签名不变** ⇒ 只有返回值变体变多，仓内穷尽匹配只有 1 处 |
| `ModelRequest` 序列化字节变化 | `#[serde(skip)]` ⇒ 既有序列化逐字节不变 ✓ |
| 工具之间加探针 ⇒ 已有测试的「工具调用次数」断言变化 | 探针只在**已 abort** 时提前收场；无 abort 的既有路径逐字节不变 ✓ |
| 新增 `ys-model → ys-protocol` 依赖造成环 | `ys-protocol` 只依赖 `ys-core` + `ys-event`；`ys-model` 已依赖 `ys-tool`，`ys-tool` 已依赖 `ys-protocol` ⇒ 无环 ✓ |

## 测试矩阵

| 用例 | 位置 | 断言 |
|---|---|---|
| Esc 投 Abort 且产生系统行 | `crates/ys-tui-coding/src/events.rs` | transcript 末行是 System「已请求取消…」 |
| Esc 不置 `abort_requested` | 同上 | `a.abort_requested == false`（Esc 非两段式） |
| `ModelRequest` 序列化不含 `boundary` | `crates/ys-model/src/request.rs` | serde_json 快照与既有逐字节一致 |
| `ModelRequest::clone` 保留同一 `Arc` | 同上 | `Arc::ptr_eq` |
| 取消 ⇒ `ModelError::Cancelled` | `adapters/model-openai-compatible/` | 边界已 abort 时调用 ⇒ 返回 `Cancelled` |
| 取消 ⇒ partial 携带已产生文本 | 同上 | 中止前吐过 delta ⇒ `Cancelled { partial: Some(..) }`，含该文本 |
| 取消 ⇒ partial 不含 tool_use | 同上 | 中止在 tool 参数途中 ⇒ partial 只含 `Text`，**无** `ToolUse` |
| 取消 ⇒ partial 追加进会话历史 | `crates/ys-loop/src/basic.rs` | 会话历史里**有**那段部分文本（**且在下一次请求的上下文里**） |
| 取消 ⇒ 回合以 `Cancelled` 收场 | `crates/ys-loop/src/basic.rs` | 收到 `RunFinished{stop_reason: Cancelled}`，**不发** `RunFailed` |
| 取消 ⇒ 不再发起下一次模型调用 | 同上 | mock model 的调用计数 == 1 |
| 工具之间 abort ⇒ 不再发起下一个工具 | 同上 Step 9 | 工具调用计数 < 计划数 |
| 既有路径逐字节不变 | 全仓既有测试 | `cargo test --workspace` 全绿 |

**变异测试要求**（AGENTS.md 约定）：至少 4 处
—— ① 把适配器的 `select!` 去掉 ⇒ 「取消 ⇒ `Cancelled`」用例红；
② 把 `ys-loop` 的 `Cancelled` 分流去掉 ⇒ 「不发 `RunFailed`」用例红；
③ 把 Esc 的系统行去掉 ⇒ 「Esc 产生系统行」用例红；
④ 把「先 append partial 再收场」的顺序反了 ⇒ 「partial 追加进会话历史」用例红。

### 变异实测（2026-10-21）

共 12 处变异，**每处都对应至少一个变红的用例**。

脚本：`scripts/cancellation-mutation.py`（**一次跑完全部 12 处**，跑完给汇总表）。
用法：`python3 scripts/cancellation-mutation.py`（全部）/ `--list`（只列清单）/ `1 4 7`（只跑指定编号）。
每个 mutant 跑**包级**测试（`cargo test -p <pkg>`）而非单个具名用例 ⇒ 覆盖更强，且能报出「哪些用例抓住了它」。

| # | 注入的 bug | 变红的用例 |
|---|---|---|
| ① | 适配器**发送阶段**的 `select!` 去掉 | `already_aborted_boundary_sends_no_request` 等 2 个 |
| ② | 适配器**流式阶段**的中止分支去掉 | `cancelled_after_delta_carries_partial_text` 等 2 个 |
| ③ | `into_partial` 恒返回 `None` | `cancelled_after_delta_carries_partial_text` 等 2 个 |
| ④ | `into_partial` 多吐 `ToolUse`（带出未完成的工具调用） | `cancelled_with_partial_tool_call_carries_no_tool_use` |
| ⑤ | 退避退回裸 `sleep`（退避期间不可取消） | `cancellation_during_backoff_returns_promptly` |
| ⑥ | `ys-loop` 的 `Cancelled` 分流去掉 | `model_cancelled_finishes_with_cancelled_and_never_run_failed` 等 4 个 |
| ⑦ | 「先 append partial 再收场」的顺序颠倒 | `model_cancelled_appends_partial_before_run_finished` |
| ⑧ | Esc 的系统行去掉 | `test_esc_while_turning_appends_system_line` |
| ⑨ | Esc 置 `abort_requested`（破坏「Esc 非两段式」） | `test_esc_does_not_set_abort_requested` |
| ⑩ | 工具之间的 abort 探针去掉 | `abort_between_tools_stops_the_next_tool` |
| ⑪ | `boundary` 参与序列化 | `model_request_serialization_ignores_boundary` |
| ⑫ | `ModelRequest::clone` 产生独立边界副本 | `model_request_clone_keeps_same_boundary_arc` |

**⑦ 的发现**：原 `model_cancelled_partial_is_appended_to_session` 只断言**最终状态**
（partial 进了会话），**不区分顺序** ⇒ 顺序颠倒后仍绿。故补
`model_cancelled_appends_partial_before_run_finished`（共享顺序日志同时记录
`append` 与 `RunFinished`）才把这条契约钉住。

## 不做什么

- **不改 `Model` trait 的方法签名**（决策 9）。
- **不引入 tokio `CancellationToken`**（决策 10）。
- **不改 `ModelEventSink` 的同步 `fn`**（硬约束）。
- **不新增环境变量 / 配置项**（上限由既有 `model_timeout.idle_secs` 决定）。
- **不碰轮数优化**（那是 `docs/arch/explore-speed` 的独立议题：轮数方差原因未查清，见 §11）。
- **不给 6 个短时工具加轮询**（决策 7：轮询粒度放在 loop 侧的工具之间）。

## Considered Options

### 选项 A：给 `Model::complete` 加 `Option<Arc<dyn CancelSource>>` 入参

- **优点**：显式，模型层不依赖 `ModelRequest` 的语义。
- **缺点**：改 `Model` trait 方法签名 ⇒ 改适配器、`MockModel`、所有测试替身；
  `Model` 已是 trait（`Send + Sync`），再加一个 trait object 入参让「跨动态库边界不传 trait object」的约束更紧；
  `complete` 已有 `&mut dyn ModelEventSink`，两个 trait object 入参并列。
- **不采纳**：挂进 `ModelRequest` 达到同一效果且 trait 零改动。

### 选项 B：给 `ModelError` 加 `Cancelled` vs 复用 `Provider(String)`

- **优点（复用）**：零 breaking。
- **缺点**：`ys-loop` 无法区分「用户取消」与「provider 故障」⇒ 用户取消会显示成 `RunFailed`（红色失败），
  与「主动取消」的用户预期矛盾。ADR-0028 已把 `Provider` 定位为「provider 方言/故障」的通道。
- **不采纳**：加变体。仓内穷尽匹配只有 1 处（`basic.rs:365`），可控。

### 选项 C：给 6 个短时工具加 100ms 轮询

- **优点**：与 `bash` / `exec` 形态统一。
- **缺点**：毫秒级操作上引入 100ms 粒度的取消感知 + 常驻定时器，纯开销；
  且这些工具**本身不阻塞**（真正的等待在 loop 的 await 点）。
- **不采纳**：探针放 loop 侧（决策 7）。

### 选项 D：只做决策 6（Esc 反馈），不改取消链路

- **优点**：爆炸半径 1 文件 3 行，零风险。
- **缺点**：用户知道「已请求取消」，但仍要等 150s。
- **部分采纳**：作为**第一批**单独落地（见「变更清单」的最小集）。

## 与其它文档的关系

- **ADR-0028** 决策 4 末条（「`Model::complete` 没有取消入参 ⇒ 退避期间 Ctrl+C 无效」）——
  本 ADR **正面解决**该缺口（退避的 `sleep` 同样在 `select!` 内 ⇒ 可被打断）。
- **ADR-0031**（Ctrl-C 两段式）—— 保持不变；本 ADR 只给 Esc 补反馈，不动两段式语义。
- **`docs/arch/explore-speed/cost-model.md`** —— 提供了 ttft / 轮数 / 长尾的实测依据；
  **轮数优化不在本 ADR 范围**（§11 记载轮数方差原因未查清）。
