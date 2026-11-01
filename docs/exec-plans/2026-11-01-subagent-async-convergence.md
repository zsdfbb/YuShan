# 执行计划：Subagent 异步收敛（四块）

> 对应设计：`docs/design-plans/2026-11-01-subagent-async-convergence.md`
> 日期：2026-11-01
> 纪律：**每个 `impl` 与它的 `test` 成对**；同波文件集互不相交；单单元 ≤5 文件 / ≤10 分钟。
> 构建与测试入口：`./scripts/cargo-build.sh` / `./scripts/par-test.sh`（**不用**裸 `cargo`）。

## 波次总览

波次按**依赖深度**编号；同一波内文件集**互不相交**。「串行对」= 一个 `impl` 与其 `test` 改同一批文件，
不能并行，但必须**分两个任务**分别 review（`test` 任务要检查断言是否真会变红）。

| 波次 | 串行对 | 文件集 |
|---|---|---|
| **W0a** | [U1 → T1] | `crates/ys-tool/src/subagent.rs`、`crates/ys-tool/src/context.rs`、`crates/ys-loop/src/basic.rs`（仅 2 处 `impl SubagentRunner` 的 `run`、以及 `ProbeRunner` / `FakeSubagentRunner` 的 trait 实现体） |
| **W0b** | [U3 → T3] | `crates/ys-loop/src/basic.rs`（等待链）、`crates/ys-component/src/limits.rs`、`crates/ys-runtime/src/agent.rs`、`apps/coding-agent/src/{app_loop,main,config}.rs`（**仅删等待残留**） |
| **W1** | [U2 → T2] | `apps/coding-agent/src/subagent.rs`、`assembly.rs`、`toolset.rs`、`prompt.rs` |
| **W2** | [U4 → T4] | `apps/coding-agent/src/subagent.rs` |
| **W3** | [U5 → T5] | `apps/coding-agent/src/subagent.rs`、`assembly.rs`、`toolset.rs`、`crates/ys-tool/src/subagent.rs`、`crates/ys-tool/src/context.rs`、`crates/ys-loop/src/basic.rs`（仅追加测试） |
| **W4** | [U6 → T6] | `apps/coding-agent/src/app_loop.rs`、`main.rs`、`config.rs` |
| **W5** | [U7 → T7] | `apps/coding-agent/src/prompt.rs`、`subagent.rs` |
| **W6** | [D1a]、[D1b] | 6 个 `docs/` 文件（每个子单元 3 个） |

**W0a 与 W0b 必须串行（U1 先、U3 后），不得并行**。原因：`SubagentRunner` 全仓有 **5 处实现**，
其中 `crates/ys-loop/src/basic.rs` 独占 **2 处** —— `:2580` 的 `ProbeRunner`、`:5233` 的
`FakeSubagentRunner`。U1 删掉 trait 的 `run` 后，这 5 处实现全部编译失败，而 `basic.rs`
正是 U3 的文件。⇒ 两者文件集**相交于 `basic.rs`**，同一波并行会让两个 subagent 互相把对方的
编译搞坏。

**执行方式**：先派 U1 → T1（含删 `basic.rs` 两处 `run`，使 workspace 重新可编译），
确认 `./scripts/cargo-build.sh check --all-targets` 通过后，再派 U3 → T3。

**三条硬约束（本计划避免并行冲突的核心）**：
1. `crates/ys-loop/src/basic.rs` 被 **U1（仅 2 处 `impl` 的 `run`）** 与 **U3（等待链）** 先后改，
   但二者是**串行**（W0a → W0b），不并行；T5 只在末尾**追加**一个测试函数（W3，U3 早已完成）。
2. `apps/coding-agent/src/app_loop.rs` 被 **U3（仅删 `subagent_wait` 残留）** 与 **U6（`Wake` 臂 + 测试）**
   先后改，但二者相隔多个波次、**不并行**。U6 之外不得改该文件的执行逻辑。为此：
   - U2 不动 `app_loop.rs` 里的 `:4393` 同步工具测试装配（并入 U6）；
   - U4 的回合号做成 `Arc<AtomicU32>`（与 `results` / `live` 同一条共享路径），由 U6 在
     `run_turn_once` 开头写一次 ⇒ U4 不碰 `app_loop.rs` / `main.rs`。

## 执行顺序（拓扑序，用于并行派发）

```
W0a: [U1 → T1]        # 先做：删 trait 的 run，含 basic.rs 两处 impl
W0b: [U3 → T3]        # 依赖 U1；crates + app 两侧一次删完
W1:  [U2 → T2]        # 依赖 U1
W2:  [U4 → T4]        # 依赖 U1、U2
W3:  [U5 → T5]        # 依赖 U4
W4:  [U6 → T6]        # 依赖 U2、U3、U5
W5:  [U7 → T7]        # 依赖 U6
W6:  [D1a]、[D1b]      # 全绿之后
```

**全程纪律（比波次划分更重要）**：每个 `impl` 单元结束时的验收条件是
`./scripts/cargo-build.sh check --all-targets` **全量绿**，不允许「本包能编就行」这种降级。
若某个单元确实无法做到（需跨单元配合），**就必须把那些单元合并成一个单元** —— 而不是降低验收门槛。

---

## W0

### U1 —— 删除 `SubagentRunner::run`，trait 只剩 `spawn`

- **type**：`impl`
- **依赖**：—
- **涉及文件**：
  - `crates/ys-tool/src/subagent.rs`（删 `run` 方法声明 `:61-68`；删 `running()` / `settled()` 默认方法；改 `spawn` 的「默认实现 = 不支持」注释，因为不再有「只挂同步派发的实现方」；删 `mod tests` 里 `FakeRunner::run` `:137`）
  - `crates/ys-tool/src/context.rs`（删 `FakeRunner::run` `:97`）
  - `crates/ys-loop/src/basic.rs`（**仅删这 3 处，不碰本文件的其他任何内容**：`:2580-2581` 的 `ProbeRunner::run`、`:5233-5234` 的 `FakeSubagentRunner::run`；以及该文件内对这 5 处 `impl SubagentRunner` 所实现的旧方法集的任何引用）
- **要点**：
  - `SubagentOutcome` 保留（`spawn` 的后台任务仍产出它）。
  - `spawn` 的默认实现**保持** `Err("不支持")`（不改成必需方法）—— 让「未挂派发能力」的
    实现方零改动，与既有纪律一致。
  - 文档注释里「只挂同步派发的实现方不必改」这类以 `run` 为前提的措辞要改。
- **review**：`deep`（公共 trait 契约）

### T1 —— 锚定瘦身后的 trait 形状

- **type**：`test`
- **依赖**：U1
- **涉及文件**：
  - `crates/ys-tool/src/subagent.rs`（`mod tests`：断言 `FakeRunner` 只需实现 `spawn` 即可被 `&dyn SubagentRunner` 使用，即 object-safe 仍成立）
  - `crates/ys-tool/src/context.rs`（`mod tests`：`with_subagent` 挂载后 `spawn` 可达）
- **验收方法**：`./scripts/par-test.sh -p ys-tool` 全绿；变异 —— 把 `spawn` 改成非默认
  必需方法 ⇒ 上述两处 `FakeRunner` 编译失败（**这是编译期断言**，在任务说明里写清
  「本单元的红 = 编译不过」）。
- **review**：`shallow`

### U3 —— 拆除 ADR-0055 等待链（crates + app 两侧一次删完）

- **type**：`impl`
- **依赖**：U1
- **涉及文件**：
  - `crates/ys-loop/src/basic.rs`（删 `enum SubagentWait` `:167-180`、`wait_for_subagents` `:199-240`、`has_running_subagents` `:242-244`；`BoundaryDrain::Empty` 分支 `:1221-1252` 直接收场；删 `:5288-5420` 三条等待测试与 `FakeSubagentRunner` `:5233`；**保留** `ProbeRunner` `:2580` 的 `spawn`）
  - `crates/ys-component/src/limits.rs`（删 `subagent_wait` `:88`、`subagent_poll` `:95`、默认值 `:230`/`:232`、测试 `:319-346`）
  - `crates/ys-runtime/src/agent.rs`（删 `set_subagent_wait` `:204`、`subagent_wait` `:211`）
  - `apps/coding-agent/src/app_loop.rs`（**仅删等待残留，不碰 `Wake::SubagentReturned` 臂**：`:1130-1134` 的注释、`:1484-1500` 的每回合收紧块、`:4725` 注释、`:4790-4791` 与 `:4932-4933` 的 `limits.subagent_*` 赋值）
  - `apps/coding-agent/src/main.rs`（删 `:1965` 的 `agent.set_subagent_wait(...)`）
  - `apps/coding-agent/src/config.rs`（删 `subagent_wait_secs` `:52`、`:162`、`Config::subagent_wait` `:434-435`、`:679` 的 sanitize 行）
- **要点**：
  - **本单元必须一次改完 crates 与 app 两侧**。`subagent_wait` / `subagent_poll` 两个字段的**全部**使用者
    就是等待链本身 + app 侧赋值点（已逐处核实）；只删 crates 侧会让 workspace 连续多个波次编译不过。
    本单元结束后 `./scripts/cargo-build.sh check --all-targets` **全量**必须绿。
  - `running()` / `settled()` 两个 trait 默认方法的唯一消费者也是等待链 ⇒ 连同
    `crates/ys-tool/src/subagent.rs:113`/`:124` 的声明一起删（这部分与 U1 改同一文件，但已串行）。
  - `AppSubagentRunner::running()`（`subagent.rs:1043`）/ `settled()`（`:1047-1048`）+ `LiveSubagents::settled_notified()`（`:441`）+ 它背后的 `Notify` 字段一并删 —— 删后双网查一遍：
    全仓不得再出现 `settled_notified`、`notify_waiters`、`LiveSubagents::settled` 的残留。
  - `basic.rs:5288-5420` 的删除要让 `:5295`、`:5362` 两条「变异」注释一并消失（它们描述的函数已不存在）。
  - 配置项删除已核实安全（`config.rs` 无 `deny_unknown_fields`）。
- **review**：`deep`（并发/收场路径 + 跨 6 文件）

### T3 —— 父回合不再被在跑子 agent 拖住

- **type**：`test`
- **依赖**：U3
- **涉及文件**：`crates/ys-loop/src/basic.rs`（tests mod）、`crates/ys-component/src/limits.rs`（tests mod）、`crates/ys-runtime/src/agent.rs`（tests mod）
- **验收方法**（合同 4 的 crates 层部分）：
  - 新用例 `final_answer_does_not_wait_for_running_subagents`：`FakeSubagentRunner.running == 1`
    且 `settled()` 永不就绪 ⇒ 断言 `run_turn` 在 **< 200ms** 内返回 `StopReason::Completed`。
    变异：把 `wait_for_subagents` 的调用加回 ⇒ 墙钟断言红（会挂满 `subagent_poll`）。
  - `./scripts/par-test.sh -p ys-loop -p ys-component -p ys-runtime`
- **review**：`deep`

---

## W1

### U2 —— 删除同步 `subagent` 工具 + 父表锚点 + 提示词门控改 `subagent_async`

- **type**：`impl`
- **依赖**：U1（trait 已无 `run`，否则删工具后 `impl SubagentRunner` 仍要实现它）
- **涉及文件**：
  - `apps/coding-agent/src/subagent.rs`（删 `SubagentTool` `:1814-1824` 与 `impl Tool` `:1826` 起至 `SubagentAsyncTool` 前的整段；删 `subagent.rs:2290`/`:2374`/`:2441`/`:2467`/`:2587` 等以 `SubagentTool` 为被测对象的测试；删 `:2272` 的 `FakeRunner::run` 与 `:2342` 的 `SlowRunner::run`（若该 runner 整体只为同步工具存在则整个删））
  - `apps/coding-agent/src/assembly.rs`（删 `:46` import 里的 `SubagentTool`、删 `:201` 的 push；`:198-200` 的「追加顺序是契约」注释按新形态改写）
  - `apps/coding-agent/src/toolset.rs`（删 `:645` 的 `SubagentTool::new(...).spec()` push；`:657` 的 `vec!["bash_bg", "subagent", "subagent_async"]` 暂时改为 `vec!["bash_bg", "subagent_async"]`，元数断言 `12` → `11`；**U4 会把它改回 12 并换成 `subagent_send`**）
  - `apps/coding-agent/src/prompt.rs`（`:77`、`:167` 的 `has_subagent` 判据 → `"subagent_async"`；`:222-227` 的文案由「`subagent` 工具」改为「`subagent_async` 工具」；`:485-545` 整段处理：仍成立的两条并入 `:468-484` 段，其余删；`:468-484` 内「the turn keeps running until the sub-agent reports back」一句改为「its result arrives as a new turn automatically」）
  - `apps/coding-agent/src/app_loop.rs`（**只有一处**：`:4393` 的测试装配里 `SubagentTool::new(...)`) ⇒ **本单元不得碰 `app_loop.rs` 的其他部分**；若判断该测试整体删除更简单，则删该测试函数并在此记录。为维持「`app_loop.rs` 只被 U6 触碰」的约束，**这一处改动并入 U6**，U2 不动 `app_loop.rs`
- **要点**：
  - `prompt.rs:485-545` 的逐条取舍结果**必须写进本单元的 review note**（哪几条保留、并入哪段、为什么）。
  - `toolset.rs` 的中间态（11 元）只存在于 U2 与 U4 之间，且 U2/T2 与 U4/T4 不同波 ⇒ 不会同时编译。
- **review**：`deep`（4 文件 + 对外契约：工具表）

### T2 —— 锚定「无 subagent、有 subagent_async」的父表

- **type**：`test`
- **依赖**：U2
- **涉及文件**：`apps/coding-agent/src/toolset.rs`、`apps/coding-agent/src/assembly.rs`、`apps/coding-agent/src/prompt.rs`、`apps/coding-agent/src/subagent.rs`
- **验收方法**（合同 6 的一半）：
  - `toolset.rs`：`parent_specs.len() == 11`（U4 之后改回 12 并加 `subagent_send`）；
    `assert!(!names.contains(&"subagent"))`；`names.contains(&"subagent_async")`；
    子表仍 `specs.len() == 9`（`:630`）。
  - `prompt.rs`：既有 `:1436` 的 `has_subagent` 变异注要跟着改判据名；
    新增断言 `skill_section(..., true)` 在**父表**（含 `subagent_async`）下产出
    「CAN delegate tasks to subagents via the `subagent_async` tool」。
  - `subagent.rs`：`SubagentAsyncTool::is_concurrency_safe() == true`（`:1941-1943`）。
  - `./scripts/par-test.sh -p ys-coding-agent toolset`。
- **review**：`deep`

---

## W2

### U4 —— 句柄全局唯一 `sa{turn}.{n}`（ADR-0054 决策 ⑤）

- **type**：`impl`
- **依赖**：U1、U2
- **涉及文件**：
  - `apps/coding-agent/src/subagent.rs`（`AppSubagentRunner` 加 `turn: Arc<AtomicU32>` 字段（`:804` 附近）；
    `:1084` 生成改为 `format!("sa{}.{}", self.turn.load(Ordering::Relaxed),
    self.dispatched.load(Ordering::Relaxed))`；`:1082-1083` 注释由「**不需要全局唯一**」改为
    「全局唯一 `sa{turn}.{n}`：`send` 按它寻址」）
- **只这 1 个文件**（1 ≤5 ✓）
- **要点**：
  - **不改 `SubagentSetup::shared` / `new` 的签名**（同上波次的硬约束）：`shared` 的 6 个调用点
    里有 5 个在 `app_loop.rs`，本单元不得碰它。做法同 U5：`turn` 内初始化为
    `Arc::new(AtomicU32::new(0))`，另提供 `pub fn with_turn(mut self, turn: Arc<AtomicU32>) -> Self`。
    `assembly.rs` 不需要改（它不设 `turn`）；`app_loop.rs` 的测试用默认值即可。
  - **`AppSubagentRunner::new` 也同样不能改签名**：全仓 **12 个调用点**（`subagent.rs:985/2534/2642/2716/2818/2848/3046/3507/3827/3880/3927/4148`，
    其中 11 个是该文件自己的测试）。做法：`new()` 体内直接把 `turn` 初始化为
    `Arc::new(AtomicU32::new(0))`，另提供 `pub fn with_turn(mut self, turn: Arc<AtomicU32>) -> Self`，
    由 `make_runner` 转递 `SubagentSetup::turn`。测试用 `with_turn` 或 `setup.with_turn(...)`。
  - **中间态声明（必写入 review note）**：本单元完成后 `turn` 恒为 `0`（生产写入点在
    U6 的 `run_turn_once`）⇒ 实际句柄仍是 `sa0.1`、`sa0.2`…，**跨回合重复尚未真正修好**。
    真正的唯一性在 U6 落地。合同 3 的完整验收归 T6，**不得**在 T4 里假装合同 3 已绿。
  - 制造「两个不同回合」的方式：测试里直接对 `SubagentSetup` 的 `turn` 原子写 `1` / `2`
    后各造一份 runner（`make_runner`）并 `spawn` ⇒ **不需要** `app_loop` 参与。
  - 句柄形如 `sa12.3`；`SubagentRunner::spawn` 的文档与 `SubagentAsyncTool` 的输出文案里若出
    现 `sa1` 这种范例，同步改成 `sa{turn}.{n}` 的形状。
- **review**：`deep`（并发 + 命名契约）

### T4 —— 句柄唯一性断言

- **type**：`test`
- **依赖**：U4
- **涉及文件**：`apps/coding-agent/src/subagent.rs`
- **验收方法**（合同 3）：
  - 两个不同 `turn`（写共享原子为 `1` 与 `2`）各 `spawn` 一次 ⇒ `assert_ne!(h1, h2)`，
    且都匹配正则 `^sa\d+\.\d+$`。变异：把 turn 前缀去掉 ⇒ 红（两个都是 `sa1`）。
  - 同一 runner 连派两次 ⇒ `sa1.1` / `sa1.2` 的序号递增断言。
  - 更新 `subagent.rs:4175`、`:4189` 的 `assert_eq!(handle, "sa1")` 为新形态。
    `:490`/`:501`/`:519`/`:2012`/`:2018`/`:2021`/`:2028`/`:2047` 是 `PendingResults`
    的单元测试，handle 只用作不透明键 ⇒ **不改**（在 review note 里写明理由）。
  - `./scripts/par-test.sh -p ys-coding-agent subagent`
- **review**：`shallow`

---

## W3

### U5 —— `subagent_send` 工具 + 句柄注册表

- **type**：`impl`
- **依赖**：U4（句柄形态定稿）
- **涉及文件**：
  - `apps/coding-agent/src/subagent.rs`（新增 `SubagentHandles`（`Mutex<HashMap<String, Weak<QueueBoundarySource>>>`，`register` / `send` / `live_handles`）；`ChildTask` 携带 `own: Arc<QueueBoundarySource>` 与 `handle`，`execute` 里 `:1339` 用它构造 `ChildBoundary`；`spawn` 在 `tokio::spawn` **之前**注册（`subagent.rs:1087-1099`）；新增 `SubagentSendTool` + `impl Tool`（`is_concurrency_safe() == true`）；`AppSubagentRunner` 实现 `SubagentRunner::send`）
  - `crates/ys-tool/src/subagent.rs`（trait 加 `fn send(&self, handle: &str, message: &str) -> Result<(), String>`，默认实现 `Err("不支持")`）
  - `crates/ys-tool/src/context.rs`（`FakeRunner` 补 `send` 或不补 —— 默认实现存在 ⇒ 不必补；**核对该 fake 是否需要覆盖它以测错误路径**，若需要则在此单元补齐）
  - `apps/coding-agent/src/assembly.rs`（`:202` 之后 push `SubagentSendTool::new(...)`）
  - `apps/coding-agent/src/toolset.rs`（`:646` 之后 push `SubagentSendTool::new().spec()`；`:657` 改为 `vec!["bash_bg", "subagent_async", "subagent_send"]`；元数改回 `12`；doc 注释「12 元」的组成说明同步改）
- **要点**：
  - **`SubagentSetup` 用 `with_handles` 注入，不改 `shared` / `new` 的签名 —— 这是硬约束**。
    `shared` 全仓有 **6 个调用点**：`assembly.rs:161` + `app_loop.rs:4398/4524/4659/4796/4938`（都是测试）。
    后 5 个在 `app_loop.rs`里，而该文件在本单元所属波次**不得被触碰**（归 U6）⇒ 改签名会立刻编译坏。
    做法：`fields` 里 `handles: Arc<SubagentHandles>`，在 `shared` / `new` 体内初始化为
    `Arc::new(SubagentHandles::default())`（与 `model_cache` 同模式），另提供
    `pub fn with_handles(mut self, handles: Arc<SubagentHandles>) -> Self`。`assembly.rs`
    照 `with_explore_prompt` 的写法在 `.with_explore_prompt(...)` 后接 `.with_handles(...)`。
  - **`AppSubagentRunner::new` 同样不改签名**（12 个调用点，见 U4）。`new()` 内初始化为
    `Arc::new(SubagentHandles::default())`，`make_runner` 转递 `self.handles.clone()`。
  - **注入来源很重要**：`handles: Arc<SubagentHandles>` 必须是 `AgentFactory` 上的字段
    （与 `subagent_results` / `subagent_live` 并列，`assembly.rs:66-70`、`:120-121`），
    且 `with_backgrounds`-风格共享 —— 否则 `/resume` 重建后新注册表里的 handle 与工具持有
    的不是同一份（旧子 agent 就发不到了）。在 `AgentFactory::new` 里初始化为 `Arc::new(...)`。
  - `ChildBoundary::new` 的改法：让 `execute` 用**传入的 `own`** 构造（`new_with_own` 或直接内联）。
    **不新增公共 API 的第二种构造**以外的抽象。
  - `send` 的消息 `Role::User`（不是 `Harness`）—— 理由见 design-plan §7 合同 7 的核实结论。
  - `send` 工具的 `handle` 参数描述必须写明「形如 `sa3.2`」——U4 之后句柄不再是 `sa1` 形状。
  - 未知 / 已收尾 handle ⇒ `Err(ToolError::InvalidInput)`，正文列出 `live_handles()`。
  - `send` 在 trait 上而非新 `ToolContext` 字段（避免第 4 个挂载点）。
- **验收**：`./scripts/cargo-build.sh check --all-targets` **全量绿**（不得降级为单包）。
- **review**：`deep`（5 文件 + 并发 + 公共 trait）

### T5 —— send 的通路与 fail-closed

- **type**：`test`
- **依赖**：U5
- **涉及文件**：`apps/coding-agent/src/subagent.rs`、`crates/ys-loop/src/basic.rs`
  （⚠️ `basic.rs` 在 W0 已被 U3/T3 改完，本单元只**追加**一个测试函数；为避免与 U3 并行，
  T5 排在 W3，U3 早已完成）
- **验收方法**（合同 1、2）：
  - 单测：`register("sa1.1", own.clone())` → `send("sa1.1", "收敛")` → `own.take()`
    得到 `Boundary::Steer(msg)`，`msg.role == Role::User`，正文 == `"收敛"`。
    变异：`send` 推给父的 `QueueBoundarySource` ⇒ 红。
  - fail-closed：`send("sa9.9", ...)` ⇒ `is_err()` 且错误含有效 handle 列表；
    drop 掉 `own` 的强引用后 `send` 同一 handle ⇒ 同样 `Err`。变异：改成 `Ok(())` ⇒ 红。
  - 集成（`basic.rs` 的 tests）：仿 `SteerDuringFinalModel`（`:4440-4458`），
    子侧模型第一次 `complete` 时 `send` 自己 ⇒ 断言第二次请求的 `messages` 含该文本。
  - `./scripts/par-test.sh -p ys-coding-agent -p ys-loop`
- **review**：`deep`

---

## W4

### U6 —— `Wake::SubagentReturned` 改为自动起回合 + 清掉 app 侧等待残留

- **type**：`impl`
- **依赖**：U2、U5（工具已在表里、补投链路已明确）
- **涉及文件**：
  - `apps/coding-agent/src/app_loop.rs`（`:410-446` 的 `Wake::SubagentReturned` 臂改为同构于
    `:448-...` 的 `BackgroundReturned`：复核 `s.pending_results().has_pending()` → 空则
    `continue` → `subagent_placeholder()` → `run_turn_chain(...)`，`raw_user_text = None`；
    新增 `fn subagent_placeholder()`（`:1407-1414` 旁，`Role::Harness`，正文自述占位）；
    删 `:4393` 的同步工具测试装配（U2 遗留）；
    `:4717-4860` 的 `late_subagent_result_is_announced_while_idle` 与 `:4868-4960` 的
    `parent_waits_for_slow_subagent_within_the_same_turn` **在本单元删除**，由 T6 重写/替代）
  - `apps/coding-agent/src/main.rs`（`:1958-1961` 注释改为「单回合模式没有下一回合 ⇒ 结果只落表，不自动起回合」；
    `:1973-1980` 的 `make_runner` 若有签名变化则跟随）
- **要点**：
  - **`has_pending` 只读不取**（`subagent.rs:346`）；取走归 `run_turn_once` 的补投
    （`app_loop.rs:1564-1567`）—— 双取会重复送达。
  - 占位消息的正文：`"[harness] 子 agent 结果已送入上下文，请据此继续。"`
  - 配置项删除已核实安全（`config.rs` 无 `deny_unknown_fields`）。
  - `-p` / `--json` **不**加自动回合。
- **review**：`deep`（核心路径 + 3 文件）

### T6 —— 自动回合的两条合同 + 去等待的 app 层证据

- **type**：`test`
- **依赖**：U6
- **涉及文件**：`apps/coding-agent/src/app_loop.rs`
- **验收方法**（合同 4、5、6 的 app 层部分）：
  - **合同 5**：新用例 `late_subagent_result_starts_a_new_turn`（由
    `late_subagent_result_is_announced_while_idle` 改写）：父回合先收场，子 agent 400ms
    后才返回 ⇒ 断言 ① 父模型 `complete` 被调用**两次**（第二回合）；② 第二回合的请求
    含子结果正文；③ 全程无 `Request::Prompt`（测试只经 `spawn_app_loop_full` 起一次输入）。
    变异：把臂改回「只 `send_or_flush` 一行提示」 ⇒ ① 红。
  - **合同 4**：新用例 `parent_turn_ends_while_subagent_runs`：`SubagentSetup` 的子模型睡
    400ms，父模型首次 `complete` 给最终答复 ⇒ 断言父回合墙钟 **< 200ms** 且收场时
    `live.count() == 1`。变异：把等待加回 ⇒ 墙钟红。
  - **合同 6**：`app_loop.rs` 里若有工具表快照断言，确认 12 元且名字集合为
    `bash_bg` / `subagent_async` / `subagent_send`。变异：把 `subagent` 加回 ⇒ 红。
  - `./scripts/par-test.sh -p ys-coding-agent`
- **review**：`deep`

---

## W5

### U7 —— 提示词教模型怎么用（send 的用法 + 自动回合的新事实）

- **type**：`impl`
- **依赖**：U6（行为定稿，提示词才能说真话）
- **涉及文件**：
  - `apps/coding-agent/src/prompt.rs`（`:468-484` 的 `subagent_async` 段加一句
    「results arrive as a new turn automatically」；新增 `if tools.contains(&"subagent_send")`
    的小段：何时用（告诉子 agent 收敛 / 补充约束）、handle 来自何处、**不限制语义但不替代
    重新派发**）
  - `apps/coding-agent/src/subagent.rs`（`SubagentSendTool::spec()` 的 description 定稿；
    `SubagentAsyncTool::call` 的返回正文 `:1955-1963` 里「It is cancelled if this turn
    ends before it finishes」—— **这句已经不准**（现在回合结束不取消），改为「it keeps
    running after this turn ends; you will get its result as a new turn」）
- **要点**：
  - 只写**当前事实**，不写「以前如何」。
  - `prompt.rs` 的段序与「在场才出现」形态沿用既有（`:450` 的守卫注释）。
- **review**：`shallow`

### T7 —— 提示词断言跟随

- **type**：`test`
- **依赖**：U7
- **涉及文件**：`apps/coding-agent/src/prompt.rs`、`apps/coding-agent/src/subagent.rs`
- **验收方法**：
  - 父表（含 `subagent_send`）下提示词含 `subagent_send` 的用法句；子表（9 元）下**不含**
    （子表没有该工具）；`assert!(!text.contains("turn keeps running until"))`（旧承诺已删）。
  - `SubagentAsyncTool::call` 的 `ToolResult` 正文含「new turn」且**不含**「cancelled if
    this turn ends」。
  - `./scripts/par-test.sh -p ys-coding-agent prompt`
- **review**：`shallow`

---

## W6

### D1 —— 文档收尾

- **type**：`impl`（文档）
- **依赖**：U1–U7 全部通过
- **涉及文件**：
  - `docs/adr/0055-subagent-continuation.md`（Status → `superseded by <新编号>`）
  - `docs/adr/0054-subagent-progress-board.md`（Status → 部分落地；决策 ⑤ 已落）
  - `docs/adr/<新编号>-subagent-send-and-auto-turn.md`（**新建**，按 design-plan §5 的要点）
  - `docs/design-final/subagent-continuation.md`（标 `[已被取代]`）
  - `README.md`（`:146` 的工具表描述：`bash_bg` + `subagent` + `subagent_async` → `bash_bg` + `subagent_async` + `subagent_send`）
  - `docs/TODO.md`（删等待链条目，改 ADR-0054 条目）
  - `docs/design.md`（测试矩阵里 subagent 条目按 design-plan §7 更新）
- **review**：`shallow`（⚠️ **本单元不改任何 `.rs`**）
- **单元数**：7 文件 ⇒ **拆分**：D1a（3 个 ADR 文件）、D1b（`design-final` + `TODO.md` + `design.md` + `README.md`），
  各 3~4 文件。

---

## 合同 ↔ 任务映射（自查表）

| 合同 | 覆盖任务 |
|---|---|
| 1 `send` → `Steer` → 子下一轮看到 | T5（单测 + 集成） |
| 2 未知 handle 返回 `Err` | T5 |
| 3 句柄跨回合唯一 | T4 |
| 4 父回合正常收场（不等待） | T3（crates 层）+ T6（app 层） |
| 5 子结果自动起回合 | T6 |
| 6 父表 12 元、无 `subagent`、`subagent_async` 可并发 | T2（无 `subagent`）+ T6（12 元最终态） |
| 7 被 send 后仍能正常收尾 | T5（集成测的第 ② ③ 断言：`stop_reason == Completed`、转录落盘） |
| 8 `/resume` 不串台 | **本轮不做**（design-plan §8）；留 TODO 条目由 D1b 记录 |

## 全局验证（最后一个任务之后）

```bash
./scripts/cargo-build.sh          # workspace 全量
cargo clippy --all-targets
cargo fmt --check
./scripts/par-test.sh             # 全量测试的唯一入口
```

**变异测试（AGENTS.md 要求「关键逻辑要做变异测试」）**：本计划的三条关键变异已内嵌在
T3/T4/T5/T6 的验收方法里（各写明「变异：… ⇒ 红」）。取消链路与滚轮的既有变异脚本
（`scripts/cancellation-mutation.py`、`scripts/wheel-mutation.py`）与本轮无关，不动。
