# 0026 — 会话默认新建 + 显式 `--resume`：meta 首行存状态，机械标题，起屏前 stdin 选择

把交互式启动从「无条件恢复最近会话」改为**默认开新会话**；恢复能力收进**显式 CLI flag**
`--resume[=<id>]`（裸 flag = 列表选择，`=<id>` = 指定）。会话状态（标题 / model / thinking 开关 /
token 累计）写进**同一个 JSONL 文件的 meta 首行**（无 sidecar），`JsonlSession` 全量重写时保住它。
标题**机械派生**（首条真实用户消息，存**有界片段**、显示时截断），不调用模型。三模式（TUI / `-p` / `--json`）
统一为持久会话。

## Status

accepted（2026-10-07）· **修订（2026-10-07，review 后）**：见
[`../arch/session-default-new/review.md`](../arch/session-default-new/review.md) 的「处置状态」——
决策 4（标题机械派生）的 `title` 由「存原文」改为**有界片段（500 字符）**（4 KiB 首行读取封顶的前提；
决策 3「meta 存进 JSONL 首行」不变）；决策 5/6 补齐实现落点：`finalize_turn_meta` **三模式共用**
（`-p`/`--json` 也写 title/model/tokens）、
`main()` 顺序前移（meta 模型优先级需在 `parse_args` 之后才可能生效）、picker 输出一律走 **stderr**、
空列表走**默认新会话**、`split_leading_flags` 顺序无关、`persistent_new` 保留 `create_dir_all`。
**方案与取舍不变**，只补实现落点与不变量。

## 背景

- **需求**（原话）：「考虑 coding-agent 的 session 管理，默认不 resume 而是使用新的 session。
  可以通过 resume 选项提供 resume session 的支持。」
- **现状**：`Wiring::persistent`（`apps/coding-agent/src/wiring.rs`）启动即 `latest_session_path()`
  取文件名最大者恢复 —— 用户**没有「开新的」入口**；`-p` / `--json` 用 `Wiring::ephemeral`
  （`MemorySession`，不落盘）。
- **JSONL 只存 `Message`**（`crates/ys-session/src/jsonl.rs`，一行一条），无任何会话级元数据；
  `Message` 也无时间戳（`crates/ys-core/src/message.rs:13` 仅 `role` + `content`）。
- **`/compact` 会把首条改写成 `Role::User` 摘要**（ADR-0017 决策 12）—— 这决定了「标题不能列表时
  现派生，必须落盘」。
- **`/thinking` 是 TUI 本地 `LocalAction`**（`crates/ys-tui-coding/src/commands.rs:428`），
  app 侧不知情 —— 持久化 thinking 开关需要一条 UI → app 的通道。
- **业界对照**（`docs/arch/session-default-new/research.md`）：Pi / CodeWhale / Codex **默认都是新会话、
  resume 一律显式**；三家标题**都是机械派生**（首条用户消息或用户命名），**无一调用模型**；
  需要「启动期一次性、选定后才进会话」语义的（Pi / Codex）把选择器放在**会话开始之前**。

## 决策

### 1. 默认新会话；删除隐式「恢复最近」（D3）

删 `Wiring::persistent` 的自动恢复与 `latest_session_path`。`Wiring` 拆成两个显式构造
`persistent_new` / `resume`。**旧会话仍可达**（列表 / 指定 id），不构成静默丢历史。

### 2. `--resume[=<id>]`：裸 flag 列表选择，`=<id>` 指定（D2 / D5 / D12）

`=` 形式**消除与任务文本的歧义**（`--resume 修复bug` 无法区分 id 与任务）。
选择器在**起屏 / `rt.block_on` / `enable_raw_mode` 之前**用普通 stdin 跑（不碰 crossterm raw mode，
不碰 TUI 三信道）；**非 TTY 且不带 id ⇒ 报错要求显式 id**（脚本不静默挂起）。

### 3. 状态存进 JSONL 的 meta 首行，无 sidecar（D7 / D10）

首行 `{"type":"meta",...}`，其余行仍是**裸 `Message`**。`JsonlSession` 的唯一落盘点是
`flush_to_file`，改为**先写 meta 再写消息** ⇒ `append` / `clear` / `replace_all` / `set_meta`
四条路径天然保住 meta（`/compact` 无需特判）。判别：逐行「先 `Message`、失败再 `SessionMeta`、
皆失败则既有告警跳过」——`Message` 无 `type` 字段，判别安全。

**否决 sidecar**：`JsonlSession::append` 是**全量重写**（只从 `Vec<Message>` 重写文件），
sidecar 会引入「两文件一致性」问题；单文件让 meta 随消息原子落盘（tmp → rename）。

### 4. 标题 = 机械派生，不调用模型（D6 / D8 / D9）

首条**真实**用户消息（`Role::User` 且含文本；跳过纯 `ToolResult`），**存有界片段、显示时截断**
（修订，见 Status：改为 `chars().take(500)`）。
**素材取自 skill 展开之前**的原文 —— 否则 `<skill …>` 信封（~9k 正文）会成标题。
首回合算一次并落盘（幂等），此后**永不重算** ⇒ `/compact` 改写首条也不会污染标题。

**与三家一致、且是有意收敛**：初版曾定「模型生成主题」，调研三家后改为机械派生（零调用、
零失败路径、即时可用）。

### 5. 三模式统一持久化（D1 / D4）

`-p` / `--json` 由 `ephemeral` 改走 `persistent_new`（带 `--resume` 时走 `resume`），
跑完把 `session: <id>` 写 **stderr**（stdout 是数据流），使一次性任务也可被后续 resume。
`Wiring::ephemeral` **保留**（`MemorySession` 仍是测试与「不落盘」构造的唯一入口）。

### 6. resume 恢复一切状态，但启动期一次性（D10 / D11）

模型解析优先级：**env 凭证 > meta 的 provider+model（须有 auth）> state.json > 首个有凭证者**，
provider 无凭证时**降级而非失败**（不挡起屏）。`TurnStats` 用 meta 播种；`thinking_visible` 经
`Request::SetThinking`（新协议变体）回传并写进 meta。**不做运行期 `/resume`**（无中途换会话）。

### 7. `/new` 去掉「预建空文件」

原 `open + clear()` 的理由是「否则 `latest_session_path` 重启选回旧文件」——在决策 1 下**已失效**；
保留只会写无用空文件且注释变错。默认新会话本就**惰性落盘**（不预建）。

## Considered Options

- **可扩展优先：`SessionCatalog` / `SessionPicker` / `TitlePolicy` 三 trait**（候选 B）：当前
  **无第二消费者**（唯一会话后端是本地目录；决策 6 排除运行期 `/resume`；决策 4 不做标题策略配置）
  —— 投机抽象。**否决**。
- **sidecar `{id}.meta.json`**：与全量重写的一致性成本；且违背「归一到一个文件」的明确要求。**否决**。
- **meta 塞进 JSONL 首行的「带类型条目流」**（Pi 形态，含 `model_change`/`thinking_change` 历史）：
  本仓无 branch / fork，只需**当前值**；历史条目无消费者。**否决**（只留单一 meta 行）。
- **模型生成标题**：三家皆无；多一次调用 + 失败路径 + 首答延迟。**否决**，改机械派生。
- **`--resume` 带值（空格分隔）**：与任务文本歧义。**否决**，用 `=` 形式。
- **运行期 `/resume` 切换会话**（CodeWhale 形态）：与「启动期一次性」冲突，且需扩 `Request`
  与运行期换 `Wiring.session`。**否决**。
- **`append` 改 O_APPEND 单行（去原子性）**：改变 `JsonlSession` 的原子写契约，属独立取舍。
  **本轮不做**（复访项）。
- **`serde(flatten) extra` 兜未知字段**：前向兼容非当前需求（不背旧版本）。**暂缓**。
- **恢复 thinking 用「UI/app 各存一份」**：两份真值必漂移。**否决**，走 `Request::SetThinking`。

## Consequences

- **行为变更（破坏性）**：老用户启动后**不再自动看到上次会话**，须改用 `--resume`；启动期应给
  一行可发现性提示。
- **`-p` / `--json` 现在落盘**：CI / 脚本会积空会话（清理归范围外）—— 故必须打 `session: <id>`。
- **`parse_args` 契约放宽**：`--stats` 的「紧邻模式 flag 例外槽」泛化为 `split_leading_flags`，
  须配**解析矩阵测试**，否则侵蚀「任务文本逐字保留」。
- **`/compact` 与 meta**：靠 `JsonlSession` 的「先写 meta」保证，须**变异测试**钉死（删 meta 写分支 → 红）。
- **协议新增** `Request::SetThinking { visible: bool }`（`ys-protocol`）+ `CodingView.thinking_visible`
  初值 —— `ys-tui-coding` 仍不依赖 `ys-runtime`，依赖方向不破。
- **新增文件** `crates/ys-session/src/meta.rs`、`apps/coding-agent/src/sessions.rs`；
  改 `jsonl.rs` / `lib.rs` / `wiring.rs` / `main.rs` / `app_loop.rs` / `request.rs` / `view.rs` / `app.rs`。
- 方案对比与实施步骤：[`../arch/session-default-new/design.md`](../arch/session-default-new/design.md)；
  需求上下文：[`../arch/session-default-new/context.md`](../arch/session-default-new/context.md)；
  业界对照：[`../arch/session-default-new/research.md`](../arch/session-default-new/research.md)。
