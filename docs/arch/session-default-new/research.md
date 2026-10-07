# Pi / Codex / CodeWhale 的会话与 resume 对照

> 跨产品取舍对照，供本需求决策参考（AGENTS.md：竞品对照写这里，不进代码注释）。
> 参考实现（只读）：`tmp/pi`（TypeScript）、`tmp/Codewhale`（Rust）。
> Codex 无本地源码，行为按公开 CLI 语义记录。

## 1. 默认行为与会话选择

|  | 默认启动 | 选会话（交互） | 指定会话 | 「最近一个」 |
|---|---|---|---|---|
| **Pi** | 新会话 | `--resume` / `-r` → TUI 会话选择器 | `--session <path\|id>`（文件或 UUID 前缀）、`--session-id <id>`（精确，缺则建） | `--continue`（与上面互斥） |
| **CodeWhale** | 新会话 | `/resume`（无参）→ picker | `/resume <session_id\|path>` | —（未成为独立 flag） |
| **Codex** | 新会话 | `codex resume` → picker | `codex resume <id>` | `codex resume --last` |

**共同点**：三者**默认都开新会话**，resume 一律**显式**。这印证 D3。
**CodeWhale 的 `/resume` 解析形状**（`crates/command-contract/src/facets.rs:1211`）值得抄：

```rust
enum ResumeSource {
    File(PathBuf),                                   // 路径
    Session { load_path: Option<PathBuf>, truncated_id: String, title: String },  // id / 前缀
    Imported(ResumeImportReceipt),                   // 外部容器导入
    NotFound { raw: String, error: String },
}
```

无参 → `open_resume_picker()`；有参 → `resolve_resume_source(raw)` 按「文件 → 会话 id/前缀 → 容器」三档解析。
**运行中禁止 resume**（「Cannot resume while runtime work is active」）——本仓 Q6 已定为启动期一次性，天然满足。

## 2. resume 恢复哪些状态（Q5 的关键参考）

**Pi 的答案是「把状态变更记进会话日志」**：会话是 append-only 的**带类型条目**流，而非纯消息列表。

```ts
type SessionEntry =
  | SessionMessageEntry          // 消息
  | ThinkingLevelChangeEntry     // 思考档位变更
  | ModelChangeEntry             // 模型变更
  | CompactionEntry | BranchSummaryEntry
  | CustomEntry | CustomMessageEntry | LabelEntry | SessionInfoEntry;
```

启动时（`packages/coding-agent/src/core/sdk.ts`）：
- **已有会话**：`agent.state.messages = existingSession.messages`，其余状态从条目流里重放得到；
- **新会话**：立即 `appendModelChange(...)` + `appendThinkingLevelChange(...)`，**存入初始 model 与思考档位，专为 resume 时恢复**。

**CodeWhale** 把会话元数据放状态库（`crates/state`，SQLite `threads` 表）：`model_provider` / `preview` / `created_at` / `updated_at` / `title` / `archived` / `current_leaf_id` …，列表按 `updated_at DESC`。即**每个会话带 model_provider 与时间戳**。

**映射到本仓**：YuShan 无 branch/fork，不需要 Pi 的「变更历史条目流」；只需要**当前状态**。故 D7 的**meta 首行**（每次全量重写时刷新为当前值）即可承载 —— 形状等价、代价更低。

## 3. 会话标题/主题是怎么生成的（含实现证据）

**三家都是机械派生，无一调用模型。**

| | 标题来源 | 生成方式（实现） | 截断 |
|---|---|---|---|
| **Pi** | `name ?? firstMessage` | `name` 由**用户**经 `--name` / `/rename` 写入（`sessionManager.appendSessionInfo`，`session_info` 条目）；无则回退首条用户消息 | 显示时截断 |
| **CodeWhale** | 首条**真实**用户消息 | `conversation_derived_title()` → `conversation_title_prompt()` 取首个非 runtime-handoff 的 user 文本 → `truncate_title(prompt, 50)` | **截断到 50 字 + `...`**（`crates/tui/src/session_manager.rs`） |
| **Codex** | `first_user_message` | 库列 `title == first_user_message == preview`（**逐字相同、不截断**，实测最长 4412 字符）；另可选 `name`（用户命名）优先 | **存储不截断，picker 显示时截断** |

证据：
- CodeWhale：`crates/tui/src/session_manager.rs` —— `conversation_derived_title` / `truncate_title` / `DEFAULT_SESSION_TITLE = "New Session"`；
  且**刻意跳过 runtime handoff 控制流量**（`classify_user_turn_prompt`），避免把注入的运行时提示当成标题。
- Codex：`~/.codex/state_5.sqlite` 的 `threads` 表实测 —— 22 条会话全部 `title == first_user_message == preview`；仅 2 条有 `name`。
  CLI：`codex resume`（picker）/ `--last` / `<UUID 或 session name>` / `--all`（带 CWD 列）；`archive` / `delete` 也按 id 或 name。
  注意 Codex **不跳过**注入消息 —— 首条若被 `<execution_mode>` 之类占据，标题就是它。

**结论（已采纳）**：本仓**改用与三家一致的机械派生**（D6）—— 首条真实用户消息、**存有界片段**
（500 字符，见 design §5.2 修订）、显示时截断，**不调用模型**（零失败路径、即时可用）。
存成有界片段而非原文，是因为本仓把标题放在 JSONL **首行**且列表只读首行（4 KiB 封顶）——
这是本仓独有的内部约束，不改变「机械派生」的定性。差异点：因 `/compact` 会把首条改写成 `Role::User` 摘要
（ADR-0017 决策 12），本仓**必须把主题存进 meta 首行**（D7），不能像 Codex 那样列表时现取 `first_user_message`。

## 4. 对本需求决策的支撑

| 决策 | 依据 |
|---|---|
| D2/D5：CLI flag + 列表选择 + 指定 ID | Pi `--resume` + `--session <id>`；CodeWhale `/resume [id\|path]` |
| D3：默认新会话 | 三家一致 |
| D6：时间 = 文件 mtime | Pi 的 `modified` 末级回退即 mtime；本仓消息无时间戳，mtime 是唯一低成本来源 |
| D6/D8：主题机械派生、不调用模型 | **三家皆非模型生成**（首条用户消息 / 用户命名）—— 本设计一致采用 |
| D10：恢复一切状态 | Pi 存 model + **思考档位**并恢复；CodeWhale 每会话带 `model_provider`。⚠️ 本仓的 `thinking_visible` 是**显示开关**（ADR-0025），非档位（design §5.6 修订） |
| D11：resume 启动期一次性 | Codex `codex resume` 语义；CodeWhale「运行中禁止 resume」 |

## 5. 未采纳项

- **Pi 的变更条目流**（model/thinking 历史）：本仓无 branch/fork，meta 首行足够。
- **Pi/CodeWhale 的用户命名（`--name` / `/rename`）**：本需求不做用户命名（D9），主题取机械派生。
- **外部会话容器导入**（CodeWhale `ResumeImportReceipt`）：超出本需求范围。

## 6. 会话选择器放在哪一屏（Q8）

| | 选择器位置 | 机制 |
|---|---|---|
| **Pi** | **主 TUI 之前** | `selectSession()` 先 `createStartupTui()` 起一个**独立 startup TUI**（alt-screen、键位、主题都与主 TUI 同源），选中后 `ui.stop()` 返回路径，**再**启动带 session 的主 TUI |
| **CodeWhale** | **主 TUI 之内** | `/resume` 是 slash 命令，`open_resume_picker()` 在运行中的 TUI 里开浮层；且「运行中禁止 resume」 |
| **Codex** | **会话启动之前** | `codex resume` 先出 picker，选定后才进入该会话 |

**结论**：需要「启动期一次性、选定后才进会话」语义的（Codex/Pi）都把选择放在**会话开始之前**；把 picker 放进运行中 TUI 的（CodeWhale）恰好配的是 **slash 命令**入口（本仓 D2 已排除 slash 命令）。

**本仓现状约束**（`crates/ys-tui-coding/src/`）：
- 现有 `Prompter` 抽象把「向用户要答案」做成**屏内模态浮层**（`TuiPrompter`，不切屏），已被 `/model`、`/skills` 复用 —— 但它是**主 TUI 跑起来之后**才可用。
- 会话（`Wiring`）在 `run_interactive` 里、**起屏之前**构造（`main.rs`），与「选定后才建会话」天然契合。
- `-p` / `--json` **没有 TUI**，却也要求支持 resume（D1/D4）→ 选择器不能只依赖 `ys-tui-coding`。
- 本仓**无 `inquire`**（历史文档提到过，现实现已改为 TuiPrompter）。

**选项**
- **A. 起屏前选（stdin）**：`Wiring` 构造之前，用纯文本列表 + stdin 选择。**三模式统一**（`-p`/`--json` 也能用），零 TUI 依赖，与 D11 完全契合；代价是观感朴素，且复用不了现有 TUI 选择器。
- **B. 起屏后选（TUI 浮层）**：先起 TUI，把列表画成浮层。观感好、复用 `Prompter`；但要**扩 `Request` 契约**（新增「加载会话」请求）、让 app_loop 支持**换 `Wiring.session`**，且 `-p`/`--json` 仍需另做一套 —— 与「启动期一次性」拉扯。
- **C. 折中**：TUI 走 Pi 式**独立 startup 屏**（ratatui，会话开始前），`-p`/`--json` 走 stdin；两套界面、两条路径。

**建议：A**。理由：D11 已定「启动期一次性」，A 与之一致；D1/D4 要求三模式统一支持 resume，A 是唯一一套代码覆盖三模式的方案；B 的 `Request` 扩展与运行期换 `Wiring` 正是 D11 排除的方向；C 的观感收益不足以抵两套实现。若日后要 Pi 级观感，可在 A 之上把 stdin 列表升级为独立 startup 屏（A→C 演进），不推翻数据流。

> 非交互场景（stdin/stdout 被管道，无 TTY）：Pi 用 `--session <id>` 走非交互路径。本仓同理 —— `-p`/`--json` 下「不带 id 的 resume」应**要求显式 id**（报错提示），不做 stdin 选择。
