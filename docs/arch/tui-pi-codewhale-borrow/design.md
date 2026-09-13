# TUI 借鉴：pi 与 CodeWhale 对照

> 立场：**取设计意图，不取实现规模**。起源已定路线是行式 REPL + 不做渲染器
> （`../tui-stack-revisit/design.md`）；本文只回答"哪些设计意图值得借、哪些反例让
> 我们更确信取舍"。
>
> 前置：
> - `docs/arch/tui-stack-revisit/design.md`——行式 REPL 终态设计
> - `docs/arch/tui-stack-revisit/analysis.md`——ratatui vs 行式 REPL 存量分析
> - `docs/architecture.md` §3 ——产品定位"后台 agent + 简洁 TUI"
>
> 输入材料：
> - `tmp/pi/packages/tui/src/`—— 42 个 TS 文件 / 18k 行（pi TUI 全量）
> - `tmp/pi/packages/coding-agent/src/{main,core/skills,core/slash-commands,modes/interactive/interactive-mode}.ts`
> - `tmp/codewhale/crates/tui/src/`—— 808 个 .rs 文件 / 966k 行（CodeWhale TUI 全量；HEAD `06b44cc`）
> - `tmp/codewhale/docs/design/{TUI_DECONSTRUCTION,TIDELINE_RATATUI_TRANSLATION,STATUS_BAR_COLOR_GRAMMAR,SUBAGENT_FOCUS,AUTO_MODE_PARITY,CLAUDE_CODE_PARITY}.md`
> - `apps/coding-agent/src/ui/*`——YuShan 现状基线（1684 行）
>
> 约束（来自 `docs/arch/gap-closure/context.md` 与 `docs/design.md`）：
> - **不做渲染器**（无 `Frame/Buffer/cell diff`）
> - **不做全屏**（无 alt-screen、无 raw mode）
> - **静态组件优先**（无动态插件、热插拔）
> - **后台 agent 定位**：纯 UI 花活（Markdown 渲染、Diff Viewer、Theme、**鼠标**、可配快捷键）已正式砍掉

---

## 1. 体量与姿态

| 维度 | pi | CodeWhale | YuShan（当前） | YuShan（目标） |
|---|---:|---:|---:|---:|
| TUI 实现规模 | 42 文件 / 18k 行 TS | 808 文件 / 966k 行 Rust | 1684 行 Rust | origin 估约 500 行 |
| 形态 | alt-screen | alt-screen | ratatui alt-screen | **行式 REPL（rustyline）** |
| 输入 | 自研 Editor 2.4k 行 | 自研 Composer ~600 行 + UI 2.7k 行 | ratatui events.rs 305 行 | **rustyline**（行编辑/历史/补全白送） |
| Markdown 渲染 | `marked` 替换 + 自定义着色器 ~1k 行 | 自研 tag-parse + 4k 行 renderer | 无 | **无（已砍）** |
| Slash 命令数 | 23 | 96（9 groups） | 10 | 10–15（按 v1 补遗 P6） |
| 主题/多语言 | 单色 / 单英文 | 13 主题 / 15 语言 | 单色 / 单中文 | **单色 / 单中文** |
| 取消路径 | Esc → Loader.AbortSignal | Esc → cancel stack + `1ms` 节拍 | ratatui KeyEvent 路由 | **`tokio::signal::ctrl_c()`** |

CodeWhale 的 tui crate 比 pi 大 53 倍、比我们大 600 倍；**它的存在只用于"它解决过
的问题"，不用于借鉴实现规模**。

---

## 2. 形态对照

```
pi / CodeWhale 共同形态（alt-screen，全屏多面板）：
┌─ topbar ────────────────────────────────────────────────┐
│  [brand]  model · theme · folder · ctx% · clock          │
├──────────────────────────────────────────────────────────┤
│ ▎rail   │  transcript (markdown)                         │
│ RUNS    │                                                 │
│ WHALES  │  user: ...                                      │
│ FLEET   │  assistant: ...                                 │
│         │  [working…]                                     │
├─────────┴────────────────────────────────────────────────┤
│ ╭─ composer (multi-line, paste-handling) ─────── [↑]    │
├──────────────────────────────────────────────────────────┤
│ ⏵ working · esc to interrupt · ↑1.2k ↓8.4k · 1.3s       │
└──────────────────────────────────────────────────────────┘

YuShan 目标形态（origin `tui-stack-revisit/design.md:30-49`）：
  对话（scrollback）   ← assistant/user 行，⏺ / > 前缀
  status ← /status on 时挂单行：model · ↑k↓k · ctx% · ⏱ · cwd
  input  ← rustyline 输入区，Enter 提交，Shift/Alt+Enter 换行
  hint   ← Tab 触发候选 + 输入时 inline 灰字
```

**借鉴判断**：

| 元素 | 决策 | 来源 |
|---|---|---|
| 全屏 / alt-screen / rail / 多面板 | **不借鉴** | origin 已砍 |
| Composer 圆角边框 + 聚焦换色 | **可借鉴**为 line REPL 输入框视觉提示（约 10 行 ANSI） | pi `tui.ts` + CodeWhale `tui/composer_ui.rs` |
| Topbar 状态条 | **借鉴为单行 footer** | origin §3.3 + CodeWhale `tui/infoline.rs` |
| Inline hint（光标后灰字）| **直接借鉴** | rustyline `Hinter` 内置 |
| Markdown 高亮 | **不借鉴** | origin 砍；pi 1k 行 / CodeWhale 4k 行 |
| 持续 alt-screen 刷新 | **不借鉴**——line REPL 用 `\r` 原地覆盖 | — |
| 终端原生 scrollback 作为 transcript | **完全借鉴** | pi `tui-main-screen.ts:247-616` 的"scrollback inflation"教训是反面教材 |

---

## 3. 输入层（Editor / Composer）

| 能力 | pi | CodeWhale | YuShan line REPL |
|---|---|---|---|
| **行编辑核心** | 自研 2.4k 行（grapheme-aware、`Intl.Segmenter`、undo / kill-ring / paste markers） | 自研 ~600 行 + UI 2.7k 行（grapheme cluster `unicode_segmentation`） | **rustyline**（行编辑/历史/补全白送） |
| **多行** | `Shift+Enter` / `Ctrl+J` / `\<Enter>`（优先级可配） | `\n` 在 buffer 里，渲染换行 | **`Shift+Enter` / `Alt+Enter` + kitty 协议探测** |
| **粘贴** | 大粘贴折叠成 `[paste #N +M lines]` 标记 | 同款 | **rustyline bracketed paste**（内置） |
| **图簇感知** | `Intl.Segmenter` 自带 | `unicode_segmentation` | **rustyline 自带** |
| **Tab 补全** | `CombinedAutocompleteProvider` 三层：`/` → `/cmd arg` → path | `slash_menu.rs:30-118` 分 bare-`/`、`/cmd arg`、edit-distance 模糊 | **`CombinedAutocomplete` 思路直接搬** |

**直接搬的触发层形态**（pi `autocomplete.ts:289-378` + CodeWhale `tui/slash_menu.rs:20-118` 一致）：

```
text-before-cursor
  ├─ starts with `/` (无空格)        → SlashCmdCompletion 列表
  ├─ starts with `/cmd `             → SlashArgCompletion(cmd) 列表
  ├─ 触发字符含 `@` / 路径分隔符      → PathCompletion
  └─ 其他                            → None
```

Rust 实现就一个 `enum CompletionKind { SlashCmd, SlashArg(&'static str), Path }`，
由 `Completer::pre_hook` 在 rustyline `readline` 之前读 `line.cursor()` 之前的字符
串得出。

---

## 4. Slash 命令

| 维度 | pi | CodeWhale | YuShan 现状 |
|---|---|---|---|
| 命令数 | 23 | 96（9 groups） | 10 |
| 元数据 | `{name, description, argumentHint}` | `{name, aliases, usage, description, i18n}` | `HelpEntry { name, description, arg_hint }` |
| 触发 | 模糊匹配 | 模糊 + edit-distance ≤ 2 + retired-command hints | 当前未触发（P0 待修） |
| 参数解析 | `splitn(2, char::is_whitespace)` | 同款 | 同款 |
| 注册中心 | `BUILTIN_SLASH_COMMANDS` 数组 | `OnceLock<CommandRegistry>` + 9 groups | `CommandRegistry` trait object |
| 帮助 | 无 `/help`，靠补全 dropdown | `/help [cmd]` 返回 localized | `/help [cmd]` 返回 `HelpEntry` |

**借鉴判断**：

- pi 的"无 `/help`、靠补全"：**不借鉴**——我们保留 `/help`（用户更熟悉），但补全也
  借鉴实现
- CodeWhale 9 groups 分组：**不借鉴**——10 条命令不需要分 group；**等到 ≥ 30 条再分**
- 编辑距离 ≤ 2 降级：**完全借鉴**——`CodeWhale commands/mod.rs:362-476`，约 30 行
- 触发检测分层：**完全借鉴**——见 §3
- 注册中心结构（`OnceLock` + groups）：**不借鉴**——我们 trait object 注册已经够用

`Command` trait / `CommandRegistry` / `CommandContext` / `Prompter` /
`HelpEntry`（`apps/coding-agent/src/commands/{mod,builtin}.rs`）已经覆盖 80% 借
鉴内容；**只补两件**：

1. **补全触发**：把 `ui/events.rs:100-118` 的 `complete_inline` 改成 **rustyline
   `Completer` pre-hook**，并真正连上 prompt 显示（origin §7 P0）
2. **fuzzy 降级**：prefix 零命中时退到 edit-distance ≤ 2

---

## 5. 对话流（transcript）渲染

| 维度 | pi | CodeWhale | YuShan line REPL |
|---|---|---|---|
| 流式增量 | `Markdown` 组件 + `(text, width)` 缓存 | `streaming_thinking.rs:35` 100ms debounce 重新 wrap + `markdown_render.rs:898` | **直接 `print!` 每块 delta 写 stdout** |
| Markdown | `marked` 替换 + 自定义着色器 1k 行 | 自研 tag-parse 4k 行 | **不做**（origin 已砍） |
| 代码块 | 语法高亮（推测 chroma） | syntax 渲染 | **不打高亮**（或做最小款：识别 fenced block 加 `\x1b[36m`，约 10 行） |
| 滚动 | `ScrollView` 完整 viewport | `pager/` 子模块 + `live_transcript` | **终端原生 scrollback** |
| 单元 | typed cell（`User/Assistant/Tool/Reasoning/Diff/...`） | typed history cell | `TranscriptLine` enum 已是 typed |

**借鉴判断**：

- Markdown 高亮：**不做**——成本 1k+ 行，origin 已砍
- 流式增量：`apply_agent_event` 已经做对了，**原样保留**；这是 `ui/mod.rs:359-372`
  的纯函数，单元测试（`ui/mod.rs:582-664`）会迁移到 line REPL 测试里
- 代码块最小着色：**可借鉴**——CodeWhale `tui/markdown_render.rs` 的 fenced-block 检
  测降级为简单正则，10 行；非必需
- "无滚动区"：pi/CodeWhale 都为 alt-screen 重写 viewport；我们**直接利用终端原生
  scrollback**——这是 origin 路线的核心收益

---

## 6. 状态条 / Footer

| 维度 | pi | CodeWhale | YuShan line REPL |
|---|---|---|---|
| 形态 | Footer 组件（always-on） | 两行：top posture + bottom infoline | **单行 dot-chain**（origin `tui-stack-revisit/design.md:122-125`） |
| 颜色语法 | 单色 | **7 family**（Outcome / Cognition / Active / Policy / Identity / Metadata / Failure） | **4 色**（Outcome / Active / Metadata / Failure） |
| 内容 | 模式 + 键位提示 + turn 中 esc | `model · ctx% · $cost · ttft · tok/s · ↑k ↓k` | **`⏳ Working · esc to interrupt · /help · ↑k ↓k · elapsed`** |
| 缩屏剥除顺序 | 不缩屏（alt-screen） | 显式 shed order（`infoline.rs:29-32`） | **按 width 切片字段**（model → tokens → cwd → time） |
| 刷新机制 | 持续渲染 | 持续渲染 | **`\r\x1b[2K` 原地覆盖**（不写换行） |
| 常驻 vs 按需 | 常驻 | 常驻 | **默认关闭，`/status on` 挂载**（origin §3.3） |

**借鉴判断**：

- dot-chain 分隔符（` · `）：**两边都用**——直接采用
- 7 family 降为 4 色：**完全借鉴** CodeWhale 语义骨架
  （`docs/design/STATUS_BAR_COLOR_GRAMMAR.md:21-37`），按我们的 4 色裁剪
- infoline 字段表：**借鉴**字段（`model · ctx% · $cost · ttft · tok/s · ↑k ↓k`），
  shed 算法约 15 行 if-else
- 持续刷新（alt-screen）：**不借鉴**——line REPL 用 `\r` 原地覆盖
- 常驻 footer：**不借鉴**——origin 已定默认关闭

> CodeWhale 的 7 family 语义在我们这里只取 4——任何状态只用一个颜色，**红色只在
> `Failed`**。这条规则值得写入 ADR-0013"chrome 语义"留档。

---

## 7. Thinking 隔离

| 维度 | pi | CodeWhale | YuShan line REPL |
|---|---|---|---|
| 位置 | inline 在 active cell | inline 在 active cell + 100ms debounce wrap | **默认剥离；`/thinking on` 独立行** |
| 默认可见 | 折叠（toggle 控制） | 常显（无 toggle） | **不可见**（与 origin 一致） |
| 切换方式 | `app.thinking.toggle` 键 | 无切换 | **`/thinking on/off`**（origin P4） |
| 与 reply 混行 | 不混行（独立 cell 类型） | 不混行（独立 cell） | **强类型 `TranscriptLine::Thinking(String)`**（v1 补遗 §2） |

**借鉴判断**：

- 独立 cell 类型：**完全借鉴**——thinking **不是** assistant 文本的一部分
- toggle 存在：**借鉴** pi 的 `app.thinking.toggle` 概念，做成 `/thinking on/off`
- 100ms debounce wrap：**不借鉴**——line REPL 直接 print 不需 wrap

---

## 8. Skill 提示

| 维度 | pi | CodeWhale | YuShan line REPL |
|---|---|---|---|
| 数据形态 | `SKILL.md` 文件，frontmatter 校验 `name=^[a-z0-9-]+$` | `crates/skills/` + `SKILL.md` frontmatter | **新 crate `ys-skill`**（v1 补遗 §3） |
| 触发 | 注入 system prompt + `/skill-name` slash | `$name` / `/skill name` / 模糊匹配 slash | **`/skills` 显式 + `@skill` inline**（v1 补遗 §3.3） |
| Tab 候选 | 是（slash 列表） | 是 | **是** |
| 复用 markdown | 是 | 是 | **是**（skill body = markdown） |

**借鉴判断**：

- `SKILL.md` frontmatter 格式：**完全借鉴**——业界事实标准
- 校验 name 正则 `^[a-z0-9-]+$`：**完全借鉴** pi（`tmp/pi/packages/coding-agent/src/core/skills.ts:92-127`）
- 触发：pi 的"注入 system + slash"两条路径**我们都借鉴**，再加一条 inline `@skill`
- 目录约定 `.pi/skills/` / `.codewhale/skills/`：**统一为 `.yushan/skills/`**

---

## 9. Subagent / Modes / Workflow

| 维度 | pi | CodeWhale | YuShan line REPL |
|---|---|---|---|
| Plan / Act / Operate | 无 | 有（`AppMode::Agent/Plan/Operate` + posture chip） | **本轮不做** |
| Subagent UI | 无（agent loop 有，TUI 看不到） | 完整 roster + focus chip + composer target | **本轮不做** |
| Workflow | 无 | JS inline script + replay journal | **本轮不做** |

**借鉴判断**：整层**不做**——超出 v1 范围。

---

## 10. 借鉴决策矩阵（精炼版）

### 必借鉴（落地快、与 origin 不冲突）

| 项 | 来源 | 落点 | 行数 |
|---|---|---|---:|
| **三段补全触发**（`/` → `/cmd arg` → path）| pi `autocomplete.ts:289-378` + CodeWhale `slash_menu.rs:30-118` | `repl_complete.rs::detect_kind()` | ~30 |
| **fuzzy 降级**（edit-distance ≤ 2） | CodeWhale `commands/mod.rs:362-476` | `commands/mod.rs::suggest()` | ~30 |
| **dot-chain 分隔符**（` · `）| 两边共用 | `format.rs::join_chain()` | ~5 |
| **7 family → 4 色裁剪**（Outcome/Active/Metadata/Failure）| CodeWhale `STATUS_BAR_COLOR_GRAMMAR.md:21-37` | `format.rs::PhaseColor` | ~20 |
| **context % 4 段**（Low/Moderate/High/Critical）| CodeWhale `context_budget.rs:42-105` | `view.rs::pressure_level()` | ~15 |
| **infoline 字段表**（model · ctx% · $cost · ttft · tok/s · ↑k↓k）| CodeWhale `infoline.rs:14-21` | `format.rs::print_infoline(width, ...)` | ~20 |
| **thinking 强类型 cell**（不与 reply 混行）| 两边都用 | `TranscriptLine::Thinking(String)` | ~10 |
| **SKILL.md frontmatter**（name 正则 `^[a-z0-9-]+$`）| pi `skills.ts:92-127` | `ys-skill/src/lib.rs`（v1 补遗 §3） | ~25 |

**合计 ≈155 行**。**全部叠加在 origin P1+ 之上**，不破坏既有决策。

### 可借鉴（讨论后再定）

| 项 | 评估 |
|---|---|
| composer 圆角边框 + 聚焦换 Info 色 | 视觉精致，约 30 行；与"轻量"冲突小，**留到 P7（footer）同期**做 |
| 代码块最小着色（fenced block 检测）| CodeWhale `markdown_render.rs` 降级；约 20 行；可做但非必需 |
| Markdown 整体着色 | 1000+ 行 / 外部 crate，**不做**（origin 砍） |
| spinner frames（Braille）| pi 80ms × 10 帧 vs CodeWhale 420ms × 6 帧——我们用纯文本 `⏳ Working… 1.2s`，**不做动画** |
| `@skill` inline 触发 | 借鉴 pi `@file` 的 trigger detection（prefix before cursor），新增 `MentionSkill` 触发层 |
| 历史文件持久化 | rustyline 自带 `~/.yushan/history`，**不做** |

### 不借鉴（明确反例，写进 ADR 留档）

#### CodeWhale 不借鉴

| 做法 | 不做的理由 |
|---|---|
| `App` god struct 6,781 行 | 我们 `Wiring` 372 行已稳 |
| 155 个 sibling 模块 | 我们 4 个 ys-crate + product crate，方向不同 |
| alt-screen ratatui 渲染（`underwater.rs` 3k / `infoline.rs` 442 / `phase_strip.rs` 1.4k / `markdown_render.rs` 4k 行） | 行式 REPL 不渲染，**全砍** |
| `Engine` + `EngineHandle` + `Op/Event` 7,692 行 | 我们 `Agent` + `AgentPorts` + `Inbox` + `AgentEvent` 已等效，**不动** |
| 13 主题 + 17 ink palette crate | 4 色 enum 够用，**不做 palette crate** |
| 15 语言 i18n + 几百 MessageId | 单中文，**不做** |
| `Operate` mode 2,111 行 fleet runtime | **不做**（与 origin v1 范围外） |
| `Markdown` 渲染器 4k 行 | **不做**（origin 已砍） |
| `mention_completion.rs` 异步文件 walker（`fd` shelled out）| **不做**（不做 `@file`） |
| `cost_status.rs` side-channel 成本累加 | 单个 `TurnStats` 累加就够，**不做** |
| `plugin` / `marketplace` | **不做**（后续阶段） |
| `AppMode` Plan/Operate | **不做**（后续阶段） |
| Subagent roster / `agent_focus` chip | **不做**（后续阶段） |
| `tui/composer_history.rs` 写入线程 | rustyline 自带文件历史，**不做** |

#### pi 不借鉴

| 做法 | 不做的理由 |
|---|---|
| 自研 2.4k 行 Editor | rustyline 替代 |
| `marked` 替换 + 自定义着色 | 不做 markdown |
| `intl.Segmenter` 全 grapheme | rustyline `unicode-segmentation` 已覆盖 |
| Container 组件树 + differential renderer（1.7k 行）| 不渲染 |
| Kitty / iTerm2 / OSC 9;4 / OSC 11 探测 | 不做 alt-screen，不需要 |
| `SettingsList` / `SelectList` / `Box` / `Text` 全套 widget | 不渲染 |
| KillRing / UndoStack / WordNavigation 自研 | rustyline 已给 |
| NAPI native bridge（clipboard）| 我们做最小 crate，不引入 native |
| `TuiMainScreen` scrollback inflation（250 行微妙逻辑）| "print-and-fall-through-to-scrollback" 解决 |

---

## 11. 实施分期（在 origin P0–P4 与 v1 补遗 P5–P8 之上）

| 阶段 | 内容 | 来源 | 行数估算 |
|---|---|---|---:|
| P0 | 修两个真 bug：中文退格 + 补全 popup 不渲染 | origin | — |
| P1 | 去 alt-screen 改行式：`ui/` → `repl.rs`，对话进 scrollback，命令直接跑 | origin | ~230 |
| P2 | 键盘协议 + `Shift/Alt+Enter` 多行 + 历史 + 粘贴 | origin | ~80 |
| P3 | 补全候选（含 `arg_hint`）+ inline 提示 | origin | ~50 |
| P4 | think 标签剥离 + `/thinking` 开关 | origin | ~30 |
| P5 | thinking 强类型 cell（`TranscriptLine::Thinking`） | v1 补遗 §2 | ~10 |
| P6 | `/skills` 命令 + `@skill` inline 触发 + fuzzy 降级 | v1 补遗 §3 + 本分析 §10 必借鉴.1,2 | ~95 |
| P7 | footer + 4 色 + dot-chain + infoline + shed | v1 补遗 §4 + 本分析 §10 必借鉴.3–6 | ~95 |
| **P8（新增）** | **SKILL.md frontmatter + name 正则校验** | 本分析 §10 必借鉴.8 | ~25 |
| **P9（可选）** | composer 圆角边框 + 代码块最小着色 | 本分析 §10 可借鉴 | ~50 |

P5–P8 全部与 origin 行式 REPL 主线正交，**任何阶段延后/取消不影响总体稳定**。

---

## 12. 立即可拿走的 3 件事

1. **三段补全触发**：写一个 30 行纯函数 `detect_completion_kind(line: &str) -> CompletionKind`——一行测试覆盖三种 case。
2. **`PhaseColor` enum**：4 色集中映射，红色只在 `Failed`。`format.rs` + 一个测试。
3. **dot-chain footer 模板**：
   ```
   turn:  ⏳ <phase_name> · esc to interrupt · /help · ↑<in>k ↓<out>k · <elapsed>s
   idle:  ❯ /help · ↑↓ history · Esc to clear · Tab to complete
   ```
   单一字符串拼接函数（10 行），turn 与 idle 共用模板。

三件事总计 ≈ 60 行 + 1 个 ADR-0013 候选稿。与 origin P1+ 之后的任何阶段正交，
独立可合并。

---

## 13. ADR 候选稿（chrome 语义）

提议写 `docs/adr/0013-tui-chrome-grammar.md`，把以下三条留档：

1. **状态只 4 色**（绿/青/灰/红）；红只在 `✗ failed`
2. **状态语义和颜色一一映射**——杜绝"状态用黄色装饰"之类的漂移
3. **footer 永远在最后一行**——对话区在它上方；turn 输出写到对话区，绝不写 footer
4. **footer 用 `\r\x1b[2K` 原地覆盖**——绝不写换行
5. **窄屏剥除有顺序**——不是 "`flex` 自动折叠"那种模糊语义；明确 model → tokens → cwd → time

外加 §10 不借鉴反例的两张表，注明"为什么这里没有"，避免后人重复踩坑。

---

## 14. 待办（不在本轮范围）

- **v2 候选**：`@file` mention + 异步 fs 索引（贴文件进 prompt）
- **v2 候选**：`/loop` 调度 + `CronCreate`（长任务）
- **v2 候选**：`/workflow` inline script + journal 回放（sub-agent fleet）
- **v2 候选**：plugin marketplace
- **v2 候选**：`AppMode` 三态（Plan /Act / Operate）
- **v2 候选**：subagent roster + focus chip

---

## 附录 A：关键源文件路径

### pi

- TUI 入口：`tmp/pi/packages/tui/src/{index,tui,tui-main-screen,tui-alt-screen}.ts`
- 输入：`tmp/pi/packages/tui/src/{components/editor,autocomplete,keybindings,terminal}.ts`
- 组件：`tmp/pi/packages/tui/src/components/{markdown,loader,cancellable-loader,select-list,box,text,scroll-view}.ts`
- Slash / Skill：`tmp/pi/packages/coding-agent/src/core/{slash-commands,skills,system-prompt}.ts`
- Interactive 装配：`tmp/pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts`

### CodeWhale

- TUI/Engine 边界：`tmp/codewhale/crates/tui/src/{lib.rs,tui/mod.rs,core/mod.rs}`
- App god struct：`tmp/codewhale/crates/tui/src/tui/app.rs`（6,781 行）
- 输入：`tmp/codewhale/crates/tui/src/tui/{app/composer.rs,composer_ui.rs,slash_menu.rs,mention_completion.rs,user_input.rs}`
- 渲染：`tmp/codewhale/crates/tui/src/tui/{markdown_render.rs,streaming_thinking.rs,infoline.rs,phase_strip.rs,footer_ui.rs,underwater.rs}`
- Slash 命令：`tmp/codewhale/crates/tui/src/commands/{mod.rs,traits.rs,groups/*}`
- 上下文/成本：`tmp/codewhale/crates/tui/src/{context_budget.rs,cost_status.rs}`
- 拆解/重设计：`tmp/codewhale/docs/design/{TUI_DECONSTRUCTION,TIDELINE_RATATUI_TRANSLATION,STATUS_BAR_COLOR_GRAMMAR}.md`

### YuShan 现状

- 入口：`apps/coding-agent/src/ui/{mod.rs,app.rs,draw.rs,events.rs,completion.rs}`
- 接线器：`apps/coding-agent/src/{wiring.rs,channel.rs}`
- 命令：`apps/coding-agent/src/commands/{mod.rs,builtin.rs}`
- 显示快照：`apps/coding-agent/src/{view.rs,format.rs,status.rs}`
- 设计：`docs/arch/tui-stack-revisit/{analysis.md,design.md}`