# turn-reliability-and-lean-context — 架构上下文

> 日期：2026-10-13
> 性质：arch-explore 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「看看如何修复，并避免产生类似问题。我希望 agent 能够稳定的解决工作，
> 并减少不必要的工具调用、文件上下文等。」触发材料是一次真实失败会话
> `~/.yushan/sessions/1791821478_689435000.jsonl`（`/diagnose` 一个 TUI bug，两个回合后静默停摆）。
> 相关既有上下文：[`../tool-call-efficiency/context.md`](../tool-call-efficiency/context.md)（工具调用形态）、
> [`../llm-api-unavailable/context.md`](../llm-api-unavailable/context.md)（传输层故障族）、
> [`../turn-output-and-feedback/context.md`](../turn-output-and-feedback/context.md)（输出规模与可见性）。
> 本文件不复述它们，只写**它们没覆盖**的那一层。

## 概述

把一次真实失败拆成两件事，并界定各自的落点：

- **A · 回合可靠性**：回合在「工作没做完」时停下来时，系统**必须有界地接手**，
  而不是把 `NoOutput` 显示一行就收工。当前三档完全不同的情况被压成同一档终止事件
  （模型没说话 / 传输把答复丢了 / 输出预算被截断），且**没有一档会自动接力**。
- **B · 工具与上下文节制**：一次 13 轮的探索把 **92% 的会话 token 花在工具输出上**
  （156 KB ≈ 51.4k est token，而人说的话只有 ~4.4k），却一次 `edit`/`build`/`test` 都没做。
  现有闸门全是**单条结果上限**，**没有回合级总量**；压缩阈值远在其后（`window × 0.85`）。

## 现状（问题证据）

### 采样：`1791821478_689435000`（本机真实会话，只读统计）

| 指标 | 值 |
|---|---|
| 回合 | 2（turn 1 = 13 轮 / 170 s；turn 2「汇报状态」= 1 轮 / 38 s） |
| 工具调用 | 21 条结果（`read` 7 / `grep` 10 / `bash` 2 / `find` 1 / `ls` 1） |
| 工具结果总量 | **156,073 B ≈ 51,371 est token** |
| 整会话正文 est token | ≈ 55,824（⇒ **工具输出占 92%**） |
| 单条最大结果 | 25.9 KiB（`grep -C 8` 文档）、19.0 KiB（`read` 整文件）、16.9 / 16.7 / 16.5 KiB |
| `edit` / `write` / `bash` 构建测试 | **0 / 0 / 0** |
| 终止形态 | 两条 assistant 消息 `content: []`（第 26、28 条），`StopReason` 无从从文件判别 |

> 单会话口径，只作定性依据；结论不依赖具体比例，依赖「**工具输出吞掉九成上下文**」与
> 「**13 轮零编辑**」这两个形态。

### 复现实验（2026-10-13，只读重放 + 直连网关）

1. **同会话重放**（`--json --resume=<id> "汇报状态"`，会话副本落在 `/tmp/ysprobe`）：
   **正常收工** —— `Completed`、`input_tokens=60,868`、`output_tokens=2,405`、1 轮、23 s。
   ⇒ 不是上下文超限、不是这套输入系统性坏掉；失败是**偶发**的。
2. **原始 SSE 形状**（`curl` 直连）：健康流 = `reasoning_content` 增量 → `content` → 带
   `finish_reason:"stop"` + usage 的包 → usage 尾包（`choices: []`）→ `[DONE]`。
   同一请求 `completion_tokens=37`，其中 `reasoning_tokens=35` —— **输出预算几乎全花在思考上**。
3. **网关错误形状**：坏模型名回 **HTTP 400 + JSON 错误体**（非 200 垃圾体）。

### 四条根因（均可指到代码）

**R1 · 「零产出 + 零终止信号」被当成合法空响应。**
`feed_sse_bytes` 对非 `data:` 行一律跳过（`adapters/model-openai-compatible/src/stream.rs:67`），
流干净关闭（对端关连接、无 `[DONE]`）也返回 `Ok(())`；`StreamAccumulator::finish()`
只在 `error.is_some()` 时 `Err`（`.../lib.rs:719`），零 delta 于是收敛成 `Ok(空响应)`。
后果是**双向**的：上层显示成「模型无可见产出」（正常终止路径），而且**永不重试** ——
重试层只对 `Err` 生效（`.../lib.rs:295`，ADR-0028）。一次网关抖动 = 整个回合静默终结。
该缺口在 [`../llm-api-unavailable/context.md`](../llm-api-unavailable/context.md) 的现状矩阵里
**早已记录**（「200 + 非 SSE 错误体 ⇒ 静默成功，零输出 ❌」），只是从未落地。

**R2 · 有「单条上限」，没有「回合总量」。**
`read` 硬闸门 2000 行 / 50 KiB、`grep` 结果预算 16 KiB、软阈值 8 KiB
（`crates/ys-tool/src/truncate.rs:23-33`，`adapters/tools-basic/src/{read,grep}.rs`）——
本次 21 条结果**全部在各自上限之内**，所以一条都没被截断，也没有任何机制看见
「本回合已累计 51k token 的工具输出」。压缩（ADR-0017）要到 `window × trigger_ratio`
（默认 `0.85`，128k 窗口 ⇒ ~108k）才介入，远在其后。**软阈值只是一句话，模型可以无视**
（本次就是：5 条 ≥16 KiB 的结果之后它继续整文件读）。

**R3 · 「非收工原因」没有接力。**
唯一的自动追问只对 `Completed ∧ rounds ≥ REVIEW_MIN_ROUNDS` 开门
（`apps/coding-agent/src/follow_up.rs:38`，ADR-0027）。于是：
`NoOutput`（模型没说话）与 `Truncated`（输出被截断）都以「回合结束」收场，**不会自动续**。
同一文件里的 `MAX_CONSECUTIVE_ERRORS = 3` 是**空守卫**（`crates/ys-loop/src/basic.rs:634-641`
只有注释、无行为），`max_rounds` 在 app 侧默认 `u32::MAX`（ADR-0027 决策 1）——
即「原地打转」在代码层**没有任何防线**，只有 48 h wall-clock。

**R4 · 输出预算与推理型模型相乘。**
两个 model factory 都写死 `max_tokens: Some(4096)`（`apps/coding-agent/src/main.rs:1667,1706`），
而实测该模型 ~95% 的输出 token 花在 `reasoning_content`。思考吃满 4096 ⇒ 可见答案被截断
（`finish_reason=length` ⇒ 现走 `Truncated`），或干脆没有正文。这一档与「传输丢答复」
在**上层看起来同样是「回合没产出」**，但今天的处理路径完全不同（R1 那条连 `Err` 都不是）。

### 已经做过、不要重做的部分

- `bash` 的 cwd 契约已写进工具描述（`adapters/tools-basic/src/bash.rs:92-96`）；
- `read`/`grep`/`find`/`ls` 已声明 `is_concurrency_safe()`，批内全只读才并行
  （`apps/coding-agent/src/toolset.rs:504` 起的准入表测试）—— `tool-call-efficiency` 的 R1/R3 已闭合；
- 「先窄后宽」与「多目标 ⇒ 一条消息多个调用」两条 guideline 已在系统提示里
  （`apps/coding-agent/src/prompt.rs:187-209`）。
  ⇒ **本次要回答的不是「再加一句提示」，而是「提示不够时谁兜底」。**

## 现有架构

### 模块边界

```text
adapters/model-openai-compatible   流解析 + 收敛（StreamAccumulator::finish）+ 超时/有界重试
        │  Ok(ModelResponse) / Err(ModelError)
        ▼
crates/ys-loop/basic.rs            回合内推进：模型调用 → 工具批次 → 回喂；判 StopReason
        │  RunResult{ stop_reason, rounds, usage, final_message }
        ▼
apps/coding-agent/app_loop.rs      回合边界：stats / meta / follow-up（pending.take() 循环）
        │
        ├── follow_up.rs           唯一的「自动追问」纯函数（ADR-0027）
        └── prompt.rs / toolset.rs 工具表与行为纪律的唯一装配点
adapters/tools-basic               单条结果的截断与 notice
crates/ys-tool                     truncate / notice 的唯一实现（纯函数）
```

### 核心抽象

```rust
// 终止词汇（ys-core，#[non_exhaustive]）
enum StopReason { Completed, MaxRounds, Cancelled, Truncated, NoOutput }

// 适配器出口：零产出今天也是 Ok
fn finish(self) -> Result<ModelResponse, ModelError>;   // Err 仅当 error.is_some()

// app 侧唯一追问判据（纯函数，副作用全在 app_loop）
fn follow_up_for(kind: TurnKind, result: &RunResult) -> Option<AgentInput>;

// 工具面：单条结果的闸门（无回合级累计）
const MAX_LINES: usize = 2000; const MAX_BYTES: usize = 50 * 1024;
const SOFT_RESULT_BYTES: usize = 8 * 1024;   // grep 另有 16 KiB 专用预算
```

### 关键数据流（决定「改哪里会动到什么」）

```text
每轮：session.messages() 全量重发 ──► 模型 ──► 文本/工具调用 ──► 工具结果 append 回 session
                                                  │
                          回合收场 ──► StopReason ──► RunFinished ──► app_loop 决定是否追问
```

> **成本含义**（依据 [`../prompt-delivery/notes.md`](../prompt-delivery/notes.md)）：常驻稳定前缀
> （system / 工具表）缓存命中后 ≈ 0.1×，**加常量文字几乎免费**；`messages` **尾部**注入按 1.0× 付；
> 而**多一轮就是一次完整模型调用**（延迟 + 该轮全部新 token）。故「减少工具输出」的收益
> 不只是当轮体积 —— 它会**减少后续每一轮的重发量**（前缀增长）。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `reqwest`（流式）+ `tokio::time` | 传输、超时 | 超时/重试已在适配器（ADR-0028） |
| `rg` / `fd` | `grep` / `find` 后端 | 缺失降级 POSIX（ADR-0015） |
| 无新 crate | — | 本需求口径同 ADR-0015：不引依赖 |

## 约束

- **技术**：`ys-core` / `ys-event` / `ys-protocol` **不得**引入 tokio；`Model` trait 与
  `ModelEventSink`（同步 `fn`）不得改签名；不加 Hook（ADR-0021）。
- **契约**：终局事件不变式（ADR-0004 点 5）——新增任何失败路径必须**先发 `RunFailed` 再 `Err`**。
- **契约**：`FinishReason` / `ModelError` 的**方言解读归适配器**，`ys-loop` 不认识 provider 词汇。
- **契约**：工具参数面与描述是**对模型可见的契约**，改 `ToolSpec` 必须同步
  `prompt.rs` 的 snippet / guideline 与 `EXPECTED_ORDER` 漂移测试；
  子 agent 工具表必须是父表的**字节前缀**（缓存命中）。
- **成本**：不放宽任何输出上限；新增常驻文本要计入 `fixed_prompt_tokens`（参与压缩触发比较，
  小窗口下有「固定开销 ≥ 阈值 ⇒ 跳过压缩」的 R5 警告）。
- **性能**：超时值必须容纳长思考模型（实测首字节可达数十秒；现状 `idle_secs = 120`）。
- **演进**：`ModelError` **不是** `#[non_exhaustive]`（加变体是 breaking）；`FinishReason` 是。
- **组织**：小团队、无沙箱（安全外置）；关键逻辑必须有**变异测试**；测试隔离走
  `test_env::env_lock()` + 唯一临时目录（含 pid + 进程内原子序号）。

## 需求范围

### 范围内（候选，待澄清后取子集）

1. **零产出不是合法答复**（治 R1）：适配器把「零产出 + 零终止信号」判为 `Err`，
   交给既有有界重试（`max_attempts=5`、退避 500 ms–30 s）；**有终止信号**的空响应照旧 `Ok`
   （那是 `NoOutput`，属正常路径，不得误伤 —— `llm-api-unavailable` 场景 4）。
2. **非收工原因 ⇒ 有界接力**（治 R3）：把 `follow_up_for` 的门从「仅 `Completed`」
   扩到「`NoOutput` / `Truncated`」，各自一句专属常量文案，仍由 `TurnKind` 结构保证**每用户输入至多一个追问回合**。
3. **回合级工具预算**（治 R2）：给 `BasicLoop` 的回合内工具结果总量加一个可观测上限，
   超出后**收窄后续结果 + 明确回喂指引**（不是静默丢弃），并让该事实在事件/日志里可见。
4. **输出预算与推理分流**（治 R4）：核实 `max_tokens=4096` 与推理型模型的乘法关系，
   决定是抬预算、按模型给值，还是至少把「截断且零可见产出」并入第 2 条接力。
5. **可诊断性**：空产出 / 截断的现场证据（`finish_reason`、usage、是否见过 `[DONE]`）
   落一行日志 —— 本次排障就卡在「文件里看不出来」。

### 范围外（明确不做）

- 传输层三个缺口（超时 / 非 SSE 体 / 无 `[DONE]` 的完整性判定）—— 归
  [`../llm-api-unavailable/context.md`](../llm-api-unavailable/context.md)，本需求只碰它与
  `Ok/Err` 交界处那一格（第 1 条）。**不重复设计**。
- 加沙箱 / 权限 / 审批（项目硬约束）；引新依赖；持久 shell；改 `system` 做每轮注入。
- 放宽任何输出上限 / 放大单条结果（与 `turn-output-and-feedback` 的收紧口径一致）。
- 重排工具表、新增工具（先看能否用既有参数面解决；新增要走 ADR + `EXPECTED_ORDER`）。
- 把 `todo` 真值搬出来（ADR-0024 已定，尚未落地）—— 本需求只在「接力判据」上与它交界。

### 关键场景（成功路径）

- **S1 · 模型沉默**：某轮拿到零可见产出但**有**终止信号 ⇒ 回合判 `NoOutput` ⇒ 系统追加一条追补消息、
  模型接着干或收尾；用户看到的是「继续」，不是停摆。
- **S2 · 传输丢答复**：200 + 垃圾体 / 干净关闭的半截流 ⇒ 适配器 `Err` ⇒ 有界重试成功；
  重试耗尽才以 `RunFailed` 明确上报（不再是「模型没说话」）。
- **S3 · 探索期收窄**：一次探索（`/diagnose` 这类）不再把 50k token 的工具输出堆进会话；
  超出回合预算时结果被收窄并附「下一步怎么拿得更准」的指引。
- **S4 · 长任务接力**：回合因截断/无产出停下时，系统有界接力一次，而不是等用户敲「继续」。

### 异常 / 边界场景

- **合法空回复**（`finish_reason=stop` + 零内容）：必须仍是 `NoOutput`（可见），**不得**被第 1 条误判成故障。
- **推理型模型把预算烧在思考上**：可能表现为截断（`length`）或零正文 —— 两条路径都要有确定归宿。
- **只读并行与多目标**：不得为了「少调用」放宽批次准入（混入 `bash`/`write`/`edit` 必须退回串行）。
- **重试与已渲染输出**：一旦吐过 delta 不得重放（`DeltaWatchingSink`）；第 1 条的 `Err` 只可能发生在**零 delta** 时。
- **小窗口 + 大固定开销**：新增常驻文本必须仍满足 `fixed < trigger`（否则压缩空转，R5 告警）。

## 未澄清问题

- [ ] **Q1 · 范围取哪一半**：只做 A（回合可靠性：第 1/2/4/5 条），还是 A + B
      （工具与上下文节制：第 3 条）一起做？两半的验收口径与风险面不同。 待用户确认
- [ ] **Q2 · B 偏「轮次」还是「体积」**：减少**轮次**（多目标参数面 / 布局快照 ——
      [`../tool-call-efficiency/context.md`](../tool-call-efficiency/context.md) 的 Q2/Q3 仍开着），
      还是压缩**每轮体积**（回合级总量 + `read` 硬预算）？前者收益更大、动的是对模型可见的契约。 待用户确认
- [ ] **Q3 · 强制还是引导**：第 3 条要上**机制**（超预算即收窄/拒绝并要求收窄），
      还是继续只靠 guideline + 软提示？（本次证据显示软提示被无视。） 待用户确认
- [ ] **Q4 · 输出预算**：`max_tokens=4096` 是抬、是按模型给，还是不动、只把「截断且零产出」并入接力？ 待用户确认
- [ ] **Q5 · 验收指标**：用「同 prompt 的工具输出总字节 / 轮数前后对照」，还是「无产出回合数 = 0」
      这类布尔断言？（本仓目前没有 A/B 基线，可用 `app_loop` 的 mock 脚手架 + 真实会话重放。） 待用户确认
- [ ] **Q6 · 目录名**：本文件目录 `turn-reliability-and-lean-context` 是否合适（可换名，不得覆盖既有目录）。 待用户确认

## 后续建议

- 建议用 `arch-design` 做方案设计：把 A 的第 1/2/4/5 条与 B 的第 3 条拆成**可独立落地的小切片**，
  每片给出「改哪里 + 测试 + 变异点」。A 的第 1/2 条已有可复用的测试脚手架
  （`adapters/model-openai-compatible/tests/retry.rs` 的回环 mock 服务器、`app_loop` 的 `MockModel` + 队列长度钉死）。
- 建议 A 先落地（缺陷性质、改动小、可变异测），B 的形态先用 `prototype` 量一版
  「同一 prompt 的工具输出总字节 / 轮数」再定预算 —— 否则容易做出「测试全绿但把正常探索也掐了」的版本。
- 第 1 条落地后，用 `/tmp/ysprobe/1791821478_689435000.jsonl` 做一次真实重放回归
  （同一会话 + 同一 prompt），作为端到端证据。
