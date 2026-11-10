# 程序化入口契约（headless worker）— As-built

> 性质：**实际建成记录**。设计见 `docs/arch/headless-worker-api/design.md`（+ `context.md`），
> 决策见 `docs/arch/headless-worker-api/adr-001-headless-worker-contract.md`，
> 审查见 `docs/arch/headless-worker-api/review.md`（R1–R7，全部回修进设计）。
> 对外说明见 `apps/coding-agent/README.md`「退出码与输出约定」（`:267`）。
> 日期：2026-11-10 ｜ 基线 `3d991fb`

## 一句话

`-p` / `--json` 从「人也能用」提升为「**进程可编排**」，服务 `../YuShan-lark-agent-bridge`
（飞书 ↔ 本地 agent 桥）：每条用户消息 spawn 一个短命 `--json` 进程跑**一个回合**。
三件事 —— ① `--json` 首行是会话记录 `{"type":"session","id":…}`（先于任何 `Envelope`），
调用方据此拿到可 `--resume` 的 id；② `SIGTERM` 不再被内核硬杀，改为 abort 本回合并以
可重试码退出（后台进程组收净、debug 尾部完好）；③ 退出码分三档
`0` / `1` / `75`（`EX_TEMPFAIL`），只回答「能不能重试」。

## 不变量与实现落点

| # | 不变量 | 落点 |
|---|---|---|
| **I1** | 会话行先于任何 `Envelope`，且晚于 `ensure_configured` | `main.rs:2645` `ensure_configured(...)?` → `:2646` `emit_session_line(wiring.session_path())`，均在 `loop`（`:2660`）之前 |
| **I2** | 会话行**只在** `--json` 出现，`-p` 的 stdout 仍只有正文 | 唯一调用点 `main.rs:2646`（`run_json_mode` 内）；不进两条入口共用的 `consume_events` |
| **I3** | id 取会话文件名的 stem，不读文件 | `main.rs:2789` `session_record()` → `session_id_of`（`:1556`）；`Wiring` 构造期已算定路径 |
| **I4** | 无落盘路径（内存会话）⇒ 不输出该行，不编造 id | `main.rs:2789-2792` `path.and_then(session_id_of)?` ⇒ `None` ⇒ `emit_session_line` 不打印 |
| **I5** | `--resume` 时首行回显**传入的那个** id | id 来自 `wiring.session_path()`；`persistent_at(Some(path))` 使 stem 恒等于传入 id（`tests/headless_contract.rs:534` 钉住） |
| **I6** | 信号只投 `Abort`，不退进程、不改退出码 | `main.rs:2211-2215` 分支体仅 `signal_aborted = true` + `eprintln!` + `boundary.push(Boundary::Abort)` |
| **I7** | 信号分支与预算分支**对称**：都只触发一次 | `main.rs:2168` `budget_exhausted` / `:2173` `signal_aborted`，各自作 `if` 前置条件（`:2178`、`:2211`） |
| **I8** | 监听器在 `loop` **之前**建一次，逐回合 reborrow | `main.rs:2479`（`run_print_mode`）、`:2658`（`run_json_mode`）；reborrow 于 `:2508` / `:2683` |
| **I9** | 收尾与退出码全走既有路径，无新退出路径 | `main.rs:2845` `exit_with_run_end`（内部 `:2865` 清理 → `:2866` 退出原语）；裸 `process::exit` 全文件**恰好一处**（单测 `main.rs:7484`） |
| **I10** | 退出码三档穷举，`#[non_exhaustive]` 兜底不静默吞掉失败 | `main.rs:2804` `single_turn_exit_code`：六个变体显式归类 + `#[allow(unreachable_patterns)] _ => 0` |
| **I11** | 不碰 `crates/`；`--json` 既有行的字节不变 | `git diff 3d991fb -- crates/` 为空；golden 仍 4 行（`tests/golden/json_no_debug_baseline.jsonl`） |

## 改动清单（按层）

全部落在 `apps/coding-agent/`。**`crates/` 零改动**（`git diff 3d991fb -- crates/` 空输出）。

| 文件 | 改动 |
|---|---|
| `Cargo.toml:29` | tokio features 追加 `"signal"` |
| `src/main.rs` | `SingleTurnInputs` 加 `#[cfg(unix)] signal` 字段（`:2059`）；`sigterm_listener()`（`:2069`）、`recv_sigterm()`（`:2084`）两个 `#[cfg(unix)]` 帮助函数；`select!` 三分支（`:2175-2216`）；两处 `let mut sigterm = sigterm_listener();`（`:2479` / `:2658`）+ 两处 reborrow 传参（`:2508` / `:2683`）；`emit_session_line`（`:2779`）/ `session_record`（`:2789`），调用点 `:2646`；`single_turn_exit_code` 改三档（`:2804`）；三条新单测（`:5321`、`:5387`，另 goldens 相关的 `:7484` / `:7512` 既有） |
| `tests/headless_contract.rs` | **新建**，732 行，6 个进程级用例（`#[cfg(unix)]` 两个），自带回环 mock HTTP 服务；**无 `#[ignore]`**（全部默认跑） |
| `tests/debug_log.rs` | `without_round_usage` 兼丢「无 `event` 字段」的行（`:591`）；`envelopes()` 改为显式分流而非 `filter`（`:559`）；`t5` 断言首行是会话记录（`:1064-1071`） |
| `README.md` | 三档退出码表（`:267-277`）+ `--json` 首行说明（`:280-288`）+ 三模式表 `:42` |
| `docs/design-final/turn-output-and-feedback.md:47-50` | 退出码同步为三档（R7 的函数名漂移同处修掉：`:50` 现为 `exit_with_run_end`） |

## 退出码契约（对外，最重要的表）

| 码 | 含义 | `StopReason` | 调用方该做什么 |
|---|---|---|---|
| **`0`** | 有可交差的输出 | `Completed` / `MaxRounds` / `Truncated` | 交付结果 |
| **`1`** | 故障，**不可重试**（直接重试只会重现） | `Failed`；以及 `ensure_configured` 早退等非 `StopReason` 失败（`tests/headless_contract.rs:648` 钉住） | 报警 |
| **`75`**（`EX_TEMPFAIL`） | 暂时没跑完，**可重试** | `Cancelled`（预算到点 / `SIGTERM`）/ `NoOutput` | 重试 |

分工：**退出码只答「能不能重试」**，细粒度原因由终局事件的 `stop_reason` 给。
`75` 不占 `2`（留给 `goal-command` 的「未达成」），也不用 `143` —— 进程是**自己收尾后主动退出**，
不是被内核杀掉。按 `!= 0` 判断的调用方不受影响；按 `== 1` 判断的会漏 `Cancelled` / `NoOutput`。

## `SIGTERM` 收尾的完整时序

```
① bridge 发 SIGTERM（回合正卡在模型调用或工具里）
② tokio 信号分支就绪（I8：监听器建在 loop 之前，故任何回合内都能收到）
③ signal_aborted = true；eprintln!("SIGTERM received — aborting this turn")
④ boundary.push(Boundary::Abort)                        ← 分支体到此为止
⑤ 回合在下一次轮边界探针看到 is_aborted() ⇒ RunFinished{Cancelled}
⑥ 工具的 abort 链路同步收手：bash/exec 每 100ms 轮询并杀整个进程组
⑦ join! 自然返回（回合 side 完成 + 消费循环见终局事件即终止）
⑧ continue 循环体照常跑到 loop 头部；advance 见 Cancelled ⇒ Stop(Aborted)，不再起追问回合
⑨ exit_with_run_end：flush debug（写 run_end，exit_code=75）→ kill_backgrounds_on_exit → process::exit(75)
```

关键点：**没有第 ④ 步之外的任何动作**。分支不抢 `&mut DebugSink`（`join!` 期间它归
`consume_events` 独占），不 `process::exit`，不加「信号专用」标志位 —— 退出码由既有的
`single_turn_exit_code` 从 `Cancelled` 自然得出 `75`。

## 与设计的偏差

### 偏差 1：设计的「固定 5 秒宽限」在代码里**没有对应计时器**

`design.md:113` 写「固定 5 秒宽限，不做配置」，`design.md:49` 论证「`push(Abort)` 后百毫秒级
就能收干净，5 秒宽限有 50× 余量」。**实际实现没有 5 秒计时器**：`main.rs:2211-2215` 只
`push(Abort)`，随后等回合自己在轮边界收手。

所谓「5 秒」是**对 bridge 侧 `stopGraceMs` 的承诺**（bridge 的宽限应当大于我们收尾所需时间），
不是本进程里强制执行的上限。工具侧 abort 轮询是 100ms 级 + 杀整个进程组，故实际收尾远小于 5s。

**这是有意的实现简化**：写一个强制的 5s 计时器会引入「计时器到点就硬退」的**新退出路径**，
与 I9（唯一退出落点）直接冲突。代价是「工具永久卡住时本进程不会自我了断」，那一层由 bridge
的宽限期兜。

bridge 侧的实际行为（`../YuShan-lark-agent-bridge` 源码核实，非推断）：
`stopGraceMs` 默认 **5000ms**（`src/agent/claude/adapter.ts:144`）；`stop()` 先 `kill('SIGTERM')`
（`:150`），计时器到点若子进程仍未退出则 `kill('SIGKILL')`（`:159`）。**双方的 5 秒是同一个数**
—— 我们收尾远快于此，故有余量；一旦本进程真被卡到 5 秒，SIGKILL 会让 debug 尾部丢失（与硬停同形）。
**待复访**：若将来出现「工具可以无限久且不响应 `Abort`」的形态，这个判断需要重估。

### 偏差 2：`select!` 分支门控形式与设计草图不同

`design.md:119-123` 的草图是 `_ = sigterm.recv(), if !signal_aborted =>`。实际写法：

```rust
_ = async {
    #[cfg(unix)]
    recv_sigterm(&mut signal).await;
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}, if !signal_aborted => { … }
```

语义一致（`if !signal_aborted` 守卫在），但**平台门控的位置不同**：`tokio::select!` 的分支规则
以 `$p:pat = $f:expr` 开头，**不接受**位置属性 —— `#[cfg(unix)] _ = … =>` 报
`no rules expected }`（tokio 1.54.1 实测）。故门控放进分支的未来表达式内（`main.rs:2203-2210`
的注释写了这条理由）。另外分支走 `recv_sigterm(&mut signal)` 而非 `sigterm.recv()`：
`None`（测试路径）时 `pending()` 永不就绪，省掉分支外的 `Option` 判空。

### 偏差 3：`cfg(unix)` 覆盖不完整 —— 本 crate 本就非 unix 不可编译

`SingleTurnInputs.signal` 的字段类型是 `Option<&'a mut tokio::signal::unix::Signal>`（`main.rs:2060`），
这个**类型路径**本身不受 `#[cfg(unix)]` 管辖。因此 `cfg(not(unix))` 下：字段类型未定义、
`sigterm_listener()` 调用点未定义、两处 `signal: sigterm.as_mut()`（`:2508` / `:2683`）未定义
—— 整个 crate 编不过。

**结论**：本设计**不承诺** Windows 支持。`cfg(unix)` 与既有风格一致，且已做到**四处对称**
（字段 `:2059` / 解构 `:2116` / `let mut signal = signal` `:2163` / 分支未来表达式 `:2207`）；
非 unix 的 `pending()` 分支（`:2210`）是为「将来若有人补齐类型」预留的形状，不是可用的移植路径。

## 实测数字

| 检查 | 命令 | 结果 |
|---|---|---|
| 全量测试 | `./scripts/par-test.sh` | **42 个测试二进制全部通过** |
| 新契约用例 | `cargo test -p ys-coding-agent --test headless_contract` | **6 passed**（4.21s） |
| 既有 debug 用例 | `cargo test -p ys-coding-agent --test debug_log` | **9 passed / 1 ignored**（2.74s；`write_baseline_golden` 手动生成基线） |
| 静态检查 | `cargo clippy -p ys-coding-agent --all-targets` | 无告警 |
| 核心库零改动 | `git diff 3d991fb -- crates/` | **空** |
| golden 未动 | `wc -l apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl` | **4 行**，字节未变 |

## 测试钉子（用例 → 不变量 → 变异自检）

### 进程级（`tests/headless_contract.rs`，真实二进制 + 回环 mock）

| 用例 | 不变量 | 变异自检 |
|---|---|---|
| `json_first_line_is_session_record`（`:457`） | I1 / I3 / I5 —— 首行 `type=session`、id 非空且等于落盘文件名，其后每行都带 `event` | 首行写进共用的 `consume_events` ⇒「`-p` 无该行」用例红；挪到 `ensure_configured` 之前 ⇒ 无配置用例红 |
| `print_stdout_has_no_session_record`（`:506`） | I2 | 同上（两条用例从两侧夹住同一个错误） |
| `json_resume_first_line_echoes_requested_id`（`:534`） | I5 —— 首行 id == `--resume` 传入的 id，且不新建第二个文件 | 首行改取「本次新建的 id」⇒ 红 |
| `sigterm_aborts_turn_and_exits_75`（`:581`，`#[cfg(unix)]`） | I6 / I9 —— 码 `Some(75)`（`None` = 被信号直接杀死）、stderr 有 `SIGTERM`、回合仍以 `RunFinished` 收场 | 分支改成直接退进程 ⇒ 码不对；监听器改每回合新建 ⇒ 信号落进无人 `recv` 的窗口 ⇒ 超时挂死 |
| `no_config_early_exit_prints_no_session_record`（`:633`） | I1 的下界 —— `ensure_configured` 早退时 stdout **必须为空** | `emit_session_line` 前移 ⇒ 红 |
| `sigterm_still_writes_debug_run_end`（`:671`，`#[cfg(unix)]`） | I9 —— debug 文件以完整行结尾，最后一行为 `run_end` 且 `exit_code == 75` | 分支改成 `process::exit` ⇒ `run_end` 缺席或码不对 |

用例工程上的三条要点：① 信号送达时刻由 `MockServer::wait_for_first_request()`（`:264`）钉在
「模型请求在飞」之后，不靠墙钟 sleep；② 每个用例套 `CHILD_TIMEOUT = 30s`（`:28`），超时先
`kill` 再 panic；③ mock 对启动期的 `GET /v1/models` 立即应答（`:289-293`），否则信号会落在
监听器建好之前。

### 单测

| 用例 | 不变量 | 变异自检 |
|---|---|---|
| `main.rs:5321` `single_turn_exit_code_has_three_tiers` | I10 | `Cancelled` 与 `Failed` 同码 ⇒ 第一条断言红 |
| `main.rs:5387` `session_record_prints_id_from_file_stem` | I3 / I4 | id 改成编造值 / 内存会话也输出 ⇒ 红 |
| `main.rs:7484` `bare_process_exit_appears_only_inside_exit_with_debug` | I9 | 任何一处裸 `process::exit` ⇒ 计数变 2 ⇒ 红 |
| `main.rs:7512` `background_cleanup_runs_before_the_process_exit_primitive` | I9 | 清理移到退出原语之后 ⇒ 位置断言红 |
| `debug_log.rs:859` `t2_…_matches_baseline_golden` | I11 —— 过滤后与 4 行 golden 逐字节相同 | 见下「golden 的正确修法」 |
| `debug_log.rs:1045` `t5_…_complete_and_structured` | I1 / I2 —— debug 不进 stdout、首行为会话记录 | stdout 混入 debug 行 ⇒ 红 |

### 两个必须记住的测试决策

**① golden 的正确修法是改过滤器，不改 golden（review R1）。** `without_round_usage`
（`debug_log.rs:591`）扩展为「兼丢无 `event` 字段的行」，golden 本体保持 4 行不变。理由：
golden 是**基线二进制的产物**（`write_baseline_golden` 要求 `YUSHAN_BASELINE_BIN`，
`debug_log.rs:926-936`），它本来就没有 session 行；改 golden 等于伪造基线证据，
该测试拦输出漂移的能力随之消失。

**② `envelopes()` 只放行已知的那一行（`debug_log.rs:559`）。** 非法 JSON ⇒ panic；
无 `event` 且 `type != "session"` ⇒ panic。变异实测：往 stdout 注入 `NOT-JSON-LEAK` 后
`t2` / `t5` / `t7` **三个都红**；若改用 `filter` 默默丢弃，则只有 `t5` 红（`t2` / `t7` 失明）。

## 已知缺口 / 复访项

| 项 | 现状 | 影响 |
|---|---|---|
| `sigterm_listener()` 注册失败退化为**默认处置**（`main.rs:2069-2077`：打 stderr、返回 `None` ⇒ `recv_sigterm` 永不就绪） | 环境问题（信号 fd 耗尽）；进程按普通路径跑到完成 | 退出码只能分三档：监听器缺失时 `SIGTERM` 走**默认处置**（进程被杀死 ⇒ 无退出码），与「bridge 自己 `SIGKILL`」「启动期信号」不可区分；唯一能指向它的证据是 stderr 的 `failed to install SIGTERM handler` 行（review P2） |
| 偏差 1 的「无 5s 上限」 | 有意简化，见上 | 工具永久卡住时靠 bridge 宽限期兜；**待复访** |
| `sigterm_aborts_turn_and_exits_75` 的 2s 余量（`SLOW_RESPONSE_DELAY`，`headless_contract.rs:34`） | 信号在「模型请求在飞」之后立即发出，mock 要在 2s 后才回包 | 极端负载下若子进程启动 + 请求发出就耗掉大半余量，用例可能测到「回合已收场后的信号」（码 `0`）⇒ 退化为弱测试。review 归 P2 |
| `SIGTERM` 落在回合刚 `Completed` 之后 | 退出码 `0`（design §8 已归类） | 对 bridge 是「成功了只是慢」，**不加标志强改 `75`** |
| 「有 id 无文件」窗口（`Abort` 探针在追加用户消息之前） | `--resume` 明确报「找不到会话」 | 已接受的响亮失败（ADR-001 决策 2） |
| 跨平台 | 不承诺 Windows（偏差 3） | 非 unix 编不过 |

## 参考

- 需求与拍板：`docs/arch/headless-worker-api/context.md`（Q1–Q8，`:140-153`）
- 设计：`docs/arch/headless-worker-api/design.md`（§4.3 退出码 / §4.4 SIGTERM / §5 详细设计）
- 决策：`docs/arch/headless-worker-api/adr-001-headless-worker-contract.md`（4 条）
- 审查：`docs/arch/headless-worker-api/review.md`（R1–R7）
- 对外契约：`apps/coding-agent/README.md` §退出码与输出约定（`:267`）
- 相邻 as-built：`docs/design-final/turn-output-and-feedback.md`（退出码语义的三处可见性，`:47-50`）
