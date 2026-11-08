# 跨回合推进设计：唤醒 · 判据 · 动作

> 日期：2026-11-06
> ✅ **已实现**（2026-11-06，W7–W8 落地 §5 的验收层）。as-built 见
> [`../../design-final/todo.md`](../../design-final/todo.md) —— 那里记录实际形态、与本文的偏差与已知取舍。
> 性质：**架构设计**（`docs/arch/**/design.md`）。决策见 ADR-0014（follow-up）、
> ADR-0041（撤回完工复核）、ADR-0010 附注（多回合驱动归 `app_loop`）。
> 竞品对照见同目录 `research.md`（**不进本文件作为理由**）。
> 质量分析见 `review.md`；follow-up 的完整实现细节见
> `docs/design-plans/2026-09-15-follow-up.md`。

> 📌 **状态更新（ADR-0073）**：二期定时器的「零 token 判据」**已撤销** —— `/loop`
> 改为纯时间驱动，到点即起回合（`should_tick` / `LoopState.last_head` /
> 非 git 拒绝登记三条一起删除）。本文件 §4.2–§4.5 与 §8 相关条目**保留原样作为历史设计记录**，
> 各自开头有撤销标注。**一期 follow-up 的 `git HEAD` 判据不变**（§3 不受影响），
> 三期（§5）不受影响。决策见
> [`../adr/0073-loop-time-driven-without-criterion.md`](../adr/0073-loop-time-driven-without-criterion.md)。

## 0. 一句话

**在回合结束的那一刻问一句「要不要再来一轮」** —— 需要就顺序再跑一个回合，
不需要就照常回到提示符。三种消费者（提交后同步 / 定时盯梢 / 干到清单核完）
都是这一句话的不同判据。

## 1. 三层划分

> ⚠️ **已撤销（ADR-0073）**：下表中「判据」一行对**二期定时器**已不存在 —— 定时器
> 唤醒即动作（纯时间驱动），`/loop` 不再有任何零 token 判据。三层划分对**一期**（`should_follow_up`）
> 与**三期**（清单 + 判官）仍然成立。另：「唤醒与判据不可合并」是结构约束；
> 唤醒源本身没有额外语义时（定时器），中间那层可以没有。以下为原设计记录。

| 层 | 回答什么 | 今天是 | 落点 |
|---|---|---|---|
| **唤醒** | 什么时候问 | 只在 `Request::Prompt` 臂内，回合刚结束时 | `app_loop` 各臂 |
| **判据** | 要不要再来一轮 | `git HEAD` 前后不等 | `post_turn.rs` 纯函数 |
| **动作** | 怎么再来一轮 | 顺序调 `run_turn_chain` | `app_loop.rs:570` 臂 |

**硬约束：三层不可合并。** 定时器只负责**醒来**，绝不判断「任务完没完」——
判据与唤醒混在一处，就会退化成「到点就跑」或「跑到完为止」这两种各自都错的形态。

## 2. 消费者与阶段

| 消费者 | 唤醒 | 判据 | 阶段 |
|---|---|---|---|
| **提交后同步文档** | 回合结束 | `HEAD` 变了 | **一期** |
| **定时盯梢** | 定时器到点 | ~~有活可干~~ → **无判据**（ADR-0073） | 二期 |
| **干到清单核完** | 回合结束 | todo 清单 + 证据验收 | 三期 |

一期只做第一行，但它把**动作层**与**判据的接线口**都建好了；
二、三期只换唤醒源与判据，动作层不动。

## 3. 一期：提交后追加一个回合

### 3.1 形状：第四个 `run_turn_chain` 调用点

`run_turn_chain`（`app_loop.rs:1304`）是现有三条调用点的唯一回合实现 ——
`Request::Prompt`（`:570`）、`Wake::SubagentReturned`（`:426`）、
`Wake::BackgroundReturned`（`:483`）。

本设计在 **`Request::Prompt` 臂内**追加**第二个顺序调用**：

```rust
Request::Prompt(message) => {
    let raw_user_text = crate::sessions::user_text(&message);
    let message = expand_skill_invocation(message, &config.skills);

    // 取点用**回合真正的工作目录** `config.cwd`，不用 `worktree.get()` ——
    // 后者在共享工作区（未 attach 树）下是 `None`，会让判据恒为假。
    let enabled = config.follow_up_on_commit();
    let workspace = enabled.then(|| config.cwd.clone());
    let head_before = post_turn::git_head(workspace.as_deref());

    let outcome = run_turn_chain(/* … */, message, raw_user_text).await?;

    // 追问**至多一次**，由**结构**保证：没有计数器、没有循环
    if post_turn::should_follow_up(&outcome, head_before.as_deref(), config.follow_up_on_commit()) {
        run_turn_chain(/* … */, post_turn::harness_input(), None).await?;
    }
}
```

**为什么不是 `while` 循环**：追问至多一次是**结构事实**，不是运行期约束。
一个顺序 `if` 让「不可能套娃」由代码形状保证 —— 无法被配置调坏。

**为什么尾部 `emit_view()`（`:972`）不动**：两次调用都在**同一个臂内**顺序执行，
尾部刷新看到的是 `stats` / `turns` **累加后**的值。这与 `Wake::*` 两臂（`:446` / `:503`）
不同 —— 那两臂 `continue` 跳过了尾部刷新，才需要各自补 `build_view`。
**本设计不 `continue`，不自补 view。**

### 3.1a 追问输入的呈现：`Role::Harness`（已定）

| 选项 | 判断 |
|---|---|
| `Role::Harness` | ✅ **采用** |
| `Role::User` | ❌ 用户没说过这些话，记成用户说的会污染“我是谁、我说过什么” |

**为什么不影响压缩**：“进不进摘要”由 `plan()`（`compact.rs:85`）按消息序列决定，
**与 `Role` 无关** ⇒ 用 `Harness` 不会把它排除出摘要。两件事正交。

**为什么不影响模型**：`Role::Harness` 只是**本地来源标记**；
出站时适配器把它映射成 user 角色（`crates/ys-core/src/message.rs:14` 明写）
⇒ 模型看到的仍是 user 输入。

**为什么对用户友好**：TUI 把它画成 `System` 行（`transcript.rs:186`，已有变异测试守这条）
⇒ 用户一眼能看出“这不是我说的”，不会误以为是自己的发言。

**追问内容本身要说清来源**，照 `background_placeholder()`（`app_loop.rs:1416`）的写法：

```rust
fn harness_input() -> Message {
    Message {
        role: Role::Harness,
        content: vec![ContentBlock::Text {
            text: "[harness] 本回合产生了新的提交，请检查文档（AGENTS.md 等）\
                   是否需同步更新；无需改动则直接说明。".to_string(),
        }],
        ..Default::default()
    }
}
```

**无新提交时** `should_follow_up` 返回 `false`，`harness_input` 根本不被调用
⇒ 不需要“无需改”的分支处理。

### 3.2 判据：`git HEAD` 前后对比

```rust
/// `git rev-parse HEAD`；非 git 仓库 / 无 git / 命令失败 → None
fn git_head(ws: Option<&Path>) -> Option<String>;
```

回合前记一次、回合后再记一次，**两者不等**即视为「这一回合里发生过提交」。

**为什么不解析工具参数找 `git commit`**：脆 —— `git ci`（别名）、`a && git commit`、
`bash -c "…"`、`git -c x=y commit` 都能绕过。

**为什么不装 `.git/hooks/post-commit`**：要往**用户仓库**写文件，越界；且「谁读标记、
何时读」需额外设计。

**副作用（正面的）**：HEAD 对比**也覆盖用户自己**的提交，不只 agent 的。

实现照 `worktree.rs:445` 的 `git()`（`Command::new("git").arg("-C")…`，清
`GIT_DIR` / `GIT_WORK_TREE`），但**不从 `worktree.rs` 导出** —— 它是 `WorktreeError`
的私有实现，跨模块复用会把错误类型绑死。

### 3.3 护栏

| # | 护栏 | 做法 |
|---|---|---|
| ① | **防套娃** | **由结构消除**：追问分支只出现一次，且自己不再递归 ⇒ 追问回合里若又提交，不可能触发第二次 |
| ② | **开关** | **默认关**。`user_config` 增字段（照 `max_turn_duration_secs` 的形态 `config.rs:161`），来源 `YUSHAN_FOLLOW_UP_ON_COMMIT`（照 `Config::from_env` 的越权形态 `config.rs:252`）。**开关关时不 spawn git** —— 先把开关读完，关则不取 `HEAD`（否则每回合白付两次 fork+exec） |
| ③ | **非 git 仓库静默跳过** | `git_head` 返回 `None` ⇒ 视为「检测不可用」，不追问、不报错 |
| ④ | **只在正常收场后追问** | `outcome` 为 `Err`，或 `StopReason` ∈ {`Cancelled`, `Truncated`, `MaxRounds`, `NoOutput`, `Failed`} ⇒ **不追问** |

护栏 ④ 的理由：与 ADR-0041 撤完工复核所确立的原则一致 ——「用户中止永远赢」，
非正常收场不该再追加成本。

### 3.4 改动面

| 落点 | 改什么 |
|---|---|
| `app_loop.rs` | `Request::Prompt` 臂两次 `run_turn_chain`；`run_turn_chain` 返回 `TurnOutcome`（§3.5，**复用已有类型别名**）；不变量 ④（`:54`）措辞同步 |
| **新增** `post_turn.rs` | `git_head` + `should_follow_up` + `harness_input`（纯函数，易测） |
| `config.rs` | 开关字段（`:161` 的 `user_config` + `:252` 的 `from_env`） |

**总量约 80 行**（其中 `post_turn.rs` 约 60 行，含测试）。`worktree: &WorktreeHandle`
已是 `run_turn_chain` 的参数（`:1318`）。

> ⚠️ **HEAD 的取点必须是 `config.cwd`，不能是 `worktree.get()`。**
> `WorktreeBinding` 在**共享工作区**（未 attach 工作树，即绝大多数启动形态）下是 `None`
> —— 主程序用 `worktree_name.zip(worktree_path)` 组出它，两半缺一即为 `None`。
> 用它作取点，`git_head(None)` 直接返回 `None` ⇒ 判据恒为假 ⇒ **功能 100% 不生效，
> 且纯函数测试全绿**。`config.cwd` 才是「本回合在哪跑」，且 `/resume` 切工作区时
> 已被 `apply_resumed_workspace` 刷新。

> ⚠️ **模块不叫 `follow_up.rs`** —— 那是 ADR-0041 显式删除的文件名，复用会让后人读成回退。

**不碰** `ys-loop` / `ys-runtime` / `ys-protocol`。理由：多回合驱动本就归 `app_loop`
（ADR-0010 附注）；且 `RunResult` 住在 `ys-loop`，从 `ys-component` 碰它会成依赖环（ADR-0014）。

### 3.5 唯一不能绕开的结构改动：`run_turn_chain` 交出 `TurnOutcome`

`run_turn_chain` 今天是 `-> Result<(), AppLoopError>`（`app_loop.rs:1320`）⇒
**拿不到 `StopReason`**，护栏 ④ 无法实现。

**`TurnOutcome` 已经存在**，不必新建（`app_loop.rs:1152`）：

```rust
// app_loop.rs:1152（已有）
type TurnOutcome = Result<RunResult, LoopError>;
```

`run_turn_once` 已经返回 `Result<TurnOutcome, AppLoopError>`（`:1457`），
`RunResult.stop_reason` 就是 `StopReason`（`crates/ys-loop/src/result.rs:5`）。

⇒ 改动只是把 `run_turn_chain` 的 `Ok(())` 换成 `Ok(outcome)`：

```rust
) -> Result<TurnOutcome, AppLoopError> {          // 原：Result<(), AppLoopError>
    …
    match run_turn_once(…).await {
        Ok(outcome) => {
            match &outcome {
                Ok(result) => { /* 原有 stats / meta 写入，改用引用 */ }
                Err(e) => { /* 原有日志 */ }
            }
            Ok(outcome)                                  // ← 交出去
        }
        Err(e) => { /* 原有 debug 收尾 */ return Err(e); }
    }
}
```

三个既有调用点（`:426` / `:483` / `:570`）忽略返回值即可，不改行为。

> ⚠️ **不要新建同名结构体** —— `TurnOutcome` 是 `:1152` 已占用的类型别名，重名会遮蔽它。

### 3.6 不变量 ④ 的措辞同步

`app_loop.rs:54` 现在写：

> **④ 每次用户输入恰好跑一个回合**（ADR-0041 撤回 ADR-0027 决策 2 的完工复核）

一期落地后这句**必须改**，否则代码注释与行为相反：

> **④ 每次用户输入跑一个回合，至多追加一个 harness 回合**：
> 追加只能由**外部可观测事实**触发（`git HEAD` 前后不等），**不能**由模型自述触发。
> ADR-0041 撤回的是后者（模型说「我做完了」），不是前者。

**ADR-0041 的决策不退**：它反对的是「模型自述」作为触发依据。HEAD 差异是外部事实，
与「模型觉得自己完工了」不是一回事。

## 4. 二期：定时唤醒

### 4.1 形状：第四条 `select!` 分支

`Wake`（`app_loop.rs:282`）加一个变体，外层 `select!`（`:389`）加一条分支：

```rust
enum Wake {
    Request(Option<Request>),
    Models(Option<ModelCache>),
    SubagentReturned,
    BackgroundReturned,
    TimerFired,        // 新增
}
```

`optional_wake` 已把「可选唤醒源」做成一条 `select!` 分支（源为 `None` 时该分支永不就绪），
加第二条与加第一条同构：

```rust
let timer = loop_state.as_ref().map(|s| s.next_at);
// tokio::time::sleep_until 吸收 Instant；源为 None 时分支被禁用。
Some(()) = optional_wake(timer.map(|at| tokio::time::sleep_until(at.into()))) => {
    Wake::TimerFired
}
```

**`LoopState` 存在哪**：`app_loop::run` 主循环的**局部可变变量**（与 `turns` / `stats` / `session_started`
同级），**不进 `Wiring`**。理由：它不属于任何组件，且生命周期就是本 `run` 调用。

| 事件 | 对 `LoopState` 的效应 |
|---|---|
| `/loop <间隔> <提示词>` | 覆盖为 `Some(LoopState { next_at: now + interval, .. })` |
| `/loop off` | 置 `None` |
| `Wake::TimerFired` | `next_at = now + interval`（无论是否起回合，**必须先推远**）|
| `/new` / `/clear` | 置 `None`（新会话不继承旧循环）|
| `/resume` | 置 `None` + 一次性提示（不自动续跑）|

### 4.2 硬规则：唤醒后必须先过**零 token 判据**

> ⚠️ **已撤销（ADR-0073）**：本节这条硬规则对定时器作废。`Wake::TimerFired` 现在
> 仍是「先 `advance()` 推远 `next_at`（防忙转）」→ 直接起回合，**不跑任何判据**。
> 撤销理由：判据答不了「有没有活」，且它把四类正当工作（展示/汇报、读脚本输出、
> 远程状态、纯维护）判成「没活」⇒ 永不执行。以下为原设计记录。

**定时器只能「醒来」，绝不直接起回合。** 醒来后先跑一个判据，通过才花钱：

```rust
Wake::TimerFired => {
    // 先推远下次唤醒 —— 无论判据结果如何。放在这里而不是分支末尾，
    // 是因为下面两条路径都要经过它（包括 `continue`）。
    let next = timer.advance();
    let work = post_turn::has_work(ws.clone(), timer.last_head.clone()).await;
    if !work {
        continue;          // 不 append、不调模型 ⇒ 成本 0
    }
    timer.last_head = post_turn::git_head(ws.as_deref());
    let message = loop_placeholder();
    run_turn_chain(...).await?;
}
```

**两次 `run_turn_chain` 的位置不同**：`Wake::*` 两臂 `continue` 跳过尾部 `emit_view()`
（`:972`），所以它们各自补 `build_view`；本臂**不 `continue` 在成功的路径上** ——
它落到尾部刷新。但 `continue` 在判据为空时仍要执行 ⇒ **那条路径不需要刷视图**
（什么都没变）。

**这不是新发明** —— 现有两条分支就是这个形状：

| 分支 | 判据 | 位置 |
|---|---|---|
| `Wake::SubagentReturned` | `pending_results().has_pending()` | `app_loop.rs:413-421` |
| `Wake::BackgroundReturned` | `should_start_background_turn(...)` | `app_loop.rs:472-478` |
| **`Wake::TimerFired`** | **`has_work(...)`** | 本设计 |
照抄形状，只换判据。另外要照抄一条细节：**判据只读不取**（`app_loop.rs:414` 注释）——
真正的取走发生在回合开头，双取会重复送达。

### 4.3 `has_work` 的四条硬约束

> ⚠️ **已撤销（ADR-0073）**：`has_work` 与 C1–C4 随判据层一起删除。其中
> **C3（命中后必须 `advance()`）以「无条件先推远」的形式保留** —— 它与判据无关，
> 是防忙转本身的要求；C1/C2/C4 是判据的约束，没有判据就没有它们要约束的对象。
> 以下为原设计记录。

| # | 约束 | 违反的后果 |
|---|---|---|
| C1 | **不得调用模型** | 「零 token」失效，等于醒来就跑回合 |
| C2 | **不得 append 任何东西进 session** | 每 5 分钟一条，会话照样涨，压缩白忙 |
| C3 | **命中后必须 `timer.advance()`** | `continue` 立刻再次就绪 ⇒ 忙转 |
| C4 | **必须快（毫秒级）且不阻塞用户输入** | 它跑在 `select!` 之后；几秒的判据会拖住用户敲字 |

#### C4 的实现手段：`spawn_blocking`

`git rev-parse` 是**阻塞**调用（`std::process::Command`），直接 `.await` 会卡住整个
app 循环 —— 用户敲字、后台清扫、子 agent 回传全部停。

⇒ `has_work` 是 `async fn`，内部把 git 调用交给 `tokio::task::spawn_blocking`：

```rust
pub async fn has_work(workspace: Option<PathBuf>, last_head: Option<String>) -> bool
```

判据本身只有一条：**HEAD 与上次起完回合时不同**。理由：

| 判据 | 为何不收 |
|---|---|
| 文件 mtime | 工作区里**任何**后台进程（含编译器、编辑器自动保存）都能碰它 ⇒ 假阳性常态化，每 5 分钟白跑一轮 |
| 外部命令 | 起进程秒级，破坏 C1 与 C4 |

**代价**：一期判据（HEAD 不等）在二期被复用，但**基准不同** ——
一期比「本回合前后」，二期比「上次 tick 之后」。两者共用一个纯函数，两个调用点传不同的基准。

**假阴性风险与缓解**：只看 HEAD 会漏掉「改了文件但没提交」的工作。
这不是疏忽 —— 它由 §4.4 的代价不对称决定：宁可宽松。真要盯未提交改动，
写进 `/loop` 的提示词让模型自己看（一期已证明 HEAD 不是唯一信号）。

C4 决定了什么能放进 `has_work`：

| 判据 | 成本 | 归哪档 |
|---|---|---|
| `git HEAD` 变了没 | 一次 `rev-parse`，微秒 | 可直接进 |
| 文件 mtime 变了没 | `stat`，微秒 | 可直接进 |
| 外部命令（`gh pr checks` / `make -q`） | **起进程，秒级** | **不进**（除非带超时）|

⇒ **`has_work` 只查本地廉价事实。** 外部状态检查进回合内的工具去查 ——
把外部命令塞进 `has_work` 会同时破坏 C1（本质上已经是「跑一段活」）与 C4。

### 4.4 `has_work` 的职责边界：它不判语义

> ⚠️ **已撤销（ADR-0073）**：本节对定时器不再适用。特别是「两种错误的代价不对称」
> 推出的「判据宁可宽松」—— 判据本身就没了，这条取舍不再有承载者。
> 「tick 回合不判语义、语义判断归三期」这一结论**仍然成立**（属于回合内容，与判据无关）。
> 以下为原设计记录。

`has_work` 只能回答「**外部状态变了没**」，**不能**回答「**这活干完了没**」：

| 问题 | 例子 | `has_work` 能做吗 |
|---|---|---|
| 外部状态变了没 | CI 红了？文件改了？新 issue？ | ✅ 能 |
| **语义上算不算完** | 文档与代码一致了？测试覆盖够吗？ | ❌ **不能** —— 要理解内容，任何零 token 判据都做不到 |

这不是实现缺陷，是**问题性质**决定的。语义判断归三期（§5）。

#### 两种错误的代价**不对称**

| 错误 | 后果 | 严重性 |
|---|---|---|
| **假阴性**（有活说没活） | 该跑没跑 ⇒ **漏了** | **严重** |
| **假阳性**（没活说有活） | 白跑一个回合，多花点 token | 可接受 |

⇒ **判据宁可宽松，不能太严。** 例：判断「PR 有没有新评论」时，
`has_work` 只看「评论数变了没」，**不判断评论重不重要** —— 让模型在回合里判断。

### 4.5 session 归属（已定）

> ⚠️ **已撤销（ADR-0073）**：本节末尾「§4.2 零 token 判据要求解决的问题：
> **没活的 tick 不 append ⇒ 会话不增长**」这一句失效 —— 已无判据，**每次 tick 都 append
> 并起回合**，会话按间隔线性增长。压缩接管（本节前半段的阈值论证）不受影响，
> 但「每轮重发多少」这一成本项现在没有 harness 侧的刹车，属用户责任（间隔由用户给）。
> 本节关于「进摘要 / 画成 System 行 / user 角色」的三层取向**不变**。以下为原设计记录。

**一个 session：tick 消息与用户主动交流的消息在存储与压缩上完全同等。**

| 方面 | 结果 |
|---|---|
| 落盘 / `--resume` | 和用户对话一样恢复 |
| 进压缩摘要 | **进**，`plan()` （`compact.rs:85`）一行不用改 |
| 模型看到 | user 角色（`Harness` 出站映射，同 §3.1a）|
| UI | 画成 `System` 行（与追问同规）|

**为什么不需要为 tick 单独设计 session**：压缩阈值是 `window × 0.85`
（`ys-component/src/compact.rs:70`，默认窗口 1e6）⇒ **85 万 token 才压一次**。
一条 tick 按 500 token 估，填满要 ~1,700 条；5 分钟一次 = **6 天**才压一次。
摘要里的 tick 占比 < 1%，**不值得为它加过滤逻辑**。

> 真正的问题不是「占多少」，而是「**每轮重发多少**」（压缩后 `keep_ratio` 仍按窗口算）。
> 这正是 §4.2 零 token 判据要求解决的问题：**没活的 tick 不 append ⇒ 会话不增长**。

**三个层面各自的取向**（易混淆，故分开写）：

| 层面 | 取向 | 理由 |
|---|---|---|
| **压缩** | tick **进**摘要 | 量小（<1%）、不做特例、`plan()` 不用改 ⇒ **最易维护** |
| **UI** | 画成 `System` 行 | 用户没说过 ⇒ 不能画成用户发言 |
| **模型** | user 角色 | 适配器既有映射，无需改 |

**“同等”指的是存储与压缩路径，不是 UI 呈现。**

### 4.6 定时器的创建与取消

§4.1–§4.5 只说了“到点唤醒”，没说**谁创建它、怎么停**。

#### 创建：先做**一条命令**，不做通用工具

| 方式 | 判断 |
|---|---|
| **`/loop <间隔> <提示词>`** | ✅ **采用** —— 用户直观，与 `research.md` 三家形态一致 |
| config 字段 | ❌ 只能再启动生效，不符合“边聊边用” |
| 模型调新 tool | ❌ 模型可自己排循环 = 又一个“模型自述驱动”，与 ADR-0041 相左 |

**改动面**：`Request` 加一个 `Request::Loop { interval, prompt }`（`ys-protocol/src/request.rs`），
在 `app_loop` 的 `Request` 分支处理（不发回合，只更新定时器状态）。

**命令解析在 `crates/ys-tui-coding/src/commands.rs` 的命令分派处**（`"compact"` 那一带），
**不在** `capabilities.rs` —— 后者是「收到 `Request` 之后世界怎么变」，
命令名 → `Action` 的映射归 TUI。两类结果分开：

| 输入 | `Action` | 谁处理 |
|---|---|---|
| `/loop 5m xxx` | `Request::Loop { .. }` | app 更新定时器 |
| `/loop off` | `Request::LoopOff` | app 清定时器 |
| `/loop`（无参）/ 非法间隔 | `Action::Local(LocalAction::Hint(..))` | **TUI 本地提示**，不发请求（**多字节单位也不得 panic** —— 按字符切、不按字节切） |
| `/loop` 但工作区非 git 仓库 | ~~`Request::Loop` 被拒~~ → **不再有这条分支**（ADR-0073） | — |
| 状态显示 | — | 状态行读**视图快照**，不是命令输出 |

**间隔格式**：只收 `<数字><单位>`，单位 ∈ `s` / `m` / `h`（`5m` / `90s` / `2h`）。
下限 **10s**（低于它就是在烧钱）；上限 **24h**。非法值本地提示、不改状态。

#### 取消：单一入口

| 触发器 | 行为 |
|---|---|
| `/loop off` | 清定时器；**已在跑的回合不中断** |
| `Ctrl+C`（空闲时） | **不单独接** —— 它直接退出进程，循环随进程一起消失。多按一次确认是 `Ctrl-D` 的既有语义，插入一层「先停循环、不退出」会让退出路径变长 |
| `Esc`（空闲时） | **不接** —— 它是「清空输入框」，被用户高频用来改错字。让它顺手停掉循环 = 一次误按丢掉定时器 |
| 用户直接敲字 | **默认不取消** —— 用户可能只是想插话 |

**只有 `/loop off` 一个取消入口**，这是刻意的：定时唤醒会在「用户不在看屏幕」时
花钱，所以停它的动作必须是**明确的、不能被别的键顺带触发**的。退出进程也是一条
合法取消路径（进程没了就没有定时器），只是它同时结束会话。

**“已在跑的回合不中断”是硬规则**：取消定时器 ≠ 取消当前工作。
若一并中断，用户会因为“取消了个循环”而丢掉一个已干一半的回合。

#### 状态必须可见

状态行必须显示（无可见性的定时器是隐形烧钱机器）。这要**跨三层**，缺一层就看不见：

| 层 | 落点 | 改什么 |
|---|---|---|
| 协议/视图 | `crates/ys-tui-coding/src/view.rs` `CodingView` | 加 `loop_state: Option<LoopView>` |
| 快照 | `apps/coding-agent/src/view.rs` `build_view_with_loop` | 在 `build_view` 之上填 `loop_state`（`build_view` 有十余处调用点，其中绝大多数与定时器无关，所以另开一个入口而不是拉长签名） |
| 渲染 | `crates/ys-tui-coding/src/draw.rs` `status_text` | 插一段（位置见下） |

```rust
pub struct LoopView {
    /// 间隔的显示串（`5m` / `2h`）——已格式化，渲染层不再算。
    pub interval: String,
    /// `None` = 正在跑回合；`Some` = 空闲，给的是**相对时长**（`14 分后`）。
    ///
    /// 不是钟点：`Instant → 本地时间` 需要时区偏移，而 `ys-tui-coding::clock`
    /// 里的 `hhmm_at` 是 `pub(crate)`、app 侧拿不到。冒充一个 UTC 钟点让用户
    /// 按错的时间等待，比给相对时长更糟。
    pub next_label: Option<String>,
}
```

状态行形状：

```
[loop] 5m · 14 分后     ← 空闲
[loop] 5m · 正在跑      ← 有回合在执行
```

**视图刷新只有一个位置**：循环尾部那次统一 `emit_view`。各请求臂不自行补刷 ——
那会让每个 `/loop` 产生两个 `View`（UI 白重绘一帧，且测试会把第一条命令的第二个
`View` 误认成第二条命令的结果）。`Wake::TimerFired` 是例外（它 `continue`，走不到
尾部），故由它自己发。

**段的位置**：紧跟首段（`⏳` / `○`）之后、`model` 之前。理由同 `status_note`
（`draw.rs:967-969`）：窄终端剥尾时它先掉不了；它是「正在被自动推进」的信号，
比 model / tokens 更紧急。

`--resume` **不自动续跑**，但恢复后要显示一条**一次性提示**（走 `Outbound::Output`，
不常驻状态行）：`上次的循环已停止，/loop 重启` ⇒ 用户不会误以为还在被盯着。

### 4.7 队列形状：单条（已定）
```rust
struct LoopState {
    next_at: Instant,
    interval: Duration,
    prompt: String,
}
```

**一个 session 一个循环。** `/loop <间隔> <提示词>` 再敲一次即**覆盖**上一条
（与「单条」语义一致，不报错、不叠加）。

| 选单条的理由 | |
|---|---|
| 覆盖绝大多数场景 | 用户说「盯着 CI」就是一个循环 |
| 多任务是**假想未来需求** | 与 ADR-0014 拒绝建 hook 系统同一理由 |
| 多条要处理冲突 | 两个循环同时到点怎么办？串行 / 合并 / 排队 —— 每个都要设计 |
| 多条要处理可见性 | 状态行要显示多条 + 各自的下次时刻 |
| **甲→乙是加法** | 把 `next_at` 换成堆，判据层与动作层一行不动 |

**何时该改成乙**：真出现「同时盯几件不相关的事」时。
但注意：那三件事本来就可以写进**同一条循环的提示词**，让模型每轮自己判断查哪个 ——
所以乙的需求比看上去少。

## 5. 三期：todo 清单 + 证据验收

判据从「`HEAD` 不等」换成「清单里的活真的干完了」。**动作层与唤醒层完全不动**，
只换「该不该继续」的判断来源。

### 5.1 三层判据，各归其位

| 层 | 判据 | 成本 | 能答 | 阶段 |
|---|---|---|---|---|
| 一 | `should_follow_up`（HEAD 不等） | **0** | 「哪儿变了」 | 一期 |
| 二 | ~~`has_work`（本地 mtime / HEAD）~~ → **无判据**（ADR-0073） | — | — | 二期 |
| 三 | 清单 + 判官（读证据） | 清单 0 / 判官每轮一次 | **「真干完了吗」** | 三期 |

**关键澄清：零 token 判据不是万能解。** 它只能把「完全没事」的 tick 拦掉；
真正需要核对时，token 该花就得花 —— 那是问题性质，不是实现选择。

### 5.2 两档判据（三期内部）

| 档 | 判据 | 成本 | 用途 |
|---|---|---|---|
| 一档 | `has_open_items()` / 剩余 todo 集合指纹 | **零**（harness 自己比） | 推进与停滞 |
| 二档 | 清单 + 证据交给判官逐条核对 | 每轮一次模型调用 | 「声称干完的那条，真干了吗」 |

**分层理由**：一档便宜且可信（结构化数据），先跑；二档贵，只在核对凭据时用。
判官**不调工具**，只能看回合里**已经露出**的证据 ⇒ agent 必须把验证跑出来才算数。

### 5.3 判据必须外部可读

ADR-0041 否决的是「模型自述」作为依据。判据来源的合法集合：

- ✅ harness 可直接观测的事实（HEAD / 文件存在 / 退出码 / 测试结果）
- ✅ 独立判官读**对话里已露出的证据**（判官不能改状态，只能判）
- ❌ 执行回合的模型自己写「我完成了」—— **包括把 todo 项标成 `completed`**

**最后一条是关键**：清单项的状态本身也是自陈，所以它只能当「待核对的问题」，
不能当「已完成的证据」（§5.5）。ADR-0024 自陈的天花板就在这里。

### 5.4 三期的形状：todo 清单 + 证据验收

**用户只说目标**（`/goal 修好登录超时`）—— **不说「什么算完成」**。用户写不出验收条件
（他不知道仓里有哪些测试、该验哪条），要他写等于把实现细节推回给用户。

完整链路分两层，**分别由 ADR-0024 与 ADR-0072 定义**：

```
用户：/goal 修好登录超时
   │
   ├─【清单层 · ADR-0024】agent 边干边维护 todo 清单（落盘）
   │     「还剩什么没干」—— 零 token，机器可读
   │
   └─【验收层 · 本期】判官拿清单逐条去证据里找
         「声称干完的那条，证据在哪」—— 一次模型调用
```

| 层 | 回答什么 | 机制 | 出处 |
|---|---|---|---|
| 清单 | 还有活没干完？ | `store.has_open_items()` | ADR-0024 决策 3 |
| 验收 | 干完那条真干完了？ | 判官 + 证据窗口 + 引用校验 | 本期（§5.5–5.7） |
| 停滞 | 在原地打转？ | **剩余 todo 集合指纹**连续 N 轮相同 | 本期（§5.8） |

**为什么不是「用户写条件」**：上一版设计要求用户写 `/goal 测试通过`。那是错的 ——
它把「该测什么」这个只有 agent 知道答案的问题推给了用户。

**为什么不是「agent 事前冻结契约」**：真实干活时计划会演化（干到第 3 步才发现还要改文档）。
冻结契约不允许追加 ⇒ 逼 agent 在第一轮就想全，与实际不符。
TODO 清单允许追加，但**不允许静默删除**（§5.4.2）。

#### 5.4.1 清单层：复用 ADR-0024，不重新设计

ADR-0024（accepted，**未实现**：全仓 `TodoStore` / `todo.json` / `should_inject` grep = 0）
已定完整形状：`adapters/tools-todo` 新 crate、`TodoStore` 唯一真值、
`apply` 全量重写、`render_block` 字节稳定、`has_open_items` 推进判据、
`MAX_FOLLOW_UPS = 8`。本期**直接实现它**，不另起炉灶。

**落盘（本期明确采纳）**：真值在 `<sessions_dir>/<stem>.todo.json`，与会话同级。

| 理由 | 说明 |
|---|---|
| 活过压缩 | 压缩摘的就是旧消息；todo 在 session 里必然被摘掉。ADR-0021 已记过这个缺口 |
| 活过重启 | 复用 `Wiring::resume` / `resume_into` 的既有恢复路径（同 stem 开回），**零新机制** |
| 活过回合 | 长程任务必然撞 `max_rounds` ⇒ 多回合；真值不能只在回合内有效 |
| `/new` 语义清楚 | 换 stem ⇒ 计划天然归零，与 ADR-0021「`/new` 后 todo 归零是正确语义」一致 |
| 零新目录 | 会话同级 ⇒ `YUSHAN_SESSIONS_DIR` 的测试覆盖自动生效 |

**落盘策略按入口分**：TUI 落 `<stem>.todo.json`（用户裁决：必须活过压缩 / 重启 / 回合）；
`-p` / `--json` 走 `TodoStore::open(None)`（纯内存）—— 一次性会话没有「下次」，但**同一次运行内的
多回合追问仍然生效**（store 生命周期 = 进程）。

#### 5.4.2 清单的两条硬规则（不靠模型自觉）

都在 `TodoStore::apply` 里，**执行者是代码**：

| 规则 | 防什么 |
|---|---|
| **拒绝丢弃既有非终态项** | 模型发一个 `[]` 就关掉整个闭环 |
| **无 open 项 ⇒ 清空 + 删文件** | 判据写「全部 Completed」时，一个被放弃的项会永久卡住 |

配套终止态 `Cancelled`：它是模型能**诚实停下**的唯一出口。没有它，模型想让环停下
就得把没做完的事标成 `completed` —— 那是**伪造完成**。

#### 5.4.3 两层如何咬合

清单提供**结构**（该验哪几条），验收提供**可信度**（那几条真做到了吗）。

**只有清单没有验收** = ADR-0024 自陈的天花板：

> todo 是模型的**自陈**，不是**验收**。……模型可以把全部项标成 `completed` 然后收工，
> 代码照约删除。**真正的保证来自可验证的验收**，不来自清单。

**只有验收没有清单** = 判官没有检查对象，只能拿用户那句模糊目标去猜。

⇒ 两者缺一不可。这是把 ADR-0024 与 ADR-0072 合成一条链路的理由。

### 5.5 证据窗口：判官读什么

判官读三样东西，划分是防幻觉的**机制**，不是措辞：

| 段 | 内容 | 可否支撑「工作已完成」 |
|---|---|---|
| **清单** | todo 的**全部项**（内容 + 状态），含 `skipped` 与理由 | 这是**待核对的问题**，不是证据 |
| **证据** | 工具调用（名字 + 参数，截 200 B）+ 工具输出（截 1200 B）+ harness 事实（HEAD 变化 / 轮数 / 耗时 / 工具调用数） | ✅ 可以 |
| **助手自述** | 助手本轮正文 | ❌ **不可以** |

**判官的任务不是「条件满足了吗」，而是「逐条核对」**：

```
对清单里每个非终态项，在证据里找它的完成凭据：
  - 找得到 ⇒ 该项 OK
  - 找不到 ⇒ 该项未完成 ⇒ 整体 NOT_MET
```

⇒ 判官输出必须**逐条**回答（§5.7 的 `EVIDENCE` 格式），不允许一句「都做完了」结论式放行。

**为什么清单进证据窗口但状态不算证据**：
模型把某项标成 `completed` 是**自陈**（ADR-0024 决策 4 的天花板），不能当证据；
但它标了哪些项、跳过了哪些项，是判官**必须核对的对象**。
⇒ 清单提供「查什么」，证据提供「查到什么」。

> ⚠️ **证据窗口 ≠ stall 摘要的输入。** 这里列的是**判官**能看到的东西；
> stall 摘要（§5.8）只看 **todo 集合**（零成本），根本不读工具输出。
> 两者刻意分开：判官看全部（贵、准），stall 看集合（免费、糙）。

> 两段可按 `ContentBlock` 变体精确区分：工具结果是 `ContentBlock::ToolResult`
> （`crates/ys-core/src/message.rs:35`），助手正文是 `ContentBlock::Text`。
> **不靠 `Role` 区分** —— 工具结果以 `Role::User` 进 session（`crates/ys-loop/src/basic.rs:1420`）。

**大小界定：只读「最后一轮」，不读整段 session。**

- ⇒ 判官输入 **O(1)**，与总轮数无关（成本不随轮数线性增长）。
- 代价：跨轮证据不可见 ⇒ 判官只能核对**本轮可验的项**。
  跨轮才完成的项（前面轮跑过的测试）拿不出凭据 ⇒ 被保守地判为未完成。
  缓解：`guideline` 要求 agent 在收尾轮**重跑关键验证**（测试/构建）。
  这本来就是好习惯，且比让判官读整段 session 便宜得多。
- 读整段 session / 滚动摘要被否：成本随轮数线性增长，且把**模型自己的叙述**重新喂给判官 ⇒ 放大 ADR-0041 的坑。

**硬上限**：清单 + 证据摘录合计 ≤ 6000 B。
清单过长（>600 token）⇒ 提示 agent 合并条目，**不静默截断**（截了判官就看不见被截的项）。

### 5.6 判官的调用形态

参照 `crates/ys-loop/src/summarize.rs:56` `ModelSummarizer` 的既成模式（「用模型读一段内容做加工」）：
判官是一次独立的 `model.complete` 调用，**不经 `Agent`、不进回合、不带工具表**。

继承该文件注释里的两条硬理由：

1. **不带工具表**（`tools: Arc::new(vec![])`）—— 省 token，且杜绝判官吐 `tool_call`（它拿不到工具结果）。
2. **关闭思考**（`thinking: Some(ThinkingOverride::off())`）—— 思考会计入 `max_tokens`，能把预算吃光导致正文为 **0 词**。
   判官 `max_tokens = 128` 比摘要的 1024 更紧 ⇒ 关闭思考是**必需**而非可选。

**不进事件系统**（不扩 `ys-event`、不加 `AgentEvent` 变体）：目标属 coding-agent，
加进核心词汇会让所有消费者都得认识目标概念。

**判官自由文本永不回灌进 session。** 判官结果只以**一行标签**进 session（`Role::Harness`，与一期/二期同形态）：

```
[harness] 目标判定：not_met（继续）
[harness] 目标判定：met（停止）
[harness] 目标判定：blocked（停止）：<理由 ≤120 字>
```

**理由只给人看**（`-p`/`--json` 打到 stderr / 终态摘要），不进模型上下文 ⇒
① 断掉「对话里的恶意文本 → 判官 → 回灌 → 再进模型上下文」的注入放大回路；
② 每轮理由进 session = 每轮永久 token 成本，且随上下文复利。

### 5.7 判定与降级：两种错误的代价不对称

假阳性（误判达成 ⇒ 过早停止，用户以为做完了）**远比**假阴性（误判未满足 ⇒ 多烧几轮钱）严重
⇒ 全部偏向保守。

判官输出结构化但宽容解析：

```
VERDICT: MET | NOT_MET | BLOCKED
ITEMS:
- <清单项原文或编号> | OK | <证据里逐字摘录的一段，≥12 字符>
- <清单项原文或编号> | MISSING | -
EVIDENCE: "<从证据段逐字摘录的一段，≥12 字符>"
```

**`MET` 的充分条件（机制强制，不靠判官自觉）**：

1. `VERDICT: MET`；
2. `ITEMS` 里**每个非终态项**都标了 `OK`，且每项的引用都能在证据窗口里逐字命中；
3. 至少一条引用命中。

任一不满足 ⇒ 降级 `NOT_MET`。⇒ **模型把项标成 `completed` 但没跑出凭据，判官结构上不可能放行**。

解析扫**前 8 行非空行**取 `VERDICT`，值**必须整词等于** `MET|NOT_MET|BLOCKED`（不做前缀匹配 ——
否则 `METRICALLY` 会被误判成 MET，正是最危险的假阳性方向）。`ITEMS` 逐行解析，解析不出的行忽略。

| 情况 | 处理 |
|---|---|
| 解析失败 / 空输出 | `NOT_MET` + 计入可靠性计数。**不重试解析**（同输出重试无益） |
| `MET` 但**任一项**引用未命中证据窗口 | 降级 `NOT_MET` + 计入可靠性计数 |
| `MET` 且全部项引用命中 | `Met` |
| `NOT_MET` | `NotMet`（继续） |
| `BLOCKED` | `Blocked`（目标不可达成 ⇒ 停止，让人能及时止损） |
| 模型调用失败 / 超时 | 重试**一次** → 仍失败则**停止**（不放行继续：没有判据就没有停止条件 ⇒ 跑飞） |

**引用校验是本设计的核心防幻觉机制**：harness 在**排除助手自述**的证据窗口里做子串匹配
（归一化空白后比对）。⇒ **模型自称「已完成」在机制上无法通过判官** ——
除非它真的跑出了通过的证据，因为自述段不在匹配池里。

这正面解决了 ADR-0041 的真问题（模型自述不可信），且是**机制**而非提示词叮嘱。
**三家里没有一家这么做**：codex 靠 `continuation.md` 的提示词叮嘱模型自审，
CodeWhale 靠模型**自填** `verification` 回执且 harness 不校验（`goal.rs:421`）。
引用校验是本期唯一比三家强的地方。

### 5.8 闸门：停止与防跑飞

闸门分两类：**硬闸门**（触发即停）与**软提醒**（触发只提示，不中断）。
全部由 app 层 `GoalState` 计数（唯一持有者，两条入口共用）。

| 闸门 | 形态 | 默认 | 触发结果 |
|---|---|---|---|
| 轮数 | **软提醒** | **60 轮**（触发后翻倍） | 提示「已第 N 轮」，**不中断** |
| 墙钟 | **硬闸门** | **48 h** | 未达成（预算） |
| 无进展 stall | **硬闸门** | 连续 **3 轮** todo 集合指纹不变 | 无进展 |
| 判官可靠性 | **硬闸门** | 连续 **2 次**解析失败 / 引用未命中 | 判官不可靠 |
| 回合健康 | **硬闸门** | 主回合 `Failed`/`Cancelled`/`MaxRounds`/`Truncated`/`NoOutput` | 回合失败（**立即停**，不算「未满足」） |
| token 预算 | **不设** | — | — |

**轮数只做软提醒，不做硬顶。** 依据 ADR-0027 决策 6（「真正的兜底是这条**小时级**阀门，不是轮数」）与
仓库既有形态：`Config::max_rounds` 的 app 默认是 `u32::MAX`（`apps/coding-agent/src/config.rs:434`）、
`Config::max_turn_duration` 默认 48 h（`config.rs:441`）、回合内轮数软提醒默认 60 轮且**触发后翻倍**
（`crates/ys-component/src/limits.rs:34`）。三期的轮数提醒沿用同一形态（60 轮起、翻倍、只提醒），
**不引入比仓库现状更紧的硬顶**。

**token 预算不设。** 去掉 token 上限后，「长任务被 token 掐死」的风险改由**墙钟**兜底
（48 h）。代价：极端情况下的花费无上界 —— 这是「不设限」的必然结果，由用户自担。

**stall = 「剩余 todo 集合连续多轮不变」**（stuck / 无进展）：

判官只回答「核对通过没有」，不回答「你有没有在原地打转」。
后者用**清单本身**判定 —— 这是本期从 ADR-0024 接过来的机制，比上版的「哈希命令输出」更稳：

```
stall_digest = DefaultHasher( Σ(归一化内容 ‖ 状态) for 非终态项 )
```

| 为什么用清单而非命令输出 | 说明 |
|---|---|
| 天然解决「输出微变漏检」 | 上版靠哈希命令输出，输出差一个字符就漏检。清单是**离散集合**，不受日志噪声影响 |
| 与推进判据同源 | 判据是 `has_open_items()`，停滞就是「这些项一字未变」 |
| 零额外采集成本 | 不需要从 session 里抽工具调用、不需要归一化参数/输出 |
| 不读助手自述 | 清单是结构化数据，不可能被模型换个措辞绕过 |

**归一化链**（参照 CodeWhale `gap_fingerprint`）：
折叠空白 → 小写 → 排序 → 去重 → 喂 `DefaultHasher`。

- 剩余集合不变 ⇒ `stall += 1`；变了 ⇒ 归零。
- **只算非终态项**：已完成的项不参与（否则模型把已完成项重新描述一遍就重置了 stall）。
- 阈值**连续 3 轮**（`stall_limit`）。
- **与 ADR-0024 决策 d 同一判据**：那里的「本轮 `revision` 未变」指的就是这条集合指纹，不是 store 的内部版本号。
- **和「模型自称收尾」取 AND**（ADR-0024 决策 d）：只靠集合不变会误杀
  「一轮里干了活但还没更新状态」的情形；加上「模型也说自己干完了」才判定为真打转。

**用 `DefaultHasher` 而不是 `sha256`**：

- 本机制只比**相邻两轮**（O(1)，不存历史、不落盘、不跨进程）⇒ 不需要密码学强度，
  只需要「同输入同输出、异输入大概率异输出」。
- 实测（单轮 21 KB 证据）：`sha256` 36.3 µs / `DefaultHasher` 3.4 µs —— 快 10 倍。
- `sha256` 会新增 **9 个依赖包**（`sha2` → `cfg-if` / `cpufeatures` / `libc` / `digest` /
  `block-buffer` / `generic-array` / `typenum` / `crypto-common`），而 workspace 的
  `Cargo.lock` 里目前没有它。`ys-core` 现在只依赖 `serde` / `serde_json` / `thiserror`。
- `DefaultHasher` 的实现**不保证跨 Rust 版本稳定** —— 本机制不落盘、不跨进程，**不需要**这个保证。

摘要的输入是**中间结构**（`TodoItem` 的归一化内容 + 状态字符串），
不是 `Message` / `ContentBlock`。

**例**：修一个不生效的配置 bug 时，agent 可能连着几轮都在改同一行、跑同一条命令、拿到同一个失败输出，
但 todo 清单**一字未变** ⇒ `stall` 累加 ⇒ 判定「原地打转」⇒ 停下来告诉人「卡住了，别再烧钱了」。

**阈值取 3 轮而非 2 轮**：修 bug 常见的「改 → 测 → 再改 → 再测」需要至少两轮往返，
2 轮会把**正常的多轮迭代**误判成卡死。3 轮是「至少白转一圈才认定打转」的保守取法。

**清单法天然免疫「输出微变」**：上版用哈希命令输出，反复跑同一条测试命令时
每次输出带不同耗时（`3 failed (2.1s)` / `(2.3s)`）⇒ 摘要不同 ⇒ stall 归零 ⇒ **漏检**。
改用 todo 集合后这个缺口**不再存在** —— 清单是离散集合，不受日志噪声影响。
这是采纳清单层的一个额外收益，不是巧合：**判据与停滞信号同源就不会互相打架**。

**stall 与判官理由的关系**：两者正交 —— 判官说「还没达到」（事实），stall 说「也没进展」（趋势）。
清单变了但判官仍说 `NOT_MET` ⇒ 有进展，stall 归零；清单完全没变 ⇒ 才累加。

**闸门可配置**（`config.json` 的 `goal` 段），默认值宽松、用户按需收紧：

```json
{
  "goal": {
    "max_rounds": 60,
    "wall_clock_secs": 172800,
    "stall_limit": 3,
    "judge_unreliable_limit": 2
  }
}
```

`max_rounds = 0` ⇒ 不提醒（沿用仓库 `0` = 关闭的惯例，见 `config.rs:404` 对
`soft_round_advice_at` 的处理）。`wall_clock_secs = 0` ⇒ 不设墙钟闸门。

### 5.9 两条入口都要覆盖

ADR-0041 的 Consequences 明写：重新引入机制时「应先解决本案暴露的真问题：
**它不该只在一条入口上生效**」。

**事实**：一期的追问只在 TUI 生效 —— `-p`/`--json` 的 `run_single_turn`（`apps/coding-agent/src/main.rs:2059`）
**没有 Request 通道**，追问判据不在那条路径上。

⇒ 三期的求值函数**只有一份**（`goal::evaluate` / `goal::after_turn`），**两条入口都调用它**：

| 入口 | 落点 |
|---|---|
| TUI | `app_loop.rs` 的 `Request::Prompt` 臂内，回合结束后求值；未满足则**追加一轮** |
| `-p` / `--json` | `run_single_turn` **外**的循环，**调用同一个** `goal::after_turn` |

**继续条件只有一份，两个入口共用**：

```text
continue = todo.has_open_items()          // 清单层（ADR-0024，无开关）
         ∨ (goal.is_some() ∧ ¬goal.met)   // 验收层（ADR-0072，需显式 --goal / /goal）
         ∨ should_follow_up(head)         // 一期 HEAD 判据（仅 TUI，follow_up_on_commit 开关）
```

⇒ **`-p` 的默认行为在实践中不变**：模型不写清单、用户不给 `--goal` ⇒ 三项均假 ⇒ 跑完即退出。
⚠️ **但「逐字节不变」不成立** —— 加 `todo` 工具本身会改工具表（W5 的**预期内**前缀漂移，
所有入口同步受影响，与是否传 `--goal` 无关）。本句原文写「逐字节不变」是错的。

退出码：`0` = 达成（或未指定目标且清单已空）；`2` = 未达成（预算 / 无进展 / 阻塞 / 判官不可用）。

### 5.10 三期的文件落点

| 文件 | 动作 | 内容 |
|---|---|---|
| `adapters/tools-todo/` | **新增 crate** | `TodoItem` / `TodoStatus` / `TodoStore`（唯一真值 + 落盘）/ `render_block` / `TodoTool`（ADR-0024 §7.1） |
| `crates/ys-protocol/src/goal.rs` | 新增 | `GoalSpec` / `Verdict` / `ItemVerdict`（纯数据、**无 tokio**） |
| `crates/ys-protocol/src/request.rs` | 改 | `Request::Goal { objective }` / `Request::GoalOff` |
| `crates/ys-tui-coding/src/commands.rs` | 改 | `parse_goal`（紧邻 `parse_loop`） |
| `apps/coding-agent/src/goal.rs` | 新增 | `GoalState` / `Evidence` / `EvidenceBuilder` / `evaluate` / 闸门 / `after_turn` / stall 指纹 |
| `apps/coding-agent/src/goal_judge.rs` | 新增 | 判官提示词、请求构造、`VERDICT`/`ITEMS` 解析、引用校验、usage 累计 |
| `apps/coding-agent/src/toolset.rs` | 不改 | `EXPECTED_ORDER` 与 `vec!` **都不动** —— `todo` 不进子表 |
| `apps/coding-agent/src/assembly.rs` | 改 | 父表追加段加 `todo`（**末位**，在 `subagent_send` 之后）；父表 `assert_eq!(len, 15)` |
| `apps/coding-agent/src/app_loop.rs` | 改 | `Request::Goal` 处理；回合边界注入；回合结束求值；抑制一期/二期自驱 |
| `apps/coding-agent/src/main.rs` | 改 | `--goal` 等旗标；`-p`/`--json` 的目标循环；退出码 |

**依赖方向检查**：`adapters/tools-todo` 与 `tools-basic` / `tools-web` 同形（项目既有的工具落点惯例），
依赖 `ys-tool` + `ys-core` ✅；`ys-protocol` 新增的是纯数据（无 tokio ✅）；
`ys-tui-coding` 只依赖 `ys-protocol`（✅ 已有）；
判官代码放 **app 层** —— 它需要 `Wiring` 的 model + session 视图，且不是通用运行时能力
（无第二个消费者 ⇒ 按 ADR-0014 不进 `ys-*`）。

**核心 crate 零改动**：`ys-core` / `ys-event` / `ys-loop` / `ys-component` / `ys-runtime` / `ys-session` 均不改（ADR-0024 §7.1 同结论）。

### 5.11 三期的非目标

- 不做通用 hook / 插件系统（ADR-0014）。
- 不做多个并行目标 / 目标树 / 子目标分解。
- 不做**默认开启**的完工复核：目标必须由用户显式给出（`/goal` / `--goal`）。
  ADR-0041 的教训被吸收为「**机制在两条入口上都实现了**」，而不是「每轮都强制复核」。
- 不做跨会话的**目标**恢复（目标本身不落盘、`--resume` 不恢复）。
  ⚠️ **todo 清单是另一回事 —— 它落盘**（§5.4.1）：目标是「本回合想达到什么」，
  清单是「还剩什么活」。前者过期就失效，后者重启后仍然有效。
- 不让判官调工具、不扩权、不改写目标。
- 不让判官产生「完成声明」式的强指令（它的输出永远不构造给模型的指令）。
- 不把「助手自述」纳入证据池。
- 不改 `ys-event` 词汇、不新增 `AgentEvent` 变体。
- 不把判官能力放进 `ys-runtime` 的公共 API。
- **不让子 agent 有 todo**（同时打破字节前缀与单写者不变量，理由见 `docs/arch/long-horizon-todo/design.md` §7.4 e）。

### 5.12 最小可交付切片

本期分两个切片，**顺序不可换**（先有清单，判官才有核对对象）：

**切片 1：todo 清单 + 推进闭环**（= 实现 ADR-0024）

`adapters/tools-todo` crate；`TodoStore` 落盘 `<stem>.todo.json`；`apply` 两条硬规则；
回合边界注入（`should_inject`）；`decide` 五道护栏；**父表追加段末位加 `todo`**
（`toolset.rs` 的 `EXPECTED_ORDER` 不动 —— 它描述子表）；子表无 `todo`；侧栏渲染。

**切片 2：验收层 + 两入口**

`GoalSpec`/`Verdict` + `Request::Goal`；判官调用 + `VERDICT`/`ITEMS` 解析 + **逐条引用校验**；
`/goal` + `--goal`；四类硬闸门 + 轮数软提醒 + 可靠性计数；stall 指纹；
两条入口都跑；标签行 harness 消息；抑制一期/二期自驱。

**切片 2 明确不做**：跨轮证据、滚动摘要、目标状态落盘、`--resume` 恢复目标、多目标。

**验收标准**（两切片合计）：

- **清单层**：`apply([])` 丢弃非终态项 ⇒ 拒绝并返回错误。
- **清单层**：全部终态 ⇒ 落盘空表 + 删文件。
- **清单层**：压缩把注入块吃掉 ⇒ 下回合自动补回（`should_inject` 自愈）。
- **清单层**：`/new` ⇒ 新文件且计划为空。
- **清单层**：重启 ⇒ 计划随会话恢复（落盘生效）。
- **验收层**：mock 第 1 轮 `NOT_MET`、第 2 轮 `MET` ⇒ 回合数 = 2、退出码 0。
- **验收层**：mock 返回 `MET` 但引用不存在于证据 ⇒ 降级 `NOT_MET` + 计入可靠性计数。
- **验收层**：mock 返回 `MET` 但**某一项**标 `MISSING` ⇒ 降级 `NOT_MET`（逐条核对生效）。
- **验收层**：mock 连续两次格式错误 ⇒ 目标停止且**不再起回合**。
- **验收层**：mock 调用失败 ⇒ 重试 1 次后停止。
- **验收层**：墙钟闸门触发 ⇒ 停止且退出码 2。
- **验收层**：轮数达阈值 ⇒ **只提醒、不中断**（与 `soft_round_advice_at` 同形）。
- **验收层**：todo 集合连续 3 轮相同 **且** 模型自称收尾 ⇒ 判定无进展并停。
- **验收层**：todo 集合变了 ⇒ stall 归零（即使判官仍说 `NOT_MET`）。
- **验收层**：**TUI 与 `-p` 跑同一条用例，断言行为一致**（ADR-0041 的回归防线）。
- **验收层**：`--resume` 后目标未激活（但 todo 清单恢复）。

**变异测试**（仓库纪律「关键逻辑要做变异测试」）：对两条链注入 bug ——
把 `NOT_MET` 解析成 `MET`、闸门恒真、stall 不计数、引用校验恒真、`apply` 允许丢弃非终态项
⇒ 期望用例变红。建议落 `scripts/goal-mutation.py`（形态对齐 `scripts/cancellation-mutation.py`）。

## 6. 非目标

| 不做 | 理由 |
|---|---|
| 通用 hook 系统 | ADR-0014；只有一个真实消费者 |
| 跨回合输入队列（ADR-0047 的 `Delivery` 协议格） | 三种真实用途都不需要；成本高一个数量级 |
| 落盘的 automation 子系统 | 本仓 `jsonl` 已落盘 + `--resume` 可接，再建一套是重复 |
| 抖动 / 过期 / 任务数上限 | 那是多用户云端问题，本地单用户不成立 |
| 热插拔判据 | 不设计假想未来需求；升级路径见 §7 |

## 7. 升级路径（今天就一条）

「一个函数一个 seam」：今天 `should_follow_up` 是**一个纯函数**，不引 trait、不引 registry、
不引 `HashMap`。将来若要「可插」（多点、多来源），把这**一个函数**升级成列表 ——
那是加法，不是重写。**今天不预埋任何抽象。**

## 8. 测试矩阵

> ⚠️ **已撤销（ADR-0073）**：涉及定时器判据的验收条目作废 —— 本表 **#11 / #13 / #14 / #16b**，
> 以及 §8.1 首两行（`last_head` 初值、基准取在回合之后）与 §8.2 第 2 行
> （`loop_rejected_in_non_git_workspace`）。现行语义：**到点即起回合**，因此
> - 「有活 ⇒ 追加 / 没活 ⇒ 不动」不再有两个分支，只剩「到点 ⇒ 追加」一条；
> - 「零 token 的变异测试点」不再存在（无判据可变异）；
> - 非 git 工作区不再拒绝登记，该用例应改为「非 git 工作区也能正常起回合」；
> - `advance()` 与忙转（本表 #12 / #14 里与 `advance` 相关的部分）仍然必测。
>
> 以下表格保留原样作为历史设计记录。

| # | 用例 | 层 | 阶段 |
|---|---|---|---|
| 1 | HEAD 变了 + `Completed` ⇒ 追加一轮 | 判据 | 一期 |
| 2 | HEAD 没变 ⇒ 不追加 | 判据 | 一期 |
| 3 | 非 git 仓库 ⇒ 静默跳过，不报错 | 判据 | 一期 |
| 4 | `StopReason::Cancelled` ⇒ 不追加 | 判据 | 一期 |
| 5 | `StopReason::{Truncated, MaxRounds, NoOutput, Failed}` ⇒ 不追加 | 判据 | 一期 |
| 6 | `Err` ⇒ 不追加 | 判据 | 一期 |
| 7 | 追问回合里 HEAD 又变了 ⇒ **仍只两轮**（防套娃） | 结构 | 一期 |
| 8 | 开关关闭 ⇒ 不追加 | 配置 | 一期 |
| 9 | 两次回合后 `stats` / `turns` 是累加值（`emit_view` 正确） | 集成 | 一期 |
| 10 | 追问输入的角色是 `Role::Harness` | 集成 | 一期 |
| 11 | 定时到点 + 有活 ⇒ 追加；没活 ⇒ 不动 | 唤醒 | 二期 |
| 12 | 定时器与用户输入同时到达 ⇒ 不丢、不死锁 | 并发 | 二期 |
| 13 | `has_work` 为假 ⇒ **不 append、不调模型、不增 session**（零 token 的变异测试点） | 判据 | 二期 |
| 14 | `has_work` 为假 ⇒ 定时器被推远，不忙转（C3） | 并发 | 二期 |
| 15 | `/loop 5m x` 再敲一次 ⇒ **覆盖**，不叠加（单条语义） | 命令 | 二期 |
| 16 | `/loop off` ⇒ 定时器清空；**已在跑的回合不中断**；没在跑时**零输出** | 命令 | 二期 |
| 16a | `/clear` ⇒ 循环作废 | 命令 | 二期 |
| 16b | 非 git 工作区 ⇒ `/loop` 拒绝登记并说明原因 | 命令 | 二期 |
| 16c | 多字节间隔（`5秒`）⇒ 本地提示，**不得 panic** | 命令 | 二期 |
| 17 | 状态行显示 `[loop] <间隔> · <还有多久>` / `正在跑`（可见性） | 集成 | 二期 |
| 18 | `/resume` 后不自动续跑，且**运行中换会话**时显式提示 | 集成 | 二期 |
| 19 | `apply([])` 丢弃非终态项 ⇒ 拒绝并返回错误 | 清单 | 三期 |
| 19a | 全部终态 ⇒ 落盘空表 + 删文件 | 清单 | 三期 |
| 19b | 压缩吃掉注入块 ⇒ 下回合自动补回 | 清单 | 三期 |
| 19c | `/new` ⇒ 新文件且计划为空；重启 ⇒ 清单随会话恢复 | 清单 | 三期 |
| 19d | 子 registry 无 `todo`；子表是父表字节前缀 | 结构 | 三期 |
| 20 | todo 集合连续 3 轮相同 **且** 模型自称收尾 ⇒ 判定无进展并停 | 闸门 | 三期 |
| 20a | todo 集合变了 ⇒ stall 归零（即使判官仍说 `NOT_MET`） | 闸门 | 三期 |
| 20b | 已完成的项不参与 stall 指纹（重描述不重置 stall） | 闸门 | 三期 |
| 21 | 判官输出 `MET` 但引用未命中证据窗口 ⇒ 降级 `NOT_MET` | 判据 | 三期 |
| 21a | 判官输出 `MET` 但**某一项**标 `MISSING` ⇒ 降级 `NOT_MET` | 判据 | 三期 |
| 22 | 判官输出格式不对 ⇒ `NOT_MET` + 计入可靠性；连续 N 次 ⇒ 停 | 判据 | 三期 |
| 23 | 判官调用失败 ⇒ 重试一次后停（不放行继续） | 判据 | 三期 |
| 24 | 助手自述**不在**证据窗口 ⇒ 模型自称完成无法通关 | 判据 | 三期 |
| 24a | 模型把全部项标 `completed` 但无凭据 ⇒ **不得放行**（ADR-0024 天花板） | 判据 | 三期 |
| 25 | 墙钟闸门触发 ⇒ 停 | 闸门 | 三期 |
| 25b | 轮数达阈值 ⇒ **只提醒不中断**（与 `soft_round_advice_at` 同形） | 闸门 | 三期 |
| 26 | 主回合 `Failed`/`Cancelled`/`MaxRounds`/`Truncated`/`NoOutput` ⇒ 立即停 | 闸门 | 三期 |
| 27 | 目标激活期间一期追问与二期 tick 不追加回合 | 互斥 | 三期 |
| 28 | **TUI 与 `-p` 跑同一条用例，行为一致** | 集成 | 三期 |
| 29 | `--resume` 后目标未激活，但 todo 清单恢复 | 集成 | 三期 |

### 8.1 实现注记（写在这里的原因：三条都是实现期才暴露的取舍）

| 取舍 | 决定 | 依据 |
|---|---|---|
| `last_head` 的初值 | **`/loop` 创建时刻的 `HEAD`**，不是 `None` | 初值为 `None` ⇒「首次醒来必判有活」⇒ 设完就走开的人白花一次模型调用。设计只写了「与上次起完回合时不同」，没写首次怎么办 |
| 基准取在**回合之后** | `TimerFired` 起完回合**才** `last_head = git_head()`，不是起之前 | 取之前 ⇒ 本回合自己提交的代码在下一周期被当成「外部有新变化」⇒ 空闲循环每个间隔白跑一次，退化成 §4.2 明令禁止的「到点就跑」 |
| 状态行用**相对时长** | `[loop] 5m · 14 分后`，不是 `next 14:32` | `Instant → 本地钟点` 要时区偏移，而 `ys-tui-coding::clock::hhmm_at` 是 `pub(crate)`、app 侧拿不到。冒充 UTC 钟点比不显示更糟（用户按错的时间等） |

**变异测试实测**（`apps/coding-agent/src/app_loop.rs` 的 `loop_*` 用例，8 个变异体全部被抓）：

| 变异 | 抓它的用例 |
|---|---|
| 判据恒真（到点就跑） | `loop_timer_tick_without_changes_costs_nothing` + `loop_tick_does_not_busy_spin` |
| 缺 `advance()`（忙转） | `loop_tick_does_not_busy_spin`（读状态行的「0 秒后」） |
| `/loop` 不取初始基线 | `loop_timer_tick_without_changes_costs_nothing` |
| `last_head` 取在回合之前 | `loop_tick_does_not_busy_spin` |
| `/loop off` 不清状态 | `loop_off_stops_further_ticks` |
| `/new` 不清循环 | `new_session_clears_loop` |
| `loop_placeholder` 改 `Role::User` | `loop_placeholder_is_harness_role` + `loop_timer_fires_one_turn_when_head_moved` |
| `/loop` 覆盖变叠加 | `loop_overwrites_instead_of_stacking` |

**忙转不可用「模型调用数」检测**：判据为假时本臂 `continue`、一个 token 都不花。
它的唯一可观测面是**状态行** —— 缺 `advance()` 时 `next_at` 停在过去，
`relative_label` 恒为 `0 秒后`。这是 `loop_tick_does_not_busy_spin` 后半段的由来。

**视图刷新只有一个位置**：`loop_state` 必须在**循环尾部那次统一 `emit_view`** 里带上。
曾分别在各请求臂里补刷，结果是每个 `/loop` 产生两个 `View` —— UI 白重绘一帧，
且连测试都会把第一条命令的第二个 View 误认为第二条命令的结果。

### 8.2 评审发现（已修，记录防止回退）

| # | 缺陷 | 根因 | 守卫用例 |
|---|---|---|---|
| 1 | `/loop 5秒` 让整个 TUI **panic** | `spec.split_at(len - 1)` 按**字节**切，落在多字节字符内部。而该路径的契约是「非法输入只本地提示」 | `test_parse_interval_rejects_out_of_range_and_malformed`（含 `5秒` / `5分` / `五分钟`） |
| 2 | 非 git 工作区下 `/loop` 静默永不生效，状态行却一直显示在跑 | `should_tick` 恒假 ⇒ 循环登记成功但永不触发。**主动误导**比报错更坑 | `loop_rejected_in_non_git_workspace` |
| 3 | 没在跑时 `/loop off` 发一条**空 `Output`** ⇒ transcript 落一条空白 System 行 | `lines.push("")` 让 `Vec` 非空 ⇒ 尾部 `if !lines.is_empty()` 通过 | `loop_off_without_loop_is_silent`（断言**任何** `Output` 都不许有，而非「不含 Loop stopped」） |
| 4 | `/clear` / `/resume` 两条清零点零测试 | 只测了 `/new` | `clear_session_clears_loop` / `resume_to_other_session_stops_loop_with_notice` |
| 5 | `/resume` 到**同一**会话也报 `session changed` 并丢掉循环 | 只看 `resumed.is_some()`，没比对前后路径 | `resume_to_same_session_keeps_loop` |
| 6 | 8 条新用例绕过 `env_lock` 且真往 `~/.yushan/logs` 写文件 | 用 `spawn_app_loop_prelocked` 却没持锁（该参数的契约就是「调用方已持锁」） | `LoopEnv`（持锁 + 隔离 `YUSHAN_LOG_DIR` + 关 debug） |
| 7 | `/loop` 不在 `COMMANDS` 表 ⇒ 补全浮层 / `/help` / 未知命令提示都看不到它 | 只加了分派分支 | 表内条目 + `test_every_command_maps_to_the_designed_action` |
| 8 | `--help` 手写清单与 `COMMANDS` 漂移（`/clear` 描述、`/compact` 措辞、`/loop` 缺 `off`） | 两份独立文案，对账只比命令名 | 命令段改为生成 + `help_text_embeds_command_list_verbatim` |

**`--help` 的命令段改为生成**（顺带修掉一类同源偏离）：`apps/coding-agent/src/help.rs`
曾手写一份斜杠命令清单，与 `COMMANDS` 是**两份独立文案**。对账测试只比命令名，
于是 `/clear` 的描述、`/compact` 的措辞、`/loop` 的 `arg_hint`（少了个 `off`）三处
已经静默漂移。现在该段由 `ys_tui_coding::command_list_text()` 从 `COMMANDS` 生成，
`--help` 与 TUI 内 `/help` 同源；静态三段（用法 / 公共 flag / 环境变量 / 示例）
留在常量里 —— 它们没有运行时事实源可查。

对齐口径：按**显示宽度**（CJK = 2 列，借渲染层同一个 `display_width`），并给对齐列
**封顶 28 列**。不封顶时 `/effort` 那个 47 列的枚举提示会把其余 17 行的描述推到
50 列外 —— 表看着齐，读起来是「逐行找描述」。

**`env_lock` 的粒度**：只圈住 spawn（`Config::from_env` / `resolve_log_dir` /
`debug_enabled` 都在那时读环境变量），不留到用例结束 —— 后者会让 8 条用例整段
串行，实测 2.6s → 9s 而收益为零。注意它是**不可重入**的 `std::sync::Mutex`：
`env_prelocked = true` 的调用方自己持锁，`spawn_app_loop_full` 内**不得**再加。

## 9. 固有限制（不是缺陷，不要当 bug 修）

| 限制 | 原因 |
|---|---|
| 追问只在「用户刚说完话」之后触发 | 唤醒层是回合边界；要覆盖「用户没说话」得等二期定时 |
| `-p` / `--json` 默认不追问 | 不写清单、不传 `--goal` ⇒ 跑完即退出（与一期一致）；写清单或传 `--goal` 才循环。见 §5.9 |
| 追问预算与主回合**独立** | `deadline` 在 `run_turn_chain` 入口算（`:1322-1323`）；默认 `max_turn_duration` 是 48h（`config.rs:431`）⇒ 实际不构成限制 |
