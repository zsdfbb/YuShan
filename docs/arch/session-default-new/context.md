# coding-agent session 管理：默认新会话 + resume 选项 — 架构上下文

## 概述

把 coding-agent 的会话行为从「启动自动恢复最近一个会话」改为「**默认开新会话**」，
并提供一个**显式的 CLI resume 选项**来恢复历史会话（不带具体目标时**列出会话供选择**）。

## 现有架构

### 模块边界

| 层 | 位置 | 职责 |
|---|---|---|
| 会话存储 | `crates/ys-session/` | `Session` trait（`messages` / `append` / `clear` / `replace_all`）；`MemorySession`、`JsonlSession` |
| 接线器 | `apps/coding-agent/src/wiring.rs` | `Wiring::persistent`（持久，落盘 + 启动恢复）/ `ephemeral`（一次性）/ `new_session`（`/new`） |
| 入口 | `apps/coding-agent/src/main.rs` | `parse_args`（flag 解析）、`sessions_dir`（落盘目录四档降级）、`run_interactive` / `run_single_turn` |
| 命令 | `crates/ys-tui-coding/src/commands.rs` | 12 条 `CommandSpec`（UI 侧表）；实现在 `apps/coding-agent/src/capabilities.rs` |
| 快照 | `crates/ys-tui-coding/src/view.rs` | `CodingView.session_path`，状态行显示当前会话文件 |

### 核心抽象

- **`Session` trait**：`messages() -> &[Message]`、`append`、`clear`、`replace_all`。
  `replace_all` 默认实现非原子（`clear + N×append`），`JsonlSession` 覆写为单次落盘。
- **`JsonlSession`**：一行一条 `Message` JSON；写策略 = 写 `{file}.jsonl.tmp` → `rename`（原子）；
  读时坏行**跳过并告警**、不终止；`append` **每次全量重写整个文件**。
- **`Wiring`**：持有 `model` / `session`（`Box<dyn Session>`）/ `events` / `sessions_dir` / `session_path`。
  Agent 无状态，会话归属在接线器（ADR-0010）。

### 关键数据流（现状）

```text
启动（TUI）
  → sessions_dir()                        # 4 档：$YUSHAN_SESSIONS_DIR > <HOME>/.yushan/sessions
                                          #      > <USERPROFILE>/… > <temp>/yushan-sessions-<pid>
  → Wiring::persistent(model, sink, dir)
      → latest_session_path(dir)          # 取文件名最大者（{unix秒}_{纳秒} ⇒ 字典序 == 时间序）
      → JsonlSession::open(path)          # 无则新建（惰性，首次 append 才落盘）
  → 正常运行；每回合 append → 全量重写文件

/new（capabilities::new_session）
  → 生成新 id → open + clear()（立即落盘空文件）→ 替换 session / session_path
  → 旧文件保留（不删）
```

`-p` / `--json` 用 `Wiring::ephemeral`（`MemorySession`，不落盘、不恢复）。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| 无新增 | — | — | 纯本地文件读写 |

## 约束

- **技术**
  - `parse_args` **只在首个参数位置判定 flag**（`--stats` 是唯一例外，且必须**紧邻**模式 flag）。
    新增 resume flag 的解析需遵守此约定。
  - TUI 走「三信道 / 两线程」；`Wiring` 构造发生在**起屏之前**（`rt.block_on`）。
  - 依赖方向：`ys-tui-coding` 不依赖 `ys-runtime`；命令的 UI 行为在 TUI 侧、能力实现在 app 侧，
    二者唯一契约是 `Request`（漏实现 = 编译错误）。
  - **列表选择发生在主 TUI 起来之前**（D11：启动期一次性）→ **不能用** `ys-tui-coding` 的
    `Prompter` 浮层（它只在主循环跑起来后可用）。本仓当前**无 `inquire`**，故交互面只能是
    启动期 stdin（R1）。`-p` / `--json` 同样无 TUI，必须共用同一交互面（否则三模式分叉两套）。
- **性能**
  - `JsonlSession::append` 全量重写文件（O(n) 写 / O(n²) 累积）。resume 一个大会话 = 一次性读全量。
  - 「列出会话」按 D6 只需两列（时间 = mtime、主题 = meta 首行）→ 每文件**只读首行**即可，无需全量解析。
    唯「无 meta 的老/空会话」需回退到读首条用户消息（少见）。
- **演进**
  - 现有「自动恢复最近会话」被移除后，**旧会话必须仍有可达路径**（resume 列表），否则等于静默丢历史。
  - `docs/adr/0021` 依赖「重启恢复会话 ⇒ 某类列表自然恢复」；本需求会改变这条前提的触发方式。
  - `docs/architecture.md` §12「会话落盘与恢复」需同步更新。
  - 已知缺口（既有）：多进程并发写同一文件无锁。
- **组织**
  - 无。

## 需求范围

### 范围内

- **所有模式**（TUI / `-p` / `--json`）改为「默认新会话」，且统一为**持久会话**（D4）。
- CLI **resume flag**：指定目标 ⇒ 恢复该会话；不指定 ⇒ **列出会话让用户选择**。
- 移除启动期「自动恢复最近会话」的隐式行为。

### 范围外（明确不做的）

- 外部会话容器导入（CodeWhale `ResumeImportReceipt` 形态）。
- 多进程并发写锁。
- TUI 内 `/resume` slash 命令与运行期会话切换（D11：resume 仅启动期一次性）。

### 关键场景

- 场景 1：无参启动 → 落到**新**会话文件，旧会话原样保留在磁盘。
- 场景 2：`<resume flag>` 不带目标 → 列出会话，用户选一个 → 恢复其历史，后续 append 写回该文件。
- 场景 3：`<resume flag> <id/path>` → 直接恢复指定会话（脚本友好）。
- 场景 4：resume 后又 `/new` → 正常换到新会话，不丢已恢复的历史。
- 场景 5：`-p` / `--json` 默认新会话；带 resume flag 时载入历史（含 model 等状态）后跑一次性任务。

## 已确认决策

- **D1（Q1）**：**全都改** —— TUI / `-p` / `--json` 三种模式一起调整。
- **D2（Q2）**：resume 走 **CLI flag**；不指定具体会话时，**提供会话列表供选择**。
- **D3（Q4）**：**默认使用新会话**；不再有隐式的「自动恢复最近」。
- **D4（Q1b）**：`-p` / `--json` **也持久化** —— 跑完把本次对话落到 `sessions/`，后续可被 resume。
  即三模式统一为「持久会话 + 默认新会话」；`Wiring::ephemeral` 的去留待设计定。
- **D5（Q3a）**：resume 同时支持**列表选择**与**指定 ID**。
- **D6（Q3b）**：列表两列 —— **时间 = 会话文件 mtime**（最后一次写入，反映最近活动）；
  **主题 = 机械派生**：首条**真实**用户消息（`Role::User` 且含文本），**存有界片段（500 字符）、
  显示时再截断**（50 字 + `…`）。**不调用模型** —— 与 Pi / Codex / CodeWhale 一致（证据见 `research.md` §3）。
  > **修订（2026-10-07，review T2）**：初稿是「存原文」，但列表只读首行且封顶 4 KiB（C4）——
  > 超长首条消息会让 meta 行被截断成非法 JSON、标题静默丢失。改为存有界片段（design §5.2/C6）。
- **D7（Q3d）**：主题**写进同一个 JSONL 文件**的 **meta 首行**（如 `{"kind":"meta","topic":"…"}`），
  消息行保持裸 `Message`。**必须「存下来」而非列表时现派生** —— `/compact` 会把首条改成 `Role::User`
  摘要（ADR-0017 决策 12），派生会漂移。`JsonlSession` 纳入 meta 并在**全量重写**时保住（否则 append 丢主题）；
  `Session` trait 加 `meta()` / `set_meta()`（`MemorySession` 同步实现）；`/compact` 的 `replace_all` 也保住 meta。
  列表读取只需读每个文件**首行**。
- **D8（Q3c）**：主题在**首个回合内**（首条用户消息已存在且 meta 尚无主题）**计算一次**并落盘；幂等。
  纯机械、无额外调用，故不存在「拖慢首答」问题。
- **D9（Q3e）**：**无模型、无失败路径**。首条用户消息为空 → 用默认标题（如「新会话」，参考 CodeWhale
  `DEFAULT_SESSION_TITLE`）。**不考虑历史旧版本兼容**（不写双格式解析的兼容层）。
- **D10（Q5）**：resume **恢复一切状态**（不是只恢复消息历史）。状态清单以现有 `meta` 首行承载：
  消息历史 + **model（provider + id）** + **thinking 显示开关** + **token 累计**（`TurnStats`）。
  参考 Pi：新会话即记录 model 与思考档位，resume 时恢复（见 `research.md` §2）。
  > **修订（2026-10-07，review T7）**：本仓 `thinking_visible` 是**内联思考的显示开关**（ADR-0025），
  > 不是 Pi 的**模型思考档位**。为免「两份真值漂移」，采用单真值机制：UI 拥有运行期真值、变化时
  > 经 `Request::SetThinking` 单向同步、app 侧缓存随**进程**（`/new` 不重置）、回合末落 meta（design §5.6）。
- **D11（Q6）**：resume 是**启动期一次性** —— 启动时选定会话、载入历史后进入正常回合循环；
  **不做运行期切换**（无 `/resume` 中途换会话）。参考 Codex `codex resume` 语义（`research.md` §1）。
- **D12（Q8）**：列表交互落点 = **起屏前 stdin 选择**（主 TUI / `Wiring` 之前）。理由：D11 要启动期一次性、
  D1/D4 要三模式统一，现有 `Prompter` 浮层只在主 TUI 起来后可用；唯有 stdin 一套覆盖 TUI / `-p` / `--json`。
  非交互（无 TTY）下「不带 id 的 resume」**要求显式 id** 并报错，不做 stdin 选择。证据见 `research.md` §6。
- **D13（Q7）**：目录增长/清理策略**归入范围外**（独立需求，与 resume 语义正交）。

## 后续建议

- 调研产物：`context.md`（本文）+ `research.md`（Pi/Codex/CodeWhale 对照）。
- 下一步：用 `arch-design` 出候选方案 —— resume flag 形状、`Wiring::persistent` 构造接口改动、
  meta 首行的具体格式（topic/model/thinking/tokens）、`-p`/`--json` 的落盘时机、列表渲染与选择器实现。
