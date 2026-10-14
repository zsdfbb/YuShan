# bash-cwd-contract — 架构上下文

> 日期：2026-10-13
> 性质：arch-explore 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「优化 ys-coding-agent，为什么**总是** `bash cd xxx`？」
> 承接：`docs/arch/tool-call-efficiency/context.md`（2026-10-10）的 **R1**，那一轮已由
> commit `d553740` 落了「把 cwd 契约写进 `bash` 描述」。**本轮的新事实是：那条修复只在部分模型上生效。**

## 概述

工具调用形态问题，不是功能缺失问题。`bash` 的每次调用都在 workspace 根起新 shell，模型却
**仍然**给命令加 `cd /Users/zs/Develop/YuShan && ` 前缀（该前缀恒为同一绝对路径，对行为零影响）。

要回答的是：**这个前缀的成因是什么、为什么 `d553740` 的修复没有消掉它、剩下能改的是什么。**

## 现状（问题证据）

### 采样（本机 `~/.yushan/sessions/*.jsonl`，真实开发会话，只读统计）

| 会话 | 时间 | 记录的模型 | bash 调用 | 以 `cd ` 开头 |
|---|---|---|---:|---:|
| `1791818019` | 10-12 23:16 | MiniMax-M3 | 22 | **0** |
| `1791819446` | 10-13 00:10 | MiniMax-M3 | 22 | **0** |
| `1791821478` | 10-13 00:17 | deepseek-flash | 4 | 1 |
| `1791906130` | 10-13 23:44 | 未记录（见下） | 34 | **34** |

四个会话**都在 `d553740`（10-10 21:02）之后**，即看到的是同一份已修复的工具描述。

- 最新会话 `1791906130` 的 34 次 bash 调用 **全部** 以
  `cd /Users/zs/Develop/YuShan && ` 开头（31 字符前缀 × 34 ≈ 1054 字符 ≈ 263 est token，
  且 34 次全是**同一路径**）。
- 该会话未在会话文件里记模型；据 `~/.yushan/state.json`
  （`last_active_model: "deepseek-flash"`，`last_active_provider: "opencode-go"`）与时间推断为
  **deepseek-flash**（与 `1791821478` 同族，形态一致）。**此推断待用户确认。**
- 前缀之后 31/34 条**用的是相对路径**（`docs/design.md`、`crates/...`）—— 这些相对路径**本来就**
  落在 workspace 根（见下「同源」），故 `cd` 是**纯冗余**。
- 该会话工具分布：`bash` 34 / `grep` 2 / `read` 1 / `ls` 1 —— 探索动作几乎全塞进了 bash
  （`sed -n`、`head`、`for f in ...; do`、`awk`），这与 `tool-call-efficiency` 的 **R2** 同形，
  也是「需要 cd 的场合变多」的放大器。

### 关键对照：同一份描述，两个模型

MiniMax-M3 的 44 次 bash 调用里 **0 次** cd，且多数直接用相对路径（**理想形态**）；
deepseek-flash 的 38 次里 **35 次** cd。⇒ **`d553740` 的修复对前者充分、对后者不充分**，
「写描述」这条通道的效力**依赖模型**。用户当前用的是后者。

> ⚠️ 采样规模小（单仓、4 会话、2 个模型），只作**定性**依据。结论不依赖具体比例，依赖的是
> 「同一份描述下 0/44 与 35/38 的形态差」这一事实。

### 新证据：不是「上下文忘了」，而是**开局假设**

用**未改的代码**跑了一次基线（`--json`，deepseek-flash，输出 `tmp/baseline-adr.json`）：

- prompt「列出 `docs/adr` 下所有 ADR 文件的标题」→ 3 次调用：`find`（相对 `docs/adr/*.md`）
  → `bash`（`for f in docs/adr/*.md; do …` —— **无 cd、相对路径**）→ `read`。**0 次 cd。**

⇒ **cd 不是确定性的**：同一模型换个任务就不出现。「总是 cd」是**任务形态相关的高频倾向**，
不是恒定行为；验收口径必须按比例看（见 Q4）。

而 `1791906130` 的**逐轮时序**（`#n` = 会话第 n 条消息）定位了它出现的**位置**：

| 轮 | 动作 |
|---|---|
| `#2`（**第一次**工具调用） | `ls {path:"/Users/zs/Develop/YuShan/docs"}`、`grep {path:"/Users/zs/…/docs"}` —— **结构化工具里也钉绝对路径** |
| `#4`（第一次 bash） | 已经是 `cd /Users/zs/Develop/YuShan && git log …` |
| `#6`–`#44` | 每条 bash 一律 `cd <根> &&`，**内部却用相对路径**（`docs/design.md`、`crates/…`） |
| `#34` | 唯一一次 `read` 也是绝对 `path` |

两点结论：

1. **从第一次调用起就在钉位置** —— 不是长上下文把 cwd 挤掉了，而是**开局的默认假设**：
   「我不知道自己在哪，所以每次动作自己钉一次」。
2. **结构化工具钉绝对 `path` 与 bash 钉 `cd` 是同一行为的两种表现** ⇒ 只改 `bash` 描述治不了它。

## 根因链（每一环都可指到代码）

**R1 · 机制上 `cd` 是真必要的 —— 只要模型不确定自己在哪。**
`BashTool::call` 每次 `Command::new("sh").arg("-c").current_dir(&self.workspace)`
（`adapters/tools-basic/src/bash.rs:115-118`）。**没有持久 shell**，`cd` 确实不跨调用。
⇒ 模型若想「保证在根目录」，唯一能做的是在**同一条命令行**里 cd。**它的行为是合理的**。

**R2 · 没有任何反馈回路告诉它「已经到了」。**
工具结果 = stdout + stderr + 截断通知 + 非零退码（`render_result`，`bash.rs:23-66`），
**不回显 cwd**。模型无从验证，于是每次调用**重新钉一次**。`cd` 的习惯因此被持续再确认。

**R3 · 描述里的契约带了逃生口，且禁令是削弱式的。**
现行描述（`bash.rs:92-107`）：

> Every call starts a fresh shell in the workspace root (the system prompt's
> `Current working directory`) — `cd` does NOT carry over between calls, so **do not prefix
> every command with `cd`**; use absolute paths, **or chain `cd dir && cmd` inside the one
> call that needs it.**

两处可被合理绕过：① 「do not prefix **every** command」是**削弱式禁令**（"并不是每一条都要"），
不是「不要以 `cd <workspace> &&` 开头」；② 紧接着**明确许可** `cd dir && cmd` ——
一个不确信 cwd 的模型，会把「这一条也需要」判给自己。**描述同时说了「别做」和「可以这样做」。**

**R4 · 描述的另一条建议制造了**另一种**噪音。**
「use absolute paths」被 MiniMax 采纳过（`17918180` 里 7/22 条含绝对路径、共 13 处
`/Users/zs/Develop/YuShan`）。这与 `cd` 前缀是同一笔税的两种付法 —— 而**真正的答案是第三条路**：
**相对路径本来就落在 workspace 根，既不用 cd 也不用绝对路径。描述没说这句。**

**R5 · 系统提示里那句话太弱、且位置靠后。**
`prompt.rs:69` 只在整条提示里追加一行 `Current working directory: {}`，
且位于 `<project_context>`（本项目即一整份 `AGENTS.md`）**之后**、`<available_skills>` 之前。
它陈述的是「进程的 cwd」这一**事实**，不是「bash 每次在这里起、相对路径都在这里解析」这一**契约**。

> 附：命中自定义提示（`<cwd>/.yushan/SYSTEM.md` 或 `~/.yushan/SYSTEM.md`）时
> `build_system_prompt` **整体短路**（`prompt.rs:29-32`）⇒ **这一行连同它一起消失**，
> 而 `bash` 描述里「the system prompt's `Current working directory`」的**交叉引用也随之落空**。
> 本机当前**没有** SYSTEM.md（已核），故不是本次的活跃成因，但它是这条链上的边界情况。

> **用户 2026-10-13 确认：这一环是主因，落点就是「开始的时候就告诉它当前目录在哪里」。**
> 提示头现在是 `You are a coding agent. …` → `Available tools:` → `Guidelines:`，
> **完全没有环境信息**（`prompt.rs` 里无任何 `platform` / `os` / `uname` 字样）；
> cwd 只以一行事实陈述出现在 `<project_context>` 之后 —— 一个几 KB 的 `AGENTS.md`
> 会把它推到提示的**中后段**，且它**不是**最后一段（`<available_skills>` 还在后面）。

### 模型每轮知道自己在哪吗（2026-10-14）

**知道 —— 而且比「知道」更强：它每轮都被重新告知。**

- 每轮请求都带完整 system prompt：`crates/ys-loop/src/basic.rs:331` 的
  `system: ctx.system_prompt.clone()` → 适配器把它作为 `messages[0]` 的 `role:"system"`
  发出（`adapters/model-openai-compatible/src/lib.rs:782-785`）。
  ⇒ 那行 `Current working directory: X` **每一轮都在**，不是只在第一轮。
- 值是**装配期冻结**的，且**会话中途不可能变**：`BashTool::workspace` 构造时定死
  （`bash.rs:13,118`）；工具表 / `working_dir` / system prompt / child_prompt 同源
  （`assembly.rs:96-118`）；只有 `/new`、worktree attach 才换工作区，且**四者一起重建**
  （`app_loop.rs:485`）。⇒ **那行永远是真的。**

**缺的不是陈述，是旁证** —— 它拿到的是**一句陈述**，不是**一个可观察的事实**：

| 选项 | 成本 | 收益 |
|---|---|---|
| 相信提示 | 0 | 赌「它没变」（实际确实没变，但模型无从知道） |
| 自证：`cd <根> && cmd` | **31 字符**（同一调用内） | 确定 |
| 求证：跑一次 `pwd` | **一整个来回**（一次模型调用 + 工具执行） | 确定 |

⇒ **求证的成本比自证高两个数量级** ⇒ 永不求证，永远自证。**这正是理性选择。**

**且「每轮知道」≠「每轮注意到」**：请求里确实有那行，但历史每轮变长，那行的相对显著性下降
⇒ 与形态相关性（长任务 34/34、简单任务 0）吻合。

> **对方案的含义（重要纠正）**：既然那行**每轮都在**，「前置到开头 / 再陈述一遍」改善的只是
> **显著性**（边际收益有限）；而 **③ 结果回显 cwd 改变的是信息的种类** —— 从「一句要相信的
> 陈述」变成「一个可观察的事实」，**从根本上删掉「要不要赌」这个问题**。
> ⇒ **③ 不是「再告诉它一次」，是换一类信息，优先级应高于位置调整。**

### 如何让模型「相信」——pi 的答案是**不要求它相信**（2026-10-14）

**pi 的逐字原文**：

| 位置 | 原文 | 做了什么 |
|---|---|---|
| 系统提示末尾（`:165`） | `Current working directory: ${promptCwd}` | 一行事实，**没有任何劝说**，也没有「不要 cd」 |
| bash 描述第一句（`bash.ts:235`） | `Execute a bash command in the current working directory. Returns stdout and stderr.` | 把事实放到**动作旁边** —— 模型填 `command` 参数时眼前就是这句 |
| 同句后半 | `Output is truncated to last N lines or N KB… **If truncated, full output is saved to a temp file.**` | **不要求模型接受「被截断了」**，而是给一条自主补全的路 |
| 工具结果（`bash.ts:367`） | `content: [{ type: "text", text: outputText }]` | **不回显 cwd** |
| 前置校验（`bash.ts:89-91`） | `Working directory does not exist: ${cwd}\nCannot execute bash commands.` | 事实不成立时**硬报错**，而不是静默在别处跑 |
| 每次调用（`bash.ts:250`） | `ctx?.cwd \|\| cwd` | **cwd 是参数**（留给扩展）—— 「我想在别处跑」有正当表达，不必用 `cd` |
| `session-cwd.ts` | 存的 cwd 不存在 ⇒ 报错 / 问用户 | 绝不静默沿用调用方 cwd（我们已有等价语义） |

**所以 pi 的答案不是「怎么写才能让它信」，而是：消除「必须相信」的环节。**
它把事实**放到动作旁边**、**在事实不成立时硬报错**、**不承诺任何需要信任的东西**。
pi **也保留了**「模型仍可能 cd」这个问题 —— 它没有禁令。

**让 LLM 相信的手段，按可靠性排序**（我们可从中挑）：

1. **不要它相信** —— 把「一句陈述」变成「一个可观察的事实」：结果回显 / 可自取的环境变量 /
   不成立即报错。**pi 走的就是这条**（只做到「动作旁边 + 硬校验」这一半）。
2. **让违反立刻有可见后果** —— 我们的路**基本被堵死**：`cd <工作目录>` **永远成功**，
   不报错、不失败、慢到察觉不到 ⇒ 零惩罚。要在这一格拿分，只能在**结果里给信号**。
3. **把约束放在动作旁边**（工具描述，紧挨参数 schema）—— pi 的 `bash.ts:235` 就是这个位置。
4. **措辞从「事实」改成「禁令 + 代价」**，且**删掉逃生口**（见上文第 2 条原因）。
5. **一致性** —— 同一事实在多个通道互相印证；**矛盾会摧毁它**（见下）。
6. **重复 / 前置** —— **最弱**：那行本来就每轮都在（见上一节）。

> **⚠️ 我们自己的提示里有一条反例**（实测逐字，`prompt.rs:115-116`）：
> 「Relative paths inside a skill body … resolve relative to that skill's own directory,
> **not the current working directory**.」
> 同一份提示里，一边说「cwd 是 X」，一边教模型「**相对路径的解析根是看情况的**」
> —— 这会削弱前者的权威性。
> （pi 有同型句式：`system-prompt.ts:141` 的 `resolve docs/... under Additional docs …
> **not the current working directory**` ⇒ 这是「存在多个路径解析根」的**固有代价**，
> 两家都有；不必消除，但**别再往这个方向加**。）

> **由此得出的候选 ③′（比无条件回显更省）**：**只在命令带了冗余 `cd <工作目录>` 前缀时才回显**
> —— 把噪音本身当触发器，在**行为发生的那一刻**给出「这本来就是工作目录，前缀是冗余的」。
> 好处：① 位置精准（在犯错的点上，不在系统提示里）；② **自我消灭** —— 模型一旦不 cd，
> 回显就消失，成本降到 0；③ 不占每轮固定前缀。
> 对照无条件回显：每结果 +≈40 字符 × 每次调用 > 要消掉的 31 字符/调用 ⇒ **无条件回显反而净增 token**，
> 这是原先候选 ③ 的隐藏成本，必须修正。

### 为什么「已经陈述了位置」还不够（2026-10-14）

**先排除送达问题**：system prompt 确实作为 `messages[0]` 的 `role:"system"` 发出
（`adapters/model-openai-compatible/src/lib.rs:782-785`）；`prompt.rs:69` 恒追加该行
（无 `SYSTEM.md` 短路）；`opencode-go` 网关只要求一个会话头、**不改写内容**
（`docs/design-final/opencode-go.md`）。⇒ **不是「没告诉它」**。

四条原因，按解释力排序：

1. **我们给的是「事实」，模型需要的是「约束」。** 那行逐字是 `Current working directory: X`
   —— 它回答「你在哪」，**没回答「所以你别做什么」**。禁令只存在于**另一条消息**里
   （工具表的 `description`），而且还要求模型**交叉引用**回去（"the system prompt's
   `Current working directory`"）。可模型生成 bash 调用那一刻，眼前是**参数 schema**，
   不是系统提示的第 69 行 ⇒ **信号在，但不在需要它的位置**。
2. **那条禁令自带逃生口，且是削弱式的。** 原文 `do not prefix every command with cd` 是
   「并不是每条都要」，紧接着**明确许可** `or chain cd dir && cmd inside the one call that
   needs it`。不确信自己在哪的模型，会把「这一条也需要」判给自己 ⇒ **我们同时说了
   「别做」和「可以这样做」**。
3. **没有反馈回路 ⇒ 模型无法验证 ⇒ 它选择自证。** 工具结果只有 stdout/stderr，
   **从不回显 cwd**。模型对「我在哪」只有提示里那一句话作证据，且那句是**会话开始时**
   生成的、它无从核实中途有没有变。而 `cd <根> &&` 成本 **31 字符**、收益是**确定正确**
   （cd 到根目录从不失败）⇒ 一边是「免费的赌注」，一边是「极便宜的保险」。
   **更要命的是它从不产生负反馈** —— 不失败、不报错、慢到察觉不到。**没有惩罚，习惯就不消退。**
4. **同一份陈述、两个模型、两个结果 ⇒ 陈述必要但不充分。** MiniMax-M3 0/44、
   deepseek-flash 35/38，看到的是**同一份已修复的提示**（见「关键对照」）。
   ⇒ 提示只是诸多信号之一，强度不足以压过该模型的默认习惯（多半来自「每条命令自成一体」
   的语料，那里 cd 是必要的）。**行为是模型相关的**，从提示侧只能提高概率。

**形态相关性**（同模型）：简单任务（列 ADR 标题）**0 次** cd，多目录批量探索的长任务
**34/34** ⇒ **不确定性越高、命令越长，模型越要钉住起点**。

**比例感**：`cd` 是**噪音，不是错误** —— 263 token/会话，从不导致失败。目标是降噪，不是修 bug。

**判别性实验（别一把全上）**：三条杠杆各测一条原因 ——
① 去逃生口 + 无歧义禁令（测第 2 条）；② cwd 挪到系统提示最后一行（测第 1 条的「位置」）；
③ 结果回显 cwd（测第 3 条「无反馈」）。**三个同时上就看不出谁起作用** ⇒ 建议分两批：
先 ①②（零机制改动），再 ③。

### 已澄清的既有事实（避免重复劳动）

- **`cd` 在语义上确为 no-op**：工具表 / `working_dir` / system prompt / **子 agent 的 child_prompt**
  全部同源于同一个 `workspace`（`apps/coding-agent/src/assembly.rs:96-118`；装配见
  `apps/coding-agent/src/toolset.rs:83-110`；换工作区时四者一起换，`app_loop.rs:485`）。⇒
  prompt 承诺的 cwd **就是** bash 的 cwd，**不存在**「prompt 说 A、bash 在 B」的分歧。
- **`d553740` 已落的三件事**：bash 描述写 cwd 契约、`read` 区间提示、只读工具并行
  （`is_concurrency_safe`）。R2（多目标参数面）与「布局快照」当时**未做**。
- **不去做持久 shell**：`tool-call-efficiency/context.md` 已定「破坏无状态与并行安全」为范围外，
  本轮的候选方案**同样不碰它**。

## 现有架构

### 模块边界

```text
ys-tool（Tool trait / ToolSpec / truncate / notice）
   ▲
   ├── adapters/tools-basic（read / write / edit / bash / grep / find / ls）
   └── adapters/tools-web
           ▲
apps/coding-agent
   ├── toolset.rs   build_tools(workspace) → Vec<Box<dyn Tool>>（顺序即真值）
   ├── prompt.rs    build_system_prompt(cwd, &specs, &catalogue)
   └── assembly.rs  三者同源：工具表 / working_dir / system prompt / child_prompt
```

### 关键数据流（决定「改哪里会动到什么」）

```text
启动期 / 换工作区（每进程或每次 /new、worktree attach）
  assembly::build(workspace)
    → toolset::build_tools(workspace)          ← BashTool::new(ws)：bash 的 cwd
    → prompt::build_system_prompt(workspace, …) ← 系统提示末尾那行 cwd
    → build_agent(system_prompt, workspace, …) ← working_dir
每轮（BasicLoop）
  Step 5  registry.specs().to_vec() → ModelRequest.tools   ← ★ 工具描述每轮随请求重发
  Step 9  批次执行：all-safe ⇒ 并行；否则串行
```

> **成本含义**（依据 `docs/arch/prompt-delivery/notes.md` §2）：常量文字进**稳定前缀**
> （system prompt / 工具描述）⇒ 缓存命中 ≈ **0.1×**。故「加一段常量文字进工具描述」的本钱
> 极低；反过来说，任何**每轮变化**的注入都是它的 **10 倍**，且会废掉前缀缓存。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `sh` | bash 工具 | 每次新进程；进程组 + `kill_on_drop` |
| （无需新依赖） | — | 本需求**不引入任何新依赖** |

## 约束

- **技术**
  - 不引入持久 shell / 跨调用 cwd（`tool-call-efficiency` 既定口径：破坏无状态与并行安全。
    同批多个 bash 并发时会互相踩 cwd，`is_concurrency_safe` 的准入也依赖「工具无共享状态」）。
  - 工具参数面 / 描述是**对模型可见的契约**：改 `ToolSpec` 必须同步 `prompt.rs` 的
    snippet / guideline 与两侧测试（含 `EXPECTED_ORDER`、工具表指纹
    `fp_tools = 0x505fa9711a7adcbc`，`toolset.rs:626`）。
  - 子 agent 的 tools JSON 是父表的**字节前缀**（命中缓存）⇒ 共享工具的描述改动
    **两侧一起变**，不影响该不变量。
- **性能 / 成本**
  - 新增常驻文本进 `fixed_prompt_tokens`（参与压缩触发比较）。按 token 计价，不是「反正有缓存」。
  - 工具结果侧的文字（若回显 cwd）**不在**稳定前缀的最前，但在历史里同样命中缓存；
    每轮成本远低于尾部注入，仍应按字符计价。
  - 收益指标是**同一任务的调用数 / 轮数 / 噪音字符数**，不是「某条路径必定发生」。
- **演进**
  - 描述与 snippet / guideline 由测试钉住（漂移即红）；改一处必须同步另一处。
  - 关键逻辑要做**变异测试**（本项目多个真 bug 是「测试全绿时依然存在」）。
- **组织**
  - 小团队；安全边界不在本需求内（项目硬约束「不做安全」）。
  - 测试隔离用 `test_env::env_lock()` + 含 `process::id()` 的唯一临时目录。

## 需求范围

### 范围内（候选，待澄清后取子集）

1. **加固描述措辞**（治 R3/R4）：把削弱式禁令换成**无歧义禁令**，删掉 `cd dir && cmd` 的许可
   与 `use absolute paths` 的建议，改为**正面陈述唯一正解**：「bash 已经工作目录为 cwd，
   **相对路径直接可用**；不要以 `cd <工作目录>` 开头、也不要写绝对路径」。
   同时把「the system prompt's `Current working directory`」这处**跨引用改为自足描述**
   （自定义 SYSTEM.md 会令该行消失）。
2. **把同一条纪律放进 system prompt 的 guidelines**（治 R5）：`prompt.rs::tool_guidelines`
   按 `tools` 派生，已有 `bash` 的档位可挂一行。相对工具描述是**另一条通道**，
   对「先读系统提示」的模型更有效。（成本：稳定前缀，缓存命中后 ≈ 0.1×）
3. **工具结果回显 cwd**（治 R2）：每次 bash 结果首行带 `[cwd: <path>]`。**建立反馈回路**，
   让「不需要 cd」变成可观察事实而非需要相信的断言。代价：每次调用的结果里多一段文字。
4. **机械剥离冗余前缀**（模型无关）：exec 前识别 `cd <workspace 绝对路径> && <rest>` 形态，
   剥掉后执行。语义等价（cd 目标是 bash 的既有 cwd，必成功），**零 token**、**不依赖模型配合**。
   代价：静默「容忍」模型行为，且形态匹配是启发式的（需限定：只匹配 == workspace 根的绝对路径）。
5. **`bash` 增加 `cwd` / `dir` 参数**（把隐含需求变成声明式参数）：让「我想在那个目录跑」有
   正当表达，而不必用 `cd`。**改的是对模型可见的参数面**（需过 ADR 与测试基线）。
6. **（承接上轮）R2 的多目标参数面**：这是 `cd … && (grep|sed|for …)` 成串出现的**上游**成因
   —— 模型为了「一次覆盖多目标、少一轮」才退回 bash 拼串。

### 范围外（明确不做）

- 持久 shell / 跨调用保留 cwd。
- 加沙箱 / 权限 / 审批（项目硬约束：安全外置）。
- 引新 crate 或新二进制依赖。
- 放宽任何输出上限（2000 行 / 50KB / limit 条）。
- 改模型侧：不做「每轮注入」（缓存经济学，`prompt-delivery/notes.md` §2）。任何新增文字
  必须是**常量**、进稳定前缀。
- 为「文风偏好」建源码扫描测试（`prompt-delivery/notes.md` §6 已立此立场）。

### 关键场景（成功路径）

- **S1 · bash 里跑真命令**：`cargo test` / `git log` 直接成句，**不带任何 `cd` 前缀、不带绝对路径**。
- **S2 · bash 里跑相对路径命令**：`sed -n '1,40p' docs/design.md` 直接成句（相对路径落在根）。
- **S3 · 探索仍用结构化工具**：多目标探索走 `read` / `grep` / `ls` 的**多条并行调用**，
  而不是 `cd root && for f in ...; do ...` 一条长串。

### 异常 / 边界场景

- 模型**仍可能**继续 cd：改动只降低概率，不保证 ⇒ 验收看**形态比例**，不假设某条路径必然发生。
- 用户**有意**在子目录跑命令（`cd crates/ys-loop && cargo test`）：这是**正当**用法，
  任何改动**不得**误伤 —— 剥离只能针对「cd 目标 == workspace 根」这一种形态。
- 相对路径与 workspace 根**不同**的场合（子目录切换、worktree）：**必须以 workspace 为准**，
  不得因为「剥离前缀」而让命令落到错误目录。
- 命中自定义 `SYSTEM.md`：系统提示整体短路 ⇒ 依赖系统提示那行的方案**在该用户上失效**，
  描述/结果侧的方案不受影响。
- 大 `AGENTS.md` 项目：`<project_context>` 会把 cwd 那行推得更深，进一步稀释其注意力权重。

## 系统提示头部：基础信息块（2026-10-14 用户提出）

> 用户原话：「考虑一下如何优化 coding agent 刚开始的系统提示词。大模型可能需要一些基础的信息。」
> 这是「cwd 前置」的**推广**：开局该给模型的「基本盘」有哪些。

### 现状：头部逐字只有角色一句话

```text
You are a coding agent. You help users with software engineering tasks.

Available tools:
- read: Read file contents
…

Guidelines:
- …
```

（模板见 `prompt.rs:44-52`。）整条提示**没有任何环境信息**：cwd 在尾部（R5）、无平台 / OS、
无 shell 类型、无日期、无 git 状态、无布局。仓库里也**没有任何 OS 探测**
（全仓 grep `consts::OS` / `target_os` 为空）。

### 候选内容（按证据强度排序）

| # | 基础信息 | 缺了会怎样（本项目里的实例） | 是否常量 |
|---|---|---|---|
| **E1** | **工作目录** + 「相对路径就落在这里」的**契约** | 本轮主问题：模型每步自己钉位置 | 常量（换工作区整条重建） |
| **E2** | **平台 / OS + shell 类型** | bash 跑的是 **`sh -c`**（`bash.rs:115`），**不是**用户的交互 shell ⇒ 模型写 `[[ ]]`、数组、`source`、GNU 专有 flag 会静默跑偏。本仓实测：macOS 上**没有 `timeout`**（采基线时刚撞到 `command not found`）；`grep`/`sed`/`stat` 的 BSD↔GNU 差异同理 | 常量（每机器固定） |
| **E3** | **日期（会话启动时）** | 模型靠猜「今天」。本仓的**指针纪律**（旧 ADR 只记 why、不能佐证 now）与「最新版本」类问题都依赖 now-ness | 会话内常量 |
| **E4** | **git：是否仓库 / 根在哪 / 当前分支** | 模型频繁 `git log`、`git status`；**从子目录启动时 `cwd ≠ 仓库根`** —— 这才是 `cd ..` 真正必要的场合，现在它无从知道 | 会话内常量（分支会变 ⇒ 须声明时效） |
| **E5** | **运行时能力边界**：无沙箱 / 无审批（命令与写入**真的**生效）、无持久 shell（`cd` 不跨调用）、工具是唯一行动通道 | 模型会问「要我执行吗」、或假设 `cd` 会保留 | 常量 |
| **E6** | **截断语义**：工具结果可能被截断（`[Truncated N lines.]` / `limit`），续读用 `read offset=` 或更窄条件 | 模型把「没看到」当成「不存在」。形态已在（`ys_tool::notice`），缺的是这条**契约** | 常量 |
| **E7** | **项目布局快照**（顶层 + 深度 1~2，硬上限） | 上轮 `tool-call-efficiency` 候选 4，当时未做。省掉开局探索轮 | 启动时拍，**会过期** |
| **E8** | **工作语言 / 文档语言** | 本仓要中文（来自 `AGENTS.md` —— 有 AGENTS.md 才有）；裸仓库无 fallback | 常量，但**产品级强加**，争议最大 |

### 竞品对照（实读 `tmp/pi` + 本会话自带的 Claude Code 环境段，2026-10-14）

| | **pi**（`tmp/pi/packages/coding-agent/src/core/system-prompt.ts`） | **Claude Code**（本会话系统提示里可见的那份） | **YuShan 现状** |
|---|---|---|---|
| cwd 位置 | **末尾**（`:165` `prompt += "\nCurrent working directory: …"`）—— **与我们完全相同** | 提示**靠后**的 `# Environment` 段 | 尾部（`prompt.rs:69`） |
| cwd 措辞 | 只陈述位置，**无「不要 cd」** | 只陈述位置 | 只陈述位置 |
| 平台 / shell | **无平台块**；但 **shell 名注入进 bash 描述**（`Execute a ${config.shellName} command in the current working directory`，`tools/bash.ts:235`），由 `getShellConfig()` 按平台解析（Windows 找 Git Bash、Unix 取 `/bin/bash` → 兜底 `sh`，`utils/shell.ts:67`） | `Platform: darwin` / `Shell: zsh` / `OS Version` | **无**；且 bash **硬编码 `sh -c`**（`bash.rs:115`），模型无从知道 |
| 日期 | 无 | `Today's date is …` | **无** |
| git | 无 | `Is a git repository: true` | **无** |
| 截断语义 | **写在 bash 描述里**：`Output is truncated to last N lines or N KB… full output is saved to a temp file` | — | 描述里无；结果里只有 `[Truncated N lines.]`，**且全文未落盘** |
| 角色段 | 点名 harness + 枚举能力（读文件 / 执行命令 / 改代码 / 写文件） | 类似 | 只有一句话 |

**三条从对照里读出来的判断**：

1. **「cwd 放头部」没有竞品先例** —— pi 在末尾、Claude Code 在中后段。但**缓存在两者上完全等价**
   （都在稳定前缀里，头/尾不影响命中）⇒ 差别纯粹在**注意力**。故前置是**我们自己的**判断，
   依据是「第一次工具调用在任何反馈之前就生成」；**成本为零、风险低**，值得自己试。
2. **E2（shell 类型）是有先例的做法**：pi 把 `shellName` 注入 bash 描述。我们**更该说** ——
   pi 用的是真 bash，我们**硬编码 `sh -c`**，模型按 bash/zsh 的习惯写（`[[ ]]`、数组、
   GNU 专有 flag）就会静默跑偏。
3. **E6（截断语义）pi 写在 bash 描述里，且比我们能给的更好**：它明说「完整输出已存临时文件」⇒
   模型可以去读。**我们没有这个能力**（`render_result` 截断即丢弃）⇒ 我们的文案只能是
   「续读靠更窄的条件 / `read offset=`」，**不能**承诺有全文可读。

### pi 的机制清单（实读 `tmp/pi`，2026-10-14）

| 机制 | pi 怎么做 | 对我们的意义 |
|---|---|---|
| **A · 工具自带提示贡献** | 每个工具导出 `*ToolSystemPromptContribution { snippet, guidelines }`（`bash.ts:43`、`edit.ts:44`、`read.ts:21`、`write.ts:17`、`ls.ts:16`），装配端收集成 `Available tools:` + `Guidelines:` | **结构上不可能漂移**。我们反过来：集中在 `prompt.rs` 按 name 查表，靠测试防漂移（`toolset.rs:626` 指纹 + `prompt.rs` 的漂移断言）。可作后续架构改进，**不是本轮必需** |
| **B · 运行期信息走环境变量自取** | bash 子进程注入 `PI_SESSION_ID` / `PI_SESSION_FILE` / `PI_PROVIDER` / `PI_MODEL`（`bash.ts:174-183`），配一条 guideline：`You can inspect PI_* environment variables for current model and session details` | **这是对「大模型需要基础信息」最省的答案：不占提示 token，模型自己问**。我们可对 `YUSHAN_*` 照做 |
| **C · shell 名进 bash 描述** | `Execute a ${config.shellName} command in the current working directory.`（`bash.ts:235`），`shellName` 由 `getShellConfig()` 按平台解析（`utils/shell.ts:67`） | **pi 把「跑在哪个 shell、在哪个目录」放在工具描述里，不放系统提示**。我们照此：在 bash 描述里说明跑的是 `sh -c` |
| **D · 截断契约写在工具描述里** | 同上那句续写：`Output is truncated to last N lines or N KB… full output is saved to a temp file`；`BashToolDetails.fullOutputPath` | pi **有能力**承诺全文可读；我们 `render_result` 截断即丢弃 ⇒ **只能写「续读靠更窄条件 / `read offset=`」** |
| **E · 统一 `resolveToCwd`** | 所有工具把相对 path 解析到 cwd（`edit.ts:162`、`path-utils.ts:resolveToCwd`），另带 macOS 文件名变体容错（NFD / 弯引号 / AM-PM 窄空格） | 与我们 `workspace` 同义。**文件名容错是 pi 独有的成熟件**，与本轮无关但值得记 |
| **F · cwd 是系统提示的最后一行** | `:160-165`：**skills 段之后**才 append cwd ⇒ cwd 紧邻第一条用户消息 | **与我们不同**：我们在 `:69` 就追加 cwd，**之后才追加 skills 段**（`prompt.rs:73-81`）⇒ 有 skill 时，cwd 与任务之间隔着整个目录段。**这可能正是「开始的时候告诉它」的真实含义** |
| **G · pi 没有的东西** | 无平台 / OS 块、无日期、无 git 状态、无布局快照、无「不要 cd」禁令 | 我的 E2/E3/E4/E7/E8 里，**E3/E4/E7 在 pi 里没有对应物**；E2 有对应物但**走 C 通道**（工具描述），不进系统提示 |
| **H · 恢复会话时的 cwd 校验** | `session-cwd.ts`：存的 cwd 不存在 ⇒ 明确报错 / 问用户，**不静默沿用调用方 cwd** | 我们**已有等价语义**（`app_loop.rs:485-537` 四情形「绝不回退成调用方 cwd」） |

### 我的建议（pi 对齐版 · 2026-10-14 用户定：**不加禁令**）

> **用户 2026-10-14 决定：不要加禁令。** 方案一律走 pi 的**纯陈述**路线 —— 只报事实、
> 不写「不要 X」。由此还需**删掉本仓现有的禁令句**（`bash.rs:96-97` 的
> `do not prefix every command with cd` 及其逃生口 `or chain cd dir && cmd`），
> 只保留其中的**事实**（「每次新 shell、`cd` 不跨调用」）；
> `bash.rs:260` 的 `spec_description_states_the_cwd_contract` 需同步改写。
>
> ⚠️ **风险要记**：本机没有一个「`d553740` 之前」的 MiniMax-M3 样本 ⇒ **无法把
> MiniMax 的 0/44 归因于那句禁令**。删禁令后若它也退化成 cd，P7 的事实回显是兜底。

| 做什么 | 落在哪 | pi 依据 |
|---|---|---|
| **P1 · 事实放进 bash 描述的第一句**：跑在哪个 shell（`sh -c`）、从哪个目录起 | bash 工具描述 | **C** + `bash.ts:235` 的位置 |
| **P1′ · 删掉现有的禁令与逃生口**，只留事实 | 同上 | pi 无禁令 |
| **P2 · cwd 那行补一句纯事实**：「相对路径按它解析」（**不是**「不要 cd」） | 系统提示尾部 | pi 的 cwd 行只有事实；我们把事实说完整 |
| **P3 · cwd 改成系统提示的最后一行**（skills 段之后） | `prompt.rs` 尾部顺序 | **F** —— cwd 紧邻第一条用户消息（**代价见下方不变量说明**） |
| **P4 · workspace 不存在 ⇒ 明确报错**（照 pi 的 `Working directory does not exist: …`）。现在是含糊的 `Failed to spawn command` | `bash.rs` 调用前 | `bash.ts:89-91` —— **事实不成立就硬报错**，正是「无需相信」的那一半 |
| **P5 · 每次调用可带 `cwd`**（pi 的 `ctx?.cwd \|\| cwd`）—— 「我想在别处跑」有正当表达，**从根上不需要 `cd`** | bash 参数面（新增可选字段） | `bash.ts:250` |
| **P6 · 角色段加厚**（点名 harness + 枚举能力） | 系统提示首段 | pi 首段 |
| **P7 · 冗余 `cd` 时的**事实**回显**（「这条 `cd` 到的就是工作目录」—— 报事实，不训话） | bash 结果 | 见「如何让模型相信」§候选 ③′ |
| **不做**：任何禁令 / 训话式措辞 | — | 用户决定 + pi 无 |
| **不做**：`<environment>` 平台 / 日期 / git 块 | — | **G** —— pi 一个都没有 |
| **不做**：截断「全文落盘」的承诺 | — | **D** —— 我们没有这个能力 |
| **不做**：E7 布局快照、E8 工作语言、静默剥离前缀 | — | **G** / 既定（剥离=静默改写模型输入） |
| **后续可选**：工具自带 snippet / guidelines（**A**） | 架构改动 | 与本轮正交 |

> **③ 会撞上一条既有不变量**：`prompt.rs:16-20` 与 `docs/arch/subagent/design.md:308` 写明
> 「skill 目录段在**最末尾**，故空 catalogue 的输出是任何非空 catalogue 输出的**逐字节前缀**」。
> 把 cwd 移到 skills 之后 ⇒ **该前缀性质失效**（空 = `…project_context + cwd`；
> 非空 = `…project_context + skills + cwd`）。代价：**skills 目录一变，缓存失效点从尾部提前到
> skills 段开头**。取 pi 的「cwd 最后」= 放弃这条前缀不变量 —— 这是本轮唯一的真取舍。

### 建议形状（pi 对齐草案 · 无禁令版）

**系统提示尾部**（P2 + P3：skill 段之后，与 pi 一致）：

```text
Current working directory: /Users/zs/Develop/YuShan
Relative paths in every tool resolve against it. Each `bash` call starts there in a fresh
shell, so `cd` does not carry over from one call to the next.
```

（两句都是**事实**：解析根 + 新 shell。**不含**「不要 cd」「不要写绝对路径」。）

**系统提示首段**（P6：照 pi 点名 harness + 枚举能力）：

```text
You are a coding agent running inside YuShan — a light, component-based agent runtime.
You help users by reading files, executing commands, editing code, and writing new files.
```

**bash 工具描述**（P1 + P1′：pi 把 shell 名与 cwd 放在这里，且**只有事实**）：

```text
Execute a shell command via `sh -c` (POSIX sh, not your user's interactive shell), starting
in the working directory shown at the end of the system prompt. Returns stdout and stderr.
Output is truncated from the front (the last lines are kept); continue with a narrower
command or `read offset=`.
```

（**已删除**现行文本里的三处「命令式」内容：`do not prefix every command with cd`、
`use absolute paths`、`or chain cd dir && cmd …` —— 前一句是禁令，后两句既是指令、
又是那个被模型当作逃生口的许可。留下的全是可核对的事实。
每条事实的对应关系：shell 名 = pi 的 **C** 通道；截断契约 = pi 的 **D**（但我们**不承诺**
「全文已存临时文件」，因为没有；）`sh -c` 一句比 pi 的 `bash` 更该说 ——
pi 真的是 bash，我们硬编码 `sh`。）

### 成本（对齐 `docs/arch/prompt-delivery/notes.md` §2）

- pi 对齐版新增的常驻文字 ≈ **120–180 est token**（尾部 cwd 契约 ≈ 40 + 首段角色 ≈ 40 +
  bash 描述增量 ≈ 60），**会话内是常量** ⇒ 落稳定前缀，缓存命中后每轮当量 ≈ **12–18 token**。
  对照：`1791906130` 一个会话里纯 `cd` 前缀即 **263** token（还没算绝对路径那笔），
  而**多一次探索轮 = 一次完整模型调用** ⇒ 这笔投入划算。
- 严格前缀不变量不变：`<available_skills>` 仍是最后一段。
- E3 日期 / E4 分支**每会话变** ⇒ 新会话首调本就未命中缓存，无额外损失。

### 反例与风险（必须先想清楚）

1. **只加位置、不换措辞 = 没用。** 现行 cwd 行是「陈述事实」，而 deepseek-flash 在描述已明说
   「`cd` 不跨调用」的前提下仍 35/38 次 cd。⇒ 头部那行必须是**契约 + 禁令**
   （「不要 cd、不要绝对路径」），不是「当前目录是 X」。同时 `bash` 描述里
   「the system prompt's `Current working directory`」这处**交叉引用**要改指 `<environment>`
   （否则改名后它指向一个不存在的段）。
2. **E8 是产品级强加**，与「不是自带全部功能的完整 Agent 产品」的定位有张力
   （同 `prompt-delivery/notes.md` §5 的 a/b 未决项）。倾向不做，或只做到「与用户语言一致」。
3. **E7 会过期**（agent 自己会建/删文件）⇒ 必须自带「启动快照、可能过期」声明 + 硬上限。
4. **E3 / E4 是「会话启动时的真值」**，长会话跨天、换分支后会失真 ⇒ 文案须写明时效。
5. **自定义 `SYSTEM.md` 短路**（`prompt.rs:29-32`）：整块**一起消失**（同 Q7）。
6. **绝不把它做成每轮注入**：那样缓存全废（10× 成本）。整块必须进**稳定前缀**。

## 本轮决策（2026-10-13，用户确认）

**主改动（用户 2026-10-13 直接指示）：「应该是开始的时候告诉他当前目录在哪里」。**
⇒ 把 cwd 从提示**中后段**（`prompt.rs:69`，在 `<project_context>` 之后）**前置到系统提示头部**，
紧接 `You are a coding agent. …` 的角色段，位于 `Available tools:` **之前**。

| 问题 | 决定 |
|---|---|
| **Q0 · 落点（主改动）** | **cwd 前置到提示头部**。现状顺序（`docs/arch/subagent/design.md:310` 记为 as-built）：`角色段 → Available tools → Guidelines → <project_context> → Current working directory → <available_skills>`。 |
| Q1 · 范围 | 取**候选 0（前置 cwd）+ 1（加固描述措辞）+ 2（guideline 同义）+ 3（结果回显 cwd）**；**不做**候选 4（机械剥离）、候选 5（`cwd` 参数）。 |
| Q2 · 机械手段 | 接受**回显**（补事实，不改模型输入）；**不接受**静默剥离。 |
| Q4 · 验收 | **同一模型**修复前后对照：先采 deepseek-flash 基线，再改，再复跑同一组 prompt。跨模型对照（MiniMax-M3 0/44 vs deepseek-flash 35/38）只作定性依据，**不作**效果证据。 |
| Q5 · 上轮遗留 R2 | 本轮不做，留下轮。 |
| Q3 · 措辞归属 | **仍未决** —— 见下。 |

### 前置改动会动到的既有断言与文档（改之前必须一起改）

- `prompt.rs:861`（测试）断言空 catalogue 下提示**以** `Current working directory: {dir}` **结尾**
  ⇒ 前置后必须改为断言它出现在**头部**。这条断言正是本次改动的守卫兼变异点。
- `prompt.rs:16-20`（函数文档）写着「段落在整条提示的**最末尾**（`Current working directory: …` 之后）」
  ⇒ 注释须改为「skill 目录段仍是最后一段；cwd 行已前置到头部」。
- `docs/arch/subagent/design.md:308-310` 逐字引用了同一段注释并列出 as-built 顺序 ⇒ 需同步。
- `docs/arch/coding-agent/design.md:1134,1155` 的模板字面量、`docs/arch/worktree/context.md:56`
  按 `:69` 行号引用该行 ⇒ 行号与模板均会漂。
- **严格前缀不变量不受影响**：`<available_skills>` 仍是最后一段，空 catalogue 的输出仍是
  非空 catalogue 输出的逐字节前缀。cwd 行同为**常量**、仍在稳定前缀内 ⇒ `fixed_prompt_tokens`
  与缓存行为不变（`toolset.rs:633` 的 `fp_prompt` 不入锚，逐次运行本就不同）。
- `assembly.rs:171-176` 的「cwd 跟着工作区变」断言不依赖该行位置，**不需**改。

## 实验：系统提示布局 A/B（2026-10-14）

> 用户决定：「cwd 不一定要放在最末尾，你可以按照实际效果进行提示词的调整。比如多搞几个
> 系统提示词，跑一跑任务看看哪个效果好。」⇒ **P3（cwd 放哪）改为实测决定，不再靠论证。**

**单变量设计**：四个变体的**工具表、guidelines、project_context、skills 段全部逐字相同**，
只有 cwd 那一段的**位置**与**措辞**不同。实测长度证实了这一点：`a`/`b`/`c` 都是 **1456 字节**
（内容全同、只换顺序），`d` 是 1664（多了那一句事实 + 角色段加厚）。

| 变体 | cwd 位置（实测字节偏移） | cwd 措辞 | 角色段 | 破严格前缀不变量？ |
|---|---|---|---|---|
| **a**（现状，默认） | 795 / 1456（project_context 之后、skills 之前） | 一句事实 | 现状 | 否 |
| **b**（照 pi） | **1354 / 1456（最末，skills 之后）** | 一句事实 | 现状 | **是** |
| **c** | **0（最前）** | 一句事实 | 现状 | **否** |
| **d** | **0（最前）** | **事实说完整**（解析根 + 每次新 shell） | **加厚** | 否 |

> **一条提前得到的结论**：`b` 破严格前缀不变量，而 **`c` 不破** —— 把 cwd 放最前时，
> skills 段仍在最后，故「空 catalogue 是任何非空 catalogue 输出的字节前缀」照样成立。
> 所以「cwd 紧邻任务」与「保住缓存不变量」**并非二选一**：选 `c` 可以两者兼得。

**接缝（临时，实验后删）**：`prompt.rs::variant()` 读 `YUSHAN_PROMPT_VARIANT`（`a`/`b`/`c`/`d`，
缺省 = `a`）。刻意**不做成正式配置项** —— 项目对新增配置的口径是「第二个消费者是谁」
（ADR-0014），一次实验不算消费者。干跑工具：
`cargo test -p ys-coding-agent --bin ys-coding-agent dump_prompt_variants -- --ignored --nocapture`
（实测抓到了装配 bug：初版 `b` 分支漏推 cwd 段，长度少 104 字节 —— 干跑工具的价值当场兑现）。

**任务**：`T1` = 「统计 crates/ 下每个 crate 的 src 目录里的 .rs 文件数和总行数，输出一张表。」
（多目录型；简单任务实测 0 次 cd，量不出差异。）

**指标**（脚本 `tmp/exp/measure.py`）：两类 cd **分开数** ——
「`cd <工作目录根> &&`」= 冗余噪音（本轮目标）、「`cd <子目录> &&`」= **正当用法**（不得误伤）；
外加一条**新指标**：结构化工具（`read`/`grep`/`ls`…）的 `path` 若为工作目录内的**绝对路径**，
属**同一种「不信任 cwd」**（实测在真实会话 `#2` 就出现过：`ls {path:"/Users/zs/Develop/YuShan/docs"}`）。

> **校准教训（已付出一次运行）**：首个任务 `T1`（「统计 crates/ 下每个 crate 的 src…」）
> **测错了东西** —— 它只产生 1 次 bash，且是 `cd crates && for d in */`，
> 那是**正当**用法（要进子目录迭代），不是冗余噪音。任务必须**跨顶级目录**
> （cd 进任何一个都够不着其余路径），才会引出「cd 到工作目录根」这种防御动作。

**任务**
- `T2`（受控、短）：跨三个顶级目录 —— 「给出仓库 workspace 里有几个 crate；列出
  `docs/architecture.md` 的一级标题；统计 `crates/ys-core/src` 下所有 `.rs` 的总行数。」
- `T4`（贴近真实会话的开放式大任务）：见下。

**结果 · T2（每变体 1 次）**：

| 变体 | 工具分布 | bash | cd→根 | cd→子目录 | cd 字符 | 结构化工具用绝对 path |
|---|---|---:|---:|---:|---:|---:|
| **a**（现状） | read 1 / grep 1 / bash 3 | 3 | **1（33%）** | 0 | 31 | **2 / 2** |
| b | read 1 / grep 2 / bash 1 | 1 | 0 | 0 | 0 | 0 / 3 |
| c | grep 1 / bash 3 | 3 | 0 | 0 | 0 | 0 / 1 |
| d | grep 1 / bash 4 | 4 | 0 | 0 | 0 | 0 / 1 |

**读法**：a 同时出现两种「钉位置」形态（bash 的 `cd <根> &&`、结构化工具的绝对 `path`），
b/c/d 一个都没有。**但样本过小（bash 1–4 次/变体）⇒ 只能当方向性证据，不能定论。**

**结果 · T4（开放式大任务，与真实会话同型，样本大得多）**：

| 变体 | 位置 | 措辞 | 角色段 | 工具分布 | bash | cd→根 | 占比 | 绝对 path |
|---|---|---|---|---:|---:|---:|---:|
| **a** | Mid | 一句事实 | 现状 | ls 11 / read 10 / bash 64 / grep 1 | 64 | **64** | **100%** | 22/22 |
| **b** | Last | 一句事实 | 现状 | ls 7 / bash 3 / read 1 / subagent 2 | 3 | 3 | **100%** | 0/10 |
| **c** | First | 一句事实 | 现状 | bash 34 / ls 9 / read 8 | 34 | **34** | **100%** | 17/17 |
| **d** | First | **说完整** | **加厚** | bash 88 / grep 2 / read 1 | 88 | **3** | **3%** | 1/3 |

**结论一（已成立）：位置不是原因。** `b`（挪最末，照 pi）与 `c`（挪最前）**都仍然是 100%** ——
把 cwd 从第 795 字节搬到第 0 或第 1354 字节，行为一点没变。
⇒ **「放头部还是放尾部」是伪问题**；此前围绕它的取舍（包括那条严格前缀不变量）**根本不必动**。

**结论二（待归因）：是措辞把它打下去的。** `d` 从 100% 掉到 3%，但 `d` 同时改了
**措辞**（补两条事实）与**角色段**（加厚）⇒ 尚不能归因。为此加跑两个变体（见下）。

> `b` 另有一处形态差异：它用了 **2 次 `subagent`**，只做 3 次 bash —— 模型改走「派子 agent
> 去探索」的路线。⇒ `b` 的低 bash 数（3）不构成「b 减少了 cd」的证据，别误读。
> `c` 的形态（34 次 bash / 1054 字符）与真实会话 `1791906130`（同样 34 / 1054）**完全同形** ——
> 同一任务型下可复现。

**归因实验（变体 `e`/`f`，与上面同任务 T4）**：

| 变体 | 位置 | 措辞 | 角色段 | bash | cd→根 | 占比 | 绝对 path |
|---|---|---|---:|---:|---:|---:|---:|
| **e** | **First** | 说完整 | **现状（不加厚）** | 56 | **0** | **0%** | 0/3 |
| **f** | **Mid（现状位置）** | 说完整 | 现状 | 44 | 17 | **39%** | **6/6** |

**结论二：角色段加厚是多余的。** `e`（不加厚）比 `d`（加厚）**更干净**（0% vs 3%）
⇒ **删掉角色段加厚**，diff 更小。pi 首段那种写法**不是**必要条件。

**结论三（修正结论一）：位置与措辞有交互 —— 位置只在措辞说完整之后才起作用。**
- 措辞弱（只有一句事实）时：Mid / Last / First **全是 100%**（`a`/`b`/`c`）⇒ 位置**无效**。
- 措辞说完整后：**First 0–3%**（`d`/`e`）vs **Mid 39%**（`f`）⇒ 位置**开始起作用**。

合理解释是**注意力**：Mid 位置紧跟在 `<project_context>` 之后，而本仓的
`<project_context>` 是一整份 `AGENTS.md`（数 KB）⇒ 那两条事实被一大段文本压住了。
（**不是**「开头 vs 末尾」之争 —— `b` 把它放到**最末**同样是 100%。）

> ⚠️ **每格只有 1 次运行**（复跑被额度打断，见下）。`a` 的两份独立样本
> （本实验 64/64 与真实会话 `1791906130` 34/34）都稳定在 100%，故 `a` 的基线是稳的；
> `e`/`f` 各只有 1 次，**结论强度有限**。

### ⚠️ 复跑被额度打断（2026-10-14）

`e`/`f` 各复跑 2 次，四次**全部在第一次调用就被拒**：

```text
API error 429 Too Many Requests: {"type":"GoUsageLimitError",
  "message":"Go usage limit exceeded", "limitName":"monthly"}
```

opencode-go 的**月度额度已耗尽**（本实验总共跑了 10 次真实会话，其中 5 次是开放式大任务）。
⇒ **`d`/`e`/`f` 每格 n=1，未复现**。本机另配了 `minimax`（MiniMax-M3）可用，但换模型即换实验对象
（它原本就是 0/44 的那个模型），只能作**跨模型验证**，不能用来复现 deepseek 的数字。

### 落地（2026-10-14，用户「做 1」）

**只落地赢家 `e`**，接缝（`YUSHAN_PROMPT_VARIANT` / `Pos` / `Variant` / `variant()` / 干跑测试）
**已删**：

| 改动 | 内容 |
|---|---|
| cwd 段 | 由一句事实改成**两句**：加上「相对路径按它解析」与「每次调用是新 shell」 |
| cwd 段位置 | 由 `project_context` 之后挪到**整条提示最前** |
| 禁令 | **不加**（用户 2026-10-14 定） |
| 角色段 | **不加厚**（实测多余 —— `e` 不加厚比 `d` 加厚更干净） |
| `prompt.rs:16-23` 函数文档 | 段序改为「cwd 段（最前）→ … → skill 段（最末）」 |
| 断言 | `skills_section_is_gated_on_read_tool_and_non_empty_lines` 的 `ends_with(cwd)` 改为 `starts_with(cwd)`（位置变了，但它守的语义不变） |
| **严格前缀不变量** | **保住**（skill 段仍在最末）⇒ 原先要在「pi 的 cwd 最后」与「缓存不变量」之间做的取舍**不用做了** |

**未做（且**零数据**）**：

- **P1′（删 `bash` 描述里的禁令与逃生口）**：六个变体的**工具表恒为原样**（实验只变系统提示），
  所以「删禁令是否安全」**没有测过**。故本轮**不动** `bash.rs` 的描述 ——
  现行描述里的 `do not prefix every command with cd` / `use absolute paths` /
  `or chain cd dir && cmd …` 三处命令式内容**保留**。若要删，需另做一轮实验。
- P2 的「cwd 那行补事实」已被 `cwd_section` 的吸收（同一条改动）。
- P4（workspace 不存在 ⇒ 明确报错）、P5（bash 增加 `cwd` 参数）、P7（冗余 cd 的**事实**回显）：
  都**未做**。P4/P5 仍是「不依赖模型配合」的那一半，价值最高，建议下一轮先做。
- 旧设计文档（`docs/arch/subagent/design.md:308-310` 逐字引用了被改的那段注释）**按规矩不改** ——
  旧设计文档只记 **why**，不承载 **now**（`CLAUDE.md` 的指针纪律）。

## 未澄清问题

- [ ] **Q3 · 措辞归属**：新纪律（描述措辞 + 头部 cwd 文案）是**产品内置**（对所有项目生效），
      还是项目可配？（`prompt-delivery/notes.md` §5 的 a/b 未决项在此复现。） 待用户确认
- [ ] **Q6 · 头部块的范围**：只前置 cwd 一行，还是顺手做成一个小的 `Environment:` 块
      （cwd + 平台 + 日期）？**日期每轮都变 ⇒ 会破坏前缀缓存**，若加只能取「会话启动时」或干脆不加；
      平台目前提示里**完全没有**（`prompt.rs` 无任何 `platform` / `os` / `uname` 字样）。 待用户确认
- [ ] **Q7 · 自定义 SYSTEM.md 短路**：命中 `<cwd>/.yushan/SYSTEM.md` / `~/.yushan/SYSTEM.md` 时提示
      **整体短路**（`prompt.rs:29-32`），头部 cwd 行**同样消失**，且 `bash` 描述里
      「the system prompt's `Current working directory`」的交叉引用随之落空。短路路径是否也补 cwd？
      **会改动「用户全权接管提示词」的既有契约**，需明确点头。 待用户确认
- [ ] **Q8 · 头部基础信息块收哪几项**（`E1`–`E8`，见上节）？其中 **E2/E3/E4 需要新增启动期探测**
      （平台 / shell / 日期 / git），涉及与 `config.rs` 的接缝；**E7 布局快照**要定预算上限与
      过期声明形态；**E8 工作语言**是否做（产品级强加）。 待用户确认

## 后续建议（已按决策收敛为执行顺序）

1. **采基线**（改之前，同一份未改代码）：deepseek-flash + 固定 prompt 组 → 记录 bash 调用数、
   `cd ` 开头占比、噪音字符数。凭证：`~/.yushan/auth.json` 已有登录态
   （`state.json`: provider `opencode-go` / model `deepseek-flash`）；`YUSHAN_API_BASE` /
   `YUSHAN_API_KEY` **未**在环境里设置，故走 CLI 自身的登录态而非 e2e 测试。
2. **改**：① cwd 前置到提示头部；② 描述措辞（删掉削弱式禁令与 `cd dir && cmd` 许可、
   正面陈述「相对路径直接可用」）；③ `tool_guidelines` 补同义一条；④ bash 结果首行回显 cwd。
   每一处都带**变异测试**（删掉回显 / 还原旧措辞 ⇒ 用例必须变红）。
3. **验证**：同一组 prompt 复跑对照，与基线比 cd 占比 / 噪音字符。
4. **不做**：候选 4（静默剥离）、候选 5（`cwd` 参数）、R2 多目标参数面（留下轮）。
