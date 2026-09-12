# YuShan vs Pi — 基础能力差距分析

> 生成日期：2026-09-12
> 对比对象：YuShan（Rust Agent Runtime）vs tmp/pi（TypeScript Coding Agent）

## Context

YuShan 当前已实现 v0 最小闭环（8 个核心 crate + 2 个适配器 + coding-agent 二进制），`cargo test` 全部通过。`tmp/pi` 是一个成熟的 TypeScript 编码 Agent，拥有 40+ provider、流式响应、session 树分支、hooks/插件系统、富 TUI 等。本分析对比二者，识别 YuShan 需要补齐的基础能力。

---

## TIER 1：MVP 必须补齐

### 1.1 流式模型响应（Critical / Medium 复杂度）

**现状**：`ModelEvent::TextDelta` 和 `ModelEventSink` 已定义（`crates/agent-model/src/event.rs`），但 `stream.rs` 中的 `parse_sse_stream` 未被调用。`stream: Some(false)` 硬编码在 `adapters/model-openai-compatible/src/lib.rs:192`。

**缺口**：
- `ModelEvent` 只有 `TextDelta`，缺少 `ToolCallDelta`（增量工具调用组装）和 `ThinkingDelta`（推理内容）
- OpenAI 适配器不发流式请求
- TUI 在 turn 期间不处理增量文本事件

**实现路径**：
1. 扩展 `ModelEvent`：增加 `ToolCallDelta { index, id?, name?, arguments_delta }` 和 `ThinkingDelta { text }`
2. OpenAI 适配器改为 `stream: true`，使用 SSE 逐块推送事件到 sink
3. `BasicLoop::Forwarder` 已桥接 `ModelEvent→AgentEvent`，扩展处理新事件变体
4. TUI turn 期间处理 `AgentEvent::ModelTextDelta`，增量渲染

**关键文件**：`crates/agent-model/src/event.rs`、`adapters/model-openai-compatible/src/`、`crates/agent-loop/src/basic.rs`、`apps/coding-agent/src/ui/mod.rs`

---

### 1.2 JsonlSession 接入生产（Critical / Easy 复杂度）

**现状**：`JsonlSession` 已完整实现（原子写入、损坏行跳过），但 `main.rs:138` 硬编码 `MemorySession::new()`。

**实现路径**：`main.rs` 改为 `JsonlSession::open(~/.yushan/sessions/{id}.jsonl)`，`/new` 命令创建新 session 文件。

**关键文件**：`apps/coding-agent/src/main.rs:138`、`apps/coding-agent/src/commands/builtin.rs`

---

### 1.3 主流 Provider 接入（Critical / Medium 复杂度）

**现状**：`ProviderRegistry` 只有 deepseek、minimax、custom。OpenAI 可通过现有适配器接入，但 Anthropic（不同 API 格式）和 Google 需要新适配器。

**实现路径**：
1. **OpenAI**：加入 ProviderRegistry，api_base = `https://api.openai.com/v1`（零开发）
2. **Anthropic**：新建 `adapters/model-anthropic/`，实现 Messages API（不同请求格式 + 流式事件类型）
3. **Google**：可用 Gemini 的 OpenAI 兼容端点，或新建适配器
4. 扩展 `ProviderCompat` 支持 Anthropic 特有字段

**关键文件**：`apps/coding-agent/src/provider.rs`、`adapters/model-openai-compatible/src/compat.rs`

---

### 1.4 Thinking/Reasoning Level（Critical / Medium 复杂度）

**现状**：`ModelRequest` 无思考参数。`ProviderCompat::has_reasoning_content` 仅做 DeepSeek 字段映射，无法控制思考深度。

**实现路径**：
1. `ModelRequest` 增加 `thinking_level: Option<ThinkingLevel>`
2. `ThinkingLevel` 枚举：`None, Minimal, Low, Medium, High, XHigh, Max`
3. 各适配器翻译为 provider 特有参数（DeepSeek → `reasoning_effort`，Anthropic → `thinking.budget_tokens`）
4. 新增 `/thinking` slash command

**关键文件**：`crates/agent-model/src/request.rs`、适配器 crate

---

### 1.5 上下文压缩改进（Critical / Medium 复杂度）

**现状**：`BasicLoop::compact_session` 能工作但粗糙（旧消息替换为单条摘要）。`/compact` 命令只调用 `clear_session()`。

**实现路径**：
1. 改进摘要 prompt，保留更多上下文
2. `/compact` 调用模型做真正摘要，而非清空 session
3. 摘要注入为系统上下文

**关键文件**：`crates/agent-loop/src/basic.rs:323`、`apps/coding-agent/src/commands/builtin.rs:528`

---

## TIER 2：高价值增强

### 2.1 更多工具（High / Easy）— grep、glob、git

当前只有 4 个工具（Read/Write/Edit/Bash）。补充 GrepTool、GlobTool、GitTool 可直接提升编码效率。实现复用已有 `Tool` trait。

### 2.2 Markdown 渲染（High / Medium）

TUI 当前纯文本渲染 assistant 消息。用 `pulldown-cmark` 解析 + `syntect` 代码高亮，显著提升可读性。

### 2.3 Diff Viewer（High / Medium）

EditTool 结果以纯文本显示。渲染彩色 diff（绿增/红删）帮助理解 agent 改了什么。

### 2.4 Hook 系统（High / Hard）

`design.md` §6 已完整规范（before_model_request、after_model_response、before_tool_call 等），但零代码。`RuntimeContext` 自然承载 hooks，`BasicLoop::run_turn` 插入分发点。

### 2.5 分层配置（High / Medium）

当前仅环境变量 + auth.json。增加 `~/.yushan/settings.json`（全局）< `.yushan/settings.json`（项目）分层配置，新增 `/settings` 命令。

### 2.6 Session 树分支（High / Hard）

`JsonlSession` 是平面列表。增加 `parent_id` + `branch_point`，实现 fork/clone/resume，新增 `/tree` 命令。建议作为单独 adapter crate。

---

## TIER 3：锦上添花

| 特性 | 重要度 | 复杂度 | 说明 |
|------|--------|--------|------|
| Skills 系统 | Medium | Medium | Markdown + YAML frontmatter 发现，注入 system prompt |
| Prompt 模板 | Medium | Easy | 位置参数替换（$1, $2, $@） |
| Subagent 系统 | Medium | Hard | 多角色（planner/worker/reviewer），`AgentBuilder` 已支持组合 |
| Theme 系统 | Low-Med | Easy | 颜色主题切换 |
| /export 命令 | Low-Med | Easy | 已有 stub，序列化为 HTML |
| 鼠标事件 | Low | Easy | ratatui 原生支持 |
| 可配置快捷键 | Low | Easy | `/hotkeys` |

---

## TIER 4：跳过（不符合 YuShan 设计哲学）

| 特性 | 原因 |
|------|------|
| Client-Server RPC | YuShan 是轻量 CLI，不是服务端 |
| Lanes 并行工作队列 | 过度工程，CLI 多实例即可 |
| 动态插件系统 | 设计文档 Phase 4，静态组合已够用 |
| 评估框架 | 测试关注点，非运行时 |
| Scoped Models | `/model` 已覆盖 |
| 项目信任系统 | YuShan 不做安全/沙箱 |
| Kitty/iTerm2 图片 | 小众 + 复杂 |
| Mermaid 渲染 | 终端渲染引擎太重 |

---

## 推荐实施顺序

| 阶段 | 内容 | 预估 |
|------|------|------|
| **Phase 1：核心流式** | 流式响应 (1.1) + JsonlSession 接入 (1.2) + OpenAI/Anthropic provider (1.3) | 2 周 |
| **Phase 2：推理与上下文** | Thinking levels (1.4) + 上下文压缩改进 (1.5) | 1 周 |
| **Phase 3：工具与 TUI** | grep/glob/git 工具 (2.1) + Markdown 渲染 (2.2) + Diff Viewer (2.3) | 1-2 周 |
| **Phase 4：Hooks 与配置** | Hook 系统 (2.4) + 分层配置 (2.5) | 1-2 周 |
| **Phase 5：打磨** | Session 树 (2.6) + Skills + Prompt 模板 + Theme + Export | 2-3 周 |

---

## 架构建议

1. **Model trait 不改**：`complete()` + `ModelEventSink` 已是流式就绪设计，流式是适配器实现细节
2. **扩展 ModelRequest 而非 Model**：thinking_level、cache_control 等加到请求层，保持适配器向后兼容
3. **Session 树做独立 adapter**：`adapters/session-jsonl-tree/`，不修改现有 `JsonlSession`
4. **Hooks 放 agent-component**：`RuntimeContext` 自然承载，避免循环依赖
5. **Provider 数据驱动**：从 TOML/JSON 配置文件加载，而非硬编码 Rust 代码
