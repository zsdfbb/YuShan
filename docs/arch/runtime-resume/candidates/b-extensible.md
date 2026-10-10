# 候选 B —— 可扩展优先：`/resume` 运行期切换会话

> 设计哲学：**接口抽象、能力可组合、为长期演进与第二消费者留位**。
> 输入：`../context.md`（Q1–Q10）、`docs/adr/0026-session-default-new-and-resume.md`、`docs/arch/per-turn-model-switch/context.md`。
> 本文逐条回答 Q1–Q10，并**正面裁决** ADR-0026 候选 B 的三 trait。
> 代码证据对应 `master` HEAD `3a3bb86`。

---

## 0. 一句话骨架

> 把「**激活一个会话**」抽成一条**两处共用的能力路径**（启动期 `--resume` 与运行期 `/resume`），
> 数据经**协议层**（`ys-protocol` 的纯数据类型）过，UI 侧只做渲染；
> `SessionCatalog` 与 `SessionPicker` 复活为**具名接缝**（各有今天就在的第二个消费者），
> `TitlePolicy` 维持否决（今天的抽象没有第二消费者，属投机）。

三条支柱：

1. **一个语义**：`/resume` = 「激活会话」= 启动期 `--resume` 的**同一件事**。二者**共用一条 app 侧函数**（`activate_session`）与**一条回放载体**。
2. **协议是唯一接缝**：列表与历史都经 `ys-protocol`（纯数据、零 tokio）送到 UI，`ys-tui-coding` 仍不认识 `Session`。
3. **窄抽象、具名消费者**：每个新接缝都必须点名「今天谁在用它」，否则不做（`TitlePolicy` 即因此出局）。

---

## 1. 对 ADR-0026 候选 B 的正面裁决

ADR-0026 决策 4 否决候选 B 的理由是「**无第二消费者**：唯一会话后端是本地目录；决策 6 排除运行期 `/resume`；决策 4 不做标题策略配置」。
本需求推翻决策 6 ⇒ **该否决的第二个前提消失**。逐 trait 复访：

| trait | 裁决 | 理由（今天就在的消费者 / 诚实标注投机） |
|---|---|---|
| **`SessionCatalog`** | **复活（收窄）** | 消费者**今天就在**：运行期 `/resume` + **启动期 `--resume` 裸 flag 的 `list()`**（`sessions.rs:56`）—— 同一功能第二个入口。计划中消费者：`-p` 模式的 resume、会话归档/清理、`/branch`（要 `create`）、跨目录列举（Codewhale 的 current/all）。**第二实现暂无**（只有本地目录）⇒ 收窄为**单实现接缝**：泛型 / `impl` 传参，**不用 `Box<dyn>`、不建注册表**（守 `CLAUDE.md`「静态组合优先」）。 |
| **`SessionPicker`** | **复活** | 消费者**今天已经有两个**：启动期 stdin 选择器（`sessions::pick`，`sessions.rs:118`）与运行期 TUI 模态浮层。二者**共用同一段「选哪一个」的决策逻辑**（把它抽成纯函数，接进既有 `Prompter` trait，`prompter.rs:52`）——这正是 `prompter.rs` 已经证明可用的范式（`resolve_prompt`）。**不投机**。 |
| **`TitlePolicy`** | **维持否决（保持具体函数）** | ADR-0026 决策 4 已定「机械派生、不调模型」（`derive_title`，`sessions.rs:173`）；**今天没有任何第二策略消费者**（不做标题策略配置，`/resume` 也不改标题规则）。把 `derive_title` trait 化 = 纯投机。**明确不做**。 |

**结论**：候选 B **三之其二复活**（`SessionCatalog` 收窄为单实现接缝；`SessionPicker` 复用既有 `Prompter` 范式），`TitlePolicy` 继续出局。
抽象层次**有明确服务对象**：第二消费者是「启动期 resume / `-p` resume / 会话生命周期动作 / `/branch`」，第二**实现**目前没有 ⇒ 因此**只留接缝、不建框架**。

---

## 2. 模块划分与职责

```
ys-protocol（纯数据、零 tokio）          apps/coding-agent（能力实现）        ys-tui-coding（显示）
──────────────────────────────          ──────────────────────────────      ──────────────────────
SessionSummary         ← 由 Listing 上移 SessionCatalog (trait, 单实现)      commands: +/resume
SessionActivation      （新）            activate_session()  ← 启动/运行共用   PromptKind::PickSession
Request += ListSessions/ResumeSession    capabilities::list_sessions/         run.rs: fetch→modal
Outbound += SessionList/SessionActivated resume_session                      app.rs: apply_outbound
                                          sessions::LocalDirCatalog            replay::history_lines
```

**职责边界（沿用既有不变量）**：

- `ys-protocol`：只装**数据**（列表项、激活载荷、新请求/出站变体）。零 tokio；`SessionSummary` 里 `mtime: SystemTime` 无 tokio 依赖且 serde 原生支持。
- `apps/coding-agent`：**能力实现**——扫目录、解析目标、原子加载、重建 model、播种 stats、产出激活载荷。**绝不 print**（`capabilities.rs` 契约）。
- `ys-tui-coding`：**显示**——命令表、浮层、回放渲染。**不认识 `Session`**；只消费协议里的纯数据。

---

## 3. 关键数据流

### 3.1 列表 → 浮层（Q1 走「按需往返」）

```
TUI: /resume
  commands.rs::parse        → Action::Prompt(PromptKind::PickSession)
  run.rs                    → 进入「fetch→modal」子循环：
      blocking_send(Request::ListSessions)                    （UI 线程，非 async）
  app_loop 下一轮 recv      → capabilities::list_sessions(&catalog)
                            → Outbound::SessionList(Vec<SessionSummary>)
  run.rs 排空 rx_out        → 收到 SessionList（有界等待；超时/取消 ⇒ 报错、不弹空浮层）
                            → 若空 ⇒ E1「没有可恢复的会话」（不弹空浮层）
                            → 否则 select_modal(summaries)  ← 复用既有模态浮层
                            → Esc ⇒ 取消，无副作用（S2/E6）
      blocking_send(Request::ResumeSession { target })
```

**为什么按需往返（(a)）而非塞 `CodingView`（(b)）**：约束 11 —— `build_view` 每回合跑一次，把扫目录塞进去 = **每回合扫一次**（会话多时是 N 次 open）。按需往返把扫描**限死在用户敲 `/resume` 那一刻**，且天然支持第二个 UI / `-p`。

### 3.2 切换 → 激活 + 回放（Q2 载荷与转换位置）

```
app_loop 下一轮 recv（= 回合结束后 ⇒ D4 排队零机制）
  capabilities::resume_session(&mut config, &mut wiring, &state, &catalog, target)
     1. catalog.resolve(target)              // id → 路径，复用 sessions::resolve（sessions.rs:102）
     2. 原子加载：先 open 成功，再换句柄        // 失败 ⇒ 保持当前会话不变（约束 5）
     3. 跟随 meta 重建 model（共用优先级链）    // D2；无凭证降级不失败
     4. stats 由 meta 播种（seed_stats 同款）；session_started = now
     5. session_path.record(新路径)           // 退出提示快照刷新
  → Outbound::SessionActivated { summary, thinking_visible, history: Vec<Message> }

UI::apply_outbound(SessionActivated):
  → transcript.clear()                       // D3 清屏
  → transcript += replay::history_lines(&history)   // messages → TranscriptLine（UI 侧转换）
  → thinking_visible = 载荷值                  // Q6
  → 回合态复位（is_turning / tool_index / suppress_user_echo）
```

**Q2 裁决：载荷 = `Vec<Message>`；转换在 UI 侧**，用一条**共用纯函数** `history_lines(&[Message]) -> Vec<TranscriptLine>`。

理由（可扩展优先）：

1. **渲染策略属 UI**：`TranscriptLine` 在 `ys-tui-coding`，`ys-core::Message` 里没有「工具摘要 / thinking 显隐」这些产品知识。让 app 产出 `TranscriptLine` 会把显示策略塞进 app。
2. **复用同一套映射**：`history_lines` 与 `App::apply_event` 的逐块映射**共用**（重构出 `message_to_lines`），保证「回放」与「实时回合」永不漂移 —— 这是**正确性**收益。
3. **第二消费者今天就在**：`history_lines` 同时修好「**启动期 `--resume` 空屏**」（`app.rs:87` transcript 恒空）—— 同一载体 `SessionActivated` 从启动路径也发一次即可。这是 D3 推论点名的那条通路。
4. **未来消费者**：`/branch`（复制历史）、`/export`（渲染成 transcript）、第二个非 coding UI（渲染方式不同）—— 都想要**原始 messages**，而非 coding 专属的 `TranscriptLine`。

**为什么用 `Outbound::SessionActivated`（结构体变体）而非裸 `Replay(Vec<...>)`**：一次切换是**原子单元**（清屏 + 回放 + thinking + 后续事件的顺序），把载荷并进一个变体让 UI 一次拿到全部、不可能出现「清了屏但没回放」的中间态。字段可增（向后兼容只需给新字段 `#[serde(default)]`），比「多发送几条 `Outbound`」更少竞态面。

**顺序语义**（约束 9）：`SessionActivated` 由 app 在**处理该 Request 时**立即发送，先于该请求后的一切 `View`/事件 —— 而切换本身发生在回合边界（D4），回合进行中不会有事件溢出到它前面。回放先于后续事件成立。

---

## 4. 核心接口签名

### 4.1 `ys-protocol`（纯数据）

```rust
// crates/ys-protocol/src/session.rs（新文件；模块头 //! 说明它是「会话的只读摘要与激活载荷」）

use std::path::PathBuf;
use std::time::SystemTime;
use ys_core::Message;

/// 会话列表项 —— 由 apps 的 `sessions::Listing` **上移**而来（`sessions.rs:26-34`）。
/// 刻意最小（id/path/mtime/title）：不读消息体、不读 token（成本可控）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 文件 stem，即 `resolve` 接受的 id。
    pub id: String,
    pub path: PathBuf,
    /// 最近活动（全量重写 ⇒ mtime ≈ 最近活动）；排序键。
    pub mtime: SystemTime,
    /// 显示标题（`derive_title` 折叠截断后）。
    pub title: String,
}

/// 会话激活载荷 —— 「清屏 + 回放 + 采用目标状态」的原子单元。
/// 启动期 `--resume` 与运行期 `/resume` **共用**此载体（Q2/Q6）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionActivation {
    /// 目标会话的只读摘要（UI 据此更新「当前会话」显示；pick 浮层标 current 也靠它）。
    pub summary: SessionSummary,
    /// 目标会话 meta 的 thinking 显示值（Q6：切换采用它；`/new` 语义不变）。
    pub thinking_visible: bool,
    /// 目标会话历史。**原始 `Message`** —— 映射成 `TranscriptLine` 是 UI 的事。
    pub history: Vec<Message>,
}
```

```rust
// request.rs —— 新增两个变体（既有 8 → 10）

pub enum Request {
    /* …既有 8 变体不变… */
    /// 列出可恢复的会话（按需往返，**不**塞进 View）。UI → app → `Outbound::SessionList`。
    ListSessions,
    /// 切换到指定会话。`target` = id 或字面路径，由 app 侧**唯一解析器**
    /// `sessions::resolve` 解析（与 CLI `--resume=<id>` 同源，杜绝两套语义漂移）。
    ResumeSession { target: String },
}

pub enum Outbound<V> {
    /* …既有 4 变体不变… */
    /// 会话列表（`Request::ListSessions` 的应答）。纯数据，产品无关。
    SessionList(Vec<SessionSummary>),
    /// 会话激活：清屏 + 回放历史 + 采用 thinking。产品无关载荷（**不含** V）。
    SessionActivated(SessionActivation),
}
```

> `-p` / `--json` 的消费循环匹配的是 `Envelope`（`main.rs:835/875` 收 `Receiver<Envelope>`），**不碰 `Outbound`** ⇒ 新增 `Outbound` 变体**不影响**一次性模式（与 `context.md`「那两模式也要处理它」的揣测相反）。真实改动面只有 `ys-tui-coding::App::apply_outbound`（穷尽 match）与 `app_loop` 的构造点。

### 4.2 `apps/coding-agent` —— `SessionCatalog` 接缝（单实现）

```rust
// sessions.rs

/// 会话来源（接缝）。单实现：本地目录。**泛型传参，不用 Box<dyn>、不建注册表**。
/// 消费者（今天）：运行期 /resume、启动期 pick；计划：-p resume、archive/prune、/branch。
pub trait SessionCatalog {
    /// 列出会话（mtime 倒序）。**不静默过滤 / 不静默截断**（opencode 教训）。
    fn list(&self) -> Vec<SessionSummary>;
    /// id 或路径 → 会话文件路径。id 先、路径后（`sessions::resolve` 的既有契约）。
    fn resolve(&self, target: &str) -> Result<PathBuf, String>;
    /// 读回一个会话的摘要 + 历史（激活用）。
    fn read(&self, path: &Path) -> Result<(SessionSummary, Vec<Message>), String>;
}

/// 唯一实现：`sessions/{id}.jsonl` 目录。
pub struct LocalDirCatalog { dir: PathBuf }
impl SessionCatalog for LocalDirCatalog { /* 转调既有 list/resolve/read_meta/JsonlSession::open */ }
```

### 4.3 `apps/coding-agent` —— 共用的激活路径

```rust
// capabilities.rs（或新 activation.rs）

/// **启动期 `--resume` 与运行期 `/resume` 共同调用的唯一入口**。
/// 步骤严格有序（失败原子性，约束 5）：解析 → open 成功 → 才换句柄 → 重建 model → 播种。
/// 任一步失败返回 `Err`，`wiring` / `config` **保持原状**。
pub async fn activate_session<C: SessionCatalog>(
    config: &mut Config,
    wiring: &mut Wiring,
    state: &StateStore,
    catalog: &C,
    target: &str,
) -> Result<SessionActivation, String>;

/// 列表：薄包装 `catalog.list()`（供 `/resume` 浮层）。
pub fn list_sessions<C: SessionCatalog>(catalog: &C) -> Vec<SessionSummary> {
    catalog.list()
}

/// 抽出启动期模型优先级链（`main.rs:1104-1152`）为**可复用函数**：
/// env 凭证 > meta(provider+model，须 auth_for 命中) > state.json > 首個有凭证者。
/// 运行期与启动期共用；也是 `per-turn-model-switch` 跨 provider 切换的**共享机制**。
pub fn resolve_model_from_meta(config: &mut Config, registry: &ProviderRegistry, meta: &SessionMeta);
```

### 4.4 `ys-tui-coding` —— 命令与选择器

```rust
// commands.rs
static COMMANDS: [CommandSpec; 13] = [ /* …12 条… */ CommandSpec {
    name: "resume", description: "切换到既有会话（无参时浮层选择）", arg_hint: Some("[id]"),
}];

// parse：
"resume" => if args.is_empty() {
    Action::Prompt(PromptKind::PickSession)          // 无参 ⇒ 浮层
} else {
    Action::Request(Request::ResumeSession { target: arg() })   // 有参 ⇒ 直切（Q3）
},

// commands.rs
pub enum PromptKind { /* …Login|PickModel|PickSkill… */ PickSession }

// prompter.rs —— 与 `resolve_prompt` 同构的**纯决策函数**（可单测、可被启动期 stdin 复用）
pub fn resolve_session_pick(
    prompter: &dyn Prompter,
    sessions: &[SessionSummary],
    current_path: Option<&Path>,
) -> Result<PromptOutcome, PromptError>;   // 选中 ⇒ Request::ResumeSession { target: id }

// replay.rs（新）—— 共用映射，回放与实时同一套
pub fn history_lines(messages: &[Message]) -> Vec<TranscriptLine>;
```

### 4.5 `App` 的接入

```rust
// app.rs::apply_outbound
Outbound::SessionList(list)      => self.known_sessions = list,   // 供 fetch→modal
Outbound::SessionActivated(a)    => {
    self.transcript.clear();                       // D3 清屏
    self.transcript.extend(replay::history_lines(&a.history));
    self.thinking_visible = a.thinking_visible;    // Q6
    self.finish_turn_reset();                      // 复用既有复位：is_turning/tool_index/suppress
    self.follow = true; self.scroll_offset = 0;
}
```

---

## 5. 扩展点清单与预期消费者

| # | 扩展点 | 今天的消费者（**已存在**） | 计划 / 投机 |
|---|---|---|---|
| E1 | `SessionSummary`（入 `ys-protocol`） | TUI 浮层；启动期 pick | 计划：`-p` resume 列表；投机：第二个非 coding UI |
| E2 | `SessionCatalog` trait（单实现） | 运行期 resume；启动期 `list` | 计划：`-p` resume、archive/prune、`/branch`(create)、跨目录；**投机：第二实现（云/远端）** |
| E3 | `Outbound::SessionList` | `/resume` 浮层 | 计划：`--json` 若将来做运行期；投机：第二 UI |
| E4 | `SessionActivated`（回放载体） | 运行期 `/resume`；**启动期 `--resume` 修空屏** | 计划：`/branch`（克隆后激活） |
| E5 | `history_lines`（共用映射） | 实时 `apply_event`（重构共用）；运行期回放；启动期回放 | 计划：`/export` 渲染、`/branch` |
| E6 | `PromptKind::PickSession` + `resolve_session_pick` | TUI 模态浮层；**启动期 stdin 选择器**（同一决策函数） | 计划：新的非 TUI 选择器测试 |
| E7 | `resolve_model_from_meta`（优先级链抽出） | 启动期模型解析；运行期 resume | **与 `per-turn-model-switch` 的跨 provider 切换共享**（见 §7） |
| **X1** | ~~`TitlePolicy` trait~~ | — | **明确不做**（无第二消费者，投机） |
| **X2** | 列表范围/过滤（current workspace vs all、archived 显隐、分页） | — | **投机**（Codewhale 有，YuShan 暂无消费者）。**只立约束不实现**：列表**不得静默过滤/截断**（opencode 教训），若将来加范围，浮层必须**显式**标出「已过滤 / 共 N 条」。 |
| **X3** | 排队期的 pending 反馈（Q8） | — | **投机**：本轮与 `/model` 一致（无反馈）。 |

**诚实度声明**：E1/E3 的「第二实现」与 X2/X3 属**投机**，本轮**不实现**，仅在接缝形状上不堵死。E2 的 trait 是**单实现接缝**——如果评审认为连这个也过早，可**降级为模块自由函数**且**不改任何调用点**（调用点接收 `&C` 泛型，把 `LocalDirCatalog` 换成模块不变量即可）。这是本方案对「过度设计」批评的**可回退设计**。

---

## 6. Q1–Q10 逐条回答

| Q | 答案 |
|---|---|
| **Q1 列表数据怎么到 TUI？** | **走 (a) 按需往返**。`Request::ListSessions` → `Outbound::SessionList(Vec<SessionSummary>)`，`SessionSummary` **上移进 `ys-protocol`**（纯数据、零 tokio）。否决 (b) 塞 `CodingView`（触发每回合扫描，违反约束 11）；否决 (c) TUI 自己扫盘（违反依赖方向、要复制 4 KiB 封顶细节）。 |
| **Q2 回放载荷与转换位置？** | 载荷 = `Vec<Message>`，装在 `Outbound::SessionActivated`（原子单元）；**转换在 UI 侧**，用共用纯函数 `history_lines`。**顺带修启动期空屏**——同一条通路、同一载体（启动路径也发一次 `SessionActivated`）。否决「app 产出 `TranscriptLine`」（把显示策略塞进 app，且第二 UI 拿不到原始历史）。 |
| **Q3 支持 `/resume <id\|路径>` 直切？** | **支持**，与 `/model` 双形态对称：无参 → `Prompt(PickSession)` 浮层；有参 → `Request::ResumeSession{target}`。`target: String` 由 **app 侧唯一解析器 `sessions::resolve`** 解析（id 先、路径后）——与 CLI `--resume=<id>` 同源，**一套解析、两处复用**。 |
| **Q4 时长/统计随切换归零还是播种？** | **照启动期口径**：tokens + turns 由 meta **播种**（复用 `seed_stats`）、**时长不恢复**（`session_started = now`）。理由：质量目标「= 重启后用 `--resume=<id>` 打开它」要求**两处同一套统计**，这正是不分叉的关键。 |
| **Q5 ADR-0026 决策 6 怎么处置？** | **新增 ADR 显式取代决策 6**（保留 0026 其余：默认新会话 / meta 首行 / 机械标题 / 三模式持久化）。新 ADR **同时复访候选 B**：`SessionCatalog`/`SessionPicker` 复活（收窄）、`TitlePolicy` 维持否决（§1）。ADR 是历史记录，**不就地改写 0026**。 |
| **Q6 `thinking_visible` 是否跟随目标 meta？** | **跟随**，经 `SessionActivated.thinking_visible` 传递。这是对 `session-default-new` T7 的**精化而非推翻**：T7 说「`/new` 不重置、UI 拥有运行期真值」——`/resume` **不是 `/new`**，它是一次**具名的会话激活**，语义上「等于打开那个会话」（D3）。值随激活载体**显式单次传递**（非经 `View` 快照），不会引入第二份常驻真值。 |
| **Q7 浮层排序与字段？** | 排序 **mtime DESC**（复用 `list`，`sessions.rs:96`）；显示 title（`derive_title` 50 字符，`sessions.rs:173`）+ 时间 + id。**「当前会话」标记由 UI 本地按 `path` 比较得出**（不放进 `SessionSummary`，避免 app/UI 两份真值）——防 E3 误选。**不静默过滤/截断**（X2 约束）；本轮不加 message_count/token（要读消息体，成本可控但无消费者）。 |
| **Q8 排队期 UI 反馈？** | **本轮无额外反馈，与 `/model` 一致**。D4 的排队直接复用信道①的「下一轮 `recv` 才处理」性质（回合进行中 app 线程阻塞在 `run_turn`，不可能 `recv`）——**零额外机制**。pending 指示器列为投机扩展点 X3，本轮不做（保持与 `/model` 语义一致）。 |
| **Q9 回放与退出回放叠加？** | **不叠加、不重复**。回放时 `transcript.clear()` 后重填（D3 清屏），退出回放（`run.rs:exit_payload`）仍照常把**当前** transcript 倾到 scrollback —— 切换后退出会看到目标会话历史，**这正是「打开了那个会话」的正确观感**。多次切换因每次都 clear，不会累积重复。**无需**标记「回放行」（清屏本身就是去重机制）。 |
| **Q10 `context_window` / `compat` 跟随吗？** | **自动跟随，且免费**。`context_window` 已由 `app_loop::run_turn_once` **每回合**按 `config.context_window()` 重算（`app_loop.rs:371`，`prompt_refreshes_context_window_each_turn` 钉住）；`compat` 由 model factory 读 `config.current_compat()`（`main.rs` 的 `set_model_factory`）。只要激活走「**改 `config.provider`/`model` + 重建 model**」这条路（与启动期同路），两者**自然跟上**。这是**与 `per-turn-model-switch` 共享的核心机制**（见 §7）。 |

---

## 7. 与 `per-turn-model-switch` 共享的机制

`per-turn-model-switch/context.md` 的现状 ③（跨 provider 轻量切换，现只能走 `/login`）与本需求都要「**改 `config.model`/`provider` → 重建 model，不落盘**」。共享：

1. **`resolve_model_from_meta`（E7）**：本需求抽出的启动期优先级链，正是跨 provider 切换「据 provider+model 选端点+凭证」的同一段逻辑（`main.rs:1104-1152`）。**抽出一次、两处复用**，避免漂移。
2. **「重建不落盘」路径**：`activate_session` 里的重建步骤（`config.build_model()` + `wiring.set_model`）与 `set_model` 的重建同源。二者都**不写 `auth.json`**（不像 `/login`）；本需求按 D2 **也不写 `state.json`**（跟随会话 meta，改的是会话不是全局默认）——恰是 `per-turn-model-switch` Q5「临时切换 vs 持久默认」的一个已验证答案。
3. **Q10 的 `context_window`/`compat` 跟随**：对两者是**同一个坑、同一个解**（走 config + factory）。建议两条线**共用同一次 `config` 更新 API**。

---

## 8. 实施步骤与测试要点

### 实施步骤（顺序即依赖）

1. **写 ADR**（取代决策 6 + 复访候选 B）——先立设计文档，否则实现即设计偏离（`CLAUDE.md` 硬约束）。
2. **`ys-protocol`**：新增 `session.rs`（`SessionSummary`/`SessionActivation`）、`Request`/`Outbound` 新变体；补 serde roundtrip + 向后兼容测试。
3. **app 侧接缝**：`sessions::LocalDirCatalog`（转调既有 `list/resolve/read_meta`）；抽出 `resolve_model_from_meta`；抽出 `activate_session`（把启动期 `wiring.rs:71-78` + 优先级链 + `seed_stats` 收进共用路径）。
4. **`capabilities`**：`list_sessions` / `resume_session`（薄包装 `activate_session`）。
5. **`app_loop`**：`Request::ListSessions`/`ResumeSession` 两个 arm（穷尽 match 强制）+ 发 `Outbound::SessionList`/`SessionActivated`。
6. **TUI**：`COMMANDS` 12→13、`parse`、`PromptKind::PickSession`、`resolve_session_pick`、`run.rs` 的 fetch→modal 子循环、`app.rs::apply_outbound` 两臂、`replay.rs::history_lines`（重构 `apply_event` 共用映射）。
7. **启动期空屏修复**：`run_interactive` 起屏前若 `resume_path` 存在，经同一 `activate_session` 产出 `SessionActivated`，作为 `view₀` 之后的第一条 outbound。
8. **文档校正**：`docs/architecture.md:706`（11 → 13 条）、§14 命令表加 `/resume`；`wiring.rs:114-129` 的 as-built 说明。

### 测试要点（含变异靶，对应 `docs/design.md` §12）

- **失败原子性**（E2/约束 5）：目标不存在/不可读/JSONL 损坏 ⇒ `wiring.session_path()` 与 `session_messages()` **逐字节不变**。变异：把「先 open 成功后再换句柄」改成先换 ⇒ 红。
- **Esc 无副作用**（S2）：取消后不发任何 `Request`、不扫残留、session 不变。变异：取消分支触发 `ListSessions` 之外的动作 ⇒ 红。
- **空列表 E1**：`[]` ⇒ 「没有可恢复的会话」，**不弹空浮层**。变异：空列表仍开浮层 ⇒ 红。
- **当前会话 E3**：选中自身 ⇒ no-op（不 open、不 clear、不重发 `Activated`）。变异：无条件重放 ⇒ 红。
- **无凭证降级**（S3/D2）：目标 meta 的 provider 无 `auth_for` ⇒ 落到 state.json / 首个有凭证者，**切换照常成功**。
- **回放正确性**：tool_use/tool_result 按 id 配对；thinking 段按 meta 显隐。变异：`history_lines` 不按 id 配对、改成顺序配对 ⇒ 红。
- **回放顺序**：`SessionActivated` 必先于其后一切 `View`/事件。变异：延后发送 ⇒ 红。
- **退出打新 id**（R5）：切换后退出提示 `session: <新 id>`。变异：漏 `session_path.record` ⇒ 红。
- **共用路径等价性**：同一目标，启动期 `--resume=<id>` 与运行期 `/resume <id>` 产出**同一套** model / stats / history（质量目标「两处不分叉」）。变异：两处各写一份重建逻辑并改动其一 ⇒ 红。
- **`-p`/`--json` 不回归**：新 `Outbound` 变体不影响一次性模式（断言其 `Envelope` 消费路径未动）。
- **serde 向后兼容**：`SessionActivation` 新增字段带 `#[serde(default)]`，老 JSON 仍可反序列化。

---

## 9. 过度设计的风险自评

| 风险 | 严重度 | 缓解 |
|---|---|---|
| **`SessionCatalog` trait 只有一个实现** —— 在「静态组合优先」的项目里，trait 化单一本地目录实现可能被视为投机 | **高（最可能被批评处）** | 收窄为**泛型单实现接缝**（无 `Box<dyn>`、无注册表）；且**可无损降级**为模块自由函数（调用点接收 `&C`，换实现不改调用点）。裁决里已诚实标注「第二实现暂无」。 |
| **`Outbound` 两个新变体是协议膨胀** | 中 | 载荷都进同一个信息流（约束 9 单一出站写者），且**不影响 `-p`/`--json`**（它们消费 `Envelope`）。净增 2 个变体换「列表 / 回放」两条通路。 |
| **fetch→modal 的嵌套子循环**（UI 线程 `blocking_send` + 有界排空）最易出死锁/竞态 | 中 | 复用既有模态浮层的形状；**有界等待 + 超时/取消**兜底；Q1 已否决「塞 View」（那样虽省一次往返却带来每回合扫描）。 |
| **`SessionActivated` 一次塞 summary+thinking+history** | 低 | 为**原子性**付的价；避免「多条 outbound 之间的中间态」。 |
| **Q6 与 T7 的措辞冲突** | 低 | 已在 ADR 里显式声明「精化 T7：`/new` 语义不变，会话激活是一次具名转换」。 |

---

## 10. 附：与「最小垂直切片」的关系

`context.md` 建议先做「浮层 → 切换 → 不回放」的最薄切片。本候选**保留这条切法**：
`SessionActivated` 的 `history` 允许**先传空 `Vec`**（切换本身即可验证 D2/D4/原子性/退出 id），
`history_lines` 与启动期空屏修复作为**第二片**（同一载体、同一函数，无需改协议）。
故本方案**增量兼容最薄切片**——回放不是前置条件，而是同一接缝上的后续填充。
