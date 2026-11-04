# A/B/C 三件事的计划（视觉输入的前置修缮）

> 背景：为 `docs/arch/vision-input/design.md` 做准备时发现，本仓有两处架构债，使得
> 「图片字节放哪」这个选择被人为放大。参照实现是 OpenAI Codex（源码在 `tmp/codex/`），
> 它在两处都用了正确形态。本计划先修债，再做 vision 功能。
>
> **A/B/C 都是与 vision 无关的独立收益**，但因为它们是 vision 的前提，所以先做。

## 0. 三件事与 Codex 的对应

| | 本仓现状 | Codex 做法 | 收益 |
|---|---|---|---|
| **A** 请求零拷贝 | `basic.rs:705` `ctx.session.messages().to_vec()` —— 每轮深拷贝整个会话 | `history.rs:106` `items: Arc<Vec<..>>`；`:338` 取快照只 `Arc::clone` | 每轮省一次 O(会话) memcpy |
| **B** 会话 append-only | `jsonl.rs:167` `append` → `flush_to_file` **全量重写** + rename | `recorder.rs:1754` `OpenOptions::new().append(true)` | append 从 O(会话) 降到 O(1) |
| **C** base64 进会话 | 无图片能力 | `snapshot_local_user_input` 原地替换成 `Image{Inline{image_url}}` | 获得可复现性，无需旁路文件 |

**关键依赖：C 的可接受性依赖 B。** 不做 B 就做 C，会出现「加图后每次 append 变慢」——
那是 B 的债，不是 C 的错。

## 1. 已核实的约束（决定方案形状）

| 事实 | 位置 | 对方案的影响 |
|---|---|---|
| `Session::messages() -> &[Message]`，`Message: Clone`（非 `Copy`） | `crates/ys-session/src/lib.rs:46` | **B 的根因**：返回 `&[Message]` 逼调用方为了 append 而 clone |
| `ModelRequest.messages: Vec<Message>` 且 `derive(Clone)` | `crates/ys-model/src/request.rs:11` | A 要在「改公开类型」与「保持 trait 不变」间选 |
| **全仓无 `from_str::<ModelRequest>`** | 已 grep 确认 | A 可以把 `messages` 换成非反序列化友好的类型，代价低 |
| `Model` trait 是**静态分发**（`async fn complete(&self, request: ModelRequest, ..)`） | `crates/ys-model/src/trait_def.rs:14-18` | 泛型化 `complete` 会让 `dyn Model` 不可用 ⇒ **不走泛型路** |
| 所有 serde 均无 `rc` feature | 各 `Cargo.toml` | 若要 `Arc<str>` 参与 serde 需加 feature；`Arc<Vec<_>>` 自带 impl，无需 |
| `translate_request` 把所有 `Text` 块**拼进新 String** | `lib.rs:1251-1280` | **A 的第二处拷贝**，代码位置在适配器侧，不属 loop |
| `flush_to_file` 是 meta 写入的**唯一落点** | `jsonl.rs:117-118` 注释 | B 改 append 时必须另立 meta 的落点，否则「meta 永不丢」不变量被破 |
| `replace_all` 必须原子且单次落盘 | `jsonl.rs:177-186` + `flush_count` 测试 | B 要保留这条能力（压缩依赖它） |

## 2. 总体顺序与理由

```
Phase 1 ─ B（append-only 落盘）        最独立，且有现成测试可复用（flush_count）
   ↓
Phase 2 ─ A（零拷贝请求）              纯内存，可与 B 并行开发，但合入顺序放后面
   ↓                                  （A 会让 basic.rs 的 3 处构造点全变，
   ↓                                   先稳定 B 的写路径能减少同时改动的面积）
Phase 3 ─ C（vision 功能，base64）     消费前两个阶段的成果
```

**为什么 B 先做**：它改的是持久化语义，牵涉恢复、meta、`replace_all` 三条不变量，
是最容易出错的一块；独立做、单独验收，比和 vision 混在一起安全。

**为什么 A 第二**：A 改 `basic.rs` 的消息构造路径，而 C 也要在那里加 Image 块折叠。
先做 A，C 就只需在「稳定的新形态」上加一臂，不必同时改结构又加功能。

## 3. Phase 1 — B：会话 append-only 落盘

> **状态：已完成并合入 master**（`6e2e259`）。实际形态见
> `docs/design-final/session-append-only.md`；本节保留为计划当时的形态，偏差如下：
> - `meta_dirty` 字段**未采用** —— `set_meta` 是同步全量落盘，该字段只会成为 `dead_code`
> - `SessionRecovery.skipped_lines` 复用记录尾部残行，不新增字段
> - 新增 `truncate_partial_tail`，比原计划的 `ensure_newline_terminated` 更省（末字节已是 `\n` 时只读 1 字节）
> - 新增 `persisted` 错位兜底对齐 + `debug_assert!`

### 3.1 目标形态（计划稿）

```rust
// crates/ys-session/src/jsonl.rs
struct JsonlSession {
    path: PathBuf,
    meta: Option<SessionMeta>,
    messages: Vec<Message>,
    /// 已落盘的行数 —— append 的增量依据
    persisted: usize,
    /// meta 行是否需要在下次写入时重写（set_meta 置位）
    meta_dirty: bool,
    recovery: Option<SessionRecovery>,
}
```

- **`append()`**：只把 `messages[persisted..]` 序列化并 **`OpenOptions::append(true)`** 写入，
  然后 `persisted = messages.len()`。
- **`set_meta()`**：meta 在**首行**，无法追加 ⇒ 置 `meta_dirty`，下一次 `flush` 时走全量重写。
  （备选：把 meta 改成尾行「最新覆盖」语义 —— **不采用**，会破 `jsonl.rs:57-60` 的行判别契约。）
- **`replace_all()`**：仍是全量重写（它本来就是语义上的整体替换，压缩场景），
  但复用同一条 tmp+rename 原子路径。
- **`clear()`**：全量重写为空文件（保留 meta）。

### 3.2 必须守住的不变量

| 不变量 | 现有依据 | B 下如何保持 |
|---|---|---|
| **meta 永不因重写丢失** | `jsonl.rs:117-118` 注释 | meta 只经 `flush_to_file`（全量路径）写；append 路径**不碰** meta |
| **写入原子**（tmp + rename） | `jsonl.rs:112,141` | append 天然原子（O_APPEND 单行写）；replace_all 保持 tmp+rename |
| **`replace_all` 单次落盘** | `flush_count` 测试（`jsonl.rs:19-22`） | 不变；再补一条「append 不触发 flush_count」 |
| **崩溃后行完整** | rename 保证 | append 需保证**单次 write 一条完整行**（含 `\n`），不得分两次写 |

### 3.3 风险与缓解

| 风险 | 缓解 |
|---|---|
| O_APPEND 下**并发写**（同进程多 session 实例指向同一文件） | 现有设计不允许多实例（`JsonlSession` 独占）；在文档写明，不加锁 |
| append 期间进程崩溃，留下**半行** | 单次 `write_all` 整行；启动时 `ensure_newline_terminated` 式修复（Codex `recorder.rs:1761` 有同名逻辑，可参照） |
| `persisted` 与实际文件不一致（外部改文件） | 启动时以**读到的行数**初始化 `persisted`；不检测外部修改（现有语义也不检测） |
| 回放仍需**全量读** | 保留；B 只优化写，不优化读（读发生在 `/resume`，一次性） |

### 3.4 验收

- 单测：`append` 后文件**字节增量** == 新行字节；`flush_count` 不增。
- 单测：`set_meta` 后 meta 与消息都在；`replace_all` 后文件内容 == 新集合。
- 单测：模拟半行（手写截断文件）后 `open` 能修复/跳过并计入 `skipped_lines`。
- 变异：把 append 改回全量重写 ⇒ 「字节增量为 0」的断言变红。

## 4. Phase 2 — A：请求零拷贝

### 4.1 问题拆成三处

**A-1：loop 主请求的 `to_vec()`**（`basic.rs:705`）
**A-1b：轮预算裁剪路径的 `to_vec()`**（`basic.rs:1481`）
**A-2：适配器侧的 `translate_request` 拼 String**（`lib.rs:1251-1280`）

三处相对独立，**A-1 优先**（它在每条消息都走的路径上，且改动面小）。

### 4.1b A-1b 与 A-1 不能一刀切

`basic.rs:705` 是「前缀不变 + 末尾追加 0–1 条」；
`basic.rs:1481` 是**轮预算裁剪**，要逐条替换 `messages[idx] = elided` —— 它改的是**中间元素**。

后者无法靠 `Arc::clone` + 追加实现，**必须复制**（写时复制），除非引入更复杂的结构。
所以 A 阶段要分开处理：

| 路径 | 形态 | 方案 |
|---|---|---|
| `basic.rs:705` | 只追加尾巴 | `Arc::clone`，有提示时 `Arc::make_mut` |
| `basic.rs:1481` | 就地改中间元素 | **保留复制**；但只在触发裁剪时付（`plan.is_empty()` 为真时走零拷贝路径） |

结论：**零拷贝的责任只落在 A-1**。A-1b 不追求零拷贝 —— 它已经在罕见路径上（需触发裁剪），
把复制留着、但要确认它不连带把 A-1 的 Arc 拆开。

### 4.2 A-1 的方案选择

三个候选，我推荐 **S1**：

| | 做法 | 代价 | 评价 |
|---|---|---|---|
| S1 | `ModelRequest.messages: Arc<Vec<Message>>` | 追加提示时要 `Arc::make_mut` + 一次整体 clone（带提示时才付） | ✅ **推荐**：改动面最小；`ModelRequest` 已有 `Clone`，`Arc` 让 clone 变 O(1)；无提示的轮次**零拷贝** |
| S2 | `messages: Cow<'a, [Message]>` + `ModelRequest<'a>` | 引入生命周期，`Model` trait 签名要带 `'a` | ❌ 穿透面大，且 `dyn Model` 下生命周期易踩坑 |
| S3 | 给 `complete` 加 `messages: &[Message]` 参数 | trait 改动最大，所有适配器 + mock + 测试全改 | ❌ 收益不比 S1 多 |

**S1 光改 `ModelRequest` 不够**：`session.messages()` 返回 `&[Message]`，
从它构造 `Arc<Vec<Message>>` 仍需一次 `to_vec()`。所以 **`Session` trait 必须同时加
`shared_messages()`**（返回持有内部快照的 `Arc<Vec<Message>>`），否则 A-1 只是把拷贝换了个位置。

**S1 为何够**：真实形态是「前缀不变 + 末尾追加至多 1 条」（`final_report → narration →
 batch → soft_round` 是 if/else if 链，每轮最多一条）。无提示时 `Arc::clone` 即完成；
有提示时 clone 前缀（这是**必要的**，因为要拼尾巴）。而 S2/S3 省下的正是这一次
必要 clone —— 收益有限，代价是穿透式的类型改动。

### 4.3 A-2 的做法

`translate_request` 目前对每条消息新建 `String` 并拼所有 `Text` 块。
- **单块且是 `Text` 时直接复用**（不需要拼）：`ChatMessage.content` 已有「String 或块数组」
  的需求（vision 的 D-决策），一并在这次做掉。
- 多块时仍要拼 —— 但这是**极少见**情形（多数消息只有一块）。

### 4.4 验收

- 性能：`basic.rs:705` 的 `to_vec()` 消失；用一个「1000 条消息」的 bench 断言
  单轮构造耗时下降（或至少断言无 clone：测试用 `Arc::ptr_eq` 断言无提示时未复制）。
- 性能：`Session::shared_messages()` 连续两次返回同一 `Arc`（`Arc::ptr_eq` 为真）—— 钉死
  「快照不是每次重算」。
- 功能：`basic.rs:6522` 的记录器（`request.messages.clone()`）仍然工作。

## 5. Phase 3 — C：vision（只存路径）

采用 `docs/arch/vision-input/design.md` 的 **§3.7 现行形态**：

- `ContentBlock::Image { path, detail }` —— **会话里只有路径**
- 图片字节在**适配器出站时**现读、base64 内联进**请求**（data URL），不落会话
- 原图失效 ⇒ 文本占位（含原因），回合不失败

**不做的事**（已裁决，不得再引入）：

| 不做 | 理由 |
|---|---|
| 旁路快照目录（内容寻址 + GC） | 引入目录管理、引用计数、清理命令，而这些只为「原图删了还能重放」—— 用户已裁决不要 |
| 任何进会话的 base64（字段叫 `snapshot` / `data` 都一样） | 会把会话变成不可读、不可 diff 的二进制堆，且让压缩后的磁盘不缩 |
| `id` / 内容指纹字段 | 它只为旁路目录服务；只存路径时无意义 |

**C 依赖 B 与 A 的理由**：
- B：append 已是 O(1)，所以「图片会不会撑大写入代价」不再是约束（但仍不内联，理由见上：可读性）
- A：图片会让 `Message` 显著变大，`to_vec()` 的代价跟着变大

## 6. 每阶段的验收门槛

| 阶段 | 必过 |
|---|---|
| B | `./scripts/cargo-build.sh --tests` 绿；`./scripts/par-test.sh` 绿；新增变异点能变红 |
| A | 同上 + 无 `to_vec()` 残留（grep 断言） |
| C | 同上 + `design.md` §9 的测试矩阵；新增 `scripts/vision-mutation.py` |

## 7. 不做的事

- 不把 `Model` trait 泛型化 / 不改 `complete` 签名（S3 否决）。
- 不做旁路快照目录 + GC。
- 不做本地图像缩放/转码。
- B 阶段**不**顺手改会话读路径（`/resume` 仍全量读）。
- A-1b（`basic.rs:1481` 裁剪路径）**不**追求零拷贝 —— 它在罕见路径上，只要求不拆坏 A-1。
