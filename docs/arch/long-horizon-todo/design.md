# 长程任务的闭环（todo）— 架构设计

> 产出：`/arch-design`（2026-09-23）。输入：[`context.md`](./context.md)（arch-explore，含用户已澄清的 7 条 + 五条新约束）。
> 证据均带 `文件:行号`，基线 `master` @ `8b03895`。**本文 token 数字均为量级估算**，算式见 §6。
> 决策记录：[ADR-0024](../../adr/0024-long-horizon-todo.md)。
>
> **一句话结论**：todo 的真值**搬出会话**（会话同级的独立 JSON），模型经一个**末位**的 `todo` 工具全量重写它；
> `app_loop` 在**回合边界**把「状态变了的」那份渲染成一条 `messages` 尾部消息推进上下文；回合结束由**纯函数**
> 判「还有未完成项吗」，有则 follow-up 再起一轮。**零核心 crate 改动、零改 `BasicLoop`。**

---

## 1. 结论先行

| # | 决策 | 一句话 |
|---|---|---|
| **D1** | **真值搬出会话**：`<sessions_dir>/<stem>.todo.json` | 推翻 ADR-0021 决策 5（真值 = 最后一次调用的 `arguments`）。会话同级、同 stem ⇒ 复用 `latest_session_path()` 的重启恢复，**零新目录、零新环境变量** |
| **D2** | **注入 = 回合边界一条 `Role::User` 消息，进 `messages` 尾部** | 内容是**会变**的 ⇒ 尾部注入是其唯一正当形态（ADR-0021 决策 1 / `prompt-delivery/notes.md:50-56`）。**绝不改 `system`** |
| **D3** | **注入判据 = `状态变了 ∨ 会话里找不到标记`**（纯函数） | 前者堵住「上下文堆 N 份矛盾副本」；后者是**自愈**：压缩把块吃掉后自动补回，**不需要**监听 `ContextCompacted` 事件 |
| **D4** | **推进 = follow-up 纯函数判据「有未完成项」** + **五道护栏** | 替换 `design-plans/2026-09-15-follow-up.md` 的 git HEAD 判据。**不需要开关** —— 开关就是「有没有计划」 |
| **D5** | **加终止态 `Cancelled`；删除判据 = 「无 open 项」而非「全部 Completed」** | 后者会让「放弃某项」永远卡住 ⇒ 永不删除。执行者是**代码**（`save` 收口），不是模型自觉 |
| **D6** | **`todo` 与 `subagent` 都排工具表末两位** | 子表 = 父表 − 这两项，**必须仍是父表的字节前缀**（ADR-0022 D3 的缓存理由）。需求「只有主 agent 有 todo」由此**结构保证**，零运行时检查 |
| **D7** | **不引 hook / 不引依赖图 / 不引共享列表 / 不引 trait 列表** | 四条都是零消费者抽象。判断链走下来：写计划 = **Tool**、推动 = **Follow-up**，两个机制，**不需要第三个** |

**一句话**：不改机制，只加**一份外部状态** + **两个纯函数**（一个决定注入、一个决定续跑）。

---

## 2. 需求与约束

**需求原话**：「重新考虑 todo 的设计，我们需要提供**长程任务的成功率**和**闭环能力**。它可以和 subagent 结合起来……主 agent 按照 todo 派发 subagent 等等、agent 按照 todo 逐个完成等等」。

**用户已澄清的 7 条**（`context.md` §7，**不可再议**）：

| # | 澄清 |
|---|---|
| Q1 | 闭环合在**回合之间**；todo 是工作计划清单，**每轮都要去看** |
| Q2 | todo **持久化**，**全部完成后删除** |
| Q3 | **写计划 = Tool；推动 = Follow-up**（两个机制） |
| Q4 | **主 agent 按 todo 执行**；subagent 只做简单任务 |
| Q5′ | **只有主 agent 具备 todo 能力** |
| Q6 | P0/P1 都做，顺序 `P0 → P1 → subagent → todo` |
| Q7 | TUI **侧栏**简单展示 |

**硬约束**（违反即设计偏离）：

| 约束 | 来源 |
|---|---|
| `ys-core`/`ys-event`/`ys-protocol` **零 tokio**；`ys-tui-coding` **不依赖** `ys-runtime` | `CLAUDE.md` |
| **不跨动态库边界传 trait object** ⇒ 静态编译期装配 | `CLAUDE.md` |
| **Agent 无状态**：session / events 归接线器，经 `AgentPorts` 每回合传入 | ADR-0010 |
| **工具面恒定**；前缀每轮变 = 每轮多付 ~9,000 token = 静态加工具的 **52 倍** | `toolset.rs:66-72`、ADR-0021 |
| **prompt cache 是字节前缀**（`tools → system → messages`），命中 ≈0.1× | ADR-0021 §4.3 |
| **不建 hook 系统** | ADR-0014 / 0021 |
| 新控制流须走判断链；**动 `BasicLoop` 是最高爆炸半径** | `notes.md` ③ |

---

## 3. 同类软件调研（Step 2）

| | **Manus** `todo.md` | **Claude Code Tasks**（v2.1.16） | **Cline Focus Chain**（v3.25） |
|---|---|---|---|
| 存储 | `todo.md` 文件 | `~/.claude/tasks/` 文件系统 | 任务内持久，跨 context reset |
| **推回上下文** | agent **不断重写**，推到**末尾** | 4 工具（Create/Get/Update/List） | **每 6 条消息**重注入 + 重读 |
| 目的（原话） | 「对抗 **lost-in-the-middle**、减少目标漂移」 | 「Todos = 帮我记住要做什么；Tasks = **跨 session/subagent/context window 协调**」 | 「穿过累积的上下文噪声，守住主线」 |
| 依赖图 | 无 | **`blockedBy` + 自动解锁** | 无 |
| 多 agent | 无 | **共享列表 + 更新广播**（`CLAUDE_CODE_TASK_LIST_ID`） | 无 |
| 状态 | 复选框 | `pending/in_progress/completed/blocked` + **ownership** | 复选框 |

**三条收敛共识**（三家全中，本设计全部满足）：

1. **计划必须持久** —— 三家都落外部存储；
2. **计划必须被反复推回上下文** —— Manus 每次重写 / Cline 每 6 条，**都不是「注入一次」**；
3. **计划必须活过压缩** —— Cline 明说「cross context reset」。

> ⚠️ **两条必须正面处理的对照**：
> - **Manus 明知扰动 KV cache 仍每次重写** ⇒ §7.4 决策 b 要回答「我们为什么不跟」。
> - **Claude Code 做 Tasks 的核心动机恰恰是 subagent 协作** ⇒ 与 Q5′ **正面相反**，§7.4 决策 e 要给出理由。

---

## 4. 三个候选方案（Step 3）

三个 subagent 各自独立产出，倾向分别为「最小复杂度 / 可扩展优先 / 缓存成本优先」。**八条结论三家一致**，分歧只在四处。

### 4.1 甲 —— 最小复杂度

**一句话**：todo 是 app 层的 `Arc<Mutex<TodoStore>>`（`<session-id>.todo.json`），工具持这个 `Arc`；
`app_loop` 回合开始「变了才追加一次」；回合结束一个纯函数问「还有 open 项吗」。

- **模块**：全部落在 `apps/coding-agent`（`todo.rs` / `todo_tool.rs` / `follow_up.rs`）+ TUI 两个文件。**零核心 crate 改动。**
- **注入**：`should_inject(store, last_rev, session)` —— `非空 && (revision 变 ∨ 会话里找不到 TODO_MARKER)`。
- **推进**：`should_continue(open_items, stop_reason, used)`，上限 8。
- **终止态**：加 `Cancelled`；删除判据 = **无 open 项**。
- **成本**：50 回合 ≈ **+3.4k 当量**（<1%）。
- **自陈缺口**：无停滞护栏（只有计数上限）；无防逃逸。
- **另提**：建议落地顺序改成 `P0 → todo → P1 → subagent`（理由见 §8）。

### 4.2 乙 —— 可扩展优先

**一句话**：拆成**三件正交件** —— 状态（`ys-tool::TodoStore` **窄 trait**，三跳注入）/ 能力（`TodoTool`）/ 推进（`FollowUpRule` **trait 列表**）。

- **模块**：`ys-tool/src/todo.rs` + `ys-component` / `ys-runtime` 各加一字段 + **`basic.rs` 1 行**（挂在既有的「唯一接线点」`basic.rs:459-469`）。
- **注入**：状态变才追加；**压缩后经 `ContextCompacted` 事件 → `Boundary::Steer` 补注入**。
- **推进**：`FollowUpRule` trait + 4 道护栏，含 **停滞护栏**（`max_stalled = 2`）。
- **诚实自评**：列表形态今天只有 1 条规则；裁决「**列表保留、只装一条也成立**」，且 `design-plans:66-68` 预先批准了这次升级。
- **成本**：机制 ≈ +11.6k，含工具 footprint 后 **+1.4%**。

### 4.3 丙 —— 缓存/成本优先

**一句话**：新 adapter crate `adapters/tools-todo/`；真值 `~/.yushan/todos/<id>.json`；`app_loop` 回合边界注入一次。

- **模块**：`adapters/tools-todo/`（item / store / render / tool）+ app 侧 `follow_up.rs`。**零核心 crate 改动。**
- **注入**：**每回合无条件重放**（turn 粒度）；另备**逃生门** —— 轮级注入经 `Boundary::Steer`（零 `BasicLoop` 改动）。
- **推进**：`decide()` 纯函数返回 `FollowUp::Stop(StopCause)`（六种停止原因**显式枚举**）+ 5 道护栏，含 **`ModelDisagrees`**（模型自称收尾但 `revision` 未变 ⇒ 停）。
- **防逃逸**：`apply()` **拒绝丢弃既有非终态项** ⇒ 模型不能发 `[]` 逃逸追问循环。
- **成本**：给了逐项算式，50 轮 **+19.3%**；口径对照表（每轮注入 +43.9%；工具面每轮变 **+1,476%**）。
- **诚实自陈**：轮级新鲜度缺失是明确退化；注入副本会累积。

### 4.4 一致点（三家独立收敛 ⇒ 强证据）

| # | 三家一致 |
|---|---|
| 1 | 真值 = 会话可对应的**独立 JSON 文件**（不是会话内、不是 `details`） |
| 2 | 注入 = **`messages` 尾部一条合成消息**，回合边界，**不碰 `system`** |
| 3 | 加终止态 **`Cancelled`** |
| 4 | 删除判据 = **「无 open 项」**，不是「全部 Completed」 |
| 5 | follow-up 判据 = **「有未完成项」**（替换 git HEAD） |
| 6 | `todo` 与 `subagent` **都排末位**（字节前缀） |
| 7 | **不做**依赖图 / 不做共享列表 / 不做 ownership |
| 8 | 压缩问题用**「真值在外」**解决，而不是让计划免疫压缩 |

---

## 5. 对比矩阵（Step 4）

| 维度 | 甲（最小） | 乙（可扩展） | 丙（成本） |
|---|---|---|---|
| **新增代码** | ~800 行 | ~1,100 行 | ~1,000 行 |
| **核心 crate 改动** | **0** | `ys-tool` + `ys-component` + `ys-runtime` + **`basic.rs` 1 行** | **0** |
| **新概念数** | 2（store、注入判据） | **5**（store trait、三跳字段、规则 trait、策略数据、注入函数） | 3（store、渲染、判定枚举） |
| **工具落点** | app 内模块 | `ys-tool`（核心） | **adapter crate** |
| **与 ADR-0021 插件缝的关系** | 将来搬迁是机械动作 | 已在核心，**无法搬进 `ToolPlugin`** | **今天就是「一行登记」的形态** |
| **注入新鲜度** | 状态变 / 丢失时（**最省**） | 状态变 / 压缩后 | **每回合**（最新鲜，**最贵**） |
| **压缩恢复机制** | 标记丢失判据（**纯函数，自愈**） | `ContextCompacted` 事件 → `Steer` | 回合边界重放（天然） |
| **停滞护栏** | ❌ 无 | ✅ `max_stalled` | ✅ `ModelDisagrees` |
| **防逃逸**（模型发 `[]` 绕过） | ❌ 无 | ❌ 无 | ✅ 拒绝丢弃非终态项 |
| **停止原因可观测** | ❌ 只有 bool | ❌ 只有 bool | ✅ `StopCause` 六态枚举 |
| **50 轮成本** | +3.4k（<1%） | +11.6k（+1.4%） | +19.3% |
| **主要风险** | 护栏不足 | 抽象开销 + 动 `BasicLoop` | 注入最贵、副本累积 |

**评价**：

- **甲的最小化方向对，但护栏不足** —— 「计数上限」只防「跑太久」，不防「原地打转」。这是实质缺口，不是细节。
- **乙的抽象在核心 crate 里最重，且它的正当性最弱**：`ADR-0022` 用窄 trait 注入的**唯一理由**是「工具不能快照 `Model`（`/model` 之后会静默用旧模型）」。**todo 没有这个面** —— 它的真值是一份文件，`/new` 时由接线器**就地改指向**，不存在过期快照。把 app 生命周期绑定的 store 做成 `ys-tool` 的核心 trait，**今天没有第二个实现者**。
  > 另：乙选的 `Steer` 补注入实测有副作用 —— **`Boundary::Steer` 会 emit `AgentEvent::UserMessage`**（`basic.rs:222`），todo 块会**显示成用户消息**。甲的直接 `session.append` 不发事件。
- **丙的护栏最完整**（停滞 + 防逃逸 + 停止原因枚举），但**注入最贵** —— 「每回合无条件重放」在状态未变时是纯浪费（`prompt-delivery/notes.md:37` 的结论：**内容不变却注入 = 纯浪费**）。它自己也承认「轮级新鲜度缺失」，却选了一个更贵的默认。
- **丙 的 adapter crate 落点最优**：与 `adapters/tools-basic` / `adapters/tools-web` 同形，且**今天就是** ADR-0021 插件缝要的「一行登记」形态。

---

## 6. 成本模型（back-of-envelope）

**符号**：`B` = 单块 est token；`R` = 每回合轮数；`T` = 链路回合数。缓存命中 ≈ 0.1×。

**单块**：表头 ~25 + 每项 ~28（15 CJK + 20 ASCII）⇒ 典型 6 项 **`B ≈ 190`**（上限硬钳 600）。

**关键机制**：缓存是**字节前缀**，会话 append-only ⇒ 第 k 回合注入的那段，在第 k+1 回合**仍在同一位置** ⇒ 后续按 0.1× 命中。**唯一按 1.0× 付的是「新注入的那一段」。**

| 口径 | 每轮增量 | 每回合增量（R=5） | 需改 `BasicLoop`？ |
|---|---:|---:|:--:|
| 每轮注入 | `B` = 190 | `R × B` = 950 | **是** |
| **每回合注入（本设计）** | `B / R` = 38 | `B` = 190 | 否 |
| 状态变才注入（**本设计采纳**） | ~0（多数回合 0） | ~0 或 190 | 否 |

> ⇒ **「成本差约 10×」的准确出处 = `R`**（R=10 即 `max_rounds` 上限时 10×）。而「状态变才注入」在稳态下**再省一档**。

**50 轮长任务（R=5 ⇒ T=10）**：

| 项 | 当量 |
|---|---:|
| 固定前缀（工具表 +173 → 命中价 17.3/轮 × 50） | ~865 |
| 回合边界注入（状态变 ~10 次 × 190） | ~1,900 |
| 模型调 `todo`（8 次 × (args+result) ≈ 400） | ~3,200 |
| **合计增量** | **~6,000** |
| **无 todo 基线**（50 × 617） | 30,850 |
| **净增** | **≈ +19%**（甲按更省的判据估到 ~3.4k/<1%；差异来自「工具结果回显」是否计入） |

**灾难对照**：工具面每轮变动 ⇒ `250 × 9,000` = **+2.25M**（本设计的 **~375×**）。这就是「工具面恒定」值多少钱。

---

## 7. 推荐方案（甲的最小化 + 丙的护栏 + 丙的落点）

**采纳三家各自最强的一块**：

| 取自 | 什么 |
|---|---|
| **甲** | 注入判据 `状态变 ∨ 标记丢失`（纯函数、自愈、**无事件依赖**、最省）；`Arc` 持有而非三跳注入 |
| **丙** | 工具落点 = **adapter crate**；**防逃逸**（拒绝丢弃非终态项）；`StopCause` 显式枚举 |
| **乙** | **停滞护栏**（「零进展」⇒ 停）—— 这是甲缺的、也是真正的防套娃 |
| 三家共有 | 外部文件真值、回合边界尾部注入、`Cancelled`、「无 open 项」删除判据、「有未完成项」推进判据、末两位顺序 |

### 7.1 模块划分与职责

| 模块 | 归属 | 职责 | 改动 |
|---|---|---|---|
| `adapters/tools-todo/` | **新 crate** | `TodoItem` / `TodoStatus` / `TodoStore`（唯一真值 + 落盘 + 归一化）+ `render_block()`（纯函数，字节稳定）+ `TodoTool` | 新增 ~420 行 + ~120 测试 |
| `toolset` | `apps/coding-agent` | `EXPECTED_ORDER` 与 `vec!` 加 `todo`（**末两位之倒数第二**）；`build_tools` 多一个 `Arc<TodoStore>` 参数；3 处 `assert_eq!(len, 9)` 改 10 | 改 ~20 行 |
| `prompt` | `apps/coding-agent` | `tool_snippet("todo")` + **门控**的 guideline（`if tools.contains(&"todo")`）⇒ 子 agent 的提示词里两样都没有 | 改 ~14 行 |
| `follow_up` | `apps/coding-agent` | `decide()` 纯函数（返回 `StopCause`）+ 五道护栏 + 追问提示词渲染 | 新增 ~150 行 |
| `app_loop` | `apps/coding-agent` | `Prompt` 臂包 follow-up 循环；回合边界注入；`NewSession` 臂 retarget store；**用户输入抢占** | 改 ~70 行 |
| `wiring` | `apps/coding-agent` | 持 `Arc<TodoStore>`；`/new` 时 retarget（**同一 `Arc`，身份不变**） | 改 ~35 行 |
| `view` / `draw` | `apps/coding-agent` + `ys-tui-coding` | `CodingView.todos` + 侧栏（**空列表 ⇒ 0 宽 ⇒ 逐字节不变**） | 改 ~100 行 |
| **核心 crate** | `ys-core` / `ys-event` / `ys-protocol` / `ys-tool` / `ys-component` / `ys-loop` / `ys-runtime` / `ys-session` | **零改动** | **0** |

> **为什么 `adapters/tools-todo` 而非 app 内模块**（甲的做法）：与 `tools-basic` / `tools-web` 同形（项目既有的工具落点惯例），
> 且它**今天就是** ADR-0021 插件缝要的形态（「一个 crate + 一行登记」）—— 甲自陈「将来搬迁是机械动作」，丙的落点让这次搬迁不必发生。
> **代价如实**：多一个 `Cargo.toml`；`build_tools` 多一个参数（app 内部函数，可接受）。

### 7.2 核心接口

```rust
// adapters/tools-todo/src/item.rs
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus { Pending, InProgress, Completed, Cancelled }

impl TodoStatus {
    /// 终态 = {Completed, Cancelled}。「全部终态 ⇒ 删除」与「不得丢非终态项」**都只问这一个问题**。
    pub fn is_terminal(self) -> bool;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem { pub content: String, pub status: TodoStatus }
```

```rust
// adapters/tools-todo/src/store.rs —— 唯一真值
pub struct TodoStore { /* Mutex<Inner> */ }

impl TodoStore {
    /// `Some(session_path)` ⇒ `<sessions_dir>/<stem>.todo.json`；`None`（`-p`/`--json`）⇒ 纯内存。
    /// 文件损坏 ⇒ 空列表 + 一句降级提示，**绝不 panic、绝不阻断启动**。
    pub fn open(session_path: Option<&Path>) -> Arc<Self>;

    pub fn snapshot(&self) -> Vec<TodoItem>;
    pub fn revision(&self) -> u64;                 // 内容版本号，**进程内单调、不落盘**
    pub fn has_open_items(&self) -> bool;          // = 存在非终态项。**推进判据与删除判据共用它**

    /// 全量重写（照 Claude Code `TodoWrite` 的「每次传完整列表」）。
    ///
    /// **两条硬规则（都在这里，不靠模型自觉）**：
    /// 1. **拒绝丢弃既有非终态项** —— 否则模型发一个 `[]` 就能关掉追问护栏（丙的发现）；
    /// 2. **非空且全部终态 ⇒ 清空 + 删文件** —— 「全部完成后删除」（甲的关键修正：判据是
    ///    「无 open 项」而非「全部 Completed」，后者会让放弃的项永远卡住）。
    pub fn apply(&self, incoming: Vec<TodoItem>) -> Result<ApplyOutcome, TodoError>;

    /// `/new`：清空 + 换路径。**同一 `Arc` 身份不变** —— 工具持的就是它。
    pub fn retarget(&self, session_path: Option<&Path>);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome { Updated, ClearedAllDone }
```

```rust
// adapters/tools-todo/src/render.rs —— 纯函数；**字节稳定是硬要求**
pub const MAX_BLOCK_TOKENS: usize = 600;   // ≈2400 B
/// 渲染成 `<todo_list revision="…">` + 每项一行 + 省略统计。
///
/// ⚠️ **不得含时间戳 / turn 号 / session id** —— 那些会让同样内容产出不同字节，
/// 而「字节稳定」是「状态没变就不注入」判据成立的前提。
pub fn render_block(items: &[TodoItem], revision: u64) -> String;
```

```rust
// adapters/tools-todo/src/lib.rs
pub fn todo_tool(store: Arc<TodoStore>) -> Box<dyn Tool>;
```

```rust
// apps/coding-agent/src/follow_up.rs —— 「一个函数一个 seam」，**不引 trait / registry**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp { Push, Stop(StopCause) }

/// **为什么停** —— 显式枚举（丙的形态）：六种原因可测、可显示、可分别断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    NoPlan,          // 没有计划 ⇒ 普通回合（**这也是「不需要开关」的原因**）
    AllDone,         // 刚被清空 —— 唯一的正常收场
    CapReached,      // used >= MAX_FOLLOW_UPS
    Stalled,         // 本轮 todo 零变化 且 模型自称收尾 ⇒ 原地打转（乙的停滞护栏）
    Aborted,         // stop_reason == Cancelled（用户 Ctrl+C 永远赢）
    NotContinuable,  // stop_reason ∈ {Truncated, ...}
}

/// 回合结束后问一次：要不要为 todo 再起一轮？**纯函数、同步、不 await、不碰 model**。
/// 本仓对「策略」的既有形状就是纯函数（`compact::plan` 同款）。
pub fn decide(store: &TodoStore, used: u32, last_stop: &StopReason, rev_before: u64) -> FollowUp;

/// 组装注入文本。`None` = 不注入（列表空）。
pub fn injection(store: &TodoStore) -> Option<String>;
```

```rust
// apps/coding-agent/src/todo_inject.rs —— 注入判据（甲的形态，纯函数）
pub const TODO_MARKER: &str = "<todo_list";

/// 状态变了 **或** 会话里已找不到标记 ⇒ 注入。
///
/// 「找不到就补」是**自愈**：压缩（或手敲 `/compact`）把块吃掉后自动补回 ——
/// **不需要监听 `ContextCompacted` 事件**，因此也覆盖得了那条不发事件的手敲路径。
pub fn should_inject(store: &TodoStore, last_injected_rev: Option<u64>, session: &[Message]) -> bool;
```

### 7.3 关键数据流

```text
Request::Prompt(user_msg)
│
├─ expand_skill_invocation（既有）
├─ pending = Some(AgentInput::new(user_msg));  used = 0
└─ while let Some(input) = pending.take():
     │
     ├─ 【用户输入抢占】if request_rx.try_recv() 有待处理 Prompt ⇒ 中断链路，交还外层  ⚠️ 见 §7.4 决策 d
     ├─ turns += 1; begin_turn(turns); set_context_window（既有）
     │
     ├─ 【回合边界 · 注入】if should_inject(..) ⇒
     │      ports.session.append(Message{ User, [Text(injection())] })
     │      ⚠️ 直接 append，**不发事件** ⇒ UI 不显示（对比 `Boundary::Steer` 会 emit UserMessage）
     │      last_injected_rev = store.revision()
     │
     ├─ run_turn(input) ── 轮循环 Step 1..9，**一字未改**
     │    ├─ 轮 k：模型调 todo(todos=[…])
     │    │     TodoStore::apply(..)  ← 唯一写入点：校验 → 归一化 → 落盘
     │    │        └─ 无 open 项 ⇒ 清空 + 删文件
     │    │     ToolResult = render_block(..)   ← 最新渲染即刻回到 messages 尾部
     │    ├─ 轮 k+1..：注入块 + 工具结果都在历史里 ⇒ **每轮都看得见，0 额外 token**
     │    └─ Step 4b 压缩（既有）：可能把注入块摘要掉 ⇒ 下回合 `should_inject` 自动补
     │
     ├─ 排空事件信道（既有）
     ├─ match decide(store, used, stop_reason, rev_before):
     │      Push ⇒ used += 1; pending = Some(追问提示词);  刷 View; 回循环顶
     │      Stop(_) ⇒ 落下
     └─ build_view → Outbound::View（侧栏刷新）
```

### 7.4 六个关键决策的取舍

**a. 真值为什么搬出会话？**
ADR-0021 决策 5 的真值（会话里最后一次调用的 `arguments`）在长程下**必然失效**：压缩摘的就是旧消息，任务越长那条调用越早变成旧消息。而三家调研一致：计划必须**活过压缩**。外部文件的额外收益：`/new` 换 stem ⇒ 计划天然归零（与 v1 判断一致），重启由 `latest_session_path()`（`wiring.rs:64,155`）自动恢复 —— **零新机制**。

**b. 为什么不像 Manus 那样每轮重写？**
Manus 明知扰动 KV cache 仍每次重写，因为它 (i) 没有压缩做上下文管理，文件是唯一持久记忆；(ii) 尾部注入的「扰动」其实只限尾部那一段 —— 与本文算式同构，**不是整段前缀失效**。但本仓有**两家没有的东西**：一个**代码拥有的回合边界**（`app_loop`）与一个**已携带 `## Progress` / `## Next Steps` 的压缩摘要**。⇒ 用「状态变才注入」即可达到同样效果，成本约 **1/10**。
> **诚实的天花板**：注入块在长回合里会变旧（Manus 不会）。这是明确接受的退化，逃生门见决策 d。

**c. 为什么判据是「无 open 项」而不是「全部 Completed」？**
「全部 Completed」下，一个被**放弃**的项永久处于非 Completed ⇒ **永不满足 ⇒ 永不删除**（`context.md` §8.2b）。改成「无 open 项」后，这一条**结构上不可能发生**。配套：必须有 `Cancelled` 这个终止态 —— 它是让模型能**诚实停下**的唯一出口；没有它，模型想让环停下就得把没做完的事标成 `completed`，那是**伪造完成**（与引入 `StopReason::Truncated` 是同一条纪律）。

**d. 防套娃靠什么？**
三道，缺一不可：
1. **计数上限**（`MAX_FOLLOW_UPS = 8`）—— 防「跑太久」；
2. **停滞护栏**（本轮 `revision` 未变 **且** 模型自称收尾 ⇒ 停）—— 防「**原地打转**」。这是**真正的防套娃**：上限只防量，它防质。**取自乙**（丙的 `ModelDisagrees` 是同一件事）；
3. **用户输入抢占** —— 追问链路的每次迭代开头先 `try_recv`；有待处理输入立即中断。**用户的意图高于系统的**，否则一个 8 次追问的链路会把用户的输入排队到 8 个回合之后。

**e. 为什么与 Claude Code Tasks 相反（只有主 agent 有 todo）？**
理由是**结构的，不是偏好**：共享要求子表里**有** `todo` ⇒ 同时打破 (i) **子表字节前缀**（每次派发多付 ~1.6k + 其后 system 全 miss）与 (ii) **单写者不变量**（ADR-0022 D17 已开并行派发 ⇒ 并发写同一 JSON 就是 lost update，需引入锁/仲裁/冲突合并）。而收益是零：Q4 明说 subagent 只做简单任务，**子 agent 看不到父的对话**（ADR-0022 D19 的子 prompt 必须自包含）⇒ 它也就不需要父的计划 —— 计划由主 agent 在派发时**编译进 `prompt`**。
> **代价如实**：主 agent 把一项 todo 拆给 3 个子 agent 时，那 3 个之间无法通过共享列表协调（只能靠 `prompt` 写清边界）。
> **翻案条件**：出现「子 agent 之间需要动态认领彼此的工作」的真实需求 ⇒ 换带广播的 store + 子表加回 `todo`，但那时 §8.3 的前缀结论要**重算**。

**f. 为什么不需要开关？**
老 follow-up 设计默认**关**，是因为它的触发（commit 后同步文档）与用户意图无关 —— 用户没要也得发生。**本设计的触发前提是用户自己的计划**：模型不写计划 ⇒ `has_open_items() == false` ⇒ `StopCause::NoPlan` ⇒ 永不追问。**开关从「配置项」变成了「计划的有无」。**

---

## 8. 分阶段落地

按 `context.md` §8.5 已定顺序，**本设计插在 subagent 之后**：

| 阶段 | 内容 | 验收锚点（机械的） |
|---|---|---|
| **P0** | `sha256(tools_json)` + `sha256(system_prompt)` 双锚点 | 搬任何东西前后指纹不变 |
| **P1** | `ToolPlugin` 装配缝（零行为变化） | **指纹保持绿** = 搬家忠实的证明 |
| **P2** | `subagent`（第 10 个工具，`prompt.rs:107` 条件式派生） | 重新锚定，漂移**显式** |
| **P3（本设计）** | `todo`（第 11 个）+ store + 注入 + follow-up 闭环 | 见下 |

**本设计新增的四类锚点**（§8.5 已指出 P0 的字节指纹**盖不住前两类**）：

1. **字节前缀类**：`child_specs_are_byte_prefix_of_parent()`；`removable_suffix_is_exactly_todo_and_subagent()`。
2. **回合形状类**（P0 盖不住）：注入**恰好一次**且**只在状态变/丢失时**；**绝不进 `system`**（新增不变量测试 `system_prompt_does_not_depend_on_todo_state()`）；追问回合**跳过**注入。
3. **控制流类**（变异测试）：删掉停滞护栏 ⇒ 「原地打转」用例必须变红；删掉计数上限 ⇒ 「无限套娃」必须变红；`Cancelled` 时不追问。
4. **状态机类**：全部终态 ⇒ 落盘空表 + 删文件；拒绝丢弃非终态项；`/new` ⇒ 新文件且计划为空；重启 ⇒ 计划随会话恢复。

**⚠️ 既有测试的回归风险**（必须盯）：`app_loop.rs:854` 的 `every_request_variant_is_handled` 与 `basic.rs` Step 4b 的用例都依赖「`MockModel` 响应队列不被意外消费」。本设计给 `Prompt` 臂包 `while`、并在回合前 `load()` store —— **两者都不调模型**，且测试里计划为空（`MemoryTodoStore`）⇒ 不注入、不追问、包循环只跑一次 ⇒ **逐字节不变**。

> **甲提了一个替代顺序**（`P0 → todo → P1 → subagent`）：理由是 todo 的消费者今天是刚需，而 P1 是重构、晚做不产生错误。
> **不采纳，但记账**：§8.5 的理由（P1 在加工具之前做完，`指纹保持绿` 才是可用的验收信号）更强；且 P1 先做能让 todo 的加入从「6 处手工」变成「1 行」。甲的担心（P1 的消费者是纸上的）**是对的**，但那影响的是**要不要做 P1**，不是**它的顺序**。

---

## 9. 代价与风险（如实）

| # | 退化 / 风险 | 量化 / 缓解 |
|---|---|---|
| 1 | **`-p` / `--json` 拿不到闭环** | 这两个入口**根本不走 `app_loop`**（`main.rs:488 run_single_turn` 直调 `agent.run_turn`），且消费循环以**终局事件**为终止条件 ⇒ 多回合会让消费提前收尾。`MemoryTodoStore` 保证不写盘。**脚本化长任务仍会烂尾** —— 本设计最大的功能缺口 |
| 2 | **子 agent 无计划、也看不到父的计划** | Q5′ 的直接后果。派发的子任务必须自包含；子 agent 撞 `max_rounds` 后**无法续** |
| 3 | **注入副本累积** | 每次状态变追加一份，长任务 ~10–20 份 × 190 token ≈ 3k，旧副本与新副本可能矛盾。缓解：块带 `revision` + 前言行写明「以 revision 最大的一份为准」；**不删旧块**（删了会废掉删除点之后的缓存） |
| 4 | **长回合内新鲜度不足** | 一个 R=10 的回合里，计划若在第 1 轮后变了，模型最多 9 轮看到旧 revision。**逃生门**：轮级注入经 `Boundary::Steer`（`push(&self)`，`boundary.rs:74`）⇒ **零 `BasicLoop` 改动**即可拿到轮级粒度，价钱 ×R。⚠️ 但 `Steer` **会 emit `UserMessage`**（`basic.rs:222`）⇒ 块会显示成用户消息。**默认关**，触发条件：实测出现「长回合里明显丢失计划」 |
| 5 | **`ModelDisagrees` / 停滞判据是保守的** | 模型真推进了但忘了更新状态 ⇒ 被误判为停滞而停下。缓解：guideline 明写「结束回合前更新状态」。**宁可停下，宁可漏推** |
| 6 | **追问消耗回合号** | `turns += 1` ⇒ 状态行回合数跳格。缓解：追问输入带 `[auto-continue]` 前缀，TUI 渲染成 System 行（纯外观，**不引入协议字段**） |
| 7 | **静默写会话** | 注入块是 UI 从未显示过的消息。既有先例：压缩写回。后果：`/export` 会带走快照（视为合理） |
| 8 | **摘要散文 vs 结构化计划仍可能不一致** | §3.3 的发现**未被本设计解决**。裁决：**让权威源唯一** —— 追问判据只读 todo（机器读），摘要只作模型的理解背景。**矛盾时行为由数组决定** |
| 9 | **追加式注入是「模型看到了什么」的额外来源** | 隐藏它等于让 transcript 说谎，故**刻意保留可见** |
| 10 | **`/new` 留下旧 todo 文件** | 与「旧会话文件保留」一致，不新开清理规则 |
| 11 | **成本随计划长度线性** | 计划是模型**自陈**，可以写 50 项撑大上下文。上限靠块预算 600 token + 工具描述里的「3–8 项」约束（常量 ⇒ 命中缓存 ⇒ ~4 当量/轮） |

**明确不做**：通用 hook / 注入系统（消费者仍为 0）；依赖图 `blockedBy`（本仓单 agent **串行**执行，无调度器消费「解锁」⇒ 零消费者）；跨 session/subagent 共享 + 广播（与 Q5′ 正面冲突）；`ToolResult.details`（ADR-0021 决策 2 继续有效）；TUI 通用渲染注册表（ADR-0021 决策 4 继续有效）；`TodoStore` 核心 trait / `FollowUpRule` trait（各 1 个实现 ⇒ 退化为具体类型 + 纯函数）；`TodoInjectionPolicy` trait（同上，若将来真要「每轮重述」形态，**那是一个 bool 字段**，不是 trait）。

---

## 10. 与既有 ADR 的记账

| 既有决策 | 本设计 | 处理 |
|---|---|---|
| **ADR-0021 决策 5**（真值 = 最后一次调用的 `arguments`） | **被取代** | 真值移到外部文件；会话里的 `arguments` 降级为「写入日志」（也是删除后的审计余量）。**必须写进 ADR-0024 的 supersede 声明** |
| **ADR-0021「否决落盘持久化 todo」** | **翻案** | 原否决是「为 todo 而 todo」+「引入第二真值源使 `/new` 语义含糊」；本设计正面回答后一半（文件为唯一真值、`/new` 换 stem ⇒ 计划天然归零），前一半（量级）由长程需求承担 |
| **ADR-0021「已知缺口：todo 会被压缩摘要掉」** | **关闭** | 真值在外 + `should_inject` 的标记丢失判据 ⇒ 压缩后自动补回，**零核心改动** |
| **ADR-0021 决策 1**（注入走 `messages` 尾部） | **遵守**，并首次真正使用 | 新增不变量测试把它变成机械保证 |
| **ADR-0021 决策 3 / 4**（`ToolPlugin`；显示走 `CodingView`） | **遵守** | 侧栏 = `CodingView.todos`；工具放 adapter crate ⇒ **今天就是**插件缝要的形态 |
| **ADR-0022 D3**（子表 = 父表 − 派发工具） | **扩展为** − {`subagent`, `todo`} | 增加「被摘的必须都在**后缀**」+ 字节前缀测试 |
| **ADR-0022 D17**（并行派发） | **不冲突** | 子表无 `todo` ⇒ 无并发写。**写下**：将来若给子表加回 `todo`，会**同时**打破字节前缀与单写者两个不变量 |
| **`design-plans/2026-09-15-follow-up.md`** | **判据替换、seam 复用** | HEAD 对比整体不再使用；`while let Some(msg) = pending.take()` 循环正是本设计的落点，将来 commit-doc 追问只是**同一循环里的第二个判据**（加法） |
| **`design-plans` §3.3 护栏上限默认 1** | 改为 **8** + 停滞护栏 + 抢占 | 上限 1 对长程显然太小 |
| **动 `BasicLoop`** | **零** | 开新路记账：本设计全部新机制在 `apps/coding-agent` + 一个新 adapter crate |

---

## 11. 参考

- [`context.md`](./context.md) —— 需求澄清（7 条）+ 五条新约束
- [ADR-0024](../../adr/0024-long-horizon-todo.md) —— 决策记录
- [ADR-0021](../../adr/0021-plugin-seam-without-hook-system.md) —— 被 supersede 的决策 5；`ToolPlugin` 缝
- [ADR-0022](../../adr/0022-subagent-dispatch.md) —— D3（子表前缀）、D17（并行派发）
- [`../prompt-delivery/notes.md`](../prompt-delivery/notes.md) —— 注入通道与成本（决策 b 的判据来源）
- [`../../extension-points.md`](../../extension-points.md) —— 判断链（写计划 = Tool / 推动 = Follow-up）
- [`../../design-plans/2026-09-15-follow-up.md`](../../design-plans/2026-09-15-follow-up.md) —— follow-up seam 的既有设计
- 调研：[Manus 上下文工程](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus) ｜ [Cline Focus Chain](https://cline.bot/blog/focus-attention-isnt-enough) ｜ [Claude Code Tasks](https://venturebeat.com/orchestration/claude-codes-tasks-update-lets-agents-work-longer-and-coordinate-across)
