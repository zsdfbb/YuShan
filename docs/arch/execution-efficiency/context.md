# execution-efficiency — 架构上下文

> 日期：2026-10-20
> 性质：**arch-explore 产出**（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起：一次同任务、**同模型、同推理强度、同网关、同工作区、同提示**的跨 harness 对照，
> 显示两个 harness 的墙钟差 **4.9×**。本文要回答的是：**这 4.9× 花在哪、哪些是本仓能动的。**
>
> **与既有文档的边界**（三份治的不是同一件事）：
>
> | 文档 | 治什么 | 本文的处置 |
> |---|---|---|
> | [`explore-speed/`](../explore-speed/context.md) | 探索慢 = **轮数**多 | 不重复；本文只引用其「墙钟 ≈ 轮数 × 每轮模型耗时」的成本模型，**并指出它不适用于真命令** |
> | [`tool-call-efficiency/`](../tool-call-efficiency/context.md) | **调用数量**多（少绕弯） | 不重复；只读批次并行已于 `d553740` 落地，不在本文范围 |
> | **本文** | **非探索部分的执行成本**：分钟级真命令、每轮模型往返、窗口声明 | — |
>
> **口径纪律**：本文所有「当前是 X」的事实断言都指 `code/`；ADR 只用于解释 **why**。

---

## 概述

一次真实开发会话（YuShan coding-agent 跑在 `/Users/zs/Develop/YuShan`）的墙钟构成，与同一任务在
另一 harness 上的同模型对照相比，暴露出**四个可归因的差距**：

| # | 差距 | 量级 | 归属 |
|---|---|---|---|
| **G1** | **分钟级真命令同步阻塞整轮**，且同一条命令**被跑了两遍** | 本次会话 58s / 430s = **13.5%** | 工具契约 + 提示准则 |
| **G2** | **每轮生成吞吐**只有对照组的 ~56% | 43 tok/s vs ≥77 tok/s | 模型适配器（协议面） |
| **G3** | **上下文窗口少报一半** | 128000 vs 258400 | 配置声明（**已有用户级覆盖可用**） |
| **G4** | **轮数**多 8 轮（21 vs 13） | 放大上面三项 | 已在 `explore-speed` 射程内，本文不重开 |

G4 是既有主题的领地；**G1–G3 是本轮的新领地**。

---

## 现有架构

### 模块边界

```text
ys-core ──► ys-tool（Tool trait / ToolSpec / ToolContext / truncate / notice / registry）
              ▲
              ├── adapters/tools-basic（read / write / edit / bash / grep / find / ls）
              └── adapters/tools-web（websearch / webfetch）
                      ▲
              adapters/model-openai-compatible（OpenAI 兼容后端）
                      ▲
crates/ys-loop（BasicLoop：轮次编排 / 并行准入 / 回合级预算）
                      ▲
apps/coding-agent（toolset 装配 / prompt 派生 / app_loop 回合边界 / debug 日志）
```

### 核心抽象（与本需求相关的五个闸门）

```rust
// crates/ys-tool/src/lib.rs:31 —— 工具 trait：全部无状态
fn timeout(&self) -> Option<Duration> { None }              // 工具声明的超时优先
fn is_concurrency_safe(&self) -> bool { false }             // 并行准入，默认 false
async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
```

```rust
// crates/ys-tool/src/context.rs:8 —— ToolContext：三个可选挂载点，**全是只读借用**
pub boundary: Option<&'a dyn BoundarySource>,
pub cwd: PathBuf,
pub workspace_root: PathBuf,
pub summarizer: Option<&'a dyn Summarizer>,
pub subagent: Option<&'a dyn SubagentRunner>,
```

| 闸门 | 取值 | 落点 | 作用 |
|---|---|---|---|
| **轮次时序** | 模型 → 工具**全部完成** → 回喂 | `crates/ys-loop/src/basic.rs:599`（Step 9） | 严格交替，**没有「工具未完成」这个状态** |
| **并行准入** | `pending.len() > 1 && 每个工具 is_concurrency_safe()` | `crates/ys-loop/src/basic.rs:633`；上限 `DEFAULT_MAX_PARALLEL_TOOLS = 4`（`crates/ys-component/src/limits.rs:8`） | 默认 false ⇒ 全部串行 |
| **单条输出** | `MAX_LINES = 2000` / `MAX_BYTES = 50KB` | `crates/ys-tool/src/truncate.rs:22,24`；`tail()` 保尾丢头（`:131`） | 工具结果上限 |
| **回合级输出** | `context_window / 4` | `apps/coding-agent/src/main.rs:1034` | 一回合内多条结果之和超预算 ⇒ 更早的结果被削 |
| **上下文窗口** | 默认 `128_000`；`trigger_ratio 0.85` / `keep_ratio 0.10` | `crates/ys-component/src/limits.rs:65`；`crates/ys-component/src/compact.rs:70-71` | 压缩触发点 = 窗口 × 0.85 |

**工具表的生命周期是进程级**：`AgentFactory` 在 `apps/coding-agent/src/main.rs:2618` 构建一次，
`toolset::build_tools` 在 `apps/coding-agent/src/assembly.rs:100` 调用一次；`/new` 只换 session，
**不重建工具表** ⇒ 任何放进工具实例的跨轮状态都会**跨会话存活**。

### 关键数据流

```text
每轮（Round）= 一次 Model.complete()
  Step 4b   压缩判定（estimated + fixed_prompt_tokens vs window × trigger_ratio）
  Step 5    组装 ModelRequest（ToolSpec 每轮重发）
  Step 6    model.complete()  ← ttft 在这里，gen 在这里
  Step 9    执行工具调用（串行/并行，见上）  ← **真命令在这里阻塞**
  Step 11   回合级工具输出闸门（降级更早的结果）
  回喂      单条消息（Step 9c 保序）→ 下一轮
```

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `sh` | `bash` 工具 | 每次调用新进程；Unix 上 `process_group(0)`（`adapters/tools-basic/src/bash.rs:247`） |
| `rg` / `fd` | `grep` / `find` 后端 | 缺失时 POSIX 降级 + 启动期提示（ADR-0015） |
| opencode-go 网关 | 模型后端 | `https://opencode.ai/zen/go/v1`，当前走 `/chat/completions`（`adapters/model-openai-compatible/src/lib.rs:405-407`） |

---

## 现状证据（实测，2026-10-20）

### 采样口径

- **YuShan 侧**：本机真实会话 `~/.yushan/sessions/1792506807_969975000.jsonl` +
  同名 debug 日志 `~/.yushan/logs/debug-1792506807_969975000.jsonl`（157 行，含逐轮
  `round_start` / `round_usage` / `round_end` / `tool_call` / `tool_result`）。
  逐轮耗时字段 `ttft_ms` / `gen_ms` / `tools_ms` / `total_ms` 是**工具自报的墙钟**，
  不是我方估算。
- **对照组**：同一任务（提示「考虑后续可以做的事情」）在另一 harness 的同一次运行记录。
  两侧**同模型 `minimax-m3.1-flash-free`、同 `reasoning_effort = high`、同 opencode-go 网关、
  同工作区 `/Users/zs/Develop/YuShan`、同提示**。
- 两侧任务的**性质**是开放式调研（「这个仓库接下来该补什么」），不是可枚举任务 ——
  与 `explore-speed/cost-model.md` §2 的对照一致：开放式任务轮数显著更多。

### 对照结果

| 指标 | YuShan | 对照组 | 倍数 |
|---|---:|---:|---:|
| 墙钟（run 起 → 终局事件） | **440.2s**（turn 内 430.6s） | **90.4s** | **4.9×** |
| 模型响应轮数 | 21 | 13 | 1.6× |
| 工具调用数 | 35（33 bash + 2 read） | 19（18 exec + 1 write_stdin） | 1.8× |
| 输出 token | 15593（可见 5134 / **推理 10459**） | 6563（可见 2604 / 推理 3959） | 2.4× |
| 推理 token 占比 | 67% | 60% | — |
| 输入 token | 404367（cache 375368 / 未命中 **28999**） | 398739（cache 354816 / 未命中 **43923**） | 1.0× |
| **上下文窗口** | **128000** | **258400** | **0.5×** |
| 生成吞吐 | **43.0 tok/s** | **≥76.8 tok/s** | 0.56× |
| 每次响应平均墙钟 | 17.3s | 6.9s | 2.5× |

> **对照组的生成吞吐是下界**：其 85.4s 里含工具等待，故真实模型吞吐更高。
> ⇒ **G2 是保守估计。**

### 墙钟构成（YuShan 侧，逐轮自报，430.4s / 430.6s）

| 分项 | 耗时 | 占比 |
|---|---:|---:|
| TTFT（首 token 前：排队 + prefill） | 113.1s | **26.3%** |
| 生成（thinking + 正文） | 249.5s | **58.0%** |
| 工具执行 | 67.8s | **15.8%** |

**零空转**：逐轮 `total_ms` 之和 430.4s ≈ `elapsed_ms` 430.6s ⇒ **harness 自身没有浪费**，
全部差距来自「模型行为 + 工具阻塞 + 配置」。

> ⚠️ 与 `explore-speed/cost-model.md` §0 的口径差异：那份记「ttft ≈ 80% / 工具 ≈ 1%」，
> 因为样本是**固定提示的只读探索流**（上下文小、工具快）。本次是**真实开发会话**
> （上下文 404k、真命令两次）⇒ **工具项升到 15.8%**。两份数字各自成立，**不可互相替换**。

### G1 · 分钟级真命令阻塞整轮，且同一条命令跑了两遍

逐轮工具耗时（`round_end.tools_ms`）：

| round | 工具耗时 | 调用 | 内容 |
|---|---:|---:|---|
| 10 | 31301ms | 2 | `cargo test --workspace 2>&1 \| tail -30`（timeout 900） |
| 11 | 27121ms | 1 | `cargo test --workspace 2>&1 \| grep -E "^test result" \| awk ...`（timeout 900） |

**同一条命令（`cargo test --workspace`）被跑了两遍，合计 58.4s = 墙钟的 13.5%。**
触发原因可从会话原文读出：第一遍用 `tail -30`，返回的是**结尾的 doctest 段**，
拿不到全局 pass/fail 计数 ⇒ 模型补跑一遍带聚合过滤器。

对照组的做法：`cargo test --workspace` **放后台**（拿到一个会话句柄），
**继续做文档探索**，只在需要时轮询一次 ⇒ 90s 墙钟内**没有被测试阻塞**。

### G2 · 同模型同 effort，生成吞吐差 1.8×

| | 推理 token | 可见输出 | 模型侧耗时 | 吞吐 |
|---|---:|---:|---:|---:|
| YuShan（`/chat/completions`） | 10459 | 5134 | 362.6s | **43.0 tok/s** |
| 对照组（Responses API） | 3959 | 2604 | ≤85.4s | **≥76.8 tok/s** |

同模型、同 effort、同网关 ⇒ 差异**不能归因于模型能力**。候选归因（**尚未区分**）：

1. **协议面**：YuShan 走 `/chat/completions`（`adapters/model-openai-compatible/src/lib.rs:405-407`），
   对照组走 Responses API。两者的事件形态与推理内容呈现方式不同。
2. **提示词面**：工具面（YuShan 9+1 个工具、29 个 skill 注入）、system prompt 结构、
   guideline 措辞不同 ⇒ 同一模型的行为（思考长度、是否重复劳动）不同。
3. **服务端排队**：同网关同模型，但不同协议可能落到不同后端队列。

> **这三者必须先分开，否则任何优化都归因不清。** 本文只把它们列为**待区分的候选**，
> 不下结论。

### G3 · 上下文窗口少报一半

- YuShan 侧 `RunLimits::default().context_window = 128_000`（`crates/ys-component/src/limits.rs:65`）。
- 解析链（`apps/coding-agent/src/config.rs:235` `context_window_for`）三级：
  ① 用户配置 `context_window`（JSON 键；内部字段名 `windows`）→ ② 网关 `/v1/models` 目录自报 → ③ 内建表
  `KNOWN_MODEL_WINDOWS`（`apps/coding-agent/src/provider.rs:488`）。
- ②级实测**对 `minimax-m3.1-flash-free` 不报窗口**（`/v1/models` 返回的 `context_length` 为 `None`）。
- ③级内建表**只精确收录** `MiniMax-M3` / `MiniMax-M2.7`，匹配不到本模型 ⇒ **落到 128000**。
- 对照组运行时用 **258400**。

**后果链**：窗口 × `trigger_ratio 0.85` = 压缩触发点。128000 ⇒ **108800** 触发；
真实可用若为 258400 ⇒ 触发点应为 **219640**。⇒ **YuShan 会比应有的早约 2× 触发压缩**。
本次会话输入峰值仅 29127 token，**未触发**，故本条**在本例中无损失** ——
但它是**长会话的真实风险**，且属于**静默**行为（用户只看到「上下文被压缩了」）。

> ✅ **用户级覆盖已存在**：`~/.yushan/config.json` 的 `context_window` 字段是 ①级，
> 改配置即可覆盖（`apps/coding-agent/src/config.rs:236`）。
> ⇒ **本条是「立即可用但没人知道」的配置问题**，不是必须改代码的问题。
> 真正的代码问题是 ③级表为什么是精确匹配 —— 但那是另一个话题。

### 工具面的既有闸门（本需求的约束来源）

| 闸门 | 现状 | 对本需求的影响 |
|---|---|---|
| **轮次严格交替** | Step 9 同步执行完才回喂（`crates/ys-loop/src/basic.rs:599`） | 「后台执行」要求引入「工具未完成」这个**新的状态** |
| **工具无状态** | `Tool::call(&self, ...)`；`ToolContext` 全是只读借用（`crates/ys-tool/src/context.rs:8`） | 跨轮句柄表需要**新的存放点** |
| **工具表进程级** | `/new` 不重建（`apps/coding-agent/src/main.rs:2618`） | 放进工具实例的状态**跨会话存活** ⇒ 需要显式的清理边界 |
| **并行准入默认 false** | `crates/ys-loop/src/basic.rs:594` | 后台化的 bash 若能被并发启动，会**绕过**「同批串行」保证 |
| **子进程随 future drop 而杀** | `kill_on_drop(true)`（`adapters/tools-basic/src/bash.rs:240`） | 后台化后**不能**沿用这条：进程必须活过工具调用 |
| **中止语义** | `Boundary::Abort` + `BoundarySource`（ADR-0013） | 后台进程在 abort 后必须被杀；但 abort 发生在**工具调用之间**，工具早已返回 |
| **已有杀整组能力** | `process_group(0)` + `kill_process_group`（`adapters/tools-basic/src/bash.rs:247,167`） | 可复用，不必新写 |
| **回合级输出闸门** | `context_window / 4`（`apps/coding-agent/src/main.rs:1034`） | 后台命令的输出回喂时会被降级 |
| **单条输出上限** | 2000 行 / 50KB，保尾丢头（`crates/ys-tool/src/truncate.rs:22,24,131`） | 后台命令的输出同样受限；**保尾丢头**对「测试失败在开头」不友好 |

---

## 约束

- **技术**
  - 依赖方向单向：`ys-core ← 组件接口 ← loop/runtime ← 应用适配器`；`ys-core`/`ys-event`/`ys-protocol` **零 tokio**（直接与间接皆无）。
  - `ys-tui-coding` **不依赖 `ys-runtime`**（编译器强制）；UI 线程必须无 runtime 上下文。
  - **Agent 无状态**（ADR-0010）：不持有 session / events / model；会话、配置、历史归接线器。
  - **取消即消息**（ADR-0013）：`CancelToken` / `Agent::cancel()` 已删；`Boundary::Abort` + `BoundarySource` 取代。**每回合必须新建 `BoundarySource`**（`Abort` 标记永久置位）。
  - **信道契约**（ADR-0009/0011/0013）：有界、容量可配；信道满时实现**必须内部缓冲**，不得返回 `Err`。
  - **事件只观察，Hook 才控制流程**：`AgentEvent` 不可用于改变执行路径。
  - **最小核心 / 不做安全**：沙箱、隔离、权限、审批、多租户由上层解决。
  - 新增代码必须能对应上 `docs/design.md` §12 测试矩阵中的条目；关键逻辑要做**变异测试**。

- **性能**
  - 墙钟 = 轮数 × 每轮模型耗时 + 工具执行（`explore-speed/cost-model.md` §1）；**只读工具是毫秒项，真命令是分钟级**。
  - 输出上限 2000 行 / 50KB（单条）、`context_window / 4`（回合级）—— **不放宽**。
  - 压缩触发点 = 窗口 × 0.85；窗口声明错误 ⇒ 压缩**静默提前**。
  - 缓存命中率在 opencode-go 上**可测**（`explore-speed/cost-model.md` §10）；本例 YuShan 侧 92.8%。

- **演进**
  - 已有主题不得重复设计：`explore-speed`（轮数）、`tool-call-efficiency`（调用数）、
    `prompt-delivery`（提示投递通道）、`skills-and-compaction`（压缩）。
  - `docs/TODO.md` §C 仍有 7 项「待用户裁决」，本需求**不并入**其中任何一项的裁决范围。
  - 协议面若要从 `/chat/completions` 扩到 Responses API，属**模型后端能力扩展**，须过 ADR。

- **组织**
  - 本仓单人开发，无外部兼容性负担；**没有 CI**（无 `.github/`）⇒ 验收必须自带可复现命令。
  - 文档 / 注释 / commit message 用中文，标识符用英文。

---

## 需求范围

### 范围内

- **P0 · 零契约改动**（当天可落地）
  1. **窗口声明修正**：把 `minimax-m3.1-flash-free` 的窗口从 128000 提到实测可用值，
     走**已有**的用户级覆盖（`~/.yushan/config.json` 的 **JSON 键是 `context_window`**，内部字段名才叫 `windows` —— `apps/coding-agent/src/config.rs:31`）；并把「窗口值从哪来」写进 `/model` 或 `docs/`。
  2. **「长命令一次取全量」提示准则**：在 guideline / prompt 层点明「要统计口径就一次跑完并带聚合过滤器，
     不要先 `tail` 再补跑」⇒ 直接治 G1 的**重复执行**那一半。
  3. **度量补齐**：在 debug 日志里加三个可判定指标（设计已细化为四个，见 [`design.md`](./design.md) §6.1）—— **重复命令识别**（同一条命令串在一回合内出现 >1 次）、
     **真命令耗时**、**工具阻塞占比**。让 G1/G2/G4 之后的任何改动**都有账可查**。

- **P1 · 工具契约改动**（本轮最大的杠杆，需 arch-design）
  4. **bash 后台执行 + 句柄 + 轮询**：让分钟级真命令**不阻塞整轮**。
     需要新引入「工具未完成」这个状态 + 跨轮句柄表 + abort 时的清理边界。
     **形态（新增独立工具 vs 给现有 bash 加参数）与状态存放点尚未选定** —— 见未澄清问题。

- **P2 · 协议面**（可与 P0/P1 并行推进的独立实验）
  5. **`/chat/completions` vs Responses API 的同 prompt A/B**：在**同模型、同 effort、同提示**下
     比较墙钟 / 推理 token / 吞吐 / 质量。目的是**把 G2 的三个候选归因分开**，
     再决定是否给 `ys-model` 增加一个 Responses 后端（须过 ADR）。

### 范围外（明确不做）

- **重开「减少轮数」**：`explore-speed` 已有成本模型与推理强度 A/B（ADR-0042 定 `high` 默认），
  本文只引用不重做。
- **提高只读并发度**：通道已通电（`d553740`），天花板 ≈ 0。
- **改推理强度默认值**：ADR-0042 已定 `high`，`explore-speed` §8.5 已做过 `low`/`high` A/B。
- **放宽任何输出上限**（2000 行 / 50KB / `context_window / 4`）。
- **加沙箱 / 权限 / 审批 / 多租户**（项目硬约束：安全外置）。
- **引入持久 shell / 跨调用 cwd**（破坏无状态与并行准入）。
- **改模型侧做「每轮注入」**（`docs/arch/prompt-delivery/notes.md` §2 的缓存经济学）。
- **放宽并行准入**来让后台 bash 并发启动。
- **新建代码库索引 / embedding / 向量检索**（新依赖）。
- **把「后台执行」扩成通用异步工具框架**：本轮只治 bash 一个工具的真命令阻塞，
  不预建 hook/plugin 系统（ADR-0021 只留装配缝）。

### 关键场景（成功路径）

- **S1 · 分钟级真命令不阻塞**：模型发起 `cargo test` ⇒ 立刻拿到句柄 ⇒ **同一轮内继续做别的探索**
  ⇒ 下一轮取测试结果。墙钟上，测试的 27s 与探索的 N 轮**重叠**。
- **S2 · 长命令一次取全量**：模型要 pass/fail 计数 ⇒ **一条命令**带聚合过滤器跑完 ⇒ 不补跑。
- **S3 · 中止即清理**：用户 Ctrl-C / `Abort` ⇒ **所有后台进程组被杀** ⇒ 无孤儿进程。
- **S4 · 会话结束不留残留**：会话切换（`/new`）或进程退出 ⇒ 后台进程被清理，
  且**不跨会话泄漏**（工具表进程级这条约束必须被显式处理）。
- **S5 · 窗口正确**：长会话跑到 150k token ⇒ **不**在 108800 处被静默压缩。

### 异常 / 边界场景

- 模型**仍然**重复跑同一条命令：改动只降低概率 ⇒ 验收看**重复命令数**，不能假设某条路径必然发生。
- 后台命令**永不结束**（如 `tail -f`）：必须有句柄级的终止手段，且**不能**让 loop 死等。
- 后台命令**输出巨大**：受单条 2000 行 / 50KB 上限约束；`tail` 保尾丢头 ⇒ **测试失败在开头时会被丢掉**，
  需在形态设计时考虑（要么改截断方向，要么让后台命令的输出分次取）。
- 后台命令与**同批其它工具**并存：批次并行准入（`crates/ys-loop/src/basic.rs:594`）必须仍然成立 ——
  启动一个后台进程**不等于**它是并发安全的。
- 后台进程**跨轮存活**期间用户 `/new`：旧会话的后台进程必须被杀（否则是跨会话副作用）。
- **abort 发生在工具调用之间**（工具早已返回、进程还在跑）：`Boundary::Abort` 的轮边界探针
  （`crates/ys-loop/src/basic.rs:136` Step 1）必须能触达后台进程的清理路径。
- 仓库很大 / 端口被占 / 权限不足：后台启动失败必须**同步报错**，不能返回一个永远就绪的句柄。
- **协议 A/B 的模型行为漂移**：换协议后模型的轮数/工具形态可能变 ⇒ A/B 必须**同时**记录质量，
  不能只看墙钟。

---

## 未澄清问题

> 全部**待用户确认**。本轮已按 skill 约定「未回答也照样产出」，以下问题原样保留。

- [ ] **Q1 · 本轮范围**：只做 P0（配置 / 提示 / 度量，零契约改动）？还是把 P1（bash 后台执行）
      一起纳入本轮设计？P1 是回收 G1 的主杠杆，但要**引入跨轮状态**并**改对模型可见的工具契约**，
      是本轮最大的一块。**待用户确认**
- [ ] **Q2 · 后台执行的形态**：新增独立工具（如 `bash_bg` + `bash_poll`）？还是给现有 `bash`
      加参数（`background` / `handle`）？前者契约清晰、模型心智负担小，代价是**工具表从 9 变 11**
      （每轮固定 token 上升，且需处理「派发类工具」的既有约定）；后者上下文成本最低，
      代价是**同一工具返回形态分叉**（同步结果 vs 句柄），模型容易混用。**待用户确认**
- [ ] **Q3 · 跨轮状态的存放点**：进程级（工具实例内 `Mutex`，**跨会话存活**，需显式清理边界）？
      会话级（由 `Wiring` 持有，`/new` 时重建）？还是 `ToolContext` 的**第 4 个挂载点**
      （`Option<&'a dyn ...>`，与 `summarizer`/`subagent` 同形）？三者的**清理时机**各不相同，
      且都与 ADR-0010「Agent 无状态」的边界相关。**待用户确认**
- [ ] **Q4 · 验收口径**：接受「同批真实 prompt 的前后对照（墙钟 / 轮数 / 工具耗时 / **重复命令数**）」？
      还是只做定性？**基线必须在改动之前采** —— 本文的对照数据只覆盖**一个**真实会话，
      样本量不足以当基线。**待用户确认**
- [ ] **Q5 · 协议面是否进本轮 scope**：`/chat/completions` vs Responses API 的 A/B 是否本轮做？
      若结论是 Responses 明显更省更快，是否**直接给 `ys-model` 增加 Responses 后端**？
      后者涉及 `Model` trait 与流式事件形态，**须过 ADR**，不是本轮能顺手带上的。**待用户确认**
- [ ] **Q6 · 跨 harness 对照的定位**：本文的对照组记录来自**另一个 harness 的运行日志**。
      它作为「同模型同 effort 下 4.9× 的差距是可回收的」这一判断的证据是充分的；
      但若要把它写进**对外可引用的结论**，需要先确认那份日志的可引用口径
      （本仓文档约定：跨产品取舍对照放 `research.md`，不放 `context.md`）。
      **待用户确认**

---

## 后续建议

1. **先做 P0 的第 3 项（度量补齐）** —— 它是后面一切验收的前提：
   「重复命令数 / 真命令耗时 / 工具阻塞占比」三个指标不进日志，P1/P2 的收益就**无法证明**。
   debug 日志已有 `yushan.debug/1` schema 与逐轮 `round_end.tools_ms`，扩字段成本低。
2. **P0 的第 1 项（窗口声明）当天就能验**：把窗口值通过 `~/.yushan/config.json` 的 `context_window`
   覆盖到实测可用值，跑一次长会话看压缩触发点是否后移。**先确认用户级覆盖这一条路走不走得通**，
   再决定要不要动 ③级内建表的匹配策略。
3. **P1 交 `arch-design`**：设计要点至少三条 ——
   ① 「工具未完成」这个新状态怎么进 `BasicLoop` 的轮次模型（不回喂？回喂一个句柄？）；
   ② 句柄表的**清理边界**（abort / `/new` / 进程退出，三处时机不同）；
   ③ 「启动后台进程」**不算**并发安全 —— 并行准入判定不能被绕过。
4. **P2 用 `prototype` 或现有 `e2e_bench` 脚手架做 A/B**：目标是把 G2 的三个候选归因
   （协议面 / 提示词面 / 服务端排队）**分开**，而不是直接得出「换协议更快」的结论。
   `apps/coding-agent/tests/e2e_bench.rs` 已有固定提示集与 `YUSHAN_BENCH_*` 开关可复用。
5. **本文的对照组数据建议单独落 `research.md`**（按本仓文档约定：跨产品对照放 research，
   `context.md` 只留结论与指针），并在其中标注那两份日志的**采集口径与时间**。
6. 与 `docs/TODO.md` 的关系：P0 的第 3 项（度量补齐）可考虑作为一条新的待办登记；
   **P1 涉及工具契约变更，是否要占 §B（ADR 已 accepted、代码零落地）还是新开 §A，
   待本轮范围（Q1）定下来再决定**。
