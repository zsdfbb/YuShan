# git worktree 支持 — 架构设计

> 上承 [`context.md`](./context.md)。决策记录见 [`docs/adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md)。
> 质量分析见 [`review.md`](./review.md)（本版已按其实施修订，见 §9 修订记录）。
> §4 记录了并行 subagent 产出的三个候选方案；**最终形态比三者都小**（见 §1 与 §4.3）。

## 1. 结论先行

**CLI 子命令组 `ys-coding-agent worktree {create, attach, list, remove}`；工作区只在进程启动时选定一次。**

| 动词 | 性质 | 实现 |
|---|---|---|
| `create <name>` | git 包装 | `git worktree add -b worktree-<name> <ys目录>/<仓库的 HOME 相对路径，逐分量作目录>/<name> HEAD` |
| `list` | git 包装 | `git worktree list --porcelain`，筛出 ys 目录下的条目 |
| `remove <name>` | git 包装 | `git worktree remove <path>`（**天然留分支、脏则拒删**） |
| **`attach <name>`** | **ys 特有** | **启动（或 resume）一个会话，工作区绑定到该树** —— git 里没有对应物 |

⇒ **无工具、无斜杠命令、无中途切换、无 IPC、零核心 crate 改动。**

### 1.1 注入点仍是 `cwd`；项目级 `.yushan/` 不随树迁入（本版修订）

`cwd` 在启动序列里**同时喂三个消费者**，所以「工作区切到哪」不是一个自由参数：

```text
config.rs:176   cwd = current_dir()
main.rs:1227    skill_roots = default_skill_roots(&config.cwd)   ← ① skill 根
main.rs:1341    let cwd = config.cwd.clone();
main.rs:1352    build_system_prompt(&cwd, …)                      ← ② 提示词（含 Current working directory
main.rs:1375    build_system_prompt(&cwd, …)                         与 find_custom_prompt(cwd)）
main.rs:1342    let workspace = cwd.clone()  →  build_tools(&workspace) / working_dir(...)  ← ③ 工具
```

只改 `workspace` 会让**工具在树里、提示词却说主树**；只改 `cwd` 又会让**项目级 skill 与 `.yushan/SYSTEM.md`
在树内消失**（树里没有未跟踪文件）。二者都不可接受，故注入点必须在 **skill 扫描之前**（§1.2）。

| 消费者 | 取值来源 |
|---|---|
| ① skill 根（`default_skill_roots(&config.cwd)`） | `config.cwd` |
| ② 提示词（`Current working directory` / `find_custom_prompt`） | `config.cwd` |
| ③ 工具（`build_tools(&workspace)` / `working_dir`） | `config.cwd` 派生的 `workspace` |

**三者都由同一个 `cwd` 派生**，**不引入「项目根」第二量**。但**不建软链**：项目级 `.yushan/`
（skills / `.yushan/SYSTEM.md`）是**未跟踪文件**，不随 `git worktree` 迁入 ⇒ **在树内不可见**；
`attach` 成功后若主仓存在 `.yushan/`，向 stderr 打一条响亮警告告知用户（§5.3-d′）。
非 worktree 路径下无软链、`cwd` 原样 ⇒ **行为逐字节不变**。

### 1.2 改动的真实位置（不是「一行」）

```rust
// main.rs：紧接 Config::from_env()、且在 skill 扫描（:1224）之前
if let Some(WorktreeCmd::Attach(name)) = &args.worktree {
    match resolve_attach_target(&config.cwd, name) {
        Ok((tree, _)) => {
            let main_root = worktree::main_repo(&config.cwd).ok().map(|r| r.root);
            config.cwd = tree;                  // 工作区 + 提示词 + skill 根 全部跟随
            // 项目级 `.yushan/` 不随 `git worktree` 迁入 ⇒ 树内不可见；主仓有则告知用户。
            if let Some(root) = main_root && worktree::has_project_config(&root) {
                eprintln!("警告：本仓有项目级配置 `.yushan/`…本会话中不可见。");
            }
        }
        Err(e) => eprintln!("警告：无法绑定工作树（{e}）"),  // 响亮警告，保持 cwd 不变，继续共享工作区启动
    }
}
```

只需改写 `config.cwd` 一处（§1.1）：三个消费者都从它派生；项目级 `.yushan/` 不随树迁入（§5.3-d′），
故 `attach` 时按需打缺失警告。
**触及 1 个 app 文件（`main.rs`）+ 1 个新文件（`worktree.rs`）；`config.rs` / `prompt.rs` 完全不动，0 个核心 crate。**

## 2. 需求与约束

用户裁定（不可改）：一个需求 ⇒ 一个 worktree；CLI 四动词；`attach` = 启动即绑定；
需求内多 agent 共用同一树、**冲突靠 skill 协商（运行时不做机制兜底）**；**只隔离不回流**；
**非 git / git 不可用 / 目标不存在 ⇒ 显式降级**（`attach` 见 §5.3-b）；不做安全/沙箱；不做 Windows。

设计约束（带指针）：

- workspace 是启动期单点量（`main.rs:1341-1342`），四路扇出全从它派生
  （`toolset.rs:88` / `AgentBuilder::working_dir` / `prompt.rs:28` / `main.rs:1362`）。
- 工具**构造期捕获** workspace（`tools-basic/src/read.rs:48` 等）⇒ 工作区必须在建工具**之前**定死。
- 项目级 skill 根取 `config.cwd`（`main.rs:1227` → `config.rs:321-331`）；自定义 prompt 取
  `cwd/.yushan/SYSTEM.md`（`prompt.rs:323-327`）⇒ 两者都跟随同一个 `cwd`（§1.1）；树内可见性**不成立**
  （项目级 `.yushan/` 不随树迁入，§5.3-d′），故 `attach` 时按需打缺失警告。
- **参数解析是手写的** `parse_args`（`main.rs:161-222`），match 首 token。加 `"worktree"` 臂是纯增量；
  ⚠️ 今天的 `_ =>` 兜底会**静默忽略**非 flag 首 token（`ys-coding-agent worktree` 现在会直接进交互模式）。
- `parse_args` 之后立刻是 `Config::from_env()`（`main.rs:1207`） ⇒ `worktree list/create/remove`
  **在此前分流并早退**（它们不需要 model / session / TUI）；`attach` 继续走正常启动。
- `ys-core`/`ys-event`/`ys-protocol` **零 tokio** ⇒ git 只在 apps 层调。
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
  **假互斥比显式警告更危险**」）。
- **Claude Code**：`--worktree/-w` 启动即进树（本设计的直接对应物）；`.claude/worktrees/<name>/`、分支
  `worktree-<name>`；另有 `EnterWorktree`/`ExitWorktree` 工具（**中途切换**）。
  ⚠️ 那两个工具带来**两个已记录 bug**：#76250（会话级共享 CWD 槽 ⇒ 并发 subagent 跨树误改）、
  #33（切换是会话级副作用 ⇒ 并行 agent 共享一树）。**本设计只取 `--worktree` 形态、不取工具**。
- **`tmp/Codewhale/crates/lane/src/worktree.rs`**（Rust 554 行）：`git worktree add -b <branch> <path> <base>`；
  清理前用 `git worktree list --porcelain` + `.git` 的 `gitdir:` 确认 git 认它是本仓托管的树；
  删分支用 `-d` 不 `-D`。**其 TTL 与安全闸不采纳**（本设计无自动清理）；**采纳**其「主仓根由
  `worktree list` 首条给出」的思路（本设计用 `--git-common-dir`，见 §5.2）。
- **`tmp/pi/.../resource-loader.ts` 的 `findShadowedContextFile`**：嵌套 worktree 会**重复加载**主仓同名
  上下文文件 ⇒ 支持「落仓库外」（§5.3-d）；其 `dirname(commonGitDir)` 也正是 §5.2 取的**主仓根**。
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

用户裁定 `attach` = **启动即绑定** ⇒ 「中途切换」这个前提消失了：上卷机制（A 的信元 / B·C 的挂载点）
与 `assembly.rs` 重构**整块删掉**；工具、`Request::Worktree`、命令表项也删掉。⇒ 净新增 =
一个 `worktree.rs` + `parse_args` 一个 match 臂 + `attach` 的单点注入（§1.2，`main.rs` 一处 + 新文件）。
**B/C 被否的理由**（穿核心 crate、仓库内落点）在 §5.3 保留。

## 5. 推荐方案

### 5.1 CLI 表面

```text
ys-coding-agent worktree create [<name>]   # 建树：落 ys 目录、分支 worktree-<name>、base HEAD；unspecified ⇒ 自动命名
                                          # 建完**即以新树为工作区启动**（可带 -p / --json / --resume）
ys-coding-agent worktree attach [<name>]   # 启动/resume 会话，工作区 = 该树（可带 -p / --json / --resume）；unspecified ⇒ 列表选取
ys-coding-agent worktree list              # 列出 ys 目录下的工作树（含路径 / 分支 / 是否脏）
ys-coding-agent worktree remove <name>     # 删树：直套 git worktree remove（留分支、脏则拒删）
```

`attach` **要求树已存在**（否则见 §5.3-b 的降级）。`create` = **建树 + 以新树为工作区启动**，
与 `attach` 共用同一注入点、同一套尾部 flag；`list` / `remove` 仍是纯进程级动作、执行后早退。
**两个动词的名参都可省**（§5.3-i/§5.3-j）。

### 5.2 模块与接口

```rust
// 新：apps/coding-agent/src/worktree.rs —— 唯一 git 调用点
pub fn dir() -> PathBuf;                            // ~/.yushan/worktrees/（YUSHAN_WORKTREES_DIR 覆盖）
pub fn main_repo(cwd: &Path) -> Result<RepoInfo, WorktreeError>;  // ← 主仓根 + 名
pub fn path_for(repo: &RepoInfo, name: &str) -> Result<PathBuf, WorktreeError>;  // 校验后按「仓库的 HOME 相对路径」逐分量作目录，再拼 <name>
pub fn auto_name() -> String;                       // §5.3-i：复用 `new_session_id` 形态
pub fn create(repo: &Path, name: Option<&str>) -> Result<Entry, WorktreeError>;  // None ⇒ auto_name()
pub fn list(repo: &Path) -> Result<Vec<Entry>, WorktreeError>;
pub fn remove(repo: &Path, name: &str) -> Result<(), WorktreeError>;
pub fn attach_target(cwd: &Path, name: &str) -> Result<PathBuf, WorktreeError>;  // 工作区（单一返回值）

pub struct RepoInfo { pub root: PathBuf, pub name: String }
pub struct Entry { pub name: String, pub path: PathBuf, pub branch: Option<String>, pub dirty: bool }
pub enum WorktreeError { NotARepo, GitUnavailable, InvalidName(String), NotFound(String), Git(String) }
```

**主仓根/名的取法**（修订 review 风险 6）：用 `git rev-parse --git-common-dir` 的**父目录**，
**不是** `--show-toplevel` —— 后者在**树内**运行时返回的是**树根**，据此派生的落点会把树根当成仓库、
多嵌一层错名（`path_for` 的逐分量嵌套会把它当作一组额外分量）。
（`tmp/pi` 的 `dirname(commonGitDir)` 同款。）

```rust
// 改：apps/coding-agent/src/main.rs（§1.2）+ 新文件 worktree.rs；config.rs / prompt.rs 不动
struct Args { mode: Mode, stats: bool, resume: Resume, worktree: Option<WorktreeCmd> }
enum WorktreeCmd { Create(String), Attach(String), List, Remove(String) }
```

**不改**：`ys-protocol` / `ys-tui-coding` / `ys-tool` / `ys-runtime` / `ys-loop` / `tools-basic`。

### 5.3 关键决策

**a. CLI 而非工具或斜杠命令**（用户裁定）。产物存在 **ys 自己的目录**下 ⇒ 用户不该被迫去认识 git 的
路径与命令；创建是**进程级副作用**，CLI 天生合适；覆盖面最广（**用户 shell / 模型经 `bash` 工具 /
skill 文本 / CI 脚本**都能调）。且它消灭了「工具返回『下回合生效』」这个说不出实话的契约。

**b. 降级：`attach` 回退、`create`/`remove` 硬失败**（修订 review 风险 2，与用户原裁定对齐）。
- `attach` 在非 git / git 不可用 / 树不存在时 ⇒ **响亮警告 + 以共享工作区继续启动**（用户已明确要
  「显式降级为共享工作区」；此处自动回退并继续，正是这个语义）。
- `create` / `remove` 是**显式动作**，语义不该被静默改写 ⇒ **硬失败**（非零退出 + 明确原因 + 退路提示）。

**c. `remove` 直套 `git worktree remove`，不写自定义安全闸。** 该命令**天然留分支、脏则拒删** ——
恰好是「只隔离不回流」要的策略，免费获得。Codewhale 的 `is_managed_worktree` 安全闸是为防**自动清理**
误删陈旧记录，而本设计**不引入 TTL / ledger / 自动清理**，故无此前提。（将来若加自动清理，须回头补。）
⚠️ 需把 git 的**拒删原因透传给用户**（修订 review 风险 7）：提示「该树有未提交改动，先 merge 或
`--force`」——否则用户只看到一句失败。

**d. 落点仓库外** `$HOME/.yushan/worktrees/<仓库的 HOME 相对路径，逐分量作目录>/<name>`（如主仓在
`~/Develop/YuShan` ⇒ `~/.yushan/worktrees/Develop/YuShan/<name>`）；仓库在 HOME 以外 ⇒ 用绝对路径的
各分量，逐分量嵌套（罕见）。
用「仓库的 HOME 相对路径」而非裸仓库名，是为了让**不同路径、同名目录**的两个仓库不相撞（§7）。
**逐分量作目录**（而非拼成单个目录名）使 `~/a/b` 与 `~/a-b` 落点不同、`list` 不互相串台。
三条理由：① `EXCLUDED_DIRS`
（`tools-basic/src/lib.rs:44`）不含 worktree 根 ⇒ 仓库内落点会被 `grep -r`/`find` 搜进完整副本；
② `load_project_context` 逐级向上**全收**（`prompt.rs:277-305`）⇒ 仓库内落点时 worktree 自己的
`AGENTS.md` 与主仓的会**两份都进上下文**（`tmp/pi` 专门修过）；③ 不污染主树 `git status`。

**d′. 不建软链；项目级 `.yushan/` 在树内不可见**（用户裁定）。`create` **只做 `git worktree add`**，
不在 `<树>/.yushan` 建任何东西。项目级配置（skills / `.yushan/SYSTEM.md`）是未跟踪文件，不随
`git worktree` 迁入 ⇒ **树内不可见**；`attach` 成功后若主仓存在 `.yushan/`（目录存在即可），向 stderr
打一条响亮警告说明「本会话中不可见」（主仓无 `.yushan/` ⇒ 不打，免噪声）。工作区仍是**单量 `config.cwd`**，
不引入「项目根」第二量。

**为什么不建软链**：git 2.54 实测 linked worktree **不读** `<gitdir>/info/exclude`，故软链会把树弄脏，
`remove` 需 `--force` 才能过。要让这条软链被忽略，只有两条路 —— ① 改**用户仓库 common config**
（`extensions.worktreeConfig` + `core.excludesFile`，把仓库格式升到 1），② 保留一条 `--force` 销毁路径。
**两者都被否决**（用户裁定「暂时还没用到这种场景」，不为一个尚未发生的需求付这个代价）。

**e. 不自动回收、无 TTL。** 「只隔离不回流」⇒ 自动删会毁掉未 merge 的工作（deepseek-harness 的
「清理是部署决策」同旨）。

**f. 不引入 ledger / 元数据存储。** `list` 从 `git worktree list --porcelain` 现算。

**g. 陈旧 registration 的处置**（修订 review 风险 3）：用户的 `rm -rf ~/.yushan/worktrees/…` 是常见动作，
git 仍持有登记 ⇒ 同名 `create` 会报 `already registered`。⇒ `create` 在**同名路径不存在但 git 仍登记**时，
先 `git worktree prune` 再重试一次；仍失败则错误信息直接给 `git worktree prune` 指引。

**h. `name` 必须校验**（修订 review 风险 5）：非空、不含 `/` 与 `\`、不等于 `.` / `..`、不含路径分隔符。
否则 `path_for` 可被 `../` 带出 ys 目录（`create` 会因此在任意位置建树）。违反 ⇒ `InvalidName`。

### 5.4 成本

| 项 | 值 |
|---|---|
| `create` | 一次 `git worktree add`（中仓 ~0.5–2s，I/O bound，建树即 checkout） |
| `attach` | 一次 `rev-parse --git-common-dir` + 路径拼接；**启动路径其余部分零变化** |
| 子进程输出污染 | **自动满足** —— CLI 是独立进程，stdout/stderr 由终端或 `bash` 工具捕获，碰不到 TUI raw mode |
| 首次 attach 的首轮 | 该树一份新 prompt 前缀 ⇒ 一次 cache miss（cwd + `project_instructions` 路径进文本） |
| `list` | `dirty` 每树一次 `git status --porcelain` ⇒ 树多时 N 次子进程（文档写明；必要时加 `--dirty` 开关） |

### 5.5 自动命名与两处打印（用户追加需求）

**i. `create` 可不给名字 ⇒ 自动生成。** 复用**既有**的 id 生成形态 `wiring::new_session_id()`
（`{unix_secs}_{subsec_nanos:09}`，`wiring.rs:235-240`）—— **零新依赖、零碰撞、与既有 id 同风格**。
`attach` 无参 ⇒ **从 `list` 中选取**（照 `--resume` 无参 `Resume::Pick` 的先例，`main.rs:205-209`），
所以名字不必好记、不必手打。

> **不采纳「可读名」**（`bright-running-fox` 那类，Claude Code 的做法）：它要带一份**词表**（数据 + 维护 +
> 仍可能碰撞），而本仓无此先例、亦无此需求。名字在 `attach` 可选取的前提下几乎不可见。
> 若将来确实要可读名，那是**独立小改动**，可随时加。

**j. 建树告知与会话退出行，各按既有去处。**

- **`create` 成功 ⇒ 告知行（`worktree: <name>` / 路径 / 分支未合并）并入启动期提示**，不单独打 stderr。
  理由：`create` 建完即起 TUI，起屏前写 stderr 的行会在退出 alt-screen 后变成主屏残渣（§5 输出三分）。
  并入启动期提示后，TUI 进 transcript、`-p` / `--json` 进 stderr —— 一处文案、两处正确去处。
- **非终端环境（stdin/stdout 不是终端）⇒ 不启 TUI**：`enable_raw_mode()` 在管道 stdio 下必失败，
  CI / 脚本里的 `create` 不能因此不可用。此时打印告知行 + 一句「未启动 TUI；要在该树里开会话：
  `worktree attach <name>`」后 `exit(0)` —— 建树本身成功，降级只影响「是否顺手起会话」。
- **会话结束 ⇒ 在既有退出行处追加 `worktree: <name>`**，并提示「分支 `worktree-<name>` **未合并**；
  `git merge worktree-<name>` 或 `worktree remove <name>`」。复用 ADR-0032 的三模式统一出口
  （TUI 在 `main.rs:1191` 退出回放之后；`-p`/`--json` 在 `persist_single_turn`），文案与 `session_exit_line`
  （`main.rs:551`）并列。

> **⚠️ 退出行需要新句柄（U4 修订）。** 原先判「工作区在启动期定死、运行期不变 ⇒ 退出行不需要
> `SessionPathHandle` 式快照」—— **U4 推翻了这个判断**：会话内 `/resume` 可切到一条记录了别的
> 工作树的会话（用户明确要求），故工作区**运行期可变**。退出行与回合末 `SessionMeta.workspace`
> 现由 **`app_loop::WorktreeHandle`**（照 ADR-0032 `SessionPathHandle` 的 `Arc<Mutex<Option<…>>>`
> 范式）承载：`main` 启动期播种、app 循环在 `/resume` 后刷新、`main` 在 `rt.shutdown_timeout`
> 之后读。见 §5.3-k。

**k. 会话内 `/resume` 换工作区（U4，as-built）。**

`/resume` 发生在**回合边界**（`app_loop` 处理 `Request::ResumeSession`），此时可**重建 agent**：
一次切换要**同时**换掉三处派生自工作区的量 —— 工具表（每个工具在构造期捕获工作区）、
`AgentBuilder::working_dir`、system prompt（`Current working directory` + 项目上下文）。三者
必须一起换，否则会话处于不一致状态（工具写到旧树、提示词谎报 cwd）。

- **装配工厂**：`app/src/assembly.rs::AgentFactory` 冻结工作区**无关**的量（`WebConfig` /
  `RunLimits` / skill `Catalogue` / `SubagentsDir`），只把**工作区**作为每次变化的入参 ——
  启动路径与会话内重建共用同一份序列（防漂移）。**工厂无副作用**（`sweep_fetched` /
  `SubagentsDir::sweep_old` 仍只由 `main` 在启动期各调一次）。
- **四情形**（对齐启动期 U3 的语义，但**绝不静默采用调用方 cwd**）：
  ①记录存在且在且与当前不同 ⇒ 重建到该树并告知；②记录存在但树已不在 ⇒ 明确告知、**保持当前
  工作区**（不重建）；③无记录且当前在工作树 ⇒ 切回**共享工作区**（进程启动目录，`main` 在
  `attach` 改写 `config.cwd` 前捕获）并告知；④无记录且已在共享 ⇒ 不动。
- **`/new` 不换工作区**（同一会话谱系、同一需求）。
- `/status` 的 `cwd` 经 `CodingView.cwd`（取自 `config.cwd`）刷新 —— 切工作区后 `app_loop` 每次
  请求末尾重推 `Outbound::View`，无需新 `Outbound` 变体。

参见 ADR-0034 的 U4 修订与 `apps/coding-agent/src/assembly.rs` / `app_loop.rs::apply_resumed_workspace`。

## 6. 分阶段落地

- **P0**：`worktree.rs`（含 §5.3-g/h/i 与 §5.2 的主仓根取法）+ `parse_args` 加 `worktree` 子命令与
  **早期分流早退**；`create`（含无参自动命名）/ `list` / `remove` 三个 git 薄包装 + `WorktreeError` 四态
  + `create` 的 `worktree: <name>` stderr 打印（§5.3-j）。
- **P1**：`attach`（含无参列表选取）+ §1.2 的单点注入（`main.rs` 一处改写 `config.cwd`）+ §5.3-d′ 的项目级
  `.yushan/` 缺失警告（主仓有 `.yushan/` 时打）。
  含 §5.3-b 的降级回退，以及**退出行追加 `worktree: <name>`**（§5.3-j）。
- **P2**：`SessionMeta.workspace` 只读记录（`#[serde(default)]`）⇒ `--resume` 提示工作区错位
  （**升为 P1 级重要性**：不做则 `--resume` 静默落回主树）。
- **U4（已落地）**：会话内 `/resume` 换工作区 —— 装配工厂 `assembly.rs::AgentFactory` +
  `app_loop::WorktreeHandle` + `apply_resumed_workspace`（§5.3-k）。
- **P3（可延后）**：`attach --create-if-missing`；`/status` 显示当前是否在树内；`list --dirty` 开关。

## 7. 已知风险与待验证项

- [x] ~~`attach` 的注入点未定~~ ⇒ 已定为 §1.1/§1.2 的单量注入（改写 `config.cwd` 一处；项目级 `.yushan/` 不随树迁入，§5.3-d′）。
- [x] ~~降级语义与裁定不符~~ ⇒ 已定为 §5.3-b。
- [x] ~~陈旧 registration~~ ⇒ 已定 §5.3-g。
- [x] ~~`name` 未校验~~ ⇒ 已定 §5.3-h。
- [x] ~~主仓名推导错~~ ⇒ 改用 `--git-common-dir`（§5.2）。
- [ ] **`SessionMeta.workspace` 未落地前，`--resume` 会静默落回主树**（P2 缓解）。
- [ ] **首次 attach 首轮 cache miss**（每树一份前缀）—— 已知代价，不可免。
- [ ] **测试纪律**：`create`/`remove` 会真调 git、真建目录 ⇒ 必须走 `crate::test_env::env_lock()` +
      `EnvRestore` 改 `YUSHAN_WORKTREES_DIR`，并在临时 git 仓内跑（`test_env.rs:1-40` 的既有约定）。
- [x] ~~同名仓库撞名~~ ⇒ 落点用**仓库的 HOME 相对路径**（逐分量作目录，§5.3-d），「不同路径、同名目录」不再撞。
- [ ] 建议用 `prototype` 验证：① worktree 内 `build_system_prompt` 的实际加载差异（`.yushan/` 缺席）；
      ② `attach` 与 `-p` / `--json` / `--resume` 的组合行为。

**已知限制（U5 收尾，各一行）**

- **会话内换工作区不重扫 skill 目录**（catalogue 在装配期冻结）：`/resume` 重建 agent 时不重新扫描 skill 根
  （`assembly::AgentFactory` 的 `catalogue` 是工作区无关量）；影响面仅限项目级 skill 段。
- **`SessionMeta.workspace` 只写不清**（`None` 不写）：`finalize_turn_meta` 仅在 `workspace` 为 `Some` 时写入；
  若将来允许「同一会话退回共享工作区」，旧记录会残留。
- **`SubagentSetup::model_cache` 每次重建即重置**（`shared()` 新建一份 `Arc<Mutex<HashMap>>`）：`/resume`
  换工作区重建时模型缓存清空，仅一次轻量重建，无正确性影响。

## 8. 相关文档

- [`context.md`](./context.md) — 上下文与约束
- [`review.md`](./review.md) — 质量分析（本版的修订来源）
- [`../../adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md) — 决策记录
- [`../../design-final/coding-agent-tui.md`](../../design-final/coding-agent-tui.md) §13 — subagent 派发与 workspace 继承的 as-built

## 9. 修订记录（相对 `review.md`）

| review 风险 | 级别 | 处置 |
|---|---|---|
| 1 注入点（`cwd` 同时喂提示词与 skill 根） | P0 | **§1.1/§1.2**：单量 `config.cwd` 注入（上移到 `Config::from_env` 之后、skill 扫描之前）；项目级 `.yushan/` 不随树迁入（§5.3-d′） |
| 2 降级语义与裁定不符 | P1 | **§5.3-b**：`attach` 回退并继续；`create`/`remove` 硬失败 |
| 3 陈旧 registration | P1 | **§5.3-g**：`create` 遇同名登记先 `prune` 重试，失败给指引 |
| 4 项目级 `.yushan/` 缺席 | P1 | **§5.3-d′**：**不建软链**；项目级 `.yushan/` 不随树迁入 ⇒ 树内不可见，`attach` 时主仓有则打缺失警告 |
| 5 `name` 未校验 | P2 | **§5.3-h**：`InvalidName` |
| 6 主仓名推导错 | P2 | **§5.2**：`--git-common-dir` 的父目录 |
| 7 `remove` 脏则拒删的 UX | P2 | **§5.3-c**：透传原因 + merge/`--force` 指引 |
| 8 测试会真建目录 | P2 | **§7**：`env_lock` + `EnvRestore` + 临时仓 |
| 9 术语未入 `CONTEXT.md` | P3 | 待定（设计尚为「提议」；落地后再登记） |
| — 追加：`create` 免起名 | — | **§5.3-i**：无参 ⇒ `auto_name()`（复用 `new_session_id` 形态）；`attach` 无参 ⇒ 列表选取 |
| — 追加：创建/退出各打印一次 | — | **§5.3-j**：`create` 打 `worktree: <name>` 到 stderr（同 `session: <id>`，ADR-0032）；退出行追加同名行 |
| — 追加带来的简化 | — | ~~工作区运行期不变 ⇒ 退出行不需要 `SessionPathHandle` 式快照~~（**U4 推翻**：见 §5.3-j 尾注 / §5.3-k） |
| — 取消软链（用户裁定） | — | **§5.3-d′**：删除软链机制（含 `ignore_own_artifact` / `extensions.worktreeConfig` / `core.excludesFile`）；项目级 `.yushan/` 树内不可见，改由 `attach` 打缺失警告 |
| — 追加：落点改 HOME 相对路径前缀 | — | **§5.3-d**：`$HOME/.yushan/worktrees/<仓库的 HOME 相对路径，逐分量作目录>/<name>`（消歧同名目录） |
