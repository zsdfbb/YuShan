# Skills 支持 — 架构上下文

> 产出：`/arch-explore`（2026-09-20 重跑）。用途：为「**支持 skills**」界定现状、约束与范围，供后续 `arch-design` 使用。
> 证据均带 `文件:行号`，对应本工作区 `master`（`981a401`）。
>
> **本次重跑的前提变化**：上一版 context 把 skills 与「自动压缩上下文」合并在
> [`../skills-and-compaction/context.md`](../skills-and-compaction/context.md)（2026-09-19）。**压缩加固已完成并落地**
> （`9c9bf5b` + ADR-0017 + [`../../design-final/compaction.md`](../../design-final/compaction.md)），
> 故本次**只聚焦 skills**，并把压缩过程中沉淀出的**四个既定模式**（§「可复用的既定模式」）当作 skills 设计的现成地基。

---

## 概述

**skills 在本仓是零代码**（`grep -rn skill --include=*.rs` 只有 1 处无关命中），需从零设计。

要复用的对象是 `/Users/zs/Develop/JackCC/claude/skills`（本机经软链 `~/.claude/skills` 暴露，**56 个** skill）。

一句话风险提示：**skill 的 `name` + `description` 必须常驻系统提示（渐进式披露的第一级），实测 56 条 description 共 18,138 B ≈ 4.5k～9k token/请求 —— 这是每轮固定成本**，且**工具面必须恒定**（工具面抖动会废掉 prompt cache）。因此 skills 的形态被两条既有不变量夹住：**列表是启动期静态扫描的数据（不是运行期增删的工具）**，**注入进系统提示的内容会直接吃掉对话预算（与压缩阈值耦合）**。

---

## 已定决策（承接 2026-09-19，skills 相关部分）

> 来源：[`../skills-and-compaction/context.md`](../skills-and-compaction/context.md) 的 D1/D2/D4 及其推论。压缩侧的 D3/D5–D20 已随 ADR-0017 落地，此处不再复述。

| # | 决策 | 含义 |
|---|---|---|
| **D1** | **skills 激活 = 工具 + `/` 命令，两者都要**（同 Claude Code） | ① 恒注册一个 `skill` 工具（模型自主调用，描述常驻系统提示）；② `/skill-name` 命令（用户显式触发）。**两者共享同一份 skill 定义**，不是两套数据 |
| **D1′** | ⚠️ **工具半边已被 design 推翻**（2026-09-20） | 设计阶段发现 pi（主要参照）**零工具**可行，且专用工具在模型自主路线上**净收益为零**（`read` 绝对路径与 `skill(name)` 都是 1 次 round-trip）却要付 10 处连锁点 + 每轮 ~220 token。**改为：正文由既有 `read` 加载，零新工具**；`/skill-name` 复用既有 `Request::Prompt`（不加协议变体）。**D1 的「两条激活路共享同一份定义」保留。** 详见 [`design.md`](./design.md) §选择理由 A + [`../../adr/0019-skills.md`](../../adr/0019-skills.md) |
| **D2** | **发现路径 = `~/.yushan/skills/`（用户级）+ `.yushan/skills/`（项目级）** | **不扫 `~/.claude/skills`**。JackCC 靠用户自行软链进 `~/.yushan/skills`，代码不内置对 Claude Code 目录约定的依赖 |
| **D2′** | **默认路径即最终路径，不引入「路径配置项」**（2026-09-20 复核确认） | 扫描根**固定**为上面两条；**不在 `config.json` 开 `skills.roots` 之类旋钮**（避免无谓的配置链路与解析分支）。实现上扫描根仍以**显式 `&Path` 列表**传入（见模式 ②），但那是**测试注入接口**，**不是**用户面向的配置。⇒ 原 Q 系列中「是否可配路径」的问题就此关闭 |
| **D4** | ~~推进顺序 = 先压缩加固，再做 skills~~ | ✅ **前半已完成**（压缩加固落地）。**现在轮到 skills**，且它是压缩之后唯一被显式推迟的存量需求 |

**D1 的直接推论（含压缩落地后需更新的部分）**：

1. 加 1 个工具 ⇒ 触及**加工具的连锁点**：`toolset::EXPECTED_ORDER`（`toolset.rs:40`，**现为 9 元**）、`build_tools` 的 `vec![...]`、`toolset.rs` 的 3 处 `assert_eq!(tools.len(), 9)`（`226/316/493`）、`prompt.rs` 的 `tool_snippet` + `tool_guidelines` 及其断言（`every_real_toolset_tool_has_non_fallback_snippet` 会强制新工具补 snippet）。**注意**：ADR-0016 当时记的「9 处连锁点」按**当前** 9 工具状态应重新点算，数目可能已变。
2. **56 条 `name` + `description` 常驻系统提示**（实测 18,138 B）⇒ 与压缩阈值耦合：留给对话的预算被压掉 4.5k～9k token。**本轮压缩已把阈值比例化并从 `config.json` 可配**（`Config::compact_policy`），故 skills 的成本**可被显式计入**而非静默吞掉。
3. 工具面**必须恒定** ⇒ skill 列表是**启动期静态扫描的数据**，不支持运行期增删（与「工具面恒定」「prompt cache 前缀稳定」一致）。
4. tool 路线 + `/` 命令路线的**优先级**需在设计中定（同一 skill 既被模型选中又被用户敲 `/` 时的行为）。

---

## 现有架构

### 模块边界（skills 相关的部分）

```
TUI(ys-tui-coding)  ──Request──►  app_loop  ──run_turn──►  Agent(ys-runtime)
      ▲                (信道①)          │                        │
      │                                 │                  BasicLoop(ys-loop)
      └──── Outbound ◄── ChannelSink ◄── AgentEvent ◄────────────┘
                                                              │
                                              session(ys-session) / registry(ys-tool)
```

- 命令的**用户可见行为**在 `crates/ys-tui-coding/src/commands.rs`（表 `COMMANDS`，**现为 11 条**，`33-101`）
- 命令的**能力实现**在 `apps/coding-agent/src/capabilities.rs`（app 侧）
- 二者唯一契约是 `ys_protocol::Request`（`crates/ys-protocol/src/request.rs`，**7 个变体**）；**漏实现是编译错误**（app 侧 `match Request` 穷尽）

### 核心抽象（skills 可能触及）

| 抽象 | 位置 | 与 skills 的关系 |
|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs:26-33` | `spec()` + `async call(input, ToolContext)` —— **skill 若走工具路线落这里** |
| `ToolSpec` | `crates/ys-tool/src/spec.rs:5-10` | `{name, description, parameters}`，**刻意无 prompt 字段** |
| `ToolContext` | `crates/ys-tool/src/context.rs:6-11` | `{boundary, cwd, workspace_root, summarizer}` —— **拿不到 `Model`**；新增的 `summarizer` 是**窄能力**挂载点（见下） |
| `Summarizer`（窄能力 trait） | `crates/ys-tool/src/summarizer.rs:11-17` | 一次性摘要能力，`&str → String`。**这是「工具不碰 `Model`、但按需借用模型能力」的既定范式** |
| `ToolRegistry` | `crates/ys-tool/src/registry.rs` | `HashMap` + **保序 `Vec<ToolSpec>`**（保序为 prompt cache） |
| `Message` / `ContentBlock` | `crates/ys-core/src/message.rs` | `Role::{User,Assistant,System}`；`ContentBlock::{Text,ToolUse,ToolResult}`，**无消息级元数据挂点** |
| `AgentEvent` | `crates/ys-event/src/event.rs` | 已有 `ContextCompacted` / `ContextCompactionFailed`（压缩可见性的先例）—— skills 若要发事件可照此 |
| `Agent` | `crates/ys-runtime/src/agent.rs` | 持 `system_prompt`（**启动期冻结**）；但**新增了 `set_context_window`**（`142`，运行期可改）—— 见下 |
| `CodingView` | `crates/ys-tui-coding/src/view.rs:33-56` | TUI 的显示快照（`Outbound::View` 整体替换，`app.rs:75`）—— **skills 列表要进 `/` 浮层，这里是唯一数据通路** |
| `COMMANDS` / `completion` | `commands.rs:33` / `completion.rs` | `/` 补全与 `/help` **都从这张静态表派生**（`entries()`/`matches()`/`open()` 只读 `all_commands()`）—— **动态如 skills 者，当前接不进去** |

### 关键数据流

**系统提示词构建** —— `apps/coding-agent/src/prompt.rs:13-56`：

```
build_system_prompt(cwd, tools: &[ToolSpec]) -> String
  1. find_custom_prompt(cwd) 命中 .yushan/SYSTEM.md 或 ~/.yushan/SYSTEM.md → 整体短路
  2. 工具清单逐字按 tools 顺序渲染 `- {name}: {snippet}`（snippet 按 name 查表 tool_snippet）
  3. guideline 按「工具是否在场」条件生成（tool_guidelines）
  4. <project_context>：~/.yushan/AGENTS.md + 从 cwd 逐级向上 AGENTS.override.md/AGENTS.md/YUSHAN.md/CLAUDE.md
  5. 末尾 current working directory
```

**装配顺序** —— `apps/coding-agent/src/main.rs`：

```
build_tools(&workspace, web)  →  specs  →  build_system_prompt(&cwd, &specs)
                              →  AgentBuilder::… .system_prompt(...) .build()   ← system_prompt 冻结
```

**每回合刷新** —— `apps/coding-agent/src/app_loop.rs:100-150`：

```
Request::Prompt(message) =>
    agent.set_context_window(config.context_window());   // ← 每回合唯一调用点
    run_turn(...)
```

### 可复用的既定模式（**本轮增量 —— 压缩加固沉淀，skills 应直接沿用**）

压缩加固在落地时确定下来的四个做法，对 skills 一一对应，**skills 设计不必重新发明**：

| 模式 | 压缩侧的实例 | skills 的对应物 |
|---|---|---|
| **① 用户配置文件：`~/.yushan/config.json`（用户写、app 只读）** | `RawUserConfig`（宽容反序列化）→ `UserConfig`（校验后）两级：**一条非法只丢自己**；未知字段忽略；解析失败落日志不 panic 不阻断启动（`config.rs:13-63`） | 若 skills 需要配置（如启停某目录、`disable-model-invocation` 的开关），**照此文件与两级解析**。**绝不写进 `state.json`**（app 会 `save()` 覆盖，`state.rs`） |
| **② 显式 `path` 而非内部拼 `$HOME`** | `Config::load_user_config(&mut self, path: &Path)`（测试直接传临时文件，**不必改写 `HOME`** ⇒ 与 `test_env::env_lock` 的竞争面消失，`config.rs:146`） | skills 的**扫描根目录**也应以**显式 `&Path` 列表**传入（`scan(roots: &[PathBuf])`），而不是内部读 `$HOME`/`$PWD` —— 测试友好且去进程级 env 依赖 |
| **③ 窄能力 trait（不把 `Model` 塞进核心）** | `Summarizer`（`&str → String`，放 `ys-tool`），经 `ToolContext::with_summarizer` 按回合挂载；**依赖不成环**是硬约束 | 若某类 skill 需要模型能力（如 JackCC 里要求 subagent 的），**只能走这种窄 trait**，而不是给 `ToolContext` 加 `Model` |
| **④ 运行期 setter（虽冻结仍是默认）** | `Agent::set_context_window`（`agent.rs:142`，由 ADR-0017 覆盖 ADR-0007 的「新 API 须 ADR」）；**每回合在 app_loop 刷新** | 若决定让 skill 列表运行期可变（当前**不推荐**，见「工具面恒定」），须走同一 ADR 流程；`system_prompt` 仍无 setter |

> 另有一条**可观测性**先例：压缩为「静默丢历史」补了 `ContextCompacted` / `ContextCompactionFailed` 两个 non-terminal 事件。skills 若需在 transcript 显示「已加载 skill X」，可照此，**但更可能的落点是普通 `ToolResult` / `Outbound::Output`**（见 Q13）。

---

## JackCC skill 事实（2026-09-20 重测）

- **它不是 skill 运行时**，只是 skill 定义 + 安装脚本（软链 `~/.claude/skills -> JackCC/claude/skills`）。加载逻辑在 Claude Code 二进制里。
- **规模**：**56 个** skill（上一版记 57，有增减）；56 个均有 `SKILL.md`。
- **frontmatter 字段实测**（`grep '^[a-z-]*:'`）：`name`（56）、`description`（56）、`version`（28，lark 系列）、`metadata`（27，含 `requires.{bins,siblings}` / `cliHelp`）、`disable-model-invocation`（**3**：`domain-model` / `ubiquitous-language` / `zoom-out`）、`license`（1）、`argument-hint`（**1**：`handoff`）。
- **成本**：`name` 合计 1,128 B；**`description` 合计 18,138 B ≈ 4.5k～9k token**。
- **格式**：每 skill 一个目录，必需 `SKILL.md`，可选 `REFERENCE.md` / `references/` / `scripts/` / `assets/`。
- **渐进式披露是核心机制**：**只有 `name` + `description` 常驻系统提示；正文由模型决定后 Read 进上下文**（两级披露）。

**兼容性缺陷（复用 JackCC 必须处理）**：

| 缺陷 | 影响 |
|---|---|
| 正文写死 Claude 专有工具名 | 真堵点是 **`Agent`（subagent，约 16～17 个 skill）**；`WebSearch`/`WebFetch` **名字对得上**（本仓已有 `websearch`/`webfetch`，但 `webfetch` 语义是「落盘+指针」而非「抓取并小结」） |
| 私有 ` ```tool ` 围栏语法 | 非标准 markdown，本仓无解释器 |
| `name` ≠ 目录名 | `fireworks-tech-graph-main` 自称 `fireworks-tech-graph`（用目录名索引会冲突） |
| skill 目录**可写** | `sequential-workflow/learned/`、`plans/` 有 agent 写入的文件 ⇒ 不能假设只读（见 Q14） |
| 无版本契约 | 纯 markdown，上游改动会静默影响行为 |

反过来：**多数 skill 只用基础工具**（`read`/`write`/`edit`/`bash`），本仓有同名工具，可直接复用。

---

## 约束

### 技术

1. **`Agent::system_prompt` 启动期冻结、无 setter**（`agent.rs`；`set_context_window` 是**唯一的运行期 setter 特例**）→ 若 skill 列表要进系统提示，**只能在启动期一次性构建**（与「工具面恒定」一致，多半是好事）。
2. **工具面必须恒定**（`toolset.rs` 文档块；ADR-0016 `:58`）→ skills 若走工具路线，**只能恒注册一个 `skill` 工具，技能列表作为数据**；绝不按登录态/技能数增减工具。
3. **`ToolContext` 拿不到 `Model`**（`context.rs:6-11`）→ 需要模型能力的 skill（subagent 类）走**窄能力 trait**（范式见 `Summarizer`），不给 `ToolContext` 加 `Model`。
4. **`Session` 无重写 API**（`ys-session/src/lib.rs`）→ 与 skills 弱相关，但影响「skill 正文进上下文后又被压缩」的处置（E5）。
5. **依赖方向 / 零 tokio 边界**：`ys-core`/`ys-event`/`ys-protocol` 不含 tokio；`ys-tui-coding` 不依赖 `ys-runtime`（编译器强制）；`ys-component` 不直接用 tokio API。
6. **依赖基线**：ADR-0015 否决 `ignore`/`regex`/`globset`/`walkdir`；ADR-0016 只破例一个 `htmd`。⇒ **解析 YAML frontmatter 需显式决策**（自写极简行解析 / 引 `serde_yaml`（已在 2024-03 归档弃用）/ 换格式）。实测 56 个 frontmatter 结构简单（两种标量 + 少量 `metadata.*` + 折叠 `>`/`>-` 块），**自写解析器可行**。
7. **加工具 / 改工具面的连锁点**（`EXPECTED_ORDER`、`vec!`、`toolset.rs` 3 处 `assert_eq!(len, 9)`、`prompt.rs` 的 snippet + guideline + 断言）—— 见 D1 推论 1，按当前 9 工具状态重新点算。

### 性能

8. **56 条 description 常驻 ≈ 4.5k～9k token/请求**（实测 18,138 B）。对比：工具数 7→9 是「每轮固定 +60~120 token」（ADR-0016）—— **skills 的量级是它的 ~50 倍，且每轮固定**。这是**必须显式接受并纳入压缩阈值**的成本（压缩侧现已可配，见模式 ①）。
9. **prompt cache 前缀稳定性**：skill 列表进系统提示的位置一旦确定，**前缀必须逐字节稳定**（排序确定、内容确定、不含时间戳/随机数）。
10. **token 估算器偏保守**（`ys-loop/src/token.rs`）⇒ skills 注入后的真实增量需在真实请求上验证（用 `Usage.input_tokens`），而非只看静态字节。

### 演进

11. **`docs/arch/coding-agent-tui/tui-repl-context.md` 把 `ys-skill` crate 列为「明确不做」**（该文档是**已作废的行式 REPL 前身**，其否决不自动延续到当前 ratatui 路线）—— **需显式裁决或限定**（见 Q3）。
12. 新模块/接口改动须对应 `docs/design.md` §12 测试矩阵条目。

### 组织

13. 文档/注释/commit **用中文**；代码标识符英文。
14. 测试隔离硬约定：临时目录名含**进程内原子序号 + `process::id()`**；进程级 env 改写走 `test_env::env_lock()` + `EnvRestore`。**优先用模式 ②（显式 path）绕开 env**。
15. **关键逻辑必须做变异测试**（本项目多个真 bug 是「测试全绿时依然存在」）。

---

## 需求范围

### 范围内

- **B1 发现**：递归扫 `SKILL.md`（根 = `~/.yushan/skills/` + `.yushan/skills/`），解析 frontmatter（`name` + `description`，缺失回退目录名）
- **B2 渐进式披露**：`name` + `description` 常驻系统提示；正文按需加载
- **B3 激活（两条路都要）**：① 恒注册 `skill` 工具（模型自主）；② `/skill-name` 命令（用户显式）—— **共享同一份定义**
- **B4 复用 JackCC**：靠用户自行软链进 `~/.yushan/skills`（D2），代码不内置 `~/.claude` 路径
- **B5 处理 `disable-model-invocation`**（不进模型候选，仍可显式 `/` 调用）—— 实测 3 个
- **B6 兼容缺陷的最小处置**：`name`≠目录名、` ```tool ` 围栏、Claude 专有工具名的静默失效
- **B7 `/` 浮层与 `/help` 能列出 skill**（S6）—— 需把 skill 列表接进 `CodingView`（当前补全只读静态 `COMMANDS`）

### 范围外（明确不做）

- ❌ **Hook 系统**（ADR-0014，有意推迟）
- ❌ **Subagent / Agent 工具**（JackCC 约 16 个 skill 依赖它；本仓无此能力）
- ❌ **Plan 模式 / TodoWrite / TaskCreate**（少数 skill 依赖）
- ❌ skill 的**热插拔 / 市场 / 运行期增删**（与「工具面恒定」冲突；**启动期静态扫描**）
- ❌ skill 的**权限/沙箱**（本项目「不做安全」）
- ❌ skill 里 `scripts/` 的**自动执行**（若做，须走现有 `bash` 工具，不自建执行器）

### 关键场景

- **S4（skill 自主激活）**：用户说「检查 git 状态」→ 模型从描述里选中 `git-status` → 正文进上下文 → 按 SKILL.md 执行
- **S5（skill 显式激活）**：用户敲 `/git-status` → 正文进上下文（不等模型判断）
- **S6（`/` 发现）**：补全浮层列出 skill（源自同一张表）
- **S7（`disable-model-invocation`）**：`/zoom-out` 可用，但**不出现**在模型的候选描述列表里（省 ~1KB 常驻）

### 边界/异常场景

- **E1**：skills 目录不存在 / 权限不足 / 空目录 → 不报错，静默零 skill
- **E2**：`SKILL.md` frontmatter 缺失 / 畸形 → 跳过该 skill 或回退目录名（**需定策略**）
- **E3**：同名 skill 出现在两个扫描路径 → 去重策略（**就近优先**：项目级 > 用户级）
- **E4**：单个 skill 正文极大（`fireworks-tech-graph-main` 32 KB）→ 进上下文后可能触发压缩
- **E5**：**压缩把注入的 skill 正文摘要掉** → 长任务中 skill 指令丢失（与已落地的压缩交互）
- **E6**：`name` 含字符集/大小写（如 `MiniMax` 类）与 `/` 命令名派生规则需定义

### 质量目标

- **复用率**（多数 skill 可直接跑）优先，其次是启动开销
- 不破坏既有不变量：**工具面恒定 / prompt 前缀稳定 / 依赖方向**
- **token 成本可观测**（真实 `Usage` 增量，而非仅静态估算）

---

## 未澄清问题

**已在承接决策中解决**：~~Q1 激活机制~~ → D1；~~Q2 发现路径~~ → D2；~~推进顺序~~ → D4（压缩已完成）。

**设计前仍须回答**

- [ ] **Q3 是否需要新 crate？落在哪一层？** 前身文档把 `ys-skill` 列为「不做」（已作废前身，需显式裁决）。**倾向 app 层**（`apps/coding-agent/src/skills.rs`）——skill 是**产品概念**，不是核心能力。**但**：若 `/` 浮层要列出 skill，数据须经 `CodingView`（`ys-tui-coding` 不依赖 `ys-runtime`，也不认识 app 的类型）**流到 TUI** ⇒ 需定「skill 列表在 `CodingView` 里长什么样」（一个新字段 `skills: Vec<SkillEntry>`？），以及补全 `matches()`/`open()` 如何同时读静态 `COMMANDS` 与动态 `view.skills`。
- [ ] **Q4 YAML frontmatter 怎么解析？** 自写极简行解析（守住不引依赖基线）／引 `serde_yaml`（已被上游归档，需 ADR）／改格式？实测结构简单（标量 + `metadata.*` + `>`/`>-` 折叠块），**自写解析器可行**，但须覆盖 4 种 description 写法。
- [ ] **Q6 `disable-model-invocation` 的 3 个 skill 如何表现？** 不进模型候选列表（省常驻），但 `/zoom-out` 可调用。是否也支持 `config.json` 覆盖该开关？
- [ ] **Q7 正文里的 Claude 专有工具名**（`Agent` / `WebFetch` / `EnterPlanMode` / `TaskCreate` / `WebSearch`）要不要**符号映射或降级提示**，还是原样注入、失效自负？（影响约 20 个 skill。）
- [ ] **Q11 skill 列表注入系统提示的精确位置**（工具清单后？`<project_context>` 前？）—— 直接影响 prompt cache 前缀稳定性。
- [ ] **Q13 模型自主调用 skill 时，正文进上下文的形态**：作为 `ToolResult` 内容？还是后续追加一条独立消息？前者最省事但正文会带 `ToolResult` 语义。
- [ ] **Q14 skill 正文是否每次调用重新读盘**（目录可写，JackCC 的 `learned/`），还是启动期缓存？（E5 也与此相关。）
- [ ] **Q17（新）`/skill-name` 是否与内置 11 条命令在同一命名空间？** 冲突时谁优先（如某 skill 名为 `status`）？skill 名如何派生 `/` 命令名（`name` vs 目录名，见 B6）。
- [ ] **Q18（新）「展开成消息」的路线选择**：`tui-repl-context.md:266-296` 曾把「展开型命令」（`/skill` / prompt template）归为「产物是文本，走与纯文本输入完全相同的路，agent 不知道它来自命令」。**这条是否沿用**？（若沿用，`/skill-name` 不需要新 `Request` 变体；若走工具路线则相反。两条路线的**统一或分叉**需定。）

---

## 后续建议

→ 进入 `arch-design`，产出一份独立设计方案（**不并入压缩**，压缩已完成）。

**先答 Q3 / Q4 / Q18** —— 它们决定**数据通路**（skill 列表如何到 TUI）、**依赖基线**（是否引 YAML）与**命令模型**（展开成消息 vs 新 `Request` 变体），其余问题都挂在它们下面。

**最小垂直切片优先**：扫描（D2 两条路径）→ 解析 frontmatter → 注入系统提示 → 1 个 `skill` 工具 + `/skill-name` 命令 → 拿 `git-status` 之类**只用基础工具**的 skill 端到端验证。

**必须复用的既成地基**（§「可复用的既定模式」）：
- 扫描/配置走**显式 `&Path` roots**（模式 ②），绕开 `HOME` 改写与 env 竞争；
- 若要配置，挂 **`~/.yushan/config.json`** 的两级宽容解析（模式 ①）；
- 需要模型能力的 skill 走**窄能力 trait**（模式 ③）；
- 若要事件，照 `ContextCompacted` 的 non-terminal 形状（可观测性先例）。

**三个必须先量/先验的点**：
1. **量 token 成本**：56 条描述常驻的真实 `Usage.input_tokens` 增量，并据此回看压缩阈值（与已落地的 `CompactPolicy` 耦合）。
2. **验模型选择行为**：静态分析给不出「模型是否真能选中对的 skill」—— 建议用 `prototype` 验证「描述常驻」对实际选择的影响。
3. **点算连锁点**：加第 10 个工具在当前 `toolset.rs`（9 工具、3 处 `len()==9` 断言）下影响的**确切处数**，避免照抄 ADR-0016 的旧「9 处」。

**需要显式裁决的既有记录冲突**：

| 冲突 | 出处 | 处置 |
|---|---|---|
| `ys-skill` crate「明确不做」 | `tui-repl-context.md`（已作废前身） | 显式推翻或限定（→ Q3 一并裁决） |
| 「展开型命令」路线（产物=文本，agent 不知来自命令） | `tui-repl-context.md:266-296`（已作废前身） | 显式裁决是否沿用到 ratatui 路线（→ Q18） |
