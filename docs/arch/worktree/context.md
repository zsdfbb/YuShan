# git worktree 支持 — 架构上下文

> 本轮范围已由用户逐条裁定（见「需求范围」）。**一句话**：让「一个需求」有自己的 git
> worktree，经 **`/worktree` 斜杠命令（用户）** 或 **`worktree` 工具（模型，由 skill 正文
> 指示调用）** 创建 **并把当前会话切进去**；需求内多 agent 的写冲突靠 **skill 协商**、
> 运行时不做机制兜底；**只隔离不回流**；无法建 worktree 时**显式降级**为共享工作区。

## 概述

让 coding-agent 能以 **git worktree** 作为「一个需求」的工作区单位：需求开始时给出一个
独立工作树，该需求下的 agent（父 + 子 agent）都在这个工作树里读写；需求之间、需求与主树
之间互不踩踏。触发有**两条口**：用户敲 `/worktree`，或 skill 正文指示模型调用 `worktree`
工具 —— 两者共用同一份「创建 worktree + 切换当前会话工作区」的实现。

**worktree 只做隔离，不解决同一工作树内多 agent 的写冲突** —— 后者下放给一条 skill
（提示词层的协商协议），运行时不做机制兜底。

## 现有架构

### 模块边界

- **workspace 是进程级、启动期冻结的量**。`Config::from_env` 取 `std::env::current_dir()`
  （`apps/coding-agent/src/config.rs:176`），`main` 里 `let workspace = cwd.clone()`
  （`apps/coding-agent/src/main.rs:1341-1342`），此后**再不改动**。
- **工具装配的唯一接缝**是 `toolset::build_tools(workspace, web)`
  （`apps/coding-agent/src/toolset.rs:88`）。它把 `workspace` 的 clone 交给每个工具
  （`ReadTool::new(ws)` / `BashTool::new(ws)` …）。
- **子 agent 复用父的 workspace**：`AppSubagentRunner::run` 里
  `build_tools(&self.workspace, …)` + `working_dir(self.workspace, self.workspace)`
  （`apps/coding-agent/src/subagent.rs:349-353`），`self.workspace` 来自装配期
  `SubagentSetup::new(…, workspace.clone(), …)`（`main.rs:1362`）。
- **命令系统**（以代码为准）：`ys-tui-coding/src/commands.rs` 一张静态表 `COMMANDS`
  + `parse()` → `Action::{Local(LocalAction), Request(Request), Prompt(PromptKind), Abort, Quit}`。
  **纯用户输入驱动**；有副作用的命令走 `Request` → app 线程
  （`/new` → `capabilities::new_session`、`/model` → `set_model`）。
  ⚠️ `docs/arch/commands/adr-command-architecture.md` 写「选 Command trait + Registry」是**过期稿**；
  `capabilities.rs:3` 明说「没有 `Command` trait、没有 registry」，实际为 `match` 分发。
- **skill 不是可执行物**：`parse_with_skills` 命中 skill 名 ⇒「原文、零展开」塞进上下文
  （`crates/ys-tui-coding/src/commands.rs` 注释；`skills::expand_invocation`）。⇒ **skill 只能
  经模型调用工具这条路生效**，不能直接发命令。
- **没有任何 `git` 调用**（全仓 grep 无 `Command::new("git")`）。
- **session 与 workspace 无绑定**：会话按 id 落在 `YUSHAN_SESSIONS_DIR`（`sessions.rs` / `wiring.rs`），
  与当前工作区无关。

### 核心抽象

- `ToolContext { boundary, cwd, workspace_root, summarizer, subagent }`
  （`crates/ys-tool/src/context.rs:8-25`）—— 工具**不读 `cwd`/`workspace_root`**，路径解析一律用
  **构造期捕获**的 `self.workspace`（`read.rs:48`、`write.rs:41`、`edit.rs:166`；`bash.rs:118`
  用 `.current_dir(&self.workspace)`）。
  - `summarizer` / `subagent` 是**同形挂载点**的先例（`with_summarizer` / `with_subagent`，
    `context.rs:19-24`）。**`worktree` 工具若要在调用期上卷「切换工作区」，最自然的落法是第 4 个
    同形挂载点**（`Option<&dyn WorktreeRunner>` 之类），照抄这条先例。
- `AgentBuilder::working_dir(cwd, root)` —— 只填 `ToolContext` 两个字段，**不改工具已捕获的 workspace**。
- `prompt::build_system_prompt(cwd, tools, catalogue)`（`prompt.rs:28`）—— **cwd 直接进提示词**
  （`Current working directory: {cwd}`，:69）、从 cwd 逐级向上加载项目上下文（:277）、从
  `cwd/.yushan/SYSTEM.md` 取自定义 prompt（:323）。项目级 skills 根也是
  `[cwd/.yushan/skills, home/.yushan/skills]`（`Config::default_skill_roots`）。

### 关键数据流（目标形态）

```
两条触发口 ─┬─ 用户: /worktree <args>  (commands.rs parse → Request)  ─┐
            └─ 模型: worktree 工具       (ToolContext 新挂载点)         ─┤
                                                                       ▼
                          共享实现（apps/coding-agent，允许 tokio）
                          ├─ git worktree add <path> <branch>
                          ├─ 校验结果 / 失败 ⇒ 显式降级（返回共享工作区 + 警示）
                          └─ 重绑 workspace（下一回合生效）
                                 ├─ toolset::build_tools(new_ws)
                                 ├─ prompt::build_system_prompt(new_cwd)
                                 └─ AgentBuilder / Agent 重建
```

今天（未实现）的四路扇出：`build_tools` / `working_dir` / `build_system_prompt` / `SubagentSetup.workspace`
**全都取同一个进程级 `workspace`**。把它改成「每需求一份」⇒ 这四路都要能按需求取值。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|------|------|------|------|
| git CLI | `git worktree add/remove/list` | 用户机器上的系统 git | 全仓**尚无**任何 git 调用；**不新增 Rust crate 依赖** |
| 现有 `bash` 工具 | 备选执行通道 | — | 若生命周期代码直接 `tokio::process::Command::new("git")`，则不复用 bash |

## 约束

- **技术**
  - **依赖方向单向**：`ys-core` / `ys-event` / `ys-protocol` **零 tokio** ⇒ worktree 生命周期
    （跑 git 子进程）只能落在允许 tokio 的层：`apps/coding-agent` 或 `ys-session`/`ys-loop`/`ys-runtime`。
  - **工具构造期捕获 workspace 且不读 `ToolContext.cwd`** ⇒ 换工作区**必须重建工具表**（改
    `working_dir` 无效）。
  - **工具无法重建 app 级的 agent / 工具表 / 系统提示词**：`worktree` 工具的「切换」不能由工具
    自己完成 ⇒ 必须经 `ToolContext` 挂载点上卷给 app（4th 同形挂载点，先例 `summarizer`/`subagent`），
    或把「创建」与「切换」拆开（工具只创建、app 侧切换）。
  - **agent / 工具表 / 系统提示词在构造期冻结** ⇒ **切换的生效边界 = 下一回合**。这与 `/new`
    换 Wiring、`/model` 换模型**同层、同边界**（都在回合之间改装配）。本回合内工具仍按旧工作区跑；
    结果里须如实告知模型「自下一回合起你的工作区是 X」。
  - **命令归 UI、能力归 app**（`commands.rs` 模块头）：`/worktree` 的解析与文案在 `ys-tui-coding`，
    git/重绑的**实现**在 app 线程；`capabilities.rs` **绝不 print**，一切输出走 `Outbound::Output`。
  - **子表不含本工具**（父表 = 基础 9 元 + `subagent` 追加末位，`main.rs:1370-1373`）⇒ 照此模式，
    `worktree` 也追加末位则**子 agent 结构上拿不到它**，无法自行切工作区。
  - **不做安全**（`CLAUDE.md` 硬约束）：worktree 是**隔离手段**，不是权限/沙箱边界；不承担
    「禁止 agent 越出工作树」。
- **性能**
  - **prompt cache 是一等要求**：cwd 进 system prompt ⇒ **每个 worktree 一份前缀**，前缀缓存只在
    **同一 worktree 内**命中；**每次切换 = 新前缀首轮 cache miss**。这也从性能侧支持「一需求一
    worktree」而非「一子 agent 一 worktree」。
  - 切工作区还改**项目上下文文件**与**项目级 skills** 的加载结果 ⇒ 提示词长度随之变（进
    `RunLimits::fixed_prompt_tokens`，与压缩阈值耦合，ADR-0017）。
- **演进**
  - **ADR-0022 / ADR-0029 已记为「已知缺口」**：并行子 agent 写冲突无人管，因「本仓无
    branch/worktree」只能靠 guideline（`docs/adr/0022-subagent-dispatch.md:135,293`、
    `docs/design-final/coding-agent-tui.md:629`）。本需求**部分**回应：隔离单位提升到「需求」，
    **同一需求内仍是 guideline 协商**（用户明确「我们不直接解决这个问题」）。
  - **无 workspace 参数的口**：今天只能 `cd <worktree>` 再启动。是否新增 CLI flag 未定（见未澄清）。
  - **`/new` 换 Wiring、`--resume` 回放**既有机制（`docs/arch/runtime-resume/`）⇒ 切换 worktree 与
    换会话是否联动需定关系。
- **组织**
  - 单人维护；**不设计假想未来需求**（ADR-0014 / `docs/extension-points.md`）。
  - **归类判据**：用户口径「命令 + 工具都要」⇒ 本质是**同一能力的两个入口**
    （命令 = 应用装配；工具 = 给模型的能力，落 Tool 桶）。

## 需求范围

### 范围内

- **一个需求 ⇒ 一个 git worktree**。
- **两条触发口，一份实现**：
  - **`/worktree` 斜杠命令**（用户敲；解析在 `ys-tui-coding`，执行在 app）。
  - **`worktree` 工具**（模型调用；skill 正文可指示模型调用它）。
- **创建 + 切进当前会话**：既 `git worktree add` 建工作树，也把当前会话的 workspace 重绑过去
  （重建工具表 + 系统提示词 + agent），**下一回合生效**。
- **需求内多 agent 共用同一 worktree**：父 agent 与其子 agent 共享该需求的工作树（延续今天的继承行为）。
- **同工作树内的写冲突由 skill 协商**：交付一条提示词层协议（skill），**运行时不做冲突检测/锁/合并**。
- **只隔离不回流**：worktree 内改动**不自动**合并回主树；保留分支供用户后续手动 merge / 对比。
- **显式降级**：非 git 仓库 / git 不可用 / `worktree add` 失败 ⇒ **退回共享工作区（当前行为）**，
  同时给出**明确可见的警示**（命令 → transcript；工具 → `ToolResult`；启动降级 → stderr），**不得静默**。

### 范围外（明确不做的）

- ❌ **一子 agent 一 worktree**（用户选定「一需求一 worktree」；且会打碎 prompt cache）。
- ❌ **运行时层面的写冲突消解**：不做锁、不做写集不相交检测、不做自动合并。
- ❌ **自动合并 / cherry-pick 回主树**（用户选「只隔离不回流」）。
- ❌ **安全边界**：worktree 不是沙箱；不拦「agent 越界写到主树或其他目录」（本仓不做安全）。
- ❌ **Windows 支持**（`platform-scope`：只 Linux/macOS，无 win 路径分支）。

### 关键场景

- **场景 1a（用户路径）**：在 git 仓库里，用户敲 `/worktree <name>` → 建工作树、当前会话切进去
  → 后续回合的读写都落在该工作树；该分支保留，用户自行 merge。
- **场景 1b（模型路径）**：装了含「开工前先建工作树」的 skill → 模型调 `worktree` 工具 → 同上。
- **场景 2（需求内并行）**：同一需求内父 agent 派发多个子 agent，全部落在**同一** worktree；
  写目标重叠时靠 skill 协商，而非运行时。
- **场景 3（降级）**：非 git 目录 / 没装 git / `worktree add` 失败 → 不失败退出，退回共享工作区，
  **显式**告知（含模型侧：工具结果里说明「未隔离」）。
- **场景 4（需求间隔离）**：两个需求各自一个 worktree，互不可见对方的未提交改动与主树。

## 未澄清问题

- [ ] **命令/工具的参数与命名**：`/worktree` 带什么参（新分支名 / 路径 / 基于哪个 ref）？名字默认
      从哪派生（需求标题 / 会话 id / 序号）？（待用户确认）
- [ ] **「切回主树 / 退出 worktree」的语义与入口**：只隔离不回流下，退出 = 保留分支仅切回主树？
      入口是 `/worktree` 无参、还是 `/worktree exit`？（待用户确认）
- [ ] **切换后会话文件是否迁移**：session 与 workspace 今天**无绑定**；切换只改工作区、会话文件
      留在原处，还是要把当前会话「搬」到 worktree？（待用户确认）
- [ ] **同一会话内多次切换**是否允许？已存在同名 worktree / 分支时是复用、报错还是换名？（待用户确认）
- [ ] **工具调用是否需要确认**：创建目录 + 切工作区是侵入性副作用；本仓默认 `AutoApprove`
      （`ys-tool/src/approval.rs`），工具这条路是否要挂审批？（待用户确认）
- [ ] **worktree 落点**：仓库内 `.worktrees/<name>`（需 `.gitignore`）还是仓库外
      `~/.yushan/worktrees/…`？**直接影响项目级 `.yushan/skills` 与 `.yushan/SYSTEM.md` 在 worktree 内是否可见**
      （见约束·技术）。（待用户确认）
- [ ] **需求 ↔ 会话 ↔ worktree 是否一一对应**？`/new`、`--resume` 时 worktree 跟着切/恢复吗？（待用户确认）
- [ ] **worktree 目录的回收时机**：需求结束时 `git worktree remove`，还是永久保留？（分支因「不回流」必留）（待用户确认）
- [ ] **是否新增 CLI 口**（如 `--workspace`/`--worktree`），还是只做命令 + 工具？（待用户确认）
- [ ] **skill 协商协议的边界**：只给「写目标不相交」的静态纪律，还是含「发现冲突如何让步」的动态协议？（待用户确认）
- [ ] **复用既有 `bash` 工具执行 git，还是新增生命周期代码直接 `Command::new("git")`**？（影响可测试性与降级点）

## 后续建议

- 建议用 `arch-design` 做方案设计。核心张力有两个：
  1. **「切换工作区」的上卷机制**：`worktree` 工具无法重建 app 级装配 ⇒ 用 `ToolContext` 新挂载点
     （4th 同形）还是「工具只创建、切换走命令」？两条口如何共用一份实现？
  2. **workspace 参数化的落点**：改 `apps/coding-agent` 装配层 vs 下沉到 `ys-session`/`ys-runtime`。
- 建议用 `prototype` 验证两个假设：
  1. 换 workspace 后 `build_system_prompt` 的项目上下文 / 项目级 skills 加载在 worktree 内的实际差异
     （尤其 `.yushan/` 未随 `git worktree` 迁入的情况）。
  2. 「下一回合生效」的边界在 TUI 与 `-p` / `--json` 三模式下的一致行为。
- 建议对照既有 as-built：`docs/design-final/coding-agent-tui.md` §13（subagent 派发与 workspace 继承）、
  `docs/arch/subagent-rebaseline/context.md`（约束一节）、`docs/arch/commands/`（命令体系现状，
  注意其 ADR 为过期稿，以 `crates/ys-tui-coding/src/commands.rs` 为准）。
