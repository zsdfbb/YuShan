# git worktree 支持（`worktree` 子命令）— As-built

> 性质：**实际建成记录**。设计见 `docs/arch/worktree/design.md`（+ `context.md`），决策见
> `docs/adr/0034-worktree-cli.md`，质量分析见 `docs/arch/worktree/review.md`，计划见
> `docs/design-plans/completed/2026-10-11-worktree.md` 与 `docs/exec-plans/completed/2026-10-11-worktree.md`。
> 日期：2026-10-12 ｜ 基线 `555d7fe`。

## 一句话

`ys-coding-agent worktree {create, attach, list, remove}` —— 一个需求 = 一棵 git 工作树，
安置在 ys 自己的目录下（`~/.yushan/worktrees/`）。`list` / `remove` 是 `git worktree` 的薄包装；
**`create` = 建树 + 以新树为工作区启动**，**`attach` = 启动（或 resume）时绑定到既有树**
—— 两者共用同一注入点与同一套尾部 flag。无工具、无斜杠命令、无中途切换、无 IPC。

## 落点

```
apps/coding-agent/src/worktree.rs       唯一 git 调用点（全仓仅此模块 spawn git）
apps/coding-agent/src/assembly.rs       AgentFactory：启动与会话内重建共用的装配序列
apps/coding-agent/src/main.rs           WorktreeCmd / parse_worktree / resolve_attach_target /
                                        run_worktree_early / attach 注入 / --resume 恢复 / 退出行
apps/coding-agent/src/app_loop.rs       WorktreeBinding + WorktreeHandle + apply_resumed_workspace
crates/ys-session/src/meta.rs           SessionMeta.workspace: Option<PathBuf>（**唯一核心 crate 改动**）
apps/coding-agent/tests/worktree_cli.rs 子进程 e2e（真起 git 仓）
```

**落点是嵌套目录** `<worktrees_dir>/<仓库的 HOME 相对路径，逐分量作目录>/<name>/`
（`~/Develop/YuShan` + `feat` ⇒ `~/.yushan/worktrees/Develop/YuShan/feat/`，`worktree.rs:155-186`）。
逐分量**嵌套**（而非拼成单个目录名）使 `~/a/b` 与 `~/a-b` 落点不同、`list` 不互相串台；
`list` 用 `path.parent() == <base>` 精确比对筛出本仓子树下的条目（`worktree.rs:358-361`）。
`worktrees_dir` 由 `YUSHAN_WORKTREES_DIR` 覆盖，否则 `$HOME/.yushan/worktrees`，无 HOME 时回退到
**绝对**临时目录（相对路径会解析到 CWD = 用户仓库，把树建进仓内，`worktree.rs:107-125`）。
主仓根取 `git rev-parse --git-common-dir` 的**父目录**，**不是** `--show-toplevel`（后者在
linked worktree 内返回**树根**，派生落点会多一层错名，`worktree.rs:216-244`）。

`AgentFactory`（`assembly.rs:46-132`）冻结**工作区无关**的量（`WebConfig` / `RunLimits` / skill
`Catalogue` / `SubagentsDir` / 子预算），`build(workspace)` 只把工作区作入参，**启动路径与会话内
`/resume` 重建共用同一份序列**（防漂移）。工厂**无副作用**：`sweep_fetched` 与
`SubagentsDir::sweep_old` 仍只由 `main` 在启动期各调一次。`WorktreeHandle`
（`app_loop.rs:147-171`）是跨生命周期的工作区句柄，形态照 `SessionPathHandle`：`main` 启动期播种
（`main.rs:1759-1763`）、`app` 循环在 `/resume` 后刷新、`main` 在 `rt.shutdown_timeout` 之后读
（打退出行，`main.rs:1371-1373`）。

## 三个「工作区可变」时机

| 时机 | 触发 | 实际行为 |
|---|---|---|
| 启动期 `create` | `main` 在 `Config::from_env()` 后、skill 扫描前（`main.rs:2523-2560`） | 先 `worktree::create` 建树；成功 ⇒ **与 `attach` 同点注入**（`config.cwd` / `worktree_name` / `worktree_path`），告知行并入启动期提示；失败 ⇒ 硬失败（非零退出 + 原因 + 退路）。非终端环境（`tui_available()` 假）⇒ 打印告知后 `exit(0)`，不启 TUI |
| 启动期 `attach` | 同上（`main.rs:2561-2590`） | `resolve_attach_target` 解析树路径 ⇒ `config.cwd = tree`；其余启动路径逐字节不变（skill 根 / 提示词 / 工具三者都从 `config.cwd` 派生）。主仓有项目级 `.yushan/` 则向 stderr 打**缺失警告**（`worktree.rs:406-412`）。失败 ⇒ 响亮警告 + 保持共享工作区继续启动，不早退不 panic |
| 启动期 `--resume` 按 `SessionMeta.workspace` 恢复 | `main` 在 `create` / `attach` 之后、skill 扫描之前（`main.rs:2590-2630`） | 显式 `create` / `attach` 优先（与记录不同则说明）；无显式绑定且有记录且树仍在 ⇒ `config.cwd = recorded` 并告知「已恢复到工作树」；有记录但树已不在 ⇒ 告知「已不存在」后**按共享工作区启动**；无记录 ⇒ 什么都不做、不打任何东西 |
| 会话内 `/resume` 重建 Agent | `app_loop` 处理 `Request::ResumeSession`（`app_loop.rs:373-412`） | `apply_resumed_workspace`（`app_loop.rs:455-507`）按目标 meta 的 `workspace` 换工作区，**成功则 `factory.build` 一次性换掉工具表 + `working_dir` + system prompt + subagent**，并刷新 `config.cwd` 与工作区句柄 |

**会话内 `/resume` 的四情形**（`app_loop.rs:446-454`）：①记录存在、树仍在且与当前不同 ⇒ 重建到该树并告知；
②记录存在但树已不在 ⇒ 明确告知、**保持当前工作区**（不重建）；③无记录且当前在工作树 ⇒ 切回**共享工作区**
（`main.rs:1504-1507` 在 `attach` 改写 `config.cwd` **之前**捕获的进程启动目录：`shared_workspace`）并告知；
④无记录且已在共享 ⇒ 不动。

**「绝不取调用方 cwd」**：三个时机的目标工作区**只**来自 meta 记录、显式 `attach` 或共享工作区 ——
绝不回退成调用方当前 cwd（那会静默扩大可写根，`app_loop.rs:453-454` / `main.rs:1562-1563`）。

## 与设计的偏离（本轮真实发生）

1. **取消 `.yushan` 软链**（用户裁定）。设计稿让 `create` 在 `<树>/.yushan` 建一条指向主仓的软链；
   实测 git 2.54 下 linked worktree **不读** `<gitdir>/info/exclude`，软链会把树弄脏、`remove` 需
   `--force` 才能过。消解只有两条路 —— 改**用户仓库 common config**（`extensions.worktreeConfig` +
   `core.excludesFile`）或保留 `--force` 销毁路径 —— **两者皆否决**。实际形态：`create` **只做
   `git worktree add`，不建任何软链**（`worktree.rs:246-294`）；项目级 `.yushan/` 在树内**不可见**，
   `attach` 成功后若主仓有 `.yushan/`（目录存在即可）则向 stderr 打缺失警告（`main.rs:1532-1541`）。
2. **落点从扁平改嵌套**。设计计划的落点是单目录名 `<仓库路径，`/`→`-`>-<name>``
   （`~/Develop/YuShan` + `feat` ⇒ `Develop-YuShan-feat`）；实际改为逐分量嵌套
   （`Develop/YuShan/feat`，`worktree.rs:155-186`）。理由（深审 P2-1）：扁平拼接下 `~/a/b` 与 `~/a-b`
   会撞同一落点、`list` 串台。
3. **`--force` 分支整体删除**。设计稿在 `remove` 的脏则拒删路径上留 `--force` 退路；深审 P1-2 指出
   原守卫按**文件名字符串**判，在跟踪 `.yushan/` 的仓库会**误删真实改动**，故整块删除。实际形态：
   `remove` 只调 `git worktree remove <path>`，**绝不加任何强制选项**（`worktree.rs:377-391`）；
   git 的拒删原因原样透传，是否强制由用户在 `main.rs:1473-1476` 的退路提示后自行决定。
4. **「会话内 `/resume` 重建 Agent」被加入**（用户要求）。设计稿断言「工作区运行期不变、退出行不需要
   `SessionPathHandle` 式快照」—— **已被推翻**。故新增 `assembly.rs::AgentFactory` +
   `app_loop::WorktreeHandle` + `apply_resumed_workspace`（见上「三个时机」第三行）。
5. **`SessionMeta` 加了 `workspace: Option<PathBuf>`**（`meta.rs:53-58`，`#[serde(default)]`）。这是本轮
   **唯一触碰核心 crate**（`ys-session`）之处；`#[serde(default)]` 是回写兼容契约 —— 早期首行无此键
   ⇒ `None`，仍可反序列化（`meta.rs:79-99`）。回合末由 `finalize_turn_meta` 写穿（`main.rs:664-666`）。
6. **无参 `attach` 不是交互式列表选取**。设计稿把它类比 `--resume` 无参 `Resume::Pick`；实际是
   「**恰一棵**则自动用它，0 棵 / 多棵都报 `NoAttachTarget`（多棵时逐行列出候选）」
   （`resolve_attach_target`，`main.rs:1387-1416`）。`NoAttachTarget` 是 `WorktreeError` 的**第 6 个变体**
   （设计 §5.2 只列了 5 个），刻意不复用 `NotFound`（后者外壳会拼出无意义的 `create <提示>` 退路，
   `worktree.rs:50-54`）。
7. **`create` 建完即启动（U6）**。设计稿让 `create` 成功后提示「下一步：`worktree attach <name>`」
   再早退；实际改为**建树后直接以新树为工作区启动**。落点：`parse_worktree` 的 `create` / `attach`
   合并为 `parse_worktree_binding`（按动词名分派，共用 `parse_mode_tail` 与 `-` 开头守卫）；
   `main` 里 `Create` 臂与 `Attach` 臂同样注入 `config.cwd` / 名字 / 路径；`run_worktree_early`
   只剩 `List` / `Remove`。两个必要的配套：
   - **告知行并入启动期提示**（`worktree_created_note` → `with_worktree_note`）而不是直写 stderr
     —— 起屏前的 stderr 会在退出 alt-screen 后变成主屏残渣（§5 输出三分）。
   - **非终端环境不启 TUI**（`tui_available()`）：`enable_raw_mode()` 在管道 stdio 下必失败，
     CI / 脚本里的 `create` 不能因此不可用 ⇒ 打印告知行 + 一句「未启动 TUI」后 `exit(0)`。
     `-p` / `--json` 不看 TTY，照常真跑。

## 已知限制

- **会话内换工作区不重扫 skill 目录**：`factory.catalogue` 是工作区无关量，`/resume` 重建 agent 时
  **不重新扫描 skill 根**（`assembly.rs:49-50,96-98`）。影响面仅限项目级 skill 段。
- **`SessionMeta.workspace` 只写不清**（`None` 不写）：`finalize_turn_meta` 仅在 `workspace` 为 `Some`
  时写入（`main.rs:664-666` / `631-633`）；若将来允许「同一会话退回共享工作区」，旧记录会残留。
- **`SubagentSetup::model_cache` 每次重建即重置**：`shared()` 新建一份
  `Arc::new(Mutex::new(HashMap::new()))`（`subagent.rs:245-260`）；`/resume` 换工作区重建时模型缓存清空，
  仅一次轻量重建，无正确性影响。
- **`prune` 重试在当前 git 下不可成功**：失败的 `worktree add -b` 会**留下同名分支**（实测），而 `prune`
  不删分支 ⇒ 重试必撞「分支已存在」。故集成测只钉**可观测的 prune 副作用**（失败后陈旧登记已被
  `git worktree prune` 从 git 清除，`worktree_cli.rs:922-962`），谓词本身由
  `is_stale_registration_tracks_registered_but_missing_dir` 覆盖（`worktree.rs:632-723`）。判据按**状态**
  （登记在、目录不在）而非匹配 git 的 stderr 文案（文案随版本 / locale 变，`worktree.rs:296-314`）。
- **`worktree remove -p` 仍会把 `-p` 当名字**：`create` / `attach` 已改为「首 token 以 `-` 开头即视为无名」，
  故 `-p` 在那两个动词下是模式 flag（`parse_worktree_binding`，`main.rs:390-412`）；但 `remove` 臂未收紧
  （`main.rs:275-283`）—— `-p` 会通过 `validate_name` 被当成名字，落点不存在则 `remove` 失败。属已知缺口。
- **`create` 在非终端环境不启会话**：降级为纯建树（打印告知后 `exit(0)`），要进树得再跑
  `worktree attach <name>`。这是有意的 —— `enable_raw_mode()` 在管道 stdio 下必失败，不能因为
  「想顺手起会话」而让 CI / 脚本里的 `create` 不可用。`-p` / `--json` 不受此限（本就不起 TUI）。
- **首次 `attach` 首轮 prompt cache miss**：cwd 与 `<project_instructions path="…">` 都进提示词文本
  ⇒ 每棵树一份前缀（设计 §5.4，已知代价非缺陷）。
- **`create` 会真调 git、真建目录**：测试必须走 `crate::test_env::env_lock()` + `EnvRestore` 或
  **per-child env 注入**，并在临时 git 仓内跑（本仓此前零 git 调用）。

## 测试现状

- **全量**：`cargo test --workspace` ⇒ **1109 passed / 0 failed / 9 ignored**。
- `tests/worktree_cli.rs`：**19 用例**（子进程 e2e，真起临时 git 仓）—— N1 建树+分支、N2 免起名唯一、
  N3 非 git 硬失败、N4 非法名、N5 list、N6 remove 留分支/脏则拒删、F1 干净树直接 remove、N8 attach 降级
  不 panic、N9 退出行、A1 项目配置缺失警告、U3 四条（恢复到记录树 / 记录树失效 / 显式绑定优先）、
  U5-4a 陈旧登记 prune 副作用、U5-4b 树内取主仓根、U5-4c 无参 attach 的 0 棵 / 2 棵候选提示、
  U6 两条（`create -p` 真在新树里跑回合 —— 断言**请求体的系统提示**里 `Current working directory`
  就是新树；非终端 `create` 保持 headless 且不走到模型装配）。
  用例全部经 **per-child env 注入**（不碰父进程 env），并套 60s `CHILD_TIMEOUT`（挂起即失败）。
- `app_loop.rs`：**5 条会话内 `/resume` 工作区用例**（`resume_switches_to_recorded_workspace_consistently`
  / `..._missing_recorded_workspace_keeps_current` / `..._without_recorded_workspace_returns_to_shared`
  / `..._without_workspace_stays_in_shared_silently` / `..._same_recorded_workspace_is_a_noop`）。
- `worktree.rs`：**7 条内联单测**（`auto_name` 唯一性、`validate_name`、`path_for` 嵌套与 `a/b`≠`a-b`、
  `worktrees_dir` 绝对兜底、`attach_target`、`is_stale_registration`、错误 `Display`）。
- `assembly.rs`：1 条（同一工厂两个工作区 ⇒ 工具表恒 10 元而 `fixed_prompt_tokens` 随 cwd 变）。
- `meta.rs`：**1 条向后兼容用例**（无 `workspace` 键的旧首行仍可反序列化 ⇒ `None`）。

**测试基建修正**：`canonical()` 对**已删除**的路径不解析符号链接，加上 macOS `temp_dir()` 派生 `/var/…`
而 git 报告 `/private/var/…` ⇒ 两侧字面不等，曾使 prune 用例的 `is_stale_registration` 判据**空过**
（prune 不执行）。已统一：`Sandbox::new` 的 `base` 与相关单测的临时目录一律取 `canonical`
（`worktree_cli.rs:83-87` / `worktree.rs:635-644`）。

## 参考

- `docs/adr/0034-worktree-cli.md` — 决策记录（含 U4 修订）
- `docs/arch/worktree/{context,design,review}.md`
- `docs/design-plans/completed/2026-10-11-worktree.md`、`docs/exec-plans/completed/2026-10-11-worktree.md`
- `docs/design-final/runtime-resume.md` — `SessionPathHandle` 范式与启动期 `--resume` 的 as-built
