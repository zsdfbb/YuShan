# 跨回合推进：三家实现对照

> 性质：**竞品机制研究**（`docs/arch/**/research.md` 类产物，**不含决策**）。
> 要落地的项须另走 `arch-design`，决策进 `docs/adr/`。
> 日期：2026-11-06
> 对象与版本：
> - Claude Code 官方文档（`code.claude.com/docs`，2026-11-06 抓取）
> - CodeWhale：`tmp/Codewhale`（本地检出，未固定 commit —— 行号对该检出有效）
> - Codex：`tmp/codex` @ `7b54fd2`（2026-11-06）

---

## 0. 一句话

三家**都把「定时」与「条件推进」分开实现**，没有一家合成一个机制；
分歧点全在**「谁启动下一回合」**与**「谁判定完成」**这两个问题上。
YuShan 的 follow-up 设计在这两个问题上已经站在最省的一侧（宿主启动 + 外部事实判定），
缺的只是「回合结束时问一句要不要续」这个原语。

---

## 1. 三家总览

| | Claude Code | CodeWhale | Codex |
|---|---|---|---|
| **定时** | `/loop`（会话内投 prompt） | `automation_manager`（落盘后台任务） | **无** |
| **条件推进** | `/goal`（Stop hook + 小模型判） | `/goal`（模型调工具） | `/goal`（宿主驱动） |
| **通用干预点** | Stop hook（`exit 2`） | 无对等物 | 无对等物 |
| **外部事件** | Channels（MCP server） | 无 | cloud-tasks（云端异步） |

**结论一：`/loop` 与 `/goal` 不一起用。** Claude Code 文档标题即
*Compare ways to keep a session running*，判据是「什么启动下一回合」，三选一。

---

## 2. Claude Code

### 2.1 `/loop` —— 定时

两档：

| 档 | 行为 |
|---|---|
| 固定间隔 | `5m` / `every 2 hours` → cron 表达式。粒度 1 分钟；`7m` / `90m` 不成整的会就近取整并告知 |
| 自定节奏 | 省略间隔 ⇒ **模型自己定下次延迟**，`ScheduleWakeup { delaySeconds, reason, stop }`，钳制 `[1min, 1hour]`，依据观察到的状态（构建快完则短、PR 安静则长） |

要点：

- **终止**：模型 `stop: true`；若某轮既不定时也不停止 ⇒ 兜底 ~20 分钟后再跑一轮，仍不续期则结束
- **并发**：调度器每秒检查，但**只在回合之间触发**；繁忙时排队等当前回合结束；**错过不补**（missed fires 不逐次补跑）
- **上限**：7 天自动过期 + 单 session 最多 50 个任务
- **抖动**：`*/5` 提前 15 秒（正好卡在 5 分钟 prompt cache TTL 前），其它延后最多半个间隔、上限 30 分钟
- **默认提示词**：裸 `/loop` 跑内置 maintenance prompt（续未完的活 → 照顾当前分支的 PR → 空闲时清理），可被 `loop.md` 覆盖（`.claude/loop.md` > `~/.claude/loop.md`，25 KB 截断，改动下一轮生效）

### 2.2 `/goal` —— 条件

**`/goal` 是「session-scoped prompt-based Stop hook」的封装。**

每轮结束把「条件 + 对话」发给**配置的小快模型**判定，三态：

- **Not yet met** → 再跑一轮，理由作为指导传入
- **Met** → 结束
- **Impossible** → 结束

要点：

| 点 | 做法 |
|---|---|
| 判官 | 独立小模型（`ANTHROPIC_DEFAULT_HAIKU_MODEL`） |
| **判官不调工具** | 只能看对话里**已经露出的**证据 ⇒ 条件必须让主回合先摆出来 |
| 状态 | session 级；`/goal` 无参显示条件 / 耗时 / 评估轮数 / token 花费 / 最近理由 |
| 恢复 | `--resume` 恢复条件，但**轮数 / 计时 / token 基线重置** |
| 终止 | 达成 / 判不可达 / `/goal clear` / `/clear` / 不可恢复错误 / **自动重试三次后暂停** |

**`/goal` 内含一个心跳**：交互会话中若 Claude 空闲且后台还有活在跑，它**自己启动一个回合**
送 check-in，不等用户输入；间隔由 `CLAUDE_CODE_GOAL_CHECKIN_MINUTES` 控制（`0` 关闭），有退避。

⇒ 准确划分不是「一个定时、一个条件」，而是：**`/goal` 主路径是回合边界判定，定时只做空闲兜底。**

### 2.3 Stop hook

`exit 2` = block（继续），`exit 1` 不 block。官方文档自己承认「一个总是 block 的 `Stop` hook
会形成循环」。

**CLAUDE CODE 的防风靠「上限 + 兜底」**：7 天过期 / 三次重试 / `stop:true` / 兜底 20 分钟。

---

## 3. CodeWhale

### 3.1 `/goal` —— 模型调工具改状态

`tmp/Codewhale/crates/tui/src/tools/goal.rs`

- 文件头自述 **"Goal tools for the model-visible LLM-as-judge loop"**（`:1`）
- 模型可见工具：`create_goal` / `get_goal` / `update_goal`（`:982` / `:1048` / `:1102`）
- 状态机 `GoalStatus`：`Active / Paused / Complete / Blocked`（`:49-57`）
- 完成路径：模型调 `update_goal` → `mark_complete(evidence, verification)`（`:410`）

**completion gate**（`:410-436`）：

- 必须是 `GoalReviewRole::Critical` 的复核才允许关单，`Advisory` 不行（`:421`）
- 封印后不可替换（`:419` "already sealed and cannot be replaced"、`:567` `completion_sealed()`）
- 防停滞：`repeated_gap_count` + `continuation_count`，同一个 gap 反复出现则暂停；
  `record_not_achieved` 每 pass 最多计一次（否则单轮内重复报告同一 gap 会误暂停）

> **判官就是模型自己**——它通过 `update_goal` 写结论，只是外面套了一层复核角色与封印。

### 3.2 定时 —— `automation_manager`，不是 `/loop`

`tmp/Codewhale/crates/tui/src/automation_manager.rs`

- **durable**：落盘 `~/.codewhale/automations`（`DEEPSEEK_AUTOMATIONS_DIR` 可覆盖，`:5`）
- schedule 解析支持 cron / RRULE / 自然语言（`:426` `parse_cron_schedule`）
- 跑的是**后台任务**（"enqueue standard background tasks"，`:3`），**不是往当前会话投 prompt**
- 有 run history、archive（有界保留）、恢复（`overdue_recurring_schedule_coalesces_to_one_catch_up_run`）
- 创建需审批（`tools/automation.rs:138`）

**这是「任务队列 + 调度器」，与 Claude Code 的 `/loop` 不是一回事。**

### 3.3 两者不组合

`goal.rs` 与 `automation_manager.rs` 是两套独立代码，无交叉调用。

---

## 4. Codex

`tmp/codex/codex-rs/ext/goal/`，feature-gated（`Feature::Goals`）。

### 4.1 `/goal` —— 宿主驱动续跑

关键在 `runtime.rs:489-510`：

```rust
thread.start_turn_if_idle(
    TurnInputRequest::new(TurnInput::ResponseItem(item))  // continuation.md 模板
        .on_start(TurnStartOptions { turn_trigger: Some("goal".to_string()), .. })
)
```

| 点 | 做法 |
|---|---|
| **谁启动下一回合** | **宿主**（`start_turn_if_idle`），不是模型 |
| 下一回合输入 | 注入 `templates/goals/continuation.md`，带目标 + `tokens_used` / `token_budget` |
| 谁判定完成 | 模型调 `update_goal`，`status = Complete`（`tool.rs:312`） |
| 工具名 | `spec.rs:9-11` `get_goal` / `create_goal` / `update_goal` |
| 状态 | `Active / Paused / Blocked / UsageLimited / BudgetLimited / Complete`（`protocol.rs:4409`） |
| 状态存储 | **落库** `state_dbs.thread_goals()`，跨重启可恢复 |
| 预算 | `token_budget` + `tokens_used`；超限转 `BudgetLimited` 并注入 `budget_limit.md` |
| 忙时行为 | `NotSubmitted { reason }`（`runtime.rs:504`）⇒ **跳过，不排队** |

### 4.2 没有定时

`codex-rs` 内搜不到「定时往会话投 prompt」的机制；`schedule` 命中全是 TUI 帧调度
（`schedule_frame`）。`cloud-tasks` 是云端异步跑任务，性质同 CodeWhale 的 automation。

---

## 5. 核心对照：两个问题

### 5.1 谁启动下一回合

| | 做法 | 代价 |
|---|---|---|
| Claude Code `/goal` | 回合结束 = Stop hook 触发 | 需 hook 子系统 |
| CodeWhale | **模型调工具**推进 | 模型不主动就断了 |
| **Codex** | **宿主 `start_turn_if_idle`** | 无 —— 一个函数调用 |
| YuShan follow-up | harness 在 `Prompt` 臂追加 | 无 —— 已设计 |

### 5.2 谁判定完成

| | 判据 | 可信度 | 每轮成本 |
|---|---|---|---|
| Claude Code `/goal` | 独立小模型读对话 | 中（只能看已露出证据） | 一次模型调用 |
| CodeWhale | 模型自述 + completion gate | 低 → 靠大量代码兜 | 零 |
| Codex | 模型调 `update_goal` | 低（无 gate） | 零 |
| **YuShan follow-up** | **`git HEAD` 前后不等** | **高（外部可观测）** | **零** |

> CodeWhale 用 completion gate / fingerprint / gap 计数**大量代码去堵「模型说完成了」这个洞**。
> ADR-0041 的选择是**直接不让模型有这个机会** —— 这是两种应对同一风险的路线。

---

## 6. 可借鉴项

| # | 借鉴什么 | 出处 | 与 YuShan 现状的关系 |
|---|---|---|---|
| **L1** | **`start_turn_if_idle` 语义**：回合结束后问一句「现在能开新回合吗」，忙则跳过、不排队 | Codex `runtime.rs:491` | **最直接可落**。比 `select!` + 定时器简单，因为**不需要等待**，只在回合边界检查一次 |
| **L2** | 续跑输入用**模板 + 目标 + 已用预算**，非裸 prompt | Codex `continuation.md` | follow-up 的追问 prompt 可带「上次 HEAD / 本次 HEAD」做上下文 |
| **L3** | 目标状态**落库**，跨重启恢复 | Codex `thread_goals()` | YuShan 已无状态（ADR-0010）；若要做 `/goal` 形态，状态该进 session 还是独立表是**未决问题** |
| **L4** | `BudgetLimited` 作为**一等状态**，超预算注入专用提示词 | Codex `accounting.rs` | YuShan 有 `max_turn_duration`（默认 48h）与 token 预算，但**没有把「超预算」表达成可被模型看见的状态** |
| **L5** | 定时与条件分开实现，不合成一个 | 三家一致 | 支持 YuShan 分开做 follow-up（条件）与唤醒源（定时） |
| **L6** | 自定节奏（模型选下轮延迟） | CC `ScheduleWakeup` | 仅在真做定时后才相关；优先级最低 |

### 6.1 明确**不**借鉴

| # | 不借鉴什么 | 理由 |
|---|---|---|
| N1 | 模型可自判完成（CodeWhale / Codex） | ADR-0041 的方向相反：完工复核由模型自述触发已被证伪 |
| N2 | 独立小模型做判官（CC） | 每轮一次调用；YuShan 的 `HEAD` 判据零成本且更可信，先做便宜的那档 |
| N3 | 防风靠上限与兜底（CC Stop hook / 7 天过期） | YuShan follow-up「结构上只追加一次」更强 —— **无法通过配置调坏** |
| N4 | 落盘 automation 子系统（CodeWhale） | 本仓 `jsonl` 已落盘 + `--resume` 可接；再建一套是不必要重复 |
| N5 | 抖动 / 7 天过期 / 50 任务上限 | 这些是**多租户云端**问题（API 尖峰、遗忘的循环），本地单用户场景不成立 |

---

## 7. 对 YuShan 的三层划分（研究结论，非决策）

| 层 | 内容 | 依据 | 现状 |
|---|---|---|---|
| **唤醒** | 回合边界「问一句要不要续」（L1）；定时器**最后**做 | Codex `start_turn_if_idle` | 无 |
| **判据** | 第一档 `git HEAD` 差异（零成本）；第二档条件判官（贵，后做） | follow-up 已设计 / CC `/goal` | 有设计、无代码 |
| **动作** | `run_turn_chain` | 已有 | 现成 |

**关键约束**：定时器只负责**醒来**，不判「任务完没完」。判据与唤醒必须分层 ——
这正是三家一致的拆分方式（L5）。

**优先级**（按依赖与成本）：

1. 唤醒层（L1）—— 一个函数语义，覆盖 follow-up 与大部分 `/goal` 场景
2. 判据第一档（`HEAD`）—— 已有设计，零成本
3. 判据第二档（条件判官）—— 贵，等前两项跑起来再评估
4. 纯定时唤醒 —— 只服务「用户走了、无人触发」；优先级最低

---

## 8. 记录一处已纠正的错误

`cross-turn-advance/context.md:186` 曾写 Claude Code 的 `ScheduleWakeup` 有 **`noop` 字段**
（「把『这轮没变化』变成一等事实」）。**该字段不存在**：`ScheduleWakeup` 只有
`delaySeconds` / `reason` / `stop`，官方文档无 `noop`。

同处的「`isLoopSession` 会话被 `/resume` 过滤」亦无出处 —— Claude Code 文档只说
**self-paced `/loop` 不被 `--resume` 恢复**（cron 驱动的一般可恢复）。两者不是一回事。
