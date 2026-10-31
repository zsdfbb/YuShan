# Plan 模式 — 架构上下文

> 产出：`/arch-explore`（2026-11-03）。
> 相关：`docs/arch/subagent/`（角色机制）、`docs/arch/hooks-and-plugins/notes.md:29`（pi 的 plan-mode 扩展）、
> `skills/sequential-workflow/REFERENCE.md`（用户既有用法）。
> Claude Code 行为的一手证据：`~/.claude/projects/**/*.jsonl` + `~/.claude/plans/`；
> seabox 库证据：`~/Develop/seabox/src/lib.rs`、`src/linux/mod.rs`、`src/config.rs`。

## 概述

给主 agent 一个**会话级的只读规划模式**：`Plan | Yolo`，默认 `Yolo`。用户用 `/plan` 进入、`/yolo` 退出；`write` / `edit` 在 `Plan` 下被门禁拒绝。

**本设计分两片推进**（见 §切片）：

| 片 | 内容 |
|---|---|
| ① | seabox 执行边界 |
| ② | plan 模式 v1（模式 + 门禁 + 命令 + 状态栏 + 两个工具 + `PlanPrompt` 第四通道） |

两件事明确不在范围内：**给 subagent 加第二个子 agent 角色**，以及**主 agent 切换**（理由见 §范围外）。

---

## 现状

### 模块边界

```text
apps/coding-agent/src/
  toolset.rs        工具表装配的唯一接缝（build_tools / EXPECTED_ORDER）
  assembly.rs       AgentFactory：按工作区装配 agent
  prompt.rs         系统提示组装
  subagent.rs       子 agent 派发、角色（SubagentMode）、预算
  app_loop.rs       回合编排、进程级状态镜像（thinking_visible / theme）
crates/ys-tui-coding/src/commands.rs    命令表 + 解析
crates/ys-tui-coding/src/draw.rs        状态栏渲染（status_text）
crates/ys-protocol/src/request.rs       UI ↔ app 唯一契约（Request / Outbound）
crates/ys-tool/src/approval.rs          ApprovalHandler
adapters/tools-basic/                   bash / 文件 / 搜索工具
```

### 核心抽象

| 抽象 | 位置 | 要点 |
|---|---|---|
| `Tool` | `crates/ys-tool/src/lib.rs` | `spec()` + `call()`；`is_concurrency_safe()` / `timeout()` 不进 `ToolSpec` |
| `ToolContext` | `crates/ys-tool/src/context.rs` | **每次调用新建**（`basic.rs:1337` 并行、`:1409` 串行）。已有四个可选挂载点：`boundary` / `summarizer` / `subagent` / `tool_budget`，共性是不挂载即逐字节不变，且均不进 `ToolSpec` |
| `ApprovalHandler` | `crates/ys-tool/src/approval.rs` | `needs_approval` / `request_approval` → `Approved \| Denied{reason}`。已接入生产（`toolset.rs:258`、`main.rs:1112`）但注入 no-op `AutoApprove`；调用点 `basic.rs:1274-1286`，拒绝渲染为 `operation denied: {reason}` + `is_error` |
| `SubagentMode` | `crates/ys-tool/src/subagent.rs:18` | `General \| Explore`，行为散在六处 `match` |
| `child_tools` | `apps/coding-agent/src/subagent.rs:1082` | 子表由 `toolset::build_tools()` **独立构造**，不经过主 agent 状态 |
| `AgentFactory::build` | `assembly.rs:141` | 唯一的 agent 装配序列 |

### 数据流

```text
启动 / resume
  └─> AgentFactory::build(workspace)
        ├─> toolset::build_tools()      9 元基础表
        ├─> prompt::build_system_prompt()
        └─> AgentBuilder::tools()...system_prompt()...working_dir()

回合
  └─> BasicLoop Step 9：审批(9a) → 分段执行(9b) → 保序组装(9c)
```

### 外部依赖

| 依赖 | 说明 |
|---|---|
| seabox | OS 级沙箱（Landlock + seccomp + namespaces），同机 `~/Develop/seabox`，crates.io 有发布 |
| 平台支持 | Linux 5.13+ 完整；3.5–5.12 无文件系统限制；**macOS（Seatbelt）规划中**。本机 Linux 7.0.0-38，**可用性未实测**（见 §前置验证） |

---

## 约束

### 硬约束

1. **`ys-core` / `ys-event` / `ys-protocol` 不含 tokio** —— 模式 enum 若落这三处，不得带运行时依赖。
2. **`ys-tui-coding` 不依赖 `ys-runtime`** —— UI 只经 `Request` / `Outbound` 与 app 通信。
3. **工具面恒定**（`toolset.rs:96`）：*「工具面一旦会变，模型会把『工具消失』误读成『我记错了』」*。
4. **缓存前缀契约**：`prefix_fingerprint_is_anchored_and_stable`（`toolset.rs:624`）钉住 `fp_tools` / `fp_parent`；父表 JSON 必须是子表的字节前缀。
5. **`capabilities.rs` / `toolset.rs` 绝不 print** —— 输出走 `Outbound::Output`。
6. **系统提示是回合级字段**（`ys-component/src/context.rs:21`）—— 回合内不可更换。

### 信道约束（决策 4 的出发点）

`Request` **只在回合边界被消费**（`ys-protocol/src/request.rs:11`）。`boundary.rs:26-31` 已写明其后果：

> 请求信道只在回合之间被消费 ⇒ 回合中投的 `Request` 要等整个回合跑完，可能长达几分钟。

**因此审批结果不能经 `Request` 回传** —— `request_approval` 正在 await 时，它的回复要等回合结束才被读到 ⇒ 死锁。

`run_turn_once` 的 `select!`（`app_loop.rs:1457-1461`）只等三个分支：run 本体、`sink_rx` 事件、预算计时器。**`request_rx` 不在其中** —— 回合内没有人读 `Request`。

`ExitPlanMode` 的工具结果要带上「用户切了没有、切到了什么」，因此必须**当场等到回答**。这条约束所以变成**必须解决**，不是绕开：见决策 4 的第四通道。

### 性能与演进

- 模式切换**不重建 agent**（门禁方案），无缓存冷启。
- 若系统提示因模式追加段，必须**附加在末尾**：前缀缓存按字节比对，插在中间会让其后全部失效。
- seabox 是可选 feature，未启用时 `tools-basic` 行为逐字节不变 ⇒ 指纹锚不动。

### 组织

- 「不做安全」是项目定位（`AGENTS.md`、`README.md:87`）：**本仓只消费 seabox**，不定义沙箱抽象层，不包可替换 trait。
- 不跨动态库边界传 trait object ⇒ seabox 只能静态链接。

---

## 范围

### 范围内

- 会话级模式：**`Plan | Yolo`**，`Yolo` 为默认。
- `/plan`：只进入 `Plan`，无参、不需确认、幂等。
- `/yolo`：回到默认模式，不需确认。
- **调用期门禁**：`Plan` 下 `write` / `edit` 返回 `ToolError::PermissionDenied`（变体已存在，`ys-tool/src/error.rs:9`）。
- **状态栏显示模式**：`Plan` 时出现，`Yolo` 时不显示。
- **两个模型侧工具**：`EnterPlanMode`（能进入）、`ExitPlanMode`（**等待**用户的切换结果并把它回给模型，见决策 4）。
- **`PlanPrompt` 第四通道**：回合内工具 → UI 的提问 / UI → 工具的应答（`ys-tool` 定义、app 实现、TUI 作答）。
- **`Request::SetPlanMode { plan }`**：UI → app 的模式同步（与 `SetThinking` 同形，无输出、只回 `View`）。`/plan` / `/yolo` 经它落到 Task 层。
- **seabox 执行边界**：`bash` / `bash_bg` / 搜索二进制在 `Plan` 下以只读策略运行。
- 无沙箱时的**降级告知**。
- 系统提示的能力边界声明改写（见 §必需项）。

### 范围外

| 项 | 理由 |
|---|---|
| **通用审批 UI** | 第四通道本轮只为 `ExitPlanMode` 建（决策 4）。`ApprovalHandler` 仍注 `AutoApprove`，接入它留给将来 |
| agent 切换 / 多主 agent | 撞 ADR-0024 D6：「只有主 agent 有 todo」由「子表是父表字节前缀」结构保证。opencode 的 `mode: "primary" \| "subagent" \| "all"` 正是在这个压力下收成一个枚举（`docs/arch/subagent/research.md:99`） |
| plan 作为第三个 subagent 角色 | 用户的用法是「主 agent 的模式」；角色表重构可独立推进 |
| `SubagentMode` 收成 `RoleSpec` 表 | 纯重构，行为不变 |
| 沙箱细粒度配置（per-tool / per-path） | 本轮只有两档：`workspace:rw` / `workspace:ro` |
| `-p` / `--json` 的模式支持 | 无 TUI；且 `ExitPlanMode` 只提示不等待，CLI 侧无退出问题 |

### 场景

| 场景 | 行为 |
|---|---|
| 主路径 | `/plan` → 门禁只读 → `write`/`edit` 被拒并附出口提示 → `bash` 在只读沙箱内 → 模型产出计划文本 → 模型说「请运行 `/yolo`」→ 用户敲 `/yolo` → 解除 |
| 模型想退出 | 调 `ExitPlanMode` ⇒ 工具**挂住等**；TUI 推固定文本，用户答 `yes`/`Y` ⇒ 退出并回「已切到 Yolo」；答别的 ⇒ 回「模式未变」并由那次回答继续对话 |
| 模型自行进入 | 调 `EnterPlanMode` ⇒ 门禁**立即**生效；状态栏到回合末才显示（决策 4） |
| 无沙箱 | 仍进入模式，`write`/`edit` 仍被拒，**`bash` 可写**，用户已被告知 |
| 派 `General` 子 agent | 子 agent **可写**（`Plan` 不传染，见决策 2） |
| `/new` / `/clear` / `/resume` | **模式不变**（Q2） |

---

## 决策

### 决策 1：门禁在执行时，工具表恒定

**不改工具表**，而是让 `write` / `edit` 在 `call()` 开头读状态并拒绝。三条理由都是结构性的：

| 若改为过滤工具表 | 障碍 |
|---|---|
| 工具表在回合内是只读借用 | `ys-component/src/context.rs:14` `pub registry: &'a ToolRegistry` —— 回合第 3 轮改表，第 4 轮看到的仍是旧表 |
| 没有「切换后继续」机制 | `ys-tool` / `basic.rs` 无 follow-up 概念；唯一出口 `Boundary::Abort` 会误伤用户请求 |
| 指纹锚会漂 | `toolset.rs:40` `EXPECTED_ORDER` 9 元真值 + 两个指纹 + 「父表是子表字节前缀」断言 |

门禁方案的收益是三重的：

- **工具表一字不动** ⇒ 两个指纹不漂，硬约束 3 不必破例；
- **立即生效** ⇒ 无需重建 agent，无缓存冷启；
- **拒绝信息量更大**：`tool not found` 只说明「没有」，门禁文案能说明「为什么」与「怎么出去」。

**拒绝文案属于设计的一部分**（硬约束 3 警告的正是「模型以为工具坏了」）：

```
plan mode: editing is disabled. Produce the plan as text;
the user runs /yolo to start executing.
```

**时序**：门禁即时生效（每次调用新建 `ToolContext`），系统提示要到下一回合才变（硬约束 6）。不一致由 `/plan` 命令的输出行补位——命令是用户动作，本来就有可见反馈。

**不把模式写进系统提示的正文差异**：约束靠机制，提示词只做说明。参照 CodeWhale——它用测试钉住三种模式的系统提示逐字节相同（`tmp/codewhale/crates/tui/src/prompts.rs:1592-1595`，含 `!contains("##### Mode:")`），Plan 的约束全在 `authority.rs:53/333/406`。

### 决策 2：两个模式值，默认那个叫 `Yolo`

| 模式 | 工具门禁 | 沙箱策略 | 无沙箱时 |
|---|---|---|---|
| **`Yolo`**（默认） | 全开 | 无 | 同左 |
| `Plan` | `write`/`edit` 拒 | `workspace:ro` | 仅工具层门禁 |

**`Yolo` 就是当前生产行为**——`main.rs:1112` 注入的是 `AutoApprove`，且 seabox 尚未接入（当前 `bash` 完全无边界）。给它命名不是为了新增行为，而是让「默认档」与「规划档」在代码与 UI 里有一对对称的名字。

命名沿用户的既定叫法。需注意的是这个词在外部语境里常指「跳过全部确认」——而在本仓它恰好就是字面意思。因此**不加安全含义式提示文案**，避免把名字读成一种警告。

#### 保证范围：只覆盖主 agent，不向下传染

**这是本设计的已知边界，必须显式写明。**

`child_tools`（`apps/coding-agent/src/subagent.rs:1082`）从 `toolset::build_tools()` 独立构造子表，**不经过主 agent 的模式状态**。因此：

| 派发 | 子工具表 | `Plan` 下能写吗 |
|---|---|---|
| `Explore` | 过滤掉 `write`/`edit`（既有行为） | ❌ |
| `General` | 完整 9 元表 | **✅ 能写** |

即：**`Plan` 模式不传染给子 agent。** 这个选择是明确的：

- **好处**：`General` 的职责就是干活，让它继承只读等于让它在 `Plan` 下完全没用。用户想干活时不必先退模式。
- **代价**：`Plan` 的写保护**可被模型经 `subagent` 绕过**——只需派一个 `General`。因此它**不是安全边界**，而是防误触的门禁。

**这句话不写进系统提示给模型**（不列举逃生口，同决策 6 第 2 条），但**必须出现在面向用户的文档与代码注释里**。

**若将来需要真正的全局只读**，落点是 seabox：把 `workspace:ro` 应用在**子 agent 的 spawn** 路径上，那是进程级的，模型绕不过。本设计不做。

#### 安全底线：模式不得由模型参数决定

`Plan` 的沙箱策略必须由 **app 侧从模式真值取**，不能从模型可控的任何参数取。

理由：若工具 schema 带有沙箱相关字段，模型可以填一个 `workspace:rw` 进 `Plan`。那样只读保证就在模型手里，不在机制里。

### 决策 3：用户侧入口 `/plan` / `/yolo`

命令表是单一真值——补全浮层、`/help`、解析三者都从 `COMMANDS` 派生（`commands.rs:42-45`），故「浮层里看得到、敲下去却不认识」在结构上不可能发生。加命令只需：`COMMANDS` 数组 17→19、`parse` 加两臂。

**两条命令都不经审批**。理由：用户自己敲的命令，用户就是授权源。模型侧的审批要防的是**模型**自行逃离只读态，与用户手动切换无关——这是两条不同信任级的路径。

**`/plan` 幂等**：已在 `Plan` 时再敲 ⇒ 空操作 + 一行提示，不报错。

**`/yolo` 是设成而非 toggle**：两档下二者等价，取「设成」语义更直白（已在该档时为空操作）。

**模式是进程级状态**，与 `thinking_visible`（`app_loop.rs:341-345`）、`theme`（`app_loop.rs:347-359`）同形，**不在 session 里**：

1. **`/new` / `/clear` 不重置它**；
2. **`/resume` 也不改它** —— 与 `theme` 同理（`app_loop.rs:352-353`）。模式不写 session meta，否则一条旧会话能把进程切回只读态。
3. **`/clear` 须交代模式仍在** —— `/clear` 的字面语义是「一切重来」，而模式不重来。未告知则用户会对着被拒的 `write` 找原因。

#### 状态栏：模式必须比 token 先活下来

状态栏是 `status_text`（`crates/ys-tui-coding/src/draw.rs:749`）拼的 `segs: Vec<String>`，窄终端下**从尾部逐段剥**（`:803-810`：「取能放下的最长前缀」）。

**故插入位置决定它在窄屏上是否可见。** 现有顺序：`model` → 新内容提示 → 轮数 / 耗时（回合中）→ token / 缓存 → 时长。

模式段插在 **`model` 之后、其余段之前**，理由与「↓ 有新内容」同（`draw.rs:757-758` 明写了这个优先级）——它在窄终端下必须比 token 活得久。

```
<model> · Plan · ↑1.2k ↓340 · 缓存 62% · 2m13s
```

**`Yolo` 下不显示这一段** —— 不因本需求多出一个常驻词。只在 `Plan` 时出现，既是信号也是提醒。

> **附带纠正**：`skills/sequential-workflow/REFERENCE.md:13` 写的 `.yushan/plans/` **在磁盘上不存在**（`ls` 实测）。实际用的是 `~/.claude/plans/`（客户端管理）与仓库内 `docs/design-plans/`。该句应按实测订正。

### 决策 4：模型侧两个工具，一个能进、一个能问

| 工具 | 改模式吗 | 返回什么 |
|---|---|---|
| `EnterPlanMode` | **自己改**（立即生效） | 「已进入 plan 模式」 |
| `ExitPlanMode` | **不改**，问用户 | **用户是否切了、切到了哪档** |

**这个不对称是刻意的。** 进入是**收紧**——即便模型误判，代价只是它自己写不了文件，用户 `/yolo` 即可复原。退出是**放宽**——若模型能自行退出，plan 模式的保证就落在模型手里，等于自证无效。

所以 `ExitPlanMode` 的语义是「**请求并等待**」，不是「切换」。它的工具结果必须回答三件事：

```text
switched: yes | no
mode now: Yolo | Plan
-> 按当前模式工作（Yolo：可以写；Plan：继续只读、不要再调本工具）
```

第三句不能省：否则模型拿到 `switched: no` 会反复重试同一个工具。

#### `EnterPlanMode` 需要一个可写挂载点

`ToolContext` 的四个既有挂载点全是 `&'a` 只读借用（`ys-tool/src/context.rs`）——`boundary` / `summarizer` / `subagent` 是 `&dyn`，`tool_budget` 是 `Copy` 值。**没有一个能写。**

故第五个挂载点是**带内部可变的共享单元**：

```text
PlanMode（内部 AtomicBool）
  is_plan() -> bool      门禁读
  enter()                EnterPlanMode 工具写
  exit()                 /yolo 命令与 UI 写（app 侧）
```

app 持有 `Arc`：一份挂进 `ToolContext`，一份留在 `app_loop` 供命令与 `build_view` 读。**这是第一个可变挂载点**——前四个都是只读能力或 `Copy` 值。

#### 状态栏滞后到回合末

工具没有输出通道（`Outbound` 由 `app_loop` 持有），故 `EnterPlanMode` 写入的状态**要等本回合结束、`build_view` 刷新**才反映到状态栏。

不影响正确性，因为三条路径的时效不同：

| 谁 | 何时知道 |
|---|---|
| 门禁 | **立即**（读同一个原子，下一次 `write`/`edit` 即被拒） |
| 模型 | **立即**（工具结果直接告诉它已进入） |
| 用户的状态栏 | 回合末 |

模型进入模式后通常立刻转为规划并结束回合，故滞后终点很短。若将来要求即时反馈，才需要回合内 UI 推送通道；v1 不做。

#### `ExitPlanMode` 的一条已知代价

`ExitPlanMode` 在回合中途挂住等用户，期间**整个回合停住**——模型不能同时继续别的轮，用户也看不到模型在做什么新事。这是「工具结果必须带用户选择」的必然代价，接受它，不做「异步等待 + 继续跑」的复杂形态。

**不必担心它无限等**：外层预算本来就把每次工具调用包在 `tokio::time::timeout` 里（`basic.rs:456`，默认 300s）。`ExitPlanMode` 只需实现 `Tool::timeout()` 返回一个更长的值——等待被人为拉长，不是新增机制。

#### 第四通道：工具挂住，等 UI 作答

工具结果要带上用户的切换结果 ⇒ 工具必须**当场等到**。这是本设计**唯一**新增的通信路径，由两条腿加一个 trait 组成：

| 方向 | 载体 | 说明 |
|---|---|---|
| app → UI（问） | `Outbound::PlanPrompt { text }` | 复用既有 `out_tx`，只多一个变体 |
| UI → app（答） | **新** `mpsc::Sender<PlanAnswer>` | 与 `requests` / `boundaries` **并列的第三个 sender**，TUI 构造时多拿一个 |
| app → 工具（唤醒） | `PlanPrompt` trait（`ys-tool`） | 形照 `ApprovalHandler`，挂 `ToolContext` 第六个挂载点 |

**为什么不复用 `Request`**：`Request` 的契约就是「回合之间消费」（§信道约束）。让 `run_turn_once` 去读 `request_rx`，会把 `/model`、`/new` 这些控制命令也吸进回合内提前执行，破坏那条契约。**新开一条只承载应答的信道，语义单一，不牵连任何既有变体。**

**为什么应答不是 `Request::SetPlanMode`**：那条走回合边界，等它到时工具早就超时了。模式真值本身仍由 `SetPlanMode` 同步（`/plan` / `/yolo` 用），但**回合内这一次**由 `PlanAnswer` 直接携带。

```rust
// ys-tool/src/plan.rs
#[async_trait]
pub trait PlanPrompt: Send + Sync {
    /// 展示固定文本并等待用户作答。返回**作答后**的模式真值。
    async fn ask_exit(&self, summary: Option<&str>) -> PlanAnswer;
}

pub struct PlanAnswer { pub switched: bool, pub now_plan: bool }
```

`ask_exit` 内部：推 `Outbound::PlanPrompt` → 等 oneshot → 收到应答后写 `PlanMode` 真值并返回。`oneshot` 的 sender 被 drop（UI 线程消失 / Esc 收场）⇒ 返回 `switched: false, now_plan: true`，**按「没切」收场**，与审批「悬空即拒」同一条原则。

#### TUI 侧：固定文本 + 输入框作答，仍然不弹模态

`Outbound::PlanPrompt` 到达时（`App::apply_outbound`）往 transcript 推一行固定文本并记下「有待答 prompt」：

```text
计划已完成。回复 yes / Y 退出 plan 模式；否则直接说你想改什么。
```

用户在 `submit`（`events.rs:692`）提交时，**先于** `commands::parse_with_skills` 加一条前置分支：

```text
if app.pending_plan_prompt {
    if is_exit_affirmative(&text) || text.trim() == "/yolo" {
        发 PlanAnswer { switched: true, now_plan: false }
        return;                              // 不当作 Prompt 发出去
    }
    发 PlanAnswer { switched: false, now_plan: true }
    // 然后**继续往下走**：这次输入仍作为普通 Prompt / 命令 dispatch
}
```

- `is_exit_affirmative`：去首尾空白后 `eq_ignore_ascii_case` 匹 `yes` / `y`。
- **退回通道顺带有了**：用户不答 `yes`、直接提修改意见 ⇒ 工具立刻回「模式未变」，那句话同时作为普通 `Prompt` 进对话，模型接着改计划。不需要另造「否决」概念。
- 输入框里看得见自己敲的字，比模态更接近「这就是对话的一部分」。`PromptKind` 现有五个值全是选择型，加确认框等于开一种新输入模式，不值。

#### 无 TUI 时

`PlanPrompt` **不挂载**（照 `summarizer` / `subagent` 的 `Option` 惯例）⇒ `ExitPlanMode` 直接返回「没有可询问的用户，模式未变，请把计划作为文本给出」。`-p` / `--json` 由此优雅降级，不需要另写分支。

#### 工具表：进追加段，父表 12 → 14

两者都是**主 agent 专属**（`Plan` 不传染子 agent，见决策 2），故进追加段而非基础表：

| 锚点 | 变化 |
|---|---|
| `EXPECTED_ORDER`（9 元） | **不变** ⇒ `fp_tools` 不变 |
| 父表（12 元） | **→ 14 元**，追加在 `subagent_async` 之后 ⇒ 只改 `fp_parent` 与那条「父表应为 12 元」断言 |
| 「父表是子表字节前缀」 | 结构不变 |

排在末尾而非插进现有三条之间：追加段已有的顺序契约（`bash_bg` 在派发前）与 plan 无关，不动它。

### 决策 5：seabox 先落地，plan 模式只是它的第一个消费者

`bash` 本就该有执行边界（现在完全没有）；`Plan` 只是把策略从 `workspace:rw` 换成 `workspace:ro`。生产路径**至少三处** spawn：

| 文件:行 | 用途 | 接入形态 |
|---|---|---|
| `adapters/tools-basic/src/bash.rs:526` | `BashTool`（`sh -c`） | `CommandSpec::with_program("sh").with_args(["-c", …])` |
| `adapters/tools-basic/src/background.rs:641` | 后台命令启动（`bash_bg` 是独立于 `bash` 的第二入口） | 同上 + `spawn()` |
| `adapters/tools-basic/src/exec.rs:226` | `run_bounded`（`rg` / `fd` / `grep` / `find` 直连二进制） | `CommandSpec`，无 shell 层 |

> 完整枚举留给 design —— `background.rs:86`（`wc -c`）、`lib.rs:315`（经 stdin 的 `cat`）需逐个确认。

**「只覆盖 `bash`」是错的**：`grep` / `find` 也 spawn。

**现有进程管控在 seabox 侧均已等价或更好**（源码核对结论）：

| 现有 `bash.rs` 行为 | seabox 对应 |
|---|---|
| `Command::new("sh").arg("-c")` | `CommandSpec` 是同一件事 |
| `stdin(Stdio::null())` 防 `rg`/`grep` 读 stdin（`bash.rs:530-535` 实测吃满 300s 预算） | `CommandSpec::stdin: None` ⇒ 子进程读端立即 EOF |
| 自行杀**整个进程组**（孙进程随组而死） | `SandboxConfig::process_group` **默认 `true`** ⇒ `kill()` 打 `-kill_pid` 组杀（`src/linux/mod.rs:634-641`）；`setpgid(0,0)` 恒执行（`src/config.rs:39-41`） |
| 工具自管超时 | `CommandSpec::with_timeout()` + `SandboxChild::wait_timeout()` |

⇒ **接入是替换 spawn 调用，不是重写进程管理。** `Sandbox::execute()` 已内部捕获全部输出，`bash` 这种「跑完拿结果」的语义直接用它即可。

接入形态：`tools-basic/sandbox` feature + 静态链接，未启用即逐字节不变。

#### 拒绝诊断如何交给模型

seabox 返回结构化原因（`src/lib.rs:137-165`）：

```rust
ExitReason::Denied { mechanism: DenyMechanism /* Landlock | Seccomp | Unknown */, message: String }
ExitReason::Program(i32)      // 与沙箱无关的非零退出
ExitReason::InternalError(String)
```

`Sandbox::execute()` 返回 `(CommandOutput, ExitReason)`；`SandboxChild::blocked() -> Option<(u32, u32)>` 给出被拒的 syscall 与 errno。

**接缝**：`BashTool` 在 `Denied` 分支改写错误文本，把内核语义翻译成模型能执行的判断：

```
sandbox denied write (Landlock): <kernel message>
plan mode is read-only; produce the plan as text. The user runs /yolo to start executing.
```

后半句是关键——裸 `Permission denied` 会让模型反复重试同一条命令。

### 决策 6：无沙箱时仍进入模式，且明确告知

门禁不依赖 seabox——`write` / `edit` 的拒绝在任何平台都成立。拒绝进入模式等于把这份可用保护也扔掉。

| | 有沙箱 | 无沙箱 |
|---|---|---|
| 工具层门禁 | ✅ | ✅ |
| `bash` 可写 | ❌ | ✅ |
| 告知 | — | **必须** |

告知三原则：

1. **面向用户**：`degraded_hint`（先例 `adapters/tools-web/src/lib.rs:262`，合并路径 `toolset.rs:88`），逐字说明「本平台无沙箱，plan 模式下 `bash` 仍可写」。
2. **不面向模型**：系统提示**不列举**「`bash` 可绕过」——列举等于教它绕过。
3. **不复述平台细节**：模型只需知道 `write` / `edit` 不可用，其余从拒绝文案中习得。

---

## 必需项（与本需求强耦合，不能留到后面）

### 1. 系统提示的能力边界声明会变成谎话

三处 + 一处测试拷贝，做本功能即失效：

| 位置 | 现文 |
|---|---|
| `apps/coding-agent/src/prompt.rs:224` | `it has no plan mode — ignore instructions that ask for plan mode` |
| `apps/coding-agent/src/prompt.rs:226` | `has no subagents and no plan mode` |
| `apps/coding-agent/src/prompt.rs:1310` | 断言 `contains("no plan mode")` |
| `apps/coding-agent/tests/e2e_skills.rs:90` | 逐字拷贝 |

`docs/arch/skills/design.md:278` 记录：约 20 个 skill 依赖这句话判断能力边界，故改写不是删句，而是**换成准确的现状**。

**新说法（Q10 的答案）**：不再声明「没有 plan mode」，改为说明**用户拥有它**：

> `` `EnterPlanMode` / `ExitPlanMode` are available. Plan mode restricts writes. `ExitPlanMode`
> asks the user to switch back and waits: its result tells you whether they did and what mode
> you are in now — work according to that. ``

最后半句不能省：它同时告诉模型「不要假装已退出」与「结果以工具返回为准，不要重试」。

### 2. 后台任务跨模式：只拦新进程

`bash_bg` 起的进程（`make` / `npm install`）会活过模式切换。**已决：只拦新启动的，已有进程不动。**

| 情形 | 行为 |
|---|---|
| `Plan` 下新起 `bash_bg` | **拒绝**（`ToolError::PermissionDenied`，与 `write`/`edit` 同一文案形状） |
| 切 `Plan` 前已在跑的后台任务 | **不动**，继续跑、继续写 |

理由：后台命令本就是长任务（`npm install` 可能跑十几分钟），切模式时杀掉等于毁掉已完成的工作。而「只拦新」是**单向**的——它只会让写更少，不会让写更多。

落点：`background.rs` 的启动点（`:641`）读门禁状态，规格与 `write`/`edit` 同一条。**注意这不影响前台 `bash`**——那条由 seabox 的 `workspace:ro` 管（决策 5），两层各管各的。

> 已知漏洞：切进 `Plan` 时若已有 `bash_bg` 在写，`Plan` 的只读性就只覆盖工具层，不覆盖文件系统。这与「`General` 子 agent 可写」（决策 2）同属**已知边界**，一并写进面向用户的文档。

---

## 前置验证（动手前先做，5 分钟）

```
ls /sys/kernel/security/landlock    → 不存在
uname -r                            → 7.0.0-38-generic
grep -i landlock /boot/config-*     → 未查
```

内核够新，但 securityfs 未挂载或 Landlock 未编进内核两种解释都成立。**决策 5 整个建立在「seabox 可用」上**，先跑一次 `seabox check` 或最小 demo 确认，不要写到一半才发现。

---

## 未澄清问题

- [x] ~~**Q2 模式在 `/new` / `/clear` 后的存续**~~ **已结：不切换、不重置**（见决策 3）。
- [x] ~~**Q3 plan 下派 `General` 子 agent**~~ **已结：允许，且 `General` 可写**（见决策 2「保证范围」）。
- [x] ~~**Q4 sandbox 拒绝的错误文本**~~ **已结：`BashTool` 在 `ExitReason::Denied` 分支改写文案**（见决策 6）。
- [x] ~~**Q8 退出命令的形态**~~ **已结：独立名 `/yolo`**（见决策 3）。
- [x] ~~**Q9 `/yolo` 是设成还是 toggle**~~ **已结：设成**（见决策 3）。
- [x] ~~**Q5 seabox 版本钉法**~~ **已结：用最新版。** 不钉 git rev、不用路径依赖（`~/Develop/seabox` 只是开发期参考）。macOS 支持后降级探测自然收窄，不需额外代码。
- [ ] **Q6**：`ExitPlanMode` 的 `summary` 是否设长度上限？`MAX_SUMMARY_BYTES = 8*1024`（`subagent.rs:32`）是先例。
- [x] ~~**Q10 系统提示与 skill 的措辞**~~ **已结：改 skill。** `EnterPlanMode` 工具落地后 `REFERENCE.md:21/25` 的「立即 `EnterPlanMode`」**原样可行**，不需改；只需订正 `ExitPlanMode` 的几处（`:23` / `:30` / `:352` / `:359` / `:373`）为「调 `ExitPlanMode` 请求退出，TUI 会请用户在输入框确认」。系统提示侧见 §必需项 1。

---

## 切片与顺序

### ① seabox 接入（先做，可独立交付）

`tools-basic/sandbox` feature + 三处 spawn 改造 + 平台/内核探测降级 + `Denied` 文案翻译。

**为何先做**：它独立就有价值（`bash` 现在毫无边界），且其风险（平台、内核、API 稳定性）不应与 plan 模式的观感捆绑评估。**未启用 feature 时逐字节不变 ⇒ 指纹锚不动。**

### ② plan 模式 v1

模式状态（进程级）+ `ToolContext` 门禁 + `/plan` / `/yolo` + `EnterPlanMode` / `ExitPlanMode` 两个工具 + 状态栏 + 必需项 1/2。

复用既有先例：`tool_budget` 的挂载点形状、`LocalAction::Thinking` 的命令形状、`status_text` 的 `segs` 优先级、`thinking_visible` 的进程级状态。

两个新概念，都在决策 4：

- **`PlanMode`** —— 第一个**可变**挂载点（前四个都是只读能力或 `Copy` 值）；
- **`PlanPrompt` 第四通道** —— 回合内工具与 UI 之间的问 / 答。它绕开了 `Request` 只在回合边界消费的契约，故必须是独立信道而非复用 `Request`。

### 不做的：通用审批 UI

`ApprovalHandler` 仍注 `AutoApprove`。第四通道与它形状相近（都是 app → UI → app），但本轮只为 `ExitPlanMode` 建，不顺手把 `request_approval` 接上——那会牵出「哪些工具需要审批」「超时阈值」「拒绝文案」一整串独立决策，与本需求无关。
---

## 后续建议

- **plan 模式的独立价值收敛为「主 agent 规划期防误写」**，不含「规划能力」增益，也不是安全边界（`General` 子 agent 可绕过）。依据：Aider 实测同模型跑两次 77.4% → 80.5%（`docs/arch/subagent/research.md:151`）⇒ 收益部分来自「多一次推理」而非角色分工；而用户已在用 `arch-explore → arch-design → sequential-workflow` 的先规划后开发流程。另，多阶段机制有固定成本——小任务派发比父 agent 自己做慢 2.7×（37s vs 100s，`docs/design-final/subagent-explore.md:70-80`）。
- **为决策 6 写一条变异测试**：删掉「降级告知」后必须有用例变红。
- **为「`Plan` 不传染子 agent」写一条显式断言**：`General` 子表在 `Plan` 下仍含 `write`/`edit`。这条边界若无声退化，将来读代码的人会以为 `Plan` 是全覆盖的。
- **为第四通道写三条变异**：① 应答 sender 被 drop ⇒ 工具必须返回 `switched: false`（不得 panic、不得挂死）；② 应答丢失 ⇒ 门禁不得因此放行；③ 删掉 `submit` 里的前置分支 ⇒「答 `yes` 退出」用例变红。
- **`PlanPrompt` 不需要新超时机制**：工具调用已有外层预算（`basic.rs:456` 的 `tokio::time::timeout(budget, tool.call(..))`，默认 300s，`Tool::timeout()` 可覆盖）。`ExitPlanMode` 只要**声明**一个比默认值大的 `timeout()`（例如 3600s）即可——等待被人为拉长，不必另造定时器。UI 线程崩溃时 oneshot 被 drop 同样有解。
