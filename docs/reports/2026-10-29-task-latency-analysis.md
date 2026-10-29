# 任务完成时长归因：为什么 ys-coding-agent 比 Codex / Claude Code 慢

- **日期**：2026-10-29
- **性质**：**调研记录**（只读）。本文不改代码、不改配置，只把「时间花在哪」用真实日志与已有测量拆开。
- **样本限制**：真实日志只覆盖本机 `~/.yushan/logs` 的 5 个长会话（deepseek-v4.1-flash /
  opencode-go 为主，另 1 个 minimax），跨 harness 对照抄自仓库已有测量。样本量小，
  下文所有数字都应视为**方向性证据**，不是统计结论。

---

## 0. 结论

**慢的主因是「轮数 × 每轮固定延迟」，不是工具执行，也不是 harness 内部开销。**

一轮 = 一次完整的模型请求（全量 system + 工具表 + 历史）。真实会话里每轮中位
**4.5–5.8 s**，而每轮平均只产出 **1.1 个工具调用**。工具自身执行只占 1%–20%，
其余时间都在等模型。把轮数压下来（一次消息发多个工具、用结构化工具替代 `bash grep/sed`），
是唯一能直接砍掉大头时间的杠杆。

另有一个**用户可直接观察、但不改变主因排序**的现象：收尾阶段会重复做已经做过的验证
（要 `commit+push` 时先重跑测试、再做一轮 git 检查）。它每次只花几十秒，属于纯仪式性开销，
但会让「明明刚测完」的用户觉得慢，见 §6。

---

## 1. 数据来源

| 来源 | 用途 |
|---|---|
| `~/.yushan/logs/debug-*.jsonl`（`schema: yushan.debug/1`） | 逐轮 `model_attempt` / `round_usage` / `tool_call` / `tool_result`，用于时间分解 |
| `docs/arch/explore-speed/cost-model.md:205-220` | codex 同提示对照 |
| `docs/arch/execution-efficiency/context.md:122-132` | 同模型跨 harness 对照 |
| `docs/arch/session-latency/context.md` | 长会话时间账单、TTFT 斜率 |
| `docs/arch/effort-per-call-type/context.md`、`docs/adr/0060-thinking-policy-per-call-kind.md` | reasoning / thinking 的实测影响 |

统计口径：`model_attempt.duration_ms` 为一轮模型调用耗时；`ttfb_ms` 为首字节；
`tool_result.duration_ms` 为工具执行耗时。子 agent 的模型调用与主循环写在同一个
debug 文件里（`run=0`），会与主循环时间轴重叠，故本文把「与 `round_usage` 配对的
attempt」记作主循环时间，不与子 agent 相加。

---

## 2. 真实会话的时间分解

以会话 `1793181762_239659000`（deepseek-v4.1-flash / opencode-go / effort=high，24 个 turn）为例：

| 指标 | 数值 |
|---|---|
| 主循环轮数 / 工具调用 | **555 轮 / 626 次** |
| 主循环模型耗时 | **54.0 min**（平均 **5.8 s/轮**） |
| 工具执行总耗时 | **15.2 min**（中位 **10 ms**） |
| TTFT 中位 | **2.23 s** |
| 模型单轮耗时中位 / 最大 | **4.49 s / 188.3 s** |
| 每轮工具调用数 | **1.13**；526 个有工具的轮里 **426 轮恰好 1 个**，100 轮 2 个，没有 3 个以上 |
| 输入 token | 合计 **1.38 亿**；每轮中位 **254,313**，最大 **418,613** |
| 缓存命中 | **99.3%** |
| reasoning / output | **27.5%**（67,760 / 246,609） |

另外三个会话的形态一致：

| 会话 | 轮数 | 工具 | 工具/轮 | 每轮输入中位 | 模型 total | 工具 total |
|---|---:|---:|---:|---:|---:|---:|
| `1793074808_399173000`（minimax） | 341 | 386 | 1.13 | 230,701 | 69.2 min | 3.4 min |
| `1793151820_034032000`（deepseek） | 403 | 445 | 1.10 | 205,309 | 84.5 min | 32.0 min |
| `1793181762_239659000`（deepseek） | 555 | 626 | 1.13 | 254,313 | 153.8 min* | 15.2 min |

\* 该行模型 total 含子 agent 调用（998 次 attempt），与主循环时间轴重叠；主循环配对
时间为 54.0 min。其余两行同样含子 agent，仅作量级参考。

**长尾**：同一会话里 3 个 turn（166 / 93 / 65 轮）占掉 **54%** 的主循环模型时间。
166 轮的那个 turn 里，170 次 `bash` 中有大量逐文件 `grep` / `sed -n` 探测。

---

## 3. 轮数为什么多

### 3.1 每轮只发一个工具调用

loop 本身支持「同一条 assistant 消息多个工具、只读段并发执行（上限 4）」
（`crates/ys-loop/src/basic.rs:336-363`、`:1112-1267`；`crates/ys-component/src/limits.rs:9`）。
但真实日志里 81% 的轮只有 1 个调用。模型没有用上这个能力，提示词里的指引
（`apps/coding-agent/src/prompt.rs:228-245`）执行率很低。

对照 codex：同一条 `t4` 提示，codex 是 33 次工具 / ≈6 轮（≈5.5 个/轮），
本仓是 60 次 / 33 轮（≈1.8 个/轮）——**轮数差主要就是批处理差**
（`docs/arch/explore-speed/cost-model.md:205-220`）。

### 3.2 `bash` 被当成文件查看器

会话 `1793181762_239659000` 的 626 次调用中，`bash` 占 **575 次**：

| 用法 | 次数 |
|---|---:|
| `bash grep` | 230 |
| `bash sed -n` | 143 |
| `bash head/tail` | 67 |
| `bash cat` | 16 |
| `bash ls` | 12 |
| `bash git` | 24 |
| `bash awk` | 3 |
| 其他 `bash` | 80 |
| **结构化 `grep` 工具** | **1** |
| `read` | 16 |
| `edit` / `write` | 18 / 7 |

即 **456/626 = 73% 的调用是「用 shell 看一个文件片段」，每条命令独占一次完整模型往返**。
`bash` 还是非并发安全工具，会切断只读工具的并行段（`crates/ys-tool/src/lib.rs:46-55`；
`basic.rs:336-363`），所以一批里混一个 `bash` 就退化成串行。

粗算：这 456 次若按每次消息 4 个独立只读调用批处理，约 114 轮，可省 342 轮 × 5.8 s ≈ **33 min**
（该会话主循环 54 min）。这是上界估算，实际受调用间依赖限制。

### 3.3 子 agent 轮询与同步等待

- `subagent_async` 的工具描述明确写「结果会自动送达，**不需要轮询**」
  （`apps/coding-agent/src/subagent.rs:1534-1538`），但日志里仍出现
  `sleep 1; git status` 反复探测。
- 父回合在给出最终答复后，若还有子 agent 在跑，会最多再等 **30 min**
  （`crates/ys-loop/src/basic.rs:983-1017`、`crates/ys-component/src/limits.rs:163-165`）。
- 仓库已有测量：14 次同步 subagent 累计 **3,605 s**，中位 238 s，最大 965 s
  （`docs/arch/session-latency/context.md:282`）。

---

## 4. 跨 harness 对照

| 对照 | 墙钟 | 轮数 | 累计输入 token | 备注 |
|---|---:|---:|---:|---|
| codex 0.160.1（`t4`） | **128 s** | **≈6** | **236,411** | 同提示，各跑 1 次 |
| 本仓（`t4`） | **291 s** | **33** | **1,286,802** | 同上 |
| 另一 harness（minimax-m3.1-flash） | **90.4 s** | **13** | 398,739 | 同模型、同网关、同工作区、同提示 |
| 本仓（minimax-m3.1-flash） | **440.2 s** | **21** | 404,367 | 生成吞吐 **43.0 vs ≥76.8 tok/s** |

出处：`docs/arch/explore-speed/cost-model.md:205-220`、
`docs/arch/execution-efficiency/context.md:122-132`。

两个对照指向同一个结论：**每请求输入 token 基本相同（≈39k），累计输入差 5.4×，
差距来自轮数；同模型下还有约 1.8× 的生成吞吐差**。

---

## 5. 原因清单（按影响排序）

### 大

1. **模型一次只发一个工具调用** —— 81% 的轮只有 1 个；并行能力存在但几乎没用上。
2. **`bash` 代替结构化工具做文件查看** —— 73% 的调用是逐条 shell 探测，放大轮数并破坏并行段。
3. **每轮固定成本高** —— TTFT 中位 2.2 s + 生成约 2 s；每轮重发完整 system + 12 个工具 schema
   + 全量历史（`crates/ys-loop/src/basic.rs:634-666`）。缓存命中 99%，但 TTFT 随上下文
   从 ~2.1 s 涨到 ~3.5 s（40 万 token），斜率约 76.7 ms/1k token
   （`docs/arch/session-latency/context.md:133-142`）。

### 中

4. **自动压缩几乎不触发** —— `context_window` 默认声明 **1,000,000**
   （`crates/ys-component/src/limits.rs:153`），`CompactPolicy` 触发点 85%
   （`crates/ys-component/src/compact.rs:67-75`）⇒ **85 万 token 才压**；回合级工具结果降级
   阈值是窗口/4 ⇒ **25 万才降**（`apps/coding-agent/src/main.rs:1053`）。实测会话长到
   41.8 万 token 仍未压缩。后果是每轮持续为 20–40 万 token 付 prefill 延迟与费用。
5. **reasoning effort 默认 `high`** —— reasoning 占 output 27%–53%；TTFT 里包含思考时间。
   已有 A/B：`daily/2` 关思考轮数 10→5、墙钟 −37%；摘要类调用关思考墙钟 −28%~−42%；
   但 `fact/1` 出现过一次质量退化（`docs/adr/0060-thinking-policy-per-call-kind.md`、
   `docs/arch/effort-per-call-type/context.md`）。是杠杆，不是错误默认。
6. **探索型回合失控** —— 单个 turn 65–166 轮，3 个 turn 占 54% 主循环时间。
   `max_rounds` 实际为 `u32::MAX`（`run_start.limits.max_rounds=4294967295`），没有轮数护栏。

### 小（长会话会放大，但不值得先动）

7. 会话 JSONL **每次 append 全量重写整个文件**，O(n²)（`crates/ys-session/src/jsonl.rs:108-170`）。
8. `read` 即使给 `offset/limit` 也先读入整个文件（`adapters/tools-basic/src/read.rs:80-102`）。
9. 普通日志同步 `std::fs`，每行 open/write/close，运行在 async 线程（`apps/coding-agent/src/logging.rs:84-100`）。
10. 工具参数不流式，必须等完整 SSE 结束才执行工具（`adapters/model-openai-compatible/src/lib.rs:1066-1071`）。
11. 空响应重试默认开启，会白跑一整轮（`crates/ys-component/src/limits.rs:161-167`、`crates/ys-loop/src/basic.rs:957-973`）。
12. 事件信道有界背压，UI 消费慢会反压到工具执行（`apps/coding-agent/src/channel.rs:242-275`）。

### 非 runtime 因素

13. **工作流形态**：`sequential-workflow` 要求实现全经 subagent 派发，且带 plan / 验收 / 归档。
    同一个需求，Claude Code 直接改文件时，本仓可能先走 arch-explore → design → plan → 派发 → 验收。
    用户感知的「任务更久」里有一部分是**任务形态差异**，不是执行引擎低效。
14. **收尾阶段的重复验证**：用户要 `commit+push` 时先重跑刚跑过的测试、再做一轮 git 检查。
    单次 30–50 s，零信息增量；机制与实例见 §6。

---

## 6. 收尾阶段的重复验证（「要 commit，却先测试一遍」）

### 6.1 现象与实例

用户要求纯机械的 `commit+push` 时，模型仍会先跑测试 / 再做一轮 git 检查，即使**上一轮刚跑完并汇报过**。
真实会话里的三个例子：

**会话 `1793181762_239659000` turn 25**（用户原话：「合并成一个 commit 直接 push」）：

| 轮 | 动作 |
|---|---|
| r1 | `git status --short` + `git remote -v` + `git branch -vv` |
| r2 | `git branch --show-current` + `git status -sb` |
| r3 | `git commit` |
| r4 | **`./scripts/cargo-build.sh --tests`**（提交之后、推送之前） |
| r5 | **`./scripts/par-test.sh` + `cargo clippy`**（同上） |

该 turn 模型 25.4 s + 工具 12.1 s；其中 r4/r5 的验证发生在 commit 之后，对「提交」这个动作没有信息增量
（若真有问题，此时已产生一个坏提交）。turn 26 的 `git push` 之后又做了 4 轮 `git status` / `git stash` 检查
（模型 41.9 s + 工具 9.2 s）。

**会话 `1793247213_415077000` turn 7–8**（用户原话：「commit+push」）：
turn 7 连续 3 轮只做 git 元信息检查（`branch --show-current`、`upstream`、`log`、`remote`、`rev-list --left-right`），
没有任何写操作；turn 8 才 `diff --stat` → `add` → `commit` → `push` → `status`。

**会话 `1793151820_034032000`**：用户不得不在后续回合补一句「**直接 commit+psuh，不用再测试了**」
（turn 20）——说明默认行为确实会重复测试，且显式指令可以立即纠正。

### 6.2 来源分析

1. **模型自身的「不可逆动作前先验证」习惯**。仓库里**没有**对应指令：`apps/coding-agent/src/prompt.rs`
   的 guidelines 不含 commit / push / 提交前验证条目，`AGENTS.md` 与 `CONTRIBUTING.md` 也没有提交流程约定
   （grep 均无命中）。所以这是模型默认倾向，不是本仓提示词要求的。
2. **workflow skill 的仪式被带到了临时请求**。`skills/sequential-workflow/SKILL.md:154` 要求 Phase 5
   「跑全量测试确认通过」，`:129` 要求 review 逐条核对；当会话上下文里带着这套工作流时，模型会把
   「实现任务的收尾」与「用户临时要的 git 操作」混为一谈。
3. **没有「证据时效」概念**。loop 里没有任何「上次验证后的树状态」记录（例如 tree hash + 测试命令 + 结果），
   模型只能靠对话历史自行判断「刚才测过、之后没改」。一旦历史里的测试结果被回合级工具结果降级
   （`max_turn_tool_result_tokens = 窗口/4`，`apps/coding-agent/src/main.rs:1053`）或压缩改写，
   「刚才测过」这条证据本身就变弱了，重跑反而成了最省事的做法。
4. **确认范围与重新验证被混在一起**。`git status` / `git diff --stat` 是廉价的「确认改了什么」，
   本身合理；问题在于它常与「重跑全量测试」串在同一条链上，后者对已汇报过的结果没有增量。

### 6.3 成本与可行的收口方式

观察到的单次额外开销约 **30–50 s**（5 轮左右模型往返 + 一次测试）；若碰上完整 `build --tests` + `par-test`，
可到 **1–2 min**。相对主因（轮数）不大，但它是**零信息增量**的等待，对体感影响被放大。

可考虑的方向（本文不实施）：

- 在 guidelines 里加一条明确边界：用户要求的纯 git 操作，若工作区自上次验证后未变，复用最近一次
  验证结果，只做 `git status` / `git diff --stat` 确认范围，不重跑测试。
- 把 workflow skill 的 Phase 4/5 验证限定在「由该 workflow 驱动的实现任务」，不适用于用户临时插入的
  commit / push。
- 若要更结构化：记录「最近一次成功验证 = 命令 + 结果 + 树状态」，作为可复用证据暴露给模型；这需要
  单独设计，不适合作为提示词补丁硬塞。

---

## 7. 尚未解释的空白

- **生成吞吐 43.0 vs ≥76.8 tok/s**：同模型、同网关、同提示，差距为何存在未隔离。
  可能是流式消费/usage 口径、provider 侧调度、或 harness 的等待点，需要单独实验。
- **没有 Claude Code 的性能对照**：仓库只有机制对照（`docs/reports/2026-10-27-reasoning-effort-comparison.md:152`
  明确写「未覆盖 Claude Code」）。与 Codex 的对照也只有各 1 次运行。

---

## 8. 建议的验证实验（不改产品代码）

1. **压轮数 A/B**：用 `e2e_bench` 的 `fact/1` / `exp/t1` 固定提示，比较
   「一次消息发多个 `read`/`grep`、禁止用 bash 查文件」与现状的轮数 / 墙钟。
   这是唯一能直接砍掉 2/3 时间的杠杆。
2. **按真实窗口配置压缩**：把 `context_window` 从 1M 调到一个实际可用值（或给压缩单独设
   触发阈值），观察长会话的 TTFT 与轮数。
3. **常驻指标**：运行结束时打印 `rounds`、`tools/round`、`ttft`、`gen`、`context tokens`，
   让「感觉慢」可归因（debug 日志已有全部数据）。
4. **隔离吞吐差**：同一模型在同一提示下，只替换流式消费路径，测生成吞吐是否仍是 43 tok/s。

---

## 9. 保留意见

- 真实日志的样本是 5 个长会话，且任务形态偏「大型功能开发 + 仓库自带 skill 工作流」，
  与「Claude Code 直接完成一个小任务」不可直接比较。
- `t4` 两次运行轮数方差极大（33 vs 246 轮，见 `docs/arch/explore-speed/cost-model.md:490-492`），
  单次对照只能看方向。
- 本文的 33 min 节省估算是上界，假设只读调用可独立批处理；有依赖的探测无法合并。
