# explore-speed — 架构上下文

> 日期：2026-10-14
> 性质：arch-explore 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「我发现 coding-agent 的 explore 查上下文的过程很慢，有没有什么改进办法？
> 能不能一次性获取很多需要的信息或者并发？其他产品是如何做的？是否需要 explore subagent？」
>
> **与既有文档的关系**：`docs/arch/tool-call-efficiency/`（2026-10-10）记的是「少绕弯的调用」，
> 其中 R3（只读批次并行）**已于 `d553740` 落地**；`docs/arch/subagent/` 是 subagent 的完整设计，
> 已把「用 subagent 做代码勘探以省父上下文」定为目标用例。本文件**不重复**这两份的结论，
> 只回答「**慢**」这一个新问题，并核对它们留下的现状漂移。

---

## 概述

「explore 慢」指模型为搞清代码上下文而连续发出的只读工具调用（`read` / `grep` / `find` / `ls` / `bash`）
所消耗的**墙钟时间**。本需求要回答的是：**这段时间花在哪里，哪些杠杆真的能动它。**

**先行结论（详见 §现状证据 与 §杠杆分析）**：

1. **并发这一条已经做完**，且**它不是 explore 慢的主因** —— 本地只读工具是毫秒级，
   一轮的开销几乎全是**一次模型调用**。所以「能不能并发」的答案是「已经能，但不解决慢」。
2. 真正的杠杆是**轮数**（每轮 = 一次模型往返）。本轮范围应围绕「**减少轮数**」而不是「提高并发度」。
3. **explore subagent 会让墙钟更慢**，本仓设计文档自己写明「subagent 不省 token，省的是父的上下文占用」。
   它是**上下文卫生**工具，不是**延迟**工具 —— 用它提速是方向反了。

---

## 现有架构

### 模块边界

```text
ys-core ──► ys-tool（Tool trait / ToolSpec / is_concurrency_safe / truncate / notice）
              ▲
              ├── adapters/tools-basic（read / write / edit / bash / grep / find / ls）
              └── adapters/tools-web（websearch / webfetch）
                      ▲
apps/coding-agent（toolset.rs 装配 / prompt.rs 派生提示 / subagent.rs 派发）
```

### 核心抽象（与本需求相关的三个闸门）

```rust
// crates/ys-tool/src/lib.rs:53 —— 并行准入，默认 false
fn is_concurrency_safe(&self) -> bool { false }
```

```rust
// crates/ys-loop/src/basic.rs:594 —— 批次并行的唯一准入判定
let parallel_ok = pending.len() > 1
    && pending.iter().all(|&i| ctx.registry.get(&tool_calls[i].1)
        .is_some_and(|t| t.is_concurrency_safe()));
```

| 闸门 | 常量 / 取值 | 落点 | 作用 |
|---|---|---|---|
| **并行度** | `DEFAULT_MAX_PARALLEL_TOOLS = 4` | `crates/ys-component/src/limits.rs:8` | 同批并发上限（`buffered(n)`，**保序**） |
| **单条输出** | `MAX_LINES = 2000` / `MAX_BYTES = 50KB` | `crates/ys-tool/src/truncate.rs:22,24` | 一条工具结果的上限 |
| **回合级输出** | `context_window / 4`（128k ⇒ 32k tok） | 设于 `apps/coding-agent/src/main.rs:529`，算法 `crates/ys-loop/src/turn_budget.rs` | 一回合内**多条**结果之和超预算 ⇒ 更早的结果被头部截断（40 行 / 2KB）+ 续读指针 |

### 关键数据流

```text
每一轮（Round）= 一次 Model.complete()
  BasicLoop Step 5  registry.specs().to_vec() → ModelRequest.tools   ← 工具描述每轮重发
  Step 7            append(assistant message，含 N 个 ToolUse block)
  Step 9            pending.len() > 1 且逐个 safe ⇒ 并行（buffered，保序）；否则串行
  Step 10+          turn_budget 降级更早结果
回合边界日志：`turn={} rounds={} elapsed={}s …`（apps/coding-agent/src/app_loop.rs:731）
TUI 显示：rounds + elapsed_secs（crates/ys-tui-coding/src/app.rs:265-277）
```

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `rg` / `fd` | `grep` / `find` 后端 | 缺失时 POSIX 降级 + 启动期提示（ADR-0015） |
| `sh` | `bash` 工具 | 每次新进程 |
| （无索引 / 无 embedding / 无新 crate） | — | 全仓**无**任何代码库索引或语义检索（`docs/` 只有 tree-sitter 出现在 markdown 渲染） |

---

## 现状证据（实测，2026-10-14）

### 采样口径

本机 `~/.yushan/sessions/*.jsonl` 共 **8 个真实会话**（2026-10-12 ~ 10-14，**均在 `d553740` 之后**
⇒ 下面的形态是**带 guideline、并行通道已通电**之后的形态），按 `content[]` 的 `ToolUse` 计数。
**无逐条时间戳** ⇒ 本采样只给**调用形态**，给不出耗时归因（这本身是缺口，见 §约束·可观测性）。

| 指标 | 值 |
|---|---|
| 工具调用总数 | **272**（`bash` 145 / `read` 69 / `grep` 44 / `edit` 10 / `ls` 2 / `find` 1 / `ask_user` 1） |
| 批大小分布 | 单调用 ×124、全只读多调用 ×20、含 `bash`/写的多调用 ×64 |
| `bash` 以 `cd ` 开头 | **112 / 163**（69%）—— 见下方更正 |
| `subagent` 调用 | **0** |
| 工具结果体量 | 均值 1.7–6.5 KB，单条最大 ~24 KB；单会话上下文最大 ~288 KB |

### ⚠️ 「模型不批」是错的 —— 要按模型拆开看

| 模型 | 批次总数 | 单调用 | 全只读多调用（**走并行**） | 含 `bash`/写（**退回串行**） |
|---|---:|---:|---:|---:|
| **deepseek-flash** | 64 | **10** | 8 | **46** |
| MiniMax-M3 | 117 | **104** | 11 | 2 |

两条结论，与直觉相反：

1. **`deepseek-flash` 的 84% 批次是多调用**（64 批里只有 10 批是单调用）⇒ **「一次发多个调用」已经在发生**，
   轮数红利**已经在拿**。**不批的只有 `MiniMax-M3`**（117 批里 104 批单调用）。
   ⇒ 「能不能一次多发」不是能力问题，**是模型行为问题**，且**因模型而异**。
2. **并行通道几乎没被用上**：全库只有 **20 / 208** 批次是全只读。原因是模型的批次**普遍混着 `bash`**
   （典型形态 `['bash','read']` / `['bash','grep']`），而 `bash` 不是 `is_concurrency_safe`
   ⇒ 整批退回串行。**但这只损失毫秒，不损失轮数** —— 信息仍在一轮内拿回。

**读法**：① 探索类调用占绝大多数，`edit` 只占 4% ⇒ 探索确实是成本主项；
② subagent 落地后**从未被自然使用过**；
③ **「批得多」与「省轮数」是同一件事，「批得多」与「走并行」不是** —— 后者只是毫秒。

### ⚠️ 更正：`cd <workspace> &&` 前缀**没有**被治净

本文档首版曾写「0 / 145，已被 `d49c66a` 治净」—— **那条是测量 bug**
（脚本读了不存在的 `input` 字段，实际字段是 `arguments`，故每条命令都被读成空串）。

**这条线不归本文档**，已有唯一的真值来源，本节只留指针：

- **度量**：`scripts/bash-cd-rate.py`（`python3 scripts/bash-cd-rate.py --workspace <路径>`）
  —— 零成本的长期尺子，带明确的度量边界（跨模型不可比、跨任务不可比、旧格式无 meta）。
  **本文档不再自留 `cd` 比率数字**，避免两处真值分叉。
- **结论与修法**：`docs/adr/0039-bash-redundant-cd-fact-echo.md`
  —— 措辞通道不稳定（`d49c66a` 变体实验 100%→0%，落地后实测 97%）；
  缺的不是「告诉它」，而是**旁证**（模型拿不到「你在哪」的回执）。修法落在工具侧：冗余时回一行事实。

**对本文的意义只有一条**：`1791990139` 那个会话的 26+24 轮探索里，绝大多数 `bash` 调用都带着
这个 31 字符的冗余前缀 —— 它是**探索轮数之外**的第二种每轮成本，但**属于 `bash-cwd-contract` 那条线的账**。

### 「一轮 ~6 秒」这个数从哪来

`docs/adr/0027-completion-check-and-round-budget.md` 的背景记了一次实证：
一次 `/arch-explore` 在 `⚠ 10 rounds · 60.9s` 处被切断 ⇒ **≈6.1 s/轮**。
一轮 = 一次模型往返（含思考 + 工具参数的生成），而本地 `read`/`grep` 是毫秒级。

> ⚠️ **这是单次观测，不是基线**：它来自另一台/另一次运行，模型与端点未必同今日。
> 但它给出了**量级**：轮的成本是**秒级**，工具的成本是**毫秒级**。

---

## 杠杆分析（本文件的核心结论）

把一回合的墙钟拆开：

```
墙钟 ≈ Σ(每轮：请求排队 + 前缀处理 + 生成) + Σ(工具执行) + 证据回喂带来的后续轮
        └────── 主导项（秒级 × 轮数）──────┘   └─ 毫秒级 ─┘
```

| 杠杆 | 对 explore 墙钟的预期收益 | 现状 |
|---|---|---|
| **A. 减少轮数**（一次调用覆盖更多目标 / 开局就有骨架） | **高**（每省一轮 ≈ 省数秒） | **未做**：`ls` 无 `depth`、`grep`/`find`/`read` 只吃**单个** `path` |
| **B. 每轮多发几个调用**（模型把独立目标放进同一条消息） | **中**（把 N 轮压成 1 轮） | **已通电**：guideline 在（`prompt.rs:224-235`），但采样里批次从没超过 3 |
| **C. 工具执行并行** | **≈0**（本地工具本就毫秒级；只对 `bash`/`web*`/`subagent` 有意义） | **已做**（`d553740`） |
| **D. 降低单轮延迟**（模型选择 / 思考预算 / 稳定缓存前缀） | **高**（直接缩放主导项） | 部分在做（缓存前缀经济学见 `docs/arch/prompt-delivery/notes.md`） |
| **E. 减少回喂 token**（更早的结果被截断） | 低–中（省 token 与压缩触发，间接影响延迟） | **已做**，且**与 A/B 直接冲突**（见下） |
| **F. explore subagent** | **负**（多一次子回合 + 父等待） | 已落地，设计目标是「省父上下文」 |
| **G. 代码库索引 / embedding** | 未知 | 不在本仓口径内，**且调研的 12 家无一采用** |

### 两条必须点明的冲突 / 缺口

**① 「一次拿更多」与「回合级工具结果闸门」互相抵消。**
`max_turn_tool_result_tokens = window/4`（`main.rs:529`）的语义是「一回合里工具结果之和超预算 ⇒
**截断更早的**，只留 40 行 / 2KB + 续读指针」。于是「一条消息抓 10 个文件」会**直接把前几个的结果削掉**
⇒ 模型要么再补一轮读回来（轮数回来），要么拿着残缺结果往下走。**A/B 与 E 是同一块预算的两端，
方案设计必须一起算，不能各自为政。**

**② 没有逐轮耗时读数 ⇒ 无法归因。**
今天只有**回合级**日志 `turn=… rounds=… elapsed=…s`（`app_loop.rs:731`）与 TUI 的回合级 `rounds/elapsed`
（`app.rs:265-277`），**没有任何逐轮（模型耗时 / 工具耗时）的切分**。因此在动手改之前，
「慢在模型还是慢在工具、慢在哪一轮」全是推测。**这是本需求唯一的前置障碍。**
（`--json` 模式有逐事件的 `Envelope` 流，但是否带时间戳需另查。）

**③ 一处过时注释（不承重，但会误导方案设计）。**
`crates/ys-loop/src/basic.rs:591` 仍写着「今天除 `subagent` 外没有工具声明 `true`」，
而 `read`/`grep`/`find`/`ls` 均已声明 `true`（`ls.rs:51` / `grep.rs:119` / `find.rs:62` / `read.rs:37`）。
无测试或代码把它当承重事实。

**④ 请求里从不发 `parallel_tool_calls`。**
`ChatCompletionRequest` 只有 `model / messages / tools / stream / stream_options / max_tokens /
temperature` 七个字段（`adapters/model-openai-compatible/src/request.rs:6-21`），**没有**
`parallel_tool_calls`，也没有 `tool_choice`。OpenAI 系端点该字段默认 `true`，**但各家兼容实现的默认值不一致**
⇒ 「模型只发一个调用」在某个端点上可能就是**服务端默认关掉了并行调用**，而不是模型不想。
⇒ 这是**一行就能试**的实验（显式发 `parallel_tool_calls: true`，看批大小分布是否变化），
但它需要**先有读数**（§范围内 P0）才能判断有没有用。

**⑤ 多 tool_call 解析结构性成立、但无测试。**
适配器按 `index` 累积到 `BTreeMap`（`adapters/model-openai-compatible/src/lib.rs:677-680`），
收敛为多个 `ContentBlock::ToolUse`（`:734-739`），`ys-session` 以泛型 serde 整条落盘重放。
但全仓**找不到**任何 `index:1` / 两个 `ToolUse` 的用例（唯一相关的 `lib.rs:1100` 只覆盖单调用分片）。
⇒ 在「让模型一次发更多调用」之前，**先补这条测试**，否则我们是在一个未被验证的通道上加压。

---

## 其他产品怎么做（读法：本仓已有的调研，不新做）

出处集中在 `docs/arch/subagent/research.md`（12 个项目对比）与 Pi 源码实测（`/Users/zs/Develop/YuShan/tmp/pi`）。

| 产品 | 同一条消息内多个 tool call | 只读探索子 agent | 代码库索引 |
|---|---|---|---|
| **Pi**（本仓对齐基准） | **默认并行**：`toolExecution ?? "parallel"`（`packages/agent/src/agent.ts:237`），preflight 串行、执行并发（`packages/agent/src/harness/runtime/drive/tools.ts:611`），可按工具退化为 `sequential` | **内置无** —— README 原文 "No sub-agents."（`packages/coding-agent/README.md:501`）；只有**示例扩展**，其中 `agents/scout.md` = 只读探索角色（`tools: read, grep, find, ls, bash`，模型 haiku） | 无 |
| **opencode** | 未见记载 | ✅ 内建 `explore`（只读）/ `general`；`subagent_depth` 默认 1 | 无 |
| **Gemini CLI** | 未见记载 | ✅ `codebase-investigator`（只读），`max_turns=30` / `timeout_mins=10` | 无 |
| **Codex CLI** | 并发**硬上限 4**，超限报错不排队 | ❌ **无**只读角色 | 无 |
| **Kimi / OpenHands / Amp / Roo** | 见 `research.md` 全景表 | ✅ 各有（`explore` / `code-explorer` / `oracle` / `architect`） | 无 |
| 统计 | — | **6/9 有内建只读角色** | **12 家全部为 0** |

三条可直接用的读法：

1. **「只读探索角色」是全行业共识**，本仓 `subagent`（子表无 `subagent` ⇒ 结构上只读不了写工具以外的东西）
   与 opencode / Gemini 的形状同源，**不是本仓的发明**。
2. **「并行执行同一条消息里的多个工具调用」与「并行派发多个 subagent」是两件事**，
   本仓已把前者通电（Pi 的做法是**默认并行**，不靠提示词），而各家文档里的「并行」多指后者。
3. **没有任何一家用 embedding / 代码库索引** —— 探索全靠 ripgrep 类字面检索。
   ⇒ 「加索引」在本仓不只是「引依赖」的问题，而是**没有同类先例**。

---

## 约束

- **技术**
  - **不新增依赖**是既定口径（ADR-0015）；`ys-core`/`ys-event`/`ys-protocol` 零 tokio。
  - 工具参数面是**对模型可见的契约**：改 `ToolSpec` 必须同步 `prompt.rs` 的 snippet/guideline
    与两侧测试（`EXPECTED_ORDER`、准入表 `toolset.rs:515`、漂移断言）。
  - **不新增工具**是倾向（`tool-call-efficiency/context.md` §范围外）：先看能否用既有工具的参数面解决。
  - 布局快照若做，必须有硬上限且自带「快照可能过期」的时效声明。
- **成本 / 性能**
  - 任何**常驻**文本进 `fixed_prompt_tokens`，**参与压缩触发比较**（`main.rs:541`、`assembly.rs:121`）
    ⇒ 新增文本按 token 计价，不能以「反正有缓存」为由无限加。
  - 输出上限（2000 行 / 50KB / 回合级 window÷4）不得放宽。
  - **本需求的收益指标只能是「同任务的轮数 / 墙钟」**；token 是伴随变量，不是目标。
- **演进**
  - 工具表顺序与 `prompt.rs` 的 snippet 表由测试钉住；子表 = 父表的**字节前缀**（缓存相干），
    改共享工具描述会同时改父子两侧。
- **组织 / 安全**
  - 无沙箱、无审批（项目硬约束）⇒ 只读是唯一的内建安全边界；`bash` 是逃生口，不能靠它兜底。
  - 关键逻辑要能**变异测试**；测试隔离用 `test_env::env_lock()` + 唯一临时目录。

---

## 需求范围

### 已澄清（2026-10-14 用户裁定）

| 问题 | 裁定 | 含义 |
|---|---|---|
| 慢的形状 | **轮次太多** | 不是单轮慢、也不是上下文涨大后慢 ⇒ 优化落在**减少轮数**，不在模型/端点 |
| 目标场景 | **人在 TUI 里等** | 收益指标 = **墙钟 / 轮数**；token 是伴随变量 |
| 工具参数面 | **本轮不动，先只补读数** | `ls depth` / 多 `path` 推迟；不先改对模型可见的契约 |
| explore subagent | **重新设计它的触发方式** | 认为它该被自动触发；**另开需求**（见下「与本文分析的张力」） |

### 范围内

1. **P0 · 补逐轮读数（本轮唯一落地项）**：把逐轮耗时按**模型 / 工具分开**落到既有日志
   或 `--json` 的事件流里。判据很具体：拿 2–3 条真实 prompt 跑一遍，能回答
   「这一轮 6 秒里，模型占几秒、工具占几秒」。
   **这是既定前提**：轮次是主成本（用户已确认），而当前连「一轮有多贵」都只有一次外部观测
   （ADR-0027 的 60.9s/10 轮）。补上它，「减少轮数」的每条候选才有可比的口径。
   - 反向约束：读数本身不得进常驻前缀（不得扰动 `fixed_prompt_tokens` 与缓存前缀）；
     优先复用既有 `Envelope` / `logging::log` 通道。
2. **P1 · explore subagent 的触发方式（另开需求，本文只记边界）**：见下节。
3. **暂缓（本轮不做，记账留档）**：参数面（`ls depth` / 多 `path`）、
   开局布局快照、提示词增补、既有并行通道的使用率对照、多 tool_call 的测试补课。

### 与本文分析的张力（explore subagent 触发方式）

用户裁定要**重新设计 subagent 的触发方式**（倾向自动触发）。本文的分析给出两条必须一起算的前提，
新需求若忽略它们，很可能做了却不见效果：

1. **对墙钟是负的**：本仓设计文档自己写明「subagent 不省 token，省的是父的上下文占用」
   （`docs/arch/subagent/design.md:237-241`）。一次派发 = 另一个完整子回合 + 父等待，
   **总轮数只增不减**。在「人在 TUI 里等」这个已确认场景下，它**不会**让用户更早拿到答案。
2. **它唯一可能的墙钟红利**：多个**真正独立**的探索任务并行派发（父等 1 份而不是 N 份）。
   但这个红利与「父在一条消息里并行发 N 个 `grep`/`read`」是重叠的 —— 后者成本近乎为零（毫秒级），
   且不牺牲可见性（子 agent 的事件**根本不进父事件流**，`subagent.rs:370-371` + ADR-0022 决策 2，
   排障只能看落盘转录）。⇒ **新需求必须先证明「为什么并行 subagent 优于父自己并行发调用」**，
   否则它买到的是上下文卫生（真价值），不是速度。

3. **且现状有一个未解释的事实**：subagent 已落地（刀 0–6），但 5 个真实会话 **0 次使用**。
   在做「自动触发」之前，先要弄清这是「提示词没教会」（可修）还是「模型判断不值得」（合理）。
   ADR-0022 已否决过若干触发形状（`mode` 字段 / roster / 后台 detached / fork），
   重新设计必须先与那份 Considered Options 对账。

### 范围外（明确不做）

- **把「提高并行度」当作提速手段**：本地只读工具是毫秒级，收益 ≈ 0；且通道已通电。
- **代码库索引 / embedding / 向量检索**：调研的 12 家无一采用；且属新依赖。
- 放宽任何输出上限（2000 行 / 50KB / 回合级预算）。
- 加沙箱 / 权限 / 审批（安全外置）。
- 持久 shell / 跨调用 cwd（破坏无状态与并行准入）。
- 改模型侧做「每轮注入」（`docs/arch/prompt-delivery/notes.md` §2 的缓存经济学）。

### 关键场景（成功路径）

- **S1 · 开局定位**：用户说「改一下 X 模块」⇒ 模型第一轮就拿到布局骨架，而不是
  `ls .` → `ls docs` → `grep` 三轮。
- **S2 · 多目标阅读**：读 5 个文件的特定区间 ⇒ 一条消息 5 个 `read`（并行 + 保序），
  而不是 `sed -n a,b f1; sed -n c,d f2; echo ===`。
- **S3 · 大范围检索**：一次 `grep` 覆盖多个目录，结果不被回合级闸门削掉头部。

### 异常 / 边界场景

- **回合级闸门削头**（§缺口①）：一批抓太多 ⇒ 更早的结果被截断 ⇒ 模型补读 ⇒ 轮数反弹。
- 模型**仍然**用 `bash` 兜底：改动只降低概率；验收看形态比例，不能假设某条路径必然发生。
- 混合批次（读 + 写）必须仍走串行；不得为提速放宽准入。
- 仓库很大（monorepo）：快照与 `ls depth` 都必须有硬上限。
- 批次执行中途**不响应 abort**（`basic.rs` 无批内 abort 探针）—— 一次大批次会拖长取消的响应时间。

---

## 未澄清问题

**已裁定（2026-10-14）**：慢的形状 = 轮次太多；目标场景 = 人在 TUI 里等；
参数面本轮不动；explore subagent = 重新设计触发方式（另开需求）。详见 §需求范围。

剩余待确认：

- [ ] **Q5 · 布局快照**：**这是「减少轮数」最直接的候选**（开局就给出骨架 ⇒ 省掉前 2–3 轮探索），
      做不做？形态（顶层+深度2 / `rg --files` 前 N 行 / 两者）与预算（如 ≤1k est token，常驻计入
      `fixed_prompt_tokens`）？**待用户确认**（`tool-call-efficiency/context.md` Q3 同一问题，至今未裁定）
- [ ] **Q6 · 验收口径**：接受「同一批 prompt 的前后对照（轮数 / 墙钟 / 模型-工具耗时切分）」
      还是只能定性？现在**没有任何基线** —— 若要做前后对照，基线必须在改动**之前**采。**待用户确认**

---

## 后续建议

1. **本轮收口在读数（§范围内 P0）** —— 交 `arch-design` 或直接小步落地。设计要点就三条：
   ① 逐轮切分模型 / 工具耗时；② 不扰动常驻前缀与 `fixed_prompt_tokens`；
   ③ 复用既有 `Envelope` / `logging::log` 通道，不新增输出面。
2. **explore subagent 的触发方式另开需求**（`docs/arch/<新名>/`）—— 必须先用 §「与本文分析的张力」
   的三条对账：对墙钟为负、与「父自己并行发调用」重叠、以及「0 次使用」的原因尚未解释。
   并与 `docs/adr/0022-subagent-dispatch.md` 的 Considered Options 逐条对账（已有 12+ 条否决记录）。
3. **Q5 布局快照** 是「减少轮数」最直接的候选，但**应与读数同批设计**：它的收益（省几轮）要用读数
   才能证明，否则只是加常驻 token。若要做，先用 **`prototype`** 量真实 token 与轮数下降。
4. **基线要在改动前采**（Q6）：至少留一份「同任务轮数 / 墙钟 / 批次大小分布」的对照，
   否则本轮之后无法回答「有没有变快」。
5. §缺口③ 的过时注释（`basic.rs:591`）与 §缺口⑤ 的多 tool_call 测试属顺手可清的债
   —— 后者不影响本轮结论，但在任何「一次发更多调用」的方案前必须先补。
