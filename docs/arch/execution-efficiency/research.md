# execution-efficiency — 同类系统调研

> 日期：2026-10-20
> 性质：**跨产品对照**（本仓约定：跨产品取舍对照放 `research.md`，`context.md` 只留结论与指针）。
> 调研目的：为 [`context.md`](./context.md) 的 **G1**（分钟级真命令阻塞 + 重复执行）找可借鉴的模式，
> 并识别**不能照搬**的地方。

---

## 0. 调研问题

「怎么让一条耗时几分钟的命令**不阻塞 agent loop**，而模型仍能在命令跑完时拿到结果。」

这不是 YuShan 独有的问题 —— 业界已有多种成熟解法，差别在于**触发方式**与**回收方式**两个轴。

---

## 1. 参照物清单

| 系统 | 进入后台的**触发** | 结果**回收**方式 | 进程清理 | 备注 |
|---|---|---|---|---|
| **Codex CLI** | agent 显式请求；前台执行返回 `Process running with session ID <N>` | **显式轮询**（`write_stdin` 带 session id） | — | 本仓有**一手运行记录**：见 §2.1 |
| **Claude Code** | Bash 工具 `run_in_background`；v2.1.x 起**默认全部异步**（无 opt-out） | **通知注入**（后台完成事件作为 system reminder 进入对话） | Ctrl+B 快捷键 + auto-cleanup + **5GB** 输出上限 | 另有「async/background mode for MCP tool calls」的公开 feature request —— 说明 MCP 侧**至今**还是阻塞的 |
| **OpenHands / Wizard（"Background tasks"）** | ① agent 显式要求；② **前台超时自动移交** | 任务注册表查询（状态 / 输出） | 超 `tools.background.max_timeout_seconds`（3600s）⇒ **杀进程组**并记 `timed_out` | 超时**不杀进程**，是「移交」不是「终止」 |
| **learn-claude-code s08 / s11** | 显式 `run_in_background` | **通知注入**：每次 LLM 调用前把完成结果注入对话历史 | 进程组清理 | 教学实现，把四个环节讲得最清楚：占位 `tool_result` 立即返回 `bg_id` → 完成队列 → 通知注入 → 进程组清理 |
| **pi**（本仓 TUI 的对标对象，`tmp/pi`） | **无** | — | 超时杀进程树（`killProcessTree`） | bash **纯同步**：`spawn` + timeout + kill，**无任何后台概念** |
| **Codewhale**（`tmp/Codewhale`） | 「lane」是 **worktree 并行单元**，不是后台 shell | — | — | 概念不适用 |

> **关键观察**：**YuShan 的对标对象 `pi` 在这一点上也是纯同步** ——
> 也就是说「后台执行」不是「跟上 pi」的问题，是**比 pi 更进一步**的问题。
> 走得最远的是 Codex（显式轮询）与 Claude Code（通知注入 + 默认异步）。

---

## 2. 一手证据

### 2.1 Codex 的显式轮询（本仓可直接复核）

来源：本机一次真实运行记录（同模型同 effort 同网关同工作区，与
`~/.yushan/sessions/1792506807_969975000.jsonl` 对照的那一次）。

```
① exec_command("cargo test --workspace 2>&1 | grep -E \"^(test result|error|...)\" | tail -40")
   → "Process running with session ID 26246"        ← 立即返回，不等
② 继续做别的探索（docs/arch/、design-plans、adr……共 17 次调用）
③ write_stdin(session_id=26246, …)                  ← 需要时才轮询
   → "test result: ok. 395 passed… / test result: FAILED. 0 passed; 4 failed…"
```

**三点观察**：

1. **启动不阻塞**：长命令立刻变成一个句柄，loop 继续推进 ⇒
   那 27s 的测试与 17 次探索**完全重叠**。
2. **回收是模型显式驱动**：模型**自己决定**什么时候去查。代价是
   **模型可能忘记查** —— 而这正是下面 M5 要治的。
3. **回收有成本**：本例只轮询了**一次**，且因为 `cargo` 默认 fail-fast，
   那次轮询拿回的是**失败中止**的片段，**工作区其余 target 根本没跑** ——
   模型据此写出「`cargo test --workspace` 全绿」这句**与自己的证据矛盾**的话。
   ⇒ **轮询的完整性问题会被放大成事实性断言错误**。

### 2.2 YuShan 本仓的对应失败模式

来源：同一个开发任务的 YuShan 会话。

```
round 10: bash("cargo test --workspace 2>&1 | tail -30")        31.3s  阻塞
round 11: bash("cargo test --workspace 2>&1 | grep -E … | awk …") 27.1s  阻塞（重复执行）
```

- **同一条命令跑两遍**：第一遍 `tail -30` 拿不到全局计数 ⇒ 模型补跑一遍带聚合过滤器。
  **58.4s = 墙钟 13.5%**，且这 58s 是**纯串行的**。
- 这不是「模型笨」—— 是**工具契约的必然**：一次调用只返回**一个**输出形态，
  想要另一个口径就得再跑一次。

---

## 3. 可借鉴的模式（提炼）

| # | 模式 | 出处 | 对 YuShan 的适配性 |
|---|---|---|---|
| **M1** | **三种进入后台的触发**：显式请求 / 前台超时自动移交 / 用户快捷键 | OpenHands、Claude Code、Codex | 「前台超时自动移交」**直接对症** YuShan：现有 `bash` 已有 `timeout` 参数与 300s 兜底，改成「到点不杀、移交」即可**零新增参数** |
| **M2** | **两种回收方式**：显式轮询（模型驱动） vs 通知注入（harness 驱动） | Codex / Claude Code | 显式轮询**适配 YuShan 的严格交替轮次**（工具是唯一通道，模型驱动一切）；通知注入需要 loop 支持回合内注入，**触碰「事件只观察」边界** |
| **M3** | **超时不杀进程，而是移交** | OpenHands | 直接可用：YuShan 现有 `timeout` 路径在到期时 `kill_process_group` + 返回错误；改成「移交后台 + 返回句柄」是**同一段代码的另一种收尾** |
| **M4** | **模型可声明期望时长**（`expected_seconds`），系统给 3×（下限 60s） | OpenHands | 适配：YuShan 的 `Tool::timeout()` 已经是「工具声明值优先」（`crates/ys-tool/src/lib.rs:36`），**同一机制** |
| **M5** | **禁止前台 sleep**：harness 拒绝模型用 `sleep` 等待，提示改用后台 + 通知 | "Background Work and Time"（Chapter 9） | 理由值得记下：「模型会轮询，因为训练数据教它轮询；harness 应挡住前台 sleep」。YuShan 现状：`sleep 30` 走 `bash` 且**真的睡满 30s** |
| **M6** | **后台事件是 system-attributed，不是用户消息**；带免责声明「这是自动后台事件，不是用户输入」 | Claude Code / feature request | 若选通知注入路线，**必须**区分「用户说的」与「系统注入的」—— YuShan 的消息模型里这两者的区分已有先例（`Boundary` vs 用户消息） |
| **M7** | **超时算失败**：命中上限 ⇒ 杀进程组 + 记 `timed_out`，且**要求模型如实报告为失败** | OpenHands | 直接可用，且是 YuShan `docs/TODO.md` §C `llm-api-unavailable` 同一类问题的反面教材（静默成功） |
| **M8** | **持久性警告**：fire-and-forget 会随 `/new` / 进程退出消失，**没有结果通道** | "A background leaf is not a job" | 直接命中 YuShan 的结构约束：工具表**进程级**、`/new` 只换 session ⇒ 后台进程的生命周期必须显式定义 |

---

## 4. 明确**不适用**之处

| 参照做法 | 为什么不能照搬到 YuShan |
|---|---|
| **Claude Code「默认全部 Bash 异步」** | YuShan 的 loop 是**严格交替**（`crates/ys-loop/src/basic.rs:599`）：模型 → 工具全部完成 → 回喂。**没有「工具未完成」这个状态**。全面异步化意味着要重写轮次模型，远超本轮范围。 |
| **通知注入（回合内插入消息）** | 触碰「**事件只观察，Hook 才控制流程**」的硬边界（`docs/design.md` 不变量）。要支持它，loop 需要一条「非工具来源的消息注入」通道 —— 这是**架构级**改动，不是工具层改动。 |
| **Codex 的 `write_stdin` 轮询语义照搬** | 它依赖 harness 提供**两个**工具（启动 + 轮询），且轮询是**模型的自由**。YuShan 若照搬，需要处理「模型忘记轮询 ⇒ 结果永远丢失」——Codex 也没解决（M2 的代价）。 |
| **`expected_seconds` 声明**（M4） | 模型对「这条命令要跑多久」的估计**不可靠**；OpenHands 用 3× + 60s 下限兜底。YuShan 可以先**不暴露**这个参数，保留固定超时。 |
| **任何「提高只读并发度」类做法** | `explore-speed/cost-model.md` §0 已证：本地只读工具是毫秒项，并行天花板 ≈ 0。 |

---

## 5. 给设计的输入（收敛成 6 条）

1. **进入后台至少要有「显式」与「超时自动移交」两条路**（M1/M3）——
   后者对 YuShan 尤其划算：现有 `timeout` 路径已经是「到点杀进程组」，改成「到点移交」是**同一段代码的另一种收尾**。
2. **回收方式必须在「显式轮询」与「通知注入」之间选**（M2）——
   这不是偏好问题：**通知注入要求 loop 有回合内注入通道**，那是架构级改动；
   显式轮询只要求工具面多一个能力。**成本差一个数量级。**
3. **工具契约的表达形态**（Q2）需要与回收方式**配套**：
   若选显式轮询，句柄必须是**模型能引用**的东西（一个 id + 一个可轮询的工具）。
4. **生命周期必须有明确边界**（M8）：工具表进程级 ⇒ 后台进程至少要区分
   「随会话结束」还是「随进程结束」，以及 abort 时如何清理。
5. **超时必须算失败且可如实报告**（M7）—— 与 YuShan 对「静默成功」的既有立场一致。
6. **禁止前台 sleep**（M5）是**低成本高收益**的一条：它不解决根因，但能立刻掐掉
   「模型用 `sleep` 假装在等」这一类墙钟浪费。

---

## 6. 采集口径说明

- Web 检索：`mmx search query`（2026-10-20），中英文各若干组关键词；结论**只采信能与源码/运行记录对上的部分**。
- 源码级参考：`tmp/pi`（TypeScript harness）、`tmp/Codewhale`（Rust agent）—— **均在 `tmp/`（已 gitignore）**，
  不是本仓正式依赖，引用仅作设计参考。
- 一手运行记录：本机 `~/.codex/sessions/2026/10/20/rollout-2026-10-20T22-41-51-01a159c2-*.jsonl`
  与 `~/.yushan/sessions/1792506807_969975000.jsonl` + `~/.yushan/logs/debug-1792506807_969975000.jsonl`。
  **跨 harness 的运行记录属外部数据，不作为本仓「当前实现」的事实来源** —— 本仓现状一律以 `code/` 为准。
