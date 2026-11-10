# headless-worker-api — 架构上下文

> 日期：2026-11-10
> 触发：`../YuShan-lark-agent-bridge`（飞书 ↔ 本地 agent 桥）要接 YuShan 作为**被动 worker**。
> 状态：**探索完成，等用户对「未澄清问题」表态** ⇒ 之后走 `arch-design`。

---

## 概述

一句话：把 `-p` / `--json` 这条**程序化入口**从「人也能用」提升为「进程可编排」——
会话可续、（被信号）杀死可收尾、失败原因可判。

已有事实：bridge 侧已写好两套 agent 适配器（claude / codex），接口是
`AgentAdapter::run(AgentRunOptions) -> AgentRun`，靠 spawn 子进程 + 解析 stdout 事件流驱动。
YuShan 只需要成为**第三个适配器**，不需要 bridge 改协议。

---

## 现有架构

### 模块边界

```
飞书事件 ──▶ YuShan-lark-agent-bridge（TypeScript，长期驻留）
                  │  spawn(每次一个回合) + 解析 stdout + SIGTERM 收尾
                  ▼
            ys-coding-agent -p / --json（Rust，短命进程）
                  │
                  ▼
            session jsonl（落盘，续跑的唯一真值）
```

- **bridge 不属于本仓**，它有自己的会话目录与 catalog（`src/session/catalog.ts`），
  按 `sessionKind` 区分：`claude-session` 存 `sessionId`，`codex-thread` 存 `threadId`。
- 本仓对外的程序化面**只有 CLI**：`-p <任务>` / `--json <任务>` / `--resume <id>`。
- 进程内没有 IPC / socket / HTTP 入口，且本轮不打算加（见「范围外」）。

### 核心抽象

| 位置 | 抽象 | 现状 |
|---|---|---|
| `crates/ys-protocol/src/envelope.rs:48` | `Envelope { source, turn, event }` | `--json` 的逐行输出单元。**不含会话标识** |
| `crates/ys-event/src/event.rs:39` | `AgentEvent` | 13 个变体；终局只有 `RunFinished` / `RunFailed` |
| `crates/ys-core/src/stop.rs:5` | `StopReason` | `Completed` / `MaxRounds` / `Cancelled` / `Truncated` / `Failed` / `NoOutput` |
| `apps/coding-agent/src/wiring.rs:167` | `Wiring::new_session()` | 生成 id 并落盘：`new_session_id()` = `{secs}_{nanos:09}`（`:361`） |
| `apps/coding-agent/src/wiring.rs:197` | `Wiring::resume_into(path)` | 运行期换会话；`JsonlSession::open` 成功后才替换 |
| `apps/coding-agent/src/main.rs:2689` | `single_turn_exit_code()` | `Cancelled` / `NoOutput` / `Failed` ⇒ `1`；其余 ⇒ `0` |
| `apps/coding-agent/src/main.rs:2745` | `kill_backgrounds_on_exit()` | 进程退出**前**杀全部会话的后台进程组 |
| `apps/coding-agent/src/main.rs:1445` | `stderr_is_human_facing()` | 口径 = **stdout 是终端**；决定是否打 `session: <id>` |

### 关键数据流

**首次回合（bridge 视角）**

1. bridge `spawn(ys-coding-agent, ['--json', '<任务>'], {cwd})`
2. 进程建**新**会话（`--resume` 缺席）⇒ `Wiring::new_session` 生成 id，惰性落盘
3. 逐行输出 `Envelope`
4. **`session: <id>` 只打到 stderr，且仅当 stdout 是终端**（`main.rs:1582`）⇒ 管道下**永远看不到**
5. 进程以 `0` / `1` 退出

**续跑回合（bridge 视角）**

1. bridge 需要 `--resume <id>`，但 id 拿不到（上一步第 4 点）⇒ **当前无法续跑**

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| `../YuShan-lark-agent-bridge` | 消费方，只读参考 | — | 不在本仓，不进 CI |
| `tokio` | 进程内定时/信号 | 1.x | `apps/coding-agent/Cargo.toml:29` 的 features **不含 `signal`** |
| 现有信号处理 | 仅 TUI 挂断看守 | — | `crates/ys-tui-coding/src/run.rs:1472` 是独立看守线程，与单回合模式无关 |

---

## 约束

- **技术**
  - `-p` / `--json` 与 TUI 共用装配序列，但**只有 TUI 有常驻 UI 线程**；单回合模式没有
    任何信号处理，进程被 `SIGTERM` 即硬停 ⇒ `kill_backgrounds_on_exit()` 不跑、
    debug sink 尾部不 flush、后台进程组变孤儿（`main.rs:2745` 的注释明说「退出原语不跑析构」）。
  - `single_turn_exit_code`（`main.rs:2689`）把三种不同失败压成同一个 `1`。
  - 会话 id 的生成归 `wiring.rs:361`（`secs_nanos`），调用方无法指定。
  - 会话落盘是**惰性**的：一个回合都没跑的会话没有文件。⚠️ **但 id 在构造期已确定**
    （`wiring.rs:137` 立即算出路径并存入 `session_path`）—— 「有 id」与「有文件」是两件事。
- **性能**：无要求。新增一个首行事件 / 一次信号分支，开销可忽略。
- **演进**
  - `Envelope` 是 `--json` 的**稳定契约**（消费方已在解析）。加字段要保证老消费方不炸。
  - 退出码语义已被 `README` / `docs/arch/goal-command/context.md:150` 引用 ⇒ 改要写 ADR。
  - 工具面 / 系统提示词**一律不动**：任何碰固定前缀的改动都要过 `toolset.rs:813` 的字节锚。
- **组织**：本仓自己维护，`AGENTS.md` 定死「文档、注释用中文」；实现一律经 subagent 派发。

---

## 需求范围

### 范围内（2026-11-10 拍板）

1. **会话 id 可编程获取** —— `--json` 首行一条独立结构化记录带 id；首行形态已定为
   `{"type":"session","id":…}`（依据见 `design.md` §4.1）。
2. **`SIGTERM` 有序收尾** —— 投 `Abort` → 收后台进程组 → flush debug → 可重试退出码。
   **固定 5s 宽限**，接进已有 `select!`（`main.rs:2129`）。
3. **退出码分档** —— `0` 正常 / `1` 故障 / `75` 可重试；`Truncated` 归 `0`，
   `MaxRounds` 归 `0`（定稿表见 `design.md` §4.3）。

### 范围外（明确不做）

- **指定会话 id**（`--session-id`）：引入 id 清洗、路径逃逸、并发冲突三个新问题，
  只为省 bridge 存一个字符串。`persistent_at` 已有 `path` 形参，将来是纯加法。
- **MCP**：bridge 直接 spawn 进程，不走工具层，与工具面无关。
- **「系统推一个回合」**：推送由 bridge 驱动，harness 不需要自己醒
  （`docs/goals.md` 欠账 B 对这条路径**不成立**）。
- **队列 / 去重 / 批处理 / 多租户**：bridge README 已写明这些归它自己。
- **IPC / HTTP / socket 入口**：保持「一次 spawn = 一个回合」的进程模型。
- **`--append-system-prompt`**：桥注的 `<bridge_context>` 可以走任务文本或工作区文件，
  不必占本仓的对外面。
- **子 agent 跨回合补投**（`main.rs:2092` 的已知边界）：单回合模式下这是设计事实，
  不是本需求的缺口 —— 要补属于另一条线。

### 关键场景

- **S1 首次回合**：bridge 拿到 id 并能持久化，用户看到回复。
- **S2 续跑回合**：bridge 用上次的 id 起进程，上下文连续。
- **S3 用户发 `/stop`**：bridge 发 `SIGTERM`，进程收尾退出，无孤儿进程组，退出码表明「被中断」。
- **S4 provider 故障**：退出码可区分于「预算耗尽」与「正常完成」，bridge 决定重试还是报错。
- **S5 进程被 kill 后重启**：会话文件仍是完好的 append-only JSONL（不出现半条记录）。

### 异常 / 边界场景

- 指定的会话 id **已存在** / **含非法字符** / **路径逃逸**（`../`）
- 会话目录不可写（`ensure_sessions_dir` 失败）
- 首个回合零输出（`NoOutput`）—— id 已上报但文件未创建（`Abort` 探针在追加用户消息之前）；
  `--resume` 会明确报「找不到会话」（已接受，见 `adr-001` 决策 2）
- `SIGTERM` 到达时正卡在一次工具调用里（长 `bash`）—— 固定 5s 宽限，超时也先跑收尾
- `SIGTERM` 期间子 agent 还在跑
- 同一 id 被两个进程同时续跑（本轮不做，bridge 串行）

---

## 未澄清问题（已于 2026-11-10 拍板，见 `design.md` / `adr-001`）

- [x] **Q1（载体）会话 id 走哪条路？** ⇒ **独立首行记录**（`{"type":"session","id":…}`），
  不进 `AgentEvent` / `Envelope`。依据：层级正确 + 改动面差两个数量级 + claude/codex 同构。
- [x] **Q2（指定形态）／Q3（id 合法性）** ⇒ **不做指定**。bridge 自己存一个字符串即可；
  指定会引入清洗、路径逃逸、并发冲突三个新问题。`persistent_at` 已有 `path` 形参，将来是纯加法。
- [x] **Q4（退出码表）** ⇒ `0` 正常 / `1` 故障不可重试 / **`75`** 暂时没跑完可重试。
  不用 `2`（让给 `goal-command` 的「未达成」），不用 `143`（我们不是被信号杀死）。
- [x] **Q5（SIGTERM 窗口）** ⇒ **固定 5 秒**，不做配置。接进 `run_single_turn` 已有的 `select!`。
- [x] **Q6（落盘时机）** ⇒ **不饿汉落盘**。调研发现 id 在构造期（`wiring.rs:137`）就已确定，
  与文件是否创建无关 ⇒ 早先的 eager flush 构想解决的是不存在的问题。
- [x] **Q7（stdin）** ⇒ 本轮不做（本平台 argv 够用）。
- [x] **Q8（并发同 id）** ⇒ 本轮不做（bridge 串行；append-only 可自愈）。

---

## 后续建议

- 方案已定，见 [`design.md`](./design.md) 与 [`adr-001`](./adr-001-headless-worker-contract.md)。
- 实现前先在 `docs/TODO.md` 立条目（三件：id 上报、SIGTERM + 退出码）。
- 验收建议直接用 bridge 侧三个场景（S1/S2/S3）跑通，而不是只写单测 ——
  `-p` / `--json` 的既有 bug（消费循环首个终局事件就返回、`consume_events` 陷阱）
  都属于「单测全绿但端到端不对」那一类。
