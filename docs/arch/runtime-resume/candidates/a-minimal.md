# 候选 A：视图即一次性载荷（View-as-payload）——「最小复杂度」方案

> 目标：coding-agent 运行期 `/resume` → 浮层选会话 → 清屏回放目标历史、模型跟随目标 meta。
> 本方案的取舍原则：**能不新增协议变体就不新增，能复用既有浮层/回放机制就复用，能不搬 `Listing` 就不搬。**
> 与 `context.md` 的约束逐条对照，凡擦边/偏离处均**显式标注**。

---

## 0. 一句话骨架

**把「会话列表」和「历史回放」都当成 `CodingView` 上的两个一次性载荷字段**：`/resume` 先发一条请求让 app 把列表拍进视图 → UI 收到非空列表就开既有浮层 → 选中后发切换请求 → app 换会话、把目标历史转成 `TranscriptLine` 拍进下一次视图 → UI 收到非空回放就「清屏 + 重建 transcript」。

**协议层只加一个 `Request` 变体，`ys-protocol` 完全不碰 `Outbound`。** 会话列表不搬进协议层（Q1 的三条候选一条都不用）。

---

## 1. 模块划分与职责

| 模块 | 职责 | 新增/改动 |
|---|---|---|
| `ys-protocol::Request` | 只加 **1 个**变体 `Resume { target: Option<String> }` | 改（+1 变体 + serde/roundtrip） |
| `ys-tui-coding::view` | `CodingView` 加 **2 个一次性载荷字段**；新增 TUI 原生 `SessionEntry` | 改 |
| `ys-tui-coding::transcript` | 新增 `replay_lines(&[Message]) -> Vec<TranscriptLine>`（消息→显示行） | 改（+1 函数） |
| `ys-tui-coding::commands` | 命令表 +1（`resume`）、`parse` 分支、`PromptKind::PickSession` | 改 |
| `ys-tui-coding::prompter` | `resolve_prompt` 加 `PickSession` 臂 | 改 |
| `ys-tui-coding::app` | `App::new` 消费 `view.replay`；`apply_outbound(View)` 消费 replay / 列表 | 改 |
| `ys-tui-coding::run` | 事件循环在**排空 Outbound 之后**也检查 `pending_prompt` | 改（小） |
| `apps/coding-agent::wiring` | 新增 `resume_session(path)`（原子换会话）+ `sessions_dir()` getter | 改 |
| `apps/coding-agent::main` | 抽 `restore_model_from_meta(config, meta)`；启动期 resume 顺带填充 `view0.replay` | 改 |
| `apps/coding-agent::app_loop` | 处理 `Request::Resume`：列表 / 切换 / 失败原子性 / 统计播种 | 改 |
| `apps/coding-agent::view` | `build_view` 把两个载荷字段置空 | 改 |

**不新增 crate、不新增模块、不新增 Outbound 变体、不搬 `Listing`。**

---

## 2. 关键数据流

### 2.1 浮层列表（两跳：请求→视图→浮层）

```
TUI: 输入 /resume  → parse → Action::Request(Request::Resume { target: None })
  events.rs: send_request ──①──► app_loop
  app_loop: sessions::list(dir) → Vec<SessionEntry>（含 current 标记）
            → Outbound::View{ session_list: [...] }        （末尾标准 View 携带）
  run.rs: 排空 Outbound → apply_outbound(View)
            ↳ session_list 非空 ⇒ app.pending_prompt = Some(PickSession)
  事件循环紧接着的「pending_prompt 检查」⇒ run_prompt(PickSession)
            ↳ resolve_prompt 读 app.view.session_list 建选项
              用户 ↑↓ Enter ⇒ PromptOutcome::Request(Request::Resume { target: Some(id) })
              Escape       ⇒ Cancelled（零副作用，S2）
```

### 2.2 运行期切换 + 清屏回放

```
  events.rs: send_request ──①──► app_loop（回合中则天然排队，D4 免费）
  app_loop: Request::Resume { target: Some(id) }
    ├─ sessions::resolve(dir, id)               失败 ⇒ Output(错误)，会话不变（E2）
    ├─ sessions::read_meta(path) 读回首行
    ├─ main::restore_model_from_meta(&mut config, meta)   （D2；无凭证 ⇒ 返回 false，保持原 model）
    ├─ config.build_model() → Option<Box<dyn Model>>
    ├─ wiring.resume_session(path).await        先 open 成功才替换（约束 5，失败保持原状）
    ├─ wiring.set_model(built)
    ├─ stats = seed_stats(&wiring)              （Q4：tokens/turns 由 meta 播种）
    ├─ session_started = Instant::now()         （Q4：时长归零）
    ├─ session_path.record(wiring.session_path())（退出提示快照）
    └─ replay = ys_tui_coding::replay_lines(wiring.session_messages())
  ── 出站序列 ──►
    Outbound::View{ replay }       （仅此一次，内含目标全历史）
    Outbound::Output("Resumed …")  （既有的「命令输出走 Output」约定，落在回放之后）
    Outbound::View{ … }            （末尾标准快照，replay 已清空）
  UI: apply_outbound
    View{replay 非空} ⇒ transcript = replay（清屏 + 重建），follow=true、scroll=0、tool_index 清空、md_cache 失效
    Output            ⇒ transcript.push(System(notice))
```

> **顺序刻意是「回放 View → 通知 Output → 末尾 View」**：若沿用「Output 在前」，通知会被紧随的 replay 覆盖掉。回放单独走一条先发的 `View`，通知仍旧走 `Outbound::Output` —— 不违反「命令输出走 `Outbound::Output`」这条硬约定。

### 2.3 启动期 `--resume` 空屏问题（顺带修，D3 推论）

同一条通路：`run_interactive` 在 `build_view` 之后、spawn 之前，若 `resume_path.is_some()` 则
`view0.replay = replay_lines(wiring.session_messages())`；`App::new` 消费 `view.replay` 作为 `transcript` 初值。
⇒ 启动期 resume 与运行期 resume **共用同一个转换函数、同一条消费路径**，两处观感不可能分叉。

---

## 3. 核心接口签名

```rust
// crates/ys-protocol/src/request.rs
pub enum Request {
    // …
    /// 运行期恢复会话。`None` = 列出可选会话（app 回 `CodingView.session_list`）；
    /// `Some(id_or_path)` = 直接切换（`/resume <id>` 与浮层确认共用一条路）。
    Resume { target: Option<String> },
}

// crates/ys-tui-coding/src/view.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,          // sessions::resolve 接受的 id
    pub title: String,       // derive_title 结果
    pub mtime_unix: u64,     // 最近活动（Unix 秒）
    pub current: bool,       // 是否为当前会话（E3 标记 + 防误选）
}

pub struct CodingView {
    // …既有字段…
    /// 一次性回放载荷（TUI 原生类型）。非空 = 「清屏并用这些行重建 transcript」。
    /// **除 resume 响应外恒空** —— 不随每回合 `build_view` 重放。
    pub replay: Vec<TranscriptLine>,
    /// 一次性会话列表载荷。非空 = 「打开会话选择浮层」。**除列表响应外恒空**。
    pub session_list: Vec<SessionEntry>,
}

// crates/ys-tui-coding/src/transcript.rs（显示逻辑归 UI crate）
pub fn replay_lines(messages: &[Message]) -> Vec<TranscriptLine>;

// crates/ys-tui-coding/src/commands.rs
pub enum PromptKind { /* … */ PickSession }

// crates/ys-tui-coding/src/prompter.rs：resolve_prompt 的 PickSession 臂返回
//   PromptOutcome::Request(Request::Resume { target: Some(entry.id) })

// apps/coding-agent/src/wiring.rs
impl Wiring {
    /// 先 `JsonlSession::open(&path)`，**成功后**才替换 session / session_path（与 `new_session` 同不变量）。
    /// `sessions_dir` 为 `None`（一次性）时返回 `Ok(None)`。
    pub async fn resume_session(&mut self, path: PathBuf) -> Result<Option<PathBuf>, SessionError>;
    pub fn sessions_dir(&self) -> Option<&Path>;
}

// apps/coding-agent/src/main.rs
/// D2 的唯一实现：meta.provider 有凭证 ⇒ 落 api_base/api_key/model/provider，返回 true；
/// 否则**什么都不改**并返回 false（降级而非失败）。**启动期与运行期共用此函数。**
pub(crate) fn restore_model_from_meta(config: &mut Config, meta: &SessionMeta) -> bool;
```

---

## 4. 对 Q1–Q10 的逐条回答

| # | 答案 |
|---|---|
| **Q1 列表怎么到 TUI** | 走 **(b) 的变体**：不是塞进每回合 `build_view`（那会每回合扫目录，违反约束 11），而是**只在 `Resume{None}` 响应那一次**把 `Vec<SessionEntry>` 放进 `CodingView.session_list`。`Listing` **不搬进 `ys-protocol`**（也不搬 apps→协议），因为过线的不是 `Listing` 而是 TUI 原生的 `SessionEntry`。候选中 (a) 需新 Outbound 变体、(c) 违依赖方向，都不采纳。 |
| **Q2 回放载荷与转换位置** | 载荷 = `Vec<TranscriptLine>`（TUI 原生，走 `CodingView.replay`，**不新增 Outbound 变体**）。转换函数 `replay_lines` 放 **UI crate**（显示逻辑与 `summarize_tool_args`/`TranscriptLine` 同处，避免 app 侧复制一套 tool-result 回填语义）；**由 app 调用**。**顺带修启动期 resume 空屏**（同一条通路）。 |
| **Q3 支持 `/resume <id>` 直切** | **支持**——与 `/model` 的「无参浮层 / 有参直切」对称，且**零额外协议成本**（`Resume{Some(id)}` 本就是浮层确认要走的那条路）。 |
| **Q4 时长/统计是否归零/播种** | tokens/turns 由 `seed_stats(&wiring)` 从目标 meta 播种；`session_started = Instant::now()` **时长归零**。与启动期 resume、`/new` 三者同口径（时长属展示、不恢复）。 |
| **Q5 ADR-0026 决策 6 处置** | **新增 ADR 0033**（`runtime-resume-command.md`）显式声明**取代 0026 决策 6**，保留 0026 其余决策（默认新会话 / meta 首行 / 三模式统一持久化）。ADR 是历史记录，**不就地改写 0026**。同时复访 0026 决策 4：它对候选 B（`SessionCatalog`/`SessionPicker` 三 trait）的否决理由「决策 6 排除运行期 `/resume` ⇒ 无第二消费者」**前提已消失**；但本方案**仍不需要**那三个 trait（零 trait、零 registry），故复访结论 = **维持否决，但理由改为「本需求不需要它」**。 |
| **Q6 `thinking_visible` 是否跟随 meta** | **不跟随**，与 `/new` 同口径：UI 真值随进程，`CodingView.thinking_visible` 仍只被 `App::new` 消费一次、不被运行期快照覆盖。**这是与启动期 resume 的有意分叉**（启动期取 meta 初值，运行期切换不动），理由是「跟随」要打破既有「快照不覆盖 `App.thinking_visible`」不变量 = 新机制，与最小复杂度相悖。详见 §6 代价。 |
| **Q7 排序与呈现字段** | 沿用 `sessions::list` 的 **mtime DESC**、`derive_title`（50 字符截断）；选项行格式 `title  (id)  [current]`；**标出当前会话**（`SessionEntry.current`）以免 E3 误选。`message_count`/token **不显示**（`Listing` 刻意没有，不为此扩数据源，约束 13）。 |
| **Q8 排队期间 UI 反馈** | **什么都不显示**，与 `/model` 现状一致。切换请求落在信道①、回合结束后才被 `recv`（约束 10）——D4 天然成立，**不新增 busy 位、不新增 pending 提示**。代价：用户看不到「已受理」，见表末风险。 |
| **Q9 与退出回放的叠加** | **不特殊处理**。回放行就是普通 `TranscriptLine`；退出时 `exit_payload` 照常把整个 transcript 倾到 scrollback。进程内**不会重复**（回放是「替换」而非「追加」）；跨进程也不重复（旧进程已退出）。标记「回放行不计入退出回放」属额外机制，**不做**。 |
| **Q10 `context_window`/`compat` 跟随吗** | **两者都自动跟随，无需同刀解决**。① `compat`：`restore_model_from_meta` 会更新 `config.provider`，而 `build_model()` 用 `cfg.current_compat()`（约束 8 满足）。② `context_window`：`app_loop` **每回合开头** `agent.set_context_window(config.context_window())`，而 `config.model` 已被 D2 改写成目标模型 ⇒ 下一回合窗口即目标窗口（约束 7 在本路径上**已被既有机制化解**）。`compaction` 比例、`max_rounds` 与模型无关，冻结无碍。 |

---

## 5. 受哪些同类软件启发 / 明确不采纳什么

**采纳（仅取理念，不抄架构）：**
- **Claude Code**：运行期 `/resume` 开 picker、**不离开进程**；**加载失败时当前会话继续运行** ⇒ 本方案的失败原子性（E2，约束 5）。
- **Codex**：切 thread = 「激活新 channel」这一心智 ⇒ 本方案把它落到既有的「换 `Wiring.session` 句柄」（`/new` 的既有先例），而不是重建 runtime。
- **DeepSeek Harness**：`/resume` 无参 picker + `/resume <id>` 直切双形态 ⇒ Q3。
- **pi（TS）**：回放是「**清屏 + 由持久化条目重建**」而非事件重放 ⇒ 本方案的 `replay_lines(messages)`。
- **Codewhale（Rust）**：列表只读首行做成本优化 ⇒ 复用 `sessions::list` 的 **4 KiB 首行封顶**，不自造。
- **OpenCode 的坑**：列表**不得静默过滤/截断** ⇒ 本方案列 `sessions::list` 的全部条目，不设 `limit`。

**明确不采纳（依赖别家架构、YuShan 没有）：**
- **pi 的「工厂闭包整体重建 runtime」**：依赖其 DI 基础设施；YuShan 的 `Wiring` 持有 `Box<dyn EventSink>`（`ChannelSink` 的接收端在 app 线程），**无法重建 Wiring**（会需要第二个同源 sink）⇒ 只能「换 session 句柄」。
- **Codex 的 `ThreadEventStore` 事件重放**：YuShan 的历史是 `Message`（JSONL），**没有事件流可重放**；且事件重放会重演工具调用等副作用语义，与「只重建 UI」不同。
- **Claude Code 的 `sessions/{pid}.json` 会话指针**：本方案不引入「指针文件」概念（那正是其已知 bug 的来源）；当前会话就是 `Wiring` 手里那个句柄，单一真值。
- **pi 的「切换时 abort 当前回合」**：与用户已定 D4（忙时排队）冲突，不采纳。
- **新增 `Outbound::Replay` / `Outbound::SessionList` 变体**（context 曾提示「回放若走 Outbound 新变体，`-p`/`--json` 也得处理」）：**本方案不新增 Outbound 变体**。经核实 `-p`/`--json` 消费的是 `Envelope`（`consume_events`），**根本不 match `Outbound`**（`main.rs:1009` 的 Outbound 信道只喂 TUI）。

---

## 6. 实施步骤与测试要点

**建议切成两片，先合后分都能独立验证。**

**第 1 片（切换本身，验证 D2/D4/D1）**
1. `ys-protocol`：加 `Request::Resume { target: Option<String> }` + roundtrip 测试。
2. `wiring`：`resume_session` + `sessions_dir` getter（照 `new_session` 的「先 open 后替换」写）。
3. `main`：抽 `restore_model_from_meta`，启动期模型链改调它（行为不变，纯重构，先跑绿）。
4. `view`/`app_loop`/`commands`/`prompter`：命令、`PickSession`、列表载荷、切换臂、失败原子性。
5. `run`：排空 Outbound 后检查 `pending_prompt`。

**第 2 片（回放，验证 D3）**
6. `transcript::replay_lines` + `App::new`/`apply_outbound` 消费 replay。
7. `run_interactive` 启动期填充 `view0.replay`（顺带修空屏）。

**测试要点（对应 `docs/design.md` §12，标 ★ 者需变异测试）**
- ★ **失败原子性**：`Resume{Some("不存在")}` / JSONL 损坏 / 不可读 ⇒ 出 `Output` 错误、`message_count`/`session_path`/`model` **逐项不变**（变异：把 `resume_session` 改成「先替换后 open」⇒ 红）。
- **Esc 无副作用**：浮层取消后 session / view / transcript / config 全不变（S2）。
- **列表**：`Resume{None}` 的 View 里 `session_list` 与 `sessions::list` 逐项一致（顺序、title、`current` 标记）；**空目录 ⇒ 不弹浮层**、出「没有可恢复的会话」（E1）。
- **切换成功**：`message_count` = 目标条数、`session_path` 指向目标文件、`session_path.get()` 快照刷新、`stats`/`session_started` 播种（S1/S5/S6）。
- **回放**：`replay_lines` 对 User/Assistant/ToolUse/ToolResult 的映射与回填正确（乱序结果、失败结果）；长历史不 panic。
- **启动期回放**：`--resume` 时 `App::new().transcript` 非空（回归护栏：修掉「空屏」）。
- ★ **D2 降级**：目标 meta 的 provider 无凭证 ⇒ `restore_model_from_meta` 返回 false、切换照常成功、model 保持原值（S3）；meta 无 model ⇒ 不 panic（E8）。
- **模板/表完整性**：命令表 13 条；`test_command_table_shape` 同步（`docs/architecture.md:706` 的「11 条」一并校正为 13）。
- 测试隔离照旧：临时目录含原子序号 + `process::id()`；env 改写走 `env_lock`。

---

## 7. 本方案的已知代价 / 什么情况下会后悔

**已知代价**
1. **`CodingView` 语义被撑宽**：它名义上是「实时快照」，现在**偶尔**承载一次性大载荷（`replay` 可达整段历史）。读者容易误以为它每帧都重放 ⇒ 靠字段文档 + 「除响应外恒空」的约定兜住。
2. **浮层由「数据到达」触发**：`pending_prompt` 以往只由键盘 `dispatch` 设置，现在 `apply_outbound` 也会设置。这是一条**新的控制流形状**（数据驱动模态），已存在的 `pending_prompt.take()` 检查点需从「按键后」扩到「排空 Outbound 后」。
3. **`replay` 使 app 多克隆一份 `CodingView`**（末尾标准 View + 回放 View 各一），长会话下是一次性 O(历史) 内存拷贝；不缓存、不增量。
4. **回放不等于「重演」**：`Summary`（回合摘要行）与 `thinking` **本就不持久化**，回放里没有；工具结果的显示与 live 渲染可能微差。观感「像」重开该会话，但**不是逐像素一致**。
5. **Q6 的分叉**：运行期切换不随目标 meta 的 `thinking_visible`，与启动期 resume 不一致（有意，见 Q6）。
6. **Q8 无反馈**：忙时排队期间屏幕无任何提示，用户可能以为 `/resume` 没生效（与 `/model` 同病）。
7. **`Resume { target: Option<String> }` 把「查询」与「变更」合进一个变体**：语义上略糊（`None` 查、`Some` 改）；若将来要加「按路径直切 / 强制切换 / 分页」，这个 `Option` 大概率要拆成两个变体。

**什么情况下会后悔**
- 若「视图携带一次性大载荷」被后来者**当成通用模式**扩散（更多命令往 `CodingView` 塞大 payload）⇒ `CodingView` 退化成「什么都能装的信封」，快照语义彻底失效。
- 若**回放必须做到与 live 逐像素一致**（要回放 thinking / 回合摘要 / 事件级顺序）⇒ 数据源（JSONL 只存 `Message`）不支持，这条路会走死，得回去做「持久化事件流」。
- 若**出现 `Outbound` 的第二个消费者**（比如给 `-p`/`--json` 也加运行期能力，或换 GUI 前端）⇒ 「载荷塞进视图」这种隐式约定会立刻露出短板，届时更该有显式的 `Outbound` 变体。

---

## 8. 「本方案最可能被否掉的理由」

1. **架构纯洁性**：评审可能坚持「`CodingView` 是快照，一次性大载荷必须走**显式的 `Outbound` 变体**（`Outbound::Replay` / `Outbound::SessionList`）」，认为「把 `View` 当数据通道」是概念偷换（尽管它确实省掉了一堆 `-p`/`--json` 的连带改动——而经核实那两个模式其实不 match `Outbound`，所以显式变体的真实成本也没 context 猜的那么高）。
2. **数据驱动模态**：`apply_outbound` 里改 `pending_prompt` 被认为把 UI 控制流搞隐晦，评审可能更接受「app 直接把列表塞进 `CodingView`，由 TUI 在拿到列表时**显式**开浮层」的等价写法（同一机制、不同命名），或干脆要求两段式协议。
3. **Q6 的不一致**：D3 说「观感对齐 `--resume`」，而 `thinking_visible` 却不对齐——评审可能据此要求「运行期也跟随 meta」，从而逼出新机制。
4. **协议变体合并**：`Resume{Option}` 被指「查询与变更混装」，要求拆成 `ListSessions` + `ResumeSession` 两个变体（更啰嗦但更清）。
5. **Q8 的静默**：评审可能要求排队期至少给一条 pending 提示，从而引入本方案刻意回避的 busy 反馈机制。
