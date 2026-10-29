# task-latency-optimization — 架构上下文与优化方案

- **日期**：2026-10-29
- **性质**：arch-explore 产出。基于 `docs/reports/2026-10-29-task-latency-analysis.md`
  的测量，界定「让同一任务更快完成」的需求范围、约束与候选杠杆。
  具体机制设计与取舍留给 `arch-design`。
- **已确认的取舍**（用户 2026-10-29 回答）：
  1. 成功指标 = **墙钟时间优先**，token 成本只作为约束；
  2. **允许结构性改动**（可改 loop / 工具语义 / 新增机制）；
  3. **允许直接改默认值**，但仍须以对照实验支撑。

---

## 概述

本需求解决的是「同一任务，ys-coding-agent 比 Codex / 另一个 harness 慢 2–5 倍」。
现有测量把差距定位到**轮数**与**每轮固定延迟**，而不是工具执行或 harness 内部开销。
本文件给出可落地的杠杆清单、分批路线与验收方式。

---

## 现有架构

### 模块边界

| 层 | 职责 | 与本需求相关的文件 |
|---|---|---|
| `ys-loop` | 回合 / 轮推进、工具分段、压缩、回合预算 | `crates/ys-loop/src/basic.rs`、`compact.rs`、`turn_budget.rs`、`token.rs` |
| `ys-component` | 运行时配置与限制 | `crates/ys-component/src/limits.rs`、`compact.rs` |
| `ys-session` | 会话持久化 | `crates/ys-session/src/jsonl.rs` |
| `ys-event` / `channel` | 事件出口与背压 | `crates/ys-event/src/sink.rs`、`apps/coding-agent/src/channel.rs` |
| `adapters/model-openai-compatible` | 请求构造、SSE 解析、重试 | `lib.rs`、`request.rs`、`stream.rs` |
| `adapters/tools-basic` | 文件 / shell 工具 | `read.rs`、`bash.rs`、`grep.rs`、`exec.rs` |
| `apps/coding-agent` | 装配、提示词、指标、命令 | `prompt.rs`、`assembly.rs`、`main.rs`、`debug_log.rs` |

### 核心抽象

- **Turn / Round**：一个用户回合（`run_turn`）由 N 轮（`model.complete` 一次 + 一批工具）组成。
  每轮都重发完整 system prompt、完整工具表、完整会话历史
  （`crates/ys-loop/src/basic.rs:634-666`）。
- **工具分段**：`split_segments` 把**连续**的并发安全工具切成 `Segment::Parallel`，
  非并发安全工具是串行屏障（`basic.rs:336-363`）；并发上限 `DEFAULT_MAX_PARALLEL_TOOLS = 4`
  （`crates/ys-component/src/limits.rs:9`）。
- **压缩**：`CompactPolicy` 以**声明窗口**的比例触发（默认 85%），保留 10%
  （`crates/ys-component/src/compact.rs:45-75`）；回合级工具结果降级阈值为窗口的 1/4
  （`apps/coding-agent/src/main.rs:1053`）。
- **运行限制**：`RunLimits`（`crates/ys-component/src/limits.rs:147-175`）承载
  `context_window` / `max_rounds` / `max_parallel_tools` / `narration_silence_tool_calls` 等。

### 关键数据流（一次轮）

```
session.messages() ──clone──▶ ModelRequest(system + tools + messages)
        │                              │
        │                      model.complete()  ← 全量 prefill（缓存命中 98–99%）
        │                              ▼
        │                     assistant message（文本 + tool_calls）
        │                              ▼
        └──── append ◀── 工具分段执行（并行段上限 4，串行段逐个）
                               ▼
                     一条 User 消息装全部 ToolResult ──▶ 下一轮
```

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| OpenAI 兼容 `/chat/completions` | 唯一模型协议 | 无 Anthropic adapter |
| provider 侧前缀缓存 | 唯一缓存来源 | 客户端**不发**任何 cache 字段；只被动读取 `prompt_cache_hit_tokens` / `cached_tokens` |
| `git` | 提交 / 推送 | 收尾阶段行为受模型习惯影响 |

### 现有可观测性

debug JSONL 已记录足够归因的数据（`schema: yushan.debug/1`）：

| 事件 | 关键字段 |
|---|---|
| `run_start` | model / effort / provider / limits / 工具名 + spec 字节 / skills 计数 |
| `model_attempt` | `duration_ms`、`ttfb_ms`、`response_headers_ms`、`http_status`、`outcome`、`retried` |
| `round_usage` | input / output / reasoning / cache_read tokens |
| `tool_call` | 工具名、参数（可脱敏 / 截断） |
| `tool_result` | `duration_ms`、`result_bytes`、`result_lines`、`is_error` |

缺口：**这些数据没有被汇总成一行可读结论**，也没有进 `e2e_bench` 的落盘结果。

---

## 约束

- **技术**：`ys-core` / `ys-event` / `ys-protocol` 不含 tokio；`ys-tui-coding` 不依赖 `ys-runtime`；
  `ModelEventSink` 必须保持同步 `fn`。任何新机制不得跨这些边界。
- **缓存前缀**：工具表顺序、system prompt 逐字性、skill 目录顺序都是缓存前缀契约
  （`apps/coding-agent/src/assembly.rs:162-191`、`toolset.rs` 的顺序守卫）。
  新增工具或改工具描述会**整体平移前缀**，须一次性完成并接受一次冷启动。
- **性能**：墙钟优先，但不得显著抬高 token 成本；不得让单轮生成长度失控
  （当前主模型 `max_tokens` 不发送，`apps/coding-agent/src/main.rs:2622-2625`）。
- **演进**：`RunLimits` 是 `#[non_exhaustive]`；新字段默认值须保持老调用方逐字节不变
  （既有惯例，见 `limits.rs` 注释）。
- **质量**：报告已有 `fact/1` 一次质量退化读数（`docs/adr/0060-...`），任何降推理强度 /
  提早压缩的改动都必须用 `QUALITY_SPECS` 对照。
- **组织**：单项改动可独立回滚；每项都要有对照读数，不接受「感觉快了」。

---

## 需求范围

### 范围内

1. 降低「完成同一任务」的墙钟时间。
2. 建立可持续的归因与验收手段（否则后续改动无法判断好坏）。
3. 解决收尾阶段零信息增量的重复验证。

### 范围外（明确不做的）

- 不做安全、沙箱、权限（项目定位）。
- 不做模型训练 / 微调 / 换模型来提速。
- 不追求与 Claude Code 的严格对照（无源码、无同任务基线）。
- 不引入动态插件 / 热插拔。
- 不为「体感」做纯 UI 动画类优化（本需求只看墙钟与 token）。
- 不在本需求内解决「生成吞吐 43 vs 76.8 tok/s」的根因（单列实验，见 §6）。

### 关键场景

- 场景 1：用户提出一个需要 10–50 轮的中型任务；当前 555 轮级会话不可接受。
- 场景 2：用户先做实现、再要求「commit+push」；不应重跑已汇报过的验证。
- 场景 3：长会话（>20 万 token）继续工作；TTFT 不应随上下文线性恶化到不可用。
- 场景 4：探索型任务失控（单个 turn 166 轮）；应有软护栏与可观测的进度。

---

## 优化杠杆清单

按「预期墙钟收益 / 风险」排序。每条给出**证据 → 机制 → 改法 → 预期收益 → 风险**。
优先级：**P0 = 先建立度量**，**P1 = 压轮数**，**P2 = 压每轮成本**，**P3 = 收尾与工作流**，
**P4 = 工程量级**。

### P0 — 度量与验收（其余一切的前提）

**L1. 运行结束打印一行归因**
- 证据：debug JSONL 已有全部字段，但没有汇总；`e2e_bench` 的 `Reading` 只用于测试。
- 改法：`RunResult` / `TurnStats` 已含 usage 与轮数；在 `-p` / `--json` / TUI 收尾打印
  `turns / rounds / tools/round / ttft / gen / tools / context tokens / cache hit`。
- 收益：任何后续改动都能在同一次会话里被验证。
- 风险：无（纯输出）。

**L2. `e2e_bench` 结果落盘**
- 证据：`Reading` 的质量位只打印不落盘（`apps/coding-agent/tests/e2e_bench.rs:834-855`）。
- 改法：每轮 bench 写一份 JSON（prompt、rounds、wall、ttft/gen/tools、cache、quality）。
- 收益：A/B 有可回溯证据，不依赖临时目录。
- 风险：无。

### P1 — 压轮数（最大杠杆）

**L3. 强化「一条消息多个只读调用」的提示与工具描述**
- 证据：526 个有工具的轮里 **426 轮只有 1 个调用**；跨 harness 对照中 codex ≈5.5 个/轮。
- 机制：现有指引在 `apps/coding-agent/src/prompt.rs:228-245`，但执行率低；`read` 与 `grep`
  **已支持 `paths` 数组**（`adapters/tools-basic/src/read.rs:30-34`、`grep.rs`）。
- 改法：把「批处理」从建议升级为**默认动作**的具体形状（给出「一次读 3–5 个文件 +
  一次 grep 多路径」的示例），并收窄「整目录可以退回 shell」的口子（`prompt.rs:236-245`）。
- 预期收益：轮数从 ~1.1 工具/轮提升到 2–3，理论上可砍 40–60% 轮数。
- 风险：单轮输出变长、并行段里一个失败影响整批；已有依赖的任务无法合并。

**L4. 工具结果内的行为纠正（机制型，不靠提示词自觉）**
- 证据：73%（456/626）的调用是 `bash grep` / `sed -n` / `head/tail` / `cat` / `ls`。
- 机制：工具结果已统一带 notice 形态（`crates/ys-tool/src/truncate.rs:81`，
  `crate::notice::size_notice`）。
- 改法：当 `bash` 命令匹配只读查看器形态时，在结果尾部追加一行提示：
  「同一操作可用 `read` / `grep` / `find` / `ls`，且多个目标可放进**一条消息**并行执行」。
  需要**每会话次数上限**，避免噪声与 token 增长。
- 预期收益：把「一次性改行为」从随机事件变成确定性纠正；估计可回收 L3 的一半收益。
- 风险：噪声；可能被模型当成错误信息过度反应；增加每轮 token 前缀无关的尾部成本。

**L5. 让「面操作」在结构化工具内可表达**
- 证据：现有指引明确允许「整个目录或 glob 用一个 shell 循环」
  （`prompt.rs:236-245`），这正是 bash 查看器的入口。
- 改法：`read` / `grep` 增加目录 / glob 目标（或新增 `read_many`），使「读一个目录下全部
  markdown」「搜多个目标」不必退回 bash。
- 预期收益：直接消除 L4 想纠正的那类行为，且保留统一截断与 `.gitignore` 语义。
- 风险：改工具 schema ⇒ **缓存前缀整体平移**；结果体积需要新的预算控制。

**L6. 只读 `bash` 命令的并发安全判定**
- 证据：`bash` 默认 `is_concurrency_safe = false`（`crates/ys-tool/src/lib.rs:46-55`），
  一批里混一个 bash 就退化为串行（`basic.rs:336-363`）。
- 改法：对**严格 allowlist**（`git status` / `git log` / `git diff` / `wc` / `ls` 等无副作用
  命令）标记为可并发；解析失败一律退回串行。
- 预期收益：即使模型坚持用 bash，批内也能并行；只对**已有依赖**的工具时间有帮助
  （注意：工具时间只占 1%–20%，故这一条收益取决于批量工具是否会变多）。
- 风险：误判副作用命令；allowlist 维护成本；与 L4/L5 目标相反（鼓励 bash）。

**L7. 轮数软护栏**
- 证据：`max_rounds` 默认 `u32::MAX`（`apps/coding-agent/src/config.rs:333`，ADR-0027 有意为之）；
  实测单个 turn 166 轮，3 个 turn 占 54% 主循环时间。
- 改法：保留硬上限语义，新增**软阈值**：到阈值后注入一次「收敛或汇报」提醒
  （复用 `narration_silence_tool_calls` 的注入通道），不中断回合。
- 预期收益：压长尾；对正常任务零影响。
- 风险：可能打断合理的长时间探索；阈值需要按任务形态校准。

### P2 — 压每轮固定成本

**L8. 压缩触发与真实上下文对齐**
- 证据：`context_window` 默认声明 **1,000,000**（`crates/ys-component/src/limits.rs:153`），
  四级解析见 `apps/coding-agent/src/config.rs:275-300`；实测会话长到 **41.8 万 token 未压缩**
  （触发点 85 万）。
- 改法：给压缩加**绝对触发上限**（或 `compaction.trigger_tokens` 配置），取
  `min(窗口×85%, 绝对上限)`；同时把 `fixed_prompt_tokens`（系统提示 + 工具表）计入判定
  （已有，见 `basic.rs:570-585`）。
- 预期收益：TTFT 随输入从 ~3.5 s 回落到 ~2.3 s（40 万 → 10 万量级），555 轮约省 **9 min**。
- 风险：压缩本身是一次模型调用；压缩后首轮必然 cache miss；过早压缩可能丢细节导致重复读文件。

**L9. 回合级工具结果降级的绝对上限**
- 证据：阈值为 `context_window / 4`（`apps/coding-agent/src/main.rs:1053`），1M 窗口下为 25 万。
- 改法与风险：同 L8，随 L8 一起调整。

**L10. 按调用类型分推理强度**
- 证据：reasoning 占 output 27%–53%；摘要类调用关思考墙钟 −28%~−42%
  （`docs/arch/effort-per-call-type/context.md:361-418`）；ADR-0060 已定方向、默认未改。
- 改法：摘要 / 压缩 / 子 agent 的**非主轮**调用用低档或关思考；主轮保持现默认。
- 预期收益：中等（摘要调用占比不大），但无质量风险。
- 风险：摘要质量下降可能间接导致重复工作。

**L11. 空响应重试的默认值**
- 证据：`retry_empty_response` 默认 `true`（`crates/ys-component/src/limits.rs:161-167`），
  触发时白跑一整轮（`basic.rs:957-973`）。
- 改法：保留机制，但仅在**没有产出且没有工具调用**时重试（当前已如此），或按 provider 校准。
- 预期收益：小；主要避免长尾。
- 风险：关掉后偶发空回复直接失败。

**L12. 每轮请求的复制与序列化**
- 证据：每轮 `messages().to_vec()` + `specs().to_vec()` + 全量 `serde_json`
  （`basic.rs:634-641`、`adapters/.../lib.rs:1207-1331`）。
- 改法：在长会话下避免不必要的深拷贝（如改用引用 / 只克隆新消息 + 缓存序列化前缀）。
- 预期收益：本地 CPU，对墙钟影响小（远低于网络等待）；仅作为长会话的兜底。
- 风险：改 loop 核心数据流，回归面大。

### P3 — 收尾与工作流

**L13. 验证证据复用**
- 证据：仓库**没有**任何「最近一次验证 + 树状态」机制（全仓 grep 零命中）；
  用户被迫补「直接 commit+psuh，不用再测试了」（会话 `1793151820` turn 20）。
- 改法：记录 `(tree hash, 命令, 结果, 时间)`，作为会话内可查询的证据；提示词规定
  「工作区自上次验证后未变 ⇒ 不得重跑同一验证」。
- 预期收益：每次 git 收尾省 30–50 s；消除「明明刚测完」的体感问题。
- 风险：树状态判定要正确（未跟踪文件、子模块、gitignore）；证据可能过期而仍被复用。

**L14. 提示词边界：纯 git 操作不重跑验证**
- 证据：`prompt.rs`、`AGENTS.md`、`CONTRIBUTING.md` 均无提交流程约定
  （grep 无命中）；`skills/sequential-workflow/SKILL.md:154` 的 Phase 5「跑全量测试」
  会外溢到用户临时插入的 git 请求。
- 改法：guidelines 增加一条边界；skill 的 Phase 4/5 限定为「由该 workflow 驱动的实现任务」。
- 预期收益：同 L13，但零机制成本。
- 风险：模型可能借机跳过必要验证；需要措辞区分「本轮改过代码」与「只做 git 操作」。

**L15. 父回合不阻塞在子 agent 上**
- 证据：给出最终答复后仍会等未完成的子 agent，最长 30 min
  （`crates/ys-loop/src/basic.rs:983-1017`、`limits.rs:163-165`）；异步结果已支持跨回合送达。
- 改法：主答复已给出时直接收场，子 agent 结果在下一回合开头补投。
- 预期收益：去掉最坏 30 min 的不可见等待。
- 风险：改变「回合结束 = 结果已到齐」的语义；用户可能期望答复里包含子 agent 结论。

### P4 — 工程量级（不先做）

**L16. 会话 JSONL 增量 append**
- 证据：每次 append 全量重写整个文件，O(n²)（`crates/ys-session/src/jsonl.rs:108-170`）。
- 改法：改为追加写 + 定期压缩；`replace_all` 保留 tmp+rename。
- 收益：长会话写放大；对墙钟影响小。

**L17. `read` 按 `offset/limit` 分段读**
- 证据：先整文件 `read_to_string` 再切片（`adapters/tools-basic/src/read.rs:80-102`）。
- 收益：大文件读取；通常 < 100 ms。

**L18. 日志与后台 sweeper 的同步文件 IO**
- 证据：`apps/coding-agent/src/logging.rs:84-100` 每行 open/write/close；
  `adapters/tools-basic/src/bg_output.rs:174` 整份读输出文件。
- 收益：低；只在长回合与后台命令密集时可见。

**L19. 工具参数流式显示**
- 证据：工具参数在适配器内累积，`ModelEvent` 没有增量事件
  （`adapters/.../lib.rs:1066-1071`）。
- 收益：**体感**，不影响墙钟；列在此处仅为完整性。

---

## 路线图

| 批次 | 内容 | 目的 | 回滚成本 |
|---|---|---|---|
| **W0** | L1 + L2 | 建立归因与 A/B 能力 | 无 |
| **W1** | L3 + L4 + L5 | 压轮数（最大杠杆） | 提示词/工具可独立撤 |
| **W2** | L8 + L9 + L10 | 压每轮固定成本 | 配置项回退 |
| **W3** | L7 + L15 | 长尾与收场等待 | 单点开关 |
| **W4** | L13 + L14 | 收尾重复验证 | 提示词/机制可撤 |
| **W5** | L6 + L16–L19 | 剩余工程项 | 逐项独立 |

**每批必须**：用 `e2e_bench` 的固定 prompt（`fact/1`、`exp/t1`、`daily/2`）做
**≥3 次对照**，记录 rounds / wall / ttft / gen / tools / quality，落盘到 L2 的结果文件。

---

## 验收指标

| 指标 | 当前 | 目标 |
|---|---|---|
| 工具调用 / 轮 | 1.1 | **≥ 2.5** |
| `bash` 查看器占比 | 73% | **< 30%** |
| 中型任务轮数（`fact/1`） | 10–33（方差大） | **≤ 10** |
| 每轮输入中位 token | 20–25 万 | **≤ 10 万** |
| TTFT 中位 | 2.2–2.5 s | **≤ 2.0 s** |
| 收尾 git 操作额外轮数 | 5–8 | **≤ 3** |
| 质量位 | 现状 | **不回退** |

---

## 未澄清问题

- [ ] **待用户确认**：`deepseek-v4.1-flash` 的 1,000,000 窗口是 provider 目录真实上报，
      还是落了兜底默认（`config.rs:275-300`）？这决定 L8 是改阈值还是修窗口来源。
- [ ] **待用户确认**：L8 的绝对触发上限取多少（120k / 200k / 其他）？
      压缩越早越省 TTFT，但丢细节风险越高。
- [ ] **待用户确认**：L4 的纠正提示每会话允许出现几次？是否接受为此增加 token？
- [ ] **待用户确认**：L6 的只读 bash allowlist 是否需要（它鼓励 bash，与 L3–L5 方向相反）？
- [ ] **待用户确认**：L15 是否接受「主答复给出后不再等子 agent」的语义改变？
- [ ] **待用户确认**：L7 的轮数软阈值取多少（按任务形态校准）？
- [ ] **待用户确认**：主轮是否允许改 reasoning effort 默认？报告有 `fact/1` 一次质量退化读数。
- [ ] **待澄清**：生成吞吐 43 vs 76.8 tok/s 的根因未隔离，是否需要先做这项单独实验，
      再决定 P2 是否值得投入。

---

## 后续建议

- 用 `arch-design` 针对 **W1（压轮数）** 出 2–3 个截然不同的候选机制并对比：
  「纯提示词强化」「工具结果纠正」「工具面操作扩容」是三种不同形状，互不排斥但成本不同。
- 用 `arch-design` 针对 **L13 验证证据** 单独设计：它的数据模型（树状态、命令、结果、
  失效条件）比提示词补丁复杂，不适合混在 W1 里。
- 先落地 W0：没有 `rounds / tools-per-round / ttft` 的常驻读数，后续每一项都无法验收。
