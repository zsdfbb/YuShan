# 0019 — Skills 支持：零新工具、展开型命令、压缩耦合修正

给 YuShan coding agent 加 **skills** —— ① 发现 `SKILL.md`（`~/.yushan/skills` + `.yushan/skills`，自写 frontmatter 解析，零新依赖）；② 渐进式披露（`name`+`description` 常驻系统提示，正文按需）；③ 两条激活路（模型自主 + `/skill-name`）；④ **不新增工具**（推翻既定 D1 的工具半边）；⑤ **修正压缩触发对 system prompt / tools 的盲区**。skill 正文由既有的 `read` 工具加载，`/skill-name` 复用既有 `Request::Prompt`。

## Status

accepted（2026-09-20）· 上游：`docs/arch/skills/context.md`（需求/约束/D1/D2/D2′）、`docs/arch/skills/design.md`（三候选融合）。
**验证（2026-09-20）**：决策 1（零工具路线）的核心假设 R1 已用真实模型 + 真实 agent 验证通过（见「后果」）；决策 2 的确定性路线被同一次验证确立为**必备**。

## 背景

- **skills 在本仓零代码**（`grep -rn skill --include=*.rs` 仅 1 处无关命中）。要复用的是 JackCC 的 56 个 `SKILL.md`（用户自行软链进 `~/.yushan/skills`）。
- **既定决策 D1**（`context.md`）原案为「**恒注册一个 `skill` 工具** + `/skill-name` 命令，两者共享同一份定义」。D1 的推论 1 同时警告：加第 10 个工具会触及一串连锁点（工具面顺序 / `assert_eq!(len,9)` / `prompt.rs` 的 snippet 与 guideline 及断言）。
- **D2 / D2′**：发现路径 = `~/.yushan/skills` + `.yushan/skills`，**不扫 `~/.claude/skills`**，**不开路径配置项**。本 ADR 不变更此决策。
- **同类软件调研**（`design.md` §候选方案对比）：
  - **pi**（本项目主要参照）：**零新工具** —— 注入 `name`+`description`+`location`（绝对路径），声明「用 `read` 加载」，模型自己读。`disable-model-invocation` 过滤出提示词。仅当 `read`/`bash` 在场才注入。
  - **CodeWhale**：四层 catalog/audit/mutation/TUI；prompt 预算硬上限；`invocation: explicit-only`；`aliases-for`。重方案可行，但**不证明必要**。
  - **Claude Code**：`Skill` 工具按名加载正文（格式源头）。
- **关键约束**（决定形态，不是审美）：
  - **工具面必须恒定**、**prompt 前缀逐字节稳定**（prompt cache）。
  - `Agent::system_prompt` **启动期冻结、无 setter**（`set_context_window` 是唯一的运行期 setter 特例）⇒ skill 列表只能启动期一次性进系统提示。
  - `ToolContext` **拿不到 `Model`**。
  - **依赖基线**：ADR-0015 否决 `ignore`/`regex`/`globset`/`walkdir`；ADR-0016 只破例一个 `htmd`。`serde_yaml` 已被上游归档弃用。
  - `ys-tui-coding` **不依赖 `ys-runtime`**；TUI 拿数据的唯一通路是 `CodingView`。
  - **ADR-0007**：给 `Agent` 加新公开 API 须先写 ADR。
- **实测语料**：56 个 skill；`description` 合计 **18,226 B**，其中 **4,886 个 CJK 字符**。本仓估算器下 ≈ **9,442 token**（CJK ≈ 1 token/字节，ASCII 1/4）。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 最小复杂度** | 零新工具（`<location>` + `read`）；`/skill-name` 复用 `Request::Prompt` + app 层展开；自写解析；无预算；末尾追加 | 2 个新概念、1 个新文件、0 依赖、0 连锁点 | 依赖模型读 location；未处理压缩耦合 |
| **B 可扩展优先** | `SkillSource` trait + registry + 专用 `skill` 工具（返回正文）+ `Request::Skill` 变体 + 四组合 invocation + aliases + 预算 | 容纳未来远程 source；有审计 | 6 新概念、10 处连锁点、每轮 +~220 token、~45 测试；**新变体分叉 app_loop 回合启动路径** |
| **C token 预算优先** | 无 trait；专用 `skill` 工具（**只返回 path**）+ JSON `enum`；实测语料；**独家发现并修压缩盲区** | **实测驱动**；修正 8.7k 盲区 | 工具只返回 path ⇒ **2 跳**（比 A 多一跳）；enum 与「工具面恒定」有张力 |

## 决策

**1. 不新增 `skill` 工具 —— skill 正文由既有的 `read` 加载**（采纳 A，推翻 D1 的工具半边）

目录注入 `name` + `description` + `location`（绝对路径）；前缀句声明「用 `read` 打开 location，相对路径按 skill 目录解析」。模型自主路线因此是**恰好 1 次 tool round-trip**——与专用 `skill` 工具**相同**。

**理由**：工具路线净收益为零（同样的 round-trip），却要付出 10 处连锁点 + 每轮永久 +~220 token（工具 spec 进每次 `Request.tools`）+ 一个 `Tool` 实现 + `build_tools` 签名改动。`read` 已存在且已能用绝对路径读 workspace 之外（`ReadTool` 是 `workspace.join(path)`，绝对路径替换基址）。
**复访条件**：若真实使用中观察到模型只看描述、不读 location，再评估加工具。

**2. `/skill-name` 复用 `Request::Prompt`，展开在 app 层**（采纳 A，关闭 Q18）

TUI 只「认名」（`parse` 未命中内置表但命中 `view.skills` ⇒ 发 `Request::Prompt("/name args")` 原文）；`app_loop` 的 `Prompt` 臂调 `expand_invocation` 重写为带正文的信封。**不加 `Request` 变体** —— 新变体除协议/serde/测试外，还会**分叉 `app_loop` 的回合启动路径**（`turns += 1` / `set_context_window` / `run_turn` 都在 `Prompt` 臂里）。TUI **不读盘**（UI 线程不做 fs IO、也不该持有 frontmatter 剥离逻辑）。

**3. 自写 frontmatter 解析，零新依赖**（采纳 A/C）

只认 `name` / `description` / `disable-model-invocation`；覆盖实测 4 种写法（裸标量 / 引号 / `>` 折叠 / `>-` 折叠）；「key 后的缩进行即续行」一条规则同时覆盖折叠块与 plain-multiline；`description` 缺失/空 ⇒ 整个 skill 不加载。**有界读**（只读前 4 KB 取 frontmatter，不读正文）。

**4. 描述不按字符截断，只设整表字节安全阀**（采纳 C 的实测结论）

实测证明 CodeWhale 的 280 字符预算在 CJK 语料上只省 ~2.1k/9.4k，而要真正省下来必须砍掉「当…时使用」的触发从句（模型判断该不该选的唯一依据）。故截断**是安全阀、不是节流阀**；默认阈值 ≥ 当前语料（今天恒不触发），超限按 name 升序收 + 显式 `omitted` 行。**所有预算按字节定义，不按字符**（CJK token 密度会打穿字符预算）。

**5. 修正压缩触发对 system prompt / tools 的盲区**（采纳 C 的独家发现，已验证）

`basic.rs:140` 的触发是 `estimate_session_tokens(messages) > trigger_at(window)` —— **只算消息，不算 system prompt 与 tools**。9 工具时缺口 ≈1.1k（可忽略）；skill 目录入场后 **≈9.6k 且不被任何阈值看见** ⇒ 压缩系统性晚触发。

修法：`RunLimits` 增 `fixed_prompt_tokens: usize`（**默认 0 ⇒ 既有行为逐字节不变**），app 启动期算一次；触发条件改为 `estimated + fixed_prompt_tokens > trigger_at(window)`。**`CompactPolicy::trigger_at` 的签名与语义（含 P1 不变量 `trigger > keep`）不动** —— 扣减发生在比较的输入端。

**6. 目标格式 = Claude Code / Agent Skills 格式**（命令名取目录名；`name` 仅显示；支持两个 invocation 字段）

用户确认 skill 格式**与 Claude Code 一致**（参照 `~/.claude/skills/grill-me`）。据此定：

- **命令名 = 目录名**，frontmatter `name` 只作**显示标签**（CC 语义）。实测唯一不一致：`fireworks-tech-graph-main/` 目录 vs `name: fireworks-tech-graph`。
- **支持 `disable-model-invocation` 与 `user-invocable` 两个配对字段**，各建模为 `bool` —— 格式兼容需要，但不为自己造 `InvocationPolicy` 类型（拒绝 B 的自定义类型）。
- frontmatter 解析覆盖**实测全部形态**（裸标量 30 / 引号 26 / `>` 折叠 2 / `>-` 1；`[`/`{` 开头按 CC 规则判为非字符串并丢弃），守 ADR-0015 **不引 YAML crate**。
- **冲突优先级取 `project > user`**（**已确认 2026-09-20**），与 CC 的 `personal > project` **相反**，理由是 Agent Skills 客户端指南如此、且符合「项目本地覆盖」直觉（我们无 enterprise 级）。**精确语义**：命令名相同 ⇒ **整条替换**（`path`/`description`/`label`/invocation 标志全取高优先级者），被遮蔽者丢弃 + 一条启动诊断，**不做字段级合并**。
- **不扫 `.claude/skills`**（守 D2）、**不扫父目录**（守 D2′）—— 与 CC 不同，显式记档。

> 机制**不同**但格式**兼容**：CC 用 `Skill` 工具触发，我们用 `read`。实测 56 条正文**无一条**提及 `Skill` 工具（`grep "Skill tool"` 零命中）⇒ 该差异不破坏任何既有 skill。完整兼容性对照见 [`design.md`](../../arch/skills/design.md) §Claude Code 格式兼容性。

**7. 其余裁决**

- **不做** `SkillSource` trait / registry（今天只有一种 source；OCP 收益要到第二个 source 才兑现）。
- **不做** aliases / `passthrough` / `SkillLoaded` 事件 / JSON `enum`。
- **做**遮蔽诊断：项目级覆盖用户级时，产一条启动期提示（走既有 `merge_hints` 三源出口）—— 回应「指令完整性」（一个仓库能静默换掉你的 `/git-status`）。
- 注入位置 = **系统提示最末**（纯追加 ⇒ 可测的严格前缀不变量）。
- **保留**自定义 prompt（`SYSTEM.md`）短路，但**发一行启动提示**（不静默）。
- `/` 命名空间冲突：**内置命令 > skill**。
- 正文**不缓存**，每次激活即时读盘（skill 目录可写）。
- Q3 落点 = **app 层**（`apps/coding-agent/src/skills.rs`），**维持**前身文档「不做 `ys-skill` crate」的裁决。

## 后果

**正面**
- 新增概念最少（2 个）、1 个新文件、0 新依赖、**0 新工具 / 0 新协议变体 / 0 新事件**；`app_loop` 的回合启动路径不分叉。
- 工具面与 prompt cache 前缀的既有不变量**不受任何结构性威胁**。
- 顺带修正一个**既有的真实缺陷**（压缩触发看不见固定开销）——它以 9 工具时 ~1.1k 的规模潜伏，skills 会把它放大 8 倍。
- 与 pi（主要参照）同构，降低理解成本。

**负面 / 接受的代价**
- **依赖模型读 location**（本设计唯一无法静态验证的假设）。**✅ 已于 2026-09-20 用真实模型验证**：5/5 命中场景下模型首跳即 `read` 绝对路径并照正文行事，12 skill 干扰测试零误读。**但附带限定**：模型自主激活是**机会性**的 —— 任务可被直接回答、描述匹配为推断性时可能**不加载**（1 次观测）⇒ **`/skill-name` 的确定性路线是必备，不是冗余**（见决策 2）。详见 `tmp/skill-probe/NOTES.md`。
- 每轮固定 +~9.6k token（实测语料）；只能靠用户标 `disable-model-invocation` 或精简 description 来降，**代码不替用户裁**。
- 项目级 skill 使前缀**随 cwd 变化**（量级 ~9.6k 一次性 miss）—— 这是既有 `<project_context>` 行为的一部分，非本设计引入。
- 小窗口下 `fixed_prompt_tokens` 可能 ≥ `trigger_at`（压缩帮不上或永不触发）—— **实现时须处理并补测试**。
- 20 个依赖 Claude 专有工具的 skill 在正文层面**静默失效**（只在目录前言加一句能力边界声明，不做符号映射）。

## 关联

- [design.md](../../arch/skills/design.md) — 设计方案与三候选对比
- [context.md](../../arch/skills/context.md) — 上下文、约束、D1/D2/D2′
- [0017-compaction-hardening.md](./0017-compaction-hardening.md) — 压缩加固（决策 5 与之耦合：改的是触发比较的输入端，不动 `CompactPolicy`）
- [0007-agent-public-api.md](./0007-agent-public-api.md) — 本 ADR 不新增 `Agent` 公开 API，故未触发其要求
- [0015-search-tools-via-external-binaries.md](./0015-search-tools-via-external-binaries.md) / [0016-web-tools-design.md](./0016-web-tools-design.md) — 依赖基线与「工具面恒定」
