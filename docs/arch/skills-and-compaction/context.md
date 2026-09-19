# Skills 支持 + 自动压缩上下文 — 架构上下文

> 产出：`/arch-explore`。用途：为两个需求（**① 支持 skills**、**② 自动压缩上下文**）界定现状、约束与范围，供后续 `arch-design` 使用。
> 证据均带 `文件:行号`，对应本工作区状态（`AGENTS.md` 有未提交修改；`crates/ys-tool/src/notice.rs`、`adapters/tools-web/`、`docs/adr/0016-*.md` 为未跟踪文件）。

---

## 概述

两件事被放在一起提，但**成熟度差一个数量级**：

- **自动压缩上下文**：**已经实现且在跑**（`BasicLoop` Step 4b，`crates/ys-loop/src/basic.rs:130-137`），有完整算法（切分 + 模型摘要 + 重建 session）。缺的不是机制，是**可观测性、测试、可配阈值，以及「真 `/compact`」**。
- **支持 skills**：**全仓零代码**（`.rs` 里 grep `skill` 只有 1 处无关命中），需要从零设计。用户明确要复用的是 `/Users/zs/Develop/JackCC` 里的 skill（57 个）。

一句话风险提示：**压缩已实现但把历史写成了 `Role::User`、零测试零可见性，且 `context_window` 恒为 128_000（无配置入口）—— 1M 模型实际在 11 万 token 处就被压缩，89 万 token 从未使用；skills 若把 57 条描述常驻系统提示，前缀会撑大 ~18.5KB（4.6k～9k token/请求）—— 这个成本必须显式接受并纳入压缩阈值的设计。**

---

## 已定决策（2026-09-19）

| # | 决策 | 含义 |
|---|---|---|
| **D1** | **skills 激活 = 工具 + `/` 命令，两者都要**（同 Claude Code） | ① 恒注册一个 `skill` 工具（模型自主调用，描述常驻系统提示）；② `/skill-name` 命令（用户显式触发）。**两者共享同一份 skill 定义**，不是两套数据 |
| **D2** | **发现路径 = `~/.yushan/skills/`（用户级）+ `.yushan/skills/`（项目级）** | 不扫 `~/.claude/skills`。**JackCC 的 57 个 skill 靠用户自行软链进 `~/.yushan/skills`**，代码不内置对 Claude Code 目录约定的依赖 |
| **D3** | **压缩 = 加固既有实现**，不重写算法 | 补可见性（事件）、补测试、阈值提到 `RunLimits`、`/compact` 接真压缩。**不动**估算器对账 / 摘要策略重写 / `Session` 重写 API（均延后） |
| **D4** | **推进顺序 = 先压缩加固，再做 skills** | 两个需求**分成两份独立设计**，不混在同一个 `arch-design` 里 |
| **D5** | ~~`context_window` 配置 = 内置 provider 表 + `config.json`（单值全局）~~ → **已被 D12 修订为按 `model` 解析** | 原案：① `ProviderInfo` 新增 `context_window`；② 新增 `~/.yushan/config.json`（用户写、app 只读）；③ 解析顺序 `config.json` > 内置表 > 默认；④ **单值全局**。**解析键已由 D12 从 `provider` 改为 `model`**；「用户写、app 只读」「失败落日志」「绝不阻断启动」等其余条款不变 |
| **D6** | ~~阈值与保留量全比例化~~ → **已被 D10 修订** | 原案「触发 = `window × trigger_ratio`；保留 = `window × keep_ratio`」的**形态**保留，**数值被 D10 替换** |
| **D7** | **第一刀范围 = A 骨架 + C 的三项 bug 修复** | 调研推翻了 D3 的部分前提：其中「`keep_from == len`」「空摘要静默丢历史」「压缩失败=静默失忆」是** bug 而非增强**，本轮一并修。`TokenLedger` / 溢出恢复 / 结构化摘要仍延后 |
| **D8** | **keep 段先只设 floor，不设 ceiling** | keep 是**每轮重付**的 token（1M 下 156k/轮），但工作集大小是任务属性而非模型属性 ⇒ 先用第一刀新加的事件收数据再调 |
| **D9** | **KV cache 成本纳入设计**（2026-09-19 增补） | 三处漏：**K-1 摘要调用必然 100% 缓存未命中**（换 system/清 tools ⇒ 最大单次输入全价重发）；**K-2 `Usage` 无缓存字段 ⇒ 完全盲**；K-3 压缩必然击穿缓存（不可消除，只能减小 keep）。**主项是「每轮背着大上下文」，缓存命中是 4 倍杠杆 ⇒ 第一优先级是保证命中，不是调 keep** |
| **D10** | **数值定稿：触发 = 窗口 × 85%；保留 = 窗口的 1%~10%（带）** | 修订 D6 的数值。**「128k 逐位等价」护栏正式放弃**（旧值是从 pi 抄的、无测量依据）。`trigger = 0.85w` 与 `keep ≤ 0.10w` 使 `trigger > keep` 恒成立 ⇒ **D-1 小窗口退化由构造消除**。依据：pi/Codewhale/Claude Code 三家触发落在 80%~92%、保留独立收敛到「约 10% 窗口」；KV cache 量化显示保留 50% 窗口比 1% 贵 1.66x。**与三家的唯一分歧：按窗口比例 vs 固定 20k** |
| **D11** | ~~摘要内容策略：只描述最主要的；引用类信息不必穷尽保留~~ → **已被 D13 推翻** | 原案「引用类信息可回头再找」被推翻，理由：**「改过哪些文件」是重复劳动 / 覆盖文件的唯一直接入口**，且 Codex/pi/Codewhale 三家独立收敛到「结构化骨架 + 显式保留精确引用」 |

### 评审驱动的新决策（`arch-validate`，2026-09-19）

| # | 决策 | 依据 |
|---|---|---|
| **D12** | **窗口解析粒度改为按 `model`**（修订 D5） | 评审 R5：`ProviderInfo` 每 provider 只有**一个** `default_model`（`provider.rs:9-15`），但 DeepSeek 至少两个（`deepseek-chat`/`deepseek-reasoner`）⇒ 同 provider 内切模型窗口不变；且全局覆盖会跨 provider 误用。窗口是**模型**的属性，不是 provider 的 |
| **D13** | **摘要采用 pi / Codewhale 的形状：结构化骨架 + 显式保留精确文件路径/命令/错误文本 + `<read-files>`/`<modified-files>` 清单**（推翻 D11） | 三家（Codex/pi/Codewhale）独立收敛；重读文件的 token 成本远高于在摘要里留几个路径。**「结构化摘要」因此从 D3 延后项进入第一刀** |
| **D14** | **`ys-model::ModelRequest` 加 `tool_choice`**（摘要调用传 `"none"`） | 评审 R1：K-1「复用 tools 以命中缓存」必须有抑制工具调用的手段，否则摘要可能被当工具调用 → 空摘要 → B-2。Codewhale 同处明确传 `tool_choice: "none"`。**触及核心 crate 公开 API，须 ADR 补记** |
| **D15** | **触发点减去输出预留**：`trigger_at = min(w × 0.85, w − output_reserve − 1024)` | 评审 R4：窗口需容纳模型输出（`max_tokens = 4096`）；8K 窗口下 `6800 + 4096 > 8192` ⇒ 溢出，而溢出恢复已延后。Codewhale 同形状 |
| **D16** | **压缩期本轮只加超时**（按 B-3 处理：超时即失败、session 不动）；**中断显式延后** | 评审 R6：`-p`/`--json` 下 `boundary = None`，一次 30s+ 的摘要调用无刹车。加中断要引入 app 层信号管线，成本高于本轮收益 |
| **D17** | **`CompactPolicy` 不设 `summary_max_tokens`** | 评审 R2：adapter 是 `config.max_tokens.or(request.max_tokens)`（`lib.rs:415`），app 写死 4096（`main.rs:610`）⇒ 请求里的值被静默覆盖。**本仓库已立此原则**（`summarize.rs:37-38`「设值会被静默覆盖，那是句假话，故不设」）。**不留死旋钮** |
| **D18** | **删掉合成的 assistant ack 消息**（`I have the context summary. Continuing.`），交替**由构造保证** | 评审 R7 升级：它是**伪造的模型输出**（写进 JSONL、重启重放、`/export` 带出、下次压缩再摘要一遍）；且它**根本没解决交替**（只把失败位置从 `kept[0]=User` 挪到 `kept[0]=Assistant`）。pi / Codewhale **都不加**合成 assistant 消息。构造保证：`kept[0]` 是 User 则摘要**并入**它，否则摘要独立成条。**`Role::User` 保留**（改角色另议） |
| **D19** | **`keep_ratio = 0.10`（带上沿）** | ① 带内成本差仅约 10%（1M 下 1% vs 10%：430K vs 475K eq/轮），对比 50% 档的 1.66x 可忽略；② 三家收敛点就是 ~10%；③ **先保真后调低比先有损后调高安全**。硬边界 `[1%, 10%]` 仍保证落带内 |
| **D20** | **实施拆两片**：片 1 = 配置 + 可见性 + 正确性；片 2 = 成本 + 摘要质量；第二刀 = `TokenLedger` | `tool_choice` 动的是 `ys-model` **公开 API**，值得单独评审；片 2 依赖片 1 的事件数据。**第二刀须赶在 D1（skills）之前** |

> **D7/D8 的设计已产出**：→ [`design-compaction.md`](./design-compaction.md) + [`../../adr/0017-compaction-hardening.md`](../../adr/0017-compaction-hardening.md)

**D5 的直接推论（必须在设计中体现）**：

1. **`~/.yushan/config.json` 是新文件** —— 与 `auth.json`（app 写凭证）、`state.json`（**app 会 `save()` 覆盖**，`state.rs:44-64`）职责分离；用户配置**绝不能放进 `state.json`**
2. 需要**路径覆盖钩子给测试**（照 `StateStore::set_override`（`state.rs:71-73`）/ `ProviderRegistry::set_auth_override` 的现成模式）
3. **解析失败走日志文件**（照 `state.rs:38-42` 的 `logging::log`，不 print、不 panic），回退到内置表
4. `main.rs::build_agent`（`main.rs:257-269`）**当前完全不调 `.limits()`** —— 需新增 `RunLimits` 构造并把解析结果传进去
5. `/status` 显示的 `context_window`（`view.rs:77` → `agent.context_window()`）会随之修正 —— 1M 模型不再误报 128000
6. 内置表与 `config.json` 的**优先级必须有测试锁死**（照项目「关键逻辑做变异测试」约定）
7. **值在启动期冻结**（`limits` 进 `Agent` 后无 setter）—— 改配置需重启。本轮接受（设计需显式记录）

**D6 的直接推论**：

8. `16384` / `20_000` 两个硬编码常量退役，改为 `RunLimits` 上的比例字段（或 `CompactionPolicy` 子结构）
9. **比例是策略、窗口是事实**：config.json 里推荐形状是 `{"context_window": N}` + 可选压缩比例（带默认值）
10. ⚠️ **与 D1 耦合**：skills 描述常驻 4.6k～9k token 必须计入触发阈值，否则 1M 窗口下实际可用对话预算被悄悄吃掉

**D1 的直接推论（必须在设计中体现）**：

1. 加 1 个工具 ⇒ 触及 ADR-0016 记录的 **9 处连锁点**（`EXPECTED_ORDER`、`vec!`、3 处 `assert_eq!(len,9)`、`prompt.rs` 的 snippet + guideline + 2 条断言）
2. **57 条 `name` + `description` 常驻系统提示**（实测 18,473 B）⇒ 与 **D3 的压缩阈值耦合**：留给对话的预算被压缩了 4.6k～9k
3. 工具面**必须恒定** ⇒ skill 列表是**启动期静态扫描的数据**，不支持运行期增删（与「工具面恒定」「prompt cache 前缀稳定」一致）
4. tool 路线 + `/` 命令路线的**优先级**需在设计中定（同一 skill 既被模型选中又被用户敲 `/` 时的行为）

**D2 的直接推论**：

5. 需要**分层去重**（项目级优先于用户级）—— 见 E3
6. `~/.yushan/skills` 不存在时不报错（E1）；JackCC 的接入是**文档 + 软链**，不是代码

---

## 现有架构

### 模块边界（与两个需求相关的部分）

```
TUI(ys-tui-coding)  ──Request──►  app_loop  ──run_turn──►  Agent(ys-runtime)
      ▲                                                            │
      └──── Outbound ◄── ChannelSink ◄── AgentEvent ◄── BasicLoop(ys-loop)
                                                              │
                                              session(ys-session) / registry(ys-tool)
```

- 命令的**用户可见行为**在 `crates/ys-tui-coding/src/commands.rs`（表 `COMMANDS`，`45-101`）
- 命令的**能力实现**在 `apps/coding-agent/src/capabilities.rs`（app 侧）
- 二者唯一契约是 `ys_protocol::Request`（`crates/ys-protocol/src/request.rs:13-42`，7 个变体）；**漏实现是编译错误**

### 核心抽象

| 抽象 | 位置 | 与需求的关系 |
|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs:26-33` | `spec()` + `async call(input, ToolContext)` —— **skills 若走工具路线落这里** |
| `ToolSpec` | `crates/ys-tool/src/spec.rs:5-22` | `{name, description, parameters}`，**刻意无 prompt 字段**（`docs/arch/tools-align-pi/design.md:188-197`） |
| `ToolContext` | `crates/ys-tool/src/context.rs:5-12` | `{boundary, cwd, workspace_root}` —— **拿不到 `Model`** |
| `ToolRegistry` | `crates/ys-tool/src/registry.rs:5-64` | `HashMap` + **保序 `Vec<ToolSpec>`**（保序为 prompt cache） |
| `Message` / `ContentBlock` | `crates/ys-core/src/message.rs:12-34` | `Role::{User,Assistant,System}`；`ContentBlock::{Text,ToolUse,ToolResult}`，**无消息级元数据挂点** |
| `Session` trait | `crates/ys-session/src/lib.rs:14-27` | 只有 `messages` / `append` / `clear` —— **无任何重写/替换 API** |
| `RunLimits` | `crates/ys-component/src/limits.rs:10-27` | `context_window` 默认 **128_000**；无压缩相关字段 |
| `Agent` | `crates/ys-runtime/src/agent.rs:55-134` | 持 `system_prompt: Option<String>`，**无 setter，启动期冻结** |

### 关键数据流（两个需求各自相关）

**压缩流（已存在）** —— `crates/ys-loop/src/basic.rs`：

```
每轮 model 调用前 → Step 4b（130-137）
  total_tokens = estimate_session_tokens(session.messages())
  if total_tokens > context_window - 16384:      ← 阈值硬编码字面量
      compact_session(ctx).await?                ← 401-466
        ① 从最新往回扫，累积到 ~COMPACT_KEEP_TOKENS(20_000) 处切分（405-414）
        ② keep_from <= 1 → 直接返回（417-419）
        ③ generate_summary(model, to_summarize)  ← 469-500，额外一次 model 调用，max_tokens=1024，system 写死
        ④ session.clear() 后重建（428-463）：
             [Context Summary]…[/Context Summary] 作为 Role::User（438-444）
           + 合成 Role::Assistant「I have the context summary. Continuing.」（447-455）
           + 重放 to_keep（458-463）
        ⑤ summary 失败 → 降级为 [Compression failed — proceeding with truncated history]（436）
```

**模型请求组装** —— `basic.rs:182-190`：`messages: session.messages().to_vec()`，`tools: registry.specs().to_vec()`，`system: ctx.system_prompt.clone()`。

**系统提示词构建** —— `apps/coding-agent/src/prompt.rs:13-56`：
1. `find_custom_prompt(cwd)`（`.yushan/SYSTEM.md` → `~/.yushan/SYSTEM.md`）命中即**整体短路**（14-18）
2. 工具清单从传入 `specs` **逐字保序**渲染（20-26），snippet 按 **name** 查表（`tool_snippet`，61-81）
3. guideline 按工具是否在场条件生成（`tool_guidelines`，83-138）
4. `<project_context>`：`~/.yushan/AGENTS.md` + 从 cwd 逐级向上遍历 `AGENTS.override.md`/`AGENTS.md`/`YUSHAN.md`/`CLAUDE.md`（`load_project_context`，145-173）

**装配顺序** —— `apps/coding-agent/src/main.rs`：
```
635: let (tools, hint) = toolset::build_tools(&workspace, web);
640: let system_prompt = prompt::build_system_prompt(&cwd, &specs);
654/670: let mut agent = build_agent(system_prompt, workspace, tools)?;   ← 冻结
```
→ **`build_tools` 早于 model 构建**（ADR-0016 点名的硬约束）。

### ⚠️ 压缩阈值的真实配置现状（2026-09-19 新发现）

**`context_window` 没有任何配置入口 —— 永远是默认的 `128_000`，与真实模型无关：**

| 检查点 | 证据 | 结论 |
|---|---|---|
| `build_agent` 是否设 limits | `apps/coding-agent/src/main.rs:257-269` —— 只调 `.tools()`/`.system_prompt()`/`.working_dir()`/`.approval()`，**无 `.limits()`** | 落回 `AgentBuilder::default()` 的 `RunLimits::default()`（`crates/ys-runtime/src/builder.rs:28`）→ **`context_window = 128_000`**（`crates/ys-component/src/limits.rs:27`） |
| `/v1/models` 是否带回窗口 | `apps/coding-agent/src/provider.rs:26-27` —— `ApiModel { id }`，**只解析 id** | 无 per-model 窗口元数据 |
| 是否有 env 覆盖 | `YUSHAN_*` 全集（实测 grep）含 `API_BASE`/`API_KEY`/`MODEL`/`CHANNEL_CAPACITY`/`FETCHED_DIR`/`SESSIONS_DIR`/`SEARCH_*` | **无 `YUSHAN_CONTEXT_WINDOW` 类变量** |
| `/status` 显示什么 | `crates/ys-tui-coding/src/view.rs:47,77` + `commands.rs:288-290` | 显示 `agent.context_window()` = **恒 128000（对 1M 模型是错的）** |

> `crates/ys-tui-coding/src/view.rs:130` 的 `Some(65_536)` 是**测试夹具默认值**，非生产路径。

**直接后果**：用 1M 上下文的模型时，YuShan 在 **≈111,616 token**（`128_000 − 16_384`）处就触发压缩，**其余 ~89 万 token 从未被使用**。

**三个常量在 1M 窗口下都比例失调**：

| 常量 | 现值 | 位置 | 1M 下占比 |
|---|---|---|---|
| 触发阈值 | `context_window − 16_384`（字面量） | `basic.rs:134` | reserve 仅 **1.6%** |
| 压缩后保留 | `COMPACT_KEEP_TOKENS = 20_000` | `basic.rs:16` | 仅保留 **2%** 历史 |

### 命令的四分类（既有架构结论，skills 的直接先例）

`docs/arch/coding-agent-tui/tui-repl-context.md:266-296`：

| 结果去哪 | 命令 | 对 agent 的影响 |
|---|---|---|
| 只给用户看 | `/help` `/status` `/copy` `/export` | 无 |
| 改配置 | `/login` `/logout` `/model` | 无消息（agent 回合开始自取） |
| 改 Session | `/new`（换）、`/compact`（重写） | 靠换/重写 Session |
| **展开成消息** | *（现有命令中无；**将来的 `/skill:` / prompt template**）* | **就是一条普通用户输入** |

> 该文档的结论（`284-289`）：**展开型命令的产物是文本，走与「纯文本输入」完全相同的路 —— agent 根本不知道它来自命令**。这让「命令」这个产品概念完全不泄漏进 agent。

⚠️ 但该文档是**已作废的行式 REPL 前身**（`AGENTS.md` 参考资料表把它列为「已作废的前身」），且其 §"范围外"把 **`ys-skill` crate 列为「明确不做」**。**这条路线的否决不自动延续到当前 ratatui 路线**（路线 B 已落地，git `5555446`），但需要显式裁决。

### JackCC 的 skill 事实（被复用的对象）

调研 `/Users/zs/Develop/JackCC` 的结论：

- **它不是 skill 运行时**，只是 skill 定义 + 安装脚本（`install.py:104` 把 `~/.claude/skills` 软链到 `claude/skills`）。加载逻辑在 Claude Code 二进制里。
- 实测本机：`~/.claude/skills -> /Users/zs/Develop/JackCC/claude/skills`（57 个 skill）。
- **格式**：每个 skill 一个目录，必需 `SKILL.md`，可选 `REFERENCE.md` / `references/` / `scripts/` / `assets/`。
- **frontmatter 字段**（实测）：`name`（57/57）、`description`（57/57）、`version`（lark 系列）、`metadata.requires.{bins,siblings}`、`metadata.cliHelp`、`argument-hint`、`license`、`disable-model-invocation`（3 个：`domain-model` / `ubiquitous-language` / `zoom-out`）。
- **无一个用 `allowed-tools`**（全仓 grep 零命中）。

**渐进式披露是 skill 的核心机制**（`claude/skills/write-a-skill/SKILL.md:62`）：
> 描述是**代理在决定加载哪个技能时看到的唯一东西**。它在**系统提示中与所有其他已安装的技能一起呈现**。

即：**只有 `name` + `description` 常驻系统提示；正文由模型决定后再 Read 进上下文**。`lark-doc/SKILL.md:24-29` 是典型两级披露（"执行前 MUST 先 Read `references/*.md`"）。

**规模与成本**（实测统计）：

| 指标 | 数值 |
|---|---|
| skill 数 | 57 |
| `SKILL.md` 总行数 / 总字节 | 6,788 行 / 409 KB |
| **`name` 总字节** | **715** |
| **`description` 总字节（UTF-8）** | **18,473** ≈ **4.6k～9k token** |
| 最长 description | `lark-sheets` 978 B；`lark-drive` 693 B |
| 最大 `SKILL.md` | `fireworks-tech-graph-main` 599 行 / 32.5 KB |
| 资源文件最多 | `lark-slides`（75 个非 SKILL.md 文件） |
| 无 `SKILL.md` 递归发现的深度约束 | `tmp/skills` 里嵌套两层（`skills/engineering/diagnose/SKILL.md`） |

**兼容性缺陷（复用 JackCC 必须处理）**：

| 缺陷 | 证据 | 影响 |
|---|---|---|
| 正文写死 Claude 专有工具名 | 见下表（实测逐 skill 统计） | **部分可用**：`WebSearch`/`WebFetch` 已有同名工具；**真堵点是 `Agent`（17 个）** |
| 私有 ` ```tool ` 围栏语法 | `git-status/SKILL.md:12-19`、`minimax-cli/SKILL.md:22,64,72` | 非标准 markdown，YuShan 无解释器 |
| `name` ≠ 目录名 | `fireworks-tech-graph-main/SKILL.md:2` 自称 `fireworks-tech-graph` | 用目录名索引会与自称冲突（57 个里唯一） |
| skill 目录**可写** | `sequential-workflow/learned/`、`plans/` 有 agent 写入的文件 | 不能假设只读 |

反过来：**37/57 个 skill 只用基础工具**（`read`/`write`/`edit`/`bash`），YuShan 有同名工具，可直接复用。

**依赖 Claude 专有工具的 20 个 —— 逐 skill 实测清单**：

| skill | 依赖的工具 | YuShan 现状 |
|---|---|---|
| `arch-design` | `Agent`, `WebSearch` | `websearch` ✅ / `Agent` ❌ |
| `lark-drive` | `WebFetch` | `webfetch` ⚠️ 语义不同 |
| `lark-openapi-explorer` | `WebFetch` | ⚠️ 同上 |
| `lark-slides` | `WebFetch` | ⚠️ 同上 |
| `lark-wiki` | `WebFetch` | ⚠️ 同上 |
| `lark-sheets` | `Agent`, `WebFetch` | 部分 |
| `quick-workflow` | `Agent`, `TaskCreate`, `TaskUpdate` | ❌ |
| `sequential-workflow` | `Agent`, `Bash`, `EnterPlanMode`, `ExitPlanMode` | `bash` ✅ / 其余 ❌ |
| `design-an-interface` · `fireworks-tech-graph-main` · `improve-codebase-architecture` · `lark-apps` · `lark-doc` · `lark-minutes` · `lark-shared` · `lark-vc` · `lark-vc-agent` · `markitdown` · `triage-issue` · `write-agents-md` | `Agent` | ❌ |

> **更正先前的粗判**：并非「20 个全废」。YuShan 已有 `websearch`/`webfetch`（ADR-0016）
> ⇒ `WebSearch`/`WebFetch` 的提及**名字对得上**（虽然 `webfetch` 语义是「落盘+指针」而非
> Claude Code 的「抓取并小结」，行为不等价）。**真正的硬堵点是 `Agent`（subagent）——
> 17 个 skill**，YuShan 完全没有子 agent 能力。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| JackCC 的 skill 文件 | 复用来源 | 纯 markdown，**无版本契约、无 lockfile** —— 上游改动会静默影响行为 |
| `~/.yushan/skills` ← 用户自行软链 | 安装机制（D2） | 代码**不内置**对 `~/.claude/skills` 的依赖 |
| **（潜在，唯一未决）YAML frontmatter 解析** | 读 `SKILL.md` | 见下 |

**新增第三方 crate 依赖：全项目只有一处存疑**（实测 `Cargo.lock`）：

| 需求 | 需要的依赖 | 现状 |
|---|---|---|
| `config.json` 解析（D5） | `serde` + `serde_json` | **app 层已有**（`apps/coding-agent/Cargo.toml`）→ **零新增** ✅ |
| `RunLimits` 加比例字段（D6） | 无 | `ys-component` **零外部依赖**（实测）→ 加 `f32` 字段**零代价** ✅ |
| 压缩可见性事件 | 无 | `ys-event` 加变体即可 ✅ |
| **skill frontmatter 解析（Q4）** | YAML | **`Cargo.lock` 里零 YAML crate**（实测）。`serde_yaml` 已于 2024-03 被上游**归档弃用**，引它需谨慎；0015/0016 立的「不引依赖」基线 ⇒ **无论选哪条路都要显式决策** |

---

## 约束

### 技术

1. **`Agent::system_prompt` 启动期冻结、无 setter**（`agent.rs:62,111`；`main.rs:640,654`）→ 运行期注入 skills 需要新 API（**ADR-0007 要求「新加 API 必须有 ADR」**，`docs/adr/0007-agent-public-api.md:71`）。
2. **工具面必须恒定**（`toolset.rs:68-72`；`docs/adr/0016-web-tools-design.md:58`）→ 若按登录态/技能数增减工具，**prompt 前缀抖动 → prompt cache 全废**。skills 若走工具路线，**只能恒注册一个 `skill` 工具，技能列表作为数据**。
3. **`ToolContext` 拿不到 `Model`**（`context.rs:5-12`；`docs/adr/0016-web-tools-design.md:15-16`）→ skill 如果要「自己调模型」（如 JackCC 里那些要求 subagent 的 skill）当前做不到。
4. **`Session` 无重写 API**（`ys-session/src/lib.rs:14-27`）→ 真压缩只能 `clear` + 逐条重放；对 `JsonlSession` 每次 `append` **全量重写文件**（`jsonl.rs:50-84`）⇒ **压缩一次是 O(n²)**。
5. **`ys-core`/`ys-event`/`ys-protocol` 不含 tokio（直接与间接皆无）**；`ys-tui-coding` 不依赖 `ys-runtime`（编译器强制）；`ys-component` 不直接用 tokio API（`AGENTS.md:189-196`）。
6. **依赖基线**：0015 否决 `ignore`/`regex`/`globset`/`walkdir`；0016 只破例一个 `htmd` 且限一个 crate（`docs/adr/0016-web-tools-design.md:62-68`）。⇒ **解析 YAML frontmatter 需显式决策**（自写极简解析器 vs 引依赖 vs 换格式）。
7. **加工具 / 改工具面的连锁点固定 9 处**（`EXPECTED_ORDER`、`vec!`、3 处 `assert_eq!(len,9)`、`prompt.rs` 的 snippet + guideline + 2 条断言）—— `docs/adr/0016-web-tools-design.md:103-104`。

### 性能

8. **57 个 skill 的 description 常驻 ≈ 4.6k～9k token/请求**（实测 18,473 B）。对比：工具数 7→9 是「每轮固定 +60~120 token」（ADR-0016）—— **skills 的量级是它的 ~50 倍，且是每轮固定成本**。
9. **token 估算器未与真实 `Usage` 对账**（`crates/ys-loop/src/token.rs:18-41`；真实 usage 在 `basic.rs:213` 累加）⇒ 压缩触发点会系统性偏。
10. **压缩不可中断**：`Step 4b` 是两条串行 `await`（摘要调用 + 重建），期间的 `Abort` 只能落到轮边界（`basic.rs:139-147`，注释自称「几乎不可达」）。且 `-p`/`--json` 下 `boundary` 恒 `None`（`wiring.rs:115-122`，ADR-0015 的 B2）⇒ **超时是唯一刹车**。

### 演进

11. **`docs/arch/coding-agent-tui/tui-repl-context.md:158,216` 把 `ys-skill` crate 列为「明确不做」**，且指出它与 `gap-closure` 的 `ys-` 命名方案冲突（**方案里没有 `ys-skill`**）。该文档是已作废前身，但**这条否决需显式推翻**。
12. **`docs/arch/tui-status-display/context.md:117` 否决「context 占比」字段**（理由：需 `RunLimits` API 扩展）；同处还写「**Auto-compact 状态指示**（compact 是手动命令）」❌ —— **这条已被现状推翻**：自动压缩早已实现（`basic.rs:130-137`），否决理由（"手动命令"）与事实不符。
13. `docs/extension-points.md:51` 已把「自动压缩上下文」归入 **Compaction（回合内，已有）**并标注「**已实现**」—— 与本次调研一致。

### 组织

14. 文档/注释/commit **用中文**；代码标识符英文（`AGENTS.md:205`）。
15. 新模块/接口改动须对应 `docs/design.md` §12 测试矩阵条目。
16. 测试隔离硬约定：临时目录名含**进程内原子序号 + `process::id()`**；进程级 env 改写走 `test_env::env_lock()` + `EnvRestore`（`AGENTS.md:210-214`）。
17. **关键逻辑必须做变异测试**——本项目多个真 bug 是「测试全绿时依然存在」（`AGENTS.md:216`）。

---

## 需求范围

### 范围内

**A. 自动压缩上下文（加固既有实现，见 D3）**
- A1. 压缩的**可观测性**：发事件（transcript 可见「已压缩 N 条 → M 条」），而不是静默丢历史
- A2. 压缩的**测试**（当前零测试：全仓 `compact_session`/`generate_summary` 只出现在 `basic.rs` 自身）
- A3. **窗口可配 + 阈值比例化**（D5/D6）：新增 `~/.yushan/config.json` + `ProviderInfo.context_window`；`main.rs::build_agent` 接上 `.limits()`；`16384` / `20_000` 两个硬编码常量改为比例
- A5. **`/compact` 接真压缩**（当前是「清空会话」占位，`capabilities.rs:194-200`）—— 需给 capability 传 `model`
- A6. 摘要消息的**角色形态**（现为 `Role::User`，`basic.rs:439-441`）—— 是否本轮处理待定（Q8）
- A7. 压缩期间的**边界处理**（`-p`/`--json` 下超时兜底）—— 是否本轮处理待定（Q16）

> **D3 明确延后**：A4 估算器与真实 `Usage` 对账、摘要策略重写（结构化/分层）、`Session` 重写 API 消除 O(n²)。

**B. Skills 支持（从零，见 D1/D2）**
- B1. **发现**：递归扫 `SKILL.md`（路径 = `~/.yushan/skills/` + `.yushan/skills/`），解析 frontmatter（`name` + `description`，缺失回退目录名）
- B2. **渐进式披露**：`name` + `description` 常驻系统提示；正文按需加载
- B3. **激活（两条路都要）**：① 恒注册 `skill` 工具（模型自主）；② `/skill-name` 命令（用户显式）—— **共享同一份定义**
- B4. **复用 JackCC**：靠用户自行软链进 `~/.yushan/skills`（D2），代码不内置 `~/.claude` 路径
- B5. 处理 `disable-model-invocation`（不进模型候选，仍可显式 `/` 调用）
- B6. 兼容缺陷的最小处置（`name`≠目录名、` ```tool ` 围栏、Claude 专有工具名的静默失效）

### 范围外（明确不做）

- ❌ **Hook 系统**（ADR-0014，有意推迟）
- ❌ **Subagent / Agent 工具**（JackCC 里 16 个 skill 依赖它；YuShan 无此能力）
- ❌ **Plan 模式 / TodoWrite / TaskCreate**（JackCC 少数 skill 依赖）
- ❌ skill 的**热插拔 / 市场 / 动态增删**（与「工具面恒定」冲突；建议启动期静态扫描）
- ❌ skill 的**权限/沙箱**（本项目「不做安全」）
- ❌ skill 里 `scripts/` 的**自动执行**（若做，须走现有 `bash` 工具，不自建执行器）

### 关键场景

- **S1（压缩基本路径）**：长程任务累积超阈值 → 轮边界自动压缩 → transcript 可见提示 → 任务继续，模型保有摘要上下文
- **S2（压缩降级）**：摘要 model 调用失败 → 降级为截断历史，任务不中断
- **S3（手动压缩）**：用户敲 `/compact` → 立即压缩（不等阈值）→ 反馈「压缩前/后 token」
- **S4（skill 自主激活）**：用户说「检查 git 状态」→ 模型从描述里选中 `git-status` → 正文进上下文 → 按 SKILL.md 执行
- **S5（skill 显式激活）**：用户敲 `/git-status` → 正文进上下文（不等模型判断）
- **S6（不看 skill 列表也能用 `/` 发现）**：补全浮层列出 skill（源自同一张表）
- **S7（`disable-model-invocation`）**：`/zoom-out` 可用，但不出现在模型的候选描述列表里

### 边界/异常场景

- **E1**：skills 目录不存在 / 权限不足 / 空目录 → 不报错，静默零 skill
- **E2**：`SKILL.md` frontmatter 缺失 / 畸形 → 跳过该 skill 或回退目录名（需定策略）
- **E3**：同名 skill 出现在两个扫描路径 → 去重策略（就近优先？先到先得？）
- **E4**：单个 skill 正文极大（`fireworks-tech-graph-main` 32 KB）→ 进上下文后可能触发压缩
- **E5**：**压缩把注入的 skill 正文摘要掉** → 长任务中 skill 指令丢失（与 A 交互）
- **E6**：`keep_from <= 1` 短路（`basic.rs:417-419`）→ 首条消息巨大时永不压缩

### 质量目标

- 压缩：**成功率与可观测性**优先（模型知道被压缩过、用户看得见），其次是省 token
- skills：**复用率**（37/57 可直接跑）优先，其次是启动开销
- 两者都不能破坏既有不变量（工具面恒定 / prompt 前缀稳定 / 依赖方向）

---

## 未澄清问题

**已由上节「已定决策」解决**：~~Q1 激活机制~~ → D1（工具 + `/`，两者都要）；~~Q2 发现路径~~ → D2；~~Q5 压缩深度~~ → D3（加固）；~~推进顺序~~ → D4。

**skills 设计前仍需回答**

- [ ] **Q3 是否需要新 crate？** 前身文档把 `ys-skill` 列为「不做」且与 `ys-` 命名方案冲突。放在 app 层（`apps/coding-agent/src/skills.rs`）是否够？（**倾向 app 层** —— skill 是产品概念，不是核心能力；但若 TUI 侧补全浮层需要读 skill 列表，则 `ys-tui-coding` 也要拿到数据，需定数据通路）
- [ ] **Q4 YAML frontmatter 怎么解析？** 自写极简行解析（守住不引依赖基线）／引 `serde_yaml`（需 ADR）／改格式？**实测 57 个 skill 的 frontmatter 结构简单**（`name:` / `description:` 两种标量 + 少量 `metadata.*` 嵌套 + 折叠 `>`/`>-` 块），自写解析器可行但需覆盖 4 种 description 写法（纯文本 / `>` / `>-` / 双引号）
- [ ] **Q6 `disable-model-invocation` 的 3 个 skill**（`domain-model` / `ubiquitous-language` / `zoom-out`）如何表现？—— 不进模型候选列表（省 ~1KB 常驻），但仍需 `/zoom-out` 可调用
- [ ] **Q7 正文里的 Claude 专有工具名**（`Agent` 16 个 / `WebFetch` 5 个 / `EnterPlanMode` / `TaskCreate` / `WebSearch`）要不要做**符号映射或降级提示**，还是原样注入、失效自负？（影响 20/57 个 skill）
- [ ] **Q11 skill 列表注入系统提示的精确位置**（工具清单后？`<project_context>` 前？）—— 直接影响 prompt cache 前缀稳定性
- [ ] **Q13（新）** 模型自主调用 skill 时，正文进上下文的**形态**：作为 `ToolResult` 内容？还是后续追加一条独立消息？前者最省事但正文会带 `ToolResult` 语义
- [ ] **Q14（新）** skill 正文是否需要在**每次调用时重新读盘**（skill 目录可写，见 JackCC 的 `learned/`），还是启动期缓存？

**压缩加固设计前仍需回答**

- [ ] **Q8 压缩后摘要消息用什么角色？**（现为 `Role::User`，`basic.rs:439-441`；`Role::System` 在 `ys-core` 存在且 adapter 已映射，但全仓无生产者）
- [ ] **Q9（已降级，见 D5/D6）** 窗口配置入口与阈值形态已定。**设计时仍需定的细节**：
  - (a) **`trigger_ratio` / `keep_ratio` 的默认值**（`0.9` / `0.15`？）
  - (b) 这两个比例**是否也进 `config.json`**，还是只作内部常量（D5 的「单值」指的是窗口，未表态比例）
  - (c) `ProviderInfo` 内置表的**初始值**：哪些 provider 填多少（或全填 `None` 只靠 config.json）
  - (d) config.json 的**完整 schema**（是否预留其他字段位？`#[serde(default)]` 的向后兼容范式照 `Request::Login.api_base`，`request.rs:30-31`）
- [ ] **Q10 `/compact` 与自动压缩共用同一份 `compact_session` 吗？** 若共用，capability 需要拿到 `model` ⇒ **改 `capabilities.rs` 签名**（现为 `compact(wiring: &mut Wiring)`，`capabilities.rs:195`）
- [ ] **Q12 是否需要给 `Session` 加重写 API** 以避免压缩时的 O(n²)？→ **D3 已决定延后**，但若实测压缩耗时不可接受需回来重开
- [ ] **Q15（新）** 压缩事件的具体形状（新增 `AgentEvent` 变体？还是走 `Outbound::Output`？）—— 决定 transcript 如何显示
- [ ] **Q16（新）** 压缩期间 `Abort` 的处理（`basic.rs:139-147` + `-p`/`--json` 的 `boundary=None`）是否本轮一并解决？

---

## 后续建议

按 **D4（先压缩加固，再做 skills）** 排：

### 第一刀：压缩加固（→ `arch-design` 一份方案）

机制已在跑（`basic.rs:130-137`），边际成本低、风险低。设计需覆盖 D3 圈定的四件：**可见性（事件）/ 测试 / 窗口可配 + 阈值比例化（D5/D6）/ `/compact` 接真压缩**。

- **配置链路是本刀最大的一处结构改动**，且牵动三处：① 新文件 `~/.yushan/config.json`（+ 测试覆盖钩子）；② `ProviderInfo` 加 `context_window`；③ **`main.rs::build_agent` 接上 `.limits()`**（当前完全不调，是 1M 模型被当 128k 用的根因）。先在设计中定这条链路，其余都挂在它下面
- **`/compact` 接真压缩会改 `capabilities.rs` 签名**（须拿到 `model`）—— 本刀第二处结构改动
- 可见性会牵动 `tui-status-display/context.md:117` 的既有否决（「Auto-compact 状态指示」），但**该否决的理由已被现实推翻**（它写的是「compact 是手动命令」，而自动压缩早已存在）—— 需在设计中显式更新这条记录
- 补测试时注意 E6：`keep_from <= 1` 短路（`basic.rs:417-419`）—— 首条消息巨大时永不压缩，是个真实可复现的边界
- **优先级链必须有测试锁死**（`config.json` > 内置表 > 默认），**建议做变异测试**（`AGENTS.md:216`）：本项目多个真 bug 是「测试全绿时依然存在」

### 第二刀：skills（→ 另一份独立 `arch-design`）

先答 **Q3 / Q4**（crate 落点 + frontmatter 解析），它们决定数据通路与依赖基线。

- **最小垂直切片优先**：扫描（D2 两条路径）→ 解析 frontmatter → 注入系统提示 → 1 个 `skill` 工具 + `/skill-name` 命令 → 拿 `git-status` 之类**只用基础工具的 skill** 端到端验证
- **必须先量 token 成本**：57 条描述常驻 18,473 B 是静态估算，需在真实请求上确认实际 `Usage.input_tokens` 增量，并据此回看压缩阈值（与第一刀耦合）
- **兼容缺陷要给策略**（Q6/Q7）：`disable-model-invocation` 过滤、`name`≠目录名（`fireworks-tech-graph-main`）、` ```tool ` 围栏、20 个依赖 Claude 专有工具的 skill 的失效表现
- **建议用 `prototype` 验证**「描述常驻」对实际模型选择行为的影响 —— 静态分析给不出「模型是否真能选中对的 skill」

### 需要先裁决的既有记录冲突

| 冲突 | 出处 | 处置 |
|---|---|---|
| `ys-skill` crate「明确不做」 | `tui-repl-context.md:158,216`（已作废前身） | 显式推翻或限定（→ Q3 一并裁决） |
| 「Auto-compact 状态指示」被否决 | `tui-status-display/context.md:117`（理由「compact 是手动命令」） | **理由已被现实推翻**（自动压缩早已存在），需更新该记录 |
| 「context 占比」字段被否决 | `tui-status-display/context.md:111-117` | 若压缩要做可见性，需先解此否决 |
