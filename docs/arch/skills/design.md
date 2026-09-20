# Skills 支持 — 架构设计

> 产出：`/arch-design`（2026-09-20）。输入：[`context.md`](./context.md)（需求/约束/已定决策）。
> 候选方案由三个倾向各异的 subagent 并行产出（最小复杂度 / 可扩展优先 / token 预算优先），
> 本节记录融合结果。决策落 [`../../adr/0019-skills.md`](../../adr/0019-skills.md)。

## 概述

让 YuShan coding agent 支持 **skills**：复用 JackCC 的 56 个 `SKILL.md`（用户自行软链进 `~/.yushan/skills`）。
渐进式披露 —— `name` + `description` 常驻系统提示，正文按需进上下文。两条激活路：模型自主 / 用户 `/skill-name`。

**本设计的核心结论（推翻了 `context.md` 的 D1 工具半边）**：**不新增 `skill` 工具**。
pi（本项目的主要参照实现）已证明零工具可行，且在本仓的代码结构下，专用工具**买不到任何东西、却要付出实打实的代价**（§选择理由 A）。

---

## 设计方案

### 模块划分

全新 1 个文件，改动 6 个既有文件的接缝：

```
main.rs ──► skills::scan(roots) ──► catalogue() ──┬─► prompt::build_system_prompt(…, &catalogue) → 冻结
  │                                               └─► view::build_view(…) → CodingView.skills ──► TUI / 浮层
  │
  └─► resolve_limits(…, fixed_prompt_tokens = est(system_prompt) + est(tools))  ← 压缩耦合修正
```

| 模块 | 文件 | 职责 |
|---|---|---|
| **M1 领域逻辑**（新） | `apps/coding-agent/src/skills.rs` | 扫描（显式 `&[PathBuf]` roots、有界读）/ frontmatter 自写解析 / `catalogue()` 纯函数（去重→排序→准入→预算）/ `load_body()`（激活时即时读盘）/ `expand_invocation()` |
| **M2 提示注入** | `apps/coding-agent/src/prompt.rs` | `build_system_prompt` 末尾追加 `<available_skills>` 段；自定义 prompt 短路时**不注入但发启动提示** |
| **M3 TUI 通路** | `crates/ys-tui-coding/src/{view,commands,completion}.rs` | `CodingView.skills: Vec<SkillEntry>`；`parse` 认名 → 复用 `Request::Prompt`；补全/`/help` 追加 skill 段 |
| **M4 装配与耦合** | `apps/coding-agent/src/{main,app_loop,config}.rs` | 启动期扫描（紧跟 `load_user_config`）；`Request::Prompt` 臂里展开 `/skill-name`；计算 `fixed_prompt_tokens` |
| **M5 压缩耦合**（改） | `crates/ys-component/src/limits.rs` + `crates/ys-loop/src/basic.rs` | `RunLimits.fixed_prompt_tokens`（默认 0）+ 触发条件改为 `estimated + fixed > trigger_at(window)` |

**为什么放 app 层而不是新 crate**：skill 是**产品概念**（启动期扫描 + 提示词拼装），不是运行时能力。前身文档把 `ys-skill` crate 列为「不做」，本设计**维持该裁决** —— 不是推翻，是在其范围内实现。TUI 只拿到 `SkillEntry`（纯数据），`ys-tui-coding` 仍不认识任何 app 类型。

### 关键数据流

```
【启动期 · 一次 · 此后冻结】
  roots = [ <cwd>/.yushan/skills , ~/.yushan/skills ]        ← 显式路径；顺序即优先级
  skills::scan(roots)                                        ← 每 root 一层 readdir + 每 SKILL.md 有界读(4KB)
      → raw: Vec<SkillEntry>  (name/description/path/scope/model_visible)
  skills::catalogue(raw, budget)                             ← 纯函数，无 IO
      → Catalogue { lines, entries, omitted, bytes }
  prompt::build_system_prompt(cwd, specs, &catalogue)        ← 末尾追加 <available_skills>
  AgentBuilder::system_prompt(…)                             ← 冻结（无 setter —— 对本设计是好事）
  resolve_limits(…) → RunLimits.fixed_prompt_tokens          ← 压缩耦合修正
  build_view(…) → CodingView.skills                          ← / 浮层的唯一数据通路

【路线 1 · 模型自主】（1 次 tool round-trip）
  模型读 <available_skills> 的 name+description+location
      → 调既有 read(path = location 绝对路径)                 ← ReadTool: workspace.join(abs)==abs，workspace 外可读
      → 正文（带行号）进 ToolResult → 进上下文（transcript 可见）
  正文引用的 references/ 相对路径 → 按 skill 目录解析（提示句逐字写在目录里）

【路线 2 · 用户显式 /git-status <args>】（0 次额外 round-trip）
  events.rs::submit → commands::parse(text, &app.view.skills)
      → Action::Request(Request::Prompt("/git-status <args>"))  ← 零协议改动
  app_loop.rs  Request::Prompt 臂（唯一改动 ≈2 行）：
      skills::expand_invocation(text, &config.skills)
      → <skill name="…" location="…">\n{SKILL.md 原文}\n</skill>\n\n{args}
      → 走既有的 turns+=1 / set_context_window / run_turn（**不分叉回合启动路径**）
```

**两条路线给模型的都是 SKILL.md 原文**（路线 1 多行号与截断提示），因此 app 侧**只需要一个 `expand_invocation`**，不需要第二个「剥 frontmatter」函数。

`-p` / `--json`：`main` 里对 task 调同一个 `expand_invocation`，三模式一致。

### 接口概要

```rust
// apps/coding-agent/src/skills.rs
pub struct SkillEntry {
    pub command: String,       // **目录名** —— `/` 命令名 + 去重键（Claude Code 语义，见 §CC 兼容性）
    pub label: String,         // frontmatter `name` —— **仅显示用**（缺省 = command）
    pub description: String,   // 已单行化（折叠块压平）
    pub path: PathBuf,         // SKILL.md 绝对路径（进 <location>，也是激活时读的路径）
    pub scope: Scope,          // User | Project（仅用于去重与诊断，不参与排序）
    pub model_visible: bool,   // !disable-model-invocation
    pub user_invocable: bool,  // !(user-invocable: false)（默认 true）
}
pub struct Catalogue { pub lines: String, pub bytes: usize, pub omitted: usize, pub entries: Vec<SkillEntry> }
pub struct Budget { pub total_bytes: usize }   // 安全阀；默认 ≥ 当前 56 条语料 ⇒ 今天恒不触发

pub fn scan(roots: &[PathBuf]) -> (Vec<SkillEntry>, Vec<String>);   // 第二项 = 启动期诊断
pub fn catalogue(raw: Vec<SkillEntry>, budget: Budget) -> Catalogue; // 纯函数
pub fn load_body(path: &Path, cap_bytes: usize) -> Result<String, String>;
pub fn expand_invocation(text: &str, skills: &[SkillEntry]) -> Option<String>;
/// 只认 5 个 key；`command` 不来自 frontmatter（来自目录名），故此处返回 label。
fn parse_frontmatter(head: &str)
    -> (Option<String> /*name*/, Option<String> /*description*/, bool, bool);

// crates/ys-tui-coding/src/view.rs
pub struct SkillEntry { pub command: String, pub label: String, pub description: String }
// crates/ys-component/src/limits.rs
pub struct RunLimits { …, pub fixed_prompt_tokens: usize /* 默认 0 */ }
```

**`parse_frontmatter` 要覆盖的形态**（对 56 条真实语料实测穷举）：

| 形态 | 实测数量 | 处理 |
|---|---|---|
| 裸标量 `key: value` | 30 | 原样取值（**按第一个 `:` 切** —— 这样值里的 `: ` 天然安全） |
| 单/双引号标量 | **26** | 去外层引号；**不做完整 YAML 转义** |
| `>` 折叠块 | **2**（`caveman` / `lark-whiteboard`） | 缩进续行用**单空格** join，末尾 trim |
| `>-` 折叠块 | **1**（`fireworks-tech-graph-main`） | 同 `>`（chomping 差异对单行化无意义） |
| `|` / `|-` 字面块 | 0（**仍支持**） | 先保留换行，随后由单行化压平 |

- **统一规则**：任何 `key: value` 后紧跟的**缩进行**都算续行 → 一条规则同时覆盖折叠块与 plain-multiline，且天然终止于下一个非缩进的 key（实测 `lark-whiteboard` 的 `description: >` 之后紧接**顶格**的 `metadata:`，正好命中）。
- **只认 5 个 key**（`name`/`description`/`disable-model-invocation`/`user-invocable`/`argument-hint`），其余（`version` 27 个 / `metadata` 26 个 / `license`）整体忽略（「有缩进即跳过」自然吃掉嵌套）。
- **值的形态校验照 Claude Code**：取值若以 `[` 或 `{` 开头（YAML 流集合）⇒ **不是字符串**，按 CC 的规则「warn + 丢弃该字段」处理，而不是静默塞进提示词。
- `description` 缺失/空 ⇒ **整个 skill 不加载**（一条规则同时吃掉 E2 与 pi 的 "description is required"）。
- `name` 缺失 ⇒ `label = command`。**不做 name 字符集校验**（CC 对个人/项目 skill 也不校验；非法名只表现为显示怪，与 B6「失效自负」同一立场）。
- **块界**：`^---\s*\n … \n---\s*\n?`（容 BOM/CRLF；无闭合 = 无 frontmatter）。**不用正则**（ADR-0015 否决 `regex`）—— 逐行状态机即可。

> **不做真正的 YAML 解析是已知取舍**：CC 用真 YAML 解析器 + 一段「把含 `: ` 的裸标量加引号后重试」的兜底。我们的子集解析器覆盖**实测用到的全部形态**，且在 `[`/`{` 这类**明确非字符串**的形态上主动诊断而非静默出错。**已知不支持**：锚点/别名（`&`/`*`）、多文档、流集合 —— 遇到即产诊断。这是「守依赖基线」换来的代价，须写进文档（见 §CC 兼容性）。

### 每轮固定成本（back-of-envelope，基于实测语料）

实测：56 条 description 合计 **18,226 B**，其中 **4,886 个 CJK 字符**。用本仓估算器（`token.rs`：`(bytes − 2×cjk)/4 + cjk×1.5`）：

| 项 | 每轮 token |
|---|---|
| 现状基线（9 工具 + 骨架） | ~1,100 |
| skill 目录 · **全量 56 条** | **~9,600**（description 9,442 + 每行 `- name: ` + 前言） |
| skill 目录 · 剔除 3 条 `disable-model-invocation` | ~9,250 |
| （参考）单条截断到 280 B | ~7,400 —— **只省 ~2.1k**，见风险 R1 |

**关键**：这批 description 是 **CJK 密集**的，估算器给 CJK ≈ 1 token/字节（ASCII 是 1/4）。
⇒ **CodeWhale 的 280 字符预算在这批数据上几乎不起作用**（它的目录是英文的）。**任何按「字符数」定义的预算规则都会被 CJK 的 token 密度打穿** —— 预算必须按**字节**定义。

---

## Claude Code 格式兼容性（2026-09-20 补）

用户确认 skill 格式**与 Claude Code 一致**（参照 `~/.claude/skills/grill-me`：`name` + `description` + markdown 正文）。
下表按官方 Agent Skills spec + Claude Code 文档 + 已安装 CLI 实现核对，**记录采纳哪些、偏离哪些**。

### 采纳（格式兼容）

| 事实 | 落地 |
|---|---|
| 布局 `skill/SKILL.md` + 可选 `references/` `scripts/` `assets/` | 扫描「含 `SKILL.md` 的目录」，**不再下探**（自动剪掉 references/scripts） |
| 三级渐进披露（目录 → 正文 → 资源） | 目录常驻提示；正文 `read`；资源由正文的**相对路径**引用 |
| **命令名来自目录名**，frontmatter `name` 只是显示标签 | `command = 目录名`、`label = name`。实测唯一不一致：`fireworks-tech-graph-main/` 目录 vs `name: fireworks-tech-graph` |
| `disable-model-invocation: true` ⇒ **从模型目录整体移除**（不是调用时拦截） | `model_visible=false` ⇒ 不进 `<available_skills>`，但 `/` 仍可用 |
| `user-invocable: false` ⇒ 只给模型、不给用户敲 | `user_invocable` |
| 正文作为**一条 user 消息**进上下文 | `/skill-name` 的展开形态与 CC 完全一致 |
| `description` 缺失/空 ⇒ 不可被模型匹配 | 整个 skill 不加载 |

### 已知偏离（有意，须记档）

1. **触发机制**：CC 用 `Skill` 工具（且其 tool result 只是**桩**，正文另以 user 消息注入）；我们用 `read` 绝对路径。
   **依据**：实测 56 条正文**无一条**提及 `Skill` 工具（`grep "Skill tool"` 零命中）⇒ 机制差异不破坏任何既有 skill；而工具路线在模型自主路线上净收益为零（§选择理由 A）。**我们与 CC 共享的是「正文以 user 消息进上下文」这一半，差的只是触发方式。**
2. **正文含 frontmatter**：CC 剥离后再注入；我们 `read` 的是**含 frontmatter 的原文**（每次多 ~150 B）。**接受** —— 避免维护第二个「剥 frontmatter」函数，且 frontmatter 对模型无害。
3. **`<location>` 是承重的**：实测**多条正文按相对路径交叉引用兄弟 skill**（如 `arch-design` 引用 `../design-an-interface/SKILL.md`、lark 系列互引）。故目录里必须给**绝对路径**并逐字写明「相对路径按 skill 目录解析」（照 pi）。
4. **冲突优先级**：CC 为 `enterprise > personal > project`（**personal 胜 project**）；Agent Skills 客户端指南为 **project 胜 user**。**我们取后者（已确认 2026-09-20）** —— 项目本地覆盖更符合直觉；我们只有 user/project 两级、无 enterprise。同名（命令名 = 目录名相同）时**整条替换**，被遮蔽者丢弃 + 一条诊断，**不做字段级合并**。**显式分歧，记档。**
5. **不扫 `.claude/skills`**：官方指南建议「为兼容也扫 `.claude/skills`」，我们守 **D2 不扫**（JackCC 靠软链接入）。
6. **父目录下探 / 子目录按需加载**：CC 会扫 cwd 的**所有父目录**、且子目录的 skill 按需加载。我们**只扫两条固定根**（D2′）。

### 待核实（不据此决策）

CC 对**目录列表**另有预算（单条截断 + 整表约窗口的极小比例）。方向与「需要安全阀」一致，但**具体口径未核实**（若真是 1%×128k，则只能装个位数 skill，与该功能存在矛盾）⇒ **不据此定默认值**，仍用「默认 ≥ 当前语料 ⇒ 今天恒不触发」的安全阀口径（§决策 D）。

---

## 候选方案对比

### 方案 A — 最小复杂度

零新工具（pi 路线：`<location>` + 模型 `read`）；`/skill-name` 复用 `Request::Prompt`、app 层展开；自写 ~60 行 frontmatter 解析；**不做**描述预算；1 个新文件 + 4 处接缝；skill 段**追加在系统提示最末**（⇒「无 skill 的 prompt」是「有 skill 的 prompt」的严格前缀）。

### 方案 B — 可扩展优先

`SkillSource` trait + `SkillRegistry` 合并器 + `SkillCatalog` 窄接口（provider/registry/consumer 三角）；**专用 `skill` 工具**（第 10 个工具，10 处连锁点）；**新 `Request::Skill` 变体**；`InvocationPolicy{model,user}` 四组合 + `aliases-for` + 未知字段 `passthrough`；`SkillPromptBudget{12,000 chars / 280 per desc}`；`SkillLoaded` 事件。**6 个新概念、~45 个新测试、4 期实施**。

### 方案 C — token 预算 / cache 优先

3 个模块、无 trait 抽象；**实测了语料**（推翻「截断能省钱」的假设）；**专用 `skill` 工具但只返回 path（不返回正文）**；工具 spec 里放 56 个 name 的 JSON `enum`；按 `requires.siblings` 准入过滤；**独家发现并在压缩侧修 `fixed_prompt_tokens`**（本设计的采纳点）。

### 对比矩阵

| 维度 | A 最小复杂度 | B 可扩展优先 | C token 预算优先 |
|---|---|---|---|
| 新增概念数 | **2**（`SkillEntry`/`Catalogue`） | 6（+2 trait +registry+tool+`Request` 变体+event） | 4（+tool+`fixed_prompt_tokens`） |
| 新文件 / 改动点 | 1 / ~6 | 4 / ~12 | 1 / ~8 |
| 新增依赖 | **0** | 0（自写解析） | 0 |
| 新工具（连锁点） | **0** | 1（10 处） | 1（7 处） |
| 新协议变体 | **0** | 1 | 0 |
| 模型激活 round-trip | **1** | 1 | **2**（调 tool 拿 path，再 `read`） |
| 每轮固定 token | ~9,600 | ~9,600（目录）+ ~220（工具 spec） | ~9,600（目录）+ ~370（工具 spec+enum） |
| 前缀稳定性 | **强**（末尾追加 ⇒ 严格前缀不变量） | 中（插在 guidelines 后） | 中 |
| 压缩耦合 | ❌ 未处理（**8.7k 盲区**） | ❌ 未处理 | ✅ **修正** |
| 是否可容纳未来远程 source | ❌ 需重写合并层 | ✅ 加一个 source，消费者零改 | ❌ |
| 测试面 | ~20 | ~45 | ~18 |
| 主要风险 | 模型不读 location | 抽象税 + 概念数量 | tool+enum 与「工具面恒定」的张力；返回 path 多一跳 |

---

## 选择理由

**推荐：以 A 为骨架，吸收 C 的压缩耦合修正与 C 的实测纪律，裁剪吸收 B 的遮蔽诊断。**

### A. 零新工具 —— 决定性论证

**`skill` 工具在模型自主路线上买不到任何东西**：模型调 `read(绝对路径)` 与调 `skill(name)` **都是恰好 1 次 tool round-trip**。而工具路线要付：

- `toolset.rs` 的连锁点（`EXPECTED_ORDER`、`vec!`、3 处 `assert_eq!(len,9)`、文档块与注释）；
- `prompt.rs` 的 `tool_snippet` + `tool_guidelines` + 逐字守卫；
- 一个 `Tool` 实现 + `build_tools` 签名改动（工具需注入 skill 列表，因为 `ToolContext` 拿不到）；
- **每轮永久 +~220 token**（工具 spec 进每一次 `ModelRequest.tools`）—— 对 56 个 skill 的用户是净亏。

**净收益为零、代价可数 ⇒ 不做。** pi（本项目主要参照）走的正是这条路。方案 C 的工具**只返回 path**，比 A 还多一跳，是严格劣解。

> 复访条件：若真实使用中观察到模型**只看描述不读 location**，或对绝对路径犹豫，再评估加工具 —— 那时连锁点已点算清楚，是半天的事。

### B. `/skill-name` 复用 `Request::Prompt`，app 层展开

TUI **不读盘**（UI 线程不做 fs IO，也不该持有 frontmatter 剥离逻辑）。TUI 只「认名」：`parse` 未命中内置表但命中 `view.skills` ⇒ 发 `Request::Prompt("/name args")`（原文，零展开）；app 侧 `expand_invocation` 把首 token 是已知 skill 名的消息**重写**为带正文的信封。

**不加 `Request` 变体**：新变体除了协议 + serde + 测试，还会**分叉 app_loop 的回合启动路径**（`turns += 1` / `set_context_window` / `run_turn` 全在 `Request::Prompt` 臂里，新臂要么复制要么重构循环）—— 这是方案 B 的隐藏大代价。复用 `Prompt` 让「展开型命令」与「纯文本输入」走**同一条路**，正是前身文档主张的形状。

### C. 自写 frontmatter 解析（A/C 一致）

守 ADR-0015/0016 的依赖基线（`serde_yaml` 已被上游归档弃用）。实测结构简单（4 种写法），自写解析器可行。**有界读**（只读前 4 KB）把启动期 IO 从 1.5 MB 降到 ~224 KB。

### D. 描述**不按字符截断**，但设**整表字节安全阀** —— 采纳 C 的实测结论

C 实测证明：280 字符截断在这批 CJK 语料上**只省 ~2.1k / 9.4k**，而要「真正省下来」只能砍掉「当…时使用」的触发从句（38/56 条含），那正好是模型**判断该不该选**的唯一依据 —— 省 6.5k 换掉选择准确率，不划算。

故：**截断是安全阀、不是节流阀**。只设一个**整表字节上限**（默认 ≥ 当前语料 ⇒ 今天恒不触发），超限按 name 升序收 + 末尾一行 `- (N more skills omitted)`。它是防「某次加 skill 悄悄吃掉 8% 窗口」的闸门，不是把 18 KB 压成 5 KB 的工具。真正的省额杠杆只有两个，都归用户：① 标 `disable-model-invocation`（每条约 -120 token，零质量损失）；② 自行精简 description。

### E. 压缩耦合修正 `fixed_prompt_tokens` —— 采纳 C 的独家发现（**已验证**）

**实测确认**：`crates/ys-loop/src/basic.rs:140` 的压缩触发是
`estimate_session_tokens(session.messages()) > policy.trigger_at(window)`
—— **只算 session 消息，不算 system prompt，也不算 tools**。9 工具时这个缺口 ≈1.1k（可忽略）；**skill 目录入场后缺口变成 ~9.6k，且不被任何阈值看见**。后果：压缩**系统性地晚触发 ~9.6k token** —— 小窗口（8k/16k）下这会直接把请求顶出窗口。

**修法（加法式、最小）**：`RunLimits` 增 `fixed_prompt_tokens: usize`（**默认 0 ⇒ 既有行为逐字节不变**）；app 在 `main.rs` 启动期用 `estimate_text_tokens(system_prompt) + estimate_text_tokens(&serde_json::to_string(&specs))` 算一次；`basic.rs` 的触发条件改为：

```rust
if estimated + ctx.limits.fixed_prompt_tokens > policy.trigger_at(window) {
```

**刻意不在 `CompactPolicy::trigger_at` 里改**：`trigger_at` 的签名与语义（含 P1 不变量 `trigger > keep`）**原样不动**，压缩加固的三条性质不受影响。扣减发生在比较的**输入端**。

### F. 其余裁决

| 议题 | 裁决 | 理由 |
|---|---|---|
| 命令名来源 | **目录名**（CC 语义）；`name` 仅作显示 `label` | 格式兼容（用户明确要求）；且目录名**必然存在**，少一条「name 非法/缺失怎么办」的分支 |
| `disable-model-invocation` / `user-invocable` | **两个 `bool` 字段**（拒绝 B 的自定义 `InvocationPolicy` 类型） | 格式兼容需要支持 CC 的**两个**配对字段，但用两个 `bool` 即可，不值得为它造新类型 + 解析分支；实测语料只用 `disable-model-invocation`（3 个） |
| 冲突优先级（同名） | **project > user**（**已确认 2026-09-20**） | 与 Agent Skills 客户端指南一致，且符合「项目本地覆盖」直觉。**与 CC 相反**（CC: personal > project），显式记档（§CC 兼容性 偏离 4）。**精确语义**：命令名（=目录名）相同 ⇒ **整条替换**（`path`/`description`/`label`/两个 invocation 标志全部取高优先级那条，**不做字段级合并**）；被遮蔽的那条**丢弃 + 产一条启动期诊断**；正文从高优先级的 `path` 加载。**绝不降级为「合并」** —— 半套 frontmatter 配另一套正文是纯粹的 bug 来源 |
| aliases / `passthrough` | **不做** | 无需求；`aliases-for` 在 JackCC 零命中 |
| `SkillSource` trait / registry | **不做** | 今天只有**一种** source 实现；OCP 收益要到第二个 source 才兑现。将来真要远程，是重写合并层（一个函数）的代价，不值得现在付抽象税 |
| `SkillLoaded` 事件 | **不做** | 两条路线的正文都进 transcript（ToolResult / user message），已有回执 |
| 遮蔽（项目级覆盖用户级） | **做诊断**（裁剪版 B 的 audit） | 采纳 B 的洞察但砍掉四层：仅「遮蔽发生时产一条启动期提示」（走既有 `merge_hints` 三源出口）。回应 R4「指令完整性」——一个仓库能静默换掉你的 `/git-status` |
| 注入位置 | **系统提示最末**（`cwd` 之后），纯追加 | 给出可测的严格前缀不变量；且是**最小 diff**（追加 vs 插入） |
| 自定义 prompt 短路 | **保留短路**（不注入 skill）+ 启动期一行提示 | 尊重既有契约（`test_custom_prompt_takes_precedence` 逐字锁死）；但**不静默** —— 用户知道 `/skill-name` 仍可用 |
| `/` 命名空间冲突 | **内置命令 > skill** | 内置命令是 `Request` 穷尽 match 的契约，skill 是可增删的数据；若 skill 能遮蔽 `/status`，装一个 skill 就能让内置命令静默失效 |
| 正文缓存 | **不缓存，每次激活即时读盘** | skill 目录可写（JackCC 的 `sequential-workflow/learned/`、`plans/` 有 agent 写入的文件）；缓存会给出陈旧指令 |
| 扫描根 | 固定两条，**不开路径配置项**（D2′） | 见 context.md D2′；实现上仍以显式 `&Path` 传入（测试注入接口，非用户配置） |

---

## 已知风险与待验证项

- [x] **R1 模型是否可靠地 `read` location —— ✅ 已验证（2026-09-20）**。真实模型（MiniMax-M3）+ 真实 agent（`--json`）跑探针：**5/5** 命中场景下模型**第一跳即 `read` 绝对路径**，且**照正文行事**（复刻 git-status 规定的 6 条命令 + 输出表格骨架）；**12 个 skill 的干扰测试里零误读**。**但附带一个限定**：任务可被直接回答、描述匹配是**推断性**时，模型可能**根本不加载**（1 次观测）⇒ 模型自主路线是**机会性**的，`/skill-name` 的**确定性**路线因此**必需**（不是冗余）。**§选择理由 A 的复访条件（「只看描述不读 location」）未出现。** 详见 `tmp/skill-probe/NOTES.md`。
- [ ] **R2 前缀的 cwd 相关抖动**（**必然发生，只能限定**）：项目级 skill 只从 `<cwd>/.yushan/skills` 扫 ⇒ 同一目录列表在不同 cwd 下不同 ⇒ 前缀不同。**这不是本设计引入的** —— `build_system_prompt` 早已把 `cwd` 写进提示词末尾、`<project_context>` 也随 cwd 变。要写进文档的是它的**量级**（多 ~9.6k 的一次性 miss），以及一条纪律：**绝不为省缓存而阉掉项目级 skill**。
- [ ] **R3 整表预算的可见性降级**：超预算后模型看不到被收起的部分。缓解：`omitted` 计数进 `/status`；文案给「如何列出全部」的线索。
- [ ] **R4 项目级覆盖用户级 = 静默换指令**：已用诊断缓解，但**诊断只在启动期可见**。残余风险记档。
- [ ] **R5 小窗口下 `fixed_prompt_tokens` 可能 ≥ `trigger_at(window)`** ⇒ 压缩触发但帮不上（消息已到底）、或永不触发而溢出。**需在实现时处理**：`fixed ≥ trigger_at` 时跳过压缩并在启动期给一条明确提示（「窗口装不下固定开销，请调大 `context_window` 或减少 skill」）。这是 E 的新边界，**必须补测试**。
- [ ] **R6 压缩会把刚注入的 skill 正文摘要掉**（E5）：正文进 session ⇒ 会被摘要。长任务里模型可能凭印象行事。**不引入「skill 正文免疫压缩」这种新机制**（会破坏压缩的单一阈值语义）。缓解：名字常驻系统提示 ⇒ 模型可重新 `read`。
- [ ] **R7 正文里 ~20 个 Claude 专有工具名静默失效**（`Agent` 16~17 个）：**不做符号映射**（映射表是把上游知识写死在代码里，上游一改就撒谎）。改为在目录前言里加一句**能力边界声明**（「本 runtime 无 subagent / plan mode」），逐字断言。
- [ ] **R8 token 估算器未与真实 `Usage` 对账**：落地后**必须**用真实 `Usage.input_tokens` 差（有/无 skill 段）校准一次，据此回看 `Budget.total_bytes` 与压缩阈值。**不要用静态字节估算替代真实 usage。**

### `prototype` 验证结果（R1，2026-09-20 —— 已完成）

用**真实模型**（MiniMax-M3，真凭证）+ **真实 agent**（`target/debug/ys-coding-agent --json`）跑探针：
把设计里的 `<available_skills>` 块写进临时 git 仓库的 `.yushan/SYSTEM.md`（借 `find_custom_prompt` 短路点注入，**零生产代码改动**），任务用「这个仓库现在有什么未提交的改动？」。

| 观测 | 结果 |
|---|---|
| 命中场景下是否 `read` location | **5/5**（首跳即 `read {"path": "/Users/zs/.claude/skills/git-status/SKILL.md"}`） |
| 12 个 skill 干扰下是否选对 | **2/2 只读 git-status，零误读** |
| 是否**照正文行事** | ✅ 复刻了正文规定的 6 条 `bash` 命令（连 `description` 字段都一致），并输出正文规定的表格骨架 |
| B6 的 ` ```tool ` 围栏 | ✅ 模型**自行翻译**成真正的 `bash` 调用 —— 实测严重度**低于**设计假设 |
| **反例** | 当任务可被直接回答、描述匹配是**推断性**时（「用最少的字解释 X」），模型可能**根本不加载**（1 次观测） |

**两条结论**：① 零工具路线成立，`read` 会被调用且会被照做；② **模型自主激活是机会性的** ⇒ `/skill-name` 的确定性路线是**必备**，不是冗余。

探针（可抛弃，位于 gitignore 的 `tmp/skill-probe/`，含 `NOTES.md` 完整数据）。

---

## 相关文档

- [`context.md`](./context.md) — 上下文、约束、已定决策（D1/D2/D2′/D4）
- [`../../adr/0019-skills.md`](../../adr/0019-skills.md) — 决策记录
- [`../../adr/0017-compaction-hardening.md`](../../adr/0017-compaction-hardening.md) — 压缩加固（本设计的 E 决策与之耦合）
- 参照实现：`tmp/pi/packages/coding-agent/src/core/skills.ts`（零工具路线）、`tmp/Codewhale/docs/SKILLS.md`（预算不变式）
- 格式规范：[Agent Skills 规范](https://agentskills.io/specification.md)、[给 agent 加 skills 支持](https://agentskills.io/client-implementation/adding-skills-support.md)、[Claude Code skills 文档](https://code.claude.com/docs/en/skills.md)

---

## 实现回写（as-built，2026-09-21）

**本文的正文照原样保留**（它是设计推导的记录）。实现已落地并通过两轮独立审查，**实际形态与偏差的完整记录**在
[`../../design-final/skills.md`](../../design-final/skills.md) —— 该文含「与设计的偏差」一节（12 条）与遗留清单。

实现**逐条兑现了本文的核心裁决**：零新工具（工具仍 9 个）/ 零新协议变体（`Request` 仍 7 个）/ 零新依赖 /
`Request::Prompt` 复用且回合启动路径不分叉 / project > user 整条替换 / `description` 缺失或空 ⇒ 整条不加载 /
系统提示末尾纯追加（严格前缀不变量）/ 内置命令 > skill / 正文不缓存 / `fixed_prompt_tokens` 加在比较的输入端
且 `CompactPolicy::trigger_at` 语义不动 / R5 早退 + 启动期提示。

**最需要知道的三条偏差**（细节与理由见 as-built 文）：

1. **XML 转义（`escape_xml`）是本文没有的** —— 审查发现「不可信输入进 XML」合同后补的。
   `description` / `command` / `path` 会原样进 `<available_skills>`，实测语料已命中裸 `<`（`lark-event`）。
   转义在**渲染时**做（`SkillEntry` 仍持原文，供 `/` 浮层与 `expand_invocation` 匹配），**正文绝不转义**；
   实现是单趟字符扫描，不是 pi 的 replace 链。
2. **`load_body` 剥掉了 frontmatter** —— 与本文 §CC 兼容性「已知偏离 2」及设计计划 §3.2 的「给原文」**相反**。
   **最终裁决：剥**。理由是 `frontmatter_block` 本就是 `parse_frontmatter` 需要的同一个纯函数，
   复用**不产生**本文担心的「第二个解析器」；代价是两条路线不再给模型同一段文本
   （路线 1 走 `read` 拿到含 frontmatter 的原文，路线 2 拿到剥过的正文），**该代价已记档**。
3. **`load_body` 的有界读是 `max(cap, 4096) + 4096`**（本文只给了 `cap_bytes`）。多留的 4 KB 是给
   frontmatter 闭合符的余量；**有意偏离**：frontmatter 本身 > 4 KB 的文件会被当作「无 frontmatter」——
   这是「有界性（P1-1：防 GB 级分配）换来的可见损失」，非语料形态。

其余偏差（`scan` 靠位置判 scope 的承重隐式契约 / 第三类「目录名非法」诊断 / `user_invocable` 过滤点落在
`build_view` / `-p`·`--json` 下未知 `/xxx` 仍原文发给模型 / `parse_frontmatter` 返回私有结构体 /
「遇到即产诊断」收窄为只诊断被消费的 key / `cwd == $HOME` 的根去重）逐条列在 as-built 文里。
