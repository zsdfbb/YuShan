# 模型请求改共享 `Arc`：实测报告

- **日期**：2026-11-05
- **性质**：**实现与验证记录**。as-built 见
  [`docs/design-final/model-request-arc.md`](../design-final/model-request-arc.md)，
  决策见 [`docs/adr/0071-model-request-shared-arc.md`](../adr/0071-model-request-shared-arc.md)。
- **被测**：`ModelRequest` 的 `messages` / `tools` 字段、`Session::shared_messages()`、
  `ToolRegistry::shared_specs()`，以及 `basic.rs` 的请求构造路径。
- **提交**：`34ac115`（去 serde 派生）、`e6ddb99`（改共享 `Arc`），均已合入 master。

---

## 1. 概要

动机不是 `to_vec()` 本身，而是 **`crates/ys-loop/src/basic.rs:830` 的 `request.clone()`**。
`ModelRequest` 的 `Clone` 是派生实现，而 `complete()` 取所有权、重试循环必须复用 ⇒ 每轮都要 clone。
字段是 `Vec<Message>` 时，这一次 clone 就是整个历史深拷贝 —— 每回合都付的 O(n)。

改动：`messages` / `tools` 改 `Arc<Vec<_>>`；`Session` 加 `shared_messages()`（带默认实现，
`JsonlSession` / `MemorySession` 覆写为 `Arc::clone`，写路径走 `Arc::make_mut`）；
`ToolRegistry` 内部持 `Arc<Vec<ToolSpec>>` 并新增 `shared_specs()`；`ModelRequest` 去掉
`Serialize` / `Deserialize` 派生（`Arc` 需要 serde 的 `rc` feature，而该派生无实际收益）。

两个提交合计改动 **11 个文件，+343 / −71**（含设计文档 1 个）。

## 2. 改动清单

### `34ac115` refactor(model): ModelRequest 不再派生 Serialize/Deserialize

| 文件 | 行数 | 改动 |
|---|---|---|
| `crates/ys-model/src/request.rs` | +10 / −12 | 删 `Serialize, Deserialize` 派生；删结构体内 5 处 `#[serde]` 属性（`system` / `max_tokens` / `temperature` 的 `default`，`thinking` / `boundary` 的 `skip`）；`messages` / `tools` 注释改写为当前设计 |
| `crates/ys-model/src/lib.rs` | +7 / −31 | 删掉「挂载 `boundary` 不改变序列化字节」的序列化用例，改为**说明性注释**（N10 由类型层守护，见 §7）；保留 N11 的 clone 共享语义用例 |
| `docs/design-plans/2026-11-05-model-request-arc.md` | +222 / −0 | 设计计划（新） |

### `e6ddb99` perf(model): 请求的消息与工具表改共享 Arc

| 文件 | 行数 | 改动 |
|---|---|---|
| `crates/ys-model/src/request.rs` | +14 / −2 | `messages: Arc<Vec<Message>>`、`tools: Arc<Vec<ToolSpec>>` + 字段文档 |
| `crates/ys-model/src/lib.rs` | +32 / −12 | 测试构造点改 `Arc::new(...)`；新增 `model_request_clone_shares_messages_arc`（`:215`） |
| `crates/ys-session/src/lib.rs` | +34 / −0 | `Session::shared_messages()` 默认实现（`:47-58`）+ `memory_session_shared_messages_is_stable`（`:310`） |
| `crates/ys-session/src/memory.rs` | +13 / −4 | 内部存 `Arc<Vec<Message>>`；覆写 `shared_messages`；`append` / `clear` 走 `Arc::make_mut` |
| `crates/ys-session/src/jsonl.rs` | +55 / −6 | 同上（`messages` `:19`、`shared_messages` `:234`、`append` 的 `make_mut` `:261`、`clear` 的 `make_mut` `:297`、`replace_all` 赋值 `:310`）；新增 `jsonl_append_after_snapshot_release_is_in_place`（`:1425`） |
| `crates/ys-tool/src/registry.rs` | +16 / −2 | `specs: Arc<Vec<ToolSpec>>`、`specs()` 返回切片、新增 `shared_specs()` |
| `crates/ys-loop/src/basic.rs` | +124 / −7 | `:698` 改 `shared_specs()`、`:706` 改 `shared_messages()`；新增快照记录器 `SnapshotRecordingSession` / `SnapshotRecordingModel` 与 `turn_request_reuses_session_snapshot`（`:6736`） |
| `crates/ys-loop/src/compact.rs` | +3 / −2 | `messages: Arc::new(request_messages)`（调用点构造） |
| `crates/ys-loop/src/summarize.rs` | +5 / −3 | `Arc::new(vec![...])` / `Arc::new(vec![])` |
| `adapters/model-openai-compatible/src/lib.rs` | +16 / −16 | `translate_request` 的 `for msg in &request.messages` → `request.messages.iter()`（`:1251`）；5 处测试构造 `Arc::new` |
| `docs/design-plans/2026-11-05-model-request-arc.md` | +31 / −17 | 计划增补「被推翻的判断」与实测数据 |

## 3. 实测数据

| 项 | 值 |
|---|---|
| tools schema 总字节 | **9108 字节**（本仓 10 个工具） |
| `specs().to_vec()` 单次 | **约 42 µs**（1000 次累计 42.04 ms） |

**测量方法**：临时探针，在 `build_tools()` 之后对 spec 表 `serde_json::to_string` 量长度、
再循环 `to_vec()` 计时；测完探针已删除，仓库里没有残留代码。

这类数字只用于判断「值不值得改」：42 µs × 每轮 1 次 × 每回合若干轮，与 `messages` 的历史长度无关，
是**固定开销**。改后它是 `Arc::clone`，成本与工具数、schema 大小都无关。

## 4. 验证结果

| 项 | 结果 |
|---|---|
| `./scripts/par-test.sh` | **39 个测试二进制全绿** |
| workspace `--all-targets` 构建 | 0 error |
| `cargo clippy --all-targets` | 见下方说明 |

**clippy 说明**：收尾复查时 `clippy --all-targets` 报 1 条 `empty line after doc comment`
（`crates/ys-model/src/lib.rs:188`，删掉序列化用例后两条 doc 注释之间留下的空行）。
提交时该检查是 0 warning；这条是文档边界处的格式警告，不影响行为。

## 5. 变异验证（4 个变异点，实测结果）

| 变异 | 期望 | 实际 |
|---|---|---|
| 主路径 `shared_messages()` 改回 `Arc::new(ctx.session.messages().to_vec())` | `turn_request_reuses_session_snapshot` 红 | FAILED ✅ |
| `MemorySession::shared_messages` 退回 trait 默认实现 | `memory_session_shared_messages_is_stable` 红 | FAILED ✅（41 passed, 1 failed） |
| `messages` 改回 `Vec<Message>` | 相关测试红 | **编译失败**（构造点类型不符）✅ |
| `tools` 改回 `Vec<ToolSpec>` | 相关测试红 | **编译失败** ✅ |

后两条是编译期变红而非测试红 —— 这比运行期断言更强，也是没有为 `tools` 单写一条用例的理由：
`ModelRequest.tools` 是 `Arc`，任何 `Vec` 形态的变异都在构造点就被编译器挡住。

第 2 条的 `41 passed, 1 failed` 同时说明该变异的**隔离性**：只有一个用例红，其余全绿，
即这条用例确实单独承担了那份契约，不是「顺带红一片」（若两条断言同在一个用例里，合起来仍算 1 failed）。

## 6. 两个被推翻的原计划判断

### (a) `basic.rs:1481` 不在请求路径上

原计划把它列为「A-1b：裁剪路径的 `to_vec()`」，暗示它也要做零拷贝。实际是：

```rust
// crates/ys-loop/src/basic.rs:1482-1489
let mut messages = ctx.session.messages().to_vec();
for &idx in &plan { /* 降级该条工具结果 */ }
ctx.session.replace_all(messages).await?;   // ← 写回会话
```

它是**会话修改**（工具结果降级后的写回，`max_turn_tool_result_tokens` 分支内），
后面紧跟 `replace_all`，不在请求路径上，与 `ModelRequest` 无关。原计划 §4.1b 的判断是错的，
本次不做改动。

### (b) `translate_request` 的单块快速路径是假优化

原计划要在 `adapters/model-openai-compatible/src/lib.rs` 加「单块且是 `Text` 时直接 `clone`」。
实施前实测后**否决**：

```rust
let mut text_content = String::new();   // :1259
text_content.push_str(text);            // :1264 —— 1 次分配 + 1 次拷贝
```
对比
```rust
let text_content = text.clone();        // 1 次分配 + 1 次拷贝，没有更省
```

两者代价相同：`ChatMessage.content` 是 `Option<String>`（所有权类型），无论如何都要拥有一个 `String`。
要真省掉得改 `Cow<'a, str>`，而 `ChatCompletionRequest` 是序列化后立即丢弃的临时结构，
引入生命周期得不偿失。故该项从设计里删除，只在文档里留记录防止重提。

## 7. 过程中的自我修正

W0 阶段（拆出 serde 派生前）曾把「挂载 `boundary` 不改变序列化字节」那条用例改写成：

```rust
fn assert_not_serialize<T>() {}   // 空函数体
assert_not_serialize::<ModelRequest>();
```

**实测发现它抓不到任何问题**：给 `ModelRequest` 加回 `Serialize` 后这个用例照样通过 ——
空函数体不含任何断言，等于没写。已删除该函数与调用，改为在 `crates/ys-model/src/lib.rs:180-187`
写说明性注释，指明守护在**类型层**：`boundary: Option<Arc<dyn BoundarySource>>` 无 `Serialize`，
加回派生会直接编译失败。

这也是 as-built 文档里专门写明「为什么没有对应的运行期测试」的原因 —— 否则后人会以为漏了一条。

## 8. 已知边界与未完成项

- **`translate_request` 的 `Cow` 优化**：明确不做（假优化，见 §6b）。
- **`compact.rs:355` / `summarize.rs:64` 的复制**：**保留**。两者都在罕见路径（压缩 / 工具结果摘要），
  且本来就要追加尾巴，复制是必要的。
- **未覆写 `Session` 的实现每轮仍复制一次**：默认实现是有意保留的兼容路径。
- **`Arc::make_mut` 的原地性依赖时序**：请求必须在写回前释放。由
  `jsonl_append_after_snapshot_release_is_in_place` 钉住（`Arc::strong_count` + 堆缓冲区地址不变）；
  退化的后果是变慢，不是出错。
- **读路径未动**：`/resume` 仍全量读 + 逐行解析，本次只动请求构造与会话内存态。
