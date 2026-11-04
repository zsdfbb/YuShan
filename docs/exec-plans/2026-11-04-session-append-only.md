# 会话 append-only 落盘 执行计划

> 对应设计：`docs/design-plans/2026-11-04-session-append-only.md`
> 日期：2026-11-04
> 纪律：**每个 `impl` 与它的 `test` 成对**；同波文件集互不相交；单单元 ≤5 文件 / ≤10 分钟。
> 构建与测试入口：`./scripts/cargo-build.sh` / `./scripts/par-test.sh`（**不用**裸 `cargo`）。

## 基线

- **基线 ref**: `<Phase 3 开始时由 ./scripts/build-baseline.sh <label> 填写>`
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据

## 任务清单

| 任务 | 类型 | 依赖 | 文件 |
|---|---|---|---|
| **U1** | impl | 无 | `crates/ys-session/src/jsonl.rs` |
| **T1** | test | U1 | `crates/ys-session/src/jsonl.rs`（`mod tests`） |
| **U2** | impl（仅注释） | U1 | `crates/ys-session/src/lib.rs` |
| **T2** | test（验证） | U1、U2 | 无源码改动；全量测试 |

### U1 [type=impl]

改动全部在 `crates/ys-session/src/jsonl.rs`：

1. `JsonlSession` 加两个字段（带 doc 注释）：
   - `persisted: usize` —— 已落盘的 messages 条数，`append` 的增量依据
   - `meta_dirty: bool` —— meta 需重写（meta 在首行，无法追加）⇒ 下次写入走全量路径
2. `open()`：构造时 `persisted: messages.len()`、`meta_dirty: false`；修复落盘（若
   `repaired_tool_calls > 0 || normalized_tool_arguments > 0`）后 `persisted` 保持
   `messages.len()`（全量写完成后文件与内存一致，无需额外赋值，但**必须**保证构造时已初始化）。
3. `append()`：`messages.push(message)` → 取 `messages[persisted]`（刚推入的那条）→
   `serde_json::to_string` → 拼 `"\n"` → `tokio::fs::OpenOptions::new().append(true).open(&self.path)`
   → **单次** `write_all` → `persisted += 1`。删掉对 `flush_to_file()` 的调用。
   - 打开失败 / 写失败映射为 `SessionError::Storage(...)`，与现有错误文本风格一致。
   - `debug_assert!(self.persisted < self.messages.len())` 放在取值之前，兜住内部不变量。
4. `set_meta()`：`self.meta = Some(meta)`；`self.meta_dirty = true`；`flush_to_file().await?`；
   成功后 `meta_dirty = false`、`persisted = messages.len()`。
5. `replace_all()`：`self.messages = messages`；`flush_to_file().await?`；`persisted = messages.len()`。
6. `clear()`：`self.messages.clear()`；`flush_to_file().await?`；`persisted = 0`。
7. `flush_to_file()` 内 `:117-118` 的注释需**保留「meta 唯一落点」这个不变量**，只是把措辞改准确。
   原注释说「append / clear / replace_all / set_meta 都经本函数」—— append 改后不再经本函数，
   但**结论不变且更强**：meta 行只可能由本函数写出，`append` 是唯一不写 meta 的路径。
   改写后的契约：
   - **`flush_to_file` 是无条件写 meta 行的唯一实现**（`if let Some(meta) = &self.meta`）；
   - **所有全量路径**（`set_meta` / `replace_all` / `clear` / `open` 的修复落盘）**都必须调用本函数**
     —— 不得有任何一条自己序列化写文件、绕过 meta 写入的路径；
   - `append` 是唯一不写 meta 的路径，因此**它没有能力破坏 meta**。
   
   ⚠️ **这是硬约束，不是文档改写**：`replace_all` 与 `clear` 必须始终走 `flush_to_file()`，
   不得为了「省一次 meta 写」而改成自己写文件。否则会出现真实数据丢失：
   一个从未 `set_meta` 过的会话，若用户直接 `/compact`（走 `replace_all`）且此时消息被替换为空，
   绕过 `flush_to_file` 就会写出一个**空文件**，重开时 meta 与消息全丢。
8. **不改任何测试**。现有测试 `test_jsonl_replace_all_flushes_exactly_once`（`jsonl.rs:503`）
   里 `assert_eq!(before, 3, "3 次 append 各落盘一次")` 在 append 改造后**必然变红**
   —— 那是预期的红，由 T1 负责修正断言（见 T1 的 #4）。本单元只改生产代码。
9. **保持 `persisted` 与文件内容的一致性**：`set_meta` / `replace_all` / `clear` 在
   `flush_to_file().await?` **成功之后**必须更新 `persisted`（分别为 `messages.len()` /
   `messages.len()` / `0`）。若 `flush_to_file` 返回 `Err`，`persisted` 不得变动
   —— 用 `?` 提前返回即可自然满足，**不要**先赋值再 flush。
10. **`append` 打开文件必须带 `.create(true)`**。`open()` 只在路径已存在时读文件
    （`jsonl.rs:29-33`），**从不创建文件** —— 全新会话第一次 `append` 时文件并不存在。
    现在能工作是因为 `flush_to_file` 里 `File::create(tmp)` + rename 顺带创建了它；
    改成 `OpenOptions::append(true)` 后，对不存在的文件返回 `NotFound` ⇒ 新会话第一条消息就写不进去。

    ```rust
    tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&self.path)
    ```

    变异判据（T1 的 `first_append_creates_file`）：去掉 `.create(true)` ⇒ 该用例直接 `Err`。

11. **`open()` 必须修复「尾部半行」**。`load_from_file` 读到坏行只跳过、**不截断**
    （`jsonl.rs:91-102`）。append-only 下，若上次进程在 `write_all` 中途崩溃，文件尾留下半行
    （无 `\n`）；`open` 跳过后 `persisted` 指向完好条数，但**文件尾仍是那半行**——
    下一次 `append` 用 `O_APPEND` 写在半行后面，半行 + 新行粘成一条新坏行，
    **每次重开后都少一条消息，且完全静默**。

    修复方式：在 `load_from_file` 之后（或其中），若文件非空且最后一个字节不是 `b'\n'`，
    **截断到最后一个 `\n` 之后**（半行本就是坏数据；补换行只会让它变成一条被跳过的坏行，
    还永久留在文件里）。注意：
    - 截断要在计算 `persisted` **之前**完成，否则 `persisted` 会偏大；
    - 若文件里**一个 `\n` 都没有**（全文件半行），截断为空文件，`persisted = 0`；
    - 截断属于「修复」，是否计入 `SessionRecovery` —— **计入**，与 `skipped_lines` 同栏位即可，
      调用方能感知到「上次退出不干净」；
    - 截断后再走原有的「修复落盘」分支（`repaired_tool_calls > 0 || ...`）时保持一致。

- 依赖：无
- 单单元自检：`./scripts/cargo-build.sh check --all-targets`

### T1 [type=test]

在 `crates/ys-session/src/jsonl.rs` 的 `mod tests` 内。下表是本任务的完整用例清单
（含设计文档「测试合同」的 7 条 + Phase 2 复核发现的两个缺陷的回归用例）：

| # | 用例 | 要点 |
|---|---|---|
| 1 | `append_writes_only_new_line_bytes` | `metadata().len()` 增量 == `serialize(msg)+"\n"` 长度；并断言文件尾部即该行字节 |
| 2 | `append_does_not_flush` | `flush_count` 在 `append` 前后相等（空会话为 0） |
| 3 | `set_meta_keeps_meta_and_messages` | append×3 → `set_meta` → `open`：meta 在、3 条消息顺序一致 |
| 4 | `replace_all_flushes_exactly_once` | 沿用现有用例，断言重新 `open` 后内容 == 新集合，`flush_count` 增量 == 1 |
| 5 | `open_skips_truncated_tail_line` | 手写「完整行 + 半行」；`skipped_lines == 1`、消息只剩完整行；再 `append` 后重开不新增跳过 |
| 6 | `reopen_after_multi_append_counts_messages` | N 次 append（中间夹一次 `set_meta`）后 `open`：`messages().len() == N` |
| 7 | `replace_all_without_prior_meta_keeps_meta` | **治空文件丢失**：先 `append`×2（不调 `set_meta`）→ `replace_all(vec![])` → 重新 `open`：文件里若本会话曾有 meta 则须仍在；重点断言**文件不是空的**（`metadata().len() > 0` 当且仅当有过 meta 或有过消息），且 `messages()` 为空。变异：让 `replace_all` 绕过 `flush_to_file` 自己写文件 ⇒ 若会话曾有 meta 则该用例变红 |
| 8 | `append_after_replace_all_lands_at_end` | `append`×2 → `replace_all`（换 1 条新消息）→ `append`×1 → 重新 `open`：得到 2 条消息、顺序为「replace 的那条 + 新 append 的那条」。变异：`replace_all` 后忘记更新 `persisted` ⇒ 新 `append` 会重复写或漏写，重开断言变红 |
| 9 | `first_append_creates_file` | 用 `temp_path` 造一个**不存在**的路径 → 直接 `append` 一条 → 断言 `Ok`、文件存在、内容 == `serde_json::to_string(&msg)+"\n"`。变异：`OpenOptions` 去掉 `.create(true)` ⇒ `Err(NotFound)`，用例红 |
| 10 | `append_after_truncated_tail_does_not_merge` | 手写「完整行 + 半行（无 `\n`）」→ `open`（须触发尾行截断）→ `append` 一条 → **重新 `open`**：断言 `messages().len() == 2`（完整行那条 + 新 append 那条），**不是 1**。变异：去掉 `open` 的尾行修复 ⇒ 半行与新行粘连，重开只剩 1 条，用例红 |

测试注意：

- 临时路径沿用文件内现有 `temp_path(tag)` 辅助函数（`jsonl.rs:307`）；若需新建目录，命名遵守并行安全约定
  （**原子序号 + `process::id()`**）。
- 只读文件字节用 `std::fs::metadata` / `tokio::fs::read`，不要引入新依赖。
- 不得因为「新行可能含中文/转义」而在断言里写死猜测的字节串 —— 一律以
  `serde_json::to_string(&msg).unwrap().len() + 1` 计算期望值。
- **本任务承担修正现有断言的职责**：`jsonl.rs:503` 的
  `test_jsonl_replace_all_flushes_exactly_once` 里 `assert_eq!(before, 3, ...)`
  在 append 改造后必然变红 —— 改为断言 `before == 0`（3 次 `append` 都不经 `flush_to_file`），
  并加一行注释指向本任务的 `append_writes_only_new_line_bytes`
  —— 那里才是「每次 append 各写一次增量」的新落点。

- 依赖：U1
- 自检：`./scripts/par-test.sh -p ys-session`

### U2 [type=impl，仅注释]

`crates/ys-session/src/lib.rs:75-86` 的 `replace_all` 默认实现文档：删去「对『`append` 每次全量重写
整个文件』的实现（如 `JsonlSession`），默认实现会把整体替换退化成 O(n²)」这一前提性论断
（它在本单元后不再成立），改为陈述**默认实现自身**的事实：

- 默认实现 = `clear()` + N 次 `append()`，**非原子**，中途失败留下被截断的会话；
- 代价是 1 次 `clear` + N 次 `append`，由各实现自己的 `append` 成本决定；
- 要求「整体替换单次落盘」的实现必须覆写（`JsonlSession` 覆写为赋值一次 + 单次 `flush_to_file`）。

**仅改注释，不改任何逻辑**。

- 依赖：U1
- 自检：`./scripts/cargo-build.sh check --all-targets`（确认无 `clippy::doc_markdown` 之类新告警）

### T2 [type=test，验证]

无源码改动，纯验证：

1. `./scripts/cargo-build.sh --tests` 绿；
2. `./scripts/par-test.sh -p ys-session` 绿；
3. `./scripts/par-test.sh` 全量绿 —— 确认 `crates/ys-loop`（`basic.rs:3545` 的 `OrderedSession`）、
   `apps/coding-agent`（会话装配 / `/resume`）等下游没有被写路径变更破坏；
4. **变异**：把 `append()` 改回「`messages.push` + `flush_to_file()`」，重跑 T1 的 #1 / #2
   —— 两者都必须变红；记录变红信息后还原。

- 依赖：U1、U2

## 波次计划（Phase 3 计算后回填）

| 波次 | 单元 | 含任务 | 文件集 | review 档 |
|---|---|---|---|---|
| **W0** | U1 → T1 | U1, T1 | `crates/ys-session/src/jsonl.rs` | deep |
| **W0** | U2 | U2 | `crates/ys-session/src/lib.rs` | shallow |
| **W1** | U3 | T2 | 无源码改动 | deep（变异验证） |

**调度说明**：

- **W0 两条并行**：U1→T1（`jsonl.rs`）与 U2（`lib.rs`）文件集不相交。但 U2 的正确性以 U1
  的最终形态为准，**改的是不同文件、不冲突**，可并行。
- **U1 → T1 串行对**：改同一文件，不得并行。先派 U1（只改生产代码，跑
  `./scripts/cargo-build.sh check --all-targets` 确认编译通过但测试预期有红），
  再派 T1（修断言 + 写新用例，跑到全绿）。
- **W1 的 T2 必须在 W0 全部完成后**：它读最终工作区，且变异测试要临时改回 `append` 全量重写。
  变异验证结束后**必须还原**并重跑一次确认绿。

## 验证清单
- [ ] 所有 (impl, test) 配对完成
- [ ] `./scripts/cargo-build.sh --tests` 绿
- [ ] `./scripts/par-test.sh -p ys-session` 绿
- [ ] `./scripts/par-test.sh` 全量绿
- [ ] 变异：append 改回全量重写 ⇒ 字节增量断言变红
- [ ] 新会话（文件不存在）第一次 append 成功创建文件
- [ ] 尾部半行文件 open 后 append 不会与半行粘连（重开消息数正确）
