# git worktree 支持 — 架构质量分析报告

## 分析范围

- **对象**：设计方案（`docs/arch/worktree/design.md` + `context.md` + `docs/adr/0034-worktree-cli.md`），
  对照 `apps/coding-agent` 现有代码基线（`main.rs` / `config.rs` / `toolset.rs` / `prompt.rs`）。
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性），重点在**错误处理、资源管理、故障模式**。
- **形态**：`ys-coding-agent worktree {create, attach, list, remove}`，`attach` = 启动即绑定；
  无中途切换 ⇒ 无工具、无命令、零核心改动。

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | 纯 CLI 薄包装 `git worktree`；本机 git 2.54，`add/list/remove` 齐备；零新 crate |
| 1.2 依赖成熟度 | 🟡 | **新增对系统 `git` 的运行时依赖**（全仓此前零 git 调用）；git 缺失需判为显式错误而非静默 |
| 2.1 模块边界 | 🟢 | 一个 `worktree.rs` + `parse_args` 一臂；**不改任何核心 crate**；扇出不动 |
| 2.2 接口稳定性 | 🟢 | CLI 表面是新增、不外泄内部类型；`WorktreeError` 仅 app 内部 |
| 2.3 错误处理 | 🟡 | 有 `WorktreeError` 五态，但**未定退出码、未定 git stderr 截断、未区分「git 不可执行」与「非零退出」** |
| 2.4 并发安全 | 🟢 | 无新增共享状态；工作区启动期定死，单进程 |
| 2.5 资源管理 | 🟡 | 不自动回收（有意）；但**用户 `rm -rf` 树会留下 git 陈旧 registration**，未定义处理 |
| 3.1 概念一致性 | 🟡 | 「需求」在系统里**没有载体**（实际只是「具名工作树」）；新术语未入 `docs/CONTEXT.md` |
| 3.2 上手成本 | 🟢 | 概念极少；四动词 + 一个落点规则 |
| 4.1 性能模型 | 🟢 | `create` 一次 `worktree add`（0.5–2s）；`attach` 只多一次 `rev-parse`；首轮 cache miss 已知 |
| 4.2 故障模式 | 🟡 | 降级语义与用户原裁定有出入（见风险 2） |

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|------|------|--------|--------|------|
| 1 | **`attach` 的注入点未定**：`cwd` 同时喂「提示词」与「skill 根」 | 高 | 高 | **P0** | 见改进建议·需讨论 |
| 2 | **非 git / 失败时是「报错」而非用户原裁定的「降级为共享工作区」** | 中 | 高 | P1 | `attach` 改为「响亮警告 + 回退共享工作区」 |
| 3 | **陈旧 registration**：用户 `rm -rf` 树 ⇒ git 仍记着 ⇒ 同名 `create` 失败 | 中 | 中 | P1 | `create` 前对同名做 `git worktree prune` 或明确提示 |
| 4 | **项目级 `.yushan/skills` 在树内缺席**（未跟踪文件不被 checkout） | 中 | 高 | P1 | 与风险 1 一并决策；或 `attach` 时同步 `.yushan/` |
| 5 | `name` 未校验 ⇒ 路径穿越（`../`）或意外落点 | 中 | 低 | P2 | 校验：非空、无 `/`、无 `..`、无路径分隔 |
| 6 | 在树内运行时仓库名推导错误：`rev-parse --show-toplevel` 给的是**树根** | 低 | 中 | P2 | 用 `--git-common-dir` 的父目录（或 `worktree list` 首条）取主仓名 |
| 7 | `remove` 因「脏」拒删，用户不知道为什么 | 低 | 高 | P2 | 透传 git 原因 + 提示「先 merge 或 `--force`」 |
| 8 | 测试会真建目录/真调 git ⇒ 污染与并行冲突 | 中 | 中 | P2 | 走 `crate::test_env::env_lock()` + `EnvRestore`（`test_env.rs` 明令） |
| 9 | 新术语未入 `docs/CONTEXT.md` | 低 | 高 | P3 | 补「工作树 / attach / 共享工作区」等条目 |

## 改进建议

### 易修复（低风险快速改进）

- **`name` 校验**：非空、不含 `/` 与 `..`、不含路径分隔符 —— 否则 `path_for` 可被 `../` 带出 ys 目录。
- **仓库名取主仓**：用 `git rev-parse --git-common-dir` 的父目录（`tmp/pi` 的 `dirname(commonGitDir)` 同款），
  而非 `--show-toplevel`（在树内运行时后者返回树根，会派生出 `YuShan-feat-<name>` 这种错名）。
- **错误面补全**：`WorktreeError` 映射到非零退出码；git stderr **截断后**再展示；
  区分「spawn 失败（未装 git）」⇒ `GitUnavailable` 与「非零退出」⇒ `Git(String)`。
- **测试纪律**：`create`/`remove` 的测试必须 `env_lock()` + `EnvRestore` 改 `YUSHAN_WORKTREES_DIR`，
  并在临时 git 仓内跑（`test_env.rs:1-40` 的既有约定）。
- **`list` 代价标注**：`dirty` 需要**每树一次** `git status --porcelain`；树多时是 N 次子进程，
  文档里写明，或按需 `--dirty` 开关。

### 需讨论（需要团队决策）

- **① `attach` 到底注入哪里？**（最高优先级）启动序列是：

  ```text
  config.rs:176   cwd = current_dir()
  main.rs:1227    skill_roots = default_skill_roots(&config.cwd)   ← skill 根取 config.cwd
  main.rs:1341    let cwd = config.cwd.clone();
  main.rs:1342    let workspace = cwd.clone();
  main.rs:1352    build_system_prompt(&cwd, …)                      ← 提示词取 cwd（不是 workspace！）
  main.rs:1375    build_system_prompt(&cwd, …)
  ```

  ⇒ **设计 §1 的「只改 workspace 一处」不成立**：只改 `workspace` 会让**工具在树里、提示词却说主树**
  （`Current working directory: <主树>`，项目上下文也从主树 walk-up）。两个候选注入点各有代价：

  | 注入点 | 工具在树 | 提示词 cwd | 项目级 skills | 结论 |
  |---|---|---|---|---|
  | 只改 `workspace`（设计原文） | ✅ | ❌ 主树 | ✅ 可见 | **不一致，不可取** |
  | 改 `config.cwd`（早期注入，1241 前后） | ✅ | ✅ 树 | ❌ **全消失** | 一致，但触发风险 4 |

  **建议**：改 `config.cwd`（保证语义自洽），**并且**在 `attach` 时把「项目级 `.yushan/` 未随树迁入、
  项目 skill 不可见」作为**响亮警告**输出 —— 或评估把 `<树>/.yushan` 指向主树（软链）以保住 skill。
  这是本轮唯一需要用户拍板的设计问题。

- **② 降级语义与用户原裁定的出入**：用户原裁定是「非 git / git 不可用 / add 失败 ⇒ **显式降级为共享工作区**」，
  而设计改成「**明确报错 + 指出退路**」。二者不等价：前者是 `attach` **自动回退**并继续启动会话，
  后者要用户**再敲一次不带子命令**。建议 `attach` 采纳「响亮警告 + 继续以共享工作区启动」，
  与裁定一致；`create`/`remove` 则保持硬失败（它们是显式动作，不该静默改语义）。

- **③ 陈旧 registration**：用户直接 `rm -rf ~/.yushan/worktrees/…` 是常见动作，git 仍持有登记 ⇒
  之后同名 `create` 报 `already registered`。建议 `create` 在失败前对该名做一次 `git worktree prune`
  重试，或至少在错误里直接给出 `git worktree prune` 的指引。

### 架构级（影响面大）

- **「需求」概念缺载体**：设计的隔离单位实际是「一个具名工作树」，「一个需求一个 worktree」目前是**约定**
  而非结构。若要让 `--resume` 不落回主树（当前是**静默错位**），需要 `SessionMeta.workspace`（只读、
  `#[serde_default]`）—— 设计已列 P2，建议**升为 P1**，因为它直接影响「隔离是否真的成立」的观感。
- **项目级 `.yushan/` 缺席是结构性的**：`git worktree add` 不搬未跟踪文件，这不是 bug 而是 git 语义。
  要么接受并警告，要么在 `attach` 时把主树的 `.yushan/` 软链进树 —— 后者会引入跨树共享配置的新语义
  （deepseek-harness 的 lefthook 笔记显示共享 common config 有坑），需谨慎。

## 总体评价

**整体健康，形态收敛得对。** 用「启动即绑定」把「中途切换」整块换掉，是本轮最正确的决定：它把
「工具/命令/挂载点/信元/`assembly` 重构」全部消除，**零核心 crate 改动**，并**结构上**绕开了 Claude Code
的两个已记录 bug（#76250 会话级共享 CWD 槽、#33 切换是会话级副作用）—— 没有切换，就没有那类副作用。

**最大风险不在技术可行性，而在一个被一句话盖过的耦合**：`cwd` 同时喂「提示词」与「skill 根」，
所以「工作区切到哪」不是一个自由参数，而是**必须同时决定提示词正确性与项目 skill 可见性**。
建议**先拍板风险 1（注入点）**，再动手；它决定 P1 的形状，也决定风险 4 的处置方式。

其余风险都是**增量可修**的工程量（name 校验、主仓名推导、退出码、prune 提示、测试 env 锁），
不构成架构级返工。

**推荐下一步**：① 定风险 1 与风险 2；② 按 design §6 的 P0（`worktree.rs` + 三个 git 薄包装）**先落地**，
它对上述决策不敏感；③ `attach` 的 P1 等决策定了再做。
