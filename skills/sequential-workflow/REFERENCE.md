# sequential-workflow — REFERENCE

详细文档。SKILL.md 给"是什么 / 怎么走 / 看哪里"，这里给"每步具体怎么做"。

按需查阅：Phase 1-5 详细流程、跨阶段公约、模板字段含义、防挂策略、反模式表、完整速查。

---

## 跨阶段公约 A: Plan mode 兼容性

<a id="plan-mode"></a>

### 核心事实

plan 模式的限制会从主 agent **传染**给通过 `Agent` 派发的 subagent。subagent 会发现自己的 Write / Edit 工具被禁用，或把内容写到 `.yushan/plans/` 而非目标路径。

### 新架构下的规则

本技能的核心设计就是用 plan mode 作为物理保护：

1. **Skill 加载 → 立即 `EnterPlanMode`** — Phase 1 全程在 plan mode 下执行
2. Phase 1（plan mode）：只读探索、对话设计、协商测试合同 — 完全不需要写文件
3. **用户签字同意后 → `ExitPlanMode`** — 再进入 Phase 2-4 执行阶段
4. 执行阶段所有文件写入通过 subagent 完成（主 agent 不直接写代码）
5. **Phase 5 → 再 `EnterPlanMode`** — 只读检查需求完成度

### 为什么这个架构有效

- Phase 1 物理禁止写代码（plan mode 限制）
- 用户不签字就不 ExitPlanMode，强迫达成共识
- ExitPlanMode 后的执行阶段有明确的 SKILL.md 指令约束

### 检测信号（subagent 被 plan mode 传染）

如果某个 subagent 报告无法写文件或路径是 `.yushan/plans/`：

- **原因**：subagent 检测到 plan mode 限制（或误判为 plan mode）
- **处理**：主 agent 确认自己已 ExitPlanMode，重新派发 subagent，提示以 `<execution_mode>` 块开头

> **预防**：所有 subagent 派发提示**必须**以 `<execution_mode>` 块开头 — 它是从实战中提炼的"最重要补丁"。

---

## 跨阶段公约 B: 文件写入 subagent 标准模板

<a id="write-template"></a>

Phase 2 / 3.5 / 4.1 / 4.2 都需要派发 subagent 写文件。任何这类派发**必须**遵循以下模板，不再各自发挥。

### B.1 派发前的准备

- 用 `Bash(pwd)` 拿到项目根目录的绝对路径，记为 `[ROOT]`
- 一次性创建目录：`bash scripts/init-workflow.sh [ROOT]`
- 所有目标文件路径用 `[ROOT]/docs/...` 的形式（绝对路径），不要用相对路径
- 把 markdown 内容准备好，作为 subagent 提示中的 `content` 块

### B.2 subagent 提示模板

```
Agent(
  description: "<write-task-slug>",
  subagent_type: "general-purpose",
  prompt: |
    用 Write 工具创建文件 [ROOT]<绝对路径>
    参数:
      - file_path = [ROOT]<绝对路径>
      - content = [以下 markdown 块]

    <content>
    [完整的 markdown 内容]
    </content>

    强制要求:
    1. **必须**真正调用 Write 工具 — 不要把 markdown 当作回复文本返回
    2. 调用后, 用 Read 工具回读一次, 确认内容完整
    3. 在你的回复里报告 Write 是否成功、文件大小

    > **退化分支**: 如果 Write 不可用 (被 plan 模式限制), subagent 明确报告
    > 'Write tool unavailable due to plan mode' 并返回完整 markdown 内容。
    > orchestrator 会重新派发 subagent, 提示以 `<execution_mode>` 块开头。

)
```

### B.3 Orchestrator 验证流程 (派发后必做)

直接用脚本验证（替代手写 Read）：

```bash
bash scripts/verify-write.sh [ROOT] \
  docs/design-plans/<feat>.md \
  docs/exec-plans/<feat>.md
# 退出 0 = 全部 OK, 退出 1 = 任一 MISSING
```

如缺失，按以下分支处理：

```
判定分支:
  a. 全部存在且内容正确 → TaskUpdate 推进
  b. 文件不存在 + subagent 报告 'plan mode 限制' → 重新派发, 提示以 `<execution_mode>` 块开头, 显式声明"执行模式"
  c. 文件不存在 + subagent 没报告限制 → 重新派发, 提示中追加:
     '上次你没有调用 Write 工具, 只是返回了文本。这次必须真的调用 Write 工具。'
  d. 反复失败 (≥ 3 次) → 在 Phase 5 反思里记录 'subagent 文件写入反复失败'
```

> **原则**：永远不要相信 subagent 的返回消息。它描述的是意图，不是实际完成状态。脚本验证是唯一的真相来源 — 这与项目级约定一致（subagent 派发后用 Read 验证实际状态）。

---

## 跨阶段公约 C: 设计产物消费与自动放行

<a id="design-artifacts"></a>

`arch-explore` / `arch-design` / `arch-validate` 会在 `docs/arch/<需求名>/` 下产出 `context.md` / `design.md` / `review.md`。若这些产物已存在，Phase 1 的工作从「重新设计」切换为「核实设计 + 自动放行判定」—— 把省下的时间投入对真实代码的核实。

### C.1 探测（Phase 1 第一步）

```bash
ls -d [ROOT]/docs/arch/*/ 2>/dev/null
```

对每个候选目录读 `context.md` / `design.md` / `review.md`，判定：

| 产物 | 路径 | 处置 |
|------|------|------|
| design.md + review.md 齐全 | 「设计已就绪」 | 走 C.2 核实；Phase 1 不再做 Three-Pass Discovery |
| 仅 context.md | 半成品 | 建议用户先跑 `arch-design`；或经用户同意后走「从零设计」路径 |
| 无 | 从零设计 | 保留原 Three-Pass Discovery 流程（见 [Phase 1](#phase-1)） |

**反误读**：探测到产物 ≠ 可以跳过 Phase 1。它只把 Phase 1 的**工作内容**从「设计」换成「核实」；阶段本身、以及后续 Phase 2-5 一步都不能省。

### C.2 设计核实清单（取代 Three-Pass Discovery）

逐条核对 design.md 的断言与真实代码。**读实现，不读注释、不读文档、不信任设计文档的转述。**

```
[ ] 提到的每个文件路径真实存在             — Read / ls 确认
[ ] 引用的函数/类/接口真实存在, 且签名一致  — grep 符号 → Read 定义, 比对参数与返回类型
[ ] 假设的调用关系成立                     — grep 调用方, 确认谁调谁
[ ] 描述的"现有行为"与实现一致             — 读实现验证
[ ] 新增文件路径不与现有文件冲突            — ls 目标路径
[ ] 引用的依赖已声明                       — package.json / Cargo.toml / go.mod / pyproject.toml
[ ] 测试策略在本项目可执行                  — 测试框架存在, 命令真能跑起来
```

任一不成立 → 记为「设计-现实偏差」：

- 偏差小（路径改名、签名微调）→ orchestrator 直接修正，写入简报的「偏差」一节
- 偏差大（推翻架构决策、暴露未考虑的约束）→ **回落到用户签字路径**，不得自行消化

产出：**核实后的涉及文件清单**（真实路径）+ **偏差清单**。

### C.3 自动放行判据

```
自动放行 checklist（全部 ✅ 才可跳过用户签字）:
[ ] docs/arch/<name>/design.md 存在
[ ] docs/arch/<name>/review.md 存在, 且无红灯、无未决项
[ ] C.2 核实全部通过, 无未解决偏差
[ ] 测试合同每条都有可自动化验收方法 (无 MANUAL_ACK_REQUIRED 阻塞项)
[ ] 不涉及不可逆操作 (无 migration / 删除 / 破坏性 schema 变更)
[ ] 编排方案 (full/quick/bugfix) 无歧义
```

**全绿 → 输出「自动放行简报」后直接 `ExitPlanMode`，不等待用户回复。**

简报格式：

```markdown
## 自动放行简报 — <feature>
- 设计来源: docs/arch/<name>/design.md (+ context.md / review.md)
- 核实结果: 7/7 通过 | 偏差: <无 / 逐条列出>
- 测试合同: N 条 (全部可自动化)
- 编排方案: full | quick | bugfix
- 波次计划: W0=[单元...] W1=[单元...]
- **跳过签字依据**: 上述 6 条判据全绿。如需干预请现在打断。
```

**任一 ❌ → 回落到用户签字路径**（[Phase 1](#phase-1) 步骤 7）。

### C.4 反滥用条款

- 自动放行**只覆盖 Phase 1 → Phase 2 的转换**。不免除 Phase 3 的不可逆操作门禁（见 [3.0](#phase-3-baseline)），不免除 Phase 5。
- 简报必须显式写出跳过签字的依据，让用户可事后审计、可打断。
- **若你发现自己用自动放行来回避 C.2 的设计核实本身 —— 立即停止，回到用户签字路径。**
- 任一判据说不清 → 按 ❌ 处理，走签字路径。宁慢勿错。

---

## Phase 1: Plan（需求澄清与设计）

<a id="phase-1"></a>

**执行模式**：plan mode（只读）。当前 agent 直接执行。纯对话、探索、设计，不写文件。

### 两条路径

Phase 1 第一步永远是 **Step 0 设计产物探测**（公约 C.1）：

- **检测到 design.md + review.md** → 走「设计已就绪」路径：Step 1' 核实（C.2）→ Step 7.5 测试合同 → Step 7.6 自动放行判据。**跳过下面的 Three-Pass Discovery。**
- **未检测到** → 走「从零设计」路径：下面的 Step 1-7 全流程，最后按 Step 7 由用户签字放行。

### Step 0: 设计产物探测

见 [公约 C.1](#design-artifacts)。先做这一步，再决定下面走哪条路。

### 核心方法：三轮文件发现 (Three-Pass Discovery)

> **仅用于「从零设计」路径。** 若已消费 arch-* 产物，本方法论由 C.2 核实清单取代。

```
Pass 1: Seed      → 从需求关键词出发，并行搜索所有匹配的源码、测试、配置
Pass 2: Expand    → 沿依赖链（import/引用）双向扩展，补全文件树
Pass 3: Verify    → 对照完整性清单确认无遗漏
```

最终产出完整的 **涉及文件清单**，供 Phase 2 填充 design-plan / exec-plan。

### 流程

#### Step 1: 收集需求与关键词提取

1.1 询问用户：
- 要解决什么问题 / 构建什么功能？
- 涉及哪些文件 / 模块？
- 约束、偏好、期望的验收标准？
- **测试策略**：
  - 是否有现有测试框架 / 测试规范？
  - 期望的测试方式（单元测试 / 集成测试 / E2E / 手动验证）？
  - 是否需要 TDD（测试先行）？
  - 验收标准中哪些可自动化验证？

1.2 **提取搜索关键词**（在对话中同步整理，不必额外提问）：
- **功能/模块名称**: 中文名、英文名、驼峰、下划线、短横线各变体
- **实体/数据模型名称**: 涉及的 domain model、DTO、schema 名称
- **行为动词**: 验收标准中可映射到代码动作的关键词（如「导出」「同步」「验证」）
- **技术栈关键词**: 框架名、库名、协议名
- **已有线索**: 用户提到的具体文件路径、模块名、函数名

#### Step 2: Seed Pass — 并行种子搜索

根据 Step 1 的关键词，**同时启动 2-3 个 Explore agent** 并行搜索。

每个 agent 使用以下通用的 **Search checklist** 作为指令模板：

```
根据关键词 `[关键词列表]`，在项目中搜索以下类别的文件：

1. **源码匹配**: `grep -rn` 关键词在源码中的出现位置；`find . -name '*关键词*'` 按文件名匹配
2. **测试文件**: 搜索 `test_*关键词*`、`*关键词*_test`、`*关键词*.spec.*`、`*关键词*.test.*`
3. **类型定义**: 搜索 schema / type / interface / model / dto / proto 等类型定义文件
4. **配置/注册**: 搜索配置文件（yaml/json/toml/env）、依赖注入注册、路由注册、中间件注册
5. **入口文件**: API handler、controller、CLI command、事件订阅者等入口文件
6. **数据库变更**: migration 文件、seed 文件、DDL 脚本
7. **文档**: 文件名或内容包含关键词的 markdown / adoc 文档

对每个找到的文件，标注：文件路径、匹配原因（关键词 / 文件模式 / 类型）。
```

**agent 分工建议**（非强制，视项目复杂度调整）：

| Agent | 搜索重点 | 方法 |
|-------|---------|------|
| Agent A: 源码匹配 | 关键词在源码中的直接出现 | grep / find / ripgrep |
| Agent B: 测试与配置 | 测试文件、配置文件、注册入口 | grep 测试文件模式、搜索 config/ 目录 |
| Agent C: 模式参考 | 相邻模块的类似实现、命名空间相近的文件 | 搜索同目录下的兄弟模块、搜索同级命名空间 |

**产出**: 初始文件列表，按「源码/测试/配置/文档」分类。

#### Step 3: Expand Pass — 依赖扩展

基于 Seed Pass 的文件列表，做双向依赖追踪：

3.1 **向上追踪**（找依赖）：对每个源文件，读取其 import / require / use 声明，找到被依赖的模块文件
3.2 **向下追踪**（找引用）：`grep -rn` 这些文件中的导出符号（函数名、类名、变量名），找到所有调用者
3.3 **补充覆盖**：将新发现的依赖文件加入清单，重复追踪直到不再发现新文件（或达到合理的深度，通常 2-3 层依赖链）

启动 1-2 个 Explore agent 执行上述追踪：

```
基于以下初始文件列表，做依赖扩展：
[初始文件列表]

对每个文件：
1. 读取 import/require/use 声明 → 找出被依赖模块的实际路径
2. grep 文件中导出的符号 → 找出项目中引用了这些符号的文件
3. 将新发现的文件加入清单，继续追踪（最多 2 层）
4. 标注新增文件的「引入原因」（如：被 A.py import / 引用了 B.py 的 exportXX 函数）
```

#### Step 4: Verify Pass — 完整性验证

```
完整性检查清单:
[ ] 用户提到的所有文件/模块已列入
[ ] 所有相关源文件已列入（内容含关键词 + 依赖链上的文件）
[ ] 对应的测试文件已列入（单元测试 + 集成测试 + E2E）
[ ] 类型定义 / schema / model 文件已列入
[ ] 配置文件 / 注册文件 / 路由文件已列入
[ ] 数据库 migration / seed 文件已列入
[ ] 相关文档已列入
[ ] 未引入无关联的无关文件
```

如有遗漏 → 补充搜索，标记为 found。如文件过多（>20）→ 与用户确认范围是否需要裁剪。

#### Step 5: 提出方案

给出 1-2 个可选方案，说明利弊和架构决策（数据流、接口变更）。「涉及文件」列表基于上述三轮发现的完整结果。

#### Step 6: 确认范围

与用户确认：设计思路、完整文件列表、关键决策、验收标准、潜在风险。

#### Step 7: 确定波及文档

列出 `docs/design-final/` 下描述受影响模块的最终设计文档。同时识别其他可能受影响的文档（README、API 文档、变更日志等）。

#### Step 7.5: 测试合同协商（必须完成，不得跳过）

在进入 Phase 2 之前，orchestrator 必须主动与用户起草并签字**测试合同**——这是 Phase 3 review subagent 的执行依据。

**强制动作**：

1. 把 Phase 1 收集到的需求逐条列出（用序号 N1, N2, N3...）

2. 对每条需求起草**一条验收方法**：
   - **自动验收**：用什么命令（pytest / curl / 日志 grep / node -e 等）、跑哪条命令、判通过的信号是什么（具体到 "返回 200" / "包含 'success'" / "passed 5/5"）
   - **手动验收**：需要看 / 点 / 体验什么、验收 checklist 长什么样

3. **优先级三档**（用于 review 不通过时的重派纪律）：
   - **必测** —— 核心路径、对外契约、复杂逻辑。review 不通过 = 必须重派到通过
   - **可推迟** —— 边缘情况、错误处理。review 不通过 = 报告里标注，由用户决定
   - **MANUAL_ACK_REQUIRED** —— 无法自动化（如 staging 真实告警）。由用户在 Phase 4 报告里人工勾选

4. 与用户逐项确认 / 修订 / 签字（"OK 这条就这么验"）

5. 把确认结果**原样**写入 design-plan「测试策略」节，格式见 2.2 design-plan 模板

**绝对禁止**：
- 跳过此步直接进入 Phase 2。即使是 1 行的小任务，也至少要 1 条消息向用户简述"我会用什么方式验证 X"，得到 OK 再走。
- subagent 自行决定验证方式——subagent 在 Phase 1 不参与协商（Phase 1 由 orchestrator 直接执行）。
- 验收方法用模糊语言（如"测试一下"）——必须具体到可执行的命令或操作。

> **「设计已就绪」路径的简化**：合同条目直接派生自 `context.md` 的「关键场景 / 异常场景」+ `review.md` 的改进建议，一条场景一条验收条款，不必从零起草。

#### Step 7.6: 自动放行判定

按 [公约 C.3](#design-artifacts) 的 6 条判据逐条判定：

- **全绿** → 输出「自动放行简报」→ 直接 `ExitPlanMode`，**不等待用户回复**
- **任一 ❌** → 回到 Step 7 的用户签字路径

> 从零设计路径（未消费 arch-* 产物）判据天然含 ❌（第 1 条不满足），必然走签字路径 —— 这是预期的。

#### Step 7: 用户签字（回落路径）

用户明确同意设计方案 + 测试合同 + 编排方案后，才 `ExitPlanMode`。

### 产出物

1. 达成共识的设计决策
2. **完整的涉及文件清单**（含路径、类型、引入关系），作为 Phase 2 的输入
3. **验证策略**（写入 design-plan 「测试策略」节）—— 经用户签字，或经 Step 7.6 自动放行判据确认

---

## Phase 2: Write（写入计划文档）

<a id="phase-2"></a>

> **进入 Phase 2 前**：确认已 `ExitPlanMode`。如仍在 plan 模式，先退出再派 subagent。

当前 agent 编排内容，然后通过 `Agent` 派发 subagent 统一写入所有文件。**严格遵循「跨阶段公约 B」的文件写入 subagent 标准模板**。

### 2.1 统一写入流程

派发前准备：
- `Bash(pwd)` 拿项目根绝对路径，记为 `[ROOT]`
- `bash scripts/init-workflow.sh [ROOT]` 创建目录
- 按 2.2 / 2.3 模板填好 design-plan / exec-plan 完整内容

派发 subagent（参照公约 B.2 模板，本 phase 专用实例）：

```
Agent(
  description: "write-plan-docs",
  subagent_type: "general-purpose",
  prompt: |
    用 Write 工具创建以下两个文件 (项目根为 [ROOT]):

    1. [ROOT]/docs/design-plans/YYYY-MM-DD-<feature>.md
       content = [完整的 design-plan markdown — 见 2.2 模板]

    2. [ROOT]/docs/exec-plans/YYYY-MM-DD-<feature>.md
       content = [完整的 exec-plan markdown — 见 2.3 模板]

    强制要求:
    - 必须**真正调用 Write 工具两次**, 不是返回 markdown 文本
    - 每次 Write 后用 Read 回读一次, 确认内容完整
    - 在回复中报告: 两个文件的 Write 是否都成功、每个文件的大小
    - 如果 Write 不可用 (被 plan mode 限制), 明确报告并返回完整 markdown
)
```

### 2.1.1 Orchestrator 验证 (派发后必做)

直接用脚本：

```bash
bash scripts/verify-write.sh [ROOT] \
  docs/design-plans/<feat>.md \
  docs/exec-plans/<feat>.md
```

判定分支按公约 B.3，缺失则按 a/b/c/d 退化处理。

### 2.2 design-plan 模板

`docs/design-plans/YYYY-MM-DD-<feature>.md`：

```markdown
# [功能名称] 设计计划

## 背景
- 当前状态 / 问题描述
- 为什么需要这个变更

## 设计
- 整体方案
- 架构决策（方案选择及理由）
- 数据流 / 接口变更

## 涉及文件
- 新增: `path/to/new.py`
- 修改: `path/to/existing.py`
- 删除: `path/to/old.py`

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。
> 合同由用户签字确认，或经 [公约 C.3](#design-artifacts) 的自动放行判据确认。

#### N1: [需求简述]
- **验收方法**: `[具体命令或操作]` — 期望 `[具体输出或现象]`
- **优先级**: 必测 / 可推迟 / MANUAL_ACK_REQUIRED
- **验收人**: review subagent / 用户

#### N2: [需求简述]
- **验收方法**: ...
- **优先级**: ...

### 测试框架与命令
- 单元测试: `pytest tests/...`
- 集成测试: `pytest tests/integration/...`
- 类型检查: `mypy path/...`
- Lint: `ruff check path/...`

## 波及文档
<!-- 本变更可能影响到的 docs/design-final/ 下最终设计文档。Phase 4 据此逐项确认更新。如无需更新已有文档则标记「无」 -->
- `docs/design-final/architecture.md`
- `docs/design-final/api.md`

## 风险与注意事项
```

### 2.3 exec-plan 模板

`docs/exec-plans/YYYY-MM-DD-<feature>.md`：

```markdown
# [功能名称] 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据** — Phase 3 据此合并执行单元并划分波次（见 [3.2](#phase-3)）。

## 上下文引用

参考设计计划：`docs/design-plans/YYYY-MM-DD-<feature>.md`

## 基线

- **基线 ref**: `<Phase 3 开始时由 scripts/baseline.sh 填写>`
- 用途: review 执行 `git diff <基线 ref> -- <files>` 取字节级证据（含 `??` 新文件）

## 任务清单

> **每个任务必须有 `type` 字段（`impl` 或 `test`）**。测试任务（`type=test`）是一等公民——它们是 Phase 3 必须创建、必须完成、必须被证据支持的独立任务，不是 impl 任务附带的"checkmark"。

### Task 1: [目录/模块名称] [type=impl]
- **涉及目录**: `path/to/dir/`
- **涉及文件**: `path/to/dir/file1.py`, `path/to/dir/file2.py`
- **描述**: 一句话
- **关联测试任务**: T2
- **依赖**: 无（无依赖 → 可与其他无依赖任务并发）

### Task 2: tests/ — [被测模块] 测试 [type=test]
- **涉及目录**: `tests/...`
- **涉及文件**: `tests/.../test_xxx.py`
- **描述**: 一句话（覆盖什么场景）
- **验证方法**:
  - `pytest tests/.../test_xxx.py -v` — 期望 N passed
  - `pytest --cov=path/to/module` — 期望 ≥ X%
  - `mypy path/to/` — 期望 No issues
  - `ruff check path/to/` — 期望 All checks passed
- **关联 impl**: T1
- **MANUAL_ACK_REQUIRED**（仅当 type=test 但需用户人工验收时填写，Phase 4 报告须原样保留、由用户勾选）:
  - [ ] 人工动作 1（例如：在 staging 触发失败部署，确认 Slack 收到告警）
  - [ ] 人工动作 2

### Task 3: [目录/模块名称] [type=impl]
- **涉及目录**: `path/to/dir/`
- **涉及文件**: `path/to/dir/file1.py`, `path/to/dir/file2.py`
- **描述**: 一句话
- **关联测试任务**: T4
- **依赖**: T1（无依赖写「无」—— 这决定能否并发）

### Task 4: tests/ — [被测模块] 测试 [type=test]
...

## 波次计划（Phase 3 计算后回填）

> Phase 3 开始时按 [3.2](#phase-3) 合并单元并分层，回填于此。示例：

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `src/a/`, `tests/a/` | 无 | W0 | deep |
| U2 | T3, T4 | `src/b/`, `tests/b/` | 无 | W0 | shallow |

（同波内文件集必须互不相交；相交则顺延到下一波）

## 验证清单
- [ ] **所有 (impl, test) 配对均已 completed**（Phase 3 review 放行条件）
- [ ] 运行 `pytest tests/related/` — 通过
- [ ] 运行 `npm test -- --related` — 通过
（具体命令从 Phase 1 验证策略协商结果抄录）
```

#### EXEC-PLAN VALIDATION（Phase 2 派发后必做）

派发 subagent 写入 exec-plan 后，orchestrator 必须**亲自**做以下检查（不依赖脚本）：

- [ ] 每个 `type=impl` 任务**有对应的 `type=test` 任务**（通过"关联任务"字段对应）
  - 例外：可标 `MANUAL_ACK_REQUIRED` 的 impl 任务无需 test 任务，但**必须在"验证方法"列写明人工判据**
- [ ] 每个 `type=test` 任务的"关联 impl"指向了存在的 impl 任务
- [ ] 无 `type=test` 任务被遗漏
- [ ] MANUAL_ACK_REQUIRED 项（如有）已写入"验证方法"列
- [ ] **每个任务都填了「依赖」字段**（无依赖必须显式写「无」）—— 缺了它 Phase 3 无法算波次
- [ ] **每个任务的「涉及文件」是具体文件路径**（不是目录笼统描述）—— 并发的冲突检测依赖它

任一不满足 → exec-plan 视为不通过 → 派 subagent 补齐或 orchestrator 亲自修，**不得**进入 Phase 3。

**拆分原则**：
- 按目录划分任务 — 同一目录下的所有文件归属一个 task，由一个 agent 独立完成
- **按依赖编号**；无依赖关系的 task 由 Phase 3 并发执行，有依赖的排到后续波次
- **`type=impl` 与 `type=test` 任务成对出现**，每个 impl 必须有至少一个 test 配对
- 不要为了"能并发"而拆得过细 —— Phase 3 会把小任务重新合并（[3.2 R2/R3](#phase-3)）
- 最后一个 task 通常是"运行测试套件确保全部通过"

完整填好的样例见 [EXAMPLES.md](EXAMPLES.md#design-plan) / [EXAMPLES.md#exec-plan](EXAMPLES.md#exec-plan)。

---

### 2.4 为什么必须先写 plan 才派 Phase 3 (subagent plan-first 防御)

Phase 3 的 subagent 收到"实现 X"这类多文件任务时, 有 reflex 倾向"先写 plan 再 execute" — 这是合理工程习惯, 但在 sequential-workflow 框架下会出问题 (subagent 把 plan 写到 `.yushan/plans/`, 而不是直接执行)。

**防御机制**: Phase 2 必须先把 `design-plan` 和 `exec-plan` 写到 `docs/`, **Phase 3 派 subagent 时 prompt 显式指明"读这两个文件作为唯一依据"**。这样 subagent 收到任务时已经有 Read 入口, 不会"自己规划"。

证据: 跑该 workflow 时, 不指明 plan 路径会导致 subagent 自己规划, 加上 `<execution_mode>` 块 + plan 路径后立即成功。

---

## Phase 3: Execute（基线 → 波次并发 → 分档 review）

<a id="phase-3"></a>

> **CRITICAL**: 每个任务单元 **MUST** 通过 Agent tool 派发独立 subagent（`subagent_type="general-purpose"`）执行。
> 禁止在当前 agent 上下文中直接实现任何任务。
>
> **Why subagent**: 干净上下文边界 → 防止上下文污染 → 每个单元聚焦单一目标 → 更容易 review。

### 3.0 基线与安全网（任何 impl 之前）

<a id="phase-3-baseline"></a>

```bash
bash scripts/baseline.sh [ROOT] <slug>
```

| 输出 | 含义 | orchestrator 动作 |
|------|------|------------------|
| `BASELINE: <sha>` | 基线已建立 | 记录该 ref，继续 |
| `BASELINE: <sha>` + `BACKUP: <branch>` | 工作区原本不干净；已提交基线 + 存备份分支 | 记录两个 ref，继续 |
| `NONE: <reason>` | 无法建立安全网 | **停下，问用户**，不得继续 |

基线 ref 的用途：review subagent 用 `git diff <baseline> -- <files>` 拿到**字节级**证据 — 包括 git 中显示为 `??` 的新文件。这直接解决 2026-09-17 反思中「未跟踪文件无法做字节基线比对」的问题。

**不可逆操作门禁**：若某单元涉及 migration / `rm` / `DROP` / 破坏性 schema 变更 / force push / 外部写调用，派发前**额外**备份目标（导出 schema、拷贝待删文件）。备份建不了 → 停下问用户。**对外发布动作（push / 部署 / 发消息）始终先问用户**，不受自动放行影响。

### 3.1 执行前检查清单

- [ ] 已读取 `design-plan` 和 `exec-plan`
- [ ] 基线已建立（3.0）
- [ ] 已按 3.2 完成合并与波次计算
- [ ] **确认将使用 Agent subagent 执行每个单元（非直接实现）**

### 3.2 合并与波次计算

**先合并，再拓扑分层。** 输入是 exec-plan 的任务清单，输出是「执行单元」列表 + 波次划分。

#### 合并规则（冲突消解 + 提速）

```
R1. 两任务的「涉及文件集」有交集 → 必须合并为同一单元（并发写同一文件会互相覆盖）
R2. 同目录/同模块 且 单个 ≤2 文件的小任务 → 合并
R3. 合并后单元预算: 文件数 ≤ 5 且预计 subagent 工作时间 ≤ 10 分钟
R4. type=impl 与其配对的 type=test 始终留在同一单元（保持上下文连续）
```

R3 的上限来自 [3.5 防挂策略](#anti-hang)：单 subagent 超过 10 分钟容易被 stream watchdog 杀掉。

#### 波次分层

```
W0 = 无依赖的单元
Wn = 依赖全部落在 W0..W(n-1) 的单元
同波任意两单元的文件集必须互不相交 — 相交则把后者推到下一波
```

波次结果写入自动放行简报 / exec-plan，作为人类可读的计划视图。

### 3.3 并发派发（ready-set 调度）

执行机制用依赖驱动的 ready-set，而非死等整波 —— 某单元一完成，其下游立刻可以开工：

```
ready    = { 单元 | 所有 deps 已 completed }
inflight = 0

while ready 非空 or inflight > 0:
    从 ready 中挑 ≤ (3 - inflight) 个彼此文件集不相交的单元
    并发派发 impl subagent（用 [实现 subagent 模板](#templates)）
    inflight += N

    任一完成 → Read 验证文件真实落盘（不信任返回消息）
             → 标记 completed / incomplete
             → 重算 ready（incomplete 单元的下游保持阻塞）
```

- **并发上限 3**：超过则排队，等 slot 释放
- **失败隔离**：某单元失败/中断 → 只标记该单元，同波其他单元不受影响；其下游单元跳过，进入 Phase 5 统一处理
- **重派纪律**：同一单元最多 3 轮。重派前先读 review 报告，把意见附进新的实现 subagent 提示。3 轮后仍不通过 → 标记「阻塞」→ Phase 5 与用户讨论

### 3.4 Review 分档

每个单元先定档，再决定 review 方式。

#### 风险分档规则（满足任一即为 `deep`）

- 触及核心路径（被多模块调用 / 在关键数据流上）
- 并发 / 锁 / 异步 / 资源生命周期
- 对外契约（API、序列化格式、CLI 参数、DB schema）
- 安全相关（鉴权、输入校验、密钥处理）
- 单元含 > 3 文件 或 新增文件 > 2
- 在 Phase 1 被标记为高风险的模块

#### deep — 独立 review subagent

用 [review subagent 模板](#templates)，并追加硬要求：

- 必须实际跑命令，不只看代码
- 必须 `git diff <baseline> -- <files>` 对照基线，确认无计划外改动
- 测试合同逐条核对
- 问题清单按 **P0 / P1 / P2 分节**；P3 措辞级问题**仅在无 P0/P1 时**才允许列出

#### shallow — 本波批量 review

一个 subagent 审查本波所有非 deep 单元（模板见 [shallow review 模板](#templates-shallow)）：

- 对每个单元跑其验证命令并确认通过
- **只报 P0/P1**（功能错误 / 数据损坏 / 契约破坏）
- 输出一张表：`单元 | 命令 | 结果 | 判定`
- **禁止**把措辞、风格、文档建议写进报告 — 那类合并成一行「可选建议」带过

这直接解决 2026-09-17 反思的「P3 级文档措辞与 P0 级真缺陷同列报告」的信噪比问题。

**关键纪律**：
1. 实现 subagent 和 review subagent **必须是不同的 agent 调用**，不能是同一个
2. 实现 subagent 完成后，主 agent 必须先 Read 文件验证写入成功，再派发 review
3. review subagent **绝不修改任何业务代码或测试代码**——他是裁判，不是球员
4. deep 档单元各自独立 review；shallow 档按波批量，但批量 subagent 仍必须与实现 subagent 不同

## Subagent 派发模板
<a id="templates"></a>

标准化的 subagent 派发模板，所有 phase 通用。

### 实现 subagent 派发模板

Phase 3 使用。派发一个 subagent 写业务代码 + 写测试 + 跑测试。

```
Agent(
  subagent_type: "general-purpose",
  description: "<task-slug>",
  prompt: |
    <execution_mode>
    你现在处于**执行模式**, 不是 plan mode。
    - 任务规格已写在 <design-plan / exec-plan 路径>, 请用 Read 工具读取作为唯一依据
    - **直接执行** —— 用 Write/Edit 工具修改目标文件, 用 Bash 工具验证
    - 不要写任何"计划"或"摘要"文件 (尤其不要写到 .yushan/plans/)
    - 不要调用 EnterPlanMode
    - 你的最终输出应当是**真实的文件修改 + 验证命令执行**, 不是对动作的描述
    </execution_mode>

    <task>
    [任务描述 — 从 exec-plan 对应 Task 复制]
    </task>

    <files>
    [涉及文件路径列表]
    </files>

    <design_context>
    [关键设计决策 — 从 design-plan 提取]
    </design_context>

    <verification>
    [验证标准 checkbox 列表]
    </verification>

    <instructions>
    1. 读取涉及的文件
    2. 按设计决策实现任务
    3. 编写配套测试（确保覆盖正常路径和边界）
    4. 运行相关测试:
       - 执行 `[具体命令 — 从 exec-plan 确认]`
       - 确认全部通过
       - 如有失败：修复后重新运行直到通过
    5. 如果项目有类型检查：运行确认通过
    6. 如果项目有 lint 检查：运行确认通过
    7. **自审** — 重新读取修改过的所有文件，检查：
       - 代码逻辑是否正确，有无明显的遗漏或错误
       - 与设计决策是否一致
       - 所有调试代码 / 临时修改是否已清理
       - 如有问题 → 立即修复，重新验证
    8. 返回：变更摘要 + 测试结果 + 自审结论 + 遇到的问题
    </instructions>
)
```

### review subagent 派发模板

```
Agent(
  subagent_type: "general-purpose",
  description: "review-<task-slug>",
  prompt: |
    <execution_mode>
    你现在处于**审查模式**，只读，绝不修改任何业务文件。
    - 任务：审查实现 subagent 的产出
    - **不要修改任何业务文件或测试文件**
    - 不要调用 EnterPlanMode
    - 你的最终输出是 review 报告 + 通过/不通过判定
    </execution_mode>

    <task>
    审查以下实现是否符合设计:
    - 设计计划: [路径]
    - 执行计划: [路径]
    - 实现任务: [任务描述]
    - 涉及文件: [文件列表]
    </task>

    <instructions>
    1. Read 读取所有实现代码
    2. Read 读取所有测试代码
    3. 跑测试：执行 `[命令]`，确认测试通过（不要只看代码，要实际跑）
    4. 审查要点：
       a. 功能实现是否正确，与 design-plan 是否一致
       b. 测试是否覆盖正常路径和边界场景
       c. 测试是否真的能测出问题（不是弱测试）
       d. 代码质量：有无明显 bug、安全风险、性能问题
    5. 出具 review 报告：
       - 功能审查结论
       - 测试审查结论
       - 发现的任何问题（如有）
       - **最终判定：通过 / 不通过**
    </instructions>
)
```

### shallow review subagent 派发模板

<a id="templates-shallow"></a>

用于本波所有非 deep 单元。**核心约束：只报 P0/P1，禁止把措辞/风格/文档建议写进报告。**

```
Agent(
  subagent_type: "general-purpose",
  description: "review-shallow-wave-<n>",
  prompt: |
    <execution_mode>
    你现在处于**审查模式**，只读，绝不修改任何业务文件。
    - **不要修改任何业务文件或测试文件**
    - 不要调用 EnterPlanMode
    - 你的最终输出是一张结果表 + 通过/不通过判定
    </execution_mode>

    <task>
    批量审查本波以下单元:
    1. [单元 A: 任务描述 / 涉及文件 / 验证命令]
    2. [单元 B: ...]
    基线 ref: [BASELINE ref]
    </task>

    <instructions>
    1. 对每个单元:
       a. Read 其实现代码与测试代码
       b. 跑其验证命令 — 不要只看代码, 要实际跑
       c. 确认功能与 design-plan 一致
    2. **只报 P0 (功能错误/数据损坏) 和 P1 (契约破坏/测试失效)**。
       措辞、命名、风格、文档建议 **一律不写进报告** —
       如有必要, 全部压缩成最后一行「可选建议: ...」。
    3. 输出一张表:
       | 单元 | 命令 | 结果 | 判定(通过/不通过) |
    4. 如有 P0/P1: 逐条列出 — 单元 / 文件:行 / 问题 / 为什么算 P0/P1
    5. **最终判定: 通过 / 不通过**
    </instructions>
)
```

### 3.5 Subagent 防挂策略

<a id="anti-hang"></a>

Subagent 可能因「stream watchdog: no progress for 600s」而失败 — 连续 600 秒无任何输出，被系统自动终止。

**常见原因**

| 原因 | 说明 | 概率 |
|------|------|------|
| 长耗时命令 | `npm install` / `bun install` / 慢测试 / 大构建 | 高 |
| Bash 命令卡住 | 等待输入、网络超时、死锁 | 中 |
| subagent 任务过大 | 单个 subagent 需要改太多文件、做太多事 | 中 |
| subagent 陷入循环 | tool call 反复失败重试 | 低 |

**预防措施**

- 按 task 模板正常派发即可，但注意控制每个 subagent 的任务粒度
- 如果某个 task 预期需要长时间运行（如安装依赖、全量测试），将其拆为独立 task 或交给主 agent 的 review 阶段执行
- 单个 subagent 的工作量控制在 10 分钟内能完成为宜
- 长命令（>2 分钟）使用 `run_in_background` 或设置明确 timeout
- 避免在 subagent 内运行全量测试套件 — 改为只跑相关测试，全量测试留给主 agent 的 Phase 5 阶段

**如果 subagent 仍然挂了**：
- 主 agent 检测到 subagent 失败 → 标记 task 为「中断」
- 重新派发 subagent，附上以下帮助信息：
  - 已完成的部分变更说明（不让新 subagent 从零开始）
  - 明确指示**避免**上次卡住的操作（如"不要运行 npm install，依赖已就绪"）
  - 建议设置较短的 Bash timeout（如 120s）让失败快速暴露
- 如果反复挂 → 拆分为更小的 task
- **标记反思**：记录「有 subagent 中断」，在最后进入 Phase 5 反思

### 3.6 整体 Review

所有单元完成后，主 agent 执行整体审查：

1. **遍历 review 报告** — 读取 deep + shallow 全部 review 报告，确认全部通过
2. **阻塞单元处理** — 如有标记为「阻塞」的单元（3 轮 review 不通过），与用户讨论
3. **基线全量 diff** — `git diff <baseline> --stat`，确认无计划外改动（改到了 design-plan 未列出的文件）
4. **全量测试** — 运行项目测试套件确认无回归
5. **类型检查** — 运行 mypy / tsc / pyright 等确认通过
6. **Lint 检查** — 运行 ruff / eslint 等确认通过
7. **设计一致性** — 对比最终实现与 design-plan，确认无偏离

如发现问题 → 回到 3.3 重派实现 subagent 修复，再派 review 复核。

### 3.7 生成修改报告

所有任务完成后，派 subagent 写入 `docs/reports/YYYY-MM-DD-<feature>-report.md`。**严格遵循「跨阶段公约 B」**。

派发 subagent：

```
Agent(
  description: "write-report",
  subagent_type: "general-purpose",
  prompt: |
    用 Write 工具创建文件 [ROOT]/docs/reports/YYYY-MM-DD-<feature>-report.md
    参数:
      - file_path = [ROOT]/docs/reports/YYYY-MM-DD-<feature>-report.md
      - content = [以下 markdown 块]

    <content>
    [完整的报告 markdown — 见 EXAMPLES.md]
    </content>

    强制要求:
    - 必须**真正调用 Write 工具**, 不是返回 markdown 文本
    - Write 后用 Read 回读一次, 确认内容完整
    - 在回复中报告: Write 是否成功、文件大小
    - 如果 Write 不可用 (plan mode 限制), 明确报告并返回完整 markdown
)
```

#### Orchestrator 验证 (派发后必做)

```bash
bash scripts/verify-write.sh [ROOT] docs/reports/<feat>-report.md
```

判定分支按公约 B.3，缺失则按 a/b/c/d 退化处理。

报告模板见 [EXAMPLES.md#report](EXAMPLES.md#report)。

### 3.8 反模式（编排者红线）

#### 反模式表

| ❌ 错误做法 | ✅ 正确做法 |
|------------|------------|
| 在当前 agent 上下文中直接改文件实现任务 | 派发 `Agent(subagent_type="general-purpose")` 执行 |
| 编排者用 Write/Edit 写**业务代码 / 测试 / 业务文档** | 永远派 subagent 写 — 即使 subagent 反复失败 |
| Subagent 失败后 orchestrator 失去耐心, 自己 Write 兜底 | 重新派发, 附上失败原因 + 显式 `<execution_mode>` 块 |
| 实现和 review 用同一个 subagent | 必须分两个 agent：实现 agent 改代码，review agent 只读审查 |
| 跳过自审，直接标记 completed | Read 文件验证后再标记 |
| 验证失败后忽略，继续下一任务 | 重新派发 subagent 修复 |
| Subagent 返回后手动修补代码 | 将问题反馈给 subagent，让它修复 |
| 两个并发单元的文件集有交集 | 合并为同一单元，或将后者推到下一波（3.2 R1） |
| 并发数超过 3，或并发派发前不复核文件集不相交 | 派发前复核；超出则排队等 slot |
| 某单元失败就中止整波 | 失败隔离：只标记该单元，同波其他单元继续 |
| 用 auto-proceed 来回避 C.2 设计核实 | 核实不做完 → 判据不满足 → 回签字路径 |
| 自动放行模式下执行不可逆操作 | 先建备份（baseline.sh / 导出 schema）；建不了则问用户 |

#### Orchestrator Write 红线

- ❌ **绝对禁止**：用 Write 工具创建源代码 / 测试文件 / 业务文档
- ❌ **绝对禁止**：用 Edit 工具修改业务代码
- ✅ **允许**：用 Write 工具写元文档 — design-plan (`docs/design-plans/...`)、exec-plan (`docs/exec-plans/...`)、report (`docs/reports/...`)、design-final (`docs/design-final/...`)。这些是 workflow 自身的产物, 不是"业务实现"
- ✅ **允许**：用 Bash 工具运行 `scripts/init-workflow.sh` / `verify-write.sh` / `archive.sh` (确定性操作)

打破红线会污染 orchestrator 上下文, 绕过 subagent 的 review 机制, 让 Phase 3 的 review 失效。

---

## Phase 4: Finalize（归档与融入）

<a id="phase-4"></a>

### 4.1 融入最终设计文档

当前 agent 按 design-plan 中「波及文档」清单，逐一检查每个项是否需要更新。**严格遵循「跨阶段公约 B」**：

1. **需要更新** → 派发 subagent 在 `docs/design-final/` 下追加对应章节
2. **不需要** → 标记已确认
3. 全部确认/更新完成后，进入 4.2 归档
4. 如新增文档则直接在 `docs/design-final/` 下创建

派发前准备：
- `Bash(pwd)` 拿项目根绝对路径，记为 `[ROOT]`
- 明确列出本轮需要追加/创建的目标文件（来自 design-plan 的「波及文档」清单），每个文件对应一个 `Write` 调用
- 准备好每个文件的完整 markdown 内容（追加时读取现有内容 + 拼接新章节；新建时直接准备完整文档）

派发 subagent（一个 subagent 处理整个清单，而不是每个文件一个 subagent — 保持上下文清洁）：

```
Agent(
  description: "finalize-design-docs",
  subagent_type: "general-purpose",
  prompt: |
    处理 [ROOT]/docs/design-final/ 下的波及文档清单。

    目标文件清单:
    1. [ROOT]/docs/design-final/<file-1>.md — 追加章节
       追加内容: [markdown 块]
    2. [ROOT]/docs/design-final/<file-2>.md — 新建
       完整内容: [markdown 块]
    3. ...

    强制要求:
    - 对每个文件**真正调用 Write 工具一次**
    - 追加前先用 Read 读取现有内容, 把新章节拼接到合适位置
    - 每个 Write 后用 Read 回读一次, 确认内容完整
    - 在回复中报告: 每个文件的 Write 是否成功、文件大小变化
    - 如果 Write 不可用 (plan mode 限制), 明确报告并返回所有目标文件的完整 markdown
)
```

#### Orchestrator 验证 (派发后必做)

```bash
bash scripts/verify-write.sh [ROOT] \
  docs/design-final/<file-1>.md \
  docs/design-final/<file-2>.md
```

判定分支按公约 B.3，缺失则按 a/b/c/d 退化处理。

### 4.2 归档

通过 subagent 将 design-plan 和 exec-plan 移动到各自的 completed 目录。**遵循「跨阶段公约 B」的 plan mode 退化策略**（本 phase 用 Bash 调 archive 脚本）。

派发前准备：
- `Bash(pwd)` 拿项目根绝对路径，记为 `[ROOT]`
- 确认源文件存在（`bash scripts/verify-write.sh [ROOT] docs/design-plans/<slug>.md docs/exec-plans/<slug>.md`）

派发 subagent：

```
Agent(
  description: "archive-plans",
  subagent_type: "general-purpose",
  prompt: |
    用 Bash 工具执行以下脚本 (项目根为 [ROOT]):

    bash [ROOT]/scripts/archive.sh [ROOT] YYYY-MM-DD-<feature>

    强制要求:
    - 必须**真正执行脚本**, 不是返回命令字符串
    - 在回复中报告: 脚本的退出码、输出
    - 如果 Bash 受限 (plan mode 限制), 明确报告
)
```

#### Orchestrator 验证 (派发后必做)

直接用脚本：

```bash
ls -la [ROOT]/docs/design-plans/completed/<slug>.md
ls -la [ROOT]/docs/exec-plans/completed/<slug>.md
```

判定分支按公约 B.3，缺失则按 a/b/c/d 退化处理。

> **退化简化**：归档是确定性操作，建议 orchestrator 直接跑 `bash scripts/archive.sh [ROOT] <slug>`，不必派 subagent（节省一次 subagent 调用 + 减少 plan mode 风险）。派 subagent 仅在需要复杂判断时（如归档前修改文件）使用。

### 4.3 确认状态

- [ ] 报告已就绪：`docs/reports/YYYY-MM-DD-<feature>-report.md`
- [ ] 波及文档清单已全部确认更新（4.1）
- [ ] 最终设计文档已更新到 `docs/design-final/`
- [ ] design-plan 已归档
- [ ] exec-plan 已归档
- [ ] 反思判断：如有 subagent 中断 / review 未通过 / 效果不符预期 → 进入 Phase 5 反思

---

## Phase 5: Check（核对需求完成度 + 条件反思）

<a id="phase-5"></a>

**执行模式**：`EnterPlanMode`（回到只读模式）。

### 5.1 核对需求

1. 读所有 review 报告（deep 单元的独立报告 + shallow 的各波批量报告）
2. 跑全量测试确认通过
3. 对照 Phase 1 的测试合同逐条确认验收状态
4. 输出状态报告：完成 / 有风险 / 跳过 / 阻塞
5. 如有遗漏或失败 → 与用户讨论 → 决定是否补充轮次

### 5.2 反思（条件触发）

工作流执行完毕后，反思 **sequential-workflow skill 自身的设计**是否适合本次任务，记录改进方向。不是反思项目功能，而是反思工作流这个工具本身。

反思由当前 agent 直接执行，不派发 subagent。

**触发条件**：

| 条件 | 触发场景 | 标记来源 |
|------|---------|---------|
| subagent 中断 | Phase 3 中 subagent 被 kill、超时、返回不完整 | Phase 3.5 标记 |
| review 未通过 | Phase 3.6 整体 review 发现多处问题或反复修复不通过 | Phase 3.6 标记 |
| 用户中断 | 用户在工作流中途要求停止或转向 | 当前 agent 自行判断 |
| 效果不符预期 | 最终结果不符合验收标准或用户不满意 | Phase 5.1 判断 |
| 执行流畅但 workflow 设计有优化空间 | 无上述问题，但 agent 发现某阶段设计可改进 | 当前 agent 自行判断 |

如以上条件均不满足 → 跳过反思，工作流结束。

**反思模板**（写入 `~/.yushan/skills/sequential-workflow/learned/YYYY-MM-DD-HHMM.md`）：

```markdown
# YYYY-MM-DD HH:MM — sequential-workflow 反思

## 本次任务类型
- 项目性质 / 需求类型 / 规模

## 触发阶段
哪个阶段出问题了？Phase 1(Plan) / Phase 2(Write) / Phase 3(Execute) / Phase 4(Finalize) / 用户提前中断

## 问题描述
- workflow 的哪个设计假设不成立？
- 哪一步做起来别扭 / 效率低 / 容易出错？

## 根因
- 是 workflow 阶段划分不合理？
- 是模板（design-plan/exec-plan）的内容/结构不符合这类需求？
- 是 subagent 策略（并发控制、任务粒度）不适合？

## 对 workflow 的改进建议
- 应该新增 / 删除 / 合并哪个阶段？
- 某个模板应该怎么调整？
- subagent 策略应该怎么调？

## 参考案例
- 具体链接或文件路径（design-plan / exec-plan / report 路径）
```

> **注意**：反思记录在 `~/.yushan/skills/sequential-workflow/learned/` 下累积，所有未来的 sequential-workflow 运行都应主动检查 `learned/` 目录中的历史记录，看看本次任务类型是否有前人踩过的坑。

填好的样例见 [EXAMPLES.md#reflection](EXAMPLES.md#reflection)。

如无需反思，直接跳过本阶段。

---

## 完整流程速查

<a id="checklist"></a>

```
前置（进入 Phase 2 之前必做）:
  [ ] 已 EnterPlanMode（Skill 加载后立即执行）
  [ ] Phase 1 全程在 plan mode 下执行（只读探索 + 对话设计）
  [ ] 测试合同已确认（用户签字, 或 C.3 自动放行判据全绿）
  [ ] 编排方案已确认（同上）

放行后 ExitPlanMode:
  [ ] Bash(pwd) 拿到 [ROOT] 绝对路径 — 所有文件路径以 [ROOT] 开头
  [ ] bash scripts/init-workflow.sh [ROOT] — 创建目录（full 方案）

Phase 1: Plan（plan mode，只读执行）
  [ ] **Step 0 设计产物探测** — ls [ROOT]/docs/arch/*/，读 context/design/review.md
      ├── 有 design.md + review.md → 走「设计已就绪」路径
      └── 无 → 走「从零设计」路径（下面的 Three-Pass）
  [ ] 【设计已就绪】C.2 设计核实 — 7 条清单逐条对照真实代码, 记录偏差
  [ ] 【从零设计】收集需求 & 提取关键词
  [ ] 【从零设计】Seed Pass — 并行 2-3 个 Explore agent 搜索源码/测试/配置
  [ ] 【从零设计】Expand Pass — 双向依赖追踪，补全文件树
  [ ] 【从零设计】Verify Pass — 对照完整性清单确认无遗漏
  [ ] 【从零设计】提出方案 & 确认范围
  [ ] **测试合同协商（不得跳过）** — 逐条确认验收方法，写入 design-plan「测试策略」节
  [ ] **推荐编排方案**（full/quick/bugfix）
  [ ] **Step 7.6 自动放行判定** — C.3 六条判据
      ├── 全绿 → 输出自动放行简报 → 直接 ExitPlanMode（不等用户）
      └── 有 ❌ → 回落 Step 7 用户签字

Phase 2: Write（full 方案，主 agent → subagent）
  [ ] 派发 subagent 写入 design-plan + exec-plan（遵循「跨阶段公约 B」）
  [ ] bash scripts/verify-write.sh [ROOT] docs/design-plans/<feat>.md docs/exec-plans/<feat>.md
  [ ] **EXEC-PLAN VALIDATION** — type=impl 都有 type=test 配对；每个任务都有「依赖」和具体「涉及文件」

Phase 3: Execute（主 agent → subagent，波次并发）
  [ ] **3.0 基线** — bash scripts/baseline.sh [ROOT] <slug>
      └── NONE → 停下问用户
  [ ] **3.2 合并与波次** — 文件集有交集则合并；同波文件集互不相交
  [ ] **3.3 ready-set 并发派发**（上限 3）
      [ ] 派发**实现 subagent**（写代码 + 写测试 + 跑测试）
      [ ] Read 文件验证写入成功（不信任返回消息）
      [ ] 失败隔离 — 只标记该单元, 同波其他单元继续
  [ ] **3.4 review 分档**
      [ ] deep 单元 → 各自独立 review subagent（跑命令 + git diff 基线 + P0/P1/P2 分节）
      [ ] shallow 单元 → 本波一个批量 review subagent（只报 P0/P1）
      [ ] 不通过 → 重派该单元（最多 3 轮）
  [ ] 3.6 整体 review（遍历报告 + 基线全量 diff + 全量测试 + 类型 + lint + 设计一致性）
  [ ] 3.7 派发 subagent 生成修改报告
  [ ] bash scripts/verify-write.sh [ROOT] docs/reports/<feat>-report.md

Phase 4: Finalize（主 agent → subagent）
  [ ] 按波及文档清单逐项派发 subagent 更新
      [ ] bash scripts/verify-write.sh [ROOT] docs/design-final/<file>.md ...
  [ ] bash scripts/archive.sh [ROOT] <slug> 归档
      [ ] ls [ROOT]/docs/{design,exec}-plans/completed/<slug>.md 确认移入
  [ ] 最终确认（如建过基线 commit, 提示用户可 git reset --soft <base>）

Phase 5: Check（回 plan mode，主 agent 直接执行）
  [ ] EnterPlanMode
  [ ] 读所有 review 报告（deep 独立报告 + shallow 批量报告）
  [ ] 跑全量测试确认通过
  [ ] 对照 Phase 1 测试合同逐条确认
  [ ] 输出状态报告（完成/有风险/跳过/阻塞）
  [ ] 如有遗漏或失败 → 与用户讨论 → 决定是否补充

反思判断（Phase 5 内）:
  [ ] 检查触发条件：subagent 中断 / review 未通过 / 用户中断 / 效果不符预期
  [ ] 如触发 → 写反思到 `~/.yushan/skills/sequential-workflow/learned/<timestamp>.md`
```
---

## 附录: skill 自身维护检查清单

<a id="skill-checklist"></a>

修改本 skill 后对照检查：

- [ ] description 含 "Use when..." 触发器
- [ ] SKILL.md < 200 行（深度内容在 REFERENCE.md）
- [ ] 头部有 "Quick start"（探/核/签/走/查）
- [ ] 核心约束清晰列出（含自动放行唯一例外）
- [ ] 5 个 phase 流程完整
- [ ] Plans 目录和 3 个默认方案存在
- [ ] Subagent 纪律和模板在 REFERENCE.md
- [ ] 反思目录 `learned/` 路径正确
- [ ] 脚本表格与实际 `scripts/` 内容一致
