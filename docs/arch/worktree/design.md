# git worktree 支持 — 架构设计

> 上承 [`context.md`](./context.md)。决策记录见 [`docs/adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md)。
> §4 记录了并行 subagent 产出的三个候选方案；**最终形态经用户两轮裁定后，比三者都小**（见 §1 与 §4.3）。

## 1. 结论先行

**CLI 子命令组 `ys-coding-agent worktree {create, attach, list, remove}`；工作区只在进程启动时选定一次。**

| 动词 | 性质 | 实现 |
|---|---|---|
| `create <name>` | git 包装 | `git worktree add -b worktree-<name> <ys目录>/<repo>-<name> HEAD` |
| `list` | git 包装 | `git worktree list --porcelain`，筛出 ys 目录下的条目 |
| `remove <name>` | git 包装 | `git worktree remove <path>`（**天然留分支、脏则拒删**） |
| **`attach <name>`** | **ys 特有** | **启动（或 resume）一个会话，工作区绑定到该树** —— git 里没有对应物 |

⇒ **无工具、无斜杠命令、无中途切换、无 IPC、零核心改动。**

**为什么这么小**：用户裁定 `attach` = 「**启动即绑定**」⇒ 一切「工作区切换」都发生在**构造点**
（`main` 启动期）。而代码里 workspace **本来就是单点量、启动期冻结**
（`apps/coding-agent/src/main.rs:1341-1342`：`let cwd = config.cwd.clone(); let workspace = cwd.clone();`）
⇒ **四路扇出（`build_tools` / `working_dir` / `build_system_prompt` / `SubagentSetup`）原样不动**，
只把「workspace 取什么值」从「恒等于 `cwd`」改成：

```rust
let workspace = match args.worktree {
    Some(Attach::Name(name)) => worktree::resolve(&cwd, &name)?,  // 解析到 ys 目录下的树
    None => cwd.clone(),                                          // 现状：共享工作区
};
```

## 2. 需求与约束

用户裁定（不可改）：一个需求 ⇒ 一个 worktree；CLI 四动词；`attach` = 启动即绑定；
需求内多 agent 共用同一树、**冲突靠 skill 协商（运行时不做机制兜底）**；**只隔离不回流**；
非 git / git 不可用 / 目标不存在 ⇒ **显式报错并指出共享工作区的退路**；不做安全/沙箱；不做 Windows。

设计约束（带指针）：

- **零核心改动的前提**：workspace 是启动期单点量（`main.rs:1341-1342`），且四路扇出都从它派生
  （`toolset.rs:88` / `AgentBuilder::working_dir` / `prompt.rs:28` / `main.rs:1362`）。
  `attach` 只改**取值来源**，不触碰扇出。**这是本设计成立的全部理由。**
- 工具**构造期捕获** workspace（`tools-basic/src/read.rs:48` 等）⇒ 工作区必须在建工具**之前**定死
  —— 启动期解析恰好满足。
- **参数解析是手写的** `parse_args`（`main.rs:161-222`），match 首 token。加 `"worktree"` 臂是纯增量；
  ⚠️ 注意今天的 `_ =>` 兜底会**静默忽略**非 flag 首 token（`ys-coding-agent worktree` 现在会直接进交互模式）。
- `ys-core`/`ys-event`/`ys-protocol` **零 tokio** ⇒ git 只在 apps 层调。
- `capabilities.rs` 绝不 print；但 CLI 子命令走的是 **stdout/stderr**（非 transcript），与 `-p`/`--json` 同侧。
- **prompt cache**：cwd 与 `<project_instructions path="…">` 都进提示词文本（`prompt.rs:60-64,69`）
  ⇒ **每个 worktree 一份前缀**，首次 attach 首轮 cache miss。
- 全仓**零 git 调用**；`.gitignore` 不含 `.worktrees/` / `.yushan/`。

## 3. 同类软件调研（Step 2）

- **最有价值的先例** —— `tmp/deepseek-harness/.agents/notes/implemented/feature/2026-08-05-agent-teams.md`：
  > "**Worktree isolation is not a harness runtime behavior.** A deployment or prompt may arrange
  > separate worktrees, but the Team domain does not infer branches, merge changes, or silently
  > change cwd."

  其「Alternatives considered」明确否决 **自动建 worktree**（「创建/命名/合并策略/清理都是部署决策」）
  与 **把写范围当锁**（「外部写者绕过锁、崩溃持有者永久占位、路径前缀重叠无法证明语义独立，
  **假互斥比显式警告更危险**」）。⇒ 支持「隔离交给部署（CLI/启动参数）、冲突交给 skill」。
- **Claude Code**：`--worktree/-w` 启动即进树（本设计的直接对应物）；`.claude/worktrees/<name>/`、分支
  `worktree-<name>`；另有 `EnterWorktree`/`ExitWorktree` 工具（**中途切换**）。
  ⚠️ 那两个工具带来**两个已记录 bug**：#76250（会话级共享 CWD 槽 ⇒ 并发 subagent 跨树误改）、
  #33（切换是会话级副作用 ⇒ 并行 agent 共享一树）。**本设计只取 `--worktree` 形态、不取工具，正是为了
  绕开这两个 bug**（不切换，就不存在会话级 CWD 副作用）。
- **`tmp/Codewhale/crates/lane/src/worktree.rs`**（Rust 554 行）：`git worktree add -b <branch> <path> <base>`；
  清理前用 `git worktree list --porcelain` + `.git` 的 `gitdir:` 确认 git 认它是本仓托管的树；
  删分支用 `-d` 不 `-D`（「a branch carrying unmerged commits is someone's work, and a TTL timer is not a
  mandate to throw it away」）。**其 TTL 与安全闸不采纳**（本设计无自动清理，见 §5.3-c）。
- **`tmp/pi/.../resource-loader.ts` 的 `findShadowedContextFile`**：嵌套 worktree 会**重复加载**主仓同名
  上下文文件 ⇒ 直接支持「落仓库外」（§5.3-d）。
- **生态编排器**（vmux/dmux/parallel-code/CLITrigger/Baton）一律「一任务 = 一分支 = 一 worktree、review
  diff 再合并」；`pact` 自陈 file-claim coordination「not yet reliable under concurrency」。

## 4. 候选方案（决策记录）

三个候选方案由并行 subagent 产出，**都假设了「运行中途切换工作区」**这一前提。

### 4.1 三者

| | 骨架 | 上卷机制 | 落点 | 生命周期 |
|---|---|---|---|---|
| **A 最小复杂度** | `worktree.rs`+`assembly.rs`（0 新 trait） | `RebindHandle`（`Arc<Mutex<Option<PathBuf>>>`） | 仓库外 | 只建不回收 |
| **B 可扩展优先** | provider/ledger/service + 2 新 trait | `ToolContext` 第 4 挂载点 + 状态机 + TTL | 仓库内 | 状态机 + ledger + TTL |
| **C 资源/缓存效率** | `worktree.rs`+`assembly.rs`+挂载点 | `ToolContext` 第 4 挂载点 | 仓库内 | 不回收 |

### 4.2 对比矩阵

| 维度 | A | B | C |
|---|---|---|---|
| 新模块 | 2 | 4+ | 3 |
| 新增 trait | **0** | 2 | 1 |
| 改核心 crate | **0** | 3（`ys-tool`/`ys-runtime`/`ys-loop`） | 3 |
| 新增状态 | 1 个 `Arc<Mutex>` | 状态机 + `ledger.jsonl` | 1 个 `Arc<Mutex>` |
| 落点正确性 | 仓库外 ✅ | 仓库内 ❌（重复加载 + 搜索遍历） | 仓库内 ❌ |
| 主要风险 | 同回合不生效 | 重建漏换 `SubagentSetup`；ledger 一致性 | 缓存论断经核对**不成立** |

### 4.3 最终形态不在三者之中（且更小）

用户裁定 `attach` = **启动即绑定** ⇒ 「中途切换」这个前提消失了：

- **上卷机制**（A 的信元 / B·C 的挂载点）**整块删掉** —— 没有中途意图要上卷；
- **`assembly.rs` 重构整块删掉** —— 一个进程只有一个工作区，无需把四路扇出收成可重建的工厂；
- **工具、`Request::Worktree`、命令表项全部删掉**；
- ⇒ **净新增 = 一个 `worktree.rs` + `parse_args` 一个 match 臂 + `workspace` 取值改一处。**

这也顺带删掉了候选 A/C 共同的、无法回避的**「同回合不生效」**问题 —— 不切换就没有生效延迟。
**B/C 被否的理由**（穿核心 crate、仓库内落点）在 §5.3 保留。

## 5. 推荐方案

### 5.1 CLI 表面

```text
ys-coding-agent worktree create <name>     # 建树：落 ys 目录、分支 worktree-<name>、base HEAD
ys-coding-agent worktree attach <name>     # 启动/resume 会话，工作区 = 该树（可带 -p / --json / --resume）
ys-coding-agent worktree list              # 列出 ys 目录下的工作树（含路径 / 分支 / 是否脏）
ys-coding-agent worktree remove <name>     # 删树：直套 git worktree remove（留分支、脏则拒删）
```

`attach` **要求树已存在**（否则报错并指向 `create`）—— 与 `create` 分离是有意的：
创建与绑定是两个动作，`create` 可脱离会话先跑。

### 5.2 模块与接口

```rust
// 新：apps/coding-agent/src/worktree.rs —— 唯一 git 调用点
pub fn dir() -> PathBuf;                            // ~/.yushan/worktrees/（YUSHAN_WORKTREES_DIR 覆盖）
pub fn path_for(repo: &Path, name: &str) -> PathBuf; // <dir>/<repo>-<name>
pub fn repo_root(cwd: &Path) -> Result<PathBuf, WorktreeError>;  // git rev-parse --show-toplevel
pub fn create(repo: &Path, name: &str) -> Result<Entry, WorktreeError>;
pub fn list(repo: &Path) -> Result<Vec<Entry>, WorktreeError>;
pub fn remove(repo: &Path, name: &str) -> Result<(), WorktreeError>;
pub fn resolve(repo: &Path, name: &str) -> Result<PathBuf, WorktreeError>;  // attach 用

pub struct Entry { pub name: String, pub path: PathBuf, pub branch: Option<String>, pub dirty: bool }
pub enum WorktreeError { NotARepo, GitUnavailable, AlreadyExists(String), NotFound(String), Git(String) }
```

```rust
// 改：apps/coding-agent/src/main.rs
struct Args { mode: Mode, stats: bool, resume: Resume, worktree: Option<WorktreeCmd> }
enum WorktreeCmd { Create(String), Attach(String), List, Remove(String) }
// parse_args：match 首 token 加 "worktree" 臂（纯增量，见 §2 的注意点）
// main：workspace 取值改为「Attach ⇒ resolve()，否则 cwd」（§1 的 6 行）
```

**不改**：`ys-protocol` / `ys-tui-coding` / `ys-tool` / `ys-runtime` / `ys-loop` / `tools-basic`。

### 5.3 关键决策

**a. CLI 而非工具或斜杠命令**（用户裁定）。产物存在 **ys 自己的目录**下 ⇒ 用户不该被迫去认识 git 的
路径与命令；创建是**进程级副作用**，CLI 天生合适；覆盖面最广（**用户 shell / 模型经 `bash` 工具 /
skill 文本 / CI 脚本**都能调）。且它消灭了「工具返回『下回合生效』」这个说不出实话的契约。

**b. `attach` = 启动即绑定**（用户裁定）⇒ 无中途切换 ⇒ 零核心改动、无 deferred 语义、无 IPC。
`attach` 是四个动词里**唯一没有 git 对应物**的，因为「绑定会话」不是 git 操作。

**c. `remove` 直套 `git worktree remove`，不写自定义安全闸。** 该命令**天然留分支、脏则拒删**
—— 恰好就是「只隔离不回流」要的策略，免费获得。Codewhale 的 `is_managed_worktree` 安全闸是为了防
**自动清理**误删陈旧记录，而本设计**不引入 TTL / ledger / 自动清理**，故无此前提。（若将来加自动清理，
须回头补那道闸。）

**d. 落点仓库外** `~/.yushan/worktrees/<repo>-<name>/`。三条理由：① `EXCLUDED_DIRS`
（`tools-basic/src/lib.rs:44`）不含 worktree 根 ⇒ 仓库内落点会被 `grep -r`/`find` 搜进完整副本；
② `load_project_context` 逐级向上**全收**（`prompt.rs:277-305`）⇒ 仓库内落点时 worktree 自己的
`AGENTS.md` 与主仓的会**两份都进上下文**（`tmp/pi` 专门修过）；③ 不污染主树 `git status`。

**e. 不自动回收、无 TTL。** 「只隔离不回流」⇒ 自动删会毁掉未 merge 的工作（deepseek-harness 的
「清理是部署决策」同旨）。回收 = 用户显式 `remove`。

**f. 不引入 ledger / 元数据存储。** `list` 从 `git worktree list --porcelain` 现算即可，
ys 目录前缀天然把「我们的树」筛出来。

### 5.4 成本

| 项 | 值 |
|---|---|
| `create` | 一次 `git worktree add`（中仓 ~0.5–2s，I/O bound，建树即 checkout） |
| `attach` | 一次 `git rev-parse` + `resolve` 路径拼接；**启动路径其余部分零变化** |
| 子进程输出污染 | **自动满足** —— CLI 是独立进程，stdout/stderr 由终端或 `bash` 工具捕获，碰不到 TUI raw mode |
| 首次 attach 的首轮 | 该树一份新 prompt 前缀 ⇒ 一次 cache miss（cwd + `project_instructions` 路径进文本） |

## 6. 分阶段落地

- **P0**：`worktree.rs` + `parse_args` 加 `worktree` 子命令；`create` / `list` / `remove`（三个 git 薄包装）
  + 落点/命名/`YUSHAN_WORKTREES_DIR` + `WorktreeError` 三态降级文案。
- **P1**：`attach` —— `main` 的 `workspace` 取值改点（§1 的 6 行）+ 树不存在/非 git 时的显式报错与退路提示。
- **P2（增量）**：`SessionMeta.workspace` 只读记录（`#[serde(default)]`）⇒ `--resume` 时提示「本会话原在
  工作树 X，当前在主树 Y」，避免**静默错位**。
- **P3（可延后）**：`attach --create-if-missing`；`/status` 显示当前是否在树内。

## 7. 已知风险与待验证项

- [ ] **session ↔ worktree 无绑定**：`attach` 起的会话不记录工作区 ⇒ 之后裸 `--resume` 会落回**主树**，
      历史与工作区错位。P2 的 `SessionMeta.workspace` 是缓解（提示而非自动切换）。
- [ ] **项目级 `.yushan/`（skills / SYSTEM.md）在树内缺席**（未跟踪文件不随 checkout 搬）⇒ 同一 agent
      在两棵树里**行为漂移**。P1 须在 `attach` 时**明示**。
- [ ] **首次 attach 首轮 cache miss**（每树一份前缀）。
- [ ] **同名仓库撞名**：`<repo>-<name>` 在「不同路径、同名目录」的两个仓库间会撞 ⇒ 是否加短 hash（待定）。
- [ ] `attach` 时树不存在 ⇒ 报错（**暂定**；`--create-if-missing` 留 P3）。
- [ ] **将来加自动清理时**须补回 Codewhale 的 `is_managed_worktree` 安全闸（防陈旧记录变无界递归删）。
- [ ] **非 git 仓库**下四个动词都不可用 ⇒ 明确报错，并指出「不带子命令即共享工作区启动」的退路。
- [ ] 建议用 `prototype` 验证：① worktree 内 `build_system_prompt` 的实际加载差异（尤其 `.yushan/`）；
      ② `attach` 与 `--resume` / `-p` / `--json` 的组合行为。

## 8. 相关文档

- [`context.md`](./context.md) — 上下文与约束
- [`../../adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md) — 决策记录
- [`../../design-final/coding-agent-tui.md`](../../design-final/coding-agent-tui.md) §13 — subagent 派发与 workspace 继承的 as-built
