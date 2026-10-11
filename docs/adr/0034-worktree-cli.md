# ADR-0034: worktree 走 CLI + 启动即绑定 —— 不做运行时切换、不引入工具

## 状态

提议（aligned with 用户三轮裁定；设计见 [`docs/arch/worktree/design.md`](../arch/worktree/design.md)）

## 上下文

并行 subagent 写同一棵树是本仓长期记录的**已知缺口**：ADR-0022 / ADR-0029 与
`docs/design-final/coding-agent-tui.md:629` 都写「**本仓无 branch/worktree、无沙箱** ⇒ 只能靠提示词
约束『写目标不相交』」。

用户为「补上 worktree」逐轮裁定，最终口径：

1. **一个需求 ⇒ 一个 worktree**（不是一子 agent 一个）；
2. **做成 CLI 子命令，支持 `create` / `attach` / `list` / `remove`**，可以直接套 git，
   **但不要让用户去找 git** —— 因为产物存在 **ys 自己的目录**下；
3. **`attach` = 启动即绑定**：启动（或 resume）一个会话，工作区就是该树；
4. 需求内多 agent 的写冲突**靠 skill 协商，运行时不做机制兜底**；
5. **只隔离不回流**；非 git / git 不可用 ⇒ **显式报错并指出退路**。

关键代码事实（决定本 ADR 的结论）：

- workspace 是**启动期单点量**：`apps/coding-agent/src/main.rs:1341-1342`
  `let cwd = config.cwd.clone(); let workspace = cwd.clone();`，此后四路扇出全从它派生
  （`toolset.rs:88` / `AgentBuilder::working_dir` / `prompt.rs:28` / `main.rs:1362`）。
- 工具**构造期捕获** workspace（`tools-basic/src/read.rs:48` 等）⇒ 工作区必须在建工具**之前**定死。
- `Agent` 冻结持有 `registry`/`cwd`/`system_prompt`（`ys-runtime/src/agent.rs:67-75`），
  `RuntimeContext.registry` 是**不可变借用**且 `ToolRegistry` **不实现 `Clone`**
  （`ys-tool/src/registry.rs:5-8`）⇒ **中途换工具表需要改核心类型**。
- 参数解析是手写的 `parse_args`（`main.rs:161-222`），match 首 token。
- 全仓**零 git 调用**。

## 方案对比

设计阶段由并行 subagent 产出三个候选，**三者都假设「运行中途切换工作区」**：

| 方案 | 描述 | 优点 | 代价 |
|------|------|------|------|
| **A 最小复杂度** | `worktree.rs`+`assembly.rs`；`RebindHandle` 信元上卷；仓库外落点 | 0 新 trait、0 核心 crate 改动 | 有「同回合不生效」窗口 |
| **B 可扩展优先** | provider/ledger/service + `WorkspaceController` 挂载点 + 状态机 + TTL | 可换隔离后端；状态可查 | 穿 3 核心 crate；落仓库内 ⇒ 重复加载上下文 + 搜索遍历 |
| **C 资源/缓存效率** | 同 A 骨架但用挂载点；仓库内落点；显式成本账 | 成本核算细 | 缓存论断经核对不成立；落仓库内同 B 隐患 |
| **D（最终，用户裁定）** | **CLI 四动词 + 启动即绑定**；无工具、无命令、无切换 | **净新增最小；零核心改动；无 deferred 语义** | 无中途切换能力 |

## 决策

选择 **D：CLI 子命令组 + 启动即绑定**。

### 理由

1. **「启动即绑定」把「切换」从运行时问题变成装配问题。** 工作区在构造点（`main` 启动期）定死 ⇒
   四路扇出**原样不动**，只改取值来源。**A/B/C 的上卷机制（信元或挂载点）与 `assembly.rs` 重构整块消失。**
   ⚠️ 质量分析修订：取值来源**不是** `main.rs:1341-1342` 的 `workspace` 一行 —— `cwd` 同时喂
   「提示词」（`build_system_prompt(&cwd)`，:1352/:1375）与「skill 根」+「自定义 prompt」
   （`default_skill_roots(&config.cwd)`，:1227），只改 `workspace` 会让**工具在树里、提示词却说主树**。
   正确注入点是 `Config::from_env()` 之后、skill 扫描之前，**改写单一量 `config.cwd`**；项目级配置
   （skill 根 / `.yushan/SYSTEM.md`）在树内的一致性由 `create` 建的**软链 `.yushan`** 保证
   （design §1.1 / §5.3-d′），**不引入第二量**。
2. **它顺带删掉了 A/C 无法回避的「同回合不生效」问题** —— 不切换就没有生效延迟。
3. **它绕开 Claude Code 的两个已记录 bug**（#76250 会话级共享 CWD 槽、#33 切换是会话级副作用）——
   那两个 bug 都源于**中途切换工具**；本设计没有那个工具。
4. **CLI 而非工具/斜杠命令**（用户裁定）：产物在 ys 目录下，用户不该被迫认识 git；创建是进程级副作用，
   天然适合 CLI；覆盖面最广（用户 shell / 模型经 `bash` / skill / CI）。且消灭了「工具返回『下回合生效』」
   这个说不出实话的契约。
5. **`remove` 直套 `git worktree remove`**：**天然留分支、脏则拒删** —— 恰好是「只隔离不回流」想要的策略，
   免费获得。Codewhale 的 `is_managed_worktree` 安全闸以「有自动清理」为前提，本设计不引入 TTL/ledger ⇒
   无此前提（将来若加自动清理须补回）。
6. **落点仓库外** `$HOME/.yushan/worktrees/<仓库的 HOME 相对路径，`/`→`-`>-<name>/`
   （如 `~/Develop/YuShan` ⇒ `~/.yushan/worktrees/Develop-YuShan-<name>/`；仓库在 HOME 外 ⇒ 绝对路径变换）：
   ① 仓库内落点会被 `grep -r`/`find` 搜进完整副本
   （`EXCLUDED_DIRS`，`tools-basic/src/lib.rs:44` 不含 worktree 根）；② `load_project_context`
   （`prompt.rs:277-305`）逐级向上**全收** ⇒ 仓库内落点时 worktree 自己的 `AGENTS.md` 与主仓的会**两份都进
   上下文**（`tmp/pi` 的 `findShadowedContextFile` 专治此坑）；③ 不污染主树 `git status`。
   HOME 相对路径前缀另使「不同路径、同名目录」的两个仓库**不再撞名**。

7. **`create` 免起名 + 两处打印**（用户追加需求）。`create` 无参 ⇒ `auto_name()`，复用既有的
   `wiring::new_session_id()` 形态（`wiring.rs:235-240`，零新依赖、零碰撞）；`attach` 无参 ⇒ 从 `list`
   选取（照 `--resume` 无参 `Resume::Pick` 先例）。**不采纳可读名**（`bright-running-fox` 那类需要词表，
   本仓无先例；名字在可选取的前提下几乎不可见）。打印：`create` 成功打 `worktree: <name>` 到 **stderr**
   （与 **ADR-0032** 的 `session: <id>` 同侧同风格）；会话结束在既有退出行处追加同名行 + 「分支未合并」提示。
   ⚠️ **不需要新句柄** —— 工作区在启动期定死、运行期不变，而 `SessionPathHandle` 的存在理由是会话会在
   运行期被 `/new` 换掉（`app_loop.rs:91`）。

### 明确否决

- ❌ **`ToolContext` 第 4 挂载点**（B/C）：要么把字段穿 `ys-runtime::AgentPorts`→`ys-loop`→`ys-tool` 三个
  核心 crate（收益为零：工具终究无法重建 app 级 Agent），要么改 `RuntimeContext.registry` 为可变。
  「启动即绑定」后**连这个需求都不存在了**。
- ❌ **`worktree` 工具**：中途切换在今天的架构里**做不到「调用即生效」**，只能承诺「下一回合生效」——
  一个工具说得出却做不到的契约。且 `bash` 工具本就无限制，模型**今天就能** `git worktree add`。
- ❌ **中途切换（工具或斜杠命令）**：用户裁定 `attach` = 启动即绑定。
- ❌ **状态机 + `ledger.jsonl` + TTL**（B）：与「只隔离不回流」冲突（自动删会毁未 merge 的工作）；
  deepseek-harness 已论证「worktree 的创建/命名/清理是部署决策」。
- ❌ **仓库内落点**（B/C）：见上。
- ❌ **运行时做写冲突消解 / 锁**：deepseek-harness 的否决理由直接适用 ——「路径前缀重叠无法证明语义独立，
  **假互斥比显式警告更危险**」；用户也裁定下放给 skill。

## 后果

**正面**

- 补上 ADR-0022/0029 的已知缺口：需求之间、需求与主树之间有机制隔离（同需求内仍是 skill 协商）。
- **净新增最小**：一个 `worktree.rs` + `parse_args` 一个 match 臂 + `attach` 的单点注入
  （**一个 `worktree.rs` + `main.rs` 一处**；`config.rs` / `prompt.rs` 不动）；**零核心 crate 改动**。
- 结构上不可能出现 Claude Code 那两个并发 bug（无中途切换、无会话级 CWD 副作用）。
- 覆盖面最广：用户、模型（经 `bash`）、skill、CI 同一套入口；`list`/`remove` 有 git 背书。

**负面（诚实记录）**

- **没有任何中途切换能力**：同一会话要换需求 = 重开（这被视为反模式：一个会话混两个需求）。
- **session ↔ worktree 无绑定**：`attach` 起的会话不记录工作区 ⇒ 裸 `--resume` 会落回主树。需 P2 的
  `SessionMeta.workspace` 只读记录来**提示**（非自动切换）。
- **项目级 `.yushan/`（skills / SYSTEM.md）在树内缺席**（未跟踪文件不被 checkout）—— 已由 `create` 建的
  **软链 `<树>/.yushan` → 主仓 `.yushan`** 解决（design §5.3-d′）：项目级配置在树内**照常可见**。
  非 worktree 路径下无软链、`cwd` 原样 ⇒ 行为逐字节不变。
- **首次 attach 首轮 prompt cache miss**（cwd 与 `project_instructions` 路径都进提示词文本）。
- **清理成本转嫁给用户**：不自动回收 ⇒ 须把落点约定文档化，并明确「分支永不自动删」。
- `list`/`remove` 是薄包装 ⇒ git 版本过低/行为差异时，错误信息来自 git。

## 质量分析后的修订（`docs/arch/worktree/review.md`）

| review 风险 | 处置 |
|---|---|
| P0 注入点（`cwd` 同时喂提示词与 skill 根） | **单量 `config.cwd`**；注入点上移至 `Config::from_env()` 之后、skill 扫描之前；一致性靠软链 |
| P1 降级语义与用户裁定不符 | **`attach` 响亮警告 + 回退共享工作区继续启动**；`create`/`remove` 是显式动作 ⇒ 硬失败 |
| P1 陈旧 registration（`rm -rf` 后 git 仍登记） | `create` 遇同名登记先 `git worktree prune` 重试，失败给指引 |
| P1 项目级 `.yushan/` 缺席 | `create` 建软链 `<树>/.yushan` → 主仓 `.yushan`（design §5.3-d′） |
| P2 `name` 未校验（路径穿越） | 新增 `InvalidName`：非空、无 `/`/`\`、非 `.`/`..` |
| P2 主仓名推导错 | 用 `git rev-parse --git-common-dir` 的**父目录**，不用 `--show-toplevel` |
| P2 `remove` 脏则拒删的 UX | 透传 git 原因 + 「先 merge 或 `--force`」指引 |
| P2 测试会真建目录/真调 git | 走 `crate::test_env::env_lock()` + `EnvRestore`，临时 git 仓内跑 |

## 关联

- 设计：[`docs/arch/worktree/design.md`](../arch/worktree/design.md)
- 上下文：[`docs/arch/worktree/context.md`](../arch/worktree/context.md)
- 前序缺口：[ADR-0022](./0022-subagent-dispatch.md) / [ADR-0029](./0029-subagent-landing-decisions.md)
- 边界先例：[ADR-0021](./0021-plugin-seam-without-hook-system.md)（挂载点边界）
