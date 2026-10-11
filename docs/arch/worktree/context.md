# git worktree 支持 — 架构上下文

> 本轮范围经用户三轮裁定收敛（见「需求范围」）。**一句话**：给 coding-agent 一套
> `ys-coding-agent worktree {create,attach,list,remove}` CLI，把「一个需求」安置在自己的
> git worktree 里；`attach` = **启动即绑定**（无中途切换）。需求内多 agent 的写冲突靠
> **skill 协商**，运行时不做机制兜底；**只隔离不回流**；非 git / git 不可用 ⇒ **显式报错并指出退路**。
>
> 设计与决策见 [`design.md`](./design.md) 与 [`docs/adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md)。

## 概述

让 coding-agent 能以 **git worktree** 作为「一个需求」的工作区单位：需求开始时给出一个
独立工作树，该需求下的 agent（父 + 子 agent）都在这个工作树里读写；需求之间、需求与主树
之间互不踩踏。**触发只有 CLI 一条路**（`create` 建树 / `attach` 启动即绑定 / `list` / `remove`），
**不做工具、不做斜杠命令、不做中途切换**。

**worktree 只做隔离，不解决同一工作树内多 agent 的写冲突** —— 后者下放给一条 skill
（提示词层的协商协议），运行时不做机制兜底。

## 现有架构

### 模块边界

- **workspace 是进程级、启动期冻结的量**。`Config::from_env` 取 `std::env::current_dir()`
  （`apps/coding-agent/src/config.rs:176`），`main` 里
  `let cwd = config.cwd.clone(); let workspace = cwd.clone();`（`main.rs:1341-1342`），此后**再不改动**。
- **工具装配的唯一接缝**是 `toolset::build_tools(workspace, web)`（`apps/coding-agent/src/toolset.rs:88`），
  把 `workspace` 的 clone 交给每个工具。
- **子 agent 复用父的 workspace**：`AppSubagentRunner::run` 里
  `build_tools(&self.workspace, …)` + `working_dir(self.workspace, self.workspace)`
  （`subagent.rs:349-353`），`self.workspace` 来自装配期 `SubagentSetup::new(…, workspace.clone(), …)`
  （`main.rs:1362`）。
- **CLI 参数解析是手写的** `parse_args`（`main.rs:161-222`），match **首 token**；
  ⚠️ 今天的 `_ =>` 兜底会**静默忽略**非 flag 首 token（`ys-coding-agent worktree` 现在会直接进交互模式）。
- **命令系统**（以代码为准，不是旧 ADR）：`ys-tui-coding/src/commands.rs` 静态表 `COMMANDS` + `parse()`
  → `Action::{Local, Request, Prompt, …}`，**纯用户输入驱动**；有副作用的命令走 `Request` → app 线程。
  ⚠️ `docs/arch/commands/adr-command-architecture.md` 写「选 Command trait + Registry」是**过期稿**
  （`capabilities.rs:3` 明说「没有 `Command` trait、没有 registry」）。
- **skill 不是可执行物**：`parse_with_skills` 命中 skill 名 ⇒「原文零展开」塞进上下文给模型
  （`skills::expand_invocation`）。
- **没有任何 `git` 调用**（全仓 grep 无 `Command::new("git")`）。
- **session 与 workspace 无绑定**：会话按 id 落在 `YUSHAN_SESSIONS_DIR`（`sessions.rs` / `wiring.rs`）。

### 核心抽象

- `ToolContext { boundary, cwd, workspace_root, summarizer, subagent }`
  （`crates/ys-tool/src/context.rs:8-25`）—— 工具**不读 `cwd`/`workspace_root`**，路径解析一律用
  **构造期捕获**的 `self.workspace`（`tools-basic/src/read.rs:48`、`write.rs:41`、`edit.rs:166`；
  `bash.rs:118` 用 `.current_dir(&self.workspace)`）⇒ **工作区必须在建工具之前定死**。
- `Agent` 冻结持有 `registry`/`cwd`/`workspace_root`/`system_prompt`
  （`crates/ys-runtime/src/agent.rs:67-75`），`run_turn` 每回合 clone 进新的 `RuntimeContext`
  （:114-124）；`RuntimeContext.registry` 是**不可变借用** `&'a ToolRegistry`（`ys-component/src/context.rs:13`），
  且 `ToolRegistry` **不实现 `Clone`**（内含 `Box<dyn Tool>`，`ys-tool/src/registry.rs:5-8`）
  ⇒ **中途换工具表要改核心类型**（这正是本设计回避中途切换的技术原因）。
- `prompt::build_system_prompt(cwd, tools, catalogue)`（`prompt.rs:28`）—— **cwd 与路径都进提示词**：
  `Current working directory: {cwd}`（:69）、`<project_instructions path="{path}">`（:60-64）、
  从 cwd 逐级向上加载项目上下文（:277）、从 `cwd/.yushan/SYSTEM.md` 取自定义 prompt（:323）。
  项目级 skills 根是 `[cwd/.yushan/skills, home/.yushan/skills]`（`Config::default_skill_roots`）。

### 关键数据流（目标形态）

```
ys-coding-agent worktree create <name>   ── git worktree add ──► $HOME/.yushan/worktrees/<仓库的 HOME 相对路径，逐分量作目录>/<name>
ys-coding-agent worktree attach <name>   ── main: config.cwd = worktree::attach_target(...)
                                            └─► 四路扇出（build_tools / working_dir /
                                                build_system_prompt / SubagentSetup）**原样不动**
```

要点：**workspace 仍是单点量、仍只在启动期定死** —— 本设计只改「它取什么值」，不改任何扇出。
⚠️ 但取值来源**不是** `main.rs:1341-1342` 的 `workspace` 一行：`cwd` 同时喂
「提示词」（`build_system_prompt(&cwd)`，:1352/:1375）与「skill 根 / 自定义 prompt」
（`default_skill_roots(&config.cwd)`，:1227）。⇒ 注入点是**单一量 `config.cwd`**（见 `design.md` §1.1）：
项目级 `.yushan/`（skill 根 / `.yushan/SYSTEM.md`）**不随 `git worktree` 迁入** ⇒ 树内不可见；
`attach` 成功后若主仓有 `.yushan/` 则打缺失警告（§5.3-d′）。**不建软链、不引入第二量**；
非 worktree 路径下 `cwd` 原样。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|------|------|------|------|
| git CLI | `worktree add/list/remove`、`rev-parse --git-common-dir` | 用户机器上的系统 git | 全仓**尚无**任何 git 调用；**不新增 Rust crate 依赖** |

## 约束

- **技术**
  - **依赖方向单向**：`ys-core` / `ys-event` / `ys-protocol` **零 tokio** ⇒ git 子进程只能落在 apps 层。
  - **工具构造期捕获 workspace 且不读 `ToolContext.cwd`** ⇒ 工作区必须在**建工具之前**定死
    （启动期解析恰好满足；**这是「不引入中途切换」的技术根因**）。
  - **中途切换要改核心类型**（`RuntimeContext.registry` 可变化）⇒ 本设计不做。
  - **`parse_args` 手写、match 首 token** ⇒ 加 `worktree` 子命令是纯增量臂，但须留意 `_ =>` 的静默兜底。
  - **不做安全**（`CLAUDE.md` 硬约束）：worktree 是**隔离手段**，不是权限/沙箱边界。
- **性能**
  - **prompt cache**：cwd 与 `<project_instructions path=…>` 都进提示词文本 ⇒ **每个 worktree 一份前缀**，
    首次 `attach` 首轮 cache miss。
  - 切工作区还改**项目上下文文件**与**项目级 skills** 的加载结果 ⇒ 提示词长度随之变
    （进 `RunLimits::fixed_prompt_tokens`，与压缩阈值耦合，ADR-0017）。
- **演进**
  - **ADR-0022 / ADR-0029 已记为「已知缺口」**：并行 subagent 写冲突无人管。本需求**部分**回应：
    隔离单位提升到「需求」，**同一需求内仍是 skill 协商**。
  - **`--resume` 与 worktree 的关系未定**：会话不记录工作区 ⇒ 裸 `--resume` 会落回主树（P2 缓解项）。
- **组织**
  - 单人维护；**不设计假想未来需求**（ADR-0014 / `docs/extension-points.md`）。
  - **归类判据**：用户裁定「东西存在 ys 目录下、别让用户找 git」⇒ **进程级副作用 + 管理表面** ⇒ 落 **CLI**，
    不是 Tool（模型决定何时做）、不是 Hook（无流程控制需求）。

## 需求范围

### 范围内

- **一个需求 ⇒ 一个 git worktree**。
- **CLI 子命令组**：`ys-coding-agent worktree {create, attach, list, remove}`。
  - `create` / `list` / `remove` = `git worktree {add, list, remove}` 的薄包装（`remove` 天然留分支、脏则拒删）；
  - **`attach` = 启动（或 resume）一个会话，工作区绑定到该树** —— 唯一的 ys 特有动词（git 无对应物）。
- **`create` 可不给名字**（用户追加）：无参 ⇒ 自动命名（复用 `new_session_id` 形态）；`attach` 无参 ⇒ 列表选取。
  避免起名麻烦，且名字不必好记。
- **两处打印**（用户追加）：`create` 成功打 `worktree: <name>` 到 stderr（同 ADR-0032 的 `session: <id>`）；
  会话结束在既有退出行处追加同名行 + 「分支未合并」提示。**不需要新句柄**（工作区运行期不变）。
- **`attach` = 启动即绑定** ⇒ 无中途切换 ⇒ **零核心 crate 改动**（注入点在 `Config::from_env()`
  之后、skill 扫描之前，**改写单一量 `config.cwd`**：一个 `worktree.rs` + `main.rs` 一处，`config.rs` / `prompt.rs` 不动）。
- **落点 `$HOME/.yushan/worktrees/<仓库的 HOME 相对路径，逐分量作目录>/<name>`**（仓库在 HOME 外 ⇒ 用绝对路径的各分量，逐分量嵌套）；
  `create` 只做 `git worktree add`，**不建软链** ⇒ 项目级 skill / `.yushan/SYSTEM.md` 在树内**不可见**；
  `attach` 成功后若主仓有 `.yushan/` 则向 stderr 打缺失警告。
- **需求内多 agent 共用同一 worktree**（父 + 子 agent，延续今天的继承行为）。
- **同工作树内的写冲突由 skill 协商**：提示词层协议，**运行时不做冲突检测/锁/合并**。
- **只隔离不回流**：不自动合并回主树；分支保留（`git worktree remove` 默认留分支，与策略天然一致）。
- **显式降级**：`attach` 遇非 git / git 不可用 / 树不存在 ⇒ **响亮警告 + 以共享工作区继续启动**；
  `create` / `remove` 是显式动作 ⇒ **硬失败**（非零退出 + 原因 + 退路）。

### 范围外（明确不做的）

- ❌ **`worktree` 工具 / 斜杠命令 / 任何中途切换**（用户裁定 `attach` = 启动即绑定；且中途切换要么改核心、
    要么只能承诺「下回合生效」）。
- ❌ **`ToolContext` 第 4 挂载点**（无中途切换需求后，连同其成本一起消失）。
- ❌ **一子 agent 一 worktree**（会打碎 prompt cache）。
- ❌ **运行时层面的写冲突消解**：不做锁、不做写集不相交检测、不做自动合并。
- ❌ **自动合并 / cherry-pick 回主树**；**自动回收 / TTL / ledger**（只隔离不回流 ⇒ 自动删会毁未 merge 的工作）。
- ❌ **安全边界**：worktree 不是沙箱（本仓不做安全）。
- ❌ **Windows 支持**（`platform-scope`：只 Linux/macOS，无 win 路径分支）。

### 关键场景

- **场景 1（主路径）**：git 仓里 `worktree create feat && worktree attach feat` → 会话在新树里跑；
  分支保留，用户自行 merge。
- **场景 2（需求内并行）**：同一需求内父 agent 派发多个子 agent，全部落在**同一** worktree；
  写目标重叠时靠 skill 协商，而非运行时。
- **场景 3（降级）**：非 git 目录 / 没装 git / attach 一个不存在的树 → **明确报错** + 退路提示，
  用户可照常（共享工作区）启动。
- **场景 4（需求间隔离）**：两个需求各自 `attach` 到不同树，互不可见对方的未提交改动与主树。
- **场景 5（会话外管理）**：用户在 shell 里 `worktree list` / `worktree remove`，或从 CI 脚本调用；
  模型也可经 `bash` 工具调用同一套命令。

## 未澄清问题

- [x] ~~`attach` 时树不存在~~ ⇒ 已定：响亮警告 + 回退共享工作区启动（`design.md` §5.3-b）；
      是否再提供 `--create-if-missing` 仍留 P3。
- [ ] **`SessionMeta.workspace`（只读记录）是否本轮就做**？不做则 `--resume` 会静默落回主树（P2 缓解项）。
- [x] ~~同名仓库撞名~~ ⇒ 落点用**仓库的 HOME 相对路径**（逐分量作目录，`design.md` §5.3-d），「不同路径、同名目录」不再撞。
- [ ] **`attach` 与 `--resume` 的组合语义**：`attach` 只定工作区、`--resume` 只定会话，两者独立可叠加？
- [ ] **降级措辞与去处**：CLI 走 stdout/stderr（与 `-p`/`--json` 同侧），具体文案待定。
- [ ] **skill 协商协议的边界**：只给「写目标不相交」的静态纪律，还是含「发现冲突如何让步」的动态协议？
- [ ] **`list` 是否要标出「当前进程所在的那棵树」**（需要 CLI 与运行中会话通信，或由 `/status` 承担）？

## 后续建议

- 设计见 [`design.md`](./design.md)（§4 保留了并行 subagent 产出的三个候选方案作决策记录）。
- 建议用 `prototype` 验证两个假设：
  1. 项目级 `.yushan/` 缺失的行为：确认 worktree 内 `build_system_prompt` 加载差异（项目 skill /
     自定义 prompt **缺席**）与 `attach` 的缺失警告（含主仓无 `.yushan/` 时不打扰用户）。
  2. `attach` 与 `-p` / `--json` / `--resume` 的组合行为。
