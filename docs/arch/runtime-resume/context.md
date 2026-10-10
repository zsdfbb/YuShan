# 运行期 `/resume`（切换到指定 session）— 架构上下文

> 产出：`/arch-explore`。用途：为「**coding-agent 运行期间提供 resume 命令，直接切换到指定 session**」界定现状、约束与范围，供后续 `arch-design` 使用。
> 证据均带 `文件:行号`，对应本工作区状态（`master`，HEAD `3a3bb86`，工作区干净）。
>
> **后续**：`/arch-design` 已完成 —— 见 `./design.md`（推荐方案 + 三候选对比）与
> `docs/adr/0033-runtime-resume-command.md`。本文的 Q1–Q10 在 design.md §4 逐条裁决；
> 约束 7/8 经核实**在本路径上不成立**（`app_loop.rs:371` 每回合刷新 `context_window`）。

---

## 概述

在 TUI 运行期间敲 `/resume` → **浮层列出既有会话，选一个直接切过去**（Esc 取消选择）。
切换后：当前 `Wiring.session` 换成目标会话、模型跟随该会话 meta、屏幕清屏并回放目标会话历史、后续回合续写目标文件。

本质是**把启动期 `--resume` 的能力搬进运行期**，形态对称于已有的 `/model`（无参浮层 / 有参直切）。

### ⚠️ 前提：这与两条既有决策**正面冲突**

本需求不是「缺一块」，而是**推翻一条被明确否决过的决策**：

| 出处 | 原文 | 冲突点 |
|---|---|---|
| `docs/adr/0026-session-default-new-and-resume.md:80`（决策 6） | 「**不做运行期 `/resume`**（无中途换会话）」 | 直接否定 |
| `docs/adr/0026-session-default-new-and-resume.md:97-98`（Considered Options） | 「**运行期 `/resume` 切换会话**（CodeWhale 形态）：与『启动期一次性』冲突，且需扩 `Request` 与运行期换 `Wiring.session`。**否决**。」 | 否决理由正是本轮要付的代价 |
| `docs/arch/session-default-new/context.md:126-127`（D11） | 「resume 是**启动期一次性** …… **不做运行期切换**（无 `/resume` 中途换会话）」 | 同源 |
| `docs/arch/session-default-new/context.md:128-131`（D12） | 列表交互落点 = **起屏前 stdin 选择**；理由之一是「D11 要启动期一次性」 | 运行期切换使 stdin picker 不再是唯一落点 |
| `docs/arch/commands/context.md:78, :122` | 早期命令规划把 `/import` `/resume` 列为**范围外**（理由：`JsonlSession` 未就绪，后续再做） | 该理由**已失效**（JsonlSession 早已落地），但否决在 ADR-0026 里被重新表述为设计取向 |
| ADR-0026 决策 4 的否决理由 | 候选 B（`SessionCatalog` / `SessionPicker` 三 trait）因「**决策 6 排除运行期 `/resume`** ⇒ 无第二消费者」被否 | 本需求若落地，该否决的**前提**（无第二消费者）随之动摇 |

**推论**：本轮产出必须包含一条 **ADR**（新增以取代决策 6，或修订 0026）—— 见「未澄清问题 Q5」。
ADR 文件按项目惯例**是历史记录，不就地改写**（参照 `docs/adr/` 现有编号序）；倾向**新增 ADR 0026-bis 并显式声明取代决策 6，保留 0026 其余决策**。

---

## 已定决策（本轮问答，2026-10-10）

| # | 决策 | 含义 |
|---|---|---|
| **D1** | **入口 = 浮层列表选择** | `/resume` 出示会话列表（title / 时间 / id），用户选一个；**Esc 可退出选择**（无副作用）。 |
| **D2** | **模型跟随目标会话 meta** | 复用启动期那条优先级链（env 凭证 > meta > state.json > 首个有凭证者）；provider 无凭证时**降级而非失败**。 |
| **D3** | **历史呈现 = 清屏并回放目标会话历史** | 与「用 `--resume` 打开该会话」的观感对齐 —— 切过去就等于「打开那个会话」。 |
| **D4** | **忙时排队，回合结束后生效** | 请求落在信道①，app 线程下一轮 `recv` 才处理 —— 正是 `/model` 现有的「下一回合生效」语义，**零额外机制**。 |

**D3 的直接推论（本轮最重的一块）**：

> **现状的启动期 resume 根本不在屏幕上回放历史。**
> `App::new(view)` 的 `transcript` 恒为 `Vec::new()`（`crates/ys-tui-coding/src/app.rs:87`），
> `CodingView` 只带 `message_count: usize`（`crates/ys-tui-coding/src/view.rs:59`）而**不带 messages**；
> `--resume` 后用户看到的是**空屏**（`/status` 里 `messages N` 是唯一的痕迹，`commands.rs:380`）。
> 文档侧的「回放」指的是**另一回事**：退出时把 TUI 自己的 transcript 倾到主屏 scrollback
> （`run.rs` 的 `exit_payload`，由 `exit_payload_replays_without_absolute_positioning` 钉住）。
>
> ⇒ D3 需要**一条今天不存在的「会话历史 → TUI transcript」数据通路**（见「未澄清问题 Q2」），
> 并且这条通路**同时会补上启动期 resume 的空屏问题**（两者共用同一条）。

---

## 现有架构

### 模块边界（与本需求相关的部分）

```
UI(ys-tui-coding)                     app 线程（apps/coding-agent）
────────────────                      ────────────────────────────────
/ 输入 → commands::parse              app_loop::run { recv Request → capabilities }
  → Action::Request(NewSession)  ──①──►  Request::NewSession
  → Action::Prompt(PickModel)            └─ capabilities::new_session → Wiring::new_session
  ← Outbound::Output / View      ◄──③──  out_tx
```

- 命令的**用户可见行为**：`crates/ys-tui-coding/src/commands.rs`（静态表 `COMMANDS` `:45`，本文件成稿时 12 条；**`/resume` 落地后为 13 条**；`CommandSpec` `:32`）
- 命令的**能力实现**：`apps/coding-agent/src/capabilities.rs`
- 唯一契约：`ys_protocol::Request`（8 变体，`crates/ys-protocol/src/request.rs:14-47`）与 `Outbound<V>`（4 变体，`request.rs:55-64`）
- **app 侧 `match` 穷尽** ⇒ 加变体漏实现是编译错误（这是设计上的优点，也是本需求的改动成本）

### 核心抽象

| 抽象 | 位置 | 与本需求的关系 |
|---|---|---|
| `enum Request` | `crates/ys-protocol/src/request.rs:14-47` | **无 `Resume`/`LoadSession` 变体**，只有 `NewSession`（`:41`）—— 必须新增 |
| `enum Outbound<V>` | `request.rs:55-64` | `Event / View / Output / Quit` 四变体。若回放走这里，需新增变体。**`-p` / `--json` 不消费它** —— 那两个模式直接收 `Receiver<Envelope>`（`main.rs:802 / :838 / :878`），故新增 `Outbound` 变体的改动面只有 `ys-tui-coding::App::apply_outbound` 与 `app_loop` 的构造点 |
| `enum Action` / `PromptKind` | `crates/ys-tui-coding/src/commands.rs:115` / `:151` | `PromptKind` 现有 3 个：`Login \| PickModel \| PickSkill` —— 浮层选择器的既有范式，`PickSession` 照它加 |
| `CommandSpec` / `COMMANDS[12]` | `commands.rs:32` / `:45` | 表是 `/help`、`/` 补全、`parse` 的**唯一来源**（三处派生）⇒ 加命令只改这一处 |
| `Wiring { model, session, events, sessions_dir, session_path }` | `apps/coding-agent/src/wiring.rs:34` | **切换 = 换 `session` + `session_path` + 可能的 `model`** |
| `Wiring::new_session` | `wiring.rs:114-129` | **运行期替换 `session` 的既有先例** —— 且带一条必须保持的不变量：**先 `open` 成功后才替换**（失败时状态保持原样） |
| `Wiring::resume` | `wiring.rs:71-78` | 启动期加载既有会话的入口（转调 `persistent_at` `:84-105`）；**运行期可直接复用** |
| `sessions::{list, resolve, pick}` | `apps/coding-agent/src/sessions.rs:56 / :102 / :118` | `list` 给出 `Listing { id, path, mtime, title }`（`:26-34`，**无消息数/token**）；`pick` 是 **stdin 交互**（`:132-170`），**TUI 用不了** ⇒ TUI 浮层需要另做 |
| `JsonlSession` / `SessionMeta` | `crates/ys-session/src/jsonl.rs` / `meta.rs:26-51` | 目标会话的读取与 meta（provider/model/thinking/tokens/turns/title） |
| `SessionPathHandle` | `apps/coding-agent/src/app_loop.rs:90-113` | 退出提示用的会话路径快照；`/new` 后在 `app_loop.rs:272` 刷新 —— **切换也必须刷新** |
| `CodingView` | `crates/ys-tui-coding/src/view.rs:43-72` | UI 侧唯一数据入口；`session_path: Option<PathBuf>`（`:66`）、`message_count`（`:59`）；**无 messages** |
| `App::transcript` | `crates/ys-tui-coding/src/app.rs:25` | UI 侧渲染缓冲，**启动恒为空**（`:87`）—— D3 的落点 |
| `Listing` | `apps/coding-agent/src/sessions.rs:26-34` | 若列表要经协议送到 UI，它是**候选搬迁对象**（apps → `ys-protocol`，须保持纯数据、零 tokio） |

### 关键数据流

**现状 `/new`（唯一的运行期会话动作）**：

```
TUI: /new
  commands.rs:232          → Action::Request(Request::NewSession)
  events.rs:274            → requests.blocking_send(req)              （UI 线程，非 async）
  app_loop.rs:267-273      → capabilities::new_session(&mut wiring).await
                             session_started = Instant::now()          （状态行时长归零）
                             session_path.record(wiring.session_path())（刷新退出快照）
  capabilities.rs:205-214  → wiring.new_session().await
  wiring.rs:114-129        → JsonlSession::open(dir.join("{id}.jsonl")) 先成功后替换；旧文件保留
  app_loop.rs:287-302      → Outbound::Output(lines) + Outbound::View(build_view(..))
  app.rs:110-113           → self.view = view; transcript.push(System(text))
```

**现状启动期 `--resume`**：

```
parse_args(main.rs:161-222) → resolve_resume(main.rs:643-680)
  ├─ Resume::Id(id)  → sessions::resolve → 未命中 ⇒ stderr + exit(1)
  └─ Resume::Pick    → sessions::list(mtime DESC) → sessions::pick（stdin；非 TTY ⇒ 报错 exit(1)）
                     → 空列表 ⇒ 回落默认新会话（不报错）
  → read_meta 读回首行（main.rs:678）
  ★ 全程在「建 runtime / 起屏」之前 —— 因为读回的 meta 参与模型选择
→ 模型解析优先级链（main.rs:1104-1152）：env 凭证 > meta（须 registry.auth_for 命中）> state.json > 首个有凭证者
→ Wiring::resume(model, events, sessions_dir, path)（wiring.rs:71-78）
→ 恢复：messages + meta；thinking_visible 初值（view.rs:69-73）；TurnStats 由 meta 播种（seed_stats main.rs:480-488）
   不恢复：cwd/workspace（meta 无此字段）、system prompt / 工具表 / skill（每次重装）、
          session_started（显式声明「属展示、非执行状态」main.rs:990-993）、
          **以及：历史不上屏（见 D3 推论）**
```

**切换后必须同步更新的量（现状 `/new` 已覆盖的 + 新增的）**：
`Wiring.session` / `Wiring.session_path` / 模型（跟随 meta）/ `TurnStats` 播种 / `session_started` / `SessionPathHandle` / UI `view` / transcript 回放。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| （无新 crate） | — | 本需求不引入新依赖；改动集中在 `ys-protocol` + `ys-tui-coding` + `apps/coding-agent` |
| `serde` | `Request` 的序列化契约 | 新增变体须照 `Request::Login.api_base` 的 `#[serde(default)]` 范式考虑向后兼容（`request.rs:27-31`）。**TUI 内进程内传 `Request` 不落盘**，但 `--json` 模式的契约稳定性仍需确认 |

---

## 约束

### 技术

1. **依赖方向**：`ys-tui-coding` **不依赖** `ys-runtime`（不认识 `Agent` / `Session`），**编译器强制**。⇒ 会话列表与历史回放的数据**必须经 `CodingView` 或 `Outbound` 过**，UI 侧不得自己扫目录/读 `JsonlSession`。
2. **`ys-protocol` 零 tokio**（`AGENTS.md` 硬性约束）：若把 `Listing` 搬进协议层，它必须是纯数据（现值 `id/path/mtime/title` 满足；`mtime: SystemTime` 无 tokio 依赖）。
3. **UI 线程必须无 runtime 上下文**：UI 用 `blocking_send`（`events.rs:274`），在 async 上下文 panic。⇒ 浮层的发起/取消**只能在 UI 事件循环里做**，不能 `rt.spawn` 出个 async 选择器。
4. **app 侧 `match` 穷尽**：新增 `Request` 变体必须同时改 `app_loop.rs` 的分发（漏改是编译错误，好事）。新增 `Outbound` 变体则只需改 `ys-tui-coding::App::apply_outbound` —— **`-p` / `--json` 不经 `Outbound`**（它们直接消费 `Receiver<Envelope>`，`main.rs:802 / :838 / :878`）。
5. **`Wiring::new_session` 的失败原子性必须保持**：**先 `open` 成功后才替换句柄**（`wiring.rs:114-129`）。运行期切换同理 —— 目标不存在/不可读/JSONL 损坏时**必须保持当前会话不变**，不得留下「半个切换」。
6. **模型重建路径已存在但**：启动期那条优先级链目前**内联在 `main.rs:1104-1152`**，不是可复用函数。运行期要「跟随 meta」就得把它**抽成共用函数**（否则两处语义会漂移）。
7. **`RunLimits` / `context_window` 构建期冻结**（`run_limits` 在 `build_agent` 一次构造）：跨 provider/模型切换后**窗口不跟随**（与 `per-turn-model-switch` 的 ⑦ / E3 同源问题）。切换目标是**不同窗口的模型**时会算错压缩阈值。⇒ **已被既有机制化解**：`app_loop` 每回合开头调 `set_context_window(config.context_window())`（**唯一调用点**，测试 `context_window_is_recomputed_after_model_switch` 钉住）—— 窗口下回合自动跟随 `config.model`，无需本需求另做。
8. **`ProviderCompat` 随 `config.provider`**：跟随 meta 换 provider 时 compat 必须一起换，否则 reasoning / tool-call 格式处理错。⇒ **已被既有机制化解**：`compat` 随 `config.provider` 经 `build_model()` 重建 —— 只要切换时改 `config.provider`，compat 下回合自动跟随。
9. **`Outbound` 是单一出站写者**：事件、`Output`、`View` 共用一条信道（容量 1024，`main.rs:235-239`），顺序即用户所见顺序。插入回放数据时**顺序语义要明确**（回放必须先于后续事件）。
10. **回合进行中 app 线程阻塞在 `agent.run_turn()`**，不可能 `recv` 信道① —— 这正是 D4「排队」天然成立的原因，**不需要** busy 状态位；但反过来也意味着**切换期间 UI 无从知道「已受理、待生效」**（见 Q8）。

### 性能

11. **目录扫描是有代价的**：「默认启动零扫描」是 session-default-new 的既定点（C4）。`sessions::list` 只 `stat` + 读首行、封顶 4 KiB（`sessions.rs:20`），但会话多时仍是 N 次 open。⇒ 扫描应**按需触发**（用户敲 `/resume` 时），不得塞进每次 `build_view`（那会变成每回合扫一次）。
12. **回放的内存/渲染预算**：长会话（数百条消息、含工具结果）一次性进 `transcript` 需要评估 `RenderCache`（`app.rs` 的 `md_cache`）与滚动行为。

### 演进

13. **`Listing` 刻意最小**（`:26-34` 只有 id/path/mtime/title，**无消息数/token**）—— 若浮层想显示更多（轮数、token），要先扩数据源与封装（`message_count` 之类的字段默认不读消息体）。
14. **多进程并发写同一文件无锁**是已知缺口（`session-default-new` 已知缺口）：运行期切换到「另一个进程正在写的会话」有整文件重写丢更新的风险。
15. **`-p` / `--json` 无运行期概念**（一次性进程）：本命令的范围天然限于 TUI。它们**不消费 `Outbound`**（直接收 `Receiver<Envelope>`），故回放通路若走 `Outbound` 新变体也**不会**牵动这两个模式。

### 组织

16. 文档/注释/commit **用中文**；代码标识符英文。
17. 新模块/接口改动须对应 `docs/design.md` §12 测试矩阵条目。
18. 测试隔离硬约定：临时目录名含**进程内原子序号 + `process::id()`**；进程级 env 改写走 `crate::test_env::env_lock()` + `EnvRestore`。
19. **关键逻辑必须做变异测试**（本项目多个真 bug 是「测试全绿时依然存在」）—— 切换的失败原子性、回放正确性都属此列。
20. **文档口径已有既存偏差**：`docs/architecture.md:706` 写「11 条 `CommandSpec`」，代码实际 **12 条**（`commands.rs:45`，`/skills` 未同步）。`/resume` 落地后建议一并校正。
    > **已处置（T14）**：`architecture.md` §14 已改为 **13 条**并补上原先整表缺失的 `/skills` 与新增的 `/resume [id]`；本节偏差关闭。

---

## 需求范围

### 范围内

- **R1. TUI `/resume` 命令（无参）** —— 浮层列出既有会话（至少：title + 时间 + id），可选、可 Esc 取消。
- **R2. 运行期切换的 app 侧能力** —— 新增 `capabilities::resume_session`：解析目标 → `Wiring` 层加载（复用 `Wiring::resume`）→ 换 `session` + `session_path` → **失败保持原状**。
- **R3. 模型跟随目标会话 meta**（D2）—— 抽出启动期优先级链为共用函数，运行期复用；provider 无凭证降级不失败。
- **R4. 历史回放**（D3）—— 新数据通路：目标会话 messages → UI transcript；清屏后回放。**顺带**决定是否同时修「启动期 resume 空屏」。
- **R5. 状态量同步** —— `TurnStats` 播种（照 `seed_stats`）、`session_started` 归零、`SessionPathHandle` 刷新、退出提示 `session: <id>` 打新 id。
- **R6. 排队语义**（D4）—— 显式记录为设计（不新增 busy 机制）。

### 范围外（明确不做）

- ❌ **`-p` / `--json` 的运行期切换** —— 这两个模式一次性、无交互循环；resume 只走 CLI flag（既有）。
- ❌ **会话分支 / fork / clone / `/tree` / `/import`** —— 早期已列范围外（`docs/arch/commands/context.md:72, :116`），与本需求正交。
- ❌ **会话的删除 / 重命名 / 清理** —— 目录增长策略归 D13 范围外（`docs/arch/session-default-new/context.md:134`）。
- ❌ **多进程并发写的加锁** —— 已知缺口，独立需求。
- ❌ **`/model` 的跨 provider 轻量切换** —— 属 `per-turn-model-switch` 那条线；本需求只在「跟随会话 meta」这一点上与它有交集（可能共享「重建 model 不落盘」的路径）。
- ❌ **回合内（轮粒度）切换会话** —— 与 D4 冲突，不做。

### 关键场景

- **S1（成功路径）**：`/resume` → 浮层 → 选「重构工具层」→ 清屏、回放其历史、状态行模型变成该会话记录的模型、`/status` 的 `session` 指向新文件。
- **S2（取消）**：`/resume` → Esc → 无任何副作用（不扫描残留状态、不改 session）。
- **S3（凭证降级）**：目标会话 meta 的 provider 当前无凭证 → 按优先级链落到 state.json / 首个有凭证者，**切换照常成功**。
- **S4（忙时排队）**：回合进行中敲 `/resume` + 选择 → 回合跑完才生效（与 `/model` 一致）。
- **S5（续写）**：切换后继续提问 → 新消息 append 进**目标文件**；退出时 `session: <id>` 是目标 id。
- **S6（切回）**：再 `/resume` 选回原会话 → 原会话内容与统计照旧（因为它一直在磁盘上、从未被删）。

### 边界 / 异常场景

- **E1**：会话列表为空 → 提示「没有可恢复的会话」，**不弹空浮层**（对齐启动期 `resolve_resume` 的空列表分支 `main.rs:664-666`）。
- **E2**：选中后加载失败（文件被删/不可读/JSONL 损坏）→ 明确报错、**保持当前会话不变**（约束 5）。
- **E3**：选中的就是当前会话 → no-op 提示（或浮层里标灰），不得产生副作用。
- **E4**：当前会话是**惰性未落盘**的新会话 → 切走不需要清理（本来就没文件）；但要注意退出提示不该打出旧 id。
- **E5**：长历史回放的渲染/滚动表现（约束 12）。
- **E6**：`/resume` 与 Esc 的按键竞态：选择器打开时 Esc 是「取消选择」而非「退出应用」（TUI 已有 Ctrl-C 两阶段退出语义，ADR-0031；Esc 与它不冲突，但需明确）。
- **E7**：`thinking_visible` 是否跟随目标 meta（见 Q6）。
- **E8**：目标会话的 meta 无 model（老会话 / 空 meta）→ 模型解析走到 state.json 档，不报错。

### 质量目标

- **语义一致**：运行期切到某会话，与「重启后用 `--resume=<同一 id>` 打开它」应给出**同一套**模型、统计与历史 —— 两处不得分叉（这是 D2/D3 的动因）。
- **失败不改状态**：切换的任何一步失败都不得让当前会话处于半切换状态。
- **不破依赖方向**：`ys-tui-coding` 不认识 `Session`；回放与列表数据一律经协议层。
- **零额外机制优先**：排队语义（D4）应直接复用信道①的既有性质，不引入 busy 标志。

---

## 未澄清问题

> **均已由 `./design.md` §4 裁决**（本节保留为探索时的原始问题清单）。

- [ ] **Q1 列表数据怎么到 TUI？** 三条候选：
      (a) **按需往返** —— TUI 发 `Request::ListSessions` → app 回 `Outbound::SessionList(Vec<Listing>)`，TUI 再开浮层（`Listing` 需搬进 `ys-protocol`）；
      (b) **塞进 `CodingView`** —— 照 `view.skills` 的既有范式；但会话列表随外部进程变化，且塞进 `build_view` 会带来每回合扫描（违反约束 11）；
      (c) **TUI 自己扫目录** —— 违反依赖方向（约束 1）与「数据/显示分离」，且要复制 `sessions.rs` 的 4 KiB 封顶等细节。**倾向 (a)**，待 `arch-design` 定案。
- [ ] **Q2 历史回放的载荷与转换位置？** messages → `TranscriptLine` 的映射放 **app 侧**（TUI 只收已渲染行）还是 **UI 侧**（TUI 收 messages 自己转）？承载用**新 `Outbound` 变体**（如 `Outbound::Replay(Vec<...>)`）还是 `CodingView` 加字段？**并决定是否顺带修「启动期 resume 空屏」**（D3 推论：同一条通路）。
- [ ] **Q3 是否支持 `/resume <id|路径>` 直切？** 用户只答了浮层形态；原话是「直接切换到指定的 session」。`per-turn-model-switch` 的 `/model` 是「无参浮层 / 有参直切」双形态 —— 是否照此对称？（成本低但多一个入口）
- [ ] **Q4 状态行「会话时长」与统计是否随切换归零/播种？** 启动期：tokens 由 meta 播种、**时长不恢复**（`main.rs:990-993`）。运行期照同一口径吗？
- [ ] **Q5 ADR-0026 决策 6 怎么处置？** 新增 ADR 显式取代决策 6（保留 0026 其余：默认新会话、meta 首行、三模式统一持久化…），还是修订 0026 原文？**倾向新增**（ADR 是历史记录）。另外：ADR-0026 决策 4 对候选 B 的否决理由（「决策 6 排除运行期 `/resume` ⇒ 无第二消费者」）**前提已消失** —— 是否在本轮一并复访？
- [ ] **Q6 `thinking_visible` 是否跟随目标 meta？** 启动期 resume 会取 meta 的 `thinking_visible` 作初值（`view.rs:69-73`）；但 session-default-new 的 T7 修订规定「UI 拥有运行期真值、`/new` 不重置」（`session-default-new/context.md` 的 D10 修订、`session-default-new.md:132-139`）。运行期切换**既不是启动也不是 `/new`** —— 三种语义需对齐，否则「两份真值漂移」会复现。
- [ ] **Q7 浮层列表的排序与呈现字段？** 沿用 `list` 的 mtime DESC？标题用 `derive_title`（50 字符截断，`sessions.rs:173-180`）？是否显示「当前会话」标记（避免 E3 误选）？
- [ ] **Q8 排队期间的 UI 反馈？** D4 下切换是「静默排队到回合末」。回合跑动中用户敲了 `/resume` 选完，屏幕上要显示什么（pending 提示？）还是什么都不显示（与 `/model` 现状一致）？
- [ ] **Q9 回放与「退出回放」的叠加？** 退出时 TUI 把 `transcript` 倾到 scrollback；切换后 transcript 含回放的历史 —— 退出时是否会重复倾倒（观感）？是否需要标记「回放行」不计入退出回放？
- [ ] **Q10 切换后 `context_window` / `compat` 跟随吗？** 约束 7/8：`RunLimits` 构建期冻结、`compat` 随 `config.provider`。目标会话模型窗口不同 ⇒ 压缩阈值错。是与本需求同刀解决，还是留给压缩那条线（`skills-and-compaction` D5/D6）？

---

## 后续建议

- **先裁决 Q5**（ADR 处置）：本需求推翻的是**明写的否决**，不是遗漏。设计文档不先立起来，实现就是设计偏离（`AGENTS.md` 硬性约束）。
- **用 `arch-design` 出方案**，建议按两条正交轴切开：
  1. **协议与数据通路**（Q1 + Q2）：`Request` / `Outbound` 的新变体、`Listing` 是否搬进 `ys-protocol`、回放的载荷粒度。这是**唯一碰协议层**的部分，也是 `-p` / `--json` 会被牵动的地方。
  2. **app 侧切换语义**（R2/R3/R5 + Q4/Q6/Q10）：复用 `Wiring::resume`、抽出模型优先级链、失败原子性、统计播种 —— 与启动期共用实现，避免两套语义分叉。
- **建议先做一条最小垂直切片**：`/resume` 浮层 → 切换 → **不回放**（仅清屏 + `/status` 可见），把回放（Q2）留作第二片。理由：回放要动协议与渲染两层，而「切换本身」已能验证 D2/D4 的全部语义。
  - 若采用这条切法，需临时接受「切换后屏幕是空的」—— 与现状启动期 resume 的观感**恰好一致**，因此不构成回归。
- **建议用 `prototype` 验证的假设**：
  - 浮层选择器的交互（列表长度、标题截断、当前会话标记）—— 静态分析给不出。
  - 长历史一次性回放的渲染/滚动体感（约束 12）。
- **实现期须补的测试**（对应 `docs/design.md` §12）：切换失败保持原状（变异测试：把「先 open 成功后替换」改成先替换）、Esc 无副作用、meta 无凭证降级、切换后退出打新 id。
- **文档需同步校正**：`docs/architecture.md:706`（11 条 → 12/13 条）、§14 命令表加 `/resume`、`docs/architecture.md:593` 关于 `/new` 换 `session`/`model` 两字段的表述（as-built 实际只换 session + session_path，见 `wiring.rs:114-129`）。
  > **已处置（T14）**：§14 命令表已改 13 条并补 `/skills` + `/resume [id]`，§12 补了运行期 `/resume` 一段；`architecture.md:593` 的 `/new` 两字段表述**未改**（不在 T14 范围内，仍待订正）。
