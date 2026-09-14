# 行式 REPL（rustyline）— 架构上下文

> **已作废（2026-09-14）**：本文件描述的行式 REPL 方案已被反转，架构结论已并入 docs/arch/coding-agent-tui/design.md 与 docs/design-final/coding-agent-tui.md。仅作历史留档。


> 目标：**新增行式 REPL 作为交互入口，与现有 ratatui TUI 并存（各为一个 feature），REPL 为默认。**
> 产出本文后进入 `arch-design`。
>
> 前置文档：
> - `../tui-stack-revisit/analysis.md` —— ratatui vs 行式 的存量分析（why）
> - `../tui-stack-revisit/design.md` —— 上一版整体方案（**注意：该文写的是「删除 ui/」，与本轮的「保留并存」冲突，需回改**）
> - `../tui-pi-codewhale-borrow/design.md` —— pi / CodeWhale 借鉴对照
> - `../gap-closure/context.md` —— 产品定位（后台 agent + 简洁 TUI）

---

## 概述

把交互入口从「单一 ratatui 全屏 TUI」改为「**两套并存**」：

```
tui-repl    （默认）行式 REPL —— rustyline 输入，对话进 native scrollback
tui-ratatui （保留）现有全屏 TUI —— 1684 行，不删
```

驱动原因：用户报告的 5 个实际使用问题（详见 `tui-stack-revisit/design.md` §1），其中 **#1「slash 命令时退出 TUI」的根因是 alt-screen**——而 alt-screen 的原始动机（侧栏 / 鼠标 / sticky footer）已被产品决策砍掉或默认关闭。

---

## 现有架构

### 模块边界（`apps/coding-agent/src/`）

**实测行数**（`total / 生产代码`）：

| 模块 | 行数 | UI 专属？ | 被谁用 |
|---|---|---|---|
| `ui/`（5 文件） | 1684 / 1111 | **是**（文件级 feature 门控） | 仅 `main.rs:484` Interactive 分支 |
| `main.rs` | 923 / 510 | 入口 | 三模式共用分发 |
| `wiring.rs` | 372 / 178 | 否 | 三模式都建 `Wiring` |
| `channel.rs` | 666 / 290 | 否 | **三模式都建 `ChannelSink`** |
| `commands/`（mod+builtin） | 1673 / 876 | 否（架构上 UI 无关） | registry 无条件构建（`main.rs:415`），但**只有 `ui/run` 消费** |
| `view.rs`（`AppView`） | 173 / 116 | 否（**零 ratatui 依赖**） | 当前唯一消费者是 `ui/` |
| `format.rs` | 59 / 40 | **是** | 唯一生产消费者 `ui/draw.rs` |
| `status.rs`（`TurnStats`） | 75 / 20 | 否 | 仅 Interactive |
| `config.rs` / `provider.rs` / `state.rs` / `prompt.rs` | 221 / 445 / 142 / 247 | 否 | 三模式共用 |
| **`ansi.rs`** | 105 / 33 | — | **死代码**：生产零消费者 |
| `test_env.rs` | 59 | 测试 | `#[cfg(test)]` |

**关键**：
- **`format.rs` 只有 `ui/draw.rs` 一个消费者** → 客观上是 UI 专属（虽在 `ui/` 外）
- **`AppView` 不依赖 ratatui** → 两套 UI 都能直接复用，无需改动
- **`ansi.rs` 是死代码** → 可顺手删

### 核心抽象

```rust
// ui/mod.rs:48 —— 现有唯一入口，7 个参数
pub async fn run(
    agent: &mut Agent, wiring: &mut Wiring, config: &mut Config,
    commands: &CommandRegistry, stats: &mut TurnStats,
    state_store: &mut StateStore, rx: mpsc::Receiver<Envelope>,
) -> Result<(), Box<dyn std::error::Error>>;
```

REPL 需要**几乎相同的输入集合**，二者签名可保持同构。

### 关键数据流（三模式已共用）

```
parse_args → Mode{Print|Json|Interactive}
   ↓
ChannelSink::new(capacity, StopWhenConsumerGone)     ← 三模式统一
   ↓
Wiring::ephemeral（-p/--json）  |  Wiring::persistent（Interactive）
   ↓
tokio::join!(agent.run(ports, &inbox), 消费者(rx))    ← -p/--json 已是这个形状
```

**可复用性最高的发现**：`consume_print_events`（`main.rs:254-265`，12 行）+ `consume_events`（:185-203，19 行）+ `print_delta_text`（:164-169）+ `is_terminal`（:172-177）**就是行式 REPL 需要的流式打印**——无换行、按序增量写 stdout、终局即返回、`W: Write` 可测。

差别只在 `-p` 是**一次性**（入队一次、join 一次即退出），REPL 需把它包进 `while` 循环。`run_print_mode`（:287-318）已含全部正确性要点（**并发消费防有界信道死锁**、`Agent::run` 驱动 `begin_turn`）。

> ⚠ 这些函数目前是 `main.rs` 私有。**它们是并存方案最该抽出的共享层**——否则 REPL 会复制这段并发/防死锁逻辑。

### 外部依赖

| 依赖 | 用途 | 现状 |
|---|---|---|
| `ratatui` 0.29 | 全屏 TUI | optional，`tui-ratatui` |
| `crossterm` 0.28 | 事件流 + 键盘协议 | optional，`tui-ratatui`（features = `event-stream`） |
| `futures` | `StreamExt` | optional，`tui-ratatui` |
| `inquire` 0.8 | 交互式选择器（`/login` `/model`） | 非 optional，**两套 UI 都要** |
| `rustyline` | 行编辑 | **已删除，需重新引入** |

**已验证可用的 API**（本机 crate 源码）：
- `crossterm::event::PushKeyboardEnhancementFlags` / `PopKeyboardEnhancementFlags` / `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`（Shift+Enter 的前提）
- `ratatui::terminal::Viewport::Inline(u16)` + `Terminal::insert_before()`（若将来走 inline 路线）

---

## 约束

### 技术

- **两套 UI 各为一个 feature**：`tui-repl`（默认）/ `tui-ratatui`（保留）
- 现有 `ui/` 5 文件是**文件级** `#![cfg(feature="tui-ratatui")]`；`main.rs:17` 是 item 级 `#[cfg] mod ui;`
- **`--no-default-features` 现状**：Interactive 分支**直接报错**（`main.rs:499-502`），无任何回退
- 契约层零 tokio（ADR-0012）；`apps/*` 可用
- `ModelEventSink` 必须保持同步（动态插件 ABI）
- **`AppView` 快照 + `Prompter` trait 是既有抽象，两套 UI 共用**，不改

### 性能

- 信道有界、容量可配（`YUSHAN_CHANNEL_CAPACITY`，下限 16）
- 行式下消费者在 turn 期间**顺序打印**；打印阻塞时背压经有界信道反作用于 agent（`StopWhenConsumerGone`）

### 演进

- **「ratatui 保留」意味着这不是替换，而是并存** —— 两套都得能编译、能跑、能测
- 关键风险：两套 UI 共享的层（turn 驱动 / 命令桥 / 快照）**必须抽出**，否则会分叉出两份并发/防死锁逻辑

### 组织

- 单人维护 → 优先「低耦合、可独立验证」的切分
- 产品定位：**后台 agent + 简洁 TUI**；富展示已正式砍掉

---

## 需求范围

### 范围内

1. **新增 `tui-repl` feature**（默认）：rustyline 行式 REPL
   - 对话直接写 stdout，进 native scrollback
   - 输入：多行（Shift+Enter / Alt+Enter）、历史、补全、粘贴
   - 状态：turn 中一行 spinner（`\r` 原地刷）+ turn 结束一行摘要（**无常驻状态栏**）
   - slash 命令直接执行（无 suspend/resume）
   - turn 中 `^C` → 真 SIGINT → `cancel_handle()`
2. **保留 `tui-ratatui` feature**：现有 1684 行不动（或仅做必要的最小适配）
3. **抽出共享层**：turn 驱动（`consume_events` 等）+ 命令桥（`CommandContext` 组装 + `execute` + `CommandResult` 处理）
4. **修两个真 bug**（与架构无关，可先做）：
   - `ui/events.rs:131` 中文退格 **panic**
   - 补全 popup **从未渲染**（`CompletionState` 无渲染层）

### 范围外（明确不做）

**产品层红线**（`gap-closure/context.md:10-11` 已正式砍掉）：
- Markdown 渲染、Diff Viewer、Theme、**鼠标**、可配快捷键

**字段层**（`tui-status-display/context.md:111-117` 已否决）
- cache 命中率、cost 美元、context 占比、git branch、thinking level、extension status
- **`tui-pi-codewhale-borrow` §10 想引入的 `ctx% · $cost · ttft · tok/s` 属此类，不得重新引入**（见「矛盾」§D）

**架构层**
- alt-screen / 全屏渲染器（`Frame`/`Buffer`/cell diff）
- 「footer 永远在最后一行」这类 sticky 机制
- skill 系统 / `ys-skill` crate（`tui-pi-codewhale-borrow` 引入，**超出命名方案且非 TUI 范围**）
- Plan/Act/Operate 模式、Subagent UI、Workflow、plugin/marketplace
- 多进程 / daemon 化

### 关键场景

| # | 场景 | 期望 |
|---|---|---|
| S1 | 多轮对话 | 对话按序进 scrollback，可回滚查看、`Cmd+F` 可搜 |
| S2 | 跑 slash 命令（含 `/login` 的 inquire） | 无切屏、无闪烁；命令输出留在 scrollback |
| S3 | turn 进行中 | 文本逐块出现；一行 spinner 原地刷新；不冻结 |
| S4 | turn 中打断 | `^C` → 立即（步骤边界）取消，以 `Cancelled` 收场 |
| S5 | 输入 `/` | 候选列表（含 `arg_hint`） |
| S6 | 输入中文后退格 | 正确删一个**字符**，不 panic |
| S7 | 粘贴多行 | 作为一次输入，不被当多次提交 |
| S8 | 切换 feature 构建 | 两套都能编译、能跑；默认 = REPL |

---

## 必须继承的既有结论

**产品红线**：后台 agent + 简洁 TUI；Markdown / Diff / Theme / 鼠标 / 快捷键**正式砍掉**。

**面板默认关闭**：`show_status` / `show_footer` 默认 `false`；**不做常驻状态栏**。

**取消语义**（ADR-0002）：协作式、**回合边界**，`Ok(RunResult{Cancelled})` 非 Err，最坏延迟 = 单次调用耗时。
> ⚠ 新设计若写「立即中断」需与此对齐措辞。

**`cancel_handle` 保留且自转后更需要**（ADR-0010）。

**数据/显示分离**：`AppView` 快照 + `Prompter` trait 保留。

**`ys-` 命名方案 A 已批准待执行**（`gap-closure/context.md:275-286`）——**注意该方案里没有 `ys-skill`**。

**历史决策链**（避免重复推导）：

| 阶段 | 动机 | 现状 |
|---|---|---|
| 裸 `read_line` | — | 无 footer/补全/Ctrl-C |
| 引入 **rustyline** | 历史 / 补全+ghost text / 多行 / Ctrl-C 免 signal | **4 条动机全部仍然成立** |
| 换成 **ratatui** | alt-screen：sticky footer / 独立滚动区 / 侧栏 / 鼠标 / 富展示 | **5 条里 3 条被砍、1 条默认关闭** |
| **本轮**：加回行式 | ratatui 的理由已消解 | 两套并存 |

---

## 已发现的文档矛盾（新设计须先澄清）

来源：`../tui-pi-codewhale-borrow/design.md`（下称「borrow 文档」）与本轮两份文档的交叉检查。

| # | 矛盾 | 处置建议 |
|---|---|---|
| **A** | borrow §6 说 footer「默认关闭，`/status on` 挂载」，§13.3 说「footer **永远在最后一行**」——后者是 alt-screen 概念，**行式 REPL 物理上做不到** | 去掉 §13.3；footer 只作**瞬时行**（spinner / turn 摘要） |
| **B** | borrow §6 发明 `/status on` 切换挂载，而 origin `design.md:125` 的 `/status` 是**打印多行详情** | 二选一，倾向 origin（无状态 toggle 更轻） |
| **C** | borrow footer 文案含 `esc to interrupt` —— 行式下 **agent 运行期间不在 `readline` 内，Esc 根本捕获不到** | 改为 `Ctrl-C to interrupt` |
| **D** | borrow §10 要引入 `ctx% · $cost · ttft · tok/s` —— **这些字段已被 `tui-status-display` 明确否决**（无基建） | 不引入；若确需，须先记录决策反转 |
| **E** | borrow §10 的 4 色方案 vs `improvements-p1-p2-p3.md:238`「ANSI 颜色 user 决策：暂不引入」 | 需显式决策反转或放弃 |
| **F** | 净收益行数：`analysis.md:134` 说保守 **600-800**，`design.md:78` 说 **-1150**（对应 analysis 的「乐观」） | 统一口径 |
| **G** | borrow §7 P5（强类型 `TranscriptLine::Thinking`）**不解决**用户问题 #3（provider 内联 think 标签）；与 origin P4 语义重叠 | 合并或明确分工 |
| **H** | borrow §8 引入 `ys-skill` crate + `@skill` inline —— 与 `gap-closure` 命名方案冲突，且 §10 明确**不做**同类的 `@file` mention | 拆出本文档 |
| **I** | borrow §11 的分期骨架依赖 **`v1 补遗`（该文档不存在）**，P5–P8 全部悬空 | 补写或删除这四期 |

**验证**：`v1 补遗` 经三重确认不存在（全仓 find / grep 只命中 borrow 文档自身 / `docs/` 下无对应结构）。

---

## 信道拓扑（本轮 grill 定稿）

### 结论：**2 进 + 1 出**

```
┌──────────────────────────────────────────────────────────────────────┐
│  UI 线程（REPL / ratatui 各一份）                                     │
└───────┬──────────────────────────────┬───────────────────────────────┘
        │                              │
   入站 A（app 线程，回合边界）      出站（UI 消费）
   入站 B（BasicLoop，轮边界）
        │                              ▲
        ▼                              │
┌──────────────────────────────────────┴───────────────────────────────┐
│  app / agent 线程（持 Agent + Wiring + Session）                      │
│                                                                       │
│  ① A: Request::{ Prompt | Command | Abort | SetModel | ... }          │
│     loop { req = A.recv().await; match req { ... } }                  │
│                       ↑ 这就是 agent 线程的「叫醒」                    │
│                                                                       │
│  ② B: Boundary::{ Steer | Abort }                                     │
│     BasicLoop 轮边界 try_recv()                                       │
│                                                                       │
│  ③ 出站: Outbound::{ Event(Envelope) | View(AppView) }                │
│     ChannelSink.emit()（有界，满则 await = 背压）                     │
└───────────────────────────────────────────────────────────────────────┘
```

### 三条硬约束（每条逼出一个机制，不是设计偏好）

| 边 | 为什么必须独立 |
|---|---|
| **① 回合边界入站** | app 线程的「叫醒」。`recv().await` 就是它 |
| **② 轮边界入站** | **回合跑动时 app 线程阻塞在 `agent.run()` 里，不可能 `recv` ①**；而 `Steer` 要回合**中途**被看到 → 只能由 `BasicLoop` 在轮边界 `try_recv`。合并成一条会互相抢（轮边界拿到一条 `Prompt`，它不知道怎么处理） |
| **③ 出站** | 生产者/消费者分离 |

### 关键分层：`Command` 与 `Prompt` 的区别

**两者同在 ①（同一个消费者、同一个时机），但语义完全不同**：

- `Prompt` → 转成 `Message` 推入 agent 的输入，**agent 不知道它从哪来**
- `Command`（`/login` `/model` `/new`）→ **由接线器执行**。它们要 `ProviderRegistry` / `auth.json` / 换 session 文件 —— **全是产品层概念**

> **不能让命令进 agent 的输入队列**，否则等于把产品层塞进 `ys-runtime`。
> **pi 与 CodeWhale 都避开了**：pi 在 `interactive-mode.ts`（适配层）解析，CodeWhale 在 `commands::execute(cmd, app)`（TUI 侧）。

### 命令按「**结果去哪**」分四类（不是一类）

现有 10 个内置命令的实际归位：

| 结果去哪 | 命令 | 谁处理 | 对 agent 的影响 |
|---|---|---|---|
| **只给用户看** | `/help` `/status` `/copy` `/export` | 接线器 | 无 |
| **改配置** | `/login` `/logout` `/model` | 接线器 | **无消息** —— agent 回合开始时**自取** config |
| **改 Session** | `/new`（换）、`/compact`（重写） | 接线器 | 靠**换 / 重写 Session** 影响，不是发消息 |
| **展开成消息** | *（现有命令中无；将来的 `/skill:` / prompt template）* | 接线器**展开** → 作为普通 `Prompt` | **就是一条普通用户输入** |

**pi 恰好就是这三层**（`agent-session.ts:1159+` 的 `AgentSession.prompt`）：

```
1. /xxx 命中扩展命令  → _tryExecuteExtensionCommand，执行完 return，**不进 LLM**
2. /skill:name / 模板 → **展开成文本**，继续往下走
3. 纯文本             → 交给 agent 队列
```

**第 2 条是关键**：展开型命令的产物是**文本**，走的是与第 3 类**完全相同的路** ——
**所以 agent 根本不知道它来自命令**。这条让「命令」这个产品概念**完全不泄漏进 agent**。

### 接线器的职责清单（由此定稿）

```
接线器（app 线程）
  ├─ ① 收到 Request::Prompt   → 转成 Message 入队给 agent
  ├─ ② 收到 Request::Command  → 按四类分流：
  │     ├─ 纯展示     → 输出给 UI（Outbound::CommandOutput）
  │     ├─ 改配置     → 改 Config（agent 自取，不发消息）
  │     ├─ 改 Session → 换 / 重写（/compact 需 model）
  │     └─ 展开型     → **展开成文本**，走 ①
  └─ ③ 收到 SetModel / Abort 等 → 改配置 / 置 CancelToken
```

**② 的最后一条是根本原因**：同一个 `Command` 请求的产物有**四种去向** ——
这正是「命令执行」必须与「agent 队列」分开的理由（若命令直接进 agent 队列，
agent 就得同时认识这四种）。

### ⚠ 一个真实后果：`/compact` 会产生双份实现

**它是唯一需要 `Session` + `Model` 的命令**：生成摘要要**调模型**，结果要**重写 Session**。

而 `compact_session` **现在是 `BasicLoop` 的私有函数**（`ys-loop/src/basic.rs:371`，
`generate_summary` 在 :439），**唯一调用点是自动压缩**（`basic.rs:134`，每轮开始检查）。

| | 位置 | 触发 |
|---|---|---|
| 自动压缩 | `BasicLoop` 内 | 上下文接近上限 |
| `/compact` | 接线器侧 | 用户显式 |

**两者逻辑相同、代码两处 → 必然漂移。** 这与 `CommandMeta`（元数据在 UI、实现在 app）
是**同一类问题**。

**解法**：把 `compact_session` 从 `BasicLoop` 提出来，成为**两处都能调的共享操作**
（它只需 `&mut dyn Session` + `&dyn Model`，接线器手里都有）。

> **顺带发现**：`/compact` 命令**当前调的是 `agent.clear_session()`** ——
> 即**清空**而非压缩（报告 1.5 记载的问题）。`compact_session` 的能力**已实现但悬空**，
> 命令层根本没接它。这条与上面的"共享操作"是同一个修复。

### 快照必须**推送**，不能请求-响应

UI 若用「发一个 GetView 请求 → 等响应」拿状态，**app 线程在 `run()` 里时无法响应** —— 中途刷新会等到整个回合结束。

**所以 `View` 并入出站 ③（推送）**，不是独立信道，也不是请求-响应。

### 过程中的一次过度分解（记录以免后人误读）

**曾经拆到 5 条信道**（prompt / steer / abort / command / outbound），并把 `CancelToken` 也改成消息。**这是过度分解**：

- 把「**谁消费**」和「**什么时机消费**」两个维度混在一起，导致**同一个消费者（app 线程）被拆成三条**
- 合并后 `Prompt` 与 `Command` 共享 ① —— 它们**同一个消费者、同一个时机**

### `CancelToken`：口味问题，不是架构问题（更正）

曾论证「Abort 必须是信号，因为走信道延迟不可控」——**该论证错误**：

> 协作式取消**本来就没有即时性**（ADR-0002：只在**步骤边界**生效，最坏延迟 = 单次调用耗时）。
> `CancelToken` 与「边界信道里的一条消息」**在同一个边界被检查，延迟完全相同**。

**所以两种都行**：

| 做法 | 理由 |
|---|---|
| **保留 `CancelToken`**（倾向） | ADR-0002 已定义；"是否被取消"是**布尔状态**，不是事件；跨线程取消的正解 |
| 改成 `Request::Abort` 消息 | UI 侧更统一（只认信道）；但要用队列模拟标志位，且要修 ADR-0002 |

**折中**（若希望 UI 侧全走信道）：UI 发 `Request::Abort`，**接线器收到后置 `CancelToken`** —— UI 不碰原子，agent 侧保留原语。

### 单出站 = 单消费者（已知边界）

`mpsc` 是 multi-producer **single**-consumer。**现在够用**（`-p`/`--json`/TUI 互斥，一次一个消费者）。

若将来要 **TUI + 日志同时**：

| 做法 | 代价 |
|---|---|
| `broadcast` | 慢消费者**丢帧**（`Lagged`） |
| 每消费者一条信道（扇出） | 生产者 N 次 `send`；**最慢的拖住 agent**（背压） |
| 一个消费者再分发 | 多一跳延迟 |

### 入站不需要「队列 + 日志」的结合

信道与队列的能力高度重叠（下表）。**入站不需要日志语义**——因为**消费即转移**：

```
入站（待处理） ──消费 = 转移──► Session（历史，可回看、落 JSONL）
```

「可回看」由 `Session` 承担，**它才是那个「队列 + 日志」的结合体**（`Vec<Message>` + JSONL + 可回放），且已实现。

| 队列（`Inbox`） | `mpsc` 对应 |
|---|---|
| `push(&self, msg)` | `send().await` / `try_send` |
| `take_steering()`（轮边界非阻塞拉批） | `while let Ok(x) = rx.try_recv()` |
| `take_followup()`（按 `QueueMode`） | `recv().await`（一条）/ `try_recv` 循环（All） |
| `close()` | drop 全部 `Sender` |
| 有界 + 背压 | `mpsc::channel(cap)` |

**队列真正独有的只有「可回看」（peek / 游标 / 多消费者各自视图）**——而那正是 `Session` 的角色。

---

## UI 在 agent 干活时**能输入**（本轮定，像 Claude Code）

**决定**：回合跑动期间用户仍可输入，消息入队（信道天然是队列）。

### 结构

```
UI 线程：  Editor（rustyline）
             │  create_external_printer()
             │  loop { readline() → tx.send(Request) }      ← 一直可输入
             ▼
app 线程：  loop {
               select! {
                 ① 收 Request（Prompt/Command）      ← 空闲时才 recv（否则留在信道里当队列）
                 ② turn_fut 完成
                 ③ 从事件信道取事件 → printer.print(渲染)  ← 流式输出走这里
               }
             }
```

**注意**：app 线程的结构**与今天 `ui/mod.rs` 的 `run_turn_with_ticks` 三路 `select!` 同形**（`turn_fut` | 事件 | ticker）—— 只是"打印"从"重绘全屏"变成"交给 ExternalPrinter"。

### 关键：`ExternalPrinter` 已由源码确认可行

`rustyline-14.0.0/src/tty/unix.rs:1490-1513`：

```rust
pub struct ExternalPrinter {
    writer: PipeWriter,
    raw_mode: Arc<AtomicBool>,   // ← 与 rustyline 内部**同一个**标志
    tty_out: RawFd,
}
impl ExternalPrinter {
    fn print(&mut self, msg: String) -> Result<()> {
        if !self.raw_mode.load(SeqCst) {
            write_all(self.tty_out, msg)?;      // 不在 readline 中 → 直接写 stdout
        } else {
            self.writer.1.send(msg)?;           // 正在 readline → 经 pipe 注入
            writer.write_all(&[b'm'])?;         //   readline 负责显示在提示符上方并重绘
        }
    }
}
```

**它自带 `raw_mode` 标志，自动选路径** —— 用户没在输入就直写；正在输入就交给 readline 正确显示。
`ExternalPrinter` 是 `Send` 的（`Arc<Mutex<File>>` + `SyncSender` + `RawFd`），**可跨线程交给 app 线程**。

**附带好处**：`sync_channel(1)`（`unix.rs:1454`）容量为 1 → **打印自带背压**。

### 一条推论

「能输入」使 `QueueMode` **真正有用**（消息会累积），也使「信道即队列」成为**必需**：

- 回合跑动中来的 `Prompt` → **停在 ① 信道里**（app 线程不 recv 它）
- 用户连发几条 → 依次成回合，或按 `QueueMode::All` 合并



**结论：`readline()` 返回时 raw mode 已被恢复为进入前的原状。** 方案的地基成立，**无需原型验证**。

**方法**：下载 `rustyline-14.0.0` 源码（历史实现用的版本）逐行核对。

### 证据

`src/lib.rs:664-677` 的 `readline_with`：

```rust
} else if self.term.is_input_tty() {
    let (original_mode, term_key_map) = self.term.enable_raw_mode()?;
    let guard = Guard(&original_mode);          // ← RAII
    let user_input = self.readline_edit(...);
    ...
    drop(guard);                                // ← 显式 drop
    self.term.writeln()?;                        // ← 收尾换行已在 cooked mode
    user_input
}
```

`src/lib.rs:446-454` 的 `Guard`（**且标了 `#[must_use]`**）：

```rust
#[must_use = "You must restore default mode (disable_raw_mode)"]
struct Guard<'m>(&'m tty::Mode);
impl Drop for Guard<'_> {
    fn drop(&mut self) { mode.disable_raw_mode(); }
}
```

`src/tty/unix.rs:122-130` 的 `disable_raw_mode` —— **写回的是进入时保存的原始 termios**，不是盲目 disable：

```rust
fn disable_raw_mode(&self) -> Result<()> {
    termios_::disable_raw_mode(self.tty_in, &self.termios)?;   // &self.termios = 原始值
    if let Some(out) = self.tty_out { write_all(out, BRACKETED_PASTE_OFF)?; }
    self.raw_mode.store(false, Ordering::SeqCst);
}
```
（`enable_raw_mode`（:1376）先 `tcgetattr` 存原值；`disable`（:1532）用 `tcsetattr(original)` 写回。）

### 四个疑虑逐一落地

| 疑虑 | 结论 |
|---|---|
| 所有退出路径都恢复吗？ | ✅ **RAII** —— 正常返回 / `Err(Interrupted)`（Ctrl-C）/ `Err(Eof)`（Ctrl-D）/ unwind 都恢复 |
| 恢复成"原状"还是简单关掉？ | ✅ **写回原始 termios** |
| bracketed paste 会残留吗？ | ✅ disable 时一并发 `BRACKETED_PASTE_OFF` |
| stdin 不是 tty 时？ | ✅ **根本不进 raw mode**，走 `readline_direct`（`is_input_tty()` 分支） |

**关键细节**：`drop(guard)` 在函数体内、`self.term.writeln()` 在它**之后** —— 连收尾换行都在 cooked mode 下写。

### 顺带答掉：`inquire` 与 rustyline 的 raw mode 交替

**两者各自 RAII 恢复原状**，交替安全：rustyline 退出 → cooked；inquire 进入 raw、退出 → cooked；rustyline 再进。
（`Cmd::Suspend` 那条印证同一模式：`disable_raw_mode` → `suspend` → `enable_raw_mode`。）

### 待验清单（已收窄）

**源码答不了的**只剩三条，且都是**我们的集成细节**，不是库行为：

| # | 待验 | 为什么源码答不了 |
|---|---|---|
| 1 | `select!(agent.run(ports,&inbox), tokio::signal::ctrl_c())` 能否真打断 turn | 是**我们的**集成，非库职责 |
| 2 | `PushKeyboardEnhancementFlags` 在**目标终端**的实际支持 | 取决于终端，不取决于库 |
| 3 | spinner 的 `\r` 刷新在真终端的观感 | 观感，非正确性 |

**注意**：rustyline **当前不在依赖树**（c 阶段删除），版本需重新选（历史用 14.0.0，核对即基于该版本）。



### 本轮已定

| # | 结论 |
|---|---|
| **Q1 ys-tui 存废** | **不建 `ys-tui`**。改：两个独立 UI crate（REPL / ratatui）+ 一个**协议 crate**（`AppView` / `CommandMeta` / 请求 / 出站） |
| **Q2 feature 规则** | 两个 feature：`tui-repl`（**默认**）/ `tui-ratatui`（保留）。**只能开一个** —— 同时开启用 `compile_error!` 挡住 |
| **协议 crate 定位与命名** | **`ys-protocol`**。定位：**通用能力平面**（agent 能做什么），任何 peer（UI / 控制器 / 测试 / 另一个 agent）可用 |
| **线程模型** | **多线程优先，多进程不计划** |
| **UI ↔ app 传输** | **信道**（2 进 1 出，见上） |
| **命令执行位置** | **接线器**（不进 agent 队列）；命令按「结果去哪」分四类（见上） |
| **快照传递** | **出站推送**（不是请求-响应，也不是共享 `Arc`） |
| **`Envelope.turn`** | **保留**（消费者不必自己维护计数即可分组） |
| **跑动中能否输入** | **能**（像 Claude Code）—— 用 rustyline `ExternalPrinter`（已由源码确认） |

### 仍待决

- [ ] **`Inbox` 废不废**（**唯一剩下的结构性分歧**）
  - **反对保留的理由**：在「2 进 1 出」+「能输入」已定的前提下，`Inbox` 的两个角色都被信道接管 —— followUp → ① 信道（回合跑动时消息停在信道里），steering → ② 信道（直达 `BasicLoop`）。**而同线程那一段（接线器→agent）直接调 `agent.run_turn` 即可，本不需要队列。** 保留它是"同一件事两套做法且有一套没被用到"
  - **支持保留的理由**：已写好、测过（15 个单测）、能用；废掉要 `ys-component` 加 trait（零 tokio）+ `BasicLoop` 改写 + 四处返工
  - **权衡**：「能输入」定下后，① 信道**必须**存在且**必须**充当队列 —— 这使 `Inbox` 更显多余
- [ ] **`CancelToken` 保留还是信道化**：两种延迟相同（协作式取消本就无即时性），是**口味问题**。保留的理由：它是**电平信号**，被**多处反复读**（loop 各步骤 + `ToolContext` 里的工具）—— 消息读一次就没了
- [ ] **`crossterm` 的 feature 归属**（REPL 也要它做键盘协议）
- [ ] **`CommandMeta` 放哪**：两个参考都放 UI 侧（纯静态数据 + 补全用），但**会与 app 侧实现漂移**
- [ ] **快照的更新策略**：全量推 vs 增量折叠（pi-mini 用复制状态折叠）
- [ ] **出站多消费者**（现在不需要，但要知道边界）
- [ ] **`format.rs` 归属**（只被 `ui/draw.rs` 用；REPL 需要另写 `print_*`）
- [x] ~~**原型归属与判据**~~ → **已收窄为三条集成细节**，见上「待验清单」
- [ ] **`^C` 在 `readline`/`turn` 两段的边界**（rustyline 侧已确认：Ctrl-C → `Err(Interrupted)`；turn 侧待验）
- [x] ~~**`inquire` 与 rustyline 的 raw mode 交替**~~ → **已由源码答掉**（两者各自 RAII 恢复）
- [ ] **键盘协议异常清理**（RAII guard / panic hook）—— rustyline 自己的 `Guard` 是 RAII 的，我们的 `PushKeyboardEnhancementFlags` 应对齐同一模式
- [ ] **文档回改**：`architecture.md` 的「四路 select!」与 `gap-closure/context.md` 的「TUI 本轮不改」**均已被推翻**

## 后续建议

1. **原型已大幅收窄**：原以为要验「地基」（`readline` 返回后是否恢复 cooked mode）——**已由源码证实成立**。现在只剩三条**集成细节**（见上「待验清单」），且都不是正确性风险
2. **再进 `arch-design`**，重点解「仍待决」里的四项结构性选择：协议 crate 名、`Inbox` 存废、`CancelToken` 去向、两 feature 的互斥规则
3. **`borrow 文档` 的 9 处矛盾**（A–I）建议在进入设计前先修，尤其是 **I（`v1 补遗` 不存在）**——悬空引用比没有更糟
4. **两个真 bug 可独立先修**（与架构无关）：中文退格 panic、补全 popup 从未渲染
5. **文档回改**：`architecture.md` §TUI 的「四路 select!」与 `gap-closure/context.md` 的「TUI 本轮不改」均已被推翻

### 一条前提失效的提醒

本轮早先的若干论证建立在「**为跨进程做准备**」上（消息必须 serde-able、要评估 RPC 库、`Arc<Mutex>` 出不了进程所以要信道化）。

**「多线程优先，多进程不计划」定下后，这些论据全部失效。**

结论**未变**（仍用信道），但**理由换了**：从「跨进程就绪」变成「**线程间无共享可变状态、消费时机显式**」。

**同时失效的**：

- 「必须 serde-able」—— 同进程不需要序列化（**但保持 serde-able 仍无成本，可留**）
- 「评估 RPC 库」（tarpc / kameo / jsonrpsee）—— **同进程下价值为零**，且单一 RPC 库只覆盖「一元调用」，覆盖不了「事件推送」。**详见本轮调研：抄 tarpc 的「服务定义与传输解耦」结构，不引实现**
- 「`Inbox` 的 `Arc<Mutex>` 是跨进程障碍」—— **跨线程完全可用**，这削弱了「废掉 `Inbox`」的理由

---

## ⛔ 决定反转：定死 ratatui（2026-09）

> **交互界面用 ratatui，不再讨论。** 本文件以下内容**部分作废**。

### 反转的原因

行式 REPL 的**候选列表体验不够**：rustyline 的补全只在绑定键（Tab）触发，且
`ConditionalEventHandler` 一个按键事件只能产出一个 `Cmd` → **无法"边打边弹下拉"**；
候选列表的显示方式**官方不可定制**（`completion.rs:77` 的 TODO，issue #302）。
reedline 可定制 `Menu`，但引入新库 + 重验全部行为。**为交互易用性，选 ratatui。**

### 作废的内容

| 作废 | 原因 |
|---|---|
| rustyline / reedline 的全部调研（RAII 恢复、`ExternalPrinter`、`Completer`/`Menu`） | 为行式 REPL 做的 |
| 「删 `ui/`，改 `repl.rs`」 | 不删了 |
| 「行式更稳定」的论据（无帧管理 / 无 raw mode / 无 alt-screen） | 前提不存在了 |
| 「UI 跑动中能否输入」这个待决问题 | **ratatui 本来就能**（`select!` 收键鼠），现状已实现 |
| 「两个 feature 只能开一个」 | 只剩一个 UI，feature 门控问题消失 |

### 存活的内容（与 UI 库无关）

| 存活 | 说明 |
|---|---|
| **`ys-protocol`（通用能力平面）** | agent 与外界的接口，任何 peer 可用 |
| **命令四分类 + 接线器职责清单** | 与 UI 无关 |
| **快照必须推送**（不是请求-响应） | ratatui 同样需要 |
| **`Envelope.turn` 保留** | |
| **`CancelToken` 移除**（改为 `Boundary::Abort` 消息） | 代价：工具失去"自愿检查取消"的可选能力（见下节） |
| **UI 独立线程 + 信道（2 进 1 出）** | 分线程的理由是「agent 自己转」，与 UI 库无关 |
| **tarpc 的「服务定义与传输解耦」结构** | |
| **两个真 bug**（中文退格 panic、补全 popup 从未渲染） | 与架构无关，**现在更要修了**（见下） |

### 需要重定（「两个 UI」前提塌了）

```
原：ys-tui-repl + ys-tui-ratatui    两个 crate
现：只有一个 UI                     「两个 crate」不成立
```

连带三处：

1. **UI 还拆不拆 crate**？（原来拆是为了"两个 UI 对称"）
2. **`ys-protocol` 还要不要**？若 UI 不拆 crate，`AppView` 等留在 app 内即可
3. **UI 还走不走独立线程 + 信道**？分线程的理由（agent 自己转）仍在，但代价与收益要重算

### 回到真问题

用户最初报告的 **5 个使用问题**仍未解决，现在要**在 ratatui 内**解：

| # | 问题 | 方向 |
|---|---|---|
| 1 | slash 命令时退出 TUI | **条件 suspend**（只交互命令让位；`/help` `/status` 等纯文本命令不必） |
| 2 | 无 Shift+Enter 多行 | `PushKeyboardEnhancementFlags`（已确认 crossterm 0.28 支持）+ 多行输入模型 + 渲染 |
| 3 | thinking 显示到对话 | provider 把 think 标签内联进 `content` → 需**剥离标签**（代码从不显示 `reasoning_content`） |
| 4 | 命令无提示 | **`CompletionState` 从未在 `draw.rs` 渲染**（数据早已齐全，只差画） |
| — | 中文退格 panic | `events.rs:131` 的 `cursor - 1` 落在多字节字符中间 |
