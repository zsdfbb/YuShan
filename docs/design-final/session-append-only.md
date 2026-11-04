# 会话写入的 append-only 路径 — As-built

> 性质：**实际建成记录**。决策见 `docs/adr/0070-session-append-only-write-path.md`，
> 设计计划 `docs/design-plans/2026-11-04-session-append-only.md`，
> 执行计划 `docs/exec-plans/2026-11-04-session-append-only.md`，
> 报告 `docs/reports/2026-11-04-session-append-only-report.md`。
> 日期：2026-11-04 ｜ 提交 `6e2e259`。

**已实现**。本文只描述 `crates/ys-session/src/jsonl.rs` 中子 `append` 路径**当前**的形态与契约；
meta 行的产生与判别、`/resume` 的读取路径见 `docs/design-final/session-default-new.md`。

## 一句话

`JsonlSession::append` 用 `O_APPEND` 单次 `write_all` 写一整行，**不触碰既有内容**，
代价与已落盘条数无关；`flush_to_file`（tmp + rename 全量重写）退居为 **meta 行的唯一写入实现**，
只服务 `set_meta` / `clear` / `replace_all` / `open` 的修复落盘这四条全量路径。

## 核心形态

```rust
// crates/ys-session/src/jsonl.rs:13-30
pub struct JsonlSession {
    path: PathBuf,
    /// 会话级元数据，落盘于文件**首行**；缺省即无首行 meta。
    meta: Option<SessionMeta>,
    messages: Vec<Message>,
    /// 已落盘的消息条数 —— `append` 的增量起点。
    persisted: usize,
    /// 打开文件时完成的修复记录；由 Session::take_recovery 一次性取走。
    recovery: Option<SessionRecovery>,
    /// 仅测试用：flush_to_file 的调用次数。
    #[cfg(test)]
    flush_count: std::sync::atomic::AtomicUsize,
}
```

**`persisted` 的契约**（`jsonl.rs:18-23`）：

- `messages.len() - persisted` = **尚未写进文件的尾巴**。`append` 写的正是 `messages[persisted]`。
- `append` 成功后 `+1`（`jsonl.rs:287`）。
- 全量路径成功后**对齐到 `messages.len()`**：`clear` → `0`（`jsonl.rs:294`）、
  `replace_all` → `messages.len()`（`jsonl.rs:307`）、`open` 构造 `Self` 时取 `messages.len()`
  （`jsonl.rs:56`，修复落盘发生在构造之后，故这个初值即对齐值）。
- `set_meta` **不改 `messages`**，而它之前 `persisted` 已等于 `messages.len()`，故不需要再对齐
  （`jsonl.rs:243-244`）。
- `persisted` 只由本类型维护，**不对外暴露**，不从磁盘重算。

## 五条写入路径的落盘语义

| 路径 | 落盘方式 | 是否写 meta | 代价 |
|---|---|---|---|
| `append`（`jsonl.rs:255`） | `O_APPEND` 单次 `write_all` 整行（含 `\n`） | **否** | O(1)，与已落盘条数无关 |
| `set_meta`（`jsonl.rs:241`） | `flush_to_file`（tmp + rename 全量） | 是 | O(n) |
| `clear`（`jsonl.rs:291`） | `flush_to_file` | 是 | O(n) |
| `replace_all`（`jsonl.rs:304`） | `flush_to_file` | 是 | O(n) |
| `open` 的修复落盘（`jsonl.rs:68-70`） | `flush_to_file` | 是 | O(n)，仅当有修复时 |

**不变量：`flush_to_file` 是 meta 行的唯一写入实现**（`jsonl.rs:195-207` 的注释即此契约）。
三条全量路径必须继续经它 —— 绕过会让「从未 `set_meta` 的会话被 `replace_all` 成空消息」
写出**空文件**：消息被清空、meta 也没写，全部会话状态在磁盘上消失。
`append` 是唯一不经 `flush_to_file` 的写入路径，它只追加消息行、完全不碰 meta（`jsonl.rs:197`）。

`append` 的单次写也是硬要求（`jsonl.rs:250-251`、`:271-272`）：`O_APPEND` 下分两次写会在两次之间的
崩溃窗口留下半行，那条半行会污染下一次 `append`。故整行先 `push(b'\n')` 再一把 `write_all`。

## `append` 的两个强制细节

### (a) `.create(true)` 必需

`open()` 只在路径**已存在**时读文件（`jsonl.rs:36-41` 的 `if existed { load_from_file }`），
否则直接给空 `meta` + 空 `messages`，**从不创建文件**。会话采用惰性落盘（`/new` 也不预建），
故全新会话的第一次 `append` 时文件尚不存在；`OpenOptions::append(true)` 单独用会 `NotFound`。
`jsonl.rs:275-280` 同时带 `.append(true).create(true)`。

### (b) `file.flush()` 必需

`write_all` 返回只表示字节已交给**内核缓冲**。`tokio::fs::File` 是**异步 close** —— drop 时把
关闭任务丢给阻塞线程池，不等它完成。于是 `append` 返回后、close 真正完成前，其它进程 / 其它句柄
去 `stat` 可能读到 `0` 或旧长度，而字节其实已可见。`jsonl.rs:284-286` 显式 `flush().await`。

实测证据（`crates/ys-session/src/jsonl.rs` tests mod 与本次报告 §4）：

- 缺 `flush` 时 `std::fs::metadata` 读到 `len=0`，而 `std::fs::read_to_string` 能读到**完整内容**
  —— 数据没丢，是元数据滞后。
- 守护这条的是 `append_is_visible_to_other_readers_immediately`（`jsonl.rs:1384`，doc 见 `:1373-1382`）：
  `append` 后立刻用 `std::fs::metadata` 校验长度，**循环 20 次**累加期望值，任一次不符即失败。
- 删掉 `flush` ⇒ 该用例**第 0 次即红**（`left: 0, right: 57`）。守着它是**确定性**的，
  不依赖并发运气。

注意这与 `file_len` 辅助函数（`jsonl.rs:447`）的分工：该辅助函数刻意用「读内容算长度」而非
`metadata().len()`，因为它在同一进程内、同一批断言里被高频调用；**可见性**这条契约由
`append_is_visible_to_other_readers_immediately` 单独用 `std::fs` 钉住。两者不重复。

## `open` 的尾部残行截断

`truncate_partial_tail`（`jsonl.rs:85-133`，doc `:74-84`）：文件末尾无 `\n` 时截到**最后一个 `\n`
之后**（整份无 `\n` ⇒ 截为空）。返回被截掉的字节数（`原长度 - keep`，未截断为 `0`）。

**为什么必需**：纯追加下，一条只落下一半的 JSON 行（崩溃或短写）会让**下一条消息接在它后面**，
两条缝成一行坏数据。重开时这一行既非 `Message` 也非 `SessionMeta` ⇒ 计入坏行、**两条消息一起消失**，
且**完全静默**（库层不写 stderr，只经 `SessionRecovery` 上报）。

实现要点：

- **不整份读**：先 `seek(End(-1))` 读 1 字节，末字节是 `\n` 就返回（常见路径只读 1 字节）；
  否则按 **8 KiB** 块从尾部向前扫，命中 `\n` 即停（`jsonl.rs:100-128`）。
- **不重复计数**：残行**已由 `load_from_file` 计入 `skipped_lines`**（它是被读出的坏行，
  `jsonl.rs:150-181`），截断只负责把它从磁盘切掉。故 `truncate_partial_tail` 返回字节数、
  `open` 不累加（`jsonl.rs:47` 的 `let _ =`）。
- **必须在构造 `Self` 之前完成**（`jsonl.rs:42-48`）：`append` 是纯追加，构造之后的任何写入
  都会落在残行后面。
- 截断只处理**磁盘上的尾巴**，已读入内存的行不受影响。

## 已知边界

- **`persisted` 错位会静默丢消息**：`append` 取 `messages[persisted]`（`jsonl.rs:269`）。若 `persisted`
  偏大 1，写的是**上一条已写过的**消息，新消息留在内存里永不上线；偏大到越界则 panic。
  现有防护：取值前 `if self.persisted >= self.messages.len()` 兜底从尾部重新对齐 + `debug_assert!`
  （`jsonl.rs:260-268`）—— 宁可在 debug 下炸出来、在 release 下多写一行，也不静默丢消息或崩掉会话。
- **`meta` 变更触发一次全量重写**：meta 在文件**首行**，无法追加。`set_meta` 恒为 O(n)。
  meta 每会话通常只变几次（回合末刷新 tokens / turns），可接受。
- **同进程多实例指向同一文件时无锁**：`O_APPEND` 保证**单次写**原子，但本类型设计上独占文件，
  不允许多实例。不加锁。
- **不检测外部修改**：`persisted` 只由本类型维护，外部改动文件不会被发现。
- **读路径未优化**：`/resume` 仍全量读 + 逐行解析（`load_from_file`）。本次只动写路径。

## 测试守护网

`crates/ys-session/src/jsonl.rs` 的 `mod tests`（`:422` 起）共 **30** 条（29 `#[tokio::test]` +
1 同步 `#[test]`）；`cargo test -p ys-session` 全 crate **40 passed**（另含 `lib.rs` 7 条、
`meta.rs` 3 条）。关键用例及其钉死的契约（"变异"即该用例的判据）：

| 用例 | 行 | 钉死的契约 | 变异 |
|---|---|---|---|
| `append_writes_only_new_line_bytes` | `:1049` | 文件长度增量 == 新行序列化长度 + 1，且文件尾部就是该行、首行原样保留 | `append` 退回全量重写 ⇒ 增量变成两行总长 ⇒ 红 |
| `append_does_not_flush` | `:1093` | 连续 3 次 `append` 前后 `flush_count` 不变（`append` 不经 `flush_to_file`） | 走 `flush_to_file` ⇒ 计数 +3 ⇒ 红 |
| `append_is_visible_to_other_readers_immediately` | `:1384` | `append` 返回即对其它读者可见（20 次累加校验 `std::fs::metadata().len()`） | 删 `file.flush()` ⇒ 第 0 次即红（`left: 0, right: 57`） |
| `set_meta_keeps_meta_and_messages` | `:1121` | `set_meta` 的 O(n) 重写不丢既有消息；重开后 meta 与消息双还原、`recovery` 为空 | 只写 meta 不写消息 ⇒ 重开条数变少 ⇒ 红 |
| `test_jsonl_replace_all_flushes_exactly_once` | `:634` | `replace_all` 只落盘一次（`after - before == 1`），且之前 3 次 `append` 计数仍为 0 | 退回 trait 默认实现（clear + N append）⇒ 3 次 ⇒ 红 |
| `open_skips_truncated_tail_line` | `:1161` | 残行只计 **1** 次 `skipped_lines`；`open` 后文件以 `\n` 结尾；后续 append 能成为独立一行 | 截断也计数 ⇒ 变 2；不截断 ⇒ 末字节非 `\n` ⇒ 红 |
| `reopen_after_multi_append_counts_messages` | `:1206` | 多条 `append` 中间夹一次 `set_meta`，重开后条数与顺序均不变 | `set_meta`/`append` 的 `persisted` 对齐错位 ⇒ 条数不对 ⇒ 红 |
| `replace_all_without_prior_meta_keeps_meta` | `:1244` | `replace_all(vec![])` 后文件**非空**（meta 行仍在），重开 meta 一致 | `replace_all` 绕开 `flush_to_file` 自己写 ⇒ 空文件 ⇒ 红 |
| `append_after_replace_all_lands_at_end` | `:1277` | `replace_all` 后的 `append` 落文件尾部、不重写旧行 | 忘对齐 `persisted` ⇒ 重复写或跳过本条 ⇒ 红 |
| `first_append_creates_file` | `:1315` | `open` **不创建**文件；首次 `append` 创建，内容恰为该行 + `\n` | 去掉 `.create(true)` ⇒ `append` 返回 `Err` ⇒ 红 |
| `append_after_truncated_tail_does_not_merge` | `:1341` | 截断后 `append` 与残行不粘连，重开仍是 2 条独立消息 | 去掉截断 ⇒ 半行与新消息缝成一行 ⇒ 只剩 1 条 ⇒ 红 |

`flush_count` 只在 `#[cfg(test)]` 下存在（`jsonl.rs:26-29`、`:187-189`），不进入生产结构体布局。
