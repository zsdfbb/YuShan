# A：请求构造零拷贝 —— 设计计划

> 结论先行：**核心收益不在 `to_vec()`，而在 `basic.rs:829` 的 `request.clone()`。**
> 每轮模型调用都深拷贝一次整个消息历史，这是每回合都付的 O(n) 拷贝。
> 原计划把它当成次要项，实测定位后它才是主要项。

## 1. 已核实的现状

### 1.1 生产路径上的拷贝点

| 位置 | 形态 | 频次 | 是否热路径 |
|---|---|---|---|
| `basic.rs:705` `ctx.session.messages().to_vec()` | 全量深拷贝 | 每模型轮 1 次 | ✅ |
| `basic.rs:829` `request.clone()` | **全量深拷贝** | **每模型轮 1 次**，流重试时再次 | ✅ **主要成本** |
| `adapters/.../lib.rs:1251` `translate_request` 拼 `String` | 逐块字符串拼接 | 每模型轮 1 次 | ✅ 次要 |
| `compact.rs:353` `messages.to_vec()` | 全量深拷贝 | 仅在压缩时 | ❌ 罕见 |
| `summarize.rs:61` 单条消息 | 1 条 | 仅在工具结果摘要时 | ❌ 罕见 |

### 1.2 三处关键事实（纠正原计划）

**事实 1 —— `basic.rs:1481` 不是请求路径。**
原计划把它列为「A-1b，裁剪路径的 `to_vec()`」，暗示它要在请求构造上做零拷贝。
实际是：

```rust
let mut messages = ctx.session.messages().to_vec();
for &idx in &plan { messages[idx] = elided; }
ctx.session.replace_all(messages).await?;   // ← 写回会话
```

它是**会话修改**（工具结果降级后的写回），发生在回合末、下一轮请求之前。
它与 `ModelRequest` 无关，**不在 A 的范围内**。原计划 §4.1b 的判断是错的。

**事实 2 —— `request.clone()` 才是主要成本。**
`Model::complete(&self, request: ModelRequest, ...)` 取**所有权**，而重试循环要复用：

```rust
let request = ModelRequest { messages: request_messages, .. };
let response = loop {
    match ctx.model.complete(request.clone(), &mut forwarder).await { ... }
};
```

`ModelRequest` 的 `Clone` 是派生实现 ⇒ 深拷贝 `Vec<Message>`。
默认 `stream_interrupt_retries = 0`（`ys-component/src/limits.rs:224`），所以常态是**每轮 1 次**；
开了重试则每次尝试再 1 次。

**如果把 `messages` 改成 `Arc<Vec<Message>>`，这一个 `clone()` 从 O(n) 变成 O(1) —— 收益是原来的数倍**（它同时消掉了 `basic.rs:705` 的 `to_vec()` 的必要性）。

**事实 3 —— 单改 `ModelRequest` 不够，`Session` trait 必须同时加方法。**
`Session::messages()` 返回 `&[Message]`。从 `&[Message]` 构造 `Arc<Vec<Message>>` 仍需 `to_vec()`。
故必须新增 `fn shared_messages(&self) -> Arc<Vec<Message>>`，由实现方持有内部快照。

### 1.3 serde 阻塞点（必须先解）

`ModelRequest` 派生了 `Serialize + Deserialize`（`crates/ys-model/src/request.rs:9`），
而 workspace 的 serde **未启用 `rc` feature**（`crates/ys-model/Cargo.toml:14`
仅 `features = ["derive"]`）。`Arc<Vec<Message>>` 直接编译失败。

核实结果：
- **生产代码零处反序列化 `ModelRequest`**（grep `from_str::<ModelRequest>` / `from_value` 均无命中）
- 唯一的 `Serialize` 用途是 `crates/ys-model/src/lib.rs:195` 的测试：
  「挂载 `boundary` 不改变序列化字节」

故 `Serde` 派生的实际收益接近零，而它挡住 A 的方案。两个选项：

| 选项 | 做法 | 代价 |
|---|---|---|
| **S-a** | 启用 `serde/rc` feature | 一次性依赖变更；`Arc` 会按值序列化（丢共享语义，但无人反序列化所以无影响） |
| **S-b** | 去掉 `Serialize/Deserialize`，测试改用别的方式钉住「boundary 不入线上」 | 删两个 derive；那条测试要改写成断言 `boundary` 字段不存在（可用 `..Default::default()` 构造对比） |

**推荐 S-b** —— 理由：`ModelRequest` 是**进程内**的调用契约，不是线格式（它从不跨进程，
真正跨进程的是适配器内部的 `ChatCompletionRequest`）。留着 `Serialize` 却没有任何生产者/消费者，
只会持续挡住后续优化。删掉后 serde 依赖也不必加 `rc`。

## 2. 方案

### 2.1 `ModelRequest.messages: Arc<Vec<Message>>`

```rust
#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    /// 消息历史快照。`Arc` 让 `clone()` 变 O(1) —— 重试循环每轮都 clone 一次。
    pub messages: Arc<Vec<Message>>,
    ...
}
```

读侧兼容：`Arc<Vec<T>>` 对 `&[T]` 的 deref 链是 `Arc<Vec<T>> → Vec<T> → [T]`，
故 `&request.messages`、`request.messages.iter()`、`request.messages.len()`、
`request.messages.last()`、`&request.messages[..k]` **全部无需改动**。

需要改的只有**构造侧**：`messages: vec![...]` → `messages: Arc::new(vec![...])`（约 20 处，
其中多数在测试里）。

### 2.2 `Session::shared_messages()` —— 带默认实现

```rust
/// 取消息历史的共享快照。
///
/// 默认实现 `Arc::new(self.messages().to_vec())` —— 不做额外优化，行为正确。
/// 持有内部 `Arc` 的实现应覆写为 O(1)。
fn shared_messages(&self) -> Arc<Vec<Message>> {
    Arc::new(self.messages().to_vec())
}
```

**用默认实现而不是强制实现**，理由：
- 只加方法不改签名，现有实现零改动即编译通过（`MemorySession`、测试替身都不用动）
- `JsonlSession` / `MemorySession` 覆写为 O(1)，是**可选优化**而非强制负担

### 2.3 `JsonlSession` 的形态选择

`JsonlSession` 需要能**就地修改**（`append` push、`clear` 清空、`replace_all` 赋值、
修复逻辑改中间元素）。持 `Arc<Vec<Message>>` 时"写"要做 `Arc::make_mut`，
而**只要有一个未释放的 `Arc` 副本，`make_mut` 就会整体复制**。

回合内的真实时序：

```
轮 N:  shared_messages() → Arc（refcount=1）
       ModelRequest { messages: Arc::clone } → refcount=2
       model.complete(request.clone()) 内持有 → refcount=3
       调用返回，request 释放 → refcount=1
轮 N:  session.append(assistant)  ⇒ make_mut 时 refcount=1 ⇒ 就地改 ✅
```

只要 `complete()` 返回后 request 已释放，`append` 就能就地改。这一条要在 T 阶段用测试钉死，
不能靠推理（见 §5 的 `append_after_request_dropped_does_not_copy`）。

风险：若有实现把 `request` 长期持有（例如跨轮缓存），`make_mut` 会退化成每次全量复制。
本仓无此形态（所有 `complete` 实现都在返回前释放 request）。

**备选（不采用）**：`JsonlSession` 保留 `Vec<Message>` + 缓存 `Arc`（脏标记失效）。
代价是多一个状态机要维护，而 §2.3 的时序已证明 `Arc` 独占在真实路径上成立。

### 2.4 `translate_request` 单块复用

现状（`adapters/model-openai-compatible/src/lib.rs:1251` 起）：每条消息循环拼 `String`，
即使只有 1 个 `Text` 块也要 `push_str` 到空 `String`。

改为：

```rust
// 单块且是 Text ⇒ 直接 clone，不走 String 拼接
let text_content = match msg.content.as_slice() {
    [ContentBlock::Text { text }] => Some(text.clone()),
    blocks => {
        // 多块仍要拼（极少见）
        ...
    }
};
```

注意：这仍是**一次 `String` clone**（`ChatMessage.content: Option<String>` 是所有权类型）。
要真正零拷贝得把 `ChatMessage.content` 改成 `Cow<'a, str>`，但 `ChatCompletionRequest`
是 `Serialize` 后立即消费的临时结构，改 `Cow` 只省一次 `String` 分配 —— **收益远小于代价，
不做**。（这与 `ModelRequest.messages` 的取舍不同：那是 `Vec<Message>` 的 O(n) 深拷贝。）

### 2.5 不在范围内

- `compact.rs:353` 的 `to_vec()` —— 压缩时一次性，且它要 push 尾巴，本来就得复制
- `summarize.rs:61` —— 单条消息
- `basic.rs:1481` —— 不是请求路径（见 §1.2 事实 1）
- `ChatMessage.content` 改 `Cow` —— 见 §2.4

## 3. 改动清单

| crate / 文件 | 改动 | 风险 |
|---|---|---|
| `crates/ys-model/src/request.rs` | `messages: Arc<Vec<Message>>`；删 `Serialize/Deserialize` 派生（§1.3 S-b） | ⚠️ 构造点全要 `Arc::new` |
| `crates/ys-model/src/lib.rs` | 5 处 `ModelRequest{}` 测试构造 + `:195` 的序列化测试改写 | — |
| `crates/ys-session/src/lib.rs` | `Session` trait 加 `shared_messages()` 默认实现 | 加方法不改签名 ⇒ 现有实现零改动 |
| `crates/ys-session/src/memory.rs` | 覆写 `shared_messages` 为 O(1)（需加 `Arc` 缓存字段或 `messages: Arc<Vec<Message>>`） | — |
| `crates/ys-session/src/jsonl.rs` | 同上；`messages: Arc<Vec<Message>>`，写路径走 `Arc::make_mut` | ⚠️ `persisted` 与 `make_mut` 的交互要测 |
| `crates/ys-loop/src/basic.rs:705,803` | 去掉 `to_vec()`，改 `shared_messages()` | ⚠️ 主路径 |
| `crates/ys-loop/src/basic.rs:829` | `request.clone()` 保留（现在已是 O(1)） | — |
| `crates/ys-loop/src/compact.rs:353` | `Arc::new(messages.to_vec())` | 不变 |
| `crates/ys-loop/src/summarize.rs:61` | `Arc::new(vec![...])` | 不变 |
| `adapters/model-openai-compatible/src/lib.rs:1251` | 单块 Text 快速路径 | 线上字节必须不变 |
| `adapters/.../lib.rs` 测试构造 (5 处) | `Arc::new` | — |
| `apps/coding-agent/src/*` 测试替身 (6 处 `Vec<ModelRequest>`) | `request.messages` 读取点按需 `.iter()` / `&**` | 编译期暴露 |

## 4. 验收

| 项 | 判据 |
|---|---|
| **无提示的轮次零拷贝** | `Arc::ptr_eq(&session.shared_messages(), &request.messages)` 为真 |
| **`shared_messages` 是快照不是重算** | 连续两次调用返回同一 `Arc`（`Arc::ptr_eq`） |
| **`request.clone()` 是 O(1)** | clone 后 `Arc::ptr_eq(request.messages, cloned.messages)` 为真 |
| **append 不因快照而复制** | 取出快照并释放后 `append`，再取快照仍 `Arc::ptr_eq` 前一次（说明未 `make_mut` 复制） |
| **线上字节不变** | 适配器 golden 基线逐字节相同 |
| **行为不变** | `./scripts/par-test.sh` 全绿 |

## 5. 测试矩阵与变异点

| 测试 | 位置 | 变异（去掉即应变红） |
|---|---|---|
| `request_clone_shares_messages_arc` | `ys-model` | 把 `messages` 改回 `Vec<Message>` ⇒ 红 |
| `session_shared_messages_is_stable` | `ys-session` | 让默认实现每次 `to_vec` ⇒ 红 |
| `loop_request_reuses_session_snapshot` | `ys-loop` | 恢复 `ctx.session.messages().to_vec()` ⇒ 红 |
| `append_after_request_dropped_does_not_copy` | `ys-session` | 持 `Arc` 不放时 `make_mut` 复制 ⇒ 红 |
| `translate_request_single_text_unchanged` | 适配器 | 改拼接逻辑写错字节 ⇒ 红 |
| 既有 golden | 适配器 | — |

## 6. 实施顺序

1. **解 serde 阻塞**：删 `ModelRequest` 的 `Serialize/Deserialize`，改写 `:195` 的测试
2. **`ModelRequest.messages: Arc<Vec<Message>>`** + 修全部构造点（编译期驱动的机械改动）
3. **`Session::shared_messages()` 默认实现** + `MemorySession` / `JsonlSession` 覆写
4. **`basic.rs:705` 改 `shared_messages()`**，`basic.rs:829` 无需改（自动 O(1)）
5. **`translate_request` 单块快速路径** + golden 验证
6. 补 §5 的 5 条测试 + 变异验证

## 7. 风险

| 风险 | 缓解 |
|---|---|
| `Arc::make_mut` 在某些路径上退化复制 | §5 的 `append_after_request_dropped_does_not_copy` 钉住；若真退化，退路是 `JsonlSession` 保留 `Vec` + 缓存 `Arc`（脏标记） |
| 删 `Serialize` 破坏某个未发现的调用点 | 编译期暴露（无生产反序列化点，已 grep 确认） |
| `Arc<Vec<T>>` 的 deref 在某个泛型上下文里不成立 | 编译期暴露；已确认读侧用法均可行 |
| 测试替身 `Vec<ModelRequest>` 大量读取 `.messages` | 编译期暴露，逐个加 `.iter()` / `&*` |
