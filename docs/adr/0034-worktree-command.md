# ADR-0034: worktree 作为「需求级」隔离 —— 共享信元上卷、仓库外落点、只建不回收

## 状态

提议（aligned with 用户裁定；设计见 [`docs/arch/worktree/design.md`](../arch/worktree/design.md)）

## 上下文

并行 subagent 写同一棵树是本仓长期记录的**已知缺口**：ADR-0022 / ADR-0029 与
`docs/design-final/coding-agent-tui.md:629` 都写「**本仓无 branch/worktree、无沙箱** ⇒ 只能靠提示词
约束『写目标不相交』」。

用户为「补上 worktree」裁定如下范围（不可改）：

- **一个需求 ⇒ 一个 worktree**（不是一子 agent 一个）；
- **两条触发口共用一份实现**：`/worktree` 斜杠命令（用户）与 `worktree` 工具（模型，由 skill 正文指示）；
- 职责 **创建 + 切进当前会话**；
- **需求内多 agent 的写冲突靠 skill 协商，运行时不做机制兜底**（不做锁 / 不检测 / 不合并）；
- **只隔离不回流**（不自动 merge，分支保留）；
- 非 git / git 不可用 / `add` 失败 ⇒ **显式降级为共享工作区**。

约束（带指针）：`Agent` 冻结持有 `registry`/`cwd`/`system_prompt`
（`crates/ys-runtime/src/agent.rs:67-75`）⇒ **换工作区只能重建 Agent**；工具**构造期捕获** workspace
（`tools-basic/src/read.rs:48` 等）⇒ 重建即换工具表；`ys-core`/`ys-protocol` **零 tokio** ⇒ git 只能落
apps 层；`capabilities.rs` 绝不 print。

同类软件调研（详见 design.md §3）：deepseek-harness 明确否决「harness 自动建 worktree」与「把写范围
当锁」；Claude Code 的 `EnterWorktree` 有**两个已记录 bug**（#76250 会话级共享 CWD 槽导致并发子 agent
跨树误改；#33 切换是会话级副作用导致并行 agent 共享一树）。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|------|------|------|------|
| **A 最小复杂度** | `worktree.rs`+`assembly.rs`；`RebindHandle` 信元；仓库外落点；只建不回收 | 0 新 trait、0 核心 crate 改动、落点无坑 | 扩展性低（当前够用） |
| **B 可扩展优先** | provider/ledger/service 多模块 + `WorkspaceController` 挂载点 + 状态机 + TTL | 可换隔离后端；状态可查 | 穿 3 核心 crate；落仓库内 ⇒ 重复加载上下文 + 搜索遍历；TTL 与「不回流」冲突 |
| **C 资源/缓存效率** | 同 A 骨架但用挂载点；仓库内落点；显式成本账 | 成本核算细 | 缓存论断经核对**不成立**；落仓库内同 B 隐患 |

## 决策

选择 **A 的骨架**，并吸收 C 的成本纪律、B 的显式降级枚举。落地要素：

1. **两个新函数承载全部逻辑**：`provision()`（唯一 git 调用点）+ `assemble()`（收编
   `main.rs:1341-1375` 的四路扇出）。两入口共用这两个函数，**不新增 trait、不改核心 crate**。
2. **上卷 = `RebindHandle = Arc<Mutex<Option<PathBuf>>>`**（照 `app_loop::SessionPathHandle`,
   `app_loop.rs:91` 的先例）。**否决 `ToolContext` 第 4 挂载点**：工具在 apps 层、`provision` 是本地
   函数，加挂载点要把字段穿 3 个核心 crate 而**收益为零**（工具终究无法重建 app 级 Agent）。
   信元回合末 `take()` 自清（同 `BoundarySource` 的每回合新建纪律）。
3. **落点 = 仓库外** `$HOME/.yushan/worktrees/<repo>-<name>/`（可选 `YUSHAN_WORKTREES_DIR`）。
4. **生命周期 = 只建不回收、无 TTL**；分支永留。
5. **显式降级** `WorktreeActivation::{Isolated, Degraded(DegradeReason)}`。
6. **工具追加父表末位** ⇒ 子表仍是 9 元，**子 agent 结构上拿不到**。
7. **生效边界 = 下一回合**（构造期冻结 + 回合中 `&mut Agent` 已借出）。

### 理由（落点的三条独立论据，前两条是真 bug）

- **搜索遍历灾难**：`EXCLUDED_DIRS`（`tools-basic/src/lib.rs:44`）不含 worktree 根 ⇒ 仓库内落点会被
  `grep -r`/`find` 搜进完整副本。
- **重复加载上下文**：`load_project_context`（`prompt.rs:277-305`）逐级向上**全收** ⇒ 仓库内落点时
  worktree 自己的 `AGENTS.md` 与主仓的**两份都进上下文**（`tmp/pi` 专门修过此坑）。
- **主树零污染**：`.gitignore` 不含这些目录 ⇒ 仓库内落点污染 `git status`。

### 明确否决

- ❌ **`ToolContext` 第 4 挂载点**（B/C）：收益为零，成本是三个核心 crate 的字段穿透。
- ❌ **状态机 + `ledger.jsonl` + `TTL`**（B）：与「只隔离不回流」冲突 —— 自动删会毁未 merge 的工作；
  且 deepseek-harness 已论证「worktree 的创建/命名/清理是部署决策」。
- ❌ **仓库内落点**（B/C）：见上三条。
- ❌ **运行时做写冲突消解 / 锁**：deepseek-harness 的否决理由直接适用 ——「路径前缀重叠无法证明语义
  独立，**假互斥比显式警告更危险**」；裁定也要求下放给 skill。

## 后果

**正面**

- 补上 ADR-0022/0029 的已知缺口：需求之间、需求与主树之间**有机制隔离**（同需求内仍是 guideline 协商）。
- 零核心 crate 改动、零新 trait ⇒ 与「最小核心」原则一致，回归面小。
- **结构上绕开** Claude Code 的两个已知 bug：切换只在 app 单点、回合末生效（非会话级全局副作用）；
  子 agent 拿不到该工具（不存在并发子 agent 各自换树）。
- 复用 `BuildKit`（探测结果 / web / 模型缓存 / skill catalogue）⇒ 切换重建为 µs 级、git 仅 1 fork。

**负面（诚实记录）**

- **切换必然付一次 prompt cache miss**，且范围比「末尾 cwd 一行」大：`prompt.rs:60-64` 把路径写进
  `<project_instructions path=...>` ⇒ 稳定前缀到 `Guidelines` 为止，`project_context` 起每次切换首轮重计。
- **同回合内不生效**：工具调用后本回合其余写仍落旧树，靠文案与 skill 缓解，运行时不做兜底。
- **项目级 `.yushan/`（skills / SYSTEM.md）在 worktree 缺席**（未跟踪文件不被 checkout）⇒ 跨树行为漂移，
  须在切换输出明示。
- 多次切换不回收 ⇒ 磁盘增长（由用户显式清理，符合「不回流」）。
- worktree 共享 common repo 的 config/hooks ⇒ 将来若做 worktree 级 git 配置须走 `extensions.worktreeConfig`。

## 关联

- 设计：[`docs/arch/worktree/design.md`](../arch/worktree/design.md)
- 上下文：[`docs/arch/worktree/context.md`](../arch/worktree/context.md)
- 前序缺口：[ADR-0022](./0022-subagent-dispatch.md) / [ADR-0029](./0029-subagent-landing-decisions.md)
- 同类先例：[ADR-0021](./0021-plugin-seam-without-hook-system.md)（挂载点边界）
