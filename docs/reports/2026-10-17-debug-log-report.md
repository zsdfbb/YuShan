# `--debug` 结构化执行日志（P0）验收报告

- 执行计划：`docs/exec-plans/2026-10-17-debug-log.md`（Task 1–18）
- 设计：`docs/arch/agent-observability/design.md` + `adr-debug-log.md`
- 基线：`7987b4b`（`chore(sw): baseline before debug-log`）
- 验收日期：2026-10-17

## 结论

P0 范围内的 T1–T8 全部通过；T5 的「进程被 kill」部分按计划推迟为 P1（正常 flush、
失败退出与 UI 收摊路径均已在集成层验证）；T9（真实 provider 的思考/正文归因）需要用户
以真实凭证人工确认（`MANUAL_ACK_REQUIRED`）。

## 验收项

| 项 | 结果 | 证据 |
|---|---|---|
| **T1** 关闭 debug | ✅ | 单元：`main.rs::debug_off_writes_no_file_and_leaves_existing_log_byte_identical`（无逐轮行，逐字节不变）。集成：`tests/debug_log.rs::t1_*` —— 目标目录零 `debug-*.jsonl`；`yushan.log` 的预置内容原样保留，debug 开/关的诊断行在归一化计时（`round_*` 的毫秒、`turn=` 行的 `elapsed`/`budget_remaining` 秒）后逐行相同；`-p` 的 stdout 仍只有模型文本，stderr 无 debug 内容 |
| **T2** `--json` 加法契约 | ✅ | `tests/debug_log.rs::t2_*`。golden 由基线二进制（`7987b4b`）生成并入库：`tests/golden/json_no_debug_baseline.jsonl`（443 B）。过滤 `RoundUsage` 后逐字节相同；**所有** `RoundUsage` 行数恰等于模型轮数（防止普通回合混入 `compaction`）；`reasoning_tokens: None` 不出现该键 |
| **T3** 10k 增量 | ✅ | 单元：`debug_log::tests::test_ten_thousand_deltas_keep_debug_file_under_200_kib`（10,000 条增量落真实文件后 **< 200 KiB**、零增量记录）；`test_records_have_valid_envelope_and_monotonic_seq`（`seq` 连号、`t_ms - dt_ms` 恒定）；`test_slow_serialize_is_excluded_from_dt_ms` 追加 `t_ms - dt_ms == run 原点`；`round_clock::tests::model_deltas_only_count_and_produce_no_records`。集成 `t5_*` 在真实运行上复核结构 |
| **T4** 队列满 | ✅ | 单元：`debug_log::tests::test_queue_full_drops_but_written_lines_are_complete`（慢 writer、`dropped > 0`、每行完整） |
| **T5** 不 flush 直接退出 | ⚠️ P1 | P0 验证：正常 flush 后文件以完整 JSON 行结尾且末行为 `run_end`（`t5_*`）；**模型错误 / 持久化失败退出也写 `run_end`**（`failed_run_still_writes_run_end`）；TUI 出站失败先 flush 再返回。只剩「进程被 kill -9」按设计推迟到 P1 |
| **T6** 退出路径收敛 | ✅ | `grep -c 'process::exit' apps/coding-agent/src/main.rs` = **1**（位于 `exit_with_run_end`，`exit_with_debug` 是其简化入口）；单元 `bare_process_exit_appears_only_inside_exit_with_debug` |
| **T7** 压缩覆盖 | ✅ | 单元：`basic::tests::compaction_usage_has_compaction_purpose_and_summary_usage`。集成：`tests/debug_log.rs::t7_*` —— 小窗口 + 80 KB 输入 + 工具轮强制压缩；`purpose=turn` 两条（round 1/2）、`purpose=compaction` 一条（round 2），且压缩 usage 等于摘要调用的 usage |
| **T8** 五条变异 | ✅ | 见下表，逐条注入后对应用例变红，恢复后全绿 |
| **T9** 真实 provider 思考/正文 | ⏳ 人工 | 当前环境无凭证。需要用户运行 `--json --debug -p "<任务>"`，用 `round_end.thinking_span_ms` / `text_span_ms` 与 `round_usage.gen_ai.usage.reasoning.output_tokens` 交叉验证 |

## T8：五条变异实测

| # | 注入 | 变红的用例 | 观察 |
|---|---|---|---|
| 1 | `RoundClock` 给 `ModelTextDelta` 加一条记录 | `round_clock::tests::model_deltas_only_count_and_produce_no_records` | `assertion failed: text.lines.is_empty() && text.records.is_empty()` |
| 2 | `debug_log.rs` 把 `dt_ms` / `t_ms` 挪到序列化之后 | `debug_log::tests::test_slow_serialize_is_excluded_from_dt_ms` | `dt_ms 不得包含序列化耗时：33` |
| 3 | `try_send` 改成阻塞 `send` | `debug_log::tests::test_queue_full_drops_but_written_lines_are_complete` | 生产者挂住；**用例自身无超时**，由人工 harness 以 20 s 外部超时判红 |
| 4 | `main.rs` 一处 `exit_with_run_end` 改回裸 `process::exit` | `tests::bare_process_exit_appears_only_inside_exit_with_debug` | 计数 2 ≠ 1 |
| 5 | 删掉 `is_sensitive_key` 的匹配 | `debug_log::tests::test_redaction_applies_in_all_modes` | `left: [] right: ["Authorization", "api_key", "password", "token"]`（`none` 档） |

恢复方式：文件级备份 + 内容哈希核对（三份改动文件恢复后 sha256 与注入前一致）。
**未使用** `git checkout --`（工作树含未提交的本轮改动）。

## 全量回归

- `cargo test --workspace --no-fail-fast`：**1247 passed / 0 failed / 11 ignored**。
- `cargo clippy --all-targets`：**无新增 warning**。当前 28 个 warning 站点，基线（同一工具链）
  34 个；差异为既有站点的行号漂移、5 处 `collapsible_if` 与 2 处 `too_many_arguments` 消失
  （前者的代码在本轮已被 let-chain 改写，后者是签名合并）。
- `grep -c 'process::exit' apps/coding-agent/src/main.rs` = **1**（位于 `fn exit_with_run_end`）。
- `cargo test -p ys-coding-agent --test debug_log`：7 passed / 1 ignored（ignored 为手动 golden 生成器）；
  子进程带 30 s 看门狗，并**边跑边读** stdout/stderr（避免大输出撑满管道导致假死锁）。

## 备注

- **T2 golden 的生成环境**：stub 在 T2 用例里**不报** `completion_tokens_details`。若报，改动后的
  `RunFinished.usage` 会多出 `reasoning_tokens` 键，过滤 `RoundUsage` 后就不再与基线逐字节相同 ——
  这是 `Usage` 加法字段的预期形态，`reasoning_tokens` 三态由单元用例覆盖。
- **T1 的「yushan.log 逐字节不变」**：一次性运行本来就会追加逐轮诊断行（既有行为，与 debug 开关无关）。
  因此集成层比较「debug 开 vs 关」的归一化内容（计时数字不可复现），逐字节契约由
  `main.rs` 的单元用例（不含逐轮行）钉住。
- **T3/T4** 的注入式场景（10k 增量、慢 writer）在单元层完成；T3 的文件大小上限由真实写入
  临时文件的用例断言，集成层只复核真实运行产出的结构。
- **失败路径**：模型错误 / 持久化失败与正常完成走同一个退出出口（`exit_with_run_end`），
  因此 `run_end` 不会只在成功路径出现。
- **预算 Abort**：CLI 在 `push(Boundary::Abort)` 的同刻用可克隆 `DebugHandle` 写
  `boundary{enqueued,origin=budget}`；回合以 `Cancelled` 收束时补 `observed`。
- 基线 golden 再生成：`YUSHAN_BASELINE_BIN=<7987b4b 构建的二进制>` +
  `cargo test -p ys-coding-agent --test debug_log -- --ignored write_baseline_golden`。

## 审查后的修复（两轮 review）

- **P1**：`-p` / `--json` 的错误退出改为经 `exit_with_run_end` 收尾（失败运行也有 `run_end`）；
  TUI 的出站失败与 `run_turn_once` 外层错误先 flush 再返回。
- **P1**：CLI 预算 Abort 的 `enqueued` 记录移到 `push` 同刻（`DebugHandle`），并补 `observed`。
- **P1**：集成测试子进程加 30 s 看门狗 + 并发排空管道；补 `--debug` 不污染 stdout/stderr 的断言。
- **P2**：移除模块级 `#![allow(dead_code)]`；失败回合 `turn_end.rounds` 取事件流累计；
  `run_end` 补 `rounds` / `duration_ms` / `exit_code` / 累计 `usage`；TUI 宽限 3 s > flush 2 s；
  跨进程 `--resume` 的 `run` 递增；`worktree attach --debug` 解析顺序无关；5/6 字节字符串的
  预算截断收敛；脱敏与文件大小用例补 `none` 档 / 200 KiB 断言。
