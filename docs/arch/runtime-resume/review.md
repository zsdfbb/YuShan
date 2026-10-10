# runtime-resume — 架构质量分析报告

## 分析范围

- **对象**：设计方案 `docs/arch/runtime-resume/design.md` + 决策 `docs/adr/0033-runtime-resume-command.md`
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **来源**：design.md + context.md + 三份候选 + **直接读码交叉核对**（`apps/coding-agent` / `crates/ys-tui-coding` / `crates/ys-protocol` / `crates/ys-session`）
- **方法**：两轮**独立对抗性复核**（一轮攻可行性/错误处理/并发/资源，一轮攻可理解性/性能/否决理由），
  再与设计自述交叉验证；每条标注**核实**（读了代码）或**推断**
- **日期**：2026-10-10

> 立场：对抗式审查。**P1 项已在开工前回修进 `design.md`**（见下表现状）。
> 结论先行：**核心决策站得住**（协议加变体、一次性载荷走 `Outbound`、失败原子性、`RenderCache` 显式失效、
> 启动期与运行期同源）；但设计**自称的「失败不改状态」在最要紧处写错了顺序** ——
> 它把 `resume_into`（已替换会话）排在模型解析之前，与同文档的不变量 I1 互斥。
> 这类缺陷恰好属于本项目最警惕的「**测试全绿时依然存在**」一类。

---

## 处置状态（已回修进 `design.md` / `adr-0033`）

| # | 处置 |
|---|---|
| **T1** | ✅ 修入 design **§3.3**（新增「恢复的步骤顺序：可失败步全部前置」）+ I1 重写 + ADR 决策 2 |
| **T2** | ✅ 修入 design **§3.4 / I9** + ADR 决策 4：app 侧 `thinking_visible` 镜像必须随 resume 重设 |
| **T3** | ✅ 修入 design **I6**：显式 `reset()`；并点明 `sync_len` 增长分支会绕过缩短分支 |
| **T4** | ✅ 修入 design **§5 测试要点**：I1 变异改为「模型解析挪到替换之后」 |
| **T5** | ✅ 修入 design **I7**：度量指标改为「reset 后**首帧** md 渲染」，给 100 ms 阈值 |
| **T6** | ✅ 修入 design **§3.5 / §2.3**：改为「**镜像** `apply_event` 的输出约定」+ 跨路径等价单测 |
| **T7** | ✅ 修入 design **§3.5**：新增 `resolve_session_pick`（**不改** `resolve_prompt` 签名）+ 标签含 **id 短码**以便反解 |
| **T8** | ✅ 修入 design **I8**：抽 `reset_for_replay()`，穷举同源状态 |
| T9 | ✅ 修入 design §3.1：**删除** `Listing`，改用 `ys_protocol::SessionSummary` |
| T10 | ✅ 修入 design §1/§3.2 + ADR：统一叫 **resume**，不造「激活」 |
| T11 | ✅ 修入 design **I10**：启动期 `SessionResumed` 必须排在 `startup_hint` 之后 |
| T12 | ✅ 修入 design **I11** + ADR 决策 12：空用户行与「看着像成功」的悬空工具 |
| T13 | ✅ 修入 design §7：`pending_sessions` 与 `pending_prompt` 同 take 同清；后到覆盖 |
| T14 | ✅ 修入 design **I13**：`sessions_dir == None` ⇒ 提示不可恢复，不弹浮层 |
| T15 | ✅ 修入 design §3.4：`spawn_blocking` 或照 `capabilities::export` 先例写上界注释 |
| T16 | ✅ 修入 design §3.4：沿既有「返回文本行」形态，但不得吞掉 `SessionError` 的分类 |
| T17 | ✅ 修入 design §3.5：检查点三处（主循环 poll 后、排空 Outbound 后、模态内 drain） |
| T18 | ✅ 记入 design §7 遗留（TOCTOU：被**改写**而非删除） |
| T19 | ✅ 修入 design §2.3：改判据为**生命周期**（长期快照 vs 一次性载荷）；引用错误订正 |
| T20 | ✅ 修入 design Q6 + ADR 决策 4：明写为**有意的破例**，不说「不破不变量」 |
| T21 | ✅ 修入 design §2.3 + ADR 决策 7：补「本仓测试用真目录、不 mock」这条更强理由 |
| T22 | ✅ 修入 design §3.2：保留 `SystemTime`，补一句类型自证的理由 |
| T23 | ✅ 修入 design §3.2：`SessionActivated` → `SessionResumed` |
| T24 | ✅ 记入 design §3.4（同步 IO 先例清单） |

> 未采纳：无（T1–T24 全部采纳；其中 T19/T20 改写了论证方式，T23 改了命名，T7 增了接口）。

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|---|---|---|
| 1.1 技术可实现性 | 🟢 | 纯本地文件 + 既有 `JsonlSession`/`serde`；机制全在 pi/Codewhale 有先例；无黑科技、无新依赖 |
| 1.2 依赖成熟度 | 🟢 | **零新增依赖** |
| 1.3 实现周期 | 🟡 | 增量路径清晰（4 片），但三处**易错点**都已确认在真实代码里成立：顺序（T1）、prompter 入口签名（T7）、缓存失效时机（T3） |
| 2.1 模块边界 | 🟢 | `ys-tui-coding` 仍不依赖 `ys-runtime`；新增数据类型满足 `ys-protocol` 零 tokio；`-p`/`--json` 不被牵动（已核实 `main.rs:802/838/878`） |
| 2.2 接口稳定性 | 🟡 | `Request` 8→10、`Outbound` 4→6（新增变体是加法，旧 JSON 仍可解析 ✅）；但**删除 `sessions::Listing`** 属破坏性改名（仅内部，可接受） |
| 2.3 错误处理 | 🟡 | **T1 是本报告最重的一条**；另有 T14（内存会话未定义）、T16（`Result<_, String>` 抹平 `SessionError` 类别）、T18（TOCTOU 改写） |
| 2.4 并发安全 | 🟢 | 无新增锁/共享可变状态；有界信道无新增死锁（已核实 `send().await` 与 UI 每 50 ms 排空并发）；T2/T17 属状态同步而非竞态 |
| 2.5 资源管理 | 🟡 | 回放峰值约 **3×**（`session.messages` + `history` clone + `TranscriptLine` 产物），带大工具结果的会话可达数十 MB —— 设计**未估**；T8 的重置清单是资源纪律问题 |
| 3.1 概念一致性 | 🟡 | T10（「激活」vs 代码 `resume_*`）、T23（`List`/`Activated` 不对称）、T20（把破例说成不变量） |
| 3.2 上手成本 | 🟡 | T7（最易做错：改 `resolve_prompt` 签名会波及 4 个 prompt 臂 + 全部 `FakePrompter` 单测）、T17、T3 |
| 4.1 性能模型 | 🟡 | T5：**指标量错了** —— 原 I7 量的是既存的稳态成本，量了也白量 |
| 4.2 故障模式 | 🟡 | T12（回放渲染：悬空工具「看着像成功」、`ToolResult`-only 会出**空用户行**）、T18、T14 |
| 4.3 可观测性 | 🟢 | 恢复通知走 `Outbound::Output` → transcript，与既有命令一致；会话路径快照刷新覆盖退出提示 |

---

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|---|---|---|---|---|
| **T1** | `resume_into` 排在模型解析之前 ⇒ 模型解析失败留下「会话已换、模型还是旧的」半切换；与 I1 互斥 | 高 | 中-高 | **P1** | 可失败步全部前置、提交步不可失败（design §3.3） |
| **T2** | app 侧 `thinking_visible` 镜像（`app_loop.rs:147`）不随 resume 重设 ⇒ 下一回合用**旧会话**的值覆写**新会话 meta** | 高 | 中 | **P1** | 提交步一并重设（I9）+ 变异测试 |
| **T3** | I6 的验证草图**抓不到**假命中（键含 `src.len()`，长度不等就天然 miss ⇒ 删掉 `reset()` 也照绿）；且 `sync_len` 增长分支会绕过缩短分支 | 中 | 高 | **P1** | 显式 `reset()`；测试构造**等长碰撞** |
| **T5** | I7 量错指标 ⇒ 实测会结论「没事」，漏掉 `reset()` 后首帧的真实卡顿 | 中 | 高 | **P1** | 改为量**首帧** md 渲染，阈值 100 ms |
| **T7** | PickSession 若沿用 `resolve_prompt(&CodingView)` 则签名装不下；标签不可反解会让**同标题会话切错** | 中 | 高 | **P1** | 新增 `resolve_session_pick` + 标签含 id 短码 |
| **T6** | 「与 `apply_event` 共用映射」与事实不符 ⇒ 实现时变成两套并行映射，正是它想避免的漂移 | 中 | 高 | **P1** | 改口为「镜像」+ 跨路径等价单测 |
| **T8** | 回放只清 `md_cache`，漏 `tool_index`/`pending_echo_texts` 等同源状态 | 中 | 中 | **P1** | 抽 `reset_for_replay()`，逐项断言 |
| **T4** | I1 的变异只覆盖 `open` 失败，抓不到 T1 的半切换 | 中 | 中 | **P1** | 补「模型解析挪到替换之后」的变异 |
| T12 | 悬空 `ToolUse` 渲染成「✓」看着像成功；`ToolResult`-only 的 User 消息渲染成空行（且这是**常态**） | 中 | 中 | P2 | I11：无配对置 `success=false`；只含工具结果的消息不画 User 行 |
| T11 | 启动期 `SessionResumed` 的 `clear()` 会吃掉先入队的启动提示（`main.rs:1017-1021`） | 低-中 | 中 | P2 | I10：启动期顺序 |
| T9 | `Listing` 与 `SessionSummary` 两个同义类型并存 | 低 | 高（必发生） | P2 | 删除 `Listing` |
| T13 | `pending_sessions` 何时清空未定义 ⇒ 残留旧列表 | 低 | 中 | P2 | 与 `pending_prompt` 同 take 同清 |
| T15 | `sessions::list` 是同步 O(N) 次 open，跑在 async app 线程 | 低 | 中 | P2 | `spawn_blocking` 或照 `export` 先例写上界注释 |
| T14 | `sessions_dir == None` 未定义 | 低 | 低（生产恒 `Some`） | P2 | I13 |
| T16 | `Result<_, String>` 是新形态且抹平 `SessionError` 分类 | 低 | 中 | P2 | 沿既有「返回文本行」，但保留错误类别 |
| T18 | 列表读完到加载之间目标被**改写**（非删除）无校验 | 低 | 低 | P2 | 记入遗留 |
| T10/T23 | 术语「激活」与命名 `List`/`Activated` 不对称 | 低 | 中 | P2 | 统一 resume / `SessionResumed` |
| T20 | 把「破例改写 UI 运行期真值」说成「不破不变量」 | 低 | 中 | P2 | 明写为有意的破例 |
| T21/T22 | 否决理由与类型选择未给最强依据 | 低 | 低 | P3 | 已在 §2.3/§3.2 补 |
| T24 | 同步 IO 先例未列举 | 低 | 低 | P3 | 已在 §3.4 补 |

---

## 关键发现（含证据）

### T1（P1）「失败不改状态」在最要紧处写错了顺序

设计同时说了两件互斥的事：

- §3.3 的调用序：`resolve → resume_into（open + 替换会话）→ resolve_model_from_meta（改 config）→ stats`；
- I1 的承诺：**「模型解析任一步失败 ⇒ 会话 / transcript / 模型全不变」**。

`resume_into` 一旦执行就已替换 `self.session`，此后的模型解析再失败，「当前会话」**已经变了** ⇒ I1 为假。

更关键的是它与**启动期真实顺序相反**：

```
启动期： resolve_resume（含 read_meta）main.rs:678
      → 模型优先级链           main.rs:1104-1152
      → Wiring::resume         main.rs:966        ← open 在最后
```

而 design §3.4 自称「与启动期同一条优先级链，杜绝漂移」—— **顺序本身已分叉**。
三个候选原文（`candidates/c-performance.md:54-55`、`b-extensible.md:202`）也都是这个错序，
说明这是**承重设计**而非笔误。

**修法**：把可失败步全部前置，提交步不可失败（已修入 §3.3）。

### T2（P1）thinking 有**三份**真值，只对齐了其中两份

- `App.thinking_visible`（UI 运行期真值）
- `app_loop.rs:147` 的**进程级镜像** —— 每回合末**无条件**写穿：
  `finalize_turn_meta(..., Some(ctx.thinking_visible), ...)`（`app_loop.rs:219`）
- 会话 `meta.thinking_visible`

Q6 只说了「跟随 meta」并让 `SessionResumed` 改 UI 那份；§3.4 的 app 侧改动清单**没有**这一项。
⇒ 若 `T_old != T_target`，resume 后**第一个回合**就把 `T_old` 写进**新会话的 meta** —— 与 Q6 直接相悖。
且这条**没有任何测试**覆盖。

（同类：session-default-new 的 review T7 曾因「两份真值漂移」改过设计，此处是同一坑的第三次出现。）

### T3（P1）I6 的测试设计会**假绿**，且失效时机有陷阱

`crates/ys-tui-coding/src/cache.rs:114-124` 的槽位键 = `(槽下标, src.len(), width)`。
I6 原草图的断言是「切到 B 后屏上不出现 A 的文本」—— 但新旧会话对应槽位的**字节长度通常不等**，
键天然 miss ⇒ **即使删掉 `reset()` 也照样绿**。

更细的一层：`sync_len` 有「增长」（`cache.rs:61-67`）与「缩短」（`:68-73`）两条分支。
若 `clear()` 与回放的 `extend` 在**同一帧前**完成，长度看起来没变，会走**增长**分支而**绕过**缩短分支
⇒ 直接假命中。⇒ 不能指望 `sync_len` 自动兜住，必须**显式 `reset()`**。

**修法**：测试构造两条**同字节长度、同 width** 的 assistant 文本。

### T5（P1）I7 量错了指标

原 I7 写「量 500 条 transcript 的每帧 `line_count` 成本」。但：

- `line_count`/`build_text` 的 O(transcript) 是**固有**成本 —— `run.rs:160`（帧）与 `run.rs:201-203`（滚动夹取）
  每轮各一次，节拍 50 ms（`run.rs:59`）⇒ 空闲也 20 fps 常驻；**任何长会话今天就在付**（推断，与 resume 无关）。
- resume **新增**的那笔是 `reset()` 之后**首帧的全量 `md::render`**（每条 assistant 重解析）——
  正是候选 C 自评「最不确定项 5–50 ms」的那一项。

⇒ 按原 I7 实测，结论必然是「没事」，而真正的首帧卡顿被漏掉。

### T7（P1）选择器入口：签名装不下，且有**切错会话**的风险

- `resolve_prompt(kind, prompter, &CodingView)`（`prompter.rs:86`）**只看 view**；
  而 `PickSession` 的候选在 `App.pending_sessions`。新人极可能去改 `resolve_prompt` 的签名 ——
  那会波及 Login/PickModel/PickSkill 三臂与全部 `FakePrompter` 单测（`prompter.rs:292-566`）。
- `Prompter::select` 只回传**显示串**（`prompter.rs:52-59`）；`PickSkill` 靠 label 反解（`prompter.rs:136-147`）。
  ⇒ 会话标签**必须含可反解的 id**，否则两个同名会话（默认标题都是「新会话」！）会**切错**。
  这一点在 YuShan 尤其危险：`derive_title` 的缺省就是「新会话」。

### T12（P2）回放的渲染边界（设计完全没覆盖）

- **`ToolResult`-only 的 User 消息是常态**（`sessions.rs:440-449` 的测试证明）。`apply_event` 里
  `message_text` 只取 `Text` 块（`app.rs:359-367`）⇒ 直接复用会画出**空用户行**。
- **无配对的 `ToolUse`**（被中断的会话）：`apply_event` 的 ToolCall 分支默认 `success: true, result: None`
  （`app.rs:172-178`），失败才显结果（`draw.rs:549`）⇒ 回放会渲染成「⏺ name ✓」，**看着像成功**。
  这是「静默误导」，比崩溃更糟。

---

## 改进建议

### 易修复（低风险快速改进）

- **删除 `sessions::Listing`**，直接 `use ys_protocol::SessionSummary`（消两个同义类型）。
- 统一术语为 **resume**，`SessionActivated` → `SessionResumed`。
- 补 `sessions_dir == None` 的分支语义；补 `pending_sessions` 的清理时机。
- 文档引用订正：`thinking_visible` 的「启动初值」文档在 `view.rs:69-73`（原引 `:120`/`:124` 是相邻其他字段）。

### 需讨论（需要决策）

- **`Result<_, String>` 是否引入**：既有 6 个 `capabilities::*` **没有一个返回 `Result`**，失败也是文本行。
  要维持形态一致，还是借这次把能力层错误类型化？（`sessions::resolve` 已是 `Result<_, String>`。）
- **`list()` 的同步 IO**：包 `spawn_blocking`（全仓零先例）还是照 `capabilities::export` 的先例
  写一条上界论证注释？两者都能接受，但要对齐既有风格。
- **回放峰值内存**（约 3×）是否需要给一个显式上界，还是先记入遗留。

### 架构级（影响面大，需要跨 phase 规划）

- **`SessionMeta` 缺 cwd 字段** ⇒ 不同项目的会话混在一个列表里（pi/Codewhale 都有 workspace 分组）。
  这不是 resume 引入的，但**是本需求的 picker 把它变成了用户可见问题**。建议单独立项。
- **回合进行中拿不到列表**（app 线程阻塞在 `run_turn`）：若将来要「浮层立刻弹」，
  需要把列表服务拆成独立任务 —— 那是对本仓「单 app 循环」结构的改动。

---

## 总体评价

**整体健康度：良好，但有一处必须在开工前改掉的硬伤。**

站得住的部分：协议切分（查询/变更分开、一次性载荷走 `Outbound` 而非 `CodingView`）、
「恢复既有会话」抽成启动期与运行期共用能力、`RenderCache` 必须显式失效这一条机制性发现、
以及 `-p`/`--json` 不受影响的判断 —— 这些都经过代码核实。

最大风险点是 **T1**：设计把「失败原子性」当作核心承诺写进了 I1，
却在 §3.3 里给出了一个与之互斥的调用顺序，且这个错序**三个候选原文都是如此**（不是笔误，是共识性错误）。
它的危害形态正是本项目最警惕的 —— 编译通过、测试全绿、只在「模型解析失败」这条罕见分支上留下半切换状态。
**同类的问题还有 T2（thinking 镜像）与 T3（测试假绿）**：三者共性是「不变量写在了文档里，但没有机制或测试使它不可违反」。

**推荐下一步**：先把 T1/T2/T3/T4/T5 五条落实为**可失败的测试**（尤其 T3 的等长碰撞与 T4 的新变异），
再进入切片 1 的实现。顺序不变式（T1）应当**以测试形式钉住**，而不是只写在文档里。
