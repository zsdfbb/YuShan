# 后台命令执行 + 两条既存缺陷修正 执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。
> **依赖声明是并发调度的依据** — Phase 3 据此合并执行单元并划分波次。
> 下文的 `Task N` 是**任务编号**；`T1`–`T14` 是设计计划里的**验收项编号**，两套编号不通用。
>
> **配对纪律**：每个 `type=impl` 任务都有一个配对的 `type=test` 任务（**同一改动的测试**），
> 二者**同属一个执行单元**（见「波次计划」的合并表）—— impl 与 test 不得被分到不同波次。

## 上下文引用

参考设计计划：`docs/design-plans/2026-10-21-background-command.md`
设计来源：`docs/arch/execution-efficiency/design.md`（**§4.9 切片表**、§5 D1/D2、§6 验收口径、§7 残留风险、§8 不做的方向）
决策记录：`docs/arch/execution-efficiency/adr-background-command-execution.md`（含 2026-10-21 定稿的三条架构级决策）
现状/约束：`docs/arch/execution-efficiency/context.md`
评审处置：`docs/arch/execution-efficiency/review.md`（R1–R18 **已全部处置**）

## 基线

- **基线 ref**: `abdcb057b5e75bca564ea9ed8c6bc98176481435`（当前 HEAD）
- **工作树状态**：基线时**干净**（仅本计划的两个新文件未跟踪）。
  开工前先确认 `git status --short`；Phase 3 review 用 `git diff <基线 ref> -- <files>` 取字节级证据，
  **须把新文件（`??`）一并纳入**。
- **本机无凭证**：`YUSHAN_API_BASE` / `YUSHAN_API_KEY` **未设置**
  ⇒ 一切**需要真实模型**的验收一律标 `MANUAL_ACK_REQUIRED`，**不阻塞**其它任务。

## 切片覆盖表（design.md §4.9 → 任务）

| 切片 | 任务 | 依赖 | 验收口径（design.md §4.9 原句） |
|---|---|---|---|
| **0a** 修 D1 | Task 1, Task 2, Task 3, Task 4, Task 7, Task 8 | 无 | ① 传 `timeout: 900` ⇒ ~295s 被砍、文案写**生效值**、**进程组被杀**；② **不传** `timeout` ⇒ 外层预算到点**进程组被杀**（不靠 `kill_on_drop`） |
| **0b** 修 D2 | Task 5, Task 6 | 无 | `bash("sleep 60 & wait", timeout 1)` ⇒ **孙进程**不在了 |
| **0c** 窗口声明 | Task 9, Task 10 | 无 | 长会话跑到 150k token ⇒ **不**在 108800 处被静默压缩（**MANUAL_ACK_REQUIRED**） |
| **0d** 提示词两条准则 | Task 11, Task 12 | 无 | 同批 prompt 对照：重复执行次数 1 → 0（**MANUAL_ACK_REQUIRED**） |
| **0e** debug 度量补齐 | Task 13, Task 14 | 无 | 日志里能直接读出三个指标 + **按工具名分桶**的耗时 |
| **0f** 文档同步 | Task 15, Task 16（+ Task 30, Task 31 回写父表数字） | 无 | 三处都能查到同一份数字 |
| **1** `bash_bg` + `bash_poll` | Task 17 – Task 29, Task 30, Task 31 | **0a–0e** | §6 的前后对照（**MANUAL_ACK_REQUIRED**；代码级验收见 Task 29） |
| **2** 协议面 A/B | Task 32, Task 33 | 无 | 把 G2 的三个候选归因**分开**（**MANUAL_ACK_REQUIRED**；本机无凭证，**不阻塞**其它切片） |

## 任务清单

### Task 1: crates/ys-tool — `ToolContext` 加 `tool_budget` 字段 [type=impl]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/context.rs`
- **描述**:
  - `ToolContext` 加 `pub tool_budget: Option<Duration>` —— **第 4 个可选挂载点**
    （编号按「可选挂载点」计，现有 3 个：`boundary` / `summarizer` / `subagent`）。
    `Option<Duration>` 是 `Copy` ⇒ 无生命周期负担，与既有三个挂载点**同形**。
  - `ToolContext::new` 的默认值 `None`；新增链式构造器 `with_tool_budget(mut self, budget: Duration) -> Self`
    （照 `with_summarizer` / `with_subagent` 的先例）。**既有 `new` 的签名与调用点一字不改。**
  - `ToolContext` 是 `#[non_exhaustive]` ⇒ 加字段**不破坏**外部实现。
  - **不进 `ToolSpec`** ⇒ 工具表指纹与父表前缀都不受影响（`Tool::timeout()` 同样不进 `ToolSpec`）。
- **依据**: design.md §5 D1 第 1 步、§4.4（`ToolContext` 字段草案与「为什么是它而不是工具里缓存预算」）、
  §3.5 修正 6；review.md R3（核心 crate 改动重新记账为 4 → 2）。
- **验收命令**:
  - `cargo build -p ys-tool` — 期望通过
  - `cargo test -p ys-tool` — 期望全部 passed（Task 2 的用例在本任务后即可跑）
- **关联测试任务**: Task 2
- **依赖**: 无

### Task 2: crates/ys-tool — `ToolContext::tool_budget` 单测 [type=test]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/context.rs`（`#[cfg(test)]`）
- **描述**:
  - `ToolContext::new(...)` ⇒ `tool_budget == None`。
  - `with_tool_budget(Duration::from_secs(295))` ⇒ `Some(295s)`。
  - 挂载后 `boundary` / `cwd` / `workspace_root` / `summarizer` / `subagent` **均不受影响**。
  - 变异：把 `with_tool_budget` 改成不写字段 ⇒ 第二条断言变红。
- **验证方法**:
  - `cargo test -p ys-tool` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 1
- **依赖**: Task 1

### Task 3: crates/ys-loop — 两处 `ToolContext::new` 填外层预算 [type=impl]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`
- **描述**:
  - **两处构造点都要填**（design.md §3.5 修正 6 的核实补记）：
    并行路径 `crates/ys-loop/src/basic.rs:666`、串行路径 `crates/ys-loop/src/basic.rs:716`。
  - 各填**各自的**工具预算：`tool_budget(tool, &ctx.limits)`（三级链：工具声明 `tool.timeout()`
    → `limits.bash_timeout` → 300s）。语义：**这是外层预算**（loop 的 `tokio::time::timeout` 用它），
    工具**自己**再钳一层（Task 7 / 切片 0a 的第 2 步）。
  - **轮次模型不动**：不碰 Step 9 的编排、并行准入与保序逻辑，只多填一个字段。
  - 顺带把「构造 `ToolContext`」抽成一个小函数（便于 Task 4 直接单测），**不改变**既有行为。
- **依据**: design.md §3.5 修正 6（两处构造点）、§4.4、§4.8（「`ys-loop` 填一处」的更新口径）、
  §5 D1 第 1 步。
- **验收命令**:
  - `cargo build -p ys-loop` — 期望通过
  - `cargo test -p ys-loop` — 期望全部 passed（既有用例零改动通过）
- **关联测试任务**: Task 4
- **依赖**: Task 1

### Task 4: crates/ys-loop — 预算传递单测 [type=test]
- **涉及目录**: `crates/ys-loop/src/`
- **涉及文件**: `crates/ys-loop/src/basic.rs`（`#[cfg(test)]`）
- **描述**:
  - 三级链：`tool.timeout()` 优先 → `limits.bash_timeout` → 300s（**不取 min** —— 照既有 `tool_budget` 的语义）。
  - 构造出的 `ToolContext` 带的是**本工具**的预算（不同工具的预算不同）。
  - **源码级断言**（钉住「两处都要填」）：`include_str!("basic.rs")` 里 `with_tool_budget`
    出现次数 **≥ 2**（并行 + 串行各一处）。
  - 变异：把并行路径的 `with_tool_budget` 删掉 ⇒ 计数断言变红。
- **验证方法**:
  - `cargo test -p ys-loop` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 3
- **依赖**: Task 1, Task 3

### Task 5: adapters/tools-basic — 进程组内核独立成模块 + 无 boundary 路径补杀（D2） [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/proc_group.rs`（**新建**）, `adapters/tools-basic/src/bash.rs`,
  `adapters/tools-basic/src/exec.rs`, `adapters/tools-basic/src/lib.rs`
- **描述**:
  - **新建** `proc_group.rs`：`pub(crate) fn kill_group(pid: u32)` —— **同步**内核
    （`sh -c "kill -9 -{pid}"`，stdin/stdout/stderr 全 null）。
  - 把 `bash.rs` 现有的 `kill_process_group`（`adapters/tools-basic/src/bash.rs:167`）改为**委托**该内核
    —— **保留原 `async fn` 签名与语义** ⇒ `bash` 的既有行为逐字节不变。
  - 把现居 `adapters/tools-basic/src/bash.rs:157-166` 的两条告诫（「**必须先杀后收**」「pid 复用窗口」）
    **搬进** `proc_group.rs` 的 doc comment ⇒ `bash.rs` 与后台模块共用同一份内核与同一条告诫，
    两份实现不会漂移。
  - **D2 修法**：`bash.rs` 的**无 boundary 路径**超时分支（`adapters/tools-basic/src/bash.rs:265-268`
    的空 `#[cfg(unix)]` 块）补上**同款进程组杀**。
  - `exec.rs` 的 `use crate::bash::kill_process_group` 改指新模块（**行为不变**）。
  - `lib.rs` 加 `pub mod proc_group;`。
- **依据**: design.md §5 D2（修法 + 四条组合矩阵）、§4.2（`proc_group.rs` 改动行）、
  ADR 2026-10-21 决策 5（进程组内核独立成模块）；review.md R7（先杀后收）。
- **验收命令**:
  - `cargo build -p ys-tools-basic` — 期望通过
  - `cargo test -p ys-tools-basic` — 期望全部 passed（既有 `bash` 用例零改动通过）
- **关联测试任务**: Task 6
- **依赖**: 无

### Task 6: adapters/tools-basic — 无 boundary 路径超时杀进程组单测 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bash.rs`（`#[cfg(test)]`）
- **描述**:
  - `bash("sleep 60 & wait", timeout 1)` ⇒ 断言**孙进程**（`sleep`）**不存在**
    —— 判据是**进程存在性**（`pgrep` 式检查），**不是**错误文案。
  - 有 boundary 路径同样断言（对照两路同款内核）。
  - 变异：把无 boundary 分支的进程组杀去掉 ⇒ 第一条断言变红；
    把 `kill_on_drop(true)` 去掉 ⇒ `bash.rs` 既有 sentinel 用例变红。
  - **测试隔离**：sentinel 路径名含**进程内原子序号 + `process::id()`**（AGENTS.md「测试隔离」）。
  - ⏱ **时间预算**：本用例用 `timeout 1` ⇒ 约 1–2s 挂钟，不得用更长的 sleep。
- **验证方法**:
  - `cargo test -p ys-tools-basic bash` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 5
- **依赖**: Task 5


### Task 7: adapters/tools-basic — `bash.rs` 工具内 deadline 与超时文案（D1 第 2/3 步） [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bash.rs`
- **描述**:
  - **工具内始终有 deadline**：`effective = min(模型值, 外层预算 − Δ)`；
    **模型未传 `timeout` 时取 `外层预算 − Δ`**（不是「没有 deadline」）。
    `Δ` 取 **5s** 量级 ⇒ 内层**严格早于**外层到期（不会出现同刻竞态）。
  - 外层预算从 `ctx.tool_budget` 取（Task 1 新增的字段）；`ctx.tool_budget == None`
    ⇒ 沿用既有语义（只受模型值约束）。
  - **外层预算小于 `Δ` 时**（测试里用小预算会撞上）⇒ 钳到一个**最小正秒数**，
    不得算出 0 或负数。
  - **两条路径（有 / 无 boundary）用同一份生效值**。
  - **文案写生效值**：`Command timed out after <effective> seconds` ——
    不是模型传的值，也不是 loop 的 `Tool '<name>' timed out after <budget>`。
  - `bash` 的 `ToolSpec` 与 `description` **一字不改**（工具表指纹锚点不动）。
- **依据**: design.md §5 D1 第 2/3 步、§3.5 修正 4（deadline 的归属：工具内**始终**有）
  与修正 5（钳位必须**严格小于**外层且留余量）。
- **验收命令**:
  - `cargo build -p ys-tools-basic` — 期望通过
  - `cargo test -p ys-tools-basic bash` — 期望全部 passed（Task 10 的用例）
- **关联测试任务**: Task 8
- **依赖**: Task 1, Task 5

### Task 8: adapters/tools-basic — deadline 钳位与超时文案单测 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bash.rs`（`#[cfg(test)]`）
- **描述**:
  - 传 `timeout: 900` + 外层 300s ⇒ 生效 deadline 为 **295s** 量级（**严格小于** 300s）；
    超时文案写**生效值**（**不是** 900）。
  - **不传** `timeout` + 外层 300s ⇒ **仍有 deadline**（≈295s），不是「无 deadline」。
  - `ctx.tool_budget == None` ⇒ 只有模型值约束（沿用既有语义）。
  - 外层预算 < `Δ` ⇒ 生效值为**最小正秒数**（不出现 0 / 负数）。
  - 变异：把 `min` 改成取模型值 ⇒ 第一条断言变红；
    删掉「未传值时取外层 − Δ」那一支 ⇒ 第二条断言变红。
  - ⏱ **时间预算**：**不要真的等 295s** —— 断言落在**算出来的生效值**上
    （小预算 + 读取超时文案里的秒数），单条用例挂钟 ≤ 2s。
- **验证方法**:
  - `cargo test -p ys-tools-basic bash` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 7
- **依赖**: Task 1, Task 5, Task 7

### Task 9: apps/coding-agent — `/model` 显示窗口值与来源（G3） [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/config.rs`, `apps/coding-agent/src/capabilities.rs`
- **描述**:
  - `config.rs`：新增「窗口来源」查询（与 `context_window_for` 的**四级同源**：
    ① `~/.yushan/config.json` 的 `context_window` 覆盖 > ② provider 目录的 `context_window` >
    ③ 内置兜底表 > ④ 默认 128000）。**不另写一套判定** —— 两份判定会漂移 ⇒ 窗口值与来源可能对不上。
  - `capabilities.rs::set_model`：输出里追加窗口值与**来源**（走 `Outbound::Output` → transcript，
    **绝不 print**）。文案示例：`Context window: 258400 (source: user override)`。
  - **用户级配置（用户动作，仓库外）**：在 `~/.yushan/config.json` 写
    `{"context_window": {"<model>": <实测可用窗口>}}`（用户级配置 JSON 的键是 `context_window`；内部 Rust 字段名才叫 `windows`）—— 走**已有**的覆盖机制，**不改代码**。
- **依据**: design.md §4.9 切片 0c（「并把『窗口值从哪来』写进 `/model`」）；
  `apps/coding-agent/src/config.rs` 的四级解析（① 覆盖 > ② 目录 > ③ 内置表 > ④ 默认）。
- **验收命令**:
  - `cargo build -p ys-coding-agent` — 期望通过
  - `cargo test -p ys-coding-agent config` — 期望全部 passed（Task 10 的用例）
- **关联测试任务**: Task 10
- **依赖**: 无

### Task 10: apps/coding-agent — 窗口值与来源单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/config.rs`（`#[cfg(test)]`）, `apps/coding-agent/src/capabilities.rs`（`#[cfg(test)]`）
- **描述**:
  - 四级来源各一条断言：用户覆盖 / provider 目录 / 内置兜底表 / 默认 128000；
    **窗口值与来源必须同时正确**（不是只测其一）。
  - `set_model` 的输出含窗口值与来源；换模型后来源跟着变。
  - 变异：把来源硬编码成「user override」⇒ 后三条断言变红。
  - **进程级 env 改写**必须走 `crate::test_env::env_lock()` + `EnvRestore`（AGENTS.md「测试隔离」）。
  - ⚠ **「长会话跑到 150k token ⇒ 不在 108800 处被静默压缩」**需真实模型与长会话
    ⇒ 标 **MANUAL_ACK_REQUIRED**，不阻塞本任务。
- **验证方法**:
  - `cargo test -p ys-coding-agent config` — 期望全部 passed
  - `cargo test -p ys-coding-agent capabilities` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 9
- **依赖**: Task 9

### Task 11: apps/coding-agent — 提示词两条 guideline [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`
- **描述**:
  - 在 `tool_guidelines`（`apps/coding-agent/src/prompt.rs:188`）里加**两条** guideline，
    文本照 design.md §4.6 的逐字稿：
    ① 长命令走 `bash_bg`、收尾时**与其它调用同一条消息**收、**绝不重复执行**
    （完整输出在磁盘上，要哪段 `grep` 哪段）；② **不用 `sleep` 假装在等**。
  - **按工具名派生**（守卫形态与既有 `websearch` / `webfetch` / `subagent` 的「在场才出现」同形）：
    `bash_bg` 在场才有第一条；`bash_bg` + `bash_poll` 都在场才有第二条。
  - **两条只对父表生效** —— 子表 9 元不含 bg 工具 ⇒ 子 agent 天然看不到。
  - `bash` 的 `ToolSpec` 与 `description` **一字不改** ⇒ 工具表指纹锚点不动。
- **依据**: design.md §4.6（两条 guideline 的逐字文本与「只对父表生效」）、
  §4.9 切片 0d、§3.3 校正 2（合批才是杠杆，不是并发闸门）。
- **验收命令**:
  - `cargo build -p ys-coding-agent` — 期望通过
  - `cargo test -p ys-coding-agent prompt` — 期望全部 passed（Task 12 的用例）
- **关联测试任务**: Task 12
- **依赖**: 无

### Task 12: apps/coding-agent — guideline 单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`（`#[cfg(test)]`）
- **描述**:
  - `bash_bg` 不在场 ⇒ 两条 guideline **都不出现**；在场 ⇒ 按序出现。
  - 只有 `bash_bg` 没有 `bash_poll` ⇒ 第二条不出现。
  - 变异：把守卫从 `tools.contains(&"bash_bg")` 改成无条件 ⇒ 「缺席不出现」断言变红。
- **验证方法**:
  - `cargo test -p ys-coding-agent prompt` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 11
- **依赖**: Task 11

### Task 13: apps/coding-agent — debug 度量补齐（新增 `command_fp`） [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/debug_log.rs`, `apps/coding-agent/src/round_clock.rs`
- **描述**:
  - `debug_log.rs`：加**生产版** `fnv1a64`（现成实现在 `apps/coding-agent/src/toolset.rs:546`，
    但它在 `#[cfg(test)]` 内 ⇒ 生产代码要自己放一份，**不引 `sha2`**）；
    `ToolArgsSummary` 增 `command_fp: Option<u64>` —— **只对命令类工具**算（`bash` / `bash_bg`），
    其余工具为 `None`。
  - `round_clock.rs`：`tool_call` 记录写入 `command_fp`（`yushan.debug/1` **同一 major 内只许加字段**
    ⇒ 不升版本、不改既有键）。
  - **只落指纹、不落命令正文** ⇒ 不泄密、不增体积。
  - ⚠ **不能用 `args_original_bytes`** —— 它是**整个参数对象的字节数**，不同命令同长度会**碰撞**；
    命令串本身在默认 `head` 档被截断 ⇒ 两条不同的长命令会被误判为重复。
  - 其余三个指标**已有、无需新字段**：`tool_result.duration_ms`、`round_end.tools_ms`、
    `turn_end.elapsed_ms`；「按工具名分桶的耗时」靠 `tool_call` 的 `gen_ai.tool.name` +
    `tool_call_id` 与 `tool_result` 的 `tool_call_id` + `duration_ms` **离线关联**得到。
- **依据**: design.md §6.1（四个指标与「重复命令数」的落点）、§4.9 切片 0e；review.md R14 / R18。
- **验收命令**:
  - `cargo build -p ys-coding-agent` — 期望通过
  - `cargo test -p ys-coding-agent debug_log` — 期望全部 passed（Task 14 的用例）
- **关联测试任务**: Task 14
- **依赖**: 无

### Task 14: apps/coding-agent — `command_fp` 与指标可读性单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/debug_log.rs`（`#[cfg(test)]`）, `apps/coding-agent/src/round_clock.rs`（`#[cfg(test)]`）
- **描述**:
  - 同一条命令串两次 ⇒ 指纹**相同**；两条**不同长度**的命令 ⇒ 指纹**不同**
    （这正是 `args_original_bytes` 会碰撞、`command_fp` 不会的场景）。
  - 记录里**只有指纹、没有命令正文**；非命令类工具的 `command_fp` 为 `None`。
  - 合成一轮（2 次相同命令 + 1 次不同命令 + 1 个慢调用）⇒ **重复命令数 = 1** 可从日志算出，
    真命令耗时分布、按工具名分桶的耗时、工具阻塞占比**四项可读**。
  - 变异：把 `command_fp` 改成 `args_original_bytes` ⇒ 「不同长度异串异指纹」断言变红。
- **验证方法**:
  - `cargo test -p ys-coding-agent debug_log` — 期望全部 passed
  - `cargo test -p ys-coding-agent round_clock` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 13
- **依赖**: Task 13

### Task 15: 文档 — 切片 0f 文档同步 [type=impl]
- **涉及目录**: `docs/` + 仓库根
- **涉及文件**: `AGENTS.md`, `docs/design.md`, `docs/design-final/coding-agent-tui.md`
- **描述**:
  - `AGENTS.md` 的**环境变量段**登记 `YUSHAN_BG_DIR`（**须绝对路径**，与 `YUSHAN_FETCHED_DIR` 同规矩；
    未设置时的默认目录一并写明）。`AGENTS.md` 是「每一轮都需要」的入口文件 ⇒ 不登记就等于不存在。
  - `docs/design.md` §12 测试矩阵**新增一行**「后台命令执行」（对应 AGENTS.md「新模块和接口改动须能对应上
    `docs/design.md` §12 测试矩阵中的条目」）。
  - `docs/design-final/coding-agent-tui.md` 新增 as-built 段落：父/子表元素数、六个清理点、
    `/model` 的窗口来源、两个新工具的形态。**只写已成立的事实**，切片 1 未落地的部分标「待落地」。
  - ⚠ **不提前改父表数字**：父表 10 元 → 12 元是**切片 1** 落地后的事实，数字回写归 Task 30。
- **依据**: design.md §4.9 切片 0f、§4.8（既有断言与文档）、ADR §7；review.md R8 / R11。
- **验收命令**:
  - `grep -n 'YUSHAN_BG_DIR' AGENTS.md` — 期望**恰好 1 处**且带「绝对路径」说明
  - `grep -n '后台命令' docs/design.md` — 期望 §12 里有对应条目
- **关联测试任务**: Task 16
- **依赖**: 无

### Task 16: 文档 — 三处一致性核对 [type=test]
- **涉及目录**: `docs/` + 仓库根
- **涉及文件**: `AGENTS.md`, `docs/design.md`, `docs/design-final/coding-agent-tui.md`
- **描述**:
  - 三条 grep 断言：
    ① `YUSHAN_BG_DIR` 在 `AGENTS.md` 出现**恰好 1 次**且说明「绝对路径」；
    ② `docs/design.md` §12 有「后台命令执行」条目；
    ③ `docs/design-final/coding-agent-tui.md` 有对应的 as-built 段。
  - ④ **父表元素数在两处同值**（本任务执行时两处都应是 **10**；切片 1 后由 Task 31 改成 **12**）。
- **验证方法**:
  - `grep -c 'YUSHAN_BG_DIR' AGENTS.md` — 期望 `1`
  - `grep -c '后台命令' docs/design.md` — 期望 `≥ 1`
  - `grep -c '后台命令' docs/design-final/coding-agent-tui.md` — 期望 `≥ 1`
- **关联 impl**: Task 15
- **依赖**: Task 15

### Task 17: crates/ys-tool — `head_tail` 纯函数 [type=impl]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/truncate.rs`, `crates/ys-tool/src/lib.rs`
- **描述**:
  - `truncate.rs` 新增 `pub struct HeadTail { head, tail, omitted_lines, omitted_bytes, total_lines, shown_lines }`
    与 `pub fn head_tail(text: &str, max_lines: usize, max_bytes: usize, head_bytes: usize) -> HeadTail`
    —— 复用既有 `head_lines`（`crates/ys-tool/src/truncate.rs:75`）与 `tail`（`:131`）。
  - **字节预算与行数预算都不放宽**（`MAX_BYTES = 50 KiB`、`MAX_LINES = 2000`）。
  - **`tail` 本身一字不改** ⇒ `bash` 的输出逐字节不变（`bash` 仍走 `tail`）。
  - `lib.rs` 按既有导出清单的形态导出 `head_tail` / `HeadTail`。
- **依据**: design.md §4.4（`HeadTail` 草案）、§4.2（改动表）、§4.3 原则 2（截断方向与流式化冲突 ⇒ 头尾混合）。
- **验收命令**:
  - `cargo build -p ys-tool` — 期望通过
  - `cargo test -p ys-tool` — 期望全部 passed（既有 `tail` 用例零改动通过）
- **关联测试任务**: Task 18
- **依赖**: 无

### Task 18: crates/ys-tool — `head_tail` 单测 [type=test]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/truncate.rs`（`#[cfg(test)]`）
- **描述**:
  - 短文本 ⇒ `omitted_lines == 0`、`omitted_bytes == 0`、头尾拼起来等于原文。
  - 超长文本 ⇒ 头 1/4 + 尾 3/4 的预算切分生效；`omitted_bytes` = **源文本中未出现在展示内容里的字节数**
    （含头尾之间的换行符与源文本尾随换行）⇒ 恒等式 `omitted_bytes + head.len() + tail.len() == 源文本字节数`。
  - **单行超预算** ⇒ 头部给出该行的**前缀**（≤ 头部预算、字符边界安全），尾部为空；被裁字节计入 `omitted_bytes`。
  - 预算**不放宽**：超出 `max_bytes` 的部分必然被裁掉并**报出**。
  - 变异：把 `omitted_bytes` 恒置 0 ⇒ 第二条断言变红。
- **验证方法**:
  - `cargo test -p ys-tool truncate` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 17
- **依赖**: Task 17

### Task 19: adapters/tools-basic — 后台命令注册表（`background.rs`） [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/background.rs`（**新建**）, `adapters/tools-basic/src/lib.rs`
- **描述**:
  - 类型：`BgHandle(pub u32)`（**进程内单调递增、永不复用**）、`BgState { Running, Exited(Option<i32>), Killed }`
    （**无 `Failed` 变体** —— 启动失败**不产生条目**）、`BgEntry { session, handle, pid, command, cwd, state, consumed, output_path, started_at }`、
    `BgSnapshot { handle, pid, command, cwd, state, elapsed, output_path, output_bytes, consumed }`、
    `BgLimits { max_running: 4, max_entries: 8, max_output_bytes: 2 MiB, default_timeout: 900s, max_timeout: 3600s }`、
    `BackgroundTasks`（内部 `Mutex<Inner>`）。
  - 方法：`new(dir, limits)`、`start(session, command, cwd, timeout)`（**同步返回 `Err`**，失败不留条目）、
    `snapshot(handle, session)`（**session 不匹配 ⇒ `None`**）、`sweep()` 骨架。
  - **落盘**：`<bg_root>/<session_id>/bg<N>.log`；`bg_root` = `$YUSHAN_BG_DIR`（**须绝对路径**）
    或 `<temp_dir>/yushan-bg` ⇒ **不用相对路径**（相对路径会把产物落进用户的 git 工作区）。
  - `spawn`：`std::process::Command::new("sh").arg("-c").arg(cmd)`，`.current_dir(cwd)`，
    stdout/stderr **指向同一个文件**（合并流），`.process_group(0)`。
    **全部同步实现**（`std::process` + `std::fs`），**零 tokio**。
  - `lib.rs` 加 `pub mod background;` + 必要的 `pub use`。
- **依据**: design.md §4.4（接口草案）、§4.5（落盘目录与「不用相对路径」的理由）、§4.7（资源预算）；
  review.md R6（条目带 `session` 字段）、R16（删掉 `BgState::Failed`）。
- **验收命令**:
  - `cargo build -p ys-tools-basic` — 期望通过
  - `cargo test -p ys-tools-basic background` — 期望全部 passed（Task 20 的用例）
- **关联测试任务**: Task 20
- **依赖**: 无

### Task 20: adapters/tools-basic — 注册表单测 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/background.rs`（`#[cfg(test)]`）
- **描述**:
  - 句柄**单调递增、永不复用**：连开两条 ⇒ `bg1` / `bg2`；释放后下一条是 `bg3`。
  - **session 核对**：用会话 A 的句柄去问会话 B ⇒ `None`（对模型表现为「未知句柄」）。
  - **启动失败不产生条目**：命令目录不存在 ⇒ `start` 返回 `Err`，注册表条目数不变。
  - 输出文件落在 `<bg_root>/<session>/bg<N>.log`；条目数到 `max_entries` 时淘汰**最旧的已完成**条目。
  - **测试隔离**：`<bg_root>` 用**进程内原子序号 + `process::id()`** 命名的临时目录。
- **验证方法**:
  - `cargo test -p ys-tools-basic background` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 19
- **依赖**: Task 19

### Task 21: adapters/tools-basic — `bg_tools.rs` 两个工具 + 进程组杀接入 [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bg_tools.rs`（**新建**）, `adapters/tools-basic/src/background.rs`
- **描述**:
  - `background.rs` 补齐：`kill(handle, session)` / `kill_all(session)`（**按会话过滤** ⇒ 漏掉的清理点不波及别的会话）、
    `sweep()` 的**输出上限闸门**（先杀进程组再收尸）、启动时的 **`ulimit -f`** 内核侧兜底、
    `impl Drop for BackgroundTasks`（**panic / unwind 兜底**；只能用同步 kill）。
  - `bg_tools.rs` 新增 `BashBgTool` / `BashPollTool`：
    `BashBgTool { backgrounds: Arc<BackgroundTasks> }` —— `timeout()` = **30s**，
    `is_concurrency_safe()` = **`false`**（启动会 spawn 进程 ⇒ 有副作用 ⇒ **永不准开并行**）；
    `BashPollTool { backgrounds: Arc<BackgroundTasks> }` —— `timeout()` = **35s**，
    `is_concurrency_safe()` = **`false`**（虽对模型是只读，但它推进注册表里的**交付偏移**，
    **对模型可观测** ⇒ 按 `crates/ys-tool/src/lib.rs:53` 的准入纪律判不安全）。
  - `BashPollTool::call` 的读法：**按 `consumed` 偏移增量读**，**只到最后一个完整行**为止
    （不切半行、不切多字节字符）；用 `bg_output::assemble` 组装（`bg_tools` 不再自带拼装；`assemble` 内部用 `ys_tool::truncate::head_tail`）（同一 2000 行 / 50 KiB 预算，**不放宽**）。
  - 交付语义三条：不重复送达 / 未送达必须明示（报 `skipped_bytes` + 完整输出路径）/ 不静默丢弃。
  - 措辞纪律：「**被 harness 杀掉**」与「**命令自身非零退码**」用**不同措辞**；退码 ≠ 0 ⇒ `is_error == true`
    且退码原样带出；句柄不存在 ⇒ 明确的「未知句柄」错误。
  - `cwd` 契约：用**工具自带的 workspace**（与 `bash` 同一条），**不是** `ctx.cwd`。
- **依据**: design.md §4.4（`bg_tools.rs` 草案与两个 `timeout()` 值）、§4.3（数据流与两条契约）、
  §4.6（返回形态与四条取舍）、§4.7（资源预算）；§3.3 校正 3（`bash_poll` 判 `false`）；review.md R4 / R5 / R9 / R17。
- **验收命令**:
  - `cargo build -p ys-tools-basic` — 期望通过
  - `cargo test -p ys-tools-basic bg_tools` — 期望全部 passed（Task 22 的用例）
- **关联测试任务**: Task 22
- **依赖**: Task 5, Task 17, Task 19

### Task 22: adapters/tools-basic — `bg_tools.rs` 单测 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bg_tools.rs`（`#[cfg(test)]`）
- **描述**:
  - 两个工具的 `spec` 逐字断言（名字、描述、`required`）。
  - `is_concurrency_safe()` 两者皆 `false`；`timeout()` 分别为 30s / 35s。
  - `bash_bg` 启动失败 ⇒ **同步报错**且注册表**无新条目**。
  - `bash_poll` 未知句柄 ⇒ 明确的「未知句柄」错误（`is_error == true`）。
  - 退码 ≠ 0 ⇒ `is_error == true` 且**退码原样带出**；被 harness 杀掉 ⇒ 措辞**与**非零退码**不同**。
  - 增量语义：连续两次 poll ⇒ 第二次**不重复**交付已送达字节；被裁剪时**报出 `skipped_bytes`** 与完整路径。
  - 完整行边界：`consumed` 停在**完整行**边界（不切半行、不切多字节字符）。
  - 变异：把 `BashPollTool::is_concurrency_safe` 改成 `true` ⇒ 断言变红；
    把 poll 读改成「读到文件尾」⇒ 「不重复交付」断言变红。
- **验证方法**:
  - `cargo test -p ys-tools-basic bg_tools` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 21
- **依赖**: Task 21

### Task 23: apps/coding-agent — 注册表装配 + `bg_output.rs` [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`, `apps/coding-agent/src/`
- **涉及文件**: `adapters/tools-basic/src/bg_output.rs`（**新建**）, `apps/coding-agent/src/wiring.rs`,
  `apps/coding-agent/src/assembly.rs`, `apps/coding-agent/src/main.rs`
- **描述**:
  - `bg_output.rs`：头尾混合截断的**组装**（调用 `ys_tool::truncate::head_tail`）+ `skipped_bytes` 报出；
    **输出拼装的唯一真源** —— `bg_tools` 不再自带第二份拼装逻辑
    ⇒ `assemble(text, handle_label, output_path, pending_note)`：`pending_note` 的**措辞**由 `bg_tools`
    给出（读域知识），**插入位置**由 `assemble` 决定（输出契约）
    + 完整输出路径。**截断策略本身仍在 `ys-tool`**（本模块只组装）。
  - `assembly.rs`：`AgentFactory` 持 `Arc<BackgroundTasks>`（**进程级**，随工厂冻结 ⇒ `/resume` 重建时是同一份）；
    父表插入 `bash_bg` / `bash_poll` —— **在 `subagent` 之前** ⇒ 子表 9 元**仍是父 JSON 的字节前缀**。
  - `main.rs`：启动期创建 `BackgroundTasks`（`bg_root` = `$YUSHAN_BG_DIR` 或 `<temp_dir>/yushan-bg`），
    传进 `AgentFactory::new`；启动期**清扫** `bg_root` 下残留的旧会话目录。
  - 子表**仍是基础 9 元** ⇒ 子 agent 天然没有后台能力。
- **依据**: design.md §4.5（落盘目录、注册表对象 vs 内容）、§4.8（父表前缀不变量、核心 crate 改动表）、
  §3.1（父表 12 / 子 9）；ADR 2026-10-21 决策 4（注册表留在 adapter crate，app 持 `Arc`）。
- **验收命令**:
  - `cargo build --workspace` — 期望通过
  - `cargo test -p ys-coding-agent assembly` — 期望全部 passed（Task 24 的用例）
- **关联测试任务**: Task 24
- **依赖**: Task 19, Task 21

### Task 24: apps/coding-agent — 组装单测 + assembly 级断言 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`, `apps/coding-agent/src/`
- **涉及文件**: `adapters/tools-basic/src/bg_output.rs`（`#[cfg(test)]`）, `apps/coding-agent/src/assembly.rs`（`#[cfg(test)]`）
- **描述**:
  - `bg_output.rs`：`skipped_bytes` 报出正确；未裁剪时为 0；字节 / 行数预算**不放宽**。
  - `assembly.rs`：**assembly 级断言**（design.md §4.8 点名的缺口 ——
    既有的 9 元顺序用例**自拼父表**，插入新工具后照样全绿却不再描述真实父表）：
    ① 真实父表 **12** 元、子表 **9** 元；
    ② 真实父表 JSON **以子表 JSON 去末尾 `]` 开头**（前缀不变量）；
    ③ `subagent` **仍是末位**。
  - 变异：把两个新工具插到 `subagent` **之后** ⇒ 前缀断言与末位断言同时变红。
- **验证方法**:
  - `cargo test -p ys-coding-agent assembly` — 期望全部 passed
  - `cargo test -p ys-tools-basic bg_output` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 23
- **依赖**: Task 23

### Task 25: apps/coding-agent — 四个清理点 + sweeper + 回合末计数 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`
- **描述**:
  - 六个清理点中的**四个**（design.md §4.5 清理点表）：
    ① `abort` 臂（`apps/coding-agent/src/app_loop.rs:915` 附近）；
    ② 回合预算到点臂（同文件 `:931` 附近，同样 push `Boundary::Abort` ⇒ 同一路径）；
    ③ `/new` 臂（`:463`）⇒ `kill_all(session)` **+ 删除该会话的输出目录**；
    ④ `/resume` 臂（`:517`）⇒ `kill_all(session)`。
  - **按会话过滤** ⇒ 漏掉的清理点不会波及别的会话。
  - 启动**一个共享 sweeper**（**全局 1 个**，100ms tick 调 `sweep()`）—— 不是每条命令一个。
  - 回合末在 debug 日志记「还有 N 个后台任务在跑」（**日志，不进模型上下文**）。
- **依据**: design.md §4.5（六个清理点表）、§4.7（tokio 任务 1 个共享 sweeper）、§7 风险 1（缓解措施）。
- **验收命令**:
  - `cargo build -p ys-coding-agent` — 期望通过
  - `cargo test -p ys-coding-agent app_loop` — 期望全部 passed（Task 26 的用例）
- **关联测试任务**: Task 26
- **依赖**: Task 23

### Task 26: apps/coding-agent — 清理点单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`（`#[cfg(test)]`）
- **描述**:
  - 四个清理点各触发一次 ⇒ 本会话的后台进程组**被杀**（进程存在性断言，**不看文案**）。
  - **跨会话不误杀**：会话 A、B 各起一条 ⇒ `kill_all(A)` 后 **B 仍在**。
  - `/new` ⇒ 旧会话的输出目录被删；`/resume` ⇒ 旧工作区的命令被杀。
  - 变异：把 `kill_all(session)` 改成全局清空 ⇒ 「跨会话不误杀」断言变红；
    删掉 `/new` 臂的清理 ⇒ 第一条断言变红。
  - **测试隔离**：临时目录名含**进程内原子序号 + `process::id()`**；
    env 改写走 `crate::test_env::env_lock()` + `EnvRestore`。
- **验证方法**:
  - `cargo test -p ys-coding-agent app_loop` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 25
- **依赖**: Task 25

### Task 27: apps/coding-agent — 进程退出清理点 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`
- **描述**:
  - 清理点之五：**进程退出** —— `std::process::exit`（`apps/coding-agent/src/main.rs:1971`）**之前**
    显式 `kill_all(全部)`。
  - **理由**：`std::process::exit` **不跑析构** ⇒ `Drop` 兜底在此路径**无效**。
  - 既有 `exit_with_debug`（`:1935`）的 flush 语义**保持不变**。
- **依据**: design.md §4.5 清理点表第 5 行（「`std::process::exit` 不跑析构 ⇒ `Drop` 兜底在此路径无效」）。
- **验收命令**:
  - `cargo build -p ys-coding-agent` — 期望通过
  - `grep -c 'process::exit' apps/coding-agent/src/main.rs` — 期望仍为 `1`
- **关联测试任务**: Task 28
- **依赖**: Task 23

### Task 28: apps/coding-agent — 退出路径断言 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/main.rs`（`#[cfg(test)]`）
- **描述**:
  - `include_str!("main.rs")` 计数断言：清理调用**在 `exit_with_debug` 内**；
    `std::process::exit` 在全文件**仍只有 1 处**。
  - 变异：把清理调用移到 `exit_with_debug` 之后 ⇒ 位置断言变红。
- **验证方法**:
  - `cargo test -p ys-coding-agent exit` — 期望全部 passed
  - `grep -c 'process::exit' apps/coding-agent/src/main.rs` — 期望 `1`
- **关联 impl**: Task 27
- **依赖**: Task 27

### Task 29: apps/coding-agent — 后台命令集成测试（`bash_bg` → `bash_poll`） [type=test]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/background_command.rs`（**新建**）
- **描述**:
  - **不引新依赖、不需要模型**：直接构造 `BackgroundTasks` + `BashBgTool` / `BashPollTool`，
    在临时目录里跑完整链路。
  - 断言：启动 ⇒ 立刻拿到句柄（**不等命令结束**）；poll ⇒ 增量输出；
    第二次 poll **不重复**交付；命令退出后 poll 给**退码**；退码 ≠ 0 ⇒ `is_error == true`。
  - **孤儿进程数 = 0**：清理后用进程存在性检查（`pgrep` 式）复查。
  - **裁剪报出**：输出超限时，结果里出现 `skipped_bytes` 与**完整输出路径**。
  - **测试隔离**：临时目录名含**进程内原子序号 + `process::id()`**；并发测试不得互删文件。
- **验证方法**:
  - `cargo test -p ys-coding-agent --test background_command` — 期望全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 21, Task 23, Task 25, Task 27
- **依赖**: Task 21, Task 23, Task 25, Task 27

### Task 30: 文档 — 切片 1 的文档回写（父表 12 元） [type=impl]
- **涉及目录**: `docs/`
- **涉及文件**: `docs/design.md`, `docs/design-final/coding-agent-tui.md`
- **描述**:
  - 父表元素数 **10 → 12**（基础 9 + `bash_bg` + `bash_poll` + `subagent`），子表仍 **9**。
  - `docs/design-final/coding-agent-tui.md` 的 as-built 段落回填切片 1 的**实际形态**：
    两个新工具、文件后端、六个清理点、交付语义三条、`YUSHAN_BG_DIR`。
  - **只写已成立的事实**；本仓的 as-built 文档要记「实现与设计的分歧」时单列，**不把分歧藏起来**。
- **依据**: design.md §4.8（既有断言与文档）、§4.9 切片 0f 与切片 1；ADR §7。
- **验收命令**:
  - `grep -n '12 元' docs/design.md docs/design-final/coding-agent-tui.md` — 期望两处同值
- **关联测试任务**: Task 31
- **依赖**: Task 23

### Task 31: 文档 — 父表数字一致性核对 [type=test]
- **涉及目录**: `docs/`
- **涉及文件**: `docs/design.md`, `docs/design-final/coding-agent-tui.md`
- **描述**:
  - 两处文档的父表元素数**同值**（**12**），且**子表 9** 未被误改。
  - 变异：把其中一处改成 10 ⇒ 断言变红。
- **验证方法**:
  - `grep -n '12 元' docs/design.md docs/design-final/coding-agent-tui.md` — 期望两处同值
  - `grep -n '9 元' docs/design.md docs/design-final/coding-agent-tui.md` — 期望子表仍为 9
- **关联 impl**: Task 30
- **依赖**: Task 30

### Task 32: apps/coding-agent — 协议面 A/B 的测量开关（切片 2） [type=impl]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/e2e_bench.rs`
- **描述**:
  - 在既有基准脚手架上加**协议面**一列与 `YUSHAN_BENCH_PROTOCOL` 开关（`chat` / `responses`，默认 `chat`）。
  - 记录表输出：**墙钟 / 轮数 / `ttft`·`gen`·`tools` 切分 / 质量**（质量必须同时记录 —— 不可只看墙钟）。
  - **只加测量，不给 `ys-model` 增加 Responses 后端**：那涉及 `Model` trait 与流式事件形态，**须过 ADR**
    （context.md Q5 的裁定）。`responses` 档的 base 由外部端点/shim 提供。
  - **本机无凭证** ⇒ 真实 A/B 判定标 **MANUAL_ACK_REQUIRED**，**不阻塞**其它任务。
- **依据**: design.md §6.3（P2 的 A/B 口径）、§4.9 切片 2、§9 后续 3（复用 `e2e_bench` 脚手架）；
  context.md Q5。
- **验收命令**:
  - `cargo build -p ys-coding-agent --tests` — 期望通过
  - `cargo test -p ys-coding-agent --test e2e_bench` — 期望非 `#[ignore]` 部分全部 passed
- **关联测试任务**: Task 33
- **依赖**: 无

### Task 33: apps/coding-agent — bench 离线单测 [type=test]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/e2e_bench.rs`
- **描述**:
  - 开关解析：未设置 ⇒ `chat`；设为 `responses` ⇒ 生效；非法值 ⇒ 回退 `chat`（不 panic）。
  - 记录表：**列名与渲染**正确（墙钟 / 轮数 / ttft / gen / tools / 质量位）。
  - **不需要凭证**（`#[ignore]` 的真实跑分部分不参与）。
- **验证方法**:
  - `cargo test -p ys-coding-agent --test e2e_bench` — 期望非 `#[ignore]` 部分全部 passed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 32
- **依赖**: Task 32

### Task 34: workspace — 全量回归 + 变异测试 [type=test]
- **涉及目录**: 全仓（只做临时注入，不改终态）
- **涉及文件**: 无（临时修改后用 `git apply -R` / 临时 patch 恢复；**不得**用 `git checkout --` 覆盖未提交改动）
- **描述**: 逐条注入并跑对应用例，期望变红；恢复后全绿。**变异清单（至少 7 条）**：

| # | 注入 | 应变红的任务 |
|---|---|---|
| 1 | `crates/ys-loop/src/basic.rs` 并行路径删掉 `with_tool_budget` | Task 4 |
| 2 | `adapters/tools-basic/src/bash.rs` 的内层钳位改成「取模型值」（不取 min） | Task 6 / T2 |
| 3 | `adapters/tools-basic/src/bash.rs` 无 boundary 分支去掉进程组杀 | Task 6 |
| 4 | `apps/coding-agent/src/debug_log.rs` 的 `command_fp` 改成 `args_original_bytes` | Task 14 |
| 5 | `apps/coding-agent/src/prompt.rs` 的 guideline 守卫改成无条件 | Task 12 |
| 6 | `adapters/tools-basic/src/bg_tools.rs` 的 `is_concurrency_safe` 改成 `true` | Task 22 |
| 7 | `apps/coding-agent/src/app_loop.rs` 的 `kill_all` 去掉会话过滤 | Task 26 |
| 8 | `adapters/tools-basic/src/bg_tools.rs` 的 poll 读**不**停在完整行 | Task 29 |

  - **全量回归**：`cargo test --workspace`（基线 1295 passed / 12 ignored ⇒ 期望 **0 failed**，
    通过数不低于基线）；`cargo clippy --all-targets` **零新增 warning**；
    `grep -c 'process::exit' apps/coding-agent/src/main.rs` 仍为 `1`。
  - ⚠ **MANUAL_ACK_REQUIRED 三项**（本机无凭证，**不阻塞**收尾）：切片 1 的前后对照（T13）、
    长会话压缩验收（T6）、协议面 A/B（T14）。
- **验证方法**:
  - 每条注入后 `cargo test -p <crate> <对应用例>` — 期望 failed
  - 恢复后 `cargo test --workspace` — 期望 0 failed
  - `cargo clippy --all-targets` — 期望零 warning
- **关联 impl**: Task 1, Task 3, Task 5, Task 7, Task 9, Task 11, Task 13, Task 15, Task 17,
  Task 19, Task 21, Task 23, Task 25, Task 27, Task 30, Task 32
- **依赖**: Task 2, Task 4, Task 6, Task 8, Task 10, Task 12, Task 14, Task 16, Task 18,
  Task 20, Task 22, Task 24, Task 26, Task 28, Task 29, Task 31, Task 33

## 波次计划（Phase 3 计算后回填）

> Phase 3 开始时按 3.2 合并单元并分层，回填于此。**同目录文件集**已逐对核对：
> `crates/ys-tool/src/context.rs` 由 Task 1 改、`crates/ys-loop/src/basic.rs` 由 Task 3 改、
> `adapters/tools-basic/src/bash.rs` 由 Task 5 与 Task 7 先后改（**同文件 ⇒ 必须串行**）、
> `adapters/tools-basic/src/background.rs` 由 Task 19 与 Task 21 先后改、
> `adapters/tools-basic/src/lib.rs` 由 Task 5 与 Task 19 先后改、
> `apps/coding-agent/src/main.rs` 由 Task 23 与 Task 27 先后改、
> `apps/coding-agent/src/{config.rs,capabilities.rs}` 由 Task 9 改、
> `apps/coding-agent/src/{debug_log.rs,round_clock.rs}` 由 Task 13 改 ——
> 均以依赖保证顺序，不同波。

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|--------|
| U1 | Task 1, Task 2 | `crates/ys-tool/src/context.rs` | 无 | W0 | deep |
| U2 | Task 5, Task 6 | `adapters/tools-basic/src/{proc_group.rs,bash.rs,exec.rs,lib.rs}` | 无 | W0 | deep |
| U3 | Task 3, Task 4 | `crates/ys-loop/src/basic.rs` | U1 | W1 | deep |
| U4 | Task 7, Task 8 | `adapters/tools-basic/src/bash.rs` | U1, U2 | W1 | deep |
| U5 | Task 9, Task 10 | `apps/coding-agent/src/{config.rs,capabilities.rs}` | 无 | W0 | deep |
| U6 | Task 11, Task 12 | `apps/coding-agent/src/prompt.rs` | 无 | W0 | shallow |
| U7 | Task 13, Task 14 | `apps/coding-agent/src/{debug_log.rs,round_clock.rs}` | 无 | W0 | deep |
| U8 | Task 15, Task 16 | `AGENTS.md`, `docs/design.md`, `docs/design-final/coding-agent-tui.md` | 无 | W0 | shallow |
| U9 | Task 17, Task 18 | `crates/ys-tool/src/{truncate.rs,lib.rs}` | 无 | W1 | deep |
| U10 | Task 19, Task 20 | `adapters/tools-basic/src/{background.rs,lib.rs}` | 无 | W1 | deep |
| U11 | Task 21, Task 22 | `adapters/tools-basic/src/{bg_tools.rs,background.rs}` | U2, U4, U9, U10 | W2 | deep |
| U12 | Task 23, Task 24 | `adapters/tools-basic/src/bg_output.rs`, `apps/coding-agent/src/{wiring.rs,assembly.rs,main.rs}` | U10, U11 | W3 | deep |
| U13 | Task 25, Task 26 | `apps/coding-agent/src/app_loop.rs` | U12 | W4 | deep |
| U14 | Task 27, Task 28 | `apps/coding-agent/src/main.rs` | U12 | W4 | deep |
| U15 | Task 29 | `apps/coding-agent/tests/background_command.rs`（新建） | U11, U12, U13, U14 | W5 | deep |
| U16 | Task 30, Task 31 | `docs/design.md`, `docs/design-final/coding-agent-tui.md` | U12 | W4 | shallow |
| U17 | Task 32, Task 33 | `apps/coding-agent/tests/e2e_bench.rs` | 无 | W0 | shallow |
| U18 | Task 34 | 无（临时注入） | U1–U17 | W6 | deep |

> **合并依据**：每个 impl 与其配对 test 同单元（配对同单元的硬要求）。
> **U2 + U4 同文件**（`bash.rs`）：U4 排在 U2 之后（**同文件必须串行**）；
> 若 Phase 3 按目录强制合并，二者合并为一个执行单元，依赖图不变。
> **U12 + U14 同目录**（`main.rs`）：若 Phase 3 按目录强制合并，合并为 W3–W4 的同一执行单元；
> 依赖图不变（U14 依赖 U12 的注册表接线）。
> **review 档依据**：U1–U5、U7、U9–U15、U18 均触及对外契约（`ToolContext` 形状 / 工具超时与进程组语义 /
> 窗口来源 / debug schema 加字段 / 截断策略 / 工具表形状 / 生命周期清理 / 工具 spec）⇒ deep；
> U6、U8、U16、U17 只改提示词文案与文档 / 测量脚手架 ⇒ shallow。

## 验证清单

- [x] **所有 (impl, test) 配对均已 completed**（Phase 3 review 放行条件）
- [x] `cargo test --workspace` — **1485 passed / 0 failed / 12 ignored**
      （基线 1377 passed / 12 ignored ⇒ 通过数高于基线，failed = 0）
- [x] `cargo clippy --all-targets` — **无新增 warning**（既有 warning 24 处，逐条核对均不在本次改动内）
- [x] `grep -c 'process::exit' apps/coding-agent/src/main.rs` — 等于 **1**
- [x] `grep -c 'YUSHAN_BG_DIR' AGENTS.md` — 等于 **1**
- [x] T1/T2/T3：预算进工具、钳位严格小于外层、不传 `timeout` 也被砍且进程组被杀
- [x] T4：无 boundary 路径超时不留孤儿（孙进程不存在）
- [x] T5：`/model` 输出含窗口值与来源
- [x] T7：两条 guideline 按工具名派生
- [x] T8：`command_fp` 区分同命令与异命令，且不落命令正文
- [x] T9：四个指标可从日志直读
- [x] T10：`bash_bg` / `bash_poll` 的 spec、并发声明、错误措辞
- [x] T11：交付语义三条 + 完整行边界
- [x] T12：六个清理点 + 孤儿进程数 = 0
- [x] 变异清单（Task 34）**全部能变红**（共 7 + 6 + 4 + 2 + 3 条，逐单元留证）
- [ ] T6（长会话 150k 不被压缩）—— **MANUAL_ACK_REQUIRED**（本机无凭证）
- [ ] T13（切片 1 前后对照）—— **MANUAL_ACK_REQUIRED**（本机无凭证）
- [ ] T14（协议面 A/B）—— **MANUAL_ACK_REQUIRED**（本机无凭证）

## 验收记录

- 验收报告：`docs/reports/2026-10-21-background-command-report.md`（待生成）
- ⚠ **本机无凭证**（`YUSHAN_API_BASE` / `YUSHAN_API_KEY` 未设置）⇒ T6 / T13 / T14 三项
  保留为 **MANUAL_ACK_REQUIRED**，**不阻塞**其它任务的收尾。
