# 运行期 `/resume`（切换到指定 session）— 设计

> 产出：`/arch-design`。上游：`./context.md`（`/arch-explore`）。
> 决策记录：`docs/adr/0033-runtime-resume-command.md`。
> 候选方案原文：`./candidates/a-minimal.md`、`./candidates/b-extensible.md`、`./candidates/c-performance.md`。

---

## 1. 结论

**在 `ys-protocol` 加两个请求、两个出站变体；把「激活一个会话」抽成启动期 `--resume` 与运行期 `/resume` 共用的 app 侧能力；UI 侧只做「清屏 + 由 `Vec<Message>` 重建 transcript」。**

一句话的数据流：

```
/resume ──► Request::ListSessions ──► app 扫目录 ──► Outbound::SessionList
       ──► 浮层选择（Esc 取消）──► Request::ResumeSession{target}
       ──► app: 换 Wiring.session + 跟随 meta 的模型 + 播种统计
       ──► Outbound::SessionActivated{summary, thinking_visible, history}
       ──► UI: transcript.clear() + md_cache.reset() + 回放 + 退出提示快照刷新
```

**同时修掉启动期 `--resume` 的空屏**（`App::new` 的 transcript 恒空）—— 同一通路、同一约定。

**这不是「多一个命令」，而是推翻 ADR-0026 决策 6**（「不做运行期 `/resume`」）—— 见 §6 与 ADR-0033。

---

## 2. 候选对比

### 2.1 三个候选的一句话

| 候选 | 骨架 | 出处 |
|---|---|---|
| **A 最小复杂度** | 列表与回放都做成 `CodingView` 上的**一次性载荷字段**；协议只加 1 个 `Request::Resume{Option<String>}`，`Outbound` 一字不动，`Listing` 不搬迁 | `candidates/a-minimal.md` |
| **B 可扩展优先** | 「激活会话」抽成**共用能力路径** `activate_session`；列表与历史经 `ys-protocol` **纯数据**过；复活 `SessionCatalog`/`SessionPicker` 为具名接缝 | `candidates/b-extensible.md` |
| **C 性能优先** | **扫描有界（top-K）+ 回放有界（尾 N 条 + 折叠）**；转换推给 app 线程，把每帧渲染成本钉死 | `candidates/c-performance.md` |

### 2.2 对比矩阵

| 维度 | A 最小复杂度 | B 可扩展优先 | C 性能优先 |
|---|---|---|---|
| **实现复杂度** | **最低**：无新 `Outbound` 变体、无新协议类型；但把「消息」塞进「快照」类型，类型语义变浑 | 中：新增 `session.rs` 纯数据类型 + 2 个 `Outbound` 变体 + 1 个 trait；**不改 `-p`/`--json`** | 中高：同 B 再加 top-K 扫描与有界回放 + `RenderCache::reset` |
| **性能特征** | 全量回放：每帧 `line_count` 随 transcript 线性增长（500 条时约 3–10 ms/帧，C 的估算，**最不确定项**） | 同 A（全量） | **每帧成本恒定**；但冷启动多一次 `stat` 全扫（N=1000 约 60 ms 冷） |
| **演进灵活性** | 低：`Resume{Option}` 把查询与变更混装（A 自评「将来大概率要拆」）；一次性字段会不断往 `CodingView` 上加 | **高**：接缝明确、扩展点列了预期消费者；但 `TitlePolicy` 仍属投机 | 中：为「有界」写死的 K 会成为新常量要调 |
| **运维负担** | 低 | 低 | 低；但「只回放尾 N 条」会引出「我的历史呢」的支持问题 |
| **风险** | `View` 当数据通道的隐晦性；数据驱动模态改控制流 | 单实现 trait 被判投机（静态组合优先的项目尤其敏感） | 有界回放与 D3 的「切过去=打开该会话」语义**冲突**；`md_cache` 失效漏调=静默显示旧内容 |

### 2.3 逐项裁决

| 议题 | A | B | C | **裁决** | 理由 |
|---|---|---|---|---|---|
| 列表/回放的载荷载体 | `CodingView` 字段 | `Outbound` 变体 | `Outbound` 变体 | **`Outbound` 变体** | `CodingView` 自我定义是「**快照**」（`crates/ys-tui-coding/src/view.rs:41`）；一次性消息不是快照。`thinking_visible` 不是反例 —— 它是**启动初值**，仍派生自实时状态（`view.rs:120`），与「一次性载荷」是两回事 |
| 回放载荷类型 | `Vec<TranscriptLine>` | `Vec<Message>` | `Vec<ReplayLine>` | **`Vec<Message>`** | 显示知识归 UI：`Message → TranscriptLine` 的映射 `apply_event` 里已有，UI 侧重构共用即可；`Message` 已是 `ys-core` 的 serde 纯数据，无需新类型 |
| 回放范围 | 全量 | 全量 | 尾 N 条 + 折叠 | **全量，但把「有界」立为实测后的预案** | D3 要的是「切过去就等于打开那个会话」；C 的每帧成本是真风险，但**先量再优化**（见 §5 不变量 I7） |
| 会话列表范围 | 全量 | 全量 | top-K + 「另有 N 个」 | **全量** | OpenCode 两个 issue 的根因就是 picker 静默过滤/截断；`list()` 本就是 `stat` + 首行 4 KiB 封顶，量级可控 |
| `SessionCatalog` trait | 不收 | 收窄复活 | 不收 | **不收** | trait 服务**多实现**，不是多消费者。B 自己承认「第二实现=投机」。维持 ADR-0026 决策 4 的否决，但把「前提之一已变化」记进 ADR-0033 |
| `Request` 形态 | `Resume{Option}` | 拆两个 | 拆两个 | **拆两个** | 「查询」与「变更」混装，A 自评将来要拆；现在就拆的成本≈0 |
| Q6 `thinking_visible` | 不跟随 | 跟随 | 跟随 | **跟随** | D3 要与 `--resume` 观感一致；用**一次性载荷**（不经快照）传递，不破 TUI「运行期真值不属 app」的不变量 |
| Q3 `/resume <id>` | 支持 | 支持 | 不做 | **支持** | 零额外成本（同一个 `resolve`），与 `/model` 的「无参浮层 / 有参直切」对称 |
| Q8 排队期反馈 | 静默 | 静默 | 静默 | **静默** | 与 `/model` 一致，零新增机制 |

**被吸收的三个具体发现**（都写进了 §5 不变量）：

- **C**：`RenderCache` 的合法性前提被回放打破 ⇒ 必须显式失效。
- **A**：出站**顺序**成为不变式（回放 → 通知 → 快照）。
- **A**：`pending_prompt` 的检查点必须从「按键后」扩到「排空 Outbound 后」。

---

## 3. 推荐方案

### 3.1 模块划分与职责

| 层 | 职责 | 不做什么 |
|---|---|---|
| `ys-protocol` | 两个新 `Request`、两个新 `Outbound`、纯数据类型 `SessionSummary`/`SessionActivation` | 不认识任何 TUI 类型；零 tokio |
| `apps/coding-agent` | 扫目录、解析目标、**原子切换** `Wiring`、跟随 meta 的重建模型、播种统计、刷新会话路径快照 | 不 print；不做会话生命周期管理（archive/prune） |
| `ys-tui-coding` | 命令表与解析、选择器浮层、`Message → TranscriptLine` 回放映射、清屏与缓存失效 | 不读 `Session`/文件系统；不认识 `Agent` |

**`sessions.rs` 保持模块函数集**（沿用 `list/resolve/read_meta`），不抽 trait —— 但把「运行期」作为**第二个调用者**接入，使该模块从「启动期专用」升格为「会话目录视图」。

### 3.2 协议层接口

```rust
// crates/ys-protocol/src/session.rs（新；纯数据，零 tokio）
/// 会话目录中的一个条目 —— 列表**只读首行**（4 KiB 封顶）得到，不含消息体。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub title: String,
}

/// 一次**成功的**会话激活 —— 三样东西**同时**生效，故装在一个原子单元里。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionActivation {
    pub summary: SessionSummary,
    /// 目标会话 meta 的 thinking 开关（照启动期 `--resume` 的初值语义）。
    pub thinking_visible: bool,
    /// 目标会话的完整历史 —— UI 侧负责转成 transcript 行。
    pub history: Vec<Message>,
}

// crates/ys-protocol/src/request.rs
pub enum Request {
    // …既有 8 个…
    /// 列出既有会话（`/resume` 无参）。只读元数据，不加载消息体。
    ListSessions,
    /// 切换到既有会话。`target` 交给 app 侧 `sessions::resolve` 解析（id 或路径）。
    ResumeSession { target: String },
}

pub enum Outbound<V> {
    // …既有 4 个…
    /// 会话列表（`Request::ListSessions` 的响应）。空列表也要发 —— 让 UI 能提示「没有可恢复的会话」。
    SessionList(Vec<SessionSummary>),
    /// 会话已激活。UI 见它即「清屏 + 回放」。
    SessionActivated(SessionActivation),
}
```

**为什么 `SessionActivated` 把三样装在一起**：模型跟随、thinking 跟随、历史回放必须**同时**生效 —— 拆成三条消息就会出现「模型换了但屏上还是旧会话」的中间态。

### 3.3 关键数据流

```
①  /resume（无参）
    commands.rs  → Action::Request(Request::ListSessions)
    app_loop     → capabilities::list_sessions(dir) → Outbound::SessionList(items)
    App          → 非空 ⇒ 开 PickSession 浮层；空 ⇒ transcript 提示「没有可恢复的会话」

②  浮层选中 / `/resume <id>`
    prompter     → Request::ResumeSession { target }
    app_loop     → capabilities::resume_session(..)
                     ├─ sessions::resolve(dir, target)        （未命中 ⇒ Err，不改状态）
                     ├─ Wiring::resume_into(path)             （先 open 成功后才替换）
                     ├─ resolve_model_from_meta(..)           （启动期同一条优先级链）
                     ├─ stats.seed_from_meta(meta)
                     └─ session_started = Instant::now(); session_path.record(..)
    out          → Outbound::SessionActivated{..}             ← 必须先于下面两条
                 → Outbound::Output(通知文本)
                 → Outbound::View(build_view(..))            （新快照：新 session_path / 模型 / 统计）
    App          → transcript.clear() + md_cache.reset() + 回放 history + follow = true
```

### 3.4 app 侧改动

| 位置 | 改动 |
|---|---|
| `main.rs:1104-1152` | **抽出** `resolve_model_from_meta(cfg, meta, saved_state) -> bool` —— 启动期与运行期共用同一条优先级链（env 凭证 > meta > state.json > 首个有凭证者），杜绝两处漂移 |
| `wiring.rs` | 新增 `resume_into(&mut self, path) -> Result<SessionMeta, SessionError>`：**镜像 `new_session` 的「先 open 成功后才替换」**（`wiring.rs:114-129` 的不变量） |
| `wiring.rs` | 暴露 `sessions_dir()`（`resume_into` 需要） |
| `capabilities.rs` | `list_sessions(&Path) -> Vec<SessionSummary>`；`resume_session(&mut Config, &mut Wiring, &StateStore, &Path, &str) -> Result<(Vec<String>, SessionActivation), String>` |
| `app_loop.rs` | 两个 `match` 臂；`session_started` 归零、`session_path.record`、`stats` 播种；**出站顺序固定为 Activated → Output → View** |

### 3.5 UI 侧改动

| 位置 | 改动 |
|---|---|
| `commands.rs` | `COMMANDS` 12 → 13（`resume`，`arg_hint = "[id]"`）；`parse` 分支：无参 → `Request(ListSessions)`，有参 → `Request(ResumeSession{target})`；`PromptKind` + `PickSession` |
| `app.rs` | `pending_sessions: Option<Vec<SessionSummary>>`（浮层数据，**不放 `CodingView`**）；`apply_outbound` 两个新臂；新增 `apply_activation(..)` 做清屏 + 失效 + 回放 |
| `transcript.rs` | `pub fn replay_lines(&[Message]) -> Vec<TranscriptLine>` —— 与 `apply_event` 的映射**共用**同一套规则，避免「实时显示」与「回放显示」两套 |
| `cache.rs` | `RenderCache::reset()`（或 `sync_len(0)` 等价的显式失效）—— 见不变量 I6 |
| `run.rs` | **扩宽 `pending_prompt` 检查点**：现在只在 `poll()` 分支内检查（`run.rs:175-190`），要提到「排空 Outbound 之后」也检查，否则浮层要等用户再敲一个键才弹 |
| `prompter.rs` | `PickSession` 的选择器入口（与 `resolve_prompt` 同构，但候选来自 `pending_sessions` 而非 `view`） |

---

## 4. Q1–Q10 裁决

| # | 问题 | 裁决 | 理由（未选项见 §2.3） |
|---|---|---|---|
| **Q1** | 列表数据怎么到 TUI | **按需往返**：`Request::ListSessions` → `Outbound::SessionList`，`SessionSummary` 进 `ys-protocol` | 不塞 `CodingView`（快照语义 + `build_view` 每请求重建）；不 TUI 自扫（破坏依赖方向，且要复制 `sessions.rs` 的 4 KiB 封顶细节） |
| **Q2** | 回放载荷与转换位置 | **`Outbound::SessionActivated.history: Vec<Message>`，UI 侧转换**；**顺带修启动期空屏** | 显示归 UI；`Message` 已是 serde 纯数据。启动期 resume 复用同一载荷：`view0` 之前把 history 放进一条 `SessionActivated` |
| **Q3** | 是否支持 `/resume <id>` | **支持** | 与 `/model` 双形态对称；解析复用 `sessions::resolve`，零额外成本 |
| **Q4** | 统计与时长 | **照启动期**：`stats` 由 meta 播种、`session_started` 归零 | 「等价于重启打开该会话」是 D2/D3 的立论基础，不允许分叉 |
| **Q5** | ADR-0026 决策 6 处置 | **新增 ADR-0033 显式取代决策 6**，保留 0026 其余；同时记录决策 4 的否决前提已部分变化（但本轮**仍不收 trait**） | ADR 是历史记录，不就地改写 |
| **Q6** | `thinking_visible` | **跟随 meta**，经 `SessionActivated` 一次性传递 | 启动期跟随（`view.rs:124`）、`/new` 不重置（T7）—— 运行期切换**不是** `/new`：它换的是整个会话，与启动期同构。一次性载荷**不**走快照，故不破「快照不覆盖 `App.thinking_visible`」 |
| **Q7** | 列表排序与字段 | mtime DESC（沿用 `list`）；显示 `title`（`derive_title` 50 字符）+ 相对时间 + id；**当前会话标 `[current]`**；**不静默过滤** | 避免 E3 误选；OpenCode 的教训 |
| **Q8** | 排队期反馈 | **静默**（与 `/model` 一致） | D4 由信道①天然成立，零新增机制。**已知代价**：回合进行中连**列表**都拿不到（app 线程阻塞在 `run_turn`），浮层要等回合结束 |
| **Q9** | 与退出回放的叠加 | 不特殊标记 | 回放前已 `clear()`，退出时倾倒的就是当前 transcript，不会重复 |
| **Q10** | `context_window` / `compat` 跟随 | **自动跟随，零新机制** | 已核实：`app_loop.rs:371` **每回合** `set_context_window(config.context_window())`（注释「唯一调用点，结构上不可能漏」，测试 `context_window_is_recomputed_after_model_switch` 钉住）；`compat` 随 `config.provider` 经 `build_model()` 重建。⇒ `context.md` 约束 7/8 在**本路径上不成立**，无需专门处理 |

---

## 5. 不变量与测试要点

| # | 不变量 |
|---|---|
| **I1** | **失败不改状态**：`resolve` / `open` / 模型解析任一步失败 ⇒ 当前会话、transcript、模型**全不变**，只回一条错误 `Output`。照 `Wiring::new_session` 的「先 open 成功后替换」 |
| **I2** | **出站顺序固定**：`SessionActivated` → `Output` → `View`。顺序反了，通知会被回放的清屏吃掉 |
| **I3** | **会话路径快照必须刷新**（`SessionPathHandle.record`）。Claude Code issue #37737 的教训：切换后指针文件不更新 ⇒ 两个进程写同一会话、消息链分叉 |
| **I4** | **空列表不弹空浮层**，只提示。对齐启动期 `resolve_resume` 的空列表分支（`main.rs:664-666`） |
| **I5** | **列表不静默过滤/截断**（OpenCode 两个 issue 的根因） |
| **I6** | **回放必须显式使 `RenderCache` 失效**。`cache.rs` 的键是 `(槽下标, src.len(), width)`，其合法性由 `debug_assert!` 钉在「transcript **只增不减**」上；回放是本 app **第一个**清屏/缩短 transcript 的操作 ⇒ 不失效就会**假命中、显示上一个会话的旧内容**。⇒ 本需求同时要**修订 `cache.rs` 的模块头不变量**（`/new` 不触；`/resume` 触） |
| **I7** | **回放全量是默认，有界是预案**：先实测「500 条 transcript 的每帧 `line_count` 成本」，再决定是否引入尾切片。若引入，**必须显式提示**被折叠了多少（I5） |
| **I8** | **`/resume` 后 `/new` 行为不变**：`/new` 仍只换 session 句柄、不清 transcript（既有语义不动） |

**测试要点**（对应 `docs/design.md` §12）：

- 变异测试 I1（把「先 open 成功后替换」改成先替换 ⇒ 必须变红）。
- 变异测试 I6（删掉 `md_cache.reset()` ⇒ 必须先加载会话 A、再切到 B、断言屏上不出现 A 的文本）。
- I2 顺序（断言出站序列）。
- E2 加载失败保持原状；E3 选中当前会话；E1 空列表；S3 meta 无凭证降级。
- 端到端：切换后继续提问 → 新消息 append 进**目标文件**；退出 `session: <id>` 是目标 id。

---

## 6. 实施步骤（建议切片）

1. **协议 + app 侧切换（无回放）**：加两个 `Request`、`Outbound::SessionActivated` 的 `summary`/`thinking_visible` 先走通，`history` 留空；UI 只清屏 + 提示。**这一片就验证了 D2/D4 的全部语义**，且「切换后屏幕空」与现状启动期 resume 观感一致，不构成回归。
2. **回放通路**：`replay_lines` + `md_cache.reset()` + 启动期 `--resume` 复用（顺带修空屏）。
3. **选择器浮层**：`PromptKind::PickSession` + `run.rs` 检查点扩宽 + Esc 取消。
4. **ADR + 文档同步**：ADR-0033；`architecture.md` §14 命令表（11 → 13 条，顺带修既存的 11/12 偏差）、§12 会话落盘节加运行期切换；`context.md` 的约束 7/8 标注「已被既有机制化解」。

---

## 7. 已知代价与遗留

- **回合进行中拿不到列表**（Q8）：app 线程阻塞在 `run_turn`，连 `SessionList` 都要等回合末。若将来要让浮层「立刻弹」，需要独立的列表服务任务 —— **本轮不做**。
- **回放 ≠ 重演**：`Summary`（压缩摘要）与 thinking 内容本就不持久化，回放里没有；与 live 观感不逐像素一致。
- **`CodingView` 与「会话列表」的生命周期分离**：列表暂存在 `App.pending_sessions`，与 `view` 不同源；打开浮层期间 `/resume` 再次触发需定义覆盖语义（建议：忽略/替换）。
- **会话目录无跨项目分组**：`SessionMeta` **没有 cwd 字段**（`meta.rs`），故不同项目的会话会混在一个列表里 —— pi/Codewhale 都有 workspace 分组，YuShan 没有。**这是本轮暴露的一个新的、独立的设计缺口**，不在本需求范围内。
- **多进程并发写无锁**（`session-default-new` 既有缺口）：切到别的进程正在写的会话有整文件重写丢更新的风险。
- **`SessionCatalog` trait**：本轮不收；若将来出现**第二实现**（远程会话后端、归档目录），再按 B 的形态收窄引入。
- **有界回放**：见 I7，待实测。
