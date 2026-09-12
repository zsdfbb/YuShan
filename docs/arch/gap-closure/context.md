# YuShan 基础能力补齐（Pi 差距收敛）— 架构上下文

## 概述

以 `docs/reports/gap-analysis-pi-comparison.md` 为输入，确定「必须补齐」（Tier 1）与「高价值增强」（Tier 2 精选）工作的**落地顺序与解耦边界**。目标是补能力，不破坏 `docs/design.md` 已锁定的四条不变量（依赖单向、最小核心、四者边界、静态组合）。

**产品定位（已决策）**：YuShan 是**后台 agent**，TUI 展示保持简洁（只覆盖基本 coding 功能；pi 的展示本也不复杂）。因此：

- 事件出口（`--json` / `-p`）是流式工作的**首个消费者**，TUI 是次要消费者且从简；
- 纯 UI 花活（Markdown 渲染、Diff Viewer、Theme、鼠标、可配快捷键）**正式砍掉**，不再列为暂缓项。

**交互 REPL 入口（待决，本轮不动）**：当前交互模式用 ratatui（`tui-ratatui` feature 默认开），历史上 rustyline 已被 ratatui 替换并彻底删除（`c9a4177`→`645e339`）。后台 agent 定位下，**rustyline 可能是更合适的方向**——它能删掉 `suspend_terminal`/`resume_terminal` dance（`ui/mod.rs:97-128`）、raw-mode 下的取消路由复杂度、手写 `complete_inline` 补全，并白拿持久 history；代价是反转旧决策。**本轮先不替换**，只记录为待决方向，把精力放在后台事件出口那半（见「未澄清问题」Q7）。

本文件不逐条复述报告，只回答三个问题：

1. 每一项工作**动哪一层**、**不动哪一层**；
2. 哪些工作是**解耦枢纽**（先做能解锁其他项）；
3. 哪些边界必须先定，否则会埋下耦合债。

---

## 现有架构（关键事实，均已核对代码）

### 模块边界

```
agent-core        Message/ContentBlock/ToolCall/ToolResult/Usage/StopReason/CancelToken
agent-model       Model trait + ModelEvent{TextDelta} + ModelEventSink + ModelRequest/Response
agent-event       AgentEvent + EventSink（同步 push trait）+ Noop/Collecting
agent-session     Session trait + MemorySession + JsonlSession（已完整实现）
agent-component   RuntimeContext + RunLimits
agent-loop        AgentLoop + BasicLoop（含 compact_session 私有实现 + Forwarder 桥接）
agent-runtime     AgentBuilder + Agent（set_model 热替换、clear_session）
adapters/          model-openai-compatible（stream 解析已写未接）、tools-basic（4 工具）
apps/coding-agent config/provider/commands/ui/view/state
```

### 与本次工作强相关的现状事实

| # | 事实 | 位置 |
|---|------|------|
| F1 | `ModelEvent` 只有 `TextDelta`，无 `ToolCallDelta` / `ThinkingDelta` | `crates/agent-model/src/event.rs:8` |
| F2 | SSE 解析 `parse_sse_stream` **返回缓冲的 `Vec`**，且**从未被调用**；请求硬编码 `stream: Some(false)` | `adapters/model-openai-compatible/src/stream.rs:16`、`lib.rs:192` |
| F3 | 流式响应类型 `ChunkToolCall`/`ChunkFunction` **已存在**（工具增量解析数据结构就绪） | `adapters/model-openai-compatible/src/response.rs:49-63` |
| F4 | `Forwarder` 只把 `TextDelta` 映射到 `AgentEvent::ModelTextDelta`，其余 `_ => Ok(())` | `crates/agent-loop/src/basic.rs:24-31` |
| F5 | `AgentEvent` 只有 `ModelTextDelta` 一个模型增量事件 | `crates/agent-event/src/event.rs:11` |
| F6 | **生产环境挂的是 `NoopEventSink`** —— 即使发出 `ModelTextDelta` 也被丢弃 | `apps/coding-agent/src/main.rs:139` |
| F7 | TUI turn 期间只画「Working 动画」，turn 结束后一次性追加 `final_message`；不消费增量事件 | `apps/coding-agent/src/ui/mod.rs:290-316`、`draw.rs:176-179` |
| F8 | `main.rs` 硬编码 `MemorySession::new()`；`JsonlSession` 已完整实现（原子写 + 损坏行跳过） | `main.rs:138`、`crates/agent-session/src/jsonl.rs` |
| F9 | `JsonlSession::open` 是 **async**，而 `AgentBuilder.session()` 是同步链式调用 | `jsonl.rs:14`、`builder.rs:52` |
| F10 | `model_factory` 是**单一闭包**，永远构造 `OpenAICompatibleModel`；新增 Anthropic 意味着闭包必须按 provider 分支 | `main.rs:72-86`、`config.rs:9,50-66` |
| F11 | `ProviderCompat` 只有 `has_reasoning_content` / `tool_calls_as_text` 两个布尔 | `compat.rs:1-27` |
| F12 | `ModelRequest` 无 thinking 参数 | `crates/agent-model/src/request.rs:7` |
| F13 | `BasicLoop::compact_session` **已做真摘要**（`generate_summary` 调模型）；但 `/compact` 命令只调 `clear_session()`，二者没接上 | `basic.rs:323-388`、`commands/builtin.rs:528-534` |
| F14 | `RuntimeContext` 无 hooks 字段；HookDispatcher 零代码 | `crates/agent-component/src/context.rs` |
| F15 | 工具仅 4 个：read/write/edit/bash | `adapters/tools-basic/src/lib.rs` |
| F16 | TUI 对 assistant 文本是**纯文本单行**渲染，无 Markdown/代码高亮/diff | `apps/coding-agent/src/ui/draw.rs:176-179` |
| F17 | `EventSink::emit` 是**同步** push 接口；design.md 明示「异步 sink 由 channel 适配器实现」（ADR-0004） | `crates/agent-event/src/sink.rs` |

### 外部依赖

| 依赖 | 用途 | 备注 |
|------|------|------|
| `reqwest` | HTTP 客户端（model-openai-compatible、provider） | 已锁 |
| `futures::StreamExt` | SSE 流消费 | stream.rs 已用 |
| `ratatui` + `crossterm` | TUI | `tui-ratatui` feature 默认开 |
| `tokio` | 运行时 | 全链 async |
| `serde` / `serde_json` | 序列化 | core 层已依赖 |
| `inquire` | slash 命令交互式选择器 | 仅 coding-agent |

---

## 约束

- **技术**：`Model` trait **不改**（`complete() + &mut dyn ModelEventSink` 已是流式就绪）；流式是适配器实现细节。新参数加到 `ModelRequest`，不加到 `Model`。
- **依赖单向**：`agent-core` 不依赖 Tokio/HTTP/TUI；任何 UI 逻辑不进 core/loop。
- **事件是一等接口**：UI、持久化、日志都走事件流，UI 不直接读写运行时可变对象（coding-agent 已用 `AppView` 快照隔离，本约束延续到增量事件）。
- **最小核心**：Session 文件管理、Markdown 渲染、provider 目录都是 coding-agent / adapter 层，不进通用核心。
- **演进**：`ModelEvent`/`AgentEvent` 均已 `#[non_exhaustive]`，新增变体是向后兼容扩展，不破坏既有 match（F4 的 `_ =>` 兜底正好利用这一点）。
- **组织**：单人维护节奏，优先「低耦合、可独立验证」的切分，每项落地都应对得上 `design.md §12` 测试矩阵。

---

## 需求范围

### 范围内（必须 = Tier 1 全量 + 高价值 = Tier 2 精选）

**Tier 1（必须）**：
1. 流式模型响应（文本 + 工具增量 + 思考增量）
2. `JsonlSession` 接入生产 + `/new` 建新会话文件
3. 主流 Provider 接入（OpenAI 零成本、Anthropic 新适配器、Google 走 OpenAI 兼容端点）
4. Thinking/Reasoning Level 控制
5. 上下文压缩闭环（把已实现但悬空的 `compact_session` 接到 `/compact`）

**Tier 2（高价值，本轮纳入）**：
- 2.1 更多工具（grep/glob/git）——复用 `Tool` trait，零核心改动
- 2.4 Hook 系统——**解耦枢纽**，见下文
- 2.5 分层配置（`settings.json` 全局 < 项目）

**Tier 2 暂缓（本轮不纳入，见「范围外」）**：2.6 Session 树分支。

### 范围外（明确不做，本轮）

- **Markdown 渲染 / Diff Viewer / Theme / 鼠标事件 / 可配快捷键**：纯 UI 花活。TUI 定位为「简洁 coding 界面」，这些不纳入；不影响后台 agent 能力。
- **Session 树分支**：独立 adapter crate（`session-jsonl-tree`），不修改现有 `JsonlSession`；本轮只做平面 `JsonlSession` 接入，树结构留给后续。
- **动态插件系统 / Client-Server RPC / Lanes**：报告 Tier 4 已明确跳过，符合静态组合哲学。
- **安全/沙箱/审批**：设计文档「不做安全」，`ToolRegistry` 仍只按名查找。

### 关键场景

- **S1 流式文本（后台事件出口）**：用户 `-p "task"` 或 `--json` → turn 期间逐 token 增量输出 assistant 文本 → 结束时终态与增量拼接一致（对齐测试矩阵「增量拼接与终态消息一致」）。TUI 仅做简洁显示（Working 动画 + turn 结束一次性显示，维持现状）。
- **S2 流式工具调用**：模型先出文本、再出 tool_calls → 增量组装工具参数 → 进入工具执行 → 结果回喂。事件出口逐个发出 `ToolCall`/`ToolResult`。
- **S3 会话恢复**：启动时 `JsonlSession::open` 恢复上次对话；`/new` 生成新 id 的新文件，旧文件保留在 `~/.yushan/sessions/`。
- **S4 多 Provider 切换**：`/login anthropic` 与 `/login openai` 走不同适配器，但 `Model` trait 统一；`/model` 热替换不重建会话。
- **S5 Thinking 控制**：`/thinking high` 设置思考级别 → 适配器翻译为 provider 特有参数 → 无思考参数的 provider 忽略（优雅降级）。
- **S6 主动压缩**：用户 `/compact` → 走 `compact_session` 真摘要路径（而非清空）→ 摘要以系统上下文注入。
- **S7 Hook 注入**：`before_model_request` 注入项目上下文/Coding Prompt；`before_tool_call` 更新操作状态。上下文注入不写进通用 Runtime。

---

## 实现与解耦策略（核心）

### 解耦枢纽 1：事件通道先通（F6 是流式的真正瓶颈，首个消费者是事件出口）

流式渲染**不是**「适配器发不发 SSE」的问题——适配器侧 `parse_sse_stream` 早已就绪（F3），`Model` trait 也早已流式就绪。真正的断点在：

- 生产挂 `NoopEventSink`（F6）→ 增量事件发出来就被丢；
- 现有消费者（TUI）turn 期间只 poll 键鼠事件、不消费模型事件（F7），且「后台 agent」定位下 TUI 不是重点。

**解耦做法**（严格贴合 design.md「异步 sink 由 channel 适配器实现」）：

1. 新增一个 **channel 适配器 EventSink**（`tokio::sync::mpsc`），实现 `EventSink`，`emit` 把 `AgentEvent` 投递到 channel。它只依赖 `agent-event` + `tokio`，放 coding-agent 层（`apps/coding-agent/src/events_bridge.rs` 或独立小 crate），**不进 core/loop**。
2. 用它替换 `main.rs` 的 `NoopEventSink`（F6），使所有事件出口共享同一通道。
3. **首个消费者 = 事件出口**：`--json` 逐事件序列化输出（机器接口）；`-p` 边生成边打印文本增量（人/管道消费）。二者复用同一 channel，边际成本低。
4. TUI 保持现状（Working 动画 + turn 结束一次性显示），**本轮不升级为增量渲染**；若将来需要，channel 已就位，只需在 `run_turn_with_ticks` 的三路 select 加第四路收 channel。

**为什么放第一**：它是后台 agent 的标准事件出口，服务 print / JSON / 日志 / 测试 / 回放所有消费者，且不触碰任何核心接口，风险最低、可独立验证。

**注**：`-p` 流式需要新增「文本增量逐块打印」路径（当前 `-p` 是 turn 结束后一次性 `println!`，见 `main.rs:161-168`）；`--json` 模式当前未实现，需新增。

### 解耦枢纽 2：Hook 系统（2.4）是多项功能的公共底座

design.md §6 已把 Hook 规范完整（Observer/Transform/Control + 8 个 hook 点）。观察本次范围，多项工作如果各自在 `BasicLoop` 里加分支会互相纠缠：

- Thinking 的 `reasoning_effort` 翻译 → 可作 `before_model_request` Transform；
- 上下文压缩注入 → 可作 `before_model_request` Transform（对齐 design.md「上下文通过 before_model_request Hook 注入」）；
- Coding Prompt / 项目上下文注入（design.md §10）→ 同上；
- 工具调用 UI 状态更新 → `before_tool_call` / `after_tool_result`。

**解耦做法**：`HookDispatcher` 放 `agent-component`（`RuntimeContext` 增加 `hooks` 字段，F14 位置），**`BasicLoop` 只留分发点**，不感知 hook 来自静态 crate 还是动态库。先做 `before_model_request` 一个 hook 点的 Observer/Transform，作为「最小可验证切片」，其余 hook 点按需补齐。

**风险提示**：Hook 是横切改动，若与 Tier 1 并行会放大 surface。建议**流式（枢纽 1）与 Hook（枢纽 2）分两条独立切**，各自可单独合并、单独测试。

### Tier 1 逐项落点与「动哪层/不动哪层」

**1.1 流式**（依赖枢纽 1）：
- 动 `agent-model/event.rs`：`ModelEvent` 增 `ToolCallDelta{index,id?,name?,arguments_delta}`、`ThinkingDelta{text}`（F1）。
- 动 `agent-event/event.rs`：`AgentEvent` 增对应增量事件（F5）。
- 动 `model-openai-compatible`：`stream: Some(true)` + 把 `parse_sse_stream` 从「缓冲 Vec」改为「边解析边 push 到 sink」（F2/F3）。**不动 `Model` trait**。
- 动 `basic.rs::Forwarder`：补映射新变体（F4，利用 `#[non_exhaustive]` 兜底做增量工具参数组装）。
- 动 `main.rs`：`-p` 改为边生成边打印；新增 `--json` 逐事件序列化输出（首个消费者，见枢纽 1）。**TUI 本轮不改**（维持 Working 动画 + 一次性显示）。

**1.2 JsonlSession**（低风险）：
- 动 `main.rs`：`JsonlSession::open(...).await` 替换 `MemorySession::new()`（F8/F9，注意 open 是 async，需在 `build()` 前 await）。
- 动 `commands/builtin.rs` `/new`：生成新 session id + 新文件，替换当前 `clear_session()` 语义。
- **不动** `agent-session` 本体（`JsonlSession` 已实现完毕）；session 目录/id 管理是 coding-agent 产品层职责，不进 core。

**1.3 Provider**（解耦重点，F10/F11）：
- **引入 adapter 分发**：把「provider 名 → Model 构造」从单一闭包解耦。OpenAI 零成本进 `ProviderRegistry`（api_base 常量）；Anthropic 新建 `adapters/model-anthropic/`（独立 crate，实现 `Model` trait，含自有 SSE 事件类型翻译）；Google 用 OpenAI 兼容端点（零新适配器）。
- `ProviderCompat` 扩展为**结构化的 provider 差异描述**（thinking 参数名、cache_control 支持、流式事件形状），替代继续堆布尔（F11）。
- 建议方向（供决策）：`config.model_factory` 从「单闭包」改为「provider → adapter 构造器」的映射，或引入最小 `ModelAdapter` 门面。**不动 `Model` trait**。
- 数据驱动（报告架构建议 #5，provider 目录从 TOML/JSON 加载）**本轮不做**，仅预留方向——它属于纯配置层重构，与能力补全可解耦。

**1.4 Thinking**：
- 动 `agent-model/request.rs`：`ModelRequest` 增 `thinking_level: Option<ThinkingLevel>`（F12），枚举 `None/Minimal/Low/Medium/High/XHigh/Max`。
- 各适配器翻译（DeepSeek→`reasoning_effort`，Anthropic→`thinking.budget_tokens`），无支持者静默忽略。
- 动 `commands`：新增 `/thinking`。
- **不动 `Model`**；参数进请求层，保持适配器向后兼容。

**1.5 压缩闭环**：
- 核心逻辑已存在（F13），缺的是「从命令层触达」的入口。两条路二选一（见「未澄清问题」）：
  - (a) 给 `Agent`/`AgentLoop` 暴露 `compact()` 方法，`/compact` 调它；
  - (b) 依赖枢纽 2，把压缩做成 `before_model_request` Hook，`/compact` 只置标志。
- 优先 (b) 以复用 Hook 底座、避免在命令层复刻压缩逻辑；但 (b) 受枢纽 2 进度约束。

### Tier 2 精选落点

- **2.1 grep/glob/git**：新增 `adapters/tools-basic` 内三个 `Tool` 实现，复用 `Tool` trait + `ToolContext`（F15）。零核心改动，最高性价比。
- **2.4 Hook**：见枢纽 2。
- **2.5 分层配置**：`~/.yushan/settings.json` < `.yushan/settings.json`。建议做成 coding-agent 层独立的 `SettingsStore`，与 `ProviderRegistry`/`StateStore` 平级，不耦合进 core。与 provider 数据驱动（1.3 的预留项）共用同一加载器更佳，可合并设计。

---

## 未澄清问题

- [ ] **Q1（范围）**：Tier 2 的 Markdown 渲染/Diff Viewer 是否明确延后？我的建议是延后（依赖流式通道先通），但若产品优先级更高可提前到 Phase 3 之前。
- [ ] **Q2（压缩入口）**：1.5 走 (a) `Agent::compact()` 方法还是 (b) Hook 注入？影响与枢纽 2 的先后依赖。
- [ ] **Q3（Provider 分发）**：1.3 的 adapter 分发用「`ModelAdapter` 门面」还是「provider→构造器 HashMap」？影响 `config::ModelFactory` 现有签名是否要动。
- [ ] **Q4（Anthropic 范围）**：本轮 Anthropic 只做 `messages` 基础（无 thinking、无 cache_control），还是直接覆盖 thinking？影响 1.3 与 1.4 的合流方式。
- [ ] **Q5（流式粒度）**：工具增量是「边收边组装并在事件出口发半成品」，还是「后台组装、仅在参数完整时发一个事件」？前者对消费端复杂，后者更简单且足够 v0。建议后者。
- [ ] **Q6（顺序确认）**：是否认可「枢纽 1 事件通道 + 事件出口」与「枢纽 2 Hook」两条独立切、并行推进？还是严格串行（先 Tier 1 全量再 Tier 2）？
- [ ] **Q7（交互入口，本轮不动）**：rustyline 是否要替换 ratatui（后台 agent 方向下的倾向建议）？若采纳，是「彻底删除 ratatui」还是「rustyline 默认 + ratatui 降为非默认 feature」。本轮已决定**先不动**，此问题留到事件出口那半落地后再评估。

---

## 后续建议

- 用 `arch-design` 分别对**枢纽 1（事件通道 + 流式）**和**枢纽 2（HookDispatcher 最小切片）**做方案设计与 ADR，二者各自成文档、各自可独立合并。
- 用 `prototype` 验证 S1/S2 的「增量拼接与终态一致」假设（测试矩阵已要求该不变式）。
- 每项落地补测试矩阵条目：`ModelEvent` 序列化、SSE 增量组装、channel sink 事件顺序、`compact` 闭环、`/new` 会话文件恢复、provider compat 映射、Hook 顺序/Observer 容错。
