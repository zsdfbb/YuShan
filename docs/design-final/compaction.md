# 上下文压缩（片 1）— As-built

> 性质：**实际建成记录**。设计见 `docs/arch/skills-and-compaction/design-compaction.md`
> （+ 上游 `context.md`），决策见 `docs/adr/0017-compaction-hardening.md`，
> 质量分析见 `docs/arch/skills-and-compaction/review.md`。
> 日期：2026-09-19

## 一句话

压缩从 `BasicLoop` 里的**无名副作用**变成 `ys-loop::compact` 的显式两步
（纯函数 `plan()` + 唯一 `await` 点 `execute()`），与 `RuntimeContext` **完全解耦**——
`/compact` 与 Step 4b 自动触发**共用同一份实现**；窗口改为**按 model 解析的三级链**；
失败**不再把历史截断成一行占位符**。

## 落点

```
crates/ys-component/src/compact.rs   CompactPolicy（纯数据：比例 / output_reserve / summary_system_prompt）
                                    + trigger_at / keep_budget（纯函数，P1/P2/P3 性质单测）
crates/ys-component/src/limits.rs    RunLimits 增 context_window + compaction 两字段
crates/ys-component/src/lib.rs       mod compact; pub use compact::*
crates/ys-event/src/event.rs         ContextCompacted / ContextCompactionFailed（平铺字段，见「偏离」）
crates/ys-loop/src/compact.rs        算法唯一宿主：CompactPlan / plan() / execute() / CompactOutcome / CompactError
crates/ys-loop/src/basic.rs          Step 4b：阈值 → plan → execute → session.replace_all → 发事件
crates/ys-loop/src/lib.rs            mod compact 私有，pub use compact::*; 根级再导出 plan/execute
crates/ys-loop/src/token.rs          估算器（estimate_tokens / estimate_session_tokens）—— 兼作阈值与切分
crates/ys-session/src/lib.rs         Session::replace_all（默认 = clear + N append，非原子）
crates/ys-session/src/jsonl.rs       JsonlSession 覆写 replace_all（赋值一次 + 单次 flush_to_file）
apps/coding-agent/src/provider.rs    KNOWN_MODEL_WINDOWS 内置表 + builtin_context_window()
apps/coding-agent/src/config.rs      三级窗口解析 context_window() + compact_policy() + load_user_config()
apps/coding-agent/src/main.rs        读 config.json → 算启动初值 limits → build_agent(..).limits(limits)
apps/coding-agent/src/app_loop.rs    Prompt 臂开头 set_context_window；Compact 臂 → capabilities::compact
apps/coding-agent/src/capabilities.rs /compact 真压缩（无 model / 无事可压 / 成功 / 失败，四路径）
apps/coding-agent/src/wiring.rs      clear_session 已删（见「偏离」）
crates/ys-tui-coding/src/app.rs      ContextCompacted → System 行；Failed → Error 行（**不** finish_turn）
crates/ys-runtime/src/agent.rs       Agent::context_window() / set_context_window()（limits 最小暴露面）
```

## 关键行为（实现期定死，勿凭直觉改）

**三级窗口解析链（优先级从高到低，按 `model` 不按 `provider`）**：
① `~/.yushan/config.json` 的 `context_window[model]`（校验为正整数）→
② 内置表 `provider::builtin_context_window(model)` → ③ `RunLimits::default().context_window` = **128_000**。
`Config::context_window()` 每次调用重算；`app_loop` 在每个 `Request::Prompt` 开头
`agent.set_context_window(config.context_window())` —— **单一刷新点**（不散在 login/set_model）。

**阈值公式（`compact.rs`，默认值 0.85 / 0.10 / 0.01 / 0.10 / 4096 / headroom 1024）**：

```
keep_budget(w) = max(min(w*keep_ratio, w*keep_max_ratio), w*keep_min_ratio)
effective_reserve = min(output_reserve + HEADROOM, w - keep - 1)   // 小窗口下钳制
trigger_at(w)  = min( floor(w*trigger_ratio), w - effective_reserve ).max(keep + 1)
```

- **`trigger_at > keep_budget` 是承重不变量（P1）**：一旦违反，`plan()` 的尾扫 `keep_from` 恒为 0 ⇒
  压缩每轮空转、永不推进。`trigger_ratio(0.85) > keep_max_ratio(0.10)` 本应天然成立，但
  **小窗口下 `w - output_reserve - headroom` 会被 `saturating_sub` 归零**（window ≤ 5120），
  故预留量先钳到「不吞掉保留下限」，触发点再兜底 `.max(keep + 1)`。测试含 1..=20000 的**密集扫描**，
  不只采样点。`w = 1` 与 P2（触发点 ≤ `w × ratio`）不可同时成立，实现**以 P1 为准**。
- `keep_budget` 用 `max/min` 而非 `f64::clamp`（后者在配置越界或 NaN 时 panic）。

**切分（`plan()`，纯同步、绝不碰 model）**：从最新一条往回扫，累计到 `keep_budget` 为止。
两条硬不变量：
- **B-1 两段非空**：`keep_from` 同时 `.max(1)`（摘要段 ≥ 首条）与 `.min(len - 1)`（保留段 ≥ 末条）；
  `len < 2` → `None`。旧实现只做尾扫 —— **末条自己超预算时保留段为空，整段会话连本轮用户消息一起被摘要掉**。
- **P1-2 保留段首条不得含 `ToolResult` 块**：算出 `keep_from` 后**只向前推进**跳过含 `ToolResult` 的消息
  （上界 `len - 1`）。推到末尾仍非法（末条自己就是 tool_result）→ **返回 `None`（不压缩）**。
  **由来**：切点只按 token 尾扫、不看语义 ⇒ 保留段可能以一条 `ToolResult` 消息开头，其对应 `ToolUse`
  已被摘要掉；适配器会为它推 `{role:"tool", tool_call_id}`，前无 `assistant.tool_calls` ⇒ **provider 400**。
  而会话已被 `replace_all` 成这形状并落盘 ⇒ **之后每回合都 400，会话被锁死到 `/new`**。
  这不是「压缩失败」，是「压缩成功后主调用硬失败」。`BasicLoop` Step 10 把工具结果打包成**一条
  `Role::User` 消息** append ⇒ 倒数第二条通常正是 tool_result，切点很容易落上去，**不是罕见形状**。
  `while` 的顺序承重：**先套 B-1 的钳制、再在钳制之上前推**（前推只增大 `keep_from`，两条不变量同时保住）。

**三个 bug 修复**（修 bug，不改算法结构）：

| # | 症状 | 修复 |
|---|---|---|
| **B-1** | 保留段为空 ⇒ 整段历史连同本轮输入被摘要掉，一条不留 | 两段非空硬不变量（见上） |
| **B-2** | 模型回 tool-call（响应无 `Text` 块）时旧实现 `Ok(String::new())` ⇒ 写出**空摘要**，历史静默丢失 | 拼接全部 `Text` 块后 `trim().is_empty()` → **`Err`** |
| **B-3** | 压缩失败时 `clear()` + 写一行 `[Compression failed — proceeding with truncated history]` | **失败零副作用**：`execute` 自己什么都不改、返回 `Err`；`replace_all` 由**调用方**做（R3），故失败时 session **一个字节未动**，本轮照常继续 |

**`execute()` 的覆盖性校验必须是显式的、release 下同样生效（不能 `debug_assert`）**：
`plan.len == messages.len()`，且两区间**无缝且完整覆盖 `0..len`**
（`summarize.start == 0`、`summarize.end == keep.start`、`keep.end == len`），否则 `Err`。
`CompactPlan` 是 `pub`、doc 明说「纯数据、可逐字段构造」——下一片 `/compact` 正是被鼓励「手搓 plan」的
第二调用方。`debug_assert_eq!` 在 release 下被编译掉 ⇒ 陈旧/畸形 plan 会返回 `Ok` 并**静默丢弃尾部或中间消息**
（调用方随后 `replace_all` 即真实丢历史，违反 B-3）。

**删掉合成 assistant ack（`"I have the context summary. Continuing."`）**：它是**伪造的模型输出** ——
会被写进 JSONL、重启后当真实发言重放、`/export` 带出去、下次压缩再当真实历史摘要一遍；
且它想解决的「user/assistant 交替」**根本没解决**，只是把冲突挪到 `kept[0]` 是 Assistant 那一侧。
**改用构造保证交替**：`kept[0]` 是 `User` ⇒ 摘要**并入**它（前缀）；否则摘要作为**独立 `User` 消息**插在最前。
摘要消息角色仍是 `Role::User`（ADR-0017 决策 12：改角色需新决策）。测试用
`assert_no_synthetic_assistant`（每条 Assistant 必须在输入里逐字存在）钉死这一点。

**摘要 prompt 的形状**：`CompactPolicy.summary_system_prompt` 是**覆盖口子**，默认值 `DEFAULT_SUMMARY_SYSTEM_PROMPT`
是**两家之合并** —— **节骨架取 pi**（七节固定 Markdown：`## Goal` / `## Constraints & Preferences` /
`## Progress` / `## Key Decisions` / `## Files` / `## Next Steps` / `## Critical Context`），
**内容规则取 Codewhale**（*"exact file paths, commands, and error text"* 逐字引用 +
*"Summarize the task, not the checkpoint machinery"* 不描述压缩机器）。
**最初落地版曾是逐字 Codewhale 的 `COMPACT_PROMPT`**（bullet 式 handoff 指令）——内容要点对，但
**没有结构**：接手模型拿到的是一坨 bullet，无从预期「哪段在哪」；改成固定节骨架后才有可预期的
解析结构（详见「偏离」8）。
**摘要调用刻意仍是「独立 `ModelRequest` + `policy.summary_system_prompt`、`tools: vec![]`、`max_tokens` 不设」**：
「复用活对话前缀 + `tool_choice`（K-1）」是**片 2** 的事（有单测 `summary_request_shape_is_unchanged_in_this_slice` 钉住本片没越界）。
`max_tokens` 不设是因为 app 侧写死 `config.max_tokens.or(request.max_tokens) = Some(4096)`，设了会被静默覆盖，是假话。

**事件只有两个终局、不发「开始」**：`ContextCompacted`（含 `estimated_tokens_* / summarized / kept`）与
`ContextCompactionFailed { reason }`。**不发 `Started`** —— 它会引入悬挂状态：压缩中途被 `Abort`
（Step 4b `await` 窗口）时终止事件永不发出，transcript 会永远挂着「Compacting…」。两者都是**非终局事件**，
不影响 `-p` / `--json` 的消费循环退出条件。**手动 `/compact` 不发事件**（`Request::Compact` 臂不在 `select!` 里，
emit 的事件会躺在 `sink_rx` 直到下个 `Prompt` 才转发 → 顺序错乱），改走 `Outbound::Output`，但**底层是同一份 `CompactOutcome`**。

## 与设计的偏离

1. **`CompactError::NoModel` 未实现** —— `RuntimeContext.model` 本就非 `Optional`（`&dyn Model`），
   枚举里没有这个变体也没有意义。真正「无 model」的判定在 app 侧 `/compact`（`Wiring::ports().model == None`
   → 返回一行提示，**绝不退回 `clear_session`**）。
2. **事件字段命名 `estimated_tokens_before/after`**（设计草稿写的是 `tokens_before/after`）——
   `estimated_` 前缀是**有意的**：这两个数来自启发式估算器（`token.rs`：ASCII 字节 / 4 + CJK 每字 1.5），
   **不是 provider 回报的真实 `usage`**，命名不得简化（`ys-event` 的 doc 与 TUI 文案的 `~` 前缀同一条约束）。
3. **`Wiring::clear_session` 被删除**（设计未要求删）—— 失去唯一生产消费者后成 `dead_code`，
   且它本身是「压缩退回清空」的**诱惑**（`capabilities::compact` 里有两处注释专门警告别退回去）。
   `session_clear_empties_history_but_keeps_file` 用例保留，钉的是底层 `Session::clear` 语义。
4. **调用点走 `ys_loop::{plan, execute}` 根级再导出** —— `compact` 是**私有模块**，`lib.rs` 用
   `pub use compact::*;` 再导出。app 侧不写 `ys_loop::compact::plan`。
5. **评审新增的两个 P1 修复**（超出 ADR 决策 7 的三条）：
   - **P1-1 `execute` 覆盖性校验显式化** —— `review.md` R10 原处置写的是「`CompactPlan` 带 `len` +
     `debug_assert_eq!`」，实现**升级为 release 下同样生效的显式校验**（见上「关键行为」）。
   - **P1-2 保留段首条不得含 `ToolResult`** —— 见上「关键行为」，ADR 决策 7 未涵盖。
6. **`JsonlSession` 加了 `#[cfg(test)] flush_count: AtomicUsize` 字段** —— 为**可确定性地数落盘次数**，
   钉住 `replace_all` 只触发一次 `flush_to_file`（防 O(n²) 退化回归）。
7. **`KNOWN_MODEL_WINDOWS` 只填了确实可查证的模型**：`deepseek-chat`/`deepseek-reasoner` = 128K、
   `MiniMax-M3` = 1M、`MiniMax-M2.7` = 204,800。**`MiniMax-Text-01`（minimax provider 的 default_model）
   有意不填** —— 厂商口径「训练 1M / 推理可达 4M」差 4 倍，**API 实际放行哪个无法确证**，
   宁可回退 128K（偏保守：提前压缩），也不编造一个可能让上下文溢出的值。
8. **摘要 prompt 是 pi 骨架 × Codewhale 规则的合并版** —— ADR-0017 决策 2 与
   `design-compaction.md` §摘要内容策略 要的是 pi 的 Markdown 节骨架；**最初落地版曾是 Codewhale 的
   `COMPACT_PROMPT` 逐字**（一条 bullet 式 handoff 指令，**没有 `##` 节**），后修正为
   **七节固定骨架**（`## Goal` / `## Constraints & Preferences` / `## Progress` / `## Key Decisions` /
   `## Files` / `## Next Steps` / `## Critical Context`）**＋ Codewhale 的两条规则**（逐字引用路径/命令/
   错误文本、「不描述 checkpoint 机制」）。比设计多出 `## Files` 一节 —— 它是**避免「凭印象重读/重写
   已改文件」的唯一直接入口**，承重，勿在「美化」时合并或删除；`test_summary_prompt_pins_structure_and_rules`
   钉住七节与两条规则。`summary_system_prompt` 仍是覆盖口子。
9. **`config.rs` / `provider.rs` 的 `#[allow(dead_code)]` + 「尚未接线（U6b）」「当前尚无生产消费者」
   注释已过时** —— `load_user_config` 在 `main.rs:575-576` 有生产调用点，`context_window()` /
   `compact_policy()` 在 `main.rs:630-631` 与 `app_loop.rs:111` 有生产调用点。属性与注释没随接线清理，**读代码时别被误导**。

## 已知遗留 / 后续

1. **估算器未与真实 usage 对账，且不计 system prompt / 工具表**（系统性低估）——
   第二刀 `TokenLedger` 的目标，**须赶在 skills（D1）之前完成**。skills 落地会再加 ~9k 输入，低估风险放大。
2. **`replace_all` 先赋值后 flush**（`self.messages = messages; flush_to_file()`）：落盘失败时内存领先磁盘。
   有 `Err` 上报、可自愈，且与既有 `append` / `clear` **同形**（都是「先改内存后落盘」）。
3. **`plan()` 只约束保留段自洽**：若会话**首条**就是 `ToolResult`，它不受 P1-2 保护（正常会话不会这样）。
4. **`main()` 里 limits 装配：部分关闭** —— 已抽出 `main::resolve_limits(&config)` 并有单测
   `resolve_limits_reads_window_and_policy_from_config`（变异：把**函数体**改成 `RunLimits::default()` → 红）。
   **但抽取只钉住函数体，钉不住调用点**：`main()` 本身不可测，把 `main()` 里的 `resolve_limits(&config)`
   换成 `RunLimits::default()`，**全部测试仍绿**。故「装配真的用上 resolve_limits」**仍未闭合** ——
   除非把装配再上提一层做出可测的调用点，否则只能靠人工 review。
5. **[已关闭]「每回合刷新」现已覆盖运行中换模型** —— 新增
   `app_loop::context_window_is_recomputed_after_model_switch`：造出「切到更大窗口模型 ⇒ 下一回合按新窗口
   压缩」的用例（`prompt_refreshes_context_window_each_turn` 原先只用固定 `config.json`，覆盖不到换模型）。
6. **minimax 默认模型 `MiniMax-Text-01` 未收录** ⇒ 该 provider 下「1M 被按 128k 压缩」的症状残留，
   需用户写 `config.json` 覆盖。
7. **`/status` 的 context 值滞后一个回合** —— 窗口只在 `Request::Prompt` 臂刷新，`/login` / `/model`
   之后、下一次 `Prompt` 之前，view 里显示的仍是旧窗口。
8. **[已关闭]** `docs/design-final/coding-agent-tui.md` 里「`/compact` 仍是 `clear_session` 占位」
   （该文件 §8、§10 两处）的叙述**已随本刀更新为真压缩**。
9. **片 2 未做**：K-1（摘要复用活对话前缀 + `tool_choice`）、K-2（`Usage` 缓存字段 `cache_read_tokens`）、
   压缩超时。`tool_choice` 要动 `ys-model` 公开 API，须单独评审。

## 测试资产

- `crates/ys-component/src/compact.rs`：P1 承重不变量（采样点 + 1..=20000 密集扫描）、P2 上界、P3 大窗口锚点、keep 取值钳制
- `crates/ys-loop/src/compact.rs`：`plan` 早退**不调用 model**（替身一被调即 panic）、B-1 两种退化、切分边界精确、
  P1-2（复现 / 无合法切点 / 不误伤）、P1-1（陈旧 len / 有缝 / 覆盖不全 / 合法手搓 plan 放过）、
  B-2（tool-call / 全空白）、B-3（失败后输入未变 + **不多消费**响应）、R7（并入 vs 独立 + 零伪造 Assistant）、摘要请求形状
- `crates/ys-loop/src/lib.rs`：Step 4b 自动压缩成功（事件序 `ContextCompacted` → `RunFinished`、数字自洽、会话含摘要）/ 失败（session 逐字节不动、本轮照常）
- `crates/ys-session/src/{lib,jsonl}.rs`：`replace_all` 默认实现语义、`JsonlSession` 恰好一次落盘、空 vec 截断文件
- `apps/coding-agent/src/{config,provider,main,capabilities,app_loop}.rs`：三级解析优先级与非法条目丢弃、
  内置表、`build_agent` 挂 limits 回归、`/compact` 四路径、`prompt_refreshes_context_window_each_turn`
- ⚠️ **承重耦合**：`app_loop::every_request_variant_is_handled` 会走到 Step 4b，它安全**只因为**会话太小
  ⇒ 阈值不满足 / `plan()` 返回 `None` ⇒ `MockModel` 空队列不被消费（`remove(0)` 会 panic）。
  **护栏叠加三道**（阈值 / `len < 2` 的 `None` / `execute` 拒绝空区间），任意让压缩可能无条件调用 model 的改动都会打破它。
