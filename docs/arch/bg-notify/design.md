# bg-notify — 后台命令完成通知（架构设计）

> 产出：`/arch-design`。决策记录 [`adr-bg-notify.md`](./adr-bg-notify.md)。
> 探索与实测数据：[`context.md`](./context.md)。
>
> **本文只做设计，不含业务代码。**

## 0. 一句话

**删掉 `bash_poll`。** 后台命令结束时，由既有的 100ms sweeper 把结果 `push` 成
`Boundary::Deferred`；`BasicLoop` 在下一个轮边界自动 append 进会话并 emit
`DeferredResult` —— 模型再也不用问「好了吗」。

## 1. 概述

需求来自实测，不是猜测。会话 `1792727582_325572909`：

| 指标 | 数值 |
|---|---|
| `bash_bg` 启动 | 16 |
| `bash_poll` 调用 | **68** |
| 返回 `running`（零新信息） | **54（79%）** |
| `wait_seconds = 20` | 60 / 68 |
| 模型声明等待累计 | **1243 s** |
| 单句柄最多被 poll | **24 次** |

根因不是模型笨，是**工具契约逼它轮询**：`MAX_WAIT_SECONDS = 20`（`bg_tools.rs:57`）
小于典型任务时长 ⇒ 任何超过 20s 的命令注定 poll 多次。那个上限意图是「别等超过一次
模型往返」，实际效果是**在制造往返**。

## 2. 设计方案

### 2.1 模块划分

```
crates/ys-protocol            ← 零改动（Boundary::Deferred / QueueBoundarySource::push 已够用）
crates/ys-tool                ← 零改动（不加 ToolContext 挂载点，见 §2.3）
crates/ys-loop                ← 零改动（drain_boundary 已在轮边界消费 Deferred，basic.rs:74-112）
crates/ys-event               ← 零改动
crates/ys-tui-coding          ← 零改动（app.rs:380 已渲染 `↓ {handle} finished: …`）

adapters/tools-basic
  background.rs   ← 承载全部新增机制：落点格 + notified 位 + take_completions +
                    adopt（超时过继）。删除 consumed 增量偏移机器
  bg_output.rs    ← 新增 render_completion（唯一渲染点）；POLL_TOOL 前缀改中性
  bg_tools.rs     ← 删 BashPollTool 全族；BashBgTool 文案改写

apps/coding-agent
  app_loop.rs     ← 每回合 bind 守卫 + 回合开头补投 + sweeper tick 调用
  toolset.rs / assembly.rs ← 不再构造 BashPollTool
  prompt.rs       ← 两条 bg guideline 改写（去掉「collect with bash_poll」）
```

| 模块 | 职责 | 核心抽象 |
|---|---|---|
| `background.rs` | 注册表 + 落点 | `BackgroundTasks::bind_deferred` / `take_completions` |
| `bg_output.rs` | 正文渲染 | `render_completion` |
| `app_loop.rs` | 每回合绑定 | `DeferredBinding`（RAII 守卫） |

### 2.2 「工具表进程级 vs 边界源每回合」——本设计的核心难点

`BashBgTool` 在装配期构造一次（`assembly.rs:156`），而真实边界源每回合新建
（`app_loop.rs:1133`）。

若把启动那刻的 `Arc` 存进条目：回合 5 启动的 `cargo test` 在回合 7 才结束，回合 5 的
边界源早已销毁 ⇒ `push` 成功、**结果永久沉底**。这比「没收到」更坏——它看起来成功了。

**解法**：落点存在 `BackgroundTasks` 上（进程级），每回合由 `app_loop` 绑一次：

```
进程级（装配期一次）                        每回合（app_loop）
┌──────────────────────────────┐
BashBgTool ─┐                   │  bind(boundary.clone())
BashTool   ─┼→ Arc<BackgroundTasks> ←────────────────────────  app_loop: 起回合绑定
            │   ├ entries（条目 = 完成项队列）
            │   ├ deferred: Mutex<Option<Arc<QueueBoundarySource>>>
            │   └ adopters: Vec<(pgid, session, killed)>
            └── 生产者只持 Arc<BackgroundTasks>，不需要每回合的东西
```

**为什么不给 `ToolContext` 加第 5 个挂载点**：它已有 4 个可选挂载点
（`context.rs:4-32`）。加第 5 个能跑通，但**只在同回合内完成的任务上成立**；跨回合时
工具手上只剩一份死队列。而 `bash_bg` 的存在理由恰恰是跨回合 ⇒ 加了无用。

### 2.3 关键数据流

```
模型 → bash_bg("cargo build") → [条目 Running, notified=false] → 立即返回句柄 bg7
模型 → （继续干别的，不问）
          ↓ 100ms sweeper
       进程退出 → BgState::Exited(0)
       sweep() 末尾：notified==false 且已终态 ⇒ push(Boundary::Deferred{bg7, 正文})
                                            ⇒ mark_notified（**在 push 之后**）
          ↓ 下一个轮边界
       drain_boundary → session.append(正文) + emit DeferredResult{bg7, 正文}
          ↓
       UI: "↓ bg7 finished: <首行>"
          ↓
       模型下一轮看到完整结果
```

### 2.4 接口概要

```rust
impl BackgroundTasks {
    /// 绑定本回合投递落点。守卫 drop 时解绑（**RAII**：app_loop 回合体内多处 `?`
    /// 提前返回，手动 unbind 必漏）。
    pub fn bind_deferred(&self, sink: Arc<QueueBoundarySource>) -> DeferredBinding;

    /// 取出「已终态且未通知」的条目。无绑定时返回空（条目留在表里等下一回合）。
    pub fn take_completions(&self, session: &str) -> Vec<BgCompletion>;

    /// 超时过继：登记 (pgid, session)，由同一套 sweeper 检测退出。
    pub fn adopt(&self, child: Child, session: &str, meta: AdoptMeta) -> BgHandle;
}

pub struct BgCompletion {
    pub handle: BgHandle,
    pub message: Message,   // 正文已渲染
}
```

### 2.5 `bash` 超时 → 转后台

当前 `bash.rs:352` 算 `deadline`，到点 `kill_process_group`（`:304`），返回
`ToolError::Timeout`（`:501`）。改为：**过继**给 `BackgroundTasks` —— 登记
`(pgid, session)`，**保留既有管道排空路径**（`bash.rs:441-449` 的不变量：
阶段 A 绝不 `try_wait`），到点不杀。

外层 `tool_budget` 到点仍 drop future（`:18-32` 既有语义），但进程组已登记在收养表里
⇒ 不会漏杀。

**未挂载后台能力时行为逐字节不变**（仍 `Err` + 杀进程组）。

### 2.6 通知正文

```
[bg7] cargo build — finished (exit 0) in 27.4s
<尾部摘要，≤ 50 KiB，沿用既有 head_tail 策略>
[full output: /…/yushan-bg/<session>/bg7.log]
```

- **必须给完整路径**：模型的「不静默丢弃」纪律（`research.md` M1）——要别的片段用
  `read` / `grep`。
- **失败与被杀措辞必须分开**：`finished (exit 2)` vs `killed after 900s (lifetime exceeded)`。
  否则模型会把「被 harness 杀掉」误报成「构建失败」。
- 角色用 `User`（沿 ADR-0017 决策 12 的系统注入先例），**来源靠事件类型区分**。
- 跨回合送达的条目必须打**时间戳式事实**（`finished 342s ago`），否则模型会把它当刚发生。

## 3. 候选方案对比

### 3.1 方案 A — 最小复杂度（落点挂在 `BackgroundTasks`）

注册表自己当队列：一格 `Mutex<Option<Arc<QueueBoundarySource>>>` + 一个 `notified: bool`
同时表达「还没投」与「已投过」。零新增 trait / crate / 协议类型。

- **代码量**：≈ +420 / −715（净减约 300 行），测试另计 ≈ +380 / −450。
- **优点**：`ys-protocol` / `ys-loop` / `ys-tool` / `ys-tui-coding` 全部零改动；
  与 sweeper 天然合流；「滞留」是条目的副产物而非新增策略。
- **缺点（R1，最痛）**：机制不外溢。第二个后台任务类型（MCP 长任务、后台 LSP）要再写
  一遍「检测完成 → 渲染 → 找落点 → 投递」那 30 行，且各自落点格不同步。

### 3.2 方案 B — 可扩展优先（`ys-protocol` 通用投递平面）

`DeferredSink` / `DeferredOutlet` / `Parking` / `BindGuard` 四个小类型，约 200 行，
放 `ys-protocol`（零 tokio）。边界划在**投递**，不划在执行。

- **代码量**：≈ +625 / −555（净 +70）。
- **优点**：新任务类型零接线；顺带消除一处既有类型泄漏（`Arc<QueueBoundarySource>`
  出现在 `SubagentRunner` 构造链上）。
- **缺点**：只有一个生产用例（bash）时是过度设计。**B 自己也把它标为「D3 建议砍」**。
  另有一个进程级对象与 `SessionSlot` 的一致性风险。

### 3.3 方案 C — 性能 / 资源优先

未成稿（子代理被取消）。其关注点（**恰好一次注入**的竞争分析、资源账、清扫路径）
不构成第三种架构方向，而是 A 实现时必须解决的正确性问题 —— **已并入 §2.4 / §4 与本设计的
测试矩阵**。

### 3.4 对比矩阵

| 维度 | A 落点挂注册表 | B 通用投递平面 |
|---|---|---|
| 实现复杂度 | **低**（约 30 行核心） | 中（~200 行 + 一致性约束） |
| 新增概念 | 0（无 trait / 类型） | 4 个小类型 |
| 改动 crate 数 | 1（tools-basic）+ app 侧接线 | 2（+ ys-protocol） |
| 性能 | 复用 sweeper，零新增 tokio 任务 | 同 A（outlet 只是多一层间接） |
| 演进灵活性 | **低**（第二个任务类型要重写 30 行） | **高**（零接线） |
| 运维负担 | 低 | 中（多一个进程级对象需与 SessionSlot 同步） |
| 风险 | R1 技术债（需显式记录触发条件） | 过度设计（YAGNI） |

## 4. 选择理由

**采纳 A。**

- 本仓立场是「最小核心 / 静态组合优先 / 不做用不上的抽象」。**只有一个生产用例时**，
  30 行 vs 200 行的差距是实打实的。
- B 的收益（多类型复用）**当前为零**，代价（多一个需与 `SessionSlot` 同步的进程级对象）
  **当前就在**。
- 关键的诚实交代：**A 欠下 R1 这笔债**。必须在 `docs/TODO.md` 显式记一条 ——
  「第三个后台任务类型出现时，把『落点格 + take/mark 两步』抽成 `ys-protocol` 的
  `DeferredSink`（即 B §2.2）」，并写明触发条件。否则这 30 行会被复制第三遍。

**代价（选择 A 意味着放弃什么）**：放弃「一次把投递平面做对」的机会。将来抽取时，
改动面会横跨两个已有生产者的构造链 —— 比现在一次做完更贵。**这是有意接受的**。

## 5. 生命周期

| 事件 | 行为 |
|---|---|
| 命令正常结束 | sweeper 通知（**恰好一次**） |
| 命令失败 / 被杀 | 同上，正文标明，`is_error` 正确 |
| 回合先结束、命令还在跑 | 结果**滞留**在注册表，下一回合开头补投 |
| abort（`Ctrl-C` / Esc） | 按既有 `background.rs:630-700` 杀进程组，**且不投递** |
| `/new` | 同上，并清空滞留的完成项（session 过滤） |
| 进程退出 | 同既有清扫 |

**滞留是相对 subagent 先例的有意差异**：subagent 回合末不等（价值是「父回合能继续」）；
后台命令的用户期望是**必须拿到结果**。

⚠️ **这条是本次设计对 `context.md` Q1 的修正**。Q1 原本答「回合结束则不送达」，
但删掉 `bash_poll` 后若不滞留，结果就是**静默丢失**（详见候选 B 的 D1）——
那比现状更糟。故改为滞留 + 下回合补投。

## 6. 已知风险与待验证项

- [ ] **R1 技术债**：机制不外溢。→ 记入 `docs/TODO.md` 并写明触发条件
- [ ] **R2 两种所有权**：`BgEntry`（有 `Child`、参与 `reap`）与 adopter（只有 pgid、
      自己 `wait`）并存，`sweep` 语义从此有两种
- [ ] **R3 渲染失败**（输出文件已被删）⇒ 该条结果丢弃。缓解：退回路径指针 + 日志
- [ ] **R4 行为契约变更**：`bash` 超时不再是 `Err`。`bash.rs:592` / `:623` 等既有用例
      必须重写（须在 ADR 签字）
- [ ] **R5 跨回合送达最迟等一条用户输入**：若用户不再发消息，结果留在表里（受
      `max_entries` 淘汰）
- [ ] **R6 绑定/解绑与 sweeper 的窄竞态**（窗口 ≤ 100ms + 一次 push）。三道缓解：
      `mark_notified` 在 `push` 之后、补投复用同一个 `take_completions`、
      收尾顺序先 `drop(binding)` 再排空事件信道。正向风险是「重复一条」而非「丢一条」
- [ ] **R7** `prompt.rs` guideline 必须整体重写，否则模型会去调已不存在的工具
- [ ] **R8** 子 agent 拿不到后台能力（子表本来就没有 `bash_bg`，`assembly.rs:131`）—— 明记为限制

## 7. 测试矩阵

| 断言 | 变异（注入后应变红） |
|---|---|
| 完成时**恰好一条** `Deferred`；连续 5 次 tick 不重发 | 去掉 `notified` ⇒ 100ms 刷出 5 条 |
| 未绑定时完成 ⇒ 留在注册表 ⇒ 下回合补投且**只补一次** | `mark_notified` 挪到 `push` 之前 ⇒ 结果消失；不调 ⇒ 每 tick 重复注入 |
| 落点守卫在回合的**所有** `?` 返回路径上解绑 | 改手动 unbind 并删任一调用点 ⇒ 迟到结果投进死队列 |
| abort 杀掉的后台条目**不**投递 | `kill_entry` 不置 `notified` ⇒ 取消后仍收到「killed」 |
| `bash` 超时返回 `is_error: false` 且随后收到含完整输出的 `Deferred` | 恢复成 `terminate` ⇒ 变红 |
| `bash` 超时转后台后 abort ⇒ 进程组真死且不投递 | 去掉 `adopt` ⇒ 进程成孤儿 |
| 未挂载后台能力时 `bash` 超时**逐字节不变** | 无条件走 `hand_off` ⇒ `bash.rs:592` 变红 |
| `/new` 之后新会话收不到旧会话的完成项 | `take_completions` 去掉 session 过滤 ⇒ 变红 |
| 工具表从 12 元降到 11 元 | 指纹锚点断言 |

## 8. 相关文档

- [`context.md`](./context.md) — 现状探索与实测数据
- [`adr-bg-notify.md`](./adr-bg-notify.md) — 决策记录
- [ADR-0017](../../adr/0017-compaction-hardening.md) — 决策 12：系统注入走 `User` 角色
- [ADR-0049](../../adr/0049-final-answer-boundary-drain.md) — `Boundary` 消费语义
- `crates/ys-protocol/src/boundary.rs:23` — `Boundary::Deferred` 的既有定义
- `apps/coding-agent/src/subagent.rs` — 异步 subagent「不需要 poll」的先例
- `docs/arch/execution-efficiency/design.md` §4.6 / §4.7 — 需同步更新
