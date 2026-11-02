# Subagent 预算与收敛 — 架构上下文

> 产出：`/arch-explore`（2026-10-31）。
> **需求原话**：「考虑一下收缩 subagent 的执行时间和轮数。每次执行很不受控。其他产品怎么做的？」
> 后续补充原话：「我觉得可以执行一段时间，就提醒 subagent 尽快结束，比如执行 5 分钟或者 16 轮之后？」、
> 「中位数 47 轮的话，那应该在 50 轮左右提醒收敛？毕竟已经足够久了」
>
> 本文只记录**现状基线、证据、约束与范围**，不做方案决策。方案交 `arch-design`。
>
> **与既有文档的关系**：`../subagent/`（ADR-0022）是派发能力的权威设计；
> `../explore-speed/` 讨论探索成本；`docs/design-final/subagent-explore.md` 记录 `explore` 模式的
> as-built。**本文不覆盖上述任一份**，只处理「子 agent 跑多久、什么时候该收手」这一个横切问题。

---

## 概述

`General` 模式的子 agent **没有任何轮数上限、也没有墙钟上限**，唯一兜底是 48 h 的回合预算。
实测中位数已经达到 49 轮 / 483 s，最长 116 轮 / 3004 s。需求是给它加上「跑一段就该收手」的约束，
并评估其他产品（Claude Code / OpenAI Agents SDK / LangGraph）的做法是否值得借鉴。

---

## 现有架构

### 预算组装链

```
Config::max_rounds()        = u32::MAX（缺省，config.rs:416）
Config::max_turn_duration() = 48h（缺省，config.rs:422-424）
        │
        ├──> AgentFactory::child_budget = config.max_turn_duration()   (assembly.rs:164)
        └──> AgentFactory::limits       （父的 RunLimits）
                    │
                    └──> child_limits = limits.clone()                  (assembly.rs:156)
                              │  只重算 fixed_prompt_tokens
                              └──> child_limits_for(model, mode)        (subagent.rs:1011)
                                        ├── Explore: 收紧 4 项
                                        └── General: **原样返回**
```

### 两种模式的实际差异

| 约束 | `Explore` | `General` | 指针 |
|---|---|---|---|
| `max_rounds` | `min(父, 16)` | **继承父 = `u32::MAX`** | `subagent.rs:1017` / `config.rs:416` |
| `wall_clock` | `300 s` | **`None`** | `subagent.rs:1018` |
| `final_report_grace` | `60 s` | `ZERO` | `subagent.rs:1019` |
| `final_report_prompt` | `Some(...)` | **`None`** | `subagent.rs:1020` |
| `soft_round_advice_at` | **继承 60（永不触发）** | 继承 60 | `assembly.rs:156`、`limits.rs:34` |
| `child_budget`（外闸基值） | `min(父, 300 s)` | 父值 = **48 h** | `subagent.rs:1039-1041` |

`SubagentMode` 只有两个变体（`crates/ys-tool/src/subagent.rs:18-24`），`General` 是 `#[default]`。

### 三个既有机制的语义（关键，易误解）

**1. `soft_round_advice_at` — 唯一的「软」机制**

- 纯提醒，**不终止**；注入一条 `User` 消息，不落 session（`basic.rs:821-828`）。
- 触发后阈值**翻倍**：`50 → 100 → 200`（`basic.rs:766-768`）⇒ 长任务梯度加压，短任务只收一次。
- 默认值 `DEFAULT_SOFT_ROUND_ADVICE_AT = 60`（`limits.rs:34`），app 从 config 覆盖（`main.rs:1150`）。
- ⚠️ **该默认值 `60` 没有任何依据记录**（`limits.rs:31-34` 只说「库面默认 `0`，app 侧设为本值」）。
  注意别混淆：`limits.rs:109` 那句「全程只多 3 条提醒」属于 [`narration_silence_tool_calls`]，
  **不是**本字段。
- 提醒原文（`basic.rs:78-80`）：
  > "this turn has run for many rounds without converging. If you are still exploring, stop and
  > report: what is done, what is blocked, and what you need from the user. Do not keep probing
  > without telling the user where things stand."

**2. `final_report_prompt` — 不是提醒，是「一次性硬终止」**

- 到点后**放行一轮**收卷轮，之后无论模型是否又发起工具调用都强制收场（`basic.rs:644-682`）。
- 注入文本含 `Stop investigating now — do not call any more tools. … This is your final answer.`
  （`basic.rs:783-795`，末句在 `:794`）。
- 保留原工具表是为了不破前缀缓存（`limits.rs:170-188`）。
- ⇒ **把它当「软提醒」用会直接把上限变成硬上限。** `Explore` 的 16 轮之所以「到点交卷而不是
  切断」，靠的就是这个机制。

**3. `wall_clock` — 或关系，独立于轮数**

- `deadline_reached` 先判轮数、再判墙钟（`basic.rs:136-153`）；二者语义不同：
  轮数约束 token 成本，墙钟约束「工具卡住 / 单轮特别慢」（`limits.rs:189-196`）。
- 墙钟到点归因为 `StopReason::Cancelled`，不是 `MaxRounds`（`basic.rs:659-663`）。

### 其他相关闸门

| 常量 | 值 | 作用 | 指针 |
|---|---|---|---|
| `OUTER_GRACE` | 60 s | 外闸 = `child_budget + 60 s`，只兜「连轮边界都到不了」的挂死 | `subagent.rs:60`、`:764-765` |
| `subagent_wait` | 30 min | 父回合为在跑子 agent 再挂多久（ADR-0055） | `config.rs:434` |
| `DEFAULT_MAX_DISPATCHES_PER_TURN` | 16 | 每回合派发次数上限 | `subagent.rs:34` |
| `MAX_PENDING_RESULTS` | 32 | 跨回合滞留结果上限 | `subagent.rs:67` |

---

## 实测证据

从 `~/.yushan/subagents/*.md` 的 38 份子 agent 转录提取（轮数 = `## Assistant` 段数，
耗时 = 转录尾部的 `elapsed_ms`）。

### 总体分布（29 次 `General` 成功派发）

```
轮数: 4 6 6 7 11 13 16 18 19 24 35 44 47 49 49 50 51 54 56 60 60 62 65 68 73 92 96 114 116
      中位 49 轮 / 483 s    均值 47 轮 / 644 s    最长 116 轮 / 3004 s
```

另有 3 次 `Explore` 撞满 16 轮（`stop_reason=max`）、5 次取消
（其中 2 次是 `cause=child-budget`、`elapsed_ms=300074/300078`），1 次用户取消。

### 关键观察 1：轮数与时长**不同轴**

| 转录 | 轮数 | 耗时 | s/轮 |
|---|---|---|---|
| `2959299_1` | 49 | 1526 s | **31.1** |
| `2959299_4` | 116 | 1041 s | 9.0 |

49 轮那次比 116 轮那次**慢 47%**。全体 s/轮 从 1.9 到 31.1 跨 16 倍。
⇒ **限制轮数不等于限制时长**，反之亦然。两者必须分别考虑。

### 关键观察 2：没有「模型空转」

主 agent 最长一段 run 是 347 轮 / 53568 s，**中位轮间隔 6.0 s**，全程在推进。
`General` 子 agent 也未观察到「长时间无任何活动」。⇒ 不存在「停滞」形态的失控。

### 关键观察 3：唯一的真挂死是工具不返回

`408698_1.md` / `408698_2.md` 各自 300 s 只完成 1 轮，全文只有两条 `rg`：

```
[tool_result ... is_error=true] aborted by user
[tool_result ... is_error=true] tool not executed: turn cancelled
## Run
stop_reason=cancelled  cause=child-budget  elapsed_ms=300074
```

任务本身是「搜索一个常量的使用点」，正常 2 s 可完成。
**是 `Explore` 的 300 s 硬墙钟把它们切掉的** —— 说明墙钟确实是唯一能抓住工具挂死的机制。

### 轮数上限的真实代价（仅计 29 次成功派发）

| 上限 | 被截断 | 具体 |
|---|---|---|
| 64 | **7 次（24%）** | 含 116 轮 / 384 KB 的方案 |
| 80 | 4 次 | 114、116、96、92 轮 |
| 100 | 2 次 | 114、116 轮 |
| **128** | **0 次** | — |

### 软提醒阈值命中率（仅计 29 次成功派发）

| 阈值 | 命中 | 触发时典型已耗时 |
|---|---|---|
| 16 轮 | 23/29（79%） | ≈148 s |
| 32 轮 | 19/29（65%） | ≈296 s |
| **50 轮** | **14/29（48%）** | **≈462 s** |
| 60 轮 | 10/29（34%） | ≈555 s |

---

## 其他产品的做法（调研）

> 数值来源：Claude Code 从 233 MB 单文件二进制提取字符串；OpenAI Agents SDK 与 LangGraph 从官方
> 源码读取。二者**均不在本工作区**，无法用 `文件:行号` 指针佐证，属调研结论而非本仓事实。

| 产品 | 轮数上限 | 时间约束 | 备注 |
|---|---|---|---|
| **OpenAI Agents SDK** | `DEFAULT_MAX_TURNS = 10` | 无 | 超限抛 `MaxTurnsExceeded`，可配 `error_handlers` 转受控输出 |
| **LangGraph** | `DEFAULT_RECURSION_LIMIT = 10007` | 无 | 定位是**图递归保护**，不是 agent 预算 |
| **Claude Code** `worker` / `fork` | `maxTurns: 200` | stall watchdog 600 s | — |
| **Claude Code** `Explore` / `Plan` / `general-purpose` | **未设**（继承默认） | 同上 | `Explore` 另用 `disallowedTools` 限制 |
| **Claude Code** 内部工具 | `maxTurns: 1` / `: 5` | — | compact / summary / extract_memories |

### Claude Code 的 stall watchdog（关键细节）

```js
h = Ge.CLAUDE_ASYNC_AGENT_STALL_TIMEOUT_MS || 600000   // 600 s
if (L.size > 0) {   // 有工具在飞
    "stall watchdog deferred — N tool(s) in flight"
    F(); return;    // 重新计时，不杀
}
"stall watchdog fired after {h}ms with no progress; aborting"
```

两个特征：
1. 它测的是**「多久没进展」**，不是「总共跑了多久」。
2. **有工具在飞时推迟并重新计时。**

### 对本仓的适用性判断

| 机制 | 判断 | 依据 |
|---|---|---|
| 轮数硬上限 | **部分适用** | 能填 `General` 的 `u32::MAX` 空洞，但 200 高于实测最高 116 轮，等于没限 |
| stall watchdog | **不适用** | 实测中位轮间隔 6 s、无空转 ⇒ 触发次数为零；且「有工具在飞就推迟」恰好放过 `bash` 挂死场景 |
| token 预算（`taskBudget`） | **不适用于时间诉求** | 用户抱怨的是时长，不是成本 |
| **按用途分档** | **值得借鉴** | `worker:200` / 内部工具 `:1` / `:5` —— 不同用途不同上限，而非一个全局数字 |

---

## 约束

**技术**

- `soft_round_advice_at` / `final_report_prompt` / `wall_clock` **三个机制都已存在**，本需求原则上
  只调参数与 `child_limits_for` 的分支，不需新增核心机制。
- `ys-component` 的 `RunLimits` 默认值改动会影响所有调用方 —— 既有字段默认值全部是
  「关闭 / 不干预」语义（`limits.rs:242-248`），新增或调整必须保持这条纪律。
- `child_limits_for` 的 `Explore` 分支已有「父配置更低时听父的」约定（`subagent.rs:1013-1021`），
  `General` 分支须保持同一语义。

**性能**

- 软提醒每条只多一次 `User` 消息（≈数十 token），阈值翻倍 ⇒ 长任务也只 2–3 条。
- 收卷轮多**一次**模型往返，仅在撞上限时发生；保留原工具表以命中前缀缓存。

**演进**

- `General` **不**加 `max_rounds` / `wall_clock` ⇒ 无行为变更风险，但也没有任何硬性兜底
  （`u32::MAX` + `None` 保持不变）。代价已接受：若软提醒失效，无界派发仍可能发生。

**组织**

- 用户明确偏好**最简**、反对过度设计、接受已知边界而非增加机制 ⇒ 优先调参，不引入新机制。

---

## 需求范围

### 范围内

- 让两种模式都收到**软收敛提醒**（`soft_round_advice_at`），且阈值与各自长度相称。
- 修 `Explore` 的软提醒死角（继承 60 > 硬上限 16 ⇒ 永不触发）。
- **`General` 保持无硬上限**（见下「已定决策」）。

### 已定决策：`General` 不设硬上限

用户已明确：**`General` 不加 `max_rounds`、不加 `wall_clock`**。

这使本需求的形态收缩为「**纯软提醒**」：约束全部来自提示词，不来自切断。

后果（设计时必须正面处理，不能当它不存在）：

- `General` 的 `max_rounds` 继续是 `u32::MAX`，`wall_clock` 继续是 `None` ⇒ 唯一兜底仍是
  `child_budget` = 48 h（`assembly.rs:164`）。这是**已经接受的已知边界**。
- 与「可控性」直接冲突：软提醒**不保证收敛**。若模型忽略提醒，还会出现
  `116 轮 / 3004 s` 这类派发，且**没有任何机制会阻止它**。
- 因此「软提醒 + 硬上限」的组合论证作废；只有「软提醒单独能不能起作用」成立。

### 范围外（明确不做）

- **`General` 不设硬上限** —— 轮数、墙钟均不加（见上面的已定决策）。
- **不引入 stall watchdog** —— 实测无空转，且其「工具在飞则推迟」的语义恰好放过本仓唯一的挂死形态。
- **不引入 token 预算（`taskBudget`）** —— 诉求是时长不是成本。
- **不改 `bash` 工具的超时行为** —— 工具挂死是独立问题（见未澄清问题 4）。
- **不改 `subagent_wait` / `max_turn_duration`** —— 二者是回合级兜底，与本需求正交。
- **不新增 `SubagentMode` 变体** —— 除非后续证明需要第三档。

### 关键场景

- 场景 1：一次正常规模的 `General` 派发（≈49 轮 / 483 s）—— 在第 50 轮前后收到一次软提醒，
  不被打断，正常完成。
- 场景 2：一次长任务（116 轮）—— 收到 50、100 两档提醒，仍可完成（无硬上限，不会误伤）。
- 场景 3：一次 `Explore` 派发 —— 10 轮收到提醒，16 轮进入收卷轮交出已找到的内容。
- 场景 4：工具挂死（如 `bash` 不返回）—— `General` 下**无任何机制介入**（靠 48 h 兜底）；
  `Explore` 下由 300 s 墙钟切掉。

### 异常/边界场景

- 父配置显式设了更低的上限 ⇒ 子 agent 必须听父的更低值（`General` 默认无上限，但父若显式设了
  `max_rounds` 仍会继承）。
- 软提醒阈值设 `0` ⇒ 关闭，行为逐字节不变。
- 模型忽略软提醒 ⇒ **无任何其他机制介入**（已接受）。

---

## 已定倾向

| 项 | 值 | 依据 | 状态 |
|---|---|---|---|
| `General` `soft_round_advice_at` | **50** | 中位 49 轮；命中 48%，触发时 ≈7.7 分钟 | 用户倾向 |
| `General` `max_rounds` | **不设**（保持 `u32::MAX`） | 用户决策：接受无界，靠软提醒 | **已定** |
| `Explore` `soft_round_advice_at` | **10** | 硬上限 16 的 2/3，留 6 轮收尾余量 | 提议，未定 |
| `General` `wall_clock` | **不设**（保持 `None`） | 用户决策同 `max_rounds` | **已定** |

已作废的候选（仅作追溯）：`General max_rounds = 128`（0 截断）、`= 64`（截断 7/29 成功派发）、
`maxTurns = 200`（高于实测最高 116）。三者均不再适用 —— **`General` 不加硬上限**。

---

## 未澄清问题

- [ ] **`soft_round_advice_at` 到底管不管用？** 本仓**没有任何实证**。`limits.rs:31-34` 只说明
      「库面默认 `0`、app 侧设为 60」，**连阈值 60 是怎么选的都没记录**。若无效，软提醒整条路径
      就是装饰。⇒ 在「`General` 不设硬上限」前提下，它**是唯一可能的约束机制**，
      失效则本需求整体落空。**这是本需求最大的未知。**
      （旁证不足：转录里 4 次恰好停在 60 轮，但同批也有 62/65/68/73 轮，无法区分是提醒生效还是
      模型自然收敛。）
- [ ] **`General` 不设硬上限后，「不受控」能改善多少？** 若软提醒无效，本需求整体无效。
      建议先实测：设 50、跑 3–5 个真实派发、看轮数分布是否左移，再决定是否推广。
- [ ] **`bash` 挂死是否高频？** `408698_1/2` 两次 300 s 卡死是偶发还是常态？若高频，它比调
      subagent 参数重要得多，优先级应前置。
- [ ] **是否需要第三档模式？** Claude Code 按用途分档（`worker:200` / 内部工具 `:1`、`:5`）。
      本仓目前 `General` / `Explore` 两档够不够，未评估。

---

## 后续建议

- 本需求形态已收敛为「**纯软提醒**」⇒ `arch-design` 的价值有限（实现只是
  `child_limits_for` 里两行常量赋值）。**建议直接进入实现**，但先面对未澄清问题 1。
- 验证软提醒有效性：把 `General` 阈值设 50，跑几个真实派发，对比轮数分布。
  这是区分「提醒生效」与「模型自然收敛」的唯一办法。
- 未澄清问题 2（`bash` 挂死）与本次改动正交，建议单独走 `diagnose`。
- 实现落点集中在 `apps/coding-agent/src/subagent.rs` 的 `child_limits_for`，
  不涉及 `crates/` 的默认值改动 ⇒ 改动面小。
