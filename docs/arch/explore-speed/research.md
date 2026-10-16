# explore-speed — 同类实现调研（加速机制对照）

> 日期：2026-10-15
> 性质：**跨产品取舍对照**（按 `AGENTS.md` 的文档分工，竞品对照归 `docs/arch/**/research.md`）。
> 缘起：追问「`explore-speed/context.md` 有没有考虑过 `tmp/` 下同类 agent 的提速做法」。答案是**没有** ——
> 那份文档的 §其他产品怎么做 系**二手转引** `subagent/research.md`，且只覆盖「并行 / 子 agent」一个轴。
> 本文用**源码直读**补齐三条轴：**减少模型思考 / 降低单次往返延迟 / 减少工具往返轮次**。
>
> **方法**：3 个并行调研 agent 直读本机源码 —— `tmp/pi`（TypeScript monorepo）、`tmp/Codewhale`（Rust workspace）、
> `tmp/deepseek-harness`（TS/Cordis）、`tmp/Vibe-Trading`（Python）。
> **置信度**：本文结论**全部标 `[源]`**（读过源码）。`tmp/exp` 已核实为**本仓自己的运行产物**（事件 schema 与
> `crates/ys-event/src/event.rs` 逐字对应、session 文件名符合 `{unix秒}_{纳秒:09}`），`tmp/skill-probe` 为探针脚本
> —— 二者**不是同类实现**，未纳入对照。
>
> **与 `context.md` 的关系**：`context.md` 记的是现状与范围；本文只补跨产品对照，**不改写它的结论**。

---

## 0. 一句话结论

四家在「**省轮次**」上收敛于**同批并行**（本仓已落地），在「**省思考**」上三家有显式开关而**本仓为零**，
在「**省往返延迟**」上最成熟的机制是**前缀缓存稳定性契约**（Codewhale 已把它写成编译期不变量，本仓行为上做对但未成文）。
唯一能改变"轮次数量级"的机制是 deepseek-harness 的 **PTC（`run_code`）**，本仓零、其余三家也零。

---

## 1. 轴 A · 减少模型思考

| 项目 | 机制 | 出处 `[源]` |
|---|---|---|
| **pi** | 思考预算分级 `{ minimal:1024, low:2048, medium:8192, high:16384 }`，配 `MIN_ANSWER_TOKENS = 1024` 保证答案至少留 1024；**思考关闭时该字段不发**（而非发 0） | `packages/ai/src/api/simple-options.ts:55-72`、`:74-94`；`api/anthropic-messages.ts:1130-1145` |
| **pi** | 角色/场景分档：子 agent 传 `--thinking`；compaction 尊重 `thinkingLevel !== "off"`；eval harness 直接 `"off"` | `examples/extensions/subagent/index.ts:304-305`；`core/compaction/compaction.ts:566`；`evals/src/pi-harness.ts:148` |
| **Codewhale** | 请求体字段 `reasoning_effort: "off"/"low"/"medium"/"high"/"max"` —— **注释直言由 client 译为 DeepSeek 的 `reasoning_effort` + `thinking`** | `crates/core/src/request.rs:28-32`；`crates/tui/src/session.rs:41` |
| **Codewhale** | 「router 调用自身只准 `off`/`low`」—— `medium`/`high`/`max` **解析期报错**而非静默钳制（类型上不可表示） | `crates/workflow/src/reasoning_router.rs:19-26`、`:61-73`、`:97-113`；默认值 `fleet_reasoning.rs:285` |
| **deepseek-harness** | `reasoningEffort` 为 adapter 拥有的值 + `UNSUPPORTED_REASONING_EFFORT` 校验；子 agent 可清零 route 继承的 effort | `packages/llm/llm/src/index.ts:880-898`；`subagent/tool-subagent/src/model-selection.ts:92,120-126` |
| **Vibe-Trading** | `LANGCHAIN_REASONING_EFFORT` 环境变量端到端（README 宣称 + 代码实做一致） | `agent/src/providers/chat.py:108,348`；`providers/capabilities.py:31-39,52` |

**本仓现状：零。** 全仓 grep `reasoning_effort` / `thinking_budget` / `enable_thinking` **无命中**；
`adapters/model-openai-compatible` 的请求面只有 `max_tokens` / `temperature`（`src/request.rs:18,20`）；
`src/compat.rs` 只能**解析** `reasoning_content`（DeepSeek 方言），**不能控制**它。

> **可用性判断**：本仓主用 provider 即 DeepSeek 系（`compat.rs` 已认该方言），Codewhale 的同名字段就是为该方言而设 —— 接入面看起来是"请求体加一个字段"，但**这是猜测，未验证**（见 §7 建议 1）。

---

## 2. 轴 B · 降低单次往返延迟

### 2.1 前缀缓存稳定性（本轴最成熟的机制）

| 项目 | 做法 | 出处 `[源]` |
|---|---|---|
| **Codewhale** | **Volatile-content-last invariant**（编译期契约）：六个块按 most-static → most-volatile 追加，注释明写"volatile 块之后的内容全部失缓存" | `crates/tui/src/prompts.rs:986-1000` |
| **Codewhale** | **三区契约**：`PinnedPrefix`（构造后冻结）/ `AppendLog`（**只 push，无 insert/remove/edit**）/ `TurnScratch`（每轮临时） | `crates/tui/src/prompt_zones.rs:1-13` |
| **Codewhale** | 漂移（工作区/指令/skills/memory/goal）渲染为 `<context_update>` **历史追加**，pinned system prompt 永不重写 | `crates/core/src/prefix_cache.rs:743-815` |
| **Codewhale** | git 快照**只在变化时**发（否则"模型自己改一次文件，字节就变、每轮重新 prime"）；release 版本号因"churn 静态前缀"被移除；工作区路径移到每轮 `<turn_meta>` | `crates/tui/src/core/engine.rs:3583-3598`；`crates/tui/src/prompts.rs:171-198`；`engine.rs:3529-3534` |
| **Codewhale** | 工具目录按 **provider wire 顺序**排序后再哈希（顺序敏感） | `crates/core/src/prefix_cache.rs:64-71` |
| **deepseek-harness** | **摘要复用热前缀**：压缩指令作为**最后一条 user 消息**追加在重放对话之后，使辅助调用是上次 routed 请求的真前缀 | `packages/compaction/compaction-basic/src/summarizer.ts:24-30,108-111` |
| **deepseek-harness** | 工具按**字典序**发出，工具集不变 ⇒ 文本逐字节相同 | `packages/core/tools/src/ts-types.ts:285-292` |
| **deepseek-harness** | 时间注入走历史（per-step user message），**不进** system prompt | `packages/context/time-context/src/index.ts:105,181-215` |
| **pi** | `cache_control` 三处打点：system prompt、**最后一个工具**、最后一条对话消息 | `packages/ai/src/api/anthropic-messages.ts:1080-1089,1458,1373-1384`；OpenAI-compat 版 `openai-completions.ts:1074-1096` |
| **Vibe-Trading** | 有意稳定化（内存快照会话开始时冻结、输出原则无替换）—— **但** system prompt 模板里仍有每轮填入的当前时间 | `agent/src/agent/context.py:278-285`；**反例** `context.py:240-242,293,321` |

**本仓现状（已核对源码）：**

| 项 | 状态 |
|---|---|
| 易变内容不进稳定前缀 | ✅ 已做对 —— `apps/coding-agent/src/prompt.rs` 中 grep `date` / `git status` / `now()` **无命中** |
| 工具序确定 | ✅ `apps/coding-agent/src/toolset.rs:34-40` 的 `EXPECTED_ORDER` + `vec![...]` 字面量（测试钉死，改一处即红） |
| **"易变内容最后"成文为不变量** | ❌ **未成文** —— 行为对，但没有断言保护，后人加一段日期/环境块不会被任何测试拦住 |
| 压缩调用是否复用热前缀 | ❌ 未见此设计 —— 压缩摘要走独立系统提示词（`crates/ys-loop/src/compact.rs::generate_summary`），**不重放对话前缀**，与 dsh 的"重放前缀 + 尾部指令"形态相反 |

### 2.2 连接复用

| 项目 | 做法 | 出处 `[源]` |
|---|---|---|
| **Codewhale** | 进程内持久 `reqwest::Client` + `tcp_keepalive(30s)` + `http2_keep_alive_interval(15s)` + `http2_keep_alive_timeout(20s)`；另有 HTTP/1.1 回退 client | `crates/tui/src/client.rs:1866-1873`、`:269-275`、`:1889-1893` |
| **deepseek-harness** | 进程级 dispatcher 共享连接池；SSE keepAlive | `packages/web/web-fetch-http/src/network.ts:220`；`api/gateway/src/client/stream-client.ts:37,249-262` |
| **pi / Vibe-Trading** | 未找到连接池相关代码 | — |

**本仓现状**：client **已复用**（`adapters/model-openai-compatible/src/lib.rs:81` 存字段，建一次），
但 `build_client` 只设了 `connect_timeout`（`:197-201`）—— **无 tcp/http2 keep-alive**。

### 2.3 流式与早停

**四家均未找到「tool_call 收全即结束本轮、不等 finish_reason」的早停实现。** 均以 provider 的 stop 信号为准
（pi `anthropic-messages.ts:501,806`；Codewhale `client.rs:5666,7418`）。
最接近的机制是 pi 的 `eager_input_streaming`（`anthropic-messages.ts:1451-1454`），**仅 Anthropic 传输适用**，与 OpenAI 兼容面无关。

---

## 3. 轴 C · 减少工具往返轮次

| 机制 | pi | Codewhale | deepseek-harness | Vibe-Trading | **本仓** |
|---|---|---|---|---|---|
| 同批只读并行 | ✅ **默认** `toolExecution ?? "parallel"`（`agent.ts:237`） | ✅ plan 判 `read_only && supports_parallel`（`tools/file_tool.rs:445-457`） | ✅ 滚动池，`DEFAULT_MAX_PARALLEL_TOOL_CALLS = 10`，exclusive 形成屏障（`agent-loop/src/constants.ts:6`、`tool-calls.ts:199-231`） | ✅ 连续只读走 `ThreadPoolExecutor`（`loop.py:2494-2541`） | ✅ **已落地**（`d553740`），逐工具 opt-in 声明 `is_concurrency_safe` |
| **开局布局快照** | ❌ 无 | ✅ **`Project Context Pack`**：确定性有界快照 `<project_context_pack>`，`PACK_MAX_ENTRIES=220` / `PACK_MAX_DEPTH=4`，**默认关闭**；另有默认兜底的 ephemeral 概览 | ❌ **明确拒绝** —— *"No model-facing directory listing ships"* | ❌ 无 | ⏸ 未做（`context.md` Q5 待裁定） |
| **PTC / `run_code`** | ❌ | ❌ | ✅ **独有** | ❌ | ❌ |
| 大批输出落盘 + 检索 | 行内指针（`read.ts:153-157`） | ✅ spillover 100KB → `retrieve_tool_result`（`metadata/summary/head/tail/lines/query/bytes` 七种 mode） | ✅ spill notice + locator + retrievalHint | 截断 | ✅ 续读指针 + 回合级预算（`truncation-guard`） |
| 批量级早停 | ✅ `terminate`（**全批**都为 `true` 才停） | ❌ | ❌ | ❌ | ❌ |

**PTC 是四家里唯一能改变轮次数量级的机制**（deepseek-harness `core/tools/src/ptc.ts:1-8`）：
模型写代码调用工具，**只有 curated 结果进模型历史，中间结果不进对话**。
⇒ `N` 次工具调用收敛为**一轮**模型往返。

**Q5（布局快照）的对账**：Codewhale 做了但**默认关**；deepseek-harness **明确拒绝**；pi 无。
⇒ **三家无一默认开启。** 这是 `context.md` Q5 决策时应纳入的对照证据。

---

## 4. 轴 D · 减少模型调用次数

| 机制 | 结论 | 出处 `[源]` |
|---|---|---|
| 简单任务快速路径（跳过 agent loop） | **四家全无** | 搜 `fast path` / `simple task` / `trivial` / `shortcut` / `direct answer` / `triage` |
| 零 LLM 调用的压缩 | 仅 **Vibe-Trading**（5 层压缩，Layer 2 `context_collapse` 不吃模型调用） | `agent/src/agent/loop.py:1-12,63-75` |
| 工具结果聚合后再喂模型 | **dsh `run_code`**（见 §3）；**Vibe `_verification_ledger`** —— 压缩后重附"已验算式"清单，防模型重跑同样的计算（注释记载真实事故：一次运行重算 5–9 次） | `loop.py:396-404`、`:454-455` |
| compaction 触发阈值 | pi：`contextTokens > contextWindow - reserveTokens`（reserve 16384 / keepRecent 20000）；Codewhale：**token-only**（移除 message 计数触发，理由是"小消息长会话恰是最不值得重写前缀缓存的场景"） | pi `compaction/compaction.ts:235-238,132-135`；Codewhale `compaction.rs:27-34` |

---

## 5. 本仓现状汇总

| 轴 | 本仓 |
|---|---|
| A · 减少模型思考 | ❌ **零**（无任何 reasoning 控制面） |
| B · 前缀缓存稳定 | ✅ 行为已对（无易变内容 / 工具序钉死 / client 复用），❌ **不变量未成文**、keep-alive 未设 |
| C · 减少轮次 | ✅ 只读并行已落地，但**准入过粗**（§3.1 实测：97% 的多调用轮拿不到并行）；❌ PTC 零；⏸ 布局快照待裁 |
| D · 减少调用次数 | 与同类相当（无快速路径是四家共识，不算缺口） |

**最值得注意的落差是轴 A** —— 唯一一条"同类普遍有、本仓完全没有"的加速杠杆。

### 3.1 本仓实测：并行准入的实际收益面

**口径**：`tmp/exp/*.json`（15 条 `--json` 事件流，212 个有工具调用的轮）。
**方法**：一轮 = 两次模型输出之间的全部 `ToolCall`；`ToolResult` **不作边界**（一轮内 N 个调用的事件序是
`Call, Result, Call, Result, …`，拿 Result 切会把 1 轮 N 调用误数成 N 轮各 1 个 —— 首版脚本就踩了这个坑）。
脚本 `tmp/batch_stats.py`（`tmp/` 已 gitignore，故此处内联口径以保证可复现）。

| 指标 | 值 |
|---|---|
| 有工具调用的轮数 | 212 |
| **每轮工具调用数分布** | **1 个 ×47 · 2 个 ×145 · 3 个 ×15 · 4 个 ×2 · 5 个 ×1 · 6 个 ×2** |
| ⇒ 每轮 ≥2 个调用的轮 | **165 / 212 = 78%** |
| 工具名分布 | `bash` 337 · `read` 33 · `ls` 27 · `grep` 8 · `subagent` 2 |
| **≥2 调用、但含非只读工具 ⇒ 本仓整批串行** | **160 / 165 = 97%** |
| bash 调用中按**命令契约**判为只读的 | 153 / 337 = 45%（首词：`grep` 31 · `ls` 18 · `git` 13 · `sed` 13 · `echo` 7 · `find` 5…），判据见 `tmp/bash_class.py` |

**读法**：模型**在积极成批**（78% 的轮是 ≥2 个调用），**但 97% 的多调用轮拿不到任何并行** ——

> ⚠️ **2026-10-16 更正**：**"≥2 个调用"不能作为"已在成批"的证据** —— 标准定低了。
> 与 codex 的同题对照（[`cost-model.md`](./cost-model.md) §7）显示：同类**每轮发 ~5.5 个**
> 工具调用，我们 ~1.8，**轮数差 5.5 倍**，而轮数是墙钟与输入 token 的乘数。
> 本文下面这张表的用途**仅限**「并行准入的收益面」这一个问题，不可再当作成批程度的依据。
因为准入是**全批 all-or-nothing**（`crates/ys-loop/src/basic.rs:603-607`：`pending.len() > 1` **且逐个** safe 才并行），
含一个 `bash` 即整批退化为串行；而 `bash` 占全部调用的 **83%**，且其中 45% 是只读命令。

⚠️ **口径边界**：样本是 15 条固定 prompt 的实验流（非自然使用），且其 `cd <工作根> &&` 前缀占比 71%，
**看起来早于 `d49c66a`（cwd 段置顶）**。⇒ 上述比例**只作形态依据**，不能当当前版本的绝对值。

⇒ 与 §3 对账可见两处差距：**① 分块准入**（dsh/Codewhale/Vibe 切"并行段 + 串行屏障"，本仓与 pi 同为全批退化）；
**② bash 按命令契约判只读**（Codewhale `crates/tui/src/tools/shell.rs:4485-4493`，本仓 `bash` 恒 `false`）。
`②` 的杠杆最大（bash 占 83%），但它**是从命令字符串猜副作用**，属正确性判断而非安全边界 —— 采纳前须单独论证误判代价。

---

## 6. 负结论（四家都没有的，免得误以为漏了）

以下机制在四家中**均为零**（已列各项目搜索关键词）：

- **`preflight` 式"先并发跑一批只读工具再交给模型"** —— 三家各自的 `preflight` 含义都不同：
  pi 是**逐工具准备**（跑 `before_tool` 门，其测试文件即命名为 `8935-parallel-preflight-abort.test.ts`）与 RPC 的 prompt 受理回调；
  Codewhale 是路由冻结/客户端预检；deepseek-harness 是 webhook 事务与实验 code-runtime；Vibe 是启动期连通性检查。
  ⇒ `context.md` 表里"preflight 串行、执行并发"是对 **pi 术语**的准确转述，但**不要**把它读成"预取批次"。
- 简单任务快速路径；tool_call 收全即早停；预热 / 并行向模型发多请求 / spec decoding。
- pi 对子 agent 关思考的**运行时自动策略**（只有 example/extension 显式传参）。

---

## 7. 附一 · 逐轮读数（对 `context.md` P0 口径的影响）

`context.md` 的 P0 写的是"把逐轮耗时按**模型 / 工具分开**落到日志或 `--json`"。三家实测都**不止于此**：

| | Codewhale | deepseek-harness | pi |
|---|---|---|---|
| 模型/工具分开 | ✅ `SessionMetrics { llm_time, tool_time }`（`session_metrics.rs:44-56`） | ✅ `llmMs` / `toolMs` 落盘投影（`session-stats/README.md:12-24`） | ❌ 仅 operation 级起止（`session/types.ts:108-117`） |
| 事件带耗时字段 | ✅ `TurnUsage { usage, duration_ms, first_token_ms, request_ms }`（`protocol/src/event_msg.rs:347-445`） | ✅ 由事件时间推导 ttft/decode | ❌ turn 事件只带 id |
| TTFT / tok·s⁻¹ | ✅ infoline `ttft 400ms · 38 tok/s`（`infoline.rs:14`） | ✅ `{tps} tok/s` pill | ❌ 无 |
| 缓存命中率 | ✅ ledger `Cache hit 99%`（`session_metrics.rs:518`） | ✅ cache hit pill | ✅ footer `CH%` |
| 跨重启存活 | ✅ `cumulative_turn_secs` 持久化（`session_manager.rs:342-345`） | ✅ projection 跨 reload/compaction 稳定 | — |

⇒ **P0 口径偏窄**：若只记"模型 vs 工具"，将来无法与同类读数对照，也答不了"缓存有没有命中"。
`first_token_ms` 与 cache-hit 两项**都是零额外依赖**（前者是流式首个 token 的时间戳，后者 provider 已在 usage 里返回）。

---

## 8. 附二 · 回合内进度可见性（非提速，同批调研所得）

**三家都没有"当前回合第 N 轮"的计数**：

| | 回合中显示什么 | 出处 `[源]` |
|---|---|---|
| pi | 固定文案 `"Working"` + braille spinner（80ms）；**无轮号、无 tok/s**；仅 bash 工具行显示 `Took 1.2s` | `interactive-mode.ts:411,2268-2270`；`tools/renderers/bash.ts:117-121` |
| Codewhale | **相位词** `working`/`reading`/`reasoning`/`using tool`/`verifying`/`waiting on you`/`finishing` + **turn 时钟** `working 1m 15s`；轮次是**会话累计** `4 turns · 108 steps`；单工具 >3s 显示 `running (7s)` | `phase_strip.rs:1100-1135`；`session_metrics.rs:82-99,518`；`history.rs:2584-2594` |
| deepseek-harness | turn 标签 + **15 秒后才出现**秒表（`showClock = elapsedMs >= 15_000`）；`{turns} 轮 {steps} 步` 是**累计** pill | `ChatView.tsx:167-203`；`StatsPills.tsx:139-160` |

**与本仓在途改动的关系**：工作区里未提交的 `RoundStarted { round }` 事件（回合内显示 `第 N 轮`）在四家**没有对应物**。
最接近的替代是 Codewhale 的**相位词**（回答"现在在干什么"，而不是"第几轮"）与 deepseek-harness 的 **15s 阈值**
（短回合不打扰）。⇒ 该改动**需要本设计自身的理由**，不能以"别家也这么做"佐证（本项目多数决策的对照惯例见 `context.md` §其他产品怎么做）。

---

## 9. 建议（按性价比排序，均未落地）

1. **改准入为分块**（§3.1 ①）：把"全批 all-safe 才并行"改为"切连续只读段并行 + 其余串行屏障"，
   直接对着**"97% 的多调用轮拿不到并行"**这个数。**纯调度改动** —— 不碰工具面契约（对模型不可见）、不引依赖，
   是全部候选里风险最低的一条。
2. **评估 `bash` 按命令契约判只读**（§3.1 ②）：杠杆最大（bash 占 83%，其中 45% 只读），
   但它是**从命令字符串猜副作用** —— 采纳前必须单独论证误判代价（一个写命令被误判为只读 ⇒ 与同批并发读竞争）。
   注意这是**正确性判断**，不是安全边界（项目硬约束"不做安全"）。
3. **评估 `reasoning_effort` / `thinking` 开关**（轴 A）。本仓主用 provider 正是该方言，Codewhale 已有同名字段的先例。
   ⚠️ **本仓适配器是否支持、provider 是否接受，均未验证** —— 落地前必须先跑通一条真实请求。
4. **修 `context.md` 的 P0 口径**（§7）：把 `first_token_ms` 与 cache-hit 纳入逐轮读数。**建议在采基线之前定稿**，否则基线对不上同类读数。
5. **把前缀缓存不变量成文**（轴 B）：Codewhale 的三区契约（`PinnedPrefix` / `AppendLog` / `TurnScratch`）可直接借形状；
   本仓现有 `toolset.rs` 的工具序测试是同类保护的一个实例，缺的是"**易变内容必须最后、且不得进稳定前缀**"这条断言。
6. **`build_client` 补 keep-alive**（轴 B）：一行级改动，Codewhale 的参数可参照。
7. **Q5 布局快照重新判定**：现有一份"默认关"（Codewhale）与一份"明确拒"（deepseek-harness）的对照证据。
8. **PTC / `run_code` 单开需求评估**：机制最强但改动最大（需要模型侧契约 + 沙箱语义），**不建议塞进本轮**。
