# ADR-0071: 模型请求的消息与工具表改共享 `Arc`

## 状态

已接受（2026-11-05）

## 上下文

`Model::complete` 取 `ModelRequest` 的**所有权**，而 `ys-loop` 的流中断重试循环必须复用同一份请求
（`crates/ys-loop/src/basic.rs:830` 的 `request.clone()`）。`ModelRequest` 的 `Clone` 是派生实现 ⇒
字段是 `Vec<Message>` 时，这一次 clone 就是整个消息历史深拷贝。默认
`stream_interrupt_retries = 0`，故常态是每轮 1 次；开重试则每次尝试再 1 次。这是每回合都付的 O(n)，
与 vision（图片进会话，单条消息显著变大）的方向直接冲突。

`tools` 是另一笔**固定**开销：本仓 10 个工具，schema 合计 9108 字节，`specs().to_vec()` 单次约 42 µs
（1000 次累计 42.04 ms），每轮都付，而循环内从不修改它。

仅改 `ModelRequest` 不够：`Session::messages()` 返回 `&[Message]`，从它构造 `Arc<Vec<Message>>`
仍需一次 `to_vec()` —— 拷贝只是换了个位置。

## 决策

1. `ModelRequest.messages` / `.tools` 改为 `Arc<Vec<Message>>` / `Arc<Vec<ToolSpec>>`；
   `clone()` 退化为引用计数递增，O(n) → O(1)。
2. `Session` trait 新增 `shared_messages(&self) -> Arc<Vec<Message>>`，**带默认实现**
   `Arc::new(self.messages().to_vec())`；`JsonlSession` / `MemorySession` 覆写为 `Arc::clone`，
   内部存储改 `Arc<Vec<Message>>`，写路径（`append` / `clear`）走 `Arc::make_mut`。
3. `ToolRegistry` 内部持 `Arc<Vec<ToolSpec>>`：`specs()` 仍返回切片（兼容全部既有调用），
   新增 `shared_specs()` 返回 `Arc::clone`。
4. `ModelRequest` **不派生** `Serialize` / `Deserialize`。

读侧不受影响是第 1 条成立的前提：`Arc<Vec<T>>` 的 deref 链是 `Arc<Vec<T>> → Vec<T> → [T]`，
`&request.messages` / `.iter()` / `.len()` / `[i]` 全部照旧。

## 为什么用默认实现而非强制实现

现有的 `Session` 实现（含各 crate 的测试替身）**零改动即编译通过** —— 只加方法、不改任何签名。
覆写是**可选优化**：覆写了拿 O(1)，没覆写拿「正确但每轮复制一次」。若改成强制实现，
每个测试替身都要跟着改，而它们的历史长度通常只有个位数，改动的唯一产出是噪声。

代价是：未覆写的实现仍每轮复制一次。这是有意保留的兼容路径，不是遗漏。

## 为什么删 serde 派生

- 生产代码**零处**序列化或反序列化 `ModelRequest`，测试也没有。
- `ModelRequest` 是**进程内调用契约**，不是线格式：从 `ys-loop` 构造、交给 `Model::complete`，
  生命周期不超出一个回合。真正跨线的是适配器内部的 `ChatCompletionRequest`。
- 它**直接挡住**本 ADR 的方案：workspace 的 serde 未启用 `rc` feature，`Arc<Vec<_>>` 派生会编译失败。
  为一个无使用者的派生引入依赖特征变更不划算。
- 删掉后守护**转移到编译期**：`boundary: Option<Arc<dyn BoundarySource>>` 无 `Serialize`，
  一旦加回派生就编译失败 —— 比运行期断言强，且无法被绕开。

## 代价

- **`Arc::make_mut` 的原地性成为一条隐式契约**：只有当快照在写回前被释放，`make_mut` 才就地修改；
  否则退化为全量复制。本仓所有 `Model` 实现都在返回前释放请求。这条由
  `jsonl_append_after_snapshot_release_is_in_place` 用 `Arc::strong_count` + 堆缓冲区地址不变钉住。
  退化时语义仍正确，只是变慢。
- **未覆写的 `Session` 实现仍是每轮复制**：默认实现的既定行为。
- **请求构造点全部需要 `Arc::new(...)`**：纯机械改动，编译期暴露。
- **`compact.rs` / `summarize.rs` 仍新分配**：它们在罕见路径且本就要追加尾巴，复制是必要的。

## 备选方案与否决理由

| 方案 | 否决理由 |
|---|---|
| 保持 `Vec<Message>` | 每轮深拷贝整个历史；与 vision（图片让消息变大）冲突，且随会话长度线性恶化 |
| `Cow<'a, [Message]>` + 生命周期（`ModelRequest<'a>`） | 穿透 `Model` trait 签名，`dyn Model` 下生命周期易踩坑，改动面远大于收益 |
| 给 `complete` 加 `messages: &[Message]` 参数 | trait 改动最大（所有适配器 + mock + 测试全改），收益不比 `Arc` 多 —— 重试循环仍需构造请求 |
| 启用 `serde/rc` feature 保留派生 | 为无用的派生引入依赖变更；派生本身零收益 |
| `JsonlSession` 保留 `Vec` + 缓存 `Arc`（脏标记失效） | 多一个状态机要维护；真实时序下 `Arc` 独占已成立 |

## 后果

**正面**：

- 请求构造的拷贝从 O(n) 降到 O(1)，与历史长度、工具表大小无关
- `Session::shared_messages()` 给了「交出快照」一个明确的 trait 位置，覆写即优化
- `ModelRequest` 的边界更清晰：进程内契约不派生 serde，类型层排除「把取消通道序列化出去」

**负面 / 新增路径**：

- 新增一条时序依赖：快照必须在写回前释放，否则 `make_mut` 复制
- 新增两个可选覆写点（`shared_messages` / `shared_specs`），不覆写则静默退回复制行为
- `ModelRequest` 的构造点全需 `Arc::new`；改字段类型的成本一次性支付

## 关联

- [model-request-arc.md](../design-final/model-request-arc.md) — **当前形态以它为准**
- [2026-11-05-model-request-arc-report.md](../reports/2026-11-05-model-request-arc-report.md) — 实测数据与变异验证
- [design-plans/completed/2026-11-05-model-request-arc.md](../design-plans/completed/2026-11-05-model-request-arc.md) — 设计计划
- ADR-0070 — 会话写入的 append-only 路径（同一 `JsonlSession` 的另一条性能路径）
