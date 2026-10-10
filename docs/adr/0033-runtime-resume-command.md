# 0033 — 运行期 `/resume`：切换到指定会话（取代 0026 决策 6）

## Status

accepted（2026-10-10）

**取代**：`0026-session-default-new-and-resume.md` 的**决策 6 后半句**（「**不做运行期 `/resume`**（无中途换会话）」）
及其 Considered Options 中「运行期 `/resume` 切换会话（CodeWhale 形态）」一条。
0026 的**其余决策全部保留**（默认新会话、meta 首行、三模式统一持久化、`/new` 惰性不预建文件、标题派生…）。

同时记录：0026 决策 4 否掉「候选 B（`SessionCatalog` / `SessionPicker` / `TitlePolicy` 三 trait）」的理由之一是
「决策 6 排除运行期 `/resume` ⇒ 无第二消费者」。**该前提已部分消失**（第二消费者今天就有：运行期 `/resume` 与
启动期裸 flag 的 `list()`）。本轮**仍不收 trait** —— 见「决策 6」与「Considered Options」。

## 背景

- **需求（原话）**：「提供一个 coding-agent 运行期间的 resume 命令。命令是直接切换到指定的 session」。
  经澄清，入口形态为**浮层列出现有会话、选一个**（Esc 取消）。
- **现状**：运行期唯一的会话动作是 `/new`（换到**全新**会话，`Wiring::new_session`）。
  `Request` / `Outbound` 里没有任何加载既有会话的变体；`sessions::{list, resolve}` 只被
  `main.rs` 的**启动期** `resolve_resume` 使用。恢复既有会话的能力**只存在于启动期 CLI flag**。
- **既有设计明确否决过**：0026 决策 6 与其 Considered Options 逐条否决 —— 本轮是**推翻**，不是补漏。
  按 `AGENTS.md`「新增代码违反不变量算设计偏离，需先改设计文档」，故先立本 ADR。
- **一条被顺手修掉的既有缺陷**：启动期 `--resume` **根本不在屏幕上回放历史** ——
  `App::new` 的 transcript 恒为 `Vec::new()`（`crates/ys-tui-coding/src/app.rs:87`），
  `CodingView` 只带 `message_count`。恢复后用户看到的是**空屏**。本需求需要一条
  「会话历史 → transcript」的通路，启动期与运行期共用同一条，缺陷随之消失。
- **约束**：`ys-tui-coding` 不依赖 `ys-runtime`（编译器强制）；UI 线程必须无 runtime 上下文；
  `ys-protocol` 零 tokio；app 侧对 `Request` 的 `match` 穷尽；命令输出走 `Outbound::Output`（绝不 print）。

## 决策

1. **协议层加两个请求、两个出站变体**（`crates/ys-protocol`）：
   - `Request::ListSessions` / `Request::ResumeSession { target: String }` —— **查询与变更分开**
     （不合并成 `Resume { Option<String> }`：两种语义的生命周期与失败模式不同）。
   - `Outbound::SessionList(Vec<SessionSummary>)` / `Outbound::SessionActivated(SessionActivation)`。
   - 新纯数据类型 `SessionSummary { id, path, mtime, title }`（从 `apps/.../sessions.rs` 的 `Listing` 升格）与
     `SessionActivation { summary, thinking_visible, history: Vec<Message> }`。
   - **一次性载荷走 `Outbound`，不放 `CodingView`**：`CodingView` 自我定义是「显示**快照**」，
     而列表与历史是**消息**。（`CodingView.thinking_visible` 不是反例 —— 它是**启动初值**，仍派生自实时状态。）
2. **「激活一个会话」是启动期与运行期共用的能力**：把模型恢复的优先级链
   （env 凭证 > meta > state.json > 首个有凭证者）从 `main.rs` 抽出为共用函数；
   `Wiring` 新增 `resume_into(&mut self, path) -> Result<SessionMeta, _>`，
   **镜像 `new_session` 的「先 open 成功后才替换」**（失败 ⇒ 当前会话完好无损）。
3. **模型与 thinking 跟随目标会话 meta**（`SessionActivation` 一次性传递）。
   运行期切换**不等同于 `/new`**：`/new` 换的是「同一会话谱系里的下一段」，切换换的是**整个会话**，
   与「重启后用 `--resume` 打开它」同构 —— 观感必须一致，否则同一会话两处语义分叉。
   一次性载荷**不**经快照，故不破「快照不覆盖 `App.thinking_visible` 运行期真值」。
4. **历史回放 = 清屏 + 由持久化 `Vec<Message>` 重建 transcript**（不是事件重放 —— 与 pi、Codewhale 一致）。
   转换 `Message → TranscriptLine` 归 **UI 侧**，与 `apply_event` 的映射共用同一套规则。
   回放**默认全量**（要的是「切过去就等于打开那个会话」）。
5. **回放前必须显式使 `RenderCache` 失效**。`cache.rs` 的槽位键是 `(槽下标, src.len(), width)`，
   其「长度相等即内容相同」的合法性由 `debug_assert!` 钉在「**transcript 只增不减**」上；
   回放是本 app **第一个**清屏/缩短 transcript 的操作，不失效就会**假命中、显示上一个会话的旧内容**。
   ⇒ 同时修订 `cache.rs` 的模块头不变量（`/new` 不触、`/resume` 触）。
6. **不引入 `SessionCatalog` / `SessionPicker` trait**：trait 服务**多实现**，不是多消费者；
   本轮的第二消费者（启动期 + 运行期）用**模块函数**即可。`sessions.rs` 升格为「会话目录视图」，
   出现第二实现（远程后端、归档目录）时再按 0026 候选 B 的形态收窄引入。`TitlePolicy` 维持否决。
7. **忙碌时排队，回合结束后生效**：不新增 busy 状态位 —— app 线程跑回合时本就不 `recv` 信道①，
   语义天然成立（与 `/model` 的「下一回合生效」一致）。**代价**：回合进行中连会话列表都拿不到。
8. **出站顺序是不变量**：`SessionActivated` → `Output` → `View`。反了，切换通知会被回放的清屏吃掉。
9. **失败不改状态**：目标不存在/不可读/JSONL 损坏/模型不可解析 ⇒ 报错并保持当前会话、transcript、
   模型全不变（照 `Wiring::new_session` 的既有不变量）。空列表 ⇒ 提示「没有可恢复的会话」，不弹空浮层。
10. **列表不静默过滤或截断**。会话目录的 picker「只见最近几个」是两个生态级 issue 的根因；
    `list()` 本就是 `stat` + 首行 4 KiB 封顶，量级可控，先全量。

## Considered Options

- **一次性载荷塞进 `CodingView`（不碰 `Outbound`）**：少一个协议变体，且更贴 `/model`、`/skills` 浮层
  「从 `view` 取候选」的既有范式。**不采纳** —— 把**消息**混进「**快照**」类型会让该类型的语义变浑，
  下一个加字段的人无从判断哪些是快照、哪些是一次性载荷；且新增 `Outbound` 变体的真实成本比想象小
  （`-p` / `--json` **不消费 `Outbound`**，它们直接收 `Receiver<Envelope>`，`main.rs:802/838/878`）。
- **有界回放（只放尾 N 条 + 折叠更早）**：每帧渲染成本恒定（长 transcript 的 `line_count` 是线性开销），
  是真实的性能收益。**本轮不采纳** —— 与「切过去就等于打开那个会话」直接冲突；改为**先实测再定**
  （设计文档不变量 I7）。若日后引入，**必须显式提示折叠了多少**（本 ADR 决策 10 同理）。
- **忙碌时拒绝切换**（Codewhale `session_transition_blocked` 形态）：更显式，但要新增 busy 状态位与
  拒绝路径，且 Codewhale 的判定依赖其 `task_panel` / `runtime_turn_status` 等本仓没有的运行时状态。**不采纳**。
- **忙碌时先 abort 当前回合再切**（pi 形态）：中断与切换两个动作叠在一起，失败时状态难说清。**不采纳**。
- **`Request::Resume { target: Option<String> }`**（无参=列表、有参=直切）：少一个变体。**不采纳** ——
  查询与变更混装，将来加路径直切/强制/分页时必然要拆。
- **只做无参浮层，不做 `/resume <id>`**：YAGNI。**不采纳** —— 解析复用 `sessions::resolve`，成本≈0，
  且与 `/model` 的双形态对称。
- **TUI 侧自己扫会话目录**：不用过协议。**不采纳** —— 破坏依赖方向（`ys-tui-coding` 不得读 `Session`/文件系统），
  且要复制 `sessions.rs` 的字节级首行读取与 4 KiB 封顶细节。

## 后果

- **好转**：启动期 `--resume` 的空屏被同一条通路顺带修掉；「用 `--resume` 打开」与「运行期切进去」从此
  共享同一条模型/统计/历史语义，不可能分叉。
- **代价**：`ys-protocol` 的 `Request` 8 → 10 变体、`Outbound` 4 → 6 变体；`ys-tui-coding` 多一个
  `Message → TranscriptLine` 的共用映射与一处缓存失效约定；`run.rs` 的 `pending_prompt` 检查点要从
  「按键后」扩到「排空 Outbound 后」（否则浮层要等用户再敲一个键才弹）。
- **暴露的独立缺口（不在本 ADR 范围）**：`SessionMeta` **没有 cwd 字段**，故不同项目的会话混在一个列表里；
  pi 与 Codewhale 都有 workspace 分组。
- **实现顺序建议**：先做「协议 + 切换（暂不回放）」—— 这一片就验证模型跟随与排队语义，
  且「切换后屏幕空」与现状启动期 resume 观感一致，不构成回归；回放通路单独一片。
