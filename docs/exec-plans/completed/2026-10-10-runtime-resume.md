# 运行期 `/resume`（切换到指定 session）执行计划

> **纪律声明**：本次有 **5 条 P1 不变量**（**I1** 步骤顺序 / **I2** 出站序 / **I6** 等长碰撞 /
> **I9** thinking 三方真值 / **I7** 首帧指标）。**同一单元内必须先写测试（应红）再实现（转绿）**；
> review 只认「**变异测试变红**」作为证据（本仓多个真 bug 是「测试全绿时依然存在」）。
>
> 每个 `type=impl` 配一个 `type=test`。字段：类型 / 依赖 / 涉及目录 / 涉及文件 / 描述 / 关联 / 验证方法。

## 上下文引用

- 设计计划：`docs/design-plans/completed/2026-10-10-runtime-resume.md`
- 权威设计：`docs/arch/runtime-resume/design.md`（§3.2 接口 / §3.3 恢复步骤 / §5 不变量 I1–I13 / §6 切片）
- 质量分析：`docs/arch/runtime-resume/review.md`（24 条发现 / 8 条 P1，均已回修进设计）
- 现状与约束：`docs/arch/runtime-resume/context.md`
- 决策记录：`docs/adr/0033-runtime-resume-command.md`

## 基线

Phase 3 开始时由 `scripts/baseline.sh` 填写。

## 任务清单

### T1 [impl] `ys-protocol` 协议层新增
- **类型**: impl
- **涉及目录**: `crates/ys-protocol`
- **涉及文件**: `crates/ys-protocol/src/session.rs`（新，`SessionSummary`）、`crates/ys-protocol/src/request.rs`（2 个 `Request` + 2 个 `Outbound` 变体）、`crates/ys-protocol/src/lib.rs`（导出）
- **描述**: 加纯数据类型 `SessionSummary { id, path: PathBuf, mtime: SystemTime, title }`（零 tokio）；`Request` 加 `ListSessions` / `ResumeSession { target: String }`（查询与变更分开）；`Outbound` 加 `SessionList(Vec<SessionSummary>)` / `SessionResumed { summary, thinking_visible: bool, history: Vec<Message> }`（三者同一条消息，避免中间态）。命名不再出现「激活 / Activated」。
- **关联**: 测试 T2
- **依赖**: **无**

### T2 [test] `ys-protocol` serde roundtrip（N1）
- **类型**: test
- **涉及目录**: `crates/ys-protocol`
- **涉及文件**: `crates/ys-protocol/src/request.rs`、`crates/ys-protocol/src/session.rs`（内联 `#[cfg(test)] mod tests`，照 `request.rs:90` 既有 `assert_roundtrip` 范式）
- **描述**: 覆盖 2 个新 `Request` + 2 个新 `Outbound`，含 `SessionSummary` 的 `PathBuf` / `SystemTime` 字段。
- **关联**: impl T1
- **依赖**: T1
- **验证方法**: `cargo test -p ys-protocol` — 期望 roundtrip 往返相等、用例 passed

### T3 [impl] `apps/coding-agent` 会话目录视图
- **类型**: impl
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/src/sessions.rs`、`apps/coding-agent/src/main.rs`
- **描述**: **删 `Listing`**，`sessions.rs` 改用 `ys_protocol::SessionSummary`（全仓仅 `sessions.rs` 内部具名使用，`main.rs` 不具名）。
- **关联**: 测试 T4
- **依赖**: T1

### T4 [test] `apps/coding-agent` 目录视图回归
- **类型**: test
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/src/sessions.rs`
- **描述**: `list` / `resolve` / `read_meta` / 首行 4 KiB 封顶在新类型下行为不变。
- **关联**: impl T3
- **依赖**: T3
- **验证方法**: `cargo test -p ys-coding-agent` — 期望既有目录用例全绿、新类型通过

### T5 [impl] `ys-tui-coding` 回放基建
- **类型**: impl
- **涉及目录**: `crates/ys-tui-coding`
- **涉及文件**: `crates/ys-tui-coding/src/cache.rs`、`crates/ys-tui-coding/src/transcript.rs`、`crates/ys-tui-coding/src/lib.rs`
- **描述**: `cache.rs` 加 `RenderCache::reset()` 并**修订模块头「只增不减」不变量**（`/new` 不触、`/resume` 触）；`transcript.rs` 加 `pub fn replay_lines(&[Message]) -> Vec<TranscriptLine>`（**镜像** `apply_event` 的输出约定，非共用规则 —— 共享的是 `summarize_tool_args` 与 id 配对约定）；`lib.rs` 导出。
- **关联**: 测试 T6
- **依赖**: **无**

### T6 [test] `ys-tui-coding` 回放基建测试（N6 / N9 / N10 / N13）
- **类型**: test
- **涉及目录**: `crates/ys-tui-coding`
- **涉及文件**: `crates/ys-tui-coding/src/cache.rs`、`crates/ys-tui-coding/src/transcript.rs`
- **描述**: N6 等长碰撞变异（两条同字节长度、同 width 的 assistant 文本；删 `reset()` 必须红）；N9 两条渲染边界（无配对 `ToolUse` 不得显示为成功；只含 `ToolResult` 的 User 消息不得渲染成空用户行）；N10 跨路径逐行等价（live `apply_event` vs `replay_lines`）；N13 `#[ignore]` 首帧基准。
- **关联**: impl T5
- **依赖**: T5
- **验证方法**: `cargo test -p ys-tui-coding` — 期望上述用例 passed 且变异项变红；`cargo test -p ys-tui-coding --release -- --ignored --nocapture` 读 N13 首帧耗时 vs 100 ms
- **注**: N13 基准**不建 `tests/replay_bench.rs`** —— `RenderCache` 是 `pub(crate)`，集成测试文件访问不到，基准写成 crate 内 `#[cfg(test)]` 内联单测 + `#[ignore]`。

### T7 [impl] `ys-tui-coding` 命令与浮层
- **类型**: impl
- **涉及目录**: `crates/ys-tui-coding`
- **涉及文件**: `crates/ys-tui-coding/src/commands.rs`、`crates/ys-tui-coding/src/prompter.rs`、`crates/ys-tui-coding/src/app.rs`、`crates/ys-tui-coding/src/run.rs`
- **描述**: `commands.rs`（`COMMANDS` 12→13 + `parse` 分支 + `PromptKind::PickSession`）；`prompter.rs`（`resolve_session_pick`，**不改** `resolve_prompt` 签名；标签含 id 短码以备反解）；`app.rs`（`pending_sessions` + `reset_for_replay()` + 两个 `apply_outbound` 臂）；`run.rs`（检查点三处：poll 分支后 / 排空 Outbound 后 / 模态内 `ModalHost::drain_outbound`）。
- **关联**: 测试 T8
- **依赖**: T1, T5

### T8 [test] `ys-tui-coding` 命令与状态测试（N8 / N11）
- **类型**: test
- **涉及目录**: `crates/ys-tui-coding`
- **涉及文件**: `crates/ys-tui-coding/src/app.rs`、`crates/ys-tui-coding/src/commands.rs`、`crates/ys-tui-coding/src/prompter.rs`、`crates/ys-tui-coding/src/run.rs`
- **描述**: N8 `reset_for_replay()` 逐项清空（transcript / md_cache / tool_index / suppress_user_echo / pending_echo_texts / follow / scroll_offset / thinking_visible）；`apply_outbound(SessionList)` **同时**设 `pending_sessions` 与 `pending_prompt`；Esc 取消无副作用；N11 中 `ys-tui-coding` 侧的边界（空列表不弹浮层、`sessions_dir == None` 不发请求、选中当前会话 = no-op）。
- **关联**: impl T7
- **依赖**: T7
- **验证方法**: `cargo test -p ys-tui-coding` — 期望逐项清空断言与浮层状态用例 passed

### T9 [impl] `apps/coding-agent` 恢复能力
- **类型**: impl
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/src/wiring.rs`、`apps/coding-agent/src/capabilities.rs`、`apps/coding-agent/src/main.rs`
- **描述**: `wiring.rs`（`resume_into` 镜像 `new_session` 的 open-then-replace + sessions_dir getter，注意 `main.rs:616` 已有同名自由函数，用 `sessions_dir_path()` 避混）；`capabilities.rs`（`list_sessions` / `resume_session`，**错误形态沿既有 `Vec<String>`**，但不得吞掉 `SessionError` 分类）；`main.rs`（抽纯函数 `resolve_model_from_meta`，启动期与运行期共用，**不写 `cfg`**）。
- **关联**: 测试 T10
- **依赖**: T1, T3

### T10 [test] `apps/coding-agent` 恢复能力测试（N2）
- **类型**: test
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/src/wiring.rs`、`apps/coding-agent/src/main.rs`
- **描述**: N2 顺序变异（把模型解析挪到 `resume_into` 之后且失败 ⇒ 必须红）；`resume_into` 失败原子性（open 失败时当前 session 完好无损）；`resolve_model_from_meta` 优先级链（env 凭证 > meta > state.json > 首个有凭证者）。
- **关联**: impl T9
- **依赖**: T9
- **验证方法**: `cargo test -p ys-coding-agent` — 期望失败原子性用例 passed、顺序变异项变红、优先级链用例 passed

### T11 [impl] `apps/coding-agent` 接入
- **类型**: impl
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`、`apps/coding-agent/src/main.rs`
- **描述**: `app_loop.rs`（两个 `match` 臂 + `design.md` §3.3 提交步 + **`thinking_visible` 镜像重设** + `turns` 不归零）；`main.rs`（启动期回放，注意 I10 顺序 —— **先投 `SessionResumed`（回放）、再投 `startup_hint`**，反序会让提示被 `clear()` 吃掉）。
- **关联**: 测试 T12
- **依赖**: T9

### T12 [test] `apps/coding-agent` 接入测试（N3 / N4 / N7 / N11）
- **类型**: test
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/src/app_loop.rs`、`apps/coding-agent/src/main.rs`
- **描述**: N3 运行期出站顺序 `SessionResumed → Output → View`；N4 启动期顺序（`startup_hint` 不被 `clear()` 吃掉）；N7 thinking 变异（令 `T_old != T_target`，切换后跑一回合，断言目标文件 meta 的 `thinking_visible == T_target`）；N11 中 app 侧边界（`sessions_dir == None`、`Envelope.turn` **不归零**）。
- **关联**: impl T11
- **依赖**: T11
- **验证方法**: `cargo test -p ys-coding-agent` — 期望出站序断言 passed、thinking 变异项变红、边界用例 passed

### T13 [test] 子进程 e2e（N5 / N12）
- **类型**: test
- **涉及目录**: `apps/coding-agent`
- **涉及文件**: `apps/coding-agent/tests/resume_cli.rs`（新，照 `apps/coding-agent/tests/session_cli.rs` 的脚手架：子进程 + 临时目录名含原子序号与 `process::id()` + 无凭证/回环 mock）
- **描述**: N5 会话路径快照刷新（退出 `session: <id>` 是目标 id）；N12 切换后继续提问 ⇒ 新消息 append 进**目标文件**、`--resume` 启动期回放上屏。
- **关联**: impl T11
- **依赖**: T11
- **验证方法**: `cargo test -p ys-coding-agent --test resume_cli` — 期望子进程用例 passed（离线、无凭证、走回环 mock）

### T14 [impl] 文档订正与同步
- **类型**: impl
- **涉及目录**: `docs/`（文档）
- **涉及文件**: `docs/arch/runtime-resume/design.md`、`docs/architecture.md`、`docs/arch/runtime-resume/context.md`、`docs/CONTEXT.md`、`AGENTS.md`
- **描述**: `design.md` 订正两处断言（「同步 IO 只有 export 一例」不成立 —— `login`/`logout`/`set_model` 同样在 app 线程写 `std::fs`；I11 把「User 消息只含 ToolResult」说成「常态」降级为「必须处理的一种输入」；顺带订正行号 `capabilities::export` 实为 `:282-303`）；`architecture.md`（§14 命令表 11→13 条并加 `/resume`；§12 会话落盘节）；`context.md`（约束 7/8 标注已被既有机制化解）；`CONTEXT.md`（加「resume vs `/new`」术语）。
- **回填项（执行期新增，必须写进 `design.md`）**：
  1. **§3.5**：`PromptOutcome` 新增 **`NoOp(String)`** 变体（「有意义的无操作：不发请求、只提示」）—— 原枚举只有 `Request`/`Prefill`，装不下「选中当前会话 ⇒ no-op 并明确提示」这条要求。
  2. **§4 Q7 与 §3.5**：「标签放 **id 短码**」**已被实现推翻**，改为放**完整 id**。理由：会话 id 取文件名 stem（`{unix秒}_{纳秒}`），同日创建的会话共享日期前缀 ⇒ **任何定长短码都会相撞**，靠「显示串相等」反解会**切错会话**。完整 id 唯一 ⇒ 唯一可反解。
  3. 若 U3-fix 选定 I11 的 (b) 方案，同步 §5 的 I11 措辞（U3-fix 已获授权改 §5 的 I11 行与 §7）。
  4. **§5「测试要点」里的「跨路径等价」条目仍是过度声称** —— 它写「live transcript 与 `replay_lines` 输出**逐行相等**」，与已修订的 §7 矛盾。改为「**除 §7 列出的三条已知形状分叉外**逐行相等」。（U3-fix 已按授权范围只改了 §7 与 I11，未动 §5 测试要点。）
  5. **`/skill-name` 分叉还有第三种形态**（`review.md` 未列）：**回合进行中**提交 skill 时，`pending_echo_texts` 登记的是展开前原文、回声却是信封文本 ⇒ 认领不到，且届时 `is_turning == false` ⇒ live 渲染成 `System(信封)`（而非空闲提交时的 `User("/myskill")`）。`app.rs` 的 `pending_echo_texts` docstring 已记这条边界。**并入 §7 的分叉清单。**
  6. **§5 的 I11 行里「只含 `ToolResult` 块是常态」措辞要降级** —— 见上文「核实出的 2 处断言偏差」。
- **⚠️ 本任务不再包含 `AGENTS.md`**（用户裁定）：该文件工作区内含**与本需求无关的在途改动**（另一会话/来源写入的「事实性断言的指针纪律」条目），本变更**不接受它**。且 `#[ignore]` 计数因新增 N13 基准恰好回正为 7，与文件现有数字一致，无需订正。
- **关联**: 测试 T15
- **依赖**: **无**

### T15 [test] 文档一致性核对
- **类型**: test
- **涉及目录**: `scripts/`
- **涉及文件**: `scripts/doc-consistency.sh`（新增；纯 bash + grep，不引新依赖）
- **描述**: grep 断言 —— `architecture.md` §14 含 `/resume` 且不再写 11 条；`CONTEXT.md` 含 resume 术语；`AGENTS.md` 的 ignore 数字与 `grep -c` 实测一致；`design.md` 不再出现「常态」措辞。
- **关联**: impl T14
- **依赖**: T14
- **验证方法**: `bash scripts/doc-consistency.sh` — 期望退出码 0（每条断言成立：空命中 / 数字一致）

### T16 [test] 全量回归
- **类型**: test
- **涉及目录**: 全仓
- **涉及文件**: 无（纯验证任务，不产生文件变更；空文件集不参与并发冲突检测）
- **描述**: 全量测试 + clippy 干净。
- **关联**: 上游各 test 任务
- **依赖**: T2, T4, T6, T8, T10, T12, T13, T15
- **验证方法**: `cargo test --workspace` 与 `cargo clippy --all-targets` — 期望全绿、无 warning

## 波次计划（Phase 3 计算后回填）

**基线**：`eb3ff236a747c5e78de46b9496435a4bd4a8a65a`

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `crates/ys-protocol/src/{session,request,lib}.rs` | 无 | W0 | deep |
| U2 | T3, T4 | `apps/coding-agent/src/sessions.rs` | U1 | W1 | shallow |
| U3 | T5, T6 | `crates/ys-tui-coding/src/{cache,transcript,lib}.rs` | 无 | W0 | deep |
| U4 | T7, T8 | `crates/ys-tui-coding/src/{commands,prompter,app,run}.rs` | U1, U3 | W1 | deep |
| U5 | T9, T10, T11, T12, T13 | `apps/coding-agent/src/{wiring,capabilities,main,app_loop}.rs`、`apps/coding-agent/tests/resume_cli.rs` | U1, U2 | W2 | deep |
| U6 | T14, T15 | `docs/arch/runtime-resume/design.md`、`docs/architecture.md`、`docs/arch/runtime-resume/context.md`、`docs/CONTEXT.md`、`AGENTS.md`、`scripts/doc-consistency.sh` | U1–U5 | W3 | shallow |
| U7 | T16 | 无 | U1–U6 | W4 | —（纯验证） |

**合并说明**（R1–R4）：
- U1/U3/U4/U5 都是「impl 与其配对 test 同单元」（R4）。
- **U5 是唯一被迫合并的单元**：T9 / T11 都要改 `apps/coding-agent/src/main.rs`，T11 与 T13 同属接入（R4）⇒ 五者合并为 5 文件（恰在 R3 上限）。
- U2 之所以**能**独立成单元：已核实 `main.rs` 从不具名 `Listing`（只调 `sessions::list/pick/resolve/read_meta`，`main.rs:654/663/668/678`），故 T3 只碰 `sessions.rs`。
- U6 依赖**全部**实现单元：T15 要断言 `AGENTS.md` 的 `#[ignore]` 计数与实测一致，而 N13 的基准会给全仓**新增 1 个 `#[ignore]`**（6 → 7）。

**同波文件集互不相交**：W0 三单元所在目录互不相交；W1 的 U2（`apps/.../sessions.rs`）与 U4（`crates/ys-tui-coding/src/*`）不相交；W2/W3/W4 各只有一个单元。

### ⚠️ 波次修正（执行中记录，2026-10-10）

**原划分的缺口**：R1「文件集不相交」**不足以**保证并发安全 —— 它漏了**类型级耦合**。

U1 给 `ys_protocol::Outbound` 加两个变体后，**消费者** `crates/ys-tui-coding/src/app.rs:108` 与
`apps/coding-agent/src/app_loop.rs` 的 `match` **立即非穷尽**（`E0004`）⇒ W0 期间**整棵树编译不过**，
U3（与 U1 同属 W0）无法独立验证（U3 报告已记录：临时代 `app.rs` 打垫片、验证后撤回）。

**教训**：`Request` / `Outbound` 这类**强制穷尽 match 的协议枚举**，其扩展点会**跨 crate 强制同步**。
这类单元不能按「文件集不相交」拆到不同波次，应视为**同一个编译单元**（或至少把消费者侧的补臂与协议加臂排进同一波）。

**处置**：U4（负责 `app.rs` 的两臂）在 U3 完成后**立即派发**以修复关键路径；`app_loop.rs` 的两臂由 U5 负责。
U2 与 U1-fix 由 orchestrator 发协调通知，明确「构建失败非你造成、不要越界去修」。

## 验证清单

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo clippy --all-targets` 无 warning
- [ ] `cargo test -p ys-protocol`（N1）
- [ ] `cargo test -p ys-tui-coding`（N6 / N8 / N9 / N10）
- [ ] `cargo test -p ys-tui-coding --release -- --ignored --nocapture`（N13，读首帧耗时 vs 100 ms）
- [ ] `cargo test -p ys-coding-agent`（N2 / N3 / N4 / N7 / N11）
- [ ] `cargo test -p ys-coding-agent --test resume_cli`（N5 / N12）
- [ ] 五条 P1 不变量各自完成**变异自检**（注入 bug 确认对应测试变红）
- [ ] T15 文档一致性 grep 断言全部成立
