# execution-efficiency — 架构设计

> 日期：2026-10-20
> 性质：**arch-design 产出**（多方案对比 + 推荐）。现状与约束见 [`context.md`](./context.md)，
> 跨产品对照见 [`research.md`](./research.md)，决策记录见
> [`adr-background-command-execution.md`](./adr-background-command-execution.md)。
>
> **指针纪律**：本文所有「当前是 X 状态」的断言都指 `code/`；ADR 只用于解释 **why**。

---

## 0. 一句话

**把「分钟级真命令」从 agent loop 的关键路径上摘下来，摘除的边际开销 ≈ 0；**
同时修掉探索过程中暴露的**两条既存缺陷**与**一条配置错误**。

本设计回收的是 **G1（工具项 15.8% + 重复执行）**，
**不是** 4.9× 差距的主因 —— 差距主体是 G2（生成吞吐 43 vs ≥77 tok/s）与 G4（轮数 21 vs 13），
分别属于模型适配器协议面与 `explore-speed`，本设计不碰（见 §8）。

---

## 1. 需求确认

### 1.1 关键场景（成功路径）

| 场景 | 期望 |
|---|---|
| S1 分钟级真命令不阻塞 | 模型发起长命令 ⇒ 立即拿句柄 ⇒ 同回合继续别的探索 ⇒ 命令时间与模型时间**重叠** |
| S2 长命令一次取全量 | 模型要 pass/fail 计数 ⇒ **一条命令**带聚合过滤器跑完 ⇒ 不补跑 |
| S3 中止即清理 | Ctrl-C / `Abort` ⇒ 所有后台进程组被杀 ⇒ 无孤儿 |
| S4 会话结束不留残留 | `/new` / `/resume` / 进程退出 ⇒ 后台进程与落盘产物一并清理 |
| S5 窗口正确 | 长会话跑到 150k token ⇒ **不**在 108800 处被静默压缩 |

### 1.2 异常 / 边界场景

- 模型**忘记轮询** ⇒ 结果丢失（本设计的**已知残留风险**，§7 方案 C 风险 1）。
- 后台命令**永不结束** ⇒ 有寿命上限，到点杀进程组并**记为失败**（不静默成功）。
- 后台命令**输出巨大** ⇒ 有磁盘上限，超限杀进程组并**记为失败**。
- 后台进程与同工作区的其它工具调用**并发写** ⇒ 绕过「同批串行」闸门，属新增风险类别，
  无机制性解法，只能靠提示词约束。
- 命令**自行脱离进程组**（`setsid` / daemonize）⇒ 进程组清理覆盖不到。

### 1.3 质量目标

| 目标 | 指标 | 基线 |
|---|---|---|
| 墙钟 | 同一任务的回合墙钟 | 430.6s（真会话 `1792506807_969975000`） |
| 工具阻塞占比 | `tools_ms` / 总墙钟 | **15.8%**（67.8s） |
| 重复执行 | 同一条命令在一回合内跑 >1 次 | **1 次**（58.4s） |
| 上下文成本 | 每轮固定前缀增量 | 缓存前缀内，未命中增量 ≈ 0 |

### 1.4 约束

见 [`context.md`](./context.md) §约束（技术 / 性能 / 演进 / 组织）全文。此处只强调三条**承重**的：

1. **工具表是进程级**（`apps/coding-agent/src/main.rs:2618` 建工厂一次，`apps/coding-agent/src/assembly.rs:100` 装表一次，
   `/new` 只换 session）⇒ 任何跨轮状态**默认跨会话存活**，必须有显式清理边界。
2. **`Tool::call(&self, …)` 无状态**，`ToolContext` 的**三个可选挂载点**（`boundary` / `summarizer` / `subagent`）全是**只读借用**（`crates/ys-tool/src/context.rs:8-26`），
   且 `ToolContext` 由 `BasicLoop` **逐次构造**（`crates/ys-loop/src/basic.rs` 的 Step 9）⇒ **没有会话级持有者**。
3. **不做安全**：沙箱、隔离、权限、审批由上层解决。

### 1.5 非目标

见 [`context.md`](./context.md) §范围外（不重开轮数、不提高只读并发度、不改推理强度默认值、
不放宽输出上限、不加沙箱、不建通用异步工具框架、不做通知注入）。

### 1.6 `context.md` 未澄清问题的裁定

| 问题 | 裁定 | 依据 |
|---|---|---|
| Q1 本轮范围 | **三层全收，但分切片**：P0 先落（零契约改动），P1 工具契约，P2 协议面 A/B 独立 | P0 是 P1/P2 的**验收仪器**，不先做则收益不可证 |
| Q2 后台形态 | **两个新工具**（`bash_bg` + `bash_poll`），**不改** `bash` | 见 §3 对比矩阵「逐字节不变」行 |
| Q3 句柄表存放 | **进程级、app 侧持有，工具持 `Arc` clone** | 见 §4.5 |
| Q4 验收口径 | **可判定的前后对照**；基线**先采**后改 | 见 §6 |
| Q5 协议面 | 进 scope，但**独立切片**，不阻塞 P0/P1 | 见 §4.9 切片 2 |
| Q6 对照组日志口径 | 已拆 [`research.md`](./research.md) | — |

---

## 2. 候选方案

三个候选由三个并行 subagent 各自按一种设计哲学生成：
**A = 最小复杂度**、**B = 可扩展优先**、**C = 性能/资源优先**。
三者共享同一份 `research.md` 作为输入。

### 2.1 候选 A · 超时自动移交（最小复杂度）

> 由「最小复杂度」倾向的 agent 生成，同时给出了「显式后台」变体并自行判定前者劣于后者。

**核心**：把 `bash` 现有 `timeout` 路径的收尾从「杀进程组 + 报 `ToolError::Timeout`」
换成「**不杀**，把进程移交后台，工具立即返回句柄」。触发点 = 内层 `timeout` 到点；回收 = 显式轮询。

- 工具表**不变**（仍 9 元）。
- 参数净增 1 个：`handle`（轮询用）；`timeout` 复用但**词义变了**（从「超时上限」变成「前台等多久后移交」）。
- 机制与候选 C 同构（同一张句柄表、同一批收集任务、同一个 `render_result`），**差别只在触发点**。

**候选 A 自评的致命处**（我已独立核实，见 §3.3）：在观察到的真实会话参数（`timeout 900`）下**根本不生效** ——
外层工具预算恒为 300s（`crates/ys-loop/src/basic.rs:47-50`），先于内层 900s 到点。

**候选 A 给出的「共同底座」**（本设计全部采纳，见 §4.10 切片 0b）：
一次调用只返回**一种输出形态**，是模型补跑同一条命令的结构性成因；
把口径写进同一条命令即可治，**零新概念**。

### 2.2 候选 B · 「长任务（Job）」一等能力 + 工具面「可后台化」标记（可扩展优先）

**核心**：把后台执行抽成 `ys-tool` 的**一条能力平面** —— 两个纯 trait（`Job` / `JobSupervisor`）+
`ToolContext` 的**第 4 个同形挂载点**；工具面只做两件事：`bash` 加 `background` 参数（启动面）+
新增通用 `job` 工具（`status` / `cancel` / `list`，回收面）。

- 句柄表由 **app 层持有**，每回合经 `AgentPorts` → `RuntimeContext` → `ToolContext` 借用挂载。
- `ys-loop` 轮次模型、`ys-event` 事件枚举、`ys-protocol` **一行不动**。
- 新增 4 文件（`ys-tool/src/job.rs`、`tools-basic/src/job.rs`、`apps/.../jobs.rs`、`apps/.../job_tool.rs`），
  改动 9 处，预计 **450–550 行**。

**候选 B 的三条关键判断**（我全部采纳其方向）：
1. 「后台任务」是通用能力，抽象落在 `ys-tool`（与 `summarizer.rs` / `subagent.rs` 同层同形，零 tokio）。
2. 回收方式选**显式轮询**（通知注入是架构级改动）。
3. **句柄表不进工具实例** —— 这是对「工具表进程级」这条约束的正面回答。

**候选 B 的诚实自评**：若只有 bash 一个消费者，这 450 行**超支**；
但仍推荐，理由是「跨会话副作用无安放处」是**现在**就要解的真问题。

### 2.3 候选 C · 文件后端零管道后台执行（性能/资源优先）

**核心**：命令输出由**内核直接写进文件**，父进程**不持有管道、不持有 reader 任务、不在内存里攒输出**；
轮询按**字节偏移增量读**；进程级句柄表用一把 `Mutex` 守着；清理点显式定义。

**三条设计原则**：
1. **后台化只买「重叠」这一件事，其余必须免费。** 走管道会引入「管道写满 → 子进程阻塞」
   与「跨轮无人读 → 输出停在管道里」两个经典故障；每条命令多一个 reader 任务与一份无界内存缓冲。
   ⇒ **不用管道。**
2. **截断方向与流式化冲突，用「头尾混合 + 文件路径」解。** `ys_tool::tail` 保尾丢头
   （`crates/ys-tool/src/truncate.rs:131`）对「测试失败在开头」不友好 —— 这正是模型补跑一遍的直接诱因。
   后台工具改用**头尾混合**（同一 `MAX_BYTES` / `MAX_LINES` 预算内取头 1/4 + 尾 3/4），
   并把输出**落成真实文件**交给已有的 `read` / `grep` 工具取任意片段
   （`webfetch` 已是同一形态的先例，`apps/coding-agent/src/prompt.rs` 的 webfetch 条目）。
3. **轮询不许重读整份输出。** 每次只读「上次交付位置 → 当前文件尾」，单次读**恒 ≤ 50 KiB**
   （与 `MAX_BYTES` 同额），总拷贝量 O(n) 而非 O(n²)。

**候选 C 的形状**：新增**两个**工具 `bash_bg`（启动）+ `bash_poll`（回收）；
`bash` **一字不改**；父表 10 → 12 元，**子表仍 9 元**；
`ys-loop` / `ys-component` / `ys-runtime` **零改动**，`ys-tool` 只加一个纯函数 `head_tail`。
（评审后修正：推荐方案最终要改 `ys-loop` 一处 —— 填工具预算，见 §3.5 修正 6。）

---

## 3. 对比分析

### 3.1 对比矩阵

| 维度 | A · 超时自动移交 | B · Job 抽象 | C · 文件后端双工具 |
|---|---|---|---|
| **模型可见契约** | `bash` 加 `background` / `handle`；**`required` 从 `["command"]` 放宽到 `[]`** | `bash` 加 `background` + **新工具 `job`** | **两个新工具** `bash_bg` / `bash_poll`；`bash` 不动 |
| **父表元素数** | 10（不变） | 11 | 12 |
| **子表元素数** | 9 | 9 | 9 |
| **「不做后台时逐字节不变」** | ✗ `bash` 的 spec 变了 + `required` 放宽 | ✗ `bash` 的 spec 变了 | ✔ **`bash` 一字不改** |
| **工具表指纹锚点** | ✗ 必重锚（`0xeced4483ff29ef22`） | ✗ 必重锚 | ✔ **不动**（锚点只覆盖 `build_tools` 的 9 元，`apps/coding-agent/src/toolset.rs:601-606`） |
| **核心 crate 改动** | 0 | **4**（`ys-tool` / `ys-component` / `ys-runtime` / `ys-loop`） | **2 crate / 3 处**（`ys-tool` 加 `head_tail` + `ToolContext` 字段；`ys-loop` 填一处） |
| **新 trait / 新挂载点** | 0 / 0 | **2 / 1** | 0 / 0 |
| **新增文件** | 1 | 4 | 2 |
| **预估机制代码量** | ~350 行 | ~450–550 行 | ~400 行 |
| **输出存储** | 内存环形缓冲（保尾） | 内存环形缓冲（保尾） | **文件**（内核直写，磁盘有闸门） |
| **父进程 fd（稳态）** | 2/条 + 2 个 reader 任务 | 2/条 + 2 个 reader 任务 | **0/条** |
| **无界内存风险** | 需另设上限 | 需另设上限 | **无** |
| **G1b（重复执行）是否被结构性治掉** | ✗ 只能靠提示词 | ✗ 只能靠提示词 | ✔ **输出文件可 `grep`** |
| **子 agent 行为** | 共享表（跨会话污染） | fail-closed（挂载点为 `None` ⇒ 明确错误） | **无该工具**（最简） |
| **清理点数量** | 4 | 4 | 6 |
| **对既有不变量** | 违背 M7（超时不再算失败） | 无 | 无 |
| **在实测会话参数下是否生效** | **✗ 不生效**（§3.3） | ✔ | ✔ |

### 3.2 Back-of-envelope

基线：回合墙钟 **430.6s**；逐轮自报 ttft 113.1s（26.3%）/ gen 249.5s（58.0%）/ **工具 67.8s（15.8%）**；
round 10 工具 31.3s、round 11 工具 27.1s（**同一条命令跑两遍**，合计 **58.4s = 13.5%**）；
每次模型响应平均 **17.3s**（21 轮）。

| 情景 | 重复执行消除 | 重叠收益 | 额外轮次 | 净墙钟 | 相对 430.6s |
|---|---:|---:|---:|---:|---:|
| **下界**（模型无独立工作可做） | −27.1s（round 11 工具）+ −17.3s（round 11 整轮消失） | 0 | +17.3s（多一轮去收） | **−27.1s** | **−6.3%** |
| **中枢**（收轮与 `read`/`grep` 合批，测试期间有 ~20s 无关探查） | −44.4s | −20s | 0 | **−64.4s** | **−15.0%** |
| **上界**（完全重叠） | −44.4s | −31.3s | 0 | **−75.7s** | **−17.6%** |

**新增固定开销**：工具 spec + guideline **+230 token/轮**，全在缓存前缀 ⇒ 未命中增量 ≈ 0；
`sweeper` CPU **0.016% 单核**（430s 全程 ≈ 70ms）；注册表加锁 ~100ns/轮；
常驻内存 **< 10 KB**（不缓存输出正文）。⇒ **净开销 < 0.1% 墙钟。**

### 3.3 我对候选的三处校正

**校正 1 · 候选 A 的「不生效」结论 —— 核实成立，且比候选自己说的更严重。**

`crates/ys-loop/src/basic.rs:47-50` 的 `tool_budget`：`tool.timeout()` → `limits.bash_timeout` → **300s**。
`BashTool` **未覆盖** `timeout()`（`crates/ys-tool/src/lib.rs:42` 取默认 `None`）
⇒ **bash 的外层预算恒为 300s**，与模型在 `timeout` 参数里传的值**完全无关**。

后果链：模型传 `timeout: 900` ⇒ 内层 deadline 900s，但**外层 300s 先到** ⇒
`crates/ys-loop/src/basic.rs:62` 的 `tokio::time::timeout(budget, tool.call(...))` 把工具 future **drop** 掉
⇒ `adapters/tools-basic/src/bash.rs:240` 的 `kill_on_drop(true)` 只杀直接子进程 `sh`
⇒ `cargo` / 测试二进制等**孙进程成孤儿**。

而 `bash.rs` 的**无 boundary 路径**（`ctx.boundary` 为 `None` 时走这条）超时分支里
`#[cfg(unix)]` 块是**空的**（`adapters/tools-basic/src/bash.rs:265-268`，只有一句注释）⇒
**这条路径连进程组都不杀**；只有有 boundary 的路径在 `adapters/tools-basic/src/bash.rs:353` 才 `kill_process_group(pid).await`。

⇒ 由此得到**两条既存缺陷**（与后台执行特性无关，可独立修）：
- **D1**：模型的 `timeout` 参数与外层 300s 预算**脱钩** ⇒ 模型以为自己有 900s，实际 300s 被砍。
- **D2**：无 boundary 路径超时**不杀进程组** ⇒ 孙进程孤儿。

**校正 2 · 候选 C 的「轮询不占额外一轮」假设 —— 结论对，归因错。**

候选 C 把「轮询是否占额外一轮」归因于 `bash_poll.is_concurrency_safe = true`。
但按 `crates/ys-loop/src/basic.rs:411-428`（Step 8），一轮的 `tool_calls` 全部取自**同一条** assistant 消息
⇒ **一条消息 = 一次模型往返**，与批内几个调用、并行还是串行**无关**。
`is_concurrency_safe` 只影响**批内执行的墙钟**（`max` 还是 `sum`），不影响**轮次数**。

⇒ 真正的杠杆是**提示词**（「把 `bash_poll` 与其它调用放进同一条消息」），
不是并发闸门。验收指标随之改为：**每回合 `bash_poll` 所在的 assistant 消息里有几个工具调用**
（`apps/coding-agent/src/round_clock.rs` 的逐调用记录已能给出批内序列）。

**校正 3 · `bash_poll` 的 `is_concurrency_safe` 应判 `false`（偏离候选 C）。**

候选 C 判 `true`（只读）。但 `crates/ys-tool/src/lib.rs:53` 的准入纪律是
「可证明对**共享状态**无副作用才准开」，而 `bash_poll` 会推进注册表里的**交付偏移** ——
这**对模型可观测**（并发 poll 时模型可能重复看到同一段输出）。
按纪律应判 **`false`**：含它的批次退回串行。

代价：批内执行从 `max` 变 `sum` —— **毫秒级**，不影响收益（收益来自**跨轮**重叠，不来自批内并行）。
收益：消除偏移竞态，且不必修改 `is_concurrency_safe` 的语义。

### 3.4 逐条对账：被否方案的理由

| 被否 | 理由 |
|---|---|
| **A · 超时自动移交** | ① 在实测会话参数下**不生效**（§3.3 校正 1）；② 与外层 300s 预算**打架**；③ 违背 M7「超时算失败」；④ 要求模型**猜**一个反直觉的参数；⑤ `timeout` 一词二义。**机制与 C 同构、触发点每项都劣** ⇒ 无独立价值。 |
| **B · Job trait 抽象** | ① **450–550 行** vs C 的 ~400 行，而本仓硬约束明写「最小核心」「不设计假想未来需求」；② 核心 crate 改动 **4 → 2**（评审后重新记账，见 §3.5 修正 6；差距仍在，但不再是「4 → 0」）；③ `ToolContext` 挂载点要穿 `ys-tool` → `ys-component` → `ys-runtime` → `ys-loop` 四个 crate 的签名，**blast radius 远大于收益**；④ 第一个消费者只有一个（bash）。**但它的「回收面与启动面解耦」是对的** —— 那正是 C 的双工具形态，故**采纳其形状、不采纳其抽象层**。 |

### 3.5 评审引入的三处修正（R1–R3）

`arch-validate` 的评审（[`review.md`](./review.md)）核出三处**推荐方案自身**的缺口，
与 §3.3 对候选的校正同性质 —— 都是「不改，就会以『看起来正常』的方式把缺陷留下」。

**修正 4 · deadline 的归属：工具内始终有 deadline（对应 R1）。**

上面两条缺陷（D1 / D2）的修法若只写成「把外层预算传给工具、内层钳到外层以内」，
仍覆盖不到**模型不传 `timeout`** 这条路径：

- `adapters/tools-basic/src/bash.rs:201` 的 `timeout_secs` 是 `Option` ⇒ 参数可选；
- `adapters/tools-basic/src/bash.rs:256` 的 `if let Some(secs) = timeout_secs` ——
  **不传时走 else 分支**，工具内**没有任何 deadline**；
- 有 boundary 的路径同理（`adapters/tools-basic/src/bash.rs:305` 的 `timeout_secs.map(…)`）。

⇒ 这条路径上，超时**不是工具自己发现的**，而是外层
`crates/ys-loop/src/basic.rs:62` 的 `tokio::time::timeout(budget, …)` 到点 **drop 工具 future** ⇒
只触发 `kill_on_drop(true)`（`adapters/tools-basic/src/bash.rs:240`）杀直接子进程 `sh` ⇒ 孙进程成孤儿。
**而「超时分支补杀进程组」这个修法在这条路径上根本不会被执行到**（它是被 drop 的，不是自己超时的）。

⇒ 修正：**deadline 是工具的责任，不是「模型传了才有的东西」**。
工具内**始终**持有 deadline：模型传值时取 `min(模型值, 外层预算 − Δ)`，
不传时取 `外层预算 − Δ`。「杀进程组」因此永远由**工具自己的分支**完成，
不再依赖 future 被 drop 时的 `kill_on_drop` 副作用。

**修正 5 · 钳位必须严格小于外层，且留余量（对应 R2）。**

若内层 deadline **等于**外层预算，两者同刻到期 ⇒ 谁先返回不确定 ⇒ 外层若先赢，
仍然 drop future ⇒ 回到修正 4 要治的那条路径。

⇒ 钳位写成「**严格小于 + 余量**」：`effective = min(模型值, 外层预算 − Δ)`（Δ 取 5s 量级）。
同时，超时文案必须写**生效值**（模型传 900、实际 295 ⇒ 文案说 295），
否则模型仍以为自己有 900s —— 这正是 D1「模型毫不知情」的另一半。

**修正 6 · 「核心 crate 零改动」的口径要改（对应 R3）。**

修正 4/5 的实现路径是「把外层预算经 `ToolContext` 传给工具」⇒
必须给 `crates/ys-tool/src/context.rs` 的 `ToolContext` **加一个字段**，
并由 `crates/ys-loop/src/basic.rs` 填上。
⇒ `ys-tool` 与 `ys-loop` 各改一处，**不是零改动**。

**核实补记**：`ToolContext::new` 在 `crates/ys-loop/src/basic.rs` 有**两处**构造点 ——
并行路径 `:666` 与串行路径 `:716`。两处的 `tool_budget(tool, &ctx.limits)` 都在紧邻位置算出
（并行路径在循环内逐工具算、串行路径在 `call_one` 之前算）⇒
**两处都要填**（各填各自的工具预算，语义正确：预算本就是**按工具**的）。

为什么仍走 `ToolContext` 而不是在工具里缓存预算：预算**每回合刷新**
（`apps/coding-agent/src/app_loop.rs` 每回合重算 limits），而工具是**建表期产物**
⇒ 任何在工具里缓存预算的静态做法都会过期。

⇒ 对 §3.4「B 的 blast radius」的**重新记账**：B 要改 **4** 个 crate 的签名，
本方案改 **2** 个 crate 各 **1** 处 ⇒ 差距仍在，但不再是「4 → 0」而是「4 → 2」。
（这不改变「不采纳 B 的抽象层」的结论 —— B 的 450–550 行与两个新 trait 的代价
不因本方案多改两处而消失。）

---

## 4. 推荐方案（融合）

### 4.1 决策摘要

采纳**候选 C 的形态与文件后端** + **候选 A 的提示词底座** +
**候选 B 的「启动面 / 回收面解耦」**；**否决候选 A 的触发点**与**候选 B 的抽象层**；
并**追加两条既存缺陷的修正**。

三条主线：

1. **P0（模型可见契约零改动）**：窗口声明修正 + 提示词两条准则 + debug 度量补齐 + **修 D1/D2 两条既存缺陷**。
   （「契约」指**模型可见**的 `ToolSpec`；D1 的修法要动 `ys-tool` 的 `ToolContext` 内部字段，
   **不进 `ToolSpec`** ⇒ 工具表指纹与父表前缀都不受影响。）
2. **P1（工具契约）**：`bash_bg` + `bash_poll` 两个新工具，文件后端。
3. **P2（协议面）**：`/chat/completions` vs Responses API 的同 prompt A/B，**独立切片**。

### 4.2 模块划分与职责

**新增（3 个文件）**

| 文件 | 职责 |
|---|---|
| `adapters/tools-basic/src/background.rs` | 后台命令注册表（**进程级**）：句柄分配、`spawn`、状态快照、输出字节偏移、进程组 kill、输出上限闸门、`sweep()`、`Drop` 兜底。**全部同步实现**（`std::process` + `std::fs`），零 tokio。 |
| `adapters/tools-basic/src/bg_tools.rs` | `BashBgTool` / `BashPollTool` 两个 `Tool` 实现。唯一用 tokio 的地方是读输出文件（`tokio::fs`）。 |
| `adapters/tools-basic/src/bg_output.rs` | 头尾混合截断的**组装**（截断**策略**本身仍在 `ys-tool`，见下） |

**改动**

| 文件:行 | 改动 |
|---|---|
| `crates/ys-tool/src/truncate.rs` | 新增 `head_tail()`（纯函数，复用既有 `head_lines` `:75` 与 `tail` `:131`）。**`tail` 本身一字不改** ⇒ `bash` 的输出逐字节不变。 |
| `crates/ys-tool/src/lib.rs` | 导出 `head_tail` / `HeadTail`（照 `:8-19` 的清单形态） |
| `adapters/tools-basic/src/bash.rs:167` | `kill_process_group` 的**同步内核**提取到**共享模块** `adapters/tools-basic/src/proc_group.rs`（`sh -c "kill -9 -<pid>"` 逐字保留），`bash.rs` 侧保留原 `async fn` 签名与语义 ⇒ 避免两份「杀进程组」实现漂移。**新模块承载「先杀后收 / pid 复用窗口」的告诫**（从 `bash.rs:158-162` 搬过来），两个调用点共用同一份内核与同一条告诫。 |
| `adapters/tools-basic/src/bash.rs:265-268` | **修 D2**：无 boundary 路径的超时分支补上进程组杀（与 `adapters/tools-basic/src/bash.rs:353` 的有 boundary 路径同款）。配合下一行，**「模型未传 `timeout`」这一支也会走到这里**（deadline 由外层预算兜底，§3.5 修正 4）。 |
| `crates/ys-tool/src/context.rs` | **修 D1**：给 `ToolContext` 加一个**只读**的外层预算字段（`tool_budget: Option<Duration>`）。它是**第四个可选挂载点**，形态与 `boundary` / `summarizer` / `subagent` 同形 ⇒ 工具据此算出自己的 deadline。 |
| `crates/ys-loop/src/basic.rs:666`（并行路径）+ `:716`（串行路径） | **修 D1**：把本次调用的外层预算（`tool_budget(tool, limits)` 的结果）填进 `ToolContext`（**两处**都填）⇒ 工具内**始终**有 deadline（§3.5 修正 4/5）。 |
| `adapters/tools-basic/src/bash.rs`（超时分支） | **修 D1/D2**：内层 deadline = `min(模型值, 外层预算 − Δ)`；**模型未传 `timeout` 时也用外层预算** ⇒ 到点必走**杀进程组**的那条分支，不再依赖 future 被 drop 时的 `kill_on_drop`。 |
| `adapters/tools-basic/src/proc_group.rs`（新增） | **进程组内核的共享落点**：同步 `kill_group(pid)` + 「必须先杀后收」的告诫（`bash.rs:158-162`） |
| `adapters/tools-basic/src/lib.rs:1-17` | 加 `background` / `bg_tools` / `bg_output` / `proc_group` 四个 `pub mod` 与 re-export |
| `apps/coding-agent/src/assembly.rs:117-119` | 父表在 `subagent` **之前**插入 `bash_bg` + `bash_poll` ⇒ 父表 12 元，**子表 9 元仍是父 JSON 的字节前缀**（保住 `apps/coding-agent/src/assembly.rs:117` 注释记的缓存性质）。 |
| `apps/coding-agent/src/assembly.rs:81` | `AgentFactory::new` 多收一个 `Arc<BackgroundTasks>`（与工作区无关 ⇒ 属 `AgentFactory` 冻结的那类量） |
| `apps/coding-agent/src/main.rs:2618` | 创建 `Arc<BackgroundTasks>`（进程级）+ 启 sweeper |
| `apps/coding-agent/src/main.rs:1971` | `std::process::exit(code)` **之前**显式 `kill_all(全部)`（该行跳过析构 ⇒ `Drop` 兜底在此路径无效） |
| `apps/coding-agent/src/app_loop.rs:463` / `:517` / `:914` | 三个清理点：`/new` → `kill_all(session)` + 删会话输出目录；`/resume` → `kill_all(session)`；`Boundary::Abort` → `kill_all(session)` |
| `apps/coding-agent/src/prompt.rs` | 两条 guideline（§4.6） |
| `apps/coding-agent/src/debug_log.rs` | **四个**度量字段（§6.1）—— 三个的原始数据**已有**，只有「重复命令数」要**新增 `command_fp` 字段** |
| `apps/coding-agent/src/assembly.rs`（新增断言） | **父表前缀不变量的真实守卫**：断言真实父表 JSON 以「子表 JSON 去末尾 `]`」开头、且 `subagent` 仍是**末位**（现有 `apps/coding-agent/src/toolset.rs:596` 的测试**自己拼**父表 ⇒ 插入新工具后它照样全绿，却不再描述真实父表）。 |
| `AGENTS.md` | 环境变量段登记 `YUSHAN_BG_DIR`（**须绝对路径**，与 `YUSHAN_FETCHED_DIR` 同规矩）。AGENTS.md 是「每一轮都需要」的入口文件 ⇒ 新环境变量不登记就等于不存在。 |

**零改动**：`ys-core` / `ys-event` / `ys-protocol` / `ys-model` / `ys-session` /
`ys-component` / `ys-runtime` / `ys-tui-coding`。

**核心 crate 的两处例外**（§3.5 修正 6）：`ys-tool` 加**一个只读字段**（`ToolContext`）、
`ys-loop` **填一处**（构造 `ToolContext` 的地方）。**轮次模型不动。**

⇒ **依赖方向不变，无新 crate，无新依赖。**

### 4.3 关键数据流

```
① 模型发 bash_bg(command="cargo test --workspace")
   └─ BasicLoop Step 9 串行路径（bash_bg 的 is_concurrency_safe = false
      ⇒ 批内退回串行，crates/ys-loop/src/basic.rs 的准入闸门不被绕过）
      └─ BashBgTool::call
         └─ BackgroundTasks::start
            ├─ 打开 <bg_root>/<session_id>/bg<N>.log
            ├─ std::process::Command::new("sh").arg("-c").arg(cmd)
            │     .current_dir(工具的 workspace)   ← 与 `bash` 同契约（见下）
            │     .stdout(Stdio::from(f.try_clone()?))   ← 内核直写文件
            │     .stderr(Stdio::from(f))               ← 同一个文件 ⇒ **stdout+stderr 合并流**
            │     .process_group(0)      ← 复用 adapters/tools-basic/src/bash.rs:247 的做法
            │     .spawn()               ← 同步 syscall，~1ms
            └─ 返回 BgHandle(N)；**父进程侧 fd = 0**
   └─ 工具返回文本（handle + pid + 输出路径 + 寿命上限）→ Step 9 回喂给模型
      ⇒ 整个过程**不等命令结束**：loop 关键路径上只多了 ~1ms

   **两条契约（与 `bash` 对齐，写进 §4.6）**：
   ① **cwd**：用**工具自带的 workspace**（建表时固定），**不是** `ctx.cwd` ——
      与 `adapters/tools-basic/src/bash.rs:235` 的 `current_dir(&self.workspace)` 同一条；
      否则同批调用里两条命令可能落在不同目录，模型会误判。
   ② **输出是 stdout+stderr 的合并流**（既有 `bash` 是分开收再拼，
      `adapters/tools-basic/src/bash.rs:40-49`）⇒ 后台场景下两者按写入顺序交错，
      模型无法区分来源 —— 这是后台形态**必须接受**的差异，契约里要写明。

② 命令在后台跑（内核写文件）；模型继续干别的（read/grep/find…）
   └─ sweeper 任务（**全局 1 个**）每 100ms 调 sweep()：
      ├─ stat 输出文件大小 → 超上限 ⇒ **先杀进程组**，再收尸，记为失败
      └─ child.try_wait() → 已退出则取退码、收尸
      ⚠ **顺序是承重的**：先杀后收。反了就命中 `adapters/tools-basic/src/bash.rs:158-162`
        的告诫 —— leader 一旦被收尸，pid 就可能被复用，`kill -9 -<pid>` 会打到无关的进程组。
        这条告诫必须**继承**到新模块（同一份内核、同一条注释）。
      ⚠ 2 MiB 闸门是**轮询式**的 ⇒ 两跳之间可瞬时超限 ⇒ 启动命令时用
        `ulimit -f`（POSIX sh 支持）让内核在写超限时以 `SIGXFSZ` 杀掉进程 ⇒
        **超限即失败**，不静默截断（与本仓「不得静默成功」的立场一致）。

③ 模型发 bash_poll(handle="bgN")（**与其它调用同一条消息** ⇒ 不占额外轮次）
   └─ BashPollTool::call
      ├─ snapshot(handle) → 状态 / 退码 / 已交付偏移（锁内，O(1)）
      ├─ tokio::fs: open + seek(consumed) → 读到**最后一个完整行**为止（≤ 50 KiB）
      │           ⇒ **不切半行、不切多字节字符**（AGENTS.md「流式/字节路径注意 UTF-8 边界」
      │             要求只在完整行上解码）；余下字节留给下一次 poll
      ├─ ys_tool::truncate::head_tail(...) 组装（同一 2000 行 / 50 KiB 预算，不放宽）
      │     ⇒ 头尾混合**会跳过中间段** ⇒ 跳过的字节数必须**显式报出**（`skipped_bytes`）
      └─ 锁内推进 consumed ← size（文件尾）

   **交付语义（三条，缺一不可）**：
   - **已送达的字节不重复送达**（consumed 单调推进 ⇒ 总拷贝 O(n)）。
   - **未送达的字节必须明示**：凡未随本次结果交付的字节（头尾裁掉的中间段），
     结果里要报出数量与**完整输出的路径** ⇒ 模型的 `grep` / `read` 逃生门才有效。
   - **不静默丢弃**：头尾裁剪与字节预算都**不放宽**，裁掉就是裁掉，但**要报出来**。

   ⚠ 「consumed ← size」与「宁可重复不可跳过」**不能同时成立** ——
   头尾混合本就跳过中间段，再把 consumed 推到文件尾 ⇒ 中间段**永久丢失**且无从得知。
   ⇒ 采用上表的组合：**推进到文件尾 + 显式报出跳过量**。

④ 模型发现聚合计数没拿到 ⇒ **不再重跑命令**，改用已有工具读同一份输出：
   grep(pattern="^test result", path="<bg_root>/<session_id>/bg<N>.log")
   ⇒ G1 的「同一条命令跑两遍」（58.4s = 13.5%）在此被**结构性**掐断
```

**为什么这样能省时间**：命令的 27–31s 从「loop 的串行段」搬到「与模型生成并行的后台段」。

### 4.4 核心接口

#### `crates/ys-tool/src/truncate.rs`（新增）

```rust
/// 头尾混合截断的结果：**不含任何通知文案**（文案由调用方拼 —— 与本模块既有分工一致）。
pub struct HeadTail {
    pub head: String,
    pub tail: String,
    pub omitted_lines: usize,
    pub omitted_bytes: usize,
    pub total_lines: usize,
    pub shown_lines: usize,
}

/// 头尾混合截断：字节预算**不放宽**，行数预算**不放宽**。
/// `head_bytes` 是分配给头部的字节（其余给尾部）。
pub fn head_tail(text: &str, max_lines: usize, max_bytes: usize, head_bytes: usize) -> HeadTail;
```

#### `crates/ys-tool/src/context.rs`（改：加一个字段）

```rust
pub struct ToolContext<'a> {
    /* …既有字段不动… */
    /// 本次调用的**外层工具预算**（`ys-loop` 的 `tool_budget(tool, limits)` 的结果）。
    ///
    /// `None` = 上层未声明预算 ⇒ 工具按自己的语义兜底。
    /// **只读**（`Option<Duration>` 是 Copy）⇒ 与 `boundary` / `summarizer` / `subagent`
    /// 三个既有可选挂载点同形；**不进 `ToolSpec`** ⇒ 工具表指纹与父表前缀都不受影响。
    pub tool_budget: Option<Duration>,
}
```

**为什么是它，而不是工具自己缓存预算**：预算**每回合刷新**
（`apps/coding-agent/src/app_loop.rs` 每回合重算 limits），而工具是**建表期产物**
⇒ 工具里缓存的任何预算都会过期。

**字段名与既有命名的关系**：`ToolContext` 现有 5 个字段，其中**可选挂载点 3 个**
（`boundary` / `summarizer` / `subagent`）、2 个 owned 路径字段（`cwd` / `workspace_root`）。
本字段是**第 4 个可选挂载点**（编号按「可选挂载点」计，不按字段总数计）。

#### `adapters/tools-basic/src/background.rs`（新增）

```rust
/// 模型可见的后台命令句柄。**进程内单调递增、永不复用**。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BgHandle(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgState {
    Running,
    /// 已退出；`None` = 被信号杀死。
    Exited(Option<i32>),
    /// 被 harness 主动杀掉（会话结束 / abort / 输出超限 / 超时）。
    Killed,
}
// 启动失败**不产生条目**：`start()` 直接返回 `Err` ⇒ 注册表里不会有「永远就绪」的句柄
// （故没有 `Failed` 变体）。

/// 后台命令条目（**会话级**内容）：注册表对象是进程级，条目随会话结束而失效。
struct BgEntry {
    /// 启动时的会话标识 —— **poll 时必须核对**。
    ///
    /// 句柄不复用是第一道防线；这条是第二道：即便某个清理点漏了，
    /// 上个会话的句柄也**命中不到**本会话的条目（只会得到「未知句柄」）。
    session: String,
    /* …句柄、pid、命令、cwd、状态、偏移… */
}

/// 状态快照：**不含输出正文**。
#[derive(Clone, Debug)]
pub struct BgSnapshot {
    pub handle: BgHandle,
    pub pid: u32,
    pub command: String,
    pub cwd: PathBuf,
    pub state: BgState,
    pub elapsed: Duration,
    pub output_path: PathBuf,
    pub output_bytes: u64,
    pub consumed: u64,      // 已交付给模型的字节数（读偏移）
}

/// 后台命令额度（进程级，构造时冻结）。
pub struct BgLimits {
    pub max_running: usize,        // 4
    pub max_entries: usize,        // 8
    pub max_output_bytes: u64,     // 2 MiB
    pub default_timeout: Duration, // 900s
    pub max_timeout: Duration,     // 3600s
}

/// 后台命令注册表（**进程级**；内容是**会话级**）。
pub struct BackgroundTasks { /* Mutex<Inner> */ }

impl BackgroundTasks {
    pub fn new(dir: PathBuf, limits: BgLimits) -> Self;
    /// 启动并**立刻**返回句柄。失败**同步返回 Err**。
    pub fn start(&self, session: &str, command: &str, cwd: &Path, timeout: Option<Duration>)
        -> Result<BgHandle, BgError>;
    /// 状态快照（顺带回收已退出的子进程）。
    /// `session` 不匹配 ⇒ 返回 `None`（对模型表现为「未知句柄」）。
    pub fn snapshot(&self, handle: BgHandle, session: &str) -> Option<BgSnapshot>;
    /// 杀掉一个后台命令的**整个进程组**。
    pub fn kill(&self, handle: BgHandle, session: &str) -> bool;
    /// 杀掉**某会话**的全部（abort / `/new` / `/resume` / 退出）。
    ///
    /// 按会话过滤（不是全局清空）⇒ 漏掉的清理点不会波及别的会话。
    pub fn kill_all(&self, session: &str);
    /// 清扫：收僵尸 + 执行输出上限闸门 + 淘汰过期条目。
    pub fn sweep(&self);
}

/// 进程组清理（与 `adapters/tools-basic/src/bash.rs:167` 共用同一份内核）。
pub(crate) fn kill_group(pid: u32);
```

#### `adapters/tools-basic/src/bg_tools.rs`（新增）

```rust
pub struct BashBgTool   { backgrounds: Arc<BackgroundTasks> }
pub struct BashPollTool { backgrounds: Arc<BackgroundTasks> }

impl Tool for BashBgTool {
    fn spec(&self) -> ToolSpec;
    fn timeout(&self) -> Option<Duration> { Some(Duration::from_secs(30)) }
    /// 启动会 spawn 进程 ⇒ 有副作用 ⇒ **永不准开并行**。
    fn is_concurrency_safe(&self) -> bool { false }
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}

impl Tool for BashPollTool {
    fn spec(&self) -> ToolSpec;
    fn timeout(&self) -> Option<Duration> { Some(Duration::from_secs(35)) }
    /// 虽对模型是只读，但它推进注册表里的交付偏移（**模型可观测**：重复看到同一段）
    /// ⇒ 按 `crates/ys-tool/src/lib.rs:53` 的准入纪律判**不安全**。
    fn is_concurrency_safe(&self) -> bool { false }
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}
```

> `BashPollTool::timeout()` 的 35s 来自「`wait_seconds` ≤ 20 + 余量」。
> **为什么上限是 20 而不是 60**：一次模型往返实测均值 **17.3s**
> （`context.md` 对照表「每次响应平均墙钟」）。等 60s 只为省一次 17.3s 的往返 ⇒ **期望为负**；
> 等 20s 仍小于一次往返，期望才可能为正。且 `wait_seconds` 的等待必须用
> `tokio::time::sleep`（异步），不得阻塞 runtime 线程。
> 另：`Tool::timeout()` **不进 `ToolSpec`**，故不污染工具表 JSON。

### 4.5 状态与生命周期

**存放点：进程级注册表 + 会话级内容。** 这是对「工具表是进程级」这条约束的正面处理：

| 角色 | 落点 | 生命周期 |
|---|---|---|
| 注册表**对象** | `Arc<BackgroundTasks>`，`main` 里建一次 ⇒ 随 `AgentFactory` 冻结 | 进程 |
| 注册表**内容** | 后台命令条目（**每条带 `session` 字段**） | **会话** |
| 句柄分配 | `BgHandle(u32)` **单调递增、永不复用**（`/new` 不重置） | 进程 |

**两道防线**：模型拿着上个会话的旧句柄来 poll，**不可能命中新会话的别的命令** ——
只会拿到一条明确的「未知句柄」错误。句柄不复用是第一道；**条目带 `session` 字段、
poll 时核对**是第二道：即便某个清理点漏了（新增清理路径、panic 路径），
上个会话的句柄也命中不到本会话的条目。

⇒ 相应地，`kill_all` 收成 `kill_all(session)`：**按会话过滤**，漏掉的清理点不会波及别的会话。

**落盘目录**：`<bg_root>/<session_id>/bg<N>.log`。`bg_root` = `$YUSHAN_BG_DIR`（**须绝对路径**，
与 `YUSHAN_FETCHED_DIR` 同规矩）或 `<temp_dir>/yushan-bg`。
**不用相对路径** —— coding agent 的 CWD 就是用户的仓库，相对路径会把产物落进用户的 git 工作区
（`apps/coding-agent/src/main.rs` 的 fetched 目录约定已把这条理由写死）。
会话目录在 `/new` 时整体删除；`bg_root` 下残留旧目录在**启动期**清扫（照 `sweep_fetched` 的形态）。

**六个清理点**（时机各不相同，全部显式）

| 时机 | 落点 | 动作 |
|---|---|---|
| **abort** | `apps/coding-agent/src/app_loop.rs:914` 的 `Boundary::Abort` 臂 | `kill_all(session)` |
| **回合预算到点** | `app_loop.rs` 的 `sleep_until(deadline)` 臂（同样 push `Boundary::Abort`） | 同一路径 ⇒ 同样 `kill_all(session)` |
| **`/new`** | `apps/coding-agent/src/app_loop.rs:463` | `kill_all(session)` + 删除该会话输出目录 |
| **`/resume`** | `apps/coding-agent/src/app_loop.rs:517` | `kill_all(session)`（旧工作区的命令对新工作区无意义） |
| **进程退出** | `apps/coding-agent/src/main.rs:1971` **之前** | `kill_all(全部)`（`std::process::exit` 不跑析构 ⇒ `Drop` 兜底在此路径无效） |
| **panic（unwind）** | `impl Drop for BackgroundTasks` | 兜底杀全部进程组（`Drop` 只能用同步 kill） |

**为什么不用 `ToolContext` 的第 4 个挂载点**（Q3 的答案）：
`ToolContext` 的**可选挂载点**全是**只读借用**（`crates/ys-tool/src/context.rs:8-26`），
而 `ToolContext` 由 `BasicLoop` 在 Step 9 **逐次构造**（每次工具调用新建一份），
**没有任何会话级持有者**。走挂载点路线要同时改 `ys-tool`（trait）、`ys-component`（`RuntimeContext`）、
`ys-runtime`（`AgentPorts`）、`ys-loop`（构造点）**四个 crate**才能拿到一个会话级所有者 ——
**blast radius 远大于收益**。而进程级注册表 + app 层六个显式清理点达到同样的生命周期语义。

> **与 §3.5 修正 6 不矛盾**：D1 的修法确实要动 `ToolContext` —— 但那是**传一个只读预算**
> （`Option<Duration>`，Copy，无生命周期），**不是**「挂载点持有会话级状态」。
> 两者是不同的事：**预算**要穿过 `ToolContext`，**会话级持有者**不能。

**子 agent**：子表 9 元（`apps/coding-agent/src/assembly.rs:99` 的 base 9 +
`apps/coding-agent/src/subagent.rs:400` 的子表装配）**不含** bg 工具 ⇒ 子 agent **天然没有**该能力。
（这比候选 B 的「挂载点为 `None` ⇒ 明确错误」更简单：工具压根不存在。）

### 4.6 模型可见契约

**`bash_bg`**

```json
{
  "name": "bash_bg",
  "description": "Start a shell command in the background and get a handle back immediately; the command keeps running while you do other work. Collect it later with bash_poll. Use this for commands that take more than a few seconds (test suites, builds); use bash for quick commands.",
  "parameters": {
    "type": "object",
    "properties": {
      "command": { "type": "string", "description": "Shell command to execute" },
      "timeout": { "type": "number", "description": "Max lifetime in seconds (default 900, max 3600)" }
    },
    "required": ["command"]
  }
}
```

返回（成功）：

```
Started in background.
handle: bg7
pid: 41233
cwd: /Users/zs/Develop/YuShan
output: <bg_root>/<session_id>/bg7.log
Collect it with bash_poll(handle="bg7"). The command is killed when this session ends, is aborted, or exceeds its timeout.
```

返回（失败，**同步报错**，绝不返回「永远就绪」的句柄）：

```
[bash_bg] too many background commands are running (4) — poll or kill one first
[bash_bg] failed to start command: <原因>
```

**`bash_poll`**

```json
{
  "name": "bash_poll",
  "description": "Check a background command started with bash_bg: returns its state plus the output produced since the last poll. The full output stays on disk — grep or read that path for any part you missed instead of running the command again.",
  "parameters": {
    "type": "object",
    "properties": {
      "handle": { "type": "string", "description": "Handle returned by bash_bg" },
      "wait_seconds": { "type": "number", "description": "Wait up to N seconds for the command to finish (default 0, max 20)" }
    },
    "required": ["handle"]
  }
}
```

| 场景 | 返回 | `is_error` |
|---|---|---|
| 仍在跑 | `[bg7] running — 12.3s elapsed — 41.2 KiB produced, 41.2 KiB new` + 增量摘录 | false |
| 已结束 | `[bg7] finished — exit code 0 — 27.4s — 118.2 KiB produced, 77.0 KiB new` + 增量摘录 + **完整输出路径提示** | false |
| 已结束且退码 ≠ 0 | 同上，**退码原样带出** | **true** |
| 被杀 / 超限 / 超时 | `[bg7] failed — killed after 900s (lifetime exceeded)` / `[bg7] failed — killed: output exceeded 2 MiB` | **true** |
| 句柄不存在 | `[bash_poll] unknown handle "bg9" — running handles: bg7` | **true** |

**四条设计取舍**：

1. **未送达的字节要报出来。** 头尾裁剪跳过中间段时，结果里给出
   `[bg7] 38.0 KiB 未送达（头尾裁剪）—— 完整输出见 <path>`。
   模型用 `grep` / `read` 指向那个路径取任意片段 ⇒ **「不静默丢弃」是硬要求**。
   （交付语义见 §4.3 ③）
2. **「被 harness 杀掉」与「命令自身非零退码」必须用不同措辞。** 否则模型会把被杀误报成「测试挂了」——
   这正是本仓明确反对的「静默成功」类问题的变体（research M7）。
3. **完整输出永远给路径。** 模型要别的片段就用已有的 `read` / `grep` 指向那个路径 ⇒
   **G1b 的重复执行被结构性掐断**，不依赖提示词是否奏效。
4. **`wait_seconds` 默认 0、上限 20。** 它是**有界 join**（等真实后台任务），不是前台 sleep；
   默认 0 是为了不让模型把「阻塞」重新引回来。
   **上限 20 而不是 60**：一次模型往返实测均值 17.3s ⇒ 等 60s 只为省一次往返，期望为负；
   20s 仍小于一次往返。等待用 `tokio::time::sleep`，不阻塞 runtime 线程。

**两条 guideline**（`apps/coding-agent/src/prompt.rs` 的 `tool_guidelines`，按工具名派生）：

```
- Run a command that takes more than a few seconds with bash_bg, then keep working; collect it
  with bash_poll **in the same batch as your other calls** — the command runs while you do
  something else. Never run the same long command twice: its full output stays on disk, so grep
  or read that path for the part you missed instead of re-running it.
- Never use sleep to wait for something: start it with bash_bg and collect it with bash_poll.
```

第一条同时覆盖三件事：后台化、**合批收轮**、不重复执行；
第二条是 research M5 的低成本形态（`sleep 30` 今天是真的睡满 30s）。

**两条 guideline 只对父表生效**（子表 9 元不含 bg 工具 ⇒ 子 agent 看不到这两条）。

### 4.7 资源预算

| 资源 | 上限 / 数值 | 依据 |
|---|---|---|
| **并发在跑的后台命令** | **4** | 与 `DEFAULT_MAX_PARALLEL_TOOLS = 4`（`crates/ys-component/src/limits.rs:8`）同量级。超限 ⇒ `bash_bg` **同步报错**（不排队、不静默）。 |
| **句柄表条目** | **8**（在跑 + 已完成未取走） | 满了 ⇒ 淘汰**最旧的已完成**条目（删文件）；若全在跑 ⇒ 报错。 |
| **单命令输出（磁盘）** | **2 MiB** | 超出 ⇒ 杀进程组 + 记 `failed`。`cargo test --workspace` 1295 个用例 ≈ 90 KiB 输出，2 MiB ≈ 20× 余量。闸门由**启动时的 `ulimit -f`**（内核侧，POSIX sh 支持）+ sweeper 的 `stat` 双保险 ⇒ 轮询间隙的瞬时超限由内核兜住。 |
| **磁盘总量** | ≤ 8 × 2 MiB = **16 MiB** | 会话结束删目录 ⇒ 稳态更低。 |
| **父进程 fd（稳态）** | **0 / 条** | 子进程持有 stdout/stderr；父侧随 `spawn` 关闭。 |
| **父进程 fd（轮询瞬时）** | **+1** | 读完即关。 |
| **tokio 任务** | **1 个共享 sweeper**（不是每条一个） | 100ms tick；8 条 ⇒ 160 syscall/s ≈ **0.16 ms/s ≈ 0.016% 单核**。 |
| **常驻内存** | 8 条 × ~200 B + sweeper ~8 KB ≈ **< 10 KB** | **不缓存输出正文**；单次轮询临时缓冲 ≤ 50 KiB（读完即弃）。 |
| **单次轮询读** | **≤ 50 KiB** | 与 `MAX_BYTES` 同额（`crates/ys-tool/src/truncate.rs:24`）；行数同样受 `MAX_LINES = 2000`（`:22`）约束 ⇒ **不放宽**。读**只到最后一个完整行**为止 ⇒ 不切半行、不切多字节字符。 |
| **轮询总拷贝** | **O(n)**（增量 + 一次收尾） | 第 k 次轮询只读 `[consumed_k, size_k]`。被头尾裁掉的字节**不重复读**，但**必须报出跳过量**（§4.3 ③）。 |
| **上下文 token** | **+230 / 轮**（2 spec + 2 guideline），全在缓存前缀 | 未命中增量 ≈ 0；`fixed_prompt_tokens` 同步 +230 ⇒ 压缩触发点位移 0.09%（窗口 258400）。 |

**fd 泄漏逐项交代**：管道读端 —— **本方案没有管道**；进程组 —— 不占 fd（只是 pgid）；
信号 —— `sh -c "kill -9"` 是短命进程；输出文件 —— 父侧 `Stdio::Owned` 在 spawn 后关闭，轮询时按需开关。
⇒ **稳态 fd 增量 = 0。**

### 4.8 与既有行为的关系

| 面 | 结论 |
|---|---|
| `bash` 的命令执行语义 | **不变** —— 只把 `kill -9 -<pid>` 的实现提取共享（`adapters/tools-basic/src/bash.rs:167`），命令串、cwd 契约、进程组、`kill_on_drop` 全不动 |
| `bash` 的输出字节 | **不变** —— 截断仍走 `ys_tool::tail`（`adapters/tools-basic/src/bash.rs:49` → `crates/ys-tool/src/truncate.rs:131`）；新增的 `head_tail` **只被新工具调用** |
| `bash` 的 spec / 工具表 | **不变** ⇒ `apps/coding-agent/src/toolset.rs:601-606` 的指纹锚点 `0xeced4483ff29ef22` **不动** |
| 「不做后台时逐字节不变」 | **✔ 成立** |
| `ys-loop` 轮次模型 | **零改动** —— 「工具全部完成才回喂」原样保留；**未引入「工具未完成」状态**。（Step 9 会多填一个 `ToolContext` 字段，**不碰**轮次编排。） |
| 核心 crate | `ys-core`/`ys-event`/`ys-protocol`/`ys-model`/`ys-session`/`ys-component`/`ys-runtime`/`ys-tui-coding` **零改动**；**`ys-tool` 加一个纯函数 + `ToolContext` 一个只读字段**；**`ys-loop` 填一处**（§3.5 修正 6）。`ToolContext` 是 `#[non_exhaustive]` ⇒ 加字段不破坏外部实现。 |
| 并行准入 | 既有工具不变；新工具：`bash_bg` 不 safe、`bash_poll` 不 safe（§3.3 校正 3） |
| 回合级输出闸门 | 不变（`context_window / 4`，`apps/coding-agent/src/main.rs:1034`） |
| 子 agent | 工具表仍 9 元 ⇒ **无后台能力**（显式非目标） |
| Agent 无状态 | 注册表既不在 `Agent` 里，也不在 `RuntimeContext` / `ToolContext` 里；`Tool::call(&self, …)` 签名不变 |
| 取消即消息 | 不引入 `CancelToken`；abort 走既有 `Boundary::Abort` 通道，只把清理挂在同一处 |
| 既有断言与文档 | `docs/design.md` §12（`:537`）的「父 10 元 / 子 9 元」→ **父 12 / 子 9**；`docs/design-final/coding-agent-tui.md` 的「父工具表 = 基础 n + `SubagentTool` 追加末位」→ 基础 n + bg×2 + `SubagentTool`。`apps/coding-agent/src/toolset.rs:596` 的 9 元顺序用例**不动**，但它**自拼父表** ⇒ 插入新工具后照样全绿却不再描述真实父表 ⇒ **需补一条 assembly 级断言**（真实父表 JSON 以子表 JSON 去末尾 `]` 开头、`subagent` 仍是末位） |

### 4.9 切片与落地顺序

| 切片 | 内容 | 依赖 | 验收 |
|---|---|---|---|
| **0a** | **修 D1**：外层工具预算经 `ToolContext` 传给工具（`ys-tool` 加字段 + `ys-loop` 填一处）；内层 deadline = `min(模型值, 外层 − Δ)`，**模型未传值时也用外层** ⇒ 工具**始终**有 deadline（§3.5 修正 4/5） | 无 | 两条用例：① 模型传 `timeout: 900` ⇒ 实际在 ~295s 被砍、文案写**生效值**、**进程组被杀**；② 模型**不传** `timeout` ⇒ 外层预算到点时**进程组被杀**（不靠 `kill_on_drop`） |
| **0b** | **修 D2**：无 boundary 路径的超时补上进程组杀（`adapters/tools-basic/src/bash.rs:265-268`），与有 boundary 路径同款内核 | 无 | 用例：`bash("sleep 60 & wait", timeout 1)` ⇒ **孙进程**（`sleep`）不在了 |
| **0c** | **G3 窗口声明**：把 `minimax-m3.1-flash-free` 的窗口从 128000 提到实测可用值，走**已有**的用户级 `windows` 覆盖；并把「窗口值从哪来」写进 `/model` | 无 | 长会话跑到 150k token ⇒ **不**在 108800 处被静默压缩 |
| **0d** | **提示词两条准则**（§4.6） | 无 | 同批 prompt 对照：重复执行次数 1 → 0 |
| **0e** | **debug 度量补齐**（§6） | 无 | 日志里能直接读出三个指标 + **按工具名分桶**的耗时 |
| **0f** | **文档同步**：`AGENTS.md` 环境变量段登记 `YUSHAN_BG_DIR`；`docs/design.md` §12 与 `docs/design-final/coding-agent-tui.md` 的父表元素数 | 无 | 三处都能查到同一份数字 |
| **1** | `bash_bg` + `bash_poll` + 文件后端 + 六个清理点 | 0a–0e | §6 的前后对照 |
| **2** | `/chat/completions` vs Responses API 的同 prompt A/B（独立切片，不阻塞 0/1） | 无 | 把 G2 的三个候选归因（协议面 / 提示词面 / 服务端排队）**分开** |

**0a–0e 是 P1 的前提**：没有 0e 的度量，1 的收益无法证明；没有 0a/0b，后台化会把
「外层预算抢占 + 孤儿进程」这两个既存缺陷放大到跨轮存活。

---

## 5. 附带修正（两条既存缺陷）

这两条**不是**后台执行带来的，是探索过程中从实测会话里挖出来的**既存**问题，
与 P1 解耦，**可独立落地**：

> 本节按 §3.5 修正 4/5 重写过一次：**deadline 的归属**从「模型传了才有」改成
> 「工具内**始终**有」，并把钳位写成「**严格小于 + 余量**」。
> 旧写法只覆盖「模型传了 `timeout`」那一支 ⇒ 「不传」那一支仍会留孤儿进程。

### D1 · 模型的 `timeout` 参数与外层工具预算脱钩

- **现状**：`crates/ys-loop/src/basic.rs:47-50` 的 `tool_budget` = `tool.timeout()` → `limits.bash_timeout` → 300s。
  `BashTool` 未覆盖 `timeout()` ⇒ **恒为 300s**。模型在 `timeout` 参数里传的值**只作用于内层**。
- **后果**：模型传 `timeout: 900` ⇒ 内层 deadline 900s，但外层 300s 先到 ⇒ 工具 future 被 drop
  ⇒ 走的是**不杀进程组**的那条路径（见 D2）⇒ 孤儿进程。
  模型对此**毫不知情** —— 它以为自己有 900s。
- **修法（三步）**：
  1. **预算进工具**：外层预算经 `ToolContext` 的新字段传给工具
     （`ys-tool` 加字段 + `ys-loop` 填一处，§3.5 修正 6）。
  2. **工具内始终有 deadline**：`effective = min(模型值, 外层预算 − Δ)`；
     **模型未传值时取 `外层预算 − Δ`**（不是「没有 deadline」）。
     `Δ` 取 5s 量级 ⇒ 内层**严格早于**外层到期，不会出现同刻竞态（§3.5 修正 5）。
  3. **文案写生效值**：超时错误由工具自己产出（`Command timed out after <effective> seconds`），
     而不是 loop 的 `Tool '<name>' timed out after <budget>`（`crates/ys-loop/src/basic.rs:86`）。
     ⇒ 模型看到的是**实际生效的**秒数，不是它传的值。
- **验收（两条）**：
  ① 模型传 `timeout: 900` ⇒ 实际在 ~295s 被砍、文案写 ~295、**进程组被杀**；
  ② 模型**不传** `timeout` ⇒ 外层预算到点时**进程组被杀**（不靠 `kill_on_drop`）。

### D2 · 无 boundary 路径的超时不杀进程组

- **现状**：`adapters/tools-basic/src/bash.rs:265-268` 的 `#[cfg(unix)]` 块是**空的**（只有一句注释）；
  有 boundary 路径在 `adapters/tools-basic/src/bash.rs:353` 才 `kill_process_group(pid).await`。
  `kill_on_drop(true)`（`adapters/tools-basic/src/bash.rs:240`）只杀**直接子进程** `sh`。
- **后果**：`ctx.boundary` 为 `None` 时（`-p` / `--json` 路径与任何未挂边界的调用），
  一条命令超时 ⇒ `sh` 被杀，但 `cargo` / 测试二进制等**孙进程继续跑到自然结束**
  ⇒ 孤儿进程 + 端口/文件锁占用。
  **注意**：`ctx.boundary` 为 `Some` 时**也有**同一个缺口 —— 外层预算到点 drop future 时，
  走的同样是 `kill_on_drop`（杀 `sh`），**不经**那条会杀进程组的分支。
- **修法**：在无 boundary 路径的超时分支里同样调用进程组杀（`kill_process_group` 的**同步内核**提取共享）；
  配合 D1 的「始终有 deadline」，**四条组合**都收敛到「工具自己的超时分支 ⇒ 杀进程组」：

  | 模型传 `timeout` | `ctx.boundary` | 工具内 deadline | 到点动作 |
  |---|---|---|---|
  | 传 | 有 | `min(模型值, 外层 − Δ)` | 工具超时分支 ⇒ **杀进程组** |
  | 传 | 无 | 同上 | 同上 |
  | 不传 | 有 | `外层 − Δ` | 同上 |
  | 不传 | 无 | `外层 − Δ` | 同上 |

  ⇒ **不再依赖 future 被 drop 时的 `kill_on_drop` 副作用**（那只是兜底，不是契约）。
- **验收**：`bash("sleep 60 & wait", timeout 1)` ⇒ 断言 `sleep` 进程不存在；
  再加一条「不传 `timeout` + 外层预算到点 ⇒ 断言孙进程不存在」。

---

## 6. 验收口径

**基线必须在改动之前采。** 本文引用的对照只有一个真实会话（`1792506807_969975000`），
样本量不足以当基线。

### 6.1 度量补齐（切片 0e）

在 `yushan.debug/1`（`apps/coding-agent/src/debug_log.rs:48`，**同一 major 内只许加字段** ⇒ 可直接加）
里补四个可判定指标（**其中三个的原始数据已有**，只有「重复命令数」需要新字段）：

| 指标 | 定义 | 落点 |
|---|---|---|
| **重复命令数** | 同一回合内**同一条命令串**出现 >1 次的次数 | ⚠ **不能用 `args_original_bytes`** —— 它是**整个参数对象的字节数**（`apps/coding-agent/src/debug_log.rs:1069`），不是命令串的指纹 ⇒ 不同命令同长度会**碰撞**；命令串本身在默认 `head` 档被截断成「头 160 + 尾 64」（`apps/coding-agent/src/debug_log.rs:1147`）⇒ 两条不同的长命令会被误判为重复。⇒ 切片 0e **要新加一个字段** `command_fp`（命令串的 `fnv1a64` 指纹；**不落正文** ⇒ 不泄密、不增体积；`fnv1a64` 的现成实现在 `apps/coding-agent/src/toolset.rs:546`，但那是 `#[cfg(test)]` 内的 ⇒ 生产代码要自己放一份） |
| **真命令耗时** | 单条工具调用耗时超过阈值（如 5s）的分布 | 已有：`tool_result.duration_ms`（`apps/coding-agent/src/round_clock.rs:214`） |
| **按工具名分桶的耗时** | 逐调用耗时按工具名分桶（`bash` / `bash_bg` / `bash_poll` / 其它） | **已有、可离线分桶、无需新字段**：`tool_call` 带 `gen_ai.tool.name` + `tool_call_id`（`apps/coding-agent/src/round_clock.rs:181`），`tool_result` 带 `tool_call_id` + `duration_ms`（`:224`）⇒ 按 id 关联即得。**必须有它**，否则分不清「那 58.4s 是经 `bash` 阻塞的还是经 `bash_bg` 的」，主假设就无法证伪 |
| **工具阻塞占比** | `tools_ms` / 回合墙钟 | 已有：`round_end.tools_ms`（`apps/coding-agent/src/round_clock.rs:362`）与 `turn_end.elapsed_ms`（`apps/coding-agent/src/main.rs:890`） |

### 6.2 前后对照

同一批真实 prompt，逐个进程跑，记录：

| 指标 | 期望 |
|---|---|
| 回合墙钟 | 下降 |
| 工具阻塞占比 | 下降 |
| **重复命令数** | **1 → 0**（这是 G1b 的直接判据） |
| **经 `bash` 执行且耗时 > 5s 的调用数** | **→ 0**（主假设「长命令不再阻塞 loop」的**直接**判据；靠 §6.1 的分桶） |
| **`bash_poll` 所在 assistant 消息的调用数** | **> 1**（合批的判据 —— §3.3 校正 2） |
| 后台命令的**孤儿进程数**（退出后 `pgrep` 复查） | **0** |
| **未送达字节的报出** | 每次 poll 的结果里都报出 `skipped_bytes`（裁剪时）⇒ **不得静默丢弃** |
| 答案质量 | **不可只看墙钟** —— 必须同时看「结论是否与证据一致」 |

> 最后一条来自 research §2.1 的记录：对照 harness 正是因为只看了墙钟，
> 才写出「`cargo test --workspace` 全绿」这句与自己证据矛盾的话。

### 6.3 P2 的 A/B

同模型、同 effort、同提示，逐个进程跑，对比：墙钟 / 推理 token / 吞吐 / **轮数** / 质量。
目标是把 G2 的三个候选归因**分开**，而不是直接得出「换协议更快」。

---

## 7. 残留风险（明示）

1. **模型忘记轮询 ⇒ 结果丢失。** 缓解：`bash_poll` 返回里带路径与句柄、guideline 要求收尾前取回、
   回合末在日志里记「还有 N 个后台任务在跑」（日志，不进模型上下文）。**根治需要通知注入，
   本轮不做**（架构级，见 research §4）。
2. **后台任务与后续工具调用并发写同一工作区 ⇒ 竞态。** 绕过了「同批串行」闸门 ——
   那道闸门只管**工具调用之间**，管不住**进程存活期间**。只能靠提示词约束。
3. **轮询若不合批 ⇒ 净收益可能为负。** 每次轮询付一次模型往返（17.3s 量级）。
   缓解是 guideline，但**不可保证** ⇒ 6.2 把「合批」列为独立判据。
   `wait_seconds` 同理：上限已从 60s 收到 20s（§4.6），但**模型仍可传 20** ⇒
   最坏情况一轮里多等 20s。缓解同一条 guideline（默认 0）。
4. **句柄表占满 ⇒ 模型被卡住。** 满了先淘汰最旧的**已完成**条目；若全在跑 ⇒ 报错。
5. **脱离进程组的子进程逃逸。** 进程组清理覆盖不到自行 `setsid` / daemonize 的命令。
6. **后台输出是 stdout+stderr 的合并流。** 既有 `bash` 分开收再拼（`adapters/tools-basic/src/bash.rs:40-49`），
   后台形态下两者按写入顺序交错 ⇒ 模型**无法区分来源**。这是后台形态**必须接受**的差异（已写进契约）。
7. **头尾裁剪会跳过中间段。** 缓解是「显式报出跳过量 + 给完整路径」⇒ 模型用 `grep` / `read` 补齐；
   但**模型是否真的去补**取决于它读不读那行提示 —— 这是提示词问题，无机制性解法。

---

## 8. 明确不做的方向（含理由）

| 不做 | 理由 |
|---|---|
| **通知注入**（后台完成自动进对话） | 需要 loop 有「回合内非工具来源消息」的通道 ⇒ 架构级改动，**触碰「事件只观察，Hook 才控制流程」的硬边界**。成本差一个数量级。 |
| **重开「减少轮数」** | `explore-speed` 已有成本模型与推理强度 A/B（ADR-0042 定 `high` 默认）。 |
| **提高只读并发度** | 通道已通电，本地只读工具是毫秒项，天花板 ≈ 0。 |
| **改推理强度默认值** | ADR-0042 已定 `high`；`explore-speed` 已做过 `low`/`high` A/B。 |
| **把后台能力做成通用 trait 平面**（候选 B） | 第一个消费者只有一个；核心 crate 改动 4 → 1 的收益不抵 450 行。 |
| **`expected_seconds`（模型自报时长）** | 模型的时长估计不可靠。 |
| **硬拒绝 `sleep`** | 会改 `bash` 的既有行为；本轮只做 guideline，硬拒绝另开一刀。 |
| **放宽输出上限 / 改 `ys-loop` 轮次模型 / 加沙箱 / 引新依赖** | 硬约束。 |

---

## 9. 后续

1. 本设计交 `exec-plan` 落地切片 **0a–0f**（P0，含两条既存缺陷的修正与文档同步）。
   §3.5 的三处修正**已在设计层落定**，exec-plan 不必再重新判断 deadline 归属与钳位规则。
2. 切片 0e 的度量落地后，用它**重采基线**（多会话、多 prompt），再动切片 1。
3. 切片 2 用 `apps/coding-agent/tests/e2e_bench.rs` 的既有脚手架做 A/B；
   `YUSHAN_BENCH_ONLY` 可指定子集，`YUSHAN_BENCH_REPEATS` 控制重复次数。
4. 若切片 1 的收益被验收证伪，**回退成本很低** —— 因为 `bash` 一字未改，
   新工具可整组摘掉而不留残余。
5. `arch-validate` 的评审已出（[`review.md`](./review.md)）：R1–R3 已并入本文 §3.5 / §4 / §5；
   R4–R17 中与契约相关的条款也已补入（交付语义、完整行读、kill 先于收尸、
   会话归属、`wait_seconds` 上限、cwd 契约、stdout/stderr 合并、度量分桶、父表断言守卫、
   `YUSHAN_BG_DIR` 登记）。**评审中的「架构级」三条（adapter 承载进程级状态、
   进程组生命周期的契约化、「用句柄替代未完成状态」的显式记账）留在切片 1 落地时决策。**
