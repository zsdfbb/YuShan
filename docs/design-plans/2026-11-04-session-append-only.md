# 会话 append-only 落盘 设计计划

> 日期：2026-11-04
> 性质：**持久化语义的形态改造**（不改 `Session` trait 签名、不改读路径）
> 上游背景：`docs/design-plans/vision-input-prereq-abc.md` 的 Phase 1「B」

## 背景

`JsonlSession` 的每一条消息写入都走**全量重写**：`append()` 把消息推进内存
（`crates/ys-session/src/jsonl.rs:167`），随后调用 `flush_to_file()`
（`crates/ys-session/src/jsonl.rs:108-145`）—— 后者 `File::create` 一个 `.jsonl.tmp`、
把 meta 行与**全部** messages 逐条重写、再 `tokio::fs::rename` 原子替换。

代价随会话长度线性增长，并乘在每条消息上：

| 写入第 k 条消息 | 当前落盘字节 |
|---|---|
| `append` | `O(前 k-1 条 + 本条)` |
| 一个 R 轮回合合计 | `O(R × 会话总字节)`，即 `O(n²)` 量级 |

会话越长，`append` 越慢；`replace_all`（压缩场景）本身为了避免退化已经覆写为单次落盘，
但常规的逐条 `append` 仍是全量。vision 功能（C，把图片 base64 放进会话）会把每行的字节
放大到 MB 级，把这一项从「听感上的慢」放大成「不可接受」—— 这是本单元先行落地的原因。

另有一处**注释与实现绑死**：`crates/ys-session/src/lib.rs:75-86` 的 `replace_all` 默认实现
文档写着「对『`append` 每次全量重写整个文件』的实现（如 `JsonlSession`），默认实现会把整体
替换退化成 O(n²)」。本单元完成后该前提不再成立 ⇒ 注释必须同步改写，否则文档在说谎。

## 设计

### 目标形态

```rust
// crates/ys-session/src/jsonl.rs
pub struct JsonlSession {
    path: PathBuf,
    /// 会话级元数据，落盘于文件**首行**；缺省即无首行 meta。
    meta: Option<SessionMeta>,
    messages: Vec<Message>,
    /// 已落盘的 messages 条数 —— append 的增量依据
    persisted: usize,
    /// meta 需要重写（meta 在文件首行，无法追加）⇒ 下次写入时走全量路径
    meta_dirty: bool,
    /// 打开文件时完成的修复记录；由 `Session::take_recovery` 一次性取走。
    recovery: Option<SessionRecovery>,
    #[cfg(test)]
    flush_count: std::sync::atomic::AtomicUsize,
}
```

行为契约（逐方法）：

| 方法 | 落盘行为 |
|---|---|
| `append(msg)` | `messages.push(msg)`；`OpenOptions::new().append(true).open(path)`；**单次 `write_all`** 写入「序列化后的一整行 + `\n`」；`persisted += 1`。**不碰 meta**，**不调 `flush_to_file`** |
| `set_meta(meta)` | `self.meta = Some(meta)`；`meta_dirty = true`；走全量重写 `flush_to_file()`；成功后 `meta_dirty = false`、`persisted = messages.len()` |
| `replace_all(msgs)` | `messages = msgs`；全量重写（tmp + rename 原子路径）；`persisted = messages.len()` |
| `clear()` | `messages.clear()`；全量重写为「只有 meta（若有）的空文件」；`persisted = 0` |
| `open(path)` | 读路径不变；`persisted = 读到的 messages 条数`；若 `repair_dangling_tool_calls` / `normalize_tool_arguments` 修了任何东西，仍走一次全量 `flush_to_file()` |

代码形状约束：

- `append` 的写必须是**一个** `write_all` 调用，参数是「`serde_json::to_string(msg)` + `"\n"`」拼出的
  单个 `String`/`Vec<u8>`。分两次写（先正文后换行）会在崩溃时留下无终止符的半行，
  使下次 `open` 的逐行解析丢掉整行。
- `flush_to_file` 的签名保持 `&self` 不变（`open` 在构造后调用它时无需 `&mut`）。
  `persisted` / `meta_dirty` 的更新由各调用点在**全量写成功之后**做，或让 `flush_to_file`
  返回后由调用点设定 —— **不得**在 `flush_to_file` 内部写 `persisted`（它是 `&self`）；
  若需要统一，可把 `persisted` / `meta_dirty` 改为 `Cell<usize>` / `Cell<bool>`，但首选做法是
  在各调用点（`set_meta` / `replace_all` / `clear` / `open`，均为 `&mut self` 或构造期）
  显式赋值，保持字段类型是普通的 `usize` / `bool`。
- 全量路径仍是 tmp + rename；`flush_count` 只在 `flush_to_file` 内自增，`append` 不经过它。

### 为什么 meta 要单独处理

meta 落盘在文件**首行**（`jsonl.rs:117-118`：「先写 meta 行（若存在）……append / clear /
replace_all / set_meta 都经本函数，故不存在漏写路径」）。`O_APPEND` 只能把字节加到**文件尾**，
无法改写首行 ⇒ meta 的变更天然与「只追加」冲突。

两条路，取前者：

| 方案 | 做法 | 评价 |
|---|---|---|
| **S1（采用）** | `set_meta` 置 `meta_dirty = true` 并**立即走一次全量重写**（meta 变更本就罕见） | 保住首行位置、保住行判别契约；meta 每次会话只变几次，一次 `O(会话)` 写可接受 |
| S2（不采用） | 把 meta 改写成**尾行**、「最新覆盖」语义 | 破坏 `load_from_file` 的行判别：`Message` 无 `type` 字段、`SessionMeta` 的 `type` 为必填，两者互不误判是当前唯一的安全依据（`jsonl.rs:57-60`）。尾行 meta 与尾行消息交错后，判别仍成立但**语义要靠顺序兜底**（谁最后谁生效），且「跳过坏行」会顺带丢掉最新 meta |

`meta_dirty` 字段因此是**语义文档**而非必需状态机：在 S1 里 `set_meta` 立即重写，置位只为
「若将来把重写推迟到下次写入」留明确的判定点；当前实现中它在重写成功后必须被清掉，
不得残留为 `true`（否则 `open` 后的首次写入路径会误判）。

### 不变量

1. **meta 永不因重写丢失** —— meta 只经全量路径（`flush_to_file`）写；`append` 路径完全不碰 meta。
   全量路径仍是「先写 meta（若有）再写消息」。
2. **写入原子** —— `append` 天然原子（`O_APPEND` 单次写整行）；`replace_all` / `set_meta` /
   `clear` / 修复落盘保持 tmp + `rename` 原子替换。
3. **`replace_all` 单次落盘** —— 保持「赋值一次 + 单次 `flush_to_file`」，`flush_count` 增量 == 1。
4. **崩溃后行完整** —— `append` 必须单次 `write_all` 写一整行（含 `\n`），不得分两次写。
5. **读路径不变** —— `load_from_file` 仍逐行 `Message` → `SessionMeta` 判别、坏行 `skipped_lines += 1`
   跳过；本单元不优化读。

### 数据流

```
append(msg)
  messages.push(msg)
  └─ OpenOptions::append(true).open(self.path)
       └─ write_all(serialize(msg) + "\n")     # 单次写，整行
  persisted += 1
  （meta 未触及；flush_to_file 未调用）

set_meta(meta)
  self.meta = Some(meta); meta_dirty = true
  └─ flush_to_file()      # tmp: meta 行 + 全部 messages → rename
       └─ meta_dirty = false; persisted = messages.len()

replace_all(msgs)
  self.messages = msgs
  └─ flush_to_file()      # tmp: meta 行 + 新集合 → rename
       └─ persisted = messages.len()

clear()
  self.messages.clear()
  └─ flush_to_file()      # tmp: 仅 meta 行（若有） → rename
       └─ persisted = 0

open(path)
  load_from_file → (meta, messages, skipped_lines)
  repair_dangling_tool_calls / normalize_tool_arguments
  if 修了任何东西 ⇒ flush_to_file()
  persisted = messages.len()   # 以**读到的**条数为准
```

`append` 的增量依据是 `persisted`：正常情况下它与 `messages.len()` 的差恒为 0 或 1
（`append` 后立即对齐）。若出现 `persisted > messages.len()`（不可能由本实现的公开路径产生），
应视为内部不变量破坏 —— 实现上可用 `debug_assert!(self.persisted < self.messages.len())` 兜住，
**不做静默钳制**（静默钳制会让「丢行」伪装成正常）。

### 已知边界与风险

| 风险 | 说明与处置 |
|---|---|
| 同进程多实例指向同一文件时 `O_APPEND` 并发写无锁 | 现有设计不允许（`JsonlSession` 独占一个路径，每会话一份）。**不加锁**，在 `JsonlSession` 的 doc 注释里写明「同一路径同一时刻只允许一个实例」 |
| `persisted` 与实际文件不一致（外部改文件） | 启动时以**读到的**条数初始化；**不检测**外部修改 —— 现有语义同样不检测，本单元不新增探测成本 |
| meta 变更触发一次全量重写 | 可接受：meta 每次会话变几次（标题、模型等），一次 `O(会话)` 写不足以成为瓶颈 |
| 半行残留（崩溃于 `append` 的 `write_all` 中途） | 单次写把窗口压到最小；若仍留下半行，`open` 的逐行解析会把它计入 `skipped_lines` 并跳过（该行必然是**最后一条**，前面的行完整） |
| 空文件 / 仅 meta 文件的 `append` | `replace_all(vec![])` / `clear` 后文件可能只有 meta 行或无内容；`append` 直接尾部追加即为合法续写，无需补换行（全量路径已为每条行写了 `\n`） |

## 涉及文件

- 修改: `crates/ys-session/src/jsonl.rs` —— 主改动（结构体字段、`append`、`set_meta`、
  `replace_all`、`clear`、`open` 的 `persisted` 初始化、`flush_count` 相关测试）
- 修改: `crates/ys-session/src/lib.rs` —— **仅注释**：`replace_all` 默认实现文档中
  「对『`append` 每次全量重写整个文件』的实现（如 `JsonlSession`）」一句在 B 完成后不再成立，
  改写为陈述默认实现的真实代价（`clear` + N 次 `append`，非原子；对持久化实现仍应覆写）

**不改动**：`crates/ys-session/src/memory.rs`、`crates/ys-session/src/lib.rs:206` 的
`CountingSession`、`crates/ys-loop/src/basic.rs:3545` 的 `OrderedSession` —— 它们与落盘语义无关。

## 测试合同

全部落在 `crates/ys-session/src/jsonl.rs` 的 `mod tests`。现有测试
`test_jsonl_replace_all_flushes_exactly_once`（`jsonl.rs:502` 起）中的
`assert_eq!(before, 3, "3 次 append 各落盘一次")` 在本单元后语义改变
（`append` 不再经 `flush_to_file`），须改为「3 次 append 后 `flush_count` 仍为 0」
并保留「每次 append 各写一次增量」的等价断言（由下表 #1 承担）。

| # | 覆盖点 | 验收方法 | 优先级 |
|---|---|---|---|
| 1 | append 后**文件字节增量** == 新行字节数 | 记下 `metadata().len()`；`append` 一条已知消息；`serialize(msg)+"\n"` 的长度应等于增量。断言 `after - before == expected`，并断言文件末尾就是该行 | **必测**（核心；也是变异点） |
| 2 | append 期间 `flush_to_file` 不被调用 | `flush_count` 在 `append` 前后不变（空会话首次 append 前为 0） | **必测** |
| 3 | `set_meta` 后 meta 与所有消息都在 | 3 次 `append` → `set_meta` → 重新 `open`：`meta()` 为 `Some`，`messages().len() == 3` 且内容顺序一致 | **必测** |
| 4 | `replace_all` 后文件内容 == 新集合，且 `flush_count` 只 +1 | `replace_all(vec![n1, n2])` 后重新 `open`：消息恰为 `[n1, n2]`；`flush_count` 增量 == 1 | **必测** |
| 5 | 半行文件（手工截断）打开后能修复/跳过，`skipped_lines` 正确 | 手写「完整行 + 半行」；`open` 后 `messages().len()` 只剩完整行，`take_recovery().skipped_lines == 1`；再 `append` 一条后重新 `open` 不产生新 `skipped_lines` | **必测** |
| 6 | 多轮 append 后重新 `open`，消息数正确 | 循环 N 次 `append`（含一次 `set_meta` 夹在中间）后 `open`：`messages().len() == N`，顺序与内容一致 | **必测** |
| 7 | 变异点：把 `append` 改回全量重写 ⇒ #1 变红 | 手工注入（`append` 内改调 `flush_to_file`）跑 #1 与 #2，两者均须失败；确认后还原 | **必测**（验证测试本身有效） |

## 不做的事

- **不改读路径**：`load_from_file` 仍是全量逐行读 + 跳过坏行；`/resume` 的读取代价不在本单元范围。
- **不加文件锁**：不排查外部修改、不做跨进程互斥；「同路径单实例」写进注释作为契约。
- **不改 `Session` trait 签名**：`append` / `clear` / `replace_all` / `set_meta` / `messages` 的形状不变。
- **不改 `MemorySession` / `CountingSession` / `OrderedSession`**，也不调整它们在测试里的用法。
- **不顺手改 `load_from_file` 的判别顺序**，不把 meta 挪到文件尾。
- **不引入新依赖**（append 用 `tokio::fs::OpenOptions`，已在 tokio 的 `fs` feature 内）。
