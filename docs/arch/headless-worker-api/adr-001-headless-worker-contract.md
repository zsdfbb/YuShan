# ADR-001（headless-worker-api）：程序化入口的三处契约补齐

- **状态**：proposed
- **日期**：2026-11-10
- **相关**：ADR-0010（Agent 无状态）、ADR-0013（取消即消息）、ADR-0049（终局答案边界拉取）
- **上下文**：[`context.md`](./context.md)
- **设计**：[`design.md`](./design.md)

## 背景

`../YuShan-lark-agent-bridge`（飞书 ↔ 本地 agent 桥）要把本仓当作**被动 worker**：
每条用户消息 spawn 一个 `--json` 进程，跑完一个回合即退出。它已有两套同构适配器
（claude / codex），接口是「spawn 子进程 + 解析 stdout 事件流 + SIGTERM 收尾」。

本仓的程序化入口在三处不满足这个用法：

1. **拿不到会话 id**。`Envelope` 只有 `{source,turn,event}`
   （`crates/ys-protocol/src/envelope.rs:48`）；`session: <id>` 只在 stdout 是终端时
   打到 stderr（`main.rs:1445`、`:1582`）⇒ 管道调用永远无法 `--resume`。
2. **不处理信号**。全仓无 `SIGTERM` 处理（tokio features 不含 `signal`，
   `apps/coding-agent/Cargo.toml:29`）⇒ 硬停，`kill_backgrounds_on_exit()` 不跑、
   debug sink 尾部丢失、后台进程组变孤儿。
3. **退出码只有 `0` / `1`**，`1` 同时表示故障 / 预算耗尽 / 无输出（`main.rs:2689`）。

## 决策

### 决策 1：会话标识走**独立首行记录**，不进 `AgentEvent` / `Envelope`

`--json` 的第一行输出 `{"type":"session","id":"<id>"}`，其后仍是 `Envelope` 流。
内存会话（`session_path == None`）不输出该行。

**理由**：

- **层级正确**。会话 id 是「进程 ↔ 调用方」的握手信息，不是「回合内发生了什么」。
  塞进 `AgentEvent` 会让「事件只观察回合」这条边界变浑。
- **两家同类实现的独立收敛**。claude 的 `stream-json` 首行是
  `{"type":"system","subtype":"init","session_id":…}`；codex 的 `exec --json` 首行是
  `thread.started` 携带 thread id。都是**独立于业务消息**的一条记录。
- **改动面差两个数量级**。`AgentEvent::RunFinished` 在 15 个文件里约 66 处构造、
  约 200 处匹配；复用 `Envelope.source` 则是把「谁发的」悄悄改成「哪个会话」。
  独立记录只需在 `run_json_mode` 加一处输出。
- **可区分**。`Envelope` 没有 `type` 字段，首行没有 `event` 字段 ⇒ 老消费方按
  `event` 过滤天然跳过。

### 决策 2：**不做**饿汉落盘 —— id 在构造期就已确定

`persistent_at`（`wiring.rs:137`）**立即**算出会话路径并存入 `session_path`，
文件只是延迟到首次 `append` 才创建。故「拿 id」与「文件已存在」是两件事，
不需要为前者去推动后者。

**被否的方案**：一次性模式饿汉落盘。它解决的是**不存在的问题**，代价却是
任何零产出回合都会在 `sessions/` 留一个只有 meta 的条目，混进 `--resume` 列表
（`sessions.rs:48` 按目录扫 `*.jsonl`）。

**接受的边界**：`Abort` 探针在追加用户消息**之前**（`basic.rs:495` vs `:518`），
所以「进程刚起来就被 `SIGTERM`」这类罕见情形会给出一个指向不存在文件的 id。
此时 `--resume` 明确报「找不到会话」—— **响亮的失败**，可接受。

### 决策 3：退出码分三档，用 `75` 而不用 `2`

| 码 | 含义 | `StopReason` |
|---|---|---|
| `0` | 正常（含 `Truncated` / `MaxRounds`，有产出能交差） | `Completed` / `MaxRounds` / `Truncated` |
| `1` | 故障，不可重试 | `Failed`；以及 `ensure_configured` 早退等非 `StopReason` 失败 |
| `75` | 暂时没跑完，可重试 | `Cancelled` / `NoOutput` / 收到 `SIGTERM` |

**分工**：退出码只回答「能不能重试」，细粒度原因由 `stop_reason` 给。

**为什么不用 `2`**：`docs/arch/goal-command/context.md:150` 已为「目标未达成」预留，
共用会留下永久歧义。

**为什么不用 `143`**：那是「被信号杀死」的 shell 惯例。本设计里进程是**自己收尾后
主动退出**，不是被内核杀掉 —— 伪装成 143 会让调用方以为我们没处理信号。

**已知不兼容**：`Cancelled` / `NoOutput` 从 `1` 改为 `75`。按 `!= 0` 判断的调用方
不受影响；按 `== 1` 判断的会漏。

### 决策 4：`SIGTERM` 收尾接进**已有的** `select!`，固定 5 秒宽限

信号分支加在 `run_single_turn` 现有的 `tokio::select!`（`main.rs:2129`）里 ——
那里已经在做「回合 future vs 预算计时器」，形状现成。命中后 `boundary.push(Abort)`，
回合自然以 `Cancelled` 收场，再由唯一退出落点 `exit_with_run_end`（`main.rs:2720`）
做后台进程清理与 debug flush。

**理由**：

- **复用唯一退出落点**。`main.rs:2745` 的注释自标「退出原语不跑析构」，
  另起退出路径等于复制一遍收尾逻辑。
- **不需要把 boundary 外提**。boundary 是 `run_single_turn` 内部局部量
  （`main.rs:2086`），信号分支写在同一函数内即可，`SingleTurnInputs` 只加一个
  `Option<&mut Signal>`。
- **宽限期不做配置**。超时方是 bridge（有自己的 `stopGraceMs`），被超时方只要
  比它短即可。
- **只 `cfg(unix)`**。本项目当前不在 Windows 上跑，不做没人验证的跨平台分支。

## 明确不做

| 不做 | 理由 |
|---|---|
| 指定会话 id（`--session-id`） | 引入 id 清洗、路径逃逸、并发冲突三个新问题，只为省 bridge 存一个字符串。`persistent_at` 已有 `path` 形参，将来是纯加法 |
| 饿汉落盘 | 决策 2 |
| MCP | bridge 直接 spawn 进程，不走工具层 |
| 「系统推一个回合」 | 推送由 bridge 驱动，harness 不需要自己醒 |
| stdin 传提示词 | 本平台 argv 够用；bridge 的 Windows 坑不适用 |
| 同 id 并发锁 | bridge 串行；append-only + `repair_dangling_tool_calls`（`jsonl.rs:326`）可自愈 |

## Consequences

- **`--json` 的输出契约从「纯 Envelope 流」变成「一行 session 记录 + Envelope 流」**。
  两个既有测试直接受影响：`tests/debug_log.rs:1041-1044`（断言每条都有 `event` 字段）与
  `tests/debug_log.rs:876-890` 的 golden 逐字节比对。`README.md:281` 需同步。
  - **golden 仍然保持 4 行不变**：`write_baseline_golden` 要求用**基线二进制**生成
    （它不输出 session 行），硬改 golden 会毁掉该测试拦漂移的能力。正确修法是扩展
    `without_round_usage`（`debug_log.rs:567`）兼丢「无 `event` 字段」的行 ——
    无 `event` 按定义就不是 `Envelope`。
  - 而 `envelopes()`（`:553`）与 `parse_all_json_lines()`（`budget_exit.rs:378`）
    **不会红** —— 它们只要「每行是合法 JSON」，不要求是 `Envelope`。
- **`-p` 契约不变**：stdout 仍只放正文 —— 新行只写在 `run_json_mode`，
  不进两个模式共用的 `consume_events`。
- **退出码表是行为变更**：`README.md:267-275` 与
  `docs/design-final/turn-output-and-feedback.md:48` 需同步。
- **`crates/` 下零改动**：不碰 `AgentEvent` / `Envelope` / 工具表 / 系统提示词
  ⇒ `toolset.rs:813` 的字节锚不受影响。
- **新增一个依赖 feature**：tokio `signal`。
- **信号不新增退出路径**：只 `push(Abort)`，回合以 `Cancelled` 收场后由既有
  `exit_with_run_end` 收尾、由 `single_turn_exit_code` 得 `75`。两条既有单测强制
  （`main.rs:7315` 裸退出原语全文只许一处、`:7339` 清理先于退出）⇒ 无新护栏。
- **工具的 abort 链路已存在**，本需求不需动它：`bash` 每 100ms 轮询并杀整个进程组
  （`bash.rs:110-118`、`:386-398`）、`exec` 类同（`exec.rs:326`）、web 工具与网络赛跑
  （`net.rs:285-305`）、`subagent` 透传（`subagent.rs:156-157`）。
  ⇒ 5 秒宽限有百倍余量，G3 成立。
- **遗留**：会话 id 一旦上报，就隐含「首次 `append` 后文件存在」。若将来做「指定 id」，
  需重新决定非法 id 的处理，本 ADR 不预设。
