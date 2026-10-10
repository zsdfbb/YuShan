# 运行期 `/resume`（切换到指定 session）— 设计

> 产出：`/arch-design`。上游：`./context.md`（`/arch-explore`）。
> 质量分析：`./review.md`（`/arch-validate`）—— **本文已按其 P1 项回修**（见 review 的处置状态表）。
> 决策记录：`docs/adr/0033-runtime-resume-command.md`。
> 候选方案原文：`./candidates/a-minimal.md`、`./candidates/b-extensible.md`、`./candidates/c-performance.md`。

---

## 1. 结论

**在 `ys-protocol` 加两个请求、两个出站变体；把「恢复一个既有会话」抽成启动期 `--resume` 与运行期 `/resume` 共用的 app 侧能力；UI 侧只做「清屏 + 由 `Vec<Message>` 重建 transcript」。**

一句话的数据流：

```
/resume ──► Request::ListSessions ──► app 扫目录 ──► Outbound::SessionList
       ──► 浮层选择（Esc 取消）──► Request::ResumeSession{target}
       ──► app: 全部可失败步前置 → 换 Wiring.session + 跟随 meta 的模型/thinking + 播种统计
       ──► Outbound::SessionResumed{summary, thinking_visible, history}
       ──► UI: reset_for_replay() + 回放 + 退出提示快照刷新
```

**同时修掉启动期 `--resume` 的空屏**（`App::new` 的 transcript 恒空）—— 同一通路、同一约定。

**这不是「多一个命令」，而是推翻 ADR-0026 决策 6**（「不做运行期 `/resume`」）—— 见 §6 与 ADR-0033。

**术语**：本需求的动作一律叫 **resume（恢复既有会话）**，不另造「激活」。`resume ≠ /new`：
`/new` 是**同一会话谱系**里开下一段；resume 是**换到另一条谱系**，与「重启后用 `--resume` 打开它」同构。

---

## 2. 候选对比

### 2.1 三个候选的一句话

| 候选 | 骨架 | 出处 |
|---|---|---|
| **A 最小复杂度** | 列表与回放都做成 `CodingView` 上的**一次性载荷字段**；协议只加 1 个 `Request::Resume{Option<String>}`，`Outbound` 一字不动，`Listing` 不搬迁 | `candidates/a-minimal.md` |
| **B 可扩展优先** | 「恢复会话」抽成**共用能力路径**；列表与历史经 `ys-protocol` **纯数据**过；复活 `SessionCatalog`/`SessionPicker` 为具名接缝 | `candidates/b-extensible.md` |
| **C 性能优先** | **扫描有界（top-K）+ 回放有界（尾 N 条 + 折叠）**；转换推给 app 线程，把每帧渲染成本钉死 | `candidates/c-performance.md` |

### 2.2 对比矩阵

| 维度 | A 最小复杂度 | B 可扩展优先 | C 性能优先 |
|---|---|---|---|
| **实现复杂度** | **最低**：无新 `Outbound` 变体、无新协议类型；但把「消息」塞进「快照」类型，类型语义变浑 | 中：新增 `session.rs` 纯数据类型 + 2 个 `Outbound` 变体 + 1 个 trait；**不改 `-p`/`--json`** | 中高：同 B 再加 top-K 扫描与有界回放 + `RenderCache` 失效 |
| **性能特征** | 全量回放：`md_cache.reset()` 后首帧要重渲染全部 assistant 行（C 估 5–50 ms，**最不确定项**）；此后稳态每帧 `line_count` 成本是**既有**量级，非本需求引入 | 同 A | 首帧成本随 K 恒定；冷启动多一次 `stat` 全扫 |
| **演进灵活性** | 低：`Resume{Option}` 把查询与变更混装（A 自评「将来大概率要拆」）；一次性字段会不断往 `CodingView` 上加 | **高**：接缝明确、扩展点列了预期消费者；但 `TitlePolicy` 仍属投机 | 中：为「有界」写死的 K 会成为新常量要调 |
| **运维负担** | 低 | 低 | 低；但「只回放尾 N 条」会引出「我的历史呢」的支持问题 |
| **风险** | `View` 当数据通道的隐晦性；数据驱动模态改控制流 | 单实现 trait 被判投机（静态组合优先的项目尤其敏感） | 有界回放与 D3 的「切过去=打开该会话」语义**冲突** |

### 2.3 逐项裁决

| 议题 | A | B | C | **裁决** | 理由 |
|---|---|---|---|---|---|
| 列表/回放的载荷载体 | `CodingView` 字段 | `Outbound` 变体 | `Outbound` 变体 | **`Outbound` 变体** | 判据不是「`CodingView` 不装列表」——它本就装三张（`providers`/`available_models`/`skills`，`crates/ys-tui-coding/src/view.rs:50/52/62`）。真正的判据是**生命周期**：那三张是**长期有效**的快照（每请求重建、`app.rs:110` 整体替换也无妨），而 session list / replay 是**一次性、消费即弃**的载荷 —— 塞进快照会被每回合重发。另：新增 `Outbound` 变体的真实成本比想象小（`-p`/`--json` **不消费 `Outbound`**，直收 `Receiver<Envelope>`，`main.rs:802/838/878`） |
| 回放载荷类型 | `Vec<TranscriptLine>` | `Vec<Message>` | `Vec<ReplayLine>` | **`Vec<Message>`** | 显示知识归 UI；`Message` 已是 `ys-core` 的 serde 纯数据，无需新类型 |
| 回放范围 | 全量 | 全量 | 尾 N 条 + 折叠 | **全量，但把「有界」立为**可度量的**预案** | D3 要的是「切过去就等于打开那个会话」；见不变量 I7（**已按复核改正度量指标**） |
| 会话列表范围 | 全量 | 全量 | top-K | **全量** | 生态级 issue 的根因就是 picker 静默过滤/截断；`list()` 本就是 `stat` + 首行 4 KiB 封顶 |
| `SessionCatalog` trait | 不收 | 收窄复活 | 不收 | **不收** | trait 服务**多实现**，不是多消费者；本仓测试**用真临时目录而非 mock**（`sessions.rs:228-238`），trait 的唯一潜在收益在本项目不存在 |
| `Request` 形态 | `Resume{Option}` | 拆两个 | 拆两个 | **拆两个** | 「查询」与「变更」混装，A 自评将来要拆；现在就拆的成本≈0 |
| Q6 `thinking_visible` | 不跟随 | 跟随 | 跟随 | **跟随** | 见 §4 Q6 —— 这是一次**有意的破例**，不是「不破不变量」 |
| Q3 `/resume <id>` | 支持 | 支持 | 不做 | **支持** | 零额外成本，与 `/model` 的双形态对称 |
| Q8 排队期反馈 | 静默 | 静默 | 静默 | **静默** | 与 `/model` 一致，零新增机制 |

**被吸收的具体发现**：C 的 `RenderCache` 失效（→ I6）、A 的出站顺序（→ I2）与检查点扩宽（→ §3.5）、
C 的「每轮 2×`line_count`」实测依据（`run.rs:160` + `run.rs:201-203`，→ I7）。

---

## 3. 推荐方案

### 3.1 模块划分与职责

| 层 | 职责 | 不做什么 |
|---|---|---|
| `ys-protocol` | 两个新 `Request`、两个新 `Outbound`、纯数据类型 `SessionSummary` | 不认识任何 TUI 类型；零 tokio |
| `apps/coding-agent` | 扫目录、解析目标、**原子恢复** `Wiring`、跟随 meta 重建模型与 thinking、播种统计、刷新会话路径快照 | 不 print；不做会话生命周期管理（archive/prune） |
| `ys-tui-coding` | 命令表与解析、选择器浮层、`Message → TranscriptLine` 回放映射、清屏与全部同源状态重置 | 不读 `Session`/文件系统；不认识 `Agent` |

**`sessions.rs` 保持模块函数集**，但**删掉本地的 `Listing`，改用 `ys_protocol::SessionSummary`** ——
避免两个同义类型并存（`Listing` 只在 `sessions.rs` 内部出现，`main.rs` 不具名使用）。

### 3.2 协议层接口

```rust
// crates/ys-protocol/src/session.rs（新；纯数据，零 tokio）
/// 会话目录中的一个条目 —— 列表**只读首行**（4 KiB 封顶）得到，不含消息体。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    /// 最近活动时间。用 `SystemTime` 而非裸 `u64`：类型自证、无需约定单位。
    pub mtime: SystemTime,
    pub title: String,
}

// crates/ys-protocol/src/request.rs
pub enum Request {
    // …既有 8 个…
    /// 列出既有会话（`/resume` 无参）。只读元数据，不加载消息体。
    ListSessions,
    /// 恢复既有会话。`target` 交给 app 侧 `sessions::resolve` 解析（id 或路径）。
    ResumeSession { target: String },
}

pub enum Outbound<V> {
    // …既有 4 个…
    /// 会话列表（`Request::ListSessions` 的响应）。**空列表也要发** —— 让 UI 能提示「没有可恢复的会话」。
    SessionList(Vec<SessionSummary>),
    /// 会话已恢复 —— UI 见它即「清屏 + 回放」。三样必须**同时**生效，故装在同一条消息里：
    /// 分开传会出现「模型换了但屏上还是旧会话」的中间态。
    SessionResumed {
        summary: SessionSummary,
        /// 目标会话 meta 的 thinking 开关（照启动期 `--resume` 的初值语义）。
        thinking_visible: bool,
        /// 目标会话的完整历史 —— UI 侧负责转成 transcript 行。
        history: Vec<Message>,
    },
}
```

命名：`ResumeSession`（请求）对 `SessionResumed`（响应），且不再出现「激活 / Activated」一词。

### 3.3 恢复的步骤顺序（**可失败步全部前置，提交步不可失败**）

这是本设计最要紧的一条 —— 顺序错了，§5 的 I1「失败不改状态」就不成立。

```
1. sessions::resolve(dir, target)                 — Err ⇒ 只发错误 Output，**状态未动**
2. sessions::read_meta(&path)                     — 复用启动期的读法（只读首行）
3. 按优先级链**纯解析**出候选 (provider, api_base, api_key, model)（`resolve_model_from_meta`：
   env 凭证 > meta > state.json > 首个有凭证者），再由 `build_candidate_model` 得到**候选 model**
   （可能失败：无凭证 / 配置非法）—— 后者**就地换入候选四字段 → 同步 `build_model()` → 立即还原**：
   `Config` 持 `Box<dyn Fn>` factory、**非 `Clone`**，拷不出「带候选字段的副本」，只能就地换入再还原。
   ⇒ 这三个动作之间**不得插入 `await`**（有 `await` 会把「已换入、未还原」的中间态暴露给并发观察者），
   且此步**不得**碰 wiring
4. wiring.resume_into(path).await                 — open 成功**才**替换 session 句柄
5. 提交（此后不再有失败路径）：
     · 写回 config（provider / api_base / api_key / model 四字段）
     · wiring.set_model(Some(built))
     · **thinking_visible 镜像 = meta.thinking_visible**   ← 见 I9
     · *stats = seed_stats(meta)；session_started = Instant::now()
     · session_path.record(wiring.session_path())          ← 见 I3
6. 出站：SessionResumed → Output → View                   ← 顺序见 I2
```

**能失败在哪一步**：`JsonlSession::open` 是**惰性 + 逐行容错**的 —— 路径不存在 ⇒ 返回 `Ok(空会话)`；
非法 JSONL 行只被逐行跳过（告警、不终止）。故「路径不存在 / 非法 JSONL ⇒ open 失败」不成立。
真正的失败只有**真 OS 读错**（`File::open` / `read` 报错，例如把**目录**当会话路径 ⇒ `EISDIR`）。
⇒ §5 的 I1「失败原子性」只能用这类真 OS 错误构造测试。

**为什么必须这个序**：启动期的真实顺序就是「meta → 模型 → open」（`main.rs:678` 读 meta →
`main.rs:1104-1152` 解析模型 → `main.rs:966` 才 `Wiring::resume`）。运行期照抄，两处才真正同源；
若照 A/B/C 三候选原文的写法（`resume_into` 在模型解析之前），一旦模型解析失败就留下
**「会话已换、模型还是旧的」**的半切换，而 I1 承诺的「全不变」被打破。

### 3.4 app 侧改动

| 位置 | 改动 |
|---|---|
| `main.rs:1104-1152` | **抽出** `resolve_model_from_meta(cfg, meta, saved_state, registry) -> Option<BuiltModel>` —— 启动期与运行期共用同一条优先级链；**纯函数、不写 `cfg`**（写入由调用方在提交步做） |
| `wiring.rs` | 新增 `resume_into(&mut self, path) -> Result<SessionMeta, SessionError>`：**镜像 `new_session` 的「先 open 成功后才替换」**（`wiring.rs:114-129` 的不变量）；暴露 `sessions_dir()` getter |
| `capabilities.rs` | `list_sessions(&Path) -> Vec<SessionSummary>`；`resume_session(..)`。**错误约定**：沿既有 `capabilities::*` 形态返回文本行（既有 6 个能力函数**没有一个返回 `Result`**），但内部**不得吞掉 `SessionError` 的分类**（致命 vs 可恢复）—— 至少把类别带进输出文本 |
| `app_loop.rs` | 两个 `match` 臂；**提交步**见 §3.3 第 5 步；出站顺序见 I2。**`thinking_visible` 镜像必须一并重设**（`:147` 那个变量） |
| 目录扫描 | `sessions::list` 是**同步 O(N) 次 open**。app 线程跑在 multi-thread runtime 上，阻塞一个 worker 可容忍，但 `list()` 是唯一值得包 `tokio::task::spawn_blocking` 的调用（全仓零 `spawn_blocking`）。app 线程写 `std::fs` 本就**不是孤例** —— `login` / `logout` / `set_model` 也各写一次（`provider.rs` 的 `write_auth`、`state.rs` 的 `StateStore::save`），`capabilities::export` 的 `std::fs::copy`（`:282-303`）只是**其中带论证注释的一例**。⇒ 二选一：包 `spawn_blocking`，或**照既有惯例写一条上界论证注释** |

**已知语义边界（档 1 短路）**：`resolve_model_from_meta` 的第一档是 `if cfg.is_configured()` ⇒ **原样保留当前模型、不读 meta**。`is_configured()` = `api_base.is_some() && api_key.is_some()`，在「设了 env 凭证」或「曾经 `/login` 过」之后即为真。⇒ **对常见用法，「模型跟随目标会话 meta」不会生效**。这是该优先级链固有的语义边界（启动期 `--resume` 走同一条链、同源行为），不是运行期引入的回归；本设计如实记录，不绕开。

### 3.5 UI 侧改动

| 位置 | 改动 |
|---|---|
| `commands.rs` | `COMMANDS` 12 → 13（`resume`，`arg_hint = "[id]"`）；`parse` 分支：无参 → `Request(ListSessions)`，有参 → `Request(ResumeSession{target})`；`PromptKind` + `PickSession` |
| `app.rs` | `pending_sessions: Option<Vec<SessionSummary>>`；`apply_outbound` 两个新臂；新增 **`reset_for_replay()`**（见 I6/I8） |
| `transcript.rs` | `pub fn replay_lines(&[Message]) -> Vec<TranscriptLine>` —— **镜像** `apply_event` 的输出约定（**不是**「共用同一套规则」：`apply_event` 是**事件驱动**的单块事件（`app.rs:122-253`），回放输入是**复合** `Message`（`Vec<ContentBlock>` 可同时含 Text + ToolUse），形状不同。共享的是 `summarize_tool_args`（`transcript.rs:65`）与 id 配对约定） |
| `cache.rs` | `RenderCache::reset()`；并**修订模块头不变量**（`cache.rs:1-11`）：`/new` 不触、`/resume` 触 |
| `run.rs` | **两处**检查点：主循环的 `poll()` 分支**之后**（`run.rs:175-190`，原处保留 —— 键盘路径 `events.rs:268` 仍在用它）**与**排空 Outbound 之后；**模态内 `ModalHost::drain_outbound`（`run.rs:326-340`）也排空 Outbound**，那里同样要处理 `SessionList`，否则 `pending_prompt` 会被覆盖 |
| `prompter.rs` | **新增** `resolve_session_pick(prompter, &[SessionSummary], current_path) -> Result<...>`（`/resume` 无参浮层的独立入口）。<br>**不要**去改 `resolve_prompt` 的签名（它取 `&CodingView`，`prompter.rs:86`，候选却在 `App.pending_sessions`）—— 那会波及 Login/PickModel/PickSkill 三臂与全部 `FakePrompter` 单测（`prompter.rs:292-566`）；`resolve_prompt` 里对 `PickSession` 只留一条**防御性 `Err` 臂**（该 kind 只走 `resolve_session_pick`）。<br>**`PromptOutcome` 新增 `NoOp(String)`** 变体（「有意义的无操作：不发请求、只提示」）—— 选中**当前会话**时用它，原 `Request` / `Prefill` 两变体装不下这种结局。<br>**标签必须可反解**：`Prompter::select` 只回传显示串（`prompter.rs:52-59`），`PickSkill` 靠 label 反解（`prompter.rs:136-147`）。会话标签含 title + 时间 + **完整 id**，由 id 反解 ⇒ 同标题会话不会切错（定长截断会因共享前缀相撞，见 §4 Q7） |

---

## 4. Q1–Q10 裁决

| # | 问题 | 裁决 | 理由 |
|---|---|---|---|
| **Q1** | 列表数据怎么到 TUI | **按需往返**：`Request::ListSessions` → `Outbound::SessionList`，`SessionSummary` 进 `ys-protocol` | 不塞 `CodingView`（一次性载荷 vs 长期快照，§2.3）；不 TUI 自扫（破坏依赖方向） |
| **Q2** | 回放载荷与转换位置 | **`Outbound::SessionResumed.history: Vec<Message>`，UI 侧转换**；**顺带修启动期空屏** | 显示归 UI；`Message` 已是 serde 纯数据。启动期复用同一载荷（**注意 I10：启动期的出站顺序**） |
| **Q3** | 是否支持 `/resume <id>` | **支持** | 与 `/model` 双形态对称；解析复用 `sessions::resolve`，零额外成本 |
| **Q4** | 统计与时长 | **照启动期**：`stats` 由 meta 播种、`session_started` 归零 | 「等价于重启打开该会话」是 D2/D3 的立论基础，不允许分叉 |
| **Q5** | ADR-0026 决策 6 处置 | **新增 ADR-0033 显式取代决策 6**，保留 0026 其余；记录决策 4 的否决前提变化，但本轮**仍不收 trait** | ADR 是历史记录，不就地改写 |
| **Q6** | `thinking_visible` | **跟随 meta**，经 `SessionResumed` 一次性传递 | **这是一次有意的破例**：`SessionResumed` 确实改写了 UI 的运行期真值 `App.thinking_visible`，只是换了条信道 —— 与「快照不得覆盖运行期真值」（`app.rs:83-84`）相抵。破例的理由：`/resume` **不是** `/new`，其语义是「打开该会话」，而启动期 resume 本就跟随 meta（`view.rs:69-73`）。**不要把这条说成「不破不变量」**。三方真值（UI / app 镜像 / meta）见 I9 |
| **Q7** | 列表排序与字段 | mtime DESC；显示 `title`（`derive_title` 50 字符）+ 相对时间 + **完整 id**；当前会话标 `[current]`；**不静默过滤** | 标签要能**反解**出会话（`Prompter::select` 只回传显示串）。会话 id = `{unix 秒}_{纳秒零填充}`（`wiring.rs:235-240`），取自文件名 stem —— **同一秒创建的会话共享 `{秒}_` 前缀**，故任何定长截断都可能在真实语料上相撞、令反解切错会话；**完整 id 唯一**，是唯一可反解的写法。避免同标题误选；OpenCode 的教训 |
| **Q8** | 排队期反馈 | **静默**（与 `/model` 一致） | D4 由信道①天然成立，零新增机制。**已知代价**：回合进行中连**列表**都拿不到（app 线程阻塞在 `run_turn`） |
| **Q9** | 与退出回放的叠加 | 不特殊标记 | 回放前已 `clear()`，退出时倾倒的就是当前 transcript，不会重复 |
| **Q10** | `context_window` / `compat` 跟随 | **自动跟随，零新机制** | 已核实：`app_loop.rs:371` **每回合** `set_context_window(config.context_window())`（注释「唯一调用点，结构上不可能漏」，测试 `context_window_is_recomputed_after_model_switch` + `prompt_refreshes_context_window_each_turn` 钉住）；`compat` 随 `config.provider` 经 `build_model()` 重建（`main.rs:1169/1210`）。⇒ `context.md` 约束 7/8 在**本路径上不成立** |

---

## 5. 不变量与测试要点

| # | 不变量 |
|---|---|
| **I1** | **失败不改状态**：§3.3 的 1–4 步任一失败 ⇒ 当前会话、transcript、模型、thinking、统计**全不变**，只回一条错误 `Output`。达成手段 = **可失败步全部前置、提交步不可失败**（顺序即证明，见 §3.3） |
| **I2** | **出站顺序固定**：`SessionResumed` → `Output` → `View`。顺序反了，通知会被回放的清屏吃掉 |
| **I3** | **会话路径快照必须刷新**（`SessionPathHandle.record`，`app_loop.rs:103`）。Claude Code issue #37737 的教训：切换后指针不更新 ⇒ 两个进程写同一会话、消息链分叉 |
| **I4** | **空列表不弹空浮层**，只提示。对齐启动期 `resolve_resume` 的空列表分支（`main.rs:664-666`） |
| **I5** | **列表不静默过滤/截断** |
| **I6** | **回放必须使 `RenderCache` 失效，且失效要发生在正确的时机**：`cache.rs` 的键是 `(槽下标, src.len(), width)`，合法性由 `debug_assert!` 钉在「transcript **只增不减**」上。回放是本 app **第一个**清屏/缩短 transcript 的操作。<br>⚠️ **时机陷阱**：`sync_len` 有「增长」与「缩短」两条分支（`cache.rs:61-73`）；若 clear 与 extend 在**同一帧前**完成，长度看起来没变，会走**增长**分支而**绕过**缩短 ⇒ 假命中。⇒ 必须用**显式 `reset()`**（或在 extend **之前**单独 `sync_len(0)`），不能指望 `sync_len` 自动兜住 |
| **I7** | **回放全量是默认，有界是预案 —— 度量指标必须是「首帧」**：<br>· **要量的**：`reset()` 之后**首帧**的全量 `md::render` 耗时（每条 assistant 重解析）——这才是 resume **新增**的那笔（C 估 5–50 ms，最不确定项）。<br>· **不要量的**：稳态每帧的 `line_count` —— 那是 `line_count`/`build_text` 的**固有** O(transcript) 成本（`run.rs:160` 与 `run.rs:201-203` 每轮各一次，节拍 50 ms ⇒ 空闲也 20 fps），任何长会话今天就在付，与 resume 无关。**量它只会得出「没事」而漏掉真正的首帧卡顿。**<br>· **阈值**：首帧 > 100 ms ⇒ 触发有界回放预案；预案必须**显式提示**被折叠了多少（I5） |
| **I8** | **清屏要同时重置全部同源状态**：抽 `reset_for_replay()`，含与 transcript 同源的 5 项 `transcript` / `md_cache`(I6) / `tool_index` / `suppress_user_echo` / `pending_echo_texts`，外加 `follow = true` / `scroll_offset = 0`。逐项由测试钉住。<br>**`thinking_visible` 不在此列** —— 它由 `SessionResumed` 臂**按目标会话 meta 单独设定**（见 I9），与「清空同源状态」是两种语义，不混入 `reset_for_replay` |
| **I9** | **thinking 三方真值必须一次对齐**：UI 的 `App.thinking_visible`、**app 侧进程级镜像**（`app_loop.rs:147`，**每回合末无条件写进 meta**，`:219`）、以及会话 `meta`。若只改 UI 不改镜像，切换后**第一回合就把旧会话的 thinking 写进新会话 meta** |
| **I10** | **启动期也适用 I2**：启动期 `SessionResumed` 的 `clear()` 会吃掉**先**入队的启动提示。`startup_hint` 目前在 `main.rs:1017-1021` 先行入队 —— 启动期带 `--resume` 时必须改序：**先投 `SessionResumed`（回放），再投 `startup_hint`**。反过来（提示在前、回放在后）提示会被 `clear()` 抹掉，正是本条要防的 bug |
| **I11** | **回放的渲染边界**：<br>· `User` 消息**只含 `ToolResult` 块**是回放**必须处理的一种输入**（`sessions.rs` 的 `user_text_skips_non_text_and_non_user` 用例只证明「代码把这种消息判为非文本」，**不证明其出现频率**）⇒ 不可渲染成**空用户行**（`message_text` 只取 Text，`app.rs:359-367` 得空串）；<br>· `ToolUse` **无配对 `ToolResult`**（被中断的会话）：`apply_event` 的 ToolCall 默认 `success: true, result: None`（`app.rs:172-178`），失败才显结果（`draw.rs:549`）⇒ 回放会渲染成「⏺ name ✓」**看着像成功**。⇒ 无配对时置 `success = false`（屏上显 ✗），**不伪造结果文案** —— `draw.rs` 的既有语义是「失败才显结果首行」，无 `result` 即无说明行；不往 transcript 里塞合成文本 |
| **I12** | **`/resume` 后 `/new` 行为不变**：`/new` 仍只换 session 句柄、不清 transcript（既有语义不动） |
| **I13** | **`sessions_dir == None`**（内存会话，`wiring.rs:39`；生产恒 `Some`，仅测试台用）⇒ **app 侧**收到 `ListSessions` 后回一条提示（而非 `SessionList`），UI 因此不弹浮层。UI 无从知晓 `sessions_dir`，故**必然**先发 `ListSessions` —— 该边界在 **app 侧**，不在 UI 侧 |

**测试要点**（对应 `docs/design.md` §12）：

- **I1 变异**：把 `resume_into` 挪到模型解析**之前**且令模型解析失败 ⇒ 必须红（只覆盖 `open` 失败的变异抓不到半切换）。
- **I6 变异 + 等长碰撞**：务必构造两条**同字节长度、同 width** 的 assistant 文本（键含 `src.len()`，长度不等就天然 miss ⇒ 删掉 `reset()` 也照旧绿 —— 典型静默假绿）；再断言切到 B 后屏上不出现 A 的文本。
- **I9 变异**：令 `T_old != T_target`，切换后跑一个回合，断言**目标文件 meta** 的 `thinking_visible == T_target`。
- **I2/I10 顺序**：断言出站序列（运行期与启动期各一条）。
- **I11**：喂一条「只含 ToolResult 的 User 消息」与一条「无配对的 ToolUse」，断言不出现空用户行、且悬空工具不显示为成功。
- **I8**：`apply_outbound(SessionList)` 的单测断言它**同时**设置 `pending_sessions` 与 `pending_prompt`；并断言 `reset_for_replay()` 逐项清空。
- **跨路径等价**：同一逻辑会话 ⇒ live transcript 与 `replay_lines` 输出**除 §7 列出的已知形状分叉外**逐行相等（钉住「镜像」不漂移）。
- E2 加载失败保持原状；E3 选中当前会话（**no-op**）；E1 空列表；S3 meta 无凭证降级；I13 内存会话。
- 端到端：切换后继续提问 → 新消息 append 进**目标文件**；退出 `session: <id>` 是目标 id。

---

## 6. 实施步骤（建议切片）

1. **协议 + 恢复（无回放）**：加两个 `Request`、`Outbound::SessionResumed` 的 `summary`/`thinking_visible` 先走通，`history` 留空；UI 只清屏 + 提示。**这一片就验证了 D2/D4/Q6/Q10 的全部语义**，且「恢复后屏幕空」与现状启动期 resume 观感一致，不构成回归。
2. **回放通路**：`replay_lines` + `reset_for_replay()` + I11 的两条渲染边界 + 启动期复用（顺带修空屏，注意 I10 顺序）。
3. **选择器浮层**：`PromptKind::PickSession` + `resolve_session_pick`（**完整 id** 反解）+ 两处/模态内检查点 + Esc 取消。
4. **ADR + 文档同步**：ADR-0033；`architecture.md` §14 命令表（11 → 13 条，顺带修既存的 11/12 偏差）、§12 会话落盘节加运行期 resume；`context.md` 的约束 7/8 标注「已被既有机制化解」；`CONTEXT.md` 加「resume vs `/new`」条。

---

## 7. 已知代价与遗留

- **回合进行中拿不到列表**（Q8）：app 线程阻塞在 `run_turn`，连 `SessionList` 都要等回合末。若将来要让浮层「立刻弹」，需要独立的列表服务任务 —— **本轮不做**。
- **回放 ≠ 重演**：`Summary`（压缩摘要）与 thinking 内容本就不持久化，回放里没有；与 live 观感不逐像素一致。
- **回放 ≠ 逐行等价（P1-1）**：§5 宣称的「跨路径逐行等价」**已知不成立**，只对**下列形状之外**才成立。三条已知分叉都有同一根源 —— **历史里没有 live 侧才有的状态**（`is_turning`、提交原文、流式合并窗口），回放拿不到、即无法复刻。每条由 `transcript.rs` 的 `p1_1_*_diverges_*` 用例**显式断言** `replay != live` 的具体形状（任一侧改动立即变红）：
  - **app 发起的复核回合**：主回合结束后 `is_turning == false`，此时到达的 `UserMessage` live 渲染 `System(text)`，回放只有 role=User ⇒ `User(text)`。**可达**（复核是既有功能）。
  - **`/skill-name` 提交**：live 显本地回显的**展开前原文** `User("/myskill")`（随后的 `<skill …>` 信封回声被抑制），回放显落盘的信封 `User("<skill …>…</skill>")`。**空闲提交**时内容即已不同。**可达**。
    - **第三种形态（回合进行中提交）**：回合进行中提交 skill 时，`pending_echo_texts` 登记的是**展开前原文**、而回显的回声是**信封文本** ⇒ 认领不到（`position` 落空），且回声抵达时 `is_turning == false` ⇒ live 渲染成 `System(信封)` —— 与空闲提交的 `User("/myskill")` 又是另一种形状。
  - **相邻两条 assistant 文本消息**：live 合并成一条 `Assistant("ab")`，回放各起一条。**本仓 agent loop 不产出相邻 assistant 消息 ⇒ 实际不可达**，仅作防御性断言。
  - **不复刻 live 的 `is_turning` 启发式**：历史里没有该信息，只有**明示为已知代价**（即本节），不追求逐行等价。
- **全量回归现状**：`cargo test --workspace` = 1061 passed / 2 failed，两条失败**全部**在 `crates/ys-tui-coding/src/draw.rs` 的 `draw::tests::*`，系**另一会话在途的 TUI 布局重构**所致（其 diff 重写了那两个测试与几何推导），**与本次变更无关**。
- **TOCTOU**：列表已读、用户已选、加载之间目标被**改写**（不只是删除）时，`SessionSummary` 的 title/mtime 与实际内容可能不符，无校验（删除已被 E2/I1 覆盖）。
- **`pending_sessions` 生命周期**：与 `pending_prompt` **同 take 同清**；连发两条 `ListSessions` 时**后到覆盖、只开一次浮层**。
- **会话目录无跨项目分组**：`SessionMeta` **没有 cwd 字段**，不同项目的会话混在一个列表里 —— pi/Codewhale 都有 workspace 分组。**本轮暴露的独立设计缺口**，不在本需求范围内。
- **多进程并发写无锁**（`session-default-new` 既有缺口）。
- **`SessionCatalog` trait**：若将来出现**第二实现**（远程会话后端、归档目录），再按 B 的形态收窄引入。
- **有界回放**：见 I7，待实测（首帧指标 + 100 ms 阈值）。
