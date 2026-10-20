# execution-efficiency — 架构质量分析报告

> 日期：2026-10-20
> 对象：[`design.md`](./design.md) + [`adr-background-command-execution.md`](./adr-background-command-execution.md)
> 方法：`arch-validate`。**每条结论都回代码核实过**（行号见各条；无指针的断言不写）。
> 关注维度：可行性 / 可维护性 / 可理解性 / 性能与可靠性（全量）。
> 相关：[`context.md`](./context.md)（现状与约束）、[`research.md`](./research.md)（跨产品对照）。

---

## 分析范围

| 项 | 内容 |
|---|---|
| 对象 | 「后台命令执行 + 两条既存缺陷修正 + 窗口声明 + 度量」设计方案 |
| 维度 | 全量（含基础软件四问：错误处理 / 并发安全 / 资源管理 / 故障模式） |
| 基线事实 | 全部来自 `code/`；跨 harness 对照数据来自 `research.md`（外部记录） |

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|---|---|---|
| **1.1 技术可实现性** | 🟢 | 机制全部仓内已有：`std::process` + 进程组（`adapters/tools-basic/src/bash.rs:167`）、`head_tail` 可复用 `crates/ys-tool/src/truncate.rs:75`/`:131`；无新依赖、无新 crate |
| **1.2 依赖成熟度** | 🟢 | 零新第三方依赖；只用 `tokio` 既有 feature（`adapters/tools-basic/Cargo.toml` 已含 `process`） |
| **1.3 增量实现路径** | 🟡 | 切片 0a–0e → 1 → 2 依赖关系清晰，但 **0a 的修法与「核心 crate 零改动」自相矛盾**（R3） |
| **2.1 模块边界** | 🟡 | 「不改 `bash`、新增双工具」边界干净；但进程级状态落在**可选适配器 crate**（R10），且「会话级内容」在 API 上无落点（R6） |
| **2.2 接口稳定性** | 🟢 | `bash` 的 spec 一字不改 ⇒ 工具表指纹锚点（`apps/coding-agent/src/toolset.rs:658` 记 `0xeced4483ff29ef22`）不动；`Tool::timeout()` 不进 `ToolSpec`（`crates/ys-tool/src/lib.rs:36`） |
| **2.3 错误处理** | 🟢（**已处置**，见「处置」章 R1/R2） | 原为 🔴：**D1/D2 的修法覆盖不到「模型不传 `timeout`」这一主路径**（R1）；内层外层同刻竞态（R2）⇒ 已改为「工具内**始终**有 deadline」+「钳位严格小于外层 + 余量」 |
| **2.4 并发安全** | 🟡 | 准入纪律正确（`bash_bg`/`bash_poll` 均判 `false`，符合 `crates/ys-tool/src/lib.rs:53`）；但**增量读的行边界**未定（R5）、**kill 与收尸的先后**未定（R7） |
| **2.5 资源管理** | 🟢 | fd / 内存 / 磁盘 / tokio 任务四项均有上限与推导；**2 MiB 闸门是轮询式**⇒ 可瞬时超限（R12） |
| **3.1 概念一致性** | 🟢 | 「句柄 / 交付偏移 / 被 harness 杀掉 ≠ 命令非零退码」的措辞区分明确，是本设计最强的一处 |
| **3.2 文档完整度** | 🟡 | 「零改动」清单与 D1 修法打架（R3）；`ToolContext`「四个挂载点」措辞不准（R15） |
| **3.3 上手成本** | 🟢 | 新人可按 §4.2 的文件级改动表定位；接口草案带 doc comment 形态 |
| **4.1 性能模型** | 🟡 | 收益区间（6.3%–17.6%）有上下界推导、诚实标注了「轮询不合批则净收益为负」；但 `wait_seconds` 的期望值实为**负**（R9） |
| **4.2 故障模式** | 🟡 | 残余风险 7 条明示（「模型忘记轮询」「并发写」「句柄表占满」「逃逸子进程」「stdout+stderr 合并流」「头尾裁剪跳过中间段」）；**启动失败 / 磁盘满 / 半行**的处置已补（R5、R12、R13） |
| **4.3 可观测性** | 🟡 | 度量三指标方向对，但缺**按工具名分桶**⇒ 无法证伪主假设（R14）；bg 生命周期事件未进 debug 日志 |

**总体健康度：🟡（设计质量高、可落地；R1–R18 已全部处置，其中 R1/R2/R3 是必须改的语义缺口，已改）**

设计本身有三处判断是对的、且容易被后来者改坏：

1. **「不改 `bash`」是承重的** —— `required: ["command"]` 一旦放宽，schema 便不再拦任何东西（`adapters/tools-basic/src/bash.rs:192`）。
2. **校正 2（轮询是否占额外一轮由提示词而非并发闸门决定）** —— 依据是 `crates/ys-loop/src/basic.rs:412` 的一轮 `tool_calls` 全部取自同一条 assistant 消息。
3. **不夸大 4.9× 的归因** —— 明确说 G1 只占 13.5%、主因（G2 协议面、G4 轮数）本设计不碰。

---

## 风险排序

| 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|---|---|---|---|
| R1 D1/D2 修法覆盖不到「无 `timeout`」路径 ⇒ 孤儿进程仍在 | 高 | 高 | **P0** | 工具内**始终**有 deadline |
| R2 内层 deadline 与外层预算同刻 ⇒ 竞态回落到 drop 路径 | 高 | 中高 | **P0** | 内层 = 外层 − 余量（严格小于） |
| R3 「核心 crate 零改动」与 D1 修法自相矛盾 | 高 | 已发生 | **P0** | 二选一并改口径 |
| R4 `consumed ← size` 与头尾混合互斥 ⇒ 中间段永久丢失 | 中高 | 中 | **P1** | 定死交付语义 |
| R5 增量读切半行 / 切多字节字符 | 中高 | 高 | **P1** | 只读到最后一个完整行 |
| R18 「重复命令数」用 `args_original_bytes` 聚合算不出来 | 中高 | 高 | **P1** | 新增命令串指纹字段 `command_fp` |
| R6 「会话级内容」在 API 无落点 | 中 | 中 | **P1** | 条目带 `session_id` |
| R7 kill 与收尸的先后未定 ⇒ 可能误杀无关进程组 | 中 | 中 | **P1** | 先杀后收（继承 `bash.rs:158` 的告诫） |
| R9 `wait_seconds` 期望值为负 | 中 | 中高 | **P1** | 上限降到 20s 或取消 |
| R8 `YUSHAN_BG_DIR` 未登记进 AGENTS.md 环境变量清单 | 中 | 高 | **P1** | 同步 AGENTS.md |
| R10 adapter crate 承载进程级状态 | 中 | 已发生 | **P2** | 显式记账或改放 app |
| R11 父表前缀不变量失去测试守卫 | 中 | 高 | **P2** | 加 assembly 级断言 |
| R12 2 MiB 闸门轮询式 ⇒ 瞬时超限 | 低中 | 中 | **P2** | `ulimit -f` 或缩短跳距 |
| R13 `Stdio::from(f)` 写法编译不过 + stderr/stdout 合并语义未声明 | 低中 | 高 | **P2** | `try_clone()` + 写进契约 |
| R14 度量缺「按工具名分桶」 | 低中 | 中 | **P2** | 补一项判据 |
| R15 「四个挂载点」措辞不准 | 低 | 已发生 | **P3** | 改措辞 |
| R16 `BgState::Failed` 是死变体 | 低 | 已发生 | **P3** | 删或赋予语义 |
| R17 `bash_bg` 的 cwd 契约未写 | 低 | 中 | **P3** | 写明用工具 workspace |
| R18 「重复命令数」用 `args_original_bytes` 聚合算不出来 | 中高 | 高 | **P1** | 新增命令串指纹字段 `command_fp` |

> **状态（2026-10-21）**：R1–R9、R11–R18 **已处置**（逐条落点见文末「处置」章）；
> R10 与另两条架构级决策**已在设计层定稿**（见「处置」章末）⇒ **本表已无未决项**。

---

## 发现

### R1 · D1/D2 的修法覆盖不到「模型不传 `timeout`」这条主路径 —— 孤儿进程仍在（影响 高 / 可能性 高）

**证据**：

- `adapters/tools-basic/src/bash.rs:201`：`let timeout_secs = input["timeout"].as_u64();` —— 参数可选。
- `adapters/tools-basic/src/bash.rs:256`：`if let Some(secs) = timeout_secs { … }` —— **无 `timeout` 时走 else 分支**：`child.wait_with_output().await`，**工具内部没有任何 deadline**。
- 有 boundary 的路径同理：`adapters/tools-basic/src/bash.rs:305` `let deadline = timeout_secs.map(…)` —— 同样只在传了 `timeout` 时才存在 deadline。
- 外层预算的落点：`crates/ys-loop/src/basic.rs:62` `tokio::time::timeout(budget, tool.call(args, tool_ctx))` —— 到点即 **drop 工具 future**。

**后果链**：模型不传 `timeout` ⇒ 工具内无 deadline ⇒ 外层 300s 到点 drop future ⇒
`kill_on_drop(true)`（`adapters/tools-basic/src/bash.rs:240`）只杀**直接子进程 `sh`** ⇒
`cargo` / 测试二进制等孙进程继续跑到自然结束 ⇒ 孤儿进程 + 文件锁占用。

而设计 §5 的 D2 修法是「在**无 boundary 路径的超时分支**里补进程组杀」——
**「无 `timeout`」时根本走不到那个分支**（它是被外层 drop 的，不是自己超时的）。
⇒ D2 按现写法修完，**这条路径上的孤儿依旧存在**。

**修**：把「超时分支补杀」改成「**工具内始终有 deadline**」——
外层预算传进工具后，工具自己算 `effective = min(模型值, 外层预算 − 余量)`；
**模型未传值时用外层预算**，不再依赖 future 被 drop 时的 `kill_on_drop`。
这样「杀进程组」永远由工具自己的分支完成，而不是靠 drop 的副作用。

**验收补一条**：不传 `timeout` 的长命令（如 `bash("sleep 60 & wait")`）+ 外层预算到点 ⇒ 断言孙进程不存在。
（现有验收用例 `bash("sleep 60 & wait", timeout 1)` 只覆盖了**传了** `timeout` 的分支。）

### R2 · 内层 deadline 钳到外层「以内」会撞上同刻竞态（影响 高 / 可能性 中高）

**证据**：`crates/ys-loop/src/basic.rs:62` 的外层是 `tokio::time::timeout(budget, …)`；
工具内层是 `tokio::time::timeout(timeout, child.wait_with_output())`（`adapters/tools-basic/src/bash.rs:257`）。

若内层 deadline **等于**外层预算，两者同刻到期 ⇒ 谁先返回不确定 ⇒
外层若先赢，仍然 drop future ⇒ 回到 R1 的孤儿路径。

设计 §5 D1 写的是「内层 `timeout` **钳到外层以内**」——「以内」是否含等号未定。
按现在的写法（`timeout: 900` ⇒ 内层 300s = 外层 300s）**恰好落在等号上**。

**修**：把钳位写成「**严格小于且留余量**」：`effective = min(模型值, 外层预算 − Δ)`（Δ 取 5s 量级），
并让工具的超时文案写明生效值（模型传 900、实际 295 ⇒ 文案说 295，否则模型仍以为自己有 900s）。
这一条同时把 D1 的「模型不知情」治掉。

### R3 · 「核心 crate 零改动」与 D1 的修法自相矛盾（影响 高 / 可能性 已发生）

**证据**：

- 设计 §5 D1 的修法：「把外层预算经 `ToolContext` 传给工具」⇒ 必须给
  `crates/ys-tool/src/context.rs` 的 `ToolContext` **加字段**（该 struct 是 `#[non_exhaustive]`，加字段可行）。
- `ToolContext` 由 `crates/ys-loop/src/basic.rs` 在 Step 9 **逐次构造** ⇒ **`ys-loop` 也要改**（填字段的那一处）。
- 但设计 §4.2 的「零改动」清单排除了 `ys-tool` 与 `ys-loop`；§4.8 写「核心 crate … 零改动（`ys-tool` 只加一个纯函数）」。

⇒ **同一份文档里，§5 要求改的两个 crate 被 §4.2/§4.8 声明为不改。**

**两条出路**：

- **(a) 承认改两个核心 crate**（`ys-tool` 加一个只读预算字段 + `ys-loop` 填一处），
  把 §4.2/§4.8 的口径改成「**除 `ys-tool` 加一个字段、`ys-loop` 填一处外，核心 crate 不动**」。
- **(b) 换修法**：让 `BashTool::timeout()` 返回工具声明值、loop 侧取 min —— 但这会破坏
  `crates/ys-tool/src/lib.rs:36` 写明的既有语义（「工具声明值优先于 `bash_timeout`」，
  理由是长任务工具如 subagent 不该被 `bash_timeout` 砍掉），**属既有契约变更，须走 ADR**。

**倾向 (a)**：预算每回合刷新（`apps/coding-agent/src/app_loop.rs` 每回合重算 limits），
而工具是**建表期产物** ⇒ 任何在工具里缓存预算的静态做法都会过期。`ToolContext` 路线是唯一正确的。

### R4 · 「`consumed ← size`」与「头尾混合」互斥 —— 中间段永久丢失（影响 中高 / 可能性 中）

**证据**：设计 §4.3 数据流 ③ 同时写了两件冲突的事：

- 「read ≤ 50 KiB（头）**+** seek(size−tail) **+** read ≤ 37.5 KiB（尾）」⇒ 中间段被**跳过**；
- 「锁内推进 `consumed` ← size（**宁可重复不可跳过**）」⇒ 交付位置被推到**文件尾**。

⇒ 中间段既没交付、又被标记为「已交付」⇒ **永久丢失**，与「宁可重复不可跳过」相反。

**修**：二选一并写死。推荐 (a)：

- (a) `consumed` 只推进**实际交付给模型的字节数**；被跳过/被头尾裁掉的字节数在结果里**显式标注**
  （「中间 N 字节未送达，完整输出见 `<path>`」）—— 模型的 `grep` 逃生门因此仍然可用。
- (b) 取消头尾混合，只做纯增量 —— 但这会让「测试失败在开头」看不见，**正是 G1b 的诱因**，不建议。

另需明确：`bash_poll` 结果里的「N KiB new」= **实际交付字节数**，不是文件增长量。

### R5 · 增量读会切断半行、切在多字节字符中间（影响 中高 / 可能性 高）

**证据**：后台进程正在**追加写**同一个文件 ⇒ 任一时刻文件尾都可能停在半行；
按字节偏移读（设计 §4.3）可能把一行拆成两次交付，更糟的是把一个 UTF-8 多字节字符拆成两次
⇒ 模型看到乱码。

本仓硬约束（`AGENTS.md`「流式/字节路径注意 UTF-8 边界」）明确要求：**缓冲必须是字节级，只在完整行上解码**。

**修**：poll 的读**只读到最后一个完整行为止**，`consumed` 停在完整行边界，剩余字节留给下一次 poll。
（这与 R4 的「`consumed` 只推进实际交付量」是同一条规则的两面，一起改。）

### R6 · 「进程级注册表 + 会话级内容」在 API 上没有落点（影响 中 / 可能性 中）

**证据**：设计 §4.4 的 `BgEntry` 形态里**没有 `session_id`**；
`snapshot(&self, handle)` / `kill(&self, handle)` 的签名里也**没有会话参数**。
§4.5 说「注册表**内容** = 会话级」，但「六个清理点」是**唯一**的隔离机制。

⇒ 一旦新增清理路径、或某条清理点漏了（`/resume` 走到未覆盖的分支、panic 路径、
`Drop` 只在最后一个 `Arc` 释放时才跑），**上个会话的句柄仍可被 poll** ⇒
模型拿到的是**另一个工作区**的输出路径（条目里存着旧 cwd）。

**修**：`BgEntry` 带 `session_id`；`snapshot`/`poll` 传入当前会话 id，不匹配即报「未知句柄」；
`kill_all` 收成 `kill_all(session_id)`（`/new` 只清自己的）。句柄不复用是**第二道**防线，不是唯一一道。

### R7 · 进程组 kill 与「收尸」的先后没写死 —— 可能误杀无关进程组（影响 中 / 可能性 中）

**证据**：`adapters/tools-basic/src/bash.rs:158-162` 明写告诫：

> 调用方**必须**在 `wait()` 收尸**之前**调用：leader 一旦被收尸，它的 pid 就可能被复用，
> `kill -9 -<pid>` 便可能打到无关的进程组。

设计 §4.3 的 sweeper 同时做两件事：「`child.try_wait()` → 已退出则取退码、收尸」与
「`stat` 输出文件大小 → 超上限 ⇒ 杀进程组」，**没有定序** ⇒ 收尸后再杀 ⇒ 命中上述告诫。

**修**：定序为「**先杀、后收**」：超限时先 `kill_group(pid)`，再 `try_wait()` 收尸；
把 `bash.rs:158` 那段告诫**继承**到新模块的 doc comment（否则两份实现会漂移）。

### R8 · `YUSHAN_BG_DIR` 未登记进 AGENTS.md 的环境变量清单（影响 中 / 可能性 高）

**证据**：`AGENTS.md` 的「环境变量」段列了 `YUSHAN_FETCHED_DIR`、`YUSHAN_SUBAGENTS_DIR` 等，
**没有** `YUSHAN_BG_DIR`。设计 §4.5 引入了这个新变量。

⇒ `AGENTS.md` 是「每一轮都需要」的入口文件，新环境变量不登记 ⇒ 下一个人（含未来的 agent）
不知道它存在 ⇒ 会去改默认目录或问用户。

**修**：AGENTS.md 的环境变量段加一行（保持该文件「只留一行指针」的纪律）。
另：`bg_root` 的启动期清扫照 `sweep_fetched` 的形态（`apps/coding-agent/src/main.rs:2613` 只清 Primary 档），
语义要一并写明。

### R9 · `wait_seconds` 把「阻塞」原样引回来，且期望值为负（影响 中 / 可能性 中高）

**证据**：设计 §4.4 定 `BashPollTool::timeout() = 75s`、`wait_seconds` 上限 60s；
§4.6 又说「默认 0 是为了不让模型把『阻塞』重新引回来」。

算一下期望值：一次模型往返实测均值 **17.3s**（`context.md` 对照表「每次响应平均墙钟」）。
若命令尚未结束，`wait_seconds=60` 要多等 60s 才可能省掉一次 17.3s 的往返 ⇒ **期望为负**。
而 §3.2 的下界场景（模型无独立工作可做）恰恰就是这种情形。

**修**：`wait_seconds` 上限降到 **20s 量级**（小于一次模型往返），或**取消该参数**、
未完成就返回 `running`。另：等待必须用 `tokio::time::sleep`（异步），不得阻塞 runtime 线程。

### R10 · 可选适配器 crate 将成为第一个持有进程级可变状态的模块（影响 中 / 可能性 已发生）

**证据**：`rg "static |OnceLock|LazyLock|Mutex<" adapters/tools-basic/src/` 只命中**各文件测试里的局部 `static COUNTER`** ⇒
今天 `tools-basic` **无任何进程级状态**。设计把 `BackgroundTasks`（进程级注册表 + app 级的清理语义）放进这个**可选适配器** crate。

依赖方向没破（`adapters/` 在 `apps/` 之下），但「适配器 = 可选实现」的语义被稀释：
一个可选的 adapter crate 现在拥有**进程生命周期**的状态，且它的清理时机由 app 决定。

这个取舍的**根因**是设计自己否掉候选 B 的那条：不加 `ys-tool` 的 trait ⇒ 注册表不能放 app
（工具在 `tools-basic`、装配在 app，注册表放 app ⇒ 工具跨 crate 拿不到它）。

**修（二选一）**：

- (a) **显式记账**：在 ADR 的「后果」里写明这条语义变化，并说明为何不选 (b)。
- (b) 把 bg 工具**与注册表一起放 app**（`apps/coding-agent/src/bg_tools.rs` + `background.rs`）⇒
  零跨 crate 问题，代价是「bash 的兄弟工具」被拆出 adapter，`apps/coding-agent` 继续变大。

### R11 · 现有指纹测试不会因插入 bg 工具而变红 ⇒ 父表前缀不变量**失去测试守卫**（影响 中 / 可能性 高）

**证据**：`apps/coding-agent/src/toolset.rs:596-666` 的 `prefix_fingerprint_is_anchored_and_stable`
**自己拼** `parent_specs = base9 + subagent`，断言 `len == 10`、`last == "subagent"`、
`parent_json.starts_with(&json[..json.len()-1])`。它**不调用 `assembly`**。

⇒ 真实父表变成 12 元后，这条测试**照样全绿**，但从此它描述的父表**不再是真实父表**。
设计 §4.8 写「唯一需更新的既有断言 = `docs/design.md` §12 的『父 10 元 / 子 9 元』」——
这个说法不完整：真正缺的是「**父表前缀不变量没有对着真实装配做断言**」。

**修**：加一条 assembly 级断言 —— 真实父表 JSON 以「子表 JSON 去末尾 `]`」开头，且
`subagent` 仍是**末位**（设计把 bg 插在 `subagent` **之前**正是为保住这条，这个选择是对的，
但要有测试守卫）。同步 `docs/design.md` §12（`:537`）与 `docs/design-final/coding-agent-tui.md`
（那里写的是「父工具表 = 基础 n + `SubagentTool` 追加末位」，需改成 基础 n + bg×2 + subagent）。

### R12 · 2 MiB 输出闸门是轮询式 ⇒ 可瞬时超限（影响 低中 / 可能性 中）

**证据**：sweeper 100ms 一跳 ⇒ 一个高速写盘命令在两跳之间可写远超 2 MiB。

**修**：或用 `ulimit -f`（POSIX sh 支持）让内核在写超限时以 `SIGXFSZ` 杀掉进程（**记为失败**，
不是静默截断），或把跳距压到 20ms 并接受小幅超限。前者更符合本仓「超限即失败、不得静默」的立场。

### R13 · `Stdio::from(f)` 同一 fd 给 stdout+stderr 的写法编译不过；合并语义未声明（影响 低中 / 可能性 高）

**证据**：设计 §4.3 写 `.stdout(Stdio::from(f)).stderr(Stdio::from(f))` ——
`File` 被 move 两次，**编译不过**；需 `f.try_clone()`。

更实质的是语义：既有 `bash` **分开收 stdout / stderr 再拼接**（`adapters/tools-basic/src/bash.rs:40-49`），
后台合并成一个文件 ⇒ 模型看到的 stderr 与 stdout **混排**，且无法区分。

**修**：契约里写明「后台输出是 stdout+stderr 的**合并流**、按写入顺序交错」；
实现用 `try_clone()`。另需决定 bg 是否沿用 `bash` 的 `cd <workspace> &&` 前缀提示
（`adapters/tools-basic/src/bash.rs:101` 的 `redundant_root_cd`）—— 沿用则契约要写，不沿用要说清。

### R14 · 度量缺「按工具名分桶」⇒ 无法证伪主假设（影响 低中 / 可能性 中）

设计 §6.1 的三个指标里，「真命令耗时」是**分布**不是**归属** ——
分不清那 58.4s 是经 `bash` 阻塞的还是经 `bash_bg` 的。

**修**：按工具名分桶统计耗时（`bash` / `bash_poll` / 其它），把验收判据写成
「**经 `bash` 执行且耗时 > 5s 的调用数 → 0**」。这才是主假设（「长命令不再阻塞 loop」）的直接判据。

另建议：bg 的生命周期事件（启动 / 退出码 / 被杀 / 超限 / 淘汰）进 debug 日志 ——
现在只有「回合末还有 N 个后台任务在跑」（§7 残留风险 1），排障时看不到单个任务的结局。

### R15 · `ToolContext`「四个挂载点」措辞不准（影响 低 / 可能性 已发生）

**证据**：`crates/ys-tool/src/context.rs:8-26` 的字段是
`boundary` / `cwd` / `workspace_root` / `summarizer` / `subagent` ——
5 个字段，其中**可选挂载点 3 个**（boundary / summarizer / subagent），
`cwd` / `workspace_root` 是 `PathBuf`（**owned**，不是借用）。

设计与 context.md 都写「四个挂载点全是只读借用」。且 §4.5 的论证前提正是
「挂载点全是只读借用 ⇒ 放不进去」—— 措辞不准会削弱这条论证。

**修**：统一改成「**三个可选挂载点**（boundary / summarizer / subagent），另有两个 owned 路径字段」。

### R16 · `BgState::Failed` 是死变体（影响 低 / 可能性 已发生）

设计 §4.4 同时规定「启动失败 ⇒ **同步报错**，不产生『永远就绪』的句柄」（§4.6 强调过）
⇒ 注册表里**永远不会**出现 `Failed`。

**修**：删掉该变体；或赋予它新语义（「启动后立即异常退出」），并与 `Killed`（被 harness 主动杀）区分开。

### R17 · `bash_bg` 的 cwd 契约未写（影响 低 / 可能性 中）

**证据**：既有 `bash` 用**工具自带的 workspace**（`adapters/tools-basic/src/bash.rs:235` 的 `self.workspace`，建表时固定），
**不是** `ctx.cwd`（后者可能是子目录）。

设计 §4.4 的 `start(&self, command, cwd, timeout)` 里的 `cwd` 未定义来源。

**修**：写明 `bash_bg` 与 `bash` **同一条 cwd 契约**（工具 workspace，每次从工作区根起新 shell），
否则同批调用里两条命令可能落在不同目录，模型会误判。

### R18 · 「重复命令数」用 `args_original_bytes` 聚合**算不出来**（影响 中高 / 可能性 高）

**证据**：

- `apps/coding-agent/src/debug_log.rs:1069`：`original_bytes = serde_json::to_vec(args).len()`
  —— 是**整个参数对象的字节数**，**不是命令串的指纹** ⇒ 两条不同但等长的命令会**碰撞**。
- `apps/coding-agent/src/debug_log.rs:1147`：命令串在默认档下被截断成「头 160 + 尾 64」
  ⇒ 两条不同的长命令会被截成**同一串** ⇒ 误报「重复」。
- 默认档就是 `Head`（`apps/coding-agent/src/debug_log.rs:203-211`，未设 `YUSHAN_DEBUG_ARGS` 时）⇒
  **默认配置下就有这个问题**（`Full` 档才有完整命令串，但那是环境变量，不是默认）。

⇒ 「重复命令数」是 G1b 的**唯一直接判据**（design §6.2「1 → 0」），
而 design §6.1 指的落点（`args_original_bytes`）**算不出这个数**。

**修**：切片 0e 新加一个字段 `command_fp` = 命令串的 `fnv1a64` 指纹 ——
**只落指纹、不落正文** ⇒ 不泄密、不增体积。`fnv1a64` 的现成实现在
`apps/coding-agent/src/toolset.rs:546`，但它在 `#[cfg(test)] mod tests` 内
⇒ 生产代码要自己放一份（不引 `sha2`，与既有锚点同一取舍）。

---

## 改进建议

> **状态（2026-10-21）**：本节是评审当时的建议清单。**其中易修复与需讨论两级已全部并入 design / ADR**
> （逐条落点见文末「处置」章），**架构级三条已定稿**（见「处置」章末）⇒ **无未决项**。

### 易修复（低风险、文档级）

1. 统一「核心 crate 零改动」口径（R3）：改为「除 `ys-tool` 加一个只读预算字段、`ys-loop` 填一处外，核心 crate 不动」。
2. 补 D1 的钳位规则为「**严格小于 + 余量**」，并要求超时文案写生效值（R2）。
3. 改 `ToolContext` 挂载点措辞（R15）。
4. 删 `BgState::Failed` 或赋予语义（R16）。
5. 写明 `bash_bg` 的 cwd 契约与 stderr/stdout 合并语义（R13、R17）。
6. 登记 `YUSHAN_BG_DIR` 进 AGENTS.md 环境变量清单（R8）。
7. 同步 `docs/design.md` §12 与 `docs/design-final/coding-agent-tui.md` 的父表元素数（R11）。

### 需讨论（需要决策）

1. **R1/R2 的 deadline 归属**：建议采纳「工具内始终有 deadline（= 外层预算 − 余量）」，
   并把「杀进程组」从「超时分支」提升为「工具自己负责」。
   这会把验收用例从 1 条扩到 2 条（传 / 不传 `timeout` 两条路径各一条）。
2. **R4 的交付语义**：选 (a)（`consumed` 只推进实际交付量 + 显式标注未送达区间），
   与 R5（只读到完整行）合并成一条规则写进 §4.3。
3. **R6 的会话归属**：`BgEntry` 加 `session_id` + `kill_all(session_id)`（防御纵深，不依赖清理点完备）。
4. **R9 的 `wait_seconds`**：上限降到 20s 或取消。
5. **R10 的归属**：显式记账（adapter 承载进程级状态）还是把 bg 工具整体搬进 app。
6. **R14 的度量**：加「按工具名分桶的耗时」+ bg 生命周期事件进 debug 日志。

### 架构级（影响面大）

1. **R3 的路线选择**：若最终确认 D1 必须改 `ys-tool` + `ys-loop`，则
   「候选 C 核心 crate 零改动」这条**设计卖点**不再成立 ⇒ 需回改 ADR 的方案对比表
   （B 的 blast radius 论证会随之变化：「4 个 crate」变成「2 个 crate + 一个 trait 字段」，
   **B 与 C 的差距被压缩**，值得重新看一眼 B 是否更优）。
2. **R7 的进程组生命周期**：把「先杀后收」「pid 复用窗口」这两条从 `bash.rs` 的注释**提升为共享内核的契约**
   （`kill_group` 的 doc comment），两个调用点共用同一份实现与同一份告诫。
3. **「工具未完成」状态**：设计用「句柄」代替「未完成状态」，**绕开了轮次模型改造** —— 这是本设计最大的价值，
   建议在 ADR 里显式记一句「用句柄语义替代未完成状态」，避免以后有人「好心」去加状态而破坏轮次交替。

---

## 总体评价

**设计质量高于本仓既有多数方案**：三个候选的对比维度是可判定的（逐字节不变 / 指纹锚点 / 父表前缀 / crate 改动数），
校正 1–3 有代码依据，残余风险与「不做的方向」都给了理由，**没有夸大 4.9× 的归因**。
「不改 `bash`」「不改轮次模型」「不改工具表指纹」三条不变量是本方案真正的护栏。

**但它还没到「可以直接交给 exec-plan」的程度**，卡在两处语义没定死：

1. **R1 + R2**：工具的 deadline 归属与钳位规则 —— 这两条决定「外层 300s 砍掉工具」这条路径上
   到底会不会留孤儿进程。**现状是：会。** 而这恰恰是本设计想顺手修掉的既存缺陷。
2. **R3**：「核心 crate 零改动」是 ADR 与 design 里反复出现的卖点，
   而 D1 的修法要求改 `ys-tool` 与 `ys-loop` —— 这个矛盾若不先解，
   落地时会有人「顺手」绕过它，结果是**既没修好 D1，又破了不变量**。

**推荐下一步**：先把 R1/R2/R3 三条落成 design.md 的具体条款（补进 §4.4/§5），
其余 P1 级条目可在切片 0a/0b 落地时一并修；**不建议**在 R3 未决前就进入 exec-plan。

---

## 处置（2026-10-20）

本评审的结论已并入 [`design.md`](./design.md) 与
[`adr-background-command-execution.md`](./adr-background-command-execution.md)。

| # | 处置 | 落点 |
|---|---|---|
| **R1** | **已改**：deadline 的归属改为「工具内**始终**有」；D1/D2 加了四条组合的覆盖矩阵；验收从 1 条扩到 2 条 | design §3.5 修正 4、§5 D1/D2、§4.9 切片 0a/0b |
| **R2** | **已改**：钳位写成「**严格小于 + 余量**」（`min(模型值, 外层 − Δ)`，Δ 取 5s 量级），超时文案写**生效值** | design §3.5 修正 5、§5 D1 |
| **R3** | **已改**：「核心 crate 零改动」口径改为「**2 个 crate 各 1 处**」；B 的对比重新记账为 4 → 2；改动表补 `ys-tool` / `ys-loop` 两行 | design §3.5 修正 6、§4.2、§4.8、§3.4；ADR「评审修订」 |
| **R4** | **已改**：交付语义定死三条（不重复送达 / 未送达必须明示 / 不静默丢弃）；`skipped_bytes` 进契约 | design §4.3 ③、§4.6 |
| **R5** | **已改**：读**只到最后一个完整行**为止；`consumed` 停在完整行边界 | design §4.3 ③、§4.7 |
| **R6** | **已改**：条目带 `session` 字段、`poll` 核对、`kill_all(session)` 按会话过滤 | design §4.4、§4.5 |
| **R7** | **已改**：定序「**先杀后收**」，并把 `bash.rs:158` 的 pid 复用告诫继承到新模块；输出闸门加 `ulimit -f` 内核侧兜底 | design §4.3 ② |
| **R8** | **已落为切片 0f**：`AGENTS.md` 环境变量段登记 `YUSHAN_BG_DIR` | design §4.9 切片 0f；ADR §7 |
| **R9** | **已改**：`wait_seconds` 上限 60 → **20**（小于一次模型往返 17.3s 量级的上界），`timeout()` 75s → 35s；等待用 `tokio::time::sleep` | design §4.4、§4.6、§7 风险 3 |
| **R10** | **留待切片 1 决策**（显式记账 vs 搬进 app） | design §9 后续 5 |
| **R11** | **已落为改动项**：加 assembly 级断言（真实父表 JSON 前缀 + `subagent` 末位）；同步 `docs/design.md` §12 与 `docs/design-final/coding-agent-tui.md` | design §4.2、§4.8 |
| **R12** | **已改**：`ulimit -f` 内核侧兜底 + sweeper 双保险 | design §4.3 ②、§4.7 |
| **R13** | **已改**：`try_clone()` 写法 + stdout/stderr **合并流**语义写进契约 | design §4.3 ①、§4.6、§7 风险 6 |
| **R14** | **已改**：度量加「**按工具名分桶的耗时**」；判据加「经 `bash` 执行且 > 5s 的调用数 → 0」 | design §6.1、§6.2；ADR §6 |
| **R15** | **已改**：「四个挂载点」→「**第四个可选挂载点**」，并注明编号按可选挂载点计（现有 3 个） | design §4.4 |
| **R16** | **已改**：`BgState::Failed` 删除（启动失败不产生条目） | design §4.4 |
| **R17** | **已改**：`bash_bg` 的 cwd 契约写明用**工具自带的 workspace**（与 `bash` 同条） | design §4.3 ①、§4.6 |
| **R18** | **已改**：「重复命令数」的落点从 `args_original_bytes` 改成**新增 `command_fp` 字段**（命令串的 `fnv1a64` 指纹，不落正文）；并补齐 `tool_result.duration_ms` / `round_end.tools_ms` / `turn_end.elapsed_ms` 的精确指针 | design §6.1 |

**三条架构级决策 —— 已定稿（2026-10-21）**：

1. **R10 · `tools-basic` 承载进程级状态 ⇒ 决定：保持现状 + 显式记账。**
   注册表留在 `adapters/tools-basic/src/background.rs`（app 持 `Arc`）。
   **理由**：注册表若放 app，工具（`tools-basic`）跨 crate 拿不到它，除非在 `ys-tool` 加 trait ——
   那正是候选 B 被否的根因（blast radius 4 个 crate + 450–550 行）。
   **代价记账**：一个**可选适配器 crate** 现在拥有进程级状态与 app 级的生命周期语义 ——
   这是本仓首例，已写进 ADR 的「后果」。
2. **进程组生命周期契约化 ⇒ 决定：共享内核独立成模块。**
   新增 `adapters/tools-basic/src/proc_group.rs`（同步 `kill_group(pid)`），
   「**必须先杀后收**」「pid 复用窗口」两条告诫从 `bash.rs:158-162` 搬进该模块的 doc comment，
   `bash.rs` 与后台模块**共用同一份内核与同一条告诫** ⇒ 两份实现不会漂移。
3. **「用句柄语义替代『工具未完成』状态」⇒ 决定：在 ADR 里显式记一句。**
   理由已记：轮次模型是**严格交替**的（`crates/ys-loop/src/basic.rs` Step 9），
   加「工具未完成」状态要改轮次模型 ⇒ 句柄是**代价低一个数量级**的等价表达。
   **写进 ADR 是为了**：免得以后有人「好心」去给轮次模型加状态而破坏轮次交替。
