# 会话写入改 append-only：实测报告

- **日期**：2026-11-04
- **性质**：**实现与验证记录**。as-built 见
  [`docs/design-final/session-append-only.md`](../design-final/session-append-only.md)，
  决策见 [`docs/adr/0070-session-append-only-write-path.md`](../adr/0070-session-append-only-write-path.md)。
- **被测**：`crates/ys-session/src/jsonl.rs` 的 `JsonlSession::append` 写入路径。
- **提交**：`6e2e259`，已合入 master。

---

## 1. 概要

`JsonlSession::append` 从「`flush_to_file` 全量重写整个文件（tmp + rename）」改为
「`O_APPEND` 单次 `write_all` 只追加新行」。写入代价从 O(n) 降到 O(1)，
与会话已落盘条数无关。

`flush_to_file` 保留下来，成为 **meta 行的唯一写入实现**，继续服务 `set_meta` / `clear` /
`replace_all` / `open` 的修复落盘。

改动 **4 文件，+843 / −16**。

## 2. 改动清单

| 文件 | 改动 |
|---|---|
| `crates/ys-session/src/jsonl.rs` | 新增 `persisted` 字段（`:18-23`）、`truncate_partial_tail`（`:85`）、append-only 实现（`append` `:255`，`clear`/`replace_all` 对齐 `persisted`）、`flush_count` 测试计数器、30 条测试（`mod tests`，`:422` 起） |
| `crates/ys-session/src/lib.rs` | `replace_all` 默认实现注释（`:73-80`）：移除已失效的 O(n²) 论断，改为「代价 = 1 次 `clear()` + N 次 `append()`，由各实现自己的 `append` 成本决定」 |
| `docs/design-plans/2026-11-04-session-append-only.md` | 设计计划（新） |
| `docs/exec-plans/2026-11-04-session-append-only.md` | 执行计划（新） |

## 3. 验证结果

| 项 | 结果 |
|---|---|
| `./scripts/par-test.sh` | **12 次连跑全绿**（修复 `flush` 后） |
| `./scripts/cargo-build.sh test -p ys-session` | **40 passed**（`jsonl` 30 + `lib` 7 + `meta` 3） |
| `cargo clippy -p ys-session --all-targets` | **0 warning** |
| `cargo fmt --check` | 干净 |

`par-test.sh` 连跑 12 次是为排除时序偶然性：`append` 后的可见性断言与并行测试同机竞争文件系统，
单次绿不足以说明问题。

## 4. 变异验证（4 个变异点，实测结果）

| 变异 | 期望 | 实际 |
|---|---|---|
| `append` 改回 `flush_to_file` 全量重写 | 字节增量断言红 | `append_does_not_flush`、`test_jsonl_replace_all_flushes_exactly_once` FAILED ✅ |
| 去掉 `.create(true)` | 首次 append 失败 | 4 条 FAILED（`append_does_not_flush`、`first_append_creates_file`、`append_writes_only_new_line_bytes`、`append_after_replace_all_lands_at_end`）✅ |
| 去掉尾部截断 | 半行粘连 | `append_after_truncated_tail_does_not_merge`、`open_skips_truncated_tail_line` FAILED ✅ |
| 去掉 `file.flush()` | 可见性断言红 | **初始全绿（39 passed），抓到率仅 1/4** ❌ → 补 `append_is_visible_to_other_readers_immediately` 后，删 `flush` 第 0 次即 FAILED（`left: 0, right: 57`）✅ |

**第 4 个变异最初抓不到，是本次测试网的漏洞**，已由新增用例补上。原测试网只校验「文件里读出的
内容正确」，而缺 `flush` 时**内容确实正确** —— 滞后的只是元数据（`metadata().len()`）。
`append_is_visible_to_other_readers_immediately` 改用 `std::fs::metadata` 校验长度、并循环 20 次累加，
把这条概率性缺口变成确定性判据。

4 个变异点现已全部可**确定性**抓到（不依赖并发运气）。

## 5. 过程中发现的两个真缺陷（都已修复）

### (a) 半行重复计数

`load_from_file` 用 `read_until(b'\n')` 读行，末尾**无换行**的残行同样会被读出并计入
`skipped_lines`；而 `open` 又因尾部截断再加 1 ⇒ 一条残行计成 **2**，`recovery.is_empty()`
的判定跟着错（调用方会看到夸大的坏行数）。

已修正为：`truncate_partial_tail` **返回被截掉的字节数**而非 bool，`open` 不再累加，
只用返回值做日志 / 诊断。判据用例 `open_skips_truncated_tail_line` 断言 `skipped_lines == 1`。

### (b) `persisted` 错位静默丢消息

`append` 取 `messages[persisted]`。若 `persisted` 偏大 1，写入的是**上一条已写过的**消息，
新消息留在内存里永不上线 —— 磁盘上看起来一切正常，重开少一条；偏大到越界则直接 panic。

已修正为：取值前加兜底对齐（`if self.persisted >= self.messages.len()` ⇒ 从尾部重新对齐）
+ `debug_assert!`。release 下不 panic、宁可多写一行也不静默丢消息。

## 6. 已知边界

- `set_meta` 仍为 O(n)：meta 在文件首行，无法追加。
- 同进程多实例指向同一文件时无锁（设计上独占文件，不允许）。
- 不检测外部对文件的修改。
- 读路径（`/resume`）仍全量读 + 逐行解析。

## 7. 未完成项

- 本次只优化**写**路径；读路径仍全量读（`load_from_file` + 逐行 `serde_json` 解析）。
- 未做文件锁 —— 设计上不允许同一文件多实例，故不引入。
