# Skills 支持 — As-built

> 性质：**实际建成记录**。设计见 `docs/arch/skills/design.md`（+ 上游 `context.md`），
> 决策见 `docs/adr/0019-skills.md`，设计计划见 `docs/design-plans/2026-09-21-skills.md`
> （其 §4 记录了实现前的 5 条现实约束 D1–D5）。
> 日期：2026-09-21

## 一句话

coding agent 支持 **skills**：启动期扫描 `SKILL.md`（自写 frontmatter 子集解析，**零新依赖**），
`name` + `description` + **绝对路径**常驻系统提示，正文按需加载。两条激活路 —— 模型自主
（`<location>` + 既有 `read`）/ 用户 `/skill-name`（app 层展开成带正文的信封）。**零新工具**
（工具仍 **9 个**，`toolset.rs` 零改动）、**零新协议变体**（`Request` 仍 **7 个**）、**零新依赖**。

## 交付物

| 路径 | 角色 | 新建？ |
|---|---|---|
| `apps/coding-agent/src/skills.rs` | **M1 领域逻辑**（全部）：`scan` / frontmatter 子集解析 / `catalogue` / `load_body` / `expand_invocation` / `escape_xml`。纯数据、**不 print**、不读 env | ✅ 新建（~1750 行，含 34 条测试） |
| `apps/coding-agent/tests/skills_cli.rs` | T14：skill 诊断在 `-p` / `--json` 下的**去向**（子进程、无凭证） | ✅ 新建（3 条） |
| `apps/coding-agent/tests/e2e_skills.rs` | T15：R8 对账 —— 真实 `Usage.input_tokens` 差 vs 静态估算 | ✅ 新建（`#[ignore]`，**已校准**：真实/估算 = 0.704，见下） |
| `apps/coding-agent/src/main.rs` | 启动期装配（扫描 / 归一 / 提示合并 / `fixed_prompt_tokens`）；`-p` / `--json` 的 `expand_task` | 改 |
| `apps/coding-agent/src/prompt.rs` | `build_system_prompt(…, &Catalogue)` 末尾追加 `<available_skills>`；`has_custom_prompt` | 改 |
| `apps/coding-agent/src/config.rs` | `Config.skills` / `skills_omitted`；`default_skill_roots` / `load_skills` | 改 |
| `apps/coding-agent/src/view.rs` | `build_view` 透传 `skills` / `skills_omitted`，**并在此过滤 `user_invocable`** | 改 |
| `apps/coding-agent/src/app_loop.rs` | `Request::Prompt` 臂里**唯一**的展开点 `expand_skill_invocation` | 改 |
| `crates/ys-tui-coding/src/{view,commands,completion,events,lib}.rs` | `CodingView.skills`；`parse_with_skills` / `help_text_with_skills`；补全候选两段；`SkillEntry` 导出 | 改 |
| `crates/ys-component/src/limits.rs` | `RunLimits.fixed_prompt_tokens`（默认 0） | 改 |
| `crates/ys-loop/src/basic.rs` | Step 4b 触发含固定开销 + **R5 早退** | 改 |

**未动**：`toolset.rs`（工具面 9 个、`EXPECTED_ORDER`、3 处 `assert_eq!(len, 9)` 全未碰）、
`crates/ys-protocol`、`crates/ys-tui-coding/src/{draw,prompter}.rs`（这两处测试里的 `CodingView`
字面量走 `..CodingView::for_test()`，加字段不需要它们改 —— 设计计划 §4 D4 预判的「9 处穷尽字面量
都要显式补字段」**未发生**，实际只有 `view.rs` 的 `Default` / `for_test` 与
`apps/coding-agent/src/view.rs:71` 需要动）。

## 实际形态

### 启动期装配顺序（`main.rs`，一次、此后冻结）

```
Config::from_env()
  → registry.load_auth()
  → load_user_config(~/.yushan/config.json)
  → roots = Config::default_skill_roots(&cwd)        // [<cwd>/.yushan/skills, <home>/.yushan/skills]
  → let skill_diags = config.load_skills(&roots)     // scan + catalogue（entries 落 config.skills）
  → let catalogue = skills::catalogue(config.skills.clone(), Budget::default())
        config.skills_omitted = catalogue.omitted    // lines 供 prompt，omitted 供 view
  → state.json 恢复 provider/model
  → let mut limits = resolve_limits(&config)
  → sweep_fetched → toolset::build_tools → (tools, tool_hint)
  → let specs = tools.iter().map(spec)
  → let system_prompt = prompt::build_system_prompt(&cwd, &specs, &catalogue)   // 末尾追加
  → limits.fixed_prompt_tokens = fixed_prompt_tokens(&system_prompt, &specs)    // 只能在此处量
  → let hint = startup_hints(tool_hint, &skill_diags, custom_prompt_note(..), fixed_cost_note(&limits))
```

- **`catalogue` 实际被调用两次**：一次在 `Config::load_skills` 内（归一 `entries`），一次在
  `main` 里（取 `lines`）。`catalogue` 是纯函数、无 IO，重复调用只是白算一遍（语料规模下可忽略），
  但读代码时别以为只有一处。
- **`fixed_prompt_tokens` 的赋值必须后置**（计划 §4 的 D3）：`resolve_limits` 早于
  `build_system_prompt`，故 `limits` 声明为 `mut`，在提示词派生之后补写。
- **提示合并成一条、绝不互相覆盖**（`startup_hints`，四源 `"\n"` 连接）：工具降级 + skill 诊断
  + 自定义 prompt 代价说明 + R5 窗口警告。去处按模式三分：TUI → `Outbound::Output`（transcript
  第一条）；`-p` / `--json` → **stderr**（且**绝不**混进 stdout 的逐行 `Envelope`）。
- `skills.rs` **不 print、不读 env**：扫描根由调用方以 `&[PathBuf]` 显式传入，诊断是**返回值**。

### `/skill-name` 端到端（路线 2，确定性路线）

```
TUI  events.rs::submit
  → commands::parse_with_skills(text, &app.view.skills)    // 内置表未命中才查 skill
  → Action::Request(Request::Prompt(原文，含 args))         // 零展开、零协议改动
app  app_loop Request::Prompt 臂
  → expand_skill_invocation(message, &config.skills)        // 唯一接线点，在 turns += 1 之前
  → <skill name="…" location="…">\n{正文}\n</skill>\n\n{args}
  → 走既有的 turns += 1 / set_context_window / run_turn（**回合启动路径不分叉**）
```

- TUI **不读盘**、不碰 frontmatter：`ys_tui_coding::SkillEntry` 只有 `command` / `label` /
  `description` 三个显示字段（**不携带 `user_invocable`**，也不携带 `path`）。
- 触发条件（`app_loop::expand_skill_invocation`）：**首块必须是 `Text`**；未知命令 /
  `user-invocable: false` / 正文读不出 ⇒ **原样返回**（宁可发一条普通消息，也不塞半截指令）。
- 命中并展开时落一行日志（`logging::log`，含展开后字节数）；未命中静默。
- **`-p` / `--json` 走同一套**：`main::expand_task(task, &config.skills)`，三模式行为一致。

### 模型自主（路线 1，机会性路线）

系统提示末尾（`cwd` 之后）追加，**门控**：工具表里有 `read` **且** `catalogue.lines` 非空才追加，
否则一字不加（不留空行）：

```
<available_skills>
The following skills are available. Each entry is `- <command>: <description> (location: <absolute path>)`.
To use a skill, open its `location` with the read tool and follow the instructions in that file.
Relative paths inside a skill body (e.g. `references/...`, or a sibling skill) resolve relative to that skill's own directory, not the current working directory.
This runtime has no subagents and no plan mode — ignore instructions that ask for them.

- <command>: <description> (location: <abs path>)
…
</available_skills>
```

- 模型据此调**既有** `read`（绝对路径，`ReadTool` 是 `workspace.join(path)`，绝对路径替换基址）。
- 前言里三句是**承重**的：指出用 `read` 打开 `location`、相对路径按 skill 自身目录解析（实测多条
  正文交叉引用兄弟 skill）、能力边界声明（正文里约 20 处 Claude 专有工具名在本 runtime 静默失效，
  **不做符号映射**）。
- 目录行三个字段**全过 `escape_xml`**（见「与设计的偏差」1）。

## 接口

`apps/coding-agent/src/skills.rs` 的公开面（照实际签名）：

```rust
pub const DEFAULT_BUDGET_BYTES: usize = 32 * 1024;   // 整表字节安全阀（默认，今天恒不触发）

pub enum Scope { User, Project }                     // 仅用于去重与诊断；Project 优先

pub struct SkillEntry {
    pub command: String,       // 目录名 —— `/` 命令名 + 去重键
    pub label: String,         // frontmatter `name` —— 仅显示用（缺省 = command）
    pub description: String,   // 已单行化
    pub path: PathBuf,         // SKILL.md 路径（原样拼接，**不 canonicalize**）
    pub scope: Scope,
    pub model_visible: bool,   // !disable-model-invocation
    pub user_invocable: bool,  // !(user-invocable: false)
}

pub struct Budget { pub total_bytes: usize }         // Default = 32 KB
pub struct Catalogue {
    pub lines: String,          // 进系统提示的段（只有 model_visible 的条目）
    pub bytes: usize,           // lines.len()（逐字节）
    pub omitted: usize,         // 因预算被收起的条数
    pub entries: Vec<SkillEntry>,  // 全部条目（含 model_visible=false），已去重、按 command 升序
}

pub fn scan(roots: &[PathBuf]) -> (Vec<SkillEntry>, Vec<String>);
pub fn catalogue(raw: Vec<SkillEntry>, budget: Budget) -> Catalogue;   // 纯函数、无 IO
pub fn load_body(path: &Path, cap_bytes: usize) -> Result<String, String>;
pub fn expand_invocation(text: &str, skills: &[SkillEntry]) -> Option<String>;
```

**内部**（非 pub，但承载语义）：`escape_xml`、`render_line`、`body_read_limit`、
`read_prefix` / `read_head`、`frontmatter_block` / `parse_block` / `decode_scalar` /
`single_line`、`shadow_diagnostics`、`normalise`、`truncate_body`、`floor_char_boundary`。
`parse_frontmatter` 返回**私有结构体** `Frontmatter`（含 `dropped: Vec<(Key, char)>` 供诊断），
**不是**设计里的四元组 —— 见「与设计的偏差」8。

`crates/ys-tui-coding` 侧新增 `pub struct SkillEntry { command, label, description }`（已从
`lib.rs` 导出）；`crates/ys-component` 加 `RunLimits.fixed_prompt_tokens: usize`（默认 0）。

## 与设计的偏差

对照 `docs/arch/skills/design.md`（及 ADR-0019）逐条记录。**偏差就是偏差，不写成「优化」。**

1. **`escape_xml` 是设计里没有的** —— 设计全文（以及设计计划、ADR）**没有一处**提到 XML 转义；
   `render_line`（`command` / `description` / `path` 三个字段）与 `expand_invocation` 的属性值
   现在都过它。**由来**：审查时发现「不可信输入进 XML」这条合同 —— `description` 来自 skill 作者写的
   frontmatter，`command` 来自目录名，`path` 来自扫描根；实测语料已命中（`lark-event/SKILL.md` 的
   description 含 `` `lark-cli event consume <EventKey>` ``，不转义时裸 `<EventKey>` 直接破坏
   `<available_skills>` 结构）。转义**只在渲染时做**（`SkillEntry` 仍持原文供 `/` 浮层与
   `expand_invocation` 匹配）；**正文绝不转义**（markdown，转义是内容损坏）。与 pi 的 `escapeXml`
   输出逐字节相同，但实现是**单趟字符扫描**而非 5 次 `replace` 链 —— 从根上不存在
   「`&` 必须先替换」的二次转义顺序陷阱。
   （`label` = frontmatter `name` **不在**此列：它只进 TUI 浮层做纯文本渲染，不进任何 XML 块。）
2. **`load_body` 剥掉 frontmatter（设计与 ADR 原写「不剥、给原文」）** —— 设计
   §CC 兼容性「已知偏离 2」与设计计划 §3.2 都明确：CC 剥离后再注入，**我们 `read` 的是含
   frontmatter 的原文**，理由是「避免维护第二个『剥 frontmatter』函数」。**实现剥了**。
   **最终裁决：剥。理由**：① `load_body` 无论如何都要有界读、且 `frontmatter_block` 是
   `parse_frontmatter` 已经需要的**同一个**纯函数 —— 复用它**没有**引入设计所担心的「第二个解析器」，
   设计那条理由的成本项在实现里为零；② frontmatter 对模型是噪声（`name`/`version`/`metadata`
   已被目录行表达），而正文才是指令；③ 顺带 `trim()` 掉首尾空白，信封形状更干净。
   **代价（必须记）**：两条路线**不再给模型同一段文本** —— 路线 1（模型自己 `read`）拿到的是
   **含 frontmatter 的原文**（+ 行号 + 截断提示），路线 2（`/skill-name`）拿到的是**剥过的正文**。
   设计 §关键数据流那句「两条路线给模型的都是 SKILL.md 原文」**不再成立**。
3. **`scan` 靠位置判 scope（`roots[0]` ⇒ `Project`）** —— 设计只给了 `scan(roots: &[PathBuf])`
   的签名与 `roots = [<cwd>/.yushan/skills, ~/.yushan/skills]` 的顺序，**没有写明**「位置即 scope」。
   实现把它定成**承重契约**：`roots[0]` ⇒ `Project`、`roots[1..]` ⇒ `User`，同名时 project 胜。
   推论：**生产侧必须传 `[project, user]`**（`Config::default_skill_roots` 是唯一来源），
   顺序错了就是静默的语义错误（用户级反而遮蔽项目级），而**没有任何编译期或运行期检查**拦它。
   `config.rs` 里 `default_skill_roots_are_project_first_and_never_claude` 是这个顺序的唯一守卫。
4. **诊断 3 类，其中「目录名非法」是设计里没有的** —— 设计只要求两类（同名遮蔽、字段形态非法）。
   实现加了第三类：**目录名含空白或控制字符 ⇒ 整条跳过 + 诊断**。理由是目录名**就是**命令名且会
   原样进目录行 —— POSIX 允许目录名含换行（`mkdir $'a\nb'`），能往系统提示里**伪造整行**
   （多出一条假的 `- name: desc (location: …)`），ESC 则能往 TUI 塞 ANSI 序列。
   选择「丢弃 + 诊断」而非「复用单行化」：单行化会把 `a\nb` 悄悄改名成一个**永远敲不出来**的命令。
5. **`user_invocable: false` 的过滤点在 `build_view`（app 侧）** —— 设计只说
   「`user-invocable: false` ⇒ 只给模型、不给用户敲」，**没说在哪过滤**。实现：
   `config.skills` 保留**全部**条目（`expand_invocation` 自己判），**只有下发给 TUI 的那份快照**
   过滤掉不可用户调用的条目。理由是 `ys_tui_coding::SkillEntry` **刻意只有三个显示字段**、
   不携带 `user_invocable`（数据/显示分离，UI 不做准入决策）。若不在 app 侧过滤，不可调用的 skill
   会进候选，用户敲下它时 `expand_invocation` 拒绝展开 ⇒ 整条消息**原样发回模型**（静默降级）。
6. **`load_body` 的有界读是 `max(cap_bytes, 4096) + 4096`** —— 设计只给 `load_body(path, cap_bytes)`，
   没说读多少。实现：`body_read_limit = cap.max(HEAD_BYTES).saturating_add(HEAD_BYTES)`。
   为什么要多留 4 KB：正文起点在 frontmatter 闭合之后，若读的上限恰好等于 `cap_bytes`，
   一份 frontmatter 稍长的**正常**文件就连闭合符都读不到 ⇒ 解析器按「无 frontmatter」处理 ⇒
   frontmatter 原文漏进正文。**有意偏离**：frontmatter **本身**超过 4 KB 的文件（非语料形态）
   会被当作「无 frontmatter」，frontmatter 原文因此进正文并被 `cap_bytes` 截断 —— 这是**有界性
   换来的代价**，与 `truncate_body` 的提示行同属「宁可有损也不无界」的安全阀口径。
   驱动它的是 P1-1：`std::fs::read` 会整份读入再 `from_utf8_lossy` 复制一份（峰值 ≈ 2× 文件大小），
   一个软链/误命名进来的大 `SKILL.md` 能让一次 `/skill` 把 app 线程拖进 GB 级分配。
7. **`-p` / `--json` 下未知 `/xxx` 仍原文发给模型** —— 设计只说「`-p` / `--json` 对 task 调同一个
   `expand_invocation`」，没说未命中怎么办。实现：`expand_task` 在 `None` 时**原样返回 task**，
   于是 `-p "/unknown"` 就是把 `/unknown` 当普通消息发给模型。这与这两个模式的**既有行为一致**
   （它们从来没有本地命令表），但**与 TUI 不同**：TUI 的 `parse_with_skills` 未命中时给「未知命令」
   本地提示（`LocalAction::Hint`），**一个请求都不发**。同一串输入在两种模式下行为不同，如实记。
8. **`parse_frontmatter` 的签名与设计不同** —— 设计：`-> (Option<String>, Option<String>, bool, bool)`；
   实现：`-> Frontmatter`（私有结构体，字段同上 **+ `dropped: Vec<(Key, char)>`**）。
   多出的 `dropped` 是**字段形态非法诊断**的数据来源（`scan` 读它产线），设计要求的诊断必须有它。
9. **设计「已知不支持 ⇒ 遇到即产诊断」收窄为「只有被消费的 4 个 key 的流集合值才诊断」** ——
   设计 §frontmatter 说「锚点/别名、多文档、流集合 —— 遇到即产诊断」。实现只在
   `name` / `description` / `disable-model-invocation` / `user-invocable` 的**值**以裸 `[` / `{`
   开头时产诊断（`Scalar::Flow`）；**被忽略的 key**（`version` / `metadata` / `license` …）里的
   嵌套流集合（如 `bins: ["lark-cli"]`）**静默跳过、不产诊断**（否则实测 56 条语料会被几十行
   假诊断淹没）。锚点/别名与多文档**既不支持也不诊断**（`&` / `*` 会被当普通文本读走）。
10. **新增 `Config::default_skill_roots` 的 `cwd == $HOME` 去重** —— 设计未提。两条根算出同一路径时
    只返回一条，否则 `scan` 会把每个 skill 收两遍（一条 `Project`、一条 `User`、`path` 相同），
    于是**每个** skill 都产一条**假的**「project 级遮蔽 user 级」诊断（实测 56 条语料 ⇒ 56 行噪音），
    恰好淹没 R4 依赖的真信号，启动期 IO 还翻倍。**注意：去重只按路径字符串相等**（见「已知未决」）。
11. **`expand_invocation` 额外要求「首块是 `Text`」** —— 设计未提。由 app 侧的
    `expand_skill_invocation` 施加：首块不是 `Text`（如带图/多块消息）就不展开，只认纯文本首块的
    `/` 命名空间。这是 app 侧的新增守卫，不在 `skills.rs` 里。
12. **架构决策 0019「决策 3」的 key 清单漏了 `user-invocable`** —— ADR 决策 3 写「只认
    `name` / `description` / `disable-model-invocation`」，决策 6 又要求支持 `user-invocable`。
    实现**认 4 个 key**（`name` / `description` / `disable-model-invocation` / `user-invocable`），
    以决策 6 与设计 §接口为准。这是 ADR 行文内部的矛盾，不是实现偏离设计，但记在此处备查。

> **实现与设计的其余部分一致**（此处不是「都一致」的断言，而是「未发现新偏差」）：
> 零新工具 / 零协议变体 / 零新依赖；`Request::Prompt` 复用、回合启动路径不分叉；project > user
> 整条替换、不做字段级合并；`description` 缺失或空 ⇒ 整条不加载；4 种值形态全覆盖（裸标量按第一个
> `:` 切 / 引号去外层 / `>` `>-` 折叠 / `|` 字面块）；不用正则、不引 YAML；系统提示**末尾纯追加**
> （空 catalogue 的输出是任何非空 catalogue 输出的逐字节前缀）；内置命令 > skill；自定义 prompt
> 保留短路 + 一行提示；正文不缓存、每次激活重读；`fixed_prompt_tokens` 默认 0 且加在**比较的输入端**、
> `CompactPolicy::trigger_at` 原样不动；R5（`fixed ≥ trigger`）跳过压缩 + 启动期提示。

## R8 对账结果（2026-09-21 实测，已校准）

用真实凭证（`~/.yushan/auth.json` 的 minimax + `state.json` 的 `MiniMax-M3`）跑
`cargo test -p ys-coding-agent --test e2e_skills -- --ignored --nocapture`：

| 指标 | 值 |
|---|---|
| 段规模（12 条 fixture） | 4,542 B（含 `<available_skills>` wrapper）/ 3,208 chars |
| 真实 `input_tokens`（无段 / 有段） | 520 / 1,785 |
| **真实增量** | **1,265 token** |
| 静态估算（`estimate_text_tokens`，同一段文本） | 1,796 token |
| **真实 / 估算** | **0.704** |

**结论与影响**：

1. **估算器高估约 42%**（真实仅为估算的 0.70 倍）。设计原文假设「CJK ≈ 1 token/字节」过于悲观 ——
   真实 BPE 下 CJK 更接近 **0.7 token/字节**。这条与 `docs/design-final/compaction.md` 的
   「估算器未与真实 `Usage` 对账」互相印证，**本次给出了第一个量化比值**。
2. **`Budget.total_bytes = 32 KB` 无需调整**：56 条语料的 `lines` 实测 21,601 B ⇒ 安全阀仍不触发，
   余量约 10 KB。**结论：默认值保留。**
3. **压缩阈值方向是保守的**：`fixed_prompt_tokens` 用 `estimate_text_tokens` 算（高估 42%）
   ⇒ 触发点被**推前**、压缩比实际需要更早发生。这是**安全方向**（宁可早压，不至于顶出窗口），
   代价是比必要更频繁地压缩。**不改**：把估算器换成真实 usage 需要 provider 先回报（首轮不可知），
   而估算器的保守性恰是既有设计所依赖的性质。
4. **56 条语料的每轮固定成本推算**：`lines` 21,601 B × 0.395 tok/B（本次样本比）≈ **~6.0k 真实 token**
   （设计文档原估 ~9.6k，同为高估方向）。

> 本测量为**单模型单次**（MiniMax-M3，12 条 fixture）。换模型/换语料需重跑 —— `e2e_skills.rs`
> 保留为 `#[ignore]`，随时可复测；它仍是**测量**而非判定门槛。

## 后续追加：`/skills` 命令（2026-09-22）

skills 落地后补的第四个**用户可见面**（此前三个：`/help` 只列名字、`/status` 只给条数、
`/` 浮层一放即走且需按 Tab 才开、且是**前缀过滤**的，不能用来浏览全部）。

- **形态**：`/skills` → `PromptKind::PickSkill` → TUI 模态浮层列全部 skill
  （`/command  — description`，按 `command` 升序、**排除被内置命令遮蔽的** —— 与 `/` 浮层
  共用 `completion::selectable_skills`）。
- **选中不发送**：**回填输入框** `/<command> `（尾随空格），用户补参数后自行回车。
  skill 常带参数（`/git-status --short`），选中即发会把参数路堵死；这也是 `Tab` 补全的既有语义。
- **数据仍是启动期快照**（`CodingView.skills`），**不重扫磁盘** —— 中途新增的 `SKILL.md`
  要重启才可见（与其余三个面一致）。**不显示 path / scope**（`ys_tui_coding::SkillEntry`
  刻意只有三个显示字段），故「这条是谁盖了谁」看不见。
- **零协议改动**：不新增 `Request` 变体（仍 7 个）、不新增工具（仍 9 个）。唯一接口改动在
  `ys-tui-coding` 内部 —— `resolve_prompt` 返回类型由 `Result<Request, _>` 变为
  `Result<PromptOutcome, _>`（`Request` | `Prefill(String)`）。
- **已知形态限制（实测）**：真实语料的 description 是 CJK 长句，选择器宽度被
  `MODAL_MAX_WIDTH` 夹住 ⇒ 每条只显示描述的前一截（与 `/` 补全面同一取舍）。**`/`
  浮层仍是按前缀过滤的那个面**（且要按 Tab 才开），两个面互补而非替代。
- as-built 细节见 [`./coding-agent-tui.md`](./coding-agent-tui.md) §6 表 / §8.2 M10。

## 已知未决 / 遗留

1. ~~**R8 未校准**~~ —— **已校准（2026-09-21）**，见上一节。剩余局限：只测了 MiniMax-M3 与
   12 条 fixture 的单次样本；比值 0.704 的普适性未经多模型交叉验证。
2. **`/help <name>` 不认 skill** —— `/help`（无参）已追加 skill 段（`help_text_with_skills`，
   只列 `/<command>`、**不列 description**），但 `/help <skill>` 走 `command_help_text`，
   **只认内置命令表** ⇒ 对 skill 名给「未知命令」提示。设计 §M3 只要求「`/help` 追加 skill 段」，
   故不算偏离，但这是用户会撞上的行为缺口。**部分缓解**：`/skills`（见上「后续追加」）提供了
   「带 description 浏览全部」的面，但按名查详情（`/help <skill>`）仍未接。
3. **诊断未按 `(command, key)` 去重** —— 同一条 `SKILL.md` 形态错误若在两条 root 上同时出现
   （两条同名 skill 各有一个非法 `description`），`scan_root` 会各产一条措辞相同的诊断。
   今天的形态（两条 root、同名时遮蔽诊断已收敛）下不会明显放大，但去重逻辑不存在。
4. **`default_skill_roots` 的去重只比路径字符串** —— 只处理「`cwd == $HOME` ⇒ 两条根是同一字符串」
   这一情形。**符号链接不覆盖**：若 `~/.yushan/skills` 是软链、或 `<cwd>/.yushan/skills` 指到
   home 下的同一目录而路径字符串不同，两条根都会被扫 ⇒ 每个 skill 收两遍 ⇒ 一串**假的**遮蔽诊断
   （同上第 3 条的形态）。后果**有界**：只是诊断噪音 + 启动期 IO 翻倍，去重语义（project 胜）不受影响。
5. **`SkillEntry.path` 不做 `canonicalize`** —— 有意：那会把 macOS 的 `/var` 解析成 `/private/var`，
   让调用方手里的路径与这里的不再相等。调用方传绝对 roots 即可得绝对路径。副作用是
   `~` 软链场景下同一个 skill 经不同路径进来会**不视为同一个**（与第 4 条同源）。
6. **R6 未缓解** —— 压缩会把刚注入的 skill 正文摘要掉（正文进 session ⇒ 会被摘要），
   长任务里模型可能凭印象行事。设计明确**不引入**「skill 正文免疫压缩」机制；缓解只有
   「名字常驻系统提示 ⇒ 模型可重新 `read`」。
7. **R7 只做声明** —— 正文里约 20 个 Claude 专有工具名在本 runtime **静默失效**，靠目录前言
   一句能力边界声明兜底；**不做符号映射**（有意，映射表会把上游知识写死）。
8. **`frontmatter_block` 的「无闭合 = 无 frontmatter」与有界读叠加** —— 一份 frontmatter 超过
   4 KB 的 `SKILL.md`，扫描期会被当作**没有** frontmatter（⇒ 无 `description` ⇒ 整条不加载），
   激活期会被当作「无 frontmatter」（⇒ frontmatter 原文进正文）。两者都是「有界性换来的代价」，
   非语料形态（见偏差 6）。

## 测试资产

- `apps/coding-agent/src/skills.rs`：**34 条** —— 四种值形态（裸/引号/`>`/`>-`/`|` + 顶格 key 终止续行）、
  `description` 缺失或空 ⇒ 不加载、CRLF/BOM、command=目录名 vs label=name、流集合丢弃 + 诊断、
  有界读（4 KB 边界切在多字节字符中间 + 1 MB 非法 UTF-8 尾巴）、**「闭合符落在 4 KB 之后 ⇒ 不加载」**
  （专门抓「全量读」实现）、project > user 整条替换、`catalogue` **与输入顺序无关**（正/逆序逐字节相同）、
  `model_visible=false` 不进 `lines` 但在 `entries`、预算溢出（全收起 / 部分收起 / 不触发）、
  默认预算 ≥ 实测语料 18,226 B、目录行含 `location`、**XML 转义**（含 `&amp;lt;` 不得出现 + 正文绝不转义）、
  **目录名含换行/制表符/ANSI 转义 ⇒ 丢弃 + 诊断**、静默失败（空根 / 无 `SKILL.md` / 空文件 / 二进制）、
  两个 invocation 标志独立且默认 true、`load_body` 剥 frontmatter + 每次重读 + char 边界截断 + 读失败
  `Err`。**模块内注释列了 6 条实测跑过的变异注入**（去掉 description 守卫 / 对调 `Scope::priority` /
  `BTreeMap` 换保序 `Vec` / 去掉 `escape_xml` 的 `<` / `read_prefix` 换 `fs::read` /
  注释掉目录名检查）。
- `apps/coding-agent/tests/skills_cli.rs`：**3 条**（子进程 + per-child env，**不碰父进程 env**，
  套 30s 超时防挂死）—— `-p` 下遮蔽与非法值两类诊断**进 stderr 且 stdout 为空**；
  `--json` 下 **stdout 逐行纯 JSON 且为空**；无 skills 目录 / 空目录时**零诊断且两次 stderr 逐字节相同**。
- `apps/coding-agent/tests/e2e_skills.rs`：**1 条 `#[ignore]`** —— R8 对账（真实 `usage` 差 vs
  `estimate_text_tokens`，比值打印给人看；provider 不回报 usage 时**只打印「未校准」**、不做断言）。
  **已于 2026-09-21 用真实凭证跑通并回写比值**（见上文「R8 对账结果」）。
- `apps/coding-agent/src/{main,prompt,config,view,app_loop}.rs`：启动提示四源合并 / R5 提示触发的
  两侧边界 / 自定义 prompt 提示的两个必要条件 / `-p` task 展开 / **严格前缀不变量** /
  skill 段内容契约（三句承重前言）/ 门控（无 `read` 或空目录 ⇒ 一字不加）/ `default_skill_roots`
  的顺序与去重 / `load_skills` 尊重 roots 顺序 / `fixed_prompt_tokens` 由 prompt + 工具表派生 /
  `/skill-name` 展开**发生在 `turns += 1` 之前**（首轮 `ModelRequest` 即含正文且**不含 frontmatter**）/
  非 skill 文本**逐字节不变**（5 种情形 + 首块非 `Text` + `user_invocable: false`）/ 透传保序且
  **路径不外泄**（`SkillEntry` 根本没有这个字段）/ **`user_invocable: false` 被过滤**。
- `crates/ys-tui-coding/src/{commands,completion,events,view}.rs`：**内置命令 > skill**（在 `parse`、
  补全、事件层三处各钉一次）/ 命中 skill 发**原文含 args** / `/help` 追加 skill 段 /
  `/status` 显示条数与被收起数 / 补全两段顺序（内置在前、skill 按 `command` 升序）/
  同名 skill 不进补全 / Tab 补全含 skill / `CodingView.skills` 与 `skills_omitted` 的透传。
- `crates/ys-component/src/limits.rs` + `crates/ys-loop/src/basic.rs`：`fixed_prompt_tokens`
  默认 0 且可写；**U1-1 触发点被固定开销推前**（同消息量下 `fixed=0` 不压、`fixed=50` 压，
  经真实 `BasicLoop::run_turn` + `MockModel` 响应队列可见）；**U1-2 R5 边界**
  （`fixed ≥ trigger` ⇒ 跳过压缩，用 `CountingModel` 断言模型**恰好被调用 1 次**）。
- ⚠️ **承重耦合**：`app_loop::every_request_variant_is_handled` 会走到 Step 4b，它安全**只因为**
  R5 早退与阈值判定都在 `ctx.model.complete` **之前**且此处不触发 model 调用
  （`MockModel` 空队列 `remove(0)` 会 panic）。任何让「固定开销 ≥ 触发点」也能进入压缩的改动都会打破它。

## 相关文档

- [`../../arch/skills/design.md`](../../arch/skills/design.md) — 设计（含 §CC 格式兼容性与候选对比）
- [`../../arch/skills/context.md`](../../arch/skills/context.md) — 需求 / 约束 / D1 / D2 / D2′
- [`../../adr/0019-skills.md`](../../adr/0019-skills.md) — 决策记录
- [`../../design-plans/2026-09-21-skills.md`](../../design-plans/2026-09-21-skills.md) — 设计计划（§4 D1–D5 实现前约束）
- [`../../adr/0017-compaction-hardening.md`](../../adr/0017-compaction-hardening.md) — 压缩加固（`fixed_prompt_tokens` 与它耦合）
- [`./compaction.md`](./compaction.md) — 压缩 as-built（R8 的「估算器未对账」在其遗留清单里）
