# 执行计划：装配缝（P1）→ todo 清单 → 证据验收

> 对应设计：`docs/arch/hooks-and-plugins/design.md` §6（P1）、
> `docs/arch/long-horizon-todo/design.md` §7（清单层）、
> `docs/arch/cross-turn-advance/design.md` §5（验收层）
> 对应决策：ADR-0021 决策 3、ADR-0024、ADR-0072
> 日期：2026-11-06
> 纪律：**每个 `impl` 与它的 `test` 成对**；同波文件集互不相交；单单元 ≤5 文件 / ≤10 分钟。
> 构建与测试入口：`./scripts/cargo-build.sh` / `./scripts/par-test.sh`（**不用**裸 `cargo`）。

## 0. 先修正前提：阶段编号已过时

三份文档的阶段编号**互相冲突**，且与代码实际状态不符。执行前必须以代码为准重排：

| 文档 | 编号 | 该文档说的内容 | 代码实际 |
|---|---|---|---|
| `hooks-and-plugins` §6 | P0 | prefix 指纹测试 | ✅ **已实现**（`toolset.rs:635` `prefix_fingerprint_is_anchored_and_stable`） |
| | P1 | `ToolPlugin` 装配缝 | ❌ **未实现**（`grep ToolPlugin` = 0） |
| | P2 | todo 作为第一个消费者 | ❌ 未实现 |
| `long-horizon-todo` §8 | P2 | `subagent`（第 10 个工具） | ✅ **已实现**（`assembly.rs:218-219`，父表 14 元） |
| | P3 | todo + store + 注入 + follow-up | ❌ 未实现 |

⇒ **两份文档的 P2 指的是完全不同的东西**。本计划统一重编为 W 波次，不复用 P 编号。

### 0.1 P0 已完成，但**比设计更细**（不能照设计文档的字面执行）

`toolset.rs:635` 的实测形态与 `hooks-and-plugins` §6 P0 的伪代码**有四处不同**，计划按代码走：

| 设计文档写 | 代码实际 | 影响 |
|---|---|---|
| `sha256(&json)` | `fnv1a64(json.as_bytes())`（`toolset.rs:646`） | **不引 `sha2`**（省 9 个依赖包），与 ADR-0072 D10 同一取舍 |
| 一个 `PREFIX_TOOLS_SHA256` | **两个锚点**：`fp_tools`（子表 11 元）+ `fp_parent`（父表 14 元） | 追加段工具的 schema 漂移在子表锚点里是静默的，父锚补上 |
| 锚两个 system prompt（空 / 一个 skill） | 只断言 `fp_prompt` **两次构建一致**，**不入锚** | 注释理由（`toolset.rs:715-717`）：`fp_prompt` 含 `Current working directory: <临时目录>`，逐次运行都不同 |
| — | 额外断言父表 JSON 是子表 JSON 的**字节前缀**（`toolset.rs:684`） | 这是子 agent 命中父前缀缓存的机械保证 |

**结论：P0 不需要重做，直接作为 P1 的验收信号使用。**

## 1. P1 真正的形状：三个陷阱

P1 是「纯搬家，零行为变化」。设计文档已标出陷阱 1，我核实后又发现两个。

### 陷阱 1（设计已标，但**实际比设计记的更绕**）：guideline 序 ≠ 工具序

```
工具序：    read, write, edit, bash, grep, find, ls, ocr, view_image, websearch, webfetch
guideline： edit(3条), write(1条), edit|write(1条), [read组(1条), grep(1条)*嵌套*, read组(2条)], read|grep(1条), narrate(1条), websearch(1条), webfetch(1条), bash_bg(2条), subagent_async(9条), subagent_send(1条), 无条件3条
```

`render_guidelines`（`prompt.rs:305-599`）实际是 **13 个门控块 + 3 条无条件**，
而不是「每工具一个字符串列表」：

| # | 行号 | 门控 | 归属 |
|---|---|---|---|
| 1 | `:308` | `edit` | `edit` |
| 2 | `:319` | `write` | `write` |
| 3 | `:328` | `edit \|\| write` | `edit`（`AnyOf`） |
| 4 | `:357` | `read/grep/find/ls` 任一 | `read`（`AnyOf`） |
| 5 | `:376` | `grep`（**嵌在第 4 块内**） | `grep` |
| 6 | `:383` | 同第 4 块 | `read`（`AnyOf`） |
| 7 | `:394` | `read \|\| grep` | `read`（`AnyOf`） |
| 8 | `:426` | `narrate` 且 7 工作型工具任一 | `read`（`AnyOf` + `MainOnly`） |
| 9 | `:445` | `websearch` | `websearch` |
| 10 | `:451` | `webfetch` | `webfetch` |
| 11 | `:473` | `bash_bg` | `bash_bg` |
| 12 | `:488` | `subagent_async` | `subagent_async` |
| 13 | `:572` | `subagent_send` | `subagent_send` |
| — | `:588/598/599` | 无条件 | **不属任何插件** |

**两个致命细节**：
① `read` 的**三块不连续** —— 第 4、6、7 块被第 5 块（`grep`）**插在中间**。⇒ **不能**按插件 order
  遍历、每插件吐出自己全部 guideline。必须每块带**全局 `slot`**。
② 第 8 块（narrate）夹在 read 组与 `websearch` 之间 ⇒ 必须归到 `read` 插件。

**若按工具 order 展开 ⇒ 字节全变 ⇒ P0 红。**

### 陷阱 2（**新发现，设计文档未记**）：`narrate` 参数不是插件能表达的

`render_guidelines(tools, narrate)` 有**两个调用点**，`narrate` 不同：

| 调用点 | narrate | 用途 |
|---|---|---|
| `prompt.rs:298`（`tool_guidelines`，`build_system_prompt` 调用） | `true` | 主 agent |
| `prompt.rs:154`（`build_explore_prompt`） | `false` | 探索子 agent |

`narrate=false` 只删掉**一条** guideline —— `prompt.rs:426-444` 那条「Before the first tool call … narrate only at milestones」。
注意它的门控是 **`narrate` 且 7 个工作型工具任一在场**（`read/grep/find/ls/bash/edit/write`），
**不是**单工具门控 —— 这也是它必须用 `Gate::AnyOf` + `Scope::MainOnly` 两个维度才能表达的原因。

⇒ **`ToolPrompt` 若只是 `{order, guidelines: Vec<String>}`，搬家后 `narrate=false` 会失效**：
那条 guideline 会被无条件渲染进探索子 agent 的提示词，而 `prompt.rs:1906` 有反向断言守着这件事
（「`narrate = false` 的实现若仍渲染过程叙述 ⇒ 变红」）。

**两种解法**：

| 解法 | 形状 | 代价 |
|---|---|---|
| **甲**（**已采纳，写进设计 §5.2**） | 每块 `GuidelineBlock { slot, gate, scope, lines }` | 多两个 enum，但**结构上正确**：`scope` 管 narrate，`gate` 管跨工具门控，`slot` 管全局块序 |
| 乙 | 那条 guideline 留在 `prompt.rs`，不进插件 | 少一个概念，但**「插件自带 snippet/guidelines」的承诺破了一个洞**（ADR-0021 决策 3 的原话），且下一个插件照抄这个例外 |

推荐**甲**，**已写入设计 §5.2**：narrate 是**既成事实的语义**（主 agent 与探索 agent 的提示词本就不同），不是为未来预留。

### 陷阱 3（**新发现**）：`EXPECTED_ORDER` 的交叉校验有**五处**消费者

设计文档 §6 P1 第 4 条说「测试侧保留一份独立硬编码的名字序列」，并提醒 `prompt.rs` 的测试也引用它。
实测引用点有**五处**：

| 位置 | 用途 |
|---|---|
| `assembly.rs:326` | 父表期望序 |
| `prompt.rs:687`（`use`）+ `:822-823` | 派生性测试（正序 + 逆序两份） |
| `prompt.rs:869` | 「应取到装配端的真实 10 元工具表」（**注释已过时：实际 11 元**） |
| `prompt.rs:2117` | 构造 specs |

⇒ 降为派生量时，这五处都要改成**测试侧独立字面量**，否则交叉校验名存实亡。
**顺带修** `prompt.rs:869` 那句「10 元」的过时注释。

## 2. 波次总览

波次按**依赖深度**编号；同一波内文件集**互不相交**。
「串行对」= 一个 `impl` 与其 `test` 改同一批文件，不能并行，但必须**分两个任务**分别 review
（`test` 任务要检查断言是否真会变红）。

| 波次 | 串行对 | 文件集 | 验收 |
|---|---|---|---|
| **W0** | [U0] | `docs/arch/hooks-and-plugins/design.md` §6（P 编号 → 代码状态） | 文档与代码对账，无代码改动 |
| **W1** | [U1 → T1] | `crates/ys-tool/src/plugin.rs`（新）、`crates/ys-tool/src/lib.rs` | 单元测试：排序、重名 fail-fast |
| **W2** | [U2 → T2] | `apps/coding-agent/src/toolset.rs` | **P0 指纹保持绿**（纯搬家证明） |
| **W3** | [U3 → T3] | `apps/coding-agent/src/prompt.rs`、`assembly.rs` | **P0 指纹保持绿** |
| **W4** | [U4 → T4] | `adapters/tools-todo/`（新 crate）、`Cargo.toml` | 清单层单元测试 |
| **W5** | [U5 → T5] | `apps/coding-agent/src/toolset.rs`、`prompt.rs` | 工具表 11 → 12 元，**指纹显式平移** |
| **W6** | [U6 → T6] | `apps/coding-agent/src/todo_inject.rs`（新）、`app_loop.rs` | 注入判据 + 自愈 |
| **W7** | [U7 → T7] | `apps/coding-agent/src/goal.rs`、`goal_judge.rs`（新）、`ys-protocol/src/goal.rs`（新） | 判官 + 引用校验 |
| **W8** | [U8 → T8] | `apps/coding-agent/src/app_loop.rs`、`main.rs` | 两条入口一致 |
| **W9** | [D1] | `docs/` 若干 | 归档 |

**W2 与 W3 必须串行**：两者都以「P0 指纹保持绿」为验收，同时改会让失败无法归因。

## 3. 各波次细节

### W0：先修文档的编号（无代码改动）

`docs/arch/hooks-and-plugins/design.md` §6 的阶段表加一列「代码状态」，标明 P0 已实现、P2 的 subagent 已在别处实现。
理由：不修的话，下一个读这份文档的人会重做 P0。

**同时修** `prompt.rs:869` 的「10 元」→「11 元」（注释过时，不影响行为）。

### W1：`ys-tool::plugin.rs`

```rust
// crates/ys-tool/src/plugin.rs
pub struct ToolPlugin {
    pub order: u16,                  // 工具装配位次；编译期常量
    pub tools: Vec<Box<dyn Tool>>,
    pub prompts: Vec<ToolPrompt>,
}

pub struct ToolPrompt {
    pub tool: &'static str,          // 归属工具名（用于 snippet 查表）
    pub snippet: &'static str,
    pub guidelines: Vec<GuidelineBlock>,
}

/// 一块 guideline。**块序是全局量** —— `read` 的三块被 `grep` 的块插在中间
/// （`prompt.rs:376` 嵌在 `:357` 的 `if` 体内）⇒ 不能用「每插件内序」。
pub struct GuidelineBlock {
    pub slot: u16,                   // **全局**渲染位次（陷阱 1）
    pub gate: Gate,                  // 门控（陷阱 1）
    pub scope: Scope,                // narrate 维度（陷阱 2）
    pub lines: Vec<&'static str>,
}

pub enum Gate {
    ThisTool,
    AnyOf(&'static [&'static str]),  // 例：edit||write、read 组 4 工具、narrate 块 7 工具
}

pub enum Scope { Always, MainOnly }
```

**硬约束**：

- `order` / `slot` 必须是**编译期常量**，不用注册顺序 / 名字序（ADR-0021 决策 3）。
- 拒绝 `inventory` / `linkme`（顺序由链接器决定，撞缓存前缀稳定）。
- 跨插件重名工具 **fail-fast**，错误信息**点名来源**（形如 `tool "todo" declared by plugin #3 and plugin #7`）——
  否则第三方拿到 `duplicate tool name: todo`（`registry.rs:30`）不知道是谁撞了谁。
- **`slot` 全局唯一且不重复**（否则排序不稳定）；启动期断言。

**测试（T1）**：

- 打乱传入顺序 ⇒ 输出序不变（变异：不排序 ⇒ 红）。
- 跨插件重名 ⇒ 错误信息含两个 plugin 序号。
- `Scope::MainOnly` 在 `narrate=false` 时被过滤（变异：忽略 scope ⇒ 红）。
- `Gate::AnyOf` 只命中一个成员时也渲染（变异：改成 `all` ⇒ 红）。
- `slot` 重复 ⇒ 启动期报错。

### W2：`toolset.rs` 改为 `build_toolset`

1. 内置 11 元 `vec!` 字面量**一字不动**，改为 `builtin_plugins()` 的 11 条声明。
2. `EXPECTED_ORDER` 降为派生量；**测试侧保留独立硬编码**（陷阱 3 的**五处**引用全改）。
3. **五处** `assert_eq!(len, 11)`（`toolset.rs:238/:328/:505/:641`、`assembly.rs:388`）
   保持 11，只是来源变了。另 `assembly.rs:387` 的 `parent_names.len() == 14` 不动。

**验收**：`fp_tools`（`toolset.rs:726`）与 `fp_parent`（`toolset.rs:736`）**逐字节不变**。

> ⚠️ **这是 P1 的唯一机械证明。** 若指纹变了，先回滚再查 —— 不要「跑红 → 抄新值」，
> 那正是真实前缀漂移被洗白的唯一窗口（设计 §6 P0 原话）。

### W3：`prompt.rs` 删除两张手抄表

1. `tool_snippet` → 查 `ToolPlugin.prompts[].snippet`。
2. `tool_guidelines` / `render_guidelines` → 收集全部 `GuidelineBlock`，
   **按 `slot` 全局升序**展开（**不是**按工具 order、也不是按插件 order —— 陷阱 1）。
3. `narrate=false` ⇒ 过滤 `Scope::MainOnly`（陷阱 2）。
4. **全局尾部三条**（`"Do exactly what the user asked…"` `prompt.rs:588`、
   `"Be concise…"` / `"Show file paths clearly…"` `prompt.rs:598-599`）
   **留在 `build_system_prompt`**，逐字追加在所有插件 guideline 之后。

**验收**：同 W2，两个指纹不变。

### W4：`adapters/tools-todo/` 新 crate

照 `long-horizon-todo/design.md` §7.2 的接口，**一字不改**：

- `TodoItem` / `TodoStatus`（含 `Cancelled` 终止态）
- `TodoStore`：`open` / `snapshot` / `revision` / `has_open_items` / `apply` / `retarget`
- `render_block`（字节稳定，**不含时间戳 / turn 号 / session id**）
- `todo_tool(store) -> Box<dyn Tool>`

**两条硬规则在 `apply` 里**（不靠模型自觉）：

| 规则 | 防什么 |
|---|---|
| 拒绝丢弃既有非终态项 | 模型发一个 `[]` 就关掉整个闭环 |
| 无 open 项 ⇒ 清空 + 删文件 | 判据写「全部 Completed」时，被放弃的项永久卡住 |

**落盘**：`<sessions_dir>/<stem>.todo.json`；`-p`/`--json` ⇒ 纯内存（`open(None)`）。

**测试（T4）**：

- `apply([])` 丢弃非终态项 ⇒ 拒绝并返回错误。
- 全部终态 ⇒ 落盘空表 + 删文件。
- 文件损坏 ⇒ 空列表 + 降级提示，**绝不 panic**。
- `render_block` 字节稳定：同内容两次渲染逐字节相同（变异：加时间戳 ⇒ 红）。
- `/new` retarget ⇒ 新文件且计划为空；同一 `Arc` 身份不变。

### W5：把 todo 接进工具表

- `builtin_plugins()` 加一条 `todo`，`order` 取**倒数第二**（`…, webfetch, todo`）。
- `EXPECTED_ORDER`（派生量）自动变成 12 元；测试侧独立字面量同步加 `todo`。
- `toolset.rs` **四处** `assert_eq!(tools.len(), 11)` → `12`（`:238/:328/:505/:641`）；`assembly.rs:388` 子表 `11` → `12`。共**五处**。
- 子 registry **不得**含 `todo`（ADR-0024 决策 e：同时打破字节前缀与单写者不变量）。

**⚠️ 这是**预期内**的前缀漂移**：`fp_tools` 与 `fp_parent` **都会变**。
更新锚点时必须在 commit message 里写明「新增 todo 工具，前缀 +N B」，
且**从 `git stash` 的干净树取值**，不是从跑红的树里抄。

**验收**：父表 JSON 仍是子表 JSON 的字节前缀（`toolset.rs:684` 那条断言）。

### W6：注入判据 `todo_inject.rs`

```rust
pub const TODO_MARKER: &str = "<todo_list";

/// 状态变了 **或** 会话里已找不到标记 ⇒ 注入。
pub fn should_inject(store: &TodoStore, last_injected_rev: Option<u64>, session: &[Message]) -> bool;
```

**「找不到就补」是自愈**：压缩把块吃掉后自动补回 —— 不需要监听 `ContextCompacted` 事件，
因此也覆盖得了那条不发事件的手敲 `/compact` 路径。

**接入点**：`app_loop.rs` 的 `Request::Prompt` 臂，回合边界 `session.append(...)`。
⚠️ **直接 append，不发事件** ⇒ UI 不显示（对比 `Boundary::Steer` 会 emit `UserMessage`）。
⚠️ **绝不改 `system`**（ADR-0021 新增硬约束：`system` 是每轮整块重发的前缀，动它等于每轮废掉缓存）。

**测试（T6）**：

- 状态变了 ⇒ 注入一次。
- 状态没变且标记仍在 ⇒ 不注入。
- 压缩后标记丢失 ⇒ 自动补回（变异：去掉「找不到就补」⇒ 红）。
- `system_prompt_does_not_depend_on_todo_state()`（新增不变量测试）。

### W7：验收层

照 `cross-turn-advance/design.md` §5.5–5.8 + ADR-0072 D4–D10：

- `ys-protocol/src/goal.rs`：`GoalSpec` / `Verdict` / `ItemVerdict`（纯数据、**无 tokio**）。
- `goal.rs`：`GoalState` / `Evidence` / `EvidenceBuilder` / `evaluate` / 闸门 / stall 指纹。
- `goal_judge.rs`：判官提示词、请求构造、`VERDICT`/`ITEMS` 解析、**引用校验**、usage 累计。

**判官调用形态**继承 `summarize.rs:56` 的 `ModelSummarizer`：
`tools: Arc::new(vec![])` + `thinking: Some(ThinkingOverride::off())` + `max_tokens: 128`。

**`MET` 三条件缺一不可**（机制强制）：`VERDICT: MET` ∧ 每个非终态项都 `OK` 且引用逐字命中 ∧ 至少一条命中。

**stall 指纹**：`DefaultHasher(Σ(归一化内容 ‖ 状态) for 非终态项)`，
归一化链 `折叠空白 → 小写 → 排序 → 去重`，**只算非终态项**。

**测试（T7）**：

- `MET` 但引用不存在于证据 ⇒ 降级 `NOT_MET` + 计入可靠性计数。
- `MET` 但某项标 `MISSING` ⇒ 降级 `NOT_MET`。
- 模型把全部项标 `completed` 但无凭据 ⇒ **不得放行**（ADR-0024 天花板）。
- 助手自述**不在**证据窗口 ⇒ 模型自称完成无法通关。
- 判官连续两次格式错误 ⇒ 停；调用失败 ⇒ 重试 1 次后停。
- todo 集合连续 3 轮相同 **且** 模型自称收尾 ⇒ 停；集合变了 ⇒ stall 归零。

### W8：两条入口

| 入口 | 落点 |
|---|---|
| TUI | `app_loop.rs` 的 `Request::Prompt` 臂内，回合结束后求值；未满足则追加一轮 |
| `-p` / `--json` | `run_single_turn`（`main.rs:2059`）**外**的循环 |

**继续条件只有一份，两个入口共用**（不是「`-p` 被 `--goal` 单独门控」）：

```text
continue = todo.has_open_items()          // 清单层（无开关）
         ∨ (goal.is_some() ∧ ¬goal.met)   // 验收层（需显式 --goal / /goal）
         ∨ should_follow_up(head)         // 一期 HEAD 判据（仅 TUI）
```

**`-p` 默认行为不变**：不写清单、不传 `--goal` ⇒ 三项均假 ⇒ 跑完即退出。
⚠️ 但**不是「逐字节不变」** —— 加 `todo` 工具本身会改工具表（W5 的预期内漂移，全入口同步）。
退出码：`0` = 达成；`2` = 未达成。

**测试（T8）**：

- **TUI 与 `-p` 跑同一条用例，断言行为一致**（ADR-0041 的回归防线）。
- `--resume` 后目标未激活，但 todo 清单恢复。

### W9：归档

- `docs/arch/hooks-and-plugins/design.md` §6 标 P1 完成。
- `docs/arch/long-horizon-todo/design.md` §8 标 P3 完成。
- `docs/TODO.md` 删掉对应两行。
- `docs/design-final/` 新增 `todo.md`（as-built，含实现与设计的分歧）。

## 4. 变异测试

落 `scripts/goal-mutation.py`（形态对齐 `scripts/cancellation-mutation.py`），注入以下 bug ⇒ 期望变红：

| # | 注入的 bug | 抓它的用例 |
|---|---|---|
| 1 | `apply` 允许丢弃非终态项 | `apply_rejects_dropping_open_items` |
| 2 | `render_block` 加时间戳 | `render_block_is_byte_stable` |
| 3 | 去掉「标记丢失就补」 | `inject_self_heals_after_compact` |
| 4 | guideline 按工具 order 展开 | P0 指纹（W3） |
| 5 | `narrate=false` 不过滤 `MainOnly` | `explore_prompt_omits_narration_guideline` |
| 6 | `MET` 不校验引用 | `met_without_evidence_downgrades` |
| 7 | `MET` 不校验逐项 | `met_with_missing_item_downgrades` |
| 8 | stall 不计数 | `stall_stops_after_three_identical_sets` |
| 9 | 闸门恒真 | `wall_clock_gate_stops` |
| 10 | 子表含 `todo` | `child_specs_exclude_todo` |

## 5. 与设计文档的分歧（需回写）

| # | 分歧 | 处理 |
|---|---|---|
| 1 | `hooks-and-plugins` §6 用 `sha256`，代码用 `fnv1a64` | 文档改（代码的取舍更优：省 9 个依赖包） |
| 2 | `hooks-and-plugins` §6 P0 锚两个 prompt，代码只断言可复现 | 文档改（`fp_prompt` 含临时目录路径，入锚会随机器而变） |
| 3 | **`narrate` 参数不在设计里**，且 `read` 的 guideline 块**被 `grep` 插开** | 设计补 `GuidelineBlock { slot, gate, scope, lines }`（陷阱 1+2）—— ✅ **已回写** |
| 4 | 两份文档的 P2 编号冲突 | 本计划统一用 W 编号；两文档各加一行「编号见 exec-plan」 |
| 5 | `prompt.rs:869` 注释写「10 元」 | 顺手修成 11 |
| 6 | `hooks-and-plugins` §6 P2 写 `plugins/todo`，`long-horizon-todo` §7.1 写 `adapters/tools-todo` | 取 `adapters/tools-todo`（与 `tools-basic` / `tools-web` 同形） |

## 6. 风险

| 风险 | 缓解 |
|---|---|
| **W2/W3 静默改写缓存前缀** | P0 指纹是唯一护栏。搬完指纹不变 = 机械证明；变了就回滚，**不许抄新值** |
| W5 是预期内漂移 | commit message 写明字节增量；锚点从 `git stash` 的干净树取 |
| W1 的 `GuidelineBlock` 被后人「简化」成每插件一个 `Vec<String>` | `slot` / `gate` / `scope` 各配一条变异测试（T1） |
| W7 判官假阳性 | 三条件缺一不可；引用必须逐字命中；解析失败一律降级 |
| 切片 4 之前 todo 无处可存 | W4 自带落盘，不依赖 W6 |

## 7. 不做

- 运行期动态加载（`.so` / ABI / Manifest）—— `CLAUDE.md` 禁止跨动态库边界传 trait 对象；与缓存经济学冲突（52×）。
- `inventory` / `linkme` 链接期注册 —— 顺序不确定。
- `ToolResult.details` —— ADR-0021 决策 2 继续有效。
- 通用渲染注册表 —— 消费者仍为 1 个。
- Hook 系统 —— ADR-0014；消费者仍为 0。
- 子 agent 的 todo —— ADR-0024 决策 e。
- 多目标 / 目标树 / 跨会话目标恢复 —— ADR-0072 明确不做。
