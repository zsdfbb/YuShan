# 运行期 `/resume`（切换到指定会话）— As-built

> 性质：**实际建成记录**。设计见 `docs/arch/runtime-resume/design.md`
> （+ `context.md` / `candidates/`），决策见 `docs/adr/0033-runtime-resume-command.md`
> （**取代 ADR-0026 决策 6**）与 `docs/adr/0043-resume-repairs-interrupted-tool-calls.md`，
> 质量分析见 `docs/arch/runtime-resume/review.md`
> （24 条发现 / 8 条 P1，全部回修进设计），执行计划见
> `docs/exec-plans/completed/2026-10-10-runtime-resume.md`（含「波次修正」节）。
> 日期：2026-10-11 ｜ 基线 `eb3ff23`。

## 一句话

TUI 运行期新增 **`/resume [id]`** —— 无参弹浮层列出现有会话、选一个**整体替换**当前会话（`Wiring.session`
换到另一条谱系，磁盘上不删任何文件）；有参直切。**同一通路顺带修掉启动期 `--resume` 的空屏缺陷**
（`App::new` 的 transcript 恒为 `Vec::new()` —— 恢复后用户看到的是空屏）。这是一次对 ADR-0026
「不做运行期 `/resume`」的**推翻**：协议加 2 个 `Request` + 2 个 `Outbound` 变体，把「恢复一个既有会话」
抽成启动期与运行期**共用**的 app 侧能力。

## 不变量（I1–I13）与实现落点

| # | 不变量 | 落点 |
|---|---|---|
| **I1** | 失败不改状态（会话/transcript/模型/thinking/统计全不变，只回一条错误 `Output`） | `capabilities::resume_session`：**可失败步全部前置**到 `wiring.resume_into` 之前，之后只有不可失败的提交步 |
| **I2** | 出站顺序固定 `SessionResumed → Output → View` | `app_loop`：`session_event` 在 `Output`/`View` 之前发送 |
| **I3** | 会话路径快照必须刷新 | `app_loop` 提交步 `session_path.record(wiring.session_path())` |
| **I4** | 空列表不弹空浮层，只提示 | `app.rs` 的 `SessionList` 臂：`items.is_empty()` ⇒ 推 `System("没有可恢复的会话")` 并清 `pending_sessions`/`pending_prompt` |
| **I5** | 列表不静默过滤/截断 | `capabilities::list_sessions` → `sessions::list`（全量，`stat` + 首行 4 KiB 封顶） |
| **I6** | 回放必须使 `RenderCache` 失效，且时机正确 | `RenderCache::reset()`（**显式**，不指望 `sync_len` 兜住等长碰撞） |
| **I7** | 回放全量是默认，有界是预案（度量口径 = `reset()` 后**首帧**） | `cache.rs` 的 `n13_first_frame_baseline`（`#[ignore]`） |
| **I8** | 清屏同时重置全部同源状态 | `App::reset_for_replay()` |
| **I9** | thinking 三方真值一次对齐（UI / app 进程级镜像 / 会话 meta） | `app_loop` 的 `ResumeSession` 臂重设 `thinking_visible` 镜像 |
| **I10** | 启动期也适用 I2（回放先于启动提示） | `main::startup_outbound_sequence`（`SessionResumed` → `Output(hint)`） |
| **I11** | 回放的渲染边界（`ToolResult`-only 用户消息不画空行；无配对 `ToolUse` 不显成功） | `transcript::replay_lines`（无配对 ⇒ `success = false`） |
| **I12** | `/resume` 后 `/new` 行为不变（只换句柄、不清 transcript） | `/new` 路径未动 |
| **I13** | `sessions_dir == None`（内存会话）⇒ app 回提示、不弹浮层 | `app_loop` 的 `ListSessions` 臂 `None` 分支 |
| **I14** | 被中断的工具调用在 resume 时补成错误结果并先落盘；修复摘要只消费一次，排在回放之后 | `JsonlSession::open` / `Session::take_recovery` / 三模式应用层接线（ADR-0043） |

## 改动清单（按层）

```
crates/ys-protocol/
  src/session.rs（新）       SessionSummary { id, path: PathBuf, mtime: SystemTime, title }（纯数据、零 tokio）
  src/request.rs（改）       Request::{ListSessions, ResumeSession { target: String }}
                             Outbound::{SessionList(Vec<SessionSummary>), SessionResumed { summary, thinking_visible, history: Vec<Message> }}
                             （查询 / 变更分开；三样同一条消息，避免中间态）
  src/lib.rs（改）           mod session; pub use session::*

crates/ys-tui-coding/
  src/cache.rs（改）         RenderCache::reset()；模块头「只增不减」不变量修订为「/new 不触、/resume 触」
  src/transcript.rs（改）    pub fn replay_lines(&[Message]) -> Vec<TranscriptLine>（镜像 apply_event 输出约定）
  src/commands.rs（改）      COMMANDS 12→13（`resume`，arg_hint = "[id]"）；parse 分支；PromptKind::PickSession
  src/prompter.rs（改）      resolve_session_pick / resolve_session_pick_at；PromptOutcome::NoOp(String)；
                             session_label（完整 id 前置、可反解）；resolve_prompt 对 PickSession 留防御性 Err
  src/app.rs（改）           pending_sessions: Option<Vec<SessionSummary>>；apply_outbound 两个新臂；
                             reset_for_replay()（清屏）
  src/run.rs（改）           三处检查点：排空 Outbound 之后、按键处理后（原有）、模态内 ModalHost::drain_outbound
  src/lib.rs（改）           导出 replay_lines / SessionSummary 相关

apps/coding-agent/
  src/wiring.rs（改）        resume_into(&mut self, path) -> Result<SessionMeta,_>（镜像 new_session 的 open-then-replace）；
                             sessions_dir_path() getter
  src/capabilities.rs（改）  list_sessions / resume_session / ResumedSession / session_summary / build_candidate_model
  src/main.rs（改）          resolve_model_from_meta（纯解析、不写 cfg）；startup_outbound_sequence；resume_replay
  src/app_loop.rs（改）      Request::{ListSessions, ResumeSession} 两个 match 臂 + 提交步（thinking 镜像 / 统计播种 /
                             路径快照） + session_event 抢在 Output 之前
  src/sessions.rs（改）      删本地 Listing（改用 ys_protocol::SessionSummary）；DEFAULT_TITLE 提为 pub(crate)
  tests/resume_cli.rs（新）     子进程 e2e（N5 / N12）
```

**未动**：`ys-core` / `ys-event` / `ys-runtime` / `ys-loop`；`-p` / `--json` 两入口（它们不收 `Outbound`，
直收 `Receiver<Envelope>`，不受新增出站变体影响）；`crates/ys-tui-coding/Cargo.toml`（`ys-core` 已在依赖里）。

## 恢复的步骤顺序（顺序即 I1 的证明）

```
1. sessions::resolve(dir, target)            — Err ⇒ 只发错误 Output，状态未动
2. sessions::read_meta(&path)                — 复用启动期读法（只读首行）
3. resolve_model_from_meta（纯解析四字段）   — 无可用凭证 ⇒ 报错、状态未动
4. build_candidate_model（换入候选 → build_model() → 立即还原；三段间不得插 await）
5. wiring.resume_into(path).await            — open 成功才替换 session 句柄
6. 提交（此后无失败路径）：写回 config 四字段 / wiring.set_model / thinking 镜像 /
   *stats = seed_stats(meta) / session_started = now / session_path.record
7. 出站：SessionResumed → Output → View
```

**为什么必须是这个序**：启动期的真实顺序就是「meta → 模型 → open」（`main.rs` 读 meta → 解析模型 →
`Wiring::resume`）。运行期照抄，两处才真正同源；反过来写（`resume_into` 在模型解析之前）一旦模型解析
失败就留下「会话已换、模型还是旧的」半切换，与 I1 互斥 —— 这正是质量分析 T1 的硬伤（三个候选原文
都错了这个序）。

## 落地与设计的偏差（执行期发现，均已回填进 `design.md`）

1. **「id 短码」不可行 ⇒ 标签用完整 id**。会话 id = `{unix秒}_{纳秒}`，取自文件名 stem；同一秒创建的
   会话共享 `{秒}_` 前缀 ⇒ **任何定长短码都可能相撞**，靠「显示串相等」反解会**切错会话**。完整 id 唯一
   ⇒ 唯一可反解。`session_label` 因此把 id **前置**（窄屏截断时先保留 id 前缀）。
2. **`PromptOutcome` 新增 `NoOp(String)` 变体**（原两变体 `Request` / `Prefill` 装不下「选中当前会话
   ⇒ no-op 且要提示」）。
3. **`thinking_visible` 不在 `reset_for_replay()` 内**，改由 `SessionResumed` 臂按目标 meta 单独设定
   （功能等价，但与 I8 原字面「清屏时一并重置 thinking」不同 —— 清屏与「跟随目标会话」是两种语义）。
4. **§3.3 第 3 步的「cfg 克隆」不可行**（`Config` 持 `Box<dyn Fn>` factory、**非 `Clone`**）⇒ 实际是
   `build_candidate_model` 的**换入 → 同步 `build_model()` → 立即还原**；三段间**不得插 `await`**
   （否则把「已换入、未还原」的中间态暴露给并发观察者）。
5. **`JsonlSession::open` 惰性 + 逐行容错** ⇒ 「路径不存在 / 非法 JSONL 令 open 失败」**不成立**
   （不存在 ⇒ `Ok(空会话)`；坏行逐行跳过并计数）。失败原子性只能用**真 OS 读错**构造 ——
   `resume_into_failure_leaves_current_session_untouched` 传一个**目录**（`EISDIR`）。
6. **I11 取 (b) 方案**：无配对 `ToolUse` 置 `success = false`（屏上 ✗），**不伪造结果文案**
   （`draw.rs` 的既有语义是「失败才显结果首行」，无 `result` 即无说明行）。
7. **`resolve_model_from_meta` 档 1 短路**：`if cfg.is_configured()` ⇒ 原样保留当前模型、**不读 meta**。
   故「设了 env 凭证或曾 `/login`」时，「模型跟随目标会话 meta」**不生效**。这与启动期 `--resume`
   **同源**（走同一条优先级链），**非运行期回归** —— 设计如实记录，不绕开。
8. **波次划分的缺口（执行计划的「波次修正」节）**：`Outbound` / `Request` 加变体会让**消费者**的 `match`
   立刻非穷尽（`E0004`）⇒ 「文件集不相交」**不足以**判定并发安全。协议枚举的扩展点会**跨 crate 强制
   同步**：`app.rs` 与 `app_loop.rs` 的补臂必须与协议加臂排进同一编译单元。
9. **启动期出站顺序**：**先投 `SessionResumed`（回放）、再投 `startup_hint`**；反序会让回放的 `clear()`
   吃掉提示（I10）。`startup_outbound_sequence` 是启动期唯一给出该次序的地方。

## 恢复韧性：悬空工具调用与坏行（ADR-0043）

`JsonlSession::open` 现在同时完成「读取」与一次性的**历史规范化**：

1. 按字节收集完整行，再对整行做 UTF-8 与 JSON 解码；非 UTF-8、非法 JSON、
   `Message` / `SessionMeta` 皆失败的行只增加 `skipped_lines`，库层不直接打印。
2. 收集全部已有 `ToolResult.tool_call_id`，并为缺失结果的 `ToolUse` 合成
   `is_error = true` 的 `ToolResult`（`Tool execution was interrupted; result unavailable.`）。
3. 若 assistant 后紧跟工具结果组，缺失项追加到该 User 消息；否则在其后插入一条只含合成结果的
   User 消息。后续用户文本仍排在结果组之后。
4. 只要发生工具修复，就先经既有 tmp + rename 原子路径重写；失败则 `open` 失败。无修复不重写。
5. `SessionRecovery { repaired_tool_calls, skipped_lines }` 暂存在会话句柄，
   `Session::take_recovery` 一次性取走。

应用层只在三个位置消费摘要：启动 TUI 把它并入启动提示（`SessionResumed` 之后）、运行期 `/resume`
把它并入成功 `Output`（同样在回放之后）、`-p` / `--json` 写 stderr。这样 TUI 回放清屏不会吞掉告警，
JSON stdout 也不会混入非 `Envelope` 行。

**边界**：不重新执行工具（副作用不可重放）；已有结果按 id 全局计为已配对，不主动重排结果位于
其他位置的畸形历史。当前 agent 写路径只会产生「assistant 后紧跟完整结果组」，进程在两个 append
之间终止时则产生文件尾悬空调用，二者都被本策略覆盖。

## 回放 ≠ 逐行等价：三条已知形状分叉

`replay_lines` **镜像** `apply_event` 的**输出约定**（不是「共用同一套规则」——`apply_event` 是事件驱动的
单块事件，回放输入是复合 `Message`；共享的只有 `summarize_tool_args` 与 id 配对约定）。**跨路径逐行等价
只在下列形状之外成立**；三条分叉有同一根源 —— 历史里没有 live 侧才有的状态（`is_turning`、提交原文、
流式合并窗口）。每条由 `transcript.rs` 的 `p1_1_*` 用例**显式断言 `replay != live`**（任一侧改动立即变红）：

1. **回合刚落幕时到达的 `UserMessage`**（`is_turning == false`）：live 渲染 `System(text)`，
   回放只有 `role=User` ⇒ `User(text)`（可达）。
   `p1_1_idle_user_message_diverges_system_vs_user`。
   注：该分叉原先是「完工复核回合」的落点；复核已由 [ADR-0041](../adr/0041-remove-completion-review-turn.md)
   撤除 ⇒ 现在只剩上述收尾竞态。
2. **`/skill-name` 提交**：live 显本地回显的**展开前原文** `User("/myskill")`，回放显落盘的信封
   `User("<skill …>…</skill>")`（空闲提交即可达）。**第三种形态** —— **回合进行中**提交 skill 时，
   `pending_echo_texts` 登记的是展开前原文、回声却是信封文本 ⇒ 认领不到，且届时 `is_turning == false`
   ⇒ live 反而渲染成 `System(信封)`。`p1_1_skill_invocation_diverges_raw_vs_envelope`。
3. **相邻两条 assistant 文本消息**：live 合并成一条 `Assistant("ab")`，回放各起一条（**本仓 agent loop
   不产出相邻 assistant 消息 ⇒ 实际不可达**，仅作防御性断言）。
   `p1_1_adjacent_assistant_text_diverges_merged_vs_split`。

**不复刻 live 的 `is_turning` 启发式**：历史里没有该信息，只作为已知代价记录，不追求逐行等价。

## 实测数字

- **N13 首帧基准**（`cache::tests::n13_first_frame_baseline`，500 条 × ~2 KiB / 共 **1,039,890 字节**）：
  执行期样本 **3.88 ms**（阈值 **100 ms** ⇒ **有界回放预案不触发**；写本文时复核重测约 8 ms，
  同为个位数毫秒量级、远低于阈值 —— 单次运行有波动，结论不变）。
- 量的是 `reset()` 之后**首帧**的全量 `md::render`（每条 assistant 重解析）——这才是 `/resume`
  **新增**的那笔；**不**量稳态每帧的 `line_count`（任何长会话今天就在付既有成本）。

## 测试钉子（关键用例 → 不变量 → 变异自检）

> 变异自检的证据写在各用例的注释里（本项目只认「注入 bug ⇒ 用例变红」）。本 as-built 为文档单元，
> 未重跑变异（不碰 Rust 代码）。

| 用例 | 抓的不变量 | 变异自检 |
|---|---|---|
| `wiring::resume_into_failure_leaves_current_session_untouched` | I1（open 失败原子性，传目录取 `EISDIR`） | 注释：失败时 session/session_path/meta/history 全断言不变 |
| `capabilities::resume_session_model_failure_leaves_session_untouched` | I1 / **N2 半切换** | **变异：把模型解析挪到 `resume_into` 之后且失败 ⇒ 必须红**（只覆盖 `open` 失败的变异抓不到） |
| `app_loop::resume_emits_session_resumed_then_output_then_view` | I2（运行期出站顺序） | 变异：把 `session_event` 移到 `Output` 之后 ⇒ 红 |
| `jsonl::tests::dangling_tool_use_at_tail_is_repaired_and_persisted` | I14（合成、落盘、再次打开不重复修） | 变异：只修内存 / 不写文件 ⇒ 重开断言红 |
| `jsonl::tests::repair_flush_failure_fails_open_instead_of_returning_memory_only_fix` | I14（修复落盘失败 ⇒ open 失败） | 变异：忽略 flush 错误 ⇒ `open` 返回 `Ok` ⇒ 红 |
| `capabilities::tests::resume_session_reports_interrupted_tool_repair_once` | I14（运行期摘要与一次性消费） | 变异：不取摘要 / 重复取 ⇒ 文案或 `None` 断言红 |
| `tests::with_session_recovery_hint_preserves_existing_hint_and_skips_empty` | I14（启动提示合并） | 变异：覆盖原提示 / 无修复也追加 ⇒ 红 |
| `main::startup_outbound_sequence_puts_replay_before_hint` | I10（启动期顺序） | 变异：交换 replay 与 hint ⇒ 红 |
| `app_loop::resume_resets_thinking_mirror_so_next_turn_writes_target_value` | I9（thinking 三方真值） | 变异：删 `thinking_visible = r.thinking_visible` ⇒ 红（下一回合把旧值写进新 meta） |
| `app_loop::resume_success_keeps_process_turn_sequence_but_seeds_view_from_target_meta` | 决策 5（`turns` 不归零 / `TurnStats` 重新播种） | 断言 `Envelope.turn` 续号、view 取自目标 meta |
| `cache::n6_reset_prevents_equal_length_collision` | I6（等长碰撞） | **变异：删掉 `reset()` ⇒ 必须红**（两条同字节长度、同 width 的 assistant 文本；长度不等会天然 miss） |
| `app::test_apply_outbound_empty_session_list_hints_without_modal` | I4（空列表不弹浮层） | 变异：空分支漏清 `pending_sessions`/`pending_prompt` ⇒ 旧候选残留用例红 |
| `prompter::test_session_label_id_leads_so_narrow_truncation_stays_distinguishable` | Q7（标签可反解） | 变异：把标签换成「id 前 8 字符」短码（两 id 同前缀）⇒ 红 |
| `tests/resume_cli.rs::resume_continues_target_session_and_replays_history` | N5（退出 `session: <id>` = 目标 id）/ N12（切换后新消息 append 进目标文件、`--resume` 回放上屏） | 子进程 + 回环 mock，离线 |

## 已知缺口 / 复访项

- **回合进行中拿不到列表**（Q8）：app 线程阻塞在 `run_turn`，连 `SessionList` 都要等回合末。若将来要让
  浮层「立刻弹」，需独立的列表服务任务 —— 本轮不做。
- **`SessionMeta` 无 cwd 字段** ⇒ 不同项目的会话混在一个列表里（pi/Codewhale 都有 workspace 分组）。
  本需求的 picker 把它变成**用户可见问题**，建议单独立项。
- **会话修复不做备份**：需要工具修复时会按现有原子重写流程移除无法解析的坏行；摘要给出数量，但不生成
  `.bak`。若后续需要取证，应独立设计归档副本，避免 resume 路径隐式堆积文件。
- **多进程并发写同一文件无锁**（`session-default-new` 既有缺口）：切到「另一进程正在写的会话」有整文件
  重写丢更新的风险。
- **有界回放**：待实测（I7 首帧指标 + 100 ms 阈值）；当前实测远低于阈值，预案不触发。
- **`architecture.md:593`** 关于 `/new` 换「session + model 两字段」的表述仍待订正（本轮未动 `architecture.md`
  该处；设计阶段 T14 已改命令表与 §12，此项属遗留）。
- **`build_candidate_model` 非 RAII**：还原靠控制流顺序到达；`Config` 的 model factory 若 panic，栈展开会
  **跳过还原**，把候选四字段留在 `cfg` 里。生产 factory 只构造、不 panic；已显式声明该契约。
- **TOCTOU**：列表已读、用户已选、加载之间目标被**改写**（非删除）时 `SessionSummary` 的 title/mtime 与
  实际可能不符，无校验。
- **`pending_sessions` 生命周期**：与 `pending_prompt` 同 take 同清；连发两条 `ListSessions` 时**后到覆盖、
  只开一次浮层**。
- **回放峰值内存约 3×**（`session.messages` + `history` clone + `TranscriptLine` 产物），带大工具结果的会话
  可达数十 MB —— 设计未估，记此。
- **`SessionCatalog` trait**：出现第二实现（远程后端、归档目录）时再按候选 B 的形态收窄引入；本轮不收。

## 全量回归现状

2026-10-19 实测 `cargo test --workspace` **全绿**。本次恢复韧性新增单测覆盖：会话摘要文案、
运行期修复摘要与一次性消费、启动提示合并。既有 `/resume` 出站顺序用例同时扩为真实悬空工具调用，
断言合成结果进入回放历史且摘要随 `Output` 抵达。
`cargo clippy -p ys-session -p ys-model-openai-compat --all-targets --no-deps -- -D warnings`
通过；全 workspace clippy 仍受基线既有的 `ys-protocol` / coding-agent lint 阻塞，本次未顺手修改
范围外代码。

## 参考

- `docs/adr/0033-runtime-resume-command.md`（取代 ADR-0026 决策 6）
- `docs/arch/runtime-resume/{context,design,review}.md`、`candidates/{a-minimal,b-extensible,c-performance}.md`
- `docs/design-plans/completed/2026-10-10-runtime-resume.md`、`docs/exec-plans/completed/2026-10-10-runtime-resume.md`
- `docs/design-final/session-default-new.md`（被取代处已加标注）、`docs/design-final/coding-agent-tui.md`（命令表已更新）
- `scripts/doc-consistency.sh`（本批文档订正的对账点）
