# ADR: 回合可靠性与工具输出节制（turn-reliability-and-lean-context）

## 状态

提议（2026-10-13；同日按竞品源码直读修订 —— 新增决策 3「零产出重采样」，见 §修订）

## 上下文

一次真实会话（`~/.yushan/sessions/1791821478_689435000.jsonl`）在 13 轮探索后静默停摆，
随后用户追问「汇报状态」再次静默 —— 两条 assistant 消息内容为空，日志里没有任何错误或重试。
同会话原样重放却正常收工 ⇒ 偶发。同一次会话把 **156 KB ≈ 51.4k est token** 的工具输出堆进上下文
（占整会话 92%），却一次 `edit`/`build`/`test` 都没做。

四条根因、约束与既有相邻范围见 [`context.md`](./context.md)；方案对比见 [`design.md`](./design.md)；
跨产品对照见 [`research.md`](./research.md)。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| A 最小复杂度 | 三处判据（适配器 `Err` + 追问扩门 + 循环侧常量阈值收窄） | 改动最小、零常驻成本、可变异测 | 收窄放循环侧会截掉 `bash` 的尾部错误；第五种原因要重踩读点 |
| B 可扩展优先 | `TurnHealth` + `relay_plan`/`narrow::plan` + `RunLimits` 两字段 | 收场语义收敛到单一落点，扩展 = 加一个 `match` 臂 | 本轮只收敛一个站点；退出码 / TUI 图标 / 子 agent 诊断一并收敛要动四个 crate |
| C 性能/资源优先 | 回合账本 + **工具侧**应用预算 + 落盘指针 + 新事件 | 减量最大、且不依赖模型合规 | 指针文案成为对模型可见的新契约；落盘目录 / 原子写 / 新事件面大；需先证明「收窄 + 指引」不足 |
| **融合（推荐）** | A 的判据与接力 + **零产出同回合重采样（A3）** + C 的「循环定价、工具收窄」分工（**去落盘**）+ B 的「预算默认关」纪律 | 判据清楚、零常驻成本、各工具保留自己的截断方向；重采样把「抽样退化」挡在终局之前 | 预算值与重采样次数需校准；`-p`/`--json` 不接力（记录在案） |

## 决策

### 1. 「零产出 + 零终止信号」是失败，不是空答复

- `adapters/model-openai-compatible/src/lib.rs` 的 `StreamAccumulator` 加 `saw_done: bool`
  （`StreamEvent::Done` 置位），并在 `finish()` 里（`error` 检查与 `filter.flush()` 之后）加判据：

  ```text
  正文空 ∧ 思考空 ∧ 工具调用空 ∧ finish_reason 未出现 ∧ [DONE] 未出现  ⇒ Err(ModelError::Provider)
  ```

- 命中即落入 **ADR-0028 的既有有界重试**（`max_attempts=5`，退避 500 ms–30 s）；
  重试时零 delta，故不触发 `DeltaWatchingSink` 的「已吐出内容不重试」守卫。
- **有任一终止信号**（`finish_reason` 或 `[DONE]`）即为「provider 明确说了：模型没有产出」⇒
  维持 `Ok` ⇒ 上层判 `StopReason::NoOutput`（**正常终止路径，不误伤**）。
- **判据取最保守的一档**：只有「连一个字节都没到」才算传输失败。思考到了、正文没到，仍算
  「模型没说话」（走第 2 条的接力），不判成传输故障 —— 适配器的辖区是传输，不是模型行为。
- **与 `llm-api-unavailable` 的分界**：本决策只取 `Ok/Err` 交界这一格。
  「无 `[DONE]` 的完整性判定」「非 SSE 响应体识别」「首字节 / 流空闲超时」仍归那条线，不在此实现。

### 2. 自动追问扩到「空产出」与「被截断」，结构不变

- `apps/coding-agent/src/follow_up.rs` 的 `follow_up_for` 加两个门：`StopReason::NoOutput` ⇒ 追补文案、
  `StopReason::Truncated` ⇒ 续写文案；**两者都不看 `rounds`**（它们正是「第 1 轮就停摆」的形态，
  加门槛等于不修）。`Completed ∧ rounds ≥ REVIEW_MIN_ROUNDS` 的复核门不变。
- **「至多一次」仍由 `TurnKind` 的二态结构保证**（ADR-0027）：追问回合（`FollowUp`）恒不再追问
  ⇒ 每次用户输入最多多出一个回合。
- 两句文案都是**纯常量**（无时间戳 / 回合号 / 计数），`Role::User`，经 `AgentInput::text` 走
  与普通回合同一条路径（因此在 transcript 可见）。
- `Cancelled` / `MaxRounds` **一律不追问** —— 用户中止与撞顶各有自己的可见信号。

### 3. 「完成但零可见产出」在同回合内重采样（≤ `EMPTY_RESAMPLE_ATTEMPTS`）

- 判据（`BasicLoop` Step 8）：`text.trim().is_empty() && tool_calls.is_empty()`
  **且** 本回合的重采样次数未用尽 ⇒ **不 append、不 emit**，`continue`（重发同一请求）。
  `rounds` 与 `total_usage` 照常推进（真实成本不许漏记），耗尽后才按现状 append + 判 `StopReason::NoOutput`。
- **判据与 A1 刻意不重叠**：A1 管「一个字节都没到」（传输格，判 `Err` 走既有重试）；
  本条管「到了数据但零可见产出」（语义格，含只输出思考的情况）。
  `finish_reason=length` 的零产出**不走本条** —— 那是 `Truncated`，由决策 2 的续写文案处理。
- **两条不变量**：① 重采样时**不 append 空 assistant 消息** ⇒ 下一次请求与上一次**逐字节相同**，
  prompt 缓存前缀仍命中，且不把空消息留在会话 / 回放里；② 它在**回合之内**，不占决策 2 的
  「每用户输入至多一个追问回合」额度。
- **依据**：把「完成但零内容」当退化结果并**默认重试**是同类实现的既有做法
  （[`research.md`](./research.md) §4.1，其默认可重试集含该错误码、上限 5）；本设计采纳其
  **可重试语义**，但把重试落在循环内，以保住下面这条终局语义。
- **不采纳的部分**：不把零产出直接判成 `Err`。那会让重试耗尽后走 `RunFailed`，把本仓既定的、
  有可见信号的正常终止路径（决策 2 的 `NoOutput` + 一行说明 + 退出码 1）换成硬失败，
  也与「合法空回复不得被误伤」的边界冲突。

### 4. 回合级工具输出预算：库侧默认关、app 派生、循环定价、工具收窄

- `RunLimits` 加 `tool_result_turn_bytes: usize`，**默认 `0` = 关闭**（既有调用方与全部既有测试行为
  逐字节不变，照 `fixed_prompt_tokens` 的承重默认先例）。
- **唯一接线点**：`apps/coding-agent/src/main.rs::resolve_limits` 由当前窗口派生
  `= context_window / 8 * 4`（128k ⇒ 64 KiB）。**不进 `system` / 工具描述**（参数恒定 ⇒ 不需要
  每轮注入；模型可见的前缀逐字节不变 ⇒ 缓存前缀零损伤）。
- **定价在循环、收窄在工具**：`BasicLoop` 的 `ToolLedger` 按批均摊剩余预算
  （`max(FLOOR_BYTES, remaining / batch)`）并记账；预算经 `ToolContext.result_budget` 下达，
  各工具用 `min(自身硬上限, visible_bytes)` 收窄，**保留自己的截断方向与通知**
  （`read` 保头 + `offset` 续读；`bash` 保尾；`grep` 保头 + 收窄指引）。
- **不放宽任何上限**：预算只能让工具**下调**自己的 cap；`None`（默认）时逐字节走现状。
- **不改写历史消息**：收窄只发生在结果 `emit` / `append` **之前**（改写历史会从改动点起废掉
  prompt 缓存前缀，代价高于它省下的量）。
- v1 **不做**落盘 + 指针（那是 v2 第一候选）；**不新增事件**（收窄事实由工具 notice 可见 + 一行日志）。

### 5. 现场证据落日志（诊断）

- 适配器在拿到「无可见产出」或「被截断」的 `Ok` 响应时落一行日志
  （`finish_reason` + usage）；`BasicLoop` 在回合末落一行账本（已用字节 / 收窄条数）。
- **只写日志，不改行为** —— 这两行正是本次排障缺少的东西（会话文件里看不出 `finish_reason`
  与 usage，导致「模型没说话」和「网关把答复丢了」无法区分）。

## 后果

- **稳态成本 ≈ 0**：不改 `system`、不改工具表、不加常驻文本；追问文案只在异常路径出现，
  每个异常回合至多一条（≈ 40–60 token，1.0×，且进入前缀后按 0.1× 复用）。
- **异常路径的一次额外调用是值得的**：空产出重采样最多 2 次，且**请求逐字节不变 ⇒ 全前缀缓存命中**
  （只付生成费）；它挡掉的是「本来就白跑 + 还要用户手动重问」那一次更贵的来回。
- **收益**：三格收场各自有确定归宿（重试 / 重采样 → 追补 / 续写），静默停摆不再可能；
  工具输出的**回合级**累计首次有了闸门（不是单条上限），实测样本 156 KB → ≈ 81 KB。
- **不对称（记录在案）**：`-p` / `--json` 走 `run_single_turn`（`main.rs:1001/1116/1155`），
  不经 `app_loop` 的追问循环，且 `consume_events` 见首个终局事件即返回 ⇒ 一次性模式**不接力**，
  只有「明确失败」（`NoOutput ⇒ exit 1` + stderr）。这与「一次性模式本就是单回合语义」一致。
- **未纳入**：`max_tokens=4096` 的取值（先由第 5 条的日志取证据）、任何放宽输出上限、
  工具参数面改动（`read` 默认窄读 / 多目标 / 布局快照）、`TurnHealth` 抽象、`MAX_CONSECUTIVE_ERRORS` 空守卫。
  另记两条候选（依据同类实现，见 [`research.md`](./research.md) §4）：**B2** 用 provider 真实
  `usage` 判上下文占用（属 ADR-0017 修订面）、**B3** 溢出 ⇒ 压缩 + 重试同一请求（与 ADR-0028
  「确定性 4xx 不重试」正面相关，需一并改分类规则）。
- **升级判据（预留，不预建）**：当出现**第二个非 `stop_reason` 驱动的接力原因**
  （ADR-0024 的 todo 接力）时，把 `follow_up_for` 提升为 `relay_plan(RelayInputs)` 并收敛
  退出码 / TUI 图标 / 子 agent 诊断这些重复读点 —— 那时第二消费者真实到齐。

## 关联

- 上游上下文：[`context.md`](./context.md)｜设计：[`design.md`](./design.md)｜同类对照：[`research.md`](./research.md)
- ADR-0004（终局事件不变式）、ADR-0020（截断保护）、ADR-0027（完工复核 + 轮数预算）、
  ADR-0028（模型调用重试 + 超时）
- 相邻设计：[`../llm-api-unavailable/context.md`](../llm-api-unavailable/context.md)（传输层故障族）、
  [`../turn-output-and-feedback/design.md`](../turn-output-and-feedback/design.md)（单条收窄与 `NoOutput` 可见性）

## 修订

- **v2（2026-10-13）**：按「竞品都在 `tmp/`」直读 `tmp/{pi,deepseek-harness,Codewhale}` 源码后修订：
  新增决策 3（零产出重采样）、明确决策 1 天然排除 `finish_reason=length`、记入候选 B2/B3。
  证据与 `文件:行` 见 [`research.md`](./research.md) §4；本文件与 `design.md` 只留结论与
  本设计自身的理由（不写竞品对照）。
