# 运行期 `/resume`（切换到指定 session）设计计划

## 背景

- **现状**：TUI 运行期**只有**一个会话动作 —— `/new`（换到全新会话，`Wiring::new_session`）。
  `Request` / `Outbound` 里没有任何加载既有会话的变体；`sessions::{list, resolve, pick}` 只被
  `main.rs` 的**启动期** `resolve_resume` 使用。**恢复既有会话的能力只存在于启动期 CLI flag `--resume`。**
- **启动期 `--resume` 还不上屏**：`App::new` 的 `transcript` 恒为 `Vec::new()`
  （`crates/ys-tui-coding/src/app.rs:87`），`CodingView` 只带 `message_count`
  （`crates/ys-tui-coding/src/view.rs:59`）而不带 messages。`--resume` 后用户看到的是**空屏**
  （`/status` 里 `messages N` 是唯一痕迹）。文档侧的「回放」指的是另一回事：退出时把 transcript
  倾到主屏 scrollback（`run.rs` 的 `exit_payload`）。
- **为什么是「推翻既有决策」而非补漏**：`docs/adr/0026-session-default-new-and-resume.md` 的
  **决策 6 明写「不做运行期 `/resume`（无中途换会话）」**，其 Considered Options 更把
  「运行期切换会话（CodeWhale 形态）」逐条**否决**（理由：「与『启动期一次性』冲突，且需扩 `Request`
  与运行期换 `Wiring.session`」）。`docs/arch/session-default-new/context.md` 的 D11/D12 同源。
  ⇒ 按 `AGENTS.md`「新增代码违反不变量算设计偏离，需先改设计文档」，本轮**先立 ADR**。
- **本轮以 `docs/adr/0033-runtime-resume-command.md` 显式取代决策 6**：保留 0026 其余全部决策
  （默认新会话、meta 首行、三模式统一持久化、`/new` 惰性不预建文件、标题派生…）。
  同时记录 0026 决策 4 否掉「候选 B（三 trait）」的前提（「无第二消费者」）**已部分消失**，
  但本轮**仍不收 trait**（见设计 §7）。
- **权威文档**：`docs/arch/runtime-resume/design.md`（含三候选对比与 Q1–Q10 裁决）、
  `context.md`（现状/约束）、`review.md`（质量分析：24 条发现，8 条 P1，**均已回修进设计**）、
  `docs/adr/0033-runtime-resume-command.md`（决策记录）。

## 设计

**不重述候选对比** —— 见 `docs/arch/runtime-resume/design.md` §2/§3 全文。要点：

### 整体方案

**在 `ys-protocol` 加两个请求、两个出站变体；把「恢复一个既有会话」抽成启动期 `--resume` 与运行期
`/resume` 共用的 app 侧能力；UI 侧只做「清屏 + 由 `Vec<Message>` 重建 transcript」。**

一句话的数据流：

```
/resume ──► Request::ListSessions ──► app 扫目录 ──► Outbound::SessionList
       ──► 浮层选择（Esc 取消）──► Request::ResumeSession{target}
       ──► app: 全部可失败步前置 → 换 Wiring.session + 跟随 meta 的模型/thinking + 播种统计
       ──► Outbound::SessionResumed{summary, thinking_visible, history}
       ──► UI: reset_for_replay() + 回放 + 退出提示快照刷新
```

**同时修掉启动期 `--resume` 的空屏**（`App::new` 的 transcript 恒空）—— 同一通路、同一约定。

### 语义判据（**替换而非合并**）

> **`/resume` = 整体替换 `Wiring.session` + 丢弃内存里的旧 transcript；磁盘上不删任何文件。**

- 切过去就等于「用 `--resume=<同一 id>` 打开该会话」—— 模型、thinking、统计、历史**不得与启动期分叉**。
- 旧会话文件**原样留在磁盘上**（惰性落盘的未写会话本就无文件）；「再切回来」原会话内容与统计照旧（S6）。
- **与 `/new` 的差别**：`/new` 是**同一会话谱系**里开下一段（只换 session 句柄、不清 transcript，I12）；
  resume 是**换到另一条谱系**，与「重启后打开它」同构。
- **`Envelope.turn` 与 `TurnStats` 在切换点上刻意分叉**（决策 5）：`turns` 是「进程序」计数器、**不归零**；
  `TurnStats` 照旧按目标 meta 重新播种。

### 协议层接口签名（照 design.md §3.2）

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
    /// 会话已恢复 —— UI 见它即「清屏 + 回放」。三样必须**同时**生效，故装在同一条消息里。
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

### 恢复的步骤顺序（照 design.md §3.3 —— 可失败步全部前置，提交步不可失败）

这是本设计最要紧的一条 —— 顺序错了，不变量 I1「失败不改状态」就不成立。

```
1. sessions::resolve(dir, target)                 — Err ⇒ 只发错误 Output，**状态未动**
2. sessions::read_meta(&path)                     — 复用启动期的读法（只读首行）
3. 按优先级链**纯解析**出 (provider, entry, model) — env 凭证 > meta > state.json > 首个有凭证者
   并 config.build_model() 得到**候选 model**（可能失败：无凭证/配置非法）
   ⇒ 此步之前**不得**写 config、**不得**碰 wiring
4. wiring.resume_into(path).await                 — open 成功**才**替换 session 句柄
5. 提交（此后不再有失败路径）：
     · 写回 config（provider / api_base / api_key / model 四字段）
     · wiring.set_model(Some(built))
     · **thinking_visible 镜像 = meta.thinking_visible**   ← 见 I9
     · *stats = seed_stats(meta)；session_started = Instant::now()
     · session_path.record(wiring.session_path())          ← 见 I3
6. 出站：SessionResumed → Output → View                   ← 顺序见 I2
```

**为什么必须这个序**：启动期的真实顺序就是「meta → 模型 → open」（`main.rs:678` 读 meta →
`main.rs:1104-1152` 解析模型 → `main.rs:966` 才 `Wiring::resume`）。运行期照抄，两处才真正同源；
反过来写（`resume_into` 在模型解析之前）一旦模型解析失败就留下**「会话已换、模型还是旧的」**半切换。

### Phase 1 已定的 7 条决策

| # | 决策 |
|---|---|
| **1** | `capabilities::resume_session` 错误形态：**沿既有 `Vec<String>`**（与现有一切 `capabilities::*` 一致，**不引入 `Result`**）；但内部**不得吞掉 `SessionError` 的分类**（致命 vs 可恢复），至少把类别带进输出文本 |
| **2** | `sessions::list` 的同步 O(N) IO：**照惯例在 app 线程同步调用** + 照 `capabilities::export` 先例写上界论证注释（**不**引入全仓首个 `spawn_blocking`） |
| **3** | 启动期 `--resume` 空屏：**同轮修**（共用同一条回放通路，注意 I10 顺序） |
| **4** | I7 首帧渲染耗时：**本轮加 `#[ignore]` 基准测试**（阈值 100 ms；超阈才触发有界回放预案） |
| **5** | `Envelope.turn`（`app_loop.rs:142` 的 `turns`）：**不归零** —— 它是「进程序」计数器；`TurnStats` 照旧按目标 meta 重新播种。**两者语义不同，切换点上刻意分叉** |
| **6** | `sessions::Listing`：**删除**，统一用 `ys_protocol::SessionSummary`（全仓仅 `sessions.rs` 内部具名使用，`main.rs` 不具名） |
| **7** | 验收层级：**含子进程 e2e**（照 `apps/coding-agent/tests/session_cli.rs` 既有脚手架） |

## 涉及文件

**新增**
- `crates/ys-protocol/src/session.rs` —— `SessionSummary`（纯数据、零 tokio）
- `crates/ys-tui-coding/tests/replay_bench.rs` —— I7 首帧基准（`#[ignore]`）
- `apps/coding-agent/tests/resume_cli.rs` —— 子进程 e2e（照 `session_cli.rs` 脚手架）

**修改**
- `crates/ys-protocol`：`src/request.rs`（2 个 `Request` + 2 个 `Outbound` 变体）、`src/lib.rs`（导出）。
- `crates/ys-tui-coding`：
  - `src/cache.rs`（`RenderCache::reset()` + 修订模块头「只增不减」不变量：`/new` 不触、`/resume` 触）；
  - `src/transcript.rs`（`pub fn replay_lines(&[Message]) -> Vec<TranscriptLine>` —— **镜像** `apply_event` 输出约定）；
  - `src/commands.rs`（`COMMANDS` 12→13 + `parse` 分支 + `PromptKind::PickSession`）；
  - `src/prompter.rs`（新增 `resolve_session_pick`，**不改** `resolve_prompt` 签名；标签含 id 短码以备反解）；
  - `src/app.rs`（`pending_sessions` + `reset_for_replay()` + 两个 `apply_outbound` 臂）；
  - `src/run.rs`（检查点三处：主循环 poll 分支**之后** / 排空 Outbound 之后 / 模态内 `ModalHost::drain_outbound`）；
  - `src/lib.rs`（导出 `replay_lines`）。
- `apps/coding-agent`：
  - `src/sessions.rs`（删 `Listing`，改用 `ys_protocol::SessionSummary`）；
  - `src/wiring.rs`（`resume_into` 镜像 `new_session` 的 open-then-replace；`sessions_dir_path()` getter —— 注意
    `main.rs:616` 已有**同名自由函数**，用 `sessions_dir_path()` 避混）；
  - `src/capabilities.rs`（`list_sessions` / `resume_session`，错误形态沿既有 `Vec<String>`）；
  - `src/main.rs`（抽纯函数 `resolve_model_from_meta`，启动期与运行期共用；启动期回放，注意 I10 顺序）；
  - `src/app_loop.rs`（两个 `match` 臂 + §3.3 提交步 + **`thinking_visible` 镜像重设** + `turns` 不归零）。

- `crates/ys-tui-coding/Cargo.toml` **无需改动**（`ys-core` 已在依赖里，`Message` 可用）。

## 测试策略

### 测试合同（Phase 3 审查依据）

> 每条不变量 / 边界一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。
> **review 只认「变异测试变红」作为证据**（本仓多个真 bug 是「测试全绿时依然存在」）。

#### N1: 协议 serde roundtrip
- **验收方法**: `cargo test -p ys-protocol` — 期望：2 个新 `Request`（`ListSessions` / `ResumeSession{target}`）与 2 个新 `Outbound`（`SessionList` / `SessionResumed`）经 `assert_roundtrip`（照 `request.rs:90` 既有范式）往返相等；含 `SessionSummary` 的 `PathBuf` / `SystemTime` 字段
- **优先级**: 必测
- **验收人**: review subagent

#### N2: 恢复失败不改状态
- **验收方法**: `cargo test -p ys-coding-agent` — 期望：**变异「模型解析挪到 `resume_into` 之后且失败」⇒ 必须红**（只覆盖 `open` 失败的变异抓不到半切换）；`resume_into` 失败时当前 session / transcript / 模型 / thinking / 统计**全不变**
- **优先级**: 必测
- **验收人**: review subagent

#### N3: 出站顺序（运行期）
- **验收方法**: `cargo test -p ys-coding-agent` — 期望：运行期出站序列断言为 `SessionResumed → Output → View`（顺序反了通知会被回放的清屏吃掉）
- **优先级**: 必测
- **验收人**: review subagent

#### N4: 启动期顺序
- **验收方法**: `cargo test -p ys-coding-agent` — 期望：启动期带 `--resume` 时**先投 `SessionResumed`（回放）、再投 `startup_hint`**（`main.rs:1017-1021`），使启动提示**不被回放的 `clear()` 吃掉**；反序应能被测试抓住
- **优先级**: 必测
- **验收人**: review subagent

#### N5: 会话路径快照刷新
- **验收方法**: `cargo test -p ys-coding-agent --test resume_cli` — 期望：切换后退出提示 `session: <id>` 是**目标 id**（`SessionPathHandle.record` 被刷新；Claude Code issue #37737 教训：指针不更新 ⇒ 两进程写同一会话、消息链分叉）
- **优先级**: 必测
- **验收人**: review subagent

#### N6: 回放前显式 `RenderCache::reset()`
- **验收方法**: `cargo test -p ys-tui-coding` — 期望：**测试须构造两条同字节长度、同 width 的 assistant 文本**（键含 `src.len()`，长度不等就天然 miss ⇒ 删掉 `reset()` 也照旧绿 —— 典型静默假绿）；再**变异：删掉 `reset()` ⇒ 必须红**；断言切到 B 后屏上不出现 A 的文本
- **优先级**: 必测
- **验收人**: review subagent

#### N7: thinking 三方真值对齐
- **验收方法**: `cargo test -p ys-coding-agent` — 期望：**变异**令 `T_old != T_target`，切换后跑一回合，断言**目标文件 meta** 的 `thinking_visible == T_target`（三方 = UI `App.thinking_visible` / app 镜像 `app_loop.rs:147`（每回合末无条件写进 meta，`:219`）/ 会话 meta；只改 UI 不改镜像 ⇒ 切换后第一回合就把旧会话 thinking 写进新会话 meta）
- **优先级**: 必测
- **验收人**: review subagent

#### N8: `reset_for_replay()` 逐项清空
- **验收方法**: `cargo test -p ys-tui-coding` — 期望：断言 `reset_for_replay()` 逐项清空 `transcript` / `md_cache` / `tool_index` / `suppress_user_echo` / `pending_echo_texts` / `follow`（= true）/ `scroll_offset`（= 0）/ `thinking_visible`
- **优先级**: 必测
- **验收人**: review subagent

#### N9: 回放渲染边界
- **验收方法**: `cargo test -p ys-tui-coding` — 期望：喂一条**无配对 `ToolUse`**（被中断会话）⇒ 不得显示为成功（`success = false` + 文案），不得「看着像成功」；喂一条**只含 `ToolResult` 块的 User 消息**⇒ 不得渲染成**空用户行**
- **优先级**: 必测
- **验收人**: review subagent

#### N10: `replay_lines` 与 live `apply_event` 逐行等价
- **验收方法**: `cargo test -p ys-tui-coding` — 期望：同一逻辑会话 ⇒ live transcript 与 `replay_lines` 输出**逐行相等**（钉住「镜像」不漂移）
- **优先级**: 必测
- **验收人**: review subagent

#### N11: 边界
- **验收方法**: `cargo test -p ys-tui-coding -p ys-coding-agent` — 期望：**空列表不弹浮层**（I4，只提示，对齐 `main.rs:664-666`）；**选中当前会话 = no-op**（E3，无副作用）；**`sessions_dir == None`**（内存会话，I13）⇒ 提示「本会话不可恢复」，不弹浮层、不发请求；**`Envelope.turn` 不归零**（决策 5）
- **优先级**: 必测
- **验收人**: review subagent

#### N12: 子进程 e2e
- **验收方法**: `cargo test -p ys-coding-agent --test resume_cli` — 期望：切换后**继续提问** ⇒ 新消息 append 进**目标文件**（S5）；退出 `session: <id>` 是目标 id；`--resume` 启动期**回放上屏**（修掉空屏）
- **优先级**: 必测
- **验收人**: review subagent

#### N13: I7 首帧 md 渲染耗时基准
- **验收方法**: `cargo test -p ys-tui-coding --release --test replay_bench -- --ignored --nocapture` — 期望：读 `reset()` 之后**首帧**的全量 `md::render` 耗时（**不是**稳态 `line_count` —— 那是任何长会话今天就在付的既有成本）；阈值 **100 ms**，超阈才触发有界回放预案
- **优先级**: 必测（可运行，**不作阻塞项**）
- **验收人**: review subagent

### 测试框架与命令

- 全量: `cargo test --workspace`
- 静态: `cargo clippy --all-targets` / `cargo fmt --check`（**禁止裸 `cargo fmt`** —— 本机 rustfmt 与该仓库 CJK 文本有既有分歧）
- 子进程 e2e: `cargo test -p ys-coding-agent --test resume_cli`
- 基准: `cargo test -p ys-tui-coding --release --test replay_bench -- --ignored --nocapture`

## 波及文档

Phase 4 更新 `docs/design-final/`：

- **新增** `docs/design-final/runtime-resume.md` —— 本特性的 as-built 形态。
- **更新** `docs/design-final/coding-agent-tui.md` —— 命令表与 `/resume` 通路、回放与 `reset_for_replay()`。
- **更新** `docs/design-final/session-default-new.md` —— 与「启动期一次性 / 不做运行期切换」相关处（D11/D12 已被
  ADR-0033 取代），以及启动期 resume 上屏这一变化。

另（设计阶段即须同步，见 T14）：`docs/architecture.md`（§14 命令表 11→13 条并加 `/resume`；§12 会话落盘节）、
`docs/CONTEXT.md`（加「resume vs `/new`」术语）、`docs/arch/runtime-resume/design.md` 与 `context.md` 的订正、
`AGENTS.md`（校正 ignore 数字：实测 6 个 `#[ignore]`）。

## 风险与注意事项

从 `review.md` 风险表提炼（P1 已回修进设计，**开工前须先落成可失败测试**）：

- **T1（P1）顺序写反 ⇒ 半切换**：设计把「失败原子性」当核心承诺写进 I1，却在 §3.3 给出与之互斥的调用序，
  且**三个候选原文都是这个错序**（是共识性错误，非笔误）。已达成的修法：**可失败步全部前置、提交步不可失败**。
  危害形态正是本项目最警惕的 —— 编译通过、测试全绿、只在「模型解析失败」这条罕见分支留下半切换。
- **T2（P1）thinking 有三份真值，只对齐两份**：`App.thinking_visible` / app 侧进程级镜像
  （`app_loop.rs:147`，每回合末**无条件**写穿，`:219`）/ 会话 meta。同类问题在
  session-default-new 的 review T7 已出现过一次（「两份真值漂移」），这是同一坑的第三次。
- **T3（P1）I6 的验证草图会假绿、且失效时机有陷阱**：`cache.rs` 槽位键 = `(槽下标, src.len(), width)`；
  新旧会话对应槽位字节长度**通常不等** ⇒ 键天然 miss ⇒ 删掉 `reset()` 也照绿。
  更细一层：`sync_len` 有「增长」「缩短」两条分支，clear 与 extend 在同一帧前完成 ⇒ 走增长分支绕过缩短 ⇒ 假命中。
  ⇒ **必须显式 `reset()`**，测试**必须构造等长碰撞**。
- **T5（P1）I7 量错指标**：原 I7 量的是稳态 `line_count`（任何长会话今天就在付，量了结论必然「没事」），
  漏掉 resume **新增**的那笔 —— `reset()` 后**首帧**的全量 `md::render`（候选 C 自评「最不确定项 5–50 ms」）。
- **T7（P1）选择器入口**：`resolve_prompt` 取 `&CodingView`（`prompter.rs:86`）装不下 `App.pending_sessions`
  里的候选 —— **不要**改它的签名（会波及 Login/PickModel/PickSkill 三臂与全部 `FakePrompter` 单测）。
  `Prompter::select` 只回传显示串，故标签**必须含可反解的 id 短码**；否则默认标题都是「新会话」的会话会**切错**。
- **T6（P1）**「与 `apply_event` 共用映射」与事实不符（事件驱动单块 vs 复合 `Message`）⇒ 实现时会变成两套并行映射，
  正是它想避免的漂移。已改口为**镜像** + 跨路径等价单测（N10）。
- **T8（P1）**清屏只清 `md_cache` 会漏 `tool_index` / `pending_echo_texts` 等同源状态 ⇒ 抽 `reset_for_replay()`（N8）。
- **T4（P1）**I1 的变异只覆盖 `open` 失败抓不到 T1 的半切换 ⇒ 补「模型解析挪到替换之后」的变异（N2）。
- **T12（P2）**悬空 `ToolUse` 渲染成「✓」看着像成功；`ToolResult`-only 的 User 消息渲染成空行（N9）。
- **T11（P2）**启动期 `SessionResumed` 的 `clear()` 会吃掉先入队的启动提示（I10，N4）。
- **T15（P2）**`sessions::list` 是同步 O(N) 次 open，跑在 async app 线程 ⇒ `spawn_blocking` 或照 `export` 先例写上界注释（决策 2）。
- **T18（P2）TOCTOU**：列表已读、用户已选、加载之间目标被**改写**（不只是删除）时 `SessionSummary` 的 title/mtime
  可能与实际不符，无校验 —— 记入 `design.md` §7 遗留（删除已被 E2/I1 覆盖）。
- **回放峰值内存约 3×**（`session.messages` + `history` clone + `TranscriptLine` 产物），带大工具结果的会话可达数十 MB
  —— 设计未估，先记入遗留（review 2.5）。
- **架构级缺口（不在本需求范围）**：`SessionMeta` **缺 cwd 字段** ⇒ 不同项目的会话混在一个列表里（pi/Codewhale 都有
  workspace 分组）—— 本需求的 picker 把它变成了用户可见问题，建议单独立项。
- **依赖方向**：`ys-tui-coding` 不得依赖 `ys-runtime`（编译器强制）；`ys-protocol` 零 tokio；
  UI 线程必须无 runtime 上下文（它用 `blocking_send`，在 async 上下文 panic）；`capabilities.rs` 绝不 print。
- 每条契约改动都要有**变异自检**（本仓多个真 bug 是「测试全绿时依然存在」）。
