# lsp-support — 同类实现调研（Pi / CodeWhale / Codex）

> 日期：2026-10-21。性质：**跨产品取舍对照**（按 `AGENTS.md` 的文档分工，竞品对照归 `docs/arch/**/research.md`）。
> 与 [`context.md`](./context.md) 的关系：本文只补对照，**不改写它的结论**。
>
> **方法**：本机源码直读（`tmp/pi`、`tmp/Codewhale` 为既有克隆）+ 已安装二进制实测。
> **置信度**：标 `[源]` 的结论读过源码或实测过；未标的是推断。
> 外部检索受限：`developers.openai.com` 对本机出口返回 403，故 Codex 一节以**公开源码仓库 +
> 本机安装的二进制**为准（对「有没有这个功能」这类问题，二者比文档更硬）。

---

## 0. 结论速览

| 项目 | LSP | 形态 | 对本需求 |
|---|---|---|---|
| **Pi** | **零** `[源]` | 工具面只有 `bash`/`read`/`write`/`edit`/`find`/`grep`/`ls`（+`powershell`） | 「本仓的主要参照」**在 LSP 上没有先例** —— 但也没需求 |
| **CodeWhale** | **完整**（~3.6k 行）`[源]` | post-edit 自动诊断（默认开）+ **1 个模型工具 5 个操作** + `/lsp` 命令 + `[lsp]` 配置 | 形状与本仓约束**高度同构**：零重依赖、失败非阻塞、单工具多操作、有界输出 |
| **Codex CLI** | **零** `[源]` | 靠 `shell` 跑构建/测试 + `file-search` 全文检索 | 与 Pi 一致 ⇒ **两家「无 LSP」阵营**都靠模型自己跑命令 |

**三分天下**：一家做了完整版，两家完全没做。**没有中间形态**（没人只做「手动诊断工具」而不做 post-edit 自动注入）。

---

## 1. Pi — 零 LSP `[源]`

工具清单即全部能力面（`packages/coding-agent/src/core/tools/`）：
`bash.ts` / `read.ts` / `write.ts` / `edit.ts` / `find.ts` / `grep.ts` / `ls.ts` / `powershell.ts`。

全仓（`packages/*/src`）搜 `lsp` / `language server` / `rust-analyzer` 无真命中；
搜 `diagnostic` 的命中全部无关（`evals` 的跑测诊断、`env/ssh.ts` 的 SSH 错误文本、
`codemode` 的 JS 引擎 stderr、`ai/src/utils/diagnostics.ts` 的模型调用诊断）。

⇒ **「改完对不对」完全交给模型**：自己跑 `cargo check` / `pytest` / `tsc`。Pi 也没有 post-edit 校验钩子。

---

## 2. CodeWhale — 完整实现 `[源]`

### 2.1 两个半边

**① post-edit 自动诊断（默认开，feature #136）**

```text
成功 edit_file / write_file / apply_patch
   -> edited_paths_for_tool()  取被改文件（apply_patch 还会剔掉被删的文件）
   -> LspManager.diagnostics_for(path, seq)
   -> 渲染 <diagnostics file="..."> 块
   -> 攒入 pending_lsp_blocks
   -> 下一次 API 请求前 flush 成一条 runtime 消息（不改 system prompt）
```

落点：`crates/tui/src/core/engine/lsp_hooks.rs`（`run_post_edit_lsp_hook` / `flush_pending_lsp_diagnostics`）。
原话：*"Failure is silent by design — a missing/crashing LSP server must never block the agent."*

**② 模型可见的 `lsp` 工具：1 个工具、5 个操作**

`crates/tui/src/tools/lsp.rs` —— `operation ∈ {diagnostics, read_lints, symbols, definition, references}`，
外加 `path` / `line` / `character` / `query`。`read_lints` 吃 **1–16 个换行分隔的工作区相对路径**，
为的是「不加第二个工具」就支持多文件（CHANGELOG #4070 明写 *"without adding another tool catalog entry"*）。
`approval_requirement` = `Auto`。

> 这一条**直接命中本仓的 token 预算约束**：加一个工具 ≈ +173 est tok/轮
> （`docs/arch/hooks-and-plugins/design.md:264`）⇒ 把 N 个操作塞进 1 个工具是本仓该抄的形状。

### 2.2 客户端实现

`crates/tui/src/lsp/`：`client.rs` 1339 / `mod.rs` 1793 / `registry.rs` 223 / `diagnostics.rs` 216 ≈ **3.6k 行**。

**手写 JSON-RPC over stdio，明确拒绝 `tower-lsp`**，理由与本仓 ADR-0015 同源（`client.rs` 文件头原话）：
*"it is a server-side framework and dragging it in here would add hundreds of unnecessary transitive
dependencies and slow down `cargo build` for every contributor. The LSP wire protocol is small enough
that handling it ourselves is a self-contained ~400 LOC."*

结构：3 个 tokio 任务（reader / writer / 公共 API）加 mpsc 通道；入站消息分流到
「按请求 ID 的响应槽」与「诊断队列」两条路。

### 2.3 边界与失败语义（最值得抄的一节）

| 项 | 取值 | 出处 |
|---|---|---|
| 握手超时 | 5 s | `client.rs` 的 `INITIALIZE_TIMEOUT` |
| 帧上限 | header 8 KB / frame 16 MB | `MAX_LSP_HEADER_BYTES` / `MAX_LSP_FRAME_BYTES` |
| post-edit 等待 | `poll_after_edit_ms` 默认 5000（外层再包 +100 ms） | `[lsp]` 配置 |
| 诊断上限 | `max_diagnostics_per_file` 默认 20 | 同上 |
| 严重度 | **默认只要 error**（`include_warnings = false`） | 同上 |
| 进程回收 | `kill_on_drop(true)` + `impl Drop for StdioLspTransport` + `shutdown_all()` | `client.rs:205,317`、`mod.rs:872` |
| 惰性启动 | 按语言一台，首次使用才 spawn，之后复用 | `transport_for(lang)` |
| 文档同步 | 只 `didOpen` / `didChange`，**不做** workspace 全同步 | 文件头自述 *"not full IDE smartness"* |

**三个失败态分开**（`DiagnosticPollOutcome`）：`Success(空)` / `Error` / `Timeout` ——
「索引完没诊断」与「等超时了」**不是一回事**。这正是本仓最怕的**假阴性**（`context.md` 的异常场景）。

**版本新鲜度**：`DiagnosticPublication { items, document_version, diagnostic_version }`，
外加 `source_revision = sha256(文件文本)`。自注：*"does not pretend that a matching URI alone proves
which text was checked."*

### 2.4 配置面

`[lsp]`：`enabled`（默认 true）/ `poll_after_edit_ms` / `max_diagnostics_per_file` /
`include_warnings` / `[lsp.servers]` 覆盖内置表 / `[lsp.custom.<ext>]` 注册任意 server
（`command` + `args` + `language_id`）。

内置表：`rust -> rust-analyzer`、`python -> pyright-langserver --stdio`、`go -> gopls serve`、
`typescript -> typescript-language-server --stdio`、`c/cpp -> clangd`、`java -> jdtls`、`php`、`vue`。

另有 `/lsp` 命令做 status / set（`crates/tui/src/commands/groups/project/lsp.rs`）。

---

## 3. Codex CLI — 零 LSP `[源]`

三条独立证据，方向一致：

| # | 证据 | 结果 |
|---|---|---|
| 1 | `openai/codex` 仓库 `codex-rs/` 的 **131 个 crate 全量清单** | **无 `lsp` crate**（有 `file-search`、`file-watcher`、`file-system`、`skills`、`hooks`、`tools`、`mermaid`… 没有语言服务器相关项） |
| 2 | `codex-rs/tools/src` 全部文件名 | **无 LSP 工具**（`code_mode` / `dynamic_tool` / `mcp_tool` / `tool_search` / `tool_spec`…） |
| 3 | 本机安装的二进制（`codex-cli 0.163.0`，250 MB vendor 二进制）字符串扫描 | `textDocument` = **0**、`publishDiagnostics` = **0**、`languageServer` = **0**、`didOpen` = **0**、`rust-analyzer` = **0**（`pyright` 的 40 次命中全是 `Copyright` 的子串，假阳性） |

补一条：`codex features list` 有 **175 个 feature flag**，**没有任何一个**匹配
`lsp` / `language` / `diagnos` / `semantic` ⇒ 也不是「藏在开关后面」。

> 注：仓库里确有 `codex-rs/diagnostics`，但它是**进程内遥测 gauge**（`Gauge::increment()` 那类），
> 与「代码诊断」无关 —— 名字撞车，别误读。

**它靠什么替代**：`shell`（模型自己跑 `cargo check` / `pytest`）+ `apply_patch` + 全文 `file-search`。
与本仓现状（`bash` + `grep`/`find`）**是同一路线**，只是本仓刚补上 `bash_bg` 让这条路线不必阻塞。

---

## 4. 对 YuShan 的直接启示

1. **「Pi 是主要参照」不构成「不做 LSP」的论据** —— Pi 没做，Codex 也没做，但两家都是**靠模型跑
   命令**的路线；CodeWhale 做了，且**它的每条取舍都撞在本仓同一批约束上**（拒绝重依赖、失败非阻塞、
   有界输出、单工具多操作、显式清理）。⇒ 结论不是「别人做了」，而是「**有人在同一组约束下做成了**」。
2. **`context.md` 的 D2b（改 `EditTool`）在 CodeWhale 有对应物，但落点不同**：它把 post-edit 诊断
   放在**引擎侧**（`lsp_hooks.rs` 加注入一条 runtime 消息），**没有**让编辑工具认识 LSP。
   本仓 `EditTool` 的数据流是「结果回喂」，等价做法是**把诊断块拼进 tool result** 或**追加一条消息**；
   两条都不需要 hook 系统，也不需要 `tools-basic` 依赖 LSP。
3. **若做，形状照它**：**一个 `lsp` 工具 + `operation` 枚举**（诊断 / 定义 / 引用 / 符号），
   而不是每个操作一个工具。本仓每工具 ≈ +173 tok/轮，这个差别是实打实的。
4. **它给出的上限也是成本证据**：3.6k 行 Rust + 一台常驻 server + 配置面 + 内置服务器表。
   这不是「半天的事」；而**诊断那一半**在本仓已被 `bash_bg` + CLI 检查器覆盖掉大半（`context.md` 的 D2c）。
5. **可抄的最便宜的三条**（与是否全做无关）：失败三态分开、`include_warnings` 默认关、
   `max_diagnostics_per_file` 截断 + 按严重度排序。
