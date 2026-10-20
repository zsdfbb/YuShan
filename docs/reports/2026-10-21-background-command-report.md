# 后台命令执行（execution-efficiency）· 切片 1 验收报告

- **日期**：2026-10-21
- **分支**：`feat/2026-10-21-background-command`
- **基线 ref**：`7d76667`（`chore(sw): baseline before 2026-10-21-background-command`）
- **设计链**：`docs/arch/execution-efficiency/`（context → design → review → ADR）
- **执行计划**：`docs/exec-plans/completed/2026-10-21-background-command.md`
- **设计计划**：`docs/design-plans/completed/2026-10-21-background-command.md`
- **ADR**：`docs/arch/execution-efficiency/adr-background-command-execution.md`（状态：accepted（切片 1 落地））

## 1. 结论

切片 1（`bash_bg` + `bash_poll` + 文件后端 + 六个清理点）**已落地并通过验收**。
全量 `cargo test --workspace` **1485 passed / 0 failed / 12 ignored**（基线 1377 passed / 12 ignored
⇒ 通过数高于基线，failed = 0）；`cargo clippy --all-targets` **无新增 warning**；
`grep -c 'process::exit' apps/coding-agent/src/main.rs` = **1**。

**三项需真实凭证的验收（T6 / T13 / T14）保留为 MANUAL_ACK_REQUIRED**，本机
`YUSHAN_API_BASE` / `YUSHAN_API_KEY` 未设置 ⇒ 不阻塞收尾。

## 2. 落地范围

### 新增文件

| 文件 | 职责 |
|---|---|
| `adapters/tools-basic/src/background.rs` | 后台命令**注册表**（进程级）：`BackgroundTasks` / `SessionSlot` / `BgHandle` / `BgState` / `BgLimits`；输出上限闸门、`kill_all(session)`、`remove_session_dir` |
| `adapters/tools-basic/src/bg_output.rs` | 输出拼装的**唯一真源**：`assemble(text, handle_label, output_path, pending_note)` |
| `adapters/tools-basic/src/bg_tools.rs` | `BashBgTool` / `BashPollTool` 两个工具 |
| `adapters/tools-basic/src/proc_group.rs` | 进程组内核（「先杀后收」「pid 复用窗口」两条告诫的**单一**落点） |
| `apps/coding-agent/tests/background_command.rs` | 集成测试（9 条用例，全链路 `bash_bg` → `bash_poll`） |

### 修改文件（要点）

| 文件 | 改动 |
|---|---|
| `crates/ys-tool/src/context.rs` | `ToolContext` 加**只读**的 `tool_budget` 字段 + `with_tool_budget` |
| `crates/ys-tool/src/truncate.rs` | 新增 `head_tail` 纯函数（头尾混合裁剪）+ `omitted_bytes` 口径与恒等式 |
| `crates/ys-loop/src/basic.rs` | 两条路径（并行批 / 串行批）构造 `ToolContext` 时**各自**填本工具的预算 |
| `adapters/tools-basic/src/bash.rs` | 工具内**始终**有 deadline；钳位**严格早于**外层；boundary / 无 boundary 两条路径都**先杀进程组后收尸** |
| `adapters/tools-basic/src/exec.rs` | 与 `bash.rs` 共享同一份进程组内核 |
| `apps/coding-agent/src/assembly.rs` | 父表插入 `bash_bg` + `bash_poll`（排在 `subagent` **之前**）⇒ **父 12 元 / 子 9 元** |
| `apps/coding-agent/src/wiring.rs` | `BackgroundContext`（进程级注册表 + 会话槽）；`/new` / `/resume` 后同步会话槽 |
| `apps/coding-agent/src/app_loop.rs` | 四个清理点 + **共享 sweeper**（全局 1 个，100ms tick）+ 回合末后台计数（只进 debug 日志） |
| `apps/coding-agent/src/main.rs` | 启动期建表 + 落盘根清扫；`std::process::exit` **之前**显式 `kill_all(全部)` |
| `apps/coding-agent/src/config.rs` | 窗口值与**来源**成对产出（四级来源） |
| `apps/coding-agent/src/capabilities.rs` | `/model` 输出追加 `Context window: <值> (source: <标签>)`（走 `Outbound::Output` ⇒ 绝不 print） |
| `apps/coding-agent/src/debug_log.rs` / `round_clock.rs` | `tool_call` 记录加 **`command_fp`**（重复命令的稳定指纹）；四个指标可从日志直读 |
| `apps/coding-agent/src/prompt.rs` | 两条后台命令 guideline（**按工具名派生**，子表不含 bg 工具 ⇒ 子 agent 看不到） |
| `apps/coding-agent/tests/e2e_bench.rs` | 协议面 A/B 的测量开关（`YUSHAN_BENCH_PROTOCOL`）+ 记录表加「协议」「质量」两列 |

### 文档

- `docs/arch/execution-efficiency/design.md` —— 按落地事实回写（粗/精确闸门分工、末行两态规则、
  `omitted_bytes` 新定义与恒等式、单行超预算取前缀、`bg_output` 唯一真源、清理点行号）。
- `docs/arch/execution-efficiency/adr-background-command-execution.md` —— 追加第 7 条决策（输出上限拆两个闸门）。
- `docs/design-final/coding-agent-tui.md` —— §18 扩为 7 小节（父/子表、六个清理点、窗口来源、
  两个新工具、交付语义、两个闸门、展示裁剪）。
- `docs/design.md` §12 —— 父表 10 → 12；补 `command_fp` 与 `ys_tool::head_tail` 两条测试矩阵行。
- `docs/arch/agent-observability/design.md` —— `tool_call` 字段表补 `command_fp`。
- `docs/TODO.md` —— A5（execution-efficiency 的落地）**已落地 ⇒ 删除**；§D 登记
  `toolset.rs` 的 `PATH` 改写波及不持锁用例的既有隐患。
- `AGENTS.md` —— 登记 `YUSHAN_BG_DIR`（须绝对路径）。

## 3. 验收项逐条核对（T1–T14）

| 项 | 结论 | 证据 |
|---|---|---|
| **T1** 预算进工具且工具内始终有 deadline | ✅ | `crates/ys-tool/src/context.rs:143`（默认 `None`）、`:149`（`with_tool_budget`）；`crates/ys-loop/src/basic.rs:47` `tool_budget(tool, limits)` 三级链 |
| **T2** 钳位**严格**小于外层 | ✅ | `adapters/tools-basic/src/bash.rs:1626` `test_effective_deadline_is_strictly_earlier_than_the_outer_budget`（变异：把 `min` 改成取模型值 ⇒ 变红） |
| **T3** 不传 `timeout` 也被砍且进程组被杀 | ✅ | `adapters/tools-basic/src/bash.rs:1026` / `:1036`（leader 先退出的两条路径）+ 孙进程**不存在**断言（不看错误文案） |
| **T4** 无 boundary 路径超时不留孤儿 | ✅ | `adapters/tools-basic/src/bash.rs:882` `test_bash_timeout_kills_grandchild_without_boundary`、`:905` `assert_grandchild_killed`（进程存在性断言） |
| **T5** 窗口声明可自证 | ✅ | `apps/coding-agent/src/config.rs:280` `context_window_with_source_for`（一次判定同时给值与来源）；`:998` 四级来源逐级；`apps/coding-agent/src/capabilities.rs:1102` `/model` 输出含窗口值与来源 |
| **T6** 长会话不再被过早压缩 | ⏸ **MANUAL_ACK_REQUIRED** | 需真实模型跑到 150k token；本机无凭证 |
| **T7** 两条 guideline 按工具名派生 | ✅ | `apps/coding-agent/src/prompt.rs:1187`（两个工具**同时**在场才出现）、`:1293`（子 prompt 不含）；变异：守卫改无条件 ⇒ 两条同时变红 |
| **T8** `command_fp` 区分同命令与异命令 | ✅ | `apps/coding-agent/src/debug_log.rs:2440`（等长异串指纹不同）、`:2473`（投影截断后不变）、`:2541`（与 `args_mode` 无关）、`:2558`（脱敏后不变）；`round_clock.rs:1157` 记录里**只有指纹、没有命令正文** |
| **T9** 四个指标可从日志直读 | ✅ | `apps/coding-agent/src/round_clock.rs:1197` `debug_records_expose_repeat_count_and_duration_buckets`（重复命令数 / 真命令耗时 / 按工具名分桶 / 工具阻塞占比） |
| **T10** 两个工具的契约 | ✅ | `adapters/tools-basic/src/bg_tools.rs:649`（spec 逐字断言）、`:696`（超时与并发声明）、`:736`（未知句柄）、`:778`（非零退码原样带出 ⇒ `is_error == true`）；**被 harness 杀掉**与**非零退码**措辞不同 |
| **T11** 交付语义三条 + 完整行边界 | ✅ | `bg_tools.rs:854`（未送达字节 + 完整路径）、`:891`（运行中半行不交付、报待交付字节）、`crates/ys-tool/src/truncate.rs:654`（恒等式断言）、`:695`（单行前缀）、`:716`（尾随换行记账）；`apps/coding-agent/tests/background_command.rs` 的「不重复交付」两段断言 |
| **T12** 六个清理点 + 孤儿进程数 = 0 | ✅ | `apps/coding-agent/src/app_loop.rs:4137`（abort）、`:4165`（回合预算到点）、`:4213`（`/new` + 删输出目录）、`:4255`（`/resume`）、`:4305`（**跨会话不误杀**）、`:4343`（共享 sweeper 收尸）；进程退出点在 `main.rs:2057`（`exit_with_run_end` 内、`std::process::exit` 之前）；panic 兜底是 `impl Drop for BackgroundTasks` |
| **T13** 切片 1 前后对照 | ⏸ **MANUAL_ACK_REQUIRED** | 需真实模型跑同一批 prompt 前后各一遍；本机无凭证 |
| **T14** 协议面 A/B | ⏸ **MANUAL_ACK_REQUIRED** | 需真实模型；本机无凭证。测量开关已备好（`YUSHAN_BENCH_PROTOCOL`） |

## 4. 变异测试

**每个单元的关键断言都做过「注入 bug → 用例变红 → 精确还原」的验证**（还原用 `cp` 备份 + `diff`
逐字节校验，**未用** `git checkout` / `stash` / `reset`）。

| 单元 | 注入 | 变红 |
|---|---|---|
| U1 / U3 | `tool_budget` 不填 / 填错工具的预算 | 对应用例红 |
| U2 | 进程组「先杀后收」反序 | 孙进程存活断言红 |
| U4 | 去掉无 boundary 路径的进程组杀 / boundary 路径先收尸后杀 | `bash.rs` 的孙进程断言红 |
| U9 | `omitted_bytes` 恒置 0 | ys-tool **5 红** + tools-basic **4 红** |
| U9 | 单行超预算退回「不返回任何内容」 | ys-tool **2 红** + tools-basic **1 红** |
| U11 | 终态不交付末行（退回旧行为） | **3 红** |
| U11 | 未送达字节数恒置 0 | **2 红** |
| U11 | `max_running` 文案退回「poll or kill」 | **1 红** |
| U11 | 在跑计数改成条目总数 | **1 红** |
| U12′ | 父表追加段顺序颠倒 | **3 红**（前缀 + 末位 + 固定成本） |
| U12′ | 清理调用移出 `exit_with_run_end` | 位置断言红 |
| U12′ | `skipped_bytes` 恒置 0 | bg_output 断言红 |
| U13 | m1 `kill_all` 去掉会话过滤 | `cleaning_one_session_does_not_kill_another_session` 红 |
| U13 | m2 删 `/new` 臂的清理 | `new_session_kills_background_processes_and_removes_the_output_directory` 红 |
| U13 | m3 删 abort 臂的清理 | `abort_kills_the_background_processes_of_the_current_session` 红 |
| U13 | m4 删预算臂的清理 | `budget_exhaustion_kills_the_background_processes_of_the_current_session` 红 |
| U13 | m5 删 `/resume` 臂的清理 | `resume_session_kills_the_old_session_background_processes` 红 |
| U13 | m6 回合末计数改成进模型上下文 | `turn_end_records_the_running_background_count_in_debug_only` 红 |
| U13 | m7 不启共享 sweeper | `the_shared_sweeper_reaps_finished_commands` 红 |
| U15 | `advance_consumed` 不推进 | 「不重复交付」红 |
| U15 | 非零退码不判错 | `is_error` 断言红 |
| U17 | 协议开关三态退化 | 两条协议用例红 |
| U6 | guideline 守卫改无条件 | 两条 guideline 用例红 |

## 5. 回归数据

```
$ cargo test --workspace
passed=1485 failed=0 ignored=12

$ cargo clippy --workspace --all-targets
既有 warning 28 处，逐条核对**均不在本次改动内**
（`unnecessary use of clone` 15、`if` 可折叠 8、`bool::then` 1、参数过多 1、
 `impl` 可派生 1、`sort_by_key` 1、`filter(..).next_back()` 1）
落点：ys-protocol/lifecycle.rs:13、tools-web/store.rs:199、
 tools-basic/{bash.rs:184,find.rs:169,170}、ys-loop/basic.rs:1887、
 tools-web/tests/harvest.rs:72、session_cli.rs:331、prompt.rs:424、sessions.rs:88、
 main.rs:{1241,1427,1700,4556,4824}、app_loop.rs:2842、skills.rs（11 处）

$ grep -c 'process::exit' apps/coding-agent/src/main.rs
1

$ grep -c 'YUSHAN_BG_DIR' AGENTS.md
1
```

**孤儿进程自查**：`pgrep -fl` 无残留；测试临时目录逐个自删。

## 6. 实现与设计的分歧（as-built 如实记录，不藏）

| # | 设计原口径 | 落地口径 | 原因 |
|---|---|---|---|
| 1 | 用 `ulimit -f` 表达「单命令输出 2 MiB 上限」 | 拆成**精确闸门**（2 MiB，sweeper 的 `stat`，harness 语义）与**粗闸门**（`ulimit -f` = `max(2 MiB, 64 MiB)`，内核语义） | `ulimit -f` 是**进程级文件写入上限**，取 2 MiB 会把 `cargo build` 写 `target/` 的正常构建一起杀掉 |
| 2 | 「单次轮询读 ≤ 50 KiB」 | 运行中 ≤ 50 KiB；**终态收尾读 ≤ `min(输出上限, 8 MiB)`** | 终态要把末段**整段**交付，否则末行（无换行结尾）永久丢失 ⇒ 展示预算（2000 行 / 50 KiB）仍由 `head_tail` 兜住、未放宽 |
| 3 | 输出的组装在 `bg_tools` 内 | 组装收敛到 `bg_output::assemble`（**唯一真源**） | 两份拷贝必然漂移；措辞（读域知识）留在 `bg_tools`，**插入位置**归 `bg_output` |
| 4 | `head_tail` 对单行超预算「整段让位」 | 头部给出该行的**前缀**（≤ 头部预算、字符边界安全） | 整段让位 ⇒ 60 KiB 单行的**正文为空**，模型只能靠「未送达 N 字节」去 `read` 文件 |
| 5 | `omitted_bytes` = 「被跳过的那段」 | = **源文本中未出现在展示内容里的字节数**（含尾随换行）⇒ 恒等式 `omitted_bytes + head.len() + tail.len() == 源文本字节数` | 「跳过量 = 实际丢弃量」原本差 1 字节（尾随换行） |
| 6 | 退出清理点写在 `exit_with_debug` | 写在 `exit_with_run_end`（含退出原语的**唯一**函数） | 有 6 处直接调用 `exit_with_run_end` 会绕过 `exit_with_debug` ⇒ 放后者会漏掉那 6 条路径 |
| 7 | 启动期清扫「全清」落盘根 | 只清**超龄 7 天**的目录 | 默认档 `<temp>/yushan-bg` 可能被**同机并行的另一个实例**共用 ⇒ 无条件清空会删掉别人**在跑**的命令的输出文件 |
| 8 | `AgentFactory::new` 加参数 | 改用 builder `with_backgrounds(context)` | `app_loop.rs` 的测试工厂也调 `AgentFactory::new`，改签名会编译不过；生产装配路径恒注入 ⇒ 父表恒为 12 元 |

## 7. 已知限制与残留

1. **模型面没有 kill 入口** —— `bash_bg` 只能 start、`bash_poll` 只能 poll ⇒
   模型起满 4 条长命令后**只能等**（`wait_seconds` ≤ 20s 一轮一轮等），无法主动中止。
   报错文案已改成**可执行**的说法（「wait for one to finish, then start another」）。
   根治需要产品层面决定（加第三个工具 / `bash_poll` 加 `kill` 参数 / `timeout` 做成模型可调的更短值）——
   三个方案都会动父表元素数或子表前缀不变量，**本轮未做**。
2. **`bash` 工具的既有裁剪口径未动** —— `bash` 共用既有的 `tail`（不是 `head_tail`），
   单行超 50 KiB 时同样是空正文。`bash` **一字不改**是本设计的承重约束 ⇒ 未越界修。
3. **`toolset.rs` 的 `PATH` 改写会波及不持锁的用例**（既有隐患）—— 已登记 `docs/TODO.md` §D。
4. **T6 / T13 / T14 未验收** —— 需真实凭证。

## 8. 后续

- 用户持凭证时补跑 T6 / T13 / T14，并把 `docs/exec-plans/completed/2026-10-21-background-command.md`
  验证清单里对应三项改为 `[x]`。
- 切片 2（协议面 A/B）待 T14 出结论后再排。
