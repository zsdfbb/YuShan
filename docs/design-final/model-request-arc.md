# 请求构造的共享快照 — As-built

> 性质：**实际建成记录**。决策见 `docs/adr/0071-model-request-shared-arc.md`，
> 设计计划 `docs/design-plans/completed/2026-11-05-model-request-arc.md`（已归档），
> 报告 `docs/reports/2026-11-05-model-request-arc-report.md`。
> 日期：2026-11-05 ｜ 提交 `34ac115`（去 serde 派生）、`e6ddb99`（改共享 `Arc`）。

**已实现**。本文只描述**当前**形态：`ModelRequest` 的 `messages` / `tools` 是 `Arc<Vec<_>>`，
由 `Session::shared_messages()` 与 `ToolRegistry::shared_specs()` 交出；
`ModelRequest` 不派生 serde。跨线格式另见适配器内部的 `ChatCompletionRequest`（不在本文范围）。

## 一句话

模型请求的**消息历史**与**工具表**都改为共享 `Arc`，`ModelRequest::clone()` 从「深拷贝整个历史」
退化为两次引用计数递增；`Arc::make_mut` 让会话写路径在快照释放后仍能**原地**追加。

## 核心形态

```rust
// crates/ys-model/src/request.rs:18-33
#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    /// 消息历史快照。
    pub messages: Arc<Vec<Message>>,
    /// 工具 schema 表。
    pub tools: Arc<Vec<ToolSpec>>,
    // … system / max_tokens / temperature / thinking / boundary
}
```

**为什么是 `Arc` 而不是 `Vec`**：`ModelRequest` 的 `Clone` 是**派生实现**，而重试循环每轮都 clone 一次：

```rust
// crates/ys-loop/src/basic.rs:830
match ctx.model.complete(request.clone(), &mut forwarder).await { ... }
```

`Model::complete` 取**所有权**，重试循环要复用同一份 request，故 clone 不可避免。字段是 `Vec<Message>` 时，
这一次 clone 就是整个历史深拷贝 —— 每回合都付的 O(n)。改成 `Arc` 后它是 O(1)，
且顺带消掉了 `basic.rs:706` 那次从 `&[Message]` 现拷一份的必要性。

默认 `stream_interrupt_retries = 0`，故常态是**每轮 1 次** clone；开启重试则每次尝试再 1 次。

**读侧完全不受影响**：`Arc<Vec<T>>` 的 deref 链是 `Arc<Vec<T>> → Vec<T> → [T]`，
`&request.messages`、`request.messages.iter()`、`.len()`、`.last()`、`&request.messages[..k]`、`[i]`
全部照旧可用。适配器因此只需在 `translate_request`（`adapters/model-openai-compatible/src/lib.rs:1251`）
把迭代写法收敛为 `for msg in request.messages.iter()` —— 迭代形态的差异，语义相同。

构造侧则需要 `Arc::new(vec![...])`：生产路径两处（`basic.rs:803`、`compact.rs:355`）、
摘要路径一处（`summarize.rs:64`），其余分布在测试替身里。

## `ModelRequest` 不派生 serde（这是设计，不是遗漏）

`ModelRequest` **不派生** `Serialize` / `Deserialize`，这是设计而非遗漏：

- 它是**进程内调用契约**，不是线格式。从 `ys-loop` 构造、交给 `Model::complete`，生命周期不超出一个回合。
- 真正跨线的是适配器内部的 `ChatCompletionRequest`（那里才需要 serde）。
- 生产代码**零处**序列化或反序列化 `ModelRequest`（`grep` `from_str::<ModelRequest>` / `from_value` 无命中）。
  （测试也不再有：见下方「为什么没有对应的运行期测试」。）

**守护是编译期的，不是运行期的。** `boundary: Option<Arc<dyn BoundarySource>>`（`request.rs:56`）没有
`Serialize`，一旦给 `ModelRequest` 加回派生就**直接编译失败**。这比任何运行期断言都强 ——
「把 boundary 序列化出去」在类型层面不可表达。

**为什么没有对应的运行期测试**：写不出有约束的运行期断言。试过 `assert_not_serialize::<ModelRequest>()`
这种空函数体，加回 `Serialize` 它照样通过 —— 空函数体不算断言，删掉是正确做法。
契约由编译期与 `request.rs:8-17` 的文档注释承担；`ys-model/src/lib.rs:180-187` 的注释记录了
这一分工（N10 由类型层守护、N11 由 clone 共享语义守护）。

## `Session::shared_messages()`

```rust
// crates/ys-session/src/lib.rs:47-58
fn messages(&self) -> &[Message];

/// 取消息历史的共享快照。
fn shared_messages(&self) -> Arc<Vec<Message>> {
    Arc::new(self.messages().to_vec())
}
```

- **默认实现**是 `Arc::new(self.messages().to_vec())` —— 行为正确但不优化：每次调用现拷一份历史。
- 持有内部 `Arc` 的实现覆写为 O(1)：`JsonlSession`（`jsonl.rs:234`）与 `MemorySession`（`memory.rs:35`）
  都是 `Arc::clone(&self.messages)`。
- **为什么用默认实现而不是强制实现**：只加方法、不改签名 ⇒ 现有 `Session` 实现（含各 crate 的测试替身）
  **零改动即编译通过**；覆写是**可选优化**，不是强制负担。未覆写的实现拿到的是正确但每轮复制一次的行为。

**契约**：返回值必须是**某一时刻的完整历史**，调用方拿到后可自由持有，不受后续 `append` / `clear` 影响。
这是 `Arc` 快照的天然语义（旧快照指向旧缓冲区），但改实现时容易破坏，故写在 trait 文档里
（`lib.rs:54-55`）。

## `Arc::make_mut` 的原地修改前提

会话实现内部存 `Arc<Vec<Message>>`，写路径走 `Arc::make_mut`（`jsonl.rs:261`、`:297`；
`memory.rs:40`、`:45`），`replace_all` 则是整体赋值（`jsonl.rs:310`，不靠 `make_mut`）。
`make_mut` 只在 refcount 为 1 时**就地改**；有任何未释放的副本则整体复制。

回合内的真实时序：

```
轮 N:  shared_messages() → refcount=1（session 自己一份）
       ModelRequest { messages: Arc::clone } → refcount=2
       complete(request.clone()) 内又持一份 → refcount=3
       调用返回，request 释放 → refcount=1
轮 N:  append(assistant) ⇒ make_mut 时 refcount=1 ⇒ 就地改
```

**前提**：`complete()` 返回后 request 已释放。若某个 `Model` 实现长期持有 `request.messages` 的副本
（例如跨轮缓存），`make_mut` 会退化成每次全量复制 —— 行为仍正确，性能退回改前。

本仓所有 `Model` 实现都在返回前释放 request。这条**隐式契约由测试钉住**：
`jsonl_append_after_snapshot_release_is_in_place` 用 `Arc::strong_count`（快照存活时必须是 2，
没有隐藏副本）+ **堆缓冲区地址不变**（全量重建会换地址）双重判定，只看计数无法区分「原地」与「重建」。

## `ToolRegistry` 同样持 `Arc`

```rust
// crates/ys-tool/src/registry.rs:6-62
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    specs: Arc<Vec<ToolSpec>>,
}

/// 获取全部 tool spec（构建时缓存）
pub fn specs(&self) -> &[ToolSpec] { &self.specs }

/// 取 spec 表的共享快照，供 `ModelRequest.tools` 直接使用（O(1)）。
pub fn shared_specs(&self) -> Arc<Vec<ToolSpec>> { Arc::clone(&self.specs) }
```

- `specs()` 保持返回切片，**兼容原有全部调用**；新增 `shared_specs()` 供请求构造共享
  （`basic.rs:698` 是唯一调用点）。
- 实测：本仓 10 个工具，schema 合计 **9108 字节**，`to_vec` 单次约 **42 µs**。
- **为什么改它**：它是每轮请求的**固定开销**，且 `build()` 之后不再变化（`registry.rs:45` 之后无写入路径）——
  没有任何理由每轮深拷一份。比 `messages` 更简单：无追加需求，故不需要 `Arc::make_mut`。

## 已知边界

- **`compact.rs` / `summarize.rs` 仍是新分配**：`compact.rs:355` 的 `Arc::new(request_messages)` 来自
  `messages.to_vec()`（`:347`）再 push 一条摘要指令；`summarize.rs:64` 是单条消息。两者都在罕见路径
  （压缩 / 工具结果摘要），且**本来就要追加尾巴** ⇒ 复制是必要的，不是遗漏。
- **`translate_request` 仍是逐块 `push_str`**：见下方「被否决的方案」，这是假优化，不做。
- **未覆写 `Session` 的实现每轮仍复制一次**：这是有意保留的兼容路径（默认实现），不是待办。
- **`Arc<Vec<T>>` 的 deref 在 `T: ?Sized` 之类泛型上下文里可能不成立**：本仓未遇到，全为具体类型。
- **`Arc::make_mut` 的原地性是性能前提而非正确性前提**：退化时只是变慢，语义不变。

## 被否决的方案

| 方案 | 否决理由 |
|---|---|
| `translate_request` 单块快速路径（单块 `Text` 时直接 `clone` 不拼 `String`） | **假优化**。`push_str` 到空 `String`（`adapters/.../lib.rs:1259`、`:1264`）与 `String::clone` 都是「一次分配 + 一次拷贝」；`ChatMessage.content` 是 `Option<String>`（所有权类型），无论如何都要拥有一个 `String`。要真省掉得改 `Cow<'a, str>`，而 `ChatCompletionRequest` 是序列化后立即丢弃的临时结构，引入生命周期得不偿失 |
| `ChatMessage.content` 改 `Cow<'a, str>` | 同上：临时结构，生命周期收益不抵成本 |
| 启用 `serde/rc` feature 保留 `ModelRequest` 的派生 | 派生无实际收益（生产零使用），却要引入依赖特征变更；且留着派生会持续挡住 `Arc` 方案 |
| `JsonlSession` 保留 `Vec<Message>` + 缓存 `Arc`（脏标记失效） | 多一个状态机要维护；而上面的时序已证明 `Arc` 独占在真实路径上成立 |
| `messages: Cow<'a, [Message]>` + 生命周期 | 穿透 `Model` trait 签名，`dyn Model` 下生命周期易踩坑（见 ADR-0071） |

## 测试守护网

| 用例 | 位置 | 钉死的契约 | 变异 |
|---|---|---|---|
| `model_request_clone_shares_messages_arc` | `crates/ys-model/src/lib.rs:215` | clone 后 `messages` 是**同一份** `Arc`（`Arc::ptr_eq`） | `messages` 改回 `Vec<Message>` ⇒ 编译失败（构造点类型不符） |
| `model_request_clone_keeps_same_boundary_arc` | `crates/ys-model/src/lib.rs:194` | clone 后 `boundary` 仍是同一份 `Arc`（取消能生效的前提） | 改成深拷贝 ⇒ 红 |
| `memory_session_shared_messages_is_stable` | `crates/ys-session/src/lib.rs:310` | ① 两次 `shared_messages()` 返回**同一份** `Arc`；② 且它指向 `messages()` 看到的那块内存 | 删掉 `MemorySession` 的覆写、退回默认实现 ⇒ 两条断言同时红 |
| `jsonl_append_after_snapshot_release_is_in_place` | `crates/ys-session/src/jsonl.rs:1425` | ① 快照存活时 `strong_count == 2`（无隐藏副本）；② 快照释放后 `append` 完**堆缓冲区地址不变** | `append` 改成 `self.messages = Arc::new(...)` 全量重建 ⇒ ② 红 |
| `turn_request_reuses_session_snapshot` | `crates/ys-loop/src/basic.rs:6736` | 主路径请求携带的 `messages` 与本次回合 `shared_messages()` 交出过的某一份 `Arc::ptr_eq` | 把 `basic.rs:706` 换回 `Arc::new(ctx.session.messages().to_vec())` ⇒ 红 |

**`tools` 没有独立用例**：`request.clone()` 对 `tools` 的 O(1) 由同一份派生 `Clone` 保证，
变异（`tools` 改回 `Vec<ToolSpec>`）表现为**编译失败**而非测试红 —— 编译期守护比运行期断言更强，
不必为对称性加一条空转的用例。`basic.rs:698` 走 `shared_specs()` 无运行期断言，
理由是它的正确性由类型（`ModelRequest.tools` 是 `Arc`）与编译期唯一构造点固定。
