# headless-worker-api — 设计

> 日期：2026-11-10
> 上下文：[`context.md`](./context.md)
> 决策：[`adr-001-headless-worker-contract.md`](./adr-001-headless-worker-contract.md)
> 状态：设计完成，待实现

---

## 1. 目标与判据

把 `-p` / `--json` 从「人也能用」提升为「**进程可编排**」。消费方是
`../YuShan-lark-agent-bridge`（飞书 ↔ 本地 agent 桥）：长期驻留的 TS 进程，
对每条用户消息 spawn 一个短命 Rust 进程。

| # | 判据 | 现在的失败形态 |
|---|---|---|
| G1 | 首次回合后，调用方能拿到可 `--resume` 的会话 id | 拿不到 ⇒ 无法续跑 |
| G2 | 续跑回合能跑通，上下文连续 | 同上 |
| G3 | `SIGTERM` 后干净退出：无孤儿进程组、debug 尾部完好 | 硬停，`kill_backgrounds_on_exit` 不跑 |
| G4 | 调用方能区分「可重试」与「不可重试」失败 | 退出码只有 `0` / `1`，`1` 三义 |

**非目标**：MCP、系统推回合、队列/去重（归 bridge）、IPC/HTTP 入口、stdin 提示词、
同 id 并发锁、指定会话 id。理由见 `context.md` §范围外与 §9。

---

## 2. 决定设计的三条既有事实

调研确认，这三条让设计比初稿简单得多：

1. **会话 id 在构造期就已确定**。`persistent_at`（`wiring.rs:137`）**立即**算出
   `sessions_dir.join(format!("{}.jsonl", new_session_id()))` 并存入 `session_path`，
   虽然文件要等首次 `append` 才创建。⇒ **不需要为拿 id 而饿汉落盘**。
2. **`Abort` 探针在追加用户消息之前**。`basic.rs:495` 的 Step 1 先查 `is_aborted()`，
   命中就 `RunFinished{Cancelled, rounds:0}` 并返回；Step 2（`basic.rs:518`）才
   `session.append(input.message)`。⇒ 只有「回合开始前就被中止」才可能没有文件。
3. **`-p` 与 `--json` 共用 `consume_events`，但写出回调不同**
   （`main.rs:2427` vs `:2582`）；`--json` 的唯一 stdout 写点是
   `write_json_envelope`（`main.rs:579`）。⇒ 新行只要不写进 `consume_events`，
   就不会污染 `-p`。

4. **工具的 abort 链路是通的**。`bash` 每 100ms 轮询 `is_aborted()`
   （`bash.rs:110-118`）命中即 `terminate()` 杀**整个进程组**
   （`:386-398` → `proc_group.rs:25`）；`exec` 类（`rg`/`fd`/`grep`/`find`）同样
   （`exec.rs:326`）；`websearch`/`webfetch` 与网络流赛跑（`net.rs:285-305`）；
   `subagent` 经 `ChildBoundary` 透传（`subagent.rs:156-157`）。
   且 `-p`/`--json` **确实挂载** boundary（`main.rs:2086`）。
   ⇒ `push(Abort)` 后百毫秒级就能收干净，5 秒宽限有 50× 余量。

---

## 3. 同类软件调研

| 来源 | 事实 | 对本设计的用法 |
|---|---|---|
| **claude CLI**（`--output-format stream-json`） | 首行 `{"type":"system","subtype":"init","session_id":…,"cwd":…,"model":…}`，**独立于消息事件** | 直接采用：会话标识作为独立首行记录 |
| **codex CLI**（`exec --json`） | 首行 `thread.started` 携带 thread id | 同构佐证，两家独立收敛 |
| **systemd** `Type=notify` | 服务必须在宽限期内响应 `SIGTERM`，否则 `SIGKILL` | 宽限固定值优于可配：超时方是调用者 |
| **MCP** | 先 `initialize` 握手再发业务 | 「先握手再干活」的通用性佐证；本仓只做一行记录 |

**不适用**：claude/codex 的 `session_id` 是**远端服务端会话**，本仓是**本地文件 stem**
⇒ 我们额外承担「文件必须存在」这条（见 §4.2）。

---

## 4. 候选方案与决策

### 4.1 会话 id 的载体

| 维度 | 甲：新增 `AgentEvent` 变体 | 乙：复用 `Envelope.source` | 丙：`RunFinished` 加字段 | **丁：独立首行记录** ✅ |
|---|---|---|---|---|
| 语义 | ⚠️ 层级错：id 是进程↔调用方握手，不是回合内事件 | ❌ 「谁发的」被挪成「哪个会话」 | ⚠️ 回合结束才可得 | ✅ 握手信息独立成行 |
| 改动面 | `crates/ys-event` + ~200 处 `match`（`RunFinished` 在 15 文件约 66 处构造） | `crates/ys-protocol` | 66 处构造 + 200 处匹配 | `main.rs` 一处 + 测试 |
| 碰核心 crate | 是 | 是 | 是 | **否** |
| 老消费方 | 未知变体可能 panic | 字段含义悄悄变 | 无 | 按 `event` 字段过滤即跳过 |
| 业界一致 | — | — | — | ✅ claude / codex |

**决策：丁。**

### 4.2 id 何时可用 —— 不落盘，因为不需要

初稿曾考虑饿汉落盘。调研否定了这个必要性：

- id 由 `session_path` 的 stem 给出，**构造期即已知**，与文件是否已创建无关。
- 唯一「有 id 无文件」的窗口是 §2 第 2 条（`Abort` 在 Step 2 之前），
  触发条件是「进程刚起来就被 `SIGTERM`」或「预算为 0」—— 罕见，且 `--resume` 会
  明确报「找不到会话」，是**响亮的失败**，不是静默错误。
- 饿汉落盘的代价相反：任何零产出回合都会在 `sessions/` 留一个只有 meta 的条目，
  出现在 `--resume` 列表里（`sessions.rs:48` 按目录扫 `*.jsonl`）。

**决策：不饿汉落盘。** 早先「eager flush」的构想作废 —— 它解决的是不存在的问题。

### 4.3 退出码

**分工：退出码回答「能不能重试」，细粒度原因由 `stop_reason` 给。**

| 码 | 含义 | `StopReason` |
|---|---|---|
| `0` | 正常（含 `Truncated` / `MaxRounds`，有产出能交差） | `Completed` / `MaxRounds` / `Truncated` |
| `1` | 故障，**不可重试** | `Failed`；以及 `ensure_configured` 早退等非 `StopReason` 失败 |
| `75` | 暂时没跑完，**可重试** | `Cancelled` / `NoOutput` / 收到 `SIGTERM` |

- **不用 `2`**：`docs/arch/goal-command/context.md:150` 已为「目标未达成」预留。
- **不用 `143`**：那是「被信号杀死」的惯例；本设计是**自己收尾后主动退出**，
  伪装成 143 会让调用方误以为我们没处理信号。
- **`75` = `EX_TEMPFAIL`**：语义正是「暂时失败，可重试」，且不占小整数码。

⚠️ **行为变更**：`Cancelled` / `NoOutput` 从 `1` 改为 `75`。

### 4.4 `SIGTERM` 收尾

**固定 5 秒宽限，不做配置；只做 `cfg(unix)`。**

信号落点是 `run_single_turn` **已有**的 `select!`（`main.rs:2129`）—— 那里已经在做
「回合 future vs 预算计时器」，加一个分支即可，形状现成：

```
tokio::select! {
    result = &mut turn               => break result,
    _ = sleep_until(deadline), if !budget_exhausted => { … boundary.push(Abort) }
    _ = sigterm.recv(),       if !signal_aborted   => { boundary.push(Abort) }   // 新增
}
```

收尾沿用唯一退出落点 `exit_with_run_end`（`main.rs:2725`）—— 它内部已有
`kill_backgrounds_on_exit()`（`:2745`）与 debug flush，**另起路径等于复制收尾逻辑**。

本仓已有两条单测把这条守住：`bare_process_exit_appears_only_inside_exit_with_debug`
（`main.rs:7315`，断言裸退出原语全文件恰好一处且在该函数体内）与
`background_cleanup_runs_before_the_process_exit_primitive`（`:7339`，断言清理先于退出）。
⇒ 实现时**不需要**新增任何护栏，只要别绕开那个函数。

**为什么宽限不做配置**：超时方是 bridge（它有自己的 `stopGraceMs`），被超时方只要
比它短即可；可配只会多一个没人调的旋钮。

---

## 5. 详细设计

### 5.1 `--json` 首行会话记录

**格式**（第一行，先于任何 `Envelope`）：

```json
{"type":"session","id":"2026-11-10-abc"}
```

- 输出点：`run_json_mode` 进回合循环**之前**（`main.rs:2572` 的 `loop` 之上），
  `println!` 一行。**不进 `consume_events`** ⇒ `-p` 不受影响。
- **位置必须晚于 `ensure_configured`**（`main.rs:2564`，`run_json_mode` 第一句）。
  写在它之前的话，无配置早退也会报出一个指向不存在会话的 id，bridge 会把垃圾 id 存下来。
- id 来源：`wiring.session_path()` 的 `file_stem`。`session_path` 为 `None`
  （内存会话）时**不输出该行** —— 没有 id 可给，比给假的强。
  ⇒ `--resume <id>` 时 `persistent_at(Some(path))`（`wiring.rs:130`）把 `session_path`
  设为传入路径，`file_stem` 天然是**同一个** id ⇒ 首行回显一致，bridge 存的 id 不漂。
- **只在 `--json`**。`-p` 的 stdout 是纯正文（`README.md:274`），加行即污染。
- 与 `Envelope` 的区分：`Envelope` 的字段是 `source`/`turn`/`event`，**没有 `type`**；
  本行**没有 `event`**。⇒ 老消费方按 `event` 字段过滤天然跳过，不会 panic。

### 5.2 信号接线

`apps/coding-agent/Cargo.toml:29` 的 tokio features 加 `signal`。

- **`signal()` 建在 `loop` 之前，循环内 reborrow**（`run_print_mode:2413` /
  `run_json_mode:2572`）。不能在每回合新建：`SingleTurnInputs` 是每回合建的
  （`:2429`/`:2582`），若 signal 随之重建，**回合之间**到达的 `SIGTERM` 会掉进
  没人 `recv` 的窗口。
- `SingleTurnInputs` 现在**不含 boundary**（`main.rs:2043`），boundary 是
  `run_single_turn` 内部局部量（`main.rs:2086`）。信号分支就写在同一个函数里的
  `select!` 中，故**不需要把 boundary 外提** —— 只加一个 `Option<&mut Signal>` 字段。
- **信号分支只做一件事：`boundary.push(Abort)`。不要自己退出、不要加标志位。**
  回合以 `Cancelled` 收场后，`join!` 自然返回（回合 side 完成 + 消费循环见终局事件
  即终止），随后走既有 `single_turn_exit_code` 得到 `75`。
  这条必须写明：`exit_with_run_end` 要 `&mut Option<DebugSink>`（`:2725`），而
  `join!` 期间它被 `consume_events` 独占着 —— 实现者若以为「信号得自己早退」，
  会去抢 sink 或另开 `process::exit`，后者直接撞红 `main.rs:7315` 那条单测。
- **loop 不会因信号再起回合**，无需额外判据：`turn_advance::advance` 对 `Cancelled`
  返回 `Stop(Aborted)`（`turn_advance.rs:294-304` 有单测钉住），
  `post_turn::should_follow_up` 也只在 `Completed` 时放行（`post_turn.rs:82-85`）。
  在这里加一份「信号后不再追问」的判断，就是 `turn_advance.rs:6-8` 模块头警告的
  「两份条件必然漂移」。
- **`SIGTERM` 落在回合刚 `Completed` 之后**：退出码为 `0`。这是**正确的** —— 对
  bridge 而言是「成功了只是慢」。不要加额外标志把它强改成 `75`，那会造出
  「跑完了却报可重试」的假失败。
- 只 `cfg(unix)`。Windows 上 bridge 走 `cmd.exe`，本项目当前不在 Windows 跑 ——
  不做没人验证的跨平台分支。

### 5.3 退出码

`single_turn_exit_code`（`main.rs:2689`）改三路：

```rust
StopReason::Failed                          => 1,
StopReason::Cancelled | StopReason::NoOutput => 75,
// 其余（`Completed` / `MaxRounds` / `Truncated`）=> 0
```

**穷举写法**：`StopReason` 是 `#[non_exhaustive]`（`post_turn.rs:80-81` 注释明说），
保留 `_ => 0` 会让将来新增的「失败类」变体**静默退 0**。改为逐个列出六个变体 +
一条 `#[allow(unreachable_patterns)]` 的兜底，让新变体在编译期提醒。

信号中止的回合走正常路径（`Cancelled`）⇒ 自动得到 `75`，**不需要额外分支**。

---

## 6. 改动面

| # | 文件 | 改动 | 风险 |
|---|---|---|---|
| 1 | `apps/coding-agent/Cargo.toml:29` | tokio 加 `signal` | 低 |
| 2 | `apps/coding-agent/src/main.rs` | ①首行 session 记录 ②`single_turn_exit_code` 三档 ③`select!` 加信号分支 ④`SingleTurnInputs` 加字段 ⑤两个 `run_*_mode` 各建一次 `signal()` | 中 |
| 3 | `apps/coding-agent/README.md:267-283` | 退出码表 + `--json` 首行说明 | 无（文档） |
| 4 | `docs/design-final/turn-output-and-feedback.md:48` | 同步退出码；顺手修正函数名（该行写 `exit_with_code`，实际是 `exit_with_run_end`） | 无（文档） |
| 5 | `apps/coding-agent/tests/debug_log.rs:567-584` | `without_round_usage` 兼丢「无 `event` 字段」的行（**不改 golden**，见 §7） | 低 |

**不碰**：`crates/` 下任何 crate、`AgentEvent`、`Envelope`、工具表、系统提示词。
⇒ `toolset.rs:813` 的字节锚不受影响。

---

## 7. 受影响的既有测试（调研已逐条核实）

**会红的（必须一起改）**

| 测试文件 | 位置 | 为什么 |
|---|---|---|
| `envelopes()` 逐行 `expect("--json 每行必须是合法 JSON")` | `tests/debug_log.rs:553-560` | ✅ **不会红** —— 只要求合法 JSON，不要求是 `Envelope` |
| 「stdout 只应有 Envelope」 | `tests/debug_log.rs:1041-1044` | 🔴 断言 `value.get("event").is_some()` ⇒ 首行必红 |
| golden 逐字节比对 | `tests/debug_log.rs:876-890` + `tests/golden/json_no_debug_baseline.jsonl` | 🔴 见下（**修法不是改 golden**） |
| `parse_all_json_lines` | `tests/budget_exit.rs:378-389` | ✅ **不会红** —— 同 `envelopes()`，只要合法 JSON |
| `single_turn_exit_code` 三条 | `src/main.rs:5219`、`:5224`、`:5230` | 🔴 `Cancelled` / `NoOutput` 改 `75` |
| `budget_exit.rs` 两条 `!success` | `tests/budget_exit.rs:415`、`:443` | ✅ 仍是 `75` ≠ 0，`!success` 成立 |

### golden 的正确修法：改过滤器，不改 golden

**不能**「4 行改 5 行」。`write_baseline_golden`（`debug_log.rs:899-919`）要求用
**基线二进制**（`YUSHAN_BASELINE_BIN`，`7987b4b` 构建）生成 —— 那个二进制不输出
session 行 ⇒ 重生成的仍是 4 行，与新二进制的 5 行恒不等。硬改 golden 就毁了该测试
的立身之本（用**未改动**的基线拦住输出漂移）。

**做法**：扩展 `without_round_usage`（`debug_log.rs:567-584`）。它现在按
`event.RoundUsage` 丢行；加一条「**没有 `event` 字段的行也丢掉**」——
无 `event` 的行按定义就不是 `Envelope`，正是该过滤器要丢的东西。

这样 golden 保持 **4 行逐字节不变**，语料仍来自基线二进制，该测试继续回答它本来的问题。

---

## 8. 风险与护栏

| 风险 | 护栏 |
|---|---|
| 首行破坏既有消费方 | 首行**无 `event` 字段** ⇒ 按 `event` 过滤的消费者天然跳过；测试钉住「首行 type==session，其后均为 Envelope」 |
| 信号路径与正常路径收尾分叉 | **不新增退出路径**：信号只 `push(Abort)`，收尾交既有 `exit_with_run_end`。已有两条单测强制（`main.rs:7315`、`:7339`）—— 裸 `process::exit` 全文件只许一处，且清理先于它 |
| 实现者在 loop 层另写一份「信号后不再追问」判据 | §5.2 已引 `turn_advance.rs:294-304` 与 `post_turn.rs:82-85`：现有判据已覆盖，重复只漂移 |
| 信号落在回合刚完成后 | 归类为 `0`（成功只是慢），**不加**强改 `75` 的标志 |
| 「有 id 无文件」（§4.2 的罕见窗口） | `--resume` 明确报「找不到会话」⇒ 响亮失败。文档写明这是已知边界 |
| `75` 是不兼容变更 | README 显式标出不兼容；按 `!= 0` 判断的调用方不受影响 |
| `select!` 让回合 future 在非安全点被 drop | 不 drop：只 `push(Abort)`，让回合自己收场 |
| `-p` 被污染 | 首行只写在 `run_json_mode`，不在共用的 `consume_events` 里 |

---

## 9. 测试矩阵

| 用例 | 层级 | 钉住 |
|---|---|---|
| 首行 `{"type":"session","id":…}`，其后每行都是 `Envelope` | 单测 | G1 + 输出契约 |
| 首个回合后，该 id 能 `--resume` 成功 | 集成（`tests/session_cli.rs` 同型） | G1 |
| **`--json --resume <id>` 的首行 id == `<id>`** | 集成 | G2 —— 钉住 bridge 存的 id 不漂 |
| `-p` 的 stdout 无新增行 | 单测 | `-p` 不被污染 |
| 内存会话（无 `session_path`）⇒ 不输出 session 行 | 单测 | 无 id 不造假 |
| 无配置早退 ⇒ 无 session 行 | 单测 | 首行在 `ensure_configured` 之后 |
| `SIGTERM` ⇒ 码 `75`、后台进程组已收、debug 尾部有 `run_end` | **进程级** | G3 |
| `SIGTERM` 打断在飞的 `bash` ⇒ 该进程组不残留 | **进程级**（`tests/budget_exit.rs` 同型） | G3 —— 钉住工具的 100ms 链路真被信号驱动 |
| `single_turn_exit_code` 三档（含变异） | 单测 | G4 |
| 续跑回合上下文连续 | `#[ignore]` e2e（需凭证） | G2 |

**阶梯纪律**（`AGENTS.md`）：G3 / G4 必须有**进程级**验证 ——
`tests/budget_exit.rs` 已开这个先例。

---

## 10. 明确不做

| 不做 | 理由 |
|---|---|
| 指定会话 id（`--session-id`） | 引入 id 清洗、路径逃逸、并发冲突三个新问题，只为省 bridge 存一个字符串。`persistent_at` 已有 `path` 形参，将来是纯加法 |
| 饿汉落盘 | §4.2 —— id 构造期已知，它解决不了任何问题，只会往会话列表里塞空条目 |
| 退出码 `2` | 让给 `goal-command` 的「未达成」 |
| 宽限期可配 | 超时方是 bridge，被超时方不需要旋钮 |
| stdin 提示词 | 本平台 argv 够用；bridge 的 Windows 坑不适用 |
| 同 id 并发锁 | bridge 串行；append-only + `repair_dangling_tool_calls`（`jsonl.rs:326`）可自愈 |
| 跨平台信号 | 只 `cfg(unix)` |
| 往 `Envelope` / `AgentEvent` 加任何字段 | 200 处匹配 vs 一行输出，不成比例 |
| 新增退出路径或「信号专用」标志位 | 既有 `exit_with_run_end` + `Cancelled → Stop(Aborted)` 已覆盖，详见 §5.2 |
