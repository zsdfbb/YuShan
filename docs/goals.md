# 能力目标与工作分解

> 性质：**目标台账**，与 `docs/TODO.md` 互补。
> `TODO.md` 收的是「ADR 已 accepted、设计已写完，代码里还没有」的条目 —— 它按**已有设计**组织。
> 本文收的是**反向**的东西：从「我要用它干什么活」出发，倒推还缺哪些能力，以及每项的**判定依据**。
> 两边交叉引用的条目保持指针一致；本文不重复 `TODO.md` 的现状描述。
>
> 目标用户不是单一角色：**本项目自维护** / **用户日常开发** / **投资分析**，三者共用同一套工具面。
> 日期：2026-11-05

---

## 0. 五个目标场景与现状对照

| 场景 | 判定「能端到端」的标准 | 现状 | 缺口类别 |
|---|---|---|---|
| **G1 代码编写** | 给定一个真实仓库任务，agent 能改代码、跑测试、自我修正到绿 | ✅ **已闭环**：11 元基础表（`apps/coding-agent/src/toolset.rs:42`）+ `subagent_async` 派发 + 6 个自带 skill（`skills/`） | 无结构性缺口 |
| **G2 网页制作** | 写完页面 → **自己看图验收** → 改到满意 | 🟡 可写文件、可 `bash` 起服务；`view_image` / `ocr` 已能看图（`adapters/tools-basic/src/view_image.rs:90`） | **缺「把页面变成图」这一步**（无截图/浏览器能力） |
| **G3 GitHub 项目维护** | 拉 issue → 改 → 测 → 提交 → 回写 issue/PR | 🟡 `bash` 可调 `git` / `gh`，零代码 | **缺凭证托管** + **缺跨回合推进**（批量 issue 循环） |
| **G4 连接飞书** | 收到事件 → 处理 → 回发消息 | ❌ 出站可 `bash` + `curl`；入站无落点 | **缺外部资源接入缝**（MCP 代码零命中）+ **缺跨回合推进** |
| **G5 投资分析** | 取数据 → 算指标 → 出报告（图表 + 结论） | ❌ `adapters/tools-finance` crate 不存在；`docs/arch/etf-display-tool/` 只有 `context.md` | **架构未启动**，不是差实现 |

一句话读法：**G1 已经能用；G2 差一步；G3 / G4 同一个根因（跨回合推进）；G5 还没开始。**

---

## 1. 核心判断：三处结构性欠账

五个场景里真正**反复出现**的缺口只有三处。工具数量不是瓶颈 —— 现在是 11 元基础 + 3 元追加，
对 G1 已经够用。

### 欠账 A：联网只有读，没有写

`webfetch` 只发 GET（`adapters/tools-web/src/fetch.rs:320` 的 `client.get(...)`；
全仓 `tools-web` 内唯一的 `post(` 在 `backends.rs:397-421`，是搜索后端专用，不是通用出口）。
⇒ 部署、调外部 API、回写飞书/Jira，全部退回 `bash` + `curl`。

**这不必然是缺陷**：`bash` + `curl` 能覆盖大部分场景，且不必在工具层重建鉴权/重试/分页抽象。
**判定何时该建**：当同一份"多步鉴权 + 重试 + 分页"逻辑在 skill 里被抄第三遍时。

### 欠账 B：系统不能推一个回合（**G3 / G4 的共同根因**）

`Boundary` 只有四格（`crates/ys-protocol/src/boundary.rs:12`：`Steer` / `Deferred` / `Compact` / `Abort`），
没有"下一回合"这一格；app 侧也没有跨回合输入队列。

已落地的两件**不等于**这一格（详见 `docs/TODO.md` §B）：

- `PendingResults`（`apps/coding-agent/src/subagent.rs:268`）+ `Wake::SubagentReturned`
  （`apps/coding-agent/src/app_loop.rs:413`）能自动起回合，但它是**进程级投递表**，不是协议变体，
  且 `take_pending` 的唯一入口仍是 `Request::Prompt`。
- ADR-0047 定了 `DeliveryTarget` / `Delivery`，但代码零命中（`accepted (partial)`）。

⇒ **G3 的"夜间批处理 issue"、G4 的"收到 webhook 就干活"，都卡在这同一格。**
这是全部五项里**唯一一处"一处修、两处通"**的工作。

### 欠账 C：外部资源没有接入缝

MCP 在 `crates/` / `adapters/` / `apps/` 全部 **零命中**（只在 `docs/design.md`、
`docs/adr/0021-plugin-seam-without-hook-system.md` 里作为"可选组件"提及）。
`ToolPlugin` 同样零命中 —— ADR-0021 只留了缝，没建。

⇒ 飞书、Jira、Notion、金融数据源，现在每条都要写一个定制适配器（各带 OAuth、限流、schema）。
**一处实现、N 处收益**的做法是 MCP 客户端，但它有一个前置决策要定：
**外部工具表要不要进子表前缀锚**（`apps/coding-agent/src/toolset.rs:648-661` 的字节前缀契约）。

---

## 2. 工作分解

按「先测量、再修地基、再加能力」排序。每项给**判定依据**与**规模**（S ≤ 半天 / M ≤ 2 天 / L > 2 天）。

### 第 0 步 · 四个基准任务，先跑一遍（M）—— **前置，不产出代码**

用**现有**工具面把 G2–G5 各做一个真实任务：一个落地页、一个 GitHub issue 修复、
一条飞书消息、一份 ETF 对比报告。记录卡点。

- **判定依据**：四份「卡点记录」，每份至少一条可复现的失败。
- **为什么先做**：`docs/TODO.md` §C 那批"卡在待裁决"的需求，共同病因是**没有真实任务数据**。
  这一步同时把 §E 的裁决题从"凭感觉选"变成"有据可判"。

### 第 1 步 · `bash` 有界收集（S，无契约变更）

`adapters/tools-basic/src/bash.rs:507`、`:512` 的 `read_to_end` 无界累积，截断到 `:207` 才做。
照 `adapters/tools-basic/src/exec.rs` 的 `run_bounded` 语义收。

- **判定依据**：一次 `yes | head -c 500M` 类的输出不把内存/上下文吃穿；已有用例全绿。
- **为什么是它**：四类场景的共同瓶颈都是"跑命令读输出"，而它是**唯一一项既省钱又防 Termux 拖死**的改动
  （`docs/TODO.md` §D 的 R2）。

### 第 2 步 · 页面截图（M）—— 装成 skill，**不进核心**

`view_image` 已能看图，缺的只是"把页面变成 png"。用 skill（脚本 + 本机 Playwright）接上。

- **判定依据**：G2 的判定标准在**不看终端**的情况下走通一次（写完 → 截图 → 看图 → 改）。
- **边界**：不进 `adapters/tools-basic` —— 不破前缀指纹锚，不增核心面积。
  `skills/diagnose/SKILL.md:23` 已把"无头浏览器脚本"列为可选手段，形态有先例。

### 第 3 步 · MCP 客户端（L）—— 一个缝换 N 个集成

先定一个决策，再动手：**外部工具表是否进子表前缀锚**。

- 若**不进**（外部工具排在前缀之外）：改动小，但子 agent 拿不到外部工具。
- 若**进**：要保证"基础段在前、追加段在后"的相对顺序不变，否则父表字节前缀契约断裂。

- **判定依据**：接一个真实外部服务（优先飞书）走通"出站调用"；`ToolPlugin` 装配缝有名字有实现。
- **为什么排在这里**：它吃掉了 G4 与 G5 的数据源接入，比逐个写适配器省一个数量级。

### 第 4 步 · 跨回合推进（L）—— 欠账 B

把"推进"放进 `Boundary`（跨回合有序队列），还是再造一张表（如 `PendingResults`）——
这是具体结构问题，须走 `arch-design`。

- **判定依据**：G3 的"连续处理 3 个 issue 不需人工敲字"与 G4 的"webhook 事件驱动一个回合"能跑通。
- **约束**：不得绕过 `Abort`（`docs/arch/superagent-reference/design.md` §4.3）。
- **前置**：第 0 步的数据要能说明"人工敲字"确实是瓶颈。

### 第 5 步 · 长任务状态：todo + follow-up（M–L）—— `TODO.md` §B

`ToolPlugin` / `todo.json` / `should_inject` 全仓零命中；follow-up 缺的正是欠账 B 那一格。
**顺序是 follow-up → todo**（0024 的推动判据依赖 0014 的机制）。

- **判定依据**：一个跨 20+ 回合的任务能自我跟踪未完成项，不靠模型记忆。

### 第 6 步 · 投资分析（M–L）—— **先用 skill，不建 crate**

用 skill + `bash`（取数、算指标、写 HTML）跑出第一份报告；等"每周都要做"成为事实再谈 `tools-finance`。

- **判定依据**：一份含图表与结论的 HTML 报告，数据来源可复现。
- **前置未决**（`docs/arch/etf-display-tool/context.md:99`）：报告目录可配 / ECharts CDN 降级 /
  ToolSpec 是否内联 JSON schema 示例 —— 这三条**放进 skill 就绕开了**，不必等立项。

### 旁支 · 工具能力的编译期 cfg 门控（M）—— **仅在出现分发需求时做**

结论已核（可行性、边界、成本）：

- **能做**，但 `vec![...]` 的元素位**写不了 `#[cfg]`**（实测 `error[E0658]: attributes on expressions
  are experimental`）；`const &[&str]` 的元素位**可以**。⇒ 工具表要改 `push` 序列。
- **粒度只到"整块能力"**（`web`、`ocr-vision`），不做每工具一个 feature —— 否则 `EXPECTED_ORDER`
  会拆成 2^n 种形态，而它被 7 处引用（`apps/coding-agent/src/assembly.rs:326`、
  `apps/coding-agent/src/prompt.rs:686`/`:810`/`:821`/`:868`/`:2116`）且是"顺序即语义"的唯一真值。
- **硬规则**：cfg 只允许在**段尾追加**，不允许插队 —— 否则子表字节前缀契约断裂。
- **成本在测试不在代码**：`apps/coding-agent/src/toolset.rs:641`（`specs.len(), 11`）、
  `:659-661`（`parent_specs.len(), 14`）把**基础表元数 / 父表元数**写成了字面量，
  要改成由 `expected_order()` 推；`./scripts/par-test.sh` 只跑默认组合
  ⇒ 非默认组合会静默腐烂，除非 CI 加 `--no-default-features` 矩阵。
- **不变量不能动**：`apps/coding-agent/src/toolset.rs:684-686` 断言「父表 JSON 是子表 JSON 的字节前缀」——
  cfg 唯一允许的形态是**段尾追加**，插队即破。反过来说：只要守住这条，
  feature 开关就**不需要**改这条断言。
- **现状**：全仓 **0 个 `[features]`、0 处 `cfg(feature)`**，干净起步。
- **判定该不该做**：出现"要给某类用户发一个不带联网能力的二进制"的真实需求。
  在此之前它是**分发形态**问题，不是**能力**问题，不阻塞 G1–G5 任何一项。

---

## 3. 明确不做

| 不做 | 理由 |
|---|---|
| 通用 `http_request` 写侧工具 | `bash` + `curl` 覆盖 90%；只在出现"多步鉴权/重试/分页"真实需求时建（欠账 A 的判据） |
| 逐工具的编译期 feature | 见旁支：会把唯一真值拆成矩阵，收益只是体积 |
| `tools-finance` crate（现阶段） | 无需求证据；skill 先跑通再说 |
| DE / notebook 类工具 | 无需求证据 |
| ADR-0054 状态板全做 | `docs/TODO.md` §E 第 1 条待裁决；第 0 步之后答案可能变 |

---

## 4. 与现有文档的关系

| 文档 | 关系 |
|---|---|
| `docs/TODO.md` | 互补：它按**已有设计**组织（还差什么），本文按**目标场景**组织（为了干什么）。交叉处已互指 |
| `docs/architecture.md` | 分层与数据流的**全貌**；本文不重复 |
| `docs/extension-points.md` | 新需求的**归类判据**（Event / Tool / Follow-up / 真 Hook）；第 3 步之前先过一遍 |
| `docs/arch/superagent-reference/design.md` | 欠账 B 的设计**约束**来源（§4.3），本文只引不复述 |
