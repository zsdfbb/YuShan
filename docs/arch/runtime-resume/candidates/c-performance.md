# 候选 C（性能 / 资源优先）：运行期 `/resume`

> 立场：**极致压低扫描与渲染开销、避免一次性全量物化、必要时惰性与分页**。
> 前提：用户已定 D1–D4（不可推翻）。D3「清屏并回放」在**形态与预算**上由本候选主张。
> 本文件对应 `docs/arch/runtime-resume/context.md` 的 Q1–Q10，逐条给成本论证。

---

## 0. 一句话骨架

> **扫描按需、有界（top-K）；回放有界（尾 N 条 + 折叠），且把转换推到 app 线程、把渲染成本钉死在有界 transcript 上。**
> 运行期切换 = `Wiring` 抽出共用「meta→模型」链 + 复用 `Wiring::resume` 的「先 open 成功后替换」；窗口跟随复用 **app_loop 每回合已有的 `set_context_window`**，不新增机制。

---

## 1. 本候选盯住的两处真实成本（先量化，再设计）

### (a) 会话列表扫描

`sessions::list()`（`apps/coding-agent/src/sessions.rs:56`）每个 `*.jsonl` 做：`std::fs::metadata`（1 stat）+ `File::open` + `read`（封顶 4 KiB）。→ **每文件 ≈ 1 stat + 1 open + 1 read**。数量增长后是 N 次 open。

- 关键结构事实：**id 是 `{unix秒}_{纳秒9位}`**（`wiring.rs:209` `new_session_id`），字典序 ≈ 时间序。当前 `list()` 用 **mtime**（全量重写 ⇒ ≈「最近活动」），因此 **mtime 才是正确排序键，filename 只是代理**。
- 结论：**stat 必须全做**（拿 mtime）；**open/read 可以只做 top-K**。这是本候选扫描优化的全部。

### (b) 历史回放

- `JsonlSession::open`（`jsonl.rs:20`）→ `load_from_file` **逐行读完整文件**并把全部 `Message` 留在内存。切到目标会话后它成为活会话，**下一次回合要把全部消息发给模型** ⇒ **加载全量是强制的，不可省**。回放不该假装能省掉它。
- 真正可省的是 **TUI 侧**：`Message → TranscriptLine` 转换、以及 `build_text` 里 assistant 走 `md::render`（第三方 markdown 解析）+ `md_cache`（`cache.rs`）。
- **被忽视的放大器**：`draw_transcript` 每帧调 `Paragraph::line_count`（`draw.rs:146`）算总视觉行，且 `max_scroll_offset_at`（`draw.rs:129`）**再算一遍** ⇒ **每帧 O(transcript 视觉行数) × 2**。回放一个 500 条会话，其后**每一帧**都在为它付费，不只是首帧。**有界 transcript 是本候选的核心收益。**

> 先例：退出回放走 **raw 路径**（`transcript_plain_lines` / `exit_payload`，`run.rs:503`）—— 按 `\n` 切行、不进 `md_cache`。但那只在退出时发生一次；在屏 transcript 不能常驻 raw（会丢 markdown 渲染）。故本候选不把 raw 路径搬进在屏回放，而是**用预算（条数/字节上限）** 控制成本。

---

## 2. 模块划分与职责

```
UI(ys-tui-coding)                          app 线程（apps/coding-agent）
──────────────────                          ──────────────────────────────
/ /resume
  parse → Action::Request(Request::ListSessions)
  events dispatch: blocking_send(ListSessions)            ← UI 线程，无 runtime
  apply_outbound(SessionList) → 存 App.session_list
                              → pending_prompt = PickSession
  run.rs: 复用 draw::Modal::Select 渲染 → 选中 index → id
  blocking_send(Request::Resume { id })
                              ────────①───────►
                                            app_loop recv(ListSessions)
                                              spawn_blocking(sessions::list_capped(dir, K))
                                              send Outbound::SessionList { items, total }
                                            app_loop recv(Resume { id })
                                              capabilities::resume_session(...)   ← 唯一改动点
                                                ├ sessions::resolve(id) → path
                                                ├ wiring.resume_into(path)  ★先 open 成功后替换
                                                ├ resolve_model_from_meta()（与启动期同一份）
                                                ├ seed_stats / session_started / session_path.record
                                                ├ tail = messages[n-k..]      ★有界切片
                                                └ to_replay_lines(tail)       ★app 侧转换
                              ◄──────③─────────
  apply_outbound(Replay { lines, folded, thinking_visible })
    → transcript.clear() + md_cache.reset() + tool_index.clear() + scroll/follow 复位
    → push 折叠行（folded>0）+ extend(lines)
  apply_outbound(View) → 状态行更新（message_count = 目标会话全量）
```

**依赖方向**：`ys-tui-coding` 只消费 `ys-protocol` 的纯数据（`SessionItem` / `ReplayPayload` / `ReplayLine`），**不认识 `Session`**（编译器强制）。转换与扫描**全在 app 侧**（app 依赖 `ys-tui-coding`，可调 `summarize_tool_args`）。

---

## 3. 关键数据流与核心接口签名

### 3.1 ys-protocol（新纯数据类型 + 变体；**零 tokio**）

```rust
// crates/ys-protocol/src/request.rs  —— 新增两个 Request 变体
pub enum Request {
    // …既有 8 个…
    ListSessions,                 // 仅 TUI 产生；-p/--json 不产生（见 §7 纠正）
    Resume { id: String },        // 浮层选中 / 直接给出；解析归 app
}

// 新增两个 Outbound 变体（V 泛型不变）
pub enum Outbound<V> {
    // …既有 Event / View / Output / Quit…
    SessionList(SessionList),     // 一次性响应
    Replay(ReplayPayload),        // 一次性 bootstrap
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub mtime_secs: u64,   // Unix 秒；刻意不用 SystemTime（跨进程/契约更干净）
    pub is_current: bool,  // app 侧比较路径得出，UI 标灰（防 E3 误选）
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionList {
    pub items: Vec<SessionItem>,
    pub total: usize,      // total > items.len() ⇒ 已截断，UI 可提示「另有 N 个」
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayPayload {
    pub lines: Vec<ReplayLine>,
    pub folded: usize,             // 被折叠的早期消息条数（0 = 未折叠）
    pub thinking_visible: bool,    // 一次性 bootstrap，语义同 App::new 对 view 的消费
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReplayLine {
    User(String),
    Assistant(String),             // 源文本；渲染仍走 md（不搬 raw）
    Thinking(String),
    Tool { id: String, name: String, summary: String, result: Option<String>, success: bool },
    System(String),
}
```

### 3.2 apps/coding-agent

```rust
// sessions.rs —— 有界扫描（复用 read_first_line / FIRST_LINE_CAP，不复制 4 KiB 细节）
pub const LIST_CAP: usize = 200;
/// 只对 top-K 做 open/read；stat 全做以拿准 mtime。返回 (items, 目录内总数)。
pub fn list_capped(dir: &Path, cap: usize) -> (Vec<SessionItem>, usize);

// wiring.rs —— 先 open 成功后替换（保持 new_session 的失败原子性）
impl Wiring {
    /// 打开既有会话并替换句柄；**open 失败 ⇒ 状态不变**。成功后返回 meta 供模型解析。
    pub async fn resume_into(&mut self, path: PathBuf) -> Result<SessionMeta, SessionError>;
    pub fn sessions_dir(&self) -> Option<&Path>;   // 供 app_loop 定位目录
}

// capabilities.rs
/// 运行期切换：解析 → open（原子）→ 模型跟随 meta → 有界回放载荷。
/// 任一步失败 ⇒ 返回 Err(String)，会话与模型**都不变**。
pub async fn resume_session(
    wiring: &mut Wiring,
    config: &mut Config,
    state: &StateStore,
    sessions_dir: &Path,
    id: &str,
    replay_limit: ReplayLimit,     // { max_lines, max_bytes }
) -> Result<(Vec<String>, ReplayPayload), String>;

// main.rs —— 抽启动期模型优先级链（1107–1152）为共用函数，两处共用
pub(crate) fn resolve_model_from_meta(
    config: &mut Config,                // 内部读 config.registry
    meta: Option<&SessionMeta>,
    saved: &AppState,
);   // 语义：env 凭证（调用方先判 is_configured）> meta（须 auth_for 命中）> state.json > 首个有凭证者；无凭证降级不失败
```

### 3.3 ys-tui-coding

```rust
// commands.rs：COMMANDS 12 → 13（加 "resume"）；parse 的 arm：
"resume" => Action::Request(Request::ListSessions),

// commands.rs：新增 PromptKind 变体
pub enum PromptKind { Login, PickModel, PickSkill, PickSession }

// app.rs：新增一处状态 + 两个 apply 分支
pub session_list: Option<Vec<SessionItem>>,   // 收到 SessionList 后存，供 run.rs 选
// apply_outbound:
//   SessionList(l) => { self.session_list = Some(l.items); self.pending_prompt = Some(PickSession); }
//   Replay(p)      => self.apply_replay(p),

impl App {
    /// 清屏式回放：**必须先使 md_cache 失效**（见 §6 正确性），再填入。
    pub fn apply_replay(&mut self, p: ReplayPayload);
}

// cache.rs：新增（缺它则回放会命中陈旧渲染，见 §6）
impl RenderCache { pub fn reset(&mut self); }   // slots.clear(); streaming = None; scratch.clear()

// run.rs：PickSession 的解析 —— 复用 draw::Modal::Select 渲染，选中 index → id
// （resolve_prompt 只看 &CodingView，装不下列表；故 PickSession 走一小段专用逻辑，
//  复用 Modal 渲染、不新增渲染代码）
```

---

## 4. 成本模型与估算表

> 全部为**数量级估算**，非实测。假设与不确定度标在表下。符号：N=目录内 `*.jsonl` 数，K=LIST_CAP，meta 首行 ≈ 200 B。

### 4.1 列表扫描

| 场景 | syscall 数 | 读取字节 | 暖缓存耗时 | 冷缓存耗时 |
|---|---|---|---|---|
| **现状 `list()`**，N=100 | ~300 | ~20 KB | ~0.7 ms | ~18 ms |
| **现状 `list()`**，N=1000 | ~3000 | ~200 KB | ~7 ms | ~180 ms |
| **本候选 `list_capped`**（stat 全做 + top-K open），N=1000, K=200 | ~1200 | ~40 KB | **~2.5 ms** | **~60 ms** |
| 进一步（filename 预筛 + K open，**可选**），N=1000, K=200 | ~400 | ~40 KB | ~0.3 ms | ~10 ms |
| `-p`/`--json` | — | — | — | 不涉及（无 `/resume`）|

### 4.2 历史回放（尾 K 条）

| 项 | 无界（500 条 × 2 KB = 1 MB） | 本候选（k=80 条 / 256 KB 取小） |
|---|---|---|
| 加载会话（`JsonlSession::open`） | ~1 MB 反序列化，~5–20 ms（**强制、不可省**） | 同左（**强制**） |
| Message→line 转换（app 线程） | ~1 MB 遍历，~2–5 ms | ~0.25 ms |
| 首帧 md 渲染（assistant 段） | ~500 KB markdown，**~5–50 ms**（不确定度大） | ~50–150 KB，**~1–10 ms** |
| 常驻渲染产物（`md_cache` + `Text`） | ~5–10 MB | **<1 MB** |
| **每帧** `line_count` ×2（`draw.rs:146`+`:129`） | ~5000 视觉行 ⇒ **~3–10 ms/帧**（20 fps ≈ 6–20% CPU **常驻**） | ~800 行 ⇒ **~0.5–1.6 ms/帧**（≈1–3% CPU） |

**假设**：stat 暖 ~1–2 µs / 冷 ~30 µs；open+read ≤4 KiB 暖 ~5 µs / 冷 ~150 µs；`serde_json` ~1–3 µs/KB；`md::render` 估计 1–10 ms/100 KB（**最不确定的一项**，需实测）；`line_count` ~0.3–1 µs/行。
**不确定度**：绝对值为数量级；**相对关系（有界 vs 无界）稳**；`md::render` 常数可能偏大/偏小 5×。

**读表结论**：
1. **扫描**在真实规模（N=几十~低百）本就 <1 ms。**扫描优化只在 N≫100 才值得**；本候选只取「按需 + top-K」两条**结构性**收益，其余标为过早优化（§8）。
2. **回放**是每帧复利成本，**有界化收益是结构性的**，与 N 无关。

---

## 5. Q1–Q10 逐条回答（每条带成本论证）

| Q | 本候选答案 | 成本论证 |
|---|---|---|
| **Q1 列表怎么到 UI** | **(a) 按需往返**：`Request::ListSessions` → `Outbound::SessionList(Vec<SessionItem>)`；`Listing` 语义搬到 `ys-protocol`（纯数据、`mtime_secs: u64`、零 tokio）。 | 只在敲 `/resume` 时扫；**绝不进 `build_view`**（否则每回合扫一次，违反约束 11）。往返一次信道开销可忽略；扫描有界（§4.1）。 |
| **Q2 回放载荷与转换位置** | **app 侧转换 + 有界尾切片**，载于**新 `Outbound::Replay`**（**不放 `CodingView`**）。 | 放 `CodingView` 会被 `build_view` **每个请求重建**、且 UI `self.view = view` **整体 clone** ⇒ 每次请求复制回放数据（浪费）。`Outbound::Replay` 一次性、消费即丢。转换放 app 线程（回合间空闲）而非 UI 线程（还要驱动渲染）⇒ UI 只做 `extend`。 |
| **Q3 `/resume <id>` 直切** | **默认不加**（YAGNI）。若加：复用 `sessions::resolve`，**2 次 stat、零扫描**，比浮层更省。 | 直切只是**多一个入口**，无净成本、也非必需。标为可选；不是性能收益点。 |
| **Q4 时长/统计** | 照启动期口径：`TurnStats` 由 meta 播种（`seed_stats`），`session_started = now`（时长**不恢复**）。 | 全 O(1)。与「重启后 `--resume=<同一 id>`」逐字段一致（质量目标「语义一致」）。 |
| **Q5 ADR 处置** | **新增 ADR 取代 0026 决策 6**，并复访决策 4 的否决前提（其「无第二消费者」已不成立）。 | **纯文档成本，无运行期成本**。属设计前置（`AGENTS.md` 硬约束）。 |
| **Q6 `thinking_visible`** | 跟随目标 meta**一次**，经 `ReplayPayload.thinking_visible` 这个**一次性 bootstrap** 传入（语义同 `App::new` 消费 view），**不经 `Outbound::View`**（视图快照不得覆盖运行期真值 —— 既有不变量）。 | 0 成本；避开「两份真值漂移」。 |
| **Q7 排序与呈现** | 沿用 `list` 的 **mtime DESC**（对 top-K 精确排序）；显示 `时间 · 标题 · id` + **当前会话标记**（app 侧 `path` 比较）。**不显示消息数 / token**。 | 现成字段，0 额外 IO。**消息数需读全文 —— 那是 pi 的反面教材**，明确不做。`is_current` 防 E3 误选，成本一次路径比较。 |
| **Q8 排队期 UI 反馈** | 与 `/model` 一致（**静默排队**）。可选加一条一次性 hint 行 `正在切换到 <id>…`（0 成本）。 | D4 机制零新增（复用信道①）。回放何时到达受 D4 约束：**回合中敲 `/resume`，列表也要等回合结束才回**（app 线程阻塞在 `run_turn`，不能 `recv`）—— 这是 D4 的必然代价，记录而非对抗。 |
| **Q9 与退出回放叠加** | **不特殊标记**；退出时 `transcript_plain_lines` 照常倾出（含回放历史与折叠行）。 | 回放已**清屏**，不存在与切换前内容的重复。折叠行是 `System` 行，一并倾出（无害）。要「不重复倾倒」得加标记 → 属 UX 取舍、非性能，本候选不加（省状态）。 |
| **Q10 窗口 / compat 跟随** | **窗口自动跟随、无需新机制**：`app_loop` 每回合开头已有 `agent.set_context_window(config.context_window())`（`app_loop.rs:371`）。切换把 `config.{provider,api_base,api_key,model}` 改好并经 `config.build_model()`（factory 读 `cfg.current_compat()`）重建 model，窗口与 compat **下一次回合自动生效**。 | **0 新机制、0 额外成本**（重建 model 是纯构造，无网络）。这是本候选「复用既有缝、不新造轮子」的典型。 |

---

## 6. 正确性约束（性能优化的代价，必须配套）

1. **回放前必须 `md_cache.reset()`**。`RenderCache` 的键是 `(槽下标, src.len(), width)`，其前提是「transcript 只增不减、同槽内容只经尾部 `push_str` 增长」（`cache.rs` 模块注释）。**清屏回放会把不同内容写到同一槽下标**，若长度相同 ⇒ **假命中、显示旧内容**。`reset()` 是硬要求，不是优化。
2. **同批复位**：`transcript.clear()`、`tool_index.clear()`、`suppress_user_echo=0`、`pending_echo_texts.clear()`、`scroll_offset=0`、`follow=true`。漏掉 `tool_index` ⇒ 回放后的 ToolResult 可能回填到错误行。
3. **失败原子性**：`resume_into` 先 `JsonlSession::open` 成功后才替换 `session`/`session_path`（与 `new_session` 同规，`wiring.rs:114`）。`resolve`/`open` 失败 ⇒ 当前会话与模型**都不动**。
4. **顺序语义**（约束 9）：`Replay` 必须先于其后的一切事件/`View`。切换发生后 app 立即回到 `recv`（无回合在跑），新的回合事件只可能由**后续** `Prompt` 产生 ⇒ 天然有序；同一轮内 **先发 `Replay`、后发 `View`**。
5. **转换的工具配对**：`Message` 里 user 角色可能是纯 `ToolResult`（非用户文本，见 `sessions::user_text`）。`to_replay_lines` 必须按 id 配对 `ToolUse`/`ToolResult`，否则回放出现悬空 Tool 行。
6. **扫描不阻塞 runtime**：`sessions::list_capped` 是同步 std IO，经 `tokio::task::spawn_blocking` 跑（冷缓存可达数十 ms）。
7. **陈旧响应丢弃**：扫描期间用户按 Esc ⇒ 用一个「待扫描」代际标记，`SessionList` 迟到时不弹浮层（避免幽灵浮层）。

---

## 7. 对 context 的一处事实纠正

context「约束 15 / Q2」称「回放通路若走 `Outbound` 新变体，`-p`/`--json` 的消费循环也得处理它」。**实测不成立**：`-p`/`--json` 走 `run_print_mode` / `run_json_mode`，**直接消费原始 `Envelope`**（`main.rs:309` `rx: mpsc::Receiver<Envelope>`），**从不构造也不消费 `Outbound<CodingView>`**，也不调用 `app_loop::run`。
⇒ 新增 `Outbound::SessionList` / `Outbound::Replay` **不需要改 `-p`/`--json`**；新增 `Request` 变体只影响 TUI 的 `app_loop` `match`。本候选的改动面因此比 context 估计的**更小**。

---

## 8. 诚实标注：哪些是过早优化

| 项 | 判定 | 理由 |
|---|---|---|
| `LIST_CAP` 有界扫描 + 按需触发 | **值得做** | 结构性：保「启动零扫描」、避免 N 次 open 随规模线性膨胀。 |
| 扫描缓存（按目录 mtime 失效，重复 `/resume` 只 1 stat） | **过早** | 一次会话敲 `/resume` 0–3 次；N 小时省不到 1 ms。 |
| filename 字典序预筛（省掉全量 stat） | **过早且有害** | 依赖「文件名≈时间」，与「全量重写 ⇒ mtime≈最近活动」语义冲突；仅在 **N≫1000 且冷缓存**才有感。列为可选仅在 profile 证实后启用。 |
| 浮层分页 / 滚动惰性加载 | **过早** | K=200 已覆盖 K≤200；分页复杂度不值得。 |
| 回放有界化（尾 k 条 + 折叠） | **值得做** | **结构性、每帧复利**，与 N 无关（§4.2）。 |
| 回放前 `md_cache.reset()` | **必需（正确性）** | 不是优化，是防假命中。 |
| 回放走 raw 路径（跳过 md） | **不采纳** | 在屏 transcript 若 raw 会丢 markdown 渲染；用**预算**控制成本即可。 |
| 记忆映射 / 流式解析会话文件 | **过早** | `JsonlSession::open` 的全量加载对切换是强制的，无法省。 |
| 跨切换复用 md 渲染缓存 | **错误** | 会命中陈旧内容；必须 reset。 |

---

## 9. 实施步骤与测试要点

**分两片落地**（对应 context 的「最小垂直切片」建议，但顺序按成本收益排）：

- **片 1（切换 + 有界扫描）** —— 不含回放。
  1. `ys-protocol`：`Request::ListSessions` / `Request::Resume`、`Outbound::SessionList`、`SessionItem`（+ 单测 serde roundtrip，照 `request.rs` 既有 `assert_roundtrip`）。
  2. `sessions.rs`：`list_capped`（+ 单测：K 截断后按 mtime 排序、stat 与 open 计数上限）。仿既有 4 KiB 封顶测试。
  3. `wiring.rs`：`resume_into` + `sessions_dir()`（+ 单测：open 失败状态不变 —— 变异：改「先替换后 open」变红）。
  4. `main.rs`：抽 `resolve_model_from_meta`，**两处调用点共用**（+ 单测：同一 meta 下启动期与运行期得到同一 model）。
  5. `app_loop` / `capabilities`：`ListSessions` → `spawn_blocking` → `SessionList`；`Resume` → `capabilities::resume_session`。
  6. `commands.rs` / `app.rs` / `run.rs`：`/resume` 命令、`PickSession`、复用 `Modal::Select`。

- **片 2（有界回放）** —— 触碰渲染层。
  7. `ReplayPayload` / `ReplayLine`；`capabilities::resume_session` 产出尾切片 + 折叠计数。
  8. `App::apply_replay` + `RenderCache::reset`。
  9. 顺带修**启动期 `--resume` 空屏**（同一条通路：`App::new` 后按 `view.session_path` 触发一次 `Request::ReplaySessions`… 或启动期由 app 主动发 `Replay`）—— 与 D3 推论一致。

**测试要点（对应 `docs/design.md` §12 + 变异）**：
- 切换失败保持原状（变异：先替换后 open）。
- Esc 取消无副作用（不扫描残留、不改 session、不发 `Resume`）。
- meta 无凭证 → 降级不失败（复用链路单测）。
- 切换后退出提示打新 id（`SessionPathHandle` 刷新）。
- **回放正确性**：`md_cache.reset` 缺失的变异 —— 回放前后同槽同长不同内容 ⇒ 断言画面为新内容（此变异必须变红）。
- **有界性**：构造 k+ 条消息，断言 `ReplayPayload.lines.len() <= max_lines` 且 `folded == n - k`。
- 测试隔离：临时目录名含**进程内原子序号 + `process::id()`**；env 改写走 `env_lock()` + `EnvRestore`。

---

## 10. 自评：为省成本牺牲了哪些体验

1. **浮层只列最近 K 个会话**（N>K 时更早的不显示，`SessionList.total` 提示「另有 N 个」）。用户需靠 `--resume=<id>`（或将来 `/resume <id>`）到达更早会话。
2. **回放只显示尾部**，更早历史折叠成一行摘要，**屏内翻不到**（要看全文得 `/export` 或开文件）。这是最大牺牲，直接来自「有界 transcript 换来每帧成本恒定」。
3. **切换后仍在屏的是「渲染后的 tail」**，非完整历史；与「打开那个会话」的**观感在长会话上不完全等价**（短会话无差别）。
4. **列表信息少**（无消息数/预览/分支），因读全文成本高。
5. **退出 scrollback 含回放历史与折叠行**（不区分「本机产生」与「回放得到」）。
6. **回合进行中敲 `/resume`，连列表都要等回合结束**（D4 + app 线程阻塞在 `run_turn` 的直接后果；本候选只是记录，不绕过）。

---

## 11. 风险

- **`md_cache.reset` 漏调** ⇒ 回放显示陈旧内容（静默、难查）。必须有变异测试钉住。
- **`resolve_model_from_meta` 抽取导致两处语义漂移**（启动期 vs 运行期）——需同一份单测覆盖两调用点。
- **`ReplayLine` 转换的工具配对**写错 ⇒ 悬空/错位 Tool 行。
- **有界回放的 K 选得过小** ⇒ 用户以为「历史丢了」（需清晰的折叠提示文案）。
- **`spawn_blocking` + 代际标记**若缺失 ⇒ 冷缓存卡顿或幽灵浮层。
- **`Outbound` 变体扩张**使 `V` 泛型 derive 的表面变大；`SessionItem`/`ReplayLine` 必须 `PartialEq + Serialize + Deserialize`（照协议层既有范式）。
