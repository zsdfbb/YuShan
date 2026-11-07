# 三类消费者的落点与状态

> 性质：`cross-turn-advance/` 的**索引层**。回答「follow-up / Loop / G4 各自的设计在哪、
> 现在什么状态、哪些文档的断言已过期」。
> 日期：2026-11-06

## 0. 为什么这三件事放在同一个目录

它们不是同一件事，但**共用同一个前提**：`app_loop` 的外层循环能在回合之外被唤醒。

| | 触发源 | 机制 | 成本 | 状态 |
|---|---|---|---|---|
| **Follow-up** | 回合结束 + HEAD 变化 | `Request::Prompt` 臂里顺序追加一个 `run_turn_chain` | 低 | 设计已修订，**代码零** |
| **Loop** | 时间到 | 第三条 `optional_wake` + 按时间排序的队列 | 低 | **无设计** |
| **G4（飞书）** | 进程外事件 | 归 `lark-agent-bridge`，本仓只是被 spawn 的子进程 | 低（不在本仓） | 桥接侧已生产可用 |

三者**都不需要** ADR-0047 的 `Boundary` 新格。ADR-0047 走的是另一条路（回合**中**投递 + 跨回合队列），
成本高一个数量级，**三者的真实用途都不依赖它**。

## 1. 各消费者的设计落点

| 消费者 | 设计文档 | 当前状态 |
|---|---|---|
| **Follow-up** | `design.md` §3（架构）+ `docs/design-plans/2026-09-15-follow-up.md`（实现细节） | 一期设计已完；**代码零命中** |
| **Loop** | `design.md` §4（形状已定，T1–T4 待定） | 二期；T1–T4 **未决策** |
| **G4** | 不在本仓 | 见 `context.md` §调查结论 4；用户已明确暂不考虑 webhook |

### 1.1 Follow-up 的现行设计（摘要，详情见 design plan）

- **形状**：`Request::Prompt` 臂内**两次顺序调用** `run_turn_chain`（不是循环、不是 `Boundary` 新格）
- **防套娃**：由**结构**保证——追问分支只出现一次，自己不再递归。**没有计数器、没有上限配置**
- **触发**：`git rev-parse HEAD` 回合前后不等，且只在**正常收场**（`StopReason::Completed`）后
- **呈现**：`Role::Harness`（照 `subagent.rs:1229` 既有约定）
- **前置改动**：`run_turn_chain` 需返回 `TurnOutcome` 而非 `()`（design plan §3.6）
- **固有限制**：只在「用户刚说完话」之后触发；`-p` / `--json` 永不触发（design plan §6.2）

## 2. 已过期的断言清单

2026-11-06 修订后，以下文档的断言**已不成立**。修这些是「后续一起做」的一部分。

| 文件 | 行 | 过期断言 | 应改成 |
|---|---|---|---|
| `arch/long-horizon-todo/context.md` | 94 | 「上限默认 **1**」 | 无上限配置；防套娃由结构保证 |
| 〃 | 107 | 落点 `2026-09-15-follow-up.md:49-63` | 行号已变（§3.1）；形状已换 |
| 〃 | 183 | 「护栏 ①，上限默认 1」 | 该护栏已不存在 |
| 〃 | 307 | 「`:88-93` 的三样护栏」 | 现为四项（新增「只在正常收场后追问」），行号已变 |
| `arch/long-horizon-todo/design.md` | 434 | 「`while pending.take()` 循环正是本设计的落点」 | **该循环已由 ADR-0041 删除**；落点改为「第四个 `run_turn_chain` 调用点」 |
| `arch/long-horizon-completion/context.md` | 91 | 落点 `:49-63` | 同 107 |
| `arch/long-horizon-completion/design.md` | 80 | 「§3.1 已给出缝的形状」 | 形状已换（顺序 `if`，非循环） |
| `arch/subagent-rebaseline/context.md` | 22 | 「follow-up 缝**已由 ADR-0027 落地**」+ `follow_up.rs` 存在 | **ADR-0027 决策 2 已被 ADR-0041 撤回**，该文件已删除 |
| `arch/hooks-and-plugins/notes.md` | 23 | 「§3.5 的扩展轴①」+ 三样护栏可继承 | 扩展轴仍在（现 §3.5），但护栏数变了 |

> `subagent-rebaseline/context.md:22` 那条**最误导**——它把「已落地」写成了既成事实，
> 而实际上实现被删了。

## 3. 与 ADR / TODO 的关系
| 文档 | 说法 | 现行判断 |
|---|---|---|
| ADR-0014 | 「只做 follow-up，不建 hook」 | **仍有效**，但**未提**骨架后来被删 |
| ADR-0027 决策 2 | 完工复核（`pending.take()` 循环） | **已被 ADR-0041 撤回** |
| ADR-0041 | 删 `follow_up.rs` + 收成单回合 | **仍有效**；但它撤的是**模型自述触发**的复核，**不撤销** ADR-0014 的 follow-up |
| ADR-0047 | 「follow-up 缺协议格」 | **对 Loop / G4 不成立**；对「回合外触发」才成立 |
| `TODO.md` §B | 「不是缺实现，是缺协议格」 | **只对「回合中投递」成立**；follow-up 的 `if` 追加不需要它 |

**核心澄清**：ADR-0041 撤掉的完工复核与 follow-up 的区别是**触发依据**——

| | 触发 | 依据 |
|---|---|---|
| ADR-0041 撤掉的 | 模型说「我做完了」 | 模型自述，不可验证 |
| Follow-up | HEAD 前后不等 | **外部可观测** |

## 4. 未决

- **Loop 要不要做、做成什么形状**——无设计，见 `context.md` 未澄清问题第 1–4 条
- **要不要建 `docs/arch/follow-up/`**——2026-11-06 决定**不建**，本目录承担该职责
  （理由：三者共用 `Boundary` 四格 / `select!` 四分支等前提，拆三份会重复记录）
