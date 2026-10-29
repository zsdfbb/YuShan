# task-latency-optimization — 设计评审

- **日期**：2026-10-29
- **评审对象**：`docs/arch/task-latency-optimization/design.md`、`adr-latency-levers.md`
- **对照**：`context.md`、`docs/reports/2026-10-29-task-latency-analysis.md`、相关代码
- **结论**：**有条件通过**。A1/A2/A5 可以直接做；A4/A6/B1/B2 各有一处设计缺口，
  必须先补。整体收益中位数（13 min）的置信度偏低，建议把「门禁」写成硬前置。

---

## 1. 维度结论

| 维度 | 判断 | 依据 |
|---|---|---|
| 可行性 | 🟡 黄 | 度量、schema 修正、有效窗口可直接落地；`batch` 与「保留验证证据」各有一处与现有数据结构冲突（R2、R6） |
| 可维护性 | 🟡 黄 | 新逻辑集中在 loop 内，边界尚可；但 `batch` 引入第 5 个挂载点与嵌套并发，测试面明显扩大 |
| 可理解性 | 🟢 绿 | 术语沿用 Turn/Round/降级/压缩；三条提醒与既有 `narration_nudge` 同形；文档有明确的「不做」清单 |
| 性能与可靠性 | 🟡 黄 | 轮数杠杆成立；但上下文收紧会提高降级频率、破坏缓存前缀（R4），且 `batch` 并发上限缺失（R7） |
| 可验收性 | 🟡 黄 | 有指标与 bench 方法，但 `tpr` 在 `batch` 上线后失效（R8），门禁表述有歧义（R9） |

---

## 2. 风险排序（影响 × 可能性）

### R1（高）· A4「单调用提醒」可能诱导错误行为，且收益证据最弱

- **事实**：模型每条 `bash` 平均 4.17 个 shell 步骤（573/575 含 `&&`/`|`/`;`）。
  它已经在 bash 内批处理。提醒文案却说「send them together in ONE message」。
- **风险**：模型可能照做，把一条 4 步管道拆成 4 个独立 `bash` 调用。它们会进
  `Segment::Serial`（`crates/ys-loop/src/basic.rs:1195`），仍在同一轮，但：
  ① 工具调用 token 变多；② 每次 bash 重启 shell、不共享 `cd` 状态；
  ③ 若拆错（后一步依赖前一步），会白拿 3 条无用结果。
- **叠加**：这是纯提示型杠杆，而 ADR-0039 已记录「提示/回显未能改变行为」，
  最终靠 `f6885fa`「执行前剥离」解决。
- **建议**：把 A4 文案从「一条消息发多个调用」改成**不规定形状、只陈述代价**：
  「每条消息是一次完整模型往返；如果多个查看彼此独立，尽量在一次调用里覆盖」。
  并**先做 pilot**：只开 A4、不开其他项，测 `rounds` 与 `bash` 调用数是否真的下降。

### R2（高）· B2「压缩保留验证证据」与 `CompactPlan` 的连续区间模型冲突

- **事实**：`CompactPlan` 只有 `summarize: Range<usize>` 与 `keep: Range<usize>`
  （`crates/ys-loop/src/compact.rs:44-57`、`:84-134`），保留段必须是**连续后缀**。
- **风险**：设计里「keep 区间优先包含验证结果」在数据结构上无法表达——
  验证结果散布在历史中间，要么把整个中间段都保留（等于不压缩），
  要么把结果挪出原位（破坏消息顺序与 `tool_call_id` 配对）。
- **建议**：改成两条可行路径中的一条：
  (a) 在压缩摘要的 system prompt 的 `## Constraints & Preferences`（或新增一节）
  要求模型**逐字保留已通过的验证命令与时间**；
  (b) 让 `execute()` 在生成摘要后，把匹配 `verify_commands` 的最近一条结果
  **原样追加到摘要块之后**（新增一条独立 `Role::User` 消息，需改 `CompactOutcome`）。
  两者都不动连续区间不变量。

### R3（高）· A5 与展示口径不一致

- **事实**：`agent.context_window()` 直接读 `self.limits.context_window`
  （`crates/ys-runtime/src/agent.rs:160-162`）；TUI 的 `/status` 用它
  （`apps/coding-agent/src/view.rs:158`）。
- **风险**：`app_loop.rs:1340` 把 `limits.context_window` 设为 effective 后，
  `/status` 会显示 200k；而设计 §6.5 写「展示仍用 `context_window()`」。
  两处说法冲突，且用户会困惑「模型不是 1M 吗」。
- **建议**：明确展示为 `1M (capped 200k)`，即 `view.rs` 同时拿
  `config.context_window()` 与 `agent.context_window()`；或干脆只显示 effective 并加注。
  顺带核对 `main.rs:4434/4440` 的启动期日志是否也要区分。

### R4（高）· A5 收紧回合预算会提高降级频率，收益估算未计

- **事实**：`max_turn_tool_result_tokens = effective/4`（`main.rs:1053`）。
  200k ⇒ 50k；实测每回合工具输出增长约 17k，但长回合会超 50k。
- **机制**：降级会 `replace_all` 改写历史中的早期工具结果
  （`basic.rs:1343-1357`），**每改写一次就断一次前缀缓存**。
  当前 1M 窗口下该闸门几乎不触发；500k 阈值变成 50k 后会频繁触发。
- **风险**：缓存命中从 99.3% 下降，prefill 成本上升，可能抵消 TTFT 收益。
- **建议**：在 A/B 中单列 `cache_hit` 与 `retry/elision` 次数；
  若下降超过 1 个百分点，把回合预算改为「窗口/8 且不低于 50k」而不是固定四分之一。

### R5（中）· 三条提醒的注入优先级未定义

- **事实**：Step 5 当前只有 `narration_nudge` 一个标志（`basic.rs:635-650`）。
- **风险**：A4 与 A6 可能在同一轮同时置位，再加 narration 共三条；
  设计只写了「A4 优先于 A6」，未写与 narration 的关系。
- **建议**：固定为 `narration > A4 > A6`，每轮至多注入一条；并加单测钉住。

### R6（中）· `batch` 的挂载位置与子 agent 可见性未定

- **事实**：子表 = 基础 9 元（`apps/coding-agent/src/toolset.rs:40-50`），
  父表追加段顺序断言为 `bash_bg + subagent + subagent_async`
  （`apps/coding-agent/src/assembly.rs:379-397`）。
- **风险**：若 `batch` 放进基础 9 元，子 agent 也会看到它；但子 agent 的
  `ToolContext` 若未挂 `ToolInvoker`，调用会得到「cannot be nested」——
  一个**可见但不可用**的工具（本仓明确反对这种形态）。
  若放在追加段，现有尾断言必须同步改。
- **建议**：`batch` 放**追加段末尾**（`bash_bg + subagent + subagent_async + batch`），
  子 agent 不提供；同时更新 `EXPECTED_ORDER`、尾断言与指纹锚。
  若确实要给子 agent，需在 `subagent.rs` 的端口装配处挂 `ToolInvoker` 并加测试。

### R7（中）· `batch` 内层并发 × 外层并行，缺全局上限

- **事实**：`batch` 声明 `is_concurrency_safe = true` ⇒ 可与其他只读工具同段并行；
  内层又允许 4 路。外层上限 `DEFAULT_MAX_PARALLEL_TOOLS = 4`（`limits.rs:9`）。
- **风险**：理论并发 = 4 × 4 = 16 个进程（`grep`/`find` 各自 spawn）。
  在低配机器或大仓库上会拖慢甚至抖动。
- **建议**：`batch` 的实际内层并发取
  `max(1, max_parallel_tools / 外层并行数)`，或干脆声明 `is_concurrency_safe = false`
  （v1 更稳），把它当串行屏障。后者会牺牲少量并行，但避免资源尖峰。

### R8（中）· 验收指标 `tpr` 在 `batch` 上线后失效

- **事实**：`batch` 让外层工具调用恒为 ~1/轮，但内层操作数上升。
- **风险**：§9 的「每轮工具调用数 ≥1.5」在第二刀之后会**必然不达标**，
  与「长回合轮数」「rounds」等指标矛盾。
- **建议**：指标改为三件套：`rounds/turn`、`inner_ops/round`（从 batch 结果头解析）、
  `dependency_layers/round`。`tpr` 只作为第一刀的临时指标。

### R9（中）· 门禁表述有歧义

- **原文**：「若 `tools_per_round < 1.5`、单调用轮 > 50%、轮数降幅 < 15%、质量位保持，
  才启动 `batch`」。
- **问题**：「轮数降幅 < 15%」既可能是**第一刀做完了但还不够**，也可能是**第一刀做砸了**，
  两者该采取的动作不同（前者上 batch，后者回滚 A4/A6）。
- **建议**：拆成两个判据：
  - **失败判据**：质量位回退，或 `rounds` 上升 ⇒ 回滚对应项；
  - **继续判据**：质量位保持且 `rounds` 降幅 < 15% ⇒ 启动 `batch`。

### R10（低）· 配置键命名易混

`context_window`（model→窗口 map）与 `max_context_tokens`（顶层标量）语义相近。
建议改为 `context_cap_tokens` 或 `effective_context_window`，并在注释里写清两者关系。

### R11（低）· B2 第 3 层「拒绝执行」与既有先例冲突

ADR-0039 明确选择「命令照原样执行，不剥离、不拒绝」。B2 的硬拦截是**语义反转**，
必须在该 ADR 或新 ADR 里记明，否则后人会把它当成违背既有决策。
另外树指纹包含未跟踪文件的 `mtime`：任何工具生成的临时文件都会让指纹失效
（方向是「漏拦截」，安全，但会让命中率低）。建议注释写明这是有意的偏保守。

### R12（低）· A2 移除 `required` 后的校验缺口

`read` 的 `required` 去掉 `path` 后，「两个都没给」只能在 `call` 里报
`InvalidInput`（`read.rs:72-75`）。需补一条单测，且确认 provider 不会因为
`required: []` 的 schema 拒绝请求（OpenAI 兼容端点一般接受，但要实测一次）。

### R13（低）· 测试矩阵映射缺失

`AGENTS.md` 要求新模块/接口对应 `docs/design.md` §12 的测试矩阵条目。
当前设计对 A1/A4/A5 给了变异测试，但 A3、A6、B1、B2 没有。
至少需要：
- A6：阈值翻倍、每回合重置、不落 session；
- B1：非法 plan 不执行任何步、引用只许向前、嵌套拒绝、预算截断；
- B2：树指纹变化即不拦截、非 git 关闭、失败记录不命中。

### R14（中）· 整体收益的置信度

第一刀 5–13 min 中，只有 A5（1.5–2.5 min）几乎确定；A3/A4/A6/A7 全部依赖行为改变。
若模型不响应提醒，第一刀接近 **2–3 min**。设计已如实标注区间，但
「中位 13 min」容易被读成期望值。建议在 ADR 的 Consequences 里补一句：
**「第一刀的真实下界是 ~2 min，行为杠杆可能全部为 0」**。

---

## 3. 改进建议分层

### 易修复（改文档即可，无需重新设计）

1. 修 R3：统一窗口展示口径（effective + 声明值并列）。
2. 修 R5：写死注入优先级 `narration > A4 > A6`，每轮至多一条。
3. 修 R6：`batch` 放追加段末尾，子 agent 不提供，并列出要同步的断言。
4. 修 R8：指标改为 `rounds/turn` + `inner_ops/round`。
5. 修 R9：把门禁拆成「失败判据 / 继续判据」。
6. 修 R10：配置键改名。
7. 修 R14：ADR 补一句真实下界。

### 需讨论（要先做决定）

8. **R1**：A4 文案是否改成「只陈述代价、不规定形状」？建议先做 A4-only pilot。
9. **R2**：B2 第 2 层走「摘要提示」还是「摘要后追加独立消息」？后者要改
   `CompactOutcome`，成本更高但更可靠。
10. **R4**：回合预算是否从 `effective/4` 改为 `max(effective/8, 50k)`？
11. **R7**：`batch` 是否声明为 `is_concurrency_safe = false`（v1 稳，牺牲并行）？

### 架构级（需要新 ADR 或改既有决策）

12. **B2 硬拦截**：与 ADR-0039「不拒绝执行」相反，需新 ADR 记录这次反转的理由与边界。
13. **A6 软提醒**：ADR-0027 当时把「软警告」记为超范围；本设计要说明语境已变
    （`max_rounds = u32::MAX`、复核回合已删），否则属于「悄悄推翻旧决策」。
14. **收益叙事**：当前「两刀 + 门禁」结构合理，但若目标是 2–5 倍，必须把
    「主轮思考强度」单独立项（ADR-0060 决策 7 有质量余量不为零的读数）。
    本设计应明确写出：**它不是那条路**。

---

## 4. 可以直接放行的部分

- **A1 度量**：`RoundClock` 在两条入口都是「回合内构造一次」
  （`apps/coding-agent/src/main.rs:961`、`app_loop.rs:1367`），累加口径成立。
- **A2 schema 修正**：代码已支持只给 `paths`（`read.rs:72-75`），修 `required` 是消除矛盾，零语义风险。
- **A5 有效窗口**：`CompactPolicy` 算法与不变量一行不改；四个生产调用点已核对
  （`main.rs:1048/1053`、`app_loop.rs:798/1340`）。
- **B1 无沙箱边界**：只调度四个 `is_concurrency_safe` 工具、不执行代码，与项目定位一致；
  这是本设计里最扎实的一处结构判断。

---

## 5. 评审后建议的动作顺序

1. 先按「易修复」7 项改文档（不改代码）。
2. 就「需讨论」4 项定调，尤其是 R1（A4 文案）与 R4（回合预算）。
3. 第一刀只做 A1 + A2 + A5（低风险、可确定收益），A3/A4/A6/A7 作为独立 A/B 开关，
   不和 A5 混在同一次发布里——否则无法归因。
4. 第二刀在门禁通过后再写实现计划。

---

## 6. 修订状态（2026-10-29）

设计已按本报告修订，明细见 `design.md` §12「评审修订」。

| 发现 | 状态 |
|---|---|
| R1 A4 文案 | ✅ 已改（只陈述代价、不规定形状）并加 pilot 要求 |
| R2 压缩保留证据 | ✅ 已改（摘要 `## Verification` 节；v2 才追加消息） |
| R3 展示口径 | ✅ 已改（effective / declared 并列） |
| R4 降级频率 | ✅ 已加读数与备选预算 |
| R5 注入优先级 | ✅ 已写死 `narration > A4 > A6` |
| R6 batch 可见性 | ✅ 已改（追加段末尾、子 agent 不提供） |
| R7 并发尖峰 | ✅ 已改（v1 串行） |
| R8 指标失效 | ✅ 已改（`rounds/turn` + `inner_ops/round`） |
| R9 门禁歧义 | ✅ 已拆成三组判据 |
| R10 键名 | ✅ 已改为 `context_cap_tokens` |
| R11 ADR-0039 反转 | ✅ 已在 ADR 决策 7 写明窄类边界 |
| R12 read 校验 | ✅ 已补测试条目 |
| R13 测试矩阵 | ✅ 已补齐 A2/A3/A6/A7/B1/B2 |
| R14 收益下界 | ✅ ADR 已写明 ~2 min 的真实下界 |

**仍未决**：B2 是否值得实现（取决于 A7 的「未请求写操作占比」读数）；
`batch` 是否有必要（取决于门禁）。
