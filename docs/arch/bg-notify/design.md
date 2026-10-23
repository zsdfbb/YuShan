# bg-notify — 后台命令完成通知（架构设计）

> 产出：`/arch-design`。决策记录 [`adr-bg-notify.md`](./adr-bg-notify.md)。
> 探索与实测数据：[`context.md`](./context.md)。质量分析：[`review.md`](./review.md)。
>
> **rev2**：按 `review.md` 的 P0-1 / P0-2 / P1-1 / P1-2 / P1-3 修订。
> 本轮修订改变了 `sweep()` 的形态与通知状态机 —— 见 §2.4、§2.7、§4。
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
  background.rs   ← 落点格 + 投递状态机 + take_completions + adopt（超时过继）
                    **sweep 拆成锁内收集 / 锁外渲染**（§2.4）
                    删除 consumed 增量偏移机器
  bg_output.rs    ← 新增 render_completion（唯一渲染点）
  bg_tools.rs     ← 删 BashPollTool 全族；BashBgTool 文案改写

apps/coding-agent
  app_loop.rs     ← 每回合 bind 守卫 + 回合开头补投 + sweeper tick 调用
  toolset.rs / assembly.rs ← 不再构造 BashPollTool
  prompt.rs       ← 两条 bg guideline 改写
  main.rs         ← **Print/JSON 模式不挂后台能力**（§2.8）
```

| 模块 | 职责 | 核心抽象 |
|---|---|---|
| `background.rs` | 注册表 + 落点 + 投递状态机 | `BackgroundTasks::bind_deferred` / `take_completions` / `adopt` |
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
            │   ├ entries（在跑项 + 未投出的完成项）
            │   ├ deferred: Mutex<Option<Arc<QueueBoundarySource>>>
            │   └ adopters: Vec<Adopter>
            └── 生产者只持 Arc<BackgroundTasks>，不需要每回合的东西
```

**为什么不给 `ToolContext` 加第 5 个挂载点**：它已有 4 个可选挂载点
（`context.rs:4-32`）。加第 5 个能跑通，但**只在同回合内完成的任务上成立**；跨回合时
工具手上只剩一份死队列。而 `bash_bg` 的存在理由恰恰是跨回合 ⇒ 加了无用。

### 2.3 通知状态机（**rev2 新增**）

review P0-2 指出「`mark` 与 `push` 失序 ⇒ 静默丢失」。修法是给条目加**第三态**，
而不是靠顺序不变量：

```rust
enum Delivery {
    /// 还没投出去。sweeper 每次 tick 都会尝试投递。
    Pending,
    /// 正在投递（已取得候选、正在锁外渲染 / push）。**只有 sweeper 会设它**，
    /// 且必定在本次 tick 内离开（成功 → Delivered，失败 → 回 Pending）。
    InFlight,
    /// 已成功 push 进边界源（或被有意丢弃）。此后不再尝试。
    Delivered,
}
```

**为什么必须三态**：「两态 + 顺序不变量」在「取候选（锁内）→ 渲染（锁外）→ push（锁外）
→ mark（锁内）」这条四步链上没有正确的顺序 —— 无论 mark 早于还是晚于 push，都有窗口：

| 顺序 | 崩溃/切换窗口 | 后果 |
|---|---|---|
| 先 mark 后 push | push 前回合切换 | **静默丢失**（review P0-2） |
| 先 push 后 mark | mark 前进程崩溃 | **重复投递** |

三态把窗口压成：`InFlight` 期间回合切换 ⇒ 条目停在 `InFlight`，**下一次 tick 强制回
`Pending` 重投**（见下「tick 修复」）。代价是**可能投两次**，但绝不丢 ——
方向选择明确：**宁可重复，绝不丢失**（重复会被模型看见「同一条结果出现两次」，
丢失则是无声无息）。

**tick 修复规则**：sweeper 每 tick 先把所有 `InFlight` 且 `inflight_since` 超过
**2 个 tick（≈200ms）**的条目重置为 `Pending`。因为投递是同步短操作（读文件 ≤ 50 KiB +
一次 `push`），200ms 足够走完；超过就说明那次投递被中断了。

### 2.4 `sweep()` 重构（**rev2 新增，P0-1**）

**问题**：`sweep()` 当前持有注册表锁期间同步执行 `sh -c "kill -9 -{pid}"`
（`background.rs:531` 持锁 → `kill_entry` → `proc_group.rs:27` 的 `.status()`）。
同一把锁是 `snapshot()` / `spawn()` / `take_completions()` 的必经之路 ⇒ **锁内
fork/exec/join**，低端设备（Termux/Android）会拖慢所有工具调用。

**rev2 要求 `sweep` 分三段**：

```rust
pub fn sweep(&self) {
    // ── 段 1（持锁）：状态推进 + 收集待办，**不做 IO、不 fork** ──
    let plan = {
        let mut inner = self.lock();
        Self::reap(&mut inner);                    // try_wait 是非阻塞的，允许
        Self::enforce_output_cap_collect(&mut inner);  // 只标记，不 kill
        let candidates = Self::claim_completions(&mut inner); // Pending → InFlight
        let to_kill = Self::take_kill_list(&mut inner);       // 只收集 pid
        let to_render = candidates;                // 后续锁外渲染
        (to_kill, to_render)
    };  // ← 锁在此释放

    // ── 段 2（无锁）：重活 ──
    for pid in plan.0 {
        kill_group(pid);                           // fork/exec/join 在锁外
    }
    for candidate in plan.1 {
        let message = render_completion(&candidate);  // 读文件在锁外
        // 投递或回退
        let delivered = match &self.boundary_sink() {
            Some(sink) => { sink.push(Boundary::Deferred { .. }); true }
            None => false,                          // 无落点 ⇒ 回 Pending 等下回合
        };
        // ── 段 3（持锁）：落状态 ──
        let mut inner = self.lock();
        Self::finish_delivery(&mut inner, &candidate, delivered);
    }
}
```

**关键不变量（写进代码注释）**：
1. **锁内绝不 fork / exec / 读文件**（`try_wait` 除外，它是非阻塞的）。
2. `InFlight` 只在段 1 设、段 3 清；段 2 期间条目对其他线程表现为「投递中」。
3. 段 2 失败（渲染出错）⇒ 段 3 写 `Unreadable` 正文并仍投递（见 §2.7）。

**范围控制**：本次**只挪渲染与 kill**，不异步化 `kill_group`（那会改 `sweep` 的同步签名，
影响面超本次）。`kill_group` 改为非阻塞投递**另记 TODO**。

### 2.5 关键数据流

```
模型 → bash_bg("cargo build") → [条目 Running, Delivery::Pending] → 返回句柄 bg7
模型 → （继续干别的，不问）
          ↓ 100ms sweeper（三段式）
       段1 持锁: reap → 进程已退出 → BgState::Exited(0)；claim: Pending → InFlight
       段2 无锁: render_completion（读 ≤50KiB）→ push(Boundary::Deferred{ bg7, 正文 })
       段3 持锁: InFlight → Delivered
          ↓ 下一个轮边界
       drain_boundary → session.append(正文) + emit DeferredResult{bg7, 正文}
          ↓
       UI: "↓ bg7 finished: <首行>"
          ↓
       模型下一轮看到完整结果
```

### 2.6 接口概要

```rust
impl BackgroundTasks {
    /// 绑定本回合投递落点。守卫 drop 时解绑（RAII：app_loop 多处 `?` 提前返回，
    /// 手动 unbind 必漏）。
    pub fn bind_deferred(&self, sink: Arc<QueueBoundarySource>) -> DeferredBinding;

    /// 取出「已终态且 Pending」的完成项（回合开头补投用）。
    /// 内部把条目置 InFlight；调用方负责渲染 + push 后调 `finish_delivery`。
    pub fn take_completions(&self, session: &str) -> Vec<BgCompletion>;

    /// 投递失败时把 InFlight 退回 Pending（下回合重试）。
    pub fn requeue(&self, handle: BgHandle);

    /// 超时过继：登记 (Child, session)，由同一 sweeper 检测退出。
    pub fn adopt(&self, child: Child, session: &str, meta: AdoptMeta) -> BgHandle;
}

pub struct BgCompletion {
    pub handle: BgHandle,
    pub state: BgState,
    pub elapsed: Duration,
    pub command: String,
    pub output_path: PathBuf,
}
// 注意：`BgCompletion` **不含 Message** —— 渲染在锁外做（§2.4 不变量 1）。
```

### 2.7 正文渲染（含 `Unreadable`，P1-3）

`bg_output::render_completion(&BgCompletion) -> Message`，三种形态：

| 情形 | 正文 | `is_error` |
|---|---|---|
| 正常结束 | `[bg7] cargo build — finished (exit 0) in 27.4s` + 尾部摘要（≤50 KiB）+ `[full output: path]` | false |
| 非零退码 | `finished (exit 2)` + 摘要 + 路径 | **true** |
| 被杀 | `killed after 900s (lifetime exceeded)` 或 `killed: output exceeded 2 MiB` | **true** |
| **文件已不可读**（P1-3） | `[bg7] cargo build — finished (exit 0), output no longer readable (removed by /new or eviction)` | 按退码 |

**为什么必须有 `Unreadable`**：没有它时，渲染失败只能二选一 —— 不投（模型以为没结果）
或投空（模型以为命令无输出）。两者都是**静默的谎**。明确说「有结果但内容没了」，
模型才知道该考虑重跑。

**跨回合送达的时间戳**：正文必须写绝对事实（`finished 342s ago`），否则模型会把
6 分钟前完成的结果当成刚发生。

### 2.8 Print / JSON 模式（**rev2 新增，P1-1**）

**问题**：`backgrounds` 在 `main.rs:2805` **无条件构造**、三种模式共用
（`BACKGROUNDS.set`）。但 `run_print_mode`（`:1868`）与 `run_json_mode`（`:1950`）
**没有 UI 回合循环**，`Deferred` 只在 `BasicLoop::drain_boundary` 的轮边界被消费；
`-p` 是单回合，回合结束即退出 ⇒ 转后台的命令**结果无人接收**。

若 BashTool 超时改转后台 + `is_error: false`，`-p` 用户会看到「命令超时但报告成功，
然后什么都没发生」—— 比现在的「明确超时失败」更差。

**修法**：`-p` / `--json` 模式**不挂后台能力**。

```
main.rs: 构造 backgrounds 的条件从「无条件」改为
          「仅 Mode::Interactive」
```

于是这些模式下：
- `BashBgTool` / `BashPollTool` 不进工具表；
- `BashTool` 拿不到 `backgrounds` ⇒ 超时走**原路径**（杀进程组 + `ToolError::Timeout`），
  行为逐字节不变。

这条也顺带解决 §R8（子 agent 拿不到后台能力）：同一套「没挂就没有」的语义。

### 2.9 `bash` 超时 → 转后台

当前 `bash.rs:352` 算 `deadline`，到点 `kill_process_group`（`:304`），返回
`ToolError::Timeout`（`:501`）。**仅在已挂后台能力时**改为过继：

登记 `(Child, session, meta)`，**保留既有管道排空路径**（`bash.rs:441-449` 的不变量：
阶段 A 绝不 `try_wait`），到点不杀。外层 `tool_budget` 到点仍 drop future
（`:18-32` 既有语义），但进程组已登记在收养表里 ⇒ 不会漏杀。

**未挂载时行为逐字节不变**（§2.8）。

## 3. 候选方案对比

### 3.1 方案 A — 最小复杂度（落点挂在 `BackgroundTasks`）

注册表自己当队列：一格 `Mutex<Option<Arc<QueueBoundarySource>>>` + `Delivery` 三态。

- **代码量**：≈ +480 / −715（净减约 235 行）；rev2 的 sweep 三分段 + 三态 + `Unreadable`
  比 rev1 多约 60 行。
- **优点**：`ys-protocol` / `ys-loop` / `ys-tool` / `ys-tui-coding` 零改动；与 sweeper 天然合流。
- **缺点（R1）**：机制不外溢，第二个后台任务类型要再写一遍那 30 行。

### 3.2 方案 B — 可扩展优先（`ys-protocol` 通用投递平面）

`DeferredSink` / `DeferredOutlet` / `Parking` / `BindGuard` 约 200 行，放 `ys-protocol`。
边界划在**投递**，不划在执行。

- **代码量**：≈ +625 / −555。
- **缺点**：只有一个生产用例时过度设计；多一个需与 `SessionSlot` 同步的进程级对象。
- **注意**：B 的 `Parking` **正好是三态语义**（`Park` / 投递中 / 已投）—— 若将来抽取，
  rev2 的 `Delivery` 可直接演进成它。

### 3.3 方案 C — 性能 / 资源优先

未成稿。其关注点（恰好一次、资源账、清扫路径）已并入 §2.3 / §2.4 / §5 与测试矩阵。

### 3.4 对比矩阵

| 维度 | A（采纳） | B |
|---|---|---|
| 实现复杂度 | **低** | 中（+200 行 + 一致性约束） |
| 新增概念 | `Delivery` 三态 | 4 个小类型 |
| 改动 crate 数 | 1 + app 接线 | 2 |
| 演进灵活性 | **低**（第二类型要重写） | **高** |
| 风险 | R1 技术债 | YAGNI + 双对象一致性 |

## 4. 选择理由

**采纳 A（rev2）**。

- 本仓立场：最小核心 / 静态组合优先 / 不做用不上的抽象。**单生产用例下** 30 行 vs 200 行。
- B 的收益（多类型复用）**当前为零**，代价（多一个需与 `SessionSlot` 同步的对象）**当前就在**。
- **诚实交代 R1**：A 欠这笔债。须在 `docs/TODO.md` 记一条 ——「第三个后台任务类型出现时，
  把『落点格 + Delivery 三态 + take/mark』抽成 `ys-protocol` 的 `DeferredSink`（即 B §2.2）」，
  并写明触发条件。否则这 30 行会被复制第三遍。

**rev2 相对 rev1 的取舍**：`Delivery` 三态比「一位两用」啰嗦，但它把
「静默丢失」这个**用户不可见**的故障换成了「可能重复」这个**用户可见**的故障。
后者可以被发现、被发现后能自行纠偏。这是**有意的方向选择**，不是复杂度失控。

## 5. 生命周期与资源

| 事件 | 行为 |
|---|---|
| 命令正常结束 | sweeper 三段式投递 |
| 命令失败 / 被杀 | 同上，正文标明，`is_error` 正确 |
| 回合先结束、命令还在跑 | 条目留表（`Delivery::Pending`）；下回合开头补投 |
| 投递中被回合切换 | 条目停在 `InFlight`；>2 tick 后被 tick 修复回 `Pending` 重投 |
| abort（`Ctrl-C` / Esc） | 按既有 `background.rs:630-700` 杀进程组，**且置 Delivered**（不投递） |
| `/new` | 同上 + 清空滞留项（session 过滤） |
| 进程退出 | 同既有清扫 |
| **Print / JSON 模式** | **不挂后台能力**（§2.8） |

### 5.1 名额（P1-2）

**rev2 修正**：滞留的完成项与在跑项**分开计数**（review P1-2 指出共用名额会让
「启动构建 → 回合结束 → 下回合再启动」这种**正常用法**丢结果）。

| 计数 | 上限 | 超限行为 |
|---|---|---|
| 在跑 | 4（`max_running`） | `bash_bg` 同步报错 |
| 未投出的完成项 | **8**（独立计数） | 淘汰最旧的（删输出文件） |

`max_entries` 语义改为「未投出完成项的上限」，不再与在跑项共享。

### 5.2 fd 账（rev2 补充）

| 项 | 数值 | 依据 |
|---|---|---|
| 稳态 fd | 0 | 子进程持 stdout/stderr；父侧 spawn 后关闭 |
| 轮询瞬时 | +1 | 读完即关 |
| **`adopt` 收养者** | **+1 / 条** | 持有 `Child`（与既有 `BgEntry` 同） |
| 收养者上限 | ≤ 4（受 `max_running` 约束） | ⇒ 增量 ≤ 4 fd |
| tokio 任务 | 0 新增 | 复用共享 sweeper |

## 6. 已知风险与待验证项

- [x] ~~P0-1 `sweep` 锁内 fork/exec~~ → **§2.4 三分段解决**
- [x] ~~P0-2 mark/push 失序~~ → **§2.3 三态 + tick 修复解决**
- [x] ~~P1-1 Print/JSON 模式范围漏洞~~ → **§2.8 不挂后台能力**
- [x] ~~P1-2 滞留项占名额~~ → **§5.1 分开计数**
- [x] ~~P1-3 渲染失败丢数据~~ → **§2.7 `Unreadable` 形态**
- [ ] **R1 技术债**：机制不外溢 → 记 `docs/TODO.md` 并写触发条件
- [ ] **R2 两种所有权**：`BgEntry`（参与 reap）与 `Adopter`（自己 wait）并存，
      `sweep` 语义有两种。**rev2 未消解**，只保证两者都在锁外 kill
- [ ] **R4 行为契约变更**：`bash` 超时不再是 `Err`（**仅 TUI 模式**）。
      `bash.rs:592` / `:623` 等既有用例必须重写（须 ADR 签字）
- [ ] **R5 跨回合送达最迟等一条用户输入**：用户不再发消息则结果留表
- [ ] **R6 重复投递**：三态下仍可能投两次（`InFlight` 被中断后重投）。
      **有意接受** —— 见 §2.3 末段
- [ ] **R7** `prompt.rs` guideline 必须整体重写
- [ ] **R8** 子 agent 拿不到后台能力（`assembly.rs:131` 子表本无 `bash_bg`）—— 明记为限制
- [ ] **R9（新增）** `kill_group` 仍是同步 fork；本次只在锁外调，未异步化 →
      另记 TODO（`kill_group_async` 已存在，`proc_group.rs:40`）
- [ ] **R10（新增）** 补投可能一次灌入多条 × 50 KiB。**限制每回合最多补 2 条**，
      其余留到下回合

## 7. 测试矩阵

| 断言 | 变异（注入后应变红） |
|---|---|
| 完成时**恰好一条** `Deferred`；连续 5 tick 不重发 | 去掉 `Delivery` 判断 ⇒ 每 tick 刷一条 |
| **锁内无 fork / 无文件读**（用 `#[cfg(test)]` 计数器断言段 1 不进 IO） | 把 `render_completion` 挪回段 1 ⇒ 计数器红 |
| `InFlight` 超 2 tick 自动回 `Pending` 并重投 | 去掉 tick 修复 ⇒ 条目永久卡在 InFlight |
| 未绑定时完成 ⇒ 留表 ⇒ 下回合补投且**只补一次** | 不写 `Delivery` ⇒ 每 tick 重复注入 |
| 补投上限 2 条 | 去掉限流 ⇒ 3 条结果一次灌入 |
| abort 杀掉的条目不投递 | `kill_entry` 不置 Delivered ⇒ 取消后仍收到「killed」 |
| `bash` 超时（TUI）返回 `is_error: false` + 随后收到 `Deferred` | 恢复 `terminate` ⇒ 变红 |
| **`--json` 模式超时仍返回 `Err` 且杀进程**（§2.8） | 无条件走 hand_off ⇒ 本用例红 |
| 输出文件被删 ⇒ `Unreadable` 正文 | 退化成空正文 ⇒ 断言红 |
| 在跑 4 + 滞留 8 并存不互斥淘汰（§5.1） | 共用 `max_entries` ⇒ 滞留被淘汰 |
| `/new` 后新会话收不到旧会话完成项 | 去掉 session 过滤 ⇒ 变红 |
| 工具表 12 → 11 元 | 指纹锚点断言 |

## 8. 相关文档

- [`context.md`](./context.md) — 现状探索与实测数据
- [`review.md`](./review.md) — 质量分析（本文 rev2 的输入）
- [`adr-bg-notify.md`](./adr-bg-notify.md) — 决策记录
- [ADR-0017](../../adr/0017-compaction-hardening.md) — 系统注入走 `User` 角色
- [ADR-0049](../../adr/0049-final-answer-boundary-drain.md) — `Boundary` 消费语义
- `crates/ys-protocol/src/boundary.rs:23` — `Boundary::Deferred` 既有定义
- `apps/coding-agent/src/subagent.rs` — 异步 subagent「不需要 poll」先例
