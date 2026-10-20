# 后台命令执行 + 两条既存缺陷修正 设计计划

> 性质：**design-plan**（把已定稿的设计拆成可落地的切片）。
> 权威设计：[`docs/arch/execution-efficiency/design.md`](../arch/execution-efficiency/design.md)
> （**§4.9 是切片表**、§5 是两条既存缺陷 D1/D2、§6 是验收口径、§7 是残留风险、§8 是明确不做的方向）；
> 决策记录：[`adr-background-command-execution.md`](../arch/execution-efficiency/adr-background-command-execution.md)
> （含 2026-10-21 定稿的三条架构级决策）；评审与处置：[`review.md`](../arch/execution-efficiency/review.md)
> （R1–R18 **已全部处置**）；现状与约束：[`context.md`](../arch/execution-efficiency/context.md)；
> 跨产品对照：[`research.md`](../arch/execution-efficiency/research.md)。

## 背景

- 缘起：一次**同任务、同模型、同推理强度、同网关、同工作区、同提示**的跨 harness 对照，
  两个 harness 的回合墙钟差 **4.9×**。拆开后，可归因到本仓的有三项：

| 差距 | 量级 | 本轮归属 |
|---|---|---|
| **G1** 分钟级真命令同步阻塞整轮，且同一条命令**被跑了两遍** | 工具项 58s / 430s = **13.5%** | **本计划的主线**（切片 0a–0f、1） |
| **G2** 每轮生成吞吐只有对照组的 ~56% | 43 tok/s vs ≥77 tok/s | **切片 2**（协议面 A/B，独立） |
| **G3** 上下文窗口少报一半 | 128000 vs 258400 | **切片 0c**（用户级覆盖 + 声明） |
| **G4** 轮数多 8 轮（21 vs 13） | 放大上面三项 | **不做**（`explore-speed` 的领地，见 design.md §8） |

- 本计划回收的是 **G1**，**不是** 4.9× 的主因（design.md §0）。G2 与 G4 本轮不碰。
- 本轮同时修掉两条**与后台执行无关**的既存缺陷（design.md §5）：D1（模型的 `timeout` 参数与外层
  工具预算脱钩）、D2（无 boundary 路径超时不杀进程组）。

## 本轮范围

**切片 0a–0f、1、2 全部落地**（design.md §4.9）。

- **0a–0f（P0）**：模型可见契约**零改动**的改动 + 两条既存缺陷修正 + 文档同步。
  P0 是 P1 的**验收仪器**：没有 0e 的度量，切片 1 的收益无法证明（design.md §4.9 末段）。
- **1（P1）**：两个新工具 `bash_bg` + `bash_poll`，文件后端，六个显式清理点。
- **2（P2）**：`/chat/completions` vs Responses API 的同 prompt A/B，**独立切片**，不阻塞 0/1。

## 设计

**不重述** —— 见 [`design.md`](../arch/execution-efficiency/design.md) 全文。本计划只做三件事：
① 逐切片交代**目标 / 落点文件 / 验收口径 / 依赖**；② 交代**哪些是零契约改动、哪些动 `ys-tool` /
`ys-loop`、哪些要用户级配置**；③ 把 design.md §7 §8 两条清单**原样带过来**（不增不减）。

### 切片总表

| 切片 | 一句话目标 | 动契约吗 | 依赖 |
|---|---|---|---|
| **0a** | 修 **D1**：外层工具预算经 `ToolContext` 传给工具，工具内**始终**有 deadline | 否（`ToolSpec` 不变；`ToolContext` 是内部字段） | 无 |
| **0b** | 修 **D2**：无 boundary 路径的超时分支补进程组杀 | 否 | 无 |
| **0c** | **G3 窗口声明**：用户级 `windows` 覆盖 + `/model` 显示窗口值与来源 | 否（用户可见文案 + 用户级配置） | 无 |
| **0d** | 提示词**两条 guideline** | 否（提示词内容；`bash` 的 spec 不变） | 无 |
| **0e** | **debug 度量补齐**：新增 `command_fp` 字段，四个指标可从日志直读 | 否（`yushan.debug/1` 同一 major 内加字段） | 无 |
| **0f** | **文档同步**：`AGENTS.md` / `docs/design.md` §12 / `docs/design-final/coding-agent-tui.md` | 否 | 无 |
| **1** | `bash_bg` + `bash_poll` + 文件后端 + 六个清理点 | **是**（父表 10 → 12 元；`bash` 一字不改） | 0a–0e |
| **2** | 协议面 A/B | 否（只跑不改） | 无 |

---

## 切片逐项

### 切片 0a · 修 D1（外层预算进工具）

**目标**

模型的 `timeout` 参数与外层工具预算脱钩 ⇒ 模型以为有 900s、实际 300s 被砍，且**模型毫不知情**。
修成三步（design.md §5 D1）：

1. **预算进工具**：外层预算经 `ToolContext` 的新字段传给工具；
2. **工具内始终有 deadline**：`effective = min(模型值, 外层预算 − Δ)`；**模型未传值时取 `外层预算 − Δ`**
   （不是「没有 deadline」）；`Δ` 取 5s 量级 ⇒ 内层**严格早于**外层到期（design.md §3.5 修正 5）；
3. **文案写生效值**：超时错误由**工具自己**产出（`Command timed out after <effective> seconds`），
   而不是 loop 的 `Tool '<name>' timed out after <budget>`。

**落点文件**

| 文件 | 改动 |
|---|---|
| `crates/ys-tool/src/context.rs` | 加 `pub tool_budget: Option<Duration>`（**第 4 个可选挂载点**，编号按可选挂载点计 —— 现有 3 个：`boundary` / `summarizer` / `subagent`）与 `with_tool_budget` 构造器；**不进 `ToolSpec`** |
| `crates/ys-loop/src/basic.rs` | **两处** `ToolContext::new` 都要填：并行路径 `:666`、串行路径 `:716`；各填**各自的**工具预算（`tool_budget(tool, &ctx.limits)`，预算本就是**按工具**的） |
| `adapters/tools-basic/src/bash.rs` | 工具内 deadline 计算与超时文案；两条路径（有/无 boundary）都用同一份生效值 |

**验收口径**（design.md §4.9 切片 0a）

1. 模型传 `timeout: 900` ⇒ 实际在 **~295s** 被砍、文案写**生效值**、**进程组被杀**；
2. 模型**不传** `timeout` ⇒ 外层预算到点时**进程组被杀**（**不靠** `kill_on_drop`）。

**依赖**：无。

**为什么预算要穿过 `ToolContext` 而不是在工具里缓存**：预算**每回合刷新**
（`apps/coding-agent/src/app_loop.rs` 每回合重算 limits），而工具是**建表期产物**
⇒ 任何在工具里缓存预算的静态做法都会过期（design.md §4.4）。

### 切片 0b · 修 D2（无 boundary 路径补进程组杀）

**目标**

`ctx.boundary` 为 `None` 时（`-p` / `--json` 路径与任何未挂边界的调用），一条命令超时 ⇒
`sh` 被杀，但 `cargo` / 测试二进制等**孙进程继续跑到自然结束** ⇒ 孤儿进程 + 端口/文件锁占用。
`ctx.boundary` 为 `Some` 时**也有**同一个缺口（外层预算到点 drop future 时走的同样是 `kill_on_drop`）。

**落点文件**

| 文件 | 改动 |
|---|---|
| `adapters/tools-basic/src/proc_group.rs`（**新建**） | 进程组清理的**同步内核**（`sh -c "kill -9 -<pid>"`）；把 `bash.rs` 现有的两条告诫（「**必须先杀后收**」「pid 复用窗口」）搬进本模块的 doc comment（ADR 2026-10-21 决策 5） |
| `adapters/tools-basic/src/bash.rs` | 现有 `kill_process_group`（`:167`）改为**委托**共享内核；无 boundary 路径的超时分支（`:265-268` 的空 `#[cfg(unix)]` 块）补上进程组杀 |
| `adapters/tools-basic/src/exec.rs` | `run_bounded` 复用同一份内核（改 import 路径，**行为不变**） |
| `adapters/tools-basic/src/lib.rs` | 加 `pub mod proc_group;` |

**验收口径**（design.md §5 D2）

- `bash("sleep 60 & wait", timeout 1)` ⇒ 断言**孙进程**（`sleep`）不存在；
- 再加一条「**不传** `timeout` + 外层预算到点 ⇒ 断言孙进程不存在」（这一条依赖 0a 的「始终有 deadline」）。

**依赖**：无。**但 0a 与 0b 必须一起验收** —— design.md §5 D2 的四条组合矩阵只有在
「工具内始终有 deadline」成立时才全部收敛到「工具自己的超时分支 ⇒ 杀进程组」。

### 切片 0c · G3 窗口声明

**目标**

本仓的窗口解析是**四级**（`apps/coding-agent/src/config.rs::context_window_for`）：
① `~/.yushan/config.json` 的 `windows` 覆盖 > ② provider 目录里的 `context_window` >
③ 内置兜底表 > ④ 默认 128000。`minimax-m3.1-flash-free` 匹配不到 ②③ ⇒ 落到 ④，
压缩触发点（128000 × 0.85 = 108800）因此比应有的早约 2×。

**落点文件 / 位置**

| 位置 | 改动 |
|---|---|
| `~/.yushan/config.json`（**用户级配置，仓库外**） | 写入 `{"windows": {"<model>": <实测可用窗口>}}` —— 走**已有**的用户级覆盖，**不改代码** |
| `apps/coding-agent/src/capabilities.rs` | `/model` 的输出里带上**窗口值**与**来源**（覆盖 / 目录 / 内置表 / 默认值），让「窗口值从哪来」可自证 |
| `apps/coding-agent/src/config.rs` | 暴露「窗口来源」的查询（与 `context_window_for` 的四级同源，不另写一套判定） |

**验收口径**

- 长会话跑到 **150k token** ⇒ **不**在 108800 处被静默压缩（**MANUAL_ACK_REQUIRED**：需真实模型与长会话）；
- `/model` 的输出里能直接读到窗口值与其来源（**可自动化**：单元层断言来源枚举）。

**依赖**：无。

### 切片 0d · 提示词两条 guideline

**目标**

把「长命令走后台 + 收尾合批 + 绝不重复执行」与「不用 `sleep` 假装在等」两条写进提示词。
第一条同时覆盖三件事：后台化、**合批收轮**（一条 assistant 消息 = 一次模型往返，
design.md §3.3 校正 2）、不重复执行；第二条是「别用 `sleep 30` 真睡 30s」。

**落点文件**

| 文件 | 改动 |
|---|---|
| `apps/coding-agent/src/prompt.rs` | `tool_guidelines`（`:188`）里加两条，按工具名派生（`bash_bg` 在场才有第一条，`bash_bg`+`bash_poll` 都在场才有第二条）—— 与既有 `websearch` / `webfetch` / `subagent` 的「在场才出现」守卫同形 |

**验收口径**

- 同批 prompt 对照：**重复执行次数 1 → 0**（**MANUAL_ACK_REQUIRED**：需真实模型）；
- 单元层：`bash_bg` 不在场 ⇒ 两条都不出现；在场 ⇒ 出现（**可自动化**）。

**依赖**：无。

### 切片 0e · debug 度量补齐

**目标**

把 design.md §6.1 的四个可判定指标变成「日志里能直接读出」：

| 指标 | 落点 | 本切片要做的事 |
|---|---|---|
| **重复命令数** | 同一回合内同一条命令串出现 >1 次的次数 | **新增 `command_fp` 字段**（命令串的 `fnv1a64` 指纹；**不落正文** ⇒ 不泄密、不增体积）。⚠ **不能用 `args_original_bytes`** —— 它是**整个参数对象的字节数**，不同命令同长度会**碰撞**；命令串本身在默认 `head` 档被截断 ⇒ 两条不同的长命令会被误判为重复 |
| **真命令耗时** | 单条调用耗时 > 阈值（如 5s）的分布 | 已有：`tool_result.duration_ms`（`apps/coding-agent/src/round_clock.rs`） |
| **按工具名分桶的耗时** | 逐调用耗时按工具名分桶 | 已有、可离线分桶、无需新字段：`tool_call` 带 `gen_ai.tool.name` + `tool_call_id`，`tool_result` 带 `tool_call_id` + `duration_ms` |
| **工具阻塞占比** | `tools_ms` / 回合墙钟 | 已有：`round_end.tools_ms` 与 `turn_end.elapsed_ms` |

**落点文件**

| 文件 | 改动 |
|---|---|
| `apps/coding-agent/src/debug_log.rs` | 生产版 `fnv1a64`（现成实现在 `apps/coding-agent/src/toolset.rs` 内、且是 `#[cfg(test)]` 内的 ⇒ 生产代码要自己放一份）+ `ToolArgsSummary` 增 `command_fp` |
| `apps/coding-agent/src/round_clock.rs` | `tool_call` 记录写入 `command_fp`（**同一 major 内只许加字段** ⇒ `yushan.debug/1` 可直接加，不升版本） |

**验收口径**：日志里能直接读出四个指标 + 按工具名分桶的耗时（**可自动化**：单元层喂两条相同命令与两条不同命令，断言指纹分组正确）。

**依赖**：无。**这是切片 1 的前提** —— 没有它，1 的收益无法证明。

### 切片 0f · 文档同步

**目标**：三处文档口径一致。

**落点文件**

| 文件 | 改动 |
|---|---|
| `AGENTS.md` | 环境变量段登记 `YUSHAN_BG_DIR`（**须绝对路径**，与 `YUSHAN_FETCHED_DIR` 同规矩）—— `AGENTS.md` 是「每一轮都需要」的入口文件，不登记就等于不存在 |
| `docs/design.md` | §12 测试矩阵新增「后台命令执行」条目（对应 design.md §12 的「新模块和接口改动须能对应上测试矩阵条目」） |
| `docs/design-final/coding-agent-tui.md` | 新增 as-built 段落：后台命令执行的实际形态（父表元素数、清理点、`/model` 的窗口来源） |

**验收口径**：三处都能查到同一份数字（**可自动化**：grep 计数断言）。

**依赖**：无。

> **备注（父表元素数的时序）**：本切片**只登记与表述**，**不提前改数字** ——
> 父表 10 元 → 12 元是**切片 1** 落地后的**事实**，文档先于代码改数字会让文档变成假话。
> 数字回写归切片 1 的文档任务（见 exec-plan 的 Task 28）。

### 切片 1 · `bash_bg` + `bash_poll`（文件后端）

**目标**

把「分钟级真命令」从 agent loop 的关键路径上摘下来，摘除的边际开销 ≈ 0（design.md §0）。

**落点文件**

| 文件 | 改动 |
|---|---|
| `adapters/tools-basic/src/background.rs`（**新建**） | 后台命令注册表（**进程级**）：句柄分配、`spawn`、状态快照、输出字节偏移、进程组 kill、输出上限闸门、`sweep()`、`Drop` 兜底。**全部同步实现**（`std::process` + `std::fs`），零 tokio |
| `adapters/tools-basic/src/bg_tools.rs`（**新建**） | `BashBgTool` / `BashPollTool` 两个 `Tool` 实现。唯一用 tokio 的地方是读输出文件（`tokio::fs`） |
| `adapters/tools-basic/src/bg_output.rs`（**新建**） | 头尾混合截断的**组装**（截断**策略**本身仍在 `ys-tool`） |
| `crates/ys-tool/src/truncate.rs` | 新增 `head_tail()`（纯函数，复用既有 `head_lines` 与 `tail`）。**`tail` 本身一字不改** ⇒ `bash` 的输出逐字节不变 |
| `crates/ys-tool/src/lib.rs` | 导出 `head_tail` / `HeadTail` |
| `apps/coding-agent/src/assembly.rs` | 父表插入两个新工具（**在 `subagent` 之前** ⇒ 子表仍是父 JSON 的字节前缀） |
| `apps/coding-agent/src/app_loop.rs` | 六个清理点中的四个：`abort`、回合预算到点、`/new`、`/resume` |
| `apps/coding-agent/src/main.rs` | 清理点之五：**进程退出**（`std::process::exit` 不跑析构 ⇒ `Drop` 兜底在此路径无效） |
| `adapters/tools-basic/src/background.rs` | 清理点之六：`impl Drop` 兜底（panic / unwind） |

**验收口径**（design.md §6.2 前后对照）

- 回合墙钟下降；工具阻塞占比下降；
- **重复命令数 1 → 0**（G1b 的直接判据）；
- **经 `bash` 执行且耗时 > 5s 的调用数 → 0**（主假设的直接判据）；
- `bash_poll` 所在 assistant 消息的调用数 **> 1**（合批判据）；
- 后台命令的**孤儿进程数 = 0**（退出后 `pgrep` 复查）；
- 每次 poll 的结果里都报出 `skipped_bytes`（裁剪时）⇒ **不得静默丢弃**；
- **答案质量**必须同时看（不可只看墙钟）。

**依赖**：**0a–0e**。

### 切片 2 · 协议面 A/B

**目标**：把 G2 的三个候选归因（协议面 / 提示词面 / 服务端排队）**分开**，
而不是直接得出「换协议更快」（design.md §6.3）。

**落点文件**

| 文件 | 改动 |
|---|---|
| `apps/coding-agent/tests/e2e_bench.rs` | 复用既有基准脚手架（`YUSHAN_BENCH_*` 开关、固定提示集）加**协议面**一列：同 prompt、同模型、同 effort 下记录墙钟 / 轮数 / `ttft`·`gen`·`tools` 切分 / 质量 |

**验收口径**：A/B 两组数据齐全且**质量同时记录**（不可只看墙钟）。

**依赖**：无（**独立切片，不阻塞 0/1**）。

> **两条边界**（不得越界）：
> ① **本机无凭证**（`YUSHAN_API_BASE` / `YUSHAN_API_KEY` 未设置）⇒ 本切片的**真实 A/B 判定
>    标 `MANUAL_ACK_REQUIRED`**，不阻塞其它切片；
> ② **不给 `ys-model` 增加 Responses 后端** —— 那涉及 `Model` trait 与流式事件形态，**须过 ADR**
>    （context.md Q5 的裁定），本轮只做 A/B 与口径记录。

---

## 契约面分类

| 改动 | 模型可见 `ToolSpec` | 父表元素数 | 子表元素数 | `bash` 的命令语义 / 输出字节 | 动 `ys-tool` | 动 `ys-loop` | 用户级配置 |
|---|---|---|---|---|---|---|---|
| **0a** | **不变** | 不变 | 不变 | **不变** | **是**（`ToolContext` 加一个只读字段） | **是**（两处构造点各填一处） | 否 |
| **0b** | 不变 | 不变 | 不变 | **不变** | 否 | 否 | 否 |
| **0c** | 不变 | 不变 | 不变 | 不变 | 否 | 否 | **是**（`~/.yushan/config.json` 的 `windows`） |
| **0d** | 不变（改的是**提示词**） | 不变 | 不变 | 不变 | 否 | 否 | 否 |
| **0e** | 不变 | 不变 | 不变 | 不变 | 否 | 否 | 否 |
| **0f** | 不变 | 不变 | 不变 | 不变 | 否 | 否 | 否 |
| **1** | **变**（父表 +2 个新工具） | **10 → 12** | **9（不变）** | **不变**（`bash` 一字不改） | 是（加 `head_tail` 纯函数） | 否 | 否（`YUSHAN_BG_DIR` 是**可选**覆盖） |
| **2** | 不变 | 不变 | 不变 | 不变 | 否 | 否 | 否 |

**三处必须守住的「不变」**（design.md §4.8）：

1. **`bash` 的 spec 一字不改** ⇒ 工具表指纹锚点 `0xeced4483ff29ef22`（`apps/coding-agent/src/toolset.rs`）
   **不动**；`bash` 的输出仍走 `ys_tool::tail` ⇒ **逐字节不变**；
2. **`ys-loop` 的轮次模型零改动** —— 「工具全部完成才回喂」原样保留，
   **未引入「工具未完成」状态**（ADR 2026-10-21 决策 6 显式记账：句柄是代价低一个数量级的等价表达）；
3. **`Tool::timeout()` 不进 `ToolSpec`** ⇒ 新工具的超时声明不污染工具表 JSON 与缓存前缀。

**依赖方向不变**：无新 crate、无新依赖；核心 crate 只改 `ys-tool`（一个只读字段 + 一个纯函数）
与 `ys-loop`（两处构造点）。

---

## 明确不做的事（design.md §8，原样带过）

| 不做 | 理由 |
|---|---|
| **通知注入**（后台完成自动进对话） | 需要 loop 有「回合内非工具来源消息」的通道 ⇒ 架构级改动，**触碰「事件只观察，Hook 才控制流程」的硬边界**。成本差一个数量级 |
| **重开「减少轮数」** | `explore-speed` 已有成本模型与推理强度 A/B（ADR-0042 定 `high` 默认） |
| **提高只读并发度** | 通道已通电，本地只读工具是毫秒项，天花板 ≈ 0 |
| **改推理强度默认值** | ADR-0042 已定 `high`；`explore-speed` 已做过 `low`/`high` A/B |
| **把后台能力做成通用 trait 平面**（候选 B） | 第一个消费者只有一个；核心 crate 改动 4 → 2 的收益不抵 450–550 行 |
| **`expected_seconds`（模型自报时长）** | 模型的时长估计不可靠 |
| **硬拒绝 `sleep`** | 会改 `bash` 的既有行为；本轮只做 guideline，硬拒绝另开一刀 |
| **放宽输出上限 / 改 `ys-loop` 轮次模型 / 加沙箱 / 引新依赖** | 硬约束 |
| **给 `ys-model` 增加 Responses 后端** | 涉及 `Model` trait 与流式事件形态，**须过 ADR**（context.md Q5 裁定）；切片 2 只做 A/B |

## 残留风险（design.md §7，原样带过）

1. **模型忘记轮询 ⇒ 结果丢失。** 缓解：`bash_poll` 返回里带路径与句柄、guideline 要求收尾前取回、
   回合末在日志里记「还有 N 个后台任务在跑」（日志，不进模型上下文）。**根治需要通知注入，本轮不做。**
2. **后台任务与后续工具调用并发写同一工作区 ⇒ 竞态。** 绕过了「同批串行」闸门 ——
   那道闸门只管**工具调用之间**，管不住**进程存活期间**。只能靠提示词约束。
3. **轮询若不合批 ⇒ 净收益可能为负。** 每次轮询付一次模型往返（17.3s 量级）。
   缓解是 guideline，但**不可保证** ⇒ 前后对照把「合批」列为独立判据。
   `wait_seconds` 同理：上限已从 60s 收到 20s，**但模型仍可传 20** ⇒ 最坏情况一轮里多等 20s。
4. **句柄表占满 ⇒ 模型被卡住。** 满了先淘汰最旧的**已完成**条目；若全在跑 ⇒ 报错。
5. **脱离进程组的子进程逃逸。** 进程组清理覆盖不到自行 `setsid` / daemonize 的命令。
6. **后台输出是 stdout+stderr 的合并流。** 既有 `bash` 分开收再拼；后台形态下两者按写入顺序交错
   ⇒ 模型**无法区分来源**。这是后台形态**必须接受**的差异（已写进契约）。
7. **头尾裁剪会跳过中间段。** 缓解是「显式报出跳过量 + 给完整路径」⇒ 模型用 `grep` / `read` 补齐；
   但**模型是否真的去补**取决于它读不读那行提示 —— 这是提示词问题，无机制性解法。

---

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。
> 下表的 `T1`…`T14` 是**验收项编号**；exec-plan 的 `Task N` 是**任务编号**，两套编号不通用。

#### T1: D1 · 预算进工具且工具内始终有 deadline
- **验收方法**: 单测断言 `ToolContext::new(...)` 的 `tool_budget` 为 `None`；
  `with_tool_budget` 后为 `Some`；`ys-loop` 两条路径构造出的 `ToolContext` 各自带**本工具**的预算
  （`tool.timeout()` → `limits.bash_timeout` → 300s 三级链）。
- **优先级**: 必测
- **验收人**: review subagent

#### T2: D1 · 钳位严格小于外层且带余量
- **验收方法**: 传 `timeout: 900` + 外层 300s ⇒ 生效 deadline 为 **295s** 量级（**严格小于** 300s）；
  超时文案写**生效值**（不是 900）。变异：把 `min` 改成取模型值 ⇒ 该断言变红。
- **优先级**: 必测
- **验收人**: review subagent

#### T3: D1 · 不传 timeout 也被砍且进程组被杀
- **验收方法**: 不传 `timeout`，外层预算到点 ⇒ 工具返回超时错误（**不是**被外层 drop），
  且**孙进程**不存在。断言依据是进程不存在，**不是**错误文案。
- **优先级**: 必测
- **验收人**: review subagent

#### T4: D2 · 无 boundary 路径超时不留孤儿
- **验收方法**: `bash("sleep 60 & wait", timeout 1)` ⇒ 断言 `sleep` 进程**不存在**（用 `pgrep` 式的进程存在性检查，
  不看错误文案）。变异：把无 boundary 分支的进程组杀去掉 ⇒ 该断言变红。
- **优先级**: 必测
- **验收人**: review subagent

#### T5: 窗口声明可自证
- **验收方法**: `/model` 的输出里能读到**窗口值**与**来源**（四级之一）；
  用户级 `config.json` 的 `windows` 覆盖生效 ⇒ 来源标为「用户覆盖」且值等于配置值。
- **优先级**: 必测
- **验收人**: review subagent

#### T6: 长会话不再被过早压缩
- **验收方法**: 需真实模型与长会话：跑到 150k token ⇒ **不**在 108800 处被静默压缩。
  **本机无凭证 ⇒ MANUAL_ACK_REQUIRED。**
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

#### T7: 两条 guideline 按工具名派生
- **验收方法**: `bash_bg` 不在场 ⇒ 两条 guideline **都不出现**；在场 ⇒ 出现。
  两条都只对**父表**生效（子表 9 元不含 bg 工具 ⇒ 子 agent 看不到）。
- **优先级**: 必测
- **验收人**: review subagent

#### T8: `command_fp` 能区分同命令与异命令
- **验收方法**: 同一条命令串两次 ⇒ 指纹**相同**；两条**不同长度**的命令 ⇒ 指纹**不同**
  （这正是 `args_original_bytes` 会碰撞而 `command_fp` 不会的场景）。
  指纹**不落命令正文**（记录里只有指纹，没有命令串）。
- **优先级**: 必测
- **验收人**: review subagent

#### T9: 四个指标可从日志直读
- **验收方法**: 合成一轮含 2 次相同命令 + 1 次不同命令 + 1 个 >5s 调用的日志 ⇒
  重复命令数 = 1、真命令耗时分布可读、按工具名分桶的耗时可读、工具阻塞占比可算。
- **优先级**: 必测
- **验收人**: review subagent

#### T10: `bash_bg` / `bash_poll` 的契约
- **验收方法**: 两个工具的 `spec` 逐字断言（名字、描述、`required`）；两者 `is_concurrency_safe() == false`；
  `bash_bg` 启动失败**同步报错且不产生条目**；`bash_poll` 对未知句柄返回**明确错误**；
  退码 ≠ 0 ⇒ `is_error == true` 且**退码原样带出**；被 harness 杀掉 ⇒ 措辞**与**非零退码**不同**。
- **优先级**: 必测
- **验收人**: review subagent

#### T11: 交付语义三条 + 完整行边界
- **验收方法**: ① 已送达字节**不重复送达**；② 未送达字节**必须明示**（`skipped_bytes` 报出 + 完整输出路径）；
  ③ **不静默丢弃**。另：读**只到最后一个完整行**为止 ⇒ `consumed` 停在完整行边界，不切半行、不切多字节字符。
- **优先级**: 必测
- **验收人**: review subagent

#### T12: 六个清理点
- **验收方法**: `abort` / 回合预算到点 / `/new` / `/resume` / 进程退出 / panic 兜底，
  每个清理点都杀**本会话**的进程组且**不误杀别的会话**（`kill_all(session)` 按会话过滤）。
  另：清理后 `pgrep` 复查 ⇒ 孤儿进程数 = 0。
- **优先级**: 必测
- **验收人**: review subagent

#### T13: 前后对照（切片 1 的收益）
- **验收方法**: 同一批真实 prompt，改动前后各跑一遍，逐个进程跑，记录：回合墙钟 / 工具阻塞占比 /
  **重复命令数（1 → 0）** / **经 `bash` 且 > 5s 的调用数（→ 0）** / **`bash_poll` 所在消息的调用数（> 1）** /
  **孤儿进程数（= 0）** / **未送达字节报出** / **答案质量**。
  **需真实模型 ⇒ MANUAL_ACK_REQUIRED。** ⚠ **基线必须在改动之前采**（design.md §6 首句）。
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

#### T14: 协议面 A/B
- **验收方法**: 同模型、同 effort、同提示，逐个进程跑，对比墙钟 / 推理 token / 吞吐 / **轮数** / 质量。
  **本机无凭证 ⇒ MANUAL_ACK_REQUIRED；不阻塞其它切片。**
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

### 测试框架与命令

- 全量：`cargo test --workspace`
- 静态：`cargo clippy --all-targets`
- 定向：`cargo test -p ys-tool -p ys-loop -p ys-tools-basic -p ys-coding-agent`
- 格式化：**不要裸跑 `cargo fmt`**（本机 rustfmt 与仓库 CJK 文本有既有分歧）；
  只对改动文件跑 `rustfmt --edition 2024 <file>`。

---

## 涉及文件

- **新增**：`adapters/tools-basic/src/{background.rs,bg_tools.rs,bg_output.rs,proc_group.rs}`
- **修改（核心 crate）**：`crates/ys-tool/src/{context.rs,truncate.rs,lib.rs}`、`crates/ys-loop/src/basic.rs`
- **修改（adapter）**：`adapters/tools-basic/src/{bash.rs,exec.rs,lib.rs}`
- **修改（app）**：`apps/coding-agent/src/{capabilities.rs,config.rs,prompt.rs,debug_log.rs,round_clock.rs,assembly.rs,app_loop.rs,main.rs}`
- **修改（文档）**：`AGENTS.md`、`docs/design.md`、`docs/design-final/coding-agent-tui.md`
- **不动**：`bash` 的 `ToolSpec` 与输出字节；`ys-loop` 的轮次模型；`ys-event` / `ys-protocol` / `ys-core` /
  `ys-model` / `ys-session` / `ys-component` / `ys-runtime` / `ys-tui-coding`；`capabilities.rs` 的 print 禁令。

## 波及文档

- `AGENTS.md`（`CLAUDE.md` 是软链）：环境变量段登记 `YUSHAN_BG_DIR`。
- `docs/design.md` §12：新增「后台命令执行」测试矩阵条目；Coding Agent 行的父/子表元素数随切片 1 更新。
- `docs/design-final/coding-agent-tui.md`：新增后台命令执行的 as-built 章节。
- `docs/TODO.md`：完成即删条目（若本轮登记了待办）。

## 风险与注意事项

- **测试隔离**：临时目录名必须含**进程内原子序号 + `process::id()`**；进程级 env 改写必须走
  `crate::test_env::env_lock()` + `EnvRestore`（RAII 恢复）。后台命令的落盘目录同理
  （`<bg_root>/<session_id>/bg<N>.log`）—— 名字必须含会话 id 与句柄，避免并发互删。
- **UTF-8 边界**：轮询读**只到最后一个完整行**为止（AGENTS.md「流式/字节路径注意 UTF-8 边界」）。
- **`kill_on_drop` 只是兜底，不是契约**：0a/0b 之后，所有超时路径都必须由**工具自己的分支**杀进程组。
- **pid 复用窗口**：**必须先杀后收**（先 `kill` 再 `try_wait` 收尸）—— 反了会打到无关的进程组。
  这条告诫从 `bash.rs` 搬进 `proc_group.rs`，两份实现不得漂移。
- **`--json` / `-p` 是加法契约**：新增工具不进事件枚举；debug 日志同一 major 内只许加字段。
- **变异测试要求**（AGENTS.md 硬性要求）：D1 / D2 / `command_fp` / 交付语义 / 清理点各至少一条变异用例。
- **`std::process::exit` 不跑析构**：进程退出路径必须**显式**清理，不能指望 `Drop`。
- **`capabilities.rs` 绝不 print**：新增的窗口来源信息走 `Outbound::Output` → transcript。

---

## 后续

1. 本计划交 exec-plan 落地切片 **0a–0f、1、2**。
2. 切片 0e 的度量落地后，用它**重采基线**（多会话、多 prompt），再动切片 1（design.md §9 后续 2）。
3. 若切片 1 的收益被验收证伪，**回退成本很低** —— 因为 `bash` 一字未改，新工具可整组摘掉而不留残余。
