# coding-agent 输入全局历史 — 架构上下文

> 产出：`/arch-explore`。用途：为「**记录用户敲进 TUI 输入框的历史，避免重新敲**」界定现状、
> 约束与范围，供后续 `arch-design` 使用。证据均带 `文件:行号`，对应本工作区（`master`，HEAD `d36912f`，工作区干净）。

## 概述

让交互式 TUI 记住用户**每次提交的原始输入**（含 `/` 命令），可跨会话持久化，之后用键调出、
不必重敲。**当前代码里完全没有这套东西** —— 输入缓冲是一次性的（`InputBuffer::take` 提交后清空），
进程退出即丢。

追问的原话：「支持 coding-agent 的输入全局历史……避免我找历史输入，避免重新敲。」

## 现有架构

### 模块边界

输入链路（三个 crate，两层）：

```
crossterm 事件
   │
   ▼
crates/ys-tui-coding        UI crate（不认识 Agent / Session）
   run.rs       同步阻塞事件循环（poll(50ms) → read → handle_key）
   events.rs    KeyEvent → App 状态迁移；submit() 在此
   app.rs       UI 交互状态（transcript / scroll / input / completion）
   input.rs     InputBuffer（编辑原语）
   draw.rs      ratatui 渲染（Chat / Input / Status 三 pane）
   │  ① Request（提交）      ▲ ③ Outbound（快照 / 回放）
   ▼                        │
apps/coding-agent           接线器（app_loop / capabilities / wiring / sessions / state）
       每回合 append 进 JSONL 会话文件（ys-session）
```

- **TUI 侧**：`crates/ys-tui-coding/src/events.rs:247` `submit()` 是唯一提交点 —— `let text = app.input.take()`
  取出全文，`commands::parse_with_skills` 分派本地命令或 `Request::Prompt`。
- **app 侧**：`apps/coding-agent/src/app_loop.rs:268` 的 `Request::Prompt(message)` 分支是唯一回合入口；
  消息随即进会话（`expand_skill_invocation` → `run_turn_once` → append）。
- **谁有「原文」**：只有 TUI 侧持有用户**原样**敲入的文本；app 侧只看到消息对象，且 `/skill-name`
  已在 `app_loop.rs:278` 被改写成 `<skill …>` 信封（标题素材为此专门保留 `raw_user_text`，见 `app_loop.rs:271`）。

### 核心抽象

| 抽象 | 位置 | 现状 |
|---|---|---|
| `InputBuffer { text, cursor }` | `crates/ys-tui-coding/src/input.rs:18` | 单/多行缓冲，`cursor` 是 **char 索引**；`take()` 提交后 `cursor=0`、`text` 清空 |
| `App` | `crates/ys-tui-coding/src/app.rs:23` | UI 状态：`transcript` / `scroll_offset` / `follow` / `input` / `completion` / `pending_prompt` / `pending_sessions` … |
| `handle_key` | `crates/ys-tui-coding/src/events.rs:74` | 唯一的键 → 状态迁移入口（补全浮层 → Ctrl 组合 → 普通键） |
| `CodingView` | `crates/ys-tui-coding/src/view.rs:43` | app 推来的**快照**（`session_path` / `is_first_run` / `model` / tokens …），不含任何输入历史 |
| `StateStore` / `AppState` | `apps/coding-agent/src/state.rs:14,19` | `~/.yushan/state.json`，字段仅 `last_active_provider` / `last_active_model`；0o600，可扩展 |
| `logging::default_log_path()` | `apps/coding-agent/src/logging.rs:22` | `~/.yushan/logs/yushan.log`（追加、best-effort、无 HOME 时静默） |
| `Config::default_user_config_path()` | `apps/coding-agent/src/config.rs:293` | `~/.yushan/config.json`（**用户手写、app 只读**） |

### 关键数据流

```
用户按键 ─► run.rs 事件循环 ─► events::handle_key ─► 编辑 app.input
                                     │
                            Enter ▼
                     events::submit (events.rs:247)
                       ├─ 本地命令 → 写 transcript，不进会话
                       └─ Request::Prompt → ① 信道 → app_loop（app_loop.rs:268）
                                                     └─ append 进 {id}.jsonl 会话文件
```

**输入历史的天然插入点就是 `submit()`** —— 它是「用户意图被确认」的唯一时刻，且此时原文在手。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `crossterm` 0.29 | 键盘/粘贴事件 | `events.rs:43` |
| `ratatui` 0.30 | 渲染 | 需 `unstable-rendered-line-info` |
| `serde_json` 1 | 已在 `ys-tui-coding` 依赖里 | 若历史落 JSONL 无需新增依赖 |
| `tokio`（仅 `sync`） | 三条信道的 mpsc 类型 | **UI 线程不跑 runtime**（`architecture.md` §1） |
| （历史提案，未定） | 无 | 见「未澄清问题」Q3：是否引入落盘 |

## 约束

- **技术**：
  - `ys-tui-coding` **不依赖 `ys-runtime`**、不认识 `Agent`（编译器强制，`architecture.md` §1 依赖表）；
    输入历史若落盘，只能由本 crate 自己用 `std::fs`，或经协议层交给 app —— **不能直接借 app 侧类型**。
  - **UI 线程是同步 `fn`，不跑 tokio runtime**（`run.rs:103` 文档）；落盘要么同步小 IO，要么不落盘。
  - 依赖方向：`ys-protocol → ys-tui-coding → apps/coding-agent`。往 `ys-protocol` 加类型可行但需评估必要性。
  - `InputBuffer.cursor` 是 char 索引 —— 回填历史必须走既有 `insert_str`（`input.rs:41`），不得直接改 `text`。
- **性能**：事件循环 `poll(50ms)`（`run.rs:61`）；落盘若同步且每次提交都写，需控制为**追加一行**级别，
  不得每次全量重写大文件（参考 `ys-session/src/jsonl.rs` 的历史教训：append 全量重写是 O(n²)）。
- **演进**：键位是**现有取舍的一部分**，改动会与既有决策冲突（下一节）。
- **组织**：轻量优先；纯 UI 花活有被砍的既定立场（`docs/arch/gap-closure/context.md:10`：
  Diff Viewer / Theme / 鼠标 / 可配快捷键砍掉；Markdown 渲染已由 ADR-0030 解除）。
  输入历史属**可用性**而非花活，但仍需论证它不属于「可配快捷键」那一类。

### ⚠️ 与既有决策的冲突点：键位与「不做」清单

| 出处 | 原文 | 冲突点 |
|---|---|---|
| `docs/arch/tui-stack-revisit/design.md:92` | 「**历史**（↑↓，持久化 `~/.yushan/history`）｜ rustyline 内建 —— **↑↓ 从「滚动」还给历史**」 | 该方案要把 ↑↓ 从聊天滚动改归历史 |
| `docs/arch/tui-pi-codewhale-borrow/design.md:262` | 「历史文件持久化 ｜ rustyline 自带 `~/.yushan/history`，**不做**」 | 明确列入**不做** |
| `docs/design-final/coding-agent-tui.md:253` | 「竖直方向归聊天（`Up/Down/PageUp/PageDown`）」 | **已改为**：`↑↓` 归历史，滚动让给 `PageUp/PageDown` 与 `Ctrl+↑↓` |
| `crates/ys-tui-coding/src/events.rs:24`（模块文档表） | `Up`/`Down`/`PageUp`/`PageDown` = **聊天滚动** | **已改为**：`↑` 取历史（历史为空才回落滚动），`Down` 仅浏览态内归历史 |
| `docs/arch/tui-resident-status/improvements-p1-p2-p3.md:197,273` | 「13. 输入历史（↑/↓）… **rustyline**」 | 旧提案绑死 rustyline（已随 `645e339` 删除） |
| `docs/arch/coding-agent-tui/tui-repl-context.md:94` | 「`rustyline` ｜ 行编辑 ｜ **已删除，需重新引入**」 | 引入 rustyline 才能白拿 history —— 但那是一次大替换 |
| `docs/arch/gap-closure/context.md:524`（Q7） | 「rustyline 是否替换 ratatui …… 本轮已决定**先不动**」 | 待决方向，未被采纳 |

**结论**：本需求不是「补一块空缺」，而是**要推翻两条已记录的「不做」并重划键位归属**。这必须由
`arch-design` 显式裁决（见后）。**键位归属已裁决**（Q4，见下）：采纳 readline 语义。

## 需求范围

### 范围内（按原话的最小理解）

- TUI 里提交过的输入文本被**记住**，可用键**取回**，不必重敲。
- **跨会话持久化**（用户说「全局」，且「记录一下」→ 期望下次还找得到）。
- 覆盖普通消息与 `/` 命令（用户明确说「历史输入」）。

### 范围外（明确不做的）

- **`-p` / `--json` 模式**：无交互输入，不涉及。
- **不做输入历史的全文搜索 UI**（除非 Q4 选中 Ctrl-R 方案）。
- **不借 rustyline 实现本需求**：Q7 未决（`gap-closure/context.md:524`），本需求不应顺手绑定该决定。
- 不做多进程并发写同一历史文件的强一致保证（沿用 `state.json` 同级 best-effort 口径）。

### 关键场景

- 场景 1：上次敲了长 prompt，这次想改一句再发 → ↑ 调出、编辑、Enter。
- 场景 2：跨会话复用某条 `/skill-name 参数` 命令。
- 场景 3：连按 ↑ 翻若干条，按 ↓ 回到更新的（直到回到「空」= 未提交的草稿）。

### 异常/边界场景

- 历史为空 / 文件不存在 / 文件损坏 → 静默降级为空历史（对齐 `StateStore::load`，`state.rs:30`）。
- 输入含多行（`Shift/Alt+Enter`）→ 落下的是含 `\n` 的多行条目，回填必须原样多行。
- 输入含 CJK → 落盘/读回必须 UTF-8 安全（注意字节切分铁律）。
- 大粘贴（bracketed paste `run.rs:488`）→ 是否入库、单条长度上限。
- 相邻重复提交 → 去重。
- 回填时输入框已有草稿 → 是否像 readline 一样先存草稿（stash）再翻历史。

## 未澄清问题

> 全部**待用户确认**。以下为本轮先问、尚未得到回答的问题原文。

- [ ] **Q1 范围**：只记「敲进输入框的原始文本」（含 `/` 命令），还是也要能回看/搜索整段回合（含 assistant 与工具输出）？
- [ ] **Q2 全局还是按项目**：单一全局文件（跨所有 cwd 共享，如 `~/.yushan/` 下），还是按 cwd 分片（同一仓库内共享）？
- [ ] **Q3 只进程内还是跨会话持久化**？若持久化，落在 `~/.yushan/`（与 `state.json` / `logs/` 同级）可接受吗？文件名（`history` / `input-history.jsonl`）？
- [x] **Q4 键位**（**与旧 as-built 冲突，已裁决**）：`Up/Down` 原本归聊天滚动。**采纳 (a) 的强化版 = readline 语义**：
      `↑` **只要历史非空就取历史**（不再看输入框是否为空 —— 首次按下把输入框现有内容存成草稿，
      `↓` 越过最新一条即原样还原并退出浏览态）；`↓` 只在浏览态内归历史。
      聊天滚动因此改由 **`PageUp` / `PageDown`**（逐页）与 **`Ctrl+↑` / `Ctrl+↓`**（跳到最早 / 最新）承担，
      退出底部另用 `End` / `Ctrl-End` / `Ctrl-Down`。**不采用** (b) 专用键（`Alt+↑↓` / `Ctrl-P/N` 在 Termux
      等终端上并非处处可发）与 (c) `Ctrl-R` 搜索浮层（那是独立的范围内新增项，本需求明确不做搜索 UI）。
      未决项只剩**持久化**相关的 Q2 / Q3 / Q5–Q8。
- [ ] **Q5 去重与上限**：相邻去重 + 保留最近 N 条（如 500）够吗？超限是截断尾部还是环形重写？
- [ ] **Q6 过滤/隐私**：输入可能含密钥或大段粘贴代码，是否要「跳过疑似密钥行」「限制单条长度」「禁止多行入库」之类的规则？
- [ ] **Q7 落盘时机**：每次 `submit` 追加（同步小 IO，可能阻塞 UI 一瞬），还是退出时批量写（崩溃即丢）？
- [ ] **Q8 归属哪一层**：写在 `ys-tui-coding`（自己 `std::fs`）还是 app 侧（新增 `Request`/`Outbound` 往返）？
      后者会动 `ys-protocol`（三模式共用的能力平面），需权衡。

## 后续建议

- 用 `arch-design` 做方案设计，重点裁决 **Q4（键位冲突）** 与 **Q8（归属层）** —— 二者决定是否触到
  `ys-protocol` / 既有键位 as-built 文档。
- ~~若采纳键位改动，需同步更新 `docs/design-final/coding-agent-tui.md:253` 与 `events.rs` 模块文档表。~~
  **已随 Q4 裁决完成**（`↑↓` 归历史、`PageUp/PageDown` + `Ctrl+↑↓` 归滚动）。
- 若落盘形态未定，可先做**进程内 + 注入式存储 trait** 的最小版（`prototype`），验证翻历史的手感后再定持久化。
