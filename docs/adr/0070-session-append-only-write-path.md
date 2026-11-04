# ADR-0070: 会话写入的 append-only 路径

## 状态

已接受（2026-11-04）

## 上下文

`JsonlSession::append` 走 `flush_to_file`：写 tmp 文件（meta 行 + 全部 N 条消息）再 `rename` 覆盖。
每次追加的代价随会话长度**线性增长**，即第 N 次追加要序列化并重写前 N−1 条。

代价分布与需求方向相反：压缩（`/compact`、Step 4b 自动触发）恰好在会话**最大**时发生，
而压缩之后的那次 `replace_all` 本身已是全量写；真正被放大的是**压缩前**累积期间的每一次追加。
vision 功能（图片进会话）会让单条消息显著变大，同一条曲线再被抬高一个量级。

同时，纯追加要成立需要补一条崩溃恢复语义：一次写入可能只落下一行 JSON 的前缀
（崩溃或短写），文件尾部于是留下既无 `\n` 也无法解析的残行。全量重写天然覆盖掉残行，
纯追加不会。

## 决策

1. `append` 改为 `O_APPEND` **单次 `write_all`** 写一整行（含 `\n`）。
2. 新增 `persisted: usize` 记录已落盘条数，作为增量起点；`append` 写 `messages[persisted]`，
   成功后 `+1`。
3. `flush_to_file` **保留**为 meta 行的唯一写入实现，`set_meta` / `clear` / `replace_all` /
   `open` 的修复落盘四条全量路径继续经它，成功后把 `persisted` 对齐到 `messages.len()`。
4. `open` 增加 `truncate_partial_tail`：文件末尾无 `\n` 时截到最后一个 `\n` 之后。

第 3 条不是风格问题：绕过 `flush_to_file` 会让「从未 `set_meta` 的会话被 `replace_all` 成空消息」
写出空文件 —— 消息被清空、meta 也没写，整个会话状态在磁盘上消失。

## 为什么不做「meta 改成尾行」

meta 在首行是唯一能保持 `append` 完全 O(1) 的布局（首行一旦写下就不必再动），
但这件事的真实理由是**行判别契约**：

- `Message` 的序列化结果**没有**顶层 `type` 字段（有快照用例钉住：
  `jsonl.rs::test_message_serde_shape_has_no_type_key`）；
- `SessionMeta` 的 `type` 是**必填**字段。

`load_from_file` 正是靠这一差异逐行判别：先按 `Message` 解析，失败再按 `SessionMeta` 解析。
把 meta 挪到尾行不改变「首行是不是 meta」这个问题 —— 它改变的是「能否只靠 `type` 是否存在
判别两种行」。要让尾行 meta 仍是权威值（读回时要覆盖先到的 meta），就必须：

- 给 `Message` 加判别字段（改 `ys-core` 的数据形状，穿透 model adapter、事件流、协议三层），或
- 引入第二种行格式（涉及整个 `load_from_file` 与所有写入路径的重写）；
- 且 `set_meta` 若仍要「不重写消息」，就得支持**截断旧 meta 行再追加**——一样要 O(n) 找位置。

改动面远超收益。现状的替代做法是接受 `set_meta` 的 O(n)（见「代价」）。

## 备选方案与否决理由

| 方案 | 否决理由 |
|---|---|
| 保持现状（全量重写） | 会话越长每次追加越慢；与 vision 功能冲突（图片让会话显著变大，单次重写代价再被抬高） |
| 落成 SQLite / 嵌入式 DB | 引入重依赖（`ys-session` 当前只有 `serde` + `tokio`）；失去「文本可读、可 diff、可手工修」的特性 —— 而手工修会话正是当前恢复修复链（`repair_dangling_tool_calls` / `normalize_tool_arguments`）存在的前提 |
| 每条消息一个文件 | inode 膨胀；回放要排序；「整份文件原子替换」这一条语义在多文件下要自己重建（更难） |

## 代价

- **`set_meta` 仍是 O(n)**：meta 在首行，无法追加。每会话通常只变几次（回合末刷新
  tokens / turns），接受。
- **`append` 需要尾部残行截断**：崩溃后不留残行是纯追加必须自己承担的责任，`open` 承担它。
- **`write_all` 后必须 `flush`**：`tokio::fs::File` 是**异步 close**（drop 只把关闭任务丢给
  阻塞线程池），不 `flush` 的话 `append` 返回后、close 完成前其它句柄 `stat` 会读到 0 或旧长度，
  而字节其实已可见。这是数据已在、元数据滞后的异步失败模式，必须显式 `flush().await`。
- **`persisted` 成为新的状态位**：它错位会静默丢消息，故 `append` 取值前有兜底对齐 +
  `debug_assert!`（release 下多写一行，不 panic、不静默丢）。
- **`.create(true)` 必需**：`open` 从不创建文件（惰性落盘），首次 `append` 得自建。

## 后果

**正面**：

- 写入从 O(n) 降到 O(1)，与会话长度无关
- `flush_to_file` 的语义收窄为「meta + 全量」这一件事，调用点只剩四条，meta 不会漏写
- 字节增量可断言 ⇒ 写入形态有了确定性判据（`append_writes_only_new_line_bytes`、
  `append_does_not_flush`）

**负面 / 新增路径**：

- 崩溃恢复新增「尾部残行截断」这条路径（`open` → `truncate_partial_tail`）；残行仍由
  `load_from_file` 计入 `skipped_lines`，截断**不重复计数**
- 可见性依赖显式 `flush`，删掉它测试网立刻红（`append_is_visible_to_other_readers_immediately`）
- 读路径未动：`/resume` 仍全量读 + 逐行解析

## 关联

- [session-append-only.md](../design-final/session-append-only.md) — **当前形态以它为准**
- [2026-11-04-session-append-only-report.md](../reports/2026-11-04-session-append-only-report.md) — 验证与变异实测
- [design-plans/2026-11-04-session-append-only.md](../design-plans/2026-11-04-session-append-only.md) — 设计计划
- ADR-0026 — 会话 meta 落首行（本 ADR 的布局前提）
