---
name: sequential-workflow
description: 端到端的功能开发工作流，覆盖从需求澄清到设计归档的 5 阶段 (Plan → Write → Execute → Finalize → Reflect)，所有实现通过 subagent 派发。Use when 用户提出完整功能开发任务、需要带设计与归档的端到端流程、或提到"顺序任务工作流""sequential workflow"。
---

# 顺序任务工作流

> 核心原则：orchestrator 只做协调/审阅/决策，所有代码修改通过 subagent 派发。

## 核心约束（不可违反）

```
1. SKILL 加载 → 立即 EnterPlanMode（物理只读，写不了代码）
2. Phase 1 未被放行前，禁止 ExitPlanMode 与进入后续阶段
   放行 = 用户签字，或 C.3 自动放行判据六条全绿（唯一例外，见约束 6）
3. 所有 subagent 由 orchestrator 通过 Agent 工具派发，永不自己写业务代码
4. 每个任务单元必须经过：实现 subagent → review subagent（独立），review 不通过重派
5. Phase 5 回 plan mode 核对需求完成度
6. 自动放行只覆盖 Phase 1 → Phase 2。不可逆操作（migration / 删除 / 破坏性 schema 变更）
   必须先建安全网（scripts/baseline.sh），建不了则停下问用户；对外发布动作永远先问用户
```

## 执行模型（严格状态机，非执行计划提纲）

> ⚠ **最重要的行为规则**：本 skill 是**严格状态机**，不是"计划提纲"。你不能一次性输出所有阶段的执行步骤然后自行按步骤执行。必须按阶段顺序，每次只做当前阶段的事。

1. **当前阶段聚焦**：加载后你只处于 Phase 1（Plan）。只做 Phase 1 的事——设计产物探测、核实、测试合同、放行判定。**不要在 Phase 1 输出后续阶段的执行步骤**。
2. **阶段顺序不可跳跃**：Phase 1 → Phase 2(Write) → Phase 3(Execute) → Phase 4(Finalize) → Phase 5(Check)。入口条件是上一阶段 100% 完成。不得跳跃、合并或省略阶段。
3. **波次计算属 Phase 3**：合并单元、划分波次、定 review 档都是 Phase 3 的动作，**禁止在 Phase 1 展开波次计划**（自动放行简报里只写一行摘要）。
4. **放行是唯一出口**：Phase 1 只有用户签字、或自动放行判据全绿，你才能 `ExitPlanMode`。否则继续在 plan mode 修改。**禁止在放行前进入 Phase 2 或执行任何代码。**
5. **不自作主张执行**：你只做协调/审阅/决策。**绝对不要**自己输出"接下来分 N 步走"然后按步执行——所有实现工作通过 Agent 工具派发 subagent。
6. **误判自检**：如果你发现自己在 Phase 1 输出了"执行计划"、"接下来分几步走"、"我的计划是..."等内容，立即停止，回到 Phase 1 的正确操作（设计产物探测、核实真实代码）。

## Quick start

1. **探**: `ls docs/arch/*/` — 有 design.md + review.md 则消费，无则从零设计
2. **核**: 逐条核实设计断言 vs 真实代码（文件存在？签名一致？依赖已声明？）
3. **签**: 判据全绿 → 自动放行简报 → 直接 ExitPlanMode；否则用户签字
4. **走**: 基线 → 合并/波次 → 并发实现 → 分档 review → Finalize
5. **查**: `EnterPlanMode` → Phase 5 逐条核对需求完成度

## Phase 1 — Plan（plan mode，探测 + 核实 + 放行判定）

**你当前处于 plan mode，只能读取文件、与用户对话、派 Explore agent 搜索信息。禁止写任何文件或跑非只读 bash 命令。**

**🚫 Phase 1 严禁行为**：
- ❌ 不要输出"执行计划"或"接下来分 N 步走"——那是 Phase 2-4 的事
- ❌ 不要一次性概括所有阶段的步骤——你只做 Phase 1
- ❌ 不要调用 Agent 工具做任何实现——那是 Phase 3 的事
- ❌ 不要因为探测到设计产物就跳过 Phase 1——它只把"设计"换成"核实"

**✅ Phase 1 的正确做法**：只做以下 5 步，做完一步做下一步：

1. **Step 0 设计产物探测**：`ls [ROOT]/docs/arch/*/`，读 `context.md` / `design.md` / `review.md`
   - 有 design.md + review.md → 走「设计已就绪」路径（第 2 步）
   - 只有 context.md → 建议先跑 `arch-design`
   - 无 → 走「从零设计」路径（Three-Pass Discovery，见 [REFERENCE.md#phase-1](REFERENCE.md#phase-1)）
2. **核实**（设计已就绪路径）：逐条核对 design.md 的断言与真实代码——文件真实存在、函数/接口签名一致、调用关系成立、依赖已声明、测试命令真能跑。偏差小→修正并记录；偏差大→回用户
3. **测试合同**（不得跳过）：逐条确认验收方法 + 优先级（必测 / 可推迟 / MANUAL_ACK_REQUIRED）
4. **推荐编排方案**：`full` / `quick` / `bugfix`
5. **放行判定**：按 [C.3 六条判据](REFERENCE.md#design-artifacts) —— 全绿则输出自动放行简报并**直接 `ExitPlanMode`**（不等用户回复）；任一 ❌ 则须用户签字后才 `ExitPlanMode`

**关键门禁**：未放行 → **什么都不发生**。继续在 plan mode 下讨论/修改。

详细设计指南 → [REFERENCE.md#phase-1](REFERENCE.md#phase-1)

---
> **Phase 1 已完成** → 已放行 → `ExitPlanMode` → 进入 Phase 2

## Phase 2 — Write（写 design-plan + exec-plan）

**你已退出 plan mode。只允许：运行 scripts 脚本 + 派 subagent 写文档。禁止自己 Write/Edit 业务代码。**

**🚫 Phase 2 严禁行为**：
- ❌ 不要自己 Write/Edit 业务代码
- ❌ 不要在 Phase 2 就开始实现（那是 Phase 3 的事）
- ❌ 不要跳过 subagent 派发——文档必须通过 Agent 工具派 subagent 写

仅 `full` 方案执行本阶段。`quick` / `bugfix` 方案跳过，设计内容直接在 subagent 提示中给出。

1. `Bash(pwd)` 拿 [ROOT] 绝对路径 → `bash scripts/init-workflow.sh [ROOT]`
2. 派 subagent 写 `docs/design-plans/<feat>.md` + `docs/exec-plans/<feat>.md`（每个 impl 任务对应一个 test 任务）
3. `bash scripts/verify-write.sh [ROOT]` 验证文件存在
4. **亲自**核对 exec-plan：每个 `type=impl` 有 `type=test` 配对；**每个任务都有「依赖」和具体「涉及文件」**（缺了 Phase 3 无法算波次）

详细 + 模板 → [REFERENCE.md#phase-2](REFERENCE.md#phase-2)

---
> **Phase 2 已完成** → 进入 Phase 3

## Phase 3 — Execute（基线 → 波次并发 → 分档 review）

**所有任务通过 Agent 工具派发 subagent 执行，禁止在当前上下文中直接实现。**

**🚫 Phase 3 严禁行为**：
- ❌ 不要在当前 agent 上下文中直接写代码——必须派发 subagent
- ❌ 不要跳过 review subagent——每个单元必须有独立 review
- ❌ 不要并发派发文件集有交集的单元——先合并
- ❌ 不要忽略测试——每个 impl 单元必须写测试并跑通

**3.0 基线**（任何 impl 之前，不可跳过）：

```
bash scripts/baseline.sh [ROOT] <slug>
→ BASELINE: <sha>              ← 正常, 记录该 ref
→ BASELINE + BACKUP: <branch>  ← 工作区原本不干净, 已提交基线 + 存备份分支
→ NONE: <reason>               ← 无法建安全网, 停下问用户
```

基线让 review 能 `git diff <baseline> -- <files>` 取**字节级**证据（含 git 中显示为 `??` 的新文件）。

**3.2 合并与波次**（先合并，再分层）：

```
R1. 两任务文件集有交集 → 合并为同一单元（并发写同一文件会互相覆盖）
R2. 同目录/同模块 且 ≤2 文件的小任务 → 合并
R3. 合并后单元 ≤5 文件 且 ≤10 分钟工作量
R4. impl 与其配对的 test 始终留在同一单元
波次: W0 = 无依赖单元; Wn = 依赖都在 W0..W(n-1); 同波文件集必须互不相交
```

**3.3 并发派发**（ready-set 调度，上限 3）：某单元完成 → `Read` 验证文件落盘（不信任返回消息）→ 重算 ready，
其下游立即可开工，不死等整波。失败隔离：只标记失败的单元，同波其他单元继续，其下游跳过留给 Phase 5。

**3.4 review 分档**：

```
deep   (独立 review subagent) — 核心路径 / 并发 / 资源 / 对外契约 / 安全 / >3 文件
        必须: 跑命令 + git diff 基线 + 测试合同逐条核对 + P0/P1/P2 分节
shallow (本波一个批量 subagent) — 其余
        只报 P0/P1; 禁止把措辞 / 风格 / 文档建议写进报告
```

每单元最多 3 轮 review；3 轮不通过 → 标记「阻塞」→ Phase 5 与用户讨论。

详细 → [REFERENCE.md#phase-3](REFERENCE.md#phase-3)

---
> **Phase 3 已完成** → 进入 Phase 4

## Phase 4 — Finalize（归档）

1. 派 subagent 更新 `docs/design-final/`
2. `bash scripts/archive.sh [ROOT] <slug>` 归档
3. 确认归档完成（如建过基线 commit，提示用户可 `git reset --soft <base>`）
   - 本阶段的验证只适用于由本 workflow 驱动的实现任务，不适用于用户临时插入的纯 git 请求

---
> **Phase 4 已完成** → 进入 Phase 5

## Phase 5 — Check（回 plan mode，核对需求完成度）

1. `EnterPlanMode`（回到只读模式）
2. 读所有 review 报告（deep 独立报告 + shallow 批量报告）
3. 跑全量测试确认通过（只适用于由本 workflow 驱动的实现任务；用户临时插入的纯 git 请求不适用）
4. 对照 Phase 1 的测试合同逐条确认
5. 输出状态报告（完成 / 有风险 / 跳过 / 阻塞）
6. 如有遗漏或失败 → 与用户讨论是否补充

## Plans 编排方案

预先定义的编排方案在 `plans/` 目录下：

| 方案 | 文件 | 适用场景 |
|------|------|----------|
| full | `plans/full.md` | 新功能/中型改动，需完整的 Write → Execute → Finalize |
| quick | `plans/quick.md` | 小改动/简单功能，跳过 Write 阶段 |
| bugfix | `plans/bugfix.md` | 已知 bug 修复，跳过 Write 阶段 |

**创建新方案**：如果默认方案都不适合，与用户协商确定流程后，新建 `plans/<name>.md` 文件。文件内容包含：适用场景描述 + 执行阶段说明。创建后回 Phase 1 步骤 4，由用户确认。

## Subagent 派发纪律

```
实现 subagent 派发：
  - 提示以 <execution_mode> 块开头（"执行模式，不是 plan mode"）
  - 包含任务描述、目标文件、设计上下文
  - 要求：Write 改代码 → Write 写测试 → Bash 跑测试 → Read 回读验证
  - 禁止写 plan 文件到 .yushan/plans/

review subagent 派发：
  - 提示以 <execution_mode> 块开头（"审查模式，只读，绝不修改文件"）
  - deep: 独立 agent, 跑命令 + git diff 基线 + P0/P1/P2 分节问题清单
  - shallow: 本波批量, 只报 P0/P1, 禁止措辞 / 风格级问题
  - 绝不修改任何业务文件
```

详细 + 完整模板 → [REFERENCE.md#templates](REFERENCE.md#templates)

## Scripts

| 脚本 | 用途 | 何时用 |
|------|------|--------|
| `scripts/baseline.sh` | 建立字节级基线 / 备份分支 | Phase 3.0（不可跳过） |
| `scripts/init-workflow.sh` | 创建 docs/ 目录结构 | Phase 2 |
| `scripts/verify-write.sh` | 检查文件存在且非空 | 每次派 subagent 写文件后 |
| `scripts/archive.sh` | 归档 design/exec-plan | Phase 4 |

维护本 skill 自身时的检查清单见 [REFERENCE.md#skill-checklist](REFERENCE.md#skill-checklist)。
