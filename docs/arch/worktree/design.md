# git worktree 支持 — 架构设计

> 上承 [`context.md`](./context.md)。决策记录见 [`docs/adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md)。
> 质量分析见 [`review.md`](./review.md)（本版已按其实施修订，见 §9 修订记录）。
> §4 记录了并行 subagent 产出的三个候选方案；**最终形态比三者都小**（见 §1 与 §4.3）。

## 1. 结论先行

**CLI 子命令组 `ys-coding-agent worktree {create, attach, list, remove}`；工作区只在进程启动时选定一次。**

| 动词 | 性质 | 实现 |
|---|---|---|
| `create <name>` | git 包装 | `git worktree add -b worktree-<name> <ys目录>/<仓库的 HOME 相对路径>-<name> HEAD` |
| `list` | git 包装 | `git worktree list --porcelain`，筛出 ys 目录下的条目 |
| `remove <name>` | git 包装 | `git worktree remove <path>`（**天然留分支、脏则拒删**） |
| **`attach <name>`** | **ys 特有** | **启动（或 resume）一个会话，工作区绑定到该树** —— git 里没有对应物 |

⇒ **无工具、无斜杠命令、无中途切换、无 IPC、零核心 crate 改动。**

### 1.1 注入点仍是 `cwd`，一致性靠软链（本版新增，修订 review 的 P0）

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
全部消失**（树里没有未跟踪文件）。二者都不可接受，故注入点必须在 **skill 扫描之前**（§1.2）。

| 消费者 | 取值来源 |
|---|---|
| ① skill 根（`default_skill_roots(&config.cwd)`） | `config.cwd` |
| ② 提示词（`Current working directory` / `find_custom_prompt`） | `config.cwd` |
| ③ 工具（`build_tools(&workspace)` / `working_dir`） | `config.cwd` 派生的 `workspace` |

**三者都由同一个 `cwd` 派生**，**不引入「项目根」第二量**：`create` 建树时在 `<树>/.yushan` 处建一条
指向主仓 `.yushan` 的**软链**（§5.3-d′），于是「skill 根 / `.yushan/SYSTEM.md` / 提示词 cwd」在树内天然自洽 ——
工作区、提示词、项目级配置**三者的一致靠软链保证**，而非靠拆量。
非 worktree 路径下无软链、`cwd` 原样 ⇒ **行为逐字节不变**。

### 1.2 改动的真实位置（不是「一行」）

```rust
// main.rs：紧接 Config::from_env()、且在 skill 扫描（:1224）之前
if let Some(WorktreeCmd::Attach(name)) = &args.worktree {
    match worktree::attach_target(&config.cwd, name) {
        Ok(tree) => config.cwd = tree,          // 工作区 + 提示词 + skill 根 全部跟随
        Err(e) => eprintln!("{警告文案}"),       // 响亮警告，保持 cwd 不变，继续共享工作区启动
    }
}
```

只需改写 `config.cwd` 一处（§1.1）：三个消费者都从它派生，项目级配置的一致性由 `create` 建的软链（§5.3-d′）保证。
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
  `cwd/.yushan/SYSTEM.md`（`prompt.rs:323-327`）⇒ 两者都跟随同一个 `cwd`（§1.1；树内可见性由软链保证）。
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
ys-coding-agent worktree attach [<name>]   # 启动/resume 会话，工作区 = 该树（可带 -p / --json / --resume）；unspecified ⇒ 列表选取
ys-coding-agent worktree list              # 列出 ys 目录下的工作树（含路径 / 分支 / 是否脏）
ys-coding-agent worktree remove <name>     # 删树：直套 git worktree remove（留分支、脏则拒删）
```

`attach` **要求树已存在**（否则见 §5.3-b 的降级）。`create` / `attach` 分离是有意的：创建与绑定是两个动作。
**两个动词的名参都可省**（§5.3-i/§5.3-j）。

### 5.2 模块与接口

```rust
// 新：apps/coding-agent/src/worktree.rs —— 唯一 git 调用点
pub fn dir() -> PathBuf;                            // ~/.yushan/worktrees/（YUSHAN_WORKTREES_DIR 覆盖）
pub fn main_repo(cwd: &Path) -> Result<RepoInfo, WorktreeError>;  // ← 主仓根 + 名
pub fn path_for(repo: &RepoInfo, name: &str) -> Result<PathBuf, WorktreeError>;  // 校验后按「仓库的 HOME 相对路径」拼接
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
**不是** `--show-toplevel` —— 后者在树内运行时返回的是**树根**，会派生出 `YuShan-feat-<name>` 这种错名。
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

**d. 落点仓库外** `$HOME/.yushan/worktrees/<仓库的 HOME 相对路径，`/`→`-`>-<name>/`（如主仓在
`~/Develop/YuShan` ⇒ `~/.yushan/worktrees/Develop-YuShan-<name>/`）；仓库在 HOME 以外 ⇒ 用绝对路径变换（罕见）。
用「仓库的 HOME 相对路径」而非裸仓库名，是为了让**不同路径、同名目录**的两个仓库不相撞（§7）。
三条理由：① `EXCLUDED_DIRS`
（`tools-basic/src/lib.rs:44`）不含 worktree 根 ⇒ 仓库内落点会被 `grep -r`/`find` 搜进完整副本；
② `load_project_context` 逐级向上**全收**（`prompt.rs:277-305`）⇒ 仓库内落点时 worktree 自己的
`AGENTS.md` 与主仓的会**两份都进上下文**（`tmp/pi` 专门修过）；③ 不污染主树 `git status`。

**d′. 软链 `.yushan` 取代「项目根」概念**（用户裁定）。`create` 在建树后于 `<树>/.yushan` 处建一条指向
主仓 `.yushan` 的**软链**（主仓无 `.yushan` 则跳过）⇒ 项目级 skill / `.yushan/SYSTEM.md` / 提示词 cwd
**三者在树内天然自洽**，无需拆出第二量。理由：实测 `build_system_prompt` 有 **20 个调用点**，
改签名会牵动全部 ⇒ 软链方案**零签名改动**（`config.rs` / `prompt.rs` 完全不动）。

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

**j. 两处打印，各按既有去处。**

- **`create` 成功 ⇒ 打 `worktree: <name>` 到 stderr**（+ 路径 / 分支 / 「下一步：`attach <name>`」两行）。
  与 `session: <id>`（**ADR-0032**，同落 stderr）**同侧同风格**：人读诊断归 stderr，stdout 留给 `list` 的表格。
- **会话结束 ⇒ 在既有退出行处追加 `worktree: <name>`**，并提示「分支 `worktree-<name>` **未合并**；
  `git merge worktree-<name>` 或 `worktree remove <name>`」。复用 ADR-0032 的三模式统一出口
  （TUI 在 `main.rs:1191` 退出回放之后；`-p`/`--json` 在 `persist_single_turn`），文案与 `session_exit_line`
  （`main.rs:551`）并列。

> **⚠️ 不需要新句柄。** ADR-0032 的 `SessionPathHandle`（`app_loop.rs:91`）之所以需要，
> 是因为**会话会在运行期变**（`/new` 换文件）。而**工作区在启动期定死、运行期不变**（§1.2）⇒
> `main` 从 `args.worktree` / `config` 直接取值即可，**退出行不需要跨生命周期快照**。
> 这是「启动即绑定」在实现上带来的第二处简化。

## 6. 分阶段落地

- **P0**：`worktree.rs`（含 §5.3-g/h/i 与 §5.2 的主仓根取法）+ `parse_args` 加 `worktree` 子命令与
  **早期分流早退**；`create`（含无参自动命名）/ `list` / `remove` 三个 git 薄包装 + `WorktreeError` 四态
  + `create` 的 `worktree: <name>` stderr 打印（§5.3-j）。
- **P1**：`attach`（含无参列表选取）+ §1.2 的单点注入（`main.rs` 一处改写 `config.cwd`）+ §5.3-d′ 的软链 `.yushan`
  ⇒ 工具 / 提示词 / 项目配置三者随软链自洽。
  含 §5.3-b 的降级回退、主仓无 `.yushan/` 时软链缺席的**响亮警告**，以及**退出行追加 `worktree: <name>`**（§5.3-j）。
- **P2**：`SessionMeta.workspace` 只读记录（`#[serde(default)]`）⇒ `--resume` 提示工作区错位
  （**升为 P1 级重要性**：不做则 `--resume` 静默落回主树）。
- **P3（可延后）**：`attach --create-if-missing`；`/status` 显示当前是否在树内；`list --dirty` 开关。

## 7. 已知风险与待验证项

- [x] ~~`attach` 的注入点未定~~ ⇒ 已定为 §1.1/§1.2 的单量注入（改写 `config.cwd` 一处，一致性靠软链）。
- [x] ~~降级语义与裁定不符~~ ⇒ 已定为 §5.3-b。
- [x] ~~陈旧 registration~~ ⇒ 已定 §5.3-g。
- [x] ~~`name` 未校验~~ ⇒ 已定 §5.3-h。
- [x] ~~主仓名推导错~~ ⇒ 改用 `--git-common-dir`（§5.2）。
- [ ] **`SessionMeta.workspace` 未落地前，`--resume` 会静默落回主树**（P2 缓解）。
- [ ] **首次 attach 首轮 cache miss**（每树一份前缀）—— 已知代价，不可免。
- [ ] **测试纪律**：`create`/`remove` 会真调 git、真建目录 ⇒ 必须走 `crate::test_env::env_lock()` +
      `EnvRestore` 改 `YUSHAN_WORKTREES_DIR`，并在临时 git 仓内跑（`test_env.rs:1-40` 的既有约定）。
- [x] ~~同名仓库撞名~~ ⇒ 落点用**仓库的 HOME 相对路径**前缀（§5.3-d），「不同路径、同名目录」不再撞。
- [ ] 建议用 `prototype` 验证：① worktree 内 `build_system_prompt` 的实际加载差异（`.yushan/` 缺席）；
      ② `attach` 与 `-p` / `--json` / `--resume` 的组合行为。

## 8. 相关文档

- [`context.md`](./context.md) — 上下文与约束
- [`review.md`](./review.md) — 质量分析（本版的修订来源）
- [`../../adr/0034-worktree-cli.md`](../../adr/0034-worktree-cli.md) — 决策记录
- [`../../design-final/coding-agent-tui.md`](../../design-final/coding-agent-tui.md) §13 — subagent 派发与 workspace 继承的 as-built

## 9. 修订记录（相对 `review.md`）

| review 风险 | 级别 | 处置 |
|---|---|---|
| 1 注入点（`cwd` 同时喂提示词与 skill 根） | P0 | **§1.1/§1.2**：单量 `config.cwd` 注入（上移到 `Config::from_env` 之后、skill 扫描之前）；一致性靠软链（§5.3-d′） |
| 2 降级语义与裁定不符 | P1 | **§5.3-b**：`attach` 回退并继续；`create`/`remove` 硬失败 |
| 3 陈旧 registration | P1 | **§5.3-g**：`create` 遇同名登记先 `prune` 重试，失败给指引 |
| 4 项目级 `.yushan/` 缺席 | P1 | **§5.3-d′**：`create` 建软链 `<树>/.yushan` → 主仓 `.yushan` ⇒ 项目 skill / 自定义 prompt **照常可见** |
| 5 `name` 未校验 | P2 | **§5.3-h**：`InvalidName` |
| 6 主仓名推导错 | P2 | **§5.2**：`--git-common-dir` 的父目录 |
| 7 `remove` 脏则拒删的 UX | P2 | **§5.3-c**：透传原因 + merge/`--force` 指引 |
| 8 测试会真建目录 | P2 | **§7**：`env_lock` + `EnvRestore` + 临时仓 |
| 9 术语未入 `CONTEXT.md` | P3 | 待定（设计尚为「提议」；落地后再登记） |
| — 追加：`create` 免起名 | — | **§5.3-i**：无参 ⇒ `auto_name()`（复用 `new_session_id` 形态）；`attach` 无参 ⇒ 列表选取 |
| — 追加：创建/退出各打印一次 | — | **§5.3-j**：`create` 打 `worktree: <name>` 到 stderr（同 `session: <id>`，ADR-0032）；退出行追加同名行 |
| — 追加带来的简化 | — | 工作区运行期不变 ⇒ **退出行不需要 `SessionPathHandle` 式快照**（§5.3-j 尾注） |
| — 追加：软链替代「项目根」 | — | **§5.3-d′**：`create` 建 `<树>/.yushan` → 主仓 `.yushan` 软链；零签名改动（`build_system_prompt` 20 个调用点不动） |
| — 追加：落点改 HOME 相对路径前缀 | — | **§5.3-d**：`$HOME/.yushan/worktrees/<仓库的 HOME 相对路径，`/`→`-`>-<name>/`（消歧同名目录） |
