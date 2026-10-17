# agent-observability — 架构评审

> 日期：2026-10-17
> 对象：[`design.md`](./design.md) + [`adr-debug-log.md`](./adr-debug-log.md)
> 方法：`arch-validate`。**每条结论都回代码核实过**（行号见各条）。
> 关注维度：可行性 / 可维护性 / 可理解性 / 性能与可靠性（全量）。

---

## 总评

| 维度 | 判断 | 一句话 |
|---|---|---|
| **可行性** | 🟢 | 无新依赖，机制都是仓里已有的（有界信道、serde、`Arc`）；实现路径清楚 |
| **可维护性** | 🟡 | 有两处会长期咬人的设计：**旁路通道的边界没划清**、**14 个 `process::exit` 漏 flush** |
| **可理解性** | 🟡 | 文档完整，但**设计里有一条事实错误**（子 agent 可见性）会误导后来者 |
| **性能与可靠性** | 🟢 | Q1 处理得扎实（只记段 + 时间戳先取 + 异步写 + 自证扰动）；代价与失败模式都写明 |

**一句话结论**：方案的主体是对的（只记段、事件流拿 usage、异步单写者）；
**但有 1 条事实错误必须改、2 处边界必须划清、2 个运维缺口必须补**，否则落地后会以"看起来正常"的方式骗人。

---

## 发现（按 影响 × 可能性 排序）

### R1 · 设计声称「子 agent 的 usage 自然可见」—— **事实错误**（影响 高 / 可能性 已发生）

**证据**：`apps/coding-agent/src/subagent.rs:4` 明写

> `子事件**不进**父事件流（`NoopEventSink`）`

`:371` 处 `let mut sink = NoopEventSink;` 就是子 agent 的事件出口。

⇒ 子 agent 在 `ys-loop` 里 emit 的 `RoundUsage` **落进 `NoopEventSink` 被丢弃**。父侧只在
`RunFinished` 看到**回合末上卷**的 usage（`app_loop.rs` 的 `subagent_totals` 合并）。

**这条被写成了「选事件流而非旁路」的理由之一**，是评审里最要紧的一处 —— 理由本身不成立，
结论（走事件流）仍然成立，但要换理由。

**修**：删掉该理由；若要子 agent 逐轮 usage，需要**另一条路**（子 agent 的 runner 显式上报，
或允许子事件转发），不在本设计内。

### R2 · 「旁路通道」「事件流」两条路的边界没划清（影响 中高 / 可能性 高）

设计用「旁路会静默退化」否掉了 observer，但**同一份设计里 P1 仍然要用旁路**：
`model_attempt` / `ttfb_ms` / 重试分类**只存在于适配器内部**，事件流里没有这些事实
（实测：适配器 `lib.rs` 里 `AgentEvent::` 出现 **0 次**）。

⇒ 真实边界是：**主循环已经握有的事实走事件；只在适配器里存在的事实只能走旁路。**
不写清的话，后来者会以为"本设计不用旁路"，然后要么重复造、要么把适配器事实塞进事件污染 `--json`。

**修**：在 design/ADR 里显式画出这条线，并说明**旁路的静默退化风险只落在 P1 的适配器记录上**，
缓解手段是「P1 上线时给适配器加一条启动期自检：debug 开着但 `model_attempt` 一条都没出现 ⇒ 打警告」。

### R3 · `RoundUsage` 只覆盖**主循环**的模型调用（影响 中 / 可能性 高）

`RoundUsage` 在 `basic.rs` Step 7（`total_usage = total_usage + response.usage;` 附近）发出。
但**压缩摘要**（`crates/ys-loop/src/compact.rs`）、**工具侧摘要**（`summarize.rs`）、
**子 agent** 都是独立的模型调用，**不在这个循环里**。

⇒ 逐轮 usage 会**系统性漏掉压缩与摘要的成本**，而"压缩是一次完整模型调用"正是我们想看见的。

**修**：要么给这几处也发 `RoundUsage`（带 `purpose` 字段区分 `turn` / `compaction` / `summarize`），
要么在 ADR 里明说"逐轮 usage 只算主循环"，并在 `run_end` 用总 usage 兜底。

### R4 · `process::exit` 有 **14 处**，flush 极易漏（影响 中高 / 可能性 中高）

`apps/coding-agent/src/main.rs` 里 `std::process::exit` 出现 **14 次**。
`process::exit` 不跑析构 ⇒ 任何一条漏调 `debug.flush()` 就丢队列尾部。

设计只说"所有退出路径必须显式 flush"，没有给出**结构保证**。

**修**：不要靠"记得调"。做一个 `exit_with_debug(code)` 包装（或在 `Args` 解析后立刻装一个
RAII guard），把 14 处收敛到 1–2 个入口。这是**易修复但必须现在做**的一条 ——
一旦散落，后面每加一个退出路径都要靠自觉。

### R5 · 队列满时**没有优先级**，最值钱的记录可能先被丢（影响 中 / 可能性 低）

队列满只会在 writer 被磁盘卡住时发生。此时按入队顺序丢弃，**`round_end` / `run_end` / `usage`
与 `round_start` 一样有风险**，而前者才是排障要的。

设计写的是"v1 不做优先级"。**可接受**，但应写明：**遇到 `dropped > 0` 时，这份日志不能用于
"某轮不存在 = 没发生"的推断**。

### R6 · debug 文件**没有清理策略**（影响 中 / 可能性 随时间必然发生）

一 session 一文件、`/resume` 追加。设计只给了"文件上限 64 MiB"，**没有保留期 / 总量上限**。
`~/.yushan/logs/` 会累积。

**修**：至少写一条"启动期 sweep，按 mtime 保留最近 N 天/份"（仓里有 `sweep_fetched` 的先例），
或明确接受并写进文档。

### R7 · P0 的 700 行估计**偏乐观**（影响 低 / 可能性 中）

P0 含：`Usage` 加字段（**34 处字面量**，已核实）、新事件 + `ys-loop` emit、`debug_log.rs`、
两条入口接线、`RoundClock` 富化。按 B/C 的估算方法，接近 **900–1,100 行**。
不影响方案，只影响排期。

---

## 已核实为**成立**的关键断言（供后来者不必重查）

| 断言 | 结论 |
|---|---|
| `AgentEvent` 是 `#[non_exhaustive]` | ✅ `event.rs:5` ⇒ 加变体对消费者是加法 |
| 消费循环只认 `RunFinished`/`RunFailed` 为终局 | ✅ `main.rs:444-449` ⇒ `RoundUsage` 非终局可行 |
| `Usage` **不是** `#[non_exhaustive]` | ✅ 加字段会破坏全部字面量；实测 **34 处** |
| `split_leading_flags` 已存在 | ✅ `main.rs:153` ⇒ `--debug` 可复用同一套 flag 解析 |
| `ys-loop` 在 Step 7 能拿到 `ctx.events` 与 `response.usage` | ✅ `basic.rs:367` |
| 适配器不上报事件 | ✅ `adapters/.../lib.rs` 中 `AgentEvent::` **0 次** ⇒ 模型层事实只能旁路 |
| provider 已上报 `completion_tokens_details.reasoning_tokens` | ✅ 实测响应原文（见 `context.md` §对照） |
| session 逐条消息无时间戳 | ✅ 只有 meta `created_at`；`tool_call_id` 可作强外键（`design.md` §4.7） |

---

## 改进建议（分层）

### 易修复（现在就该做）

1. **删掉 R1 的错误理由**，ADR 里换成成立的三条之一（旁路静默退化、`--json` 加法、覆盖三种消费者）。
2. **R4：用 `exit_with_debug` 包装 14 处 `process::exit`**，不靠自觉。
3. R5：在文档里补一句「`dropped > 0` ⇒ 不能据"某轮不存在"作否定推断」。

### 需讨论

4. **R2：显式划出「事件 / 旁路」的边界**，并给 P1 的适配器记录加启动期自检。
5. **R3：逐轮 usage 是否覆盖压缩/摘要/子 agent** —— 覆盖则加 `purpose` 字段；不覆盖则 ADR 写明范围。
6. **R6：debug 文件的保留策略** —— 启动期 sweep 还是明确接受累积。

### 架构级（本轮可不动，但要留痕）

7. **子 agent 的可观测性**（R1 的根问题）：子事件目前被 `NoopEventSink` 丢弃是 ADR-0022 的既定设计。
   要让子 agent 逐轮可见，需要独立的"子 agent 上报"设计，**不要顺手改事件转发**。
8. `t_ms` 由单调原点派生 ⇒ 墙钟跳变（NTP）不反映。设计已注明；若将来要与外部系统严格对时，
   需要另加一条"墙钟校准"记录。

---

## 与验收的关系

现有 §8 验收的五条**都是对的**，但缺两类：

- **负向验收**：`--debug` 关闭时，`--json` 输出与今天**逐字节相同**（现在只写了"`yushan.log` 不变"，
  没写 `--json`；而 `RoundUsage` 在 debug 关闭时**仍会发** —— 这是设计的选择，必须被验收钉住）。
- **变异测试清单**：本仓要求关键逻辑做变异测试。至少要有
  「不记增量」「时间戳先于摘要」「flush 收敛」「脱敏命中」四条。
